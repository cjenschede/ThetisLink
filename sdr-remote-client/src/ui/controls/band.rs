// SPDX-License-Identifier: GPL-2.0-or-later

//! Band-selector render-helper.
//!
//! Dit is de eerste geïmplementeerde control-helper (sub-stap 2a van
//! PATCH-client-controls-refactor). Vervangt de inline band-button blocks in
//! `render_rx1_controls_inner` en `render_rx2_controls_inner`.
//!
//! Band-switch is conceptueel één logische user-actie (`UiIntent::SelectBand`)
//! maar resulteert in meerdere commands (SetMode, SetFrequency, filters, NR)
//! via de bestaande `ThetisLinkApp::restore_band` methode. De helper signaleert
//! de klik; de caller voert de multi-command actie uit en sluit de
//! observability-chain via `dispatch()` voor de effectieve band-switch.

use egui::{Color32, RichText};

use super::coverage;
use super::{ControlContext, UiEvent};
use crate::ui::helpers::band_label;

/// Bands die op ieder RX-kanaal beschikbaar zijn. Één bron van waarheid —
/// voorheen twee kopieën (`render_rx1_controls_inner` + `render_rx2_controls_inner`).
pub(crate) const BANDS: &[(&str, u64)] = &[
    ("160m", 1_900_000),
    ("80m", 3_700_000),
    ("60m", 5_351_000),
    ("40m", 7_100_000),
    ("30m", 10_120_000),
    ("20m", 14_200_000),
    ("17m", 18_100_000),
    ("15m", 21_200_000),
    ("12m", 24_930_000),
    ("10m", 28_500_000),
    ("6m", 50_200_000),
];

/// Informatie over een geklikte band-knop.
pub(crate) struct BandClick {
    pub(crate) label: &'static str,
    pub(crate) default_freq_hz: u64,
}

/// Rendert de band-selector row. Guards intern op `ctx.connected` via
/// `add_enabled`, emit `ClickReceived` bij klik, registreert coverage met
/// `guarded=true`.
///
/// Retourneert `Some(BandClick { ... })` wanneer een band-knop geklikt is
/// (en het kanaal connected was — `add_enabled` garandeert dat). Caller is
/// verantwoordelijk voor de multi-command band-switch actie en de
/// intent→command chain in observability.
pub(crate) fn render_band_selector(
    ui: &mut egui::Ui,
    ctx: &ControlContext,
) -> Option<BandClick> {
    coverage::register(
        "band_selector",
        ctx.surface,
        ctx.channel,
        ctx.density,
        true,
    );

    let mut clicked: Option<BandClick> = None;
    ui.horizontal(|ui| {
        ui.label("Band:");
        let current = band_label(ctx.rx_state.frequency_hz);
        for &(label, default_freq) in BANDS {
            let btn = if label == current {
                egui::Button::new(RichText::new(label).size(11.0).strong())
                    .fill(Color32::from_rgb(100, 160, 230))
            } else {
                egui::Button::new(RichText::new(label).size(11.0))
            };
            let resp = ui.add_enabled(ctx.connected, btn);
            if resp.clicked() {
                ctx.events.emit(UiEvent::ClickReceived {
                    control_id: "band_selector",
                    channel: ctx.channel,
                    surface: ctx.surface,
                    density: ctx.density,
                    was_enabled: ctx.connected,
                });
                clicked = Some(BandClick {
                    label,
                    default_freq_hz: default_freq,
                });
            }
        }
    });
    clicked
}

