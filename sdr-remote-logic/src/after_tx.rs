// SPDX-License-Identifier: GPL-2.0-or-later
//! The RX stream right after the operator lets go of PTT.
//!
//! For ~170-190 ms after TRX off Thetis sends silence (it is switching back), and
//! the playout ring filled up with it: the first sound of the other station then
//! waited behind ~80 ms of silence (tenth measurement, 2026-10-04). This decides,
//! per RX frame after a Thetis PTT release, whether to write it to the ring:
//! silence is skipped as long as the ring keeps a small cushion, so the first
//! sound goes to the speaker at once. The cushion is there because an empty ring
//! can click at the first sound; the window ends the skipping when no sound comes
//! (a quiet band, squelch).

use std::time::{Duration, Instant};

/// A frame whose peak stays below this counts as silence (-80 dBFS). Band noise on
/// a receiver sits far above it; what Thetis sends while switching sits below.
pub const SILENCE_PEAK: f32 = 1e-4;

/// Silence is still written while the ring holds less than this - the room the
/// first sound needs not to click.
pub const CUSHION_MS: u64 = 40;

/// Where the playout ring is meant to stand at the start of a tick, in ms, on the
/// desktop and the phone alike: below it the engine pulls two frames a tick, and
/// a ring that stands clearly above it for a second gives one frame back
/// (`Settle`). The owner's choice of 2026-10-10, against ~80 ms on the
/// phone and ~58 on the desktop before (`veldproef-build157.md`): nothing pulled
/// the ring down between the refill threshold and the bleed-off ceiling, so it
/// stood wherever its history had left it. Build 158; `laagste_ms` in the
/// measurement's `ring` line says whether it is too tight for a phone.
pub const RING_TARGET_MS: u64 = 40;

/// Above this the engine writes nothing at all (the old bleed-off, 200 ms - on
/// the desktop it was 100 until build 158, the items of two channels read as one).
pub const RING_CEILING_MS: u64 = 200;

/// The ticks `Settle` looks at before it decides: one second, so a single late
/// tick or a burst of arrivals does not cost audio, and never more than one
/// frame let go per second.
pub const SETTLE_TICKS: u32 = 50;

/// How many of those seconds `Settle` remembers the noise of: a second that
/// happens to read only at the top of its swing must not make the ring look
/// calm (build 159; review of 158).
pub const SETTLE_MEMORY: usize = 10;

/// After this long without sound, frames are written as before.
pub const WINDOW: Duration = Duration::from_millis(1000);

/// What happened after a release, said once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The first frame with sound: how long after the release, how much silence
    /// was not written, and the ring's fill when the sound went in.
    FirstSound { after_ms: u64, skipped_ms: u64, ring_ms: u64 },
    /// The window ended without sound.
    NoSound { skipped_ms: u64 },
}

#[derive(Debug, Default)]
pub struct AfterTx {
    last_ptt: bool,
    released_at: Option<Instant>,
    skipped_ms: u64,
}

impl AfterTx {
    /// Follow a PTT (Thetis's, or one Yaesu's); a falling edge opens the window,
    /// a press closes it.
    pub fn ptt(&mut self, ptt: bool, now: Instant) {
        if self.last_ptt && !ptt {
            self.released_at = Some(now);
            self.skipped_ms = 0;
        }
        if ptt {
            self.released_at = None;
        }
        self.last_ptt = ptt;
    }

    /// One RX frame of `frame_ms` with its `peak`, the ring holding `ring_ms`.
    /// Returns whether to write it, and - once per release - what happened.
    pub fn frame(&mut self, peak: f32, ring_ms: u64, frame_ms: u64, now: Instant) -> (bool, Option<Outcome>) {
        let Some(at) = self.released_at else {
            return (true, None);
        };
        if now.duration_since(at) >= WINDOW {
            self.released_at = None;
            return (true, Some(Outcome::NoSound { skipped_ms: self.skipped_ms }));
        }
        if peak <= SILENCE_PEAK {
            if ring_ms >= CUSHION_MS {
                self.skipped_ms += frame_ms;
                return (false, None);
            }
            return (true, None);
        }
        self.released_at = None;
        let after_ms = now.duration_since(at).as_millis() as u64;
        (true, Some(Outcome::FirstSound { after_ms, skipped_ms: self.skipped_ms, ring_ms }))
    }

