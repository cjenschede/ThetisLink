// SPDX-License-Identifier: GPL-2.0-or-later

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use egui::{Color32, RichText};

use crate::rf2k::{self, Rf2k};

pub(super) fn render_rf2k_panel(
    ui: &mut egui::Ui,
    rf2k: &Rf2k,
    status: &rf2k::Rf2kStatus,
    peak_power: &mut u16,
    peak_time: &mut std::time::Instant,
    active_pa: &Arc<AtomicU8>,
    confirm_fw_close: &mut bool,
) {
    render_rf2k_header(ui, rf2k, status, active_pa, confirm_fw_close);
    render_rf2k_error_bar(ui, rf2k, status);
    render_rf2k_power_display(ui, status, peak_power, peak_time);
    render_rf2k_band_freq(ui, status);
    render_rf2k_telemetry(ui, status);
    render_rf2k_antennas(ui, rf2k, status);
    render_rf2k_operating_controls(ui, rf2k, status);
    render_rf2k_tuner(ui, rf2k, status);
    render_rf2k_drive_control(ui, rf2k, status, active_pa);
}

// ── Section 1: Header ──────────────────────────────────────────────────────

fn render_rf2k_header(
    ui: &mut egui::Ui,
    rf2k_dev: &Rf2k,
    status: &rf2k::Rf2kStatus,
    active_pa: &Arc<AtomicU8>,
    confirm_fw_close: &mut bool,
) {
    let is_active = active_pa.load(Ordering::Relaxed) == 2;
    ui.horizontal(|ui| {
        ui.label(RichText::new("RF2K-S").strong().size(14.0));
        let title = if status.device_name.is_empty() { "" } else { &status.device_name };
        if !title.is_empty() {
            ui.label(title);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if status.connected {
                ui.colored_label(Color32::GREEN, "Online");
            } else {
                ui.colored_label(Color32::RED, "Offline");
            }
        });
    });
    ui.horizontal(|ui| {
        let mut active = is_active;
        if ui.checkbox(&mut active, "Active").changed() {
            active_pa.store(if active { 2 } else { 0 }, Ordering::Relaxed);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.add_enabled(status.connected,
                egui::Button::new(RichText::new("FW Close").color(Color32::from_rgb(255, 80, 80)))
            ).clicked() {
                *confirm_fw_close = true;
            }
        });
    });

    // FW Close confirmation dialog
    if *confirm_fw_close {
        egui::Window::new("FW Close")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                ui.label("Hiermee wordt de RF2K-S controller afgesloten.");
                ui.label("Weet je het zeker?");
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Ja, afsluiten").color(Color32::from_rgb(255, 80, 80))).clicked() {
                        rf2k_dev.send_command(rf2k::Rf2kCmd::Close);
                        *confirm_fw_close = false;
                    }
                    if ui.button("Cancel").clicked() {
                        *confirm_fw_close = false;
                    }
                });
            });
    }

    ui.separator();
}

// ── Section 2: Error bar (conditional) ─────────────────────────────────────

fn render_rf2k_error_bar(
    ui: &mut egui::Ui,
    rf2k: &Rf2k,
    status: &rf2k::Rf2kStatus,
) {
    if status.error_state == 0 {
        return;
    }
    let error_bg = Color32::from_rgb(80, 20, 20);
    egui::Frame::none()
        .fill(error_bg)
        .inner_margin(4.0)
        .rounding(2.0)
        .show(ui, |ui: &mut egui::Ui| {
            ui.horizontal(|ui: &mut egui::Ui| {
                ui.colored_label(Color32::from_rgb(255, 80, 80),
                    RichText::new(format!("\u{26A0} {}", status.error_text)).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui: &mut egui::Ui| {
                    if ui.button("Reset").clicked() {
                        rf2k.send_command(rf2k::Rf2kCmd::ErrorReset);
                    }
                });
            });
        });
    ui.add_space(2.0);
}

// ── Section 3: Power Display (3 bars) ──────────────────────────────────────

