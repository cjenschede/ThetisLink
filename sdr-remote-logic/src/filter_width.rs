// SPDX-License-Identifier: GPL-2.0-or-later
//! The RX filter width buttons: which width comes next, where the edges go, and
//! what the label says.
//!
//! One rule for the desktop and Android. Until build 47 it existed twice - in
//! the desktop helpers and in Kotlin - and the desktop wrote the button row out
//! three times. Both copies kept the low edge and added the width on top.

use std::time::{Duration, Instant};

/// SSB width steps, also used for DIGU/DIGL and SPEC.
pub const SSB_STEPS: &[i32] = &[1800, 2100, 2400, 2700, 3000, 3300, 3600, 4000];
/// CW width steps.
pub const CW_STEPS: &[i32] = &[50, 100, 250, 500, 1000];
/// AM/SAM/DSB/DRM width steps.
pub const AM_STEPS: &[i32] = &[4000, 6000, 8000, 10000, 12000];

/// Which way a button moves the width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Narrower,
    Wider,
}

/// Width steps for a Thetis DSP mode. FM has none: Thetis derives the FM filter
/// from the deviation and ignores the edges it is sent.
pub fn steps_for_mode(mode: u8) -> &'static [i32] {
    match mode {
        5 => &[],
        3 | 4 => CW_STEPS,
        2 | 6 | 10 | 11 => AM_STEPS,
        _ => SSB_STEPS,
    }
}

/// A width within this fraction of a step counts as that step (1/50 = 2 %).
/// Without it a filter that is one hertz off a step needs a press that changes
/// nothing. It has to stay well below the gap between neighbouring filters an
/// operator tells apart: 5 % was tried first and swallowed Thetis' own 3.8 kHz
/// preset into the 4.0 step, which disabled plus there.
const SAME_STEP_DIVISOR: i32 = 50;

/// The low cut Thetis' own "Default Low Cut" may take, in Setup: 0..=500 Hz.
const THETIS_LOW_CUT_RANGE: std::ops::RangeInclusive<i32> = 0..=500;

/// The low cut used when the current one is outside that range: the Setup
/// default, and the low edge of the stock SSB presets in both field logs.
const THETIS_DEFAULT_LOW_CUT: i32 = 100;

fn next_width(steps: &[i32], width: i32, dir: Step) -> Option<i32> {
    match dir {
        Step::Wider => steps
            .iter()
            .copied()
            .find(|&p| p > width + p / SAME_STEP_DIVISOR),
        Step::Narrower => steps
            .iter()
            .rev()
            .copied()
            .find(|&p| p < width - p / SAME_STEP_DIVISOR),
    }
}

/// New (low, high) for one press, or `None` when there is no step that way.
///
/// The width comes from the filter Thetis really has, not from the nearest step,
/// so a filter between two steps goes to the next one in the direction pressed.
///
/// Where the edges go follows Thetis' own width control (`console.cs`,
/// `ptbFilterWidth_Scroll`), with one difference for SSB:
///
/// - **USB/LSB** keep the low cut and move the other edge - as every build before
///   47 did - but only while the low cut is one Thetis could have as its default
///   (0..=500 Hz). Outside that range it is replaced by 100 Hz. Thetis' control
///   always resets it; keeping a normal one is what an operator expects from a
///   width button. Replacing an abnormal one is what ends the lockup of report 74:
///   with the low cut kept at 9630, every width landed above Thetis' clip at
///   10000 and came back unchanged.
/// - **CW, DIGU, DIGL** keep the centre, pushed back onto their own side of the
///   carrier if the new width would cross it. DIGU/DIGL used to share the USB
///   rule; a digital filter lives above 500 Hz, so that rule would now move it.
/// - **AM, SAM, DSB, DRM, SPEC** are symmetric around the carrier.
/// - **FM** has no steps.
///
/// Why not "keep the low cut and grow downwards when the top is clipped": Thetis
/// clips at `MaxFilterShift`/`MaxFilterWidth`, both Setup values that TCI does
/// not report, so TL cannot know where the top is.
pub fn step(mode: u8, low: i32, high: i32, dir: Step) -> Option<(i32, i32)> {
    let width = high - low;
    if width <= 0 {
        return None;
    }
    let bw = next_width(steps_for_mode(mode), width, dir)?;
    Some(match mode {
        1 => {
            let l = if THETIS_LOW_CUT_RANGE.contains(&low) { low } else { THETIS_DEFAULT_LOW_CUT };
            (l, l + bw)
        }
        0 => {
            let h = if THETIS_LOW_CUT_RANGE.contains(&-high) { high } else { -THETIS_DEFAULT_LOW_CUT };
            (h - bw, h)
        }
        4 | 7 => {
            let l = (low + high) / 2 - bw / 2;
            if l < 0 { (0, bw) } else { (l, l + bw) }
        }
        3 | 9 => {
            let l = (low + high) / 2 - bw / 2;
            let h = l + bw;
            if h > 0 { (-bw, 0) } else { (l, h) }
        }
        _ => (-bw / 2, bw / 2),
    })
}

