//! `antibisingd` — the daemon that hosts the engine (KTD1, KTD7). Three
//! subcommands:
//!
//! - `install`   — one-shot bootstrap (U3): writes the daemon's own unit,
//!                 the permanent-source fragment, and the pipewire.service
//!                 drop-in, then enables + starts both systemd units.
//!                 User-initiated only; the running service never installs.
//! - `uninstall` — reverses `install`: stops + disables both units,
//!                 removes all three files.
//! - `run`       — what systemd's `ExecStart=` actually invokes. Assumes
//!                 installation already happened (by definition, since
//!                 something started this unit) and runs Q4 startup
//!                 reconciliation before hosting the engine session.
//!
//! U3's scope stops at making startup safe in the four Q4 states and
//! shutting down cleanly on SIGTERM. The IPC socket (U10) and the fuller
//! adopt/repair lifecycle across a live PipeWire crash (U9) are separate
//! units — `run` here hosts the engine and keeps routing alive, which is
//! already enough for R1/R3 to hold with no UI process attached.

use engine::{classify_startup, install, is_unit_active, uninstall, InstallPaths, StartupAction};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const FILTER_CHAIN_UNIT: &str = "filter-chain.service";

fn main() {
    tracing_subscriber::fmt::init();

    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("install") => cmd_install(),
        Some("uninstall") => cmd_uninstall(),
        Some("run") => cmd_run(),
        other => {
            eprintln!("usage: antibisingd <install|uninstall|run>");
            if let Some(cmd) = other {
                eprintln!("unknown subcommand: {cmd}");
            }
            std::process::exit(2);
        }
    }
}

fn cmd_install() {
    let paths = InstallPaths::production().unwrap_or_else(|e| {
        eprintln!("error: could not resolve install paths: {e}");
        std::process::exit(1);
    });
    match install(&paths) {
        Ok(()) => {
            println!("antibising installed: source and daemon units enabled and started.");
        }
        Err(e) => {
            eprintln!("error: install failed: {e}");
            std::process::exit(1);
        }
    }
}

fn cmd_uninstall() {
    let paths = InstallPaths::production().unwrap_or_else(|e| {
        eprintln!("error: could not resolve install paths: {e}");
        std::process::exit(1);
    });
    match uninstall(&paths) {
        Ok(()) => {
            println!("antibising uninstalled: units stopped, disabled, and files removed.");
        }
        Err(e) => {
            eprintln!("error: uninstall failed: {e}");
            std::process::exit(1);
        }
    }
}

