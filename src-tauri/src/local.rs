use std::time::Duration;

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::{async_runtime, Manager, Runtime};
use tauri_plugin_pinia::ManagerExt;
use tauri_specta::Event;
use tokio::{select, sync::mpsc, time};
use tpower::{
    ffi::smc::{SMCConnection, SMCPowerData, SMCReadSensor},
    provider::{get_mac_ioreg, NormalizedResource},
};

use crate::event::{PowerUpdatedEvent, PreferenceEvent, StatusBarItem, WindowLoadedEvent};

pub enum SenderMessage {
    ImmediateSend,
    ChangeInterval(Duration),
    ChangeStatusBarItem(StatusBarItem),
    StatusBarShowCharging(bool),
}

pub fn status_bar_text(
    smc: &SMCPowerData,
    is_charging: bool,
    status_bar_item: &StatusBarItem,
    show_charging: bool,
) -> f32 {
    if is_charging && show_charging {
        return smc.delivery_rate;
    }
    match status_bar_item {
        StatusBarItem::System => smc.system_total,
        StatusBarItem::Screen => smc.brightness,
        StatusBarItem::Heatpipe => smc.heatpipe,
    }
}

impl PowerUpdatedEvent {
    pub fn new(value: f32) -> Self {
        Self(format!("{:.1} w", value))
    }

    pub fn new_with(
        smc: &SMCPowerData,
        is_charging: bool,
        status_bar_item: &StatusBarItem,
        show_charging: bool,
    ) -> Self {
        Self::new(status_bar_text(smc, is_charging, status_bar_item, show_charging))
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Event, Type)]
#[serde(rename_all = "camelCase")]
pub struct PowerTickEvent {
    pub data: NormalizedResource,
}

fn read_local_ioreg() -> tpower::de::IORegistry {
    match get_mac_ioreg() {
        Ok(io) => io,
        Err(e) => {
            log::warn!("Failed to read IORegistry: {:?}", e);
            Default::default()
        }
    }
}

fn emit_local_power_tick<R: Runtime>(
    app: &tauri::AppHandle<R>,
    smc: &SMCPowerData,
    status_bar_item: &StatusBarItem,
    show_charging: bool,
) {
    let io = read_local_ioreg();
    let resource: NormalizedResource = (&io, smc).into();
    if let Err(e) =
        PowerUpdatedEvent::new_with(smc, resource.is_charging, status_bar_item, show_charging)
            .emit(app)
    {
        log::warn!("failed to emit PowerUpdatedEvent: {e}");
    }
    if let Err(e) = (PowerTickEvent { data: resource }).emit(app) {
        log::warn!("failed to emit PowerTickEvent: {e}");
    }
}

pub fn start_sender<R: Runtime>(
    app: &impl Manager<R>,
    mut rx: mpsc::Receiver<SenderMessage>,
) -> async_runtime::JoinHandle<()> {
    let app = app.app_handle().clone();
    let mut smc_conn = match SMCConnection::new("AppleSMC") {
        Ok(conn) => conn,
        Err(e) => {
            log::error!("failed to open AppleSMC connection (kern={e}); local power sender disabled");
            return async_runtime::spawn(async move {
                while rx.recv().await.is_some() {}
            });
        }
    };

    let mut timer = time::interval(Duration::from_millis(
        app.pinia()
            .try_get::<u64>("preference", "updateInterval")
            .unwrap_or(2000),
    ));
    let mut status_bar_item = app
        .pinia()
        .try_get::<StatusBarItem>("preference", "statusBarItem")
        .unwrap_or(StatusBarItem::System);
    let mut show_charging = app
        .pinia()
        .try_get::<bool>("preference", "showCharging")
        .unwrap_or(true);

    async_runtime::spawn(async move {
        loop {
            select! {
                _ = timer.tick() => {
                    let smc = smc_conn.read_sensor();
                    emit_local_power_tick(&app, &smc, &status_bar_item, show_charging);
                }
                Some(msg) = rx.recv() => match msg {
                    SenderMessage::ImmediateSend => {
                        let smc = smc_conn.read_sensor();
                        emit_local_power_tick(&app, &smc, &status_bar_item, show_charging);
                    },
                    SenderMessage::ChangeInterval(interval) => {
                        let clamped = if interval < Duration::from_millis(500) {
                            log::warn!("interval is too small, clamped to 500ms");
                            Duration::from_millis(500)
                        } else if interval > Duration::from_secs(60) {
                            log::warn!("interval is too large, clamped to 60s");
                            Duration::from_secs(60)
                        } else {
                            interval
                        };
                        timer = time::interval(clamped);
                    },
                    SenderMessage::ChangeStatusBarItem(item) => {
                        status_bar_item = item;
                        let smc = smc_conn.read_sensor();
                        let io = read_local_ioreg();
                        let resource: NormalizedResource = (&io, &smc).into();
                        if let Err(e) = PowerUpdatedEvent::new_with(&smc, resource.is_charging, &status_bar_item, show_charging)
                            .emit(&app)
                        {
                            log::warn!("failed to emit PowerUpdatedEvent: {e}");
                        }
                    },
                    SenderMessage::StatusBarShowCharging(show) => {
                        show_charging = show;
                        let smc = smc_conn.read_sensor();
                        let io = read_local_ioreg();
                        let resource: NormalizedResource = (&io, &smc).into();
                        if let Err(e) = PowerUpdatedEvent::new_with(&smc, resource.is_charging, &status_bar_item, show_charging)
                            .emit(&app)
                        {
                            log::warn!("failed to emit PowerUpdatedEvent: {e}");
                        }
                    }
                }
            }
        }
    })
}

pub fn setup_sender_with_events<R: Runtime>(app: &impl Manager<R>) {
    let app = app.app_handle();
    let (sender_tx, rx) = mpsc::channel(10);
    start_sender(app, rx);

    // send an immediate update when the main window is loaded
    let tx = sender_tx.clone();
    WindowLoadedEvent::listen(app, move |_| {
        let tx = tx.clone();
        async_runtime::spawn(async move {
            if let Err(e) = tx.send(SenderMessage::ImmediateSend).await {
                log::warn!("failed to send ImmediateSend: {e}");
            }
        });
    });

    let tx = sender_tx.clone();
    PreferenceEvent::listen(app, move |event| {
        if let Some(msg) = match event.payload {
            PreferenceEvent::UpdateInterval(interval) => Some(SenderMessage::ChangeInterval(
                Duration::from_millis(interval.into()),
            )),
            PreferenceEvent::StatusBarItem(item) => Some(SenderMessage::ChangeStatusBarItem(item)),
            PreferenceEvent::StatusBarShowCharging(show) => {
                Some(SenderMessage::StatusBarShowCharging(show))
            }
            PreferenceEvent::Language(_) => {
                // No need to send, perform some menu refreshing
                None
            }
            _ => None,
        } {
            let tx = tx.clone();
            async_runtime::spawn(async move {
                if let Err(e) = tx.send(msg).await {
                    log::warn!("failed to send SenderMessage: {e}");
                }
            });
        }
    });
}
