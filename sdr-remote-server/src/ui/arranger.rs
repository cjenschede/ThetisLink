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
        config.ultrabeam_window_pos = self.ultrabeam_window_pos;
        config.rotor_window_pos = self.rotor_window_pos;
        config.main_window_pos = self.main_window_pos;
        config.main_window_size = self.main_window_size;
        config.tuner_window_size = self.tuner_window_size;
        config.amplitec_window_size = self.amplitec_window_size;
        config.spe_window_size = self.spe_window_size;
        config.rf2k_window_size = self.rf2k_window_size;
        config.ultrabeam_window_size = self.ultrabeam_window_size;
        config.rotor_window_size = self.rotor_window_size;
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
            let col_w = aw / grid.cols as f32;
            let row_h = ah / grid.rows as f32;
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
                    ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(x, y)));
                    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(iw, ih)));
                } else {
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
    pub(super) fn render_layout_arranger(&mut self, ctx: &egui::Context) {
        if !self.show_layout_arranger { return; }

        let n_monitors = window_placement::monitor_work_areas_px()
            .map(|a| a.len()).unwrap_or(1).max(1);
        if self.layout_grid_per_monitor.len() < n_monitors {
            self.layout_grid_per_monitor.resize_with(n_monitors, LayoutGrid::new);
        }
        if self.layout_target_monitor >= n_monitors { self.layout_target_monitor = 0; }

        // Remove unavailable windows (backend not active) from all grids + selection.
        let avail: Vec<SnapWindow> =
            SnapWindow::ALL.iter().copied().filter(|w| self.snap_is_available(*w)).collect();
        for grid in self.layout_grid_per_monitor.iter_mut() {
            for slot in grid.cells.iter_mut() {
                if let Some(w) = *slot { if !avail.contains(&w) { *slot = None; } }
            }
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
        let mut paint_cell: Option<(usize, usize)> = None;
        let mut clear_cell: Option<(usize, usize)> = None;
        let mut select: Option<Option<SnapWindow>> = None;
        let mut set_anchor: Option<Option<(usize, usize)>> = None;
        let mut fill_rect: Option<((usize, usize), (usize, usize))> = None;
        let mut clear_all = false;
        let mut do_apply = false;

        egui::Window::new(rust_i18n::t!("srv_arrange_title").to_string())
            .open(&mut open_flag)
            .resizable(true)
            .default_size([360.0, 560.0])
            .show(ctx, |ui| {
              egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
                ui.label(RichText::new(rust_i18n::t!("srv_arrange_help").to_string()).size(11.0).weak());
                ui.add_space(6.0);

                if let Some(areas) = window_placement::monitor_work_areas_px() {
                    if areas.len() > 1 {
                        ui.horizontal(|ui| {
                            ui.label(rust_i18n::t!("srv_screen").to_string());
                            egui::ComboBox::from_id_source("srv_layout_monitor")
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

                // --- Grid-size picker (up to GRID_MAX x GRID_MAX; hover/click) ---
                const MAXG: usize = GRID_MAX;
                const PCELL: f32 = 15.0;
                const PGAP: f32 = 2.0;
                let dim = PCELL * MAXG as f32 + PGAP * (MAXG as f32 - 1.0);
                ui.horizontal(|ui| {
                    ui.label(rust_i18n::t!("srv_grid").to_string());
                    let (prect, presp) =
                        ui.allocate_exact_size(egui::vec2(dim, dim), egui::Sense::click());
                    let hover = presp.hover_pos().map(|p| {
                        let c = (((p.x - prect.min.x) / (PCELL + PGAP)) as i32).clamp(0, MAXG as i32 - 1) as usize;
                        let r = (((p.y - prect.min.y) / (PCELL + PGAP)) as i32).clamp(0, MAXG as i32 - 1) as usize;
                        (r, c)
                    });
                    let pnt = ui.painter_at(prect);
                    for r in 0..MAXG {
                        for c in 0..MAXG {
                            let cr = egui::Rect::from_min_size(
                                prect.min + egui::vec2(c as f32 * (PCELL + PGAP), r as f32 * (PCELL + PGAP)),
                                egui::vec2(PCELL, PCELL),
                            );
                            let preview = hover.map_or(false, |(hr, hc)| r <= hr && c <= hc);
                            let cur = r < gsnap.rows as usize && c < gsnap.cols as usize;
                            let fill = if preview {
                                SNAP_ACCENT
                            } else if cur {
                                Color32::from_gray(80)
                            } else {
                                Color32::from_gray(45)
                            };
                            pnt.rect_filled(cr, egui::Rounding::same(2.0), fill);
                            pnt.rect_stroke(cr, egui::Rounding::same(2.0), egui::Stroke::new(1.0, Color32::from_gray(100)));
                        }
                    }
                    if let Some((hr, hc)) = hover {
                        if presp.clicked() { resize_to = Some((hr as u8 + 1, hc as u8 + 1)); }
                    }
                    let (lr, lc) = hover
                        .map(|(r, c)| (r + 1, c + 1))
                        .unwrap_or((gsnap.rows as usize, gsnap.cols as usize));
                    ui.label(RichText::new(format!("{} x {}", lr, lc)).strong());
                });

                ui.add_space(6.0);
                ui.separator();

                ui.label(rust_i18n::t!("srv_arrange_pick").to_string());
                ui.horizontal_wrapped(|ui| {
                    for w in &avail {
                        let is_active = active == Some(*w);
                        let txt = RichText::new(w.label()).size(12.0).color(Color32::WHITE);
                        let mut btn = egui::Button::new(txt)
                            .fill(w.color())
                            .rounding(egui::Rounding::same(4.0));
                        if is_active {
                            btn = btn.stroke(egui::Stroke::new(2.0, Color32::WHITE));
                        }
                        if ui.add(btn).clicked() {
                            select = Some(if is_active { None } else { Some(*w) });
                        }
                    }
                });

                ui.add_space(6.0);
                ui.separator();

                ui.label(rust_i18n::t!("srv_arrange_place").to_string());
                let cols = gsnap.cols as usize;
                let rows = gsnap.rows as usize;
                let avail_w = ui.available_width().clamp(120.0, 380.0);
                let cell = (avail_w / cols as f32).clamp(22.0, 88.0);
                let gw = cell * cols as f32;
                let gh = cell * rows as f32;
                let (grect, gresp) =
                    ui.allocate_exact_size(egui::vec2(gw, gh), egui::Sense::click_and_drag());
                let gp = ui.painter_at(grect);
                for r in 0..rows {
                    for c in 0..cols {
                        let cr = egui::Rect::from_min_size(
                            grect.min + egui::vec2(c as f32 * cell, r as f32 * cell),
                            egui::vec2(cell, cell),
                        ).shrink(1.5);
                        let w = gsnap.cell(r, c);
                        let fill = w.map(|w| w.color()).unwrap_or(Color32::from_gray(48));
                        gp.rect_filled(cr, egui::Rounding::same(3.0), fill);
                        gp.rect_stroke(cr, egui::Rounding::same(3.0), egui::Stroke::new(1.0, Color32::from_gray(100)));
                        if let Some(w) = w {
                            gp.text(
                                cr.center(),
                                egui::Align2::CENTER_CENTER,
                                w.label(),
                                egui::FontId::proportional((cell * 0.22).clamp(9.0, 13.0)),
                                Color32::WHITE,
                            );
                        }
                    }
                }
                // Cell under the pointer: `inside` = strictly within the grid
                // (click/right-click); `clamped` = clamped to the edge (dragging).
                let ptr = gresp.interact_pointer_pos();
                let clamped = ptr.map(|p| {
                    let c = (((p.x - grect.min.x) / cell) as i32).clamp(0, cols as i32 - 1) as usize;
                    let r = (((p.y - grect.min.y) / cell) as i32).clamp(0, rows as i32 - 1) as usize;
                    (r, c)
                });
                let inside = ptr.and_then(|p| {
                    if !grect.contains(p) { return None; }
                    let c = ((p.x - grect.min.x) / cell) as usize;
                    let r = ((p.y - grect.min.y) / cell) as usize;
                    if r < rows && c < cols { Some((r, c)) } else { None }
                });
                // Drag preview: highlight rectangle anchor..current.
                if gresp.dragged() {
                    if let (Some(a), Some(cur), Some(w)) = (drag_anchor, clamped, active) {
                        let (r0, r1) = (a.0.min(cur.0), a.0.max(cur.0));
                        let (c0, c1) = (a.1.min(cur.1), a.1.max(cur.1));
                        let pr = egui::Rect::from_min_max(
                            grect.min + egui::vec2(c0 as f32 * cell, r0 as f32 * cell),
                            grect.min + egui::vec2((c1 + 1) as f32 * cell, (r1 + 1) as f32 * cell),
                        ).shrink(1.5);
                        gp.rect_filled(pr, egui::Rounding::same(3.0), w.color().gamma_multiply(0.55));
                        gp.rect_stroke(pr, egui::Rounding::same(3.0), egui::Stroke::new(2.0, Color32::WHITE));
                    }
                }
                // Drag = fill rectangle; single click = 1 cell; right-click /
                // click-on-active = clear.
                if gresp.secondary_clicked() {
                    if let Some((r, c)) = inside { clear_cell = Some((r, c)); }
                } else if gresp.drag_started() {
                    if active.is_some() {
                        if let Some(cell_rc) = inside.or(clamped) { set_anchor = Some(Some(cell_rc)); }
                    }
                } else if gresp.drag_stopped() {
                    if active.is_some() {
                        if let Some(cur) = clamped {
                            fill_rect = Some((drag_anchor.unwrap_or(cur), cur));
                        }
                    }
                    set_anchor = Some(None);
                } else if gresp.clicked() {
                    if let Some((r, c)) = inside {
                        match active {
                            Some(a) if gsnap.cell(r, c) == Some(a) => clear_cell = Some((r, c)),
                            Some(_) => paint_cell = Some((r, c)),
                            None => { if let Some(w) = gsnap.cell(r, c) { select = Some(Some(w)); } }
                        }
                    }
                }

                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(RichText::new(rust_i18n::t!("srv_apply").to_string()).strong())
                        .fill(SNAP_ACCENT)).clicked() { do_apply = true; }
                    if ui.button(rust_i18n::t!("srv_empty").to_string()).clicked() { clear_all = true; }
                });
              }); // ScrollArea
            });

        self.show_layout_arranger = open_flag;
        if sel_mon != cur_mon && sel_mon < self.layout_grid_per_monitor.len() {
            self.layout_target_monitor = sel_mon;
        }
        if let Some(sel) = select { self.layout_active_item = sel; }
        let mon = cur_mon;
        if let Some((rr, cc)) = resize_to { self.layout_grid_per_monitor[mon].set_size(rr, cc); }
        if clear_all {
            for slot in self.layout_grid_per_monitor[mon].cells.iter_mut() { *slot = None; }
        }
        if let Some((r, c)) = clear_cell { self.layout_grid_per_monitor[mon].set(r, c, None); }
        if let Some((r, c)) = paint_cell {
            if let Some(w) = active {
                // Remove this window from ALL grids first (incl. the current
                // monitor) so old cells don't linger and bounds() doesn't
                // unintentionally stretch the window over a larger rectangle.
                for g in self.layout_grid_per_monitor.iter_mut() { g.remove(w); }
                self.layout_grid_per_monitor[mon].set(r, c, Some(w));
            }
        }
        if let Some(sa) = set_anchor { self.layout_drag_anchor = sa; }
        if let Some((a, b)) = fill_rect {
            if let Some(w) = active {
                // Remove this window from ALL grids first (incl. the current
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
