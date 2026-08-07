// SPDX-License-Identifier: GPL-2.0-or-later

use super::*;

/// Map a `PacketType` byte to a readable label for the
/// Server-tab bandwidth-breakdown. Unknown types fall back to
/// hex notation so new packet-types are also visible without
/// touching code here.
pub(super) fn packet_type_label(t: u8) -> String {
    match t {
        0x01 => "Audio (legacy mono)".into(),
        0x03 => "HeartbeatAck".into(),
        0x04 => "Disconnect".into(),
        0x07 => "Frequency".into(),
        0x08 => "Mode".into(),
        0x09 => "S-meter RX1 Avg".into(),
        0x0A => "Spectrum RX1".into(),
        0x0B => "Full spectrum (V)RX1".into(),
        0x0C => "Equipment status".into(),
        0x0E => "Audio RX2".into(),
        0x0F => "Frequency RX2".into(),
        0x10 => "Mode RX2".into(),
        0x11 => "S-meter RX2 Avg".into(),
        0x12 => "Spectrum RX2".into(),
        0x13 => "Full spectrum (V)RX2".into(),
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
}







