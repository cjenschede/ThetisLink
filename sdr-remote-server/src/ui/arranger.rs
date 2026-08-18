// SPDX-License-Identifier: GPL-2.0-or-later
//! Server-GUI window arranger: the "Vensters schikken" snap-grid layout
//! (place/size the pop-out viewports onto a monitor grid) plus the window-
//! position persistence + monitor detection helpers. Extracted verbatim from
//! `ui/mod.rs` - pure relocation, no behaviour change. `use super::*;` reaches
//! the shared SnapWindow/LayoutGrid/Mode types + consts; `pub(super)` keeps the
//! arranger render + save + monitor-detect callable from the update loop.

use super::*;

impl ServerApp {
    pub(super) fn save_window_positions(&self) {
        let mut config = crate::config::load();
        config.tuner_window_pos = self.tuner_window_pos;
        config.amplitec_window_pos = self.amplitec_window_pos;
        config.spe_window_pos = self.spe_window_pos;
        config.rf2k_window_pos = self.rf2k_window_pos;
        config.chat_window_pos = self.chat_window_pos;
        config.ultrabeam_window_pos = self.ultrabeam_window_pos;
        config.rotor_window_pos = self.rotor_window_pos;
        config.main_window_pos = self.main_window_pos;
        config.main_window_size = self.main_window_size;
        config.tuner_window_size = self.tuner_window_size;
        config.amplitec_window_size = self.amplitec_window_size;
        config.spe_window_size = self.spe_window_size;
        config.rf2k_window_size = self.rf2k_window_size;
        config.chat_window_size = self.chat_window_size;
        config.show_chat_window = self.show_chat_window;
        config.ultrabeam_window_size = self.ultrabeam_window_size;
        config.rotor_window_size = self.rotor_window_size;
        config.layout_grids = sdr_remote_layout::layout_grids_to_config(&self.layout_grid_per_monitor);
        config.layout_memories = self.layout_memories.iter()
            .map(|m| m.to_config_string()).collect();
        config.ui_zoom = self.ui_zoom;
        config.theme = self.theme_variant.as_str().to_string();
        config.theme_custom = self.theme_custom.to_config_string();
        config.language = self.ui_language.clone();
        config.active_pa = self.active_pa.load(Ordering::Relaxed);
        crate::config::save(&config);
    }

    // ===== "Vensters schikken" matrix placer =====

    fn viewport_native_ppp(ctx: &egui::Context) -> f32 {
        ctx.input(|i| i.viewport().native_pixels_per_point)
            .unwrap_or_else(|| ctx.pixels_per_point())
    }

    /// Is this window AVAILABLE to arrange? = that backend is running (Arc exists).
    /// The main window is always there.
    fn snap_is_available(&self, w: SnapWindow) -> bool {
        match w {
            SnapWindow::Main => true,
            SnapWindow::Tuner => self.tuner.is_some(),
            SnapWindow::Amplitec => self.amplitec.is_some(),
            SnapWindow::Spe => self.spe.is_some(),
            SnapWindow::Rf2k => self.rf2k.is_some(),
            SnapWindow::Ultrabeam => self.ultrabeam.is_some(),
            SnapWindow::Rotor => self.rotor.is_some(),
        }
    }

    /// Open the corresponding popout window (if still closed) + reset init so
    /// the new geometry is applied. This makes a placed-but-closed window
    /// appear after "Toepassen".
    fn snap_open(&mut self, w: SnapWindow) {
        match w {
            SnapWindow::Main => {}
            SnapWindow::Tuner => { self.show_tuner_window = true; self.tuner_window_init_applied = false; }
            SnapWindow::Amplitec => { self.show_amplitec_window = true; self.amplitec_window_init_applied = false; }
            SnapWindow::Spe => { self.show_spe_window = true; self.spe_window_init_applied = false; }
            SnapWindow::Rf2k => { self.show_rf2k_window = true; self.rf2k_window_init_applied = false; }
            SnapWindow::Ultrabeam => { self.show_ultrabeam_window = true; self.ultrabeam_window_init_applied = false; }
            SnapWindow::Rotor => { self.show_rotor_window = true; self.rotor_window_init_applied = false; }
        }
    }

