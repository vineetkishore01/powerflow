use std::{
    collections::VecDeque,
    ffi::CString,
    mem,
    ops::{Deref, Div},
    time::Duration,
};

use anyhow::bail;
use core_foundation::{
    base::{kCFAllocatorDefault, mach_port_t, TCFType},
    dictionary::{CFDictionary, CFMutableDictionaryRef},
};
use derive_more::Add;
use io_kit_sys::{
    ret::kIOReturnSuccess, IOMasterPort, IOObjectRelease, IORegistryEntryCreateCFProperties,
    IOServiceGetMatchingService, IOServiceMatching,
};
use ratatui::widgets::SparklineBar;
use serde::{Deserialize, Serialize};

use crate::{
    de::{repr, IORegistry},
    ffi::{smc::SMCPowerData, InterfaceType},
    util::{dict_into, skip_until},
};

pub mod remote;

pub use crate::ffi::ioreport::{get_apple_soc_power, get_apple_soc_power_breakdown, SocPowerBreakdown};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub struct NormalizedResource {
    pub is_local: bool,
    pub is_charging: bool,
    pub time_remain: Duration,
    pub last_update: i64,
    pub adapter_name: Option<String>,
    pub cycle_count: i32,
    pub current_capacity: i32,
    pub max_capacity: i32,
    #[serde(default)]
    pub design_capacity: i32,
    pub not_charging_reason: Option<i64>,
    #[serde(flatten)]
    pub data: NormalizedData,
}

#[derive(Debug, Clone, Copy, Default, Add, Deserialize, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub struct NormalizedData {
    pub system_in: f32,
    pub system_load: f32,
    pub battery_power: f32,
    pub adapter_power: f32,
    pub efficiency_loss: f32,
    /// 0 if not available
    pub brightness_power: f32,
    /// 0 if not available
    pub heatpipe_power: f32,
    pub battery_level: i32,
    pub absolute_battery_level: f32,
    pub temperature: f32,

    pub adapter_watts: f32,
    pub adapter_voltage: f32,
    pub adapter_amperage: f32,

    #[serde(default)]
    pub cpu_power: f32,
    #[serde(default)]
    pub gpu_power: f32,
    #[serde(default)]
    pub battery_voltage: f32,
    #[serde(default)]
    pub battery_amperage: f32,
}

impl NormalizedData {
    pub fn max_with(self, other: &Self) -> Self {
        Self {
            system_in: self.system_in.max(other.system_in),
            system_load: self.system_load.max(other.system_load),
            battery_power: self.battery_power.max(other.battery_power),
            adapter_power: self.adapter_power.max(other.adapter_power),
            efficiency_loss: self.efficiency_loss.max(other.efficiency_loss),
            battery_level: self.battery_level.max(other.battery_level),
            absolute_battery_level: self
                .absolute_battery_level
                .max(other.absolute_battery_level),
            temperature: self.temperature.max(other.temperature),
            brightness_power: self.brightness_power.max(other.brightness_power),
            heatpipe_power: self.heatpipe_power.max(other.heatpipe_power),
            adapter_watts: self.adapter_watts.max(other.adapter_watts),
            adapter_voltage: self.adapter_voltage.max(other.adapter_voltage),
            adapter_amperage: self.adapter_amperage.max(other.adapter_amperage),
            cpu_power: self.cpu_power.max(other.cpu_power),
            gpu_power: self.gpu_power.max(other.gpu_power),
            battery_voltage: self.battery_voltage.max(other.battery_voltage),
            battery_amperage: self.battery_amperage.max(other.battery_amperage),
        }
    }
}

impl Div<f32> for NormalizedData {
    type Output = Self;

    fn div(self, rhs: f32) -> Self::Output {
        Self {
            system_in: self.system_in / rhs,
            system_load: self.system_load / rhs,
            battery_power: self.battery_power / rhs,
            adapter_power: self.adapter_power / rhs,
            efficiency_loss: self.efficiency_loss / rhs,
            brightness_power: self.brightness_power / rhs,
            heatpipe_power: self.heatpipe_power / rhs,
            battery_level: self.battery_level / rhs as i32,
            absolute_battery_level: self.absolute_battery_level / rhs,
            temperature: self.temperature / rhs,
            adapter_watts: self.adapter_watts / rhs,
            adapter_voltage: self.adapter_voltage / rhs,
            adapter_amperage: self.adapter_amperage / rhs,
            cpu_power: self.cpu_power / rhs,
            gpu_power: self.gpu_power / rhs,
            battery_voltage: self.battery_voltage / rhs,
            battery_amperage: self.battery_amperage / rhs,
        }
    }
}

