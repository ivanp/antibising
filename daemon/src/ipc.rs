//! U10: the daemon's local IPC surface. A Unix domain socket at a fixed
//! path under `$XDG_RUNTIME_DIR`, speaking newline-delimited JSON messages
//! — one message per line, so a stream reader can frame without a length
//! prefix. Multiple concurrent clients (panel + future CLI) are supported;
//! commands from any client funnel through the engine's single command
//! channel (`Session::command_sender`), so there is exactly one writer to
//! PipeWire regardless of client count (KTD7).
//!
//! ## Crash-safe, race-safe single-instance bind
//!
//! A SIGKILL'd daemon leaves its socket file behind. A naive `bind()` on
//! restart then fails `EADDRINUSE` forever, defeating `Restart=on-failure`.
//! Probe-then-unlink alone is racy: a second starter can catch a first
//! between `bind()` and `listen()`, read the refusal as staleness, and
//! delete a *live* socket out from under it.
//!
//! So instance ownership is a **separate `flock` lock file** beside the
//! socket, taken exclusively and non-blocking *first*:
//! - Lock acquired -> we're the only instance. Unlink any leftover socket
//!   path (guaranteed stale, since a live daemon would be holding the
//!   lock) and bind.
//! - Lock refused -> a live daemon already owns this socket; exit with a
//!   clear "already running" error, and critically, **never touch the
//!   winner's socket file**.
//!
//! The lock is held for the daemon's lifetime via an open `File` (never
//! explicitly unlocked); the kernel releases it on any process death,
//! including `SIGKILL`, which is exactly what makes the next restart's
//! `try_lock_exclusive()` succeed.
//!
//! ## Per-client write handle, no forwarder thread
//!
//! Each client's write side is one `Arc<Mutex<UnixStream>>`, registered
//! with the [`Broadcaster`] on connect. `broadcast()` locks and writes to
//! every registered handle; a client's own read loop locks the *same*
//! handle to send a direct reply (e.g. `Event::Error` for a malformed
//! request) that must go to that client alone, not everyone. One thread
//! per client (the read loop) is enough — no separate forwarder thread,
//! no per-client `mpsc` channel duplicating what the mutex already
//! provides.

use engine::{Config, HealthStatus, SessionCommand, SessionEvent};
use fs2::FileExt;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Protocol version this daemon build speaks. Carried in the hello message
/// so a future second client generation can detect a mismatch and
/// negotiate or refuse cleanly — beyond that, the schema is implementation
/// detail free to change while panel and daemon ship together (per the
/// plan's own scoping of U10).
pub const PROTOCOL_VERSION: u32 = 1;

/// Runtime paths for the IPC socket + its ownership lock. Parameterized
/// (like `InstallPaths`) so tests can point a whole daemon+client pair at
/// a scratch directory instead of the real `$XDG_RUNTIME_DIR`.
#[derive(Debug, Clone)]
pub struct IpcPaths {
    pub socket_path: PathBuf,
    pub lock_path: PathBuf,
}

impl IpcPaths {
    /// Real runtime location: `$XDG_RUNTIME_DIR/antibising/` (falls back
    /// to `/tmp/antibising-<uid>` if `XDG_RUNTIME_DIR` is unset — a
    /// non-systemd or minimal session still gets a working, if less
    /// hygienic, socket path rather than a hard failure).
    pub fn production() -> Self {
        let base = std::env::var("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let uid = unsafe { c_getuid() };
                PathBuf::from(format!("/tmp/antibising-{uid}"))
            })
            .join("antibising");
        Self::at(base)
    }

    pub fn at(dir: PathBuf) -> Self {
        Self {
            socket_path: dir.join("antibisingd.sock"),
            lock_path: dir.join("antibisingd.lock"),
        }
    }
}

// A tiny, dependency-free `getuid()` wrapper for the production runtime
// fallback path — avoids pulling in the `libc` crate for one syscall.
extern "C" {
    #[link_name = "getuid"]
    fn c_getuid() -> u32;
}

