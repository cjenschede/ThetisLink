// SPDX-License-Identifier: GPL-2.0-or-later
//! `SdrRemoteApp::render_thetis_screen`: the main "Thetis" tab - power toggle, the
//! RX1/RX2 area and the primary radio controls that live on that screen. Extracted
//! verbatim from `ui/screens.rs` - pure relocation, no behaviour change. `pub(super)`
//! keeps it callable from the parent module tree.

use super::*;

impl SdrRemoteApp {
    /// Launch Thetis on the server PC once per client run, if the user opted in
    /// via the Thetis-tab checkbox and the server explicitly reports that
    /// Thetis is not running.
    ///
    /// Sends the same command as a short press on the Power button. Fires at
    /// most once (`thetis_autostart_fired`): a launch that fails, or a Thetis
    /// the user shuts down later on purpose, must not be re-triggered on the
    /// next reconnect. `Some(false)` is required - an old server that reports
    /// no process state (`None`) never triggers a launch.
    pub(super) fn maybe_autostart_thetis(&mut self) {
        if !self.thetis_autostart
            || self.thetis_autostart_fired
            || !self.connected
            || !self.thetis_configured
            || self.thetis_starting
        {
            return;
        }
        let thetis_down = self
            .state_rx
            .borrow()
            .connect_status
            .thetis_reported_not_running();
        if !thetis_down {
            return;
        }
        self.thetis_autostart_fired = true;
        log::info!("Thetis autostart: server reports Thetis not running, sending power-on");
        let _ = self.cmd_tx.send(Command::SetControl(ControlId::PowerOnOff, 1));
    }

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
            (Color32::from_rgb(200, 0, 0), rust_i18n::t!("screen_shutdown").to_string())
        } else if hold_progress > 0.0 {
            let r = if self.power_on { (0.0 + 200.0 * hold_progress) as u8 } else { 150 };
            let g = if self.power_on { (150.0 * (1.0 - hold_progress)) as u8 } else { 0 };
            (Color32::from_rgb(r, g, 0), rust_i18n::t!("screen_hold").to_string())
        } else if self.thetis_starting {
            (Color32::from_rgb(180, 130, 0), rust_i18n::t!("screen_starting").to_string())
        } else if self.power_on {
            (Color32::from_rgb(0, 150, 0), rust_i18n::t!("screen_power_on").to_string())
        } else {
            (Color32::from_rgb(150, 0, 0), rust_i18n::t!("screen_power_off").to_string())
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

            // Automates exactly what the button above does, once per client run.
            if ui.checkbox(&mut self.thetis_autostart, rust_i18n::t!("screen_thetis_autostart").to_string())
                .on_hover_text(rust_i18n::t!("screen_thetis_autostart_hover").to_string())
                .changed()
            {
                crate::ui::config::save_thetis_autostart(self.thetis_autostart);
            }
        });

        ui.separator();

        // TX Profile dropdown
        if !self.tx_profiles.is_empty() {
            ui.horizontal(|ui| {
                ui.label(rust_i18n::t!("screen_tx_profile").to_string());
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

        // TX modulation bandwidth (main-radio TX) - PATCH-tx-modulation-bandwidth.
        // "Volg RX" mirrors the RX filter 1:1; otherwise set low/high directly.
        ui.separator();
        ui.label(RichText::new(rust_i18n::t!("screen_tx_modulation_bandwidth").to_string()).strong());
        if !self.tx_filter_supported {
            ui.label(RichText::new(rust_i18n::t!("screen_tx_filter_unavailable").to_string()).weak());
        } else {
            if ui.checkbox(&mut self.tx_filter_follow_rx, rust_i18n::t!("screen_follow_rx_bandwidth").to_string()).changed() {
                self.last_tx_follow_sent = None; // force resend when following
                if !self.tx_filter_follow_rx {
                    // Switched to independent -> apply the current manual band now.
                    let _ = self.cmd_tx.send(Command::SetTxFilter(self.tx_filter_low_hz, self.tx_filter_high_hz));
                }
            }
            ui.horizontal(|ui| {
                let editable = !self.tx_filter_follow_rx;
                ui.label(rust_i18n::t!("screen_low").to_string());
                let lo = ui.add_enabled(editable,
                    egui::DragValue::new(&mut self.tx_filter_low_hz).range(0..=8000).suffix(" Hz").speed(10));
                ui.label(rust_i18n::t!("screen_high").to_string());
                let hi = ui.add_enabled(editable,
                    egui::DragValue::new(&mut self.tx_filter_high_hz).range(0..=8000).suffix(" Hz").speed(10));
                if editable && (lo.changed() || hi.changed()) {
                    let _ = self.cmd_tx.send(Command::SetTxFilter(self.tx_filter_low_hz, self.tx_filter_high_hz));
                }
            });
            if self.tx_filter_follow_rx {
                // Show the actual (positive) audio passband that follows RX,
                // clamped to the TX limit: TX audio is 16 kS/s, so modulation
                // tops out at 8 kHz. A wider RX filter cannot be followed.
                let (tlo, thi) = rx_to_tx_band(self.filter_low_hz, self.filter_high_hz);
                let chi = thi.min(8000);
                if thi > 8000 {
                    ui.label(RichText::new(
                        rust_i18n::t!("screen_tx_follows_rx_clamped", lo = tlo, hi = chi).to_string()).weak());
                } else {
                    ui.label(RichText::new(
                        rust_i18n::t!("screen_tx_follows_rx", lo = tlo, hi = chi).to_string()).weak());
                }
            }
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
            let resp = ui.add(slider);
            let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.rx_volume, 0.0..=1.0, 0.02);
            if resp.changed() || scrolled {
                let _ = self.cmd_tx.send(Command::SetRxVolume(self.rx_volume));
                self.save_full_config();
            }
        });

        // RX2 Volume slider
        ui.horizontal(|ui| {
            ui.label("RX2 Vol: ");
            let slider = egui::Slider::new(&mut self.rx2_volume, 0.0..=1.0)
                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
            let resp = ui.add(slider);
            let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.rx2_volume, 0.0..=1.0, 0.02);
            if resp.changed() || scrolled {
                let _ = self.cmd_tx.send(Command::SetRx2Volume(self.rx2_volume));
                self.save_full_config();
            }
        });

        // TX Gain slider
        ui.horizontal(|ui| {
            ui.label("TX Gain: ");
            let slider = egui::Slider::new(&mut self.tx_gain, 0.0..=3.0)
                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
            let resp = ui.add(slider);
            let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.tx_gain, 0.0..=3.0, 0.05);
            if resp.changed() || scrolled {
                let _ = self.cmd_tx.send(Command::SetTxGain(self.tx_gain));
                self.save_full_config();
            }
        });

        // S-meter source selector - mirrors Thetis Multimeter Sig/Avg/MaxBin
        // choice. Applies to both RX1 and RX2 (one shared display mode).
        ui.horizontal(|ui| {
            ui.label("S-meter:");
            for (val, label, tooltip) in [
                (0u8, "Sig", "WDSP RXA_S_PK - peak-hold with 100ms decay (fast bouncing)"),
                (1u8, "Avg", "WDSP RXA_S_AV - symmetric RMS-avg over linear power (recommended)"),
                (2u8, "MaxBin", "Single highest FFT bin in passband"),
            ] {
                if ui.selectable_label(self.smeter_source == val, label).on_hover_text(tooltip).clicked() {
                    self.smeter_source = val;
                    let _ = self.cmd_tx.send(Command::SetSmeterSource(val));
                    crate::ui::config::save_smeter_source(val);
                }
            }
        });

        ui.separator();

        // --- Thetis PTT mode ---
        ui.horizontal(|ui| {
            ui.label("Thetis PTT:");
            if ui.selectable_label(!self.ptt_toggle_mode, rust_i18n::t!("screen_push_to_talk").to_string()).clicked() {
                self.ptt_toggle_mode = false;
                self.save_ptt_config();
            }
            if ui.selectable_label(self.ptt_toggle_mode, rust_i18n::t!("screen_toggle").to_string()).clicked() {
                self.ptt_toggle_mode = true;
                self.save_ptt_config();
            }
        });

        self.render_websdr_controls(ui, CatSyncTarget::Thetis, self.frequency_hz, self.mode);

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

                ui.label(rust_i18n::t!("screen_audio_label").to_string());
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
                    let resp = ui.add_sized([80.0, 16.0], bal_slider);
                    let scrolled = super::helpers::slider_wheel(ui, &resp, &mut bal, -40.0..=40.0, 1.0);
                    if resp.changed() || scrolled {
                        // Negate: slider visual left (-) -> TCI +40 (which is left audio in Thetis)
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
                    if ui.checkbox(&mut auto_agc, rust_i18n::t!("screen_auto").to_string()).changed() {
                        self.agc_auto_rx1 = auto_agc;
                        let _ = self.cmd_tx.send(Command::SetControl(CId::AgcAutoRx1, auto_agc as u16));
                    }
                    ui.label(rust_i18n::t!("screen_gain").to_string());
                    let mut gain = self.agc_gain as f32;
                    let gain_slider = egui::Slider::new(&mut gain, 0.0..=120.0)
                        .custom_formatter(|v, _| format!("{:.0}", v));
                    let resp = ui.add_sized([120.0, 16.0], gain_slider);
                    let scrolled = super::helpers::slider_wheel(ui, &resp, &mut gain, 0.0..=120.0, 2.0);
                    if resp.changed() || scrolled {
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
                let resp = ui.add_sized([120.0, 16.0], rit_slider);
                let scrolled = super::helpers::slider_wheel(ui, &resp, &mut rit_hz, -9999.0..=9999.0, 10.0);
                if resp.changed() || scrolled {
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
                let resp = ui.add_sized([120.0, 16.0], xit_slider);
                let scrolled = super::helpers::slider_wheel(ui, &resp, &mut xit_hz, -9999.0..=9999.0, 10.0);
                if resp.changed() || scrolled {
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
                let resp = ui.add_sized([100.0, 16.0], sql_slider);
                let scrolled = super::helpers::slider_wheel(ui, &resp, &mut sql_val, 0.0..=160.0, 2.0);
                if resp.changed() || scrolled {
                    self.sql_level = sql_val as u8;
                    tci_set!(self, CId::SqlLevel, self.sql_level as u16);
                }

                ui.separator();

                ui.label("CW:");
                let mut cw_spd = self.cw_keyer_speed as f32;
                let cw_slider = egui::Slider::new(&mut cw_spd, 1.0..=60.0)
                    .suffix(" WPM")
                    .custom_formatter(|v, _| format!("{:.0}", v));
                let resp = ui.add_sized([120.0, 16.0], cw_slider);
                let scrolled = super::helpers::slider_wheel(ui, &resp, &mut cw_spd, 1.0..=60.0, 1.0);
                if resp.changed() || scrolled {
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
                let resp = ui.add_sized([100.0, 16.0], td_slider);
                let scrolled = super::helpers::slider_wheel(ui, &resp, &mut td, 0.0..=100.0, 2.0);
                if resp.changed() || scrolled {
                    self.tune_drive = td as u8;
                    tci_set!(self, CId::TuneDrive, self.tune_drive as u16);
                }

                ui.separator();

                ui.label("Mon:");
                let mut mv = self.mon_volume as f32;
                let mv_slider = egui::Slider::new(&mut mv, -40.0..=0.0)
                    .suffix(" dB")
                    .custom_formatter(|v, _| format!("{:.0}", v));
                let resp = ui.add_sized([100.0, 16.0], mv_slider);
                let scrolled = super::helpers::slider_wheel(ui, &resp, &mut mv, -40.0..=0.0, 1.0);
                if resp.changed() || scrolled {
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
                    if ui.checkbox(&mut auto_agc, rust_i18n::t!("screen_auto").to_string()).changed() {
                        self.agc_auto_rx2 = auto_agc;
                        let _ = self.cmd_tx.send(Command::SetControl(CId::AgcAutoRx2, auto_agc as u16));
                    }
                    ui.label(rust_i18n::t!("screen_gain").to_string());
                    let mut gain = self.rx2_agc_gain as f32;
                    let gain_slider = egui::Slider::new(&mut gain, 0.0..=120.0)
                        .custom_formatter(|v, _| format!("{:.0}", v));
                    let resp = ui.add_sized([120.0, 16.0], gain_slider);
                    let scrolled = super::helpers::slider_wheel(ui, &resp, &mut gain, 0.0..=120.0, 2.0);
                    if resp.changed() || scrolled {
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
                let resp = ui.add_sized([100.0, 16.0], sql_slider);
                let scrolled = super::helpers::slider_wheel(ui, &resp, &mut sql_val, 0.0..=160.0, 2.0);
                if resp.changed() || scrolled {
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
}
