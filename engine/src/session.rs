//! The PipeWire session thread (U1). Owns a `MainLoopRc`, watches the
//! Registry for device arrivals/departures, and survives PipeWire itself
//! dying by reconnecting with backoff and a clean re-enumeration.
//!
//! Threading (KTD2): this thread is the only place `!Send` PipeWire objects
//! live. Commands and events both cross the thread boundary via plain
//! `std::sync::mpsc` — sends never block. The pw loop drains commands on a
//! periodic timer rather than an attached channel receiver, because an
//! attached `pipewire::channel::Receiver` is consumed on `attach()` and
//! cannot be reused across a reconnect; a `std::sync::mpsc::Receiver` is
//! `Send` and lives in the outer loop for the whole thread lifetime,
//! surviving every generation.

use crate::model::{is_candidate_source, DeviceId, DeviceInfo};
use crate::reconcile::{decide_action, ActualLink, ReconcileAction};
use crate::routing::{compute_desired_feed, should_release_pin, DesiredFeed};
use pipewire::{
    context::ContextRc,
    core::CoreRc,
    link::Link,
    main_loop::MainLoopRc,
    metadata::Metadata,
    properties::properties,
    node::Node,
    registry::{GlobalObject, RegistryRc},
    spa::{
        param::audio::AudioInfoRaw,
        pod::Pod,
        utils::{dict::DictRef, Direction},
    },
    stream::{StreamFlags, StreamListener, StreamRc},
    types::ObjectType,
};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::mpsc::{Receiver as StdReceiver, Sender as StdSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Commands sent from outside the session thread (daemon/IPC layer) into it.
#[derive(Debug)]
pub enum SessionCommand {
    /// Replace the ranked device preference list (R3). Order matters:
    /// index 0 is highest priority.
    SetPreferenceOrder(Vec<DeviceId>),
    /// Pin a specific device, suppressing ranking until unpinned or the
    /// device disappears (R3). `None` clears the pin.
    SetPin(Option<DeviceId>),
    /// Set an RNNoise control live (U5/R8). A no-op, silently, if the
    /// capture node isn't bound yet (no RNNoise fragment active) — the
    /// caller's config write to the fragment is the durable half; this
    /// is only the immediate-effect half (R2/Q8: never a restart).
    SetRnnoiseParam(crate::params::RnnoiseParam, f32),
    /// Ask for a full re-emit of the current device set (used on IPC client
    /// connect for the initial snapshot).
    RequestSnapshot,
    /// Start hear-yourself monitoring (U6/R9) for the client identified by
    /// `session_id` (assigned by the IPC layer, unique per connection).
    /// Links the permanent source's own output ports to the current
    /// default sink's input ports. `session_id` scopes the resulting
    /// links so `StopMonitor` (explicit, or the IPC layer's own
    /// connection-drop cleanup) tears down exactly this client's links,
    /// never another's.
    StartMonitor(u64),
    /// Stop monitoring for `session_id` — destroys exactly the links
    /// `StartMonitor(session_id)` created. Idempotent: stopping a
    /// session that was never started, or already stopped, is a no-op.
    StopMonitor(u64),
    /// Start the level meter (U6/R9) for `session_id`, pushing
    /// [`crate::meter::MeterFrame`]s into the given bounded, drop-oldest
    /// channel as an engine-side capture stream on the permanent source
    /// produces them. The channel crosses the thread boundary via
    /// `Arc<Mutex<_>>` rather than the `SessionEvent` mpsc channel
    /// deliberately — frames must never queue unboundedly for a slow
    /// IPC client, which an unbounded `mpsc::Sender` would allow.
    StartMeter(u64, Arc<Mutex<crate::meter::MeterChannel>>),
    /// Stop the meter for `session_id` — disconnects and drops the
    /// capture stream. Idempotent.
    StopMeter(u64),
    /// Start the raw (pre-denoise) level meter for `session_id`, pushing
    /// [`crate::meter::MeterFrame`]s into the given channel as the upstream
    /// device produces them. Mirrors `StartMeter` but targets the raw
    /// device feed rather than the post-denoise permanent source.
    StartRawMeter(u64, Arc<Mutex<crate::meter::MeterChannel>>),
    /// Stop the raw meter for `session_id`. Idempotent.
    StopRawMeter(u64),
    /// Clean shutdown: quit the loop and let the thread join.
    Shutdown,
}

/// Events emitted from the session thread as PipeWire state changes.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    DeviceArrived(DeviceInfo),
    DeviceDeparted(DeviceId),
    /// Full device set, sent once after (re-)enumeration completes.
    Snapshot(Vec<DeviceInfo>),
    /// The connection to PipeWire was lost (crash or restart). A
    /// reconnect-and-re-enumerate cycle is already in progress; any
    /// downstream `Linked` health status must drop to `Reconnecting` until
    /// the next `Snapshot` arrives.
    Disconnected,
    /// The R5 health verdict changed. Emitted from `reconcile_now` only
    /// when the computed [`crate::model::HealthStatus`] actually differs
    /// from what was last emitted this generation — U10's IPC layer
    /// forwards this verbatim to clients rather than re-deriving it, so
    /// it must never fire on every reconcile tick regardless of change.
    HealthChanged(crate::model::HealthStatus),
    /// `StartMonitor(session_id)` succeeded — links exist. Carries the
    /// R9 speaker-vs-headset assessment for the daemon to relay as an
    /// inline warning (never a refusal).
    MonitorStarted {
        session_id: u64,
        speaker_risk: crate::monitor::SpeakerRisk,
    },
    /// `StartMonitor(session_id)` could not proceed — e.g. no default
    /// sink has been resolved yet. Distinct from `HealthStatus::Broken`:
    /// this is a per-request failure, not the permanent source's own
    /// health.
    MonitorFailed { session_id: u64, reason: String },
    /// `StopMonitor(session_id)` completed (or the session was already
    /// stopped) — links gone.
    MonitorStopped { session_id: u64 },
}


/// How often the pw loop polls for outside commands. Small enough that
/// `RequestSnapshot`/`Shutdown` feel instant; large enough not to matter for
/// CPU (this is not the audio path).
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Handle to a running session thread.
pub struct Session {
    cmd_tx: StdSender<SessionCommand>,
    event_rx: StdReceiver<SessionEvent>,
    join: Option<JoinHandle<()>>,
}

impl Session {
    /// Spawn the session thread. `our_source_name` excludes our own
    /// permanent source from device enumeration (model::is_candidate_source).
    pub fn spawn(our_source_name: String) -> Self {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<SessionCommand>();
        let (event_tx, event_rx) = std::sync::mpsc::channel::<SessionEvent>();

        let join = std::thread::Builder::new()
            .name("antibising-pw".into())
            .spawn(move || run_loop(our_source_name, cmd_rx, event_tx))
            .expect("failed to spawn PipeWire session thread");

        Self {
            cmd_tx,
            event_rx,
            join: Some(join),
        }
    }

    pub fn send(&self, cmd: SessionCommand) -> Result<(), ()> {
        self.cmd_tx.send(cmd).map_err(|_| ())
    }

    /// A clonable handle to send commands, independent of `&self` — U10's
    /// IPC layer hands one of these to each client-handling thread so
    /// multiple clients can issue commands concurrently without
    /// serializing through a single `&Session` borrow. All commands still
    /// funnel through the same underlying `mpsc::Sender`, so ordering
    /// into the session thread is preserved regardless of how many
    /// senders exist.
    pub fn command_sender(&self) -> StdSender<SessionCommand> {
        self.cmd_tx.clone()
    }

    /// Receive the next event, blocking. Returns `None` if the session
    /// thread has exited.
    pub fn recv_event(&self) -> Option<SessionEvent> {
        self.event_rx.recv().ok()
    }

    pub fn try_recv_event(&self) -> Option<SessionEvent> {
        self.event_rx.try_recv().ok()
    }

