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
use crate::routing::{compute_desired_feed, should_release_pin};
use pipewire::{
    context::ContextRc,
    core::CoreRc,
    link::Link,
    main_loop::MainLoopRc,
    node::Node,
    properties::properties,
    registry::{GlobalObject, RegistryRc},
    spa::utils::dict::DictRef,
    types::ObjectType,
};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::mpsc::{Receiver as StdReceiver, Sender as StdSender, TryRecvError};
use std::thread::JoinHandle;
use std::time::Duration;

/// Commands sent from outside the session thread (daemon/IPC layer) into it.
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
        }
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
        if self.capture_node_id == Some(node_id) {
            self.capture_node_id = None;
            self.capture_node_proxy = None;
            self.capture_input_ports.clear();
            self.capture_links.clear();
        }
        if let Some(dev) = self.devices.remove(&node_id) {
            let _ = event_tx.send(SessionEvent::DeviceDeparted(dev.id));
        }
    }

    fn device_departed(&mut self, device_global_id: u32) {
        self.device_identities.remove(&device_global_id);
        // Nodes belonging to this device depart via their own
        // global_remove events; nothing to do here beyond forgetting the
        // identity so a stale node can't resolve against it.
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
            );
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
                    reconcile_now(&mut gen, &core_for_timer, &link_factory_for_timer);
                }
                Ok(SessionCommand::SetPin(pin)) => {
                    let mut gen = gen_for_timer.borrow_mut();
                    gen.pin = pin;
                    reconcile_now(&mut gen, &core_for_timer, &link_factory_for_timer);
                }
                Ok(SessionCommand::SetRnnoiseParam(param, value)) => {
                    gen_for_timer.borrow().set_rnnoise_param(param, value);
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
) {
    let props = match global.props {
        Some(p) => p,
        None => return,
    };

    match global.type_ {
        ObjectType::Device => {
            let identity = resolve_device_identity(props);
            generation
                .borrow_mut()
                .device_arrived(global.id, identity, event_tx);
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

            let media_class = props.get("media.class").unwrap_or_default();
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
            if direction == "out" {
                gen.device_output_ports
                    .entry(node_id)
                    .or_default()
                    .push((global.id, port_name));
            } else if direction == "in" && gen.capture_node_id == Some(node_id) {
                gen.capture_input_ports.push((global.id, port_name));
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
) {
    let capture_node_id = match gen.capture_node_id {
        Some(id) => id,
        None => return, // U2's fragment not up yet; nothing to link.
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
