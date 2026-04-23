// SPDX-License-Identifier: GPL-2.0-or-later

use std::collections::BTreeMap;

use crate::FRAME_SAMPLES;

/// A frame stored in the jitter buffer
#[derive(Debug, Clone)]
pub struct BufferedFrame {
    pub sequence: u32,
    pub timestamp: u32,
    pub opus_data: Vec<u8>,
    pub ptt: bool,
}

/// Result from pulling a frame out of the jitter buffer
#[derive(Debug)]
pub enum JitterResult {
    /// Normal frame available
    Frame(BufferedFrame),
    /// Frame missing — caller should use FEC or PLC
    Missing,
    /// Buffer is empty / not enough data yet (still filling)
    NotReady,
}

/// Adaptive jitter buffer.
///
/// Buffers incoming packets ordered by sequence number, and releases them
/// in order with an adaptive target delay based on observed network jitter.
pub struct JitterBuffer {
    /// Buffered frames, keyed by sequence number
    buffer: BTreeMap<u32, BufferedFrame>,
    /// Next expected sequence number for playout
    next_seq: Option<u32>,
    /// Target buffer depth in frames
    target_depth: u32,
    /// Minimum depth
    min_depth: u32,
    /// Maximum depth
    max_depth: u32,
    /// Running jitter estimate (exponential moving average, in ms)
    jitter_estimate_ms: f32,
    /// Peak jitter hold for spike resilience (fast attack, slow ~1min decay)
    spike_peak_ms: f32,
    /// Last arrival timestamp for jitter calculation
    last_arrival_ms: Option<u64>,
    /// Last packet timestamp for jitter calculation
    last_packet_ts: Option<u32>,
    /// Total frames received (stats)
    pub stats_received: u64,
    /// Total frames lost (stats)
    pub stats_lost: u64,
    /// Total late frames dropped (stats)
    pub stats_late: u64,
    /// Grace period after init/reset: suppress overflow recovery
    initial_fill_remaining: u32,
    /// Underflow recovery: when buffer drains to empty, pause until refilled to target
    refilling: bool,
}

impl JitterBuffer {
    /// Create a new jitter buffer with the given depth range (in frames).
    pub fn new(min_depth: u32, max_depth: u32) -> Self {
        Self {
            buffer: BTreeMap::new(),
            next_seq: None,
            target_depth: min_depth,
            min_depth,
            max_depth,
            jitter_estimate_ms: 0.0,
            spike_peak_ms: 0.0,
            last_arrival_ms: None,
            last_packet_ts: None,
            stats_received: 0,
            stats_lost: 0,
            stats_late: 0,
            initial_fill_remaining: 25,
            refilling: false,
        }
    }

    /// Push a received packet into the buffer.
    /// `arrival_ms` is the local monotonic time when the packet arrived.
    pub fn push(&mut self, frame: BufferedFrame, arrival_ms: u64) {
        self.stats_received += 1;

        // Update jitter estimate using RFC 3550 method.
        // Only update for in-order packets (timestamp must advance reasonably).
        if let (Some(last_arrival), Some(last_ts)) = (self.last_arrival_ms, self.last_packet_ts) {
            let ts_diff_raw = frame.timestamp.wrapping_sub(last_ts);
            // Only compute jitter for forward-moving timestamps (< 1s)
            if ts_diff_raw > 0 && ts_diff_raw < 1000 {
                let arrival_diff = arrival_ms as f64 - last_arrival as f64;
                let ts_diff_ms = ts_diff_raw as f64; // timestamps are already in ms
                let deviation = (arrival_diff - ts_diff_ms).abs() as f32;
                // Dual-alpha EMA: fast rise (1/4) on spikes, slow decay (1/16) on recovery
                let alpha = if deviation > self.jitter_estimate_ms { 0.25 } else { 1.0 / 16.0 };
                self.jitter_estimate_ms += (deviation - self.jitter_estimate_ms) * alpha;

                // Spike peak hold: instant attack, slow exponential decay (~1 min)
                // At 50 packets/sec, 3000 packets = 1 minute. Decay factor = 1 - 1/3000.
                if deviation > self.spike_peak_ms {
                    self.spike_peak_ms = deviation; // instant jump to spike level
                } else {
                    self.spike_peak_ms *= 1.0 - (1.0 / 3000.0); // ~1 min decay
                }

                // Target depth: use the higher of normal jitter or spike peak
                let jitter_for_target = self.jitter_estimate_ms.max(self.spike_peak_ms);
                let desired = (jitter_for_target / 15.0) as u32 + 2;
                self.target_depth = desired.clamp(self.min_depth, self.max_depth);
            }
        }
        self.last_arrival_ms = Some(arrival_ms);
        self.last_packet_ts = Some(frame.timestamp);

        // Drop packets that arrived too late
        if let Some(next) = self.next_seq {
            if is_seq_before(frame.sequence, next) {
                self.stats_late += 1;
                return;
            }
        }

        self.buffer.insert(frame.sequence, frame);

        // Limit buffer size: hard cap to prevent unbounded growth
        let hard_limit = self.max_depth as usize + 10;
        while self.buffer.len() > hard_limit {
            self.buffer.pop_first();
        }
    }

