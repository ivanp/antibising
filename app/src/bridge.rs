//! U7's bridge: owns the IPC client connection to `antibisingd` (KTD7 —
//! the app never talks to PipeWire directly, never holds audio state).
//! Tauri commands translate into `Request`s sent over the socket; daemon
//! `Event`s arrive on a background reader thread and are re-emitted to
//! the webview via `AppHandle::emit`, so the panel renders only
//! daemon-verified state (R5).
//!
//! **Reconnect, not relaunch.** If the daemon is unreachable — not yet
//! started, mid-restart, or crashed — the bridge reports that as its own
//! connection state (`ConnectionState::Unreachable`) rather than
//! panicking or hanging, and a background thread retries with backoff.
//! On reconnect it re-syncs from the daemon's initial `Hello`+`Snapshot`,
//! so "unreachable" heals without the user relaunching the app.

use antibisingd::ipc::{Event, IpcPaths, Request};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use engine::MeterFrame;
use tauri::{AppHandle, Emitter};

/// Event name the bridge emits every daemon `Event` under, tagged so the
/// webview's single listener can dispatch on `payload.type` (the same
/// `#[serde(tag = "type")]` shape the wire protocol already uses —
/// nothing is re-encoded, just forwarded).
pub const DAEMON_EVENT: &str = "antibising://daemon-event";

/// Emitted whenever the bridge's own connection state changes — distinct
/// from any `Event` the daemon sends, since "daemon unreachable" is a
/// bridge-local fact the daemon obviously cannot report about itself.
pub const CONNECTION_EVENT: &str = "antibising://connection-state";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    /// No connection yet, or the last one dropped; a reconnect attempt
    /// is scheduled per `RECONNECT_INTERVAL`.
    Unreachable,
    /// Connected; `Hello`+`Snapshot` handshake completed.
    Connected,
}

const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);

/// Live handle to the daemon connection, shared between Tauri commands
/// (which send requests) and the background reader thread (which owns
/// receiving). `None` while unreachable — every command checks this and
/// reports failure rather than panicking on a dead socket.
struct Connection {
    writer: UnixStream,
}

/// The last event of each per-connection-lifetime kind the bridge has
/// seen, so a webview that starts listening *after* the daemon's
/// handshake already ran (a real race: `Hello`/`Snapshot` on the fastest
/// path can beat the webview's JS finishing `listen()` registration —
/// Tauri's event bus does not queue for late listeners) can still pull
/// current state instead of waiting forever for an event that already
/// fired. `Snapshot` is the only one that matters this way — it is the
/// full-state event every client already treats as authoritative on
/// connect; `Hello` carries nothing the panel renders.
#[derive(Default)]
struct LastState {
    connection: Option<ConnectionState>,
    snapshot: Option<Event>,
}

/// Tauri-managed state: the bridge itself. One per app instance.
pub struct Bridge {
    conn: Mutex<Option<Connection>>,
    last: Mutex<LastState>,
    paths: IpcPaths,
    /// Meter lease count, guarded by a mutex so the count mutation and the
    /// Start/Stop send form one atomic transition. An AtomicUsize is not
    /// enough: tray spawn (tokio task) and panel commands (Tauri threads)
    /// acquire concurrently, and a fetch_add/send/fetch_sub rollback on a
    /// failed send could race a concurrent acquire into a state where the
    /// count claims a holder but no StartMeter ever succeeded. Holding the
    /// lock across the whole transition closes that window.
    meter_leases: Mutex<usize>,
    last_meter: Mutex<Option<MeterFrame>>,
    raw_meter_leases: Mutex<usize>,
    last_raw_meter: Mutex<Option<MeterFrame>>,
    last_health_cache: Mutex<Option<engine::HealthStatus>>,
    /// Whether the panel currently holds its meter leases, guarded by its
    /// own mutex so the panel's on-load acquire and on-unload release are a
    /// single atomic check-and-act. Makes them idempotent across webview
    /// reloads (double DOMContentLoaded, missed beforeunload on WebKitGTK):
    /// acquire only when currently inactive, release only when active.
    panel_meters_active: Mutex<bool>,
}