fn render_rf2k_power_display(
    ui: &mut egui::Ui,
    status: &rf2k::Rf2kStatus,
    peak_power: &mut u16,
    peak_time: &mut std::time::Instant,
) {
    let amber = Color32::from_rgb(255, 170, 40);

    // Peak hold logic
    let now = std::time::Instant::now();
    if status.forward_w > *peak_power {
        *peak_power = status.forward_w;
        *peak_time = now;
    } else if now.duration_since(*peak_time).as_millis() > 1000 {
        *peak_power = status.forward_w;
        *peak_time = now;
    }

    // Forward power bar — auto-scale
    let (max_fwd, fwd_divs): (f32, &[(f32, &str)]) = if *peak_power > 1000 {
        (1500.0, &[(0.0,"0"),(300.0,"300"),(600.0,"600"),(900.0,"900"),(1200.0,"1200"),(1500.0,"1500")])
    } else if *peak_power > 500 {
        (1000.0, &[(0.0,"0"),(200.0,"200"),(400.0,"400"),(600.0,"600"),(800.0,"800"),(1000.0,"1000")])
    } else if *peak_power > 200 {
        (500.0, &[(0.0,"0"),(100.0,"100"),(200.0,"200"),(300.0,"300"),(400.0,"400"),(500.0,"500")])
    } else {
        (200.0, &[(0.0,"0"),(50.0,"50"),(100.0,"100"),(150.0,"150"),(200.0,"200")])
    };
    let fwd_frac = (status.forward_w as f32 / max_fwd).clamp(0.0, 1.0);
    let fwd_color = if fwd_frac > 0.9 { Color32::from_rgb(255, 80, 80) }
        else if fwd_frac > 0.7 { amber }
        else { Color32::from_rgb(50, 180, 50) };
    let peak_frac = (*peak_power as f32 / max_fwd).clamp(0.0, 1.0);
    let fwd_value = format!("({} W / {} W)", status.forward_w, *peak_power);
    render_rf2k_meter(ui, "Forward", &fwd_value, fwd_frac, fwd_color,
        Some(peak_frac), fwd_divs, max_fwd);

    // SWR + Reflected: collapsible
    let swr = status.swr_x100 as f32 / 100.0;
    let swr_summary = format!("SWR {:.2}  |  Reflected {} W", swr, status.reflected_w);
    egui::CollapsingHeader::new(swr_summary)
        .default_open(false)
        .show(ui, |ui| {
            // Reflected power bar — auto-scale
            let (max_ref, ref_divs): (f32, &[(f32, &str)]) = if status.reflected_w > 100 {
                (250.0, &[(0.0,"0"),(50.0,"50"),(100.0,"100"),(150.0,"150"),(200.0,"200"),(250.0,"250")])
            } else if status.reflected_w > 50 {
                (100.0, &[(0.0,"0"),(25.0,"25"),(50.0,"50"),(75.0,"75"),(100.0,"100")])
            } else {
                (50.0, &[(0.0,"0"),(10.0,"10"),(20.0,"20"),(30.0,"30"),(40.0,"40"),(50.0,"50")])
            };
            let ref_frac = (status.reflected_w as f32 / max_ref).clamp(0.0, 1.0);
            let ref_color = if ref_frac > 0.7 { Color32::from_rgb(255, 80, 80) }
                else if ref_frac > 0.4 { amber }
                else { Color32::from_rgb(100, 160, 255) };
            let ref_value = format!("({} W / {} W)", status.reflected_w, status.max_reflected_w);
            render_rf2k_meter(ui, "Reflected", &ref_value, ref_frac, ref_color,
                None, ref_divs, max_ref);

            // SWR bar — fixed scale with non-linear divisions
            let max_swr = status.max_swr_x100 as f32 / 100.0;
            let swr_frac = ((swr - 1.0) / 4.0).clamp(0.0, 1.0);
            let swr_color = if swr > 2.5 { Color32::from_rgb(255, 80, 80) }
                else if swr > 1.5 { amber }
                else { Color32::from_rgb(50, 180, 50) };
            let swr_value = format!("({:.2} / {:.2})", swr, max_swr);
            let swr_divs: &[(f32, &str)] = &[
                (1.0,"1"), (1.5,"1.5"), (2.0,"2"), (3.0,"3"), (5.0,"5"),
            ];
            render_rf2k_swr_meter(ui, "SWR", &swr_value, swr_frac, swr_color, swr_divs);
        });

    ui.add_space(4.0);
}

