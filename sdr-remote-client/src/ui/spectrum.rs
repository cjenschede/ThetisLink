// SPDX-License-Identifier: GPL-2.0-or-later

use super::*;

/// Draw spectrum plot: line graph of power vs frequency.
/// Bins are pre-extracted by the server for the client's zoom/pan view.
/// Configuration for spectrum plot appearance/interaction differences between RX1 and RX2.
pub(crate) struct SpectrumPlotConfig {
    pub(crate) scroll_key: &'static str,
    pub(crate) drag_key: &'static str,
    pub(crate) click_key: &'static str,
    pub(crate) show_band_markers: bool,
    pub(crate) is_popout: bool,
    pub(crate) color_floor: f32, // 0.0=full range (dark blue), 0.2=start at blue, 0.4=start at cyan
}

pub(crate) const RX1_PLOT_CONFIG: SpectrumPlotConfig = SpectrumPlotConfig {
    scroll_key: "spectrum_scroll_freq",
    drag_key: "spectrum_drag_freq",
    click_key: "spectrum_click_freq",
    show_band_markers: true,
    is_popout: false,
    color_floor: 0.25, // skip darkest portion of colormap
};

pub(crate) const RX2_PLOT_CONFIG: SpectrumPlotConfig = SpectrumPlotConfig {
    scroll_key: "rx2_spectrum_scroll_freq",
    drag_key: "rx2_spectrum_drag_freq",
    click_key: "rx2_spectrum_click_freq",
    show_band_markers: false,
    is_popout: true,
    color_floor: 0.25,
};

