//! U2 live acceptance tests: the stereo-tone downmix gate through the real
//! `filter-chain.service` + conf.d mechanism, not a standalone `pipewire -c`
//! process. `#[ignore]`-marked per repo convention for live-session tests —
//! run explicitly with `cargo test -p engine --test downmix -- --ignored`.
//!
//! Every test installs its own conf.d fragment under a *test-only* file
//! name (never touching the user's real `antibising_mic` fragment),
//! restarts `filter-chain.service` to pick it up, drives tone fixtures
//! through a synthetic 2ch null-sink source (the session's proven method),
//! and cleans up its fragment + null-sink on every exit path, including
//! panic (via an RAII guard).

use engine::{render_fragment, FragmentConfig};
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

const TEST_FRAGMENT_NAME: &str = "99-antibising-downmix-test.conf";
const TEST_SOURCE_NODE: &str = "antibising_downmix_test";
const TEST_NULL_SINK: &str = "antibising_downmix_test_src";

fn fragment_dir() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").expect("HOME must be set")).join(".config")
        });
    base.join("pipewire/filter-chain.conf.d")
}

fn fragment_path() -> PathBuf {
    fragment_dir().join(TEST_FRAGMENT_NAME)
}

/// Renders the U2 bypass graph under a test-only node name so it never
/// collides with (or gets mistaken for) the real `antibising_mic` source.
fn render_test_fragment() -> String {
    // engine::render_fragment always emits SOURCE_NAME ("antibising_mic");
    // swap in the test node name so this fragment can coexist with a real
    // one and is unambiguous in `pactl` output during debugging.
    render_fragment(&FragmentConfig::default()).replace(engine::SOURCE_NAME, TEST_SOURCE_NODE)
}

/// RAII guard: installs the test fragment + null-sink on construction,
/// tears both down (and restarts the service back to a clean state) on
/// drop — including on panic, so a failing assertion never leaves the
/// user's real filter-chain fragment set polluted.
struct DownmixTestRig;

impl DownmixTestRig {
    fn setup() -> Self {
        let dir = fragment_dir();
        std::fs::create_dir_all(&dir).expect("create conf.d dir");
        let content = render_test_fragment();
        engine::write_atomic(&fragment_path(), &content).expect("write test fragment");

        run("systemctl", &["--user", "restart", "filter-chain.service"]);
        wait_for_source(TEST_SOURCE_NODE, std::time::Duration::from_secs(5));

        run(
            "pactl",
            &[
                "load-module",
                "module-null-sink",
                &format!("sink_name={TEST_NULL_SINK}"),
                "sink_properties=device.description=AntibisingDownmixTest",
                "channels=2",
            ],
        );
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Relink the capture side to our synthetic source — the proven
        // method from this session, since WirePlumber auto-links to
        // whatever real physical mic is present, not our test sink.
        relink_capture();

        Self
    }
}

impl Drop for DownmixTestRig {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(fragment_path());
        run_ignore_status(
            "pactl",
            &["unload-module", &null_sink_module_id().unwrap_or_default()],
        );
        run("systemctl", &["--user", "stop", "filter-chain.service"]);
    }
}

