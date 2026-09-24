use tpower::ffi::smc::SMCReadSensor;
use tpower::provider::{
    get_apple_backlight_power, get_display_linear_brightness, get_live_backlight_power,
};

#[test]
fn test_live_linear_brightness() {
    let l = get_display_linear_brightness();
    println!("Live linear brightness: {:?}", l);
    let l = l.expect("DisplayServices brightness must be readable");
    assert!((0.0..=1.0).contains(&l));
}

#[test]
fn test_backlight_power() {
    let live = get_live_backlight_power();
    println!("Live DisplayServices display power: {:?}", live);
    assert!(live.is_some(), "DisplayServices must be loadable on macOS");

    let p = get_apple_backlight_power();
    println!("Computed display power: {:?}", p);
    assert!(p.is_some());
    let watts = p.unwrap();
    println!("Display wattage: {:.2} W", watts);
    assert!(watts > 0.0 && watts < 10.0);
    // Linear model: 0.15W (min) ..= 4.0W (max)
    assert!(watts >= 0.15 && watts <= 4.0 + f32::EPSILON);
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

