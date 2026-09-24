use std::{
    collections::HashMap,
    process::Command,
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use core_foundation::{
    base::{kCFAllocatorDefault, TCFType},
    boolean::CFBoolean,
    dictionary::{CFDictionary, CFMutableDictionaryRef},
    number::CFNumber,
    string::CFString,
};
use io_kit_sys::{
    types::io_iterator_t,
    IOIteratorNext, IOObjectRelease, IORegistryEntryCreateCFProperties,
    IOServiceGetMatchingServices, IOServiceMatching, ret::kIOReturnSuccess,
};
use objc2_foundation::NSSize;
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::{async_runtime, AppHandle, Manager};
use tauri_plugin_nspopover::AppExt;
use tauri_specta::Event;
use tokio::time;

#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PeripheralType {
    Phone,
    Tablet,
    Watch,
    Headset,
    Mouse,
    Keyboard,
    Trackpad,
    Gamepad,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BatteryCell {
    pub name: String,
    pub level: u8,
    pub is_charging: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PeripheralInfo {
    pub id: String,
    pub name: String,
    pub peripheral_type: PeripheralType,
    pub battery_level: u8,
    pub is_charging: bool,
    pub cells: Vec<BatteryCell>,
    pub via: Option<String>,
    pub last_updated: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Event, Type)]
#[serde(rename_all = "camelCase")]
pub struct PeripheralUpdatedEvent {
    pub peripherals: Vec<PeripheralInfo>,
}

#[derive(Default)]
pub struct PeripheralState {
    pub peripherals: Mutex<Vec<PeripheralInfo>>,
    pub ios_cache: Mutex<HashMap<String, (String, u8, bool)>>,
}

fn get_dict_val(dict: &CFDictionary, key: &str) -> Option<*const std::ffi::c_void> {
    let cf_key = CFString::new(key);
    let item = dict.find(cf_key.as_concrete_TypeRef() as *const std::ffi::c_void)?;
    Some(*item)
}

/// Scan Magic accessories (Mouse, Keyboard, Trackpad) via IOKit HID matching.
pub fn scan_hid_accessories() -> Vec<PeripheralInfo> {
    let mut results = Vec::new();
    let service_names = [
        "AppleDeviceManagementHIDEventService",
        "AppleBluetoothHIDKeyboard",
        "BNBTrackpadDevice",
        "BNBMouseDevice",
    ];

    for name in service_names {
        unsafe {
            let c_name = match std::ffi::CString::new(name) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let matching_dict = IOServiceMatching(c_name.as_ptr());
            if matching_dict.is_null() {
                continue;
            }
            let mut iterator: io_iterator_t = 0;
            let kr = IOServiceGetMatchingServices(0, matching_dict, &mut iterator);
            if kr != kIOReturnSuccess || iterator == 0 {
                continue;
            }

            loop {
                let service = IOIteratorNext(iterator);
                if service == 0 {
                    break;
                }

                let mut props: CFMutableDictionaryRef = std::ptr::null_mut();
                let ret = IORegistryEntryCreateCFProperties(service, &mut props, kCFAllocatorDefault, 0);
                IOObjectRelease(service);
                if ret != kIOReturnSuccess || props.is_null() {
                    continue;
                }

                let dict: CFDictionary = CFDictionary::wrap_under_create_rule(props);

                let is_builtin = get_dict_val(&dict, "Built-In")
                    .map(|v| {
                        let cf_bool = CFBoolean::wrap_under_get_rule(v as _);
                        cf_bool == CFBoolean::true_value()
                    })
                    .unwrap_or(false);

                let product = get_dict_val(&dict, "Product")
                    .map(|v| {
                        let cf_str = CFString::wrap_under_get_rule(v as _);
                        cf_str.to_string()
                    })
                    .unwrap_or_default();

                if is_builtin || product.contains("Internal") || product.is_empty() {
                    continue;
                }

                let percent = get_dict_val(&dict, "BatteryPercent")
                    .and_then(|v| {
                        let cf_num = CFNumber::wrap_under_get_rule(v as _);
                        cf_num.to_i64()
                    })
                    .unwrap_or(0);

                let status = get_dict_val(&dict, "BatteryStatusFlags")
                    .and_then(|v| {
                        let cf_num = CFNumber::wrap_under_get_rule(v as _);
                        cf_num.to_i64()
                    })
                    .unwrap_or(0);

                let mac = get_dict_val(&dict, "DeviceAddress")
                    .map(|v| {
                        let cf_str = CFString::wrap_under_get_rule(v as _);
                        cf_str.to_string().replace('-', ":").to_uppercase()
                    })
                    .unwrap_or_else(|| product.clone());

                let peripheral_type = if product.contains("Trackpad") {
                    PeripheralType::Trackpad
                } else if product.contains("Keyboard") {
                    PeripheralType::Keyboard
                } else if product.contains("Mouse") {
                    PeripheralType::Mouse
                } else {
                    PeripheralType::Other
                };

                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();

                results.push(PeripheralInfo {
                    id: mac,
                    name: product,
                    peripheral_type,
                    battery_level: (percent as u8).clamp(0, 100),
                    is_charging: status != 0,
                    cells: Vec::new(),
                    via: Some("Bluetooth HID".to_string()),
                    last_updated: now,
                });
            }
            IOObjectRelease(iterator);
        }
    }
    results
}

/// Scan connected Bluetooth devices (earphones, headsets, speakers, third-party accessories) via system_profiler.
pub fn scan_bluetooth_accessories() -> Vec<PeripheralInfo> {
    let mut results = Vec::new();
    let output = match Command::new("/usr/sbin/system_profiler")
        .args(["SPBluetoothDataType", "-json"])
        .output()
    {
        Ok(out) if out.status.success() => out.stdout,
        _ => return results,
    };

    let val: serde_json::Value = match serde_json::from_slice(&output) {
        Ok(v) => v,
        Err(_) => return results,
    };

    let items = val
        .get("SPBluetoothDataType")
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .and_then(|obj| obj.get("device_connected"))
        .and_then(|c| c.as_array());

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    if let Some(devices) = items {
        for item in devices {
            if let Some(map) = item.as_object() {
                for (name, details) in map {
                    let address = details
                        .get("device_address")
                        .and_then(|s| s.as_str())
                        .unwrap_or(name);
                    let minor_type = details
                        .get("device_minorType")
                        .and_then(|s| s.as_str())
                        .unwrap_or("");

                    let parse_level = |key: &str| -> Option<u8> {
                        details
                            .get(key)
                            .and_then(|s| s.as_str())
                            .and_then(|s| s.trim_end_matches('%').trim().parse::<u8>().ok())
                    };

                    let main_level = parse_level("device_batteryLevelMain");
                    let left_level = parse_level("device_batteryLevelLeft");
                    let right_level = parse_level("device_batteryLevelRight");
                    let case_level = parse_level("device_batteryLevelCase");

                    let mut cells = Vec::new();
                    if let Some(l) = left_level {
                        cells.push(BatteryCell {
                            name: "L".into(),
                            level: l,
                            is_charging: false,
                        });
                    }
                    if let Some(r) = right_level {
                        cells.push(BatteryCell {
                            name: "R".into(),
                            level: r,
                            is_charging: false,
                        });
                    }
                    if let Some(c) = case_level {
                        cells.push(BatteryCell {
                            name: "Case".into(),
                            level: c,
                            is_charging: false,
                        });
                    }

                    let battery_level = if let Some(m) = main_level {
                        m
                    } else if let (Some(l), Some(r)) = (left_level, right_level) {
                        l.min(r)
                    } else if let Some(l) = left_level {
                        l
                    } else if let Some(r) = right_level {
                        r
                    } else if let Some(c) = case_level {
                        c
                    } else {
                        continue;
                    };

                    let peripheral_type = match minor_type {
                        "Headset" | "Headphones" => PeripheralType::Headset,
                        "Mouse" => PeripheralType::Mouse,
                        "Keyboard" => PeripheralType::Keyboard,
                        "Gamepad" => PeripheralType::Gamepad,
                        "Speaker" => PeripheralType::Other,
                        _ => {
                            let lower = name.to_lowercase();
                            if lower.contains("buds")
                                || lower.contains("airpods")
                                || lower.contains("headphones")
                                || lower.contains("headset")
                                || lower.contains("earphone")
                            {
                                PeripheralType::Headset
                            } else if lower.contains("mouse") {
                                PeripheralType::Mouse
                            } else if lower.contains("keyboard") {
                                PeripheralType::Keyboard
                            } else if lower.contains("watch") {
                                PeripheralType::Watch
                            } else {
                                PeripheralType::Other
                            }
                        }
                    };

                    results.push(PeripheralInfo {
                        id: address.to_string(),
                        name: name.clone(),
                        peripheral_type,
                        battery_level: battery_level.clamp(0, 100),
                        is_charging: false,
                        cells,
                        via: Some("Bluetooth".to_string()),
                        last_updated: now,
                    });
                }
            }
        }
    }
    results
}

/// Collect all peripherals: Bluetooth, IOKit HID, and connected iPhones/iPads.
pub fn collect_all_peripherals(app: &AppHandle) -> Vec<PeripheralInfo> {
    let mut by_id: HashMap<String, PeripheralInfo> = HashMap::new();

    // 1. Bluetooth devices (earphones, headsets, audio)
    for p in scan_bluetooth_accessories() {
        by_id.insert(p.id.clone(), p);
    }

    // 2. IOKit HID devices (Magic Keyboard, Mouse, Trackpad)
    for p in scan_hid_accessories() {
        by_id.insert(p.id.clone(), p);
    }

    // 3. Connected iOS devices (iPhone, iPad)
    if let Some(state) = app.try_state::<PeripheralState>() {
        if let Ok(guard) = state.ios_cache.lock() {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            for (udid, (name, level, is_charging)) in guard.iter() {
                let lower = name.to_lowercase();
                let peripheral_type = if lower.contains("ipad") {
                    PeripheralType::Tablet
                } else {
                    PeripheralType::Phone
                };

                by_id.insert(
                    udid.clone(),
                    PeripheralInfo {
                        id: udid.clone(),
                        name: name.clone(),
                        peripheral_type,
                        battery_level: *level,
                        is_charging: *is_charging,
                        cells: Vec::new(),
                        via: Some("USB / Wi-Fi Sync".to_string()),
                        last_updated: now,
                    },
                );
            }
        }
    }

    let mut list: Vec<PeripheralInfo> = by_id.into_values().collect();
    // Sort stable: Phones/iPads first, then Headsets/AirPods, then Keyboard/Mouse, then others
    list.sort_by(|a, b| {
        let type_order = |t: &PeripheralType| match t {
            PeripheralType::Phone => 1,
            PeripheralType::Tablet => 2,
            PeripheralType::Watch => 3,
            PeripheralType::Headset => 4,
            PeripheralType::Keyboard => 5,
            PeripheralType::Mouse => 6,
            PeripheralType::Trackpad => 7,
            PeripheralType::Gamepad => 8,
            PeripheralType::Other => 9,
        };
        type_order(&a.peripheral_type)
            .cmp(&type_order(&b.peripheral_type))
            .then_with(|| a.name.cmp(&b.name))
    });

    list
}

/// Background loop to keep peripheral battery telemetry fresh without wasting battery.
pub fn start_peripheral_scanner(app: AppHandle) -> async_runtime::JoinHandle<()> {
    async_runtime::spawn(async move {
        // Initial scan
        let initial = collect_all_peripherals(&app);
        if let Some(state) = app.try_state::<PeripheralState>() {
            if let Ok(mut guard) = state.peripherals.lock() {
                *guard = initial.clone();
            }
        }
        let _ = PeripheralUpdatedEvent {
            peripherals: initial,
        }
        .emit(&app);

        let mut timer = time::interval(Duration::from_secs(8));
        loop {
            timer.tick().await;

            let updated = collect_all_peripherals(&app);
            if let Some(state) = app.try_state::<PeripheralState>() {
                if let Ok(mut guard) = state.peripherals.lock() {
                    *guard = updated.clone();
                }
            }
            let _ = PeripheralUpdatedEvent {
                peripherals: updated,
            }
            .emit(&app);
        }
    })
}

#[tauri::command]
#[specta::specta]
pub fn get_peripherals(app: AppHandle) -> Vec<PeripheralInfo> {
    if let Some(state) = app.try_state::<PeripheralState>() {
        if let Ok(guard) = state.peripherals.lock() {
            if !guard.is_empty() {
                return guard.clone();
            }
        }
    }
    collect_all_peripherals(&app)
}

#[tauri::command]
#[specta::specta]
pub fn refresh_peripherals(app: AppHandle) -> Vec<PeripheralInfo> {
    let fresh = collect_all_peripherals(&app);
    if let Some(state) = app.try_state::<PeripheralState>() {
        if let Ok(mut guard) = state.peripherals.lock() {
            *guard = fresh.clone();
        }
    }
    let _ = PeripheralUpdatedEvent {
        peripherals: fresh.clone(),
    }
    .emit(&app);
    fresh
}

#[tauri::command]
#[specta::specta]
pub fn set_popover_height(app: AppHandle, height: f64) {
    if !height.is_finite() || height < 150.0 {
        return;
    }
    let h = height.ceil();
    let app_clone = app.clone();
    let _ = app.run_on_main_thread(move || {
        let popover = app_clone.ns_popover();
        let current_size = unsafe { popover.contentSize() };
        if (current_size.height - h).abs() < 1.0 {
            return;
        }
        unsafe {
            popover.setAnimates(true);
            popover.setContentSize(NSSize::new(352.0, h));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_bluetooth() {
        let items = scan_bluetooth_accessories();
        println!("Found {} bluetooth devices: {:?}", items.len(), items);
    }

    #[test]
    fn test_scan_hid() {
        let items = scan_hid_accessories();
        println!("Found {} HID devices: {:?}", items.len(), items);
    }
}
