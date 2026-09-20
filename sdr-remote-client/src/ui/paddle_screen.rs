// SPDX-License-Identifier: GPL-2.0-or-later
//! The Morse paddle on the desktop: the per-frame event pump and its settings
//! section. Step 2 of `DESIGN-remote-cw.md`: read the paddle, run the keyer,
//! and write every key edge to the log. Nothing is transmitted yet - the
//! sidetone is step 3, the packet to the server step 4.
//!
//! The transport is `crate::paddle`; the rule is `sdr_remote_logic::cw_keyer`.
//! This file only joins the two to the screen and the log.

use super::*;
use crate::paddle::{PaddleEvent, PaddleStatus};
use sdr_remote_logic::cw_keyer::PaddleMode;
use sdr_remote_logic::sidetone;

impl SdrRemoteApp {
    fn cw_configured(&self) -> [bool; 3] {
        [self.thetis_configured, self.yaesu_configured, self.yaesu2_configured]
    }

    /// The transmitter the key goes to right now: the setting, unless the
    /// server does not have that radio - then the first one it does have.
    /// The same rule as the Bluetooth button (design section 5.2a).
    fn cw_effective_target(&self) -> u8 {
        sdr_remote_logic::ble_ptt::effective_target(self.cw_configured(), self.cw_target, self.radios_known)
    }

    /// Does the key go to Thetis right now? The main PTT button is Thetis's.
    pub(super) fn cw_target_is_thetis(&self) -> bool {
        self.paddle_enabled && self.cw_effective_target() == 0
    }

    /// Does the key go to this Yaesu slot (0 or 1) right now?
    pub(super) fn cw_target_is_yaesu(&self, slot: usize) -> bool {
        self.paddle_enabled && usize::from(self.cw_effective_target()) == slot + 1
    }

    fn cw_target_label(&self, t: u8) -> String {
        match t {
            0 => "Thetis".to_string(),
            t => self.yaesu_slot_label(t - 1),
        }
    }

    pub(super) fn process_paddle_events(&mut self) {
        // The engine keys whatever it was last told; tell it whenever the
        // effective target changes (the setting, or what the server has).
        let target = self.cw_effective_target();
        if self.cw_target_sent != Some(target) {
            let _ = self.cmd_tx.send(Command::SetCwTarget(target));
            self.cw_target_sent = Some(target);
            log::info!("CW key goes to {}", self.cw_target_label(target));
        }
        if self.cw_hang_sent != Some(self.cw_hang_ms) {
            // Every value while the field is dragged goes to the engine (it
            // only rides along in the next packet); the log line is written
            // where the value is saved, once per change, not per frame.
            if self.cw_hang_sent.is_none() {
                log::info!("CW hang time {} ms", self.cw_hang_ms);
            }
            let _ = self.cmd_tx.send(Command::SetCwHang(self.cw_hang_ms));
            self.cw_hang_sent = Some(self.cw_hang_ms);
        }
        // Measurement 4 of the design: how long after the edge the tone was
        // first rendered, and into how big a buffer. Every onset for the first
        // twenty, then one in a hundred, so the log shows the number without
        // drowning in it.
        let onsets = self.sidetone.onset_count();
        if onsets != self.sidetone_onsets_seen {
            let (onset_us, _frames) = self.sidetone.last_onset();
            let buffer_us = self.sidetone.last_buffer_us();
            if onsets <= 20 || onsets % 100 == 0 {
                let device_us = self.sidetone.output_latency_us();
                log::info!(
                    "CW sidetone: onset {:.1} ms after the edge, rendered into a {:.1} ms buffer, device says {} to the speaker (#{})",
                    onset_us as f32 / 1000.0,
                    buffer_us as f32 / 1000.0,
                    if device_us == 0 { "nothing about the way".to_string() } else { format!("{:.1} ms", device_us as f32 / 1000.0) },
                    onsets
                );
            }
            self.sidetone_onsets_seen = onsets;
        }
        while let Ok(ev) = self.paddle.event_rx.try_recv() {
            match ev {
                PaddleEvent::Opened => {
                    self.paddle.set_status(PaddleStatus::Reading);
                    // A fresh board is a fresh time base: the first edge after
                    // it has nothing to be measured from (field test of build
                    // 88 showed "+0 ms" against the previous session's clock).
                    self.paddle_last_edge_ms = None;
                    self.paddle_key_down = false;
                    let s = self.paddle.settings();
                    log::info!(
                        "CW paddle: MCP2221A opened, reading GP0 (A), GP1 (B), GP2 (straight key, {}); {:?}, {} wpm",
                        if s.straight_enabled { "on" } else { "off" },
                        s.mode,
                        s.wpm
                    );
                }
                PaddleEvent::Lost(why) => {
                    self.paddle.set_status(PaddleStatus::Looking);
                    log::warn!("CW paddle: no board ({}) - looking again every {} s", why, crate::paddle::RECONNECT_DELAY.as_secs());
                    self.paddle_key_down = false;
                }
                PaddleEvent::Edge { edge, seen_at_ms } => {
                    let since = crate::paddle::gap_since(self.paddle_last_edge_ms, edge.at_ms);
                    // The log line of working rule 11 for this step: one per edge,
                    // with the planned time, the length of the state it ends,
                    // and how far behind its planned time the reader saw it -
                    // the number that catches a reader that stretches elements,
                    // which the planned times alone cannot (review finding).
                    log::info!(
                        "CW key {} at {} ms{} late {} ms",
                        if edge.down { "down" } else { "up" },
                        edge.at_ms,
                        since.map(|d| format!(" (+{} ms)", d)).unwrap_or_default(),
                        seen_at_ms.saturating_sub(edge.at_ms)
                    );
                    self.paddle_last_edge_ms = Some(edge.at_ms);
                    self.paddle_key_down = edge.down;
                    self.paddle_last_edge_at = Some(std::time::Instant::now());
                    self.paddle_edges += 1;
                }
                PaddleEvent::Rate { reads, seconds, median_us, p95_us, max_us } => {
                    log::info!(
                        "CW paddle: {} reads in {:.1} s ({:.0}/s), read time median {:.2} ms, 95% {:.2} ms, max {:.2} ms",
                        reads,
                        seconds,
                        reads as f32 / seconds,
                        median_us as f32 / 1000.0,
                        p95_us as f32 / 1000.0,
                        max_us as f32 / 1000.0
                    );
                    self.paddle_rate = format!(
                        "{:.0}/s, {:.1} / {:.1} / {:.1} ms",
                        reads as f32 / seconds,
                        median_us as f32 / 1000.0,
                        p95_us as f32 / 1000.0,
                        max_us as f32 / 1000.0
                    );
                }
            }
        }
    }

