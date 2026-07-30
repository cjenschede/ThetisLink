// SPDX-License-Identifier: GPL-2.0-or-later

use super::*;

/// `value`/`peak_value` carry dBm in RX mode and watts in TX mode (the same
/// disambiguation used on the wire - see `SmeterPacket` and the `transmitting`
/// / `other_tx` booleans here). The widget owns all dBm-to-display math via
/// `sdr_remote_core::dbm_to_display`.
pub(crate) fn smeter_bar(ui: &mut egui::Ui, value: f32, peak_value: f32, transmitting: bool, other_tx: bool, swr_x100: u16) -> egui::Rect {
    smeter_bar_sized(ui, value, peak_value, transmitting, other_tx, false, swr_x100)
}

pub(crate) fn smeter_bar_popout(ui: &mut egui::Ui, value: f32, peak_value: f32, transmitting: bool, other_tx: bool, swr_x100: u16) -> egui::Rect {
    smeter_bar_sized(ui, value, peak_value, transmitting, other_tx, true, swr_x100)
}

pub(crate) fn smeter_bar_sized(ui: &mut egui::Ui, value: f32, peak_value: f32, transmitting: bool, other_tx: bool, is_popout: bool, swr_x100: u16) -> egui::Rect {
    let label_h = if is_popout { 18.0 } else { 12.0 };
    let bar_h = if is_popout { 28.0 } else { 14.0 };
    let total_h = label_h + bar_h + label_h; // labels above + bar + labels below
    let bar_w = if is_popout { 392.0 } else { 280.0 };
    let desired_size = Vec2::new(bar_w, total_h);
    let (rect, _response) = ui.allocate_exact_size(desired_size, egui::Sense::hover());

    if !ui.is_rect_visible(rect) {
        return rect;
    }
    let painter = ui.painter();

    let top_label = egui::Rect::from_min_size(rect.min, Vec2::new(rect.width(), label_h));
    let bar_rect = egui::Rect::from_min_size(rect.min + Vec2::new(0.0, label_h), Vec2::new(rect.width(), bar_h));
    let bottom_label = egui::Rect::from_min_size(rect.min + Vec2::new(0.0, label_h + bar_h), Vec2::new(rect.width(), label_h));

    painter.rect_filled(bar_rect, 2.0, Color32::from_rgb(20, 20, 20));

    if other_tx {
        let watts = value;
        let frac = (watts / 100.0).clamp(0.0, 1.0);
        let fill_rect = egui::Rect::from_min_size(bar_rect.min, Vec2::new(bar_rect.width() * frac, bar_h));
        painter.rect_filled(fill_rect, 2.0, Color32::from_rgb(204, 119, 0));
        painter.text(bar_rect.center(), egui::Align2::CENTER_CENTER,
            format!("TX in use  {:.0} W", watts), egui::FontId::proportional(if is_popout { 16.0 } else { 11.0 }), Color32::WHITE);
    } else if transmitting {
        let watts = value;
        let frac = (watts / 100.0).clamp(0.0, 1.0);
        let fill_rect = egui::Rect::from_min_size(bar_rect.min, Vec2::new(bar_rect.width() * frac, bar_h));
        painter.rect_filled(fill_rect, 2.0, Color32::from_rgb(220, 30, 30));

        let tick_font = egui::FontId::proportional(if is_popout { 13.0 } else { 9.0 });
        for &w in &[25, 50, 75, 100] {
            let x = bar_rect.min.x + bar_rect.width() * (w as f32 / 100.0);
            painter.line_segment(
                [egui::pos2(x, bar_rect.min.y), egui::pos2(x, bar_rect.min.y + 3.0)],
                egui::Stroke::new(1.0, Color32::GRAY));
            painter.line_segment(
                [egui::pos2(x, bar_rect.max.y - 3.0), egui::pos2(x, bar_rect.max.y)],
                egui::Stroke::new(1.0, Color32::GRAY));
            painter.text(egui::pos2(x, top_label.min.y), egui::Align2::CENTER_TOP,
                format!("{}W", w), tick_font.clone(), Color32::GRAY);
        }

        let swr_text = if swr_x100 > 100 {
            format!("TX  {:.0}W  SWR {:.2}", watts, swr_x100 as f32 / 100.0)
        } else {
            format!("TX  {:.0}W", watts)
        };
        let swr_color = if swr_x100 > 300 { theme::TL_SWR_ALERT_TEXT }
            else if swr_x100 > 200 { Color32::from_rgb(255, 170, 40) }
            else { Color32::WHITE };
        painter.text(bar_rect.center(), egui::Align2::CENTER_CENTER,
            swr_text, egui::FontId::proportional(if is_popout { 16.0 } else { 11.0 }), swr_color);
    } else {
        // RX S-meter bar - `value` and `peak_value` are dBm.
        let dbm = value;
        let raw_for_arc = sdr_remote_core::dbm_to_display(dbm) as f32;
        let frac = (raw_for_arc / 228.0).clamp(0.0, 1.0);
        let fill_width = bar_rect.width() * frac;
        let s9_frac = 108.0 / 228.0;
        if frac <= s9_frac {
            painter.rect_filled(
                egui::Rect::from_min_size(bar_rect.min, Vec2::new(fill_width, bar_h)),
                2.0, Color32::from_rgb(0, 180, 0));
        } else {
            let green_w = bar_rect.width() * s9_frac;
            painter.rect_filled(
                egui::Rect::from_min_size(bar_rect.min, Vec2::new(green_w, bar_h)),
                2.0, Color32::from_rgb(0, 180, 0));
            painter.rect_filled(
                egui::Rect::from_min_size(bar_rect.min + Vec2::new(green_w, 0.0), Vec2::new(fill_width - green_w, bar_h)),
                0.0, Color32::from_rgb(220, 30, 30));
        }

        // Peak hold needle - peak is also dBm.
        if peak_value > dbm {
            let peak_raw = sdr_remote_core::dbm_to_display(peak_value) as f32;
            let peak_frac = (peak_raw / 228.0).clamp(0.0, 1.0);
            let peak_x = bar_rect.min.x + bar_rect.width() * peak_frac;
            painter.line_segment(
                [egui::pos2(peak_x, bar_rect.min.y), egui::pos2(peak_x, bar_rect.max.y)],
                egui::Stroke::new(2.0, Color32::from_rgb(255, 255, 0)));
        }

        // Scale: tick marks outside bar, S-units above, dB-over below
        let tick_font = egui::FontId::proportional(if is_popout { 13.0 } else { 9.0 });
        for s in 1..=9 {
            let x = bar_rect.min.x + bar_rect.width() * (s as f32 * 12.0 / 228.0);
            // Tick marks in label zones (outside bar)
            painter.line_segment(
                [egui::pos2(x, top_label.max.y - 3.0), egui::pos2(x, top_label.max.y)],
                egui::Stroke::new(1.0, Color32::GRAY));
            painter.line_segment(
                [egui::pos2(x, bottom_label.min.y), egui::pos2(x, bottom_label.min.y + 3.0)],
                egui::Stroke::new(1.0, Color32::GRAY));
            // S-unit labels above bar
            painter.text(egui::pos2(x, top_label.min.y), egui::Align2::CENTER_TOP,
                format!("{}", s), tick_font.clone(), Color32::GRAY);
        }
        for db_over in (10..=60).step_by(10) {
            let raw = 108.0 + db_over as f32 * 2.0;
            let x = bar_rect.min.x + bar_rect.width() * (raw / 228.0);
            // Tick marks in label zones (outside bar)
            painter.line_segment(
                [egui::pos2(x, top_label.max.y - 3.0), egui::pos2(x, top_label.max.y)],
                egui::Stroke::new(1.0, Color32::from_rgb(200, 100, 100)));
            painter.line_segment(
                [egui::pos2(x, bottom_label.min.y), egui::pos2(x, bottom_label.min.y + 3.0)],
                egui::Stroke::new(1.0, Color32::from_rgb(200, 100, 100)));
            // dB-over labels below bar
            painter.text(egui::pos2(x, bottom_label.max.y), egui::Align2::CENTER_BOTTOM,
                format!("+{}", db_over), tick_font.clone(), Color32::from_rgb(200, 100, 100));
        }

        // S-value text on the bar - derived directly from dBm.
        let s_text = if dbm <= -73.0 {
            let s_unit = ((dbm + 127.0) / 6.0).floor().clamp(0.0, 9.0) as u8;
            format!("S{}", s_unit)
        } else {
            let db_over = (dbm + 73.0).round() as i32;
            format!("S9+{} dB", db_over.max(0))
        };
        painter.text(bar_rect.center(), egui::Align2::CENTER_CENTER,
            s_text, egui::FontId::proportional(if is_popout { 16.0 } else { 11.0 }), Color32::WHITE);
    }
    rect
}


