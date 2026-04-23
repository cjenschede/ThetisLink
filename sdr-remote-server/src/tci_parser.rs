// SPDX-License-Identifier: GPL-2.0-or-later

//! TCI protocol parser — pure functions for parsing TCI text commands,
//! binary frame decoding, and mode conversion. No state dependencies.

use std::sync::atomic::AtomicBool;

use log::{info, warn};

// ── Globals ─────────────────────────────────────────────────────────────

/// Set to true when Thetis sends rx_channel_sensors_ex (avgdBm).
/// When active, legacy rx_channel_sensors is ignored to prevent instant values overriding avg.
pub static HAS_SENSORS_EX: AtomicBool = AtomicBool::new(false);

// ── Constants ───────────────────────────────────────────────────────────

/// TCI binary stream header size (16 x u32 = 64 bytes)
pub const TCI_HEADER_SIZE: usize = 64;

/// TCI stream types (header offset 24)
pub const STREAM_TYPE_IQ: u32 = 0;
pub const STREAM_TYPE_RX_AUDIO: u32 = 1;
pub const STREAM_TYPE_TX_AUDIO: u32 = 2;
pub const STREAM_TYPE_TX_CHRONO: u32 = 3;

/// TCI sample format codes (header offset 8)
pub const SAMPLE_INT16: u32 = 0;
pub const SAMPLE_INT32: u32 = 2;
pub const SAMPLE_FLOAT32: u32 = 3;

// ── TCI Notification enum ───────────────────────────────────────────────

#[allow(dead_code)]
pub enum TciNotification {
    // Text notifications
    Ready,
    Vfo { receiver: u32, channel: u32, freq: u64 },
    Dds { receiver: u32, freq: u64 },
    Modulation { receiver: u32, mode_str: String },
    Trx { receiver: u32, active: bool },
    Drive { receiver: u32, value: u8 },
    RxFilterBand { receiver: u32, low: i32, high: i32 },
    RxChannelSensors { receiver: u32, channel: u32, dbm: f32 },
    TxSensors { _receiver: u32, _mic_dbm: f32, power_w: f32, _peak_w: f32, swr: f32 },
    Start,
    Stop,
    Volume { db: i32 },
    RxVolume { receiver: u32, _sub_rx: u32, db: f32 },
    RxChannelEnable { receiver: u32, channel: u32, enabled: bool },
    MonEnable { enabled: bool },
    AgcMode { receiver: u32, mode: u8 },
    AgcGain { receiver: u32, gain: u8 },
    RitEnable { receiver: u32, enabled: bool },
    RitOffset { receiver: u32, offset: i32 },
    XitEnable { receiver: u32, enabled: bool },
    XitOffset { receiver: u32, offset: i32 },
    SqlEnable { receiver: u32, enabled: bool },
    SqlLevel { receiver: u32, level: u8 },
    NbEnable { receiver: u32, enabled: bool, level: u8 },
    CwKeyerSpeed { speed: u8 },
    VfoLock { enabled: bool },
    VfoLockB { enabled: bool },
    BinEnable { receiver: u32, enabled: bool },
    ApfEnable { receiver: u32, enabled: bool },
    Mute { enabled: bool },
    RxMute { receiver: u32, enabled: bool },
    NfEnable { receiver: u32, enabled: bool },
    RxBalance { receiver: u32, channel: u32, value: i32 },
    Tune { receiver: u32, active: bool },
    TuneDrive { receiver: u32, power: u8 },
    MonVolume { db: i8 },
    RxAnfEnable { receiver: u32, enabled: bool },
    AgcAutoEx { receiver: u32, enabled: bool },
    /// Diversity sweep result: type + list of (value, dBm) pairs
    DiversitySweepResult { sweep_type: String, results: Vec<(f32, f32)> },
    /// Auto-null progress: round/total, phase, gain_db, best_smeter
    DiversityAutonullProgress { round: u32, total: u32, phase: f32, gain_db: f32, smeter: f32 },
    /// Auto-null done: phase, gain_db, improvement_db, off_dbm, on_dbm
    DiversityAutonullDone { phase: f32, gain_db: f32, improvement_db: f32 },
    /// Auto-null error
    DiversityAutonullError { message: String },
    DdcSampleRateEx { receiver: u32, rate: u32 },
    DdcSampleRatesEx { rates: Vec<u32> },
    RxNrEnable { receiver: u32, enabled: bool, level: u8 },
    /// calibration_ex:rx,meter_cal,display_cal,xvtr_gain,6m_gain,tx_display
    CalibrationEx { receiver: u32, meter_cal: f32, xvtr_gain: f32, six_m_gain: f32 },
    TxProfilesEx { names: Vec<String> },
    TxProfileEx { name: String },
    /// Server capability flags (Thetis extension)
    TciCapsEx { caps: Vec<String> },
    // ThetisLink extended controls
    CtunEx { receiver: u32, enabled: bool },
    VfoSyncEx { enabled: bool },
    FmDeviationEx { receiver: u32, hz: u32 },
    StepAttenuatorEx { receiver: u32, db: i32 },
    DiversityEnableEx { enabled: bool },
    DiversityRefEx { rx1_ref: bool },
    DiversitySourceEx { source: u32 },
    DiversityGainEx { receiver: u32, gain: u16 },
    DiversityPhaseEx { phase: i32 },
    // Binary stream notifications
    RxAudio { receiver: u32, samples: Vec<f32> },
    IqStream { receiver: u32, iq_pairs: Vec<(f32, f32)> },
    TxChrono { samples_requested: u32, sample_rate: u32, channels: u32, format: u32 },
    // Connection events
    Disconnected,
}

