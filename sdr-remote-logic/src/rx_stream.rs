// SPDX-License-Identifier: GPL-2.0-or-later

//! A receive stream, from Opus packet to device samples.
//!
//! `StreamDecoder` in the core crate already makes it impossible to decode with
//! the wrong decoder. This adds the other half of the same fault: the two
//! resamplers. A wideband frame through the narrowband resampler comes out an
//! octave low and twice as long, so repairing only the decoder would have
//! replaced silence with something worse (2026-08-16).
//!
//! Both belong to the stream, and neither is chosen by the caller. Whatever
//! comes back out of `decode`, `recover` or `conceal` has already been through
//! the resampler that matches it, and carries the sample rate it was decoded at
//! so the recorder writes the right header.

use anyhow::{Context, Result};
use log::warn;
use sdr_remote_core::resample::{resampler, Resampler, UpFilter};
use sdr_remote_core::stream::{Filled, StreamDecoder};
use sdr_remote_core::{
    FRAME_SAMPLES, FRAME_SAMPLES_WIDEBAND, NETWORK_SAMPLE_RATE, NETWORK_SAMPLE_RATE_WIDEBAND,
};

/// One frame, in both the shapes the engine needs it: as decoded for the
/// recorder, and at device rate for the mixer.
pub struct Decoded {
    /// Decoded PCM at the network rate - what a recording should hold.
    pub pcm: Vec<i16>,
    /// The rate `pcm` is at, so the recorder cannot guess wrong.
    pub rate: u32,
    /// The same audio at the playback device's rate.
    pub dev: Vec<f32>,
    /// True when this frame was filled in rather than received. The mixer treats
    /// it identically; the level meters and the logs do not have to.
    pub concealed: bool,
}

/// Both decoders and both resamplers for one receive stream.
pub struct RxStream {
    dec: StreamDecoder,
    res_nb: Resampler,
    res_wb: Resampler,
    label: &'static str,
}

fn make_resamplers(
    playback_rate: u32,
    label: &str,
) -> Result<(Resampler, Resampler)> {
    // The short filter when going up (2.0 ms of delay 8k->48k, measured with
    // `output_delay()`, 2026-10-04): latency outranks everything else here. On a
    // headset slower than the stream it goes down and gets the anti-alias
    // filter instead (build 260).
    let nb = resampler(NETWORK_SAMPLE_RATE, playback_rate, FRAME_SAMPLES, UpFilter::Short)
        .with_context(|| format!("{label} 8k->device resampler"))?;
    let wb = resampler(NETWORK_SAMPLE_RATE_WIDEBAND, playback_rate, FRAME_SAMPLES_WIDEBAND, UpFilter::Short)
        .with_context(|| format!("{label} 16k->device resampler"))?;
    Ok((nb, wb))
}

impl RxStream {
    pub fn new(playback_rate: u32, label: &'static str) -> Result<Self> {
        let (res_nb, res_wb) = make_resamplers(playback_rate, label)?;
        Ok(Self { dec: StreamDecoder::new()?, res_nb, res_wb, label })
    }

    /// Its two resamplers (narrowband, wideband), for a log line.
    pub fn resamplers(&self) -> (&Resampler, &Resampler) {
        (&self.res_nb, &self.res_wb)
    }

    /// The playback device changed rate. Resamplers are rebuilt; the decoders
    /// keep their history, because the stream on the wire did not change.
    pub fn set_playback_rate(&mut self, playback_rate: u32) {
        match make_resamplers(playback_rate, self.label) {
            Ok((nb, wb)) => {
                self.res_nb = nb;
                self.res_wb = wb;
            }
            Err(e) => warn!("{}: could not rebuild resamplers: {}", self.label, e),
        }
    }

    /// New session: forget everything the old one taught the decoders.
    pub fn reset(&mut self) {
        if let Err(e) = self.dec.reset() {
            warn!("{}: could not reset decoder: {}", self.label, e);
        }
    }

    /// Format of the last real frame.
    pub fn wideband(&self) -> bool {
        self.dec.wideband()
    }

    /// True once this stream has carried real audio.
    pub fn has_history(&self) -> bool {
        self.dec.has_history()
    }

    /// A frame that arrived, with the wideband flag from its own packet header.
    pub fn decode(&mut self, opus: &[u8], wideband: bool) -> Option<Decoded> {
        let pcm = self.dec.decode(opus, wideband)?;
        Some(self.finish(pcm, wideband, false))
    }

