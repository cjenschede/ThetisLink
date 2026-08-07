// SPDX-License-Identifier: GPL-2.0-or-later

//! Frequency render-helpers (sub-step 2c + 2d).
//!
//! Contains two independent helpers:
//!
//! - `render_freq_step_controls` (sub-step 2c): `−` + step-size + `+` row.
//!   Migrates 3 render paths (RX1 popout, RX2 popout, Tab::Radio). `−`/`+`
//!   buttons are `add_enabled`-guarded; step-size selector is by-design
//!   offline (`guarded=false` in coverage).
//!
//! - `render_frequency_display` (sub-step 2d): inline freq-label/edit state
//!   machine + scroll-wheel tuning. Migrates RX1 popout + Tab::Radio (RX2
//!   popout scope-trim - original had no inline-edit). Addresses
//!   scroll-tuning + inline-edit connected-guard gaps.

use egui::{Color32, RichText};

use super::coverage;
use super::{ControlContext, RxChannel, UiDensity, UiEvent};
use crate::ui::helpers::{format_frequency, render_freq_scroll};

/// Available freq-steps in Hz. Order matches `FREQ_STEP_LABELS`.
pub(crate) const FREQ_STEPS: &[u64] = &[10, 100, 500, 1_000, 10_000];

/// Labels for the step-size selector. Must-match `FREQ_STEPS` len.
pub(crate) const FREQ_STEP_LABELS: &[&str] = &["10 Hz", "100 Hz", "500 Hz", "1 kHz", "10 kHz"];

/// Step `current_hz` by `delta_hz` and keep the result on the step grid, inside
/// `[min_hz, max_hz]`.
///
/// Tuning limits are derived from hardware widths (the DDC band edge is
/// `centre ± 0.9 · span/2 − filter_edge`), so they land on arbitrary Hz values.
/// Clamping straight to such a limit leaves the readout on something like
/// 14.238 kHz + 238 Hz while the user is stepping in whole kHz. So the limits
/// are themselves snapped inward to the grid first: the last reachable point is
/// the highest multiple of `step_hz` that still fits.
///
/// An off-grid start (click-to-tune, Copy VFO, a recalled memory) is snapped to
/// the nearest grid point before the step is applied - same rule the RX
/// scroll-tuning has always used.
pub(crate) fn step_on_grid(
    current_hz: u64,
    delta_hz: i64,
    step_hz: u64,
    min_hz: u64,
    max_hz: u64,
) -> u64 {
    if step_hz == 0 {
        return (current_hz as i64 + delta_hz).clamp(min_hz as i64, max_hz as i64) as u64;
    }
    let step = step_hz as i64;
    // Grid points that still fit inside the limits.
    let grid_min = min_hz.div_ceil(step_hz) * step_hz;
    let grid_max = (max_hz / step_hz) * step_hz;
    if grid_min > grid_max {
        // Window narrower than one step: nothing on the grid fits, stay legal.
        return current_hz.clamp(min_hz, max_hz);
    }
    let snapped = ((current_hz as i64 + step / 2) / step) * step;
    (snapped + delta_hz).clamp(grid_min as i64, grid_max as i64) as u64
}

/// Click-result of `render_freq_step_controls`.
///
/// **Step-size selector** clicks are deliberately NOT in this enum: they mutate
/// `ctx.rx_state.freq_step_index` internally (by-design offline, `guarded=false`).
/// The caller writes that mutation back to `self.<channel>_freq_step_index`
/// via the `rx_snap` writeback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FreqStepAction {
    Decrement,
    Increment,
}

impl FreqStepAction {
    /// Signed delta in Hz for the currently-selected step-size.
    pub(crate) fn delta_hz(self, freq_step_index: usize) -> i64 {
        let step = FREQ_STEPS.get(freq_step_index).copied().unwrap_or(1_000) as i64;
        match self {
            FreqStepAction::Decrement => -step,
            FreqStepAction::Increment => step,
        }
    }
}

