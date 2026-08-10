// SPDX-License-Identifier: GPL-2.0-or-later
//! Yaesu radio device UI (the ~half of the old `ui/devices.rs`): per-slot DSP /
//! levels / clarifier blocks, the Yaesu pop-out + panel, EQ, memories, the EX/menu
//! screens, the compact status strip and the device-tab Yaesu renderer. VRX/slot 0+1
//! share the same renderers (parity by construction). Extracted verbatim from
//! `ui/devices.rs` - pure relocation, no behaviour change. `pub(super)` keeps every
//! method callable from the parent module tree.

use super::*;

impl SdrRemoteApp {
    /// Shared DSP/function control block for a Yaesu radio slot (used by both slot 0
    /// and slot 1 so styling stays identical - single shared renderer, no per-window drift).
    /// PATCH-yaesu-extra-controls Phase A1: RF-ATT + Break-in toggles, driven by the
    /// feature-state bitfield (bit N = YaesuCtrl N). Blue fill = on.
    /// Optimistic toggle update: flip the local feature bit immediately on click + set the
    /// debounce, so the button responds instantly; the poll confirms/corrects (~0.5s, safety net).
    pub(super) fn yaesu_toggle_flip(&mut self, slot: u8, bit: u32) {
        if slot == 0 {
            self.yaesu_feature_toggles ^= 1 << bit;
            self.yaesu_control_changed_at = Some(Instant::now());
        } else {
            self.yaesu2_feature_toggles ^= 1 << bit;
            self.yaesu2_control_changed_at = Some(Instant::now());
        }
    }

    /// Band/mode availability of Yaesu controls (FT-991A/FTX-1), per slot:
    /// - `hf_6m`: IPO/ATT and the internal ATU only exist on HF+6m (<54 MHz); on
    ///   2m/70cm the radio has no menu option for them (tester report: button snaps
    ///   back after ~1 s). Grey out outside HF+6m.
    /// - `is_fm`: NB/DNF/Notch/Contour + ATU are not usable in FM on the 991A
    ///   (NAR/AGC/DNR do work in FM according to the 991A OM).
    /// yaesu_mode encoding: 5 = FM (see render_yaesu_popout mode_label).
    pub(super) fn yaesu_band_mode(&self, slot: u8) -> (bool, bool) {
        let (freq, mode) = if slot == 0 {
            (self.yaesu_freq_a, self.yaesu_mode)
        } else {
            (self.yaesu2_freq_a, self.yaesu2_mode)
        };
        // freq >= 1: on connect (freq still 0) don't enable HF-only buttons before the
        // band is known (parity with Android). is_fm = FM family (5=FM, 12=C4FM).
        (freq >= 1 && freq < 54_000_000, matches!(mode, 5 | 12))
    }

