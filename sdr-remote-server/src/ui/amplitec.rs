// SPDX-License-Identifier: GPL-2.0-or-later

use egui::{Color32, RichText};

use crate::amplitec::AmplitecSwitch;

/// Visuele toestand voor `antenna_button`.
#[derive(Clone, Copy)]
enum AntennaState {
    /// Deze positie is de actief geselecteerde - blauwe vulling
    /// (ThetisLink-conventie voor toggled-on, zie memory
    /// `feedback_ui_button_color_convention`).
    Active,
    /// Deze positie is bezet door de andere poort - disabled-ish look.
    Blocked,
    /// Normale klikbare staat.
    Inactive,
}

/// Twee-regelige antenne-knop:
///   - bovenste regel: `Ant<N>` (positie-id, klein/gedempt)
///   - onderste regel: optionele alias (groter, prominent)
///
/// Operator-keuze: de alias-tekst krijgt het visuele primaat omdat dat
/// de functionele naam is; het positie-nummer dient slechts als
/// identifier op een rij van 6 knoppen.
///
/// Knopvulling: blauw bij `Active`, gedempt grijs bij `Blocked`,
/// default bij `Inactive`. Op hover: lichtere fill voor visuele
/// feedback (per `feedback_ui_hover_always`). De `max_width`-cap
/// zorgt dat een rij van 6 knoppen meeschaalt met de window-breedte:
/// nooit groter dan natuurlijk, wel kleiner.
fn antenna_button(
    ui: &mut egui::Ui,
    enabled: bool,
    pos: u8,
    alias: &str,
    state: AntennaState,
    max_width: f32,
) -> egui::Response {
    use egui::{vec2, Align2, FontId, Sense, Stroke};

    let pos_text = format!("Ant{}", pos);
    let alias_text = alias.trim();

    // Font-resolutie: bovenste regel (positie-id) is de kleine
    // identifier, onderste regel (alias) is de prominent leesbare
    // functionele naam. Operator-keuze: alias-tekst krijgt zo het
    // visuele primaat.
    let style = ui.style().clone();
    let pos_font: FontId = egui::TextStyle::Small.resolve(&style);
    let alias_font: FontId = egui::TextStyle::Button.resolve(&style);

    // Layout galleys om de knop-grootte te berekenen
    let pos_galley = ui.painter().layout_no_wrap(
        pos_text.clone(),
        pos_font.clone(),
        Color32::TEMPORARY_COLOR,
    );
    let alias_galley = ui.painter().layout_no_wrap(
        alias_text.to_string(),
        alias_font.clone(),
        Color32::TEMPORARY_COLOR,
    );

    let pad_x = 10.0_f32;
    let pad_y = 4.0_f32;
    let gap = 1.0_f32;
    // Natuurlijke breedte op basis van de bredere tekst-regel. Wordt
    // afgekapt door `max_width` zodat 6 knoppen op een rij meeschalen
    // met de window-breedte - nooit groter dan natuurlijk, wel kleiner.
    let natural_w = pos_galley.size().x.max(alias_galley.size().x) + pad_x * 2.0;
    let width = natural_w.min(max_width).max(24.0);
    let height = pos_galley.size().y + alias_galley.size().y + pad_y * 2.0 + gap;

    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), sense);

    // Fill-color per state, met hover-bump
    let visuals = ui.visuals();
    let (mut fill, stroke_color) = match state {
        AntennaState::Active => (Color32::from_rgb(100, 160, 230), visuals.widgets.active.fg_stroke.color),
        AntennaState::Blocked => (
            Color32::from_rgb(180, 180, 180),
            visuals.widgets.inactive.fg_stroke.color,
        ),
        AntennaState::Inactive => (
            visuals.widgets.inactive.bg_fill,
            visuals.widgets.inactive.fg_stroke.color,
        ),
    };
    if enabled && response.hovered() {
        // Hover-bump: licht oplichten t.o.v. base-fill.
        fill = fill.linear_multiply(1.15);
    }

    let painter = ui.painter();
    painter.rect_filled(rect, 4.0, fill);
    painter.rect_stroke(rect, 4.0, Stroke::new(1.0, stroke_color));

    // Tekst-kleur per regel: positie-nummer altijd contrastrijk,
    // alias iets gedempter. Bij Active (blauwe achtergrond) wit zodat
    // tekst leesbaar blijft.
    let (pos_color, alias_color) = match state {
        AntennaState::Active => (Color32::WHITE, Color32::from_rgb(220, 230, 245)),
        AntennaState::Blocked => (Color32::from_rgb(120, 120, 120), Color32::from_rgb(160, 160, 160)),
        AntennaState::Inactive => (Color32::from_rgb(20, 20, 30), Color32::from_rgb(90, 90, 100)),
    };

    // Render boven-regel en onder-regel gecentreerd
    let center_x = rect.center().x;
    let top_y = rect.top() + pad_y + pos_galley.size().y * 0.5;
    let bottom_y = rect.bottom() - pad_y - alias_galley.size().y * 0.5;
    painter.text(
        egui::pos2(center_x, top_y),
        Align2::CENTER_CENTER,
        &pos_text,
        pos_font,
        pos_color,
    );
    if !alias_text.is_empty() {
        painter.text(
            egui::pos2(center_x, bottom_y),
            Align2::CENTER_CENTER,
            alias_text,
            alias_font,
            alias_color,
        );
    }

    response
}

