//! U7/U8: the Tauri panel process. Per KTD7, this binary owns no
//! PipeWire connection — it is purely a client of `antibisingd` over
//! `bridge.rs`'s IPC connection. Per KTD3, the system tray is `ksni`
//! (`tray.rs`), not Tauri's built-in tray (Linux's `libappindicator`
//! backend never delivers click events to the app). `main.rs` itself is
//! wiring only: build the window, manage the bridge, spawn the tray,
//! register commands.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use tauri::Manager;

mod bridge;
mod tray;

fn main() {
    tauri::Builder::default()
        // U9: refuse a second app instance — a duplicate ksni tray
        // registration on the same D-Bus session is visibly broken
        // (two icons racing to register the same well-known name), and
        // there is exactly one window worth showing. The callback runs
        // in the *original* instance when a second launch is detected;
        // `argv`/`cwd` are unused here since relaunching never carries
        // meaningful CLI state for this app.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .setup(|app| {
            let handle = app.handle().clone();
            let bridge = bridge::spawn(handle.clone());
            tray::spawn(handle, bridge.clone());
            app.manage(bridge);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            bridge::request_snapshot,
            bridge::set_pin,
            bridge::set_preference_order,
            bridge::set_threshold,
            bridge::toggle_denoise,
            bridge::start_monitor,
            bridge::stop_monitor,
            bridge::panel_meters_on,
            bridge::panel_meters_off,
            bridge::connection_state,
            bridge::pull_state,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the antibising panel");
}
