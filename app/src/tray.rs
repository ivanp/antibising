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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tauri::{AppHandle, Manager};


const MIC_SIZE: i32 = 22;

const WHITE: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];
const GREEN: [u8; 4] = [0xFF, 0x00, 0xCC, 0x44];

fn is_mic_shape(x: i32, y: i32) -> bool {
    // Top cap (semicircle approximation)
    if y == 2 && (9..=13).contains(&x) { return true; }
    if y == 3 && (8..=14).contains(&x) { return true; }
    // Body
    if (4..=12).contains(&y) && (8..=14).contains(&x) { return true; }
    // Bottom of body (rounded)
    if y == 13 && (9..=13).contains(&x) { return true; }
    // Cradle arc
    if y == 14 && ((7..=8).contains(&x) || (14..=15).contains(&x)) { return true; }
    if y == 15 && (x == 7 || x == 15) { return true; }
    if y == 16 && (8..=14).contains(&x) && !(9..=13).contains(&x) { return true; }
    // Stem
    if (16..=17).contains(&y) && (10..=12).contains(&x) { return true; }
    // Base
    if y == 18 && (8..=14).contains(&x) { return true; }
    false
}

fn render_icon(level: f32) -> Vec<u8> {
    let size = MIC_SIZE;
    let mut buf = vec![0u8; (size * size * 4) as usize];
    let clamped = level.clamp(0.0, 1.0);

    // The mic body (the fillable region) spans rows BODY_TOP..=BODY_BOTTOM.
    // Cradle, stem, and base (rows 14–18) are structural and stay white.
    const BODY_TOP: i32 = 2;
    const BODY_BOTTOM: i32 = 13;
    const BODY_HEIGHT: i32 = BODY_BOTTOM - BODY_TOP + 1; // 12 rows

    // fill_row: rows at-and-below this get green (within the body).
    // level 0.0 → fill_row = BODY_BOTTOM + 1 (nothing filled)
    // level 1.0 → fill_row = BODY_TOP (everything filled)
    let fill_row = BODY_BOTTOM + 1 - (clamped * BODY_HEIGHT as f32) as i32;

    for y in 0..size {
        for x in 0..size {
            if is_mic_shape(x, y) {
                let offset = ((y * size + x) * 4) as usize;
                // Green fill only in the body region (rows 2–13).
                let color = if y >= fill_row && y <= BODY_BOTTOM {
                    GREEN
                } else {
                    WHITE
                };
                buf[offset..offset + 4].copy_from_slice(&color);
            }
        }
    }
    buf
}

/// The ksni tray. Holds an `AppHandle` (Send+Sync, per
/// `docs/ksni-tauri-coexistence.md` §4) to show/focus the panel window
/// from `activate()`, and the shared `Bridge` to read current state for
/// `icon_name()`/`icon_pixmap()`/`menu()` and to send `set_pin`/
/// `toggle_denoise` commands from menu actions — the same bridge the
/// webview's commands use, so pinning from the tray menu is not a
/// second code path.
pub struct AntibisingTray {
    app: AppHandle,
    bridge: Arc<Bridge>,
    /// Smoothed audio level (0.0–1.0) for the tray icon's green fill.
    smoothed_level: f32,
    /// Monotonically increasing counter bumped on watcher_online to
    /// force a tooltip change so ksni re-announces after a waybar restart.
    refresh_seq: u32,
    /// Set by watcher_online (synchronous, &self only), consumed by the
    /// update loop's next tick to bump refresh_seq.
    force_icon_refresh: Arc<AtomicBool>,
}

impl Tray for AntibisingTray {
    fn id(&self) -> String {
        "antibising".into()
    }

    fn title(&self) -> String {
        "antibising".into()
    }