    /// The ring level, in the ring's own items, below which the engine pulls two
    /// frames per tick. While the window is open the ring is meant to stand at the
    /// cushion, and refilling it to the engine's normal threshold pulled two frames
    /// every tick and emptied the jitter buffer right before the first sound - 7.1
    /// concealed frames per over against 0.7 (round 106). The decision is taken at
    /// the start of a tick, one frame's playout after the skip rule left the ring at
    /// the cushion, so the threshold is the cushion less one frame - at the cushion
    /// itself every tick still pulled two (the simulation test below).
    /// Outside the window: `normal`, untouched.
    pub fn refill_low(&self, normal: usize, rate: u32, stereo: bool, now: Instant) -> usize {
        match self.released_at {
            Some(at) if now.duration_since(at) < WINDOW => crate::measure::ring_items(
                CUSHION_MS.saturating_sub(sdr_remote_core::FRAME_DURATION_MS as u64),
                rate,
                stereo,
            ),
            _ => normal,
        }
    }
}

/// How many frames go into the ring this tick: one, two while the ring stood
/// below `refill_low` when the tick began. Decided once for every stream, since
/// they all mix into the one ring: Thetis pulls this many, and a radio or VRX
/// that plays while Thetis delivers nothing brings this many from its own buffer
/// (build 156). Until then such a radio always brought one, the same as the
/// sound card takes, so a ring that had emptied once never got its cushion back.
pub fn frames_for_tick(ring_level: usize, refill_low: usize) -> u32 {
    if ring_level < refill_low { 2 } else { 1 }
}

/// The engine's pull rule for Thetis, one frame at a time: keep pulling until
/// this tick's count is reached. Here, and called by the engine, so the window's
/// dynamics can be simulated with the rule itself (build 107).
pub fn keep_pulling(frames_this_tick: u32, ring_level: usize, refill_low: usize) -> bool {
    frames_this_tick < frames_for_tick(ring_level, refill_low)
}

/// A tick as a radio or VRX stream finds it: how many ring samples its part is,
/// and whether it opened the tick itself. Only `open_tick` makes one and
/// `close_tick` takes it back, so "did this stream open the tick" is decided in
/// one place, when the stream starts - it cannot be read too late (build 157;
/// review of 156: reading it one line further down, after `open_tick` had
/// filled the buffer, stayed green and undid the dry-buffer rule).
#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub struct Tick {
    pub target: usize,
    opens: bool,
}

impl Tick {
    pub fn opens(&self) -> bool {
        self.opens
    }
}

/// The part a radio or VRX stream fills this tick. There is one ring, so every
/// stream writes as much as was written before it; when nothing was (Thetis
/// delivered nothing), the stream opens the tick itself with silence to mix
/// into (build 156).
pub fn open_tick(buf: &mut Vec<f32>, frames: u32, frame_samples: usize) -> Tick {
    let opens = buf.is_empty();
    if opens {
        buf.resize(frames as usize * frame_samples, 0.0);
    }
    Tick { target: buf.len(), opens }
}

/// The end of a tick a radio or VRX stream opened: the ring gets what that
/// stream delivered, not the silence `open_tick` laid out for it. A dry buffer
/// gives one concealed frame and stops, as on Thetis ("one concealed frame per
/// tick is all that helps"), and the second frame of a low ring must not become
/// silence instead. Never below one frame - a stream that delivered nothing
/// wrote one frame of silence before build 156 too.
/// A stream that did not open the tick leaves the ring alone.
pub fn close_tick(tick: Tick, buf: &mut Vec<f32>, delivered: usize, frame_samples: usize) {
    if tick.opens {
        buf.truncate(delivered.max(frame_samples));
    }
}