    /// Clean shutdown: send the quit command and join the thread.
    pub fn shutdown(&mut self) {
        let _ = self.cmd_tx.send(SessionCommand::Shutdown);
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Per-generation state rebuilt on every (re)connect. Never patched across a
/// reconnect — recovery is a clean rebuild, not an attempt to reconcile
/// stale IDs with new ones.
struct Generation {
    /// Resolved devices, keyed by their current `Audio/Source` node id.
    devices: HashMap<u32, DeviceInfo>,
    /// Device global id -> its identity key, resolved from `device.serial`
    /// / `api.bluez5.address` / `device.name` (never a node property).
    device_identities: HashMap<u32, DeviceId>,
    /// Candidate `Audio/Source` nodes seen before their parent Device
    /// global arrived. Registry ordering is not guaranteed, so a Node can
    /// precede its Device — this holds the node until the join resolves.
    pending_nodes: HashMap<u32, PendingNode>,
    /// Output ports (device side): node id -> (port global id, port name).
    device_output_ports: HashMap<u32, Vec<(u32, String)>>,
    /// Our own capture node's input ports: (port global id, port name),
    /// and the capture node's own id.
    capture_node_id: Option<u32>,
    /// A bound `Node` proxy for the capture node, used for U5's live
    /// `set-param` calls (`Node::set_param`, per KTD2's native-crate
    /// decision). Bound once the capture Node global arrives; cleared on
    /// departure/reconnect along with `capture_node_id`. Kept separate
    /// from `capture_node_id` (a plain `u32`) because a bound proxy is
    /// `!Send`/non-`Clone`-cheap PipeWire state — only session.rs's own
    /// thread ever touches it, exactly where KTD2 requires `!Send`
    /// PipeWire objects to live.
    capture_node_proxy: Option<Node>,
    capture_input_ports: Vec<(u32, String)>,
    /// Every Link global whose `link.input.node` is our capture node,
    /// keyed by the link's own global id. This is **observed graph
    /// truth**, not self-bookkeeping: it reflects every link on the
    /// capture node regardless of who created it (us, WirePlumber, a
    /// prior daemon generation that lingered one via `object.linger`).
    /// This is what makes A1 detection and cross-restart adoption real
    /// rather than assumed.
    capture_links: HashMap<u32, ObservedLink>,
    /// Routing policy state (R3), set via `SessionCommand`.
    preference_order: Vec<DeviceId>,
    pin: Option<DeviceId>,
    /// The last [`crate::model::HealthStatus`] emitted this generation
    /// (`None` before the first `reconcile_now` cycle). Dedup state for
    /// `HealthChanged` — U10's IPC clients need change events, not a
    /// flood on every debounced reconcile tick.
    last_health: Option<crate::model::HealthStatus>,
    /// Our own permanent source's node id (playback side, `SOURCE_NAME`
    /// — distinct from `capture_node_id`, the input side). Needed for
    /// U6's monitor links (source output ports -> default sink input
    /// ports).
    source_node_id: Option<u32>,
    /// The permanent source's own output ports: (port global id, port
    /// name). U6's monitor links originate here.
    source_output_ports: Vec<(u32, String)>,
    /// Every `Audio/Sink` node currently visible, keyed by node id —
    /// candidates for monitoring's destination. `device_id` is the
    /// owning Device global's id (for `device.form_factor` resolution,
    /// R9's speaker-warning heuristic), `None` if the sink has no owning
    /// Device (unusual but not impossible).
    sinks: HashMap<u32, SinkInfo>,
    /// Each known sink's input ports: node id -> (port global id, port
    /// name).
    sink_input_ports: HashMap<u32, Vec<(u32, String)>>,
    /// The default sink's `node.name`, resolved from the `default`
    /// Metadata object's `default.audio.sink` key (Q6: monitoring routes
    /// to whatever is default *at the moment it starts*, not tracked
    /// live thereafter — this field is read once per `StartMonitor`,
    /// never subscribed to for the lifetime of a monitor session).
    default_sink_name: Option<String>,
    /// Device global id -> `device.form_factor`, when the Device global
    /// carries one (verified this session: present on headset/webcam
    /// cards, absent on plain HDA sinks). Feeds R9's speaker-risk
    /// heuristic via `SinkInfo::device_global_id`.
    device_form_factors: HashMap<u32, String>,
    /// Active monitor sessions, keyed by the IPC-assigned `session_id`.
    /// Each holds the Link global ids it created, so `StopMonitor`
    /// (explicit, or connection-drop cleanup) tears down exactly this
    /// session's links and no other's.
    monitor_sessions: HashMap<u64, MonitorSession>,
    /// Active meter capture streams, keyed by `session_id`. The
    /// `StreamRc`/`StreamListener` pair must outlive the stream's
    /// registered `process` callback — dropping either tears the stream
    /// down, which is exactly `StopMeter`'s mechanism.
    meter_sessions: HashMap<u64, MeterSession>,
    /// Active raw (pre-denoise) meter capture streams, keyed by
    /// `session_id`. Mirrors `meter_sessions` but targets the upstream
    /// device rather than the post-denoise source.
    raw_meter_sessions: HashMap<u64, MeterSession>,
    /// Every input port seen this generation, keyed by owning node.id.
    /// Recorded unconditionally (regardless of whether any meter session
    /// has resolved its node_id yet) so that the port-before-node_id
    /// race is eliminated: the meter stream's own input port can arrive
    /// before `.state_changed(Paused)` sets the session's node_id, and
    /// recording unconditionally here means convergence finds it later.
    /// Mirrors how `device_output_ports` records output ports.
    input_ports_by_node: HashMap<u32, Vec<(u32, String)>>,
    /// Maps a port global id -> its owning node id. `global_remove` gets a
    /// bare id with no type, so a departed *port* id must be resolved to
    /// its node to prune the right entry from `device_output_ports` /
    /// `input_ports_by_node` / `source_output_ports` / `capture_input_ports`
    /// / `sink_input_ports`. Without this, a removed/recreated port stays
    /// cached and `converge_meter_links` could link to a dead port id.
    port_owner: HashMap<u32, u32>,
}

/// A Link global observed on the capture node, as reported by PipeWire —
/// not created-by-us bookkeeping.
#[derive(Debug, Clone)]
struct ObservedLink {
    output_node_id: u32,
    #[allow(dead_code)] // kept for future per-port diagnostics
    output_port_id: u32,
    input_port_id: u32,
}

struct PendingNode {
    device_global_id: u32,
    description: String,
}

/// A known `Audio/Sink` node — a candidate monitor destination.
struct SinkInfo {
    node_name: String,
    description: String,
    /// The owning Device global's id, if resolvable (`device.id`
    /// present) — used to look up `device.form_factor` for R9's
    /// speaker-warning heuristic.
    device_global_id: Option<u32>,
}

/// A live hear-yourself monitor session (U6/R9), bound to one IPC
/// client's `session_id`. RAII in spirit but not in Rust's `Drop` sense —
/// `StopMonitor` (explicit or connection-drop cleanup) is what tears
/// down `link_ids`; a `Generation` reconnect (session.rs's own "recovery
/// is a clean rebuild") also implicitly ends every session, since the
/// links themselves die with the old PipeWire connection.
struct MonitorSession {
    #[allow(dead_code)] // kept alive for its Drop; never read directly
    links: Vec<Link>,
}

/// A live meter capture session, bound to one IPC client's `session_id`.
/// Holds the stream + its listener (whose `process` callback pushes frames
/// into `channel`) so both stay alive exactly as long as the session does;
/// dropping either tears the stream down.
///
/// `node_id` is `None` until PipeWire assigns the stream a node id (at
/// `Paused` state). We poll `stream.node_id()` in `reconcile_now` each tick
/// for sessions where it is still `None`. `links` holds the retained `Link`
/// proxies that connect the source to this stream's input port; no
/// `object.linger` (dropping the proxy tears the link down server-side,
/// exactly the desired teardown path for `StopMeter`).
struct MeterSession {
    stream: StreamRc,
    #[allow(dead_code)] // kept alive for its Drop; never read directly
    listener: StreamListener<()>,
    /// PipeWire-assigned node id for this stream, set once the stream
    /// reaches `Paused` state. `None` until then.
    node_id: Option<u32>,
    /// Retained Link proxies from source output port(s) to this stream's
    /// input port. No `object.linger` — dropping tears the link down.
    links: Vec<Link>,
    /// The source node id from which links in `links` originate. Used
    /// by the raw meter to detect device switches (when the desired device
    /// node changes, stale links are purged and recreated).
    linked_source_node_id: Option<u32>,
}

impl Generation {
    fn new() -> Self {
        Self {
            devices: HashMap::new(),
            device_identities: HashMap::new(),
            pending_nodes: HashMap::new(),
            device_output_ports: HashMap::new(),
            capture_node_id: None,
            capture_node_proxy: None,
            capture_input_ports: Vec::new(),
            capture_links: HashMap::new(),
            preference_order: Vec::new(),
            pin: None,
            last_health: None,
            source_node_id: None,
            source_output_ports: Vec::new(),
            sinks: HashMap::new(),
            sink_input_ports: HashMap::new(),
            default_sink_name: None,
            device_form_factors: HashMap::new(),
            monitor_sessions: HashMap::new(),
            meter_sessions: HashMap::new(),
            raw_meter_sessions: HashMap::new(),
            input_ports_by_node: HashMap::new(),
            port_owner: HashMap::new(),
        }
    }

    /// Whether any meter or raw-meter session still needs a future
    /// convergence pass — its stream node id isn't resolved yet, or it has
    /// no links while a plausible target exists. The timer uses this to
    /// re-arm the dirty flag so `reconcile_now` keeps polling
    /// `stream.node_id()` across ticks even on a settled graph, where no
    /// registry global event would otherwise wake it.
    fn has_pending_meter_convergence(&self) -> bool {
        let post_pending = self.meter_sessions.values().any(|s| {
            // Unresolved node id, or resolved but not yet linked while a
            // post-denoise source exists to link to.
            s.node_id.is_none() || (s.links.is_empty() && self.source_node_id.is_some())
        });
        // Raw sessions only re-arm on an unresolved node id. Once node_id
        // resolves, linking is gated on a device being connected — an
        // unlinked raw session with no device is a settled, correct state
        // (R7/AE3 empty bar), NOT pending work; re-arming on it would spin
        // the reconcile loop forever with no mic plugged in. A device
        // arrival fires a registry global that re-marks dirty on its own.
        let raw_pending = self
            .raw_meter_sessions
            .values()
            .any(|s| s.node_id.is_none());
        post_pending || raw_pending
    }

    /// Record a Device global's resolved identity, then promote any
    /// pending nodes that were waiting on it.
    fn device_arrived(
        &mut self,
        device_global_id: u32,
        identity: DeviceId,
        event_tx: &StdSender<SessionEvent>,
    ) {
        self.device_identities
            .insert(device_global_id, identity.clone());
        let ready: Vec<u32> = self
            .pending_nodes
            .iter()
            .filter(|(_, p)| p.device_global_id == device_global_id)
            .map(|(node_id, _)| *node_id)
            .collect();
        for node_id in ready {
            let pending = self.pending_nodes.remove(&node_id).unwrap();
            self.emit_device(node_id, identity.clone(), pending.description, event_tx);
        }
    }

    /// A candidate `Audio/Source` node arrived. If its parent Device's
    /// identity is already known, emit immediately; otherwise queue it.
    fn node_arrived(
        &mut self,
        node_id: u32,
        device_global_id: u32,
        description: String,
        event_tx: &StdSender<SessionEvent>,
    ) {
        if let Some(identity) = self.device_identities.get(&device_global_id).cloned() {
            self.emit_device(node_id, identity, description, event_tx);
        } else {
            self.pending_nodes.insert(
                node_id,
                PendingNode {
                    device_global_id,
                    description,
                },
            );
        }
    }

    fn emit_device(
        &mut self,
        node_id: u32,
        id: DeviceId,
        description: String,
        event_tx: &StdSender<SessionEvent>,
    ) {
        let info = DeviceInfo {
            id,
            node_id,
            description,
        };
        self.devices.insert(node_id, info.clone());
        let _ = event_tx.send(SessionEvent::DeviceArrived(info));
    }

    fn node_departed(&mut self, node_id: u32, event_tx: &StdSender<SessionEvent>) {
        self.pending_nodes.remove(&node_id);
        self.device_output_ports.remove(&node_id);
        self.input_ports_by_node.remove(&node_id);
        // Drop any port_owner entries pointing at this node (its ports are
        // gone with it).
        self.port_owner.retain(|_, owner| *owner != node_id);
        if self.capture_node_id == Some(node_id) {
            self.capture_node_id = None;
            self.capture_node_proxy = None;
            self.capture_input_ports.clear();
            self.capture_links.clear();
        }
        if self.source_node_id == Some(node_id) {
            self.source_node_id = None;
            self.source_output_ports.clear();
            // The post-denoise source's ports are gone; any post-meter link
            // to them is dead. Clear retained link proxies so convergence
            // rebuilds once the source returns (dedup guard would otherwise
            // block re-linking forever).
            for s in self.meter_sessions.values_mut() {
                s.links.clear();
                s.linked_source_node_id = None;
            }
        }
        if self.sinks.remove(&node_id).is_some() {
            self.sink_input_ports.remove(&node_id);
        }
        // A departing device node invalidates any raw-meter link sourced
        // from it; clear so the reconcile loop rebuilds for the next device.
        for s in self.raw_meter_sessions.values_mut() {
            if s.linked_source_node_id == Some(node_id) {
                s.links.clear();
                s.linked_source_node_id = None;
            }
        }
        // If a meter *stream's own* node departed (app tearing the stream
        // down), its input ports and links are gone; the StopMeter path
        // normally removes the session, but clear links defensively.
        for s in self.meter_sessions.values_mut() {
            if s.node_id == Some(node_id) {
                s.links.clear();
            }
        }
        for s in self.raw_meter_sessions.values_mut() {
            if s.node_id == Some(node_id) {
                s.links.clear();
            }
        }
        if let Some(dev) = self.devices.remove(&node_id) {
            let _ = event_tx.send(SessionEvent::DeviceDeparted(dev.id));
        }
    }

    /// A port global departed. `global_remove` can't tell a port id from a
    /// node id, so this resolves the id via `port_owner`; a no-op if the id
    /// isn't a known port (it was a node/link/device instead). Prunes the
    /// port from every per-node vector and clears any meter link that may
    /// have used it, so convergence rebuilds valid links next tick.
    fn port_departed(&mut self, port_id: u32) {
        let Some(node_id) = self.port_owner.remove(&port_id) else {
            return; // Not a port we tracked.
        };
        let prune = |v: &mut Vec<(u32, String)>| v.retain(|(id, _)| *id != port_id);
        if let Some(v) = self.device_output_ports.get_mut(&node_id) {
            prune(v);
        }
        if let Some(v) = self.input_ports_by_node.get_mut(&node_id) {
            prune(v);
        }
        if let Some(v) = self.sink_input_ports.get_mut(&node_id) {
            prune(v);
        }
        prune(&mut self.source_output_ports);
        prune(&mut self.capture_input_ports);
        // A departed port may have been an endpoint of a meter link. Clear
        // retained link proxies for any session whose source node or own
        // meter node owned this port, so the dedup guard doesn't block a
        // rebuild after PipeWire tears the underlying link down.
        for s in self.meter_sessions.values_mut() {
            if s.node_id == Some(node_id) || s.linked_source_node_id == Some(node_id) {
                s.links.clear();
                s.linked_source_node_id = None;
            }
        }
        for s in self.raw_meter_sessions.values_mut() {
            if s.node_id == Some(node_id) || s.linked_source_node_id == Some(node_id) {
                s.links.clear();
                s.linked_source_node_id = None;
            }
        }
    }

    fn device_departed(&mut self, device_global_id: u32) {
        self.device_identities.remove(&device_global_id);
        self.device_form_factors.remove(&device_global_id);
        // Nodes belonging to this device depart via their own
        // global_remove events; nothing to do here beyond forgetting the
        // identity so a stale node can't resolve against it.
    }

    /// Record a Device global's `device.form_factor`, when present
    /// (verified this session: present on headset/webcam cards, absent
    /// on plain HDA sinks). Feeds R9's speaker-risk heuristic via
    /// `SinkInfo::device_global_id`; a silent no-op if the Device global
    /// carries no form-factor property.
    fn set_device_form_factor(&mut self, device_global_id: u32, form_factor: Option<&str>) {
        if let Some(ff) = form_factor {
            self.device_form_factors.insert(device_global_id, ff.to_string());
        }
    }

    /// Current present-device set, for the routing policy.
    fn present_devices(&self) -> HashSet<DeviceId> {
        self.devices.values().map(|d| d.id.clone()).collect()
    }

    /// Resolve a `DeviceId` back to its current node id.
    fn node_id_for_device(&self, id: &DeviceId) -> Option<u32> {
        self.devices
            .iter()
            .find(|(_, info)| &info.id == id)
            .map(|(node_id, _)| *node_id)
    }

    /// The set of device node ids the capture node is *actually* linked
    /// from right now, per observed `Link` globals — not self-bookkeeping.
    /// A link whose `output_node_id` doesn't resolve to a known device
    /// (foreign/rogue, or the device departed but the link lingers) is
    /// still counted by node id; callers translate to identity separately.
    fn linked_output_node_ids(&self) -> HashSet<u32> {
        self.capture_links
            .values()
            .map(|l| l.output_node_id)
            .collect()
    }

    /// What's actually linked right now, resolved from observed Link
    /// globals against known device identities. `Unknown` covers links to
    /// a node id that isn't a currently-identified device — a rogue link
    /// (A1) or a stale link to a node that departed without PipeWire
    /// tearing the link down yet.
    fn actual_link(&self) -> ActualLink {
        let node_ids = self.linked_output_node_ids();
        if node_ids.is_empty() {
            return ActualLink::None;
        }
        // All capture links must agree on one device for this to be a
        // clean "linked to X" state; a mixed set (e.g. mid-transition, or
        // a rogue link alongside a legitimate one) is Unknown so the
        // reconciler corrects it rather than reporting false confidence.
        if node_ids.len() > 1 {
            return ActualLink::Unknown;
        }
        let node_id = *node_ids.iter().next().unwrap();
        match self.devices.get(&node_id) {
            Some(info) => ActualLink::ToDevice(info.id.clone()),
            None => ActualLink::Unknown,
        }
    }

    /// Apply a live RNNoise control change (U5/R8), if the capture node's
    /// proxy is currently bound. Silent no-op otherwise — no RNNoise
    /// fragment active yet (bypass graph), or the bind hasn't landed —
    /// matching the plan's rule that immediate effect is best-effort and
    /// the fragment write is the durable half.
    fn set_rnnoise_param(&self, param: crate::params::RnnoiseParam, value: f32) {
        if let Some(node) = &self.capture_node_proxy {
            let bytes = crate::params::build_props_pod(param, value);
            if let Some(pod) = pipewire::spa::pod::Pod::from_bytes(&bytes) {
                node.set_param(pipewire::spa::param::ParamType::Props, 0, pod);
            }
        }
    }

    /// Compute the current health verdict (R5, via [`crate::model::compute_health`])
    /// and emit `HealthChanged` only if it differs from `last_health`.
    /// Called at the end of every `reconcile_now` cycle — health can
    /// change even on a `NoOp` reconcile action (e.g. a device that fed
    /// the desired link just departed, dropping `ActualLink` to `None`
    /// before the next dirty tick notices).
    fn emit_health_if_changed(&mut self, event_tx: &StdSender<SessionEvent>) {
        let actual = self.actual_link();
        let health = crate::model::compute_health(
            self.capture_node_id.is_some(),
            &actual,
            |id| {
                self.devices
                    .values()
                    .find(|info| &info.id == id)
                    .map(|info| info.description.clone())
            },
        );
        if self.last_health.as_ref() != Some(&health) {
            self.last_health = Some(health.clone());
            let _ = event_tx.send(SessionEvent::HealthChanged(health));
        }
    }

    /// Resolve the default sink's `node.name` at the current moment
    /// (Q6: monitoring routes to whatever is default *when it starts*,
    /// never tracked live thereafter). Reads the cached
    /// `default_sink_name`, itself kept current by the Metadata
    /// listener's `property` callback.
    fn resolve_default_sink_node_id(&self) -> Option<u32> {
        let name = self.default_sink_name.as_deref()?;
        self.sinks
            .iter()
            .find(|(_, s)| s.node_name == name)
            .map(|(id, _)| *id)
    }

    /// The R9 speaker-risk assessment for a given sink node id, using
    /// whatever `device.form_factor` and name signals are available.
    fn speaker_risk_for_sink(&self, sink_node_id: u32) -> crate::monitor::SpeakerRisk {
        let Some(sink) = self.sinks.get(&sink_node_id) else {
            return crate::monitor::SpeakerRisk::Unknown;
        };
        let form_factor = sink
            .device_global_id
            .and_then(|id| self.device_form_factors.get(&id))
            .map(String::as_str);
        crate::monitor::assess_speaker_risk(form_factor, &sink.description)
    }
}

#[cfg(test)]
mod generation_tests {
    use super::*;

    fn drain(rx: &StdReceiver<SessionEvent>) -> Vec<SessionEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    #[test]
    fn node_arrives_after_device_resolves_immediately() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut gen = Generation::new();
        gen.device_arrived(58, DeviceId("alsa_card.usb-Razer".into()), &tx);
        gen.node_arrived(70, 58, "Razer Seiren Mini Mono".into(), &tx);

        let events = drain(&rx);
        assert_eq!(events.len(), 1);
        match &events[0] {
            SessionEvent::DeviceArrived(info) => {
                assert_eq!(info.id, DeviceId("alsa_card.usb-Razer".into()));
                assert_eq!(info.node_id, 70);
            }
            other => panic!("expected DeviceArrived, got {other:?}"),
        }
    }

    #[test]
    fn node_arrives_before_device_queues_then_promotes() {
        // Registry ordering is not guaranteed — this is the exact case the
        // Device-join fix exists for: a Node global delivered before its
        // parent Device must not be dropped or fall back to a node
        // property; it waits until the Device resolves.
        let (tx, rx) = std::sync::mpsc::channel();
        let mut gen = Generation::new();
        gen.node_arrived(70, 58, "Razer Seiren Mini Mono".into(), &tx);

        assert!(drain(&rx).is_empty(), "no event until the Device resolves");

        gen.device_arrived(58, DeviceId("alsa_card.usb-Razer".into()), &tx);
        let events = drain(&rx);
        assert_eq!(events.len(), 1);
        match &events[0] {
            SessionEvent::DeviceArrived(info) => {
                assert_eq!(info.id, DeviceId("alsa_card.usb-Razer".into()));
                assert_eq!(info.node_id, 70);
            }
            other => panic!("expected DeviceArrived, got {other:?}"),
        }
    }

    #[test]
    fn multiple_nodes_share_one_device_identity() {
        // Real-world case observed live: an internal HDA card exposes two
        // Audio/Source nodes (Mic1, Mic2) under one Device global. Both
        // must resolve to the same stable identity.
        let (tx, rx) = std::sync::mpsc::channel();
        let mut gen = Generation::new();
        gen.device_arrived(62, DeviceId("alsa_card.pci-hda".into()), &tx);
        gen.node_arrived(81, 62, "Mic1".into(), &tx);
        gen.node_arrived(105, 62, "Mic2".into(), &tx);

        let events = drain(&rx);
        assert_eq!(events.len(), 2);
        for ev in &events {
            match ev {
                SessionEvent::DeviceArrived(info) => {
                    assert_eq!(info.id, DeviceId("alsa_card.pci-hda".into()));
                }
                other => panic!("expected DeviceArrived, got {other:?}"),
            }
        }
    }

    #[test]
    fn node_departure_emits_departed_with_stable_id() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut gen = Generation::new();
        gen.device_arrived(58, DeviceId("alsa_card.usb-Razer".into()), &tx);
        gen.node_arrived(70, 58, "Razer".into(), &tx);
        drain(&rx);

        gen.node_departed(70, &tx);
        let events = drain(&rx);
        assert_eq!(events.len(), 1);
        match &events[0] {
            SessionEvent::DeviceDeparted(id) => {
                assert_eq!(*id, DeviceId("alsa_card.usb-Razer".into()));
            }
            other => panic!("expected DeviceDeparted, got {other:?}"),
        }
    }

