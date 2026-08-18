// SPDX-License-Identifier: GPL-2.0-or-later
//! Window arranger shared by the desktop client and the server GUI.
//!
//! Both had their own copy of the same thing - the same `LayoutGrid`, the same
//! size-picker, the same placement matrix - differing only in which windows they
//! offer. They drifted: the client went to an 18x18 grid, gained arrangement
//! memories and learned to keep geometry in one unit under a UI scale, while the
//! server stayed at 12x12 with none of it.
//!
//! So the parts that are the same live here and the parts that genuinely differ
//! stay with the application: which windows exist (`SnapTarget`), whether one is
//! available, how to open it and where to put it. A change to the arranger now
//! lands in both by construction rather than by being repainted twice.

use egui::{Color32, RichText};

/// Maximum grid size in the arranger (GRID_MAX x GRID_MAX).
///
/// 18 rather than 12: with a UI scale below 100% a lot more fits on screen, and
/// 12 columns then means cells wider than a window needs.
pub const GRID_MAX: usize = 18;

/// How many arrangements can be stored. Five fits under the grid without the
/// window needing a scroll of its own.
pub const LAYOUT_MEM_SLOTS: usize = 5;

/// A window an application offers to the arranger.
///
/// `key` must be stable ASCII: it goes into the config file and into egui Ids.
/// `label` is display text and may be translated, so it is never used as a key.
pub trait SnapTarget: Copy + PartialEq + Sized + 'static {
    fn all() -> &'static [Self];
    fn key(self) -> &'static str;
    fn label(self) -> String;
    fn color(self) -> Color32;

    fn from_key(k: &str) -> Option<Self> {
        Self::all().iter().copied().find(|w| w.key() == k)
    }
}

/// One placement matrix (per monitor): a grid of `rows` x `cols` cells, each
/// holding at most one window. A window spanning several adjacent cells is
/// stretched over its enclosing rectangle when the layout is applied.
#[derive(Clone)]
pub struct LayoutGrid<W: SnapTarget> {
    rows: u8,
    cols: u8,
    cells: Vec<Option<W>>, // rows*cols, row by row
}

impl<W: SnapTarget> LayoutGrid<W> {
    pub fn new() -> Self {
        Self { rows: 2, cols: 2, cells: vec![None; 4] }
    }

    pub fn rows(&self) -> u8 { self.rows }
    pub fn cols(&self) -> u8 { self.cols }

