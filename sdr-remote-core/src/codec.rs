use std::convert::TryFrom;

use anyhow::{Context, Result};
use audiopus::coder::{Decoder, Encoder};
use audiopus::packet::Packet as OpusPacket;
use audiopus::{Application, Bandwidth, Bitrate, Channels, MutSignals, SampleRate, Signal};

use crate::{FRAME_SAMPLES, FRAME_SAMPLES_WIDEBAND};

/// Maximum encoded frame size in bytes
const MAX_ENCODED_SIZE: usize = 256;

/// Opus encoder configured for 8kHz mono VOIP with FEC
pub struct OpusEncoder {
    encoder: Encoder,
    encode_buf: Vec<u8>,
}

impl OpusEncoder {
    pub fn new() -> Result<Self> {
        let mut encoder = Encoder::new(SampleRate::Hz8000, Channels::Mono, Application::Voip)
            .context("failed to create Opus encoder")?;

        // 12.8 kbps — just above the 12.4k FEC threshold
        encoder
            .set_bitrate(Bitrate::BitsPerSecond(12_800))
            .context("set bitrate")?;
        encoder
            .set_bandwidth(Bandwidth::Narrowband)
            .context("set bandwidth")?;
        encoder
            .set_signal(Signal::Voice)
            .context("set signal type")?;
        encoder
            .set_inband_fec(true)
            .context("enable FEC")?;
        encoder
            .set_dtx(true)
            .context("enable DTX")?;
        // Expect 10% loss to make FEC useful
        encoder
            .set_packet_loss_perc(10)
            .context("set packet loss")?;

        Ok(Self {
            encoder,
            encode_buf: vec![0u8; MAX_ENCODED_SIZE],
        })
    }

    /// Encode a 20ms frame of 160 i16 samples at 8kHz mono.
    /// Returns the encoded Opus bytes.
    pub fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>> {
        assert_eq!(pcm.len(), FRAME_SAMPLES, "expected {} samples", FRAME_SAMPLES);

        let len = self
            .encoder
            .encode(pcm, &mut self.encode_buf)
            .context("opus encode")?;
        Ok(self.encode_buf[..len].to_vec())
    }
}

/// Opus decoder configured for 8kHz mono with FEC support
pub struct OpusDecoder {
    decoder: Decoder,
    decode_buf: Vec<i16>,
}

impl OpusDecoder {
    pub fn new() -> Result<Self> {
        let decoder = Decoder::new(SampleRate::Hz8000, Channels::Mono)
            .context("failed to create Opus decoder")?;

        Ok(Self {
            decoder,
            decode_buf: vec![0i16; FRAME_SAMPLES],
        })
    }

    /// Decode an Opus frame, returning 160 i16 samples at 8kHz mono.
    pub fn decode(&mut self, opus_data: &[u8]) -> Result<Vec<i16>> {
        let packet = OpusPacket::try_from(opus_data)
            .context("invalid opus packet")?;
        let output = MutSignals::try_from(&mut self.decode_buf)
            .context("invalid output buffer")?;
        let samples = self
            .decoder
            .decode(Some(packet), output, false)
            .context("opus decode")?;
        Ok(self.decode_buf[..samples].to_vec())
    }

    /// Decode with FEC using a previous packet's data.
    /// Call this when a packet is lost: pass the *next* packet's opus data
    /// to recover the lost frame via in-band FEC.
    pub fn decode_fec(&mut self, next_opus_data: &[u8]) -> Result<Vec<i16>> {
        let packet = OpusPacket::try_from(next_opus_data)
            .context("invalid opus packet")?;
        let output = MutSignals::try_from(&mut self.decode_buf)
            .context("invalid output buffer")?;
        let samples = self
            .decoder
            .decode(Some(packet), output, true)
            .context("opus decode FEC")?;
        Ok(self.decode_buf[..samples].to_vec())
    }