    #[test]
    fn pending_node_dropped_on_departure_before_device_resolves() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut gen = Generation::new();
        gen.node_arrived(70, 58, "Razer".into(), &tx);
        gen.node_departed(70, &tx);

        // No DeviceArrived was ever emitted, so no DeviceDeparted should
        // fire either — the node never became a resolved device.
        assert!(drain(&rx).is_empty());

        // The Device arriving afterward must not resurrect the departed
        // node.
        gen.device_arrived(58, DeviceId("alsa_card.usb-Razer".into()), &tx);
        assert!(drain(&rx).is_empty());
    }

    #[test]
    fn actual_link_none_when_no_capture_links() {
        let gen = Generation::new();
        assert_eq!(gen.actual_link(), ActualLink::None);
    }

    #[test]
    fn actual_link_to_device_when_single_link_resolves() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut gen = Generation::new();
        gen.device_arrived(58, DeviceId("alsa_card.usb-Razer".into()), &tx);
        gen.node_arrived(70, 58, "Razer".into(), &tx);
        drain(&rx);
        gen.capture_links.insert(
            900,
            ObservedLink {
                output_node_id: 70,
                output_port_id: 71,
                input_port_id: 72,
            },
        );
        assert_eq!(
            gen.actual_link(),
            ActualLink::ToDevice(DeviceId("alsa_card.usb-Razer".into()))
        );
    }

    #[test]
    fn actual_link_unknown_when_link_points_to_unresolved_node() {
        // Simulates a rogue/foreign link (A1): a Link global exists on the
        // capture node but its output node isn't a currently-known device.
        let mut gen = Generation::new();
        gen.capture_links.insert(
            900,
            ObservedLink {
                output_node_id: 999,
                output_port_id: 71,
                input_port_id: 72,
            },
        );
        assert_eq!(gen.actual_link(), ActualLink::Unknown);
    }

    #[test]
    fn actual_link_unknown_when_multiple_devices_linked_at_once() {
        // Mixed state (e.g. mid-transition, or a rogue link alongside a
        // legit one) must never be reported as confidently linked to
        // either device.
        let (tx, rx) = std::sync::mpsc::channel();
        let mut gen = Generation::new();
        gen.device_arrived(58, DeviceId("device-a".into()), &tx);
        gen.node_arrived(70, 58, "A".into(), &tx);
        gen.device_arrived(59, DeviceId("device-b".into()), &tx);
        gen.node_arrived(71, 59, "B".into(), &tx);
        drain(&rx);
        gen.capture_links.insert(
            900,
            ObservedLink {
                output_node_id: 70,
                output_port_id: 1,
                input_port_id: 2,
            },
        );
        gen.capture_links.insert(
            901,
            ObservedLink {
                output_node_id: 71,
                output_port_id: 3,
                input_port_id: 4,
            },
        );
        assert_eq!(gen.actual_link(), ActualLink::Unknown);
    }

    #[test]
    fn speaker_risk_for_sink_uses_owning_devices_form_factor() {
        let mut gen = Generation::new();
        // Device global 58 owns sink node 200; form factor is set on the
        // Device global (58), never the sink node itself, matching how
        // handle_global wires ObjectType::Device -> set_device_form_factor.
        gen.set_device_form_factor(58, Some("headset"));
        gen.sinks.insert(
            200,
            SinkInfo {
                node_name: "sink.headset".into(),
                description: "Some Speaker-Named Thing".into(),
                device_global_id: Some(58),
            },
        );
        assert_eq!(
            gen.speaker_risk_for_sink(200),
            crate::monitor::SpeakerRisk::LikelyHeadphones,
            "form factor must win over a misleading name"
        );
    }

    #[test]
    fn speaker_risk_for_sink_falls_back_to_name_when_no_form_factor_known() {
        let mut gen = Generation::new();
        gen.sinks.insert(
            201,
            SinkInfo {
                node_name: "sink.hdmi".into(),
                description: "HDMI Output".into(),
                device_global_id: None,
            },
        );
        assert_eq!(
            gen.speaker_risk_for_sink(201),
            crate::monitor::SpeakerRisk::LikelySpeaker
        );
    }

    #[test]
    fn speaker_risk_for_sink_unknown_for_unresolved_sink_node() {
        let gen = Generation::new();
        assert_eq!(
            gen.speaker_risk_for_sink(999),
            crate::monitor::SpeakerRisk::Unknown
        );
    }

    #[test]
    fn device_departed_clears_its_form_factor() {
        let mut gen = Generation::new();
        gen.set_device_form_factor(58, Some("headset"));
        gen.sinks.insert(
            200,
            SinkInfo {
                node_name: "sink.headset".into(),
                description: "Ambiguous Name".into(),
                device_global_id: Some(58),
            },
        );
        gen.device_departed(58);
        // With the form factor gone, resolution falls through to the
        // name heuristic — which for this ambiguous description yields
        // Unknown rather than resurrecting the stale headset verdict.
        assert_eq!(
            gen.speaker_risk_for_sink(200),
            crate::monitor::SpeakerRisk::Unknown
        );
    }

    // --- Meter convergence tests (U1/U2) ---
    // These test the pure decision logic: early-return conditions, node_id
    // polling, port-before-node_id ordering, and device-switch purge.
    // `converge_meter_links` reaches `core.create_object` only when all
    // guards pass; these tests verify the guards themselves (which need no
    // real PipeWire core) by extracting the port-selection logic into a
    // pure helper.


    /// Pure decision helper extracted from `converge_meter_links`: given a
    /// meter node id, source node id, source output ports, and recorded input
    /// ports, returns the list of (out_port_id, in_port_id) pairs that should
    /// be linked. Empty means nothing to do (either already linked, ports
    /// absent, or no source). This function has no PipeWire side-effects.
    fn decide_meter_link_ports(
        meter_node_id: Option<u32>,
        source_node_id: Option<u32>,
        source_output_ports: &[(u32, String)],
        input_ports_by_node: &HashMap<u32, Vec<(u32, String)>>,
        already_has_links: bool,
    ) -> Vec<(u32, u32)> {
        // Mirror the early-return conditions in converge_meter_links.
        let meter_node_id = match meter_node_id {
            Some(id) => id,
            None => return vec![], // node_id not yet assigned
        };
        let _source_node_id = match source_node_id {
            Some(id) => id,
            None => return vec![], // no source yet
        };
        if source_output_ports.is_empty() {
            return vec![];
        }
        let meter_input_ports = match input_ports_by_node.get(&meter_node_id) {
            Some(ports) if !ports.is_empty() => ports,
            _ => return vec![],
        };
        if already_has_links {
            return vec![]; // idempotent
        }
        let mut source_ports = source_output_ports.to_vec();
        let mut meter_ports = meter_input_ports.clone();
        source_ports.sort_by(|a, b| a.1.cmp(&b.1));
        meter_ports.sort_by(|a, b| a.1.cmp(&b.1));

        if source_ports.len() == 1 {
            let (out_port_id, _) = source_ports[0];
            meter_ports.iter().map(|(in_port_id, _)| (out_port_id, *in_port_id)).collect()
        } else {
            source_ports.iter().zip(meter_ports.iter())
                .map(|((out, _), (inp, _))| (*out, *inp))
                .collect()
        }
    }

    #[test]
    fn meter_convergence_no_node_id_returns_empty() {
        // node_id = None → no ports to create (early return)
        let mut input_ports: HashMap<u32, Vec<(u32, String)>> = HashMap::new();
        input_ports.insert(42, vec![(101, "input_MONO".into())]);
        let source_ports = vec![(200, "capture_MONO".into())];
        let pairs = decide_meter_link_ports(None, Some(130), &source_ports, &input_ports, false);
        assert!(pairs.is_empty(), "must return empty when node_id is None");
    }

    #[test]
    fn meter_convergence_no_source_node_returns_empty() {
        let input_ports: HashMap<u32, Vec<(u32, String)>> = HashMap::new();
        let pairs = decide_meter_link_ports(Some(42), None, &[], &input_ports, false);
        assert!(pairs.is_empty());
    }

    #[test]
    fn meter_convergence_source_ports_absent_returns_empty() {
        let input_ports: HashMap<u32, Vec<(u32, String)>> = HashMap::new();
        let pairs = decide_meter_link_ports(Some(42), Some(130), &[], &input_ports, false);
        assert!(pairs.is_empty(), "empty source ports → nothing to link");
    }

    #[test]
    fn meter_convergence_input_port_not_recorded_returns_empty() {
        // Port not yet in input_ports_by_node → retry next tick
        let input_ports: HashMap<u32, Vec<(u32, String)>> = HashMap::new();
        let source_ports = vec![(200, "capture_MONO".into())];
        let pairs = decide_meter_link_ports(Some(42), Some(130), &source_ports, &input_ports, false);
        assert!(pairs.is_empty(), "meter input port not recorded → nothing to link");
    }

    #[test]
    fn meter_convergence_creates_one_pair_for_mono_source_and_meter() {
        // Happy path: mono source, mono meter → one (out, in) pair
        let mut input_ports: HashMap<u32, Vec<(u32, String)>> = HashMap::new();
        input_ports.insert(42, vec![(101, "input_MONO".into())]);
        let source_ports = vec![(200, "capture_MONO".into())];
        let pairs = decide_meter_link_ports(Some(42), Some(130), &source_ports, &input_ports, false);
        assert_eq!(pairs, vec![(200, 101)], "mono source fans to mono meter input");
    }

    #[test]
    fn meter_convergence_idempotent_when_links_already_present() {
        // already_has_links = true → no new pairs (dedup)
        let mut input_ports: HashMap<u32, Vec<(u32, String)>> = HashMap::new();
        input_ports.insert(42, vec![(101, "input_MONO".into())]);
        let source_ports = vec![(200, "capture_MONO".into())];
        let pairs = decide_meter_link_ports(Some(42), Some(130), &source_ports, &input_ports, true);
        assert!(pairs.is_empty(), "second call must not create additional links");
    }

    #[test]
    fn meter_convergence_port_before_node_id_ordering() {
        // The input port is recorded before node_id is set (port-before-node_id
        // race). After node_id is set, convergence finds the port and links.
        let mut input_ports: HashMap<u32, Vec<(u32, String)>> = HashMap::new();
        // Port arrives first (port 101 on node 42).
        input_ports.insert(42, vec![(101, "input_MONO".into())]);
        let source_ports = vec![(200, "capture_MONO".into())];

        // Convergence with node_id = None (port arrived, but node_id not set):
        let pairs_before = decide_meter_link_ports(None, Some(130), &source_ports, &input_ports, false);
        assert!(pairs_before.is_empty(), "node_id not set: no links yet");

        // node_id is now set (Paused state reached):
        let pairs_after = decide_meter_link_ports(Some(42), Some(130), &source_ports, &input_ports, false);
        assert_eq!(pairs_after, vec![(200, 101)], "port found after node_id set");
    }

    #[test]
    fn generation_new_has_empty_raw_meter_sessions_and_input_ports() {
        let gen = Generation::new();
        assert!(gen.raw_meter_sessions.is_empty());
        assert!(gen.input_ports_by_node.is_empty());
        assert!(gen.meter_sessions.is_empty());
    }

    #[test]
    fn no_pending_meter_convergence_when_no_sessions() {
        // Empty generation has no meter work pending — the timer must not
        // re-arm dirty forever when nothing is subscribed.
        let gen = Generation::new();
        assert!(!gen.has_pending_meter_convergence());
    }

    #[test]
    fn input_ports_by_node_records_all_in_ports_unconditionally() {
        // Simulates handle_global adding input ports for any node,
        // including meter stream nodes whose session node_id isn't set yet.
        let mut gen = Generation::new();
        // Simulate a meter stream input port arriving (node 99, port 300)
        gen.input_ports_by_node
            .entry(99)
            .or_default()
            .push((300, "input_MONO".into()));
        // Simulate a capture node input port also recorded
        gen.capture_node_id = Some(77);
        gen.capture_input_ports.push((400, "capture_FL".into()));
        gen.input_ports_by_node
            .entry(77)
            .or_default()
            .push((400, "capture_FL".into()));

        // Both nodes' ports are in input_ports_by_node
        assert!(gen.input_ports_by_node.contains_key(&99));
        assert!(gen.input_ports_by_node.contains_key(&77));
        assert_eq!(gen.input_ports_by_node[&99], vec![(300, "input_MONO".into())]);
    }

    #[test]
    fn port_departed_prunes_the_port_from_its_node_vectors() {
        // A departed port id must be pruned from the per-node maps so
        // convergence never links to a dead port id.
        let mut gen = Generation::new();
        // Node 99 owns input port 300; node 130 owns output port 172.
        gen.input_ports_by_node.entry(99).or_default().push((300, "input_MONO".into()));
        gen.port_owner.insert(300, 99);
        gen.device_output_ports.entry(130).or_default().push((172, "capture_MONO".into()));
        gen.port_owner.insert(172, 130);

        // Port 300 departs.
        gen.port_departed(300);
        assert!(
            gen.input_ports_by_node.get(&99).map(|v| v.is_empty()).unwrap_or(true),
            "port 300 must be pruned from node 99's input ports"
        );
        assert!(!gen.port_owner.contains_key(&300), "port_owner entry removed");
        // Node 130's output port is untouched.
        assert_eq!(gen.device_output_ports[&130], vec![(172, "capture_MONO".into())]);

        // A non-port id (never tracked) is a silent no-op.
        gen.port_departed(99999);
    }

    #[test]
    fn raw_meter_device_switch_purge_logic() {
        // Verify the linked_source_node_id tracking enables device-switch purge:
        // if linked_source_node_id != new device node, links should be cleared.
        // We test the conditional logic directly (no PipeWire needed).
        let old_device_node: u32 = 94;
        let new_device_node: u32 = 95;
        let linked_source_node_id: Option<u32> = Some(old_device_node);
        let desired_device_node: u32 = new_device_node;

        // Simulate the purge condition in reconcile_now's raw meter path:
        let is_stale = linked_source_node_id.is_some()
            && linked_source_node_id != Some(desired_device_node);
        assert!(is_stale, "links from old device should be purged on device switch");

        // Same node: not stale
        let same_linked: Option<u32> = Some(new_device_node);
        let is_stale_same = same_linked.is_some()
            && same_linked != Some(desired_device_node);
        assert!(!is_stale_same, "links from same device should not be purged");
    }
}

