// SPDX-License-Identifier: GPL-2.0-or-later

//! Frequency render-helpers (sub-stap 2c + 2d).
//!
//! Bevat twee onafhankelijke helpers:
//!
//! - `render_freq_step_controls` (sub-stap 2c): `−` + step-size + `+` row.
//!   Migreert 3 render-paden (RX1 popout, RX2 popout, Tab::Radio). `−`/`+`
//!   knoppen zijn `add_enabled`-guarded; step-size selector is by-design
//!   offline (`guarded=false` in coverage).
//!
//! - `render_frequency_display` (sub-stap 2d): inline freq-label/edit state
//!   machine + scroll-wheel tuning. Migreert RX1 popout + Tab::Radio (RX2
//!   popout scope-trim — origineel had geen inline-edit). Adresseert
//!   scroll-tuning + inline-edit connected-guard gaps.

use egui::{Color32, RichText};

use super::coverage;
use super::{ControlContext, RxChannel, UiDensity, UiEvent};
use crate::ui::helpers::{format_frequency, render_freq_scroll};

/// Beschikbare freq-stappen in Hz. Volgorde matcht `FREQ_STEP_LABELS`.
pub(crate) const FREQ_STEPS: &[u64] = &[10, 100, 500, 1_000, 10_000];

/// Labels voor de step-size selector. Must-match met `FREQ_STEPS` len.
pub(crate) const FREQ_STEP_LABELS: &[&str] = &["10 Hz", "100 Hz", "500 Hz", "1 kHz", "10 kHz"];

/// Klik-resultaat van `render_freq_step_controls`.
///
/// **Step-size selector** klikken zijn bewust NIET in deze enum: ze muteren
/// `ctx.rx_state.freq_step_index` intern (by-design offline, `guarded=false`).
/// De caller schrijft die mutatie terug naar `self.<channel>_freq_step_index`
/// via de `rx_snap`-writeback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FreqStepAction {
    Decrement,
    Increment,
}

impl FreqStepAction {
    /// Signed delta in Hz voor de momenteel-geselecteerde step-size.
    pub(crate) fn delta_hz(self, freq_step_index: usize) -> i64 {
        let step = FREQ_STEPS.get(freq_step_index).copied().unwrap_or(1_000) as i64;
        match self {
            FreqStepAction::Decrement => -step,
            FreqStepAction::Increment => step,
        }
    }
}

/// Rendert de frequency step-controls row: `−` + step-size selector + `+`.
///
/// `ctx.density` bepaalt label-groottes (Basic: 16.0, Extended: 14.0 —
/// matcht originele Tab::Radio vs popout styling).
///
/// `−` en `+` knoppen zijn guarded op `ctx.connected` via `add_enabled`.
/// Step-size knoppen blijven bewust werken offline (geen command, alleen
/// lokale UI-state).
///
/// Retourneert `Some(Decrement | Increment)` wanneer de gebruiker een
/// frequentie-wijziging heeft gevraagd. De caller moet `dispatch()` aanroepen
/// en **alleen bij `dispatched==true`** de `pending_freq` state updaten —
/// anders drift UI-state vs. server-state.
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
        if ui.add_enabled(ctx.connected, minus).clicked() {
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
            // Step-size selectie werkt offline: geen add_enabled guard,
            // geen command, geen intent-emission. Alleen lokale UI-state.
            if ui.add(btn).clicked() {
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
        if ui.add_enabled(ctx.connected, plus).clicked() {
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

/// Actie-resultaat van `render_frequency_display` die command-dispatch vereist.
/// Edit-state transities (start edit, cancel edit) worden intern in `ctx.rx_state`
/// gemuteerd; alleen de actie die een netwerk-command impliceert komt terug.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FrequencyDisplayAction {
    /// Gebruiker drukte Enter in de inline-edit met een geldige frequentie (>0 Hz).
    Submit { hz: u64 },
    /// Gebruiker scrollde in het freq-gebied (alleen in display mode, alleen als
    /// scroll niet door spectrum is geconsumeerd). `delta_hz` is reeds
    /// gecombineerd met de huidige step-size.
    ScrollTune { delta_hz: i64 },
}

fn vfo_prefix(channel: RxChannel) -> &'static str {
    match channel {
        RxChannel::Rx1 => "VFO A:",
        RxChannel::Rx2 => "VFO B:",
    }
}

/// Rendert de VFO frequency display + inline edit.
///
/// **Display mode** (default): toont label "VFO X:  14.200.000 Hz" als
/// klikbare widget. Klik → ctx.rx_state.freq_editing = true (state-transition).
/// Scroll-wheel → `UiEvent::ScrollTuneApplied` + return `ScrollTune(delta)`.
///
/// **Edit mode** (wanneer `ctx.rx_state.freq_editing == true`): toont
/// TextEdit met `ctx.rx_state.freq_edit_text`. Lost-focus + Enter + valid hz
/// → `UiEvent::InlineFreqSubmitted` + return `Submit(hz)`. Lost-focus zonder
/// geldige Enter → transitie terug naar display zonder actie.
///
/// **Scroll-gating (Basic density, Tab::Radio):** wanneer `spectrum_enabled`
/// consumeert het spectrum-widget scroll-events; de helper slaat scroll-detectie
/// over in dat geval. In Extended density (popouts) is scroll altijd actief —
/// `render_freq_scroll` checkt zelf de `freq_scroll_consumed` memory-flag.
///
/// Coverage: registreert `frequency_display` (guarded=true — klik naar edit is
/// door-`add_enabled` gesloten, scroll komt alleen binnen als connected-check in
/// dispatch slaagt).
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
        // Basic density (Tab::Radio): spectrum-widget consumeert scroll wanneer
        // zichtbaar; skip per-digit scroll detection in die configuratie om
        // dubbel-fire te voorkomen. Extended density (popouts) heeft eigen
        // spectrum-viewport en deelt scroll niet.
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
                // Prefix label is klikbaar voor edit-mode transitie.
                // Guarded op `ctx.connected`: zonder verbinding geen zin om edit
                // te starten (dispatch zou toch falen) — consistent met
                // `band/mode/freq_step_arrows` UX.
                let prefix_widget = egui::Label::new(
                    RichText::new(format!("{}  ", prefix)).size(18.0).strong(),
                )
                .sense(egui::Sense::click());
                let prefix_resp = ui.add_enabled(ctx.connected, prefix_widget);
                if prefix_resp.clicked() {
                    out = DisplayOutcome::StartEdit;
                }

                if freq_hz > 0 {
                    if scroll_gated {
                        // Gated: render gewoon als label (geen per-digit scroll).
                        ui.label(
                            RichText::new(format!("{} Hz", format_frequency(freq_hz)))
                                .size(18.0)
                                .strong(),
                        );
                    } else {
                        // `render_freq_scroll` rendert de digits + " Hz" suffix
                        // en returnt `Some(delta_hz)` (absolute Hz, uit digit-positie).
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

