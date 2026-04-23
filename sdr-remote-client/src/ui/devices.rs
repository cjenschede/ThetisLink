// SPDX-License-Identifier: GPL-2.0-or-later

#![allow(dead_code)]
use super::*;

impl SdrRemoteApp {
    pub(super) fn render_devices_screen(&mut self, ui: &mut egui::Ui) {
        let amber = Color32::from_rgb(255, 170, 40);

        // Sub-tabs for each device (only show active PA)
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.device_tab, 0, "Amplitec");
            ui.selectable_value(&mut self.device_tab, 1, "JC-4s");
            if self.spe_active {
                ui.selectable_value(&mut self.device_tab, 2, "SPE Expert");
            }
            if self.rf2k_active {
                ui.selectable_value(&mut self.device_tab, 3, "RF2K-S");
            }
            if self.ub_available {
                ui.selectable_value(&mut self.device_tab, 4, "UltraBeam");
            }
            if self.rotor_available {
                ui.selectable_value(&mut self.device_tab, 5, "Rotor");
            }
            ui.selectable_value(&mut self.device_tab, 6, "Yaesu");
        });
        ui.separator();

        match self.device_tab {
            0 => self.render_device_amplitec(ui),
            1 => self.render_device_tuner(ui, amber),
            2 if self.spe_active => self.render_device_spe(ui, amber),
            3 if self.rf2k_active => self.render_device_rf2k(ui, amber),
            4 if self.ub_available => self.render_device_ultrabeam(ui, amber),
            5 if self.rotor_available => self.render_device_rotor(ui),
            6 => self.render_device_yaesu(ui, amber),
            _ => {}
        }
    }

    pub(super) fn render_device_amplitec(&mut self, ui: &mut egui::Ui) {
        // Header
        ui.horizontal(|ui| {
            ui.heading("Amplitec 6/2 Antenna Switch");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.amplitec_connected {
                    ui.colored_label(Color32::GREEN, "Online");
                } else {
                    ui.colored_label(Color32::RED, "Offline");
                }
            });
        });
        ui.separator();

        // Poort A — TX+RX
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Poort A \u{2014} TX+RX").strong());
            if self.amplitec_switch_a > 0 {
                let label = self.amplitec_label_a(self.amplitec_switch_a);
                ui.label(format!("  Huidige: {}", label));
            }
        });
        ui.horizontal(|ui| {
            for pos in 1..=6u8 {
                let is_active = self.amplitec_switch_a == pos;
                let is_blocked = self.amplitec_switch_b == pos;
                let label = self.amplitec_label_a(pos);
                let btn = if is_active {
                    egui::Button::new(RichText::new(format!(" {} ", label)).strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else if is_blocked {
                    egui::Button::new(RichText::new(format!(" {} ", label)).color(Color32::from_rgb(140, 140, 140)))
                } else {
                    egui::Button::new(format!(" {} ", label))
                };
                let resp = ui.add_enabled(self.amplitec_connected, btn);
                if resp.clicked() {
                    let _ = self.cmd_tx.send(Command::SetAmplitecSwitchA(pos));
                }
                if is_blocked {
                    resp.on_hover_text(format!("{} \u{2014} bezet door Poort B", label));
                }
            }
        });

        ui.add_space(8.0);

        // Poort B — RX
        ui.horizontal(|ui| {
            ui.label(RichText::new("Poort B \u{2014} RX").strong());
            if self.amplitec_switch_b > 0 {
                let label = self.amplitec_label_b(self.amplitec_switch_b);
                ui.label(format!("  Huidige: {}", label));
            }
        });
        ui.horizontal(|ui| {
            for pos in 1..=6u8 {
                let is_active = self.amplitec_switch_b == pos;
                let is_blocked = self.amplitec_switch_a == pos;
                let label = self.amplitec_label_b(pos);
                let btn = if is_active {
                    egui::Button::new(RichText::new(format!(" {} ", label)).strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else if is_blocked {
                    egui::Button::new(RichText::new(format!(" {} ", label)).color(Color32::from_rgb(140, 140, 140)))
                } else {
                    egui::Button::new(format!(" {} ", label))
                };
                let resp = ui.add_enabled(self.amplitec_connected, btn);
                if resp.clicked() {
                    let _ = self.cmd_tx.send(Command::SetAmplitecSwitchB(pos));
                }
                if is_blocked {
                    resp.on_hover_text(format!("{} \u{2014} bezet door Poort A", label));
                }
            }
        });

        ui.add_space(8.0);
        ui.separator();

        // Log
        ui.label(RichText::new("Log").strong());
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
            ui.heading("JC-4s Antenna Tuner");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.tuner_connected {
                    ui.colored_label(Color32::GREEN, "Online");
                } else {
                    ui.colored_label(Color32::RED, "Offline");
                }
            });
        });
        ui.separator();
        ui.add_space(4.0);

        // Status
        let olive_green = Color32::from_rgb(120, 160, 40);
        let state_text = match self.tuner_state {
            1 => "Tuning...",
            2 => "Tune OK",
            3 => "Timeout",
            4 => "Aborted",
            5 => "Done~ (already tuned)",
            _ => "Idle",
        };
        let state_color = match self.tuner_state {
            1 => Color32::from_rgb(60, 120, 220),
            2 => Color32::from_rgb(50, 180, 50),
            3 | 4 => amber,
            5 => olive_green,
            _ => Color32::GRAY,
        };
        ui.horizontal(|ui| {
            ui.label("Status:");
            ui.colored_label(state_color, RichText::new(state_text).strong().size(16.0));
        });

        ui.add_space(8.0);

        // Tune button
        let can_start = self.tuner_connected && self.tuner_can_tune
            && (self.tuner_state == 0 || self.tuner_state == 2 || self.tuner_state == 5);
        let (tune_color, tune_text) = match self.tuner_state {
            1 => (Color32::from_rgb(60, 120, 220), "Tuning..."),
            2 => (Color32::from_rgb(50, 180, 50), "Tune OK"),
            3 => (amber, "Tune X"),
            4 => (amber, "Tune X"),
            5 => (olive_green, "Tune ~"),
            _ => (Color32::from_rgb(80, 80, 80), "Tune"),
        };
        ui.horizontal(|ui| {
            let btn = egui::Button::new(RichText::new(tune_text).color(Color32::WHITE).strong().size(16.0))
                .fill(tune_color)
                .min_size(Vec2::new(120.0, 32.0));
            if ui.add_enabled(can_start, btn).clicked() {
                let _ = self.cmd_tx.send(Command::TunerTune);
            }

            let abort_enabled = self.tuner_state == 1;
            let abort_btn = egui::Button::new(RichText::new("Abort").size(14.0))
                .min_size(Vec2::new(60.0, 32.0));
            if ui.add_enabled(abort_enabled, abort_btn).clicked() {
                let _ = self.cmd_tx.send(Command::TunerAbort);
            }
        });

        if !self.tuner_can_tune {
            ui.add_space(4.0);
            ui.colored_label(amber, "Tuner not available on current antenna");
        }
    }

    pub(super) fn render_device_spe(&mut self, ui: &mut egui::Ui, amber: Color32) {
        // Header: title + active badge + Online/Offline
        ui.horizontal(|ui| {
            ui.heading("SPE Expert 1.3K-FA");
            if self.spe_active {
                ui.colored_label(Color32::from_rgb(50, 180, 50), RichText::new("ACTIVE").strong());
            } else {
                ui.colored_label(Color32::from_rgb(140, 140, 140), "INACTIVE");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.spe_connected {
                    ui.colored_label(Color32::GREEN, "Online");
                } else {
                    ui.colored_label(Color32::RED, "Offline");
                }
            });
        });
        ui.separator();

        // Warning / Alarm (prominent, above everything)
        if self.spe_alarm != b'N' && self.spe_alarm != 0 {
            ui.colored_label(Color32::from_rgb(255, 80, 80),
                RichText::new(format!("ALARM: {}", self.spe_alarm as char)).strong());
        } else if self.spe_warning != b'N' && self.spe_warning != 0 {
            ui.colored_label(amber,
                RichText::new(format!("Warning: {}", self.spe_warning as char)).strong());
        }

        ui.add_space(4.0);

        // Row 1: Power On/Off | Operate/Standby (state+color) | Tune
        ui.horizontal(|ui| {
            // Power — shows current state
            if !self.spe_connected || self.spe_state == 0 {
                let btn = egui::Button::new(RichText::new("Power Off").strong().color(Color32::WHITE))
                    .fill(Color32::from_rgb(120, 120, 120));
                if ui.add(btn).clicked() {
                    let _ = self.cmd_tx.send(Command::SpePowerOn);
                }
            } else {
                let btn = egui::Button::new(RichText::new("Power On").strong().color(Color32::WHITE))
                    .fill(Color32::from_rgb(0, 150, 0));
                if ui.add(btn).clicked() {
                    let _ = self.cmd_tx.send(Command::SpeOff);
                }
            }

            // Operate/Standby — shows current state
            let (op_text, op_color) = match self.spe_state {
                2 => ("Operate", Color32::from_rgb(50, 180, 50)),
                1 => ("Standby", amber),
                _ => ("Off", Color32::from_rgb(120, 120, 120)),
            };
            let btn = egui::Button::new(RichText::new(op_text).strong().color(Color32::WHITE))
                .fill(op_color);
            if ui.add_enabled(self.spe_connected, btn).clicked() {
                let _ = self.cmd_tx.send(Command::SpeOperate);
            }

            // Tune
            if ui.add_enabled(self.spe_connected && self.spe_state == 2,
                egui::Button::new("Tune")).clicked() {
                let _ = self.cmd_tx.send(Command::SpeTune);
            }
        });

        ui.add_space(2.0);

        // Row 2: Ant{N} | In {N} | Low/Mid/High | Band label | Drive (read-only)
        ui.horizontal(|ui| {
            // Antenna toggle — shows bypass/tuner suffix
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
            let pwr_text = match self.spe_power_level {
                0 => "Low",
                1 => "Mid",
                2 => "High",
                _ => "?",
            };
            if ui.add_enabled(self.spe_connected, egui::Button::new(pwr_text)).clicked() {
                let _ = self.cmd_tx.send(Command::SpePower);
            }

            ui.separator();

            // Drive level +/-
            let drive_enabled = self.spe_connected && self.spe_state == 2 && self.spe_active;
            if ui.add_enabled(drive_enabled, egui::Button::new("Drive -")).clicked() {
                let _ = self.cmd_tx.send(Command::SpeDriveDown);
            }
            ui.label(format!("{}%", self.drive_level));
            if ui.add_enabled(drive_enabled, egui::Button::new("Drive +")).clicked() {
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
                ui.colored_label(Color32::from_rgb(50, 180, 50), RichText::new("ACTIVE").strong());
            } else {
                ui.colored_label(Color32::from_rgb(140, 140, 140), "INACTIVE");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.rf2k_connected {
                    ui.colored_label(Color32::GREEN, "Online");
                } else {
                    ui.colored_label(Color32::RED, "Offline");
                }
            });
        });
        ui.separator();

        // Error bar
        if self.rf2k_error_state != 0 {
            let error_text = if self.rf2k_error_text.is_empty() {
                format!("Error state: {}", self.rf2k_error_state)
            } else {
                self.rf2k_error_text.clone()
            };
            ui.horizontal(|ui| {
                ui.colored_label(Color32::from_rgb(255, 80, 80),
                    RichText::new(&error_text).strong());
                if ui.button("Reset").clicked() {
                    let _ = self.cmd_tx.send(Command::Rf2kErrorReset);
                }
            });
        }

        // Row 1: Operate/Standby + Tune + FW Close
        ui.horizontal(|ui| {
            let (op_text, op_color) = if self.rf2k_operate {
                ("Operate", Color32::from_rgb(50, 180, 50))
            } else {
                ("Standby", amber)
            };
            let btn = egui::Button::new(RichText::new(op_text).strong().color(Color32::WHITE))
                .fill(op_color);
            if ui.add_enabled(self.rf2k_connected, btn).clicked() {
                let _ = self.cmd_tx.send(Command::Rf2kOperate(!self.rf2k_operate));
            }

            if ui.add_enabled(self.rf2k_connected && self.rf2k_operate,
                egui::Button::new("Tune")).clicked() {
                let _ = self.cmd_tx.send(Command::Rf2kTune);
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(self.rf2k_connected,
                    egui::Button::new(RichText::new("FW Close").color(Color32::from_rgb(255, 100, 100)))
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
            ui.colored_label(mode_color, RichText::new(format!("Tuner: {}", mode_text)).strong());

            // MAN/AUTO toggle — shows current state
            if self.rf2k_tuner_mode == 2 || self.rf2k_tuner_mode == 4 {
                let (_toggle_text, toggle_btn) = if is_manual {
                    ("Manual", egui::Button::new(RichText::new("Manual").strong())
                        .fill(Color32::from_rgb(100, 160, 230)).small())
                } else {
                    ("Auto", egui::Button::new(RichText::new("Auto").strong())
                        .fill(Color32::from_rgb(100, 160, 230)).small())
                };
                if ui.add_enabled(tuner_edit, toggle_btn).clicked() {
                    let _ = self.cmd_tx.send(Command::Rf2kTunerMode(if is_manual { 1 } else { 0 }));
                }
            }

            // Bypass — shows current state
            let is_bypass = self.rf2k_tuner_mode == 1 || self.rf2k_tuner_setup == "BYPASS";
            let byp_btn = if is_bypass {
                egui::Button::new(RichText::new("Bypass").strong())
                    .fill(Color32::from_rgb(255, 170, 40)).small()
            } else {
                egui::Button::new("Bypass").small()
            };
            if ui.add_enabled(tuner_edit, byp_btn).clicked() {
                let _ = self.cmd_tx.send(Command::Rf2kTunerBypass(!is_bypass));
            }

            // Reset + Store (manual only)
            if ui.add_enabled(tuner_edit && is_manual, egui::Button::new("Reset").small()).clicked() {
                let _ = self.cmd_tx.send(Command::Rf2kTunerReset);
            }
            if ui.add_enabled(tuner_edit && is_manual, egui::Button::new("Store").small()).clicked() {
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
            ui.label(format!("Drive: {}W", self.rf2k_drive_w));

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
            let debug_header = if self.rf2k_show_debug { "Debug ▼" } else { "Debug ▶" };
            if ui.selectable_label(self.rf2k_show_debug, RichText::new(debug_header).strong()).clicked() {
                self.rf2k_show_debug = !self.rf2k_show_debug;
            }

            if self.rf2k_show_debug {
                ui.indent("rf2k_debug_c", |ui| {
                    ui.label(RichText::new("System Info").strong());
                    ui.horizontal(|ui| {
                        ui.label(format!("FW: v{}", self.rf2k_controller_version));
                        if !self.rf2k_hw_revision.is_empty() {
                            ui.label(format!("HW: {}", self.rf2k_hw_revision));
                        }
                        ui.label(format!("BIAS: {:.1}%", self.rf2k_bias_pct_x10 as f32 / 10.0));
                        let psu = match self.rf2k_psu_source { 0 => "Internal", 1 => "External", 2 => "CAN Ctrl", _ => "?" };
                        ui.label(format!("PSU: {}", psu));
                    });
                    ui.horizontal(|ui| {
                        let hours = self.rf2k_uptime_s / 3600;
                        let mins = (self.rf2k_uptime_s % 3600) / 60;
                        if hours >= 24 {
                            ui.label(format!("Uptime: {}d {}h {}m", hours / 24, hours % 24, mins));
                        } else {
                            ui.label(format!("Uptime: {}h {}m", hours, mins));
                        }
                        let tx_h = self.rf2k_tx_time_s / 3600;
                        let tx_m = (self.rf2k_tx_time_s % 3600) / 60;
                        ui.label(format!("TX: {}h {:02}m", tx_h, tx_m));
                        ui.label(format!("Errors: {}", self.rf2k_error_count));
                    });
                    ui.horizontal(|ui| {
                        ui.label(format!("Bank: {}", self.rf2k_storage_bank));
                        ui.label("FRQ Delay:");
                        if ui.add_enabled(self.rf2k_connected, egui::Button::new("−").small()).clicked() {
                            let _ = self.cmd_tx.send(Command::Rf2kFrqDelayDown);
                        }
                        ui.label(format!("{}", self.rf2k_frq_delay));
                        if ui.add_enabled(self.rf2k_connected, egui::Button::new("+").small()).clicked() {
                            let _ = self.cmd_tx.send(Command::Rf2kFrqDelayUp);
                        }
                    });

                    ui.add_space(4.0);
                    ui.label(RichText::new("Settings").strong());
                    ui.horizontal(|ui| {
                        ui.label("Power:");
                        let (pe5_text, pe5_color) = if self.rf2k_high_power {
                            ("HIGH", Color32::from_rgb(255, 80, 80))
                        } else {
                            ("LOW", Color32::from_rgb(50, 180, 50))
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
                        ui.label("Tuner 6m:");
                        let t6m = if self.rf2k_tuner_6m { "ON" } else { "OFF" };
                        if ui.add_enabled(self.rf2k_connected, egui::Button::new(t6m).small()).clicked() {
                            let _ = self.cmd_tx.send(Command::Rf2kSetTuner6m(!self.rf2k_tuner_6m));
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Band gap:");
                        let bg = if self.rf2k_band_gap_allowed { "ON" } else { "OFF" };
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
                        ui.label(RichText::new("Error History").strong());
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
                    ui.label(RichText::new("Dangerous").strong());
                    let zero_btn = egui::Button::new(RichText::new("Zero FRAM").color(Color32::from_rgb(255, 100, 100)));
                    if ui.add_enabled(self.rf2k_connected, zero_btn).clicked() {
                        self.rf2k_confirm_zero_fram = true;
                    }
                });
            }

            // Confirmation dialogs
            if self.rf2k_confirm_high_power {
                egui::Window::new("WARNING")
                    .collapsible(false).resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ui.ctx(), |ui| {
                        ui.label("Setting HIGH power can damage equipment.");
                        ui.label("Are you sure?");
                        ui.horizontal(|ui| {
                            if ui.button("Yes, set HIGH").clicked() {
                                let _ = self.cmd_tx.send(Command::Rf2kSetHighPower(true));
                                self.rf2k_confirm_high_power = false;
                            }
                            if ui.button("Cancel").clicked() {
                                self.rf2k_confirm_high_power = false;
                            }
                        });
                    });
            }
            if self.rf2k_confirm_zero_fram {
                egui::Window::new("DESTRUCTIVE")
                    .collapsible(false).resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ui.ctx(), |ui| {
                        ui.label("All tuner memories will be erased.");
                        ui.label("This cannot be undone!");
                        ui.horizontal(|ui| {
                            if ui.button(RichText::new("Yes, Zero FRAM").color(Color32::from_rgb(255, 80, 80))).clicked() {
                                let _ = self.cmd_tx.send(Command::Rf2kZeroFRAM);
                                self.rf2k_confirm_zero_fram = false;
                            }
                            if ui.button("Cancel").clicked() {
                                self.rf2k_confirm_zero_fram = false;
                            }
                        });
                    });
            }

            // --- Drive Config section ---
            ui.add_space(6.0);
            let drive_header = if self.rf2k_show_drive_config { "Drive Config ▼" } else { "Drive Config ▶" };
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
                        ui.label(RichText::new("Band").strong());
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
                    if ui.add_enabled(self.rf2k_connected, egui::Button::new("Save to Pi")).clicked() {
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
            egui::Window::new("FW Close confirmation")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui.ctx(), |ui| {
                    ui.label("Are you sure? This will close the RF2K-S firmware.");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Yes").clicked() {
                            let _ = self.cmd_tx.send(Command::Rf2kClose);
                            self.rf2k_confirm_fw_close = false;
                        }
                        if ui.button("No").clicked() {
                            self.rf2k_confirm_fw_close = false;
                        }
                    });
                });
        }
    }

    pub(super) fn render_device_ultrabeam(&mut self, ui: &mut egui::Ui, _amber: Color32) {
        // Header
        ui.horizontal(|ui| {
            ui.heading("UltraBeam RCU-06");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.ub_connected {
                    ui.colored_label(Color32::GREEN, "Online");
                } else {
                    ui.colored_label(Color32::RED, "Offline");
                }
                if self.ub_fw_major > 0 {
                    ui.label(format!("FW {}.{}", self.ub_fw_major, self.ub_fw_minor));
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
            ui.label("Direction:");
            let dirs = [("Normal", 0u8), ("180\u{00B0}", 1), ("BiDir", 2)];
            for &(label, dir) in &dirs {
                let is_active = self.ub_direction == dir;
                let btn = if is_active {
                    egui::Button::new(RichText::new(label).strong().color(Color32::WHITE))
                        .fill(Color32::from_rgb(50, 180, 50))
                } else {
                    egui::Button::new(label)
                };
                if ui.add_enabled(self.ub_connected, btn).clicked() {
                    let _ = self.cmd_tx.send(Command::UbSetFrequency(self.ub_frequency_khz, dir));
                }
            }
        });

        // Frequency step buttons + sync
        ui.horizontal(|ui| {
            ui.label("Freq step:");
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
            let sync_btn = egui::Button::new(RichText::new(format!("Sync {}", track_label)).strong())
                .fill(if can_sync { Color32::from_rgb(50, 130, 200) } else { Color32::from_rgb(80, 80, 80) });
            if ui.add_enabled(can_sync, sync_btn).on_hover_text(
                format!("Stel UltraBeam in op {}: {} kHz", track_label, track_khz)
            ).clicked() {
                let _ = self.cmd_tx.send(Command::UbSetFrequency(track_khz, self.ub_direction));
            }
            ui.checkbox(&mut self.ub_auto_track, "Auto")
                .on_hover_text(format!("Auto-track {} frequency", track_label));
        });

        // Motor progress bar (only when moving)
        if self.ub_motors_moving != 0 {
            ui.add_space(4.0);
            let progress = (self.ub_motor_completion as f32 / 60.0).clamp(0.0, 1.0);
            ui.horizontal(|ui| {
                ui.label("Motor:");
                let bar = egui::ProgressBar::new(progress)
                    .text(format!("{:.0}%", progress * 100.0));
                ui.add(bar);
            });
        }

        ui.add_space(4.0);

        // Band presets
        ui.horizontal_wrapped(|ui| {
            ui.label("Band:");
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
                egui::Button::new(RichText::new("Retract").color(Color32::from_rgb(255, 100, 100)))
            ).clicked() {
                self.ub_confirm_retract = true;
            }

            // Read elements
            if ui.add_enabled(self.ub_connected,
                egui::Button::new("Read Elements")).clicked() {
                let _ = self.cmd_tx.send(Command::UbReadElements);
            }
        });

        // Retract confirmation popup
        if self.ub_confirm_retract {
            egui::Window::new("Retract confirmation")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui.ctx(), |ui| {
                    ui.label("Are you sure? This will retract all elements.");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Yes").clicked() {
                            let _ = self.cmd_tx.send(Command::UbRetract);
                            self.ub_confirm_retract = false;
                        }
                        if ui.button("No").clicked() {
                            self.ub_confirm_retract = false;
                        }
                    });
                });
        }

        // Element lengths (read-only)
        if self.ub_elements_mm.iter().any(|&e| e > 0) {
            ui.add_space(4.0);
            ui.separator();
            ui.label("Element lengths (mm):");
            ui.horizontal(|ui| {
                for (i, &mm) in self.ub_elements_mm.iter().enumerate() {
                    ui.label(format!("E{}: {}", i + 1, mm));
                }
            });
        }
    }

    pub(super) fn render_device_rotor(&mut self, ui: &mut egui::Ui) {
        // Header
        ui.horizontal(|ui| {
            ui.heading("Rotor");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.rotor_connected {
                    ui.colored_label(Color32::GREEN, "Online");
                } else {
                    ui.colored_label(Color32::RED, "Offline");
                }
            });
        });
        ui.separator();

        let angle_deg = self.rotor_angle_x10 as f32 / 10.0;
        let target_deg = if self.rotor_rotating { Some(self.rotor_target_x10 as f32 / 10.0) } else { None };

        // Compass circle — click to GoTo
        if let Some(goto) = Self::render_compass(ui, angle_deg, target_deg, self.rotor_connected) {
            let _ = self.cmd_tx.send(Command::RotorGoTo(goto));
        }

        ui.add_space(4.0);

        // Stop button + GoTo text input
        ui.horizontal(|ui| {
            if ui.add_enabled(self.rotor_connected, egui::Button::new("STOP").min_size(egui::vec2(70.0, 30.0))).clicked() {
                let _ = self.cmd_tx.send(Command::RotorStop);
            }

            ui.label("GoTo:");
            let resp = ui.add(egui::TextEdit::singleline(&mut self.rotor_goto_input).desired_width(60.0));
            if (ui.add_enabled(self.rotor_connected, egui::Button::new("Go")).clicked()
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

    pub(super) fn render_yaesu_popout(&mut self, ui: &mut egui::Ui) {
        let mode_label = match self.yaesu_mode {
            0 => "LSB", 1 => "USB", 3 => "CW-L", 4 => "CW-U",
            5 => "FM", 6 => "AM", 7 => "DIGU", 9 => "DIGL",
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

        // Frequency display with scroll-to-tune
        ui.horizontal(|ui| {
            ui.label(RichText::new("A:  ").size(16.0).strong());
            if let Some(delta) = render_freq_scroll(ui, self.yaesu_freq_a) {
                let new_freq = (self.yaesu_freq_a as i64 + delta).max(0) as u64;
                let _ = self.cmd_tx.send(Command::SetYaesuFreq(new_freq));
                self.yaesu_freq_a = new_freq;
            }
        });

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
            let mode_names = ["LSB","USB","CW","CW-R","FM","AM","DIG-U","DIG-L","RTTY","C4FM","DATA-FM","DATA-USB"];
            let mode_codes: &[u8] = &[0, 1, 3, 4, 5, 6, 7, 9, 9, 5, 5, 7];

            // Mode buttons
            ui.horizontal(|ui| {
                ui.label("Mode:");
                for (i, &name) in mode_names.iter().enumerate().take(8) {
                    if ui.add(btn(name)).clicked() {
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
                let split_btn = if self.yaesu_split_active {
                    egui::Button::new(RichText::new("Split").size(11.0).color(Color32::WHITE))
                        .fill(Color32::from_rgb(180, 100, 0)).min_size(egui::vec2(38.0, 20.0))
                } else { btn("Split") };
                if ui.add(split_btn).clicked() {
                    self.yaesu_split_active = !self.yaesu_split_active;
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuButton,
                        if self.yaesu_split_active { 7 } else { 8 }));
                }
                let scan_btn = if self.yaesu_scan_active {
                    egui::Button::new(RichText::new("Scan").size(11.0).color(Color32::WHITE))
                        .fill(Color32::from_rgb(0, 120, 0)).min_size(egui::vec2(38.0, 20.0))
                } else { btn("Scan") };
                if ui.add(scan_btn).clicked() {
                    self.yaesu_scan_active = !self.yaesu_scan_active;
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuButton,
                        if self.yaesu_scan_active { 1 } else { 2 }));
                }
                let tune_btn = if self.yaesu_tuner_active {
                    egui::Button::new(RichText::new("Tune").size(11.0).color(Color32::WHITE))
                        .fill(Color32::from_rgb(180, 0, 0)).min_size(egui::vec2(38.0, 20.0))
                } else { btn("Tune") };
                if ui.add(tune_btn).clicked() {
                    self.yaesu_tuner_active = !self.yaesu_tuner_active;
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuButton,
                        if self.yaesu_tuner_active { 3 } else { 4 }));
                }
            });

            // Sliders: aligned grid layout
            let label_w = 55.0;
            let slider_w = 120.0;
            egui::Grid::new("yaesu_sliders").num_columns(4).spacing([4.0, 2.0]).show(ui, |ui| {
                ui.allocate_space(egui::vec2(label_w, 0.0));
                ui.label("SQL");
                let sql_slider = egui::Slider::new(&mut self.yaesu_squelch, 0..=100)
                    .custom_formatter(|v, _| format!("{:.0}", v));
                if ui.add_sized([slider_w, 16.0], sql_slider).changed() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuSquelch, self.yaesu_squelch));
                    self.yaesu_control_changed_at = Some(Instant::now());
                }

                ui.label("PWR");
                let pwr_slider = egui::Slider::new(&mut self.yaesu_rf_power, 0..=100)
                    .custom_formatter(|v, _| format!("{:.0}W", v));
                if ui.add_sized([slider_w, 16.0], pwr_slider).changed() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuRfPower, self.yaesu_rf_power as u16));
                    self.yaesu_control_changed_at = Some(Instant::now());
                }
                ui.end_row();

                ui.allocate_space(egui::vec2(label_w, 0.0));
                ui.label("RF Gain");
                let rf_slider = egui::Slider::new(&mut self.yaesu_rf_gain, 0..=255)
                    .custom_formatter(|v, _| format!("{:.0}", v));
                if ui.add_sized([slider_w, 16.0], rf_slider).changed() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuRfGain, self.yaesu_rf_gain));
                    self.yaesu_control_changed_at = Some(Instant::now());
                }
                ui.end_row();
            });
        }

        ui.separator();

        // S-meter bar
        {
            let frac = (self.yaesu_smeter as f32 / 255.0).clamp(0.0, 1.0);
            let desired = egui::vec2(ui.available_width().min(350.0), 18.0);
            let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
            if ui.is_rect_visible(rect) {
                let painter = ui.painter();
                painter.rect_filled(rect, 3.0, Color32::from_rgb(30, 30, 30));
                let fill_w = rect.width() * frac;
                let fill_rect = egui::Rect::from_min_size(rect.min, egui::vec2(fill_w, rect.height()));
                let color = if frac < 0.7 { Color32::from_rgb(0, 180, 0) } else { Color32::from_rgb(220, 40, 40) };
                painter.rect_filled(fill_rect, 3.0, color);
                let s_val = (self.yaesu_smeter as f32 / 12.0).min(9.0);
                let text = if s_val >= 9.0 {
                    let db_over = ((self.yaesu_smeter as f32 - 108.0) / (152.0 / 60.0)).max(0.0);
                    format!("S9+{:.0} dB", db_over)
                } else {
                    format!("S{:.0}", s_val)
                };
                painter.text(rect.center(), egui::Align2::CENTER_CENTER,
                    text, egui::FontId::proportional(12.0), Color32::WHITE);
            }
        }

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
            ui.label(if self.yaesu_power_on { "Power ON" } else { "Power OFF" });
        });

        ui.separator();

        // Volume slider
        ui.horizontal(|ui| {
            ui.label("Volume:");
            let slider = egui::Slider::new(&mut self.yaesu_volume, 0.001..=1.0)
                .logarithmic(true)
                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
            if ui.add_sized([140.0, 16.0], slider).changed() {
                let _ = self.cmd_tx.send(Command::SetYaesuVolume(self.yaesu_volume));
            }
        });

        ui.separator();

        // 5-band Equalizer
        egui::CollapsingHeader::new(RichText::new("Equalizer").strong().size(14.0))
            .default_open(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let mut eq_on = self.yaesu_eq_enabled;
                    if ui.checkbox(&mut eq_on, "EQ").changed() {
                        self.yaesu_eq_enabled = eq_on;
                        let _ = self.cmd_tx.send(Command::SetYaesuEqEnabled(eq_on));
                    }
                    // Profile selector
                    let profile_names: Vec<String> = self.yaesu_eq_profiles.iter().map(|(n, _, _)| n.clone()).collect();
                    egui::ComboBox::from_id_salt("eq_profile")
                        .selected_text(if self.yaesu_eq_active_profile.is_empty() { "---" } else { &self.yaesu_eq_active_profile })
                        .width(100.0)
                        .show_ui(ui, |ui| {
                            for name in &profile_names {
                                if ui.selectable_label(&self.yaesu_eq_active_profile == name, name).clicked() {
                                    self.yaesu_eq_active_profile = name.clone();
                                    if let Some((_, en, g)) = self.yaesu_eq_profiles.iter().find(|(n, _, _)| n == name) {
                                        self.yaesu_eq_enabled = *en;
                                        self.yaesu_eq_gains = *g;
                                        let _ = self.cmd_tx.send(Command::SetYaesuEqEnabled(*en));
                                        for i in 0..5 {
                                            let _ = self.cmd_tx.send(Command::SetYaesuEqBand(i as u8, g[i]));
                                        }
                                    }
                                    self.save_full_config();
                                }
                            }
                        });
                    if ui.small_button("Save").clicked() && !self.yaesu_eq_active_profile.is_empty() {
                        let name = self.yaesu_eq_active_profile.clone();
                        if let Some(p) = self.yaesu_eq_profiles.iter_mut().find(|(n, _, _)| *n == name) {
                            p.1 = self.yaesu_eq_enabled;
                            p.2 = self.yaesu_eq_gains;
                        } else {
                            self.yaesu_eq_profiles.push((name, self.yaesu_eq_enabled, self.yaesu_eq_gains));
                        }
                        self.save_full_config();
                    }
                    if ui.small_button("Del").clicked() && !self.yaesu_eq_active_profile.is_empty() {
                        self.yaesu_eq_profiles.retain(|(n, _, _)| n != &self.yaesu_eq_active_profile);
                        self.yaesu_eq_active_profile.clear();
                        self.save_full_config();
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("New:");
                    ui.add(egui::TextEdit::singleline(&mut self.yaesu_eq_new_name).desired_width(100.0));
                    if ui.small_button("+").clicked() && !self.yaesu_eq_new_name.is_empty() {
                        let name = self.yaesu_eq_new_name.clone();
                        self.yaesu_eq_profiles.push((name.clone(), self.yaesu_eq_enabled, self.yaesu_eq_gains));
                        self.yaesu_eq_active_profile = name;
                        self.yaesu_eq_new_name.clear();
                        self.save_full_config();
                    }
                });
                ui.horizontal(|ui| {
                    for i in 0..5 {
                        ui.vertical(|ui| {
                            ui.set_width(50.0);
                            ui.label(egui::RichText::new(sdr_remote_logic::eq::BAND_LABELS[i]).size(10.0));
                            let mut g = self.yaesu_eq_gains[i];
                            let slider = egui::Slider::new(&mut g, -12.0..=12.0)
                                .vertical()
                                .custom_formatter(|v, _| format!("{:+.0}", v));
                            if ui.add_sized([20.0, 60.0], slider).changed() {
                                self.yaesu_eq_gains[i] = g;
                                let _ = self.cmd_tx.send(Command::SetYaesuEqBand(i as u8, g));
                            }
                        });
                    }
                });
            });

        ui.separator();

        // Memory channels
        egui::CollapsingHeader::new(RichText::new("Memory Channels").strong().size(14.0))
            .default_open(false)
            .show(ui, |ui| {
                self.render_yaesu_memories(ui);
            });

        egui::CollapsingHeader::new(RichText::new("Radio Settings (EX Menu)").strong().size(14.0))
            .default_open(false)
            .show(ui, |ui| {
                self.render_yaesu_menu(ui);
            });
    }

    fn render_yaesu_menu(&mut self, ui: &mut egui::Ui) {
        use super::yaesu_menu;

        ui.horizontal(|ui| {
            if ui.button("Read radio").clicked() {
                self.yaesu_menu_received = false;
                let _ = self.cmd_tx.send(Command::SetControl(
                    sdr_remote_core::protocol::ControlId::YaesuReadMenus, 0));
            }
        });

        if self.yaesu_menu_items.is_empty() {
            ui.label("Click 'Read radio' to load all 153 menu settings.");
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
                    ui.label(RichText::new("Setting").strong());
                    ui.label(RichText::new("Value").strong());
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

                        // Value — read-only, enum dropdown, or text
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
                            // Numeric value — show as text, editable
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
            if ui.button("Read radio").clicked() {
                self.yaesu_mem_radio_received = false; // allow processing new data
                let _ = self.cmd_tx.send(Command::SetControl(
                    sdr_remote_core::protocol::ControlId::YaesuReadMemories, 0));
            }
            if !self.yaesu_mem_channels.is_empty() {
                if ui.button("Write radio").clicked() {
                    let path = std::path::Path::new(&self.yaesu_mem_file);
                    // Save to file first, then send to server for writing
                    let _ = yaesu_memory::save_tab_file(path, &self.yaesu_mem_channels);
                    if let Ok(text) = std::fs::read_to_string(path) {
                        let _ = self.cmd_tx.send(Command::WriteYaesuMemories(text));
                    }
                }
            }
            if ui.button("Load file").clicked() {
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
                if ui.button("Save").clicked() {
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
        });

        // File path
        ui.horizontal(|ui| {
            ui.label("File:");
            ui.add(egui::TextEdit::singleline(&mut self.yaesu_mem_file).desired_width(250.0));
        });

        // Filter
        ui.horizontal(|ui| {
            ui.label("Filter:");
            ui.add(egui::TextEdit::singleline(&mut self.yaesu_mem_filter).desired_width(150.0));
            if !self.yaesu_mem_filter.is_empty() {
                if ui.button("×").clicked() {
                    self.yaesu_mem_filter.clear();
                }
            }
            ui.label(format!("{} channels", self.yaesu_mem_channels.len()));
        });

        if self.yaesu_mem_channels.is_empty() {
            ui.label("No channels loaded. Set file path and click Load.");
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
            let on_off = |v: bool| if v { "On" } else { "—" };

            egui::Grid::new("yaesu_mem_grid")
                .striped(true)
                .min_col_width(28.0)
                .spacing([6.0, 3.0])
                .num_columns(17)
                .show(ui, |ui| {
                    // Header row
                    for h in &["CH", "Name", "RX Freq", "Mode", "Dir", "Offset", "Tone", "CTCSS/DCS",
                               "AGC", "NB", "DNR", "IPO", "ATT", "Tuner", "Skip", "Step", ""] {
                        ui.label(header_style(h));
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
                                    .width(70.0).selected_text(if ch.offset_freq.is_empty() { "—" } else { &ch.offset_freq })
                                    .show_ui(ui, |ui| {
                                        for &f in yaesu_memory::OFFSET_FREQS {
                                            let label = if f.is_empty() { "—" } else { f };
                                            if ui.selectable_label(ch.offset_freq == f, label).clicked() {
                                                ch.offset_freq = f.to_string();
                                                self.yaesu_mem_dirty = true;
                                            }
                                        }
                                    });
                            } else {
                                ui.label("—");
                            }

                            // Tone mode
                            egui::ComboBox::from_id_salt(format!("mt_{}", idx))
                                .width(55.0).selected_text(if ch.tone_mode == "None" { "—" } else { &ch.tone_mode })
                                .show_ui(ui, |ui| {
                                    for &t in yaesu_memory::TONE_MODES {
                                        let l = if t == "None" { "—" } else { t };
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
                            } else { ui.label("—"); }

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
                                if ui.small_button("Tune").clicked() {
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
                            ui.label(RichText::new(if ch.offset_freq.is_empty() { "—" } else { &ch.offset_freq }).color(c));

                            ui.label(RichText::new(if ch.tone_mode == "None" { "—" } else { &ch.tone_mode }).color(c));

                            let tone_val = match ch.tone_mode.as_str() {
                                "Tone" | "Tone ENC" | "T SQL" => ch.ctcss.clone(),
                                "DCS" | "DCS ENC" | "D Code" => ch.dcs.clone(),
                                _ => "—".into(),
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

                            if ui.small_button("Edit").clicked() {
                                self.yaesu_mem_selected = Some(idx);
                            }

                            ui.end_row();
                        }
                    }
                });
        });

        // Execute deferred actions: recall memory channel only.
        // FM → DATA-FM switch happens transparently at PTT time (server-side).
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
        }
    }

    pub(super) fn render_device_yaesu(&mut self, ui: &mut egui::Ui, _amber: Color32) {
        ui.horizontal(|ui| {
            ui.heading("Yaesu FT-991A");
            ui.separator();
            if ui.checkbox(&mut self.yaesu_enabled, "Enable").changed() {
                let _ = self.cmd_tx.send(Command::SetControl(
                    sdr_remote_core::protocol::ControlId::YaesuEnable, self.yaesu_enabled as u16));
            }
            ui.separator();
            if self.yaesu_enabled {
                let popout_label = if self.yaesu_popout { "Close window" } else { "Open window" };
                if ui.button(popout_label).clicked() {
                    self.yaesu_popout = !self.yaesu_popout;
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("PTT:");
            if ui.selectable_label(!self.yaesu_ptt_toggle_mode, "Push to talk").clicked() {
                self.yaesu_ptt_toggle_mode = false;
                self.save_ptt_config();
            }
            if ui.selectable_label(self.yaesu_ptt_toggle_mode, "Toggle").clicked() {
                self.yaesu_ptt_toggle_mode = true;
                self.save_ptt_config();
            }
        });
        ui.separator();

        let mode_label = match self.yaesu_mode {
            0 => "LSB", 1 => "USB", 3 => "CW-L", 4 => "CW-U",
            5 => "FM", 6 => "AM", 7 => "DIGU", 9 => "DIGL",
            _ => "?",
        };

        egui::Grid::new("yaesu_grid")
            .num_columns(2)
            .spacing([20.0, 6.0])
            .show(ui, |ui| {
                ui.label("VFO A:");
                ui.label(RichText::new(format!("{} Hz", format_frequency(self.yaesu_freq_a)))
                    .size(18.0).strong());
                ui.end_row();

                ui.label("VFO B:");
                ui.label(RichText::new(format!("{} Hz", format_frequency(self.yaesu_freq_b)))
                    .size(14.0));
                ui.end_row();

                ui.label("Mode:");
                ui.label(RichText::new(mode_label).size(14.0).strong());
                ui.end_row();

                ui.label("Power:");
                ui.label(if self.yaesu_power_on { "ON" } else { "OFF" });
                ui.end_row();

                ui.label("TX:");
                ui.label(if self.yaesu_tx_active {
                    RichText::new("TX").color(Color32::RED).strong()
                } else {
                    RichText::new("RX").color(Color32::GREEN)
                });
                ui.end_row();

                ui.label("S-Meter:");
                ui.label(format!("{}", self.yaesu_smeter));
                ui.end_row();
            });

        ui.separator();

        // Yaesu audio volume
        ui.horizontal(|ui| {
            ui.label("Audio:");
            let slider = egui::Slider::new(&mut self.yaesu_volume, 0.001..=1.0)
                .logarithmic(true)
                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
            if ui.add(slider).changed() {
                let _ = self.cmd_tx.send(Command::SetYaesuVolume(self.yaesu_volume));
            }
        });
    }
}
