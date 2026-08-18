// SPDX-License-Identifier: GPL-2.0-or-later
//! RX1/RX2 control + spectrum rendering: the per-channel control rows (VFO, S-meter,
//! band, mode, step, filter, NR/ANF), the RX1/RX2 pop-out content, the split-aware
//! RX2 renderer, the spectrum keyboard handlers and the RX2 content/spectrum-only
//! panels. Extracted verbatim from `ui/mod.rs` - pure relocation, no behaviour change.
//! `pub(super)` keeps them callable from the parent module tree (update/popouts/screens).

use super::*;

impl SdrRemoteApp {
    /// Render RX1 controls only (VFO, S-meter, band, mode, freq step, filter, NR, ANF).
    /// `surface` determines which UI surface this is (MainTab, PopoutSeparate,
    /// PopoutJoined) - passed to the controls helpers for coverage and events.
    pub(super) fn render_rx1_controls(&mut self, ui: &mut egui::Ui, surface: controls::UiSurface) {
        if self.meter_analog[M_RX1] {
            let total_w = ui.available_width();
            let start = ui.cursor().min;

            // First pass: measure controls natural height at full width
            let measure_rect = egui::Rect::from_min_size(start, egui::vec2(total_w, 500.0));
            let mut measure = ui.new_child(egui::UiBuilder::new().max_rect(measure_rect).layout(egui::Layout::top_down(egui::Align::LEFT)));
            self.render_rx1_controls_inner(&mut measure, surface);
            let controls_h = measure.min_rect().height();

            // Meter width: max 2x height, and leave at least 480px for controls
            let meter_w = (controls_h * SMETER_VIS_ASPECT).min(total_w - 480.0).max(0.0);
            let controls_w = total_w - meter_w - if meter_w > 0.0 { 8.0 } else { 0.0 };

            // Left: actual controls render
            let controls_rect = egui::Rect::from_min_size(start, egui::vec2(controls_w, 500.0));
            let mut left = ui.new_child(egui::UiBuilder::new().max_rect(controls_rect).layout(egui::Layout::top_down(egui::Align::LEFT)));
            self.render_rx1_controls_inner(&mut left, surface);

            // Right: analog meter (only if there's room)
            if meter_w > 80.0 {
                let meter_pos = egui::pos2(start.x + controls_w + 4.0, start.y);
                let meter_rect = egui::Rect::from_min_size(meter_pos, egui::vec2(meter_w, controls_h));
                let mut right = ui.new_child(egui::UiBuilder::new().max_rect(meter_rect).layout(egui::Layout::top_down(egui::Align::LEFT)));
                self.popout_rx1_smeter_rect = smeter_analog_sized(&mut right, self.smeter, self.smeter_peak, self.ptt, self.other_tx, Some((meter_w, controls_h)));
            }

            ui.advance_cursor_after_rect(egui::Rect::from_min_size(start, egui::vec2(total_w, controls_h)));
        } else {
            self.render_rx1_controls_inner(ui, surface);
        }
        // Clicking the RX1 s-meter toggles analog <-> bar.
        let mrect = self.popout_rx1_smeter_rect;
        self.meter_click(ui, mrect, M_RX1);
    }