impl Bridge {
    fn new(paths: IpcPaths) -> Self {
        Self {
            conn: Mutex::new(None),
            last: Mutex::new(LastState::default()),
            paths,
            meter_leases: Mutex::new(0),
            last_meter: Mutex::new(None),
            raw_meter_leases: Mutex::new(0),
            last_raw_meter: Mutex::new(None),
            last_health_cache: Mutex::new(None),
            panel_meters_active: Mutex::new(false),
        }
    }

    /// Send `request` over the current connection. Returns `Err` if
    /// unreachable right now — commands surface this to the webview as
    /// a normal (non-panicking) failure; the reconnect loop is what
    /// eventually restores the connection, not the caller retrying.
    fn send(&self, request: &Request) -> Result<(), String> {
        let mut guard = self.conn.lock().expect("bridge connection mutex poisoned");
        let Some(conn) = guard.as_mut() else {
            return Err("daemon unreachable".to_string());
        };
        let mut line = serde_json::to_string(request).expect("Request must always serialize");
        line.push('\n');
        conn.writer
            .write_all(line.as_bytes())
            .map_err(|e| format!("write to daemon failed: {e}"))
    }

    /// The most recent `Event::Snapshot` the bridge has observed, if
    /// any — the same cache `pull_state` exposes to the webview, read
    /// here by U8's tray so its `menu()`/`icon_name()` never assert a
    /// device set or health verdict the daemon hasn't actually
    /// confirmed (R5), and never diverge from what the panel itself
    /// shows.
    pub fn last_snapshot(&self) -> Option<Event> {
        self.last.lock().expect("bridge last-state mutex poisoned").snapshot.clone()
    }

    /// The health verdict from the most recent `Snapshot`, if the
    /// bridge has ever received one — `None` when no connection has
    /// completed a handshake yet, which the tray's `icon_name()` treats
    /// identically to `SilentNoDevice` (an honest "nothing confirmed"
    /// icon, never a false "healthy" default).
    pub fn last_health(&self) -> Option<engine::HealthStatus> {
        self.last_health_cache.lock().expect("bridge last_health_cache mutex poisoned").clone()
    }

    /// Send `SetPin` — the identical command the panel's own dropdown
    /// issues (R3: one routing path, never a second one for the tray
    /// menu).
    pub fn send_set_pin(&self, device: Option<String>) -> Result<(), String> {
        self.send(&Request::SetPin { device })
    }

    /// Send `ToggleDenoise` — the identical command the panel's own
    /// button issues (R6: one routing path).
    pub fn send_toggle_denoise(&self, enabled: bool) -> Result<(), String> {
        self.send(&Request::ToggleDenoise { enabled })
    }

    pub fn acquire_meter(&self) -> Result<(), String> {
        // The lease count is the DESIRED-holder count (who wants metering),
        // not a wire-subscribed flag. Increment it unconditionally under the
        // lock so a holder's desire is recorded even if the StartMeter send
        // fails (daemon unreachable at spawn). The reconnect path resubscribes
        // whenever the count is > 0, so a transient send failure self-heals
        // on the next connect instead of stranding the meter until restart.
        // The lock spans the count change and the send so concurrent
        // acquires can't interleave a duplicate StartMeter.
        let mut count = self.meter_leases.lock().expect("bridge meter_leases mutex poisoned");
        *count += 1;
        if *count == 1 {
            // First holder: try to subscribe now. On failure, desire stays
            // recorded (count == 1) and reconnect will retry.
            return self.send(&Request::StartMeter);
        }
        Ok(())
    }

    pub fn release_meter(&self) {
        let mut count = self.meter_leases.lock().expect("bridge meter_leases mutex poisoned");
        if *count == 0 {
            return; // saturate at 0
        }
        *count -= 1;
        if *count == 0 {
            let _ = self.send(&Request::StopMeter);
        }
    }

    pub fn last_meter(&self) -> Option<MeterFrame> {
        self.last_meter.lock().expect("bridge last_meter mutex poisoned").clone()
    }

    pub fn acquire_raw_meter(&self) -> Result<(), String> {
        // Desire-count semantics, same as acquire_meter — a failed initial
        // send leaves the count recorded so reconnect resubscribes.
        let mut count = self.raw_meter_leases.lock().expect("bridge raw_meter_leases mutex poisoned");
        *count += 1;
        if *count == 1 {
            return self.send(&Request::StartRawMeter);
        }
        Ok(())
    }

