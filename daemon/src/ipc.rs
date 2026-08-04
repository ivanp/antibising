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
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    /// Start hear-yourself monitoring (U6/R9) for this connection: links
    /// the permanent source's output to the current default sink's
    /// input. Bound to this connection's own session id, assigned once
    /// at connect — `StopMonitor`, or the connection simply dropping,
    /// releases it. Idempotent to call while already active (the engine
    /// re-resolves the default sink and replaces the session's links).
    StartMonitor,
    /// Stop monitoring for this connection. Idempotent: a no-op if no
    /// monitor session is active.
    StopMonitor,
    /// Subscribe this connection to the live level meter (U6/R9) — an
    /// engine-side capture stream on the permanent source starts
    /// producing `Event::MeterFrame`s. Idempotent while already
    /// subscribed.
    StartMeter,
    /// Unsubscribe from the level meter. Idempotent: a no-op if no meter
    /// session is active.
    StopMeter,
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
    /// `DeviceId` is a newtype around a bare `String`, which cannot
    /// serialize as an internally-tagged (`#[serde(tag = "type")]`)
    /// enum variant when carried as a tuple payload -- serde requires
    /// every variant's payload to serialize as a map so the tag can be
    /// merged in. A struct variant sidesteps that entirely.
    DeviceDeparted { id: engine::DeviceId },
    HealthChanged(HealthStatus),
    /// This connection's `StartMonitor` succeeded — links exist. Carries
    /// the R9 speaker-vs-headset assessment as an inline warning (never
    /// a refusal). Reaches only the requesting connection (per-session,
    /// like `Error`) via `SessionRegistry`, never broadcast.
    MonitorStarted { speaker_risk: engine::SpeakerRisk },
    /// This connection's `StartMonitor` could not proceed (e.g. no
    /// default sink resolved yet). Per-session, not broadcast.
    MonitorFailed { reason: String },
    /// This connection's monitor session ended — explicit `StopMonitor`,
    /// or implied by disconnect. Per-session, not broadcast.
    MonitorStopped,
    /// One live level-meter measurement (U6/R9), pushed only to
    /// connections currently subscribed via `StartMeter` — never
    /// broadcast to clients that never asked for it.
    MeterFrame(engine::MeterFrame),
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

/// Assigns each accepted connection a stable `u64` session id — the same
/// identity `StartMonitor`/`StartMeter` bind their engine-side session to
/// (`SessionCommand::StartMonitor(session_id)` etc.), and the key
/// `SessionRegistry` and the engine's own `MonitorStarted`/`MonitorFailed`/
/// `MonitorStopped` events route back on. Distinct from any engine-side
/// device identity; monotonic for the daemon process's lifetime, so ids
/// are never reused even across reconnects.
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

fn next_session_id() -> u64 {
    NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
}

/// How often a subscribed connection's meter-frame drain thread wakes to
/// push queued frames to its client (R9's own ~50ms per-frame cadence)
/// — fast enough to feel live, without a busy-spin.
const METER_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Capacity of a per-client meter-frame channel: small and bounded, per
/// the plan's own rule ("meter frames never block the pw thread ... a
/// slow or busy client loses frames, never stalls audio-side event
/// processing"). A few poll intervals' worth of headroom for jitter,
/// never unbounded growth.
const METER_CHANNEL_CAPACITY: usize = 8;

/// Routes engine events and meter frames that are scoped to **one**
/// connection — never broadcast — back to that connection's own writer.
/// `Broadcaster` fans device/health deltas out to everyone; this is the
/// opposite shape: `StartMonitor`/`StartMeter`'s `MonitorStarted` /
/// `MonitorFailed` / `MonitorStopped` / `MeterFrame` replies must reach
/// only the requesting client, keyed by the `session_id` assigned at
/// accept time.
#[derive(Clone, Default)]
pub struct SessionRegistry {
    sessions: Arc<Mutex<HashMap<u64, ClientWriter>>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn register(&self, session_id: u64, writer: ClientWriter) {
        self.sessions
            .lock()
            .expect("session registry mutex poisoned")
            .insert(session_id, writer);
    }

    fn unregister(&self, session_id: u64) {
        self.sessions
            .lock()
            .expect("session registry mutex poisoned")
            .remove(&session_id);
    }

