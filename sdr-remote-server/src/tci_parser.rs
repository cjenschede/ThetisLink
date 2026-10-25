// SPDX-License-Identifier: GPL-2.0-or-later

//! TCI protocol parser - pure functions for parsing TCI text commands,
//! binary frame decoding, and mode conversion. No state dependencies.

use log::{info, warn};

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
    /// S9-frequency threshold push from the Thetis fork (MHz). VFO frequencies
    /// at or above this value switch the S-meter scale to S9 = -93 dBm. Stock
    /// Thetis / extensions-off never push this; the server falls back to the
    /// IARU Region 1 convention (50 MHz) in that case.
    S9FrequencyEx { mhz: f64 },
    RxChannelSensors {
        receiver: u32,
        channel: u32,
        /// Avg dBm (true-mean detector) - always present; legacy non-`_ex` pushes only this slot.
        dbm: f32,
        /// Sig dBm (peak-hold detector). `Some` only for `_ex` format.
        dbm_sig: Option<f32>,
        /// MaxBin dBm (single highest FFT bin). `Some` only for _ex format.
        dbm_peakbin: Option<f32>,
    },
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
    RxNrEnable { receiver: u32, enabled: bool, level: u8 },
    /// calibration_ex:rx,meter_cal,display_cal,xvtr_gain,6m_gain,tx_display
    CalibrationEx { receiver: u32, meter_cal: f32, xvtr_gain: f32, six_m_gain: f32 },
    TxProfilesEx { names: Vec<String> },
    TxProfileEx { name: String },
    /// Server capability flags (Thetis extension)
    TciCapsEx { caps: Vec<String> },
    /// What Thetis' TCI TX queue did in one MOX (fork TL2-5, build 212).
    TxQueueReportEx(TxQueueReport),
    /// A `tx_queue_report_ex` in a format version this server does not read.
    TxQueueReportUnknownVersion { version: String },
    /// A version-1 or version-2 `tx_queue_report_ex` that does not read
    /// (review of 212).
    TxQueueReportUnreadable { line: String },
    /// run_cat_ex response (stock v2.10.3.14 native CAT-relay over TCI):
    /// outgoing  `run_cat_ex:ZZxxx;` / incoming `run_cat_ex:ZZxxx;,response;`
    RunCatExResponse { cat_cmd: String, response: String },
    // ThetisLink extended controls
    CtunEx { receiver: u32, enabled: bool },
    VfoSyncEx { enabled: bool },
    FmDeviationEx { receiver: u32, hz: u32 },
    // Stock Thetis v2.10.3.14 native sample rate / IF limits (global, not per-RX)
    /// `iq_samplerate:<rate>;` - primary source for DDC sample rate (Hz)
    IqSamplerate { rate: u32 },
    /// `if_limits:<low>,<high>;` - IF range in Hz; sample rate = high - low
    /// (fallback when iq_samplerate does not arrive)
    IfLimits { low: i32, high: i32 },
    // Stock Thetis v2.10.3.14 native attenuator/preamp commands
    RxStepAttEx { receiver: u32, db: u32 },
    RxStepAttEnabledEx { receiver: u32, enabled: bool },
    RxPreampAttEx { receiver: u32, db: i32 },
    // Stock Thetis v2.10.3.14 native TX commands
    TxFilterBandEx { low: i32, high: i32 },
    TxFrequencyEx { freq: u64, band: String, rx2_enabled: bool, tx_vfob: bool },
    DiversityEnableEx { enabled: bool },
    /// rx_only_ex echo - Thetis' current "Receive only" state. Used by the
    /// TL-server to track the live RXOnly so it can snapshot/restore around
    /// an RX-only Amplitec position.
    RxOnlyEx { rx_only: bool },
    /// vfo_source_ex:<rx>,tl|thetis - who changed this receiver's VFO last
    /// (fork TL2-8, build 218).
    VfoSourceEx { receiver: u32, source: crate::ctun_recenter::VfoSource },
    DiversityRefEx { rx1_ref: bool },
    DiversitySourceEx { source: u32 },
    DiversityGainEx { receiver: u32, gain: u16 },
    DiversityGainMultiEx { multi: u16 },
    DdcSampleRateEx { receiver: u32, rate_hz: u32 },
    DiversityPhaseEx { phase: i32 },
    // Binary stream notifications
    RxAudio { receiver: u32, samples: Vec<f32> },
    IqStream { receiver: u32, iq_pairs: Vec<(f32, f32)> },
    TxChrono { samples_requested: u32, sample_rate: u32, channels: u32, format: u32 },
    // Connection events
    Disconnected,
}