impl Deref for NormalizedResource {
    type Target = NormalizedData;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

fn real_capacity_from(io: &IORegistry) -> (i32, i32, i32) {
    let bd = io.battery_data.as_ref();
    let max_cap = bd
        .and_then(|b| (b.full_charge_capacity > 0).then_some(b.full_charge_capacity))
        .or_else(|| io.nominal_charge_capacity.filter(|&c| c > 0))
        .unwrap_or(io.apple_raw_max_capacity);
    let cur_cap = bd
        .and_then(|b| (b.remaining_capacity > 0).then_some(b.remaining_capacity))
        .unwrap_or(io.apple_raw_current_capacity);
    let design_cap = bd
        .and_then(|b| (b.design_capacity > 0).then_some(b.design_capacity))
        .unwrap_or(io.design_capacity);
    (max_cap, cur_cap, design_cap)
}

impl From<&IORegistry> for NormalizedResource {
    fn from(io: &IORegistry) -> Self {
        let (system_in, system_load, battery_power, adapter_power, efficiency_loss) =
            if let Some(d) = io.ptd() {
                (
                    d.system_power_in as f32 / 1000.,
                    d.system_load as f32 / 1000.,
                    d.battery_power as f32 / 1000.,
                    (d.system_power_in + d.adapter_efficiency_loss) as f32 / 1000.,
                    d.adapter_efficiency_loss as f32 / 1000.,
                )
            } else {
                // iOS/iPadOS remote device: calculate from InstantAmperage × Voltage
                // InstantAmperage is in mA, Voltage is in mV
                // Power (W) = mA × mV / 1,000,000
                let battery_power =
                    (io.instant_amperage.abs() as f32 * io.voltage as f32) / 1_000_000.0;

                let adapter_watts = io.adapter_details.watts.unwrap_or(0) as f32;
                let system_in = if io.is_charging {
                    adapter_watts.max(battery_power)
                } else {
                    0.0
                };
                let system_load = if io.is_charging {
                    (system_in - battery_power).max(0.0)
                } else {
                    battery_power
                };

                (system_in, system_load, battery_power, system_in, 0.0)
            };

        let time_remain = if io.time_remaining >= 65535 || io.time_remaining <= 0 {
            Duration::ZERO
        } else {
            Duration::from_secs(io.time_remaining as u64 * 60)
        };
        let is_charging = io.is_charging || io.adapter_details.watts.map_or(false, |w| w > 0);
        let (max_capacity, current_capacity, design_capacity) = real_capacity_from(io);
        let brightness_power = 0.0;

        Self {
            is_local: false,
            is_charging,
            time_remain,
            last_update: io.update_time,
            adapter_name: io
                .adapter_details
                .name
                .clone()
                .or_else(|| io.adapter_details.description.clone()),
            cycle_count: io.cycle_count,
            max_capacity,
            design_capacity,
            not_charging_reason: io.not_charging_reason,
            current_capacity,
            data: NormalizedData {
                system_in,
                system_load,
                battery_power,
                adapter_power,
                efficiency_loss,
                brightness_power,
                heatpipe_power: 0.,
                battery_level: io.current_capacity,
                absolute_battery_level: if max_capacity > 0 {
                    current_capacity as f32 / max_capacity as f32 * 100.
                } else {
                    io.current_capacity as f32
                },
                temperature: io.temperature as f32 / 100.,

                adapter_watts: io.adapter_details.watts.unwrap_or_default() as f32,
                adapter_voltage: io.adapter_details.adapter_voltage.unwrap_or_default() as f32
                    / 1000.,
                adapter_amperage: io.adapter_details.current.unwrap_or_default() as f32 / 1000.,
                ..Default::default()
            },
        }
    }
}

pub fn get_apple_backlight_power() -> Option<f32> {
    let name = CString::new("AppleARMBacklight").ok()?;
    let matching = unsafe { IOServiceMatching(name.as_ptr()) };
    let service = unsafe { IOServiceGetMatchingService(0, matching) };
    if service == 0 {
        return None;
    }
    let mut properties: CFMutableDictionaryRef = std::ptr::null_mut();
    let ret = unsafe { IORegistryEntryCreateCFProperties(service, &mut properties, kCFAllocatorDefault, 0) };
    unsafe { IOObjectRelease(service) };
    if ret != kIOReturnSuccess || properties.is_null() {
        return None;
    }
    let dict: CFDictionary = unsafe { CFDictionary::wrap_under_create_rule(properties) };
    let param_key = core_foundation::string::CFString::new("IODisplayParameters");
    let params_ref = dict.find(param_key.as_concrete_TypeRef() as *const std::ffi::c_void)?;
    let params_dict: CFDictionary = unsafe { CFDictionary::wrap_under_get_rule(*params_ref as _) };

    let b_key = core_foundation::string::CFString::new("brightness");
    let b_ref = params_dict.find(b_key.as_concrete_TypeRef() as *const std::ffi::c_void)?;
    let b_dict: CFDictionary = unsafe { CFDictionary::wrap_under_get_rule(*b_ref as _) };

    let val_key = core_foundation::string::CFString::new("value");
    let max_key = core_foundation::string::CFString::new("max");

    let val_ref = b_dict.find(val_key.as_concrete_TypeRef() as *const std::ffi::c_void)?;
    let max_ref = b_dict.find(max_key.as_concrete_TypeRef() as *const std::ffi::c_void)?;

    let cf_val = unsafe { core_foundation::number::CFNumber::wrap_under_get_rule(*val_ref as _) };
    let cf_max = unsafe { core_foundation::number::CFNumber::wrap_under_get_rule(*max_ref as _) };

    let val = cf_val.to_i64()? as f32;
    let max = cf_max.to_i64()? as f32;
    if max <= 0.0 {
        return None;
    }
    let ratio = (val / max).clamp(0.0, 1.0);
    // Typical MacBook Air / Pro SDR display draws ~0.15W to ~4.0W across brightness range
    let power = if ratio < 0.001 {
        0.0
    } else {
        0.15 + 3.85 * ratio.powf(1.4)
    };
    Some(power)
}

impl From<(&IORegistry, &SMCPowerData)> for NormalizedResource {
    fn from((io, smc): (&IORegistry, &SMCPowerData)) -> Self {
        let is_connected_to_power = smc.is_charging()
            || io.adapter_details.watts.map_or(false, |w| w > 0)
            || smc.delivery_rate > 0.5;
        let is_charging = is_connected_to_power;
        let is_actively_charging = io.is_charging;
        let time_remain = if is_charging {
            if !is_actively_charging || smc.time_to_full >= 65535.0 || smc.time_to_full <= 0.0 {
                Duration::ZERO
            } else {
                Duration::from_secs_f32(60.0 * smc.time_to_full)
            }
        } else {
            if smc.time_to_empty >= 65535.0 || smc.time_to_empty <= 0.0 {
                Duration::ZERO
            } else {
                Duration::from_secs_f32(60.0 * smc.time_to_empty)
            }
        };
        let (max_capacity, current_capacity, design_capacity) = real_capacity_from(io);
        let brightness_power = if smc.brightness > 0.0 {
            smc.brightness
        } else {
            get_apple_backlight_power().unwrap_or(0.0)
        };
        let cycle_count = if io.cycle_count > 0 {
            io.cycle_count
        } else if smc.cycle_count > 0 {
            smc.cycle_count as i32
        } else {
            io.cycle_count
        };
        let battery_voltage = io.voltage as f32 / 1000.0;
        let battery_amperage = io.amperage.abs() as f32 / 1000.0;
        let battery_power = if is_charging {
            if io.is_charging {
                smc.battery_rate
                    .max(smc.delivery_rate - smc.system_total)
                    .max((io.amperage.max(0) as f32 * io.voltage as f32) / 1_000_000.0)
            } else {
                // Connected to AC but battery cell charging is paused/held: AC Passthrough
                0.0
            }
        } else if smc.battery_rate > 0.05 {
            smc.battery_rate
        } else {
            (io.amperage.abs() as f32 * io.voltage as f32) / 1_000_000.0
        };
        let temperature = if smc.temperature > 0.0 {
            smc.temperature
        } else {
            io.temperature as f32 / 100.0
        };

        Self {
            is_local: true,
            last_update: io.update_time,
            is_charging,
            time_remain,
            adapter_name: io
                .adapter_details
                .name
                .clone()
                .or_else(|| io.adapter_details.description.clone()),
            cycle_count,
            max_capacity,
            design_capacity,
            not_charging_reason: io.not_charging_reason,
            current_capacity,
            data: NormalizedData {
                system_in: smc.delivery_rate,
                system_load: smc.system_total,
                battery_power,
                efficiency_loss: io
                    .ptd()
                    .map_or(0.0, |d| d.adapter_efficiency_loss as f32 / 1000.),
                brightness_power,
                heatpipe_power: smc.heatpipe,
                battery_level: io.current_capacity,
                absolute_battery_level: if max_capacity > 0 {
                    current_capacity as f32 / max_capacity as f32 * 100.
                } else {
                    io.current_capacity as f32
                },
                temperature,
                adapter_power: smc.delivery_rate
                    + io.ptd()
                        .map_or(0.0, |d| d.adapter_efficiency_loss as f32 / 1000.),

                adapter_watts: io.adapter_details.watts.unwrap_or_default() as f32,
                adapter_voltage: io.adapter_details.adapter_voltage.unwrap_or_default() as f32
                    / 1000.,
                adapter_amperage: io.adapter_details.current.unwrap_or_default() as f32 / 1000.,

                cpu_power: smc.cpu_power,
                gpu_power: smc.gpu_power,
                battery_voltage,
                battery_amperage,
            },
        }
    }
}

pub fn get_mac_ioreg_dict() -> anyhow::Result<CFDictionary> {
    let mut master_port: mach_port_t = 0;
    if unsafe { IOMasterPort(0, &mut master_port) } != 0 {
        bail!("could not get master port");
    }
    let name = CString::new("AppleSmartBattery").unwrap();
    let matching_dict = unsafe { IOServiceMatching(name.as_ptr()) };

    let result = unsafe { IOServiceGetMatchingService(master_port, matching_dict) };
    if result == 0 {
        bail!("AppleSmartBattery service not found");
    }

    let mut properties: CFMutableDictionaryRef = unsafe { mem::zeroed() };
    let ret = unsafe { IORegistryEntryCreateCFProperties(result, &mut properties, kCFAllocatorDefault, 0) };
    unsafe { IOObjectRelease(result) };
    if ret != kIOReturnSuccess || properties.is_null() {
        bail!("could not get properties");
    }

    unsafe { Ok(CFDictionary::wrap_under_create_rule(properties)) }
}

pub fn get_mac_ioreg() -> anyhow::Result<IORegistry> {
    let dic = get_mac_ioreg_dict()?;
    let r: repr::IORegistry = dict_into(dic)?;
    Ok(r.into())
}

#[derive(Debug)]
pub struct MergedPowerData {
    pub from: PowerDataFrom,
    pub smc: Option<SMCPowerData>,
    pub ioreg: IORegistry,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PowerDataFrom {
    #[default]
    Local,
    Remote((String, String, InterfaceType)),
}

impl Deref for MergedPowerData {
    type Target = IORegistry;

