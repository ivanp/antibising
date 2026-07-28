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
use pipewire::{
    context::ContextRc,
    core::CoreRc,
    main_loop::MainLoopRc,
    registry::{GlobalObject, RegistryRc},
    spa::utils::dict::DictRef,
    types::ObjectType,
};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::{Receiver as StdReceiver, Sender as StdSender, TryRecvError};
use std::thread::JoinHandle;
use std::time::Duration;

/// Commands sent from outside the session thread (daemon/IPC layer) into it.
pub enum SessionCommand {
    /// Link the filter-chain capture node to the given device's current
    /// `Audio/Source` node. See U4 for the reconciler that decides *which*
    /// device to request.
    LinkCapture { device_node_id: u32 },
    /// Remove the current capture-side link, if any.
    UnlinkCapture,
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
    /// Only populated once both the Node *and* its parent Device global
    /// have arrived — see `try_resolve_pending`.
    devices: HashMap<u32, DeviceInfo>,
    /// Device global id -> its identity key, resolved from `device.serial`
    /// / `api.bluez5.address` / `device.name` (never a node property).
    device_identities: HashMap<u32, DeviceId>,
    /// Candidate `Audio/Source` nodes seen before their parent Device
    /// global arrived. Keyed by node id: (device_id, node_name_or_description).
    /// Registry ordering is not guaranteed, so a Node can precede its
    /// Device — this holds the node until the join resolves.
    pending_nodes: HashMap<u32, PendingNode>,
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

    let _reg_listener = registry
        .add_listener_local()
        .global(move |global: &GlobalObject<&DictRef>| {
            handle_global(global, &name_for_cb, &gen_for_global, &tx_for_global);
        })
        .global_remove(move |id: u32| {
            let mut gen = gen_for_remove.borrow_mut();
            gen.device_departed(id);
            gen.node_departed(id, &tx_for_remove);
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
    let timer = mainloop.loop_().add_timer(move |_expirations| {
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
                Ok(SessionCommand::LinkCapture { .. } | SessionCommand::UnlinkCapture) => {
                    // U4 owns link management; the session thread here only
                    // routes the command — actual link creation happens in
                    // the reconciler built on top of this module.
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
fn handle_global(
    global: &GlobalObject<&DictRef>,
    our_source_name: &str,
    generation: &Rc<RefCell<Generation>>,
    event_tx: &StdSender<SessionEvent>,
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
        ObjectType::Node => {
            let media_class = props.get("media.class").unwrap_or_default();
            let node_name = props.get("node.name").unwrap_or_default();
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
