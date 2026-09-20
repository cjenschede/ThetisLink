// SPDX-License-Identifier: GPL-2.0-or-later

use super::*;

/// Mouse-wheel support for a slider. Call this right AFTER `ui.add(slider)`
/// with the slider's Response and the same value/range: while the pointer is
/// over the slider, one wheel notch moves the value by `step` (clamped to
/// `range`). Returns true if the wheel changed the value, so the caller can
/// OR it with `resp.changed()` before sending its command. Kept as a post-hoc
/// helper (not a slider builder) so it composes with any slider config
/// (log scale, formatters, width) without lifetime churn.
pub(crate) fn slider_wheel<N: egui::emath::Numeric>(
    ui: &egui::Ui,
    resp: &egui::Response,
    value: &mut N,
    range: std::ops::RangeInclusive<N>,
    step: f64,
) -> bool {
    // Never scroll a disabled slider (display-only readouts stay put).
    if !resp.enabled() || !resp.hovered() {
        return false;
    }
    let scroll = ui.input(|i| i.raw_scroll_delta.y);
    if scroll == 0.0 {
        return false;
    }
    let dir = if scroll > 0.0 { 1.0 } else { -1.0 };
    let cur = value.to_f64();
    let lo = range.start().to_f64();
    let hi = range.end().to_f64();
    let nv = (cur + dir * step).clamp(lo, hi);
    if (nv - cur).abs() > f64::EPSILON {
        *value = N::from_f64(nv);
        true
    } else {
        false
    }
}

/// Collapsible-section header with a manually drawn filled
/// triangle-chevron. Mirrored from `sdr-remote-server/src/ui/utils.rs`
/// so client and server use the same visual style - no
/// font-glyph dependency (egui's default font has no ▼/▶).
///
/// - `open == false`: right-pointing filled triangle (▶), collapsed
/// - `open == true`:  down-pointing filled triangle (▼), expanded
pub(super) fn chevron_label(
    ui: &mut egui::Ui,
    open: bool,
    label: impl Into<egui::WidgetText>,
) -> egui::Response {
    let text: egui::WidgetText = label.into();
    let chevron_size = ui.text_style_height(&egui::TextStyle::Button);
    let spacing = ui.spacing().item_spacing.x;

    let galley = text.into_galley(
        ui,
        Some(egui::TextWrapMode::Extend),
        f32::INFINITY,
        egui::TextStyle::Button,
    );

    let row_size = egui::vec2(
        chevron_size + spacing + galley.size().x,
        chevron_size.max(galley.size().y),
    );
    let (rect, response) = ui.allocate_exact_size(row_size, egui::Sense::click());

    let color = if response.hovered() {
        ui.visuals().widgets.hovered.fg_stroke.color
    } else {
        ui.visuals().text_color()
    };

    let chev_center = egui::pos2(rect.left() + chevron_size / 2.0, rect.center().y);
    let scale = if response.hovered() { 1.35 } else { 1.0 };
    let r = chevron_size * 0.28 * scale;
    let points = if open {
        vec![
            egui::pos2(chev_center.x - r * 0.7, chev_center.y - r * 0.5),
            egui::pos2(chev_center.x + r * 0.7, chev_center.y - r * 0.5),
            egui::pos2(chev_center.x, chev_center.y + r * 1.0),
        ]
    } else {
        vec![
            egui::pos2(chev_center.x - r * 0.5, chev_center.y - r * 0.7),
            egui::pos2(chev_center.x - r * 0.5, chev_center.y + r * 0.7),
            egui::pos2(chev_center.x + r * 1.0, chev_center.y),
        ]
    };
    ui.painter()
        .add(egui::Shape::convex_polygon(points, color, egui::Stroke::NONE));

    let label_pos = egui::pos2(
        rect.left() + chevron_size + spacing,
        rect.center().y - galley.size().y / 2.0,
    );
    ui.painter().galley(label_pos, galley, color);

    response
}

