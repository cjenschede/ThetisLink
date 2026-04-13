use egui::{Color32, RichText};

use crate::amplitec::AmplitecSwitch;

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
                ui.colored_label(Color32::GREEN, "Online");
            } else {
                ui.colored_label(Color32::RED, "Offline");
            }
            ui.checkbox(show_log, "Log");
        });
    });
    ui.separator();

    // Poort A (TX+RX)
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Poort A \u{2014} TX+RX").strong());
        if status.switch_a > 0 {
            let label = &labels[(status.switch_a - 1).min(5) as usize];
            ui.label(format!("  Huidige: {}", label));
        }
    });
    ui.horizontal(|ui| {
        for pos in 1..=6u8 {
            let is_active = status.switch_a == pos;
            let is_blocked = status.switch_b == pos;
            let label = &labels[(pos - 1) as usize];
            let btn = if is_active {
                egui::Button::new(RichText::new(format!(" {} ", label)).strong())
                    .fill(Color32::from_rgb(100, 160, 230))
            } else if is_blocked {
                egui::Button::new(RichText::new(format!(" {} ", label)).color(Color32::from_rgb(140, 140, 140)))
            } else {
                egui::Button::new(format!(" {} ", label))
            };
            let resp = ui.add_enabled(status.connected, btn);
            if resp.clicked() {
                amplitec.send_command(crate::amplitec::AmplitecCmd::SetSwitchA(pos));
            }
            if is_blocked {
                resp.on_hover_text(format!("{} — bezet door Poort B", label));
            }
        }
    });

    ui.add_space(8.0);

    // Poort B (RX only)
    ui.horizontal(|ui| {
        ui.label(RichText::new("Poort B \u{2014} RX").strong());
        if status.switch_b > 0 {
            let label = &labels[(status.switch_b - 1).min(5) as usize];
            ui.label(format!("  Huidige: {}", label));
        }
    });
    ui.horizontal(|ui| {
        for pos in 1..=6u8 {
            let is_active = status.switch_b == pos;
            let is_blocked = status.switch_a == pos;
            let label = &labels[(pos - 1) as usize];
            let btn = if is_active {
                egui::Button::new(RichText::new(format!(" {} ", label)).strong())
                    .fill(Color32::from_rgb(100, 160, 230))
            } else if is_blocked {
                egui::Button::new(RichText::new(format!(" {} ", label)).color(Color32::from_rgb(140, 140, 140)))
            } else {
                egui::Button::new(format!(" {} ", label))
            };
            let resp = ui.add_enabled(status.connected, btn);
            if resp.clicked() {
                amplitec.send_command(crate::amplitec::AmplitecCmd::SetSwitchB(pos));
            }
            if is_blocked {
                resp.on_hover_text(format!("{} — bezet door Poort A", label));
            }
        }
    });

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
