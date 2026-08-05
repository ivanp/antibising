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
//!                 reconciliation, loads the persisted config (U5/KTD6),
//!                 and starts the U10 IPC server before hosting the
//!                 engine session.
//!
//! U3's scope stops at making startup safe in the four Q4 states and
//! shutting down cleanly on SIGTERM. The fuller adopt/repair lifecycle
//! across a live PipeWire crash (U9) is a separate unit — `run` here
//! hosts the engine and keeps routing alive, which is already enough for
//! R1/R3 to hold with no UI process attached.

use antibisingd::ipc::{
    accept_loop, bind_singleton, handle_session_event, BindError, Broadcaster, IpcPaths,
    SessionRegistry, SharedState,
};
use engine::{
    classify_startup, install, is_unit_active, uninstall, Config, InstallPaths, StartupAction,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
/// loads the persisted config (U5/KTD6), applies it to the engine, starts
/// the U10 IPC server, then hosts the engine session thread until
/// SIGTERM, at which point it shuts everything down cleanly and exits 0.
///
/// **Auto-rank fallback, now persisted.** If the loaded config's
/// `preference_order` is empty (first run, no user ranking yet — R3's
/// own rule is that a present-but-unranked device is never chosen, so an
/// empty order would otherwise sit `SilentNoDevice` forever even with a
/// mic plugged in), every device this daemon observes is appended to the
/// config's `preference_order` and the config is saved — so the *next*
/// daemon start already has a real ranking, and this fallback only ever
/// engages once per machine rather than every run. A future U7 panel, or
/// a manual pin, immediately supersedes it (R3: an unpinned
/// ranked-routing mode is a normal, permanent operating state, not
/// solely a bootstrap artifact — this fallback simply seeds it).
fn cmd_run() {
    let startup_broken_reason = reconcile_startup();

    let config_path = Config::default_path();
    let mut config = Config::load(&config_path).unwrap_or_else(|e| {
        tracing::error!(
            "failed to load config at {}: {e}; starting from defaults",
            config_path.display()
        );
        Config::default()
    });

    let mut session = engine::Session::spawn(engine::SOURCE_NAME.to_string());

    if !config.preference_order.is_empty() {
        let ids: Vec<engine::DeviceId> = config
            .preference_order
            .iter()
            .cloned()
            .map(engine::DeviceId)
            .collect();
        let _ = session.send(engine::SessionCommand::SetPreferenceOrder(ids));
    }
    if let Some(pin) = &config.pin {
        let _ = session.send(engine::SessionCommand::SetPin(Some(engine::DeviceId(
            pin.clone(),
        ))));
    }

    let ipc_paths = IpcPaths::production();
    let (_lock_file, listener) = match bind_singleton(&ipc_paths) {
        Ok(pair) => pair,
        Err(BindError::AlreadyRunning) => {
            tracing::error!("{}", BindError::AlreadyRunning);
            std::process::exit(1);
        }
        Err(e) => {
            tracing::error!("failed to bind IPC socket: {e}");
            std::process::exit(1);
        }
    };
    let mut initial_state = SharedState::new(config.clone(), config_path.clone());
    // U9: a startup repair failure (corrupt fragment still broken after
    // the fragment-regenerate + one-restart retry) must be visible in
    // the very first `Snapshot` any IPC client receives -- set it into
    // `SharedState` before `accept_loop` starts serving, rather than
    // leaving new clients to see a misleading `SilentNoDevice` until
    // the engine's own health computation eventually catches up (which
    // it never fully will here: `compute_health`'s own `Broken` path
    // only fires on "capture node not present," a narrower check than
    // "filter-chain.service itself failed to start").
    if let Some(reason) = &startup_broken_reason {
        initial_state.health = engine::HealthStatus::Broken {
            reason: reason.clone(),
        };
    }
    let shared = Arc::new(Mutex::new(initial_state));
    let broadcaster = Broadcaster::new();
    let session_registry = SessionRegistry::new();
    let session_cmd_tx = session.command_sender();
    {
        let shared = shared.clone();
        let broadcaster = broadcaster.clone();
        let session_registry = session_registry.clone();
        std::thread::spawn(move || {
            accept_loop(
                listener,
                shared,
                broadcaster,
                session_registry,
                session_cmd_tx,
            )
        });
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    if let Err(e) = signal_hook::flag::register(signal_hook::consts::SIGTERM, shutdown.clone()) {
        tracing::warn!("failed to register SIGTERM handler: {e}");
    }
    if let Err(e) = signal_hook::flag::register(signal_hook::consts::SIGINT, shutdown.clone()) {
        tracing::warn!("failed to register SIGINT handler: {e}");
    }

    tracing::info!("antibisingd running");
    loop {
        if shutdown.load(Ordering::Relaxed) {
            tracing::info!("shutdown requested, stopping session thread");
            session.shutdown();
            break;
        }
        match session.try_recv_event() {
            Some(event) => {
                tracing::debug!(?event, "session event");
                if config.preference_order.is_empty() {
                    if let Some(order) = update_known_devices(&mut config.preference_order, &event)
                    {
                        let ids: Vec<engine::DeviceId> =
                            order.into_iter().map(engine::DeviceId).collect();
                        let _ = session.send(engine::SessionCommand::SetPreferenceOrder(ids));
                        let mut state = shared.lock().expect("shared state mutex poisoned");
                        state.config.preference_order = config.preference_order.clone();
                        let content = toml::to_string_pretty(&state.config)
                            .expect("Config must always serialize to TOML");
                        let _ = engine::write_atomic(&config_path, &content);
                    }
                }
                handle_session_event(&shared, &broadcaster, &session_registry, event);
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// Pure core of the persisted auto-rank fallback (see `cmd_run`'s doc
/// comment): updates `preference_order` per `event`, returning
/// `Some(order)` to resend/persist or `None` if nothing changed. Kept
/// event-in/state-out so it's unit-testable without a live `Session`.
/// Operates on plain `String` device identities (the config's own wire
/// type) rather than `engine::DeviceId`, so it has zero engine coupling.
///
/// **Why `Disconnected` clears the working order:** `Snapshot` fires once
/// per PipeWire *generation* (session.rs's `Generation::new()` on every
/// initial connect and every reconnect after a crash/restart), and each
/// fresh generation's engine-side `preference_order` starts empty
/// regardless of what this daemon process previously sent — the engine's
/// per-generation state is never carried across a reconnect. Without
/// clearing here, the post-reconnect `Snapshot` would report the *same*
/// devices this process already tracked, `added` would stay `false`, and
/// `SetPreferenceOrder` would never be resent to the new
/// (empty-preference-order) generation — silently breaking R3 routing
/// after every PipeWire restart, exactly the SIGKILL-recovery scenario R2
/// exists to cover. **This function only runs at all while the
/// *persisted* config's order is still empty** (see the `cmd_run`
/// call-site guard) — once a real ranking exists, `cmd_run` sends it once
/// at startup and this fallback never engages again.
///
/// The `Snapshot` devices are sorted by `DeviceId` before appending — the
/// engine's internal device map is a `HashMap`, so `Snapshot`'s vector
/// order is not stable across runs; sorting first is what makes
/// "deterministic across a daemon run" true rather than aspirational.
fn update_known_devices(
    known_devices: &mut Vec<String>,
    event: &engine::SessionEvent,
) -> Option<Vec<String>> {
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
                let id_str = id.0.clone();
                if !known_devices.contains(&id_str) {
                    known_devices.push(id_str);
                    added = true;
                }
            }
            added.then(|| known_devices.clone())
        }
        engine::SessionEvent::DeviceArrived(info) if !known_devices.contains(&info.id.0) => {
            known_devices.push(info.id.0.clone());
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
/// would silently produce no source at all. Regeneration (here, or in
/// either branch of the `match` below) writes the fragment from the
/// daemon's *persisted* config (falling back to
/// `FragmentConfig::default()` if the config itself is unreadable —
/// U3's bypass graph must still come up even when U5's config is
/// corrupt) and retries the unit start exactly once, never looping —
/// this applies whether the first attempt was `StartFilterChain` (cold
/// start; the fragment can still hold corrupt *content* even though it
/// exists) or `RegenerateAndRestart` (unit already active, source
/// missing).
///
/// **Returns `Some(reason)` when repair genuinely failed** (U9: the
/// source still hasn't appeared after a fragment regenerate + one
/// restart) — the caller surfaces this into `SharedState.health` as
/// `Broken` *before* the IPC server starts accepting connections, so
/// the very first `Snapshot` any client receives already carries the
/// honest verdict (R5/R7), rather than a client seeing a misleadingly
/// blank `SilentNoDevice` while the daemon is silently already in a
/// known-broken state. `reason` includes the tail of `journalctl` for
/// `filter-chain.service` — precise enough to act on directly, per R7's
/// "say precisely what is wrong."  Returns `None` on `Adopt` or a
/// successful repair — the daemon proceeds to host the engine exactly
/// as before; a failure here is never retried in a loop, only reported.
fn reconcile_startup() -> Option<String> {
    let paths = match InstallPaths::production() {
        Ok(p) => p,
        Err(e) => {
            let reason = format!("could not resolve install paths for startup reconciliation: {e}");
            tracing::error!("{reason}");
            return Some(reason);
        }
    };

    let fragment_config = Config::load(&Config::default_path())
        .map(|c| c.to_fragment_config())
        .unwrap_or_default();

    if !paths.fragment_path.exists() {
        tracing::warn!(
            "startup: fragment missing at {}; regenerating from config (R7)",
            paths.fragment_path.display()
        );
        let content = engine::render_fragment(&fragment_config);
        if let Err(e) = engine::write_atomic(&paths.fragment_path, &content) {
            let reason = format!("failed to regenerate missing fragment: {e}");
            tracing::error!("{reason}");
            return Some(reason);
        }
    }

    let unit_active = is_unit_active(FILTER_CHAIN_UNIT);
    let source_present = source_node_present(engine::SOURCE_NAME);
    let action = classify_startup(unit_active, source_present);

    match action {
        StartupAction::Adopt => {
            tracing::info!("startup: adopting existing source, no action needed");
            None
        }
        StartupAction::StartFilterChain => {
            tracing::info!("startup: starting {FILTER_CHAIN_UNIT} (cold start)");
            match start_filter_chain_and_wait() {
                None => None,
                Some(first_failure) => {
                    // The fragment file existed (checked above) but its
                    // *contents* may be garbage (hand-edited or
                    // corrupted while the unit was stopped) -- the same
                    // repair this daemon applies when
                    // `RegenerateAndRestart` finds a live-but-empty
                    // unit. Regenerate from config and retry exactly
                    // once; never loop past this single retry (R7).
                    tracing::warn!(
                        "startup: cold start of {FILTER_CHAIN_UNIT} failed ({first_failure}); \
                         regenerating fragment from config and retrying once"
                    );
                    let content = engine::render_fragment(&fragment_config);
                    if let Err(e) = engine::write_atomic(&paths.fragment_path, &content) {
                        let reason =
                            format!("failed to regenerate fragment after cold-start failure: {e}");
                        tracing::error!("{reason}");
                        return Some(reason);
                    }
                    restart_filter_chain_and_wait()
                }
            }
        }
        StartupAction::RegenerateAndRestart => {
            tracing::warn!(
                "startup: {FILTER_CHAIN_UNIT} reports active but source is missing; \
                 regenerating fragment from config and restarting once"
            );
            let content = engine::render_fragment(&fragment_config);
            if let Err(e) = engine::write_atomic(&paths.fragment_path, &content) {
                let reason = format!("failed to regenerate fragment: {e}");
                tracing::error!("{reason}");
                return Some(reason);
            }
            restart_filter_chain_and_wait()
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

/// The tail of `journalctl --user -u <unit>` — attached to a `Broken`
/// reason so R7's "say precisely what is wrong" points at the actual
/// systemd/PipeWire error, not just "it didn't start." Best-effort: a
/// `journalctl` failure (missing binary, no journal access) degrades to
/// a short placeholder rather than losing the `Broken` report entirely.
fn recent_unit_journal(unit: &str, lines: u32) -> String {
    std::process::Command::new("journalctl")
        .args(["--user", "-u", unit, "-n", &lines.to_string(), "--no-pager"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(journal unavailable)".to_string())
}

/// Starts `filter-chain.service` and waits for the source to appear.
/// Returns `Some(reason)` on failure -- covers both a `systemctl start`
/// that itself fails and one that succeeds but the source never shows
/// up within the timeout (a plausible corrupt-fragment case even on a
/// cold start, not just the `RegenerateAndRestart` branch).
fn start_filter_chain_and_wait() -> Option<String> {
    let status = std::process::Command::new("systemctl")
        .args(["--user", "start", FILTER_CHAIN_UNIT])
        .status();
    match status {
        Ok(s) if s.success() => {
            if wait_for_source_bool(engine::SOURCE_NAME, Duration::from_secs(10)) {
                None
            } else {
                let reason = format!(
                    "filter-chain failed to start: source did not appear within 10s. \
                     journal:\n{}",
                    recent_unit_journal(FILTER_CHAIN_UNIT, 20)
                );
                tracing::error!("{reason}");
                Some(reason)
            }
        }
        Ok(s) => {
            let reason = format!(
                "systemctl start {FILTER_CHAIN_UNIT} exited with {s}. journal:\n{}",
                recent_unit_journal(FILTER_CHAIN_UNIT, 20)
            );
            tracing::error!("{reason}");
            Some(reason)
        }
        Err(e) => {
            let reason = format!("failed to run systemctl start {FILTER_CHAIN_UNIT}: {e}");
            tracing::error!("{reason}");
            Some(reason)
        }
    }
}

/// Restarts `filter-chain.service` (the `RegenerateAndRestart` branch's
/// retry-once) and waits for the source to reappear. Returns
/// `Some(reason)` — with the unit's own journal tail — if the source
/// still hasn't shown up after this one retry; the caller never loops
/// past this single attempt (R7: "never loop start attempts").
fn restart_filter_chain_and_wait() -> Option<String> {
    let status = std::process::Command::new("systemctl")
        .args(["--user", "restart", FILTER_CHAIN_UNIT])
        .status();
    match status {
        Ok(s) if s.success() => {
            if wait_for_source_bool(engine::SOURCE_NAME, Duration::from_secs(10)) {
                None
            } else {
                let reason = format!(
                    "filter-chain failed to start: source did not appear after regenerating \
                     the fragment and restarting once. journal:\n{}",
                    recent_unit_journal(FILTER_CHAIN_UNIT, 20)
                );
                tracing::error!("{reason}");
                Some(reason)
            }
        }
        Ok(s) => {
            let reason = format!(
                "systemctl restart {FILTER_CHAIN_UNIT} exited with {s}. journal:\n{}",
                recent_unit_journal(FILTER_CHAIN_UNIT, 20)
            );
            tracing::error!("{reason}");
            Some(reason)
        }
        Err(e) => {
            let reason = format!("failed to run systemctl restart {FILTER_CHAIN_UNIT}: {e}");
            tracing::error!("{reason}");
            Some(reason)
        }
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

    fn dev(id: &str) -> String {
        id.to_string()
    }

    fn device_info(id: &str) -> engine::DeviceInfo {
        engine::DeviceInfo {
            id: engine::DeviceId(id.to_string()),
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
        assert!(
            known.is_empty(),
            "known_devices must be cleared on disconnect"
        );
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
        assert_eq!(
            snapshot, None,
            "device already known from DeviceArrived -> no resend"
        );
    }
}