/// Meter bar with title, tick marks, divisions, and value text (v190 style)
fn render_rf2k_meter(
    ui: &mut egui::Ui,
    title: &str,
    value_text: &str,
    fraction: f32,
    fill_color: Color32,
    peak_frac: Option<f32>,
    divisions: &[(f32, &str)],
    max_val: f32,
) {
    let avail_w = ui.available_width();
    let margin = 10.0f32;
    let bar_w = avail_w - margin * 2.0;
    let bar_h = 16.0f32;
    let tick_h = 8.0f32;
    let total_h = 14.0 + bar_h + tick_h + 14.0 + 14.0; // title + bar + ticks + labels + value

    let (rect, _) = ui.allocate_exact_size(egui::vec2(avail_w, total_h), egui::Sense::hover());
    let painter = ui.painter();

    // Title centered above bar
    let title_y = rect.top() + 7.0;
    painter.text(
        egui::pos2(rect.center().x, title_y),
        egui::Align2::CENTER_CENTER,
        title,
        egui::FontId::proportional(12.0),
        Color32::WHITE,
    );

    // Bar
    let bar_top = rect.top() + 14.0;
    let bar_rect = egui::Rect::from_min_size(
        egui::pos2(rect.left() + margin, bar_top),
        egui::vec2(bar_w, bar_h),
    );
    painter.rect_filled(bar_rect, 2.0, Color32::from_rgb(30, 30, 30));
    painter.rect_stroke(bar_rect, 2.0, egui::Stroke::new(1.0, Color32::from_rgb(80, 80, 80)));

    // Bar fill
    if fraction > 0.001 {
        let fill_rect = egui::Rect::from_min_size(
            bar_rect.left_top(),
            egui::vec2(bar_w * fraction, bar_h),
        );
        painter.rect_filled(fill_rect, 2.0, fill_color);
    }

    // Peak hold marker
    if let Some(pf) = peak_frac {
        if pf > 0.01 {
            let peak_x = bar_rect.left() + bar_w * pf;
            painter.line_segment(
                [egui::pos2(peak_x, bar_rect.top()), egui::pos2(peak_x, bar_rect.bottom())],
                egui::Stroke::new(2.0, Color32::WHITE),
            );
        }
    }

    // Tick marks and division labels
    let tick_top = bar_rect.bottom();
    let label_y = tick_top + tick_h + 2.0;
    let tick_color = Color32::from_rgb(160, 160, 160);
    let label_color = Color32::from_rgb(140, 140, 140);

    for &(val, label) in divisions {
        let frac = val / max_val;
        let x = bar_rect.left() + bar_w * frac;
        // Tick mark
        painter.line_segment(
            [egui::pos2(x, tick_top), egui::pos2(x, tick_top + tick_h)],
            egui::Stroke::new(1.0, tick_color),
        );
        // Label
        painter.text(
            egui::pos2(x, label_y),
            egui::Align2::CENTER_TOP,
            label,
            egui::FontId::proportional(9.0),
            label_color,
        );
    }

    // Sub-ticks between divisions
    if divisions.len() >= 2 {
        for i in 0..(divisions.len() - 1) {
            let v0 = divisions[i].0;
            let v1 = divisions[i + 1].0;
            let mid = (v0 + v1) / 2.0;
            let frac = mid / max_val;
            let x = bar_rect.left() + bar_w * frac;
            painter.line_segment(
                [egui::pos2(x, tick_top), egui::pos2(x, tick_top + tick_h * 0.5)],
                egui::Stroke::new(1.0, Color32::from_rgb(100, 100, 100)),
            );
        }
    }

    // Value text centered below
    let value_y = rect.bottom() - 7.0;
    painter.text(
        egui::pos2(rect.center().x, value_y),
        egui::Align2::CENTER_CENTER,
        value_text,
        egui::FontId::proportional(11.0),
        Color32::from_rgb(200, 200, 200),
    );
}

