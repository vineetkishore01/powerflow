use std::{
    ptr::{null, null_mut},
    time::Duration,
};

use core_foundation::{
    base::{CFType, TCFType},
    boolean::CFBoolean,
    dictionary::CFDictionaryRef,
    propertylist::{kCFPropertyListXMLFormat_v1_0, CFPropertyListFormat},
    string::CFString,
};
use scopefn::Run;

use crate::{
    cfstr,
    ffi::{
        AMDServiceConnectionGetSocket, AMDServiceConnectionInvalidate,
        AMDServiceConnectionReceiveMessage, AMDServiceConnectionRef,
        AMDServiceConnectionSendMessage, AMDeviceConnect, AMDeviceCopyDeviceIdentifier,
        AMDeviceCopyValue, AMDeviceDisconnect, AMDeviceGetInterfaceType, AMDeviceIsPaired,
        AMDevicePair, AMDeviceRef, AMDeviceSecureStartService, AMDeviceSetValue,
        AMDeviceStartSession, AMDeviceStopSession, AMDeviceValidatePairing, InterfaceType,
    },
};

pub struct ServiceConnection(pub AMDServiceConnectionRef);

unsafe impl Send for ServiceConnection {}
unsafe impl Sync for ServiceConnection {}

impl ServiceConnection {
    pub fn start(device: AMDeviceRef, service_name: &str) -> Result<Self, i32> {
        unsafe {
            let service_name = cfstr!(service_name);
            let mut service_ptr: AMDServiceConnectionRef = null_mut();

            let result = AMDeviceSecureStartService(
                device,
                service_name.as_concrete_TypeRef(),
                null_mut(),
                &mut service_ptr,
            );

            if result != 0 {
                return Err(result);
            }

            Ok(ServiceConnection(service_ptr))
        }
    }

    /// # Safety
    /// `message` must be a valid CFDictionaryRef
    pub unsafe fn send(&self, message: CFDictionaryRef) -> Result<(), i32> {
        unsafe { self.send_with_format(message, kCFPropertyListXMLFormat_v1_0) }
    }

    /// # Safety
    /// `message` must be a valid CFDictionaryRef
    pub unsafe fn send_with_format(
        &self,
        message: CFDictionaryRef,
        format: CFPropertyListFormat,
    ) -> Result<(), i32> {
        match unsafe { AMDServiceConnectionSendMessage(self.0, message, format) } {
            0 => Ok(()),
            e => Err(e),
        }
    }

    /// Returns an owned (create rule) dictionary.
    pub fn receive(&self) -> Result<CFDictionaryRef, i32> {
        unsafe {
            let mut response: CFDictionaryRef = null_mut();
            AMDServiceConnectionReceiveMessage(
                self.0,
                &mut response,
                null(),
                null(),
                null(),
                null(),
            )
            .run(|res| match res {
                0 if !response.is_null() => Ok(response),
                0 => Err(-1),
                _ => Err(res),
            })
        }
    }

    /// Bound blocking send/receive on the underlying socket. Without this a
    /// service whose peer went away (e.g. an iPhone that dropped off Wi-Fi)
    /// can block the calling thread indefinitely.
    pub fn set_timeout(&self, timeout: Duration) -> Result<(), i32> {
        let fd = unsafe { AMDServiceConnectionGetSocket(self.0) };
        if fd < 0 {
            return Err(fd);
        }
        let tv = libc::timeval {
            tv_sec: timeout.as_secs() as libc::time_t,
            tv_usec: timeout.subsec_micros() as libc::suseconds_t,
        };
        for opt in [libc::SO_RCVTIMEO, libc::SO_SNDTIMEO] {
            let res = unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    opt,
                    &tv as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::timeval>() as libc::socklen_t,
                )
            };
            if res != 0 {
                return Err(res);
            }
        }
        Ok(())
    }
}

impl Drop for ServiceConnection {
    fn drop(&mut self) {
        unsafe { AMDServiceConnectionInvalidate(self.0) }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Device {
    pub device: AMDeviceRef,
    pub udid: String,
    pub interface_type: InterfaceType,
}

unsafe impl Send for Device {}
unsafe impl Sync for Device {}

#[allow(dead_code)]
#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("couldn't connect: {0}")]
    Connect(i32),

    #[error("pairing failed: {0}")]
    Pair(i32),

    #[error("pairing validation failed: {0}")]
    Validate(i32),

    #[error("session failed: {0}")]
    Session(i32),

    #[error("device is not paired with this Mac")]
    NotPaired,
}

impl Device {
    /// # Safety
    /// `device` must be a valid AMDeviceRef
    pub unsafe fn new(device: AMDeviceRef) -> Self {
        let udid =
            unsafe { CFString::wrap_under_create_rule(AMDeviceCopyDeviceIdentifier(device)) }
                .to_string();
        Self {
            device,
            udid,
            interface_type: InterfaceType::from_raw(unsafe { AMDeviceGetInterfaceType(device) }),
        }
    }