/// Yaesu CAT S-meter bar. The radio reports a raw 0..255-ish SM value;
/// render it on the same S1..S9 / S9+dB visual scale as the main RX meter.
/// De Yaesu s-meter-waarde is al de 0-228 display-schaal (S0=0, S9=108, S9+60=228).
/// De analoge meter verwacht dBm en converteert intern via `dbm_to_display`; dit is
/// de inverse, zodat we de raw-waarde door de gedeelde analoogmeter kunnen tonen.
pub(crate) fn yaesu_raw_to_dbm(raw: u16) -> f32 {
    let disp = (raw as f32).clamp(0.0, 228.0);
    if disp <= 108.0 { disp / 2.0 - 127.0 } else { (disp - 108.0) / 2.0 - 73.0 }
}

pub(crate) fn yaesu_smeter_bar(ui: &mut egui::Ui, raw: u16, peak_raw: u16) -> egui::Rect {
    let label_h = 12.0;
    let bar_h = 14.0;
    let total_h = label_h + bar_h + label_h;
    let bar_w = ui.available_width().min(350.0).max(120.0);
    let desired_size = Vec2::new(bar_w, total_h);
    let (rect, _response) = ui.allocate_exact_size(desired_size, egui::Sense::hover());

    if !ui.is_rect_visible(rect) {
        return rect;
    }

    let painter = ui.painter();
    let top_label = egui::Rect::from_min_size(rect.min, Vec2::new(rect.width(), label_h));
    let bar_rect = egui::Rect::from_min_size(rect.min + Vec2::new(0.0, label_h), Vec2::new(rect.width(), bar_h));
    let bottom_label = egui::Rect::from_min_size(rect.min + Vec2::new(0.0, label_h + bar_h), Vec2::new(rect.width(), label_h));

    painter.rect_filled(bar_rect, 2.0, Color32::from_rgb(20, 20, 20));

    let raw_value = raw as f32;
    let raw_for_bar = raw_value.clamp(0.0, 228.0);
    let frac = raw_for_bar / 228.0;
    let fill_width = bar_rect.width() * frac;
    let s9_frac = 108.0 / 228.0;

    if raw_for_bar <= 108.0 {
        painter.rect_filled(
            egui::Rect::from_min_size(bar_rect.min, Vec2::new(fill_width, bar_h)),
            2.0,
            Color32::from_rgb(0, 180, 0),
        );
    } else {
        let green_w = bar_rect.width() * s9_frac;
        painter.rect_filled(
            egui::Rect::from_min_size(bar_rect.min, Vec2::new(green_w, bar_h)),
            2.0,
            Color32::from_rgb(0, 180, 0),
        );
        painter.rect_filled(
            egui::Rect::from_min_size(
                bar_rect.min + Vec2::new(green_w, 0.0),
                Vec2::new((fill_width - green_w).max(0.0), bar_h),
            ),
            0.0,
            Color32::from_rgb(220, 30, 30),
        );
    }

    let peak_for_bar = (peak_raw as f32).clamp(0.0, 228.0);
    if peak_for_bar > raw_for_bar {
        let peak_x = bar_rect.min.x + bar_rect.width() * (peak_for_bar / 228.0);
        painter.line_segment(
            [egui::pos2(peak_x, bar_rect.min.y), egui::pos2(peak_x, bar_rect.max.y)],
            egui::Stroke::new(2.0, Color32::YELLOW),
        );
    }

    let tick_font = egui::FontId::proportional(9.0);
    for s in 1..=9 {
        let x = bar_rect.min.x + bar_rect.width() * (s as f32 * 12.0 / 228.0);
        painter.line_segment(
            [egui::pos2(x, top_label.max.y - 3.0), egui::pos2(x, top_label.max.y)],
            egui::Stroke::new(1.0, Color32::GRAY),
        );
        painter.line_segment(
            [egui::pos2(x, bottom_label.min.y), egui::pos2(x, bottom_label.min.y + 3.0)],
            egui::Stroke::new(1.0, Color32::GRAY),
        );
        painter.text(
            egui::pos2(x, top_label.min.y),
            egui::Align2::CENTER_TOP,
            format!("{}", s),
            tick_font.clone(),
            Color32::GRAY,
        );
    }

    for db_over in (10..=60).step_by(10) {
        let tick_raw = 108.0 + db_over as f32 * 2.0;
        let x = bar_rect.min.x + bar_rect.width() * (tick_raw / 228.0);
        let tick_color = Color32::from_rgb(200, 100, 100);
        painter.line_segment(
            [egui::pos2(x, top_label.max.y - 3.0), egui::pos2(x, top_label.max.y)],
            egui::Stroke::new(1.0, tick_color),
        );
        painter.line_segment(
            [egui::pos2(x, bottom_label.min.y), egui::pos2(x, bottom_label.min.y + 3.0)],
            egui::Stroke::new(1.0, tick_color),
        );
        painter.text(
            egui::pos2(x, bottom_label.max.y),
            egui::Align2::CENTER_BOTTOM,
            format!("+{}", db_over),
            tick_font.clone(),
            tick_color,
        );
    }

    let text = if raw_value <= 108.0 {
        let s_unit = (raw_value / 12.0).floor().clamp(0.0, 9.0) as u8;
        format!("S{}", s_unit)
    } else {
        let db_over = ((raw_value - 108.0) * 0.5).max(0.0);
        format!("S9+{:.0} dB", db_over)
    };
    painter.text(
        bar_rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(11.0),
        Color32::WHITE,
    );

    rect
}

