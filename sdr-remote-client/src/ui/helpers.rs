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

/// Collapsible-section header met handmatig getekende gevulde
/// driehoek-chevron. Gespiegeld van `sdr-remote-server/src/ui/utils.rs`
/// zodat client en server dezelfde visuele stijl gebruiken - geen
/// font-glyph dependency (egui's default font heeft geen ▼/▶).
///
/// - `open == false`: rechts-wijzende gevulde driehoek (▶), collapsed
/// - `open == true`:  omlaag-wijzende gevulde driehoek (▼), expanded
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

/// Determine amateur band from frequency (returns None if outside bands)
pub(crate) fn freq_to_band(hz: u64) -> Option<String> {
    match hz {
        1_800_000..=2_000_000 => Some("160m".to_string()),
        3_500_000..=3_800_000 => Some("80m".to_string()),
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

// Filter bandwidth presets per mode category
pub(crate) const SSB_PRESETS: &[i32] = &[1800, 2100, 2400, 2700, 3000, 3300, 3600, 4000];
pub(crate) const CW_PRESETS: &[i32] = &[50, 100, 250, 500, 1000];
pub(crate) const AM_PRESETS: &[i32] = &[4000, 6000, 8000, 10000, 12000];
pub(crate) const FM_PRESETS: &[i32] = &[5000, 10000]; // NFM (2500Hz dev) / WFM (5000Hz dev)

pub(crate) fn filter_presets_for_mode(mode: u8) -> &'static [i32] {
    match mode {
        0 | 1 | 7 | 9 => SSB_PRESETS,   // LSB, USB, DIGU, DIGL
        3 | 4 => CW_PRESETS,              // CWL, CWU
        2 | 6 | 10 | 11 => AM_PRESETS,    // DSB, AM, SAM, DRM
        5 => FM_PRESETS,                   // FM
        _ => SSB_PRESETS,
    }
}

pub(crate) fn is_cw_mode(mode: u8) -> bool {
    mode == 3 || mode == 4
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
    match mode {
        2 | 3 => AM_PRESETS,  // AM / SAM
        4 => FM_PRESETS,      // FM
        _ => SSB_PRESETS,     // USB / LSB
    }
}

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
    if cw || hz < 1000 {
        format!("{} Hz", hz)
    } else {
        let khz = hz as f32 / 1000.0;
        if (khz - khz.round()).abs() < 0.01 {
            format!("{} kHz", khz as i32)
        } else {
            format!("{:.1} kHz", khz)
        }
    }
}

pub(crate) fn closest_preset_index(presets: &[i32], bw: i32) -> usize {
    presets.iter()
        .enumerate()
        .min_by_key(|(_, &p)| (p - bw).abs())
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// Calculate new filter edges for a given bandwidth, respecting mode sideband rules.
/// USB/DIGU: anchor low edge (min 25 Hz), expand upward.
/// LSB/DIGL: anchor high edge (max -25 Hz), expand downward.
/// CW: keep center, stay within sideband.
/// AM/SAM/DSB/DRM/FM: symmetric around 0.
pub(crate) fn calc_filter_edges(mode: u8, filter_low: i32, filter_high: i32, new_bw: i32) -> (i32, i32) {
    match mode {
        1 | 7 => {  // USB, DIGU - low edge anchored
            let low = filter_low.max(25);
            (low, low + new_bw)
        }
        0 | 9 => {  // LSB, DIGL - high edge anchored
            let high = filter_high.min(-25);
            (high - new_bw, high)
        }
        3 | 4 => {  // CWL, CWU - keep center
            let center = (filter_low + filter_high) / 2;
            (center - new_bw / 2, center + new_bw / 2)
        }
        _ => {      // AM, SAM, DSB, DRM, FM - symmetric
            (-new_bw / 2, new_bw / 2)
        }
    }
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

/// Render VFO frequency with per-digit tuning: scroll wheel (desktop) én tik
/// (touch/muis) - tik op de bovenkant van een cijfer = omhoog, onderkant = omlaag.
/// Returns Some(step) als er getuned is (positief = omhoog, negatief = omlaag).
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
            // Hover-cue (desktop): licht alléén de helft op waar de muis staat
            // (boven = omhoog, onder = omlaag). Dit geeft directionele terugkoppeling -
            // het duwt de gebruiker naar wat een klik dáár gaat doen.
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
            // Tik-tunen (touch + muis): bovenste helft = omhoog, onderste = omlaag.
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
    // Wanneer de muis exact boven een digit hangt, scroll-input op nul
    // zetten zodat een parent ScrollArea (Yaesu-tab, popout, etc.) niet
    // meescrollt terwijl de gebruiker de digit aan het tunen is.
    if pointer_over_digit {
        ui.ctx().input_mut(|i| {
            i.raw_scroll_delta = egui::Vec2::ZERO;
            i.smooth_scroll_delta = egui::Vec2::ZERO;
        });
    }
    scroll_step
}

/// Touch-vriendelijke frequentie-stapper: grote −/+ knoppen met een instelbare
/// stapgrootte-kiezer (10 Hz ... 1 MHz). `step_hz` is de gekozen stap (de caller
/// bewaart 'm). Returns Some(delta_hz) als er ◄◄/►► is ingedrukt.
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
