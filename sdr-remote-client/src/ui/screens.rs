// SPDX-License-Identifier: GPL-2.0-or-later

use super::*;

/// Map een `PacketType`-byte naar een leesbare label voor de
/// Server-tab bandwidth-breakdown. Onbekende types vallen terug op
/// hex-notatie zodat nieuwe packet-types ook zichtbaar zijn zonder
/// hier code aan te raken.
pub(super) fn packet_type_label(t: u8) -> String {
    match t {
        0x01 => "Audio (legacy mono)".into(),
        0x03 => "HeartbeatAck".into(),
        0x04 => "Disconnect".into(),
        0x07 => "Frequency".into(),
        0x08 => "Mode".into(),
        0x09 => "S-meter RX1 Avg".into(),
        0x0A => "Spectrum RX1".into(),
        0x0B => "Full spectrum RX1".into(),
        0x0C => "Equipment status".into(),
        0x0E => "Audio RX2".into(),
        0x0F => "Frequency RX2".into(),
        0x10 => "Mode RX2".into(),
        0x11 => "S-meter RX2 Avg".into(),
        0x12 => "Spectrum RX2".into(),
        0x13 => "Full spectrum RX2".into(),
        0x14 => "DX spot".into(),
        0x15 => "TX profiles".into(),
        0x16 => "Audio Yaesu 1".into(),
        0x17 => "Yaesu 1 state".into(),
        0x19 => "Yaesu 1 memory data".into(),
        0x1A => "Audio BinR (deprecated)".into(),
        0x1B => "Audio RX1+RX2".into(),
        0x1C => "S-meter RX1 Sig".into(),
        0x1D => "S-meter RX1 MaxBin".into(),
        0x1E => "S-meter RX2 Sig".into(),
        0x1F => "S-meter RX2 MaxBin".into(),
        0x20 => "Amplitec power table".into(),
        0x21 => "Audio VRX (VRX1+VRX2)".into(),
        0x22 => "VRX frequency".into(),
        0x23 => "Spectrum VRX1 (high-res)".into(),
        0x24 => "Spectrum VRX2 (high-res)".into(),
        0x25 => "Audio Yaesu 2".into(),
        0x26 => "Yaesu 2 state".into(),
        0x30 => "Auth challenge".into(),
        0x32 => "Auth result".into(),
        0x33 => "TOTP challenge".into(),
        _ => format!("0x{:02X} unknown", t),
    }
}

impl SdrRemoteApp {
    pub(super) fn catsync_target_freq_mode(&self) -> (u64, u8) {
        match self.catsync_target {
            CatSyncTarget::Thetis => (self.frequency_hz, self.mode),
            CatSyncTarget::Yaesu1 => (self.yaesu_freq_a, self.yaesu_mode),
            CatSyncTarget::Yaesu2 => (self.yaesu2_freq_a, self.yaesu2_mode),
        }
    }

    pub(super) fn catsync_target_tx_active(&self) -> bool {
        match self.catsync_target {
            CatSyncTarget::Thetis => self.ptt,
            CatSyncTarget::Yaesu1 => self.yaesu_tx_active,
            CatSyncTarget::Yaesu2 => self.yaesu2_tx_active,
        }
    }

