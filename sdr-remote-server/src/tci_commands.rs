// SPDX-License-Identifier: GPL-2.0-or-later

#![allow(dead_code)]
//! TCI command setters for TciConnection.
//! Each method formats a TCI command string, sends it via WebSocket,
//! and updates the local state optimistically.

use log::info;
use crate::tci::TciConnection;
use crate::tci_parser::mode_u8_to_str;

impl TciConnection {
    pub async fn set_vfo_a_freq(&mut self, hz: u64) {
        let cmd = format!("VFO:0,0,{};", hz);
        info!("TCI: set VFO A = {} Hz", hz);
        self.send(&cmd).await;
    }

    pub async fn set_vfo_a_mode(&mut self, mode: u8) {
        let mode_str = mode_u8_to_str(mode);
        let cmd = format!("MODULATION:0,{};", mode_str);
        info!("TCI: set VFO A mode = {} ({})", mode_str, mode);
        self.send(&cmd).await;
    }

    pub async fn set_power(&mut self, on: bool) {
        let cmd = if on { "START;" } else { "STOP;" };
        info!("TCI: Power {} ({})", if on { "ON" } else { "OFF" }, cmd);
        self.send(cmd).await;
    }

    pub async fn set_tx_profile(&mut self, idx: u8) {
        if !self.has_cap("tx_profiles_ex") && !self.has_extensions() {
            info!("TCI: TX profile selection skipped (tx_profiles_ex cap not available)");
            return;
        }
        if let Some(name) = self.tx_profile_names.get(idx as usize) {
            let safe_name = name.replace([',', ';'], "");
            let cmd = format!("tx_profile_ex:{};", safe_name);
            info!("TCI: set TX profile = \"{}\" (index {})", safe_name, idx);
            self.send(&cmd).await;
        }
    }

    pub async fn set_nr(&mut self, level: u8) {
        if self.has_cap("rx_nr_enable_ex") || self.has_extensions() {
            // Extended: NR1-4 level selection (specific cap or fork extensions)
            if level == 0 {
                self.send("rx_nr_enable_ex:0,false,1;").await;
            } else {
                let cmd = format!("rx_nr_enable_ex:0,true,{};", level);
                self.send(&cmd).await;
            }
        } else {
            // Standard TCI: on/off only, no level selection
            let on = level > 0;
            let cmd = format!("rx_nr_enable:0,{};", if on { "true" } else { "false" });
            self.send(&cmd).await;
        }
        self.nr_level = level;
    }

    pub async fn set_anf(&mut self, on: bool) {
        let cmd = format!("RX_ANF_ENABLE:0,{};", if on { "true" } else { "false" });
        self.send(&cmd).await;
    }

    pub async fn set_drive(&mut self, level: u8) {
        let level = level.min(100);
        let cmd = format!("DRIVE:0,{};", level);
        info!("TCI: Drive = {}%", level);
        self.send(&cmd).await;
    }

    pub async fn set_filter(&mut self, low_hz: i32, high_hz: i32) {
        let cmd = format!("RX_FILTER_BAND:0,{},{};", low_hz, high_hz);
        info!("TCI: Filter = {} .. {} Hz", low_hz, high_hz);
        self.send(&cmd).await;
    }

    pub async fn set_vfo_b_freq(&mut self, hz: u64) {
        // TCI: receiver 0 channel 1, or receiver 1 channel 0 depending on Thetis config
        let cmd = format!("VFO:0,1,{};", hz);
        info!("TCI: set VFO B = {} Hz", hz);
        self.send(&cmd).await;
    }

    pub async fn set_vfo_b_mode(&mut self, mode: u8) {
        let mode_str = mode_u8_to_str(mode);
        let cmd = format!("MODULATION:1,{};", mode_str);
        info!("TCI: set VFO B mode = {} ({})", mode_str, mode);
        self.send(&cmd).await;
    }

    pub async fn vfo_swap(&mut self) {
        if self.has_cap("vfo_swap_ex") {
            // Extensions: use Thetis native swap (swaps freq, mode, filter, etc.)
            self.send("vfo_swap_ex;").await;
        } else {
            // Fallback: swap frequencies only
            let a = self.vfo_a_freq;
            let b = self.vfo_b_freq;
            if a != 0 && b != 0 {
                self.set_vfo_a_freq(b).await;
                self.set_vfo_b_freq(a).await;
            }
        }
    }