/// Renders the frequency step-controls row: `−` + step-size selector + `+`.
///
/// `ctx.density` determines label sizes (Basic: 16.0, Extended: 14.0 -
/// matches original Tab::Radio vs popout styling).
///
/// `−` and `+` buttons are guarded on `ctx.connected` via `add_enabled`.
/// Step-size buttons deliberately keep working offline (no command, only
/// local UI-state).
///
/// Returns `Some(Decrement | Increment)` when the user has requested a
/// frequency change. The caller must call `dispatch()`
/// and update the `pending_freq` state **only when `dispatched==true`** -
/// otherwise UI-state drifts vs. server-state.
pub(crate) fn render_freq_step_controls(
    ui: &mut egui::Ui,
    ctx: &mut ControlContext,
) -> Option<FreqStepAction> {
    coverage::register(
        "freq_step_arrows",
        ctx.surface,
        ctx.channel,
        ctx.density,
        true,
    );
    coverage::register(
        "freq_step_size",
        ctx.surface,
        ctx.channel,
        ctx.density,
        false,
    );

    let size = match ctx.density {
        super::UiDensity::Basic => 16.0,
        super::UiDensity::Extended => 14.0,
    };

    let mut action: Option<FreqStepAction> = None;
    ui.horizontal(|ui| {
        let minus = egui::Button::new(RichText::new(" - ").size(size));
        if ui.add_enabled(ctx.connected, minus).on_hover_text("Tune down one step.").clicked() {
            ctx.events.emit(UiEvent::ClickReceived {
                control_id: "freq_step_arrows",
                channel: ctx.channel,
                surface: ctx.surface,
                density: ctx.density,
                was_enabled: ctx.connected,
            });
            action = Some(FreqStepAction::Decrement);
        }

        let current_idx = ctx.rx_state.freq_step_index;
        let mut new_idx = current_idx;
        for (i, label) in FREQ_STEP_LABELS.iter().enumerate() {
            let btn = if i == current_idx {
                egui::Button::new(RichText::new(*label).strong())
                    .fill(Color32::from_rgb(100, 160, 230))
            } else {
                egui::Button::new(*label)
            };
            // Step-size selection works offline: no add_enabled guard,
            // no command, no intent-emission. Only local UI-state.
            if ui.add(btn).on_hover_text("Frequency step size.").clicked() {
                ctx.events.emit(UiEvent::ClickReceived {
                    control_id: "freq_step_size",
                    channel: ctx.channel,
                    surface: ctx.surface,
                    density: ctx.density,
                    was_enabled: true,
                });
                new_idx = i;
            }
        }
        if new_idx != current_idx {
            ctx.rx_state.freq_step_index = new_idx;
        }

        let plus = egui::Button::new(RichText::new(" + ").size(size));
        if ui.add_enabled(ctx.connected, plus).on_hover_text("Tune up one step.").clicked() {
            ctx.events.emit(UiEvent::ClickReceived {
                control_id: "freq_step_arrows",
                channel: ctx.channel,
                surface: ctx.surface,
                density: ctx.density,
                was_enabled: ctx.connected,
            });
            action = Some(FreqStepAction::Increment);
        }
    });
    action
}

/// Action-result of `render_frequency_display` that requires command-dispatch.
/// Edit-state transitions (start edit, cancel edit) are mutated internally in
/// `ctx.rx_state`; only the action that implies a network-command is returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FrequencyDisplayAction {
    /// User pressed Enter in the inline-edit with a valid frequency (>0 Hz).
    Submit { hz: u64 },
    /// User scrolled in the freq-area (only in display mode, only when
    /// scroll was not consumed by the spectrum). `delta_hz` is already
    /// combined with the current step-size.
    ScrollTune { delta_hz: i64 },
}

fn vfo_prefix(channel: RxChannel) -> &'static str {
    match channel {
        RxChannel::Rx1 => "VFO A:",
        RxChannel::Rx2 => "VFO B:",
    }
}

