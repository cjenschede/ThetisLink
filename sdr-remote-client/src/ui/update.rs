// SPDX-License-Identifier: GPL-2.0-or-later
//! The eframe main loop: `eframe::App::update` for `SdrRemoteApp` - the per-frame
//! driver that pumps state sync, input handling, spectrum/meters and every panel/
//! popout render, then schedules the next repaint. Extracted verbatim from
//! `ui/mod.rs` - pure relocation, no behaviour change. `use super::*;` pulls in the
//! parent module's types, imports and the `SdrRemoteApp` inherent methods this loop
//! calls.

use super::*;

impl eframe::App for SdrRemoteApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Bump UI frame id - stamps all UiEvents with a monotonic frame id for
        // timeline correlation.
        controls::begin_frame();

        // Clear per-frame flags
        ctx.memory_mut(|mem| mem.data.remove::<bool>(egui::Id::new("freq_scroll_consumed")));

        // Chat (docs/internal/DESIGN-relay-chat.md). Everything it does is a
        // channel send and a try_recv: no network call, no lock held, nothing
        // this frame can wait on. That is design §2.4 made structural rather
        // than remembered - a chat service that is down cannot slow this down,
        // because nothing here ever asks it anything.
        {
            let ticket = self
                .relay_status
                .as_ref()
                .and_then(|h| h.snapshot().chat_ticket);
            // The address only counts as a relay once the relay is actually set
            // up; an address left behind in the settings with the tick off is not
            // one, and saying "this relay offers no chat" about it blames a relay
            // that was never contacted.
            let relay_url = sdr_remote_relay::chat_relay_url(
                self.relay_enabled,
                &self.relay_url,
                &self.relay_station,
                &self.relay_token,
            )
            .to_string();
            // This used to slam the window shut whenever no relay was set up,
            // because back then the button that reopens it was hidden in the
            // same case and a window nothing could reopen would have been a
            // trap. The button is now always there, so the rule became the bug:
            // clicking it set `chat_open`, and this closed it again in the same
            // frame - a button that did nothing at all (2026-08-20). Without a
            // relay the window opens and says what the chat is, which is the
            // whole reason the button stayed.
            self.chat.tick(&relay_url, ticket.as_deref(), self.chat_open);
        }
        if self.chat_open {
            self.render_chat_popout(ctx);
        }
        // An answer folded away is written down straight away. Only when it
        // changed, so this costs nothing on an ordinary frame.
        if self.chat.take_answers_seen_changed() {
            super::config::save_chat_answers_seen(&self.chat.seen_ids());
        }

        // Base egui visuals per selected theme (step 1). Classic reproduces the original
        // light-grey scheme; Dark is the tuned dark scheme. Colours that read from
        // ui.visuals() follow automatically; hard-coded ones are migrated in later steps.
        theme::apply_visuals(ctx, self.theme_variant, &self.theme_custom);

        self.sync_state();
        self.process_midi_events();

        // PATCH-4: first-run / re-launched wizard owns the viewport
        // while present. Render it edge-to-edge and short-circuit the
        // rest of update() - none of the regular UI panes are valid
        // until the wizard exits.
        if self.wizard_state.is_some() {
            let lang = sdr_remote_logic::i18n::Lang::from_code(&self.ui_language);
            let outcome = egui::CentralPanel::default()
                .show(ctx, |ui| {
                    let st = self.wizard_state.as_mut().expect("checked above");
                    wizard::render_wizard(
                        ui,
                        st,
                        &self.cmd_tx,
                        &self.state_rx,
                        self.mdns_browse.as_ref(),
                        lang,
                    )
                })
                .inner;
            match outcome {
                wizard::WizardOutcome::Continue => {}
                wizard::WizardOutcome::SkipToManual => {
                    // No config write - wizard re-arms on next launch.
                    self.wizard_state = None;
                }
                wizard::WizardOutcome::Finished => {
                    // Connect succeeded; persist server+password and bump
                    // successful_connects so we don't re-arm next launch.
                    if let Some(ref st) = self.wizard_state {
                        self.server_input = st.server_input.clone();
                        self.password_input = st.password_input.clone();
                    }
                    self.save_full_config();
                    crate::ui::config::mark_successful_connect();
                    self.wizard_state = None;
                }
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
            return;
        }

        // PATCH-4: bump the successful-connect counter the first time the
        // user reaches Connected (incl. when they skipped the wizard).
        // Idempotent - mark_successful_connect() no-ops past 0.
        if self.connected {
            crate::ui::config::mark_successful_connect();
        }

        // Opt-in: start Thetis on the server PC if it is not running (once).
        self.maybe_autostart_thetis();

        // Sync frequency and mute state to the selected WebSDR target.
        let (websdr_freq_hz, websdr_mode) = self.catsync_target_freq_mode();
        self.catsync.sync_freq(websdr_freq_hz, websdr_mode);
        self.catsync.update_mute(self.catsync_target_tx_active());

        // (TX spectrum override sets ref/range directly in PTT handler)

        // Track main-window geometry for persistence. Position is gated
        // on pointer-up like popouts to avoid disk-I/O stalls during a drag
        // (which causes the OS to oscillate the window between frames).
        // Size via screen_rect (always available; i.viewport().inner_rect can
        // be None on Windows/eframe -> capture then never fired).
        // Geometry is stored in SYSTEM points, deliberately. egui reports
        // screen_rect/outer_rect in ITS points, which shrink as the UI zoom goes
        // down, while with_position/with_inner_size and the OS monitor rects are
        // in system points where that zoom does not exist. At zoom 1.0 the two
        // coincide, which is why this went unnoticed; at 70% a restored window
        // lands in the wrong place and comes back the wrong size. Multiplying by
        // the zoom converts egui points back to system points, so a saved layout
        // survives a change of UI scale.
        let z = ctx.zoom_factor().max(0.01);
        {
            let sr = ctx.screen_rect();
            let (w, h) = (sr.width() * z, sr.height() * z);
            if (w - self.window_w).abs() > 5.0 || (h - self.window_h).abs() > 5.0 {
                self.window_w = w;
                self.window_h = h;
                self.main_geom_dirty = true;
            }
        }
        if let Some(outer) = ctx.input(|i| i.viewport().outer_rect) {
            let np = egui::pos2(outer.min.x * z, outer.min.y * z);
            if self.main_window_pos.map_or(true, |p| (p.x - np.x).abs() > 5.0 || (p.y - np.y).abs() > 5.0) {
                self.main_window_pos = Some(np);
                self.main_geom_dirty = true;
            }
        }
        // Write out the final geometry once the mouse is released (not during the drag:
        // per-frame save while dragging causes I/O-stalls + window oscillation). The
        // dirty flag bridges the gap: the change falls during pointer-down, the
        // save fires on the first pointer-up frame after.
        if self.main_geom_dirty && !ctx.input(|i| i.pointer.any_down()) {
            self.save_full_config();
            self.main_geom_dirty = false;
        }

        // Finish a recall that had to wait for a window to exist.
        self.apply_pending_layout(ctx);

        // UI scale. Two directions, on purpose:
        //  - our setting -> egui, so a stored choice applies on startup and on change;
        //  - egui -> our setting, so egui's own Ctrl+/Ctrl-/Ctrl+0 is picked up and
        //    persisted instead of being silently reset on the next launch.
        // Applies to the pop-outs too: they share this context.
        let egui_zoom = ctx.zoom_factor();
        if (egui_zoom - self.ui_zoom).abs() > 0.001 {
            if self.ui_zoom_pending {
                ctx.set_zoom_factor(self.ui_zoom);
                self.ui_zoom_pending = false;
            } else {
                self.ui_zoom = egui_zoom.clamp(0.5, 2.0);
                self.save_full_config();
            }
        }

        // (removed in 2.7.0 build 3) The master volume used to be forced back to 100%
        // whenever RX1 and RX2 were not both popped out. That belonged to the morphing
        // slider: the top row then showed VFO A in every other layout, and a master left
        // at 50% would have attenuated everything while the visible slider said otherwise.
        //
        // Build 103 made that row permanently the master and removed the morphing, but
        // left this behind - so on any layout without both pop-outs the master snapped
        // back to 100% on the next repaint. Visible on a small screen, where those two
        // windows do not fit; invisible on a desktop that always has both open, which is
        // why it survived testing.

        // Push new waterfall data (always, before rendering).
        // Tuning-latch on view-data removed (experiment 2026-05-06): the
        // per-row absolute mapping in render_waterfall (spectrum.rs:646)
        // uses each row's own center_hz + span_hz. Late detail-FFT rows
        // are therefore automatically rendered at their real freq position -
        // 1-2 rows briefly at a shifted spot during fast tuning, then self-
        // correcting. Previously view-data was discarded during the tuning
        // latch so only full-DDC (coarser) went into the row.
        // Without the full-DDC row there is nothing to lay the view on, so the
        // waterfall is fed from the extracted view alone - exactly what VRX does
        // (`ChannelSpectrum::push_row`). `push` requires a full row and would
        // otherwise stop adding lines altogether.
        if self.full_spectrum_enabled {
            self.waterfall.push(
                &self.full_spectrum_bins, self.full_spectrum_center_hz,
                self.full_spectrum_span_hz, self.full_spectrum_sequence,
                &self.spectrum_bins, self.spectrum_center_hz, self.spectrum_span_hz,
            );
        } else {
            self.waterfall.push_full_only(
                &self.spectrum_bins, self.spectrum_center_hz,
                self.spectrum_span_hz, self.last_spectrum_seq,
            );
        }

        // Push RX2 waterfall data - same principle as RX1.
        if self.full_spectrum_enabled {
            self.rx2_waterfall.push(
                &self.rx2_full_spectrum_bins, self.rx2_full_spectrum_center_hz,
                self.rx2_full_spectrum_span_hz, self.rx2_full_spectrum_sequence,
                &self.rx2_spectrum_bins, self.rx2_spectrum_center_hz, self.rx2_spectrum_span_hz,
            );
        } else {
            self.rx2_waterfall.push_full_only(
                &self.rx2_spectrum_bins, self.rx2_spectrum_center_hz,
                self.rx2_spectrum_span_hz, self.rx2_last_spectrum_seq,
            );
        }

        // Sticky top panel: PTT button + local volume (always visible)
        egui::TopBottomPanel::top("ptt_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                // Is the current Amplitec-A position RX-only? The client knows this
                // itself from the power-cap table (switch_a + tx_blocked), so no
                // server push needed. On such a position we block the PTT button
                // and suppress spacebar/MIDI PTT (see new_ptt below).
                let current_pos_rx_only = (1..=6u8).contains(&self.amplitec_switch_a)
                    && self.amplitec_power_tx_blocked
                        [self.amplitec_switch_a.saturating_sub(1) as usize];

                // PTT button (compact for top bar)
                let (ptt_color, ptt_text, ptt_locked) = if !self.thetis_configured {
                    // No Thetis configured -> this (Thetis) PTT keys nothing. Disable it
                    // so the spacebar/mouse cannot turn it red for a radio that is absent.
                    (Color32::from_rgb(60, 60, 60), "PTT".to_string(), true)
                } else if current_pos_rx_only {
                    (Color32::from_rgb(120, 50, 50), rust_i18n::t!("main_rx_only").to_string(), true)
                } else if self.other_tx {
                    (Color32::from_rgb(200, 120, 0), rust_i18n::t!("main_tx_in_use").to_string(), true)
                } else if self.ptt {
                    (Color32::RED, "TX".to_string(), false)
                } else {
                    (Color32::from_rgb(60, 60, 60), "PTT".to_string(), false)
                };

                let button = egui::Button::new(
                    RichText::new(ptt_text).size(20.0).color(Color32::WHITE),
                )
                .fill(ptt_color)
                .min_size(Vec2::new(80.0, 36.0));

                let response = ui.push_id("ptt_button", |ui| {
                    ui.add_enabled(!ptt_locked, button)
                }).inner;
                if current_pos_rx_only {
                    response.clone().on_hover_text(
                        rust_i18n::t!("main_hover_rx_only_antenna").to_string(),
                    );
                }

                // PTT button: toggle or momentary (push-to-talk) mode
                if self.ptt_toggle_mode {
                    // Toggle: click to switch on/off
                    if response.clicked() {
                        self.mouse_ptt = !self.mouse_ptt;
                    }
                } else {
                    // Momentary: hold to TX, release to RX
                    let pointer_on_btn = ui.input(|i| {
                        i.pointer.primary_down()
                            && response.rect.contains(i.pointer.interact_pos().unwrap_or(Pos2::ZERO))
                    });
                    self.mouse_ptt = pointer_on_btn;
                }

                let space_held = ui.input(|i| i.key_down(egui::Key::Space));
                // RX-only Amplitec position: fully suppress the client PTT -
                // including spacebar and MIDI. Also reset the toggle states
                // (mouse/MIDI), otherwise a toggle pressed during RX-only stays
                // "on" and it would still transmit as soon as we switch away from
                // the RX-only position. Spacebar is real-time
                // (key_down) and is caught by the `&&` condition.
                if current_pos_rx_only || !self.thetis_configured {
                    self.mouse_ptt = false;
                    self.midi_ptt = false;
                }
                // No Thetis -> never key the Thetis PTT (spacebar/mouse/MIDI all suppressed),
                // so the button cannot go red for a radio that is not there.
                let new_ptt = (self.mouse_ptt || space_held || self.midi_ptt)
                    && !current_pos_rx_only
                    && self.thetis_configured;
                if new_ptt != self.ptt {
                    self.midi.send_led(crate::midi::MidiAction::Ptt, new_ptt);
                    // TX spectrum override
                    if new_ptt {
                        // Entering TX: save ref, range, auto - then set TX defaults
                        self.tx_spectrum_saved_ref_db = Some(self.spectrum_ref_db);
                        self.tx_spectrum_saved_range = Some(self.spectrum_range_db);
                        self.tx_spectrum_saved_auto_ref = Some(self.auto_ref_enabled);
                        self.tx_spectrum_restore_auto_at = None;
                        self.auto_ref_enabled = false;
                        self.spectrum_ref_db = -30.0;
                        self.spectrum_range_db = 120.0;
                    } else {
                        // Leaving TX: restore ref+range immediately, auto_ref after 200ms
                        if let Some(saved) = self.tx_spectrum_saved_ref_db.take() {
                            self.spectrum_ref_db = saved;
                        }
                        if let Some(saved) = self.tx_spectrum_saved_range.take() {
                            self.spectrum_range_db = saved;
                        }
                        if self.tx_spectrum_saved_auto_ref.is_some() {
                            self.tx_spectrum_restore_auto_at = Some(std::time::Instant::now() + std::time::Duration::from_millis(200));
                        }
                    }
                }
                // Auto-switch TX profile for current mic before PTT on
                if new_ptt {
                    let mic = if self.selected_input.is_empty() { "(Default)" } else { &self.selected_input };
                    if let Some(profile_name) = self.mic_profile_map.get(mic).or_else(|| self.mic_profile_map.get("(Default)")) {
                        if let Some((idx, _)) = self.tx_profiles.iter().find(|(_, n)| n == profile_name) {
                            if *idx != self.tx_profile {
                                let _ = self.cmd_tx.send(Command::SetControl(sdr_remote_core::protocol::ControlId::TxProfile, *idx as u16));
                                self.tx_profile = *idx;
                            }
                        }
                    }
                }
                self.apply_ptt_spike_protection(false, new_ptt);
                let _ = self.cmd_tx.send(Command::SetPtt(new_ptt));
                self.ptt = new_ptt;

                // Tune button (visible when tuner available on the active antenna).
                // Multi-tuner note: stale-detection is handled SERVER-side now -
                // the server tracks per-tuner tune frequency and already broadcasts
                // state = IDLE when the VFO has drifted >25 kHz from the active
                // tuner's last-tune freq. Client-side stale check used to apply a
                // second filter on top, but its `tuner_tune_freq` only updated on
                // TUNING->DONE_OK transitions, so an Amplitec switch (which sees
                // IDLE->DONE_OK without TUNING) left the client comparing against
                // the wrong tuner's freq. Drop the client-side gate and trust the
                // server state directly.
                if self.tuner_can_tune && self.tuner_connected {
                    let olive_green = Color32::from_rgb(120, 160, 40);
                    let (tune_color, tune_text) = match self.tuner_state {
                        1 => (Color32::from_rgb(60, 120, 220), rust_i18n::t!("main_tune_tuning").to_string()),  // Tuning = blue
                        2 => (Color32::from_rgb(50, 180, 50), rust_i18n::t!("main_tune_ok").to_string()),  // Done OK = green
                        5 => (olive_green, rust_i18n::t!("main_tune_assumed").to_string()),  // Done assumed = olive green
                        3 | 4 => (Color32::from_rgb(220, 160, 40), rust_i18n::t!("main_tune_failed").to_string()),  // Timeout/Aborted = orange
                        _ => (Color32::from_rgb(80, 80, 80), rust_i18n::t!("main_tune").to_string()),  // Idle = grey
                    };

                    let tune_btn = egui::Button::new(
                        RichText::new(tune_text).size(16.0).color(Color32::WHITE),
                    )
                    .fill(tune_color)
                    .min_size(Vec2::new(70.0, 36.0));

                    if ui.add(tune_btn).clicked() {
                        if self.tuner_state == 1 {
                            let _ = self.cmd_tx.send(Command::TunerAbort);
                        } else {
                            let _ = self.cmd_tx.send(Command::TunerTune);
                        }
                    }
                }

                // SPE Expert compact status (only when active PA)
                if self.spe_connected && self.spe_active {
                    ui.separator();
                    // Operate/Standby toggle button with status text
                    let (btn_text, btn_color) = match self.spe_state {
                        2 => ("SPE OPR", Color32::from_rgb(0, 150, 0)),
                        1 => ("SPE STBY", Color32::from_rgb(255, 170, 40)),
                        _ => ("SPE OFF", Color32::GRAY),
                    };
                    let spe_btn = egui::Button::new(RichText::new(btn_text).size(11.0).strong().color(Color32::WHITE))
                        .fill(btn_color)
                        .min_size(Vec2::new(70.0, 20.0));
                    if ui.add(spe_btn).clicked() {
                        let _ = self.cmd_tx.send(Command::SpeOperate);
                    }

                    if self.spe_ptt {
                        ui.label(RichText::new(format!("{}W", self.spe_power_w)).size(12.0));
                        let swr = self.spe_swr_x10 as f32 / 10.0;
                        let swr_color = if swr > 3.0 { Color32::from_rgb(255, 80, 80) }
                            else if swr > 2.0 { Color32::from_rgb(255, 170, 40) }
                            else { ui.visuals().text_color() };
                        ui.colored_label(swr_color, RichText::new(format!("{:.1}", swr)).size(12.0));
                    }
                    ui.label(RichText::new(format!("{}°C", self.spe_temp)).size(11.0).weak());

                    // Warning/alarm indicator
                    if self.spe_alarm != b'N' && self.spe_alarm != 0 {
                        ui.colored_label(Color32::from_rgb(255, 80, 80), RichText::new("ALM").size(11.0).strong());
                    } else if self.spe_warning != b'N' && self.spe_warning != 0 {
                        ui.colored_label(Color32::from_rgb(255, 170, 40), RichText::new("WRN").size(11.0).strong());
                    }
                }

                // RF2K-S compact status (only when active PA)
                if self.rf2k_connected && self.rf2k_active {
                    ui.separator();
                    if self.rf2k_error_state != 0 {
                        // Error: red reset button + error text
                        let err = if self.rf2k_error_text.is_empty() {
                            format!("ERR {}", self.rf2k_error_state)
                        } else {
                            self.rf2k_error_text.clone()
                        };
                        let reset_btn = egui::Button::new(RichText::new(rust_i18n::t!("main_rf2ks_reset").to_string()).size(11.0).strong().color(Color32::WHITE))
                            .fill(Color32::from_rgb(200, 40, 40))
                            .min_size(Vec2::new(80.0, 20.0));
                        if ui.add(reset_btn).clicked() {
                            let _ = self.cmd_tx.send(Command::Rf2kErrorReset);
                        }
                        ui.colored_label(Color32::from_rgb(255, 80, 80), RichText::new(err).size(11.0).strong());
                    } else {
                        // Normal: Operate/Standby toggle + telemetry
                        let (btn_text, btn_color) = if self.rf2k_operate {
                            ("RF2K-S OPR", Color32::from_rgb(0, 150, 0))
                        } else {
                            ("RF2K-S STBY", Color32::from_rgb(255, 170, 40))
                        };
                        let rf2k_btn = egui::Button::new(RichText::new(btn_text).size(11.0).strong().color(Color32::WHITE))
                            .fill(btn_color)
                            .min_size(Vec2::new(80.0, 20.0));
                        if ui.add(rf2k_btn).clicked() {
                            let _ = self.cmd_tx.send(Command::Rf2kOperate(!self.rf2k_operate));
                        }

                        if self.rf2k_forward_w > 0 {
                            ui.label(RichText::new(format!("{}W", self.rf2k_forward_w)).size(12.0));
                            let swr = self.rf2k_swr_x100 as f32 / 100.0;
                            if swr > 1.0 {
                                let swr_color = if swr > 3.0 { Color32::from_rgb(255, 80, 80) }
                                    else if swr > 2.0 { Color32::from_rgb(255, 170, 40) }
                                    else { ui.visuals().text_color() };
                                ui.colored_label(swr_color, RichText::new(format!("{:.1}", swr)).size(12.0));
                            }
                        }
                        let temp = self.rf2k_temperature_x10 as f32 / 10.0;
                        ui.label(RichText::new(format!("{:.0}°C", temp)).size(11.0).weak());
                    }
                }

                if self.ptt_denied {
                    ui.colored_label(Color32::from_rgb(255, 165, 0), rust_i18n::t!("main_ptt_blocked").to_string());
                }
            });
        });

        // Thetis volume/RX2 controls + Connect row (between PTT bar and tabs).
        egui::TopBottomPanel::top("vol_rx2_panel").show(ctx, |ui| {
          ui.vertical(|ui| {
            // Row 1: VFO A-volume. The channel chips are on row 2 (RX1 left, under VFO A),
            // so this row stays narrow and the window can be made smaller.
            ui.horizontal(|ui| {
                // The master gain, and only ever that. It used to change identity
                // into the VFO A volume depending on which windows were open, so
                // dragging it to 50% meant something different per layout - with
                // one word next to it as the only clue. Every channel now carries
                // its own volume in its own place (RX1 below, the rest in their
                // channel window); this one lies over all of them.
                let channels = self.audio_channel_count();
                if channels > 0 {
                    // "Master" only says something when there is more than one
                    // channel to be master over; on a single-channel setup it is
                    // simply the volume.
                    let label = if channels > 1 {
                        rust_i18n::t!("main_master_label").to_string()
                    } else {
                        rust_i18n::t!("main_volume_label").to_string()
                    };
                    ui.label(label);
                    let slider = egui::Slider::new(&mut self.local_volume, 0.001..=1.0)
                        .logarithmic(true)
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
                    let resp = ui.add(slider)
                        .on_hover_text(rust_i18n::t!("main_hover_master_volume").to_string());
                    let scrolled = helpers::slider_wheel(ui, &resp, &mut self.local_volume, 0.001..=1.0, 0.02);
                    if resp.changed() || scrolled {
                        let _ = self.cmd_tx.send(Command::SetLocalVolume(self.local_volume));
                        // Save on release, not per frame: egui reports "changed" on every
                        // frame of a drag even when the value stands still, which wrote the
                        // config to disk dozens of times a second. Same reason the window
                        // geometry above uses a dirty flag.
                        self.master_volume_dirty = true;
                    }
                    if self.master_volume_dirty && !ui.ctx().input(|i| i.pointer.any_down()) {
                        self.save_full_config();
                        self.master_volume_dirty = false;
                    }
                }
            }); // end row 1

            // Row 2: channel chips - RX1 starts left, under VFO A. Yaesu + Connect follow.
            ui.horizontal(|ui| {
                if self.thetis_configured {
                    // Uniform capability rows (parity by construction, model §1b):
                    // each RX channel = [audio-checkbox] [spectrum-toggle] via the same helper.
                    // RX1: audio = rx1_enabled, spectrum = spectrum_enabled (inline/pop-out).
                    let (rx1_audio_click, rx1_spec_click) = Self::channel_sub_chips(
                        ui, "RX1", self.rx1_enabled, self.spectrum_enabled,
                        &rust_i18n::t!("main_hover_chip_audio", name = "RX1").to_string(),
                        &rust_i18n::t!("main_hover_chip_window", name = "RX1").to_string());
                    if rx1_audio_click { self.toggle_rx1_audio(); }
                    if rx1_spec_click {
                        self.spectrum_enabled = !self.spectrum_enabled;
                        let _ = self.cmd_tx.send(Command::EnableSpectrum(self.spectrum_enabled));
                        self.save_full_config();
                    }

                    ui.separator();

                    // RX2 + VRX2 only if the radio has a second receiver
                    // (rx2_present; server on single-receiver = gone). VRX2 hangs off
                    // RX2, so follows the same gate; VRX1 (on RX1) always stays.
                    if self.rx2_present {
                    // RX2: audio = rx2_enabled, spectrum = rx2_spectrum_enabled (pop-out).
                    let (rx2_audio_click, rx2_spec_click) = Self::channel_sub_chips(
                        ui, "RX2", self.rx2_enabled, self.rx2_spectrum_enabled,
                        &rust_i18n::t!("main_hover_chip_audio", name = "RX2").to_string(),
                        &rust_i18n::t!("main_hover_chip_window", name = "RX2").to_string());
                    if rx2_audio_click { self.toggle_rx2_audio(); }
                    if rx2_spec_click {
                        self.rx2_spectrum_enabled = !self.rx2_spectrum_enabled;
                        let _ = self.cmd_tx.send(Command::EnableRx2Spectrum(self.rx2_spectrum_enabled));
                        // Window derives from want (rx2_spectrum_enabled) && can_rx2 (model B);
                        // no imperative popout sync here.
                        if self.rx2_spectrum_enabled {
                            self.rx2_last_sent_zoom = 0.0;
                            self.rx2_last_sent_pan = 0.0;
                            self.rx2_zoom_pan_changed_at = Some(Instant::now());
                        }
                        self.save_full_config();
                    }

                    ui.separator();
                    } // end rx2_present gate for RX2 (VRX2 follows below)

                    // VRX1/VRX2: same uniform [audio][spec] chips as RX1/RX2,
                    // now also on the main screen — audio checkbox = channel on, spectrum =
                    // high-res. This way the VRX state is visible + controllable without
                    // opening the pop-out (previously: audio could play without a visible
                    // state on the main screen). Shared toggle methods.
                    let (vrx1_audio_click, vrx1_spec_click) =
                        Self::channel_sub_chips(
                            ui, "VRX1", self.vrx1_enabled, self.vrx1_high_res_spectrum,
                            &rust_i18n::t!("main_hover_chip_audio", name = "VRX1").to_string(),
                            &rust_i18n::t!("main_hover_chip_window", name = "VRX1").to_string());
                    if vrx1_audio_click { self.toggle_vrx_audio(VrxChannel::Vrx1); }
                    if vrx1_spec_click { self.toggle_vrx_spectrum(VrxChannel::Vrx1); }

                    ui.separator();

                    if self.rx2_present {
                        let (vrx2_audio_click, vrx2_spec_click) =
                            Self::channel_sub_chips(
                            ui, "VRX2", self.vrx2_enabled, self.vrx2_high_res_spectrum,
                            &rust_i18n::t!("main_hover_chip_audio", name = "VRX2").to_string(),
                            &rust_i18n::t!("main_hover_chip_window", name = "VRX2").to_string());
                        if vrx2_audio_click { self.toggle_vrx_audio(VrxChannel::Vrx2); }
                        if vrx2_spec_click { self.toggle_vrx_spectrum(VrxChannel::Vrx2); }

                        ui.separator();
                    }
                } else {
                    // RX2 + VRX popouts are derived (model B); no imperative force-close.
                }

                // Yaesu 1/2 on the main screen - only if that radio is present. Gated on
                // the OPTIMISTIC presence (yaesu_present_last): a radio present last session
                // shows at once pre-connect, like RX/VRX; the server prunes present_last on
                // connect if it is (no longer) there (see sync_state). [type]=audio-enable,
                // [win]=control window. This way you no longer need to go to devices->Yaesu.
                // Independent of thetis_configured: works without Thetis too. "0 or 1 Yaesu"
                // follows automatically from the presence gate.
                if self.yaesu_present_last {
                    let full = self.yaesu_slot_label(0);
                    let short = self.yaesu_short_label(0);
                    let (en_click, win_click) = Self::channel_sub_chips(
                        ui, &short, self.yaesu_enabled, self.yaesu_popout,
                        &rust_i18n::t!("main_hover_yaesu_audio", label = &full).to_string(),
                        &rust_i18n::t!("main_hover_yaesu_window", label = &full).to_string());
                    if en_click { self.toggle_yaesu_audio(0); }
                    if win_click {
                        // Window open/closed, independent of the audio.
                        self.yaesu_popout = !self.yaesu_popout;
                        self.save_full_config();
                    }
                    ui.separator();
                }
                if self.yaesu2_present_last {
                    let full = self.yaesu_slot_label(1);
                    let short = self.yaesu_short_label(1);
                    let (en_click, win_click) = Self::channel_sub_chips(
                        ui, &short, self.yaesu2_enabled, self.yaesu2_popout,
                        &rust_i18n::t!("main_hover_yaesu_audio", label = &full).to_string(),
                        &rust_i18n::t!("main_hover_yaesu_window", label = &full).to_string());
                    if en_click { self.toggle_yaesu_audio(1); }
                    if win_click {
                        self.yaesu2_popout = !self.yaesu2_popout;
                        self.save_ptt_config();
                    }
                    ui.separator();
                }

                // Connect/Disconnect button + status
                if self.connected {
                    if ui.button(rust_i18n::t!("main_disconnect").to_string()).clicked() {
                        let _ = self.cmd_tx.send(Command::Disconnect);
                        self.connected = false;
                        self.catsync.force_unmute();
                    }
                    ui.colored_label(Color32::GREEN, rust_i18n::t!("main_connected").to_string());
                } else {
                    // PATCH-1 smoke-test fix (2026-05-12): when we are mid-auth in
                    // AwaitingTotp, Connect must not be clickable - the user should
                    // use the Verify-button on the 2FA input. A second Connect-press
                    // would otherwise pull the engine back to "Connecting..." while
                    // the server still has an active PendingTotp session, which
                    // would never recover.
                    let in_awaiting_totp = matches!(
                        self.state_rx.borrow().connect_status,
                        sdr_remote_logic::state::ConnectStatus::AwaitingTotp
                    );
                    let in_connecting = matches!(
                        self.state_rx.borrow().connect_status,
                        sdr_remote_logic::state::ConnectStatus::Connecting
                    );
                    let can_connect = !self.password_input.is_empty()
                        && !in_awaiting_totp
                        && !in_connecting;
                    if ui.add_enabled(can_connect, egui::Button::new(rust_i18n::t!("main_connect").to_string())).clicked() {
                        // Reset span to 0 so first spectrum packet triggers zoom calculation.
                        // The bins go with the span. They are the twin of that
                        // latch: kept, they are a picture of the band the last
                        // session was on, and the code that decides what to draw
                        // asks whether they are empty (2026-08-15).
                        self.reset_view_for_new_session();
                        let pw = if self.password_input.is_empty() { None } else { Some(self.password_input.clone()) };
                        // In relay mode there is no server IP: send a placeholder label
                        // (the Relay transport ignores the address; the engine skips the DNS check).
                        let connect_addr = if self.relay_external {
                            format!("relay:{}", self.relay_station)
                        } else {
                            self.server_input.clone()
                        };
                        let _ = self.cmd_tx.send(Command::Connect(connect_addr, pw));
                        self.save_full_config();
                    }
                    ui.colored_label(Color32::RED, rust_i18n::t!("main_disconnected").to_string());
                }
                if self.audio_error {
                    ui.colored_label(Color32::from_rgb(255, 165, 0), rust_i18n::t!("main_audio_error").to_string());
                }
                // PATCH-1 smoke-test fix (2026-05-13): auto-switch to the
                // Server tab when connect_status transitions into a state
                // that demands user attention (AwaitingTotp, Failed). Without
                // this the user can be on the Radio tab and never see the
                // 2FA prompt or error message until they manually navigate.
                {
                    use sdr_remote_logic::state::ConnectStatus;
                    let current = self.state_rx.borrow().connect_status.clone();
                    let needs_attention = matches!(
                        current,
                        ConnectStatus::AwaitingTotp | ConnectStatus::Failed(_)
                    );
                    let was_attention = matches!(
                        self.last_connect_status,
                        ConnectStatus::AwaitingTotp | ConnectStatus::Failed(_)
                    );
                    if needs_attention && !was_attention {
                        self.active_tab = Tab::Server;
                    }
                    self.last_connect_status = current;
                }
                // PATCH-1 smoke-test fix (2026-05-12 #2): show connect-status
                // error globally in the top bar so users on any tab see it,
                // not just on the Server tab. Avoids the "press Connect on
                // Radio tab -> silent failure" UX gap.
                {
                    use sdr_remote_logic::i18n::{connect_status_text, Lang};
                    use sdr_remote_logic::state::ConnectStatus;
                    let connect_status = self.state_rx.borrow().connect_status.clone();
                    let lang = Lang::from_code(&self.ui_language);
                    // PATCH-1 smoke-test fix (2026-05-13): top-bar headline 16pt bold
                    // so it stands out - operator feedback: was smaller than other UI.
                    match &connect_status {
                        ConnectStatus::Failed(_) => {
                            let (headline, _) = connect_status_text(&connect_status, lang, sdr_remote_logic::i18n::Platform::Desktop);
                            ui.label(egui::RichText::new(headline).size(16.0).strong().color(Color32::from_rgb(220, 40, 40)));
                        }
                        ConnectStatus::AwaitingTotp => {
                            let (headline, _) = connect_status_text(&connect_status, lang, sdr_remote_logic::i18n::Platform::Desktop);
                            ui.label(egui::RichText::new(headline).size(16.0).strong().color(Color32::from_rgb(255, 165, 0)));
                        }
                        ConnectStatus::Connecting => {
                            let (headline, _) = connect_status_text(&connect_status, lang, sdr_remote_logic::i18n::Platform::Desktop);
                            ui.label(egui::RichText::new(headline).size(16.0).strong().color(Color32::from_rgb(180, 180, 60)));
                        }
                        _ => {}
                    }
                }
            });
          }); // end ui.vertical (row 1 + row 2)
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if !self.thetis_configured && matches!(self.active_tab, Tab::Radio | Tab::Thetis) {
                self.active_tab = Tab::Devices;
            }
            ui.horizontal(|ui| {
                if self.thetis_configured {
                    ui.selectable_value(&mut self.active_tab, Tab::Radio, rust_i18n::t!("main_tab_radio").to_string());
                    ui.selectable_value(&mut self.active_tab, Tab::Thetis, "Thetis");
                }
                ui.selectable_value(&mut self.active_tab, Tab::Server, rust_i18n::t!("main_tab_server").to_string());
                ui.selectable_value(&mut self.active_tab, Tab::Devices, rust_i18n::t!("main_tab_devices").to_string());
                ui.selectable_value(&mut self.active_tab, Tab::Midi, "MIDI");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button(RichText::new(rust_i18n::t!("main_about").to_string()).size(11.0)).clicked() {
                        self.show_about = !self.show_about;
                    }
                    ui.toggle_value(&mut self.show_log, RichText::new(rust_i18n::t!("main_log").to_string()).size(11.0));
                    {
                        // "Arrange" button opens the drag grid to organize open windows.
                        let mut s = self.show_layout_arranger;
                        if ui.toggle_value(&mut s, RichText::new(rust_i18n::t!("main_arrange_btn").to_string()).size(11.0))
                            .on_hover_text(rust_i18n::t!("hover_arrange_windows").to_string())
                            .changed()
                        {
                            self.show_layout_arranger = s;
                            if s {
                                // Start on the monitor where the main window is;
                                // render_layout_arranger handles the per-monitor rows.
                                self.layout_target_monitor = self.detect_monitor_index(ctx);
                            }
                        }
                    }
                    // The "VRX" button that toggled both VRX windows at once used to
                    // sit here, next to Arrange. It is gone: every channel now has its
                    // own `venster` button in its own block, so a control that acts on
                    // two channels from a third place is a second way to do the same
                    // thing - and the one that says least about what it will do.
                    // VRX popouts are derived (model B): no imperative force-close.
                });
            });
            ui.separator();

            if self.active_tab == Tab::Devices {
                self.render_devices_screen(ui);
            } else if self.active_tab == Tab::Thetis {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    self.render_thetis_screen(ui);
                });
            } else if self.active_tab == Tab::Server {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    self.render_server_screen(ui);
                });
            } else if self.active_tab == Tab::Midi {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    self.render_midi_screen(ui);
                });
            } else {
            // Wrap the Radio tab content in a ScrollArea so the panel stays
            // usable when the user expands the Diversity panel (or sets a
            // tall spectrum_total_h) and the total content height exceeds
            // the window. Spectrum height itself is fixed at
            // `self.spectrum_total_h` (set via the H: slider / drag-handle
            // below the waterfall) - without the ScrollArea wrapper any
            // overflow would push content off-screen.
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {

            // VFO A frequency
            ui.separator();
            {
                // -- Inline freq display + edit + scroll (via render_frequency_display) --
                // Combines scroll-tuning + inline-edit in one helper.
                let freq_action = self.with_rx_ctx(
                    controls::RxChannel::Rx1,
                    controls::UiDensity::Basic,
                    controls::UiSurface::MainTab,
                    |ctx| {
                        controls::render_frequency_display(ui, ctx).map(|a| match a {
                            controls::FrequencyDisplayAction::Submit { hz } => {
                                let intent = controls::UiIntent::InlineFreqEdit {
                                    channel: controls::RxChannel::Rx1,
                                    hz,
                                };
                                let dispatched = ctx.dispatch(intent, Command::SetFrequency(hz));
                                (hz, dispatched)
                            }
                            controls::FrequencyDisplayAction::ScrollTune { delta_hz } => {
                                let new_freq = (ctx.rx_state.frequency_hz as i64 + delta_hz).max(0) as u64;
                                let intent = controls::UiIntent::TuneByDelta {
                                    channel: controls::RxChannel::Rx1,
                                    delta_hz,
                                };
                                let dispatched = ctx.dispatch(intent, Command::SetFrequency(new_freq));
                                (new_freq, dispatched)
                            }
                        })
                    },
                );
                if let Some((new_freq, true)) = freq_action {
                    self.set_pending_freq_a(new_freq);
                }

                // S-meter / TX power / other TX
                smeter_bar(ui, self.smeter, self.smeter_peak, self.ptt, self.other_tx, self.thetis_swr_x100);

                // RX1's own volume. The core transceiver has to be complete in the
                // main window (UI-STYLE-GUIDE §3.5), so it can be set without
                // opening the RX1 window - and the master above stays master.
                ui.horizontal(|ui| {
                    ui.label("VFO A:");
                    let slider = egui::Slider::new(&mut self.vfo_a_volume, 0.001..=1.0)
                        .logarithmic(true)
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
                    let resp = ui.add(slider)
                        .on_hover_text(rust_i18n::t!("main_hover_set_mix_volume").to_string());
                    let scrolled = helpers::slider_wheel(ui, &resp, &mut self.vfo_a_volume, 0.001..=1.0, 0.02);
                    if resp.changed() || scrolled {
                        let _ = self.cmd_tx.send(Command::SetVfoAVolume(self.vfo_a_volume));
                        self.save_full_config();
                    }
                });

                // -- Frequency step buttons (via controls::render_freq_step_controls) --
                // ± buttons here in Tab::Radio had no connected-guard
                // (raw `ui.button(...)`).
                let step_action = self.with_rx_ctx(
                    controls::RxChannel::Rx1,
                    controls::UiDensity::Basic,
                    controls::UiSurface::MainTab,
                    |ctx| {
                        controls::render_freq_step_controls(ui, ctx).map(|step| {
                            let delta = step.delta_hz(ctx.rx_state.freq_step_index);
                            let new_freq = (ctx.rx_state.frequency_hz as i64 + delta).max(0) as u64;
                            let intent = controls::UiIntent::TuneByDelta {
                                channel: controls::RxChannel::Rx1,
                                delta_hz: delta,
                            };
                            let dispatched = ctx.dispatch(intent, Command::SetFrequency(new_freq));
                            (new_freq, dispatched)
                        })
                    },
                );
                if let Some((new_freq, true)) = step_action {
                    self.set_pending_freq_a(new_freq);
                }

                // Scroll-wheel tuning is in render_frequency_display above;
                // the Basic-density helper gates itself on !spectrum_enabled.
            }

            // RX1 spectrum display. The spectrum TOGGLE (subscription) is now in the
            // uniform capability row at the top (phase 4b); here only the
            // pop-out/pop-in choice (display) + the inline spectrum.
            {
                if self.spectrum_enabled {
                    ui.horizontal(|ui| {
                        let popout_label = if self.spectrum_popout { "Spectrum: Pop-in" } else { "Spectrum: Pop-out" };
                        if ui.button(popout_label).clicked() {
                            self.spectrum_popout = !self.spectrum_popout;
                            // Persist the inline/popout choice (present). Without this the
                            // toggle is lost on restart and RX1 falls back to inline even
                            // though the user last chose popout.
                            self.save_full_config();
                            // RX2 spectrum has its own toggle + pop-out (phase 4).
                        }
                    });
                }

                if self.spectrum_enabled && self.can_rx1() && !self.spectrum_bins.is_empty() && !self.spectrum_popout {
                    self.render_spectrum_content(ui, ctx, 300.0, false);
                }
            }

            // Mode buttons (via controls::render_mode_selector - Basic density = 4 modes)
            // Tab::Radio mode-block previously had `ui.add(btn)` without a
            // connected-guard.
            let ptt_active = self.ptt;
            let mode_action = self.with_rx_ctx(
                controls::RxChannel::Rx1,
                controls::UiDensity::Basic,
                controls::UiSurface::MainTab,
                |ctx| {
                    ui.add_enabled_ui(!ptt_active, |ui| controls::render_mode_selector(ui, ctx)).inner.map(|c| {
                        let intent = controls::UiIntent::SelectMode {
                            channel: controls::RxChannel::Rx1,
                            mode: c.mode,
                        };
                        let dispatched = ctx.dispatch(intent, Command::SetMode(c.mode));
                        (c, dispatched)
                    })
                },
            );
            if let Some((click, true)) = mode_action {
                self.mode = click.mode;
                self.filter_changed_at = None;
                self.tci_control_changed_at = Some(Instant::now());
            }

            // Filter bandwidth control
            {
                let presets = filter_presets_for_mode(self.mode);
                let cw = is_cw_mode(self.mode);
                let current_bw = self.filter_high_hz - self.filter_low_hz;
                let idx = closest_preset_index(presets, current_bw);

                ui.horizontal(|ui| {
                    ui.label(rust_i18n::t!("main_filter_label").to_string());
                    let minus_btn = egui::Button::new(RichText::new(" - ").size(14.0));
                    if ui.add_enabled(idx > 0, minus_btn).on_hover_text(rust_i18n::t!("main_hover_narrow_filter").to_string()).clicked() {
                        let (low, high) = calc_filter_edges(
                            self.mode, self.filter_low_hz, self.filter_high_hz, presets[idx - 1]);
                        let _ = self.cmd_tx.send(Command::SetControl(
                            ControlId::FilterLow, low as i16 as u16));
                        let _ = self.cmd_tx.send(Command::SetControl(
                            ControlId::FilterHigh, high as i16 as u16));
                        self.filter_low_hz = low;
                        self.filter_high_hz = high;
                        self.filter_changed_at = Some(Instant::now());
                    }

                    ui.label(RichText::new(format_bandwidth(presets[idx], cw)).strong().size(14.0));

                    let plus_btn = egui::Button::new(RichText::new(" + ").size(14.0));
                    if ui.add_enabled(idx < presets.len() - 1, plus_btn).on_hover_text(rust_i18n::t!("main_hover_widen_filter").to_string()).clicked() {
                        let (low, high) = calc_filter_edges(
                            self.mode, self.filter_low_hz, self.filter_high_hz, presets[idx + 1]);
                        let _ = self.cmd_tx.send(Command::SetControl(
                            ControlId::FilterLow, low as i16 as u16));
                        let _ = self.cmd_tx.send(Command::SetControl(
                            ControlId::FilterHigh, high as i16 as u16));
                        self.filter_low_hz = low;
                        self.filter_high_hz = high;
                        self.filter_changed_at = Some(Instant::now());
                    }
                });
            }

            // Memory buttons
            ui.horizontal(|ui| {
                for i in 0..NUM_MEMORIES {
                    let label = if let Some(hz) = self.memories[i].frequency_hz {
                        let band = band_label(hz);
                        if band.is_empty() {
                            format!("M{}", i + 1)
                        } else {
                            band.to_string()
                        }
                    } else {
                        format!("M{}", i + 1)
                    };

                    // Highlight: save mode (orange), current band match (blue), default
                    let is_current_band = self.memories[i].frequency_hz
                        .map(|hz| {
                            let mem_band = band_label(hz);
                            let cur_band = band_label(self.frequency_hz);
                            !mem_band.is_empty() && mem_band == cur_band
                        })
                        .unwrap_or(false);

                    let btn = if self.save_mode {
                        egui::Button::new(RichText::new(&label))
                            .fill(Color32::from_rgb(120, 80, 30))
                    } else if is_current_band {
                        egui::Button::new(RichText::new(&label).strong())
                            .fill(Color32::from_rgb(100, 160, 230))
                    } else {
                        egui::Button::new(&label)
                    };

                    if ui.add(btn).clicked() {
                        if self.save_mode {
                            if self.frequency_hz > 0 {
                                self.memories[i] = Memory {
                                    frequency_hz: Some(self.frequency_hz),
                                    mode: Some(self.mode),
                                };
                                self.save_full_config();
                            }
                            self.save_mode = false;
                        } else if let Some(hz) = self.memories[i].frequency_hz {
                            let _ = self.cmd_tx.send(Command::SetFrequency(hz));
                                self.set_pending_freq_a(hz);
                            if let Some(mode) = self.memories[i].mode {
                                let _ = self.cmd_tx.send(Command::SetMode(mode));
                                self.mode = mode;
                                self.filter_changed_at = None;
                            }
                        }
                    }
                }

                let save_btn = if self.save_mode {
                    egui::Button::new(RichText::new(rust_i18n::t!("main_save").to_string()).strong())
                        .fill(Color32::from_rgb(150, 60, 30))
                } else {
                    egui::Button::new(rust_i18n::t!("main_save").to_string())
                };
                if ui.add(save_btn).clicked() {
                    self.save_mode = !self.save_mode;
                }
            });


            ui.horizontal(|ui| {
                // NR cycle: OFF -> NR1 -> NR2 -> NR3 -> NR4 -> OFF
                let nr_label = if self.nr_level == 0 { "NR".to_string() } else { format!("NR{}", self.nr_level) };
                let nr_btn = if self.nr_level > 0 {
                    egui::Button::new(RichText::new(&nr_label).strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else {
                    egui::Button::new(&nr_label)
                };
                if ui.add(nr_btn).on_hover_text(rust_i18n::t!("main_hover_nr").to_string()).clicked() {
                    let new_val = if self.nr_level >= 4 { 0 } else { self.nr_level + 1 };
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::NoiseReduction, new_val as u16));
                    self.nr_level = new_val;
                }

                // ANF toggle
                let anf_btn = if self.anf_on {
                    egui::Button::new(RichText::new("ANF").strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else {
                    egui::Button::new("ANF")
                };
                if ui.add(anf_btn).on_hover_text(rust_i18n::t!("main_hover_anf").to_string()).clicked() {
                    let new_val = !self.anf_on;
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::AutoNotchFilter, new_val as u16));
                    self.anf_on = new_val;
                }

                // Mic AGC toggle
                let agc_btn = if self.agc_enabled {
                    egui::Button::new(RichText::new("Mic AGC").strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else {
                    egui::Button::new("Mic AGC")
                };
                if ui.add(agc_btn).on_hover_text(rust_i18n::t!("main_hover_mic_agc").to_string()).clicked() {
                    let new_val = !self.agc_enabled;
                    let _ = self.cmd_tx.send(Command::SetAgcEnabled(new_val));
                    self.agc_enabled = new_val;
                    self.save_full_config();
                }

                ui.separator();

                // Drive level slider (inline)
                ui.label(rust_i18n::t!("main_drive_label").to_string());
                let mut drive_f32 = self.drive_level as f32;
                let slider = egui::Slider::new(&mut drive_f32, 0.0..=100.0)
                    .custom_formatter(|v, _| format!("{:.0}%", v));
                let resp = ui.add(slider);
                let scrolled = helpers::slider_wheel(ui, &resp, &mut drive_f32, 0.0..=100.0, 2.0);
                if resp.changed() || scrolled {
                    let new_val = drive_f32.round() as u8;
                    if new_val != self.drive_level {
                        let _ = self.cmd_tx.send(Command::SetControl(ControlId::DriveLevel, new_val as u16));
                        self.drive_level = new_val;
                    }
                }
            });

            // Diversity: two receivers phased into one. On a radio with a single
            // receiver there is nothing to combine, so the section goes rather
            // than sit there doing nothing - the same signal that already hides
            // RX2 and VRX2 everywhere (the server's SINGLE_RECEIVER flag).
            if self.rx2_present {
                ui.separator();
                if helpers::chevron_label(
                    ui,
                    self.collapse_diversity,
                    RichText::new(rust_i18n::t!("main_diversity").to_string()).strong().size(14.0),
                )
                .clicked()
                {
                    self.collapse_diversity = !self.collapse_diversity;
                    self.save_full_config();
                }
                if self.collapse_diversity {
                    ui.indent("diversity_body", |ui| {
                        self.render_diversity(ui);
                    });
                }
            }

            }); // end of Radio-tab ScrollArea
            } // end of Radio tab
        });

        // Pop-out viewports: joined or separate. Both gates intentionally
        // omit a bins/connected check so the popout opens at its saved
        // geometry from the very first frame - same UX as the Yaesu popout.
        // The popout's content gates internally on bin-availability and
        // shows a "Waiting for ..." placeholder pre-connect.
        // Model B (Phase 1, RX2): the RX2 window is DERIVED - want (rx2_spectrum_enabled)
        // AND can_rx2 (Thetis DDC + a second receiver present). Replaces the imperative
        // toggle sync (was `rx2_popout = rx2_spectrum_enabled`) and the force-close gate.
        self.rx2_popout = self.rx2_spectrum_enabled && self.can_rx2();
        // Model B (Phase 1, RX1): shown(RX1) = want (spectrum_enabled) && can_rx1
        // (thetis_configured); `spectrum_popout` is the *present* choice (popout vs
        // inline), not the open flag. Gating on can_rx1 also fixes the latent
        // empty-RX1-window case in a Yaesu-only setup.
        let show_rx1_popout = self.spectrum_popout && self.spectrum_enabled && self.can_rx1();
        let show_rx2_popout = self.rx2_popout && self.rx2_spectrum_enabled;
        let joined_active = show_rx1_popout && show_rx2_popout && self.popout_joined;

        // Reset init_applied when a window is NOT shown as its own viewport -
        // either closed, OR absorbed into the joined window - so reopening it
        // (after a snap, or when leaving joined mode) re-applies the saved
        // position/size, like VRX/Yaesu. Without the `|| joined_active` guard the
        // separate RX1/RX2 windows kept init_applied=true through joined mode and
        // reopened at the default/joined position after a join -> split toggle.
        if !show_rx1_popout || joined_active { self.spectrum_popout_init_applied = false; }
        if !show_rx2_popout || joined_active { self.rx2_popout_init_applied = false; }
        // Symmetric: reset the joined flag whenever the joined window is not shown,
        // so a split -> join toggle re-applies the saved joined geometry too.
        if !joined_active { self.popout_joined_init_applied = false; }

        if joined_active {
            // Joined mode: single combined window with RX1 on top, RX2 below.
            // Shared show_popout lifecycle; reuses the RX1-solo ViewportId.
            self.render_joined_popout(ctx);
        } else {
            // Separate mode: individual windows
            if show_rx1_popout {
                self.render_rx1_popout(ctx);
            }

            if show_rx2_popout {
                self.render_rx2_popout(ctx);
            }
        }

        // Once the window is closed (via X, win-chip or enable-off): reset
        // init_applied so on reopen it re-applies the saved position/size
        // (otherwise egui opens at the last session spot).
        // Reset init when the window is NOT shown - either closed (want off) OR the
        // radio is gone (presence off). Without the presence term the flag stayed true
        // through a disconnect (want preserved), so on reconnect apply_popout_geometry
        // skipped the saved pos/size and the window reopened at the default spot.
        if !(self.yaesu_popout && self.yaesu_present_last) { self.yaesu_popout_init_applied = false; }
        // Yaesu popout window. Gated on optimistic PRESENCE (yaesu_present_last) + want
        // (yaesu_popout) - present_last, not yaesu_connected, so the window is optimistic
        // pre-connect like the chips/RX/VRX; the server prunes present_last on connect.
        // exactly like the main-screen chip that opens/closes it. The audio-enable flag
        // (yaesu_enabled) is the mute toggle and is SEPARATE from the window (audio off,
        // window open is valid) - it must NOT gate the window: keeping it in the gate left
        // the pop-out hanging when the radio disappeared (presence false) while audio was
        // still enabled, with no main-screen chip left to close it. Yaesu is not a spectrum
        // source (model B), so the open flag + presence gate stay here; the shared
        // show_popout helper owns only the geometry/focus/close/save lifecycle.
        if self.yaesu_popout && self.yaesu_present_last {
            self.render_yaesu1_popout(ctx);
        }

        // Slot-1 (FTX-1) own popout window - separate from the 991A window, routed
        // to slot 1. Same presence-gated model as slot 0.
        if !(self.yaesu2_popout && self.yaesu2_present_last) { self.yaesu2_popout_init_applied = false; }
        if self.yaesu2_popout && self.yaesu2_present_last {
            self.render_yaesu2_popout(ctx);
        }

        // Handle spectrum interaction keys (fallback for main-window spectrum;
        // popout viewports handle their own keys inside the viewport closure)
        self.handle_rx2_spectrum_keys(ctx);
        self.handle_rx1_spectrum_keys(ctx);

        // VRX DDC-bucket tracking: detect ANY DDC center change of
        // ≥100 kHz (= bucket size), not just amateur-band boundaries.
        // CTUN re-centering within the same band still trips the
        // detector. When the bucket changes, save the previous freq
        // under the OLD bucket and restore the LAST-USED freq for the
        // new bucket. Defaults to VFO if no memory or restored value
        // falls outside the new DDC range.
        const BUCKET_HZ: u64 = 100_000;
        {
            let vrx1_center = if self.full_spectrum_center_hz > 0 {
                self.full_spectrum_center_hz as u64
            } else { self.frequency_hz };
            let vrx1_span = if self.full_spectrum_span_hz > 0 {
                self.full_spectrum_span_hz as u64
            } else { 384_000 };
            if vrx1_center > 0 {
                let cur_bucket = vrx1_center / BUCKET_HZ;
                let last_bucket = self.last_vrx1_ddc_center_hz / BUCKET_HZ;
                if cur_bucket != last_bucket && self.last_vrx1_ddc_center_hz > 0 {
                    if self.vrx1_freq_hz > 0 {
                        self.vrx1_freq_by_bucket.insert(last_bucket, self.vrx1_freq_hz);
                    }
                    let new_min = vrx1_center.saturating_sub(vrx1_span / 2);
                    let new_max = vrx1_center + vrx1_span / 2;
                    let restored = self.vrx1_freq_by_bucket.get(&cur_bucket).copied()
                        .filter(|&f| f >= new_min && f <= new_max)
                        .unwrap_or(self.frequency_hz);
                    // Commit the restored freq only after a confirmed send (no drift).
                    if self.cmd_tx.send(Command::SetVrxFrequency(restored)).is_ok() {
                        self.vrx1_freq_hz = restored;
                    }
                }
                self.last_vrx1_ddc_center_hz = vrx1_center;
            }
        }
        {
            let vrx2_center = if self.rx2_full_spectrum_center_hz > 0 {
                self.rx2_full_spectrum_center_hz as u64
            } else { self.rx2_frequency_hz };
            let vrx2_span = if self.rx2_full_spectrum_span_hz > 0 {
                self.rx2_full_spectrum_span_hz as u64
            } else { 384_000 };
            if vrx2_center > 0 {
                let cur_bucket = vrx2_center / BUCKET_HZ;
                let last_bucket = self.last_vrx2_ddc_center_hz / BUCKET_HZ;
                if cur_bucket != last_bucket && self.last_vrx2_ddc_center_hz > 0 {
                    if self.vrx2_freq_hz > 0 {
                        self.vrx2_freq_by_bucket.insert(last_bucket, self.vrx2_freq_hz);
                    }
                    let new_min = vrx2_center.saturating_sub(vrx2_span / 2);
                    let new_max = vrx2_center + vrx2_span / 2;
                    let restored = self.vrx2_freq_by_bucket.get(&cur_bucket).copied()
                        .filter(|&f| f >= new_min && f <= new_max)
                        .unwrap_or(self.rx2_frequency_hz);
                    // Commit the restored freq only after a confirmed send (no drift).
                    if self.cmd_tx.send(Command::SetVrx2Frequency(restored)).is_ok() {
                        self.vrx2_freq_hz = restored;
                    }
                }
                self.last_vrx2_ddc_center_hz = vrx2_center;
            }
        }

        // Detached VRX windows: VRX1 (on RX1+VFO-A) and VRX2 (on RX2+VFO-B) each in
        // their own viewport, independently placeable. Closing (X/button/spec-chip-off)
        // resets init_applied so the saved position is re-applied on reopen.
        //
        // Model B (Phase 1, VRX): the window is DERIVED, not stored - want (the
        // high-res spectrum the user asked for) AND can_vrx (Thetis DDC available).
        // This single line replaces the hardcoded-false init, the imperative toggle
        // sync, and the two `!thetis_configured` force-close branches: the window
        // opens on frame 1 iff want && can, and closes when either drops.
        let vrx_can = self.can_vrx();
        self.vrx1_popout = self.vrx1_high_res_spectrum && vrx_can;
        self.vrx2_popout = self.vrx2_high_res_spectrum && vrx_can;
        if !self.vrx1_popout { self.vrx_popout_init_applied = false; }
        if !self.vrx2_popout { self.vrx2_popout_init_applied = false; }
        if self.vrx1_popout { self.render_vrx_popout(ctx, VrxChannel::Vrx1); }
        if self.vrx2_popout { self.render_vrx_popout(ctx, VrxChannel::Vrx2); }

        // "Arrange windows" drag grid (normal egui window in the main viewport).
        self.render_layout_arranger(ctx);

        // About window
        if self.show_about {
            egui::Window::new(rust_i18n::t!("main_about_title").to_string())
                .collapsible(false)
                .resizable(true)
                .default_size([420.0, 500.0])
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new("ThetisLink").size(22.0).strong());
                            ui.label(RichText::new(format!("v{}", sdr_remote_core::version_string())).size(14.0));
                            ui.add_space(4.0);
                            ui.label(rust_i18n::t!("main_about_tagline").to_string());
                        });
                        ui.add_space(8.0);
                        ui.separator();

                        ui.label(RichText::new(rust_i18n::t!("main_about_author").to_string()).size(13.0).strong());
                        ui.label("Chiron van der Burgt - PA3GHM");

                        ui.add_space(6.0);
                        ui.label(RichText::new(rust_i18n::t!("main_about_special_thanks").to_string()).size(13.0).strong());
                        ui.label("Richie (ramdor) - Thetis SDR development, TCI protocol extensions");

                        ui.add_space(6.0);
                        ui.label(RichText::new(rust_i18n::t!("main_about_protocols").to_string()).size(13.0).strong());
                        ui.label("TCI - Expert Electronics / Thetis");
                        ui.label("DX Spider - DX cluster telnet protocol");
                        ui.label("HPSDR / OpenHPSDR Protocol 2");
                        ui.label("WebSDR (PA3FWM) / KiwiSDR - CatSync targets");
                        ui.label("ThetisLink Relay - self-hosted WebSocket + UDP relay (internet remote)");
                        ui.label("ThetisLink Chat - optional service beside a relay (chat + problem reports)");

                        ui.add_space(6.0);
                        ui.label(RichText::new(rust_i18n::t!("main_about_hardware").to_string()).size(13.0).strong());
                        egui::Grid::new("hw_grid").num_columns(2).spacing([12.0, 2.0]).show(ui, |ui| {
                            for (dev, iface) in [
                                ("ANAN 7000DLE", "TCI (via Thetis)"),
                                ("Yaesu FT-991A", "Serial CAT + USB Audio"),
                                ("Yaesu FTX-1", "Serial CAT + USB Audio"),
                                ("RF2K-S PA", "HTTP API"),
                                ("SPE Expert 1.3K-FA", "Serial"),
                                ("StockCorner JC-4s / JC-3s Tuner", "MCP2221A USB-HID"),
                                ("UltraBeam RCU-06", "Serial"),
                                ("Amplitec 6/2", "Serial"),
                                ("EA7HG Visual Rotor", "UDP"),
                                ("Yaesu G-1000DXC Rotor", "MCP2221A USB-HID"),
                                ("PstRotator (any supported rotor)", "XML over UDP"),
                            ] {
                                ui.label(dev);
                                ui.label(RichText::new(iface).color(Color32::GRAY));
                                ui.end_row();
                            }
                        });

                        ui.add_space(6.0);
                        ui.label(RichText::new(rust_i18n::t!("main_about_libraries").to_string()).size(13.0).strong());
                        let libs = [
                            ("tokio", "Async runtime"),
                            ("eframe / egui", "Desktop GUI"),
                            ("cpal", "Audio I/O"),
                            ("audiopus", "Opus codec"),
                            ("rubato", "Resampling"),
                            ("rustfft", "FFT spectrum"),
                            ("ringbuf", "Lock-free buffers"),
                            ("tokio-tungstenite", "TCI WebSocket"),
                            ("serialport", "Yaesu CAT"),
                            ("mcp2221-hal", "MCP2221A USB-HID (tuners, rotor)"),
                            ("rustls", "Relay TLS (wss)"),
                            ("midir", "MIDI controller"),
                            ("wry", "WebView (CatSync)"),
                        ];
                        egui::Grid::new("lib_grid").num_columns(2).spacing([12.0, 1.0]).show(ui, |ui| {
                            for (lib, purpose) in libs {
                                ui.label(RichText::new(lib).size(11.0));
                                ui.label(RichText::new(purpose).size(11.0).color(Color32::GRAY));
                                ui.end_row();
                            }
                        });

                        ui.add_space(6.0);
                        ui.label(RichText::new(rust_i18n::t!("main_about_license").to_string()).size(13.0).strong());
                        ui.label("GPL-2.0-or-later (see LICENSE)");
                        ui.label("Copyright © 2025-2026 Chiron van der Burgt");
                        ui.horizontal(|ui| {
                            ui.label(rust_i18n::t!("main_source_label").to_string());
                            ui.hyperlink("https://github.com/cjenschede/ThetisLink");
                        });
                        ui.label("Based on the Thetis SDR lineage - see ATTRIBUTION.md");
                        ui.label("Third-party licenses & SBOM: see NOTICE.md, THIRD-PARTY-LICENSES.html");

                        ui.add_space(12.0);
                        ui.vertical_centered(|ui| {
                            if ui.button(rust_i18n::t!("main_close").to_string()).clicked() {
                                self.show_about = false;
                            }
                        });
                    });
                });
        }

        // Log panel (collapsible, bottom of window)
        egui::TopBottomPanel::bottom("log_panel").show_animated(ctx, self.show_log, |ui| {
            ui.set_max_height(150.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(rust_i18n::t!("main_log").to_string()).strong().size(11.0));
                if ui.small_button(rust_i18n::t!("main_clear").to_string()).clicked() {
                    if let Ok(mut buf) = self.log_buffer.lock() {
                        buf.clear();
                    }
                }
            });
            ui.separator();
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .max_height(120.0)
                .show(ui, |ui| {
                    if let Ok(buf) = self.log_buffer.lock() {
                        for line in buf.iter() {
                            ui.label(RichText::new(line).monospace().size(9.0).color(Color32::from_rgb(180, 180, 180)));
                        }
                    }
                });
        });

        // Adaptive repaint rate: 30fps when active, 2fps when idle
        let needs_fast_repaint = self.connected
            || self.spectrum_popout
            || self.rx2_popout;
        let repaint_ms = if needs_fast_repaint { 33 } else { 500 };
        ctx.request_repaint_after(std::time::Duration::from_millis(repaint_ms));
    }
}
