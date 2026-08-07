// SPDX-License-Identifier: GPL-2.0-or-later

use super::*;

/// Visual state for `antenna_button`.
#[derive(Clone, Copy)]
enum AntennaState {
    Active,
    Blocked,
    Inactive,
}

/// Two-line antenna button - visually identical to the server version
/// (`sdr-remote-server::ui::amplitec::antenna_button`) so client and
/// server share the same look. No rename context menu here:
/// labels are managed only on the server, clients show the current
/// CSV broadcast.
fn antenna_button(
    ui: &mut egui::Ui,
    enabled: bool,
    pos: u8,
    alias: &str,
    state: AntennaState,
    max_width: f32,
) -> egui::Response {
    use egui::{vec2, Align2, FontId, Sense, Stroke};

    let pos_text = format!("Ant{}", pos);
    let alias_text = alias.trim();

    let style = ui.style().clone();
    let pos_font: FontId = egui::TextStyle::Small.resolve(&style);
    let alias_font: FontId = egui::TextStyle::Button.resolve(&style);

    let pos_galley = ui.painter().layout_no_wrap(
        pos_text.clone(),
        pos_font.clone(),
        Color32::TEMPORARY_COLOR,
    );
    let alias_galley = ui.painter().layout_no_wrap(
        alias_text.to_string(),
        alias_font.clone(),
        Color32::TEMPORARY_COLOR,
    );

    let pad_x = 10.0_f32;
    let pad_y = 4.0_f32;
    let gap = 1.0_f32;
    let natural_w = pos_galley.size().x.max(alias_galley.size().x) + pad_x * 2.0;
    let width = natural_w.min(max_width).max(24.0);
    let height = pos_galley.size().y + alias_galley.size().y + pad_y * 2.0 + gap;

    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), sense);

    let visuals = ui.visuals();
    let (mut fill, stroke_color) = match state {
        AntennaState::Active => (Color32::from_rgb(100, 160, 230), visuals.widgets.active.fg_stroke.color),
        AntennaState::Blocked => (
            Color32::from_rgb(180, 180, 180),
            visuals.widgets.inactive.fg_stroke.color,
        ),
        AntennaState::Inactive => (
            visuals.widgets.inactive.bg_fill,
            visuals.widgets.inactive.fg_stroke.color,
        ),
    };
    if enabled && response.hovered() {
        fill = fill.linear_multiply(1.15);
    }

    let painter = ui.painter();
    painter.rect_filled(rect, 4.0, fill);
    painter.rect_stroke(rect, 4.0, Stroke::new(1.0, stroke_color));

    let (pos_color, alias_color) = match state {
        AntennaState::Active => (Color32::WHITE, Color32::from_rgb(220, 230, 245)),
        AntennaState::Blocked => (Color32::from_rgb(120, 120, 120), Color32::from_rgb(160, 160, 160)),
        AntennaState::Inactive => (Color32::from_rgb(20, 20, 30), Color32::from_rgb(90, 90, 100)),
    };

    let center_x = rect.center().x;
    let top_y = rect.top() + pad_y + pos_galley.size().y * 0.5;
    let bottom_y = rect.bottom() - pad_y - alias_galley.size().y * 0.5;
    painter.text(
        egui::pos2(center_x, top_y),
        Align2::CENTER_CENTER,
        &pos_text,
        pos_font,
        pos_color,
    );
    if !alias_text.is_empty() {
        painter.text(
            egui::pos2(center_x, bottom_y),
            Align2::CENTER_CENTER,
            alias_text,
            alias_font,
            alias_color,
        );
    }

    response
}

impl SdrRemoteApp {
    pub(super) fn render_devices_screen(&mut self, ui: &mut egui::Ui) {
        let amber = Color32::from_rgb(255, 170, 40);
        let show_amplitec = self.amplitec_available;
        let show_tuner = self.tuner_available;
        let show_yaesu = self.yaesu_connected || self.yaesu2_connected || self.yaesu_enabled || self.yaesu2_enabled;

        let mut tabs: Vec<(u8, &str)> = Vec::new();
        if show_amplitec { tabs.push((0, "Amplitec")); }
        if show_tuner { tabs.push((1, "JC-4s")); }
        if self.spe_active { tabs.push((2, "SPE Expert")); }
        if self.rf2k_active { tabs.push((3, "RF2K-S")); }
        if self.ub_available { tabs.push((4, "UltraBeam")); }
        if self.rotor_available { tabs.push((5, "Rotor")); }
        if show_yaesu { tabs.push((6, "Yaesu")); }

        if tabs.is_empty() {
            ui.colored_label(Color32::GRAY, rust_i18n::t!("dev_no_external_devices").to_string());
            return;
        }

        if !tabs.iter().any(|(id, _)| *id == self.device_tab) {
            self.device_tab = tabs[0].0;
        }

        ui.horizontal(|ui| {
            for (id, label) in &tabs {
                ui.selectable_value(&mut self.device_tab, *id, *label);
            }
        });
        ui.separator();

        match self.device_tab {
            0 if show_amplitec => self.render_device_amplitec(ui),
            1 if show_tuner => self.render_device_tuner(ui, amber),
            2 if self.spe_active => self.render_device_spe(ui, amber),
            3 if self.rf2k_active => self.render_device_rf2k(ui, amber),
            4 if self.ub_available => self.render_device_ultrabeam(ui, amber),
            5 if self.rotor_available => self.render_device_rotor(ui),
            6 if show_yaesu => self.render_device_yaesu(ui, amber),
            _ => {}
        }
    }