/// Take single-instance ownership of `paths`, unlink any leftover socket,
/// and bind. Returns the held lock `File` (drop it only at process exit —
/// its lifetime *is* the instance-ownership guarantee) and the bound
/// listener.
///
/// # Errors
/// [`BindError::AlreadyRunning`] if another instance holds the lock —
/// callers must treat this as a normal, expected outcome (the "socket
/// busy" case the plan's test scenarios name), not a crash.
pub fn bind_singleton(paths: &IpcPaths) -> Result<(std::fs::File, UnixListener), BindError> {
    let dir = paths
        .socket_path
        .parent()
        .expect("socket_path must have a parent directory");
    std::fs::create_dir_all(dir).map_err(BindError::Io)?;

    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&paths.lock_path)
        .map_err(BindError::Io)?;

    // Non-blocking exclusive lock: refusal means a live instance holds
    // it. This check happens *before* touching the socket path at all —
    // the race this whole design exists to close is exactly "unlink a
    // live socket because we mistook a refusal for staleness."
    lock_file
        .try_lock_exclusive()
        .map_err(|_| BindError::AlreadyRunning)?;

    // We hold the lock exclusively, so any socket file on disk right now
    // is guaranteed stale (a live daemon would be holding this same
    // lock). Safe to unlink unconditionally.
    let _ = std::fs::remove_file(&paths.socket_path);

    let listener = UnixListener::bind(&paths.socket_path).map_err(BindError::Io)?;

    Ok((lock_file, listener))
}

#[derive(Debug)]
pub enum BindError {
    /// Another instance already holds the flock — not a crash, the
    /// expected "socket busy" outcome the plan's concurrent-start test
    /// scenario names.
    AlreadyRunning,
    Io(std::io::Error),
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindError::AlreadyRunning => {
                write!(f, "another antibisingd instance is already running (socket busy)")
            }
            BindError::Io(e) => write!(f, "IPC bind failed: {e}"),
        }
    }
}

/// Requests a client can send, one JSON object per line.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Ask for a full snapshot re-send (also sent unprompted on connect).
    RequestSnapshot,
    /// Pin a specific device by its stable identity string. `None` clears
    /// the pin (R3).
    SetPin { device: Option<String> },
    /// Replace the ranked device preference list (R3), most-preferred
    /// first.
    SetPreferenceOrder { order: Vec<String> },
    /// Set RNNoise's VAD Threshold (%) — R8's primary control. Applied
    /// live immediately; persisted to config synchronously here (U10;
    /// R8's debounce for rapid slider drags is a panel-side concern —
    /// the daemon's own write is already atomic and cheap per message).
    SetThreshold { value: f64 },
    /// Toggle denoise on/off via RNNoise's own `Dry Mix` control (R6/U5):
    /// `true` = suppression on (`Dry Mix=0.0`), `false` = clean
    /// passthrough (`Dry Mix=1.0`).
    ToggleDenoise { enabled: bool },
}

/// Events pushed from daemon to client. `Snapshot` on connect and on
/// explicit `RequestSnapshot`; everything else is a delta.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Sent once, immediately after connect — names the protocol version
    /// so a future client generation can detect a mismatch.
    Hello { protocol_version: u32 },
    /// Full current state: every known device, the current health
    /// verdict, and the persisted config (pin/order/denoise settings) —
    /// enough for a client to render its whole UI with no further
    /// round-trips.
    Snapshot {
        devices: Vec<engine::DeviceInfo>,
        health: HealthStatus,
        config: Config,
    },
    DeviceArrived(engine::DeviceInfo),
    DeviceDeparted(engine::DeviceId),
    HealthChanged(HealthStatus),
    /// A request this connection sent was malformed or invalid — sent
    /// only to the connection that sent it; the connection survives (per
    /// the plan's own test scenario: malformed message -> error reply,
    /// connection survives, daemon never panics).
    Error { message: String },
}

fn write_event(stream: &mut UnixStream, event: &Event) -> std::io::Result<()> {
    let mut line = serde_json::to_string(event).expect("Event must always serialize");
    line.push('\n');
    stream.write_all(line.as_bytes())
}

