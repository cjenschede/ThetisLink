// SPDX-License-Identifier: GPL-2.0-or-later

//! FFT-channelizer that converts a wideband complex I/Q stream into
//! narrowband audio at a single tunable carrier offset. Designed for
//! amateur-radio SSB demodulation (USB / LSB) but the bin-selection
//! is generic.
//!
//! The output sample rate is fixed at 8 kHz; `FFT_N` is derived from
//! the input rate at construction so the FFT/iFFT bin-widths match
//! exactly regardless of which DDC rate the host SDR provides
//! (96 / 192 / 384 / 768 / 1536 kHz are all supported).

use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

use crate::config::{VrxConfig, VrxMode};

/// Narrowband output (8 kHz, Opus NB): 128-point iFFT → 62.5 Hz/bin,
/// audio bandwidth limited to ~4 kHz. Default.
pub const NB_OUTPUT_RATE_HZ: u32 = 8_000;
pub const NB_IFFT_N: usize = 128;

/// Wideband output (16 kHz, Opus WB): 256-point iFFT → 62.5 Hz/bin
/// (same bin width as NB!), audio bandwidth up to ~8 kHz. For AM/FM
/// quality and avoiding NBFM sideband clipping.
pub const WB_OUTPUT_RATE_HZ: u32 = 16_000;
pub const WB_IFFT_N: usize = 256;

// Legacy aliases kept for non-runtime callers (vrx-spike, examples).
pub const OUTPUT_RATE_HZ: u32 = NB_OUTPUT_RATE_HZ;
pub const IFFT_N: usize = NB_IFFT_N;
pub const OUTPUT_HOP: usize = NB_IFFT_N / 2;

/// Default symmetric SSB bandwidth (3 kHz = 48 audio bins). Kept as a
/// convenience constant; runtime filter is asymmetric (lo + hi).
pub const DEFAULT_BW_BINS: usize = 48;

/// Default FFT size assumed for the legacy 384 kHz DDC rate (kept for
/// the offline synthetic test path in `vrx-spike`).
pub const DEFAULT_FFT_N: usize = 6144;
pub const DEFAULT_INPUT_HOP: usize = 3072;

/// Compute the FFT size for given input + output rates so the FFT bin-
/// width matches the iFFT bin-width exactly. Both NB and WB iFFTs use
/// 62.5 Hz/bin, so the math is `fft_n = ifft_n × input_rate / output_rate`.
pub fn fft_n_for_rates(input_rate_hz: u32, output_rate_hz: u32) -> usize {
    let ifft_n = ifft_n_for_output_rate(output_rate_hz);
    let m = (input_rate_hz / output_rate_hz).max(1) as usize;
    let n = ifft_n.saturating_mul(m);
    if n % 2 == 0 { n } else { n + 1 }
}

/// Back-compat shim: assumes narrowband (8 kHz) output rate.
pub fn fft_n_for_rate(input_rate_hz: u32) -> usize {
    fft_n_for_rates(input_rate_hz, NB_OUTPUT_RATE_HZ)
}

/// Map output rate → iFFT size. Only NB / WB supported.
pub fn ifft_n_for_output_rate(output_rate_hz: u32) -> usize {
    match output_rate_hz {
        WB_OUTPUT_RATE_HZ => WB_IFFT_N,
        _ => NB_IFFT_N,
    }
}

/// Live FFT-channelizer for one VRX. Accumulates input I/Q samples,
/// processes complete FFT frames, returns narrowband audio at
/// `OUTPUT_RATE_HZ`.
pub struct VrxChannelizer {
    config: VrxConfig,
    input_rate_hz: u32,
    output_rate_hz: u32,

    // Runtime-derived FFT parameters (set in `new` from input_rate_hz
    // so FFT bin-width matches iFFT bin-width on any DDC rate).
    fft_n: usize,
    ifft_n: usize,
    input_hop: usize,
    output_hop: usize,

    // FFT/iFFT plans (Arc'd internally by rustfft)
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,

    // Hann analysis window
    window: Vec<f32>,

    // Accumulating input buffer; consumed in `input_hop`-sized chunks.
    input_buf: Vec<Complex32>,

    // FFT scratch buffers
    fft_buf: Vec<Complex32>,
    ifft_buf: Vec<Complex32>,

