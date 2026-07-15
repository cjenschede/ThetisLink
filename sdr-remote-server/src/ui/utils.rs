// SPDX-License-Identifier: GPL-2.0-or-later

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Globale request-flag voor server auto-restart. UI-knoppen die
/// een herstart triggeren (tuner config wijzigingen, slot delete, etc.)
/// zetten deze op `true` via `request_auto_restart()`. De event-loop
/// in `ServerApp::update()` checkt de flag elke frame, voert daar
/// gracieuze cleanup uit (`shutdown_tx.send(true)` + alle hardware-
/// Arcs droppen zodat Drop-handlers runnen + ~600 ms sleep zodat cpal
/// audio devices vrijgegeven worden) en pas dan spawn + `exit(0)`.
///
/// Voorheen riepen UI-knoppen direct `process::exit(0)` aan na spawn -
/// Drop-handlers werden overgeslagen, audio cpal-streams + TCI-connect
/// bleven open tot het OS de proces opruimde. De nieuwe child kreeg
/// "device in use" en audio werkte vaak niet meer.
static AUTO_RESTART_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Vraag een server auto-restart aan. Niet-blokkerend: zet alleen de
/// flag. De daadwerkelijke restart loopt in `ServerApp::update()`
/// zodra die de flag detecteert, zodat hij Drop-handlers correct kan
/// runnen voor hij `process::exit` doet.
pub(crate) fn request_auto_restart() {
    log::info!("Auto-restart requested - cleanup in event-loop");
    AUTO_RESTART_REQUESTED.store(true, Ordering::Relaxed);
}

/// True als er een auto-restart pending is. Event-loop reset de flag
/// niet - er wordt na cleanup direct `process::exit(0)` gedaan.
pub(crate) fn auto_restart_requested() -> bool {
    AUTO_RESTART_REQUESTED.load(Ordering::Relaxed)
}

/// Kleine "verwijder"-knop met een handmatig getekend kruis (×).
/// Wordt geometrisch via `Painter::line_segment` getekend in plaats van
/// een Unicode-glyph (`\u{2715}` / `\u{2716}` etc.) omdat egui's default
/// font die karakters niet rendert (zie memory `egui-font-tofu`).
///
/// Visueel: vierkant op text-button-hoogte, twee diagonale lijnen die
/// elkaar in het midden kruisen. Op hover wisselt de kleur naar
/// `visuals.widgets.hovered.fg_stroke.color` voor visuele feedback.
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

/// Collapsible-section header met een handmatig getekende, volledig
/// gevulde driehoek-chevron. Gebruikt op plekken waar we niet zonder
/// layout-shift naar egui's native `CollapsingHeader` kunnen - de
/// chevron wordt geometrisch via `Shape::convex_polygon` getekend,
/// niet als font-glyph, zodat we niet afhankelijk zijn van de
/// `\u{25BC}` / `\u{25B6}` glyphs die in egui's default font tofu-
/// vierkanten opleveren.
///
/// - `open == false`: rechts-wijzende gevulde driehoek (▶) - collapsed
/// - `open == true`:  omlaag-wijzende gevulde driehoek (▼) - expanded
///
/// Het label staat altijd *rechts* van het driehoekje, ongeacht de
/// parent-layout-richting (helper rekent zelf het row-rect uit en
/// schildert manueel, dus een `right_to_left` parent verandert hier
/// niets aan - alleen de cell-positie binnen die parent verschuift).
///
/// Mouse-over op chevron én label highlight't beide naar de
/// `visuals.widgets.hovered.fg_stroke.color` van de actieve theme.
pub(crate) fn chevron_label(
    ui: &mut egui::Ui,
    open: bool,
    label: impl Into<egui::WidgetText>,
) -> egui::Response {
    let text: egui::WidgetText = label.into();
    let chevron_size = ui.text_style_height(&egui::TextStyle::Button);
    let spacing = ui.spacing().item_spacing.x;

    // Layout het tekst-galley om de row-size te berekenen
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

    // Hover-state bepaalt de kleur voor zowel chevron als label
    let color = if response.hovered() {
        ui.visuals().widgets.hovered.fg_stroke.color
    } else {
        ui.visuals().text_color()
    };

    // Chevron links - handmatig geplaatst zodat parent-layout
    // (left_to_right of right_to_left) het niet kan omdraaien.
    //
    // Vorm matcht egui's native CollapsingHeader chevron: een
    // isoceles driehoek met één duidelijk kortere achterkant en
    // twee langere benen die naar een scherpe punt lopen. Op
    // hover groeit het driehoek 35% - kleinere base met meer
    // grow geeft een duidelijker hover-feedback.
    let chev_center = egui::pos2(rect.left() + chevron_size / 2.0, rect.center().y);
    let scale = if response.hovered() { 1.35 } else { 1.0 };
    let r = chevron_size * 0.28 * scale;
    let points = if open {
        // Down-pointing: korte achterkant boven (0.7r breed van center),
        // scherpe punt 1.0r naar onder. Benen ≈ 1.66r, achterkant 1.4r.
        vec![
            egui::pos2(chev_center.x - r * 0.7, chev_center.y - r * 0.5),
            egui::pos2(chev_center.x + r * 0.7, chev_center.y - r * 0.5),
            egui::pos2(chev_center.x, chev_center.y + r * 1.0),
        ]
    } else {
        // Right-pointing: korte achterkant links (0.7r hoog van center),
        // scherpe punt 1.0r naar rechts.
        vec![
            egui::pos2(chev_center.x - r * 0.5, chev_center.y - r * 0.7),
            egui::pos2(chev_center.x - r * 0.5, chev_center.y + r * 0.7),
            egui::pos2(chev_center.x + r * 1.0, chev_center.y),
        ]
    };
    ui.painter()
        .add(egui::Shape::convex_polygon(points, color, egui::Stroke::NONE));

    // Label rechts van chevron, vertically centered
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
