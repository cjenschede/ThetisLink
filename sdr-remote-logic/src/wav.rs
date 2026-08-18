// SPDX-License-Identifier: GPL-2.0-or-later

//! Simple WAV file writer — no external dependencies.
//! Writes 16-bit mono PCM at the sample rate given to `new()`.

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

pub struct WavWriter {
    file: File,
    data_bytes: u32,
    /// 0 = not yet known; set on the first `write_samples` so the WAV rate
    /// automatically follows the actual decode rate (8k NB / 16k WB / future).
    sample_rate: u32,
}

impl WavWriter {
    /// Create a new WAV file at the given path (16-bit, mono). The sample rate
    /// is determined dynamically by the first `write_samples` call — not
    /// fixed up front, so a future rate change scales along automatically.
    pub fn new(path: &Path) -> io::Result<Self> {
        let mut file = File::create(path)?;
        // Placeholder header (rate 8000) — rewritten with the real rate at finalize.
        let header = wav_header(0, 8000);
        file.write_all(&header)?;
        Ok(Self { file, data_bytes: 0, sample_rate: 0 })
    }

    /// Write decoded i16 PCM samples at `sample_rate` Hz. The rate of the first
    /// write determines the WAV header — the caller passes the rate of the
    /// decoder in use, so the recording is correct regardless of NB/WB or future rates.
    /// Append samples, converting if the source rate has changed mid-file.
    ///
    /// A WAV file states one rate in its header and the first write decides it.
    /// The receive streams can change rate while recording - the wideband
    /// option is a toggle, and a client changing its mind switches the whole
    /// stream between 8 and 16 kHz - and the samples after such a switch used
    /// to be appended as if nothing had happened. The result is a file whose
    /// second half plays at half or double speed, in this player and in every
    /// other one.
    ///
    /// The two rates are always 8 k and 16 k, so the conversion is exactly two
    /// to one and needs no filter design to be worth having: dropping or
    /// interpolating alternate samples on what is already a diagnostic
    /// recording beats handing over a file that is wrong by an octave.
    pub fn write_samples(&mut self, samples: &[i16], sample_rate: u32) -> io::Result<()> {
        if self.sample_rate == 0 {
            self.sample_rate = sample_rate;
        }
        let converted: Vec<i16>;
        let out: &[i16] = if sample_rate == self.sample_rate {
            samples
        } else if sample_rate == self.sample_rate * 2 {
            // Halve: average pairs, so the discarded half is not simply lost.
            converted = samples
                .chunks(2)
                .map(|c| if c.len() == 2 { ((c[0] as i32 + c[1] as i32) / 2) as i16 } else { c[0] })
                .collect();
            &converted
        } else if self.sample_rate == sample_rate * 2 {
            // Double: one interpolated sample between each pair.
            converted = samples
                .windows(2)
                .flat_map(|w| [w[0], ((w[0] as i32 + w[1] as i32) / 2) as i16])
                .chain(samples.last().into_iter().flat_map(|&s| [s, s]))
                .collect();
            &converted
        } else {
            // Not a rate this project produces; better a gap than a wrong pitch.
            return Ok(());
        };
        for &s in out {
            self.file.write_all(&s.to_le_bytes())?;
        }
        self.data_bytes += (out.len() * 2) as u32;
        Ok(())
    }

    /// Write f32 PCM samples (converted to i16).
    pub fn write_f32(&mut self, samples: &[f32]) -> io::Result<()> {
        for &s in samples {
            let i = (s * 32767.0).clamp(-32768.0, 32767.0) as i16;
            self.file.write_all(&i.to_le_bytes())?;
        }
        self.data_bytes += (samples.len() * 2) as u32;
        Ok(())
    }

    /// Finalize: rewrite header with correct sizes + the determined sample rate.
    pub fn finalize(mut self) -> io::Result<()> {
        let rate = if self.sample_rate == 0 { 8000 } else { self.sample_rate };
        let header = wav_header(self.data_bytes, rate);
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header)?;
        self.file.flush()?;
        Ok(())
    }

    /// Duration in seconds based on bytes written.
    pub fn duration_secs(&self) -> f32 {
        let rate = if self.sample_rate == 0 { 8000 } else { self.sample_rate };
        self.data_bytes as f32 / (rate as f32 * 2.0)
    }
}

