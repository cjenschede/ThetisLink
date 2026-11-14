// SPDX-License-Identifier: GPL-2.0-or-later

//! How fast receive audio arrives over TCI, against the rate its frames
//! announce.
//!
//! A TCI server that sends fewer samples per second than it announces gives
//! audio that stutters on the client however clean the link is: the jitter
//! buffer runs dry and conceals. Nothing else in the log says that the
//! source is slow. This measures it per receiver over [`WINDOW`]: the first
//! window is always said, after that only a window off by more than
//! [`OFF_BY`], once at the start of a run of such windows and then every
//! [`OFF_REPEAT`] windows. With a source that keeps time, one line per
//! receiver per TCI connection.
//!
//! A frame carries the samples made before it arrived, so a window starts at
//! a frame and counts the samples of the frames after it. A gap longer than
//! [`PAUSE`] (the stream stopped, or Thetis restarted it) starts a new window
//! and a new run rather than count as slow. A shorter gap is counted, and is
//! in the line as the longest gap: that number tells a hole in the stream
//! from a source that is slow all along.

use std::time::{Duration, Instant};

pub const WINDOW: Duration = Duration::from_secs(10);
/// The fraction off the announced rate that is said.
pub const OFF_BY: f64 = 0.02;
/// In a run of windows off, every this many is said again.
pub const OFF_REPEAT: u32 = 6;
/// A gap this long is a pause, not a slow source.
pub const PAUSE: Duration = Duration::from_secs(2);

#[derive(Debug, Default)]
pub struct AudioRate {
    start: Option<Instant>,
    last: Option<Instant>,
    samples: u64,
    frames: u64,
    longest_gap: Duration,
    said_first: bool,
    off_run: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub samples_per_s: f64,
    pub announced: u32,
    pub over: Duration,
    pub frames: u64,
    pub longest_gap: Duration,
}

impl Report {
    pub fn line(&self, receiver: &str) -> String {
        let announced = if self.announced > 0 {
            format!(
                "the frames announce {} ({:.1}%)",
                self.announced,
                100.0 * self.samples_per_s / self.announced as f64
            )
        } else {
            "the frames announce no rate".to_string()
        };
        format!(
            "TCI {} audio arrives at {:.0} samples/s over {:.1} s, {}; {} frames, longest gap {} ms",
            receiver,
            self.samples_per_s,
            self.over.as_secs_f64(),
            announced,
            self.frames,
            self.longest_gap.as_millis()
        )
    }

    /// Off its announced rate by more than [`OFF_BY`]: a fault, said as an
    /// ordinary line; anything else this says is a measurement.
    pub fn off(&self) -> bool {
        self.announced > 0 && (self.samples_per_s / self.announced as f64 - 1.0).abs() > OFF_BY
    }
}

impl AudioRate {
    /// One received frame of `samples` per channel; a report when a window
    /// closes and it is to be said.
    pub fn note(&mut self, now: Instant, samples: usize, announced: u32) -> Option<Report> {
        let (Some(start), Some(last)) = (self.start, self.last) else {
            self.begin(now);
            return None;
        };
        let gap = now.saturating_duration_since(last);
        if gap > PAUSE {
            // A slow source after a pause is said at once, as a new run.
            self.off_run = 0;
            self.begin(now);
            return None;
        }
        self.samples += samples as u64;
        self.frames += 1;
        self.longest_gap = self.longest_gap.max(gap);
        self.last = Some(now);
        let over = now.saturating_duration_since(start);
        if over < WINDOW {
            return None;
        }
        let report = Report {
            samples_per_s: self.samples as f64 / over.as_secs_f64(),
            announced,
            over,
            frames: self.frames,
            longest_gap: self.longest_gap,
        };
        self.begin(now);
        let say = if report.off() {
            self.off_run += 1;
            self.off_run == 1 || (self.off_run - 1) % OFF_REPEAT == 0
        } else {
            self.off_run = 0;
            false
        };
        let first = !self.said_first;
        self.said_first = true;
        (first || say).then_some(report)
    }