/// A group of settings behind a chevron - so what belongs together is seen to
/// belong together on a tab that keeps growing (owner, 2026-09-17).
///
/// Every group has exactly one line above it and exactly one below, open or
/// closed, and the one below is always under the last thing that belongs to
/// it: under the chevron when closed, under the last row when open (owner,
/// 2026-09-18). This draws the one below. The one above is the caller's
/// (`group_rule` in front of the call) - unless another group comes directly
/// before, whose line below is then this group's line above, so two groups
/// in a row share one line instead of showing two. The caller owns the open
/// flag and persists it; this only draws and reports a click, so the body can
/// borrow whatever it needs.
pub(super) fn settings_group(
    ui: &mut egui::Ui,
    open: bool,
    title: impl Into<egui::WidgetText>,
    body: impl FnOnce(&mut egui::Ui),
) -> bool {
    let clicked = chevron_label(ui, open, title).clicked();
    if open {
        body(ui);
    }
    group_rule(ui);
    clicked
}

/// The line above and below a settings group. egui's own separator
/// (one thin pixel in the faint border colour, a few pixels of room) was hard
/// to see and sat close to the text (owner, 2026-09-18): this one is drawn in
/// the theme's weak text colour, so it follows all four themes, a little
/// heavier, with more room on both sides.
pub(super) fn group_rule(ui: &mut egui::Ui) {
    const ROOM: f32 = 6.0;
    const STROKE: f32 = 1.5;
    ui.add_space(ROOM);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), STROKE), egui::Sense::hover());
    ui.painter().hline(rect.x_range(), rect.center().y, egui::Stroke::new(STROKE, ui.visuals().weak_text_color()));
    ui.add_space(ROOM);
}

/// Determine amateur band from frequency (returns None if outside bands)
pub(crate) fn freq_to_band(hz: u64) -> Option<String> {
    match hz {
        1_800_000..=2_000_000 => Some("160m".to_string()),
        3_500_000..=3_800_000 => Some("80m".to_string()),
        // Wide on purpose: 60m is channelised and the channels differ per
        // country (WRC-15 gives 5351.5-5366.5, the US has five discrete ones
        // from 5330.5 to 5403.5). This range covers every one of them. It labels
        // and keys local preferences - it does not decide what is transmitted.
        // `dxcluster::freq_to_band` on the server is a separate function with the
        // same name that gates DX spots; keep the two in step.
        5_250_000..=5_450_000 => Some("60m".to_string()),
        7_000_000..=7_200_000 => Some("40m".to_string()),
        10_100_000..=10_150_000 => Some("30m".to_string()),
        14_000_000..=14_350_000 => Some("20m".to_string()),
        18_068_000..=18_168_000 => Some("17m".to_string()),
        21_000_000..=21_450_000 => Some("15m".to_string()),
        24_890_000..=24_990_000 => Some("12m".to_string()),
        28_000_000..=29_700_000 => Some("10m".to_string()),
        50_000_000..=52_000_000 => Some("6m".to_string()),
        _ => None,
    }
}

/// Derive amateur band label from frequency
pub(crate) fn band_label(hz: u64) -> &'static str {
    match hz {
        1_800_000..=1_999_999 => "160m",
        3_500_000..=3_999_999 => "80m",
        5_250_000..=5_449_999 => "60m",   // see the note in `freq_to_band`
        7_000_000..=7_299_999 => "40m",
        10_100_000..=10_149_999 => "30m",
        14_000_000..=14_349_999 => "20m",
        18_068_000..=18_167_999 => "17m",
        21_000_000..=21_449_999 => "15m",
        24_890_000..=24_989_999 => "12m",
        28_000_000..=29_699_999 => "10m",
        50_000_000..=53_999_999 => "6m",
        _ => "",
    }
}

