// SPDX-License-Identifier: GPL-2.0-or-later

//! Mode-selector render-helper (sub-stap 2b).
//!
//! Vervangt drie render-paden met één helper:
//! - `render_rx1_controls_inner` (RX1 popouts, ~mod.rs:2275) — 8 modes, Extended
//! - `render_rx2_controls_inner` (RX2 popouts, ~mod.rs:2636) — 8 modes, Extended
//! - Tab::Radio hoofdvenster (~mod.rs:3594) — 4 modes, Basic — was **ongegeguard**
//!   (connected-guard ontbrak).
//!
//! `UiDensity` bepaalt welke mode-set zichtbaar is: Basic toont alleen de
//! meest gebruikte voice-modes, Extended toont ook CW + digital-modes.

use egui::{Color32, RichText};

use super::coverage;
use super::{ControlContext, UiDensity, UiEvent};

/// Volledige mode-set (popouts). (mode_val, label) — mode_val komt uit het
/// TCI-protocol: 0=LSB, 1=USB, 3=CW-L, 4=CW-U, 5=FM, 6=AM, 7=DIGU, 9=DIGL.
pub(crate) const MODES_EXTENDED: &[(u8, &str)] = &[
    (0, "LSB"), (1, "USB"), (3, "CW-L"), (4, "CW-U"),
    (6, "AM"), (5, "FM"), (7, "DIGU"), (9, "DIGL"),
];

/// Basisscherm mode-set (Tab::Radio): alleen de meestgebruikte voice-modes.
pub(crate) const MODES_BASIC: &[(u8, &str)] = &[
    (0, "LSB"), (1, "USB"), (6, "AM"), (5, "FM"),
];

pub(crate) struct ModeClick {
    pub(crate) mode: u8,
}

/// Rendert de mode-selector row. Guards intern op `ctx.connected` via
/// `add_enabled`, emit `ClickReceived` bij klik, registreert coverage met
/// `guarded=true`.
///
/// Leest actuele mode uit `ctx.rx_state.mode`; selecteert mode-set op basis
/// van `ctx.density`. Retourneert `Some(ModeClick)` wanneer een knop is
/// aangeklikt en door `add_enabled` is gekomen (dus connected==true).
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
    // Popouts (Extended) gebruiken kleinere tekst voor compacte row.
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
            let resp = ui.add_enabled(ctx.connected, btn);
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
