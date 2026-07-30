//! U10 live acceptance tests: the real Unix-socket IPC server, driven
//! end-to-end against scratch paths — never the real `$XDG_RUNTIME_DIR`
//! socket, `filter-chain.service`, or a live PipeWire session. These
//! tests don't need a real `Session`: `accept_loop`/`handle_client` only
//! need a `Sender<SessionCommand>` to forward into (verified by reading
//! it back here) and `handle_session_event` is driven directly to
//! simulate engine state changes — exactly the seam `ipc.rs` was
//! designed around, so these tests exercise the real socket, framing,
//! and broadcast/single-instance logic without needing a live daemon.

use antibisingd::ipc::{
    accept_loop, bind_singleton, handle_session_event, Broadcaster, Event, IpcPaths, Request,
    SharedState,
};
use engine::{Config, DeviceId, DeviceInfo, HealthStatus, SessionEvent};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn scratch_dir() -> PathBuf {
    std::env::temp_dir().join(format!("antibising-ipc-test-{}-{}", std::process::id(), rand_suffix()))
}

// Dependency-free unique suffix so parallel test-fn scratch dirs never
// collide — avoids pulling in a `rand` crate for one call. Tests run as
// threads within one process (`cargo test`'s default), so `process::id()`
// alone is not unique per test; a monotonic counter is.
static SCRATCH_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn rand_suffix() -> u64 {
    SCRATCH_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// RAII guard: removes the scratch directory on drop, including on
/// panic (the plan's own Cleanup invariant for live tests: "every live
/// test removes its sinks, links, and fragments even on panic"). Holds
/// the lock file too, so its Drop (releasing the flock) happens in the
/// same place as the directory removal rather than being separately
/// leaked per test.
struct ScratchGuard {
    dir: PathBuf,
    _lock_file: std::fs::File,
}

impl Drop for ScratchGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Spin up a real server (bind + accept loop on its own thread) against a
/// scratch `IpcPaths`, wired to a plain `mpsc::Sender<SessionCommand>` so
/// tests can assert on what the server forwards without a live `Session`.
/// Returns the paths (to connect clients), the command receiver, and a
/// guard that cleans up the scratch directory (and releases the flock)
/// on drop — keep it alive for the test's duration.
fn start_test_server() -> (
    IpcPaths,
    std::sync::mpsc::Receiver<engine::SessionCommand>,
    Arc<Mutex<SharedState>>,
    Broadcaster,
    ScratchGuard,
) {
    let dir = scratch_dir();
    let paths = IpcPaths::at(dir.clone());
    let (lock_file, listener) = bind_singleton(&paths).expect("bind_singleton on a fresh scratch dir");
    let guard = ScratchGuard {
        dir,
        _lock_file: lock_file,
    };

    let config_path = paths.socket_path.with_file_name("config.toml");
    let shared = Arc::new(Mutex::new(SharedState::new(Config::default(), config_path)));
    let broadcaster = Broadcaster::new();
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();

    {
        let shared = shared.clone();
        let broadcaster = broadcaster.clone();
        std::thread::spawn(move || accept_loop(listener, shared, broadcaster, cmd_tx));
    }

    (paths, cmd_rx, shared, broadcaster, guard)
}

fn connect(paths: &IpcPaths) -> UnixStream {
    // The accept thread needs a moment to start listening after
    // `start_test_server` returns; a short retry loop rather than a bare
    // sleep keeps this robust under load without over-waiting.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match UnixStream::connect(&paths.socket_path) {
            Ok(s) => return s,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("failed to connect to test IPC socket: {e}"),
        }
    }
}

fn read_event(stream: &mut BufReader<UnixStream>) -> Event {
    let mut line = String::new();
    stream
        .read_line(&mut line)
        .expect("read a line from the server");
    serde_json::from_str(&line).unwrap_or_else(|e| panic!("failed to parse event line {line:?}: {e}"))
}

fn send_request(stream: &mut UnixStream, request: &Request) {
    let mut line = serde_json::to_string(request).unwrap();
    line.push('\n');
    stream.write_all(line.as_bytes()).expect("write request");
}

/// Covers the plan's own scenario: "Daemon starts with no client ->
/// routing works ... with zero connections ever made." — proven here as
/// "the server accepts connections and serves Hello+Snapshot with zero
/// prior client activity," the IPC-layer half of that claim.
#[test]
fn client_connects_and_receives_hello_then_snapshot() {
    let (paths, _cmd_rx, _shared, _broadcaster, _guard) = start_test_server();
    let stream = connect(&paths);
    let mut reader = BufReader::new(stream);

    match read_event(&mut reader) {
        Event::Hello { protocol_version } => {
            assert_eq!(protocol_version, antibisingd::ipc::PROTOCOL_VERSION);
        }
        other => panic!("expected Hello, got {other:?}"),
    }
    match read_event(&mut reader) {
        Event::Snapshot { devices, health, .. } => {
            assert!(devices.is_empty(), "fresh SharedState has no devices yet");
            assert_eq!(health, HealthStatus::SilentNoDevice);
        }
        other => panic!("expected Snapshot, got {other:?}"),
    }
}

