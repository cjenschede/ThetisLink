// SPDX-License-Identifier: GPL-2.0-or-later

//! A copy of what a Yaesu's transmit chain hands the Opus encoder, for the
//! transmit spectrum on screen.
//!
//! The tap sits where the level meter sits (`to_encoder` in the engine): after
//! the gain, EQ, compressor, AGC and limiter, at 16 kHz, the frame exactly as
//! it is encoded. It copies only while a spectrum is on screen (`set_enabled`),
//! and the transmit path never waits on it: a push that finds the lock taken is
//! dropped. The analysis (the FFT) is the screen's business, on its own thread.
//!
//! Per Yaesu slot it keeps the newest `LIVE_SAMPLES` for the live spectrum and
//! all samples of the current over, which become "the last over" once no frame
//! has come for `OVER_GAP`. A roger beep shows in the live spectrum but is
//! left out of the over: it would colour the average. The over is kept in
//! blocks of `OVER_BLOCK` samples, so a long over never grows by copying what
//! it already has on the transmit thread.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The rate of the frames the encoder takes (wideband).
pub const SAMPLE_RATE: u32 = sdr_remote_core::NETWORK_SAMPLE_RATE_WIDEBAND;
/// Samples kept for the live spectrum: one FFT of 2048 points, 128 ms.
pub const LIVE_SAMPLES: usize = 2048;
/// No frame for this long ends an over. Frames come every 20 ms while the
/// transmitter is keyed.
pub const OVER_GAP: Duration = Duration::from_millis(300);
/// The longest over kept, in samples (5 minutes); a longer one keeps its start.
pub const MAX_OVER_SAMPLES: usize = SAMPLE_RATE as usize * 300;
/// One block of the over (one second), allocated whole when the previous is full.
pub const OVER_BLOCK: usize = SAMPLE_RATE as usize;

/// Where a frame came from, as far as the spectrum cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The microphone, through the whole chain.
    Mic,
    /// A played WAV: no EQ, compressor or AGC on it.
    Wav,
    /// The roger beep: live only, not in the over.
    Roger,
}

/// A finished over, as the screen gets it.
pub struct Over {
    pub samples: Vec<f32>,
    /// A played WAV was in it (sent without the EQ).
    pub wav: bool,
}

#[derive(Default)]
struct Slot {
    live: VecDeque<f32>,
    live_wav: bool,
    over: Vec<Vec<f32>>,
    over_len: usize,
    over_wav: bool,
    finished: Option<(Vec<Vec<f32>>, bool)>,
    last_push: Option<Instant>,
}

impl Slot {
    /// Move a running over to `finished` once no frame came for `OVER_GAP`.
    fn close_over_if_quiet(&mut self, now: Instant) {
        if let Some(at) = self.last_push {
            if now.duration_since(at) >= OVER_GAP && self.over_len > 0 {
                self.finished = Some((std::mem::take(&mut self.over), self.over_wav));
                self.over_len = 0;
                self.over_wav = false;
            }
        }
    }

    fn push_over(&mut self, v: f32) {
        if self.over_len >= MAX_OVER_SAMPLES {
            return;
        }
        if self.over.last().is_none_or(|b| b.len() == OVER_BLOCK) {
            self.over.push(Vec::with_capacity(OVER_BLOCK));
        }
        if let Some(b) = self.over.last_mut() {
            b.push(v);
            self.over_len += 1;
        }
    }
}

/// The tap, shared by the engine (which pushes) and the screen (which reads).
#[derive(Default)]
pub struct TxTap {
    enabled: AtomicBool,
    slots: Mutex<[Slot; 2]>,
}

