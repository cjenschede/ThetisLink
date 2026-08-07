// SPDX-License-Identifier: GPL-2.0-or-later

//! Band-selector render helper.
//!
//! This is the first implemented control helper (sub-step 2a of
//! PATCH-client-controls-refactor). Replaces the inline band-button blocks in
//! `render_rx1_controls_inner` and `render_rx2_controls_inner`.
//!
//! A band switch is conceptually one logical user action (`UiIntent::SelectBand`)
//! but results in multiple commands (SetMode, SetFrequency, filters, NR)
//! via the existing `ThetisLinkApp::restore_band` method. The helper signals
//! the click; the caller performs the multi-command action and closes the
//! observability chain via `dispatch()` for the effective band switch.

use egui::{Color32, RichText};

use super::coverage;
use super::{ControlContext, UiEvent};
use crate::ui::helpers::band_label;

/// Bands that are available on every RX channel. A single source of truth -
/// previously two copies (`render_rx1_controls_inner` + `render_rx2_controls_inner`).
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

/// Information about a clicked band button.
pub(crate) struct BandClick {
    pub(crate) label: &'static str,
    pub(crate) default_freq_hz: u64,
}

/// Renders the band-selector row. Guards internally on `ctx.connected` via
/// `add_enabled`, emits `ClickReceived` on click, registers coverage with
/// `guarded=true`.
///
/// Returns `Some(BandClick { ... })` when a band button has been clicked
/// (and the channel was connected - `add_enabled` guarantees that). The caller is
/// responsible for the multi-command band-switch action and the
/// intent->command chain in observability.
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
            let resp = ui.add_enabled(ctx.connected, btn).on_hover_text("Select band.");
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