/// Pending rename-state voor het Amplitec-paneel: (positie 1..=6,
/// edit-buffer). `None` betekent: geen dialog open. Het dialog wordt
/// gerenderd door `render_amplitec_panel` aan het einde wanneer de
/// state Some is - context-menu op een antenne-knop zet de state via
/// `open_rename_dialog`.
fn rename_state() -> &'static std::sync::Mutex<Option<(u8, String)>> {
    use std::sync::{Mutex, OnceLock};
    static STATE: OnceLock<Mutex<Option<(u8, String)>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(None))
}

fn open_rename_dialog(pos: u8, current: &str) {
    *rename_state().lock().unwrap() = Some((pos, current.to_string()));
}

/// Render de rename-modal als de state Some is. Operator kan een nieuw
/// label invoeren of cancelen. Bij OK wordt `config.amplitec_labels`
/// via `modify_config` bijgewerkt - een auto-restart is niet nodig
/// omdat labels live geinjecteerd worden in elke render-call.
fn render_rename_dialog(ctx: &egui::Context) {
    let state = rename_state();
    let mut current = state.lock().unwrap().clone();
    let Some((pos, ref mut buffer)) = current else { return };
    let mut close = false;
    let mut save = false;
    egui::Window::new(format!("Hernoem antenne-positie {}", pos))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.label(rust_i18n::t!("srv_new_name").to_string());
            let resp = ui.add(
                egui::TextEdit::singleline(buffer)
                    .desired_width(220.0)
                    .hint_text(rust_i18n::t!("srv_new_name_hint").to_string()),
            );
            // Enter in het tekstvak commit ook
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                save = true;
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("OK").clicked() {
                    save = true;
                }
                if ui.button(rust_i18n::t!("srv_cancel").to_string()).clicked() {
                    close = true;
                }
            });
        });
    if save {
        let new_label = buffer.trim().to_string();
        if !new_label.is_empty() {
            crate::config::modify_config(|c| {
                if let Some(idx) = (pos as usize).checked_sub(1) {
                    if idx < c.amplitec_labels.len() {
                        c.amplitec_labels[idx] = new_label.clone();
                    }
                }
            });
            log::info!("Amplitec label pos {} renamed to \"{}\"", pos, new_label);
        }
        close = true;
    }
    if close {
        *state.lock().unwrap() = None;
    } else {
        // Update buffer-state na user-typing
        *state.lock().unwrap() = current;
    }
}