/// SWR meter with non-linear scale (1, 1.5, 2, 3, 5, ∞)
fn render_rf2k_swr_meter(
    ui: &mut egui::Ui,
    title: &str,
    value_text: &str,
    fraction: f32,
    fill_color: Color32,
    divisions: &[(f32, &str)],
) {
    let amber = Color32::from_rgb(255, 170, 40);
    let avail_w = ui.available_width();
    let margin = 10.0f32;
    let bar_w = avail_w - margin * 2.0;
    let bar_h = 16.0f32;
    let tick_h = 8.0f32;
    let total_h = 14.0 + bar_h + tick_h + 14.0 + 14.0;

    let (rect, _) = ui.allocate_exact_size(egui::vec2(avail_w, total_h), egui::Sense::hover());
    let painter = ui.painter();

    // Title
    painter.text(
        egui::pos2(rect.center().x, rect.top() + 7.0),
        egui::Align2::CENTER_CENTER,
        title,
        egui::FontId::proportional(12.0),
        Color32::WHITE,
    );

    // Bar background
    let bar_top = rect.top() + 14.0;
    let bar_rect = egui::Rect::from_min_size(
        egui::pos2(rect.left() + margin, bar_top),
        egui::vec2(bar_w, bar_h),
    );
    painter.rect_filled(bar_rect, 2.0, Color32::from_rgb(30, 30, 30));
    painter.rect_stroke(bar_rect, 2.0, egui::Stroke::new(1.0, Color32::from_rgb(80, 80, 80)));

    // Multi-color background segments: green (1-1.5), yellow (1.5-2.5), red (2.5-5)
    let swr_to_frac = |swr: f32| -> f32 { ((swr - 1.0) / 4.0).clamp(0.0, 1.0) };
    let segments: &[(f32, f32, Color32)] = &[
        (1.0, 1.5, Color32::from_rgb(30, 80, 30)),    // green zone
        (1.5, 2.5, Color32::from_rgb(80, 70, 20)),    // amber zone
        (2.5, 5.0, Color32::from_rgb(80, 25, 25)),    // red zone
    ];
    for &(start, end, color) in segments {
        let x0 = bar_rect.left() + bar_w * swr_to_frac(start);
        let x1 = bar_rect.left() + bar_w * swr_to_frac(end);
        let seg = egui::Rect::from_min_max(
            egui::pos2(x0, bar_rect.top()),
            egui::pos2(x1, bar_rect.bottom()),
        );
        painter.rect_filled(seg, 0.0, color);
    }

    // Fill bar overlay (brighter color for actual SWR)
    if fraction > 0.001 {
        let fill_rect = egui::Rect::from_min_size(
            bar_rect.left_top(),
            egui::vec2(bar_w * fraction, bar_h),
        );
        painter.rect_filled(fill_rect, 2.0, fill_color);
    }

    // Tick marks and labels (non-linear: map SWR value to fraction)
    let tick_top = bar_rect.bottom();
    let label_y = tick_top + tick_h + 2.0;
    let tick_color = Color32::from_rgb(160, 160, 160);

    for &(swr_val, label) in divisions {
        let frac = swr_to_frac(swr_val);
        let x = bar_rect.left() + bar_w * frac;
        painter.line_segment(
            [egui::pos2(x, tick_top), egui::pos2(x, tick_top + tick_h)],
            egui::Stroke::new(1.0, tick_color),
        );
        painter.text(
            egui::pos2(x, label_y),
            egui::Align2::CENTER_TOP,
            label,
            egui::FontId::proportional(9.0),
            Color32::from_rgb(140, 140, 140),
        );
    }

    // ∞ label at far right
    painter.text(
        egui::pos2(bar_rect.right(), label_y),
        egui::Align2::CENTER_TOP,
        "\u{221E}",
        egui::FontId::proportional(9.0),
        Color32::from_rgb(140, 140, 140),
    );

    // Value text
    painter.text(
        egui::pos2(rect.center().x, rect.bottom() - 7.0),
        egui::Align2::CENTER_CENTER,
        value_text,
        egui::FontId::proportional(11.0),
        Color32::from_rgb(200, 200, 200),
    );

    let _ = amber; // suppress warning
}

// ── Section 4: Band & Frequency ────────────────────────────────────────────

fn render_rf2k_band_freq(
    ui: &mut egui::Ui,
    status: &rf2k::Rf2kStatus,
) {
    ui.horizontal(|ui| {
        ui.label("Band:");
        ui.label(RichText::new(rf2k::band_name(status.band)).strong());
        ui.add_space(16.0);
        ui.label("Freq:");
        ui.label(RichText::new(format!("{} kHz", status.frequency_khz)).strong().monospace());
    });
    ui.add_space(2.0);
}

// ── Section 5: Telemetry ───────────────────────────────────────────────────

fn render_rf2k_telemetry(
    ui: &mut egui::Ui,
    status: &rf2k::Rf2kStatus,
) {
    let amber = Color32::from_rgb(255, 170, 40);
    let temp = status.temperature_x10 as f32 / 10.0;
    let volt = status.voltage_x10 as f32 / 10.0;
    let cur = status.current_x10 as f32 / 10.0;

    ui.horizontal(|ui| {
        let temp_color = if temp > 60.0 { Color32::from_rgb(255, 80, 80) }
            else if temp > 50.0 { amber }
            else { ui.visuals().text_color() };
        ui.colored_label(temp_color, format!("Temp: {:.1}\u{00B0}C", temp));

        ui.label(format!("Volt: {:.1}V", volt));
        ui.label(format!("Cur: {:.1}A", cur));
    });
    ui.separator();
}

// ── Section 6: Antenna Selection ───────────────────────────────────────────

