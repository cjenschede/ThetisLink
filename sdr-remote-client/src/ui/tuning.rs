// SPDX-License-Identifier: GPL-2.0-or-later
//! Tuning / frequency helpers for `SdrRemoteApp`: spectrum-center math, the
//! full-tuning-force + tuning-latch predicates, and the pending-frequency setters /
//! Yaesu frequency acceptance (optimistic-set + reconcile). Extracted verbatim from
//! `ui/mod.rs` - pure relocation, no behaviour change (no timing change). `pub(super)`
//! keeps them callable from the parent module tree.

use super::*;

impl SdrRemoteApp {
    pub(super) fn spectrum_target_center_hz(vfo_hz: u64, full_span_hz: u32, pan: f32, fallback_center_hz: u32) -> f64 {
        if full_span_hz > 0 {
            vfo_hz as f64 + pan as f64 * full_span_hz as f64
        } else {
            fallback_center_hz as f64
        }
    }

    pub(super) fn should_force_full_tuning(
        target_center_hz: f64,
        extracted_center_hz: u32,
        extracted_span_hz: u32,
    ) -> bool {
        if extracted_center_hz == 0 || extracted_span_hz == 0 {
            return false;
        }
        let delta_hz = (target_center_hz - extracted_center_hz as f64).abs();
        let threshold_hz = (extracted_span_hz as f64 * 0.5).clamp(8_000.0, 24_000.0);
        delta_hz > threshold_hz
    }

    pub(super) fn tuning_latch_active(
        force_full_tuning: bool,
        pending_freq: Option<u64>,
        pending_freq_at: Option<Instant>,
    ) -> bool {
        if !force_full_tuning {
            return false;
        }
        if pending_freq.is_some() {
            return true;
        }
        pending_freq_at.map_or(false, |t| t.elapsed().as_millis() < 250)
    }

    pub(super) fn set_pending_freq_a(&mut self, freq: u64) {
        let target_center = Self::spectrum_target_center_hz(
            freq,
            self.full_spectrum_span_hz,
            self.spectrum_pan,
            self.spectrum_center_hz,
        );
        self.frequency_hz = freq;
        self.pending_freq = Some(freq);
        self.pending_freq_at = Some(Instant::now());
        self.rx1_force_full_tuning = Self::should_force_full_tuning(
            target_center,
            self.spectrum_center_hz,
            self.spectrum_span_hz,
        );
    }

    pub(super) fn set_pending_freq_b(&mut self, freq: u64) {
        let target_center = Self::spectrum_target_center_hz(
            freq,
            self.rx2_full_spectrum_span_hz,
            self.rx2_spectrum_pan,
            self.rx2_spectrum_center_hz,
        );
        self.rx2_frequency_hz = freq;
        self.rx2_pending_freq = Some(freq);
        self.rx2_pending_freq_at = Some(Instant::now());
        self.rx2_force_full_tuning = Self::should_force_full_tuning(
            target_center,
            self.rx2_spectrum_center_hz,
            self.rx2_spectrum_span_hz,
        );
    }
    pub(super) fn set_pending_yaesu_freq(&mut self, slot: u8, freq: u64) {
        let now = Instant::now();
        if slot == 0 {
            self.yaesu_freq_a = freq;
            self.yaesu_pending_freq = Some(freq);
            self.yaesu_pending_freq_at = Some(now);
        } else {
            self.yaesu2_freq_a = freq;
            self.yaesu2_pending_freq = Some(freq);
            self.yaesu2_pending_freq_at = Some(now);
        }
    }

    pub(super) fn accept_yaesu_freq(
        current: &mut u64,
        pending: &mut Option<u64>,
        pending_at: &mut Option<Instant>,
        state_freq: u64,
    ) {
        if let Some(target) = *pending {
            let age_ms = pending_at.map_or(u128::MAX, |t| t.elapsed().as_millis());
            if age_ms >= 120 {
                let delta = (state_freq as i64 - target as i64).unsigned_abs();
                if delta == 0 || age_ms > 1000 {
                    *pending = None;
                    *pending_at = None;
                }
            }
        }
        if pending.is_none() {
            *current = state_freq;
        }
    }
}
