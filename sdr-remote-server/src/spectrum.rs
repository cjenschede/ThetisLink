#![allow(dead_code)]
use std::sync::Arc;
use std::time::Instant;

use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

use sdr_remote_core::protocol::SpectrumPacket;
use sdr_remote_core::{ddc_fft_size, SPECTRUM_BINS};

/// Number of 16-bit samples per complete wideband frame (legacy HPSDR constant, still used for wideband FFT)
const WIDEBAND_SAMPLES: usize = 16_384;

/// Spectrum processor: generates test data or processes real FFT data.
/// Rate-limited to configured FPS.
/// Supports server-side zoom/pan extraction from 262k-bin smoothed buffer.
pub struct SpectrumProcessor {
    enabled: bool,
    fps: u8,
    sequence: u16,
    last_frame: Instant,
    /// Smoothed spectrum bins for EMA filtering (262144 bins in DDC mode)
    smoothed: Vec<f32>,
    /// Whether we have real HPSDR data (false = use test generator)
    has_real_data: bool,
    /// FFT pipeline for processing real ADC samples (wideband)
    fft_pipeline: Option<FftPipeline>,
    /// FFT pipeline for DDC I/Q data
    ddc_pipeline: Option<DdcFftPipeline>,
    /// Test signal phase accumulator
    test_phase: f32,
    /// VFO frequency in Hz (= DDC NCO center when CTUN is off)
    vfo_freq_hz: u64,
    /// DDC display center in Hz (stays fixed during tuning, recenters after settling)
    ddc_center_hz: u64,
    /// DDC sample rate in Hz (determines span)
    sample_rate_hz: u32,
    /// Whether DDC mode is active (false = wideband)
    ddc_mode: bool,
    /// Whether DDC center is being set from HP packets (true = use HP data, false = use CAT/heuristic)
    has_hp_center: bool,
    /// Skip N FFT frames after frequency change (stale pipeline data)
    skip_fft_frames: u8,
    /// TCI mode: IQ data is always at correct frequency, no shift/skip needed
    tci_mode: bool,
    /// Custom FFT size override (None = auto from sample rate)
    custom_fft_size: Option<usize>,
    /// Calibration offset in dB (from TCI calibration_ex display_offset)
    /// Added to spectrum bins to compensate for hardware attenuator/preamp
    cal_offset_db: f32,
}

impl SpectrumProcessor {
    pub fn new() -> Self {
        Self {
            enabled: false,
            fps: sdr_remote_core::DEFAULT_SPECTRUM_FPS,
            sequence: 0,
            last_frame: Instant::now(),
            smoothed: vec![0.0; SPECTRUM_BINS],
            has_real_data: false,
            fft_pipeline: None,
            ddc_pipeline: None,
            test_phase: 0.0,
            vfo_freq_hz: 14_200_000, // Default 20m band
            ddc_center_hz: 14_200_000,
            sample_rate_hz: 1_536_000,
            ddc_mode: false,
            has_hp_center: false,
            skip_fft_frames: 0,
            tci_mode: false,
            custom_fft_size: None,
            cal_offset_db: 0.0,
        }
    }

    /// Get current calibration offset (dB).
    pub fn cal_offset_db(&self) -> f32 {
        self.cal_offset_db
    }

    /// Set calibration offset (dB) to compensate for hardware attenuator/preamp.
    pub fn set_cal_offset_db(&mut self, offset: f32) {
        self.cal_offset_db = offset;
    }

    /// Enable TCI mode: no buffer shifting or frame skipping on freq changes.
    pub fn set_tci_mode(&mut self, enabled: bool) {
        self.tci_mode = enabled;
    }

    /// Initialize the FFT pipeline for real wideband data processing
    pub fn init_fft(&mut self) {
        self.fft_pipeline = Some(FftPipeline::new());
        log::info!("FFT pipeline initialized ({}pt → {} bins)", WIDEBAND_SAMPLES, SPECTRUM_BINS);
    }

    /// Initialize the DDC FFT pipeline for complex I/Q data
    pub fn init_ddc_fft(&mut self, sample_rate_hz: u32) {
        let fft_size = self.custom_fft_size.unwrap_or_else(|| ddc_fft_size(sample_rate_hz));
        self.ddc_pipeline = Some(DdcFftPipeline::new(fft_size));
        self.sample_rate_hz = sample_rate_hz;
        self.ddc_mode = true;
        self.ddc_center_hz = self.vfo_freq_hz;
        self.smoothed = vec![0.0; fft_size];
        log::info!("DDC FFT pipeline initialized ({}pt, {}kHz span, {:.3} Hz/bin)",
            fft_size, sample_rate_hz / 1000,
            sample_rate_hz as f64 / fft_size as f64);
    }

    /// Update sample rate and reinitialize FFT pipeline if rate changed.
    /// Called by auto-detection when DDC sample rate changes in Thetis.
    pub fn update_sample_rate(&mut self, sample_rate_hz: u32) {
        if sample_rate_hz == self.sample_rate_hz {
            return;
        }
        let old_rate = self.sample_rate_hz;
        let fft_size = self.custom_fft_size.unwrap_or_else(|| ddc_fft_size(sample_rate_hz));
        self.ddc_pipeline = Some(DdcFftPipeline::new(fft_size));
        self.sample_rate_hz = sample_rate_hz;
        self.smoothed = vec![0.0; fft_size];
        self.has_real_data = false;
        log::info!("DDC sample rate changed: {}kHz → {}kHz (FFT: {} → {})",
            old_rate / 1000, sample_rate_hz / 1000,
            ddc_fft_size(old_rate), fft_size);
    }

