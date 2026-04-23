// SPDX-License-Identifier: GPL-2.0-or-later

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use egui::{Color32, RichText};

use crate::ultrabeam::{self, UltraBeam};

/// Determine which VFO the UltraBeam should track based on Amplitec switch position.
/// Returns the VFO frequency in Hz. If Amplitec switch_a points to the UltraBeam port, use VFO A.
/// If switch_b points to UltraBeam, use VFO B. Otherwise default to VFO A.
fn ub_track_vfo_hz(
    vfo_a: &Arc<AtomicU64>,
    vfo_b: &Arc<AtomicU64>,
    amp_status: &Option<crate::amplitec::AmplitecStatus>,
    amp_labels: &[String; 6],
) -> (u64, &'static str) {
    // Find which Amplitec port has the UltraBeam
    let ub_port: Option<u8> = amp_labels.iter().position(|l| {
        let lower = l.to_lowercase();
        lower.contains("ultrabeam") || lower.contains("ultra beam") || lower.contains("ub")
    }).map(|i| (i + 1) as u8);

    if let (Some(ub_pos), Some(ref amp)) = (ub_port, amp_status) {
        if amp.connected {
            if amp.switch_b == ub_pos {
                return (vfo_b.load(Ordering::Relaxed), "VFO B");
            }
            if amp.switch_a == ub_pos {
                return (vfo_a.load(Ordering::Relaxed), "VFO A");
            }
        }
    }
    // Default: VFO A
    (vfo_a.load(Ordering::Relaxed), "VFO A")
}

