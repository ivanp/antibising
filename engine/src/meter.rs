//! Pure level-meter math (U6/R9). No PipeWire I/O here — the engine-side
//! capture stream that produces raw f32 sample buffers lives in
//! `session.rs`; this module only turns those buffers into the RMS/peak
//! numbers the panel's speech indicator needs, so the math is
//! unit-testable without a live session.
//!
//! **No `peak`/`level` property exists on the node** (verified this
//! session) — a Discord-style level meter must compute levels from a
//! real captured stream, not a cheap property read. That capture lives
//! engine-side (U6's own stream, separate from any IPC client's), so the
//! UI never opens its own PipeWire connection.

/// One meter measurement, computed over roughly a 50ms frame of captured
/// samples (R9's own target cadence — fast enough to feel live, slow
/// enough not to flood the bounded per-client channel U6's daemon-side
/// binding uses).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MeterFrame {
    /// Root-mean-square level, 0.0-1.0 (assumes F32LE samples already in
    /// that native PipeWire range).
    pub rms: f32,
    /// Peak absolute sample value in the frame, 0.0-1.0.
    pub peak: f32,
}

/// Compute one [`MeterFrame`] from a slice of interleaved or mono f32
/// samples. Empty input is a legal silent frame (R4-adjacent: "no data
/// yet" must never be an error), not a panic or NaN.
pub fn compute_frame(samples: &[f32]) -> MeterFrame {
    if samples.is_empty() {
        return MeterFrame { rms: 0.0, peak: 0.0 };
    }
    let sum_sq: f32 = samples.iter().map(|&s| s * s).sum();
    let rms = (sum_sq / samples.len() as f32).sqrt();
    let peak = samples.iter().fold(0.0f32, |acc, &s| acc.max(s.abs()));
    MeterFrame { rms, peak }
}

/// A bounded meter-frame queue that drops the oldest frame when full
/// (the plan's own rule: "Meter frames never block the pw thread ...
/// through a bounded channel that drops the oldest frame when full -- a
/// slow or busy client loses meter frames, never stalls audio-side event
/// processing"). Not built on `std::sync::mpsc` because that channel has
/// no drop-oldest-on-full mode; this is a plain `VecDeque` with a
/// capacity cap, meant to be wrapped in a `Mutex` by the caller (the
/// session thread pushes, an IPC client thread drains) — no internal
/// locking here, so it stays trivially unit-testable.
#[derive(Debug)]
pub struct MeterChannel {
    frames: std::collections::VecDeque<MeterFrame>,
    capacity: usize,
}

impl MeterChannel {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "MeterChannel capacity must be positive");
        Self {
            frames: std::collections::VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Push a frame, dropping the oldest queued frame if already at
    /// capacity. Never blocks, never grows unboundedly.
    pub fn push(&mut self, frame: MeterFrame) {
        if self.frames.len() >= self.capacity {
            self.frames.pop_front();
        }
        self.frames.push_back(frame);
    }

    /// Drain every currently-queued frame, oldest first.
    pub fn drain(&mut self) -> Vec<MeterFrame> {
        self.frames.drain(..).collect()
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_samples_is_silent_not_a_panic() {
        let frame = compute_frame(&[]);
        assert_eq!(frame, MeterFrame { rms: 0.0, peak: 0.0 });
    }

    #[test]
    fn silence_reports_zero() {
        let frame = compute_frame(&[0.0; 100]);
        assert_eq!(frame.rms, 0.0);
        assert_eq!(frame.peak, 0.0);
    }

    #[test]
    fn constant_amplitude_rms_equals_amplitude() {
        // A DC-like constant signal's RMS equals its own magnitude.
        let frame = compute_frame(&[0.5; 480]);
        assert!((frame.rms - 0.5).abs() < 1e-6);
        assert!((frame.peak - 0.5).abs() < 1e-6);
    }

    #[test]
    fn full_scale_square_wave_rms_matches_known_value() {
        // A +-1.0 square wave has RMS exactly 1.0 -- a standard textbook
        // check that the formula (not just the code) is correct.
        let samples: Vec<f32> = (0..100).map(|i| if i % 2 == 0 { 1.0 } else { -1.0 }).collect();
        let frame = compute_frame(&samples);
        assert!((frame.rms - 1.0).abs() < 1e-6);
        assert_eq!(frame.peak, 1.0);
    }

    #[test]
    fn peak_ignores_sign() {
        let frame = compute_frame(&[0.1, -0.9, 0.3, -0.2]);
        assert_eq!(frame.peak, 0.9);
    }

    #[test]
    fn motion_on_speech_near_zero_on_silence() {
        // Drives R10's speech indicator honesty: a "loud" frame must
        // measure clearly higher than a silent one.
        let silent = compute_frame(&[0.001, -0.001, 0.0005, -0.0002]);
        let speech = compute_frame(&[0.6, -0.5, 0.7, -0.4, 0.55]);
        assert!(
            speech.rms > silent.rms * 10.0,
            "speech frame (rms {}) should be clearly louder than silence (rms {})",
            speech.rms,
            silent.rms
        );
    }

    #[test]
    fn meter_channel_drops_oldest_when_full() {
        let mut ch = MeterChannel::new(3);
        ch.push(MeterFrame { rms: 0.1, peak: 0.1 });
        ch.push(MeterFrame { rms: 0.2, peak: 0.2 });
        ch.push(MeterFrame { rms: 0.3, peak: 0.3 });
        ch.push(MeterFrame { rms: 0.4, peak: 0.4 }); // must drop 0.1's frame

        let frames = ch.drain();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].rms, 0.2, "oldest frame (0.1) must have been dropped");
        assert_eq!(frames[2].rms, 0.4);
    }

    #[test]
    fn meter_channel_drain_empties_the_queue() {
        let mut ch = MeterChannel::new(4);
        ch.push(MeterFrame { rms: 0.5, peak: 0.5 });
        assert_eq!(ch.len(), 1);
        let frames = ch.drain();
        assert_eq!(frames.len(), 1);
        assert!(ch.is_empty(), "drain must leave the channel empty");
    }

    #[test]
    fn meter_channel_never_exceeds_capacity() {
        let mut ch = MeterChannel::new(2);
        for i in 0..100 {
            ch.push(MeterFrame { rms: i as f32, peak: i as f32 });
            assert!(ch.len() <= 2, "queue must never exceed its capacity");
        }
    }
}