    pub(super) fn render_rx1_controls_inner(&mut self, ui: &mut egui::Ui, surface: controls::UiSurface) {
        let amber = Color32::from_rgb(255, 170, 40);

        // -- Top bar: frequency + mode (via controls::render_frequency_display) --
        ui.horizontal(|ui| {
            let action = self.with_rx_ctx(
                controls::RxChannel::Rx1,
                controls::UiDensity::Extended,
                surface,
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
            if let Some((new_freq, true)) = action {
                self.set_pending_freq_a(new_freq);
            }

            let mode_label = match self.mode {
                0 => "LSB", 1 => "USB", 2 => "DSB", 3 => "CW-L", 4 => "CW-U",
                5 => "FM", 6 => "AM", 7 => "DIGU", 8 => "SPEC", 9 => "DIGL",
                10 => "SAM", 11 => "DRM", _ => "?",
            };
            ui.label(RichText::new(mode_label).size(16.0).color(amber));

            let bw = self.filter_high_hz - self.filter_low_hz;
            let bw_text = if bw >= 1000 {
                format!("{:.1}k", bw as f32 / 1000.0)
            } else {
                format!("{} Hz", bw)
            };
            ui.label(RichText::new(bw_text).size(12.0).weak());
        });

        // S-meter bar (only in bar mode)
        if !self.meter_analog[M_RX1] {
            self.popout_rx1_smeter_rect = smeter_bar_popout(ui, self.smeter, self.smeter_peak, self.ptt, self.other_tx, self.thetis_swr_x100);
        }

        // -- Controls row: audio on/off + VFO A Volume (toggle the s-meter type by
        // clicking the meter). The audio button is the same switch as the one on
        // the main screen, same label and same shared toggle method - a channel
        // can be muted from either place without the two disagreeing.
        ui.horizontal(|ui| {
            if Self::render_window_audio_toggle(
                ui, self.rx1_enabled, self.connected,
                &rust_i18n::t!("main_hover_chip_audio", name = "RX1").to_string(),
            ) {
                self.toggle_rx1_audio();
            }
            ui.separator();
            ui.label("VFO A:");
            let vol_slider = egui::Slider::new(&mut self.vfo_a_volume, 0.001..=1.0)
                .logarithmic(true)
                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
            let resp = ui.add(vol_slider).on_hover_text(rust_i18n::t!("main_hover_set_mix_volume").to_string());
            let scrolled = helpers::slider_wheel(ui, &resp, &mut self.vfo_a_volume, 0.001..=1.0, 0.02);
            if resp.changed() || scrolled {
                let _ = self.cmd_tx.send(Command::SetVfoAVolume(self.vfo_a_volume));
                self.save_full_config();
            }
        });

        // -- Band buttons (via controls::render_band_selector) --
        let band_click = self.with_rx_ctx(
            controls::RxChannel::Rx1,
            controls::UiDensity::Extended,
            surface,
            |ctx| controls::render_band_selector(ui, ctx),
        );
        if let Some(click) = band_click {
            self.handle_band_switch(Vfo::A, click);
        }

        // -- Mode selector (via controls::render_mode_selector) --
        // Disabled during own TX: a mid-TX mode change is dropped by the server
        // (Thetis-bug workaround), so block it in the UI too to avoid a misleading
        // indication.
        let ptt_active = self.ptt;
        let mode_action = self.with_rx_ctx(
            controls::RxChannel::Rx1,
            controls::UiDensity::Extended,
            surface,
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
        // Only mutate local state if dispatch actually sent a command
        // (not on Disconnected / SendFailed) - otherwise UI state drifts vs. server state.
        if let Some((click, true)) = mode_action {
            self.mode = click.mode;
            self.filter_changed_at = None;
            self.tci_control_changed_at = Some(Instant::now());
        }

        // -- Frequency step buttons (via controls::render_freq_step_controls) --
        let step_action = self.with_rx_ctx(
            controls::RxChannel::Rx1,
            controls::UiDensity::Extended,
            surface,
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
        // Only update pending_freq if dispatch succeeded - otherwise UI drift.
        if let Some((new_freq, true)) = step_action {
            self.set_pending_freq_a(new_freq);
        }

        // -- Filter + NR + ANF --
        {
            let presets = filter_presets_for_mode(self.mode);
            let cw = is_cw_mode(self.mode);
            let is_fm = self.mode == 5;
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

                if is_fm {
                    // FM: show actual bandwidth from Thetis + deviation label
                    let dev_label = if current_bw <= 6000 { "NFM" } else { "WFM" };
                    let bw_text = format!("{} {}", format_bandwidth(current_bw, false), dev_label);
                    ui.label(RichText::new(bw_text).strong().size(14.0));
                } else {
                    ui.label(RichText::new(format_bandwidth(presets[idx], cw)).strong().size(14.0));
                }

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

                ui.add_space(10.0);

                // NR cycle
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

                // NB cycle: OFF -> NB1 -> NB2 (extensions) -> OFF
                let nb_label = match self.nb_level { 1 => "NB1".to_string(), 2 => "NB2".to_string(), _ => "NB".to_string() };
                let nb_btn = if self.nb_level > 0 {
                    egui::Button::new(RichText::new(&nb_label).strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else {
                    egui::Button::new(&nb_label)
                };
                if ui.add(nb_btn).on_hover_text(rust_i18n::t!("main_hover_nb").to_string()).clicked() {
                    let max_nb: u8 = if self.ddc_sample_rate_rx1 > 0 { 2 } else { 1 };
                    let new_val = if self.nb_level >= max_nb { 0 } else { self.nb_level + 1 };
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::NoiseBlanker, new_val as u16));
                    self.nb_level = new_val;
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

                // MON (TX Monitor) toggle
                let mon_btn = if self.mon_on {
                    egui::Button::new(RichText::new("MON").strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else {
                    egui::Button::new("MON")
                };
                if ui.add(mon_btn).on_hover_text(rust_i18n::t!("main_hover_tx_monitor").to_string()).clicked() {
                    let new_val = !self.mon_on;
                    let _ = self.cmd_tx.send(Command::SetMonitor(new_val));
                    self.mon_on = new_val;
                }
            });
        }
    }

    pub(super) fn render_rx1_popout_content(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.render_rx1_controls(ui, controls::UiSurface::PopoutSeparate);
        ui.separator();
        // -- Spectrum + waterfall (placeholder when no bins yet, mirrors RX2) --
        if !self.spectrum_bins.is_empty() {
            self.render_spectrum_content(ui, ctx, 0.0, true);
        } else {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new(rust_i18n::t!("main_waiting_rx1_spectrum").to_string()).weak());
            });
        }
    }

    /// Render RX2 controls only (VFO, S-meter, band, mode, freq step, filter, NR, ANF)
    /// If `show_split_button` is true, a Split button is shown right-aligned on the S-meter row.
    pub(super) fn render_rx2_controls_with_split(&mut self, ui: &mut egui::Ui, show_split_button: bool, is_popout: bool, surface: controls::UiSurface) {
        if is_popout && self.meter_analog[M_RX2] {
            let total_w = ui.available_width();
            let start = ui.cursor().min;

            // Measure controls height
            let measure_rect = egui::Rect::from_min_size(start, egui::vec2(total_w, 500.0));
            let mut measure = ui.new_child(egui::UiBuilder::new().max_rect(measure_rect).layout(egui::Layout::top_down(egui::Align::LEFT)));
            self.render_rx2_controls_inner(&mut measure, show_split_button, is_popout, surface);
            let controls_h = measure.min_rect().height();

            // Reserve a slim column for the Split button so the analog meter
            // can render at its natural size (matching RX1's meter).
            let split_w: f32 = if show_split_button { 60.0 } else { 0.0 };
            let meter_w = (controls_h * SMETER_VIS_ASPECT).min(total_w - 480.0).max(0.0);
            let gap_left = if meter_w > 0.0 { 8.0 } else { 0.0 };
            let split_gap = if split_w > 0.0 { 4.0 } else { 0.0 };
            let controls_w = (total_w - meter_w - split_w - split_gap - gap_left).max(0.0);

            let controls_rect = egui::Rect::from_min_size(start, egui::vec2(controls_w, 500.0));
            let mut left = ui.new_child(egui::UiBuilder::new().max_rect(controls_rect).layout(egui::Layout::top_down(egui::Align::LEFT)));
            self.render_rx2_controls_inner(&mut left, show_split_button, is_popout, surface);

            if show_split_button {
                let btn_h: f32 = 24.0;
                let pair_h: f32 = btn_h * 2.0 + 4.0;
                let split_x = start.x + controls_w + split_gap;
                // Top-align with the meter so RX1 (A<>B) and RX2 (Split/Join)
                // buttons share the same visual baseline.
                let btn_rect = egui::Rect::from_min_size(
                    egui::pos2(split_x, start.y),
                    egui::vec2(split_w, pair_h),
                );
                let mut btn_ui = ui.new_child(
                    egui::UiBuilder::new().max_rect(btn_rect).layout(egui::Layout::top_down(egui::Align::Center))
                );
                let sz = Some(egui::vec2(split_w, btn_h));
                self.render_split_join_segmented(&mut btn_ui, true, sz);
            }

            if meter_w > 80.0 {
                let meter_x = start.x + controls_w + split_gap + split_w + gap_left;
                let meter_rect = egui::Rect::from_min_size(
                    egui::pos2(meter_x, start.y),
                    egui::vec2(meter_w, controls_h),
                );
                let mut right = ui.new_child(
                    egui::UiBuilder::new().max_rect(meter_rect).layout(egui::Layout::top_down(egui::Align::LEFT))
                );
                self.popout_rx2_smeter_rect = smeter_analog_sized(&mut right, self.rx2_smeter, self.rx2_smeter_peak, false, false, Some((meter_w, controls_h)));
            }

            ui.advance_cursor_after_rect(egui::Rect::from_min_size(start, egui::vec2(total_w, controls_h)));
        } else {
            self.render_rx2_controls_inner(ui, show_split_button, is_popout, surface);
        }
        // Clicking the RX2 s-meter toggles analog <-> bar (only in popout, where the
        // meter rect is captured).
        if is_popout {
            let mrect = self.popout_rx2_smeter_rect;
            self.meter_click(ui, mrect, M_RX2);
        }
    }

    pub(super) fn render_rx2_controls_inner(&mut self, ui: &mut egui::Ui, show_split_button: bool, is_popout: bool, surface: controls::UiSurface) {
        let amber = Color32::from_rgb(255, 170, 40);

        // -- Top bar: frequency + mode (via render_frequency_display) --
        // PATCH-rx2-inline-edit: RX2 now gets the same inline-edit UX as RX1
        // (click VFO B label -> edit -> Enter -> dispatch). Scroll-wheel keeps
        // working (Extended density is not scroll-gated).
        let _ = is_popout; // parameter stays for signature-compat; not-popout is dead code.
        ui.horizontal(|ui| {
            let action = self.with_rx_ctx(
                controls::RxChannel::Rx2,
                controls::UiDensity::Extended,
                surface,
                |ctx| {
                    controls::render_frequency_display(ui, ctx).map(|a| match a {
                        controls::FrequencyDisplayAction::Submit { hz } => {
                            let intent = controls::UiIntent::InlineFreqEdit {
                                channel: controls::RxChannel::Rx2,
                                hz,
                            };
                            let dispatched = ctx.dispatch(intent, Command::SetFrequencyRx2(hz));
                            (hz, dispatched)
                        }
                        controls::FrequencyDisplayAction::ScrollTune { delta_hz } => {
                            let new_freq = (ctx.rx_state.frequency_hz as i64 + delta_hz).max(0) as u64;
                            let intent = controls::UiIntent::TuneByDelta {
                                channel: controls::RxChannel::Rx2,
                                delta_hz,
                            };
                            let dispatched = ctx.dispatch(intent, Command::SetFrequencyRx2(new_freq));
                            (new_freq, dispatched)
                        }
                    })
                },
            );
            if let Some((new_freq, true)) = action {
                self.set_pending_freq_b(new_freq);
            }

            ui.separator();

            let mode_label = match self.rx2_mode {
                0 => "LSB", 1 => "USB", 2 => "DSB", 3 => "CW-L", 4 => "CW-U",
                5 => "FM", 6 => "AM", 7 => "DIGU", 8 => "SPEC", 9 => "DIGL",
                10 => "SAM", 11 => "DRM", _ => "?",
            };
            ui.label(RichText::new(mode_label).size(16.0).color(amber));

            let bw = self.rx2_filter_high_hz - self.rx2_filter_low_hz;
            let bw_text = if bw >= 1000 {
                format!("{:.1}k", bw as f32 / 1000.0)
            } else {
                format!("{} Hz", bw)
            };
            ui.label(RichText::new(bw_text).size(12.0).weak());
        });

        // S-meter bar for RX2 (hidden when analog meter is shown in popout wrapper)
        if !(is_popout && self.meter_analog[M_RX2]) {
            if show_split_button {
                ui.horizontal(|ui| {
                    self.popout_rx2_smeter_rect = if is_popout {
                        smeter_bar_popout(ui, self.rx2_smeter, self.rx2_smeter_peak, false, false, 100)
                    } else {
                        smeter_bar(ui, self.rx2_smeter, self.rx2_smeter_peak, false, false, 100)
                    };
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        self.render_split_join_segmented(ui, false, None);
                    });
                });
            } else {
                self.popout_rx2_smeter_rect = if is_popout {
                    smeter_bar_popout(ui, self.rx2_smeter, self.rx2_smeter_peak, false, false, 100)
                } else {
                    smeter_bar(ui, self.rx2_smeter, self.rx2_smeter_peak, false, false, 100)
                };
            }
        }

