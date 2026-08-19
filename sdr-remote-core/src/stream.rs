// SPDX-License-Identifier: GPL-2.0-or-later

//! One receive stream's decoding, in one place.
//!
//! Every audio stream in ThetisLink can arrive narrowband (8 kHz Opus) or
//! wideband (16 kHz), and the two are separate decoder instances with separate
//! history. Getting that choice wrong is not loud: a decoder that has never
//! decoded anything conceals SILENCE, so the fault reads as "the audio stopped"
//! rather than as a bug. Four of six paths had it wrong for months for exactly
//! that reason, and the concealment ones were missed because they are spelled
//! `decode_plc` and `decode_fec` rather than `decode` (2026-08-16).
//!
//! So the choice is not made at the call site any more. This type owns both
//! decoders and the format of the last real frame, and offers `decode`,
//! `recover` and `conceal`. There is no way to ask it to conceal without it
//! knowing which decoder holds the history.
//!
//! It generates nothing. A gap is filled by Opus' own concealment and by
//! nothing else, which is what makes it sound like the operator's own receiver:
//! it is extrapolated from their signal. That has been true since the first
//! build in February. A synthetic noise generator lived here from 2026-08-16 to
//! 2026-08-19 and was removed - it arrived in the same commit as the extension
//! to every channel, though only the extension had been asked for, and it made
//! a gap louder and sharper than the band it stands in for.
//!
//! The premise it was built on does not hold either. `plc_fades_and_this_is_how_fast`
//! measures the fade on SPEECH; on band noise Opus does not fade at all -
//! measured across four seconds it holds the level of the band. There was
//! therefore nothing for a top-up to do, which is why lowering its level had no
//! audible effect.

use anyhow::Result;

use crate::codec::{OpusDecoder, OpusDecoderWideband};

/// How long a gap keeps being filled before concealment gives up and goes quiet.
/// The connection is declared lost after 6 s, so this only ever runs out when
/// something else is stuck - it is a backstop, not a timer anyone waits for.
const CONCEAL_MAX_FRAMES: u32 = 400; // 8 s at 20 ms

/// One receive stream: both decoders and the format of the last real frame.
pub struct StreamDecoder {
    nb: OpusDecoder,
    wb: OpusDecoderWideband,
    /// Format of the last frame that really arrived. Concealment has no frame
    /// of its own to read a flag from, so this is the best available truth.
    last_wb: bool,
    /// False until a real frame has been decoded. Concealing before that would
    /// be silence anyway; returning nothing says so honestly.
    has_history: bool,
    conceal_frames: u32,
    /// The invariant below is said once per stream, not once per frame.
    said_silent: bool,
}

impl StreamDecoder {
    pub fn new() -> Result<Self> {
        Ok(Self {
            nb: OpusDecoder::new()?,
            wb: OpusDecoderWideband::new()?,
            last_wb: false,
            has_history: false,
            conceal_frames: 0,
            said_silent: false,
        })
    }

    /// Format of the last real frame - what a concealed frame is played back as.
    pub fn wideband(&self) -> bool {
        self.last_wb
    }

    /// True once a real frame has been decoded on this stream.
    pub fn has_history(&self) -> bool {
        self.has_history
    }

    /// Drop all history. Used on reconnect: the far side starts a new stream and
    /// the old history would be extrapolated into the new one.
    pub fn reset(&mut self) -> Result<()> {
        // Flags first, decoders second. Building a decoder can fail, and `?`
        // would leave a stream carrying an old decoder while still claiming
        // history - so `conceal()` would go on extrapolating from the session
        // this call was meant to end. Cleared up front, a failure leaves a
        // stream that believes it is empty: silence, not the wrong audio.
        self.last_wb = false;
        self.has_history = false;
        self.conceal_frames = 0;
        self.said_silent = false;
        self.nb = OpusDecoder::new()?;
        self.wb = OpusDecoderWideband::new()?;
        Ok(())
    }

