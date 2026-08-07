// SPDX-License-Identifier: GPL-2.0-or-later
//
// TL2-1 ctun-auto-recenter feature: server-side trigger evaluation + recenter action.
//
// Per PATCH-tl2-server-ctun-auto-recenter (A'-revised, operator: PA3GHM, 2026-05-07):
// - Operator-specific formula: threshold = 0.6 × visible_span; trigger when VFO comes within
//   threshold of DDC-edge. Guarantees the visible-window is always full (no black edges).
// - Per-RX independent state-machines (RX1, RX2)
// - Multi-client zoom-aggregation via MIN-zoom (server-side; clients push zoom via
//   ControlId::SpectrumZoom 0x09 / Rx2SpectrumZoom 0x10)
// - Checkbox-strictest enforcement: zoom-min 2× as long as one client has the checkbox off
// - PTT-defer (skip trigger during TX; on PTT-off transition: force eval)
// - Global CAT-mutex serializes RX1+RX2 recenter-actions (no interleave)
// - Cap-gated: feature only active when Thetis advertises `auto_recenter_ex`
//
// Implements observability requirements + edge-case-resilience.

use std::time::Instant;

/// Per-RX trigger state-machine.
#[derive(Debug)]
pub struct PerRxState {
    /// Recentering-burst is in-flight; trigger-eval is skipped until the flag clears.
    pub recentering: bool,
    /// When the flag is automatically cleared (200 ms after recenter-start).
    pub flag_clear_at: Option<Instant>,
    /// Last trigger-eval result for test-hook + debugging.
    pub last_eval: Option<TriggerEvalResult>,
}

impl Default for PerRxState {
    fn default() -> Self {
        Self {
            recentering: false,
            flag_clear_at: None,
            last_eval: None,
        }
    }
}

/// Per-RX trigger-state pair (RX1, RX2) plus connection-wide flags.
#[derive(Debug, Default)]
pub struct CtunRecenterState {
    pub rx1: PerRxState,
    pub rx2: PerRxState,
    /// Set when the fork-extensions cap is first seen. One-off log-event.
    pub fork_active_logged: bool,
}

impl CtunRecenterState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Tick flag-clear timers; called periodically (≤200ms granularity).
    pub fn tick_flag_clear(&mut self, now: Instant) {
        for rx in [&mut self.rx1, &mut self.rx2] {
            if let Some(clear_at) = rx.flag_clear_at {
                if now >= clear_at {
                    rx.recentering = false;
                    rx.flag_clear_at = None;
                }
            }
        }
    }

    /// Per-RX accessor (rx_index: 0 = RX1, 1 = RX2).
    pub fn rx_mut(&mut self, rx_index: u8) -> &mut PerRxState {
        if rx_index == 0 { &mut self.rx1 } else { &mut self.rx2 }
    }

    pub fn rx(&self, rx_index: u8) -> &PerRxState {
        if rx_index == 0 { &self.rx1 } else { &self.rx2 }
    }
}

