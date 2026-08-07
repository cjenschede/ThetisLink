// SPDX-License-Identifier: GPL-2.0-or-later

//! Mode-selector render-helper (sub-step 2b).
//!
//! Replaces three render paths with a single helper:
//! - `render_rx1_controls_inner` (RX1 popouts, ~mod.rs:2275) - 8 modes, Extended
//! - `render_rx2_controls_inner` (RX2 popouts, ~mod.rs:2636) - 8 modes, Extended
//! - Tab::Radio main window (~mod.rs:3594) - 4 modes, Basic - was **unguarded**
//!   (connected-guard was missing).
//!
//! `UiDensity` determines which mode-set is visible: Basic shows only the
//! most-used voice-modes, Extended also shows CW + digital-modes.

use egui::{Color32, RichText};

use super::coverage;
use super::{ControlContext, UiDensity, UiEvent};

/// Full mode-set (popouts). (mode_val, label) - mode_val comes from the
/// TCI-protocol: 0=LSB, 1=USB, 3=CW-L, 4=CW-U, 5=FM, 6=AM, 7=DIGU, 9=DIGL,
/// 10=SAM (synchronous AM, AM-variant).
pub(crate) const MODES_EXTENDED: &[(u8, &str)] = &[
    (0, "LSB"), (1, "USB"), (3, "CW-L"), (4, "CW-U"),
    (6, "AM"), (10, "SAM"), (5, "FM"), (7, "DIGU"), (9, "DIGL"),
];

/// Basic-screen mode-set (Tab::Radio): only the most-used voice-modes.
pub(crate) const MODES_BASIC: &[(u8, &str)] = &[
    (0, "LSB"), (1, "USB"), (6, "AM"), (10, "SAM"), (5, "FM"),
];

pub(crate) struct ModeClick {
    pub(crate) mode: u8,
}

/// Renders the mode-selector row. Guards internally on `ctx.connected` via
/// `add_enabled`, emits `ClickReceived` on click, registers coverage with
/// `guarded=true`.
///
/// Reads the current mode from `ctx.rx_state.mode`; selects the mode-set based
/// on `ctx.density`. Returns `Some(ModeClick)` when a button has been
/// clicked and passed through `add_enabled` (i.e. connected==true).
pub(crate) fn render_mode_selector(
    ui: &mut egui::Ui,
    ctx: &ControlContext,
) -> Option<ModeClick> {
    coverage::register(
        "mode_selector",
        ctx.surface,
        ctx.channel,
        ctx.density,
        true,
    );

    let modes = match ctx.density {
        UiDensity::Basic => MODES_BASIC,
        UiDensity::Extended => MODES_EXTENDED,
    };
    // Popouts (Extended) use smaller text for a compact row.
    let label_size = match ctx.density {
        UiDensity::Basic => 14.0,
        UiDensity::Extended => 11.0,
    };

    let mut clicked: Option<ModeClick> = None;
    ui.horizontal(|ui| {
        ui.label("Mode:");
        for &(mode_val, label) in modes {
            let btn = if ctx.rx_state.mode == mode_val {
                egui::Button::new(RichText::new(label).size(label_size).strong())
                    .fill(Color32::from_rgb(100, 160, 230))
            } else {
                egui::Button::new(RichText::new(label).size(label_size))
            };
            let resp = ui.add_enabled(ctx.connected, btn).on_hover_text("Set receive mode.");
            if resp.clicked() {
                ctx.events.emit(UiEvent::ClickReceived {
                    control_id: "mode_selector",
                    channel: ctx.channel,
                    surface: ctx.surface,
                    density: ctx.density,
                    was_enabled: ctx.connected,
                });
                clicked = Some(ModeClick { mode: mode_val });
            }
        }
    });
    clicked
}