        // -- Controls row: VFO Sync, Volume --
        ui.horizontal(|ui| {
            let sync_btn = if self.vfo_sync {
                egui::Button::new(RichText::new("VFO Sync").size(12.0).strong())
                    .fill(Color32::from_rgb(100, 160, 230))
            } else {
                egui::Button::new(RichText::new("VFO Sync").size(12.0))
            };
            if ui.add_enabled(self.connected, sync_btn).on_hover_text(rust_i18n::t!("main_hover_vfo_sync").to_string()).clicked() {
                self.vfo_sync = !self.vfo_sync;
                let _ = self.cmd_tx.send(Command::SetVfoSync(self.vfo_sync));
            }

            ui.separator();

            if Self::render_window_audio_toggle(
                ui, self.rx2_enabled, self.connected,
                &rust_i18n::t!("main_hover_chip_audio", name = "RX2").to_string(),
            ) {
                self.toggle_rx2_audio();
            }
            ui.separator();

            ui.label("VFO B:");
            let vol_slider = egui::Slider::new(&mut self.vfo_b_volume, 0.001..=1.0)
                .logarithmic(true)
                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
            let resp = ui.add(vol_slider).on_hover_text(rust_i18n::t!("main_hover_set_mix_volume").to_string());
            let scrolled = helpers::slider_wheel(ui, &resp, &mut self.vfo_b_volume, 0.001..=1.0, 0.02);
            if resp.changed() || scrolled {
                let _ = self.cmd_tx.send(Command::SetVfoBVolume(self.vfo_b_volume));
                self.save_full_config();
            }
        });

