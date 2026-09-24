use std::{
    collections::{HashMap, HashSet},
    ffi::c_void,
    mem::{self, MaybeUninit},
    sync::{Arc, RwLock},
    time::Duration,
};

use derive_more::derive::Deref;
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::{async_runtime, AppHandle, Manager};
use tauri_specta::Event;
use tokio::{select, sync::mpsc, task::spawn_blocking, time};
use tpower::{
    ffi::{
        core_foundation::runloop::CFRunLoopRun,
        wrapper::{Device, ServiceConnection},
        AMDeviceNotificationCallbackInfo, AMDeviceNotificationSubscribe, Action, InterfaceType,
    },
    provider::{remote::get_device_ioreg, NormalizedResource},
};

use crate::event::DeviceEvent;

#[derive(Default, Deref)]
pub struct DeviceState(RwLock<HashMap<String, (String, HashSet<InterfaceType>)>>);

#[derive(Serialize, Deserialize, Debug, Clone, Event, Type)]
#[serde(rename_all = "camelCase")]
pub struct DevicePowerTickEvent {
    pub udid: String,
    pub data: NormalizedResource,
}

#[derive(Debug)]
pub struct DeviceMessage {
    device: Device,
    action: Action,
}

pub fn start_device_listener() -> mpsc::Receiver<DeviceMessage> {
    let (tx, rx) = mpsc::channel::<DeviceMessage>(10);

    extern "C" fn callback(info: *const AMDeviceNotificationCallbackInfo, context: *mut c_void) {
        let tx = unsafe { &*(context as *mut mpsc::Sender<DeviceMessage>) };
        let info = unsafe { *info };
        let device = unsafe { Device::new(info.device) };

        let tx = tx.clone();

        async_runtime::spawn(async move {
            tx.send(DeviceMessage {
                device,
                action: info.action,
            })
            .await
            .unwrap();
            mem::forget(tx);
        });
    }

    spawn_blocking(move || {
        let boxed = Arc::new(tx);
        let mut not = MaybeUninit::uninit();
        unsafe {
            AMDeviceNotificationSubscribe(
                callback,
                0,
                0,
                Arc::as_ptr(&boxed) as *mut _,
                not.as_mut_ptr(),
            )
        };
        unsafe { CFRunLoopRun() };
    });

    rx
}

pub fn start_device_sender(handle: AppHandle) -> async_runtime::JoinHandle<()> {
    let mut rx = start_device_listener();
    let mut timer = time::interval(Duration::from_millis(2000));

    let mut devices: HashMap<Device, ServiceConnection> = HashMap::new();

    async_runtime::spawn(async move {
        loop {
            select! {
                _ = timer.tick() => {
                    for (device, conn) in devices.iter() {
                        match get_device_ioreg(conn) {
                            Ok(res) => {
                                let norm = NormalizedResource::from(&res);
                                let _ = DevicePowerTickEvent {
                                    udid: device.udid.clone(),
                                    data: norm.clone(),
                                }.emit(&handle);

                                if let Some(p_state) = handle.try_state::<crate::peripheral::PeripheralState>() {
                                    if let Ok(mut guard) = p_state.ios_cache.lock() {
                                        guard.insert(
                                            device.udid.clone(),
                                            (
                                                device.name(),
                                                norm.battery_level as u8,
                                                norm.battery_power > 0.0 || norm.is_charging,
                                            ),
                                        );
                                    }
                                }
                            }
                            Err(err) => {
                                log::error!("Failed to get IORegistry: {err}");
                            }
                        }
                    }
                }
                Some(DeviceMessage { device, action }) = rx.recv() => {
                    match action {
                        Action::Attached => {
                            if let Err(e) = device.prepare_device() {
                                log::warn!("Failed to prepare iOS device {}: {:?}", device.udid, e);
                                continue;
                            }
                            match device.start_service("com.apple.mobile.diagnostics_relay") {
                                Ok(conn) => {
                                    let name = device.name();
                                    let _ = DeviceEvent {
                                        udid: device.udid.clone(),
                                        name,
                                        interface: device.interface_type,
                                        action,
                                    }.emit(&handle);
                                    devices.insert(device, conn);
                                }
                                Err(err) => {
                                    log::warn!("Failed to start diagnostics_relay for {}: {}", device.udid, err);
                                }
                            }
                        },
                        Action::Detached => {
                            log::debug!("Device detached: {}", device.udid);
                            if let Some(p_state) = handle.try_state::<crate::peripheral::PeripheralState>() {
                                if let Ok(mut guard) = p_state.ios_cache.lock() {
                                    guard.remove(&device.udid);
                                }
                            }
                            let _ = DeviceEvent {
                                udid: device.udid.clone(),
                                name: String::new(),
                                interface: device.interface_type,
                                action,
                            }.emit(&handle);
                            devices.remove(&device);
                        },
                        _ => ()
                    }
                }
            }
        }
    })
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