// ── Parser functions ────────────────────────────────────────────────

pub fn parse_tci_text(cmd: &str) -> Option<TciNotification> {
    // Commands are case-insensitive per spec
    // Handle tx_profiles_ex / tx_profile_ex BEFORE lowercase (names are case-sensitive)
    // and before comma-split (names can contain commas and braces)
    let cmd_lower = cmd.to_lowercase();
    if let Some(colon_pos) = cmd_lower.find("tci_caps_ex:") {
        let payload = &cmd_lower[colon_pos + "tci_caps_ex:".len()..];
        let caps: Vec<String> = payload.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        info!("TCI: server capabilities: {:?}", caps);
        return Some(TciNotification::TciCapsEx { caps });
    }
    if let Some(colon_pos) = cmd_lower.find("tx_profiles_ex:") {
        let payload = &cmd[colon_pos + "tx_profiles_ex:".len()..];
        // Try braced format first: {name1},{name2},...
        let names = parse_braced_list(payload);
        if !names.is_empty() {
            return Some(TciNotification::TxProfilesEx { names });
        }
        // Fallback: comma-separated without braces
        let plain: Vec<String> = payload.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !plain.is_empty() {
            return Some(TciNotification::TxProfilesEx { names: plain });
        }
        return None;
    }
    if let Some(colon_pos) = cmd_lower.find("tx_profile_ex:") {
        let payload = &cmd[colon_pos + "tx_profile_ex:".len()..];
        let trimmed = payload.trim();
        // Accept both {name} and name (without braces)
        let name = if trimmed.starts_with('{') && trimmed.ends_with('}') {
            trimmed[1..trimmed.len()-1].to_string()
        } else {
            trimmed.to_string()
        };
        if !name.is_empty() {
            return Some(TciNotification::TxProfileEx { name });
        }
        return None;
    }

    let lower = cmd.to_lowercase();

    if lower == "ready" {
        return Some(TciNotification::Ready);
    }
    if lower == "start" {
        return Some(TciNotification::Start);
    }
    if lower == "stop" {
        return Some(TciNotification::Stop);
    }

    // Split on colon: "command:args"
    let (name, args_str) = lower.split_once(':')?;
    let args: Vec<&str> = args_str.split(',').collect();

    match name {
        "vfo" => {
            if args.len() >= 3 {
                let receiver = args[0].trim().parse().ok()?;
                let channel = args[1].trim().parse().ok()?;
                let freq = args[2].trim().parse().ok()?;
                Some(TciNotification::Vfo { receiver, channel, freq })
            } else {
                None
            }
        }
        "dds" => {
            if args.len() >= 2 {
                let receiver = args[0].trim().parse().ok()?;
                let freq = args[1].trim().parse().ok()?;
                Some(TciNotification::Dds { receiver, freq })
            } else {
                None
            }
        }
        "modulation" => {
            if args.len() >= 2 {
                let receiver = args[0].trim().parse().ok()?;
                // Use original case for mode string
                let original_args: Vec<&str> = cmd.split_once(':')?.1.split(',').collect();
                let mode_str = original_args.get(1)?.trim().to_string();
                Some(TciNotification::Modulation { receiver, mode_str })
            } else {
                None
            }
        }
        "trx" => {
            if args.len() >= 2 {
                let receiver = args[0].trim().parse().ok()?;
                let active = args[1].trim() == "true";
                Some(TciNotification::Trx { receiver, active })
            } else {
                None
            }
        }
        "drive" => {
            if args.len() >= 2 {
                let receiver = args[0].trim().parse().ok()?;
                let value: u8 = args[1].trim().parse().ok()?;
                Some(TciNotification::Drive { receiver, value: value.min(100) })
            } else {
                None
            }
        }
        "rx_filter_band" => {
            if args.len() >= 3 {
                let receiver = args[0].trim().parse().ok()?;
                let low = args[1].trim().parse().ok()?;
                let high = args[2].trim().parse().ok()?;
                Some(TciNotification::RxFilterBand { receiver, low, high })
            } else {
                None
            }
        }
        "rx_channel_sensors" => {
            // Ignore legacy format when _ex is available (avoids instant values overriding avg)
            if HAS_SENSORS_EX.load(std::sync::atomic::Ordering::Relaxed) {
                None
            } else if args.len() >= 3 {
                let receiver = args[0].trim().parse().ok()?;
                let channel = args[1].trim().parse().ok()?;
                let dbm = args[2].trim().parse().ok()?;
                Some(TciNotification::RxChannelSensors { receiver, channel, dbm })
            } else {
                None
            }
        }
        "rx_channel_sensors_ex" => {
            // Extended format: rx_channel_sensors_ex:rx,chan,dBm,avgdBm,peakBinDbm
            // Use avgdBm (field 3) = time-domain RMS averaged power over the
            // passband — integrates noise + signal, so the S-meter reads a
            // representative signal level. peakBinDbm (field 4) is the single
            // highest FFT bin and under-reports broadband noise levels.
            if !HAS_SENSORS_EX.swap(true, std::sync::atomic::Ordering::Relaxed) {
                info!("TCI: rx_channel_sensors_ex active (using avgdBm for S-meter)");
            }
            if args.len() >= 5 {
                let receiver = args[0].trim().parse().ok()?;
                let channel = args[1].trim().parse().ok()?;
                let avg_dbm: f32 = args[3].trim().parse().ok()?;
                Some(TciNotification::RxChannelSensors { receiver, channel, dbm: avg_dbm })
            } else {
                None
            }
        }
        "rx_sensors" => {
            // Legacy format: rx_sensors:receiver,dbm
            if args.len() >= 2 {
                let receiver = args[0].trim().parse().ok()?;
                let dbm = args[1].trim().parse().ok()?;
                Some(TciNotification::RxChannelSensors { receiver, channel: 0, dbm })
            } else {
                None
            }
        }
        "tx_sensors" => {
            if args.len() >= 5 {
                let receiver = args[0].trim().parse().ok()?;
                let mic_dbm = args[1].trim().parse().ok()?;
                let power_w = args[2].trim().parse().ok()?;
                let peak_w = args[3].trim().parse().ok()?;
                let swr = args[4].trim().parse().ok()?;
                Some(TciNotification::TxSensors { _receiver: receiver, _mic_dbm: mic_dbm, power_w, _peak_w: peak_w, swr })
            } else {
                None
            }
        }
        "volume" => {
            if args.len() >= 1 {
                let db = args[0].trim().parse().ok()?;
                Some(TciNotification::Volume { db })
            } else {
                None
            }
        }
        "rx_volume" => {
            if args.len() >= 3 {
                let receiver: u32 = args[0].trim().parse().ok()?;
                let sub_rx: u32 = args[1].trim().parse().ok()?;
                let db: f32 = args[2].trim().parse().ok()?;
                Some(TciNotification::RxVolume { receiver, _sub_rx: sub_rx, db })
            } else {
                None
            }
        }
        "rx_channel_enable" => {
            if args.len() >= 3 {
                let receiver: u32 = args[0].trim().parse().ok()?;
                let channel: u32 = args[1].trim().parse().ok()?;
                let enabled = args[2].trim() == "true";
                Some(TciNotification::RxChannelEnable { receiver, channel, enabled })
            } else {
                None
            }
        }
        "mon_enable" => {
            if !args.is_empty() {
                let enabled = args[0].trim() == "true";
                Some(TciNotification::MonEnable { enabled })
            } else {
                None
            }
        }
        "agc_mode" => {
            if args.len() >= 2 {
                let receiver = args[0].trim().parse().ok()?;
                let mode = match args[1].trim() {
                    "off" | "fixd" | "fixed" => 0,
                    "long" => 1,
                    "slow" => 2,
                    "normal" | "med" | "medium" => 3,
                    "fast" => 4,
                    "custom" => 5,
                    other => other.parse().unwrap_or(3),
                };
                Some(TciNotification::AgcMode { receiver, mode })
            } else { None }
        }
        "agc_gain" => {
            if args.len() >= 2 {
                let receiver = args[0].trim().parse().ok()?;
                let gain: u8 = args[1].trim().parse().ok()?;
                Some(TciNotification::AgcGain { receiver, gain })
            } else { None }
        }
        "rit_enable" => {
            if args.len() >= 2 {
                let receiver = args[0].trim().parse().ok()?;
                let enabled = args[1].trim() == "true";
                Some(TciNotification::RitEnable { receiver, enabled })
            } else { None }
        }
        "rit_offset" => {
            if args.len() >= 2 {
                let receiver = args[0].trim().parse().ok()?;
                let offset = args[1].trim().parse().ok()?;
                Some(TciNotification::RitOffset { receiver, offset })
            } else { None }
        }
        "xit_enable" => {
            if args.len() >= 2 {
                let receiver = args[0].trim().parse().ok()?;
                let enabled = args[1].trim() == "true";
                Some(TciNotification::XitEnable { receiver, enabled })
            } else { None }
        }
        "xit_offset" => {
            if args.len() >= 2 {
                let receiver = args[0].trim().parse().ok()?;
                let offset = args[1].trim().parse().ok()?;
                Some(TciNotification::XitOffset { receiver, offset })
            } else { None }
        }
        "sql_enable" => {
            if args.len() >= 2 {
                let receiver = args[0].trim().parse().ok()?;
                let enabled = args[1].trim() == "true";
                Some(TciNotification::SqlEnable { receiver, enabled })
            } else { None }
        }
        "sql_level" => {
            if args.len() >= 2 {
                let receiver = args[0].trim().parse().ok()?;
                let level: u8 = args[1].trim().parse().ok()?;
                Some(TciNotification::SqlLevel { receiver, level })
            } else { None }
        }
        "rx_nb_enable" | "rx_nb_enable_ex" => {
            if args.len() >= 2 {
                let receiver = args[0].trim().parse().ok()?;
                let enabled = args[1].trim() == "true";
                let level: u8 = if args.len() >= 3 { args[2].trim().parse().unwrap_or(if enabled { 1 } else { 0 }) } else { if enabled { 1 } else { 0 } };
                Some(TciNotification::NbEnable { receiver, enabled, level })
            } else { None }
        }
        "cw_keyer_speed" => {
            if !args.is_empty() {
                let speed: u8 = args[0].trim().parse().ok()?;
                Some(TciNotification::CwKeyerSpeed { speed })
            } else { None }
        }
        "vfo_lock" => {
            // Thetis sends vfo_lock:rx,chan,bool for each rx+chan combo.
            // Only accept rx=0,chan=0 (VFO A on RX1) as our lock state.
            if args.len() >= 3 {
                let rx: u32 = args[0].trim().parse().unwrap_or(99);
                let chan: u32 = args[1].trim().parse().unwrap_or(99);
                let enabled = args[2].trim() == "true";
                if rx == 0 && chan == 0 {
                    Some(TciNotification::VfoLock { enabled })
                } else if rx == 1 && chan == 0 {
                    Some(TciNotification::VfoLockB { enabled })
                } else {
                    None // ignore other rx/chan combos
                }
            } else if !args.is_empty() {
                let enabled = args[0].trim() == "true";
                Some(TciNotification::VfoLock { enabled })
            } else { None }
        }
        "rx_bin_enable" => {
            if args.len() >= 2 {
                let receiver = args[0].trim().parse().ok()?;
                let enabled = args[1].trim() == "true";
                Some(TciNotification::BinEnable { receiver, enabled })
            } else { None }
        }
        "rx_apf_enable" => {
            if args.len() >= 2 {
                let receiver = args[0].trim().parse().ok()?;
                let enabled = args[1].trim() == "true";
                Some(TciNotification::ApfEnable { receiver, enabled })
            } else { None }
        }
        "mute" => {
            if !args.is_empty() {
                let enabled = args[0].trim() == "true";
                Some(TciNotification::Mute { enabled })
            } else { None }
        }
        "rx_mute" => {
            if args.len() >= 2 {
                let receiver = args[0].trim().parse().ok()?;
                let enabled = args[1].trim() == "true";
                Some(TciNotification::RxMute { receiver, enabled })
            } else { None }
        }
        "rx_nf_enable" => {
            if args.len() >= 2 {
                let receiver = args[0].trim().parse().ok()?;
                let enabled = args[1].trim() == "true";
                Some(TciNotification::NfEnable { receiver, enabled })
            } else { None }
        }
        "rx_balance" => {
            if args.len() >= 3 {
                let receiver: u32 = args[0].trim().parse().ok()?;
                let channel: u32 = args[1].trim().parse().ok()?;
                let value: i32 = args[2].trim().parse().ok()?;
                Some(TciNotification::RxBalance { receiver, channel, value })
            } else { None }
        }
        "tune" => {
            if args.len() >= 2 {
                let receiver = args[0].trim().parse().ok()?;
                let active = args[1].trim() == "true";
                Some(TciNotification::Tune { receiver, active })
            } else { None }
        }
        "tune_drive" => {
            if args.len() >= 2 {
                let receiver = args[0].trim().parse().ok()?;
                let power: u8 = args[1].trim().parse().ok()?;
                Some(TciNotification::TuneDrive { receiver, power })
            } else { None }
        }
        "mon_volume" => {
            if !args.is_empty() {
                let db: i8 = args[0].trim().parse().ok()?;
                Some(TciNotification::MonVolume { db })
            } else { None }
        }
        "ddc_sample_rate_ex" => {
            if args.len() >= 2 {
                let receiver: u32 = args[0].trim().parse().ok()?;
                let rate: u32 = args[1].trim().parse().ok()?;
                Some(TciNotification::DdcSampleRateEx { receiver, rate })
            } else { None }
        }
        "ddc_sample_rates_ex" => {
            let rates: Vec<u32> = args.iter()
                .filter_map(|s| s.trim().parse().ok())
                .collect();
            if !rates.is_empty() { Some(TciNotification::DdcSampleRatesEx { rates }) } else { None }
        }
        "diversity_autonull_status_ex" => {
            if args.len() >= 2 {
                let status = args[0].trim();
                match status {
                    "progress" if args.len() >= 6 => {
                        let round: u32 = args[1].trim().parse().ok()?;
                        let total: u32 = args[2].trim().parse().ok()?;
                        let phase: f32 = args[3].trim().parse().ok()?;
                        let gain_db: f32 = args[4].trim().parse().ok()?;
                        let smeter: f32 = args[5].trim().parse().ok()?;
                        Some(TciNotification::DiversityAutonullProgress { round, total, phase, gain_db, smeter })
                    }
                    "done" if args.len() >= 4 => {
                        let phase: f32 = args[1].trim().parse().ok()?;
                        let gain_db: f32 = args[2].trim().parse().ok()?;
                        let improvement_db: f32 = args[3].trim().parse().ok()?;
                        Some(TciNotification::DiversityAutonullDone { phase, gain_db, improvement_db })
                    }
                    "error" => {
                        let message = args[1..].join(",");
                        Some(TciNotification::DiversityAutonullError { message })
                    }
                    _ => None,
                }
            } else { None }
        }
        "diversity_fastsweep_result_ex" => {
            // diversity_fastsweep_result_ex:type,t0:v0:r0,t1:v1:r1,...
            if !args.is_empty() {
                let sweep_type = args[0].trim().to_string();
                let results: Vec<(u32, f32, f32)> = args[1..].iter().filter_map(|triple| {
                    let parts: Vec<&str> = triple.trim().split(':').collect();
                    if parts.len() == 3 {
                        let t: u32 = parts[0].parse().ok()?;
                        let val: f32 = parts[1].parse().ok()?;
                        let dbm: f32 = parts[2].parse().ok()?;
                        Some((t, val, dbm))
                    } else { None }
                }).collect();
                if !results.is_empty() {
                    // Find minimum and log full data
                    let min = results.iter().min_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
                    if let Some((t, val, dbm)) = min {
                        log::info!("TCI: Fastsweep {}: {} points in {}ms, min={:.1}° at {:.1}dBm (t={}ms)",
                            sweep_type, results.len(),
                            results.last().map(|r| r.0).unwrap_or(0),
                            val, dbm, t);
                    }
                    // Summary log (avoid saturating I/O with per-point logging)
                    if let Some(min_r) = results.iter().min_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal)) {
                        log::info!("  sweep min: {:.1}° = {:.1}dBm at t={}ms", min_r.1, min_r.2, min_r.0);
                    }
                }
                None // no state change needed
            } else { None }
        }
        "diversity_sweep_result_ex" => {
            // diversity_sweep_result_ex:type,val1:rssi1,val2:rssi2,...
            if !args.is_empty() {
                let sweep_type = args[0].trim().to_string();
                let results: Vec<(f32, f32)> = args[1..].iter().filter_map(|pair| {
                    let parts: Vec<&str> = pair.trim().split(':').collect();
                    if parts.len() == 2 {
                        let val: f32 = parts[0].parse().ok()?;
                        let dbm: f32 = parts[1].parse().ok()?;
                        Some((val, dbm))
                    } else { None }
                }).collect();
                if !results.is_empty() {
                    Some(TciNotification::DiversitySweepResult { sweep_type, results })
                } else { None }
            } else { None }
        }
        "agc_auto_ex" => {
            if args.len() >= 2 {
                let receiver: u32 = args[0].trim().parse().ok()?;
                let enabled = args[1].trim() == "true";
                Some(TciNotification::AgcAutoEx { receiver, enabled })
            } else { None }
        }
        "rx_anf_enable" => {
            if args.len() >= 2 {
                let receiver: u32 = args[0].trim().parse().ok()?;
                let enabled = args[1].trim() == "true";
                Some(TciNotification::RxAnfEnable { receiver, enabled })
            } else { None }
        }
        "rx_nr_enable" | "rx_nr_enable_ex" => {
            if args.len() >= 2 {
                let receiver: u32 = args[0].trim().parse().ok()?;
                let enabled = args[1].trim() == "true";
                let level: u8 = if args.len() >= 3 { args[2].trim().parse().unwrap_or(1) } else { 1 };
                Some(TciNotification::RxNrEnable { receiver, enabled, level })
            } else { None }
        }
        "calibration_ex" => {
            // calibration_ex:rx,meter_cal,display_cal,xvtr_gain,6m_gain,tx_display
            if args.len() >= 5 {
                let receiver: u32 = args[0].trim().parse().ok()?;
                let meter_cal: f32 = args[1].trim().parse().ok()?;
                let xvtr_gain: f32 = args[3].trim().parse().ok()?;
                let six_m_gain: f32 = args[4].trim().parse().ok()?;
                Some(TciNotification::CalibrationEx { receiver, meter_cal, xvtr_gain, six_m_gain })
            } else { None }
        }
        // ── ThetisLink extended controls ────────────────────────────────
        "ctun_ex" | "rx_ctun_ex" => {
            if args.len() >= 2 {
                let receiver: u32 = args[0].trim().parse().ok()?;
                let enabled = args[1].trim() == "true";
                Some(TciNotification::CtunEx { receiver, enabled })
            } else { None }
        }
        "vfo_sync_ex" => {
            if !args.is_empty() {
                let enabled = args[0].trim() == "true";
                Some(TciNotification::VfoSyncEx { enabled })
            } else { None }
        }
        "fm_deviation_ex" => {
            if args.len() >= 2 {
                let receiver: u32 = args[0].trim().parse().ok()?;
                let hz: u32 = args[1].trim().parse().ok()?;
                Some(TciNotification::FmDeviationEx { receiver, hz })
            } else { None }
        }
        "step_attenuator_ex" => {
            if args.len() >= 2 {
                let receiver: u32 = args[0].trim().parse().ok()?;
                let db: i32 = args[1].trim().parse().ok()?;
                Some(TciNotification::StepAttenuatorEx { receiver, db })
            } else { None }
        }
        "diversity_enable_ex" => {
            if !args.is_empty() {
                let enabled = args[0].trim() == "true";
                Some(TciNotification::DiversityEnableEx { enabled })
            } else { None }
        }
        "diversity_ref_ex" => {
            if !args.is_empty() {
                let rx1_ref = args[0].trim() == "true";
                Some(TciNotification::DiversityRefEx { rx1_ref })
            } else { None }
        }
        "diversity_source_ex" => {
            if !args.is_empty() {
                let source: u32 = args[0].trim().parse().ok()?;
                Some(TciNotification::DiversitySourceEx { source })
            } else { None }
        }
        "diversity_gain_ex" => {
            if args.len() >= 2 {
                let receiver: u32 = args[0].trim().parse().ok()?;
                let gain: u16 = args[1].trim().parse().ok()?;
                Some(TciNotification::DiversityGainEx { receiver, gain })
            } else { None }
        }
        "diversity_phase_ex" => {
            if !args.is_empty() {
                let phase: i32 = args[0].trim().parse().ok()?;
                Some(TciNotification::DiversityPhaseEx { phase })
            } else { None }
        }
        other => {
            // Log unknown commands to discover spot data and new TCI features
            // Keep original case for readability
            let _original_name = cmd.split_once(':').map(|(n, _)| n).unwrap_or(cmd);
            log::debug!("TCI unknown: {}", cmd);
            // Log spot-related commands at info level
            if other.starts_with("spot") || other.starts_with("clicked_on_spot") || other.starts_with("rx_clicked_on_spot") {
                log::debug!("TCI SPOT: {}", cmd);
            }
            None
        }
    }
}