    // Overlap-add tail for the audio output. Complex so AM / SAM / FM
    // demod (non-linear) can combine overlapped frames before the
    // mode-specific demodulator runs. SSB modes just take .re.
    ola_tail: Vec<Complex32>,

    // AM envelope DC-removal filter state (running mean of |z|).
    am_dc_filter: f32,
    // FM previous-sample state for phase-diff demod.
    fm_last_sample: Complex32,

    // Residual NCO — compensates for the sub-bin frequency offset.
    // Bin-selection quantises to bin_width (typically 62.5 Hz). The
    // NCO rotates the complex baseband output after the iFFT so the
    // actual carrier lands exactly on the iFFT's zero frequency.
    nco_rotor: Complex32,                 // exp(-j 2π Δf / Fs_out) per sample
    nco_phase_at_frame_start: Complex32,
    // Odd frame since construction. With 50 % overlap, bin k of frame m
    // carries the phase (-1)^(k*m); the shift to baseband has to take that
    // off, or an odd carrier bin cancels in the overlap-add (build 225).
    frame_odd: bool,
    samples_since_nco_renorm: u32,

    // AGC state
    agc_envelope: f32,
    agc_attack_alpha: f32,
    agc_decay_alpha: f32,
    agc_target: f32,
    agc_max_gain: f32,

    // SAM carrier-recovery PLL (2nd-order, mirrors Thetis/WDSP amd.c
    // mode 1). Only used in VrxMode::Sam. Tracks the carrier offset left
    // after the residual NCO and pins the carrier to the real axis so the
    // in-phase component is the recovered audio (synchronous AM).
    pll_phs: f32,       // VCO phase (radians, kept in [0, 2π))
    pll_omega: f32,     // loop-filter frequency estimate / integrator (rad/sample)
    pll_fil_out: f32,   // last loop-filter output (instantaneous freq, rad/sample)
    pll_g1: f32,        // proportional gain (WDSP g1)
    pll_g2: f32,        // integral gain (WDSP g2)
    pll_omega_min: f32, // integrator lower clamp (rad/sample) = -capture range
    pll_omega_max: f32, // integrator upper clamp (rad/sample) = +capture range

    // SAM lock detector + unlock-decay (PATCH-vrx-wide-sam-ux). The lock
    // metric (in-phase vs quadrature power, amplitude-normalised) gates the
    // host's auto-tune AFC; the carrier-present reference drives an
    // unlock-decay that leaks the integrator back to 0 in a deep fade so
    // re-acquisition restarts from centre (the wider ±3 kHz clamp would
    // otherwise leave it wandered). First-cut design — see patch brief §5.2.
    pll_lock_pi: f32,      // ema of in-phase power Re(z_d)²
    pll_lock_pq: f32,      // ema of quadrature power Im(z_d)²
    pll_mag_peak: f32,     // slow-decaying peak of total power (carrier-present ref)
    pll_locked: bool,      // lock state (hysteresis on pi/pq ratio)
    pll_lock_alpha: f32,   // ema coefficient for pi/pq (~50 ms)
    pll_peak_decay: f32,   // per-sample decay of mag_peak (~3 s)
    pll_unlock_decay: f32, // per-sample decay of pll_omega when carrier absent (~0.5 s)
}

impl VrxChannelizer {
    pub fn new(config: VrxConfig, input_rate_hz: u32) -> Self {
        Self::new_with_output_rate(config, input_rate_hz, NB_OUTPUT_RATE_HZ)
    }

