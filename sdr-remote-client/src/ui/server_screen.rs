// SPDX-License-Identifier: GPL-2.0-or-later
//! `SdrRemoteApp::render_server_screen`: the "Server" tab - server-side status,
//! device/backend state, audio levels and the controls that live on that screen.
//! Extracted verbatim from `ui/screens.rs` - pure relocation, no behaviour change.
//! `pub(super)` keeps it callable from the parent module tree.

use super::*;

/// One tick in the recording row.
///
/// Three states, and the middle one is the point. A channel this station does
/// not have is not shown at all. A channel that exists but whose audio is off
/// is shown **greyed**, not hidden: recording writes what this client receives,
/// so a channel it is not listening to would produce an empty file - and a
/// control that simply vanishes tells the operator nothing about why. The
/// tooltip on the disabled box says what to switch on.
///
/// This is where a present FT-991A or FTX-1 went missing from the row: it was
/// gated on the audio subscription alone, so a radio that was there but muted
/// left no trace at all (owner, 2026-08-20).
fn rec_source(ui: &mut egui::Ui, exists: bool, audio_on: bool, on: &mut bool, label: &str) {
    if !exists {
        *on = false;
        return;
    }
    if !audio_on {
        // Never leave a tick standing on a channel that cannot deliver: the
        // Rec button reads these straight out.
        *on = false;
    }
    ui.add_enabled_ui(audio_on, |ui| {
        ui.checkbox(on, label)
            .on_disabled_hover_text(rust_i18n::t!("screen_rec_needs_audio").to_string());
    });
}

impl SdrRemoteApp {
    pub(super) fn render_server_screen(&mut self, ui: &mut egui::Ui) {
        // Repaint at 30fps when connected (live audio levels), slow when idle
        let ms = if self.connected { 33 } else { 500 };
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(ms));

        // Server address + password. The "Wizard" and "Recenter windows"
        // buttons stood on the right of this row until build 197; they are on
        // the relay group's header row now (owner, 2026-10-14).
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
        super::helpers::rule(ui);

