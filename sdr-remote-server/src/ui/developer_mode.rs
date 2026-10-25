// SPDX-License-Identifier: GPL-2.0-or-later
//
//! The developer mode, in the server GUI (build 294; owner decision
//! 2026-10-23). What the mode is - the code, the log target, the logger's
//! rule - is `sdr_remote_core::dev_mode`; this file is the server's switch and
//! its sign. The switch is stored with the settings (`developer_mode`), and
//! acts on the log at once.

use eframe::egui;
use egui::{Color32, RichText};

use super::ServerApp;

/// The colour of the sign: the server's amber for "not the ordinary state".
const DEV_COLOUR: Color32 = Color32::from_rgb(220, 160, 0);

/// The sign beside the title while the mode is on, so a screenshot or a
/// glance says in which mode the server runs.
pub(super) fn dev_badge(ui: &mut egui::Ui) {
    if sdr_remote_core::dev_mode::is_on() {
        ui.label(RichText::new("DEV").color(DEV_COLOUR).strong());
    }
}

impl ServerApp {
    /// In the settings screen: the code field while the mode is off; the sign,
    /// a button to close it and the developer's tools while it is on.
    pub(super) fn render_developer_mode(&mut self, ui: &mut egui::Ui) {
        if self.developer_mode {
            ui.horizontal(|ui| {
                ui.label(RichText::new("DEV").color(DEV_COLOUR).strong());
                ui.label(rust_i18n::t!("srv_dev_on").to_string());
                if ui.small_button(rust_i18n::t!("srv_dev_off").to_string()).clicked() {
                    self.switch_developer_mode(false);
                }
            });
            // Read when the server starts (build 230), and only in this mode
            // (`ServerConfig::measuring`).
            ui.checkbox(&mut self.tx_measurement, rust_i18n::t!("srv_tx_measurement").to_string())
                .on_hover_text(rust_i18n::t!("srv_tx_measurement_hover").to_string());
        } else {
            ui.horizontal(|ui| {
                ui.label(RichText::new(rust_i18n::t!("srv_dev_code").to_string()).small().weak());
                let field = ui.add(
                    egui::TextEdit::singleline(&mut self.dev_code)
                        .password(true)
                        .desired_width(90.0),
                );
                if field.changed() && sdr_remote_core::dev_mode::code_matches(&self.dev_code) {
                    self.dev_code.clear();
                    self.switch_developer_mode(true);
                }
            });
        }
    }

    /// The one way the screen switches the mode: the field kept for saving,
    /// the process's switch for the log now, and a line that says so.
    fn switch_developer_mode(&mut self, on: bool) {
        self.developer_mode = on;
        sdr_remote_core::dev_mode::set(on);
        // Named in advance (`logregels-build294.md`).
        log::info!("{}", sdr_remote_core::dev_mode::state_line(on));
        // Outside the mode every over gets the lead of every user (build 296):
        // a choice made in the mode does not outlive it.
        if !on {
            if let Some(shared) = self.status_panel_state.as_ref() {
                for radio in shared.radios.get().into_iter().flatten().flatten() {
                    radio.status_arc().lock().unwrap().tx_timing.set_lead(crate::yaesu::tx_timing::LEAD_MS, false);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// Both loggers apply the mode's one rule before they write anything
    /// (build 294): the GUI's before its fan-out to stderr, screen and file,
    /// the command line's around env_logger. The loggers run in no test.
    #[test]
    fn both_server_loggers_hold_back_the_developer_lines() {
        let main = include_str!("../main.rs");
        let gui = &main[main.find("impl Log for GuiLogger {").expect("the GUI logger")..];
        let gui = &gui[..gui.find("fn flush").unwrap()];
        let gate = gui.find("self.inner.matches(record) && sdr_remote_core::dev_mode::record_passes(record.target())")
            .expect("the GUI logger checks the mode");
        assert!(gate < gui.find("self.inner.log(record)").unwrap() && gate < gui.find("writeln!").unwrap(), "before the fan-out");
        let cli = &main[main.find("impl Log for DevGate {").expect("the command-line logger")..];
        let cli = &cli[..cli.find("fn flush").unwrap()];
        assert!(cli.contains("if sdr_remote_core::dev_mode::record_passes(record.target()) {\n            self.0.log(record);"));
        assert!(main.contains("log::set_boxed_logger(Box::new(DevGate(env_log))).unwrap();"), "the command line uses it");
        assert_eq!(main.matches("set_boxed_logger(").count(), 2, "and no third logger");
        // The saved mode reaches the switch as the GUI opens, and the start line
        // says what the switch holds, not what the settings say (review of 294:
        // without the first the log said "on" and held every developer line back).
        assert_eq!(main.matches("let config = config::load();\n        // The developer mode as saved (build 294), before anything is logged\n        // that it holds back.\n        sdr_remote_core::dev_mode::set(config.developer_mode);").count(), 1);
        assert_eq!(main.matches("dev_mode::set(").count(), 1);
        assert_eq!(main.matches("state_line(sdr_remote_core::dev_mode::is_on())").count(), 1);
        assert_eq!(main.matches("state_line(").count(), 1);
    }

    /// Closing the mode puts every radio back on the lead of every user
    /// (build 296): a choice from the mode's list does not outlive it.
    #[test]
    fn closing_the_mode_puts_the_lead_back() {
        let src = include_str!("developer_mode.rs");
        let switch = &src[src.find("fn switch_developer_mode(&mut self, on: bool) {").unwrap()..];
        let switch = &switch[..switch.find("\n    }\n").unwrap()];
        assert!(switch.contains("        if !on {\n            if let Some(shared) = self.status_panel_state.as_ref() {\n                for radio in shared.radios.get().into_iter().flatten().flatten() {\n                    radio.status_arc().lock().unwrap().tx_timing.set_lead(crate::yaesu::tx_timing::LEAD_MS, false);"));
    }

    /// The measurement box is read through `ServerConfig::measuring` only
    /// (box and mode), never alone: a conf ticked from before the mode starts
    /// nothing (build 294). The fields that carry the box to and from the
    /// settings screen are the only other places that name it.
    #[test]
    fn the_measurement_box_is_read_through_measuring_only() {
        for (file, src) in [
            ("main.rs", include_str!("../main.rs")),
            ("network.rs", include_str!("../network.rs")),
            ("status_panel.rs", include_str!("status_panel.rs")),
            ("tx_timing.rs", include_str!("../yaesu/tx_timing.rs")),
        ] {
            let code = &src[..src.find("#[cfg(test)]\nmod tests").unwrap_or(src.len())];
            let named = code.matches("tx_measurement").count();
            let allowed = code.matches("tx_measurement: false,").count();
            assert_eq!(named, allowed, "{file} reads the box without the mode - use measuring()");
        }
    }
}