    pub(super) fn set_paddle_enabled(&mut self, on: bool) {
        self.paddle_enabled = on;
        if on {
            self.paddle.start();
            log::info!("CW paddle: switched on");
        } else {
            self.paddle.stop();
            // Whatever the thread sent on its way out (a key lifted) is still
            // on the channel; drain it now so the log shows the release.
            self.process_paddle_events();
            self.paddle_key_down = false;
            self.paddle_rate.clear();
            log::info!("CW paddle: switched off");
        }
        super::config::save_cw_paddle_enabled(on);
    }

    pub(super) fn render_cw_section(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let mut on = self.paddle_enabled;
            if ui.checkbox(&mut on, rust_i18n::t!("screen_cw_paddle_hint").to_string()).changed() {
                self.set_paddle_enabled(on);
            }
        });
        if !self.paddle_enabled {
            return;
        }
        ui.horizontal(|ui| {
            let status = match self.paddle.status() {
                PaddleStatus::Off => String::new(),
                PaddleStatus::Looking => rust_i18n::t!("screen_cw_paddle_looking").to_string(),
                PaddleStatus::Reading => rust_i18n::t!("screen_cw_paddle_reading").to_string(),
            };
            ui.label(status);
            if self.paddle.status() == PaddleStatus::Reading {
                if self.paddle_key_down {
                    ui.label(RichText::new(rust_i18n::t!("screen_cw_key_down").to_string()).strong().color(Color32::from_rgb(220, 60, 60)));
                } else {
                    ui.label(RichText::new(rust_i18n::t!("screen_cw_key_up").to_string()).color(ui.visuals().weak_text_color()));
                }
                ui.label(
                    RichText::new(format!("{} {}", self.paddle_edges, rust_i18n::t!("screen_cw_edges")))
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
            }
        });
        if !self.paddle_rate.is_empty() {
            ui.label(RichText::new(&self.paddle_rate).small().color(ui.visuals().weak_text_color()));
        }
        // The keyer: speed, paddle mode, reversed paddles, the straight key.
        // Handed to the reader on change (it picks it up at its next read;
        // the element in flight keeps its length) and saved.
        let before = self.keyer_settings;
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("screen_cw_speed").to_string());
            ui.add(egui::DragValue::new(&mut self.keyer_settings.wpm).speed(0.2).range(4..=60).suffix(" wpm"))
                .on_hover_text(rust_i18n::t!("screen_cw_speed_tooltip").to_string());
            egui::ComboBox::from_id_source("cw_paddle_mode")
                .selected_text(match self.keyer_settings.mode {
                    PaddleMode::IambicA => "Iambic A",
                    PaddleMode::IambicB => "Iambic B",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.keyer_settings.mode, PaddleMode::IambicA, "Iambic A");
                    ui.selectable_value(&mut self.keyer_settings.mode, PaddleMode::IambicB, "Iambic B");
                })
                .response
                .on_hover_text(rust_i18n::t!("screen_cw_mode_tooltip").to_string());
            ui.checkbox(&mut self.keyer_settings.reversed, rust_i18n::t!("screen_cw_reversed").to_string())
                .on_hover_text(rust_i18n::t!("screen_cw_reversed_tooltip").to_string());
            ui.checkbox(&mut self.keyer_settings.straight_enabled, rust_i18n::t!("screen_cw_straight").to_string())
                .on_hover_text(rust_i18n::t!("screen_cw_straight_tooltip").to_string());
        });
        if self.keyer_settings != before {
            self.paddle.set_settings(self.keyer_settings);
            super::config::save_cw_keyer(&self.keyer_settings);
            log::info!(
                "CW keyer: {} wpm, {:?}, reversed {}, straight key {}",
                self.keyer_settings.wpm,
                self.keyer_settings.mode,
                self.keyer_settings.reversed,
                if self.keyer_settings.straight_enabled { "on" } else { "off" }
            );
        }
        // How long the transmitter is held after the last key-up. The server
        // keys MOX for a run (Thetis's own TCI keyer drops it at every key-up),
        // so this is the semi break-in delay, and it is ours to set.
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("screen_cw_hang").to_string());
            let resp = ui
                .add(egui::DragValue::new(&mut self.cw_hang_ms).speed(10.0).range(200..=3000).suffix(" ms"))
                .on_hover_text(rust_i18n::t!("screen_cw_hang_tooltip").to_string());
            // Saved and logged when the drag ends or a typed value is taken,
            // not on every frame of a drag (build 105 wrote 142 log lines and
            // as many config saves for one setting).
            if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
                super::config::save_cw_hang_ms(self.cw_hang_ms);
                log::info!("CW hang time {} ms", self.cw_hang_ms);
            }
        });
        // Which radio the key goes to - only when there is something to choose.
        if sdr_remote_logic::ble_ptt::target_needs_a_choice(self.cw_configured()) {
            ui.horizontal(|ui| {
                ui.label(rust_i18n::t!("screen_cw_target").to_string());
                let configured = self.cw_configured();
                let mut chosen = self.cw_effective_target();
                let before = chosen;
                for t in 0u8..3 {
                    if configured[t as usize] {
                        let label = self.cw_target_label(t);
                        ui.selectable_value(&mut chosen, t, label);
                    }
                }
                if chosen != before {
                    self.cw_target = chosen;
                    super::config::save_cw_target(chosen);
                }
            });
        }
        // The sidetone: on/off, pitch, level. Applied to the shared state at
        // once (the callback reads it on its next buffer) and saved.
        let before = self.sidetone_settings;
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.sidetone_settings.on, rust_i18n::t!("screen_cw_sidetone").to_string());
            ui.add(
                egui::DragValue::new(&mut self.sidetone_settings.hz)
                    .speed(10.0)
                    .range(sidetone::HZ_MIN..=sidetone::HZ_MAX)
                    .suffix(" Hz"),
            )
            .on_hover_text(rust_i18n::t!("screen_cw_sidetone_hz_tooltip").to_string());
            ui.label(rust_i18n::t!("screen_roger_volume").to_string());
            let resp = ui.add(egui::Slider::new(&mut self.sidetone_settings.volume, 0.0..=1.0).fixed_decimals(2));
            let _ = super::helpers::slider_wheel(ui, &resp, &mut self.sidetone_settings.volume, 0.0..=1.0, 0.05);
        });
        if self.sidetone_settings != before {
            self.sidetone_settings = self.sidetone_settings.clamped();
            self.sidetone.apply(self.sidetone_settings);
            super::config::save_cw_sidetone(&self.sidetone_settings);
        }
        ui.label(
            RichText::new(rust_i18n::t!("screen_cw_paddle_note").to_string())
                .small()
                .color(ui.visuals().weak_text_color()),
        );
    }
}
