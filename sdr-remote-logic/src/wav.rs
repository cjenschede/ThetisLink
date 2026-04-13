//! Simple WAV file writer — no external dependencies.
//! Writes 8kHz 16-bit mono PCM.

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

pub struct WavWriter {
    file: File,
    data_bytes: u32,
}

impl WavWriter {
    /// Create a new WAV file at the given path (8kHz, 16-bit, mono).
    pub fn new(path: &Path) -> io::Result<Self> {
        let mut file = File::create(path)?;
        // Write placeholder header (44 bytes), finalized on close
        let header = wav_header(0);
        file.write_all(&header)?;
        Ok(Self { file, data_bytes: 0 })
    }

    /// Write decoded i16 PCM samples.
    pub fn write_samples(&mut self, samples: &[i16]) -> io::Result<()> {
        for &s in samples {
            self.file.write_all(&s.to_le_bytes())?;
        }
        self.data_bytes += (samples.len() * 2) as u32;
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

    /// Finalize: rewrite header with correct sizes.
    pub fn finalize(mut self) -> io::Result<()> {
        let header = wav_header(self.data_bytes);
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header)?;
        self.file.flush()?;
        Ok(())
    }

    /// Duration in seconds based on bytes written.
    pub fn duration_secs(&self) -> f32 {
        self.data_bytes as f32 / (8000.0 * 2.0)
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
                .map(|&b| ((b as i16 - 128) * 256))
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

fn wav_header(data_bytes: u32) -> [u8; 44] {
    let sample_rate: u32 = 8000;
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