    fn begin(&mut self, now: Instant) {
        self.start = Some(now);
        self.last = Some(now);
        self.samples = 0;
        self.frames = 0;
        self.longest_gap = Duration::ZERO;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source that keeps time is a measurement, said in developer mode only;
    /// one off its rate is a fault, said to everyone. The TX_CHRONO line, one
    /// per transmission, is a measurement too.
    #[test]
    fn only_a_source_off_its_rate_is_an_ordinary_line() {
        let report = |samples_per_s: f64, announced: u32| Report {
            samples_per_s,
            announced,
            over: WINDOW,
            frames: 500,
            longest_gap: Duration::from_millis(20),
        };
        assert!(!report(48_000.0, 48_000).off(), "on time");
        assert!(!report(47_200.0, 48_000).off(), "1.7 % slow: within");
        assert!(report(46_000.0, 48_000).off(), "4 % slow: a fault");
        assert!(report(50_000.0, 48_000).off(), "4 % fast: a fault");
        assert!(!report(46_000.0, 0).off(), "no announced rate: nothing to be off from");
        let tci = sdr_remote_core::source_text::production_code(include_str!("tci.rs"));
        assert_eq!(
            tci.matches("if report.off() {
                                info!(\"{line}\");
                            } else {
                                dev_info!(\"{line}\");").count(),
            1,
            "the fault to everyone, the measurement in developer mode"
        );
        assert_eq!(tci.matches("dev_info!(\"TCI TX_CHRONO: the first request of this transmission").count(), 1);
        assert!(!tci.contains("    info!(\"TCI TX_CHRONO"), "not as an ordinary line");
    }

    /// Frames of `per` samples, `every` apart, for `secs` seconds; the
    /// reports that came out.
    fn run(m: &mut AudioRate, t0: Instant, per: usize, every: Duration, secs: u64) -> (Vec<Report>, Instant) {
        run_at(m, t0, per, every, secs, 48_000)
    }

    fn run_at(m: &mut AudioRate, t0: Instant, per: usize, every: Duration, secs: u64, announced: u32) -> (Vec<Report>, Instant) {
        let mut out = Vec::new();
        let mut t = t0;
        while t < t0 + Duration::from_secs(secs) {
            if let Some(r) = m.note(t, per, announced) {
                out.push(r);
            }
            t += every;
        }
        (out, t)
    }

    // 2048 samples at 48 kHz: one frame every 42.666 ms.
    const FRAME: Duration = Duration::from_nanos(42_666_667);

    #[test]
    fn a_source_in_time_is_said_once_then_quiet() {
        let mut m = AudioRate::default();
        let (out, _) = run(&mut m, Instant::now(), 2048, FRAME, 61);
        assert_eq!(out.len(), 1, "{out:?}");
        let r = &out[0];
        assert!((r.samples_per_s - 48_000.0).abs() < 100.0, "{r:?}");
        assert!(r.longest_gap >= Duration::from_millis(42) && r.longest_gap <= Duration::from_millis(43));
        assert!(r.line("RX1").contains("(100.0%)"), "{}", r.line("RX1"));
    }

    #[test]
    fn a_source_at_half_speed_is_said_at_the_start_of_the_run_and_then_every_minute() {
        let mut m = AudioRate::default();
        let (out, _) = run(&mut m, Instant::now(), 1024, FRAME, 135);
        // Windows close at about 10, 20, ... 130 s: said are the 1st, the 7th
        // and the 13th (every 7th would give two).
        assert_eq!(out.len(), 3, "{out:?}");
        for r in &out {
            assert!(r.over >= Duration::from_secs(10) && r.over < Duration::from_millis(10_100), "{r:?}");
            assert!((r.samples_per_s - 24_000.0).abs() < 100.0, "{r:?}");
            assert!(r.line("RX2").starts_with("TCI RX2 audio arrives at 24000 samples/s"), "{}", r.line("RX2"));
        }
        // Six windows: only the first is said (every 5th would say the 6th too).
        let mut m = AudioRate::default();
        let (out, _) = run(&mut m, Instant::now(), 1024, FRAME, 65);
        assert_eq!(out.len(), 1, "{out:?}");
    }

    #[test]
    fn two_percent_is_the_edge() {
        // The rate as a fraction of the announced one, slow and fast.
        let at = |fraction: f64| Duration::from_secs_f64(FRAME.as_secs_f64() / fraction);
        let mut m = AudioRate::default();
        let (_, mut t) = run(&mut m, Instant::now(), 2048, FRAME, 11);
        for within in [0.981, 1.019] {
            let (out, next) = run(&mut m, t, 2048, at(within), 21);
            assert!(out.is_empty(), "{within} is within: {out:?}");
            t = next;
        }
        for off in [0.979, 1.021] {
            let mut m = AudioRate::default();
            let (_, t) = run(&mut m, Instant::now(), 2048, FRAME, 11);
            let (out, _) = run(&mut m, t, 2048, at(off), 21);
            assert!(!out.is_empty(), "{off} is said");
            assert!((out[0].samples_per_s / 48_000.0 - off).abs() < 0.002, "{:?}", out[0]);
        }
    }

    #[test]
    fn a_hole_in_the_stream_is_in_the_line_as_the_longest_gap() {
        let mut m = AudioRate::default();
        let t0 = Instant::now();
        let (out, t) = run(&mut m, t0, 2048, FRAME, 11);
        assert_eq!(out.len(), 1);
        // A hole of 1.5 s is shorter than PAUSE, so the window counts it and
        // is said as short; the gap is what tells it from a slow source.
        let (out, _) = run(&mut m, t + Duration::from_millis(1500), 2048, FRAME, 11);
        assert_eq!(out.len(), 1, "{out:?}");
        let r = &out[0];
        assert!(r.longest_gap >= Duration::from_millis(1500), "{r:?}");
        assert!(r.over >= WINDOW, "{r:?}");
        assert!(r.frames > 150 && r.frames < 250, "{r:?}");
        let line = r.line("RX1");
        assert!(line.contains(&format!("longest gap {} ms", r.longest_gap.as_millis())), "{line}");
        assert!(line.contains(&format!("; {} frames", r.frames)), "{line}");
        assert!(line.contains(&format!("over {:.1} s", r.over.as_secs_f64())), "{line}");
    }

    #[test]
    fn a_gap_of_exactly_the_pause_is_still_counted() {
        let mut m = AudioRate::default();
        let (_, t) = run(&mut m, Instant::now(), 2048, FRAME, 11);
        // `t` is one frame after the last one.
        let (out, _) = run(&mut m, t - FRAME + PAUSE, 2048, FRAME, 11);
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(out[0].longest_gap >= PAUSE, "{:?}", out[0]);
    }

    #[test]
    fn a_source_that_announces_no_rate_is_said_without_a_percentage() {
        let mut m = AudioRate::default();
        let (out, _) = run_at(&mut m, Instant::now(), 2048, FRAME, 61, 0);
        assert_eq!(out.len(), 1, "said once, and never off: {out:?}");
        let line = out[0].line("RX1");
        assert!(line.contains("at 48000 samples/s") && line.contains("the frames announce no rate;"), "{line}");
        assert!(!line.contains('%'), "{line}");
        assert!(!out[0].off(), "a rate of 0 is never off: {:?}", out[0]);
    }

    #[test]
    fn a_pause_starts_a_new_window_and_is_not_counted_as_slow() {
        let mut m = AudioRate::default();
        let t0 = Instant::now();
        let (out, t) = run(&mut m, t0, 2048, FRAME, 11);
        assert_eq!(out.len(), 1);
        // The stream stops for five seconds, then comes back in time.
        let (out, _) = run(&mut m, t + Duration::from_secs(5), 2048, FRAME, 31);
        assert!(out.is_empty(), "the pause made nothing slow: {out:?}");
    }

    #[test]
    fn a_slow_source_after_a_pause_is_said_at_once() {
        let mut m = AudioRate::default();
        let (out, t) = run(&mut m, Instant::now(), 1024, FRAME, 11);
        assert_eq!(out.len(), 1);
        let (out, _) = run(&mut m, t + Duration::from_secs(5), 1024, FRAME, 11);
        assert_eq!(out.len(), 1, "a pause starts a new run: {out:?}");
    }

    #[test]
    fn a_slow_run_that_recovers_and_slows_again_is_said_again_at_once() {
        let mut m = AudioRate::default();
        let t0 = Instant::now();
        let (out, t) = run(&mut m, t0, 1024, FRAME, 11);
        assert_eq!(out.len(), 1);
        // Twenty seconds in time: the window across the change is still off,
        // the one after it is clean and ends the run.
        let (out, t) = run(&mut m, t, 2048, FRAME, 20);
        assert!(out.is_empty());
        let (out, _) = run(&mut m, t, 1024, FRAME, 11);
        assert_eq!(out.len(), 1, "a new run of slow windows is said at its start");
    }
}