    /// Packet Loss Concealment: generate comfort noise / interpolation
    /// when no packet data is available at all.
    pub fn decode_plc(&mut self) -> Result<Vec<i16>> {
        let output = MutSignals::try_from(&mut self.decode_buf)
            .context("invalid output buffer")?;
        let samples = self
            .decoder
            .decode(None, output, false)
            .context("opus PLC")?;
        Ok(self.decode_buf[..samples].to_vec())
    }
}


/// Opus encoder configured for 16kHz mono VOIP with FEC (wideband)
pub struct OpusEncoderWideband {
    encoder: Encoder,
    encode_buf: Vec<u8>,
}

impl OpusEncoderWideband {
    pub fn new() -> Result<Self> {
        let mut encoder = Encoder::new(SampleRate::Hz16000, Channels::Mono, Application::Voip)
            .context("failed to create wideband Opus encoder")?;

        // 24 kbps — good quality for wideband voice
        encoder
            .set_bitrate(Bitrate::BitsPerSecond(24_000))
            .context("set bitrate")?;
        encoder
            .set_bandwidth(Bandwidth::Wideband)
            .context("set bandwidth")?;
        encoder
            .set_signal(Signal::Voice)
            .context("set signal type")?;
        encoder
            .set_inband_fec(true)
            .context("enable FEC")?;
        encoder
            .set_dtx(true)
            .context("enable DTX")?;
        encoder
            .set_packet_loss_perc(10)
            .context("set packet loss")?;

        Ok(Self {
            encoder,
            encode_buf: vec![0u8; MAX_ENCODED_SIZE],
        })
    }

    /// Encode a 20ms frame of 320 i16 samples at 16kHz mono.
    pub fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>> {
        assert_eq!(
            pcm.len(),
            FRAME_SAMPLES_WIDEBAND,
            "expected {} samples",
            FRAME_SAMPLES_WIDEBAND
        );

        let len = self
            .encoder
            .encode(pcm, &mut self.encode_buf)
            .context("opus wideband encode")?;
        Ok(self.encode_buf[..len].to_vec())
    }
}

/// Opus decoder configured for 16kHz mono with FEC support (wideband)
pub struct OpusDecoderWideband {
    decoder: Decoder,
    decode_buf: Vec<i16>,
}

impl OpusDecoderWideband {
    pub fn new() -> Result<Self> {
        let decoder = Decoder::new(SampleRate::Hz16000, Channels::Mono)
            .context("failed to create wideband Opus decoder")?;

        Ok(Self {
            decoder,
            decode_buf: vec![0i16; FRAME_SAMPLES_WIDEBAND],
        })
    }

    /// Decode an Opus frame, returning 320 i16 samples at 16kHz mono.
    pub fn decode(&mut self, opus_data: &[u8]) -> Result<Vec<i16>> {
        let packet = OpusPacket::try_from(opus_data).context("invalid opus packet")?;
        let output =
            MutSignals::try_from(&mut self.decode_buf).context("invalid output buffer")?;
        let samples = self
            .decoder
            .decode(Some(packet), output, false)
            .context("opus wideband decode")?;
        Ok(self.decode_buf[..samples].to_vec())
    }

    /// Decode with FEC.
    pub fn decode_fec(&mut self, next_opus_data: &[u8]) -> Result<Vec<i16>> {
        let packet = OpusPacket::try_from(next_opus_data).context("invalid opus packet")?;
        let output =
            MutSignals::try_from(&mut self.decode_buf).context("invalid output buffer")?;
        let samples = self
            .decoder
            .decode(Some(packet), output, true)
            .context("opus wideband decode FEC")?;
        Ok(self.decode_buf[..samples].to_vec())
    }