/// Renders the VFO frequency display + inline edit.
///
/// **Display mode** (default): shows label "VFO X:  14.200.000 Hz" as a
/// clickable widget. Click -> ctx.rx_state.freq_editing = true (state-transition).
/// Scroll-wheel -> `UiEvent::ScrollTuneApplied` + return `ScrollTune(delta)`.
///
/// **Edit mode** (when `ctx.rx_state.freq_editing == true`): shows a
/// TextEdit with `ctx.rx_state.freq_edit_text`. Lost-focus + Enter + valid hz
/// -> `UiEvent::InlineFreqSubmitted` + return `Submit(hz)`. Lost-focus without
/// a valid Enter -> transition back to display without action.
///
/// **Scroll-gating (Basic density, Tab::Radio):** when `spectrum_enabled` the
/// spectrum-widget consumes scroll-events; the helper skips scroll-detection
/// in that case. In Extended density (popouts) scroll is always active -
/// `render_freq_scroll` checks the `freq_scroll_consumed` memory-flag itself.
///
/// Coverage: registers `frequency_display` (guarded=true - click to edit is
/// closed by `add_enabled`, scroll only comes in when the connected-check in
/// dispatch succeeds).
pub(crate) fn render_frequency_display(
    ui: &mut egui::Ui,
    ctx: &mut ControlContext,
) -> Option<FrequencyDisplayAction> {
    coverage::register(
        "frequency_display",
        ctx.surface,
        ctx.channel,
        ctx.density,
        true,
    );

    let prefix = vfo_prefix(ctx.channel);

    // Edit mode
    if ctx.rx_state.freq_editing {
        #[derive(Clone)]
        enum EditOutcome {
            Keep,
            Cancel,
            Submit(u64),
        }
        let outcome = {
            let mut out = EditOutcome::Keep;
            ui.horizontal(|ui| {
                ui.label(RichText::new(prefix).size(18.0).strong());
                let response = ui.add(
                    egui::TextEdit::singleline(&mut ctx.rx_state.freq_edit_text)
                        .desired_width(140.0)
                        .font(egui::TextStyle::Heading),
                );
                if response.lost_focus() {
                    let enter_pressed = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    out = if enter_pressed {
                        let clean: String = ctx
                            .rx_state
                            .freq_edit_text
                            .chars()
                            .filter(|c| c.is_ascii_digit())
                            .collect();
                        match clean.parse::<u64>() {
                            Ok(hz) if hz > 0 => EditOutcome::Submit(hz),
                            _ => EditOutcome::Cancel,
                        }
                    } else {
                        EditOutcome::Cancel
                    };
                }
                if !response.has_focus() {
                    response.request_focus();
                }
                ui.label(RichText::new("Hz").size(18.0).strong());
            });
            out
        };

        match outcome {
            EditOutcome::Keep => None,
            EditOutcome::Cancel => {
                ctx.rx_state.freq_editing = false;
                None
            }
            EditOutcome::Submit(hz) => {
                let connected = ctx.connected;
                let channel = ctx.channel;
                ctx.events.emit(UiEvent::InlineFreqSubmitted {
                    channel,
                    hz,
                    connected,
                });
                ctx.rx_state.freq_editing = false;
                Some(FrequencyDisplayAction::Submit { hz })
            }
        }
    } else {
        // Display mode
        let freq_hz = ctx.rx_state.frequency_hz;
        // Basic density (Tab::Radio): spectrum-widget consumes scroll when
        // visible; skip per-digit scroll detection in that configuration to
        // prevent double-fire. Extended density (popouts) has its own
        // spectrum-viewport and does not share scroll.
        let scroll_gated = ctx.density == UiDensity::Basic && ctx.shared.spectrum_enabled;

        #[derive(Clone, Copy)]
        enum DisplayOutcome {
            Nothing,
            StartEdit,
            Scroll(i64),
        }
        let outcome = {
            let mut out = DisplayOutcome::Nothing;
            ui.horizontal(|ui| {
                // Prefix label is clickable for edit-mode transition.
                // Guarded on `ctx.connected`: without a connection there is no
                // point starting edit (dispatch would fail anyway) - consistent
                // with `band/mode/freq_step_arrows` UX.
                let prefix_widget = egui::Label::new(
                    RichText::new(format!("{}  ", prefix)).size(18.0).strong(),
                )
                .sense(egui::Sense::click());
                let prefix_resp = ui.add_enabled(ctx.connected, prefix_widget)
                    .on_hover_text("Click to edit frequency; scroll over a digit to tune.");
                if prefix_resp.clicked() {
                    out = DisplayOutcome::StartEdit;
                }

                if freq_hz > 0 {
                    if scroll_gated {
                        // Gated: just render as label (no per-digit scroll).
                        ui.label(
                            RichText::new(format!("{} Hz", format_frequency(freq_hz)))
                                .size(18.0)
                                .strong(),
                        );
                    } else {
                        // `render_freq_scroll` renders the digits + " Hz" suffix
                        // and returns `Some(delta_hz)` (absolute Hz, from digit-position).
                        if let Some(delta_hz) = render_freq_scroll(ui, freq_hz) {
                            out = DisplayOutcome::Scroll(delta_hz);
                        }
                    }
                } else {
                    ui.label(RichText::new("--- Hz").size(18.0).strong());
                }
            });
            out
        };

        match outcome {
            DisplayOutcome::Nothing => None,
            DisplayOutcome::StartEdit => {
                ctx.rx_state.freq_editing = true;
                ctx.rx_state.freq_edit_text = if freq_hz > 0 {
                    freq_hz.to_string()
                } else {
                    String::new()
                };
                None
            }
            DisplayOutcome::Scroll(delta_hz) => {
                let channel = ctx.channel;
                let connected = ctx.connected;
                ctx.events.emit(UiEvent::ScrollTuneApplied {
                    channel,
                    delta_hz,
                    connected,
                });
                Some(FrequencyDisplayAction::ScrollTune { delta_hz })
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use super::step_on_grid;

    /// The band edge is an arbitrary Hz value; stepping into it must still land
    /// on the step grid instead of parking the readout on ...238 Hz.
    #[test]
    fn the_band_edge_is_snapped_inward_to_the_grid() {
        let max = 14_241_238; // centre + 0.9*span/2 - filter edge
        let min = 14_158_762;
        assert_eq!(step_on_grid(14_240_000, 1_000, 1_000, min, max), 14_241_000);
        // Already at the last grid point: another step stays there, not on the raw edge.
        assert_eq!(step_on_grid(14_241_000, 1_000, 1_000, min, max), 14_241_000);
        assert_eq!(step_on_grid(14_159_000, -1_000, 1_000, min, max), 14_159_000);
    }

    /// Off-grid starting points (click-to-tune, Copy VFO, memory recall) are
    /// pulled onto the grid by the first step - same rule as RX scroll tuning.
    #[test]
    fn an_off_grid_start_is_snapped_before_stepping() {
        let (min, max) = (0, 30_000_000);
        assert_eq!(step_on_grid(14_200_400, 1_000, 1_000, min, max), 14_201_000);
        assert_eq!(step_on_grid(14_200_600, -1_000, 1_000, min, max), 14_200_000);
    }

    /// A fine step keeps its own grid; the helper is not hard-wired to kHz.
    #[test]
    fn the_grid_follows_the_step_size() {
        let (min, max) = (0, 14_241_238);
        assert_eq!(step_on_grid(14_241_200, 10, 10, min, max), 14_241_210);
        // Last 10 Hz point below the raw edge (…238) is …230, and it holds there.
        assert_eq!(step_on_grid(14_241_230, 10, 10, min, max), 14_241_230);
        assert_eq!(step_on_grid(14_200_000, 10_000, 10_000, min, max), 14_210_000);
    }

    /// Degenerate windows must stay legal rather than panic or escape.
    #[test]
    fn a_window_narrower_than_one_step_stays_inside_the_limits() {
        let got = step_on_grid(14_200_100, 1_000, 1_000, 14_200_050, 14_200_900);
        assert!((14_200_050..=14_200_900).contains(&got), "escaped: {got}");
    }
}
