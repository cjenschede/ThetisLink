// SPDX-License-Identifier: GPL-2.0-or-later
//! Catching up the transmit buffer in the pauses of speech
//! (`PLAN-inlopen-in-pauzes.md`, build 257). Instead of one frame every ~250 ms
//! wherever it falls (`TRIM_EVERY_PULLS`), the playout tick decodes each frame,
//! and with a backlog lets a quiet one go - a frame within a few dB of the noise
//! floor of the last seconds. When no quiet frame comes within `MAX_WAIT`, the
//! next frame goes all the same, so the buffer always catches up.
//!
//! Measured against the noise floor, not a fixed level: over the radio a pause
//! is noise, not silence, and a compressor narrows the gap to the speech. On
//! two recordings of the owner a quiet frame came within 0.5-0.8 s nine times in
//! ten (owner, 2026-10-19: "Ik persoonlijk stoor me daar erg aan, [aan veel
//! kleine hapjes], maar dan 1 keer 200ms").
//!
//! First a trial in a development build (build 257, trial run 3); in the server
//! since build 258 (owner 2026-10-19: "1, en B mag ook mee in stap 5").

use std::time::{Duration, Instant};

/// A frame this close to the noise floor is a pause...
pub const QUIET_ABOVE_FLOOR_DB: f32 = 4.0;
/// ...if it is also this far under the speech (the 80th percentile). Without it
/// a voice with no pause at all would have its floor at the speech, and every
/// frame would count as quiet (found by the test of the fallback, build 257).
pub const QUIET_BELOW_SPEECH_DB: f32 = 5.0;
pub const SPEECH_PERCENTILE: f32 = 0.80;
/// The floor is the 10th percentile of the levels of the last 3 s (150 frames).
pub const FLOOR_FRAMES: usize = 150;
pub const FLOOR_PERCENTILE: f32 = 0.10;
/// Before half a second of levels there is no floor, and nothing counts as quiet.
pub const FLOOR_WARMUP_FRAMES: usize = 25;
/// The longest wait for a quiet frame with a backlog (owner, 2026-10-19: "1 s is goed").
pub const MAX_WAIT: Duration = Duration::from_millis(1000);
/// How often the counts are said.
pub const LOG_EVERY: Duration = Duration::from_secs(10);
/// The level of digital silence.
const SILENT_DB: f32 = -96.0;

/// The level of a frame in dB below full scale.
pub fn level_db(pcm: &[i16]) -> f32 {
    if pcm.is_empty() {
        return SILENT_DB;
    }
    let sum: f64 = pcm.iter().map(|&s| (s as f64) * (s as f64)).sum();
    let rms = (sum / pcm.len() as f64).sqrt();
    if rms < 1.0 {
        return SILENT_DB;
    }
    (20.0 * (rms / 32768.0).log10()) as f32
}

/// A backlog to catch up: the buffer stands more than one frame above its
/// target after the pull - the threshold of the trim it replaces
/// (`trim_above_target`, 2, counted before the pull).
pub fn backlog(depth_after_pull: usize, target: u32) -> bool {
    depth_after_pull > target as usize + 1
}

/// On the transmit path, once per decoded frame: nothing is allocated - the
/// levels sit in a fixed ring, sorted in a fixed copy (round 257).
pub struct PauseTrim {
    /// The levels of the last `FLOOR_FRAMES` frames, oldest at `next` once full.
    levels: [f32; FLOOR_FRAMES],
    len: usize,
    next: usize,
    sorted: [f32; FLOOR_FRAMES],
    /// When the last frame was noted: after a silence past the safety's packet
    /// window the levels are another over's (round 257).
    last_noted: Option<Instant>,
    backlog_since: Option<Instant>,
    in_pause: u64,
    by_fallback: u64,
    said_at: Instant,
}

impl PauseTrim {
    pub fn new(now: Instant) -> Self {
        PauseTrim {
            levels: [0.0; FLOOR_FRAMES],
            len: 0,
            next: 0,
            sorted: [0.0; FLOOR_FRAMES],
            last_noted: None,
            backlog_since: None,
            in_pause: 0,
            by_fallback: 0,
            said_at: now,
        }
    }

    /// The quiet threshold: within `QUIET_ABOVE_FLOOR_DB` of the floor and at
    /// least `QUIET_BELOW_SPEECH_DB` under the speech; `None` before the warm-up.
    fn quiet_below(&mut self) -> Option<f32> {
        if self.len < FLOOR_WARMUP_FRAMES {
            return None;
        }
        let v = &mut self.sorted[..self.len];
        v.copy_from_slice(&self.levels[..self.len]);
        v.sort_unstable_by(|a, b| a.total_cmp(b));
        let at = |q: f32| v[((v.len() - 1) as f32 * q).round() as usize];
        Some((at(FLOOR_PERCENTILE) + QUIET_ABOVE_FLOOR_DB).min(at(SPEECH_PERCENTILE) - QUIET_BELOW_SPEECH_DB))
    }

