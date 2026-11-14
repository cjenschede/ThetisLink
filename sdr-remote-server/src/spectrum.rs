// SPDX-License-Identifier: GPL-2.0-or-later

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Instant;

use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

use sdr_remote_core::protocol::SpectrumPacket;
use sdr_remote_core::{
    ddc_fft_size, SPECTRUM_BINS, SPECTRUM_PACK_FLOOR_DB, SPECTRUM_PACK_MAX, SPECTRUM_PACK_RANGE_DB,
};

/// How far the passband may stand above the spectrum's own floor and still
/// count as empty. The calibration against the S-meter only learns from an
/// empty passband: with a signal in it the peak-held bins rise at once and the
/// S-meter far less, and the whole spectrum sank with every word spoken there.
pub const CAL_QUIET_EXCESS_DB: f32 = 6.0;

/// How long the passband has to stay empty after a signal before the
/// calibration learns again. The S-meter is an average and still stands high
/// in the pauses between words and for a while after the last one; learning
/// there lifted the whole spectrum during an over and dropped it after.
pub const CAL_SIGNAL_HOLDOFF: std::time::Duration = std::time::Duration::from_secs(5);

/// A smoothed bin as linear power (mW): the packed scale back to dBm.
fn bin_power(packed: f32) -> f64 {
    let db = packed as f64 * SPECTRUM_PACK_RANGE_DB as f64 / SPECTRUM_PACK_MAX as f64 + SPECTRUM_PACK_FLOOR_DB as f64;
    10.0_f64.powf(db / 10.0)
}

/// The passband's power from the smoothed bins: the linear power of every bin
/// it overlaps summed, an edge bin by the part it overlaps, together with the
/// width that sum covers, in bins. Both the passband power and its height over
/// the floor are taken from this one sum. `None` without bins or when the
/// passband lies outside them.
fn passband_power(
    smoothed: &[f32],
    sample_rate_hz: u32,
    vfo_offset_hz: f64,
    filter_low: i32,
    filter_high: i32,
) -> Option<(f64, f64)> {
    let n = smoothed.len();
    if n == 0 || sample_rate_hz == 0 {
        return None;
    }
    let hz_per_bin = sample_rate_hz as f64 / n as f64;
    let half = (n / 2) as f64;
    let bin_lo_f = half + (vfo_offset_hz + filter_low as f64) / hz_per_bin;
    let bin_hi_f = half + (vfo_offset_hz + filter_high as f64) / hz_per_bin;
    let first = (bin_lo_f.floor() as isize).max(0) as usize;
    let last = ((bin_hi_f.ceil() as isize) - 1).clamp(0, n as isize - 1) as usize;
    if first > last {
        return None;
    }
    let mut power = 0.0f64;
    let mut width = 0.0f64;
    for (i, &v) in smoothed.iter().enumerate().take(last + 1).skip(first) {
        let overlap = ((i + 1) as f64).min(bin_hi_f) - (i as f64).max(bin_lo_f);
        if overlap <= 0.0 {
            continue;
        }
        power += overlap * bin_power(v);
        width += overlap;
    }
    if power <= 0.0 {
        return None;
    }
    Some((power, width))
}

/// The passband's power in dBm, without the calibration offset: the
/// passband sum plus 3 dB for the Hann window's coherent gain and the FFT
/// normalisation. `None` without real data, without bins, or when the
/// passband lies outside them.
fn raw_passband_dbm(
    smoothed: &[f32],
    has_real_data: bool,
    sample_rate_hz: u32,
    vfo_offset_hz: f64,
    filter_low: i32,
    filter_high: i32,
) -> Option<f32> {
    if !has_real_data {
        return None;
    }
    let (power, _) = passband_power(smoothed, sample_rate_hz, vfo_offset_hz, filter_low, filter_high)?;
    Some((10.0 * power.log10() + 3.0) as f32)
}

/// The passband over the floor of the whole spectrum, in dB: the passband's
/// power against the power the same width would have at the median of all
/// bins. Measured as power, like the calibration step itself, so a carrier
/// a few bins wide counts as fully as speech across the whole passband.
/// `None` without bins or when the passband lies outside them.
fn passband_excess_db(
    smoothed: &[f32],
    sample_rate_hz: u32,
    vfo_offset_hz: f64,
    filter_low: i32,
    filter_high: i32,
) -> Option<f32> {
    let (power, width) = passband_power(smoothed, sample_rate_hz, vfo_offset_hz, filter_low, filter_high)?;
    let mut all = smoothed.to_vec();
    let mid = all.len() / 2;
    let (_, median, _) = all.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
    let floor = width * bin_power(*median);
    Some((10.0 * (power / floor).log10()) as f32)
}

/// What one tick of the calibration against the S-meter did.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CalStep {
    /// The passband was empty: the offset moved a tenth towards the S-meter.
    Learned,
    /// A signal stood in the passband: the offset was held.
    HeldForSignal,
    /// The passband was empty, but a signal stood in it less than
    /// `CAL_SIGNAL_HOLDOFF` ago: the offset was held.
    WaitingAfterSignal,
    /// No usable S-meter or passband reading.
    NoReading,
}

/// One tick of the calibration of a receiver's spectrum against the S-meter
/// the radio software reports, for a receiver without a step attenuator
/// reading. RX1 and RX2 both ask this. The offset is a property of the
/// receive chain, so it is learned only while the passband holds nothing but
/// noise (`CAL_QUIET_EXCESS_DB`) and has done so for `CAL_SIGNAL_HOLDOFF`;
/// `last_signal` is when this receiver last had a signal in its passband.
/// `raw_dbm` is `None` when the passband gave no reading.
pub fn calibrate_against_smeter(
    static_cal: f32,
    current: f32,
    smeter_dbm: f32,
    raw_dbm: Option<f32>,
    excess_db: Option<f32>,
    last_signal: &mut Option<Instant>,
    now: Instant,
) -> (f32, CalStep) {
    let held = if current.abs() < 0.01 { static_cal } else { current };
    let Some(raw_dbm) = raw_dbm.filter(|raw| *raw > -130.0 && smeter_dbm > -130.0) else {
        return (held, CalStep::NoReading);
    };
    match excess_db {
        Some(excess) if excess < CAL_QUIET_EXCESS_DB => {
            if last_signal.is_some_and(|at| now.saturating_duration_since(at) < CAL_SIGNAL_HOLDOFF) {
                return (held, CalStep::WaitingAfterSignal);
            }
            let dynamic = smeter_dbm - raw_dbm - static_cal;
            let cur_dynamic = current - static_cal;
            (static_cal + cur_dynamic * 0.9 + dynamic * 0.1, CalStep::Learned)
        }
        _ => {
            *last_signal = Some(now);
            (held, CalStep::HeldForSignal)
        }
    }
}

/// A tick while transmitting: the calibration does not run, and the
/// receivers hear our own signal. The hold-off after a signal starts again at
/// every such tick, so after the release the calibration waits
/// `CAL_SIGNAL_HOLDOFF` as it does after a signal received.
pub fn hold_calibration_while_transmitting(last_signal: &mut Option<Instant>, now: Instant) {
    *last_signal = Some(now);
}

/// Ten seconds of the calibration against the S-meter for one receiver, as
/// one log line: how often it learned, how often a signal held it, how far the
/// passband stood over the floor, and where the offset went.
pub struct CalWatch {
    name: &'static str,
    started: Option<Instant>,
    learned: u32,
    held: u32,
    waiting: u32,
    no_reading: u32,
    on_the_air: u32,
    excess: Option<(f32, f32)>,
    offset: Option<(f32, f32)>,
}

impl CalWatch {
    pub fn new(name: &'static str) -> Self {
        Self { name, started: None, learned: 0, held: 0, waiting: 0, no_reading: 0, on_the_air: 0, excess: None, offset: None }
    }

    /// Count one tick on the air: the calibration did not run, and the
    /// hold-off started again. Shown in the next line.
    pub fn note_on_the_air(&mut self) {
        self.on_the_air += 1;
    }

    /// Count one tick; every ten seconds the line to log.
    #[allow(clippy::too_many_arguments)]
    pub fn note(
        &mut self,
        step: CalStep,
        excess_db: Option<f32>,
        smeter_dbm: f32,
        raw_dbm: Option<f32>,
        offset_db: f32,
        static_cal: f32,
        now: Instant,
    ) -> Option<String> {
        let started = *self.started.get_or_insert(now);
        match step {
            CalStep::Learned => self.learned += 1,
            CalStep::HeldForSignal => self.held += 1,
            CalStep::WaitingAfterSignal => self.waiting += 1,
            CalStep::NoReading => self.no_reading += 1,
        }
        let widen = |range: Option<(f32, f32)>, v: f32| Some(range.map_or((v, v), |(lo, hi)| (lo.min(v), hi.max(v))));
        if let Some(e) = excess_db {
            self.excess = widen(self.excess, e);
        }
        self.offset = widen(self.offset, offset_db);
        if now.duration_since(started) < std::time::Duration::from_secs(10) {
            return None;
        }
        let range = |r: Option<(f32, f32)>| r.map_or("-".to_string(), |(lo, hi)| format!("{lo:.1}..{hi:.1}"));
        let line = format!(
            concat!(
                "Spectrum {} calibration against the S-meter (10 s): learned {}, held for a signal {}, ",
                "waiting after a signal {}, no reading {}, on the air {}; passband over the floor {} dB (held from {:.0}); ",
                "offset {} dB (static {static_cal:.1}); now S-meter {smeter_dbm:.1} dBm, passband {raw} dBm raw"
            ),
            self.name,
            self.learned,
            self.held,
            self.waiting,
            self.no_reading,
            self.on_the_air,
            range(self.excess),
            CAL_QUIET_EXCESS_DB,
            range(self.offset),
            static_cal = static_cal,
            smeter_dbm = smeter_dbm,
            raw = raw_dbm.map_or("-".to_string(), |r| format!("{r:.1}")),
        );
        *self = Self::new(self.name);
        Some(line)
    }
}

