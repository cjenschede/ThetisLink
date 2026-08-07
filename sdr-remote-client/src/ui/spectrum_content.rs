// SPDX-License-Identifier: GPL-2.0-or-later
//! `SdrRemoteApp::render_spectrum_content`: the shared RX1 spectrum panel - the
//! Ref/Auto/Range + Zoom/Pan/WF control rows and the spectrum/waterfall strip - used
//! by both the inline main-tab view and the detached spectrum pop-out. Extracted
//! verbatim from `ui/mod.rs` - pure relocation, no behaviour change. `pub(super)`
//! keeps it callable from the parent module tree (update/popouts).

use super::*;

impl SdrRemoteApp {
    /// Render spectrum controls + plot + waterfall (used by both inline and pop-out)
    pub(super) fn render_spectrum_content(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, reserve_bottom: f32, is_popout: bool) {
        // Row 1: Ref + Auto checkbox + Range
        ui.horizontal(|ui| {
            ui.spacing_mut().slider_width = 80.0;
            ui.label(rust_i18n::t!("main_ref_label").to_string());
            if self.auto_ref_enabled {
                // Show value but non-interactive
                let mut display_val = self.spectrum_ref_db;
                ui.add_enabled(false, egui::Slider::new(&mut display_val, -90.0..=0.0)
                    .suffix(" dB")
                    .step_by(5.0)
                );
            } else {
                let resp = ui.add(egui::Slider::new(&mut self.spectrum_ref_db, -90.0..=0.0)
                    .suffix(" dB")
                    .step_by(5.0)
                ).on_hover_text(rust_i18n::t!("main_hover_ref").to_string());
                let scrolled = helpers::slider_wheel(ui, &resp, &mut self.spectrum_ref_db, -90.0..=0.0, 5.0);
                if resp.changed() || scrolled {
                    self.save_full_config();
                }
            }
            if ui.checkbox(&mut self.auto_ref_enabled, rust_i18n::t!("main_auto").to_string()).on_hover_text(rust_i18n::t!("main_hover_auto_ref").to_string()).changed() {
                if self.auto_ref_enabled {
                    self.rx1_spectrum.reset_auto_ref();
                }
                self.save_full_config();
            }
            ui.label(rust_i18n::t!("main_range_label").to_string());
            let resp = ui.add(egui::Slider::new(&mut self.spectrum_range_db, 20.0..=130.0)
                .suffix(" dB")
                .step_by(5.0)
            ).on_hover_text(rust_i18n::t!("main_hover_range").to_string());
            let scrolled = helpers::slider_wheel(ui, &resp, &mut self.spectrum_range_db, 20.0..=130.0, 5.0);
            if resp.changed() || scrolled {
                if self.auto_ref_enabled {
                    self.rx1_spectrum.reset_auto_ref();
                }
                self.save_full_config();
            }
        });
        // Row 2: Zoom + Pan + WF Contrast
        ui.horizontal(|ui| {
            ui.spacing_mut().slider_width = 80.0;
            ui.label(rust_i18n::t!("main_zoom_label").to_string());
            // TL2-1 ctun-auto-recenter: zoom-min = 2.0 default (anti-smear feature workable);
            // 1.0 allowed via setup checkbox "Allow zoom <2x" (smear trade-off).
            let zoom_min: f32 = if self.allow_zoom_below_2x { 1.0 } else { 2.0 };
            // Clamp current zoom to new minimum (when the checkbox toggles off)
            if self.spectrum_zoom < zoom_min {
                self.spectrum_zoom = zoom_min;
            }
            let zoom_resp = ui.add(egui::Slider::new(&mut self.spectrum_zoom, zoom_min..=1024.0)
                .logarithmic(true)
                .custom_formatter(|v, _| format!("{:.0}x", v))
            ).on_hover_text(rust_i18n::t!("main_hover_zoom").to_string());
            let zoom_step = (self.spectrum_zoom as f64 * 0.1).max(1.0);
            let zoom_scrolled = helpers::slider_wheel(ui, &zoom_resp, &mut self.spectrum_zoom, zoom_min..=1024.0, zoom_step);
            let zoom_changed = zoom_resp.changed() || zoom_scrolled;
            if zoom_changed {
                let max_pan = (0.5 - 0.5 / self.spectrum_zoom) * 0.05;
                self.spectrum_pan = self.spectrum_pan.clamp(-max_pan, max_pan);
            }
            // TL2-1 ctun-auto-recenter setup checkbox. Persist + push to server on toggle.
            // Server enforces strictest: as long as one client has it off, the server clamps to 2x.
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
            let max_pan = if self.spectrum_zoom > 1.01 { (0.5 - 0.5 / self.spectrum_zoom) * 0.05 } else { 0.0 };
            let pan_resp = ui.add(egui::Slider::new(&mut self.spectrum_pan, -max_pan..=max_pan)
                .custom_formatter(|v, _| format!("{:+.2}", v))
            ).on_hover_text(rust_i18n::t!("main_hover_pan").to_string());
            let pan_scrolled = helpers::slider_wheel(ui, &pan_resp, &mut self.spectrum_pan, -max_pan..=max_pan, (max_pan as f64 * 0.1).max(0.0001));
            let pan_changed = pan_resp.changed() || pan_scrolled;
            ui.label(rust_i18n::t!("main_wf_label").to_string());
            let wf_resp = ui.add(egui::Slider::new(&mut self.waterfall_contrast, 0.3..=3.0)
                .logarithmic(true)
                .custom_formatter(|v, _| format!("{:.1}", v))
            ).on_hover_text(rust_i18n::t!("main_hover_wf").to_string());
            let wf_scrolled = helpers::slider_wheel(ui, &wf_resp, &mut self.waterfall_contrast, 0.3..=3.0, 0.1);
            if wf_resp.changed() || wf_scrolled {
                // Update per-band storage
                if let Some(ref band) = self.current_band {
                    self.wf_contrast_per_band.insert(band.clone(), self.waterfall_contrast);
                }
                self.save_full_config();
            }

            // FFT size selector - labels computed from current DDC sample rate
            let ddc_rate = if self.ddc_sample_rate_rx1 > 0 { self.ddc_sample_rate_rx1 as u32 * 1000 } else { 384_000 };
            let auto_fft = sdr_remote_core::ddc_fft_size(ddc_rate);
            let auto_k = auto_fft / 1024;
            let fft_label = if self.spectrum_fft_size_k == 0 {
                format!("FFT: Auto ({}K)", auto_k)
            } else {
                format!("FFT: {}K", self.spectrum_fft_size_k)
            };
            // Build options: Auto + fixed sizes that make sense for this sample rate
            let hop = |fft_k: u32| -> u32 { let fft = fft_k * 1024; ddc_rate / (fft / 8) };
            let options: Vec<(u16, String)> = {
                let mut opts = vec![(0u16, format!("Auto ({}K, ~{} FFT/s)", auto_k, hop(auto_k as u32)))];
                for &k in &[32u16, 64, 128, 256, 512, 1024] {
                    let fft = k as u32 * 1024;
                    if fft <= ddc_rate * 4 { // reasonable range
                        let fps = hop(k as u32);
                        if fps > 0 {
                            opts.push((k, format!("{}K (~{} FFT/s)", k, fps)));
                        }
                    }
                }
                opts
            };
            egui::ComboBox::from_id_salt("fft_size")
                .selected_text(&fft_label)
                .width(80.0)
                .show_ui(ui, |ui| {
                    for (k, label) in &options {
                        if ui.selectable_label(self.spectrum_fft_size_k == *k, label).clicked() {
                            self.spectrum_fft_size_k = *k;
                            let _ = self.cmd_tx.send(Command::SetSpectrumFftSize(*k));
                            self.save_full_config();
                        }
                    }
                });
            // Height slider - only in the main Radio tab. Popouts fill the
            // whole window and ignore this setting.
            if !is_popout {
                ui.label("H:");
                let resp = ui.add(egui::Slider::new(&mut self.spectrum_total_h, 300.0..=1200.0)
                    .custom_formatter(|v, _| format!("{:.0}", v))
                ).on_hover_text(rust_i18n::t!("main_hover_spectrum_height").to_string());
                let scrolled = helpers::slider_wheel(ui, &resp, &mut self.spectrum_total_h, 300.0..=1200.0, 20.0);
                if resp.changed() || scrolled {
                    self.save_full_config();
                }
            }
            if zoom_changed || pan_changed {
                self.zoom_pan_changed_at = Some(Instant::now());
            }
        });

        // Debounce: send zoom/pan + dynamic bins to server after 100ms stability
        if let Some(changed_at) = self.zoom_pan_changed_at {
            if changed_at.elapsed().as_millis() >= 100 {
                let zoom_diff = (self.spectrum_zoom - self.last_sent_zoom).abs();
                let pan_diff = (self.spectrum_pan - self.last_sent_pan).abs();
                if zoom_diff > 0.01 {
                    let _ = self.cmd_tx.send(Command::SetSpectrumZoom(self.spectrum_zoom));
                    self.last_sent_zoom = self.spectrum_zoom;
                }
                if pan_diff > 0.001 {
                    let _ = self.cmd_tx.send(Command::SetSpectrumPan(self.spectrum_pan));
                    self.last_sent_pan = self.spectrum_pan;
                }
                // Dynamic bins: screen_width × zoom, capped at MAX_SPECTRUM_SEND_BINS
                let pixel_width = ui.available_width().max(100.0) as u32;
                let dynamic_bins = ((pixel_width as f32 * self.spectrum_zoom) as u32)
                    .clamp(512, sdr_remote_core::MAX_SPECTRUM_SEND_BINS as u32) as u16;
                if dynamic_bins != self.spectrum_max_bins {
                    self.spectrum_max_bins = dynamic_bins;
                    let _ = self.cmd_tx.send(Command::SetSpectrumMaxBins(dynamic_bins));
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Rx2SpectrumMaxBins, dynamic_bins));
                }
                self.zoom_pan_changed_at = None;
            }
        }

        // Smooth display center: interpolate toward target for smooth tuning
        let target_center = Self::spectrum_target_center_hz(
            self.frequency_hz,
            self.full_spectrum_span_hz,
            self.spectrum_pan,
            self.spectrum_center_hz,
        );
        let rx1_tuning_active = Self::tuning_latch_active(
            self.rx1_force_full_tuning,
            self.pending_freq,
            self.pending_freq_at,
        );
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame_time).as_secs_f64();
        self.last_frame_time = now;
        // Exponential smoothing: ~90% of the way in ~50ms (alpha = 1 - e^(-dt/tau))
        let tau = 0.02; // 20ms time constant - fast but smooth
        let alpha = (1.0 - (-dt / tau).exp()).clamp(0.0, 1.0);
        self.smooth_alpha = alpha;
        if self.pending_freq.is_some() {
            self.smooth_display_center_hz = target_center.round();
        } else if self.smooth_display_center_hz == 0.0 {
            self.smooth_display_center_hz = target_center;
        } else {
            self.smooth_display_center_hz += (target_center - self.smooth_display_center_hz) * alpha;
        }
        // Snap when very close (< 1 Hz) to avoid perpetual drift
        if (self.smooth_display_center_hz - target_center).abs() < 1.0 {
            self.smooth_display_center_hz = target_center;
        }
        let smooth_center = self.smooth_display_center_hz as u64;
        // VFO marker follows the smooth center (minus pan offset) - stays perfectly stationary
        let smooth_vfo = (self.smooth_display_center_hz
            - self.spectrum_pan as f64 * self.full_spectrum_span_hz as f64) as u64;

        // Spectrum + waterfall area sizing.
        // - Popout: fills the popout window (dynamic from available_height).
        // - Main Radio tab: fixed `self.spectrum_total_h` so the rest of the
        //   tab (Diversity etc.) can expand below into a scrollable area
        //   instead of pushing the spectrum off-screen.
        let spec_area = if is_popout {
            let available = ui.available_height();
            (available - reserve_bottom).max(200.0)
        } else {
            self.spectrum_total_h.clamp(300.0, 1200.0)
        };
        let spec_h = (spec_area * 0.50).max(100.0);
        let wf_h = (spec_area * 0.50).max(80.0);

        // The tuning latch falls back to the full row because the extracted view
        // lags while retuning. With the row switched off there is nothing to fall
        // back to, so the view is drawn throughout - as on VRX.
        let (plot_bins, plot_center_hz, plot_span_hz) = if !rx1_tuning_active || self.full_spectrum_bins.is_empty() {
            (&self.spectrum_bins, self.spectrum_center_hz, self.spectrum_span_hz)
        } else {
            (&self.full_spectrum_bins, self.full_spectrum_center_hz, self.full_spectrum_span_hz)
        };

        spectrum_plot(
            ui,
            plot_bins,
            plot_center_hz,
            plot_span_hz,
            smooth_center,
            smooth_vfo,
            self.frequency_hz,
            self.spectrum_ref_db,
            self.spectrum_range_db,
            self.smeter,
            self.ptt,
            self.other_tx,
            self.filter_low_hz,
            self.filter_high_hz,
            self.rit_offset as i32,
            self.rit_enable,
            spec_h,
            &SpectrumPlotConfig { is_popout, ..RX1_PLOT_CONFIG },
            &self.dx_spots,
        );
        render_waterfall(
            ui,
            ctx,
            &mut self.waterfall,
            if self.full_spectrum_enabled { self.full_spectrum_span_hz } else { self.spectrum_span_hz },
            smooth_center,
            self.frequency_hz,
            self.spectrum_zoom,
            self.waterfall_contrast,
            self.spectrum_ref_db,
            self.spectrum_range_db,
            wf_h,
            &SpectrumPlotConfig { is_popout, ..RX1_PLOT_CONFIG },
        );
        // Drag-handle for resizing spectrum + waterfall height in the main
        // Radio tab. Popouts skip - they always fill their window.
        if !is_popout {
            let handle_h = 6.0;
            let (rect, response) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), handle_h),
                egui::Sense::drag(),
            );
            let visuals = ui.visuals();
            let fill = if response.hovered() || response.dragged() {
                visuals.widgets.hovered.bg_fill
            } else {
                visuals.widgets.inactive.bg_fill
            };
            ui.painter().rect_filled(rect, 2.0, fill);
            // Small visual "grip" - three short horizontal lines centred.
            let grip_color = visuals.widgets.inactive.fg_stroke.color;
            let cx = rect.center().x;
            let cy = rect.center().y;
            for dx in [-12.0, 0.0, 12.0] {
                ui.painter().line_segment(
                    [egui::pos2(cx + dx - 4.0, cy), egui::pos2(cx + dx + 4.0, cy)],
                    egui::Stroke::new(1.0, grip_color),
                );
            }
            if response.dragged() {
                let dy = response.drag_delta().y;
                if dy.abs() > 0.01 {
                    self.spectrum_total_h = (self.spectrum_total_h + dy).clamp(300.0, 1200.0);
                }
            }
            if response.drag_stopped() {
                self.save_full_config();
            }
            if response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
            }
        }
    }
}