pub(super) fn render_amplitec_panel(
    ui: &mut egui::Ui,
    amplitec: &AmplitecSwitch,
    status: &crate::amplitec::AmplitecStatus,
    labels: &[String; 6],
    log_entries: &[(String, String)],
    show_log: &mut bool,
) {
    // Header
    ui.horizontal(|ui| {
        ui.heading("Amplitec 6/2 Antenna Switch");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if status.connected {
                ui.colored_label(Color32::GREEN, rust_i18n::t!("srv_online").to_string());
            } else {
                ui.colored_label(Color32::RED, rust_i18n::t!("srv_offline").to_string());
            }
            ui.checkbox(show_log, rust_i18n::t!("srv_log_cb").to_string());
        });
    });
    ui.separator();

    // Poort A (TX+RX)
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(rust_i18n::t!("srv_port_a_txrx").to_string()).strong());
        if status.switch_a > 0 {
            let label = &labels[(status.switch_a - 1).min(5) as usize];
            ui.label(format!("  {} {}", rust_i18n::t!("srv_current"), label));
        }
    });
    ui.horizontal(|ui| {
        let available = ui.available_width();
        let spacing = ui.spacing().item_spacing.x;
        let max_btn_w = ((available - 5.0 * spacing) / 6.0).max(24.0);
        for pos in 1..=6u8 {
            let is_active = status.switch_a == pos;
            let is_blocked = status.switch_b == pos;
            let label = &labels[(pos - 1) as usize];
            let state = if is_active {
                AntennaState::Active
            } else if is_blocked {
                AntennaState::Blocked
            } else {
                AntennaState::Inactive
            };
            let resp = antenna_button(ui, status.connected, pos, label, state, max_btn_w);
            if resp.clicked() {
                amplitec.send_command(crate::amplitec::AmplitecCmd::SetSwitchA(pos));
            }
            let resp = if is_blocked {
                resp.on_hover_text(rust_i18n::t!("srv_ant_busy_b", pos = pos, label = label).to_string())
            } else {
                resp
            };
            resp.context_menu(|ui| {
                if ui.button(rust_i18n::t!("srv_rename").to_string()).clicked() {
                    open_rename_dialog(pos, label);
                    ui.close_menu();
                }
            });
        }
    });

    ui.add_space(8.0);

    // Poort B (RX only)
    ui.horizontal(|ui| {
        ui.label(RichText::new(rust_i18n::t!("srv_port_b_rx").to_string()).strong());
        if status.switch_b > 0 {
            let label = &labels[(status.switch_b - 1).min(5) as usize];
            ui.label(format!("  {} {}", rust_i18n::t!("srv_current"), label));
        }
    });
    ui.horizontal(|ui| {
        let available = ui.available_width();
        let spacing = ui.spacing().item_spacing.x;
        let max_btn_w = ((available - 5.0 * spacing) / 6.0).max(24.0);
        for pos in 1..=6u8 {
            let is_active = status.switch_b == pos;
            let is_blocked = status.switch_a == pos;
            let label = &labels[(pos - 1) as usize];
            let state = if is_active {
                AntennaState::Active
            } else if is_blocked {
                AntennaState::Blocked
            } else {
                AntennaState::Inactive
            };
            let resp = antenna_button(ui, status.connected, pos, label, state, max_btn_w);
            if resp.clicked() {
                amplitec.send_command(crate::amplitec::AmplitecCmd::SetSwitchB(pos));
            }
            let resp = if is_blocked {
                resp.on_hover_text(rust_i18n::t!("srv_ant_busy_a", pos = pos, label = label).to_string())
            } else {
                resp
            };
            resp.context_menu(|ui| {
                if ui.button(rust_i18n::t!("srv_rename").to_string()).clicked() {
                    open_rename_dialog(pos, label);
                    ui.close_menu();
                }
            });
        }
    });

    // Rename-dialog (modal) - verschijnt boven het paneel zolang
    // `rename_state()` Some is. Open via rechtermuisknop op een
    // antenne-knop -> "Hernoem...".
    render_rename_dialog(ui.ctx());

    // Log (collapsible, toggled via header checkbox)
    if *show_log {
        ui.add_space(4.0);
        ui.separator();
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .max_height(150.0)
            .show(ui, |ui| {
                for (time, msg) in log_entries.iter().rev() {
                    ui.label(
                        RichText::new(format!("{}  {}", time, msg))
                            .monospace()
                            .size(10.0)
                            .color(Color32::from_rgb(180, 180, 180)),
                    );
                }
            });
    }
}