/// The calibration offset in dB as a shift on the packed bin scale.
fn cal_shift(cal_offset_db: f32) -> f32 {
    cal_offset_db * SPECTRUM_PACK_MAX / SPECTRUM_PACK_RANGE_DB
}

/// A smoothed bin plus the calibration shift, clamped onto the packed scale.
fn pack_shifted(v: f32, cal_shift: f32) -> u16 {
    (v + cal_shift).clamp(0.0, SPECTRUM_PACK_MAX) as u16
}

/// Peak-hold decay coefficient shared by RX1 and RX2 spectrum processing.
/// New peaks are captured instantly; values below the current peak decay
/// exponentially toward the incoming bin. At ~11.7 FFT/sec, 0.35 gives a
/// ~50 ms effective time constant (snappier spectrum). RX1 and RX2 MUST use
/// the same value, they previously drifted (RX2 was 0.6, sluggish).
const PEAK_DECAY: f32 = 0.35;

/// A measurement (build 172): the RX1 spectrum in the first moments after the
/// transmitter goes on or off. The spectrum was seen to shoot up for a
/// moment when the PTT goes down, on both clients and not in Thetis itself;
/// the hypothesis is that the smoothing below holds every rise at once
/// (`update_from_fft`) and so holds the own signal in the receive data of the
/// first moments. One line per change, so the field log can tell.
#[derive(Debug, Default)]
pub struct TxProbe {
    tx: bool,
    since: Option<Instant>,
    frames: Vec<(u64, SpectrumLevels)>,
}

/// One frame's levels for `TxProbe`: the highest and the mean bin in dB
/// (calibrated, as the clients draw it), and how many bins sat on the pack
/// scale's ceiling - a peak there says "at least", not "this much" (in pack
/// units the line could not tell a clipped frame; build 174).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpectrumLevels {
    pub peak_db: f32,
    pub mean_db: f32,
    pub at_ceiling: usize,
}

/// How long after a change the probe looks.
pub const TX_PROBE_MS: u64 = 1500;

/// `SpectrumLevels` of a buffer in pack units.
fn levels_of(packed: &[f32], cal_offset_db: f32) -> SpectrumLevels {
    if packed.is_empty() {
        return SpectrumLevels { peak_db: SPECTRUM_PACK_FLOOR_DB, mean_db: SPECTRUM_PACK_FLOOR_DB, at_ceiling: 0 };
    }
    let to_db = |v: f32| v / SPECTRUM_PACK_MAX * SPECTRUM_PACK_RANGE_DB + SPECTRUM_PACK_FLOOR_DB + cal_offset_db;
    let peak = packed.iter().cloned().fold(f32::MIN, f32::max);
    let mean = packed.iter().sum::<f32>() / packed.len() as f32;
    let at_ceiling = packed.iter().filter(|&&v| v >= SPECTRUM_PACK_MAX).count();
    SpectrumLevels { peak_db: to_db(peak), mean_db: to_db(mean), at_ceiling }
}

impl TxProbe {
    /// Whether the next frame is wanted: a change of the transmitter, or a
    /// window open - so the levels are only computed then
    /// (they were computed on every frame, under the spectrum's lock).
    pub fn wants(&self, tx: bool) -> bool {
        tx != self.tx || self.since.is_some()
    }

    /// One spectrum frame: whether the transmitter is on, and the frame's
    /// levels. The line of a window once it is over - also when a change ends
    /// it early, and then this frame opens the next window (it
    /// was lost).
    pub fn frame(&mut self, tx: bool, levels: SpectrumLevels, now: Instant) -> Option<String> {
        let mut line = None;
        if tx != self.tx {
            line = self.finish();
            self.tx = tx;
            self.since = Some(now);
        }
        let since = self.since?;
        let ms = now.saturating_duration_since(since).as_millis() as u64;
        if ms > TX_PROBE_MS {
            return line.or_else(|| self.finish());
        }
        self.frames.push((ms, levels));
        line
    }

    fn finish(&mut self) -> Option<String> {
        self.since.take()?;
        let frames: Vec<String> = self
            .frames
            .drain(..)
            .map(|(ms, l)| format!("{ms}:{:.0}/{:.0}/{}", l.peak_db, l.mean_db, l.at_ceiling))
            .collect();
        Some(format!("Spectrum around TX {}: {}", if self.tx { "on" } else { "off" }, frames.join(" ")))
    }
}

/// A jump of the DDC centre - a band change - and what it does to the FFT
/// window (build 236). The window is `ddc_fft_size` samples, ~0.34 s at every
/// DDC rate, and slides by an eighth: after a jump it still held the old band
/// for most of that time, and the first frames showed the old band under the
/// new centre. The loop that fills the window drops it on a new jump, and a
/// frame built from a window of before the jump is not taken.
#[derive(Debug, Default)]
pub struct BandJump {
    generation: u32,
    last: Option<JumpSeen>,
    waiting_since: Option<Instant>,
}

/// One jump, as the loop that fills the window sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JumpSeen {
    pub generation: u32,
    pub from_hz: u64,
    pub to_hz: u64,
}

impl BandJump {
    fn jumped(&mut self, from_hz: u64, to_hz: u64) {
        self.generation = self.generation.wrapping_add(1);
        self.last = Some(JumpSeen { generation: self.generation, from_hz, to_hz });
        self.waiting_since = Some(Instant::now());
    }

    /// The jump a window of generation `seen` has not dropped yet.
    pub fn since(&self, seen: u32) -> Option<JumpSeen> {
        self.last.filter(|j| j.generation != seen)
    }

    /// A frame from a window of generation `window` is of the current band.
    fn takes(&self, window: u32) -> bool {
        window == self.generation
    }

    /// The first frame of the new band: how long the gap was, once per jump.
    fn first_frame(&mut self) -> Option<u128> {
        self.waiting_since.take().map(|t| t.elapsed().as_millis())
    }
}

