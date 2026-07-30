// SPDX-License-Identifier: GPL-2.0-or-later

//! ThetisLink UI design-system: gedeelde kleur-, font- en spacing-constanten
//! plus widget-helpers zodat windows identiek zijn DOOR CONSTRUCTIE i.p.v. per
//! window handmatig nageschilderd. Eén bron van waarheid voor alle popout-
//! en tab-UI's (RX1/RX2/VRX/Yaesu/rotor).
//!
//! Waarden komen uit de UI-consistency audit (exacte parity-spec). Verander hier,
//! niet per call-site.

use egui::Color32;

// ── Thema-varianten ────────────────────────────────────────────────

// Het thema-systeem zelf woont in `sdr-remote-theme`, zodat de server exact
// dezelfde varianten en visuals kan tonen. Hier alleen doorgegeven, zodat alle
// bestaande `theme::`-call-sites ongewijzigd blijven.
pub(crate) use sdr_remote_theme::{apply_visuals, Palette, ThemeVariant};

// ── Toggle / selectie kleuren ──────────────────────────────────────────────

/// Enige selected/toggled-ON fill. Blauw is uitsluitend voor toggled-ON state
/// (`feedback_ui_button_color_convention`). Momentane actie-knoppen krijgen GEEN
/// fill (default `egui::Button`).
pub(crate) const TL_SELECTED_FILL: Color32 = Color32::from_rgb(100, 160, 230);

/// Mode-/status-label in de frequency top-row (amber).
pub(crate) const TL_AMBER_TEXT: Color32 = Color32::from_rgb(255, 170, 40);

/// SWR te hoog. Eén kleur voor dezelfde conditie: de SWR-uitlezing boven 3.0
/// én de HIGH SWR-alarmindicator van de Yaesu-radio's.
pub(crate) const TL_SWR_ALERT_TEXT: Color32 = Color32::from_rgb(255, 80, 80);

// ── Spacing / layout ───────────────────────────────────────────────────────

/// Verticale gap tussen spectrum en waterval binnen één paneel.
pub(crate) const TL_INNER_GAP_Y: f32 = 2.0;
/// Breedte van de spectrum-control sliders (Ref/Range/Zoom/Pan/WF).
pub(crate) const TL_SLIDER_WIDTH: f32 = 80.0;

// ── Font-maten ─────────────────────────────────────────────────────────────

pub(crate) const TL_FREQ_FONT: f32 = 18.0;
pub(crate) const TL_MODE_STATUS_FONT: f32 = 16.0;
pub(crate) const TL_BW_STATUS_FONT: f32 = 12.0;
pub(crate) const TL_SEGMENT_FONT: f32 = 11.0;
pub(crate) const TL_CHANNEL_HEADER_FONT: f32 = 13.0;

// ── Widget-helpers ───────────────────────────────────────────────────────────

/// Gedeelde toggle/selected-button. Dwingt de huisregels af:
/// - blauwe `TL_SELECTED_FILL` ALLEEN wanneer `selected` (toggled-ON);
/// - OFF-state = default `egui::Button` (geen custom fill / geen "disabled"-grijs);
/// - hover-tekst is VERPLICHT (`feedback_ui_hover_always`).
///
/// Gebruik dit i.p.v. inline `Button::new(...).fill(...)` zodat alle windows
/// dezelfde toggle-stijl én hover krijgen. Retourneert de `Response` zodat de
/// caller `.clicked()` kan checken.
pub(crate) fn tl_toggle_button(
    ui: &mut egui::Ui,
    label: &str,
    selected: bool,
    enabled: bool,
    size: f32,
    hover: &str,
) -> egui::Response {
    let text = if selected {
        egui::RichText::new(label).size(size).strong()
    } else {
        egui::RichText::new(label).size(size)
    };
    let mut btn = egui::Button::new(text);
    if selected {
        btn = btn.fill(TL_SELECTED_FILL);
    }
    ui.add_enabled(enabled, btn).on_hover_text(hover)
}

/// Momentane actie-knop (geen toggle): default styling, GEEN fill, met
/// verplichte hover. Voor knoppen als "Copy VFO", "Refresh", A<>B swap
/// (`feedback_ui_button_color_convention`: geen blauw voor momentane acties).
pub(crate) fn tl_action_button(
    ui: &mut egui::Ui,
    label: &str,
    enabled: bool,
    size: f32,
    hover: &str,
) -> egui::Response {
    let btn = egui::Button::new(egui::RichText::new(label).size(size));
    ui.add_enabled(enabled, btn).on_hover_text(hover)
}

/// Gedeelde segmented-selector: een rij toggle-knoppen uit (waarde, label)-paren,
/// allemaal met dezelfde stijl en verplichte hover (via `tl_toggle_button`). De
/// knop van de huidige `selected`-waarde krijgt de blauwe ON-fill. Retourneert de
/// aangeklikte waarde (of `None`); de caller handelt de klik af, zodat de selector
/// vrij blijft van state/dispatch. Dedupliceert mode-/BW-keuzerijen (en is
/// herbruikbaar voor andere windows).
pub(crate) fn tl_segmented_selector<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    items: impl IntoIterator<Item = (T, String)>,
    selected: T,
    enabled: bool,
    size: f32,
    hover: &str,
) -> Option<T> {
    let mut clicked: Option<T> = None;
    for (val, label) in items {
        if tl_toggle_button(ui, &label, val == selected, enabled, size, hover).clicked() {
            clicked = Some(val);
        }
    }
    clicked
}