    fn deref(&self) -> &Self::Target {
        &self.ioreg
    }
}

#[derive(Debug, Default)]
pub struct PowerStatistic {
    pub max_battery_power: f32,
    pub max_input_power: f32,
    pub max_system_power: f32,

    pub battery_history: VecDeque<u64>,
    pub input_history: VecDeque<u64>,
    pub system_history: VecDeque<u64>,
}

impl PowerStatistic {
    pub fn update(&mut self, battery_power: f32, input_power: f32, system_power: f32) {
        if battery_power > self.max_battery_power {
            self.max_battery_power = battery_power;
        }

        if input_power > self.max_input_power {
            self.max_input_power = input_power;
        }

        if system_power > self.max_system_power {
            self.max_system_power = system_power;
        }

        self.battery_history.push_back(battery_power.abs() as u64);
        if self.battery_history.len() > 50 {
            self.battery_history.pop_front();
        }

        self.input_history.push_back(input_power.abs() as u64);
        if self.input_history.len() > 50 {
            self.input_history.pop_front();
        }

        self.system_history.push_back(system_power.abs() as u64);
        if self.system_history.len() > 200 {
            self.system_history.pop_front();
        }
    }

    pub fn battery_history(&self, width: usize) -> Vec<SparklineBar> {
        skip_until(self.battery_history.iter(), width)
            .map(|v| SparklineBar::from(*v))
            .collect()
    }

    pub fn input_history(&self, width: usize) -> Vec<SparklineBar> {
        skip_until(self.input_history.iter(), width)
            .map(|v| SparklineBar::from(*v))
            .collect()
    }

    pub fn system_history(&self, width: usize) -> Vec<SparklineBar> {
        skip_until(self.system_history.iter(), width)
            .map(|v| SparklineBar::from(*v))
            .collect()
    }
}
