use std::time::Duration;

use core_foundation::{
    base::TCFType, boolean::CFBoolean, dictionary::CFDictionary, number::CFNumber,
    propertylist::kCFPropertyListBinaryFormat_v1_0, string::CFString,
};
use serde::Deserialize;
use thiserror::Error;

use crate::{
    cfdic, cfstr,
    de::{repr, IORegistry},
    ffi::wrapper::{Device, ServiceConnection},
    util::{dict_into, DictParseError},
};

#[derive(Debug, Error)]
pub enum DeviceDataError {
    #[error("Failed to send message: {0}")]
    Send(i32),
    #[error("Failed to receive message: {0}")]
    Receive(i32),
    #[error("Failed to parse message: {0}")]
    Parse(#[from] DictParseError),
    #[error("Failed to start service: {0}")]
    Service(i32),
    #[error("Service returned error: {0}")]
    Protocol(String),
}

pub fn get_device_ioreg(conn: &ServiceConnection) -> Result<IORegistry, DeviceDataError> {
    unsafe {
        conn.send(
            cfdic! {
                "EntryClass" = "IOPMPowerSource"
                "Request" = "IORegistry"
            }
            .as_concrete_TypeRef(),
        )
        .map_err(DeviceDataError::Send)
    }?;

    let response = unsafe {
        CFDictionary::wrap_under_create_rule(conn.receive().map_err(DeviceDataError::Receive)?)
    };

    let data = dict_into::<repr::IORegistryDiagnostic>(response)?;
    Ok(data.diagnostics.ioregistry.into())
}

/// Battery state from the lockdown `com.apple.mobile.battery` domain. This is a
/// plain lockdownd `GetValue`, so unlike `diagnostics_relay` it is cheap and
/// works for network (Wi-Fi) devices as well as USB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockdownBattery {
    pub level: u8,
    pub is_charging: bool,
    pub external_connected: bool,
}

pub fn get_lockdown_battery(device: &Device) -> Option<LockdownBattery> {
    const DOMAIN: Option<&str> = Some("com.apple.mobile.battery");

    let level = device
        .copy_value(DOMAIN, "BatteryCurrentCapacity")?
        .downcast_into::<CFNumber>()?
        .to_i64()?;
    if !(0..=100).contains(&level) {
        return None;
    }
    let flag = |key: &str| {
        device
            .copy_value(DOMAIN, key)
            .and_then(|v| v.downcast_into::<CFBoolean>())
            .is_some_and(bool::from)
    };

    Some(LockdownBattery {
        level: level as u8,
        is_charging: flag("BatteryIsCharging"),
        external_connected: flag("ExternalConnected"),
    })
}

/// A watch paired with an iPhone, as reported by the phone's
/// `com.apple.companion_proxy` lockdown service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompanionDevice {
    pub udid: String,
    pub name: Option<String>,
    pub product_type: Option<String>,
    pub battery_level: Option<u8>,
    pub is_charging: bool,
}

#[derive(Debug, Deserialize)]
struct RegistryResponse {
    #[serde(rename = "PairedDevicesArray", default)]
    paired_devices: Vec<String>,
    #[serde(rename = "Error")]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RegistryValueResponse {
    #[serde(rename = "RetrievedValueDictionary")]
    values: Option<plist::Dictionary>,
}

fn companion_request(
    conn: &ServiceConnection,
    pairs: &[(&str, &str)],
) -> Result<CFDictionary, DeviceDataError> {
    let pairs: Vec<(CFString, CFString)> =
        pairs.iter().map(|(k, v)| (cfstr!(*k), cfstr!(*v))).collect();
    let message = CFDictionary::from_CFType_pairs(&pairs);
    unsafe {
        conn.send_with_format(
            message.as_concrete_TypeRef(),
            kCFPropertyListBinaryFormat_v1_0,
        )
    }
    .map_err(DeviceDataError::Send)?;

    Ok(unsafe {
        CFDictionary::wrap_under_create_rule(conn.receive().map_err(DeviceDataError::Receive)?)
    })
}

fn companion_value(
    conn: &ServiceConnection,
    watch_udid: &str,
    key: &str,
) -> Result<Option<plist::Value>, DeviceDataError> {
    let response = companion_request(
        conn,
        &[
            ("Command", "GetValueFromRegistry"),
            ("GetValueGizmoUDIDKey", watch_udid),
            ("GetValueKeyKey", key),
        ],
    )?;
    let response = dict_into::<RegistryValueResponse>(response)?;
    Ok(response.values.and_then(|mut v| v.remove(key)))
}

/// Query every watch paired to the iPhone behind `device`. `device` must have
/// an active session. Watches that are out of range still appear, but with
/// `battery_level == None`.
pub fn get_companion_devices(device: &Device) -> Result<Vec<CompanionDevice>, DeviceDataError> {
    let conn = device
        .start_service("com.apple.companion_proxy")
        .map_err(DeviceDataError::Service)?;
    // Values are relayed from the watch over Bluetooth, so a watch that is out
    // of range makes the phone stall rather than error.
    conn.set_timeout(Duration::from_secs(4))
        .map_err(DeviceDataError::Service)?;

    let registry =
        dict_into::<RegistryResponse>(companion_request(&conn, &[("Command", "GetDeviceRegistry")])?)?;
    if let Some(err) = registry.error {
        return Err(DeviceDataError::Protocol(err));
    }

    let mut watches = Vec::with_capacity(registry.paired_devices.len());
    for udid in registry.paired_devices {
        let mut watch = CompanionDevice {
            udid,
            name: None,
            product_type: None,
            battery_level: None,
            is_charging: false,
        };
        let desynced = read_companion_values(&conn, &mut watch).is_err();
        watches.push(watch);
        // A send/receive failure (usually a timeout) leaves request/response
        // pairs out of step, so nothing after it on this connection is usable.
        if desynced {
            break;
        }
    }
    Ok(watches)
}

/// Fills `watch` in place so values read before a transport failure are kept.
/// Only transport errors are returned; per-key errors just leave `None`.
fn read_companion_values(
    conn: &ServiceConnection,
    watch: &mut CompanionDevice,
) -> Result<(), DeviceDataError> {
    let udid = watch.udid.clone();
    let get = |key: &str| match companion_value(conn, &udid, key) {
        Ok(v) => Ok(v),
        Err(e @ (DeviceDataError::Send(_) | DeviceDataError::Receive(_))) => Err(e),
        Err(_) => Ok(None),
    };

    // Battery first: it's the value we actually need if the watch stalls.
    watch.battery_level = get("BatteryCurrentCapacity")?
        .and_then(|v| v.as_signed_integer().or_else(|| v.as_real().map(|r| r as i64)))
        .filter(|l| (0..=100).contains(l))
        .map(|l| l as u8);
    watch.is_charging = get("BatteryIsCharging")?
        .and_then(|v| v.as_boolean())
        .unwrap_or(false);
    watch.name = get("DeviceName")?.and_then(plist::Value::into_string);
    watch.product_type = get("ProductType")?.and_then(plist::Value::into_string);
    Ok(())
}