    /// A frame that really arrived. `wideband` comes from the packet header.
    pub fn decode(&mut self, opus: &[u8], wideband: bool) -> Option<Vec<i16>> {
        let r = if wideband { self.wb.decode(opus) } else { self.nb.decode(opus) };
        self.accept(r, wideband)
    }

    /// A lost frame rebuilt from the in-band redundancy of the NEXT frame.
    /// `wideband` is that next frame's flag, not the last-seen one: the
    /// redundancy travels inside the packet it was copied into, so its format
    /// is that packet's format. It is real audio, so it counts as history.
    pub fn recover(&mut self, next_opus: &[u8], wideband: bool) -> Option<Vec<i16>> {
        let r = if wideband {
            self.wb.decode_fec(next_opus)
        } else {
            self.nb.decode_fec(next_opus)
        };
        self.accept(r, wideband)
    }

    fn accept(&mut self, r: Result<Vec<i16>>, wideband: bool) -> Option<Vec<i16>> {
        match r {
            Ok(pcm) => {
                self.last_wb = wideband;
                self.has_history = true;
                self.conceal_frames = 0;
                Some(pcm)
            }
            Err(_) => None,
        }
    }

    /// Fill a gap, using the decoder that holds the history. Opus' own
    /// concealment and nothing else.
    ///
    /// A generator that added synthetic noise on top lived here between
    /// 2026-08-16 and 2026-08-19. It arrived in the same commit that brought
    /// concealment to every channel, although only the second of those had been
    /// asked for, and it is what made a gap sound louder and sharper than the
    /// band it stands in for. What this project has called comfort noise since
    /// February is what the codec produces: extrapolated from the operator's own
    /// signal, so it sounds like their own receiver.
    ///
    /// Returns nothing when this stream has never carried audio, or when the gap
    /// has gone on so long that something else is wrong.
    ///
    /// KNOWN AND DELIBERATELY NOT FIXED: a gap early in a listening session
    /// sounds like silence rather than like the band. Opus needs to have decoded
    /// for a while before its concealment has anything to extrapolate from, and
    /// the two formats are separate decoders with separate history - so
    /// switching between narrowband and wideband starts the other one from
    /// nothing.
    ///
    /// Measured by the operator on 2026-08-19, one variable at a time:
    ///
    /// | after                        | a two-second gap sounds like |
    /// |------------------------------|------------------------------|
    /// | audio just switched on       | silence                      |
    /// | a minute of narrowband       | the band, at its level       |
    /// | switching to wideband        | silence again                |
    /// | minutes of wideband          | the band, at its level       |
    ///
    /// Measured again on 2026-08-19, switching only the format on one channel:
    /// **the two run-ins are not the same length.** Narrowband carries the band
    /// again after seconds; wideband needs far longer. So the same test on the
    /// same channel answers differently depending on which format is running,
    /// and a narrowband stream (a radio) sounds unlike a wideband one (RX1 with
    /// wideband on) with everything else equal. That is the codec, not the
    /// channel - the two were compared by putting RX1 on narrowband, where it
    /// behaved like the radio.
    ///
    /// A warning for whoever measures this next: staged packet loss at this end
    /// makes the client report loss, and the server answers by switching error
    /// correction on for about twenty seconds (`LossProtection` in
    /// `audio_loops.rs`). A gap inside that window is filled by the correction
    /// and not by this function, which reads as a wildly inconsistent result.
    /// One gap per observation, and leave the link alone in between.
    ///
    /// Nothing on this side estimates a level any more, so the run-in is the
    /// codec's own state. It costs nothing in practice - a station left running
    /// is past it, in seconds on narrowband and rather longer on wideband - and
    /// closing it would mean feeding the idle decoder frames it never received.
    pub fn conceal(&mut self) -> Option<Vec<i16>> {
        if !self.has_history || self.conceal_frames >= CONCEAL_MAX_FRAMES {
            return None;
        }
        self.conceal_frames += 1;
        let r = if self.last_wb { self.wb.decode_plc() } else { self.nb.decode_plc() };
        let pcm = r.ok()?;
        self.note_if_codec_went_silent(rms_of(&pcm));
        Some(pcm)
    }

