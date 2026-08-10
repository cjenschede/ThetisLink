// SPDX-License-Identifier: GPL-2.0-or-later
//! "Arrange windows" matrix placer: the SnapWindow / LayoutGrid types and the
//! SdrRemoteApp methods that render and apply the per-monitor placement grid.
//! Extracted from ui/mod.rs (pure relocation, no behaviour change).

use egui::{Color32, RichText};
use super::*;

/// A window that can be snapped onto the screen by the "Arrange windows" matrix.
/// `Main` = the main window (root viewport); the rest are popouts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SnapWindow {
    Main,
    Rx1,
    Rx2,
    Vrx1,
    Vrx2,
    Yaesu1,
    Yaesu2,
}
impl sdr_remote_layout::SnapTarget for SnapWindow {
    fn all() -> &'static [Self] {
        &[
            SnapWindow::Main, SnapWindow::Rx1, SnapWindow::Rx2, SnapWindow::Vrx1,
            SnapWindow::Vrx2, SnapWindow::Yaesu1, SnapWindow::Yaesu2,
        ]
    }
    /// Short cell label. `Main` has an i18n text; the rest are product names.
    fn label(self) -> String {
        match self {
            SnapWindow::Main => rust_i18n::t!("main_window_label").to_string(),
            SnapWindow::Rx1 => "RX1".to_string(),
            SnapWindow::Rx2 => "RX2".to_string(),
            SnapWindow::Vrx1 => "VRX1".to_string(),
            SnapWindow::Vrx2 => "VRX2".to_string(),
            SnapWindow::Yaesu1 => "Yaesu 1".to_string(),
            SnapWindow::Yaesu2 => "Yaesu 2".to_string(),
        }
    }
    /// Stable ASCII key for egui Ids (label() is translated/unstable).
    fn key(self) -> &'static str {
        match self {
            SnapWindow::Main => "main",
            SnapWindow::Rx1 => "rx1",
            SnapWindow::Rx2 => "rx2",
            SnapWindow::Vrx1 => "vrx1",
            SnapWindow::Vrx2 => "vrx2",
            SnapWindow::Yaesu1 => "yaesu1",
            SnapWindow::Yaesu2 => "yaesu2",
        }
    }
    /// Fill color of this window in the placement matrix (distinguishable per window).
    fn color(self) -> Color32 {
        match self {
            SnapWindow::Main => Color32::from_rgb(70, 110, 175),
            SnapWindow::Rx1 => Color32::from_rgb(60, 140, 95),
            SnapWindow::Rx2 => Color32::from_rgb(95, 155, 70),
            SnapWindow::Vrx1 => Color32::from_rgb(160, 115, 55),
            SnapWindow::Vrx2 => Color32::from_rgb(175, 145, 60),
            SnapWindow::Yaesu1 => Color32::from_rgb(145, 80, 140),
            SnapWindow::Yaesu2 => Color32::from_rgb(115, 90, 165),
        }
    }
}

/// The placement matrix, the stored arrangements and the grid limit come from the
/// shared arranger (`sdr-remote-layout`), which the server GUI uses too. They were
/// a second copy on each side and the two drifted apart; a change lands in both by
/// construction now.
pub(crate) type LayoutGrid = sdr_remote_layout::LayoutGrid<SnapWindow>;
pub(crate) type LayoutMemory = sdr_remote_layout::LayoutMemory<SnapWindow>;
pub(crate) use sdr_remote_layout::{SnapTarget, LAYOUT_MEM_SLOTS};
pub(crate) use sdr_remote_layout::{layout_grids_from_config, layout_grids_to_config};

impl SdrRemoteApp {
    /// Is this window AVAILABLE to arrange (server configuration), regardless of
    /// whether it is currently open/active? Same gates as the toolbar: RX/VRX
    /// require Thetis, RX2/VRX2 a second receiver, Yaesu the presence of that
    /// radio. This way you can also place a still-disabled window in the matrix.
    fn snap_is_available(&self, w: SnapWindow) -> bool {
        match w {
            SnapWindow::Main => true,
            SnapWindow::Rx1 => self.thetis_configured,
            SnapWindow::Rx2 => self.thetis_configured && self.rx2_present,
            SnapWindow::Vrx1 => self.thetis_configured,
            SnapWindow::Vrx2 => self.thetis_configured && self.rx2_present,
            SnapWindow::Yaesu1 => self.yaesu_connected,
            SnapWindow::Yaesu2 => self.yaesu2_connected,
        }
    }

