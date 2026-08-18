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
//! It also generates the comfort noise. Opus concealment is built to bridge a
//! lost packet, not a lost link: measured, it is inaudible after about 260 ms
//! (see `codec::tests::plc_fades_and_this_is_how_fast`). A short dropout should
//! stay audible as band noise for as long as the link is still considered up,
//! so once the codec's own concealment fades this fills the rest in at the
//! stream's own noise floor.

use anyhow::Result;

use crate::codec::{OpusDecoder, OpusDecoderWideband};

/// How long concealment keeps making noise before it gives up and goes quiet.
/// The connection is declared lost after 6 s, so this only ever runs out when
/// something else is stuck - it is a backstop, not a timer anyone waits for.
const CONCEAL_MAX_FRAMES: u32 = 400; // 8 s at 20 ms

/// Slow rise of the noise-floor estimate. Falls instantly, rises over seconds,
/// so a burst of speech does not drag the comfort noise up with it.
const FLOOR_RISE: f32 = 0.001;

/// How loud the comfort noise sits against the band noise it stands in for.
///
/// Matching the floor exactly is the honest choice on paper and the wrong one
/// by ear: a gap then announces itself instead of passing for the band, because
/// generated noise is steadier than the real thing and steadiness is what the
/// ear picks out. Three decibels under, asked for after listening to it
/// (2026-08-17).
const COMFORT_LEVEL: f32 = 0.708; // -3 dB

/// One receive stream: both decoders, the format of the last real frame, and
/// the noise floor that the comfort noise is generated at.
pub struct StreamDecoder {
    nb: OpusDecoder,
    wb: OpusDecoderWideband,
    /// Format of the last frame that really arrived. Concealment has no frame
    /// of its own to read a flag from, so this is the best available truth.
    last_wb: bool,
    /// False until a real frame has been decoded. Concealing before that would
    /// be silence anyway; returning nothing says so honestly.
    has_history: bool,
    /// Quietest recent frame, in i16 units - the band noise under the signal.
    floor_rms: f32,
    conceal_frames: u32,
    /// The invariant below is said once per stream, not once per frame.
    said_silent: bool,
    noise_seed: u32,
    noise_lp: f32,
}

impl StreamDecoder {
    pub fn new() -> Result<Self> {
        Ok(Self {
            nb: OpusDecoder::new()?,
            wb: OpusDecoderWideband::new()?,
            last_wb: false,
            has_history: false,
            floor_rms: 0.0,
            conceal_frames: 0,
            said_silent: false,
            noise_seed: 0x5eed_1234,
            noise_lp: 0.0,
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
        self.nb = OpusDecoder::new()?;
        self.wb = OpusDecoderWideband::new()?;
        self.last_wb = false;
        self.has_history = false;
        self.floor_rms = 0.0;
        self.conceal_frames = 0;
        self.said_silent = false;
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
                let rms = rms_of(&pcm);
                if !self.has_floor() || rms < self.floor_rms {
                    self.floor_rms = rms;
                } else {
                    self.floor_rms += (rms - self.floor_rms) * FLOOR_RISE;
                }
                Some(pcm)
            }
            Err(_) => None,
        }
    }

    fn has_floor(&self) -> bool {
        self.floor_rms > 0.0
    }

    /// Fill a gap. Uses the decoder that holds the history, then tops the result
    /// up to the stream's noise floor so the gap keeps sounding like the band
    /// instead of falling silent once the codec's own concealment has faded.
    ///
    /// Returns nothing when this stream has never carried audio, or when the gap
    /// has gone on so long that something else is wrong.
    pub fn conceal(&mut self) -> Option<Vec<i16>> {
        if !self.has_history || self.conceal_frames >= CONCEAL_MAX_FRAMES {
            return None;
        }
        self.conceal_frames += 1;
        let r = if self.last_wb { self.wb.decode_plc() } else { self.nb.decode_plc() };
        let mut pcm = r.ok()?;

        // What the CODEC produced, measured before anything is added to it.
        //
        // The order is the whole point and it is easy to lose: the check wants a
        // noise floor to exist, and where one exists the comfort noise has
        // already filled the frame - so measured afterwards it can never fire.
        // The test below fails if these two lines ever swap.
        let bare = rms_of(&pcm);
        self.add_comfort_noise(&mut pcm);
        self.note_if_codec_went_silent(bare);
        Some(pcm)
    }