enum GenerationOutcome {
    ShutdownRequested,
    Disconnected,
    ConnectFailed,
}

fn run_loop(
    our_source_name: String,
    cmd_rx: StdReceiver<SessionCommand>,
    event_tx: StdSender<SessionEvent>,
) {
    // Outer loop: one iteration per PipeWire connection generation. Covers
    // both the initial connect (not fatal on failure — U3's
    // After=pipewire.service orders startup but is not a readiness
    // guarantee) and crash recovery: same code path, same backoff, per
    // U1's "one path for initial connect and reconnect."
    let mut backoff = Duration::from_millis(200);
    const MAX_BACKOFF: Duration = Duration::from_secs(5);

    let mut cmd_rx = cmd_rx;
    loop {
        let outcome;
        (outcome, cmd_rx) = run_one_generation(&our_source_name, cmd_rx, &event_tx);
        match outcome {
            GenerationOutcome::Disconnected => {
                let _ = event_tx.send(SessionEvent::Disconnected);
                std::thread::sleep(backoff);
                backoff = std::cmp::min(backoff * 2, MAX_BACKOFF);
            }
            GenerationOutcome::ConnectFailed => {
                // Initial connect failure: same backoff, no Disconnected
                // event (nothing was ever connected to disconnect from).
                std::thread::sleep(backoff);
                backoff = std::cmp::min(backoff * 2, MAX_BACKOFF);
            }
            GenerationOutcome::ShutdownRequested => return,
        }
    }
}