    pub async fn set_rx2_af_gain(&mut self, level: u8) {
        // rx_volume supported since Thetis v2.10.3.13
        let db = (level as i32) - 100; // 0-100 → -100..0 dB
        let cmd = format!("rx_volume:1,0,{};", db);
        self.send(&cmd).await;
        self.rx2_af_gain = level;
    }

    pub async fn set_rx2_nr(&mut self, level: u8) {
        if self.has_cap("rx_nr_enable_ex") || self.has_extensions() {
            if level == 0 {
                self.send("rx_nr_enable_ex:1,false,1;").await;
            } else {
                let cmd = format!("rx_nr_enable_ex:1,true,{};", level);
                self.send(&cmd).await;
            }
        } else {
            let on = level > 0;
            let cmd = format!("rx_nr_enable:1,{};", if on { "true" } else { "false" });
            self.send(&cmd).await;
        }
        self.rx2_nr_level = level;
    }

    pub async fn set_rx2_anf(&mut self, on: bool) {
        let cmd = format!("RX_ANF_ENABLE:1,{};", if on { "true" } else { "false" });
        self.send(&cmd).await;
    }

    pub async fn set_rx2_filter(&mut self, low_hz: i32, high_hz: i32) {
        let cmd = format!("RX_FILTER_BAND:1,{},{};", low_hz, high_hz);
        info!("TCI: RX2 Filter = {} .. {} Hz", low_hz, high_hz);
        self.send(&cmd).await;
    }

    pub async fn set_mon(&mut self, on: bool) {
        let cmd = format!("MON_ENABLE:{};", if on { "true" } else { "false" });
        info!("TCI: MON {}", if on { "ON" } else { "OFF" });
        self.send(&cmd).await;
    }

    pub async fn set_vfo_sync_cat(&mut self, _on: bool) {
        // Legacy stub — VFO Sync now handled via TCI _ex command (see extended section)
    }

    pub async fn set_agc_mode(&mut self, mode: u8) {
        let mode_str = match mode {
            0 => "off", 1 => "long", 2 => "slow", 3 => "normal", 4 => "fast", 5 => "custom",
            _ => "normal",
        };
        let cmd = format!("agc_mode:0,{};", mode_str);
        self.send(&cmd).await;
        self.agc_mode = mode;
    }

    pub async fn set_agc_gain(&mut self, gain: u8) {
        let cmd = format!("agc_gain:0,{};", gain);
        self.send(&cmd).await;
        self.agc_gain = gain;
    }

    pub async fn set_rit_enable(&mut self, on: bool) {
        let cmd = format!("rit_enable:0,{};", if on { "true" } else { "false" });
        self.send(&cmd).await;
        self.rit_enable = on;
    }

    pub async fn set_rit_offset(&mut self, hz: i32) {
        let cmd = format!("rit_offset:0,{};", hz);
        self.send(&cmd).await;
        self.rit_offset = hz;
    }

    pub async fn set_xit_enable(&mut self, on: bool) {
        let cmd = format!("xit_enable:0,{};", if on { "true" } else { "false" });
        self.send(&cmd).await;
        self.xit_enable = on;
    }

    pub async fn set_xit_offset(&mut self, hz: i32) {
        let cmd = format!("xit_offset:0,{};", hz);
        self.send(&cmd).await;
        self.xit_offset = hz;
    }

    pub async fn set_sql_enable(&mut self, on: bool) {
        let cmd = format!("sql_enable:0,{};", if on { "true" } else { "false" });
        self.send(&cmd).await;
        self.sql_enable = on;
    }

    pub async fn set_sql_level(&mut self, level: i16) {
        let cmd = format!("sql_level:0,{};", level);
        self.send(&cmd).await;
        self.sql_level = level as u8;
    }

    pub async fn set_nb_enable(&mut self, on: bool) {
        let cmd = format!("rx_nb_enable:0,{};", if on { "true" } else { "false" });
        self.send(&cmd).await;
        self.nb_enable = on;
    }

