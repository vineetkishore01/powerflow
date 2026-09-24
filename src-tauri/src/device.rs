//! iPhone / iPad (and paired Apple Watch) telemetry over MobileDevice.
//!
//! MobileDevice delivers devices from usbmuxd, which knows about a device over
//! Wi-Fi only if this Mac holds a pairing record for it *and* the device has
//! `com.apple.mobile.wireless_lockdown/EnableWifiConnections` set (Finder's
//! "Show this iPhone when on Wi-Fi"). Pairing is only possible over USB, so a
//! device must be plugged in once; the USB worker below then enables Wi-Fi
//! connections so that every later session can be wireless.

use std::{
    collections::{HashMap, HashSet},
    ffi::c_void,
    mem::ManuallyDrop,
    ops::Deref,
    ptr::null_mut,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc as std_mpsc, Arc, Mutex, RwLock,
    },
    thread,
    time::{Duration, Instant},
};

use derive_more::derive::Deref;
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::{async_runtime, AppHandle, Manager};
use tauri_specta::Event;
use tokio::sync::mpsc;
use tpower::{
    ffi::{
        core_foundation::{
            base::TCFType,
            boolean::CFBoolean,
            dictionary::CFDictionary,
            runloop::{CFRunLoopGetCurrent, CFRunLoopRun, CFRunLoopStop},
            string::CFString,
        },
        notification_option,
        wrapper::{Device, DeviceError, ServiceConnection},
        AMDeviceNotification, AMDeviceNotificationCallbackInfo,
        AMDeviceNotificationSubscribeWithOptions, AMDeviceNotificationUnsubscribe,
        AMDeviceRef, AMDeviceRelease, AMDeviceRetain, Action, InterfaceType,
    },
    provider::{
        remote::{get_companion_devices, get_device_ioreg, get_lockdown_battery},
        NormalizedResource,
    },
};

use crate::{
    event::DeviceEvent,
    peripheral::{clear_remote, set_remote, unix_now, PeripheralInfo, PeripheralType},
};

#[derive(Default, Deref)]
pub struct DeviceState(RwLock<HashMap<String, (String, HashSet<InterfaceType>)>>);

#[derive(Serialize, Deserialize, Debug, Clone, Event, Type)]
#[serde(rename_all = "camelCase")]
pub struct DevicePowerTickEvent {
    pub udid: String,
    pub data: NormalizedResource,
}

/// USB: a long-lived `diagnostics_relay` connection is cheap, so poll fast
/// enough for the live power chart.
const USB_POLL: Duration = Duration::from_secs(2);
/// Wi-Fi: every poll wakes the phone's radio, so poll slowly and reconnect
/// each time (sessions on a sleeping phone go stale silently).
const WIFI_POLL: Duration = Duration::from_secs(30);
const WIFI_MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);
/// After this many consecutive failed Wi-Fi polls the device is treated as gone
/// even if MobileDevice never sent `Detached` (e.g. after usbmuxd restarted).
const WIFI_MAX_FAILURES: u32 = 8;
/// Watch values are relayed over Bluetooth by the phone; don't ask often.
const COMPANION_POLL: Duration = Duration::from_secs(2 * 60);
/// How long a USB worker keeps retrying while "Trust This Computer?" is shown.
const TRUST_TIMEOUT: Duration = Duration::from_secs(3 * 60);
const SERVICE_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug)]
pub struct DeviceMessage {
    /// Retained (+1) `AMDeviceRef` for `Attached`, borrowed otherwise.
    raw: usize,
    udid: String,
    interface: InterfaceType,
    action: Action,
}

struct ListenerContext {
    tx: mpsc::UnboundedSender<DeviceMessage>,
    /// Set while we unsubscribe ourselves, which also delivers
    /// `NotificationStopped`.
    unsubscribing: AtomicBool,
}

