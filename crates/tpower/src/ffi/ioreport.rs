//! Apple Silicon SoC power via `libIOReport.dylib` ("Energy Model" channels).
//! Does not require root.

use std::{
    ptr,
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

use core_foundation::{
    array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef},
    base::{kCFAllocatorDefault, CFRelease, CFTypeRef, TCFType},
    dictionary::{
        CFDictionaryCreateMutableCopy, CFDictionaryGetValue, CFDictionaryRef,
        CFMutableDictionaryRef,
    },
    string::{CFString, CFStringRef},
};
use libc::c_void;

type CVoidRef = *const c_void;
pub type IOReportSubscriptionRef = *const c_void;

#[link(name = "IOReport", kind = "dylib")]
extern "C" {
    pub fn IOReportCopyAllChannels(a: u64, b: u64) -> CFDictionaryRef;
    pub fn IOReportCopyChannelsInGroup(
        group: CFStringRef,
        subgroup: CFStringRef,
        a: u64,
        b: u64,
        c: u64,
    ) -> CFDictionaryRef;
    pub fn IOReportCreateSubscription(
        a: CVoidRef,
        desired_channels: CFMutableDictionaryRef,
        subbed_channels: *mut CFMutableDictionaryRef,
        channel_id: u64,
        b: CFTypeRef,
    ) -> IOReportSubscriptionRef;
    pub fn IOReportCreateSamples(
        subscription: IOReportSubscriptionRef,
        subbed_channels: CFMutableDictionaryRef,
        a: CFTypeRef,
    ) -> CFDictionaryRef;
    pub fn IOReportCreateSamplesDelta(
        prev: CFDictionaryRef,
        current: CFDictionaryRef,
        a: CFTypeRef,
    ) -> CFDictionaryRef;
    pub fn IOReportChannelGetGroup(item: CFDictionaryRef) -> CFStringRef;
    pub fn IOReportChannelGetChannelName(item: CFDictionaryRef) -> CFStringRef;
    pub fn IOReportChannelGetUnitLabel(item: CFDictionaryRef) -> CFStringRef;
    pub fn IOReportSimpleGetIntegerValue(item: CFDictionaryRef, index: i32) -> i64;
}

const ENERGY_MODEL_GROUP: &str = "Energy Model";

/// Per-component Apple Silicon SoC power in Watts.
#[derive(Debug, Clone, Copy, Default)]
pub struct SocPowerBreakdown {
    pub total: f32,
    pub cpu: f32,
    pub gpu: f32,
    pub ane: f32,
    pub dram: f32,
}

struct Sampler {
    subscription: IOReportSubscriptionRef,
    channels: CFMutableDictionaryRef,
    prev: Option<(CFDictionaryRef, Instant)>,
}

// SAFETY: the CF objects are only ever accessed while holding `SAMPLER`'s lock.
unsafe impl Send for Sampler {}

enum State {
    Uninit,
    Ready(Sampler),
    Unavailable,
}

static SAMPLER: Mutex<State> = Mutex::new(State::Uninit);

fn cfstr_to_string(s: CFStringRef) -> Option<String> {
    if s.is_null() {
        return None;
    }
    Some(unsafe { CFString::wrap_under_get_rule(s) }.to_string())
}

impl Sampler {
    fn new() -> Option<Self> {
        let group = CFString::new(ENERGY_MODEL_GROUP);
        let group_channels = unsafe {
            IOReportCopyChannelsInGroup(group.as_concrete_TypeRef(), ptr::null(), 0, 0, 0)
        };
        if group_channels.is_null() {
            return None;
        }
        let channels =
            unsafe { CFDictionaryCreateMutableCopy(kCFAllocatorDefault, 0, group_channels) };
        unsafe { CFRelease(group_channels as CFTypeRef) };
        if channels.is_null() {
            return None;
        }

        let mut subbed: CFMutableDictionaryRef = ptr::null_mut();
        let subscription = unsafe {
            IOReportCreateSubscription(ptr::null(), channels, &mut subbed, 0, ptr::null())
        };
        if !subbed.is_null() {
            unsafe { CFRelease(subbed as CFTypeRef) };
        }
        if subscription.is_null() {
            unsafe { CFRelease(channels as CFTypeRef) };
            return None;
        }

        Some(Self {
            subscription,
            channels,
            prev: None,
        })
    }