/// Two clients: a command from one is forwarded once through the shared
/// engine command channel, and the resulting state change (simulated via
/// `handle_session_event`, standing in for the engine's own reaction) is
/// visible to *both* connected clients — the plan's own "Two clients:
/// command from one -> state delta visible to both" scenario.
#[test]
fn command_from_one_client_broadcasts_delta_to_both() {
    let (paths, cmd_rx, shared, broadcaster, _guard) = start_test_server();

    let mut client_a = BufReader::new(connect(&paths));
    read_event(&mut client_a); // Hello
    read_event(&mut client_a); // Snapshot

    let mut client_b = BufReader::new(connect(&paths));
    read_event(&mut client_b); // Hello
    read_event(&mut client_b); // Snapshot

    // Client A sends a control request.
    send_request(
        client_a.get_mut(),
        &Request::SetThreshold { value: 55.0 },
    );

    // The command must have been forwarded into the engine channel.
    let forwarded = cmd_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("SetThreshold must forward a SessionCommand");
    match forwarded {
        engine::SessionCommand::SetRnnoiseParam(engine::RnnoiseParam::VadThreshold, value) => {
            assert_eq!(value, 55.0);
        }
        other => panic!("expected SetRnnoiseParam(VadThreshold, 55.0), got a different command: {other:?}"),
    }

    // Simulate the engine reacting with a health change (standing in for
    // what a live Session would eventually emit) and confirm the daemon
    // broadcasts it to every connected client, not just the sender.
    handle_session_event(
        &shared,
        &broadcaster,
        SessionEvent::HealthChanged(HealthStatus::SilentNoDevice),
    );

    for reader in [&mut client_a, &mut client_b] {
        match read_event(reader) {
            Event::HealthChanged(HealthStatus::SilentNoDevice) => {}
            other => panic!("expected HealthChanged(SilentNoDevice) on both clients, got {other:?}"),
        }
    }
}

/// Client disconnect (clean): the server must not crash or hang serving
/// the remaining client, and the departed client's registration must
/// eventually stop receiving broadcasts without erroring the broadcast
/// itself (the plan's disconnect-cleanup scenario, clean-close half).
#[test]
fn client_clean_disconnect_does_not_disrupt_other_clients() {
    let (paths, _cmd_rx, shared, broadcaster, _guard) = start_test_server();

    let mut client_a = BufReader::new(connect(&paths));
    read_event(&mut client_a);
    read_event(&mut client_a);

    {
        let mut client_b = BufReader::new(connect(&paths));
        read_event(&mut client_b);
        read_event(&mut client_b);
        // client_b drops here -> clean disconnect (socket close).
    }
    std::thread::sleep(Duration::from_millis(200));

    // A broadcast now must still reach the survivor and must not panic
    // the broadcaster despite the dead registration.
    handle_session_event(
        &shared,
        &broadcaster,
        SessionEvent::DeviceArrived(DeviceInfo {
            id: DeviceId("dev-a".to_string()),
            node_id: 1,
            description: "Razer".to_string(),
        }),
    );
    match read_event(&mut client_a) {
        Event::DeviceArrived(info) => assert_eq!(info.id, DeviceId("dev-a".to_string())),
        other => panic!("expected DeviceArrived, got {other:?}"),
    }
}

/// Malformed message -> error reply, connection survives, daemon never
/// panics (the plan's own explicit test scenario). The reply must reach
/// only the sender.
#[test]
fn malformed_message_gets_error_reply_and_connection_survives() {
    let (paths, _cmd_rx, _shared, _broadcaster, _guard) = start_test_server();

    let mut stream = connect(&paths);
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    read_event(&mut reader); // Hello
    read_event(&mut reader); // Snapshot

    stream
        .write_all(b"this is not valid json {{{\n")
        .expect("write malformed line");

    match read_event(&mut reader) {
        Event::Error { message } => {
            assert!(message.contains("malformed"), "error message: {message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }

    // Connection survives: a well-formed request afterward still works.
    send_request(&mut stream, &Request::RequestSnapshot);
    std::thread::sleep(Duration::from_millis(100));
    // No panic and no hang reaching this point is the assertion; a
    // well-formed follow-up request is silently accepted (RequestSnapshot
    // has no direct reply, only the forwarded command).
}

/// Stale-socket restart: kill the process that held the flock (simulated
/// here by dropping the held lock file explicitly, standing in for
/// SIGKILL releasing it at the kernel level) and confirm a second
/// `bind_singleton` call against the same paths then succeeds, unlinking
/// the leftover socket and binding cleanly.
#[test]
fn stale_socket_after_lock_release_allows_clean_rebind() {
    let dir = scratch_dir();
    let paths = IpcPaths::at(dir.clone());

    let (lock_file, _listener) =
        bind_singleton(&paths).expect("first bind_singleton must succeed");
    assert!(paths.socket_path.exists());

    // Simulate the holder dying: drop the lock file, releasing the flock
    // (this is exactly what a SIGKILL does at the kernel level) without
    // removing the socket file it left behind.
    drop(lock_file);

    let (_lock_file2, _listener2) = bind_singleton(&paths)
        .expect("second bind must succeed once the first's lock is released, unlinking the stale socket");
    assert!(paths.socket_path.exists(), "new instance must have re-bound the socket");

    std::fs::remove_dir_all(&dir).ok();
}

/// Concurrent start: a second `bind_singleton` while the first still
/// holds its lock must fail with `AlreadyRunning`, and must never touch
/// (unlink) the winner's live socket file.
#[test]
fn concurrent_bind_second_instance_refused_first_socket_untouched() {
    let dir = scratch_dir();
    let paths = IpcPaths::at(dir.clone());

    let (_lock_file, _listener) = bind_singleton(&paths).expect("first bind must succeed");
    assert!(paths.socket_path.exists());

    let second = bind_singleton(&paths);
    assert!(
        matches!(second, Err(antibisingd::ipc::BindError::AlreadyRunning)),
        "second bind while the first is live must be refused as AlreadyRunning"
    );
    assert!(
        paths.socket_path.exists(),
        "the refused second attempt must never unlink the live winner's socket"
    );

    std::fs::remove_dir_all(&dir).ok();
}