    pub fn name(&self) -> String {
        self.copy_string(None, "DeviceName").unwrap_or_default()
    }

    /// Lockdown `GetValue`. Requires an active connection; most domains also
    /// require a session (see [`Device::prepare_device`]).
    pub fn copy_value(&self, domain: Option<&str>, key: &str) -> Option<CFType> {
        let domain = domain.map(|d| cfstr!(d));
        let value = unsafe {
            AMDeviceCopyValue(
                self.device,
                domain
                    .as_ref()
                    .map_or(null(), |d| d.as_concrete_TypeRef()),
                cfstr!(key).as_concrete_TypeRef(),
            )
        };
        if value.is_null() {
            return None;
        }
        Some(unsafe { CFType::wrap_under_create_rule(value) })
    }

    pub fn copy_string(&self, domain: Option<&str>, key: &str) -> Option<String> {
        self.copy_value(domain, key)?
            .downcast_into::<CFString>()
            .map(|s| s.to_string())
    }

    /// Lockdown `SetValue`. Requires an active session.
    pub fn set_value(&self, domain: &str, key: &str, value: &CFType) -> Result<(), i32> {
        match unsafe {
            AMDeviceSetValue(
                self.device,
                cfstr!(domain).as_concrete_TypeRef(),
                cfstr!(key).as_concrete_TypeRef(),
                value.as_CFTypeRef(),
            )
        } {
            0 => Ok(()),
            err => Err(err),
        }
    }

    /// Equivalent to Finder's "Show this iPhone when on Wi-Fi". Once set, the
    /// device advertises `_apple-mobdev2._tcp` and usbmuxd exposes it as a
    /// network device to every host holding a valid pairing record.
    /// Returns `Ok(true)` if the value was changed.
    pub fn ensure_wifi_connections_enabled(&self) -> Result<bool, i32> {
        const DOMAIN: &str = "com.apple.mobile.wireless_lockdown";
        const KEY: &str = "EnableWifiConnections";

        let enabled = self
            .copy_value(Some(DOMAIN), KEY)
            .and_then(|v| v.downcast_into::<CFBoolean>())
            .is_some_and(bool::from);
        if enabled {
            return Ok(false);
        }
        self.set_value(DOMAIN, KEY, &CFBoolean::true_value().as_CFType())?;
        Ok(true)
    }

    pub fn interface_type(&mut self) -> InterfaceType {
        let interface_type = InterfaceType::from_raw(unsafe { AMDeviceGetInterfaceType(self.device) });

        self.interface_type = interface_type;

        interface_type
    }

    pub fn connect(&self) -> Result<(), DeviceError> {
        match unsafe { AMDeviceConnect(self.device) } {
            0 => Ok(()),
            err => Err(DeviceError::Connect(err)),
        }
    }

    pub fn disconnect(&self) {
        unsafe { AMDeviceStopSession(self.device) };
        unsafe { AMDeviceDisconnect(self.device) };
    }

    pub fn is_paired(&self) -> bool {
        unsafe { AMDeviceIsPaired(self.device) == 1 }
    }

    pub fn pair(&self) -> Result<(), DeviceError> {
        match unsafe { AMDevicePair(self.device) } {
            0 => Ok(()),
            err => Err(DeviceError::Pair(err)),
        }
    }

    pub fn validate_pairing(&self) -> Result<(), DeviceError> {
        match unsafe { AMDeviceValidatePairing(self.device) } {
            0 => Ok(()),
            err => Err(DeviceError::Validate(err)),
        }
    }

    pub fn start_session(&self) -> Result<(), DeviceError> {
        match unsafe { AMDeviceStartSession(self.device) } {
            0 => Ok(()),
            err => Err(DeviceError::Session(err)),
        }
    }

    pub fn stop_session(&self) {
        unsafe {
            AMDeviceStopSession(self.device);
        }
    }

    pub fn prepare_device(&self) -> Result<(), DeviceError> {
        self.connect()?;
        if !self.is_paired() {
            self.pair()?;
        }
        self.validate_pairing()?;
        self.start_session()?;
        Ok(())
    }

    /// Like [`Device::prepare_device`] but never initiates pairing. Pairing
    /// shows the "Trust This Computer?" prompt and is only possible over USB,
    /// so network devices must already have a pairing record.
    pub fn prepare_paired_device(&self) -> Result<(), DeviceError> {
        self.connect()?;
        if !self.is_paired() {
            self.disconnect();
            return Err(DeviceError::NotPaired);
        }
        self.validate_pairing()?;
        self.start_session()?;
        Ok(())
    }

    pub fn start_service(&self, service_name: &str) -> Result<ServiceConnection, i32> {
        ServiceConnection::start(self.device, service_name)
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        self.stop_session();
        self.disconnect();
    }
}
