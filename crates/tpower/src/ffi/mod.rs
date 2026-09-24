use std::marker::{PhantomData, PhantomPinned};

use core_foundation::{
    array::CFArrayRef, base::CFTypeRef, dictionary::CFDictionaryRef,
    propertylist::CFPropertyListFormat, string::CFStringRef,
};
use libc::{c_char, c_void};

pub mod ioreport;
pub mod smc;
pub mod wrapper;

pub use core_foundation;

#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct AMDevice {
    _data: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

pub type AMDeviceRef = *const c_void;

#[derive(Copy, Clone, Debug, Default)]
#[repr(C)]
pub struct AMDeviceNotification {
    // pub unknown0: c_uint,
    // pub unknown1: c_uint,
    // pub unknown2: c_uint,
    // pub callback: AMDeviceNotificationCallback,
    // pub cookie: c_uint,
    _data: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

// github.com/yury/cidre
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    feature = "specta",
    derive(specta::Type, serde::Deserialize, serde::Serialize)
)]
#[non_exhaustive]
#[repr(i32)]
pub enum Action {
    /// A device has attached. The device reference belongs to the
    /// client. It must be explicitly released, or else it will leak.
    Attached = 1,
    /// A device has detached. The device object delivered will be
    /// the same as the one delivered in the Attached notification. This
    /// device reference does not need to be released.
    Detached = 2,

    /// This notification is delivered in response to
    ///
    ///   1. A call to am::DeviceNotificationUnsubscribe().
    ///   2. An error occurred on one of the underlying notification systems
    ///      (i.e. usbmuxd or mDNSResponder crashed or stopped responding).
    ///      Unsubcribing and resubscribing may recover the notification system.
    NotificationStopped = 3,

    Paired = 4,
}

impl Action {
    /// The callback's action field comes straight from MobileDevice, so map it
    /// explicitly instead of trusting it to be a valid discriminant.
    pub const fn from_raw(raw: i32) -> Option<Self> {
        match raw {
            1 => Some(Self::Attached),
            2 => Some(Self::Detached),
            3 => Some(Self::NotificationStopped),
            4 => Some(Self::Paired),
            _ => None,
        }
    }
}

#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct AMDeviceNotificationCallbackInfo {
    pub device: AMDeviceRef,
    /// Raw [`Action`]; decode with [`Action::from_raw`].
    pub action: i32,
    pub subscription: *mut AMDeviceNotification,
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct AMDServiceConnection {
    pub unknown: [u8; 16],
    pub socket: u32,
    pub unknown2: u32,
    pub secure_io_context: *mut c_void,
    pub flags: u32,
    pub device_connection_id: u32,
    pub service_name: [c_char; 128],
}

unsafe impl Send for AMDServiceConnection {}
unsafe impl Sync for AMDServiceConnection {}

pub type AMDServiceConnectionRef = *const AMDServiceConnection;

type AMDeviceNotificationCallback =
    extern "C" fn(_: *const AMDeviceNotificationCallbackInfo, _: *mut c_void);

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(
    feature = "specta",
    derive(specta::Type, serde::Deserialize, serde::Serialize)
)]
pub enum InterfaceType {
    Unknown = 0,
    USB = 1,
    WiFi = 2,
}

impl InterfaceType {
    pub const fn from_raw(raw: i32) -> Self {
        match raw {
            1 => Self::USB,
            2 => Self::WiFi,
            _ => Self::Unknown,
        }
    }
}

/// Keys accepted in the `options` dictionary of
/// [`AMDeviceNotificationSubscribeWithOptions`] (values are `CFBoolean`).
pub mod notification_option {
    /// Receive devices reported by usbmuxd: USB devices and, if usbmuxd has a
    /// pairing record with Wi-Fi sync enabled, network devices.
    pub const ENABLE_USBMUX: &str = "NotificationOptionEnableUSBMux";
    /// Have MobileDevice itself browse Bonjour (`_apple-mobdev2._tcp`) for
    /// network devices that this host holds a pairing record for.
    pub const SEARCH_FOR_PAIRED_DEVICES: &str = "NotificationOptionSearchForPairedDevices";
    /// iOS 17+ CoreDevice/RemoteXPC transport (used by Xcode 15+).
    pub const ENABLE_REMOTE_XPC: &str = "NotificationOptionEnableRemoteXPC";
}