    /// One decoded frame of an over. Its level always counts for the floor;
    /// `true`: let it go instead of sending it - there is a backlog, the frame
    /// may go (`may_go`: not a second one this tick), and it is quiet or the
    /// wait is over. Each over has its own levels: after a silence past the
    /// safety's packet window - another operator, a new over, a line that went -
    /// the earlier ones are forgotten (round 257: a soft voice after a noisy
    /// line counted as quiet for seconds).
    pub fn let_go(&mut self, pcm: &[i16], backlog: bool, may_go: bool, now: Instant) -> bool {
        if self.last_noted.is_some_and(|t| crate::ptt::silence_ended_the_over(Some(now.saturating_duration_since(t)))) {
            self.len = 0;
            self.next = 0;
            self.backlog_since = None;
        }
        self.last_noted = Some(now);
        let db = level_db(pcm);
        let quiet = self.quiet_below().is_some_and(|below| db < below);
        self.levels[self.next] = db;
        self.next = (self.next + 1) % FLOOR_FRAMES;
        self.len = (self.len + 1).min(FLOOR_FRAMES);
        if !backlog {
            self.backlog_since = None;
            return false;
        }
        if !may_go {
            self.backlog_since.get_or_insert(now);
            return false;
        }
        let since = *self.backlog_since.get_or_insert(now);
        if quiet {
            self.in_pause += 1;
        } else if now.saturating_duration_since(since) >= MAX_WAIT {
            self.by_fallback += 1;
        } else {
            return false;
        }
        self.backlog_since = None;
        true
    }