    /// The viewport this window is drawn in, so an ALREADY OPEN one can be moved
    /// with an explicit command. See `snap_move_now` for why that is needed.
    fn snap_viewport_id(w: SnapWindow) -> Option<egui::ViewportId> {
        let name = match w {
            SnapWindow::Main => return None, // the root viewport, moved by the caller
            SnapWindow::Tuner => "tuner_control",
            SnapWindow::Amplitec => "amplitec_control",
            SnapWindow::Spe => "spe_expert_control",
            SnapWindow::Rf2k => "rf2k_control",
            SnapWindow::Ultrabeam => "ultrabeam_control",
            SnapWindow::Rotor => "rotor_control",
        };
        Some(egui::ViewportId::from_hash_of(name))
    }

    /// Move a window that is ALREADY OPEN, by command rather than by builder.
    ///
    /// Measured 2026-08-08: the RF2K-S window kept its old position while the log
    /// showed the correct one being handed to its ViewportBuilder. A builder only
    /// takes effect for fields egui sees CHANGE, and egui compares against the last
    /// builder it was given - not against where the window actually is. Drag a
    /// window by hand and then arrange it back to the position egui already has on
    /// record, and nothing happens: egui believes it is already there. It stood out
    /// on the RF2K-S because that was the one window whose grid position equalled
    /// its stored one; the others differed and so did move.
    ///
    /// A command carries no such comparison, so it always lands.
    fn snap_move_now(&self, ctx: &egui::Context, w: SnapWindow, pos: [f32; 2], size: [f32; 2]) {
        if !self.snap_is_open(w) { return; } // not open yet: the builder does it
        let Some(id) = Self::snap_viewport_id(w) else { return };
        let z = ctx.zoom_factor().max(0.01);
        ctx.send_viewport_cmd_to(id, egui::ViewportCommand::OuterPosition(
            egui::pos2(pos[0] / z, pos[1] / z)));
        ctx.send_viewport_cmd_to(id, egui::ViewportCommand::InnerSize(
            egui::vec2(size[0] / z, size[1] / z)));
    }

    /// Set pos+size of a popout (root viewport via ViewportCommand in apply_layout).
    fn snap_set_geometry(&mut self, w: SnapWindow, pos: [f32; 2], size: [f32; 2]) {
        match w {
            SnapWindow::Main => {}
            SnapWindow::Tuner => { self.tuner_window_pos = Some(pos); self.tuner_window_size = Some(size); self.tuner_window_init_applied = false; }
            SnapWindow::Amplitec => { self.amplitec_window_pos = Some(pos); self.amplitec_window_size = Some(size); self.amplitec_window_init_applied = false; }
            SnapWindow::Spe => { self.spe_window_pos = Some(pos); self.spe_window_size = Some(size); self.spe_window_init_applied = false; }
            SnapWindow::Rf2k => { self.rf2k_window_pos = Some(pos); self.rf2k_window_size = Some(size); self.rf2k_window_init_applied = false; }
            SnapWindow::Ultrabeam => { self.ultrabeam_window_pos = Some(pos); self.ultrabeam_window_size = Some(size); self.ultrabeam_window_init_applied = false; }
            SnapWindow::Rotor => { self.rotor_window_pos = Some(pos); self.rotor_window_size = Some(size); self.rotor_window_init_applied = false; }
        }
    }

    /// Index of the monitor the main window is on (default screen on open).
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

