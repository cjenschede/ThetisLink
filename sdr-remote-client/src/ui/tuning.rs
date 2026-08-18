// SPDX-License-Identifier: GPL-2.0-or-later
//! Tuning / frequency helpers for `SdrRemoteApp`: spectrum-center math, the
//! full-tuning-force + tuning-latch predicates, and the pending-frequency setters /
//! Yaesu frequency acceptance (optimistic-set + reconcile). Extracted verbatim from
//! `ui/mod.rs` - pure relocation, no behaviour change (no timing change). `pub(super)`
//! keeps them callable from the parent module tree.

use super::*;

/// How far a spectrum view may be panned, as a fraction of the full span.
///
/// One rule for RX1, RX2 and both VRX windows, because a pan slider that means
/// something different per window is four sliders to learn. Two limits, and the
/// tighter one wins:
///
/// * **Never more than the window you are looking at.** The pan range is the
///   visible span itself - half a screen either way - so the slider always
///   means the same thing in screens, and more zoom is less pan in Hz. That is
///   what makes it usable at 50x, where the old range walked the view clean off
///   the display.
/// * **Never past the edge of the spectrum there is.** The view has to stay
///   inside the data, so at low zoom - where the window nearly fills the span -
///   there is barely any room to pan, and at 1x none at all. Below about 2x
///   this is the limit that binds, and the pan then cannot reach a whole
///   window's worth. That is not a defect: there is nothing beyond the edge to
///   pan to.
///
/// Replaces a flat `* 0.05` factor that had no relation to either the span or
/// the zoom: at 8x it allowed a fifth of what fits, at 32x it allowed more than
/// the window was wide.
pub(super) fn max_pan_fraction(zoom: f32) -> f32 {
    if !(zoom > 1.01) {
        return 0.0;
    }
    let one_screen = 0.5 / zoom;
    let to_the_edge = 0.5 - 0.5 / zoom;
    one_screen.min(to_the_edge)
}

impl SdrRemoteApp {
    /// Where the middle of the view belongs: the VFO, shifted by the pan.
    ///
    /// This used to be two different quantities behind one name. With the
    /// receiver's width known it was the VFO plus the pan, computed here and
    /// current with the tuning. Without it, it fell back to the view centre the
    /// *server* reported - a different number, from a different source, one
    /// round trip later, and with the pan left out altogether.
    ///
    /// The marker meanwhile always came from the local VFO, immediately. So in
    /// the second regime the marker moved and the centre did not, which put the
    /// line off centre by exactly the amount just tuned until the next packet
    /// arrived and it snapped back - and left it standing wrong against the pan
    /// point in the meantime. Three field reports say precisely that
    /// (2026-08-14).
    ///
    /// Which regime you were in depended on whether a full-band row had ever
    /// arrived, and that was a race at connect: the server starts a session
    /// with the full-band row on and the client sends its own preference only
    /// after the handshake. One row inside that window latched the good regime
    /// for the whole session - and the row arrives only if the receiver is
    /// already streaming when the client connects. Restart the server and
    /// connect straight away and it is the bad one; leave the radio running and
    /// it is the good one. Hence the same build behaving differently, and hence
    /// four repairs aimed at the value rather than the regime.
    ///
    /// There is one regime now. The caller supplies the best width it has - the
    /// full-band span when there is one, the DDC rate otherwise - and with no
    /// width at all this returns the VFO, which is the right answer when there
    /// is no pan and the only sensible one when there is.
    pub(super) fn spectrum_target_center_hz(vfo_hz: u64, full_span_hz: u32, pan: f32) -> f64 {
        vfo_hz as f64 + pan as f64 * full_span_hz as f64
    }

    /// The receiver's full width, from the live rate first and the remembered
    /// full-band row only if there is no live rate.
    ///
    /// The two are the same quantity: the server fills the full-band row's
    /// `span_hz` with the DDC sample rate. So the remembered one is at best
    /// equal and at worst out of date - `full_spectrum_span_hz` is never
    /// cleared when the option is switched off, and it outlives the stream that
    /// set it.
    ///
    /// It was the other way round until the review pointed at it (2026-08-15).
    /// Switch the full band on at 384 kHz, off again, then change the DDC rate
    /// to 192: the client would go on scaling the pan by 384 while the server
    /// used 192. That is the same class of fault as the one this file just
    /// fixed - a width from a source that is no longer live - arrived at from
    /// the other direction.
    pub(super) fn full_span_hz(ddc_rate_khz: u16, latched_span_hz: u32) -> u32 {
        if ddc_rate_khz > 0 {
            ddc_rate_khz as u32 * 1000
        } else {
            latched_span_hz
        }
    }

