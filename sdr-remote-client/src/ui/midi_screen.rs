// SPDX-License-Identifier: GPL-2.0-or-later
//! MIDI subsystem UI: the per-frame MIDI event pump and the MIDI settings screen
//! (learn/map, action pickers, band-step + VRX-tune actions, volume curve).
//! Extracted verbatim from `ui/screens.rs` - pure relocation, no behaviour change.
//! `pub(super)` keeps the methods callable from the parent module tree.

use super::*;
use crate::ui::controls::frequency::step_on_grid;

impl SdrRemoteApp {
    pub(super) fn process_midi_events(&mut self) {
        use crate::midi::{MidiEvent, MidiAction};
        use sdr_remote_core::protocol::ControlId;

        let freq_steps: &[u64] = &[10, 100, 500, 1_000, 10_000];

        while let Ok(event) = self.midi.event_rx.try_recv() {
            match event {
                MidiEvent::Learn(is_note, channel, number, value) => {
                    self.midi_last_event = format!(
                        "{} ch{} #{} val={}",
                        if is_note { "Note" } else { "CC" },
                        channel + 1, number, value,
                    );
                    // If learning, create the mapping
                    if self.midi_learn_for.is_some() {
                        let mapping = crate::midi::MidiMapping {
                            is_note,
                            channel,
                            number,
                            control_type: if is_note {
                                crate::midi::ControlType::Button
                            } else {
                                // Auto-detect: if action is encoder-type, default to Encoder
                                match self.midi_learn_action {
                                    MidiAction::VfoATune | MidiAction::VfoBTune
                                    | MidiAction::Vrx1Tune | MidiAction::Vrx2Tune
                                    | MidiAction::Radio1Tune | MidiAction::Radio2Tune
                                    | MidiAction::NrLevel => crate::midi::ControlType::Encoder,
                                    MidiAction::MasterVolume | MidiAction::VfoAVolume
                                    | MidiAction::VfoBVolume | MidiAction::TxGain
                                    | MidiAction::Drive | MidiAction::AgcGain
                                    | MidiAction::SqlLevel | MidiAction::CwSpeed
                                    | MidiAction::TuneDrive | MidiAction::MonVolume
                                    | MidiAction::RxBalance | MidiAction::RitOffset
                                    | MidiAction::XitOffset | MidiAction::YaesuVolume
                                    | MidiAction::Radio2Volume | MidiAction::Vrx1Volume
                                    | MidiAction::Vrx2Volume | MidiAction::YaesuRfGain
                                    | MidiAction::YaesuMicGain
                                    | MidiAction::SpectrumZoom | MidiAction::SpectrumPan
                                    | MidiAction::RefLevel | MidiAction::WaterfallContrast
                                    | MidiAction::Rx2SpectrumZoom | MidiAction::Rx2SpectrumPan
                                    | MidiAction::Rx2RefLevel | MidiAction::Rx2WaterfallContrast
                                        => crate::midi::ControlType::Slider,
                                    _ => crate::midi::ControlType::Button,
                                }
                            },
                            action: self.midi_learn_action,
                        };
                        self.midi.add_mapping(mapping);
                        self.midi_learn_for = None;
                        self.midi.set_learn_mode(false);
                        self.save_full_config();
                    }
                }
                MidiEvent::Button(action, velocity) => {
                    self.midi_last_event = format!("{} {}", action.label(), if velocity > 0 { "ON" } else { "OFF" });
                    let pressed = velocity > 0;
                    match action {
                        MidiAction::Ptt => {
                            if self.midi_ptt_toggle_mode {
                                // Toggle: press to switch on/off (ignore release)
                                if pressed { self.midi_ptt = !self.midi_ptt; }
                            } else {
                                // Momentary: press=TX, release=RX
                                self.midi_ptt = pressed;
                            }
                        }
                        MidiAction::ModeCycle if pressed => {
                            let modes: &[u8] = &[0, 1, 3, 4, 7, 9, 6, 5]; // LSB USB CWL CWU DIGU DIGL AM FM
                            if let Some(idx) = modes.iter().position(|&m| m == self.mode) {
                                let next = modes[(idx + 1) % modes.len()];
                                let _ = self.cmd_tx.send(Command::SetMode(next));
                            }
                        }
                        MidiAction::BandUp if pressed => {
                            self.midi_band_step(1);
                        }
                        MidiAction::BandDown if pressed => {
                            self.midi_band_step(-1);
                        }
                        MidiAction::Rx2BandUp if pressed => {
                            self.midi_band_step_for(Vfo::B, 1);
                        }
                        MidiAction::Rx2BandDown if pressed => {
                            self.midi_band_step_for(Vfo::B, -1);
                        }
                        MidiAction::Radio1BandUp if pressed => {
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuButton, 5));
                        }
                        MidiAction::Radio1BandDown if pressed => {
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuButton, 6));
                        }
                        MidiAction::Radio2BandUp if pressed => {
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button, 5));
                        }
                        MidiAction::Radio2BandDown if pressed => {
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button, 6));
                        }
                        MidiAction::NrToggle if pressed => {
                            let new_nr = if self.nr_level > 0 { 0 } else { 2 };
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::NoiseReduction, new_nr as u16));
                        }
                        MidiAction::AnfToggle if pressed => {
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::AutoNotchFilter, if self.anf_on { 0 } else { 1 }));
                        }
                        MidiAction::Rx2Toggle if pressed => {
                            self.rx2_enabled = !self.rx2_enabled;
                            self.rx2_enabled_pending = Some((Instant::now(), self.rx2_enabled));
                            let _ = self.cmd_tx.send(Command::SetRx2Enabled(self.rx2_enabled));
                        }
                        MidiAction::VfoSwap if pressed => {
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::VfoSwap, 2));
                        }
                        MidiAction::PowerToggle if pressed => {
                            let val = if self.power_on { 0 } else { 1 };
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::PowerOnOff, val));
                        }
                        MidiAction::MicAgcToggle if pressed => {
                            let new_val = !self.agc_enabled;
                            let _ = self.cmd_tx.send(Command::SetAgcEnabled(new_val));
                            self.agc_enabled = new_val;
                        }
                        MidiAction::FreqStepUp if pressed => {
                            if self.freq_step_index < freq_steps.len() - 1 {
                                self.freq_step_index += 1;
                            }
                        }
                        MidiAction::FreqStepDown if pressed => {
                            if self.freq_step_index > 0 {
                                self.freq_step_index -= 1;
                            }
                        }
                        MidiAction::FilterWiden if pressed => {
                            // Widen filter: decrease low, increase high by 50 Hz
                            let new_low = self.filter_low_hz - 50;
                            let new_high = self.filter_high_hz + 50;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::FilterLow, new_low as i16 as u16));
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::FilterHigh, new_high as i16 as u16));
                        }
                        MidiAction::FilterNarrow if pressed => {
                            // Narrow filter: increase low, decrease high by 50 Hz
                            let new_low = self.filter_low_hz + 50;
                            let new_high = self.filter_high_hz - 50;
                            if new_high > new_low {
                                let _ = self.cmd_tx.send(Command::SetControl(ControlId::FilterLow, new_low as i16 as u16));
                                let _ = self.cmd_tx.send(Command::SetControl(ControlId::FilterHigh, new_high as i16 as u16));
                            }
                        }
                        MidiAction::NrLevel if pressed => {
                            // Cycle NR level: 0 -> 1 -> 2 -> 3 -> 4 -> 0
                            let next = (self.nr_level + 1) % 5;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::NoiseReduction, next as u16));
                        }
                        MidiAction::AgcMode if pressed => {
                            // Cycle AGC: 0=Off,1=Long,2=Slow,3=Med,4=Fast
                            let next = (self.agc_mode + 1) % 5;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::AgcMode, next as u16));
                        }
                        MidiAction::NbToggle if pressed => {
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::NoiseBlanker, if self.nb_enable { 0 } else { 1 }));
                        }
                        MidiAction::ApfToggle if pressed => {
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::ApfEnable, if self.apf_enable { 0 } else { 1 }));
                        }
                        MidiAction::VfoLock if pressed => {
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::VfoLock, if self.vfo_lock { 0 } else { 1 }));
                        }
                        MidiAction::RitToggle if pressed => {
                            self.rit_enable = !self.rit_enable;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::RitEnable, self.rit_enable as u16));
                        }
                        MidiAction::XitToggle if pressed => {
                            self.xit_enable = !self.xit_enable;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::XitEnable, self.xit_enable as u16));
                        }
                        MidiAction::SqlToggle if pressed => {
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::SqlEnable, if self.sql_enable { 0 } else { 1 }));
                        }
                        MidiAction::TuneToggle if pressed => {
                            // Toggle tune - send 1 to activate, server handles state
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::ThetisTune, 1));
                        }
                        MidiAction::MuteAll if pressed => {
                            self.mute = !self.mute;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::Mute, self.mute as u16));
                        }
                        MidiAction::Rx1Mute if pressed => {
                            self.rx_mute = !self.rx_mute;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::RxMute, self.rx_mute as u16));
                        }
                        MidiAction::YaesuPtt => {
                            if self.midi_ptt_toggle_mode {
                                if pressed {
                                    let new_tx = !self.yaesu_tx_active;
                                    let _ = self.cmd_tx.send(Command::SetYaesuPtt(new_tx));
                                    self.midi.send_led(MidiAction::YaesuPtt, new_tx);
                                }
                            } else {
                                let _ = self.cmd_tx.send(Command::SetYaesuPtt(pressed));
                                self.midi.send_led(MidiAction::YaesuPtt, pressed);
                            }
                        }
                        MidiAction::Radio2Ptt => {
                            if self.midi_ptt_toggle_mode {
                                if pressed {
                                    let new_tx = !self.yaesu2_tx_active;
                                    let _ = self.cmd_tx.send(Command::SetYaesu2Ptt(new_tx));
                                    self.midi.send_led(MidiAction::Radio2Ptt, new_tx);
                                }
                            } else {
                                let _ = self.cmd_tx.send(Command::SetYaesu2Ptt(pressed));
                                self.midi.send_led(MidiAction::Radio2Ptt, pressed);
                            }
                        }
                        _ => {}
                    }
                }
                MidiEvent::Slider(action, value) => {
                    self.midi_last_event = format!("{} = {}", action.label(), value);
                    let frac = value as f32 / 127.0;
                    match action {
                        MidiAction::MasterVolume => {
                            self.rx_volume = frac;
                            let _ = self.cmd_tx.send(Command::SetRxVolume(frac));
                        }
                        MidiAction::VfoAVolume => {
                            // Log scale to match UI slider (0.001..=1.0 logarithmic)
                            self.vfo_a_volume = (0.001_f32 * (1000.0_f32).powf(frac)).max(0.001);
                            let _ = self.cmd_tx.send(Command::SetVfoAVolume(self.vfo_a_volume));
                        }
                        MidiAction::VfoBVolume => {
                            self.vfo_b_volume = (0.001_f32 * (1000.0_f32).powf(frac)).max(0.001);
                            let _ = self.cmd_tx.send(Command::SetVfoBVolume(self.vfo_b_volume));
                        }
                        MidiAction::TxGain => {
                            self.tx_gain = frac * 3.0;
                            let _ = self.cmd_tx.send(Command::SetTxGain(self.tx_gain));
                        }
                        MidiAction::Drive => {
                            let drive = (frac * 100.0).round() as u8;
                            self.drive_level = drive;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::DriveLevel, drive as u16));
                        }
                        MidiAction::SpectrumZoom | MidiAction::Rx2SpectrumZoom => {
                            // Slider: 0=1x, 127=max zoom (logarithmic)
                            let zoom = 1024.0_f32.powf(frac).max(1.0);
                            if matches!(action, MidiAction::Rx2SpectrumZoom) {
                                self.rx2_spectrum_zoom = zoom;
                                self.rx2_zoom_pan_changed_at = Some(std::time::Instant::now());
                            } else {
                                self.spectrum_zoom = zoom;
                                self.zoom_pan_changed_at = Some(std::time::Instant::now());
                            }
                        }
                        MidiAction::SpectrumPan | MidiAction::Rx2SpectrumPan => {
                            // Slider: 0=full left (-1.0), 64=center (0.0), 127=full right (+1.0)
                            let pan = (frac * 2.0 - 1.0).clamp(-1.0, 1.0);
                            if matches!(action, MidiAction::Rx2SpectrumPan) {
                                self.rx2_spectrum_pan = pan;
                                self.rx2_zoom_pan_changed_at = Some(std::time::Instant::now());
                            } else {
                                self.spectrum_pan = pan;
                                self.zoom_pan_changed_at = Some(std::time::Instant::now());
                            }
                        }
                        MidiAction::RefLevel | MidiAction::Rx2RefLevel => {
                            // Slider: 0=-140dB, 127=0dB
                            let ref_db = -140.0 + frac * 140.0;
                            if matches!(action, MidiAction::Rx2RefLevel) {
                                self.rx2_spectrum_ref_db = ref_db;
                                self.rx2_auto_ref_enabled = false;
                            } else {
                                self.spectrum_ref_db = ref_db;
                                self.auto_ref_enabled = false;
                            }
                        }
                        MidiAction::WaterfallContrast | MidiAction::Rx2WaterfallContrast => {
                            // Slider: 0=0.3x, 127=3.0x
                            let contrast = 0.3 + frac * 2.7;
                            if matches!(action, MidiAction::Rx2WaterfallContrast) {
                                self.rx2_waterfall_contrast = contrast;
                            } else {
                                self.waterfall_contrast = contrast;
                            }
                        }
                        MidiAction::AgcGain => {
                            // Slider: 0=-20, 127=120
                            let gain = (-20.0 + frac * 140.0).round() as u16;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::AgcGain, gain));
                        }
                        MidiAction::SqlLevel => {
                            let level = (frac * 160.0).round() as u16;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::SqlLevel, level));
                        }
                        MidiAction::CwSpeed => {
                            let wpm = (1.0 + frac * 59.0).round() as u16;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::CwKeyerSpeed, wpm));
                        }
                        MidiAction::TuneDrive => {
                            let drive = (frac * 100.0).round() as u16;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::TuneDrive, drive));
                        }
                        MidiAction::MonVolume => {
                            // Slider: 0=-40dB, 127=0dB
                            let db = (-40.0 + frac * 40.0).round() as i16;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::MonitorVolume, db as u16));
                        }
                        MidiAction::RxBalance => {
                            // Slider: 0=-40, 64=0, 127=+40
                            let bal = (-40.0 + frac * 80.0).round() as i16;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::RxBalance, bal as u16));
                        }
                        MidiAction::RitOffset => {
                            // Slider: 0-127 -> ±1270 Hz in 20 Hz steps (center=0)
                            let hz = ((value as i16 - 64) * 20).clamp(-9999, 9999);
                            self.rit_offset = hz;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::RitOffset, hz as u16));
                        }
                        MidiAction::XitOffset => {
                            let hz = ((value as i16 - 64) * 20).clamp(-9999, 9999);
                            self.xit_offset = hz;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::XitOffset, hz as u16));
                        }
                        MidiAction::YaesuVolume => {
                            self.yaesu_volume = Self::midi_log_volume(frac);
                            let _ = self.cmd_tx.send(Command::SetYaesuVolume(self.yaesu_volume));
                        }
                        MidiAction::Radio2Volume => {
                            self.yaesu2_volume = Self::midi_log_volume(frac);
                            let _ = self.cmd_tx.send(Command::SetYaesu2Volume(self.yaesu2_volume));
                        }
                        MidiAction::Vrx1Volume => {
                            self.vrx1_volume = Self::midi_log_volume(frac);
                            let _ = self.cmd_tx.send(Command::SetVrxVolume(self.vrx1_volume));
                        }
                        MidiAction::Vrx2Volume => {
                            self.vrx2_volume = Self::midi_log_volume(frac);
                            let _ = self.cmd_tx.send(Command::SetVrx2Volume(self.vrx2_volume));
                        }
                        MidiAction::YaesuRfGain => {
                            self.yaesu_rf_gain = (frac * 255.0).round() as u16;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuRfGain, self.yaesu_rf_gain));
                        }
                        MidiAction::YaesuMicGain => {
                            self.yaesu_radio_mic_gain = (frac * 100.0).round() as u16;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuRadioMicGain, self.yaesu_radio_mic_gain));
                        }
                        _ => {}
                    }
                }
                MidiEvent::Encoder(action, delta) => {
                    self.midi_last_event = format!("{} delta={}", action.label(), delta);
                    let step = self.midi_encoder_hz;
                    match action {
                        MidiAction::VfoATune => {
                            // Clamp to +/-1 on direction change (encoder backlash compensation)
                            let dir = if delta > 0 { 1i8 } else { -1 };
                            let clamped = if dir != self.midi_last_dir_a && self.midi_last_dir_a != 0 {
                                dir
                            } else {
                                delta
                            };
                            self.midi_last_dir_a = dir;
                            let new_freq = (self.frequency_hz as i64 + clamped as i64 * step as i64).max(0) as u64;
                            let _ = self.cmd_tx.send(Command::SetFrequency(new_freq));
                            self.set_pending_freq_a(new_freq);
                        }
                        MidiAction::VfoBTune => {
                            let dir = if delta > 0 { 1i8 } else { -1 };
                            let clamped = if dir != self.midi_last_dir_b && self.midi_last_dir_b != 0 {
                                dir
                            } else {
                                delta
                            };
                            self.midi_last_dir_b = dir;
                            let new_freq = (self.rx2_frequency_hz as i64 + clamped as i64 * step as i64).max(0) as u64;
                            let _ = self.cmd_tx.send(Command::SetFrequencyRx2(new_freq));
                            self.set_pending_freq_b(new_freq);
                        }
                        MidiAction::Vrx1Tune => {
                            self.midi_vrx_tune(0, delta, step);
                        }
                        MidiAction::Vrx2Tune => {
                            self.midi_vrx_tune(1, delta, step);
                        }
                        MidiAction::Radio1Tune => {
                            let new_freq = (self.yaesu_freq_a as i64 + delta as i64 * step as i64).max(0) as u64;
                            let _ = self.cmd_tx.send(Command::SetYaesuFreq(new_freq));
                            self.set_pending_yaesu_freq(0, new_freq);
                        }
                        MidiAction::Radio2Tune => {
                            let new_freq = (self.yaesu2_freq_a as i64 + delta as i64 * step as i64).max(0) as u64;
                            let _ = self.cmd_tx.send(Command::SetYaesu2Freq(new_freq));
                            self.set_pending_yaesu_freq(1, new_freq);
                        }
                        MidiAction::SpectrumZoom | MidiAction::Rx2SpectrumZoom => {
                            let factor = 1.1_f32.powi(delta as i32);
                            if matches!(action, MidiAction::Rx2SpectrumZoom) {
                                self.rx2_spectrum_zoom = (self.rx2_spectrum_zoom * factor).clamp(1.0, 1024.0);
                                self.rx2_zoom_pan_changed_at = Some(std::time::Instant::now());
                            } else {
                                self.spectrum_zoom = (self.spectrum_zoom * factor).clamp(1.0, 1024.0);
                                self.zoom_pan_changed_at = Some(std::time::Instant::now());
                            }
                        }
                        MidiAction::SpectrumPan | MidiAction::Rx2SpectrumPan => {
                            let pan_step = 0.05 * delta as f32;
                            if matches!(action, MidiAction::Rx2SpectrumPan) {
                                self.rx2_spectrum_pan = (self.rx2_spectrum_pan + pan_step).clamp(-1.0, 1.0);
                                self.rx2_zoom_pan_changed_at = Some(std::time::Instant::now());
                            } else {
                                self.spectrum_pan = (self.spectrum_pan + pan_step).clamp(-1.0, 1.0);
                                self.zoom_pan_changed_at = Some(std::time::Instant::now());
                            }
                        }
                        MidiAction::RefLevel | MidiAction::Rx2RefLevel => {
                            if matches!(action, MidiAction::Rx2RefLevel) {
                                self.rx2_spectrum_ref_db = (self.rx2_spectrum_ref_db + delta as f32).clamp(-140.0, 0.0);
                                self.rx2_auto_ref_enabled = false;
                            } else {
                                self.spectrum_ref_db = (self.spectrum_ref_db + delta as f32).clamp(-140.0, 0.0);
                                self.auto_ref_enabled = false;
                            }
                        }
                        MidiAction::WaterfallContrast | MidiAction::Rx2WaterfallContrast => {
                            let factor = 1.1_f32.powi(delta as i32);
                            if matches!(action, MidiAction::Rx2WaterfallContrast) {
                                self.rx2_waterfall_contrast = (self.rx2_waterfall_contrast * factor).clamp(0.3, 3.0);
                            } else {
                                self.waterfall_contrast = (self.waterfall_contrast * factor).clamp(0.3, 3.0);
                            }
                        }
                        MidiAction::NrLevel => {
                            // Encoder: up = increase, down = decrease, clamp 0-4
                            let new_level = (self.nr_level as i32 + if delta > 0 { 1 } else { -1 }).clamp(0, 4) as u8;
                            if new_level != self.nr_level {
                                let _ = self.cmd_tx.send(Command::SetControl(ControlId::NoiseReduction, new_level as u16));
                            }
                        }
                        MidiAction::RitOffset => {
                            let new_hz = (self.rit_offset as i32 + delta as i32 * 10).clamp(-9999, 9999) as i16;
                            self.rit_offset = new_hz;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::RitOffset, new_hz as u16));
                        }
                        MidiAction::XitOffset => {
                            let new_hz = (self.xit_offset as i32 + delta as i32 * 10).clamp(-9999, 9999) as i16;
                            self.xit_offset = new_hz;
                            let _ = self.cmd_tx.send(Command::SetControl(ControlId::XitOffset, new_hz as u16));
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    pub(super) fn midi_log_volume(frac: f32) -> f32 {
        (0.001_f32 * (1000.0_f32).powf(frac)).max(0.001)
    }

    pub(super) fn midi_vrx_tune(&mut self, vrx_id: u8, delta: i8, step_hz: u64) {
        let (center, span, current, source) = if vrx_id == 0 {
            (
                self.full_spectrum_center_hz,
                self.full_spectrum_span_hz,
                self.vrx1_freq_hz,
                self.frequency_hz,
            )
        } else {
            (
                self.rx2_full_spectrum_center_hz,
                self.rx2_full_spectrum_span_hz,
                self.vrx2_freq_hz,
                self.rx2_frequency_hz,
            )
        };
        let center = if center > 0 { center as u64 } else { source };
        let _ = span; // reachable range comes from the shared VRX limit, not the raw DDC span
        // Same boundary as the panel and the server: stop where the audio stops.
        let (min_hz, max_hz) = self.vrx_tune_limits(
            if vrx_id == 0 { VrxChannel::Vrx1 } else { VrxChannel::Vrx2 },
            center,
        );
        let base = if current > 0 { current } else { source }.clamp(min_hz, max_hz);
        let next = step_on_grid(base, delta as i64 * step_hz as i64, step_hz as u64, min_hz, max_hz);
        if vrx_id == 0 {
            self.vrx1_freq_hz = next;
            let _ = self.cmd_tx.send(Command::SetVrxFrequency(next));
        } else {
            self.vrx2_freq_hz = next;
            let _ = self.cmd_tx.send(Command::SetVrx2Frequency(next));
        }
    }

    pub(super) fn midi_band_step(&mut self, direction: i32) {
        self.midi_band_step_for(Vfo::A, direction);
    }

    pub(super) fn midi_band_step_for(&mut self, vfo: Vfo, direction: i32) {
        const BANDS: &[(&str, u64)] = &[
            ("160m", 1_900_000), ("80m", 3_700_000), ("60m", 5_351_000),
            ("40m", 7_100_000), ("30m", 10_120_000), ("20m", 14_200_000),
            ("17m", 18_100_000), ("15m", 21_200_000), ("12m", 24_930_000),
            ("10m", 28_500_000), ("6m", 50_200_000),
        ];
        let freq_hz = match vfo {
            Vfo::A => self.frequency_hz,
            Vfo::B => self.rx2_frequency_hz,
        };
        let current = band_label(freq_hz);
        let idx = BANDS.iter().position(|&(name, _)| name == current);
        let new_idx = match idx {
            Some(i) => (i as i32 + direction).rem_euclid(BANDS.len() as i32) as usize,
            None => 0,
        };
        self.save_current_band(vfo);
        self.restore_band(vfo, BANDS[new_idx].0, BANDS[new_idx].1);
        self.save_full_config();
    }

    pub(super) fn render_midi_action_section(ui: &mut egui::Ui, title: &str) {
        ui.add_space(3.0);
        ui.label(RichText::new(title).strong().color(Color32::from_rgb(170, 205, 240)));
    }

    pub(super) fn render_midi_action_pair(
        ui: &mut egui::Ui,
        selected: &mut crate::midi::MidiAction,
        left: crate::midi::MidiAction,
        right: crate::midi::MidiAction,
    ) {
        ui.horizontal(|ui| {
            ui.selectable_value(selected, left, left.label());
            ui.selectable_value(selected, right, right.label());
        });
    }

    pub(super) fn render_midi_action_single(
        ui: &mut egui::Ui,
        selected: &mut crate::midi::MidiAction,
        action: crate::midi::MidiAction,
    ) {
        ui.selectable_value(selected, action, action.label());
    }

    pub(super) fn render_midi_action_picker(ui: &mut egui::Ui, selected: &mut crate::midi::MidiAction) {
        use crate::midi::MidiAction;

        Self::render_midi_action_section(ui, "RX");
        Self::render_midi_action_pair(ui, selected, MidiAction::VfoATune, MidiAction::VfoBTune);
        Self::render_midi_action_pair(ui, selected, MidiAction::VfoAVolume, MidiAction::VfoBVolume);
        Self::render_midi_action_pair(ui, selected, MidiAction::BandUp, MidiAction::Rx2BandUp);
        Self::render_midi_action_pair(ui, selected, MidiAction::BandDown, MidiAction::Rx2BandDown);
        Self::render_midi_action_single(ui, selected, MidiAction::Ptt);

        ui.separator();
        Self::render_midi_action_section(ui, "VRX");
        Self::render_midi_action_pair(ui, selected, MidiAction::Vrx1Tune, MidiAction::Vrx2Tune);
        Self::render_midi_action_pair(ui, selected, MidiAction::Vrx1Volume, MidiAction::Vrx2Volume);

        ui.separator();
        Self::render_midi_action_section(ui, &rust_i18n::t!("screen_radio").to_string());
        Self::render_midi_action_pair(ui, selected, MidiAction::YaesuPtt, MidiAction::Radio2Ptt);
        Self::render_midi_action_pair(ui, selected, MidiAction::Radio1Tune, MidiAction::Radio2Tune);
        Self::render_midi_action_pair(ui, selected, MidiAction::YaesuVolume, MidiAction::Radio2Volume);
        Self::render_midi_action_pair(ui, selected, MidiAction::Radio1BandUp, MidiAction::Radio2BandUp);
        Self::render_midi_action_pair(ui, selected, MidiAction::Radio1BandDown, MidiAction::Radio2BandDown);

        ui.separator();
        Self::render_midi_action_section(ui, &rust_i18n::t!("screen_other_advanced").to_string());
        const PRIMARY_ACTIONS: &[MidiAction] = &[
            MidiAction::Ptt,
            MidiAction::VfoATune,
            MidiAction::VfoBTune,
            MidiAction::VfoAVolume,
            MidiAction::VfoBVolume,
            MidiAction::BandUp,
            MidiAction::BandDown,
            MidiAction::Rx2BandUp,
            MidiAction::Rx2BandDown,
            MidiAction::Vrx1Tune,
            MidiAction::Vrx2Tune,
            MidiAction::Vrx1Volume,
            MidiAction::Vrx2Volume,
            MidiAction::YaesuPtt,
            MidiAction::Radio2Ptt,
            MidiAction::Radio1Tune,
            MidiAction::Radio2Tune,
            MidiAction::YaesuVolume,
            MidiAction::Radio2Volume,
            MidiAction::Radio1BandUp,
            MidiAction::Radio1BandDown,
            MidiAction::Radio2BandUp,
            MidiAction::Radio2BandDown,
        ];
        for action in crate::midi::MidiAction::ALL {
            if PRIMARY_ACTIONS.contains(action) {
                continue;
            }
            Self::render_midi_action_single(ui, selected, *action);
        }
    }
    pub(super) fn render_midi_screen(&mut self, ui: &mut egui::Ui) {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));

        // Device selection
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("screen_midi_device").to_string());
            if ui.button(rust_i18n::t!("screen_refresh").to_string()).clicked() {
                self.midi_ports = crate::midi::MidiManager::list_ports();
            }
        });

        if self.midi_ports.is_empty() && !self.midi.is_connected() {
            ui.label(rust_i18n::t!("screen_no_midi_devices").to_string());
        } else {
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("midi_port")
                    .selected_text(if self.midi_selected_port.is_empty() {
                        rust_i18n::t!("screen_select_device").to_string()
                    } else {
                        self.midi_selected_port.clone()
                    })
                    .show_ui(ui, |ui| {
                        for port in &self.midi_ports {
                            ui.selectable_value(&mut self.midi_selected_port, port.clone(), port);
                        }
                    });

                if self.midi.is_connected() {
                    if ui.button(rust_i18n::t!("screen_disconnect").to_string()).clicked() {
                        self.midi.disconnect();
                    }
                    ui.colored_label(Color32::GREEN, rust_i18n::t!("screen_connected").to_string());
                } else {
                    let can_connect = !self.midi_selected_port.is_empty();
                    if ui.add_enabled(can_connect, egui::Button::new(rust_i18n::t!("screen_connect").to_string())).clicked() {
                        if self.midi.connect(&self.midi_selected_port) {
                            self.save_full_config();
                        }
                    }
                    ui.colored_label(Color32::RED, rust_i18n::t!("screen_disconnected").to_string());
                }
            });
        }

        ui.separator();

        // MIDI PTT mode (independent from main PTT mode)
        ui.horizontal(|ui| {
            ui.label("MIDI PTT:");
            if ui.selectable_label(!self.midi_ptt_toggle_mode, rust_i18n::t!("screen_push_to_talk").to_string()).clicked() {
                self.midi_ptt_toggle_mode = false;
                self.save_ptt_config();
            }
            if ui.selectable_label(self.midi_ptt_toggle_mode, rust_i18n::t!("screen_toggle").to_string()).clicked() {
                self.midi_ptt_toggle_mode = true;
                self.save_ptt_config();
            }
        });

        // Encoder step setting
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("screen_encoder_step").to_string());
            let steps: &[u64] = &[1, 10, 100, 500, 1000];
            let labels = ["1 Hz", "10 Hz", "100 Hz", "500 Hz", "1 kHz"];
            for (i, &step) in steps.iter().enumerate() {
                let btn = if self.midi_encoder_hz == step {
                    egui::Button::new(RichText::new(labels[i]).size(11.0).strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else {
                    egui::Button::new(RichText::new(labels[i]).size(11.0))
                };
                if ui.add(btn).clicked() {
                    self.midi_encoder_hz = step;
                    self.save_full_config();
                }
            }
        });

        ui.separator();

        // Activity monitor
        if !self.midi_last_event.is_empty() {
            ui.horizontal(|ui| {
                ui.label(rust_i18n::t!("screen_last_midi").to_string());
                ui.monospace(&self.midi_last_event);
            });
            ui.separator();
        }

        // Mappings table
        ui.label(RichText::new(rust_i18n::t!("screen_mappings").to_string()).strong());

        let mappings = self.midi.get_mappings();
        let mut remove_idx: Option<usize> = None;

        egui::Grid::new("midi_mappings")
            .striped(true)
            .min_col_width(60.0)
            .show(ui, |ui| {
                ui.label(RichText::new(rust_i18n::t!("screen_col_source").to_string()).strong());
                ui.label(RichText::new(rust_i18n::t!("screen_col_type").to_string()).strong());
                ui.label(RichText::new(rust_i18n::t!("screen_col_action").to_string()).strong());
                ui.label("");
                ui.end_row();

                for (i, mapping) in mappings.iter().enumerate() {
                    ui.monospace(mapping.source_label());
                    ui.label(mapping.control_type.label());
                    ui.label(mapping.action.label());
                    if ui.small_button("X").clicked() {
                        remove_idx = Some(i);
                    }
                    ui.end_row();
                }
            });

        if let Some(idx) = remove_idx {
            self.midi.remove_mapping(idx);
            self.save_full_config();
        }

        ui.add_space(8.0);

        // Learn mode / Add mapping
        if let Some(_) = self.midi_learn_for {
            ui.horizontal(|ui| {
                ui.label(rust_i18n::t!("screen_learning").to_string());
                ui.label(RichText::new(self.midi_learn_action.label()).strong());
                ui.label(rust_i18n::t!("screen_move_control_hint").to_string());
                if ui.button(rust_i18n::t!("screen_cancel").to_string()).clicked() {
                    self.midi_learn_for = None;
                    self.midi.set_learn_mode(false);
                }
            });
        } else {
            ui.horizontal(|ui| {
                ui.label(rust_i18n::t!("screen_add").to_string());
                egui::ComboBox::from_id_salt("midi_learn_action")
                    .selected_text(self.midi_learn_action.label())
                    .width(280.0)
                    .show_ui(ui, |ui| {
                        Self::render_midi_action_picker(ui, &mut self.midi_learn_action);
                    });
                if ui.button(rust_i18n::t!("screen_learn").to_string()).clicked() && self.midi.is_connected() {
                    self.midi_learn_for = Some(mappings.len());
                    self.midi.set_learn_mode(true);
                }
            });
        }
    }
}