/// Daemon-side state shared across all client-handling threads: the
/// persisted config (mutated by control requests, read for `Snapshot`)
/// and the last-known device set + health, kept current by the main
/// event-pump loop so a newly-connected client's `Snapshot` is accurate
/// without waiting on the engine to re-emit (each PipeWire generation
/// emits its own `Snapshot` exactly once, not on every new IPC client).
pub struct SharedState {
    pub config: Config,
    pub config_path: PathBuf,
    pub devices: std::collections::HashMap<engine::DeviceId, engine::DeviceInfo>,
    pub health: HealthStatus,
}

impl SharedState {
    pub fn new(config: Config, config_path: PathBuf) -> Self {
        Self {
            config,
            config_path,
            devices: std::collections::HashMap::new(),
            health: HealthStatus::SilentNoDevice,
        }
    }

    fn snapshot_event(&self) -> Event {
        Event::Snapshot {
            devices: self.devices.values().cloned().collect(),
            health: self.health.clone(),
            config: self.config.clone(),
        }
    }
}

/// One connected client's write side, shared between the broadcaster (for
/// pushed deltas) and that client's own read loop (for direct replies
/// like `Event::Error`, which must reach only the sender).
type ClientWriter = Arc<Mutex<UnixStream>>;

/// Fan-out to every currently-connected client. Each entry is a
/// [`ClientWriter`] — `broadcast()` locks and writes to each in turn; a
/// write failure (client gone) prunes that entry rather than erroring the
/// whole broadcast.
#[derive(Clone, Default)]
pub struct Broadcaster {
    clients: Arc<Mutex<Vec<ClientWriter>>>,
}

impl Broadcaster {
    pub fn new() -> Self {
        Self::default()
    }

    fn register(&self, writer: ClientWriter) {
        self.clients
            .lock()
            .expect("broadcaster mutex poisoned")
            .push(writer);
    }

    pub fn broadcast(&self, event: &Event) {
        let mut clients = self.clients.lock().expect("broadcaster mutex poisoned");
        clients.retain(|writer| {
            let mut stream = writer.lock().expect("client writer mutex poisoned");
            write_event(&mut stream, event).is_ok()
        });
    }
}

/// Translate a client `Request` into the engine command(s) it implies,
/// plus any config mutation. Pure with respect to socket I/O (no reads,
/// no writes here — only the `mpsc::Sender` into the engine and the
/// in-memory `Config`) so the mapping is independently unit-testable; the
/// caller persists the config and replies.
///
/// Returns `Err(message)` for a request this daemon can't currently
/// satisfy — none of today's request variants can actually fail
/// validation (every field is already the right type by construction of
/// successful JSON deserialization), so this is `Result` for forward
/// compatibility with a future request that can be rejected, not because
/// today's variants exercise the error path.
pub fn apply_request(
    request: Request,
    config: &mut Config,
    session_cmd_tx: &std::sync::mpsc::Sender<SessionCommand>,
) -> Result<(), String> {
    match request {
        Request::RequestSnapshot => {
            let _ = session_cmd_tx.send(SessionCommand::RequestSnapshot);
            Ok(())
        }
        Request::SetPin { device } => {
            config.pin = device.clone();
            let pin = device.map(engine::DeviceId);
            let _ = session_cmd_tx.send(SessionCommand::SetPin(pin));
            Ok(())
        }
        Request::SetPreferenceOrder { order } => {
            config.preference_order = order.clone();
            let ids: Vec<engine::DeviceId> = order.into_iter().map(engine::DeviceId).collect();
            let _ = session_cmd_tx.send(SessionCommand::SetPreferenceOrder(ids));
            Ok(())
        }
        Request::SetThreshold { value } => {
            let clamped = engine::RnnoiseParam::VadThreshold.clamp(value as f32);
            config.vad_threshold = clamped as f64;
            let _ = session_cmd_tx.send(SessionCommand::SetRnnoiseParam(
                engine::RnnoiseParam::VadThreshold,
                clamped,
            ));
            Ok(())
        }
        Request::ToggleDenoise { enabled } => {
            // R6: Dry Mix is the toggle. enabled=true means suppression
            // on, i.e. Dry Mix=0.0 (verified live in params.rs's doc
            // comment on the measured semantics).
            let dry_mix = if enabled { 0.0 } else { 1.0 };
            config.dry_mix = dry_mix;
            // The RNNoise node must be in the graph for Dry Mix to mean
            // anything — toggling denoise on implies it's present.
            config.denoise_enabled = true;
            let _ = session_cmd_tx.send(SessionCommand::SetRnnoiseParam(
                engine::RnnoiseParam::DryMix,
                dry_mix as f32,
            ));
            Ok(())
        }
    }
}