extern "C" fn notification_callback(
    info: *const AMDeviceNotificationCallbackInfo,
    context: *mut c_void,
) {
    let ctx = unsafe { &*(context as *const ListenerContext) };
    let info = unsafe { *info };

    let Some(action) = Action::from_raw(info.action) else {
        log::debug!("Ignoring unknown MobileDevice action {}", info.action);
        return;
    };
    if action == Action::NotificationStopped {
        // usbmuxd or mDNSResponder went away. Leave the run loop so the
        // listener thread can resubscribe.
        if !ctx.unsubscribing.load(Ordering::SeqCst) {
            unsafe { CFRunLoopStop(CFRunLoopGetCurrent()) };
        }
        return;
    }
    if info.device.is_null() {
        return;
    }

    // Only read identifiers here; the device is owned by its worker.
    let device = ManuallyDrop::new(unsafe { Device::new(info.device) });
    if action == Action::Attached {
        unsafe { AMDeviceRetain(info.device) };
    }
    let msg = DeviceMessage {
        raw: info.device as usize,
        udid: device.udid.clone(),
        interface: device.interface_type,
        action,
    };
    if let Err(err) = ctx.tx.send(msg) {
        if err.0.action == Action::Attached {
            unsafe { AMDeviceRelease(err.0.raw as AMDeviceRef) };
        }
    }
}

pub fn start_device_listener() -> mpsc::UnboundedReceiver<DeviceMessage> {
    let (tx, rx) = mpsc::unbounded_channel::<DeviceMessage>();

    let spawned = thread::Builder::new()
        .name("mobiledevice-notifications".into())
        .spawn(move || {
            // Leaked on purpose: MobileDevice may call back until unsubscribed,
            // and this thread lives for the whole process.
            let ctx: &'static ListenerContext = Box::leak(Box::new(ListenerContext {
                tx,
                unsubscribing: AtomicBool::new(false),
            }));

            let options = CFDictionary::from_CFType_pairs(&[
                (
                    CFString::new(notification_option::ENABLE_USBMUX),
                    CFBoolean::true_value(),
                ),
                // Also find paired devices on the LAN directly, in case
                // usbmuxd hasn't picked them up.
                (
                    CFString::new(notification_option::SEARCH_FOR_PAIRED_DEVICES),
                    CFBoolean::true_value(),
                ),
            ]);

            loop {
                let mut subscription: *mut AMDeviceNotification = null_mut();
                let err = unsafe {
                    AMDeviceNotificationSubscribeWithOptions(
                        notification_callback,
                        0,
                        0, // any connection type: USB and network
                        ctx as *const ListenerContext as *mut c_void,
                        &mut subscription,
                        options.as_concrete_TypeRef(),
                    )
                };
                if err != 0 {
                    log::error!("AMDeviceNotificationSubscribeWithOptions failed: {err:#x}");
                } else {
                    log::info!("Listening for iOS devices over USB and Wi-Fi");
                    unsafe { CFRunLoopRun() };
                    log::warn!("MobileDevice notifications stopped; resubscribing");
                    ctx.unsubscribing.store(true, Ordering::SeqCst);
                    unsafe { AMDeviceNotificationUnsubscribe(subscription.cast()) };
                    ctx.unsubscribing.store(false, Ordering::SeqCst);
                }
                // Devices still present are re-announced as `Attached` after
                // resubscribing, which replaces their workers.
                thread::sleep(Duration::from_secs(5));
            }
        });
    if let Err(e) = spawned {
        log::error!("Failed to spawn MobileDevice listener: {e}");
    }

    rx
}

type WorkerKey = (String, InterfaceType);

fn remote_key((udid, interface): &WorkerKey) -> String {
    format!("{udid}#{interface:?}")
}

struct WorkerHandle {
    raw: usize,
    generation: u64,
    stop: Arc<AtomicBool>,
    wake: std_mpsc::Sender<()>,
}

impl WorkerHandle {
    fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.wake.send(());
    }
}

type Registry = Arc<Mutex<HashMap<WorkerKey, WorkerHandle>>>;