/// The pull down that the refill rule lacks. Per second (`SETTLE_TICKS`) it
/// keeps the lowest and highest reading at the start of a tick; their
/// difference is how much the reading swings on this device - the sound card
/// takes in chunks (2 ms on the phone, 10 on the desktop), so the same ring
/// reads differently tick by tick. One frame is let go at the end of a second
/// only when even its lowest reading stands at the target plus a frame plus the
/// largest swing of the last `SETTLE_MEMORY` seconds: after it the ring cannot
/// read below the target, so no refill can undo it. Build 158 let go at a fixed
/// target plus a frame, and on the phone the next low reading refilled it - a
/// pendulum costing 20 ms each time (`veldproef-build158.md`). A device that
/// swings more gets more room by itself (owner's choice B, 2026-10-10). Never
/// while transmitting; a press starts the second over.
#[derive(Debug, Default)]
pub struct Settle {
    ticks: u32,
    lowest: u64,
    highest: u64,
    swings: [u64; SETTLE_MEMORY],
    next: usize,
}

impl Settle {
    /// One tick, with the ring at its start in ms: true when this tick's audio
    /// should not go into the ring. The frames are still pulled and decoded, so
    /// the jitter buffers keep their pace and the 20 ms are really gone.
    pub fn drain(&mut self, start_ms: u64, ptt: bool) -> bool {
        if ptt {
            self.ticks = 0;
            return false;
        }
        if self.ticks == 0 {
            self.lowest = start_ms;
            self.highest = start_ms;
        }
        self.lowest = self.lowest.min(start_ms);
        self.highest = self.highest.max(start_ms);
        self.ticks += 1;
        if self.ticks < SETTLE_TICKS {
            return false;
        }
        self.ticks = 0;
        self.swings[self.next] = self.highest - self.lowest;
        self.next = (self.next + 1) % SETTLE_MEMORY;
        self.lowest >= self.line()
    }

    /// The lowest reading a second needs before a frame is let go.
    pub fn line(&self) -> u64 {
        let swing = self.swings.iter().copied().max().unwrap_or(0);
        RING_TARGET_MS + sdr_remote_core::FRAME_DURATION_MS as u64 + swing
    }
}