pub(super) fn render_ultrabeam_panel(
    ui: &mut egui::Ui,
    ub: &UltraBeam,
    status: &ultrabeam::UltraBeamStatus,
    show_menu: &mut bool,
    confirm_retract: &mut bool,
    _confirm_calibrate: &mut bool,
    auto_track: &mut bool,
    last_auto_khz: &mut u16,
    vfo_freq_shared: &Arc<AtomicU64>,
    vfo_b_freq_shared: &Arc<AtomicU64>,
    amp_status: &Option<crate::amplitec::AmplitecStatus>,
    amp_labels: &[String; 6],
) {
    // Header
    ui.horizontal(|ui| {
        ui.heading("UltraBeam 2el 6-40");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if status.connected {
                ui.colored_label(Color32::from_rgb(0, 200, 0), RichText::new("\u{25CF} Online").strong());
            } else {
                ui.colored_label(Color32::from_rgb(200, 0, 0), RichText::new("\u{25CF} Offline").strong());
            }
        });
    });
    ui.separator();

    if !status.connected {
        ui.label("Geen verbinding met RCU-06");
        return;
    }

    // Large frequency display
    ui.add_space(8.0);
    ui.vertical_centered(|ui| {
        let freq_text = if status.frequency_khz > 0 {
            format!("{} kHz", status.frequency_khz)
        } else {
            "--- kHz".to_string()
        };
        ui.label(
            RichText::new(&freq_text)
                .monospace()
                .size(28.0)
                .strong()
                .color(Color32::from_rgb(0, 220, 255)),
        );
    });
    ui.add_space(4.0);

    // Band + FW version
    ui.horizontal(|ui| {
        ui.label(format!("Band: {}", ultrabeam::band_name(status.band)));
        ui.separator();
        ui.label(format!("FW: {}.{:02}", status.fw_major, status.fw_minor));
        if status.off_state {
            ui.separator();
            ui.colored_label(Color32::from_rgb(255, 170, 40), "Retracted");
        }
    });
    ui.add_space(6.0);

    // Direction buttons
    ui.horizontal(|ui| {
        ui.label("Direction:");
        let dir_names = [("Normal", 0u8), ("180\u{00B0}", 1), ("Bi-Dir", 2)];
        for (label, dir_val) in &dir_names {
            let is_active = status.direction == *dir_val;
            let btn = if is_active {
                egui::Button::new(RichText::new(*label).strong().color(Color32::BLACK))
                    .fill(Color32::from_rgb(80, 200, 80))
            } else {
                egui::Button::new(*label)
            };
            if ui.add(btn).clicked() && !is_active {
                ub.send_command(ultrabeam::UltraBeamCmd::SetFrequency {
                    khz: status.frequency_khz,
                    direction: *dir_val,
                });
            }
        }
    });
    ui.add_space(6.0);

    // Frequency step buttons
    ui.horizontal(|ui| {
        ui.label("Freq:");
        let steps: &[i16] = &[-100, -50, -25, 25, 50, 100];
        for &step in steps {
            let label = if step > 0 {
                format!("+{}", step)
            } else {
                format!("{}", step)
            };
            if ui.button(&label).clicked() {
                let new_freq = (status.frequency_khz as i32 + step as i32).max(0) as u16;
                if new_freq > 0 {
                    ub.send_command(ultrabeam::UltraBeamCmd::SetFrequency {
                        khz: new_freq,
                        direction: status.direction,
                    });
                }
            }
        }
        ui.separator();
        let (track_hz, track_label) = ub_track_vfo_hz(vfo_freq_shared, vfo_b_freq_shared, amp_status, amp_labels);
        let track_khz = (track_hz / 1000) as u16;
        let can_sync = status.connected && track_khz >= 1800 && track_khz <= 54000
            && track_khz != status.frequency_khz;
        if ui.add_enabled(can_sync, egui::Button::new(format!("Sync {}", track_label))).on_hover_text(
            format!("Stel UltraBeam in op {}: {} kHz", track_label, track_khz)
        ).clicked() {
            ub.send_command(ultrabeam::UltraBeamCmd::SetFrequency {
                khz: track_khz,
                direction: status.direction,
            });
        }
        ui.checkbox(auto_track, "Auto")
            .on_hover_text(format!("Auto-track {} frequency", track_label));
    });

    // Auto-track: send SetFrequency when VFO changes by >= 25 kHz
    if *auto_track && status.connected {
        let (track_hz, _) = ub_track_vfo_hz(vfo_freq_shared, vfo_b_freq_shared, amp_status, amp_labels);
        let track_khz = (track_hz / 1000) as u16;
        let diff = (track_khz as i32 - *last_auto_khz as i32).unsigned_abs();
        if track_khz >= 1800 && track_khz <= 54000 && diff >= 25 {
            *last_auto_khz = track_khz;
            ub.send_command(ultrabeam::UltraBeamCmd::SetFrequency {
                khz: track_khz,
                direction: status.direction,
            });
        }
    }

    ui.add_space(6.0);

    // Motor progress bar (only when moving)
    if status.motors_moving != 0 {
        ui.horizontal(|ui| {
            ui.label("Moving:");
            let progress = if status.motor_completion > 0 {
                (status.motor_completion as f32) / 60.0
            } else {
                0.0
            };
            let bar = egui::ProgressBar::new(progress)
                .text(format!("{}/60", status.motor_completion))
                .animate(true);
            ui.add(bar);
        });
        ui.add_space(4.0);
    }

    // Band preset buttons
    ui.label(RichText::new("Band presets:").strong());
    ui.horizontal_wrapped(|ui| {
        for &(_band_code, name, center_khz) in &ultrabeam::BAND_PRESETS {
            if ui.button(name).clicked() {
                ub.send_command(ultrabeam::UltraBeamCmd::SetFrequency {
                    khz: center_khz,
                    direction: status.direction,
                });
            }
        }
    });
    ui.add_space(6.0);

    // Retract + Menu buttons
    ui.horizontal(|ui| {
        if *confirm_retract {
            ui.colored_label(Color32::from_rgb(255, 80, 80), "Retract elementen?");
            if ui.button("Ja").clicked() {
                ub.send_command(ultrabeam::UltraBeamCmd::Retract);
                *confirm_retract = false;
            }
            if ui.button("Nee").clicked() {
                *confirm_retract = false;
            }
        } else {
            if ui.button("Retract").clicked() {
                *confirm_retract = true;
            }
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let menu_label = if *show_menu { "Menu \u{25BC}" } else { "Menu \u{25B6}" };
            if ui.button(menu_label).clicked() {
                *show_menu = !*show_menu;
                if *show_menu {
                    // Request element lengths when opening menu
                    ub.send_command(ultrabeam::UltraBeamCmd::ReadElements);
                }
            }
        });
    });

    // Collapsible menu section
    if *show_menu {
        ui.add_space(8.0);
        ui.separator();
        ui.label(RichText::new("Menu").strong().size(16.0));

        // Elements display
        ui.add_space(4.0);
        ui.label(RichText::new("Element Lengths").strong());
        ui.indent("ub_elements", |ui| {
            for i in 0..6 {
                let len = status.elements_mm[i];
                if len > 0 {
                    ui.horizontal(|ui| {
                        ui.label(format!("E{}: {} mm", i + 1, len));
                        if ui.small_button("-").clicked() {
                            if len > 10 {
                                ub.send_command(ultrabeam::UltraBeamCmd::ModifyElement {
                                    index: i as u8,
                                    length_mm: len - 10,
                                });
                                // Refresh after modify
                                ub.send_command(ultrabeam::UltraBeamCmd::ReadElements);
                            }
                        }
                        if ui.small_button("+").clicked() {
                            ub.send_command(ultrabeam::UltraBeamCmd::ModifyElement {
                                index: i as u8,
                                length_mm: len + 10,
                            });
                            ub.send_command(ultrabeam::UltraBeamCmd::ReadElements);
                        }
                    });
                } else {
                    ui.label(format!("E{}: --", i + 1));
                }
            }
            if ui.button("Refresh").clicked() {
                ub.send_command(ultrabeam::UltraBeamCmd::ReadElements);
            }
        });

        // Controller info
        ui.add_space(8.0);
        ui.label(RichText::new("Controller Info").strong());
        ui.indent("ub_info", |ui| {
            ui.label("Model: 2 elements 6-40");
            ui.label(format!("FW: v{}.{:02}", status.fw_major, status.fw_minor));
            ui.label(format!("Freq range: {} - {} MHz", status.freq_min_mhz, status.freq_max_mhz));
            ui.label(format!("Operation mode: {}", match status.operation {
                0 => "Normal",
                2 => "User Adjust",
                3 => "Setup",
                _ => "Unknown",
            }));
        });
    }
}