    /// Ensure this window is actually open/visible - mirror of the
    /// toolbar toggles - so a window placed via the matrix but still closed
    /// actually appears after "Apply". Guards ensure an already
    /// active subscription is not re-enabled.
    fn snap_open(&mut self, w: SnapWindow) {
        match w {
            SnapWindow::Main => {} // always open
            SnapWindow::Rx1 => {
                if !self.spectrum_enabled {
                    self.spectrum_enabled = true;
                    let _ = self.cmd_tx.send(Command::EnableSpectrum(true));
                }
                self.spectrum_popout = true;
                self.spectrum_popout_init_applied = false;
            }
            SnapWindow::Rx2 => {
                if !self.rx2_spectrum_enabled {
                    self.rx2_spectrum_enabled = true;
                    let _ = self.cmd_tx.send(Command::EnableRx2Spectrum(true));
                    self.rx2_last_sent_zoom = 0.0;
                    self.rx2_last_sent_pan = 0.0;
                    self.rx2_zoom_pan_changed_at = Some(Instant::now());
                }
                self.rx2_popout = true;
                self.rx2_popout_init_applied = false;
            }
            // toggle_vrx_spectrum sets popout + init + subscription itself.
            SnapWindow::Vrx1 => {
                if !self.vrx1_high_res_spectrum { self.toggle_vrx_spectrum(VrxChannel::Vrx1); }
            }
            SnapWindow::Vrx2 => {
                if !self.vrx2_high_res_spectrum { self.toggle_vrx_spectrum(VrxChannel::Vrx2); }
            }
            SnapWindow::Yaesu1 => { self.yaesu_popout = true; self.yaesu_popout_init_applied = false; }
            SnapWindow::Yaesu2 => { self.yaesu2_popout = true; self.yaesu2_popout_init_applied = false; }
        }
    }

    /// The viewport a pop-out is drawn in, so an ALREADY OPEN one can be moved
    /// with an explicit command. See `snap_move_now`.
    fn snap_viewport_id(w: SnapWindow) -> Option<egui::ViewportId> {
        let name = match w {
            SnapWindow::Main => return None, // root viewport, moved by the caller
            SnapWindow::Rx1 => "spectrum_popout",
            SnapWindow::Rx2 => "rx2_popout",
            SnapWindow::Vrx1 => "vrx1_popout",
            SnapWindow::Vrx2 => "vrx2_popout",
            SnapWindow::Yaesu1 => "yaesu_popout",
            SnapWindow::Yaesu2 => "yaesu2_popout",
        };
        Some(egui::ViewportId::from_hash_of(name))
    }

    /// Move a pop-out that is ALREADY OPEN, by command rather than by builder.
    ///
    /// Measured 2026-08-08 on the server, same mechanism here: a ViewportBuilder
    /// only takes effect for fields egui sees CHANGE, and egui compares against the
    /// last builder it was given - not against where the window actually is. Move a
    /// window by hand and then recall an arrangement that puts it back where egui
    /// already has it on record, and nothing happens. That is exactly the case when
    /// recalling a STORED arrangement: the values match what egui last saw, so the
    /// builder path is silent. A command carries no comparison and always lands.
    fn snap_move_now(&self, ctx: &egui::Context, w: SnapWindow, pos: egui::Pos2, size: egui::Vec2) {
        if !self.snap_is_open(w) { return; } // not open yet: the builder does it
        let Some(id) = Self::snap_viewport_id(w) else { return };
        let z = ctx.zoom_factor().max(0.01);
        ctx.send_viewport_cmd_to(id, egui::ViewportCommand::OuterPosition(
            egui::pos2(pos.x / z, pos.y / z)));
        ctx.send_viewport_cmd_to(id, egui::ViewportCommand::InnerSize(
            egui::vec2(size.x / z, size.y / z)));
    }