    /// A lost frame rebuilt from the redundancy carried in the next packet.
    /// `wideband` is that packet's flag - the redundancy is in its format, not
    /// in whatever the stream last saw.
    pub fn recover(&mut self, next_opus: &[u8], wideband: bool) -> Option<Decoded> {
        let pcm = self.dec.recover(next_opus, wideband)?;
        Some(self.finish(pcm, wideband, false))
    }

    /// One lost frame, rebuilt or concealed - the decision is the core
    /// decoder's (`StreamDecoder::recover_or_conceal`, build 155); this only
    /// puts what came out through the resampler that matches its format.
    pub fn recover_or_conceal(&mut self, next: Option<(&[u8], bool)>) -> Option<Decoded> {
        let filled = self.dec.recover_or_conceal(next)?;
        Some(self.filled(filled))
    }

    /// Fill a gap, in the format this stream is actually carrying - which the
    /// core decoder decides, for this and for the arm above (build 156).
    pub fn conceal(&mut self) -> Option<Decoded> {
        let filled = self.dec.conceal_filled()?;
        Some(self.filled(filled))
    }

    fn filled(&mut self, filled: Filled) -> Decoded {
        match filled {
            Filled::Rebuilt(pcm, wideband) => self.finish(pcm, wideband, false),
            Filled::Concealed(pcm, wideband) => self.finish(pcm, wideband, true),
        }
    }

    fn finish(&mut self, pcm: Vec<i16>, wideband: bool, concealed: bool) -> Decoded {
        let dev = if wideband {
            resample(&mut self.res_wb, &pcm, self.label)
        } else {
            resample(&mut self.res_nb, &pcm, self.label)
        };
        let rate = if wideband { NETWORK_SAMPLE_RATE_WIDEBAND } else { NETWORK_SAMPLE_RATE };
        Decoded { pcm, rate, dev, concealed }
    }
}

fn resample(
    resampler: &mut Resampler,
    pcm: &[i16],
    label: &str,
) -> Vec<f32> {
    let input: Vec<f32> = pcm.iter().map(|&s| s as f32 / 32768.0).collect();
    match resampler.process(input) {
        Ok(result) => result,
        Err(e) => {
            warn!("{}: resample network->device error: {}", label, e);
            Vec::new()
        }
    }
}

/// Pull one channel's Opus payload out of a multi-channel blob.
///
/// Layout: one byte channel count, then per channel one byte id, two bytes
/// big-endian length, then the payload. Returns nothing when the channel is not
/// in this packet - which is normal: RX2 and the binaural right only travel when
/// the operator has them on.
pub fn channel_opus(blob: &[u8], want: u8) -> Option<&[u8]> {
    if blob.is_empty() {
        return None;
    }
    let count = blob[0] as usize;
    let mut pos = 1usize;
    for _ in 0..count {
        if pos + 3 > blob.len() {
            return None;
        }
        let id = blob[pos];
        let len = u16::from_be_bytes([blob[pos + 1], blob[pos + 2]]) as usize;
        if pos + 3 + len > blob.len() {
            return None;
        }
        if id == want {
            return Some(&blob[pos + 3..pos + 3 + len]);
        }
        pos += 3 + len;
    }
    None
}

/// One lost frame for one channel: rebuild it from the redundancy in the next
/// packet if that packet carries this channel, and otherwise fill the gap.
///
/// `next` is the following packet's payload and its own wideband flag. That flag
/// is the right one for the rebuild - the redundancy is a copy of the lost audio
/// carried inside that packet, in that packet's format.
pub fn recover_or_conceal(
    stream: &mut RxStream,
    next: Option<(&[u8], bool)>,
    channel: u8,
) -> Option<Decoded> {
    recover_or_conceal_one(stream, next.and_then(|(blob, wideband)| channel_opus(blob, channel).map(|o| (o, wideband))))
}