/// center_freq_hz and span_hz describe the bins frequency window (from server).
/// display_center_hz overrides the display center (VFO-centered, like waterfall).
pub(crate) fn spectrum_plot(
    ui: &mut egui::Ui,
    bins: &[u16],
    center_freq_hz: u32,   // bins center (DDC center at zoom=1)
    span_hz: u32,           // bins span
    display_center_hz: u64, // display center (VFO frequency)
    vfo_hz: u64,            // display VFO marker position
    tune_base_hz: u64,      // actual VFO frequency for scroll/click/drag tuning
    ref_db: f32,
    range_db: f32,
    smeter: u16,
    transmitting: bool,
    other_tx: bool,
    filter_low_hz: i32,
    filter_high_hz: i32,
    rit_offset_hz: i32,
    rit_enable: bool,
    plot_height: f32,
    config: &SpectrumPlotConfig,
    dx_spots: &[sdr_remote_logic::state::DxSpotInfo],
) {
    let width = ui.available_width();
    let plot_height = plot_height.max(80.0);
    let label_height = 18.0;
    let desired_size = Vec2::new(width, plot_height + label_height);
    let (rect, response) = ui.allocate_exact_size(desired_size, egui::Sense::click_and_drag());

    if !ui.is_rect_visible(rect) {
        return;
    }
    let painter = ui.painter_at(rect);
    let plot_rect = egui::Rect::from_min_max(rect.min, Pos2::new(rect.max.x, rect.max.y - label_height));
    let label_strip = egui::Rect::from_min_max(Pos2::new(rect.min.x, plot_rect.max.y), rect.max);

    // Scroll wheel over spectrum = tune VFO in 1 kHz steps, snapped to whole kHz
    // Skip if frequency digit scroll already consumed this event
    let scroll_consumed: bool = ui.memory(|mem| mem.data.get_temp(egui::Id::new("freq_scroll_consumed")).unwrap_or(false));
    if response.hovered() && !scroll_consumed {
        let scroll = ui.input(|i| i.raw_scroll_delta.y);
        if scroll.abs() > 0.1 {
            let steps = if scroll > 0.0 { 1i64 } else { -1i64 };
            let current_khz = ((tune_base_hz + 500) / 1000) as i64;
            let new_khz = current_khz + steps;
            if new_khz > 0 {
                let new_freq = new_khz as u64 * 1000;
                ui.memory_mut(|mem| {
                    mem.data.insert_temp(egui::Id::new(config.scroll_key), new_freq);
                });
            }
        }
    }

    // Background: plot area + label strip
    painter.rect_filled(plot_rect, 2.0, Color32::from_rgb(10, 15, 30));
    painter.rect_filled(label_strip, 0.0, Color32::from_rgb(18, 22, 40));

    let num_bins = bins.len();
    if num_bins == 0 || span_hz == 0 {
        return;
    }

    // Frequency range — display window centered on VFO (like waterfall)
    let visible_span = span_hz as f64;
    let disp_center = display_center_hz as f64;
    let start_hz = disp_center - visible_span / 2.0;
    let end_hz = start_hz + visible_span;

    // Bins frequency range (may differ from display window when CTUN & zoom=1)
    let bins_center_hz = center_freq_hz as f64;
    let bins_start_hz = bins_center_hz - visible_span / 2.0;
    let floor_db = ref_db - range_db;

    // ── Dynamic grid spacing (Thetis-style "nice numbers") ──────────────
    // Pick a "nice" spacing that gives roughly target_ticks lines on screen.
    // Nice numbers: 1, 2, 5 × 10^n (e.g. 100, 200, 500, 1k, 2k, 5k, 10k, ...)

    // Frequency grid: target ~7 vertical lines
    let tick_spacing_hz = {
        let raw = visible_span / 7.0;
        let pow = 10.0f64.powf(raw.log10().floor());
        let n = raw / pow;
        let nice = if n < 1.5 { 1.0 } else if n < 3.5 { 2.0 } else if n < 7.5 { 5.0 } else { 10.0 };
        nice * pow
    };

    // dB grid: target ~6 horizontal lines
    let db_spacing = {
        let raw = range_db / 6.0;
        let pow = 10.0f32.powf(raw.log10().floor());
        let n = raw / pow;
        let nice = if n < 1.5 { 1.0 } else if n < 3.5 { 2.0 } else if n < 7.5 { 5.0 } else { 10.0 };
        nice * pow
    };

    // ── Grid lines (behind everything) ──────────────────────────────────
    let grid_stroke = Stroke::new(1.0, Color32::from_rgb(60, 60, 85));

    // Vertical frequency gridlines (plot area + through label strip)
    let first_tick = (start_hz / tick_spacing_hz).ceil() as i64;
    let last_tick = (end_hz / tick_spacing_hz).floor() as i64;
    for tick_idx in first_tick..=last_tick {
        let freq = tick_idx as f64 * tick_spacing_hz;
        let frac = (freq - start_hz) / visible_span;
        if frac < 0.01 || frac > 0.99 { continue; }
        let x = rect.min.x + frac as f32 * rect.width();
        painter.line_segment(
            [Pos2::new(x, plot_rect.min.y), Pos2::new(x, plot_rect.max.y)],
            grid_stroke,
        );
        // Small tick mark in label strip
        painter.line_segment(
            [Pos2::new(x, label_strip.min.y), Pos2::new(x, label_strip.min.y + 4.0)],
            Stroke::new(1.0, Color32::from_rgb(80, 80, 110)),
        );
    }

    // Horizontal dB gridlines (full width, plot area only)
    let first_db_tick = (floor_db / db_spacing).ceil() as i32;
    let last_db_tick = (ref_db / db_spacing).floor() as i32;
    for db_idx in first_db_tick..=last_db_tick {
        let db = db_idx as f32 * db_spacing;
        let frac = (ref_db - db) / range_db;
        if frac < 0.02 || frac > 0.98 { continue; }
        let y = plot_rect.min.y + frac * plot_rect.height();
        painter.line_segment(
            [Pos2::new(plot_rect.min.x, y), Pos2::new(plot_rect.max.x, y)],
            grid_stroke,
        );
    }

    // ── Filter passband background ──────────────────────────────────────
    if vfo_hz > 0 && visible_span > 0.0 && (filter_low_hz != 0 || filter_high_hz != 0) {
        let lo_hz = vfo_hz as f64 + filter_low_hz as f64;
        let hi_hz = vfo_hz as f64 + filter_high_hz as f64;
        let lo_frac = (lo_hz - start_hz) / visible_span;
        let hi_frac = (hi_hz - start_hz) / visible_span;
        let lo_x = (rect.min.x + lo_frac as f32 * rect.width()).max(rect.min.x);
        let hi_x = (rect.min.x + hi_frac as f32 * rect.width()).min(rect.max.x);
        if hi_x > lo_x {
            painter.rect_filled(
                egui::Rect::from_min_max(
                    Pos2::new(lo_x, plot_rect.min.y),
                    Pos2::new(hi_x, plot_rect.max.y),
                ),
                0.0,
                Color32::from_rgb(25, 30, 45),
            );
            let edge_stroke = Stroke::new(0.5, Color32::from_rgba_premultiplied(200, 200, 0, 120));
            if lo_x > rect.min.x {
                painter.line_segment(
                    [Pos2::new(lo_x, plot_rect.min.y), Pos2::new(lo_x, plot_rect.max.y)],
                    edge_stroke,
                );
            }
            if hi_x < rect.max.x {
                painter.line_segment(
                    [Pos2::new(hi_x, plot_rect.min.y), Pos2::new(hi_x, plot_rect.max.y)],
                    edge_stroke,
                );
            }
        }
    }

    // ── Spectrum line (frequency-mapped, max-per-pixel aggregation) ────
    let server_floor_db = -150.0f32;
    let server_range_db = 120.0f32;
    let pixel_count = plot_rect.width().max(1.0) as usize;
    let bins_f = num_bins as f64;
    let hz_per_bin = if bins_f > 0.0 { visible_span / bins_f } else { 1.0 };
    let points_with_frac: Vec<(Pos2, f32)> = (0..pixel_count).map(|px| {
        let x = plot_rect.min.x + px as f32;
        // Map pixel to frequency, then frequency to bin index
        let freq0 = start_hz + (px as f64 / pixel_count as f64) * visible_span;
        let freq1 = start_hz + ((px + 1) as f64 / pixel_count as f64) * visible_span;
        let b0 = ((freq0 - bins_start_hz) / hz_per_bin).max(0.0);
        let b1 = ((freq1 - bins_start_hz) / hz_per_bin).max(0.0);
        let bs = b0.floor() as usize;
        let be = (b1.ceil() as usize).min(num_bins).max(bs + 1);
        let mut max_val = 0u16;
        if bs < num_bins {
            for i in bs..be.min(num_bins) {
                max_val = max_val.max(bins[i]);
            }
        }
        let db = server_floor_db + (max_val as f32 / 65535.0) * server_range_db;
        let frac = (ref_db - db) / range_db;
        let y = plot_rect.min.y + frac * plot_rect.height();
        (Pos2::new(x, y.clamp(plot_rect.min.y, plot_rect.max.y)), frac.clamp(0.0, 1.0))
    }).collect();
    let points: Vec<Pos2> = points_with_frac.iter().map(|(p, _)| *p).collect();

    // ── VFO frequency label (behind spectrum) ──────────────────────────
    // Compute VFO text position first; draw label behind spectrum, line on top with gap
    let mut vfo_text_rect: Option<egui::Rect> = None;
    let mut vfo_x: Option<f32> = None;
    if vfo_hz > 0 && visible_span > 0.0 {
        let vfo_frac = (vfo_hz as f64 - start_hz) / visible_span;
        if (0.0..=1.0).contains(&vfo_frac) {
            let x = rect.min.x + vfo_frac as f32 * rect.width();
            vfo_x = Some(x);
            let vfo_mhz = vfo_hz as f32 / 1_000_000.0;
            let vfo_text = format!("{:.3}", vfo_mhz);
            let vfo_font = egui::FontId::proportional(24.0);
            let vfo_color = Color32::from_rgb(255, 120, 120);
            let galley = painter.layout_no_wrap(vfo_text, vfo_font, vfo_color);
            let text_pos = egui::Align2::CENTER_TOP.anchor_size(Pos2::new(x, plot_rect.min.y + 2.0), galley.size());
            let bg_rect = text_pos.expand(2.0);
            vfo_text_rect = Some(bg_rect);
            painter.rect_filled(bg_rect, 2.0, Color32::from_rgba_premultiplied(10, 15, 30, 220));
            painter.galley(text_pos.min, galley, vfo_color);
        }
    }

    // Fill under curve with level-dependent colors
    if points_with_frac.len() >= 2 {
        let bottom_y = plot_rect.max.y;
        let mut mesh = egui::Mesh::default();
        for (i, (pt, frac)) in points_with_frac.iter().enumerate() {
            let uv = egui::epaint::WHITE_UV;
            let (r, g, b) = spectrum_level_color_with_floor(1.0 - *frac, config.color_floor);
            let fill_color = Color32::from_rgba_premultiplied(r, g, b, 50);
            let bottom_color = Color32::from_rgba_premultiplied(r / 4, g / 4, b / 4, 20);
            mesh.vertices.push(egui::epaint::Vertex { pos: *pt, uv, color: fill_color });
            mesh.vertices.push(egui::epaint::Vertex { pos: Pos2::new(pt.x, bottom_y), uv, color: bottom_color });
            if i > 0 {
                let base = (i as u32 - 1) * 2;
                mesh.indices.extend_from_slice(&[base, base + 1, base + 2]);
                mesh.indices.extend_from_slice(&[base + 1, base + 3, base + 2]);
            }
        }
        painter.add(egui::Shape::Mesh(mesh));
    }

    // Spectrum line with level-dependent color (per-segment)
    if points_with_frac.len() >= 2 {
        for i in 1..points_with_frac.len() {
            let (p0, f0) = &points_with_frac[i - 1];
            let (p1, f1) = &points_with_frac[i];
            let avg_frac = (f0 + f1) / 2.0;
            let (r, g, b) = spectrum_level_color_with_floor(1.0 - avg_frac, config.color_floor);
            painter.line_segment([*p0, *p1], Stroke::new(1.5, Color32::from_rgb(r, g, b)));
        }
    }

    // ── VFO line (on top of spectrum, interrupted at text) ──────────────
    if let Some(x) = vfo_x {
        let vfo_stroke = Stroke::new(2.0, Color32::from_rgba_premultiplied(255, 50, 50, 180));
        if let Some(tr) = vfo_text_rect {
            // Line above text (if any space)
            if plot_rect.min.y < tr.min.y {
                painter.line_segment(
                    [Pos2::new(x, plot_rect.min.y), Pos2::new(x, tr.min.y)],
                    vfo_stroke,
                );
            }
            // Line below text
            painter.line_segment(
                [Pos2::new(x, tr.max.y), Pos2::new(x, plot_rect.max.y)],
                vfo_stroke,
            );
        } else {
            painter.line_segment(
                [Pos2::new(x, plot_rect.min.y), Pos2::new(x, plot_rect.max.y)],
                vfo_stroke,
            );
        }
    }

    // ── RIT offset marker ─────────────────────────────────────────────────
    if rit_enable && rit_offset_hz != 0 && vfo_hz > 0 && visible_span > 0.0 {
        let rit_hz = vfo_hz as f64 + rit_offset_hz as f64;
        let rit_frac = (rit_hz - start_hz) / visible_span;
        if (0.0..=1.0).contains(&rit_frac) {
            let x = rect.min.x + rit_frac as f32 * rect.width();
            let rit_color = Color32::from_rgba_premultiplied(0, 200, 255, 160);
            // Dashed line
            let dash_len = 6.0;
            let gap_len = 4.0;
            let mut y = plot_rect.min.y;
            while y < plot_rect.max.y {
                let y_end = (y + dash_len).min(plot_rect.max.y);
                painter.line_segment(
                    [Pos2::new(x, y), Pos2::new(x, y_end)],
                    Stroke::new(1.5, rit_color),
                );
                y += dash_len + gap_len;
            }
            // Label
            let rit_text = format!("RIT {:+}", rit_offset_hz);
            let rit_font = egui::FontId::proportional(11.0);
            let galley = painter.layout_no_wrap(rit_text, rit_font, rit_color);
            let text_x = x + 3.0;
            let text_y = plot_rect.min.y + 30.0;
            painter.galley(Pos2::new(text_x, text_y), galley, rit_color);
        }
    }

    // ── DX Cluster spot markers ──────────────────────────────────────────
    // Spots slide from top to 3/4 height over their lifetime, then fade out in the last 20%.
    if !dx_spots.is_empty() && visible_span > 0.0 {
        let spot_font = egui::FontId::new(13.0, egui::FontFamily::Monospace);
        let now = std::time::Instant::now();
        let plot_h = plot_rect.height();
        for spot in dx_spots {
            let spot_frac = (spot.frequency_hz as f64 - start_hz) / visible_span;
            if spot_frac < 0.0 || spot_frac > 1.0 {
                continue;
            }
            let x = rect.min.x + spot_frac as f32 * rect.width();

            let expiry = spot.expiry_seconds.max(60) as f32;
            let total_age = spot.age_seconds as f32 + now.duration_since(spot.received).as_secs_f32();
            let age_frac = (total_age / expiry).clamp(0.0, 1.0); // 0.0 = new, 1.0 = expired

            // Alpha: full until 80% of lifetime, then fade to 0
            let alpha = if age_frac > 0.8 {
                ((1.0 - age_frac) / 0.2).clamp(0.0, 1.0)
            } else {
                1.0
            };
            let alpha_u8 = (alpha * 200.0) as u8;
            if alpha_u8 == 0 { continue; }

            // Color based on mode
            let [r, g, b, _] = sdr_remote_logic::state::mode_color_rgba(&spot.mode, 255);
            let color = Color32::from_rgba_premultiplied(
                (r as u16 * alpha_u8 as u16 / 255) as u8,
                (g as u16 * alpha_u8 as u16 / 255) as u8,
                (b as u16 * alpha_u8 as u16 / 255) as u8,
                alpha_u8,
            );

            // Callsign label position: slides from top to 3/4 of plot height
            let label_y = plot_rect.min.y + 2.0 + age_frac * plot_h * 0.75;

            // Dashed vertical line from label to bottom
            let dash_len = 4.0;
            let gap_len = 3.0;
            let line_color = Color32::from_rgba_premultiplied(
                (r as u16 * (alpha_u8 / 2) as u16 / 255) as u8,
                (g as u16 * (alpha_u8 / 2) as u16 / 255) as u8,
                (b as u16 * (alpha_u8 / 2) as u16 / 255) as u8,
                alpha_u8 / 2,
            );
            let mut y = plot_rect.min.y;
            while y < plot_rect.max.y {
                let end_y = (y + dash_len).min(plot_rect.max.y);
                painter.line_segment(
                    [Pos2::new(x, y), Pos2::new(x, end_y)],
                    Stroke::new(1.0, line_color),
                );
                y += dash_len + gap_len;
            }

            // Bold callsign label
            let galley = painter.layout_no_wrap(
                spot.callsign.clone(),
                spot_font.clone(),
                color,
            );
            let text_pos = Pos2::new(x + 3.0, label_y);
            let bg_rect = egui::Rect::from_min_size(text_pos, galley.size()).expand(2.0);
            painter.rect_filled(bg_rect, 2.0, Color32::from_rgba_premultiplied(10, 15, 30, (alpha * 220.0) as u8));
            painter.galley(text_pos, galley, color);
        }
    }

    // ── Grid labels (on top of spectrum) ────────────────────────────────
    let grid_font = egui::FontId::proportional(11.0);
    let grid_color = Color32::from_rgb(200, 200, 210);

    // Frequency labels in bottom label strip (clear of spectrum data)
    let freq_label_font = egui::FontId::proportional(11.0);
    let freq_label_color = Color32::from_rgb(220, 220, 230);
    for tick_idx in first_tick..=last_tick {
        let freq = tick_idx as f64 * tick_spacing_hz;
        let frac = (freq - start_hz) / visible_span;
        if frac < 0.02 || frac > 0.98 { continue; }
        let x = rect.min.x + frac as f32 * rect.width();

        let freq_mhz = freq / 1_000_000.0;
        let label = if tick_spacing_hz >= 1_000_000.0 {
            format!("{:.0}", freq_mhz)
        } else if tick_spacing_hz >= 100_000.0 {
            format!("{:.1}", freq_mhz)
        } else if tick_spacing_hz >= 10_000.0 {
            format!("{:.2}", freq_mhz)
        } else if tick_spacing_hz >= 1_000.0 {
            format!("{:.3}", freq_mhz)
        } else {
            format!("{:.4}", freq_mhz)
        };

        painter.text(
            Pos2::new(x, label_strip.min.y + label_strip.height() / 2.0),
            egui::Align2::CENTER_CENTER,
            label,
            freq_label_font.clone(),
            freq_label_color,
        );
    }

    // dB labels at left edge (with background for readability)
    let bg_color = Color32::from_rgba_premultiplied(10, 15, 30, 220);
    for db_idx in first_db_tick..=last_db_tick {
        let db = db_idx as f32 * db_spacing;
        let frac = (ref_db - db) / range_db;
        if frac < 0.02 || frac > 0.98 { continue; }
        let y = plot_rect.min.y + frac * plot_rect.height();
        let db_text = format!("{:.0}", db);
        let galley = painter.layout_no_wrap(db_text, grid_font.clone(), grid_color);
        let text_pos = egui::Align2::LEFT_TOP.anchor_size(Pos2::new(plot_rect.min.x + 2.0, y + 1.0), galley.size());
        painter.rect_filled(text_pos.expand(1.0), 1.0, bg_color);
        painter.galley(text_pos.min, galley, grid_color);
    }

    // ── Band markers (RX1 only) ─────────────────────────────────────────
    if config.show_band_markers {
    let bands: &[(f64, &str)] = &[
        (1.8e6, "160m"), (3.5e6, "80m"), (7.0e6, "40m"), (10.1e6, "30m"),
        (14.0e6, "20m"), (18.068e6, "17m"), (21.0e6, "15m"), (24.89e6, "12m"),
        (28.0e6, "10m"), (50.0e6, "6m"),
    ];
    let band_font = egui::FontId::proportional(10.0);
    for &(freq, label) in bands {
        if freq < start_hz || freq > end_hz { continue; }
        let frac = (freq - start_hz) / visible_span;
        let x = rect.min.x + frac as f32 * rect.width();
        painter.line_segment(
            [Pos2::new(x, plot_rect.max.y - 26.0), Pos2::new(x, plot_rect.max.y - 14.0)],
            Stroke::new(1.0, Color32::from_rgb(100, 80, 40)),
        );
        let band_text = label.to_string();
        let band_color = Color32::from_rgb(170, 140, 70);
        let galley = painter.layout_no_wrap(band_text, band_font.clone(), band_color);
        let text_pos = egui::Align2::CENTER_BOTTOM.anchor_size(Pos2::new(x, plot_rect.max.y - 27.0), galley.size());
        painter.rect_filled(text_pos.expand(1.0), 1.0, bg_color);
        painter.galley(text_pos.min, galley, band_color);
    }
    } // show_band_markers

    // ── S-meter / TX power overlay (top-right) ──────────────────────────
    let meter_text = if other_tx {
        let watts = smeter as f32 / 10.0;
        format!("TX in use: {:.0}W", watts)
    } else if transmitting {
        let watts = smeter as f32 / 10.0;
        format!("TX: {:.0}W", watts)
    } else if smeter <= 108 {
        let s_unit = smeter / 12;
        format!("S{}", s_unit)
    } else {
        let db_over = ((smeter as f32 - 108.0) * 60.0 / 152.0).round() as u16;
        format!("S9+{}dB", db_over)
    };

    let meter_color = if transmitting || other_tx {
        Color32::from_rgb(255, 80, 80)
    } else {
        Color32::from_rgb(0, 220, 0)
    };

    {
        let meter_font = egui::FontId::proportional(if config.is_popout { 42.0 } else { 21.0 });
        let galley = painter.layout_no_wrap(meter_text, meter_font, meter_color);
        let text_pos = egui::Align2::RIGHT_TOP.anchor_size(Pos2::new(plot_rect.max.x - 4.0, plot_rect.min.y + 4.0), galley.size());
        painter.rect_filled(text_pos.expand(2.0), 2.0, bg_color);
        painter.galley(text_pos.min, galley, meter_color);
    }

    // ── Span indicator (top-left, below VFO) ────────────────────────────
    let span_khz = visible_span / 1000.0;
    if span_khz < 1536.0 {
        let label = if span_khz < 100.0 {
            format!("{:.1} kHz", span_khz)
        } else {
            format!("{:.0} kHz", span_khz)
        };
        let span_font = egui::FontId::proportional(11.0);
        let span_color = Color32::from_rgb(220, 220, 80);
        let galley = painter.layout_no_wrap(label, span_font, span_color);
        let text_pos = egui::Align2::LEFT_TOP.anchor_size(Pos2::new(plot_rect.min.x + 2.0, plot_rect.min.y + 14.0), galley.size());
        painter.rect_filled(text_pos.expand(2.0), 2.0, bg_color);
        painter.galley(text_pos.min, galley, span_color);
    }

    // ── Filter edge positions for drag detection ─────────────────────
    let filter_lo_x = if vfo_hz > 0 && visible_span > 0.0 {
        let lo_hz = vfo_hz as f64 + filter_low_hz as f64;
        let frac = (lo_hz - start_hz) / visible_span;
        if (0.0..=1.0).contains(&frac) { Some(rect.min.x + frac as f32 * rect.width()) } else { None }
    } else { None };
    let filter_hi_x = if vfo_hz > 0 && visible_span > 0.0 {
        let hi_hz = vfo_hz as f64 + filter_high_hz as f64;
        let frac = (hi_hz - start_hz) / visible_span;
        if (0.0..=1.0).contains(&frac) { Some(rect.min.x + frac as f32 * rect.width()) } else { None }
    } else { None };

    // Detect if mouse is near a filter edge (inside plot area only)
    let grab_dist = 8.0;   // Drag grab zone
    let cursor_dist = 3.0; // Cursor icon zone (tight)
    let hover_pos = response.hover_pos().filter(|p| plot_rect.contains(*p));
    let near_lo = hover_pos.and_then(|p| filter_lo_x.map(|x| (p.x - x).abs() < grab_dist)).unwrap_or(false);
    let near_hi = hover_pos.and_then(|p| filter_hi_x.map(|x| (p.x - x).abs() < grab_dist)).unwrap_or(false);
    let cursor_lo = hover_pos.and_then(|p| filter_lo_x.map(|x| (p.x - x).abs() < cursor_dist)).unwrap_or(false);
    let cursor_hi = hover_pos.and_then(|p| filter_hi_x.map(|x| (p.x - x).abs() < cursor_dist)).unwrap_or(false);

    // Track filter grab state (persist across frames).
    // "hover_ready" remembers that we were near a filter edge BEFORE drag started.
    // This prevents the grab decision from failing when the mouse moves during drag start.
    let drag_state_id = egui::Id::new(config.drag_key).with("filter_drag");
    let hover_ready_id = drag_state_id.with("hover_ready");
    let dragging_filter: Option<bool> = ui.memory(|mem| mem.data.get_temp(drag_state_id));

    // While hovering (not dragging): track if we're near a filter edge
    if !response.dragged() {
        let ready = near_lo || near_hi;
        ui.memory_mut(|mem| mem.data.insert_temp(hover_ready_id, ready));
        if cursor_lo || cursor_hi {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
    }

    // ── Interaction ─────────────────────────────────────────────────────
    if response.dragged() {
        if let Some(pos) = response.interact_pointer_pos() {
            // On drag start: use hover_ready state (set before mouse moved)
            let filter_edge = dragging_filter.unwrap_or_else(|| {
                let was_near = ui.memory(|mem| mem.data.get_temp::<bool>(hover_ready_id)).unwrap_or(false);
                ui.memory_mut(|mem| mem.data.insert_temp(drag_state_id, was_near));
                was_near
            });

            if filter_edge {
                // Dragging a filter edge
                let freq_hz = start_hz + (pos.x - rect.min.x) as f64 / rect.width() as f64 * visible_span;
                let offset_hz = (freq_hz - vfo_hz as f64).round() as i32;
                // Determine which edge: use the one that was closest at drag start
                let is_low = dragging_filter.is_none() && {
                        let dl = filter_lo_x.map(|x| (pos.x - x).abs()).unwrap_or(f32::MAX);
                        let dh = filter_hi_x.map(|x| (pos.x - x).abs()).unwrap_or(f32::MAX);
                        dl < dh
                    }
                    || dragging_filter.is_some() && {
                        // Check which edge key is already set
                        ui.memory(|mem| mem.data.get_temp::<bool>(drag_state_id.with("is_low"))).unwrap_or(near_lo)
                    };
                // Persist which edge we're dragging
                ui.memory_mut(|mem| mem.data.insert_temp(drag_state_id.with("is_low"), is_low));

                let key = if is_low { "spectrum_filter_low" } else { "spectrum_filter_high" };
                ui.memory_mut(|mem| {
                    mem.data.insert_temp(egui::Id::new(key), offset_hz);
                });
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            } else {
                // Normal VFO drag
                let frac = (pos.x - rect.min.x) / rect.width();
                let freq_hz = (start_hz + frac as f64 * visible_span) as u64;
                if freq_hz > 1000 {
                    let rounded = (freq_hz / 100) * 100;
                    ui.memory_mut(|mem| {
                        mem.data.insert_temp(egui::Id::new(config.drag_key), rounded);
                    });
                }
            }
        }
    } else {
        // Clear drag state when not dragging
        if dragging_filter.is_some() {
            ui.memory_mut(|mem| {
                mem.data.remove::<bool>(drag_state_id);
                mem.data.remove::<bool>(drag_state_id.with("is_low"));
            });
        }

        if response.clicked() {
            if let Some(pos) = response.interact_pointer_pos() {
                // Check if click is near a DX spot (snap to spot frequency)
                let mut clicked_freq: Option<u64> = None;
                if visible_span > 0.0 {
                    for spot in dx_spots {
                        let spot_frac = (spot.frequency_hz as f64 - start_hz) / visible_span;
                        if spot_frac >= 0.0 && spot_frac <= 1.0 {
                            let spot_x = rect.min.x + spot_frac as f32 * rect.width();
                            if (pos.x - spot_x).abs() < 15.0 {
                                clicked_freq = Some(spot.frequency_hz);
                                break;
                            }
                        }
                    }
                }
                let freq_hz = clicked_freq.unwrap_or_else(|| {
                    let frac = (pos.x - rect.min.x) / rect.width();
                    let f = (start_hz + frac as f64 * visible_span) as u64;
                    (f / 1000) * 1000 // round to 1 kHz
                });
                if freq_hz > 1000 {
                    ui.memory_mut(|mem| {
                        mem.data.insert_temp(egui::Id::new(config.click_key), freq_hz);
                    });
                }
            }
        }
    }
}

/// Draw waterfall with hybrid resolution: high-res extracted view + full DDC fallback.
/// Ring buffer push is handled by WaterfallRingBuffer::push() before rendering.
pub(crate) fn render_waterfall(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    wf: &mut WaterfallRingBuffer,
    full_span_hz: u32,
    display_center: u64,  // pre-computed display center (smooth VFO + pan)
    tune_base_hz: u64,
    zoom: f32,
    contrast: f32,
    ref_db: f32,
    range_db: f32,
    display_height: f32,
    config: &SpectrumPlotConfig,
) {
    let width = ui.available_width();
    let display_height = display_height.max(60.0);

    if wf.count == 0 || full_span_hz == 0 {
        let desired_size = Vec2::new(width, display_height);
        ui.allocate_exact_size(desired_size, egui::Sense::hover());
        return;
    }

    let wf_height = wf.height;
    let ddc_span_f = full_span_hz as f64;

    // View in absolute frequency space — use the most recent view span from the
    // spectrum packet rather than recomputing from ddc_span/zoom, because the
    // server's integer FFT bin count can differ slightly from ddc_span/zoom,
    // causing a scaling mismatch between spectrum line and waterfall at the edges.
    let newest_idx = (wf.write_idx + wf.height - 1) % wf.height;
    let view_span_from_packet = wf.view_spans[newest_idx] as f64;
    let display_center_hz = display_center as f64;
    let display_span_hz = if view_span_from_packet > 0.0 { view_span_from_packet } else { ddc_span_f / (zoom as f64).max(1.0) };
    let display_start_hz = display_center_hz - display_span_hz / 2.0;

    let out_width = (width.max(1.0) as usize).min(1024);
    let mut pixels = vec![0u8; out_width * wf_height * 4];
    let px_hz_step = display_span_hz / out_width as f64;

    for row in 0..wf_height {
        if row >= wf.count { continue; }
        let src_row_idx = (wf.write_idx + wf_height - 1 - row) % wf_height;

        // Get both data sources for this row
        let row_full = &wf.full_rows[src_row_idx];
        let row_full_center = wf.full_centers[src_row_idx] as f64;
        let row_view = &wf.view_rows[src_row_idx];
        let row_view_center = wf.view_centers[src_row_idx] as f64;
        let row_view_span = wf.view_spans[src_row_idx] as f64;

        if row_full.is_empty() || row_full_center == 0.0 { continue; }

        // Precompute full DDC bin mapping for this row
        let full_len = row_full.len() as f64;
        let full_hz_per_bin = ddc_span_f / full_len;
        let full_start_hz = row_full_center - ddc_span_f / 2.0;

        // Precompute extracted view bin mapping (if available)
        let has_view = !row_view.is_empty() && row_view_span > 0.0;
        let view_len = if has_view { row_view.len() as f64 } else { 0.0 };
        let view_hz_per_bin = if has_view { row_view_span / view_len } else { 1.0 };
        let view_start_hz = if has_view { row_view_center - row_view_span / 2.0 } else { 0.0 };
        let view_end_hz = if has_view { row_view_center + row_view_span / 2.0 } else { 0.0 };

        let dst_start = row * out_width * 4;

        for px in 0..out_width {
            let px_start_hz = display_start_hz + px as f64 * px_hz_step;
            let px_end_hz = px_start_hz + px_hz_step;
            let px_mid_hz = (px_start_hz + px_end_hz) / 2.0;

            // Try high-res extracted view first (if pixel falls within its coverage)
            let max_val = if has_view && px_mid_hz >= view_start_hz && px_mid_hz < view_end_hz {
                let b0_f = (px_start_hz - view_start_hz) / view_hz_per_bin;
                let b1_f = (px_end_hz - view_start_hz) / view_hz_per_bin;
                let b0 = (b0_f as isize).max(0) as usize;
                let b1 = ((b1_f.ceil() as usize).max(b0 + 1)).min(row_view.len());
                let mut mv = 0u16;
                for j in b0..b1 { mv = mv.max(row_view[j]); }
                mv
            } else {
                // Fall back to full DDC row
                let b0_f = (px_start_hz - full_start_hz) / full_hz_per_bin;
                let b1_f = (px_end_hz - full_start_hz) / full_hz_per_bin;
                let b0 = b0_f as isize;
                let b1 = (b1_f.ceil() as isize).max(b0 + 1);
                if b1 <= 0 || b0 >= row_full.len() as isize { continue; }
                let b0c = b0.max(0) as usize;
                let b1c = (b1 as usize).min(row_full.len());
                let mut mv = 0u16;
                for j in b0c..b1c { mv = mv.max(row_full[j]); }
                mv
            };

            // Same normalization as spectrum plot: raw→dB→frac→color
            let server_floor_db = -150.0f32;
            let server_range_db = 120.0f32;
            let db = server_floor_db + (max_val as f32 / 65535.0) * server_range_db;
            let frac = ((ref_db - db) / range_db).clamp(0.0, 1.0); // 0=top(strong), 1=bottom(weak)
            let level = (1.0 - frac).powf(1.0 / contrast).clamp(0.0, 1.0); // contrast as gamma
            let (r, g, b) = spectrum_level_color_with_floor(level, config.color_floor);
            let idx = dst_start + px * 4;
            pixels[idx] = r;
            pixels[idx + 1] = g;
            pixels[idx + 2] = b;
            pixels[idx + 3] = 255;
        }
    }

    let color_image = egui::ColorImage::from_rgba_unmultiplied(
        [out_width, wf_height],
        &pixels,
    );

    match &mut wf.texture {
        Some(tex) => {
            tex.set(color_image, egui::TextureOptions::LINEAR);
        }
        None => {
            wf.texture = Some(ctx.load_texture(
                "waterfall",
                color_image,
                egui::TextureOptions::LINEAR,
            ));
        }
    }

    if let Some(tex) = &wf.texture {
        let desired_size = Vec2::new(width, display_height);
        let (rect, response) = ui.allocate_exact_size(desired_size, egui::Sense::click_and_drag());

        // Scroll wheel over waterfall = tune VFO in 1 kHz steps
        let wf_scroll_consumed: bool = ui.memory(|mem| mem.data.get_temp(egui::Id::new("freq_scroll_consumed")).unwrap_or(false));
        if response.hovered() && !wf_scroll_consumed {
            let scroll = ui.input(|i| i.raw_scroll_delta.y);
            if scroll.abs() > 0.1 {
                let steps = if scroll > 0.0 { 1i64 } else { -1i64 };
                let current_khz = ((tune_base_hz + 500) / 1000) as i64;
                let new_khz = current_khz + steps;
                if new_khz > 0 {
                    let new_freq = new_khz as u64 * 1000;
                    ui.memory_mut(|mem| {
                        mem.data.insert_temp(egui::Id::new(config.scroll_key), new_freq);
                    });
                }
            }
        }

        // Click/drag on waterfall = tune to frequency (same as spectrum)
        if response.dragged() {
            if let Some(pos) = response.interact_pointer_pos() {
                let frac = (pos.x - rect.min.x) / rect.width();
                let freq_hz = (display_start_hz + frac as f64 * display_span_hz) as u64;
                if freq_hz > 1000 {
                    let rounded = (freq_hz / 100) * 100;
                    ui.memory_mut(|mem| {
                        mem.data.insert_temp(egui::Id::new(config.drag_key), rounded);
                    });
                }
            }
        } else if response.clicked() {
            if let Some(pos) = response.interact_pointer_pos() {
                let frac = (pos.x - rect.min.x) / rect.width();
                let freq_hz = (display_start_hz + frac as f64 * display_span_hz) as u64;
                if freq_hz > 1000 {
                    let rounded = (freq_hz / 1000) * 1000;
                    ui.memory_mut(|mem| {
                        mem.data.insert_temp(egui::Id::new(config.click_key), rounded);
                    });
                }
            }
        }

        if ui.is_rect_visible(rect) {
            let uv = egui::Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
            let painter = ui.painter_at(rect);
            painter.image(tex.id(), rect, uv, Color32::WHITE);
        }
    }
}

/// Map 8-bit power value to RGB color (waterfall colormap)
/// Black → Blue → Cyan → Yellow → Red → White
pub(crate) fn waterfall_colormap(value: u8) -> (u8, u8, u8) {
    let v = value as f32 / 255.0;
    let (r, g, b) = if v < 0.2 {
        // Black → Blue
        let t = v / 0.2;
        (0.0, 0.0, t)
    } else if v < 0.4 {
        // Blue → Cyan
        let t = (v - 0.2) / 0.2;
        (0.0, t, 1.0)
    } else if v < 0.6 {
        // Cyan → Yellow
        let t = (v - 0.4) / 0.2;
        (t, 1.0, 1.0 - t)
    } else if v < 0.8 {
        // Yellow → Red
        let t = (v - 0.6) / 0.2;
        (1.0, 1.0 - t, 0.0)
    } else {
        // Red → White
        let t = (v - 0.8) / 0.2;
        (1.0, t, t)
    };
    ((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}

/// Map normalized level (0.0=bottom/weak → 1.0=top/strong) to RGB for spectrum line/fill.
/// Uses waterfall colormap with adjustable floor (skips dark end of the scale).
/// floor: 0.0=full range (starts at black), 0.3=starts at cyan, 0.5=starts at yellow
fn spectrum_level_color_with_floor(level: f32, floor: f32) -> (u8, u8, u8) {
    let mapped = floor + level.clamp(0.0, 1.0) * (1.0 - floor);
    waterfall_colormap((mapped * 255.0) as u8)
}