/// Parse a brace-delimited list: `{name1},{name2},{name with {nested} braces}`
/// Returns the contents between each top-level `{` and `}` pair.
pub fn parse_braced_list(s: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut depth = 0i32;
    let mut start = None;

    for (i, ch) in s.char_indices() {
        match ch {
            '{' => {
                if depth == 0 {
                    start = Some(i + 1); // content starts after the opening brace
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s_idx) = start.take() {
                        result.push(s[s_idx..i].to_string());
                    }
                }
            }
            _ => {}
        }
    }
    result
}

// --- Binary payload decoders ---

/// Decode audio payload to stereo (left, right). Right is empty if mono.
pub fn decode_audio_payload_stereo(payload: &[u8], format: u32, length: u32, channels: u32) -> (Vec<f32>, Vec<f32>) {
    let n = length as usize;
    let ch = channels.max(1) as usize;
    let stereo = ch >= 2;
    let mut left = Vec::with_capacity(n);
    let mut right = if stereo { Vec::with_capacity(n) } else { Vec::new() };

    match format {
        SAMPLE_FLOAT32 => {
            let bytes_per_sample = 4 * ch;
            for i in 0..n {
                let offset = i * bytes_per_sample;
                if offset + 4 > payload.len() { break; }
                let l = f32::from_le_bytes([
                    payload[offset], payload[offset + 1],
                    payload[offset + 2], payload[offset + 3],
                ]);
                left.push(l);
                if stereo && offset + 8 <= payload.len() {
                    let r = f32::from_le_bytes([
                        payload[offset + 4], payload[offset + 5],
                        payload[offset + 6], payload[offset + 7],
                    ]);
                    right.push(r);
                }
            }
        }
        SAMPLE_INT16 => {
            let bytes_per_sample = 2 * ch;
            for i in 0..n {
                let offset = i * bytes_per_sample;
                if offset + 2 > payload.len() { break; }
                let l = i16::from_le_bytes([payload[offset], payload[offset + 1]]);
                left.push(l as f32 / 32768.0);
                if stereo && offset + 4 <= payload.len() {
                    let r = i16::from_le_bytes([payload[offset + 2], payload[offset + 3]]);
                    right.push(r as f32 / 32768.0);
                }
            }
        }
        SAMPLE_INT32 => {
            let bytes_per_sample = 4 * ch;
            for i in 0..n {
                let offset = i * bytes_per_sample;
                if offset + 4 > payload.len() { break; }
                let l = i32::from_le_bytes([
                    payload[offset], payload[offset + 1],
                    payload[offset + 2], payload[offset + 3],
                ]);
                left.push(l as f32 / 2147483648.0);
                if stereo && offset + 8 <= payload.len() {
                    let r = i32::from_le_bytes([
                        payload[offset + 4], payload[offset + 5],
                        payload[offset + 6], payload[offset + 7],
                    ]);
                    right.push(r as f32 / 2147483648.0);
                }
            }
        }
        _ => { warn!("TCI: unsupported audio format {}", format); }
    }

    (left, right)
}

