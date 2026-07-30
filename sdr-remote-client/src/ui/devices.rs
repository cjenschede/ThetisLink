// SPDX-License-Identifier: GPL-2.0-or-later

use super::*;

/// Visuele toestand voor `antenna_button`.
#[derive(Clone, Copy)]
enum AntennaState {
    Active,
    Blocked,
    Inactive,
}

/// Twee-regelige antenne-knop - visueel identiek aan de server-versie
/// (`sdr-remote-server::ui::amplitec::antenna_button`) zodat client en
/// server hetzelfde uiterlijk hebben. Geen rename-context-menu hier:
/// labels worden alleen op de server beheerd, clients tonen de huidige
/// CSV-broadcast.
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

        // Poort A - TX+RX
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

        // Poort B - RX
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

        // Power-cap tabel (collapsing section). Server pusht de actuele
        // tabel via AmplitecPowerTablePacket; edit-state wordt lokaal
        // bijgehouden en pas op "Save to server" naar de server gestuurd.
        if super::helpers::chevron_label(
            ui,
            self.amplitec_power_show,
            RichText::new(rust_i18n::t!("dev_power_cap_table").to_string()).strong(),
        )
        .clicked()
        {
            self.amplitec_power_show = !self.amplitec_power_show;
        }
        if self.amplitec_power_show {
            ui.indent("amplitec_power_table", |ui| {
                if !self.amplitec_power_loaded {
                    ui.colored_label(
                        Color32::from_rgb(180, 180, 180),
                        rust_i18n::t!("dev_waiting_for_server").to_string(),
                    );
                } else {
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
                                let mut val = self.amplitec_power_edit_max_w[i] as i32;
                                let drag = egui::DragValue::new(&mut val)
                                    .range(0..=3000)
                                    .suffix(" W")
                                    .speed(1.0);
                                if ui.add(drag).changed() {
                                    self.amplitec_power_edit_max_w[i] =
                                        val.clamp(0, 3000) as u16;
                                }
                                ui.checkbox(&mut self.amplitec_power_edit_tx_blocked[i], "");
                                ui.end_row();
                            }
                        });
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let dirty = self.amplitec_power_edit_max_w != self.amplitec_power_max_w
                            || self.amplitec_power_edit_tx_blocked != self.amplitec_power_tx_blocked;
                        if ui.add_enabled(dirty, egui::Button::new(rust_i18n::t!("dev_save_to_server").to_string())).clicked() {
                            let _ = self.cmd_tx.send(Command::SetAmplitecPowerTable {
                                max_w: self.amplitec_power_edit_max_w,
                                tx_blocked: self.amplitec_power_edit_tx_blocked,
                            });
                        }
                        if ui.add_enabled(dirty, egui::Button::new(rust_i18n::t!("dev_revert").to_string())).clicked() {
                            self.amplitec_power_edit_max_w = self.amplitec_power_max_w;
                            self.amplitec_power_edit_tx_blocked = self.amplitec_power_tx_blocked;
                        }
                        ui.label(
                            RichText::new(rust_i18n::t!("dev_no_cap").to_string())
                                .size(10.0)
                                .color(Color32::from_rgb(160, 160, 160)),
                        );
                    });
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

        // --- Debug section (Fase D) ---
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

        // Per-motor moving + progress bar (alleen tonen bij beweging).
        // ub_motors_moving is een bitfield: bit 0 = motor 1, bit 1 = motor 2.
        // De progress-balk is een gedeelde waarde; de RCU-06 deelt geen
        // afzonderlijke voortgang per motor.
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

    /// Shared DSP/function control block for a Yaesu radio slot (used by both slot 0
    /// and slot 1 so styling stays identical - single shared renderer, no per-window drift).
    /// PATCH-yaesu-extra-controls Fase A1: RF-ATT + Break-in toggles, driven by the
    /// feature-state bitfield (bit N = YaesuCtrl N). Blauwe fill = aan.
    /// Optimistische toggle-update: flip de lokale feature-bit meteen bij klik + zet de
    /// debounce, zodat de knop direct reageert; de poll bevestigt/corrigeert (~0,5s, vangnet).
    fn yaesu_toggle_flip(&mut self, slot: u8, bit: u32) {
        if slot == 0 {
            self.yaesu_feature_toggles ^= 1 << bit;
            self.yaesu_control_changed_at = Some(Instant::now());
        } else {
            self.yaesu2_feature_toggles ^= 1 << bit;
            self.yaesu2_control_changed_at = Some(Instant::now());
        }
    }

    /// Band/mode-beschikbaarheid van Yaesu-controls (FT-991A/FTX-1), per slot:
    /// - `hf_6m`: IPO/ATT en de interne ATU bestaan alleen op HF+6m (<54 MHz); op
    ///   2m/70cm heeft de radio geen menu-optie ervoor (tester-melding: knop klapt
    ///   na ~1 s terug). Buiten HF+6m uitgrijzen.
    /// - `is_fm`: NB/DNF/Notch/Contour + ATU zijn in FM niet bruikbaar op de 991A
    ///   (NAR/AGC/DNR werken volgens de 991A OM juist wél in FM).
    /// yaesu_mode-encoding: 5 = FM (zie render_yaesu_popout mode_label).
    fn yaesu_band_mode(&self, slot: u8) -> (bool, bool) {
        let (freq, mode) = if slot == 0 {
            (self.yaesu_freq_a, self.yaesu_mode)
        } else {
            (self.yaesu2_freq_a, self.yaesu2_mode)
        };
        // freq >= 1: bij connect (freq nog 0) geen HF-only-knoppen aanzetten vóór de
        // band bekend is (pariteit met Android). is_fm = FM-familie (5=FM, 12=C4FM).
        (freq >= 1 && freq < 54_000_000, matches!(mode, 5 | 12))
    }

    fn render_yaesu_dsp_block(&mut self, ui: &mut egui::Ui, slot: u8, toggles: u32, levels: [u8; 16]) {
        let (hf_6m, is_fm) = self.yaesu_band_mode(slot);
        // BK-IN is CW-only (991A OM: Break-In staat volledig onder CW Mode Operation).
        let is_cw = matches!(if slot == 0 { self.yaesu_mode } else { self.yaesu2_mode }, 3 | 4);
        let toggle_btn = |label: &str, on: bool| {
            if on {
                egui::Button::new(RichText::new(label).size(11.0).color(Color32::WHITE))
                    .fill(Color32::from_rgb(0, 90, 200)).min_size(egui::vec2(46.0, 20.0))
            } else {
                egui::Button::new(RichText::new(label).size(11.0)).min_size(egui::vec2(46.0, 20.0))
            }
        };
        ui.horizontal_wrapped(|ui| {
            ui.label("DSP:");
            let att_on = toggles & (1 << 0) != 0; // YaesuCtrl::RfAtt (HF+6m only)
            if ui.add_enabled(hf_6m, toggle_btn("ATT", att_on)).clicked() {
                let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, 0, if att_on { 0 } else { 1 }));
                self.yaesu_toggle_flip(slot, 0);
            }
            let bi_on = toggles & (1 << 1) != 0; // YaesuCtrl::BreakIn (CW-only)
            if ui.add_enabled(is_cw, toggle_btn("BK-IN", bi_on)).clicked() {
                let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, 1, if bi_on { 0 } else { 1 }));
                self.yaesu_toggle_flip(slot, 1);
            }
            let nar_on = toggles & (1 << 2) != 0; // YaesuCtrl::Narrow
            if ui.add(toggle_btn("NAR", nar_on)).clicked() {
                let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, 2, if nar_on { 0 } else { 1 }));
                self.yaesu_toggle_flip(slot, 2);
            }
            let dnf_on = toggles & (1 << 3) != 0; // YaesuCtrl::AutoNotch (DNF; niet in FM)
            if ui.add_enabled(!is_fm, toggle_btn("DNF", dnf_on)).clicked() {
                let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, 3, if dnf_on { 0 } else { 1 }));
                self.yaesu_toggle_flip(slot, 3);
            }
            // AGC cyclus (multi-state) - YaesuCtrl::Agc index 6. Geen blauw: er is altijd
            // één stand actief; het label toont de stand. Hardware-geverifieerd (§13):
            // 1=FAST/2=MID/3=SLOW/4=AUTO op beide radio's (AUTO leest terug als 4/5/6, door
            // de server naar 4 genormaliseerd). OFF (0) bewust weggelaten (remote riskant).
            let agc = levels[6];
            let agc_lbl = match agc { 1 => "FAST", 2 => "MID", 3 => "SLOW", 4 => "AUTO", _ => "AUTO" };
            // Cyclus FAST->MID->SLOW->AUTO->FAST; onbekende/0-stand start op FAST.
            let next_agc: u16 = if (1..4).contains(&agc) { (agc + 1) as u16 } else { 1 };
            if ui.add(egui::Button::new(RichText::new(format!("AGC:{}", agc_lbl)).size(11.0))
                .min_size(egui::vec2(74.0, 20.0))).clicked() {
                let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, 6, next_agc));
            }
            // Pre-amp/IPO cyclus (HF) - YaesuCtrl::PreAmp index 7. Label toont de stand.
            let ipo = levels[7];
            let ipo_lbl = match ipo { 0 => "IPO", 1 => "AMP1", _ => "AMP2" };
            if ui.add_enabled(hf_6m, egui::Button::new(RichText::new(ipo_lbl).size(11.0))
                .min_size(egui::vec2(52.0, 20.0))).clicked() {
                let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, 7, ((ipo + 1) % 3) as u16));
            }
        });
    }

    /// Collapsible level-slider block (Fase C): NB/DNR/Processor/AMC as sliders,
    /// per radio slot. Debounced via yaesu(2)_control_changed_at zodat een sleep niet
    /// elk frame door de radio-readback wordt teruggezet. Gedeeld voor slot 0/1.
    fn render_yaesu_levels_block(&mut self, ui: &mut egui::Ui, slot: u8) {
        let s = slot as usize;
        // AMC (AO) bestaat alleen op de FTX-1; de 991A kent geen AO CAT-commando
        // (geen readback -> slider zou terugspringen). Model-code 1 = FTX-1.
        let is_ftx1 = (if slot == 0 { self.yaesu_model } else { self.yaesu2_model }) == 1;
        // APF is CW-only (mode 3=CW, 4=CW-R); buiten CW uitgrijzen.
        let is_cw = matches!(if slot == 0 { self.yaesu_mode } else { self.yaesu2_mode }, 3 | 4);
        // NB/DNR/Contour/Notch zijn in FM (mode 5) niet bruikbaar -> uitgrijzen.
        let is_fm = matches!(if slot == 0 { self.yaesu_mode } else { self.yaesu2_mode }, 5 | 12);
        let toggles = if slot == 0 { self.yaesu_feature_toggles } else { self.yaesu2_feature_toggles };
        egui::CollapsingHeader::new(rust_i18n::t!("dev_dsp_levels").to_string())
            .id_salt(("yaesu_levels", slot))
            .show(ui, |ui| {
                // (level-ctrl, label, min, max, ftx1_only, on/off-ctrl voor 991A)
                // Proc (ctrl 10) verwijderd: de radio-speech-processor doet niets op
                // USB-audio (radio-shaping gebypassed op REAR/USB); client-side EQ/AGC
                // vervangen het.
                let specs: [(u8, &str, i32, i32, bool, Option<u8>); 3] = [
                    (8, "NB", 0, 10, false, Some(13)),   // 991A: NB aan/uit = ctrl 13
                    (9, "DNR", 0, 10, false, Some(14)),  // 991A: NR aan/uit = ctrl 14
                    (11, "AMC", 1, 100, true, None),
                ];
                for (j, &(ctrl, label, lo, hi, ftx1_only, toggle_ctrl)) in specs.iter().enumerate() {
                    if ftx1_only && !is_ftx1 { continue; }
                    // NB niet in FM (tester-bevestigd op hardware). DNR blijft: de 991A OM
                    // beperkt DNR-ruisreductie niet tot niet-FM. AMC (TX-audio) altijd bruikbaar.
                    let row_enabled = !(is_fm && label == "NB");
                    ui.add_enabled_ui(row_enabled, |ui| {
                    ui.horizontal(|ui| {
                        // 991A: aparte aan/uit-knop vóór de niveau-slider (FTX-1 codeert
                        // "uit" in het niveau zelf, dus daar geen toggle).
                        let has_toggle = toggle_ctrl.is_some() && !is_ftx1;
                        if let Some(tc) = toggle_ctrl {
                            if !is_ftx1 {
                                let on = toggles & (1 << tc) != 0;
                                let tbtn = if on {
                                    egui::Button::new(RichText::new(label).size(11.0).color(Color32::WHITE))
                                        .fill(Color32::from_rgb(0, 90, 200)).min_size(egui::vec2(46.0, 20.0))
                                } else {
                                    egui::Button::new(RichText::new(label).size(11.0)).min_size(egui::vec2(46.0, 20.0))
                                };
                                if ui.add(tbtn).clicked() {
                                    let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, tc, if on { 0 } else { 1 }));
                                    self.yaesu_toggle_flip(slot, tc as u32);
                                }
                            }
                        }
                        let sl_label = if has_toggle { "" } else { label };
                        let resp = ui.add(egui::Slider::new(&mut self.yaesu_level_sliders[s][j], lo..=hi).text(sl_label));
                        let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.yaesu_level_sliders[s][j], lo..=hi, ((hi - lo) as f64 / 50.0).max(1.0));
                        // Send alleen op drag-release (of een niet-sleep-wijziging: klik/toets),
                        // niet op elke tussenwaarde - anders burst een sleep CAT-commando's over
                        // de seriële Yaesu-link (deelt met de poll -> CAT-lag). Eindwaarde altijd.
                        if resp.drag_stopped() || (resp.changed() && !resp.dragged()) || scrolled {
                            let v = self.yaesu_level_sliders[s][j] as u16;
                            let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, ctrl, v));
                            if slot == 0 {
                                self.yaesu_control_changed_at = Some(Instant::now());
                            } else {
                                self.yaesu2_control_changed_at = Some(Instant::now());
                            }
                        }
                    });
                    });
                }
                // Fase D: Contour / APF / Manual-Notch - aan/uit-knop + frequentie-slider.
                let dspecs: [(usize, &str, u8, u8, i32, i32); 3] = [
                    (0, "Contour", 15, 18, 10, 3200),
                    (1, "APF", 16, 19, 0, 50),
                    (2, "Notch", 17, 20, 1, 320),
                ];
                for &(fidx, label, on_ctrl, freq_ctrl, flo, fhi) in dspecs.iter() {
                    let enabled = match label {
                        "APF" => is_cw,                    // APF alleen in CW (beide OM's)
                        "Contour" => !is_fm && !is_cw,     // Contour: niet in FM, niet in CW (FTX-1 OM)
                        _ => !is_fm,                       // Notch: niet in FM (manual notch werkt wel in CW)
                    };
                    ui.add_enabled_ui(enabled, |ui| {
                    ui.horizontal(|ui| {
                        let on = toggles & (1 << on_ctrl) != 0;
                        let tbtn = if on {
                            egui::Button::new(RichText::new(label).size(11.0).color(Color32::WHITE))
                                .fill(Color32::from_rgb(0, 90, 200)).min_size(egui::vec2(58.0, 20.0))
                        } else {
                            egui::Button::new(RichText::new(label).size(11.0)).min_size(egui::vec2(58.0, 20.0))
                        };
                        if ui.add(tbtn).clicked() {
                            let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, on_ctrl, if on { 0 } else { 1 }));
                            self.yaesu_toggle_flip(slot, on_ctrl as u32);
                        }
                        let resp = ui.add(egui::Slider::new(&mut self.yaesu_freq_sliders[s][fidx], flo..=fhi).text("Hz"));
                        let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.yaesu_freq_sliders[s][fidx], flo..=fhi, ((fhi - flo) as f64 / 50.0).max(1.0));
                        // Send op drag-release i.p.v. elke tussenwaarde - brede freq-sliders
                        // (Contour 10-3200, Notch 1-320) zouden anders tientallen CAT-commando's
                        // per sleep queuen. Eindwaarde altijd verzonden.
                        if resp.drag_stopped() || (resp.changed() && !resp.dragged()) || scrolled {
                            let v = self.yaesu_freq_sliders[s][fidx] as u16;
                            let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, freq_ctrl, v));
                            if slot == 0 {
                                self.yaesu_control_changed_at = Some(Instant::now());
                            } else {
                                self.yaesu2_control_changed_at = Some(Instant::now());
                            }
                        }
                    });
                    });
                }
            });
    }

    /// Clarifier-blok (§15): RIT/XIT aan/uit (blauw=aan, optimistisch), offset-display
    /// (freqs[3] als i16) + stapknoppen (±10/±100 Hz) en Clear. Gedeeld voor slot 0/1.
    /// Per-model verschil zit server-side (991A relatief RU/RD, FTX-1 absolute CF).
    fn render_yaesu_clarifier_block(&mut self, ui: &mut egui::Ui, slot: u8) {
        let toggles = if slot == 0 { self.yaesu_feature_toggles } else { self.yaesu2_feature_toggles };
        let offset = if slot == 0 { self.yaesu_clar_offset } else { self.yaesu2_clar_offset }; // signed Hz
        let toggle_btn = |label: &str, on: bool| {
            if on {
                egui::Button::new(RichText::new(label).size(11.0).color(Color32::WHITE))
                    .fill(Color32::from_rgb(0, 90, 200)).min_size(egui::vec2(40.0, 20.0))
            } else {
                egui::Button::new(RichText::new(label).size(11.0)).min_size(egui::vec2(40.0, 20.0))
            }
        };
        let step_btn = |label: &str| egui::Button::new(RichText::new(label).size(11.0))
            .min_size(egui::vec2(40.0, 20.0));
        ui.horizontal(|ui| {
            ui.label("Clar:");
            let rit_on = toggles & (1 << 21) != 0; // YaesuCtrl::RitOn
            if ui.add(toggle_btn("RIT", rit_on)).clicked() {
                let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, 21, if rit_on { 0 } else { 1 }));
                self.yaesu_toggle_flip(slot, 21);
            }
            let xit_on = toggles & (1 << 22) != 0; // YaesuCtrl::XitOn
            if ui.add(toggle_btn("XIT", xit_on)).clicked() {
                let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, 22, if xit_on { 0 } else { 1 }));
                self.yaesu_toggle_flip(slot, 22);
            }
            // Offset-display (signed): oranje alleen als RIT/XIT AAN staat én ≠0; anders
            // grijs. De 991A kan een opgeslagen P3-offset teruggeven terwijl de clarifier
            // uit is - die is dan niet actief, dus niet als actieve offset tonen.
            let clar_active = rit_on || xit_on;
            let (txt, col) = if clar_active && offset != 0 {
                (format!("{:+05} Hz", offset), Color32::from_rgb(255, 170, 40))
            } else {
                (" +0000 Hz".to_string(), Color32::from_rgb(120, 120, 120))
            };
            ui.label(RichText::new(txt).size(11.0).family(egui::FontFamily::Monospace).color(col));
        });
        ui.horizontal(|ui| {
            // Stapknoppen: value = i16-as-u16 signed stap (YaesuCtrl::ClarStep = 24).
            for &step in &[-100i16, -10] {
                if ui.add(step_btn(&format!("{}", step))).clicked() {
                    let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, 24, step as u16));
                }
            }
            if ui.add(egui::Button::new(RichText::new("Clr").size(11.0))
                .min_size(egui::vec2(40.0, 20.0))).clicked() {
                let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, 23, 0)); // ClarClear
            }
            for &step in &[10i16, 100] {
                if ui.add(step_btn(&format!("+{}", step))).clicked() {
                    let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, 24, step as u16));
                }
            }
        });
    }

    pub(super) fn render_yaesu_popout(&mut self, ui: &mut egui::Ui) {
        let mode_label = match self.yaesu_mode {
            0 => "LSB", 1 => "USB", 3 => "CW-L", 4 => "CW-U",
            5 => "FM", 6 => "AM", 7 => "DIGU", 9 => "DIGL", 12 => "C4FM",
            _ => "?",
        };

        // VFO A / VFO B / Memory selection
        ui.horizontal(|ui| {
            let btn_size = egui::vec2(44.0, 24.0);
            if ui.add(egui::Button::new(RichText::new("A/B").strong()).min_size(btn_size)).clicked() {
                let _ = self.cmd_tx.send(Command::SetControl(
                    sdr_remote_core::protocol::ControlId::YaesuSelectVfo, 2)); // SV;
            }
            if ui.add(egui::Button::new(RichText::new("V/M").strong()).min_size(btn_size)).clicked() {
                let _ = self.cmd_tx.send(Command::SetControl(
                    sdr_remote_core::protocol::ControlId::YaesuSelectVfo, 3)); // VM;
            }
            ui.separator();
            ui.label(RichText::new(mode_label).size(14.0).color(Color32::from_rgb(255, 170, 40)));
        });

        // Mode indicator: VFO / Memory
        if self.yaesu_in_memory_mode {
            if let Some(idx) = self.yaesu_current_mem_ch {
                if let Some(ch) = self.yaesu_mem_channels.get(idx) {
                    let c = Color32::from_rgb(100, 200, 255);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("MEM {:02}", ch.channel_number))
                            .size(14.0).strong().color(c));
                        ui.label(RichText::new(&ch.name).size(14.0).strong().color(c));
                        ui.label(RichText::new(super::yaesu_memory::format_freq_display(ch.rx_freq_hz))
                            .size(14.0).family(egui::FontFamily::Monospace).color(c));
                    });
                }
            }
        } else {
            let label = if self.yaesu_split_active { "VFO  Split" } else { "VFO" };
            let c = if self.yaesu_split_active { Color32::from_rgb(255, 180, 50) } else { Color32::from_rgb(100, 255, 100) };
            ui.label(RichText::new(label).size(14.0).strong().color(c));
        }

        // Frequency display with scroll/tap-to-tune + touch-vriendelijke stapper (§16)
        ui.horizontal(|ui| {
            ui.label(RichText::new("A:  ").size(16.0).strong());
            if let Some(delta) = render_freq_scroll(ui, self.yaesu_freq_a) {
                let new_freq = (self.yaesu_freq_a as i64 + delta).max(0) as u64;
                let _ = self.cmd_tx.send(Command::SetYaesuFreq(new_freq));
                self.set_pending_yaesu_freq(0, new_freq);
            }
        });
        if let Some(delta) = render_freq_stepper(ui, &mut self.tune_step_hz) {
            let new_freq = (self.yaesu_freq_a as i64 + delta).max(0) as u64;
            let _ = self.cmd_tx.send(Command::SetYaesuFreq(new_freq));
            self.set_pending_yaesu_freq(0, new_freq);
        }

        ui.horizontal(|ui| {
            ui.label(RichText::new("B:  ").size(12.0));
            ui.label(RichText::new(format!("{} Hz", format_frequency(self.yaesu_freq_b)))
                .size(14.0).family(egui::FontFamily::Monospace));
        });

        ui.separator();

        // Mode + Band + controls row
        {
            use sdr_remote_core::protocol::ControlId;
            let btn = |text: &str| egui::Button::new(RichText::new(text).size(11.0))
                .min_size(egui::vec2(38.0, 20.0));
            let mode_names = ["LSB","USB","CW-L","CW-U","FM","AM","DIG-U","DIG-L","RTTY","C4FM","DATA-FM","DATA-USB"];
            let mode_codes: &[u8] = &[0, 1, 3, 4, 5, 6, 7, 9, 9, 5, 5, 7];

            // Mode buttons
            ui.horizontal(|ui| {
                ui.label(rust_i18n::t!("dev_mode").to_string());
                for (i, &name) in mode_names.iter().enumerate().take(8) {
                    let mb = if mode_codes[i] == self.yaesu_mode {
                        egui::Button::new(RichText::new(name).size(11.0).color(Color32::WHITE))
                            .fill(Color32::from_rgb(0, 90, 200))
                    } else { btn(name) };
                    if ui.add(mb).clicked() {
                        let _ = self.cmd_tx.send(Command::SetYaesuMode(mode_codes[i]));
                    }
                }
            });

            // Band + A=B + Split + Scan + Tune
            ui.horizontal(|ui| {
                if ui.add(btn("Band-")).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuButton, 6));
                }
                if ui.add(btn("Band+")).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuButton, 5));
                }
                ui.separator();
                if ui.add(btn("Mem-")).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuButton, 10));
                }
                if ui.add(btn("Mem+")).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuButton, 9));
                }
                ui.separator();
                if ui.add(btn("A=B")).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuButton, 0));
                }
                let split_lbl = rust_i18n::t!("dev_split").to_string();
                let split_btn = if self.yaesu_split_active {
                    egui::Button::new(RichText::new(split_lbl.as_str()).size(11.0).color(Color32::WHITE))
                        .fill(Color32::from_rgb(180, 100, 0)).min_size(egui::vec2(38.0, 20.0))
                } else { btn(split_lbl.as_str()) };
                if ui.add(split_btn).clicked() {
                    self.yaesu_split_active = !self.yaesu_split_active;
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuButton,
                        if self.yaesu_split_active { 7 } else { 8 }));
                }
                let scan_lbl = rust_i18n::t!("dev_scan").to_string();
                let scan_btn = if self.yaesu_scan_active {
                    egui::Button::new(RichText::new(scan_lbl.as_str()).size(11.0).color(Color32::WHITE))
                        .fill(Color32::from_rgb(0, 120, 0)).min_size(egui::vec2(38.0, 20.0))
                } else { btn(scan_lbl.as_str()) };
                if ui.add(scan_btn).clicked() {
                    self.yaesu_scan_active = !self.yaesu_scan_active;
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuButton,
                        if self.yaesu_scan_active { 1 } else { 2 }));
                }
                // Interne ATU: momentane Tune (band-gated: HF+6m, <54 MHz) + aan/uit-toggle
                // die de echte radio-stand toont (via AC;-poll). PATCH-yaesu-internal-atu.
                // ATU: HF+6m (<54 MHz) en niet in FM (mode 5).
                let atu_avail = self.yaesu_freq_a >= 1 && self.yaesu_freq_a < 54_000_000 && !matches!(self.yaesu_mode, 5 | 12);
                let can_tune = self.yaesu_connected && atu_avail;
                let tune_lbl = rust_i18n::t!("dev_tune").to_string();
                let tune_btn = if self.yaesu_tuner_state == 2 {
                    // Actief aan het tunen -> rode voortgangsindicatie.
                    egui::Button::new(RichText::new(tune_lbl.as_str()).size(11.0).color(Color32::WHITE))
                        .fill(Color32::from_rgb(180, 0, 0)).min_size(egui::vec2(38.0, 20.0))
                } else { btn(tune_lbl.as_str()) };
                if ui.add_enabled(can_tune, tune_btn).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuButton, 3)); // start tuning (momentaan)
                }
                let atu_on = self.yaesu_tuner_state == 1;
                let atu_btn = if atu_on {
                    egui::Button::new(RichText::new("ATU").size(11.0).color(Color32::WHITE))
                        .fill(Color32::from_rgb(0, 90, 200)).min_size(egui::vec2(38.0, 20.0))
                } else { btn("ATU") };
                if ui.add_enabled(atu_avail, atu_btn).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuButton,
                        if atu_on { 4 } else { 15 })); // toggle: uit (AC000) / aan (AC001)
                }
            });
            // DSP/functie-controls (PATCH-yaesu-extra-controls) - radio 1.
            let dsp0 = self.yaesu_feature_toggles;
            let lvl0 = self.yaesu_feature_levels;
            self.render_yaesu_dsp_block(ui, 0, dsp0, lvl0);
            self.render_yaesu_clarifier_block(ui, 0);
            self.render_yaesu_levels_block(ui, 0);

            // Sliders: aligned grid layout
            let label_w = 55.0;
            let slider_w = 120.0;
            egui::Grid::new("yaesu_sliders").num_columns(4).spacing([4.0, 2.0]).show(ui, |ui| {
                ui.allocate_space(egui::vec2(label_w, 0.0));
                ui.label("SQL");
                let sql_slider = egui::Slider::new(&mut self.yaesu_squelch, 0..=100)
                    .custom_formatter(|v, _| format!("{:.0}", v));
                let resp = ui.add_sized([slider_w, 16.0], sql_slider);
                let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.yaesu_squelch, 0..=100, 1.0);
                if resp.changed() || scrolled {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuSquelch, self.yaesu_squelch));
                    self.yaesu_control_changed_at = Some(Instant::now());
                }

                ui.label("PWR");
                // Sliderbereik 5..=max voor de huidige band (uit EX137-140); 0/onbekend -> 100.
                let pwr_max = if self.yaesu_tx_power_max >= 5 { self.yaesu_tx_power_max } else { 100 };
                if self.yaesu_rf_power > pwr_max { self.yaesu_rf_power = pwr_max; }
                let pwr_slider = egui::Slider::new(&mut self.yaesu_rf_power, 5..=pwr_max)
                    .custom_formatter(|v, _| format!("{:.0}W", v));
                let resp = ui.add_sized([slider_w, 16.0], pwr_slider)
                    .on_hover_text(rust_i18n::t!("dev_max_for_band", w = pwr_max).to_string());
                let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.yaesu_rf_power, 5..=pwr_max, 1.0);
                if resp.changed() || scrolled {
                    // Blokkeer de readback-sync zolang je aan het schuiven bent, zodat de
                    // (tragere) radio-terugmelding de slider niet heen-en-weer laat bouncen.
                    self.yaesu_control_changed_at = Some(Instant::now());
                }
                // Stuur pas bij LOSLATEN (of klik/scroll), niet elke frame tijdens het slepen.
                if scrolled || resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuRfPower, self.yaesu_rf_power as u16));
                    self.yaesu_power_pending = Some(self.yaesu_rf_power);
                    self.yaesu_power_pending_at = Some(std::time::Instant::now());
                }
                ui.end_row();

                ui.allocate_space(egui::vec2(label_w, 0.0));
                ui.label(rust_i18n::t!("dev_rf_gain").to_string());
                let rf_slider = egui::Slider::new(&mut self.yaesu_rf_gain, 0..=255)
                    .custom_formatter(|v, _| format!("{:.0}", v));
                let resp = ui.add_sized([slider_w, 16.0], rf_slider);
                let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.yaesu_rf_gain, 0..=255, 2.0);
                if resp.changed() || scrolled {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuRfGain, self.yaesu_rf_gain));
                    self.yaesu_control_changed_at = Some(Instant::now());
                }
                ui.end_row();
            });
        }

        ui.separator();

        // S-meter (klik wisselt balk <-> analoog; analoog op dezelfde plek als de balk).
        let mw = ui.available_width().min(350.0).max(200.0);
        let mrect = if self.meter_analog[super::M_YAESU1] {
            smeter_analog_sized(ui,
                yaesu_raw_to_dbm(self.yaesu_smeter), yaesu_raw_to_dbm(self.yaesu_smeter_peak),
                false, false, Some((mw, 110.0)))
        } else {
            yaesu_smeter_bar(ui, self.yaesu_smeter, self.yaesu_smeter_peak)
        };
        self.meter_click(ui, mrect, super::M_YAESU1);

        ui.separator();

        // Status row
        ui.horizontal(|ui| {
            let (tx_color, tx_text) = if self.yaesu_tx_active {
                (Color32::from_rgb(220, 40, 40), "TX")
            } else {
                (Color32::from_rgb(0, 150, 0), "RX")
            };
            ui.colored_label(tx_color, RichText::new(tx_text).size(16.0).strong());
            ui.separator();
            // Radio-aan/uit via CAT PS. ALLEEN veilig als klikbare knop op de 991A
            // (code 0): daar is PS0 = standby, CAT/USB blijft leven -> remote weer
            // aan. De FTX-1 (en onbekende types) gaan bij PS0 ECHT uit incl. USB ->
            // niet meer remote aan te zetten. Daarom voor die alleen een status-label.
            let on_col = Color32::from_rgb(0, 150, 0);
            let off_col = Color32::from_rgb(90, 90, 90);
            if self.yaesu_model == 0 {
                let (txt, col): (String, Color32) = if self.yaesu_power_on { (rust_i18n::t!("dev_power_on_upper").to_string(), on_col) } else { (rust_i18n::t!("dev_standby").to_string(), off_col) };
                if ui.add(egui::Button::new(RichText::new(txt).color(Color32::WHITE)).fill(col))
                    .on_hover_text(rust_i18n::t!("dev_991a_standby_hover").to_string())
                    .clicked()
                {
                    let _ = self.cmd_tx.send(Command::SetControl(
                        sdr_remote_core::protocol::ControlId::YaesuPowerOnOff,
                        if self.yaesu_power_on { 0 } else { 1 }));
                }
            } else {
                ui.colored_label(if self.yaesu_power_on { on_col } else { off_col },
                    if self.yaesu_power_on { rust_i18n::t!("dev_power_on_upper").to_string() } else { rust_i18n::t!("dev_power_off_upper").to_string() })
                    .on_hover_text(rust_i18n::t!("dev_radio_powers_off_hover").to_string());
            }
            if self.yaesu_hi_swr {
                ui.separator();
                ui.colored_label(theme::TL_SWR_ALERT_TEXT,
                    RichText::new(rust_i18n::t!("dev_high_swr").to_string()).size(16.0).strong());
            }
        });

        ui.separator();
        self.render_websdr_controls(ui, CatSyncTarget::Yaesu1, self.yaesu_freq_a, self.yaesu_mode);

        ui.separator();

        // Mic gain slider for Yaesu USB TX audio. Display 0.5 maps
        // to the empirically matched internal gain 0.2.
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("dev_mic_gain").to_string());
            let mut mic_gain_display = super::yaesu_mic_gain_to_display(self.yaesu_mic_gain);
            let slider = egui::Slider::new(&mut mic_gain_display, 0.05..=1.0)
                .logarithmic(true)
                .custom_formatter(|v, _| format!("{:.2}x", v));
            let resp = ui.add_sized([140.0, 16.0], slider);
            let scrolled = super::helpers::slider_wheel(ui, &resp, &mut mic_gain_display, 0.05..=1.0, 0.02);
            if resp.changed() || scrolled {
                self.yaesu_mic_gain = super::yaesu_mic_gain_from_display(mic_gain_display);
                let _ = self.cmd_tx.send(Command::SetYaesuTxGain(self.yaesu_mic_gain));
            }
            if resp.drag_stopped() || scrolled {
                self.save_ptt_config();
            }
        });

        ui.separator();

        // 5-band Equalizer - gedeelde generieke component (slot 0).
        self.render_yaesu_eq(ui, 0);

        ui.separator();

        // Memory channels - visible body height is user-resizable so it
        // doesn't push the Radio Settings header off the bottom of the window.
        if super::helpers::chevron_label(
            ui,
            self.collapse_yaesu_memories,
            RichText::new(rust_i18n::t!("dev_memory_channels").to_string()).strong().size(14.0),
        )
        .clicked()
        {
            self.collapse_yaesu_memories = !self.collapse_yaesu_memories;
            self.save_full_config();
        }
        if self.collapse_yaesu_memories {
            let mem_max_h = self.yaesu_memories_h;
            ui.indent("yaesu_memories_body", |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("yaesu_memories_scroll")
                    .max_height(mem_max_h)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.render_yaesu_memories(ui);
                    });
            });
        }
        // Drag-handle below memories - only visible when memories expanded
        // so it can resize the visible portion of the channel list.
        if self.collapse_yaesu_memories {
            let handle_h = 6.0;
            let (rect, response) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), handle_h),
                egui::Sense::drag(),
            );
            let visuals = ui.visuals();
            let fill = if response.hovered() || response.dragged() {
                visuals.widgets.hovered.bg_fill
            } else {
                visuals.widgets.inactive.bg_fill
            };
            ui.painter().rect_filled(rect, 2.0, fill);
            let grip_color = visuals.widgets.inactive.fg_stroke.color;
            let cx = rect.center().x;
            let cy = rect.center().y;
            for dx in [-12.0, 0.0, 12.0] {
                ui.painter().line_segment(
                    [egui::pos2(cx + dx - 4.0, cy), egui::pos2(cx + dx + 4.0, cy)],
                    egui::Stroke::new(1.0, grip_color),
                );
            }
            if response.dragged() {
                let dy = response.drag_delta().y;
                if dy.abs() > 0.01 {
                    self.yaesu_memories_h = (self.yaesu_memories_h + dy).clamp(100.0, 800.0);
                }
            }
            if response.drag_stopped() {
                self.save_full_config();
            }
            if response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
            }
        }

        if super::helpers::chevron_label(
            ui,
            self.collapse_yaesu_menu,
            RichText::new(rust_i18n::t!("dev_radio_settings").to_string()).strong().size(14.0),
        )
        .clicked()
        {
            self.collapse_yaesu_menu = !self.collapse_yaesu_menu;
            self.save_full_config();
        }
        if self.collapse_yaesu_menu {
            ui.indent("yaesu_menu_body", |ui| {
                self.render_yaesu_menu(ui);
            });
        }
    }

    /// Paneel-/venster-naam voor een radio-slot (PATCH-dual-radio-991a-ftx1).
    /// Window-titel per radio-slot: "ThetisLink - Yaesu {N}: {type}" — consistent
    /// met de server-tab en de detail-tab (via yaesu_slot_label). Type uit RadioInfo.
    pub(super) fn yaesu_window_title(&self, slot: u8) -> String {
        format!("ThetisLink - {}", self.yaesu_slot_label(slot))
    }

    /// Puur het server-gerapporteerde radio-type per slot (het RadioInfo-model-byte),
    /// zonder slot-suffix. Naam komt uit de canonieke core-tabel (`radio_model_name`)
    /// zodat een nieuw radiotype vanzelf meekomt. Voor de "Yaesu N: <type>"-weergave.
    pub(super) fn yaesu_type_name(&self, slot: u8) -> &'static str {
        let code = if slot == 0 { self.yaesu_model } else { self.yaesu2_model };
        sdr_remote_core::protocol::radio_model_name(code)
    }

    /// Consistente slot-weergave: "Yaesu 1: 991A" / "Yaesu 2: FTX1". Slotnummer +
    /// het actieve type uit de server-config (RadioInfo). Overal in de server-tab.
    pub(super) fn yaesu_slot_label(&self, slot: u8) -> String {
        format!("Yaesu {}: {}", slot + 1, self.yaesu_type_name(slot))
    }

    /// Naam voor een packet-type in de bitstream/dataverbruik-uitsplitsing. Voor de
    /// Yaesu-slot-gebonden types injecteert dit het consistente "Yaesu N: <type>"-
    /// label (zelfde als levels/streams/recording); de rest valt terug op de
    /// statische packet-type-tabel.
    pub(super) fn bitstream_label(&self, t: u8) -> String {
        match t {
            0x16 => { let label = self.yaesu_slot_label(0); rust_i18n::t!("dev_audio_fmt", label = label).to_string() }
            0x17 => { let label = self.yaesu_slot_label(0); rust_i18n::t!("dev_state_fmt", label = label).to_string() }
            0x19 => { let label = self.yaesu_slot_label(0); rust_i18n::t!("dev_memory_data_fmt", label = label).to_string() }
            0x25 => { let label = self.yaesu_slot_label(1); rust_i18n::t!("dev_audio_fmt", label = label).to_string() }
            0x26 => { let label = self.yaesu_slot_label(1); rust_i18n::t!("dev_state_fmt", label = label).to_string() }
            _ => super::screens::packet_type_label(t),
        }
    }

    /// Gefocust slot-1 radio-paneel (dual-radio fase 1): VFO-A freq + tune, mode,
    /// PTT, S-meter, volume. Memory/EX-menu = fase 2. Leest self.yaesu2_* (gesynct
    /// uit state) en stuurt SetYaesu2*-commando's.
    /// Generieke 5-band EQ-sectie (chevron + profielen opslaan/verwijderen +
    /// waarde per slider), GEDEELD door beide Yaesu-windows (slot 0 = 991A-window,
    /// slot 1 = FTX-1-window) - parity by construction, niet per-window naschilderen.
    /// Snapshot->writeback houdt de borrow-checker tevreden bij per-slot state +
    /// per-slot commando's (SetYaesu*/SetYaesu2*).
    pub(super) fn render_yaesu_eq(&mut self, ui: &mut egui::Ui, slot: u8) {
        let mut collapse = if slot == 0 { self.collapse_yaesu_eq } else { self.collapse_yaesu2_eq };
        if super::helpers::chevron_label(ui, collapse,
            RichText::new(rust_i18n::t!("dev_equalizer").to_string()).strong().size(14.0)).clicked()
        {
            collapse = !collapse;
            if slot == 0 { self.collapse_yaesu_eq = collapse; } else { self.collapse_yaesu2_eq = collapse; }
            self.save_full_config();
        }
        if !collapse { return; }

        // Snapshot per-slot state in locals (writeback onderaan).
        let mut enabled = if slot == 0 { self.yaesu_eq_enabled } else { self.yaesu2_eq_enabled };
        let mut gains = if slot == 0 { self.yaesu_eq_gains } else { self.yaesu2_eq_gains };
        let mut mic_gain = if slot == 0 { self.yaesu_mic_gain } else { self.yaesu2_mic_gain };
        let mut profiles = if slot == 0 { self.yaesu_eq_profiles.clone() } else { self.yaesu2_eq_profiles.clone() };
        let mut active = if slot == 0 { self.yaesu_eq_active_profile.clone() } else { self.yaesu2_eq_active_profile.clone() };
        let mut new_name = if slot == 0 { self.yaesu_eq_new_name.clone() } else { self.yaesu2_eq_new_name.clone() };
        let tx = self.cmd_tx.clone();
        let mut dirty = false;
        let mk_en = |on: bool| if slot == 0 { Command::SetYaesuEqEnabled(on) } else { Command::SetYaesu2EqEnabled(on) };
        let mk_band = |b: u8, g: f32| if slot == 0 { Command::SetYaesuEqBand(b, g) } else { Command::SetYaesu2EqBand(b, g) };
        let mk_gain = |g: f32| if slot == 0 { Command::SetYaesuTxGain(g) } else { Command::SetYaesu2TxGain(g) };
        // Client-side TX-keten per radio: compressor (0-100) + AGC-toggle.
        let mut comp = if slot == 0 { self.yaesu_compressor } else { self.yaesu2_compressor };
        let mut agc = if slot == 0 { self.yaesu_tx_agc } else { self.yaesu2_tx_agc };
        let mut chain_dirty = false;
        let mk_comp = |v: u8| if slot == 0 { Command::SetYaesuCompressor(v) } else { Command::SetYaesu2Compressor(v) };
        let mk_agc = |on: bool| if slot == 0 { Command::SetYaesuTxAgc(on) } else { Command::SetYaesu2TxAgc(on) };

        ui.indent(("yaesu_eq_body", slot), |ui| {
            ui.horizontal(|ui| {
                if ui.checkbox(&mut enabled, "EQ").changed() {
                    let _ = tx.send(mk_en(enabled));
                }
                let names: Vec<String> = profiles.iter().map(|(n, _, _, _)| n.clone()).collect();
                egui::ComboBox::from_id_salt(("eq_profile", slot))
                    .selected_text(if active.is_empty() { "---" } else { active.as_str() })
                    .width(100.0)
                    .show_ui(ui, |ui| {
                        for name in &names {
                            if ui.selectable_label(&active == name, name).clicked() {
                                active = name.clone();
                                if let Some((_, en, g, mg)) = profiles.iter().find(|(n, _, _, _)| n == name) {
                                    enabled = *en; gains = *g; mic_gain = *mg;
                                    let _ = tx.send(mk_en(*en));
                                    for i in 0..5 { let _ = tx.send(mk_band(i as u8, g[i])); }
                                    let _ = tx.send(mk_gain(*mg));
                                }
                                dirty = true;
                            }
                        }
                    });
                if ui.small_button(rust_i18n::t!("dev_save").to_string()).clicked() && !active.is_empty() {
                    let name = active.clone();
                    if let Some(p) = profiles.iter_mut().find(|(n, _, _, _)| *n == name) {
                        p.1 = enabled; p.2 = gains; p.3 = mic_gain;
                    } else {
                        profiles.push((name, enabled, gains, mic_gain));
                    }
                    dirty = true;
                }
                if ui.small_button("Del").clicked() && !active.is_empty() {
                    profiles.retain(|(n, _, _, _)| n != &active);
                    active.clear();
                    dirty = true;
                }
            });
            ui.horizontal(|ui| {
                ui.label(rust_i18n::t!("dev_new").to_string());
                ui.add(egui::TextEdit::singleline(&mut new_name).desired_width(100.0));
                if ui.small_button("+").clicked() && !new_name.is_empty() {
                    let name = new_name.clone();
                    profiles.push((name.clone(), enabled, gains, mic_gain));
                    active = name;
                    new_name.clear();
                    dirty = true;
                }
            });
            ui.horizontal(|ui| {
                for i in 0..5 {
                    ui.vertical(|ui| {
                        ui.set_width(50.0);
                        ui.label(RichText::new(sdr_remote_logic::eq::BAND_LABELS[i]).size(10.0));
                        let mut g = gains[i];
                        let slider = egui::Slider::new(&mut g, -12.0..=12.0)
                            .vertical()
                            .custom_formatter(|v, _| format!("{:+.0}", v));
                        let resp = ui.add_sized([20.0, 60.0], slider);
                        let scrolled = super::helpers::slider_wheel(ui, &resp, &mut g, -12.0..=12.0, 0.5);
                        if resp.changed() || scrolled {
                            gains[i] = g;
                            let _ = tx.send(mk_band(i as u8, g));
                        }
                    });
                }
            });
            // Client-side TX-keten (radio-processing werkt niet op USB): AGC + compressor.
            ui.horizontal(|ui| {
                if ui.checkbox(&mut agc, "AGC").changed() {
                    let _ = tx.send(mk_agc(agc));
                    chain_dirty = true;
                }
                ui.label("Comp");
                let mut c = comp as f32;
                let resp = ui.add(egui::Slider::new(&mut c, 0.0..=100.0)
                    .custom_formatter(|v, _| format!("{:.0}", v)));
                let scrolled = super::helpers::slider_wheel(ui, &resp, &mut c, 0.0..=100.0, 2.0);
                if resp.changed() || scrolled {
                    comp = c.round() as u8;
                    let _ = tx.send(mk_comp(comp));
                }
                if resp.drag_stopped() || scrolled { chain_dirty = true; }
            });
        });

        // Writeback naar de slot-state.
        if slot == 0 {
            self.yaesu_eq_enabled = enabled; self.yaesu_eq_gains = gains; self.yaesu_mic_gain = mic_gain;
            self.yaesu_eq_profiles = profiles; self.yaesu_eq_active_profile = active; self.yaesu_eq_new_name = new_name;
            self.yaesu_compressor = comp; self.yaesu_tx_agc = agc;
        } else {
            self.yaesu2_eq_enabled = enabled; self.yaesu2_eq_gains = gains; self.yaesu2_mic_gain = mic_gain;
            self.yaesu2_eq_profiles = profiles; self.yaesu2_eq_active_profile = active; self.yaesu2_eq_new_name = new_name;
            self.yaesu2_compressor = comp; self.yaesu2_tx_agc = agc;
        }
        if dirty { self.save_full_config(); }
        if chain_dirty { self.save_ptt_config(); } // comp/AGC volgen het mic-gain-append-patroon
    }

    /// Geheugen-tabel per radio-slot. Slot 0 = direct. Slot 1: wissel de slot-1
    /// state via mem::swap in de (gedeelde) slot-0 velden, render dezelfde tabel,
    /// en wissel terug - zo blijft de werkende 991A-tabel ongewijzigd en is de UI
    /// gedeeld. De read/write-commando's volgen `yaesu_mem_active_slot`.
    pub(super) fn render_yaesu_memories_slot(&mut self, ui: &mut egui::Ui, slot: u8) {
        if slot == 0 {
            self.render_yaesu_memories(ui);
            return;
        }
        let swap_in_out = |s: &mut Self| {
            std::mem::swap(&mut s.yaesu_mem_channels, &mut s.yaesu2_mem_channels);
            std::mem::swap(&mut s.yaesu_mem_file, &mut s.yaesu2_mem_file);
            std::mem::swap(&mut s.yaesu_mem_selected, &mut s.yaesu2_mem_selected);
            std::mem::swap(&mut s.yaesu_mem_filter, &mut s.yaesu2_mem_filter);
            std::mem::swap(&mut s.yaesu_mem_dirty, &mut s.yaesu2_mem_dirty);
            std::mem::swap(&mut s.yaesu_mem_radio_received, &mut s.yaesu2_mem_radio_received);
        };
        swap_in_out(self);
        self.yaesu_mem_active_slot = 1;
        self.render_yaesu_memories(ui);
        self.yaesu_mem_active_slot = 0;
        swap_in_out(self);
    }

    pub(super) fn render_yaesu2_panel(&mut self, ui: &mut egui::Ui) {
        let mode_label = match self.yaesu2_mode {
            0 => "LSB", 1 => "USB", 3 => "CW-L", 4 => "CW-U",
            5 => "FM", 6 => "AM", 7 => "DIGU", 9 => "DIGL", 12 => "C4FM", _ => "?",
        };
        // A/B + V/M bovenaan (zelfde volgorde als 991A-paneel) + mode-label rechts.
        ui.horizontal(|ui| {
            if ui.add(egui::Button::new(RichText::new("A/B").strong().size(12.0))
                .min_size(egui::vec2(50.0, 22.0))).clicked()
            {
                let _ = self.cmd_tx.send(Command::SetControl(
                    sdr_remote_core::protocol::ControlId::Yaesu2SelectVfo, 2));
            }
            if ui.add(egui::Button::new(RichText::new("V/M").strong().size(12.0))
                .min_size(egui::vec2(50.0, 22.0))).clicked()
            {
                let _ = self.cmd_tx.send(Command::SetControl(
                    sdr_remote_core::protocol::ControlId::Yaesu2SelectVfo, 3));
            }
            ui.separator();
            ui.label(RichText::new(mode_label).size(14.0).color(Color32::from_rgb(255, 170, 40)));
        });
        // VFO / Memory-indicator (blauw bij Memory) - zelfde plek + naam/freq als 991A.
        if self.yaesu2_vfo_select == 1 {
            let c = Color32::from_rgb(100, 200, 255);
            let found = self.yaesu2_mem_channels.iter()
                .find(|ch| ch.channel_number == self.yaesu2_memory_channel);
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("MEM {:02}", self.yaesu2_memory_channel))
                    .size(14.0).strong().color(c));
                if let Some(ch) = found {
                    ui.label(RichText::new(&ch.name).size(14.0).strong().color(c));
                    ui.label(RichText::new(super::yaesu_memory::format_freq_display(ch.rx_freq_hz))
                        .size(14.0).family(egui::FontFamily::Monospace).color(c));
                }
            });
        } else {
            let (label, c) = if self.yaesu2_split {
                ("VFO  Split", Color32::from_rgb(255, 180, 50))
            } else {
                ("VFO", Color32::from_rgb(100, 255, 100))
            };
            ui.label(RichText::new(label).size(14.0).strong().color(c));
        }
        // A: frequentie (scroll/tik-to-tune) + touch-vriendelijke stapper (§16), daaronder B:.
        ui.horizontal(|ui| {
            ui.label(RichText::new("A:  ").size(16.0).strong());
            if let Some(delta) = render_freq_scroll(ui, self.yaesu2_freq_a) {
                let new_freq = (self.yaesu2_freq_a as i64 + delta).max(0) as u64;
                let _ = self.cmd_tx.send(Command::SetYaesu2Freq(new_freq));
                self.set_pending_yaesu_freq(1, new_freq);
            }
        });
        if let Some(delta) = render_freq_stepper(ui, &mut self.tune_step_hz) {
            let new_freq = (self.yaesu2_freq_a as i64 + delta).max(0) as u64;
            let _ = self.cmd_tx.send(Command::SetYaesu2Freq(new_freq));
            self.set_pending_yaesu_freq(1, new_freq);
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new("B:  ").size(12.0));
            ui.label(RichText::new(format!("{} Hz", format_frequency(self.yaesu2_freq_b)))
                .size(14.0).family(egui::FontFamily::Monospace));
        });
        ui.separator();
        {
            let btn = |text: &str| egui::Button::new(RichText::new(text).size(11.0))
                .min_size(egui::vec2(38.0, 20.0));
            let mode_names = ["LSB", "USB", "CW-L", "CW-U", "FM", "AM", "DIG-U", "DIG-L"];
            let mode_codes: &[u8] = &[0, 1, 3, 4, 5, 6, 7, 9];
            ui.horizontal_wrapped(|ui| {
                ui.label(rust_i18n::t!("dev_mode").to_string());
                for (i, &name) in mode_names.iter().enumerate() {
                    let mb = if mode_codes[i] == self.yaesu2_mode {
                        egui::Button::new(RichText::new(name).size(11.0).color(Color32::WHITE))
                            .fill(Color32::from_rgb(0, 90, 200))
                    } else { btn(name) };
                    if ui.add(mb).clicked() {
                        let _ = self.cmd_tx.send(Command::SetYaesu2Mode(mode_codes[i]));
                    }
                }
            });
        }
        // Band / A=B / Split / Scan / Tune - spiegel van het 991A-paneel, geroute
        // naar slot 1 (Yaesu2Button). Mem± en V/M = fase 2 (geheugen).
        {
            use sdr_remote_core::protocol::ControlId;
            let btn = |text: &str| egui::Button::new(RichText::new(text).size(11.0))
                .min_size(egui::vec2(38.0, 20.0));
            ui.horizontal(|ui| {
                if ui.add(btn("Band-")).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button, 6));
                }
                if ui.add(btn("Band+")).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button, 5));
                }
                ui.separator();
                if ui.add(btn("Mem-")).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button, 10));
                }
                if ui.add(btn("Mem+")).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button, 9));
                }
                ui.separator();
                if ui.add(btn("A=B")).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button, 0));
                }
                let split_lbl = rust_i18n::t!("dev_split").to_string();
                let split_btn = if self.yaesu2_split {
                    egui::Button::new(RichText::new(split_lbl.as_str()).size(11.0).color(Color32::WHITE))
                        .fill(Color32::from_rgb(180, 100, 0)).min_size(egui::vec2(38.0, 20.0))
                } else { btn(split_lbl.as_str()) };
                if ui.add(split_btn).clicked() {
                    self.yaesu2_split = !self.yaesu2_split;
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button,
                        if self.yaesu2_split { 7 } else { 8 }));
                }
                let scan_lbl = rust_i18n::t!("dev_scan").to_string();
                let scan_btn = if self.yaesu2_scan {
                    egui::Button::new(RichText::new(scan_lbl.as_str()).size(11.0).color(Color32::WHITE))
                        .fill(Color32::from_rgb(0, 120, 0)).min_size(egui::vec2(38.0, 20.0))
                } else { btn(scan_lbl.as_str()) };
                if ui.add(scan_btn).clicked() {
                    self.yaesu2_scan = !self.yaesu2_scan;
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button,
                        if self.yaesu2_scan { 1 } else { 2 }));
                }
                // Interne ATU (FTX-1): momentane Tune (band-gated <54 MHz) + aan/uit-toggle
                // met echte stand via AC;-poll. Server stuurt AC003 voor tune-start (FTX-1).
                // ATU: HF+6m (<54 MHz) en niet in FM (mode 5).
                let atu_avail = self.yaesu2_freq_a >= 1 && self.yaesu2_freq_a < 54_000_000 && !matches!(self.yaesu2_mode, 5 | 12);
                let can_tune = self.yaesu2_connected && atu_avail;
                let tune_lbl = rust_i18n::t!("dev_tune").to_string();
                let tune_btn = if self.yaesu2_tuner_state == 2 {
                    egui::Button::new(RichText::new(tune_lbl.as_str()).size(11.0).color(Color32::WHITE))
                        .fill(Color32::from_rgb(180, 0, 0)).min_size(egui::vec2(38.0, 20.0))
                } else { btn(tune_lbl.as_str()) };
                if ui.add_enabled(can_tune, tune_btn).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button, 3)); // start tuning (momentaan)
                }
                let atu_on = self.yaesu2_tuner_state == 1;
                let atu_btn = if atu_on {
                    egui::Button::new(RichText::new("ATU").size(11.0).color(Color32::WHITE))
                        .fill(Color32::from_rgb(0, 90, 200)).min_size(egui::vec2(38.0, 20.0))
                } else { btn("ATU") };
                if ui.add_enabled(atu_avail, atu_btn).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button,
                        if atu_on { 4 } else { 15 })); // toggle: uit (AC000) / aan (AC001)
                }
            });
            // DSP/functie-controls (PATCH-yaesu-extra-controls) - radio 2.
            let dsp1 = self.yaesu2_feature_toggles;
            let lvl1 = self.yaesu2_feature_levels;
            self.render_yaesu_dsp_block(ui, 1, dsp1, lvl1);
            self.render_yaesu_clarifier_block(ui, 1);
            self.render_yaesu_levels_block(ui, 1);
            // Quick Memory Bank (FTX-1-specifiek): momentane actie-knoppen.
            // Store = huidige VFO in QMB (QI;), Recall = QMB doorlopen (QR;).
            ui.horizontal(|ui| {
                ui.label("QMB:");
                let store_lbl = rust_i18n::t!("dev_store").to_string();
                if ui.add(btn(store_lbl.as_str()))
                    .on_hover_text(rust_i18n::t!("hover_quick_mem_save").to_string()).clicked()
                {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button, 13));
                }
                let recall_lbl = rust_i18n::t!("dev_recall").to_string();
                if ui.add(btn(recall_lbl.as_str()))
                    .on_hover_text(rust_i18n::t!("dev_recall_hover").to_string()).clicked()
                {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button, 14));
                }
            });
        }
        // Squelch / RF-power / RF-gain sliders - spiegel van het 991A-paneel.
        {
            use sdr_remote_core::protocol::ControlId;
            let slider_w = 120.0;
            egui::Grid::new("yaesu2_sliders").num_columns(4).spacing([4.0, 2.0]).show(ui, |ui| {
                ui.allocate_space(egui::vec2(55.0, 0.0));
                ui.label("SQL");
                let sql = egui::Slider::new(&mut self.yaesu2_squelch, 0..=100)
                    .custom_formatter(|v, _| format!("{:.0}", v));
                let resp = ui.add_sized([slider_w, 16.0], sql);
                let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.yaesu2_squelch, 0..=100, 1.0);
                if resp.changed() || scrolled {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Squelch, self.yaesu2_squelch));
                    self.yaesu2_control_changed_at = Some(std::time::Instant::now());
                }
                ui.label("PWR");
                let pwr_max2 = if self.yaesu2_tx_power_max >= 5 { self.yaesu2_tx_power_max } else { 100 };
                if self.yaesu2_rf_power > pwr_max2 { self.yaesu2_rf_power = pwr_max2; }
                let pwr = egui::Slider::new(&mut self.yaesu2_rf_power, 5..=pwr_max2)
                    .custom_formatter(|v, _| format!("{:.0}W", v));
                let resp = ui.add_sized([slider_w, 16.0], pwr)
                    .on_hover_text(rust_i18n::t!("dev_max_for_band", w = pwr_max2).to_string());
                let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.yaesu2_rf_power, 5..=pwr_max2, 1.0);
                if resp.changed() || scrolled {
                    self.yaesu2_control_changed_at = Some(std::time::Instant::now());
                }
                if scrolled || resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2RfPower, self.yaesu2_rf_power));
                    self.yaesu2_power_pending = Some(self.yaesu2_rf_power);
                    self.yaesu2_power_pending_at = Some(std::time::Instant::now());
                }
                ui.end_row();
                ui.allocate_space(egui::vec2(55.0, 0.0));
                ui.label(rust_i18n::t!("dev_rf_gain").to_string());
                let rf = egui::Slider::new(&mut self.yaesu2_rf_gain, 0..=255)
                    .custom_formatter(|v, _| format!("{:.0}", v));
                let resp = ui.add_sized([slider_w, 16.0], rf);
                let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.yaesu2_rf_gain, 0..=255, 2.0);
                if resp.changed() || scrolled {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2RfGain, self.yaesu2_rf_gain));
                    self.yaesu2_control_changed_at = Some(std::time::Instant::now());
                }
                ui.end_row();
            });
        }
        ui.separator();
        // S-meter (klik wisselt balk <-> analoog; analoog op dezelfde plek).
        let mw2 = ui.available_width().min(350.0).max(200.0);
        let mrect2 = if self.meter_analog[super::M_YAESU2] {
            smeter_analog_sized(ui,
                yaesu_raw_to_dbm(self.yaesu2_smeter), yaesu_raw_to_dbm(self.yaesu2_smeter_peak),
                false, false, Some((mw2, 110.0)))
        } else {
            yaesu_smeter_bar(ui, self.yaesu2_smeter, self.yaesu2_smeter_peak)
        };
        self.meter_click(ui, mrect2, super::M_YAESU2);
        ui.separator();
        // Status: RX/TX + power on/off (spiegel van het 991A-paneel).
        ui.horizontal(|ui| {
            let (tx_color, tx_text) = if self.yaesu2_tx_active {
                (Color32::from_rgb(220, 40, 40), "TX")
            } else {
                (Color32::from_rgb(0, 150, 0), "RX")
            };
            ui.colored_label(tx_color, RichText::new(tx_text).size(16.0).strong());
            ui.separator();
            // Zie slot 0: klikbaar alleen op de 991A (standby); anders label.
            let on_col = Color32::from_rgb(0, 150, 0);
            let off_col = Color32::from_rgb(90, 90, 90);
            if self.yaesu2_model == 0 {
                let (txt, col): (String, Color32) = if self.yaesu2_power_on { (rust_i18n::t!("dev_power_on_upper").to_string(), on_col) } else { (rust_i18n::t!("dev_standby").to_string(), off_col) };
                if ui.add(egui::Button::new(RichText::new(txt).color(Color32::WHITE)).fill(col))
                    .on_hover_text(rust_i18n::t!("dev_991a_standby_hover").to_string())
                    .clicked()
                {
                    let _ = self.cmd_tx.send(Command::SetControl(
                        sdr_remote_core::protocol::ControlId::Yaesu2PowerOnOff,
                        if self.yaesu2_power_on { 0 } else { 1 }));
                }
            } else {
                ui.colored_label(if self.yaesu2_power_on { on_col } else { off_col },
                    if self.yaesu2_power_on { rust_i18n::t!("dev_power_on_upper").to_string() } else { rust_i18n::t!("dev_power_off_upper").to_string() })
                    .on_hover_text(rust_i18n::t!("dev_radio_powers_off_hover").to_string());
            }
            if self.yaesu2_hi_swr {
                ui.separator();
                ui.colored_label(theme::TL_SWR_ALERT_TEXT,
                    RichText::new(rust_i18n::t!("dev_high_swr").to_string()).size(16.0).strong());
            }
        });
        ui.separator();
        self.render_websdr_controls(ui, CatSyncTarget::Yaesu2, self.yaesu2_freq_a, self.yaesu2_mode);
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("dev_mic_gain").to_string());
            let mut mic_gain_display = super::yaesu_mic_gain_to_display(self.yaesu2_mic_gain);
            let slider = egui::Slider::new(&mut mic_gain_display, 0.05..=1.0)
                .logarithmic(true)
                .custom_formatter(|v, _| format!("{:.2}x", v));
            let resp = ui.add_sized([140.0, 16.0], slider);
            let scrolled = super::helpers::slider_wheel(ui, &resp, &mut mic_gain_display, 0.05..=1.0, 0.02);
            if resp.changed() || scrolled {
                self.yaesu2_mic_gain = super::yaesu_mic_gain_from_display(mic_gain_display);
                let _ = self.cmd_tx.send(Command::SetYaesu2TxGain(self.yaesu2_mic_gain));
            }
            if resp.drag_stopped() || scrolled {
                self.save_ptt_config();
            }
        });
        ui.separator();
        // Equalizer - zelfde gedeelde generieke component als het 991A-window (slot 1).
        self.render_yaesu_eq(ui, 1);
        ui.separator();
        // Memory Channels - zelfde gedeelde tabel als het 991A-window (via slot 1).
        if super::helpers::chevron_label(ui, self.collapse_yaesu2_memories,
            RichText::new(rust_i18n::t!("dev_memory_channels").to_string()).strong().size(14.0)).clicked()
        {
            self.collapse_yaesu2_memories = !self.collapse_yaesu2_memories;
        }
        if self.collapse_yaesu2_memories {
            // Schaal de lijst mee tot de onderkant van het window i.p.v. een
            // vaste hoogte: gebruik de resterende beschikbare hoogte (met een
            // ondergrens zodat hij bruikbaar blijft in een klein window).
            let avail = ui.available_height().max(120.0);
            ui.indent("yaesu2_memories_body", |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("yaesu2_memories_scroll")
                    .max_height(avail)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.render_yaesu_memories_slot(ui, 1);
                    });
            });
        }
        // (geen separator hier - pariteit met het 991A-window; de lijn hoorde er niet)
        // Radio Settings (EX Menu) - FTX-1 hiërarchisch. C1: ruwe adres/waarde-lijst
        // ter verificatie van de server-scan; C3 maakt er een P1>P2>P3 browser van.
        if super::helpers::chevron_label(ui, self.collapse_yaesu2_menu,
            RichText::new(rust_i18n::t!("dev_radio_settings").to_string()).strong().size(14.0)).clicked()
        {
            self.collapse_yaesu2_menu = !self.collapse_yaesu2_menu;
        }
        if self.collapse_yaesu2_menu {
            ui.indent("yaesu2_menu_body", |ui| self.render_yaesu2_ex_menu(ui));
        }
    }

    /// FTX-1 EX-menu browser (Fase C3). Groepeert de live-gescande EX-waarden per
    /// groep > subgroep met labels uit de chart (Table 3); per item een waarde-veld
    /// + Set. Adressen/waarden = ground truth radio; labels = chart (cosmetisch).
    fn render_yaesu2_ex_menu(&mut self, ui: &mut egui::Ui) {
        use super::ftx1_ex_chart;
        ui.horizontal(|ui| {
            if ui.button(rust_i18n::t!("dev_read_radio").to_string()).clicked() {
                self.yaesu2_menu_received = false;
                let _ = self.cmd_tx.send(Command::SetControl(
                    sdr_remote_core::protocol::ControlId::Yaesu2ReadMenus, 0));
            }
            let n = self.yaesu2_menu_entries.len();
            ui.label(rust_i18n::t!("dev_settings_count", n = n).to_string());
            ui.separator();
            ui.label(rust_i18n::t!("dev_filter").to_string());
            ui.add(egui::TextEdit::singleline(&mut self.yaesu2_menu_filter).desired_width(120.0));
            if !self.yaesu2_menu_filter.is_empty() && ui.button("x").clicked() {
                self.yaesu2_menu_filter.clear();
            }
        });

        // Bouw de gegroepeerde view lokaal (geen self-borrow tijdens render).
        // Item = (addr, group, sub, naam+desc, huidige waarde).
        let filt = self.yaesu2_menu_filter.to_lowercase();
        let mut groups: Vec<(String, Vec<(String, String, String, String)>)> = Vec::new();
        for (addr, val) in &self.yaesu2_menu_entries {
            let (group, sub, desc) = match ftx1_ex_chart::lookup(addr) {
                Some((g, s, d)) => (g.to_string(), s.to_string(), d.to_string()),
                None => (rust_i18n::t!("dev_other").to_string(), String::new(), addr.clone()),
            };
            if !filt.is_empty()
                && !desc.to_lowercase().contains(&filt)
                && !group.to_lowercase().contains(&filt)
                && !sub.to_lowercase().contains(&filt)
            {
                continue;
            }
            match groups.iter_mut().find(|(g, _)| *g == group) {
                Some((_, items)) => items.push((addr.clone(), sub, desc, val.clone())),
                None => groups.push((group, vec![(addr.clone(), sub, desc, val.clone())])),
            }
        }

        let avail = ui.available_height().max(150.0);
        let mut to_set: Option<(String, String)> = None; // (addr, value)
        egui::ScrollArea::vertical()
            .id_salt("yaesu2_menu_scroll")
            .max_height(avail)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (group, items) in &groups {
                    egui::CollapsingHeader::new(RichText::new(group).strong())
                        .default_open(!filt.is_empty())
                        .show(ui, |ui| {
                            let mut last_sub = String::new();
                            for (addr, sub, desc, val) in items {
                                if sub != &last_sub {
                                    // Subgroep-header bold (was .weak() -> vrijwel onleesbaar,
                                    // operator-feedback build 120). Bold + volle contrast.
                                    ui.label(RichText::new(sub).strong());
                                    last_sub = sub.clone();
                                }
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(desc).size(11.0));
                                    let buf = self.yaesu2_menu_edits
                                        .entry(addr.clone()).or_insert_with(|| val.clone());
                                    ui.add(egui::TextEdit::singleline(buf)
                                        .desired_width(70.0)
                                        .font(egui::FontId::monospace(11.0)));
                                    // Set alleen tonen als de buffer afwijkt van de radio-waarde.
                                    if buf.as_str() != val.as_str() && ui.small_button(rust_i18n::t!("dev_set").to_string()).clicked() {
                                        to_set = Some((addr.clone(), buf.clone()));
                                    }
                                });
                            }
                        });
                }
            });

        if let Some((addr, value)) = to_set {
            // Optimistische baseline-update: de radio-waarde wordt anders pas
            // bij een volgende "Read radio" ververst, waardoor de UI blijft
            // denken dat de oude waarde nog in de radio staat. Daardoor
            // verschijnt "Set" niet meer als je naar het origineel terugzet.
            // Alleen bijwerken als het commando daadwerkelijk verstuurd is.
            if self.cmd_tx.send(Command::SetYaesu2Menu(addr.clone(), value.clone())).is_ok() {
                if let Some(entry) =
                    self.yaesu2_menu_entries.iter_mut().find(|(a, _)| *a == addr)
                {
                    entry.1 = value;
                }
            }
        }
    }

    fn render_yaesu_menu(&mut self, ui: &mut egui::Ui) {
        use super::yaesu_menu;

        ui.horizontal(|ui| {
            if ui.button(rust_i18n::t!("dev_read_radio").to_string()).clicked() {
                self.yaesu_menu_received = false;
                let _ = self.cmd_tx.send(Command::SetControl(
                    sdr_remote_core::protocol::ControlId::YaesuReadMenus, 0));
            }
        });

        if self.yaesu_menu_items.is_empty() {
            ui.label(rust_i18n::t!("dev_click_read_radio_153").to_string());
            return;
        }

        egui::ScrollArea::vertical().max_height(300.0).id_salt("yaesu_menu_scroll").show(ui, |ui| {
            egui::Grid::new("yaesu_menu_grid")
                .striped(true)
                .num_columns(4)
                .min_col_width(30.0)
                .spacing([6.0, 2.0])
                .show(ui, |ui| {
                    ui.label(RichText::new("#").strong());
                    ui.label(RichText::new(rust_i18n::t!("dev_setting").to_string()).strong());
                    ui.label(RichText::new(rust_i18n::t!("dev_value").to_string()).strong());
                    ui.label(RichText::new("").strong());
                    ui.end_row();

                    for item in &mut self.yaesu_menu_items {
                        let def = yaesu_menu::MENU_DEFS.iter()
                            .find(|d| d.number == item.number);

                        let name = def.map_or("?", |d| d.name);
                        let encoding = def.map_or("", |d| d.encoding);

                        // Menu number
                        ui.label(format!("{:03}", item.number));

                        // Name
                        ui.label(name);

                        // Value - read-only, enum dropdown, or text
                        let read_only = def.map_or(false, |d| d.p2_digits == 0);
                        if read_only {
                            ui.label(RichText::new(&item.raw_value).color(Color32::GRAY));
                        } else if yaesu_menu::is_enum(encoding) {
                            let options = yaesu_menu::parse_enum_options(encoding);
                            let display = yaesu_menu::format_value(&item.raw_value, encoding);
                            egui::ComboBox::from_id_salt(format!("exm_{}", item.number))
                                .width(100.0)
                                .selected_text(&display)
                                .show_ui(ui, |ui| {
                                    for (code, label) in &options {
                                        if ui.selectable_label(item.raw_value == *code, label).clicked() {
                                            let _ = self.cmd_tx.send(Command::SetYaesuMenu(item.number, code.clone()));
                                            item.raw_value = code.clone();
                                        }
                                    }
                                });
                        } else {
                            // Numeric value - show as text, editable
                            let resp = ui.add(egui::TextEdit::singleline(&mut item.raw_value)
                                .desired_width(60.0).font(egui::FontId::monospace(11.0)));
                            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                let _ = self.cmd_tx.send(Command::SetYaesuMenu(item.number, item.raw_value.clone()));
                            }
                        }

                        // Default indicator
                        if let Some(d) = def {
                            if item.raw_value == d.default {
                                ui.label("");
                            } else {
                                ui.label(RichText::new("*").color(Color32::from_rgb(255, 180, 50)));
                            }
                        } else {
                            ui.label("");
                        }

                        ui.end_row();
                    }
                });
        });
    }

    fn render_yaesu_memories(&mut self, ui: &mut egui::Ui) {
        use super::yaesu_memory;

        ui.horizontal(|ui| {
            if ui.button(rust_i18n::t!("dev_read_radio").to_string()).clicked() {
                self.yaesu_mem_radio_received = false; // allow processing new data
                // Read/write routeren naar de actieve radio (slot 0 = 991A, 1 = FTX-1).
                let cmd = if self.yaesu_mem_active_slot == 0 {
                    sdr_remote_core::protocol::ControlId::YaesuReadMemories
                } else {
                    sdr_remote_core::protocol::ControlId::Yaesu2ReadMemories
                };
                let _ = self.cmd_tx.send(Command::SetControl(cmd, 0));
            }
            if !self.yaesu_mem_channels.is_empty() {
                if ui.button(rust_i18n::t!("dev_write_radio").to_string()).clicked() {
                    let path = std::path::Path::new(&self.yaesu_mem_file);
                    // Save to file first, then send to server for writing
                    let _ = yaesu_memory::save_tab_file(path, &self.yaesu_mem_channels);
                    if let Ok(text) = std::fs::read_to_string(path) {
                        let cmd = if self.yaesu_mem_active_slot == 0 {
                            Command::WriteYaesuMemories(text)
                        } else {
                            Command::WriteYaesu2Memories(text)
                        };
                        let _ = self.cmd_tx.send(cmd);
                    }
                }
            }
            if ui.button(rust_i18n::t!("dev_load_file").to_string()).clicked() {
                let path = std::path::Path::new(&self.yaesu_mem_file);
                match yaesu_memory::parse_tab_file(path) {
                    Ok(ch) => {
                        log::info!("Loaded {} channels from {}", ch.len(), self.yaesu_mem_file);
                        self.yaesu_mem_channels = ch;
                        self.yaesu_mem_dirty = false;
                        self.yaesu_mem_selected = None;
                    }
                    Err(e) => log::warn!("Load failed: {}", e),
                }
            }
            if self.yaesu_mem_dirty {
                if ui.button(rust_i18n::t!("dev_save").to_string()).clicked() {
                    let path = std::path::Path::new(&self.yaesu_mem_file);
                    match yaesu_memory::save_tab_file(path, &self.yaesu_mem_channels) {
                        Ok(()) => {
                            log::info!("Saved {} channels to {}", self.yaesu_mem_channels.len(), self.yaesu_mem_file);
                            self.yaesu_mem_dirty = false;
                        }
                        Err(e) => log::warn!("Save failed: {}", e),
                    }
                }
            }
            if ui.button("+").clicked() {
                let next_ch = self.yaesu_mem_channels.len() as u16 + 1;
                let mut ch = yaesu_memory::YaesuMemoryChannel::default();
                ch.channel_number = next_ch;
                ch.name = format!("CH {}", next_ch);
                self.yaesu_mem_channels.push(ch);
                self.yaesu_mem_dirty = true;
                // Auto-select new channel for editing
                self.yaesu_mem_selected = Some(self.yaesu_mem_channels.len() - 1);
            }
            // Import: kopieer de volledige geheugenlijst van de ANDERE radio.
            // Door het mem::swap-patroon in render_yaesu_memories_slot staan de
            // kanalen van de andere radio altijd in `yaesu2_mem_channels`,
            // ongeacht welk slot nu getoond wordt. 991A<->FTX-1 delen exact dezelfde
            // YaesuMemoryChannel + tab-kolommen, dus een directe clone volstaat;
            // de per-model write-functie op de server mapt de mode-codes.
            // Niet-destructief: vult de lijst + markeert dirty - pas bij
            // "Write radio" gaat het écht naar de radio.
            if !self.yaesu2_mem_channels.is_empty() {
                let from_radio = if self.yaesu_mem_active_slot == 0 { 2 } else { 1 };
                let name = self.yaesu_slot_label(from_radio - 1);
                if ui.button(rust_i18n::t!("dev_import_from", name = name).to_string())
                    .on_hover_text(rust_i18n::t!("hover_take_memories").to_string())
                    .clicked()
                {
                    self.yaesu_mem_channels = self.yaesu2_mem_channels.clone();
                    self.yaesu_mem_dirty = true;
                    self.yaesu_mem_selected = None;
                    log::info!("Imported {} channels from Radio {}",
                        self.yaesu_mem_channels.len(), from_radio);
                }
            }
        });

        // File path
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("dev_file").to_string());
            ui.add(egui::TextEdit::singleline(&mut self.yaesu_mem_file).desired_width(250.0));
        });

        // Filter
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("dev_filter").to_string());
            ui.add(egui::TextEdit::singleline(&mut self.yaesu_mem_filter).desired_width(150.0));
            if !self.yaesu_mem_filter.is_empty() {
                if ui.button("×").clicked() {
                    self.yaesu_mem_filter.clear();
                }
            }
            let n = self.yaesu_mem_channels.len();
            ui.label(rust_i18n::t!("dev_channels_count", n = n).to_string());
        });

        if self.yaesu_mem_channels.is_empty() {
            ui.label(rust_i18n::t!("dev_no_channels_loaded").to_string());
            return;
        }

        // Channel table
        let filter_lower = self.yaesu_mem_filter.to_lowercase();
        let selected = self.yaesu_mem_selected;
        let mut tune_action: Option<(u64, u8, u16)> = None; // (freq, mode, channel#)
        let mut delete_idx: Option<usize> = None;

        // Use a horizontal layout so the table can exceed the viewport width
        egui::ScrollArea::both().show(ui, |ui| {
            let header_style = |t: &str| RichText::new(t).strong().size(11.0);
            let on_off = |v: bool| if v { rust_i18n::t!("dev_on").to_string() } else { "-".to_string() };

            egui::Grid::new("yaesu_mem_grid")
                .striped(true)
                .min_col_width(28.0)
                .spacing([6.0, 3.0])
                .num_columns(17)
                .show(ui, |ui| {
                    // Header row
                    let headers: [String; 17] = [
                        "CH".to_string(),
                        rust_i18n::t!("dev_name").to_string(),
                        "RX Freq".to_string(),
                        rust_i18n::t!("dev_mode_col").to_string(),
                        "Dir".to_string(),
                        rust_i18n::t!("dev_offset").to_string(),
                        rust_i18n::t!("dev_tone").to_string(),
                        "CTCSS/DCS".to_string(),
                        "AGC".to_string(),
                        "NB".to_string(),
                        "DNR".to_string(),
                        "IPO".to_string(),
                        "ATT".to_string(),
                        rust_i18n::t!("dev_tuner_col").to_string(),
                        rust_i18n::t!("dev_skip").to_string(),
                        rust_i18n::t!("dev_step").to_string(),
                        String::new(),
                    ];
                    for h in &headers {
                        ui.label(header_style(h.as_str()));
                    }
                    ui.end_row();

                    for idx in 0..self.yaesu_mem_channels.len() {
                        if !filter_lower.is_empty() {
                            let name_lower = self.yaesu_mem_channels[idx].name.to_lowercase();
                            if !name_lower.contains(&filter_lower) {
                                continue;
                            }
                        }

                        let is_selected = selected == Some(idx);

                        if is_selected {
                            // --- Editing mode ---
                            let ch = &mut self.yaesu_mem_channels[idx];
                            let hi = Color32::from_rgb(255, 220, 100);

                            ui.label(RichText::new(format!("{}", ch.channel_number)).color(hi));

                            if ui.add(egui::TextEdit::singleline(&mut ch.name).desired_width(90.0)).changed() {
                                self.yaesu_mem_dirty = true;
                            }

                            // RX Freq
                            let mut freq_str = format!("{:.5}", ch.rx_freq_hz as f64 / 1_000_000.0);
                            if ui.add(egui::TextEdit::singleline(&mut freq_str)
                                .desired_width(85.0).font(egui::FontId::monospace(11.0))).changed() {
                                if let Ok(mhz) = freq_str.trim().replace(',', ".").parse::<f64>() {
                                    let hz = (mhz * 1_000_000.0).round() as u64;
                                    if hz >= 100_000 && hz <= 500_000_000 {
                                        ch.rx_freq_hz = hz;
                                        self.yaesu_mem_dirty = true;
                                    }
                                }
                            }

                            // Mode
                            egui::ComboBox::from_id_salt(format!("mm_{}", idx))
                                .width(65.0).selected_text(&ch.mode)
                                .show_ui(ui, |ui| {
                                    for &m in yaesu_memory::MODES {
                                        if ui.selectable_label(ch.mode == m, m).clicked() {
                                            ch.mode = m.to_string(); ch.tx_mode = m.to_string();
                                            self.yaesu_mem_dirty = true;
                                        }
                                    }
                                });

                            // Offset direction
                            egui::ComboBox::from_id_salt(format!("md_{}", idx))
                                .width(55.0).selected_text(&ch.offset_direction)
                                .show_ui(ui, |ui| {
                                    for &d in yaesu_memory::OFFSET_DIRS {
                                        if ui.selectable_label(ch.offset_direction == d, d).clicked() {
                                            ch.offset_direction = d.to_string();
                                            if d == "Simplex" {
                                                ch.offset_freq.clear();
                                            } else if ch.offset_freq.is_empty() {
                                                // Default offset based on band
                                                ch.offset_freq = if ch.rx_freq_hz >= 430_000_000 {
                                                    "1,60 MHz".into()
                                                } else {
                                                    "600 kHz".into()
                                                };
                                            }
                                            self.yaesu_mem_dirty = true;
                                        }
                                    }
                                });

                            // Offset frequency
                            if ch.offset_direction != "Simplex" {
                                egui::ComboBox::from_id_salt(format!("mof_{}", idx))
                                    .width(70.0).selected_text(if ch.offset_freq.is_empty() { "-" } else { &ch.offset_freq })
                                    .show_ui(ui, |ui| {
                                        for &f in yaesu_memory::OFFSET_FREQS {
                                            let label = if f.is_empty() { "-" } else { f };
                                            if ui.selectable_label(ch.offset_freq == f, label).clicked() {
                                                ch.offset_freq = f.to_string();
                                                self.yaesu_mem_dirty = true;
                                            }
                                        }
                                    });
                            } else {
                                ui.label("-");
                            }

                            // Tone mode
                            egui::ComboBox::from_id_salt(format!("mt_{}", idx))
                                .width(55.0).selected_text(if ch.tone_mode == "None" { "-" } else { &ch.tone_mode })
                                .show_ui(ui, |ui| {
                                    for &t in yaesu_memory::TONE_MODES {
                                        let l = if t == "None" { "-" } else { t };
                                        if ui.selectable_label(ch.tone_mode == t, l).clicked() {
                                            ch.tone_mode = t.to_string(); self.yaesu_mem_dirty = true;
                                        }
                                    }
                                });

                            // CTCSS/DCS
                            if matches!(ch.tone_mode.as_str(), "Tone" | "Tone ENC" | "T SQL") {
                                egui::ComboBox::from_id_salt(format!("mc_{}", idx))
                                    .width(70.0).selected_text(&ch.ctcss)
                                    .show_ui(ui, |ui| {
                                        for &c in yaesu_memory::CTCSS_TONES {
                                            if ui.selectable_label(ch.ctcss == c, c).clicked() {
                                                ch.ctcss = c.to_string(); self.yaesu_mem_dirty = true;
                                            }
                                        }
                                    });
                            } else { ui.label("-"); }

                            // AGC
                            egui::ComboBox::from_id_salt(format!("ma_{}", idx))
                                .width(45.0).selected_text(&ch.agc)
                                .show_ui(ui, |ui| {
                                    for &a in yaesu_memory::AGC_MODES {
                                        if ui.selectable_label(ch.agc == a, a).clicked() {
                                            ch.agc = a.to_string(); self.yaesu_mem_dirty = true;
                                        }
                                    }
                                });

                            // AGC (combo)
                            egui::ComboBox::from_id_salt(format!("mag_{}", idx))
                                .width(40.0).selected_text(&ch.agc)
                                .show_ui(ui, |ui| {
                                    for &a in yaesu_memory::AGC_MODES {
                                        if ui.selectable_label(ch.agc == a, a).clicked() {
                                            ch.agc = a.to_string(); self.yaesu_mem_dirty = true;
                                        }
                                    }
                                });
                            // NB (checkbox)
                            if ui.checkbox(&mut ch.noise_blanker, "").changed() { self.yaesu_mem_dirty = true; }
                            // DNR (combo)
                            egui::ComboBox::from_id_salt(format!("mdn_{}", idx))
                                .width(35.0).selected_text(&ch.dnr)
                                .show_ui(ui, |ui| {
                                    for &d in yaesu_memory::DNR_LEVELS {
                                        if ui.selectable_label(ch.dnr == d, d).clicked() {
                                            ch.dnr = d.to_string(); self.yaesu_mem_dirty = true;
                                        }
                                    }
                                });
                            // IPO (combo)
                            egui::ComboBox::from_id_salt(format!("mip_{}", idx))
                                .width(40.0).selected_text(&ch.ipo)
                                .show_ui(ui, |ui| {
                                    for &i in yaesu_memory::IPO_MODES {
                                        if ui.selectable_label(ch.ipo == i, i).clicked() {
                                            ch.ipo = i.to_string(); self.yaesu_mem_dirty = true;
                                        }
                                    }
                                });
                            // ATT, Tuner, Skip (checkboxes)
                            if ui.checkbox(&mut ch.attenuator, "").changed() { self.yaesu_mem_dirty = true; }
                            if ui.checkbox(&mut ch.tuner, "").changed() { self.yaesu_mem_dirty = true; }
                            if ui.checkbox(&mut ch.skip, "").changed() { self.yaesu_mem_dirty = true; }

                            // Step
                            egui::ComboBox::from_id_salt(format!("ms_{}", idx))
                                .width(60.0).selected_text(&ch.step)
                                .show_ui(ui, |ui| {
                                    for &s in yaesu_memory::STEPS {
                                        if ui.selectable_label(ch.step == s, s).clicked() {
                                            ch.step = s.to_string(); self.yaesu_mem_dirty = true;
                                        }
                                    }
                                });

                            // Actions
                            ui.horizontal(|ui| {
                                if ui.small_button(rust_i18n::t!("dev_tune").to_string()).clicked() {
                                    tune_action = Some((ch.rx_freq_hz, yaesu_memory::mode_string_to_internal(&ch.mode), ch.channel_number));
                                }
                                if ui.small_button("×").clicked() { delete_idx = Some(idx); }
                            });

                            ui.end_row();
                        } else {
                            // --- Display mode ---
                            let ch = &self.yaesu_mem_channels[idx];
                            let c = Color32::from_rgb(200, 200, 200);

                            let resp = ui.add(egui::Label::new(RichText::new(format!("{}", ch.channel_number)).color(c)).sense(egui::Sense::click()));
                            if resp.clicked() {
                                self.yaesu_mem_selected = Some(idx);
                                tune_action = Some((ch.rx_freq_hz, yaesu_memory::mode_string_to_internal(&ch.mode), ch.channel_number));
                            }

                            ui.add(egui::Label::new(RichText::new(&ch.name).color(c).strong()).sense(egui::Sense::click()));
                            ui.add(egui::Label::new(RichText::new(yaesu_memory::format_freq_display(ch.rx_freq_hz)).color(c).family(egui::FontFamily::Monospace)).sense(egui::Sense::click()));
                            ui.label(RichText::new(&ch.mode).color(c));

                            // Dir
                            let dir_text = match ch.offset_direction.as_str() {
                                "Simplex" => "S", "Plus" => "+", "Minus" => "-",
                                _ => &ch.offset_direction,
                            };
                            ui.label(RichText::new(dir_text).color(c));

                            // Offset freq
                            ui.label(RichText::new(if ch.offset_freq.is_empty() { "-" } else { &ch.offset_freq }).color(c));

                            ui.label(RichText::new(if ch.tone_mode == "None" { "-" } else { &ch.tone_mode }).color(c));

                            let tone_val = match ch.tone_mode.as_str() {
                                "Tone" | "Tone ENC" | "T SQL" => ch.ctcss.clone(),
                                "DCS" | "DCS ENC" | "D Code" => ch.dcs.clone(),
                                _ => "-".into(),
                            };
                            ui.label(RichText::new(&tone_val).color(c));

                            ui.label(RichText::new(&ch.agc).color(c));
                            ui.label(RichText::new(on_off(ch.noise_blanker)).color(c));
                            ui.label(RichText::new(&ch.dnr).color(c));
                            ui.label(RichText::new(&ch.ipo).color(c));
                            ui.label(RichText::new(on_off(ch.attenuator)).color(c));
                            ui.label(RichText::new(on_off(ch.tuner)).color(c));
                            ui.label(RichText::new(on_off(ch.skip)).color(c));
                            ui.label(RichText::new(&ch.step).color(c));

                            if ui.small_button(rust_i18n::t!("dev_edit").to_string()).clicked() {
                                self.yaesu_mem_selected = Some(idx);
                            }

                            ui.end_row();
                        }
                    }
                });
        });

        // Execute deferred actions: recall memory channel only.
        // FM -> DATA-FM switch happens transparently at PTT time (server-side).
        if let Some((_freq, _mode, ch_num)) = tune_action {
            let _ = self.cmd_tx.send(Command::SetControl(
                sdr_remote_core::protocol::ControlId::YaesuRecallMemory, ch_num));
            self.yaesu_in_memory_mode = true;
            self.yaesu_current_mem_ch = self.yaesu_mem_selected;
        }
        if let Some(idx) = delete_idx {
            self.yaesu_mem_channels.remove(idx);
            self.yaesu_mem_selected = None;
            self.yaesu_mem_dirty = true;
            self.yaesu_mem_delete_popup = true;
        }

        // Popup shown after a delete: make clear the row is only removed locally
        // and cannot be erased from the radio over CAT (front-panel only).
        if self.yaesu_mem_delete_popup {
            let ctx = ui.ctx().clone();
            egui::Window::new(rust_i18n::t!("dev_row_removed_title").to_string())
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(&ctx, |ui| {
                    ui.set_max_width(360.0);
                    ui.label(rust_i18n::t!("dev_row_removed_body").to_string());
                    ui.add_space(8.0);
                    if ui.button(rust_i18n::t!("dev_ok").to_string()).clicked() {
                        self.yaesu_mem_delete_popup = false;
                    }
                });
        }
    }

    fn yaesu_compact_mode_label(mode: u8) -> &'static str {
        match mode {
            0 => "LSB",
            1 => "USB",
            3 => "CW-L",
            4 => "CW-U",
            5 => "FM",
            6 => "AM",
            7 => "DIGU",
            9 => "DIGL",
            12 => "C4FM",
            _ => "?",
        }
    }

    fn yaesu_compact_smeter_label(raw: u16) -> String {
        let raw = raw as f32;
        if raw <= 108.0 {
            let s_unit = (raw / 12.0).round().clamp(0.0, 9.0) as u8;
            format!("S{}", s_unit)
        } else {
            let db_over = ((raw - 108.0) * 0.5).round().max(0.0) as i32;
            format!("S9+{} dB", db_over)
        }
    }

    fn render_yaesu_compact_status(&mut self, ui: &mut egui::Ui, slot: u8) {
        let (freq_a, freq_b, mode, power_on, tx_active, smeter, hi_swr, target) = if slot == 0 {
            (
                self.yaesu_freq_a,
                self.yaesu_freq_b,
                self.yaesu_mode,
                self.yaesu_power_on,
                self.yaesu_tx_active,
                self.yaesu_smeter,
                self.yaesu_hi_swr,
                CatSyncTarget::Yaesu1,
            )
        } else {
            (
                self.yaesu2_freq_a,
                self.yaesu2_freq_b,
                self.yaesu2_mode,
                self.yaesu2_power_on,
                self.yaesu2_tx_active,
                self.yaesu2_smeter,
                self.yaesu2_hi_swr,
                CatSyncTarget::Yaesu2,
            )
        };

        egui::Grid::new(format!("yaesu_compact_grid_{}", slot))
            .num_columns(2)
            .spacing([20.0, 6.0])
            .show(ui, |ui| {
                ui.label("VFO A:");
                ui.label(RichText::new(format!("{} Hz", format_frequency(freq_a)))
                    .size(18.0).strong());
                ui.end_row();

                ui.label("VFO B:");
                ui.label(RichText::new(format!("{} Hz", format_frequency(freq_b)))
                    .size(14.0));
                ui.end_row();

                ui.label(rust_i18n::t!("dev_mode").to_string());
                ui.label(RichText::new(Self::yaesu_compact_mode_label(mode)).size(14.0).strong());
                ui.end_row();

                ui.label(rust_i18n::t!("dev_power_colon").to_string());
                ui.label(if power_on { rust_i18n::t!("dev_on_upper").to_string() } else { rust_i18n::t!("dev_off_upper").to_string() });
                ui.end_row();

                ui.label("TX:");
                ui.horizontal(|ui| {
                    ui.label(if tx_active {
                        RichText::new("TX").color(Color32::RED).strong()
                    } else {
                        RichText::new("RX").color(Color32::GREEN)
                    });
                    if hi_swr {
                        ui.label(RichText::new(rust_i18n::t!("dev_high_swr").to_string())
                            .color(theme::TL_SWR_ALERT_TEXT).strong());
                    }
                });
                ui.end_row();

                ui.label("S-Meter:");
                ui.label(RichText::new(Self::yaesu_compact_smeter_label(smeter)).strong());
                ui.end_row();

                ui.label(rust_i18n::t!("dev_audio_colon").to_string());
                if slot == 0 {
                    let slider = egui::Slider::new(&mut self.yaesu_volume, 0.001..=1.0)
                        .logarithmic(true)
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
                    let resp = ui.add_sized([180.0, 16.0], slider);
                    let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.yaesu_volume, 0.001..=1.0, 0.02);
                    if resp.changed() || scrolled {
                        let _ = self.cmd_tx.send(Command::SetYaesuVolume(self.yaesu_volume));
                    }
                } else {
                    let slider = egui::Slider::new(&mut self.yaesu2_volume, 0.001..=1.0)
                        .logarithmic(true)
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
                    let resp = ui.add_sized([180.0, 16.0], slider);
                    let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.yaesu2_volume, 0.001..=1.0, 0.02);
                    if resp.changed() || scrolled {
                        let _ = self.cmd_tx.send(Command::SetYaesu2Volume(self.yaesu2_volume));
                    }
                }
                ui.end_row();
            });

        ui.separator();
        self.render_websdr_controls(ui, target, freq_a, mode);
    }

    pub(super) fn render_device_yaesu(&mut self, ui: &mut egui::Ui, _amber: Color32) {
        let show_radio1 = self.yaesu_connected || self.yaesu_enabled;
        let show_radio2 = self.yaesu2_connected || self.yaesu2_enabled;

        if show_radio1 {
            ui.horizontal(|ui| {
                ui.heading(self.yaesu_slot_label(0));
                ui.separator();
                if ui.checkbox(&mut self.yaesu_enabled, rust_i18n::t!("dev_enable").to_string()).changed() {
                    let _ = self.cmd_tx.send(Command::SetControl(
                        sdr_remote_core::protocol::ControlId::YaesuEnable, self.yaesu_enabled as u16));
                }
                ui.separator();
                if self.yaesu_enabled {
                    let popout_label = if self.yaesu_popout { rust_i18n::t!("dev_close_window").to_string() } else { rust_i18n::t!("dev_open_window").to_string() };
                    if ui.button(popout_label).clicked() {
                        self.yaesu_popout = !self.yaesu_popout;
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("PTT:");
                if ui.selectable_label(!self.yaesu_ptt_toggle_mode, rust_i18n::t!("dev_push_to_talk").to_string()).clicked() {
                    self.yaesu_ptt_toggle_mode = false;
                    self.save_ptt_config();
                }
                if ui.selectable_label(self.yaesu_ptt_toggle_mode, rust_i18n::t!("dev_toggle").to_string()).clicked() {
                    self.yaesu_ptt_toggle_mode = true;
                    self.save_ptt_config();
                }
            });
            ui.separator();
            self.render_yaesu_compact_status(ui, 0);
        }

        if show_radio2 {
            if show_radio1 {
                ui.separator();
            }
            ui.horizontal(|ui| {
                ui.heading(self.yaesu_slot_label(1));
                ui.separator();
                if ui.checkbox(&mut self.yaesu2_enabled, rust_i18n::t!("dev_enable").to_string()).changed() {
                    let _ = self.cmd_tx.send(Command::SetYaesu2Enable(self.yaesu2_enabled));
                    if !self.yaesu2_enabled {
                        self.yaesu2_popout = false;
                    }
                    self.save_ptt_config();
                }
                ui.separator();
                if self.yaesu2_enabled {
                    let popout_label = if self.yaesu2_popout { rust_i18n::t!("dev_close_window").to_string() } else { rust_i18n::t!("dev_open_window").to_string() };
                    if ui.button(popout_label).clicked() {
                        self.yaesu2_popout = !self.yaesu2_popout;
                        self.save_ptt_config();
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("PTT:");
                if ui.selectable_label(!self.yaesu2_ptt_toggle_mode, rust_i18n::t!("dev_push_to_talk").to_string()).clicked() {
                    self.yaesu2_ptt_toggle_mode = false;
                    self.save_ptt_config();
                }
                if ui.selectable_label(self.yaesu2_ptt_toggle_mode, rust_i18n::t!("dev_toggle").to_string()).clicked() {
                    self.yaesu2_ptt_toggle_mode = true;
                    self.save_ptt_config();
                }
            });
            ui.separator();
            self.render_yaesu_compact_status(ui, 1);
        } else {
            self.yaesu2_popout = false;
        }
    }
}