/// Handle one connected client for its whole lifetime: send `Hello` +
/// initial `Snapshot`, register for broadcasts, then loop reading
/// `Request` lines until the client disconnects (clean or killed — both
/// surface as a read error, the signal to stop and let the thread exit;
/// no monitor/meter session exists yet in U10 to release — that's U6's
/// job, binding into this same connection lifecycle later).
fn handle_client(
    mut stream: UnixStream,
    shared: Arc<Mutex<SharedState>>,
    broadcaster: Broadcaster,
    session_cmd_tx: std::sync::mpsc::Sender<SessionCommand>,
) {
    if write_event(
        &mut stream,
        &Event::Hello {
            protocol_version: PROTOCOL_VERSION,
        },
    )
    .is_err()
    {
        return;
    }
    {
        let state = shared.lock().expect("shared state mutex poisoned");
        if write_event(&mut stream, &state.snapshot_event()).is_err() {
            return;
        }
    }

    let write_handle: ClientWriter = match stream.try_clone() {
        Ok(s) => Arc::new(Mutex::new(s)),
        Err(_) => return,
    };
    broadcaster.register(write_handle.clone());

    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break, // client disconnected (clean or killed)
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Request>(&line) {
            Ok(request) => {
                let mut state = shared.lock().expect("shared state mutex poisoned");
                match apply_request(request, &mut state.config, &session_cmd_tx) {
                    Ok(()) => {
                        let content = toml::to_string_pretty(&state.config)
                            .expect("Config must always serialize to TOML");
                        let _ = engine::write_atomic(&state.config_path, &content);
                    }
                    Err(message) => {
                        let mut w = write_handle.lock().expect("client writer mutex poisoned");
                        let _ = write_event(&mut w, &Event::Error { message });
                    }
                }
            }
            Err(e) => {
                let mut w = write_handle.lock().expect("client writer mutex poisoned");
                let _ = write_event(
                    &mut w,
                    &Event::Error {
                        message: format!("malformed request: {e}"),
                    },
                );
                // Connection survives — loop continues to the next line.
            }
        }
    }

    // Reader loop exited (client gone). The broadcaster still holds
    // `write_handle` until its *next* broadcast attempt fails and prunes
    // it (Broadcaster::broadcast's retain) — a bounded, self-healing
    // cleanup rather than requiring this thread to reach back into the
    // broadcaster's client list itself.
}

/// Accept loop: one thread per connection. Spawned by the daemon's main
/// after a successful `bind_singleton`.
pub fn accept_loop(
    listener: UnixListener,
    shared: Arc<Mutex<SharedState>>,
    broadcaster: Broadcaster,
    session_cmd_tx: std::sync::mpsc::Sender<SessionCommand>,
) {
    for connection in listener.incoming() {
        let stream = match connection {
            Ok(s) => s,
            Err(_) => continue, // transient accept error; keep serving
        };
        let shared = shared.clone();
        let broadcaster = broadcaster.clone();
        let session_cmd_tx = session_cmd_tx.clone();
        std::thread::spawn(move || handle_client(stream, shared, broadcaster, session_cmd_tx));
    }
}