    /// Pull the next frame for playout.
    pub fn pull(&mut self) -> JitterResult {
        // Initialize next_seq on first pull if we have data
        if self.next_seq.is_none() {
            if self.buffer.len() < self.target_depth as usize {
                return JitterResult::NotReady;
            }
            if let Some((&first_seq, _)) = self.buffer.first_key_value() {
                self.next_seq = Some(first_seq);
            } else {
                return JitterResult::NotReady;
            }
        }

        // Nothing buffered — enter refill mode and wait for data
        if self.buffer.is_empty() {
            if !self.refilling {
                log::debug!("Jitter buffer underflow: entering refill mode (target {})", self.target_depth);
                self.refilling = true;
            }
            return JitterResult::NotReady;
        }

        // Underflow recovery: after draining to empty, wait until buffer
        // refills to target_depth before resuming playout. This prevents
        // rapid drain/fill cycling that causes audio micro-interruptions.
        if self.refilling {
            if self.buffer.len() >= self.target_depth as usize {
                log::debug!("Jitter buffer refilled to {} (target {})", self.buffer.len(), self.target_depth);
                self.refilling = false;
            } else {
                return JitterResult::NotReady;
            }
        }

        // Decrement grace period
        if self.initial_fill_remaining > 0 {
            self.initial_fill_remaining -= 1;
        }

        // Overflow recovery: drain excess frames when buffer is above target.
        // Uses two tiers to balance smoothness vs responsiveness:
        //   > target+2: drop 1 frame per pull (gentle drain)
        //   > target+6: drop 2 frames per pull (aggressive drain after spike)
        // Suppressed during initial fill grace period (first 500ms after reset).
        // IMPORTANT: when dropping, also advance next_seq to avoid Missing for
        // frames we intentionally discarded. Without this, every overflow drop
        // triggers PLC which progressively degrades audio quality.
        if self.initial_fill_remaining == 0 {
            let excess = self.buffer.len() as i32 - self.target_depth as i32 - 2;
            let drops = if excess > 4 { 2 } else if excess > 0 { 1 } else { 0 };
            for _ in 0..drops {
                if let Some((&oldest, _)) = self.buffer.first_key_value() {
                    self.buffer.pop_first();
                    // Advance next_seq past the dropped frame to avoid
                    // a spurious Missing result on the next pull
                    if let Some(ref mut next) = self.next_seq {
                        if !is_seq_before(oldest, *next) {
                            *next = oldest.wrapping_add(1);
                        }
                    }
                    log::debug!(
                        "Jitter buffer overflow: skipped frame {} (depth {} -> {}, target {})",
                        oldest,
                        self.buffer.len() + 1,
                        self.buffer.len(),
                        self.target_depth,
                    );
                }
            }
        }

        let seq = self.next_seq.unwrap();
        self.next_seq = Some(seq.wrapping_add(1));

        match self.buffer.remove(&seq) {
            Some(frame) => JitterResult::Frame(frame),
            None => {
                // If we're far behind the buffered data, skip ahead
                if let Some((&first_buffered, _)) = self.buffer.first_key_value() {
                    if first_buffered.wrapping_sub(seq) > self.max_depth {
                        log::warn!(
                            "Jitter buffer skip-ahead: seq {} -> {} (gap {})",
                            seq, first_buffered, first_buffered.wrapping_sub(seq)
                        );
                        self.next_seq = Some(first_buffered);
                        return JitterResult::NotReady;
                    }
                }
                self.stats_lost += 1;
                JitterResult::Missing
            }
        }
    }

    /// Peek at the opus data of a specific sequence number (for FEC decoding).
    /// Returns None if the packet isn't buffered.
    pub fn peek_opus_data(&self, seq: u32) -> Option<&[u8]> {
        self.buffer.get(&seq).map(|f| f.opus_data.as_slice())
    }

    /// Peek at the next expected sequence number (for FEC look-ahead).
    pub fn next_seq_peek(&self) -> Option<u32> {
        self.next_seq
    }

    /// Current buffer depth in frames
    pub fn depth(&self) -> usize {
        self.buffer.len()
    }

    /// Current target depth in frames
    pub fn target_depth(&self) -> u32 {
        self.target_depth
    }

    /// Current jitter estimate in ms
    pub fn jitter_ms(&self) -> f32 {
        self.jitter_estimate_ms
    }

    /// Reset the buffer state
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.next_seq = None;
        self.jitter_estimate_ms = 0.0;
        self.spike_peak_ms = 0.0;
        self.last_arrival_ms = None;
        self.last_packet_ts = None;
        self.initial_fill_remaining = 25;
        self.refilling = false;
    }

    /// Get the number of output samples per frame (for sizing decode buffers)
    pub fn frame_samples(&self) -> usize {
        FRAME_SAMPLES
    }
}