    /// Snap the windows from the matrices onto the screen: each window over its
    /// bounding cell rectangle. Main window via ViewportCommand, popouts via
    /// snap_set_geometry (+ open first).
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
                self.snap_open(w);
                let span_c = (maxc - minc + 1) as f32;
                let span_r = (maxr - minr + 1) as f32;
                let x = ax + col_w * minc as f32;
                let y = ay + row_h * minr as f32;
                let iw = (col_w * span_c - GAP).max(200.0);
                let ih = (row_h * span_r - TITLE_H).max(120.0);
                if w == SnapWindow::Main {
                    self.main_window_pos = Some([x, y]);
                    self.main_window_size = Some([iw, ih]);
                    // Unit boundary. Everything above is in SYSTEM points (the work
                    // area came from the OS and was divided by the NATIVE scale), but
                    // egui-winit multiplies a viewport command by
                    // `zoom_factor * native_pixels_per_point` - so a command wants EGUI
                    // points. Without this the main window comes out a factor `zoom`
                    // too small once the UI scale is not 100%.
                    let z = ctx.zoom_factor().max(0.01);
                    ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(x / z, y / z)));
                    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(iw / z, ih / z)));
                } else {
                    self.snap_move_now(ctx, w, [x, y], [iw, ih]);
                    self.snap_set_geometry(w, [x, y], [iw, ih]);
                }
                placed_total += 1;
            }
        }
        if placed_total > 0 {
            self.save_window_positions();
            log::info!("Layout applied: {} window(s) snapped", placed_total);
        }
    }

    /// "Vensters schikken" - matrix placer. Pick a grid size (up to 8x8),
    /// select a window and paint it over the cells (click/drag). Spanning
    /// multiple cells = window stretched. A separate grid per monitor.
    /// Is this window currently OPEN? (`snap_is_available` asks whether it COULD
    /// be arranged.) Needed to store an arrangement: only what is on screen.
    fn snap_is_open(&self, w: SnapWindow) -> bool {
        match w {
            SnapWindow::Main => true,
            SnapWindow::Tuner => self.show_tuner_window,
            SnapWindow::Amplitec => self.show_amplitec_window,
            SnapWindow::Spe => self.show_spe_window,
            SnapWindow::Rf2k => self.show_rf2k_window,
            SnapWindow::Ultrabeam => self.show_ultrabeam_window,
            SnapWindow::Rotor => self.show_rotor_window,
        }
    }

    /// Current position+size, the counterpart of `snap_set_geometry`.
    fn snap_geometry(&self, w: SnapWindow) -> Option<([f32; 2], [f32; 2])> {
        match w {
            SnapWindow::Main => self.main_window_pos.zip(self.main_window_size),
            SnapWindow::Tuner => self.tuner_window_pos.zip(self.tuner_window_size),
            SnapWindow::Amplitec => self.amplitec_window_pos.zip(self.amplitec_window_size),
            SnapWindow::Spe => self.spe_window_pos.zip(self.spe_window_size),
            SnapWindow::Rf2k => self.rf2k_window_pos.zip(self.rf2k_window_size),
            SnapWindow::Ultrabeam => self.ultrabeam_window_pos.zip(self.ultrabeam_window_size),
            SnapWindow::Rotor => self.rotor_window_pos.zip(self.rotor_window_size),
        }
    }

    /// Everything open right now, ready to store in a memory slot.
    fn capture_layout(&self) -> Vec<(SnapWindow, egui::Pos2, egui::Vec2)> {
        <SnapWindow as SnapTarget>::all().iter().copied()
            .filter(|w| self.snap_is_open(*w))
            .filter_map(|w| self.snap_geometry(w)
                .map(|(pp, sz)| (w, egui::pos2(pp[0], pp[1]), egui::vec2(sz[0], sz[1]))))
            .collect()
    }

    /// Put the windows back where a stored arrangement had them. A window the
    /// server no longer offers (backend not running) is skipped, not force-opened.
    fn recall_layout(&mut self, ctx: &egui::Context, idx: usize) {
        let Some(mem) = self.layout_memories.get(idx).cloned() else { return };
        if mem.is_empty() { return; }
        let mut n = 0usize;
        for (w, pos, size) in mem.windows {
            if !self.snap_is_available(w) {
                // Not on offer yet - that backend is not running. Same rule as the
                // client: remember the intent, store the geometry so a manual open
                // lands correctly, and place it when the device appears.
                log::info!("Arrangement {}: {:?} not available yet - queued until it appears",
                           idx + 1, w);
                self.snap_set_geometry(w, [pos.x, pos.y], [size.x, size.y]);
                self.layout_pending.retain(|(pw, _, _)| *pw != w);
                self.layout_pending.push((w, [pos.x, pos.y], [size.x, size.y]));
                continue;
            }
            self.snap_open(w);
            if w == SnapWindow::Main {
                self.main_window_pos = Some([pos.x, pos.y]);
                self.main_window_size = Some([size.x, size.y]);
                // Same unit boundary as apply_layout: stored in system points, a
                // viewport command wants egui points.
                let z = ctx.zoom_factor().max(0.01);
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(
                    egui::pos2(pos.x / z, pos.y / z)));
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(
                    egui::vec2(size.x / z, size.y / z)));
            } else {
                self.snap_move_now(ctx, w, [pos.x, pos.y], [size.x, size.y]);
                self.snap_set_geometry(w, [pos.x, pos.y], [size.x, size.y]);
            }
            n += 1;
        }
        self.save_window_positions();
        log::info!("Arrangement {} recalled: {} window(s) restored", idx + 1, n);
    }

    /// Place windows a recall could not reach yet, now that the device exists.
    /// Called every frame; does nothing while the queue is empty.
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
                       w, pos[0], pos[1], size[0], size[1]);
        }
        self.save_window_positions();
    }

    /// "Vensters schikken" - matrix placer. Pick a grid size, select a window and
    /// paint it over the cells (click/drag). Spanning multiple cells stretches the
    /// window. A separate grid per monitor, plus stored arrangements.
    ///
    /// The grid, the picker, the palette and the memories come from the shared
    /// arranger crate, so this window and the client's stay the same by
    /// construction instead of being kept in step by hand.
    pub(super) fn render_layout_arranger(&mut self, ctx: &egui::Context) {
        if !self.show_layout_arranger { return; }

        let n_monitors = window_placement::monitor_work_areas_px()
            .map(|a| a.len()).unwrap_or(1).max(1);
        if self.layout_grid_per_monitor.len() < n_monitors {
            self.layout_grid_per_monitor.resize_with(n_monitors, LayoutGrid::new);
        }
        if self.layout_target_monitor >= n_monitors { self.layout_target_monitor = 0; }

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
        let gsnap = self.layout_grid_per_monitor[cur_mon].clone();
        let active = self.layout_active_item;
        let drag_anchor = self.layout_drag_anchor;

        let mut open_flag = self.show_layout_arranger;
        let mut sel_mon = cur_mon;
        let mut resize_to: Option<(u8, u8)> = None;
        let mut intents = sdr_remote_layout::PlacementIntents::<SnapWindow>::default();
        let mut select: Option<Option<SnapWindow>> = None;
        let mut clear_all = false;
        let mut do_apply = false;
        // Memory slots are edited inside the closure, which must not borrow `self` -
        // same snapshot-then-apply pattern as the grid above.
        self.layout_memories.resize_with(LAYOUT_MEM_SLOTS, LayoutMemory::default);
        let mem_names_before: Vec<String> =
            self.layout_memories.iter().map(|m| m.name.clone()).collect();
        let mut mem_names = mem_names_before.clone();
        let mem_counts: Vec<usize> =
            self.layout_memories.iter().map(|m| m.windows.len()).collect();
        let mut save_slot: Option<usize> = None;
        let mut recall_slot: Option<usize> = None;
        let mut names_changed = false;

        egui::Window::new(rust_i18n::t!("srv_arrange_title").to_string())
            .open(&mut open_flag)
            .resizable(true)
            // Wide enough for the 18x18 picker and tall enough for picker, palette,
            // matrix and memory slots, so it is usable the moment it opens.
            .default_size([470.0, 780.0])
            .show(ctx, |ui| {
              egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
                ui.label(RichText::new(rust_i18n::t!("srv_arrange_help").to_string()).size(11.0).weak());
                ui.add_space(6.0);

                if let Some(areas) = window_placement::monitor_work_areas_px() {
                    if areas.len() > 1 {
                        ui.horizontal(|ui| {
                            ui.label(rust_i18n::t!("srv_screen").to_string());
                            egui::ComboBox::from_id_salt("srv_layout_monitor")
                                .selected_text(rust_i18n::t!("srv_screen_n", n = sel_mon + 1).to_string())
                                .show_ui(ui, |ui| {
                                    for i in 0..areas.len() {
                                        ui.selectable_value(&mut sel_mon, i, rust_i18n::t!("srv_screen_n", n = i + 1).to_string());
                                    }
                                });
                        });
                        ui.add_space(6.0);
                    }
                }

                resize_to = sdr_remote_layout::grid_size_picker(
                    ui, &rust_i18n::t!("srv_grid").to_string(),
                    gsnap.rows(), gsnap.cols(), SNAP_ACCENT);

                ui.add_space(6.0);
                ui.separator();
                ui.label(rust_i18n::t!("srv_arrange_pick").to_string());
                select = sdr_remote_layout::window_palette(ui, &avail, active);

                ui.add_space(6.0);
                ui.separator();
                ui.label(rust_i18n::t!("srv_arrange_place").to_string());
                intents = sdr_remote_layout::placement_grid(ui, &gsnap, active, drag_anchor);
                if let Some(sel) = intents.select.take() { select = Some(sel); }

                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(RichText::new(rust_i18n::t!("srv_apply").to_string()).strong())
                        .fill(SNAP_ACCENT)).clicked() { do_apply = true; }
                    if ui.button(rust_i18n::t!("srv_empty").to_string()).clicked() { clear_all = true; }
                });

                // --- Arrangement memories: store/restore actual window positions ---
                ui.add_space(8.0);
                ui.separator();
                ui.label(rust_i18n::t!("srv_layout_memory").to_string());
                ui.label(RichText::new(rust_i18n::t!("srv_layout_memory_help").to_string())
                    .size(11.0).weak());
                ui.add_space(4.0);
                for i in 0..LAYOUT_MEM_SLOTS {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{}", i + 1)).strong().monospace());
                        // Save on lost focus, not per keystroke: the name lives in the
                        // config file, and rewriting that per character is a file write
                        // per keypress for nothing.
                        let name_resp = ui.add(egui::TextEdit::singleline(&mut mem_names[i])
                            .desired_width(120.0)
                            .hint_text(rust_i18n::t!("srv_layout_memory_name").to_string()));
                        if name_resp.lost_focus() && mem_names[i] != mem_names_before[i] {
                            names_changed = true;
                        }
                        if ui.button(rust_i18n::t!("srv_layout_memory_store").to_string())
                            .on_hover_text(rust_i18n::t!("srv_layout_memory_store_hint").to_string())
                            .clicked()
                        {
                            save_slot = Some(i);
                        }
                        let filled = mem_counts[i] > 0;
                        let mut recall = egui::Button::new(
                            rust_i18n::t!("srv_layout_memory_recall").to_string());
                        if filled { recall = recall.fill(SNAP_ACCENT); }
                        if ui.add_enabled(filled, recall).clicked() { recall_slot = Some(i); }
                        if filled {
                            ui.label(RichText::new(
                                rust_i18n::t!("srv_layout_memory_count", n = mem_counts[i]).to_string()
                            ).size(11.0).weak());
                        }
                    });
                }
              }); // ScrollArea
            });

        self.show_layout_arranger = open_flag;
        for (slot, name) in self.layout_memories.iter_mut().zip(mem_names.into_iter()) {
            slot.name = name;
        }
        if let Some(i) = save_slot {
            let windows = self.capture_layout();
            let n = windows.len();
            if let Some(slot) = self.layout_memories.get_mut(i) { slot.windows = windows; }
            self.save_window_positions();
            log::info!("Arrangement {} stored: {} window(s)", i + 1, n);
        } else if names_changed {
            self.save_window_positions();
        }
        if sel_mon != cur_mon && sel_mon < self.layout_grid_per_monitor.len() {
            self.layout_target_monitor = sel_mon;
        }
        if let Some(sel) = select { self.layout_active_item = sel; }
        let mon = cur_mon;
        if let Some((rr, cc)) = resize_to { self.layout_grid_per_monitor[mon].set_size(rr, cc); }
        if clear_all { self.layout_grid_per_monitor[mon].clear_all(); }
        if let Some((r, c)) = intents.clear_cell { self.layout_grid_per_monitor[mon].set(r, c, None); }
        if let Some((r, c)) = intents.paint_cell {
            if let Some(w) = active {
                // Remove this window from ALL grids first so old cells do not linger
                // and bounds() cannot stretch it over a larger rectangle.
                for g in self.layout_grid_per_monitor.iter_mut() { g.remove(w); }
                self.layout_grid_per_monitor[mon].set(r, c, Some(w));
            }
        }
        if let Some(sa) = intents.set_anchor { self.layout_drag_anchor = sa; }
        if let Some((a, b)) = intents.fill_rect {
            if let Some(w) = active {
                for g in self.layout_grid_per_monitor.iter_mut() { g.remove(w); }
                let (r0, r1) = (a.0.min(b.0), a.0.max(b.0));
                let (c0, c1) = (a.1.min(b.1), a.1.max(b.1));
                let g = &mut self.layout_grid_per_monitor[mon];
                for r in r0..=r1 { for c in c0..=c1 { g.set(r, c, Some(w)); } }
            }
        }
        if let Some(i) = recall_slot { self.recall_layout(ctx, i); }
        if do_apply { self.apply_layout(ctx); }
    }
}
