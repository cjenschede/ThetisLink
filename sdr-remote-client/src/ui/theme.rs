// SPDX-License-Identifier: GPL-2.0-or-later

//! ThetisLink UI design-system: shared color, font and spacing constants
//! plus widget helpers so windows are identical BY CONSTRUCTION instead of
//! repainted by hand per window. One source of truth for all popout and
//! tab UIs (RX1/RX2/VRX/Yaesu/rotor).
//!
//! Values come from the UI-consistency audit (exact parity spec). Change here,
//! not per call-site.

use egui::Color32;

// ── Theme variants ────────────────────────────────────────────────

// The theme system itself lives in `sdr-remote-theme`, so the server can show
// exactly the same variants and visuals. Only re-exported here, so all
// existing `theme::` call-sites remain unchanged.
pub(crate) use sdr_remote_theme::{apply_visuals, Palette, ThemeVariant};

// ── Toggle / selection colors ──────────────────────────────────────────────

/// The only selected/toggled-ON fill. Blue is exclusively for toggled-ON state
/// (`feedback_ui_button_color_convention`). Momentary action buttons get NO
/// fill (default `egui::Button`).
pub(crate) const TL_SELECTED_FILL: Color32 = Color32::from_rgb(100, 160, 230);

/// Mode/status label in the frequency top-row (amber).
pub(crate) const TL_AMBER_TEXT: Color32 = Color32::from_rgb(255, 170, 40);

/// SWR too high. One color for the same condition: the SWR readout above 3.0
/// and the HIGH SWR alarm indicator of the Yaesu radios.
pub(crate) const TL_SWR_ALERT_TEXT: Color32 = Color32::from_rgb(255, 80, 80);

// ── Spacing / layout ───────────────────────────────────────────────────────

/// Vertical gap between spectrum and waterfall within one panel.
pub(crate) const TL_INNER_GAP_Y: f32 = 2.0;
/// Width of the spectrum-control sliders (Ref/Range/Zoom/Pan/WF).
pub(crate) const TL_SLIDER_WIDTH: f32 = 80.0;

// ── Font sizes ─────────────────────────────────────────────────────────────

pub(crate) const TL_FREQ_FONT: f32 = 18.0;
pub(crate) const TL_MODE_STATUS_FONT: f32 = 16.0;
pub(crate) const TL_BW_STATUS_FONT: f32 = 12.0;
pub(crate) const TL_SEGMENT_FONT: f32 = 11.0;
pub(crate) const TL_CHANNEL_HEADER_FONT: f32 = 13.0;

// ── Widget helpers ───────────────────────────────────────────────────────────

/// Shared toggle/selected button. Enforces the house rules:
/// - blue `TL_SELECTED_FILL` ONLY when `selected` (toggled-ON);
/// - OFF-state = default `egui::Button` (no custom fill / no "disabled" gray);
/// - hover text is MANDATORY (`feedback_ui_hover_always`).
///
/// Use this instead of inline `Button::new(...).fill(...)` so all windows get
/// the same toggle style and hover. Returns the `Response` so the
/// caller can check `.clicked()`.
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

/// Momentary action button (no toggle): default styling, NO fill, with
/// mandatory hover. For buttons like "Copy VFO", "Refresh", A<>B swap
/// (`feedback_ui_button_color_convention`: no blue for momentary actions).
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

/// Shared segmented selector: a row of toggle buttons from (value, label) pairs,
/// all with the same style and mandatory hover (via `tl_toggle_button`). The
/// button of the current `selected` value gets the blue ON-fill. Returns the
/// clicked value (or `None`); the caller handles the click, so the selector
/// stays free of state/dispatch. Deduplicates mode/BW choice-rows (and is
/// reusable for other windows).
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
