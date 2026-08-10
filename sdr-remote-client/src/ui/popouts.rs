// SPDX-License-Identifier: GPL-2.0-or-later
//! Detached pop-out windows (VRX/spectrum/Yaesu viewports) and the shared
//! viewport-geometry helpers (persist/restore position+size, off-screen
//! validation, recenter). Extracted verbatim from `ui/mod.rs` - pure
//! relocation, no behaviour change.

use egui::{ViewportBuilder, ViewportId};
use super::*;


/// Identifies a detachable popout window - selects its persisted geometry
/// slot, ViewportId, title and default size (model B, Phase 2). New windows
/// join by adding a variant + its rows in the match arms above.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PopoutKind {
    Rx1,
    Rx2,
    Vrx1,
    Vrx2,
    Yaesu1,
    Yaesu2,
    /// Combined RX1+RX2 window. Reuses the RX1-solo ViewportId ("spectrum_popout")
    /// - the two are mutually exclusive (joined XOR separate) so they share the
    /// one OS window - but has its own `popout_joined_*` geometry slot.
    Joined,
}

impl SdrRemoteApp {
    // ---- Model B Phase 2: shared popout lifecycle ---------------------------
    // One declarative place for every detachable window's geometry / focus /
    // close / save, instead of the same lifecycle re-implemented per window.

    fn popout_geom(&self, k: PopoutKind) -> (Option<egui::Pos2>, Option<egui::Vec2>, bool) {
        match k {
            PopoutKind::Rx1 => (self.spectrum_popout_pos, self.spectrum_popout_size, self.spectrum_popout_init_applied),
            PopoutKind::Rx2 => (self.rx2_popout_pos, self.rx2_popout_size, self.rx2_popout_init_applied),
            PopoutKind::Vrx1 => (self.vrx_popout_pos, self.vrx_popout_size, self.vrx_popout_init_applied),
            PopoutKind::Vrx2 => (self.vrx2_popout_pos, self.vrx2_popout_size, self.vrx2_popout_init_applied),
            PopoutKind::Yaesu1 => (self.yaesu_popout_pos, self.yaesu_popout_size, self.yaesu_popout_init_applied),
            PopoutKind::Yaesu2 => (self.yaesu2_popout_pos, self.yaesu2_popout_size, self.yaesu2_popout_init_applied),
            PopoutKind::Joined => (self.popout_joined_pos, self.popout_joined_size, self.popout_joined_init_applied),
        }
    }

    fn set_popout_geom(&mut self, k: PopoutKind, pos: Option<egui::Pos2>, size: Option<egui::Vec2>) {
        match k {
            PopoutKind::Rx1 => { self.spectrum_popout_pos = pos; self.spectrum_popout_size = size; }
            PopoutKind::Rx2 => { self.rx2_popout_pos = pos; self.rx2_popout_size = size; }
            PopoutKind::Vrx1 => { self.vrx_popout_pos = pos; self.vrx_popout_size = size; }
            PopoutKind::Vrx2 => { self.vrx2_popout_pos = pos; self.vrx2_popout_size = size; }
            PopoutKind::Yaesu1 => { self.yaesu_popout_pos = pos; self.yaesu_popout_size = size; }
            PopoutKind::Yaesu2 => { self.yaesu2_popout_pos = pos; self.yaesu2_popout_size = size; }
            PopoutKind::Joined => { self.popout_joined_pos = pos; self.popout_joined_size = size; }
        }
    }

    fn set_popout_init(&mut self, k: PopoutKind, v: bool) {
        match k {
            PopoutKind::Rx1 => self.spectrum_popout_init_applied = v,
            PopoutKind::Rx2 => self.rx2_popout_init_applied = v,
            PopoutKind::Vrx1 => self.vrx_popout_init_applied = v,
            PopoutKind::Vrx2 => self.vrx2_popout_init_applied = v,
            PopoutKind::Yaesu1 => self.yaesu_popout_init_applied = v,
            PopoutKind::Yaesu2 => self.yaesu2_popout_init_applied = v,
            PopoutKind::Joined => self.popout_joined_init_applied = v,
        }
    }

    /// ViewportId source, window title, default size per kind. Takes `&self`
    /// because the Yaesu titles are dynamic (server-reported radio model, live
    /// updating); the spectrum/VRX titles are fixed. Title is owned so both
    /// forms fit one signature.
    fn popout_meta(&self, k: PopoutKind) -> (&'static str, String, [f32; 2]) {
        match k {
            PopoutKind::Rx1 => ("spectrum_popout", "ThetisLink - RX1 / VFO-A".to_string(), [900.0, 600.0]),
            PopoutKind::Rx2 => ("rx2_popout", "ThetisLink - RX2 / VFO-B".to_string(), [900.0, 600.0]),
            PopoutKind::Vrx1 => ("vrx1_popout", "ThetisLink - VRX1".to_string(), [460.0, 480.0]),
            PopoutKind::Vrx2 => ("vrx2_popout", "ThetisLink - VRX2".to_string(), [460.0, 480.0]),
            PopoutKind::Yaesu1 => ("yaesu_popout", self.yaesu_window_title(0), [465.0, 335.0]),
            PopoutKind::Yaesu2 => ("yaesu2_popout", self.yaesu_window_title(1), [465.0, 335.0]),
            PopoutKind::Joined => ("spectrum_popout", "ThetisLink - RX1 + RX2".to_string(), [900.0, 900.0]),
        }
    }

