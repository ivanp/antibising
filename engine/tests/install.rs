//! U3 live acceptance tests: the real bootstrap installer against the
//! real systemd user manager and the real PipeWire session.
//!
//! **Destructive-test gate:** per the plan's U3 execution note ("the
//! SIGKILL crash-recovery test destroyed the user's live filter once
//! this session... implement it as an opt-in test (`--destructive`
//! flag), never in the default suite"), tests that stop the user's real
//! `pipewire.service` (boot-order) or `SIGKILL` it (crash recovery)
//! additionally require `ANTIBISING_DESTRUCTIVE_TESTS=1` in the
//! environment, checked by [`assert_destructive_tests_enabled`] at the
//! top of each such test — `cargo test -- --ignored` alone is not
//! consent for those two; `--ignored` plus the env var is.
//!
//! **Why this can't run against a scratch directory:** [`InstallPaths`]
//! redirects where files are *written*, but the systemd user manager's
//! unit search path is fixed at its own process startup — a test-time
//! `systemctl` invocation cannot redirect where it looks for
//! `antibisingd.service`. So `install()`/`uninstall()` here necessarily
//! act on the real `~/.config/systemd/user/` and
//! `~/.config/pipewire/filter-chain.conf.d/` — there is no sandboxed
//! variant of the systemd-integration half. This matches the plan's
//! Verification Contract ("No container, no mock daemon — the product's
//! subject is this stack's real behavior") but also means every test
//! here is consent-gated per the plan's stop conditions (a real,
//! persistent `antibising_mic` source and a real `Upholds=` drop-in on
//! the user's actual `pipewire.service` are exactly the kind of lasting
//! change those conditions exist to gate).
//!
//! `#[ignore]`-marked per repo convention; the `InstallTestRig` RAII
//! guard always uninstalls on drop (including on panic) so a failing
//! assertion never leaves the real bootstrap half-installed.

use engine::{install, uninstall, InstallPaths};
use std::process::Command;
use std::time::Duration;

const FILTER_CHAIN_UNIT: &str = "filter-chain.service";
const DAEMON_UNIT: &str = "antibisingd.service";
const SOURCE_NAME: &str = "antibising_mic";


/// Gate for the two tests in this file that go beyond "creates a
/// persistent source/unit" (already consent-gated by `--ignored`) into
/// actually stopping or killing the user's real `pipewire.service` —
/// disrupting any audio currently in use on this machine. See the
/// module doc comment.
fn assert_destructive_tests_enabled() {
    assert!(
        std::env::var("ANTIBISING_DESTRUCTIVE_TESTS").as_deref() == Ok("1"),
        "this test stops or SIGKILLs the user's real pipewire.service — set \
         ANTIBISING_DESTRUCTIVE_TESTS=1 to opt in, only with the user present and consenting \
         (per the plan's U3 execution note)"
    );
}

fn production_paths() -> InstallPaths {
    // The daemon binary invoked by ExecStart= is this test binary's own
    // sibling `antibisingd` build — resolved the same way `cargo test`
    // lays out `target/debug/`, so the generated unit's ExecStart= points
    // at a real, buildable binary rather than a placeholder path.
    let exe = std::env::current_exe().expect("resolve test binary path");
    let target_debug = exe
        .ancestors()
        .find(|p| p.file_name().map(|n| n == "debug").unwrap_or(false))
        .expect("locate target/debug ancestor")
        .to_path_buf();
    let daemon_exec = target_debug.join("antibisingd");
    InstallPaths::production()
        .map(|p| InstallPaths {
            daemon_exec_path: daemon_exec,
            ..p
        })
        .expect("resolve production install paths")
}

/// Pre-flight guard: refuse to run any of this suite's tests if a real
/// install already exists on this machine. Every test begins with
/// `uninstall(&paths)` to guarantee a clean starting state, which is
/// safe *only* when nothing was there to destroy — if the user (or a
/// later U9 daemon) has a genuine working install, blindly wiping it
/// out from under a test run would be exactly the kind of destructive,
/// non-consented change the plan's stop conditions exist to forbid.
/// Call this before the first `uninstall` in every test.
fn assert_no_preexisting_install(paths: &InstallPaths) {
    let daemon_unit_exists = paths.daemon_unit_path.exists();
    let fragment_exists = paths.fragment_path.exists();
    let dropin_exists = paths.pipewire_dropin_path.exists();
    let daemon_active = is_unit_active(DAEMON_UNIT);
    let filter_chain_active = is_unit_active(FILTER_CHAIN_UNIT);
    assert!(
        !daemon_unit_exists
            && !fragment_exists
            && !dropin_exists
            && !daemon_active
            && !filter_chain_active,
        "refusing to run: a real antibising install already exists on this machine \
         (daemon_unit_exists={daemon_unit_exists}, fragment_exists={fragment_exists}, \
         dropin_exists={dropin_exists}, daemon_active={daemon_active}, \
         filter_chain_active={filter_chain_active}) — this test suite starts every test \
         with `uninstall`, which would destroy it; uninstall manually first if this is \
         expected, or investigate if it isn't"
    );
}