    /// Construct with explicit output sample rate (8 kHz = NB, 16 kHz = WB).
    pub fn new_with_output_rate(
        config: VrxConfig,
        input_rate_hz: u32,
        output_rate_hz: u32,
    ) -> Self {
        let ifft_n = ifft_n_for_output_rate(output_rate_hz);
        let output_hop = ifft_n / 2;
        let fft_n = fft_n_for_rates(input_rate_hz, output_rate_hz);
        let input_hop = fft_n / 2; // 50% overlap

        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(fft_n);
        let ifft = planner.plan_fft_inverse(ifft_n);

        let window: Vec<f32> = (0..fft_n)
            .map(|i| {
                let x = i as f32 / (fft_n - 1) as f32;
                0.5 - 0.5 * (std::f32::consts::TAU * x).cos()
            })
            .collect();

        let t_sample = 1.0 / output_rate_hz as f32;
        let agc_attack_alpha = 1.0 - (-t_sample / 0.010).exp();
        let agc_decay_alpha = 1.0 - (-t_sample / 0.500).exp();

        // Compute residual offset: target carrier may not land exactly
        // on a bin centre. NCO rotates by -residual_hz per sample to
        // bring the carrier to true zero in the iFFT output.
        let bin_width = input_rate_hz as f32 / fft_n as f32;
        let nearest_bin = (config.carrier_offset_hz / bin_width).round();
        let residual_hz = config.carrier_offset_hz - nearest_bin * bin_width;
        let theta_per_sample =
            -std::f32::consts::TAU * residual_hz / output_rate_hz as f32;
        let nco_rotor = Complex32::from_polar(1.0, theta_per_sample);

        // SAM carrier-recovery PLL coefficients — mirrors Thetis/WDSP
        // amd.c (Warren Pratt, GPL-2.0): ζ = 1.0, ωN = 250 rad/s, scaled
        // to the output rate. g1 = proportional, g2 = integral gain.
        // Capture/hold range ±3 kHz (Thetis uses ±2 kHz; widened to ±3 kHz
        // so the loop locks across a typical ≥6 kHz AM channel).
        let pll_zeta = 1.0_f32;
        let pll_omega_n = 250.0_f32; // rad/s
        let fs = output_rate_hz as f32;
        let pll_g1 = 1.0 - (-2.0 * pll_omega_n * pll_zeta / fs).exp();
        let pll_g2 = -pll_g1
            + 2.0
                * (1.0
                    - (-pll_omega_n * pll_zeta / fs).exp()
                        * (pll_omega_n / fs * (1.0 - pll_zeta * pll_zeta).sqrt()).cos());
        let pll_omega_max = std::f32::consts::TAU * 3000.0 / fs;
        let pll_omega_min = -pll_omega_max;

        // Lock-detector / unlock-decay time constants (scaled to rate).
        let pll_lock_alpha = 1.0 - (-1.0 / (fs * 0.05)).exp(); // ~50 ms ema
        // Slow peak-hold (~30 s) for the carrier-present reference: keeps the
        // "what a carrier looks like" level long after a station drops, so a
        // deep fade reads as carrier-absent (→ unlock-decay) rather than the
        // peak adapting down to the noise floor within seconds.
        let pll_peak_decay = (-1.0 / (fs * 30.0)).exp();
        let pll_unlock_decay = (-1.0 / (fs * 0.5)).exp(); // ~0.5 s integrator leak

        Self {
            config,
            input_rate_hz,
            output_rate_hz,
            fft_n,
            ifft_n,
            input_hop,
            output_hop,
            fft,
            ifft,
            window,
            input_buf: Vec::with_capacity(fft_n * 2),
            fft_buf: vec![Complex32::new(0.0, 0.0); fft_n],
            ifft_buf: vec![Complex32::new(0.0, 0.0); ifft_n],
            ola_tail: vec![Complex32::new(0.0, 0.0); output_hop],
            am_dc_filter: 0.0,
            fm_last_sample: Complex32::new(0.0, 0.0),
            nco_rotor,
            nco_phase_at_frame_start: Complex32::new(1.0, 0.0),
            frame_odd: false,
            samples_since_nco_renorm: 0,
            // Channelizer output is typically tiny (~1e-5) after bin-
            // selection on a wideband stream. Pulling that up to an
            // AGC target of 0.5 needs gain ~50 000, so the ceiling is
            // set to 100 000. The attack/decay envelope still limits
            // how fast the gain can climb on noise-only spans.
            agc_envelope: 0.00001,
            agc_attack_alpha,
            agc_decay_alpha,
            agc_target: 0.5,
            agc_max_gain: 100_000.0,
            pll_phs: 0.0,
            pll_omega: 0.0,
            pll_fil_out: 0.0,
            pll_g1,
            pll_g2,
            pll_omega_min,
            pll_omega_max,
            pll_lock_pi: 0.0,
            pll_lock_pq: 0.0,
            pll_mag_peak: 0.0,
            pll_locked: false,
            pll_lock_alpha,
            pll_peak_decay,
            pll_unlock_decay,
        }
    }

    pub fn output_rate(&self) -> u32 {
        self.output_rate_hz
    }

    pub fn ifft_size(&self) -> usize {
        self.ifft_n
    }

    pub fn config(&self) -> &VrxConfig {
        &self.config
    }

