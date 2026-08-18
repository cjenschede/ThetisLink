// SPDX-License-Identifier: GPL-2.0-or-later

//! Thin Opus encoder wrapper for VRX audio. Defaults match the
//! ThetisLink RX1 encoder (Voip + Voice + 12.8 kbps + Narrowband +
//! FEC off) but with DTX off so continuous channelizer output is
//! preserved (no silence-frame holes during quiet passages).
//!
//! Duplicated from `sdr-remote-core::codec::OpusEncoder` so this
//! crate stays standalone — no internal dependency on a ThetisLink
//! crate. The duplication is ~50 lines of config-only code (no DSP).

use anyhow::{Context, Result};
use audiopus::coder::Encoder;
use audiopus::{Application, Bandwidth, Bitrate, Channels, SampleRate, Signal};

/// Frame size in samples at 8 kHz NB, 20 ms.
pub const FRAME_SAMPLES_NB: usize = 160;
/// Frame size in samples at 16 kHz WB, 20 ms.
pub const FRAME_SAMPLES_WB: usize = 320;
/// Back-compat alias (= NB).
pub const FRAME_SAMPLES: usize = FRAME_SAMPLES_NB;
/// Maximum Opus frame size in bytes (safe upper bound).
const MAX_ENCODED_SIZE: usize = 1275;

pub struct VrxOpusEncoder {
    encoder: Encoder,
    encode_buf: Vec<u8>,
    frame_samples: usize,
}

impl VrxOpusEncoder {
    /// Narrowband encoder (8 kHz, 160 sample frames).
    pub fn new() -> Result<Self> {
        Self::new_with_rate(8_000)
    }

    /// Construct Opus encoder for VRX at the given output sample rate.
    /// 8000 Hz → Narrowband, 16000 Hz → Wideband (Voice/Voip, 12.8 kbps
    /// for NB, 20 kbps for WB; DTX off so silent channelizer output
    /// still keeps the stream alive).
    pub fn new_with_rate(output_rate_hz: u32) -> Result<Self> {
        let (sample_rate, bandwidth, bitrate, frame_samples) = match output_rate_hz {
            // 24 kbps, the same as every other wideband receive stream in
            // ThetisLink. It sat at 20 for no reason anybody wrote down, and a
            // receive path that is a little different from its neighbours for
            // no stated reason is how a fault hides for a fortnight.
            16_000 => (SampleRate::Hz16000, Bandwidth::Wideband, 24_000, FRAME_SAMPLES_WB),
            _ => (SampleRate::Hz8000, Bandwidth::Narrowband, 12_800, FRAME_SAMPLES_NB),
        };
        let mut encoder =
            Encoder::new(sample_rate, Channels::Mono, Application::Voip)
                .context("create Opus encoder")?;
        encoder.set_bitrate(Bitrate::BitsPerSecond(bitrate)).context("set bitrate")?;
        encoder.set_bandwidth(bandwidth).context("set bandwidth")?;
        encoder.set_signal(Signal::Voice).context("set signal type")?;
        encoder.set_inband_fec(false).context("set FEC")?;
        encoder.set_dtx(false).context("set DTX")?;
        encoder.set_packet_loss_perc(0).context("set packet loss")?;
        Ok(Self {
            encoder,
            encode_buf: vec![0u8; MAX_ENCODED_SIZE],
            frame_samples,
        })
    }

    /// Turn packet-loss protection on or off on a running encoder.
    ///
    /// Opus takes the redundancy out of the same bits it pays for sound with,
    /// so this is worth having only on a link that is actually losing packets.
    /// The server decides that from what its clients report and says so here;
    /// nothing is reallocated and no audio is interrupted.
    pub fn set_loss_protection(&mut self, on: bool, loss_pct: u8) -> Result<()> {
        self.encoder.set_inband_fec(on).context("set FEC")?;
        self.encoder
            .set_packet_loss_perc(if on { loss_pct.min(100) } else { 0 })
            .context("set packet loss")
    }

    pub fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    /// Encode one 20 ms frame (= `self.frame_samples()` i16 samples).
    pub fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>> {
        assert_eq!(pcm.len(), self.frame_samples, "expected {} samples", self.frame_samples);
        let len = self
            .encoder
            .encode(pcm, &mut self.encode_buf)
            .context("opus encode")?;
        Ok(self.encode_buf[..len].to_vec())
    }
}
