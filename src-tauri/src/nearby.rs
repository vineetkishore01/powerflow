//! Nearby Apple devices over Bluetooth LE.
//!
//! Apple's manufacturer-specific advertisement data (company ID `0x004C`) is a
//! sequence of `type, length, value` records. Two of them are useful here:
//!
//! * Proximity Pairing (`0x07`): broadcast by AirPods/Beats while the case is
//!   open or they are in use, even when they're connected to another device.
//!   It carries coarse (10 %) left/right/case levels and charging flags in the
//!   clear, so no connection is needed.
//! * Handoff (`0x0C`) / Nearby Info (`0x10`): broadcast by iPhones, iPads,
//!   Watches and Macs. These carry no battery level, but iOS exposes the
//!   standard GATT Battery Service (`0x180F`/`0x2A19`), so we connect briefly
//!   to read it. That connects to devices rather than just listening, so it's
//!   opt-in via the `discoverIosOverBluetooth` preference.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use btleplug::{
    api::{
        bleuuid::uuid_from_u16, Central, CentralEvent, CentralState, Manager as _,
        Peripheral as _, ScanFilter,
    },
    platform::{Adapter, Manager, PeripheralId},
};
use futures::StreamExt;
use tauri::{async_runtime, AppHandle, Manager as _};
use tauri_plugin_pinia::ManagerExt;
use tokio::time::{sleep, sleep_until, timeout};

use crate::peripheral::{
    unix_now, BatteryCell, KnownAccessory, PeripheralInfo, PeripheralState, PeripheralType,
};

const APPLE_COMPANY_ID: u16 = 0x004C;
const TYPE_PROXIMITY_PAIRING: u8 = 0x07;
const TYPE_HANDOFF: u8 = 0x0C;
const TYPE_NEARBY_INFO: u8 = 0x10;

const BATTERY_LEVEL: u16 = 0x2A19;
const MODEL_NUMBER: u16 = 0x2A24;

/// Scan for `SCAN_WINDOW` out of every `SCAN_INTERVAL` to keep radio use low.
const SCAN_WINDOW: Duration = Duration::from_secs(10);
const SCAN_INTERVAL: Duration = Duration::from_secs(60);
/// Ignore anything weaker than this: it's likely in another room or flat.
const MIN_RSSI: i16 = -75;

const GATT_TIMEOUT: Duration = Duration::from_secs(10);
const GATT_RETRY: Duration = Duration::from_secs(5 * 60);
/// Macs and other non-iOS devices also send Nearby Info; skip them for longer.
const GATT_IGNORE: Duration = Duration::from_secs(30 * 60);
const MAX_GATT_PER_CYCLE: usize = 3;

pub const PREF_DISCOVER_IOS: &str = "discoverIosOverBluetooth";
const GATT_ID_PREFIX: &str = "ble-gatt-";