pub fn start_device_sender(handle: AppHandle) -> async_runtime::JoinHandle<()> {
    let mut rx = start_device_listener();
    let registry: Registry = Arc::default();
    let mut next_generation = 0u64;

    async_runtime::spawn(async move {
        while let Some(DeviceMessage {
            raw,
            udid,
            interface,
            action,
        }) = rx.recv().await
        {
            let key = (udid, interface);
            match action {
                Action::Attached => {
                    let generation = next_generation;
                    next_generation += 1;
                    let stop = Arc::new(AtomicBool::new(false));
                    let (wake, wake_rx) = std_mpsc::channel();
                    let worker = Worker {
                        app: handle.clone(),
                        key: key.clone(),
                        generation,
                        stop: stop.clone(),
                        wake: wake_rx,
                        registry: registry.clone(),
                        announced: false,
                    };
                    let mut guard = registry.lock().unwrap();
                    if let Some(old) = guard.insert(
                        key.clone(),
                        WorkerHandle {
                            raw,
                            generation,
                            stop,
                            wake,
                        },
                    ) {
                        old.stop();
                    }
                    drop(guard);

                    let device = OwnedDevice::new(raw as AMDeviceRef);
                    let spawned = thread::Builder::new()
                        .name(format!("idevice-{:?}", key.1))
                        .spawn(move || worker.run(device));
                    if let Err(e) = spawned {
                        log::error!("Failed to spawn device worker: {e}");
                        registry.lock().unwrap().remove(&key);
                    }
                }
                Action::Detached => {
                    log::debug!("Device detached: {} ({:?})", key.0, key.1);
                    let mut guard = registry.lock().unwrap();
                    if guard.get(&key).is_some_and(|w| w.raw == raw) {
                        if let Some(w) = guard.remove(&key) {
                            w.stop();
                        }
                    }
                }
                Action::Paired => {
                    // The user tapped "Trust"; let a waiting USB worker retry now.
                    if let Some(w) = registry.lock().unwrap().get(&key) {
                        let _ = w.wake.send(());
                    }
                }
                _ => {}
            }
        }
    })
}

/// A device reference we hold a MobileDevice retain on.
struct OwnedDevice(ManuallyDrop<Device>);

impl OwnedDevice {
    fn new(raw: AMDeviceRef) -> Self {
        Self(ManuallyDrop::new(unsafe { Device::new(raw) }))
    }
}

impl Deref for OwnedDevice {
    type Target = Device;
    fn deref(&self) -> &Device {
        &self.0
    }
}