    pub(super) fn render_websdr_controls(
        &mut self,
        ui: &mut egui::Ui,
        target: CatSyncTarget,
        freq_hz: u64,
        mode: u8,
    ) {
        ui.horizontal(|ui| {
            if ui.checkbox(&mut self.catsync.enabled, rust_i18n::t!("screen_websdr_mute_on_tx").to_string()).changed() {
                if !self.catsync.enabled {
                    self.catsync.force_unmute();
                }
                self.save_full_config();
            }
            if self.catsync_target == target && self.catsync.webview_open() {
                if self.catsync.is_muted() {
                    ui.colored_label(Color32::from_rgb(255, 165, 0), rust_i18n::t!("screen_muted").to_string());
                } else {
                    ui.colored_label(Color32::from_rgb(100, 100, 100), rust_i18n::t!("screen_listening").to_string());
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let webview_open = self.catsync.webview_open();
                let target_open = webview_open && self.catsync_target == target;
                if target_open {
                    if ui.button(rust_i18n::t!("screen_close_websdr").to_string()).clicked() {
                        self.catsync.close_websdr_window();
                    }
                    if ui.button(rust_i18n::t!("screen_reload").to_string()).on_hover_text(
                        rust_i18n::t!("screen_reload_websdr_tooltip").to_string(),
                    ).clicked() {
                        self.catsync_target = target;
                        self.catsync.websdr_url = self.websdr_urls[target.idx()].clone();
                        self.catsync.reload_websdr_window(freq_hz, mode);
                    }
                    ui.colored_label(Color32::from_rgb(100, 200, 100), rust_i18n::t!("screen_window_open").to_string());
                } else {
                    let button_label = if webview_open { rust_i18n::t!("screen_use_here").to_string() } else { "WebSDR".to_string() };
                    if ui.button(button_label).clicked() {
                        self.catsync_target = target;
                        self.catsync.websdr_url = self.websdr_urls[target.idx()].clone();
                        if webview_open {
                            self.catsync.reload_websdr_window(freq_hz, mode);
                        } else {
                            self.catsync.open_websdr_window(freq_hz, mode);
                        }
                    }
                }
                if ui.small_button("ext").on_hover_text(rust_i18n::t!("screen_open_in_external_browser").to_string()).clicked() {
                    let url = crate::catsync::build_tune_url(&self.websdr_urls[target.idx()], freq_hz, mode);
                    let _ = open::that(&url);
                }
            });
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("URL:").size(11.0).color(Color32::GRAY));
            ui.add(egui::TextEdit::singleline(&mut self.websdr_urls[target.idx()])
                .desired_width(ui.available_width() - 40.0)
                .font(egui::FontId::proportional(11.0)));
            if ui.small_button("*").on_hover_text(rust_i18n::t!("screen_add_to_favorites").to_string()).clicked() {
                let url = self.websdr_urls[target.idx()].clone();
                self.catsync.add_favorite_url(&url);
                self.save_full_config();
            }
        });
        if !self.catsync.favorites.is_empty() {
            let active_url = self.websdr_urls[target.idx()].clone();
            let editing = self.websdr_favorite_editing;
            let mut select_idx = None;
            let mut remove_idx = None;
            let mut edit_idx: Option<Option<usize>> = None;
            let mut label_committed = false;
            for (i, (label, url)) in self.catsync.favorites.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    let active = *url == active_url;
                    let text_color = if active { Color32::from_rgb(100, 200, 100) } else { Color32::GRAY };
                    if editing == Some(i) {
                        let resp = ui.add(
                            egui::TextEdit::singleline(label)
                                .desired_width(180.0)
                                .font(egui::FontId::proportional(11.0))
                                .text_color(text_color),
                        );
                        if !resp.has_focus() && !resp.gained_focus() {
                            resp.request_focus();
                        }
                        let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if enter || resp.lost_focus() {
                            edit_idx = Some(None);
                            label_committed = true;
                        }
                    } else {
                        let text = RichText::new(label.as_str()).size(11.0).color(text_color);
                        if ui.add(egui::Label::new(text).sense(egui::Sense::click())).clicked() {
                            select_idx = Some(i);
                        }
                    }
                    let type_label = if crate::catsync::is_kiwi_url(url) { "kiwi" } else { "wsdr" };
                    ui.label(RichText::new(type_label).size(9.0).color(Color32::DARK_GRAY));
                    let edit_label = if editing == Some(i) { rust_i18n::t!("screen_done").to_string() } else { rust_i18n::t!("screen_edit").to_string() };
                    if ui.small_button(edit_label).on_hover_text(rust_i18n::t!("screen_rename_favorite").to_string()).clicked() {
                        if editing == Some(i) {
                            edit_idx = Some(None);
                            label_committed = true;
                        } else {
                            edit_idx = Some(Some(i));
                        }
                    }
                    if ui.small_button("X").on_hover_text(rust_i18n::t!("screen_remove").to_string()).clicked() {
                        remove_idx = Some(i);
                    }
                });
            }
            if let Some(new_editing) = edit_idx {
                self.websdr_favorite_editing = new_editing;
            }
            if label_committed {
                self.save_full_config();
            }
            if let Some(sel) = select_idx {
                if let Some((_, url)) = self.catsync.favorites.get(sel) {
                    self.websdr_urls[target.idx()] = url.clone();
                }
                self.save_full_config();
            }
            if let Some(idx) = remove_idx {
                if self.websdr_favorite_editing == Some(idx) {
                    self.websdr_favorite_editing = None;
                }
                self.catsync.remove_favorite(idx);
                self.save_full_config();
            }
        }
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

    pub(super) fn render_server_screen(&mut self, ui: &mut egui::Ui) {
        // Repaint at 30fps when connected (live audio levels), slow when idle
        let ms = if self.connected { 33 } else { 500 };
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(ms));

        // Server address + password, plus a right-anchored "Re-run setup
        // wizard" button. Right-to-left layout keeps the wizard button
        // glued to the right edge regardless of window width and never
        // pushes the address/password fields off-screen on narrow windows
        // - PATCH-4 follow-up: the previous bottom-of-screen placement
        // disappeared off the visible viewport on short windows.
        ui.horizontal(|ui| {
            let enabled = !self.connected;
            if self.relay_external {
                // Relay is de actieve transport: het directe server-IP is niet de
                // route (de relay bepaalt de bestemming via station/token). Toon de
                // relay-bestemming + live status i.p.v. een misleidend IP-veld.
                ui.label(rust_i18n::t!("via_relay").to_string());
                let station = self.relay_station.trim();
                ui.label(
                    egui::RichText::new(if station.is_empty() { rust_i18n::t!("screen_station_placeholder").to_string() } else { station.to_string() })
                        .strong(),
                );
                if let Some(handle) = self.relay_status.as_ref() {
                    let status = handle.snapshot();
                    let color = match status.phase {
                        sdr_remote_relay::RelayPhase::Authenticated => Color32::from_rgb(50, 200, 50),
                        sdr_remote_relay::RelayPhase::Connecting
                        | sdr_remote_relay::RelayPhase::WaitingForPeer
                        | sdr_remote_relay::RelayPhase::WaitingForConfig => Color32::from_rgb(200, 160, 40),
                        sdr_remote_relay::RelayPhase::Error => Color32::from_rgb(220, 60, 60),
                        sdr_remote_relay::RelayPhase::Disabled
                        | sdr_remote_relay::RelayPhase::Disconnected => Color32::from_rgb(130, 130, 130),
                    };
                    let msg = status.message.clone();
                    let shown = if matches!(status.phase, sdr_remote_relay::RelayPhase::Error) && !msg.is_empty() {
                        msg.clone()
                    } else {
                        status.label()
                    };
                    ui.colored_label(color, shown).on_hover_text(msg);
                }
            } else {
                ui.label(rust_i18n::t!("screen_server").to_string());
                ui.add_enabled(enabled, egui::TextEdit::singleline(&mut self.server_input).desired_width(150.0));
            }
            ui.label(rust_i18n::t!("screen_password").to_string());
            ui.add_enabled(enabled, egui::TextEdit::singleline(&mut self.password_input)
                .desired_width(100.0).password(true).hint_text(rust_i18n::t!("screen_required_hint").to_string()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let tip = rust_i18n::t!("screen_rerun_setup_wizard").to_string();
                if ui.small_button(rust_i18n::t!("screen_wizard").to_string()).on_hover_text(tip).clicked() {
                    self.wizard_state = Some(super::wizard::WizardState::new(
                        self.server_input.clone(),
                        self.password_input.clone(),
                    ));
                }
                let recenter_tip = rust_i18n::t!("screen_recenter_windows_tooltip").to_string();
                if ui.small_button(rust_i18n::t!("screen_recenter_windows").to_string()).on_hover_text(recenter_tip).clicked() {
                    let ctx = ui.ctx().clone();
                    self.recenter_popouts(&ctx);
                }
            });
        });

        // PATCH-3: mDNS-discovered servers shown progressively as they
        // arrive. The browse runs in a background thread; the UI just
        // snapshots the latest list each frame. Empty list (no mDNS yet
        // or no servers on this subnet) is the steady-state - the user
        // can still type an IP above.
        //
        // Removal happens via mdns-sd's own `ServiceRemoved` event (TTL
        // expiry or explicit goodbye) - we do NOT time-prune client-side
        // because `ServiceResolved` only fires on first-resolve / change,
        // so a stable server would otherwise get pruned ~30s after the
        // initial scan even while it kept advertising. Refresh button
        // below re-arms the browse for the user who wants to force a sweep.
        if !self.connected {
            if let Some(ref handle) = self.mdns_browse {
                let servers = handle.snapshot();
                ui.horizontal(|ui| {
                    if !servers.is_empty() {
                        ui.label(rust_i18n::t!("screen_found").to_string());
                        egui::ComboBox::from_id_salt("mdns_server_picker")
                            .selected_text(rust_i18n::t!("screen_choose_discovered_server", n = servers.len()).to_string())
                            .width(260.0)
                            .show_ui(ui, |ui| {
                                for srv in &servers {
                                    if ui.selectable_label(false, srv.display_label()).clicked() {
                                        self.server_input = srv.addr_port.clone();
                                    }
                                }
                            });
                    } else {
                        ui.label(egui::RichText::new(rust_i18n::t!("screen_scanning_local_network").to_string()).size(11.0).color(egui::Color32::from_rgb(150, 150, 150)));
                    }
                    if ui.button(rust_i18n::t!("screen_refresh").to_string()).clicked() {
                        // Re-arm the browse: drop the current daemon (its
                        // worker thread exits on receiver-close) and start
                        // a fresh one. Forces a new query sweep and rebuilds
                        // the snapshot list from scratch.
                        self.mdns_browse = Some(crate::mdns::BrowseHandle::start());
                    }
                });
            }
        }
        ui.add_space(6.0);
        ui.collapsing(rust_i18n::t!("screen_relay_connection").to_string(), |ui| {
            // Alle velden slaan meteen op bij wijziging (geen aparte "Apply"-knop meer).
            let mut relay_changed = false;
            relay_changed |= ui
                .checkbox(
                    &mut self.relay_enabled,
                    rust_i18n::t!("screen_connect_via_relay").to_string(),
                )
                .changed();
            ui.horizontal(|ui| {
                ui.label(rust_i18n::t!("screen_relay_url").to_string());
                relay_changed |= ui
                    .add(
                        egui::TextEdit::singleline(&mut self.relay_url)
                            .desired_width(260.0)
                            .hint_text("ws://relay.example.com:18080"),
                    )
                    .changed();
            });
            ui.horizontal(|ui| {
                ui.label(rust_i18n::t!("screen_station_name").to_string());
                relay_changed |= ui
                    .add(
                        egui::TextEdit::singleline(&mut self.relay_station)
                            .desired_width(180.0)
                            .hint_text("my-station"),
                    )
                    .changed();
                ui.label(rust_i18n::t!("screen_token").to_string());
                relay_changed |= ui
                    .add(
                        egui::TextEdit::singleline(&mut self.relay_token)
                            .desired_width(160.0)
                            .password(true),
                    )
                    .changed();
            });
            if relay_changed {
                super::config::save_relay_config(
                    self.relay_enabled,
                    &self.relay_url,
                    &self.relay_station,
                    &self.relay_token,
                    self.relay_udp_enabled,
                );
            }
            ui.horizontal(|ui| {
                ui.label(rust_i18n::t!("screen_device_name").to_string());
                if ui
                    .add(egui::TextEdit::singleline(&mut self.relay_device_name).desired_width(160.0))
                    .on_hover_text(rust_i18n::t!("screen_device_name_tooltip").to_string())
                    .changed()
                {
                    super::config::save_relay_device_name(&self.relay_device_name);
                }
            });
            ui.horizontal(|ui| {
                if ui
                    .checkbox(&mut self.relay_udp_enabled, rust_i18n::t!("screen_audio_over_udp").to_string())
                    .on_hover_text(rust_i18n::t!("screen_audio_over_udp_tooltip").to_string())
                    .changed()
                {
                    super::config::save_relay_config(
                        self.relay_enabled,
                        &self.relay_url,
                        &self.relay_station,
                        &self.relay_token,
                        self.relay_udp_enabled,
                    );
                }
            });
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(rust_i18n::t!("screen_status").to_string()).strong());
                if self.relay_external && !self.relay_enabled {
                    // Relay draait nog als transport deze sessie, maar de gebruiker
                    // heeft 'm net uitgezet -> net als bij aanzetten pas actief na
                    // een herstart. Toon dezelfde restart-melding (symmetrisch).
                    ui.colored_label(
                        Color32::from_rgb(200, 160, 40),
                        rust_i18n::t!("screen_relay_saved_stop").to_string(),
                    );
                } else if self.relay_external {
                    // Relay is deze sessie de actieve transport -> toon de live status.
                    if let Some(handle) = self.relay_status.as_ref() {
                        let status = handle.snapshot();
                        let color = match status.phase {
                            sdr_remote_relay::RelayPhase::Authenticated => Color32::from_rgb(50, 200, 50),
                            sdr_remote_relay::RelayPhase::Connecting
                            | sdr_remote_relay::RelayPhase::WaitingForPeer
                            | sdr_remote_relay::RelayPhase::WaitingForConfig => Color32::from_rgb(200, 160, 40),
                            sdr_remote_relay::RelayPhase::Error => Color32::from_rgb(220, 60, 60),
                            sdr_remote_relay::RelayPhase::Disabled
                            | sdr_remote_relay::RelayPhase::Disconnected => Color32::from_rgb(130, 130, 130),
                        };
                        // On Error, show the concrete reason inline (why the relay
                        // refused: device/connection/data limit, block, bad key) rather
                        // than just the word "Error"; keep the full text on hover too.
                        let msg = status.message.clone();
                        let shown = if matches!(status.phase, sdr_remote_relay::RelayPhase::Error)
                            && !msg.is_empty()
                        {
                            msg.clone()
                        } else {
                            status.label()
                        };
                        ui.colored_label(color, shown).on_hover_text(msg);
                    }
                } else if self.relay_enabled {
                    // Config staat op relay, maar deze sessie draait nog niet via de relay.
                    ui.colored_label(
                        Color32::from_rgb(200, 160, 40),
                        rust_i18n::t!("screen_relay_saved_connect").to_string(),
                    );
                } else {
                    ui.colored_label(Color32::from_rgb(130, 130, 130), rust_i18n::t!("screen_relay_off_direct").to_string());
                }
            });
            ui.label(
                egui::RichText::new(rust_i18n::t!("screen_relay_description").to_string())
                .size(11.0)
                .color(Color32::from_rgb(150, 150, 150)),
            );
        });
        // PATCH-1: read connect_status from logic-state and render via
        // i18n helper. Single source of truth - same NL/EN text as Android
        // bridge. The legacy `auth_rejected` / `totp_required` booleans are
        // still set for back-compat but new UI code uses connect_status.
        let connect_status = self.state_rx.borrow().connect_status.clone();
        use sdr_remote_logic::i18n::{connect_status_text, Lang};
        use sdr_remote_logic::state::ConnectStatus;

        // PATCH-1: language from client config (set via `language=nl|en` in
        // thetislink-client.conf). Defaults to English when unset.
        let lang = if self.ui_language == "nl" { Lang::Nl } else { Lang::En };

        // PATCH-1 smoke-test fix (2026-05-12 #2): show the TOTP input + Verify
        // button in BOTH AwaitingTotp AND Failed(WrongTotp) states. Without
        // that, a user who types a wrong TOTP code has no way to retry: the
        // TOTP input field disappears, and pressing the regular Connect
        // button regresses the engine to "Connecting..." without a path back
        // to AwaitingTotp.
        let in_wrong_totp = matches!(
            &connect_status,
            ConnectStatus::Failed(sdr_remote_logic::state::ConnectError::WrongTotp)
        );
        // PATCH-1 smoke-test fix (2026-05-13): operator feedback - connect-status text
        // was smaller than surrounding UI; should *stand out*. Headline now 18pt bold,
        // action 14pt (body default, not `.small()`).
        match &connect_status {
            ConnectStatus::AwaitingTotp => {
                let (headline, action) = connect_status_text(&connect_status, lang, sdr_remote_logic::i18n::Platform::Desktop);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&headline).size(18.0).strong());
                    let re = ui.add(egui::TextEdit::singleline(&mut self.totp_input)
                        .desired_width(80.0).hint_text(rust_i18n::t!("screen_six_digits").to_string()));
                    if (re.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
                        || ui.button(rust_i18n::t!("screen_verify").to_string()).clicked())
                        && self.totp_input.len() == 6
                    {
                        let _ = self.cmd_tx.send(sdr_remote_logic::commands::Command::SendTotpCode(self.totp_input.clone()));
                        self.totp_input.clear();
                    }
                });
                if let Some(a) = action {
                    ui.label(egui::RichText::new(a).size(14.0));
                }
            }
            ConnectStatus::Failed(_) if in_wrong_totp => {
                // Wrong TOTP: keep the TOTP input + Verify button visible so
                // the user can correct and retry without disconnecting.
                let (headline, action) = connect_status_text(&connect_status, lang, sdr_remote_logic::i18n::Platform::Desktop);
                ui.label(egui::RichText::new(&headline).size(18.0).strong().color(Color32::from_rgb(220, 40, 40)));
                if let Some(a) = action {
                    ui.label(egui::RichText::new(a).size(14.0));
                }
                ui.horizontal(|ui| {
                    ui.label(rust_i18n::t!("screen_retry").to_string());
                    let re = ui.add(egui::TextEdit::singleline(&mut self.totp_input)
                        .desired_width(80.0).hint_text(rust_i18n::t!("screen_six_digits").to_string()));
                    if (re.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
                        || ui.button(rust_i18n::t!("screen_verify").to_string()).clicked())
                        && self.totp_input.len() == 6
                    {
                        let _ = self.cmd_tx.send(sdr_remote_logic::commands::Command::SendTotpCode(self.totp_input.clone()));
                        self.totp_input.clear();
                    }
                });
            }
            ConnectStatus::Failed(_) => {
                let (headline, action) = connect_status_text(&connect_status, lang, sdr_remote_logic::i18n::Platform::Desktop);
                ui.label(egui::RichText::new(&headline).size(18.0).strong().color(Color32::from_rgb(220, 40, 40)));
                if let Some(a) = action {
                    ui.label(egui::RichText::new(a).size(14.0));
                }
            }
            ConnectStatus::Connecting => {
                let (headline, _) = connect_status_text(&connect_status, lang, sdr_remote_logic::i18n::Platform::Desktop);
                ui.label(egui::RichText::new(&headline).size(18.0).strong().color(Color32::from_rgb(180, 180, 60)));
            }
            ConnectStatus::Disconnected | ConnectStatus::Connected => {
                if !self.connected && self.password_input.is_empty() {
                    ui.colored_label(Color32::from_rgb(255, 165, 0), rust_i18n::t!("screen_password_required_to_connect").to_string());
                }
            }
        }

        ui.separator();

        // UI language: English base + choice of NL/DE/FR. Applies immediately
        // (rust_i18n::set_locale) and persists (language= in the client conf).
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("language").to_string());
            let langs = [("en", "English"), ("nl", "Nederlands"), ("de", "Deutsch"), ("fr", "Francais")];
            let cur_name = langs.iter().find(|(c, _)| *c == self.ui_language).map(|(_, n)| *n).unwrap_or("English");
            let mut picked: Option<&str> = None;
            egui::ComboBox::from_id_salt("ui_language_select")
                .selected_text(cur_name)
                .show_ui(ui, |ui| {
                    for (code, name) in langs {
                        if ui.selectable_label(self.ui_language == code, name).clicked() {
                            picked = Some(code);
                        }
                    }
                });
            if let Some(code) = picked {
                self.ui_language = code.to_string();
                rust_i18n::set_locale(code);
                self.save_full_config();
            }
        });

        // UI theme: pick a preset (Classic/Dark/Slate) or Custom. Applies immediately and
        // persists. Custom exposes colour pickers for the base slots; the per-element
        // colours join the palette in later migration steps.
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("screen_theme").to_string());
            let mut sel = self.theme_variant;
            egui::ComboBox::from_id_salt("theme_select")
                .selected_text(sel.label())
                .show_ui(ui, |ui| {
                    for v in theme::ThemeVariant::ALL {
                        ui.selectable_value(&mut sel, v, v.label());
                    }
                });
            if sel != self.theme_variant {
                self.theme_variant = sel;
                self.save_full_config();
            }
        });
        if self.theme_variant == theme::ThemeVariant::Custom {
            ui.horizontal(|ui| {
                ui.label(rust_i18n::t!("screen_custom_colours").to_string());
                let mut p = self.theme_custom;
                let mut changed = false;
                ui.label(rust_i18n::t!("screen_background").to_string());
                changed |= ui.color_edit_button_srgba(&mut p.background).changed();
                ui.label(rust_i18n::t!("screen_widgets").to_string());
                changed |= ui.color_edit_button_srgba(&mut p.widget).changed();
                ui.label(rust_i18n::t!("screen_text").to_string());
                changed |= ui.color_edit_button_srgba(&mut p.text).changed();
                ui.label(rust_i18n::t!("screen_slider_knob").to_string());
                changed |= ui.color_edit_button_srgba(&mut p.accent).changed();
                if ui.button(rust_i18n::t!("screen_reset").to_string()).clicked() {
                    p = theme::Palette::slate();
                    changed = true;
                }
                if changed {
                    self.theme_custom = p; // live preview
                    self.theme_custom_dirty = true;
                }
            });
            // Persist once the user releases the picker (avoids per-frame disk I/O while
            // dragging the hue/saturation sliders).
            if self.theme_custom_dirty && ui.input(|i| i.pointer.any_released()) {
                self.save_full_config();
                self.theme_custom_dirty = false;
            }
        }

        ui.separator();

        // Audio device selection - refresh device list only when combo opened or first time
        // Device enumeration (cpal/WASAPI) blocks the UI thread for 50-200ms on Windows,
        // causing audio hiccups. Only refresh on first render or when user opens the combo.
        let needs_device_refresh = self.device_refresh_at.is_none();
        if needs_device_refresh {
            self.input_devices = crate::audio::list_input_devices();
            self.output_devices = crate::audio::list_output_devices();
            self.device_refresh_at = Some(Instant::now());
        }
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("screen_input").to_string());
            let default_label = rust_i18n::t!("screen_default").to_string();
            let current_input = if self.selected_input.is_empty() {
                default_label.clone()
            } else {
                self.selected_input.clone()
            };
            let resp = egui::ComboBox::from_id_salt("input_dev")
                .selected_text(current_input)
                .width(250.0)
                .show_ui(ui, |ui| {
                    if ui.selectable_label(self.selected_input.is_empty(), default_label.as_str()).clicked() {
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
            ui.label(rust_i18n::t!("screen_output").to_string());
            let default_label = rust_i18n::t!("screen_default").to_string();
            let current_output = if self.selected_output.is_empty() {
                default_label.clone()
            } else {
                self.selected_output.clone()
            };
            let resp = egui::ComboBox::from_id_salt("output_dev")
                .selected_text(current_output)
                .width(250.0)
                .show_ui(ui, |ui| {
                    if ui.selectable_label(self.selected_output.is_empty(), default_label.as_str()).clicked() {
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

        // Mic -> TX Profile auto-switch mapping
        if !self.tx_profiles.is_empty() && !self.input_devices.is_empty() {
            ui.separator();
            ui.label(rust_i18n::t!("screen_mic_tx_profile_mapping").to_string());
            let mut changed = false;
            for dev_name in &self.input_devices {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(dev_name).size(11.0));
                    let current = self.mic_profile_map.get(dev_name).cloned().unwrap_or_default();
                    let none_label = rust_i18n::t!("screen_none_paren").to_string();
                    let display = if current.is_empty() { none_label.clone() } else { current.clone() };
                    egui::ComboBox::from_id_salt(format!("mic_prof_{}", dev_name))
                        .selected_text(display)
                        .width(150.0)
                        .show_ui(ui, |ui| {
                            if ui.selectable_label(current.is_empty(), none_label.as_str()).clicked() {
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
        // PTT switch-on spike protection for a built-in speaker + mic in one chassis
        // (tablets/laptops). Off by default so nobody pays extra latency.
        let mut sp_changed = ui
            .checkbox(
                &mut self.spike_protection,
                rust_i18n::t!("screen_builtin_speaker_mic").to_string(),
            )
            .on_hover_text(rust_i18n::t!("screen_builtin_speaker_mic_tooltip").to_string())
            .changed();
        if self.spike_protection {
            ui.indent("spike_delays", |ui| {
                ui.horizontal(|ui| {
                    ui.label(rust_i18n::t!("screen_mic_gate_delay_thetis").to_string());
                    sp_changed |= ui
                        .add(
                            egui::DragValue::new(&mut self.mic_gate_delay_thetis_ms)
                                .range(0..=800)
                                .suffix(" ms")
                                .speed(1.0),
                        )
                        .changed();
                });
                ui.horizontal(|ui| {
                    ui.label(rust_i18n::t!("screen_mic_gate_delay_yaesu").to_string());
                    sp_changed |= ui
                        .add(
                            egui::DragValue::new(&mut self.mic_gate_delay_yaesu_ms)
                                .range(0..=800)
                                .suffix(" ms")
                                .speed(1.0),
                        )
                        .changed();
                });
                ui.label(
                    egui::RichText::new(rust_i18n::t!("screen_gate_delay_hint").to_string())
                        .size(11.0)
                        .italics(),
                );
            });
        }
        if sp_changed {
            crate::ui::config::save_spike_protection(
                self.spike_protection,
                self.mic_gate_delay_thetis_ms,
                self.mic_gate_delay_yaesu_ms,
            );
        }

        ui.separator();

        // Audio levels: hide Thetis-only streams in Yaesu-only setups.
        ui.label(rust_i18n::t!("screen_audio_levels").to_string());
        ui.horizontal(|ui| {
            let (mic_label, mic_level) = if self.yaesu_tx_active || self.yaesu2_tx_active {
                ("Yaesu Mic:", self.yaesu_mic_level)
            } else if self.ptt && self.thetis_configured {
                ("Thetis Mic:", self.capture_level)
            } else {
                ("Mic:       ", self.capture_level)
            };
            ui.label(mic_label);
            level_bar(ui, mic_level, "mic");
        });
        if self.thetis_configured {
            // Alleen tonen als RX1-audio geabonneerd is (rx1_enabled). NIET op
            // playback_level: comfort-ruis houdt dat > 0, waardoor RX1 bleef staan.
            if self.rx1_enabled {
                if self.binaural && self.playback_level_bin_r > 0.0 {
                    ui.horizontal(|ui| {
                        ui.label("RX1 L:     ");
                        level_bar(ui, self.playback_level, "rx1");
                    });
                    ui.horizontal(|ui| {
                        ui.label("RX1 R:     ");
                        level_bar(ui, self.playback_level_bin_r, "rx1r");
                    });
                } else {
                    ui.horizontal(|ui| {
                        ui.label("RX1:       ");
                        level_bar(ui, self.playback_level, "rx1");
                    });
                }
            }
            if self.rx2_enabled {
                ui.horizontal(|ui| {
                    ui.label("RX2:       ");
                    level_bar(ui, self.playback_level_rx2, "rx2");
                });
            }
        }
        // Alleen tonen als de Yaesu-audio geabonneerd is (yaesu_enabled), niet als
        // de radio alleen maar hardware-verbonden is (yaesu_connected). Consistente
        // naam via yaesu_slot_label (zelfde als de detail-tab).
        if self.yaesu_enabled {
            ui.horizontal(|ui| {
                ui.label(self.yaesu_slot_label(0));
                level_bar(ui, self.playback_level_yaesu, "yaesu1");
            });
        }
        if self.yaesu2_enabled {
            ui.horizontal(|ui| {
                ui.label(self.yaesu_slot_label(1));
                level_bar(ui, self.playback_level_yaesu2, "yaesu2");
            });
        }
        if self.thetis_configured {
            if self.vrx1_enabled {
                ui.horizontal(|ui| {
                    ui.label("VRX1:      ");
                    level_bar(ui, self.playback_level_vrx1, "vrx1");
                });
            }
            if self.vrx2_enabled {
                ui.horizontal(|ui| {
                    ui.label("VRX2:      ");
                    level_bar(ui, self.playback_level_vrx2, "vrx2");
                });
            }
        }

        ui.separator();

        // Audio recording
        let rec_yaesu_label = self.yaesu_slot_label(0);
        let rec_yaesu2_label = self.yaesu_slot_label(1);
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("screen_record").to_string());
            if self.recording {
                if ui.button(RichText::new(rust_i18n::t!("screen_stop_icon").to_string()).color(Color32::WHITE))
                    .highlight()
                    .clicked()
                {
                    let _ = self.cmd_tx.send(Command::StopRecording);
                    self.recording = false;
                }
            } else {
                if self.thetis_configured && self.rx1_enabled {
                    ui.checkbox(&mut self.rec_rx1, "RX1");
                } else {
                    self.rec_rx1 = false;
                }
                if self.thetis_configured && self.rx2_enabled {
                    ui.checkbox(&mut self.rec_rx2, "RX2");
                } else {
                    self.rec_rx2 = false;
                }
                if self.yaesu_enabled {
                    ui.checkbox(&mut self.rec_yaesu, rec_yaesu_label.as_str());
                } else {
                    self.rec_yaesu = false;
                }
                if self.yaesu2_enabled {
                    ui.checkbox(&mut self.rec_yaesu2, rec_yaesu2_label.as_str());
                } else {
                    self.rec_yaesu2 = false;
                }
                if self.thetis_configured && self.vrx1_enabled {
                    ui.checkbox(&mut self.rec_vrx1, "VRX1");
                } else {
                    self.rec_vrx1 = false;
                }
                if self.thetis_configured && self.vrx2_enabled {
                    ui.checkbox(&mut self.rec_vrx2, "VRX2");
                } else {
                    self.rec_vrx2 = false;
                }
                let any = self.rec_rx1 || self.rec_rx2 || self.rec_yaesu
                    || self.rec_yaesu2 || self.rec_vrx1 || self.rec_vrx2;
                if ui.add_enabled(any, egui::Button::new("Rec")).clicked() {
                    let path = std::env::current_exe()
                        .ok()
                        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                        .unwrap_or_default();
                    let _ = self.cmd_tx.send(Command::StartRecording {
                        rx1: self.rec_rx1,
                        rx2: self.rec_rx2,
                        yaesu: self.rec_yaesu,
                        yaesu2: self.rec_yaesu2,
                        vrx1: self.rec_vrx1,
                        vrx2: self.rec_vrx2,
                        path: path.to_string_lossy().to_string(),
                    });
                    self.recording = true;
                }
                // Play button for last recording
                if let Some(ref wav_path) = self.last_recorded_path {
                    if !self.playing {
                        if ui.button(rust_i18n::t!("screen_play_icon").to_string()).clicked() {
                            let _ = self.cmd_tx.send(Command::PlayRecording { path: wav_path.clone() });
                            self.playing = true;
                        }
                    } else {
                        if ui.button(rust_i18n::t!("screen_stop_icon").to_string()).clicked() {
                            let _ = self.cmd_tx.send(Command::StopPlayback);
                            self.playing = false;
                        }
                    }
                    // Play-volume: schaalt de WAV-playback (speaker + TX-inject).
                    ui.label("Play vol:");
                    let resp = ui
                        .add(
                            egui::Slider::new(&mut self.play_volume, 0.0..=2.0)
                                .fixed_decimals(2),
                        )
                        .on_hover_text(rust_i18n::t!("screen_play_volume_tooltip").to_string());
                    let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.play_volume, 0.0..=2.0, 0.05);
                    if resp.changed() || scrolled {
                        let _ = self.cmd_tx.send(Command::SetPlayVolume(self.play_volume));
                    }
                }
            }
        });

        ui.separator();

        // Stats
        ui.label(rust_i18n::t!("screen_statistics").to_string());
        egui::Grid::new("stats_grid")
            .num_columns(2)
            .spacing([20.0, 4.0])
            .show(ui, |ui| {
                ui.label("RTT:");
                ui.label(format!("{} ms", self.rtt_ms));
                ui.end_row();

                // Down (RX) is clickable for per-PacketType breakdown of the last 5 s.
                if super::helpers::chevron_label(ui, self.bw_breakdown_expanded, rust_i18n::t!("screen_down_rx").to_string()).clicked() {
                    self.bw_breakdown_expanded = !self.bw_breakdown_expanded;
                    self.save_full_config();
                }
                ui.label(format!("{} Kbit/s", self.down_kbps));
                ui.end_row();

                ui.label(rust_i18n::t!("screen_up_tx").to_string());
                ui.label(format!("{} Kbit/s", self.up_kbps));
                ui.end_row();
            });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("screen_audio_streams").to_string());
            // Fase 3c: transport indicator (relay only). Normal = low-latency UDP; when the
            // network blocks/degrades UDP the audio auto-falls back to the reliable (slower)
            // wss/TCP path. Honest wording: a brief gap can occur on a sudden total UDP loss.
            if let Some(handle) = self.relay_status.as_ref() {
                let st = handle.snapshot();
                if st.connected {
                    if st.transport_fallback {
                        ui.colored_label(
                            theme::TL_AMBER_TEXT,
                            rust_i18n::t!("screen_transport_tcp_fallback").to_string(),
                        );
                    } else {
                        ui.weak(rust_i18n::t!("screen_transport_udp").to_string());
                    }
                }
            }
        });
        egui::Grid::new("audio_stream_stats_grid")
            .num_columns(5)
            .spacing([14.0, 3.0])
            .show(ui, |ui| {
                ui.label(egui::RichText::new(rust_i18n::t!("screen_col_stream").to_string()).strong().size(11.0));
                ui.label(egui::RichText::new(rust_i18n::t!("screen_col_jitter").to_string()).strong().size(11.0));
                ui.label(egui::RichText::new(rust_i18n::t!("screen_col_buffer").to_string()).strong().size(11.0));
                ui.label(egui::RichText::new(rust_i18n::t!("screen_col_packets").to_string()).strong().size(11.0));
                ui.label(egui::RichText::new(rust_i18n::t!("screen_col_loss").to_string()).strong().size(11.0));
                ui.end_row();

                // Alleen ACTIEVE stromen tonen (huidige abonnement), niet op
                // `connected` of cumulatieve `packets>0` (die nooit terugloopt,
                // waardoor een uitgezet kanaal bleef staan).
                let mut shown = false;
                if self.rx1_enabled || self.rx2_enabled {
                    shown = true;
                    ui.label("RX1+RX2");
                    ui.label(format!("{:.1} ms", self.jitter_ms));
                    ui.label(format!("{} frames", self.buffer_depth));
                    ui.label(format!("{}", self.rx_packets));
                    ui.label(format!("{}%", self.loss_percent));
                    ui.end_row();
                }
                if self.yaesu_enabled {
                    shown = true;
                    ui.label(self.yaesu_slot_label(0));
                    ui.label(format!("{:.1} ms", self.yaesu_jitter_ms));
                    ui.label(format!("{} frames", self.yaesu_buffer_depth));
                    ui.label(format!("{}", self.yaesu_audio_packets));
                    ui.label("-");
                    ui.end_row();
                }
                if self.yaesu2_enabled {
                    shown = true;
                    ui.label(self.yaesu_slot_label(1));
                    ui.label(format!("{:.1} ms", self.yaesu2_jitter_ms));
                    ui.label(format!("{} frames", self.yaesu2_buffer_depth));
                    ui.label(format!("{}", self.yaesu2_audio_packets));
                    ui.label("-");
                    ui.end_row();
                }
                if self.vrx1_enabled {
                    shown = true;
                    ui.label("VRX1");
                    ui.label(format!("{:.1} ms", self.vrx1_jitter_ms));
                    ui.label(format!("{} frames", self.vrx1_buffer_depth));
                    ui.label(format!("{}", self.vrx1_audio_packets));
                    ui.label("-");
                    ui.end_row();
                }
                if self.vrx2_enabled {
                    shown = true;
                    ui.label("VRX2");
                    ui.label(format!("{:.1} ms", self.vrx2_jitter_ms));
                    ui.label(format!("{} frames", self.vrx2_buffer_depth));
                    ui.label(format!("{}", self.vrx2_audio_packets));
                    ui.label("-");
                    ui.end_row();
                }
                if !shown {
                    ui.label(egui::RichText::new(rust_i18n::t!("screen_no_audio_stream_yet").to_string()).color(egui::Color32::GRAY));
                    ui.label("");
                    ui.label("");
                    ui.label("");
                    ui.label("");
                    ui.end_row();
                }
            });

        // Per-stream breakdown - alleen tonen wanneer de gebruiker
        // op "Down" heeft geklikt. Lijst is leeg in de eerste ~5 s
        // na connect (engine vult 'm bij elke 5 s window-rollover).
        if self.bw_breakdown_expanded {
            ui.add_space(2.0);
            egui::Grid::new("bw_breakdown_grid")
                .num_columns(2)
                .spacing([20.0, 2.0])
                .show(ui, |ui| {
                    // Alleen ACTIEVE stromen tonen (kbps > 0) — een uitgezet kanaal
                    // verdwijnt zo uit het dataverbruik.
                    let active: Vec<_> = self.bw_breakdown.iter().filter(|(_, kbps)| *kbps > 0).collect();
                    if self.bw_breakdown.is_empty() {
                        ui.label(egui::RichText::new(rust_i18n::t!("screen_collecting_5s").to_string()).size(11.0).color(egui::Color32::GRAY));
                        ui.label("");
                        ui.end_row();
                    } else if active.is_empty() {
                        ui.label(egui::RichText::new(rust_i18n::t!("no_active_streams").to_string()).size(11.0).color(egui::Color32::GRAY));
                        ui.label("");
                        ui.end_row();
                    } else {
                        for (ptype, kbps) in active {
                            ui.label(egui::RichText::new(format!("  {}", self.bitstream_label(*ptype))).size(11.0));
                            ui.label(egui::RichText::new(format!("{} Kbit/s", kbps)).size(11.0));
                            ui.end_row();
                        }
                    }
                });
        }

        if self.thetis_configured {
            // Data-saving toggle: disables the DX-cluster spot stream on metered links.
            ui.add_space(4.0);
            let mut dx_spots = self.dx_spots_enabled;
            if ui.checkbox(&mut dx_spots, rust_i18n::t!("screen_receive_dx_spots").to_string()).changed() {
                self.dx_spots_enabled = dx_spots;
                let _ = self.cmd_tx.send(sdr_remote_logic::commands::Command::SetDxSpotsEnabled(dx_spots));
            }

            // Wideband Thetis audio opt-in: sends RX1/RX2/BinR in 16 kHz Opus
            // instead of the default 8 kHz. Default OFF (doubles bandwidth per
            // channel). Useful for FM/AM/broadcast listening via ANAN.
            let mut wb = self.thetis_wideband_audio;
            if ui.checkbox(&mut wb, rust_i18n::t!("screen_wideband_thetis_audio").to_string()).changed() {
                self.thetis_wideband_audio = wb;
                let _ = self.cmd_tx.send(sdr_remote_logic::commands::Command::SetThetisWidebandAudio(wb));
                self.save_full_config();
            }

            // VRX audio-rate: NB / WB / Auto, independent per VRX.
            ui.horizontal(|ui| {
                let labels = ["NB (8k)", "WB (16k)", "Auto"];
                ui.label(rust_i18n::t!("screen_vrx1_rate").to_string());
                let mut sel1 = (self.vrx_rate_mode as usize).min(2);
                egui::ComboBox::from_id_source("vrx1_audio_rate")
                    .selected_text(labels[sel1])
                    .show_ui(ui, |ui| {
                        for (i, lbl) in labels.iter().enumerate() {
                            ui.selectable_value(&mut sel1, i, *lbl);
                        }
                    });
                if sel1 as u8 != self.vrx_rate_mode {
                    self.vrx_rate_mode = sel1 as u8;
                    let _ = self.cmd_tx.send(sdr_remote_logic::commands::Command::SetVrxRateMode(self.vrx_rate_mode));
                }
                ui.label(rust_i18n::t!("screen_vrx2_rate").to_string());
                let mut sel2 = (self.vrx_rate_mode2 as usize).min(2);
                egui::ComboBox::from_id_source("vrx2_audio_rate")
                    .selected_text(labels[sel2])
                    .show_ui(ui, |ui| {
                        for (i, lbl) in labels.iter().enumerate() {
                            ui.selectable_value(&mut sel2, i, *lbl);
                        }
                    });
                if sel2 as u8 != self.vrx_rate_mode2 {
                    self.vrx_rate_mode2 = sel2 as u8;
                    let _ = self.cmd_tx.send(sdr_remote_logic::commands::Command::SetVrxRateMode2(self.vrx_rate_mode2));
                }
            }).response.on_hover_text(rust_i18n::t!("screen_vrx_rate_tooltip").to_string());
        }

        if self.thetis_configured {
            // TCI Status
            ui.separator();
            ui.label(rust_i18n::t!("screen_tci_status").to_string());
            egui::Grid::new("tci_grid")
            .num_columns(2)
            .spacing([20.0, 4.0])
            .show(ui, |ui| {
                ui.label(rust_i18n::t!("screen_tx_profile").to_string());
                let profile_name = self.tx_profiles.iter()
                    .find(|(idx, _)| *idx == self.tx_profile)
                    .map(|(_, name)| name.as_str())
                    .unwrap_or("?");
                ui.label(profile_name);
                ui.end_row();

                ui.label(rust_i18n::t!("screen_tx_profiles_plural").to_string());
                let names: Vec<&str> = self.tx_profiles.iter().map(|(_, n)| n.as_str()).collect();
                ui.label(if names.is_empty() { rust_i18n::t!("screen_none_paren").to_string() } else { names.join(", ") });
                ui.end_row();

                ui.label("MON:");
                ui.label(if self.mon_on { "ON" } else { "OFF" });
                ui.end_row();

                ui.label("VFO Sync:");
                ui.label(if self.vfo_sync { "ON" } else { "OFF" });
                ui.end_row();
            });
        }

        // Remote reboot / shutdown
        ui.separator();
        ui.horizontal(|ui| {
            if self.connected {
                if self.reboot_confirm {
                    ui.label(rust_i18n::t!("screen_remote_server_pc").to_string());
                    if ui.button(rust_i18n::t!("screen_reboot").to_string()).clicked() {
                        let _ = self.cmd_tx.send(Command::ServerReboot);
                        self.reboot_confirm = false;
                    }
                    if ui.button(rust_i18n::t!("screen_shutdown_btn").to_string()).clicked() {
                        let _ = self.cmd_tx.send(Command::ServerShutdown);
                        self.reboot_confirm = false;
                    }
                    if ui.button(rust_i18n::t!("screen_cancel").to_string()).clicked() {
                        self.reboot_confirm = false;
                    }
                } else if ui.button(rust_i18n::t!("screen_remote_reboot_shutdown").to_string()).clicked() {
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
                                    | MidiAction::Vrx1Tune | MidiAction::Vrx2Tune
                                    | MidiAction::Radio1Tune | MidiAction::Radio2Tune
                                    | MidiAction::NrLevel => crate::midi::ControlType::Encoder,
                                    MidiAction::MasterVolume | MidiAction::VfoAVolume
                                    | MidiAction::VfoBVolume | MidiAction::TxGain
                                    | MidiAction::Drive | MidiAction::AgcGain
                                    | MidiAction::SqlLevel | MidiAction::CwSpeed
                                    | MidiAction::TuneDrive | MidiAction::MonVolume
                                    | MidiAction::RxBalance | MidiAction::RitOffset
                                    | MidiAction::XitOffset | MidiAction::YaesuVolume
                                    | MidiAction::Radio2Volume | MidiAction::Vrx1Volume
                                    | MidiAction::Vrx2Volume | MidiAction::YaesuRfGain
                                    | MidiAction::YaesuMicGain
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
                        MidiAction::Rx2BandUp if pressed => {
                            self.midi_band_step_for(Vfo::B, 1);
                        }
                        MidiAction::Rx2BandDown if pressed => {
                            self.midi_band_step_for(Vfo::B, -1);
                        }
                        MidiAction::Radio1BandUp if pressed => {
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuButton, 5));
                        }
                        MidiAction::Radio1BandDown if pressed => {
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuButton, 6));
                        }
                        MidiAction::Radio2BandUp if pressed => {
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button, 5));
                        }
                        MidiAction::Radio2BandDown if pressed => {
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button, 6));
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
                            // Cycle NR level: 0 -> 1 -> 2 -> 3 -> 4 -> 0
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
                            // Toggle tune - send 1 to activate, server handles state
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
                        MidiAction::Radio2Ptt => {
                            if self.midi_ptt_toggle_mode {
                                if pressed {
                                    let new_tx = !self.yaesu2_tx_active;
                                    let _ = self.cmd_tx.send(Command::SetYaesu2Ptt(new_tx));
                                    self.midi.send_led(MidiAction::Radio2Ptt, new_tx);
                                }
                            } else {
                                let _ = self.cmd_tx.send(Command::SetYaesu2Ptt(pressed));
                                self.midi.send_led(MidiAction::Radio2Ptt, pressed);
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
                            // Slider: 0-127 -> ±1270 Hz in 20 Hz steps (center=0)
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
                            self.yaesu_volume = Self::midi_log_volume(frac);
                            let _ = self.cmd_tx.send(Command::SetYaesuVolume(self.yaesu_volume));
                        }
                        MidiAction::Radio2Volume => {
                            self.yaesu2_volume = Self::midi_log_volume(frac);
                            let _ = self.cmd_tx.send(Command::SetYaesu2Volume(self.yaesu2_volume));
                        }
                        MidiAction::Vrx1Volume => {
                            self.vrx1_volume = Self::midi_log_volume(frac);
                            let _ = self.cmd_tx.send(Command::SetVrxVolume(self.vrx1_volume));
                        }
                        MidiAction::Vrx2Volume => {
                            self.vrx2_volume = Self::midi_log_volume(frac);
                            let _ = self.cmd_tx.send(Command::SetVrx2Volume(self.vrx2_volume));
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
                        MidiAction::Vrx1Tune => {
                            self.midi_vrx_tune(0, delta, step);
                        }
                        MidiAction::Vrx2Tune => {
                            self.midi_vrx_tune(1, delta, step);
                        }
                        MidiAction::Radio1Tune => {
                            let new_freq = (self.yaesu_freq_a as i64 + delta as i64 * step as i64).max(0) as u64;
                            let _ = self.cmd_tx.send(Command::SetYaesuFreq(new_freq));
                            self.set_pending_yaesu_freq(0, new_freq);
                        }
                        MidiAction::Radio2Tune => {
                            let new_freq = (self.yaesu2_freq_a as i64 + delta as i64 * step as i64).max(0) as u64;
                            let _ = self.cmd_tx.send(Command::SetYaesu2Freq(new_freq));
                            self.set_pending_yaesu_freq(1, new_freq);
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

    fn midi_log_volume(frac: f32) -> f32 {
        (0.001_f32 * (1000.0_f32).powf(frac)).max(0.001)
    }

    fn midi_vrx_tune(&mut self, vrx_id: u8, delta: i8, step_hz: u64) {
        let (center, span, current, source) = if vrx_id == 0 {
            (
                self.full_spectrum_center_hz,
                self.full_spectrum_span_hz,
                self.vrx1_freq_hz,
                self.frequency_hz,
            )
        } else {
            (
                self.rx2_full_spectrum_center_hz,
                self.rx2_full_spectrum_span_hz,
                self.vrx2_freq_hz,
                self.rx2_frequency_hz,
            )
        };
        let center = if center > 0 { center as u64 } else { source };
        let span = if span > 0 { span as u64 } else { 384_000 };
        let min_hz = center.saturating_sub(span / 2);
        let max_hz = center + span / 2;
        let base = if current > 0 { current } else { source }.clamp(min_hz, max_hz);
        let next = (base as i64 + delta as i64 * step_hz as i64)
            .clamp(min_hz as i64, max_hz as i64) as u64;
        if vrx_id == 0 {
            self.vrx1_freq_hz = next;
            let _ = self.cmd_tx.send(Command::SetVrxFrequency(next));
        } else {
            self.vrx2_freq_hz = next;
            let _ = self.cmd_tx.send(Command::SetVrx2Frequency(next));
        }
    }

    pub(super) fn midi_band_step(&mut self, direction: i32) {
        self.midi_band_step_for(Vfo::A, direction);
    }

    pub(super) fn midi_band_step_for(&mut self, vfo: Vfo, direction: i32) {
        const BANDS: &[(&str, u64)] = &[
            ("160m", 1_900_000), ("80m", 3_700_000), ("60m", 5_351_000),
            ("40m", 7_100_000), ("30m", 10_120_000), ("20m", 14_200_000),
            ("17m", 18_100_000), ("15m", 21_200_000), ("12m", 24_930_000),
            ("10m", 28_500_000), ("6m", 50_200_000),
        ];
        let freq_hz = match vfo {
            Vfo::A => self.frequency_hz,
            Vfo::B => self.rx2_frequency_hz,
        };
        let current = band_label(freq_hz);
        let idx = BANDS.iter().position(|&(name, _)| name == current);
        let new_idx = match idx {
            Some(i) => (i as i32 + direction).rem_euclid(BANDS.len() as i32) as usize,
            None => 0,
        };
        self.save_current_band(vfo);
        self.restore_band(vfo, BANDS[new_idx].0, BANDS[new_idx].1);
        self.save_full_config();
    }

    fn render_midi_action_section(ui: &mut egui::Ui, title: &str) {
        ui.add_space(3.0);
        ui.label(RichText::new(title).strong().color(Color32::from_rgb(170, 205, 240)));
    }

    fn render_midi_action_pair(
        ui: &mut egui::Ui,
        selected: &mut crate::midi::MidiAction,
        left: crate::midi::MidiAction,
        right: crate::midi::MidiAction,
    ) {
        ui.horizontal(|ui| {
            ui.selectable_value(selected, left, left.label());
            ui.selectable_value(selected, right, right.label());
        });
    }

    fn render_midi_action_single(
        ui: &mut egui::Ui,
        selected: &mut crate::midi::MidiAction,
        action: crate::midi::MidiAction,
    ) {
        ui.selectable_value(selected, action, action.label());
    }

    fn render_midi_action_picker(ui: &mut egui::Ui, selected: &mut crate::midi::MidiAction) {
        use crate::midi::MidiAction;

        Self::render_midi_action_section(ui, "RX");
        Self::render_midi_action_pair(ui, selected, MidiAction::VfoATune, MidiAction::VfoBTune);
        Self::render_midi_action_pair(ui, selected, MidiAction::VfoAVolume, MidiAction::VfoBVolume);
        Self::render_midi_action_pair(ui, selected, MidiAction::BandUp, MidiAction::Rx2BandUp);
        Self::render_midi_action_pair(ui, selected, MidiAction::BandDown, MidiAction::Rx2BandDown);
        Self::render_midi_action_single(ui, selected, MidiAction::Ptt);

        ui.separator();
        Self::render_midi_action_section(ui, "VRX");
        Self::render_midi_action_pair(ui, selected, MidiAction::Vrx1Tune, MidiAction::Vrx2Tune);
        Self::render_midi_action_pair(ui, selected, MidiAction::Vrx1Volume, MidiAction::Vrx2Volume);

        ui.separator();
        Self::render_midi_action_section(ui, &rust_i18n::t!("screen_radio").to_string());
        Self::render_midi_action_pair(ui, selected, MidiAction::YaesuPtt, MidiAction::Radio2Ptt);
        Self::render_midi_action_pair(ui, selected, MidiAction::Radio1Tune, MidiAction::Radio2Tune);
        Self::render_midi_action_pair(ui, selected, MidiAction::YaesuVolume, MidiAction::Radio2Volume);
        Self::render_midi_action_pair(ui, selected, MidiAction::Radio1BandUp, MidiAction::Radio2BandUp);
        Self::render_midi_action_pair(ui, selected, MidiAction::Radio1BandDown, MidiAction::Radio2BandDown);

        ui.separator();
        Self::render_midi_action_section(ui, &rust_i18n::t!("screen_other_advanced").to_string());
        const PRIMARY_ACTIONS: &[MidiAction] = &[
            MidiAction::Ptt,
            MidiAction::VfoATune,
            MidiAction::VfoBTune,
            MidiAction::VfoAVolume,
            MidiAction::VfoBVolume,
            MidiAction::BandUp,
            MidiAction::BandDown,
            MidiAction::Rx2BandUp,
            MidiAction::Rx2BandDown,
            MidiAction::Vrx1Tune,
            MidiAction::Vrx2Tune,
            MidiAction::Vrx1Volume,
            MidiAction::Vrx2Volume,
            MidiAction::YaesuPtt,
            MidiAction::Radio2Ptt,
            MidiAction::Radio1Tune,
            MidiAction::Radio2Tune,
            MidiAction::YaesuVolume,
            MidiAction::Radio2Volume,
            MidiAction::Radio1BandUp,
            MidiAction::Radio1BandDown,
            MidiAction::Radio2BandUp,
            MidiAction::Radio2BandDown,
        ];
        for action in crate::midi::MidiAction::ALL {
            if PRIMARY_ACTIONS.contains(action) {
                continue;
            }
            Self::render_midi_action_single(ui, selected, *action);
        }
    }
    pub(super) fn render_midi_screen(&mut self, ui: &mut egui::Ui) {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));

        // Device selection
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("screen_midi_device").to_string());
            if ui.button(rust_i18n::t!("screen_refresh").to_string()).clicked() {
                self.midi_ports = crate::midi::MidiManager::list_ports();
            }
        });

        if self.midi_ports.is_empty() && !self.midi.is_connected() {
            ui.label(rust_i18n::t!("screen_no_midi_devices").to_string());
        } else {
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("midi_port")
                    .selected_text(if self.midi_selected_port.is_empty() {
                        rust_i18n::t!("screen_select_device").to_string()
                    } else {
                        self.midi_selected_port.clone()
                    })
                    .show_ui(ui, |ui| {
                        for port in &self.midi_ports {
                            ui.selectable_value(&mut self.midi_selected_port, port.clone(), port);
                        }
                    });

                if self.midi.is_connected() {
                    if ui.button(rust_i18n::t!("screen_disconnect").to_string()).clicked() {
                        self.midi.disconnect();
                    }
                    ui.colored_label(Color32::GREEN, rust_i18n::t!("screen_connected").to_string());
                } else {
                    let can_connect = !self.midi_selected_port.is_empty();
                    if ui.add_enabled(can_connect, egui::Button::new(rust_i18n::t!("screen_connect").to_string())).clicked() {
                        if self.midi.connect(&self.midi_selected_port) {
                            self.save_full_config();
                        }
                    }
                    ui.colored_label(Color32::RED, rust_i18n::t!("screen_disconnected").to_string());
                }
            });
        }

        ui.separator();

        // MIDI PTT mode (independent from main PTT mode)
        ui.horizontal(|ui| {
            ui.label("MIDI PTT:");
            if ui.selectable_label(!self.midi_ptt_toggle_mode, rust_i18n::t!("screen_push_to_talk").to_string()).clicked() {
                self.midi_ptt_toggle_mode = false;
                self.save_ptt_config();
            }
            if ui.selectable_label(self.midi_ptt_toggle_mode, rust_i18n::t!("screen_toggle").to_string()).clicked() {
                self.midi_ptt_toggle_mode = true;
                self.save_ptt_config();
            }
        });

        // Encoder step setting
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("screen_encoder_step").to_string());
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
                ui.label(rust_i18n::t!("screen_last_midi").to_string());
                ui.monospace(&self.midi_last_event);
            });
            ui.separator();
        }

        // Mappings table
        ui.label(RichText::new(rust_i18n::t!("screen_mappings").to_string()).strong());

        let mappings = self.midi.get_mappings();
        let mut remove_idx: Option<usize> = None;

        egui::Grid::new("midi_mappings")
            .striped(true)
            .min_col_width(60.0)
            .show(ui, |ui| {
                ui.label(RichText::new(rust_i18n::t!("screen_col_source").to_string()).strong());
                ui.label(RichText::new(rust_i18n::t!("screen_col_type").to_string()).strong());
                ui.label(RichText::new(rust_i18n::t!("screen_col_action").to_string()).strong());
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
                ui.label(rust_i18n::t!("screen_learning").to_string());
                ui.label(RichText::new(self.midi_learn_action.label()).strong());
                ui.label(rust_i18n::t!("screen_move_control_hint").to_string());
                if ui.button(rust_i18n::t!("screen_cancel").to_string()).clicked() {
                    self.midi_learn_for = None;
                    self.midi.set_learn_mode(false);
                }
            });
        } else {
            ui.horizontal(|ui| {
                ui.label(rust_i18n::t!("screen_add").to_string());
                egui::ComboBox::from_id_salt("midi_learn_action")
                    .selected_text(self.midi_learn_action.label())
                    .width(280.0)
                    .show_ui(ui, |ui| {
                        Self::render_midi_action_picker(ui, &mut self.midi_learn_action);
                    });
                if ui.button(rust_i18n::t!("screen_learn").to_string()).clicked() && self.midi.is_connected() {
                    self.midi_learn_for = Some(mappings.len());
                    self.midi.set_learn_mode(true);
                }
            });
        }
    }
}