    /// Set NB level: 0=off, 1=NB1, 2=NB2
    pub async fn set_nb(&mut self, level: u8) {
        if self.has_cap("rx_nb_enable_ex") || self.has_extensions() {
            // Extended: NB1/NB2 level selection.
            // In het _ex-pad bepaalt het `level`-argument de uiteindelijke NB-stand
            // bij de server; de `enabled` flag wordt daar niet gebruikt om het
            // level te resetten. Bij disable moeten we dus `level=0` sturen in
            // plaats van `level.max(1)`, anders blijft NB1 actief en werkt de
            // cycle→off transitie niet.
            let enabled = level > 0;
            let cmd = format!("rx_nb_enable_ex:0,{},{};", if enabled { "true" } else { "false" }, level);
            self.send(&cmd).await;
            self.nb_enable = enabled;
            self.nb_level = level;
        } else {
            // Standard TCI: on/off only. Level ≥2 degrades to NB1 (better than silent fail).
            let actual = level.min(1);
            let enabled = actual > 0;
            let cmd = format!("rx_nb_enable:0,{};", if enabled { "true" } else { "false" });
            self.send(&cmd).await;
            self.nb_enable = enabled;
            self.nb_level = actual;
        }
    }

    pub async fn set_cw_keyer_speed(&mut self, wpm: u8) {
        let cmd = format!("cw_keyer_speed:{};", wpm);
        self.send(&cmd).await;
        self.cw_keyer_speed = wpm;
    }

    pub async fn cw_key(&mut self, pressed: bool, duration_ms: Option<u16>) {
        let cmd = match duration_ms {
            Some(ms) => format!("keyer:0,{},{};", if pressed { "true" } else { "false" }, ms),
            None => format!("keyer:0,{};", if pressed { "true" } else { "false" }),
        };
        info!("TCI: CW key {} dur={:?}", if pressed { "DOWN" } else { "UP" }, duration_ms);
        self.send(&cmd).await;
    }

    pub async fn cw_macro_stop(&mut self) {
        info!("TCI: CW macro stop");
        self.send("cw_macros_stop;").await;
    }

    pub async fn set_vfo_lock(&mut self, on: bool) {
        let cmd = format!("vfo_lock:0,0,{};", if on { "true" } else { "false" });
        self.send(&cmd).await;
        self.vfo_lock = on;
    }

    pub async fn set_binaural(&mut self, on: bool) {
        let cmd = format!("rx_bin_enable:0,{};", if on { "true" } else { "false" });
        self.send(&cmd).await;
        // Switch TCI audio channels: stereo for binaural, mono otherwise
        let ch_cmd = format!("AUDIO_STREAM_CHANNELS:{};", if on { 2 } else { 1 });
        self.send(&ch_cmd).await;
        info!("TCI: binaural {} → audio channels {}", if on { "ON" } else { "OFF" }, if on { 2 } else { 1 });
        self.binaural = on;
    }

    pub async fn set_apf_enable(&mut self, on: bool) {
        let cmd = format!("rx_apf_enable:0,{};", if on { "true" } else { "false" });
        self.send(&cmd).await;
        self.apf_enable = on;
    }

    pub async fn set_rx2_agc_mode(&mut self, mode: u8) {
        let mode_str = match mode {
            0 => "off", 1 => "long", 2 => "slow", 3 => "normal", 4 => "fast", 5 => "custom",
            _ => "normal",
        };
        let cmd = format!("agc_mode:1,{};", mode_str);
        self.send(&cmd).await;
        self.rx2_agc_mode = mode;
    }

    pub async fn set_rx2_agc_gain(&mut self, gain: u8) {
        let cmd = format!("agc_gain:1,{};", gain);
        self.send(&cmd).await;
        self.rx2_agc_gain = gain;
    }

    pub async fn set_rx2_sql_enable(&mut self, on: bool) {
        let cmd = format!("sql_enable:1,{};", if on { "true" } else { "false" });
        self.send(&cmd).await;
        self.rx2_sql_enable = on;
    }

    pub async fn set_rx2_sql_level(&mut self, level: i16) {
        let cmd = format!("sql_level:1,{};", level);
        self.send(&cmd).await;
        self.rx2_sql_level = level as u8;
    }

    pub async fn set_rx2_nb_enable(&mut self, on: bool) {
        let cmd = format!("rx_nb_enable:1,{};", if on { "true" } else { "false" });
        self.send(&cmd).await;
        self.rx2_nb_enable = on;
    }

    pub async fn set_rx2_nb(&mut self, level: u8) {
        if self.has_cap("rx_nb_enable_ex") || self.has_extensions() {
            // Zie set_nb() — zelfde Thetis-gotcha, stuur echte level i.p.v. .max(1).
            let enabled = level > 0;
            let cmd = format!("rx_nb_enable_ex:1,{},{};", if enabled { "true" } else { "false" }, level);
            self.send(&cmd).await;
            self.rx2_nb_enable = enabled;
        } else {
            let actual = level.min(1);
            let enabled = actual > 0;
            let cmd = format!("rx_nb_enable:1,{};", if enabled { "true" } else { "false" });
            self.send(&cmd).await;
            self.rx2_nb_enable = enabled;
        }
    }

