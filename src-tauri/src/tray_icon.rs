use std::process::{self, Command};

use tauri::{
    menu::{CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    ActivationPolicy, Emitter, Manager, Runtime,
};
use tauri_plugin_nspopover::{AppExt, WindowExt as _};
use tauri_specta::Event;

use crate::{event::PowerUpdatedEvent, ext::WebviewWindowExt};

pub fn is_sleep_disabled() -> bool {
    let output = Command::new("pmset").arg("-g").output();
    if let Ok(out) = output {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let lower = line.to_lowercase();
            if lower.contains("sleepdisabled") {
                if let Some(val) = lower.split_whitespace().last() {
                    return val == "1";
                }
            }
        }
    }
    false
}

pub fn set_sleep_disabled(disabled: bool) -> bool {
    let val = if disabled { "1" } else { "0" };

    // 1. Try sudo -n directly (succeeds if passwordless sudo is configured)
    if let Ok(status) = Command::new("sudo")
        .args(["-n", "/usr/bin/pmset", "-a", "disablesleep", val])
        .status()
    {
        if status.success() {
            return is_sleep_disabled() == disabled;
        }
    }

    // 2. Fallback: run with administrator privileges via AppleScript
    // Also writes a sudoers rule so subsequent toggles require zero prompts
    let script = format!(
        "do shell script \"/usr/bin/pmset -a disablesleep {val} && (mkdir -p /etc/sudoers.d && echo '%admin ALL=(ALL) NOPASSWD: /usr/bin/pmset' > /etc/sudoers.d/pmset_sleep && chmod 0440 /etc/sudoers.d/pmset_sleep || true)\" with administrator privileges"
    );

    let _ = Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .status();

    is_sleep_disabled() == disabled
}

pub fn setup_tray_icon<R: Runtime>(app: &impl Manager<R>) -> tauri::Result<()> {
    let show = MenuItemBuilder::new("Show Window").build(app)?;
    let sleep_disabled = is_sleep_disabled();
    let toggle_sleep = CheckMenuItemBuilder::new("Prevent Sleep When Lid Closed")
        .checked(sleep_disabled)
        .build(app)?;
    let quit = MenuItemBuilder::new("Quit").build(app)?;

    let menu = MenuBuilder::new(app)
        .item(&show)
        .separator()
        .item(&toggle_sleep)
        .separator()
        .item(&quit)
        .build()
        .unwrap();

    let tray_icon = TrayIconBuilder::with_id("main")
        .title("0 w")
        .menu_on_left_click(false)
        .menu(&menu)
        .build(app)
        .unwrap();

    let toggle_sleep_clone = toggle_sleep.clone();
    tray_icon.on_menu_event(move |tray_handle, event| match event.id() {
        val if val == show.id() => {
            let (window, _) = tray_handle
                .app_handle()
                .get_or_create_window("main")
                .unwrap();

            if !window.is_visible().unwrap() {
                window.show().unwrap();
                window.set_focus().unwrap();

                tray_handle
                    .app_handle()
                    .set_activation_policy(ActivationPolicy::Regular)
                    .unwrap();
            }
        }
        val if val == toggle_sleep_clone.id() => {
            let currently_disabled = is_sleep_disabled();
            let _ = set_sleep_disabled(!currently_disabled);
            let _ = toggle_sleep_clone.set_checked(is_sleep_disabled());
        }
        val if val == quit.id() => {
            tray_handle.app_handle().cleanup_before_exit();
            process::exit(0);
        }
        _ => {}
    });

    let toggle_sleep_refresh = toggle_sleep.clone();
    tray_icon.on_tray_icon_event(move |tray_handle, event| {
        tauri_plugin_positioner::on_tray_event(tray_handle.app_handle(), &event);
        match event {
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } => {
                let handle = tray_handle.app_handle();
                if handle.is_popover_shown() {
                    handle.hide_popover();
                } else {
                    crate::peripheral::sync_popover_height_for_state(handle, false);
                    handle.show_popover();
                    let _ = handle.emit("popover-opened", ());
                }
            }
            TrayIconEvent::Click {
                button: MouseButton::Right,
                ..
            } => {
                let _ = toggle_sleep_refresh.set_checked(is_sleep_disabled());
            }
            _ => {}
        }
    });

    PowerUpdatedEvent::listen(app.app_handle(), move |event| {
        tray_icon.set_title(Some(event.payload.0)).unwrap();
    });

    app.popover_window().unwrap().to_popover();

    Ok(())
}