    /// The check that would have found the wrong-decoder fault on the first gap
    /// of the first wideband session, in anyone's log, without anybody
    /// listening for it: a stream that has carried audio cannot conceal
    /// silence. That is what being on the wrong decoder looks like from the
    /// outside, and it is what nothing was watching for.
    ///
    /// Said once per stream, because a broken gap makes fifty of these a second.
    fn note_if_codec_went_silent(&mut self, level: f32) {
        if self.said_silent || !self.has_history || level > 0.0 {
            return;
        }
        self.said_silent = true;
        log::warn!(
            "concealment produced silence on a stream that has carried audio (wideband={}) - the decoder that conceals is not the one holding the history",
            self.last_wb
        );
    }
}

fn rms_of(pcm: &[i16]) -> f32 {
    if pcm.is_empty() {
        return 0.0;
    }
    let sum: f64 = pcm.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum / pcm.len() as f64).sqrt() as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{OpusEncoder, OpusEncoderWideband};
    use crate::{FRAME_SAMPLES, FRAME_SAMPLES_WIDEBAND};

    fn band_noise(n: usize, seed: &mut u32, scale: i16) -> Vec<i16> {
        (0..n)
            .map(|_| {
                *seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (((*seed >> 16) as i32 as f32 / 32_768.0) * scale as f32) as i16
            })
            .collect()
    }

    fn peak(pcm: &[i16]) -> i16 {
        pcm.iter().map(|s| s.saturating_abs()).max().unwrap_or(0)
    }

    /// The whole point of the type: a wideband stream conceals with the wideband
    /// decoder. Before this existed the concealment always ran on the narrowband
    /// one, which on a wideband stream has never decoded anything - so it
    /// produced a full-length frame of silence and the operator heard the audio
    /// simply stop.
    #[test]
    fn wideband_stream_conceals_with_the_wideband_decoder() {
        let mut enc = OpusEncoderWideband::new().unwrap();
        let mut s = StreamDecoder::new().unwrap();
        let mut seed = 7u32;

        for _ in 0..25 {
            let pcm = band_noise(FRAME_SAMPLES_WIDEBAND, &mut seed, 6000);
            let packet = enc.encode(&pcm).unwrap();
            assert!(s.decode(&packet, true).is_some());
        }
        assert!(s.wideband(), "stream should remember it is wideband");

        let hidden = s.conceal().expect("a stream with history must conceal");
        assert_eq!(hidden.len(), FRAME_SAMPLES_WIDEBAND, "wideband frame length");
        assert!(peak(&hidden) > 0, "concealment must carry audio, not silence");
    }

    /// Narrowband keeps working the way it always did.
    #[test]
    fn narrowband_stream_conceals_with_the_narrowband_decoder() {
        let mut enc = OpusEncoder::new().unwrap();
        let mut s = StreamDecoder::new().unwrap();
        let mut seed = 11u32;

        for _ in 0..25 {
            let pcm = band_noise(FRAME_SAMPLES, &mut seed, 6000);
            let packet = enc.encode(&pcm).unwrap();
            assert!(s.decode(&packet, false).is_some());
        }
        assert!(!s.wideband());

        let hidden = s.conceal().unwrap();
        assert_eq!(hidden.len(), FRAME_SAMPLES);
        assert!(peak(&hidden) > 0);
    }

    /// A stream that has never carried audio says so instead of handing out a
    /// frame of silence that the mixer cannot tell apart from real quiet audio.
    #[test]
    fn a_stream_that_never_carried_audio_does_not_conceal() {
        let mut s = StreamDecoder::new().unwrap();
        assert!(s.conceal().is_none());
    }

    /// A gap does not fade to nothing.
    ///
    /// This is the property the whole feature rests on, and the reason no
    /// generated noise is needed: on band noise Opus keeps extrapolating instead
    /// of decaying, so a dropout still sounds like the band. (The often-quoted
    /// fade within about 260 ms is a speech figure and does not apply here.)
    ///
    /// Stated as a comparison of the gap against ITSELF - early against late -
    /// because there is no fair absolute reference. Concealment finds its own
    /// level rather than holding the loudness of the last real frame, so
    /// measuring it against that frame would measure the signal, not the fade.
    #[test]
    fn concealment_stays_audible_for_seconds() {
        let mut enc = OpusEncoderWideband::new_rx_continuous().unwrap();
        let mut s = StreamDecoder::new().unwrap();
        let mut seed = 99u32;
        for _ in 0..50 {
            let pcm = band_noise(FRAME_SAMPLES_WIDEBAND, &mut seed, 4000);
            let packet = enc.encode(&pcm).unwrap();
            s.decode(&packet, true).unwrap();
        }

        let mean_over = |n: usize, s: &mut StreamDecoder| {
            let mut sum = 0.0f64;
            for _ in 0..n {
                sum += rms_of(&s.conceal().expect("still concealing")) as f64;
            }
            (sum / n as f64) as f32
        };

        // Half a second in, once the codec has settled into filling the gap.
        let _ = mean_over(25, &mut s);
        let early = mean_over(25, &mut s);
        assert!(early > 0.0, "concealment went silent immediately");

        // Three seconds in.
        let _ = mean_over(75, &mut s);
        let late = mean_over(25, &mut s);

        // One-sided, deliberately, and worth knowing: nothing in this file can
        // fail on "too loud" any more, and "too loud" was the complaint that
        // started this. There is no honest upper bound left to assert - the
        // level is entirely Opus' now, so a ceiling would pin a codec property
        // rather than a choice of ours. `level_over_a_gap` is the instrument if
        // the question comes back.
        assert!(
            late > early * 0.5,
            "a gap faded away: {early:.1} at half a second, {late:.1} at three seconds"
        );
    }




    /// It does not run forever. If a gap outlives every timeout the stream goes
    /// quiet rather than concealing for the rest of the session.
    #[test]
    fn concealment_gives_up_eventually() {
        let mut enc = OpusEncoder::new_rx_continuous().unwrap();
        let mut s = StreamDecoder::new().unwrap();
        let mut seed = 3u32;
        for _ in 0..10 {
            let pcm = band_noise(FRAME_SAMPLES, &mut seed, 4000);
            let packet = enc.encode(&pcm).unwrap();
            s.decode(&packet, false).unwrap();
        }
        for _ in 0..CONCEAL_MAX_FRAMES {
            assert!(s.conceal().is_some());
        }
        assert!(s.conceal().is_none(), "concealment should stop at the backstop");
    }

    /// A real frame ends the gap and starts the budget over, which is what makes
    /// a ragged link keep hissing while a dead one falls quiet: every frame that
    /// still gets through refills the history.
    #[test]
    fn a_real_frame_restarts_the_concealment_budget() {
        let mut enc = OpusEncoder::new_rx_continuous().unwrap();
        let mut s = StreamDecoder::new().unwrap();
        let mut seed = 5u32;
        let pcm = band_noise(FRAME_SAMPLES, &mut seed, 4000);
        let packet = enc.encode(&pcm).unwrap();
        s.decode(&packet, false).unwrap();

        for _ in 0..CONCEAL_MAX_FRAMES {
            assert!(s.conceal().is_some());
        }
        assert!(s.conceal().is_none());

        let packet = enc.encode(&band_noise(FRAME_SAMPLES, &mut seed, 4000)).unwrap();
        s.decode(&packet, false).unwrap();
        assert!(s.conceal().is_some(), "a real frame should reopen the budget");
    }




    /// What the invariant is worth, measured through the route that runs.
    ///
    /// `note_if_codec_went_silent` only fires on EXACTLY zero, and the only
    /// measurement of how likely that is went out with the generator: in the
    /// DECODE branch Opus turns digital silence into 0.1, not 0. Nobody had ever
    /// measured the PLC branch, so the one self-reporting check in the audio path
    /// was of unknown value. This measures it.
    ///
    /// It also drives `conceal()` rather than calling the check directly, which
    /// is the difference between testing the branch that runs and the branch you
    /// just wrote - a distinction review caught twice in three days.
    #[test]
    fn a_decoder_without_history_conceals_exact_silence() {
        let mut s = StreamDecoder::new().unwrap();
        // What being on the wrong decoder looks like: the stream is believed to
        // have carried audio, but this decoder has never seen a frame of it.
        s.has_history = true;

        let hidden = s.conceal().expect("conceal returns a frame");
        let level = rms_of(&hidden);
        assert_eq!(
            level, 0.0,
            "PLC on a decoder with no history returned {level:.3}, not exact zero -              the invariant below tests `level > 0.0` and can therefore never fire"
        );
        assert!(s.said_silent, "the invariant did not report it");
    }

    /// Not a check - a measurement, kept because reading it is what found the
    /// fault. It was removed with the generator and put back, because the level
    /// in a gap is now entirely Opus' and nothing here pins it: see the note on
    /// `concealment_stays_audible_for_seconds`.
    ///
    /// The reference changed with the generator: dB here is now relative to the
    /// last real frame, where it used to be relative to the estimated noise
    /// floor. A run from before that removal is not comparable with one from
    /// after it.
    ///
    /// `cargo test -p sdr-remote-core --lib level_over_a_gap -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn level_over_a_gap() {
        let mut enc = OpusEncoderWideband::new_rx_continuous().unwrap();
        let mut s = StreamDecoder::new().unwrap();
        let mut seed = 0x51ee_d00du32;
        let mut live = 0.0f32;
        for _ in 0..80 {
            let pcm = band_noise(FRAME_SAMPLES_WIDEBAND, &mut seed, 4000);
            live = rms_of(&s.decode(&enc.encode(&pcm).unwrap(), true).unwrap());
        }
        println!("last real frame = {live:.1}");
        for i in 0..200 {
            let f = s.conceal().expect("concealing");
            if i % 10 == 0 {
                let lvl = rms_of(&f);
                println!("  frame {:3} ({:4} ms): {:8.1}  = {:+5.1} dB vs the last real frame",
                         i, i * 20, lvl, 20.0 * (lvl / live).log10());
            }
        }
    }

    /// Said once, not fifty times a second for as long as the gap lasts.
    #[test]
    fn the_silence_check_speaks_once_per_stream() {
        let mut s = StreamDecoder::new().unwrap();
        s.has_history = true;
        s.note_if_codec_went_silent(0.0);
        assert!(s.said_silent);
        // A second silent frame must not reset or repeat it; the flag is the
        // whole mechanism, so this is what "once" means in code.
        s.note_if_codec_went_silent(0.0);
        assert!(s.said_silent);
        s.reset().unwrap();
        assert!(!s.said_silent, "a new session may report it again");
    }

    /// Reset drops the history, so a new session does not get the old one
    /// extrapolated into it.
    #[test]
    fn reset_forgets_the_stream() {
        let mut enc = OpusEncoderWideband::new().unwrap();
        let mut s = StreamDecoder::new().unwrap();
        let mut seed = 17u32;
        let packet = enc.encode(&band_noise(FRAME_SAMPLES_WIDEBAND, &mut seed, 6000)).unwrap();
        s.decode(&packet, true).unwrap();
        assert!(s.has_history() && s.wideband());

        s.reset().unwrap();
        assert!(!s.has_history());
        assert!(!s.wideband());
        assert!(s.conceal().is_none());
    }
}