/// One lost frame of any receive stream: rebuild it from the redundancy in the
/// next packet when there is one, and otherwise fill the gap. `next` is that
/// packet's own Opus data and wideband flag.
///
/// The one place that decides this for all five streams - the Thetis bundle
/// through `recover_or_conceal`, the radios and VRX directly (build 154). Until
/// then only Thetis rebuilt; the radios and VRX always concealed, while the
/// server spent bits on redundancy for them that nobody used.
pub fn recover_or_conceal_one(stream: &mut RxStream, next: Option<(&[u8], bool)>) -> Option<Decoded> {
    stream.recover_or_conceal(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdr_remote_core::codec::{OpusEncoder, OpusEncoderWideband};

    fn noise(n: usize, seed: &mut u32) -> Vec<i16> {
        (0..n)
            .map(|_| {
                *seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                ((*seed >> 16) as i16) / 6
            })
            .collect()
    }

    /// The trap this type exists to close: a concealed frame has to come out at
    /// the same length per unit of time as a real one. Repairing the decoder
    /// alone would have sent a 16 kHz frame through the 8 kHz resampler - twice
    /// as long and an octave down, which is worse than the silence it replaced.
    #[test]
    fn a_concealed_frame_is_the_same_length_as_a_real_one() {
        for wide in [false, true] {
            let mut s = RxStream::new(48_000, "test").unwrap();
            let mut enc_nb = OpusEncoder::new_rx_continuous().unwrap();
            let mut enc_wb = OpusEncoderWideband::new_rx_continuous().unwrap();
            let mut seed = 42u32;

            let mut real_len = 0;
            for _ in 0..25 {
                let d = if wide {
                    let pcm = noise(FRAME_SAMPLES_WIDEBAND, &mut seed);
                    s.decode(&enc_wb.encode(&pcm).unwrap(), true).unwrap()
                } else {
                    let pcm = noise(FRAME_SAMPLES, &mut seed);
                    s.decode(&enc_nb.encode(&pcm).unwrap(), false).unwrap()
                };
                real_len = d.dev.len();
            }

            let hidden = s.conceal().expect("stream with history conceals");
            assert_eq!(
                hidden.dev.len(), real_len,
                "wide={wide}: concealed frame is a different length at the device"
            );
            assert!(hidden.concealed);
            assert_eq!(
                hidden.rate,
                if wide { NETWORK_SAMPLE_RATE_WIDEBAND } else { NETWORK_SAMPLE_RATE },
                "wide={wide}: recorder would write the wrong header"
            );
            assert!(
                hidden.dev.iter().any(|s| s.abs() > 0.0),
                "wide={wide}: concealed frame is silent"
            );
        }
    }

    /// A lost frame with a protected packet behind it comes back rebuilt, not
    /// concealed, on narrowband (20 kbps while protecting) and on wideband; with
    /// no packet behind it, it is concealed (build 154).
    #[test]
    fn a_lost_frame_is_rebuilt_from_the_next_packet() {
        for wide in [false, true] {
            let mut enc_nb = OpusEncoder::new_rx_continuous().unwrap();
            let mut enc_wb = OpusEncoderWideband::new_rx_continuous().unwrap();
            enc_nb.set_loss_protection(true, 10).unwrap();
            enc_wb.set_loss_protection(true, 10).unwrap();
            let mut seed = 7u32;
            let pkts: Vec<Vec<u8>> = (0..40)
                .map(|_| {
                    if wide {
                        enc_wb.encode(&noise(FRAME_SAMPLES_WIDEBAND, &mut seed)).unwrap()
                    } else {
                        enc_nb.encode(&noise(FRAME_SAMPLES, &mut seed)).unwrap()
                    }
                })
                .collect();
            let (mut a, mut b) = (RxStream::new(48_000, "test").unwrap(), RxStream::new(48_000, "test").unwrap());
            for p in &pkts[..30] {
                a.decode(p, wide).unwrap();
                b.decode(p, wide).unwrap();
            }
            // packet 30 is lost; 31 is there
            let rebuilt = recover_or_conceal_one(&mut a, Some((&pkts[31], wide))).unwrap();
            let concealed = recover_or_conceal_one(&mut b, None).unwrap();
            assert!(!rebuilt.concealed, "wide={wide}: rebuilt counts as concealed");
            assert!(concealed.concealed, "wide={wide}");
            // The format of a concealed frame picks the resampler: a wideband
            // gap through the narrowband one is an octave down (build 156,
            // review of 155: no test held it).
            assert_eq!(
                concealed.rate,
                if wide { NETWORK_SAMPLE_RATE_WIDEBAND } else { NETWORK_SAMPLE_RATE },
                "wide={wide}: a concealed frame came out in the wrong format"
            );
            assert_ne!(rebuilt.pcm, concealed.pcm, "wide={wide}: the redundancy brought nothing");
            // The bundle path for Thetis goes through the same decision.
            let mut c = RxStream::new(48_000, "test").unwrap();
            for p in &pkts[..30] {
                c.decode(p, wide).unwrap();
            }
            let mut blob = vec![1u8, 0];
            blob.extend_from_slice(&(pkts[31].len() as u16).to_be_bytes());
            blob.extend_from_slice(&pkts[31]);
            let via_bundle = recover_or_conceal(&mut c, Some((&blob, wide)), 0).unwrap();
            assert_eq!(via_bundle.pcm, rebuilt.pcm, "wide={wide}: the bundle decides differently");
        }
    }

    /// In a bundle of two channels each channel is rebuilt from its own
    /// redundancy, not from channel 0's (build 155: with a fixed channel 0 the
    /// binaural right would carry the left on every rebuilt frame, and the
    /// first test, with one channel, stayed green - review of 154).
    #[test]
    fn each_channel_of_a_bundle_is_rebuilt_from_its_own_redundancy() {
        let mut enc0 = OpusEncoderWideband::new_rx_continuous().unwrap();
        let mut enc1 = OpusEncoderWideband::new_rx_continuous().unwrap();
        enc0.set_loss_protection(true, 10).unwrap();
        enc1.set_loss_protection(true, 10).unwrap();
        let (mut s0, mut s1) = (7u32, 99u32);
        let p0: Vec<Vec<u8>> = (0..40).map(|_| enc0.encode(&noise(FRAME_SAMPLES_WIDEBAND, &mut s0)).unwrap()).collect();
        let p1: Vec<Vec<u8>> = (0..40).map(|_| enc1.encode(&noise(FRAME_SAMPLES_WIDEBAND, &mut s1)).unwrap()).collect();
        let mut blob = vec![2u8, 0];
        blob.extend_from_slice(&(p0[31].len() as u16).to_be_bytes());
        blob.extend_from_slice(&p0[31]);
        blob.push(1);
        blob.extend_from_slice(&(p1[31].len() as u16).to_be_bytes());
        blob.extend_from_slice(&p1[31]);
        let (mut right, mut alone) = (RxStream::new(48_000, "test").unwrap(), RxStream::new(48_000, "test").unwrap());
        for p in &p1[..30] {
            right.decode(p, true).unwrap();
            alone.decode(p, true).unwrap();
        }
        let from_bundle = recover_or_conceal(&mut right, Some((&blob, true)), 1).unwrap();
        let from_own = recover_or_conceal_one(&mut alone, Some((&p1[31], true))).unwrap();
        assert!(!from_bundle.concealed);
        assert_eq!(from_bundle.pcm, from_own.pcm, "channel 1 was rebuilt from another channel");
    }

    /// The common case on a clean link: protection is off, the next packet is
    /// there but carries no redundancy - the frame is concealed (build 155;
    /// review of 154: no test had this state).
    #[test]
    fn a_next_packet_without_redundancy_conceals() {
        let mut enc = OpusEncoder::new_rx_continuous().unwrap();
        let mut seed = 7u32;
        let pkts: Vec<Vec<u8>> = (0..40).map(|_| enc.encode(&noise(FRAME_SAMPLES, &mut seed)).unwrap()).collect();
        let (mut a, mut b) = (RxStream::new(48_000, "test").unwrap(), RxStream::new(48_000, "test").unwrap());
        for p in &pkts[..30] {
            a.decode(p, false).unwrap();
            b.decode(p, false).unwrap();
        }
        let with_next = recover_or_conceal_one(&mut a, Some((&pkts[31], false))).unwrap();
        let without = recover_or_conceal_one(&mut b, None).unwrap();
        assert!(with_next.concealed, "no redundancy to rebuild from");
        assert_eq!(with_next.pcm, without.pcm);
    }

    /// Every stream's missing-frame arm in the engine goes through the one
    /// decision, and none conceals directly any more (build 154). A text guard:
    /// it pins that the calls are there, not that they run - the test above
    /// is what shows the decision itself works.
    #[test]
    fn every_missing_frame_arm_asks_for_a_rebuild_first() {
        let engine = include_str!("engine.rs");
        let arms: Vec<&str> = engine.lines().filter(|l| l.trim_start().starts_with("JitterResult::Missing =>")).collect();
        // Five receive streams in four blocks: the two radios share one (build 180).
        assert_eq!(arms.len(), 4, "five receive streams, four blocks");
        assert!(arms.iter().all(|l| !l.contains(".conceal()")), "a stream conceals a lost frame without asking for a rebuild");
        // Each of the four through the one macro, with its own buffer - the
        // pair is written once per stream and checked here (build 155; review
        // of 154: radio 2 looking in radio 1's buffer stayed green).
        // The radios' block takes its pair from the call, and the calls are
        // pinned whole in `each_radio_measures_its_own_after_tx` (build 180).
        assert_eq!(engine.matches("JitterResult::Missing => rebuild_or_conceal!($st, $jb),").count(), 1);
        for x in ["vrx1", "vrx2"] {
            let call = format!("rebuild_or_conceal!(st_{x}, {x}_jitter_buf)");
            assert_eq!(engine.matches(call.as_str()).count(), 1, "{x} is not tied to its own buffer");
        }
        // The packet after the gap, not the lost one, is looked up in one place,
        // and both the macro and the Thetis arm take it from there (build 156;
        // review of 155: the Thetis arm had its own copy, which no test read).
        assert_eq!(engine.matches("peek_frame(seq)").count(), 1, "a second way to find the packet after a gap");
        let after = &engine[engine.find("macro_rules! next_after_gap").unwrap()..];
        let after = &after[..after.find("};").unwrap()];
        assert!(after.contains("$jb.next_seq_peek().and_then(|seq| $jb.peek_frame(seq))"));
        let mac = &engine[engine.find("macro_rules! rebuild_or_conceal").unwrap()..];
        let mac = &mac[..mac.find("}};").unwrap()];
        assert!(mac.contains("let next = next_after_gap!($jb);"));
        assert!(mac.contains("recover_or_conceal_one(&mut $st, next)"));
        // The Thetis arm: its own buffer, and each channel of the bundle into
        // its own stream.
        assert_eq!(engine.matches("let next = next_after_gap!(jitter_buf);").count(), 1);
        for (st, ch) in [("st_rx1", 0), ("st_bin_r", 1), ("st_rx2", 2)] {
            let call = format!("recover_or_conceal(&mut {st}, next, {ch});");
            assert_eq!(engine.matches(call.as_str()).count(), 1, "{st} is not channel {ch}");
        }
        // And the one list that empties all five buffers names all five
        // (build 155; the list for a change of sound device had four).
        let reset = &engine[engine.find("macro_rules! reset_jitter_buffers").unwrap()..];
        let reset = &reset[..reset.find("}};").unwrap()];
        for jb in ["jitter_buf", "yaesu_jitter_buf", "yaesu2_jitter_buf", "vrx1_jitter_buf", "vrx2_jitter_buf"] {
            assert!(reset.lines().any(|l| l.trim() == format!("{jb}.reset();")), "{jb} missing from the reset list");
        }
        // ...and the four places that empty them all still call it (build 156;
        // review of 155: removing a call stayed green).
        assert_eq!(engine.matches("reset_jitter_buffers!();").count(), 4, "a place that empties every buffer no longer does");
    }

    /// A stream nobody sent anything on stays quiet rather than inventing audio.
    #[test]
    fn an_unused_stream_conceals_nothing() {
        let mut s = RxStream::new(48_000, "test").unwrap();
        assert!(s.conceal().is_none());
        assert!(!s.has_history());
    }

    /// The blob reader, because every stream depends on it and an off-by-one
    /// here would silently drop a channel rather than fail.
    #[test]
    fn channels_come_out_of_the_blob_by_id() {
        // two channels: id 0 with 3 bytes, id 2 with 2 bytes
        let blob = vec![2u8, 0, 0, 3, 0xAA, 0xBB, 0xCC, 2, 0, 2, 0x11, 0x22];
        assert_eq!(channel_opus(&blob, 0), Some(&[0xAAu8, 0xBB, 0xCC][..]));
        assert_eq!(channel_opus(&blob, 2), Some(&[0x11u8, 0x22][..]));
        assert_eq!(channel_opus(&blob, 1), None, "absent channel is not an error");
        assert_eq!(channel_opus(&[], 0), None);
        // Truncated: refuse rather than read past the end.
        assert_eq!(channel_opus(&[2u8, 0, 0, 9, 0xAA], 0), None);
    }
}