/// Decode IQ payload to (I, Q) pairs
pub fn decode_iq_payload(payload: &[u8], format: u32, length: u32, _channels: u32) -> Vec<(f32, f32)> {
    let n = length as usize;
    let mut pairs = Vec::with_capacity(n);

    // IQ: length = total samples across all channels, pairs = length / channels
    let num_pairs = n / _channels.max(1) as usize;

    match format {
        SAMPLE_FLOAT32 => {
            for i in 0..num_pairs {
                let offset = i * 8; // 2 * 4 bytes per I/Q pair
                if offset + 8 > payload.len() {
                    break;
                }
                let i_val = f32::from_le_bytes([
                    payload[offset], payload[offset + 1],
                    payload[offset + 2], payload[offset + 3],
                ]);
                let q_val = f32::from_le_bytes([
                    payload[offset + 4], payload[offset + 5],
                    payload[offset + 6], payload[offset + 7],
                ]);
                pairs.push((i_val, q_val));
            }
        }
        SAMPLE_INT16 => {
            for i in 0..num_pairs {
                let offset = i * 4; // 2 * 2 bytes per I/Q pair
                if offset + 4 > payload.len() {
                    break;
                }
                let i_val = i16::from_le_bytes([payload[offset], payload[offset + 1]]) as f32 / 32768.0;
                let q_val = i16::from_le_bytes([payload[offset + 2], payload[offset + 3]]) as f32 / 32768.0;
                pairs.push((i_val, q_val));
            }
        }
        SAMPLE_INT32 => {
            for i in 0..num_pairs {
                let offset = i * 8; // 2 * 4 bytes per I/Q pair (int32)
                if offset + 8 > payload.len() {
                    break;
                }
                let i_val = i32::from_le_bytes([
                    payload[offset], payload[offset + 1],
                    payload[offset + 2], payload[offset + 3],
                ]) as f32 / 2147483648.0;
                let q_val = i32::from_le_bytes([
                    payload[offset + 4], payload[offset + 5],
                    payload[offset + 6], payload[offset + 7],
                ]) as f32 / 2147483648.0;
                pairs.push((i_val, q_val));
            }
        }
        _ => {
            warn!("TCI: unsupported IQ format {}", format);
        }
    }

    pairs
}