/// Whether filter edges fit a Thetis DSP mode - one rule for the client and the
/// server, in the core (build 244).
pub use sdr_remote_core::filter_fit::edges_fit_mode;

/// Where a dragged filter edge goes, for RX1 and RX2 alike (build 245; each had
/// its own copy in the client).
///
/// - **AM, SAM, DSB, FM, DRM** mirror the edge to both sides: Thetis forces
///   symmetry in these modes (it adopts the wider edge for both).
/// - **SSB, CW, DIG** stop the edge at the carrier, so a pair never crosses it:
///   the server sends Thetis only edges that fit the mode (build 244), and every
///   frame of a drag past the carrier was a refused pair and a log line.
/// - **SPEC** takes the edge as it is.
///
/// An edge dragged across the other one, or a mirror to zero width, leaves the
/// filter as it was.
pub fn drag_edge(mode: u8, low: i32, high: i32, is_low: bool, hz: i32) -> (i32, i32) {
    if !matches!(mode, 0 | 1 | 3 | 4 | 7 | 8 | 9) {
        let w = hz.abs();
        return if w > 0 { (-w, w) } else { (low, high) };
    }
    let hz = match mode {
        0 | 3 | 9 => hz.min(0),
        1 | 4 | 7 => hz.max(0),
        _ => hz,
    };
    let (l, h) = if is_low { (hz, high) } else { (low, hz) };
    if h > l { (l, h) } else { (low, high) }
}

/// A width as the operator reads it: hertz for CW and below 1 kHz, otherwise
/// kilohertz with at most two decimals and no trailing zeros ("2.7 kHz",
/// "2.42 kHz", "10 kHz"). `--` when there is no width to show.
pub fn format_width_hz(hz: i32, cw: bool) -> String {
    if hz <= 0 {
        return "--".to_string();
    }
    if cw || hz < 1000 {
        return format!("{} Hz", hz);
    }
    let khz = format!("{:.2}", hz as f64 / 1000.0);
    format!("{} kHz", khz.trim_end_matches('0').trim_end_matches('.'))
}

/// The label between the buttons: the width Thetis has, not the nearest step.
pub fn width_label(mode: u8, low: i32, high: i32) -> String {
    format_width_hz(high - low, matches!(mode, 3 | 4))
}

/// How long a filter the client just sent outranks the one the server reports.
///
/// Long enough for the change to go to Thetis and come back, so the label does
/// not flick back to the old value in between. The TCI controls in the desktop
/// client hold for the same time (`sync_state.rs`, `tci_control_changed_at`).
pub const LOCAL_CHANGE_HOLD: Duration = Duration::from_millis(1000);

/// Whether the client should take the filter the server reports.
///
/// Until build 47 the desktop held its own filter until the mode changed, so
/// after one press it stopped following Thetis altogether - and wrote its stale
/// low edge back with the next press.
pub fn accept_server_filter(local_change_at: Option<Instant>, now: Instant) -> bool {
    match local_change_at {
        None => true,
        Some(t) => now.saturating_duration_since(t) >= LOCAL_CHANGE_HOLD,
    }
}

#[cfg(test)]
mod drag_tests {
    use super::drag_edge;
    use sdr_remote_core::filter_fit::edges_fit_mode;

