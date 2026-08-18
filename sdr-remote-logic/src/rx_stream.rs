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
use sdr_remote_core::stream::StreamDecoder;
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
    res_nb: rubato::SincFixedIn<f32>,
    res_wb: rubato::SincFixedIn<f32>,
    label: &'static str,
}

fn sinc_params() -> rubato::SincInterpolationParameters {
    // Low-latency sinc: a short filter is about 20 ms of group delay, and
    // latency outranks everything else here.
    rubato::SincInterpolationParameters {
        sinc_len: 32,
        f_cutoff: 0.90,
        oversampling_factor: 32,
        interpolation: rubato::SincInterpolationType::Cubic,
        window: rubato::WindowFunction::Blackman,
    }
}

fn make_resamplers(
    playback_rate: u32,
    label: &str,
) -> Result<(rubato::SincFixedIn<f32>, rubato::SincFixedIn<f32>)> {
    let nb = rubato::SincFixedIn::<f32>::new(
        playback_rate as f64 / NETWORK_SAMPLE_RATE as f64,
        1.0,
        sinc_params(),
        FRAME_SAMPLES,
        1,
    )
    .with_context(|| format!("{label} 8k->device resampler"))?;
    let wb = rubato::SincFixedIn::<f32>::new(
        playback_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64,
        1.0,
        sinc_params(),
        FRAME_SAMPLES_WIDEBAND,
        1,
    )
    .with_context(|| format!("{label} 16k->device resampler"))?;
    Ok((nb, wb))
}

impl RxStream {
    pub fn new(playback_rate: u32, label: &'static str) -> Result<Self> {
        let (res_nb, res_wb) = make_resamplers(playback_rate, label)?;
        Ok(Self { dec: StreamDecoder::new()?, res_nb, res_wb, label })
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

    /// Fill a gap, in the format this stream is actually carrying.
    pub fn conceal(&mut self) -> Option<Decoded> {
        let wideband = self.dec.wideband();
        let pcm = self.dec.conceal()?;
        Some(self.finish(pcm, wideband, true))
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
    resampler: &mut impl rubato::Resampler<f32>,
    pcm: &[i16],
    label: &str,
) -> Vec<f32> {
    let input: Vec<f32> = pcm.iter().map(|&s| s as f32 / 32768.0).collect();
    match resampler.process(&[input], None) {
        Ok(result) => result.into_iter().next().unwrap_or_default(),
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
    if let Some((blob, wideband)) = next {
        if let Some(opus) = channel_opus(blob, channel) {
            if let Some(d) = stream.recover(opus, wideband) {
                return Some(d);
            }
        }
    }
    stream.conceal()
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