/// Analog needle S-meter. Size from `override_size` or default 392x120.
/// `value` and `peak_value` carry dBm in RX, watts in TX (same convention as
/// `smeter_bar_sized` above).
pub(crate) fn smeter_analog_sized(ui: &mut egui::Ui, value: f32, peak_value: f32, transmitting: bool, other_tx: bool, override_size: Option<(f32, f32)>) -> egui::Rect {
    let (width, height) = if let Some((w, h)) = override_size {
        (w, h)
    } else {
        (392.0, 120.0)
    };
    let desired_size = Vec2::new(width, height);
    let (rect, _response) = ui.allocate_exact_size(desired_size, egui::Sense::hover());

    if !ui.is_rect_visible(rect) {
        return rect;
    }
    let painter = ui.painter();

    // Background: dark rounded rect
    painter.rect_filled(rect, 6.0, Color32::from_rgb(25, 25, 30));
    painter.rect_stroke(rect, 6.0, egui::Stroke::new(1.0, Color32::from_rgb(60, 60, 70)));

    // Arc geometry - pivot at bottom, radius fits labels inside rect
    let center_x = rect.center().x;
    let center_y = rect.max.y - 4.0;
    let center = egui::pos2(center_x, center_y);
    let max_by_width = width * 0.42;
    let max_by_height = height - 26.0;
    let radius = max_by_width.min(max_by_height).max(30.0);

    // Needle sweep: -145° to -35° (left to right arc, 0° = right)
    let min_angle: f32 = -145.0_f32.to_radians();
    let max_angle: f32 = -35.0_f32.to_radians();

    let (is_tx, frac) = if other_tx || transmitting {
        let watts = value;
        (true, (watts / 100.0).clamp(0.0, 1.0))
    } else {
        let raw = sdr_remote_core::dbm_to_display(value) as f32;
        (false, (raw / 228.0).clamp(0.0, 1.0))
    };

    // Draw scale arc
    let arc_inner = radius - 4.0;
    let n_segments = 60;
    for i in 0..n_segments {
        let t0 = i as f32 / n_segments as f32;
        let t1 = (i + 1) as f32 / n_segments as f32;
        let a0 = min_angle + t0 * (max_angle - min_angle);
        let a1 = min_angle + t1 * (max_angle - min_angle);
        let color = if is_tx {
            Color32::from_rgb(180, 40, 40)
        } else if t0 < 108.0 / 228.0 {
            Color32::from_rgb(0, 140, 0)
        } else {
            Color32::from_rgb(180, 40, 40)
        };
        let p0 = center + egui::vec2(a0.cos(), a0.sin()) * arc_inner;
        let p1 = center + egui::vec2(a1.cos(), a1.sin()) * arc_inner;
        painter.line_segment([p0, p1], egui::Stroke::new(3.0, color));
    }

    // Scale ticks and labels
    let tick_font = egui::FontId::proportional(11.0);
    if is_tx {
        // TX: watt scale
        for &w in &[0, 25, 50, 75, 100] {
            let t = w as f32 / 100.0;
            let angle = min_angle + t * (max_angle - min_angle);
            let outer = center + egui::vec2(angle.cos(), angle.sin()) * (radius + 2.0);
            let inner = center + egui::vec2(angle.cos(), angle.sin()) * (radius - 10.0);
            painter.line_segment([inner, outer], egui::Stroke::new(1.0, Color32::GRAY));
            let text_pos = center + egui::vec2(angle.cos(), angle.sin()) * (radius + 14.0);
            painter.text(text_pos, egui::Align2::CENTER_CENTER,
                format!("{}W", w), tick_font.clone(), Color32::GRAY);
        }
    } else {
        // RX: S-unit scale (S1-S9)
        for s in 1..=9u16 {
            let raw = s as f32 * 12.0;
            let t = raw / 228.0;
            let angle = min_angle + t * (max_angle - min_angle);
            let outer = center + egui::vec2(angle.cos(), angle.sin()) * (radius + 2.0);
            let inner = center + egui::vec2(angle.cos(), angle.sin()) * (radius - 10.0);
            let stroke_w = if s == 9 { 1.5 } else { 1.0 };
            painter.line_segment([inner, outer], egui::Stroke::new(stroke_w, Color32::from_rgb(200, 200, 200)));
            let text_pos = center + egui::vec2(angle.cos(), angle.sin()) * (radius + 14.0);
            let label = if s == 9 { "S9".to_string() } else { format!("{}", s) };
            painter.text(text_pos, egui::Align2::CENTER_CENTER,
                label, tick_font.clone(), Color32::from_rgb(200, 200, 200));
        }
        // dB over S9 ticks
        let db_font = egui::FontId::proportional(9.0);
        for db_over in (10..=60).step_by(10) {
            let raw = 108.0 + db_over as f32 * 2.0;
            let t = raw / 228.0;
            let angle = min_angle + t * (max_angle - min_angle);
            let outer = center + egui::vec2(angle.cos(), angle.sin()) * (radius + 2.0);
            let inner = center + egui::vec2(angle.cos(), angle.sin()) * (radius - 8.0);
            painter.line_segment([inner, outer], egui::Stroke::new(1.0, Color32::from_rgb(200, 100, 100)));
            let text_pos = center + egui::vec2(angle.cos(), angle.sin()) * (radius + 13.0);
            painter.text(text_pos, egui::Align2::CENTER_CENTER,
                format!("+{}", db_over), db_font.clone(), Color32::from_rgb(200, 100, 100));
        }
    }

    // Peak hold needle (thin, yellow) - extends through the scale arc
    if !is_tx && peak_value > value {
        let peak_raw = sdr_remote_core::dbm_to_display(peak_value) as f32;
        let peak_frac = (peak_raw / 228.0).clamp(0.0, 1.0);
        let peak_angle = min_angle + peak_frac * (max_angle - min_angle);
        let tip = center + egui::vec2(peak_angle.cos(), peak_angle.sin()) * (radius + 2.0);
        let base = center + egui::vec2(peak_angle.cos(), peak_angle.sin()) * 15.0;
        painter.line_segment([base, tip], egui::Stroke::new(1.5, Color32::from_rgb(255, 255, 0).gamma_multiply(0.6)));
    }

    // Main needle - extends through the scale arc
    let angle = min_angle + frac * (max_angle - min_angle);
    let tip = center + egui::vec2(angle.cos(), angle.sin()) * (radius + 2.0);
    let needle_color = if is_tx {
        Color32::from_rgb(255, 60, 60)
    } else {
        Color32::WHITE
    };
    painter.line_segment([center, tip], egui::Stroke::new(2.0, needle_color));
    // Pivot dot
    painter.circle_filled(center, 4.0, needle_color);

    rect
}
