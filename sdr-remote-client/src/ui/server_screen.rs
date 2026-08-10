// SPDX-License-Identifier: GPL-2.0-or-later
//! `SdrRemoteApp::render_server_screen`: the "Server" tab - server-side status,
//! device/backend state, audio levels and the controls that live on that screen.
//! Extracted verbatim from `ui/screens.rs` - pure relocation, no behaviour change.
//! `pub(super)` keeps it callable from the parent module tree.

use super::*;

impl SdrRemoteApp {
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
                // Relay is the active transport: the direct server IP is not the
                // route (the relay determines the destination via station/token). Show the
                // relay destination + live status instead of a misleading IP field.
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
            // All fields save immediately on change (no separate "Apply" button anymore).
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
                    // Relay is still running as transport this session, but the user
                    // has just turned it off -> like turning it on, only active after
                    // a restart. Show the same restart message (symmetric).
                    ui.colored_label(
                        Color32::from_rgb(200, 160, 40),
                        rust_i18n::t!("screen_relay_saved_stop").to_string(),
                    );
                } else if self.relay_external {
                    // Relay is the active transport this session -> show the live status.
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
                    // Config is set to relay, but this session is not yet running via the relay.
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

        // UI scale, independent of the Windows display scaling. A high-DPI screen at
        // 200% gives the app FEWER points to lay out in than an ordinary 1080p monitor
        // at 100%, so less fits despite the higher resolution - this scales the app
        // down without touching the system-wide setting. egui's Ctrl+/Ctrl- does the
        // same thing and is picked up and stored (see update.rs).
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("screen_ui_scale").to_string());
            // One step list, shared with the server GUI, so the two cannot end up
            // offering different scales.
            let picked = sdr_remote_layout::ui_scale_picker(ui, "ui_zoom_select", self.ui_zoom);
            if let Some(v) = picked {
                self.ui_zoom = v;
                self.ui_zoom_pending = true;
                self.save_full_config();
            }
            ui.label(
                egui::RichText::new(rust_i18n::t!("screen_ui_scale_hint").to_string())
                    .size(11.0)
                    .color(egui::Color32::GRAY),
            );
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
        // RX bars are measured as received (before the volume sliders), the mic
        // bar as transmitted (after gain/compressor) - so a bar answers "is this
        // link carrying audio", independent of how loud it is played here.
        ui.label(rust_i18n::t!("screen_audio_levels").to_string())
            .on_hover_text(rust_i18n::t!("screen_audio_levels_hover").to_string());
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
            // Only show when RX1 audio is subscribed (rx1_enabled). NOT on
            // playback_level: comfort noise keeps it > 0, which left RX1 showing.
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
        // Only show when the Yaesu audio is subscribed (yaesu_enabled), not when
        // the radio is merely hardware-connected (yaesu_connected). Consistent
        // name via yaesu_slot_label (same as the detail tab).
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
                    // Play volume: scales the WAV playback (speaker + TX-inject).
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

                // Only show ACTIVE streams (current subscription), not on
                // `connected` or cumulative `packets>0` (which never decreases,
                // which left a disabled channel showing).
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

        // Per-stream breakdown - only show when the user
        // has clicked "Down". List is empty for the first ~5 s
        // after connect (engine fills it on every 5 s window rollover).
        if self.bw_breakdown_expanded {
            ui.add_space(2.0);
            egui::Grid::new("bw_breakdown_grid")
                .num_columns(2)
                .spacing([20.0, 2.0])
                .show(ui, |ui| {
                    // Only show ACTIVE streams (kbps > 0) — a disabled channel
                    // thus disappears from the data usage.
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

            // Second spectrum row per receiver chain, shared by RX1+VRX1 and
            // by RX2+VRX2 - one row per client, not one per window. Off, every
            // waterfall is built from its own view alone and the RX plot no
            // longer widens out during fast tuning.
            let mut full_spec = self.full_spectrum_enabled;
            if ui.checkbox(&mut full_spec, rust_i18n::t!("screen_full_spectrum_row").to_string())
                .on_hover_text(rust_i18n::t!("screen_full_spectrum_row_hover").to_string())
                .changed()
            {
                self.full_spectrum_enabled = full_spec;
                let _ = self.cmd_tx.send(sdr_remote_logic::commands::Command::SetFullSpectrumEnabled(full_spec));
                self.save_full_config();
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
}
