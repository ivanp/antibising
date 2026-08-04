fn main() {
    let attributes = tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "request_snapshot",
            "set_pin",
            "set_preference_order",
            "set_threshold",
            "toggle_denoise",
            "start_monitor",
            "stop_monitor",
            "start_meter",
            "stop_meter",
            "connection_state",
            "pull_state",
        ]),
    );
    tauri_build::try_build(attributes).expect("failed to run tauri-build");
}