        // The relay group, a group like the others (build 198; owner: generic
        // where it can be), with "Wizard" and "Recenter windows" on the right
        // of its header row (owner, 2026-10-14). The header reports the two
        // clicks; they are handled below, after the body has had `self`.
        let relay_title = RichText::new(rust_i18n::t!("screen_relay_connection").to_string()).strong();
        let (folds_open, (wizard_clicked, recenter_clicked)) = super::helpers::settings_group_with(
            ui,
            self.folds_open,
            super::config::Fold::Relay,
            relay_title,
            |ui| {
                let tip = rust_i18n::t!("screen_rerun_setup_wizard").to_string();
                let wizard = ui.small_button(rust_i18n::t!("screen_wizard").to_string()).on_hover_text(tip).clicked();
                let recenter_tip = rust_i18n::t!("screen_recenter_windows_tooltip").to_string();
                let recenter = ui
                    .small_button(rust_i18n::t!("screen_recenter_windows").to_string())
                    .on_hover_text(recenter_tip)
                    .clicked();
                (wizard, recenter)
            },
            |ui| {
            // All fields save immediately on change (no separate "Apply" button anymore).
            let mut relay_changed = false;
            // What the engine actually runs on: the relay setting applies at the
            // next start, and the line says so while the two differ (review 110).
            let route = self.state_rx.borrow().route;
            // The route the settings ask for, by the rule main.rs chooses with.
            let expected = sdr_remote_logic::measure::Route::asked_for(sdr_remote_relay::is_configured(
                self.relay_enabled,
                &self.relay_url,
                &self.relay_station,
                &self.relay_token,
            ));
            let differs = expected != route;
            // The way the traffic takes now (build 139) while a session runs;
            // before that, what the engine was started with.
            let lang = sdr_remote_logic::i18n::Lang::from_code(&self.ui_language);
            let in_use = match self.state_rx.borrow().path {
                Some(kind) => sdr_remote_logic::path_name::long_name(kind, lang),
                None => route.label().to_string(),
            };
            let text = rust_i18n::t!("screen_route_in_use", route = in_use).to_string();
            let text = if differs {
                format!("{text} - {}", rust_i18n::t!("screen_route_restart"))
            } else {
                text
            };
            ui.label(
                egui::RichText::new(text)
                    .size(11.0)
                    .color(if differs { egui::Color32::from_rgb(200, 160, 40) } else { ui.visuals().weak_text_color() }),
            );
            if ui
                .checkbox(
                    &mut self.relay_enabled,
                    rust_i18n::t!("screen_connect_via_relay").to_string(),
                )
                .changed()
            {
                relay_changed = true;
            }
            ui.horizontal(|ui| {
                ui.label(rust_i18n::t!("screen_relay_url").to_string());
                relay_changed |= ui
                    .add(
                        egui::TextEdit::singleline(&mut self.relay_url)
                            .desired_width(260.0)
                            // wss://, not ws:// - the relay sits behind TLS on
                            // 443, and the first real user copied this hint
                            // verbatim and could not connect.
                            .hint_text("wss://relay.example.com"),
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
            },
        );
        self.folds_open = folds_open;
        if wizard_clicked {
            self.wizard_state = Some(super::wizard::WizardState::new(
                self.server_input.clone(),
                self.password_input.clone(),
            ));
        }
        if recenter_clicked {
            let ctx = ui.ctx().clone();
            self.recenter_popouts(&ctx);
        }
        // PATCH-1: read connect_status from logic-state and render via
        // i18n helper. Single source of truth - same NL/EN text as Android
        // bridge. The legacy `auth_rejected` / `totp_required` booleans are
        // still set for back-compat but new UI code uses connect_status.
        let connect_status = self.state_rx.borrow().connect_status.clone();
        use sdr_remote_logic::i18n::{connect_status_text, Lang};
        use sdr_remote_logic::state::ConnectStatus;

        // PATCH-1: language from client config (set via `language=nl|en` in
        // thetislink-client.conf). Defaults to English when unset.
        let lang = Lang::from_code(&self.ui_language);

        // Whether the match below draws a line of status (build 198).
        let mut status_shown = true;
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
                } else {
                    status_shown = false;
                }
            }
        }

        // The relay group's line below is the line above the chat when no
        // status stands in between (build 198): one line, not two.
        if status_shown {
            super::helpers::rule(ui);
        }

