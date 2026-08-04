//! U8: the ksni system tray (KTD3 — not Tauri's built-in tray; Tauri v2's
//! Linux tray relies on `libappindicator`, which never delivers click
//! events to the application — verified in
//! `docs/ksni-tauri-coexistence.md`). ksni is pure D-Bus (zbus); it never
//! touches GTK, so it runs on its own tokio task (Tauri's own runtime)
//! with zero event-loop conflict against the GTK/WebKitGTK main thread.
//!
//! Left-click (`activate`) shows/focuses the panel window; right-click
//! (`secondary_activate`, or automatically per SNI's own `ItemIsMenu`
//! rule once `menu()` returns items) shows the context menu — device
//! pick/pin, denoise toggle, quit. The menu and icon read the exact same
//! `Bridge::last` snapshot the panel itself renders from (R5: no second
//! code path asserting state the daemon hasn't confirmed), and the
//! device-pin menu action calls the identical `set_pin` bridge command
//! the panel's own dropdown uses — never a second routing path.

use crate::bridge::Bridge;
use antibisingd::ipc::Event;
use engine::HealthStatus;
use ksni::menu::{CheckmarkItem, MenuItem, StandardItem};
use ksni::{Tray, TrayMethods};
use std::sync::Arc;
use tauri::{AppHandle, Manager};

/// The ksni tray. Holds an `AppHandle` (Send+Sync, per
/// `docs/ksni-tauri-coexistence.md` §4) to show/focus the panel window
/// from `activate()`, and the shared `Bridge` to read current state for
/// `icon_name()`/`menu()` and to send `set_pin`/`toggle_denoise`
/// commands from menu actions — the same bridge the webview's commands
/// use, so pinning from the tray menu is not a second code path.
pub struct AntibisingTray {
    app: AppHandle,
    bridge: Arc<Bridge>,
}

impl Tray for AntibisingTray {
    fn id(&self) -> String {
        "antibising".into()
    }

    fn title(&self) -> String {
        "antibising".into()
    }

