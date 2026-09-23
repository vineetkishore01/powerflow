use std::ffi::CString;
use core_foundation::{base::TCFType, dictionary::{CFDictionary, CFMutableDictionaryRef}};
use io_kit_sys::{
    ret::kIOReturnSuccess,
    IOObjectRelease, IORegistryEntryCreateCFProperties, IOServiceGetMatchingService,
    IOServiceMatching,
};
use core_foundation::base::kCFAllocatorDefault;
use tpower::ffi::smc::SMCReadSensor;

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
    // Typical MacBook Air / Pro SDR display draws ~0.2W to ~4.0W across brightness range
    let power = if ratio < 0.001 {
        0.0
    } else {
        0.15 + 3.85 * ratio.powf(1.4)
    };
    Some(power)
}

#[test]
fn test_backlight_power() {
    let p = get_apple_backlight_power();
    println!("Computed display power: {:?}", p);
    assert!(p.is_some());
    let watts = p.unwrap();
    println!("Display wattage: {:.2} W", watts);
    assert!(watts > 0.0 && watts < 10.0);
}

#[test]
fn test_live_normalized_resource() {
    let io = tpower::provider::get_mac_ioreg().unwrap();
    let mut smc = tpower::ffi::smc::SMCConnection::new("AppleSMC").unwrap();
    let smc_data = smc.read_sensor();
    
    let res = tpower::provider::NormalizedResource::from((&io, &smc_data));
    println!("NormalizedResource: {:#?}", res);
    
    // Display consumption must NOT be 0.0W on active display
    println!("res.data.brightness_power: {} W", res.data.brightness_power);
    assert!(res.data.brightness_power > 0.0, "Display consumption must not be 0.0W");
    
    // Cycle count must match hardware
    println!("res.cycle_count: {}", res.cycle_count);
    assert!(res.cycle_count > 0, "Cycle count must be positive");
    
    // Max capacity must match nominal charge capacity
    println!("res.max_capacity: {} mAh", res.max_capacity);
    println!("res.design_capacity: {} mAh", res.design_capacity);
    let health = res.max_capacity as f32 / res.design_capacity as f32 * 100.0;
    println!("Battery health: {:.1}%", health);
    assert!(health > 90.0 && health <= 100.0, "Battery health should match macOS ~94%");
    
    // is_charging is true when connected to external power
    println!("res.is_charging: {}", res.is_charging);
    assert_eq!(res.is_charging, smc_data.is_charging() || io.adapter_details.watts.unwrap_or(0) > 0);
    
    // time_remain must not be overflow (1092 hours)
    println!("res.time_remain: {:?}", res.time_remain);
    assert!(res.time_remain.as_secs() < 86400 * 5, "Time remain must not be 45 days");
}