/// The filter width row for one receiver: label, minus, width, plus. Returns the
/// edges to send when a button was pressed.
///
/// The rule - which width comes next, where the edges go, what the label says -
/// lives in `sdr_remote_logic::filter_width` and is shared with Android. Until
/// build 47 this row was written out three times here (Radio tab, RX1 popout,
/// RX2).
pub(crate) fn filter_width_buttons(ui: &mut egui::Ui, mode: u8, low: i32, high: i32) -> Option<(i32, i32)> {
    use sdr_remote_logic::filter_width::{step, width_label, Step};
    let narrower = step(mode, low, high, Step::Narrower);
    let wider = step(mode, low, high, Step::Wider);
    let mut pressed = None;

    ui.label(rust_i18n::t!("main_filter_label").to_string());
    let minus_btn = egui::Button::new(RichText::new(" - ").size(14.0));
    if ui.add_enabled(narrower.is_some(), minus_btn)
        .on_hover_text(rust_i18n::t!("main_hover_narrow_filter").to_string())
        .clicked()
    {
        pressed = narrower;
    }

    // No NFM/WFM suffix in FM. It used to guess the deviation from the width with
    // "6 kHz or less is NFM", a threshold written for the VRX's own 5/10 kHz
    // widths. Thetis reports 2 x (deviation + FM high cut): 11 kHz for NFM and
    // 16 kHz for WFM at the default 3 kHz high cut, so it always said WFM. Thetis
    // itself shows just the width.
    ui.label(RichText::new(width_label(mode, low, high)).strong().size(14.0));

    let plus_btn = egui::Button::new(RichText::new(" + ").size(14.0));
    if ui.add_enabled(wider.is_some(), plus_btn)
        .on_hover_text(rust_i18n::t!("main_hover_widen_filter").to_string())
        .clicked()
    {
        pressed = wider;
    }
    pressed
}

// ── VRX-specific mode helpers ──────────────────────────────────────────
// VRX mode numbering (different from Thetis to keep backward-compat with
// older client config files): 0=USB, 1=LSB, 2=AM, 3=SAM, 4=FM.
// Display order matches VFO A+B (LSB first), with mode value sorted accordingly.
pub(crate) const VRX_MODES: &[(u8, &str)] = &[
    (1, "LSB"), (0, "USB"), (2, "AM"), (3, "SAM"), (4, "FM"),
];

pub(crate) fn vrx_mode_label(mode: u8) -> &'static str {
    match mode {
        0 => "USB", 1 => "LSB", 2 => "AM", 3 => "SAM", 4 => "FM",
        _ => "?",
    }
}

/// Default (filter_low_hz, filter_high_hz) for a VRX mode. Preserves
/// the current SSB bandwidth on USB<->LSB swap; uses mode-specific
/// defaults for AM/SAM/FM (their natural bandwidths differ from SSB).
pub(crate) fn vrx_mode_default_filter(new_mode: u8, current_bw: i32) -> (i32, i32) {
    let bw = if current_bw > 0 { current_bw } else { 3000 };
    match new_mode {
        0 => (0, bw),                // USB
        1 => (-bw, 0),               // LSB
        2 | 3 => (-3000, 3000),      // AM/SAM: 6 kHz symmetric
        4 => (-4000, 4000),          // FM: ±4 kHz (max channelizer can pass)
        _ => (0, bw),
    }
}

/// Filter presets to show in VRX BW dropdown (mode-dependent).
pub(crate) fn vrx_filter_presets(mode: u8) -> &'static [i32] {
    use sdr_remote_logic::filter_width::{AM_STEPS, SSB_STEPS};
    match mode {
        2 | 3 => AM_STEPS,        // AM / SAM
        4 => VRX_FM_PRESETS,      // FM
        _ => SSB_STEPS,           // USB / LSB
    }
}

/// VRX FM widths: NFM (2500 Hz deviation) / WFM (5000 Hz). The VRX channelizer is
/// TL's own, so unlike the main receivers these edges are honoured.
const VRX_FM_PRESETS: &[i32] = &[5000, 10000];

/// Convert a "total bandwidth" preset to (filter_low_hz, filter_high_hz)
/// for a given VRX mode. SSB modes use one side; AM-family + FM are
/// symmetric around carrier.
pub(crate) fn vrx_filter_from_preset(mode: u8, preset_bw: i32) -> (i32, i32) {
    match mode {
        0 => (0, preset_bw),                          // USB
        1 => (-preset_bw, 0),                         // LSB
        2 | 3 | 4 => (-preset_bw / 2, preset_bw / 2), // AM/SAM/FM symmetric
        _ => (0, preset_bw),
    }
}

pub(crate) fn format_bandwidth(hz: i32, cw: bool) -> String {
    sdr_remote_logic::filter_width::format_width_hz(hz, cw)
}