    /// The check that would have found the wrong-decoder fault on the first gap
    /// of the first wideband session, in anyone's log, without anybody
    /// listening for it: a stream that has carried audio cannot conceal
    /// silence. That is what being on the wrong decoder looks like from the
    /// outside, and it is what nothing was watching for.
    ///
    /// `bare` is the codec's own output, measured before the comfort noise.
    /// Said once per stream, because a broken gap makes fifty of these a second.
    fn note_if_codec_went_silent(&mut self, bare: f32) {
        if self.said_silent || !self.has_floor() || bare > 0.0 {
            return;
        }
        self.said_silent = true;
        log::warn!(
            "concealment produced silence on a stream that has carried audio              (wideband={}, floor {:.1}) - the decoder that conceals is not the              one holding the history",
            self.last_wb, self.floor_rms
        );
    }

    /// Top the frame up to the noise floor. Energy is added, never removed: while
    /// Opus is still extrapolating something real it stays dominant, and the
    /// generated part fades in underneath it as the codec fades out. No step at
    /// the hand-over, because the sum is held constant rather than the parts.
    fn add_comfort_noise(&mut self, pcm: &mut [i16]) {
        if !self.has_floor() || pcm.is_empty() {
            return;
        }
        let have = rms_of(pcm);
        let want = self.floor_rms * COMFORT_LEVEL;
        if have >= want {
            return;
        }
        // Missing energy, not missing amplitude: uncorrelated sources add in
        // power, so this is what brings the sum to the floor.
        let deficit = (want * want - have * have).max(0.0).sqrt();

        // White noise with a gentle tilt sounds like band noise; flat white is
        // hissier than anything a receiver produces.
        let n = pcm.len();
        let mut noise = Vec::with_capacity(n);
        for _ in 0..n {
            self.noise_seed = self.noise_seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let white = ((self.noise_seed >> 16) as i16) as f32 / 32_768.0;
            self.noise_lp += (white - self.noise_lp) * 0.55;
            noise.push(self.noise_lp);
        }
        let noise_rms = (noise.iter().map(|s| s * s).sum::<f32>() / n as f32).sqrt();
        if noise_rms <= 0.0 {
            return;
        }
        let gain = deficit / (noise_rms * 32_768.0);
        for (s, ns) in pcm.iter_mut().zip(noise.iter()) {
            let v = *s as f32 + ns * 32_768.0 * gain;
            *s = v.clamp(-32_768.0, 32_767.0) as i16;
        }
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

    /// The measured fault: Opus concealment fades within about 260 ms, but a
    /// dropout stays audible for as long as the link is still up. The comfort
    /// noise fills the rest in - so seconds into a gap there is still band
    /// noise, at the level the band had.
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
        let floor = s.floor_rms;
        assert!(floor > 0.0, "band noise should set a floor");

        // Three seconds of gap - well past the point where Opus alone is gone.
        for frame in 0..150 {
            let hidden = s.conceal().expect("still concealing");
            let level = rms_of(&hidden);
            // Against the level it is meant to sit at, not merely "above
            // nothing": the point is that a gap keeps sounding like the band,
            // three decibels under it.
            assert!(
                level > floor * COMFORT_LEVEL * 0.8,
                "frame {} at {:.1} fell away from the comfort level ({:.1} of floor {:.1})",
                frame, level, floor * COMFORT_LEVEL, floor
            );
        }
    }