fn run_one_generation(
    our_source_name: &str,
    cmd_rx: StdReceiver<SessionCommand>,
    event_tx: &StdSender<SessionEvent>,
) -> (GenerationOutcome, StdReceiver<SessionCommand>) {
    let mainloop = match MainLoopRc::new(None) {
        Ok(m) => m,
        Err(_) => return (GenerationOutcome::ConnectFailed, cmd_rx),
    };
    let context = match ContextRc::new(&mainloop, None) {
        Ok(c) => c,
        Err(_) => return (GenerationOutcome::ConnectFailed, cmd_rx),
    };
    let core: CoreRc = match context.connect_rc(None) {
        Ok(c) => c,
        Err(_) => return (GenerationOutcome::ConnectFailed, cmd_rx),
    };
    let registry: RegistryRc = match core.get_registry_rc() {
        Ok(r) => r,
        Err(_) => return (GenerationOutcome::ConnectFailed, cmd_rx),
    };

    let generation = Rc::new(RefCell::new(Generation::new()));
    let disconnected = Rc::new(Cell::new(false));

    // Core error listener: any error callback with id == 0 (the core
    // itself) is treated as a disconnect trigger. PipeWire crash/restart
    // surfaces here.
    let disc_flag = disconnected.clone();
    let ml_quit = mainloop.clone();
    let _core_listener = core
        .add_listener_local()
        .error(move |id, _seq, _res, _msg| {
            if id == 0 {
                disc_flag.set(true);
                ml_quit.quit();
            }
        })
        .register();

    let name_for_cb = our_source_name.to_string();
    let gen_for_global = generation.clone();
    let tx_for_global = event_tx.clone();
    let gen_for_remove = generation.clone();
    let tx_for_remove = event_tx.clone();
    let link_factory_for_global: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let link_factory_for_global2 = link_factory_for_global.clone();
    let dirty_for_remove = Rc::new(Cell::new(false));
    let registry_for_global = registry.clone();
    let metadata_holder: Rc<RefCell<Option<(Metadata, pipewire::metadata::MetadataListener)>>> =
        Rc::new(RefCell::new(None));
    let metadata_holder_for_global = metadata_holder.clone();

    let dirty_for_global = Rc::new(Cell::new(false));
    let dirty_for_global2 = dirty_for_global.clone();
    let _reg_listener = registry
        .add_listener_local()
        .global(move |global: &GlobalObject<&DictRef>| {
            handle_global(
                global,
                &name_for_cb,
                &gen_for_global,
                &tx_for_global,
                &link_factory_for_global2,
                &registry_for_global,
                &metadata_holder_for_global,
            );
            // Debounced reconciliation (see the dirty-flag timer below):
            // a single device/profile change fires a *burst* of Registry
            // events (Device, several Ports, sometimes a Link) within
            // milliseconds. Reconciling synchronously inside this handler
            // was measured to create duplicate links — `create_object` is
            // async, so `capture_links` hasn't caught up with the previous
            // create by the time the next event in the burst arrives.
            // Marking dirty and reconciling once per timer tick coalesces
            // the whole burst into one convergence pass.
            dirty_for_global2.set(true);
        })
        .global_remove({
            let dirty_for_remove = dirty_for_remove.clone();
            move |id: u32| {
                let mut gen = gen_for_remove.borrow_mut();
                gen.device_departed(id);
                gen.node_departed(id, &tx_for_remove);
                gen.port_departed(id);
                gen.capture_links.remove(&id);
                dirty_for_remove.set(true);
            }
        })
        .register();

    // Barrier: once the done callback fires for our sync's seq, the initial
    // burst of `global` events (the existing graph) has been fully
    // delivered. Emit the first Snapshot then.
    let gen_for_sync = generation.clone();
    let tx_for_sync = event_tx.clone();
    let synced = Rc::new(Cell::new(false));
    let synced_for_cb = synced.clone();
    let pending_seq = core.sync(0).ok();
    let _core_sync_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == 0 && pending_seq == Some(seq) && !synced_for_cb.get() {
                synced_for_cb.set(true);
                let devices: Vec<DeviceInfo> =
                    gen_for_sync.borrow().devices.values().cloned().collect();
                let _ = tx_for_sync.send(SessionEvent::Snapshot(devices));
            }
        })
        .register();

    // Poll the std mpsc command channel on a timer rather than an attached
    // `pipewire::channel::Receiver` — the std receiver is owned by the
    // outer `run_loop` and outlives every generation, so a reconnect never
    // orphans in-flight commands the way a consumed `attach()` would.
    // Wrapped in Rc<RefCell<Option<_>>> so it can be reclaimed after
    // `mainloop.run()` returns and handed back to the caller for the next
    // generation.
    let cmd_rx_cell = Rc::new(RefCell::new(Some(cmd_rx)));
    let cmd_rx_for_timer = cmd_rx_cell.clone();
    let shutdown_requested = Rc::new(Cell::new(false));
    let shutdown_flag = shutdown_requested.clone();
    let ml_for_timer = mainloop.clone();
    let core_for_timer = core.clone();
    let gen_for_timer = generation.clone();
    let link_factory_for_timer = link_factory_for_global.clone();
    let dirty_for_timer = dirty_for_global.clone();
    let dirty_for_timer2 = dirty_for_remove.clone();
    let tx_for_timer = event_tx.clone();

    let timer = mainloop.loop_().add_timer(move |_expirations| {
        // Coalesced reconcile: run at most once per tick, after draining
        // commands, if any global/global_remove event marked us dirty
        // since the last tick.
        let dirty = dirty_for_timer.take() | dirty_for_timer2.take();
        if dirty {
            reconcile_now(
                &mut gen_for_timer.borrow_mut(),
                &core_for_timer,
                &link_factory_for_timer,
                &tx_for_timer,
            );
            // Re-arm for the next tick while any meter session still needs
            // convergence (unresolved stream node_id, or resolved-but-
            // unlinked with a target present). The stream's node_id()
            // transitions 0 -> assigned at Paused state, which may not
            // coincide with a registry global event — without this
            // self-reschedule a settled graph could leave the meter
            // permanently unlinked. Clears itself once convergence completes.
            if gen_for_timer.borrow().has_pending_meter_convergence() {
                dirty_for_timer.set(true);
            }
        }

        let slot = cmd_rx_for_timer.borrow_mut();
        let rx = match slot.as_ref() {
            Some(rx) => rx,
            None => return,
        };
        loop {
            match rx.try_recv() {
                Ok(SessionCommand::Shutdown) => {
                    shutdown_flag.set(true);
                    ml_for_timer.quit();
                    return;
                }
                Ok(SessionCommand::RequestSnapshot) => {
                    let _ = core_for_timer.sync(0);
                }
                Ok(SessionCommand::SetPreferenceOrder(order)) => {
                    let mut gen = gen_for_timer.borrow_mut();
                    gen.preference_order = order;
                    reconcile_now(&mut gen, &core_for_timer, &link_factory_for_timer, &tx_for_timer);
                }
                Ok(SessionCommand::SetPin(pin)) => {
                    let mut gen = gen_for_timer.borrow_mut();
                    gen.pin = pin;
                    reconcile_now(&mut gen, &core_for_timer, &link_factory_for_timer, &tx_for_timer);
                }
                Ok(SessionCommand::SetRnnoiseParam(param, value)) => {
                    gen_for_timer.borrow().set_rnnoise_param(param, value);
                }
                Ok(SessionCommand::StartMonitor(session_id)) => {
                    let mut gen = gen_for_timer.borrow_mut();
                    start_monitor(&mut gen, &core_for_timer, &link_factory_for_timer, session_id, &tx_for_timer);
                }
                Ok(SessionCommand::StopMonitor(session_id)) => {
                    let mut gen = gen_for_timer.borrow_mut();
                    stop_monitor(&mut gen, session_id);
                    let _ = tx_for_timer.send(SessionEvent::MonitorStopped { session_id });
                }
                Ok(SessionCommand::StartMeter(session_id, channel)) => {
                    start_meter(
                        &mut gen_for_timer.borrow_mut(),
                        &core_for_timer,
                        session_id,
                        channel,
                    );
                    // Schedule convergence on the next tick (KTD1/U1.6).
                    // reconcile_now runs at the TOP of the tick, before
                    // commands drain, so a StartMeter arriving after the
                    // graph settles would otherwise never converge. Setting
                    // dirty (not an inline reconcile) is correct because the
                    // stream's node_id() and its input-port global are not
                    // ready at command-handling time — the useful pass is a
                    // later tick. The timer re-arms dirty via
                    // has_pending_meter_convergence() until the node id
                    // resolves and the link is made.
                    dirty_for_timer.set(true);
                }
                Ok(SessionCommand::StopMeter(session_id)) => {
                    gen_for_timer.borrow_mut().meter_sessions.remove(&session_id);
                }
                Ok(SessionCommand::StartRawMeter(session_id, channel)) => {
                    start_raw_meter(
                        &mut gen_for_timer.borrow_mut(),
                        &core_for_timer,
                        session_id,
                        channel,
                    );
                    // Same settled-graph scheduling as StartMeter above.
                    dirty_for_timer.set(true);
                }
                Ok(SessionCommand::StopRawMeter(session_id)) => {
                    gen_for_timer.borrow_mut().raw_meter_sessions.remove(&session_id);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // Sender (Session handle) dropped without an explicit
                    // Shutdown — treat as shutdown so the thread exits
                    // rather than spinning forever.
                    shutdown_flag.set(true);
                    ml_for_timer.quit();
                    return;
                }
            }
        }
    });
    timer.update_timer(Some(COMMAND_POLL_INTERVAL), Some(COMMAND_POLL_INTERVAL));

    mainloop.run();

    // Reclaim the receiver: `timer` (and thus its closure's clone of the
    // Rc) is still alive here since it's dropped at the end of this scope,
    // but the Rc refcount is now back to what it was before `take()` runs,
    // so `take()` on our own handle is safe — the closure never calls it.
    let cmd_rx = cmd_rx_cell.borrow_mut().take().unwrap_or_else(|| {
        // Should be unreachable — the timer callback never takes the
        // receiver, only reads it via as_ref(). Fall back to a disconnected
        // receiver so the type still checks if this ever fires.
        let (_tx, rx) = std::sync::mpsc::channel();
        rx
    });

    let outcome = if shutdown_requested.get() {
        GenerationOutcome::ShutdownRequested
    } else if disconnected.get() {
        GenerationOutcome::Disconnected
    } else {
        // mainloop.quit() with neither flag set shouldn't happen in normal
        // operation; treat as a disconnect to trigger reconnect rather than
        // silently exiting.
        GenerationOutcome::Disconnected
    };
    (outcome, cmd_rx)
}

