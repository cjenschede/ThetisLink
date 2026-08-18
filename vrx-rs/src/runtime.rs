// SPDX-License-Identifier: GPL-2.0-or-later

//! `VrxRuntime` glues the channelizer + Opus encoder + control-state
//! into a single sync `feed()` entry-point. Callers drive it with an
//! IQ batch + current VFO/DDC frequencies; the runtime reads the
//! shared control-state (enable / freq / mode), runs the DSP when
//! enabled, encodes Opus 20 ms frames and invokes a user-supplied
//! callback for each frame.
//!
//! The runtime is intentionally sync and free of async runtimes —
//! the host application's async loop (tokio / async-std / smol /
//! single-threaded) calls `feed()` whenever a new IQ batch arrives.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use log::{info, warn};

use crate::channelizer::{fft_n_for_rates, VrxChannelizer, NB_OUTPUT_RATE_HZ, WB_OUTPUT_RATE_HZ};
use crate::config::AUDIO_BIN_HZ;

/// Fraction of the DDC band a VRX may use. The outer part is the DDC's own
/// filter roll-off: a channel window reaching into it comes out attenuated on
/// one side, which the demodulator turns into audible distortion.
const DDC_USABLE_FRACTION: f32 = 0.9;

/// Convert signed-Hz filter edges (UI convention, like main spectrum)
/// to non-negative audio-bin offsets the channelizer wants. For USB
/// both Hz values are typically positive; for LSB both negative. The
/// runtime takes |Hz|/bin_width and orders lo<hi.
fn filter_to_audio_bins(low_hz: i32, high_hz: i32, mode: crate::config::VrxMode) -> (usize, usize) {
    let (near_hz, far_hz) = match mode {
        crate::config::VrxMode::Usb => (low_hz.max(0), high_hz.max(0)),
        crate::config::VrxMode::Lsb => ((-high_hz).max(0), (-low_hz).max(0)),
        // AM-family: symmetric around carrier. Half-bandwidth = max(|lo|, |hi|).
        crate::config::VrxMode::Am
        | crate::config::VrxMode::Sam
        | crate::config::VrxMode::Fm => (0, low_hz.abs().max(high_hz.abs())),
    };
    let lo = (near_hz as f32 / AUDIO_BIN_HZ).round().max(0.0) as usize;
    let hi = (far_hz as f32 / AUDIO_BIN_HZ).round().max(0.0) as usize;
    (lo.min(hi), lo.max(hi))
}
use crate::config::{VrxConfig, VrxControlState, VrxMode};
use crate::opus::VrxOpusEncoder;
use crate::wav::RollingWavWriter;

pub struct VrxRuntimeOptions {
    /// Identifies this VRX channel (0 = first VRX, 1 = second, ...).
    /// Passed through to the callback so a multi-VRX host can route
    /// frames to the right consumer.
    pub vrx_id: u8,
    /// Optional directory for rolling WAV captures (dev tooling).
    pub wav_dir: Option<String>,
    /// Segment length (s) for rolling WAV. Ignored if `wav_dir` is None.
    pub wav_segment_sec: u32,
    /// Wideband (16 kHz Opus, 256-pt iFFT, audio bandwidth up to 8 kHz)
    /// versus narrowband (8 kHz Opus, 128-pt iFFT, audio up to 4 kHz).
    /// Default narrowband. Affects channelizer iFFT size + Opus rate.
    pub wideband: bool,
}

impl Default for VrxRuntimeOptions {
    fn default() -> Self {
        Self {
            vrx_id: 0,
            wav_dir: None,
            wav_segment_sec: 10,
            wideband: false,
        }
    }
}

/// Callback invoked once per encoded Opus frame (20 ms at 8 kHz NB,
/// `FRAME_SAMPLES` audio samples). The host implements this to ship
/// the audio anywhere it wants — UDP, local playback, file, etc.
pub trait VrxAudioCallback {
    fn on_frame(
        &mut self,
        vrx_id: u8,
        audio_8k: &[f32],
        opus_bytes: &[u8],
        sequence: u32,
    );

