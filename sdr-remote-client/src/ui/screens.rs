use super::*;

impl SdrRemoteApp {
    pub(super) fn render_thetis_screen(&mut self, ui: &mut egui::Ui) {
        // Power toggle: click = on/off, long press (2s) = shutdown Thetis (ZZBY)
        const SHUTDOWN_HOLD_SECS: f32 = 2.0;
        let hold_progress = self.power_press_start
            .map(|t| t.elapsed().as_secs_f32() / SHUTDOWN_HOLD_SECS)
            .unwrap_or(0.0);
        let shutting_down = hold_progress >= 1.0;

        if shutting_down && !self.shutdown_sent {
            self.shutdown_sent = true;
            let _ = self.cmd_tx.send(Command::SetControl(ControlId::PowerOnOff, 2));
        }

        let (power_color, power_label) = if shutting_down {
            (Color32::from_rgb(200, 0, 0), "SHUTDOWN!")
        } else if hold_progress > 0.0 {
            let r = if self.power_on { (0.0 + 200.0 * hold_progress) as u8 } else { 150 };
            let g = if self.power_on { (150.0 * (1.0 - hold_progress)) as u8 } else { 0 };
            (Color32::from_rgb(r, g, 0), "HOLD...")
        } else if self.thetis_starting {
            (Color32::from_rgb(180, 130, 0), "STARTING...")
        } else if self.power_on {
            (Color32::from_rgb(0, 150, 0), "POWER ON")
        } else {
            (Color32::from_rgb(150, 0, 0), "POWER OFF")
        };

        ui.horizontal(|ui| {
            let btn = egui::Button::new(
                RichText::new(power_label).color(Color32::WHITE),
            ).fill(power_color).min_size(egui::vec2(90.0, 0.0));
            let response = ui.add(btn);

            let pointer_held_on_btn = ui.input(|i| {
                i.pointer.primary_down()
                    && response.rect.contains(i.pointer.interact_pos().unwrap_or(Pos2::ZERO))
            });

            if pointer_held_on_btn {
                if self.power_press_start.is_none() {
                    self.power_press_start = Some(Instant::now());
                    self.shutdown_sent = false;
                }
                ui.ctx().request_repaint();
            } else if self.power_press_start.is_some() {
                let was_short = !self.shutdown_sent;
                self.power_press_start = None;
                if was_short && !self.thetis_starting {
                    let new_val = !self.power_on;
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::PowerOnOff, new_val as u16));
                }
            }

        });

        ui.separator();

        // TX Profile dropdown
        if !self.tx_profiles.is_empty() {
            ui.horizontal(|ui| {
                ui.label("TX Profile:");
                let current_name = self.tx_profiles.iter()
                    .find(|(idx, _)| *idx == self.tx_profile)
                    .map(|(_, name)| name.as_str())
                    .unwrap_or("?");
                egui::ComboBox::from_id_salt("tx_profile_select")
                    .selected_text(RichText::new(current_name).strong())
                    .width(180.0)
                    .show_ui(ui, |ui| {
                        for (idx, name) in &self.tx_profiles {
                            if ui.selectable_label(*idx == self.tx_profile, name).clicked() {
                                let _ = self.cmd_tx.send(Command::SetControl(ControlId::TxProfile, *idx as u16));
                                self.tx_profile = *idx;
                            }
                        }
                    });
            });
        }

        // Thetis TUNE button (with PA bypass + delays)
        // Process delayed tune-on: PA standby sent, wait 500ms then ZZTU1
        if let Some(t) = self.tune_pending_on {
            if t.elapsed().as_millis() >= 500 {
                let _ = self.cmd_tx.send(Command::ThetisTune(true));
                self.thetis_tuning = true;
                self.tune_pending_on = None;
            }
        }
        // Process delayed PA restore: ZZTU0 sent, wait 1s then restore PA
        if let Some(t) = self.tune_pending_restore {
            if t.elapsed().as_millis() >= 1000 {
                if self.rf2k_connected && self.rf2k_active {
                    let _ = self.cmd_tx.send(Command::Rf2kOperate(true));
                }
                if self.spe_connected && self.spe_active {
                    let _ = self.cmd_tx.send(Command::SpeOperate);
                }
                self.tune_pending_restore = None;
                self.tune_pa_was_operate = false;
            }
        }
        ui.horizontal(|ui| {
            let waiting = self.tune_pending_on.is_some();
            let (tune_color, tune_text) = if self.thetis_tuning {
                (Color32::from_rgb(220, 60, 60), "TUNE ON")
            } else if waiting {
                (Color32::from_rgb(180, 130, 0), "PA STBY...")
            } else {
                (Color32::from_rgb(80, 80, 80), "TUNE")
            };
            let tune_btn = egui::Button::new(
                RichText::new(tune_text).color(Color32::WHITE),
            ).fill(tune_color).min_size(egui::vec2(80.0, 0.0));
            let enabled = self.power_on && self.connected && !waiting && self.tune_pending_restore.is_none();
            if ui.add_enabled(enabled, tune_btn).clicked() {
                if !self.thetis_tuning {
                    // Starting tune: bypass PA first, then delayed ZZTU1
                    self.tune_pa_was_operate = self.rf2k_operate || self.spe_state == 2;
                    if self.tune_pa_was_operate {
                        if self.rf2k_operate {
                            let _ = self.cmd_tx.send(Command::Rf2kOperate(false));
                        }
                        if self.spe_state == 2 {
                            let _ = self.cmd_tx.send(Command::SpeOperate);
                        }
                        self.tune_pending_on = Some(Instant::now()); // 500ms delay
                    } else {
                        // No PA active, tune immediately
                        let _ = self.cmd_tx.send(Command::ThetisTune(true));
                        self.thetis_tuning = true;
                    }
                } else {
                    // Stopping tune: ZZTU0 immediately, delayed PA restore
                    let _ = self.cmd_tx.send(Command::ThetisTune(false));
                    self.thetis_tuning = false;
                    if self.tune_pa_was_operate {
                        self.tune_pending_restore = Some(Instant::now()); // 1s delay
                    }
                }
            }

            if self.thetis_tuning {
                ui.label(RichText::new("Carrier ON").color(Color32::from_rgb(255, 100, 100)));
            }
        });

        ui.separator();

        // RX1 Volume slider
        ui.horizontal(|ui| {
            ui.label("RX1 Vol: ");
            let slider = egui::Slider::new(&mut self.rx_volume, 0.0..=1.0)
                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
            if ui.add(slider).changed() {
                let _ = self.cmd_tx.send(Command::SetRxVolume(self.rx_volume));
                self.save_full_config();
            }
        });

        // RX2 Volume slider
        ui.horizontal(|ui| {
            ui.label("RX2 Vol: ");
            let slider = egui::Slider::new(&mut self.rx2_volume, 0.0..=1.0)
                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
            if ui.add(slider).changed() {
                let _ = self.cmd_tx.send(Command::SetRx2Volume(self.rx2_volume));
                self.save_full_config();
            }
        });

        // TX Gain slider
        ui.horizontal(|ui| {
            ui.label("TX Gain: ");
            let slider = egui::Slider::new(&mut self.tx_gain, 0.0..=3.0)
                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
            if ui.add(slider).changed() {
                let _ = self.cmd_tx.send(Command::SetTxGain(self.tx_gain));
                self.save_full_config();
            }
        });

        ui.separator();

        // --- Thetis PTT mode ---
        ui.horizontal(|ui| {
            ui.label("Thetis PTT:");
            if ui.selectable_label(!self.ptt_toggle_mode, "Push to talk").clicked() {
                self.ptt_toggle_mode = false;
                self.save_ptt_config();
            }
            if ui.selectable_label(self.ptt_toggle_mode, "Toggle").clicked() {
                self.ptt_toggle_mode = true;
                self.save_ptt_config();
            }
        });

        // --- WebSDR ---
        ui.horizontal(|ui| {
            if ui.checkbox(&mut self.catsync.enabled, "WebSDR mute on TX").changed() {
                if !self.catsync.enabled {
                    self.catsync.force_unmute();
                }
            }
            if self.catsync.enabled {
                if self.catsync.is_muted() {
                    ui.colored_label(Color32::from_rgb(255, 165, 0), "MUTED");
                } else {
                    ui.colored_label(Color32::from_rgb(100, 100, 100), "listening");
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let webview_open = self.catsync.webview_open();
                if webview_open {
                    if ui.button("Close WebSDR").clicked() {
                        self.catsync.close_websdr_window();
                    }
                    ui.colored_label(Color32::from_rgb(100, 200, 100), "Window open");
                } else {
                    if ui.button("WebSDR").clicked() {
                        self.catsync.open_websdr_window(self.frequency_hz, self.mode);
                    }
                }
                if ui.small_button("ext").on_hover_text("Open in external browser").clicked() {
                    let url = self.catsync.websdr_tune_url(self.frequency_hz, self.mode);
                    let _ = open::that(&url);
                }
            });
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("URL:").size(11.0).color(Color32::GRAY));
            ui.add(egui::TextEdit::singleline(&mut self.catsync.websdr_url)
                .desired_width(ui.available_width() - 40.0)
                .font(egui::FontId::proportional(11.0)));
            if ui.small_button("\u{2605}").on_hover_text("Add to favorites").clicked() {
                self.catsync.add_favorite();
                self.save_full_config();
            }
        });
        if !self.catsync.favorites.is_empty() {
            let mut select_idx = None;
            let mut remove_idx = None;
            for (i, (label, url)) in self.catsync.favorites.iter().enumerate() {
                ui.horizontal(|ui| {
                    let active = *url == self.catsync.websdr_url;
                    let text = RichText::new(label).size(11.0).color(
                        if active { Color32::from_rgb(100, 200, 100) } else { Color32::GRAY }
                    );
                    if ui.add(egui::Label::new(text).sense(egui::Sense::click())).clicked() {
                        select_idx = Some(i);
                    }
                    let type_label = if crate::catsync::is_kiwi_url(url) { "kiwi" } else { "wsdr" };
                    ui.label(RichText::new(type_label).size(9.0).color(Color32::DARK_GRAY));
                    if ui.small_button("X").on_hover_text("Remove").clicked() {
                        remove_idx = Some(i);
                    }
                });
            }
            if let Some(idx) = select_idx {
                self.catsync.select_favorite(idx);
                self.save_full_config();
            }
            if let Some(idx) = remove_idx {
                self.catsync.remove_favorite(idx);
                self.save_full_config();
            }
        }

        ui.separator();

        // --- TCI controls ---
        use sdr_remote_core::protocol::ControlId as CId;
        macro_rules! tci_set {
            ($self:ident, $id:expr, $val:expr) => {{
                let _ = $self.cmd_tx.send(Command::SetControl($id, $val));
                $self.tci_control_changed_at = Some(Instant::now());
            }};
        }

        // ===== RX1 =====
        ui.group(|ui| {
            ui.label(RichText::new("RX1").strong());

            // Row 1: AGC mode + NB + BIN + APF + Lock
            ui.horizontal(|ui| {
                ui.label("AGC:");
                let agc_labels = ["Off", "Long", "Slow", "Med", "Fast", "Custom"];
                let cur = agc_labels.get(self.agc_mode as usize).unwrap_or(&"?");
                egui::ComboBox::from_id_salt("agc_mode_rx1")
                    .selected_text(*cur)
                    .width(70.0)
                    .show_ui(ui, |ui| {
                        for (i, label) in agc_labels.iter().enumerate() {
                            if ui.selectable_label(self.agc_mode == i as u8, *label).clicked() {
                                self.agc_mode = i as u8;
                                tci_set!(self, CId::AgcMode, i as u16);
                            }
                        }
                    });

                let mut apf = self.apf_enable;
                if ui.checkbox(&mut apf, "APF").changed() {
                    self.apf_enable = apf;
                    tci_set!(self, CId::ApfEnable, apf as u16);
                }

                ui.label("Audio:");
                egui::ComboBox::from_id_salt("audio_mode_rx1")
                    .width(60.0)
                    .selected_text(match self.audio_mode { 1 => "BIN", 2 => "Split", _ => "Mono" })
                    .show_ui(ui, |ui| {
                        for (val, label) in [(0u16, "Mono"), (1, "BIN"), (2, "Split")] {
                            if ui.selectable_label(self.audio_mode == val, label).clicked() {
                                self.audio_mode = val;
                                let _ = self.cmd_tx.send(Command::SetControl(ControlId::AudioMode, val));
                                // Auto-toggle Thetis BIN mode
                                // Note: engine also toggles BIN off during TX and back on after
                                // (Thetis BIN has a side-effect on TX audio quality)
                                let bin_on = val == 1;
                                if bin_on != self.binaural {
                                    self.binaural = bin_on;
                                    tci_set!(self, CId::Binaural, bin_on as u16);
                                }
                                if !bin_on && self.rx_balance != 0 {
                                    self.rx_balance = 0;
                                    tci_set!(self, CId::RxBalance, 0);
                                }
                            }
                        }
                    });

                if self.binaural {
                    ui.separator();
                    ui.label("Bal:");
                    let mut bal = self.rx_balance as f32;
                    let bal_slider = egui::Slider::new(&mut bal, -40.0..=40.0)
                        .custom_formatter(|v, _| {
                            if v < -1.0 { format!("L{:.0}", -v) }
                            else if v > 1.0 { format!("R{:.0}", v) }
                            else { "C".to_string() }
                        });
                    if ui.add_sized([80.0, 16.0], bal_slider).changed() {
                        // Negate: slider visual left (-) → TCI +40 (which is left audio in Thetis)
                        self.rx_balance = bal as i8;
                        let tci_val = (-self.rx_balance) as i16 as u16;
                        let _ = self.cmd_tx.send(Command::SetControl(CId::RxBalance, tci_val));
                        // Don't use tci_set! macro to avoid overwriting with server echo
                    }
                }
            });

            // AGC gain slider + Auto AGC
            if self.agc_mode != 0 {
                ui.horizontal(|ui| {
                    let mut auto_agc = self.agc_auto_rx1;
                    if ui.checkbox(&mut auto_agc, "Auto").changed() {
                        self.agc_auto_rx1 = auto_agc;
                        let _ = self.cmd_tx.send(Command::SetControl(CId::AgcAutoRx1, auto_agc as u16));
                    }
                    ui.label("Gain:");
                    let mut gain = self.agc_gain as f32;
                    let gain_slider = egui::Slider::new(&mut gain, 0.0..=120.0)
                        .custom_formatter(|v, _| format!("{:.0}", v));
                    if ui.add_sized([120.0, 16.0], gain_slider).changed() {
                        self.agc_gain = gain as u8;
                        tci_set!(self, CId::AgcGain, self.agc_gain as u16);
                    }
                });
            }

            // Row 2: RIT + XIT (with ±10 Hz fine tune buttons)
            ui.horizontal(|ui| {
                let mut rit = self.rit_enable;
                if ui.checkbox(&mut rit, "RIT").changed() {
                    self.rit_enable = rit;
                    tci_set!(self, CId::RitEnable, rit as u16);
                }
                if ui.small_button("-10").clicked() {
                    self.rit_offset = (self.rit_offset - 10).max(-9999);
                    tci_set!(self, CId::RitOffset, self.rit_offset as u16);
                }
                let mut rit_hz = self.rit_offset as f32;
                let rit_slider = egui::Slider::new(&mut rit_hz, -9999.0..=9999.0)
                    .step_by(10.0)
                    .suffix(" Hz")
                    .custom_formatter(|v, _| format!("{:+.0}", v));
                if ui.add_sized([120.0, 16.0], rit_slider).changed() {
                    self.rit_offset = rit_hz as i16;
                    tci_set!(self, CId::RitOffset, self.rit_offset as u16);
                }
                if ui.small_button("+10").clicked() {
                    self.rit_offset = (self.rit_offset + 10).min(9999);
                    tci_set!(self, CId::RitOffset, self.rit_offset as u16);
                }
                if ui.small_button("0").clicked() {
                    self.rit_offset = 0;
                    tci_set!(self, CId::RitOffset, 0);
                }

                ui.separator();

                let mut xit = self.xit_enable;
                if ui.checkbox(&mut xit, "XIT").changed() {
                    self.xit_enable = xit;
                    tci_set!(self, CId::XitEnable, xit as u16);
                }
                if ui.small_button("-10").clicked() {
                    self.xit_offset = (self.xit_offset - 10).max(-9999);
                    tci_set!(self, CId::XitOffset, self.xit_offset as u16);
                }
                let mut xit_hz = self.xit_offset as f32;
                let xit_slider = egui::Slider::new(&mut xit_hz, -9999.0..=9999.0)
                    .step_by(10.0)
                    .suffix(" Hz")
                    .custom_formatter(|v, _| format!("{:+.0}", v));
                if ui.add_sized([120.0, 16.0], xit_slider).changed() {
                    self.xit_offset = xit_hz as i16;
                    tci_set!(self, CId::XitOffset, self.xit_offset as u16);
                }
                if ui.small_button("+10").clicked() {
                    self.xit_offset = (self.xit_offset + 10).min(9999);
                    tci_set!(self, CId::XitOffset, self.xit_offset as u16);
                }
                if ui.small_button("0").clicked() {
                    self.xit_offset = 0;
                    tci_set!(self, CId::XitOffset, 0);
                }
            });

            // Row 3: Squelch + CW speed
            ui.horizontal(|ui| {
                let mut sql = self.sql_enable;
                if ui.checkbox(&mut sql, "SQL").changed() {
                    self.sql_enable = sql;
                    tci_set!(self, CId::SqlEnable, sql as u16);
                }
                let mut sql_val = self.sql_level as f32;
                let sql_slider = egui::Slider::new(&mut sql_val, 0.0..=160.0)
                    .custom_formatter(|v, _| format!("{:.0}", v));
                if ui.add_sized([100.0, 16.0], sql_slider).changed() {
                    self.sql_level = sql_val as u8;
                    tci_set!(self, CId::SqlLevel, self.sql_level as u16);
                }

                ui.separator();

                ui.label("CW:");
                let mut cw_spd = self.cw_keyer_speed as f32;
                let cw_slider = egui::Slider::new(&mut cw_spd, 1.0..=60.0)
                    .suffix(" WPM")
                    .custom_formatter(|v, _| format!("{:.0}", v));
                if ui.add_sized([120.0, 16.0], cw_slider).changed() {
                    self.cw_keyer_speed = cw_spd as u8;
                    tci_set!(self, CId::CwKeyerSpeed, self.cw_keyer_speed as u16);
                }
            });

            // Row 4: Tune drive + Mon volume
            ui.horizontal(|ui| {
                ui.label("Tune drv:");
                let mut td = self.tune_drive as f32;
                let td_slider = egui::Slider::new(&mut td, 0.0..=100.0)
                    .suffix("%")
                    .custom_formatter(|v, _| format!("{:.0}", v));
                if ui.add_sized([100.0, 16.0], td_slider).changed() {
                    self.tune_drive = td as u8;
                    tci_set!(self, CId::TuneDrive, self.tune_drive as u16);
                }

                ui.separator();

                ui.label("Mon:");
                let mut mv = self.mon_volume as f32;
                let mv_slider = egui::Slider::new(&mut mv, -40.0..=0.0)
                    .suffix(" dB")
                    .custom_formatter(|v, _| format!("{:.0}", v));
                if ui.add_sized([100.0, 16.0], mv_slider).changed() {
                    self.mon_volume = mv as i8;
                    tci_set!(self, CId::MonitorVolume, self.mon_volume as i16 as u16);
                }
            });

            // Row 5: DDC sample rate
            if self.ddc_sample_rate_rx1 > 0 {
                ui.horizontal(|ui| {
                    ui.label("DDC:");
                    let ddc_rates: &[u16] = &[48, 96, 192, 384, 768, 1536];
                    let cur_khz = self.ddc_sample_rate_rx1;
                    let cur_label = format!("{}kHz", cur_khz);
                    egui::ComboBox::from_id_salt("ddc_rate_rx1")
                        .selected_text(&cur_label)
                        .width(90.0)
                        .show_ui(ui, |ui| {
                            for &rate in ddc_rates {
                                let label = format!("{}kHz", rate);
                                if ui.selectable_label(cur_khz == rate, &label).clicked() {
                                    self.ddc_sample_rate_rx1 = rate;
                                    let _ = self.cmd_tx.send(Command::SetControl(CId::DdcSampleRateRx1, rate));
                                }
                            }
                        });
                });
            }
        });

        // ===== RX2 =====
        ui.group(|ui| {
            ui.label(RichText::new("RX2").strong());

            // Row 1: AGC mode + NB + BIN + APF
            ui.horizontal(|ui| {
                ui.label("AGC:");
                let agc_labels = ["Off", "Long", "Slow", "Med", "Fast", "Custom"];
                let cur = agc_labels.get(self.rx2_agc_mode as usize).unwrap_or(&"?");
                egui::ComboBox::from_id_salt("agc_mode_rx2")
                    .selected_text(*cur)
                    .width(70.0)
                    .show_ui(ui, |ui| {
                        for (i, label) in agc_labels.iter().enumerate() {
                            if ui.selectable_label(self.rx2_agc_mode == i as u8, *label).clicked() {
                                self.rx2_agc_mode = i as u8;
                                tci_set!(self, CId::Rx2AgcMode, i as u16);
                            }
                        }
                    });

                let mut nb = self.rx2_nb_enable;
                if ui.checkbox(&mut nb, "NB").changed() {
                    self.rx2_nb_enable = nb;
                    tci_set!(self, CId::Rx2NoiseBlanker, nb as u16);
                }

                let mut apf = self.rx2_apf_enable;
                if ui.checkbox(&mut apf, "APF").changed() {
                    self.rx2_apf_enable = apf;
                    tci_set!(self, CId::Rx2ApfEnable, apf as u16);
                }
            });

            // AGC gain slider + Auto AGC
            if self.rx2_agc_mode != 0 {
                ui.horizontal(|ui| {
                    let mut auto_agc = self.agc_auto_rx2;
                    if ui.checkbox(&mut auto_agc, "Auto").changed() {
                        self.agc_auto_rx2 = auto_agc;
                        let _ = self.cmd_tx.send(Command::SetControl(CId::AgcAutoRx2, auto_agc as u16));
                    }
                    ui.label("Gain:");
                    let mut gain = self.rx2_agc_gain as f32;
                    let gain_slider = egui::Slider::new(&mut gain, 0.0..=120.0)
                        .custom_formatter(|v, _| format!("{:.0}", v));
                    if ui.add_sized([120.0, 16.0], gain_slider).changed() {
                        self.rx2_agc_gain = gain as u8;
                        tci_set!(self, CId::Rx2AgcGain, self.rx2_agc_gain as u16);
                    }
                });
            }

            // Row 2: Squelch
            ui.horizontal(|ui| {
                let mut sql = self.rx2_sql_enable;
                if ui.checkbox(&mut sql, "SQL").changed() {
                    self.rx2_sql_enable = sql;
                    tci_set!(self, CId::Rx2SqlEnable, sql as u16);
                }
                let mut sql_val = self.rx2_sql_level as f32;
                let sql_slider = egui::Slider::new(&mut sql_val, 0.0..=160.0)
                    .custom_formatter(|v, _| format!("{:.0}", v));
                if ui.add_sized([100.0, 16.0], sql_slider).changed() {
                    self.rx2_sql_level = sql_val as u8;
                    tci_set!(self, CId::Rx2SqlLevel, self.rx2_sql_level as u16);
                }
            });

            // DDC sample rate
            if self.ddc_sample_rate_rx2 > 0 {
                ui.horizontal(|ui| {
                    ui.label("DDC:");
                    let ddc_rates: &[u16] = &[48, 96, 192, 384, 768, 1536];
                    let cur_khz = self.ddc_sample_rate_rx2;
                    let cur_label = format!("{}kHz", cur_khz);
                    egui::ComboBox::from_id_salt("ddc_rate_rx2")
                        .selected_text(&cur_label)
                        .width(90.0)
                        .show_ui(ui, |ui| {
                            for &rate in ddc_rates {
                                let label = format!("{}kHz", rate);
                                if ui.selectable_label(cur_khz == rate, &label).clicked() {
                                    self.ddc_sample_rate_rx2 = rate;
                                    let _ = self.cmd_tx.send(Command::SetControl(CId::DdcSampleRateRx2, rate));
                                }
                            }
                        });
                });
            }
        });

    }

    pub(super) fn render_diversity(&mut self, ui: &mut egui::Ui) {
        use sdr_remote_core::protocol::ControlId;

        let gain_max = self.diversity_gain_multi;

        // Enable + dropdowns row
        ui.horizontal(|ui| {
            if ui.add(egui::Button::new(
                if self.diversity_enabled {
                    RichText::new("Diversity ON").color(Color32::WHITE)
                } else {
                    RichText::new("Diversity OFF")
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
            ui.label("Source:");
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
                painter.text(egui::pos2(rect.left() + 4.0, rect.top() + 3.0), egui::Align2::LEFT_TOP,
                    format!("Phase: {:.1}°  Gain: {:.3}", self.diversity_phase, non_ref_gain),
                    egui::FontId::proportional(11.0), Color32::from_rgb(200, 200, 220));
            }

            // Handle mouse click/drag in circle — only send when diversity enabled
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
                ui.label("Gain Multi:");
                let gm_slider = egui::Slider::new(&mut self.diversity_gain_multi, 1.0..=10.0)
                    .custom_formatter(|v, _| format!("{:.0}", v))
                    .step_by(1.0);
                ui.add_sized([160.0, 16.0], gm_slider);

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
                    if ui.add_sized([160.0, 16.0], g1_slider).changed() && self.diversity_enabled {
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
                    if ui.add_sized([160.0, 16.0], g2_slider).changed() && self.diversity_enabled {
                        let val = (self.diversity_gain_rx2 * 1000.0) as u16;
                        let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityGainRx2, val));
                    }
                }

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.diversity_gain_lock, "Lock Gain");
                    ui.checkbox(&mut self.diversity_phase_lock, "Lock Phase");
                });

                ui.add_space(4.0);
                ui.label("Phase:");
                let phase_slider = egui::Slider::new(&mut self.diversity_phase, -180.0..=180.0)
                    .custom_formatter(|v, _| format!("{:.1}°", v))
                    .step_by(0.1);
                if ui.add_sized([160.0, 16.0], phase_slider).changed() && self.diversity_enabled {
                    let encoded = ((self.diversity_phase * 100.0) as i32 + 18000) as u16;
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityPhase, encoded));
                }

                ui.add_space(6.0);
                // Auto-null button with result color
                if self.diversity_auto_active {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        let label = if self.diversity_auto_smart {
                            format!("Smart (Thetis-side)")
                        } else if self.diversity_auto_slow {
                            let param = if self.diversity_sa_param == 0 { "Phase" } else { "Gain" };
                            format!("SA {} iter {} step {:.1}", param, self.diversity_sa_iteration + 1, self.diversity_sa_step)
                        } else {
                            format!("Round {}...", self.diversity_auto_round + 1)
                        };
                        ui.label(label);
                        if ui.add(egui::Button::new("Stop")
                            .fill(Color32::from_rgb(200, 120, 0))).clicked() {
                            self.diversity_auto_active = false;
                            self.diversity_auto_result = 0;
                            // Note: Thetis-side autonull can't be stopped mid-run
                        }
                    });
                } else {
                    let (btn_color, btn_text) = match self.diversity_auto_result {
                        2 => (Color32::from_rgb(0, 140, 0), format!("Auto Null ({:+.1} dB)", -self.diversity_auto_improvement_db)),
                        3 => (Color32::from_rgb(140, 0, 0), "Auto Null (no gain)".to_string()),
                        _ => (Color32::from_rgb(60, 60, 60), "Auto Null".to_string()),
                    };
                    ui.horizontal(|ui| {
                        if ui.add(egui::Button::new(RichText::new(&btn_text).color(Color32::WHITE))
                            .fill(btn_color)).clicked() {
                            let smeter_dbm = display_to_dbm(self.smeter);
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

        // Convert display S-meter (0-260) back to dBm
        fn display_to_dbm(display: u16) -> f32 {
            if display <= 108 {
                (display as f32) * (48.0 / 108.0) - 121.0
            } else {
                (display as f32 - 108.0) * (60.0 / 152.0) - 73.0
            }
        }

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
                let smeter_dbm = display_to_dbm(self.smeter);
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
                        // Both receivers active — read S-meters
                        let rx1_dbm = display_to_dbm(self.smeter);
                        let rx2_dbm = display_to_dbm(self.rx2_smeter);
                        // Non-ref needs gain to match ref: gain = ref_dBm - nonref_dBm
                        let (ref_dbm, nonref_dbm) = if self.diversity_ref == 1 {
                            (rx1_dbm, rx2_dbm) // RX1 is ref, boost RX2
                        } else {
                            (rx2_dbm, rx1_dbm) // RX2 is ref, boost RX1
                        };
                        let diff_db = ref_dbm - nonref_dbm; // positive = non-ref is weaker → needs boost
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
                    // Smart done — handled by measurement phase below

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
                        // All steps done → measurement phase
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
                                // Round complete — apply best, advance
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
                    let smeter_dbm = display_to_dbm(self.smeter);
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
                                // Center is best — keep position
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
                                    // Phase done → switch to gain SA (shrink range per iteration)
                                    self.diversity_sa_param = 1;
                                    self.diversity_sa_step = 10.0 / (self.diversity_sa_iteration as f32 + 1.0);
                                } else {
                                    // Gain done → next iteration or finish
                                    self.diversity_sa_iteration += 1;
                                    if self.diversity_sa_iteration < 3 {
                                        // Another phase+gain pass with current best as starting point
                                        self.diversity_sa_param = 0;
                                        self.diversity_sa_step = 45.0 / (self.diversity_sa_iteration as f32 + 1.0); // shrinking start
                                    } else {
                                        // Done → go to measurement phase
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
                    // Rounds done → turn diversity off to measure baseline
                    self.diversity_enabled = false;
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityEnable, 0));
                    self.diversity_auto_result = 4; // measuring off
                    self.diversity_auto_last_set = Instant::now();
                } else if self.diversity_auto_result == 4 {
                    // Diversity OFF — read baseline S-meter
                    self.diversity_auto_start_smeter = smeter_dbm;
                    log::info!("Auto-null: diversity OFF S-meter = {:.1} dBm (raw={})", smeter_dbm, self.smeter);
                    // Turn diversity back on
                    self.diversity_enabled = true;
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::DiversityEnable, 1));
                    self.diversity_auto_result = 5;
                    self.diversity_auto_last_set = Instant::now();
                } else if self.diversity_auto_result == 5 {
                    // Diversity ON — read final S-meter and compare
                    log::info!("Auto-null: diversity ON S-meter = {:.1} dBm (raw={})", smeter_dbm, self.smeter);
                    let improvement = self.diversity_auto_start_smeter - smeter_dbm;
                    log::info!("Auto-null: improvement = {:.1} dB (OFF {:.1} → ON {:.1})", improvement, self.diversity_auto_start_smeter, smeter_dbm);
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
                        // Gain sweep — in dB around best gain (slow) or linear (fast)
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
                            // Check edge — extend if needed (fast mode only)
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

    pub(super) fn render_server_screen(&mut self, ui: &mut egui::Ui) {
        // Repaint at 30fps when connected (live audio levels), slow when idle
        let ms = if self.connected { 33 } else { 500 };
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(ms));

        // Server address + password
        ui.horizontal(|ui| {
            ui.label("Server:");
            let enabled = !self.connected;
            ui.add_enabled(enabled, egui::TextEdit::singleline(&mut self.server_input).desired_width(150.0));
            ui.label("Password:");
            ui.add_enabled(enabled, egui::TextEdit::singleline(&mut self.password_input)
                .desired_width(100.0).password(true).hint_text("(required)"));
        });
        if self.state_rx.borrow().auth_rejected {
            ui.colored_label(Color32::from_rgb(220, 40, 40), "Authentication failed - wrong password");
        } else if self.state_rx.borrow().totp_required {
            ui.horizontal(|ui| {
                ui.label("2FA Code:");
                let re = ui.add(egui::TextEdit::singleline(&mut self.totp_input)
                    .desired_width(80.0).hint_text("6 digits"));
                if (re.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
                    || ui.button("Verify").clicked())
                    && self.totp_input.len() == 6
                {
                    let _ = self.cmd_tx.send(sdr_remote_logic::commands::Command::SendTotpCode(self.totp_input.clone()));
                    self.totp_input.clear();
                }
            });
        } else if !self.connected && self.password_input.is_empty() {
            ui.colored_label(Color32::from_rgb(255, 165, 0), "Password is required to connect");
        }

        ui.separator();

        // Audio device selection — refresh device list only when combo opened or first time
        // Device enumeration (cpal/WASAPI) blocks the UI thread for 50-200ms on Windows,
        // causing audio hiccups. Only refresh on first render or when user opens the combo.
        let needs_device_refresh = self.device_refresh_at.is_none();
        if needs_device_refresh {
            self.input_devices = crate::audio::list_input_devices();
            self.output_devices = crate::audio::list_output_devices();
            self.device_refresh_at = Some(Instant::now());
        }
        ui.horizontal(|ui| {
            ui.label("Input:");
            let default_label = "(Default)";
            let current_input = if self.selected_input.is_empty() {
                default_label
            } else {
                &self.selected_input
            };
            let resp = egui::ComboBox::from_id_salt("input_dev")
                .selected_text(current_input)
                .width(250.0)
                .show_ui(ui, |ui| {
                    if ui.selectable_label(self.selected_input.is_empty(), default_label).clicked() {
                        self.selected_input.clear();
                        let _ = self.cmd_tx.send(Command::SetInputDevice(String::new()));
                        self.save_full_config();
                    }
                    for name in &self.input_devices {
                        if ui.selectable_label(*name == self.selected_input, name).clicked() {
                            self.selected_input = name.clone();
                            let _ = self.cmd_tx.send(Command::SetInputDevice(name.clone()));
                            self.save_full_config();
                        }
                    }
                });
            // Refresh device list when combo is opened (not every frame)
            if resp.response.clicked() {
                self.input_devices = crate::audio::list_input_devices();
            }
        });
        ui.horizontal(|ui| {
            ui.label("Output:");
            let default_label = "(Default)";
            let current_output = if self.selected_output.is_empty() {
                default_label
            } else {
                &self.selected_output
            };
            let resp = egui::ComboBox::from_id_salt("output_dev")
                .selected_text(current_output)
                .width(250.0)
                .show_ui(ui, |ui| {
                    if ui.selectable_label(self.selected_output.is_empty(), default_label).clicked() {
                        self.selected_output.clear();
                        let _ = self.cmd_tx.send(Command::SetOutputDevice(String::new()));
                        self.save_full_config();
                    }
                    for name in &self.output_devices {
                        if ui.selectable_label(*name == self.selected_output, name).clicked() {
                            self.selected_output = name.clone();
                            let _ = self.cmd_tx.send(Command::SetOutputDevice(name.clone()));
                            self.save_full_config();
                        }
                    }
                });
            // Refresh device list when combo is opened (not every frame)
            if resp.response.clicked() {
                self.output_devices = crate::audio::list_output_devices();
            }
        });

        // Mic → TX Profile auto-switch mapping
        if !self.tx_profiles.is_empty() && !self.input_devices.is_empty() {
            ui.separator();
            ui.label("Mic → TX Profile mapping:");
            let mut changed = false;
            for dev_name in &self.input_devices {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(dev_name).size(11.0));
                    let current = self.mic_profile_map.get(dev_name).cloned().unwrap_or_default();
                    let display = if current.is_empty() { "(none)" } else { &current };
                    egui::ComboBox::from_id_salt(format!("mic_prof_{}", dev_name))
                        .selected_text(display)
                        .width(150.0)
                        .show_ui(ui, |ui| {
                            if ui.selectable_label(current.is_empty(), "(none)").clicked() {
                                self.mic_profile_map.remove(dev_name);
                                changed = true;
                            }
                            for (_, prof_name) in &self.tx_profiles {
                                if ui.selectable_label(current == *prof_name, prof_name).clicked() {
                                    self.mic_profile_map.insert(dev_name.clone(), prof_name.clone());
                                    changed = true;
                                }
                            }
                        });
                });
            }
            if changed {
                self.save_full_config();
            }
        }

        ui.separator();

        // Audio levels — single mic bar, label changes with PTT state
        ui.label("Audio Levels:");
        ui.horizontal(|ui| {
            let (mic_label, mic_level) = if self.yaesu_tx_active {
                ("Yaesu Mic:", self.yaesu_mic_level)
            } else if self.ptt {
                ("Thetis Mic:", self.capture_level)
            } else {
                ("Mic:       ", self.capture_level)
            };
            ui.label(mic_label);
            level_bar(ui, mic_level);
        });
        if self.binaural && self.playback_level_bin_r > 0.0 {
            ui.horizontal(|ui| {
                ui.label("RX1 L:     ");
                level_bar(ui, self.playback_level);
            });
            ui.horizontal(|ui| {
                ui.label("RX1 R:     ");
                level_bar(ui, self.playback_level_bin_r);
            });
        } else {
            ui.horizontal(|ui| {
                ui.label("RX1:       ");
                level_bar(ui, self.playback_level);
            });
        }
        if self.rx2_enabled {
            ui.horizontal(|ui| {
                ui.label("RX2:       ");
                level_bar(ui, self.playback_level_rx2);
            });
        }
        if self.yaesu_connected || self.yaesu_enabled {
            ui.horizontal(|ui| {
                ui.label("Yaesu RX:  ");
                level_bar(ui, self.playback_level_yaesu);
            });
        }

        ui.separator();

        // Audio recording
        ui.horizontal(|ui| {
            ui.label("Record:");
            if self.recording {
                if ui.button(RichText::new("⏹ Stop").color(Color32::WHITE))
                    .highlight()
                    .clicked()
                {
                    let _ = self.cmd_tx.send(Command::StopRecording);
                    self.recording = false;
                }
            } else {
                ui.checkbox(&mut self.rec_rx1, "RX1");
                if self.rx2_enabled {
                    ui.checkbox(&mut self.rec_rx2, "RX2");
                }
                if self.yaesu_connected || self.yaesu_enabled {
                    ui.checkbox(&mut self.rec_yaesu, "Yaesu");
                }
                let any = self.rec_rx1 || self.rec_rx2 || self.rec_yaesu;
                if ui.add_enabled(any, egui::Button::new("⏺ Rec")).clicked() {
                    let path = std::env::current_exe()
                        .ok()
                        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                        .unwrap_or_default();
                    let _ = self.cmd_tx.send(Command::StartRecording {
                        rx1: self.rec_rx1,
                        rx2: self.rec_rx2,
                        yaesu: self.rec_yaesu,
                        path: path.to_string_lossy().to_string(),
                    });
                    self.recording = true;
                }
                // Play button for last recording
                if let Some(ref wav_path) = self.last_recorded_path {
                    if !self.playing {
                        if ui.button("▶ Play").clicked() {
                            let _ = self.cmd_tx.send(Command::PlayRecording { path: wav_path.clone() });
                            self.playing = true;
                        }
                    } else {
                        if ui.button("⏹ Stop").clicked() {
                            let _ = self.cmd_tx.send(Command::StopPlayback);
                            self.playing = false;
                        }
                    }
                }
            }
        });

        ui.separator();

        // Stats
        ui.label("Statistics:");
        egui::Grid::new("stats_grid")
            .num_columns(2)
            .spacing([20.0, 4.0])
            .show(ui, |ui| {
                ui.label("RTT:");
                ui.label(format!("{} ms", self.rtt_ms));
                ui.end_row();

                ui.label("Jitter:");
                ui.label(format!("{:.1} ms", self.jitter_ms));
                ui.end_row();

                ui.label("Buffer:");
                ui.label(format!("{} frames", self.buffer_depth));
                ui.end_row();

                ui.label("Loss:");
                ui.label(format!("{}%", self.loss_percent));
                ui.end_row();

                ui.label("RX packets:");
                ui.label(format!("{}", self.rx_packets));
                ui.end_row();
            });

        // TCI Status
        ui.separator();
        ui.label("TCI Status:");
        egui::Grid::new("tci_grid")
            .num_columns(2)
            .spacing([20.0, 4.0])
            .show(ui, |ui| {
                ui.label("TX Profile:");
                let profile_name = self.tx_profiles.iter()
                    .find(|(idx, _)| *idx == self.tx_profile)
                    .map(|(_, name)| name.as_str())
                    .unwrap_or("?");
                ui.label(profile_name);
                ui.end_row();

                ui.label("TX Profiles:");
                let names: Vec<&str> = self.tx_profiles.iter().map(|(_, n)| n.as_str()).collect();
                ui.label(if names.is_empty() { "(none)".to_string() } else { names.join(", ") });
                ui.end_row();

                ui.label("MON:");
                ui.label(if self.mon_on { "ON" } else { "OFF" });
                ui.end_row();

                ui.label("VFO Sync:");
                ui.label(if self.vfo_sync { "ON" } else { "OFF" });
                ui.end_row();
            });

        // Remote reboot / shutdown
        ui.separator();
        ui.horizontal(|ui| {
            if self.connected {
                if self.reboot_confirm {
                    ui.label("Remote server PC:");
                    if ui.button("Reboot").clicked() {
                        let _ = self.cmd_tx.send(Command::ServerReboot);
                        self.reboot_confirm = false;
                    }
                    if ui.button("Shutdown").clicked() {
                        let _ = self.cmd_tx.send(Command::ServerShutdown);
                        self.reboot_confirm = false;
                    }
                    if ui.button("Cancel").clicked() {
                        self.reboot_confirm = false;
                    }
                } else if ui.button("Remote Reboot / Shutdown").clicked() {
                    self.reboot_confirm = true;
                }
            }
        });
    }

    pub(super) fn process_midi_events(&mut self) {
        use crate::midi::{MidiEvent, MidiAction};
        use sdr_remote_core::protocol::ControlId;

        let freq_steps: &[u64] = &[10, 100, 500, 1_000, 10_000];

        while let Ok(event) = self.midi.event_rx.try_recv() {
            match event {
                MidiEvent::Learn(is_note, channel, number, value) => {
                    self.midi_last_event = format!(
                        "{} ch{} #{} val={}",
                        if is_note { "Note" } else { "CC" },
                        channel + 1, number, value,
                    );
                    // If learning, create the mapping
                    if self.midi_learn_for.is_some() {
                        let mapping = crate::midi::MidiMapping {
                            is_note,
                            channel,
                            number,
                            control_type: if is_note {
                                crate::midi::ControlType::Button
                            } else {
                                // Auto-detect: if action is encoder-type, default to Encoder
                                match self.midi_learn_action {
                                    MidiAction::VfoATune | MidiAction::VfoBTune
                                    | MidiAction::NrLevel => crate::midi::ControlType::Encoder,
                                    MidiAction::MasterVolume | MidiAction::VfoAVolume
                                    | MidiAction::VfoBVolume | MidiAction::TxGain
                                    | MidiAction::Drive | MidiAction::AgcGain
                                    | MidiAction::SqlLevel | MidiAction::CwSpeed
                                    | MidiAction::TuneDrive | MidiAction::MonVolume
                                    | MidiAction::RxBalance | MidiAction::RitOffset
                                    | MidiAction::XitOffset | MidiAction::YaesuVolume
                                    | MidiAction::YaesuRfGain | MidiAction::YaesuMicGain
                                    | MidiAction::SpectrumZoom | MidiAction::SpectrumPan
                                    | MidiAction::RefLevel | MidiAction::WaterfallContrast
                                    | MidiAction::Rx2SpectrumZoom | MidiAction::Rx2SpectrumPan
                                    | MidiAction::Rx2RefLevel | MidiAction::Rx2WaterfallContrast
                                        => crate::midi::ControlType::Slider,
                                    _ => crate::midi::ControlType::Button,
                                }
                            },
                            action: self.midi_learn_action,
                        };
                        self.midi.add_mapping(mapping);
                        self.midi_learn_for = None;
                        self.midi.set_learn_mode(false);
                        self.save_full_config();
                    }
                }
                MidiEvent::Button(action, velocity) => {
                    self.midi_last_event = format!("{} {}", action.label(), if velocity > 0 { "ON" } else { "OFF" });
                    let pressed = velocity > 0;
                    match action {
                        MidiAction::Ptt => {
                            if self.midi_ptt_toggle_mode {
                                // Toggle: press to switch on/off (ignore release)
                                if pressed { self.midi_ptt = !self.midi_ptt; }
                            } else {
                                // Momentary: press=TX, release=RX
                                self.midi_ptt = pressed;
                            }
                        }
                        MidiAction::ModeCycle if pressed => {
                            let modes: &[u8] = &[0, 1, 3, 4, 7, 9, 6, 5]; // LSB USB CWL CWU DIGU DIGL AM FM
                            if let Some(idx) = modes.iter().position(|&m| m == self.mode) {
                                let next = modes[(idx + 1) % modes.len()];
                                let _ = self.cmd_tx.send(Command::SetMode(next));
                            }
                        }
                        MidiAction::BandUp if pressed => {
                            self.midi_band_step(1);
                        }
                        MidiAction::BandDown if pressed => {
                            self.midi_band_step(-1);
                        }
                        MidiAction::NrToggle if pressed => {
                            let new_nr = if self.nr_level > 0 { 0 } else { 2 };
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::NoiseReduction, new_nr as u16));
                        }
                        MidiAction::AnfToggle if pressed => {
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::AutoNotchFilter, if self.anf_on { 0 } else { 1 }));
                        }
                        MidiAction::Rx2Toggle if pressed => {
                            self.rx2_enabled = !self.rx2_enabled;
                            let _ = self.cmd_tx.send(Command::SetRx2Enabled(self.rx2_enabled));
                        }
                        MidiAction::VfoSwap if pressed => {
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::VfoSwap, 2));
                        }
                        MidiAction::PowerToggle if pressed => {
                            let val = if self.power_on { 0 } else { 1 };
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::PowerOnOff, val));
                        }
                        MidiAction::MicAgcToggle if pressed => {
                            let new_val = !self.agc_enabled;
                            let _ = self.cmd_tx.send(Command::SetAgcEnabled(new_val));
                            self.agc_enabled = new_val;
                        }
                        MidiAction::FreqStepUp if pressed => {
                            if self.freq_step_index < freq_steps.len() - 1 {
                                self.freq_step_index += 1;
                            }
                        }
                        MidiAction::FreqStepDown if pressed => {
                            if self.freq_step_index > 0 {
                                self.freq_step_index -= 1;
                            }
                        }
                        MidiAction::FilterWiden if pressed => {
                            // Widen filter: decrease low, increase high by 50 Hz
                            let new_low = self.filter_low_hz - 50;
                            let new_high = self.filter_high_hz + 50;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::FilterLow, new_low as i16 as u16));
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::FilterHigh, new_high as i16 as u16));
                        }
                        MidiAction::FilterNarrow if pressed => {
                            // Narrow filter: increase low, decrease high by 50 Hz
                            let new_low = self.filter_low_hz + 50;
                            let new_high = self.filter_high_hz - 50;
                            if new_high > new_low {
                                let _ = self.cmd_tx.send(Command::SetControl(ControlId::FilterLow, new_low as i16 as u16));
                                let _ = self.cmd_tx.send(Command::SetControl(ControlId::FilterHigh, new_high as i16 as u16));
                            }
                        }
                        MidiAction::NrLevel if pressed => {
                            // Cycle NR level: 0 → 1 → 2 → 3 → 4 → 0
                            let next = (self.nr_level + 1) % 5;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::NoiseReduction, next as u16));
                        }
                        MidiAction::AgcMode if pressed => {
                            // Cycle AGC: 0=Off,1=Long,2=Slow,3=Med,4=Fast
                            let next = (self.agc_mode + 1) % 5;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::AgcMode, next as u16));
                        }
                        MidiAction::NbToggle if pressed => {
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::NoiseBlanker, if self.nb_enable { 0 } else { 1 }));
                        }
                        MidiAction::ApfToggle if pressed => {
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::ApfEnable, if self.apf_enable { 0 } else { 1 }));
                        }
                        MidiAction::VfoLock if pressed => {
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::VfoLock, if self.vfo_lock { 0 } else { 1 }));
                        }
                        MidiAction::RitToggle if pressed => {
                            self.rit_enable = !self.rit_enable;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::RitEnable, self.rit_enable as u16));
                        }
                        MidiAction::XitToggle if pressed => {
                            self.xit_enable = !self.xit_enable;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::XitEnable, self.xit_enable as u16));
                        }
                        MidiAction::SqlToggle if pressed => {
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::SqlEnable, if self.sql_enable { 0 } else { 1 }));
                        }
                        MidiAction::TuneToggle if pressed => {
                            // Toggle tune — send 1 to activate, server handles state
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::ThetisTune, 1));
                        }
                        MidiAction::MuteAll if pressed => {
                            self.mute = !self.mute;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::Mute, self.mute as u16));
                        }
                        MidiAction::Rx1Mute if pressed => {
                            self.rx_mute = !self.rx_mute;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::RxMute, self.rx_mute as u16));
                        }
                        MidiAction::YaesuPtt => {
                            if self.midi_ptt_toggle_mode {
                                if pressed {
                                    let new_tx = !self.yaesu_tx_active;
                                    let _ = self.cmd_tx.send(Command::SetYaesuPtt(new_tx));
                                    self.midi.send_led(MidiAction::YaesuPtt, new_tx);
                                }
                            } else {
                                let _ = self.cmd_tx.send(Command::SetYaesuPtt(pressed));
                                self.midi.send_led(MidiAction::YaesuPtt, pressed);
                            }
                        }
                        _ => {}
                    }
                }
                MidiEvent::Slider(action, value) => {
                    self.midi_last_event = format!("{} = {}", action.label(), value);
                    let frac = value as f32 / 127.0;
                    match action {
                        MidiAction::MasterVolume => {
                            self.rx_volume = frac;
                            let _ = self.cmd_tx.send(Command::SetRxVolume(frac));
                        }
                        MidiAction::VfoAVolume => {
                            // Log scale to match UI slider (0.001..=1.0 logarithmic)
                            self.vfo_a_volume = (0.001_f32 * (1000.0_f32).powf(frac)).max(0.001);
                            let _ = self.cmd_tx.send(Command::SetVfoAVolume(self.vfo_a_volume));
                        }
                        MidiAction::VfoBVolume => {
                            self.vfo_b_volume = (0.001_f32 * (1000.0_f32).powf(frac)).max(0.001);
                            let _ = self.cmd_tx.send(Command::SetVfoBVolume(self.vfo_b_volume));
                        }
                        MidiAction::TxGain => {
                            self.tx_gain = frac * 3.0;
                            let _ = self.cmd_tx.send(Command::SetTxGain(self.tx_gain));
                        }
                        MidiAction::Drive => {
                            let drive = (frac * 100.0).round() as u8;
                            self.drive_level = drive;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::DriveLevel, drive as u16));
                        }
                        MidiAction::SpectrumZoom | MidiAction::Rx2SpectrumZoom => {
                            // Slider: 0=1x, 127=max zoom (logarithmic)
                            let zoom = 1024.0_f32.powf(frac).max(1.0);
                            if matches!(action, MidiAction::Rx2SpectrumZoom) {
                                self.rx2_spectrum_zoom = zoom;
                                self.rx2_zoom_pan_changed_at = Some(std::time::Instant::now());
                            } else {
                                self.spectrum_zoom = zoom;
                                self.zoom_pan_changed_at = Some(std::time::Instant::now());
                            }
                        }
                        MidiAction::SpectrumPan | MidiAction::Rx2SpectrumPan => {
                            // Slider: 0=full left (-1.0), 64=center (0.0), 127=full right (+1.0)
                            let pan = (frac * 2.0 - 1.0).clamp(-1.0, 1.0);
                            if matches!(action, MidiAction::Rx2SpectrumPan) {
                                self.rx2_spectrum_pan = pan;
                                self.rx2_zoom_pan_changed_at = Some(std::time::Instant::now());
                            } else {
                                self.spectrum_pan = pan;
                                self.zoom_pan_changed_at = Some(std::time::Instant::now());
                            }
                        }
                        MidiAction::RefLevel | MidiAction::Rx2RefLevel => {
                            // Slider: 0=-140dB, 127=0dB
                            let ref_db = -140.0 + frac * 140.0;
                            if matches!(action, MidiAction::Rx2RefLevel) {
                                self.rx2_spectrum_ref_db = ref_db;
                                self.rx2_auto_ref_enabled = false;
                            } else {
                                self.spectrum_ref_db = ref_db;
                                self.auto_ref_enabled = false;
                            }
                        }
                        MidiAction::WaterfallContrast | MidiAction::Rx2WaterfallContrast => {
                            // Slider: 0=0.3x, 127=3.0x
                            let contrast = 0.3 + frac * 2.7;
                            if matches!(action, MidiAction::Rx2WaterfallContrast) {
                                self.rx2_waterfall_contrast = contrast;
                            } else {
                                self.waterfall_contrast = contrast;
                            }
                        }
                        MidiAction::AgcGain => {
                            // Slider: 0=-20, 127=120
                            let gain = (-20.0 + frac * 140.0).round() as u16;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::AgcGain, gain));
                        }
                        MidiAction::SqlLevel => {
                            let level = (frac * 160.0).round() as u16;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::SqlLevel, level));
                        }
                        MidiAction::CwSpeed => {
                            let wpm = (1.0 + frac * 59.0).round() as u16;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::CwKeyerSpeed, wpm));
                        }
                        MidiAction::TuneDrive => {
                            let drive = (frac * 100.0).round() as u16;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::TuneDrive, drive));
                        }
                        MidiAction::MonVolume => {
                            // Slider: 0=-40dB, 127=0dB
                            let db = (-40.0 + frac * 40.0).round() as i16;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::MonitorVolume, db as u16));
                        }
                        MidiAction::RxBalance => {
                            // Slider: 0=-40, 64=0, 127=+40
                            let bal = (-40.0 + frac * 80.0).round() as i16;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::RxBalance, bal as u16));
                        }
                        MidiAction::RitOffset => {
                            // Slider: 0-127 → ±1270 Hz in 20 Hz steps (center=0)
                            let hz = ((value as i16 - 64) * 20).clamp(-9999, 9999);
                            self.rit_offset = hz;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::RitOffset, hz as u16));
                        }
                        MidiAction::XitOffset => {
                            let hz = ((value as i16 - 64) * 20).clamp(-9999, 9999);
                            self.xit_offset = hz;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::XitOffset, hz as u16));
                        }
                        MidiAction::YaesuVolume => {
                            // Log scale: 0.001..1.0
                            self.yaesu_volume = (0.001_f32 * (1000.0_f32).powf(frac)).max(0.001);
                            let _ = self.cmd_tx.send(Command::SetYaesuVolume(self.yaesu_volume));
                        }
                        MidiAction::YaesuRfGain => {
                            self.yaesu_rf_gain = (frac * 255.0).round() as u16;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuRfGain, self.yaesu_rf_gain));
                        }
                        MidiAction::YaesuMicGain => {
                            self.yaesu_radio_mic_gain = (frac * 100.0).round() as u16;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuRadioMicGain, self.yaesu_radio_mic_gain));
                        }
                        _ => {}
                    }
                }
                MidiEvent::Encoder(action, delta) => {
                    self.midi_last_event = format!("{} delta={}", action.label(), delta);
                    let step = self.midi_encoder_hz;
                    match action {
                        MidiAction::VfoATune => {
                            // Clamp to +/-1 on direction change (encoder backlash compensation)
                            let dir = if delta > 0 { 1i8 } else { -1 };
                            let clamped = if dir != self.midi_last_dir_a && self.midi_last_dir_a != 0 {
                                dir
                            } else {
                                delta
                            };
                            self.midi_last_dir_a = dir;
                            let new_freq = (self.frequency_hz as i64 + clamped as i64 * step as i64).max(0) as u64;
                            let _ = self.cmd_tx.send(Command::SetFrequency(new_freq));
                            self.set_pending_freq_a(new_freq);
                        }
                        MidiAction::VfoBTune => {
                            let dir = if delta > 0 { 1i8 } else { -1 };
                            let clamped = if dir != self.midi_last_dir_b && self.midi_last_dir_b != 0 {
                                dir
                            } else {
                                delta
                            };
                            self.midi_last_dir_b = dir;
                            let new_freq = (self.rx2_frequency_hz as i64 + clamped as i64 * step as i64).max(0) as u64;
                            let _ = self.cmd_tx.send(Command::SetFrequencyRx2(new_freq));
                            self.set_pending_freq_b(new_freq);
                        }
                        MidiAction::SpectrumZoom | MidiAction::Rx2SpectrumZoom => {
                            let factor = 1.1_f32.powi(delta as i32);
                            if matches!(action, MidiAction::Rx2SpectrumZoom) {
                                self.rx2_spectrum_zoom = (self.rx2_spectrum_zoom * factor).clamp(1.0, 1024.0);
                                self.rx2_zoom_pan_changed_at = Some(std::time::Instant::now());
                            } else {
                                self.spectrum_zoom = (self.spectrum_zoom * factor).clamp(1.0, 1024.0);
                                self.zoom_pan_changed_at = Some(std::time::Instant::now());
                            }
                        }
                        MidiAction::SpectrumPan | MidiAction::Rx2SpectrumPan => {
                            let pan_step = 0.05 * delta as f32;
                            if matches!(action, MidiAction::Rx2SpectrumPan) {
                                self.rx2_spectrum_pan = (self.rx2_spectrum_pan + pan_step).clamp(-1.0, 1.0);
                                self.rx2_zoom_pan_changed_at = Some(std::time::Instant::now());
                            } else {
                                self.spectrum_pan = (self.spectrum_pan + pan_step).clamp(-1.0, 1.0);
                                self.zoom_pan_changed_at = Some(std::time::Instant::now());
                            }
                        }
                        MidiAction::RefLevel | MidiAction::Rx2RefLevel => {
                            if matches!(action, MidiAction::Rx2RefLevel) {
                                self.rx2_spectrum_ref_db = (self.rx2_spectrum_ref_db + delta as f32).clamp(-140.0, 0.0);
                                self.rx2_auto_ref_enabled = false;
                            } else {
                                self.spectrum_ref_db = (self.spectrum_ref_db + delta as f32).clamp(-140.0, 0.0);
                                self.auto_ref_enabled = false;
                            }
                        }
                        MidiAction::WaterfallContrast | MidiAction::Rx2WaterfallContrast => {
                            let factor = 1.1_f32.powi(delta as i32);
                            if matches!(action, MidiAction::Rx2WaterfallContrast) {
                                self.rx2_waterfall_contrast = (self.rx2_waterfall_contrast * factor).clamp(0.3, 3.0);
                            } else {
                                self.waterfall_contrast = (self.waterfall_contrast * factor).clamp(0.3, 3.0);
                            }
                        }
                        MidiAction::NrLevel => {
                            // Encoder: up = increase, down = decrease, clamp 0-4
                            let new_level = (self.nr_level as i32 + if delta > 0 { 1 } else { -1 }).clamp(0, 4) as u8;
                            if new_level != self.nr_level {
                                let _ = self.cmd_tx.send(Command::SetControl(ControlId::NoiseReduction, new_level as u16));
                            }
                        }
                        MidiAction::RitOffset => {
                            let new_hz = (self.rit_offset as i32 + delta as i32 * 10).clamp(-9999, 9999) as i16;
                            self.rit_offset = new_hz;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::RitOffset, new_hz as u16));
                        }
                        MidiAction::XitOffset => {
                            let new_hz = (self.xit_offset as i32 + delta as i32 * 10).clamp(-9999, 9999) as i16;
                            self.xit_offset = new_hz;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::XitOffset, new_hz as u16));
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    pub(super) fn midi_band_step(&mut self, direction: i32) {
        const BANDS: &[(&str, u64)] = &[
            ("160m", 1_900_000), ("80m", 3_700_000), ("60m", 5_351_000),
            ("40m", 7_100_000), ("30m", 10_120_000), ("20m", 14_200_000),
            ("17m", 18_100_000), ("15m", 21_200_000), ("12m", 24_930_000),
            ("10m", 28_500_000), ("6m", 50_200_000),
        ];
        let current = band_label(self.frequency_hz);
        let idx = BANDS.iter().position(|&(name, _)| name == current);
        let new_idx = match idx {
            Some(i) => (i as i32 + direction).rem_euclid(BANDS.len() as i32) as usize,
            None => 0,
        };
        self.save_current_band(Vfo::A);
        self.restore_band(Vfo::A, BANDS[new_idx].0, BANDS[new_idx].1);
        self.save_full_config();
    }

    pub(super) fn render_midi_screen(&mut self, ui: &mut egui::Ui) {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));

        // Device selection
        ui.horizontal(|ui| {
            ui.label("MIDI Device:");
            if ui.button("Refresh").clicked() {
                self.midi_ports = crate::midi::MidiManager::list_ports();
            }
        });

        if self.midi_ports.is_empty() && !self.midi.is_connected() {
            ui.label("No MIDI devices found. Click Refresh to scan.");
        } else {
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("midi_port")
                    .selected_text(if self.midi_selected_port.is_empty() {
                        "Select device..."
                    } else {
                        &self.midi_selected_port
                    })
                    .show_ui(ui, |ui| {
                        for port in &self.midi_ports {
                            ui.selectable_value(&mut self.midi_selected_port, port.clone(), port);
                        }
                    });

                if self.midi.is_connected() {
                    if ui.button("Disconnect").clicked() {
                        self.midi.disconnect();
                    }
                    ui.colored_label(Color32::GREEN, "Connected");
                } else {
                    let can_connect = !self.midi_selected_port.is_empty();
                    if ui.add_enabled(can_connect, egui::Button::new("Connect")).clicked() {
                        if self.midi.connect(&self.midi_selected_port) {
                            self.save_full_config();
                        }
                    }
                    ui.colored_label(Color32::RED, "Disconnected");
                }
            });
        }

        ui.separator();

        // MIDI PTT mode (independent from main PTT mode)
        ui.horizontal(|ui| {
            ui.label("MIDI PTT:");
            if ui.selectable_label(!self.midi_ptt_toggle_mode, "Push to talk").clicked() {
                self.midi_ptt_toggle_mode = false;
                self.save_ptt_config();
            }
            if ui.selectable_label(self.midi_ptt_toggle_mode, "Toggle").clicked() {
                self.midi_ptt_toggle_mode = true;
                self.save_ptt_config();
            }
        });

        // Encoder step setting
        ui.horizontal(|ui| {
            ui.label("Encoder step:");
            let steps: &[u64] = &[1, 10, 100, 500, 1000];
            let labels = ["1 Hz", "10 Hz", "100 Hz", "500 Hz", "1 kHz"];
            for (i, &step) in steps.iter().enumerate() {
                let btn = if self.midi_encoder_hz == step {
                    egui::Button::new(RichText::new(labels[i]).size(11.0).strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else {
                    egui::Button::new(RichText::new(labels[i]).size(11.0))
                };
                if ui.add(btn).clicked() {
                    self.midi_encoder_hz = step;
                    self.save_full_config();
                }
            }
        });

        ui.separator();

        // Activity monitor
        if !self.midi_last_event.is_empty() {
            ui.horizontal(|ui| {
                ui.label("Last MIDI:");
                ui.monospace(&self.midi_last_event);
            });
            ui.separator();
        }

        // Mappings table
        ui.label(RichText::new("Mappings").strong());

        let mappings = self.midi.get_mappings();
        let mut remove_idx: Option<usize> = None;

        egui::Grid::new("midi_mappings")
            .striped(true)
            .min_col_width(60.0)
            .show(ui, |ui| {
                ui.label(RichText::new("Source").strong());
                ui.label(RichText::new("Type").strong());
                ui.label(RichText::new("Action").strong());
                ui.label("");
                ui.end_row();

                for (i, mapping) in mappings.iter().enumerate() {
                    ui.monospace(mapping.source_label());
                    ui.label(mapping.control_type.label());
                    ui.label(mapping.action.label());
                    if ui.small_button("X").clicked() {
                        remove_idx = Some(i);
                    }
                    ui.end_row();
                }
            });

        if let Some(idx) = remove_idx {
            self.midi.remove_mapping(idx);
            self.save_full_config();
        }

        ui.add_space(8.0);

        // Learn mode / Add mapping
        if let Some(_) = self.midi_learn_for {
            ui.horizontal(|ui| {
                ui.label("Learning:");
                ui.label(RichText::new(self.midi_learn_action.label()).strong());
                ui.label("- Move a control on your MIDI device...");
                if ui.button("Cancel").clicked() {
                    self.midi_learn_for = None;
                    self.midi.set_learn_mode(false);
                }
            });
        } else {
            ui.horizontal(|ui| {
                ui.label("Add:");
                egui::ComboBox::from_id_salt("midi_learn_action")
                    .selected_text(self.midi_learn_action.label())
                    .show_ui(ui, |ui| {
                        for action in crate::midi::MidiAction::ALL {
                            ui.selectable_value(
                                &mut self.midi_learn_action,
                                *action,
                                action.label(),
                            );
                        }
                    });
                if ui.button("Learn").clicked() && self.midi.is_connected() {
                    self.midi_learn_for = Some(mappings.len());
                    self.midi.set_learn_mode(true);
                }
            });
        }
    }
}