    /// Packet Loss Concealment.
    pub fn decode_plc(&mut self) -> Result<Vec<i16>> {
        let output =
            MutSignals::try_from(&mut self.decode_buf).context("invalid output buffer")?;
        let samples = self
            .decoder
            .decode(None, output, false)
            .context("opus wideband PLC")?;
        Ok(self.decode_buf[..samples].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let mut encoder = OpusEncoder::new().unwrap();
        let mut decoder = OpusDecoder::new().unwrap();

        // Opus has a decoder delay, so feed multiple frames and check the later ones
        for frame_idx in 0..5 {
            let pcm: Vec<i16> = (0..FRAME_SAMPLES)
                .map(|i| {
                    let t = (frame_idx * FRAME_SAMPLES + i) as f32 / 8000.0;
                    (f32::sin(2.0 * std::f32::consts::PI * 400.0 * t) * 16000.0) as i16
                })
                .collect();

            let encoded = encoder.encode(&pcm).unwrap();
            assert!(!encoded.is_empty());
            assert!(encoded.len() < MAX_ENCODED_SIZE);

            let decoded = decoder.decode(&encoded).unwrap();
            assert_eq!(decoded.len(), FRAME_SAMPLES);

            // After the codec warms up, check that output has energy
            if frame_idx >= 2 {
                let energy: f64 = decoded.iter().map(|&s| (s as f64).powi(2)).sum();
                assert!(energy > 0.0, "decoded frame {} should have energy", frame_idx);
            }
        }
    }

    #[test]
    fn encode_silence() {
        let mut encoder = OpusEncoder::new().unwrap();
        let mut decoder = OpusDecoder::new().unwrap();

        let silence = vec![0i16; FRAME_SAMPLES];
        let encoded = encoder.encode(&silence).unwrap();
        // DTX enabled: silence frames should be very small
        assert!(encoded.len() < 10, "DTX silence frame should be tiny, got {} bytes", encoded.len());

        let decoded = decoder.decode(&encoded).unwrap();
        assert_eq!(decoded.len(), FRAME_SAMPLES);
    }

    #[test]
    fn plc_produces_output() {
        let mut encoder = OpusEncoder::new().unwrap();
        let mut decoder = OpusDecoder::new().unwrap();

        // Feed one real frame first
        let pcm: Vec<i16> = (0..FRAME_SAMPLES)
            .map(|i| {
                let t = i as f32 / 8000.0;
                (f32::sin(2.0 * std::f32::consts::PI * 400.0 * t) * 16000.0) as i16
            })
            .collect();
        let encoded = encoder.encode(&pcm).unwrap();
        let _ = decoder.decode(&encoded).unwrap();

        // Now simulate packet loss
        let plc_output = decoder.decode_plc().unwrap();
        assert_eq!(plc_output.len(), FRAME_SAMPLES);
    }

    #[test]
    fn multiple_frames() {
        let mut encoder = OpusEncoder::new().unwrap();
        let mut decoder = OpusDecoder::new().unwrap();

        for frame_num in 0..10 {
            let pcm: Vec<i16> = (0..FRAME_SAMPLES)
                .map(|i| {
                    let t = (frame_num * FRAME_SAMPLES + i) as f32 / 8000.0;
                    (f32::sin(2.0 * std::f32::consts::PI * 800.0 * t) * 10000.0) as i16
                })
                .collect();

            let encoded = encoder.encode(&pcm).unwrap();
            let decoded = decoder.decode(&encoded).unwrap();
            assert_eq!(decoded.len(), FRAME_SAMPLES);
        }
    }

    #[test]
    fn wideband_encode_decode_roundtrip() {
        let mut encoder = OpusEncoderWideband::new().unwrap();
        let mut decoder = OpusDecoderWideband::new().unwrap();

        for frame_idx in 0..5 {
            let pcm: Vec<i16> = (0..FRAME_SAMPLES_WIDEBAND)
                .map(|i| {
                    let t = (frame_idx * FRAME_SAMPLES_WIDEBAND + i) as f32 / 16000.0;
                    (f32::sin(2.0 * std::f32::consts::PI * 1000.0 * t) * 16000.0) as i16
                })
                .collect();

            let encoded = encoder.encode(&pcm).unwrap();
            assert!(!encoded.is_empty());

            let decoded = decoder.decode(&encoded).unwrap();
            assert_eq!(decoded.len(), FRAME_SAMPLES_WIDEBAND);

            if frame_idx >= 2 {
                let energy: f64 = decoded.iter().map(|&s| (s as f64).powi(2)).sum();
                assert!(energy > 0.0, "wideband decoded frame {} should have energy", frame_idx);
            }
        }
    }
}