    /// The receiver's full width for RX1, however it is known.
    pub(super) fn rx1_full_span_hz(&self) -> u32 {
        Self::full_span_hz(self.ddc_sample_rate_rx1, self.full_spectrum_span_hz)
    }

    /// The same for RX2.
    pub(super) fn rx2_full_span_hz(&self) -> u32 {
        Self::full_span_hz(self.ddc_sample_rate_rx2, self.rx2_full_spectrum_span_hz)
    }

    /// What a new session does to the view, in one place.
    ///
    /// This existed twice - once for the automatic reconnect and once behind
    /// the Connect button - and the two drifted the moment one was changed.
    /// Taking the pan reset out of the first copy while the second kept it
    /// made the zoom survive a restart and the pan not, because pressing
    /// Connect is how a restart gets back in (2026-08-15).
    ///
    /// What goes is the previous session's full-band picture, which belongs to
    /// a band this session may not be on. What stays is the operator's zoom
    /// and pan; only the last-sent markers are cleared, and clearing those
    /// forces the view that is held to be sent to the server again.
    pub(super) fn reset_view_for_new_session(&mut self) {
        self.full_spectrum_span_hz = 0;
        self.full_spectrum_bins.clear();
        self.last_sent_zoom = 0.0;
        self.last_sent_pan = 0.0;
        self.zoom_pan_changed_at = Some(Instant::now());
        self.rx2_full_spectrum_span_hz = 0;
        self.rx2_full_spectrum_bins.clear();
        self.rx2_last_sent_zoom = 0.0;
        self.rx2_last_sent_pan = 0.0;
        self.rx2_zoom_pan_changed_at = Some(Instant::now());
    }

    /// Match RX1 and VRX1 to the width they are looking at, once a session.
    ///
    /// VRX rides RX's DDC, so its opening zoom comes from the same number. It
    /// had no derivation of its own: it borrowed one from the full-band block,
    /// which for VRX1 has been unreachable since build 79 - the RX1 branch sets
    /// the flag that guards it long before a full-band row can arrive. That was
    /// invisible while the value came out of the config file, and would have
    /// opened VRX1 at a hard-coded 32x the moment it stopped (2026-08-15).
    ///
    /// One method per receiver because the derivation has two callers - the
    /// live DDC rate and the full-band fallback - and this is the third time
    /// tonight that two copies of one decision drifted apart.
    pub(super) fn adopt_opening_zoom_rx1(&mut self, span_hz: u32) {
        self.rx_zoom_initialized = true;
        self.spectrum_zoom = default_zoom_for_span(span_hz);
        self.spectrum_pan = 0.0;
        // Force the send: the server is on whatever it was told last.
        self.last_sent_zoom = 0.0;
        self.last_sent_pan = 0.0;
        self.zoom_pan_changed_at = Some(Instant::now());
        if !self.vrx1_zoom_initialized {
            self.vrx1_zoom_initialized = true;
            self.vrx1_spectrum_zoom = default_zoom_for_span(span_hz);
            self.vrx1_pan = 0.0;
        }
    }

    /// The same for RX2 and VRX2.
    pub(super) fn adopt_opening_zoom_rx2(&mut self, span_hz: u32) {
        self.rx2_zoom_initialized = true;
        self.rx2_spectrum_zoom = default_zoom_for_span(span_hz);
        self.rx2_spectrum_pan = 0.0;
        self.rx2_last_sent_zoom = 0.0;
        self.rx2_last_sent_pan = 0.0;
        self.rx2_zoom_pan_changed_at = Some(Instant::now());
        if !self.vrx2_zoom_initialized {
            self.vrx2_zoom_initialized = true;
            self.vrx2_spectrum_zoom = default_zoom_for_span(span_hz);
            self.vrx2_pan = 0.0;
        }
    }

    /// Whether the opening zoom should be derived from the receiver's width.
    ///
    /// Three conditions, and each one has been wrong in the field at least
    /// once, which is why they live here rather than in a line of `if`:
    ///
    /// - a live link, because a stale rate derived at disconnect put the slider
    ///   and the picture on different numbers;
    /// - not done yet this session, because the flag used to be cleared on
    ///   disconnect and every reconnect then threw away the operator's zoom;
    /// - a rate to derive from, because zero means the width is not known and
    ///   the fallback is not the receiver's.
    pub(super) fn should_derive_opening_zoom(
        connected: bool,
        already_initialized: bool,
        ddc_rate_khz: u16,
    ) -> bool {
        connected && !already_initialized && ddc_rate_khz > 0
    }