/// Iterate the `type, length, value` records in Apple manufacturer data
/// (company ID already stripped).
fn apple_records(data: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let mut rest = data;
    std::iter::from_fn(move || {
        let [ty, len, tail @ ..] = rest else {
            return None;
        };
        let len = usize::from(*len);
        if tail.len() < len {
            return None;
        }
        let (value, next) = tail.split_at(len);
        rest = next;
        Some((*ty, value))
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProximityPairing {
    /// Same value system_profiler reports as `device_productID`.
    pub model: u16,
    pub left: Option<u8>,
    pub right: Option<u8>,
    pub case: Option<u8>,
    pub left_charging: bool,
    pub right_charging: bool,
    pub case_charging: bool,
}

/// Decode a Proximity Pairing record value (after the type and length bytes):
/// `[prefix, model_lo, model_hi, status, pods, charge|case, lid, color, ...]`.
/// Battery nibbles are 0-10 (x10 %), `0xF` meaning not available.
pub fn parse_proximity_pairing(value: &[u8]) -> Option<ProximityPairing> {
    let &[_prefix, model_lo, model_hi, status, pods, charge_case, ..] = value else {
        return None;
    };
    let level = |n: u8| (n <= 10).then_some(n * 10);

    // Status bit 5 says which pod is broadcasting; the pod nibbles and their
    // charging bits are swapped depending on it.
    let flipped = status & 0x20 == 0;
    let (high, low) = (level(pods >> 4), level(pods & 0x0F));
    let charge = charge_case >> 4;
    let (left, right) = if flipped { (high, low) } else { (low, high) };
    let (left_charging, right_charging) = if flipped {
        (charge & 0x02 != 0, charge & 0x01 != 0)
    } else {
        (charge & 0x01 != 0, charge & 0x02 != 0)
    };

    let reading = ProximityPairing {
        model: u16::from_le_bytes([model_lo, model_hi]),
        left,
        right,
        case: level(charge_case & 0x0F),
        left_charging,
        right_charging,
        case_charging: charge & 0x04 != 0,
    };
    (reading.left.is_some() || reading.right.is_some() || reading.case.is_some())
        .then_some(reading)
}

/// Build a peripheral for AirPods/Beats this Mac is paired with. Advertisements
/// don't identify their owner, so anything whose model isn't paired with this
/// Mac is assumed to belong to someone else and dropped.
fn proximity_peripheral(pp: &ProximityPairing, known: &[KnownAccessory]) -> Option<PeripheralInfo> {
    let matches: Vec<&KnownAccessory> = known
        .iter()
        .filter(|k| k.vendor_id == APPLE_COMPANY_ID && k.product_id == pp.model)
        .collect();
    // Connected ones are already reported, more precisely, by system_profiler.
    if matches.is_empty() || matches.iter().any(|k| k.connected) {
        return None;
    }
    let name = match matches.as_slice() {
        [only] => only.name.clone(),
        _ => "AirPods (nearby)".to_string(),
    };

    let cell = |name: &str, level: Option<u8>, is_charging: bool| {
        level.map(|level| BatteryCell {
            name: name.into(),
            level,
            is_charging,
        })
    };
    let pods: Vec<BatteryCell> = [
        cell("L", pp.left, pp.left_charging),
        cell("R", pp.right, pp.right_charging),
    ]
    .into_iter()
    .flatten()
    .collect();
    let case = cell("Case", pp.case, pp.case_charging);

    let battery_level = pods
        .iter()
        .map(|c| c.level)
        .min()
        .or(pp.case)?;
    let is_charging = pods.iter().any(|c| c.is_charging);
    // Single-battery headphones (AirPods Max) report one pod and no case.
    let cells = if pods.len() == 1 && case.is_none() {
        Vec::new()
    } else {
        pods.into_iter().chain(case).collect()
    };

    Some(PeripheralInfo {
        id: format!("ble-pp-{:04x}", pp.model),
        name,
        peripheral_type: PeripheralType::Headset,
        battery_level,
        is_charging,
        cells,
        via: Some("Bluetooth LE".into()),
        last_updated: unix_now(),
    })
}

fn model_to_type(model: &str) -> Option<PeripheralType> {
    if model.starts_with("iPhone") || model.starts_with("iPod") {
        Some(PeripheralType::Phone)
    } else if model.starts_with("iPad") {
        Some(PeripheralType::Tablet)
    } else if model.starts_with("Watch") {
        Some(PeripheralType::Watch)
    } else {
        None
    }
}

enum GattOutcome {
    Found(PeripheralInfo),
    /// Not an iOS device (e.g. a Mac), or no Battery Service.
    NotApplicable,
    Failed,
}

async fn read_gatt_battery(central: &Adapter, id: &PeripheralId) -> GattOutcome {
    let Ok(p) = central.peripheral(id).await else {
        return GattOutcome::Failed;
    };

    let result = timeout(GATT_TIMEOUT, async {
        p.connect().await?;
        p.discover_services().await?;
        let chars = p.characteristics();
        let find = |short| chars.iter().find(|c| c.uuid == uuid_from_u16(short)).cloned();

        let model = match find(MODEL_NUMBER) {
            Some(c) => p.read(&c).await.ok().map(|v| {
                String::from_utf8_lossy(&v)
                    .trim_end_matches('\0')
                    .trim()
                    .to_string()
            }),
            None => None,
        };
        let level = match find(BATTERY_LEVEL) {
            Some(c) => p.read(&c).await?.first().copied(),
            None => None,
        };
        Ok::<_, btleplug::Error>((model, level))
    })
    .await;
    let _ = p.disconnect().await;

    let (model, level) = match result {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            log::debug!("GATT read of {id:?} failed: {e}");
            return GattOutcome::Failed;
        }
        Err(_) => {
            log::debug!("GATT read of {id:?} timed out");
            return GattOutcome::Failed;
        }
    };
    let (Some(peripheral_type), Some(level)) = (model.as_deref().and_then(model_to_type), level)
    else {
        return GattOutcome::NotApplicable;
    };

    let name = p
        .properties()
        .await
        .ok()
        .flatten()
        .and_then(|props| props.local_name)
        .or(model)
        .unwrap_or_else(|| "iPhone".into());

    GattOutcome::Found(PeripheralInfo {
        // Private addresses rotate, so key by name to avoid duplicates.
        id: format!("{GATT_ID_PREFIX}{name}"),
        name,
        peripheral_type,
        battery_level: level.min(100),
        is_charging: false,
        cells: Vec::new(),
        via: Some("Bluetooth LE".into()),
        last_updated: unix_now(),
    })
}

#[derive(Default)]
struct GattSchedule {
    next_attempt: HashMap<PeripheralId, Instant>,
}

impl GattSchedule {
    fn due(&self, id: &PeripheralId) -> bool {
        self.next_attempt
            .get(id)
            .is_none_or(|t| Instant::now() >= *t)
    }

    fn defer(&mut self, id: PeripheralId, by: Duration) {
        let now = Instant::now();
        // Rotating addresses would otherwise grow this forever.
        self.next_attempt.retain(|_, t| *t > now);
        self.next_attempt.insert(id, now + by);
    }
}

pub fn start_nearby_scanner(app: AppHandle) -> async_runtime::JoinHandle<()> {
    async_runtime::spawn(async move {
        loop {
            if let Err(e) = run(&app).await {
                log::warn!("Bluetooth LE scanner stopped: {e}");
            }
            sleep(SCAN_INTERVAL).await;
        }
    })
}

async fn run(app: &AppHandle) -> btleplug::Result<()> {
    let manager = Manager::new().await?;
    let Some(central) = manager.adapters().await?.into_iter().next() else {
        return Err(btleplug::Error::DeviceNotFound);
    };
    let mut events = central.events().await?;
    let mut schedule = GattSchedule::default();

    loop {
        if central.adapter_state().await? != CentralState::PoweredOn {
            sleep(SCAN_INTERVAL).await;
            continue;
        }

        central.start_scan(ScanFilter::default()).await?;
        let mut seen: HashMap<PeripheralId, Vec<u8>> = HashMap::new();
        let deadline = tokio::time::Instant::now() + SCAN_WINDOW;
        loop {
            tokio::select! {
                event = events.next() => match event {
                    Some(CentralEvent::ManufacturerDataAdvertisement { id, mut manufacturer_data }) => {
                        if let Some(data) = manufacturer_data.remove(&APPLE_COMPANY_ID) {
                            seen.insert(id, data);
                        }
                    }
                    Some(_) => {}
                    None => return Ok(()),
                },
                () = sleep_until(deadline) => break,
            }
        }
        central.stop_scan().await?;

        process_cycle(app, &central, seen, &mut schedule).await;
        sleep(SCAN_INTERVAL.saturating_sub(SCAN_WINDOW)).await;
    }
}

async fn process_cycle(
    app: &AppHandle,
    central: &Adapter,
    seen: HashMap<PeripheralId, Vec<u8>>,
    schedule: &mut GattSchedule,
) {
    let Some(state) = app.try_state::<PeripheralState>() else {
        return;
    };
    let known = state
        .known_accessories
        .lock()
        .map(|k| k.clone())
        .unwrap_or_default();
    let discover_ios = app
        .pinia()
        .try_get::<bool>("preference", PREF_DISCOVER_IOS)
        .unwrap_or(false);

    // Strongest advertisement wins when several match the same entry.
    let mut headsets: HashMap<String, (i16, PeripheralInfo)> = HashMap::new();
    let mut candidates: Vec<(i16, PeripheralId)> = Vec::new();

    for (id, data) in seen {
        let rssi = match central.peripheral(&id).await {
            Ok(p) => p.properties().await.ok().flatten().and_then(|p| p.rssi),
            Err(_) => None,
        };
        let Some(rssi) = rssi.filter(|r| *r >= MIN_RSSI) else {
            continue;
        };

        for (ty, value) in apple_records(&data) {
            match ty {
                TYPE_PROXIMITY_PAIRING => {
                    let Some(info) = parse_proximity_pairing(value)
                        .and_then(|pp| proximity_peripheral(&pp, &known))
                    else {
                        continue;
                    };
                    match headsets.get(&info.id) {
                        Some((best, _)) if *best >= rssi => {}
                        _ => {
                            headsets.insert(info.id.clone(), (rssi, info));
                        }
                    }
                }
                TYPE_HANDOFF | TYPE_NEARBY_INFO if discover_ios => {
                    if !candidates.iter().any(|(_, c)| *c == id) {
                        candidates.push((rssi, id.clone()));
                    }
                }
                _ => {}
            }
        }
    }

    let mut found = Vec::new();
    if discover_ios {
        candidates.sort_by(|a, b| b.0.cmp(&a.0));
        candidates.retain(|(_, id)| schedule.due(id));
        candidates.truncate(MAX_GATT_PER_CYCLE);
        for (_, id) in candidates {
            match read_gatt_battery(central, &id).await {
                GattOutcome::Found(info) => {
                    found.push(info);
                    schedule.defer(id, GATT_RETRY);
                }
                GattOutcome::NotApplicable => schedule.defer(id, GATT_IGNORE),
                GattOutcome::Failed => schedule.defer(id, GATT_RETRY),
            }
        }
    }

    let Ok(mut guard) = state.nearby.lock() else {
        return;
    };
    if !discover_ios {
        guard.retain(|id, _| !id.starts_with(GATT_ID_PREFIX));
    }
    for p in headsets.into_values().map(|(_, p)| p).chain(found) {
        guard.insert(p.id.clone(), p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_records() {
        let data = [0x10, 0x02, 0xAA, 0xBB, 0x07, 0x01, 0xCC];
        let records: Vec<_> = apple_records(&data).collect();
        assert_eq!(records, vec![(0x10, &[0xAA, 0xBB][..]), (0x07, &[0xCC][..])]);
        // Truncated trailing record is dropped rather than over-read.
        assert_eq!(apple_records(&[0x07, 0x19, 0x01]).count(), 0);
    }

    #[test]
    fn decodes_airpods_pro_2() {
        // prefix, model 0x2014 (LE), status (bit 5 clear), L=8 R=7, charging
        // flags 0b0101 (case + first pod), case=5.
        let value = [0x01, 0x14, 0x20, 0x0B, 0x87, 0x55, 0x00, 0x00];
        let pp = parse_proximity_pairing(&value).unwrap();
        assert_eq!(pp.model, 0x2014);
        assert_eq!((pp.left, pp.right, pp.case), (Some(80), Some(70), Some(50)));
        assert!(pp.right_charging && !pp.left_charging && pp.case_charging);

        // Same payload with status bit 5 set swaps the pods.
        let value = [0x01, 0x14, 0x20, 0x2B, 0x87, 0x55, 0x00, 0x00];
        let pp = parse_proximity_pairing(&value).unwrap();
        assert_eq!((pp.left, pp.right), (Some(70), Some(80)));
        assert!(pp.left_charging && !pp.right_charging);
    }

    #[test]
    fn rejects_unavailable_levels() {
        let value = [0x01, 0x0A, 0x20, 0x00, 0xFF, 0x0F, 0x00];
        assert_eq!(parse_proximity_pairing(&value), None);
    }

    #[test]
    fn only_reports_accessories_paired_with_this_mac() {
        let pp = ProximityPairing {
            model: 0x200A,
            left: Some(60),
            right: None,
            case: None,
            left_charging: false,
            right_charging: false,
            case_charging: false,
        };
        assert!(proximity_peripheral(&pp, &[]).is_none());

        let known = [KnownAccessory {
            name: "AirPods Max".into(),
            vendor_id: APPLE_COMPANY_ID,
            product_id: 0x200A,
            connected: false,
        }];
        let info = proximity_peripheral(&pp, &known).unwrap();
        assert_eq!(info.name, "AirPods Max");
        assert_eq!(info.battery_level, 60);
        assert!(info.cells.is_empty());

        let connected = [KnownAccessory {
            connected: true,
            ..known[0].clone()
        }];
        assert!(proximity_peripheral(&pp, &connected).is_none());
    }
}