/// True for modes whose filter is symmetric around the carrier - AM, SAM, DSB,
/// FM, DRM. Main-RX DSPMode numbering: LSB=0, USB=1, DSB=2, CWL=3, CWU=4,
/// FM=5, AM=6, DIGU=7, SPEC=8, DIGL=9, SAM=10, DRM=11. One-sided modes (SSB,
/// CW, DIG) and SPEC are NOT symmetric. Used to mirror a single dragged filter
/// edge to both sides so the spectrum matches Thetis, which forces symmetry in
/// these modes (it adopts the wider edge for both sides).
pub(crate) fn is_symmetric_mode(mode: u8) -> bool {
    !matches!(mode, 0 | 1 | 3 | 4 | 7 | 8 | 9)
}

/// Get current time as HH:MM:SS string
pub(crate) fn chrono_time() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let secs = now % 60;
    let mins = (now / 60) % 60;
    let hours = (now / 3600) % 24;
    // Crude local time approximation (UTC); good enough for a log timestamp
    format!("{:02}:{:02}:{:02}", hours, mins, secs)
}

/// Format frequency with dots as thousands separators (e.g. 14.205.350)
pub(crate) fn format_frequency(hz: u64) -> String {
    let s = hz.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push('.');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

/// Render VFO frequency with per-digit tuning: scroll wheel (desktop) and tap
/// (touch/mouse) - tap on the top of a digit = up, bottom = down.
/// Returns Some(step) if tuning occurred (positive = up, negative = down).
pub(crate) fn render_freq_scroll(ui: &mut egui::Ui, hz: u64) -> Option<i64> {
    let formatted = format_frequency(hz);
    let num_digits = formatted.chars().filter(|c| c.is_ascii_digit()).count();

    let mut scroll_step: Option<i64> = None;
    let pointer_pos = ui.input(|i| i.pointer.hover_pos());
    // Check if scroll was already consumed this frame (by another VFO's digit scroll)
    let already_consumed: bool = ui.ctx().memory(|mem|
        mem.data.get_temp(egui::Id::new("freq_scroll_consumed")).unwrap_or(false));
    let scroll_y = if already_consumed {
        0.0
    } else {
        let raw = ui.input(|i| i.raw_scroll_delta.y);
        if raw > 0.0 { 1.0 } else if raw < 0.0 { -1.0 } else { 0.0 }
    };

    // Render all chars, tracking digit index (0 = leftmost/highest digit)
    let mut digit_idx = 0;
    let mut pointer_over_digit = false;
    for ch in formatted.chars() {
        let is_digit = ch.is_ascii_digit();
        let label = ui.add(
            egui::Label::new(RichText::new(ch.to_string()).size(18.0).strong().family(egui::FontFamily::Monospace))
                .sense(if is_digit { egui::Sense::click() } else { egui::Sense::hover() }),
        );
        if is_digit {
            // digit_idx 0 = highest digit, num_digits-1 = ones
            let power = (num_digits - 1 - digit_idx) as u32;
            let step = 10i64.pow(power);
            // Hover-cue (desktop): highlight only the half where the mouse is
            // (top = up, bottom = down). This gives directional feedback -
            // it nudges the user toward what a click there will do.
            if label.hovered() {
                if let Some(p) = pointer_pos {
                    let rect = label.rect;
                    let mid = rect.center().y;
                    let half = if p.y < mid {
                        egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x, mid))
                    } else {
                        egui::Rect::from_min_max(egui::pos2(rect.min.x, mid), rect.max)
                    };
                    ui.painter().rect_filled(half, 2.0,
                        Color32::from_rgba_unmultiplied(80, 160, 255, 60));
                }
            }
            // Tap-tuning (touch + mouse): top half = up, bottom half = down.
            if label.clicked() {
                let up = label.interact_pointer_pos()
                    .map_or(true, |p| p.y < label.rect.center().y);
                scroll_step = Some(if up { step } else { -step });
            }
            if let Some(pos) = pointer_pos {
                if label.rect.contains(pos) {
                    pointer_over_digit = true;
                    if scroll_y != 0.0 {
                        scroll_step = Some(if scroll_y > 0.0 { step } else { -step });
                    }
                }
            }
            digit_idx += 1;
        }
    }
    // " Hz" suffix
    ui.add(
        egui::Label::new(RichText::new(" Hz").size(18.0).strong())
            .sense(egui::Sense::hover()),
    );
    // Mark scroll as consumed so other VFO and spectrum don't also fire
    if scroll_step.is_some() {
        ui.ctx().memory_mut(|mem| mem.data.insert_temp(egui::Id::new("freq_scroll_consumed"), true));
    }
    // When the mouse is hovering exactly over a digit, zero out the scroll
    // input so a parent ScrollArea (Yaesu-tab, popout, etc.) does not
    // scroll along while the user is tuning the digit.
    if pointer_over_digit {
        ui.ctx().input_mut(|i| {
            i.raw_scroll_delta = egui::Vec2::ZERO;
            i.smooth_scroll_delta = egui::Vec2::ZERO;
        });
    }
    scroll_step
}