/// Read a WAV file into i16 samples. Returns (sample_rate, samples).
/// Supports 8-bit, 16-bit, and 32-bit float PCM.
pub fn read_wav(path: &Path) -> io::Result<(u32, Vec<i16>)> {
    use std::io::Read;
    let mut file = File::open(path)?;
    let mut header = [0u8; 44];
    file.read_exact(&mut header)?;

    if &header[0..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a WAV file"));
    }

    let channels = u16::from_le_bytes([header[22], header[23]]) as usize;
    let sample_rate = u32::from_le_bytes([header[24], header[25], header[26], header[27]]);
    let bits_per_sample = u16::from_le_bytes([header[34], header[35]]);

    // Find data chunk (skip any extra format chunks)
    let mut data_size = u32::from_le_bytes([header[40], header[41], header[42], header[43]]) as usize;
    if &header[36..40] != b"data" {
        // Search for data chunk
        let mut buf = vec![0u8; 4096];
        loop {
            let mut chunk_hdr = [0u8; 8];
            file.read_exact(&mut chunk_hdr)?;
            let chunk_size = u32::from_le_bytes([chunk_hdr[4], chunk_hdr[5], chunk_hdr[6], chunk_hdr[7]]) as usize;
            if &chunk_hdr[0..4] == b"data" {
                data_size = chunk_size;
                break;
            }
            // Skip unknown chunk (may be larger than buf)
            let mut remaining = chunk_size;
            while remaining > 0 {
                let skip = remaining.min(buf.len());
                file.read_exact(&mut buf[..skip])?;
                remaining -= skip;
            }
        }
    }

    let mut raw = vec![0u8; data_size];
    file.read_exact(&mut raw)?;

    let samples: Vec<i16> = match bits_per_sample {
        16 => {
            raw.chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .step_by(channels) // take first channel only
                .collect()
        }
        8 => {
            raw.iter()
                .step_by(channels)
                .map(|&b| (b as i16 - 128) * 256)
                .collect()
        }
        32 => {
            raw.chunks_exact(4)
                .step_by(channels)
                .map(|c| {
                    let f = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                    (f * 32767.0).clamp(-32768.0, 32767.0) as i16
                })
                .collect()
        }
        _ => return Err(io::Error::new(io::ErrorKind::InvalidData,
            format!("unsupported bits_per_sample: {}", bits_per_sample))),
    };

    Ok((sample_rate, samples))
}

fn wav_header(data_bytes: u32, sample_rate: u32) -> [u8; 44] {
    let bits_per_sample: u16 = 16;
    let channels: u16 = 1;
    let byte_rate: u32 = sample_rate * channels as u32 * bits_per_sample as u32 / 8;
    let block_align: u16 = channels * bits_per_sample / 8;
    let file_size = 36 + data_bytes;

    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&file_size.to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes()); // chunk size
    h[20..22].copy_from_slice(&1u16.to_le_bytes());  // PCM format
    h[22..24].copy_from_slice(&channels.to_le_bytes());
    h[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    h[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    h[32..34].copy_from_slice(&block_align.to_le_bytes());
    h[34..36].copy_from_slice(&bits_per_sample.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_bytes.to_le_bytes());
    h
}

#[cfg(test)]
mod rate_change_tests {
    use super::{read_wav, WavWriter};

    /// A recording that spans a narrowband/wideband switch must stay at one
    /// speed. Before this, the samples after the switch were appended raw and
    /// the second half of the file played at half or double speed.
    #[test]
    fn a_rate_change_mid_file_does_not_change_the_speed() {
        let dir = std::env::temp_dir().join("thetislink_wav_rate_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("mixed.wav");
        let _ = std::fs::remove_file(&path);

        let mut w = WavWriter::new(&path).unwrap();
        // One second at 8 kHz, then one second's worth arriving at 16 kHz.
        w.write_samples(&vec![1000i16; 8_000], 8_000).unwrap();
        w.write_samples(&vec![-1000i16; 16_000], 16_000).unwrap();
        w.finalize().unwrap();

        let (rate, samples) = read_wav(&path).unwrap();
        assert_eq!(rate, 8_000, "the header rate is set by the first write");
        // Two seconds of audio at the file's rate, not three.
        assert_eq!(samples.len(), 16_000, "the second stretch was not converted");
        assert_eq!(samples[0], 1000);
        assert_eq!(samples[15_999], -1000);
        let _ = std::fs::remove_file(&path);
    }

    /// And the other way round: a file that starts wide and continues narrow.
    #[test]
    fn a_drop_to_narrowband_is_stretched_back_up() {
        let dir = std::env::temp_dir().join("thetislink_wav_rate_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("mixed_down.wav");
        let _ = std::fs::remove_file(&path);

        let mut w = WavWriter::new(&path).unwrap();
        w.write_samples(&vec![500i16; 16_000], 16_000).unwrap();
        w.write_samples(&vec![-500i16; 8_000], 8_000).unwrap();
        w.finalize().unwrap();

        let (rate, samples) = read_wav(&path).unwrap();
        assert_eq!(rate, 16_000);
        assert_eq!(samples.len(), 32_000, "one second each, at the file's rate");
        let _ = std::fs::remove_file(&path);
    }
}