        // -- Band buttons (via controls::render_band_selector) --
        let band_click = self.with_rx_ctx(
            controls::RxChannel::Rx2,
            controls::UiDensity::Extended,
            surface,
            |ctx| controls::render_band_selector(ui, ctx),
        );
        if let Some(click) = band_click {
            self.handle_band_switch(Vfo::B, click);
        }

        // -- Mode selector (via controls::render_mode_selector) --
        let mode_action = self.with_rx_ctx(
            controls::RxChannel::Rx2,
            controls::UiDensity::Extended,
            surface,
            |ctx| {
                controls::render_mode_selector(ui, ctx).map(|c| {
                    let intent = controls::UiIntent::SelectMode {
                        channel: controls::RxChannel::Rx2,
                        mode: c.mode,
                    };
                    let dispatched = ctx.dispatch(intent, Command::SetModeRx2(c.mode));
                    (c, dispatched)
                })
            },
        );
        if let Some((click, true)) = mode_action {
            self.rx2_mode = click.mode;
            // PATCH-tl2-rx2-mode-switch-filter-restore: mirror RX1 behavior
            // (line 2786-2790). On click of an RX2 mode button we again accept
            // server-side filter updates (the server's rx_filter_band
            // after a mode switch overwrites the locally-adjusted filter
            // edges that applied to the old mode profile). Without this
            // reset the sync-loop compare on `state.mode_rx2 !=
            // self.rx2_mode` is false because the optimistic `self.rx2_mode =
            // click.mode` above already ran; so
            // `rx2_filter_changed_at = Some(...)` stays set and the
            // sync-loop ignores the incoming Rx2FilterLow/High ControlIds.
            self.rx2_filter_changed_at = None;
        }

        // -- Frequency step buttons (via controls::render_freq_step_controls) --
        // ± buttons here had no connected-guard for the RX2 popout
        // (raw `ui.button(...)`).
        let step_action = self.with_rx_ctx(
            controls::RxChannel::Rx2,
            controls::UiDensity::Extended,
            surface,
            |ctx| {
                controls::render_freq_step_controls(ui, ctx).map(|step| {
                    let delta = step.delta_hz(ctx.rx_state.freq_step_index);
                    let new_freq = (ctx.rx_state.frequency_hz as i64 + delta).max(0) as u64;
                    let intent = controls::UiIntent::TuneByDelta {
                        channel: controls::RxChannel::Rx2,
                        delta_hz: delta,
                    };
                    let dispatched = ctx.dispatch(intent, Command::SetFrequencyRx2(new_freq));
                    (new_freq, dispatched)
                })
            },
        );
        if let Some((new_freq, true)) = step_action {
            self.set_pending_freq_b(new_freq);
        }

