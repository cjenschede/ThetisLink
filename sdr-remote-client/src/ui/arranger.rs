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
impl SnapWindow {
    const ALL: [SnapWindow; 7] = [
        SnapWindow::Main, SnapWindow::Rx1, SnapWindow::Rx2, SnapWindow::Vrx1,
        SnapWindow::Vrx2, SnapWindow::Yaesu1, SnapWindow::Yaesu2,
    ];
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

/// Maximum grid size in the "Arrange windows" matrix (GRID_MAX x GRID_MAX).
const GRID_MAX: usize = 12;

/// One placement matrix (per monitor): grid of `rows` x `cols` cells; each
/// cell holds at most one window. A window that spans multiple adjacent cells
/// is stretched over its enclosing rectangle on "Apply".
#[derive(Clone)]
pub(crate) struct LayoutGrid {
    rows: u8, // 1..=GRID_MAX
    cols: u8, // 1..=GRID_MAX
    cells: Vec<Option<SnapWindow>>, // rows*cols, row by row
}
impl LayoutGrid {
    fn new() -> Self {
        Self { rows: 2, cols: 2, cells: vec![None; 4] }
    }
    /// Reformat to rows x cols; keep assignments that still fall within the
    /// new grid.
    fn set_size(&mut self, rows: u8, cols: u8) {
        let rows = rows.clamp(1, GRID_MAX as u8);
        let cols = cols.clamp(1, GRID_MAX as u8);
        if rows == self.rows && cols == self.cols { return; }
        let mut cells = vec![None; rows as usize * cols as usize];
        for r in 0..self.rows.min(rows) as usize {
            for c in 0..self.cols.min(cols) as usize {
                cells[r * cols as usize + c] = self.cells[r * self.cols as usize + c];
            }
        }
        self.rows = rows;
        self.cols = cols;
        self.cells = cells;
    }
    fn cell(&self, r: usize, c: usize) -> Option<SnapWindow> {
        self.cells.get(r * self.cols as usize + c).copied().flatten()
    }
    fn set(&mut self, r: usize, c: usize, w: Option<SnapWindow>) {
        if let Some(slot) = self.cells.get_mut(r * self.cols as usize + c) {
            *slot = w;
        }
    }
    /// Remove a window from all cells of this grid.
    fn remove(&mut self, w: SnapWindow) {
        for slot in self.cells.iter_mut() {
            if *slot == Some(w) { *slot = None; }
        }
    }
    /// Enclosing rectangle (min_r, min_c, max_r, max_c) of a window, or None.
    fn bounds(&self, w: SnapWindow) -> Option<(usize, usize, usize, usize)> {
        let (mut minr, mut minc, mut maxr, mut maxc) = (usize::MAX, usize::MAX, 0, 0);
        let mut found = false;
        for r in 0..self.rows as usize {
            for c in 0..self.cols as usize {
                if self.cell(r, c) == Some(w) {
                    found = true;
                    minr = minr.min(r); minc = minc.min(c);
                    maxr = maxr.max(r); maxc = maxc.max(c);
                }
            }
        }
        if found { Some((minr, minc, maxr, maxc)) } else { None }
    }
    /// All distinct windows in this grid (in ALL order).
    fn placed(&self) -> Vec<SnapWindow> {
        SnapWindow::ALL.iter().copied()
            .filter(|w| self.cells.contains(&Some(*w)))
            .collect()
    }
}

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
            let col_w = aw / grid.cols as f32;
            let row_h = ah / grid.rows as f32;
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
                    ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(x, y)));
                    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(iw, ih)));
                } else {
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

        egui::Window::new(rust_i18n::t!("main_arrange_windows_title").to_string())
            .open(&mut open_flag)
            .resizable(true)
            .default_size([360.0, 560.0])
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

                // --- Grid-size picker (up to GRID_MAX x GRID_MAX; hover/click) ---
                const MAXG: usize = GRID_MAX;
                const PCELL: f32 = 15.0;
                const PGAP: f32 = 2.0;
                let dim = PCELL * MAXG as f32 + PGAP * (MAXG as f32 - 1.0);
                ui.horizontal(|ui| {
                    ui.label(rust_i18n::t!("main_grid_size").to_string());
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
                                theme::TL_SELECTED_FILL
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

                // --- Palette: open windows as colored buttons (click = select) ---
                ui.label(rust_i18n::t!("main_pick_window").to_string());
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

                // --- Placement grid: click/drag to paint `active` ---
                ui.label(rust_i18n::t!("main_place_hint").to_string());
                let cols = gsnap.cols as usize;
                let rows = gsnap.rows as usize;
                let avail = ui.available_width().clamp(120.0, 380.0);
                let cell = (avail / cols as f32).clamp(22.0, 88.0);
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
                // (for click/right-click); `clamped` = clamped to the edge (for
                // dragging, so past the edge you still hit the edge cell).
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
                // Drag preview: highlight rectangle anchor..current (translucent + border).
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
                // Interaction. Dragging = fill rectangle (anchor -> release); single
                // click = 1 cell; right-click / click-on-active = clear.
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
                            // click on already-active window = clear that cell (toggle)
                            Some(a) if gsnap.cell(r, c) == Some(a) => clear_cell = Some((r, c)),
                            Some(_) => paint_cell = Some((r, c)),
                            // nothing selected: clicking a filled cell selects that window
                            None => { if let Some(w) = gsnap.cell(r, c) { select = Some(Some(w)); } }
                        }
                    }
                }

                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(RichText::new(rust_i18n::t!("main_apply").to_string()).strong())
                        .fill(theme::TL_SELECTED_FILL)).clicked() { do_apply = true; }
                    if ui.button(rust_i18n::t!("main_empty").to_string()).clicked() { clear_all = true; }
                });
              }); // ScrollArea
            });

        // --- Apply intents to the state (after the closure) ---
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