    /// A dragged edge stops at the carrier in SSB, CW and DIG, mirrors in the
    /// symmetric modes, and never makes a pair the server refuses (build 245).
    #[test]
    fn a_dragged_edge_never_crosses_the_carrier() {
        assert_eq!(drag_edge(1, 100, 2900, true, -300), (0, 2900), "USB low edge stops at the carrier");
        assert_eq!(drag_edge(0, -2900, -100, false, 50), (-2900, 0), "LSB high edge stops at the carrier");
        assert_eq!(drag_edge(4, 250, 750, true, -100), (0, 750), "CW-U");
        assert_eq!(drag_edge(9, -3000, -300, false, 200), (-3000, 0), "DIGL");
        assert_eq!(drag_edge(1, 100, 2900, true, 300), (300, 2900), "an ordinary drag");
        assert_eq!(drag_edge(1, 100, 2900, true, 3000), (100, 2900), "across the other edge: unchanged");
        assert_eq!(drag_edge(6, -5000, 5000, false, 3000), (-3000, 3000), "AM mirrors");
        assert_eq!(drag_edge(10, -5000, 5000, true, -4000), (-4000, 4000), "SAM mirrors");
        assert_eq!(drag_edge(6, -5000, 5000, true, 0), (-5000, 5000), "no width: unchanged");
        assert_eq!(drag_edge(8, -1000, 4000, true, -2000), (-2000, 4000), "SPEC as it is");
        // Whatever the drag, a fitting filter stays fitting.
        for mode in 0u8..=11 {
            if mode == 5 || mode == 8 { continue; }
            let (lo, hi) = match mode { 0 | 3 | 9 => (-2900, -100), 1 | 4 | 7 => (100, 2900), _ => (-3000, 3000) };
            for hz in [-20_000, -3000, -50, 0, 50, 3000, 20_000] {
                for is_low in [true, false] {
                    let (l, h) = drag_edge(mode, lo, hi, is_low, hz);
                    assert!(edges_fit_mode(mode, l, h), "mode {mode}, {is_low} to {hz}: {l}..{h}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USB: u8 = 1;
    const LSB: u8 = 0;
    const CWU: u8 = 4;
    const CWL: u8 = 3;
    const AM: u8 = 6;
    const FM: u8 = 5;
    const DIGU: u8 = 7;
    const DIGL: u8 = 9;

    /// Thetis' own clip on the edges it is sent (`ConstrainFilter`, both limits
    /// at their Setup default of 10000). Only the high edge matters for these
    /// tests; the low edge is within range in all of them.
    fn thetis(low: i32, high: i32) -> (i32, i32) {
        (low.max(-10000), high.min(10000))
    }

    /// Press a button the way the field did: send, let Thetis clip, read back.
    fn press(mode: u8, edges: (i32, i32), dir: Step) -> Option<(i32, i32)> {
        step(mode, edges.0, edges.1, dir).map(|(l, h)| thetis(l, h))
    }

    // ---- the reports -------------------------------------------------------

    /// PD0PLK, report 74: low edge 9630, high clipped at 10000. Android showed
    /// 1.8 kHz, minus was disabled and plus sent 9630 .. 11730 every time.
    #[test]
    fn report_74_plus_is_not_a_fixed_point() {
        let before = (9630, 10000);
        let after = press(USB, before, Step::Wider).expect("plus must be possible");
        assert_ne!(after, before, "plus changed nothing - the lockup of report 74");
    }

    /// The owner's reproductions on 2026-09-15, with the low edges TL received.
    /// Every one of them had a plus press that Thetis turned back into the
    /// state it came from.
    #[test]
    fn owner_reproductions_never_stall_on_plus() {
        for low in [6999, 7579, 7699, 9029] {
            let mut edges = thetis(low, low + 1000);
            let mut presses = 0;
            while let Some(next) = press(USB, edges, Step::Wider) {
                assert_ne!(next, edges, "low {}: plus stalled at {:?}", low, edges);
                edges = next;
                presses += 1;
                assert!(presses < 20, "low {}: plus never reaches the widest step", low);
            }
            assert_eq!(edges.1 - edges.0, 4000, "low {}: did not end on the widest SSB step", low);
        }
    }

    /// The mirror image in LSB.
    #[test]
    fn lsb_far_from_the_carrier_does_not_stall() {
        let mut edges = (-10000, -9030);
        let mut presses = 0;
        while let Some(next) = press(LSB, edges, Step::Wider) {
            assert_ne!(next, edges, "plus stalled at {:?}", edges);
            edges = next;
            presses += 1;
            assert!(presses < 20);
        }
        assert_eq!(edges.1 - edges.0, 4000);
    }

    // ---- what must not change for a normal filter ------------------------

    /// A normal SSB filter keeps its low cut; only the top moves. This is what
    /// every build before 47 did and what an operator expects.
    #[test]
    fn normal_usb_keeps_its_low_cut() {
        assert_eq!(step(USB, 100, 2800, Step::Wider), Some((100, 3100)));
        assert_eq!(step(USB, 100, 2800, Step::Narrower), Some((100, 2500)));
        assert_eq!(step(USB, 300, 2700, Step::Wider), Some((300, 3000)));
        assert_eq!(step(USB, 0, 2400, Step::Wider), Some((0, 2700)));
        assert_eq!(step(USB, 500, 2900, Step::Wider), Some((500, 3200)));
    }

    #[test]
    fn normal_lsb_keeps_its_low_cut() {
        assert_eq!(step(LSB, -2800, -100, Step::Wider), Some((-3100, -100)));
        assert_eq!(step(LSB, -2800, -100, Step::Narrower), Some((-2500, -100)));
        assert_eq!(step(LSB, -2900, -500, Step::Wider), Some((-3200, -500)));
    }

    // ---- where the low cut comes from when it has to be replaced ---------

    /// Outside 0..=500 Hz - the range Thetis' own "Default Low Cut" may take -
    /// the low cut is replaced by 100 Hz, the Setup default, which is also the
    /// low edge of the stock SSB presets seen in both field logs.
    #[test]
    fn a_low_cut_outside_thetis_range_goes_back_to_100() {
        assert_eq!(step(USB, 9630, 10000, Step::Wider), Some((100, 1900)));
        assert_eq!(step(USB, 501, 2900, Step::Wider), Some((100, 2800)));
        assert_eq!(step(USB, -109, 2897, Step::Wider), Some((100, 3400)));
        assert_eq!(step(LSB, -10000, -9030, Step::Wider), Some((-1900, -100)));
        assert_eq!(step(LSB, -2900, 109, Step::Wider), Some((-3400, -100)));
    }

    // ---- the steps follow the width Thetis really has --------------------

    /// A width between two steps goes to the next step in the direction of the
    /// press, and a width within 2 % of a step counts as that step - otherwise
    /// a filter one hertz off would need a press that changes nothing.
    #[test]
    fn steps_are_taken_from_the_real_width() {
        // 2421 (7579 .. 10000): between 2400 and 2700
        assert_eq!(step(USB, 100, 2521, Step::Wider), Some((100, 2800)));
        assert_eq!(step(USB, 100, 2521, Step::Narrower), Some((100, 2200)));
        // 2699: that is 2700, so plus goes to 3000
        assert_eq!(step(USB, 100, 2799, Step::Wider), Some((100, 3100)));
        // 971: below the narrowest step - plus to 1800, no minus
        assert_eq!(step(USB, 100, 1071, Step::Wider), Some((100, 1900)));
        assert_eq!(step(USB, 100, 1071, Step::Narrower), None);
    }

    /// Thetis' stock USB presets (F1..F10: 5.0, 4.4, 3.8, 3.3, 2.9, 2.7, 2.4, 2.1,
    /// 1.8, 1.0 kHz, low edge 100). From each one, plus and minus go to the
    /// neighbouring step on that side - never skip one and never refuse while a
    /// step exists. A 5 % "counts as that step" band failed both: at 3.8 plus was
    /// disabled although 4.0 exists, and from 2.9 plus skipped 3.0.
    #[test]
    fn thetis_stock_presets_step_to_the_neighbour() {
        let cases = [
            // (width, expected wider, expected narrower)
            (5000, None, Some(4000)),
            (4400, None, Some(4000)),
            (3800, Some(4000), Some(3600)),
            (3300, Some(3600), Some(3000)),
            (2900, Some(3000), Some(2700)),
            (2700, Some(3000), Some(2400)),
            (2400, Some(2700), Some(2100)),
            (2100, Some(2400), Some(1800)),
            (1800, Some(2100), None),
            (1000, Some(1800), None),
        ];
        for (w, wider, narrower) in cases {
            let got_w = step(USB, 100, 100 + w, Step::Wider).map(|(l, h)| h - l);
            let got_n = step(USB, 100, 100 + w, Step::Narrower).map(|(l, h)| h - l);
            assert_eq!(got_w, wider, "plus from Thetis' {} Hz preset", w);
            assert_eq!(got_n, narrower, "minus from Thetis' {} Hz preset", w);
        }
    }

    #[test]
    fn the_ends_of_the_range() {
        assert_eq!(step(USB, 100, 1900, Step::Narrower), None);
        assert_eq!(step(USB, 100, 4100, Step::Wider), None);
        assert_eq!(step(USB, 100, 6000, Step::Wider), None, "wider than every step");
        assert_eq!(step(USB, 100, 6000, Step::Narrower), Some((100, 4100)));
        assert_eq!(step(CWU, 575, 625, Step::Narrower), None);
    }

    #[test]
    fn no_filter_known_means_no_step() {
        assert_eq!(step(USB, 0, 0, Step::Wider), None);
        assert_eq!(step(USB, 500, 400, Step::Wider), None);
    }

    // ---- centred modes ----------------------------------------------------

    /// CW keeps the centre - the pitch - as it always did.
    #[test]
    fn cw_keeps_the_pitch() {
        assert_eq!(step(CWU, 350, 850, Step::Narrower), Some((475, 725)));
        assert_eq!(step(CWU, 350, 850, Step::Wider), Some((100, 1100)));
        assert_eq!(step(CWL, -850, -350, Step::Wider), Some((-1100, -100)));
    }

    /// DIGU/DIGL keep their centre, as Thetis' own width control does. They
    /// used to share the USB rule, and the USB rule now replaces a low cut above
    /// 500 Hz - a digital filter lives exactly there.
    #[test]
    fn digital_keeps_its_centre() {
        assert_eq!(step(DIGU, 1000, 2000, Step::Wider), Some((600, 2400)));
        assert_eq!(step(DIGL, -2000, -1000, Step::Wider), Some((-2400, -600)));
    }

    /// A centred filter does not cross the carrier: it is pushed back onto its
    /// own side with its width intact.
    #[test]
    fn centred_filters_stay_on_their_side() {
        assert_eq!(step(CWU, 0, 100, Step::Wider), Some((0, 250)));
        assert_eq!(step(CWL, -100, 0, Step::Wider), Some((-250, 0)));
        assert_eq!(step(DIGU, 200, 1000, Step::Wider), Some((0, 1800)));
    }

    /// The kilohertz boundary sits at 1000 Hz, not somewhere above it.
    #[test]
    fn the_label_switches_to_kilohertz_at_1000_hz() {
        assert_eq!(format_width_hz(999, false), "999 Hz");
        assert_eq!(format_width_hz(1000, false), "1 kHz");
        assert_eq!(format_width_hz(1999, false), "2 kHz");
    }

    /// Which ladder each mode uses, named one by one. Dropping DSB, SAM or DRM
    /// out of the AM ladder left every other test green.
    #[test]
    fn every_mode_has_its_own_ladder() {
        for m in [0u8, 1, 7, 9, 8] {
            assert_eq!(steps_for_mode(m), SSB_STEPS, "mode {}", m);
        }
        for m in [3u8, 4] {
            assert_eq!(steps_for_mode(m), CW_STEPS, "mode {}", m);
        }
        for m in [2u8, 6, 10, 11] {
            assert_eq!(steps_for_mode(m), AM_STEPS, "mode {}", m);
        }
        assert!(steps_for_mode(5).is_empty(), "FM");
    }

    /// The bottom of each ladder, so a rung added underneath is seen. AM at
    /// 3 kHz has nothing narrower; the narrowest CW step is 50 Hz.
    #[test]
    fn the_bottom_of_each_ladder() {
        // Deliberately narrower than the bottom rung: a rung added underneath
        // would be caught here, and at 3 kHz it was not (review mutant).
        assert_eq!(step(AM, -1000, 1000, Step::Narrower), None);
        assert_eq!(step(AM, -1000, 1000, Step::Wider), Some((-2000, 2000)));
        assert_eq!(step(CWU, 580, 620, Step::Narrower), None);
        assert_eq!(step(CWU, 580, 620, Step::Wider), Some((575, 625)));
        assert_eq!(step(USB, 100, 1200, Step::Narrower), None);
    }

    /// The low-cut range is 0..=500 on both sides: 0 is kept, -1 is not.
    #[test]
    fn the_low_cut_range_has_both_ends() {
        assert_eq!(step(USB, 0, 2400, Step::Wider), Some((0, 2700)));
        assert_eq!(step(USB, -1, 2400, Step::Wider), Some((100, 2800)));
        assert_eq!(step(USB, 500, 2900, Step::Wider), Some((500, 3200)));
        assert_eq!(step(USB, 501, 2901, Step::Wider), Some((100, 2800)));
        assert_eq!(step(LSB, -2400, 0, Step::Wider), Some((-2700, 0)));
        assert_eq!(step(LSB, -2400, 1, Step::Wider), Some((-2800, -100)));
    }

    #[test]
    fn am_is_symmetric() {
        assert_eq!(step(AM, -3000, 3000, Step::Wider), Some((-4000, 4000)));
        assert_eq!(step(AM, -3000, 3000, Step::Narrower), Some((-2000, 2000)));
    }

    /// Thetis computes the FM filter from the deviation and ignores the edges
    /// it is sent (`UpdateRX1Filters`, FM branch), so a width button in FM
    /// would be a button that does nothing.
    #[test]
    fn fm_has_no_width_steps() {
        assert_eq!(step(FM, -8000, 8000, Step::Wider), None);
        assert_eq!(step(FM, -8000, 8000, Step::Narrower), None);
    }

    // ---- the label ------------------------------------------------------------

    #[test]
    fn the_label_shows_what_thetis_has() {
        assert_eq!(width_label(USB, 100, 2800), "2.7 kHz");
        assert_eq!(width_label(USB, 100, 3100), "3 kHz");
        assert_eq!(width_label(USB, 7579, 10000), "2.42 kHz");
        assert_eq!(width_label(USB, 9630, 10000), "370 Hz");
        assert_eq!(width_label(USB, 100, 10100), "10 kHz");
        assert_eq!(width_label(CWU, 350, 850), "500 Hz");
        assert_eq!(width_label(CWU, 100, 1300), "1200 Hz");
        assert_eq!(width_label(USB, 0, 0), "--");
    }

    // ---- the desktop hold ---------------------------------------------------

    /// A local change outranks the server for a moment and then lets go. Until
    /// build 47 it never let go before a mode change.
    /// The hold is one second, and that number is written here rather than
    /// expressed in terms of itself. Every bound in the test below used
    /// `LOCAL_CHANGE_HOLD`, so the constant could be anything and the suite stayed
    /// green - at an hour it is the defect this patch repairs (review finding, 2026-09-16).
    #[test]
    fn the_hold_is_one_second() {
        assert_eq!(LOCAL_CHANGE_HOLD, Duration::from_millis(1000));
    }

    /// A time that lies before the change itself must not panic and must not
    /// release the hold - a clock that went backwards is not permission.
    #[test]
    fn a_change_in_the_future_holds_and_does_not_panic() {
        let t0 = Instant::now();
        assert!(!accept_server_filter(Some(t0 + Duration::from_secs(5)), t0));
    }

    #[test]
    fn a_local_change_lets_go() {
        let t0 = Instant::now();
        assert!(accept_server_filter(None, t0));
        assert!(!accept_server_filter(Some(t0), t0));
        assert!(!accept_server_filter(Some(t0), t0 + LOCAL_CHANGE_HOLD - Duration::from_millis(1)));
        assert!(accept_server_filter(Some(t0), t0 + LOCAL_CHANGE_HOLD));
        assert!(accept_server_filter(Some(t0), t0 + Duration::from_secs(3600)));
    }
}
