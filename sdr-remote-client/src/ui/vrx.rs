// SPDX-License-Identifier: GPL-2.0-or-later
//! VRX (secondary receiver) UI: the sub-receiver send helpers, the shared
//! per-channel controls renderer, the audio/spectrum toggles and the shared VRX
//! spectrum-panel renderer. VRX1 and VRX2 share exactly this code (parity by
//! construction, per docs/internal/UI-STYLE-GUIDE.md). Extracted verbatim from
//! `ui/mod.rs` - pure relocation, no behaviour change. `pub(super)` keeps the
//! methods callable from the parent module tree (popouts/screens/update).

use super::*;
use crate::ui::controls::frequency::step_on_grid;

impl SdrRemoteApp {
    /// Model B `can(VRX)`: VRX spectra are produced only from the Thetis DDC-IQ,
    /// so a VRX window can exist iff Thetis is configured. This is *availability*,
    /// not connection - `connected` gates the wire subscription + live-vs-placeholder
    /// inside an open window, never whether the window opens.
    pub(super) fn can_vrx(&self) -> bool {
        self.thetis_configured
    }

    /// Model B `can(RX2)`: the RX2 spectrum comes from the Thetis DDC and only
    /// exists when the server actually has a second receiver (`rx2_present`;
    /// single-receiver servers report it absent). Availability, not connection.
    pub(super) fn can_rx2(&self) -> bool {
        self.thetis_configured && self.rx2_present
    }

    /// Model B `can(RX1)`: the RX1 spectrum is the main Thetis DDC spectrum, so it
    /// can only be shown when Thetis is configured. (Availability, not connection -
    /// `connected` gates live-vs-placeholder inside an already-shown spectrum.)
    pub(super) fn can_rx1(&self) -> bool {
        self.thetis_configured
    }

    pub(super) fn vrx_send_enabled(&self, ch: VrxChannel, on: bool) -> bool {
        self.cmd_tx.send(match ch {
            VrxChannel::Vrx1 => Command::SetVrxEnabled(on),
            VrxChannel::Vrx2 => Command::SetVrx2Enabled(on),
        }).is_ok()
    }

    pub(super) fn vrx_send_freq(&self, ch: VrxChannel, hz: u64) -> bool {
        self.cmd_tx.send(match ch {
            VrxChannel::Vrx1 => Command::SetVrxFrequency(hz),
            VrxChannel::Vrx2 => Command::SetVrx2Frequency(hz),
        }).is_ok()
    }

    pub(super) fn vrx_send_mode(&self, ch: VrxChannel, mode: u8) -> bool {
        self.cmd_tx.send(match ch {
            VrxChannel::Vrx1 => Command::SetVrxMode(mode),
            VrxChannel::Vrx2 => Command::SetVrx2Mode(mode),
        }).is_ok()
    }

    pub(super) fn vrx_send_volume(&self, ch: VrxChannel, v: f32) -> bool {
        self.cmd_tx.send(match ch {
            VrxChannel::Vrx1 => Command::SetVrxVolume(v),
            VrxChannel::Vrx2 => Command::SetVrx2Volume(v),
        }).is_ok()
    }

    /// A shared renderer for VRX1 AND VRX2 controls (parity by construction,
    /// per docs/internal/UI-STYLE-GUIDE.md). Top row = status (mode amber / BW weak),
    /// operation via shared segmented selectors and theme-toggle/action helpers with
    /// mandatory hover. Dispatch discipline (Decision 8) same as RX: controls are
    /// only active when connected, and local state is only mutated after the
    /// command has actually been sent.
    /// Wrapper (like RX): in analog mode the s-meter sits at the top right with the
    /// controls in a left column; in bar mode inner renders everything in-line.
    pub(super) fn render_vrx_channel_controls(
        &mut self,
        ui: &mut egui::Ui,
        ch: VrxChannel,
        ddc_center: u64,
        ddc_min: u64,
        ddc_max: u64,
    ) {
        let meter_ch = match ch { VrxChannel::Vrx1 => M_VRX1, VrxChannel::Vrx2 => M_VRX2 };
        if self.meter_analog[meter_ch] {
            let smeter = match ch { VrxChannel::Vrx1 => self.vrx1_spectrum.smeter_dbm(), VrxChannel::Vrx2 => self.vrx2_spectrum.smeter_dbm() };
            let smeter_peak = match ch { VrxChannel::Vrx1 => self.vrx1_spectrum.smeter_peak(), VrxChannel::Vrx2 => self.vrx2_spectrum.smeter_peak() };
            let total_w = ui.available_width();
            let start = ui.cursor().min;
            // Measure the controls height at full width.
            let measure_rect = egui::Rect::from_min_size(start, egui::vec2(total_w, 500.0));
            let mut measure = ui.new_child(egui::UiBuilder::new().max_rect(measure_rect).layout(egui::Layout::top_down(egui::Align::LEFT)));
            self.render_vrx_channel_controls_inner(&mut measure, ch, ddc_center, ddc_min, ddc_max);
            let controls_h = measure.min_rect().height();
            // Meter right, controls left; leave at least ~260px for the controls.
            let meter_w = (total_w - 260.0).max(0.0).min(controls_h * SMETER_VIS_ASPECT);
            let controls_w = total_w - meter_w - if meter_w > 0.0 { 8.0 } else { 0.0 };
            let controls_rect = egui::Rect::from_min_size(start, egui::vec2(controls_w, 500.0));
            let mut left = ui.new_child(egui::UiBuilder::new().max_rect(controls_rect).layout(egui::Layout::top_down(egui::Align::LEFT)));
            self.render_vrx_channel_controls_inner(&mut left, ch, ddc_center, ddc_min, ddc_max);
            let mut mrect = egui::Rect::NOTHING;
            if meter_w > 80.0 {
                let meter_pos = egui::pos2(start.x + controls_w + 4.0, start.y);
                let meter_rect = egui::Rect::from_min_size(meter_pos, egui::vec2(meter_w, controls_h));
                let mut right = ui.new_child(egui::UiBuilder::new().max_rect(meter_rect).layout(egui::Layout::top_down(egui::Align::LEFT)));
                mrect = smeter_analog_sized(&mut right, smeter, smeter_peak, false, false, Some((meter_w, controls_h.min(180.0))));
            }
            ui.advance_cursor_after_rect(egui::Rect::from_min_size(start, egui::vec2(total_w, controls_h)));
            self.meter_click(ui, mrect, meter_ch);
        } else {
            self.render_vrx_channel_controls_inner(ui, ch, ddc_center, ddc_min, ddc_max);
        }
    }

