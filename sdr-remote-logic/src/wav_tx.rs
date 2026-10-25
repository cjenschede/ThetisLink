// SPDX-License-Identifier: GPL-2.0-or-later

//! A recording on its way to a transmitter: WAV playback with PTT, from the
//! file's rate (8 or 16 kHz) to what the radio's chain takes - 16 kHz for the
//! Thetis encoder, the capture rate for a Yaesu.
//!
//! Until build 262 this was linear interpolation per tick, without a filter:
//! going up it let the images through, and going down (a 16 kHz recording to a
//! Yaesu whose microphone runs at 8 kHz) it folded everything above 4 kHz back.
//! Now it goes through `resample::resampler` with the long filter, like the
//! other paths towards a radio. One per playback and per radio, so the tail of
//! an earlier recording never sounds in the next.

use std::fmt;

use anyhow::Result;
use log::{info, warn};

use sdr_remote_core::resample::{resampler, Resampler, UpFilter};

/// One recording to one radio.
pub struct WavToRadio {
    from_hz: u32,
    to_hz: u32,
    chunk: usize,
    resampler: Resampler,
}

impl WavToRadio {
    /// Fed one tick (20 ms at `from_hz`) at a time.
    pub fn new(from_hz: u32, to_hz: u32) -> Result<Self> {
        let chunk = (from_hz / 50) as usize;
        Ok(Self { from_hz, to_hz, chunk, resampler: resampler(from_hz, to_hz, chunk, UpFilter::Long)? })
    }

    /// One tick of the recording. The last one of a file is shorter; it is
    /// filled with silence to a whole tick.
    pub fn feed(&mut self, tick: &[f32]) -> Vec<f32> {
        let mut out = Vec::new();
        for part in tick.chunks(self.chunk) {
            let mut input = part.to_vec();
            input.resize(self.chunk, 0.0);
            match self.resampler.process(input) {
                Ok(r) => out.extend_from_slice(&r),
                Err(e) => warn!("WAV TX resample error: {}", e),
            }
        }
        out
    }
}

/// For the log line: "8000 Hz to 16000 Hz, up with sinc 128, cutoff 0.95".
impl fmt::Display for WavToRadio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} Hz to {} Hz, {}", self.from_hz, self.to_hz, self.resampler)
    }
}

/// The converter in `slot` for this recording to `radio`: built and logged
/// when there is none yet, or when a rate changed (a headset swapped during
/// playback). Empty the slot when a new recording starts.
pub fn for_radio<'a>(slot: &'a mut Option<WavToRadio>, from_hz: u32, to_hz: u32, radio: &str) -> Option<&'a mut WavToRadio> {
    if !matches!(slot, Some(w) if w.from_hz == from_hz && w.to_hz == to_hz) {
        *slot = match WavToRadio::new(from_hz, to_hz) {
            Ok(w) => {
                info!("WAV TX resampler to {}: {}", radio, w);
                Some(w)
            }
            Err(e) => {
                warn!("WAV TX resampler to {}: {}", radio, e);
                None
            }
        };
    }
    slot.as_mut()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(f: f64, hz: u32) -> Vec<f32> {
        (0..hz as usize).map(|i| (0.5 * (2.0 * std::f64::consts::PI * f * i as f64 / hz as f64).sin()) as f32).collect()
    }

    /// Tick by tick, as the engine feeds it.
    fn through(x: &[f32], from: u32, to: u32) -> Vec<f32> {
        let mut w = WavToRadio::new(from, to).unwrap();
        x.chunks((from / 50) as usize).flat_map(|t| w.feed(t)).collect()
    }

    /// Level in dB, against the 0.5 of the tone, of `f` in the second half of `y`.
    fn level(y: &[f32], f: f64, hz: u32) -> f64 {
        let s = &y[y.len() / 2..];
        let n = s.len() as f64;
        let (mut re, mut im, mut w) = (0.0, 0.0, 0.0);
        for (i, &v) in s.iter().enumerate() {
            let h = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n).cos();
            let ph = 2.0 * std::f64::consts::PI * f * i as f64 / hz as f64;
            re += v as f64 * h * ph.cos();
            im += v as f64 * h * ph.sin();
            w += h;
        }
        20.0 * ((2.0 * (re * re + im * im).sqrt() / w) / 0.5).max(1e-9).log10()
    }

    #[test]
    fn a_recording_going_down_to_a_yaesu_keeps_the_aliases_out() {
        // 16 kHz recording, a microphone at 8 kHz: a 5 kHz tone must not come back at 3 kHz.
        let y = through(&tone(5000.0, 16000), 16000, 8000);
        let alias = level(&y, 3000.0, 8000);
        assert!(alias < -60.0, "alias at {alias:.1} dB");
        // and the speech band itself passes
        let y = through(&tone(1000.0, 16000), 16000, 8000);
        assert!(level(&y, 1000.0, 8000) > -1.0);
    }

    #[test]
    fn a_recording_going_up_keeps_the_images_out() {
        // 8 kHz recording to Thetis (16 kHz): a 3 kHz tone must not leave an image at 5 kHz.
        let y = through(&tone(3000.0, 8000), 8000, 16000);
        let image = level(&y, 5000.0, 16000);
        assert!(image < -60.0, "image at {image:.1} dB");
        assert!(level(&y, 3000.0, 16000) > -1.0);
    }

    #[test]
    fn the_last_short_tick_is_filled_and_goes_out() {
        let mut w = WavToRadio::new(8000, 16000).unwrap();
        let whole = w.feed(&[0.1; 160]).len() + w.feed(&[0.1; 160]).len();
        assert!(whole > 0);
        assert_eq!(w.feed(&[0.1; 40]).len(), 320, "a short tick comes out as a whole one");
        // and at the same rate it comes through whole as well
        let mut same = WavToRadio::new(16000, 16000).unwrap();
        assert_eq!(same.feed(&[0.1; 100]).len(), 320);
    }

    #[test]
    fn the_converter_is_kept_until_a_rate_changes() {
        let mut slot = None;
        assert_eq!(for_radio(&mut slot, 8000, 16000, "Thetis").unwrap().to_string(), "8000 Hz to 16000 Hz, up with sinc 128, cutoff 0.95");
        // the same rates: the same converter, its filter state kept - a new one
        // would give its first tick half a filter short
        let first = for_radio(&mut slot, 8000, 16000, "Thetis").unwrap().feed(&[0.1; 160]).len();
        assert!(first < 320, "a new converter holds back half a filter ({first})");
        assert_eq!(for_radio(&mut slot, 8000, 16000, "Thetis").unwrap().feed(&[0.1; 160]).len(), 320);
        // a headset swapped to 8 kHz during playback: a new one, going down
        assert_eq!(for_radio(&mut slot, 16000, 8000, "the Yaesu").unwrap().to_string(), "16000 Hz to 8000 Hz, down with sinc 128, cutoff 0.90");
        assert_eq!(for_radio(&mut slot, 16000, 16000, "Thetis").unwrap().to_string(), "16000 Hz to 16000 Hz, 1:1, no resampling");
    }
}
