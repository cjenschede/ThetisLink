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
    /// True when the packet header had `Flags::AUDIO_WIDEBAND` — on
    /// pop this selects the correct Opus decoder path
    /// (wideband 16 kHz vs narrowband 8 kHz). Default false so that
    /// existing callers that do not support WB remain unchanged.
    pub wideband: bool,
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
    /// Peak jitter hold for spike resilience (fast attack, decay over
    /// `SPIKE_DECAY_PACKETS`)
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
    /// Frames thrown away on arrival because the buffer held more than its
    /// hard cap (`max_depth + 10`), the oldest first (build 183).
    pub stats_capped: u64,
    /// Frames thrown away while playing to catch up above the target (build 183).
    pub stats_trimmed: u64,
    /// Frames the buffer stopped waiting for: a gap larger than `max_depth`, or
    /// a gap in front of a frame the cap or the trim took away (builds 183,
    /// 184). Not counted as lost; one that comes later is `stats_late`.
    pub stats_skipped: u64,
    /// Frames emptied out unplayed: `reset`, `discard`, and a restart of the
    /// sequence (build 184).
    pub stats_cleared: u64,
    /// Grace period after init/reset: suppress overflow recovery
    initial_fill_remaining: u32,
    /// Underflow recovery: when buffer drains to empty, pause until refilled to target
    refilling: bool,
    /// Pulls since the last frame dropped for being above target
    /// (`TRIM_EVERY_PULLS`).
    pulls_since_trim: u32,
    /// A transmit buffer (`for_transmit`): after a frame without PTT nothing
    /// more comes, so such a frame in the buffer ends the wait for the target
    /// (build 237). Kept by `reset`: it is what the buffer is for.
    ends_with_release: bool,
    /// The numbers behind the target and the trim; `Tuning::default()` is the
    /// buffer as it runs. Kept by `reset` and `discard`.
    tuning: Tuning,
}

/// The numbers that set the buffer's target and how it sheds a lead, as fields
/// so the replay can try others (build 255, step C). `Default` is a receive
/// buffer as it runs - the constants below; `transmit` is the transmit buffer's
/// (build 258). Nothing outside the replay sets another.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tuning {
    /// ms of jitter per frame of target: the target is `jitter / this + target_add`.
    pub ms_per_frame: f32,
    pub target_add: u32,
    /// `SPIKE_DECAY_PACKETS`.
    pub spike_decay_packets: f32,
    /// Frames above `max_depth` before the oldest is thrown away on arrival.
    pub cap_above_max: u32,
    /// Frames above the target before the trim starts.
    pub trim_above_target: i32,
    /// `TRIM_EVERY_PULLS` and `FAST_TRIM_ABOVE`.
    pub trim_every_pulls: u32,
    pub fast_trim_above: i32,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            ms_per_frame: TARGET_MS_PER_FRAME,
            target_add: TARGET_ADD,
            spike_decay_packets: SPIKE_DECAY_PACKETS,
            cap_above_max: CAP_ABOVE_MAX,
            trim_above_target: TRIM_ABOVE_TARGET,
            trim_every_pulls: TRIM_EVERY_PULLS,
            fast_trim_above: FAST_TRIM_ABOVE,
        }
    }
}

