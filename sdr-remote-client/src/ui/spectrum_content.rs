// SPDX-License-Identifier: GPL-2.0-or-later
//! `SdrRemoteApp::render_spectrum_content`: the shared RX1 spectrum panel - the
//! Ref/Auto/Range + Zoom/Pan/WF control rows and the spectrum/waterfall strip - used
//! by both the inline main-tab view and the detached spectrum pop-out. Extracted
//! verbatim from `ui/mod.rs` - pure relocation, no behaviour change. `pub(super)`
//! keeps it callable from the parent module tree (update/popouts).

use super::*;

impl SdrRemoteApp {
    /// The "Zoom <2x" flag changed in a panel's rows: one flag for RX1, RX2 and
    /// VRX (build 242). TL2-1 ctun-auto-recenter setup: persisted and pushed to
    /// the server; the server enforces the strictest - as long as one client
    /// has it off, it clamps to 2x.
    pub(super) fn zoom_below_2x_changed(&mut self) {
        crate::ui::config::save_allow_zoom_below_2x(self.allow_zoom_below_2x);
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::AllowZoomBelow2x,
            if self.allow_zoom_below_2x { 1 } else { 0 },
        ));
    }

    /// What follows a change of RX1's (`rx2` false) or RX2's view, from its
    /// rows or from MIDI: one place, so both keep the same rules.
    pub(super) fn rx_view_changed(&mut self, rx2: bool, changes: super::spectrum::ViewChanges) {
        if rx2 {
            if changes.restart_auto_ref(self.rx2_auto_ref_enabled) {
                self.rx2_spectrum.reset_auto_ref();
            }
            if let Some(k) = changes.fft {
                let _ = self.cmd_tx.send(Command::SetControl(ControlId::Rx2SpectrumFftSize, k));
            }
            if changes.zoom || changes.pan {
                self.rx2_zoom_pan_changed_at = Some(Instant::now());
            }
        } else {
            if changes.restart_auto_ref(self.auto_ref_enabled) {
                self.rx1_spectrum.reset_auto_ref();
            }
            if let Some(k) = changes.fft {
                let _ = self.cmd_tx.send(Command::SetSpectrumFftSize(k));
            }
            if changes.wf_contrast {
                if let Some(ref band) = self.current_band {
                    self.wf_contrast_per_band.insert(band.clone(), self.waterfall_contrast);
                }
            }
            if changes.zoom || changes.pan {
                self.zoom_pan_changed_at = Some(Instant::now());
            }
        }
        if changes.allow_zoom_below_2x {
            self.zoom_below_2x_changed();
        }
        if changes.settings() {
            self.save_full_config();
        }
    }

    /// A zoom from MIDI, inside the rows' limits; the pan follows into what
    /// the new zoom allows, as in the rows.
    pub(super) fn midi_set_zoom(&mut self, rx2: bool, zoom: f32) {
        let zooms = super::spectrum::zoom_range(self.allow_zoom_below_2x);
        let zoom = zoom.clamp(*zooms.start(), *zooms.end());
        let max_pan = super::spectrum::pan_limit(zoom);
        let (z, p) = if rx2 { (&mut self.rx2_spectrum_zoom, &mut self.rx2_spectrum_pan) } else { (&mut self.spectrum_zoom, &mut self.spectrum_pan) };
        *z = zoom;
        *p = p.clamp(-max_pan, max_pan);
        self.rx_view_changed(rx2, super::spectrum::ViewChanges { zoom: true, ..Default::default() });
    }

    /// A pan from MIDI, inside what the zoom allows.
    pub(super) fn midi_set_pan(&mut self, rx2: bool, pan: f32) {
        let (z, p) = if rx2 { (self.rx2_spectrum_zoom, &mut self.rx2_spectrum_pan) } else { (self.spectrum_zoom, &mut self.spectrum_pan) };
        let max_pan = super::spectrum::pan_limit(z);
        *p = pan.clamp(-max_pan, max_pan);
        self.rx_view_changed(rx2, super::spectrum::ViewChanges { pan: true, ..Default::default() });
    }

    /// A reference level from MIDI, inside the rows' limits; it turns Auto off.
    pub(super) fn midi_set_ref(&mut self, rx2: bool, ref_db: f32) {
        let (r, auto) = if rx2 { (&mut self.rx2_spectrum_ref_db, &mut self.rx2_auto_ref_enabled) } else { (&mut self.spectrum_ref_db, &mut self.auto_ref_enabled) };
        *r = ref_db.clamp(*super::spectrum::REF_DB_RANGE.start(), *super::spectrum::REF_DB_RANGE.end());
        let was_auto = std::mem::replace(auto, false);
        self.rx_view_changed(rx2, super::spectrum::ViewChanges { ref_db: true, auto_ref: was_auto, ..Default::default() });
    }

    /// A waterfall contrast from MIDI, inside the rows' limits.
    pub(super) fn midi_set_contrast(&mut self, rx2: bool, contrast: f32) {
        let c = if rx2 { &mut self.rx2_waterfall_contrast } else { &mut self.waterfall_contrast };
        *c = contrast.clamp(*super::spectrum::WF_CONTRAST_RANGE.start(), *super::spectrum::WF_CONTRAST_RANGE.end());
        self.rx_view_changed(rx2, super::spectrum::ViewChanges { wf_contrast: true, ..Default::default() });
    }

    /// Render spectrum controls + plot + waterfall (used by both inline and pop-out)
    pub(super) fn render_spectrum_content(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, reserve_bottom: f32, is_popout: bool) {
        // The view rows: the one set every spectrum panel has.
        let ddc_rate = if self.ddc_sample_rate_rx1 > 0 { self.ddc_sample_rate_rx1 as u32 * 1000 } else { 384_000 };
        let changes = super::spectrum::spectrum_view_rows(ui, super::spectrum::ViewControls {
            ref_db: &mut self.spectrum_ref_db,
            auto_ref: &mut self.auto_ref_enabled,
            range_db: &mut self.spectrum_range_db,
            fill_percent: &mut self.spectrum_fill_percent,
            zoom: &mut self.spectrum_zoom,
            allow_zoom_below_2x: &mut self.allow_zoom_below_2x,
            pan: &mut self.spectrum_pan,
            wf_contrast: &mut self.waterfall_contrast,
            fft: super::spectrum::FftControl::Choose { size_k: &mut self.spectrum_fft_size_k, ddc_rate_hz: ddc_rate, salt: "fft_size" },
            // Only in the main Radio tab; a window of its own fills itself.
            height: if is_popout { None } else { Some(&mut self.spectrum_total_h) },
        });
        self.rx_view_changed(false, changes);

        // Debounce: send zoom/pan + dynamic bins to server after 100ms stability
        if let Some(changed_at) = self.zoom_pan_changed_at {
            if changed_at.elapsed().as_millis() >= 100 {
                let zoom_diff = (self.spectrum_zoom - self.last_sent_zoom).abs();
                // Every change the server can see, however small: a pan below a
                // threshold was drawn here and never reached the server.
                let pan_moved = sdr_remote_core::protocol::pack_spectrum_pan_fine(self.spectrum_pan)
                    != sdr_remote_core::protocol::pack_spectrum_pan_fine(self.last_sent_pan);
                if zoom_diff > 0.01 {
                    let _ = self.cmd_tx.send(Command::SetSpectrumZoom(self.spectrum_zoom));
                    self.last_sent_zoom = self.spectrum_zoom;
                    self.view_sent_at = Some(Instant::now());
                }
                if pan_moved {
                    let _ = self.cmd_tx.send(Command::SetSpectrumPan(self.spectrum_pan));
                    self.last_sent_pan = self.spectrum_pan;
                    self.view_sent_at = Some(Instant::now());
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
            self.rx1_full_span_hz(),
            self.spectrum_pan,
            self.server_fine_spectrum_view,
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
        // The VFO marker is the VFO, smoothed on the same clock as the centre so
        // the two travel together while tuning. It used to be worked out as
        // "centre minus the pan offset", which is the VFO only while the full
        // span is known - and that value is filled in only once the full-band
        // row has been on. Without it the marker landed on the middle of the
        // view and stayed there while the spectrum panned underneath it.
        //
        // The marker is the VFO. Always, with no second case - and the way the
        // second case came back is worth the paragraph.
        //
        // Build 80 read the marker off the view instead while retuning, on the
        // reasoning that the marker must not run ahead of the picture: the
        // server cuts the view around the VFO, the cut arrives a few frames
        // later, and in between a marker at the new frequency sits on top of
        // the old bins. Sound reasoning about the wrong half. What actually
        // moved apart was the marker and the *axis*, because the axis centre
        // was taken from the server and the marker from here.
        //
        // That branch could not run at the time - without a full-band row the
        // target centre was, by definition, the server's centre, so the jump
        // was measured as zero and the latch never engaged. Build 82 made the
        // target centre VFO-derived, which is the whole fix, and in doing so
        // opened this branch for the first time. Found on 2026-08-15:
        // a repair from two days earlier, activated by the repair that replaced
        // it, putting back the very construction it removed - the axis from the
        // VFO, the marker from the server, one round trip apart.
        //
        // Both now come from the VFO and travel together. The bins still arrive
        // late; they slide in under a marker that has not moved relative to the
        // scale, which reads as the picture catching up rather than as the line
        // stepping out and snapping back.
        let vfo_target = self.frequency_hz as f64;
        // What the picture is made of, when the latch flips. Two attempts at
        // this were reasoned out and both were wrong; these are the numbers
        // that decide it.
        // Written down when the frequency moves, not when the tuning latch
        // flips.
        //
        // It hung off the latch, and the latch only engages on a jump bigger
        // than eight kilohertz (`should_force_full_tuning`). Tuning by scrolling
        // the spectrum moves a kilohertz at a time, so it never engaged, the
        // line never appeared, and a session in which the operator had tuned
        // plenty looked like one in which nothing had happened (2026-08-15).
        // The same gate is why the repair in build 80 could have no effect
        // whatever on that way of tuning: it was all inside the latch.
        //
        // A kilohertz of movement is the smallest step there is, so this fires
        // on any real tuning and on nothing else.
        // A kilohertz of movement AND at most one line a second: the gate on
        // its own turned a knob spin into two hundred lines a minute.
        let rx1_quiet_enough = self
            .rx1_logged_at
            .map(|t| t.elapsed() >= std::time::Duration::from_secs(1))
            .unwrap_or(true);
        if rx1_quiet_enough && self.frequency_hz.abs_diff(self.rx1_logged_freq_hz) >= 1_000 {
            self.rx1_logged_freq_hz = self.frequency_hz;
            self.rx1_logged_at = Some(Instant::now());
            // `width from` is the whole point of this line. The fault lived in
            // there being two regimes, entered by a race nobody could see, so a
            // session that behaves is only evidence if it also says which
            // regime it was in. "DDC" means the full-band row never arrived -
            // the case that used to be broken, and the one worth proving.
            // Two centres, and the difference between them is the fault.
            // `drawn` is what this client puts in the middle of the picture -
            // the number build 82 rewrote. `server` is what came back in the
            // packet. Only `server` was logged, so a session in which the fix
            // works read exactly like one in which the client computes the
            // wrong centre and the server happens to agree (2026-08-15).
            log::info!(
                "RX1 view: tuning={} vfo={} Hz, drawn centre={} server centre={} span={} Hz, zoom={:.1} pan={:.4}, width={} Hz from {}",
                rx1_tuning_active,
                self.frequency_hz,
                self.smooth_display_center_hz as u64,
                self.spectrum_center_hz,
                self.spectrum_span_hz,
                self.spectrum_zoom,
                self.spectrum_pan,
                self.rx1_full_span_hz(),
                // Four states, and which one it is has to be derived the same
                // way the width itself is - see `full_span_source`.
                Self::full_span_source(
                    self.ddc_sample_rate_rx1,
                    self.full_spectrum_span_hz,
                    self.full_spectrum_enabled,
                ),
            );
        }
        if self.pending_freq.is_some() || self.smooth_vfo_hz == 0.0 {
            self.smooth_vfo_hz = vfo_target;
        } else {
            self.smooth_vfo_hz += (vfo_target - self.smooth_vfo_hz) * alpha;
        }
        if (self.smooth_vfo_hz - vfo_target).abs() < 1.0 {
            self.smooth_vfo_hz = vfo_target;
        }
        let smooth_vfo = self.smooth_vfo_hz as u64;

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
        let (spec_h, wf_h) = super::spectrum::plot_and_waterfall_heights(spec_area, ui.spacing().item_spacing.y);

        // The tuning latch falls back to the full row because the extracted view
        // lags while retuning. With the row switched off, or no longer coming,
        // there is nothing to fall back to - the client keeps the last row, of
        // the old band perhaps (build 234) - so the view is drawn throughout, as
        // on VRX.
        let (plot_bins, plot_center_hz, plot_span_hz) = if !rx1_tuning_active || !self.full_row_live(self.full_row_at) {
            (&self.spectrum_bins, self.spectrum_center_hz, self.spectrum_span_hz)
        } else {
            (&self.full_spectrum_bins, self.full_spectrum_center_hz, self.full_spectrum_span_hz)
        };

        let full_span = if self.full_spectrum_enabled { self.full_spectrum_span_hz } else { self.spectrum_span_hz } as f64;
        let wf_window = super::spectrum::WaterfallWindow {
            center_hz: smooth_center as f64,
            span_hz: self.waterfall.display_span_hz(full_span / (self.spectrum_zoom as f64).max(1.0)),
            row_span_hz: full_span,
        };
        let mut texture = std::mem::take(&mut self.waterfall.texture);
        let panel = super::spectrum::SpectrumPanel {
            id: "rx1",
            bins: plot_bins,
            bins_center_hz: plot_center_hz as f64,
            bins_span_hz: plot_span_hz as f64,
            view_center_hz: smooth_center as f64,
            marker_hz: smooth_vfo,
            filter_low_hz: self.filter_low_hz,
            filter_high_hz: self.filter_high_hz,
            ref_db: self.spectrum_ref_db,
            range_db: self.spectrum_range_db,
            fill_percent: self.spectrum_fill_percent,
            wf_contrast: self.waterfall_contrast,
            meter: if self.other_tx {
                super::spectrum::SpectrumMeter::OtherTransmits(self.smeter)
            } else if self.smeter_tx {
                super::spectrum::SpectrumMeter::Transmitting(self.smeter)
            } else {
                super::spectrum::SpectrumMeter::Signal(self.smeter)
            },
            big_meter: is_popout,
            rit_hz: self.rit_enable.then_some(self.rit_offset as i32),
            band_markers: true,
            dx_spots: &self.dx_spots,
            tuning: super::spectrum::SpectrumTuning {
                tune_hz: self.frequency_hz,
                min_hz: 1_000,
                max_hz: u64::MAX,
                filter: super::spectrum::FilterEdges::Free,
            },
        };
        let action = super::spectrum::spectrum_panel_view(ui, ctx, &panel, &self.waterfall, wf_window, &mut texture, spec_h, wf_h);
        self.waterfall.texture = texture;
        super::spectrum::store_rx_action(ui, "", action);
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