    pub fn release_raw_meter(&self) {
        let mut count = self.raw_meter_leases.lock().expect("bridge raw_meter_leases mutex poisoned");
        if *count == 0 {
            return; // saturate at 0
        }
        *count -= 1;
        if *count == 0 {
            let _ = self.send(&Request::StopRawMeter);
        }
    }

    /// Cached latest raw frame, mirroring `last_meter`. Currently the panel
    /// consumes raw frames via the emitted `raw_meter_frame` event stream,
    /// not this cache; kept for parity with the post-denoise meter and for a
    /// future Rust-side reader (e.g. a raw-meter tray or a poll command).
    #[allow(dead_code)]
    pub fn last_raw_meter(&self) -> Option<MeterFrame> {
        self.last_raw_meter.lock().expect("bridge last_raw_meter mutex poisoned").clone()
    }

    /// Idempotently acquire both meter leases for the panel. A no-op if the
    /// panel already holds them (webview reload firing DOMContentLoaded
    /// twice, or a second panel window). The `panel_meters_active` lock is
    /// held across the check AND both acquisitions, so this is atomic against
    /// a concurrent `panel_meters_off`.
    ///
    /// Paired, deferred-success contract: both `acquire_meter` and
    /// `acquire_raw_meter` run unconditionally (no `?` short-circuit), so the
    /// panel always claims exactly one desired hold on EACH meter — even when
    /// the daemon is unreachable and the initial Start send fails. The desire
    /// counts stay recorded and the reconnect path resubscribes both whenever
    /// their count is > 0. `active` is set true once the pair is claimed so
    /// `panel_meters_off` releases exactly this pair and a reload never
    /// re-claims. Any send error is aggregated and returned for logging only.
    pub fn panel_meters_on(&self) -> Result<(), String> {
        let mut active = self
            .panel_meters_active
            .lock()
            .expect("bridge panel_meters_active mutex poisoned");
        if *active {
            return Ok(()); // Already active — idempotent no-op.
        }
        *active = true; // Claim ownership before acquiring the pair.
        let post = self.acquire_meter();
        let raw = self.acquire_raw_meter();
        post.and(raw)
    }

    /// Idempotently release both panel meter leases. A no-op if the panel
    /// doesn't currently hold them (a stray beforeunload, or a release after
    /// the leases were never acquired). Exactly one release per prior
    /// `panel_meters_on`, so a reload that both misses beforeunload AND
    /// re-runs on-load never accumulates, and a double-fire never
    /// double-decrements.
    pub fn panel_meters_off(&self) {
        let mut active = self
            .panel_meters_active
            .lock()
            .expect("bridge panel_meters_active mutex poisoned");
        if *active {
            self.release_meter();
            self.release_raw_meter();
            *active = false;
        }
    }
}

/// Spawn the bridge: a background thread that connects to `antibisingd`,
/// reads `Event`s and re-emits them to the webview, and reconnects with
/// backoff whenever the connection drops (daemon restart, crash, or not
/// started yet). Returns the `Bridge` to `.manage()` on the Tauri
/// builder; commands read it via `tauri::State`.
pub fn spawn(app: AppHandle) -> Arc<Bridge> {
    let paths = IpcPaths::production();
    let bridge = Arc::new(Bridge::new(paths));
    let bridge_for_thread = bridge.clone();
    std::thread::Builder::new()
        .name("antibising-bridge".into())
        .spawn(move || reconnect_loop(app, bridge_for_thread))
        .expect("failed to spawn bridge thread");
    bridge
}

/// Owns the connect -> read-until-drop -> reconnect cycle for the
/// bridge's whole lifetime. Every cycle re-emits `ConnectionState` so the
/// webview never has to guess: "unreachable" is emitted immediately on
/// disconnect (not just at the top before the first attempt), and
/// "connected" only after the real `Hello`+`Snapshot` handshake proves
/// the daemon is actually there, not merely that `connect()` succeeded.
fn reconnect_loop(app: AppHandle, bridge: Arc<Bridge>) {
    loop {
        match UnixStream::connect(&bridge.paths.socket_path) {
            Ok(stream) => {
                if let Err(e) = run_connection(&app, &bridge, stream) {
                    eprintln!("bridge: connection ended: {e}");
                }
            }
            Err(_) => {
                // Not connected yet (daemon not started, or between
                // restarts) -- emit unreachable once per attempt so the
                // panel's very first render already knows, rather than
                // silently retrying with no observable state.
                bridge.last.lock().expect("bridge last-state mutex poisoned").connection =
                    Some(ConnectionState::Unreachable);
                let _ = app.emit(CONNECTION_EVENT, ConnectionState::Unreachable);
            }
        }
        std::thread::sleep(RECONNECT_INTERVAL);
    }
}