/// Touch-friendly frequency stepper: large −/+ buttons with an adjustable
/// step-size picker (10 Hz ... 1 MHz). `step_hz` is the chosen step (the caller
/// keeps it). Returns Some(delta_hz) if ◄◄/►► was pressed.
pub(crate) fn render_freq_stepper(ui: &mut egui::Ui, step_hz: &mut i64) -> Option<i64> {
    const STEPS: [(&str, i64); 6] = [
        ("10", 10), ("100", 100), ("1k", 1_000),
        ("10k", 10_000), ("100k", 100_000), ("1M", 1_000_000),
    ];
    if *step_hz <= 0 { *step_hz = 1_000; }
    let mut delta = None;
    ui.horizontal(|ui| {
        if ui.add(egui::Button::new(RichText::new("−").size(15.0))
            .min_size(egui::vec2(42.0, 24.0)))
            .on_hover_text(rust_i18n::t!("hover_freq_down").to_string()).clicked()
        {
            delta = Some(-*step_hz);
        }
        for (lbl, s) in STEPS {
            let sel = *step_hz == s;
            let b = if sel {
                egui::Button::new(RichText::new(lbl).size(11.0).color(Color32::WHITE))
                    .fill(Color32::from_rgb(0, 90, 200)).min_size(egui::vec2(34.0, 22.0))
            } else {
                egui::Button::new(RichText::new(lbl).size(11.0)).min_size(egui::vec2(34.0, 22.0))
            };
            if ui.add(b).clicked() { *step_hz = s; }
        }
        if ui.add(egui::Button::new(RichText::new("+").size(15.0))
            .min_size(egui::vec2(42.0, 24.0)))
            .on_hover_text(rust_i18n::t!("hover_freq_up").to_string()).clicked()
        {
            delta = Some(*step_hz);
        }
    });
    delta
}

/// Compute default spectrum zoom for a given DDC span.
/// Targets ~48 kHz visible view width (matching 32x for 1536 kHz DDC).
pub(crate) fn default_zoom_for_span(span_hz: u32) -> f32 {
    (span_hz as f32 / 48000.0).clamp(1.0, 1024.0)
}