/// Check if sequence `a` comes before `b` with wrapping.
fn is_seq_before(a: u32, b: u32) -> bool {
    let diff = a.wrapping_sub(b);
    diff > 0x8000_0000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_frame(seq: u32) -> BufferedFrame {
        BufferedFrame {
            sequence: seq,
            timestamp: seq * 160, // 160 samples per 20ms frame at 8kHz
            opus_data: vec![seq as u8; 32],
            ptt: false,
        }
    }

    #[test]
    fn basic_push_pull() {
        let mut jb = JitterBuffer::new(2, 10);

        // Push 2 frames (target_depth = min_depth = 2)
        jb.push(make_frame(0), 0);
        jb.push(make_frame(1), 20);

        // Should be ready now
        match jb.pull() {
            JitterResult::Frame(f) => assert_eq!(f.sequence, 0),
            other => panic!("expected Frame, got {:?}", other),
        }
        match jb.pull() {
            JitterResult::Frame(f) => assert_eq!(f.sequence, 1),
            other => panic!("expected Frame, got {:?}", other),
        }
    }

    #[test]
    fn not_ready_until_filled() {
        let mut jb = JitterBuffer::new(3, 10);

        jb.push(make_frame(0), 0);
        assert!(matches!(jb.pull(), JitterResult::NotReady));

        jb.push(make_frame(1), 20);
        // Still not ready — target is min_depth=3
        assert!(matches!(jb.pull(), JitterResult::NotReady));

        jb.push(make_frame(2), 40);
        // Now ready (3 frames >= target 3)
        assert!(matches!(jb.pull(), JitterResult::Frame(_)));
    }

    #[test]
    fn out_of_order_reordering() {
        let mut jb = JitterBuffer::new(2, 10);

        // Push out of order — 5 frames to ensure buffer fills past target
        jb.push(make_frame(2), 0);
        jb.push(make_frame(0), 10);
        jb.push(make_frame(1), 20);
        jb.push(make_frame(3), 40);
        jb.push(make_frame(4), 60);

        match jb.pull() {
            JitterResult::Frame(f) => assert_eq!(f.sequence, 0),
            other => panic!("expected seq 0, got {:?}", other),
        }
        match jb.pull() {
            JitterResult::Frame(f) => assert_eq!(f.sequence, 1),
            other => panic!("expected seq 1, got {:?}", other),
        }
        match jb.pull() {
            JitterResult::Frame(f) => assert_eq!(f.sequence, 2),
            other => panic!("expected seq 2, got {:?}", other),
        }
    }

    #[test]
    fn missing_frame_detected() {
        let mut jb = JitterBuffer::new(2, 10);

        // Push frames 0, 1, 3 (skip 2)
        jb.push(make_frame(0), 0);
        jb.push(make_frame(1), 20);
        jb.push(make_frame(3), 40);

        let _ = jb.pull(); // seq 0
        let _ = jb.pull(); // seq 1

        // seq 2 is missing
        assert!(matches!(jb.pull(), JitterResult::Missing));
        assert_eq!(jb.stats_lost, 1);

        // seq 3 should be available
        assert!(matches!(jb.pull(), JitterResult::Frame(_)));
    }

    #[test]
    fn late_packet_dropped() {
        let mut jb = JitterBuffer::new(2, 10);

        jb.push(make_frame(0), 0);
        jb.push(make_frame(1), 20);
        jb.push(make_frame(2), 40);

        let _ = jb.pull(); // advances next_seq past 0
        let _ = jb.pull(); // advances past 1

        // Now push a late packet (seq 0)
        jb.push(make_frame(0), 60);
        assert_eq!(jb.stats_late, 1);
    }

    #[test]
    fn peek_opus_data() {
        let mut jb = JitterBuffer::new(2, 10);
        jb.push(make_frame(5), 0);

        let data = jb.peek_opus_data(5);
        assert!(data.is_some());
        assert_eq!(data.unwrap().len(), 32);

        assert!(jb.peek_opus_data(6).is_none());
    }

    #[test]
    fn reset_clears_state() {
        let mut jb = JitterBuffer::new(2, 10);
        jb.push(make_frame(0), 0);
        jb.push(make_frame(1), 20);
        jb.push(make_frame(2), 40);
        let _ = jb.pull();

        jb.reset();
        assert_eq!(jb.depth(), 0);
        assert!(matches!(jb.pull(), JitterResult::NotReady));
    }

    #[test]
    fn seq_wrapping() {
        assert!(is_seq_before(u32::MAX, 0));
        assert!(!is_seq_before(0, u32::MAX));
        assert!(is_seq_before(5, 10));
        assert!(!is_seq_before(10, 5));
    }

    #[test]
    fn jitter_adaptation() {
        let mut jb = JitterBuffer::new(2, 10);

        // Simulate high jitter: packets arrive at irregular intervals
        jb.push(make_frame(0), 0);
        jb.push(make_frame(1), 20);   // normal
        jb.push(make_frame(2), 80);   // 60ms late
        jb.push(make_frame(3), 90);   // only 10ms gap
        jb.push(make_frame(4), 150);  // 60ms late again

        // Jitter estimate should have increased
        assert!(jb.jitter_ms() > 0.0, "jitter should be > 0, got {}", jb.jitter_ms());
    }
}