fn render_rf2k_antennas(
    ui: &mut egui::Ui,
    rf2k: &Rf2k,
    status: &rf2k::Rf2kStatus,
) {
    ui.horizontal(|ui| {
        for i in 1..=4u8 {
            let active = status.antenna_type == 0 && status.antenna_number == i;
            let btn_color = if active { Color32::from_rgb(50, 180, 50) } else { ui.visuals().widgets.inactive.bg_fill };
            let btn = egui::Button::new(format!("Ant {}", i)).fill(btn_color);
            if ui.add_enabled(status.connected, btn).clicked() {
                rf2k.send_command(rf2k::Rf2kCmd::SetAntenna { antenna_type: 0, number: i });
            }
        }
        ui.add_space(8.0);
        let ext_active = status.antenna_type == 1;
        let ext_color = if ext_active { Color32::from_rgb(50, 180, 50) } else { ui.visuals().widgets.inactive.bg_fill };
        let ext_btn = egui::Button::new("Ext").fill(ext_color);
        if ui.add_enabled(status.connected, ext_btn).clicked() {
            rf2k.send_command(rf2k::Rf2kCmd::SetAntenna { antenna_type: 1, number: 1 });
        }
    });
    ui.add_space(2.0);
}

// ── Section 7: Operating Controls ──────────────────────────────────────────

fn render_rf2k_operating_controls(
    ui: &mut egui::Ui,
    rf2k: &Rf2k,
    status: &rf2k::Rf2kStatus,
) {
    let amber = Color32::from_rgb(255, 170, 40);
    ui.horizontal(|ui| {
        // Operate/Standby
        let (op_text, op_color) = if status.operate {
            ("Operate", Color32::from_rgb(50, 180, 50))
        } else {
            ("Standby", amber)
        };
        let btn = egui::Button::new(RichText::new(op_text).strong().color(Color32::WHITE))
            .fill(op_color);
        if ui.add_enabled(status.connected, btn).clicked() {
            rf2k.send_command(rf2k::Rf2kCmd::SetOperate(!status.operate));
        }

        // Tune
        let tune_color = if status.tuner_mode == 3 || status.tuner_mode == 5 {
            Color32::from_rgb(200, 200, 50)
        } else {
            ui.visuals().widgets.inactive.bg_fill
        };
        let tune_btn = egui::Button::new("Tune").fill(tune_color);
        if ui.add_enabled(status.connected, tune_btn).clicked() {
            rf2k.send_command(rf2k::Rf2kCmd::Tune);
        }

        // Reset (error reset, always visible here)
        if ui.add_enabled(status.connected && status.error_state != 0,
            egui::Button::new("Reset")
        ).clicked() {
            rf2k.send_command(rf2k::Rf2kCmd::ErrorReset);
        }
    });
    let _ = amber; // suppress warning
    ui.add_space(2.0);
}

// ── Section 8: Tuner ───────────────────────────────────────────────────────

fn render_rf2k_tuner(
    ui: &mut egui::Ui,
    rf2k: &Rf2k,
    status: &rf2k::Rf2kStatus,
) {
    let tuner_edit_enabled = status.connected && !status.operate && status.forward_w < 30;
    let is_manual = status.tuner_mode == 2;

    ui.horizontal(|ui| {
        let mode_text = rf2k::tuner_mode_name(status.tuner_mode);
        let mode_color = match status.tuner_mode {
            3 | 5 => Color32::from_rgb(200, 200, 50),
            4 => Color32::from_rgb(50, 180, 50),
            2 => Color32::from_rgb(100, 160, 255),
            _ => ui.visuals().text_color(),
        };
        ui.label("Mode:");
        ui.colored_label(mode_color, RichText::new(mode_text).strong());

        if !status.tuner_setup.is_empty() {
            ui.label(format!("Setup: {}", status.tuner_setup));
        }

        // MAN/AUTO toggle — shows current state
        if status.tuner_mode == 2 || status.tuner_mode == 4 {
            let toggle_text = if is_manual { "Manual" } else { "Auto" };
            let toggle_btn = egui::Button::new(RichText::new(toggle_text).strong())
                .fill(Color32::from_rgb(100, 160, 230)).small();
            if ui.add_enabled(tuner_edit_enabled, toggle_btn).clicked() {
                let new_mode = if is_manual { 1 } else { 0 };
                rf2k.send_command(rf2k::Rf2kCmd::TunerMode(new_mode));
            }
        }

        // Bypass toggle
        let is_bypass = status.tuner_mode == 1 || status.tuner_setup == "BYPASS";
        let mut byp = is_bypass;
        if ui.add_enabled(tuner_edit_enabled, egui::Checkbox::new(&mut byp, "Bypass")).changed() {
            rf2k.send_command(rf2k::Rf2kCmd::TunerBypass(!is_bypass));
        }
    });

    // L/C values + manual controls
    ui.horizontal(|ui| {
        if status.tuner_l_nh > 0 || is_manual {
            ui.label(format!("L: {} nH", status.tuner_l_nh));
        }
        if status.tuner_c_pf > 0 || is_manual {
            ui.label(format!("C: {} pF", status.tuner_c_pf));
        }
    });

    if is_manual {
        ui.horizontal(|ui| {
            // K cycle
            if ui.add_enabled(tuner_edit_enabled, egui::Button::new("K").small()).clicked() {
                rf2k.send_command(rf2k::Rf2kCmd::TunerK);
            }
            ui.separator();
            // L adjust
            ui.label("L:");
            if ui.add_enabled(tuner_edit_enabled, egui::Button::new("\u{2212}").small()).clicked() {
                rf2k.send_command(rf2k::Rf2kCmd::TunerLDown);
            }
            if ui.add_enabled(tuner_edit_enabled, egui::Button::new("+").small()).clicked() {
                rf2k.send_command(rf2k::Rf2kCmd::TunerLUp);
            }
            ui.separator();
            // C adjust
            ui.label("C:");
            if ui.add_enabled(tuner_edit_enabled, egui::Button::new("\u{2212}").small()).clicked() {
                rf2k.send_command(rf2k::Rf2kCmd::TunerCDown);
            }
            if ui.add_enabled(tuner_edit_enabled, egui::Button::new("+").small()).clicked() {
                rf2k.send_command(rf2k::Rf2kCmd::TunerCUp);
            }
            ui.separator();
            if ui.add_enabled(tuner_edit_enabled, egui::Button::new("Reset").small()).clicked() {
                rf2k.send_command(rf2k::Rf2kCmd::TunerReset);
            }
            if ui.add_enabled(tuner_edit_enabled, egui::Button::new("Store").small()).clicked() {
                rf2k.send_command(rf2k::Rf2kCmd::TunerStore);
            }
        });
    }
    ui.separator();
}