    fn sample(&self) -> Option<(CFDictionaryRef, Instant)> {
        let s = unsafe { IOReportCreateSamples(self.subscription, self.channels, ptr::null()) };
        (!s.is_null()).then(|| (s, Instant::now()))
    }

    fn set_prev(&mut self, sample: (CFDictionaryRef, Instant)) {
        if let Some((old, _)) = self.prev.replace(sample) {
            unsafe { CFRelease(old as CFTypeRef) };
        }
    }

    /// Returns SoC power (CPU, GPU, ANE, DRAM and their total) in Watts.
    fn read(&mut self) -> Option<SocPowerBreakdown> {
        if self.prev.is_none() {
            let first = self.sample()?;
            self.set_prev(first);
            thread::sleep(Duration::from_millis(50));
        }

        let (prev, prev_time) = self.prev?;
        let current = self.sample()?;
        let delta = unsafe { IOReportCreateSamplesDelta(prev, current.0, ptr::null()) };
        let elapsed = current.1.duration_since(prev_time).as_secs_f64();
        self.set_prev(current);
        if delta.is_null() {
            return None;
        }

        let result = if elapsed > 0.0 {
            Some(sum_energy_watts(delta, elapsed))
        } else {
            None
        };
        unsafe { CFRelease(delta as CFTypeRef) };
        result
    }
}

fn sum_energy_watts(delta: CFDictionaryRef, elapsed_secs: f64) -> SocPowerBreakdown {
    let key = CFString::from_static_string("IOReportChannels");
    let items = unsafe {
        CFDictionaryGetValue(delta, key.as_concrete_TypeRef() as CVoidRef) as CFArrayRef
    };
    if items.is_null() {
        return SocPowerBreakdown::default();
    }

    let (mut cpu, mut gpu, mut ane, mut dram) = (0.0, 0.0, 0.0, 0.0);
    let count = unsafe { CFArrayGetCount(items) };
    for i in 0..count {
        let item = unsafe { CFArrayGetValueAtIndex(items, i) } as CFDictionaryRef;
        if item.is_null() {
            continue;
        }
        let group = cfstr_to_string(unsafe { IOReportChannelGetGroup(item) });
        if group.as_deref() != Some(ENERGY_MODEL_GROUP) {
            continue;
        }
        let Some(name) = cfstr_to_string(unsafe { IOReportChannelGetChannelName(item) }) else {
            continue;
        };
        let unit = cfstr_to_string(unsafe { IOReportChannelGetUnitLabel(item) })
            .unwrap_or_default();
        let divisor = match unit.trim() {
            "mJ" => 1e3,
            "uJ" => 1e6,
            "nJ" => 1e9,
            _ => continue,
        };
        let energy = unsafe { IOReportSimpleGetIntegerValue(item, 0) } as f64;
        let watts = energy / elapsed_secs / divisor;

        if name.ends_with("CPU Energy") {
            cpu += watts;
        } else if name == "GPU Energy" {
            gpu += watts;
        } else if name.starts_with("ANE") {
            ane += watts;
        } else if name.starts_with("DRAM") {
            dram += watts;
        }
    }
    SocPowerBreakdown {
        total: (cpu + gpu + ane + dram) as f32,
        cpu: cpu as f32,
        gpu: gpu as f32,
        ane: ane as f32,
        dram: dram as f32,
    }
}

/// Apple Silicon SoC power breakdown (CPU, GPU, ANE, DRAM) in Watts,
/// averaged over the interval since the previous call.
pub fn get_apple_soc_power_breakdown() -> Option<SocPowerBreakdown> {
    let mut state = SAMPLER.lock().unwrap_or_else(|e| e.into_inner());
    if matches!(*state, State::Uninit) {
        *state = match Sampler::new() {
            Some(s) => State::Ready(s),
            None => State::Unavailable,
        };
    }
    match &mut *state {
        State::Ready(sampler) => sampler.read(),
        _ => None,
    }
}

/// Total Apple Silicon SoC power (CPU + GPU + ANE + DRAM) in Watts,
/// averaged over the interval since the previous call.
pub fn get_apple_soc_power() -> Option<f32> {
    get_apple_soc_power_breakdown().map(|b| b.total)
}