/// RAII guard: uninstalls on drop (including on panic) so a failing
/// assertion never leaves the real bootstrap half-installed on this
/// machine.
struct InstallTestRig;

impl Drop for InstallTestRig {
    fn drop(&mut self) {
        let _ = uninstall(&production_paths());
    }
}

fn run(cmd: &str, args: &[&str]) -> String {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {cmd}: {e}"));
    assert!(
        out.status.success(),
        "{cmd} {args:?} exited with {}: stderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// RAII guard around a spawned child process: on drop (including via
/// panic-unwind), sends SIGTERM, gives it a bounded window to exit, then
/// escalates to SIGKILL if it hasn't. Without this, a panic between
/// spawning `antibisingd run` and the explicit stop below — or a broken
/// SIGTERM handler in the daemon itself — would leave a second,
/// unmanaged daemon process altering the live PipeWire session for the
/// rest of this run, invisible to `InstallTestRig`'s own cleanup (which
/// only knows about files and systemd units, not this test's own child
/// process).
struct ChildGuard(std::process::Child);

impl ChildGuard {
    /// Ask the child to exit and wait up to `timeout`, polling via
    /// `try_wait` (never blocks past the deadline). Returns whether it
    /// exited within the window.
    fn terminate_and_wait(&mut self, signal: &str, timeout: Duration) -> bool {
        let _ = Command::new("kill")
            .args([signal, &self.0.id().to_string()])
            .status();
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match self.0.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(_) => return false,
            }
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // Already reaped by an explicit stop earlier in the test body —
        // try_wait here is just a cheap liveness check, not a race: if
        // the process already exited, this is a no-op.
        if matches!(self.0.try_wait(), Ok(Some(_))) {
            return;
        }
        if !self.terminate_and_wait("-TERM", Duration::from_secs(5)) {
            // SIGTERM didn't land in time (broken handler, or the daemon
            // is wedged) — escalate rather than leave an orphaned daemon
            // running against the live PipeWire session.
            let _ = Command::new("kill")
                .args(["-KILL", &self.0.id().to_string()])
                .status();
            let _ = self.0.wait();
        }
    }
}

/// RAII guard for the `boot_order_*` test: removes the
/// `pipewire.service.d` `Upholds=` drop-in on construction and restores
/// its exact prior content if the test's own explicit `restore()` call
/// never happens -- e.g. because an assertion panicked between removal
/// and that point. Without this, a panic there would leave this
/// daemon's crash-recovery mechanism (R2) permanently disabled on the
/// user's real system.
///
/// **`Drop` never uses the panicking `run()` helper.** A panic that
/// propagates out of `Drop::drop` while another panic is already
/// unwinding (exactly the scenario this guard exists for) aborts the
/// process instead of completing cleanup -- so `Drop` here is
/// deliberately best-effort with raw `Command`, swallowing failures
/// rather than risking that abort. The explicit `restore()` call in the
/// test's non-panicking path still uses strict raw status checks (via
/// `expect`) so a normal-path failure is loud, and marks the guard
/// already-restored so `Drop` becomes a no-op.
struct DropinGuard {
    path: std::path::PathBuf,
    content: String,
    /// `true` while the drop-in has genuinely been removed and not yet
    /// restored; `Drop::drop`'s best-effort restore only fires while
    /// this is `true`.
    active: bool,
}

impl DropinGuard {
    /// Panic-safe construction order: read the original content and
    /// build `Self` (so the guard -- and its eventual `Drop` restore --
    /// exists) **before** removing the file or reloading systemd. If
    /// either of those later steps panics, the already-constructed
    /// guard's `Drop` still fires and attempts to restore. The
    /// alternative order (remove-then-construct) would mean a
    /// daemon-reload failure right after removal leaves the drop-in
    /// permanently deleted, with no guard yet in existence to catch it.
    fn new(path: &std::path::Path) -> Self {
        let content = std::fs::read_to_string(path)
            .expect("read pipewire.service.d drop-in before removing it");
        let mut guard = Self {
            path: path.to_path_buf(),
            content,
            active: false,
        };
        std::fs::remove_file(&guard.path).expect("remove pipewire.service.d drop-in");
        guard.active = true; // now genuinely removed; Drop must restore if we panic past here
        let reload = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status()
            .expect("run systemctl daemon-reload after removing drop-in");
        assert!(reload.success(), "daemon-reload after removing drop-in should exit success");
        guard
    }

    /// Write the drop-in's original content back and reload systemd on
    /// the normal (non-panicking) path. Strict: any failure here panics
    /// loudly rather than leaving the caller to discover a silent no-op.
    /// Uses `engine::write_atomic` (temp file + rename), matching how
    /// the daemon itself writes this file -- a crash mid-write here
    /// must never leave a truncated drop-in on disk either.
    fn restore(mut self) {
        engine::write_atomic(&self.path, &self.content)
            .expect("restore pipewire.service.d drop-in");
        let reload = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status()
            .expect("run systemctl daemon-reload after restoring drop-in");
        assert!(reload.success(), "daemon-reload after restoring drop-in should exit success");
        self.active = false;
    }
}

impl Drop for DropinGuard {
    fn drop(&mut self) {
        if !self.active {
            return; // already restored via the explicit `restore()` call
        }
        // Best-effort only: never panic here (see the struct doc
        // comment on why -- this may run during an existing unwind).
        // `write_atomic` degrades to its own `?`-propagated io::Error
        // on failure, which we deliberately discard rather than
        // .expect() -- a partial temp file left behind by a failed
        // rename is still strictly better than nothing, and either way
        // this is the last-resort path, not the primary one.
        let _ = engine::write_atomic(&self.path, &self.content);
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
    }
}

fn is_unit_enabled(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", "is-enabled", "--quiet", unit])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn is_unit_active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", unit])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// True only if a source's **name column** (the second tab-separated
/// field of `pactl list short sources`) is exactly `name` — substring
/// matching would also match `antibising_mic_capture` or a stale
/// `antibising_mic_backup`-style node.
fn exact_source_present(name: &str) -> bool {
    run("pactl", &["list", "short", "sources"])
        .lines()
        .any(|line| line.split('\t').nth(1) == Some(name))
}