// ── Section 9: Drive Control ───────────────────────────────────────────────

fn render_rf2k_drive_control(
    ui: &mut egui::Ui,
    rf2k: &Rf2k,
    status: &rf2k::Rf2kStatus,
    active_pa: &Arc<AtomicU8>,
) {
    let amber = Color32::from_rgb(255, 170, 40);
    let is_active = active_pa.load(Ordering::Relaxed) == 2;
    let drive_enabled = status.connected && status.operate && is_active;

    ui.horizontal(|ui| {
        ui.label("Drive:");
        if ui.add_enabled(drive_enabled, egui::Button::new("\u{2212}")).clicked() {
            rf2k.send_command(rf2k::Rf2kCmd::DriveDown);
        }
        let drive_text = if status.drive_w > 0 {
            format!("{}W", status.drive_w)
        } else {
            "--W".to_string()
        };
        ui.label(RichText::new(drive_text).strong().monospace());
        if ui.add_enabled(drive_enabled, egui::Button::new("+")).clicked() {
            rf2k.send_command(rf2k::Rf2kCmd::DriveUp);
        }

        ui.add_space(8.0);

        // Modulation indicator
        if !status.modulation.is_empty() {
            let mod_color = match status.modulation.as_str() {
                "SSB" => Color32::from_rgb(100, 160, 255),
                "AM" => amber,
                _ => Color32::from_rgb(50, 180, 50),
            };
            ui.colored_label(mod_color, RichText::new(&status.modulation).strong());
        }

        if status.max_power_w > 0 {
            ui.label(format!("Max: {}W", status.max_power_w));
        }
    });
    ui.add_space(2.0);
}

// ── Section 12: Footer ─────────────────────────────────────────────────────

pub(super) fn render_rf2k_footer(
    ui: &mut egui::Ui,
    status: &rf2k::Rf2kStatus,
) {
    ui.add_space(4.0);
    ui.separator();
    let ver = if status.controller_version > 0 {
        let hw = if status.hw_revision.is_empty() {
            String::new()
        } else {
            format!(" {}", status.hw_revision)
        };
        format!("Ver. G190C{}{}", status.controller_version, hw)
    } else {
        String::new()
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new(ver).small().color(Color32::from_rgb(140, 140, 140)));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new("\u{00A9}2025 by RF-KIT").small().color(Color32::from_rgb(140, 140, 140)));
        });
    });
}