    /// Health-derived icon name. When Linked, returns an empty string so
    /// waybar falls through to `icon_pixmap()` (KTD4 — waybar resolves
    /// IconName first; a valid theme name would prevent the pixmap from
    /// ever being consulted). Non-Linked states return their themed
    /// icons as before.
    fn icon_name(&self) -> String {
        match self.bridge.last_health() {
            Some(HealthStatus::Linked { .. }) => String::new(),
            Some(HealthStatus::Broken { .. }) => "dialog-warning-symbolic".into(),
            Some(HealthStatus::Reconnecting) => "view-refresh-symbolic".into(),
            Some(HealthStatus::SilentNoDevice) | None => {
                "microphone-sensitivity-muted-symbolic".into()
            }
        }
    }

    /// Custom pixmap for the level meter (KTD4). When Linked, returns
    /// the microphone silhouette with green fill proportional to
    /// `smoothed_level`. Non-Linked states return an empty vec so
    /// waybar uses `icon_name()` instead.
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        if matches!(self.bridge.last_health(), Some(HealthStatus::Linked { .. })) {
            vec![ksni::Icon {
                width: MIC_SIZE,
                height: MIC_SIZE,
                data: render_icon(self.smoothed_level),
            }]
        } else {
            vec![]
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let mut description = match self.bridge.last_health() {
            Some(HealthStatus::Linked { description, .. }) => description,
            Some(HealthStatus::Broken { reason }) => format!("Broken: {reason}"),
            Some(HealthStatus::Reconnecting) => "Reconnecting…".to_string(),
            Some(HealthStatus::SilentNoDevice) => "No microphone".to_string(),
            None => "Daemon unreachable".to_string(),
        };
        // Append an invisible revision tag so a watcher_online bump
        // changes the tooltip hash even when the text is unchanged —
        // ksni only emits NewToolTip when the hash differs.
        if self.refresh_seq > 0 {
            use std::fmt::Write;
            let _ = write!(description, " (r{})", self.refresh_seq);
        }
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

    /// Bar/watcher restart: set the force-refresh flag so the update
    /// loop's next tick bumps `refresh_seq`, changing the tooltip hash
    /// and forcing ksni to re-announce all properties (including
    /// IconPixmap) to the restarted host. Without this, a steady-state
    /// pixmap (silence, sustained noise) produces an empty diff and
    /// waybar stays blank after restart.
    fn watcher_online(&self) {
        self.force_icon_refresh.store(true, Ordering::Relaxed);
    }
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

/// Spawn the tray on Tauri's own tokio runtime and enter a ~15 Hz
/// update loop that reads the Bridge meter cache, applies fast-rise /
/// slow-decay smoothing, and pushes pixmap updates via the retained
/// ksni `Handle`.
///
/// The `Handle` is now retained (previously dropped) so
/// `handle.update()` can signal property changes to waybar. Each tick
/// reads the smoothed level and calls `handle.update()`, which diffs
/// properties and emits `NewIcon` only when the pixmap actually
/// changed.
pub fn spawn(app: AppHandle, bridge: Arc<Bridge>) {
    // Decay factor per tick at ~15 Hz: half-life ~266ms, zero-out ~1.9s.
    const DECAY_FACTOR: f32 = 0.85;
    const LEVEL_FLOOR: f32 = 0.01;
    const UPDATE_INTERVAL: Duration = Duration::from_millis(66);

    let force_flag = Arc::new(AtomicBool::new(false));

    tauri::async_runtime::spawn(async move {
        // Subscribe to the meter stream — always-on (R2, KD2).
        match bridge.acquire_meter() {
            Ok(()) => eprintln!("tray: acquire_meter succeeded"),
            Err(e) => eprintln!("tray: acquire_meter FAILED: {e}"),
        }

        let tray = AntibisingTray {
            app,
            bridge,
            smoothed_level: 0.0,
            refresh_seq: 0,
            force_icon_refresh: force_flag,
        };
        let handle = match tray.spawn().await {
            Ok(h) => h,
            Err(e) => {
                eprintln!("tray: failed to start ksni service: {e}");
                return;
            }
        };

        let mut interval = tokio::time::interval(UPDATE_INTERVAL);
        loop {
            interval.tick().await;
            handle
                .update(|tray: &mut AntibisingTray| {
                    // Read the latest meter frame from Bridge's cache.
                    let meter = tray.bridge.last_meter();
                    if let Some(frame) = meter {
                        // Map RMS to 0.0–1.0 display level. Post-denoise
                        // speech at desk distance produces RMS ~0.01–0.05;
                        // a linear ×2 scale leaves that invisible. Use a
                        // power curve: sqrt(rms / reference) where reference
                        // 0.05 = "normal speech ≈ full". The sqrt compresses
                        // dynamic range so quiet speech is still visible.
                        let reference = 0.05_f32;
                        let level = f32::min(1.0, (frame.rms / reference).sqrt());
                        // Fast rise, slow decay.
                        tray.smoothed_level =
                            f32::max(level, tray.smoothed_level * DECAY_FACTOR);
                    } else {
                        // No data — decay only.
                        tray.smoothed_level *= DECAY_FACTOR;
                    }
                    // Clamp to zero below the floor to avoid rendering noise.
                    if tray.smoothed_level < LEVEL_FLOOR {
                        tray.smoothed_level = 0.0;
                    }
                    // Force-refresh after a watcher_online event.
                    if tray.force_icon_refresh.swap(false, Ordering::Relaxed) {
                        tray.refresh_seq = tray.refresh_seq.wrapping_add(1);
                    }
                })
                .await;
        }
    });
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_icon_output_size() {
        let buf = render_icon(0.0);
        assert_eq!(buf.len(), (MIC_SIZE * MIC_SIZE * 4) as usize);
    }

    #[test]
    fn render_icon_zero_has_no_green() {
        let buf = render_icon(0.0);
        for y in 0..MIC_SIZE {
            for x in 0..MIC_SIZE {
                let off = ((y * MIC_SIZE + x) * 4) as usize;
                let pixel = &buf[off..off + 4];
                assert_ne!(pixel, &GREEN, "green pixel found at ({x}, {y}) with level 0.0");
            }
        }
    }

    #[test]
    fn render_icon_full_body_is_all_green() {
        let buf = render_icon(1.0);
        // Body pixels (rows 2–13) should all be green at level 1.0.
        // Structural pixels (cradle/stem/base, rows 14–18) stay white.
        for y in 0..MIC_SIZE {
            for x in 0..MIC_SIZE {
                if is_mic_shape(x, y) {
                    let off = ((y * MIC_SIZE + x) * 4) as usize;
                    let pixel = &buf[off..off + 4];
                    if y <= 13 {
                        assert_eq!(pixel, &GREEN, "body pixel at ({x}, {y}) should be green at level 1.0");
                    } else {
                        assert_eq!(pixel, &WHITE, "structural pixel at ({x}, {y}) should be white at level 1.0");
                    }
                }
            }
        }
    }

    #[test]
    fn render_icon_half_has_both_colors() {
        let buf = render_icon(0.5);
        let mut has_green = false;
        let mut has_white = false;
        for y in 0..MIC_SIZE {
            for x in 0..MIC_SIZE {
                if is_mic_shape(x, y) {
                    let off = ((y * MIC_SIZE + x) * 4) as usize;
                    let pixel = &buf[off..off + 4];
                    if pixel == &GREEN { has_green = true; }
                    if pixel == &WHITE { has_white = true; }
                }
            }
        }
        assert!(has_green, "no green pixels at level 0.5");
        assert!(has_white, "no white pixels at level 0.5");
    }

    #[test]
    fn render_icon_non_shape_pixels_transparent() {
        let buf = render_icon(0.5);
        for y in 0..MIC_SIZE {
            for x in 0..MIC_SIZE {
                if !is_mic_shape(x, y) {
                    let off = ((y * MIC_SIZE + x) * 4) as usize;
                    assert_eq!(buf[off], 0x00, "non-shape pixel at ({x}, {y}) has non-zero alpha");
                }
            }
        }
    }
}