/// One connection's lifetime: complete the handshake, install the
/// writer half into `bridge` for commands to use, then read `Event`
/// lines until the daemon closes the socket or a read fails — both
/// signal the same thing (daemon gone), handled identically by
/// `reconnect_loop`'s retry.
fn run_connection(
    app: &AppHandle,
    bridge: &Arc<Bridge>,
    stream: UnixStream,
) -> std::io::Result<()> {
    let writer = stream.try_clone()?;
    *bridge.conn.lock().expect("bridge connection mutex poisoned") =
        Some(Connection { writer });

    let reader = BufReader::new(stream);
    let mut connected_emitted = false;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let event: Event = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("bridge: failed to parse daemon event line: {e}");
                continue;
            }
        };
        if !connected_emitted {
            // Hello is always the first line the daemon sends -- its
            // arrival is what actually proves the daemon is live, not
            // just that the socket accepted a connection.
            bridge.last.lock().expect("bridge last-state mutex poisoned").connection =
                Some(ConnectionState::Connected);
            let _ = app.emit(CONNECTION_EVENT, ConnectionState::Connected);
            connected_emitted = true;
            // Re-subscribe to meters if a desired lease exists (tray keeps
            // the post meter always-on; the panel holds both while open).
            // A hold whose initial Start send failed while the daemon was
            // down is recorded as a count > 0, so it resubscribes here.
            if *bridge.meter_leases.lock().expect("bridge meter_leases mutex poisoned") > 0 {
                let _ = bridge.send(&Request::StartMeter);
            }
            if *bridge.raw_meter_leases.lock().expect("bridge raw_meter_leases mutex poisoned") > 0 {
                let _ = bridge.send(&Request::StartRawMeter);
            }
        }
        if matches!(event, Event::Snapshot { .. }) {
            // Cache the full-state event so a webview whose `listen()`
            // registration lands after this line already ran (the
            // fast-daemon race `LastState` exists for) can pull it via
            // `last_snapshot` instead of waiting forever for an event
            // that already fired and will never repeat on its own.
            bridge.last.lock().expect("bridge last-state mutex poisoned").snapshot =
                Some(event.clone());
        }
        match &event {
            Event::Snapshot { health, .. } => {
                *bridge.last_health_cache.lock().expect("bridge last_health_cache mutex poisoned") = Some(health.clone());
            }
            Event::HealthChanged(health) => {
                *bridge.last_health_cache.lock().expect("bridge last_health_cache mutex poisoned") = Some(health.clone());
            }
            Event::MeterFrame(frame) => {
                *bridge.last_meter.lock().expect("bridge last_meter mutex poisoned") = Some(frame.clone());
            }
            Event::RawMeterFrame(frame) => {
                *bridge.last_raw_meter.lock().expect("bridge last_raw_meter mutex poisoned") = Some(frame.clone());
            }
            _ => {}
        }
        let _ = app.emit(DAEMON_EVENT, &event);
    }

    // Reader loop ended -- connection gone (clean close or read error).
    *bridge.conn.lock().expect("bridge connection mutex poisoned") = None;
    bridge.last.lock().expect("bridge last-state mutex poisoned").connection =
        Some(ConnectionState::Unreachable);
    *bridge.last_meter.lock().expect("bridge last_meter mutex poisoned") = None;
    *bridge.last_raw_meter.lock().expect("bridge last_raw_meter mutex poisoned") = None;
    *bridge.last_health_cache.lock().expect("bridge last_health_cache mutex poisoned") = None;
    let _ = app.emit(CONNECTION_EVENT, ConnectionState::Unreachable);
    Ok(())
}

