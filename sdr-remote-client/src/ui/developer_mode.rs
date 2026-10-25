// SPDX-License-Identifier: GPL-2.0-or-later
//
//! The developer mode, on the desktop's server screen (build 295; owner
//! decision 2026-10-23). What the mode is - the code, the log target, the
//! logger's rule - is `sdr_remote_core::dev_mode`; this file is the desktop's
//! switch and its sign. The server has its own (`sdr-remote-server`,
//! `ui/developer_mode.rs`), deliberately: another program with its own
//! settings and its own tools behind the mode, sharing the core's rule.

use eframe::egui;
use egui::{Color32, RichText};
use sdr_remote_logic::commands::Command;

use super::SdrRemoteApp;

/// The colour of the sign: the same amber as on the server. Also the sign in
/// the About window.
pub(super) const DEV_COLOUR: Color32 = Color32::from_rgb(220, 160, 0);

impl SdrRemoteApp {
    /// The code field while the mode is off; the sign and a button to close it
    /// while it is on. The tools themselves are drawn by the caller, only in
    /// the mode.
    pub(super) fn render_developer_mode(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if self.developer_mode {
                ui.label(RichText::new("DEV").color(DEV_COLOUR).strong());
                ui.label(rust_i18n::t!("screen_dev_on").to_string());
                if ui.small_button(rust_i18n::t!("screen_dev_off").to_string()).clicked() {
                    self.switch_developer_mode(false);
                }
            } else {
                ui.label(RichText::new(rust_i18n::t!("screen_dev_code").to_string()).small().weak());
                let field = ui.add(
                    egui::TextEdit::singleline(&mut self.dev_code)
                        .password(true)
                        .desired_width(90.0),
                );
                if field.changed() && sdr_remote_core::dev_mode::code_matches(&self.dev_code) {
                    self.dev_code.clear();
                    self.switch_developer_mode(true);
                }
            }
        });
    }

    /// The one way the screen switches the mode: the field and the file, the
    /// process's switch for the log now, and a line that says so. Closing it
    /// also ends what only the mode can start, so nothing keeps running out of
    /// sight: the measurement file, "relay only" and "force TCP".
    fn switch_developer_mode(&mut self, on: bool) {
        self.developer_mode = on;
        crate::ui::config::save_developer_mode(on);
        sdr_remote_core::dev_mode::set(on);
        // Named in advance (`logregels-build295.md`).
        log::info!("{}", sdr_remote_core::dev_mode::state_line(on));
        if !on {
            let (relay_only, force_tcp) = {
                let st = self.state_rx.borrow();
                (st.relay_only, st.force_tcp)
            };
            if self.measurement_file.is_some() {
                let _ = self.cmd_tx.send(Command::SetMeasurement(None));
            }
            if relay_only {
                let _ = self.cmd_tx.send(Command::SetRelayOnly(false));
            }
            if force_tcp {
                let _ = self.cmd_tx.send(Command::SetForceTcp(false));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// The desktop's logger applies the mode's one rule before a line reaches
    /// the screen or the file, and the saved mode reaches the switch before the
    /// first line (build 295). The logger runs in no test.
    #[test]
    fn the_desktop_logger_holds_back_the_developer_lines() {
        let main = include_str!("../main.rs");
        assert_eq!(
            main.matches("metadata.level() <= log::Level::Info && sdr_remote_core::dev_mode::record_passes(metadata.target())").count(),
            1
        );
        let logger = &main[main.find("fn log(&self, record: &log::Record) {").unwrap()..];
        assert!(logger.starts_with("fn log(&self, record: &log::Record) {\n        if !self.enabled(record.metadata()) {\n            return;\n        }"));
        let set = main.find("sdr_remote_core::dev_mode::set(ui::config::load_config().developer_mode);").expect("the saved mode");
        assert!(set < main.find("info!(").unwrap(), "before the first line");
        assert_eq!(main.matches("state_line(sdr_remote_core::dev_mode::is_on())").count(), 1);
        assert_eq!(main.matches("set_boxed_logger(").count(), 1, "and no second logger");
    }

    /// The tools that only measure are drawn only in the mode: the measurement
    /// file, "relay only" and "force TCP" (build 295).
    #[test]
    fn the_measuring_tools_are_drawn_only_in_the_developer_mode() {
        let src = include_str!("server_screen.rs");
        let gate = src.find("                self.render_developer_mode(ui);\n                if self.developer_mode {\n")
            .expect("the tools behind the mode");
        // All of it in one group behind its own chevron (owner, 2026-10-24).
        let group = src.find("super::config::Fold::Developer,").expect("the developer group");
        assert!(group < gate, "the code field and the tools inside the group");
        assert_eq!(src.matches("self.render_developer_mode(ui);").count(), 1);
        for tool in ["screen_measure_start", "screen_relay_only\"", "screen_force_tcp\"", "Command::SetMeasurement(", "Command::SetRelayOnly(", "Command::SetForceTcp("] {
            // The first one after the gate: then every one is.
            let at = src.find(tool).unwrap_or_else(|| panic!("{tool} is drawn"));
            assert!(at > gate, "{tool} before the gate");
        }
    }

    /// What closing the mode does, all of it: the switch the logger reads, and
    /// the three tools that would otherwise run on out of sight (review of 295:
    /// each of the four could go and every test stayed green).
    #[test]
    fn closing_the_mode_ends_what_only_the_mode_starts() {
        let src = include_str!("developer_mode.rs");
        let switch = &src[src.find("fn switch_developer_mode(&mut self, on: bool) {").unwrap()..];
        let switch = &switch[..switch.find("\n    }\n").unwrap()];
        for step in [
            "crate::ui::config::save_developer_mode(on);",
            "sdr_remote_core::dev_mode::set(on);",
            "if self.measurement_file.is_some() {\n                let _ = self.cmd_tx.send(Command::SetMeasurement(None));",
            "if relay_only {\n                let _ = self.cmd_tx.send(Command::SetRelayOnly(false));",
            "if force_tcp {\n                let _ = self.cmd_tx.send(Command::SetForceTcp(false));",
        ] {
            assert_eq!(switch.matches(step).count(), 1, "{step}");
        }
        assert!(switch.contains("        if !on {"), "the three only when it closes");
    }

    /// The setting is kept by the file's full rewrite, and filed under its own
    /// heading (build 295).
    #[test]
    fn the_developer_mode_is_kept_and_read_back() {
        assert!(crate::ui::config::survives_full_save("developer_mode=true"));
        let config = include_str!("config.rs");
        assert!(config.contains("save_single_key(\"developer_mode\", if on { \"true\" } else { \"false\" });"));
        assert!(config.contains("config.developer_mode = val.trim() == \"true\";"));
        assert!(config.contains("(\"Development\", &[\"developer_mode\"]),"));
    }
}