/// Spectrum processor: generates test data or processes real FFT data.
/// Rate-limited to configured FPS.
/// Supports server-side zoom/pan extraction from 262k-bin smoothed buffer.
pub struct SpectrumProcessor {
    enabled: bool,
    fps: u8,
    sequence: u16,
    last_frame: Instant,
    /// Smoothed spectrum bins for EMA filtering (one per FFT bin in DDC mode)
    smoothed: Vec<f32>,
    /// Whether we have real HPSDR data (false = use test generator)
    has_real_data: bool,
    /// Whether real data ever came: then a gap sends nothing (build 239).
    had_real_data: bool,
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
    /// Whether DDC mode is active (false = no DDC yet: the test spectrum)
    ddc_mode: bool,
    /// Whether DDC center is being set from HP packets (true = use HP data, false = use CAT/heuristic)
    has_hp_center: bool,
    /// The centre came from a `dds` or from the VFO with CTUN off, not from the
    /// starting value (build 232): only a measured centre goes to the clients.
    centre_measured: bool,
    /// Skip N FFT frames after frequency change (stale pipeline data)
    skip_fft_frames: u8,
    /// TCI mode: IQ data is always at correct frequency, no shift/skip needed
    tci_mode: bool,
    /// Custom FFT size override (None = auto from sample rate)
    custom_fft_size: Option<usize>,
    /// Calibration offset in dB (from TCI calibration_ex display_offset)
    /// Added to spectrum bins to compensate for hardware attenuator/preamp
    cal_offset_db: f32,
    /// Band jumps and the FFT window (build 236).
    band_jump: BandJump,
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
            had_real_data: false,
            ddc_pipeline: None,
            test_phase: 0.0,
            vfo_freq_hz: 14_200_000, // Default 20m band
            ddc_center_hz: 14_200_000,
            sample_rate_hz: 1_536_000,
            ddc_mode: false,
            has_hp_center: false,
            centre_measured: false,
            skip_fft_frames: 0,
            tci_mode: false,
            custom_fft_size: None,
            cal_offset_db: 0.0,
            band_jump: BandJump::default(),
        }
    }

    /// The jump the window of generation `seen` has not dropped (`BandJump`).
    pub fn band_jump_since(&self, seen: u32) -> Option<JumpSeen> {
        self.band_jump.since(seen)
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
        log::info!("DDC sample rate changed: {}kHz -> {}kHz (FFT: {} -> {})",
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
                let size = (size_k as usize * 1024).next_power_of_two().clamp(4096, sdr_remote_core::MAX_FFT_SIZE_K as usize * 1024);
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
        log::info!("FFT size changed: {}K -> {}K ({:.1} Hz/bin, {:.1} FFT/sec)",
            old_size / 1024, fft_size / 1024,
            self.sample_rate_hz as f64 / fft_size as f64,
            fft_per_sec);
    }

    /// Get current FFT size
    pub fn current_fft_size(&self) -> usize {
        self.ddc_pipeline.as_ref().map(|p| p.window.len()).unwrap_or(0)
    }

    /// Get current DDC FFT size (for accumulation buffer sizing)
    pub fn ddc_fft_size(&self) -> usize {
        self.ddc_pipeline.as_ref().map(|p| p.window.len()).unwrap_or(65536)
    }

    /// Current VFO frequency (Hz). Used by VRX to know where the user
    /// thinks they are tuned.
    pub fn vfo_freq_hz(&self) -> u64 {
        self.vfo_freq_hz
    }

    /// Current DDC center frequency (Hz). With CTUN+auto-recenter
    /// extension this is updated independently of VFO; the VRX
    /// channelizer needs the difference to know how to bin-select.
    pub fn ddc_center_hz(&self) -> u64 {
        self.ddc_center_hz
    }

    /// The centre is a measurement, not the starting value (build 232).
    pub fn ddc_center_measured(&self) -> bool {
        self.centre_measured
    }

    /// Process DDC I/Q samples from HPSDR DDC capture. `window` is the
    /// generation of the window they came from (`BandJump`).
    pub fn process_ddc_frame(&mut self, samples: &[(f32, f32)], window: u32) {
        if !self.band_jump.takes(window) {
            return;
        }
        if let Some(ref mut pipeline) = self.ddc_pipeline {
            let bins = pipeline.process(samples);
            self.update_from_fft(&bins);
            self.has_real_data = true;
            self.had_real_data = true;
            if let Some(ms) = self.band_jump.first_frame() {
                log::info!("Spectrum RX1: first frame of the new band after {} ms", ms);
            }
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
                self.centre_measured = true;

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
        self.centre_measured = true;

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
        // A gap after real data - a band jump, another DDC rate or FFT size -
        // sends nothing until the first real frame (build 239). The frame it
        // sent lay on the floor of the scale: the clients drew an empty
        // picture and a dark waterfall row, and the auto-ref settled on it.
        // The test spectrum stays for a server that never had a
        // radio.
        if self.had_real_data && !self.has_real_data {
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

        let cal_shift = cal_shift(self.cal_offset_db);

        let bins: Vec<u16> = if visible <= max_bins {
            // Native resolution: send bins directly
            self.smoothed[start..end].iter()
                .map(|v| pack_shifted(*v, cal_shift))
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
                pack_shifted(max_val, cal_shift)
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
            // No DDC yet: the test spectrum over 0-61.44 MHz
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

        let cal_shift = cal_shift(self.cal_offset_db);
        let bins: Vec<u16> = if total <= max_bins {
            self.smoothed.iter().map(|v| pack_shifted(*v, cal_shift)).collect()
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
                pack_shifted(max_val, cal_shift)
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

    /// Extract a view centered on an arbitrary absolute frequency
    /// (instead of zoom/pan-around-VFO). Used for per-VRX high-res
    /// spectrum where the VRX freq is not the same as RX1's VFO.
    /// Returns up to `max_bins` bins covering [center-span/2 ..
    /// center+span/2]. Clamps to available DDC range.
    pub fn extract_view_at(
        &self,
        center_hz: u64,
        span_hz: u32,
        max_bins: usize,
    ) -> SpectrumPacket {
        let total = self.smoothed.len();
        if total == 0 || !self.ddc_mode || span_hz == 0 {
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
        let hz_per_bin = self.sample_rate_hz as f64 / total as f64;
        let ddc_lo = self.ddc_center_hz as f64 - (self.sample_rate_hz as f64 / 2.0);
        let view_lo = (center_hz as f64) - (span_hz as f64 / 2.0);
        let view_hi = (center_hz as f64) + (span_hz as f64 / 2.0);
        let bin_lo = ((view_lo - ddc_lo) / hz_per_bin).floor() as isize;
        let bin_hi = ((view_hi - ddc_lo) / hz_per_bin).ceil() as isize;
        // Clamp BOTH bounds to [0, total]. A view entirely BELOW the DDC
        // yields a negative bin_hi; `negative as usize` then wraps to a
        // gigantic index -> out-of-bounds panic in the bin loop below.
        // With clamp it becomes 0, and the `end <= start` guard cleanly
        // returns an empty spectrum instead of crashing the server.
        let start = bin_lo.clamp(0, total as isize) as usize;
        let end = bin_hi.clamp(0, total as isize) as usize;
        if end <= start {
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
        let cal_shift = cal_shift(self.cal_offset_db);
        // Render the REQUESTED window, bin for bin. Positions that fall outside
        // the DDC data carry no signal and are emitted as 0 (the packing floor),
        // so the client draws the window at the span it asked for and can show
        // where the band ends - instead of receiving a narrower, re-centred view,
        // which made the display rescale as you approached the band edge.
        let requested_bins = (span_hz as f64 / hz_per_bin).ceil().max(1.0);
        let out_n = max_bins.min(requested_bins as usize).max(1);
        let stride = requested_bins / out_n as f64;
        let first = (view_lo - ddc_lo) / hz_per_bin;
        let bins: Vec<u16> = (0..out_n)
            .map(|i| {
                let s = first + i as f64 * stride;
                let e = first + (i as f64 + 1.0) * stride;
                let js = s.floor() as isize;
                let je = (e.ceil() as isize).max(js + 1);
                let mut max_val = 0.0f32;
                let mut have = false;
                for j in js.max(0)..je.min(total as isize) {
                    have = true;
                    let v = self.smoothed[j as usize];
                    if v > max_val {
                        max_val = v;
                    }
                }
                if !have {
                    return 0;
                }
                pack_shifted(max_val, cal_shift)
            })
            .collect();
        SpectrumPacket {
            sequence: self.sequence,
            num_bins: bins.len() as u16,
            center_freq_hz: center_hz as u32,
            span_hz,
            ref_level: 0,
            db_per_unit: 1,
            bins,
        }
    }

    /// Take the bins of a DDC FFT into spectrum data.
    /// Peak-hold display: new peaks are captured instantly; values below the
    /// current peak decay exponentially toward the incoming bin value.
    /// Decay coefficient `PEAK_DECAY` controls the time constant.
    pub fn update_from_fft(&mut self, bins: &[f32]) {
        // Skip stale FFT frames after frequency change (pipeline had old I/Q data)
        if self.skip_fft_frames > 0 {
            self.skip_fft_frames -= 1;
            return;
        }
        // Decay factor: at ~11.7 FFT/sec, lower decay = faster response.
        let decay = PEAK_DECAY;
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

    /// The levels of what is sent, for `TxProbe`, in calibrated dB.
    pub fn levels(&self) -> SpectrumLevels {
        levels_of(&self.smoothed, self.cal_offset_db)
    }

    /// The passband's power in dBm without the calibration offset, for the
    /// calibration (`raw_passband_dbm`). filter_low/high are in Hz relative
    /// to the VFO (e.g. 50..2850 for USB, -2850..-50 for LSB). `None` when
    /// there is no reading.
    pub fn compute_raw_passband_power_dbm(&self, filter_low: i32, filter_high: i32) -> Option<f32> {
        let vfo_offset_hz = self.vfo_freq_hz as f64 - self.ddc_center_hz as f64;
        raw_passband_dbm(&self.smoothed, self.has_real_data, self.sample_rate_hz, vfo_offset_hz, filter_low, filter_high)
    }

    /// The passband over the floor of the whole spectrum, in dB (`passband_excess_db`).
    pub fn passband_excess_db(&self, filter_low: i32, filter_high: i32) -> Option<f32> {
        if !self.has_real_data {
            return None;
        }
        let vfo_offset_hz = self.vfo_freq_hz as f64 - self.ddc_center_hz as f64;
        passband_excess_db(&self.smoothed, self.sample_rate_hz, vfo_offset_hz, filter_low, filter_high)
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
            // Band change: reset, and the FFT window goes too (build 236)
            self.smoothed.fill(0.0);
            self.has_real_data = false;
            self.band_jump.jumped(old_center, new_center);
            return;
        }

        if shift_bins == 0 { return; }

        // Skip next FFT frames (pipeline still has old-frequency I/Q data)
        // TCI mode: IQ data is immediately at new freq, no skip needed
        if !self.tci_mode {
            self.skip_fft_frames = 2;
        }

        // Shift bins: freq went up -> data shifts left (positive shift_bins)
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
            let val = (noise + signal).clamp(0.0, SPECTRUM_PACK_MAX);
            self.smoothed[i] = self.smoothed[i] * 0.6 + val * 0.4;
        }
    }

    /// Generate the test spectrum (no DDC yet) into smoothed buffer.
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
            let val = (noise + signal).clamp(0.0, SPECTRUM_PACK_MAX);
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
        // 40m band - strong
        (7_100_000.0, 40_000.0, 120.0),
        (7_050_000.0, 5_000.0, 150.0),
        // 30m band
        (10_120_000.0, 5_000.0, 70.0),
        // 20m band - strong
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
    /// Whether real data ever came: then a gap sends nothing (build 239).
    had_real_data: bool,
    ddc_pipeline: Option<DdcFftPipeline>,
    vfo_freq_hz: u64,
    ddc_center_hz: u64,
    sample_rate_hz: u32,
    skip_fft_frames: u8,
    has_hp_center: bool,
    /// The centre came from a `dds` or from the VFO with CTUN off, not from the
    /// starting value (build 232): only a measured centre goes to the clients.
    centre_measured: bool,
    tci_mode: bool,
    custom_fft_size: Option<usize>,
    /// Calibration offset in dB (from TCI calibration_ex display_offset for RX2)
    cal_offset_db: f32,
    /// Band jumps and the FFT window (build 236).
    band_jump: BandJump,
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
            had_real_data: false,
            ddc_pipeline: None,
            vfo_freq_hz: 7_050_000,
            ddc_center_hz: 7_050_000,
            sample_rate_hz: 1_536_000,
            skip_fft_frames: 0,
            has_hp_center: false,
            centre_measured: false,
            tci_mode: false,
            custom_fft_size: None,
            cal_offset_db: 0.0,
            band_jump: BandJump::default(),
        }
    }

    /// The jump the window of generation `seen` has not dropped (`BandJump`).
    pub fn band_jump_since(&self, seen: u32) -> Option<JumpSeen> {
        self.band_jump.since(seen)
    }

    /// Get current calibration offset (dB).
    pub fn cal_offset_db(&self) -> f32 {
        self.cal_offset_db
    }

    /// Set calibration offset (dB) to compensate for hardware attenuator/preamp.
    pub fn set_cal_offset_db(&mut self, offset: f32) {
        self.cal_offset_db = offset;
    }

    /// Current RX2 VFO (VFO-B) frequency in Hz. Used by VRX2 channelizer.
    pub fn vfo_freq_hz(&self) -> u64 {
        self.vfo_freq_hz
    }

    /// Current RX2 DDC center frequency in Hz. Used by VRX2 to compute
    /// bin-offset against the IQ stream's centre.
    pub fn ddc_center_hz(&self) -> u64 {
        self.ddc_center_hz
    }

    /// The centre is a measurement, not the starting value (build 232).
    pub fn ddc_center_measured(&self) -> bool {
        self.centre_measured
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
                let size = (size_k as usize * 1024).next_power_of_two().clamp(4096, sdr_remote_core::MAX_FFT_SIZE_K as usize * 1024);
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
        log::info!("RX2 FFT size changed: {}K -> {}K ({:.1} Hz/bin, {:.1} FFT/sec)",
            old_size / 1024, fft_size / 1024,
            self.sample_rate_hz as f64 / fft_size as f64,
            fft_per_sec);
    }

    /// Get current DDC FFT size (for accumulation buffer sizing)
    pub fn ddc_fft_size(&self) -> usize {
        self.ddc_pipeline.as_ref().map(|p| p.window.len()).unwrap_or(65536)
    }

    /// `window`: the generation of the window the samples came from
    /// (`BandJump`, build 236).
    pub fn process_ddc_frame(&mut self, samples: &[(f32, f32)], window: u32) {
        if !self.band_jump.takes(window) {
            return;
        }
        if let Some(ref mut pipeline) = self.ddc_pipeline {
            let bins = pipeline.process(samples);
            // Skip stale FFT frames after frequency change (pipeline had old I/Q data)
            // Same pattern as RX1: always feed pipeline, only skip smoothing update
            if self.skip_fft_frames > 0 {
                self.skip_fft_frames -= 1;
                return;
            }
            // Same peak-hold + decay as RX1 (shared constant, no drift)
            let decay = PEAK_DECAY;
            let len = bins.len().min(self.smoothed.len());
            for i in 0..len {
                if bins[i] >= self.smoothed[i] {
                    self.smoothed[i] = bins[i];
                } else {
                    self.smoothed[i] = self.smoothed[i] * decay + bins[i] * (1.0 - decay);
                }
            }
            self.has_real_data = true;
            self.had_real_data = true;
            if let Some(ms) = self.band_jump.first_frame() {
                log::info!("Spectrum RX2: first frame of the new band after {} ms", ms);
            }
        }
    }

    /// The passband's power in dBm without the calibration offset, for the
    /// calibration (`raw_passband_dbm`). filter_low/high are in Hz relative
    /// to the VFO (e.g. 50..2850 for USB, -2850..-50 for LSB). `None` when
    /// there is no reading.
    pub fn compute_raw_passband_power_dbm(&self, filter_low: i32, filter_high: i32) -> Option<f32> {
        let vfo_offset_hz = self.vfo_freq_hz as f64 - self.ddc_center_hz as f64;
        raw_passband_dbm(&self.smoothed, self.has_real_data, self.sample_rate_hz, vfo_offset_hz, filter_low, filter_high)
    }

    /// The passband over the floor of the whole spectrum, in dB (`passband_excess_db`).
    pub fn passband_excess_db(&self, filter_low: i32, filter_high: i32) -> Option<f32> {
        if !self.has_real_data {
            return None;
        }
        let vfo_offset_hz = self.vfo_freq_hz as f64 - self.ddc_center_hz as f64;
        passband_excess_db(&self.smoothed, self.sample_rate_hz, vfo_offset_hz, filter_low, filter_high)
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
                self.centre_measured = true;

                if !self.tci_mode && old_center != 0 && old_center != freq_hz {
                    log::debug!("RX2 set_vfo_freq: vfo {} -> {}, ddc_center {} -> {} (no HP, CTUN off)",
                        old_vfo, freq_hz, old_center, freq_hz);
                    self.shift_smoothed(old_center, freq_hz);
                }
            }
            // CTUN on: freeze ddc_center_hz (don't follow VFO)
        } else if old_vfo != freq_hz {
            log::debug!("RX2 set_vfo_freq: vfo {} -> {} (HP active, ddc_center stays {})",
                old_vfo, freq_hz, self.ddc_center_hz);
        }
    }

    /// Set DDC center from Protocol 2 HP packet (absolute, overrides VFO).
    pub fn set_ddc_center(&mut self, freq_hz: u64) {
        let old = self.ddc_center_hz;
        self.ddc_center_hz = freq_hz;
        self.has_hp_center = true;
        self.centre_measured = true;

        if old != 0 && old != freq_hz {
            log::debug!("RX2 set_ddc_center (HP): {} -> {} (vfo={})", old, freq_hz, self.vfo_freq_hz);
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
            self.band_jump.jumped(old_center, new_center);
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
        // A gap after real data - a band jump, another DDC rate or FFT size -
        // sends nothing until the first real frame (build 239). The frame it
        // sent lay on the floor of the scale: the clients drew an empty
        // picture and a dark waterfall row, and the auto-ref settled on it.
        // The test spectrum stays for a server that never had a
        // radio.
        if self.had_real_data && !self.has_real_data {
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

    /// Extract a view centered on absolute frequency (mirror of
    /// SpectrumProcessor::extract_view_at for RX2). Used for VRX2
    /// high-res spectrum.
    pub fn extract_view_at(
        &self,
        center_hz: u64,
        span_hz: u32,
        max_bins: usize,
    ) -> SpectrumPacket {
        let total = self.smoothed.len();
        if total == 0 || span_hz == 0 {
            return SpectrumPacket {
                sequence: self.sequence, num_bins: 0, center_freq_hz: 0,
                span_hz: 0, ref_level: 0, db_per_unit: 1, bins: Vec::new(),
            };
        }
        let hz_per_bin = self.sample_rate_hz as f64 / total as f64;
        let ddc_lo = self.ddc_center_hz as f64 - (self.sample_rate_hz as f64 / 2.0);
        let view_lo = (center_hz as f64) - (span_hz as f64 / 2.0);
        let view_hi = (center_hz as f64) + (span_hz as f64 / 2.0);
        let bin_lo = ((view_lo - ddc_lo) / hz_per_bin).floor() as isize;
        let bin_hi = ((view_hi - ddc_lo) / hz_per_bin).ceil() as isize;
        // Guard only - the window itself is not clamped; see the RX1 variant.
        let start = bin_lo.clamp(0, total as isize) as usize;
        let end = bin_hi.clamp(0, total as isize) as usize;
        if end <= start {
            return SpectrumPacket {
                sequence: self.sequence, num_bins: 0, center_freq_hz: 0,
                span_hz: 0, ref_level: 0, db_per_unit: 1, bins: Vec::new(),
            };
        }
        let cal_shift = cal_shift(self.cal_offset_db);
        // Requested window, bin for bin; outside the DDC data -> 0 (floor).
        let requested_bins = (span_hz as f64 / hz_per_bin).ceil().max(1.0);
        let out_n = max_bins.min(requested_bins as usize).max(1);
        let stride = requested_bins / out_n as f64;
        let first = (view_lo - ddc_lo) / hz_per_bin;
        let bins: Vec<u16> = (0..out_n)
            .map(|i| {
                let s = first + i as f64 * stride;
                let e = first + (i as f64 + 1.0) * stride;
                let js = s.floor() as isize;
                let je = (e.ceil() as isize).max(js + 1);
                let mut max_val = 0.0f32;
                let mut have = false;
                for j in js.max(0)..je.min(total as isize) {
                    have = true;
                    let v = self.smoothed[j as usize];
                    if v > max_val { max_val = v; }
                }
                if !have { return 0; }
                pack_shifted(max_val, cal_shift)
            })
            .collect();
        SpectrumPacket {
            sequence: self.sequence,
            num_bins: bins.len() as u16,
            center_freq_hz: center_hz as u32,
            span_hz,
            ref_level: 0,
            db_per_unit: 1,
            bins,
        }
    }

    /// Extract view - same logic as SpectrumProcessor::extract_view
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

        let cal_shift = cal_shift(self.cal_offset_db);
        let bins: Vec<u16> = if visible <= max_bins {
            self.smoothed[start..end].iter()
                .map(|v| pack_shifted(*v, cal_shift))
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
                pack_shifted(max_val, cal_shift)
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

        let cal_shift = cal_shift(self.cal_offset_db);
        let bins: Vec<u16> = if total <= max_bins {
            self.smoothed.iter().map(|v| pack_shifted(*v, cal_shift)).collect()
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
                pack_shifted(max_val, cal_shift)
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

/// DDC FFT pipeline: complex I/Q FFT -> FFT-shift -> power -> N bins (parametric size)
pub struct DdcFftPipeline {
    fft: Arc<dyn rustfft::Fft<f32>>,
    window: Vec<f32>,
    fft_buf: Vec<Complex<f32>>,
}

impl DdcFftPipeline {
    pub fn new(fft_size: usize) -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(fft_size);

        // Hann window for the FFT. Narrower main lobe than Blackman-Harris at
        // the cost of higher side lobes - the trade-off we prefer for sharper
        // peak visibility in panadapter display.
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

    /// Process DDC I/Q samples -> N log-power bins (0-255 range as f32).
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

            // Map dB range to 0..MAX per the shared pack scale (-150 dB -> 0, -30 dB -> full)
            output[i] = ((db - SPECTRUM_PACK_FLOOR_DB) / SPECTRUM_PACK_RANGE_DB * SPECTRUM_PACK_MAX)
                .clamp(0.0, SPECTRUM_PACK_MAX);
        }

        output
    }
}

#[cfg(test)]
mod centre_measured_tests {
    use super::{Rx2SpectrumProcessor, SpectrumProcessor};

    /// Only a measured centre goes to the clients:
    /// the starting value of a processor is not one - seen in the field as
    /// `DDC centre from the server: RX2 7050 kHz`.
    #[test]
    fn a_starting_value_is_not_a_measured_centre() {
        // RX1 outside the DDC mode keeps its starting value, whatever comes.
        let mut sp = SpectrumProcessor::new();
        assert!(!sp.ddc_center_measured());
        sp.set_vfo_freq(7_100_000, false);
        sp.set_ddc_center(7_138_162);
        assert!(!sp.ddc_center_measured(), "not in the DDC mode");
        // In the DDC mode: CTUN on keeps the start, a dds measures.
        let mut sp = SpectrumProcessor::new();
        sp.init_ddc_fft(384_000);
        assert!(!sp.ddc_center_measured(), "the VFO's starting value");
        sp.set_vfo_freq(7_100_000, true);
        assert!(!sp.ddc_center_measured(), "CTUN on: the centre did not follow");
        sp.set_ddc_center(7_138_162);
        assert!(sp.ddc_center_measured());
        // CTUN off: the centre is the VFO, a measurement.
        let mut sp = SpectrumProcessor::new();
        sp.init_ddc_fft(384_000);
        sp.set_vfo_freq(7_100_000, false);
        assert!(sp.ddc_center_measured());
        assert_eq!(sp.ddc_center_hz(), 7_100_000);
        // RX2 the same way.
        let mut rx2 = Rx2SpectrumProcessor::new();
        assert!(!rx2.ddc_center_measured(), "7050 kHz is a starting value");
        rx2.set_vfo_freq(7_073_000, true);
        assert!(!rx2.ddc_center_measured());
        rx2.set_ddc_center(7_073_000);
        assert!(rx2.ddc_center_measured());
        let mut rx2 = Rx2SpectrumProcessor::new();
        rx2.set_vfo_freq(7_073_000, false);
        assert!(rx2.ddc_center_measured());
    }
}

#[cfg(test)]
mod band_jump_tests {
    use super::{BandJump, Rx2SpectrumProcessor, SpectrumProcessor};

    /// A band jump drops the FFT window's old samples (build 236): the operator
    /// saw "first empty, then the old spectrum, then the right one" on every
    /// band change, RX and VRX alike, with the full-band row on or off.
    #[test]
    fn a_band_jump_drops_the_old_window() {
        let mut j = BandJump::default();
        assert_eq!(j.since(0), None, "no jump yet");
        assert!(j.takes(0));
        j.jumped(7_100_000, 14_200_000);
        let seen = j.since(0).expect("a jump the window has not dropped");
        assert_eq!((seen.from_hz, seen.to_hz), (7_100_000, 14_200_000));
        assert!(!j.takes(0), "a frame from the old window is not taken");
        assert!(j.takes(seen.generation));
        assert_eq!(j.since(seen.generation), None, "dropped once");
        assert!(j.first_frame().is_some(), "the gap is measured");
        assert!(j.first_frame().is_none(), "once per jump");

        // Both processors: a band change jumps, a small shift does not.
        let mut rx1 = SpectrumProcessor::new();
        rx1.init_ddc_fft(384_000);
        // (starts at 14 200 kHz)
        rx1.set_ddc_center(14_230_000);
        assert_eq!(rx1.band_jump_since(0), None, "30 kHz shifts, it does not jump");
        rx1.set_ddc_center(7_100_000);
        assert!(rx1.band_jump_since(0).is_some(), "RX1 band change");
        let mut rx2 = Rx2SpectrumProcessor::new();
        rx2.init_ddc_fft(384_000);
        // (starts at 7050 kHz)
        rx2.set_ddc_center(7_080_000);
        assert_eq!(rx2.band_jump_since(0), None);
        rx2.set_ddc_center(14_200_000);
        assert!(rx2.band_jump_since(0).is_some(), "RX2 band change");

        // The loop drops each receiver's own window, before it builds a frame,
        // and hands the window's generation to the processor.
        let src = include_str!("audio_loops.rs");
        for (rx, accum, gen, lock) in [
            ("RX1", "rx1_accum", "rx1_window_gen", "spectrum"),
            ("RX2", "rx2_accum", "rx2_window_gen", "rx2_spectrum"),
        ] {
            let check = format!("if let Some(jump) = {lock}.lock().await.band_jump_since({gen}) {{");
            let line = format!("info!(\"Spectrum {rx}: band jump {{}} -> {{}} kHz, {{}} old samples dropped\"");
            let clear = format!("{gen} = jump.generation;\n                    {accum}.clear();");
            let feed = format!("{lock}.lock().await.process_ddc_frame(&frame, {gen});");
            for needle in [&check, &line, &clear, &feed] {
                assert_eq!(src.matches(needle.as_str()).count(), 1, "{needle}");
            }
            assert!(src.find(&check).unwrap() < src.find(&feed).unwrap(), "{rx}: drop before build");
        }
    }

    /// After real data a gap sends nothing until the first real frame (build
    /// 239): no frame on the floor of the scale. A processor that never had
    /// real data still sends its frames (both ways).
    #[test]
    fn a_gap_after_real_data_sends_nothing() {
        use std::time::{Duration, Instant};
        macro_rules! check {
            ($p:expr, $rx:literal) => {{
                let mut p = $p;
                p.init_ddc_fft(384_000);
                p.set_enabled(true);
                macro_rules! ready {
                    ($q:expr) => {{
                        $q.last_frame = Instant::now() - Duration::from_secs(1);
                        $q.is_frame_ready()
                    }};
                }
                assert!(ready!(p), "{}: no radio yet; frames go out", $rx);
                let frame = vec![(0.1f32, 0.0f32); p.ddc_fft_size()];
                p.process_ddc_frame(&frame, 0);
                assert!(ready!(p), "{}: real data", $rx);
                let mut gen = 0;
                // Twice: to another band and back.
                for to in [3_600_000u64, 7_100_000] {
                    p.set_ddc_center(to);
                    gen = p.band_jump_since(gen).expect("a jump").generation;
                    assert!(!ready!(p), "{}: the gap sends nothing", $rx);
                    assert!(!ready!(p), "{}: still nothing", $rx);
                    p.process_ddc_frame(&frame, gen);
                    assert!(ready!(p), "{}: the first real frame goes out", $rx);
                }
                p.update_sample_rate(192_000);
                assert!(!ready!(p), "{}: another rate is a gap too", $rx);
            }};
        }
        check!(SpectrumProcessor::new(), "RX1");
        check!(Rx2SpectrumProcessor::new(), "RX2");
    }
}

#[cfg(test)]
mod pack_scale_tests {
    use super::{cal_shift, pack_shifted};
    use sdr_remote_core::{SPECTRUM_PACK_MAX, SPECTRUM_PACK_RANGE_DB};

    /// The calibration shift and the clamp, by what they give (build 74): a
    /// whole dB range of offset is the whole packed scale, and the clamp holds
    /// both ends of a u16.
    #[test]
    fn the_calibration_shift_and_the_clamp_by_value() {
        assert_eq!(cal_shift(0.0), 0.0);
        assert_eq!(cal_shift(SPECTRUM_PACK_RANGE_DB), SPECTRUM_PACK_MAX);
        assert_eq!(cal_shift(-SPECTRUM_PACK_RANGE_DB / 2.0), -SPECTRUM_PACK_MAX / 2.0);
        assert_eq!(pack_shifted(1000.0, 0.0), 1000);
        assert_eq!(pack_shifted(1000.0, 24.5), 1024);
        assert_eq!(pack_shifted(SPECTRUM_PACK_MAX, 1.0), u16::MAX);
        assert_eq!(pack_shifted(SPECTRUM_PACK_MAX - 1.0, 0.0), u16::MAX - 1);
        assert_eq!(pack_shifted(-5.0, 0.0), 0);
        assert_eq!(pack_shifted(10.0, -20.0), 0);
    }
}

#[cfg(test)]
mod tx_probe_tests {
    use super::{levels_of, SpectrumLevels, TxProbe, TX_PROBE_MS};
    use sdr_remote_core::{SPECTRUM_PACK_FLOOR_DB, SPECTRUM_PACK_MAX};
    use std::time::{Duration, Instant};

    fn lv(peak_db: f32, mean_db: f32, at_ceiling: usize) -> SpectrumLevels {
        SpectrumLevels { peak_db, mean_db, at_ceiling }
    }

    /// One line per change, after its window, with every frame in it - also
    /// the first frame of a window that a change opened early (builds 172,
    /// 174).
    #[test]
    fn the_probe_writes_one_line_per_change() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut p = TxProbe::default();
        assert!(!p.wants(false), "nothing to look at yet");
        assert_eq!(p.frame(false, lv(-60.0, -100.0, 0), at(0)), None, "no change yet");
        assert!(p.wants(true), "a change is wanted");
        assert_eq!(p.frame(true, lv(-20.0, -90.0, 3), at(100)), None, "the change opens the window");
        assert!(p.wants(true), "and the window stays wanted");
        assert_eq!(p.frame(true, lv(-30.0, -95.0, 0), at(167)), None);
        let line = p.frame(true, lv(-40.0, -95.0, 0), at(100 + TX_PROBE_MS + 1)).expect("the line after the window");
        assert_eq!(line, "Spectrum around TX on: 0:-20/-90/3 67:-30/-95/0");
        assert!(!p.wants(true), "closed");
        // A change inside a window ends it, and its own first frame is kept.
        assert_eq!(p.frame(false, lv(-60.0, -100.0, 0), at(3000)), None);
        let line = p.frame(true, lv(-10.0, -90.0, 0), at(3300)).expect("the off window ended by a new change");
        assert_eq!(line, "Spectrum around TX off: 0:-60/-100/0");
        let line = p.frame(true, lv(-50.0, -90.0, 0), at(3300 + TX_PROBE_MS + 1)).expect("its own line");
        assert!(line.starts_with("Spectrum around TX on: 0:-10/-90/0"), "{line}");
    }

    /// The levels are calibrated dB, and a bin on the ceiling is counted
    /// (the line printed raw pack units).
    #[test]
    fn the_levels_are_db_and_count_the_ceiling() {
        let full = SPECTRUM_PACK_MAX;
        let l = levels_of(&[0.0, full / 2.0, full, full], 0.0);
        assert_eq!(l.peak_db, -30.0);
        assert_eq!(l.at_ceiling, 2);
        assert!((l.mean_db - (SPECTRUM_PACK_FLOOR_DB + 0.625 * 120.0)).abs() < 0.01, "{}", l.mean_db);
        assert_eq!(levels_of(&[full / 2.0], 5.0).peak_db, -85.0);
        // Just under the ceiling is not on it.
        assert_eq!(levels_of(&[full - 1.0], 0.0).at_ceiling, 0);
        // The processor hands its calibration on.
        let code = include_str!("spectrum.rs");
        let code = &code[..code.find("mod tx_probe_tests").unwrap()];
        assert_eq!(code.matches("levels_of(&self.smoothed, self.cal_offset_db)").count(), 1);
    }
}

#[cfg(test)]
mod fft_size_tests {
    use super::{Rx2SpectrumProcessor, SpectrumProcessor};
    use sdr_remote_core::MAX_FFT_SIZE_K;

    /// A size above the shared limit runs at the limit, on both receivers;
    /// the limit itself and one below it run as asked. Auto stays inside it at
    /// the highest DDC rate the server takes.
    #[test]
    fn the_fft_size_stops_at_the_shared_limit() {
        let max = MAX_FFT_SIZE_K as usize * 1024;
        assert!(sdr_remote_core::ddc_fft_size(1_536_000) <= max);
        let mut rx1 = SpectrumProcessor::new();
        let mut rx2 = Rx2SpectrumProcessor::new();
        let rx2_size = |p: &Rx2SpectrumProcessor| p.ddc_pipeline.as_ref().map(|p| p.window.len()).unwrap_or(0);
        for (asked, runs) in [(MAX_FFT_SIZE_K * 2, max), (MAX_FFT_SIZE_K, max), (MAX_FFT_SIZE_K / 2, max / 2)] {
            rx1.set_fft_size(asked);
            rx2.set_fft_size(asked);
            assert_eq!((rx1.current_fft_size(), rx2_size(&rx2)), (runs, runs), "asked {asked}K");
        }
    }
}

#[cfg(test)]
mod smeter_calibration_tests {
    use super::{
        calibrate_against_smeter, hold_calibration_while_transmitting, passband_excess_db, CalStep, CalWatch,
        Rx2SpectrumProcessor, SpectrumProcessor, CAL_QUIET_EXCESS_DB, CAL_SIGNAL_HOLDOFF,
    };
    use sdr_remote_core::{SPECTRUM_PACK_MAX, SPECTRUM_PACK_RANGE_DB};
    use std::time::{Duration, Instant};

    /// Packed bins for a flat floor, with `db` dB more over the bins
    /// `from..to`. 1024 bins over 96 kHz: 93.75 Hz a bin, the VFO in the middle.
    fn bins(from: usize, to: usize, db: f32) -> Vec<f32> {
        let floor = 20_000.0f32;
        let step = db * SPECTRUM_PACK_MAX / SPECTRUM_PACK_RANGE_DB;
        (0..1024).map(|i| if (from..to).contains(&i) { floor + step } else { floor + (i % 7) as f32 }).collect()
    }

    const RATE: u32 = 96_000;

    /// One tick of a receiver that has not had a signal for long.
    fn quiet_tick(static_cal: f32, current: f32, smeter: f32, raw: f32, excess: Option<f32>) -> (f32, CalStep) {
        calibrate_against_smeter(static_cal, current, smeter, Some(raw), excess, &mut None, Instant::now())
    }

    /// The passband over the floor: about 0 for noise, the signal's height for
    /// a signal in the passband, and about 0 again for a signal beside it.
    #[test]
    fn the_passband_stands_over_the_floor_only_with_a_signal_in_it() {
        // USB 0..2800 Hz from the VFO at bin 512: bins 512..542.
        let quiet = passband_excess_db(&bins(0, 0, 0.0), RATE, 0.0, 0, 2_800).unwrap();
        assert!(quiet.abs() < 0.1, "noise only: {quiet}");
        let inside = passband_excess_db(&bins(512, 542, 20.0), RATE, 0.0, 0, 2_800).unwrap();
        assert!((inside - 20.0).abs() < 0.5, "a signal filling the passband: {inside}");
        let beside = passband_excess_db(&bins(600, 630, 30.0), RATE, 0.0, 0, 2_800).unwrap();
        assert!(beside.abs() < 0.1, "a signal beside the passband: {beside}");
        // The VFO off-centre moves the passband with it.
        let moved = passband_excess_db(&bins(600, 630, 30.0), RATE, 88.0 * 93.75, 0, 2_800).unwrap();
        assert!((moved - 30.0).abs() < 0.5, "the passband follows the VFO: {moved}");
        // LSB -2800..0 Hz: bins 482..512, below the VFO; its mirror above does not count.
        let lsb = passband_excess_db(&bins(482, 512, 20.0), RATE, 0.0, -2_800, 0).unwrap();
        assert!((lsb - 20.0).abs() < 0.5, "a signal in the LSB passband: {lsb}");
        let mirrored = passband_excess_db(&bins(512, 542, 20.0), RATE, 0.0, -2_800, 0).unwrap();
        assert!(mirrored.abs() < 0.1, "the same signal on the USB side: {mirrored}");
        assert_eq!(passband_excess_db(&[], RATE, 0.0, 0, 2_800), None);
        assert_eq!(passband_excess_db(&bins(0, 0, 0.0), RATE, 1.0e6, 0, 2_800), None, "outside the bins");
    }

    /// An empty passband teaches the offset, a tenth a tick, as before; a
    /// signal in it holds the offset however far the S-meter lags.
    #[test]
    fn a_signal_in_the_passband_holds_the_calibration() {
        let static_cal = -10.0;
        // Quiet for long: the old step, exactly.
        let (learned, step) = quiet_tick(static_cal, -12.0, -100.0, -95.0, Some(0.5));
        assert_eq!(step, CalStep::Learned);
        let dynamic = -100.0 - -95.0 - static_cal;
        assert_eq!(learned, static_cal + (-12.0 - static_cal) * 0.9 + dynamic * 0.1);
        // Someone talks in the passband: our bins jump 15 dB, the S-meter 3.
        // A hundred ticks (ten seconds), twice in a row, with one memory of
        // the last signal and the clock running: the offset stands, and the
        // first empty tick after it waits.
        let start = Instant::now();
        let mut last_signal = None;
        let mut offset = -12.0;
        let mut ms = 0u64;
        for pass in 0..2 {
            for tick in 0..100 {
                let now = start + Duration::from_millis(ms);
                let (next, step) = calibrate_against_smeter(static_cal, offset, -97.0, Some(-80.0), Some(15.0), &mut last_signal, now);
                assert_eq!(step, CalStep::HeldForSignal, "pass {pass} tick {tick}");
                assert_eq!(next, offset);
                assert_eq!(last_signal, Some(now), "pass {pass} tick {tick}: the signal is remembered");
                offset = next;
                ms += 100;
            }
            let now = start + Duration::from_millis(ms);
            let (next, step) = calibrate_against_smeter(static_cal, offset, -100.0, Some(-95.0), Some(0.5), &mut last_signal, now);
            assert_eq!((next, step), (offset, CalStep::WaitingAfterSignal), "pass {pass}: right after the signal");
            ms += 100;
        }
        // The ticks below stand for a receiver without a signal for long, so
        // each one starts without a memory of one (`quiet_tick`).
        // Just under the threshold still learns; at it, holds.
        assert_eq!(quiet_tick(static_cal, -12.0, -100.0, -95.0, Some(CAL_QUIET_EXCESS_DB - 0.01)).1, CalStep::Learned);
        assert_eq!(quiet_tick(static_cal, -12.0, -100.0, -95.0, Some(CAL_QUIET_EXCESS_DB)).1, CalStep::HeldForSignal);
        // A gate reading missing beside a raw reading holds and stamps, as a
        // signal would. It cannot happen today: both readings have the same
        // guard (`no_reading_is_no_signal` covers a missing raw reading).
        assert_eq!(quiet_tick(static_cal, -12.0, -100.0, -95.0, None), (-12.0, CalStep::HeldForSignal));
        assert_eq!(CAL_QUIET_EXCESS_DB, 6.0);
    }

    /// An over as the field log of build 24 showed it: words with pauses in
    /// between, the S-meter still high in every pause. Nothing is learned
    /// during the over, nor in the five seconds after the last word; then the
    /// quiet band teaches again. Two overs in a row.
    #[test]
    fn the_pauses_of_an_over_teach_nothing() {
        assert_eq!(CAL_SIGNAL_HOLDOFF, Duration::from_secs(5));
        let static_cal = 0.0;
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut last_signal = None;
        let mut offset = 10.0;
        let mut ms = 0u64;
        for over in 0..2 {
            // 15 s of speech: 600 ms of words, 400 ms of pause, the S-meter at
            // -55 dBm throughout and our passband back on the floor in a pause.
            let mut last_word = ms;
            for tick in 0..150 {
                let in_word = tick % 10 < 6;
                let (excess, raw) = if in_word { (Some(20.0), Some(-65.0)) } else { (Some(1.0), Some(-86.0)) };
                let (next, step) = calibrate_against_smeter(static_cal, offset, -55.0, raw, excess, &mut last_signal, at(ms));
                let want = if in_word { CalStep::HeldForSignal } else { CalStep::WaitingAfterSignal };
                assert_eq!(step, want, "over {over} tick {tick}");
                assert_eq!(next, offset, "over {over} tick {tick}: the offset stands");
                if in_word {
                    last_word = ms;
                }
                offset = next;
                ms += 100;
            }
            // Released: the S-meter sinks back over the next seconds. Within
            // five seconds of the last word nothing is learned; after it the
            // quiet band teaches, and the offset comes back to where it was.
            for tick in 0..100 {
                let since_word = Duration::from_millis(ms - last_word);
                let smeter = -76.0 + 21.0 * (-(since_word.as_secs_f32()) / 1.5).exp();
                let (next, step) = calibrate_against_smeter(static_cal, offset, smeter, Some(-86.0), Some(1.0), &mut last_signal, at(ms));
                if since_word < CAL_SIGNAL_HOLDOFF {
                    assert_eq!(step, CalStep::WaitingAfterSignal, "over {over} tick {tick}, {since_word:?} after");
                    assert_eq!(next, offset);
                } else {
                    assert_eq!(step, CalStep::Learned, "over {over} tick {tick}, {since_word:?} after");
                }
                offset = next;
                ms += 100;
            }
            assert!((offset - 10.0).abs() < 0.5, "over {over}: back at the quiet calibration, {offset}");
        }
    }

    /// The geometry in the field: 65536 bins over 192 kHz, 2.93 Hz a bin.
    const REAL_RATE: u32 = 192_000;
    const REAL_BINS: usize = 65_536;
    const CENTRE_HZ: u64 = 7_100_000;

    /// Packed bins of a floor with a ripple of about 0.7 dB, with `db` dB more
    /// from `from_hz` to `to_hz` off the DDC centre.
    fn real_bins(from_hz: i64, to_hz: i64, db: f32) -> Vec<f32> {
        let hz_per_bin = REAL_RATE as f64 / REAL_BINS as f64;
        let half = (REAL_BINS / 2) as f64;
        let at = |hz: i64| (half + hz as f64 / hz_per_bin).round() as usize;
        let (from, to) = (at(from_hz), at(to_hz));
        let step = db * SPECTRUM_PACK_MAX / SPECTRUM_PACK_RANGE_DB;
        (0..REAL_BINS)
            .map(|i| 20_000.0 + (i * 7_919 % 13) as f32 * 30.0 + if (from..to).contains(&i) { step } else { 0.0 })
            .collect()
    }

    /// What the calibration reads through a receiver's own methods: the
    /// passband over the floor, and the raw passband power. Each receiver
    /// carries an offset of -20 dB, which the raw reading leaves out.
    fn read(rx2: bool, smoothed: Vec<f32>, has_data: bool, vfo_off_hz: i64, lo: i32, hi: i32) -> (Option<f32>, Option<f32>) {
        let vfo_hz = (CENTRE_HZ as i64 + vfo_off_hz) as u64;
        if rx2 {
            let mut sp = Rx2SpectrumProcessor::new();
            sp.smoothed = smoothed;
            sp.sample_rate_hz = REAL_RATE;
            sp.has_real_data = has_data;
            sp.vfo_freq_hz = vfo_hz;
            sp.ddc_center_hz = CENTRE_HZ;
            sp.cal_offset_db = -20.0;
            (sp.passband_excess_db(lo, hi), sp.compute_raw_passband_power_dbm(lo, hi))
        } else {
            let mut sp = SpectrumProcessor::new();
            sp.smoothed = smoothed;
            sp.sample_rate_hz = REAL_RATE;
            sp.has_real_data = has_data;
            sp.vfo_freq_hz = vfo_hz;
            sp.ddc_center_hz = CENTRE_HZ;
            sp.cal_offset_db = -20.0;
            (sp.passband_excess_db(lo, hi), sp.compute_raw_passband_power_dbm(lo, hi))
        }
    }

    /// Each receiver reads its own passband: USB above the VFO, LSB below it,
    /// with the VFO in the middle of the DDC and 20 kHz beside it. A signal
    /// there holds; the same signal mirrored to the other side of the VFO
    /// does not; without data there is no reading.
    #[test]
    fn both_receivers_read_their_own_passband() {
        for rx2 in [false, true] {
            for (vfo_off, lo, hi) in [(0i64, 0, 2_800), (0, -2_800, 0), (20_000, 0, 2_800), (-20_000, -2_800, 0)] {
                let case = format!("rx2 {rx2}, VFO {vfo_off:+} Hz, {lo}..{hi}");
                let (lo_hz, hi_hz) = (vfo_off + lo as i64 + 200, vfo_off + hi as i64 - 200);
                let inside = read(rx2, real_bins(lo_hz, hi_hz, 20.0), true, vfo_off, lo, hi).0.unwrap();
                assert!(inside >= CAL_QUIET_EXCESS_DB, "{case}: a signal in the passband, {inside}");
                let (mirror_lo, mirror_hi) = (2 * vfo_off - hi_hz, 2 * vfo_off - lo_hz);
                let mirrored = read(rx2, real_bins(mirror_lo, mirror_hi, 20.0), true, vfo_off, lo, hi).0.unwrap();
                assert!(mirrored < 1.0, "{case}: the same signal mirrored, {mirrored}");
                let quiet = read(rx2, real_bins(0, 0, 0.0), true, vfo_off, lo, hi).0.unwrap();
                assert!(quiet < 1.0, "{case}: noise only, {quiet}");
                assert_eq!(read(rx2, real_bins(lo_hz, hi_hz, 20.0), false, vfo_off, lo, hi), (None, None), "{case}");
            }
        }
    }

    /// The number the step learns from, absolute, on both receivers: noise in
    /// an SSB and in a CW passband of 50 Hz (its edges inside a bin, once with
    /// the lower edge past half a bin, so a rounded edge shows), a 12 Hz
    /// carrier 40 dB over the floor in that CW passband, a signal with the VFO
    /// 20 kHz either side of the DDC centre, and a passband below the bins.
    /// Worked out apart from this code, from the packed scale and the edge
    /// weights.
    #[test]
    fn the_raw_passband_reading_is_absolute() {
        for rx2 in [false, true] {
            for (name, smoothed, vfo_off, lo, hi, want) in [
                ("SSB noise", real_bins(0, 0, 0.0), 0i64, 0, 2_800, -80.24),
                ("CW noise", real_bins(0, 0, 0.0), 0, 575, 625, -97.70),
                ("CW noise, the lower edge past half a bin", real_bins(0, 0, 0.0), 0, 605, 655, -97.74),
                ("CW carrier", real_bins(594, 606, 40.0), 0, 575, 625, -63.86),
                ("USB, the VFO 20 kHz up", real_bins(20_200, 22_600, 20.0), 20_000, 0, 2_800, -60.90),
                ("LSB, the VFO 20 kHz down", real_bins(-22_600, -20_200, 20.0), -20_000, -2_800, 0, -60.90),
            ] {
                let raw = read(rx2, smoothed, true, vfo_off, lo, hi).1.unwrap();
                assert!((raw - want).abs() < 0.05, "rx2 {rx2}, {name}: {raw} dBm, want {want}");
            }
            let below = read(rx2, real_bins(0, 0, 0.0), true, -100_000, 0, 2_800);
            assert_eq!(below, (None, None), "rx2 {rx2}: a passband below the bins, with data");
        }
    }

    /// Without a reading the calibration neither learns nor holds, whatever
    /// the offset: a receiver without data at an offset of -20 dB.
    #[test]
    fn no_reading_is_no_signal() {
        for rx2 in [false, true] {
            let (excess, raw) = read(rx2, real_bins(0, 0, 0.0), false, 0, 0, 2_800);
            let mut last_signal = None;
            let step = calibrate_against_smeter(-20.0, -20.0, -100.0, raw, excess, &mut last_signal, Instant::now());
            assert_eq!(step, (-20.0, CalStep::NoReading), "rx2 {rx2}");
            assert_eq!(last_signal, None, "rx2 {rx2}: no hold-off from a missing reading");
        }
    }

    /// A carrier four bins wide (12 Hz) holds the calibration like speech
    /// across the whole passband. The gate and the step take the same sum, so
    /// the gate reads exactly what the signal adds to the passband power: a
    /// signal too weak to hold moves the step by less than the threshold.
    #[test]
    fn a_carrier_a_few_bins_wide_holds_the_calibration() {
        let (noise_excess, noise_raw) = read(false, real_bins(0, 0, 0.0), true, 0, 0, 2_800);
        let noise_excess = noise_excess.unwrap();
        let mut last_signal = None;
        let now = Instant::now();
        let quiet = calibrate_against_smeter(0.0, 10.0, -100.0, noise_raw, Some(noise_excess), &mut last_signal, now);
        assert_eq!(quiet.1, CalStep::Learned, "noise only: {noise_excess}");
        let carrier = real_bins(1_494, 1_506, 40.0);
        let (excess, raw) = read(false, carrier.clone(), true, 0, 0, 2_800);
        let excess = excess.unwrap();
        assert!(excess >= CAL_QUIET_EXCESS_DB + 6.0, "a carrier 40 dB over the floor: {excess}");
        let held = calibrate_against_smeter(0.0, 10.0, -100.0, raw, Some(excess), &mut last_signal, now);
        assert_eq!(held, (10.0, CalStep::HeldForSignal));
        assert_eq!(read(true, carrier, true, 0, 0, 2_800).0, Some(excess), "RX2 reads it the same");
        for (from, to, db) in [(1_494, 1_506, 40.0), (1_494, 1_506, 20.0), (1_494, 1_506, 6.0), (300, 2_500, 3.0)] {
            let (excess, raw) = read(false, real_bins(from, to, db), true, 0, 0, 2_800);
            let (gate, step) = (excess.unwrap() - noise_excess, raw.unwrap() - noise_raw.unwrap());
            assert!((gate - step).abs() < 0.01, "{from}..{to} Hz, {db} dB: gate {gate}, step {step}");
        }
    }

    /// Our own over: the calibration does not run while transmitting, yet the
    /// hold-off starts again at every tick of it, so after the release the
    /// quiet ticks wait as after a signal received. Two overs in a row.
    #[test]
    fn transmitting_starts_the_hold_off_again() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut last_signal = None;
        let mut offset = 10.0;
        let mut ms = 0u64;
        for over in 0..2 {
            // Eight seconds of transmitting, longer than the hold-off.
            for _ in 0..80 {
                hold_calibration_while_transmitting(&mut last_signal, at(ms));
                ms += 100;
            }
            let last_tx = ms - 100;
            for tick in 0..60 {
                let (next, step) = calibrate_against_smeter(0.0, offset, -60.0, Some(-86.0), Some(1.0), &mut last_signal, at(ms));
                if Duration::from_millis(ms - last_tx) < CAL_SIGNAL_HOLDOFF {
                    assert_eq!((next, step), (offset, CalStep::WaitingAfterSignal), "over {over} tick {tick}");
                } else {
                    assert_eq!(step, CalStep::Learned, "over {over} tick {tick}");
                }
                offset = next;
                ms += 100;
            }
        }
        // Without the ticks of the over, the wall clock alone had run the
        // hold-off out by the release.
        let mut before_the_over = Some(at(0));
        let (_, step) = calibrate_against_smeter(0.0, 10.0, -60.0, Some(-86.0), Some(1.0), &mut before_the_over, at(8_100));
        assert_eq!(step, CalStep::Learned);
    }

    /// Without a usable reading nothing is learned, and an offset never set
    /// starts from the static calibration, as before; held does the same.
    #[test]
    fn without_a_reading_the_static_calibration_stands() {
        assert_eq!(quiet_tick(-10.0, 0.0, -140.0, -95.0, Some(0.0)), (-10.0, CalStep::NoReading));
        assert_eq!(quiet_tick(-10.0, 0.0, -100.0, -140.0, Some(0.0)), (-10.0, CalStep::NoReading));
        let no_passband = calibrate_against_smeter(-10.0, -12.0, -100.0, None, None, &mut None, Instant::now());
        assert_eq!(no_passband, (-12.0, CalStep::NoReading));
        assert_eq!(quiet_tick(-10.0, -12.0, -140.0, -95.0, Some(0.0)), (-12.0, CalStep::NoReading));
        assert_eq!(quiet_tick(-10.0, 0.0, -100.0, -95.0, Some(20.0)), (-10.0, CalStep::HeldForSignal));
    }

    /// RX1 and RX2 both ask the one function, each with its own memory of the
    /// last signal; the step lives nowhere else.
    #[test]
    fn both_receivers_calibrate_through_the_one_function() {
        let network = include_str!("network.rs");
        let network = &network[..network.find("#[cfg(test)]").unwrap_or(network.len())];
        assert_eq!(network.matches("crate::spectrum::calibrate_against_smeter(").count(), 2, "RX1 and RX2");
        assert_eq!(network.matches("&mut cal_signal_rx1, now,").count(), 1);
        assert_eq!(network.matches("&mut cal_signal_rx2, now,").count(), 1);
        // Each receiver with its own passband and its own static calibration.
        assert_eq!(network.matches("sp.passband_excess_db(filter_low, filter_high)").count(), 1);
        assert_eq!(network.matches("rx2_sp.passband_excess_db(rx2_filter_low, rx2_filter_high)").count(), 1);
        assert_eq!(network.matches("static_cal_rx1, sp.cal_offset_db(), tci_dbm, raw_dbm").count(), 1);
        assert_eq!(network.matches("static_cal_rx2, rx2_sp.cal_offset_db(), tci_dbm, raw_dbm").count(), 1);
        // The raw reading with the receiver's own filter edges, against its own S-meter.
        assert_eq!(network.matches("let raw_dbm = sp.compute_raw_passband_power_dbm(filter_low, filter_high);").count(), 1);
        assert_eq!(
            network.matches("let raw_dbm = rx2_sp.compute_raw_passband_power_dbm(rx2_filter_low, rx2_filter_high);").count(),
            1
        );
        assert_eq!(network.matches("} else if let Some(tci_dbm) = tci_smeter_dbm_rx1 {").count(), 1);
        assert_eq!(network.matches("} else if let Some(tci_dbm) = tci_smeter_dbm_rx2 {").count(), 1);
        // Both pause for anything on the air, not for our own PTT alone, and
        // start their hold-off again at every such tick, with the time of it.
        assert_eq!(network.matches("let on_the_air = ptt_guard.tx_now().on_the_air();").count(), 1);
        let gates: Vec<usize> = network.match_indices("if !on_the_air {").map(|(i, _)| i).collect();
        assert_eq!(gates.len(), 2, "RX1 and RX2");
        let column = |at: usize| network[..at].rsplit('\n').next().unwrap_or("").len();
        for (rx, gate) in [("rx1", gates[0]), ("rx2", gates[1])] {
            let stamp = format!("crate::spectrum::hold_calibration_while_transmitting(&mut cal_signal_{rx}, Instant::now());");
            assert_eq!(network.matches(stamp.as_str()).count(), 1, "{rx}");
            let at = network.find(stamp.as_str()).unwrap();
            assert!(network[..at].trim_end().ends_with("} else {"), "{rx}: the stamp is the else of the calibration");
            // The else of the pause itself, not of a branch inside it, and the
            // step attenuator's branch waits inside the pause too.
            assert!(gate < at, "{rx}: the stamp after its own pause");
            let block = &network[gate..at];
            assert!(block.contains(&format!("if step_att_enabled_{rx} {{")), "{rx}: the step attenuator waits for the air too");
            let els = gate + block.rfind("} else {").unwrap();
            assert_eq!(column(els), column(gate), "{rx}: the stamp is the else of the pause itself");
            let after = network[at + stamp.len()..].trim_start();
            assert!(after.starts_with(&format!("cal_watch_{rx}.note_on_the_air();")), "{rx}: and counted in the line");
        }
        assert_eq!(network.matches("cal_watch_rx1.note(").count() + network.matches("cal_watch_rx2.note(").count(), 2);
        // A measuring line, every ten seconds: only in developer mode, as the
        // other `(10 s)` lines.
        for rx in ["rx1", "rx2"] {
            let at = network.find(&format!("cal_watch_{rx}.note(step,")).unwrap();
            let after = &network[at..];
            let next = after[after.find('{').unwrap() + 1..].trim_start();
            assert!(next.starts_with("dev_info!(\"{line}\");"), "{rx}: the line in developer mode only");
        }
        assert!(!network.contains("* 0.9 + dynamic * 0.1"), "no second copy of the step");
    }

    /// The line comes every ten seconds, counts what happened, and starts over.
    #[test]
    fn the_calibration_line_counts_ten_seconds() {
        let start = Instant::now();
        let mut watch = CalWatch::new("RX1");
        for i in 0..99u64 {
            let step = match i % 3 {
                0 => CalStep::HeldForSignal,
                1 => CalStep::WaitingAfterSignal,
                _ => CalStep::Learned,
            };
            let line = watch.note(step, Some(i as f32 / 10.0), -97.0, Some(-95.0), -11.0, -10.0, start + Duration::from_millis(i * 100));
            assert!(line.is_none(), "tick {i}");
        }
        let line = watch.note(CalStep::NoReading, None, -97.0, Some(-95.0), -11.5, -10.0, start + Duration::from_secs(10)).unwrap();
        assert_eq!(
            line,
            concat!(
                "Spectrum RX1 calibration against the S-meter (10 s): learned 33, held for a signal 33, ",
                "waiting after a signal 33, no reading 1, on the air 0; passband over the floor 0.0..9.8 dB (held from 6); ",
                "offset -11.5..-11.0 dB (static -10.0); now S-meter -97.0 dBm, passband -95.0 dBm raw"
            )
        );
        // Second window: counted afresh.
        let later = start + Duration::from_secs(11);
        assert!(watch.note(CalStep::Learned, Some(1.0), -97.0, Some(-95.0), -11.0, -10.0, later).is_none());
        let line = watch.note(CalStep::Learned, Some(1.0), -97.0, Some(-95.0), -11.0, -10.0, later + Duration::from_secs(10)).unwrap();
        assert!(line.contains("learned 2, held for a signal 0, waiting after a signal 0, no reading 0, on the air 0"), "{line}");
        // On the air: counted into the next line, without one of its own; a
        // tick without a passband reading shows as "-".
        let third = later + Duration::from_secs(21);
        assert!(watch.note(CalStep::Learned, Some(1.0), -97.0, Some(-95.0), -11.0, -10.0, third).is_none());
        for _ in 0..80 {
            watch.note_on_the_air();
        }
        let line = watch.note(CalStep::NoReading, None, -97.0, None, -11.0, -10.0, third + Duration::from_secs(10)).unwrap();
        assert!(line.contains("no reading 1, on the air 80;"), "{line}");
        assert!(line.ends_with("passband - dBm raw"), "{line}");
        // The next window counts the air afresh.
        let fourth = third + Duration::from_secs(11);
        assert!(watch.note(CalStep::Learned, Some(1.0), -97.0, Some(-95.0), -11.0, -10.0, fourth).is_none());
        let line = watch.note(CalStep::Learned, Some(1.0), -97.0, Some(-95.0), -11.0, -10.0, fourth + Duration::from_secs(10)).unwrap();
        assert!(line.contains("no reading 0, on the air 0;"), "{line}");
    }
}
