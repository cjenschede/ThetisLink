// SPDX-License-Identifier: GPL-2.0-or-later

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use egui::{Color32, RichText};

use crate::spe_expert::{self, SpeExpert};

pub(super) fn render_spe_panel(
    ui: &mut egui::Ui,
    spe: &SpeExpert,
    status: &spe_expert::SpeStatus,
    log_entries: &[(String, String)],
    show_log: &mut bool,
    peak_power: &mut u16,
    peak_time: &mut std::time::Instant,
    drive_level: u8,
    active_pa: &Arc<AtomicU8>,
) {
    let amber = Color32::from_rgb(255, 170, 40);
    let is_active = active_pa.load(Ordering::Relaxed) == 1;

    // Header row: title + active checkbox + log checkbox + status indicator
    ui.horizontal(|ui| {
        ui.heading("SPE Expert 1.3K-FA");
        let mut active = is_active;
        if ui.checkbox(&mut active, "Active").changed() {
            active_pa.store(if active { 1 } else { 0 }, Ordering::Relaxed);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if status.connected {
                ui.colored_label(Color32::GREEN, "Online");
            } else {
                ui.colored_label(Color32::RED, "Offline");
            }
            ui.checkbox(show_log, "Log");
        });
    });
    ui.separator();

    // Warning / Alarm (prominent, above everything)
    if status.alarm != b'N' && status.alarm != 0 {
        ui.colored_label(Color32::from_rgb(255, 80, 80),
            RichText::new(format!("ALARM: {}", status.alarm as char)).strong());
    } else if status.warning != b'N' && status.warning != 0 {
        ui.colored_label(amber,
            RichText::new(format!("Warning: {}", status.warning as char)).strong());
    }

    ui.add_space(4.0);

    // Toggle buttons row — each button shows current state in its text
    ui.horizontal(|ui| {
        // Power — shows current state
        if !status.connected || status.state == 0 {
            let btn = egui::Button::new(RichText::new("Power Off").strong().color(Color32::WHITE))
                .fill(Color32::from_rgb(120, 120, 120));
            if ui.add(btn).clicked() {
                spe.send_command(spe_expert::SpeCmd::PowerOn);
            }
        } else {
            let btn = egui::Button::new(RichText::new("Power On").strong().color(Color32::WHITE))
                .fill(Color32::from_rgb(0, 150, 0));
            if ui.add(btn).clicked() {
                spe.send_command(spe_expert::SpeCmd::PowerOff);
            }
        }

        // Operate/Standby toggle — shows current state
        let (op_text, op_color) = match status.state {
            2 => ("Operate", Color32::from_rgb(50, 180, 50)),
            1 => ("Standby", amber),
            _ => ("Off", Color32::from_rgb(120, 120, 120)),
        };
        let btn = egui::Button::new(RichText::new(op_text).strong().color(Color32::WHITE))
            .fill(op_color);
        if ui.add_enabled(status.connected, btn).clicked() {
            spe.send_command(spe_expert::SpeCmd::ToggleOperate);
        }

        // Tune (ATU)
        if ui.add_enabled(status.connected && status.state == 2,
            egui::Button::new("Tune")).clicked() {
            spe.send_command(spe_expert::SpeCmd::Tune);
        }
    });

    ui.add_space(2.0);

    // Second row: Antenna, Input, Power level, Band
    ui.horizontal(|ui| {
        // Antenna toggle — shows "Ant1" or "Ant2" with blue "b" for bypass
        let mut ant_text = format!("Ant{}", status.antenna);
        let bypass_suffix = if status.atu_bypassed { "b" } else { "" };
        ant_text.push_str(bypass_suffix);
        let btn = if status.atu_bypassed {
            egui::Button::new(RichText::new(&ant_text).color(Color32::from_rgb(100, 160, 255)))
        } else {
            egui::Button::new(&ant_text)
        };
        if ui.add_enabled(status.connected, btn).clicked() {
            spe.send_command(spe_expert::SpeCmd::CycleAntenna);
        }

        // Input toggle — shows "In 1" or "In 2"
        let input_text = format!("In {}", status.input);
        if ui.add_enabled(status.connected, egui::Button::new(&input_text)).clicked() {
            spe.send_command(spe_expert::SpeCmd::CycleInput);
        }

        // Power level toggle — shows "Low", "Mid", "High"
        let pwr_text = match status.power_level {
            0 => "Low",
            1 => "Mid",
            2 => "High",
            _ => "?",
        };
        if ui.add_enabled(status.connected, egui::Button::new(pwr_text)).clicked() {
            spe.send_command(spe_expert::SpeCmd::CyclePower);
        }

        ui.separator();

        // Drive level +/- (LEFT/RIGHT arrow on SPE, adjusts Thetis drive during TX)
        let drive_enabled = status.connected && status.state == 2 && is_active;
        if ui.add_enabled(drive_enabled, egui::Button::new("Drive -")).clicked() {
            spe.send_command(spe_expert::SpeCmd::DriveDown);
        }
        ui.label(format!("{}%", drive_level));
        if ui.add_enabled(drive_enabled, egui::Button::new("Drive +")).clicked() {
            spe.send_command(spe_expert::SpeCmd::DriveUp);
        }
    });

    ui.add_space(4.0);

    // Peak hold: update peak, decay after 1 second
    let now = std::time::Instant::now();
    if status.forward_power > *peak_power {
        *peak_power = status.forward_power;
        *peak_time = now;
    } else if now.duration_since(*peak_time).as_millis() > 1000 {
        *peak_power = status.forward_power;
        *peak_time = now;
    }

    // Auto-scale based on power level: L=500W, M=1000W, H=1500W
    let (max_w, divisions): (f32, &[(f32, &str)]) = match status.power_level {
        0 => (500.0, &[(0.0, "0"), (100.0, "100"), (200.0, "200"), (300.0, "300"), (400.0, "400"), (500.0, "500")]),
        1 => (1000.0, &[(0.0, "0"), (200.0, "200"), (400.0, "400"), (600.0, "600"), (800.0, "800"), (1000.0, "1k")]),
        _ => (1500.0, &[(0.0, "0"), (300.0, "300"), (600.0, "600"), (900.0, "900"), (1200.0, "1.2k"), (1500.0, "1.5k")]),
    };

    // Power bar: wide with divisions and peak hold
    let bar_w = 300.0f32;
    let bar_h = 18.0f32;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(bar_w, bar_h + 14.0), egui::Sense::hover());
    let bar_rect = egui::Rect::from_min_size(rect.left_top(), egui::vec2(bar_w, bar_h));

    // Background
    ui.painter().rect_filled(bar_rect, 2.0, Color32::from_rgb(50, 50, 50));

    // Fill bar (realtime)
    let frac = (status.forward_power as f32 / max_w).clamp(0.0, 1.0);
    let fill_rect = egui::Rect::from_min_size(bar_rect.left_top(), egui::vec2(bar_w * frac, bar_h));
    let bar_color = if frac > 0.9 { Color32::from_rgb(255, 80, 80) }
        else if frac > 0.7 { amber }
        else { Color32::from_rgb(50, 180, 50) };
    ui.painter().rect_filled(fill_rect, 2.0, bar_color);

    // Peak hold marker (thin white line)
    let peak_frac = (*peak_power as f32 / max_w).clamp(0.0, 1.0);
    if peak_frac > 0.01 {
        let peak_x = bar_rect.left() + bar_w * peak_frac;
        ui.painter().line_segment(
            [egui::pos2(peak_x, bar_rect.top()), egui::pos2(peak_x, bar_rect.bottom())],
            egui::Stroke::new(2.0, Color32::WHITE),
        );
    }

    // Watt text inside bar
    if status.forward_power > 0 {
        ui.painter().text(
            bar_rect.center(),
            egui::Align2::CENTER_CENTER,
            format!("{}W", status.forward_power),
            egui::FontId::proportional(12.0),
            Color32::WHITE,
        );
    }

    // Division tick marks + labels below bar
    let label_y = bar_rect.bottom() + 1.0;
    for &(watts, label) in divisions {
        let x = bar_rect.left() + bar_w * (watts / max_w);
        // Tick
        ui.painter().line_segment(
            [egui::pos2(x, bar_rect.bottom()), egui::pos2(x, bar_rect.bottom() + 3.0)],
            egui::Stroke::new(1.0, Color32::from_rgb(140, 140, 140)),
        );
        // Label
        ui.painter().text(
            egui::pos2(x, label_y + 3.0),
            egui::Align2::CENTER_TOP,
            label,
            egui::FontId::proportional(9.0),
            Color32::from_rgb(160, 160, 160),
        );
    }

    // Band, SWR, Temp, Voltage, Current below bar
    ui.horizontal(|ui| {
        ui.label(RichText::new(spe_expert::band_name(status.band)).strong());
        let swr = status.swr_x10 as f32 / 10.0;
        let swr_color = if swr > 3.0 { Color32::from_rgb(255, 80, 80) }
            else if swr > 2.0 { amber }
            else { ui.visuals().text_color() };
        ui.colored_label(swr_color, format!("SWR {:.1}", swr));
        ui.label(format!("{}°C", status.temp));
        ui.label(format!("{:.1}V", status.voltage_x10 as f32 / 10.0));
        ui.label(format!("{:.1}A", status.current_x10 as f32 / 10.0));
    });

    // Log (collapsible, toggled via header checkbox)
    if *show_log {
        ui.add_space(4.0);
        ui.separator();
        egui::ScrollArea::vertical()
            .id_salt("spe_log")
            .stick_to_bottom(true)
            .max_height(100.0)
            .show(ui, |ui| {
                for (time, msg) in log_entries.iter().rev() {
                    ui.label(
                        RichText::new(format!("{}  {}", time, msg))
                            .monospace()
                            .size(10.0)
                            .color(Color32::from_rgb(180, 180, 180)),
                    );
                }
            });
    }
}
