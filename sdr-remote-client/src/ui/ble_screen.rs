// SPDX-License-Identifier: GPL-2.0-or-later
//! The Bluetooth PTT button on the desktop: the per-frame event pump and its
//! settings section.
//!
//! The button is one more latching control, next to MIDI. Which transmitter it
//! keys is a setting of the control - the same three choices a MIDI button has
//! (`Ptt`, `YaesuPtt`, `Radio2Ptt`) - decided with the owner on 2026-09-17: the
//! desktop picks the radio per control, not per client, and the CW key will do
//! the same (`DESIGN-remote-cw.md` §5.2a).
//!
//! The rule for what a press, a release and a lost link mean is
//! `sdr_remote_logic::ble_ptt`, shared with the phone. The transport is
//! `crate::ble`. This file only joins the two to the PTT decision.

use super::*;
use crate::ble::{BleEvent, BleStatus};
use sdr_remote_logic::ble_ptt::{BleButtonEvent, BlePttAction};
use sdr_remote_logic::ptt_intent::PttSource;

/// Which transmitter the button keys: 0 = Thetis, 1 = Yaesu slot 0, 2 = Yaesu
/// slot 1. The same numbering as `PttTarget` and the server's slots.
pub(super) const BLE_TARGET_THETIS: u8 = 0;

impl SdrRemoteApp {
    pub(super) fn process_ble_events(&mut self) {
        // A scan nobody is looking at any more stops on its own; the screen may
        // have been left while it was running.
        if self.ble.status() == BleStatus::Scanning
            && crate::ble::scan_expired(self.ble.scan_since(), std::time::Instant::now())
        {
            self.ble.scan_stop();
            self.ble_found.clear();
            self.ble_note = "scan stopped after a minute - choose again to look once more".into();
            log::info!("BLE PTT: scan stopped after {} s without a choice", crate::ble::SCAN_TIMEOUT.as_secs());
        }
        while let Ok(ev) = self.ble.event_rx.try_recv() {
            match ev {
                BleEvent::Found { addr, name, rssi, likely } => {
                    self.ble_found.retain(|f| f.0 != addr);
                    self.ble_found.push((addr, name, rssi, likely));
                    // The ones that say they are PTT buttons first, then by signal.
                    self.ble_found.sort_by(|a, b| b.3.cmp(&a.3).then(b.2.cmp(&a.2)));
                }
                BleEvent::Note(text) => {
                    log::info!("BLE PTT: {}", text);
                    self.ble_note = text;
                }
                BleEvent::Attached(generation) => {
                    if self.ble.generation() == generation {
                        self.ble.set_status(BleStatus::Connected);
                        log::info!("BLE PTT: connected to {} ({})", self.ble_name, crate::ble::format_addr(self.ble_address));
                    }
                }
                BleEvent::Failed(generation, why) => {
                    if self.ble.generation() == generation {
                        log::warn!("BLE PTT: could not connect to {}: {}", self.ble_name, why);
                        self.ble_note = why;
                        // Windows keeps trying on its own only once a session stood;
                        // a failed first attempt is over.
                        self.ble.set_status(BleStatus::Off);
                    }
                }
                BleEvent::Link(generation, event) => {
                    let was_held = self.ble_state.held();
                    if let Some(action) = self.ble_state.apply(generation, event) {
                        if event == BleButtonEvent::Disconnected {
                            // The line the Android brief demands of every drop: the last
                            // known state of the button, with a time on it. It is the
                            // only record the out-of-range case ever gets.
                            log::warn!("BLE PTT: link lost - button was {} (attempt {})",
                                if was_held { "HELD" } else { "not held" }, generation);
                            if self.ble_enabled {
                                self.ble.set_status(BleStatus::Waiting);
                            }
                        }
                        self.apply_ble_action(action);
                    }
                }
            }
        }
    }

    fn apply_ble_action(&mut self, action: BlePttAction) {
        match action {
            BlePttAction::KeyDown => self.ble_press(true),
            BlePttAction::KeyUp => self.ble_press(false),
            BlePttAction::Gone => self.ble_release_all(),
            BlePttAction::Unrecognised(b) => {
                log::warn!("BLE PTT: unknown byte 0x{:02X} from the button - ignored", b);
            }
        }
    }