/// Build a TCI binary frame (for TX_AUDIO_STREAM)
pub fn build_tci_binary_frame(
    receiver: u32,
    sample_rate: u32,
    format: u32,
    length: u32,
    stream_type: u32,
    channels: u32,
    samples: &[f32],
    output_format: u32,
) -> Vec<u8> {
    let data_size = match output_format {
        SAMPLE_INT16 => length as usize * channels as usize * 2,
        SAMPLE_FLOAT32 => length as usize * channels as usize * 4,
        _ => length as usize * channels as usize * 4,
    };

    let mut frame = vec![0u8; TCI_HEADER_SIZE + data_size];

    // Header
    frame[0..4].copy_from_slice(&receiver.to_le_bytes());
    frame[4..8].copy_from_slice(&sample_rate.to_le_bytes());
    frame[8..12].copy_from_slice(&format.to_le_bytes());
    // codec (12-16) = 0
    // crc (16-20) = 0
    frame[20..24].copy_from_slice(&length.to_le_bytes());
    frame[24..28].copy_from_slice(&stream_type.to_le_bytes());
    frame[28..32].copy_from_slice(&channels.to_le_bytes());
    // reserved (32-64) = 0

    // Data
    let data = &mut frame[TCI_HEADER_SIZE..];
    match output_format {
        SAMPLE_INT16 => {
            for (i, &s) in samples.iter().enumerate() {
                if i * 2 + 2 > data.len() { break; }
                let s16 = (s * 32767.0).clamp(-32768.0, 32767.0) as i16;
                data[i * 2..i * 2 + 2].copy_from_slice(&s16.to_le_bytes());
            }
        }
        _ => {
            for (i, &s) in samples.iter().enumerate() {
                if i * 4 + 4 > data.len() { break; }
                data[i * 4..i * 4 + 4].copy_from_slice(&s.to_le_bytes());
            }
        }
    }

    frame
}

