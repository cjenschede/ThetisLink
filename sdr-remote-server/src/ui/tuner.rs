use egui::{Color32, RichText};

use crate::tuner::{self, Jc4sTuner};

pub(super) fn render_tuner_panel(
    ui: &mut egui::Ui,
    tuner: &Jc4sTuner,
    status: &tuner::TunerStatus,
    show_log: &mut bool,
) {
    let amber = Color32::from_rgb(255, 170, 40);

    // Single compact row: title + status + buttons + Log checkbox
    ui.horizontal(|ui| {
        ui.label(RichText::new("JC-4s Tuner").strong());

        // Connection indicator
        if status.connected {
            ui.colored_label(Color32::GREEN, "[ON]");
        } else {
            ui.colored_label(Color32::RED, "[OFF]");
        }

        ui.separator();

        // Tune button
        let can_start = status.connected
            && (status.state == tuner::TUNER_IDLE || status.state == tuner::TUNER_DONE_OK);
        let (tune_color, tune_text) = match status.state {
            tuner::TUNER_TUNING => (Color32::from_rgb(60, 120, 220), "Tune..."),
            tuner::TUNER_DONE_OK if !status.stale => (Color32::from_rgb(50, 180, 50), "Tune OK"),
            tuner::TUNER_DONE_OK => (Color32::from_rgb(80, 80, 80), "Tune"), // stale: VFO moved
            tuner::TUNER_TIMEOUT => (amber, "Tune X"),
            tuner::TUNER_ABORTED => (amber, "Tune X"),
            _ => (Color32::from_rgb(80, 80, 80), "Tune"),
        };
        let btn = egui::Button::new(RichText::new(tune_text).color(Color32::WHITE).strong())
            .fill(tune_color);
        if ui.add_enabled(can_start, btn).clicked() {
            // Safe tune (PA standby/operate) is handled inside the tuner thread
            tuner.send_command(crate::tuner::TunerCmd::StartTune);
        }

        // Abort button
        let abort_enabled = status.state == tuner::TUNER_TUNING;
        if ui.add_enabled(abort_enabled, egui::Button::new("Abort")).clicked() {
            tuner.send_command(crate::tuner::TunerCmd::AbortTune);
        }

        // Status text
        match status.state {
            tuner::TUNER_TUNING => {
                ui.colored_label(Color32::from_rgb(60, 120, 220), "Tuning...");
            }
            tuner::TUNER_DONE_OK => {
                ui.colored_label(Color32::GREEN, "Done");
            }
            tuner::TUNER_TIMEOUT => {
                ui.colored_label(Color32::from_rgb(255, 80, 80), "Timeout");
            }
            tuner::TUNER_ABORTED => {
                ui.colored_label(amber, "Aborted");
            }
            _ => {}
        }

        // Log checkbox at right
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.checkbox(show_log, "Log");
        });
    });
}

pub(super) fn render_tuner_log(ui: &mut egui::Ui, log_entries: &[(String, String)], show_log: bool) {
    if show_log {
        ui.separator();
        egui::ScrollArea::vertical()
            .id_salt("tuner_log")
            .stick_to_bottom(true)
            .max_height(100.0)
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
