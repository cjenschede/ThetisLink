// SPDX-License-Identifier: GPL-2.0-or-later
//! `SdrRemoteApp::render_diversity`: the diversity-reception screen - the phase/gain
//! combiner controls and its display. Extracted verbatim from `ui/screens.rs` - pure
//! relocation, no behaviour change. `pub(super)` keeps it callable from the parent
//! module tree.

use super::*;

impl SdrRemoteApp {
    pub(super) fn render_diversity(&mut self, ui: &mut egui::Ui) {
        use sdr_remote_core::protocol::ControlId;

        let gain_max = self.diversity_gain_multi;

        // Enable + dropdowns row
        ui.horizontal(|ui| {
            if ui.add(egui::Button::new(
                if self.diversity_enabled {
                    RichText::new(rust_i18n::t!("screen_diversity_on").to_string()).color(Color32::WHITE)
                } else {
                    RichText::new(rust_i18n::t!("screen_diversity_off").to_string())
                })
                .fill(if self.diversity_enabled { Color32::from_rgb(0, 120, 0) } else { Color32::from_rgb(60, 60, 60) })
                .min_size(egui::vec2(100.0, 24.0))
            ).clicked() {
                self.diversity_enabled = !self.diversity_enabled;
                let _ = self.cmd_tx.send(Command::SetControl(
                    ControlId::DiversityEnable, self.diversity_enabled as u16));
            }
            ui.separator();
            ui.label("Ref:");
            egui::ComboBox::from_id_salt("div_ref")
                .width(60.0)
                .selected_text(if self.diversity_ref == 1 { "RX1" } else { "RX2" })
                .show_ui(ui, |ui| {
                    if ui.selectable_label(self.diversity_ref == 1, "RX1").clicked() {
                        self.diversity_ref = 1;
                        let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityRef, 1));
                    }
                    if ui.selectable_label(self.diversity_ref == 0, "RX2").clicked() {
                        self.diversity_ref = 0;
                        let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityRef, 0));
                    }
                });
            ui.separator();
            ui.label(rust_i18n::t!("screen_source").to_string());
            egui::ComboBox::from_id_salt("div_src")
                .width(80.0)
                .selected_text(match self.diversity_source { 0 => "RX1+RX2", 1 => "RX1", _ => "RX2" })
                .show_ui(ui, |ui| {
                    for (val, label) in [(0u16, "RX1+RX2"), (1, "RX1"), (2, "RX2")] {
                        if ui.selectable_label(self.diversity_source == val, label).clicked() {
                            self.diversity_source = val;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversitySource, val));
                        }
                    }
                });
        });

        ui.add_space(4.0);

        // X/Y plot + sliders side by side
        ui.horizontal(|ui| {
            // === Diversity X/Y circle ===
            let circle_size = 200.0;
            let (rect, response) = ui.allocate_exact_size(
                egui::vec2(circle_size, circle_size), egui::Sense::click_and_drag());

            if ui.is_rect_visible(rect) {
                let painter = ui.painter_at(rect);
                let center = rect.center();
                let radius = circle_size * 0.42;

                // Background
                painter.rect_filled(rect, 4.0, Color32::from_rgb(15, 15, 25));

                // Concentric circles (gain rings)
                for i in 1..=4 {
                    let r = radius * i as f32 / 4.0;
                    painter.circle_stroke(center, r, egui::Stroke::new(0.5, Color32::from_rgb(35, 35, 50)));
                }
                // Outer circle
                painter.circle_stroke(center, radius, egui::Stroke::new(1.5, Color32::from_rgb(60, 60, 90)));

                // Cross axes
                let axis_color = Color32::from_rgb(50, 50, 70);
                painter.line_segment(
                    [egui::pos2(center.x - radius, center.y), egui::pos2(center.x + radius, center.y)],
                    egui::Stroke::new(0.8, axis_color));
                painter.line_segment(
                    [egui::pos2(center.x, center.y - radius), egui::pos2(center.x, center.y + radius)],
                    egui::Stroke::new(0.8, axis_color));

                // Vector: phase = angle from positive X axis, gain = length
                let phase_rad = self.diversity_phase.to_radians();
                let non_ref_gain = if self.diversity_ref == 1 { self.diversity_gain_rx2 } else { self.diversity_gain_rx1 };
                let gain_norm = (non_ref_gain / gain_max).clamp(0.0, 1.0);
                let tip_x = center.x + phase_rad.cos() * radius * gain_norm;
                let tip_y = center.y - phase_rad.sin() * radius * gain_norm;

                // Vector line (green)
                painter.line_segment(
                    [center, egui::pos2(tip_x, tip_y)],
                    egui::Stroke::new(2.5, Color32::from_rgb(0, 200, 0)));

                // Tip circle
                painter.circle_filled(egui::pos2(tip_x, tip_y), 6.0, Color32::from_rgb(0, 255, 50));
                painter.circle_stroke(egui::pos2(tip_x, tip_y), 6.0, egui::Stroke::new(1.0, Color32::WHITE));

                // Center dot
                painter.circle_filled(center, 3.0, Color32::from_rgb(120, 120, 150));

                // Axis labels
                let label_color = Color32::from_rgb(130, 130, 160);
                let font = egui::FontId::proportional(10.0);
                painter.text(egui::pos2(center.x, rect.top() + 3.0), egui::Align2::CENTER_TOP, "0°", font.clone(), label_color);
                painter.text(egui::pos2(rect.right() - 3.0, center.y), egui::Align2::RIGHT_CENTER, "90°", font.clone(), label_color);
                painter.text(egui::pos2(center.x, rect.bottom() - 3.0), egui::Align2::CENTER_BOTTOM, "±180°", font.clone(), label_color);
                painter.text(egui::pos2(rect.left() + 3.0, center.y), egui::Align2::LEFT_CENTER, "-90°", font.clone(), label_color);

                // Value readout
                let phase_s = format!("{:.1}", self.diversity_phase);
                let gain_s = format!("{:.3}", non_ref_gain);
                painter.text(egui::pos2(rect.left() + 4.0, rect.top() + 3.0), egui::Align2::LEFT_TOP,
                    rust_i18n::t!("screen_phase_gain_readout", phase = phase_s, gain = gain_s).to_string(),
                    egui::FontId::proportional(11.0), Color32::from_rgb(200, 200, 220));
            }

            // Handle mouse click/drag in circle - only send when diversity enabled
            if (response.dragged() || response.clicked()) && self.diversity_enabled {
                if let Some(pos) = response.interact_pointer_pos() {
                    let center = rect.center();
                    let radius = circle_size * 0.42;
                    let xf = (pos.x - center.x) / radius;
                    let yf = -(pos.y - center.y) / radius;

                    let r = (xf * xf + yf * yf).sqrt().clamp(0.0, 1.0);
                    let angle = yf.atan2(xf);

                    // Apply phase (unless locked)
                    if !self.diversity_phase_lock {
                        self.diversity_phase = angle.to_degrees();
                        let phase_encoded = ((self.diversity_phase * 100.0) as i32 + 18000) as u16;
                        let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityPhase, phase_encoded));
                    }

                    // Apply gain (unless locked)
                    if !self.diversity_gain_lock {
                        let gain_val = r * gain_max;
                        let val = (gain_val * 1000.0) as u16;
                        if self.diversity_ref == 1 {
                            self.diversity_gain_rx2 = gain_val;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityGainRx2, val));
                        } else {
                            self.diversity_gain_rx1 = gain_val;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityGainRx1, val));
                        }
                    }
                }
            }

            ui.separator();

            // === Sliders column ===
            ui.vertical(|ui| {
                ui.label(rust_i18n::t!("screen_gain_multi").to_string());
                let gm_slider = egui::Slider::new(&mut self.diversity_gain_multi, 1.0..=10.0)
                    .custom_formatter(|v, _| format!("{:.0}", v))
                    .step_by(1.0);
                let gm_resp = ui.add_sized([160.0, 16.0], gm_slider);
                let gm_scrolled = super::helpers::slider_wheel(ui, &gm_resp, &mut self.diversity_gain_multi, 1.0..=10.0, 1.0);
                if gm_resp.changed() || gm_scrolled {
                    let val = (self.diversity_gain_multi * 100.0).clamp(100.0, 1000.0) as u16;
                    let _ = self.cmd_tx.send(Command::SetControl(
                        ControlId::DiversityGainMulti, val));
                }

                let rx1_is_ref = self.diversity_ref == 1;

                ui.add_space(6.0);
                ui.label(if rx1_is_ref { "RX1 Gain (ref):" } else { "RX1 Gain:" });
                if rx1_is_ref {
                    self.diversity_gain_rx1 = 1.0;
                    ui.add_enabled(false, egui::Slider::new(&mut self.diversity_gain_rx1, 0.0..=10.0)
                        .custom_formatter(|v, _| format!("{:.3}", v)));
                } else {
                    let g1_slider = egui::Slider::new(&mut self.diversity_gain_rx1, 0.0..=10.0)
                        .custom_formatter(|v, _| format!("{:.3}", v))
                        .step_by(0.001);
                    let resp = ui.add_sized([160.0, 16.0], g1_slider);
                    let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.diversity_gain_rx1, 0.0..=10.0, 0.1);
                    if (resp.changed() || scrolled) && self.diversity_enabled {
                        let val = (self.diversity_gain_rx1 * 1000.0) as u16;
                        let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityGainRx1, val));
                    }
                }

                ui.add_space(4.0);
                ui.label(if !rx1_is_ref { "RX2 Gain (ref):" } else { "RX2 Gain:" });
                if !rx1_is_ref {
                    self.diversity_gain_rx2 = 1.0;
                    ui.add_enabled(false, egui::Slider::new(&mut self.diversity_gain_rx2, 0.0..=10.0)
                        .custom_formatter(|v, _| format!("{:.3}", v)));
                } else {
                    let g2_slider = egui::Slider::new(&mut self.diversity_gain_rx2, 0.0..=10.0)
                        .custom_formatter(|v, _| format!("{:.3}", v))
                        .step_by(0.001);
                    let resp = ui.add_sized([160.0, 16.0], g2_slider);
                    let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.diversity_gain_rx2, 0.0..=10.0, 0.1);
                    if (resp.changed() || scrolled) && self.diversity_enabled {
                        let val = (self.diversity_gain_rx2 * 1000.0) as u16;
                        let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityGainRx2, val));
                    }
                }

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.diversity_gain_lock, rust_i18n::t!("screen_lock_gain").to_string());
                    ui.checkbox(&mut self.diversity_phase_lock, rust_i18n::t!("screen_lock_phase").to_string());
                });

                ui.add_space(4.0);
                ui.label(rust_i18n::t!("screen_phase").to_string());
                let phase_slider = egui::Slider::new(&mut self.diversity_phase, -180.0..=180.0)
                    .custom_formatter(|v, _| format!("{:.1}°", v))
                    .step_by(0.1);
                let resp = ui.add_sized([160.0, 16.0], phase_slider);
                let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.diversity_phase, -180.0..=180.0, 1.0);
                if (resp.changed() || scrolled) && self.diversity_enabled {
                    let encoded = ((self.diversity_phase * 100.0) as i32 + 18000) as u16;
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityPhase, encoded));
                }

                ui.add_space(6.0);
                // Auto-null button with result color
                if self.diversity_auto_active {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        let label = if self.diversity_auto_smart {
                            rust_i18n::t!("screen_smart_thetis_side").to_string()
                        } else if self.diversity_auto_slow {
                            let param = if self.diversity_sa_param == 0 { "Phase" } else { "Gain" };
                            format!("SA {} iter {} step {:.1}", param, self.diversity_sa_iteration + 1, self.diversity_sa_step)
                        } else {
                            rust_i18n::t!("screen_round_n", n = self.diversity_auto_round + 1).to_string()
                        };
                        ui.label(label);
                        if ui.add(egui::Button::new(rust_i18n::t!("screen_stop").to_string())
                            .fill(Color32::from_rgb(200, 120, 0))).clicked() {
                            self.diversity_auto_active = false;
                            self.diversity_auto_result = 0;
                            // Note: Thetis-side autonull can't be stopped mid-run
                        }
                    });
                } else {
                    let (btn_color, btn_text) = match self.diversity_auto_result {
                        2 => {
                            let db = format!("{:+.1}", -self.diversity_auto_improvement_db);
                            (Color32::from_rgb(0, 140, 0), rust_i18n::t!("screen_auto_null_db", db = db).to_string())
                        }
                        3 => (Color32::from_rgb(140, 0, 0), rust_i18n::t!("screen_auto_null_no_gain").to_string()),
                        _ => (Color32::from_rgb(60, 60, 60), rust_i18n::t!("screen_auto_null").to_string()),
                    };
                    ui.horizontal(|ui| {
                        if ui.add(egui::Button::new(RichText::new(&btn_text).color(Color32::WHITE))
                            .fill(btn_color)).clicked() {
                            let smeter_dbm = self.smeter;
                            self.diversity_auto_start_smeter = smeter_dbm;
                            self.diversity_auto_overall_best = 999.0;
                            self.diversity_auto_active = true;
                            self.diversity_auto_result = 1;
                            self.diversity_auto_round = 0;
                            self.diversity_auto_step = 0;
                            self.diversity_auto_best_smeter = 999.0;
                            self.diversity_auto_best_gain = 1.0;
                            self.diversity_auto_best_phase = 0.0;
                            self.diversity_auto_eq_gain_db = if self.diversity_auto_slow { f32::MAX } else { 0.0 };
                            // SA state reset
                            self.diversity_sa_param = 0;
                            self.diversity_sa_step = 90.0;
                            self.diversity_sa_sub = 0;
                            self.diversity_sa_iteration = 0;
                            self.diversity_auto_last_set = Instant::now();
                        }
                        let mode_label = if self.diversity_auto_ultra { "Ultra" } else if self.diversity_auto_smart { "Smart" } else if self.diversity_auto_slow { "Slow" } else { "Fast" };
                        egui::ComboBox::from_id_salt("auto_null_mode")
                            .selected_text(mode_label)
                            .width(55.0)
                            .show_ui(ui, |ui| {
                                if ui.selectable_label(!self.diversity_auto_slow && !self.diversity_auto_smart && !self.diversity_auto_ultra, "Fast").clicked() {
                                    self.diversity_auto_slow = false;
                                    self.diversity_auto_smart = false;
                                    self.diversity_auto_ultra = false;
                                }
                                if ui.selectable_label(self.diversity_auto_slow && !self.diversity_auto_smart && !self.diversity_auto_ultra, "Slow").clicked() {
                                    self.diversity_auto_slow = true;
                                    self.diversity_auto_smart = false;
                                    self.diversity_auto_ultra = false;
                                }
                                if ui.selectable_label(self.diversity_auto_smart && !self.diversity_auto_ultra, "Smart").clicked() {
                                    self.diversity_auto_slow = true;
                                    self.diversity_auto_smart = true;
                                    self.diversity_auto_ultra = false;
                                }
                                if ui.selectable_label(self.diversity_auto_ultra, "Ultra").clicked() {
                                    self.diversity_auto_slow = true;
                                    self.diversity_auto_smart = true;
                                    self.diversity_auto_ultra = true;
                                }
                            });
                    });
                }
            });
        });

        // State.smeter is already dBm (in RX context, which is the only context
        // diversity auto-null runs in). No conversion needed.

        // Auto-null state machine (runs each frame when active)
        if self.diversity_auto_active && (self.diversity_enabled || self.diversity_auto_result >= 4 || self.diversity_auto_eq_gain_db == f32::MAX || self.diversity_auto_smart) {
            use sdr_remote_core::protocol::ControlId;
            // Settle time: 350ms base (S-meter smoothing) + RTT
            let rtt = self.rtt_ms as u128;
            let settle_ms: u128 = if self.diversity_auto_result >= 4 || self.diversity_auto_eq_gain_db == f32::MAX {
                1000 + rtt
            } else {
                350 + rtt
            };
            let smart_waiting = self.diversity_auto_smart && self.diversity_sa_sub == 1;
            if smart_waiting || self.diversity_auto_last_set.elapsed().as_millis() >= settle_ms {
                let smeter_dbm = self.smeter;
                if smeter_dbm < self.diversity_auto_overall_best {
                    self.diversity_auto_overall_best = smeter_dbm;
                }

                // Define rounds: (gain, phase_center, phase_range, phase_step)
                // Fast: 3 rounds. Slow: 7 rounds with iterative refinement.
                struct Round { gain: f32, phase_range: f32, phase_step: f32, is_gain_sweep: bool, gain_step: f32 }
                let gain_max = self.diversity_gain_multi;
                let best_phase = self.diversity_auto_best_phase;

                let fast_rounds = vec![
                    Round { gain: 0.0, phase_range: 180.0, phase_step: 5.0,  is_gain_sweep: false, gain_step: 0.0 },
                    Round { gain: 0.0, phase_range: 10.0,  phase_step: 1.0,  is_gain_sweep: false, gain_step: 0.0 },
                    Round { gain: 0.0, phase_range: 0.0,   phase_step: 0.0,  is_gain_sweep: true,  gain_step: 0.2 },
                ];
                // Slow: equalize-based algorithm
                // Round 0: equalize (handled separately below before round processing)
                // Round 1: coarse phase 360° in 45° steps at equalized gain
                // Round 2: gain sweep ±3dB around equalized gain in 1dB steps
                // Round 3: phase sweep ±45° around best in 10° steps
                // Round 4: fine gain ±1.5dB in 0.25dB steps
                // Round 5: fine phase ±15° in 2° steps
                let slow_rounds = vec![
                    // Step 2: coarse phase 360° at equalized gain
                    Round { gain: 0.0, phase_range: 180.0, phase_step: 45.0, is_gain_sweep: false, gain_step: 0.0 },
                    // Step 3: gain ±3dB in 1dB steps
                    Round { gain: 0.0, phase_range: 0.0,   phase_step: 0.0,  is_gain_sweep: true,  gain_step: 0.0 },
                    // Step 4: phase ±45° in 10° steps
                    Round { gain: 0.0, phase_range: 45.0,  phase_step: 10.0, is_gain_sweep: false, gain_step: 0.0 },
                    // Step 5: gain ±1.5dB in 0.25dB steps
                    Round { gain: 0.0, phase_range: 0.0,   phase_step: 0.0,  is_gain_sweep: true,  gain_step: 0.0 },
                    // Step 6: phase ±15° in 3° steps
                    Round { gain: 0.0, phase_range: 15.0,  phase_step: 3.0,  is_gain_sweep: false, gain_step: 0.0 },
                    // Step 7: gain ±0.75dB in 0.1dB steps
                    Round { gain: 0.0, phase_range: 0.0,   phase_step: 0.0,  is_gain_sweep: true,  gain_step: 0.0 },
                    // Step 8: phase ±5° in 1° steps
                    Round { gain: 0.0, phase_range: 5.0,   phase_step: 1.0,  is_gain_sweep: false, gain_step: 0.0 },
                    // Step 9: gain ±0.3dB in 0.05dB steps
                    Round { gain: 0.0, phase_range: 0.0,   phase_step: 0.0,  is_gain_sweep: true,  gain_step: 0.0 },
                ];
                let rounds = if self.diversity_auto_slow { &slow_rounds } else { &fast_rounds };

                // Slow mode: equalize step (before round 0)
                if self.diversity_auto_slow && self.diversity_auto_eq_gain_db == f32::MAX && self.diversity_auto_result == 1 {
                    // Step 1: Read individual RX1/RX2 S-meters (TCI sensors are per-receiver)
                    // Ensure diversity is on so both receivers are active
                    if !self.diversity_enabled {
                        self.diversity_enabled = true;
                        let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityEnable, 1));
                        // Set gain to 0 temporarily so non-ref doesn't affect measurement
                        if self.diversity_ref == 1 {
                            self.diversity_gain_rx2 = 0.0;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityGainRx2, 0));
                        } else {
                            self.diversity_gain_rx1 = 0.0;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityGainRx1, 0));
                        }
                        self.diversity_auto_last_set = Instant::now();
                    } else {
                        // Both receivers active - read S-meters
                        let rx1_dbm = self.smeter;
                        let rx2_dbm = self.rx2_smeter;
                        // Non-ref needs gain to match ref: gain = ref_dBm - nonref_dBm
                        let (ref_dbm, nonref_dbm) = if self.diversity_ref == 1 {
                            (rx1_dbm, rx2_dbm) // RX1 is ref, boost RX2
                        } else {
                            (rx2_dbm, rx1_dbm) // RX2 is ref, boost RX1
                        };
                        let diff_db = ref_dbm - nonref_dbm; // positive = non-ref is weaker -> needs boost
                        self.diversity_auto_eq_gain_db = diff_db;
                        let eq_gain = 10.0f32.powf(diff_db / 20.0).clamp(0.1, 10.0);
                        self.diversity_auto_best_gain = eq_gain;
                        log::info!("Auto-null STEP 1 equalize:");
                        log::info!("  RX1={:.1}dBm  RX2={:.1}dBm", rx1_dbm, rx2_dbm);
                        log::info!("  Ref=RX{}  NonRef=RX{}", if self.diversity_ref == 1 { 1 } else { 2 }, if self.diversity_ref == 1 { 2 } else { 1 });
                        log::info!("  Ref={:.1}dBm  NonRef={:.1}dBm  diff={:.1}dB", ref_dbm, nonref_dbm, diff_db);
                        log::info!("  Equalized gain={:.3} (linear) = {:.1}dB", eq_gain, diff_db);
                        log::info!("  Expected combined: ~{:.1}dBm (~3dB above ref)", ref_dbm + 3.0);
                        // Set gain and turn diversity on
                        let val = (eq_gain * 1000.0) as u16;
                        if self.diversity_ref == 1 {
                            self.diversity_gain_rx2 = eq_gain;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityGainRx2, val));
                        } else {
                            self.diversity_gain_rx1 = eq_gain;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityGainRx1, val));
                        }
                        // Also update gain_multi if eq_gain exceeds it
                        if eq_gain > self.diversity_gain_multi {
                            self.diversity_gain_multi = (eq_gain * 1.5).min(10.0);
                        }
                        self.diversity_enabled = true;
                        let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityEnable, 1));
                        // eq_gain_db is no longer MAX, so equalize won't re-run
                        self.diversity_auto_last_set = Instant::now();
                    }
                } else if self.diversity_auto_smart && self.diversity_auto_round < 999 {
                    // Smart/Ultra mode: send autonull command to Thetis (runs server-side)
                    // value 1=Smart, 2=Ultra
                    use sdr_remote_core::protocol::ControlId;
                    if self.diversity_auto_round == 0 && self.diversity_sa_sub == 0 {
                        // Remember current result to detect when it changes
                        self.diversity_sa_center_smeter = self.state_rx.borrow().diversity_autonull_result as f32;
                        let mode_val = if self.diversity_auto_ultra { 2u16 } else { 1u16 };
                        let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityAutoNull, mode_val));
                        self.diversity_sa_sub = 1;
                        self.diversity_auto_round = 1;
                        self.diversity_auto_last_set = Instant::now();
                    }
                    // Check for NEW done signal (different from initial value)
                    let autonull_result = self.state_rx.borrow().diversity_autonull_result;
                    let initial = self.diversity_sa_center_smeter as u16;
                    if autonull_result > 0 && autonull_result != initial {
                        let improvement = (autonull_result.wrapping_sub(32000) as i16) as f32 / 10.0;
                        self.diversity_auto_improvement_db = improvement;
                        self.diversity_auto_active = false;
                        self.diversity_auto_result = if improvement > 0.5 { 2 } else { 3 };
                        log::info!("Smart: Thetis autonull done, improvement={:.1}dB", improvement);
                    }
                    // Timeout after 60s
                    if self.diversity_auto_last_set.elapsed().as_secs() > 60 {
                        log::warn!("Smart: timeout waiting for Thetis autonull");
                        self.diversity_auto_active = false;
                        self.diversity_auto_result = 3;
                    }
                } else if self.diversity_auto_smart && self.diversity_auto_round >= 999 {
                    // Smart done - handled by measurement phase below

                    let set_gain_fn = |s: &mut Self, gain: f32| {
                        let g = gain.clamp(0.05, 10.0);
                        let val = (g * 1000.0) as u16;
                        if s.diversity_ref == 1 {
                            s.diversity_gain_rx2 = g;
                            let _ = s.cmd_tx.send(Command::SetControl(ControlId::DiversityGainRx2, val));
                        } else {
                            s.diversity_gain_rx1 = g;
                            let _ = s.cmd_tx.send(Command::SetControl(ControlId::DiversityGainRx1, val));
                        }
                    };
                    let set_phase_fn = |s: &mut Self, phase: f32| {
                        let mut p = phase;
                        while p > 180.0 { p -= 360.0; }
                        while p < -180.0 { p += 360.0; }
                        s.diversity_phase = p;
                        let encoded = ((p * 100.0) as i32 + 18000) as u16;
                        let _ = s.cmd_tx.send(Command::SetControl(ControlId::DiversityPhase, encoded));
                    };

                    // Step sequence: (is_phase, offsets_in_degrees_or_dB)
                    // Load steps from diversity-smart.txt (or use defaults)
                    let loaded_steps = crate::ui::config::load_smart_steps();
                    let default_steps: Vec<(Vec<f32>, bool)> = vec![
                        (vec![-180.0, -135.0, -90.0, -45.0, 0.0, 45.0, 90.0, 135.0], true),
                        (vec![-4.0, 4.0], false),
                        (vec![-90.0, -45.0, 45.0, 90.0], true),
                        (vec![-2.0, 2.0], false),
                        (vec![-45.0, -23.0, 23.0, 45.0], true),
                        (vec![-1.0, 1.0], false),
                        (vec![-10.0, 10.0], true),
                        (vec![-0.5, 0.5], false),
                        (vec![-5.0, 5.0], true),
                    ];
                    let steps_vec = if loaded_steps.is_empty() { &default_steps } else { &loaded_steps };
                    let steps: Vec<(&[f32], bool)> = steps_vec.iter().map(|(v, b)| (v.as_slice(), *b)).collect();

                    let round = self.diversity_auto_round;
                    if round >= steps.len() {
                        // All steps done -> measurement phase
                        self.diversity_auto_round = 999;
                        self.diversity_auto_result = 1;
                    } else {
                        let &(offsets, is_phase) = &steps[round];
                        let step_idx = self.diversity_auto_step;
                        // sub 0 = set offset, sub 1 = measure result
                        let sub = self.diversity_sa_sub;

                        if step_idx == 0 && sub == 0 {
                            // Start of round: record current as baseline
                            self.diversity_auto_best_smeter = smeter_dbm;
                            self.diversity_sa_sub = 0;
                        }

                        if sub == 0 {
                            // Set the offset for this step
                            if step_idx < offsets.len() {
                                if is_phase {
                                    let phase = if round == 0 {
                                        offsets[step_idx] // absolute for first sweep
                                    } else {
                                        self.diversity_auto_best_phase + offsets[step_idx]
                                    };
                                    set_phase_fn(self, phase);
                                } else {
                                    let cur_db = 20.0 * self.diversity_auto_best_gain.max(0.01).log10();
                                    let new_gain = 10.0f32.powf((cur_db + offsets[step_idx]) / 20.0);
                                    set_gain_fn(self, new_gain);
                                }
                                self.diversity_sa_sub = 1; // next tick: measure
                            } else {
                                // Round complete - apply best, advance
                                set_phase_fn(self, self.diversity_auto_best_phase);
                                set_gain_fn(self, self.diversity_auto_best_gain);
                                log::info!("Smart round {}: phase={:.1}° gain={:.3} best={:.1}dBm",
                                    round + 1, self.diversity_auto_best_phase, self.diversity_auto_best_gain, self.diversity_auto_best_smeter);
                                self.diversity_auto_round = round + 1;
                                self.diversity_auto_step = 0;
                                self.diversity_sa_sub = 0;
                            }
                        } else {
                            // Measure: compare with best
                            let offset = offsets[step_idx];
                            if smeter_dbm < self.diversity_auto_best_smeter {
                                self.diversity_auto_best_smeter = smeter_dbm;
                                if is_phase {
                                    self.diversity_auto_best_phase = if round == 0 { offset } else { self.diversity_auto_best_phase + offset };
                                } else {
                                    let cur_db = 20.0 * self.diversity_auto_best_gain.max(0.01).log10();
                                    self.diversity_auto_best_gain = 10.0f32.powf((cur_db + offset) / 20.0).clamp(0.05, 10.0);
                                }
                            }
                            self.diversity_auto_step = step_idx + 1;
                            self.diversity_sa_sub = 0;
                        }
                    }
                    self.diversity_auto_last_set = Instant::now();
                } else if self.diversity_auto_slow && !self.diversity_auto_smart && self.diversity_auto_result == 1 && self.diversity_sa_iteration < 3 {
                    // Successive approximation mode (Slow)
                    use sdr_remote_core::protocol::ControlId;
                    let smeter_dbm = self.smeter;
                    if smeter_dbm < self.diversity_auto_overall_best {
                        self.diversity_auto_overall_best = smeter_dbm;
                    }

                    // Helper: set gain on non-ref receiver
                    let set_gain = |s: &mut Self, gain: f32| {
                        let g = gain.clamp(0.05, 10.0);
                        let val = (g * 1000.0) as u16;
                        if s.diversity_ref == 1 {
                            s.diversity_gain_rx2 = g;
                            let _ = s.cmd_tx.send(Command::SetControl(ControlId::DiversityGainRx2, val));
                        } else {
                            s.diversity_gain_rx1 = g;
                            let _ = s.cmd_tx.send(Command::SetControl(ControlId::DiversityGainRx1, val));
                        }
                    };
                    let set_phase = |s: &mut Self, phase: f32| {
                        let mut p = phase;
                        while p > 180.0 { p -= 360.0; }
                        while p < -180.0 { p += 360.0; }
                        s.diversity_phase = p;
                        let encoded = ((p * 100.0) as i32 + 18000) as u16;
                        let _ = s.cmd_tx.send(Command::SetControl(ControlId::DiversityPhase, encoded));
                    };

                    let is_phase = self.diversity_sa_param == 0;
                    let step = self.diversity_sa_step;
                    let min_step = if is_phase { 1.0 } else { 0.1 }; // 1° or 0.1dB

                    match self.diversity_sa_sub {
                        0 => {
                            // Measure center (current position)
                            self.diversity_sa_center_smeter = smeter_dbm;
                            // Set +step
                            if is_phase {
                                set_phase(self, self.diversity_auto_best_phase + step);
                            } else {
                                let cur_db = 20.0 * self.diversity_auto_best_gain.max(0.01).log10();
                                let new_gain = 10.0f32.powf((cur_db + step) / 20.0);
                                set_gain(self, new_gain);
                            }
                            self.diversity_sa_sub = 1;
                        }
                        1 => {
                            // Measure +step
                            self.diversity_sa_plus_smeter = smeter_dbm;
                            // Set -step
                            if is_phase {
                                set_phase(self, self.diversity_auto_best_phase - step);
                            } else {
                                let cur_db = 20.0 * self.diversity_auto_best_gain.max(0.01).log10();
                                let new_gain = 10.0f32.powf((cur_db - step) / 20.0);
                                set_gain(self, new_gain);
                            }
                            self.diversity_sa_sub = 2;
                        }
                        2 => {
                            // Measure -step, decide best direction
                            self.diversity_sa_minus_smeter = smeter_dbm;
                            let center = self.diversity_sa_center_smeter;
                            let plus = self.diversity_sa_plus_smeter;
                            let minus = self.diversity_sa_minus_smeter;

                            if plus < center && plus <= minus {
                                // +step is best
                                if is_phase {
                                    self.diversity_auto_best_phase += step;
                                    set_phase(self, self.diversity_auto_best_phase);
                                } else {
                                    let cur_db = 20.0 * self.diversity_auto_best_gain.max(0.01).log10();
                                    self.diversity_auto_best_gain = 10.0f32.powf((cur_db + step) / 20.0).clamp(0.05, 10.0);
                                    set_gain(self, self.diversity_auto_best_gain);
                                }
                                log::info!("SA {}: +step wins ({:.1} vs {:.1}/{:.1}), step={:.2}",
                                    if is_phase { "phase" } else { "gain" }, plus, center, minus, step);
                            } else if minus < center {
                                // -step is best
                                if is_phase {
                                    self.diversity_auto_best_phase -= step;
                                    set_phase(self, self.diversity_auto_best_phase);
                                } else {
                                    let cur_db = 20.0 * self.diversity_auto_best_gain.max(0.01).log10();
                                    self.diversity_auto_best_gain = 10.0f32.powf((cur_db - step) / 20.0).clamp(0.05, 10.0);
                                    set_gain(self, self.diversity_auto_best_gain);
                                }
                                log::info!("SA {}: -step wins ({:.1} vs {:.1}/{:.1}), step={:.2}",
                                    if is_phase { "phase" } else { "gain" }, minus, center, plus, step);
                            } else {
                                // Center is best - keep position
                                if is_phase {
                                    set_phase(self, self.diversity_auto_best_phase);
                                } else {
                                    set_gain(self, self.diversity_auto_best_gain);
                                }
                                log::info!("SA {}: center wins ({:.1} vs +{:.1}/-{:.1}), step={:.2}",
                                    if is_phase { "phase" } else { "gain" }, center, plus, minus, step);
                            }

                            // Halve step size
                            self.diversity_sa_step = step / 2.0;
                            self.diversity_sa_sub = 0;

                            // Check if this param is done (step below minimum)
                            if self.diversity_sa_step < min_step {
                                // Switch to other param or next iteration
                                if is_phase {
                                    // Phase done -> switch to gain SA (shrink range per iteration)
                                    self.diversity_sa_param = 1;
                                    self.diversity_sa_step = 10.0 / (self.diversity_sa_iteration as f32 + 1.0);
                                } else {
                                    // Gain done -> next iteration or finish
                                    self.diversity_sa_iteration += 1;
                                    if self.diversity_sa_iteration < 3 {
                                        // Another phase+gain pass with current best as starting point
                                        self.diversity_sa_param = 0;
                                        self.diversity_sa_step = 45.0 / (self.diversity_sa_iteration as f32 + 1.0); // shrinking start
                                    } else {
                                        // Done -> go to measurement phase
                                        self.diversity_auto_round = 999; // skip round processing
                                        self.diversity_auto_result = 1; // trigger final measurement
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    self.diversity_auto_last_set = Instant::now();
                } else
                if (self.diversity_auto_round >= rounds.len() || self.diversity_auto_round == 999) && self.diversity_auto_result == 1 {
                    // Rounds done -> turn diversity off to measure baseline
                    self.diversity_enabled = false;
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityEnable, 0));
                    self.diversity_auto_result = 4; // measuring off
                    self.diversity_auto_last_set = Instant::now();
                } else if self.diversity_auto_result == 4 {
                    // Diversity OFF - read baseline S-meter
                    self.diversity_auto_start_smeter = smeter_dbm;
                    log::info!("Auto-null: diversity OFF S-meter = {:.1} dBm (raw={})", smeter_dbm, self.smeter);
                    // Turn diversity back on
                    self.diversity_enabled = true;
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityEnable, 1));
                    self.diversity_auto_result = 5;
                    self.diversity_auto_last_set = Instant::now();
                } else if self.diversity_auto_result == 5 {
                    // Diversity ON - read final S-meter and compare
                    log::info!("Auto-null: diversity ON S-meter = {:.1} dBm (raw={})", smeter_dbm, self.smeter);
                    let improvement = self.diversity_auto_start_smeter - smeter_dbm;
                    log::info!("Auto-null: improvement = {:.1} dB (OFF {:.1} -> ON {:.1})", improvement, self.diversity_auto_start_smeter, smeter_dbm);
                    self.diversity_auto_improvement_db = improvement;
                    self.diversity_auto_active = false;
                    self.diversity_auto_result = if improvement > 0.5 { 2 } else { 3 };
                } else {
                    let round = &rounds[self.diversity_auto_round];

                    // Log round start
                    if self.diversity_auto_step == 0 {
                        let gain_db = 20.0 * self.diversity_auto_best_gain.max(0.01).log10();
                        let sweep_type = if round.is_gain_sweep { "GAIN sweep".to_string() } else { format!("PHASE ±{:.0}°", round.phase_range) };
                        log::info!("Auto-null ROUND {} start: smeter={:.1}dBm phase={:.1}° gain={:.3} ({:.1}dB) {}",
                            self.diversity_auto_round + 1, smeter_dbm,
                            self.diversity_auto_best_phase, self.diversity_auto_best_gain, gain_db, sweep_type);
                    }

                    if round.is_gain_sweep {
                        // Gain sweep - in dB around best gain (slow) or linear (fast)
                        let gains: Vec<f32> = if self.diversity_auto_slow {
                            // dB sweep around current best gain
                            let center_db = 20.0 * self.diversity_auto_best_gain.max(0.01).log10();
                            let (range_db, step_db) = match self.diversity_auto_round {
                                0 | 1 => (6.0, 2.0),     // ±6dB in 2dB (7 steps)
                                2 | 3 => (3.0, 1.0),     // ±3dB in 1dB (7 steps)
                                4 | 5 => (1.5, 0.25),    // ±1.5dB in 0.25dB (13 steps)
                                _     => (0.75, 0.1),     // ±0.75dB in 0.1dB (15 steps)
                            };
                            let half = (range_db / step_db) as isize;
                            (-half..=half).map(|i| {
                                let db = center_db + i as f32 * step_db;
                                10.0f32.powf(db / 20.0).clamp(0.05, 10.0)
                            }).collect()
                        } else {
                            // Linear sweep 0 to gain_max
                            let gs = round.gain_step;
                            let steps = (gain_max / gs).max(1.0) as usize;
                            (1..=steps).map(|i| i as f32 * gs).collect()
                        };

                        if self.diversity_auto_step > 0 && self.diversity_auto_step - 1 < gains.len() {
                            let prev_gain = gains[self.diversity_auto_step - 1];
                            if smeter_dbm < self.diversity_auto_best_smeter {
                                self.diversity_auto_best_smeter = smeter_dbm;
                                self.diversity_auto_best_gain = prev_gain;
                            }
                        }
                        if self.diversity_auto_step < gains.len() {
                            let gain = gains[self.diversity_auto_step];
                            let val = (gain * 1000.0) as u16;
                            if self.diversity_ref == 1 {
                                self.diversity_gain_rx2 = gain;
                                let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityGainRx2, val));
                            } else {
                                self.diversity_gain_rx1 = gain;
                                let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityGainRx1, val));
                            }
                            self.diversity_auto_step += 1;
                        } else {
                            // Check edge - extend if needed (fast mode only)
                            if !self.diversity_auto_slow && self.diversity_auto_best_gain > gain_max * 0.9 && gain_max < 10.0 {
                                self.diversity_gain_multi = (gain_max * 2.0).min(10.0);
                                self.diversity_auto_step = 0;
                                self.diversity_auto_best_smeter = 999.0;
                            } else {
                                // Apply best gain and advance round
                                let val = (self.diversity_auto_best_gain * 1000.0) as u16;
                                if self.diversity_ref == 1 {
                                    self.diversity_gain_rx2 = self.diversity_auto_best_gain;
                                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityGainRx2, val));
                                } else {
                                    self.diversity_gain_rx1 = self.diversity_auto_best_gain;
                                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityGainRx1, val));
                                }
                                self.diversity_auto_round += 1;
                                self.diversity_auto_step = 0;
                                self.diversity_auto_best_smeter = 999.0;
                            }
                        }
                    } else {
                        // Phase sweep at fixed gain
                        let range = round.phase_range;
                        let step = round.phase_step;
                        // Full sweep (range=180): always -180 to +180
                        // Narrow sweep: center on best_phase, wrap around ±180°
                        let center = if range >= 180.0 { 0.0 } else { best_phase };
                        let half_steps = (range / step).ceil() as isize;
                        let phases: Vec<f32> = (-half_steps..=half_steps)
                            .map(|i| {
                                let mut p = center + i as f32 * step;
                                // Wrap to -180..+180
                                while p > 180.0 { p -= 360.0; }
                                while p < -180.0 { p += 360.0; }
                                p
                            })
                            .collect();

                        // Set gain for this round (if specified)
                        if round.gain > 0.0 && self.diversity_auto_step == 0 {
                            let val = (round.gain * 1000.0) as u16;
                            if self.diversity_ref == 1 {
                                self.diversity_gain_rx2 = round.gain;
                                let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityGainRx2, val));
                            } else {
                                self.diversity_gain_rx1 = round.gain;
                                let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityGainRx1, val));
                            }
                        }

                        if self.diversity_auto_step > 0 && self.diversity_auto_step - 1 < phases.len() {
                            let prev_phase = phases[self.diversity_auto_step - 1];
                            if smeter_dbm < self.diversity_auto_best_smeter {
                                self.diversity_auto_best_smeter = smeter_dbm;
                                self.diversity_auto_best_phase = prev_phase;
                            }
                        }
                        if self.diversity_auto_step < phases.len() {
                            let phase = phases[self.diversity_auto_step];
                            self.diversity_phase = phase;
                            let encoded = ((phase * 100.0) as i32 + 18000) as u16;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityPhase, encoded));
                            self.diversity_auto_step += 1;
                        } else {
                            // Apply best phase and advance round
                            self.diversity_phase = self.diversity_auto_best_phase;
                            let encoded = ((self.diversity_phase * 100.0) as i32 + 18000) as u16;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityPhase, encoded));
                            self.diversity_auto_round += 1;
                            self.diversity_auto_step = 0;
                            self.diversity_auto_best_smeter = 999.0;
                        }
                    }
                }
                self.diversity_auto_last_set = Instant::now();
            }
        }
    }
}
