// SPDX-License-Identifier: GPL-2.0-or-later

#![allow(dead_code)]

//! TCI command setters for TciConnection.
//! Each method formats a TCI command string, sends it via WebSocket,
//! and updates the local state optimistically.

use log::{debug, info};
use crate::tci::TciConnection;
use crate::tci_parser::mode_u8_to_str;

impl TciConnection {
    pub async fn set_vfo_a_freq(&mut self, hz: u64) {
        let cmd = format!("VFO:0,0,{};", hz);
        debug!("TCI: set VFO A = {} Hz", hz);
        self.send(&cmd).await;
    }

    pub async fn set_vfo_a_mode(&mut self, mode: u8) {
        let mode_str = mode_u8_to_str(mode);
        let cmd = format!("MODULATION:0,{};", mode_str);
        debug!("TCI: set VFO A mode = {} ({})", mode_str, mode);
        self.send(&cmd).await;
    }

    pub async fn set_power(&mut self, on: bool) {
        let cmd = if on { "START;" } else { "STOP;" };
        debug!("TCI: Power {} ({})", if on { "ON" } else { "OFF" }, cmd);
        self.send(cmd).await;
    }

    pub async fn set_tx_profile(&mut self, idx: u8) {
        if let Some(name) = self.tx_profile_names.get(idx as usize) {
            let safe_name = name.replace([',', ';'], "");
            let cmd = format!("tx_profile_ex:{};", safe_name);
            debug!("TCI: set TX profile = \"{}\" (index {})", safe_name, idx);
            self.send(&cmd).await;
        }
    }