// --- Mode string ↔ u8 mapping ---
// Thetis CAT modes: 0=LSB, 1=USB, 2=DSB, 3=CWL, 4=CWU, 5=FM, 6=AM, 7=DIGU, 8=SPEC, 9=DIGL, 10=SAM, 11=DRM

pub fn mode_str_to_u8(s: &str) -> u8 {
    match s.to_uppercase().as_str() {
        "LSB" => 0,
        "USB" => 1,
        "DSB" => 2,
        "CWL" | "CW-L" => 3,
        "CWU" | "CW-U" => 4,
        "FM" | "NFM" | "WFM" => 5,
        "AM" => 6,
        "DIGU" => 7,
        "SPEC" => 8,
        "DIGL" => 9,
        "SAM" => 10,
        "DRM" => 11,
        other => {
            warn!("Unknown TCI mode: '{}', defaulting to USB", other);
            1
        }
    }
}

pub fn mode_u8_to_str(mode: u8) -> &'static str {
    match mode {
        0 => "LSB",
        1 => "USB",
        2 => "DSB",
        3 => "CWL",
        4 => "CWU",
        5 => "FM",
        6 => "AM",
        7 => "DIGU",
        8 => "SPEC",
        9 => "DIGL",
        10 => "SAM",
        11 => "DRM",
        _ => "USB",
    }
}