    /// Tracked SAM carrier offset in Hz (the loop's smoothed frequency
    /// estimate, i.e. the integrator — not the jittery instantaneous
    /// output). Meaningful only in `VrxMode::Sam` once locked; used by an
    /// external AFC ("auto-tune to carrier") to re-centre the tuning.
    pub fn sam_carrier_offset_hz(&self) -> f32 {
        self.pll_omega * self.output_rate_hz as f32 / std::f32::consts::TAU
    }

    /// SAM lock state (in-phase power dominates quadrature, with
    /// hysteresis). The host's auto-tune AFC only nudges the tuning while
    /// this is true, so it never snaps to noise.
    pub fn is_locked(&self) -> bool {
        self.pll_locked
    }

    /// AFC handoff: after the host has moved the tuning (carrier offset) by
    /// `applied_hz` toward the carrier, remove that amount from the PLL's
    /// frequency estimate so the loop doesn't re-accumulate it. Net
    /// frequency is unchanged; responsibility shifts from PLL to tuning,
    /// which drives the residual (and thus the loop) to ~0.
    pub fn apply_afc_handoff(&mut self, applied_hz: f32) {
        let d = std::f32::consts::TAU * applied_hz / self.output_rate_hz as f32;
        self.pll_omega =
            (self.pll_omega - d).clamp(self.pll_omega_min, self.pll_omega_max);
    }

    /// Sample rate this channelizer was constructed for. Callers use
    /// this to detect input-rate changes and rebuild the channelizer
    /// with a matching FFT size.
    pub fn input_rate_hz(&self) -> u32 {
        self.input_rate_hz
    }

    /// Update SSB filter window (audio-bin offsets from carrier).
    /// Cheap mutation; safe to call every batch.
    pub fn set_filter(&mut self, lo_bins: usize, hi_bins: usize) {
        self.config.filter_lo_bins = lo_bins;
        self.config.filter_hi_bins = hi_bins;
    }

    /// Update the carrier offset (Hz from the IQ stream's centre).
    /// Called every batch when VFO/DDC change so the bin-selection
    /// follows the user's intended frequency.
    pub fn set_carrier_offset(&mut self, offset_hz: f32) {
        if (offset_hz - self.config.carrier_offset_hz).abs() < f32::EPSILON {
            return;
        }
        self.config.carrier_offset_hz = offset_hz;
        // Recompute residual NCO so sub-bin tuning is correct for the
        // new offset. We let the running `nco_phase_at_frame_start`
        // continue from where it was — a small phase discontinuity is
        // fine; resetting it would be worse (audible click).
        let bin_width = self.input_rate_hz as f32 / self.fft_n as f32;
        let nearest_bin = (offset_hz / bin_width).round();
        let residual_hz = offset_hz - nearest_bin * bin_width;
        let theta_per_sample =
            -std::f32::consts::TAU * residual_hz / self.output_rate_hz as f32;
        self.nco_rotor = Complex32::from_polar(1.0, theta_per_sample);
    }