    /// Called (throttled) while SAM auto-tune is following the carrier,
    /// with the new absolute listen frequency (Hz) so the host can update
    /// the client VFO. Default no-op — hosts that don't use auto-tune
    /// (e.g. `vrx-spike`) need not implement it.
    fn on_carrier_freq(&mut self, _vrx_id: u8, _freq_hz: u64) {}
}

pub struct VrxRuntime {
    opts_vrx_id: u8,
    output_rate_hz: u32,
    control: Arc<Mutex<VrxControlState>>,
    channelizer: Option<VrxChannelizer>,
    current_mode: VrxMode,
    opus: Option<VrxOpusEncoder>,
    /// Last verdict from the server: (protect, percent to budget for).
    loss_protection: (bool, u8),
    opus_input_buf: Vec<i16>,
    sequence: u32,
    wav: Option<RollingWavWriter>,
    last_logged_offset_hz: i32,
    /// True while the requested listen frequency sits so close to the DDC band
    /// edge that the channel window had to be pulled back inside it. Drives the
    /// log transition below; the client derives the same boundary itself, so
    /// there is deliberately no accessor promising more than that.
    edge_clamped: bool,
    last_logged_clamp: bool,
    // SAM auto-tune AFC (PATCH-vrx-wide-sam-ux). `afc_offset_hz` is the
    // accumulated correction added to the user's tuned frequency to follow
    // the carrier; reset when the user retunes or auto-tune is inactive.
    afc_offset_hz: f32,
    afc_last_base_hz: u64,
    afc_last_reported_hz: u64,
    // Smoothed loop-frequency estimate the AFC acts on. Averaging rejects the
    // per-batch noise on the PLL estimate at low SNR (weak-carrier jitter)
    // while still tracking real drift.
    afc_resid_ema: f32,
}

impl VrxRuntime {
    pub fn new(opts: VrxRuntimeOptions, control: Arc<Mutex<VrxControlState>>) -> Self {
        let output_rate_hz = if opts.wideband { WB_OUTPUT_RATE_HZ } else { NB_OUTPUT_RATE_HZ };
        let wav = opts.wav_dir.as_ref().map(|dir| {
            info!(
                "VRX{} runtime: WAV-dir={} segment={} s output_rate={} Hz",
                opts.vrx_id + 1,
                dir,
                opts.wav_segment_sec,
                output_rate_hz,
            );
            RollingWavWriter::new(
                PathBuf::from(dir),
                output_rate_hz,
                opts.wav_segment_sec,
            )
        });
        Self {
            opts_vrx_id: opts.vrx_id,
            output_rate_hz,
            control,
            channelizer: None,
            current_mode: VrxMode::Usb,
            opus: None,
            loss_protection: (false, 0),
            opus_input_buf: Vec::with_capacity(640),
            sequence: 0,
            wav,
            last_logged_offset_hz: i32::MIN,
            edge_clamped: false,
            last_logged_clamp: false,
            afc_offset_hz: 0.0,
            afc_last_base_hz: 0,
            afc_last_reported_hz: 0,
            afc_resid_ema: 0.0,
        }
    }

    /// Output sample rate (8000 for NB, 16000 for WB). Const after construction.
    /// Pass a packet-loss verdict down to the Opus encoder.
    ///
    /// Remembered even when there is no encoder yet - one is made lazily on the
    /// first audio, and a runtime that starts on a bad link should not have to
    /// wait for the next verdict to be protected.
    pub fn set_loss_protection(&mut self, on: bool, loss_pct: u8) {
        self.loss_protection = (on, loss_pct);
        if let Some(enc) = self.opus.as_mut() {
            if enc.set_loss_protection(on, loss_pct).is_err() {
                warn!(
                    "VRX{} runtime: encoder refused the error-correction change - keeping its previous setting",
                    self.opts_vrx_id + 1
                );
            }
        }
    }

    pub fn output_rate_hz(&self) -> u32 {
        self.output_rate_hz
    }

    /// Snapshot the SAM auto-tune state: `(afc_offset_hz, base_hz)`. Used to
    /// carry the carrier-follow across a runtime rebuild (NB↔WB rate change)
    /// so the switch doesn't drop the lock and force a slow re-pull-in from
    /// the original (possibly kHz-off) manual tuning. `afc_offset_hz` is in Hz
    /// so it transfers cleanly across the rate change.
    pub fn afc_state(&self) -> (f32, u64) {
        (self.afc_offset_hz, self.afc_last_base_hz)
    }

