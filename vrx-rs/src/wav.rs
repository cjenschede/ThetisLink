// SPDX-License-Identifier: GPL-2.0-or-later

//! Rolling WAV writer: dev tooling that captures channelizer audio
//! into fixed-length WAV segments. Activated by the runtime when
//! `VrxRuntimeOptions::wav_dir` is set.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

pub struct RollingWavWriter {
    dir: PathBuf,
    sample_rate: u32,
    segment_samples: u32,
    current: Option<(BufWriter<File>, u32, u32)>, // (writer, samples_written, segment_index)
    next_index: u32,
}

impl RollingWavWriter {
    pub fn new(dir: impl Into<PathBuf>, sample_rate: u32, segment_sec: u32) -> Self {
        Self {
            dir: dir.into(),
            sample_rate,
            segment_samples: sample_rate.saturating_mul(segment_sec),
            current: None,
            next_index: 1,
        }
    }

    /// Append audio samples. When the current segment reaches its
    /// target length, the file is closed (with proper RIFF header
    /// patched in) and the next one is opened.
    pub fn push(&mut self, samples: &[f32]) -> std::io::Result<()> {
        let mut remaining = samples;
        while !remaining.is_empty() {
            if self.current.is_none() {
                std::fs::create_dir_all(&self.dir)?;
                let idx = self.next_index;
                let path = self.dir.join(format!("vrx-live-{:03}.wav", idx));
                let mut f = BufWriter::new(File::create(&path)?);
                write_wav_header(&mut f, self.sample_rate, 0)?;
                self.current = Some((f, 0, idx));
                self.next_index += 1;
                log::info!(
                    "VRX live: opened WAV segment {} ({})",
                    idx,
                    path.display()
                );
            }
            let (writer, samples_written, _idx) = self.current.as_mut().unwrap();
            let room = self.segment_samples.saturating_sub(*samples_written) as usize;
            let take = remaining.len().min(room);
            for &s in &remaining[..take] {
                let clipped = s.clamp(-1.0, 1.0);
                let i16val = (clipped * 32767.0).round() as i16;
                writer.write_all(&i16val.to_le_bytes())?;
            }
            *samples_written += take as u32;
            remaining = &remaining[take..];

            if *samples_written >= self.segment_samples {
                self.close_current()?;
            }
        }
        Ok(())
    }

    fn close_current(&mut self) -> std::io::Result<()> {
        if let Some((mut writer, samples_written, idx)) = self.current.take() {
            writer.flush()?;
            let file = writer.into_inner().map_err(|e| e.into_error())?;
            let mut file = file;
            patch_wav_header(&mut file, samples_written)?;
            log::info!(
                "VRX live: closed WAV segment {} ({} samples)",
                idx,
                samples_written
            );
        }
        Ok(())
    }
}

impl Drop for RollingWavWriter {
    fn drop(&mut self) {
        let _ = self.close_current();
    }
}

fn write_wav_header(
    w: &mut impl Write,
    rate: u32,
    placeholder_samples: u32,
) -> std::io::Result<()> {
    let data_bytes = placeholder_samples.saturating_mul(2);
    let riff_size = 36u32.saturating_add(data_bytes);
    let byte_rate = rate.saturating_mul(2);
    w.write_all(b"RIFF")?;
    w.write_all(&riff_size.to_le_bytes())?;
    w.write_all(b"WAVE")?;
    w.write_all(b"fmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&1u16.to_le_bytes())?; // PCM
    w.write_all(&1u16.to_le_bytes())?; // mono
    w.write_all(&rate.to_le_bytes())?;
    w.write_all(&byte_rate.to_le_bytes())?;
    w.write_all(&2u16.to_le_bytes())?; // block align
    w.write_all(&16u16.to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&data_bytes.to_le_bytes())?;
    Ok(())
}

fn patch_wav_header(file: &mut File, samples_written: u32) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom};
    let data_bytes = samples_written.saturating_mul(2);
    let riff_size = 36u32.saturating_add(data_bytes);
    file.seek(SeekFrom::Start(4))?;
    file.write_all(&riff_size.to_le_bytes())?;
    file.seek(SeekFrom::Start(40))?;
    file.write_all(&data_bytes.to_le_bytes())?;
    Ok(())
}