/// Versions 1 and 2 are read. Version 2 (TL2-10, build 230) has four fields
/// more before the trigger:
/// `...,<open requests>,<average ms>,<gaps>,<longest gap ms>,<tail ms>,<trx|mox>`.
///
/// One `tx_queue_report_ex` from the Thetis fork (TL2-5, build 212): what its
/// TCI TX queue did during one MOX. Format version 1, written by
/// `cmaster.TakeTCITxOverReport` in the fork - the same agreement in two
/// programs, so the first field is a version and anything else is refused out
/// loud rather than read into the wrong fields:
/// `tx_queue_report_ex:1,<rate>,<block samples>,<over ms>,<lead zero ms>,<lowest ms|-1>,<zero ms>,<underruns>,<left ms>,<open requests>,<trx|mox>`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxQueueReport {
    pub rate: u32,
    pub block_samples: u32,
    pub over_ms: u32,
    /// Zeros the DSP got before the first real sample of the over.
    pub lead_zero_ms: u32,
    /// The lowest the queue stood when the DSP asked for a block, after the
    /// first audio; `None` when no audio came at all.
    pub lowest_ms: Option<u32>,
    /// Zeros after the first audio, in `underruns` blocks.
    pub zero_ms: u32,
    pub underruns: u32,
    /// What was still queued when the over ended (dropped by Thetis).
    pub left_ms: u32,
    pub open_requests: u32,
    /// `trx` (the client switched TRX off) or `mox` (MOX ended another way).
    pub trigger: String,
    /// What version 2 adds; `None` from version 1.
    pub shape: Option<QueueShape>,
}

/// The shape of the queue over one MOX, from version 2 (build 230): what the
/// field ride of 2026-10-16 could not tell - whether Thetis holds ~10-30 ms or
/// ~100, and whether 138 ms of zeros was one gap or a hundred clicks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueShape {
    /// The average level of the queue when the DSP asked for a block, after
    /// the first audio.
    pub average_ms: u32,
    /// Runs of blocks with zeros after the first audio.
    pub gaps: u32,
    /// The longest of those runs.
    pub longest_ms: u32,
    /// The run still open when the over ended: the tail between the last
    /// audio and MOX off, not a dropout.
    pub tail_ms: u32,
}

impl TxQueueReport {
    /// The fields after the version, in the order version 1 sends them.
    fn from_v1(args: &[&str]) -> Option<TxQueueReport> {
        if args.len() != 10 {
            return None;
        }
        Self::from_fields(args, None)
    }

    /// Version 2: the nine numbers of version 1, four more, then the trigger.
    fn from_v2(args: &[&str]) -> Option<TxQueueReport> {
        if args.len() != 14 {
            return None;
        }
        let n = |i: usize| args[i].trim().parse::<u32>().ok();
        let shape = QueueShape { average_ms: n(9)?, gaps: n(10)?, longest_ms: n(11)?, tail_ms: n(12)? };
        let mut fields: Vec<&str> = args[..9].to_vec();
        fields.push(args[13]);
        Self::from_fields(&fields, Some(shape))
    }

    /// The nine numbers and the trigger that both versions share.
    fn from_fields(args: &[&str], shape: Option<QueueShape>) -> Option<TxQueueReport> {
        let n = |i: usize| args[i].trim().parse::<u32>().ok();
        let lowest = args[4].trim();
        Some(TxQueueReport {
            rate: n(0)?,
            block_samples: n(1)?,
            over_ms: n(2)?,
            lead_zero_ms: n(3)?,
            lowest_ms: if lowest == "-1" { None } else { Some(lowest.parse().ok()?) },
            zero_ms: n(5)?,
            underruns: n(6)?,
            left_ms: n(7)?,
            open_requests: n(8)?,
            trigger: args[9].trim().to_string(),
            shape,
        })
    }

    /// The server log line, named in advance (`logregels-build212.md`; the
    /// shape of version 2 in `logregels-build230.md`).
    pub fn line(&self) -> String {
        let shape = self.shape.map_or_else(String::new, |s| {
            format!(
                "; average {} ms queued, {} gap(s), longest {} ms, {} ms of it at the end",
                s.average_ms, s.gaps, s.longest_ms, s.tail_ms
            )
        });
        format!(
            "Thetis TX queue, previous over ({}): {} ms, {} ms of zeros before the first audio, lowest {} ms queued, \
             {} ms of zeros in {} underrun(s), {} ms left at the end, {} request(s) open{}; blocks of {} samples at {} Hz",
            self.trigger,
            self.over_ms,
            self.lead_zero_ms,
            self.lowest_ms.map_or_else(|| "-".to_string(), |v| v.to_string()),
            self.zero_ms,
            self.underruns,
            self.left_ms,
            self.open_requests,
            shape,
            self.block_samples,
            self.rate
        )
    }
}