    /// Set position+size of a window and reset init_applied so
    /// apply_popout_geometry applies the new geometry the next frame
    /// (same mechanism as recenter_popouts). Does NOT save - the caller
    /// (apply_layout) does one save at the end.
    fn snap_set_geometry(&mut self, w: SnapWindow, pos: egui::Pos2, size: egui::Vec2) {
        match w {
            // The main window (root viewport) is not set via popout-pos fields
            // but via ViewportCommand in apply_layout (needs ctx).
            SnapWindow::Main => {}
            SnapWindow::Rx1 => { self.spectrum_popout_pos = Some(pos); self.spectrum_popout_size = Some(size); self.spectrum_popout_init_applied = false; }
            SnapWindow::Rx2 => { self.rx2_popout_pos = Some(pos); self.rx2_popout_size = Some(size); self.rx2_popout_init_applied = false; }
            SnapWindow::Vrx1 => { self.vrx_popout_pos = Some(pos); self.vrx_popout_size = Some(size); self.vrx_popout_init_applied = false; }
            SnapWindow::Vrx2 => { self.vrx2_popout_pos = Some(pos); self.vrx2_popout_size = Some(size); self.vrx2_popout_init_applied = false; }
            SnapWindow::Yaesu1 => { self.yaesu_popout_pos = Some(pos); self.yaesu_popout_size = Some(size); self.yaesu_popout_init_applied = false; }
            SnapWindow::Yaesu2 => { self.yaesu2_popout_pos = Some(pos); self.yaesu2_popout_size = Some(size); self.yaesu2_popout_init_applied = false; }
        }
    }

    /// Is this window currently OPEN? (`snap_is_available` asks whether it COULD
    /// be arranged; this asks whether it is on screen right now.) A VRX window is
    /// derived from its spectrum subscription - model B - so that flag is the one
    /// to read.
    fn snap_is_open(&self, w: SnapWindow) -> bool {
        match w {
            SnapWindow::Main => true,
            SnapWindow::Rx1 => self.spectrum_popout,
            SnapWindow::Rx2 => self.rx2_popout,
            SnapWindow::Vrx1 => self.vrx1_high_res_spectrum,
            SnapWindow::Vrx2 => self.vrx2_high_res_spectrum,
            SnapWindow::Yaesu1 => self.yaesu_popout,
            SnapWindow::Yaesu2 => self.yaesu2_popout,
        }
    }

    /// Current position+size of a window, the counterpart of `snap_set_geometry`.
    fn snap_geometry(&self, w: SnapWindow) -> Option<(egui::Pos2, egui::Vec2)> {
        match w {
            SnapWindow::Main => self.main_window_pos
                .map(|p| (p, egui::vec2(self.window_w, self.window_h))),
            SnapWindow::Rx1 => self.spectrum_popout_pos.zip(self.spectrum_popout_size),
            SnapWindow::Rx2 => self.rx2_popout_pos.zip(self.rx2_popout_size),
            SnapWindow::Vrx1 => self.vrx_popout_pos.zip(self.vrx_popout_size),
            SnapWindow::Vrx2 => self.vrx2_popout_pos.zip(self.vrx2_popout_size),
            SnapWindow::Yaesu1 => self.yaesu_popout_pos.zip(self.yaesu_popout_size),
            SnapWindow::Yaesu2 => self.yaesu2_popout_pos.zip(self.yaesu2_popout_size),
        }
    }

    /// Everything that is open right now, ready to store in a memory slot.
    fn capture_layout(&self) -> Vec<(SnapWindow, egui::Pos2, egui::Vec2)> {
        <SnapWindow as SnapTarget>::all().iter().copied()
            .filter(|w| self.snap_is_open(*w))
            .filter_map(|w| self.snap_geometry(w).map(|(p, s)| (w, p, s)))
            .collect()
    }