    /// Where the width in the log line actually came from.
    ///
    /// This has to follow `full_span_hz` exactly, because the point of writing
    /// it down is to know which regime a session ran in. It did not: it read
    /// the two full-band fields and never looked at the live rate, so a session
    /// holding both said "a full-band row that arrived earlier" while the width
    /// came from the DDC rate. Both were 384 kHz, so the line was not wrong
    /// about the number and could not be caught by reading it - it sent the
    /// author of this line looking for a rate that was there all along
    /// (2026-08-15).
    pub(super) fn full_span_source(
        ddc_rate_khz: u16,
        latched_span_hz: u32,
        full_spectrum_enabled: bool,
    ) -> &'static str {
        match (ddc_rate_khz > 0, full_spectrum_enabled, latched_span_hz > 0) {
            (true, _, _) => "the DDC rate",
            (false, true, true) => "the full-band row, which is on",
            (false, false, true) => "a full-band row that arrived earlier (option now off)",
            (false, _, false) => "nowhere - no width known yet",
        }
    }

    /// Whether the view disagreeing with what was asked for is worth saying.
    ///
    /// Three conditions with one answer, which is exactly the shape that has no
    /// business sitting inline in a draw call. It went out twice and was wrong
    /// both times about *when* to speak: first it had no notion of a change
    /// still being on its way, and fired twice in ten seconds of ordinary
    /// zooming; the diagnostic beside it hung off a latch that the operator's
    /// way of tuning never engages, so it never spoke at all. Both were one
    /// condition each, and both reached the field because neither could be
    /// tested without a running client (2026-08-15).
    ///
    /// - `disagrees`: a tenth is far wider than the rounding in the extraction
    ///   and far narrower than one step of the zoom slider.
    /// - `quiet`: it is a standing condition, not an event; once every two
    ///   seconds is enough to notice it.
    /// - `settled`: between sending a zoom and the packet that reflects it the
    ///   two ends differ for a round trip. That is not a disagreement, it is
    ///   the operator moving a control.
    pub(super) fn view_mismatch_worth_reporting(
        expected_span_hz: f64,
        got_span_hz: f64,
        since_last_report_ms: u128,
        since_own_send_ms: u128,
        change_pending: bool,
    ) -> bool {
        if expected_span_hz <= 0.0 || got_span_hz <= 0.0 {
            return false;
        }
        let disagrees = (expected_span_hz - got_span_hz).abs() > expected_span_hz * 0.10;
        let quiet = since_last_report_ms >= 2000;
        let settled = since_own_send_ms >= 1500 && !change_pending;
        disagrees && quiet && settled
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
        let target_center =
            Self::spectrum_target_center_hz(freq, self.rx1_full_span_hz(), self.spectrum_pan);
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
        let target_center =
            Self::spectrum_target_center_hz(freq, self.rx2_full_span_hz(), self.rx2_spectrum_pan);
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

#[cfg(test)]
mod pan_range_tests {
    use super::max_pan_fraction;
    use crate::ui::SdrRemoteApp;

    /// The centre must not change meaning depending on where the width came
    /// from. It used to: with a full-band row it was the VFO plus the pan, and
    /// without one it was the view centre the server reported - a different
    /// number, a round trip late, with the pan dropped. That difference is the
    /// whole fault, so this is the test that holds it shut.
    #[test]
    fn the_centre_is_the_vfo_shifted_by_the_pan() {
        let vfo = 14_200_000u64;
        let width = 384_000u32;
        assert_eq!(
            SdrRemoteApp::spectrum_target_center_hz(vfo, width, 0.25),
            vfo as f64 + 0.25 * width as f64
        );
    }

    /// With no pan the centre is the VFO exactly - which is what keeps the
    /// marker still in the middle while tuning.
    #[test]
    fn without_pan_the_centre_is_the_vfo() {
        let vfo = 7_100_000u64;
        assert_eq!(SdrRemoteApp::spectrum_target_center_hz(vfo, 384_000, 0.0), vfo as f64);
    }

    /// The pan moves it by its share of the receiver, both ways and evenly.
    #[test]
    fn the_pan_moves_the_centre_both_ways() {
        let vfo = 7_100_000u64;
        let width = 192_000u32;
        let right = SdrRemoteApp::spectrum_target_center_hz(vfo, width, 0.1);
        let left = SdrRemoteApp::spectrum_target_center_hz(vfo, width, -0.1);
        assert!((right - vfo as f64 - 19_200.0).abs() < 1.0, "right: {right}");
        assert!((left - vfo as f64 + 19_200.0).abs() < 1.0, "left: {left}");
    }

    /// No width known yet - before the first packet of a session - must still
    /// give a usable answer rather than reaching for the server's number, which
    /// is what the old fallback did and what put the marker out of the middle.
    #[test]
    fn with_no_width_the_centre_falls_back_to_the_vfo() {
        let vfo = 3_694_000u64;
        assert_eq!(SdrRemoteApp::spectrum_target_center_hz(vfo, 0, 0.3), vfo as f64);
    }


    /// The live rate wins over the width remembered from a full-band row. They
    /// are the same quantity, so the remembered one can only be stale - and it
    /// is never cleared when the option goes off.
    #[test]
    fn the_live_rate_beats_the_remembered_one() {
        // Full band was on at 384 kHz once; the receiver is on 192 kHz now.
        assert_eq!(SdrRemoteApp::full_span_hz(192, 384_000), 192_000);
    }

    /// With no live rate the remembered width is better than nothing.
    #[test]
    fn without_a_live_rate_the_remembered_width_is_used() {
        assert_eq!(SdrRemoteApp::full_span_hz(0, 384_000), 384_000);
    }

    /// And with neither, zero - which `spectrum_target_center_hz` turns into
    /// "the centre is the VFO", the safe answer.
    #[test]
    fn with_neither_there_is_no_width() {
        assert_eq!(SdrRemoteApp::full_span_hz(0, 0), 0);
    }

    /// The first connection of a session, which is the one time this may run.
    #[test]
    fn the_width_is_matched_once_when_the_link_comes_up() {
        assert!(SdrRemoteApp::should_derive_opening_zoom(true, false, 384));
    }

    /// The operator's zoom outlives a reconnect. Clearing the flag on
    /// disconnect is what used to throw it away.
    #[test]
    fn a_reconnect_does_not_derive_it_again() {
        assert!(!SdrRemoteApp::should_derive_opening_zoom(true, true, 384));
    }

    /// A dropped link leaves the rate behind. Deriving off it put the slider
    /// and the frozen picture on different numbers.
    #[test]
    fn a_stale_rate_on_a_dead_link_derives_nothing() {
        assert!(!SdrRemoteApp::should_derive_opening_zoom(false, false, 384));
        assert!(!SdrRemoteApp::should_derive_opening_zoom(false, true, 384));
    }

    /// No rate is not a width, and the fallback is not the receiver's.
    #[test]
    fn without_a_rate_there_is_nothing_to_derive_from() {
        assert!(!SdrRemoteApp::should_derive_opening_zoom(true, false, 0));
    }

    /// The case that misled a reader: both sources present, and the line has
    /// to name the one the width was actually taken from.
    #[test]
    fn a_live_rate_is_named_even_when_a_latched_span_is_also_there() {
        assert_eq!(
            SdrRemoteApp::full_span_source(384, 384_000, false),
            "the DDC rate"
        );
        assert_eq!(
            SdrRemoteApp::full_span_source(384, 384_000, true),
            "the DDC rate"
        );
    }

    #[test]
    fn without_a_live_rate_the_latched_span_is_named_with_its_option_state() {
        assert_eq!(
            SdrRemoteApp::full_span_source(0, 384_000, true),
            "the full-band row, which is on"
        );
        assert_eq!(
            SdrRemoteApp::full_span_source(0, 384_000, false),
            "a full-band row that arrived earlier (option now off)"
        );
    }

    #[test]
    fn with_no_width_at_all_the_line_says_so() {
        assert_eq!(
            SdrRemoteApp::full_span_source(0, 0, false),
            "nowhere - no width known yet"
        );
    }

    /// The invariant that makes the line worth reading at all: it names the
    /// live rate exactly when `full_span_hz` returns the live rate.
    #[test]
    fn the_source_never_disagrees_with_the_width() {
        for ddc in [0u16, 96, 192, 384, 1536] {
            for latched in [0u32, 192_000, 384_000] {
                for enabled in [false, true] {
                    let width = SdrRemoteApp::full_span_hz(ddc, latched);
                    let source = SdrRemoteApp::full_span_source(ddc, latched, enabled);
                    let from_ddc = source == "the DDC rate";
                    assert_eq!(
                        from_ddc,
                        ddc > 0 && width == ddc as u32 * 1000,
                        "ddc={ddc} latched={latched} enabled={enabled} said {source}"
                    );
                }
            }
        }
    }

    /// The mismatch is real, nothing was sent recently, nothing is pending:
    /// this is the case the check exists for.
    #[test]
    fn a_settled_disagreement_is_reported() {
        assert!(SdrRemoteApp::view_mismatch_worth_reporting(
            48_000.0, 384_000.0, 5_000, 5_000, false
        ));
    }

    /// The first of the two field faults: the operator moving the zoom slider
    /// is not a disagreement, however different the two numbers are while the
    /// change is in flight. Either half of `settled` must suppress it.
    #[test]
    fn our_own_change_in_flight_is_not_a_disagreement() {
        assert!(
            !SdrRemoteApp::view_mismatch_worth_reporting(48_000.0, 384_000.0, 5_000, 200, false),
            "a send 200 ms ago is still on its way there"
        );
        assert!(
            !SdrRemoteApp::view_mismatch_worth_reporting(48_000.0, 384_000.0, 5_000, 5_000, true),
            "a change of our own is still queued"
        );
    }

    /// It is a standing condition, not an event: having just said so, stay
    /// quiet for a while.
    #[test]
    fn it_does_not_repeat_itself() {
        assert!(!SdrRemoteApp::view_mismatch_worth_reporting(
            48_000.0, 384_000.0, 500, 5_000, false
        ));
    }

    /// Agreement is silence, and so is a span that has not arrived.
    #[test]
    fn agreement_says_nothing() {
        assert!(!SdrRemoteApp::view_mismatch_worth_reporting(
            48_000.0, 48_000.0, 5_000, 5_000, false
        ));
        // Within a tenth - the extraction rounds, and that is not news.
        assert!(!SdrRemoteApp::view_mismatch_worth_reporting(
            48_000.0, 50_000.0, 5_000, 5_000, false
        ));
        assert!(!SdrRemoteApp::view_mismatch_worth_reporting(
            48_000.0, 0.0, 5_000, 5_000, false
        ));
    }

    /// At 1x the window is the whole span: there is nowhere to pan to.
    #[test]
    fn no_pan_when_everything_is_already_visible() {
        assert_eq!(max_pan_fraction(1.0), 0.0);
        assert_eq!(max_pan_fraction(0.5), 0.0);
        assert_eq!(max_pan_fraction(1.01), 0.0);
    }

    /// Above 2x the window rule binds: half a screen either way, so the range
    /// in Hz shrinks as the zoom grows. More zoom, less pan.
    #[test]
    fn more_zoom_means_less_pan() {
        let at4 = max_pan_fraction(4.0);
        let at8 = max_pan_fraction(8.0);
        let at32 = max_pan_fraction(32.0);
        assert!((at4 - 0.125).abs() < 1e-6, "half of a quarter-span window");
        assert!(at8 < at4 && at32 < at8, "each step in tightens the range");
        // Half the visible window, exactly.
        for z in [4.0f32, 8.0, 32.0, 200.0] {
            assert!((max_pan_fraction(z) - 0.5 / z).abs() < 1e-6);
        }
    }

    /// Below 2x the edge rule binds: the view cannot leave the data, so the pan
    /// cannot reach a whole window's worth. Nothing is wrong with that - there
    /// is nothing beyond the edge to look at.
    #[test]
    fn low_zoom_cannot_reach_a_full_window() {
        let z = 1.2f32;
        let range = max_pan_fraction(z);
        assert!((range - (0.5 - 0.5 / z)).abs() < 1e-6, "the edge is the limit here");
        assert!(range < 0.5 / z, "and it is tighter than half a window");
    }

    /// The two rules meet at 2x, and neither ever puts the view past the edge.
    #[test]
    fn the_view_always_stays_inside_the_span() {
        for z in [1.05f32, 1.5, 2.0, 3.0, 8.0, 64.0, 1024.0] {
            let pan = max_pan_fraction(z);
            let half_window = 0.5 / z;
            assert!(
                pan + half_window <= 0.5 + 1e-6,
                "at {z}x the far edge of the view lands outside the spectrum"
            );
        }
        assert!((max_pan_fraction(2.0) - 0.25).abs() < 1e-6, "the two rules meet at 2x");
    }
}