        // Chat, with the unread count on it (design §1.7): without a number
        // where people already look, nobody opens the window - and one channel
        // nobody opens is as empty as two were.
        //
        // Shown whether or not a relay is set up. From 2026-08-17 it was hidden
        // without one, because the window could then only say "no chat here" and
        // an unexplained dead control is worse than none. That reason is gone:
        // the window now explains what the chat is, that it needs a relay that
        // offers one, and how to reach one. So the button is the way in - the
        // people who would benefit from the relay are exactly the ones who click
        // out of curiosity, and they are the ones the feedback comes from
        // (owner, 2026-08-20).
        ui.horizontal(|ui| {
            let label = if self.chat.unread() > 0 {
                format!("{} ({})", rust_i18n::t!("chat_window_button"), self.chat.unread())
            } else {
                rust_i18n::t!("chat_window_button").to_string()
            };
            let btn = egui::Button::new(RichText::new(label).strong());
            // Toggled-on gets the fill, per the house rule; a momentary
            // action would not.
            let btn = if self.chat_open {
                btn.fill(Color32::from_rgb(70, 110, 170))
            } else {
                btn
            };
            if ui.add(btn).clicked() {
                self.chat_open = !self.chat_open;
                self.save_full_config();
            }
            if self.chat.unread() > 0 {
                ui.label(
                    RichText::new(rust_i18n::t!("chat_unread_hint").to_string())
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
            }
        });
        super::helpers::rule(ui);

        // Language, UI scale and theme as one group, behind a chevron like the
        // others below it (owner, 2026-10-14). The audio group directly after
        // shares this group's line below as its line above.
        self.folds_open = super::helpers::settings_group(
            ui,
            self.folds_open,
            super::config::Fold::Display,
            RichText::new(rust_i18n::t!("screen_group_display").to_string()).strong(),
            |ui| {
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
                    changed |= theme::colour_button(ui, &mut p.background).changed();
                    ui.label(rust_i18n::t!("screen_widgets").to_string());
                    changed |= theme::colour_button(ui, &mut p.widget).changed();
                    ui.label(rust_i18n::t!("screen_text").to_string());
                    changed |= theme::colour_button(ui, &mut p.text).changed();
                    ui.label(rust_i18n::t!("screen_slider_knob").to_string());
                    changed |= theme::colour_button(ui, &mut p.accent).changed();
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
            },
        );

        self.folds_open = super::helpers::settings_group(
            ui,
            self.folds_open,
            super::config::Fold::Audio,
            RichText::new(rust_i18n::t!("screen_group_audio").to_string()).strong(),
            |ui| {
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
                super::helpers::rule(ui);
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

            super::helpers::rule(ui);
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
            },
        );
        // (The group's own line below separates it from what follows.)

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
            // playback_level: concealed audio keeps it > 0, which left RX1 showing.
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

        // The roger beep. One place for the sound of it, and a tick per
        // channel - an operator wants one beep of their own, not three that
        // differ by accident.
        //
        // Only channels that exist get a tick, and with none of them the whole
        // section goes: a beep is a thing you put ON something. The row used to
        // list Thetis, "Yaesu 1: 991A" and "Yaesu 2: FTX1" on every station,
        // including one with no radio at all - and those two names were startup
        // guesses, not anything a server had said (owner, 2026-08-20).
        //
        // The stored choices are deliberately NOT cleared while a channel is
        // away, unlike the recording ticks above: recording is a thing you start
        // now, the beep is a setting, and a radio that comes back should bring
        // its setting with it rather than a silent default.
        // Presence, not the audio subscription. The beep rides out on the
        // TRANSMIT path - the tone goes while the transmitter is still keyed -
        // so whether this client happens to be listening to the radio has
        // nothing to do with it. Gating it on the audio toggle hid a connected
        // FTX-1 from the row entirely (owner, 2026-08-20). Recording is the
        // opposite case and keeps its own rule: see `rec_source`.
        let roger_thetis = self.thetis_configured;
        let roger_radio1 = self.yaesu_present_last;
        let roger_radio2 = self.yaesu2_present_last;
        // Whether the roger group is drawn: asked once, because the line above
        // the Bluetooth group depends on the same answer.
        let roger_shown = roger_thetis || roger_radio1 || roger_radio2;
        if roger_shown {
            super::helpers::rule(ui);
            self.folds_open = super::helpers::settings_group(
                ui,
                self.folds_open,
                super::config::Fold::Roger,
                RichText::new(rust_i18n::t!("screen_group_roger").to_string()).strong(),
                |ui| {
                let roger_before = self.roger;
                ui.horizontal(|ui| {
                    ui.label(rust_i18n::t!("screen_roger").to_string());
                    ui.add(
                        egui::DragValue::new(&mut self.roger.freq_hz)
                            .speed(10.0)
                            .range(sdr_remote_logic::roger::FREQ_MIN_HZ..=sdr_remote_logic::roger::FREQ_MAX_HZ)
                            .suffix(" Hz"),
                    )
                    .on_hover_text(rust_i18n::t!("screen_roger_freq_tooltip").to_string());
                    ui.add(
                        egui::DragValue::new(&mut self.roger.duration_ms)
                            .speed(10.0)
                            .range(sdr_remote_logic::roger::DURATION_MIN_MS..=sdr_remote_logic::roger::DURATION_MAX_MS)
                            .suffix(" ms"),
                    )
                    .on_hover_text(rust_i18n::t!("screen_roger_duration_tooltip").to_string());
                    ui.label(rust_i18n::t!("screen_roger_volume").to_string());
                    let resp = ui.add(egui::Slider::new(&mut self.roger.volume, 0.0..=1.0).fixed_decimals(2));
                    let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.roger.volume, 0.0..=1.0, 0.05);
                    let _ = scrolled;
                    ui.checkbox(&mut self.roger.include_fm, rust_i18n::t!("screen_roger_fm").to_string())
                        .on_hover_text(rust_i18n::t!("screen_roger_fm_tooltip").to_string());
                });
                let l1 = self.yaesu_slot_label(0);
                let l2 = self.yaesu_slot_label(1);
                ui.horizontal(|ui| {
                    ui.label(rust_i18n::t!("screen_roger_on").to_string());
                    if roger_thetis {
                        ui.checkbox(&mut self.roger.on_thetis, "Thetis");
                    }
                    if roger_radio1 {
                        ui.checkbox(&mut self.roger.on_radio1, l1);
                    }
                    if roger_radio2 {
                        ui.checkbox(&mut self.roger.on_radio2, l2);
                    }
                    ui.label(
                        RichText::new(rust_i18n::t!("screen_roger_note").to_string())
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                });
                if self.roger != roger_before {
                    self.roger = self.roger.clamped();
                    super::config::save_roger(&self.roger);
                    let _ = self.cmd_tx.send(Command::SetRogerBeep(self.roger));
                }
                },
            );
        }

        // The Bluetooth PTT button. On this tab and not with MIDI, because it is
        // a piece of station equipment like the roger beep, not a controller
        // mapping (owner, 2026-09-17). The line above it is the roger group's
        // line below, when that group is there.
        if !roger_shown {
            super::helpers::rule(ui);
        }
        self.folds_open = super::helpers::settings_group(
            ui,
            self.folds_open,
            super::config::Fold::Ble,
            RichText::new(rust_i18n::t!("screen_group_ble").to_string()).strong(),
            |ui| self.render_ble_section(ui),
        );

        // The Morse paddle: station equipment too, like the button above. Its
        // line above is the Bluetooth group's line below - which is there
        // because the Bluetooth group is always drawn. Should that ever become
        // conditional, this group needs its own line above in that case.
        self.folds_open = super::helpers::settings_group(
            ui,
            self.folds_open,
            super::config::Fold::Cw,
            RichText::new(rust_i18n::t!("screen_group_cw").to_string()).strong(),
            |ui| self.render_cw_section(ui),
        );

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
                let thetis = self.thetis_configured;
                let both_rx = thetis && self.rx2_present;
                rec_source(ui, thetis, self.rx1_enabled, &mut self.rec_rx1, "RX1");
                rec_source(ui, both_rx, self.rx2_enabled, &mut self.rec_rx2, "RX2");
                rec_source(
                    ui,
                    self.yaesu_present_last,
                    self.yaesu_enabled,
                    &mut self.rec_yaesu,
                    rec_yaesu_label.as_str(),
                );
                rec_source(
                    ui,
                    self.yaesu2_present_last,
                    self.yaesu2_enabled,
                    &mut self.rec_yaesu2,
                    rec_yaesu2_label.as_str(),
                );
                rec_source(ui, thetis, self.vrx1_enabled, &mut self.rec_vrx1, "VRX1");
                rec_source(ui, both_rx, self.vrx2_enabled, &mut self.rec_vrx2, "VRX2");
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
                // Play, and - when the last Start wrote more than one file -
                // which of them. Without the picker the button silently meant
                // the last source in a fixed order, so recording both radios
                // could only ever play back the second one.
                if !self.last_recorded.is_empty() {
                    self.play_ticked.resize(self.last_recorded.len(), true);
                    // A tick per recording, and everything ticked plays
                    // together. Only worth showing when there is a choice: with
                    // one recording the button says all there is to say.
                    if self.last_recorded.len() > 1 {
                        let mut changed = false;
                        for (i, (label, _)) in self.last_recorded.iter().enumerate() {
                            if ui.checkbox(&mut self.play_ticked[i], label).changed() {
                                changed = true;
                            }
                        }
                        // Changing the selection mid-playback would leave the
                        // button offering to stop something else.
                        if changed && self.playing {
                            let _ = self.cmd_tx.send(Command::StopPlayback);
                            self.playing = false;
                        }
                    }
                    let wav_paths: Vec<String> = self
                        .last_recorded
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| self.play_ticked.get(*i).copied().unwrap_or(false))
                        .map(|(_, (_, path))| path.clone())
                        .collect();
                    if !self.playing {
                        if ui
                            .add_enabled(
                                !wav_paths.is_empty(),
                                egui::Button::new(rust_i18n::t!("screen_play_icon").to_string()),
                            )
                            .on_disabled_hover_text(rust_i18n::t!("screen_play_none_ticked").to_string())
                            .clicked()
                        {
                            let _ = self.cmd_tx.send(Command::PlayRecording { paths: wav_paths });
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

        // Multi-TX: the same microphone to more than one transmitter at a time.
        //
        // Only shown when there is more than one transmitter - with one radio the choice
        // means nothing, and a checkbox that does nothing is worse than no checkbox. So it
        // is not greyed out but absent.
        //
        // Off: one transmitter at a time, across all three. Whoever is already
        // transmitting keeps it; whoever presses another briefly sees "TX on another
        // radio" and nothing else happens. Without that rule two Yaesus keyed at once
        // while only one got modulation - two red buttons and a bare carrier (owner,
        // 2026-09-03).
        //
        // On: the same microphone to every keyed transmitter, so Thetis and both Yaesus
        // together.
        //
        // Note: Thetis alongside a Yaesu was always possible, because of the separate
        // encoding paths. With this checkbox OFF that is no longer so - which is intended,
        // and is what "one transmitter at a time" means.
        //
        // Desktop only. The phone keys one radio at a time, and there that is the
        // intention.
        let transmitters = [self.thetis_configured, self.yaesu_present_last, self.yaesu2_present_last]
            .iter()
            .filter(|&&p| p)
            .count();
        if transmitters > 1 {
            super::helpers::rule(ui);
            if ui
                .checkbox(&mut self.multi_tx, rust_i18n::t!("screen_multi_tx").to_string())
                .on_hover_text(rust_i18n::t!("screen_multi_tx_tooltip").to_string())
                .changed()
            {
                let _ = self.cmd_tx.send(Command::SetMultiTx(self.multi_tx));
                self.save_ptt_config();
            }
        }

        super::helpers::rule(ui);

        // The server is not running this build. Said here, next to everything
        // else about the connection, and said only - nothing refuses to work
        // over it. An operator locked out of their own station at a distance by
        // a half-finished update is worse off than one reading this line.
        if self.server_build.is_trouble() {
            let theirs = self
                .server_build
                .theirs()
                .map(|t| t.to_string())
                .unwrap_or_else(|| rust_i18n::t!("screen_build_too_old").to_string());
            ui.colored_label(
                egui::Color32::from_rgb(220, 150, 40),
                rust_i18n::t!(
                    "screen_build_mismatch",
                    server = theirs,
                    client = sdr_remote_core::version_string()
                )
                .to_string(),
            );
            super::helpers::rule(ui);
        }

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
                self.folds_open = super::helpers::fold(
                    ui,
                    self.folds_open,
                    super::config::Fold::BandwidthBreakdown,
                    rust_i18n::t!("screen_down_rx").to_string(),
                );
                ui.label(format!("{} Kbit/s", self.down_kbps));
                ui.end_row();

                ui.label(rust_i18n::t!("screen_up_tx").to_string());
                ui.label(format!("{} Kbit/s", self.up_kbps));
                ui.end_row();
            });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("screen_audio_streams").to_string());
        });
        egui::Grid::new("audio_stream_stats_grid")
            .num_columns(4)
            .spacing([14.0, 3.0])
            .show(ui, |ui| {
                ui.label(egui::RichText::new(rust_i18n::t!("screen_col_stream").to_string()).strong().size(11.0));
                ui.label(egui::RichText::new(rust_i18n::t!("screen_col_jitter").to_string()).strong().size(11.0));
                ui.label(egui::RichText::new(rust_i18n::t!("screen_col_buffer").to_string()).strong().size(11.0));
                ui.label(egui::RichText::new(rust_i18n::t!("screen_col_packets").to_string()).strong().size(11.0));
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
                    ui.end_row();
                }
                if self.yaesu_enabled {
                    shown = true;
                    ui.label(self.yaesu_slot_label(0));
                    ui.label(format!("{:.1} ms", self.yaesu_jitter_ms));
                    ui.label(format!("{} frames", self.yaesu_buffer_depth));
                    ui.label(format!("{}", self.yaesu_audio_packets));
                    ui.end_row();
                }
                if self.yaesu2_enabled {
                    shown = true;
                    ui.label(self.yaesu_slot_label(1));
                    ui.label(format!("{:.1} ms", self.yaesu2_jitter_ms));
                    ui.label(format!("{} frames", self.yaesu2_buffer_depth));
                    ui.label(format!("{}", self.yaesu2_audio_packets));
                    ui.end_row();
                }
                if self.vrx1_enabled {
                    shown = true;
                    ui.label("VRX1");
                    ui.label(format!("{:.1} ms", self.vrx1_jitter_ms));
                    ui.label(format!("{} frames", self.vrx1_buffer_depth));
                    ui.label(format!("{}", self.vrx1_audio_packets));
                    ui.end_row();
                }
                if self.vrx2_enabled {
                    shown = true;
                    ui.label("VRX2");
                    ui.label(format!("{:.1} ms", self.vrx2_jitter_ms));
                    ui.label(format!("{} frames", self.vrx2_buffer_depth));
                    ui.label(format!("{}", self.vrx2_audio_packets));
                    ui.end_row();
                }
                if !shown {
                    ui.label(egui::RichText::new(rust_i18n::t!("screen_no_audio_stream_yet").to_string()).color(egui::Color32::GRAY));
                    ui.label("");
                    ui.label("");
                    ui.label("");
                    ui.end_row();
                }
            });

        // Packet loss is a property of the LINK, not of a stream: every stream
        // to this client goes down one socket to one address. It used to sit in
        // the RX1+RX2 row with a dash on the other four, which read as "no
        // measurement for those" - and since the meter now feeds on whatever
        // streams are running, a Yaesu-only session would have shown four dashes
        // while it was measuring the Yaesu. So it is stated once, here.
        ui.label(
            egui::RichText::new(format!(
                "{}: {}%",
                rust_i18n::t!("screen_link_loss"),
                self.loss_percent
            ))
            .size(11.0),
        );

        // Per-stream breakdown - only show when the user
        // has clicked "Down". List is empty for the first ~5 s
        // after connect (engine fills it on every 5 s window rollover).
        if self.folds_open.is_open(super::config::Fold::BandwidthBreakdown) {
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
            // Only where there is a cluster to switch off. A server with no
            // callsign, or with the cluster off, will never send a spot, and the
            // tick sat there ON regardless - promising a stream that could not
            // come (owner, 2026-08-20).
            if self.dx_cluster_available {
                ui.add_space(4.0);
                let mut dx_spots = self.dx_spots_enabled;
                if ui.checkbox(&mut dx_spots, rust_i18n::t!("screen_receive_dx_spots").to_string()).changed() {
                    self.dx_spots_enabled = dx_spots;
                    let _ = self.cmd_tx.send(sdr_remote_logic::commands::Command::SetDxSpotsEnabled(dx_spots));
                }
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
            super::helpers::rule(ui);
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
        super::helpers::rule(ui);
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

        // The developer mode, together and last, behind its own chevron
        // (owner, 2026-10-24): the code field or the sign, and the tools that
        // only measure.
        //
        // Measurement mode (open point 34): every audio arrival and heartbeat
        // round trip to a file, for comparing the link before and after a
        // change. Off by default; a new file each time it is switched on, in a
        // folder next to the program.
        super::helpers::rule(ui);
        self.folds_open = super::helpers::settings_group(
            ui,
            self.folds_open,
            super::config::Fold::Developer,
            rust_i18n::t!("screen_dev_section").to_string(),
            |ui| {
                self.render_developer_mode(ui);
                if self.developer_mode {
                    ui.horizontal(|ui| {
                        ui.label(rust_i18n::t!("screen_measure").to_string());
                        match self.measurement_file.clone() {
                            Some(path) => {
                                if ui
                                    .button(RichText::new(rust_i18n::t!("screen_stop_icon").to_string()).color(Color32::WHITE))
                                    .highlight()
                                    .on_hover_text(rust_i18n::t!("screen_measure_stop_tooltip").to_string())
                                    .clicked()
                                {
                                    let _ = self.cmd_tx.send(Command::SetMeasurement(None));
                                }
                                let name = std::path::Path::new(&path)
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_else(|| path.clone());
                                ui.label(name).on_hover_text(path);
                            }
                            None => {
                                if ui
                                    .button(rust_i18n::t!("screen_measure_start").to_string())
                                    .on_hover_text(rust_i18n::t!("screen_measure_tooltip").to_string())
                                    .clicked()
                                {
                                    let dir = std::env::current_exe()
                                        .ok()
                                        .and_then(|p| p.parent().map(|d| d.join("meting")))
                                        .unwrap_or_else(|| std::path::PathBuf::from("meting"));
                                    let _ = self.cmd_tx.send(Command::SetMeasurement(Some(dir.to_string_lossy().to_string())));
                                }
                            }
                        }
                        // "Relay only" (build 137): for comparing a session through the
                        // relay with one that went direct. Grey without the relay in use;
                        // two texts, because egui shows `on_hover_text` only when enabled.
                        let (via_relay, mut relay_only, mut force_tcp) = {
                            let st = self.state_rx.borrow();
                            (st.route == sdr_remote_logic::measure::Route::QuicRelay, st.relay_only, st.force_tcp)
                        };
                        ui.add_enabled_ui(via_relay, |ui| {
                            if ui
                                .checkbox(&mut relay_only, rust_i18n::t!("screen_relay_only").to_string())
                                .on_hover_text(rust_i18n::t!("screen_relay_only_tooltip").to_string())
                                .on_disabled_hover_text(rust_i18n::t!("screen_relay_only_no_relay_tooltip").to_string())
                                .changed()
                            {
                                let _ = self.cmd_tx.send(Command::SetRelayOnly(relay_only));
                            }
                            // "Force TCP" (build 146): what the TCP fallback route costs,
                            // measured on a network where UDP works.
                            if ui
                                .checkbox(&mut force_tcp, rust_i18n::t!("screen_force_tcp").to_string())
                                .on_hover_text(rust_i18n::t!("screen_force_tcp_tooltip").to_string())
                                .on_disabled_hover_text(rust_i18n::t!("screen_relay_only_no_relay_tooltip").to_string())
                                .changed()
                            {
                                let _ = self.cmd_tx.send(Command::SetForceTcp(force_tcp));
                            }
                        });
                    });
                }
            },
        );
    }
}