/// The peak of a frame, for `AfterTx::frame`.
pub fn peak(frame: &[f32]) -> f32 {
    frame.iter().fold(0.0f32, |m, s| m.max(s.abs()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One rule per radio, for the measurement only: each radio's copy follows
    /// its own PTT and reports the first sound after its release, but nothing
    /// of a Yaesu is skipped and the ring's cushion stays Thetis's (build 179;
    /// review of 178: skipping drained the radio's jitter buffer). The engine's
    /// wiring and the radios' measurement rows, as a text guard.
    #[test]
    fn each_radio_measures_its_own_after_tx() {
        let engine = include_str!("engine.rs");
        for wired in [
            "after_tx_yaesu.ptt(yaesu_ptt, Instant::now());",
            "after_tx_yaesu2.ptt(yaesu2_ptt, Instant::now());",
            // One block for both radios (build 180): the measurement and its
            // line inside it, each radio's own name, buffer, decoder, PTT and
            // rule in its own call - a call that mixes two radios' arguments
            // is what this pins against (review of 154; E10 of 178).
            "let (_, outcome) = $after_tx.frame(crate::after_tx::peak(&resampled), ring_now, frame_ms, Instant::now());",
            "concat!($name, \" RX after TX: first sound {} ms after the release, ring {} ms, jitter depth {}\"),\n                                                                after_ms, ring_ms, $jb.depth()",
            "concat!($name, \" RX after TX: no sound within {} ms, jitter depth {}\"),\n                                                                crate::after_tx::WINDOW.as_millis(), $jb.depth()",
            // The head says what each place means: two places of one type
            // swapped there compile and swap the roles for both radios
            // (review of 180).
            "($stream:expr, $name:literal, $logged_first:ident, $jb:ident, $wanted:ident,\n                                 $st:ident, $rec:ident, $accum:ident, $count:ident, $volume:ident,\n                                 $model:ident, $ptt:ident, $after_tx:ident, $level:ident) => {",
            "mix_yaesu_rx!(crate::loss_window::Stream::Radio1, \"Radio 1\", yaesu_logged_first, yaesu_jitter_buf, yaesu_wanted,\n                                st_yaesu, rec_yaesu, yaesu_level_accum, yaesu_level_count, yaesu_volume,\n                                yaesu_model, yaesu_ptt, after_tx_yaesu, playback_level_yaesu);",
            "mix_yaesu_rx!(crate::loss_window::Stream::Radio2, \"Radio 2\", yaesu2_logged_first, yaesu2_jitter_buf, yaesu2_wanted,\n                                st_yaesu2, rec_yaesu2, yaesu2_level_accum, yaesu2_level_count, yaesu2_volume,\n                                yaesu2_model, yaesu2_ptt, after_tx_yaesu2, playback_level_yaesu2);",
            // Each radio's row on its own clock, with its own buffer (review of 178, E8/E9).
            "&mut measure_buffers_at_yaesu,\n                        now_ms < measure_buffers_burst_until,\n                        crate::loss_window::Stream::Radio1,\n                        yaesu_jitter_buf.depth(),\n                        yaesu_jitter_buf.target_depth(),",
            "&mut measure_buffers_at_yaesu2,\n                        now_ms < measure_buffers_burst_until,\n                        crate::loss_window::Stream::Radio2,\n                        yaesu2_jitter_buf.depth(),\n                        yaesu2_jitter_buf.target_depth(),",
        ] {
            assert_eq!(engine.matches(wired).count(), 1, "{wired}");
        }
        // Nothing of a Yaesu is skipped, and the cushion is Thetis's alone.
        assert_eq!(engine.matches("after_tx_yaesu.refill_low(").count() + engine.matches("after_tx_yaesu2.refill_low(").count(), 0);
        assert_eq!(engine.matches("let write = if !yaesu").count(), 0);
        assert_eq!(engine.matches("radio_buf.extend_from_slice(&resampled);").count(), 1);
        assert_eq!(engine.matches("mix_yaesu_rx!(").count(), 2, "one call per radio");
        // And no radio by name inside the block: a bare name of radio 1 there
        // compiles (it is in scope where the macro is defined), radio 1 hides
        // it, and radio 2 reads radio 1's state (review of 180).
        let body = &engine[engine.find("macro_rules! mix_yaesu_rx {").unwrap()..];
        let body = &body[..body.find("mix_yaesu_rx!(").unwrap()];
        let named = body.replace("yaesu_rx_meter_cal", "").replace("mix_yaesu_rx", "");
        assert!(!named.contains("yaesu"), "a radio named inside the block");
        assert!(!named.contains("Stream::Radio"), "a radio's stream named inside the block");
        // And as the last line of the arm, outside the measurement: a concealed
        // frame that stayed out of the buffer would keep the fill loop going
        // for ever (review of 179).
        let tail = format!(
            "}}\n{0}radio_buf.extend_from_slice(&resampled);\n{1}}}\n{1}None => break,",
            " ".repeat(52),
            " ".repeat(48)
        );
        assert_eq!(engine.matches(tail.as_str()).count(), 1);
    }

    const Q: f32 = 0.0; // silence
    const S: f32 = 0.05; // sound

    /// The release, the silence skipped above the cushion and written below it,
    /// the first sound written and reported once - and twice in a row without a
    /// restart (working rule 9).
    #[test]
    fn silence_after_a_release_is_skipped_above_the_cushion() {
        let t = Instant::now();
        let ms = |n: u64| t + Duration::from_millis(n);
        let mut a = AfterTx::default();
        assert_eq!(a.frame(Q, 80, 20, t), (true, None), "no release: everything is written");
        for round in 0..2 {
            let base = round * 2000;
            a.ptt(true, ms(base));
            assert_eq!(a.frame(Q, 80, 20, ms(base + 10)), (true, None), "while transmitting: not this rule's business");
            a.ptt(false, ms(base + 100));
            assert_eq!(a.frame(Q, 80, 20, ms(base + 120)), (false, None), "silence above the cushion: skipped");
            assert_eq!(a.frame(Q, 60, 20, ms(base + 140)), (false, None));
            assert_eq!(a.frame(Q, 20, 20, ms(base + 160)), (true, None), "below the cushion: written");
            assert_eq!(
                a.frame(S, 40, 20, ms(base + 280)),
                (true, Some(Outcome::FirstSound { after_ms: 180, skipped_ms: 40, ring_ms: 40 })),
                "the first sound: written and reported"
            );
            assert_eq!(a.frame(Q, 80, 20, ms(base + 300)), (true, None), "after it, silence is written as before");
        }
    }

    /// The engine follows the PTT for this rule every tick and decides every RX1
    /// write with it - the unit tests above cannot see that (build 106).
    #[test]
    fn the_engine_writes_rx_through_the_rule() {
        let src = include_str!("engine.rs");
        let lines: Vec<&str> = src.lines().map(str::trim).collect();
        let count = |l: &str| lines.iter().filter(|x| **x == l).count();
        assert_eq!(count("after_tx.ptt(ptt, Instant::now());"), 1);
        assert_eq!(count("let (write, outcome) = after_tx.frame(sound, ring_now, frame_ms, Instant::now());"), 1);
        assert_eq!(count("if after_tx_write {"), 1);
        // Build 107: both refill decisions take the window's threshold.
        assert_eq!(count("let refill_low = after_tx.refill_low(target_ring_low, playback_rate, stereo_output, Instant::now());"), 1);
        assert_eq!(count("if !crate::after_tx::keep_pulling(frames_this_tick, ring_level, refill_low) {"), 1);
        assert_eq!(src.matches("max_pull").count(), 0, "no second pull rule beside keep_pulling");
        // Build 156: the tick's count is taken once, and every radio and VRX
        // block opens its tick with it - none with a fixed one frame of its own.
        assert_eq!(count("let frames_wanted = crate::after_tx::frames_for_tick(ring_level, refill_low);"), 1);
        // Build 158: the thresholds are ms through the one conversion, and the
        // ring gives a frame back where the rule says so - at the one write.
        assert_eq!(count("let target_ring_low = crate::measure::ring_items(crate::after_tx::RING_TARGET_MS, playback_rate, stereo_output);"), 1);
        assert_eq!(count("let target_ring_high = crate::measure::ring_items(crate::after_tx::RING_CEILING_MS, playback_rate, stereo_output);"), 1);
        assert_eq!(count("let drain_this_tick = !skip_this_tick && ring_settle.drain(ring_start_ms, ptt);"), 1);
        // A let-go tick still pulls: the flag is read at the write and the
        // measurement only, never where the pull is decided (review of 158, M6).
        assert_eq!(src.matches("drain_this_tick").count(), 3, "the definition, the measurement and the write only");
        assert_eq!(count("if !playback_buf.is_empty() && !drain_this_tick {"), 1);
        assert_eq!(src.matches("audio.write_playback_stereo(").count(), 1, "a second way into the ring");
        // Three: the radios' one block (build 180; it was two) and the two VRX.
        assert_eq!(count("let opened = crate::after_tx::open_tick(&mut playback_buf, frames_wanted, frame_samples);"), 3);
        assert_eq!(count("let target_samples = opened.target;"), 3);
        assert_eq!(src.matches("playback_buf.resize(").count(), 1, "only the final stereo write resizes the ring buffer");
        // A stream that opened the tick stops after one concealed frame, and
        // the tick keeps what it delivered (the Thetis rule for a dry buffer).
        // Whether it opened comes out of `open_tick` itself (build 157).
        assert_eq!(count("if dry && opened.opens() {"), 3);
        assert_eq!(count("dry = true;"), 3);
        assert_eq!(src.matches("opens_tick").count(), 0, "a second way to say whether a stream opened");
        for buf in ["radio_buf", "vrx_buf"] {
            let call = format!("crate::after_tx::close_tick(opened, &mut playback_buf, {buf}.len(), frame_samples);");
            assert_eq!(count(&call), if buf == "vrx_buf" { 2 } else { 1 }, "{buf}");
        }
    }

    /// While the window is open the engine refills only to the cushion, so it
    /// pulls one frame per tick once the cushion stands; outside it, the normal
    /// threshold. Closed by the first sound, by a press and by time - and a second
    /// release opens it again without a restart (working rule 9).
    #[test]
    fn the_window_refills_only_to_the_cushion() {
        let t = Instant::now();
        let ms = |n: u64| t + Duration::from_millis(n);
        let normal = 2880; // any value other than the cushion
        let below = CUSHION_MS - sdr_remote_core::FRAME_DURATION_MS as u64;
        let cushion_mono = crate::measure::ring_items(below, 48_000, false);
        let cushion_stereo = crate::measure::ring_items(below, 48_000, true);
        let mut a = AfterTx::default();
        assert_eq!(a.refill_low(normal, 48_000, false, t), normal, "no release: the normal threshold");
        for round in 0..2 {
            let base = round * 3000;
            a.ptt(true, ms(base));
            assert_eq!(a.refill_low(normal, 48_000, false, ms(base + 10)), normal, "while transmitting");
            a.ptt(false, ms(base + 100));
            assert_eq!(a.refill_low(normal, 48_000, false, ms(base + 120)), cushion_mono, "window open: the cushion less one frame");
            assert_eq!(a.refill_low(normal, 48_000, true, ms(base + 120)), cushion_stereo, "in the ring's own items");
            assert_eq!(a.frame(S, 40, 20, ms(base + 280)).0, true);
            assert_eq!(a.refill_low(normal, 48_000, false, ms(base + 300)), normal, "the first sound closes it");
        }
        // Closed by time, even when no frame came to notice it.
        a.ptt(true, ms(10_000));
        a.ptt(false, ms(10_100));
        assert_eq!(a.refill_low(normal, 48_000, false, ms(10_100) + WINDOW), normal);
        // Closed by a press.
        a.ptt(false, ms(20_000));
        a.ptt(true, ms(20_000));
        a.ptt(false, ms(20_100));
        a.ptt(true, ms(20_150));
        assert_eq!(a.refill_low(normal, 48_000, false, ms(20_200)), normal);
    }

    /// The window's dynamics, tick by tick, with the engine's own pull rule: from
    /// any ring level at the release, the engine settles on one frame per tick
    /// within two ticks and the ring stays around the cushion. Build 107's first
    /// attempt held the refill threshold AT the cushion: the skip rule capped the
    /// ring there at the end of a tick, the sound card took 20 ms before the next,
    /// and every tick started below it and pulled two - the drain of round 106,
    /// 20 ms lower. Units: rate 1000 mono, so items are ms.
    #[test]
    fn after_a_release_the_engine_pulls_one_frame_per_tick() {
        let t = Instant::now();
        for start in [0u64, 15, 20, 25, 35, 39, 41, 60, 80] {
            let mut a = AfterTx::default();
            a.ptt(true, t);
            a.ptt(false, t);
            let mut ring = start;
            for tick in 0..12u64 {
                let now = t + Duration::from_millis(20 * tick + 1);
                let low = a.refill_low(60, 1000, false, now);
                let (mut frames, mut added) = (0u32, 0u64);
                while keep_pulling(frames, ring as usize, low) {
                    frames += 1;
                    if a.frame(Q, ring + added, 20, now).0 {
                        added += 20;
                    }
                }
                if tick >= 2 {
                    assert_eq!(frames, 1, "start {start} ms, tick {tick}: one frame per tick");
                    assert!((20..=60).contains(&ring), "start {start} ms, tick {tick}: ring {ring} ms");
                }
                ring = (ring + added).saturating_sub(20); // the sound card, between two ticks
            }
        }
    }

    /// A station that plays only a radio (or only VRX): Thetis delivers nothing,
    /// so the radio opens every tick. After a gap the ring stands empty; it has
    /// to climb back to the refill threshold, as it does with Thetis - and with
    /// Thetis playing, the radio writes exactly as much as Thetis did, never
    /// more (one ring). Units: rate 1000 mono, so items are ms (build 156).
    #[test]
    fn a_radio_alone_brings_the_ring_back_to_its_cushion() {
        let low = 60;
        for start in [0usize, 10, 20, 40] {
            let mut ring = start;
            for tick in 0..20 {
                let mut buf = Vec::new(); // Thetis wrote nothing this tick
                let opened = open_tick(&mut buf, frames_for_tick(ring, low), 20);
                assert!(opened.opens());
                let n = opened.target;
                assert_eq!(buf.len(), n);
                close_tick(opened, &mut buf, n, 20);
                if tick >= 5 {
                    assert!(ring >= 40, "start {start} ms, tick {tick}: ring {ring} ms never came back");
                    // ...and no further than one frame over the threshold: a
                    // rule that refills too far stays at rest above it (build
                    // 157; review of 156: only the build-107 test caught that).
                    assert!(ring < low + 20, "start {start} ms, tick {tick}: ring {ring} ms overshoots");
                }
                ring = (ring + n).saturating_sub(20); // the sound card, between two ticks
            }
        }
        // A radio that opens a low tick but has one frame - or nothing - to give:
        // the ring gets that, not a second frame of silence.
        for (delivered, kept) in [(40usize, 40usize), (25, 25), (20, 20), (5, 20), (0, 20)] {
            let mut buf = Vec::new();
            let opened = open_tick(&mut buf, 2, 20);
            close_tick(opened, &mut buf, delivered, 20);
            assert_eq!(buf.len(), kept, "delivered {delivered}");
        }
        // Thetis wrote one frame while the ring was low: the radio follows it.
        let mut buf = vec![0.5; 20];
        let opened = open_tick(&mut buf, 2, 20);
        assert_eq!((opened.target, opened.opens()), (20, false));
        close_tick(opened, &mut buf, 0, 20);
        assert_eq!(buf, vec![0.5; 20], "the radio does not touch what Thetis wrote, at either end");
        // Thetis wrote two frames and the radio had one: still Thetis's two.
        let mut buf = vec![0.5; 40];
        let opened = open_tick(&mut buf, 1, 20);
        close_tick(opened, &mut buf, 20, 20);
        assert_eq!(buf.len(), 40, "a stream that did not open the tick cut it short");
        // And the count is the one Thetis pulls by.
        for ring in [0usize, 39, 40, 59, 60, 61, 200] {
            let mut pulled = 0;
            while keep_pulling(pulled, ring, low) {
                pulled += 1;
            }
            assert_eq!(pulled, frames_for_tick(ring, low), "ring {ring}");
        }
    }

    /// The engine's two rules on a ring that reads with the swing of a real
    /// device: two frames below the target, one let go per `Settle`. `swing`
    /// gives the reading's offset per tick (the true level is what the sound
    /// card leaves; the reading is where the tick happens to fall in its
    /// chunks). Returns, per tick, the reading, whether it refilled and whether
    /// it let go. Units: rate 1000 mono, so items are ms.
    fn run(start: i64, ticks: usize, swing: impl Fn(usize) -> i64) -> Vec<(i64, bool, bool)> {
        let target = RING_TARGET_MS as usize;
        let mut level = start;
        let mut settle = Settle::default();
        (0..ticks)
            .map(|tick| {
                let reading = (level + swing(tick)).max(0);
                let frames = frames_for_tick(reading as usize, target);
                let drain = settle.drain(reading as u64, false);
                let written = if drain { 0 } else { frames as i64 * 20 };
                level = (level + written - 20).max(0); // the sound card, between two ticks
                (reading, frames == 2, drain)
            })
            .collect()
    }

    /// No let-go that a refill undoes within two seconds, from any start, on a
    /// clean reading, on the phone's 2 ms swing and on the desktop's 20 ms - with
    /// every fourth second reading only at the top of its swing, the case that
    /// fooled a one-second memory (build 159; review of 158: the build-158 test
    /// read without any swing and the field pendulummed). And at rest the ring
    /// stands at or above the target and below the line, with at most one
    /// let-go per second.
    #[test]
    fn the_ring_settles_without_a_pendulum_on_a_swinging_reading() {
        let desktop = |t: usize| if (t / 50) % 4 == 3 { 10 } else { [-10, 0, 10][t % 3] };
        let phone = |t: usize| if (t / 50) % 4 == 3 { 0 } else { [0, -2][t % 2] };
        let clean = |_: usize| 0;
        let cases: [(&str, &dyn Fn(usize) -> i64, i64); 3] = [("clean", &clean, 0), ("phone", &phone, 2), ("desktop", &desktop, 20)];
        for (name, swing, width) in cases {
            for start in [0i64, 20, 39, 40, 59, 60, 79, 80, 100, 150, 199] {
                let ticks = run(start, 3000, swing);
                let drains: Vec<usize> = (0..ticks.len()).filter(|&t| ticks[t].2).collect();
                for w in drains.windows(2) {
                    assert!(w[1] - w[0] >= SETTLE_TICKS as usize, "{name}, start {start}: two let-goes within a second");
                }
                for &d in &drains {
                    let undone = ticks[d + 1..(d + 101).min(ticks.len())].iter().any(|t| t.1);
                    assert!(!undone, "{name}, start {start}: the let-go at tick {d} was refilled within 2 s");
                }
                // From the ceiling: at most eight let-goes, so at rest after ten
                // seconds; from then on no refill and the lowest reading of each
                // second between the target and the line.
                for second in ticks[750..].chunks(50) {
                    let lowest = second.iter().map(|t| t.0).min().unwrap();
                    assert!(second.iter().all(|t| !t.1), "{name}, start {start}: a refill at rest");
                    assert!(lowest >= 40 - width && lowest < 60 + 2 * width, "{name}, start {start}: rests at {lowest} ms");
                }
            }
        }
    }

    /// The second itself: 50 ticks, the decision at its end only, the line
    /// from the largest swing remembered, a press starting it over, and the
    /// memory holding a noisy second for `SETTLE_MEMORY` seconds (build 159;
    /// review of 158: a count not started over, two ticks for a second, and a
    /// count surviving a press all stayed green).
    #[test]
    fn a_second_decides_once_from_its_lowest_reading_and_the_remembered_swing() {
        assert_eq!(SETTLE_TICKS, 50, "one second at 20 ms a tick");
        let mut s = Settle::default();
        assert_eq!(s.line(), 60);
        for _ in 0..49 {
            assert!(!s.drain(80, false), "not before the second is over");
        }
        assert!(s.drain(80, false), "a calm second at 80: let go");
        for _ in 0..49 {
            assert!(!s.drain(60, false));
        }
        assert!(s.drain(60, false), "lowest 60, no swing: the line is 60");
        // A press starts the second over.
        for _ in 0..30 {
            assert!(!s.drain(90, false));
        }
        assert!(!s.drain(90, true));
        for _ in 0..49 {
            assert!(!s.drain(90, false));
        }
        assert!(s.drain(90, false), "fifty ticks after the press, not twenty");
        // A swinging second raises the line, and it stays raised.
        for t in 0..50 {
            assert!(!s.drain(if t % 2 == 0 { 70 } else { 90 }, false), "lowest 70 is under 60 + 20");
        }
        assert_eq!(s.line(), 80);
        for _ in 0..SETTLE_MEMORY - 1 {
            for t in 0..50 {
                assert!(!s.drain(79, false) || t < 49);
            }
        }
        assert_eq!(s.line(), 80, "still remembered after nine calm seconds");
        for _ in 0..50 {
            s.drain(79, false);
        }
        assert_eq!(s.line(), 60, "forgotten after ten");
    }

    /// No sound within the window: frames are written again and it is said once.
    #[test]
    fn the_window_ends_without_sound() {
        let t = Instant::now();
        let mut a = AfterTx::default();
        a.ptt(true, t);
        a.ptt(false, t);
        assert_eq!(a.frame(Q, 80, 20, t + Duration::from_millis(20)), (false, None));
        assert_eq!(
            a.frame(Q, 80, 20, t + WINDOW),
            (true, Some(Outcome::NoSound { skipped_ms: 20 }))
        );
        assert_eq!(a.frame(Q, 80, 20, t + WINDOW + Duration::from_millis(20)), (true, None));
        // A press during the window closes it.
        a.ptt(true, t + Duration::from_secs(3));
        a.ptt(false, t + Duration::from_secs(4));
        a.ptt(true, t + Duration::from_secs(4) + Duration::from_millis(50));
        assert_eq!(a.frame(Q, 80, 20, t + Duration::from_secs(5)), (true, None));
        assert!(peak(&[0.1, -0.3, 0.2]) == 0.3);
    }
}