    /// The ONE popout lifecycle for every detachable window: restore geometry
    /// (focus on first open), show the viewport, run the close side-effects via
    /// `on_close`, track + persist geometry, then render `content`. Each window
    /// supplies only its identity (`kind`) + body + close behaviour.
    pub(super) fn show_popout(
        &mut self,
        ctx: &egui::Context,
        kind: PopoutKind,
        // egui's show_viewport_immediate closure is FnMut, so the body + close
        // callbacks must be FnMut too (an FnOnce cannot be moved out of an FnMut).
        mut content: impl FnMut(&mut Self, &egui::Context),
        mut on_close: impl FnMut(&mut Self),
    ) {
        let (mut pos, mut size, mut init) = self.popout_geom(kind);
        let first_open = !init;
        let (id_str, title, default_size) = self.popout_meta(kind);
        // Tag every pop-out title with the instance profile (" [B]") so two
        // instances' pop-outs are distinguishable; no-op for the default profile.
        let title = super::config::window_title(&title);
        let vb = Self::apply_popout_geometry(
            ViewportBuilder::default().with_title(title),
            pos,
            size,
            default_size,
            Self::viewport_native_ppp(ctx),
            ctx.zoom_factor(),
            &mut init,
        );
        self.set_popout_init(kind, init);
        ctx.show_viewport_immediate(
            ViewportId::from_hash_of(id_str),
            vb,
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    on_close(self);
                    return;
                }
                if first_open {
                    // Raise the window on a fresh open / reopen / snap so every
                    // window comes to the foreground uniformly. Arranged windows do
                    // not overlap, so focusing each snapped window is conflict-free.
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                if Self::track_popout_geometry(ctx, &mut pos, &mut size) {
                    self.set_popout_geom(kind, pos, size);
                    self.save_full_config();
                }
                content(self, ctx);
            },
        );
    }

    /// Renders ONE detached VRX window (VRX1 or VRX2) via the shared `show_popout`
    /// lifecycle: controls on top, spectrum below. Geometry per channel.
    pub(super) fn render_vrx_popout(&mut self, ctx: &egui::Context, ch: VrxChannel) {
        let is_vrx1 = matches!(ch, VrxChannel::Vrx1);
        // Seed absolute freq on first open so the real listening freq is visible.
        if is_vrx1 {
            if self.vrx1_freq_hz == 0 && self.frequency_hz > 0 {
                self.vrx1_freq_hz = self.frequency_hz;
                let _ = self.cmd_tx.send(Command::SetVrxFrequency(self.vrx1_freq_hz));
            }
        } else if self.vrx2_freq_hz == 0 && self.rx2_frequency_hz > 0 {
            self.vrx2_freq_hz = self.rx2_frequency_hz;
            let _ = self.cmd_tx.send(Command::SetVrx2Frequency(self.vrx2_freq_hz));
        }
        let kind = if is_vrx1 { PopoutKind::Vrx1 } else { PopoutKind::Vrx2 };
        self.show_popout(
            ctx,
            kind,
            |app, ctx| app.render_vrx_popout_body(ctx, ch),
            |app| app.close_vrx_popout(ch),
        );
    }

    /// Close side-effects for a VRX popout: turn its spectrum subscription off
    /// (model B derives the window from that), clear the buffer + texture. VRX
    /// audio (`vrx*_enabled`) stays untouched.
    fn close_vrx_popout(&mut self, ch: VrxChannel) {
        if matches!(ch, VrxChannel::Vrx1) {
            self.vrx_popout_init_applied = false;
            self.vrx1_waterfall_texture = None;
            if self.vrx1_high_res_spectrum {
                self.vrx1_high_res_spectrum = false;
                let _ = self.cmd_tx.send(Command::SetVrxHighResSpectrum(0, false, self.vrx1_high_res_last_span_khz));
                self.vrx1_spectrum.clear();
            }
        } else {
            self.vrx2_popout_init_applied = false;
            self.vrx2_waterfall_texture = None;
            if self.vrx2_high_res_spectrum {
                self.vrx2_high_res_spectrum = false;
                let _ = self.cmd_tx.send(Command::SetVrxHighResSpectrum(1, false, self.vrx2_high_res_last_span_khz));
                self.vrx2_spectrum.clear();
            }
        }
        self.save_full_config();
    }

    /// Renders the detached RX1 spectrum popout via the shared `show_popout`
    /// lifecycle. Called from the separate-mode branch in the update loop.
    pub(super) fn render_rx1_popout(&mut self, ctx: &egui::Context) {
        self.show_popout(
            ctx,
            PopoutKind::Rx1,
            |app, ctx| app.render_rx1_popout_body(ctx),
            |app| app.close_rx1_popout(),
        );
    }

    /// Renders the detached RX2 spectrum popout via the shared `show_popout`
    /// lifecycle. Called from the separate-mode branch in the update loop.
    pub(super) fn render_rx2_popout(&mut self, ctx: &egui::Context) {
        self.show_popout(
            ctx,
            PopoutKind::Rx2,
            |app, ctx| app.render_rx2_popout_body(ctx),
            |app| app.close_rx2_popout(),
        );
    }

    /// Close side-effects for the RX2 spectrum popout: turns the RX2 spectrum
    /// fully off (window closed = stop the spectrum). model B derives the window.
    fn close_rx2_popout(&mut self) {
        self.rx2_popout_init_applied = false;
        if self.rx2_spectrum_enabled {
            self.rx2_spectrum_enabled = false;
            let _ = self.cmd_tx.send(Command::EnableRx2Spectrum(false));
        }
        self.save_full_config();
    }

    /// RX2 spectrum popout window body: split/join segment (only when RX1 is also
    /// popped out), the RX2 content, spectrum keys and the A<>B overlay.
    /// `show_rx1_popout` is recomputed here (want && can already applied).
    fn render_rx2_popout_body(&mut self, ctx: &egui::Context) {
        let show_rx1_popout = self.spectrum_popout && self.spectrum_enabled && self.can_rx1();
        egui::CentralPanel::default().show(ctx, |ui| {
            // Split/Join segmented right-aligned (only visible when RX1 is also popped out)
            if show_rx1_popout {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    self.render_split_join_segmented(ui, false, None);
                });
            }
            self.render_rx2_content(ui, ctx);
            // Read spectrum interaction keys inside this viewport
            self.handle_rx2_spectrum_keys(ctx);
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
            // A⇔B overlay
            if show_rx1_popout {
                let r = self.popout_rx2_smeter_rect;
                if r.is_positive() {
                    let btn_pos = if self.meter_analog[M_RX2] {
                        egui::pos2(r.left() + 27.0, r.max.y - 12.0)
                    } else {
                        let panel_right = ui.max_rect().right() - 4.0;
                        egui::pos2(panel_right - 23.0, r.center().y)
                    };
                    let btn_rect = egui::Rect::from_center_size(
                        btn_pos,
                        egui::vec2(46.0, 20.0),
                    );
                    let resp = ui.add_enabled_ui(self.connected, |ui| {
                        ui.put(btn_rect, egui::Button::new(RichText::new("A<>B").size(10.0)))
                            .on_hover_text(rust_i18n::t!("main_hover_swap_vfo").to_string())
                    }).inner;
                    if resp.clicked() {
                        let _ = self.cmd_tx.send(Command::SetControl(ControlId::VfoSwap, 2));
                    }
                }
            }
        });
    }

    /// Close side-effects for the RX1 spectrum popout: turns the RX1 spectrum
    /// fully off (owner choice - closing the window is "stop the spectrum", not
    /// "go inline"; the inline/popout choice is the separate popout toggle).
    fn close_rx1_popout(&mut self) {
        self.spectrum_popout = false;
        self.spectrum_popout_init_applied = false;
        if self.spectrum_enabled {
            self.spectrum_enabled = false;
            let _ = self.cmd_tx.send(Command::EnableSpectrum(false));
        }
        self.save_full_config();
    }

    /// RX1 spectrum popout window body: the split/join segment (only when RX2 is
    /// also popped out), the shared spectrum content, spectrum keys and the A<>B
    /// overlay. `show_rx2_popout` is recomputed here (want && can already applied).
    fn render_rx1_popout_body(&mut self, ctx: &egui::Context) {
        let show_rx2_popout = self.rx2_popout && self.rx2_spectrum_enabled;
        egui::CentralPanel::default().show(ctx, |ui| {
            // Split/Join segmented right-aligned (only visible when RX2 is also popped out)
            if show_rx2_popout {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    self.render_split_join_segmented(ui, false, None);
                });
            }
            self.render_rx1_popout_content(ui, ctx);
            // Read spectrum interaction keys inside this viewport
            self.handle_rx1_spectrum_keys(ctx);
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
            // A⇔B overlay
            if show_rx2_popout {
                let r = self.popout_rx1_smeter_rect;
                if r.is_positive() {
                    let btn_pos = if self.meter_analog[M_RX1] {
                        egui::pos2(r.left() + 27.0, r.max.y - 12.0)
                    } else {
                        let panel_right = ui.max_rect().right() - 4.0;
                        egui::pos2(panel_right - 23.0, r.center().y)
                    };
                    let btn_rect = egui::Rect::from_center_size(
                        btn_pos,
                        egui::vec2(46.0, 20.0),
                    );
                    let resp = ui.add_enabled_ui(self.connected, |ui| {
                        ui.put(btn_rect, egui::Button::new(RichText::new("A<>B").size(10.0)))
                            .on_hover_text(rust_i18n::t!("main_hover_swap_vfo").to_string())
                    }).inner;
                    if resp.clicked() {
                        let _ = self.cmd_tx.send(Command::SetControl(ControlId::VfoSwap, 2));
                    }
                }
            }
        });
    }

    /// Renders the combined RX1+RX2 (joined) spectrum popout via the shared
    /// `show_popout` lifecycle. Called from the joined branch in the update loop.
    /// Reuses the RX1-solo ViewportId (they are mutually exclusive).
    pub(super) fn render_joined_popout(&mut self, ctx: &egui::Context) {
        self.show_popout(
            ctx,
            PopoutKind::Joined,
            |app, ctx| app.render_joined_popout_body(ctx),
            |app| app.close_joined_popout(),
        );
    }

    /// Close side-effects for the joined RX1+RX2 popout: window closed = both
    /// spectra fully off (owner choice: consistent with RX2/VRX, no inline
    /// fallback). `rx2_popout` derives from `rx2_spectrum_enabled` (model B).
    fn close_joined_popout(&mut self) {
        self.spectrum_popout = false;
        self.popout_joined_init_applied = false;
        if self.spectrum_enabled {
            self.spectrum_enabled = false;
            let _ = self.cmd_tx.send(Command::EnableSpectrum(false));
        }
        if self.rx2_spectrum_enabled {
            self.rx2_spectrum_enabled = false;
            let _ = self.cmd_tx.send(Command::EnableRx2Spectrum(false));
        }
        self.save_full_config();
    }

    /// Joined RX1+RX2 popout body: RX1/RX2 controls side by side, the two spectra
    /// stacked (RX1 on top, RX2 below, each with its own "Waiting for ..."
    /// placeholder pre-connect), spectrum keys, and the floating A<>B overlay.
    /// Relocated verbatim from the inline update-loop block.
    fn render_joined_popout_body(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            // Controls side by side: RX1 left, RX2 right
            ui.columns(2, |cols| {
                // Left column: RX1 controls
                self.render_rx1_controls(&mut cols[0], controls::UiSurface::PopoutJoined);

                // Right column: RX2 controls with Split button on S-meter row
                self.render_rx2_controls_with_split(&mut cols[1], true, true, controls::UiSurface::PopoutJoined);
            });

            ui.separator();

            // Spectrums stacked: RX1 on top, RX2 below. Pre-connect both
            // halves show their respective "Waiting for ..." placeholder so
            // the layout is recognisable as two stacked RX-panes even before
            // any spectrum bins arrive (RX2 already had this gate inside
            // render_rx2_spectrum_only; RX1 gets it here to mirror that).
            let total_w = ui.available_width();
            let available = ui.available_height();
            let half = (available - 4.0) / 2.0;
            ui.allocate_ui(egui::vec2(total_w, half), |ui| {
                if !self.spectrum_bins.is_empty() {
                    self.render_spectrum_content(ui, ctx, 0.0, true);
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label(RichText::new(rust_i18n::t!("main_waiting_rx1_spectrum").to_string()).weak());
                    });
                }
            });
            ui.add_space(2.0);
            self.render_rx2_spectrum_only(ui, ctx);

            // Read RX2 spectrum interaction keys inside viewport
            self.handle_rx2_spectrum_keys(ctx);
        });

        // Read RX1 spectrum interaction keys inside viewport
        self.handle_rx1_spectrum_keys(ctx);
        ctx.request_repaint_after(std::time::Duration::from_millis(33));

        // Floating A⇔B overlay
        let r1 = self.popout_rx1_smeter_rect;
        let r2 = self.popout_rx2_smeter_rect;
        if r1.is_positive() && r2.is_positive() {
            if self.meter_analog[M_RX1] {
                // Analog: 60×28 button just left of RX1 meter,
                // top-aligned - mirrors the Split button on RX2.
                // Default styling (no fill) - blue is reserved for
                // toggled-on state in this app; A<>B is momentary.
                // Use a centered child-UI so the button text is
                // horizontally centered (matches Split).
                let btn_w = 60.0_f32;
                let btn_h = 28.0_f32;
                let pos = egui::pos2(r1.left() - btn_w - 4.0, r1.top());
                egui::Area::new(egui::Id::new("vfo_swap_joined_analog"))
                    .fixed_pos(pos)
                    .order(egui::Order::Foreground)
                    .interactable(true)
                    .show(ctx, |ui| {
                        let btn_rect = egui::Rect::from_min_size(pos, egui::vec2(btn_w, btn_h));
                        let mut btn_ui = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(btn_rect)
                                .layout(egui::Layout::top_down(egui::Align::Center))
                        );
                        let btn = egui::Button::new(RichText::new("A<>B").strong())
                            .min_size(egui::vec2(btn_w, btn_h));
                        if btn_ui.add_enabled(self.connected, btn).on_hover_text(rust_i18n::t!("main_hover_swap_vfo").to_string()).clicked() {
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::VfoSwap, 2));
                        }
                    });
            } else {
                // Bar mode: keep original between-meters placement.
                // Separate Area ID from the analog one to avoid
                // egui caching a previous frame's larger size
                // constraint and triggering character-wrap of
                // "A<>B" when toggling Analog -> Bar.
                let center_x = (r1.right() + r2.left()) / 2.0;
                let center_y = (r1.center().y + r2.center().y) / 2.0;
                let pos = egui::pos2(center_x - 23.0, center_y - 10.0);
                egui::Area::new(egui::Id::new("vfo_swap_joined_bar"))
                    .fixed_pos(pos)
                    .order(egui::Order::Foreground)
                    .interactable(true)
                    .show(ctx, |ui| {
                        let btn = egui::Button::new(RichText::new("A<>B").size(10.0))
                            .min_size(egui::vec2(40.0, 18.0));
                        if ui.add_enabled(self.connected, btn).on_hover_text(rust_i18n::t!("main_hover_swap_vfo").to_string()).clicked() {
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::VfoSwap, 2));
                        }
                    });
            }
        }
    }

    /// VRX popout window body (inside the viewport): channel controls + spectrum
    /// panel, the filter-edge memory dispatch and the repaint request.
    fn render_vrx_popout_body(&mut self, ctx: &egui::Context, ch: VrxChannel) {
        let is_vrx1 = matches!(ch, VrxChannel::Vrx1);
        egui::CentralPanel::default().show(ctx, |ui| {
            // DDC range of this channel: center+span from the spectrum packets,
            // fall back to VFO ± 192 kHz while there's no spectrum yet.
            let ddc_center = if is_vrx1 {
                if self.full_spectrum_center_hz > 0 { self.full_spectrum_center_hz as u64 } else { self.frequency_hz }
            } else {
                if self.rx2_full_spectrum_center_hz > 0 { self.rx2_full_spectrum_center_hz as u64 } else { self.rx2_frequency_hz }
            };
            // Not the raw DDC edges: the reachable range stops where the server
            // stops listening, so tuning cannot run past the audio.
            let (vmin, vmax) = self.vrx_tune_limits(ch, ddc_center);
            self.render_vrx_channel_controls(ui, ch, ddc_center, vmin, vmax);
            ui.separator();
            let outer_w = ui.available_width();
            let avail_h = ui.available_height().max(120.0);
            ui.allocate_ui(egui::vec2(outer_w, avail_h), |ui| {
                self.render_vrx_spectrum_panel(ui, ctx, ch, vmin, vmax);
            });
        });
        // Filter-edge memory dispatch for this channel (dispatch-return discipline).
        let (salt, vrx_id) = if is_vrx1 { ("vrx1", 0u8) } else { ("vrx2", 1u8) };
        let lo_key = egui::Id::new(format!("{}_filter_low_hz", salt));
        let hi_key = egui::Id::new(format!("{}_filter_high_hz", salt));
        let new_lo: Option<i32> = ctx.memory(|m| m.data.get_temp(lo_key));
        let new_hi: Option<i32> = ctx.memory(|m| m.data.get_temp(hi_key));
        if new_lo.is_some() || new_hi.is_some() {
            let cur_lo = if vrx_id == 0 { self.vrx1_filter_low_hz } else { self.vrx2_filter_low_hz };
            let cur_hi = if vrx_id == 0 { self.vrx1_filter_high_hz } else { self.vrx2_filter_high_hz };
            let final_lo = new_lo.unwrap_or(cur_lo);
            let final_hi = new_hi.unwrap_or(cur_hi);
            if self.cmd_tx.send(Command::SetVrxFilter(vrx_id, final_lo, final_hi)).is_ok() {
                if vrx_id == 0 {
                    self.vrx1_filter_low_hz = final_lo;
                    self.vrx1_filter_high_hz = final_hi;
                } else {
                    self.vrx2_filter_low_hz = final_lo;
                    self.vrx2_filter_high_hz = final_hi;
                }
                ctx.memory_mut(|m| { m.data.remove::<i32>(lo_key); m.data.remove::<i32>(hi_key); });
                self.save_full_config();
            }
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }

    /// Renders the detached Yaesu-1 (991A) popout via the shared `show_popout`
    /// lifecycle. Yaesu is NOT a spectrum source (model B), so it keeps its own
    /// `yaesu_popout` open flag + the availability gate at the call site; only
    /// the presentation lifecycle (geometry/focus/close/save) is shared here.
    pub(super) fn render_yaesu1_popout(&mut self, ctx: &egui::Context) {
        self.show_popout(
            ctx,
            PopoutKind::Yaesu1,
            |app, ctx| app.render_yaesu1_popout_body(ctx),
            |app| app.close_yaesu1_popout(),
        );
    }

    /// Renders the detached Yaesu-2 (slot-1, e.g. FTX-1) popout via the shared
    /// `show_popout` lifecycle. Mirror of Yaesu-1, routed to slot 1.
    pub(super) fn render_yaesu2_popout(&mut self, ctx: &egui::Context) {
        self.show_popout(
            ctx,
            PopoutKind::Yaesu2,
            |app, ctx| app.render_yaesu2_popout_body(ctx),
            |app| app.close_yaesu2_popout(),
        );
    }

    /// Close side-effects for the Yaesu-1 popout: clear the open flag + init so a
    /// reopen re-applies geometry, then persist.
    fn close_yaesu1_popout(&mut self) {
        self.yaesu_popout = false;
        self.yaesu_popout_init_applied = false;
        self.save_full_config();
    }

    /// Close side-effects for the Yaesu-2 popout. Uses `save_ptt_config` (the
    /// slot-1 window's closed state lives in the PTT config), matching the
    /// original inline handler.
    fn close_yaesu2_popout(&mut self) {
        self.yaesu2_popout = false;
        self.yaesu2_popout_init_applied = false;
        self.save_ptt_config(); // persist window closed state
    }

    /// Yaesu-1 popout body: fixed PTT + volume bar on the bottom, the 991A panel
    /// (scrollable) in the centre. Relocated verbatim from the inline update-loop
    /// block; PTT mouse-latch + spacebar OR mirrors the Thetis PTT handler.
    fn render_yaesu1_popout_body(&mut self, ctx: &egui::Context) {
        // Fixed PTT button at bottom
        egui::TopBottomPanel::bottom("yaesu_ptt_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                // PTT button - locked when other client is transmitting
                let (ptt_color, ptt_text, ptt_locked) = if self.other_tx {
                    (Color32::from_rgb(200, 120, 0), rust_i18n::t!("main_tx_in_use").to_string(), true)
                } else if self.yaesu_tx_active {
                    (Color32::RED, "TX".to_string(), false)
                } else {
                    (Color32::from_rgb(60, 60, 60), "PTT".to_string(), false)
                };
                let ptt_btn = egui::Button::new(
                    RichText::new(ptt_text).size(18.0).color(Color32::WHITE).strong(),
                ).fill(ptt_color).min_size(egui::vec2(80.0, 40.0));
                let response = ui.add_enabled(!ptt_locked, ptt_btn);
                // Mouse PTT into a single latch (toggle flips it, momentary
                // tracks the button hold) so the spacebar can OR with it -
                // mirrors the Thetis PTT handler.
                if self.yaesu_ptt_toggle_mode {
                    if response.clicked() {
                        self.yaesu_mouse_ptt = !self.yaesu_mouse_ptt;
                    }
                } else {
                    self.yaesu_mouse_ptt = ui.input(|i| {
                        i.pointer.primary_down()
                            && response.rect.contains(i.pointer.interact_pos().unwrap_or(egui::Pos2::ZERO))
                    });
                }
                // Spacebar keys this radio while ITS OWN window has focus (the
                // pop-out is a separate viewport with its own keyboard input,
                // so the main-window PTT handler never sees it). Momentary,
                // combined with the mouse latch; send only on the combined edge.
                let space_held = ui.input(|i| i.key_down(egui::Key::Space));
                let want_tx = (self.yaesu_mouse_ptt || space_held) && !ptt_locked;
                if want_tx != self.yaesu_ptt_last_sent {
                    self.yaesu_ptt_last_sent = want_tx;
                    self.apply_ptt_spike_protection(true, want_tx);
                    let _ = self.cmd_tx.send(Command::SetYaesuPtt(want_tx));
                }

                ui.separator();

                // Same audio switch as on the main screen: the control window can
                // stay open with the radio muted.
                if Self::render_window_audio_toggle(
                    ui, self.yaesu_enabled, self.connected,
                    &rust_i18n::t!("main_hover_yaesu_audio", label = self.yaesu_slot_label(0)).to_string(),
                ) {
                    self.toggle_yaesu_audio(0);
                }

                ui.separator();

                ui.label(rust_i18n::t!("main_volume_label").to_string());
                let slider = egui::Slider::new(&mut self.yaesu_volume, 0.001..=1.0)
                    .logarithmic(true)
                    .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
                let resp = ui.add_sized([140.0, 16.0], slider);
                let scrolled = helpers::slider_wheel(ui, &resp, &mut self.yaesu_volume, 0.001..=1.0, 0.02);
                if resp.changed() || scrolled {
                    let _ = self.cmd_tx.send(Command::SetYaesuVolume(self.yaesu_volume));
                }
                if resp.drag_stopped() || scrolled {
                    self.save_full_config();
                }
            });
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                self.render_yaesu_popout(ui);
            });
        });
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }

    /// Yaesu-2 popout body: mirror of Yaesu-1 routed to slot 1. Relocated verbatim
    /// from the inline update-loop block.
    fn render_yaesu2_popout_body(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("yaesu2_ptt_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let (ptt_color, ptt_text, ptt_locked) = if self.other_tx {
                    (Color32::from_rgb(200, 120, 0), rust_i18n::t!("main_tx_in_use").to_string(), true)
                } else if self.yaesu2_tx_active {
                    (Color32::RED, "TX".to_string(), false)
                } else {
                    (Color32::from_rgb(60, 60, 60), "PTT".to_string(), false)
                };
                let ptt_btn = egui::Button::new(
                    RichText::new(ptt_text).size(18.0).color(Color32::WHITE).strong(),
                ).fill(ptt_color).min_size(egui::vec2(80.0, 40.0));
                let response = ui.add_enabled(!ptt_locked, ptt_btn);
                // Mouse PTT latch + spacebar (this viewport's keyboard input),
                // same as radio 1.
                if self.yaesu2_ptt_toggle_mode {
                    if response.clicked() {
                        self.yaesu2_mouse_ptt = !self.yaesu2_mouse_ptt;
                    }
                } else {
                    self.yaesu2_mouse_ptt = ui.input(|i| {
                        i.pointer.primary_down()
                            && response.rect.contains(i.pointer.interact_pos().unwrap_or(egui::Pos2::ZERO))
                    });
                }
                let space_held = ui.input(|i| i.key_down(egui::Key::Space));
                let want_tx = (self.yaesu2_mouse_ptt || space_held) && !ptt_locked;
                if want_tx != self.yaesu2_ptt_last_sent {
                    self.yaesu2_ptt_last_sent = want_tx;
                    self.apply_ptt_spike_protection(true, want_tx);
                    let _ = self.cmd_tx.send(Command::SetYaesu2Ptt(want_tx));
                }
                ui.separator();
                // Same audio switch as on the main screen (see slot 0).
                if Self::render_window_audio_toggle(
                    ui, self.yaesu2_enabled, self.connected,
                    &rust_i18n::t!("main_hover_yaesu_audio", label = self.yaesu_slot_label(1)).to_string(),
                ) {
                    self.toggle_yaesu_audio(1);
                }
                ui.separator();
                ui.label(rust_i18n::t!("main_volume_label").to_string());
                let slider = egui::Slider::new(&mut self.yaesu2_volume, 0.001..=1.0)
                    .logarithmic(true)
                    .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
                let resp = ui.add_sized([140.0, 16.0], slider);
                let scrolled = helpers::slider_wheel(ui, &resp, &mut self.yaesu2_volume, 0.001..=1.0, 0.02);
                if resp.changed() || scrolled {
                    let _ = self.cmd_tx.send(Command::SetYaesu2Volume(self.yaesu2_volume));
                }
                if resp.drag_stopped() || scrolled {
                    self.save_full_config();
                }
            });
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                self.render_yaesu2_panel(ui);
            });
        });
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }

    /// Apply persisted popout geometry to a ViewportBuilder. Both
    /// `with_position()` AND `with_inner_size()` are only included on the
    /// first frame after the popout opens. Subsequent frames omit both so
    /// the OS keeps the window wherever the user dragged or resized it -
    /// without that gating the OS gets a fresh "go to this rect" request
    /// every frame and the window oscillates between successive committed
    /// rects during an active move / resize gesture.
    /// Scale factor (physical px per logical point) of the main viewport, used
    /// to compare saved logical positions against OS monitor rects (physical).
    pub(super) fn viewport_native_ppp(ctx: &egui::Context) -> f32 {
        ctx.input(|i| i.viewport().native_pixels_per_point)
            .unwrap_or_else(|| ctx.pixels_per_point())
    }

    /// UNITS. Geometry is stored in SYSTEM points, so a saved layout survives a
    /// change of UI scale: the windows stay where they are and only the content
    /// inside them scales. Everything egui-facing, however, is in EGUI points -
    /// egui-winit converts both a ViewportBuilder and a ViewportCommand with
    /// `zoom_factor * native_pixels_per_point`. So divide by the zoom on the way
    /// in, multiply by it on the way out (see `track_popout_geometry`). At zoom
    /// 1.0 the two are identical, which is why this distinction stayed invisible
    /// until a high-DPI screen made a smaller UI scale worth having.
    pub(super) fn apply_popout_geometry(
        builder: ViewportBuilder,
        pos: Option<egui::Pos2>,
        size: Option<egui::Vec2>,
        default_size: [f32; 2],
        native_ppp: f32,
        zoom: f32,
        init_applied: &mut bool,
    ) -> ViewportBuilder {
        let mut b = builder;
        let z = zoom.max(0.01);
        if !*init_applied {
            let sz = size.map(|v| [v.x / z, v.y / z]).unwrap_or(default_size);
            b = b.with_inner_size(sz);
            if let Some(p) = pos {
                // Validate the persisted position against the live monitor
                // layout. A previously-attached second monitor leaves
                // coordinates that fall outside any current display; egui would
                // then open the viewport off-screen (= invisible). Query the OS
                // work-areas and only re-apply the position when a usable part
                // of the window lands on a connected monitor - otherwise omit
                // with_position() and let it open on the primary monitor. On
                // non-Windows / query failure this returns true (trust the
                // saved value). Manual "Recenter windows" stays as a fallback.
                // The position is in SYSTEM points, so the size handed to the
                // check has to be too - `sz` above has already been divided by the
                // zoom for the builder. Mixing the two made the check reason about
                // a window a factor `zoom` too large.
                if window_placement::saved_window_is_visible(
                    p,
                    egui::vec2(sz[0] * z, sz[1] * z),
                    native_ppp,
                ) {
                    b = b.with_position(egui::pos2(p.x / z, p.y / z));
                } else {
                    log::warn!(
                        "popout: saved pos ({}, {}) falls outside all connected monitors - opening on primary screen",
                        p.x, p.y
                    );
                }
            }
            *init_applied = true;
        }
        b
    }

    /// Read the current viewport pos+size from egui and update the supplied
    /// state slots. Returns `true` when anything changed by more than 5 px
    /// AND the user is not actively dragging/resizing (i.e. mouse button is
    /// up). Per-frame `save_full_config()` during a drag causes ~5-20 ms of
    /// blocking disk-I/O which stalls the input thread; Windows then shows
    /// the window oscillating between successive committed positions. By
    /// gating the save on "pointer released" the move/resize stays smooth
    /// during the gesture and persists on release.
    pub(super) fn track_popout_geometry(
        ctx: &egui::Context,
        pos: &mut Option<egui::Pos2>,
        size: &mut Option<egui::Vec2>,
    ) -> bool {
        // Size via screen_rect (egui's canvas - moves reliably with a
        // resize; viewport().inner_rect turned out to be frozen). Position via outer_rect
        // (the only source for window position).
        // Stored in SYSTEM points - see the note in ui/update.rs. egui's points
        // shrink with the UI zoom; the OS ones do not.
        let z = ctx.zoom_factor().max(0.01);
        let outer = ctx.input(|i| i.viewport().outer_rect);
        let ns = ctx.screen_rect().size() * z;
        let mut state_changed = false;
        if let Some(o) = outer {
            let np = egui::pos2(o.min.x * z, o.min.y * z);
            if pos.map_or(true, |p| (p.x - np.x).abs() > 5.0 || (p.y - np.y).abs() > 5.0) {
                *pos = Some(np);
                state_changed = true;
            }
        }
        if size.map_or(true, |s| (s.x - ns.x).abs() > 5.0 || (s.y - ns.y).abs() > 5.0) {
            *size = Some(ns);
            state_changed = true;
        }
        state_changed && !ctx.input(|i| i.pointer.any_down())
    }

    /// Bring every pop-out window back onto the main window's monitor.
    /// Recovery for a pop-out left on a now-disconnected second monitor:
    /// egui/eframe never tell the app which monitors are currently connected,
    /// so the restored absolute position cannot be auto-validated against the
    /// live monitor layout. This is the manual escape hatch - it replaces
    /// hand-editing the `*_popout_pos` lines in the .conf and restarting.
    /// Positions are anchored just inside the main window (always visible) and
    /// staggered so stacked pop-outs don't perfectly overlap. Clearing each
    /// `*_init_applied` flag makes `apply_popout_geometry` re-apply the new
    /// position on the next frame, so an open pop-out moves immediately - no
    /// restart needed.
    pub(super) fn recenter_popouts(&mut self, ctx: &egui::Context) {
        // outer_rect is in egui points; the stored positions are in system points.
        let z = ctx.zoom_factor().max(0.01);
        let anchor = ctx.input(|i| i.viewport().outer_rect)
            .map(|r| egui::pos2(r.min.x * z, r.min.y * z))
            .unwrap_or(egui::pos2(80.0, 80.0));
        let base = egui::pos2(anchor.x + 40.0, anchor.y + 40.0);
        let place = |pos: &mut Option<egui::Pos2>, init: &mut bool, dx: f32, dy: f32| {
            *pos = Some(egui::pos2(base.x + dx, base.y + dy));
            *init = false;
        };
        place(&mut self.vrx_popout_pos, &mut self.vrx_popout_init_applied, 0.0, 0.0);
        place(&mut self.vrx2_popout_pos, &mut self.vrx2_popout_init_applied, 120.0, 120.0);
        place(&mut self.spectrum_popout_pos, &mut self.spectrum_popout_init_applied, 24.0, 24.0);
        place(&mut self.rx2_popout_pos, &mut self.rx2_popout_init_applied, 48.0, 48.0);
        place(&mut self.yaesu_popout_pos, &mut self.yaesu_popout_init_applied, 72.0, 72.0);
        place(&mut self.yaesu2_popout_pos, &mut self.yaesu2_popout_init_applied, 96.0, 96.0);
        // The joined RX1+RX2 spectrum popout also restores via apply_popout_geometry.
        place(&mut self.popout_joined_pos, &mut self.popout_joined_init_applied, 120.0, 120.0);
        self.save_full_config();
        log::info!("Pop-out windows recentered onto main monitor at {:?}", base);
    }
}