/// What systemd's `ExecStart=` invokes. Runs Q4 startup reconciliation,
/// then hosts the engine session thread until SIGTERM, at which point it
/// shuts the session down cleanly and exits 0.
///
/// **Default-ranking bootstrap:** U5 hasn't landed yet, so there is no
/// persisted preference order to load. Without *something* in
/// `preference_order`, `compute_desired_feed` (R3) never picks any
/// device — a present-but-unranked device is deliberately never chosen
/// — and the daemon would sit `SilentNoDevice` forever even with a mic
/// plugged in. Until U5's config exists, every device this daemon
/// observes (via the initial `Snapshot` or a later `DeviceArrived`) is
/// appended to a locally-tracked order and pushed with
/// `SetPreferenceOrder`, so install always reaches `Linked` whenever at
/// least one recognized device is present — never merely "source node
/// exists." U5 replaces this with the daemon's own persisted ranking;
/// this is a deterministic stopgap, not a design decision about final
/// ranking policy.
fn cmd_run() {
    reconcile_startup();

    let mut session = engine::Session::spawn(engine::SOURCE_NAME.to_string());

    let shutdown = Arc::new(AtomicBool::new(false));
    if let Err(e) = signal_hook::flag::register(signal_hook::consts::SIGTERM, shutdown.clone()) {
        tracing::warn!("failed to register SIGTERM handler: {e}");
    }
    if let Err(e) = signal_hook::flag::register(signal_hook::consts::SIGINT, shutdown.clone()) {
        tracing::warn!("failed to register SIGINT handler: {e}");
    }

    tracing::info!("antibisingd running");
    let mut known_devices: Vec<engine::DeviceId> = Vec::new();
    loop {
        if shutdown.load(Ordering::Relaxed) {
            tracing::info!("shutdown requested, stopping session thread");
            session.shutdown();
            break;
        }
        match session.try_recv_event() {
            Some(event) => {
                tracing::debug!(?event, "session event");
                bootstrap_default_ranking(&session, &mut known_devices, &event);
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// See `cmd_run`'s doc comment. Thin I/O wrapper around the pure
/// [`update_known_devices`] — sends `SetPreferenceOrder` only when that
/// function says a resend is needed.
fn bootstrap_default_ranking(
    session: &engine::Session,
    known_devices: &mut Vec<engine::DeviceId>,
    event: &engine::SessionEvent,
) {
    if let Some(order) = update_known_devices(known_devices, event) {
        let _ = session.send(engine::SessionCommand::SetPreferenceOrder(order));
    }
}

/// Pure core of the default-ranking bootstrap: updates `known_devices`
/// per `event`, returning `Some(order)` to resend or `None` if nothing
/// on the wire needs to change. Kept event-in/state-out so it's
/// unit-testable without a live `Session`.
///
/// **Why `Disconnected` clears `known_devices`:** `Snapshot` fires once
/// per PipeWire *generation* (session.rs's `Generation::new()` on every
/// initial connect and every reconnect after a crash/restart), and each
/// fresh generation's `preference_order` starts empty regardless of what
/// this daemon process previously sent — the engine's per-generation
/// state is never carried across a reconnect (session.rs: "recovery is
/// a clean rebuild, not an attempt to reconcile stale IDs with new
/// ones"). Without clearing on `Disconnected`, the post-reconnect
/// `Snapshot` would report the *same* devices this process already
/// knows about, `added` would stay `false`, and `SetPreferenceOrder`
/// would never be resent to the new (empty-preference-order)
/// generation — silently breaking R3 routing after every PipeWire
/// restart, exactly the SIGKILL-recovery scenario R2 exists to cover.
/// Clearing here means the next `Snapshot` sees every device as newly
/// known again and resends unconditionally.
///
/// The `Snapshot` devices are sorted by `DeviceId` before appending —
/// the engine's internal device map is a `HashMap`, so `Snapshot`'s
/// vector order is not stable across runs; sorting first is what makes
/// "deterministic across a daemon run" true rather than aspirational.
fn update_known_devices(
    known_devices: &mut Vec<engine::DeviceId>,
    event: &engine::SessionEvent,
) -> Option<Vec<engine::DeviceId>> {
    match event {
        engine::SessionEvent::Disconnected => {
            known_devices.clear();
            None
        }
        engine::SessionEvent::Snapshot(devices) => {
            let mut sorted_ids: Vec<&engine::DeviceId> = devices.iter().map(|d| &d.id).collect();
            sorted_ids.sort();
            let mut added = false;
            for id in sorted_ids {
                if !known_devices.contains(id) {
                    known_devices.push(id.clone());
                    added = true;
                }
            }
            added.then(|| known_devices.clone())
        }
        engine::SessionEvent::DeviceArrived(info) if !known_devices.contains(&info.id) => {
            known_devices.push(info.id.clone());
            Some(known_devices.clone())
        }
        _ => None,
    }
}

/// Q4 reconciliation: decide what to do about `filter-chain.service`
/// based on its reported unit state and whether the permanent source
/// node is actually present, per [`classify_startup`]. **Before**
/// dispatching on that classification, the fragment file itself is
/// inspected and repaired if missing — a user deleting it while the
/// unit is stopped is exactly the R7 "artifacts missing" case, and
/// starting `filter-chain.service` against a nonexistent conf.d entry
/// would silently produce no source at all. Regeneration (either here
/// or in the `RegenerateAndRestart` branch below, which covers the
/// unit-still-active-but-fragment-corrupted case) writes the fragment
/// from the daemon's default config — U5's richer config-driven
/// fragment lands later; U3 only needs the bypass graph to exist — and
/// retries the unit start exactly once, never looping.
fn reconcile_startup() {
    let paths = match InstallPaths::production() {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("could not resolve install paths for startup reconciliation: {e}");
            return;
        }
    };

    if !paths.fragment_path.exists() {
        tracing::warn!(
            "startup: fragment missing at {}; regenerating from default config (R7)",
            paths.fragment_path.display()
        );
        let content = engine::render_fragment(&engine::FragmentConfig::default());
        if let Err(e) = engine::write_atomic(&paths.fragment_path, &content) {
            tracing::error!("failed to regenerate missing fragment: {e}");
            return;
        }
    }

    let unit_active = is_unit_active(FILTER_CHAIN_UNIT);
    let source_present = source_node_present(engine::SOURCE_NAME);
    let action = classify_startup(unit_active, source_present);

    match action {
        StartupAction::Adopt => {
            tracing::info!("startup: adopting existing source, no action needed");
        }
        StartupAction::StartFilterChain => {
            tracing::info!("startup: starting {FILTER_CHAIN_UNIT} (cold start)");
            start_filter_chain_and_wait();
        }
        StartupAction::RegenerateAndRestart => {
            tracing::warn!(
                "startup: {FILTER_CHAIN_UNIT} reports active but source is missing; \
                 regenerating fragment from config and restarting once"
            );
            let content = engine::render_fragment(&engine::FragmentConfig::default());
            if let Err(e) = engine::write_atomic(&paths.fragment_path, &content) {
                tracing::error!("failed to regenerate fragment: {e}");
                return;
            }
            restart_filter_chain_and_wait();
        }
    }
}

/// True only if a source's **name column** (the second tab-separated
/// field of `pactl list short sources`) is exactly `source_name` —
/// substring matching would also match `antibising_mic_capture` or an
/// unrelated `antibising_mic_backup`-style node, misclassifying a
/// stale/different node as our own permanent source.
fn source_node_present(source_name: &str) -> bool {
    let out = std::process::Command::new("pactl")
        .args(["list", "short", "sources"])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .any(|line| line.split('\t').nth(1) == Some(source_name)),
        Err(_) => false,
    }
}

fn start_filter_chain_and_wait() {
    let status = std::process::Command::new("systemctl")
        .args(["--user", "start", FILTER_CHAIN_UNIT])
        .status();
    match status {
        Ok(s) if s.success() => wait_for_source(engine::SOURCE_NAME, Duration::from_secs(10)),
        Ok(s) => tracing::error!("systemctl start {FILTER_CHAIN_UNIT} exited with {s}"),
        Err(e) => tracing::error!("failed to run systemctl start {FILTER_CHAIN_UNIT}: {e}"),
    }
}

fn restart_filter_chain_and_wait() {
    let status = std::process::Command::new("systemctl")
        .args(["--user", "restart", FILTER_CHAIN_UNIT])
        .status();
    match status {
        Ok(s) if s.success() => {
            if !wait_for_source_bool(engine::SOURCE_NAME, Duration::from_secs(10)) {
                tracing::error!(
                    "filter-chain failed to start: source did not appear after regenerating \
                     the fragment and restarting once"
                );
            }
        }
        Ok(s) => tracing::error!("systemctl restart {FILTER_CHAIN_UNIT} exited with {s}"),
        Err(e) => tracing::error!("failed to run systemctl restart {FILTER_CHAIN_UNIT}: {e}"),
    }
}

fn wait_for_source(name: &str, timeout: Duration) {
    if !wait_for_source_bool(name, timeout) {
        tracing::error!("source '{name}' did not appear within {timeout:?} of starting the unit");
    }
}

fn wait_for_source_bool(name: &str, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if source_node_present(name) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str) -> engine::DeviceId {
        engine::DeviceId(id.to_string())
    }

    fn device_info(id: &str) -> engine::DeviceInfo {
        engine::DeviceInfo {
            id: dev(id),
            node_id: 0,
            description: id.to_string(),
        }
    }

    #[test]
    fn snapshot_populates_and_resends_sorted_order() {
        let mut known = Vec::new();
        let order = update_known_devices(
            &mut known,
            &engine::SessionEvent::Snapshot(vec![device_info("b"), device_info("a")]),
        );
        assert_eq!(order, Some(vec![dev("a"), dev("b")]));
        assert_eq!(known, vec![dev("a"), dev("b")]);
    }

    #[test]
    fn snapshot_with_no_new_devices_does_not_resend() {
        let mut known = vec![dev("a")];
        let order = update_known_devices(
            &mut known,
            &engine::SessionEvent::Snapshot(vec![device_info("a")]),
        );
        assert_eq!(order, None, "no new device -> no resend needed");
    }

    #[test]
    fn device_arrived_of_unknown_device_appends_and_resends() {
        let mut known = vec![dev("a")];
        let order = update_known_devices(
            &mut known,
            &engine::SessionEvent::DeviceArrived(device_info("b")),
        );
        assert_eq!(order, Some(vec![dev("a"), dev("b")]));
    }

    #[test]
    fn device_arrived_of_already_known_device_is_noop() {
        // A duplicate DeviceArrived (e.g. a redundant Registry event)
        // for a device already tracked must not resend — the order
        // hasn't actually changed.
        let mut known = vec![dev("a")];
        let order = update_known_devices(
            &mut known,
            &engine::SessionEvent::DeviceArrived(device_info("a")),
        );
        assert_eq!(order, None);
        assert_eq!(known, vec![dev("a")], "known_devices must be unchanged");
    }

    #[test]
    fn disconnected_clears_known_devices() {
        let mut known = vec![dev("a"), dev("b")];
        let order = update_known_devices(&mut known, &engine::SessionEvent::Disconnected);
        assert_eq!(order, None, "Disconnected itself never triggers a resend");
        assert!(known.is_empty(), "known_devices must be cleared on disconnect");
    }

    /// The regression this fix exists to prevent: without clearing on
    /// `Disconnected`, a post-reconnect `Snapshot` reporting the *same*
    /// devices as before would be silently swallowed (no resend),
    /// leaving the new PipeWire generation's `preference_order` empty
    /// forever — breaking R3 routing after every PipeWire crash/restart.
    #[test]
    fn disconnect_clears_then_reconnect_snapshot_with_same_devices_resends() {
        let mut known = Vec::new();

        // First generation: device arrives, ranking sent.
        let first = update_known_devices(
            &mut known,
            &engine::SessionEvent::Snapshot(vec![device_info("razer")]),
        );
        assert_eq!(first, Some(vec![dev("razer")]));

        // PipeWire crashes / restarts.
        let during_disconnect =
            update_known_devices(&mut known, &engine::SessionEvent::Disconnected);
        assert_eq!(during_disconnect, None);
        assert!(known.is_empty());

        // New generation reconnects and reports the *same* device the
        // daemon already saw before. Without the Disconnected-clears
        // fix, this would be a no-op (added == false) and the new
        // generation's preference_order would stay empty forever.
        let after_reconnect = update_known_devices(
            &mut known,
            &engine::SessionEvent::Snapshot(vec![device_info("razer")]),
        );
        assert_eq!(
            after_reconnect,
            Some(vec![dev("razer")]),
            "must resend to the new generation even though this device was already known \
             before the disconnect"
        );
    }

    /// A `DeviceArrived` that lands after `Disconnected` but before the
    /// reconnected generation's own `Snapshot` (a plausible ordering —
    /// individual Registry events can arrive before the sync barrier
    /// that produces `Snapshot`, per session.rs) must still append and
    /// resend correctly against the now-empty `known_devices`, and the
    /// later `Snapshot` for that same generation must not double-resend
    /// once the device is already tracked.
    #[test]
    fn device_arrived_after_disconnect_repopulates_and_later_snapshot_is_noop() {
        let mut known = vec![dev("razer")];

        update_known_devices(&mut known, &engine::SessionEvent::Disconnected);
        assert!(known.is_empty());

        let arrived = update_known_devices(
            &mut known,
            &engine::SessionEvent::DeviceArrived(device_info("razer")),
        );
        assert_eq!(
            arrived,
            Some(vec![dev("razer")]),
            "DeviceArrived after a disconnect must resend against the cleared state"
        );

        // The reconnected generation's own Snapshot arrives afterward,
        // reporting the same device already re-added above — no new
        // device, so no further resend is needed.
        let snapshot = update_known_devices(
            &mut known,
            &engine::SessionEvent::Snapshot(vec![device_info("razer")]),
        );
        assert_eq!(snapshot, None, "device already known from DeviceArrived -> no resend");
    }
}