    /// Health-derived icon (R5: icon honesty — it reflects the daemon's
    /// own verdict, never an assumption). Falls back to the muted/silent
    /// icon when the bridge has no snapshot yet (daemon unreachable or
    /// still connecting) — an honest "nothing confirmed" state, not a
    /// false "healthy" default.
    fn icon_name(&self) -> String {
        match self.bridge.last_health() {
            Some(HealthStatus::Linked { .. }) => "microphone-sensitivity-high-symbolic".into(),
            Some(HealthStatus::Broken { .. }) => "dialog-warning-symbolic".into(),
            Some(HealthStatus::Reconnecting) => "view-refresh-symbolic".into(),
            Some(HealthStatus::SilentNoDevice) | None => {
                "microphone-sensitivity-muted-symbolic".into()
            }
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let description = match self.bridge.last_health() {
            Some(HealthStatus::Linked { description, .. }) => description,
            Some(HealthStatus::Broken { reason }) => format!("Broken: {reason}"),
            Some(HealthStatus::Reconnecting) => "Reconnecting…".to_string(),
            Some(HealthStatus::SilentNoDevice) => "No microphone".to_string(),
            None => "Daemon unreachable".to_string(),
        };
        ksni::ToolTip {
            title: "antibising".into(),
            description,
            ..Default::default()
        }
    }

    /// Left-click: show and focus the panel. `run_on_main_thread` is the
    /// documented bridge from ksni's tokio-worker-thread callback to
    /// Tauri's GTK main thread (`docs/ksni-tauri-coexistence.md` §4) —
    /// window operations are not thread-safe to call directly here.
    fn activate(&mut self, _x: i32, _y: i32) {
        let app = self.app.clone();
        let app_for_closure = app.clone();
        let _ = app.run_on_main_thread(move || {
            if let Some(window) = app_for_closure.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        });
    }

    /// Right-click menu: device pick/pin (same `set_pin` bridge command
    /// the panel's own dropdown uses — no second routing path per R3),
    /// denoise toggle, and quit. Built fresh from the bridge's current
    /// snapshot on every menu-open, so it can never show stale devices.
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let Some(snapshot) = self.bridge.last_snapshot() else {
            // No snapshot yet (daemon unreachable) -- an empty device
            // list rather than a stale or fabricated one; Quit still
            // works regardless of connection state.
            return vec![
                StandardItem {
                    label: "Daemon unreachable".into(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
                quit_item(),
            ];
        };
        let Event::Snapshot { devices, config, .. } = snapshot else {
            unreachable!("Bridge::last_snapshot only ever stores Event::Snapshot");
        };

        let mut items: Vec<MenuItem<Self>> = Vec::new();

        // Device pick/pin: "Auto (ranked)" plus every known device,
        // checkmarked against the persisted pin -- identical semantics
        // to the panel's dropdown (R3: pin is the sticky override,
        // "Auto (ranked)" is the unpinned ranked-routing mode).
        let current_pin = config.pin.clone();
        items.push(
            CheckmarkItem {
                label: "Auto (ranked)".into(),
                checked: current_pin.is_none(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.bridge.send_set_pin(None);
                }),
                ..Default::default()
            }
            .into(),
        );
        for device in &devices {
            let id = device.id.0.clone();
            let checked = current_pin.as_deref() == Some(id.as_str());
            let label = device.description.clone();
            items.push(
                CheckmarkItem {
                    label,
                    checked,
                    activate: Box::new(move |tray: &mut Self| {
                        let _ = tray.bridge.send_set_pin(Some(id.clone()));
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }

        items.push(MenuItem::Separator);

        // Denoise toggle: identical Dry-Mix semantics to the panel's own
        // button (R6 -- toggling never touches source identity).
        let denoise_on = config.denoise_enabled && config.dry_mix < 1.0;
        items.push(
            CheckmarkItem {
                label: "Denoise".into(),
                checked: denoise_on,
                activate: Box::new(move |tray: &mut Self| {
                    let _ = tray.bridge.send_toggle_denoise(!denoise_on);
                }),
                ..Default::default()
            }
            .into(),
        );

        items.push(MenuItem::Separator);
        items.push(quit_item());
        items
    }

    /// Refresh the cached menu tree right before the host displays it.
    ///
    /// **The bug this exists to fix:** ksni's `DbusMenu::get_layout`
    /// reads `Service::flattened_menu` -- a field computed once at
    /// `Service::new()` (confirmed by reading ksni's own
    /// `service.rs::build_layout`, which indexes `self.flattened_menu`,
    /// never calls `tray.menu()`). Only `icon_name()`/`tool_tip()` are
    /// read live (`service.get_icon_name()` etc. call straight through
    /// to the `Tray` trait on every property read) -- the menu tree is
    /// NOT. Overriding the default `menu_about_to_show()` (a no-op that
    /// explicitly suppresses this refresh, per its own doc comment) is
    /// what makes ksni call `update_menu()` right before the host opens
    /// the menu (`Service::run_about2show_hook`), so the pin/denoise
    /// checkmarks are never stale by the time the user actually sees
    /// them -- confirmed live: without this override, a `Linked` health
    /// icon coexisted with a menu still showing "Daemon unreachable"
    /// from the tray's very first (pre-connect) registration.
    fn menu_about_to_show(&mut self) {}

    /// Bar/watcher restart survival (U8's own test scenario): ksni
    /// re-registers the item automatically on `watcher_online` (the
    /// zbus connection itself survives a bar restart); no extra work
    /// is needed here since `icon_name`/`tool_tip` are read live on
    /// every property fetch and `menu_about_to_show` above already
    /// refreshes the menu on the newly-restarted bar's very first
    /// click.
    fn watcher_online(&self) {}
}

fn quit_item() -> MenuItem<AntibisingTray> {
    StandardItem {
        label: "Quit".into(),
        icon_name: "application-exit".into(),
        activate: Box::new(|tray: &mut AntibisingTray| {
            tray.app.exit(0);
        }),
        ..Default::default()
    }
    .into()
}

/// Spawn the tray on Tauri's own tokio runtime (`tauri::async_runtime`,
/// not a bare `tokio::spawn` -- keeps the tray on the same executor
/// Tauri already manages, per the plan's own decision to use ksni's
/// default `tokio` feature).
///
/// The `Handle` returned by `TrayMethods::spawn` is intentionally
/// dropped here: it only holds a `Weak` reference into the tray
/// service plus an update-channel sender (verified in ksni's own
/// `service::run` -- the D-Bus event loop itself is spawned as an
/// independent task holding the real `Arc`), so dropping it forfeits
/// only the ability to explicitly push an update — it does NOT stop
/// the running tray. No explicit push is needed: `icon_name`/
/// `tool_tip` are read live on every property fetch, and `menu()` is
/// refreshed via `menu_about_to_show`'s override just before the host
/// displays it (see that method's doc comment for why the override is
/// required at all).
pub fn spawn(app: AppHandle, bridge: Arc<Bridge>) {
    tauri::async_runtime::spawn(async move {
        let tray = AntibisingTray { app, bridge };
        if let Err(e) = tray.spawn().await {
            eprintln!("tray: failed to start ksni service: {e}");
        }
    });
}