    /// (in a pause, by the fallback) since the last time, once per `LOG_EVERY`
    /// and only when there is something.
    pub fn counts_to_say(&mut self, now: Instant) -> Option<(u64, u64)> {
        if now.saturating_duration_since(self.said_at) < LOG_EVERY {
            return None;
        }
        self.said_at = now;
        let c = (self.in_pause, self.by_fallback);
        self.in_pause = 0;
        self.by_fallback = 0;
        (c.0 + c.1 > 0).then_some(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(amplitude: i16) -> Vec<i16> {
        (0..320).map(|i| if i % 2 == 0 { amplitude } else { -amplitude }).collect()
    }

    fn ms(t0: Instant, n: u64) -> Instant {
        t0 + Duration::from_millis(n)
    }

    /// Full scale is 0 dB, half is -6 dB, nothing is digital silence.
    #[test]
    fn the_level_of_a_frame() {
        assert!((level_db(&frame(32767)) - 0.0).abs() < 0.01);
        assert!((level_db(&frame(16384)) + 6.02).abs() < 0.05);
        assert_eq!(level_db(&frame(0)), SILENT_DB);
        assert_eq!(level_db(&[]), SILENT_DB);
    }

    /// The backlog is the threshold of the trim it replaces: more than one
    /// frame above the target after the pull.
    #[test]
    fn a_backlog_is_more_than_one_above_the_target() {
        assert!(!backlog(4, 3));
        assert!(backlog(5, 3));
    }

    /// Speech at -10 dB with pauses of noise at -30 dB: with a backlog the loud
    /// frames are sent, the first quiet one goes; without a backlog nothing goes.
    #[test]
    fn with_a_backlog_the_first_quiet_frame_goes() {
        let t0 = Instant::now();
        let mut p = PauseTrim::new(t0);
        let (loud, quiet) = (frame(10_000), frame(1_000));
        // Half a second of speech and pauses to learn the floor.
        for i in 0..40u64 {
            let f = if i % 4 == 0 { &quiet } else { &loud };
            assert!(!p.let_go(f, false, true, ms(t0, i * 20)), "no backlog: nothing goes");
        }
        let mut t = 40 * 20;
        assert!(!p.let_go(&loud, true, true, ms(t0, t)), "speech is sent");
        t += 20;
        assert!(!p.let_go(&loud, true, true, ms(t0, t)));
        t += 20;
        assert!(p.let_go(&quiet, true, true, ms(t0, t)), "the pause goes");
        assert_eq!((p.in_pause, p.by_fallback), (1, 0));
    }

    /// A soft syllable is no pause: 7 dB under the speech, but far above the noise
    /// floor of the pauses - it is sent, and the pause after it goes.
    #[test]
    fn a_soft_syllable_is_no_pause() {
        let t0 = Instant::now();
        let mut p = PauseTrim::new(t0);
        let (loud, soft, pause) = (frame(10_000), frame(4_500), frame(1_000));
        for i in 0..40u64 {
            p.let_go(if i % 4 == 0 { &pause } else { &loud }, false, true, ms(t0, i * 20));
        }
        assert!(!p.let_go(&soft, true, true, ms(t0, 800)), "-17 dB against speech at -10 and noise at -30: speech");
        assert!(p.let_go(&pause, true, true, ms(t0, 820)), "the pause");
    }

    /// Only speech, no pause: nothing counts as quiet (the floor is the speech),
    /// so the next frame goes at exactly the longest wait, not a frame earlier;
    /// then the wait starts over.
    #[test]
    fn without_a_pause_the_fallback_comes_after_one_second() {
        let t0 = Instant::now();
        let mut p = PauseTrim::new(t0);
        let loud = frame(10_000);
        for i in 0..30u64 {
            p.let_go(&loud, false, true, ms(t0, i * 20));
        }
        let start = 1_000;
        assert!(!p.let_go(&loud, true, true, ms(t0, start)), "the wait starts");
        assert!(!p.let_go(&loud, true, true, ms(t0, start + 999)));
        assert!(p.let_go(&loud, true, true, ms(t0, start + 1_000)), "at one second");
        assert_eq!((p.in_pause, p.by_fallback), (0, 1));
        assert!(!p.let_go(&loud, true, true, ms(t0, start + 1_020)), "and the wait starts over");
        assert_eq!(MAX_WAIT, Duration::from_millis(1000));
    }

    /// A backlog that goes away ends the wait: the next backlog waits a whole
    /// second again.
    #[test]
    fn a_backlog_that_goes_away_ends_the_wait() {
        let t0 = Instant::now();
        let mut p = PauseTrim::new(t0);
        let loud = frame(10_000);
        assert!(!p.let_go(&loud, true, true, ms(t0, 0)));
        assert!(!p.let_go(&loud, false, true, ms(t0, 800)));
        assert!(!p.let_go(&loud, true, true, ms(t0, 1_000)), "a new wait, not 1 s since 0");
        assert!(p.let_go(&loud, true, true, ms(t0, 2_000)));
    }

    /// A compressed voice: speech -16 dB, pauses -22 dB (6 dB apart). The floor
    /// follows the noise, so the pauses still count as quiet; before the floor is
    /// known (half a second) nothing does.
    #[test]
    fn a_compressed_voice_still_has_pauses() {
        let t0 = Instant::now();
        let mut p = PauseTrim::new(t0);
        let (speech, pause) = (frame(5_200), frame(2_600));
        assert!(!p.let_go(&pause, true, true, ms(t0, 0)), "no floor yet: not quiet");
        for i in 1..60u64 {
            p.let_go(if i % 3 == 0 { &pause } else { &speech }, false, true, ms(t0, i * 20));
        }
        assert!(!p.let_go(&speech, true, true, ms(t0, 1_300)));
        assert!(p.let_go(&pause, true, true, ms(t0, 1_320)), "6 dB is enough");
    }

    /// After a silence past the safety's packet window the levels are another
    /// over's: a soft voice on a quiet line after a noisy one is not a pause
    /// (a reviewer's probe, round 257). Within the window they stay. Both ways.
    #[test]
    fn a_silence_past_the_packet_window_forgets_the_levels() {
        let goes = |silence_ms: u64| {
            let t0 = Instant::now();
            let mut p = PauseTrim::new(t0);
            let (noise, speech, soft) = (frame(330), frame(5_800), frame(410));
            for i in 0..150u64 {
                p.let_go(if i % 4 == 0 { &noise } else { &speech }, false, true, ms(t0, i * 20));
            }
            let start = 149 * 20 + silence_ms;
            (0..10u64).filter(|&i| p.let_go(&soft, true, true, ms(t0, start + i * 20))).count()
        };
        assert_eq!(goes(1_000), 10, "within the window the noisy line's floor still says quiet");
        assert_eq!(goes(2_001), 0, "past it the soft voice is the over's own speech");
        assert_eq!(crate::ptt::silence_ended_the_over(Some(Duration::from_millis(2_001))), true);
    }

    /// Every frame counts for the floor, also one that may not go (a second in
    /// the same tick): 25 of them make the warm-up (round 257).
    #[test]
    fn a_frame_that_may_not_go_still_counts_for_the_floor() {
        let t0 = Instant::now();
        let mut p = PauseTrim::new(t0);
        let (loud, quiet) = (frame(10_000), frame(1_000));
        for i in 0..40u64 {
            assert!(!p.let_go(if i % 4 == 0 { &quiet } else { &loud }, true, false, ms(t0, i * 20)), "may not go");
        }
        assert!(p.let_go(&quiet, true, true, ms(t0, 800)), "the floor is there, and the wait went on");
        assert_eq!((p.in_pause, p.by_fallback), (1, 0));
    }

    /// The counts are said once per 10 s, then start again; nothing to say, no line.
    #[test]
    fn the_counts_are_said_every_ten_seconds() {
        let t0 = Instant::now();
        let mut p = PauseTrim::new(t0);
        let loud = frame(10_000);
        p.let_go(&loud, true, true, ms(t0, 0));
        p.let_go(&loud, true, true, ms(t0, 1_000));
        assert_eq!(p.counts_to_say(ms(t0, 9_999)), None);
        assert_eq!(p.counts_to_say(ms(t0, 10_000)), Some((0, 1)));
        assert_eq!(p.counts_to_say(ms(t0, 20_000)), None, "nothing since");
    }
}