impl TxTap {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Copy frames only while a spectrum is on screen. Switching off forgets
    /// what was copied.
    pub fn set_enabled(&self, on: bool) {
        let was = self.enabled.swap(on, Ordering::Relaxed);
        if was && !on {
            if let Ok(mut slots) = self.slots.lock() {
                *slots = Default::default();
            }
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// The engine: one frame as it goes to the encoder, for Yaesu `slot` (0 or
    /// 1). Never blocks: a frame that finds the lock taken is not copied.
    pub fn push(&self, slot: usize, pcm: &[i16], source: Source) {
        if !self.enabled() || slot > 1 {
            return;
        }
        let Ok(mut slots) = self.slots.try_lock() else { return };
        let now = Instant::now();
        let s = &mut slots[slot];
        s.close_over_if_quiet(now);
        for &x in pcm {
            let v = f32::from(x) / 32768.0;
            if s.live.len() == LIVE_SAMPLES {
                s.live.pop_front();
            }
            s.live.push_back(v);
            if source != Source::Roger {
                s.push_over(v);
            }
        }
        s.live_wav = source == Source::Wav;
        if source == Source::Wav {
            s.over_wav = true;
        }
        s.last_push = Some(now);
    }

    /// The screen: the newest samples of `slot`, oldest first; whether a frame
    /// came within `OVER_GAP` (the transmitter is keyed); and whether the
    /// newest frame was a played WAV.
    pub fn live(&self, slot: usize) -> (Vec<f32>, bool, bool) {
        let Ok(slots) = self.slots.lock() else { return (Vec::new(), false, false) };
        let s = &slots[slot.min(1)];
        let keyed = s.last_push.is_some_and(|at| at.elapsed() < OVER_GAP);
        (s.live.iter().copied().collect(), keyed, s.live_wav)
    }

    /// The screen: the over that ended last, once, or `None` when no over
    /// ended since the last call. The blocks are joined after the lock is let
    /// go, so the transmit path is not held up by it.
    pub fn take_finished_over(&self, slot: usize) -> Option<Over> {
        let (blocks, wav) = {
            let Ok(mut slots) = self.slots.lock() else { return None };
            let s = &mut slots[slot.min(1)];
            s.close_over_if_quiet(Instant::now());
            s.finished.take()?
        };
        Some(Over { samples: blocks.concat(), wav })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(v: i16) -> Vec<i16> {
        vec![v; 320]
    }

    /// Closed, the tap copies nothing; open, it does; closed and opened again,
    /// it starts empty.
    #[test]
    fn a_closed_tap_copies_nothing() {
        let tap = TxTap::new();
        tap.push(0, &frame(1000), Source::Mic);
        assert!(tap.live(0).0.is_empty());
        tap.set_enabled(true);
        tap.push(0, &frame(1000), Source::Mic);
        assert_eq!(tap.live(0).0.len(), 320);
        tap.set_enabled(false);
        tap.set_enabled(true);
        assert!(tap.live(0).0.is_empty(), "closing forgets");
        assert!(tap.take_finished_over(0).is_none(), "the over too");
    }

    /// The live window keeps the newest `LIVE_SAMPLES`.
    #[test]
    fn the_live_window_keeps_the_newest_samples() {
        let tap = TxTap::new();
        tap.set_enabled(true);
        for _ in 0..10 {
            tap.push(1, &frame(0), Source::Mic);
        }
        tap.push(1, &frame(16384), Source::Mic);
        let (live, keyed, _) = tap.live(1);
        assert_eq!(live.len(), LIVE_SAMPLES);
        assert_eq!(*live.last().unwrap(), 0.5);
        assert!(keyed);
        assert!(tap.live(0).0.is_empty(), "the other Yaesu has its own window");
    }

    /// An over ends after `OVER_GAP` without frames, comes out once, and
    /// leaves the roger beep out (which the live window does show).
    #[test]
    fn an_over_ends_after_a_gap_without_the_roger_beep() {
        let tap = TxTap::new();
        tap.set_enabled(true);
        tap.push(0, &frame(100), Source::Mic);
        tap.push(0, &frame(100), Source::Mic);
        tap.push(0, &frame(30000), Source::Roger);
        assert_eq!(*tap.live(0).0.last().unwrap(), 30000.0 / 32768.0, "the beep is live");
        assert!(tap.take_finished_over(0).is_none(), "still keyed");
        std::thread::sleep(OVER_GAP + Duration::from_millis(50));
        let over = tap.take_finished_over(0).expect("the over ended");
        assert_eq!(over.samples.len(), 640, "two frames, the beep left out");
        assert!(!over.wav);
        assert!(tap.take_finished_over(0).is_none(), "once");
    }

    /// A new frame after a gap starts a new over and hands over the old one.
    #[test]
    fn a_frame_after_a_gap_starts_a_new_over() {
        let tap = TxTap::new();
        tap.set_enabled(true);
        tap.push(0, &frame(100), Source::Mic);
        std::thread::sleep(OVER_GAP + Duration::from_millis(50));
        tap.push(0, &frame(200), Source::Mic);
        let first = tap.take_finished_over(0).expect("the first over ended");
        assert_eq!(first.samples.len(), 320);
    }

    /// The over grows in whole blocks (never by copying what it has), keeps
    /// the order across a block edge, and stops at `MAX_OVER_SAMPLES`.
    #[test]
    fn the_over_grows_in_blocks_and_keeps_its_order() {
        let mut s = Slot::default();
        let n = OVER_BLOCK + 500;
        for i in 0..n {
            s.push_over(i as f32);
        }
        assert_eq!(s.over.len(), 2);
        assert!(s.over.iter().all(|b| b.capacity() == OVER_BLOCK), "allocated whole, never grown");
        let joined = s.over.concat();
        assert_eq!(joined.len(), n);
        assert!(joined.iter().enumerate().all(|(i, &v)| v == i as f32));
        let mut full = Slot::default();
        for _ in 0..MAX_OVER_SAMPLES + 10 {
            full.push_over(0.0);
        }
        assert_eq!(full.over_len, MAX_OVER_SAMPLES);
        assert_eq!(full.over.iter().map(Vec::len).sum::<usize>(), MAX_OVER_SAMPLES);
    }

    /// A played WAV marks the live window and the over it is in.
    #[test]
    fn a_wav_is_marked_live_and_in_the_over() {
        let tap = TxTap::new();
        tap.set_enabled(true);
        tap.push(0, &frame(100), Source::Mic);
        assert!(!tap.live(0).2);
        tap.push(0, &frame(100), Source::Wav);
        assert!(tap.live(0).2, "the newest frame is a WAV");
        std::thread::sleep(OVER_GAP + Duration::from_millis(50));
        assert!(tap.take_finished_over(0).expect("ended").wav);
        tap.push(0, &frame(100), Source::Mic);
        std::thread::sleep(OVER_GAP + Duration::from_millis(50));
        assert!(!tap.take_finished_over(0).expect("ended").wav, "the next over starts unmarked");
    }
}