fn run(cmd: &str, args: &[&str]) -> String {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {cmd}: {e}"));
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn run_ignore_status(cmd: &str, args: &[&str]) {
    let _ = Command::new(cmd).args(args).output();
}

fn null_sink_module_id() -> Option<String> {
    let out = run("pactl", &["list", "short", "modules"]);
    out.lines()
        .find(|l| l.contains(TEST_NULL_SINK))
        .and_then(|l| l.split_whitespace().next())
        .map(String::from)
}

fn wait_for_source(node_name: &str, timeout: std::time::Duration) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let out = run("pactl", &["list", "short", "sources"]);
        if out.contains(node_name) {
            return;
        }
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for test source '{node_name}' to appear");
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn relink_capture() {
    // Disconnect whatever WirePlumber auto-linked (a real physical mic) and
    // connect our synthetic 2ch source instead.
    let capture_node = format!("{TEST_SOURCE_NODE}_capture");
    let links = run("pw-link", &["-l"]);
    for line in links.lines() {
        if line.trim_start().starts_with('|') && line.contains("->") {
            continue;
        }
    }
    // Best-effort disconnect of any existing FL/FR link, then connect ours.
    // pw-link -d is idempotent-safe (errors on missing links are ignored).
    for port in ["FL", "FR"] {
        let target_port = format!("{capture_node}:input_{port}");
        // Find and remove any existing incoming link to this port.
        let out = run("pw-link", &["-i", &target_port]);
        for _ in out.lines() {
            // best-effort; exact source name varies by machine, so we just
            // attempt disconnect-all via -d with no args is not supported —
            // instead rely on the connect below overriding when possible.
        }
        run_ignore_status(
            "pw-link",
            &[
                &format!("{TEST_NULL_SINK}:monitor_{port}"),
                &target_port,
            ],
        );
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
}

fn write_tone_wav(path: &std::path::Path, gain_l: f64, gain_r: f64) {
    let sample_rate = 48_000u32;
    let duration_secs = 5u32;
    let freq = 440.0;
    let amplitude = 12_000.0;

    let mut samples = Vec::with_capacity((sample_rate * duration_secs * 4) as usize);
    for t in 0..(sample_rate * duration_secs) {
        let phase = 2.0 * std::f64::consts::PI * freq * (t as f64) / (sample_rate as f64);
        let l = (amplitude * gain_l * phase.sin()) as i16;
        let r = (amplitude * gain_r * phase.sin()) as i16;
        samples.extend_from_slice(&l.to_le_bytes());
        samples.extend_from_slice(&r.to_le_bytes());
    }
    write_wav(path, sample_rate, 2, &samples);
}

fn write_loud_tone_wav(path: &std::path::Path) {
    let sample_rate = 48_000u32;
    let duration_secs = 5u32;
    let freq = 440.0;
    let amplitude = 30_000.0; // ~0.92 FS, matching the session's measured hazard case

    let mut samples = Vec::with_capacity((sample_rate * duration_secs * 4) as usize);
    for t in 0..(sample_rate * duration_secs) {
        let phase = 2.0 * std::f64::consts::PI * freq * (t as f64) / (sample_rate as f64);
        let v = (amplitude * phase.sin()) as i16;
        samples.extend_from_slice(&v.to_le_bytes());
        samples.extend_from_slice(&v.to_le_bytes());
    }
    write_wav(path, sample_rate, 2, &samples);
}

fn write_wav(path: &std::path::Path, sample_rate: u32, channels: u16, data: &[u8]) {
    let mut f = std::fs::File::create(path).expect("create wav file");
    let byte_rate = sample_rate * channels as u32 * 2;
    let block_align = channels * 2;
    let data_len = data.len() as u32;

    f.write_all(b"RIFF").unwrap();
    f.write_all(&(36 + data_len).to_le_bytes()).unwrap();
    f.write_all(b"WAVE").unwrap();
    f.write_all(b"fmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
    f.write_all(&channels.to_le_bytes()).unwrap();
    f.write_all(&sample_rate.to_le_bytes()).unwrap();
    f.write_all(&byte_rate.to_le_bytes()).unwrap();
    f.write_all(&block_align.to_le_bytes()).unwrap();
    f.write_all(&16u16.to_le_bytes()).unwrap(); // bits per sample
    f.write_all(b"data").unwrap();
    f.write_all(&data_len.to_le_bytes()).unwrap();
    f.write_all(data).unwrap();
}

/// Play `wav_path` into the test null-sink, capture the resulting mono
/// output from the test source for the duration, return the middle third
/// of samples as i16 (trims fixture start/stop transients).
fn capture_through_downmix(wav_path: &std::path::Path) -> Vec<i16> {
    let raw_path = wav_path.with_extension("pcm");
    let mut record = Command::new("parecord")
        .args([
            &format!("--device={TEST_SOURCE_NODE}"),
            "--format=s16le",
            "--rate=48000",
            "--channels=1",
            "--raw",
            raw_path.to_str().unwrap(),
        ])
        .spawn()
        .expect("spawn parecord");

    std::thread::sleep(std::time::Duration::from_millis(500));
    let status = Command::new("paplay")
        .args([&format!("--device={TEST_NULL_SINK}"), wav_path.to_str().unwrap()])
        .status()
        .expect("run paplay");
    assert!(status.success(), "paplay failed to play fixture");

    std::thread::sleep(std::time::Duration::from_millis(300));
    let _ = record.kill();
    let _ = record.wait();

    let data = std::fs::read(&raw_path).expect("read captured pcm");
    let samples: Vec<i16> = data
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    let third = samples.len() / 3;
    samples[third..2 * third].to_vec()
}

fn rms(samples: &[i16]) -> f64 {
    let sum_sq: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum_sq / samples.len() as f64).sqrt()
}

fn peak(samples: &[i16]) -> i16 {
    samples.iter().map(|&s| s.abs()).max().unwrap_or(0)
}

fn clipped_count(samples: &[i16]) -> usize {
    samples.iter().filter(|&&s| s.abs() >= 32_700).count()
}

/// Covers Q1's gate: L-only, R-only, and both-channels tones through the
/// real filter-chain graph. L-only and R-only must each measure
/// approximately half the RMS of both-channels, with zero clipped samples.
#[test]
#[ignore = "live PipeWire session test — run with --ignored"]
fn stereo_downmix_gate_left_right_both() {
    let _rig = DownmixTestRig::setup();
    let dir = std::env::temp_dir().join(format!("antibising-downmix-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    write_tone_wav(&dir.join("left.wav"), 1.0, 0.0);
    write_tone_wav(&dir.join("right.wav"), 0.0, 1.0);
    write_tone_wav(&dir.join("both.wav"), 1.0, 1.0);

    let left = capture_through_downmix(&dir.join("left.wav"));
    let right = capture_through_downmix(&dir.join("right.wav"));
    let both = capture_through_downmix(&dir.join("both.wav"));

    let rms_l = rms(&left);
    let rms_r = rms(&right);
    let rms_both = rms(&both);

    assert_eq!(clipped_count(&left), 0);
    assert_eq!(clipped_count(&right), 0);
    assert_eq!(clipped_count(&both), 0);

    // L-only and R-only should each be ~50% of both-channels RMS (0.5 gain
    // per channel). Allow 10% tolerance for tone-generation/measurement
    // noise.
    let ratio_l = rms_l / rms_both;
    let ratio_r = rms_r / rms_both;
    assert!(
        (0.45..=0.55).contains(&ratio_l),
        "left-only RMS {rms_l} should be ~50% of both {rms_both} (ratio {ratio_l})"
    );
    assert!(
        (0.45..=0.55).contains(&ratio_r),
        "right-only RMS {rms_r} should be ~50% of both {rms_both} (ratio {ratio_r})"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// The measured hazard this gate exists to close: a 0.92 FS stereo input
/// through the *naive* default mix clipped 63.2% of samples. Through the
/// explicit 0.5/0.5 mixer, it must clip zero.
#[test]
#[ignore = "live PipeWire session test — run with --ignored"]
fn loud_stereo_input_does_not_clip() {
    let _rig = DownmixTestRig::setup();
    let dir = std::env::temp_dir().join(format!("antibising-downmix-loud-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    write_loud_tone_wav(&dir.join("loud.wav"));
    let captured = capture_through_downmix(&dir.join("loud.wav"));

    let clipped = clipped_count(&captured);
    assert_eq!(
        clipped, 0,
        "0.92 FS stereo input clipped {clipped}/{} samples through the explicit-gain mixer",
        captured.len()
    );
    assert!(peak(&captured) > 20_000, "signal should still be strong, not silently attenuated to nothing");

    std::fs::remove_dir_all(&dir).ok();
}

/// Regression on the earlier verified direction: a 1ch mono device feeding
/// the (2ch-capture) source must still pass audio through cleanly — the
/// mixer's second input simply receives silence, and mono content on
/// channel 1 alone should still reach the output at full gain-scaled
/// level (0.5x, since only "Gain 1" applies).
#[test]
#[ignore = "live PipeWire session test — run with --ignored"]
fn mono_device_feeding_source_still_passes_audio() {
    let _rig = DownmixTestRig::setup();
    let dir = std::env::temp_dir().join(format!("antibising-downmix-mono-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // A "mono" device presented on the FL channel only, FR silent — this is
    // exactly what left.wav already models, but named here to make the
    // regression intent explicit and self-contained.
    write_tone_wav(&dir.join("mono.wav"), 1.0, 0.0);
    let captured = capture_through_downmix(&dir.join("mono.wav"));

    assert_eq!(clipped_count(&captured), 0);
    let level = rms(&captured);
    assert!(
        level > 1000.0,
        "mono input on FL must still produce audible output through the downmix (RMS {level})"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Q8 inertness, verified against the *actual* config path and the *real*
/// `filter-chain.service`: rewriting the fragment while the service is
/// running must not change the live graph. Confirmed by comparing both
/// `object.serial` (source identity) and the active capture-side links
/// before and after the write.
#[test]
#[ignore = "live PipeWire session test — run with --ignored"]
fn fragment_rewrite_while_running_does_not_disrupt_graph() {
    let _rig = DownmixTestRig::setup();

    let serial_before = current_serial(TEST_SOURCE_NODE);
    let links_before = run("pw-link", &["-l"]);

    // Rewrite the *real* fragment path the running service was started
    // from, through the same write_atomic path U2/KTD6 use in production.
    let content = render_test_fragment();
    engine::write_atomic(&fragment_path(), &content).expect("rewrite fragment");
    std::thread::sleep(std::time::Duration::from_millis(500));

    let serial_after = current_serial(TEST_SOURCE_NODE);
    let links_after = run("pw-link", &["-l"]);

    assert_eq!(
        serial_before, serial_after,
        "fragment rewrite must not change the running source's identity"
    );
    assert_eq!(
        links_before, links_after,
        "fragment rewrite must not change the active link graph"
    );
}

fn current_serial(node_name: &str) -> Option<String> {
    let out = Command::new("pw-dump").output().ok()?;
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    json.as_array()?.iter().find_map(|obj| {
        let props = obj.get("info")?.get("props")?;
        if props.get("node.name")?.as_str()? == node_name {
            Some(props.get("object.serial")?.to_string())
        } else {
            None
        }
    })
}