    pub async fn set_nr(&mut self, level: u8) {
        // Stock .14/.15 supports rx_nr_enable_ex without advertising the cap.
        if level == 0 {
            self.send("rx_nr_enable_ex:0,false,1;").await;
        } else {
            let cmd = format!("rx_nr_enable_ex:0,true,{};", level);
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
        debug!("TCI: Drive = {}%", level);
        self.send(&cmd).await;
    }

    pub async fn set_filter(&mut self, low_hz: i32, high_hz: i32) {
        let cmd = format!("RX_FILTER_BAND:0,{},{};", low_hz, high_hz);
        debug!("TCI: Filter = {} .. {} Hz", low_hz, high_hz);
        self.send(&cmd).await;
    }

    pub async fn set_vfo_b_freq(&mut self, hz: u64) {
        // TCI: receiver 0 channel 1, or receiver 1 channel 0 depending on Thetis config
        let cmd = format!("VFO:0,1,{};", hz);
        debug!("TCI: set VFO B = {} Hz", hz);
        self.send(&cmd).await;
    }

    pub async fn set_vfo_b_mode(&mut self, mode: u8) {
        let mode_str = mode_u8_to_str(mode);
        let cmd = format!("MODULATION:1,{};", mode_str);
        debug!("TCI: set VFO B mode = {} ({})", mode_str, mode);
        self.send(&cmd).await;
    }

    pub async fn vfo_swap(&mut self) {
        // Stock .14/.15 supports vfo_swap_ex without advertising the cap (consistent
        // patroon met andere stock-supported _ex commands). Native swap doet freq +
        // mode + filter; eerdere fallback ("swap freq alleen") was incompleet.
        // Bij smoke-test FAIL: revert naar manual freq-swap fallback.
        self.send("vfo_swap_ex;").await;
    }

    pub async fn set_rx2_af_gain(&mut self, level: u8) {
        // rx_volume supported since Thetis v2.10.3.13.
        // Schaal: 0..100 % -> −60..0 dB (matches RxVolume parser/handler in tci.rs).
        let level = level.min(100);
        let db = ((level as i32 - 100) * 60) / 100;
        let cmd = format!("rx_volume:1,0,{};", db);
        self.send(&cmd).await;
        // No optimistic state update - Thetis echoes rx_volume back and the
        // RxVolume notification handler updates `rx2_af_gain`.
    }

    pub async fn set_rx2_nr(&mut self, level: u8) {
        // Stock .14/.15 supports rx_nr_enable_ex without advertising the cap.
        if level == 0 {
            self.send("rx_nr_enable_ex:1,false,1;").await;
        } else {
            let cmd = format!("rx_nr_enable_ex:1,true,{};", level);
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
        debug!("TCI: RX2 Filter = {} .. {} Hz", low_hz, high_hz);
        self.send(&cmd).await;
    }

    pub async fn set_mon(&mut self, on: bool) {
        let cmd = format!("MON_ENABLE:{};", if on { "true" } else { "false" });
        debug!("TCI: MON {}", if on { "ON" } else { "OFF" });
        self.send(&cmd).await;
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
        // Stock .14/.15 supports rx_nb_enable_ex without advertising the cap.
        // Het `level`-argument bepaalt de uiteindelijke NB-stand bij de server;
        // bij disable sturen we `level=0` (niet `.max(1)`), anders blijft NB1
        // actief en werkt de cycle->off transitie niet.
        let enabled = level > 0;
        let cmd = format!("rx_nb_enable_ex:0,{},{};", if enabled { "true" } else { "false" }, level);
        self.send(&cmd).await;
        self.nb_enable = enabled;
        self.nb_level = level;
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
        debug!("TCI: CW macro stop");
        self.send("cw_macros_stop;").await;
    }

    pub async fn set_vfo_lock(&mut self, on: bool) {
        let cmd = format!("vfo_lock:0,0,{};", if on { "true" } else { "false" });
        self.send(&cmd).await;
        self.vfo_lock = on;
    }

    pub async fn set_binaural(&mut self, on: bool) {
        // Idempotency-guard: skip if state already matches. Defense in depth
        // against client-side spam (alpha-5 testlog: 38k+ rx_bin_enable events
        // from engine.rs SetPtt-side-effect path, root-fixed there in alpha-8
        // sub-B; this guard makes the server resilient to future variants).
        if on == self.binaural {
            return;
        }
        let cmd = format!("rx_bin_enable:0,{};", if on { "true" } else { "false" });
        self.send(&cmd).await;
        // Switch TCI audio channels: stereo for binaural, mono otherwise
        let ch_cmd = format!("AUDIO_STREAM_CHANNELS:{};", if on { 2 } else { 1 });
        self.send(&ch_cmd).await;
        debug!("TCI: binaural {} -> audio channels {}", if on { "ON" } else { "OFF" }, if on { 2 } else { 1 });
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
        // Stock .14/.15 supports rx_nb_enable_ex without advertising the cap.
        // Zie set_nb() - zelfde Thetis-gotcha, stuur echte level i.p.v. .max(1).
        let enabled = level > 0;
        let cmd = format!("rx_nb_enable_ex:1,{},{};", if enabled { "true" } else { "false" }, level);
        self.send(&cmd).await;
        self.rx2_nb_enable = enabled;
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
        debug!("TCI: TUNE {}", if on { "ON" } else { "OFF" });
        self.send(&cmd).await;
        self.tune_active = on;
    }

    pub async fn set_tune_drive(&mut self, level: u8) {
        let level = level.min(100);
        let cmd = format!("tune_drive:0,{};", level);
        debug!("TCI: Tune drive = {}%", level);
        self.send(&cmd).await;
        self.tune_drive = level;
    }

    pub async fn set_mon_volume(&mut self, db: i8) {
        let cmd = format!("mon_volume:{};", db);
        debug!("TCI: Mon volume = {} dB", db);
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


    /// Start auto-null on Thetis with step plan. Results arrive via DiversityAutonull notifications.
    /// Steps format: Vec of (is_phase, offsets) - same as client's diversity-smart.txt
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
        // Stock .14/.15 supports rx_ctun_ex without advertising the cap.
        let cmd = format!("rx_ctun_ex:{},{};", rx, enabled);
        self.send(&cmd).await;
        if rx == 0 { self.ctun = enabled; }
    }

    pub async fn set_vfo_sync(&mut self, enabled: bool) {
        // Stock .14/.15 supports vfo_sync_ex without advertising the cap.
        let cmd = format!("vfo_sync_ex:{};", enabled);
        self.send(&cmd).await;
        self.vfo_sync_on = enabled;
    }

    pub async fn set_fm_deviation(&mut self, rx: u32, hz: u32) {
        // Stock .14/.15 supports fm_deviation_ex without advertising the cap.
        let cmd = format!("fm_deviation_ex:{},{};", rx, hz);
        self.send(&cmd).await;
        self.fm_deviation = if hz >= 5000 { 1 } else { 0 };
    }

    pub async fn set_diversity_enable(&mut self, enabled: bool) {
        if self.has_cap("diversity_enable_ex") {
            let cmd = format!("diversity_enable_ex:{};", enabled);
            self.send(&cmd).await;
            self.diversity_enabled = enabled;
        }
    }

    /// Set Thetis' "Receive only" flag via the fork's `rx_only_ex` command -
    /// a preventive transmit-inhibit (MOX/spacebar/hardware-PTT/VOX all refused
    /// at the source, not reactively flipped back). Only sent when the fork
    /// advertises the capability; returns `true` if it was sent (extensions
    /// available), `false` if not (caller falls back to reactive ZZTX0).
    pub async fn set_rx_only(&mut self, rx_only: bool) -> bool {
        if self.has_cap("rx_only_ex") {
            let cmd = format!("rx_only_ex:{};", rx_only);
            self.send(&cmd).await;
            true
        } else {
            false
        }
    }

    pub async fn set_diversity_ref(&mut self, rx1_ref: bool) {
        if self.has_cap("diversity_ref_ex") {
            let cmd = format!("diversity_ref_ex:{};", rx1_ref);
            self.send(&cmd).await;
            self.diversity_ref = if rx1_ref { 0 } else { 1 };
        }
    }

    pub async fn set_diversity_source(&mut self, source: u32) {
        if self.has_cap("diversity_source_ex") {
            let cmd = format!("diversity_source_ex:{};", source);
            self.send(&cmd).await;
            self.diversity_source = source as u8;
        }
    }

    pub async fn set_diversity_gain(&mut self, rx: u32, gain: u16) {
        if self.has_cap("diversity_gain_ex") {
            let cmd = format!("diversity_gain_ex:{},{};", rx, gain.min(10000));
            self.send(&cmd).await;
            if rx == 0 { self.diversity_gain_rx1 = gain; }
            else { self.diversity_gain_rx2 = gain; }
        }
    }

    pub async fn set_diversity_phase(&mut self, phase: i32) {
        if self.has_cap("diversity_phase_ex") {
            let cmd = format!("diversity_phase_ex:{};", phase.clamp(-18000, 18000));
            self.send(&cmd).await;
            self.diversity_phase = phase;
        }
    }

    pub async fn set_diversity_gain_multi(&mut self, multi: u16) {
        if self.has_cap("diversity_gain_multi_ex") {
            let clamped = multi.clamp(100, 1000);
            let cmd = format!("diversity_gain_multi_ex:{};", clamped);
            self.send(&cmd).await;
            self.diversity_gain_multi = clamped;
        }
    }

    /// Send DDC sample-rate change via TL2-1 fork extension (no CAT fallback -
    /// stock Thetis has no equivalent in TCI, only via Setup-form UI).
    /// `rate_hz` must be one of 48000/96000/192000/384000/768000/1536000.
    pub async fn set_ddc_sample_rate(&mut self, rx: u32, rate_hz: u32) {
        if !self.has_cap("ddc_sample_rate_ex") { return; }
        match rate_hz {
            48000 | 96000 | 192000 | 384000 | 768000 | 1536000 => {}
            _ => return,
        }
        let cmd = format!("ddc_sample_rate_ex:{},{};", rx, rate_hz);
        self.send(&cmd).await;
    }

    // ── Stock Thetis v2.10.3.14 native commands ────────────────────────────

    // ── Note: stock v2.10.3.14 setters do NOT optimistically mutate local state.
    // Thetis echoes the change back as a notification (rx_step_att_ex, etc.),
    // which is parsed and dispatched in `handle_notification` - that is the single
    // source of truth. Skipping the optimistic update prevents drift if `send()`
    // silently dropped the frame (no return value to check). See
    // `feedback_dispatch_return_checked.md`.

    /// Set step attenuator value via stock v2.10.3.14 rx_step_att_ex.
    /// Wire format requires |attenuation| (positive only, per Math.Abs in Thetis).
    pub async fn set_rx_step_att(&mut self, rx: u32, db: i32) {
        let abs_db = db.unsigned_abs().min(31);
        let cmd = format!("rx_step_att_ex:{},{};", rx, abs_db);
        debug!("TCI: set rx_step_att RX{} = {} dB", rx + 1, abs_db);
        self.send(&cmd).await;
        // state will be updated by RxStepAttEx echo notification
    }

    /// Toggle step attenuator on/off via stock v2.10.3.14 rx_step_att_enabled_ex.
    pub async fn set_rx_step_att_enabled(&mut self, rx: u32, enabled: bool) {
        let cmd = format!("rx_step_att_enabled_ex:{},{};", rx, enabled);
        debug!("TCI: set rx_step_att_enabled RX{} = {}", rx + 1, enabled);
        self.send(&cmd).await;
        // state will be updated by RxStepAttEnabledEx echo notification
    }

    /// Set combined preamp+attenuator via stock v2.10.3.14 rx_preamp_att_ex.
    /// Signed wire format: negative value = preamp gain, positive = attenuation.
    pub async fn set_rx_preamp_att(&mut self, rx: u32, db: i32) {
        let cmd = format!("rx_preamp_att_ex:{},{};", rx, db);
        debug!("TCI: set rx_preamp_att RX{} = {} dB (signed)", rx + 1, db);
        self.send(&cmd).await;
        // state will be updated by RxPreampAttEx echo notification
    }

    /// Set TX filter band via stock v2.10.3.14 tx_filter_band_ex.
    pub async fn set_tx_filter_band(&mut self, low_hz: i32, high_hz: i32) {
        let cmd = format!("tx_filter_band_ex:{},{};", low_hz, high_hz);
        debug!("TCI: set tx_filter_band = {} .. {} Hz", low_hz, high_hz);
        self.send(&cmd).await;
        // state will be updated by TxFilterBandEx echo notification
    }

    /// Send tx_frequency_ex (stock v2.10.3.14). Used for split TX state.
    /// Format: tx_frequency_ex:freq,band,rx2_enabled,tx_vfob
    /// All boolean values are sent as lowercase strings per the TCI protocol spec.
    pub async fn set_tx_frequency(&mut self, freq: u64, band: &str, rx2_enabled: bool, tx_vfob: bool) {
        let cmd = format!(
            "tx_frequency_ex:{},{},{},{};",
            freq, band.to_lowercase(), rx2_enabled, tx_vfob
        );
        debug!("TCI: set tx_frequency = {} Hz band={} rx2={} tx_vfob={}",
            freq, band, rx2_enabled, tx_vfob);
        self.send(&cmd).await;
    }

    /// run_cat_ex helper: relay a Kenwood-style ZZ command via TCI (stock v2.10.3.14).
    /// Wire format: `run_cat_ex:ZZxxx;` (Thetis appends `;` on its side before invoking CAT).
    /// Response arrives via `TciNotification::RunCatExResponse`.
    /// Caller should pass the ZZ command WITHOUT a trailing `;` (helper strips defensively).
    pub async fn run_cat(&mut self, zz_cmd: &str) {
        let stripped = zz_cmd.trim().trim_end_matches(';');
        if stripped.is_empty() {
            return;
        }
        let cmd = format!("run_cat_ex:{};", stripped);
        // ZZFI/ZZFJ are 2-second filter-preset polls (alpha-1 sub-E); demote
        // their helper-log to debug to keep info-log readable. Response-handler
        // (tci.rs RunCatExResponse arm) still logs on actual state-change.
        let stripped_upper = stripped.to_uppercase();
        if stripped_upper.starts_with("ZZFI") || stripped_upper.starts_with("ZZFJ") {
            log::debug!("TCI: run_cat_ex({})", stripped);
        } else {
            debug!("TCI: run_cat_ex({})", stripped);
        }
        self.send(&cmd).await;
    }
}