pub(super) fn render_rf2k_debug_section(
    ui: &mut egui::Ui,
    rf2k: &Rf2k,
    status: &rf2k::Rf2kStatus,
    show_debug: &mut bool,
    confirm_high_power: &mut bool,
    confirm_zero_fram: &mut bool,
) {
    if !status.debug_available {
        return;
    }

    let amber = Color32::from_rgb(255, 170, 40);
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let header = if *show_debug { "Debug \u{25BC}" } else { "Debug \u{25B6}" };
        if ui.selectable_label(*show_debug, RichText::new(header).strong()).clicked() {
            *show_debug = !*show_debug;
        }
    });

    if !*show_debug {
        return;
    }

    ui.indent("rf2k_debug", |ui| {
        // System Info
        ui.label(RichText::new("System Info").strong());
        ui.horizontal(|ui| {
            ui.label(format!("FW: v{}", status.controller_version));
            if !status.hw_revision.is_empty() {
                ui.label(format!("HW: {}", status.hw_revision));
            }
            ui.label(format!("BIAS: {:.1}%", status.bias_pct_x10 as f32 / 10.0));
            let psu = match status.psu_source {
                0 => "Internal",
                1 => "External",
                2 => "CAN Ctrl",
                _ => "?",
            };
            ui.label(format!("PSU: {}", psu));
        });
        ui.horizontal(|ui| {
            let hours = status.uptime_s / 3600;
            let mins = (status.uptime_s % 3600) / 60;
            if hours >= 24 {
                ui.label(format!("Uptime: {}d {}h {}m", hours / 24, hours % 24, mins));
            } else {
                ui.label(format!("Uptime: {}h {}m", hours, mins));
            }
            let tx_h = status.tx_time_s / 3600;
            let tx_m = (status.tx_time_s % 3600) / 60;
            ui.label(format!("TX: {}h {:02}m", tx_h, tx_m));
            ui.label(format!("Errors: {}", status.error_count));
        });
        ui.horizontal(|ui| {
            ui.label(format!("Bank: {}", status.storage_bank));
            ui.label("FRQ Delay:");
            if ui.add_enabled(status.connected, egui::Button::new("\u{2212}").small()).clicked() {
                rf2k.send_command(rf2k::Rf2kCmd::FrqDelayDown);
            }
            ui.label(format!("{}", status.frq_delay));
            if ui.add_enabled(status.connected, egui::Button::new("+").small()).clicked() {
                rf2k.send_command(rf2k::Rf2kCmd::FrqDelayUp);
            }
        });

        ui.add_space(4.0);
        ui.label(RichText::new("Settings").strong());

        // PE5 High/Low toggle
        ui.horizontal(|ui| {
            ui.label("Power:");
            let (pe5_text, pe5_color) = if status.high_power {
                ("HIGH", Color32::from_rgb(255, 80, 80))
            } else {
                ("LOW", Color32::from_rgb(50, 180, 50))
            };
            let pe5_btn = egui::Button::new(RichText::new(pe5_text).strong().color(Color32::WHITE))
                .fill(pe5_color);
            if ui.add_enabled(status.connected, pe5_btn).clicked() {
                if status.high_power {
                    // Going LOW is always safe
                    rf2k.send_command(rf2k::Rf2kCmd::SetHighPower(false));
                } else {
                    // Going HIGH needs confirmation
                    *confirm_high_power = true;
                }
            }

            ui.separator();
            ui.label("Tuner 6m:");
            let t6m_text = if status.tuner_6m { "ON" } else { "OFF" };
            if ui.add_enabled(status.connected, egui::Button::new(t6m_text).small()).clicked() {
                rf2k.send_command(rf2k::Rf2kCmd::SetTuner6m(!status.tuner_6m));
            }
        });

        ui.horizontal(|ui| {
            ui.label("Band gap:");
            let bg_text = if status.band_gap_allowed { "ON" } else { "OFF" };
            if ui.add_enabled(status.connected, egui::Button::new(bg_text).small()).clicked() {
                rf2k.send_command(rf2k::Rf2kCmd::SetBandGap(!status.band_gap_allowed));
            }
            ui.separator();
            ui.label("AT thresh:");
            if ui.add_enabled(status.connected, egui::Button::new("\u{2212}").small()).clicked() {
                rf2k.send_command(rf2k::Rf2kCmd::AutotuneThresholdDown);
            }
            ui.label(format!("{:.1} dB", status.autotune_threshold_x10 as f32 / 10.0));
            if ui.add_enabled(status.connected, egui::Button::new("+").small()).clicked() {
                rf2k.send_command(rf2k::Rf2kCmd::AutotuneThresholdUp);
            }
        });

        ui.horizontal(|ui| {
            ui.label("DAC ALC:");
            if ui.add_enabled(status.connected, egui::Button::new("\u{2212}").small()).clicked() {
                rf2k.send_command(rf2k::Rf2kCmd::DacAlcDown);
            }
            ui.label(format!("{}", status.dac_alc));
            if ui.add_enabled(status.connected, egui::Button::new("+").small()).clicked() {
                rf2k.send_command(rf2k::Rf2kCmd::DacAlcUp);
            }
        });

        // Error History
        if !status.error_history.is_empty() {
            ui.add_space(4.0);
            ui.label(RichText::new("Error History").strong());
            egui::ScrollArea::vertical().max_height(100.0).id_salt("rf2k_err_hist").show(ui, |ui| {
                for (time, err) in status.error_history.iter().rev() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(time).monospace());
                        ui.colored_label(amber, err);
                    });
                }
            });
        }

        // Zero FRAM
        ui.add_space(4.0);
        ui.label(RichText::new("Dangerous").strong());
        let zero_btn = egui::Button::new(RichText::new("Zero FRAM").color(Color32::from_rgb(255, 100, 100)));
        if ui.add_enabled(status.connected, zero_btn).clicked() {
            *confirm_zero_fram = true;
        }
    });

    // Confirmation dialogs
    if *confirm_high_power {
        egui::Window::new("WARNING")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                ui.label("Setting HIGH power can damage equipment.");
                ui.label("Are you sure?");
                ui.horizontal(|ui| {
                    if ui.button("Yes, set HIGH").clicked() {
                        rf2k.send_command(rf2k::Rf2kCmd::SetHighPower(true));
                        *confirm_high_power = false;
                    }
                    if ui.button("Cancel").clicked() {
                        *confirm_high_power = false;
                    }
                });
            });
    }

    if *confirm_zero_fram {
        egui::Window::new("DESTRUCTIVE")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ui.ctx(), |ui| {
                ui.label("All tuner memories will be erased.");
                ui.label("This cannot be undone!");
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Yes, Zero FRAM").color(Color32::from_rgb(255, 80, 80))).clicked() {
                        rf2k.send_command(rf2k::Rf2kCmd::ZeroFRAM);
                        *confirm_zero_fram = false;
                    }
                    if ui.button("Cancel").clicked() {
                        *confirm_zero_fram = false;
                    }
                });
            });
    }
}