    /// A press or release, to the transmitter this button is set to. Toggle or
    /// hold follows the button's own setting, like MIDI has its own.
    fn ble_press(&mut self, down: bool) {
        log::info!("BLE PTT: button {} -> {}", if down { "pressed" } else { "released" }, self.ble_target_label());
        // Since build 77 the three transmitters have the same shape, so one
        // arm does for all of them.
        let target = self.ble_effective_target();
        let toggle = self.ble_toggle;
        let (latches, resampled) = self.tx_latches(target);
        *latches = if toggle {
            if down {
                latches.latching_press(PttSource::Bluetooth, resampled)
            } else {
                *latches
            }
        } else {
            latches.hold(PttSource::Bluetooth, down, resampled)
        };
    }

    /// The latches of a transmitter in `PttTarget` order (0 Thetis, 1 and 2 the
    /// Yaesu slots), with the sources that transmitter re-reads every frame.
    fn tx_latches(&mut self, target: u8) -> (&mut sdr_remote_logic::ptt_intent::Latches, &'static [PttSource]) {
        if target == BLE_TARGET_THETIS {
            let r = self.thetis_resampled_sources();
            (&mut self.thetis_latches, r)
        } else {
            let slot = (target - 1).min(1);
            let r = self.resampled_sources(slot);
            (&mut self.yaesu_latches[slot as usize], r)
        }
    }

    /// The link is gone, or the button was switched off: whatever this button
    /// holds is released - and only that. A MIDI latch on the same radio stays.
    fn ble_release_all(&mut self) {
        for target in 0u8..3 {
            let (latches, resampled) = self.tx_latches(target);
            *latches = latches.hold(PttSource::Bluetooth, false, resampled);
        }
    }

    /// What the server has, in `PttTarget` order.
    fn ble_configured(&self) -> [bool; 3] {
        [self.thetis_configured, self.yaesu_configured, self.yaesu2_configured]
    }

    /// The transmitter this button keys right now: the setting, unless the
    /// server does not have that radio - then the first one it does have.
    fn ble_effective_target(&self) -> u8 {
        sdr_remote_logic::ble_ptt::effective_target(self.ble_configured(), self.ble_target, self.radios_known)
    }

    fn ble_target_label(&self) -> String {
        match self.ble_effective_target() {
            BLE_TARGET_THETIS => "Thetis".to_string(),
            t => self.yaesu_slot_label(t - 1),
        }
    }

    /// Switch the feature on or off. Off releases the transmitter and lets the
    /// button go; on connects the remembered button or starts looking for one.
    pub(super) fn set_ble_enabled(&mut self, on: bool) {
        self.ble_enabled = on;
        if on {
            if self.ble_address != 0 {
                self.ble_connect_remembered();
            } else {
                self.ble.scan_start();
            }
        } else {
            self.ble.disconnect();
            self.ble_release_all();
            self.ble_state = sdr_remote_logic::ble_ptt::BlePttState::new();
            self.ble_found.clear();
            log::info!("BLE PTT: switched off");
        }
        self.save_ptt_config();
    }

    pub(super) fn ble_connect_remembered(&mut self) {
        let name = self.ble_name.clone();
        let generation = self.ble.connect(self.ble_address, &name);
        if let Some(action) = self.ble_state.begin(generation) {
            self.apply_ble_action(action);
        }
    }