    pub(super) fn render_device_amplitec(&mut self, ui: &mut egui::Ui) {
        // Header
        ui.horizontal(|ui| {
            ui.heading(rust_i18n::t!("dev_amplitec_title").to_string());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.amplitec_connected {
                    ui.colored_label(Color32::GREEN, rust_i18n::t!("dev_online").to_string());
                } else {
                    ui.colored_label(Color32::RED, rust_i18n::t!("dev_offline").to_string());
                }
            });
        });
        ui.separator();

        // Port A - TX+RX
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(rust_i18n::t!("dev_port_a_txrx").to_string()).strong());
            if self.amplitec_switch_a > 0 {
                let label = self.amplitec_label_a(self.amplitec_switch_a);
                ui.label(rust_i18n::t!("dev_current_colon_val", label = label).to_string());
            }
        });
        ui.horizontal(|ui| {
            let available = ui.available_width();
            let spacing = ui.spacing().item_spacing.x;
            let max_btn_w = ((available - 5.0 * spacing) / 6.0).max(24.0);
            for pos in 1..=6u8 {
                let is_active = self.amplitec_switch_a == pos;
                let is_blocked = self.amplitec_switch_b == pos;
                let label = self.amplitec_label_a(pos);
                let state = if is_active {
                    AntennaState::Active
                } else if is_blocked {
                    AntennaState::Blocked
                } else {
                    AntennaState::Inactive
                };
                let resp = antenna_button(ui, self.amplitec_connected, pos, &label, state, max_btn_w);
                if resp.clicked() {
                    let _ = self.cmd_tx.send(Command::SetAmplitecSwitchA(pos));
                }
                if is_blocked {
                    resp.on_hover_text(rust_i18n::t!("dev_ant_occupied_by_b", pos = pos, label = label).to_string());
                }
            }
        });

        ui.add_space(8.0);

        // Port B - RX
        ui.horizontal(|ui| {
            ui.label(RichText::new(rust_i18n::t!("dev_port_b_rx").to_string()).strong());
            if self.amplitec_switch_b > 0 {
                let label = self.amplitec_label_b(self.amplitec_switch_b);
                ui.label(rust_i18n::t!("dev_current_colon_val", label = label).to_string());
            }
        });
        ui.horizontal(|ui| {
            let available = ui.available_width();
            let spacing = ui.spacing().item_spacing.x;
            let max_btn_w = ((available - 5.0 * spacing) / 6.0).max(24.0);
            for pos in 1..=6u8 {
                let is_active = self.amplitec_switch_b == pos;
                let is_blocked = self.amplitec_switch_a == pos;
                let label = self.amplitec_label_b(pos);
                let state = if is_active {
                    AntennaState::Active
                } else if is_blocked {
                    AntennaState::Blocked
                } else {
                    AntennaState::Inactive
                };
                let resp = antenna_button(ui, self.amplitec_connected, pos, &label, state, max_btn_w);
                if resp.clicked() {
                    let _ = self.cmd_tx.send(Command::SetAmplitecSwitchB(pos));
                }
                if is_blocked {
                    resp.on_hover_text(rust_i18n::t!("dev_ant_occupied_by_a", pos = pos, label = label).to_string());
                }
            }
        });

        ui.add_space(8.0);
        ui.separator();

        // Power-cap table (collapsing section). Server pushes the current
        // table via AmplitecPowerTablePacket; edit state is tracked locally
        // and only sent to the server on "Save to server".
        if super::helpers::chevron_label(
            ui,
            self.amplitec_power_show,
            RichText::new(rust_i18n::t!("dev_power_cap_table").to_string()).strong(),
        )
        .clicked()
        {
            self.amplitec_power_show = !self.amplitec_power_show;
            self.save_full_config();
        }
        if self.amplitec_power_show {
            ui.indent("amplitec_power_table", |ui| {
                if !self.amplitec_power_loaded {
                    ui.colored_label(
                        Color32::from_rgb(180, 180, 180),
                        rust_i18n::t!("dev_waiting_for_server").to_string(),
                    );
                } else {
                    // Read-only view: the server owns this config (at the
                    // Amplitec hardware) and pushes it via AmplitecPowerTablePacket.
                    // Editing happens in the server's Amplitec window.
                    egui::Grid::new("amplitec_power_grid")
                        .striped(true)
                        .min_col_width(40.0)
                        .show(ui, |ui| {
                            ui.label(RichText::new("Pos").strong());
                            ui.label(RichText::new(rust_i18n::t!("dev_label").to_string()).strong());
                            ui.label(RichText::new(rust_i18n::t!("dev_max_w").to_string()).strong());
                            ui.label(RichText::new(rust_i18n::t!("dev_rx_only").to_string()).strong());
                            ui.end_row();
                            for i in 0..6 {
                                let pos = (i as u8) + 1;
                                ui.label(format!("A-{}", pos));
                                ui.label(self.amplitec_label_a(pos));
                                let w = self.amplitec_power_max_w[i];
                                ui.label(if w == 0 { "-".to_string() } else { format!("{} W", w) });
                                ui.label(if self.amplitec_power_tx_blocked[i] { "X" } else { "-" });
                                ui.end_row();
                            }
                        });
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(rust_i18n::t!("dev_edit_on_server").to_string())
                            .size(10.0)
                            .color(Color32::from_rgb(160, 160, 160)),
                    );
                }
            });
            ui.add_space(8.0);
            ui.separator();
        }

        // Log
        ui.label(RichText::new(rust_i18n::t!("dev_log").to_string()).strong());
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .max_height(200.0)
            .show(ui, |ui| {
                for (time, msg) in self.amplitec_log.iter().rev() {
                    ui.label(
                        RichText::new(format!("{}  {}", time, msg))
                            .monospace()
                            .size(10.0)
                            .color(Color32::from_rgb(180, 180, 180)),
                    );
                }
            });
    }

    pub(super) fn render_device_tuner(&mut self, ui: &mut egui::Ui, amber: Color32) {
        ui.horizontal(|ui| {
            ui.heading(rust_i18n::t!("dev_jc4s_title").to_string());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.tuner_connected {
                    ui.colored_label(Color32::GREEN, rust_i18n::t!("dev_online").to_string());
                } else {
                    ui.colored_label(Color32::RED, rust_i18n::t!("dev_offline").to_string());
                }
            });
        });
        ui.separator();
        ui.add_space(4.0);

        // Status
        let olive_green = Color32::from_rgb(120, 160, 40);
        let state_text: String = match self.tuner_state {
            1 => rust_i18n::t!("dev_tuning").to_string(),
            2 => rust_i18n::t!("dev_tune_ok").to_string(),
            3 => rust_i18n::t!("dev_timeout").to_string(),
            4 => rust_i18n::t!("dev_aborted").to_string(),
            5 => rust_i18n::t!("dev_done_already_tuned").to_string(),
            _ => rust_i18n::t!("dev_idle").to_string(),
        };
        let state_color = match self.tuner_state {
            1 => Color32::from_rgb(60, 120, 220),
            2 => Color32::from_rgb(50, 180, 50),
            3 | 4 => amber,
            5 => olive_green,
            _ => Color32::GRAY,
        };
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("dev_status_colon").to_string());
            ui.colored_label(state_color, RichText::new(state_text).strong().size(16.0));
        });

        ui.add_space(8.0);

        // Tune button
        let can_start = self.tuner_connected && self.tuner_can_tune
            && (self.tuner_state == 0 || self.tuner_state == 2 || self.tuner_state == 5);
        let (tune_color, tune_text): (Color32, String) = match self.tuner_state {
            1 => (Color32::from_rgb(60, 120, 220), rust_i18n::t!("dev_tuning").to_string()),
            2 => (Color32::from_rgb(50, 180, 50), rust_i18n::t!("dev_tune_ok").to_string()),
            3 => (amber, rust_i18n::t!("dev_tune_x").to_string()),
            4 => (amber, rust_i18n::t!("dev_tune_x").to_string()),
            5 => (olive_green, rust_i18n::t!("dev_tune_tilde").to_string()),
            _ => (Color32::from_rgb(80, 80, 80), rust_i18n::t!("dev_tune").to_string()),
        };
        ui.horizontal(|ui| {
            let btn = egui::Button::new(RichText::new(tune_text).color(Color32::WHITE).strong().size(16.0))
                .fill(tune_color)
                .min_size(Vec2::new(120.0, 32.0));
            if ui.add_enabled(can_start, btn).clicked() {
                let _ = self.cmd_tx.send(Command::TunerTune);
            }

            let abort_enabled = self.tuner_state == 1;
            let abort_btn = egui::Button::new(RichText::new(rust_i18n::t!("dev_abort").to_string()).size(14.0))
                .min_size(Vec2::new(60.0, 32.0));
            if ui.add_enabled(abort_enabled, abort_btn).clicked() {
                let _ = self.cmd_tx.send(Command::TunerAbort);
            }
        });

        if !self.tuner_can_tune {
            ui.add_space(4.0);
            ui.colored_label(amber, rust_i18n::t!("dev_tuner_not_available").to_string());
        }
    }

    pub(super) fn render_device_spe(&mut self, ui: &mut egui::Ui, amber: Color32) {
        // Header: title + active badge + Online/Offline
        ui.horizontal(|ui| {
            ui.heading("SPE Expert 1.3K-FA");
            if self.spe_active {
                ui.colored_label(Color32::from_rgb(50, 180, 50), RichText::new(rust_i18n::t!("dev_active_upper").to_string()).strong());
            } else {
                ui.colored_label(Color32::from_rgb(140, 140, 140), rust_i18n::t!("dev_inactive_upper").to_string());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.spe_connected {
                    ui.colored_label(Color32::GREEN, rust_i18n::t!("dev_online").to_string());
                } else {
                    ui.colored_label(Color32::RED, rust_i18n::t!("dev_offline").to_string());
                }
            });
        });
        ui.separator();

        // Warning / Alarm (prominent, above everything)
        if self.spe_alarm != b'N' && self.spe_alarm != 0 {
            let code = (self.spe_alarm as char).to_string();
            ui.colored_label(Color32::from_rgb(255, 80, 80),
                RichText::new(rust_i18n::t!("dev_alarm_fmt", code = code).to_string()).strong());
        } else if self.spe_warning != b'N' && self.spe_warning != 0 {
            let code = (self.spe_warning as char).to_string();
            ui.colored_label(amber,
                RichText::new(rust_i18n::t!("dev_warning_fmt", code = code).to_string()).strong());
        }

        ui.add_space(4.0);

        // Row 1: Power On/Off | Operate/Standby (state+color) | Tune
        ui.horizontal(|ui| {
            // Power - shows current state
            if !self.spe_connected || self.spe_state == 0 {
                let btn = egui::Button::new(RichText::new(rust_i18n::t!("dev_power_off").to_string()).strong().color(Color32::WHITE))
                    .fill(Color32::from_rgb(120, 120, 120));
                if ui.add(btn).clicked() {
                    let _ = self.cmd_tx.send(Command::SpePowerOn);
                }
            } else {
                let btn = egui::Button::new(RichText::new(rust_i18n::t!("dev_power_on").to_string()).strong().color(Color32::WHITE))
                    .fill(Color32::from_rgb(0, 150, 0));
                if ui.add(btn).clicked() {
                    let _ = self.cmd_tx.send(Command::SpeOff);
                }
            }

            // Operate/Standby - shows current state
            let (op_text, op_color): (String, Color32) = match self.spe_state {
                2 => (rust_i18n::t!("dev_operate").to_string(), Color32::from_rgb(50, 180, 50)),
                1 => (rust_i18n::t!("dev_standby").to_string(), amber),
                _ => (rust_i18n::t!("dev_off").to_string(), Color32::from_rgb(120, 120, 120)),
            };
            let btn = egui::Button::new(RichText::new(op_text).strong().color(Color32::WHITE))
                .fill(op_color);
            if ui.add_enabled(self.spe_connected, btn).clicked() {
                let _ = self.cmd_tx.send(Command::SpeOperate);
            }

            // Tune
            if ui.add_enabled(self.spe_connected && self.spe_state == 2,
                egui::Button::new(rust_i18n::t!("dev_tune").to_string())).clicked() {
                let _ = self.cmd_tx.send(Command::SpeTune);
            }
        });

        ui.add_space(2.0);

        // Row 2: Ant{N} | In {N} | Low/Mid/High | Band label | Drive (read-only)
        ui.horizontal(|ui| {
            // Antenna toggle - shows bypass/tuner suffix
            let bypass_suffix = if self.spe_atu_bypassed { "b" } else { "" };
            let ant_text = format!("Ant{}{}", self.spe_antenna, bypass_suffix);
            let btn = if self.spe_atu_bypassed {
                egui::Button::new(RichText::new(&ant_text).color(Color32::from_rgb(100, 160, 255)))
            } else {
                egui::Button::new(&ant_text)
            };
            if ui.add_enabled(self.spe_connected, btn).clicked() {
                let _ = self.cmd_tx.send(Command::SpeAntenna);
            }

            // Input toggle
            let input_text = format!("In {}", self.spe_input);
            if ui.add_enabled(self.spe_connected, egui::Button::new(&input_text)).clicked() {
                let _ = self.cmd_tx.send(Command::SpeInput);
            }

            // Power level toggle
            let pwr_text: String = match self.spe_power_level {
                0 => rust_i18n::t!("dev_low").to_string(),
                1 => rust_i18n::t!("dev_mid").to_string(),
                2 => rust_i18n::t!("dev_high").to_string(),
                _ => "?".to_string(),
            };
            if ui.add_enabled(self.spe_connected, egui::Button::new(pwr_text)).clicked() {
                let _ = self.cmd_tx.send(Command::SpePower);
            }

            ui.separator();

            // Drive level +/-
            let drive_enabled = self.spe_connected && self.spe_state == 2 && self.spe_active;
            if ui.add_enabled(drive_enabled, egui::Button::new(rust_i18n::t!("dev_drive_minus").to_string())).clicked() {
                let _ = self.cmd_tx.send(Command::SpeDriveDown);
            }
            ui.label(format!("{}%", self.drive_level));
            if ui.add_enabled(drive_enabled, egui::Button::new(rust_i18n::t!("dev_drive_plus").to_string())).clicked() {
                let _ = self.cmd_tx.send(Command::SpeDriveUp);
            }
        });

        ui.add_space(4.0);

        // Peak hold: update peak, decay after 1 second
        let now = Instant::now();
        if self.spe_power_w > self.spe_peak_power {
            self.spe_peak_power = self.spe_power_w;
            self.spe_peak_time = now;
        } else if now.duration_since(self.spe_peak_time).as_millis() > 1000 {
            self.spe_peak_power = self.spe_power_w;
            self.spe_peak_time = now;
        }

        // Auto-scale based on power level: L=500W, M=1000W, H=1500W
        let (max_w, divisions): (f32, &[(f32, &str)]) = match self.spe_power_level {
            0 => (500.0, &[(0.0, "0"), (100.0, "100"), (200.0, "200"), (300.0, "300"), (400.0, "400"), (500.0, "500")]),
            1 => (1000.0, &[(0.0, "0"), (200.0, "200"), (400.0, "400"), (600.0, "600"), (800.0, "800"), (1000.0, "1k")]),
            _ => (1500.0, &[(0.0, "0"), (300.0, "300"), (600.0, "600"), (900.0, "900"), (1200.0, "1.2k"), (1500.0, "1.5k")]),
        };

        // Power bar with divisions and peak hold
        let bar_w = 300.0f32;
        let bar_h = 18.0f32;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(bar_w, bar_h + 14.0), egui::Sense::hover());
        let bar_rect = egui::Rect::from_min_size(rect.left_top(), egui::vec2(bar_w, bar_h));

        // Background
        ui.painter().rect_filled(bar_rect, 2.0, Color32::from_rgb(50, 50, 50));

        // Fill bar (realtime)
        let frac = (self.spe_power_w as f32 / max_w).clamp(0.0, 1.0);
        let fill_rect = egui::Rect::from_min_size(bar_rect.left_top(), egui::vec2(bar_w * frac, bar_h));
        let bar_color = if frac > 0.9 { Color32::from_rgb(255, 80, 80) }
            else if frac > 0.7 { amber }
            else { Color32::from_rgb(50, 180, 50) };
        ui.painter().rect_filled(fill_rect, 2.0, bar_color);

        // Peak hold marker (thin white line)
        let peak_frac = (self.spe_peak_power as f32 / max_w).clamp(0.0, 1.0);
        if peak_frac > 0.01 {
            let peak_x = bar_rect.left() + bar_w * peak_frac;
            ui.painter().line_segment(
                [egui::pos2(peak_x, bar_rect.top()), egui::pos2(peak_x, bar_rect.bottom())],
                egui::Stroke::new(2.0, Color32::WHITE),
            );
        }

        // Watt text inside bar
        if self.spe_power_w > 0 {
            ui.painter().text(
                bar_rect.center(),
                egui::Align2::CENTER_CENTER,
                format!("{}W", self.spe_power_w),
                egui::FontId::proportional(12.0),
                Color32::WHITE,
            );
        }

        // Division tick marks + labels below bar
        let label_y = bar_rect.bottom() + 1.0;
        for &(watts, label) in divisions {
            let x = bar_rect.left() + bar_w * (watts / max_w);
            ui.painter().line_segment(
                [egui::pos2(x, bar_rect.bottom()), egui::pos2(x, bar_rect.bottom() + 3.0)],
                egui::Stroke::new(1.0, Color32::from_rgb(140, 140, 140)),
            );
            ui.painter().text(
                egui::pos2(x, label_y + 3.0),
                egui::Align2::CENTER_TOP,
                label,
                egui::FontId::proportional(9.0),
                Color32::from_rgb(160, 160, 160),
            );
        }

        // Telemetry: Band, SWR, Temp, Voltage, Current
        ui.horizontal(|ui| {
            ui.label(RichText::new(spe_band_name(self.spe_band)).strong());
            let swr = self.spe_swr_x10 as f32 / 10.0;
            let swr_color = if swr > 3.0 { Color32::from_rgb(255, 80, 80) }
                else if swr > 2.0 { amber }
                else { ui.visuals().text_color() };
            ui.colored_label(swr_color, format!("SWR {:.1}", swr));
            ui.label(format!("{}°C", self.spe_temp));
            ui.label(format!("{:.1}V", self.spe_voltage_x10 as f32 / 10.0));
            ui.label(format!("{:.1}A", self.spe_current_x10 as f32 / 10.0));
        });
    }

    pub(super) fn render_device_rf2k(&mut self, ui: &mut egui::Ui, amber: Color32) {
        // Header: title + active badge + Online/Offline
        ui.horizontal(|ui| {
            let title = if self.rf2k_device_name.is_empty() {
                "RF2K-S".to_string()
            } else {
                format!("RF2K-S ({})", self.rf2k_device_name)
            };
            ui.heading(title);
            if self.rf2k_active {
                ui.colored_label(Color32::from_rgb(50, 180, 50), RichText::new(rust_i18n::t!("dev_active_upper").to_string()).strong());
            } else {
                ui.colored_label(Color32::from_rgb(140, 140, 140), rust_i18n::t!("dev_inactive_upper").to_string());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.rf2k_connected {
                    ui.colored_label(Color32::GREEN, rust_i18n::t!("dev_online").to_string());
                } else {
                    ui.colored_label(Color32::RED, rust_i18n::t!("dev_offline").to_string());
                }
            });
        });
        ui.separator();

        // Error bar
        if self.rf2k_error_state != 0 {
            let error_text = if self.rf2k_error_text.is_empty() {
                let state = self.rf2k_error_state;
                rust_i18n::t!("dev_error_state_fmt", state = state).to_string()
            } else {
                self.rf2k_error_text.clone()
            };
            ui.horizontal(|ui| {
                ui.colored_label(Color32::from_rgb(255, 80, 80),
                    RichText::new(&error_text).strong());
                if ui.button(rust_i18n::t!("dev_reset").to_string()).clicked() {
                    let _ = self.cmd_tx.send(Command::Rf2kErrorReset);
                }
            });
        }

        // Row 1: Operate/Standby + Tune + FW Close
        ui.horizontal(|ui| {
            let (op_text, op_color): (String, Color32) = if self.rf2k_operate {
                (rust_i18n::t!("dev_operate").to_string(), Color32::from_rgb(50, 180, 50))
            } else {
                (rust_i18n::t!("dev_standby").to_string(), amber)
            };
            let btn = egui::Button::new(RichText::new(op_text).strong().color(Color32::WHITE))
                .fill(op_color);
            if ui.add_enabled(self.rf2k_connected, btn).clicked() {
                let _ = self.cmd_tx.send(Command::Rf2kOperate(!self.rf2k_operate));
            }

            if ui.add_enabled(self.rf2k_connected && self.rf2k_operate,
                egui::Button::new(rust_i18n::t!("dev_tune").to_string())).clicked() {
                let _ = self.cmd_tx.send(Command::Rf2kTune);
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(self.rf2k_connected,
                    egui::Button::new(RichText::new(rust_i18n::t!("dev_fw_close").to_string()).color(Color32::from_rgb(255, 100, 100)))
                ).clicked() {
                    self.rf2k_confirm_fw_close = true;
                }
            });
        });

        // Row 2: Antenna buttons + Band + Freq
        ui.horizontal(|ui| {
            let int_ant = self.rf2k_antenna_type == 0;
            for (nr, cmd) in [(1u8, Command::Rf2kAnt1), (2, Command::Rf2kAnt2),
                              (3, Command::Rf2kAnt3), (4, Command::Rf2kAnt4)] {
                let is_active = int_ant && self.rf2k_antenna_number == nr;
                let label = format!("{}", nr);
                let btn = if is_active {
                    egui::Button::new(RichText::new(&label).strong().color(Color32::WHITE))
                        .fill(Color32::from_rgb(50, 180, 50))
                } else {
                    egui::Button::new(&label)
                };
                if ui.add_enabled(self.rf2k_connected, btn).clicked() {
                    let _ = self.cmd_tx.send(cmd);
                }
            }
            let ext_active = self.rf2k_antenna_type == 1;
            let ext_btn = if ext_active {
                egui::Button::new(RichText::new("Ext").strong().color(Color32::WHITE))
                    .fill(Color32::from_rgb(50, 180, 50))
            } else {
                egui::Button::new("Ext")
            };
            if ui.add_enabled(self.rf2k_connected, ext_btn).clicked() {
                let _ = self.cmd_tx.send(Command::Rf2kAntExt);
            }

            ui.separator();
            ui.label(RichText::new(rf2k_band_name(self.rf2k_band)).strong());
            if self.rf2k_frequency_khz > 0 {
                ui.label(format!("{} kHz", self.rf2k_frequency_khz));
            }
        });

        // Power bar with peak hold
        let now = Instant::now();
        if self.rf2k_forward_w > self.rf2k_peak_power {
            self.rf2k_peak_power = self.rf2k_forward_w;
            self.rf2k_peak_time = now;
        } else if now.duration_since(self.rf2k_peak_time).as_millis() > 1000 {
            self.rf2k_peak_power = self.rf2k_forward_w;
            self.rf2k_peak_time = now;
        }

        // Auto-scale: 200, 500, 1000, 1500W
        let max_w: f32 = if self.rf2k_max_forward_w > 1000 { 1500.0 }
            else if self.rf2k_max_forward_w > 500 { 1000.0 }
            else if self.rf2k_max_forward_w > 200 { 500.0 }
            else { 200.0 };

        let bar_w = 300.0f32;
        let bar_h = 18.0f32;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(bar_w, bar_h + 14.0), egui::Sense::hover());
        let bar_rect = egui::Rect::from_min_size(rect.left_top(), egui::vec2(bar_w, bar_h));

        ui.painter().rect_filled(bar_rect, 2.0, Color32::from_rgb(50, 50, 50));

        let frac = (self.rf2k_forward_w as f32 / max_w).clamp(0.0, 1.0);
        let fill_rect = egui::Rect::from_min_size(bar_rect.left_top(), egui::vec2(bar_w * frac, bar_h));
        let bar_color = if frac > 0.9 { Color32::from_rgb(255, 80, 80) }
            else if frac > 0.7 { amber }
            else { Color32::from_rgb(50, 180, 50) };
        ui.painter().rect_filled(fill_rect, 2.0, bar_color);

        // Peak hold marker
        let peak_frac = (self.rf2k_peak_power as f32 / max_w).clamp(0.0, 1.0);
        if peak_frac > 0.01 {
            let peak_x = bar_rect.left() + bar_w * peak_frac;
            ui.painter().line_segment(
                [egui::pos2(peak_x, bar_rect.top()), egui::pos2(peak_x, bar_rect.bottom())],
                egui::Stroke::new(2.0, Color32::WHITE),
            );
        }

        if self.rf2k_forward_w > 0 {
            ui.painter().text(
                bar_rect.center(),
                egui::Align2::CENTER_CENTER,
                format!("{}W", self.rf2k_forward_w),
                egui::FontId::proportional(12.0),
                Color32::WHITE,
            );
        }

        // Division labels
        let divisions: u16 = max_w as u16;
        let step = divisions / 5;
        let label_y = bar_rect.bottom() + 1.0;
        for i in 0..=5 {
            let watts = step * i;
            let x = bar_rect.left() + bar_w * (watts as f32 / max_w);
            ui.painter().line_segment(
                [egui::pos2(x, bar_rect.bottom()), egui::pos2(x, bar_rect.bottom() + 3.0)],
                egui::Stroke::new(1.0, Color32::from_rgb(140, 140, 140)),
            );
            let label = if watts >= 1000 { format!("{}k", watts / 1000) } else { format!("{}", watts) };
            ui.painter().text(
                egui::pos2(x, label_y + 3.0),
                egui::Align2::CENTER_TOP,
                &label,
                egui::FontId::proportional(9.0),
                Color32::from_rgb(160, 160, 160),
            );
        }

        // Tuner controls
        let tuner_edit = self.rf2k_connected && !self.rf2k_operate && self.rf2k_forward_w < 30;
        let is_manual = self.rf2k_tuner_mode == 2;
        ui.horizontal(|ui| {
            let mode_text = match self.rf2k_tuner_mode {
                0 => "OFF",
                1 => "BYP",
                2 => "MAN",
                3 | 5 => "TUNING",
                4 => "AUTO",
                _ => "?",
            };
            let mode_color = match self.rf2k_tuner_mode {
                3 | 5 => Color32::from_rgb(200, 200, 50),
                4 => Color32::from_rgb(50, 180, 50),
                2 => Color32::from_rgb(100, 160, 255),
                _ => ui.visuals().text_color(),
            };
            ui.colored_label(mode_color, RichText::new(rust_i18n::t!("dev_tuner_fmt", mode = mode_text).to_string()).strong());

            // MAN/AUTO toggle - shows current state
            if self.rf2k_tuner_mode == 2 || self.rf2k_tuner_mode == 4 {
                let (_toggle_text, toggle_btn) = if is_manual {
                    ("Manual", egui::Button::new(RichText::new(rust_i18n::t!("dev_manual").to_string()).strong())
                        .fill(Color32::from_rgb(100, 160, 230)).small())
                } else {
                    ("Auto", egui::Button::new(RichText::new(rust_i18n::t!("dev_auto").to_string()).strong())
                        .fill(Color32::from_rgb(100, 160, 230)).small())
                };
                if ui.add_enabled(tuner_edit, toggle_btn).clicked() {
                    let _ = self.cmd_tx.send(Command::Rf2kTunerMode(if is_manual { 1 } else { 0 }));
                }
            }

            // Bypass - shows current state
            let is_bypass = self.rf2k_tuner_mode == 1 || self.rf2k_tuner_setup == "BYPASS";
            let byp_btn = if is_bypass {
                egui::Button::new(RichText::new(rust_i18n::t!("dev_bypass").to_string()).strong())
                    .fill(Color32::from_rgb(255, 170, 40)).small()
            } else {
                egui::Button::new(rust_i18n::t!("dev_bypass").to_string()).small()
            };
            if ui.add_enabled(tuner_edit, byp_btn).clicked() {
                let _ = self.cmd_tx.send(Command::Rf2kTunerBypass(!is_bypass));
            }

            // Reset + Store (manual only)
            if ui.add_enabled(tuner_edit && is_manual, egui::Button::new(rust_i18n::t!("dev_reset").to_string()).small()).clicked() {
                let _ = self.cmd_tx.send(Command::Rf2kTunerReset);
            }
            if ui.add_enabled(tuner_edit && is_manual, egui::Button::new(rust_i18n::t!("dev_store").to_string()).small()).clicked() {
                let _ = self.cmd_tx.send(Command::Rf2kTunerStore);
            }
        });

        // Manual L/C/K controls
        if is_manual {
            ui.horizontal(|ui| {
                if !self.rf2k_tuner_setup.is_empty() {
                    ui.label(&self.rf2k_tuner_setup);
                }
                if ui.add_enabled(tuner_edit, egui::Button::new("K").small()).clicked() {
                    let _ = self.cmd_tx.send(Command::Rf2kTunerK);
                }
                ui.separator();
                ui.label(format!("L:{}", self.rf2k_tuner_l_nh));
                if ui.add_enabled(tuner_edit, egui::Button::new("−").small()).clicked() {
                    let _ = self.cmd_tx.send(Command::Rf2kTunerLDown);
                }
                if ui.add_enabled(tuner_edit, egui::Button::new("+").small()).clicked() {
                    let _ = self.cmd_tx.send(Command::Rf2kTunerLUp);
                }
                ui.separator();
                ui.label(format!("C:{}", self.rf2k_tuner_c_pf));
                if ui.add_enabled(tuner_edit, egui::Button::new("−").small()).clicked() {
                    let _ = self.cmd_tx.send(Command::Rf2kTunerCDown);
                }
                if ui.add_enabled(tuner_edit, egui::Button::new("+").small()).clicked() {
                    let _ = self.cmd_tx.send(Command::Rf2kTunerCUp);
                }
            });
        } else {
            ui.horizontal(|ui| {
                if !self.rf2k_tuner_setup.is_empty() {
                    ui.label(&self.rf2k_tuner_setup);
                }
                if self.rf2k_tuner_l_nh > 0 || self.rf2k_tuner_c_pf > 0 {
                    ui.label(format!("L:{}nH C:{}pF", self.rf2k_tuner_l_nh, self.rf2k_tuner_c_pf));
                }
            });
        }

        // Drive row
        ui.horizontal(|ui| {
            let mod_color = match self.rf2k_modulation.as_str() {
                "SSB" => Color32::from_rgb(100, 160, 255),
                "AM" => amber,
                "CONT" => Color32::from_rgb(50, 180, 50),
                _ => ui.visuals().text_color(),
            };
            if !self.rf2k_modulation.is_empty() {
                ui.colored_label(mod_color, RichText::new(&self.rf2k_modulation).strong());
            }
            let w = self.rf2k_drive_w;
            ui.label(rust_i18n::t!("dev_drive_w_fmt", w = w).to_string());

            let drive_enabled = self.rf2k_connected && self.rf2k_operate && self.rf2k_active;
            if ui.add_enabled(drive_enabled, egui::Button::new("-")).clicked() {
                let _ = self.cmd_tx.send(Command::Rf2kDriveDown);
            }
            if ui.add_enabled(drive_enabled, egui::Button::new("+")).clicked() {
                let _ = self.cmd_tx.send(Command::Rf2kDriveUp);
            }
        });

        // Telemetry row
        ui.horizontal(|ui| {
            let swr = self.rf2k_swr_x100 as f32 / 100.0;
            let swr_color = if swr > 3.0 { Color32::from_rgb(255, 80, 80) }
                else if swr > 2.0 { amber }
                else { ui.visuals().text_color() };
            ui.colored_label(swr_color, format!("SWR {:.2}", swr));
            ui.label(format!("{:.1}°C", self.rf2k_temperature_x10 as f32 / 10.0));
            ui.label(format!("{:.1}V", self.rf2k_voltage_x10 as f32 / 10.0));
            ui.label(format!("{:.1}A", self.rf2k_current_x10 as f32 / 10.0));
            if self.rf2k_reflected_w > 0 {
                ui.label(format!("Refl: {}W", self.rf2k_reflected_w));
            }
        });

        // --- Debug section (Phase D) ---
        if self.rf2k_debug_available {
            ui.add_space(6.0);
            let debug_header = if self.rf2k_show_debug { rust_i18n::t!("dev_debug_expanded").to_string() } else { rust_i18n::t!("dev_debug_collapsed").to_string() };
            if ui.selectable_label(self.rf2k_show_debug, RichText::new(debug_header).strong()).clicked() {
                self.rf2k_show_debug = !self.rf2k_show_debug;
            }

            if self.rf2k_show_debug {
                ui.indent("rf2k_debug_c", |ui| {
                    ui.label(RichText::new(rust_i18n::t!("dev_system_info").to_string()).strong());
                    ui.horizontal(|ui| {
                        ui.label(format!("FW: v{}", self.rf2k_controller_version));
                        if !self.rf2k_hw_revision.is_empty() {
                            ui.label(format!("HW: {}", self.rf2k_hw_revision));
                        }
                        ui.label(format!("BIAS: {:.1}%", self.rf2k_bias_pct_x10 as f32 / 10.0));
                        let psu: String = match self.rf2k_psu_source { 0 => rust_i18n::t!("dev_internal").to_string(), 1 => rust_i18n::t!("dev_external").to_string(), 2 => "CAN Ctrl".to_string(), _ => "?".to_string() };
                        ui.label(format!("PSU: {}", psu));
                    });
                    ui.horizontal(|ui| {
                        let hours = self.rf2k_uptime_s / 3600;
                        let mins = (self.rf2k_uptime_s % 3600) / 60;
                        if hours >= 24 {
                            let d = hours / 24; let h = hours % 24; let m = mins;
                            ui.label(rust_i18n::t!("dev_uptime_dhm", d = d, h = h, m = m).to_string());
                        } else {
                            let h = hours; let m = mins;
                            ui.label(rust_i18n::t!("dev_uptime_hm", h = h, m = m).to_string());
                        }
                        let tx_h = self.rf2k_tx_time_s / 3600;
                        let tx_m = (self.rf2k_tx_time_s % 3600) / 60;
                        ui.label(format!("TX: {}h {:02}m", tx_h, tx_m));
                        let count = self.rf2k_error_count;
                        ui.label(rust_i18n::t!("dev_errors_fmt", count = count).to_string());
                    });
                    ui.horizontal(|ui| {
                        let n = self.rf2k_storage_bank;
                        ui.label(rust_i18n::t!("dev_bank_fmt", n = n).to_string());
                        ui.label(rust_i18n::t!("dev_frq_delay").to_string());
                        if ui.add_enabled(self.rf2k_connected, egui::Button::new("−").small()).clicked() {
                            let _ = self.cmd_tx.send(Command::Rf2kFrqDelayDown);
                        }
                        ui.label(format!("{}", self.rf2k_frq_delay));
                        if ui.add_enabled(self.rf2k_connected, egui::Button::new("+").small()).clicked() {
                            let _ = self.cmd_tx.send(Command::Rf2kFrqDelayUp);
                        }
                    });

                    ui.add_space(4.0);
                    ui.label(RichText::new(rust_i18n::t!("dev_settings").to_string()).strong());
                    ui.horizontal(|ui| {
                        ui.label(rust_i18n::t!("dev_power_colon").to_string());
                        let (pe5_text, pe5_color): (String, Color32) = if self.rf2k_high_power {
                            (rust_i18n::t!("dev_high_upper").to_string(), Color32::from_rgb(255, 80, 80))
                        } else {
                            (rust_i18n::t!("dev_low_upper").to_string(), Color32::from_rgb(50, 180, 50))
                        };
                        let pe5_btn = egui::Button::new(RichText::new(pe5_text).strong().color(Color32::WHITE)).fill(pe5_color);
                        if ui.add_enabled(self.rf2k_connected, pe5_btn).clicked() {
                            if self.rf2k_high_power {
                                let _ = self.cmd_tx.send(Command::Rf2kSetHighPower(false));
                            } else {
                                self.rf2k_confirm_high_power = true;
                            }
                        }
                        ui.separator();
                        ui.label(rust_i18n::t!("dev_tuner_6m").to_string());
                        let t6m = if self.rf2k_tuner_6m { rust_i18n::t!("dev_on_upper").to_string() } else { rust_i18n::t!("dev_off_upper").to_string() };
                        if ui.add_enabled(self.rf2k_connected, egui::Button::new(t6m).small()).clicked() {
                            let _ = self.cmd_tx.send(Command::Rf2kSetTuner6m(!self.rf2k_tuner_6m));
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label(rust_i18n::t!("dev_band_gap").to_string());
                        let bg = if self.rf2k_band_gap_allowed { rust_i18n::t!("dev_on_upper").to_string() } else { rust_i18n::t!("dev_off_upper").to_string() };
                        if ui.add_enabled(self.rf2k_connected, egui::Button::new(bg).small()).clicked() {
                            let _ = self.cmd_tx.send(Command::Rf2kSetBandGap(!self.rf2k_band_gap_allowed));
                        }
                        ui.separator();
                        ui.label("AT thresh:");
                        if ui.add_enabled(self.rf2k_connected, egui::Button::new("−").small()).clicked() {
                            let _ = self.cmd_tx.send(Command::Rf2kAutotuneThresholdDown);
                        }
                        ui.label(format!("{:.1} dB", self.rf2k_autotune_threshold_x10 as f32 / 10.0));
                        if ui.add_enabled(self.rf2k_connected, egui::Button::new("+").small()).clicked() {
                            let _ = self.cmd_tx.send(Command::Rf2kAutotuneThresholdUp);
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("DAC ALC:");
                        if ui.add_enabled(self.rf2k_connected, egui::Button::new("−").small()).clicked() {
                            let _ = self.cmd_tx.send(Command::Rf2kDacAlcDown);
                        }
                        ui.label(format!("{}", self.rf2k_dac_alc));
                        if ui.add_enabled(self.rf2k_connected, egui::Button::new("+").small()).clicked() {
                            let _ = self.cmd_tx.send(Command::Rf2kDacAlcUp);
                        }
                    });

                    if !self.rf2k_error_history.is_empty() {
                        ui.add_space(4.0);
                        ui.label(RichText::new(rust_i18n::t!("dev_error_history").to_string()).strong());
                        egui::ScrollArea::vertical().max_height(100.0).id_salt("rf2k_err_hist_c").show(ui, |ui| {
                            for (time, err) in self.rf2k_error_history.iter().rev() {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(time).monospace());
                                    ui.colored_label(amber, err);
                                });
                            }
                        });
                    }

                    ui.add_space(4.0);
                    ui.label(RichText::new(rust_i18n::t!("dev_dangerous").to_string()).strong());
                    let zero_btn = egui::Button::new(RichText::new(rust_i18n::t!("dev_zero_fram").to_string()).color(Color32::from_rgb(255, 100, 100)));
                    if ui.add_enabled(self.rf2k_connected, zero_btn).clicked() {
                        self.rf2k_confirm_zero_fram = true;
                    }
                });
            }

            // Confirmation dialogs
            if self.rf2k_confirm_high_power {
                egui::Window::new(rust_i18n::t!("dev_warning_upper").to_string())
                    .collapsible(false).resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ui.ctx(), |ui| {
                        ui.label(rust_i18n::t!("dev_high_power_warn1").to_string());
                        ui.label(rust_i18n::t!("dev_are_you_sure").to_string());
                        ui.horizontal(|ui| {
                            if ui.button(rust_i18n::t!("dev_yes_set_high").to_string()).clicked() {
                                let _ = self.cmd_tx.send(Command::Rf2kSetHighPower(true));
                                self.rf2k_confirm_high_power = false;
                            }
                            if ui.button(rust_i18n::t!("dev_cancel").to_string()).clicked() {
                                self.rf2k_confirm_high_power = false;
                            }
                        });
                    });
            }
            if self.rf2k_confirm_zero_fram {
                egui::Window::new(rust_i18n::t!("dev_destructive").to_string())
                    .collapsible(false).resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ui.ctx(), |ui| {
                        ui.label(rust_i18n::t!("dev_all_mem_erased").to_string());
                        ui.label(rust_i18n::t!("dev_cannot_be_undone").to_string());
                        ui.horizontal(|ui| {
                            if ui.button(RichText::new(rust_i18n::t!("dev_yes_zero_fram").to_string()).color(Color32::from_rgb(255, 80, 80))).clicked() {
                                let _ = self.cmd_tx.send(Command::Rf2kZeroFRAM);
                                self.rf2k_confirm_zero_fram = false;
                            }
                            if ui.button(rust_i18n::t!("dev_cancel").to_string()).clicked() {
                                self.rf2k_confirm_zero_fram = false;
                            }
                        });
                    });
            }

            // --- Drive Config section ---
            ui.add_space(6.0);
            let drive_header = if self.rf2k_show_drive_config { rust_i18n::t!("dev_drive_config_expanded").to_string() } else { rust_i18n::t!("dev_drive_config_collapsed").to_string() };
            if ui.selectable_label(self.rf2k_show_drive_config, RichText::new(drive_header).strong()).clicked() {
                self.rf2k_show_drive_config = !self.rf2k_show_drive_config;
                if self.rf2k_show_drive_config && !self.rf2k_drive_loaded {
                    self.rf2k_drive_edit[0] = self.rf2k_drive_config_ssb;
                    self.rf2k_drive_edit[1] = self.rf2k_drive_config_am;
                    self.rf2k_drive_edit[2] = self.rf2k_drive_config_cont;
                    self.rf2k_drive_loaded = true;
                }
            }

            if self.rf2k_show_drive_config {
                if !self.rf2k_drive_loaded {
                    self.rf2k_drive_edit[0] = self.rf2k_drive_config_ssb;
                    self.rf2k_drive_edit[1] = self.rf2k_drive_config_am;
                    self.rf2k_drive_edit[2] = self.rf2k_drive_config_cont;
                    self.rf2k_drive_loaded = true;
                }
                ui.indent("rf2k_drive_c", |ui| {
                    let bands = ["160m", "80m", "60m", "40m", "30m", "20m", "17m", "15m", "12m", "10m", "6m"];
                    let categories = ["SSB", "AM", "CONT"];
                    egui::Grid::new("rf2k_drive_grid_c").striped(true).min_col_width(40.0).show(ui, |ui| {
                        ui.label(RichText::new(rust_i18n::t!("dev_band").to_string()).strong());
                        for cat in &categories { ui.label(RichText::new(*cat).strong()); }
                        ui.end_row();
                        for band_idx in 0..11 {
                            ui.label(bands[band_idx]);
                            for cat_idx in 0..3 {
                                let mut val = self.rf2k_drive_edit[cat_idx][band_idx] as i32;
                                let drag = egui::DragValue::new(&mut val).range(0..=100).suffix("W").speed(0.5);
                                if ui.add(drag).changed() {
                                    self.rf2k_drive_edit[cat_idx][band_idx] = val.clamp(0, 100) as u8;
                                }
                            }
                            ui.end_row();
                        }
                    });
                    ui.add_space(4.0);
                    if ui.add_enabled(self.rf2k_connected, egui::Button::new(rust_i18n::t!("dev_save_to_pi").to_string())).clicked() {
                        for cat_idx in 0..3u8 {
                            let current = match cat_idx { 0 => &self.rf2k_drive_config_ssb, 1 => &self.rf2k_drive_config_am, _ => &self.rf2k_drive_config_cont };
                            for band_idx in 0..11u8 {
                                let new_val = self.rf2k_drive_edit[cat_idx as usize][band_idx as usize];
                                if new_val != current[band_idx as usize] {
                                    let _ = self.cmd_tx.send(Command::Rf2kSetDriveConfig { category: cat_idx, band: band_idx, value: new_val });
                                }
                            }
                        }
                    }
                });
            }
        }

        // FW Close confirmation popup
        if self.rf2k_confirm_fw_close {
            egui::Window::new(rust_i18n::t!("dev_fw_close_confirm").to_string())
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui.ctx(), |ui| {
                    ui.label(rust_i18n::t!("dev_fw_close_confirm_body").to_string());
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(rust_i18n::t!("dev_yes").to_string()).clicked() {
                            let _ = self.cmd_tx.send(Command::Rf2kClose);
                            self.rf2k_confirm_fw_close = false;
                        }
                        if ui.button(rust_i18n::t!("dev_no").to_string()).clicked() {
                            self.rf2k_confirm_fw_close = false;
                        }
                    });
                });
        }
    }

    pub(super) fn render_device_ultrabeam(&mut self, ui: &mut egui::Ui, _amber: Color32) {
        // Header: heading on the left, Online/Offline + FW on the right,
        // Menu toggle button between them so the layout mirrors the server window.
        ui.horizontal(|ui| {
            ui.heading("UltraBeam RCU-06");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.ub_connected {
                    ui.colored_label(Color32::GREEN, rust_i18n::t!("dev_online").to_string());
                } else {
                    ui.colored_label(Color32::RED, rust_i18n::t!("dev_offline").to_string());
                }
                if self.ub_fw_major > 0 {
                    ui.label(format!("FW {}.{}", self.ub_fw_major, self.ub_fw_minor));
                }
                ui.separator();
                if super::helpers::chevron_label(ui, self.ub_show_menu, rust_i18n::t!("dev_menu").to_string()).clicked() {
                    self.ub_show_menu = !self.ub_show_menu;
                    if self.ub_show_menu {
                        let _ = self.cmd_tx.send(Command::UbReadElements);
                    }
                    self.save_full_config();
                }
            });
        });
        ui.separator();

        // Frequency display
        if self.ub_frequency_khz > 0 {
            ui.horizontal(|ui| {
                let freq_mhz = self.ub_frequency_khz as f32 / 1000.0;
                ui.label(RichText::new(format!("{:.3} MHz", freq_mhz)).size(28.0).strong());
                let band_name = match self.ub_band {
                    0 => "6m", 1 => "10m", 2 => "12m", 3 => "15m", 4 => "17m",
                    5 => "20m", 6 => "30m", 7 => "40m", 8 => "60m", 9 => "80m", 10 => "160m",
                    _ => "?",
                };
                ui.label(RichText::new(band_name).size(20.0));
            });
        }

        // Direction buttons
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("dev_direction").to_string());
            let dirs: [(String, u8); 3] = [
                (rust_i18n::t!("dev_normal").to_string(), 0u8),
                ("180\u{00B0}".to_string(), 1),
                ("BiDir".to_string(), 2),
            ];
            for (label, dir) in &dirs {
                let is_active = self.ub_direction == *dir;
                let btn = if is_active {
                    egui::Button::new(RichText::new(label.as_str()).strong().color(Color32::WHITE))
                        .fill(Color32::from_rgb(50, 180, 50))
                } else {
                    egui::Button::new(label.as_str())
                };
                if ui.add_enabled(self.ub_connected, btn).clicked() {
                    let _ = self.cmd_tx.send(Command::UbSetFrequency(self.ub_frequency_khz, *dir));
                }
            }
        });

        // Frequency step buttons + sync
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("dev_freq_step").to_string());
            for &(label, step) in &[("-100", -100i32), ("-25", -25), ("+25", 25), ("+100", 100)] {
                if ui.add_enabled(self.ub_connected && self.ub_frequency_khz > 0,
                    egui::Button::new(label)).clicked() {
                    let new_khz = (self.ub_frequency_khz as i32 + step).max(1800).min(54000) as u16;
                    let _ = self.cmd_tx.send(Command::UbSetFrequency(new_khz, self.ub_direction));
                }
            }
            ui.separator();
            let (track_hz, track_label) = self.ub_track_vfo();
            let track_khz = (track_hz / 1000) as u16;
            let can_sync = self.ub_connected && track_khz >= 1800 && track_khz <= 54000
                && track_khz != self.ub_frequency_khz;
            let sync_btn = egui::Button::new(RichText::new(rust_i18n::t!("dev_sync_fmt", label = track_label).to_string()).strong())
                .fill(if can_sync { Color32::from_rgb(50, 130, 200) } else { Color32::from_rgb(80, 80, 80) });
            if ui.add_enabled(can_sync, sync_btn).on_hover_text(
                rust_i18n::t!("dev_set_ultrabeam_to", label = track_label, khz = track_khz).to_string()
            ).clicked() {
                let _ = self.cmd_tx.send(Command::UbSetFrequency(track_khz, self.ub_direction));
            }
            ui.checkbox(&mut self.ub_auto_track, rust_i18n::t!("dev_auto").to_string())
                .on_hover_text(rust_i18n::t!("dev_auto_track_fmt", label = track_label).to_string());
        });

        // Per-motor moving + progress bar (only shown while moving).
        // ub_motors_moving is a bitfield: bit 0 = motor 1, bit 1 = motor 2.
        // The progress bar is a shared value; the RCU-06 does not share
        // separate progress per motor.
        if self.ub_motors_moving != 0 {
            ui.add_space(4.0);
            let progress = (self.ub_motor_completion as f32 / 60.0).clamp(0.0, 1.0);
            let m1_active = (self.ub_motors_moving & 0x01) != 0;
            let m2_active = (self.ub_motors_moving & 0x02) != 0;
            let active_color = Color32::from_rgb(255, 170, 40);
            let idle_color = Color32::from_rgb(100, 100, 100);
            ui.horizontal(|ui| {
                ui.colored_label(
                    if m1_active { active_color } else { idle_color },
                    RichText::new("M1").strong(),
                );
                ui.colored_label(
                    if m2_active { active_color } else { idle_color },
                    RichText::new("M2").strong(),
                );
                let bar = egui::ProgressBar::new(progress)
                    .text(format!("{:.0}%", progress * 100.0));
                ui.add(bar);
            });
        }

        ui.add_space(4.0);

        // Band presets
        ui.horizontal_wrapped(|ui| {
            ui.label(rust_i18n::t!("dev_band_colon").to_string());
            let presets: &[(&str, u16)] = &[
                ("40m", 7100), ("30m", 10125), ("20m", 14175), ("17m", 18118),
                ("15m", 21225), ("12m", 24940), ("10m", 28500), ("6m", 50150),
            ];
            for &(name, center_khz) in presets {
                if ui.add_enabled(self.ub_connected,
                    egui::Button::new(name)).clicked() {
                    let _ = self.cmd_tx.send(Command::UbSetFrequency(center_khz, self.ub_direction));
                }
            }
        });

        ui.add_space(4.0);

        // Retract with confirmation
        ui.horizontal(|ui| {
            if ui.add_enabled(self.ub_connected,
                egui::Button::new(RichText::new(rust_i18n::t!("dev_retract").to_string()).color(Color32::from_rgb(255, 100, 100)))
            ).clicked() {
                self.ub_confirm_retract = true;
            }

            // Read elements
            if ui.add_enabled(self.ub_connected,
                egui::Button::new(rust_i18n::t!("dev_read_elements").to_string())).clicked() {
                let _ = self.cmd_tx.send(Command::UbReadElements);
            }
        });

        // Retract confirmation popup
        if self.ub_confirm_retract {
            egui::Window::new(rust_i18n::t!("dev_retract_confirm").to_string())
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui.ctx(), |ui| {
                    ui.label(rust_i18n::t!("dev_retract_confirm_body").to_string());
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(rust_i18n::t!("dev_yes").to_string()).clicked() {
                            let _ = self.cmd_tx.send(Command::UbRetract);
                            self.ub_confirm_retract = false;
                        }
                        if ui.button(rust_i18n::t!("dev_no").to_string()).clicked() {
                            self.ub_confirm_retract = false;
                        }
                    });
                });
        }

        // Collapsible Menu section - mirrors the server window so the
        // operator only has to learn one UX. Editable element lengths +/-,
        // Refresh, plus read-only Controller Info.
        if self.ub_show_menu {
            ui.add_space(8.0);
            ui.separator();
            ui.label(RichText::new(rust_i18n::t!("dev_menu").to_string()).strong().size(16.0));

            ui.add_space(4.0);
            ui.label(RichText::new(rust_i18n::t!("dev_element_lengths").to_string()).strong());
            ui.indent("ub_elements_client", |ui| {
                for i in 0..6 {
                    let len = self.ub_elements_mm[i];
                    if len > 0 {
                        ui.horizontal(|ui| {
                            ui.label(format!("E{}: {} mm", i + 1, len));
                            let can_edit = self.ub_connected;
                            if ui.add_enabled(can_edit && len > 10, egui::Button::new("-").small()).clicked() {
                                let _ = self.cmd_tx.send(Command::UbModifyElement(i as u8, len - 10));
                                let _ = self.cmd_tx.send(Command::UbReadElements);
                            }
                            if ui.add_enabled(can_edit, egui::Button::new("+").small()).clicked() {
                                let _ = self.cmd_tx.send(Command::UbModifyElement(i as u8, len + 10));
                                let _ = self.cmd_tx.send(Command::UbReadElements);
                            }
                        });
                    } else {
                        ui.label(format!("E{}: --", i + 1));
                    }
                }
                if ui.add_enabled(self.ub_connected, egui::Button::new(rust_i18n::t!("dev_refresh").to_string())).clicked() {
                    let _ = self.cmd_tx.send(Command::UbReadElements);
                }
            });

            ui.add_space(8.0);
            ui.label(RichText::new(rust_i18n::t!("dev_controller_info").to_string()).strong());
            ui.indent("ub_info_client", |ui| {
                ui.label(rust_i18n::t!("dev_model_2el").to_string());
                if self.ub_fw_major > 0 {
                    ui.label(format!("FW: v{}.{:02}", self.ub_fw_major, self.ub_fw_minor));
                }
                if self.ub_freq_min_mhz > 0 && self.ub_freq_max_mhz > 0 {
                    let min = self.ub_freq_min_mhz; let max = self.ub_freq_max_mhz;
                    ui.label(rust_i18n::t!("dev_freq_range_fmt", min = min, max = max).to_string());
                }
                let op_label: String = match self.ub_operation {
                    0 => rust_i18n::t!("dev_normal").to_string(),
                    2 => rust_i18n::t!("dev_user_adjust").to_string(),
                    3 => rust_i18n::t!("dev_setup").to_string(),
                    _ => rust_i18n::t!("dev_unknown").to_string(),
                };
                ui.label(rust_i18n::t!("dev_operation_mode_fmt", mode = op_label).to_string());
            });
        }
    }

    pub(super) fn render_device_rotor(&mut self, ui: &mut egui::Ui) {
        // Header
        ui.horizontal(|ui| {
            ui.heading(rust_i18n::t!("dev_rotor").to_string());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.rotor_connected {
                    ui.colored_label(Color32::GREEN, rust_i18n::t!("dev_online").to_string());
                } else {
                    ui.colored_label(Color32::RED, rust_i18n::t!("dev_offline").to_string());
                }
            });
        });
        ui.separator();

        let angle_deg = self.rotor_angle_x10 as f32 / 10.0;
        let target_deg = if self.rotor_rotating { Some(self.rotor_target_x10 as f32 / 10.0) } else { None };

        // Compass circle - click to GoTo
        if let Some(goto) = Self::render_compass(ui, angle_deg, target_deg, self.rotor_connected) {
            let _ = self.cmd_tx.send(Command::RotorGoTo(goto));
        }

        ui.add_space(4.0);

        // Stop button + GoTo text input
        ui.horizontal(|ui| {
            if ui.add_enabled(self.rotor_connected, egui::Button::new(rust_i18n::t!("dev_stop").to_string()).min_size(egui::vec2(70.0, 30.0))).clicked() {
                let _ = self.cmd_tx.send(Command::RotorStop);
            }

            ui.label(rust_i18n::t!("dev_goto").to_string());
            let resp = ui.add(egui::TextEdit::singleline(&mut self.rotor_goto_input).desired_width(60.0));
            if (ui.add_enabled(self.rotor_connected, egui::Button::new(rust_i18n::t!("dev_go").to_string())).clicked()
                || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))))
                && self.rotor_connected
            {
                if let Ok(deg) = self.rotor_goto_input.trim().parse::<f32>() {
                    let angle_x10 = (deg * 10.0).round() as u16;
                    if angle_x10 <= 3600 {
                        let _ = self.cmd_tx.send(Command::RotorGoTo(angle_x10));
                    }
                }
            }
        });
    }

    /// Draw a clickable compass circle. Returns Some(angle_x10) if the user clicked a position.
    pub(super) fn render_compass(ui: &mut egui::Ui, angle_deg: f32, target_deg: Option<f32>, connected: bool) -> Option<u16> {
        let size = 200.0_f32;
        let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
        let painter = ui.painter_at(rect);
        let center = rect.center();
        let radius = size * 0.45;

        let bg = ui.visuals().extreme_bg_color;
        let ring_color = ui.visuals().text_color().gamma_multiply(0.3);
        let text_color = ui.visuals().text_color().gamma_multiply(0.6);
        let needle_color = Color32::from_rgb(50, 200, 50);
        let target_color = Color32::from_rgb(255, 200, 40);

        // Background circle
        painter.circle_filled(center, radius + 2.0, bg);
        painter.circle_stroke(center, radius, egui::Stroke::new(1.5, ring_color));

        // Tick marks and labels
        let labels: [(&str, f32); 4] = [("N", 0.0), ("E", 90.0), ("S", 180.0), ("W", 270.0)];
        for (label, deg) in labels {
            let rad = (deg - 90.0).to_radians();
            let outer = center + egui::vec2(rad.cos(), rad.sin()) * radius;
            let inner = center + egui::vec2(rad.cos(), rad.sin()) * (radius - 8.0);
            painter.line_segment([inner, outer], egui::Stroke::new(1.0, ring_color));

            let text_pos = center + egui::vec2(rad.cos(), rad.sin()) * (radius + 12.0);
            painter.text(
                text_pos,
                egui::Align2::CENTER_CENTER,
                label,
                egui::FontId::proportional(12.0),
                if label == "N" { Color32::from_rgb(255, 80, 80) } else { text_color },
            );
        }

        // Minor ticks every 30°
        for i in 0..12 {
            let deg = i as f32 * 30.0;
            if deg % 90.0 == 0.0 { continue; }
            let rad = (deg - 90.0).to_radians();
            let outer = center + egui::vec2(rad.cos(), rad.sin()) * radius;
            let inner = center + egui::vec2(rad.cos(), rad.sin()) * (radius - 5.0);
            painter.line_segment([inner, outer], egui::Stroke::new(0.5, ring_color));
        }

        // Target line
        if let Some(tgt) = target_deg {
            let rad = (tgt - 90.0).to_radians();
            let tip = center + egui::vec2(rad.cos(), rad.sin()) * (radius - 10.0);
            let mid = center + egui::vec2(rad.cos(), rad.sin()) * (radius * 0.3);
            painter.line_segment([mid, tip], egui::Stroke::new(2.0, target_color));
        }

        // Current angle needle
        let rad = (angle_deg - 90.0).to_radians();
        let tip = center + egui::vec2(rad.cos(), rad.sin()) * (radius - 4.0);
        painter.line_segment([center, tip], egui::Stroke::new(2.5, needle_color));
        painter.circle_filled(center, 4.0, needle_color);

        // Angle text below compass
        painter.text(
            center + egui::vec2(0.0, radius * 0.55),
            egui::Align2::CENTER_CENTER,
            format!("{:.1}\u{00B0}", angle_deg),
            egui::FontId::proportional(18.0),
            ui.visuals().text_color(),
        );

        // Handle click
        if connected && response.clicked() {
            if let Some(pos) = response.interact_pointer_pos() {
                let dx = pos.x - center.x;
                let dy = pos.y - center.y;
                let dist = (dx * dx + dy * dy).sqrt();
                if dist > 10.0 {
                    let mut deg = dy.atan2(dx).to_degrees() + 90.0;
                    if deg < 0.0 { deg += 360.0; }
                    if deg >= 360.0 { deg -= 360.0; }
                    let angle_x10 = (deg * 10.0).round() as u16;
                    return Some(angle_x10);
                }
            }
        }

        None
    }
}