pub(super) fn render_rf2k_drive_config_section(
    ui: &mut egui::Ui,
    rf2k: &Rf2k,
    status: &rf2k::Rf2kStatus,
    show_drive_config: &mut bool,
    drive_edit: &mut [[u8; 11]; 3],
    drive_loaded: &mut bool,
) {
    if !status.debug_available {
        return;
    }

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let header = if *show_drive_config { "Drive Config \u{25BC}" } else { "Drive Config \u{25B6}" };
        if ui.selectable_label(*show_drive_config, RichText::new(header).strong()).clicked() {
            *show_drive_config = !*show_drive_config;
            if *show_drive_config && !*drive_loaded {
                // Load from status
                drive_edit[0] = status.drive_config_ssb;
                drive_edit[1] = status.drive_config_am;
                drive_edit[2] = status.drive_config_cont;
                *drive_loaded = true;
            }
        }
    });

    if !*show_drive_config {
        return;
    }

    // Load from status if not loaded yet
    if !*drive_loaded {
        drive_edit[0] = status.drive_config_ssb;
        drive_edit[1] = status.drive_config_am;
        drive_edit[2] = status.drive_config_cont;
        *drive_loaded = true;
    }

    ui.indent("rf2k_drive_cfg", |ui| {
        let bands = ["160m", "80m", "60m", "40m", "30m", "20m", "17m", "15m", "12m", "10m", "6m"];
        let categories = ["SSB", "AM", "CONT"];

        egui::Grid::new("rf2k_drive_grid")
            .striped(true)
            .min_col_width(40.0)
            .show(ui, |ui| {
                // Header
                ui.label(RichText::new("Band").strong());
                for cat in &categories {
                    ui.label(RichText::new(*cat).strong());
                }
                ui.end_row();

                for band_idx in 0..11 {
                    ui.label(bands[band_idx]);
                    for cat_idx in 0..3 {
                        let mut val = drive_edit[cat_idx][band_idx] as i32;
                        let drag = egui::DragValue::new(&mut val)
                            .range(0..=100)
                            .suffix("W")
                            .speed(0.5);
                        if ui.add(drag).changed() {
                            drive_edit[cat_idx][band_idx] = val.clamp(0, 100) as u8;
                        }
                    }
                    ui.end_row();
                }
            });

        ui.add_space(4.0);
        if ui.add_enabled(status.connected, egui::Button::new("Save to Pi")).clicked() {
            // Send each changed value
            for cat_idx in 0..3u8 {
                let current = match cat_idx {
                    0 => &status.drive_config_ssb,
                    1 => &status.drive_config_am,
                    _ => &status.drive_config_cont,
                };
                for band_idx in 0..11u8 {
                    let new_val = drive_edit[cat_idx as usize][band_idx as usize];
                    if new_val != current[band_idx as usize] {
                        rf2k.send_command(rf2k::Rf2kCmd::SetDriveConfig {
                            category: cat_idx,
                            band: band_idx,
                            value: new_val,
                        });
                    }
                }
            }
        }
    });
}