#[link(name = "MobileDevice", kind = "framework")]
extern "C" {
    pub fn AMDCreateDeviceList() -> CFArrayRef;
    /// Tail-calls [`AMDeviceNotificationSubscribeWithOptions`] with the same
    /// arguments and `options = NULL`, so `unknown0`/`unknown1` are the minimum
    /// interface speed and connection type.
    pub fn AMDeviceNotificationSubscribe(
        callback: AMDeviceNotificationCallback,
        unknown0: i32,
        unknown1: i32,
        context: *mut c_void,
        notification: *mut *mut AMDeviceNotification,
    ) -> i32;
    pub fn AMDeviceNotificationUnsubscribe(notification: *mut c_void);
    /// `connection_type`: 0 = any, 1 = USB only, 2 = network only; values >= 3
    /// are rejected ("Invalid connection type requested."). `ref_out` must be
    /// non-null. See [`notification_option`] for `options` keys.
    pub fn AMDeviceNotificationSubscribeWithOptions(
        callback: AMDeviceNotificationCallback,
        minimum_interface_speed: i32,
        connection_type: i32,
        context: *mut c_void,
        ref_out: *mut *mut AMDeviceNotification,
        options: CFDictionaryRef,
    ) -> i32;
    pub fn AMDeviceRetain(device: AMDeviceRef) -> AMDeviceRef;
    pub fn AMDeviceRelease(device: AMDeviceRef);
    pub fn AMDeviceCopyDeviceIdentifier(device: AMDeviceRef) -> CFStringRef;
    pub fn AMDeviceCopyValue(
        device: AMDeviceRef,
        domain: CFStringRef,
        key: CFStringRef,
    ) -> *const c_void;
    pub fn AMDeviceSetValue(
        device: AMDeviceRef,
        domain: CFStringRef,
        key: CFStringRef,
        value: CFTypeRef,
    ) -> i32;
    /// Raw interface type; decode with [`InterfaceType::from_raw`].
    pub fn AMDeviceGetInterfaceType(device: AMDeviceRef) -> i32;
    pub fn AMDeviceConnect(device: AMDeviceRef) -> i32;
    pub fn AMDeviceDisconnect(device: AMDeviceRef) -> i32;
    pub fn AMDeviceIsPaired(device: AMDeviceRef) -> i32;
    pub fn AMDevicePair(device: AMDeviceRef) -> i32;
    pub fn AMDeviceValidatePairing(device: AMDeviceRef) -> i32;
    pub fn AMDeviceStartSession(device: AMDeviceRef) -> i32;
    pub fn AMDeviceStopSession(device: AMDeviceRef) -> i32;
    pub fn AMDeviceSecureStartService(
        device: AMDeviceRef,
        service_name: CFStringRef,
        options: CFDictionaryRef,
        service_connection: *const AMDServiceConnectionRef,
    ) -> i32;
    pub fn AMDServiceConnectionInvalidate(connection: AMDServiceConnectionRef);
    pub fn AMDServiceConnectionGetSocket(connection: AMDServiceConnectionRef) -> i32;
    pub fn AMDServiceConnectionSendMessage(
        connection: AMDServiceConnectionRef,
        message: CFDictionaryRef,
        format: CFPropertyListFormat,
    ) -> i32;
    pub fn AMDServiceConnectionReceiveMessage(
        connection: AMDServiceConnectionRef,
        response: *mut CFDictionaryRef,
        format: *const CFPropertyListFormat,
        unknown0: *const c_void,
        unknown1: *const c_void,
        unknown2: *const c_void,
    ) -> i32;
}