fn source_present(timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if exact_source_present(SOURCE_NAME) {
            return true;
        }
        if std::time::Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Poll until the source is confirmed gone (the inverse race to
/// `source_present`): after a service restart, the old process's node
/// deregistration is asynchronous relative to `systemctl restart`
/// returning, so a fixed sleep can observe the source mid-teardown.
fn source_absent(timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !exact_source_present(SOURCE_NAME) {
            return true;
        }
        if std::time::Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// True fresh install through the bootstrap: from zero artifacts,
/// `install()` brings the source up, both units enabled + active, and
/// the pipewire.service drop-in present.
#[test]
#[ignore = "live systemd/PipeWire test — real, persistent system changes; run with --ignored, with the user present and consenting"]
fn fresh_install_brings_source_up_with_both_units_active() {
    let paths = production_paths();
    assert_no_preexisting_install(&paths);
    // Ensure zero artifacts to start (idempotent no-op if already clean).
    let _ = uninstall(&paths);
    let _rig = InstallTestRig;

    install(&paths).expect("install should succeed from a clean state");

    assert!(
        source_present(Duration::from_secs(5)),
        "expected '{SOURCE_NAME}' source to appear within 5s of install"
    );
    assert!(is_unit_enabled(DAEMON_UNIT), "daemon unit should be enabled");
    assert!(is_unit_active(DAEMON_UNIT), "daemon unit should be active");
    assert!(
        is_unit_enabled(FILTER_CHAIN_UNIT),
        "filter-chain unit should be enabled"
    );
    assert!(
        is_unit_active(FILTER_CHAIN_UNIT),
        "filter-chain unit should be active"
    );
    assert!(
        paths.pipewire_dropin_path.exists(),
        "pipewire.service.d drop-in should be present"
    );
}

/// Install twice must not duplicate files or create a second source —
/// every write replaces its target, and re-enabling an already-enabled
/// unit is a systemd no-op.
#[test]
#[ignore = "live systemd/PipeWire test — real, persistent system changes; run with --ignored, with the user present and consenting"]
fn install_twice_is_idempotent() {
    let paths = production_paths();
    assert_no_preexisting_install(&paths);
    let _ = uninstall(&paths);
    let _rig = InstallTestRig;

    install(&paths).expect("first install should succeed");
    assert!(source_present(Duration::from_secs(5)));

    install(&paths).expect("second install should also succeed (idempotent)");
    assert!(
        source_present(Duration::from_secs(5)),
        "source should still be present after a second install"
    );

    // Exactly one source node by this exact name — no duplicate.
    let sources = run("pactl", &["list", "short", "sources"]);
    let count = sources
        .lines()
        .filter(|l| l.split('\t').nth(1) == Some(SOURCE_NAME))
        .count();
    assert_eq!(count, 1, "expected exactly one '{SOURCE_NAME}' source, found {count}");
}

/// Uninstall must leave no trace: source gone, units disabled, all three
/// files removed.
#[test]
#[ignore = "live systemd/PipeWire test — real, persistent system changes; run with --ignored, with the user present and consenting"]
fn uninstall_leaves_no_trace() {
    let paths = production_paths();
    assert_no_preexisting_install(&paths);
    let _ = uninstall(&paths);
    install(&paths).expect("install should succeed before testing uninstall");
    assert!(source_present(Duration::from_secs(5)));

    uninstall(&paths).expect("uninstall should succeed");

    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !exact_source_present(SOURCE_NAME),
        "source should be gone after uninstall"
    );
    assert!(!is_unit_enabled(DAEMON_UNIT), "daemon unit should be disabled");
    assert!(
        !is_unit_enabled(FILTER_CHAIN_UNIT),
        "filter-chain unit should be disabled"
    );
    assert!(!paths.daemon_unit_path.exists());
    assert!(!paths.fragment_path.exists());
    assert!(!paths.pipewire_dropin_path.exists());
}

/// Service started with artifacts missing (user deleted the fragment
/// while the unit was stopped): startup reconciliation must regenerate
/// it rather than fail. Covers the missing-fragment repair path —
/// **exercised through the real `antibisingd run` subcommand**, not by
/// calling `write_atomic`/`classify_startup` directly. Calling the
/// engine functions ourselves would only prove the primitives work, not
/// that `reconcile_startup`'s actual dispatch notices the missing
/// fragment and repairs it before starting the unit — the real behavior
/// this test exists to catch a regression in.
#[test]
#[ignore = "live systemd/PipeWire test — real, persistent system changes; run with --ignored, with the user present and consenting"]
fn missing_fragment_is_regenerated_on_next_start() {
    let paths = production_paths();
    assert_no_preexisting_install(&paths);
    let _ = uninstall(&paths);
    let _rig = InstallTestRig;

    install(&paths).expect("install should succeed");
    assert!(source_present(Duration::from_secs(5)));

    // Simulate the user deleting the fragment. This daemon's own
    // `pipewire.service.d` drop-in (`Upholds=filter-chain.service`)
    // means filter-chain.service cannot simply be stopped and left
    // stopped while pipewire.service stays up — systemd re-pulls it
    // (that's R2 working correctly, not a bug to work around). Deleting
    // the fragment alone has no immediate effect either, since
    // filter-chain only reads conf.d at its own start (Q8: no live
    // reload). The realistic way this state arises is a *restart* of
    // filter-chain.service (e.g. unrelated churn, or the reconciler)
    // after the fragment is gone: the unit comes back `active`, but
    // with an empty graph — no antibising_mic node. That is exactly the
    // `RegenerateAndRestart` branch of `reconcile_startup` (unit
    // active, source missing), which is what this test exercises.
    std::fs::remove_file(&paths.fragment_path).expect("remove fragment to simulate user deletion");
    run("systemctl", &["--user", "restart", FILTER_CHAIN_UNIT]);
    assert!(
        is_unit_active(FILTER_CHAIN_UNIT),
        "filter-chain.service should still be active after restarting with an empty conf.d \
         (it starts successfully, just with nothing in the graph)"
    );
    assert!(
        source_absent(Duration::from_secs(5)),
        "source should be gone: the fragment was removed before the restart picked up conf.d"
    );

    // Stop the systemd-managed daemon first -- otherwise the test's own
    // spawned `antibisingd run` below would run *alongside* it, both
    // connected to PipeWire and both reconciling the same capture node.
    run("systemctl", &["--user", "stop", DAEMON_UNIT]);
    assert!(
        !is_unit_active(DAEMON_UNIT),
        "installed daemon should be stopped before the test's own instance runs"
    );

    // Exercise the real startup path: spawn the daemon binary itself
    // (`antibisingd run`), let its `reconcile_startup` notice the
    // missing fragment, regenerate it, and start the unit. `ChildGuard`
    // ensures this process is always terminated — bounded SIGTERM wait,
    // escalating to SIGKILL — even if an assertion below panics.
    let mut child = ChildGuard(
        std::process::Command::new(&paths.daemon_exec_path)
            .arg("run")
            .spawn()
            .expect("spawn `antibisingd run`"),
    );

    let repaired = source_present(Duration::from_secs(10));

    // Explicit clean stop (SIGTERM, same signal systemd sends), bounded,
    // escalating to SIGKILL immediately on timeout — never leave the
    // daemon running while the assertions below inspect the filesystem
    // and PipeWire state it's actively managing (a still-alive daemon
    // could race the read of `paths.fragment_path` or start another
    // reconcile cycle concurrently with `source_present`'s check).
    if !child.terminate_and_wait("-TERM", Duration::from_secs(5)) {
        assert!(
            child.terminate_and_wait("-KILL", Duration::from_secs(5)),
            "antibisingd run did not exit even after SIGKILL — cannot safely assert \
             on filesystem/PipeWire state it may still be mutating"
        );
    }

    assert!(
        paths.fragment_path.exists(),
        "antibisingd run should have regenerated the missing fragment"
    );
    assert!(
        repaired,
        "source should reappear within 10s of `antibisingd run` noticing the missing fragment"
    );
}

/// **Plan scenario: boot-order.** Stop `pipewire`, `pipewire.socket`, and
/// `antibisingd`, then start `antibisingd.service` directly (never via
/// `start default.target` — an already-active target does not replay
/// its `Wants=`, per the plan's own caveat). Systemd must order
/// PipeWire first (`Wants=` pulls it, `After=` sequences it) and the
/// daemon must reach `active` without a failed/restart-loop episode —
/// proving the daemon's unit doesn't race PipeWire at boot the way a
/// unit lacking `Wants=`/`After=` would.
///
/// **Status: written and compile-verified, but not passing live in an
/// active graphical session on this machine.** Four live attempts all
/// failed identically: `systemctl --user stop pipewire.socket` (and
/// `pipewire.service` alongside it) is repeatedly canceled by systemd
/// ("Job for pipewire.socket canceled") regardless of stop ordering,
/// splitting into separate invocations, or removing this daemon's own
/// `Upholds=` drop-in first. The live desktop session's own dependency
/// closure (wireplumber, pipewire-pulse, sway-session.target, and
/// ~15 further running services under `sockets.target`) transitively
/// requires `pipewire.socket`, so systemd's own dependency resolution
/// actively fights a `--user` stop of it while the session is up — this
/// is not a bug in the test's stop sequence. A logout-free, disruption-
/// free reproduction of this scenario would need either a dedicated
/// test systemd user session or `systemctl isolate`-level intervention,
/// out of proportion to what U3 needs to prove; left for a session
/// where the user is prepared for a full graphical-session interruption
/// to investigate further, or a redesign that verifies the daemon-vs-
/// pipewire.service ordering without also tearing down the socket.
/// Deferred, not passing — do not treat this scenario as covered.
///
/// Destructive: stops the user's real `pipewire.service` (silences all
/// audio on this machine until PipeWire restarts, which happens as
/// part of this test). Gated behind `ANTIBISING_DESTRUCTIVE_TESTS=1`.
#[test]
#[ignore = "DEFERRED, not passing: pipewire.socket stop is fought by the live desktop \
session's own dependency closure (wireplumber/pipewire-pulse/sway-session.target and ~15 \
other running services) -- see the scenario's doc comment for four failed live attempts. \
Needs a session-teardown-tolerant environment (dedicated test session or systemctl isolate) \
to investigate further; do not run casually against a live desktop session."]
fn boot_order_starts_pipewire_before_daemon_without_restart_loop() {
    assert_destructive_tests_enabled();
    let paths = production_paths();
    assert_no_preexisting_install(&paths);
    let _ = uninstall(&paths);
    let _rig = InstallTestRig;

    install(&paths).expect("install should succeed");
    assert!(source_present(Duration::from_secs(5)));

    // Stop antibisingd and filter-chain first, before touching the
    // pipewire.service.d drop-in below -- no active manager should be
    // fighting the drop-in removal/restoration.
    run("systemctl", &["--user", "stop", DAEMON_UNIT, FILTER_CHAIN_UNIT]);

    // Temporarily remove the pipewire.service.d Upholds= drop-in before
    // stopping pipewire itself. With the drop-in in effect, systemd
    // treats pipewire.socket as still needing to stay up to satisfy
    // Upholds=filter-chain.service on pipewire.service, and a stop of
    // either pipewire.socket or pipewire.service gets canceled by a
    // competing job from that relationship (observed live, twice: exit
    // 1, stderr "Job for pipewire.socket canceled" / "Job for
    // pipewire.service canceled" -- even after filter-chain.service was
    // already stopped above, so the relationship is being re-asserted
    // by the drop-in itself, not merely by a currently-running
    // filter-chain). `DropinGuard` removes the drop-in for the duration
    // of the cold-stop below and restores it on drop -- including on
    // panic-unwind, so an assertion failure between removal and the
    // explicit restore point below never leaves crash recovery (R2)
    // permanently disabled on this machine.
    let dropin_guard = DropinGuard::new(&paths.pipewire_dropin_path);

    // Split into two systemctl invocations: stopping pipewire.socket
    // and pipewire.service in one combined transaction fails even with
    // the Upholds= drop-in already removed above (observed live: exit
    // 1, stderr "Job for pipewire.service canceled" / "Job for
    // pipewire.socket canceled") -- systemd treats the socket as a
    // "triggering unit" for the service (socket activation) regardless
    // of the Upholds= relationship, and won't finalize a same-
    // transaction stop of the service while its triggering socket is
    // still active. Stop the socket first and confirm it's actually
    // inactive before stopping the service -- the standard systemd
    // socket-activation shutdown sequence.
    run("systemctl", &["--user", "stop", "pipewire.socket"]);
    let socket_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while is_unit_active("pipewire.socket") {
        assert!(
            std::time::Instant::now() < socket_deadline,
            "pipewire.socket should become inactive within 5s of being stopped"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    run("systemctl", &["--user", "stop", "pipewire.service"]);

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut all_inactive = false;
    while std::time::Instant::now() < deadline {
        if !is_unit_active(DAEMON_UNIT)
            && !is_unit_active(FILTER_CHAIN_UNIT)
            && !is_unit_active("pipewire.service")
            && !is_unit_active("pipewire.socket")
        {
            all_inactive = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        all_inactive,
        "all four units should be inactive within 10s of the stop (filter-chain.service via \
         its own BindsTo=pipewire.service cascade) (antibisingd={}, filter-chain={}, \
         pipewire.service={}, pipewire.socket={})",
        is_unit_active(DAEMON_UNIT),
        is_unit_active(FILTER_CHAIN_UNIT),
        is_unit_active("pipewire.service"),
        is_unit_active("pipewire.socket")
    );

    // Explicit restore now, before starting antibisingd below -- not
    // merely relying on the guard's eventual Drop at function end --
    // so the boot-order startup sequence this scenario actually tests
    // exercises the real installed Wants=/After=/Upholds= configuration.
    // Restoring the drop-in while all four units are inactive does not
    // itself start anything: Upholds= only re-pulls its target while
    // the upheld-by unit (pipewire.service) is *active* -- it is not a
    // trigger that starts units on its own, so no second inactive-poll
    // is needed here.
    dropin_guard.restore();

    // Read a real before-baseline rather than assuming reset-failed
    // zeroes NRestarts (unverified on this systemd version/config, and
    // not worth manufacturing a restart just to test that semantic --
    // "unchanged from before" is a safe invariant regardless of what
    // the baseline happens to be).
    let daemon_restarts_before = unit_nrestarts(DAEMON_UNIT);
    assert!(
        daemon_restarts_before.is_some(),
        "could not read {DAEMON_UNIT}'s NRestarts before starting -- systemd's reporting is \
         unreliable here, so the after-check below would be meaningless"
    );

    // Start the daemon unit directly -- systemd must resolve
    // Wants=/After=pipewire.service on its own, not because
    // default.target happens to already have it running.
    let start_status = std::process::Command::new("systemctl")
        .args(["--user", "start", DAEMON_UNIT])
        .status()
        .expect("run systemctl start");
    assert!(start_status.success(), "systemctl start {DAEMON_UNIT} should exit success");

    // Give systemd's dependency resolution + PipeWire's own startup
    // time to settle. filter-chain.service is not pulled by the
    // daemon's own Wants=/After= -- it comes up afterward via
    // pipewire.service's Upholds=filter-chain.service drop-in once
    // PipeWire itself is active -- so wait for all three, not just
    // pipewire.service and the daemon.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut all_active = false;
    while std::time::Instant::now() < deadline {
        if is_unit_active("pipewire.service")
            && is_unit_active(DAEMON_UNIT)
            && is_unit_active(FILTER_CHAIN_UNIT)
        {
            all_active = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(
        all_active,
        "pipewire.service, {DAEMON_UNIT}, and {FILTER_CHAIN_UNIT} should all reach active \
         within 15s of starting {DAEMON_UNIT} directly (Wants=/After= should have pulled and \
         sequenced PipeWire; Upholds= should have then pulled filter-chain)"
    );

    // The decisive check per the plan's "without a failed/restart-loop
    // episode": is-active alone can't catch a unit that failed and was
    // then restarted back to active before this poll observed it --
    // NRestarts is the systemd-tracked counter that can't be missed by
    // timing. Any *increase* from the real pre-start baseline means the
    // unit failed at least once during this boot-order sequence, which
    // is exactly the regression this scenario exists to catch (per the
    // plan: "systemd's start-rate limit exhausts Restart=on-failure
    // before PipeWire is even up" when the ordering is wrong).
    let daemon_restarts_after = unit_nrestarts(DAEMON_UNIT);
    assert_eq!(
        daemon_restarts_after, daemon_restarts_before,
        "{DAEMON_UNIT}'s NRestarts must be unchanged from before this start -- any increase \
         means it failed at least once during boot-order startup (the exact failure mode \
         Wants=/After= exists to prevent), even though is-active alone would not have caught it"
    );

    // Belt-and-braces: also confirm neither unit is sitting in the
    // --failed list (covers the case where NRestarts is 0 but the unit
    // is nonetheless in a failed state that happens to not be "active"
    // -- redundant with the loop above, but cheap and explicit).
    let failed_units = run("systemctl", &["--user", "--failed", "--no-legend"]);
    assert!(
        !failed_units.contains("pipewire.service")
            && !failed_units.contains(DAEMON_UNIT)
            && !failed_units.contains(FILTER_CHAIN_UNIT),
        "none of the three units should be in the --failed list after boot-order start; got:\n\
         {failed_units}"
    );

    assert!(
        source_present(Duration::from_secs(10)),
        "source should be present once PipeWire, the daemon, and filter-chain are all up"
    );
}

/// `systemctl --user show <unit> -p NRestarts --value`, parsed. Used to
/// prove a unit's restart count did not increase across an interval --
/// `is-active` alone can miss a unit that failed and was restarted back
/// to active before a poll observed the failed state.
fn unit_nrestarts(unit: &str) -> Option<u32> {
    let out = run("systemctl", &["--user", "show", unit, "-p", "NRestarts", "--value"]);
    out.trim().parse::<u32>().ok()
}

/// **Plan scenario: consent-gated SIGKILL pipewire recovery.** SIGKILL
/// `pipewire.service` (not a graceful stop) twice consecutively; each
/// time, the source must return automatically within 10s. Per the
/// plan's own emphasis, the assertion covers the **full stack, not just
/// the source**: `antibisingd` must still be alive throughout (its U1
/// reconnect logic handles the disconnect, rather than the process
/// dying with PipeWire), proven by polling the daemon's own PID for
/// liveness across the kill.
///
/// Destructive: this is the exact test the plan's Stop Conditions
/// describe as having destroyed the user's live NoiseTorch filter once
/// already this session. Gated behind `ANTIBISING_DESTRUCTIVE_TESTS=1`;
/// run only with the user present and consenting, per the plan.
#[test]
#[ignore = "live systemd/PipeWire test — real, persistent system changes; run with --ignored, with the user present and consenting"]
fn sigkill_pipewire_recovers_source_and_daemon_twice_consecutively() {
    assert_destructive_tests_enabled();
    let paths = production_paths();
    assert_no_preexisting_install(&paths);
    let _ = uninstall(&paths);
    let _rig = InstallTestRig;

    install(&paths).expect("install should succeed");
    assert!(source_present(Duration::from_secs(5)));
    assert!(is_unit_active(DAEMON_UNIT), "daemon should be active after install");

    let capture_node = format!("{SOURCE_NAME}_capture");
    assert!(
        capture_has_nonself_input_link(&capture_node),
        "capture node should have an incoming link before the SIGKILL round begins"
    );

    for round in 1..=2 {
        let pipewire_pid_before = pipewire_main_pid();
        let daemon_pid_before = systemd_main_pid(DAEMON_UNIT);

        run("systemctl", &["--user", "kill", "--signal=SIGKILL", "pipewire.service"]);

        // Wait for a genuinely new pipewire.service PID (not just "some
        // PID"), then the source, then confirm the capture node is
        // actually *linked* -- not merely that the source node exists.
        // A dead daemon connection can leave the source present but
        // unlinked forever; source_present alone would pass in that
        // broken state, silently satisfying the assertion below without
        // ever proving routing actually resumed.
        let pipewire_deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut pipewire_recovered = false;
        while std::time::Instant::now() < pipewire_deadline {
            let after = pipewire_main_pid();
            if after.is_some() && after != pipewire_pid_before {
                pipewire_recovered = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(
            pipewire_recovered,
            "round {round}: pipewire.service should get a fresh PID within 10s of the SIGKILL"
        );

        let source_recovered = source_present(Duration::from_secs(10));
        assert!(
            source_recovered,
            "round {round}: source should reappear within 10s of SIGKILLing pipewire.service"
        );

        // The decisive full-stack check: the capture node must be
        // *linked* on the new PipeWire generation, not merely present
        // as an unlinked node. This is what distinguishes "the source
        // reappeared" from "the daemon's routing manager actually
        // reconnected and re-linked it" -- R2's real claim. The clean
        // baseline this session starts from has no other test sources
        // active, so any incoming link on the capture node other than
        // a self-loop is evidence the daemon's own reconciliation ran
        // and relinked it -- this does not distinguish a physical
        // device from some other software source, only "the daemon
        // reconciled a link," which is exactly what this test verifies.
        let link_deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut relinked = false;
        while std::time::Instant::now() < link_deadline {
            if capture_has_nonself_input_link(&capture_node) {
                relinked = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(
            relinked,
            "round {round}: capture node should have an incoming link within 10s of the \
             SIGKILL -- the source node existing is not enough; the daemon's U1 reconnect must \
             have actually re-run reconciliation on the new PipeWire generation"
        );

        // Full-stack assertion per the plan: only `pipewire.service`
        // was signaled here, so `antibisingd` must survive via U1's
        // reconnect logic -- the whole point of Wants=/After= instead
        // of BindsTo= (which would have killed it *with* PipeWire).
        // "or was restarted by systemd" is the plan's fallback wording
        // for a daemon that *was* also killed by something else; since
        // nothing here signals the daemon, its PID must be exactly
        // unchanged -- a PID change would mean the daemon died and
        // Restart=on-failure relaunched it, which is not what this
        // scenario is supposed to exercise.
        assert!(
            is_unit_active(DAEMON_UNIT),
            "round {round}: {DAEMON_UNIT} should still report active after the SIGKILL"
        );
        let daemon_pid_after = systemd_main_pid(DAEMON_UNIT);
        assert_eq!(
            daemon_pid_before, daemon_pid_after,
            "round {round}: {DAEMON_UNIT}'s PID must be unchanged -- only pipewire.service was \
             signaled, so the daemon surviving via U1's reconnect (not being restarted) is what \
             this test verifies"
        );
    }
}

/// Whether `capture_node` (a `pw-link -l` target, e.g.
/// `antibising_mic_capture`) currently has a nonself incoming link --
/// from any source other than PulseAudio's own compat proxy, the
/// permanent source itself, or the capture node's own name (guards
/// against a self-loop through the capture side, since a source block
/// header could in principle read `antibising_mic_capture` too, not
/// just `antibising_mic`). Does **not** prove the link comes from a
/// physical device -- only that a nonself link exists, which is what
/// the SIGKILL-recovery scenario needs: on this test's clean baseline
/// (no other test sources active), any such link is evidence the
/// daemon's own reconciliation ran and relinked it. Never hardcodes a
/// specific device name, so it works whichever device the engine's
/// default-ranking bootstrap happens to have picked as
/// highest-priority-present on the current PipeWire generation.
fn capture_has_nonself_input_link(capture_node: &str) -> bool {
    let dump = run("pw-link", &["-l"]);
    let mut current_source_is_not_self = false;
    for line in dump.lines() {
        if !line.starts_with(' ') {
            let source_name = line.trim();
            current_source_is_not_self = source_name != "PulseAudio Volume Control"
                && source_name != SOURCE_NAME
                && source_name != capture_node
                && !source_name.is_empty();
        } else if current_source_is_not_self
            && line.contains("|->")
            && line.contains(&format!("{capture_node}:input_"))
        {
            return true;
        }
    }
    false
}

fn systemd_main_pid(unit: &str) -> Option<u32> {
    let out = run("systemctl", &["--user", "show", unit, "-p", "MainPID", "--value"]);
    out.trim().parse::<u32>().ok().filter(|&pid| pid != 0)
}

fn pipewire_main_pid() -> Option<u32> {
    systemd_main_pid("pipewire.service")
}

/// **Plan scenario: Q4 matrix, `Adopt` branch.** Steady state (unit
/// active, source present) must be recognized and left alone by
/// `reconcile_startup` — no restart of `filter-chain.service`, no
/// duplicate source, no re-install. Exercised via
/// `systemctl --user restart antibisingd.service`, the real path this
/// scenario models (a `Restart=on-failure` relaunch of the daemon
/// itself after an unrelated crash, PipeWire and filter-chain
/// untouched) — **not** by spawning a second `antibisingd run` process
/// alongside the systemd-managed one, which would create two
/// PipeWire-connected daemons simultaneously reconciling the same
/// capture node and fighting over links.
#[test]
#[ignore = "live systemd/PipeWire test — real, persistent system changes; run with --ignored, with the user present and consenting"]
fn q4_adopt_branch_leaves_steady_state_untouched() {
    let paths = production_paths();
    assert_no_preexisting_install(&paths);
    let _ = uninstall(&paths);
    let _rig = InstallTestRig;

    install(&paths).expect("install should succeed");
    assert!(source_present(Duration::from_secs(5)));
    assert!(is_unit_active(FILTER_CHAIN_UNIT));

    let serial_before = current_source_serial();
    let filter_chain_pid_before = systemd_main_pid(FILTER_CHAIN_UNIT);

    // Restart the installed, systemd-managed daemon -- its
    // reconcile_startup runs again against the already-correct install
    // and must classify this as Adopt, touching nothing on the
    // filter-chain side.
    let restart_status = std::process::Command::new("systemctl")
        .args(["--user", "restart", DAEMON_UNIT])
        .status()
        .expect("run systemctl restart");
    assert!(restart_status.success(), "systemctl restart {DAEMON_UNIT} should exit success");
    assert!(
        source_present(Duration::from_secs(5)),
        "source should still be present after restarting the daemon"
    );
    // Settle briefly: Adopt is supposed to be a no-op on filter-chain,
    // but give any wrongly-triggered restart time to actually happen
    // before reading the "after" state, or a slow-starting restart
    // could be missed entirely by an immediate read.
    std::thread::sleep(Duration::from_secs(1));

    assert!(
        is_unit_active(FILTER_CHAIN_UNIT),
        "filter-chain.service should remain active throughout an Adopt cycle"
    );
    let filter_chain_pid_after = systemd_main_pid(FILTER_CHAIN_UNIT);
    assert_eq!(
        filter_chain_pid_before, filter_chain_pid_after,
        "Adopt must not restart filter-chain.service -- its MainPID must be unchanged \
         (a PID change would mean something restarted it, e.g. Upholds= reacting to an \
         unrelated pipewire.service event, which would also change object.serial below and \
         falsely look like a daemon-triggered restart)"
    );

    let serial_after = current_source_serial();
    assert_eq!(
        serial_before, serial_after,
        "Adopt must not restart filter-chain.service -- the source's object.serial (which \
         changes on every filter-chain restart) must be unchanged"
    );

    // Still exactly one source by this name -- no duplicate created.
    let sources = run("pactl", &["list", "short", "sources"]);
    let count = sources
        .lines()
        .filter(|l| l.split('\t').nth(1) == Some(SOURCE_NAME))
        .count();
    assert_eq!(count, 1, "expected exactly one '{SOURCE_NAME}' source, found {count}");
}

/// Current `object.serial` of the permanent source, used to prove a
/// filter-chain restart did or did not happen (serial changes on every
/// restart per the plan's measured finding; `node.name` does not).
fn current_source_serial() -> Option<String> {
    let out = run(
        "pactl",
        &["list", "sources"],
    );
    let mut in_target = false;
    for line in out.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Name: ") {
            in_target = trimmed == format!("Name: {SOURCE_NAME}");
            continue;
        }
        if in_target {
            if let Some(serial) = trimmed.strip_prefix("Object Serial: ") {
                return Some(serial.to_string());
            }
        }
    }
    None
}