    /// Set custom FFT size (overrides auto-calculated size).
    /// size_k: FFT size in K (e.g. 32=32768, 65=65536). 0 = auto (revert to default).
    pub fn set_fft_size(&mut self, size_k: u16) {
        let fft_size = match size_k {
            0 => {
                self.custom_fft_size = None;
                ddc_fft_size(self.sample_rate_hz)
            }
            _ => {
                let size = (size_k as usize * 1024).next_power_of_two().clamp(4096, 524288);
                self.custom_fft_size = Some(size);
                size
            }
        };
        let old_size = self.ddc_pipeline.as_ref().map(|p| p.window.len()).unwrap_or(0);
        if fft_size == old_size {
            return;
        }
        self.ddc_pipeline = Some(DdcFftPipeline::new(fft_size));
        self.smoothed = vec![0.0; fft_size];
        self.has_real_data = false;
        let hop = sdr_remote_core::ddc_hop_size(fft_size);
        let fft_per_sec = self.sample_rate_hz as f32 / hop as f32;
        log::info!("FFT size changed: {}K → {}K ({:.1} Hz/bin, {:.1} FFT/sec)",
            old_size / 1024, fft_size / 1024,
            self.sample_rate_hz as f64 / fft_size as f64,
            fft_per_sec);
    }

    /// Get current FFT size
    pub fn current_fft_size(&self) -> usize {
        self.ddc_pipeline.as_ref().map(|p| p.window.len()).unwrap_or(0)
    }

    /// Process raw ADC samples from HPSDR wideband capture
    pub fn process_adc_samples(&mut self, samples: &[i16]) {
        if let Some(ref mut pipeline) = self.fft_pipeline {
            let bins = pipeline.process(samples);
            self.update_from_fft(&bins);
            self.has_real_data = true;
        }
    }

    /// Get current DDC FFT size (for accumulation buffer sizing)
    pub fn ddc_fft_size(&self) -> usize {
        self.ddc_pipeline.as_ref().map(|p| p.window.len()).unwrap_or(65536)
    }

    /// Process DDC I/Q samples from HPSDR DDC capture
    pub fn process_ddc_frame(&mut self, samples: &[(f32, f32)]) {
        if let Some(ref mut pipeline) = self.ddc_pipeline {
            let bins = pipeline.process(samples);
            self.update_from_fft(&bins);
            self.has_real_data = true;
        }
    }

    /// Set VFO frequency (called when CAT reports frequency change).
    /// In DDC mode without HP data: DDC center follows VFO (CTUN off) or freezes (CTUN on).
    /// With HP data: DDC center is set directly from HP packets, VFO only used for display.
    pub fn set_vfo_freq(&mut self, freq_hz: u64, ctun: bool) {
        self.vfo_freq_hz = freq_hz;

        if self.ddc_mode && !self.has_hp_center {
            if !ctun {
                // CTUN off: DDC center = VFO (normal behavior)
                let old_center = self.ddc_center_hz;
                self.ddc_center_hz = freq_hz;

                if !self.tci_mode && old_center != 0 && old_center != freq_hz {
                    self.shift_smoothed(old_center, freq_hz);
                }
            }
            // CTUN on: freeze ddc_center_hz (don't follow VFO)
        }
    }