// ── Parser functions ────────────────────────────────────────────────

pub fn parse_tci_text(cmd: &str) -> Option<TciNotification> {
    // Commands are case-insensitive per spec
    // Handle tx_profiles_ex / tx_profile_ex BEFORE lowercase (names are case-sensitive)
    // and before comma-split (names can contain commas and braces)
    let cmd_lower = cmd.to_lowercase();
    // run_cat_ex carries embedded `;` inside its payload (CAT-cmd echo + response),
    // so it must be parsed before the regular `:` split. Original case preserved.
    if let Some(colon_pos) = cmd_lower.find("run_cat_ex:") {
        let payload = &cmd[colon_pos + "run_cat_ex:".len()..];
        if let Some(comma_pos) = payload.find(',') {
            let cat_cmd = payload[..comma_pos].trim_end_matches(';').trim().to_string();
            let response = payload[comma_pos + 1..].trim_end_matches(';').trim().to_string();
            if !cat_cmd.is_empty() {
                return Some(TciNotification::RunCatExResponse { cat_cmd, response });
            }
        }
        return None;
    }
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
        "tx_queue_report_ex" => match args.first().map(|v| v.trim()) {
            Some("1") => Some(
                TxQueueReport::from_v1(&args[1..])
                    .map(TciNotification::TxQueueReportEx)
                    .unwrap_or_else(|| TciNotification::TxQueueReportUnreadable { line: args_str.to_string() }),
            ),
            Some("2") => Some(
                TxQueueReport::from_v2(&args[1..])
                    .map(TciNotification::TxQueueReportEx)
                    .unwrap_or_else(|| TciNotification::TxQueueReportUnreadable { line: args_str.to_string() }),
            ),
            Some(version) => Some(TciNotification::TxQueueReportUnknownVersion { version: version.to_string() }),
            None => None,
        },
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
        "s9_frequency_ex" => {
            // Format: `s9_frequency_ex:<mhz>;` - Thetis pushes its
            // user-configurable S9-frequency threshold (default 30 MHz).
            if args.is_empty() { return None; }
            let mhz: f64 = args[0].trim().parse().ok()?;
            Some(TciNotification::S9FrequencyEx { mhz })
        }
        "rx_channel_sensors" => {
            // Always parsed: whether it counts is the connection's to decide,
            // from how recently the _ex format arrived (Tci, RxChannelSensors).
            if args.len() >= 3 {
                let receiver = args[0].trim().parse().ok()?;
                let channel = args[1].trim().parse().ok()?;
                let dbm = args[2].trim().parse().ok()?;
                Some(TciNotification::RxChannelSensors {
                    receiver,
                    channel,
                    dbm,
                    dbm_sig: None,
                    dbm_peakbin: None,
                })
            } else {
                None
            }
        }
        "rx_channel_sensors_ex" => {
            // Extended format: rx_channel_sensors_ex:rx,chan,dBm,avgdBm,peakBinDbm
            // - dBm (field 2)         = "Sig" source (peak-hold detector)
            // - avgdBm (field 3)      = "Avg" source (true-mean detector)
            // - peakBinDbm (field 4)  = single highest FFT bin in passband
            // All three are pushed at every sensor tick; the server keeps all three
            // cached so a client can pick its preferred source via SmeterSource.
            if args.len() >= 5 {
                let receiver = args[0].trim().parse().ok()?;
                let channel = args[1].trim().parse().ok()?;
                let sig_dbm: f32 = args[2].trim().parse().ok()?;
                let avg_dbm: f32 = args[3].trim().parse().ok()?;
                let peak_bin_dbm: f32 = args[4].trim().parse().ok()?;
                Some(TciNotification::RxChannelSensors {
                    receiver,
                    channel,
                    dbm: avg_dbm,
                    dbm_sig: Some(sig_dbm),
                    dbm_peakbin: Some(peak_bin_dbm),
                })
            } else {
                None
            }
        }
        "rx_sensors" => {
            // Legacy format: rx_sensors:receiver,dbm
            if args.len() >= 2 {
                let receiver = args[0].trim().parse().ok()?;
                let dbm = args[1].trim().parse().ok()?;
                Some(TciNotification::RxChannelSensors {
                    receiver,
                    channel: 0,
                    dbm,
                    dbm_sig: None,
                    dbm_peakbin: None,
                })
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
        // ── Stock Thetis (v2.10.3.13+) and ThetisLink extended controls ──
        "rx_ctun_ex" => {
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
        // Stock v2.10.3.14+: iq_samplerate is the global IQ stream / DDC sample rate (Hz).
        // Primary source for tci.ddc_sample_rate_rx1/rx2 in TL2 v2.0.0-alpha-3+.
        "iq_samplerate" => {
            if !args.is_empty() {
                let rate: u32 = args[0].trim().parse().ok()?;
                Some(TciNotification::IqSamplerate { rate })
            } else { None }
        }
        // Stock v2.10.3.14+: if_limits is the IF range (low, high in Hz, relative to DDS center).
        // Fallback source for DDC sample rate when iq_samplerate is absent (rate ≈ high - low).
        "if_limits" => {
            if args.len() >= 2 {
                let low: i32 = args[0].trim().parse().ok()?;
                let high: i32 = args[1].trim().parse().ok()?;
                Some(TciNotification::IfLimits { low, high })
            } else { None }
        }
        // Stock v2.10.3.14: rx_step_att_ex sends |attenuation| (positive only)
        "rx_step_att_ex" => {
            if args.len() >= 2 {
                let receiver: u32 = args[0].trim().parse().ok()?;
                let db: u32 = args[1].trim().parse().ok()?;
                Some(TciNotification::RxStepAttEx { receiver, db })
            } else { None }
        }
        "rx_step_att_enabled_ex" => {
            if args.len() >= 2 {
                let receiver: u32 = args[0].trim().parse().ok()?;
                let enabled = args[1].trim() == "true";
                Some(TciNotification::RxStepAttEnabledEx { receiver, enabled })
            } else { None }
        }
        // Stock v2.10.3.14: rx_preamp_att_ex carries signed value (negative = gain in preamp mode)
        "rx_preamp_att_ex" => {
            if args.len() >= 2 {
                let receiver: u32 = args[0].trim().parse().ok()?;
                let db: i32 = args[1].trim().parse().ok()?;
                Some(TciNotification::RxPreampAttEx { receiver, db })
            } else { None }
        }
        // Stock v2.10.3.14: tx_filter_band_ex carries low/high cut Hz for TX
        "tx_filter_band_ex" => {
            if args.len() >= 2 {
                let low: i32 = args[0].trim().parse().ok()?;
                let high: i32 = args[1].trim().parse().ok()?;
                Some(TciNotification::TxFilterBandEx { low, high })
            } else { None }
        }
        // Stock v2.10.3.14: tx_frequency_ex:freq,band,rx2_enabled,tx_vfob
        "tx_frequency_ex" => {
            if args.len() >= 4 {
                let freq: u64 = args[0].trim().parse().ok()?;
                let band = args[1].trim().to_string();
                let rx2_enabled = args[2].trim() == "true";
                let tx_vfob = args[3].trim() == "true";
                Some(TciNotification::TxFrequencyEx { freq, band, rx2_enabled, tx_vfob })
            } else { None }
        }
        "diversity_enable_ex" => {
            if !args.is_empty() {
                let enabled = args[0].trim() == "true";
                Some(TciNotification::DiversityEnableEx { enabled })
            } else { None }
        }
        "rx_only_ex" => {
            if !args.is_empty() {
                let rx_only = args[0].trim() == "true";
                Some(TciNotification::RxOnlyEx { rx_only })
            } else { None }
        }
        "vfo_source_ex" => {
            if args.len() >= 2 {
                let receiver: u32 = args[0].trim().parse().ok()?;
                let source = match args[1].trim() {
                    "tl" => crate::ctun_recenter::VfoSource::ThetisLink,
                    "thetis" => crate::ctun_recenter::VfoSource::Thetis,
                    _ => return None,
                };
                Some(TciNotification::VfoSourceEx { receiver, source })
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
        "diversity_gain_multi_ex" => {
            if !args.is_empty() {
                let multi: u16 = args[0].trim().parse().ok()?;
                Some(TciNotification::DiversityGainMultiEx { multi })
            } else { None }
        }
        "ddc_sample_rate_ex" => {
            // TL2-1 fork extension: per-RX rate (`rx,rate_hz`). Coexists with stock global
            // `iq_samplerate:rate;` which is parsed by another arm - that one set both
            // RX rates equal; this one updates only the named receiver.
            if args.len() >= 2 {
                let receiver: u32 = args[0].trim().parse().ok()?;
                let rate_hz: u32 = args[1].trim().parse().ok()?;
                if receiver > 1 { return None; }
                if !matches!(rate_hz, 48000 | 96000 | 192000 | 384000 | 768000 | 1536000) {
                    return None;
                }
                Some(TciNotification::DdcSampleRateEx { receiver, rate_hz })
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

// --- Mode string <-> u8 mapping ---
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Who tuned last, per receiver (fork TL2-8, build 218); anything else is
    /// not a report.
    #[test]
    fn vfo_source_ex_reads_tl_and_thetis() {
        use crate::ctun_recenter::VfoSource;
        let read = |line: &str| match parse_tci_text(line) {
            Some(TciNotification::VfoSourceEx { receiver, source }) => Some((receiver, source)),
            _ => None,
        };
        assert_eq!(read("vfo_source_ex:0,thetis"), Some((0, VfoSource::Thetis)));
        assert_eq!(read("vfo_source_ex:1,tl"), Some((1, VfoSource::ThetisLink)));
        assert_eq!(read("vfo_source_ex:0,both"), None);
        assert_eq!(read("vfo_source_ex:0"), None);
        assert_eq!(read("vfo_source_ex:x,tl"), None);
    }

    #[test]
    fn ddc_sample_rate_ex_valid() {
        for &rate in &[48000u32, 96000, 192000, 384000, 768000, 1536000] {
            for rx in 0..=1u32 {
                let line = format!("ddc_sample_rate_ex:{},{}", rx, rate);
                match parse_tci_text(&line) {
                    Some(TciNotification::DdcSampleRateEx { receiver, rate_hz }) => {
                        assert_eq!(receiver, rx);
                        assert_eq!(rate_hz, rate);
                    }
                    Some(_) => panic!("expected DdcSampleRateEx variant for {:?}", line),
                    None => panic!("expected Some(DdcSampleRateEx), got None for {:?}", line),
                }
            }
        }
    }

    #[test]
    fn ddc_sample_rate_ex_rejects_out_of_range_receiver() {
        for rx in [2u32, 3, 99, u32::MAX] {
            let line = format!("ddc_sample_rate_ex:{},384000", rx);
            assert!(parse_tci_text(&line).is_none(), "should reject rx={}", rx);
        }
    }

    /// Version 1 from the fork reads into its named fields, and the server
    /// line says each of them; another version is refused out loud, and a
    /// short or broken line is not read at all (build 212).
    #[test]
    fn tx_queue_report_ex_reads_version_one_and_refuses_the_rest() {
        let line = "tx_queue_report_ex:1,48000,720,3460,180,45,30,2,60,0,trx";
        let want = TxQueueReport {
            rate: 48000,
            block_samples: 720,
            over_ms: 3460,
            lead_zero_ms: 180,
            lowest_ms: Some(45),
            zero_ms: 30,
            underruns: 2,
            left_ms: 60,
            open_requests: 0,
            trigger: "trx".to_string(),
            shape: None,
        };
        match parse_tci_text(line) {
            Some(TciNotification::TxQueueReportEx(r)) => {
                assert_eq!(r, want);
                assert_eq!(
                    r.line(),
                    "Thetis TX queue, previous over (trx): 3460 ms, 180 ms of zeros before the first audio, lowest 45 ms queued, \
                     30 ms of zeros in 2 underrun(s), 60 ms left at the end, 0 request(s) open; blocks of 720 samples at 48000 Hz"
                );
            }
            _ => panic!("version 1 not read"),
        }
        // No audio in the over: no lowest level.
        match parse_tci_text("tx_queue_report_ex:1,48000,720,300,300,-1,0,0,0,5,mox") {
            Some(TciNotification::TxQueueReportEx(r)) => {
                assert_eq!(r.lowest_ms, None);
                assert!(r.line().contains("lowest - ms queued"), "{}", r.line());
            }
            _ => panic!("a report without audio not read"),
        }
        // Version 2 is read since build 230; version 3 is the unknown one now.
        assert!(matches!(
            parse_tci_text("tx_queue_report_ex:3,48000,720,3460,180,45,30,2,60,0,trx,more"),
            Some(TciNotification::TxQueueReportUnknownVersion { version }) if version == "3"
        ));
        // A version 1 that does not read is said, not dropped (review of 212).
        for (line, why) in [
            ("tx_queue_report_ex:1,48000,720,3460", "too short"),
            ("tx_queue_report_ex:1,48000,720,3460,180,45,30,2,60,0,trx,more", "a field too many"),
            ("tx_queue_report_ex:1,48000,720,3460,180,x,30,2,60,0,trx", "not a number"),
        ] {
            assert!(
                matches!(parse_tci_text(line), Some(TciNotification::TxQueueReportUnreadable { .. })),
                "{why}"
            );
        }
    }

    /// Version 2 (fork TL2-10, build 230): four fields more before the
    /// trigger - the average level, the gaps, the longest and the one still
    /// open at the end. Version 1 still reads; a version 2 that does not read
    /// is said; version 3 is refused out loud.
    #[test]
    fn tx_queue_report_ex_reads_version_two_with_its_queue_shape() {
        let line = "tx_queue_report_ex:2,48000,64,10148,2,13,124,93,0,4,21,3,96,12,trx";
        match parse_tci_text(line) {
            Some(TciNotification::TxQueueReportEx(r)) => {
                assert_eq!((r.over_ms, r.lowest_ms, r.zero_ms, r.underruns, r.left_ms, r.open_requests), (10148, Some(13), 124, 93, 0, 4));
                assert_eq!(r.trigger, "trx");
                assert_eq!(r.shape, Some(QueueShape { average_ms: 21, gaps: 3, longest_ms: 96, tail_ms: 12 }));
                assert_eq!(
                    r.line(),
                    "Thetis TX queue, previous over (trx): 10148 ms, 2 ms of zeros before the first audio, lowest 13 ms queued, \
                     124 ms of zeros in 93 underrun(s), 0 ms left at the end, 4 request(s) open; \
                     average 21 ms queued, 3 gap(s), longest 96 ms, 12 ms of it at the end; blocks of 64 samples at 48000 Hz"
                );
            }
            _ => panic!("version 2 not read"),
        }
        // Version 1 reads as before, without a shape.
        match parse_tci_text("tx_queue_report_ex:1,48000,720,3460,180,45,30,2,60,0,trx") {
            Some(TciNotification::TxQueueReportEx(r)) => {
                assert_eq!(r.shape, None);
                assert!(!r.line().contains("average"), "{}", r.line());
            }
            _ => panic!("version 1 not read"),
        }
        for (line, why) in [
            ("tx_queue_report_ex:2,48000,64,10148,2,13,124,93,0,4,21,3,96,trx", "a field short"),
            ("tx_queue_report_ex:2,48000,64,10148,2,13,124,93,0,4,21,3,96,12,trx,more", "a field too many"),
            ("tx_queue_report_ex:2,48000,64,10148,2,13,124,93,0,4,x,3,96,12,trx", "not a number"),
            ("tx_queue_report_ex:2,48000,720,3460,180,45,30,2,60,0,trx", "a version 1 body under version 2"),
        ] {
            assert!(
                matches!(parse_tci_text(line), Some(TciNotification::TxQueueReportUnreadable { .. })),
                "{why}"
            );
        }
        assert!(matches!(
            parse_tci_text("tx_queue_report_ex:3,48000,64,10148,2,13,124,93,0,4,21,3,96,12,0,trx"),
            Some(TciNotification::TxQueueReportUnknownVersion { version }) if version == "3"
        ));
    }

    #[test]
    fn ddc_sample_rate_ex_rejects_invalid_rate() {
        for rate in [0u32, 1, 12345, 44100, 384001, u32::MAX] {
            let line = format!("ddc_sample_rate_ex:0,{}", rate);
            assert!(parse_tci_text(&line).is_none(), "should reject rate={}", rate);
        }
    }
}

/// Is this mode code one of the CW modes (CWL = 3, CWU = 4)? Thetis refuses
/// `keyer:` silently in any other mode, so the server refuses out loud first.
pub fn is_cw_mode(code: u8) -> bool {
    matches!(code, 3 | 4)
}

#[cfg(test)]
mod cw_mode_tests {
    #[test]
    fn only_the_two_cw_codes_are_cw() {
        assert!(super::is_cw_mode(3) && super::is_cw_mode(4));
        for other in [0u8, 1, 2, 5, 6, 7, 8, 9, 10, 11, 12, 255] {
            assert!(!super::is_cw_mode(other), "{other}");
        }
    }
}