    /// Send `event` to exactly the connection identified by
    /// `session_id`. A `session_id` with no registered connection (the
    /// client already disconnected, racing the engine's own eventual
    /// reaction) is a silent no-op — not an error, since disconnect
    /// cleanup already tore that session down from the engine's side
    /// too.
    pub fn send_to(&self, session_id: u64, event: &Event) {
        let sessions = self.sessions.lock().expect("session registry mutex poisoned");
        if let Some(writer) = sessions.get(&session_id) {
            let mut stream = writer.lock().expect("client writer mutex poisoned");
            let _ = write_event(&mut stream, event);
        }
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
/// Returns `Ok(true)` when `config` was mutated (so the caller should
/// broadcast a fresh `Snapshot` -- `SetPin`/`SetPreferenceOrder`/
/// `SetThreshold`/`ToggleDenoise`), `Ok(false)` for requests that never
/// touch `config` (`RequestSnapshot`, `StartMonitor`/`StopMonitor`,
/// `StartMeter`/`StopMeter` -- these already have their own
/// confirmation path: `MonitorStarted`/`MonitorFailed`/`MonitorStopped`
/// reach only the requester via `SessionRegistry`, `MeterFrame`s stream
/// to only the subscriber. Broadcasting a Snapshot on every one of
/// those would both be redundant (nothing in it changed) and would
/// leak that per-session reply's timing to every *other* connected
/// client, which the existing `start_monitor_forwards_command_and_
/// routes_reply_to_requester_only` test explicitly guards against.
pub fn apply_request(
    request: Request,
    config: &mut Config,
    session_cmd_tx: &std::sync::mpsc::Sender<SessionCommand>,
    session_id: u64,
    meter_slot: &Arc<Mutex<Option<Arc<Mutex<engine::MeterChannel>>>>>,
) -> Result<bool, String> {
    match request {
        Request::RequestSnapshot => {
            let _ = session_cmd_tx.send(SessionCommand::RequestSnapshot);
            Ok(false)
        }
        Request::SetPin { device } => {
            config.pin = device.clone();
            let pin = device.map(engine::DeviceId);
            let _ = session_cmd_tx.send(SessionCommand::SetPin(pin));
            Ok(true)
        }
        Request::SetPreferenceOrder { order } => {
            config.preference_order = order.clone();
            let ids: Vec<engine::DeviceId> = order.into_iter().map(engine::DeviceId).collect();
            let _ = session_cmd_tx.send(SessionCommand::SetPreferenceOrder(ids));
            Ok(true)
        }
        Request::SetThreshold { value } => {
            let clamped = engine::RnnoiseParam::VadThreshold.clamp(value as f32);
            config.vad_threshold = clamped as f64;
            let _ = session_cmd_tx.send(SessionCommand::SetRnnoiseParam(
                engine::RnnoiseParam::VadThreshold,
                clamped,
            ));
            Ok(true)
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
            Ok(true)
        }
        Request::StartMonitor => {
            let _ = session_cmd_tx.send(SessionCommand::StartMonitor(session_id));
            Ok(false)
        }
        Request::StopMonitor => {
            let _ = session_cmd_tx.send(SessionCommand::StopMonitor(session_id));
            Ok(false)
        }
        Request::StartMeter => {
            // A fresh channel every StartMeter (including a re-subscribe
            // after StopMeter) -- the engine's own StartMeter session
            // owns exactly the channel it was handed, so replacing the
            // slot here and sending the new Arc keeps both sides
            // pointing at the same live queue.
            let channel = Arc::new(Mutex::new(engine::MeterChannel::new(METER_CHANNEL_CAPACITY)));
            *meter_slot.lock().expect("meter slot mutex poisoned") = Some(channel.clone());
            let _ = session_cmd_tx.send(SessionCommand::StartMeter(session_id, channel));
            Ok(false)
        }
        Request::StopMeter => {
            *meter_slot.lock().expect("meter slot mutex poisoned") = None;
            let _ = session_cmd_tx.send(SessionCommand::StopMeter(session_id));
            Ok(false)
        }
    }
}

/// Handle one connected client for its whole lifetime: assign it a
/// session id, send `Hello` + initial `Snapshot`, register for
/// broadcasts and per-session routing, then loop reading `Request` lines
/// until the client disconnects (clean or killed — both surface as a
/// read error). On exit, always releases any monitor/meter session this
/// connection held (U6/R9: "never survives app exit," enforced at the
/// daemon boundary, not left to the client to ask nicely) and stops the
/// meter-frame drain thread.
fn handle_client(
    mut stream: UnixStream,
    shared: Arc<Mutex<SharedState>>,
    broadcaster: Broadcaster,
    session_registry: SessionRegistry,
    session_cmd_tx: std::sync::mpsc::Sender<SessionCommand>,
) {
    let session_id = next_session_id();

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
    session_registry.register(session_id, write_handle.clone());

    // Meter drain thread: while `meter_slot` holds a channel (set by
    // `StartMeter`, cleared by `StopMeter`), wake on `METER_POLL_INTERVAL`
    // and push every queued frame to this client — a per-connection
    // thread rather than a shared poller, so a slow client's writes never
    // hold up another client's meter delivery. Exits with the connection
    // (the `Arc<AtomicBool>` stop flag is set right before this function
    // returns, in the cleanup below).
    let meter_slot: Arc<Mutex<Option<Arc<Mutex<engine::MeterChannel>>>>> =
        Arc::new(Mutex::new(None));
    let meter_stop = Arc::new(AtomicBool::new(false));
    let drain_handle = {
        let meter_slot = meter_slot.clone();
        let meter_stop = meter_stop.clone();
        let write_handle = write_handle.clone();
        std::thread::spawn(move || {
            while !meter_stop.load(Ordering::Relaxed) {
                std::thread::sleep(METER_POLL_INTERVAL);
                let channel = meter_slot.lock().expect("meter slot mutex poisoned").clone();
                let Some(channel) = channel else { continue };
                let frames = channel.lock().expect("meter channel mutex poisoned").drain();
                for frame in frames {
                    let mut w = write_handle.lock().expect("client writer mutex poisoned");
                    if write_event(&mut w, &Event::MeterFrame(frame)).is_err() {
                        return; // client gone; the read loop will notice too
                    }
                }
            }
        })
    };

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
                match apply_request(request, &mut state.config, &session_cmd_tx, session_id, &meter_slot) {
                    Ok(config_changed) => {
                        if config_changed {
                            let content = toml::to_string_pretty(&state.config)
                                .expect("Config must always serialize to TOML");
                            let _ = engine::write_atomic(&state.config_path, &content);
                            // R3/U7's own contract: "dropdown reflects pinned
                            // state on the engine's confirmation event, not
                            // optimistically" -- and the panel's denoise
                            // button/threshold slider read back only from
                            // Snapshot too (index.html's applySnapshot).
                            // Without this broadcast, a successful mutation
                            // was applied to the daemon's config and to
                            // PipeWire (verified live: the persisted config
                            // and the running rnnoise:Dry Mix param both
                            // changed) but every connected client --
                            // including the very one that sent the request
                            // -- never learned that, so a control bound to
                            // "toggle from last-known state" could resend
                            // the same value forever. Broadcasting the full
                            // Snapshot (not a narrower delta) matches this
                            // file's own doc comment on Snapshot being
                            // "enough for a client to render its whole UI
                            // with no further round-trips" and needs no new
                            // Event variant. Gated on `config_changed` (only
                            // `apply_request`'s config-mutating branches
                            // return `true`) -- StartMonitor/StopMonitor/
                            // StartMeter/StopMeter never touch `config` and
                            // already have their own per-session reply path
                            // (MonitorStarted/MonitorFailed/MonitorStopped/
                            // MeterFrame via SessionRegistry), which an
                            // unconditional broadcast here would both
                            // duplicate and leak to every other client.
                            let snapshot = state.snapshot_event();
                            drop(state);
                            broadcaster.broadcast(&snapshot);
                        }
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

    // Reader loop exited (client gone, clean or killed). Release
    // exactly this connection's engine-side sessions -- R9's "never
    // survives app exit" enforced here at the daemon boundary rather
    // than depending on the client sending StopMonitor/StopMeter first.
    // Idempotent on the engine side even if this connection never
    // started either.
    let _ = session_cmd_tx.send(SessionCommand::StopMonitor(session_id));
    let _ = session_cmd_tx.send(SessionCommand::StopMeter(session_id));
    session_registry.unregister(session_id);
    meter_stop.store(true, Ordering::Relaxed);
    let _ = drain_handle.join();

    // The broadcaster still holds `write_handle` until its *next*
    // broadcast attempt fails and prunes it (Broadcaster::broadcast's
    // retain) — a bounded, self-healing cleanup rather than requiring
    // this thread to reach back into the broadcaster's client list
    // itself.
}

/// Accept loop: one thread per connection. Spawned by the daemon's main
/// after a successful `bind_singleton`.
pub fn accept_loop(
    listener: UnixListener,
    shared: Arc<Mutex<SharedState>>,
    broadcaster: Broadcaster,
    session_registry: SessionRegistry,
    session_cmd_tx: std::sync::mpsc::Sender<SessionCommand>,
) {
    for connection in listener.incoming() {
        let stream = match connection {
            Ok(s) => s,
            Err(_) => continue, // transient accept error; keep serving
        };
        let shared = shared.clone();
        let broadcaster = broadcaster.clone();
        let session_registry = session_registry.clone();
        let session_cmd_tx = session_cmd_tx.clone();
        std::thread::spawn(move || {
            handle_client(stream, shared, broadcaster, session_registry, session_cmd_tx)
        });
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
pub fn handle_session_event(
    shared: &Arc<Mutex<SharedState>>,
    broadcaster: &Broadcaster,
    session_registry: &SessionRegistry,
    event: SessionEvent,
) {
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
            broadcaster.broadcast(&Event::DeviceDeparted { id });
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
        SessionEvent::MonitorStarted { session_id, speaker_risk } => {
            session_registry.send_to(session_id, &Event::MonitorStarted { speaker_risk });
        }
        SessionEvent::MonitorFailed { session_id, reason } => {
            session_registry.send_to(session_id, &Event::MonitorFailed { reason });
        }
        SessionEvent::MonitorStopped { session_id } => {
            session_registry.send_to(session_id, &Event::MonitorStopped);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config::default()
    }

    /// Test-only wrapper: most `apply_request` tests exercise requests
    /// that don't touch `session_id`/`meter_slot` at all, so a fixed
    /// dummy session id and a throwaway slot keep those call sites
    /// exactly as terse as before the U6 signature extension.
    fn test_apply(
        request: Request,
        config: &mut Config,
        tx: &std::sync::mpsc::Sender<SessionCommand>,
    ) -> Result<bool, String> {
        let meter_slot = Arc::new(Mutex::new(None));
        apply_request(request, config, tx, 0, &meter_slot)
    }

    #[test]
    fn set_pin_updates_config_and_forwards_command() {
        let mut config = test_config();
        let (tx, rx) = std::sync::mpsc::channel();
        test_apply(
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
        test_apply(Request::SetPin { device: None }, &mut config, &tx).unwrap();
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
        test_apply(
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
        test_apply(Request::SetThreshold { value: 150.0 }, &mut config, &tx).unwrap();
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
        test_apply(Request::ToggleDenoise { enabled: true }, &mut config, &tx).unwrap();
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
        test_apply(Request::ToggleDenoise { enabled: false }, &mut config, &tx).unwrap();
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
        test_apply(Request::RequestSnapshot, &mut config, &tx).unwrap();
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

    /// The exact bug this test exists to catch: `DeviceDeparted` used to
    /// be a tuple variant wrapping `DeviceId` (a newtype around a bare
    /// `String`), which cannot serialize under `#[serde(tag = "type")]`
    /// internal tagging -- serde requires every variant's payload to
    /// serialize as a JSON object so the tag can be merged in, and a
    /// bare string payload has nowhere to put it. `write_event`'s
    /// `.expect(...)` meant a live daemon would panic the instant any
    /// device departed (unplugging a mic) -- exercising every variant's
    /// serialization here is cheap insurance against the same class of
    /// mistake landing on a future variant.
    #[test]
    fn every_event_variant_serializes_without_panicking() {
        let device_info = engine::DeviceInfo {
            id: engine::DeviceId("dev-a".to_string()),
            node_id: 1,
            description: "Razer".to_string(),
        };
        let variants = [
            Event::Hello { protocol_version: PROTOCOL_VERSION },
            Event::Snapshot {
                devices: vec![device_info.clone()],
                health: HealthStatus::SilentNoDevice,
                config: test_config(),
            },
            Event::DeviceArrived(device_info.clone()),
            Event::DeviceDeparted { id: device_info.id.clone() },
            Event::HealthChanged(HealthStatus::SilentNoDevice),
            Event::MonitorStarted { speaker_risk: engine::SpeakerRisk::Unknown },
            Event::MonitorFailed { reason: "no default sink".to_string() },
            Event::MonitorStopped,
            Event::MeterFrame(engine::MeterFrame { rms: 0.1, peak: 0.2 }),
            Event::Error { message: "malformed request".to_string() },
        ];
        for event in variants {
            let json = serde_json::to_string(&event)
                .unwrap_or_else(|e| panic!("{event:?} failed to serialize: {e}"));
            let _: Event = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("{event:?} round-trip failed to deserialize: {e}"));
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