    /// It does not hiss forever. If a gap outlives every timeout the stream goes
    /// quiet rather than generating noise for the rest of the session.
    #[test]
    fn comfort_noise_gives_up_eventually() {
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

    /// Comfort noise is generated at the quietest recent level, not the loudest.
    /// Otherwise a gap that starts during speech would hiss at speech level.
    #[test]
    fn the_floor_follows_the_quiet_parts() {
        let mut enc = OpusEncoder::new_rx_continuous().unwrap();
        let mut s = StreamDecoder::new().unwrap();
        let mut seed = 13u32;

        for _ in 0..20 {
            let packet = enc.encode(&band_noise(FRAME_SAMPLES, &mut seed, 500)).unwrap();
            s.decode(&packet, false).unwrap();
        }
        let quiet_floor = s.floor_rms;

        // Now something loud, as a burst of speech would be.
        for _ in 0..20 {
            let packet = enc.encode(&band_noise(FRAME_SAMPLES, &mut seed, 20000)).unwrap();
            s.decode(&packet, false).unwrap();
        }
        assert!(
            s.floor_rms < quiet_floor * 3.0,
            "loud audio dragged the floor from {:.1} to {:.1}",
            quiet_floor, s.floor_rms
        );
    }

    /// The invariant, through the real route rather than by calling the check.
    ///
    /// The repair for this was written once and never landed - the patch that
    /// carried it failed on its second half and wrote nothing, and the tests
    /// that "proved" it called the helper directly, so they passed against code
    /// that still measured after the noise. A test of the branch you just wrote
    /// instead of the branch that runs. Found by review, twice in three days
    /// (2026-08-18).
    ///
    /// So this drives `conceal()` itself. A decoder with no history conceals
    /// silence - that is `plc_on_a_fresh_decoder_is_silent` - and with a floor
    /// set, the comfort noise then fills the frame. If the check runs after the
    /// noise it sees a full frame and stays quiet; only measuring first can
    /// notice. The two assertions together can only both hold in the right
    /// order.
    #[test]
    fn conceal_notices_a_silent_codec_even_though_the_noise_fills_the_frame() {
        let mut s = StreamDecoder::new().unwrap();
        // As a stream that has carried band noise would look, with a decoder
        // that has nothing to extrapolate - which is the fault being watched for.
        s.has_history = true;
        s.floor_rms = 500.0;

        let out = s.conceal().expect("a stream with history conceals");

        assert!(
            s.said_silent,
            "the codec produced silence and nothing noticed - the check is              measuring the finished frame again"
        );
        assert!(
            peak(&out) > 0,
            "and the comfort noise should still have filled the gap"
        );
    }

    /// The invariant has to be able to fire, and for a while it could not: it
    /// ran on the finished frame, and where a noise floor exists the comfort
    /// noise has already filled that frame. The repair had removed the symptom
    /// and the detector with it - which is the shape of the fault this whole
    /// module exists to prevent, one layer up (found in review 2026-08-18).
    ///
    /// So the check is tested on its own terms: silence from the codec trips
    /// it, anything else does not, and it speaks once.
    #[test]
    fn the_silence_check_fires_on_what_the_codec_produced() {
        let mut s = StreamDecoder::new().unwrap();
        s.has_history = true;
        s.floor_rms = 500.0; // as a stream carrying band noise would have

        s.note_if_codec_went_silent(0.0);
        assert!(s.said_silent, "silence from the codec must be noticed");

        let mut again = StreamDecoder::new().unwrap();
        again.has_history = true;
        again.floor_rms = 500.0;
        again.note_if_codec_went_silent(12.0);
        assert!(!again.said_silent, "a codec that produced something is not a fault");
    }

    /// Said once, not fifty times a second for as long as the gap lasts.
    #[test]
    fn the_silence_check_speaks_once_per_stream() {
        let mut s = StreamDecoder::new().unwrap();
        s.has_history = true;
        s.floor_rms = 500.0;
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