    pub async fn set_rx2_binaural(&mut self, on: bool) {
        let cmd = format!("rx_bin_enable:1,{};", if on { "true" } else { "false" });
        self.send(&cmd).await;
        self.rx2_binaural = on;
    }

    pub async fn set_rx2_apf_enable(&mut self, on: bool) {
        let cmd = format!("rx_apf_enable:1,{};", if on { "true" } else { "false" });
        self.send(&cmd).await;
        self.rx2_apf_enable = on;
    }

    pub async fn set_rx2_vfo_lock(&mut self, on: bool) {
        let cmd = format!("vfo_lock:1,0,{};", if on { "true" } else { "false" });
        self.send(&cmd).await;
        self.rx2_vfo_lock = on;
    }

    pub async fn set_mute(&mut self, on: bool) {
        let cmd = format!("mute:{};", if on { "true" } else { "false" });
        self.send(&cmd).await;
        self.mute = on;
    }

    pub async fn set_rx_mute(&mut self, on: bool) {
        let cmd = format!("rx_mute:0,{};", if on { "true" } else { "false" });
        self.send(&cmd).await;
        self.rx_mute = on;
    }

    pub async fn set_nf_enable(&mut self, on: bool) {
        let cmd = format!("rx_nf_enable:0,{};", if on { "true" } else { "false" });
        self.send(&cmd).await;
        self.nf_enable = on;
    }

    pub async fn set_rx2_nf_enable(&mut self, on: bool) {
        let cmd = format!("rx_nf_enable:1,{};", if on { "true" } else { "false" });
        self.send(&cmd).await;
        self.rx2_nf_enable = on;
    }

    pub async fn set_rx_balance(&mut self, value: i8) {
        let val = value.clamp(-40, 40);
        let cmd = format!("rx_balance:0,0,{};", val);
        self.send(&cmd).await;
        self.rx_balance = val;
    }

    pub async fn set_tune(&mut self, on: bool) {
        let cmd = format!("tune:0,{};", if on { "true" } else { "false" });
        info!("TCI: TUNE {}", if on { "ON" } else { "OFF" });
        self.send(&cmd).await;
        self.tune_active = on;
    }

    pub async fn set_tune_drive(&mut self, level: u8) {
        let level = level.min(100);
        let cmd = format!("tune_drive:0,{};", level);
        info!("TCI: Tune drive = {}%", level);
        self.send(&cmd).await;
        self.tune_drive = level;
    }

    pub async fn set_mon_volume(&mut self, db: i8) {
        let cmd = format!("mon_volume:{};", db);
        info!("TCI: Mon volume = {} dB", db);
        self.send(&cmd).await;
        self.mon_volume = db;
    }

    /// Set IQ sample rate (call before connect, or send command if already connected)
    pub fn set_iq_sample_rate(&mut self, rate: u32) {
        self.iq_sample_rate = rate;
    }

    /// Send a spot to Thetis panorama via TCI SPOT command.
    pub async fn send_spot(&mut self, callsign: &str, mode: &str, freq_hz: u64, color: u32, text: &str) {
        let safe_call = callsign.replace([',', ';'], "");
        let safe_mode = mode.replace([',', ';'], "");
        let safe_text = text.replace([',', ';'], "");
        let cmd = format!("SPOT:{},{},{},{},{};", safe_call, safe_mode, freq_hz, color, safe_text);
        self.send(&cmd).await;
    }

    /// Clear all spots from Thetis panorama.
    pub async fn clear_spots(&mut self) {
        self.send("SPOT_CLEAR;").await;
    }

    // ── Extended TCI commands (_ex, capability-gated) ──────────────────

    pub async fn set_ddc_sample_rate(&mut self, rx: u32, rate: u32) {
        if self.has_cap("ddc_sample_rate_ex") {
            let cmd = format!("ddc_sample_rate_ex:{},{};", rx, rate);
            self.send(&cmd).await;
            if rx == 0 { self.ddc_sample_rate_rx1 = rate; }
            else { self.ddc_sample_rate_rx2 = rate; }
        }
    }