// --- Tauri commands: thin translations from webview call -> Request. ---
// Every command returns `Result<(), String>` uniformly (no command has a
// meaningful success payload -- the daemon's own `Event`s, not a command
// return value, are what update the panel, matching R5's "state served
// from the single writer" rule: a command's job is only to ask, never to
// assert the outcome itself).

#[tauri::command]
pub fn request_snapshot(bridge: tauri::State<'_, Arc<Bridge>>) -> Result<(), String> {
    bridge.send(&Request::RequestSnapshot)
}

#[tauri::command]
pub fn set_pin(bridge: tauri::State<'_, Arc<Bridge>>, device: Option<String>) -> Result<(), String> {
    bridge.send(&Request::SetPin { device })
}

#[tauri::command]
pub fn set_preference_order(
    bridge: tauri::State<'_, Arc<Bridge>>,
    order: Vec<String>,
) -> Result<(), String> {
    bridge.send(&Request::SetPreferenceOrder { order })
}

#[tauri::command]
pub fn set_threshold(bridge: tauri::State<'_, Arc<Bridge>>, value: f64) -> Result<(), String> {
    bridge.send(&Request::SetThreshold { value })
}

#[tauri::command]
pub fn toggle_denoise(bridge: tauri::State<'_, Arc<Bridge>>, enabled: bool) -> Result<(), String> {
    bridge.send(&Request::ToggleDenoise { enabled })
}

#[tauri::command]
pub fn start_monitor(bridge: tauri::State<'_, Arc<Bridge>>) -> Result<(), String> {
    bridge.send(&Request::StartMonitor)
}

#[tauri::command]
pub fn stop_monitor(bridge: tauri::State<'_, Arc<Bridge>>) -> Result<(), String> {
    bridge.send(&Request::StopMonitor)
}

/// Panel meter subscription — idempotent acquire of BOTH meters (post +
/// raw). Safe to call on every panel load; a webview reload that re-fires
/// this never accumulates leases (see `panel_meters_on`).
#[tauri::command]
pub fn panel_meters_on(bridge: tauri::State<'_, Arc<Bridge>>) -> Result<(), String> {
    bridge.panel_meters_on()
}

/// Panel meter teardown — idempotent release of both panel meter leases.
/// Safe to call on `beforeunload`; a missed or duplicated call cannot
/// corrupt the lease count (see `panel_meters_off`).
#[tauri::command]
pub fn panel_meters_off(bridge: tauri::State<'_, Arc<Bridge>>) -> Result<(), String> {
    bridge.panel_meters_off();
    Ok(())
}

/// A no-connection-yet flag some UIs want at startup — exposed as a
/// zero-arg command so the webview can ask "am I connected right now"
/// on load, rather than only reacting to the `CONNECTION_EVENT` stream
/// (which it might have missed if this command runs before the very
/// first emit).
#[tauri::command]
pub fn connection_state(bridge: tauri::State<'_, Arc<Bridge>>) -> ConnectionState {
    if bridge.conn.lock().expect("bridge connection mutex poisoned").is_some() {
        ConnectionState::Connected
    } else {
        ConnectionState::Unreachable
    }
}