impl Tuning {
    /// The transmit buffer's numbers: tuning B of the drives of 2026-10-18 (owner
    /// 2026-10-19: "B mag ook mee in stap 5"; build 258) - a target of `jitter /
    /// 30 ms + 2`, a stall's peak fading over 115 packets - and no slow trim of
    /// its own: the server's playout tick catches up in the pauses of speech
    /// instead (`pause_trim.rs`). The fast trim and the cap stay.
    pub fn transmit() -> Self {
        Tuning {
            ms_per_frame: TX_TARGET_MS_PER_FRAME,
            spike_decay_packets: TX_SPIKE_DECAY_PACKETS,
            trim_every_pulls: u32::MAX,
            ..Tuning::default()
        }
    }
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
            stats_capped: 0,
            stats_trimmed: 0,
            stats_skipped: 0,
            stats_cleared: 0,
            initial_fill_remaining: 25,
            refilling: false,
            pulls_since_trim: 0,
            ends_with_release: false,
            tuning: Tuning::default(),
        }
    }

    /// The same buffer with other numbers (the replay, build 255).
    pub fn with_tuning(self, tuning: Tuning) -> Self {
        Self { tuning, ..self }
    }

    /// A buffer for a radio's transmit audio, whose release is played out:
    /// it does not wait for its target while it holds the end of a
    /// transmission (`short_of_target`), and it has the transmit numbers
    /// (`Tuning::transmit`, build 258). The receive buffers keep `new`.
    pub fn for_transmit(min_depth: u32, max_depth: u32) -> Self {
        Self { ends_with_release: true, tuning: Tuning::transmit(), ..Self::new(min_depth, max_depth) }
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

                // Spike peak hold: instant attack, exponential decay over
                // `SPIKE_DECAY_PACKETS` (~10 s back near the minimum).
                if deviation > self.spike_peak_ms {
                    self.spike_peak_ms = deviation; // instant jump to spike level
                } else {
                    self.spike_peak_ms *= 1.0 - (1.0 / self.tuning.spike_decay_packets);
                }

                // Target depth: use the higher of normal jitter or spike peak
                let jitter_for_target = self.jitter_estimate_ms.max(self.spike_peak_ms);
                let desired = (jitter_for_target / self.tuning.ms_per_frame) as u32 + self.tuning.target_add;
                self.target_depth = desired.clamp(self.min_depth, self.max_depth);
            }
        }
        self.last_arrival_ms = Some(arrival_ms);
        self.last_packet_ts = Some(frame.timestamp);

        // Drop packets that arrived too late - UNLESS they are so far behind that
        // the sender must have restarted its sequence. Without this, a fresh
        // low-sequence stream is discarded as "too late" against a stale high
        // next_seq and stays silent until the sender's sequence climbs back.
        //
        // The threshold is a REORDER window, not a restart size. It used to be
        // 1000 frames, on the assumption that "a restart is thousands behind".
        // That holds for a server restart but not for a subscription: a VRX
        // channel switched off and on starts at 0 again, so it is only as far
        // behind as the previous session was long. Listening for four seconds
        // meant the next enable discarded exactly four seconds of audio, while
        // listening past twenty seconds crossed the old threshold and felt
        // instant - the whole difference between "sometimes direct, sometimes
        // three seconds".
        //
        // 64 frames is 1.3 s at 20 ms: orders of magnitude beyond real
        // reordering on any link this runs on, and anything further behind is a
        // restart by definition. Being wrong here is cheap - a re-baseline
        // clears the buffer and playout resumes.
        const STREAM_RESTART_BACKJUMP: u32 = 64;
        if let Some(next) = self.next_seq {
            if is_seq_before(frame.sequence, next) {
                if next.wrapping_sub(frame.sequence) > STREAM_RESTART_BACKJUMP {
                    log::info!(
                        "Jitter buffer stream restart: seq {} << next {} — re-baselining",
                        frame.sequence, next
                    );
                    self.stats_cleared += self.buffer.len() as u64;
                    self.buffer.clear();
                    self.next_seq = None;
                    self.refilling = false;
                } else {
                    self.stats_late += 1;
                    return;
                }
            }
        }

        self.buffer.insert(frame.sequence, frame);

        // Limit buffer size: hard cap to prevent unbounded growth
        let hard_limit = self.max_depth as usize + self.tuning.cap_above_max as usize;
        while self.buffer.len() > hard_limit {
            if let Some((oldest, _)) = self.buffer.pop_first() {
                self.stats_capped += 1;
                // Past it, as the trim does: otherwise the gap it leaves plays
                // as a lost frame - concealed audio for a frame thrown away on
                // purpose, and counted twice (review of 183).
                self.step_past(oldest);
            }
        }
    }

    /// Pull the next frame for playout.
    pub fn pull(&mut self) -> JitterResult {
        // Initialize next_seq on first pull if we have data
        if self.next_seq.is_none() {
            if self.short_of_target() {
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
            if !self.short_of_target() {
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

        // Overflow recovery: above target+2, drop one frame - at most one per
        // `TRIM_EVERY_PULLS`, so a grown buffer comes down in small steps
        // (build 173; it was one or two every pull, 20-40 ms at a time); far
        // above it (`FAST_TRIM_ABOVE`) one every pull (build 174).
        // Suppressed during initial fill grace period (first 500ms after reset).
        // IMPORTANT: when dropping, also advance next_seq to avoid Missing for
        // frames we intentionally discarded. Without this, every overflow drop
        // triggers PLC which progressively degrades audio quality.
        self.pulls_since_trim = self.pulls_since_trim.saturating_add(1);
        if self.initial_fill_remaining == 0 {
            let excess = self.buffer.len() as i32 - self.target_depth as i32 - self.tuning.trim_above_target;
            let drops = if excess > self.tuning.fast_trim_above
                || (excess > 0 && self.pulls_since_trim >= self.tuning.trim_every_pulls)
            {
                1
            } else {
                0
            };
            if drops > 0 {
                self.pulls_since_trim = 0;
            }
            for _ in 0..drops {
                if let Some((&oldest, _)) = self.buffer.first_key_value() {
                    self.buffer.pop_first();
                    self.stats_trimmed += 1;
                    // Advance next_seq past the dropped frame to avoid
                    // a spurious Missing result on the next pull
                    self.step_past(oldest);
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
                        self.stats_skipped += first_buffered.wrapping_sub(seq) as u64;
                        self.next_seq = Some(first_buffered);
                        return JitterResult::NotReady;
                    }
                }
                self.stats_lost += 1;
                JitterResult::Missing
            }
        }
    }

    /// Whether playout still waits for more frames: below the target - unless
    /// this is a transmit buffer that holds the end of a transmission. After a
    /// frame without PTT the sender sends nothing more, so the target would
    /// never come and the release would lie here until the safety net let go
    /// 2 s later: five of eight safety releases on two drives (field test
    /// 2026-10-17; build 237).
    fn short_of_target(&self) -> bool {
        if self.buffer.len() >= self.target_depth as usize {
            return false;
        }
        if self.ends_with_release && self.buffer.values().any(|f| !f.ptt) {
            // Named in advance (working rule 11, build 237).
            log::info!(
                "Jitter buffer: the end of a transmission is in hand - playing {} frame(s) below the target of {}",
                self.buffer.len(),
                self.target_depth
            );
            return false;
        }
        true
    }

    /// Peek at the opus data of a specific sequence number (for FEC decoding).
    /// Returns None if the packet isn't buffered.
    pub fn peek_opus_data(&self, seq: u32) -> Option<&[u8]> {
        self.buffer.get(&seq).map(|f| f.opus_data.as_slice())
    }

    /// Peek at a frame's payload together with its format.
    ///
    /// Error correction rebuilds a lost frame from redundancy carried inside the
    /// NEXT packet, so it has to decode in that packet's format - and
    /// `peek_opus_data` drops exactly that. It handed back only the bytes, so
    /// every caller had to guess the width, and every caller guessed narrowband
    /// (2026-08-16).
    pub fn peek_frame(&self, seq: u32) -> Option<(&[u8], bool)> {
        self.buffer.get(&seq).map(|f| (f.opus_data.as_slice(), f.wideband))
    }

    /// Peek at the next expected sequence number (for FEC look-ahead).
    pub fn next_seq_peek(&self) -> Option<u32> {
        self.next_seq
    }

    /// Past a frame taken away on purpose (cap or trim): the next frame to play
    /// is the one after it, and the frames still missing in front of it are
    /// given up - counted as skipped (build 184; review of 183: they vanished
    /// uncounted).
    fn step_past(&mut self, taken: u32) {
        if let Some(ref mut next) = self.next_seq {
            if !is_seq_before(taken, *next) {
                self.stats_skipped += taken.wrapping_sub(*next) as u64;
                *next = taken.wrapping_add(1);
            }
        }
    }

    /// Does a frame with the PTT flag wait in the buffer? A transmit buffer
    /// asks it when it plays a release (build 283): a press already behind it
    /// makes that release no release.
    pub fn holds_a_press(&self) -> bool {
        self.buffer.values().any(|f| f.ptt)
    }

    /// Empty the buffer unplayed, counted as cleared (build 184): for a new
    /// transmission that must not start with the end of the last one. What is
    /// learned about the line stays, as with `drain_all`.
    pub fn discard(&mut self) {
        self.stats_cleared += self.buffer.len() as u64;
        self.buffer.clear();
        self.next_seq = None;
    }

    /// Everything buffered, in sequence order, and the buffer empty - for a
    /// transmit buffer that must play out at once when the operator lets go
    /// (build 166). The next push starts as after a reset.
    pub fn drain_all(&mut self) -> Vec<BufferedFrame> {
        let frames = std::mem::take(&mut self.buffer).into_values().collect();
        self.next_seq = None;
        frames
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
        self.stats_cleared += self.buffer.len() as u64;
        self.buffer.clear();
        self.next_seq = None;
        self.jitter_estimate_ms = 0.0;
        self.spike_peak_ms = 0.0;
        self.last_arrival_ms = None;
        self.last_packet_ts = None;
        self.initial_fill_remaining = 25;
        self.refilling = false;
        self.pulls_since_trim = 0;
    }

    /// Get the number of output samples per frame (for sizing decode buffers)
    pub fn frame_samples(&self) -> usize {
        FRAME_SAMPLES
    }
}

/// ms of jitter per frame of target, and the frames added on top: the target
/// is `jitter / TARGET_MS_PER_FRAME + TARGET_ADD`, clamped to min and max.
pub const TARGET_MS_PER_FRAME: f32 = 15.0;
pub const TARGET_ADD: u32 = 2;
/// The depth of the server's Thetis transmit buffer, in 20 ms frames
/// (`for_transmit`, tuning B, build 258). One source for the buffer, its start
/// line and the replayer (build 260; the line had "2-10" as text).
pub const TX_MIN_DEPTH: u32 = 2;
pub const TX_MAX_DEPTH: u32 = 10;
/// The transmit buffer's (`Tuning::transmit`, build 258): half the target for
/// the same jitter, and a stall forgotten in half the time.
pub const TX_TARGET_MS_PER_FRAME: f32 = 30.0;
pub const TX_SPIKE_DECAY_PACKETS: f32 = 115.0;

/// Frames above `max_depth` the buffer holds before it throws the oldest away on
/// arrival (`stats_capped`, build 183).
pub const CAP_ABOVE_MAX: u32 = 10;

/// Frames above the target before the trim starts (`TRIM_EVERY_PULLS`).
pub const TRIM_ABOVE_TARGET: i32 = 2;

/// How many packets a stall's peak takes to fade by a factor e: ~230
/// packets is ~4.6 s at 50 a second, so a 270 ms stall is down to a few
/// frames within ten seconds of a steady line (owner, 2026-10-11; build 173).
/// It was 3000, about a minute.
pub const SPIKE_DECAY_PACKETS: f32 = 230.0;

/// A buffer above its target drops at most one frame per this many pulls -
/// ~250 ms at one pull per 20 ms tick (half that while the client's ring is
/// low and it pulls two a tick), 20 ms gone each time, which the owner
/// expects not to be heard (2026-10-11; build 173). It was one or two every
/// pull.
pub const TRIM_EVERY_PULLS: u32 = 12;

/// More than this many frames above target + 2, the buffer is trimmed one
/// frame every pull as before build 173: a big lead with a low target - the
/// sound card took nothing for a moment, or a sender caught up in a burst -
/// is not a stall the target followed, and would otherwise take ~10 s to
/// leave (reviews of 173; owner: "ja"; build 174).
pub const FAST_TRIM_ABOVE: i32 = 6;

/// Check if sequence `a` comes before `b` with wrapping.
fn is_seq_before(a: u32, b: u32) -> bool {
    let diff = a.wrapping_sub(b);
    diff > 0x8000_0000
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the buffer throws away is counted, each way apart (build 183): a
    /// burst above the hard cap on arrival, a trim while playing above the
    /// target, and a gap too large to wait for.
    #[test]
    fn what_the_buffer_throws_away_is_counted() {
        // On arrival: forty frames at once into a buffer that keeps thirty.
        let mut jb = JitterBuffer::new(3, 20);
        for seq in 0..40 {
            jb.push(make_frame(seq), 0);
        }
        assert_eq!(jb.depth(), 30);
        assert_eq!((jb.stats_capped, jb.stats_trimmed, jb.stats_skipped), (10, 0, 0));
        // While playing: past the grace period, far above the target, one
        // frame goes per pull.
        for _ in 0..30 {
            let _ = jb.pull();
        }
        assert!(jb.stats_trimmed > 0, "nothing trimmed above the target");
        assert_eq!(jb.stats_capped, 10, "the cap counts on arrival only");
        // A gap larger than max_depth: skipped over, not lost.
        let mut jb = JitterBuffer::new(3, 20);
        for seq in 0..3 {
            jb.push(make_frame(seq), seq as u64 * 20);
        }
        for _ in 0..3 {
            let _ = jb.pull();
        }
        jb.push(make_frame(50), 1000);
        let lost = jb.stats_lost;
        let _ = jb.pull();
        assert_eq!(jb.stats_skipped, 47, "frames 3..=49 never arrived");
        assert_eq!(jb.stats_lost, lost, "a skipped gap is not counted as lost");
    }

    /// The cap steps past what it takes away, so a burst in the first pulls
    /// after a reset does not play its dropped frames as lost ones - counted
    /// once, not twice (build 184; review of 183, measured: capped 15 and lost
    /// 15).
    #[test]
    fn a_capped_frame_is_counted_once_and_not_played_as_lost() {
        let mut jb = JitterBuffer::new(3, 20);
        for seq in 0..3 {
            jb.push(make_frame(seq), seq as u64 * 20);
        }
        for _ in 0..3 {
            let _ = jb.pull();
        }
        // Still in the grace period: no trim. A burst of 45 after a stall.
        for seq in 3..48 {
            jb.push(make_frame(seq), 2000);
        }
        assert_eq!(jb.stats_capped, 15);
        match jb.pull() {
            JitterResult::Frame(f) => assert_eq!(f.sequence, 18, "the first frame the cap kept"),
            other => panic!("expected frame 18, got {:?}", other),
        }
        assert_eq!((jb.stats_lost, jb.stats_skipped), (0, 0));
    }

    /// A trim in front of a gap: the missing frames before the one it takes
    /// are given up and counted as skipped, not lost without a trace (build
    /// 184; review of 183: 3..=5 vanished).
    #[test]
    fn a_trim_over_a_gap_counts_the_frames_it_gives_up() {
        // A steady line past the grace period, played until empty.
        let mut jb = JitterBuffer::new(3, 20);
        let mut seq = 0u32;
        for _ in 0..40 {
            jb.push(make_frame(seq), seq as u64 * 20);
            seq += 1;
            let _ = jb.pull();
        }
        while let JitterResult::Frame(_) = jb.pull() {}
        // Then seq..seq+3 never come, and the next fifteen do, on time.
        for s in seq + 3..seq + 18 {
            jb.push(make_frame(s), s as u64 * 20);
        }
        let skipped = jb.stats_skipped;
        let _ = jb.pull();
        assert!(jb.stats_trimmed > 0, "the burst is trimmed");
        assert_eq!(jb.stats_skipped - skipped, 3, "the three in front given up");
    }

    /// Emptied unplayed is counted, whichever way (build 184).
    #[test]
    fn emptying_the_buffer_unplayed_is_counted() {
        let mut jb = JitterBuffer::new(3, 20);
        for seq in 0..5 {
            jb.push(make_frame(seq), seq as u64 * 20);
        }
        let _ = jb.pull(); // playing: next_seq stands at 1
        jb.discard();
        assert_eq!((jb.stats_cleared, jb.depth()), (4, 0));
        // The next transmission starts a few numbers on (the release burst
        // used some): it plays from its first frame, with no gap behind it
        // (review of 184, U15).
        let lost = jb.stats_lost;
        for seq in 9..12 {
            jb.push(make_frame(seq), seq as u64 * 20);
        }
        match jb.pull() {
            JitterResult::Frame(f) => assert_eq!(f.sequence, 9),
            other => panic!("expected frame 9, got {:?}", other),
        }
        assert_eq!(jb.stats_lost, lost, "no gap left from the last transmission");
        let _ = jb.drain_all();
        let cleared = jb.stats_cleared;
        for seq in 50..54 {
            jb.push(make_frame(seq), 0);
        }
        jb.reset();
        assert_eq!(jb.stats_cleared, cleared + 4);
        // A restart of the sequence: playing from 200, then 10 arrives.
        for seq in 200..204 {
            jb.push(make_frame(seq), 0);
        }
        let _ = jb.pull();
        let before = jb.stats_cleared;
        let held = jb.depth() as u64;
        jb.push(make_frame(10), 0);
        assert_eq!(jb.stats_cleared - before, held);
        // drain_all plays it out: not cleared.
        let before = jb.stats_cleared;
        let _ = jb.drain_all();
        assert_eq!(jb.stats_cleared, before);
    }

    fn make_frame(seq: u32) -> BufferedFrame {
        BufferedFrame {
            // Wire protocol uses milliseconds for `timestamp`
            // (`start.elapsed().as_millis()` on the server side, see
            // `audio_loops.rs`). Each frame represents 20 ms of audio,
            // so timestamp delta between consecutive frames is 20 — not
            // 160 (the sample count). Using samples here previously
            // injected a 140 ms phantom jitter into RFC-3550 estimator
            // and made every test push min_depth up to max_depth.
            sequence: seq,
            timestamp: seq * 20,
            opus_data: vec![seq as u8; 32],
            ptt: false,
            wideband: false,
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
    fn resubscribe_restart_is_not_treated_as_late() {
        // A VRX channel switched off and on starts its sequence at 0 again, only
        // as far behind as the previous session was long - not "thousands", which
        // is what the old threshold assumed. Four seconds of listening used to
        // cost four seconds of silence on the next enable.
        let mut jb = JitterBuffer::new(3, 40);
        for seq in 0..200u32 {
            jb.push(make_frame(seq), seq as u64 * 20);
        }
        while !matches!(jb.pull(), JitterResult::NotReady) {}

        // Sender restarts at 0 after a re-subscribe.
        jb.push(make_frame(0), 5_000);
        jb.push(make_frame(1), 5_020);
        jb.push(make_frame(2), 5_040);
        jb.push(make_frame(3), 5_060);

        let mut got = 0;
        for _ in 0..8 {
            if let JitterResult::Frame(_) = jb.pull() {
                got += 1;
            }
        }
        assert!(got > 0, "a restarted stream must play, not wait for the old sequence");
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
    fn small_backward_still_late_dropped() {
        // A packet only slightly behind next_seq is a genuine late/reordered packet
        // and must still be dropped — NOT treated as a stream restart (build 15).
        let mut jb = JitterBuffer::new(2, 10);
        jb.push(make_frame(100), 0);
        jb.push(make_frame(101), 20);
        let _ = jb.pull();
        let _ = jb.pull(); // next_seq now 102
        jb.push(make_frame(100), 40); // 2 behind → late
        assert_eq!(jb.stats_late, 1);
        assert!(!jb.buffer.contains_key(&100));
    }

    #[test]
    fn large_backjump_rebaselines() {
        // A packet far behind next_seq signals a sender restart → re-baseline
        // (clear + next_seq=None) and keep the fresh packet, not drop it (build 15).
        let mut jb = JitterBuffer::new(2, 10);
        jb.push(make_frame(5000), 0);
        jb.push(make_frame(5001), 20);
        let _ = jb.pull();
        let _ = jb.pull(); // next_seq now 5002
        jb.push(make_frame(2), 40); // 5000 behind → stream restart
        assert_eq!(jb.stats_late, 0, "restart packet must not be counted late");
        assert!(jb.buffer.contains_key(&2), "fresh packet re-baselined into buffer");
        assert!(jb.next_seq.is_none(), "next_seq reset for re-init on next pull");
    }

    #[test]
    fn wraparound_late_not_rebaselined() {
        // A u32 sequence wrap (MAX -> 0) is a normal forward step; a packet just
        // before the wrap is late (small backward), not a restart. Build frames
        // manually — make_frame's `seq * 20` timestamp overflows near u32::MAX.
        let f = |seq: u32, ts: u32| BufferedFrame {
            sequence: seq,
            timestamp: ts,
            opus_data: vec![0u8; 32],
            ptt: false,
            wideband: false,
        };
        let mut jb = JitterBuffer::new(2, 10);
        jb.push(f(u32::MAX - 1, 0), 0);
        jb.push(f(u32::MAX, 20), 20);
        let _ = jb.pull();
        let _ = jb.pull(); // next_seq now 0 (wrapped)
        jb.push(f(u32::MAX, 40), 40); // 1 behind across wrap → late, not restart
        assert_eq!(jb.stats_late, 1);
        assert!(jb.next_seq.is_some(), "wrap-late must not trigger re-baseline");
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

    /// `drain_all` hands over everything in order and leaves an empty buffer
    /// that starts over (build 166).
    #[test]
    fn drain_all_hands_over_everything_in_order() {
        let mut jb = JitterBuffer::new(3, 20);
        for seq in [5u32, 3, 4] {
            jb.push(make_frame(seq), seq as u64 * 20);
        }
        let seqs: Vec<u32> = jb.drain_all().iter().map(|f| f.sequence).collect();
        assert_eq!(seqs, vec![3, 4, 5]);
        assert_eq!(jb.depth(), 0);
        assert!(matches!(jb.pull(), JitterResult::NotReady));
    }

    /// One stall raises the target; it is back near the minimum within about
    /// ten seconds of a steady line, not a minute (owner, 2026-10-11: "na bv
    /// 10 sec de buffer weer bijna leeg"; build 173).
    #[test]
    fn a_raised_target_comes_down_within_ten_seconds() {
        let mut jb = JitterBuffer::new(3, 40);
        let mut seq = 0u32;
        let mut push = |jb: &mut JitterBuffer, arrival: u64| {
            jb.push(BufferedFrame { sequence: seq, timestamp: seq * 20, opus_data: vec![0], ptt: true, wideband: true }, arrival);
            seq += 1;
        };
        for i in 0..50u64 {
            push(&mut jb, i * 20);
        }
        // One packet 270 ms late, the rest on time again.
        push(&mut jb, 50 * 20 + 270);
        assert!(jb.target_depth() >= 15, "the stall raised it: {}", jb.target_depth());
        // Right after the stall, a buffer a few frames above its high target
        // comes down in steps, not one a pull: the fast strip hangs on the
        // target, not on a fixed depth (review of 174, J12).
        {
            let mut probe = JitterBuffer::new(3, 40);
            let mut q = 0u32;
            let f = |q: u32| BufferedFrame { sequence: q, timestamp: q * 20, opus_data: vec![0], ptt: true, wideband: true };
            for i in 0..50u32 {
                probe.push(f(q), i as u64 * 20);
                q += 1;
            }
            probe.push(f(q), 50 * 20 + 270);
            q += 1;
            let target = probe.target_depth() as usize;
            while probe.depth() > target + 4 {
                let _ = probe.pull();
            }
            while probe.initial_fill_remaining > 0 || probe.depth() < target + 4 {
                let _ = probe.pull();
                probe.push(f(q), q as u64 * 20 + 270);
                q += 1;
                probe.push(f(q), q as u64 * 20 + 270);
                q += 1;
            }
            let before = probe.depth();
            for _ in 0..(2 * TRIM_EVERY_PULLS) {
                let _ = probe.pull();
                probe.push(f(q), q as u64 * 20 + 270);
                q += 1;
            }
            assert!(before - probe.depth() <= 2, "at most two frames in two periods: {} -> {}", before, probe.depth());
        }
        for i in 51..(51 + 250u64) {
            push(&mut jb, i * 20);
        }
        // Not faster than meant either: after five seconds still well up
        // (review of 173, J1c).
        assert!(jb.target_depth() >= 7, "five seconds later: {}", jb.target_depth());
        for i in (51 + 250)..(51 + 500u64) {
            push(&mut jb, i * 20);
        }
        assert!(jb.target_depth() <= 4, "ten seconds later: {}", jb.target_depth());
        assert_eq!(SPIKE_DECAY_PACKETS, 230.0);
    }

    /// A buffer above its target is brought down one frame per
    /// `TRIM_EVERY_PULLS`, not one or two every pull (build 173).
    #[test]
    fn an_overfull_buffer_is_trimmed_in_small_steps() {
        let mut jb = JitterBuffer::new(3, 40);
        let frame = |seq: u32| BufferedFrame { sequence: seq, timestamp: seq * 20, opus_data: vec![0], ptt: true, wideband: true };
        // A lead just above target + 2, within the small steps (target 3: 11
        // frames is 6 above 5), then a steady line: one in for every one out.
        for seq in 0..11u32 {
            jb.push(frame(seq), seq as u64 * 20);
        }
        let mut next = 11u32;
        let mut step = |jb: &mut JitterBuffer| {
            let played = matches!(jb.pull(), JitterResult::Frame(_));
            jb.push(frame(next), next as u64 * 20);
            next += 1;
            played
        };
        while jb.initial_fill_remaining > 0 {
            assert!(step(&mut jb));
        }
        let depth_before = jb.depth();
        for _ in 0..(2 * TRIM_EVERY_PULLS) {
            assert!(step(&mut jb), "every pull still plays a frame");
        }
        let trimmed = depth_before - jb.depth();
        assert!((1..=2).contains(&trimmed), "two trim periods, one or two frames dropped: {trimmed}");
        // And it does come all the way down, in steps.
        for _ in 0..(40 * TRIM_EVERY_PULLS) {
            step(&mut jb);
        }
        assert!(jb.depth() <= jb.target_depth() as usize + 3, "down near the target: {}", jb.depth());
        assert_eq!(TRIM_EVERY_PULLS, 12);
    }

    /// A big lead with a low target - the sound card took nothing for a
    /// moment - leaves at one frame a pull above `FAST_TRIM_ABOVE`, not in
    /// steps of 250 ms (reviews of 173, probe A: 46 frames, back in ~2 s;
    /// build 174).
    #[test]
    fn a_big_lead_with_a_low_target_leaves_quickly() {
        let mut jb = JitterBuffer::new(3, 40);
        let frame = |seq: u32| BufferedFrame { sequence: seq, timestamp: seq * 20, opus_data: vec![0], ptt: true, wideband: true };
        for seq in 0..46u32 {
            jb.push(frame(seq), seq as u64 * 20);
        }
        let mut next = 46u32;
        for n in 0..100 {
            let before = jb.depth();
            assert!(matches!(jb.pull(), JitterResult::Frame(_)), "every pull still plays");
            jb.push(frame(next), next as u64 * 20);
            next += 1;
            // One frame at a time, never a jump of 40 ms (review of 174, F4).
            assert!(before - jb.depth() <= 1, "pull {n}: {before} -> {}", jb.depth());
            // After the 25-pull grace of a new buffer, 35 pulls are enough
            // at one a pull; in steps they would take 35 x 12.
            if n == 60 {
                assert!(jb.depth() <= 3 + 2 + FAST_TRIM_ABOVE as usize + 1, "fast, not in steps (F2): {}", jb.depth());
            }
        }
        assert_eq!(jb.target_depth(), 3);
        assert!(jb.depth() <= 3 + 2 + FAST_TRIM_ABOVE as usize, "two seconds later: {}", jb.depth());
        assert_eq!(FAST_TRIM_ABOVE, 6);
    }

    /// A receive buffer as it runs is `Tuning::default()`, and that is the
    /// shipped numbers, written out by value (build 255): the replay can try
    /// others, nothing in the field does.
    #[test]
    fn the_tuning_as_it_runs_is_the_shipped_numbers() {
        let t = Tuning::default();
        assert_eq!(
            (t.ms_per_frame, t.target_add, t.spike_decay_packets, t.cap_above_max),
            (15.0, 2, 230.0, 10)
        );
        assert_eq!((t.trim_above_target, t.trim_every_pulls, t.fast_trim_above), (2, 12, 6));
        assert_eq!(JitterBuffer::new(3, 20).tuning, t);
    }

    /// The transmit buffer has its own numbers, by value (build 258): tuning B
    /// and no slow trim; the rest as a receive buffer. The receive buffers keep
    /// theirs.
    #[test]
    fn the_transmit_buffer_has_its_own_numbers() {
        let t = Tuning::transmit();
        assert_eq!((t.ms_per_frame, t.spike_decay_packets, t.trim_every_pulls), (30.0, 115.0, u32::MAX));
        assert_eq!(t, Tuning { ms_per_frame: 30.0, spike_decay_packets: 115.0, trim_every_pulls: u32::MAX, ..Tuning::default() });
        assert_eq!(JitterBuffer::for_transmit(2, 10).tuning, t);
        assert_eq!(JitterBuffer::new(3, 40).tuning, Tuning::default(), "the receive buffers");
        // the depth by value: the server and the replayer read these names (round 260)
        assert_eq!((TX_MIN_DEPTH, TX_MAX_DEPTH), (2, 10));
    }

    /// No slow trim on the transmit buffer: three frames above the target stay
    /// there however long it plays, where a receive buffer comes down; ten
    /// above, the fast trim still takes them (build 258).
    #[test]
    fn the_transmit_buffer_leaves_the_catching_up_to_the_pauses() {
        let frame = |seq: u32| BufferedFrame { sequence: seq, timestamp: seq * 20, opus_data: vec![0], ptt: true, wideband: true };
        let lead_after = |mut jb: JitterBuffer, lead: u32| {
            for seq in 0..20u32 {
                jb.push(frame(seq), seq as u64 * 20);
                let _ = jb.pull();
            }
            let target = jb.target_depth();
            let mut seq = 20u32;
            for _ in 0..(target + lead) {
                jb.push(frame(seq), seq as u64 * 20);
                seq += 1;
            }
            for _ in 0..200 {
                jb.push(frame(seq), seq as u64 * 20);
                seq += 1;
                let _ = jb.pull();
            }
            jb.depth() as i64 - jb.target_depth() as i64
        };
        assert!(lead_after(JitterBuffer::for_transmit(2, 20), 4) >= 3, "the transmit buffer keeps its lead");
        assert!(lead_after(JitterBuffer::new(2, 20), 4) <= 2, "a receive buffer trims it");
        assert!(lead_after(JitterBuffer::for_transmit(2, 20), 12) <= 9, "the fast trim stays");
    }

    /// Another tuning changes what it names, and survives a reset and a discard:
    /// a stream with ~60 ms of jitter asks 6 frames at 15 ms a frame and 4 at 30;
    /// a cap of 2 above the maximum throws away what a cap of 10 keeps.
    #[test]
    fn a_tuning_sets_the_target_and_the_cap_and_stays_through_a_reset() {
        let frame = |seq: u32| BufferedFrame { sequence: seq, timestamp: seq * 20, opus_data: vec![0], ptt: true, wideband: true };
        let jittery = |mut jb: JitterBuffer| {
            for seq in 0..200u32 {
                let late = if seq % 2 == 0 { 0 } else { 60 };
                jb.push(frame(seq), seq as u64 * 20 + late);
                while matches!(jb.pull(), JitterResult::Frame(_)) {}
            }
            jb.target_depth()
        };
        let half = Tuning { ms_per_frame: 30.0, ..Tuning::default() };
        let (as_shipped, halved) = (jittery(JitterBuffer::new(3, 20)), jittery(JitterBuffer::new(3, 20).with_tuning(half)));
        assert!(as_shipped > halved && halved >= 3, "{as_shipped} against {halved}");
        let mut jb = JitterBuffer::for_transmit(3, 20).with_tuning(half);
        jb.reset();
        jb.discard();
        assert_eq!(jb.tuning, half, "kept by reset and discard");
        let capped = |cap: u32| {
            let mut jb = JitterBuffer::for_transmit(3, 5).with_tuning(Tuning { cap_above_max: cap, ..Tuning::default() });
            for seq in 0..20u32 {
                jb.push(frame(seq), 0);
            }
            jb.stats_capped
        };
        assert_eq!((capped(10), capped(2)), (5, 13), "a cap of max + 10 and of max + 2");
    }

    /// Why a silence inside a press empties the transmit buffer (server build
    /// 249; round 248): frames that waited under the target when the line went
    /// play first once a release frame comes after the silence - the oldest
    /// key first, with PTT - and key the transmitter with old speech. Emptied
    /// before the release goes in, nothing old plays. Both ways (rule 13).
    #[test]
    fn frames_waiting_under_the_target_play_first_unless_the_buffer_is_emptied() {
        let f = |seq: u32, ptt: bool| BufferedFrame { sequence: seq, timestamp: seq * 20, opus_data: vec![0], ptt, wideband: true };
        let first_after_the_silence = |empty: bool| {
            let mut jb = JitterBuffer::for_transmit(3, 20);
            jb.push(f(100, true), 0);
            jb.push(f(101, true), 20);
            assert!(matches!(jb.pull(), JitterResult::NotReady), "under the target: waiting");
            if empty {
                jb.discard();
            }
            jb.push(f(227, false), 2_520);
            match jb.pull() {
                JitterResult::Frame(fr) => Some((fr.sequence, fr.ptt)),
                _ => None,
            }
        };
        assert_eq!(first_after_the_silence(false), Some((100, true)), "the old frame, with PTT");
        assert_eq!(first_after_the_silence(true), Some((227, false)), "only the release");
    }

    /// A transmit buffer plays the end of a transmission without waiting for
    /// its target (build 237). On a drive the buffer ran dry in a gap, then
    /// 9 and 7 frames came at once with the release in them, below a target
    /// of 10 and 16; nothing more came, and the release waited for the
    /// safety net (field test 2026-10-17).
    #[test]
    fn a_transmit_buffer_plays_the_end_of_a_transmission_below_its_target() {
        let f = |seq: u32, ptt: bool| BufferedFrame { sequence: seq, timestamp: seq * 20, opus_data: vec![0], ptt, wideband: true };
        // Playing, run dry, then a short burst; `release` frames of it without PTT.
        let after_a_gap = |mut jb: JitterBuffer, release: u32| {
            for seq in 0..12 {
                jb.push(f(seq, true), seq as u64 * 20);
            }
            for _ in 0..12 {
                assert!(matches!(jb.pull(), JitterResult::Frame(_)));
            }
            assert!(matches!(jb.pull(), JitterResult::NotReady), "run dry: refilling");
            for seq in 12..21 {
                jb.push(f(seq, seq < 21 - release), 400);
            }
            (0..12).map(|_| match jb.pull() { JitterResult::Frame(fr) => Some(fr.sequence), _ => None }).collect::<Vec<_>>()
        };
        let all: Vec<Option<u32>> = (12..21).map(Some).chain([None, None, None]).collect();
        assert_eq!(after_a_gap(JitterBuffer::for_transmit(12, 20), 5), all, "the release is played");
        // Both ways (working rule 13): a receive buffer still refills to its
        // target, and a transmit buffer without the release in hand too.
        assert_eq!(after_a_gap(JitterBuffer::new(12, 20), 5), vec![None; 12], "a receive buffer waits");
        assert_eq!(after_a_gap(JitterBuffer::for_transmit(12, 20), 0), vec![None; 12], "no release: it waits");
        // The first fill too: a transmission shorter than the target.
        let mut jb = JitterBuffer::for_transmit(12, 20);
        for round in 0..2 {
            for seq in 0..2 {
                jb.push(f(seq, true), 0);
            }
            assert!(matches!(jb.pull(), JitterResult::NotReady), "round {round}: no release yet");
            for seq in 2..5 {
                jb.push(f(seq, false), 0);
            }
            assert!(matches!(jb.pull(), JitterResult::Frame(fr) if fr.sequence == 0), "round {round}");
            // `reset` keeps it a transmit buffer.
            jb.reset();
        }
    }
}