    /// Start auto-null on Thetis with step plan. Results arrive via DiversityAutonull notifications.
    /// Steps format: Vec of (is_phase, offsets) — same as client's diversity-smart.txt
    pub async fn diversity_autonull(&mut self, settle_ms: u32, steps: &[(Vec<f32>, bool)]) {
        if !self.has_cap("diversity_sweep_ex") { return; }
        self.diversity_auto_progress = None;
        self.diversity_auto_done = None;
        // Build command: diversity_autonull_ex:settle_ms|P:off1:off2|G:off1:off2|...;
        let mut plan_parts = Vec::new();
        for (offsets, is_phase) in steps {
            let prefix = if *is_phase { "P" } else { "G" };
            let vals: Vec<String> = offsets.iter().map(|v| format!("{:.1}", v)).collect();
            plan_parts.push(format!("{}:{}", prefix, vals.join(":")));
        }
        let cmd = format!("diversity_autonull_ex:{}|{};", settle_ms, plan_parts.join("|"));
        self.send(&cmd).await;
    }

    /// Start a diversity sweep on Thetis. Results arrive via DiversitySweepResult notification.
    pub async fn diversity_sweep(&mut self, sweep_type: &str, start: f32, end: f32, step: f32, settle_ms: u32) {
        if self.has_cap("diversity_sweep_ex") {
            self.diversity_sweep_result = None;
            let cmd = format!("diversity_sweep_ex:{},{:.1},{:.1},{:.1},{};",
                sweep_type, start, end, step, settle_ms);
            self.send(&cmd).await;
        }
    }

    pub async fn set_ctun(&mut self, rx: u32, enabled: bool) {
        if self.has_cap("ctun_ex") {
            let cmd = format!("rx_ctun_ex:{},{};", rx, enabled);
            self.send(&cmd).await;
            if rx == 0 { self.ctun = enabled; }
        }
    }

    pub async fn set_vfo_sync(&mut self, enabled: bool) {
        if self.has_cap("vfo_sync_ex") {
            let cmd = format!("vfo_sync_ex:{};", enabled);
            self.send(&cmd).await;
            self.vfo_sync_on = enabled;
        }
    }

    pub async fn set_fm_deviation(&mut self, rx: u32, hz: u32) {
        if self.has_cap("fm_deviation_ex") {
            let cmd = format!("fm_deviation_ex:{},{};", rx, hz);
            self.send(&cmd).await;
            self.fm_deviation = if hz >= 5000 { 1 } else { 0 };
        }
    }

    pub async fn set_step_attenuator(&mut self, rx: u32, db: i32) {
        if self.has_cap("step_attenuator_ex") {
            let cmd = format!("step_attenuator_ex:{},{};", rx, db);
            self.send(&cmd).await;
            if rx == 0 { self.step_att_rx1 = db; }
            else { self.step_att_rx2 = db; }
        }
    }

    pub async fn set_diversity_enable(&mut self, enabled: bool) {
        if self.has_cap("diversity_ex") {
            let cmd = format!("diversity_enable_ex:{};", enabled);
            self.send(&cmd).await;
            self.diversity_enabled = enabled;
        }
    }

    pub async fn set_diversity_ref(&mut self, rx1_ref: bool) {
        if self.has_cap("diversity_ex") {
            let cmd = format!("diversity_ref_ex:{};", rx1_ref);
            self.send(&cmd).await;
            self.diversity_ref = if rx1_ref { 0 } else { 1 };
        }
    }

    pub async fn set_diversity_source(&mut self, source: u32) {
        if self.has_cap("diversity_ex") {
            let cmd = format!("diversity_source_ex:{};", source);
            self.send(&cmd).await;
            self.diversity_source = source as u8;
        }
    }

    pub async fn set_diversity_gain(&mut self, rx: u32, gain: u16) {
        if self.has_cap("diversity_ex") {
            let cmd = format!("diversity_gain_ex:{},{};", rx, gain.min(10000));
            self.send(&cmd).await;
            if rx == 0 { self.diversity_gain_rx1 = gain; }
            else { self.diversity_gain_rx2 = gain; }
        }
    }

    pub async fn set_diversity_phase(&mut self, phase: i32) {
        if self.has_cap("diversity_ex") {
            let cmd = format!("diversity_phase_ex:{};", phase.clamp(-18000, 18000));
            self.send(&cmd).await;
            self.diversity_phase = phase;
        }
    }
}
