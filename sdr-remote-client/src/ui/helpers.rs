use super::*;

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
        1 | 7 => {  // USB, DIGU — low edge anchored
            let low = filter_low.max(25);
            (low, low + new_bw)
        }
        0 | 9 => {  // LSB, DIGL — high edge anchored
            let high = filter_high.min(-25);
            (high - new_bw, high)
        }
        3 | 4 => {  // CWL, CWU — keep center
            let center = (filter_low + filter_high) / 2;
            (center - new_bw / 2, center + new_bw / 2)
        }
        _ => {      // AM, SAM, DSB, DRM, FM — symmetric
            (-new_bw / 2, new_bw / 2)
        }
    }
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

/// Render VFO frequency with per-digit scroll wheel tuning.
/// Returns Some(step) if scroll was detected, where step is positive for up, negative for down.
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
    for ch in formatted.chars() {
        let is_digit = ch.is_ascii_digit();
        let label = ui.add(
            egui::Label::new(RichText::new(ch.to_string()).size(18.0).strong().family(egui::FontFamily::Monospace))
                .sense(egui::Sense::hover()),
        );
        if is_digit {
            if scroll_y != 0.0 {
                if let Some(pos) = pointer_pos {
                    if label.rect.contains(pos) {
                        // digit_idx 0 = highest digit, num_digits-1 = ones
                        let power = (num_digits - 1 - digit_idx) as u32;
                        let step = 10i64.pow(power);
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
    scroll_step
}

/// Compute default spectrum zoom for a given DDC span.
/// Targets ~48 kHz visible view width (matching 32x for 1536 kHz DDC).
pub(crate) fn default_zoom_for_span(span_hz: u32) -> f32 {
    (span_hz as f32 / 48000.0).clamp(1.0, 1024.0)
}

/// Draw a horizontal level meter bar
pub(crate) fn level_bar(ui: &mut egui::Ui, level: f32) {
    let desired_size = Vec2::new(200.0, 16.0);
    let (rect, _response) = ui.allocate_exact_size(desired_size, egui::Sense::hover());

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();

        painter.rect_filled(rect, 2.0, Color32::from_rgb(30, 30, 30));

        let level_clamped = level.clamp(0.0, 1.0);
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

        let db = if level_clamped > 0.0001 {
            20.0 * level_clamped.log10()
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
