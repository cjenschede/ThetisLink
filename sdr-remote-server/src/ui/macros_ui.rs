use std::sync::Arc;

use egui::{Color32, RichText};

use crate::macros::{self, MacroAction, MacroDef, MacroRunner, MacroSlots};
use crate::tuner::Jc4sTuner;

pub(super) fn render_macro_button(
    ui: &mut egui::Ui,
    index: usize,
    slots: &MacroSlots,
    runner_status: &macros::MacroRunnerStatus,
    runner: &MacroRunner,
    cat_tx: &Option<tokio::sync::mpsc::Sender<String>>,
    tuner: &Option<Arc<Jc4sTuner>>,
) {
    let name = macros::slot_name(index);
    let slot = &slots[index];
    let is_active = runner_status.running && runner_status.active_slot == index;
    let is_configured = slot.is_some();
    let enabled = is_configured && !runner_status.running;

    let label = slot.as_ref().map(|d| d.label.as_str()).unwrap_or(&name);
    let display = if label.is_empty() { &name } else { label };

    let btn = if is_active {
        egui::Button::new(RichText::new(display).strong().size(11.0))
            .fill(Color32::from_rgb(255, 170, 40))
            .min_size(egui::vec2(50.0, 22.0))
    } else if is_configured {
        egui::Button::new(RichText::new(display).size(11.0))
            .min_size(egui::vec2(50.0, 22.0))
    } else {
        egui::Button::new(RichText::new(display).size(11.0).color(Color32::GRAY))
            .min_size(egui::vec2(50.0, 22.0))
    };

    let resp = ui.add_enabled(enabled, btn);
    if resp.clicked() {
        if let Some(ref def) = slot {
            if let Some(ref tx) = cat_tx {
                runner.run(index, def.clone(), tx.clone(), tuner.clone());
            }
        }
    }
    if let Some(ref def) = slot {
        let summary = macros::actions_summary(&def.actions);
        resp.on_hover_text(format!("{}\n{}", name, summary));
    }
}

pub(super) fn load_slot_into_editor(
    slots: &MacroSlots,
    index: usize,
    label: &mut String,
    actions: &mut Vec<MacroAction>,
) {
    if let Some(ref def) = slots[index] {
        *label = def.label.clone();
        *actions = def.actions.clone();
    } else {
        label.clear();
        actions.clear();
    }
}

pub(super) fn render_macro_editor(
    ui: &mut egui::Ui,
    slots: &mut MacroSlots,
    editor_slot: &mut usize,
    editor_label: &mut String,
    editor_actions: &mut Vec<MacroAction>,
    show: &mut bool,
) {
    ui.heading("Macro Editor");
    ui.separator();

    ui.columns(2, |cols| {
        // Left column: slot list
        egui::ScrollArea::vertical()
            .id_salt("macro_slot_list")
            .max_height(400.0)
            .show(&mut cols[0], |ui| {
                for i in 0..macros::NUM_MACRO_SLOTS {
                    let name = macros::slot_name(i);
                    let configured = slots[i].is_some();
                    let label_text = if let Some(ref def) = slots[i] {
                        if def.label.is_empty() {
                            name.clone()
                        } else {
                            format!("{} - {}", name, def.label)
                        }
                    } else {
                        format!("{} (empty)", name)
                    };

                    let text = if configured {
                        RichText::new(&label_text).strong()
                    } else {
                        RichText::new(&label_text).color(Color32::GRAY)
                    };

                    if ui.selectable_label(*editor_slot == i, text).clicked() {
                        // Save current slot before switching
                        save_editor_to_slot(slots, *editor_slot, editor_label, editor_actions);
                        *editor_slot = i;
                        load_slot_into_editor(slots, i, editor_label, editor_actions);
                    }
                }
            });

        // Right column: editor for selected slot
        let slot_name = macros::slot_name(*editor_slot);
        cols[1].label(RichText::new(format!("Slot: {}", slot_name)).strong());
        cols[1].add_space(4.0);

        cols[1].label("Label:");
        cols[1].text_edit_singleline(editor_label);
        cols[1].add_space(8.0);

        cols[1].label(RichText::new("Acties:").strong());

        let mut remove_idx = None;
        for (i, action) in editor_actions.iter_mut().enumerate() {
            cols[1].horizontal(|ui| {
                ui.label(format!("{}.", i + 1));

                let mut action_type = match action {
                    MacroAction::Cat(_) => 0,
                    MacroAction::Delay(_) => 1,
                    MacroAction::Tune => 2,
                };
                let prev_type = action_type;

                egui::ComboBox::from_id_salt(format!("action_type_{}", i))
                    .width(70.0)
                    .selected_text(match action_type {
                        0 => "CAT",
                        1 => "Delay",
                        _ => "Tune",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut action_type, 0, "CAT");
                        ui.selectable_value(&mut action_type, 1, "Delay");
                        ui.selectable_value(&mut action_type, 2, "Tune");
                    });

                if action_type != prev_type {
                    *action = match action_type {
                        0 => MacroAction::Cat(String::new()),
                        1 => MacroAction::Delay(200),
                        _ => MacroAction::Tune,
                    };
                }

                match action {
                    MacroAction::Cat(ref mut cmd) => {
                        let resp = ui.add(egui::TextEdit::singleline(cmd).desired_width(180.0));
                        if resp.changed() && !cmd.ends_with(';') && !cmd.is_empty() {
                            // Don't auto-add semicolon while typing
                        }
                    }
                    MacroAction::Delay(ref mut ms) => {
                        let mut ms_str = ms.to_string();
                        if ui.add(egui::TextEdit::singleline(&mut ms_str).desired_width(80.0)).changed() {
                            *ms = ms_str.parse().unwrap_or(200);
                        }
                        ui.label("ms");
                    }
                    MacroAction::Tune => {
                        ui.label("(JC-4s tune)");
                    }
                }

                if ui.small_button("\u{00D7}").clicked() {
                    remove_idx = Some(i);
                }
            });
        }

        if let Some(idx) = remove_idx {
            editor_actions.remove(idx);
        }

        cols[1].add_space(4.0);
        if cols[1].button("+ Actie").clicked() {
            editor_actions.push(MacroAction::Cat(String::new()));
        }

        if cols[1].button("Wis macro").clicked() {
            editor_label.clear();
            editor_actions.clear();
        }
    });

    ui.separator();
    ui.horizontal(|ui| {
        if ui.button("Save").clicked() {
            save_editor_to_slot(slots, *editor_slot, editor_label, editor_actions);
            macros::save(slots);
            *show = false;
        }
        if ui.button("Cancel").clicked() {
            *show = false;
        }
    });
}

pub(super) fn save_editor_to_slot(
    slots: &mut MacroSlots,
    index: usize,
    label: &str,
    actions: &[MacroAction],
) {
    if label.is_empty() && actions.is_empty() {
        slots[index] = None;
    } else {
        slots[index] = Some(MacroDef {
            label: label.to_string(),
            actions: actions.to_vec(),
        });
    }
}
