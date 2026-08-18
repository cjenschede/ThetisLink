// SPDX-License-Identifier: GPL-2.0-or-later

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
    /// The voice encoder: for a microphone, which is what it is for.
    ///
    /// Every receive path uses [`Self::new_rx_continuous`] instead - a
    /// receiver hands over band noise, tones and digital modes, and silence
    /// suppression on a stream that is never silent is what an operator hears
    /// as roughness. This one keeps DTX, because a person really does stop
    /// talking.
    pub fn new() -> Result<Self> {
        let mut encoder = Encoder::new(SampleRate::Hz8000, Channels::Mono, Application::Voip)
            .context("failed to create Opus encoder")?;
        encoder
            .set_bitrate(Bitrate::BitsPerSecond(12_800))
            .context("set bitrate")?;
        encoder
            .set_bandwidth(Bandwidth::Narrowband)
            .context("set bandwidth")?;
        encoder.set_signal(Signal::Voice).context("set signal type")?;
        encoder.set_inband_fec(true).context("enable FEC")?;
        encoder.set_dtx(true).context("enable DTX")?;
        encoder
            .set_packet_loss_perc(10)
            .context("set packet loss")?;
        Ok(Self {
            encoder,
            encode_buf: vec![0u8; MAX_ENCODED_SIZE],
        })
    }

    /// The receive encoder, for every stream that comes back from the radio
    /// end - Thetis, both radios, both VRX channels.
    ///
    /// The radio path once had its own signal model (Audio + Auto, on the
    /// argument that a receiver delivers noise and not speech) and was moved
    /// onto this one. That was suspected of a station's 991A sounding wrong
    /// and put back to check; it made no difference, and the fault was
    /// elsewhere (2026-08-14). Recorded here so the next person does not spend
    /// the evening on it again.
    ///
    /// Two streams reach the same operator over the same wire: this one, and
    /// VRX, demodulated inside the server. One is called rough and the other
    /// never is. Every difference between their encoders has now been closed
    /// one at a time - silence suppression first, and with it still rough,
    /// error correction is the only one left. So this is the VRX
    /// configuration with nothing added: Voip, Voice, 12.8 kbps, narrowband,
    /// no DTX, no in-band FEC, no packet-loss reservation.
    ///
    /// The last two are not free to keep. In-band FEC and a stated ten
    /// percent loss make Opus hold bits back to repeat itself with, and at
    /// 12.8 kbps there are not many bits to hold back - so the sound is paid
    /// for out of the same purse that pays for resilience. VRX has never
    /// spent anything on resilience and is the one that sounds right.
    ///
    /// An operator confirmed the sound was right without it, and the durable
    /// answer followed in the next build: protection is no longer a fixed
    /// setting at all but is switched on from measured loss - see
    /// `set_loss_protection` and the policy that drives it. What stays true
    /// here is the starting point: no redundancy until a link asks for it.
    ///
    /// Still worth stating plainly: the build that settled this changed two
    /// things at once - error correction off and the measuring apparatus out -
    /// so error correction is the strong candidate and not a proven cause.
    pub fn new_rx_continuous() -> Result<Self> {
        let mut encoder = Encoder::new(SampleRate::Hz8000, Channels::Mono, Application::Voip)
            .context("failed to create continuous-RX Opus encoder")?;
        encoder
            .set_bitrate(Bitrate::BitsPerSecond(12_800))
            .context("set bitrate")?;
        encoder
            .set_bandwidth(Bandwidth::Narrowband)
            .context("set bandwidth")?;
        encoder.set_signal(Signal::Voice).context("set signal type")?;
        encoder.set_inband_fec(false).context("disable FEC")?;
        encoder.set_dtx(false).context("disable DTX")?;
        encoder
            .set_packet_loss_perc(0)
            .context("set packet loss")?;
        Ok(Self {
            encoder,
            encode_buf: vec![0u8; MAX_ENCODED_SIZE],
        })
    }

    /// Turn packet-loss protection on or off on a running encoder.
    ///
    /// Opus pays for in-band redundancy out of the same bits it pays for sound
    /// with, so on a clean link this is a quality loss bought for nothing -
    /// which is what an operator heard, and why the receive encoders now start
    /// without it. On a link that actually drops packets it is the other way
    /// round entirely. Neither answer is right all the time, so the caller
    /// decides from measured loss instead of guessing once at construction.
    ///
    /// Both settings are plain runtime controls; nothing is reallocated and no
    /// audio is interrupted. The change takes effect from the next frame.
    pub fn set_loss_protection(&mut self, on: bool, loss_pct: u8) -> Result<()> {
        self.encoder
            .set_inband_fec(on)
            .context("set FEC")?;
        self.encoder
            .set_packet_loss_perc(if on { loss_pct.min(100) } else { 0 })
            .context("set packet loss")
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

    /// The VRX wideband encoder, exactly, for the Thetis receive path.
    ///
    /// Same reasoning as [`OpusEncoder::new_rx_continuous`] - VRX runs
    /// wideband this way too - and it matters more here: this is the stream an
    /// operator called rough while calling the narrowband one clean.
    pub fn new_rx_continuous() -> Result<Self> {
        let mut encoder = Encoder::new(SampleRate::Hz16000, Channels::Mono, Application::Voip)
            .context("failed to create continuous-RX wideband Opus encoder")?;
        encoder
            .set_bitrate(Bitrate::BitsPerSecond(24_000))
            .context("set bitrate")?;
        encoder
            .set_bandwidth(Bandwidth::Wideband)
            .context("set bandwidth")?;
        encoder.set_signal(Signal::Voice).context("set signal type")?;
        encoder.set_inband_fec(false).context("disable FEC")?;
        encoder.set_dtx(false).context("disable DTX")?;
        encoder
            .set_packet_loss_perc(0)
            .context("set packet loss")?;
        Ok(Self {
            encoder,
            encode_buf: vec![0u8; MAX_ENCODED_SIZE],
        })
    }

    pub fn set_bitrate_bps(&mut self, bitrate: i32) -> Result<()> {
        self.encoder
            .set_bitrate(Bitrate::BitsPerSecond(bitrate))
            .context("set wideband bitrate")
    }

    /// Turn packet-loss protection on or off on a running encoder.
    ///
    /// Opus pays for in-band redundancy out of the same bits it pays for sound
    /// with, so on a clean link this is a quality loss bought for nothing -
    /// which is what an operator heard, and why the receive encoders now start
    /// without it. On a link that actually drops packets it is the other way
    /// round entirely. Neither answer is right all the time, so the caller
    /// decides from measured loss instead of guessing once at construction.
    ///
    /// Both settings are plain runtime controls; nothing is reallocated and no
    /// audio is interrupted. The change takes effect from the next frame.
    pub fn set_loss_protection(&mut self, on: bool, loss_pct: u8) -> Result<()> {
        self.encoder
            .set_inband_fec(on)
            .context("set FEC")?;
        self.encoder
            .set_packet_loss_perc(if on { loss_pct.min(100) } else { 0 })
            .context("set packet loss")
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

    /// The two receive encoders are deliberately not the same, and both are
    /// deliberately continuous. Merging them once already cost a station its
    /// microphone comparison, so this holds the shape rather than the values:
    /// both must exist, both must keep producing real frames on steady noise.
    ///
    /// Every receive path in ThetisLink now shares one configuration, and the
    /// part of it that matters is that a stream which is never silent is never
    /// treated as silent. Steady input must always produce a real frame - a
    /// one or two byte silence frame here is the fault an operator heard as
    /// roughness (2026-08-13).
    #[test]
    fn receive_encoders_never_emit_silence_frames() {
        let mut nb = OpusEncoder::new_rx_continuous().unwrap();
        let mut wb = OpusEncoderWideband::new_rx_continuous().unwrap();

        // Steady band noise: no tone for a voice model to lock on to, which is
        // exactly the material a voice-activity detector calls silence.
        let mut seed = 12345u32;
        let mut noise = || {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            ((seed >> 16) as i16) / 8
        };
        for frame in 0..50 {
            let a: Vec<i16> = (0..FRAME_SAMPLES).map(|_| noise()).collect();
            let p = nb.encode(&a).unwrap();
            assert!(p.len() > 2, "narrowband frame {frame} collapsed to {} bytes", p.len());

            let b: Vec<i16> = (0..FRAME_SAMPLES_WIDEBAND).map(|_| noise()).collect();
            let p = wb.encode(&b).unwrap();
            assert!(p.len() > 2, "wideband frame {frame} collapsed to {} bytes", p.len());
        }
    }

    /// Protection is a runtime control, so turning it on and off mid-stream
    /// must keep producing decodable audio rather than needing a new encoder.
    #[test]
    fn loss_protection_can_be_switched_mid_stream() {
        let mut wb = OpusEncoderWideband::new_rx_continuous().unwrap();
        let mut dec = OpusDecoderWideband::new().unwrap();
        let tone: Vec<i16> = (0..FRAME_SAMPLES_WIDEBAND)
            .map(|i| ((i as f32 * std::f32::consts::TAU * 900.0 / 16000.0).sin() * 6000.0) as i16)
            .collect();

        for round in 0..6 {
            if round == 2 {
                wb.set_loss_protection(true, 8).unwrap();
            }
            if round == 4 {
                wb.set_loss_protection(false, 0).unwrap();
            }
            let packet = wb.encode(&tone).unwrap();
            assert_eq!(dec.decode(&packet).unwrap().len(), FRAME_SAMPLES_WIDEBAND,
                "round {round} did not decode to a full frame");
        }
    }

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

    /// The fact the whole concealment fault rests on, and it was written down
    /// nowhere: a decoder that has never decoded anything conceals SILENCE.
    /// Opus extrapolates from history, so with no history there is nothing to
    /// extrapolate. That is why picking the wrong decoder for concealment does
    /// not sound wrong - it sounds like nothing at all, which reads as "the
    /// audio stopped" rather than as a bug (2026-08-16).
    #[test]
    fn plc_on_a_fresh_decoder_is_silent() {
        let mut nb = OpusDecoder::new().unwrap();
        let mut wb = OpusDecoderWideband::new().unwrap();

        let a = nb.decode_plc().unwrap();
        assert_eq!(a.len(), FRAME_SAMPLES);
        assert_eq!(a.iter().map(|s| s.abs()).max().unwrap(), 0,
            "narrowband PLC without history should be silent");

        let b = wb.decode_plc().unwrap();
        assert_eq!(b.len(), FRAME_SAMPLES_WIDEBAND);
        assert_eq!(b.iter().map(|s| s.abs()).max().unwrap(), 0,
            "wideband PLC without history should be silent");
    }

    /// How long does concealment stay audible? The operator hears it as radio
    /// noise over a short dropout and expects several seconds of it. Opus is
    /// built to bridge a lost packet, not a lost link, so it is entirely
    /// possible that it fades on its own - and if it does, that is codec
    /// behaviour and not a fault to go hunting for. Measured here rather than
    /// in the field: band noise in, then concealment for three seconds, peak
    /// held against the last real frame (2026-08-16).
    #[test]
    fn plc_fades_and_this_is_how_fast() {
        let mut seed = 20260816u32;
        let mut noise = || {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            ((seed >> 16) as i16) / 8
        };

        for wide in [false, true] {
            let n = if wide { FRAME_SAMPLES_WIDEBAND } else { FRAME_SAMPLES };
            let mut enc_nb = OpusEncoder::new_rx_continuous().unwrap();
            let mut enc_wb = OpusEncoderWideband::new_rx_continuous().unwrap();
            let mut dec_nb = OpusDecoder::new().unwrap();
            let mut dec_wb = OpusDecoderWideband::new().unwrap();

            // Half a second of real audio so the decoder has history.
            let mut last_real = 0i16;
            for _ in 0..25 {
                let pcm: Vec<i16> = (0..n).map(|_| noise()).collect();
                let out = if wide {
                    let p = enc_wb.encode(&pcm).unwrap();
                    dec_wb.decode(&p).unwrap()
                } else {
                    let p = enc_nb.encode(&pcm).unwrap();
                    dec_nb.decode(&p).unwrap()
                };
                last_real = out.iter().map(|s| s.abs()).max().unwrap();
            }

            // Three seconds of concealment at 20 ms per frame.
            let mut gone_at: Option<usize> = None;
            let mut trace = String::new();
            for i in 0..150 {
                let out = if wide { dec_wb.decode_plc().unwrap() } else { dec_nb.decode_plc().unwrap() };
                assert_eq!(out.len(), n);
                let peak = out.iter().map(|s| s.abs()).max().unwrap();
                if i == 0 {
                    assert!(peak > 0, "first concealed frame must carry audio (history present)");
                }
                if (peak as i32) * 100 < last_real as i32 && gone_at.is_none() {
                    gone_at = Some(i);
                }
                if i % 10 == 0 {
                    trace.push_str(&format!(" {}ms={}", i * 20, peak));
                }
            }
            println!(
                "PLC {} - last real peak {}, {}, inaudible (<1%) after {}",
                if wide { "wideband" } else { "narrowband" },
                last_real,
                trace.trim(),
                match gone_at { Some(i) => format!("{} ms", i * 20), None => "never within 3 s".to_string() }
            );
        }
    }

}