/// Trigger-evaluation result for logging + test-hook.
#[derive(Debug, Clone, Copy)]
pub struct TriggerEvalResult {
    pub decision: TriggerDecision,
    pub effective_zoom: f32,
    pub visible_span_hz: f64,
    pub threshold_hz: f64,
    pub vfo_hz: u64,
    pub ddc_center_hz: u64,
    pub ddc_bandwidth_hz: u32,
    pub dist_from_edge_hz: f64,
    pub ts: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerDecision {
    Trigger,
    SkipNoCap,         // fork-extensions cap not advertised -> feature off
    SkipZoomLow,       // effective_zoom <= 1.2 (formula unusable at low-zoom)
    SkipPtt,           // state.ptt = true -> defer (per operator-choice 2026-05-07 = B)
    SkipFlagged,       // recentering-burst still active
    SkipWithinZone,    // VFO safely within 60% headroom
    SkipHwInit,        // ddc_bandwidth == 0 (band-switch transient)
    SkipNoClients,     // 0 clients with spectrum_enabled -> trigger unnecessary
}

/// Trigger-formula (operator-specific, per PATCH-tl2-server-ctun-auto-recenter §1.3):
///
/// ```text
/// effective_zoom = min(zoom_rx) over all connected clients        // multi-client aggregation
/// visible_span   = ddc_bandwidth_rx / effective_zoom
/// threshold      = 0.6 × visible_span
/// trigger        = (ddc_bandwidth_rx/2 - abs(vfo_rx - ddc_center_rx)) < threshold
///                  AND effective_zoom > 1.2          (strict; low zoom unusable)
///                  AND ddc_bandwidth > 0             (band-switch transient guard)
///                  AND not state.ptt                 (PTT-defer)
///                  AND not flag.recentering          (per-RX flag)
///                  AND has_cap("auto_recenter_ex")
/// ```
///
/// Note: u64-vfo and u64-ddc_center are converted via i128-arith to
/// avoid overflow/underflow in `abs(vfo - ddc_center)`.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_trigger(
    rx_index: u8,
    has_cap_auto_recenter_ex: bool,
    effective_zoom: Option<f32>,
    vfo_hz: u64,
    ddc_center_hz: u64,
    ddc_bandwidth_hz: u32,
    state_ptt: bool,
    rx_state: &PerRxState,
) -> TriggerEvalResult {
    let now = Instant::now();
    let zoom_val = effective_zoom.unwrap_or(0.0);
    let visible_span = if zoom_val > 0.0 {
        ddc_bandwidth_hz as f64 / zoom_val as f64
    } else {
        0.0
    };
    let threshold = 0.6 * visible_span;

    // i128 signed-arith for abs(vfo - ddc_center) without overflow
    let dist = (vfo_hz as i128 - ddc_center_hz as i128).unsigned_abs() as f64;
    let dist_from_edge = (ddc_bandwidth_hz as f64 / 2.0) - dist;

    let decision = if !has_cap_auto_recenter_ex {
        TriggerDecision::SkipNoCap
    } else if effective_zoom.is_none() {
        TriggerDecision::SkipNoClients
    } else if ddc_bandwidth_hz == 0 {
        TriggerDecision::SkipHwInit
    } else if zoom_val <= 1.2 {
        TriggerDecision::SkipZoomLow
    } else if state_ptt {
        TriggerDecision::SkipPtt
    } else if rx_state.recentering {
        TriggerDecision::SkipFlagged
    } else if dist_from_edge < threshold {
        TriggerDecision::Trigger
    } else {
        TriggerDecision::SkipWithinZone
    };

    let result = TriggerEvalResult {
        decision,
        effective_zoom: zoom_val,
        visible_span_hz: visible_span,
        threshold_hz: threshold,
        vfo_hz,
        ddc_center_hz,
        ddc_bandwidth_hz,
        dist_from_edge_hz: dist_from_edge,
        ts: now,
    };

    // Trigger-eval log-point. Format: rx, ddc±bw, vfo, eff_zoom, threshold, dist_from_edge, decision.
    log::debug!(
        "TCI: ctun rx={} eval ddc={}±{} vfo={} eff_zoom={:.2} threshold={:.0} dist_from_edge={:.0} decision={:?}",
        rx_index,
        ddc_center_hz,
        ddc_bandwidth_hz / 2,
        vfo_hz,
        zoom_val,
        threshold,
        dist_from_edge,
        decision
    );

    let _ = rx_index; // not used in result, only for logging
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state() -> PerRxState {
        PerRxState::default()
    }

    fn eval(
        cap: bool,
        zoom: Option<f32>,
        vfo: u64,
        ddc_c: u64,
        ddc_bw: u32,
        ptt: bool,
        st: &PerRxState,
    ) -> TriggerDecision {
        evaluate_trigger(0, cap, zoom, vfo, ddc_c, ddc_bw, ptt, st).decision
    }

    #[test]
    fn trigger_zoom_below_1_2_skips() {
        let s = make_state();
        // zoom=1.0 (full DDC visible) -> skip
        assert_eq!(eval(true, Some(1.0), 14_000_000, 14_000_000, 384_000, false, &s), TriggerDecision::SkipZoomLow);
        // zoom=1.19999 (just under 1.2) -> skip
        assert_eq!(eval(true, Some(1.19999), 14_000_000, 14_000_000, 384_000, false, &s), TriggerDecision::SkipZoomLow);
        // zoom=1.2 exact -> strict gate skips
        assert_eq!(eval(true, Some(1.2), 14_000_000, 14_000_000, 384_000, false, &s), TriggerDecision::SkipZoomLow);
    }

    #[test]
    fn trigger_within_zone_skips() {
        let s = make_state();
        // DDC=384k, zoom=8 -> visible=48k, threshold=28.8k.
        // VFO at center: dist_from_edge = 192k > 28.8k -> SkipWithinZone
        let d = eval(true, Some(8.0), 14_000_000, 14_000_000, 384_000, false, &s);
        assert_eq!(d, TriggerDecision::SkipWithinZone);
    }

    #[test]
    fn trigger_at_edge_fires() {
        let s = make_state();
        // DDC=384k, zoom=8 -> threshold=28.8k. VFO 30k past center -> dist_from_edge=162k -> still safe.
        // Push VFO to 165k past center -> dist_from_edge=27k < 28.8k -> Trigger
        let d = eval(true, Some(8.0), 14_165_000, 14_000_000, 384_000, false, &s);
        assert_eq!(d, TriggerDecision::Trigger);
    }

    #[test]
    fn trigger_during_ptt_skips() {
        let s = make_state();
        // VFO far outside zone, but PTT active -> SkipPtt (defer; per operator-choice B)
        let d = eval(true, Some(8.0), 14_165_000, 14_000_000, 384_000, true, &s);
        assert_eq!(d, TriggerDecision::SkipPtt);
    }

    #[test]
    fn trigger_during_flag_skips() {
        let mut s = make_state();
        s.recentering = true;
        let d = eval(true, Some(8.0), 14_165_000, 14_000_000, 384_000, false, &s);
        assert_eq!(d, TriggerDecision::SkipFlagged);
    }

    #[test]
    fn trigger_at_hw_init_skips() {
        let s = make_state();
        // ddc_bw=0 (hardware-init transient after band-switch) -> SkipHwInit, no panic
        let d = eval(true, Some(8.0), 14_000_000, 14_000_000, 0, false, &s);
        assert_eq!(d, TriggerDecision::SkipHwInit);
    }

    #[test]
    fn trigger_per_rx_independent() {
        // Per-RX state is via PerRxState - RX1 flag set does not block RX2.
        let mut rx1 = make_state();
        rx1.recentering = true;
        let rx2 = make_state();
        // RX1 evaluation: SkipFlagged
        let d1 = eval(true, Some(8.0), 14_165_000, 14_000_000, 384_000, false, &rx1);
        // RX2 evaluation: Trigger (no flag)
        let d2 = eval(true, Some(8.0), 14_165_000, 14_000_000, 384_000, false, &rx2);
        assert_eq!(d1, TriggerDecision::SkipFlagged);
        assert_eq!(d2, TriggerDecision::Trigger);
    }

    #[test]
    fn trigger_no_cap_skips() {
        let s = make_state();
        let d = eval(false, Some(8.0), 14_165_000, 14_000_000, 384_000, false, &s);
        assert_eq!(d, TriggerDecision::SkipNoCap);
    }

    #[test]
    fn trigger_no_clients_skips() {
        let s = make_state();
        // effective_zoom = None (no clients) -> SkipNoClients
        let d = eval(true, None, 14_000_000, 14_000_000, 384_000, false, &s);
        assert_eq!(d, TriggerDecision::SkipNoClients);
    }

    #[test]
    fn formula_no_overflow_at_extremes() {
        let s = make_state();
        // VFO at the u64::MAX boundary, ddc_center at 0 -> i128-arith prevents overflow
        // Result: huge dist > threshold -> SkipWithinZone (no panic)
        let d = eval(true, Some(8.0), u64::MAX, 0, 384_000, false, &s);
        // No panic = test passes; decision is irrelevant but must be defined
        assert!(matches!(d, TriggerDecision::SkipWithinZone | TriggerDecision::Trigger));
    }

    #[test]
    fn flag_clear_tick_works() {
        let mut state = CtunRecenterState::new();
        let now = Instant::now();
        state.rx1.recentering = true;
        state.rx1.flag_clear_at = Some(now);
        state.tick_flag_clear(now + std::time::Duration::from_millis(1));
        assert!(!state.rx1.recentering);
        assert!(state.rx1.flag_clear_at.is_none());
    }
}