    pub(super) fn render_ble_section(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if !crate::ble::BleManager::available() {
                ui.label(rust_i18n::t!("screen_ble_ptt_unavailable").to_string());
                return;
            }
            let mut on = self.ble_enabled;
            if ui.checkbox(&mut on, rust_i18n::t!("screen_ble_ptt_hint").to_string()).changed() {
                self.set_ble_enabled(on);
            }
        });
        if !self.ble_enabled || !crate::ble::BleManager::available() {
            return;
        }

        // Status and the one action that fits it.
        ui.horizontal(|ui| {
            match self.ble.status() {
                BleStatus::Connected => {
                    ui.colored_label(Color32::GREEN, rust_i18n::t!("screen_ble_ptt_connected", name = self.ble_name.as_str()).to_string());
                    if ui.button(rust_i18n::t!("screen_ble_ptt_forget").to_string()).clicked() {
                        self.ble_forget();
                    }
                }
                BleStatus::Connecting => {
                    ui.label(rust_i18n::t!("screen_ble_ptt_connecting").to_string());
                }
                BleStatus::Waiting => {
                    ui.colored_label(Color32::YELLOW, rust_i18n::t!("screen_ble_ptt_waiting").to_string());
                    if ui.button(rust_i18n::t!("screen_ble_ptt_forget").to_string()).clicked() {
                        self.ble_forget();
                    }
                }
                BleStatus::Scanning => {
                    ui.label(rust_i18n::t!("screen_ble_ptt_scanning").to_string());
                    if ui.button(rust_i18n::t!("screen_disconnect").to_string()).clicked() {
                        self.ble.scan_stop();
                    }
                }
                BleStatus::Off => {
                    if self.ble_address != 0 {
                        ui.label(format!("{} ({})", self.ble_name, crate::ble::format_addr(self.ble_address)));
                        if ui.button(rust_i18n::t!("screen_ble_ptt_reconnect").to_string()).clicked() {
                            self.ble_connect_remembered();
                        }
                        if ui.button(rust_i18n::t!("screen_ble_ptt_forget").to_string()).clicked() {
                            self.ble_forget();
                        }
                    } else if ui.button(rust_i18n::t!("screen_ble_ptt_choose").to_string()).clicked() {
                        self.ble.scan_start();
                    }
                }
            }
        });
        if !self.ble_note.is_empty() {
            ui.label(RichText::new(&self.ble_note).size(11.0).weak());
        }

        // The scan list, while scanning.
        if self.ble.status() == BleStatus::Scanning {
            if self.ble_found.is_empty() {
                ui.label(rust_i18n::t!("screen_ble_ptt_none_yet").to_string());
            }
            let found = self.ble_found.clone();
            for (addr, name, rssi, likely) in found {
                ui.horizontal(|ui| {
                    let text = format!("{} {} dBm", name, rssi);
                    if likely {
                        ui.label(RichText::new(text).strong());
                    } else {
                        ui.label(text);
                    }
                    if ui.button(rust_i18n::t!("screen_ble_ptt_use").to_string()).clicked() {
                        self.ble_address = addr;
                        self.ble_name = name.clone();
                        self.ble_found.clear();
                        self.save_ptt_config();
                        self.ble_connect_remembered();
                    }
                });
            }
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        }

        // Which transmitter: the radios the server has configured, on or off.
        // With one radio there is nothing to choose and the row is not shown.
        let configured = self.ble_configured();
        if sdr_remote_logic::ble_ptt::target_needs_a_choice(configured) {
            ui.horizontal(|ui| {
                ui.label(rust_i18n::t!("screen_ble_ptt_target").to_string());
                let effective = self.ble_effective_target();
                let labels = [
                    "Thetis".to_string(),
                    self.yaesu_slot_label(0),
                    self.yaesu_slot_label(1),
                ];
                for id in 0u8..3 {
                    if !configured[id as usize] {
                        continue;
                    }
                    if ui.selectable_label(effective == id, &labels[id as usize]).clicked() && effective != id {
                        // Never carry a press across a change of target.
                        self.ble_release_all();
                        self.ble_target = id;
                        self.save_ptt_config();
                    }
                }
            });
        } else {
            ui.label(rust_i18n::t!("screen_ble_ptt_target_single", name = self.ble_target_label().as_str()).to_string());
        }
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("screen_ble_ptt_mode").to_string());
            if ui.selectable_label(!self.ble_toggle, rust_i18n::t!("screen_push_to_talk").to_string()).clicked() {
                self.ble_release_all();
                self.ble_toggle = false;
                self.save_ptt_config();
            }
            if ui.selectable_label(self.ble_toggle, rust_i18n::t!("screen_toggle").to_string()).clicked() {
                self.ble_release_all();
                self.ble_toggle = true;
                self.save_ptt_config();
            }
        });
    }

    fn ble_forget(&mut self) {
        self.ble.disconnect();
        self.ble_release_all();
        self.ble_state = sdr_remote_logic::ble_ptt::BlePttState::new();
        log::info!("BLE PTT: forgot {} ({})", self.ble_name, crate::ble::format_addr(self.ble_address));
        self.ble_address = 0;
        self.ble_name.clear();
        self.ble_note.clear();
        self.save_ptt_config();
    }
}
