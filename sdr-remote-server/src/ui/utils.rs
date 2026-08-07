// SPDX-License-Identifier: GPL-2.0-or-later

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Global request flag for server auto-restart. UI buttons that
/// trigger a restart (tuner config changes, slot delete, etc.)
/// set this to `true` via `request_auto_restart()`. The event loop
/// in `ServerApp::update()` checks the flag every frame, performs
/// graceful cleanup there (`shutdown_tx.send(true)` + dropping all
/// hardware Arcs so Drop handlers run + ~600 ms sleep so cpal
/// audio devices are released) and only then spawns + `exit(0)`.
///
/// Previously UI buttons called `process::exit(0)` directly after spawn -
/// Drop handlers were skipped, audio cpal streams + TCI connect
/// stayed open until the OS cleaned up the process. The new child got
/// "device in use" and audio often no longer worked.
static AUTO_RESTART_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Request a server auto-restart. Non-blocking: only sets the
/// flag. The actual restart runs in `ServerApp::update()`
/// as soon as it detects the flag, so it can run Drop handlers
/// correctly before it calls `process::exit`.
pub(crate) fn request_auto_restart() {
    log::info!("Auto-restart requested - cleanup in event-loop");
    AUTO_RESTART_REQUESTED.store(true, Ordering::Relaxed);
}

/// True if an auto-restart is pending. The event loop does not reset
/// the flag - after cleanup `process::exit(0)` is called directly.
pub(crate) fn auto_restart_requested() -> bool {
    AUTO_RESTART_REQUESTED.load(Ordering::Relaxed)
}

/// Small "delete" button with a manually drawn cross (×).
/// Drawn geometrically via `Painter::line_segment` instead of
/// a Unicode glyph (`\u{2715}` / `\u{2716}` etc.) because egui's default
/// font does not render those characters (see memory `egui-font-tofu`).
///
/// Visually: a square at text-button height, two diagonal lines that
/// cross each other in the middle. On hover the color switches to
/// `visuals.widgets.hovered.fg_stroke.color` for visual feedback.
pub(crate) fn delete_button(ui: &mut egui::Ui) -> egui::Response {
    let size = ui.text_style_height(&egui::TextStyle::Button);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(size, size),
        egui::Sense::click(),
    );
    let color = if response.hovered() {
        ui.visuals().widgets.hovered.fg_stroke.color
    } else {
        ui.visuals().text_color()
    };
    let stroke = egui::Stroke::new(1.5, color);
    let pad = size * 0.22;
    let painter = ui.painter();
    painter.line_segment(
        [
            egui::pos2(rect.left() + pad, rect.top() + pad),
            egui::pos2(rect.right() - pad, rect.bottom() - pad),
        ],
        stroke,
    );
    painter.line_segment(
        [
            egui::pos2(rect.right() - pad, rect.top() + pad),
            egui::pos2(rect.left() + pad, rect.bottom() - pad),
        ],
        stroke,
    );
    response
}

/// Collapsible-section header with a manually drawn, fully
/// filled triangle chevron. Used in places where we cannot switch
/// to egui's native `CollapsingHeader` without layout shift - the
/// chevron is drawn geometrically via `Shape::convex_polygon`,
/// not as a font glyph, so we are not dependent on the
/// `\u{25BC}` / `\u{25B6}` glyphs that render as tofu squares
/// in egui's default font.
///
/// - `open == false`: right-pointing filled triangle (▶) - collapsed
/// - `open == true`:  down-pointing filled triangle (▼) - expanded
///
/// The label is always *to the right* of the triangle, regardless of
/// the parent layout direction (the helper computes the row rect itself
/// and paints manually, so a `right_to_left` parent changes nothing
/// here - only the cell position within that parent shifts).
///
/// Mouse-over on both chevron and label highlights both to the
/// `visuals.widgets.hovered.fg_stroke.color` of the active theme.
pub(crate) fn chevron_label(
    ui: &mut egui::Ui,
    open: bool,
    label: impl Into<egui::WidgetText>,
) -> egui::Response {
    let text: egui::WidgetText = label.into();
    let chevron_size = ui.text_style_height(&egui::TextStyle::Button);
    let spacing = ui.spacing().item_spacing.x;

    // Lay out the text galley to compute the row size
    let galley = text.into_galley(
        ui,
        Some(egui::TextWrapMode::Extend),
        f32::INFINITY,
        egui::TextStyle::Button,
    );

    let row_size = egui::vec2(
        chevron_size + spacing + galley.size().x,
        chevron_size.max(galley.size().y),
    );
    let (rect, response) = ui.allocate_exact_size(row_size, egui::Sense::click());

    // Hover state determines the color for both chevron and label
    let color = if response.hovered() {
        ui.visuals().widgets.hovered.fg_stroke.color
    } else {
        ui.visuals().text_color()
    };

    // Chevron on the left - manually placed so the parent layout
    // (left_to_right or right_to_left) cannot flip it.
    //
    // Shape matches egui's native CollapsingHeader chevron: an
    // isosceles triangle with one clearly shorter back and
    // two longer legs that run to a sharp point. On
    // hover the triangle grows 35% - a smaller base with more
    // grow gives clearer hover feedback.
    let chev_center = egui::pos2(rect.left() + chevron_size / 2.0, rect.center().y);
    let scale = if response.hovered() { 1.35 } else { 1.0 };
    let r = chevron_size * 0.28 * scale;
    let points = if open {
        // Down-pointing: short back on top (0.7r wide from center),
        // sharp point 1.0r downward. Legs ≈ 1.66r, back 1.4r.
        vec![
            egui::pos2(chev_center.x - r * 0.7, chev_center.y - r * 0.5),
            egui::pos2(chev_center.x + r * 0.7, chev_center.y - r * 0.5),
            egui::pos2(chev_center.x, chev_center.y + r * 1.0),
        ]
    } else {
        // Right-pointing: short back on the left (0.7r tall from center),
        // sharp point 1.0r to the right.
        vec![
            egui::pos2(chev_center.x - r * 0.5, chev_center.y - r * 0.7),
            egui::pos2(chev_center.x - r * 0.5, chev_center.y + r * 0.7),
            egui::pos2(chev_center.x + r * 1.0, chev_center.y),
        ]
    };
    ui.painter()
        .add(egui::Shape::convex_polygon(points, color, egui::Stroke::NONE));

    // Label to the right of the chevron, vertically centered
    let label_pos = egui::pos2(
        rect.left() + chevron_size + spacing,
        rect.center().y - galley.size().y / 2.0,
    );
    ui.painter().galley(label_pos, galley, color);

    response
}

/// Run a blocking init function with a timeout.
/// Returns Err if the function hangs longer than the timeout.
pub(crate) fn with_timeout<T: Send + 'static>(
    timeout: Duration,
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(timeout)
        .unwrap_or_else(|_| Err("Timeout: COM poort reageert niet".to_string()))
}