    /// Reformat to rows x cols, keeping assignments that still fall inside.
    pub fn set_size(&mut self, rows: u8, cols: u8) {
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

    pub fn cell(&self, r: usize, c: usize) -> Option<W> {
        self.cells.get(r * self.cols as usize + c).copied().flatten()
    }

    pub fn set(&mut self, r: usize, c: usize, w: Option<W>) {
        if let Some(slot) = self.cells.get_mut(r * self.cols as usize + c) {
            *slot = w;
        }
    }

    pub fn clear_all(&mut self) {
        for slot in self.cells.iter_mut() { *slot = None; }
    }

    /// Remove a window from every cell of this grid.
    pub fn remove(&mut self, w: W) {
        for slot in self.cells.iter_mut() {
            if *slot == Some(w) { *slot = None; }
        }
    }

    /// Drop windows the application no longer offers (e.g. a radio unplugged).
    pub fn retain_available(&mut self, available: &[W]) {
        for slot in self.cells.iter_mut() {
            if let Some(w) = *slot {
                if !available.contains(&w) { *slot = None; }
            }
        }
    }

    /// Enclosing rectangle (min_r, min_c, max_r, max_c) of a window, if placed.
    pub fn bounds(&self, w: W) -> Option<(usize, usize, usize, usize)> {
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

    /// All distinct windows in this grid, in `SnapTarget::all()` order.
    pub fn placed(&self) -> Vec<W> {
        W::all().iter().copied().filter(|w| self.cells.contains(&Some(*w))).collect()
    }

    /// `rows x cols` followed by one key per cell, `-` for empty:
    /// `2x3:main,-,rx1,-,-,yaesu1`. One line, so it fits the flat config format.
    pub fn to_config_string(&self) -> String {
        let cells: Vec<&str> = self.cells.iter()
            .map(|c| c.map(|w| w.key()).unwrap_or("-"))
            .collect();
        format!("{}x{}:{}", self.rows, self.cols, cells.join(","))
    }

    pub fn from_config_string(s: &str) -> Option<Self> {
        let (dims, cells) = s.split_once(':')?;
        let (r, c) = dims.split_once('x')?;
        let rows: u8 = r.trim().parse().ok()?;
        let cols: u8 = c.trim().parse().ok()?;
        if rows == 0 || cols == 0 || rows as usize > GRID_MAX || cols as usize > GRID_MAX {
            return None;
        }
        let mut grid = Self { rows, cols, cells: vec![None; rows as usize * cols as usize] };
        for (i, key) in cells.split(',').enumerate() {
            if i >= grid.cells.len() { break; }
            grid.cells[i] = W::from_key(key.trim());
        }
        Some(grid)
    }
}

impl<W: SnapTarget> Default for LayoutGrid<W> {
    fn default() -> Self { Self::new() }
}

/// Serialise/parse the per-monitor grids as one config value (`;` between
/// monitors), so a painted matrix survives a restart.
pub fn layout_grids_to_config<W: SnapTarget>(grids: &[LayoutGrid<W>]) -> String {
    grids.iter().map(|g| g.to_config_string()).collect::<Vec<_>>().join(";")
}

pub fn layout_grids_from_config<W: SnapTarget>(s: &str) -> Vec<LayoutGrid<W>> {
    s.split(';').filter(|p| !p.trim().is_empty())
        .filter_map(LayoutGrid::from_config_string)
        .collect()
}

/// One stored arrangement: where each window stood, in SYSTEM points.
///
/// Positions, not a grid. The grid is how an arrangement is BUILT; this is what
/// comes BACK, so a layout fine-tuned by hand afterwards returns exactly as it
/// was left, and a recall does not depend on the grid still having the same
/// rows and columns.
#[derive(Clone)]
pub struct LayoutMemory<W: SnapTarget> {
    pub name: String,
    pub windows: Vec<(W, egui::Pos2, egui::Vec2)>,
}

impl<W: SnapTarget> Default for LayoutMemory<W> {
    fn default() -> Self { Self { name: String::new(), windows: Vec::new() } }
}

impl<W: SnapTarget> LayoutMemory<W> {
    pub fn is_empty(&self) -> bool { self.windows.is_empty() }

    /// `name|key:x,y,w,h|...` on one line. `|` is stripped from the name because
    /// it is the field separator.
    pub fn to_config_string(&self) -> String {
        let mut s: String = self.name.chars()
            .filter(|c| *c != '|' && *c != '\n' && *c != '\r')
            .collect();
        for (w, p, sz) in &self.windows {
            s.push_str(&format!("|{}:{:.0},{:.0},{:.0},{:.0}", w.key(), p.x, p.y, sz.x, sz.y));
        }
        s
    }

    pub fn from_config_string(s: &str) -> Self {
        let mut parts = s.split('|');
        let name = parts.next().unwrap_or("").trim().to_string();
        let mut windows = Vec::new();
        for part in parts {
            let Some((key, rest)) = part.split_once(':') else { continue };
            let Some(w) = W::from_key(key.trim()) else { continue };
            let n: Vec<f32> = rest.split(',').filter_map(|v| v.trim().parse().ok()).collect();
            if n.len() == 4 && n[2] > 0.0 && n[3] > 0.0 {
                windows.push((w, egui::pos2(n[0], n[1]), egui::vec2(n[2], n[3])));
            }
        }
        Self { name, windows }
    }
}

/// What the placement matrix wants done, collected during drawing so the caller
/// can apply it afterwards - the widget never borrows application state.
pub struct PlacementIntents<W: SnapTarget> {
    pub paint_cell: Option<(usize, usize)>,
    pub clear_cell: Option<(usize, usize)>,
    pub select: Option<Option<W>>,
    pub set_anchor: Option<Option<(usize, usize)>>,
    pub fill_rect: Option<((usize, usize), (usize, usize))>,
}

impl<W: SnapTarget> Default for PlacementIntents<W> {
    fn default() -> Self {
        Self { paint_cell: None, clear_cell: None, select: None, set_anchor: None, fill_rect: None }
    }
}

/// Grid-size picker: hover to preview a size, click to choose it. Returns the
/// chosen (rows, cols).
///
/// The cells stay at 15 points rather than shrinking to hold the larger maximum:
/// this is operated on a touch screen, where a smaller target costs more than the
/// extra space does.
pub fn grid_size_picker(
    ui: &mut egui::Ui,
    label: &str,
    cur_rows: u8,
    cur_cols: u8,
    highlight: Color32,
) -> Option<(u8, u8)> {
    const PCELL: f32 = 15.0;
    const PGAP: f32 = 2.0;
    let dim = PCELL * GRID_MAX as f32 + PGAP * (GRID_MAX as f32 - 1.0);
    let mut chosen = None;
    ui.horizontal(|ui| {
        ui.label(label);
        let (prect, presp) = ui.allocate_exact_size(egui::vec2(dim, dim), egui::Sense::click());
        let hover = presp.hover_pos().map(|p| {
            let c = (((p.x - prect.min.x) / (PCELL + PGAP)) as i32).clamp(0, GRID_MAX as i32 - 1) as usize;
            let r = (((p.y - prect.min.y) / (PCELL + PGAP)) as i32).clamp(0, GRID_MAX as i32 - 1) as usize;
            (r, c)
        });
        let pnt = ui.painter_at(prect);
        for r in 0..GRID_MAX {
            for c in 0..GRID_MAX {
                let cr = egui::Rect::from_min_size(
                    prect.min + egui::vec2(c as f32 * (PCELL + PGAP), r as f32 * (PCELL + PGAP)),
                    egui::vec2(PCELL, PCELL),
                );
                let preview = hover.map_or(false, |(hr, hc)| r <= hr && c <= hc);
                let cur = r < cur_rows as usize && c < cur_cols as usize;
                let fill = if preview {
                    highlight
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
            if presp.clicked() { chosen = Some((hr as u8 + 1, hc as u8 + 1)); }
        }
        let (lr, lc) = hover
            .map(|(r, c)| (r + 1, c + 1))
            .unwrap_or((cur_rows as usize, cur_cols as usize));
        ui.label(RichText::new(format!("{} x {}", lr, lc)).strong());
    });
    chosen
}

/// Palette of windows to paint with. Returns a selection change, if any.
pub fn window_palette<W: SnapTarget>(
    ui: &mut egui::Ui,
    available: &[W],
    active: Option<W>,
) -> Option<Option<W>> {
    let mut select = None;
    ui.horizontal_wrapped(|ui| {
        for w in available {
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
    select
}

/// The placement matrix: click a cell, or drag to fill a rectangle. Right-click
/// or clicking an already-placed window clears. Drawing and interaction only -
/// everything it wants done comes back in `PlacementIntents`.
pub fn placement_grid<W: SnapTarget>(
    ui: &mut egui::Ui,
    grid: &LayoutGrid<W>,
    active: Option<W>,
    drag_anchor: Option<(usize, usize)>,
) -> PlacementIntents<W> {
    let mut out = PlacementIntents::default();
    let cols = grid.cols() as usize;
    let rows = grid.rows() as usize;
    let avail = ui.available_width().clamp(120.0, 520.0);
    let cell = (avail / cols as f32).clamp(22.0, 88.0);
    let (grect, gresp) = ui.allocate_exact_size(
        egui::vec2(cell * cols as f32, cell * rows as f32),
        egui::Sense::click_and_drag(),
    );
    let gp = ui.painter_at(grect);
    for r in 0..rows {
        for c in 0..cols {
            let cr = egui::Rect::from_min_size(
                grect.min + egui::vec2(c as f32 * cell, r as f32 * cell),
                egui::vec2(cell, cell),
            ).shrink(1.5);
            let w = grid.cell(r, c);
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
    // `inside` = strictly within the grid (for click/right-click); `clamped` =
    // clamped to the edge, so dragging past the edge still hits the edge cell.
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
    if gresp.secondary_clicked() {
        if let Some(rc) = inside { out.clear_cell = Some(rc); }
    } else if gresp.drag_started() {
        if active.is_some() {
            if let Some(rc) = inside.or(clamped) { out.set_anchor = Some(Some(rc)); }
        }
    } else if gresp.drag_stopped() {
        if active.is_some() {
            if let Some(cur) = clamped {
                out.fill_rect = Some((drag_anchor.unwrap_or(cur), cur));
            }
        }
        out.set_anchor = Some(None);
    } else if gresp.clicked() {
        if let Some((r, c)) = inside {
            match active {
                Some(a) if grid.cell(r, c) == Some(a) => out.clear_cell = Some((r, c)),
                Some(_) => out.paint_cell = Some((r, c)),
                None => { if let Some(w) = grid.cell(r, c) { out.select = Some(Some(w)); } }
            }
        }
    }
    out
}

/// UNITS. Geometry is stored in SYSTEM points so a saved layout survives a change
/// of UI scale: the windows stay where they are and only the content inside them
/// scales. Everything egui-facing is in EGUI points - egui-winit converts both a
/// ViewportBuilder and a ViewportCommand with `zoom_factor * native_pixels_per_point`.
/// So divide by the zoom on the way in and multiply on the way out. At zoom 1.0 the
/// two are identical, which is why the distinction stays invisible until a
/// high-DPI screen makes a smaller UI scale worth having.
pub fn system_to_egui(v: f32, zoom: f32) -> f32 { v / zoom.max(0.01) }

/// Inverse of [`system_to_egui`].
pub fn egui_to_system(v: f32, zoom: f32) -> f32 { v * zoom.max(0.01) }

/// The UI-scale steps offered in both applications, with their labels. One list,
/// so the client and the server cannot end up offering different scales.
pub const UI_ZOOM_STEPS: [(f32, &str); 10] = [
    (0.50, "50%"), (0.55, "55%"), (0.60, "60%"), (0.70, "70%"), (0.75, "75%"),
    (0.85, "85%"), (1.00, "100%"), (1.15, "115%"), (1.30, "130%"), (1.50, "150%"),
];

/// The scale picker, identical in both applications. Returns a newly chosen scale.
pub fn ui_scale_picker(ui: &mut egui::Ui, id: &str, current: f32) -> Option<f32> {
    let cur = UI_ZOOM_STEPS.iter()
        .min_by(|a, b| (a.0 - current).abs().total_cmp(&(b.0 - current).abs()))
        .map(|(_, l)| *l)
        .unwrap_or("100%");
    let mut picked = None;
    egui::ComboBox::from_id_salt(id)
        .selected_text(cur)
        .width(80.0)
        .show_ui(ui, |ui| {
            for (v, label) in UI_ZOOM_STEPS {
                if ui.selectable_label((current - v).abs() < 0.001, label).clicked() {
                    picked = Some(v);
                }
            }
        });
    picked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum W { Main, Second, Third }
    impl SnapTarget for W {
        fn all() -> &'static [Self] { &[W::Main, W::Second, W::Third] }
        fn key(self) -> &'static str {
            match self { W::Main => "main", W::Second => "second", W::Third => "third" }
        }
        fn label(self) -> String { self.key().to_string() }
        fn color(self) -> Color32 { Color32::WHITE }
    }

    #[test]
    fn an_arrangement_survives_the_config_round_trip() {
        let mem = LayoutMemory::<W> {
            name: "Contest".to_string(),
            windows: vec![
                (W::Main, egui::pos2(0.0, 0.0), egui::vec2(1200.0, 800.0)),
                (W::Third, egui::pos2(1200.0, 40.0), egui::vec2(460.0, 760.0)),
            ],
        };
        let back = LayoutMemory::<W>::from_config_string(&mem.to_config_string());
        assert_eq!(back.name, "Contest");
        assert_eq!(back.windows.len(), 2);
        assert_eq!(back.windows[1].0, W::Third);
        assert_eq!(back.windows[1].1, egui::pos2(1200.0, 40.0));
        assert_eq!(back.windows[1].2, egui::vec2(460.0, 760.0));
    }

    #[test]
    fn a_pipe_in_the_name_cannot_forge_an_entry() {
        let mem = LayoutMemory::<W> {
            name: "a|main:9,9,9,9".to_string(),
            windows: vec![(W::Second, egui::pos2(1.0, 2.0), egui::vec2(3.0, 4.0))],
        };
        let back = LayoutMemory::<W>::from_config_string(&mem.to_config_string());
        assert_eq!(back.windows.len(), 1);
        assert_eq!(back.windows[0].0, W::Second);
    }

    #[test]
    fn an_empty_slot_reads_back_empty() {
        assert!(LayoutMemory::<W>::from_config_string("").is_empty());
    }

    #[test]
    fn the_placement_matrix_survives_a_restart() {
        let mut grid = LayoutGrid::<W>::new();
        grid.set_size(2, 3);
        grid.set(0, 0, Some(W::Main));
        grid.set(1, 2, Some(W::Second));
        let grids = layout_grids_from_config::<W>(&layout_grids_to_config(&[grid]));
        assert_eq!(grids.len(), 1);
        assert_eq!((grids[0].rows(), grids[0].cols()), (2, 3));
        assert_eq!(grids[0].cell(0, 0), Some(W::Main));
        assert_eq!(grids[0].cell(1, 2), Some(W::Second));
        assert_eq!(grids[0].cell(0, 1), None);
    }

    #[test]
    fn a_nonsense_grid_line_is_ignored() {
        assert!(LayoutGrid::<W>::from_config_string("99x99:main").is_none());
        assert!(LayoutGrid::<W>::from_config_string("garbage").is_none());
        assert!(layout_grids_from_config::<W>("").is_empty());
    }

    /// Resizing keeps what still fits and drops what falls outside.
    #[test]
    fn resizing_keeps_the_cells_that_still_fit() {
        let mut grid = LayoutGrid::<W>::new();
        grid.set_size(3, 3);
        grid.set(0, 0, Some(W::Main));
        grid.set(2, 2, Some(W::Third));
        grid.set_size(2, 2);
        assert_eq!(grid.cell(0, 0), Some(W::Main));
        assert_eq!(grid.placed(), vec![W::Main]);
    }

    /// A window the application stops offering must leave the grid.
    #[test]
    fn an_unavailable_window_leaves_the_grid() {
        let mut grid = LayoutGrid::<W>::new();
        grid.set(0, 0, Some(W::Main));
        grid.set(0, 1, Some(W::Third));
        grid.retain_available(&[W::Main]);
        assert_eq!(grid.cell(0, 0), Some(W::Main));
        assert_eq!(grid.cell(0, 1), None);
    }

    /// System points and egui points are the same thing at 100%, and each other's
    /// inverse everywhere else - the property the pop-out geometry depends on.
    #[test]
    fn the_two_point_units_are_inverses() {
        for zoom in [0.5, 0.75, 1.0, 1.5] {
            let v = 1234.0_f32;
            assert!((egui_to_system(system_to_egui(v, zoom), zoom) - v).abs() < 0.01);
        }
        assert_eq!(system_to_egui(800.0, 1.0), 800.0);
    }
}