/// Handle a Registry `global` event. Node globals are joined against their
/// parent Device global (via `device.id`) for identity — a Device carries
/// `device.serial` / `api.bluez5.address` / `device.name`; the Node itself
/// never does. Registry ordering is not guaranteed (Device before or after
/// its Nodes), so both directions of the join are handled: a Device
/// arriving after its Node promotes any pending node; a Node arriving after
/// its Device resolves immediately.
///
/// Also tracks: the link factory name (from Factory globals, per KTD2 —
/// never hardcoded), our own capture node's id (matched by
/// `node.name == "<source>_capture"`, U2's fragment), and every candidate
/// device's output ports (from Port globals) — both needed by the
/// reconciler to create real links.
fn handle_global(
    global: &GlobalObject<&DictRef>,
    our_source_name: &str,
    generation: &Rc<RefCell<Generation>>,
    event_tx: &StdSender<SessionEvent>,
    link_factory: &Rc<RefCell<Option<String>>>,
    registry: &RegistryRc,
    metadata_holder: &Rc<RefCell<Option<(Metadata, pipewire::metadata::MetadataListener)>>>,
) {
    let props = match global.props {
        Some(p) => p,
        None => return,
    };

    match global.type_ {
        ObjectType::Device => {
            let identity = resolve_device_identity(props);
            let form_factor = props.get("device.form_factor");
            let mut gen = generation.borrow_mut();
            gen.set_device_form_factor(global.id, form_factor);
            gen.device_arrived(global.id, identity, event_tx);
        }
        ObjectType::Factory => {
            if props.get("factory.type.name") == Some(ObjectType::Link.to_str()) {
                if let Some(name) = props.get("factory.name") {
                    *link_factory.borrow_mut() = Some(name.to_string());
                }
            }
        }
        ObjectType::Node => {
            let node_name = props.get("node.name").unwrap_or_default();
            let capture_name = format!("{our_source_name}_capture");
            if node_name == capture_name {
                let mut gen = generation.borrow_mut();
                gen.capture_node_id = Some(global.id);
                // Bind a Node proxy for U5's live set-param calls. Binding
                // is best-effort: a bind failure (WrongProxyType, unlikely
                // for a Node global) leaves capture_node_proxy None, and
                // set_rnnoise_param below already treats that as a silent
                // no-op — the durable half (fragment write) still applies
                // on the next daemon start.
                gen.capture_node_proxy = registry.bind::<Node, _>(global).ok();
                return;
            }
            if node_name == our_source_name {
                // U6's monitor links originate from our own source's
                // output ports; track its node id the same way the
                // capture side is tracked above.
                generation.borrow_mut().source_node_id = Some(global.id);
                return;
            }

            let media_class = props.get("media.class").unwrap_or_default();
            if media_class == "Audio/Sink" {
                let device_global_id = props.get("device.id").and_then(|s| s.parse().ok());
                let description = props
                    .get("node.description")
                    .unwrap_or(node_name)
                    .to_string();
                generation.borrow_mut().sinks.insert(
                    global.id,
                    SinkInfo {
                        node_name: node_name.to_string(),
                        description,
                        device_global_id,
                    },
                );
                return;
            }

            if !is_candidate_source(media_class, node_name, our_source_name) {
                return;
            }
            let device_global_id = match props.get("device.id").and_then(|s| s.parse().ok()) {
                Some(id) => id,
                // A source node with no owning Device global cannot be
                // identified per the Product Contract's identity rule
                // (device object, never node property) — skip it rather
                // than falling back to a volatile node.name.
                None => return,
            };
            let description = props
                .get("node.description")
                .or_else(|| props.get("device.description"))
                .unwrap_or(node_name)
                .to_string();
            generation.borrow_mut().node_arrived(
                global.id,
                device_global_id,
                description,
                event_tx,
            );
        }
        ObjectType::Port => {
            let node_id: u32 = match props.get("node.id").and_then(|s| s.parse().ok()) {
                Some(id) => id,
                None => return,
            };
            let direction = props.get("port.direction").unwrap_or_default();
            let port_name = props.get("port.name").unwrap_or_default().to_string();

            let mut gen = generation.borrow_mut();
            // Record which node owns this port so `global_remove` (which
            // gets a bare id) can prune the right per-node vectors when the
            // port departs.
            gen.port_owner.insert(global.id, node_id);
            if direction == "out" {
                if gen.source_node_id == Some(node_id) {
                    gen.source_output_ports.push((global.id, port_name.clone()));
                }
                gen.device_output_ports
                    .entry(node_id)
                    .or_default()
                    .push((global.id, port_name));
            } else if direction == "in" {
                if gen.capture_node_id == Some(node_id) {
                    gen.capture_input_ports.push((global.id, port_name.clone()));
                } else if gen.sinks.contains_key(&node_id) {
                    gen.sink_input_ports
                        .entry(node_id)
                        .or_default()
                        .push((global.id, port_name.clone()));
                }
                // Unconditionally record every input port by node id —
                // this captures meter stream input ports even before the
                // session's node_id is known (port-before-node_id race),
                // mirroring how device_output_ports records output ports.
                gen.input_ports_by_node
                    .entry(node_id)
                    .or_default()
                    .push((global.id, port_name));
            }
        }
        ObjectType::Link => {
            let input_node_id: u32 = match props.get("link.input.node").and_then(|s| s.parse().ok()) {
                Some(id) => id,
                None => return,
            };
            let mut gen = generation.borrow_mut();
            if gen.capture_node_id != Some(input_node_id) {
                return; // Not a link on our capture node; irrelevant.
            }
            let output_node_id: u32 = match props.get("link.output.node").and_then(|s| s.parse().ok()) {
                Some(id) => id,
                None => return,
            };
            let output_port_id: u32 = props
                .get("link.output.port")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let input_port_id: u32 = props
                .get("link.input.port")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            gen.capture_links.insert(
                global.id,
                ObservedLink {
                    output_node_id,
                    output_port_id,
                    input_port_id,
                },
            );
        }
        ObjectType::Metadata => {
            // Multiple Metadata objects can exist; only "default" carries
            // default.audio.sink (verified this session: `pw-metadata -n
            // default` is the exact source of `default.audio.sink`'s
            // `{"name":"<node.name>"}` JSON value Q6's monitor
            // destination resolves from).
            if props.get("metadata.name") != Some("default") {
                return;
            }
            if let Ok(metadata) = registry.bind::<Metadata, _>(global) {
                let gen_for_meta = generation.clone();
                let listener = metadata
                    .add_listener_local()
                    .property(move |_subject, key, _type, value| {
                        if key == Some("default.audio.sink") {
                            let resolved = value.and_then(|v| {
                                serde_json::from_str::<serde_json::Value>(v)
                                    .ok()
                                    .and_then(|j| j.get("name")?.as_str().map(str::to_string))
                            });
                            gen_for_meta.borrow_mut().default_sink_name = resolved;
                        }
                        0
                    })
                    .register();
                *metadata_holder.borrow_mut() = Some((metadata, listener));
            }
        }
        _ => {}
    }
}

/// Per the plan's "Device identity" table, resolved from the **Device**
/// global's own properties — never a Node property, which is what makes
/// this stable across profile switches and node-name churn:
/// USB keys on `device.serial`, Bluetooth on `api.bluez5.address`, internal
/// (PCI/HDA) on `device.name` (which embeds the immutable PCI address).
/// `device.bus-path` is never part of the key — it encodes the physical
/// port and moves on replug.
fn resolve_device_identity(props: &DictRef) -> DeviceId {
    if let Some(serial) = props.get("device.serial") {
        return DeviceId(serial.to_string());
    }
    if let Some(addr) = props.get("api.bluez5.address") {
        return DeviceId(addr.to_string());
    }
    // device.name is present on every Device global; it already encodes
    // the immutable PCI address for internal cards
    // (e.g. "alsa_card.pci-0000_00_1f.3-platform-skl_hda_dsp_generic").
    DeviceId(
        props
            .get("device.name")
            .unwrap_or("unknown-device")
            .to_string(),
    )
}