        // -- Filter bandwidth control --
        {
            let presets = filter_presets_for_mode(self.rx2_mode);
            let cw = is_cw_mode(self.rx2_mode);
            let is_fm = self.rx2_mode == 5;
            // Fallback to RX1 filter if RX2 filter not available (Thetis ZZRL/ZZRH may not respond)
            let (fl, fh) = if self.rx2_filter_low_hz != 0 || self.rx2_filter_high_hz != 0 {
                (self.rx2_filter_low_hz, self.rx2_filter_high_hz)
            } else {
                (self.filter_low_hz, self.filter_high_hz)
            };
            let current_bw = fh - fl;
            let idx = closest_preset_index(presets, current_bw);

            ui.horizontal(|ui| {
                ui.label(rust_i18n::t!("main_filter_label").to_string());
                let minus_btn = egui::Button::new(RichText::new(" - ").size(14.0));
                if ui.add_enabled(idx > 0, minus_btn).on_hover_text(rust_i18n::t!("main_hover_narrow_filter").to_string()).clicked() {
                    let (low, high) = calc_filter_edges(
                        self.rx2_mode, fl, fh, presets[idx - 1]);
                    let _ = self.cmd_tx.send(Command::SetControl(
                        ControlId::Rx2FilterLow, low as i16 as u16));
                    let _ = self.cmd_tx.send(Command::SetControl(
                        ControlId::Rx2FilterHigh, high as i16 as u16));
                    self.rx2_filter_low_hz = low;
                    self.rx2_filter_high_hz = high;
                    self.rx2_filter_changed_at = Some(Instant::now());
                }

                if is_fm {
                    let dev_label = if current_bw <= 6000 { "NFM" } else { "WFM" };
                    let bw_text = format!("{} {}", format_bandwidth(current_bw, false), dev_label);
                    ui.label(RichText::new(bw_text).strong().size(14.0));
                } else {
                    ui.label(RichText::new(format_bandwidth(presets[idx], cw)).strong().size(14.0));
                }

                let plus_btn = egui::Button::new(RichText::new(" + ").size(14.0));
                if ui.add_enabled(idx < presets.len() - 1, plus_btn).on_hover_text(rust_i18n::t!("main_hover_widen_filter").to_string()).clicked() {
                    let (low, high) = calc_filter_edges(
                        self.rx2_mode, fl, fh, presets[idx + 1]);
                    let _ = self.cmd_tx.send(Command::SetControl(
                        ControlId::Rx2FilterLow, low as i16 as u16));
                    let _ = self.cmd_tx.send(Command::SetControl(
                        ControlId::Rx2FilterHigh, high as i16 as u16));
                    self.rx2_filter_low_hz = low;
                    self.rx2_filter_high_hz = high;
                    self.rx2_filter_changed_at = Some(Instant::now());
                }

                ui.add_space(10.0);

                // NR cycle: OFF -> NR1 -> NR2 -> NR3 -> NR4 -> OFF
                let nr_label = if self.rx2_nr_level == 0 { "NR".to_string() } else { format!("NR{}", self.rx2_nr_level) };
                let nr_btn = if self.rx2_nr_level > 0 {
                    egui::Button::new(RichText::new(&nr_label).strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else {
                    egui::Button::new(&nr_label)
                };
                if ui.add(nr_btn).on_hover_text(rust_i18n::t!("main_hover_nr").to_string()).clicked() {
                    let new_val = if self.rx2_nr_level >= 4 { 0 } else { self.rx2_nr_level + 1 };
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Rx2NoiseReduction, new_val as u16));
                    self.rx2_nr_level = new_val;
                }

                // NB cycle: OFF -> NB1 -> NB2 (extensions) -> OFF
                let rx2_nb_label = match self.rx2_nb_level { 1 => "NB1".to_string(), 2 => "NB2".to_string(), _ => "NB".to_string() };
                let nb_btn = if self.rx2_nb_level > 0 {
                    egui::Button::new(RichText::new(&rx2_nb_label).strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else {
                    egui::Button::new(&rx2_nb_label)
                };
                if ui.add(nb_btn).on_hover_text(rust_i18n::t!("main_hover_nb").to_string()).clicked() {
                    let max_nb: u8 = if self.ddc_sample_rate_rx1 > 0 { 2 } else { 1 };
                    let new_val = if self.rx2_nb_level >= max_nb { 0 } else { self.rx2_nb_level + 1 };
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Rx2NoiseBlanker, new_val as u16));
                    self.rx2_nb_level = new_val;
                }

                // ANF toggle
                let anf_btn = if self.rx2_anf_on {
                    egui::Button::new(RichText::new("ANF").strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else {
                    egui::Button::new("ANF")
                };
                if ui.add(anf_btn).on_hover_text(rust_i18n::t!("main_hover_anf").to_string()).clicked() {
                    let new_val = !self.rx2_anf_on;
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Rx2AutoNotchFilter, new_val as u16));
                    self.rx2_anf_on = new_val;
                }
            });
        }
    }

    /// Read RX1 spectrum interaction temp keys and send frequency commands.
    /// Must be called inside the same viewport that rendered the spectrum.
    pub(super) fn handle_rx1_spectrum_keys(&mut self, ctx: &egui::Context) {
        for key in ["spectrum_scroll_freq", "spectrum_click_freq", "spectrum_drag_freq"] {
            let freq: Option<u64> = ctx.memory(|mem| {
                mem.data.get_temp(egui::Id::new(key))
            });
            if let Some(freq) = freq {
                let _ = self.cmd_tx.send(Command::SetFrequency(freq));
                self.set_pending_freq_a(freq);
                ctx.memory_mut(|mem| {
                    mem.data.remove::<u64>(egui::Id::new(key));
                });
            }
        }
        // Filter edge drag - always send both low+high (server expects pair)
        {
            use sdr_remote_core::protocol::ControlId;
            let drag_lo: Option<i32> = ctx.memory(|mem| mem.data.get_temp(egui::Id::new("spectrum_filter_low")));
            let drag_hi: Option<i32> = ctx.memory(|mem| mem.data.get_temp(egui::Id::new("spectrum_filter_high")));
            // In symmetric modes (AM/SAM/DSB/FM/DRM) Thetis forces ±W around the
            // carrier - dragging one edge there must mirror to both, otherwise
            // TL2 shows an asymmetric band Thetis can't honour and narrowing one
            // edge appears to do nothing (Thetis keeps the wider side).
            let sym = is_symmetric_mode(self.mode);
            if let Some(hz) = drag_lo {
                if sym {
                    let w = hz.abs();
                    self.filter_low_hz = -w;
                    self.filter_high_hz = w;
                } else {
                    self.filter_low_hz = hz;
                }
                let _ = self.cmd_tx.send(Command::SetControl(ControlId::FilterLow, self.filter_low_hz as i16 as u16));
                let _ = self.cmd_tx.send(Command::SetControl(ControlId::FilterHigh, self.filter_high_hz as i16 as u16));
                self.filter_changed_at = Some(std::time::Instant::now());
                ctx.memory_mut(|mem| { mem.data.remove::<i32>(egui::Id::new("spectrum_filter_low")); });
            }
            if let Some(hz) = drag_hi {
                if sym {
                    let w = hz.abs();
                    self.filter_low_hz = -w;
                    self.filter_high_hz = w;
                } else {
                    self.filter_high_hz = hz;
                }
                let _ = self.cmd_tx.send(Command::SetControl(ControlId::FilterLow, self.filter_low_hz as i16 as u16));
                let _ = self.cmd_tx.send(Command::SetControl(ControlId::FilterHigh, self.filter_high_hz as i16 as u16));
                self.filter_changed_at = Some(std::time::Instant::now());
                ctx.memory_mut(|mem| { mem.data.remove::<i32>(egui::Id::new("spectrum_filter_high")); });
            }
        }
    }

    /// Read RX2 spectrum interaction temp keys and send frequency commands.
    /// Must be called inside the same viewport that rendered the spectrum.
    pub(super) fn handle_rx2_spectrum_keys(&mut self, ctx: &egui::Context) {
        for key in ["rx2_spectrum_scroll_freq", "rx2_spectrum_click_freq", "rx2_spectrum_drag_freq"] {
            let freq: Option<u64> = ctx.memory(|mem| {
                mem.data.get_temp(egui::Id::new(key))
            });
            if let Some(freq) = freq {
                let _ = self.cmd_tx.send(Command::SetFrequencyRx2(freq));
                self.set_pending_freq_b(freq);
                ctx.memory_mut(|mem| {
                    mem.data.remove::<u64>(egui::Id::new(key));
                });
            }
        }
        // PATCH-tl2-rx2-spectrum-filter-drag-isolation: RX2 filter-edge drag
        // reader, mirrors the RX1 path (line ~3290). Previously wrote to
        // the same global "spectrum_filter_low/high" keys, so the
        // RX1 reader passed the update on as an RX1 filter; now per-channel
        // keys via SpectrumPlotConfig. The server expects the other edge
        // in every filter update (low+high pair).
        use sdr_remote_core::protocol::ControlId;
        let drag_lo: Option<i32> = ctx.memory(|mem| mem.data.get_temp(egui::Id::new("rx2_spectrum_filter_low")));
        let drag_hi: Option<i32> = ctx.memory(|mem| mem.data.get_temp(egui::Id::new("rx2_spectrum_filter_high")));
        // Symmetric modes mirror a dragged edge to both sides (see RX1 above).
        let sym = is_symmetric_mode(self.rx2_mode);
        if let Some(hz) = drag_lo {
            if sym {
                let w = hz.abs();
                self.rx2_filter_low_hz = -w;
                self.rx2_filter_high_hz = w;
            } else {
                self.rx2_filter_low_hz = hz;
            }
            let _ = self.cmd_tx.send(Command::SetControl(ControlId::Rx2FilterLow, self.rx2_filter_low_hz as i16 as u16));
            let _ = self.cmd_tx.send(Command::SetControl(ControlId::Rx2FilterHigh, self.rx2_filter_high_hz as i16 as u16));
            self.rx2_filter_changed_at = Some(std::time::Instant::now());
            ctx.memory_mut(|mem| { mem.data.remove::<i32>(egui::Id::new("rx2_spectrum_filter_low")); });
        }
        if let Some(hz) = drag_hi {
            if sym {
                let w = hz.abs();
                self.rx2_filter_low_hz = -w;
                self.rx2_filter_high_hz = w;
            } else {
                self.rx2_filter_high_hz = hz;
            }
            let _ = self.cmd_tx.send(Command::SetControl(ControlId::Rx2FilterLow, self.rx2_filter_low_hz as i16 as u16));
            let _ = self.cmd_tx.send(Command::SetControl(ControlId::Rx2FilterHigh, self.rx2_filter_high_hz as i16 as u16));
            self.rx2_filter_changed_at = Some(std::time::Instant::now());
            ctx.memory_mut(|mem| { mem.data.remove::<i32>(egui::Id::new("rx2_spectrum_filter_high")); });
        }
    }

    pub(super) fn render_rx2_content(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.render_rx2_controls_with_split(ui, false, true, controls::UiSurface::PopoutSeparate);
        ui.separator();
        self.render_rx2_spectrum_only(ui, ctx);
    }

    /// Render RX2 spectrum + waterfall only (no controls)
    pub(super) fn render_rx2_spectrum_only(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {

        if !self.rx2_spectrum_bins.is_empty() {
            // Row 1: Ref + Auto checkbox + Range (same as RX1)
            ui.horizontal(|ui| {
                ui.spacing_mut().slider_width = 80.0;
                ui.label(rust_i18n::t!("main_ref_label").to_string());
                if self.rx2_auto_ref_enabled {
                    let mut display_val = self.rx2_spectrum_ref_db;
                    ui.add_enabled(false, egui::Slider::new(&mut display_val, -90.0..=0.0)
                        .suffix(" dB")
                        .step_by(5.0)
                    );
                } else {
                    let resp = ui.add(egui::Slider::new(&mut self.rx2_spectrum_ref_db, -90.0..=0.0)
                        .suffix(" dB")
                        .step_by(5.0)
                    ).on_hover_text(rust_i18n::t!("main_hover_ref").to_string());
                    let scrolled = helpers::slider_wheel(ui, &resp, &mut self.rx2_spectrum_ref_db, -90.0..=0.0, 5.0);
                    if resp.changed() || scrolled {
                        self.save_full_config();
                    }
                }
                if ui.checkbox(&mut self.rx2_auto_ref_enabled, rust_i18n::t!("main_auto").to_string()).on_hover_text(rust_i18n::t!("main_hover_auto_ref").to_string()).changed() {
                    if self.rx2_auto_ref_enabled {
                        self.rx2_spectrum.reset_auto_ref();
                    }
                    self.save_full_config();
                }
                ui.label(rust_i18n::t!("main_range_label").to_string());
                let resp = ui.add(egui::Slider::new(&mut self.rx2_spectrum_range_db, 20.0..=130.0)
                    .suffix(" dB")
                    .step_by(5.0)
                ).on_hover_text(rust_i18n::t!("main_hover_range").to_string());
                let scrolled = helpers::slider_wheel(ui, &resp, &mut self.rx2_spectrum_range_db, 20.0..=130.0, 5.0);
                if resp.changed() || scrolled {
                    if self.rx2_auto_ref_enabled {
                        self.rx2_spectrum.reset_auto_ref();
                    }
                    self.save_full_config();
                }
            });
            // Row 2: Zoom/Pan controls
            ui.horizontal(|ui| {
                ui.spacing_mut().slider_width = 80.0;
                ui.label(rust_i18n::t!("main_zoom_label").to_string());
                // TL2-1 ctun-auto-recenter: same zoom-min logic as RX1 (per-RX independent
                // zoom but same checkbox-clamp policy).
                let zoom_min_rx2: f32 = if self.allow_zoom_below_2x { 1.0 } else { 2.0 };
                if self.rx2_spectrum_zoom < zoom_min_rx2 {
                    self.rx2_spectrum_zoom = zoom_min_rx2;
                }
                let zoom_resp = ui.add(egui::Slider::new(&mut self.rx2_spectrum_zoom, zoom_min_rx2..=1024.0)
                    .logarithmic(true)
                    .custom_formatter(|v, _| format!("{:.0}x", v))
                ).on_hover_text(rust_i18n::t!("main_hover_zoom").to_string());
                let zoom_step = (self.rx2_spectrum_zoom as f64 * 0.1).max(1.0);
                let zoom_scrolled = helpers::slider_wheel(ui, &zoom_resp, &mut self.rx2_spectrum_zoom, zoom_min_rx2..=1024.0, zoom_step);
                let zoom_changed = zoom_resp.changed() || zoom_scrolled;
                if zoom_changed {
                    let max_pan = crate::ui::tuning::max_pan_fraction(self.rx2_spectrum_zoom);
                    self.rx2_spectrum_pan = self.rx2_spectrum_pan.clamp(-max_pan, max_pan);
                }
                ui.label(rust_i18n::t!("main_pan_label").to_string());
                let max_pan = crate::ui::tuning::max_pan_fraction(self.rx2_spectrum_zoom);
                let pan_resp = ui.add(egui::Slider::new(&mut self.rx2_spectrum_pan, -max_pan..=max_pan)
                    .custom_formatter(|v, _| format!("{:+.2}", v))
                ).on_hover_text(rust_i18n::t!("main_hover_pan").to_string());
                let pan_scrolled = helpers::slider_wheel(ui, &pan_resp, &mut self.rx2_spectrum_pan, -max_pan..=max_pan, (max_pan as f64 * 0.1).max(0.0001));
                let pan_changed = pan_resp.changed() || pan_scrolled;
                ui.label(rust_i18n::t!("main_wf_label").to_string());
                let wf_resp = ui.add(egui::Slider::new(&mut self.rx2_waterfall_contrast, 0.3..=3.0)
                    .logarithmic(true)
                    .custom_formatter(|v, _| format!("{:.1}", v))
                ).on_hover_text(rust_i18n::t!("main_hover_wf").to_string());
                let wf_scrolled = helpers::slider_wheel(ui, &wf_resp, &mut self.rx2_waterfall_contrast, 0.3..=3.0, 0.1);
                if wf_resp.changed() || wf_scrolled {
                    self.save_full_config();
                }

                // RX2 FFT size selector
                let rx2_ddc_rate = if self.ddc_sample_rate_rx2 > 0 { self.ddc_sample_rate_rx2 as u32 * 1000 } else { 384_000 };
                let rx2_auto_fft = sdr_remote_core::ddc_fft_size(rx2_ddc_rate);
                let rx2_auto_k = rx2_auto_fft / 1024;
                let rx2_fft_label = if self.rx2_spectrum_fft_size_k == 0 {
                    format!("FFT: Auto ({}K)", rx2_auto_k)
                } else {
                    format!("FFT: {}K", self.rx2_spectrum_fft_size_k)
                };
                let rx2_hop = |fft_k: u32| -> u32 { let fft = fft_k * 1024; rx2_ddc_rate / (fft / 8) };
                let rx2_fft_options: Vec<(u16, String)> = {
                    let mut opts = vec![(0u16, format!("Auto ({}K, ~{} FFT/s)", rx2_auto_k, rx2_hop(rx2_auto_k as u32)))];
                    for &k in &[32u16, 64, 128, 256, 512, 1024] {
                        let fft = k as u32 * 1024;
                        if fft <= rx2_ddc_rate * 4 {
                            let fps = rx2_hop(k as u32);
                            if fps > 0 { opts.push((k, format!("{}K (~{} FFT/s)", k, fps))); }
                        }
                    }
                    opts
                };
                egui::ComboBox::from_id_salt("rx2_fft_size")
                    .selected_text(&rx2_fft_label)
                    .width(80.0)
                    .show_ui(ui, |ui| {
                        for (k, label) in &rx2_fft_options {
                            if ui.selectable_label(self.rx2_spectrum_fft_size_k == *k, label).clicked() {
                                self.rx2_spectrum_fft_size_k = *k;
                                let _ = self.cmd_tx.send(Command::SetControl(ControlId::Rx2SpectrumFftSize, *k));
                                self.save_full_config();
                            }
                        }
                    });

                if zoom_changed || pan_changed {
                    self.rx2_zoom_pan_changed_at = Some(Instant::now());
                }
            });

            // Debounce: send zoom/pan to server after 100ms
            if let Some(changed_at) = self.rx2_zoom_pan_changed_at {
                if changed_at.elapsed().as_millis() >= 100 {
                    let zoom_diff = (self.rx2_spectrum_zoom - self.rx2_last_sent_zoom).abs();
                    let pan_diff = (self.rx2_spectrum_pan - self.rx2_last_sent_pan).abs();
                    if zoom_diff > 0.01 {
                        let _ = self.cmd_tx.send(Command::SetRx2SpectrumZoom(self.rx2_spectrum_zoom));
                        self.rx2_last_sent_zoom = self.rx2_spectrum_zoom;
                    }
                    if pan_diff > 0.001 {
                        let _ = self.cmd_tx.send(Command::SetRx2SpectrumPan(self.rx2_spectrum_pan));
                        self.rx2_last_sent_pan = self.rx2_spectrum_pan;
                    }
                    self.rx2_zoom_pan_changed_at = None;
                }
            }

            // Dynamic spectrum + waterfall layout
            let available = ui.available_height();
            let spec_area = available.max(200.0);
            let spec_h = (spec_area * 0.50).max(100.0);
            let wf_h = (spec_area * 0.50).max(80.0);

            // Smooth RX2 display center (same algorithm as RX1)
            let rx2_target_center = Self::spectrum_target_center_hz(
                self.rx2_frequency_hz,
                self.rx2_full_span_hz(),
                self.rx2_spectrum_pan,
            );
            let rx2_tuning_active = Self::tuning_latch_active(
                self.rx2_force_full_tuning,
                self.rx2_pending_freq,
                self.rx2_pending_freq_at,
            );
            // Use same alpha as RX1 (computed earlier in this frame)
            let alpha_rx2 = self.smooth_alpha;
            if self.rx2_pending_freq.is_some() {
                self.rx2_smooth_display_center_hz = rx2_target_center.round();
            } else if self.rx2_smooth_display_center_hz == 0.0 {
                self.rx2_smooth_display_center_hz = rx2_target_center;
            } else {
                self.rx2_smooth_display_center_hz += (rx2_target_center - self.rx2_smooth_display_center_hz) * alpha_rx2;
            }
            if (self.rx2_smooth_display_center_hz - rx2_target_center).abs() < 1.0 {
                self.rx2_smooth_display_center_hz = rx2_target_center;
            }
            let rx2_smooth_center = self.rx2_smooth_display_center_hz as u64;
            // Same rule as RX1: the marker is the VFO, smoothed alongside the
            // centre, and not a subtraction that needs a span the client may
            // not have been told yet.
            let rx2_vfo_target = self.rx2_frequency_hz as f64;
            if self.rx2_pending_freq.is_some() || self.rx2_smooth_vfo_hz == 0.0 {
                self.rx2_smooth_vfo_hz = rx2_vfo_target;
            } else {
                self.rx2_smooth_vfo_hz += (rx2_vfo_target - self.rx2_smooth_vfo_hz) * alpha_rx2;
            }
            if (self.rx2_smooth_vfo_hz - rx2_vfo_target).abs() < 1.0 {
                self.rx2_smooth_vfo_hz = rx2_vfo_target;
            }
            let rx2_smooth_vfo = self.rx2_smooth_vfo_hz as u64;
            // The same trail RX1 leaves, gated the same way: a kilohertz of
            // movement, so it fires on any real tuning and on nothing else.
            // Without it RX2's zoom and centre could only be read off the
            // sliders, which is no way to check a build.
            let rx2_quiet_enough = self
                .rx2_logged_at
                .map(|t| t.elapsed() >= std::time::Duration::from_secs(1))
                .unwrap_or(true);
            if rx2_quiet_enough && self.rx2_frequency_hz.abs_diff(self.rx2_logged_freq_hz) >= 1_000 {
                self.rx2_logged_freq_hz = self.rx2_frequency_hz;
                self.rx2_logged_at = Some(Instant::now());
                log::info!(
                    "RX2 view: tuning={} vfo={} Hz, drawn centre={} server centre={} span={} Hz, zoom={:.1} pan={:.4}, width={} Hz from {}",
                    rx2_tuning_active,
                    self.rx2_frequency_hz,
                    rx2_smooth_center,
                    self.rx2_spectrum_center_hz,
                    self.rx2_spectrum_span_hz,
                    self.rx2_spectrum_zoom,
                    self.rx2_spectrum_pan,
                    self.rx2_full_span_hz(),
                    Self::full_span_source(
                        self.ddc_sample_rate_rx2,
                        self.rx2_full_spectrum_span_hz,
                        self.full_spectrum_enabled,
                    ),
                );
            }
            let (rx2_plot_bins, rx2_plot_center_hz, rx2_plot_span_hz) = if !rx2_tuning_active || self.rx2_full_spectrum_bins.is_empty() {
                (&self.rx2_spectrum_bins, self.rx2_spectrum_center_hz, self.rx2_spectrum_span_hz)
            } else {
                (&self.rx2_full_spectrum_bins, self.rx2_full_spectrum_center_hz, self.rx2_full_spectrum_span_hz)
            };
            spectrum_plot(
                ui,
                rx2_plot_bins,
                rx2_plot_center_hz,
                rx2_plot_span_hz,
                rx2_smooth_center,
                rx2_smooth_vfo,
                self.rx2_frequency_hz,
                self.rx2_spectrum_ref_db,
                self.rx2_spectrum_range_db,
                self.rx2_smeter,
                false,
                false,
                // Fallback to RX1 filter values if RX2 filter not available (ZZRL/ZZRH unsupported)
                if self.rx2_filter_low_hz != 0 || self.rx2_filter_high_hz != 0 {
                    self.rx2_filter_low_hz
                } else {
                    self.filter_low_hz
                },
                if self.rx2_filter_low_hz != 0 || self.rx2_filter_high_hz != 0 {
                    self.rx2_filter_high_hz
                } else {
                    self.filter_high_hz
                },
                0, // RX2 has no RIT
                false,
                spec_h,
                &RX2_PLOT_CONFIG,
                &self.dx_spots,
            );
            render_waterfall(
                ui,
                ctx,
                &mut self.rx2_waterfall,
                if self.full_spectrum_enabled { self.rx2_full_spectrum_span_hz } else { self.rx2_spectrum_span_hz },
                rx2_smooth_center,
                self.rx2_frequency_hz,
                self.rx2_spectrum_zoom,
                self.rx2_waterfall_contrast,
                self.rx2_spectrum_ref_db,
                self.rx2_spectrum_range_db,
                wf_h,
                &RX2_PLOT_CONFIG,
            );
        } else {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new(rust_i18n::t!("main_waiting_rx2_spectrum").to_string()).weak());
            });
        }
    }
}
