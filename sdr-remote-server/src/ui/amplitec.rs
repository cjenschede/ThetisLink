// SPDX-License-Identifier: GPL-2.0-or-later

use egui::{Color32, RichText};

use crate::amplitec::AmplitecSwitch;

/// Visual state for `antenna_button`.
#[derive(Clone, Copy)]
enum AntennaState {
    /// This position is the actively selected one - blue fill
    /// (ThetisLink convention for toggled-on, see memory
    /// `feedback_ui_button_color_convention`).
    Active,
    /// This position is occupied by the other port - disabled-ish look.
    Blocked,
    /// Normal clickable state.
    Inactive,
}

/// Two-line antenna button:
///   - top line: `Ant<N>` (position id, small/muted)
///   - bottom line: optional alias (larger, prominent)
///
/// Operator choice: the alias text gets the visual primacy because that
/// is the functional name; the position number only serves as an
/// identifier in a row of 6 buttons.
///
/// Button fill: blue for `Active`, muted grey for `Blocked`,
/// default for `Inactive`. On hover: lighter fill for visual
/// feedback (per `feedback_ui_hover_always`). The `max_width` cap
/// ensures a row of 6 buttons scales with the window width:
/// never larger than natural, but smaller is allowed.
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

    // Font resolution: top line (position id) is the small
    // identifier, bottom line (alias) is the prominently readable
    // functional name. Operator choice: alias text thus gets the
    // visual primacy.
    let style = ui.style().clone();
    let pos_font: FontId = egui::TextStyle::Small.resolve(&style);
    let alias_font: FontId = egui::TextStyle::Button.resolve(&style);

    // Layout galleys to compute the button size
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
    // Natural width based on the wider text line. Gets clamped
    // by `max_width` so that 6 buttons in a row scale with
    // the window width - never larger than natural, but smaller is allowed.
    let natural_w = pos_galley.size().x.max(alias_galley.size().x) + pad_x * 2.0;
    let width = natural_w.min(max_width).max(24.0);
    let height = pos_galley.size().y + alias_galley.size().y + pad_y * 2.0 + gap;

    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), sense);

    // Fill color per state, with hover bump
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
        // Hover bump: lighten slightly relative to base fill.
        fill = fill.linear_multiply(1.15);
    }

    let painter = ui.painter();
    painter.rect_filled(rect, 4.0, fill);
    painter.rect_stroke(rect, 4.0, Stroke::new(1.0, stroke_color));

    // Text color per line: position number always high-contrast,
    // alias slightly more muted. For Active (blue background) white so
    // the text stays readable.
    let (pos_color, alias_color) = match state {
        AntennaState::Active => (Color32::WHITE, Color32::from_rgb(220, 230, 245)),
        AntennaState::Blocked => (Color32::from_rgb(120, 120, 120), Color32::from_rgb(160, 160, 160)),
        AntennaState::Inactive => (Color32::from_rgb(20, 20, 30), Color32::from_rgb(90, 90, 100)),
    };

    // Render top line and bottom line centered
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

/// Pending rename state for the Amplitec panel: (position 1..=6,
/// edit buffer). `None` means: no dialog open. The dialog is
/// rendered by `render_amplitec_panel` at the end when the
/// state is Some - the context menu on an antenna button sets the state via
/// `open_rename_dialog`.
fn rename_state() -> &'static std::sync::Mutex<Option<(u8, String)>> {
    use std::sync::{Mutex, OnceLock};
    static STATE: OnceLock<Mutex<Option<(u8, String)>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(None))
}

fn open_rename_dialog(pos: u8, current: &str) {
    *rename_state().lock().unwrap() = Some((pos, current.to_string()));
}

/// Render the rename modal when the state is Some. The operator can enter a new
/// label or cancel. On OK, `config.amplitec_labels` is updated
/// via `modify_config` - an auto-restart is not needed
/// because labels are injected live in every render call.
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
            // Enter in the text field also commits
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
        // Update buffer state after user typing
        *state.lock().unwrap() = current;
    }
}

/// Renders the Amplitec panel. Returns `true` when the user has changed the
/// power-limit table (`max_w`/`tx_blocked`), so the caller
/// persists it in the server conf (modify_config) + the network loop picks it up.
pub(super) fn render_amplitec_panel(
    ui: &mut egui::Ui,
    amplitec: &AmplitecSwitch,
    status: &crate::amplitec::AmplitecStatus,
    labels: &[String; 6],
    max_w: &mut [Option<u16>; 6],
    tx_blocked: &mut [bool; 6],
    log_entries: &[(String, String)],
    show_log: &mut bool,
) -> bool {
    let mut power_changed = false;
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

    // Port A (TX+RX)
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

    // Port B (RX only)
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

    // Power-limit table (below Port B, just like in the client - but here
    // EDITABLE, because the config belongs to the Amplitec hardware on the server).
    ui.add_space(6.0);
    egui::CollapsingHeader::new(
        RichText::new(rust_i18n::t!("srv_power_cap_table").to_string()).strong(),
    )
    .default_open(false)
    .show(ui, |ui| {
        egui::Grid::new("srv_amplitec_power_grid")
            .striped(true)
            .min_col_width(40.0)
            .show(ui, |ui| {
                ui.label(RichText::new("Pos").strong());
                ui.label(RichText::new(rust_i18n::t!("srv_label").to_string()).strong());
                ui.label(RichText::new(rust_i18n::t!("srv_max_w").to_string()).strong());
                ui.label(RichText::new(rust_i18n::t!("srv_rx_only").to_string()).strong());
                ui.end_row();
                for i in 0..6 {
                    let pos = (i as u8) + 1;
                    ui.label(format!("A-{}", pos));
                    ui.label(&labels[i]);
                    // 0 W = no cap (None). Directly persistent on change.
                    let mut val = max_w[i].unwrap_or(0) as i32;
                    if ui.add(egui::DragValue::new(&mut val).range(0..=3000).suffix(" W").speed(1.0)).changed() {
                        max_w[i] = if val <= 0 { None } else { Some(val.clamp(0, 3000) as u16) };
                        power_changed = true;
                    }
                    if ui.checkbox(&mut tx_blocked[i], "").changed() {
                        power_changed = true;
                    }
                    ui.end_row();
                }
            });
        ui.label(
            RichText::new(rust_i18n::t!("srv_no_cap").to_string())
                .size(10.0)
                .color(Color32::from_rgb(160, 160, 160)),
        );
    });

    // Rename dialog (modal) - appears above the panel as long as
    // `rename_state()` is Some. Open via right-click on an
    // antenna button -> "Hernoem...".
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

    power_changed
}