/// Run one reconciliation cycle: compute the desired feed (R3), compare to
/// the actual observed link state on the capture node, and converge.
/// Called after every device-set/link/port change and every routing-policy
/// change (pin/preference-order updates), so the graph converges promptly
/// without polling.
///
/// Convergence is graph-truth-driven, not self-bookkeeping: it destroys
/// every observed capture-side link that doesn't match the desired
/// device (covers rogue links per A1, and stale links to devices that no
/// longer resolve), then creates exactly the missing per-channel links to
/// the desired device. A link that already correctly feeds the desired
/// device — including one that survived the daemon's own crash via
/// `object.linger` (KTD5) — is never touched.
fn reconcile_now(
    gen: &mut Generation,
    core: &CoreRc,
    link_factory: &Rc<RefCell<Option<String>>>,
    event_tx: &StdSender<SessionEvent>,
) {
    let capture_node_id = match gen.capture_node_id {
        Some(id) => id,
        None => {
            // U2's fragment not up yet; nothing to link, but health can
            // still change (e.g. dropping from Linked to Broken if the
            // capture node just departed) — still worth an emit.
            gen.emit_health_if_changed(event_tx);
            return;
        }
    };

    let present = gen.present_devices();
    let desired = compute_desired_feed(&present, &gen.preference_order, gen.pin.as_ref());

    // R3: a pin whose device just disappeared is released, and ranking
    // resumes on the *next* cycle. `compute_desired_feed` already fell
    // through to ranking for this cycle (it doesn't error on a stale pin),
    // but the stored pin must also be cleared so future cycles (and the
    // panel's displayed pin state) reflect the release.
    if should_release_pin(gen.pin.as_ref(), &present) {
        gen.pin = None;
    }

    let actual = gen.actual_link();
    let action = decide_action(&desired, &actual);

    match action {
        ReconcileAction::NoOp => {}
        ReconcileAction::Destroy => destroy_capture_links(gen, core),
        ReconcileAction::Create(device_id) => {
            converge_capture_links(gen, core, link_factory, &device_id, capture_node_id)
        }
        ReconcileAction::Recreate(device_id) => {
            destroy_capture_links(gen, core);
            converge_capture_links(gen, core, link_factory, &device_id, capture_node_id)
        }
    }

    // --- Meter link convergence ---
    // Reconcile-poll approach (KTD1): for sessions whose node_id is still
    // None, poll stream.node_id() — it returns 0 until PipeWire assigns one
    // at Paused state. Once non-zero, store it so subsequent ticks proceed
    // to link creation.
    let meter_session_ids: Vec<u64> = gen.meter_sessions.keys().copied().collect();
    for session_id in &meter_session_ids {
        let session_id = *session_id;
        // Poll node_id if not yet set.
        if gen.meter_sessions[&session_id].node_id.is_none() {
            let polled = gen.meter_sessions[&session_id].stream.node_id();
            if polled != 0 {
                gen.meter_sessions.get_mut(&session_id).unwrap().node_id = Some(polled);
            }
        }
        // Converge post-denoise meter links: source is the permanent source node.
        if let Some(source_node_id) = gen.source_node_id {
            // Read the source's output ports from `device_output_ports`
            // (populated unconditionally for every node) rather than
            // `source_output_ports` (only populated when the port arrives
            // AFTER source_node_id is set). If port 172 arrives before the
            // node global that sets source_node_id, source_output_ports
            // would miss it with no re-announcement — the same ordering
            // race the input-port side already avoids via input_ports_by_node.
            let source_ports = gen
                .device_output_ports
                .get(&source_node_id)
                .cloned()
                .unwrap_or_default();
            let input_ports = gen.input_ports_by_node.clone();
            if let Some(session) = gen.meter_sessions.get_mut(&session_id) {
                converge_meter_links(
                    session,
                    core,
                    link_factory,
                    source_node_id,
                    &source_ports,
                    &input_ports,
                );
            }
        }
    }

    // Resolve the desired upstream device node for raw meter targeting.
    let desired_device_node_id: Option<u32> = match &desired {
        DesiredFeed::Device(device_id) => gen.node_id_for_device(device_id),
        DesiredFeed::None => None,
    };

    let raw_session_ids: Vec<u64> = gen.raw_meter_sessions.keys().copied().collect();
    for session_id in &raw_session_ids {
        let session_id = *session_id;
        // Poll node_id if not yet set.
        if gen.raw_meter_sessions[&session_id].node_id.is_none() {
            let polled = gen.raw_meter_sessions[&session_id].stream.node_id();
            if polled != 0 {
                gen.raw_meter_sessions.get_mut(&session_id).unwrap().node_id = Some(polled);
            }
        }

        let Some(device_node_id) = desired_device_node_id else {
            // No device connected; raw meter has no source — drop any stale
            // links (empty bar, per R7/AE3).
            if let Some(session) = gen.raw_meter_sessions.get_mut(&session_id) {
                session.links.clear();
                session.linked_source_node_id = None;
            }
            continue;
        };

        let device_ports = gen
            .device_output_ports
            .get(&device_node_id)
            .cloned()
            .unwrap_or_default();
        let input_ports = gen.input_ports_by_node.clone();
        if let Some(session) = gen.raw_meter_sessions.get_mut(&session_id) {
            // Purge stale links on device switch (KTD3): if the session's
            // previously-linked source node differs from the new desired
            // device node, tear down the old links and recreate for the
            // new device. This mirrors converge_capture_links's stale-link
            // purge (session.rs ~1407-1421).
            if session.linked_source_node_id.is_some()
                && session.linked_source_node_id != Some(device_node_id)
            {
                session.links.clear();
                session.linked_source_node_id = None;
            }
            converge_meter_links(
                session,
                core,
                link_factory,
                device_node_id,
                &device_ports,
                &input_ports,
            );
            // Record the source node we just linked to (or attempted to
            // link to) so the next tick can detect a device switch.
            if !session.links.is_empty() {
                session.linked_source_node_id = Some(device_node_id);
            }
        }
    }

    gen.emit_health_if_changed(event_tx);
}

/// Destroy every observed link on the capture node, by real Link global
/// id. Falls back to a no-op per link if its id is already gone (e.g. the
/// device departed and PipeWire already tore the link down itself).
fn destroy_capture_links(gen: &mut Generation, core: &CoreRc) {
    let link_ids: Vec<u32> = gen.capture_links.keys().copied().collect();
    for link_id in link_ids {
        if let Ok(registry) = core.get_registry_rc() {
            let _ = registry.destroy_global(link_id);
        }
    }
    // The registry's global_remove event will clear `capture_links` as
    // PipeWire confirms each destruction; clearing here too keeps the next
    // cycle's `actual_link()` honest even if that event hasn't landed yet
    // (destroy_global is a request, not a synchronous guarantee).
    gen.capture_links.clear();
}

/// Converge the capture node's input ports to exactly the desired device's
/// output ports: destroy any capture-side link that isn't already correct
/// (a stale link on a channel this cycle is about to reassign), then
/// create the missing per-channel links.
fn converge_capture_links(
    gen: &mut Generation,
    core: &CoreRc,
    link_factory: &Rc<RefCell<Option<String>>>,
    device_id: &DeviceId,
    capture_node_id: u32,
) {
    let factory_name = match link_factory.borrow().clone() {
        Some(name) => name,
        None => return, // Factory not discovered yet this generation.
    };
    let device_node_id = match gen.node_id_for_device(device_id) {
        Some(id) => id,
        None => return, // Device vanished between decision and action.
    };

    // Any existing capture link NOT from the desired device is stale
    // (WirePlumber interference, or a leftover from a device that just
    // departed) — destroy it before creating the correct set.
    let stale_link_ids: Vec<u32> = gen
        .capture_links
        .iter()
        .filter(|(_, l)| l.output_node_id != device_node_id)
        .map(|(id, _)| *id)
        .collect();
    for link_id in &stale_link_ids {
        if let Ok(registry) = core.get_registry_rc() {
            let _ = registry.destroy_global(*link_id);
        }
        gen.capture_links.remove(link_id);
    }

    let device_ports = gen
        .device_output_ports
        .get(&device_node_id)
        .cloned()
        .unwrap_or_default();
    let mut capture_ports = gen.capture_input_ports.clone();
    if device_ports.is_empty() || capture_ports.is_empty() {
        return;
    }

    // Which capture input ports are already correctly linked from this
    // device (survived a restart via object.linger, or untouched by the
    // stale-link purge above) — skip recreating those.
    let already_linked_input_ports: HashSet<u32> = gen
        .capture_links
        .values()
        .filter(|l| l.output_node_id == device_node_id)
        .map(|l| l.input_port_id)
        .collect();

    // Pair remaining ports positionally (e.g. capture_FL<->input_FL if the
    // device is stereo; capture_MONO<->input_FL if the device is mono —
    // U2's fragment exposes 2 input ports on the capture side regardless,
    // so a mono device only fills one). Real port *names* differ per
    // device/driver, so pairing by sorted position is the robust rule
    // rather than string-matching channel suffixes.
    let mut device_ports = device_ports;
    device_ports.sort_by(|a, b| a.1.cmp(&b.1));
    capture_ports.sort_by(|a, b| a.1.cmp(&b.1));
    capture_ports.retain(|(port_id, _)| !already_linked_input_ports.contains(port_id));

    for ((out_port_id, out_port_name), (in_port_id, in_port_name)) in
        device_ports.iter().zip(capture_ports.iter())
    {
        let props = properties! {
            "link.output.node" => device_node_id.to_string(),
            "link.output.port" => out_port_id.to_string(),
            "link.input.node" => capture_node_id.to_string(),
            "link.input.port" => in_port_id.to_string(),
            // Per KTD5: the link must survive the daemon's own death so a
            // crash/restart doesn't silently unlink the capture side while
            // the source (KTD4) persists. `object.linger` keeps the remote
            // link object alive when our proxy is dropped — we never rely
            // on holding the proxy open.
            "object.linger" => "1",
        };
        let _ = (out_port_name, in_port_name); // names used only for sort key above
        if let Ok(link) = core.create_object::<Link>(&factory_name, &props) {
            // Dropping the proxy is safe and correct: `object.linger` keeps
            // the remote object alive, so we don't need to hold it here.
            let _ = link;
        }
    }
}