    /// Set DDC center from Protocol 2 HP packet (absolute, overrides VFO).
    pub fn set_ddc_center(&mut self, freq_hz: u64) {
        if !self.ddc_mode { return; }
        let old = self.ddc_center_hz;
        self.ddc_center_hz = freq_hz;
        self.has_hp_center = true;

        if old != 0 && old != freq_hz {
            self.shift_smoothed(old, freq_hz);
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn set_fps(&mut self, fps: u8) {
        self.fps = fps.clamp(5, 30);
    }

    /// Check if a new frame is ready (rate-limited). Increments sequence if ready.
    /// Call this once per tick, then use extract_view() for each client.
    pub fn is_frame_ready(&mut self) -> bool {
        if !self.enabled {
            return false;
        }

        let interval_ms = 1000 / self.fps as u64;
        if self.last_frame.elapsed().as_millis() < interval_ms as u128 {
            return false;
        }
        self.last_frame = Instant::now();

        // Generate test data into smoothed buffer if no real data
        if !self.has_real_data {
            if self.ddc_mode {
                self.generate_test_ddc_into_smoothed();
            } else {
                self.generate_test_into_smoothed();
            }
        }

        self.sequence = self.sequence.wrapping_add(1);
        true
    }

    /// Extract a zoom/pan-dependent view from the smoothed buffer.
    /// Returns a SpectrumPacket with max `max_bins` bins.
    pub fn extract_view(&self, zoom: f32, pan: f32, max_bins: usize) -> SpectrumPacket {
        let total = self.smoothed.len();
        if total == 0 {
            return SpectrumPacket {
                sequence: self.sequence,
                num_bins: 0,
                center_freq_hz: 0,
                span_hz: 0,
                ref_level: 0,
                db_per_unit: 1,
                bins: Vec::new(),
            };
        }

        let zoom = zoom.max(1.0);
        let visible = ((total as f64) / zoom as f64) as usize;
        let visible = visible.max(1);

        // VFO auto-center: offset pan so that pan=0 centers on VFO,
        // regardless of CTUN setting (VFO may differ from DDC center).
        let vfo_offset = if self.ddc_mode && self.vfo_freq_hz != 0 {
            let delta_hz = self.vfo_freq_hz as f64 - self.ddc_center_hz as f64;
            (delta_hz / self.sample_rate_hz as f64) as f32
        } else {
            0.0
        };
        // Center of view: DC (total/2) + pan offset + VFO auto-center offset
        let center = total as isize / 2 + ((pan + vfo_offset) * total as f32) as isize;
        let half = visible as isize / 2;
        let start = (center - half).max(0).min((total - visible) as isize) as usize;
        let end = (start + visible).min(total);

        // Calibration offset: convert dB to bin-value shift (120 dB = 65535 range)
        let cal_shift = self.cal_offset_db * 65535.0 / 120.0;

        let bins: Vec<u16> = if visible <= max_bins {
            // Native resolution: send bins directly
            self.smoothed[start..end].iter()
                .map(|v| (v + cal_shift).clamp(0.0, 65535.0) as u16)
                .collect()
        } else {
            // Decimation: max-per-group to preserve signal peaks.
            // Use floating-point stride to cover the FULL visible range evenly.
            // Integer division would leave a gap at the right edge for non-power-of-2 zoom.
            let stride = visible as f64 / max_bins as f64;
            (0..max_bins).map(|i| {
                let s = start + (i as f64 * stride) as usize;
                let e = (start + ((i as f64 + 1.0) * stride) as usize).min(end);
                let mut max_val = 0.0f32;
                for j in s..e {
                    if self.smoothed[j] > max_val {
                        max_val = self.smoothed[j];
                    }
                }
                (max_val + cal_shift).clamp(0.0, 65535.0) as u16
            }).collect()
        };

        // Compute center frequency and span for this view.
        // DDC buffer is centered on ddc_center_hz (the DDC NCO frequency).
        // VFO marker moves within the display (CTUN-like behavior).
        let (view_center_hz, view_span_hz) = if self.ddc_mode {
            let hz_per_bin = self.sample_rate_hz as f64 / total as f64;
            let bin_offset = (start + end) as f64 / 2.0 - total as f64 / 2.0;
            let center_hz = self.ddc_center_hz as f64 + bin_offset * hz_per_bin;
            let span_hz = visible as f64 * hz_per_bin;
            (center_hz.round() as u32, span_hz as u32)
        } else {
            // Wideband: 0-61.44 MHz
            (30_720_000, 61_440_000)
        };

        SpectrumPacket {
            sequence: self.sequence,
            num_bins: bins.len() as u16,
            center_freq_hz: view_center_hz,
            span_hz: view_span_hz,
            ref_level: 0,
            db_per_unit: 1,
            bins,
        }
    }

    /// Get the full DDC spectrum downsampled to max_bins bins (for waterfall history).
    /// Covers the entire DDC bandwidth without zoom/pan extraction.
    pub fn get_full_row(&self, max_bins: usize) -> SpectrumPacket {
        let total = self.smoothed.len();
        if total == 0 || !self.ddc_mode {
            return SpectrumPacket {
                sequence: self.sequence,
                num_bins: 0,
                center_freq_hz: 0,
                span_hz: 0,
                ref_level: 0,
                db_per_unit: 1,
                bins: Vec::new(),
            };
        }

        let cal_shift = self.cal_offset_db * 65535.0 / 120.0;
        let bins: Vec<u16> = if total <= max_bins {
            self.smoothed.iter().map(|v| (v + cal_shift).clamp(0.0, 65535.0) as u16).collect()
        } else {
            let stride = total as f64 / max_bins as f64;
            (0..max_bins).map(|i| {
                let s = (i as f64 * stride) as usize;
                let e = (((i + 1) as f64 * stride) as usize).min(total);
                let mut max_val = 0.0f32;
                for j in s..e {
                    if self.smoothed[j] > max_val {
                        max_val = self.smoothed[j];
                    }
                }
                (max_val + cal_shift).clamp(0.0, 65535.0) as u16
            }).collect()
        };

        SpectrumPacket {
            sequence: self.sequence,
            num_bins: bins.len() as u16,
            center_freq_hz: self.ddc_center_hz as u32,
            span_hz: self.sample_rate_hz,
            ref_level: 0,
            db_per_unit: 1,
            bins,
        }
    }

    /// Process real ADC samples from HPSDR capture into spectrum data.
    /// Peak-hold with log recursive decay (matches Thetis "Peak, log recursive 120ms").
    /// New peaks are captured instantly; decay follows exponential falloff.
    pub fn update_from_fft(&mut self, bins: &[f32]) {
        // Skip stale FFT frames after frequency change (pipeline had old I/Q data)
        if self.skip_fft_frames > 0 {
            self.skip_fft_frames -= 1;
            return;
        }
        // Decay factor: at ~11.7 FFT/sec, lower decay = faster response.
        // decay = 0.35 gives ~50ms effective time constant (snappier spectrum).
        let decay = 0.35f32;
        let len = bins.len().min(self.smoothed.len());
        for i in 0..len {
            if bins[i] >= self.smoothed[i] {
                // Peak: instant capture (no smoothing on rising signals)
                self.smoothed[i] = bins[i];
            } else {
                // Decay: exponential falloff toward new value
                self.smoothed[i] = self.smoothed[i] * decay + bins[i] * (1.0 - decay);
            }
        }
    }

    /// Compute total power in the receiver passband from the smoothed FFT bins.
    /// filter_low/high are in Hz relative to VFO (e.g. 50..2850 for USB, -2850..-50 for LSB).
    /// Edge bins are weighted by their fractional overlap with the passband.
    /// Sums linear power (integration), so both narrowband signals and wideband noise
    /// are correctly measured. Returns power in dBm.
    pub fn compute_passband_power_dbm(&self, filter_low: i32, filter_high: i32) -> f32 {
        let n = self.smoothed.len();
        if n == 0 || !self.has_real_data || self.sample_rate_hz == 0 {
            return -140.0;
        }
        let hz_per_bin = self.sample_rate_hz as f64 / n as f64;
        let half = n / 2;

        // VFO offset from DDC center
        let vfo_offset_hz = self.vfo_freq_hz as f64 - self.ddc_center_hz as f64;

        // Passband edges as fractional bin positions
        let bin_lo_f = half as f64 + (vfo_offset_hz + filter_low as f64) / hz_per_bin;
        let bin_hi_f = half as f64 + (vfo_offset_hz + filter_high as f64) / hz_per_bin;

        // Integer bin range that overlaps the passband
        let first = (bin_lo_f.floor() as isize).max(0) as usize;
        let last = ((bin_hi_f.ceil() as isize) - 1).clamp(0, n as isize - 1) as usize;

        if first > last {
            return -140.0;
        }

        // Sum linear power across passband bins (integrate, not average).
        // Each bin covers [i, i+1) in bin-units. Edge bins get fractional weight.
        // smoothed[i] is 0-255 → dB = val * 120/255 - 150
        let mut sum_power = 0.0f64;

        for i in first..=last {
            let overlap = ((i + 1) as f64).min(bin_hi_f) - (i as f64).max(bin_lo_f);
            if overlap <= 0.0 { continue; }

            let db = self.smoothed[i] as f64 * 120.0 / 65535.0 - 150.0;
            sum_power += overlap * 10.0_f64.powf(db / 10.0);
        }

        if sum_power <= 0.0 {
            return -140.0;
        }

        // +3 dB correction for Hann window coherent gain / FFT normalization
        (10.0 * sum_power.log10() + 3.0) as f32 + self.cal_offset_db
    }

    /// Raw passband power without calibration offset (for auto-calibration).
    pub fn compute_raw_passband_power_dbm(&self, filter_low: i32, filter_high: i32) -> f32 {
        self.compute_passband_power_dbm(filter_low, filter_high) - self.cal_offset_db
    }

    /// Shift the smoothed buffer when center frequency changes.
    /// Small shifts: move bins left/right. Large jumps: reset entirely.
    fn shift_smoothed(&mut self, old_center: u64, new_center: u64) {
        let total = self.smoothed.len();
        if total == 0 { return; }

        let delta = (new_center as i64 - old_center as i64).unsigned_abs();
        let hz_per_bin = self.sample_rate_hz as f64 / total as f64;
        let shift_bins = ((new_center as f64 - old_center as f64) / hz_per_bin).round() as isize;

        if delta > self.sample_rate_hz as u64 / 4 || shift_bins.unsigned_abs() >= total {
            // Band change: reset
            self.smoothed.fill(0.0);
            self.has_real_data = false;
            return;
        }

        if shift_bins == 0 { return; }

        // Skip next FFT frames (pipeline still has old-frequency I/Q data)
        // TCI mode: IQ data is immediately at new freq, no skip needed
        if !self.tci_mode {
            self.skip_fft_frames = 2;
        }

        // Shift bins: freq went up → data shifts left (positive shift_bins)
        if shift_bins > 0 {
            let s = shift_bins as usize;
            self.smoothed.copy_within(s.., 0);
            for i in (total - s)..total { self.smoothed[i] = 0.0; }
        } else {
            let s = (-shift_bins) as usize;
            self.smoothed.copy_within(..total - s, s);
            for i in 0..s { self.smoothed[i] = 0.0; }
        }
    }

    /// Generate simulated DDC spectrum into smoothed buffer directly.
    fn generate_test_ddc_into_smoothed(&mut self) {
        let total = self.smoothed.len();
        let span_hz = self.sample_rate_hz as f64;
        let center_hz = self.vfo_freq_hz as f64;
        let start_hz = center_hz - span_hz / 2.0;

        self.test_phase += 0.1;
        if self.test_phase > std::f32::consts::TAU {
            self.test_phase -= std::f32::consts::TAU;
        }
        let variation = self.test_phase.sin() * 5.0;

        // Simulated signals relative to VFO: offsets in Hz, bandwidth, peak power
        let signals: &[(f64, f64, f32)] = &[
            (0.0, 3_000.0, 160.0),        // Signal at VFO
            (10_000.0, 2_500.0, 120.0),   // +10 kHz
            (-10_000.0, 2_500.0, 100.0),  // -10 kHz
            (30_000.0, 3_000.0, 80.0),    // +30 kHz
            (-30_000.0, 3_000.0, 90.0),   // -30 kHz
            (50_000.0, 5_000.0, 60.0),    // +50 kHz
            (-65_000.0, 4_000.0, 70.0),   // -65 kHz
        ];

        for i in 0..total {
            let freq_hz = start_hz + (i as f64 / total as f64) * span_hz;
            let noise = 25.0 + pseudo_noise(i as u32, self.sequence) * 10.0;

            let mut signal = 0.0f32;
            for &(offset, bw, peak) in signals {
                let sig_center = center_hz + offset;
                let dist = (freq_hz - sig_center).abs();
                if dist < bw * 2.0 {
                    let sigma = bw / 2.0;
                    let g = (-0.5 * (dist / sigma).powi(2)).exp() as f32;
                    signal += g * (peak + variation);
                }
            }

            // EMA smoothing same as real data
            let val = (noise + signal).clamp(0.0, 65535.0);
            self.smoothed[i] = self.smoothed[i] * 0.6 + val * 0.4;
        }
    }

    /// Generate simulated wideband spectrum into smoothed buffer.
    fn generate_test_into_smoothed(&mut self) {
        let total = self.smoothed.len();
        let span_hz = 61_440_000.0f64;

        self.test_phase += 0.1;
        if self.test_phase > std::f32::consts::TAU {
            self.test_phase -= std::f32::consts::TAU;
        }
        let variation = self.test_phase.sin() * 5.0;

        for i in 0..total {
            let freq_hz = (i as f64 / total as f64) * span_hz;
            let noise = 25.0 + pseudo_noise(i as u32, self.sequence) * 10.0;
            let signal = ham_signal(freq_hz, variation);
            let val = (noise + signal).clamp(0.0, 65535.0);
            self.smoothed[i] = self.smoothed[i] * 0.6 + val * 0.4;
        }
    }
}

/// Simulated ham band signals at known frequencies
fn ham_signal(freq_hz: f64, variation: f32) -> f32 {
    // (center_freq_hz, bandwidth_hz, peak_power)
    let signals: &[(f64, f64, f32)] = &[
        // 160m band
        (1_850_000.0, 20_000.0, 60.0),
        // 80m band
        (3_700_000.0, 30_000.0, 80.0),
        // 40m band — strong
        (7_100_000.0, 40_000.0, 120.0),
        (7_050_000.0, 5_000.0, 150.0),
        // 30m band
        (10_120_000.0, 5_000.0, 70.0),
        // 20m band — strong
        (14_200_000.0, 50_000.0, 130.0),
        (14_070_000.0, 10_000.0, 100.0),
        // 17m band
        (18_100_000.0, 10_000.0, 60.0),
        // 15m band
        (21_200_000.0, 30_000.0, 90.0),
        // 12m band
        (24_930_000.0, 10_000.0, 50.0),
        // 10m band
        (28_500_000.0, 40_000.0, 70.0),
        // 6m band
        (50_200_000.0, 20_000.0, 40.0),
        // Broadcast interference
        (9_500_000.0, 100_000.0, 100.0),
        (11_700_000.0, 80_000.0, 80.0),
    ];

    let mut power = 0.0f32;
    for &(center, bw, peak) in signals {
        let dist = (freq_hz - center).abs();
        if dist < bw * 2.0 {
            // Gaussian-ish shape
            let sigma = bw / 2.0;
            let g = (-0.5 * (dist / sigma).powi(2)).exp() as f32;
            power += g * (peak + variation);
        }
    }
    power
}

/// Simple pseudo-random noise (deterministic per bin+frame for consistency)
fn pseudo_noise(bin: u32, frame: u16) -> f32 {
    let seed = bin.wrapping_mul(2654435761).wrapping_add(frame as u32 * 1013904223);
    let hash = seed ^ (seed >> 16);
    (hash & 0xFF) as f32 / 255.0
}

/// RX2 spectrum processor: independent DDC pipeline for the second receiver.
/// Same logic as SpectrumProcessor (DDC mode) with CTUN and HP center support.
pub struct Rx2SpectrumProcessor {
    enabled: bool,
    fps: u8,
    sequence: u16,
    last_frame: Instant,
    smoothed: Vec<f32>,
    has_real_data: bool,
    ddc_pipeline: Option<DdcFftPipeline>,
    vfo_freq_hz: u64,
    ddc_center_hz: u64,
    sample_rate_hz: u32,
    skip_fft_frames: u8,
    has_hp_center: bool,
    tci_mode: bool,
    custom_fft_size: Option<usize>,
    /// Calibration offset in dB (from TCI calibration_ex display_offset for RX2)
    cal_offset_db: f32,
}

impl Rx2SpectrumProcessor {
    pub fn new() -> Self {
        Self {
            enabled: false,
            fps: sdr_remote_core::DEFAULT_SPECTRUM_FPS,
            sequence: 0,
            last_frame: Instant::now(),
            smoothed: Vec::new(),
            has_real_data: false,
            ddc_pipeline: None,
            vfo_freq_hz: 7_050_000,
            ddc_center_hz: 7_050_000,
            sample_rate_hz: 1_536_000,
            skip_fft_frames: 0,
            has_hp_center: false,
            tci_mode: false,
            custom_fft_size: None,
            cal_offset_db: 0.0,
        }
    }

    /// Get current calibration offset (dB).
    pub fn cal_offset_db(&self) -> f32 {
        self.cal_offset_db
    }

    /// Set calibration offset (dB) to compensate for hardware attenuator/preamp.
    pub fn set_cal_offset_db(&mut self, offset: f32) {
        self.cal_offset_db = offset;
    }

    pub fn set_tci_mode(&mut self, enabled: bool) {
        self.tci_mode = enabled;
    }

    pub fn init_ddc_fft(&mut self, sample_rate_hz: u32) {
        let fft_size = self.custom_fft_size.unwrap_or_else(|| ddc_fft_size(sample_rate_hz));
        self.ddc_pipeline = Some(DdcFftPipeline::new(fft_size));
        self.sample_rate_hz = sample_rate_hz;
        self.smoothed = vec![0.0; fft_size];
        log::info!("RX2 DDC FFT pipeline initialized ({}pt, {}kHz span)",
            fft_size, sample_rate_hz / 1000);
    }

    pub fn update_sample_rate(&mut self, sample_rate_hz: u32) {
        if sample_rate_hz == self.sample_rate_hz {
            return;
        }
        let fft_size = self.custom_fft_size.unwrap_or_else(|| ddc_fft_size(sample_rate_hz));
        self.ddc_pipeline = Some(DdcFftPipeline::new(fft_size));
        self.sample_rate_hz = sample_rate_hz;
        self.smoothed = vec![0.0; fft_size];
        self.has_real_data = false;
        log::info!("RX2 DDC sample rate changed to {}kHz (FFT: {})", sample_rate_hz / 1000, fft_size);
    }

    pub fn set_fft_size(&mut self, size_k: u16) {
        let fft_size = match size_k {
            0 => {
                self.custom_fft_size = None;
                ddc_fft_size(self.sample_rate_hz)
            }
            _ => {
                let size = (size_k as usize * 1024).next_power_of_two().clamp(4096, 524288);
                self.custom_fft_size = Some(size);
                size
            }
        };
        let old_size = self.ddc_pipeline.as_ref().map(|p| p.window.len()).unwrap_or(0);
        if fft_size == old_size {
            return;
        }
        self.ddc_pipeline = Some(DdcFftPipeline::new(fft_size));
        self.smoothed = vec![0.0; fft_size];
        self.has_real_data = false;
        let hop = sdr_remote_core::ddc_hop_size(fft_size);
        let fft_per_sec = self.sample_rate_hz as f32 / hop as f32;
        log::info!("RX2 FFT size changed: {}K → {}K ({:.1} Hz/bin, {:.1} FFT/sec)",
            old_size / 1024, fft_size / 1024,
            self.sample_rate_hz as f64 / fft_size as f64,
            fft_per_sec);
    }

    /// Get current DDC FFT size (for accumulation buffer sizing)
    pub fn ddc_fft_size(&self) -> usize {
        self.ddc_pipeline.as_ref().map(|p| p.window.len()).unwrap_or(65536)
    }

    pub fn process_ddc_frame(&mut self, samples: &[(f32, f32)]) {
        if let Some(ref mut pipeline) = self.ddc_pipeline {
            let bins = pipeline.process(samples);
            // Skip stale FFT frames after frequency change (pipeline had old I/Q data)
            // Same pattern as RX1: always feed pipeline, only skip smoothing update
            if self.skip_fft_frames > 0 {
                self.skip_fft_frames -= 1;
                return;
            }
            // Same peak-hold + decay as RX1
            let decay = 0.6f32;
            let len = bins.len().min(self.smoothed.len());
            for i in 0..len {
                if bins[i] >= self.smoothed[i] {
                    self.smoothed[i] = bins[i];
                } else {
                    self.smoothed[i] = self.smoothed[i] * decay + bins[i] * (1.0 - decay);
                }
            }
            self.has_real_data = true;
        }
    }

    /// Compute total power in the receiver passband (same algorithm as RX1).
    pub fn compute_passband_power_dbm(&self, filter_low: i32, filter_high: i32) -> f32 {
        let n = self.smoothed.len();
        if n == 0 || !self.has_real_data || self.sample_rate_hz == 0 {
            return -140.0;
        }
        let hz_per_bin = self.sample_rate_hz as f64 / n as f64;
        let half = n / 2;
        let vfo_offset_hz = self.vfo_freq_hz as f64 - self.ddc_center_hz as f64;
        let bin_lo_f = half as f64 + (vfo_offset_hz + filter_low as f64) / hz_per_bin;
        let bin_hi_f = half as f64 + (vfo_offset_hz + filter_high as f64) / hz_per_bin;
        let first = (bin_lo_f.floor() as isize).max(0) as usize;
        let last = ((bin_hi_f.ceil() as isize) - 1).clamp(0, n as isize - 1) as usize;
        if first > last { return -140.0; }

        let mut sum_power = 0.0f64;
        for i in first..=last {
            let overlap = ((i + 1) as f64).min(bin_hi_f) - (i as f64).max(bin_lo_f);
            if overlap <= 0.0 { continue; }
            let db = self.smoothed[i] as f64 * 120.0 / 65535.0 - 150.0;
            sum_power += overlap * 10.0_f64.powf(db / 10.0);
        }
        if sum_power <= 0.0 { return -140.0; }
        // +3 dB correction for Hann window coherent gain / FFT normalization
        // + calibration offset to compensate for hardware attenuator/preamp
        (10.0 * sum_power.log10() + 3.0) as f32 + self.cal_offset_db
    }

    /// Raw passband power without calibration offset (for auto-calibration).
    pub fn compute_raw_passband_power_dbm(&self, filter_low: i32, filter_high: i32) -> f32 {
        self.compute_passband_power_dbm(filter_low, filter_high) - self.cal_offset_db
    }

    /// Set VFO-B frequency. Same CTUN logic as RX1:
    /// - CTUN off + no HP data: DDC center follows VFO
    /// - CTUN on: DDC center freezes, VFO moves within display
    /// - HP data: DDC center set from HP packets, VFO only for display
    pub fn set_vfo_freq(&mut self, freq_hz: u64, ctun: bool) {
        let old_vfo = self.vfo_freq_hz;
        self.vfo_freq_hz = freq_hz;

        if !self.has_hp_center {
            if !ctun {
                // CTUN off: DDC center = VFO (normal behavior)
                let old_center = self.ddc_center_hz;
                self.ddc_center_hz = freq_hz;

                if !self.tci_mode && old_center != 0 && old_center != freq_hz {
                    log::debug!("RX2 set_vfo_freq: vfo {} → {}, ddc_center {} → {} (no HP, CTUN off)",
                        old_vfo, freq_hz, old_center, freq_hz);
                    self.shift_smoothed(old_center, freq_hz);
                }
            }
            // CTUN on: freeze ddc_center_hz (don't follow VFO)
        } else if old_vfo != freq_hz {
            log::debug!("RX2 set_vfo_freq: vfo {} → {} (HP active, ddc_center stays {})",
                old_vfo, freq_hz, self.ddc_center_hz);
        }
    }

    /// Set DDC center from Protocol 2 HP packet (absolute, overrides VFO).
    pub fn set_ddc_center(&mut self, freq_hz: u64) {
        let old = self.ddc_center_hz;
        self.ddc_center_hz = freq_hz;
        self.has_hp_center = true;

        if old != 0 && old != freq_hz {
            log::debug!("RX2 set_ddc_center (HP): {} → {} (vfo={})", old, freq_hz, self.vfo_freq_hz);
            self.shift_smoothed(old, freq_hz);
        }
    }

    fn shift_smoothed(&mut self, old_center: u64, new_center: u64) {
        let total = self.smoothed.len();
        if total == 0 { return; }
        let delta = (new_center as i64 - old_center as i64).unsigned_abs();
        let hz_per_bin = self.sample_rate_hz as f64 / total as f64;
        let shift_bins = ((new_center as f64 - old_center as f64) / hz_per_bin).round() as isize;
        if delta > self.sample_rate_hz as u64 / 4 || shift_bins.unsigned_abs() >= total {
            self.smoothed.fill(0.0);
            self.has_real_data = false;
            return;
        }
        if shift_bins == 0 { return; }
        if !self.tci_mode {
            self.skip_fft_frames = 2;
        }
        if shift_bins > 0 {
            let s = shift_bins as usize;
            self.smoothed.copy_within(s.., 0);
            for i in (total - s)..total { self.smoothed[i] = 0.0; }
        } else {
            let s = (-shift_bins) as usize;
            self.smoothed.copy_within(..total - s, s);
            for i in 0..s { self.smoothed[i] = 0.0; }
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn set_fps(&mut self, fps: u8) {
        self.fps = fps.clamp(5, 30);
    }

    pub fn is_frame_ready(&mut self) -> bool {
        if !self.enabled || self.smoothed.is_empty() {
            return false;
        }
        let interval_ms = 1000 / self.fps as u64;
        if self.last_frame.elapsed().as_millis() < interval_ms as u128 {
            return false;
        }
        self.last_frame = Instant::now();
        self.sequence = self.sequence.wrapping_add(1);
        true
    }

    /// Extract view — same logic as SpectrumProcessor::extract_view
    pub fn extract_view(&self, zoom: f32, pan: f32, max_bins: usize) -> SpectrumPacket {
        let total = self.smoothed.len();
        if total == 0 {
            return SpectrumPacket {
                sequence: self.sequence,
                num_bins: 0,
                center_freq_hz: 0,
                span_hz: 0,
                ref_level: 0,
                db_per_unit: 1,
                bins: Vec::new(),
            };
        }

        let zoom = zoom.max(1.0);
        let visible = ((total as f64) / zoom as f64) as usize;
        let visible = visible.max(1);

        // VFO auto-center: offset pan so pan=0 centers on VFO,
        // regardless of CTUN (VFO may differ from DDC center).
        let vfo_offset = if self.vfo_freq_hz != 0 {
            let delta_hz = self.vfo_freq_hz as f64 - self.ddc_center_hz as f64;
            (delta_hz / self.sample_rate_hz as f64) as f32
        } else {
            0.0
        };

        let center = total as isize / 2 + ((pan + vfo_offset) * total as f32) as isize;
        let half = visible as isize / 2;
        let start = (center - half).max(0).min((total - visible) as isize) as usize;
        let end = (start + visible).min(total);

        let cal_shift = self.cal_offset_db * 65535.0 / 120.0;
        let bins: Vec<u16> = if visible <= max_bins {
            self.smoothed[start..end].iter()
                .map(|v| (v + cal_shift).clamp(0.0, 65535.0) as u16)
                .collect()
        } else {
            let stride = visible as f64 / max_bins as f64;
            (0..max_bins).map(|i| {
                let s = start + (i as f64 * stride) as usize;
                let e = (start + ((i as f64 + 1.0) * stride) as usize).min(end);
                let mut max_val = 0.0f32;
                for j in s..e {
                    if self.smoothed[j] > max_val {
                        max_val = self.smoothed[j];
                    }
                }
                (max_val + cal_shift).clamp(0.0, 65535.0) as u16
            }).collect()
        };

        let hz_per_bin = self.sample_rate_hz as f64 / total as f64;
        let bin_offset = (start + end) as f64 / 2.0 - total as f64 / 2.0;
        let center_hz = self.ddc_center_hz as f64 + bin_offset * hz_per_bin;
        let span_hz = visible as f64 * hz_per_bin;

        SpectrumPacket {
            sequence: self.sequence,
            num_bins: bins.len() as u16,
            center_freq_hz: center_hz.round() as u32,
            span_hz: span_hz as u32,
            ref_level: 0,
            db_per_unit: 1,
            bins,
        }
    }

    pub fn get_full_row(&self, max_bins: usize) -> SpectrumPacket {
        let total = self.smoothed.len();
        if total == 0 {
            return SpectrumPacket {
                sequence: self.sequence,
                num_bins: 0,
                center_freq_hz: 0,
                span_hz: 0,
                ref_level: 0,
                db_per_unit: 1,
                bins: Vec::new(),
            };
        }

        let cal_shift = self.cal_offset_db * 65535.0 / 120.0;
        let bins: Vec<u16> = if total <= max_bins {
            self.smoothed.iter().map(|v| (v + cal_shift).clamp(0.0, 65535.0) as u16).collect()
        } else {
            let stride = total as f64 / max_bins as f64;
            (0..max_bins).map(|i| {
                let s = (i as f64 * stride) as usize;
                let e = (((i + 1) as f64 * stride) as usize).min(total);
                let mut max_val = 0.0f32;
                for j in s..e {
                    if self.smoothed[j] > max_val {
                        max_val = self.smoothed[j];
                    }
                }
                (max_val + cal_shift).clamp(0.0, 65535.0) as u16
            }).collect()
        };

        SpectrumPacket {
            sequence: self.sequence,
            num_bins: bins.len() as u16,
            center_freq_hz: self.ddc_center_hz as u32,
            span_hz: self.sample_rate_hz,
            ref_level: 0,
            db_per_unit: 1,
            bins,
        }
    }
}

/// FFT pipeline: windowed FFT → power spectral density → decimated bins
pub struct FftPipeline {
    fft: Arc<dyn rustfft::Fft<f32>>,
    window: Vec<f32>,
    fft_buf: Vec<Complex<f32>>,
}

impl FftPipeline {
    pub fn new() -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(WIDEBAND_SAMPLES);

        // Blackman-Harris window for excellent sidelobe suppression
        let window: Vec<f32> = (0..WIDEBAND_SAMPLES)
            .map(|i| {
                let n = i as f32 / (WIDEBAND_SAMPLES - 1) as f32;
                let w = std::f32::consts::TAU * n;
                0.35875 - 0.48829 * (w).cos() + 0.14128 * (2.0 * w).cos()
                    - 0.01168 * (3.0 * w).cos()
            })
            .collect();

        let fft_buf = vec![Complex::new(0.0, 0.0); WIDEBAND_SAMPLES];

        Self { fft, window, fft_buf }
    }

    /// Process 16384 raw ADC samples → 1024 log-power bins (0-255 range)
    pub fn process(&mut self, samples: &[i16]) -> Vec<f32> {
        let n = samples.len().min(WIDEBAND_SAMPLES);

        // Apply window function
        for i in 0..n {
            self.fft_buf[i] = Complex::new(
                samples[i] as f32 / 32768.0 * self.window[i],
                0.0,
            );
        }
        // Zero-pad if needed
        for i in n..WIDEBAND_SAMPLES {
            self.fft_buf[i] = Complex::new(0.0, 0.0);
        }

        // In-place FFT
        self.fft.process(&mut self.fft_buf);

        // Only use first half (positive frequencies, 0 to Nyquist)
        let half = WIDEBAND_SAMPLES / 2; // 8192 bins

        // Compute power spectral density and decimate to SPECTRUM_BINS
        let bins_per_output = half / SPECTRUM_BINS; // 8192 / 1024 = 8
        let mut output = vec![0.0f32; SPECTRUM_BINS];

        for i in 0..SPECTRUM_BINS {
            let start = i * bins_per_output;
            let end = start + bins_per_output;

            // Max power in this group of 8 bins
            let mut max_power = 0.0f32;
            for j in start..end {
                let c = self.fft_buf[j];
                let power = c.re * c.re + c.im * c.im;
                max_power = max_power.max(power);
            }

            // Convert to dB scale and map to 0-255
            // noise floor around -120 dB, strong signals around -20 dB
            let db = if max_power > 1e-20 {
                10.0 * max_power.log10()
            } else {
                -200.0
            };

            // Map dB range to 0-255: -120 dB → 0, -20 dB → 255
            let normalized = ((db + 120.0) / 100.0 * 65535.0).clamp(0.0, 65535.0);
            output[i] = normalized;
        }

        output
    }
}

/// DDC FFT pipeline: complex I/Q FFT → FFT-shift → power → N bins (parametric size)
pub struct DdcFftPipeline {
    fft: Arc<dyn rustfft::Fft<f32>>,
    window: Vec<f32>,
    fft_buf: Vec<Complex<f32>>,
}

impl DdcFftPipeline {
    pub fn new(fft_size: usize) -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(fft_size);

        // Hann window (matches Thetis — sharper peaks than Blackman-Harris)
        let window: Vec<f32> = (0..fft_size)
            .map(|i| {
                let n = i as f32 / (fft_size - 1) as f32;
                0.5 * (1.0 - (std::f32::consts::TAU * n).cos())
            })
            .collect();

        let fft_buf = vec![Complex::new(0.0, 0.0); fft_size];

        log::info!("DDC FFT pipeline: {}pt window + buffer allocated (~{:.1} MB)",
            fft_size,
            (fft_size * (4 + 8)) as f64 / 1_048_576.0);

        Self { fft, window, fft_buf }
    }

    /// Process DDC I/Q samples → N log-power bins (0-255 range as f32).
    /// Complex FFT with FFT-shift so DC (VFO) is in the center.
    pub fn process(&mut self, samples: &[(f32, f32)]) -> Vec<f32> {
        let fft_size = self.window.len();
        let n = samples.len().min(fft_size);

        // Apply window to complex I/Q data
        for i in 0..n {
            let (ii, qq) = samples[i];
            self.fft_buf[i] = Complex::new(
                ii * self.window[i],
                qq * self.window[i],
            );
        }
        for i in n..fft_size {
            self.fft_buf[i] = Complex::new(0.0, 0.0);
        }

        // In-place complex FFT
        self.fft.process(&mut self.fft_buf);

        // FFT-shift: rotate so DC is in the center
        // Before shift: bin 0 = DC, bin N/2 = -fs/2
        // After shift: bin 0 = -fs/2, bin N/2 = DC, bin N-1 = +fs/2
        let half = fft_size / 2;
        for i in 0..half {
            self.fft_buf.swap(i, i + half);
        }

        // Output all bins for maximum zoom detail.
        let mut output = vec![0.0f32; fft_size];

        // rustfft is unnormalized: output scaled by N. Normalize power by N².
        let norm = 1.0 / (fft_size as f32 * fft_size as f32);

        for i in 0..fft_size {
            let c = self.fft_buf[i];
            let power = (c.re * c.re + c.im * c.im) * norm;

            let db = if power > 1e-30 {
                10.0 * power.log10()
            } else {
                -200.0
            };

            // Map dB range to 0-65535: -150 dB → 0, -30 dB → 65535
            output[i] = ((db + 150.0) / 120.0 * 65535.0).clamp(0.0, 65535.0);
        }

        output
    }
}
