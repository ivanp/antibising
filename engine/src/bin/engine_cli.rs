//! `engine-cli` — a small verification/diagnostic binary for the engine
//! crate. `dump` prints the current device set as JSON and exits, used by
//! U1's verification: run twice, identical output; unload a test sink,
//! device disappears.

use engine::{Session, SessionCommand, SessionEvent};
use std::env;
use std::time::Duration;

fn main() {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("dump") => cmd_dump(),
        Some("watch") => cmd_watch(),
        other => {
            eprintln!("usage: engine-cli <dump|watch>");
            if let Some(cmd) = other {
                eprintln!("unknown subcommand: {cmd}");
            }
            std::process::exit(2);
        }
    }
}

fn cmd_dump() {
    let session = Session::spawn("antibising_mic".to_string());

    // Wait for the first Snapshot (post-sync, full initial enumeration) or
    // time out. In practice this arrives within a handful of milliseconds
    // on a live PipeWire session.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match session.try_recv_event() {
            Some(SessionEvent::Snapshot(mut devices)) => {
                // Sort for deterministic output — U1's verification requires
                // two runs to produce identical text, and HashMap iteration
                // order (the session thread's internal storage) is not
                // stable across runs.
                devices.sort_by(|a, b| a.id.cmp(&b.id));
                let json = serde_json::to_string_pretty(&devices)
                    .expect("device list is always serializable");
                println!("{json}");
                return;
            }
            Some(SessionEvent::Disconnected) => {
                eprintln!("error: lost connection to PipeWire while waiting for snapshot");
                std::process::exit(1);
            }
            Some(_other) => continue,
            None => {
                if std::time::Instant::now() > deadline {
                    eprintln!("error: timed out waiting for initial snapshot");
                    std::process::exit(1);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Print every session event as it happens, with a timestamp, until the
/// process is killed. Used to observe the U1 reconnect scenario live:
/// `Disconnected` followed by a fresh `Snapshot` proves the session thread
/// survived a PipeWire restart and re-enumerated cleanly.
fn cmd_watch() {
    let session = Session::spawn("antibising_mic".to_string());
    loop {
        match session.recv_event() {
            Some(event) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                println!("[{:.3}] {:?}", now.as_secs_f64(), event);
            }
            None => {
                eprintln!("session thread exited");
                return;
            }
        }
    }
}

// Explicit re-export so the compiler doesn't warn about the unused import
// when SessionCommand isn't referenced directly by `dump`/`watch` today;
// kept for forward compatibility with `engine-cli link <device>` etc.
#[allow(unused_imports)]
use SessionCommand as _SessionCommandReexport;