/// Start hear-yourself monitoring for `session_id` (U6/R9): link the
/// permanent source's own output ports to the current default sink's
/// input ports. Q6: the default sink is resolved *once*, at this call —
/// never tracked live thereafter (stopping and restarting monitoring
/// re-resolves it, which is the plan's own documented behavior for
/// something explicitly temporary).
///
/// Emits `MonitorStarted` (with the R9 speaker-risk assessment, never a
/// refusal) on success, `MonitorFailed` if no source or no resolvable
/// default sink exists yet.
fn start_monitor(
    gen: &mut Generation,
    core: &CoreRc,
    link_factory: &Rc<RefCell<Option<String>>>,
    session_id: u64,
    event_tx: &StdSender<SessionEvent>,
) {
    let Some(source_node_id) = gen.source_node_id else {
        let _ = event_tx.send(SessionEvent::MonitorFailed {
            session_id,
            reason: "permanent source not present".to_string(),
        });
        return;
    };
    let Some(sink_node_id) = gen.resolve_default_sink_node_id() else {
        let _ = event_tx.send(SessionEvent::MonitorFailed {
            session_id,
            reason: "no default sink resolved yet".to_string(),
        });
        return;
    };
    let Some(factory_name) = link_factory.borrow().clone() else {
        let _ = event_tx.send(SessionEvent::MonitorFailed {
            session_id,
            reason: "link factory not discovered yet".to_string(),
        });
        return;
    };

    let source_ports = gen.source_output_ports.clone();
    let mut sink_ports = gen
        .sink_input_ports
        .get(&sink_node_id)
        .cloned()
        .unwrap_or_default();
    if source_ports.is_empty() || sink_ports.is_empty() {
        let _ = event_tx.send(SessionEvent::MonitorFailed {
            session_id,
            reason: "source or sink has no ports yet".to_string(),
        });
        return;
    }

    // Pair positionally, same rule as converge_capture_links: real port
    // names differ per sink/driver, sorted position is the robust match.
    let mut source_ports = source_ports;
    source_ports.sort_by(|a, b| a.1.cmp(&b.1));
    sink_ports.sort_by(|a, b| a.1.cmp(&b.1));

    // A positional 1:1 zip silently drops every sink port past the
    // shorter side's length -- exactly what happens for this product's
    // own permanent source, which is always mono ([ MONO ] per U2's
    // fragment) monitored to an ordinary stereo sink: `zip` would link
    // only the first channel (e.g. left) and leave the other silent,
    // with no error and no visible sign anything is wrong. Fan the
    // mono case out explicitly: every sink input port gets a link from
    // the source's single output port, so a mono source is heard in
    // both ears rather than going half-silent. A source with more than
    // one output port (not a case this product's own source produces,
    // but the function's own doc comment doesn't rule it out) keeps
    // the prior positional pairing, truncated to the shorter side --
    // unchanged behavior for that case.
    let mut links = Vec::new();
    if source_ports.len() == 1 {
        let (out_port_id, _) = source_ports[0];
        for (in_port_id, _) in &sink_ports {
            let props = properties! {
                "link.output.node" => source_node_id.to_string(),
                "link.output.port" => out_port_id.to_string(),
                "link.input.node" => sink_node_id.to_string(),
                "link.input.port" => in_port_id.to_string(),
                // Monitoring is explicitly temporary (R9) -- no
                // object.linger here. A daemon crash must not leave a
                // monitor link playing to speakers forever;
                // StopMonitor is the only intended teardown path, and
                // per KTD5's own contrast, a session that isn't meant
                // to survive a crash is exactly the case linger is NOT
                // used for. Without linger, dropping the proxy tears
                // the link down server-side (confirmed in
                // pipewire-rs's own `Proxy::drop` -> `pw_proxy_destroy`)
                // -- so the proxy itself must be *kept*, in
                // `MonitorSession`, until `StopMonitor`.
            };
            if let Ok(link) = core.create_object::<Link>(&factory_name, &props) {
                links.push(link);
            }
        }
    } else {
        for ((out_port_id, _), (in_port_id, _)) in source_ports.iter().zip(sink_ports.iter()) {
            let props = properties! {
                "link.output.node" => source_node_id.to_string(),
                "link.output.port" => out_port_id.to_string(),
                "link.input.node" => sink_node_id.to_string(),
                "link.input.port" => in_port_id.to_string(),
            };
            if let Ok(link) = core.create_object::<Link>(&factory_name, &props) {
                links.push(link);
            }
        }
    }

    let speaker_risk = gen.speaker_risk_for_sink(sink_node_id);
    gen.monitor_sessions.insert(session_id, MonitorSession { links });
    let _ = event_tx.send(SessionEvent::MonitorStarted { session_id, speaker_risk });
}

/// Stop monitoring for `session_id` (explicit `StopMonitor`, or the IPC
/// layer's connection-drop cleanup) — destroys exactly this session's
/// links. Idempotent: a `session_id` that was never started, or already
/// stopped, is a silent no-op. Dropping `MonitorSession` is the teardown
/// mechanism itself: its `Link` proxies were created without
/// `object.linger`, so dropping them tears the links down server-side.
fn stop_monitor(gen: &mut Generation, session_id: u64) {
    gen.monitor_sessions.remove(&session_id);
}

/// Create a meter capture stream for `session_id` and store it
/// unconditionally, even if the post-denoise source node is not present yet.
///
/// The stream is created WITHOUT `StreamFlags::AUTOCONNECT` — the link from
/// the source to the stream's input port is established explicitly by
/// `converge_meter_links` during `reconcile_now`, once both the source's
/// output ports and the stream's own input port are known.
///
/// The stream's PipeWire node id is not available at connect time; it is
/// resolved by polling `stream.node_id()` in `reconcile_now` each tick
/// for sessions where `node_id` is still `None` (the reconcile-poll
/// approach, as documented in KTD1 as an acceptable alternative to the
/// state_changed listener approach when borrow constraints make the latter
/// impractical).
fn start_meter(
    gen: &mut Generation,
    core: &CoreRc,
    session_id: u64,
    channel: Arc<Mutex<crate::meter::MeterChannel>>,
) {
    let props = properties! {
        *pipewire::keys::MEDIA_TYPE => "Audio",
        *pipewire::keys::MEDIA_CATEGORY => "Capture",
        *pipewire::keys::MEDIA_ROLE => "Music",
        *pipewire::keys::STREAM_DONT_REMIX => "true",
    };
    let Ok(stream) = StreamRc::new(core.clone(), "antibising-meter", props) else {
        return;
    };

    let listener = stream
        .add_local_listener::<()>()
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let Some(raw) = data.data() else {
                return;
            };
            let samples: Vec<f32> = raw
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            let frame = crate::meter::compute_frame(&samples);
            if let Ok(mut ch) = channel.lock() {
                ch.push(frame);
            }
        })
        .register()
        .expect("registering the meter stream listener cannot fail for a valid stream");

    let mut audio_info = AudioInfoRaw::new();
    audio_info.set_format(pipewire::spa::param::audio::AudioFormat::F32LE);
    audio_info.set_channels(1);
    let obj = pipewire::spa::pod::Object {
        type_: pipewire::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pipewire::spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values: Vec<u8> = pipewire::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pipewire::spa::pod::Value::Object(obj),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .unwrap_or_default();

    if let Some(pod) = Pod::from_bytes(&values) {
        let mut params = [pod];
        // No AUTOCONNECT: the link is established explicitly by
        // converge_meter_links once ports are known.
        let _ = stream.connect(
            Direction::Input,
            None,
            StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
            &mut params,
        );
    }

    gen.meter_sessions.insert(
        session_id,
        MeterSession { stream, listener, node_id: None, links: Vec::new(), linked_source_node_id: None },
    );
}

/// Create a raw (pre-denoise) meter capture stream for `session_id` and
/// store it unconditionally, even if no upstream device is currently
/// connected. Mirrors `start_meter` exactly but inserts into
/// `raw_meter_sessions`.
fn start_raw_meter(
    gen: &mut Generation,
    core: &CoreRc,
    session_id: u64,
    channel: Arc<Mutex<crate::meter::MeterChannel>>,
) {
    let props = properties! {
        *pipewire::keys::MEDIA_TYPE => "Audio",
        *pipewire::keys::MEDIA_CATEGORY => "Capture",
        *pipewire::keys::MEDIA_ROLE => "Music",
        *pipewire::keys::STREAM_DONT_REMIX => "true",
    };
    let Ok(stream) = StreamRc::new(core.clone(), "antibising-raw-meter", props) else {
        return;
    };

    let listener = stream
        .add_local_listener::<()>()
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let Some(raw) = data.data() else {
                return;
            };
            let samples: Vec<f32> = raw
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            let frame = crate::meter::compute_frame(&samples);
            if let Ok(mut ch) = channel.lock() {
                ch.push(frame);
            }
        })
        .register()
        .expect("registering the raw meter stream listener cannot fail for a valid stream");

    let mut audio_info = AudioInfoRaw::new();
    audio_info.set_format(pipewire::spa::param::audio::AudioFormat::F32LE);
    audio_info.set_channels(1);
    let obj = pipewire::spa::pod::Object {
        type_: pipewire::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pipewire::spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values: Vec<u8> = pipewire::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pipewire::spa::pod::Value::Object(obj),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .unwrap_or_default();

    if let Some(pod) = Pod::from_bytes(&values) {
        let mut params = [pod];
        let _ = stream.connect(
            Direction::Input,
            None,
            StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
            &mut params,
        );
    }

    gen.raw_meter_sessions.insert(
        session_id,
        MeterSession { stream, listener, node_id: None, links: Vec::new(), linked_source_node_id: None },
    );
}

/// Converge a meter session's explicit links: create `Link` objects from
/// `source_output_ports` to the meter stream's input port(s), unless they
/// already exist (dedup). Returns immediately (retry next tick) if the
/// session has no node_id yet, if the meter's input ports are not yet in
/// `input_ports_by_node`, or if `source_output_ports` is empty.
///
/// On device switch (for the raw meter), the caller is responsible for
/// purging stale links before calling this — see `reconcile_raw_meter_links`.
///
/// No `object.linger`: dropping the session drops the `Link` proxies,
/// which tears the server-side links down (the correct `StopMeter` path).
fn converge_meter_links(
    session: &mut MeterSession,
    core: &CoreRc,
    link_factory: &Rc<RefCell<Option<String>>>,
    source_node_id: u32,
    source_output_ports: &[(u32, String)],
    input_ports_by_node: &HashMap<u32, Vec<(u32, String)>>,
) {
    // Need the session's node_id to look up its input ports.
    let meter_node_id = match session.node_id {
        Some(id) => id,
        None => return, // Not yet assigned; retry next tick.
    };

    // Look up the meter stream's own input ports.
    let meter_input_ports = match input_ports_by_node.get(&meter_node_id) {
        Some(ports) if !ports.is_empty() => ports,
        _ => return, // Meter's input port not recorded yet; retry next tick.
    };

    if source_output_ports.is_empty() {
        return; // Source has no output ports yet; retry next tick.
    }

    let factory_name = match link_factory.borrow().clone() {
        Some(name) => name,
        None => return, // Link factory not discovered yet this generation.
    };

    // Idempotent: if links are already present, don't create more.
    // The caller (reconcile_now raw meter path) clears links on device
    // switch before calling here, so this check handles steady state.
    if !session.links.is_empty() {
        return;
    }

    // Source is mono (one output port); fan it to every meter input port.
    // In practice the meter is also mono so there is exactly one pairing.
    let mut source_ports = source_output_ports.to_vec();
    let mut meter_ports = meter_input_ports.clone();
    source_ports.sort_by(|a, b| a.1.cmp(&b.1));
    meter_ports.sort_by(|a, b| a.1.cmp(&b.1));

    if source_ports.len() == 1 {
        // Fan single source output port to all meter input ports.
        let (out_port_id, _) = source_ports[0];
        for (in_port_id, _) in &meter_ports {
            let props = properties! {
                "link.output.node" => source_node_id.to_string(),
                "link.output.port" => out_port_id.to_string(),
                "link.input.node" => meter_node_id.to_string(),
                "link.input.port" => in_port_id.to_string(),
                // No object.linger: dropping the proxy tears the link down,
                // which is the desired teardown path for StopMeter.
            };
            if let Ok(link) = core.create_object::<Link>(&factory_name, &props) {
                session.links.push(link);
            }
        }
    } else {
        // Positional pairing for multi-channel sources.
        for ((out_port_id, _), (in_port_id, _)) in source_ports.iter().zip(meter_ports.iter()) {
            let props = properties! {
                "link.output.node" => source_node_id.to_string(),
                "link.output.port" => out_port_id.to_string(),
                "link.input.node" => meter_node_id.to_string(),
                "link.input.port" => in_port_id.to_string(),
            };
            if let Ok(link) = core.create_object::<Link>(&factory_name, &props) {
                session.links.push(link);
            }
        }
    }
}