impl Drop for OwnedDevice {
    fn drop(&mut self) {
        let raw = self.0.device;
        // Stop the session and disconnect before giving up our reference.
        unsafe {
            ManuallyDrop::drop(&mut self.0);
            AMDeviceRelease(raw);
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum PollError {
    #[error(transparent)]
    Device(#[from] DeviceError),
    #[error("no battery data")]
    NoData,
}

struct DeviceIdentity {
    name: String,
    peripheral_type: PeripheralType,
}

impl DeviceIdentity {
    /// Session-less lockdown values, so this works before pairing is validated.
    fn read(device: &Device) -> Self {
        let name = device.name();
        let class = device.copy_string(None, "DeviceClass").unwrap_or_default();
        let peripheral_type = match class.as_str() {
            "iPad" => PeripheralType::Tablet,
            "Watch" => PeripheralType::Watch,
            "iPhone" | "iPod" => PeripheralType::Phone,
            _ if name.to_lowercase().contains("ipad") => PeripheralType::Tablet,
            _ => PeripheralType::Phone,
        };
        Self {
            name,
            peripheral_type,
        }
    }
}

struct Worker {
    app: AppHandle,
    key: WorkerKey,
    generation: u64,
    stop: Arc<AtomicBool>,
    wake: std_mpsc::Receiver<()>,
    registry: Registry,
    announced: bool,
}

impl Worker {
    fn run(mut self, device: OwnedDevice) {
        match self.key.1 {
            InterfaceType::USB => self.run_usb(&device),
            _ => self.run_network(&device),
        }
        drop(device);
        self.finish();
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Sleep for `d` or until woken. Returns `false` if the worker should stop.
    fn sleep(&self, d: Duration) -> bool {
        match self.wake.recv_timeout(d) {
            Ok(()) | Err(std_mpsc::RecvTimeoutError::Timeout) => !self.stopped(),
            Err(std_mpsc::RecvTimeoutError::Disconnected) => false,
        }
    }

    fn usb_worker_active(&self) -> bool {
        self.registry
            .lock()
            .unwrap()
            .contains_key(&(self.key.0.clone(), InterfaceType::USB))
    }

    fn announce(&mut self, identity: &DeviceIdentity) {
        if self.announced {
            return;
        }
        self.announced = true;
        log::info!(
            "iOS device available over {:?}: {} ({})",
            self.key.1,
            identity.name,
            self.key.0
        );
        let _ = DeviceEvent {
            udid: self.key.0.clone(),
            name: identity.name.clone(),
            interface: self.key.1,
            action: Action::Attached,
        }
        .emit(&self.app);
    }

    fn publish(&self, identity: &DeviceIdentity, level: u8, is_charging: bool, watches: &[PeripheralInfo]) {
        if self.stopped() {
            return;
        }
        let via = match self.key.1 {
            InterfaceType::USB => "USB",
            _ => "Wi-Fi",
        };
        let mut devices = vec![PeripheralInfo {
            id: self.key.0.clone(),
            name: identity.name.clone(),
            peripheral_type: identity.peripheral_type.clone(),
            battery_level: level.min(100),
            is_charging,
            cells: Vec::new(),
            via: Some(via.to_string()),
            last_updated: unix_now(),
        }];
        devices.extend_from_slice(watches);
        set_remote(&self.app, &remote_key(&self.key), self.generation, devices);
    }

    fn emit_power(&self, norm: &NormalizedResource) {
        let _ = DevicePowerTickEvent {
            udid: self.key.0.clone(),
            data: norm.clone(),
        }
        .emit(&self.app);
    }

    fn read_watches(&self, device: &Device, phone_name: &str) -> Vec<PeripheralInfo> {
        match get_companion_devices(device) {
            Ok(watches) => watches
                .into_iter()
                .filter_map(|w| {
                    let level = w.battery_level?;
                    Some(PeripheralInfo {
                        id: w.udid,
                        name: w.name.unwrap_or_else(|| "Apple Watch".into()),
                        peripheral_type: PeripheralType::Watch,
                        battery_level: level,
                        is_charging: w.is_charging,
                        cells: Vec::new(),
                        via: Some(format!("via {phone_name}")),
                        last_updated: unix_now(),
                    })
                })
                .collect(),
            Err(e) => {
                // iPads and phones without a watch answer with an error here.
                log::debug!("companion_proxy on {}: {e}", self.key.0);
                Vec::new()
            }
        }
    }

    fn run_usb(&mut self, device: &Device) {
        let deadline = Instant::now() + TRUST_TIMEOUT;
        let mut logged = false;
        loop {
            match device.prepare_device() {
                Ok(()) => break,
                Err(e) => {
                    device.disconnect();
                    if Instant::now() >= deadline {
                        log::warn!("Gave up on {}: {e}", self.key.0);
                        return;
                    }
                    if !logged {
                        log::info!(
                            "Waiting for {} to trust this Mac (unlock it and tap Trust): {e}",
                            self.key.0
                        );
                        logged = true;
                    }
                    if !self.sleep(Duration::from_secs(3)) {
                        return;
                    }
                }
            }
        }

        match device.ensure_wifi_connections_enabled() {
            Ok(true) => log::info!("Enabled Wi-Fi connections on {}", self.key.0),
            Ok(false) => {}
            Err(e) => log::warn!("Couldn't enable Wi-Fi connections on {}: {e:#x}", self.key.0),
        }

        let identity = DeviceIdentity::read(device);
        self.announce(&identity);

        let mut conn: Option<ServiceConnection> = None;
        let mut watches = Vec::new();
        let mut next_companion = Instant::now();
        loop {
            if conn.is_none() {
                conn = start_diagnostics(device);
            }
            let reading = conn.as_ref().and_then(|c| match get_device_ioreg(c) {
                Ok(res) => Some(NormalizedResource::from(&res)),
                Err(e) => {
                    log::warn!("diagnostics_relay on {}: {e}", self.key.0);
                    None
                }
            });
            match reading {
                Some(norm) => {
                    self.emit_power(&norm);
                    let level = norm.battery_level.clamp(0, 100) as u8;
                    let charging = norm.battery_power > 0.0 || norm.is_charging;
                    self.publish(&identity, level, charging, &watches);
                }
                None => {
                    // Reconnect on the next tick; lockdown is enough meanwhile.
                    conn = None;
                    if let Some(b) = get_lockdown_battery(device) {
                        self.publish(&identity, b.level, b.is_charging, &watches);
                    }
                }
            }

            if Instant::now() >= next_companion {
                watches = self.read_watches(device, &identity.name);
                next_companion = Instant::now() + COMPANION_POLL;
            }

            if !self.sleep(USB_POLL) {
                return;
            }
        }
    }

    fn run_network(&mut self, device: &Device) {
        let mut failures = 0u32;
        let mut watches = Vec::new();
        let mut next_companion = Instant::now();
        loop {
            // While the same device is on USB, that worker has better data.
            if !self.usb_worker_active() {
                let result = self.poll_network(device, &mut watches, &mut next_companion);
                device.disconnect();
                match result {
                    Ok(()) => failures = 0,
                    Err(PollError::Device(DeviceError::NotPaired)) => {
                        log::info!(
                            "{} is on the network but not paired with this Mac; connect it over USB once",
                            self.key.0
                        );
                        return;
                    }
                    Err(e) => {
                        failures += 1;
                        log::debug!("Wi-Fi poll of {} failed ({failures}): {e}", self.key.0);
                        if failures >= WIFI_MAX_FAILURES {
                            return;
                        }
                    }
                }
            }

            let delay = if failures == 0 {
                WIFI_POLL
            } else {
                (WIFI_POLL * failures).min(WIFI_MAX_BACKOFF)
            };
            if !self.sleep(delay) {
                return;
            }
        }
    }

    fn poll_network(
        &mut self,
        device: &Device,
        watches: &mut Vec<PeripheralInfo>,
        next_companion: &mut Instant,
    ) -> Result<(), PollError> {
        device.prepare_paired_device()?;
        let identity = DeviceIdentity::read(device);

        let norm = start_diagnostics(device)
            .and_then(|c| get_device_ioreg(&c).ok())
            .map(|res| NormalizedResource::from(&res));
        let battery = get_lockdown_battery(device);

        let (level, charging) = match (&norm, battery) {
            (_, Some(b)) => (b.level, b.is_charging),
            (Some(n), None) => (
                n.battery_level.clamp(0, 100) as u8,
                n.battery_power > 0.0 || n.is_charging,
            ),
            (None, None) => return Err(PollError::NoData),
        };

        if Instant::now() >= *next_companion {
            *watches = self.read_watches(device, &identity.name);
            *next_companion = Instant::now() + COMPANION_POLL;
        }

        self.announce(&identity);
        if let Some(norm) = &norm {
            self.emit_power(norm);
        }
        self.publish(&identity, level, charging, watches);
        Ok(())
    }

    fn finish(self) {
        let mut guard = self.registry.lock().unwrap();
        let superseded = guard
            .get(&self.key)
            .is_some_and(|w| w.generation != self.generation);
        if !superseded {
            guard.remove(&self.key);
        }
        drop(guard);

        clear_remote(&self.app, &remote_key(&self.key), self.generation);
        if self.announced && !superseded {
            let _ = DeviceEvent {
                udid: self.key.0.clone(),
                name: String::new(),
                interface: self.key.1,
                action: Action::Detached,
            }
            .emit(&self.app);
        }
    }
}

fn start_diagnostics(device: &Device) -> Option<ServiceConnection> {
    match device.start_service("com.apple.mobile.diagnostics_relay") {
        Ok(conn) => {
            if let Err(e) = conn.set_timeout(SERVICE_TIMEOUT) {
                log::debug!("Couldn't set diagnostics_relay timeout: {e}");
            }
            Some(conn)
        }
        Err(err) => {
            log::warn!("Failed to start diagnostics_relay for {}: {err:#x}", device.udid);
            None
        }
    }
}

pub fn setup_device_listener(app: AppHandle) {
    DeviceEvent::listen(&app.clone(), move |event| {
        let event = event.payload;
        let app_state = app.state::<DeviceState>();

        use scopefn::Run;
        app_state
            .write()
            .unwrap()
            .entry(event.udid.clone())
            .or_insert_with(|| (event.name, HashSet::new()))
            .run(|e| match event.action {
                Action::Attached => {
                    e.1.insert(event.interface);
                }
                Action::Detached => {
                    e.1.remove(&event.interface);
                }
                _ => (),
            });
    });
}
