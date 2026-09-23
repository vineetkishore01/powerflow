use std::ops::Deref;

use serde::{Deserialize, Serialize};

macro_rules! with_repr {
    ($(
        #[out, $($out:meta),*]
        #[repr, $repr:meta]
        #[$($meta:meta),*]
        $item:item
    )*) => {
        $(
            $(#[$meta])*
            $(#[$out])*
            $item
        )*

        pub mod repr {
            use super::*;
            $(
                $(#[$meta])*
                #[$repr]
                $item
            )*
        }
    };
}

with_repr! {
    #[out, serde(rename_all = "camelCase"), cfg_attr(feature = "specta", derive(specta::Type))]
    #[repr, serde(rename_all(deserialize = "PascalCase", serialize = "camelCase"))]
    #[derive(Debug, Clone, Deserialize, Serialize)]
    pub struct IORegistryDiagnostic {
        pub diagnostics: Diagnostics,
    }

    #[out, serde(rename_all = "camelCase"), cfg_attr(feature = "specta", derive(specta::Type))]
    #[repr, serde(rename_all(deserialize = "PascalCase", serialize = "camelCase"))]
    #[derive(Debug, Clone, Deserialize, Serialize)]
    pub struct Diagnostics {
        #[serde(rename = "IORegistry")]
        pub ioregistry: IORegistry,
    }

    #[out, serde(rename_all = "camelCase"), cfg_attr(feature = "specta", derive(specta::Type))]
    #[repr, serde(rename_all(deserialize = "PascalCase", serialize = "camelCase"))]
    #[derive(Debug, Clone, Default, Deserialize, Serialize)]
    pub struct AdapterDetails {
        #[serde(default)]
        pub adapter_voltage: Option<i32>,
        #[serde(default)]
        pub is_wireless: Option<bool>,
        #[serde(default)]
        pub watts: Option<i32>,
        #[serde(default)]
        pub name: Option<String>,
        #[serde(default)]
        pub current: Option<i32>,
        #[serde(default)]
        pub description: Option<String>,
    }

    #[out, serde(rename_all = "camelCase"), cfg_attr(feature = "specta", derive(specta::Type))]
    #[repr, serde(rename_all(deserialize = "PascalCase", serialize = "camelCase"))]
    #[derive(Debug, Clone, Default, Deserialize, Serialize)]
    pub struct PowerTelemetryData {
        #[serde(default)]
        pub adapter_efficiency_loss: i32,
        #[serde(default)]
        pub battery_power: i64,
        #[serde(default)]
        pub system_current_in: i32,
        #[serde(default)]
        pub system_energy_consumed: i64,
        #[serde(default)]
        pub system_load: i64,
        #[serde(default)]
        pub system_power_in: i32,
        #[serde(default)]
        pub system_voltage_in: i32,
    }

    #[out, serde(rename_all = "camelCase"), cfg_attr(feature = "specta", derive(specta::Type))]
    #[repr, serde(rename_all(deserialize = "PascalCase", serialize = "camelCase"))]
    #[derive(Debug, Clone, Default, Deserialize, Serialize)]
    pub struct ChargerData {
        #[serde(default)]
        pub not_charging_reason: Option<i64>,
    }

    #[out, serde(rename_all = "camelCase"), cfg_attr(feature = "specta", derive(specta::Type))]
    #[repr, serde(rename_all(deserialize = "PascalCase", serialize = "camelCase"))]
    #[derive(Debug, Clone, Default, Deserialize, Serialize)]
    pub struct IORegistry {
        #[serde(default)]
        pub adapter_details: AdapterDetails,
        #[serde(default)]
        pub charger_data: Option<ChargerData>,
        #[serde(default)]
        pub power_telemetry_data: Option<PowerTelemetryData>,
        #[serde(default)]
        pub absolute_capacity: i32,
        #[serde(default)]
        pub amperage: i32,
        #[serde(default)]
        pub voltage: i32,
        #[serde(default)]
        pub apple_raw_battery_voltage: Option<i32>,
        #[serde(default)]
        pub apple_raw_current_capacity: i32,
        #[serde(default)]
        pub apple_raw_max_capacity: i32,
        #[serde(default)]
        pub nominal_charge_capacity: Option<i32>,
        #[serde(default)]
        pub current_capacity: i32,
        #[serde(default)]
        pub cycle_count: i32,
        #[serde(default)]
        pub design_capacity: i32,
        #[serde(default)]
        pub fully_charged: bool,
        #[serde(default)]
        pub instant_amperage: i32,
        #[serde(default)]
        pub is_charging: bool,
        #[serde(default)]
        pub max_capacity: i32,
        #[serde(default)]
        pub temperature: i32,
        #[serde(default)]
        pub time_remaining: i32,
        #[serde(default)]
        pub not_charging_reason: Option<i64>,
        // TODO: check
        #[serde(default)]
        pub update_time: i64,
    }
}

impl From<repr::IORegistry> for IORegistry {
    fn from(r: repr::IORegistry) -> Self {
        Self {
            adapter_details: AdapterDetails {
                adapter_voltage: r.adapter_details.adapter_voltage,
                is_wireless: r.adapter_details.is_wireless,
                watts: r.adapter_details.watts,
                name: r.adapter_details.name,
                current: r.adapter_details.current,
                description: r.adapter_details.description,
            },
            power_telemetry_data: r.power_telemetry_data.map(|d| PowerTelemetryData {
                adapter_efficiency_loss: d.adapter_efficiency_loss,
                battery_power: d.battery_power,
                system_current_in: d.system_current_in,
                system_energy_consumed: d.system_energy_consumed,
                system_load: d.system_load,
                system_power_in: d.system_power_in,
                system_voltage_in: d.system_voltage_in,
            }),
            absolute_capacity: r.absolute_capacity,
            amperage: r.amperage,
            voltage: r.voltage,
            apple_raw_battery_voltage: r.apple_raw_battery_voltage,
            apple_raw_current_capacity: r.apple_raw_current_capacity,
            apple_raw_max_capacity: r.apple_raw_max_capacity,
            nominal_charge_capacity: r.nominal_charge_capacity,
            current_capacity: r.current_capacity,
            cycle_count: r.cycle_count,
            design_capacity: r.design_capacity,
            fully_charged: r.fully_charged,
            instant_amperage: r.instant_amperage,
            is_charging: r.is_charging,
            max_capacity: r.max_capacity,
            temperature: r.temperature,
            time_remaining: r.time_remaining,
            not_charging_reason: r
                .not_charging_reason
                .or_else(|| r.charger_data.as_ref().and_then(|c| c.not_charging_reason)),
            charger_data: r.charger_data.map(|c| ChargerData {
                not_charging_reason: c.not_charging_reason,
            }),
            update_time: r.update_time,
        }
    }
}

impl Deref for IORegistry {
    type Target = Option<PowerTelemetryData>;
    fn deref(&self) -> &Self::Target {
        &self.power_telemetry_data
    }
}

impl IORegistry {
    pub fn ptd(&self) -> Option<&PowerTelemetryData> {
        self.power_telemetry_data.as_ref()
    }
}