    pub(super) fn render_yaesu_dsp_block(&mut self, ui: &mut egui::Ui, slot: u8, toggles: u32, levels: [u8; 16]) {
        let (hf_6m, is_fm) = self.yaesu_band_mode(slot);
        // BK-IN is CW-only (991A OM: Break-In is entirely under CW Mode Operation).
        let is_cw = matches!(if slot == 0 { self.yaesu_mode } else { self.yaesu2_mode }, 3 | 4);
        let toggle_btn = |label: &str, on: bool| {
            if on {
                egui::Button::new(RichText::new(label).size(11.0).color(Color32::WHITE))
                    .fill(Color32::from_rgb(0, 90, 200)).min_size(egui::vec2(46.0, 20.0))
            } else {
                egui::Button::new(RichText::new(label).size(11.0)).min_size(egui::vec2(46.0, 20.0))
            }
        };
        ui.horizontal_wrapped(|ui| {
            ui.label("DSP:");
            let att_on = toggles & (1 << 0) != 0; // YaesuCtrl::RfAtt (HF+6m only)
            if ui.add_enabled(hf_6m, toggle_btn("ATT", att_on)).clicked() {
                let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, 0, if att_on { 0 } else { 1 }));
                self.yaesu_toggle_flip(slot, 0);
            }
            let bi_on = toggles & (1 << 1) != 0; // YaesuCtrl::BreakIn (CW-only)
            if ui.add_enabled(is_cw, toggle_btn("BK-IN", bi_on)).clicked() {
                let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, 1, if bi_on { 0 } else { 1 }));
                self.yaesu_toggle_flip(slot, 1);
            }
            let nar_on = toggles & (1 << 2) != 0; // YaesuCtrl::Narrow
            if ui.add(toggle_btn("NAR", nar_on)).clicked() {
                let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, 2, if nar_on { 0 } else { 1 }));
                self.yaesu_toggle_flip(slot, 2);
            }
            let dnf_on = toggles & (1 << 3) != 0; // YaesuCtrl::AutoNotch (DNF; not in FM)
            if ui.add_enabled(!is_fm, toggle_btn("DNF", dnf_on)).clicked() {
                let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, 3, if dnf_on { 0 } else { 1 }));
                self.yaesu_toggle_flip(slot, 3);
            }
            // AGC cycle (multi-state) - YaesuCtrl::Agc index 6. No blue: there is always
            // one setting active; the label shows the setting. Hardware-verified (§13):
            // 1=FAST/2=MID/3=SLOW/4=AUTO on both radios (AUTO reads back as 4/5/6,
            // normalized to 4 by the server). OFF (0) deliberately omitted (remote risky).
            let agc = levels[6];
            let agc_lbl = match agc { 1 => "FAST", 2 => "MID", 3 => "SLOW", 4 => "AUTO", _ => "AUTO" };
            // Cycle FAST->MID->SLOW->AUTO->FAST; unknown/0 setting starts at FAST.
            let next_agc: u16 = if (1..4).contains(&agc) { (agc + 1) as u16 } else { 1 };
            if ui.add(egui::Button::new(RichText::new(format!("AGC:{}", agc_lbl)).size(11.0))
                .min_size(egui::vec2(74.0, 20.0))).clicked() {
                let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, 6, next_agc));
            }
            // Pre-amp/IPO cycle (HF) - YaesuCtrl::PreAmp index 7. Label shows the setting.
            let ipo = levels[7];
            let ipo_lbl = match ipo { 0 => "IPO", 1 => "AMP1", _ => "AMP2" };
            if ui.add_enabled(hf_6m, egui::Button::new(RichText::new(ipo_lbl).size(11.0))
                .min_size(egui::vec2(52.0, 20.0))).clicked() {
                let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, 7, ((ipo + 1) % 3) as u16));
            }
        });
    }

    /// Collapsible level-slider block (Phase C): NB/DNR/Processor/AMC as sliders,
    /// per radio slot. Debounced via yaesu(2)_control_changed_at so a drag isn't
    /// reset every frame by the radio readback. Shared for slot 0/1.
    pub(super) fn render_yaesu_levels_block(&mut self, ui: &mut egui::Ui, slot: u8) {
        let s = slot as usize;
        // AMC (AO) only exists on the FTX-1; the 991A has no AO CAT command
        // (no readback -> slider would snap back). Model code 1 = FTX-1.
        let is_ftx1 = (if slot == 0 { self.yaesu_model } else { self.yaesu2_model }) == 1;
        // APF is CW-only (mode 3=CW, 4=CW-R); grey out outside CW.
        let is_cw = matches!(if slot == 0 { self.yaesu_mode } else { self.yaesu2_mode }, 3 | 4);
        // NB/DNR/Contour/Notch are not usable in FM (mode 5) -> grey out.
        let is_fm = matches!(if slot == 0 { self.yaesu_mode } else { self.yaesu2_mode }, 5 | 12);
        let toggles = if slot == 0 { self.yaesu_feature_toggles } else { self.yaesu2_feature_toggles };
        egui::CollapsingHeader::new(rust_i18n::t!("dev_dsp_levels").to_string())
            .id_salt(("yaesu_levels", slot))
            .show(ui, |ui| {
                // (level-ctrl, label, min, max, ftx1_only, on/off-ctrl for 991A)
                // Proc (ctrl 10) removed: the radio speech processor does nothing on
                // USB audio (radio shaping bypassed on REAR/USB); client-side EQ/AGC
                // replace it.
                let specs: [(u8, &str, i32, i32, bool, Option<u8>); 3] = [
                    (8, "NB", 0, 10, false, Some(13)),   // 991A: NB on/off = ctrl 13
                    (9, "DNR", 0, 10, false, Some(14)),  // 991A: NR on/off = ctrl 14
                    (11, "AMC", 1, 100, true, None),
                ];
                for (j, &(ctrl, label, lo, hi, ftx1_only, toggle_ctrl)) in specs.iter().enumerate() {
                    if ftx1_only && !is_ftx1 { continue; }
                    // NB not in FM (tester-confirmed on hardware). DNR stays: the 991A OM
                    // doesn't limit DNR noise reduction to non-FM. AMC (TX audio) always usable.
                    let row_enabled = !(is_fm && label == "NB");
                    ui.add_enabled_ui(row_enabled, |ui| {
                    ui.horizontal(|ui| {
                        // 991A: separate on/off button before the level slider (FTX-1 encodes
                        // "off" in the level itself, so no toggle there).
                        let has_toggle = toggle_ctrl.is_some() && !is_ftx1;
                        if let Some(tc) = toggle_ctrl {
                            if !is_ftx1 {
                                let on = toggles & (1 << tc) != 0;
                                let tbtn = if on {
                                    egui::Button::new(RichText::new(label).size(11.0).color(Color32::WHITE))
                                        .fill(Color32::from_rgb(0, 90, 200)).min_size(egui::vec2(46.0, 20.0))
                                } else {
                                    egui::Button::new(RichText::new(label).size(11.0)).min_size(egui::vec2(46.0, 20.0))
                                };
                                if ui.add(tbtn).clicked() {
                                    let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, tc, if on { 0 } else { 1 }));
                                    self.yaesu_toggle_flip(slot, tc as u32);
                                }
                            }
                        }
                        let sl_label = if has_toggle { "" } else { label };
                        let resp = ui.add(egui::Slider::new(&mut self.yaesu_level_sliders[s][j], lo..=hi).text(sl_label));
                        let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.yaesu_level_sliders[s][j], lo..=hi, ((hi - lo) as f64 / 50.0).max(1.0));
                        // Send only on drag-release (or a non-drag change: click/key),
                        // not on every intermediate value - otherwise a drag bursts CAT commands over
                        // the serial Yaesu link (shares with the poll -> CAT lag). Final value always.
                        if resp.drag_stopped() || (resp.changed() && !resp.dragged()) || scrolled {
                            let v = self.yaesu_level_sliders[s][j] as u16;
                            let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, ctrl, v));
                            if slot == 0 {
                                self.yaesu_control_changed_at = Some(Instant::now());
                            } else {
                                self.yaesu2_control_changed_at = Some(Instant::now());
                            }
                        }
                    });
                    });
                }
                // Phase D: Contour / APF / Manual-Notch - on/off button + frequency slider.
                let dspecs: [(usize, &str, u8, u8, i32, i32); 3] = [
                    (0, "Contour", 15, 18, 10, 3200),
                    (1, "APF", 16, 19, 0, 50),
                    (2, "Notch", 17, 20, 1, 320),
                ];
                for &(fidx, label, on_ctrl, freq_ctrl, flo, fhi) in dspecs.iter() {
                    let enabled = match label {
                        "APF" => is_cw,                    // APF only in CW (both OMs)
                        "Contour" => !is_fm && !is_cw,     // Contour: not in FM, not in CW (FTX-1 OM)
                        _ => !is_fm,                       // Notch: not in FM (manual notch does work in CW)
                    };
                    ui.add_enabled_ui(enabled, |ui| {
                    ui.horizontal(|ui| {
                        let on = toggles & (1 << on_ctrl) != 0;
                        let tbtn = if on {
                            egui::Button::new(RichText::new(label).size(11.0).color(Color32::WHITE))
                                .fill(Color32::from_rgb(0, 90, 200)).min_size(egui::vec2(58.0, 20.0))
                        } else {
                            egui::Button::new(RichText::new(label).size(11.0)).min_size(egui::vec2(58.0, 20.0))
                        };
                        if ui.add(tbtn).clicked() {
                            let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, on_ctrl, if on { 0 } else { 1 }));
                            self.yaesu_toggle_flip(slot, on_ctrl as u32);
                        }
                        let resp = ui.add(egui::Slider::new(&mut self.yaesu_freq_sliders[s][fidx], flo..=fhi).text("Hz"));
                        let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.yaesu_freq_sliders[s][fidx], flo..=fhi, ((fhi - flo) as f64 / 50.0).max(1.0));
                        // Send on drag-release instead of every intermediate value - wide freq sliders
                        // (Contour 10-3200, Notch 1-320) would otherwise queue dozens of CAT commands
                        // per drag. Final value always sent.
                        if resp.drag_stopped() || (resp.changed() && !resp.dragged()) || scrolled {
                            let v = self.yaesu_freq_sliders[s][fidx] as u16;
                            let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, freq_ctrl, v));
                            if slot == 0 {
                                self.yaesu_control_changed_at = Some(Instant::now());
                            } else {
                                self.yaesu2_control_changed_at = Some(Instant::now());
                            }
                        }
                    });
                    });
                }
            });
    }

    /// Clarifier block (§15): RIT/XIT on/off (blue=on, optimistic), offset display
    /// (freqs[3] as i16) + step buttons (±10/±100 Hz) and Clear. Shared for slot 0/1.
    /// Per-model difference is server-side (991A relative RU/RD, FTX-1 absolute CF).
    pub(super) fn render_yaesu_clarifier_block(&mut self, ui: &mut egui::Ui, slot: u8) {
        let toggles = if slot == 0 { self.yaesu_feature_toggles } else { self.yaesu2_feature_toggles };
        let offset = if slot == 0 { self.yaesu_clar_offset } else { self.yaesu2_clar_offset }; // signed Hz
        let toggle_btn = |label: &str, on: bool| {
            if on {
                egui::Button::new(RichText::new(label).size(11.0).color(Color32::WHITE))
                    .fill(Color32::from_rgb(0, 90, 200)).min_size(egui::vec2(40.0, 20.0))
            } else {
                egui::Button::new(RichText::new(label).size(11.0)).min_size(egui::vec2(40.0, 20.0))
            }
        };
        let step_btn = |label: &str| egui::Button::new(RichText::new(label).size(11.0))
            .min_size(egui::vec2(40.0, 20.0));
        ui.horizontal(|ui| {
            ui.label("Clar:");
            let rit_on = toggles & (1 << 21) != 0; // YaesuCtrl::RitOn
            if ui.add(toggle_btn("RIT", rit_on)).clicked() {
                let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, 21, if rit_on { 0 } else { 1 }));
                self.yaesu_toggle_flip(slot, 21);
            }
            let xit_on = toggles & (1 << 22) != 0; // YaesuCtrl::XitOn
            if ui.add(toggle_btn("XIT", xit_on)).clicked() {
                let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, 22, if xit_on { 0 } else { 1 }));
                self.yaesu_toggle_flip(slot, 22);
            }
            // Offset display (signed): orange only if RIT/XIT is ON and ≠0; otherwise
            // grey. The 991A can return a stored P3 offset while the clarifier
            // is off - that one isn't active, so don't show it as an active offset.
            let clar_active = rit_on || xit_on;
            let (txt, col) = if clar_active && offset != 0 {
                (format!("{:+05} Hz", offset), Color32::from_rgb(255, 170, 40))
            } else {
                (" +0000 Hz".to_string(), Color32::from_rgb(120, 120, 120))
            };
            ui.label(RichText::new(txt).size(11.0).family(egui::FontFamily::Monospace).color(col));
        });
        ui.horizontal(|ui| {
            // Step buttons: value = i16-as-u16 signed step (YaesuCtrl::ClarStep = 24).
            for &step in &[-100i16, -10] {
                if ui.add(step_btn(&format!("{}", step))).clicked() {
                    let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, 24, step as u16));
                }
            }
            if ui.add(egui::Button::new(RichText::new("Clr").size(11.0))
                .min_size(egui::vec2(40.0, 20.0))).clicked() {
                let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, 23, 0)); // ClarClear
            }
            for &step in &[10i16, 100] {
                if ui.add(step_btn(&format!("+{}", step))).clicked() {
                    let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, 24, step as u16));
                }
            }
        });
    }

    pub(super) fn render_yaesu_popout(&mut self, ui: &mut egui::Ui) {
        let mode_label = match self.yaesu_mode {
            0 => "LSB", 1 => "USB", 3 => "CW-L", 4 => "CW-U",
            5 => "FM", 6 => "AM", 7 => "DIGU", 9 => "DIGL", 12 => "C4FM",
            _ => "?",
        };

        // VFO A / VFO B / Memory selection
        ui.horizontal(|ui| {
            let btn_size = egui::vec2(44.0, 24.0);
            if ui.add(egui::Button::new(RichText::new("A/B").strong()).min_size(btn_size)).clicked() {
                let _ = self.cmd_tx.send(Command::SetControl(
                    sdr_remote_core::protocol::ControlId::YaesuSelectVfo, 2)); // SV;
            }
            if ui.add(egui::Button::new(RichText::new("V/M").strong()).min_size(btn_size)).clicked() {
                let _ = self.cmd_tx.send(Command::SetControl(
                    sdr_remote_core::protocol::ControlId::YaesuSelectVfo, 3)); // VM;
            }
            ui.separator();
            ui.label(RichText::new(mode_label).size(14.0).color(Color32::from_rgb(255, 170, 40)));
        });

        // Mode indicator: VFO / Memory
        if self.yaesu_in_memory_mode {
            if let Some(idx) = self.yaesu_current_mem_ch {
                if let Some(ch) = self.yaesu_mem_channels.get(idx) {
                    let c = Color32::from_rgb(100, 200, 255);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("MEM {:02}", ch.channel_number))
                            .size(14.0).strong().color(c));
                        ui.label(RichText::new(&ch.name).size(14.0).strong().color(c));
                        ui.label(RichText::new(super::yaesu_memory::format_freq_display(ch.rx_freq_hz))
                            .size(14.0).family(egui::FontFamily::Monospace).color(c));
                    });
                }
            }
        } else {
            let label = if self.yaesu_split_active { "VFO  Split" } else { "VFO" };
            let c = if self.yaesu_split_active { Color32::from_rgb(255, 180, 50) } else { Color32::from_rgb(100, 255, 100) };
            ui.label(RichText::new(label).size(14.0).strong().color(c));
        }

        // Frequency display with scroll/tap-to-tune + touch-friendly stepper (§16)
        ui.horizontal(|ui| {
            ui.label(RichText::new("A:  ").size(16.0).strong());
            if let Some(delta) = render_freq_scroll(ui, self.yaesu_freq_a) {
                let new_freq = (self.yaesu_freq_a as i64 + delta).max(0) as u64;
                let _ = self.cmd_tx.send(Command::SetYaesuFreq(new_freq));
                self.set_pending_yaesu_freq(0, new_freq);
            }
        });
        if let Some(delta) = render_freq_stepper(ui, &mut self.tune_step_hz) {
            let new_freq = (self.yaesu_freq_a as i64 + delta).max(0) as u64;
            let _ = self.cmd_tx.send(Command::SetYaesuFreq(new_freq));
            self.set_pending_yaesu_freq(0, new_freq);
        }

        ui.horizontal(|ui| {
            ui.label(RichText::new("B:  ").size(12.0));
            ui.label(RichText::new(format!("{} Hz", format_frequency(self.yaesu_freq_b)))
                .size(14.0).family(egui::FontFamily::Monospace));
        });

        ui.separator();

        // Mode + Band + controls row
        {
            use sdr_remote_core::protocol::ControlId;
            let btn = |text: &str| egui::Button::new(RichText::new(text).size(11.0))
                .min_size(egui::vec2(38.0, 20.0));
            let mode_names = ["LSB","USB","CW-L","CW-U","FM","AM","DIG-U","DIG-L","RTTY","C4FM","DATA-FM","DATA-USB"];
            let mode_codes: &[u8] = &[0, 1, 3, 4, 5, 6, 7, 9, 9, 5, 5, 7];

            // Mode buttons
            ui.horizontal(|ui| {
                ui.label(rust_i18n::t!("dev_mode").to_string());
                for (i, &name) in mode_names.iter().enumerate().take(8) {
                    let mb = if mode_codes[i] == self.yaesu_mode {
                        egui::Button::new(RichText::new(name).size(11.0).color(Color32::WHITE))
                            .fill(Color32::from_rgb(0, 90, 200))
                    } else { btn(name) };
                    if ui.add(mb).clicked() {
                        let _ = self.cmd_tx.send(Command::SetYaesuMode(mode_codes[i]));
                    }
                }
            });

            // Band + A=B + Split + Scan + Tune
            ui.horizontal(|ui| {
                if ui.add(btn("Band-")).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuButton, 6));
                }
                if ui.add(btn("Band+")).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuButton, 5));
                }
                ui.separator();
                if ui.add(btn("Mem-")).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuButton, 10));
                }
                if ui.add(btn("Mem+")).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuButton, 9));
                }
                ui.separator();
                if ui.add(btn("A=B")).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuButton, 0));
                }
                let split_lbl = rust_i18n::t!("dev_split").to_string();
                let split_btn = if self.yaesu_split_active {
                    egui::Button::new(RichText::new(split_lbl.as_str()).size(11.0).color(Color32::WHITE))
                        .fill(Color32::from_rgb(180, 100, 0)).min_size(egui::vec2(38.0, 20.0))
                } else { btn(split_lbl.as_str()) };
                if ui.add(split_btn).clicked() {
                    self.yaesu_split_active = !self.yaesu_split_active;
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuButton,
                        if self.yaesu_split_active { 7 } else { 8 }));
                }
                let scan_lbl = rust_i18n::t!("dev_scan").to_string();
                let scan_btn = if self.yaesu_scan_active {
                    egui::Button::new(RichText::new(scan_lbl.as_str()).size(11.0).color(Color32::WHITE))
                        .fill(Color32::from_rgb(0, 120, 0)).min_size(egui::vec2(38.0, 20.0))
                } else { btn(scan_lbl.as_str()) };
                if ui.add(scan_btn).clicked() {
                    self.yaesu_scan_active = !self.yaesu_scan_active;
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuButton,
                        if self.yaesu_scan_active { 1 } else { 2 }));
                }
                // Internal ATU: momentary Tune (band-gated: HF+6m, <54 MHz) + on/off toggle
                // that shows the real radio state (via AC; poll). PATCH-yaesu-internal-atu.
                // ATU: HF+6m (<54 MHz) and not in FM (mode 5).
                let atu_avail = self.yaesu_freq_a >= 1 && self.yaesu_freq_a < 54_000_000 && !matches!(self.yaesu_mode, 5 | 12);
                let can_tune = self.yaesu_connected && atu_avail;
                let tune_lbl = rust_i18n::t!("dev_tune").to_string();
                let tune_btn = if self.yaesu_tuner_state == 2 {
                    // Actively tuning -> red progress indication.
                    egui::Button::new(RichText::new(tune_lbl.as_str()).size(11.0).color(Color32::WHITE))
                        .fill(Color32::from_rgb(180, 0, 0)).min_size(egui::vec2(38.0, 20.0))
                } else { btn(tune_lbl.as_str()) };
                if ui.add_enabled(can_tune, tune_btn).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuButton, 3)); // start tuning (momentary)
                }
                let atu_on = self.yaesu_tuner_state == 1;
                let atu_btn = if atu_on {
                    egui::Button::new(RichText::new("ATU").size(11.0).color(Color32::WHITE))
                        .fill(Color32::from_rgb(0, 90, 200)).min_size(egui::vec2(38.0, 20.0))
                } else { btn("ATU") };
                if ui.add_enabled(atu_avail, atu_btn).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuButton,
                        if atu_on { 4 } else { 15 })); // toggle: off (AC000) / on (AC001)
                }
            });
            // DSP/function controls (PATCH-yaesu-extra-controls) - radio 1.
            let dsp0 = self.yaesu_feature_toggles;
            let lvl0 = self.yaesu_feature_levels;
            self.render_yaesu_dsp_block(ui, 0, dsp0, lvl0);
            self.render_yaesu_clarifier_block(ui, 0);
            self.render_yaesu_levels_block(ui, 0);

            // Sliders: aligned grid layout
            let label_w = 55.0;
            let slider_w = 120.0;
            egui::Grid::new("yaesu_sliders").num_columns(4).spacing([4.0, 2.0]).show(ui, |ui| {
                ui.allocate_space(egui::vec2(label_w, 0.0));
                ui.label("SQL");
                let sql_slider = egui::Slider::new(&mut self.yaesu_squelch, 0..=100)
                    .custom_formatter(|v, _| format!("{:.0}", v));
                let resp = ui.add_sized([slider_w, 16.0], sql_slider);
                let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.yaesu_squelch, 0..=100, 1.0);
                if resp.changed() || scrolled {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuSquelch, self.yaesu_squelch));
                    self.yaesu_control_changed_at = Some(Instant::now());
                }

                ui.label("PWR");
                // Slider range 5..=max for the current band (from EX137-140); 0/unknown -> 100.
                let pwr_max = if self.yaesu_tx_power_max >= 5 { self.yaesu_tx_power_max } else { 100 };
                if self.yaesu_rf_power > pwr_max { self.yaesu_rf_power = pwr_max; }
                let pwr_slider = egui::Slider::new(&mut self.yaesu_rf_power, 5..=pwr_max)
                    .custom_formatter(|v, _| format!("{:.0}W", v));
                let resp = ui.add_sized([slider_w, 16.0], pwr_slider)
                    .on_hover_text(rust_i18n::t!("dev_max_for_band", w = pwr_max).to_string());
                let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.yaesu_rf_power, 5..=pwr_max, 1.0);
                if resp.changed() || scrolled {
                    // Block the readback sync while you're dragging, so the
                    // (slower) radio feedback doesn't bounce the slider back and forth.
                    self.yaesu_control_changed_at = Some(Instant::now());
                }
                // Only send on RELEASE (or click/scroll), not every frame while dragging.
                if scrolled || resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuRfPower, self.yaesu_rf_power as u16));
                    self.yaesu_power_pending = Some(self.yaesu_rf_power);
                    self.yaesu_power_pending_at = Some(std::time::Instant::now());
                }
                ui.end_row();

                ui.allocate_space(egui::vec2(label_w, 0.0));
                ui.label(rust_i18n::t!("dev_rf_gain").to_string());
                let rf_slider = egui::Slider::new(&mut self.yaesu_rf_gain, 0..=255)
                    .custom_formatter(|v, _| format!("{:.0}", v));
                let resp = ui.add_sized([slider_w, 16.0], rf_slider);
                let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.yaesu_rf_gain, 0..=255, 2.0);
                if resp.changed() || scrolled {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuRfGain, self.yaesu_rf_gain));
                    self.yaesu_control_changed_at = Some(Instant::now());
                }
                ui.end_row();
            });
        }

        ui.separator();

        // S-meter (click toggles bar <-> analog; analog in the same place as the bar).
        let mw = ui.available_width().min(350.0).max(200.0);
        let mrect = if self.meter_analog[super::M_YAESU1] {
            smeter_analog_sized(ui,
                yaesu_raw_to_dbm(self.yaesu_smeter), yaesu_raw_to_dbm(self.yaesu_smeter_peak),
                false, false, Some((mw, 110.0)))
        } else {
            yaesu_smeter_bar(ui, self.yaesu_smeter, self.yaesu_smeter_peak)
        };
        self.meter_click(ui, mrect, super::M_YAESU1);

        ui.separator();

        // Status row
        ui.horizontal(|ui| {
            let (tx_color, tx_text) = if self.yaesu_tx_active {
                (Color32::from_rgb(220, 40, 40), "TX")
            } else {
                (Color32::from_rgb(0, 150, 0), "RX")
            };
            ui.colored_label(tx_color, RichText::new(tx_text).size(16.0).strong());
            ui.separator();
            // Radio on/off via CAT PS. ONLY safe as a clickable button on the 991A
            // (code 0): there PS0 = standby, CAT/USB stays alive -> remote back
            // on. The FTX-1 (and unknown types) really turn off at PS0 incl. USB ->
            // can no longer be turned on remotely. So for those only a status label.
            let on_col = Color32::from_rgb(0, 150, 0);
            let off_col = Color32::from_rgb(90, 90, 90);
            if self.yaesu_model == 0 {
                let (txt, col): (String, Color32) = if self.yaesu_power_on { (rust_i18n::t!("dev_power_on_upper").to_string(), on_col) } else { (rust_i18n::t!("dev_standby").to_string(), off_col) };
                if ui.add(egui::Button::new(RichText::new(txt).color(Color32::WHITE)).fill(col))
                    .on_hover_text(rust_i18n::t!("dev_991a_standby_hover").to_string())
                    .clicked()
                {
                    let _ = self.cmd_tx.send(Command::SetControl(
                        sdr_remote_core::protocol::ControlId::YaesuPowerOnOff,
                        if self.yaesu_power_on { 0 } else { 1 }));
                }
            } else {
                ui.colored_label(if self.yaesu_power_on { on_col } else { off_col },
                    if self.yaesu_power_on { rust_i18n::t!("dev_power_on_upper").to_string() } else { rust_i18n::t!("dev_power_off_upper").to_string() })
                    .on_hover_text(rust_i18n::t!("dev_radio_powers_off_hover").to_string());
            }
            if self.yaesu_hi_swr {
                ui.separator();
                ui.colored_label(theme::TL_SWR_ALERT_TEXT,
                    RichText::new(rust_i18n::t!("dev_high_swr").to_string()).size(16.0).strong());
            }
        });

        ui.separator();
        self.render_websdr_controls(ui, CatSyncTarget::Yaesu1, self.yaesu_freq_a, self.yaesu_mode);

        ui.separator();

        // Mic gain slider for Yaesu USB TX audio. Display 0.5 maps
        // to the empirically matched internal gain 0.2.
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("dev_mic_gain").to_string());
            let mut mic_gain_display = super::yaesu_mic_gain_to_display(self.yaesu_mic_gain);
            let slider = egui::Slider::new(&mut mic_gain_display, 0.05..=1.0)
                .logarithmic(true)
                .custom_formatter(|v, _| format!("{:.2}x", v));
            let resp = ui.add_sized([140.0, 16.0], slider);
            let scrolled = super::helpers::slider_wheel(ui, &resp, &mut mic_gain_display, 0.05..=1.0, 0.02);
            if resp.changed() || scrolled {
                self.yaesu_mic_gain = super::yaesu_mic_gain_from_display(mic_gain_display);
                let _ = self.cmd_tx.send(Command::SetYaesuTxGain(self.yaesu_mic_gain));
            }
            if resp.drag_stopped() || scrolled {
                self.save_ptt_config();
            }
        });

        ui.separator();

        // 5-band Equalizer - shared generic component (slot 0).
        self.render_yaesu_eq(ui, 0);

        ui.separator();

        // Memory channels - visible body height is user-resizable so it
        // doesn't push the Radio Settings header off the bottom of the window.
        if super::helpers::chevron_label(
            ui,
            self.collapse_yaesu_memories,
            RichText::new(rust_i18n::t!("dev_memory_channels").to_string()).strong().size(14.0),
        )
        .clicked()
        {
            self.collapse_yaesu_memories = !self.collapse_yaesu_memories;
            self.save_full_config();
        }
        if self.collapse_yaesu_memories {
            self.render_memories_scroll_and_handle(ui, 0);
        }

        if super::helpers::chevron_label(
            ui,
            self.collapse_yaesu_menu,
            RichText::new(rust_i18n::t!("dev_radio_settings").to_string()).strong().size(14.0),
        )
        .clicked()
        {
            self.collapse_yaesu_menu = !self.collapse_yaesu_menu;
            self.save_full_config();
        }
        if self.collapse_yaesu_menu {
            ui.indent("yaesu_menu_body", |ui| {
                self.render_yaesu_menu(ui);
            });
        }
    }

    /// Panel/window name for a radio slot (PATCH-dual-radio-991a-ftx1).
    /// Window title per radio slot: "ThetisLink - Yaesu {N}: {type}" — consistent
    /// with the server tab and the detail tab (via yaesu_slot_label). Type from RadioInfo.
    pub(super) fn yaesu_window_title(&self, slot: u8) -> String {
        format!("ThetisLink - {}", self.yaesu_slot_label(slot))
    }

    /// Purely the server-reported radio type per slot (the RadioInfo model byte),
    /// without slot suffix. Name comes from the canonical core table (`radio_model_name`)
    /// so a new radio type comes along automatically. For the "Yaesu N: <type>" display.
    pub(super) fn yaesu_type_name(&self, slot: u8) -> &'static str {
        let code = if slot == 0 { self.yaesu_model } else { self.yaesu2_model };
        sdr_remote_core::protocol::radio_model_name(code)
    }

    /// Consistent slot display: "Yaesu 1: 991A" / "Yaesu 2: FTX1". Slot number +
    /// the active type from the server config (RadioInfo). Everywhere in the server tab.
    pub(super) fn yaesu_slot_label(&self, slot: u8) -> String {
        format!("Yaesu {}: {}", slot + 1, self.yaesu_type_name(slot))
    }

    /// Name for a packet type in the bitstream/data-usage breakdown. For the
    /// Yaesu-slot-bound types this injects the consistent "Yaesu N: <type>"
    /// label (same as levels/streams/recording); the rest falls back to the
    /// static packet-type table.
    pub(super) fn bitstream_label(&self, t: u8) -> String {
        match t {
            0x16 => { let label = self.yaesu_slot_label(0); rust_i18n::t!("dev_audio_fmt", label = label).to_string() }
            0x17 => { let label = self.yaesu_slot_label(0); rust_i18n::t!("dev_state_fmt", label = label).to_string() }
            0x19 => { let label = self.yaesu_slot_label(0); rust_i18n::t!("dev_memory_data_fmt", label = label).to_string() }
            0x25 => { let label = self.yaesu_slot_label(1); rust_i18n::t!("dev_audio_fmt", label = label).to_string() }
            0x26 => { let label = self.yaesu_slot_label(1); rust_i18n::t!("dev_state_fmt", label = label).to_string() }
            _ => super::screens::packet_type_label(t),
        }
    }

    /// Focused slot-1 radio panel (dual-radio phase 1): VFO-A freq + tune, mode,
    /// PTT, S-meter, volume. Memory/EX-menu = phase 2. Reads self.yaesu2_* (synced
    /// from state) and sends SetYaesu2* commands.
    /// Generic 5-band EQ section (chevron + save/delete profiles +
    /// value per slider), SHARED by both Yaesu windows (slot 0 = 991A window,
    /// slot 1 = FTX-1 window) - parity by construction, not per-window repainting.
    /// Snapshot->writeback keeps the borrow checker happy with per-slot state +
    /// per-slot commands (SetYaesu*/SetYaesu2*).
    pub(super) fn render_yaesu_eq(&mut self, ui: &mut egui::Ui, slot: u8) {
        let mut collapse = if slot == 0 { self.collapse_yaesu_eq } else { self.collapse_yaesu2_eq };
        // Header row: chevron + the EQ on/off checkbox next to it, so the EQ state
        // is visible (and toggleable) WITHOUT expanding - same as Android. This is
        // the ONLY EQ on/off control (the body checkbox has been removed), per the
        // UI style guide "no duplicate control for the same scope".
        ui.horizontal(|ui| {
            if super::helpers::chevron_label(ui, collapse,
                RichText::new(rust_i18n::t!("dev_equalizer").to_string()).strong().size(14.0)).clicked()
            {
                collapse = !collapse;
                if slot == 0 { self.collapse_yaesu_eq = collapse; } else { self.collapse_yaesu2_eq = collapse; }
                self.save_full_config();
            }
            let mut eq_on = if slot == 0 { self.yaesu_eq_enabled } else { self.yaesu2_eq_enabled };
            if ui.checkbox(&mut eq_on, "EQ").changed() {
                if slot == 0 { self.yaesu_eq_enabled = eq_on; } else { self.yaesu2_eq_enabled = eq_on; }
                let _ = self.cmd_tx.send(if slot == 0 { Command::SetYaesuEqEnabled(eq_on) } else { Command::SetYaesu2EqEnabled(eq_on) });
            }
        });
        if !collapse { return; }

        // Snapshot per-slot state in locals (writeback at the bottom).
        let mut enabled = if slot == 0 { self.yaesu_eq_enabled } else { self.yaesu2_eq_enabled };
        let mut gains = if slot == 0 { self.yaesu_eq_gains } else { self.yaesu2_eq_gains };
        let mut mic_gain = if slot == 0 { self.yaesu_mic_gain } else { self.yaesu2_mic_gain };
        let mut profiles = if slot == 0 { self.yaesu_eq_profiles.clone() } else { self.yaesu2_eq_profiles.clone() };
        let mut active = if slot == 0 { self.yaesu_eq_active_profile.clone() } else { self.yaesu2_eq_active_profile.clone() };
        let mut new_name = if slot == 0 { self.yaesu_eq_new_name.clone() } else { self.yaesu2_eq_new_name.clone() };
        let tx = self.cmd_tx.clone();
        let mut dirty = false;
        let mk_en = |on: bool| if slot == 0 { Command::SetYaesuEqEnabled(on) } else { Command::SetYaesu2EqEnabled(on) };
        let mk_band = |b: u8, g: f32| if slot == 0 { Command::SetYaesuEqBand(b, g) } else { Command::SetYaesu2EqBand(b, g) };
        let mk_gain = |g: f32| if slot == 0 { Command::SetYaesuTxGain(g) } else { Command::SetYaesu2TxGain(g) };
        // Client-side TX chain per radio: compressor (0-100) + AGC toggle.
        let mut comp = if slot == 0 { self.yaesu_compressor } else { self.yaesu2_compressor };
        let mut agc = if slot == 0 { self.yaesu_tx_agc } else { self.yaesu2_tx_agc };
        let mut chain_dirty = false;
        let mk_comp = |v: u8| if slot == 0 { Command::SetYaesuCompressor(v) } else { Command::SetYaesu2Compressor(v) };
        let mk_agc = |on: bool| if slot == 0 { Command::SetYaesuTxAgc(on) } else { Command::SetYaesu2TxAgc(on) };

        ui.indent(("yaesu_eq_body", slot), |ui| {
            ui.horizontal(|ui| {
                let names: Vec<String> = profiles.iter().map(|(n, _, _, _)| n.clone()).collect();
                egui::ComboBox::from_id_salt(("eq_profile", slot))
                    .selected_text(if active.is_empty() { "---" } else { active.as_str() })
                    .width(100.0)
                    .show_ui(ui, |ui| {
                        for name in &names {
                            if ui.selectable_label(&active == name, name).clicked() {
                                active = name.clone();
                                if let Some((_, en, g, mg)) = profiles.iter().find(|(n, _, _, _)| n == name) {
                                    enabled = *en; gains = *g; mic_gain = *mg;
                                    let _ = tx.send(mk_en(*en));
                                    for i in 0..5 { let _ = tx.send(mk_band(i as u8, g[i])); }
                                    let _ = tx.send(mk_gain(*mg));
                                }
                                dirty = true;
                            }
                        }
                    });
                if ui.small_button(rust_i18n::t!("dev_save").to_string()).clicked() && !active.is_empty() {
                    let name = active.clone();
                    if let Some(p) = profiles.iter_mut().find(|(n, _, _, _)| *n == name) {
                        p.1 = enabled; p.2 = gains; p.3 = mic_gain;
                    } else {
                        profiles.push((name, enabled, gains, mic_gain));
                    }
                    dirty = true;
                }
                if ui.small_button("Del").clicked() && !active.is_empty() {
                    profiles.retain(|(n, _, _, _)| n != &active);
                    active.clear();
                    dirty = true;
                }
            });
            ui.horizontal(|ui| {
                ui.label(rust_i18n::t!("dev_new").to_string());
                ui.add(egui::TextEdit::singleline(&mut new_name).desired_width(100.0));
                if ui.small_button("+").clicked() && !new_name.is_empty() {
                    let name = new_name.clone();
                    profiles.push((name.clone(), enabled, gains, mic_gain));
                    active = name;
                    new_name.clear();
                    dirty = true;
                }
            });
            ui.horizontal(|ui| {
                for i in 0..5 {
                    ui.vertical(|ui| {
                        ui.set_width(50.0);
                        ui.label(RichText::new(sdr_remote_logic::eq::BAND_LABELS[i]).size(10.0));
                        let mut g = gains[i];
                        let slider = egui::Slider::new(&mut g, -12.0..=12.0)
                            .vertical()
                            .custom_formatter(|v, _| format!("{:+.0}", v));
                        let resp = ui.add_sized([20.0, 60.0], slider);
                        let scrolled = super::helpers::slider_wheel(ui, &resp, &mut g, -12.0..=12.0, 0.5);
                        if resp.changed() || scrolled {
                            gains[i] = g;
                            let _ = tx.send(mk_band(i as u8, g));
                        }
                    });
                }
            });
            // Client-side TX chain (radio processing doesn't work over USB): AGC + compressor.
            ui.horizontal(|ui| {
                if ui.checkbox(&mut agc, "AGC").changed() {
                    let _ = tx.send(mk_agc(agc));
                    chain_dirty = true;
                }
                ui.label("Comp");
                let mut c = comp as f32;
                let resp = ui.add(egui::Slider::new(&mut c, 0.0..=100.0)
                    .custom_formatter(|v, _| format!("{:.0}", v)));
                let scrolled = super::helpers::slider_wheel(ui, &resp, &mut c, 0.0..=100.0, 2.0);
                if resp.changed() || scrolled {
                    comp = c.round() as u8;
                    let _ = tx.send(mk_comp(comp));
                }
                if resp.drag_stopped() || scrolled { chain_dirty = true; }
            });
        });

        // Writeback to the slot state.
        if slot == 0 {
            self.yaesu_eq_enabled = enabled; self.yaesu_eq_gains = gains; self.yaesu_mic_gain = mic_gain;
            self.yaesu_eq_profiles = profiles; self.yaesu_eq_active_profile = active; self.yaesu_eq_new_name = new_name;
            self.yaesu_compressor = comp; self.yaesu_tx_agc = agc;
        } else {
            self.yaesu2_eq_enabled = enabled; self.yaesu2_eq_gains = gains; self.yaesu2_mic_gain = mic_gain;
            self.yaesu2_eq_profiles = profiles; self.yaesu2_eq_active_profile = active; self.yaesu2_eq_new_name = new_name;
            self.yaesu2_compressor = comp; self.yaesu2_tx_agc = agc;
        }
        if dirty { self.save_full_config(); }
        if chain_dirty { self.save_ptt_config(); } // comp/AGC follow the mic-gain append pattern
    }

    /// Memory table per radio slot. Slot 0 = direct. Slot 1: swap the slot-1
    /// state via mem::swap into the (shared) slot-0 fields, render the same table,
    /// and swap back - this keeps the working 991A table unchanged and the UI
    /// shared. The read/write commands follow `yaesu_mem_active_slot`.
    /// The memory list plus the drag-handle underneath it, shared by both Yaesu
    /// windows (parity by construction, per docs/internal/UI-STYLE-GUIDE.md).
    ///
    /// Slot 1 used to bound the list with `ui.available_height()` instead of the
    /// stored height, and had no handle. The list then took everything below it,
    /// so Radio settings could never be reached while memories were expanded -
    /// and the handle IS the horizontal line that separates the two, so its
    /// absence also removed the visual boundary. A comment there claimed the line
    /// had been dropped "for parity with the 991A window", which is the opposite
    /// of what that window does.
    ///
    /// The height is stored PER SLOT: the two windows are arranged independently,
    /// so one shared value would move the other list while you adjust this one.
    pub(super) fn render_memories_scroll_and_handle(&mut self, ui: &mut egui::Ui, slot: u8) {
        let mem_max_h = if slot == 0 { self.yaesu_memories_h } else { self.yaesu2_memories_h };
        let (body_id, scroll_id) = if slot == 0 {
            ("yaesu_memories_body", "yaesu_memories_scroll")
        } else {
            ("yaesu2_memories_body", "yaesu2_memories_scroll")
        };
        ui.indent(body_id, |ui| {
            egui::ScrollArea::vertical()
                .id_salt(scroll_id)
                .max_height(mem_max_h)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    self.render_yaesu_memories_slot(ui, slot);
                });
        });

        // Drag-handle: resizes the visible portion of the list, and doubles as the
        // line that marks where Radio settings begins.
        let handle_h = 6.0;
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), handle_h),
            egui::Sense::drag(),
        );
        let visuals = ui.visuals();
        let fill = if response.hovered() || response.dragged() {
            visuals.widgets.hovered.bg_fill
        } else {
            visuals.widgets.inactive.bg_fill
        };
        ui.painter().rect_filled(rect, 2.0, fill);
        let grip_color = visuals.widgets.inactive.fg_stroke.color;
        let cx = rect.center().x;
        let cy = rect.center().y;
        for dx in [-12.0, 0.0, 12.0] {
            ui.painter().line_segment(
                [egui::pos2(cx + dx - 4.0, cy), egui::pos2(cx + dx + 4.0, cy)],
                egui::Stroke::new(1.0, grip_color),
            );
        }
        if response.dragged() {
            let dy = response.drag_delta().y;
            if dy.abs() > 0.01 {
                let h = if slot == 0 { &mut self.yaesu_memories_h } else { &mut self.yaesu2_memories_h };
                *h = (*h + dy).clamp(100.0, 800.0);
            }
        }
        if response.drag_stopped() {
            self.save_full_config();
        }
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
        }
    }

    pub(super) fn render_yaesu_memories_slot(&mut self, ui: &mut egui::Ui, slot: u8) {
        if slot == 0 {
            self.render_yaesu_memories(ui);
            return;
        }
        let swap_in_out = |s: &mut Self| {
            std::mem::swap(&mut s.yaesu_mem_channels, &mut s.yaesu2_mem_channels);
            std::mem::swap(&mut s.yaesu_mem_file, &mut s.yaesu2_mem_file);
            std::mem::swap(&mut s.yaesu_mem_selected, &mut s.yaesu2_mem_selected);
            std::mem::swap(&mut s.yaesu_mem_filter, &mut s.yaesu2_mem_filter);
            std::mem::swap(&mut s.yaesu_mem_dirty, &mut s.yaesu2_mem_dirty);
            std::mem::swap(&mut s.yaesu_mem_push_deferred, &mut s.yaesu2_mem_push_deferred);
            std::mem::swap(&mut s.yaesu_mem_expect_push, &mut s.yaesu2_mem_expect_push);
            std::mem::swap(&mut s.yaesu_mem_radio_received, &mut s.yaesu2_mem_radio_received);
            std::mem::swap(&mut s.yaesu_mem_blob_hash, &mut s.yaesu2_mem_blob_hash);
            std::mem::swap(&mut s.yaesu_mem_active_ch, &mut s.yaesu2_mem_active_ch);
            std::mem::swap(&mut s.yaesu_mem_active_live, &mut s.yaesu2_mem_active_live);
        };
        swap_in_out(self);
        self.yaesu_mem_active_slot = 1;
        self.render_yaesu_memories(ui);
        self.yaesu_mem_active_slot = 0;
        swap_in_out(self);
    }

    pub(super) fn render_yaesu2_panel(&mut self, ui: &mut egui::Ui) {
        let mode_label = match self.yaesu2_mode {
            0 => "LSB", 1 => "USB", 3 => "CW-L", 4 => "CW-U",
            5 => "FM", 6 => "AM", 7 => "DIGU", 9 => "DIGL", 12 => "C4FM", _ => "?",
        };
        // A/B + V/M at the top (same order as 991A panel) + mode label on the right.
        ui.horizontal(|ui| {
            if ui.add(egui::Button::new(RichText::new("A/B").strong().size(12.0))
                .min_size(egui::vec2(50.0, 22.0))).clicked()
            {
                let _ = self.cmd_tx.send(Command::SetControl(
                    sdr_remote_core::protocol::ControlId::Yaesu2SelectVfo, 2));
            }
            if ui.add(egui::Button::new(RichText::new("V/M").strong().size(12.0))
                .min_size(egui::vec2(50.0, 22.0))).clicked()
            {
                let _ = self.cmd_tx.send(Command::SetControl(
                    sdr_remote_core::protocol::ControlId::Yaesu2SelectVfo, 3));
            }
            ui.separator();
            ui.label(RichText::new(mode_label).size(14.0).color(Color32::from_rgb(255, 170, 40)));
        });
        // VFO / Memory indicator (blue for Memory) - same place + name/freq as 991A.
        if self.yaesu2_vfo_select == 1 {
            let c = Color32::from_rgb(100, 200, 255);
            let found = self.yaesu2_mem_channels.iter()
                .find(|ch| ch.channel_number == self.yaesu2_memory_channel);
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("MEM {:02}", self.yaesu2_memory_channel))
                    .size(14.0).strong().color(c));
                if let Some(ch) = found {
                    ui.label(RichText::new(&ch.name).size(14.0).strong().color(c));
                    ui.label(RichText::new(super::yaesu_memory::format_freq_display(ch.rx_freq_hz))
                        .size(14.0).family(egui::FontFamily::Monospace).color(c));
                }
            });
        } else {
            let (label, c) = if self.yaesu2_split {
                ("VFO  Split", Color32::from_rgb(255, 180, 50))
            } else {
                ("VFO", Color32::from_rgb(100, 255, 100))
            };
            ui.label(RichText::new(label).size(14.0).strong().color(c));
        }
        // A: frequency (scroll/tap-to-tune) + touch-friendly stepper (§16), B: below it.
        ui.horizontal(|ui| {
            ui.label(RichText::new("A:  ").size(16.0).strong());
            if let Some(delta) = render_freq_scroll(ui, self.yaesu2_freq_a) {
                let new_freq = (self.yaesu2_freq_a as i64 + delta).max(0) as u64;
                let _ = self.cmd_tx.send(Command::SetYaesu2Freq(new_freq));
                self.set_pending_yaesu_freq(1, new_freq);
            }
        });
        if let Some(delta) = render_freq_stepper(ui, &mut self.tune_step_hz) {
            let new_freq = (self.yaesu2_freq_a as i64 + delta).max(0) as u64;
            let _ = self.cmd_tx.send(Command::SetYaesu2Freq(new_freq));
            self.set_pending_yaesu_freq(1, new_freq);
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new("B:  ").size(12.0));
            ui.label(RichText::new(format!("{} Hz", format_frequency(self.yaesu2_freq_b)))
                .size(14.0).family(egui::FontFamily::Monospace));
        });
        ui.separator();
        {
            let btn = |text: &str| egui::Button::new(RichText::new(text).size(11.0))
                .min_size(egui::vec2(38.0, 20.0));
            let mode_names = ["LSB", "USB", "CW-L", "CW-U", "FM", "AM", "DIG-U", "DIG-L"];
            let mode_codes: &[u8] = &[0, 1, 3, 4, 5, 6, 7, 9];
            ui.horizontal_wrapped(|ui| {
                ui.label(rust_i18n::t!("dev_mode").to_string());
                for (i, &name) in mode_names.iter().enumerate() {
                    let mb = if mode_codes[i] == self.yaesu2_mode {
                        egui::Button::new(RichText::new(name).size(11.0).color(Color32::WHITE))
                            .fill(Color32::from_rgb(0, 90, 200))
                    } else { btn(name) };
                    if ui.add(mb).clicked() {
                        let _ = self.cmd_tx.send(Command::SetYaesu2Mode(mode_codes[i]));
                    }
                }
            });
        }
        // Band / A=B / Split / Scan / Tune - mirror of the 991A panel, routed
        // to slot 1 (Yaesu2Button). Mem± and V/M = phase 2 (memory).
        {
            use sdr_remote_core::protocol::ControlId;
            let btn = |text: &str| egui::Button::new(RichText::new(text).size(11.0))
                .min_size(egui::vec2(38.0, 20.0));
            ui.horizontal(|ui| {
                if ui.add(btn("Band-")).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button, 6));
                }
                if ui.add(btn("Band+")).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button, 5));
                }
                ui.separator();
                if ui.add(btn("Mem-")).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button, 10));
                }
                if ui.add(btn("Mem+")).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button, 9));
                }
                ui.separator();
                if ui.add(btn("A=B")).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button, 0));
                }
                let split_lbl = rust_i18n::t!("dev_split").to_string();
                let split_btn = if self.yaesu2_split {
                    egui::Button::new(RichText::new(split_lbl.as_str()).size(11.0).color(Color32::WHITE))
                        .fill(Color32::from_rgb(180, 100, 0)).min_size(egui::vec2(38.0, 20.0))
                } else { btn(split_lbl.as_str()) };
                if ui.add(split_btn).clicked() {
                    self.yaesu2_split = !self.yaesu2_split;
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button,
                        if self.yaesu2_split { 7 } else { 8 }));
                }
                let scan_lbl = rust_i18n::t!("dev_scan").to_string();
                let scan_btn = if self.yaesu2_scan {
                    egui::Button::new(RichText::new(scan_lbl.as_str()).size(11.0).color(Color32::WHITE))
                        .fill(Color32::from_rgb(0, 120, 0)).min_size(egui::vec2(38.0, 20.0))
                } else { btn(scan_lbl.as_str()) };
                if ui.add(scan_btn).clicked() {
                    self.yaesu2_scan = !self.yaesu2_scan;
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button,
                        if self.yaesu2_scan { 1 } else { 2 }));
                }
                // Internal ATU (FTX-1): momentary Tune (band-gated <54 MHz) + on/off toggle
                // with real state via AC; poll. Server sends AC003 for tune-start (FTX-1).
                // ATU: HF+6m (<54 MHz) and not in FM (mode 5).
                let atu_avail = self.yaesu2_freq_a >= 1 && self.yaesu2_freq_a < 54_000_000 && !matches!(self.yaesu2_mode, 5 | 12);
                let can_tune = self.yaesu2_connected && atu_avail;
                let tune_lbl = rust_i18n::t!("dev_tune").to_string();
                let tune_btn = if self.yaesu2_tuner_state == 2 {
                    egui::Button::new(RichText::new(tune_lbl.as_str()).size(11.0).color(Color32::WHITE))
                        .fill(Color32::from_rgb(180, 0, 0)).min_size(egui::vec2(38.0, 20.0))
                } else { btn(tune_lbl.as_str()) };
                if ui.add_enabled(can_tune, tune_btn).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button, 3)); // start tuning (momentary)
                }
                let atu_on = self.yaesu2_tuner_state == 1;
                let atu_btn = if atu_on {
                    egui::Button::new(RichText::new("ATU").size(11.0).color(Color32::WHITE))
                        .fill(Color32::from_rgb(0, 90, 200)).min_size(egui::vec2(38.0, 20.0))
                } else { btn("ATU") };
                if ui.add_enabled(atu_avail, atu_btn).clicked() {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button,
                        if atu_on { 4 } else { 15 })); // toggle: off (AC000) / on (AC001)
                }
            });
            // DSP/function controls (PATCH-yaesu-extra-controls) - radio 2.
            let dsp1 = self.yaesu2_feature_toggles;
            let lvl1 = self.yaesu2_feature_levels;
            self.render_yaesu_dsp_block(ui, 1, dsp1, lvl1);
            self.render_yaesu_clarifier_block(ui, 1);
            self.render_yaesu_levels_block(ui, 1);
            // Quick Memory Bank (FTX-1-specific): momentary action buttons.
            // Store = current VFO into QMB (QI;), Recall = cycle through QMB (QR;).
            ui.horizontal(|ui| {
                ui.label("QMB:");
                let store_lbl = rust_i18n::t!("dev_store").to_string();
                if ui.add(btn(store_lbl.as_str()))
                    .on_hover_text(rust_i18n::t!("hover_quick_mem_save").to_string()).clicked()
                {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button, 13));
                }
                let recall_lbl = rust_i18n::t!("dev_recall").to_string();
                if ui.add(btn(recall_lbl.as_str()))
                    .on_hover_text(rust_i18n::t!("dev_recall_hover").to_string()).clicked()
                {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button, 14));
                }
            });
        }
        // Squelch / RF-power / RF-gain sliders - mirror of the 991A panel.
        {
            use sdr_remote_core::protocol::ControlId;
            let slider_w = 120.0;
            egui::Grid::new("yaesu2_sliders").num_columns(4).spacing([4.0, 2.0]).show(ui, |ui| {
                ui.allocate_space(egui::vec2(55.0, 0.0));
                ui.label("SQL");
                let sql = egui::Slider::new(&mut self.yaesu2_squelch, 0..=100)
                    .custom_formatter(|v, _| format!("{:.0}", v));
                let resp = ui.add_sized([slider_w, 16.0], sql);
                let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.yaesu2_squelch, 0..=100, 1.0);
                if resp.changed() || scrolled {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Squelch, self.yaesu2_squelch));
                    self.yaesu2_control_changed_at = Some(std::time::Instant::now());
                }
                ui.label("PWR");
                let pwr_max2 = if self.yaesu2_tx_power_max >= 5 { self.yaesu2_tx_power_max } else { 100 };
                if self.yaesu2_rf_power > pwr_max2 { self.yaesu2_rf_power = pwr_max2; }
                let pwr = egui::Slider::new(&mut self.yaesu2_rf_power, 5..=pwr_max2)
                    .custom_formatter(|v, _| format!("{:.0}W", v));
                let resp = ui.add_sized([slider_w, 16.0], pwr)
                    .on_hover_text(rust_i18n::t!("dev_max_for_band", w = pwr_max2).to_string());
                let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.yaesu2_rf_power, 5..=pwr_max2, 1.0);
                if resp.changed() || scrolled {
                    self.yaesu2_control_changed_at = Some(std::time::Instant::now());
                }
                if scrolled || resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2RfPower, self.yaesu2_rf_power));
                    self.yaesu2_power_pending = Some(self.yaesu2_rf_power);
                    self.yaesu2_power_pending_at = Some(std::time::Instant::now());
                }
                ui.end_row();
                ui.allocate_space(egui::vec2(55.0, 0.0));
                ui.label(rust_i18n::t!("dev_rf_gain").to_string());
                let rf = egui::Slider::new(&mut self.yaesu2_rf_gain, 0..=255)
                    .custom_formatter(|v, _| format!("{:.0}", v));
                let resp = ui.add_sized([slider_w, 16.0], rf);
                let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.yaesu2_rf_gain, 0..=255, 2.0);
                if resp.changed() || scrolled {
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2RfGain, self.yaesu2_rf_gain));
                    self.yaesu2_control_changed_at = Some(std::time::Instant::now());
                }
                ui.end_row();
            });
        }
        ui.separator();
        // S-meter (click toggles bar <-> analog; analog in the same place).
        let mw2 = ui.available_width().min(350.0).max(200.0);
        let mrect2 = if self.meter_analog[super::M_YAESU2] {
            smeter_analog_sized(ui,
                yaesu_raw_to_dbm(self.yaesu2_smeter), yaesu_raw_to_dbm(self.yaesu2_smeter_peak),
                false, false, Some((mw2, 110.0)))
        } else {
            yaesu_smeter_bar(ui, self.yaesu2_smeter, self.yaesu2_smeter_peak)
        };
        self.meter_click(ui, mrect2, super::M_YAESU2);
        ui.separator();
        // Status: RX/TX + power on/off (mirror of the 991A panel).
        ui.horizontal(|ui| {
            let (tx_color, tx_text) = if self.yaesu2_tx_active {
                (Color32::from_rgb(220, 40, 40), "TX")
            } else {
                (Color32::from_rgb(0, 150, 0), "RX")
            };
            ui.colored_label(tx_color, RichText::new(tx_text).size(16.0).strong());
            ui.separator();
            // See slot 0: clickable only on the 991A (standby); otherwise label.
            let on_col = Color32::from_rgb(0, 150, 0);
            let off_col = Color32::from_rgb(90, 90, 90);
            if self.yaesu2_model == 0 {
                let (txt, col): (String, Color32) = if self.yaesu2_power_on { (rust_i18n::t!("dev_power_on_upper").to_string(), on_col) } else { (rust_i18n::t!("dev_standby").to_string(), off_col) };
                if ui.add(egui::Button::new(RichText::new(txt).color(Color32::WHITE)).fill(col))
                    .on_hover_text(rust_i18n::t!("dev_991a_standby_hover").to_string())
                    .clicked()
                {
                    let _ = self.cmd_tx.send(Command::SetControl(
                        sdr_remote_core::protocol::ControlId::Yaesu2PowerOnOff,
                        if self.yaesu2_power_on { 0 } else { 1 }));
                }
            } else {
                ui.colored_label(if self.yaesu2_power_on { on_col } else { off_col },
                    if self.yaesu2_power_on { rust_i18n::t!("dev_power_on_upper").to_string() } else { rust_i18n::t!("dev_power_off_upper").to_string() })
                    .on_hover_text(rust_i18n::t!("dev_radio_powers_off_hover").to_string());
            }
            if self.yaesu2_hi_swr {
                ui.separator();
                ui.colored_label(theme::TL_SWR_ALERT_TEXT,
                    RichText::new(rust_i18n::t!("dev_high_swr").to_string()).size(16.0).strong());
            }
        });
        ui.separator();
        self.render_websdr_controls(ui, CatSyncTarget::Yaesu2, self.yaesu2_freq_a, self.yaesu2_mode);
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("dev_mic_gain").to_string());
            let mut mic_gain_display = super::yaesu_mic_gain_to_display(self.yaesu2_mic_gain);
            let slider = egui::Slider::new(&mut mic_gain_display, 0.05..=1.0)
                .logarithmic(true)
                .custom_formatter(|v, _| format!("{:.2}x", v));
            let resp = ui.add_sized([140.0, 16.0], slider);
            let scrolled = super::helpers::slider_wheel(ui, &resp, &mut mic_gain_display, 0.05..=1.0, 0.02);
            if resp.changed() || scrolled {
                self.yaesu2_mic_gain = super::yaesu_mic_gain_from_display(mic_gain_display);
                let _ = self.cmd_tx.send(Command::SetYaesu2TxGain(self.yaesu2_mic_gain));
            }
            if resp.drag_stopped() || scrolled {
                self.save_ptt_config();
            }
        });
        ui.separator();
        // Equalizer - same shared generic component as the 991A window (slot 1).
        self.render_yaesu_eq(ui, 1);
        ui.separator();
        // Memory Channels - same shared table as the 991A window (via slot 1).
        if super::helpers::chevron_label(ui, self.collapse_yaesu2_memories,
            RichText::new(rust_i18n::t!("dev_memory_channels").to_string()).strong().size(14.0)).clicked()
        {
            self.collapse_yaesu2_memories = !self.collapse_yaesu2_memories;
            self.save_full_config();
        }
        if self.collapse_yaesu2_memories {
            // Scale the list down to the bottom of the window instead of a
            // fixed height: use the remaining available height (with a
            // lower bound so it stays usable in a small window).
            self.render_memories_scroll_and_handle(ui, 1);
        }
        // Radio Settings (EX Menu) - FTX-1 hierarchical. C1: raw address/value list
        // to verify the server scan; C3 turns it into a P1>P2>P3 browser.
        if super::helpers::chevron_label(ui, self.collapse_yaesu2_menu,
            RichText::new(rust_i18n::t!("dev_radio_settings").to_string()).strong().size(14.0)).clicked()
        {
            self.collapse_yaesu2_menu = !self.collapse_yaesu2_menu;
            self.save_full_config();
        }
        if self.collapse_yaesu2_menu {
            ui.indent("yaesu2_menu_body", |ui| self.render_yaesu2_ex_menu(ui));
        }
    }

    /// FTX-1 EX-menu browser (Phase C3). Groups the live-scanned EX values by
    /// group > subgroup with labels from the chart (Table 3); per item a value field
    /// + Set. Addresses/values = ground truth radio; labels = chart (cosmetic).
    pub(super) fn render_yaesu2_ex_menu(&mut self, ui: &mut egui::Ui) {
        use super::ftx1_ex_chart;
        ui.horizontal(|ui| {
            if ui.button(rust_i18n::t!("dev_read_radio").to_string()).clicked() {
                self.yaesu2_menu_received = false;
                let _ = self.cmd_tx.send(Command::SetControl(
                    sdr_remote_core::protocol::ControlId::Yaesu2ReadMenus, 0));
            }
            let n = self.yaesu2_menu_entries.len();
            ui.label(rust_i18n::t!("dev_settings_count", n = n).to_string());
            ui.separator();
            ui.label(rust_i18n::t!("dev_filter").to_string());
            ui.add(egui::TextEdit::singleline(&mut self.yaesu2_menu_filter).desired_width(120.0));
            if !self.yaesu2_menu_filter.is_empty() && ui.button("x").clicked() {
                self.yaesu2_menu_filter.clear();
            }
        });

        // Build the grouped view locally (no self-borrow during render).
        // Item = (addr, group, sub, name+desc, current value).
        let filt = self.yaesu2_menu_filter.to_lowercase();
        let mut groups: Vec<(String, Vec<(String, String, String, String)>)> = Vec::new();
        for (addr, val) in &self.yaesu2_menu_entries {
            let (group, sub, desc) = match ftx1_ex_chart::lookup(addr) {
                Some((g, s, d)) => (g.to_string(), s.to_string(), d.to_string()),
                None => (rust_i18n::t!("dev_other").to_string(), String::new(), addr.clone()),
            };
            if !filt.is_empty()
                && !desc.to_lowercase().contains(&filt)
                && !group.to_lowercase().contains(&filt)
                && !sub.to_lowercase().contains(&filt)
            {
                continue;
            }
            match groups.iter_mut().find(|(g, _)| *g == group) {
                Some((_, items)) => items.push((addr.clone(), sub, desc, val.clone())),
                None => groups.push((group, vec![(addr.clone(), sub, desc, val.clone())])),
            }
        }

        let avail = ui.available_height().max(150.0);
        let mut to_set: Option<(String, String)> = None; // (addr, value)
        egui::ScrollArea::vertical()
            .id_salt("yaesu2_menu_scroll")
            .max_height(avail)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (group, items) in &groups {
                    egui::CollapsingHeader::new(RichText::new(group).strong())
                        .default_open(!filt.is_empty())
                        .show(ui, |ui| {
                            let mut last_sub = String::new();
                            for (addr, sub, desc, val) in items {
                                if sub != &last_sub {
                                    // Subgroup header bold (was .weak() -> nearly unreadable,
                                    // operator feedback build 120). Bold + full contrast.
                                    ui.label(RichText::new(sub).strong());
                                    last_sub = sub.clone();
                                }
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(desc).size(11.0));
                                    let buf = self.yaesu2_menu_edits
                                        .entry(addr.clone()).or_insert_with(|| val.clone());
                                    ui.add(egui::TextEdit::singleline(buf)
                                        .desired_width(70.0)
                                        .font(egui::FontId::monospace(11.0)));
                                    // Only show Set if the buffer differs from the radio value.
                                    if buf.as_str() != val.as_str() && ui.small_button(rust_i18n::t!("dev_set").to_string()).clicked() {
                                        to_set = Some((addr.clone(), buf.clone()));
                                    }
                                });
                            }
                        });
                }
            });

        if let Some((addr, value)) = to_set {
            // Optimistic baseline update: otherwise the radio value is only
            // refreshed on a next "Read radio", which keeps the UI
            // thinking the old value is still in the radio. As a result
            // "Set" no longer appears when you set it back to the original.
            // Only update if the command was actually sent.
            if self.cmd_tx.send(Command::SetYaesu2Menu(addr.clone(), value.clone())).is_ok() {
                if let Some(entry) =
                    self.yaesu2_menu_entries.iter_mut().find(|(a, _)| *a == addr)
                {
                    entry.1 = value;
                }
            }
        }
    }

    pub(super) fn render_yaesu_menu(&mut self, ui: &mut egui::Ui) {
        use super::yaesu_menu;

        ui.horizontal(|ui| {
            if ui.button(rust_i18n::t!("dev_read_radio").to_string()).clicked() {
                self.yaesu_menu_received = false;
                let _ = self.cmd_tx.send(Command::SetControl(
                    sdr_remote_core::protocol::ControlId::YaesuReadMenus, 0));
            }
        });

        if self.yaesu_menu_items.is_empty() {
            ui.label(rust_i18n::t!("dev_click_read_radio_153").to_string());
            return;
        }

        egui::ScrollArea::vertical().max_height(300.0).id_salt("yaesu_menu_scroll").show(ui, |ui| {
            egui::Grid::new("yaesu_menu_grid")
                .striped(true)
                .num_columns(4)
                .min_col_width(30.0)
                .spacing([6.0, 2.0])
                .show(ui, |ui| {
                    ui.label(RichText::new("#").strong());
                    ui.label(RichText::new(rust_i18n::t!("dev_setting").to_string()).strong());
                    ui.label(RichText::new(rust_i18n::t!("dev_value").to_string()).strong());
                    ui.label(RichText::new("").strong());
                    ui.end_row();

                    for item in &mut self.yaesu_menu_items {
                        let def = yaesu_menu::MENU_DEFS.iter()
                            .find(|d| d.number == item.number);

                        let name = def.map_or("?", |d| d.name);
                        let encoding = def.map_or("", |d| d.encoding);

                        // Menu number
                        ui.label(format!("{:03}", item.number));

                        // Name
                        ui.label(name);

                        // Value - read-only, enum dropdown, or text
                        let read_only = def.map_or(false, |d| d.p2_digits == 0);
                        if read_only {
                            ui.label(RichText::new(&item.raw_value).color(Color32::GRAY));
                        } else if yaesu_menu::is_enum(encoding) {
                            let options = yaesu_menu::parse_enum_options(encoding);
                            let display = yaesu_menu::format_value(&item.raw_value, encoding);
                            egui::ComboBox::from_id_salt(format!("exm_{}", item.number))
                                .width(100.0)
                                .selected_text(&display)
                                .show_ui(ui, |ui| {
                                    for (code, label) in &options {
                                        if ui.selectable_label(item.raw_value == *code, label).clicked() {
                                            let _ = self.cmd_tx.send(Command::SetYaesuMenu(item.number, code.clone()));
                                            item.raw_value = code.clone();
                                        }
                                    }
                                });
                        } else {
                            // Numeric value - show as text, editable
                            let resp = ui.add(egui::TextEdit::singleline(&mut item.raw_value)
                                .desired_width(60.0).font(egui::FontId::monospace(11.0)));
                            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                let _ = self.cmd_tx.send(Command::SetYaesuMenu(item.number, item.raw_value.clone()));
                            }
                        }

                        // Default indicator
                        if let Some(d) = def {
                            if item.raw_value == d.default {
                                ui.label("");
                            } else {
                                ui.label(RichText::new("*").color(Color32::from_rgb(255, 180, 50)));
                            }
                        } else {
                            ui.label("");
                        }

                        ui.end_row();
                    }
                });
        });
    }

    pub(super) fn render_yaesu_memories(&mut self, ui: &mut egui::Ui) {
        use super::yaesu_memory;

        ui.horizontal(|ui| {
            // Tones are not part of the memory read: the radio only reports the
            // tone of the channel it is on, so filling the column means stepping
            // through the channels that have a tone mode. Explicit, because it
            // moves the radio.
            if ui.button(rust_i18n::t!("dev_read_tones").to_string())
                .on_hover_text(rust_i18n::t!("dev_read_tones_hover").to_string())
                .clicked()
            {
                self.yaesu_mem_radio_received = false;
                // Asked for on purpose, so the answer must land even with the table
                // open - and reading from the radio replaces the list, which is what
                // the operator just asked for, so an open edit is settled by it.
                self.yaesu_mem_expect_push = true;
                self.yaesu_mem_dirty = false;
                let _ = self.cmd_tx.send(Command::SetControl(
                    sdr_remote_core::protocol::ControlId::YaesuReadMemoryTones,
                    self.yaesu_mem_active_slot as u16,
                ));
            }
            if ui.button(rust_i18n::t!("dev_read_radio").to_string()).clicked() {
                self.yaesu_mem_radio_received = false; // allow processing new data
                self.yaesu_mem_expect_push = true;
                self.yaesu_mem_dirty = false;
                // Route read/write to the active radio (slot 0 = 991A, 1 = FTX-1).
                let cmd = if self.yaesu_mem_active_slot == 0 {
                    sdr_remote_core::protocol::ControlId::YaesuReadMemories
                } else {
                    sdr_remote_core::protocol::ControlId::Yaesu2ReadMemories
                };
                let _ = self.cmd_tx.send(Command::SetControl(cmd, 0));
            }
            if !self.yaesu_mem_channels.is_empty() {
                if ui.button(rust_i18n::t!("dev_write_radio").to_string()).clicked() {
                    let path = std::path::Path::new(&self.yaesu_mem_file);
                    // Save to file first, then send to server for writing
                    let _ = yaesu_memory::save_tab_file(path, &self.yaesu_mem_channels);
                    if let Ok(text) = std::fs::read_to_string(path) {
                        let cmd = if self.yaesu_mem_active_slot == 0 {
                            Command::WriteYaesuMemories(text)
                        } else {
                            Command::WriteYaesu2Memories(text)
                        };
                        if self.cmd_tx.send(cmd).is_ok() {
                            // Written to the radio and to file: the edit is settled, so
                            // the server's list may land again. Only on a successful
                            // dispatch - a failed send leaves the edit open.
                            self.yaesu_mem_dirty = false;
                        }
                    }
                }
            }
            if ui.button(rust_i18n::t!("dev_load_file").to_string()).clicked() {
                let path = std::path::Path::new(&self.yaesu_mem_file);
                match yaesu_memory::parse_tab_file(path) {
                    Ok(ch) => {
                        log::info!("Loaded {} channels from {}", ch.len(), self.yaesu_mem_file);
                        self.yaesu_mem_channels = ch;
                        self.yaesu_mem_dirty = false;
                        self.yaesu_mem_selected = None;
                    }
                    Err(e) => log::warn!("Load failed: {}", e),
                }
            }
            if self.yaesu_mem_dirty {
                if ui.button(rust_i18n::t!("dev_save").to_string()).clicked() {
                    let path = std::path::Path::new(&self.yaesu_mem_file);
                    match yaesu_memory::save_tab_file(path, &self.yaesu_mem_channels) {
                        Ok(()) => {
                            log::info!("Saved {} channels to {}", self.yaesu_mem_channels.len(), self.yaesu_mem_file);
                            self.yaesu_mem_dirty = false;
                        }
                        Err(e) => log::warn!("Save failed: {}", e),
                    }
                }
            }
            if ui.button("+").clicked() {
                let next_ch = self.yaesu_mem_channels.len() as u16 + 1;
                let mut ch = yaesu_memory::YaesuMemoryChannel::default();
                ch.channel_number = next_ch;
                ch.name = format!("CH {}", next_ch);
                self.yaesu_mem_channels.push(ch);
                self.yaesu_mem_dirty = true;
                // Auto-select new channel for editing
                self.yaesu_mem_selected = Some(self.yaesu_mem_channels.len() - 1);
            }
            // Import: copy the entire memory list from the OTHER radio.
            // Due to the mem::swap pattern in render_yaesu_memories_slot the
            // channels of the other radio are always in `yaesu2_mem_channels`,
            // regardless of which slot is currently shown. 991A<->FTX-1 share exactly the same
            // YaesuMemoryChannel + tab columns, so a direct clone suffices;
            // the per-model write function on the server maps the mode codes.
            // Non-destructive: fills the list + marks dirty - only on
            // "Write radio" does it actually go to the radio.
            if !self.yaesu2_mem_channels.is_empty() {
                let from_radio = if self.yaesu_mem_active_slot == 0 { 2 } else { 1 };
                let name = self.yaesu_slot_label(from_radio - 1);
                if ui.button(rust_i18n::t!("dev_import_from", name = name).to_string())
                    .on_hover_text(rust_i18n::t!("hover_take_memories").to_string())
                    .clicked()
                {
                    self.yaesu_mem_channels = self.yaesu2_mem_channels.clone();
                    self.yaesu_mem_dirty = true;
                    self.yaesu_mem_selected = None;
                    log::info!("Imported {} channels from Radio {}",
                        self.yaesu_mem_channels.len(), from_radio);
                }
            }
        });

        // File path
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("dev_file").to_string());
            ui.add(egui::TextEdit::singleline(&mut self.yaesu_mem_file).desired_width(250.0));
        });

        // Filter
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("dev_filter").to_string());
            ui.add(egui::TextEdit::singleline(&mut self.yaesu_mem_filter).desired_width(150.0));
            if !self.yaesu_mem_filter.is_empty() {
                if ui.button("×").clicked() {
                    self.yaesu_mem_filter.clear();
                }
            }
            let n = self.yaesu_mem_channels.len();
            ui.label(rust_i18n::t!("dev_channels_count", n = n).to_string());
        });

        if self.yaesu_mem_channels.is_empty() {
            ui.label(rust_i18n::t!("dev_no_channels_loaded").to_string());
            return;
        }

        // Channel table
        let filter_lower = self.yaesu_mem_filter.to_lowercase();
        let selected = self.yaesu_mem_selected;
        let mut tune_action: Option<(u64, u8, u16)> = None; // (freq, mode, channel#)
        let mut close_edit = false;

        // Use a horizontal layout so the table can exceed the viewport width
        egui::ScrollArea::both().show(ui, |ui| {
            let header_style = |t: &str| RichText::new(t).strong().size(11.0);

            egui::Grid::new("yaesu_mem_grid")
                .striped(true)
                .min_col_width(28.0)
                .spacing([6.0, 3.0])
                .num_columns(17)
                .show(ui, |ui| {
                    // Header row
                    let headers: [String; 17] = [
                        "CH".to_string(),
                        rust_i18n::t!("dev_name").to_string(),
                        "RX Freq".to_string(),
                        rust_i18n::t!("dev_mode_col").to_string(),
                        "Dir".to_string(),
                        rust_i18n::t!("dev_offset").to_string(),
                        rust_i18n::t!("dev_tone").to_string(),
                        "CTCSS/DCS".to_string(),
                        "AGC".to_string(),
                        "NB".to_string(),
                        "DNR".to_string(),
                        "IPO".to_string(),
                        "ATT".to_string(),
                        rust_i18n::t!("dev_tuner_col").to_string(),
                        rust_i18n::t!("dev_skip").to_string(),
                        rust_i18n::t!("dev_step").to_string(),
                        String::new(),
                    ];
                    for h in &headers {
                        ui.label(header_style(h.as_str()));
                    }
                    ui.end_row();

                    for idx in 0..self.yaesu_mem_channels.len() {
                        if !filter_lower.is_empty() {
                            let name_lower = self.yaesu_mem_channels[idx].name.to_lowercase();
                            if !name_lower.contains(&filter_lower) {
                                continue;
                            }
                        }

                        let is_selected = selected == Some(idx);

                        if is_selected {
                            // --- Editing mode ---
                            let ch = &mut self.yaesu_mem_channels[idx];
                            let hi = Color32::from_rgb(255, 220, 100);

                            ui.label(RichText::new(format!("{}", ch.channel_number)).color(hi));

                            if ui.add(egui::TextEdit::singleline(&mut ch.name).desired_width(90.0)).changed() {
                                self.yaesu_mem_dirty = true;
                            }

                            // RX Freq
                            let mut freq_str = format!("{:.5}", ch.rx_freq_hz as f64 / 1_000_000.0);
                            if ui.add(egui::TextEdit::singleline(&mut freq_str)
                                .desired_width(85.0).font(egui::FontId::monospace(11.0))).changed() {
                                if let Ok(mhz) = freq_str.trim().replace(',', ".").parse::<f64>() {
                                    let hz = (mhz * 1_000_000.0).round() as u64;
                                    if hz >= 100_000 && hz <= 500_000_000 {
                                        ch.rx_freq_hz = hz;
                                        self.yaesu_mem_dirty = true;
                                    }
                                }
                            }

                            // Mode
                            egui::ComboBox::from_id_salt(format!("mm_{}", idx))
                                .width(65.0).selected_text(&ch.mode)
                                .show_ui(ui, |ui| {
                                    for &m in yaesu_memory::MODES {
                                        if ui.selectable_label(ch.mode == m, m).clicked() {
                                            ch.mode = m.to_string(); ch.tx_mode = m.to_string();
                                            self.yaesu_mem_dirty = true;
                                        }
                                    }
                                });

                            // Offset direction
                            egui::ComboBox::from_id_salt(format!("md_{}", idx))
                                .width(55.0).selected_text(&ch.offset_direction)
                                .show_ui(ui, |ui| {
                                    for &d in yaesu_memory::OFFSET_DIRS {
                                        if ui.selectable_label(ch.offset_direction == d, d).clicked() {
                                            ch.offset_direction = d.to_string();
                                            if d == "Simplex" {
                                                ch.offset_freq.clear();
                                            }
                                            // No default is invented for the other
                                            // directions: the amount is not ours to set,
                                            // it comes from the radio's per-band menu.
                                            self.yaesu_mem_dirty = true;
                                        }
                                    }
                                });

                            // Offset frequency: shown, never chosen. Neither radio stores
                            // a shift AMOUNT per memory channel - the record holds only the
                            // direction, and the size is a menu setting per band (FT-991A
                            // 80-83, FTX-1 EX 010316-010319). This was a dropdown of ten
                            // fixed values that belonged to neither radio and that the
                            // server never read back when writing: a choice that went
                            // nowhere. The value here is derived from transmit minus
                            // receive frequency, which is what the radio actually reports.
                            ui.label(
                                RichText::new(if ch.offset_freq.is_empty() { "-" } else { &ch.offset_freq })
                                    .color(Color32::from_rgb(200, 200, 200)),
                            )
                            .on_hover_text(rust_i18n::t!("dev_mem_offset_readonly").to_string());

                            // Tone mode
                            egui::ComboBox::from_id_salt(format!("mt_{}", idx))
                                .width(55.0).selected_text(if ch.tone_mode == "None" { "-" } else { &ch.tone_mode })
                                .show_ui(ui, |ui| {
                                    for &t in yaesu_memory::TONE_MODES {
                                        let l = if t == "None" { "-" } else { t };
                                        if ui.selectable_label(ch.tone_mode == t, l).clicked() {
                                            ch.tone_mode = t.to_string(); self.yaesu_mem_dirty = true;
                                        }
                                    }
                                });

                            // CTCSS tone: editable, because "Write to radio" now
                            // stores it through CN (MT cannot carry it - P9 is
                            // fixed). Only where the mode actually uses a tone.
                            let uses_tone = yaesu_memory::mem_mode_uses_tone(&ch.mode);
                            let is_ctcss = matches!(ch.tone_mode.as_str(), "Tone" | "Tone ENC" | "T SQL");
                            let is_dcs = matches!(ch.tone_mode.as_str(), "DCS" | "DCS ENC" | "D Code");
                            if uses_tone && (is_ctcss || is_dcs) {
                                // One column, two tables: a CTCSS frequency or a DCS
                                // code, whichever this channel's tone mode calls for.
                                // Both go to the radio through CN, which selects the
                                // table itself - so the editor must not offer the wrong one.
                                let (cur, options): (String, &[&str]) = if is_dcs {
                                    (yaesu_memory::mem_text(&ch.dcs), yaesu_memory::DCS_CODES)
                                } else {
                                    (yaesu_memory::mem_text(&ch.ctcss), yaesu_memory::CTCSS_TONES)
                                };
                                egui::ComboBox::from_id_salt(format!("mc_{}", idx))
                                    .width(70.0)
                                    .selected_text(cur)
                                    .show_ui(ui, |ui| {
                                        for &c in options {
                                            let selected = if is_dcs { ch.dcs == c } else { ch.ctcss == c };
                                            if ui.selectable_label(selected, c).clicked() {
                                                if is_dcs { ch.dcs = c.to_string(); } else { ch.ctcss = c.to_string(); }
                                                self.yaesu_mem_dirty = true;
                                            }
                                        }
                                    })
                                    .response
                                    .on_hover_text(rust_i18n::t!("dev_mem_tone_write_hint").to_string());
                            } else {
                                ui.label(RichText::new(yaesu_memory::mem_tone_value(ch))
                                    .color(Color32::from_rgb(150, 150, 150)));
                            }

                            // The rest stays read-only: the radio's memory write
                            // stores only name, frequency, mode, shift direction
                            // and tone mode, so an AGC/NB/DNR/IPO/ATT/tuner/skip/
                            // step picked here would never reach it. Shown exactly
                            // as read - "-" where unknown.
                            let ro = Color32::from_rgb(150, 150, 150);
                            let unknown = rust_i18n::t!("dev_mem_field_not_stored").to_string();
                            let mut ro_label = |txt: String| {
                                ui.add(egui::Label::new(RichText::new(txt).color(ro)))
                                    .on_hover_text(&unknown);
                            };
                            ro_label(yaesu_memory::mem_text(&ch.agc));
                            ro_label(yaesu_memory::mem_flag(ch.noise_blanker));
                            ro_label(yaesu_memory::mem_text(&ch.dnr));
                            ro_label(yaesu_memory::mem_text(&ch.ipo));
                            ro_label(yaesu_memory::mem_flag(ch.attenuator));
                            ro_label(yaesu_memory::mem_flag(ch.tuner));
                            ro_label(yaesu_memory::mem_flag(ch.skip));
                            ro_label(yaesu_memory::mem_text(&ch.step));

                            // One action, because the other two earned their way out.
                            // "Tune" did what a single click on the row already does, and
                            // the delete could not delete anything: a memory channel cannot
                            // be erased over CAT on either radio, so all it did was drop the
                            // row from this list while the channel stayed in the set - a
                            // button that reads as destructive and is not, sitting one slip
                            // away from the way out.
                            ui.horizontal(|ui| {
                                if ui.small_button(rust_i18n::t!("dev_mem_close_edit").to_string())
                                    .on_hover_text(rust_i18n::t!("dev_mem_close_edit_hover").to_string())
                                    .clicked()
                                {
                                    close_edit = true;
                                }
                            });

                            ui.end_row();
                        } else {
                            // --- Display mode ---
                            let ch = &self.yaesu_mem_channels[idx];
                            // Colour says where the RADIO is, which is not the same thing as
                            // which row you last touched:
                            //   green   the radio is on this channel now
                            //   amber   the last channel you used, but you have since tuned
                            //           away into VFO - click it to come back
                            //   grey    everything else
                            // Without the amber state the row you left stayed lit as though
                            // the radio were still there.
                            let is_here = self.yaesu_mem_active_ch == Some(ch.channel_number);
                            let c = if is_here && self.yaesu_mem_active_live {
                                Color32::from_rgb(120, 230, 140)
                            } else if is_here {
                                Color32::from_rgb(230, 180, 90)
                            } else {
                                Color32::from_rgb(200, 200, 200)
                            };

                            // Clicking the channel number recalls it - and ONLY recalls it.
                            // It used to open the row editor at the same time, which turned
                            // the row into a form: the one row you most wanted to click
                            // again was the one row that no longer had anything to click.
                            // Coming back to the channel you had just left meant going to
                            // another one first. The editor has its own button, at the end
                            // of the row.
                            // The whole row recalls, not just the number. A one-character
                            // target for the action you take most often is a target you
                            // miss; every cell that only displays something now carries the
                            // click. The Edit and delete buttons at the end keep their own.
                            let hint = rust_i18n::t!("dev_mem_click_to_recall").to_string();
                            let mut recall_click = false;
                            let mut edit_click = false;
                            let mut cell = |ui: &mut egui::Ui, t: RichText| {
                                let r = ui
                                    .add(egui::Label::new(t).sense(egui::Sense::click()))
                                    .on_hover_text(&hint);
                                // Double click opens the row editor. Single click recalls,
                                // which is the frequent action and keeps the light target;
                                // the Edit button at the end of the row still works, but it
                                // is a long way right on a twenty-column table.
                                if r.double_clicked() {
                                    edit_click = true;
                                } else if r.clicked() {
                                    recall_click = true;
                                }
                            };

                            cell(ui, RichText::new(format!("{}", ch.channel_number)).color(c));
                            cell(ui, RichText::new(&ch.name).color(c).strong());
                            cell(ui, RichText::new(yaesu_memory::format_freq_display(ch.rx_freq_hz)).color(c).family(egui::FontFamily::Monospace));
                            cell(ui, RichText::new(&ch.mode).color(c));

                            // Dir
                            let dir_text = match ch.offset_direction.as_str() {
                                "Simplex" => "S", "Plus" => "+", "Minus" => "-",
                                _ => &ch.offset_direction,
                            };
                            cell(ui, RichText::new(dir_text).color(c));

                            // Offset freq
                            cell(ui, RichText::new(yaesu_memory::mem_text(&ch.offset_freq)).color(c));

                            cell(ui, RichText::new(yaesu_memory::mem_tone_mode(ch)).color(c));
                            cell(ui, RichText::new(yaesu_memory::mem_tone_value(ch)).color(c));

                            // "-" here means the radio does not report this field
                            // over CAT (or it does not apply in this mode) - it is
                            // not the same as "off". Hover explains it.
                            let not_stored = rust_i18n::t!("dev_mem_field_not_stored").to_string();
                            let mut ro_label = |txt: String| {
                                if ui
                                    .add(egui::Label::new(RichText::new(txt).color(c)).sense(egui::Sense::click()))
                                    .on_hover_text(&not_stored)
                                    .clicked()
                                {
                                    recall_click = true;
                                }
                            };
                            ro_label(yaesu_memory::mem_text(&ch.agc));
                            ro_label(yaesu_memory::mem_flag(ch.noise_blanker));
                            ro_label(yaesu_memory::mem_text(&ch.dnr));
                            ro_label(yaesu_memory::mem_text(&ch.ipo));
                            ro_label(yaesu_memory::mem_flag(ch.attenuator));
                            ro_label(yaesu_memory::mem_flag(ch.tuner));
                            ro_label(yaesu_memory::mem_flag(ch.skip));
                            ro_label(yaesu_memory::mem_text(&ch.step));

                            if ui.small_button(rust_i18n::t!("dev_edit").to_string()).clicked() {
                                self.yaesu_mem_selected = Some(idx);
                            }

                            if edit_click {
                                self.yaesu_mem_selected = Some(idx);
                            } else if recall_click {
                                tune_action = Some((ch.rx_freq_hz, yaesu_memory::mode_string_to_internal(&ch.mode), ch.channel_number));
                                // Optimistic, so the row turns green under the click instead
                                // of after the radio has confirmed; sync corrects it.
                                self.yaesu_mem_active_ch = Some(ch.channel_number);
                                self.yaesu_mem_active_live = true;
                            }

                            ui.end_row();
                        }
                    }
                });
        });

        // Execute deferred actions: recall memory channel only.
        // FM -> DATA-FM switch happens transparently at PTT time (server-side).
        if let Some((_freq, _mode, ch_num)) = tune_action {
            // Route to the radio whose memory table is on screen. This used to
            // send the slot-0 control unconditionally, so clicking a channel in
            // radio 2's list recalled it on radio 1 - radio 2 simply never moved.
            // Same slot flag the write path already uses.
            let recall = if self.yaesu_mem_active_slot == 0 {
                sdr_remote_core::protocol::ControlId::YaesuRecallMemory
            } else {
                sdr_remote_core::protocol::ControlId::Yaesu2RecallMemory
            };
            let _ = self.cmd_tx.send(Command::SetControl(recall, ch_num));
            self.yaesu_in_memory_mode = true;
            // `yaesu_current_mem_ch` is set at the click; the edit selection has
            // nothing to do with where the radio is any more.
        }
        if close_edit {
            self.yaesu_mem_selected = None;
        }
    }

    pub(super) fn yaesu_compact_mode_label(mode: u8) -> &'static str {
        match mode {
            0 => "LSB",
            1 => "USB",
            3 => "CW-L",
            4 => "CW-U",
            5 => "FM",
            6 => "AM",
            7 => "DIGU",
            9 => "DIGL",
            12 => "C4FM",
            _ => "?",
        }
    }

    pub(super) fn yaesu_compact_smeter_label(raw: u16) -> String {
        let raw = raw as f32;
        if raw <= 108.0 {
            let s_unit = (raw / 12.0).round().clamp(0.0, 9.0) as u8;
            format!("S{}", s_unit)
        } else {
            let db_over = ((raw - 108.0) * 0.5).round().max(0.0) as i32;
            format!("S9+{} dB", db_over)
        }
    }

    pub(super) fn render_yaesu_compact_status(&mut self, ui: &mut egui::Ui, slot: u8) {
        let (freq_a, freq_b, mode, power_on, tx_active, smeter, hi_swr, target) = if slot == 0 {
            (
                self.yaesu_freq_a,
                self.yaesu_freq_b,
                self.yaesu_mode,
                self.yaesu_power_on,
                self.yaesu_tx_active,
                self.yaesu_smeter,
                self.yaesu_hi_swr,
                CatSyncTarget::Yaesu1,
            )
        } else {
            (
                self.yaesu2_freq_a,
                self.yaesu2_freq_b,
                self.yaesu2_mode,
                self.yaesu2_power_on,
                self.yaesu2_tx_active,
                self.yaesu2_smeter,
                self.yaesu2_hi_swr,
                CatSyncTarget::Yaesu2,
            )
        };

        egui::Grid::new(format!("yaesu_compact_grid_{}", slot))
            .num_columns(2)
            .spacing([20.0, 6.0])
            .show(ui, |ui| {
                ui.label("VFO A:");
                ui.label(RichText::new(format!("{} Hz", format_frequency(freq_a)))
                    .size(18.0).strong());
                ui.end_row();

                ui.label("VFO B:");
                ui.label(RichText::new(format!("{} Hz", format_frequency(freq_b)))
                    .size(14.0));
                ui.end_row();

                ui.label(rust_i18n::t!("dev_mode").to_string());
                ui.label(RichText::new(Self::yaesu_compact_mode_label(mode)).size(14.0).strong());
                ui.end_row();

                ui.label(rust_i18n::t!("dev_power_colon").to_string());
                ui.label(if power_on { rust_i18n::t!("dev_on_upper").to_string() } else { rust_i18n::t!("dev_off_upper").to_string() });
                ui.end_row();

                ui.label("TX:");
                ui.horizontal(|ui| {
                    ui.label(if tx_active {
                        RichText::new("TX").color(Color32::RED).strong()
                    } else {
                        RichText::new("RX").color(Color32::GREEN)
                    });
                    if hi_swr {
                        ui.label(RichText::new(rust_i18n::t!("dev_high_swr").to_string())
                            .color(theme::TL_SWR_ALERT_TEXT).strong());
                    }
                });
                ui.end_row();

                ui.label("S-Meter:");
                ui.label(RichText::new(Self::yaesu_compact_smeter_label(smeter)).strong());
                ui.end_row();

                ui.label(rust_i18n::t!("dev_audio_colon").to_string());
                if slot == 0 {
                    let slider = egui::Slider::new(&mut self.yaesu_volume, 0.001..=1.0)
                        .logarithmic(true)
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
                    let resp = ui.add_sized([180.0, 16.0], slider);
                    let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.yaesu_volume, 0.001..=1.0, 0.02);
                    if resp.changed() || scrolled {
                        let _ = self.cmd_tx.send(Command::SetYaesuVolume(self.yaesu_volume));
                    }
                } else {
                    let slider = egui::Slider::new(&mut self.yaesu2_volume, 0.001..=1.0)
                        .logarithmic(true)
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
                    let resp = ui.add_sized([180.0, 16.0], slider);
                    let scrolled = super::helpers::slider_wheel(ui, &resp, &mut self.yaesu2_volume, 0.001..=1.0, 0.02);
                    if resp.changed() || scrolled {
                        let _ = self.cmd_tx.send(Command::SetYaesu2Volume(self.yaesu2_volume));
                    }
                }
                ui.end_row();
            });

        ui.separator();
        self.render_websdr_controls(ui, target, freq_a, mode);
    }

    pub(super) fn render_device_yaesu(&mut self, ui: &mut egui::Ui, _amber: Color32) {
        let show_radio1 = self.yaesu_connected || self.yaesu_enabled;
        let show_radio2 = self.yaesu2_connected || self.yaesu2_enabled;

        if show_radio1 {
            ui.horizontal(|ui| {
                ui.heading(self.yaesu_slot_label(0));
                ui.separator();
                if ui.checkbox(&mut self.yaesu_enabled, rust_i18n::t!("dev_enable").to_string()).changed() {
                    let _ = self.cmd_tx.send(Command::SetControl(
                        sdr_remote_core::protocol::ControlId::YaesuEnable, self.yaesu_enabled as u16));
                }
                ui.separator();
                if self.yaesu_enabled {
                    let popout_label = if self.yaesu_popout { rust_i18n::t!("dev_close_window").to_string() } else { rust_i18n::t!("dev_open_window").to_string() };
                    if ui.button(popout_label).clicked() {
                        self.yaesu_popout = !self.yaesu_popout;
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("PTT:");
                if ui.selectable_label(!self.yaesu_ptt_toggle_mode, rust_i18n::t!("dev_push_to_talk").to_string()).clicked() {
                    self.yaesu_ptt_toggle_mode = false;
                    self.save_ptt_config();
                }
                if ui.selectable_label(self.yaesu_ptt_toggle_mode, rust_i18n::t!("dev_toggle").to_string()).clicked() {
                    self.yaesu_ptt_toggle_mode = true;
                    self.save_ptt_config();
                }
            });
            ui.separator();
            self.render_yaesu_compact_status(ui, 0);
        }

        if show_radio2 {
            if show_radio1 {
                ui.separator();
            }
            ui.horizontal(|ui| {
                ui.heading(self.yaesu_slot_label(1));
                ui.separator();
                if ui.checkbox(&mut self.yaesu2_enabled, rust_i18n::t!("dev_enable").to_string()).changed() {
                    let _ = self.cmd_tx.send(Command::SetYaesu2Enable(self.yaesu2_enabled));
                    if !self.yaesu2_enabled {
                        self.yaesu2_popout = false;
                    }
                    self.save_ptt_config();
                }
                ui.separator();
                if self.yaesu2_enabled {
                    let popout_label = if self.yaesu2_popout { rust_i18n::t!("dev_close_window").to_string() } else { rust_i18n::t!("dev_open_window").to_string() };
                    if ui.button(popout_label).clicked() {
                        self.yaesu2_popout = !self.yaesu2_popout;
                        self.save_ptt_config();
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("PTT:");
                if ui.selectable_label(!self.yaesu2_ptt_toggle_mode, rust_i18n::t!("dev_push_to_talk").to_string()).clicked() {
                    self.yaesu2_ptt_toggle_mode = false;
                    self.save_ptt_config();
                }
                if ui.selectable_label(self.yaesu2_ptt_toggle_mode, rust_i18n::t!("dev_toggle").to_string()).clicked() {
                    self.yaesu2_ptt_toggle_mode = true;
                    self.save_ptt_config();
                }
            });
            ui.separator();
            self.render_yaesu_compact_status(ui, 1);
        } else {
            self.yaesu2_popout = false;
        }
    }
}