    /// Restore a snapshotted AFC state into a fresh runtime. Seeding both the
    /// offset and the base it is relative to keeps the next `feed()` from
    /// treating the rebuild as a manual retune (which would reset the offset).
    pub fn restore_afc_state(&mut self, afc_offset_hz: f32, base_hz: u64) {
        self.afc_offset_hz = afc_offset_hz;
        self.afc_last_base_hz = base_hz;
    }

    /// Feed one batch of IQ samples + current radio state. Reads
    /// `control` for enable/freq/mode each call, runs the channelizer
    /// when enabled, accumulates audio, encodes 20 ms Opus frames and
    /// invokes the callback for each frame. No-op when disabled.
    pub fn feed(
        &mut self,
        iq_sample_rate_hz: u32,
        iq: &[(f32, f32)],
        vfo_hz: u64,
        ddc_center_hz: u64,
        callback: &mut impl VrxAudioCallback,
    ) {
        let (enabled, target_freq_hz, mode, filter_low_hz, filter_high_hz, sam_auto_tune) = {
            let s = self.control.lock().expect("VrxControlState mutex poisoned").clone();
            (s.enabled, s.target_freq_hz, s.mode, s.filter_low_hz, s.filter_high_hz, s.sam_auto_tune)
        };
        if !enabled {
            // Reset opus accumulator so a stop/start doesn't bleed
            // half-filled frames across the gap.
            self.opus_input_buf.clear();
            return;
        }
        if iq_sample_rate_hz == 0 {
            return;
        }

        // (Re)create channelizer on first call, mode change, OR input
        // rate change. FFT size depends on input rate so a rate flip
        // requires a fresh channelizer with matching bin-width.
        let rate_changed = self
            .channelizer
            .as_ref()
            .map_or(true, |c| c.input_rate_hz() != iq_sample_rate_hz);
        // Translate signed Hz filter to audio-bin offsets per mode
        // (channelizer always wants non-negative distances from carrier).
        let (lo_bins, hi_bins) = filter_to_audio_bins(filter_low_hz, filter_high_hz, mode);

        if self.channelizer.is_none() || self.current_mode != mode || rate_changed {
            let config = VrxConfig {
                carrier_offset_hz: 0.0,
                mode,
                filter_lo_bins: lo_bins,
                filter_hi_bins: hi_bins,
            };
            let fft_n = fft_n_for_rates(iq_sample_rate_hz, self.output_rate_hz);
            self.channelizer = Some(VrxChannelizer::new_with_output_rate(
                config,
                iq_sample_rate_hz,
                self.output_rate_hz,
            ));
            self.current_mode = mode;
            // Drop whatever the previous configuration left in the accumulator.
            // It holds samples produced at the old mode/rate; keeping them makes
            // the first frames after a rebuild a mix of old and new audio, heard
            // as distortion right after a retune or mode change.
            self.opus_input_buf.clear();
            let ifft_n = self.channelizer.as_ref().map(|c| c.ifft_size()).unwrap_or(0);
            let bin_hz = iq_sample_rate_hz as f32 / fft_n.max(1) as f32;
            info!(
                "VRX{} runtime: channelizer (re)built — input_rate={} Hz, FFT_N={}, iFFT={}, output_rate={} Hz, bin={:.1} Hz, mode={:?}",
                self.opts_vrx_id + 1,
                iq_sample_rate_hz,
                fft_n,
                ifft_n,
                self.output_rate_hz,
                bin_hz,
                mode,
            );
        } else if let Some(ch) = self.channelizer.as_mut() {
            // Live filter update: cheap, just mutates two usize fields.
            ch.set_filter(lo_bins, hi_bins);
        }

        // target_freq_hz=0 → fall back to VFO so the listener hears
        // the current passband until the user explicitly tunes.
        let base_listen_hz = if target_freq_hz == 0 { vfo_hz } else { target_freq_hz };
        // SAM auto-tune: add the accumulated AFC correction to the user's
        // tuned frequency. Reset the correction whenever the user retunes,
        // leaves SAM, or disables auto-tune (so we snap back to the set freq).
        let afc_active = mode == VrxMode::Sam && sam_auto_tune;
        if base_listen_hz != self.afc_last_base_hz || !afc_active {
            self.afc_offset_hz = 0.0;
            self.afc_resid_ema = 0.0;
            self.afc_last_base_hz = base_listen_hz;
        }
        let absolute_listen_hz =
            (base_listen_hz as i128 + self.afc_offset_hz.round() as i128).max(0) as u64;
        let requested_offset_hz =
            (absolute_listen_hz as i128 - ddc_center_hz as i128) as f32;
        // Keep the whole channel window inside the usable part of the DDC band.
        // Past that point the DDC's own roll-off cuts into the passband and the
        // demodulated audio distorts asymmetrically - while the spectrum view,
        // which comes from the DDC FFT rather than this channelizer, still looks
        // healthy. A VRX is a passive listener inside RX1/RX2's window and must
        // never ask the radio to move, so it stops at the edge instead.
        let usable_half_hz = iq_sample_rate_hz as f32 * 0.5 * DDC_USABLE_FRACTION;
        let filter_edge_hz = filter_low_hz.abs().max(filter_high_hz.abs()) as f32;
        let max_offset_hz = (usable_half_hz - filter_edge_hz).max(0.0);
        let effective_offset_hz = requested_offset_hz.clamp(-max_offset_hz, max_offset_hz);
        self.edge_clamped = effective_offset_hz != requested_offset_hz;
        if self.edge_clamped != self.last_logged_clamp {
            self.last_logged_clamp = self.edge_clamped;
            if self.edge_clamped {
                warn!(
                    "VRX{} at DDC band edge: requested offset {:.0} Hz held at {:.0} Hz                      (usable half-band {:.0} Hz, filter edge {:.0} Hz) - retune RX or re-centre CTUN",
                    self.opts_vrx_id + 1, requested_offset_hz, effective_offset_hz,
                    usable_half_hz, filter_edge_hz,
                );
            } else {
                info!("VRX{} back inside the DDC band", self.opts_vrx_id + 1);
            }
        }
        if let Some(ch) = self.channelizer.as_mut() {
            ch.set_carrier_offset(effective_offset_hz);
        }
        // Log effective offset only when it changes by ≥ 1 audio bin
        // width (62.5 Hz). Keeps steady-state quiet, captures tuning.
        {
            let curr = effective_offset_hz as i32;
            if (curr - self.last_logged_offset_hz).abs() >= 63 {
                self.last_logged_offset_hz = curr;
                info!(
                    "VRX{} listen freq: {:.3} kHz (DDC={:.3} kHz, effective_offset={} Hz)",
                    self.opts_vrx_id + 1,
                    absolute_listen_hz as f64 / 1000.0,
                    ddc_center_hz as f64 / 1000.0,
                    curr,
                );
            }
        }

        let audio = self
            .channelizer
            .as_mut()
            .map(|ch| ch.push_iq(iq))
            .unwrap_or_default();
        if audio.is_empty() {
            return;
        }

        // SAM auto-tune AFC: once the PLL has locked, nudge the listen
        // frequency onto the carrier and report it so the client VFO follows.
        // The handoff removes the applied amount from the loop estimate so it
        // doesn't double-count; the offset is clamped to the ±3 kHz capture.
        if afc_active {
            let (locked, residual_hz) = self
                .channelizer
                .as_ref()
                .map(|ch| (ch.is_locked(), ch.sam_carrier_offset_hz()))
                .unwrap_or((false, 0.0));
            if locked {
                // Two-speed AFC tracker. FAR from the carrier (raw residual
                // > AFC_GEAR_HZ) the signal dominates noise, so pull in FAST
                // with a short averaging constant. Within AFC_GEAR_HZ switch to
                // a SLOW (~2 s) constant: a strong/wide AM carrier's modulation
                // pulls the inner PLL and the estimate wobbles, and at low SNR
                // it is noisy — slow averaging rejects both so a centred carrier
                // sits below the deadband → zero corrections → silent. This
                // keeps the fast phase going until close (no long humming tail)
                // while the locked end-state stays stable. Time-based α keeps
                // the constants independent of the IQ batch size.
                const AFC_GEAR_HZ: f32 = 30.0;
                const AFC_ACQUIRE_TAU_S: f32 = 0.3;
                const AFC_TRACK_TAU_S: f32 = 2.0;
                const AFC_DEADBAND_HZ: f32 = 5.0;
                const AFC_MAX_STEP_HZ: f32 = 150.0;
                let dt = audio.len() as f32 / self.output_rate_hz as f32;
                let tau = if residual_hz.abs() > AFC_GEAR_HZ {
                    AFC_ACQUIRE_TAU_S
                } else {
                    AFC_TRACK_TAU_S
                };
                let alpha = 1.0 - (-dt / tau).exp();
                self.afc_resid_ema += alpha * (residual_hz - self.afc_resid_ema);
                if self.afc_resid_ema.abs() >= AFC_DEADBAND_HZ {
                    let step = self.afc_resid_ema.clamp(-AFC_MAX_STEP_HZ, AFC_MAX_STEP_HZ);
                    // Clamp-aware handoff: at the ±3 kHz capture edge the offset
                    // moves less than `step`, so remove from the estimate and
                    // hand off only what was actually applied — keeps the handoff
                    // net-zero (no double-count / drift at the edge).
                    let old_offset = self.afc_offset_hz;
                    self.afc_offset_hz = (old_offset + step).clamp(-3000.0, 3000.0);
                    let actual_step = self.afc_offset_hz - old_offset;
                    self.afc_resid_ema -= actual_step;
                    if let Some(ch) = self.channelizer.as_mut() {
                        ch.apply_afc_handoff(actual_step);
                    }
                    let followed_hz =
                        (base_listen_hz as i128 + self.afc_offset_hz.round() as i128).max(0) as u64;
                    if (followed_hz as i64 - self.afc_last_reported_hz as i64).abs() >= 10 {
                        self.afc_last_reported_hz = followed_hz;
                        callback.on_carrier_freq(self.opts_vrx_id, followed_hz);
                    }
                }
            } else {
                // Not locked → don't accumulate noise into the estimate.
                self.afc_resid_ema = 0.0;
            }
        }

        if let Some(wav) = self.wav.as_mut() {
            if let Err(e) = wav.push(&audio) {
                warn!("VRX{} runtime: WAV write failed: {}", self.opts_vrx_id + 1, e);
            }
        }

        // Lazily create Opus encoder on first audio (so a never-
        // enabled runtime never instantiates one).
        if self.opus.is_none() {
            match VrxOpusEncoder::new_with_rate(self.output_rate_hz) {
                Ok(mut e) => {
                    let (on, pct) = self.loss_protection;
                    if e.set_loss_protection(on, pct).is_err() {
                        warn!(
                            "VRX{} runtime: new encoder refused the error-correction setting",
                            self.opts_vrx_id + 1
                        );
                    }
                    self.opus = Some(e);
                }
                Err(e) => {
                    warn!(
                        "VRX{} runtime: Opus encoder init failed: {} — audio frames dropped",
                        self.opts_vrx_id + 1,
                        e
                    );
                    return;
                }
            }
        }

        // f32 → i16 with clipping
        for &s in &audio {
            let clipped = s.clamp(-1.0, 1.0);
            self.opus_input_buf.push((clipped * 32767.0) as i16);
        }

        let encoder = self.opus.as_mut().expect("opus encoder just initialised");
        let frame_samples = encoder.frame_samples();
        while self.opus_input_buf.len() >= frame_samples {
            let frame: Vec<i16> = self.opus_input_buf.drain(..frame_samples).collect();
            let frame_audio_f32: Vec<f32> = frame.iter().map(|&s| s as f32 / 32767.0).collect();
            match encoder.encode(&frame) {
                Ok(opus_bytes) => {
                    callback.on_frame(
                        self.opts_vrx_id,
                        &frame_audio_f32,
                        &opus_bytes,
                        self.sequence,
                    );
                    self.sequence = self.sequence.wrapping_add(1);
                }
                Err(e) => {
                    warn!(
                        "VRX{} runtime: Opus encode failed: {} — frame dropped",
                        self.opts_vrx_id + 1,
                        e
                    );
                }
            }
        }
    }
}