    pub(super) fn render_vrx_channel_controls_inner(
        &mut self,
        ui: &mut egui::Ui,
        ch: VrxChannel,
        ddc_center: u64,
        ddc_min: u64,
        ddc_max: u64,
    ) {
        let id = ch.id();
        let connected = self.connected;
        let enabled = match ch { VrxChannel::Vrx1 => self.vrx1_enabled, VrxChannel::Vrx2 => self.vrx2_enabled };
        let freq_hz = match ch { VrxChannel::Vrx1 => self.vrx1_freq_hz, VrxChannel::Vrx2 => self.vrx2_freq_hz };
        let mode = match ch { VrxChannel::Vrx1 => self.vrx1_mode, VrxChannel::Vrx2 => self.vrx2_mode };
        let filter_low = match ch { VrxChannel::Vrx1 => self.vrx1_filter_low_hz, VrxChannel::Vrx2 => self.vrx2_filter_low_hz };
        let filter_high = match ch { VrxChannel::Vrx1 => self.vrx1_filter_high_hz, VrxChannel::Vrx2 => self.vrx2_filter_high_hz };
        let mut volume = match ch { VrxChannel::Vrx1 => self.vrx1_volume, VrxChannel::Vrx2 => self.vrx2_volume };
        let high_res = match ch { VrxChannel::Vrx1 => self.vrx1_high_res_spectrum, VrxChannel::Vrx2 => self.vrx2_high_res_spectrum };
        let smeter = match ch { VrxChannel::Vrx1 => self.vrx1_spectrum.smeter_dbm(), VrxChannel::Vrx2 => self.vrx2_spectrum.smeter_dbm() };
        let smeter_peak = match ch { VrxChannel::Vrx1 => self.vrx1_spectrum.smeter_peak(), VrxChannel::Vrx2 => self.vrx2_spectrum.smeter_peak() };
        let source_freq = match ch { VrxChannel::Vrx1 => self.frequency_hz, VrxChannel::Vrx2 => self.rx2_frequency_hz };
        let cur_bw = (filter_high - filter_low).abs();
        let meter_ch = match ch { VrxChannel::Vrx1 => M_VRX1, VrxChannel::Vrx2 => M_VRX2 };
        let analog = self.meter_analog[meter_ch];
        let (freq_prefix, en_hover, copy_hover, mode_hover, bw_hover, vol_hover) = match ch {
            VrxChannel::Vrx1 => ("A:",
                rust_i18n::t!("main_vrx_enable_hover", n = 1).to_string(),
                rust_i18n::t!("main_vrx_copy_hover_a", n = 1).to_string(),
                rust_i18n::t!("main_vrx_mode_hover", n = 1).to_string(),
                rust_i18n::t!("main_vrx_bw_hover", n = 1).to_string(),
                rust_i18n::t!("main_vrx_vol_hover", n = 1).to_string()),
            VrxChannel::Vrx2 => ("B:",
                rust_i18n::t!("main_vrx_enable_hover", n = 2).to_string(),
                rust_i18n::t!("main_vrx_copy_hover_b", n = 2).to_string(),
                rust_i18n::t!("main_vrx_mode_hover", n = 2).to_string(),
                rust_i18n::t!("main_vrx_bw_hover", n = 2).to_string(),
                rust_i18n::t!("main_vrx_vol_hover", n = 2).to_string()),
        };

        // Header: VRXn + enable toggle + DDC range (status, not clickable)
        ui.horizontal(|ui| {
            ui.label(RichText::new(ch.label()).size(theme::TL_CHANNEL_HEADER_FONT).strong());
            if Self::render_window_audio_toggle(ui, enabled, connected, &en_hover) {
                self.toggle_vrx_audio(ch);
            }
            ui.label(RichText::new(format!(
                "{} | DDC {:.3} MHz | range {:.3}-{:.3} MHz",
                band_label(ddc_center),
                ddc_center as f64 / 1_000_000.0,
                ddc_min as f64 / 1_000_000.0,
                ddc_max as f64 / 1_000_000.0,
            )).size(theme::TL_BW_STATUS_FONT).color(Color32::GRAY));
            // Orange "out of band" warning: VRX frequency falls outside the
            // current DDC window (RX is on a different band). VRX then doesn't
            // work meaningfully / very low sensitivity. Amber = status (style guide).
            if freq_hz < ddc_min || freq_hz > ddc_max {
                ui.label(
                    RichText::new(rust_i18n::t!("main_out_of_band").to_string())
                        .size(theme::TL_BW_STATUS_FONT)
                        .strong()
                        .color(theme::TL_AMBER_TEXT),
                )
                .on_hover_text(
                    rust_i18n::t!("main_out_of_band_hover").to_string(),
                );
            }
        });

        // Frequency + mode/BW status + Copy VFO
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{}  ", freq_prefix)).size(theme::TL_FREQ_FONT).strong());
            if let Some(delta) = render_freq_scroll(ui, freq_hz) {
                // The digit under the pointer sets the step, so the grid is that
                // same digit - the band edge snaps inward to it instead of
                // leaving a stray remainder in the readout.
                let next = step_on_grid(freq_hz, delta, delta.unsigned_abs(), ddc_min, ddc_max);
                if connected && self.vrx_send_freq(ch, next) {
                    match ch { VrxChannel::Vrx1 => self.vrx1_freq_hz = next, VrxChannel::Vrx2 => self.vrx2_freq_hz = next };
                    self.save_full_config();
                }
            }
            ui.label(RichText::new(vrx_mode_label(mode)).size(theme::TL_MODE_STATUS_FONT).color(theme::TL_AMBER_TEXT));
            ui.label(RichText::new(format_bandwidth(cur_bw, false)).size(theme::TL_BW_STATUS_FONT).weak());
            if theme::tl_action_button(ui, &rust_i18n::t!("main_copy_vfo").to_string(), connected, theme::TL_SEGMENT_FONT, &copy_hover).clicked()
                && self.vrx_send_freq(ch, source_freq)
            {
                match ch { VrxChannel::Vrx1 => self.vrx1_freq_hz = source_freq, VrxChannel::Vrx2 => self.vrx2_freq_hz = source_freq };
                self.save_full_config();
            }
        });

        // S-meter: in-line in bar mode; in analog mode the wrapper draws it
        // at the top right (like RX). Clicking the meter toggles the type.
        if !analog {
            let mrect = smeter_bar_popout(ui, smeter, smeter_peak, false, false, 100);
            self.meter_click(ui, mrect, meter_ch);
        }

        // Volume
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("main_volume_label").to_string());
            let resp = ui.add_enabled(connected, egui::Slider::new(&mut volume, 0.001..=1.0)
                .logarithmic(true)
                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)))
                .on_hover_text(vol_hover);
            let scrolled = helpers::slider_wheel(ui, &resp, &mut volume, 0.001..=1.0, 0.02);
            if (resp.changed() || scrolled) && self.vrx_send_volume(ch, volume) {
                match ch { VrxChannel::Vrx1 => self.vrx1_volume = volume, VrxChannel::Vrx2 => self.vrx2_volume = volume };
                self.save_full_config();
            }
        });

        // Mode (shared segmented selector; sole control)
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("main_mode_label").to_string());
            if let Some(mode_val) = theme::tl_segmented_selector(
                ui,
                VRX_MODES.iter().map(|&(m, l)| (m, l.to_string())),
                mode, connected, theme::TL_SEGMENT_FONT, &mode_hover,
            ) {
                if self.vrx_send_mode(ch, mode_val) {
                    match ch { VrxChannel::Vrx1 => self.vrx1_mode = mode_val, VrxChannel::Vrx2 => self.vrx2_mode = mode_val };
                    let (lo, hi) = vrx_mode_default_filter(mode_val, cur_bw);
                    // Default filter for this mode: only commit local filter state + persist
                    // after a confirmed send (dispatch-return discipline -
                    // no UI/server drift on a failed send).
                    if self.cmd_tx.send(Command::SetVrxFilter(id, lo, hi)).is_ok() {
                        match ch {
                            VrxChannel::Vrx1 => { self.vrx1_filter_low_hz = lo; self.vrx1_filter_high_hz = hi; }
                            VrxChannel::Vrx2 => { self.vrx2_filter_low_hz = lo; self.vrx2_filter_high_hz = hi; }
                        };
                        self.save_full_config();
                    }
                }
            }
        });

        // SAM auto-tune-to-carrier (only meaningful in SAM, mode 3)
        if mode == 3 {
            ui.horizontal(|ui| {
                let mut at = match ch { VrxChannel::Vrx1 => self.vrx1_auto_tune, VrxChannel::Vrx2 => self.vrx2_auto_tune };
                if ui.add_enabled(connected, egui::Checkbox::new(&mut at, rust_i18n::t!("main_auto_tune_carrier").to_string()))
                    .on_hover_text(rust_i18n::t!("main_auto_tune_carrier_hover").to_string())
                    .changed()
                    && self.cmd_tx.send(Command::SetVrxAutoTune(id, at)).is_ok()
                {
                    match ch { VrxChannel::Vrx1 => self.vrx1_auto_tune = at, VrxChannel::Vrx2 => self.vrx2_auto_tune = at };
                }
            });
        }

        // BW (shared segmented selector; mode-dependent presets)
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("main_bw_label").to_string());
            if let Some(p) = theme::tl_segmented_selector(
                ui,
                vrx_filter_presets(mode).iter().map(|&p| (p, format_bandwidth(p, false))),
                cur_bw, connected, theme::TL_SEGMENT_FONT, &bw_hover,
            ) {
                let (lo, hi) = vrx_filter_from_preset(mode, p);
                if self.cmd_tx.send(Command::SetVrxFilter(id, lo, hi)).is_ok() {
                    match ch {
                        VrxChannel::Vrx1 => { self.vrx1_filter_low_hz = lo; self.vrx1_filter_high_hz = hi; }
                        VrxChannel::Vrx2 => { self.vrx2_filter_low_hz = lo; self.vrx2_filter_high_hz = hi; }
                    };
                    self.save_full_config();
                }
            }
        });

        // High-res spectrum toggle (server-side extracted view)
        ui.horizontal(|ui| {
            let mut hr = high_res;
            if ui.add_enabled(connected, egui::Checkbox::new(&mut hr, rust_i18n::t!("main_high_res_spectrum").to_string()))
                .on_hover_text(rust_i18n::t!("main_high_res_spectrum_hover").to_string())
                .changed()
            {
                self.toggle_vrx_spectrum(ch);
            }
        });
    }
    /// VRX audio checkbox (channel on/off). Sends ONLY the audio subscription; the
    /// VRX spectrum is separate (own toggle) — audio off must not kill the spectrum
    /// and vice versa (checkbox model, same decouple as RX2).
    pub(super) fn toggle_vrx_audio(&mut self, ch: VrxChannel) {
        let enabled = match ch { VrxChannel::Vrx1 => self.vrx1_enabled, VrxChannel::Vrx2 => self.vrx2_enabled };
        let high_res = match ch { VrxChannel::Vrx1 => self.vrx1_high_res_spectrum, VrxChannel::Vrx2 => self.vrx2_high_res_spectrum };
        let now = !enabled;
        if self.vrx_send_enabled(ch, now) {
            match ch { VrxChannel::Vrx1 => self.vrx1_enabled = now, VrxChannel::Vrx2 => self.vrx2_enabled = now };
            match ch {
                VrxChannel::Vrx1 => log::info!("{}", self.vrx1_spectrum.debug_line(now, high_res)),
                VrxChannel::Vrx2 => log::info!("{}", self.vrx2_spectrum.debug_line(now, high_res)),
            }
            self.save_full_config();
        }
    }

    /// How many audio channels this setup can produce, counted on availability
    /// rather than on what is switched on: the label above the master slider
    /// must not flicker while channels are muted.
    pub(super) fn audio_channel_count(&self) -> usize {
        let mut n = 0;
        if self.can_rx1() { n += 1; }
        if self.can_rx2() { n += 1; }
        if self.can_vrx() { n += 1; }              // VRX1 rides RX1
        if self.can_vrx() && self.rx2_present { n += 1; } // VRX2 rides RX2
        if self.yaesu_present_last { n += 1; }
        if self.yaesu2_present_last { n += 1; }
        n
    }

    /// RX1 audio on/off. Shared by the main-screen channel block and the RX1
    /// window, so both routes run the same code instead of each repainting the
    /// same switch (docs/internal/UI-STYLE-GUIDE.md).
    ///
    /// Optimistic like all six audio toggles: the UI flips at once and the
    /// server may only veto within the grace window. That invariant is what
    /// keeps the six channels feeling identical - see `reconcile_audio_enable`.
    pub(super) fn toggle_rx1_audio(&mut self) {
        self.rx1_enabled = !self.rx1_enabled;
        self.rx1_enabled_pending = Some((Instant::now(), self.rx1_enabled));
        let _ = self.cmd_tx.send(Command::SetRx1Enabled(self.rx1_enabled));
        self.save_full_config();
    }

    /// RX2 audio on/off. Same contract as `toggle_rx1_audio`.
    pub(super) fn toggle_rx2_audio(&mut self) {
        self.rx2_enabled = !self.rx2_enabled;
        self.rx2_enabled_pending = Some((Instant::now(), self.rx2_enabled));
        let _ = self.cmd_tx.send(Command::SetRx2Enabled(self.rx2_enabled));
        self.save_full_config();
    }

    /// Yaesu audio on/off for one slot. Separate from the window: the control
    /// window can stay open with the audio muted.
    ///
    /// The two slots persist to different files - slot 0 lives in the full
    /// config, slot 1 in the PTT config - so the save differs while everything
    /// else is shared.
    pub(super) fn toggle_yaesu_audio(&mut self, slot: u8) {
        if slot == 0 {
            self.yaesu_enabled = !self.yaesu_enabled;
            let _ = self.cmd_tx.send(Command::SetControl(
                sdr_remote_core::protocol::ControlId::YaesuEnable, self.yaesu_enabled as u16));
            self.save_full_config();
        } else {
            self.yaesu2_enabled = !self.yaesu2_enabled;
            let _ = self.cmd_tx.send(Command::SetYaesu2Enable(self.yaesu2_enabled));
            self.save_ptt_config();
        }
    }

    /// The audio button as it appears INSIDE a channel window: same label,
    /// same fill convention and same hover as the block on the main screen, so
    /// a channel can be muted from either place without the two disagreeing.
    /// Returns true when clicked.
    pub(super) fn render_window_audio_toggle(
        ui: &mut egui::Ui,
        on: bool,
        connected: bool,
        hover: &str,
    ) -> bool {
        theme::tl_toggle_button(
            ui,
            &rust_i18n::t!("main_chip_audio").to_string(),
            on,
            connected,
            theme::TL_SEGMENT_FONT,
            hover,
        )
        .clicked()
    }

    /// VRX spectrum toggle (high-res). Shared logic (pop-out + row).
    pub(super) fn toggle_vrx_spectrum(&mut self, ch: VrxChannel) {
        let id: u8 = match ch { VrxChannel::Vrx1 => 0, VrxChannel::Vrx2 => 1 };
        let hr = !match ch { VrxChannel::Vrx1 => self.vrx1_high_res_spectrum, VrxChannel::Vrx2 => self.vrx2_high_res_spectrum };
        let zoom = match ch { VrxChannel::Vrx1 => self.vrx1_spectrum_zoom, VrxChannel::Vrx2 => self.vrx2_spectrum_zoom };
        let span_hz = (self.vrx_ddc_span_hz(ch) as f32 / zoom.max(1.0)) as u32;
        let span_khz = ((span_hz / 1000).max(1)) as u16;
        // Spectrum subscription is SEPARATE from the VRX audio checkbox (phase 4 / checkbox model):
        // send `hr`, not `enabled && hr`. The server produces VRX spectrum without
        // the audio runtime (subscriber list + manager entry are independent).
        if self.cmd_tx.send(Command::SetVrxHighResSpectrum(id, hr, span_khz)).is_ok() {
            match ch { VrxChannel::Vrx1 => self.vrx1_high_res_spectrum = hr, VrxChannel::Vrx2 => self.vrx2_high_res_spectrum = hr };
            match ch { VrxChannel::Vrx1 => self.vrx1_high_res_last_span_khz = span_khz, VrxChannel::Vrx2 => self.vrx2_high_res_last_span_khz = span_khz };
            if hr {
                // Spectrum on → open the detached VRX window of THIS channel so the
                // spectrum actually becomes visible (VRX spectrum only shows there).
                // Window opens via the model-B derivation (want=high_res && can_vrx);
                // here we only reset init_applied so saved geometry re-applies on reopen.
                match ch {
                    VrxChannel::Vrx1 => { self.vrx_popout_init_applied = false; }
                    VrxChannel::Vrx2 => { self.vrx2_popout_init_applied = false; }
                }
            } else {
                // Spectrum off: clear the buffer. The window closes via derivation
                // (want=high_res drops); no imperative popout write needed.
                match ch {
                    VrxChannel::Vrx1 => { self.vrx1_spectrum.clear(); }
                    VrxChannel::Vrx2 => { self.vrx2_spectrum.clear(); }
                }
            }
            self.save_full_config();
        }
    }

    /// RX-independent DDC bandwidth (Hz) of the channel this VRX
    /// runs on — the reference width for the high-res span request. Comes from the
    /// DDC sample rate (always known once connected), NOT from the
    /// RX spectrum. This way the VRX zoom also works with the RX spectrum off (§6.2,
    /// coupling #4/#6). Fallback 384 kHz matches the existing DDC defaults.
    pub(super) fn vrx_ddc_span_hz(&self, ch: VrxChannel) -> u32 {
        let sr_khz = match ch {
            VrxChannel::Vrx1 => self.ddc_sample_rate_rx1,
            VrxChannel::Vrx2 => self.ddc_sample_rate_rx2,
        };
        if sr_khz > 0 { sr_khz as u32 * 1000 } else { 384_000 }
    }

    /// Lowest/highest frequency a VRX can actually be listened to, given the
    /// DDC window it lives in and its own filter width.
    ///
    /// This mirrors the server's guard exactly (`DDC_USABLE_FRACTION` in
    /// `vrx-rs/src/runtime.rs`): the outer tenth of the DDC band is the DDC's
    /// own roll-off, and the channel's far filter edge has to fit inside the
    /// remainder. The server holds the channel at that boundary, so the UI must
    /// stop there too - otherwise the readout and the spectrum keep travelling
    /// while the audio stands still, which is exactly what it sounds like.
    pub(super) fn vrx_tune_limits(&self, ch: VrxChannel, ddc_center: u64) -> (u64, u64) {
        const DDC_USABLE_FRACTION: f64 = 0.9;
        // Width comes from the spectrum packets the server actually sends, not
        // from `vrx_ddc_span_hz()`: that guesses 384 kHz while the DDC rate is
        // still unknown, and a guess wider than the real band re-opens exactly
        // the gap build 61 closed - tuning past where the server listens, silently.
        // Only when no packet has arrived yet do we fall back, and then to the
        // narrower of the two so the limit errs inward.
        let packet_span = match ch {
            VrxChannel::Vrx1 => self.full_spectrum_span_hz,
            VrxChannel::Vrx2 => self.rx2_full_spectrum_span_hz,
        } as f64;
        let span = if packet_span > 0.0 {
            packet_span
        } else {
            packet_span.max(0.0).max(self.vrx_ddc_span_hz(ch) as f64)
        };
        let (lo_hz, hi_hz) = match ch {
            VrxChannel::Vrx1 => (self.vrx1_filter_low_hz, self.vrx1_filter_high_hz),
            VrxChannel::Vrx2 => (self.vrx2_filter_low_hz, self.vrx2_filter_high_hz),
        };
        let filter_edge = lo_hz.abs().max(hi_hz.abs()) as f64;
        let reach = (span * 0.5 * DDC_USABLE_FRACTION - filter_edge).max(0.0) as u64;
        (ddc_center.saturating_sub(reach), ddc_center.saturating_add(reach))
    }


    /// Shared renderer for ONE VRX spectrum panel (VRX1 or VRX2): the
    /// Ref/Auto/Range and Zoom/Pan/WF button rows + the spectrum/waterfall strip.
    /// Parity by construction: VRX1 and VRX2 share exactly this code (per
    /// docs/internal/UI-STYLE-GUIDE.md). Spectrum display settings are local
    /// (no server command); click-to-tune follows the dispatch discipline (Decision 8).
    pub(super) fn render_vrx_spectrum_panel(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        ch: VrxChannel,
        vrx_min: u64,
        vrx_max: u64,
    ) {
        let connected = self.connected;
        let rx_label = match ch { VrxChannel::Vrx1 => "RX1", VrxChannel::Vrx2 => "RX2" };
        let allow_zoom_below_2x = self.allow_zoom_below_2x;
        let fft_size_k = self.spectrum_fft_size_k;
        // No ui.group() frame: RX spectrum panels don't have one either (parity).
        {
            // Row 1: Ref + Auto + Range
            ui.horizontal(|ui| {
                ui.spacing_mut().slider_width = theme::TL_SLIDER_WIDTH;
                ui.label(rust_i18n::t!("main_ref_label").to_string());
                let mut changed = false;
                let auto = match ch { VrxChannel::Vrx1 => self.vrx1_auto_ref, VrxChannel::Vrx2 => self.vrx2_auto_ref };
                if auto {
                    let mut disp = match ch { VrxChannel::Vrx1 => self.vrx1_ref_db, VrxChannel::Vrx2 => self.vrx2_ref_db };
                    ui.add_enabled(false, egui::Slider::new(&mut disp, -90.0..=0.0).suffix(" dB").step_by(5.0))
                        .on_hover_text(rust_i18n::t!("main_hover_ref").to_string());
                } else {
                    let resp = ui.add(egui::Slider::new(
                        match ch { VrxChannel::Vrx1 => &mut self.vrx1_ref_db, VrxChannel::Vrx2 => &mut self.vrx2_ref_db },
                        -90.0..=0.0).suffix(" dB").step_by(5.0))
                        .on_hover_text(rust_i18n::t!("main_hover_ref").to_string());
                    let scrolled = helpers::slider_wheel(ui, &resp,
                        match ch { VrxChannel::Vrx1 => &mut self.vrx1_ref_db, VrxChannel::Vrx2 => &mut self.vrx2_ref_db },
                        -90.0..=0.0, 5.0);
                    if resp.changed() || scrolled {
                        changed = true;
                    }
                }
                if ui.checkbox(
                        match ch { VrxChannel::Vrx1 => &mut self.vrx1_auto_ref, VrxChannel::Vrx2 => &mut self.vrx2_auto_ref },
                        rust_i18n::t!("main_auto").to_string())
                    .on_hover_text(rust_i18n::t!("main_hover_auto_ref").to_string())
                    .changed()
                {
                    match ch {
                        VrxChannel::Vrx1 => if self.vrx1_auto_ref { self.vrx1_spectrum.reset_auto_ref(); },
                        VrxChannel::Vrx2 => if self.vrx2_auto_ref { self.vrx2_spectrum.reset_auto_ref(); },
                    }
                    self.save_full_config();
                }
                ui.label(rust_i18n::t!("main_range_label").to_string());
                let resp = ui.add(egui::Slider::new(
                        match ch { VrxChannel::Vrx1 => &mut self.vrx1_range_db, VrxChannel::Vrx2 => &mut self.vrx2_range_db },
                        20.0..=130.0).suffix(" dB").step_by(5.0))
                    .on_hover_text(rust_i18n::t!("main_hover_range").to_string());
                let scrolled = helpers::slider_wheel(ui, &resp,
                    match ch { VrxChannel::Vrx1 => &mut self.vrx1_range_db, VrxChannel::Vrx2 => &mut self.vrx2_range_db },
                    20.0..=130.0, 5.0);
                if resp.changed() || scrolled {
                    match ch {
                        VrxChannel::Vrx1 => if self.vrx1_auto_ref { self.vrx1_spectrum.reset_auto_ref(); },
                        VrxChannel::Vrx2 => if self.vrx2_auto_ref { self.vrx2_spectrum.reset_auto_ref(); },
                    }
                    changed = true;
                }
                if changed { self.save_full_config(); }
            });
            // Row 2: Zoom + Allow<2x (disabled stub) + Pan + WF + FFT (disabled stub)
            ui.horizontal(|ui| {
                ui.spacing_mut().slider_width = theme::TL_SLIDER_WIDTH;
                let mut changed = false;
                ui.label(rust_i18n::t!("main_zoom_label").to_string());
                let zoom_min: f32 = if allow_zoom_below_2x { 1.0 } else { 2.0 };
                match ch {
                    VrxChannel::Vrx1 => if self.vrx1_spectrum_zoom < zoom_min { self.vrx1_spectrum_zoom = zoom_min; },
                    VrxChannel::Vrx2 => if self.vrx2_spectrum_zoom < zoom_min { self.vrx2_spectrum_zoom = zoom_min; },
                }
                let resp = ui.add(egui::Slider::new(
                        match ch { VrxChannel::Vrx1 => &mut self.vrx1_spectrum_zoom, VrxChannel::Vrx2 => &mut self.vrx2_spectrum_zoom },
                        zoom_min..=1024.0).logarithmic(true).custom_formatter(|v, _| format!("{:.0}x", v)))
                    .on_hover_text(rust_i18n::t!("main_hover_zoom").to_string());
                let zoom_cur = match ch { VrxChannel::Vrx1 => self.vrx1_spectrum_zoom, VrxChannel::Vrx2 => self.vrx2_spectrum_zoom };
                let scrolled = helpers::slider_wheel(ui, &resp,
                    match ch { VrxChannel::Vrx1 => &mut self.vrx1_spectrum_zoom, VrxChannel::Vrx2 => &mut self.vrx2_spectrum_zoom },
                    zoom_min..=1024.0, (zoom_cur as f64 * 0.1).max(1.0));
                if resp.changed() || scrolled {
                    match ch {
                        VrxChannel::Vrx1 => { let mp = (0.5 - 0.5 / self.vrx1_spectrum_zoom) * 0.05; self.vrx1_pan = self.vrx1_pan.clamp(-mp, mp); self.vrx1_zoom_initialized = true; }
                        VrxChannel::Vrx2 => { let mp = (0.5 - 0.5 / self.vrx2_spectrum_zoom) * 0.05; self.vrx2_pan = self.vrx2_pan.clamp(-mp, mp); self.vrx2_zoom_initialized = true; }
                    }
                    changed = true;
                }
                // Same shared client flag as the RX panel (RX1/RX2/VRX share
                // it). Now also settable here so the VRX is usable standalone
                // without the RX spectrum panel alongside (§6, each channel independent).
                if ui.checkbox(&mut self.allow_zoom_below_2x, rust_i18n::t!("main_allow_zoom_2x").to_string())
                    .on_hover_text(rust_i18n::t!("hover_zoom_below_2x").to_string())
                    .changed()
                {
                    crate::ui::config::save_allow_zoom_below_2x(self.allow_zoom_below_2x);
                    let _ = self.cmd_tx.send(Command::SetControl(
                        sdr_remote_core::protocol::ControlId::AllowZoomBelow2x,
                        if self.allow_zoom_below_2x { 1 } else { 0 },
                    ));
                }
                ui.label(rust_i18n::t!("main_pan_label").to_string());
                let zoom_now = match ch { VrxChannel::Vrx1 => self.vrx1_spectrum_zoom, VrxChannel::Vrx2 => self.vrx2_spectrum_zoom };
                let max_pan = if zoom_now > 1.01 { (0.5 - 0.5 / zoom_now) * 0.05 } else { 0.0 };
                let pan_resp = ui.add(egui::Slider::new(
                        match ch { VrxChannel::Vrx1 => &mut self.vrx1_pan, VrxChannel::Vrx2 => &mut self.vrx2_pan },
                        -max_pan..=max_pan).custom_formatter(|v, _| format!("{:+.2}", v)))
                    .on_hover_text(rust_i18n::t!("main_hover_pan").to_string());
                let pan_scrolled = helpers::slider_wheel(ui, &pan_resp,
                    match ch { VrxChannel::Vrx1 => &mut self.vrx1_pan, VrxChannel::Vrx2 => &mut self.vrx2_pan },
                    -max_pan..=max_pan, (max_pan as f64 * 0.1).max(0.0001));
                changed |= pan_resp.changed() || pan_scrolled;
                ui.label(rust_i18n::t!("main_wf_label").to_string());
                let wf_resp = ui.add(egui::Slider::new(
                        match ch { VrxChannel::Vrx1 => &mut self.vrx1_wf_contrast, VrxChannel::Vrx2 => &mut self.vrx2_wf_contrast },
                        0.3..=3.0).logarithmic(true).custom_formatter(|v, _| format!("{:.1}", v)))
                    .on_hover_text(rust_i18n::t!("main_hover_wf").to_string());
                let wf_scrolled = helpers::slider_wheel(ui, &wf_resp,
                    match ch { VrxChannel::Vrx1 => &mut self.vrx1_wf_contrast, VrxChannel::Vrx2 => &mut self.vrx2_wf_contrast },
                    0.3..=3.0, 0.1);
                changed |= wf_resp.changed() || wf_scrolled;
                let ddc_sr = match ch { VrxChannel::Vrx1 => self.ddc_sample_rate_rx1, VrxChannel::Vrx2 => self.ddc_sample_rate_rx2 };
                let ddc_rate = if ddc_sr > 0 { ddc_sr as u32 * 1000 } else { 384_000 };
                let auto_k = sdr_remote_core::ddc_fft_size(ddc_rate) / 1024;
                let fft_label = if fft_size_k == 0 { format!("FFT: Auto ({}K)", auto_k) } else { format!("FFT: {}K", fft_size_k) };
                ui.add_enabled(false, egui::Button::new(&fft_label))
                    .on_disabled_hover_text(rust_i18n::t!("main_hover_server_wide_fft", rx = rx_label).to_string());
                if changed { self.save_full_config(); }
            });
            ui.separator();
            let remaining = ui.available_height();
            // Plot and waterfall equal height (operator wish); 16 px = the label strip in the
            // spectrum, TL_INNER_GAP_Y = space between plot and waterfall.
            let spec_h = ((remaining - 16.0 - theme::TL_INNER_GAP_Y) / 2.0).max(40.0);
            let wf_h = spec_h;
            let new_freq = match ch {
                VrxChannel::Vrx1 => {
                    // VRX renders EXCLUSIVELY its own extracted spectrum (§6.1
                    // #1): no more RX fallback. Empty bins → placeholder in the
                    // renderer. extracted_mode = true: bins map 1:1 onto the view.
                    let (b, c, s) = (
                        self.vrx1_spectrum.bins(),
                        self.vrx1_spectrum.center_hz(),
                        self.vrx1_spectrum.span_hz(),
                    );
                    spectrum::render_vrx_strip(
                        ui, ctx, "vrx1",
                        self.vrx1_freq_hz, self.vrx1_spectrum_zoom, self.vrx1_pan,
                        self.vrx1_ref_db, self.vrx1_range_db, self.vrx1_wf_contrast,
                        spec_h, wf_h, self.vrx1_mode == 1,
                        self.vrx1_filter_low_hz, self.vrx1_filter_high_hz,
                        self.vrx1_spectrum.smeter_dbm(), self.vrx1_enabled,
                        b, c, s, s, true,
                        self.vrx1_spectrum.waterfall(),
                        &mut self.vrx1_waterfall_texture, vrx_min, vrx_max,
                    )
                }
                VrxChannel::Vrx2 => {
                    let (b, c, s) = (
                        self.vrx2_spectrum.bins(),
                        self.vrx2_spectrum.center_hz(),
                        self.vrx2_spectrum.span_hz(),
                    );
                    spectrum::render_vrx_strip(
                        ui, ctx, "vrx2",
                        self.vrx2_freq_hz, self.vrx2_spectrum_zoom, self.vrx2_pan,
                        self.vrx2_ref_db, self.vrx2_range_db, self.vrx2_wf_contrast,
                        spec_h, wf_h, self.vrx2_mode == 1,
                        self.vrx2_filter_low_hz, self.vrx2_filter_high_hz,
                        self.vrx2_spectrum.smeter_dbm(), self.vrx2_enabled,
                        b, c, s, s, true,
                        self.vrx2_spectrum.waterfall(),
                        &mut self.vrx2_waterfall_texture, vrx_min, vrx_max,
                    )
                }
            };
            if let Some(f) = new_freq {
                if connected && self.vrx_send_freq(ch, f) {
                    match ch { VrxChannel::Vrx1 => self.vrx1_freq_hz = f, VrxChannel::Vrx2 => self.vrx2_freq_hz = f };
                    self.save_full_config();
                }
            }
        }
    }
}