/// Translate a `SessionEvent` into the IPC delta it implies, updating
/// `shared` in place so future `Snapshot`s stay accurate, then broadcast
/// it. `Snapshot` events from the engine are absorbed into `shared`
/// (populating `devices`) but not broadcast verbatim — each IPC client
/// already received its own `Snapshot` on connect; re-broadcasting the
/// engine's periodic re-sync `Snapshot` to every client on every PipeWire
/// reconnect would be state the client already has, framed as a delta.
/// `Disconnected` maps to a `HealthChanged(Reconnecting)` broadcast so
/// clients see the transient without a raw internal-event leak.
pub fn handle_session_event(shared: &Arc<Mutex<SharedState>>, broadcaster: &Broadcaster, event: SessionEvent) {
    match event {
        SessionEvent::DeviceArrived(info) => {
            let mut state = shared.lock().expect("shared state mutex poisoned");
            state.devices.insert(info.id.clone(), info.clone());
            drop(state);
            broadcaster.broadcast(&Event::DeviceArrived(info));
        }
        SessionEvent::DeviceDeparted(id) => {
            let mut state = shared.lock().expect("shared state mutex poisoned");
            state.devices.remove(&id);
            drop(state);
            broadcaster.broadcast(&Event::DeviceDeparted(id));
        }
        SessionEvent::Snapshot(devices) => {
            let mut state = shared.lock().expect("shared state mutex poisoned");
            state.devices = devices.into_iter().map(|d| (d.id.clone(), d)).collect();
        }
        SessionEvent::HealthChanged(health) => {
            let mut state = shared.lock().expect("shared state mutex poisoned");
            state.health = health.clone();
            drop(state);
            broadcaster.broadcast(&Event::HealthChanged(health));
        }
        SessionEvent::Disconnected => {
            let mut state = shared.lock().expect("shared state mutex poisoned");
            state.health = HealthStatus::Reconnecting;
            drop(state);
            broadcaster.broadcast(&Event::HealthChanged(HealthStatus::Reconnecting));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config::default()
    }

    #[test]
    fn set_pin_updates_config_and_forwards_command() {
        let mut config = test_config();
        let (tx, rx) = std::sync::mpsc::channel();
        apply_request(
            Request::SetPin {
                device: Some("dev-a".to_string()),
            },
            &mut config,
            &tx,
        )
        .unwrap();
        assert_eq!(config.pin, Some("dev-a".to_string()));
        match rx.try_recv().unwrap() {
            SessionCommand::SetPin(Some(id)) => assert_eq!(id, engine::DeviceId("dev-a".to_string())),
            _ => panic!("expected SetPin(Some(..))"),
        }
    }

    #[test]
    fn set_pin_none_clears_pin() {
        let mut config = test_config();
        config.pin = Some("dev-a".to_string());
        let (tx, rx) = std::sync::mpsc::channel();
        apply_request(Request::SetPin { device: None }, &mut config, &tx).unwrap();
        assert_eq!(config.pin, None);
        match rx.try_recv().unwrap() {
            SessionCommand::SetPin(None) => {}
            _ => panic!("expected SetPin(None)"),
        }
    }

    #[test]
    fn set_preference_order_updates_config_and_forwards_command() {
        let mut config = test_config();
        let (tx, rx) = std::sync::mpsc::channel();
        apply_request(
            Request::SetPreferenceOrder {
                order: vec!["dev-a".to_string(), "dev-b".to_string()],
            },
            &mut config,
            &tx,
        )
        .unwrap();
        assert_eq!(config.preference_order, vec!["dev-a".to_string(), "dev-b".to_string()]);
        match rx.try_recv().unwrap() {
            SessionCommand::SetPreferenceOrder(ids) => {
                assert_eq!(ids, vec![engine::DeviceId("dev-a".to_string()), engine::DeviceId("dev-b".to_string())]);
            }
            _ => panic!("expected SetPreferenceOrder"),
        }
    }

    #[test]
    fn set_threshold_clamps_and_updates_config() {
        let mut config = test_config();
        let (tx, rx) = std::sync::mpsc::channel();
        apply_request(Request::SetThreshold { value: 150.0 }, &mut config, &tx).unwrap();
        // VadThreshold's valid range is 0.0-99.0 (params.rs) -- 150 must clamp.
        assert_eq!(config.vad_threshold, 99.0);
        match rx.try_recv().unwrap() {
            SessionCommand::SetRnnoiseParam(engine::RnnoiseParam::VadThreshold, value) => {
                assert_eq!(value, 99.0);
            }
            _ => panic!("expected SetRnnoiseParam(VadThreshold, ..)"),
        }
    }

    #[test]
    fn toggle_denoise_on_sets_dry_mix_zero_and_enables_graph() {
        let mut config = test_config();
        let (tx, rx) = std::sync::mpsc::channel();
        apply_request(Request::ToggleDenoise { enabled: true }, &mut config, &tx).unwrap();
        assert_eq!(config.dry_mix, 0.0);
        assert!(config.denoise_enabled);
        match rx.try_recv().unwrap() {
            SessionCommand::SetRnnoiseParam(engine::RnnoiseParam::DryMix, value) => {
                assert_eq!(value, 0.0);
            }
            _ => panic!("expected SetRnnoiseParam(DryMix, 0.0)"),
        }
    }

    #[test]
    fn toggle_denoise_off_sets_dry_mix_one() {
        let mut config = test_config();
        let (tx, rx) = std::sync::mpsc::channel();
        apply_request(Request::ToggleDenoise { enabled: false }, &mut config, &tx).unwrap();
        assert_eq!(config.dry_mix, 1.0);
        match rx.try_recv().unwrap() {
            SessionCommand::SetRnnoiseParam(engine::RnnoiseParam::DryMix, value) => {
                assert_eq!(value, 1.0);
            }
            _ => panic!("expected SetRnnoiseParam(DryMix, 1.0)"),
        }
    }

    #[test]
    fn request_snapshot_forwards_command_without_mutating_config() {
        let mut config = test_config();
        let before = config.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        apply_request(Request::RequestSnapshot, &mut config, &tx).unwrap();
        assert_eq!(config, before);
        assert!(matches!(rx.try_recv().unwrap(), SessionCommand::RequestSnapshot));
    }

    #[test]
    fn request_json_round_trips_through_tagged_enum() {
        let req = Request::SetThreshold { value: 72.5 };
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        match back {
            Request::SetThreshold { value } => assert_eq!(value, 72.5),
            _ => panic!("round-trip changed variant"),
        }
    }

    #[test]
    fn event_json_round_trips_including_health_status() {
        let event = Event::HealthChanged(HealthStatus::Linked {
            device: engine::DeviceId("dev-a".to_string()),
            description: "Razer Seiren Mini".to_string(),
        });
        let json = serde_json::to_string(&event).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        match back {
            Event::HealthChanged(HealthStatus::Linked { device, description }) => {
                assert_eq!(device, engine::DeviceId("dev-a".to_string()));
                assert_eq!(description, "Razer Seiren Mini");
            }
            _ => panic!("round-trip changed variant"),
        }
    }

    #[test]
    fn malformed_json_line_is_rejected_by_deserialization() {
        // Exercises the exact failure path handle_client's read loop hits
        // for a garbage line -- confirms it's a clean Err, not a panic.
        let result: Result<Request, _> = serde_json::from_str("not valid json {{{");
        assert!(result.is_err());
    }

    #[test]
    fn broadcaster_prunes_dead_client_on_failed_write() {
        // A registered writer whose peer is gone must be dropped from the
        // broadcaster's list on the next broadcast, not accumulate
        // forever.
        let broadcaster = Broadcaster::new();
        let (a, b) = UnixStream::pair().expect("create socketpair");
        let writer: ClientWriter = Arc::new(Mutex::new(a));
        broadcaster.register(writer);
        drop(b); // peer gone -> next write on `a` fails

        broadcaster.broadcast(&Event::Hello { protocol_version: PROTOCOL_VERSION });
        assert_eq!(
            broadcaster.clients.lock().unwrap().len(),
            0,
            "dead client must be pruned after a failed broadcast write"
        );
    }

    #[test]
    fn shared_state_snapshot_reflects_current_devices_and_health() {
        let mut state = SharedState::new(test_config(), PathBuf::from("/tmp/unused"));
        let info = engine::DeviceInfo {
            id: engine::DeviceId("dev-a".to_string()),
            node_id: 1,
            description: "Razer".to_string(),
        };
        state.devices.insert(info.id.clone(), info.clone());
        state.health = HealthStatus::Linked {
            device: info.id.clone(),
            description: info.description.clone(),
        };

        match state.snapshot_event() {
            Event::Snapshot { devices, health, .. } => {
                assert_eq!(devices, vec![info.clone()]);
                assert_eq!(
                    health,
                    HealthStatus::Linked {
                        device: info.id,
                        description: info.description,
                    }
                );
            }
            _ => panic!("expected Snapshot"),
        }
    }
}