    /// Feed a batch of input I/Q samples. Returns any audio samples
    /// produced from completed FFT frames (at `OUTPUT_RATE_HZ`).
    pub fn push_iq(&mut self, iq_pairs: &[(f32, f32)]) -> Vec<f32> {
        self.input_buf
            .extend(iq_pairs.iter().map(|&(i, q)| Complex32::new(i, q)));

        let mut audio_out: Vec<f32> = Vec::new();

        while self.input_buf.len() >= self.fft_n {
            // Window + load
            for i in 0..self.fft_n {
                self.fft_buf[i] = self.input_buf[i] * self.window[i];
            }
            self.fft.process(&mut self.fft_buf);

            // Bin-select → iFFT input (zero first, then fill)
            for slot in self.ifft_buf.iter_mut() {
                *slot = Complex32::new(0.0, 0.0);
            }
            let bin_width = self.input_rate_hz as f32 / self.fft_n as f32;
            let carrier_bin =
                (self.config.carrier_offset_hz / bin_width).round() as i32;
            let lo = self.config.filter_lo_bins.min(self.ifft_n / 2);
            let hi = self.config.filter_hi_bins.min(self.ifft_n / 2).max(lo);
            match self.config.mode {
                VrxMode::Usb => {
                    for i in lo..hi {
                        let bin = ((carrier_bin + i as i32)
                            .rem_euclid(self.fft_n as i32))
                            as usize;
                        self.ifft_buf[i] = self.fft_buf[bin];
                    }
                }
                VrxMode::Lsb => {
                    let start = (lo + 1).max(1);
                    let end = hi + 1;
                    for k in start..end {
                        let bin = ((carrier_bin - k as i32)
                            .rem_euclid(self.fft_n as i32))
                            as usize;
                        self.ifft_buf[self.ifft_n - k] = self.fft_buf[bin];
                    }
                }
                VrxMode::Am | VrxMode::Sam | VrxMode::Fm => {
                    // Both sidebands → complex baseband. Carrier bin at
                    // iFFT[0], USB at iFFT[1..hi], LSB at iFFT[N-1..N-hi+1].
                    if lo == 0 {
                        self.ifft_buf[0] = self.fft_buf
                            [(carrier_bin.rem_euclid(self.fft_n as i32)) as usize];
                    }
                    let start = lo.max(1);
                    for k in start..=hi {
                        let bin_up = ((carrier_bin + k as i32)
                            .rem_euclid(self.fft_n as i32))
                            as usize;
                        let bin_dn = ((carrier_bin - k as i32)
                            .rem_euclid(self.fft_n as i32))
                            as usize;
                        self.ifft_buf[k] = self.fft_buf[bin_up];
                        self.ifft_buf[self.ifft_n - k] = self.fft_buf[bin_dn];
                    }
                }
            }
            self.ifft.process(&mut self.ifft_buf);

            // The frame phase of the whole-bin shift: (-1)^(carrier_bin * m).
            // Left out, every other frame came out inverted for an odd bin and
            // the overlap-add kept only hum - a VRX on any whole kHz under a
            // DDC centre off the 125 Hz grid (field 2026-10-16, build 225).
            if self.frame_odd && carrier_bin.rem_euclid(2) == 1 {
                for slot in self.ifft_buf.iter_mut() {
                    *slot = -*slot;
                }
            }
            self.frame_odd = !self.frame_odd;

            // Apply residual NCO to compensate sub-bin frequency offset.
            // Phase must stay coherent across overlap-add frames.
            let mut nco_phase = self.nco_phase_at_frame_start;
            for slot in self.ifft_buf.iter_mut() {
                *slot *= nco_phase;
                nco_phase *= self.nco_rotor;
            }
            for _ in 0..self.output_hop {
                self.nco_phase_at_frame_start *= self.nco_rotor;
            }
            // Periodically renormalize to keep magnitude ≈ 1.0
            // (floating-point drift accumulates over many multiplies).
            self.samples_since_nco_renorm += self.output_hop as u32;
            if self.samples_since_nco_renorm >= 1024 {
                let mag = self.nco_phase_at_frame_start.norm();
                if mag > 0.0 {
                    self.nco_phase_at_frame_start /= mag;
                }
                self.samples_since_nco_renorm = 0;
            }

            let norm = 1.0 / self.fft_n as f32;

            // Complex overlap-add → per-mode demod → AGC.
            for i in 0..self.output_hop {
                let z = self.ifft_buf[i] * norm + self.ola_tail[i];
                let sample = match self.config.mode {
                    VrxMode::Usb | VrxMode::Lsb => z.re,
                    VrxMode::Am => {
                        let mag = (z.re * z.re + z.im * z.im).sqrt();
                        // Slow DC tracker removes the carrier-amplitude bias.
                        self.am_dc_filter = 0.999 * self.am_dc_filter + 0.001 * mag;
                        mag - self.am_dc_filter
                    }
                    VrxMode::Sam => {
                        // Synchronous AM — mirrors Thetis/WDSP amd.c mode 1
                        // (both-sideband): a 2nd-order PLL locks the carrier
                        // to the real axis; the in-phase component is the
                        // recovered audio. Removes the beat that plain Re(z)
                        // shows on a mistuned carrier, and (with the ±3 kHz
                        // integrator clamp) pulls in across a wide offset.
                        let (sin_p, cos_p) = self.pll_phs.sin_cos();
                        // Derotate: z_d = z · e^{-jθ}.
                        let zd_re = z.re * cos_p + z.im * sin_p;
                        let zd_im = z.im * cos_p - z.re * sin_p;
                        // Phase detector. atan2 gives the true carrier phase
                        // error; for AM-with-carrier Re(z_d) ≥ 0 at lock so
                        // there is no Costas sign ambiguity — a plain PLL (not
                        // Costas) is the correct choice vs suppressed carrier.
                        let det = if zd_re == 0.0 && zd_im == 0.0 {
                            0.0
                        } else {
                            zd_im.atan2(zd_re)
                        };
                        // Lock detector (amplitude-normalised): at lock the
                        // in-phase power dominates the quadrature power. The
                        // slow peak of total power is a "carrier-present"
                        // reference — during pull-in the carrier is present
                        // (total ≈ peak) so the unlock-decay does NOT fire and
                        // a genuine wide-offset acquisition is preserved.
                        let pi_p = zd_re * zd_re;
                        let pq_p = zd_im * zd_im;
                        self.pll_lock_pi += self.pll_lock_alpha * (pi_p - self.pll_lock_pi);
                        self.pll_lock_pq += self.pll_lock_alpha * (pq_p - self.pll_lock_pq);
                        let total = self.pll_lock_pi + self.pll_lock_pq;
                        self.pll_mag_peak = total.max(self.pll_mag_peak * self.pll_peak_decay);
                        if self.pll_locked {
                            if self.pll_lock_pi < 1.5 * self.pll_lock_pq {
                                self.pll_locked = false;
                            }
                        } else if self.pll_lock_pi > 3.0 * self.pll_lock_pq {
                            self.pll_locked = true;
                        }
                        let carrier_present = total > 0.1 * self.pll_mag_peak;
                        // Loop filter (PI) with a one-sample delay on the phase
                        // update, exactly as WDSP. Integrator clamped to the
                        // ±3 kHz capture/hold range.
                        let del_out = self.pll_fil_out;
                        self.pll_omega = (self.pll_omega + self.pll_g2 * det)
                            .clamp(self.pll_omega_min, self.pll_omega_max);
                        // Unlock-decay: in a deep fade / empty channel let the
                        // integrator leak back toward 0 so re-acquisition after
                        // the carrier returns restarts from centre instead of a
                        // wandered ±3 kHz offset.
                        if !carrier_present {
                            self.pll_omega *= self.pll_unlock_decay;
                        }
                        self.pll_fil_out = self.pll_g1 * det + self.pll_omega;
                        self.pll_phs += del_out;
                        // Wrap into [0, 2π). A `while` (not a single test) keeps
                        // this correct even when the step approaches the clamp.
                        while self.pll_phs >= std::f32::consts::TAU {
                            self.pll_phs -= std::f32::consts::TAU;
                        }
                        while self.pll_phs < 0.0 {
                            self.pll_phs += std::f32::consts::TAU;
                        }
                        // Same slow DC tracker as AM so the recovered carrier
                        // doesn't bias the AGC.
                        self.am_dc_filter = 0.999 * self.am_dc_filter + 0.001 * zd_re;
                        zd_re - self.am_dc_filter
                    }
                    VrxMode::Fm => {
                        // FM discriminator: phase difference between
                        // consecutive complex baseband samples.
                        let prod = z * self.fm_last_sample.conj();
                        self.fm_last_sample = z;
                        prod.im.atan2(prod.re)
                    }
                };
                let abs_in = sample.abs();
                let alpha = if abs_in > self.agc_envelope {
                    self.agc_attack_alpha
                } else {
                    self.agc_decay_alpha
                };
                self.agc_envelope += alpha * (abs_in - self.agc_envelope);
                let gain = (self.agc_target / self.agc_envelope.max(1e-6))
                    .min(self.agc_max_gain);
                audio_out.push(sample * gain);
            }
            for i in 0..self.output_hop {
                self.ola_tail[i] = self.ifft_buf[i + self.output_hop] * norm;
            }

            // Slide the input buffer by input_hop (= 50% overlap).
            self.input_buf.drain(..self.input_hop);
        }

        audio_out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The share of the output power in a tone `tone_hz` from `carrier_hz`,
    /// measured over the second half of three seconds at 384 kHz. With
    /// `switch_to`, the carrier moves there after 1.5 s (`set_carrier_offset`).
    fn tone_purity(carrier_hz: f32, mode: VrxMode, tone_hz: f32, switch_to: Option<f32>) -> f64 {
        let (fs_in, fs_out) = (384_000u32, 8_000u32);
        let mut ch = VrxChannelizer::new_with_output_rate(
            VrxConfig { carrier_offset_hz: carrier_hz, mode, filter_lo_bins: 0, filter_hi_bins: 48 },
            fs_in,
            fs_out,
        );
        let n = fs_in as usize * 3;
        let mut out = Vec::new();
        let mut carrier = carrier_hz;
        let mut i0 = 0usize;
        while i0 < n {
            if i0 >= n / 2 {
                if let Some(to) = switch_to {
                    if carrier != to {
                        carrier = to;
                        ch.set_carrier_offset(to);
                        out.clear();
                    }
                }
            }
            let chunk: Vec<(f32, f32)> = (i0..(i0 + 7680).min(n))
                .map(|i| {
                    let t = i as f64 / fs_in as f64;
                    let (f, a) = match mode {
                        // A carrier with a 50 % tone on it.
                        VrxMode::Am | VrxMode::Sam => {
                            (carrier as f64, 1.0 + 0.5 * (std::f64::consts::TAU * tone_hz as f64 * t).cos())
                        }
                        _ => (carrier as f64 + tone_hz as f64, 1.0),
                    };
                    let p = std::f64::consts::TAU * f * t;
                    ((0.01 * a * p.cos()) as f32, (0.01 * a * p.sin()) as f32)
                })
                .collect();
            out.extend(ch.push_iq(&chunk));
            i0 += 7680;
        }
        let s = &out[out.len() / 2..];
        let tone = tone_hz.abs() as f64;
        let (mut re, mut im, mut tot) = (0.0f64, 0.0f64, 0.0f64);
        for (k, &x) in s.iter().enumerate() {
            let p = std::f64::consts::TAU * tone * k as f64 / fs_out as f64;
            re += x as f64 * p.cos();
            im += x as f64 * p.sin();
            tot += x as f64 * x as f64;
        }
        2.0 * (re * re + im * im) / s.len() as f64 / tot.max(1e-30)
    }

    /// Field 2026-10-16 (build 225): with the DDC centre of Thetis on 7138.162
    /// kHz every VRX on a whole kHz sat on an odd bin, and the sound was gone
    /// - hum in every mode, until a band change made the centre round. With
    /// 50 % overlap a shift by an odd number of bins turns every other frame
    /// over; the overlap-add then cancels the signal. Even, odd, the field
    /// case, a residual on top, and a move from even to odd while running.
    #[test]
    fn an_odd_carrier_bin_sounds_as_clean_as_an_even_one() {
        let bin = 384_000.0f32 / fft_n_for_rates(384_000, 8_000) as f32;
        assert_eq!(bin, 62.5);
        for (what, carrier) in [
            ("even bin", -464.0 * bin),
            ("odd bin", -451.0 * bin),
            ("the field case, 7110 kHz on 7138.162", -28_162.0),
            ("odd with a residual", -451.3 * bin),
        ] {
            assert!(tone_purity(carrier, VrxMode::Usb, 1000.0, None) > 0.99, "USB, {what}");
            assert!(tone_purity(carrier, VrxMode::Lsb, -1000.0, None) > 0.99, "LSB, {what}");
            assert!(tone_purity(carrier, VrxMode::Am, 1000.0, None) > 0.9, "AM, {what}");
        }
        assert!(
            tone_purity(-464.0 * bin, VrxMode::Usb, 1000.0, Some(-451.0 * bin)) > 0.99,
            "even, then odd while running"
        );
        assert!(
            tone_purity(-451.0 * bin, VrxMode::Usb, 1000.0, Some(-464.0 * bin)) > 0.99,
            "odd, then even while running"
        );
    }

    /// SAM mode must lock its carrier-recovery PLL onto a residual
    /// frequency offset (operator a few Hz off the true carrier). After
    /// the residual NCO, a pure tone at `carrier + Δf` sits at exactly
    /// `Δf` in the baseband, so the loop's frequency estimate must
    /// converge to `2π·Δf / output_rate` (rad/sample).
    #[test]
    fn sam_pll_locks_to_residual_carrier_offset() {
        let input_rate = 48_000u32;
        let output_rate = 8_000u32;
        let carrier = 5_000.0f32;
        let mistune_hz = 3.0f64; // a few Hz of operator mistuning

        let config = VrxConfig {
            carrier_offset_hz: carrier,
            mode: VrxMode::Sam,
            filter_lo_bins: 0,
            filter_hi_bins: 48,
        };
        let mut ch =
            VrxChannelizer::new_with_output_rate(config, input_rate, output_rate);

        // Pure carrier at (carrier + mistune) Hz. f64 phase accumulation
        // keeps the long tone numerically clean before casting to f32.
        let f = carrier as f64 + mistune_hz;
        let fs = input_rate as f64;
        let n_total = input_rate as usize; // ~1 s — ample time to lock
        let mut iq = Vec::with_capacity(n_total);
        for n in 0..n_total {
            let ph = std::f64::consts::TAU * f * (n as f64) / fs;
            iq.push((ph.cos() as f32, ph.sin() as f32));
        }
        for chunk in iq.chunks(2048) {
            let _ = ch.push_iq(chunk);
        }

        let expected =
            std::f32::consts::TAU * mistune_hz as f32 / output_rate as f32;
        let got = ch.pll_omega; // integrator converges to the freq offset
        assert!(
            got > 0.0 && (got - expected).abs() < 0.2 * expected,
            "PLL did not lock: got {got}, expected ≈ {expected}"
        );
    }

    fn sam_channelizer(carrier: f32) -> VrxChannelizer {
        VrxChannelizer::new_with_output_rate(
            VrxConfig { carrier_offset_hz: carrier, mode: VrxMode::Sam, filter_lo_bins: 0, filter_hi_bins: 48 },
            48_000,
            8_000,
        )
    }

    /// A clean on-frequency carrier must drive the lock detector to locked
    /// (in-phase power dominates quadrature) with a near-zero offset estimate.
    #[test]
    fn sam_lock_detector_locks_on_clean_carrier() {
        let carrier = 5_000.0f32;
        let mut ch = sam_channelizer(carrier);
        let fs = 48_000f64;
        let mut iq = Vec::with_capacity(48_000);
        for n in 0..48_000usize {
            let ph = std::f64::consts::TAU * carrier as f64 * (n as f64) / fs;
            iq.push((ph.cos() as f32, ph.sin() as f32));
        }
        for chunk in iq.chunks(2048) {
            let _ = ch.push_iq(chunk);
        }
        assert!(ch.is_locked(), "lock detector did not lock on a clean carrier");
        assert!(
            ch.sam_carrier_offset_hz().abs() < 5.0,
            "on-frequency carrier should give ~0 Hz offset, got {}",
            ch.sam_carrier_offset_hz()
        );
    }

    /// Deterministic noise (no carrier) must NOT false-lock: with no coherent
    /// carrier the in-phase and quadrature powers stay comparable.
    #[test]
    fn sam_lock_detector_rejects_noise() {
        let mut ch = sam_channelizer(5_000.0);
        // Simple deterministic LCG → pseudo-random complex samples in [-1,1].
        let mut seed: u32 = 0x1234_5678;
        let mut next = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / 8_388_608.0 - 1.0
        };
        let mut iq = Vec::with_capacity(48_000);
        for _ in 0..48_000usize {
            iq.push((next(), next()));
        }
        for chunk in iq.chunks(2048) {
            let _ = ch.push_iq(chunk);
        }
        assert!(!ch.is_locked(), "lock detector false-locked on noise");
    }

    /// The AFC handoff must be net-zero: after the host moves the tuning by
    /// `applied_hz`, subtracting the same amount from the loop estimate drives
    /// the residual to ~0 — and with the correct sign/scale.
    #[test]
    fn afc_handoff_is_net_zero_and_signed() {
        let mut ch = sam_channelizer(5_000.0);
        let fs = ch.output_rate_hz as f32;
        // Seed the integrator with a known +100 Hz offset; handoff of +100 Hz
        // must cancel it to ~0.
        ch.pll_omega = std::f32::consts::TAU * 100.0 / fs;
        ch.apply_afc_handoff(100.0);
        assert!(ch.pll_omega.abs() < 1e-6, "handoff not net-zero: {}", ch.pll_omega);
        // Sign/scale: handoff of +50 Hz from 0 → -(2π·50/fs).
        ch.pll_omega = 0.0;
        ch.apply_afc_handoff(50.0);
        let expected = -std::f32::consts::TAU * 50.0 / fs;
        assert!(
            (ch.pll_omega - expected).abs() < 1e-6,
            "wrong sign/scale: {} vs {}",
            ch.pll_omega,
            expected
        );
    }
}