/// Pull whatever state the bridge has already observed — closes the
/// real startup race where the daemon's `Hello`+`Snapshot` (or a
/// same-tick disconnect) fires before the webview's `listen()`
/// registration lands, so the corresponding `CONNECTION_EVENT`/
/// `DAEMON_EVENT` emit already happened and will never repeat. The
/// panel calls this once on load *after* registering its listeners, so
/// any event it missed is recovered here instead of leaving the panel
/// stuck on "Connecting…" forever.
#[tauri::command]
pub fn pull_state(bridge: tauri::State<'_, Arc<Bridge>>) -> (ConnectionState, Option<Event>) {
    let last = bridge.last.lock().expect("bridge last-state mutex poisoned");
    (
        last.connection.unwrap_or(ConnectionState::Unreachable),
        last.snapshot.clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The webview dispatches on `payload.type`; this locks that shape in
    /// place against an accidental serde attribute change on either
    /// `Event` (daemon/src/ipc.rs) or `ConnectionState` breaking the
    /// panel's event listener silently.
    #[test]
    fn connection_state_serializes_with_snake_case_tag() {
        let json = serde_json::to_string(&ConnectionState::Unreachable).unwrap();
        assert_eq!(json, "\"unreachable\"");
        let json = serde_json::to_string(&ConnectionState::Connected).unwrap();
        assert_eq!(json, "\"connected\"");
    }

    /// `Bridge::send` on a freshly constructed (never-connected) bridge
    /// must fail cleanly, never panic — this is the exact state a
    /// command hits if the user opens the panel before the daemon has
    /// finished starting.
    #[test]
    fn send_before_any_connection_fails_without_panicking() {
        let bridge = Bridge::new(IpcPaths::at(std::env::temp_dir().join("antibising-bridge-test-unused")));
        let result = bridge.send(&Request::RequestSnapshot);
        assert!(result.is_err(), "send on an unconnected bridge must return Err, not panic");
    }

    /// `last_snapshot`/`last_health` on a bridge that has never received
    /// any `Event::Snapshot` must return `None`, not panic or fabricate
    /// a default health verdict — this is the exact state U8's tray
    /// reads before the daemon connection completes, and its
    /// `icon_name()` depends on this being an honest "nothing confirmed
    /// yet" rather than a false-healthy default (R5).
    #[test]
    fn last_health_and_snapshot_are_none_before_any_snapshot_arrives() {
        let bridge = Bridge::new(IpcPaths::at(std::env::temp_dir().join("antibising-bridge-test-unused-2")));
        assert!(bridge.last_snapshot().is_none());
        assert_eq!(bridge.last_health(), None);
    }

    /// Once the bridge's `last_health_cache` holds a health value, `last_health`
    /// must return it — this is the data path U8's tray reads for both
    /// `icon_name()` and `menu()`, so a regression here would silently break both.
    #[test]
    fn last_health_extracts_health_from_cached_snapshot() {
        let bridge = Bridge::new(IpcPaths::at(std::env::temp_dir().join("antibising-bridge-test-unused-3")));
        *bridge.last_health_cache.lock().unwrap() = Some(engine::HealthStatus::Linked {
            device: engine::DeviceId("dev-a".to_string()),
            description: "Razer".to_string(),
        });
        assert_eq!(
            bridge.last_health(),
            Some(engine::HealthStatus::Linked {
                device: engine::DeviceId("dev-a".to_string()),
                description: "Razer".to_string(),
            })
        );
    }

    /// `acquire_meter` on a fresh bridge increments the lease counter to 1
    /// (send fails since there's no connection -- that's expected; we only
    /// care about the counter here).
    #[test]
    fn acquire_meter_increments_lease_counter() {
        let bridge = Bridge::new(IpcPaths::at(std::env::temp_dir().join("antibising-bridge-test-unused-4")));
        // acquire_meter returns Err because send fails with no connection,
        // but the counter must still be incremented.
        let _ = bridge.acquire_meter();
        assert_eq!(*bridge.meter_leases.lock().expect("test mutex"), 1);
    }

    /// `release_meter` from count 0 must not panic and must leave the count at 0.
    #[test]
    fn release_meter_from_zero_does_not_panic() {
        let bridge = Bridge::new(IpcPaths::at(std::env::temp_dir().join("antibising-bridge-test-unused-5")));
        bridge.release_meter(); // must not panic
        assert_eq!(*bridge.meter_leases.lock().expect("test mutex"), 0);
    }

    /// `last_meter` returns `None` on a fresh bridge.
    #[test]
    fn last_meter_is_none_initially() {
        let bridge = Bridge::new(IpcPaths::at(std::env::temp_dir().join("antibising-bridge-test-unused-6")));
        assert!(bridge.last_meter().is_none());
    }

    /// After manually setting `last_meter`, `last_meter()` returns the frame.
    #[test]
    fn last_meter_returns_set_frame() {
        let bridge = Bridge::new(IpcPaths::at(std::env::temp_dir().join("antibising-bridge-test-unused-7")));
        *bridge.last_meter.lock().unwrap() = Some(MeterFrame { rms: 0.5, peak: 0.7 });
        let frame = bridge.last_meter().expect("should have a frame");
        assert!((frame.rms - 0.5).abs() < f32::EPSILON);
        assert!((frame.peak - 0.7).abs() < f32::EPSILON);
    }

    /// `last_health_cache` returns `None` on a fresh bridge.
    #[test]
    fn last_health_cache_is_none_initially() {
        let bridge = Bridge::new(IpcPaths::at(std::env::temp_dir().join("antibising-bridge-test-unused-8")));
        assert!(bridge.last_health().is_none());
    }

    /// Two `acquire_meter` calls => lease count 2; one `release_meter` => count 1.
    #[test]
    fn acquire_twice_release_once_leaves_count_at_one() {
        let bridge = Bridge::new(IpcPaths::at(std::env::temp_dir().join("antibising-bridge-test-unused-9")));
        // First acquire: counter goes to 1, send fails (no connection) -- ignored.
        let _ = bridge.acquire_meter();
        assert_eq!(*bridge.meter_leases.lock().expect("test mutex"), 1);
        // Second acquire: counter goes to 2, send is not called (prev != 0).
        let _ = bridge.acquire_meter();
        assert_eq!(*bridge.meter_leases.lock().expect("test mutex"), 2);
        // One release: counter drops to 1, StopMeter not sent (old == 2, not 1).
        bridge.release_meter();
        assert_eq!(*bridge.meter_leases.lock().expect("test mutex"), 1);
    }

    /// `acquire_raw_meter` on a fresh bridge increments the raw lease to 1.
    #[test]
    fn acquire_raw_meter_increments_lease_counter() {
        let bridge = Bridge::new(IpcPaths::at(std::env::temp_dir().join("antibising-bridge-test-unused-10")));
        let _ = bridge.acquire_raw_meter();
        assert_eq!(*bridge.raw_meter_leases.lock().expect("test mutex"), 1);
    }

    /// `release_raw_meter` from count 0 must not panic and stays at 0.
    #[test]
    fn release_raw_meter_from_zero_does_not_panic() {
        let bridge = Bridge::new(IpcPaths::at(std::env::temp_dir().join("antibising-bridge-test-unused-11")));
        bridge.release_raw_meter();
        assert_eq!(*bridge.raw_meter_leases.lock().expect("test mutex"), 0);
    }

    /// `last_raw_meter` returns `None` on a fresh bridge, the stored frame after set.
    #[test]
    fn last_raw_meter_none_then_set() {
        let bridge = Bridge::new(IpcPaths::at(std::env::temp_dir().join("antibising-bridge-test-unused-12")));
        assert!(bridge.last_raw_meter().is_none());
        *bridge.last_raw_meter.lock().expect("test mutex") = Some(MeterFrame { rms: 0.3, peak: 0.4 });
        let frame = bridge.last_raw_meter().expect("should have a frame");
        assert!((frame.rms - 0.3).abs() < f32::EPSILON);
    }

    /// The raw and post meter leases are independent.
    #[test]
    fn raw_and_post_meter_leases_are_independent() {
        let bridge = Bridge::new(IpcPaths::at(std::env::temp_dir().join("antibising-bridge-test-unused-13")));
        let _ = bridge.acquire_meter();
        let _ = bridge.acquire_raw_meter();
        assert_eq!(*bridge.meter_leases.lock().expect("test mutex"), 1);
        assert_eq!(*bridge.raw_meter_leases.lock().expect("test mutex"), 1);
        bridge.release_meter();
        assert_eq!(*bridge.meter_leases.lock().expect("test mutex"), 0);
        assert_eq!(*bridge.raw_meter_leases.lock().expect("test mutex"), 1);
    }

    /// `panel_meters_on` is idempotent: two calls claim the pair exactly
    /// once, and `panel_meters_off` releases exactly that pair. A second
    /// on/off round is a no-op, so a webview reload can't accumulate leases.
    #[test]
    fn panel_meters_on_off_is_idempotent() {
        let bridge = Bridge::new(IpcPaths::at(std::env::temp_dir().join("antibising-bridge-test-unused-14")));
        let _ = bridge.panel_meters_on();
        let _ = bridge.panel_meters_on(); // reload re-fires — must not double-count
        assert_eq!(*bridge.meter_leases.lock().expect("test mutex"), 1);
        assert_eq!(*bridge.raw_meter_leases.lock().expect("test mutex"), 1);
        bridge.panel_meters_off();
        bridge.panel_meters_off(); // duplicate — must not underflow
        assert_eq!(*bridge.meter_leases.lock().expect("test mutex"), 0);
        assert_eq!(*bridge.raw_meter_leases.lock().expect("test mutex"), 0);
    }
}