    /// Put the windows back where a stored arrangement had them. Windows that the
    /// server configuration no longer offers (e.g. Yaesu 2 unplugged) are skipped
    /// rather than force-opened.
    fn recall_layout(&mut self, ctx: &egui::Context, idx: usize) {
        let Some(mem) = self.layout_memories.get(idx).cloned() else { return };
        if mem.is_empty() { return; }
        let mut n = 0usize;
        for (w, pos, size) in mem.windows {
            if !self.snap_is_available(w) {
                // Not on offer yet - typically a recall done before there is a
                // connection, so the radios are unknown and their windows cannot be
                // opened. Giving up would leave a stored arrangement permanently
                // incomplete, so remember the intent and carry it out the moment the
                // window becomes available. The geometry is stored right away as
                // well, so opening it by hand in the meantime also lands correctly.
                log::info!("Arrangement {}: {:?} not available yet - queued until it appears",
                           idx + 1, w);
                self.snap_set_geometry(w, pos, size);
                self.layout_pending.retain(|(pw, _, _)| *pw != w);
                self.layout_pending.push((w, pos, size));
                continue;
            }
            self.snap_open(w);
            if w == SnapWindow::Main {
                self.main_window_pos = Some(pos);
                self.window_w = size.x;
                self.window_h = size.y;
                self.main_geom_dirty = true;
                // Same unit boundary as apply_layout: stored in system points, a
                // viewport command wants egui points.
                let z = ctx.zoom_factor().max(0.01);
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(
                    egui::pos2(pos.x / z, pos.y / z)));
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(
                    egui::vec2(size.x / z, size.y / z)));
            } else {
                self.snap_move_now(ctx, w, pos, size);
                self.snap_set_geometry(w, pos, size);
            }
            n += 1;
        }
        self.save_full_config();
        log::info!("Arrangement {} recalled: {} window(s) restored", idx + 1, n);
    }

    /// Index (in monitor_work_areas_px) of the monitor where the main window is.
    /// For the default screen choice when opening the Arrange window.
    pub(super) fn detect_monitor_index(&self, ctx: &egui::Context) -> usize {
        let ppp = Self::viewport_native_ppp(ctx).max(0.1);
        if let (Some(areas), Some((mx, my))) = (
            window_placement::monitor_work_areas_px(),
            ctx.input(|i| i.viewport().outer_rect)
                .map(|r| ((r.min.x * ppp) as i32, (r.min.y * ppp) as i32)),
        ) {
            if let Some(idx) = areas.iter().position(|a|
                mx >= a.left && mx < a.right && my >= a.top && my < a.bottom)
            {
                return idx;
            }
        }
        0
    }

    /// Snap the windows from the placement matrices onto the screen. Each window
    /// is stretched over its enclosing cell rectangle (rows x cols grid
    /// per monitor). The main window is moved via ViewportCommand; the
    /// popouts via snap_set_geometry.
    fn apply_layout(&mut self, ctx: &egui::Context) {
        let ppp = Self::viewport_native_ppp(ctx).max(0.1);
        let areas = window_placement::monitor_work_areas_px();
        let per_mon: Vec<LayoutGrid> = self.layout_grid_per_monitor.clone();
        const TITLE_H: f32 = 36.0;
        const GAP: f32 = 4.0;
        let mut placed_total = 0usize;
        for (m, grid) in per_mon.iter().enumerate() {
            let placed = grid.placed();
            if placed.is_empty() { continue; }
            // Work area of monitor m in points; screen 0 falls back to the egui screen.
            let (ax, ay, aw, ah) = match areas.as_ref().and_then(|list| list.get(m).copied()) {
                Some(a) => (
                    a.left as f32 / ppp,
                    a.top as f32 / ppp,
                    (a.right - a.left) as f32 / ppp,
                    (a.bottom - a.top) as f32 / ppp,
                ),
                None if m == 0 => {
                    let sr = ctx.screen_rect();
                    (sr.min.x, sr.min.y, sr.width(), sr.height())
                }
                None => continue,
            };
            let col_w = aw / grid.cols() as f32;
            let row_h = ah / grid.rows() as f32;
            for w in placed {
                let Some((minr, minc, maxr, maxc)) = grid.bounds(w) else { continue };
                // Open the window first (if still closed) so the snap becomes visible
                // - you can also place disabled windows in the matrix.
                self.snap_open(w);
                let span_c = (maxc - minc + 1) as f32;
                let span_r = (maxr - minr + 1) as f32;
                let x = ax + col_w * minc as f32;
                let y = ay + row_h * minr as f32;
                let iw = (col_w * span_c - GAP).max(200.0);
                let ih = (row_h * span_r - TITLE_H).max(120.0);
                if w == SnapWindow::Main {
                    // Root viewport: move directly + re-save. window_h
                    // is the client height (excl. title bar), like the popouts.
                    self.main_window_pos = Some(egui::pos2(x, y));
                    self.window_w = iw;
                    self.window_h = ih;
                    self.main_geom_dirty = true;
                    // Unit boundary. Everything above is in SYSTEM points (the work
                    // area came from the OS and was divided by the NATIVE scale), but
                    // egui-winit multiplies a viewport command by
                    // `zoom_factor * native_pixels_per_point` - so a command wants EGUI
                    // points. The window builder used when a pop-out opens takes system
                    // points instead, which is why the two paths need different units.
                    // Without this the main window came out a factor `zoom` too small:
                    // at 50% you would need a 24x24 grid to fill the screen.
                    let z = ctx.zoom_factor().max(0.01);
                    ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(x / z, y / z)));
                    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(iw / z, ih / z)));
                } else {
                    self.snap_move_now(ctx, w, egui::pos2(x, y), egui::vec2(iw, ih));
                    self.snap_set_geometry(w, egui::pos2(x, y), egui::vec2(iw, ih));
                }
                placed_total += 1;
            }
        }
        if placed_total > 0 {
            self.save_full_config();
            log::info!("Layout applied: {} window(s) snapped", placed_total);
        }
    }

    /// Place windows a recall could not reach yet, now that they exist.
    ///
    /// Called every frame; does nothing while the queue is empty, which is almost
    /// always. An entry is dropped once carried out, so a window is placed once and
    /// stays wherever the operator puts it afterwards.
    pub(super) fn apply_pending_layout(&mut self, ctx: &egui::Context) {
        if self.layout_pending.is_empty() { return; }
        let ready: Vec<_> = self.layout_pending.iter().copied()
            .filter(|(w, _, _)| self.snap_is_available(*w))
            .collect();
        if ready.is_empty() { return; }
        for (w, pos, size) in ready {
            self.layout_pending.retain(|(pw, _, _)| *pw != w);
            self.snap_open(w);
            self.snap_move_now(ctx, w, pos, size);
            self.snap_set_geometry(w, pos, size);
            log::info!("Queued arrangement: {:?} appeared - placed at {:.0},{:.0} {:.0}x{:.0}",
                       w, pos.x, pos.y, size.x, size.y);
        }
        self.save_full_config();
    }

    /// "Arrange windows" - matrix placer. Pick a grid size (e.g. 2x3),
    /// select a window from the palette and paint it across the cells (click
    /// or drag). A window may span multiple adjacent cells; it is
    /// stretched over its enclosing rectangle on "Apply". A separate grid per
    /// monitor.
    pub(super) fn render_layout_arranger(&mut self, ctx: &egui::Context) {
        if !self.show_layout_arranger { return; }

        // One grid per monitor (index = monitor).
        let n_monitors = window_placement::monitor_work_areas_px()
            .map(|a| a.len()).unwrap_or(1).max(1);
        if self.layout_grid_per_monitor.len() < n_monitors {
            self.layout_grid_per_monitor.resize_with(n_monitors, LayoutGrid::new);
        }
        if self.layout_target_monitor >= n_monitors { self.layout_target_monitor = 0; }

        // Remove unavailable windows (e.g. Yaesu 2 absent) from ALL grids + the
        // active selection.
        let avail: Vec<SnapWindow> = <SnapWindow as SnapTarget>::all().iter().copied()
            .filter(|w| self.snap_is_available(*w)).collect();
        for grid in self.layout_grid_per_monitor.iter_mut() {
            grid.retain_available(&avail);
        }
        if let Some(a) = self.layout_active_item {
            if !avail.contains(&a) { self.layout_active_item = None; }
        }

        let cur_mon = self.layout_target_monitor
            .min(self.layout_grid_per_monitor.len().saturating_sub(1));
        // Snapshot for drawing; all mutations are done after the closure.
        let gsnap = self.layout_grid_per_monitor[cur_mon].clone();
        let active = self.layout_active_item;
        let drag_anchor = self.layout_drag_anchor;

        let mut open_flag = self.show_layout_arranger;
        let mut sel_mon = cur_mon;
        let mut resize_to: Option<(u8, u8)> = None;
        let mut paint_cell: Option<(usize, usize)> = None; // paint `active` into cell
        let mut clear_cell: Option<(usize, usize)> = None; // clear cell
        let mut select: Option<Option<SnapWindow>> = None; // change palette selection
        let mut set_anchor: Option<Option<(usize, usize)>> = None; // set/clear drag anchor
        let mut fill_rect: Option<((usize, usize), (usize, usize))> = None; // fill rectangle
        let mut clear_all = false;
        let mut do_apply = false;
        // The memory slots are edited inside the closure, which must not borrow
        // `self` - same snapshot-then-apply pattern as the grid above.
        self.layout_memories.resize_with(LAYOUT_MEM_SLOTS, LayoutMemory::default);
        let mem_names_before: Vec<String> =
            self.layout_memories.iter().map(|m| m.name.clone()).collect();
        let mut mem_names = mem_names_before.clone();
        let mem_counts: Vec<usize> =
            self.layout_memories.iter().map(|m| m.windows.len()).collect();
        let mut save_slot: Option<usize> = None;
        let mut recall_slot: Option<usize> = None;
        let mut names_changed = false;

        egui::Window::new(rust_i18n::t!("main_arrange_windows_title").to_string())
            .open(&mut open_flag)
            .resizable(true)
            // Wide enough for the 18x18 size-picker (18 cells of 15 points plus
            // the gaps is already ~300) and tall enough for picker + palette +
            // matrix + the memory slots, so the window is usable the moment it
            // opens instead of needing a resize first.
            .default_size([470.0, 780.0])
            .show(ctx, |ui| {
              egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
                ui.label(RichText::new(
                    rust_i18n::t!("main_arrange_windows_help").to_string()
                ).size(11.0).weak());
                ui.add_space(6.0);

                // Screen choice (only with multiple monitors).
                if let Some(areas) = window_placement::monitor_work_areas_px() {
                    if areas.len() > 1 {
                        ui.horizontal(|ui| {
                            ui.label(rust_i18n::t!("screen").to_string());
                            egui::ComboBox::from_id_source("layout_monitor")
                                .selected_text(rust_i18n::t!("main_screen_n", n = sel_mon + 1).to_string())
                                .show_ui(ui, |ui| {
                                    for i in 0..areas.len() {
                                        ui.selectable_value(&mut sel_mon, i, rust_i18n::t!("main_screen_n", n = i + 1).to_string());
                                    }
                                });
                        });
                        ui.add_space(6.0);
                    }
                }

                resize_to = sdr_remote_layout::grid_size_picker(
                    ui, &rust_i18n::t!("main_grid_size").to_string(),
                    gsnap.rows(), gsnap.cols(), theme::TL_SELECTED_FILL);

                ui.add_space(6.0);
                ui.separator();

                ui.label(rust_i18n::t!("main_pick_window").to_string());
                select = sdr_remote_layout::window_palette(ui, &avail, active);

                ui.add_space(6.0);
                ui.separator();

                ui.label(rust_i18n::t!("main_place_hint").to_string());
                let intents = sdr_remote_layout::placement_grid(ui, &gsnap, active, drag_anchor);
                if let Some(sel) = intents.select { select = Some(sel); }
                paint_cell = intents.paint_cell;
                clear_cell = intents.clear_cell;
                set_anchor = intents.set_anchor;
                fill_rect = intents.fill_rect;

                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(RichText::new(rust_i18n::t!("main_apply").to_string()).strong())
                        .fill(theme::TL_SELECTED_FILL)).clicked() { do_apply = true; }
                    if ui.button(rust_i18n::t!("main_empty").to_string()).clicked() { clear_all = true; }
                });

                // --- Arrangement memories: store/restore actual window positions ---
                ui.add_space(8.0);
                ui.separator();
                ui.label(rust_i18n::t!("main_layout_memory").to_string());
                ui.label(RichText::new(
                    rust_i18n::t!("main_layout_memory_help").to_string()
                ).size(11.0).weak());
                ui.add_space(4.0);
                for i in 0..LAYOUT_MEM_SLOTS {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{}", i + 1)).strong().monospace());
                        // Save on lost focus, not per keystroke: the name lives in the
                        // full config file, and rewriting that on every character typed
                        // is a file write per keypress for nothing.
                        let name_resp = ui.add(egui::TextEdit::singleline(&mut mem_names[i])
                            .desired_width(120.0)
                            .hint_text(rust_i18n::t!("main_layout_memory_name").to_string()));
                        if name_resp.lost_focus() && mem_names[i] != mem_names_before[i] {
                            names_changed = true;
                        }
                        if ui.button(rust_i18n::t!("main_layout_memory_store").to_string())
                            .on_hover_text(rust_i18n::t!("main_layout_memory_store_hint").to_string())
                            .clicked()
                        {
                            save_slot = Some(i);
                        }
                        let filled = mem_counts[i] > 0;
                        let mut recall = egui::Button::new(
                            rust_i18n::t!("main_layout_memory_recall").to_string());
                        if filled { recall = recall.fill(theme::TL_SELECTED_FILL); }
                        if ui.add_enabled(filled, recall).clicked() { recall_slot = Some(i); }
                        if filled {
                            ui.label(RichText::new(
                                rust_i18n::t!("main_layout_memory_count", n = mem_counts[i]).to_string()
                            ).size(11.0).weak());
                        }
                    });
                }

              }); // ScrollArea
            });

        // --- Apply intents to the state (after the closure) ---
        self.show_layout_arranger = open_flag;
        for (slot, name) in self.layout_memories.iter_mut().zip(mem_names.into_iter()) {
            slot.name = name;
        }
        if let Some(i) = save_slot {
            let windows = self.capture_layout();
            let n = windows.len();
            if let Some(slot) = self.layout_memories.get_mut(i) { slot.windows = windows; }
            self.save_full_config();
            log::info!("Arrangement {} stored: {} window(s)", i + 1, n);
        } else if names_changed {
            self.save_full_config();
        }
        if let Some(i) = recall_slot { self.recall_layout(ctx, i); }
        if sel_mon != cur_mon && sel_mon < self.layout_grid_per_monitor.len() {
            self.layout_target_monitor = sel_mon;
        }
        if let Some(sel) = select { self.layout_active_item = sel; }
        let mon = cur_mon;
        if let Some((rr, cc)) = resize_to { self.layout_grid_per_monitor[mon].set_size(rr, cc); }
        if clear_all {
            self.layout_grid_per_monitor[mon].clear_all();
        }
        if let Some((r, c)) = clear_cell { self.layout_grid_per_monitor[mon].set(r, c, None); }
        if let Some((r, c)) = paint_cell {
            if let Some(w) = active {
                // Remove this window first from ALL grids (incl. the current
                // monitor) so old cells don't linger and bounds() doesn't
                // unintentionally stretch the window over a larger rectangle.
                for g in self.layout_grid_per_monitor.iter_mut() { g.remove(w); }
                self.layout_grid_per_monitor[mon].set(r, c, Some(w));
            }
        }
        if let Some(sa) = set_anchor { self.layout_drag_anchor = sa; }
        if let Some((a, b)) = fill_rect {
            if let Some(w) = active {
                // Remove this window first from ALL grids (incl. the current
                // monitor) so old cells don't linger and bounds() doesn't
                // unintentionally stretch the window over a larger rectangle.
                for g in self.layout_grid_per_monitor.iter_mut() { g.remove(w); }
                let (r0, r1) = (a.0.min(b.0), a.0.max(b.0));
                let (c0, c1) = (a.1.min(b.1), a.1.max(b.1));
                let g = &mut self.layout_grid_per_monitor[mon];
                for r in r0..=r1 { for c in c0..=c1 { g.set(r, c, Some(w)); } }
            }
        }
        if do_apply { self.apply_layout(ctx); }
    }
}