/// Draw a horizontal level meter bar
/// Horizontal audio-level bar with a **peak-hold** marker: a thin white tick that
/// holds the recent maximum for ~1.5 s before snapping down. `id_salt` must be
/// unique per bar (e.g. "rx1", "vrx2") - the peak state lives in egui temp memory
/// keyed by it, so no per-bar struct fields are needed.
pub(crate) fn level_bar(ui: &mut egui::Ui, level: f32, id_salt: &str) {
    const HOLD_SECS: f64 = 1.5;
    let desired_size = Vec2::new(200.0, 16.0);
    let (rect, _response) = ui.allocate_exact_size(desired_size, egui::Sense::hover());

    let level_clamped = level.clamp(0.0, 1.0);

    // Peak-hold state (transient, per bar): (held_peak, held_since_seconds).
    let id = egui::Id::new(("level_peak", id_salt));
    let now = ui.input(|i| i.time);
    let (mut peak, mut peak_t) =
        ui.data(|d| d.get_temp::<(f32, f64)>(id)).unwrap_or((level_clamped, now));
    if level_clamped >= peak {
        peak = level_clamped;
        peak_t = now;
    } else if now - peak_t > HOLD_SECS {
        peak = level_clamped; // hold elapsed -> drop to current
        peak_t = now;
    } else {
        // still holding -> make sure we repaint when the hold is due to expire,
        // so the tick snaps down on time even if audio went silent (no updates).
        ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(
            (HOLD_SECS - (now - peak_t)).max(0.0),
        ));
    }
    ui.data_mut(|d| d.insert_temp(id, (peak, peak_t)));

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();

        painter.rect_filled(rect, 2.0, Color32::from_rgb(30, 30, 30));

        let fill_width = rect.width() * level_clamped;
        let fill_rect = egui::Rect::from_min_size(rect.min, Vec2::new(fill_width, rect.height()));

        let color = if level_clamped < 0.5 {
            Color32::GREEN
        } else if level_clamped < 0.8 {
            Color32::from_rgb(255, 170, 40)
        } else {
            Color32::RED
        };

        painter.rect_filled(fill_rect, 2.0, color);

        // dB text shows the PEAK-HOLD value (the held maximum), not the flickering
        // instantaneous level - easier to read off the actual peak.
        let db = if peak > 0.0001 {
            20.0 * peak.log10()
        } else {
            -80.0
        };
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            format!("{:.0} dB", db),
            egui::FontId::proportional(10.0),
            Color32::WHITE,
        );

        // Peak-hold marker: thin white vertical tick at the held peak.
        if peak > 0.01 {
            let peak_x = rect.left() + rect.width() * peak;
            painter.line_segment(
                [egui::pos2(peak_x, rect.top()), egui::pos2(peak_x, rect.bottom())],
                egui::Stroke::new(1.5, Color32::WHITE),
            );
        }
    }
}

/// Draw an S-meter bar with S-unit markings
pub(crate) fn spe_band_name(band: u8) -> &'static str {
    match band {
        0 => "160m", 1 => "80m", 2 => "60m", 3 => "40m", 4 => "30m",
        5 => "20m", 6 => "17m", 7 => "15m", 8 => "12m", 9 => "10m",
        10 => "6m", 11 => "4m", _ => "?",
    }
}

pub(crate) fn rf2k_band_name(band: u8) -> &'static str {
    match band {
        0 => "6m", 1 => "10m", 2 => "12m", 3 => "15m", 4 => "17m",
        5 => "20m", 6 => "30m", 7 => "40m", 8 => "60m", 9 => "80m",
        10 => "160m", _ => "?",
    }
}

#[cfg(test)]
mod opening_zoom_tests {
    use super::default_zoom_for_span;

    /// The width the receivers actually run at here. Both RX1 and RX2 have to
    /// land on the same figure - RX2 sat on the opening 32x for want of this.
    #[test]
    fn the_field_rate_opens_at_eight_times() {
        assert_eq!(default_zoom_for_span(384_000), 8.0);
    }

    /// The figure the doc comment is written around.
    #[test]
    fn the_widest_ddc_opens_at_thirty_two_times() {
        assert_eq!(default_zoom_for_span(1_536_000), 32.0);
    }

    #[test]
    fn a_narrower_receiver_opens_further_out() {
        assert_eq!(default_zoom_for_span(192_000), 4.0);
        assert_eq!(default_zoom_for_span(96_000), 2.0);
    }

    /// Every width lands on the same visible span, which is the whole point.
    #[test]
    fn the_visible_width_comes_out_the_same_either_way() {
        for span_hz in [96_000u32, 192_000, 384_000, 768_000, 1_536_000] {
            let visible = span_hz as f32 / default_zoom_for_span(span_hz);
            assert!((visible - 48_000.0).abs() < 1.0, "{span_hz} Hz gave {visible} Hz");
        }
    }

    /// No rate yet. The callers guard on this, but zoom below 1x is not a view.
    #[test]
    fn without_a_width_there_is_no_zooming_out_past_one() {
        assert_eq!(default_zoom_for_span(0), 1.0);
        assert_eq!(default_zoom_for_span(48_000), 1.0);
        assert_eq!(default_zoom_for_span(24_000), 1.0);
    }

    #[test]
    fn an_absurd_width_stops_at_the_ceiling() {
        assert_eq!(default_zoom_for_span(u32::MAX), 1024.0);
    }
}
