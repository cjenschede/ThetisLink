// SPDX-License-Identifier: GPL-2.0-or-later

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{debug, info, warn};
use serde::Deserialize;

/// RF2K-S power amplifier HTTP controller.
/// Communicates via REST API on the Raspberry Pi (port 8080).
/// Polls status every ~200ms, sends commands via HTTP.
pub struct Rf2k {
    cmd_tx: mpsc::Sender<Rf2kCmd>,
    status: Arc<Mutex<Rf2kStatus>>,
}

/// Observed operate-mode for the drive observer.
/// Thin wrapper so the observer event type reads naturally in match patterns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rf2kMode {
    Standby,
    Operate,
}

#[derive(Clone, Debug)]
pub struct Rf2kStatus {
    pub connected: bool,
    // Operate/PTT
    pub operate: bool,
    pub ptt: bool,
    // Band/Freq
    pub band: u8,               // index 0-10 (BAND_VALUES mapping)
    pub frequency_khz: u16,
    // Power
    pub temperature_x10: u16,   // degC x 10
    pub voltage_x10: u16,       // V x 10
    pub current_x10: u16,       // A x 10
    pub forward_w: u16,
    pub reflected_w: u16,
    pub swr_x100: u16,          // x 100 (125 = 1.25)
    pub max_forward_w: u16,
    pub max_reflected_w: u16,
    pub max_swr_x100: u16,
    // Error
    pub error_state: u8,        // 0=None
    pub error_text: String,
    // Antenna
    pub antenna_type: u8,       // 0=Internal, 1=External
    pub antenna_number: u8,     // 1-4
    // Tuner
    pub tuner_mode: u8,         // 0=OFF,1=BYPASS,2=MANUAL,3=AUTO_TUNING,4=AUTO,5=AUTO_FROM_AUTO
    pub tuner_setup: String,    // "CL"/"LC"/"BYPASS"/"NOT TUNED"
    pub tuner_l_nh: u16,
    pub tuner_c_pf: u16,
    pub tuner_freq_khz: u16,
    pub segment_size_khz: u16,
    // Drive
    pub drive_w: u16,
    pub modulation: String,     // "SSB"/"AM"/"CONT"
    pub max_power_w: u16,
    // Device
    pub device_name: String,
    // Debug (Fase D)
    pub debug_available: bool,
    pub bias_pct_x10: u16,
    pub psu_source: u8,
    pub uptime_s: u32,
    pub tx_time_s: u32,
    pub error_count: u16,
    pub error_history: Vec<(String, String)>, // (time, error)
    pub storage_bank: u16,
    pub hw_revision: String,
    pub frq_delay: u16,
    pub autotune_threshold_x10: u16,
    pub dac_alc: u16,
    pub high_power: bool,
    pub tuner_6m: bool,
    pub band_gap_allowed: bool,
    pub controller_version: u16,
    // Drive config (Fase D)
    pub drive_config_ssb: [u8; 11],
    pub drive_config_am: [u8; 11],
    pub drive_config_cont: [u8; 11],
}

impl Default for Rf2kStatus {
    fn default() -> Self {
        Self {
            connected: false,
            operate: false,
            ptt: false,
            band: 0,
            frequency_khz: 0,
            temperature_x10: 0,
            voltage_x10: 0,
            current_x10: 0,
            forward_w: 0,
            reflected_w: 0,
            swr_x100: 100,
            max_forward_w: 0,
            max_reflected_w: 0,
            max_swr_x100: 100,
            error_state: 0,
            error_text: String::new(),
            antenna_type: 0,
            antenna_number: 1,
            tuner_mode: 0,
            tuner_setup: String::new(),
            tuner_l_nh: 0,
            tuner_c_pf: 0,
            tuner_freq_khz: 0,
            segment_size_khz: 0,
            drive_w: 0,
            modulation: String::new(),
            max_power_w: 0,
            device_name: String::new(),
            debug_available: false,
            bias_pct_x10: 0,
            psu_source: 0,
            uptime_s: 0,
            tx_time_s: 0,
            error_count: 0,
            error_history: Vec::new(),
            storage_bank: 0,
            hw_revision: String::new(),
            frq_delay: 0,
            autotune_threshold_x10: 0,
            dac_alc: 0,
            high_power: false,
            tuner_6m: false,
            band_gap_allowed: false,
            controller_version: 0,
            drive_config_ssb: [0; 11],
            drive_config_am: [0; 11],
            drive_config_cont: [0; 11],
        }
    }
}

pub enum Rf2kCmd {
    SetOperate(bool),           // true=Operate, false=Standby
    Tune,
    SetAntenna { antenna_type: u8, number: u8 }, // type 0=Internal, 1=External
    ErrorReset,
    Close,                      // FW Close (shutdown Pi)
    // Tuner controls (Fase B)
    TunerMode(u8),              // 0=MANUAL, 1=AUTO
    TunerBypass(bool),          // true=bypass on, false=bypass off
    TunerReset,
    TunerStore,
    TunerLUp,
    TunerLDown,
    TunerCUp,
    TunerCDown,
    TunerK,                     // Cycle K coefficient (CL→LC→BYPASS)
    // Drive controls (Fase C)
    DriveUp,
    DriveDown,
    // Debug controls (Fase D)
    SetHighPower(bool),
    SetTuner6m(bool),
    SetBandGap(bool),
    FrqDelayUp,
    FrqDelayDown,
    AutotuneThresholdUp,
    AutotuneThresholdDown,
    DacAlcUp,
    DacAlcDown,
    ZeroFRAM,
    SetDriveConfig { category: u8, band: u8, value: u8 },
}

// JSON response structs for Pi REST API
#[derive(Deserialize)]
struct ValueUnit {
    value: serde_json::Value,
    #[allow(dead_code)]
    unit: Option<String>,
}

#[derive(Deserialize)]
struct ValueMaxUnit {
    value: serde_json::Value,
    max_value: Option<serde_json::Value>,
    #[allow(dead_code)]
    unit: Option<String>,
}

#[derive(Deserialize)]
struct PowerResponse {
    temperature: ValueUnit,
    voltage: ValueUnit,
    current: ValueUnit,
    forward: ValueMaxUnit,
    reflected: ValueMaxUnit,
    swr: ValueMaxUnit,
}

#[derive(Deserialize)]
struct DataResponse {
    band: ValueUnit,
    frequency: ValueUnit,
    status: String,
}

#[derive(Deserialize)]
struct TunerResponse {
    mode: String,
    setup: Option<String>,
    #[serde(rename = "L")]
    l: Option<ValueUnit>,
    #[serde(rename = "C")]
    c: Option<ValueUnit>,
    tuned_frequency: Option<ValueUnit>,
    segment_size: Option<ValueUnit>,
}

#[derive(Deserialize)]
struct AntennaResponse {
    #[serde(rename = "type")]
    antenna_type: String,
    number: u8,
}

#[derive(Deserialize)]
struct OperateResponse {
    operate_mode: String,
}

#[derive(Deserialize)]
struct DriveResponse {
    has_drive: bool,
    drive_power: u16,
    modulation: Option<String>,
    max_power: Option<u16>,
}

#[derive(Deserialize)]
struct InfoResponse {
    device: Option<String>,
    #[allow(dead_code)]
    software_version: Option<serde_json::Value>,
    #[allow(dead_code)]
    custom_device_name: Option<String>,
}

#[derive(Deserialize)]
struct DebugResponse {
    bias_pct: Option<f64>,
    psu_source: Option<serde_json::Value>,
    uptime_s: Option<u32>,
    tx_time_s: Option<u32>,
    error_count: Option<u16>,
    error_history: Option<Vec<ErrorHistoryEntry>>,
    storage_bank: Option<serde_json::Value>,
    hw_revision: Option<String>,
    frq_delay: Option<serde_json::Value>,
    autotune_threshold: Option<f64>,
    dac_alc: Option<serde_json::Value>,
    high_power: Option<bool>,
    tuner_6m: Option<bool>,
    band_gap_allowed: Option<bool>,
    controller_version: Option<u16>,
}

#[derive(Deserialize)]
struct ErrorHistoryEntry {
    time: String,
    error: String,
}

#[derive(Deserialize)]
struct DriveConfigResponse {
    ssb: Vec<u8>,
    am: Vec<u8>,
    cont: Vec<u8>,
}

impl Rf2k {
    /// Create an RF2K-S controller.
    ///
    /// `thetis_drive_level` is an optional shared atomic holding the current
    /// Thetis drive percentage as parsed from CAT/TCI. When provided, the
    /// drive-restore observer runs alongside the poll loop and logs
    /// RF2K-S operate-mode transitions against Thetis' ZZPC readings. When
    /// `None`, the observer is disabled and the controller behaves as before.
    pub fn new(
        addr: &str,
        cat_tx: Option<tokio::sync::mpsc::Sender<String>>,
        thetis_drive_level: Option<Arc<AtomicU8>>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Rf2kCmd>();
        let status = Arc::new(Mutex::new(Rf2kStatus::default()));

        let status_for_thread = status.clone();
        let base_url = format!("http://{}", addr);

        std::thread::Builder::new()
            .name("rf2k-http".to_string())
            .spawn(move || {
                rf2k_thread(cmd_rx, status_for_thread, &base_url, cat_tx, thetis_drive_level);
            })
            .expect("Failed to spawn RF2K thread");

        Self { cmd_tx, status }
    }

    pub fn send_command(&self, cmd: Rf2kCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn status(&self) -> Rf2kStatus {
        self.status.lock().unwrap().clone()
    }
}

fn rf2k_thread(
    cmd_rx: mpsc::Receiver<Rf2kCmd>,
    status: Arc<Mutex<Rf2kStatus>>,
    base_url: &str,
    cat_tx: Option<tokio::sync::mpsc::Sender<String>>,
    thetis_drive_level: Option<Arc<AtomicU8>>,
) {
    info!("RF2K-S HTTP thread started, polling {}", base_url);

    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(3))
        .connection_verbose(false)
        .pool_max_idle_per_host(0) // no keep-alive: fresh TCP per request
        .build()
        .expect("Failed to create HTTP client");

    let mut consecutive_failures: u32 = 0;
    let mut tune_carrier_active = false;
    let mut poll_cycle: u32 = 0;
    let mut drive_config_fetched = false;
    let mut drive_observer = DriveObserverState::new();

    // Fetch device info once at startup
    match fetch_info(&client, base_url) {
        Ok(name) => {
            let mut s = status.lock().unwrap();
            s.device_name = name;
            info!("RF2K-S device: {}", s.device_name);
        }
        Err(e) => {
            warn!("RF2K-S initial info fetch failed: {}", e);
        }
    }

    loop {
        match cmd_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Rf2kCmd::Tune) => {
                // Automated tune sequence: ZZTU1 → wait → tune → wait → poll → ZZTU0
                if let Some(ref tx) = cat_tx {
                    let _ = tx.blocking_send("ZZTU1;".to_string());
                    info!("RF2K-S tune: carrier ON (ZZTU1)");
                }
                // Wait for Thetis to start carrier + RF2K-S to detect input power
                std::thread::sleep(Duration::from_millis(1500));
                execute_command(&client, base_url, &Rf2kCmd::Tune);
                // Wait for Pi to enter AUTO_TUNING state before polling
                std::thread::sleep(Duration::from_millis(2000));
                tune_carrier_active = true;
            }
            Ok(cmd) => {
                let is_drive_config = matches!(cmd, Rf2kCmd::SetDriveConfig { .. });
                execute_command(&client, base_url, &cmd);
                // Refetch drive config after SetDriveConfig
                if is_drive_config {
                    if let Ok(dc) = fetch_drive_config(&client, base_url) {
                        apply_drive_config(&mut status.lock().unwrap(), &dc);
                    }
                }
                // Brief delay then poll
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Normal poll cycle
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                info!("RF2K-S command channel closed, stopping");
                break;
            }
        }

        // Poll status
        match poll_status(&client, base_url) {
            Ok(new_status) => {
                // Check if tune sequence completed
                if tune_carrier_active {
                    // tuner_mode 3=AUTO_TUNING, 5=AUTO_TUNING_FROM_AUTO
                    if new_status.tuner_mode != 3 && new_status.tuner_mode != 5 {
                        tune_carrier_active = false;
                        if let Some(ref tx) = cat_tx {
                            let _ = tx.blocking_send("ZZTU0;".to_string());
                            info!("RF2K-S tune: complete, carrier OFF (ZZTU0)");
                        }
                    }
                }

                {
                    let mut s = status.lock().unwrap();
                    // Preserve debug fields that are updated separately
                    let device_name = s.device_name.clone();
                    let debug_available = s.debug_available;
                    let bias_pct_x10 = s.bias_pct_x10;
                    let psu_source = s.psu_source;
                    let uptime_s = s.uptime_s;
                    let tx_time_s = s.tx_time_s;
                    let error_count = s.error_count;
                    let error_history = std::mem::take(&mut s.error_history);
                    let storage_bank = s.storage_bank;
                    let hw_revision = std::mem::take(&mut s.hw_revision);
                    let frq_delay = s.frq_delay;
                    let autotune_threshold_x10 = s.autotune_threshold_x10;
                    let dac_alc = s.dac_alc;
                    let high_power = s.high_power;
                    let tuner_6m = s.tuner_6m;
                    let band_gap_allowed = s.band_gap_allowed;
                    let controller_version = s.controller_version;
                    let drive_config_ssb = s.drive_config_ssb;
                    let drive_config_am = s.drive_config_am;
                    let drive_config_cont = s.drive_config_cont;

                    *s = new_status;
                    s.device_name = device_name;
                    s.connected = true;
                    // Restore debug/drive fields
                    s.debug_available = debug_available;
                    s.bias_pct_x10 = bias_pct_x10;
                    s.psu_source = psu_source;
                    s.uptime_s = uptime_s;
                    s.tx_time_s = tx_time_s;
                    s.error_count = error_count;
                    s.error_history = error_history;
                    s.storage_bank = storage_bank;
                    s.hw_revision = hw_revision;
                    s.frq_delay = frq_delay;
                    s.autotune_threshold_x10 = autotune_threshold_x10;
                    s.dac_alc = dac_alc;
                    s.high_power = high_power;
                    s.tuner_6m = tuner_6m;
                    s.band_gap_allowed = band_gap_allowed;
                    s.controller_version = controller_version;
                    s.drive_config_ssb = drive_config_ssb;
                    s.drive_config_am = drive_config_am;
                    s.drive_config_cont = drive_config_cont;
                } // lock dropped here

                // Drive observer: feed latest polled operate-mode and Thetis
                // ZZPC (if shared). Logs transitions only; never sends anything.
                if let Some(ref drive_arc) = thetis_drive_level {
                    let operate_now = status.lock().unwrap().operate;
                    let zzpc_now = drive_arc.load(Ordering::Relaxed);
                    drive_observer.tick(operate_now, zzpc_now);
                }

                if consecutive_failures > 0 {
                    info!("RF2K-S reconnected after {} failures", consecutive_failures);
                }
                consecutive_failures = 0;
                poll_cycle += 1;

                // Secondary poll: debug info every ~5s (25 cycles × 200ms)
                // NOTE: lock must NOT be held here — fetch_debug does HTTP I/O
                if poll_cycle % 25 == 1 {
                    if let Ok(dbg) = fetch_debug(&client, base_url) {
                        let mut s = status.lock().unwrap();
                        apply_debug(&mut s, dbg);
                        s.debug_available = true;
                    }
                }

                // Drive config: fetch once at startup, then after SetDriveConfig
                if !drive_config_fetched {
                    if let Ok(dc) = fetch_drive_config(&client, base_url) {
                        apply_drive_config(&mut status.lock().unwrap(), &dc);
                        drive_config_fetched = true;
                        info!("RF2K-S drive config loaded");
                    }
                }
            }
            Err(e) => {
                consecutive_failures += 1;
                if consecutive_failures <= 5 || consecutive_failures % 20 == 0 {
                    warn!("RF2K-S poll failed ({}x): {} [url={}]", consecutive_failures, e, base_url);
                }
                if consecutive_failures >= 3 {
                    status.lock().unwrap().connected = false;
                }
                // Safety: if we lost connection during tune, turn off carrier
                if tune_carrier_active && consecutive_failures >= 3 {
                    tune_carrier_active = false;
                    if let Some(ref tx) = cat_tx {
                        let _ = tx.blocking_send("ZZTU0;".to_string());
                        warn!("RF2K-S tune: lost connection, carrier OFF (ZZTU0)");
                    }
                }
            }
        }
    }

    // Safety: turn off carrier if thread exits during tune
    if tune_carrier_active {
        if let Some(ref tx) = cat_tx {
            let _ = tx.blocking_send("ZZTU0;".to_string());
        }
    }

    status.lock().unwrap().connected = false;
    info!("RF2K-S HTTP thread stopped");
}

fn fetch_info(client: &reqwest::blocking::Client, base_url: &str) -> Result<String, String> {
    let resp: InfoResponse = get_json(client, &format!("{}/info", base_url))?;
    Ok(resp.device.unwrap_or_else(|| "RF2K-S".to_string()))
}

fn get_json<T: serde::de::DeserializeOwned>(client: &reqwest::blocking::Client, url: &str) -> Result<T, String> {
    let resp = client
        .get(url)
        .send()
        .map_err(|e| format!("request {}: {}", url, e))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().unwrap_or_default();
        return Err(format!("{} returned {} — {}", url, status, body));
    }
    let body = resp.text().map_err(|e| format!("{} read: {}", url, e))?;
    serde_json::from_str(&body).map_err(|e| format!("{} parse: {} — body: {}", url, e, &body[..body.len().min(200)]))
}

fn poll_status(client: &reqwest::blocking::Client, base_url: &str) -> Result<Rf2kStatus, String> {
    let mut s = Rf2kStatus::default();

    // GET /power
    let power: PowerResponse = get_json(client, &format!("{}/power", base_url))?;

    s.temperature_x10 = parse_f64_x10(&power.temperature.value);
    s.voltage_x10 = parse_f64_x10(&power.voltage.value);
    s.current_x10 = parse_f64_x10(&power.current.value);
    s.forward_w = parse_u16(&power.forward.value);
    s.reflected_w = parse_u16(&power.reflected.value);
    s.swr_x100 = parse_f64_x100(&power.swr.value);
    s.max_forward_w = power.forward.max_value.as_ref().map(parse_u16).unwrap_or(0);
    s.max_reflected_w = power.reflected.max_value.as_ref().map(parse_u16).unwrap_or(0);
    s.max_swr_x100 = power.swr.max_value.as_ref().map(parse_f64_x100).unwrap_or(100);

    // GET /data
    let data: DataResponse = get_json(client, &format!("{}/data", base_url))?;

    s.band = parse_band(&data.band.value);
    s.frequency_khz = parse_u16(&data.frequency.value);
    parse_error_state(&data.status, &mut s);

    // GET /tuner
    let tuner: TunerResponse = get_json(client, &format!("{}/tuner", base_url))?;

    s.tuner_mode = parse_tuner_mode(&tuner.mode);
    s.tuner_setup = tuner.setup.unwrap_or_default();
    s.tuner_l_nh = tuner.l.as_ref().map(|v| parse_u16(&v.value)).unwrap_or(0);
    s.tuner_c_pf = tuner.c.as_ref().map(|v| parse_u16(&v.value)).unwrap_or(0);
    s.tuner_freq_khz = tuner.tuned_frequency.as_ref().map(|v| parse_u16(&v.value)).unwrap_or(0);
    s.segment_size_khz = tuner.segment_size.as_ref().map(|v| parse_u16(&v.value)).unwrap_or(0);

    // GET /antennas/active
    let antenna: AntennaResponse = get_json(client, &format!("{}/antennas/active", base_url))?;

    s.antenna_type = if antenna.antenna_type == "EXTERNAL" { 1 } else { 0 };
    s.antenna_number = antenna.number;

    // GET /operate-mode
    let operate: OperateResponse = get_json(client, &format!("{}/operate-mode", base_url))?;

    s.operate = operate.operate_mode == "OPERATE";

    // GET /drive (optional — don't fail poll if drive endpoint unavailable)
    if let Ok(drive) = get_json::<DriveResponse>(client, &format!("{}/drive", base_url)) {
        if drive.has_drive {
            s.drive_w = drive.drive_power;
            s.modulation = drive.modulation.unwrap_or_default();
            s.max_power_w = drive.max_power.unwrap_or(100);
        }
    }

    debug!(
        "RF2K-S: op={} band={} fwd={}W ref={}W swr={:.2} drv={}W err={}",
        if s.operate { "OPR" } else { "STBY" },
        s.band, s.forward_w, s.reflected_w,
        s.swr_x100 as f32 / 100.0, s.drive_w, s.error_state,
    );

    Ok(s)
}

fn execute_command(client: &reqwest::blocking::Client, base_url: &str, cmd: &Rf2kCmd) {
    let result = match cmd {
        Rf2kCmd::SetOperate(operate) => {
            let mode = if *operate { "OPERATE" } else { "STANDBY" };
            client
                .put(format!("{}/operate-mode", base_url))
                .json(&serde_json::json!({"operate_mode": mode}))
                .send()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::Tune => {
            client
                .post(format!("{}/tune", base_url))
                .send()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::SetAntenna { antenna_type, number } => {
            let body = if *antenna_type == 1 {
                serde_json::json!({"type": "EXTERNAL"})
            } else {
                serde_json::json!({"type": "INTERNAL", "number": *number})
            };
            client
                .put(format!("{}/antennas/active", base_url))
                .json(&body)
                .send()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::ErrorReset => {
            client
                .post(format!("{}/error/reset", base_url))
                .send()
                .and_then(|r| {
                    let st = r.status();
                    if !st.is_success() {
                        warn!("RF2K-S /error/reset returned HTTP {}", st);
                    }
                    Ok(())
                })
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::Close => {
            client
                .post(format!("{}/system/close", base_url))
                .send()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::TunerMode(mode) => {
            let mode_str = if *mode == 1 { "AUTO" } else { "MANUAL" };
            client
                .put(format!("{}/tuner/mode", base_url))
                .json(&serde_json::json!({"mode": mode_str}))
                .send()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::TunerBypass(on) => {
            client
                .put(format!("{}/tuner/bypass", base_url))
                .json(&serde_json::json!({"bypass": *on}))
                .send()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::TunerReset => {
            client
                .post(format!("{}/tuner/reset", base_url))
                .send()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::TunerStore => {
            client
                .post(format!("{}/tuner/store", base_url))
                .send()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::TunerLUp => {
            client
                .put(format!("{}/tuner/l", base_url))
                .json(&serde_json::json!({"delta": 1}))
                .send()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::TunerLDown => {
            client
                .put(format!("{}/tuner/l", base_url))
                .json(&serde_json::json!({"delta": -1}))
                .send()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::TunerCUp => {
            client
                .put(format!("{}/tuner/c", base_url))
                .json(&serde_json::json!({"delta": 1}))
                .send()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::TunerCDown => {
            client
                .put(format!("{}/tuner/c", base_url))
                .json(&serde_json::json!({"delta": -1}))
                .send()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::TunerK => {
            client
                .post(format!("{}/tuner/k", base_url))
                .send()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::DriveUp => {
            client
                .put(format!("{}/drive", base_url))
                .json(&serde_json::json!({"delta": 1}))
                .send()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::DriveDown => {
            client
                .put(format!("{}/drive", base_url))
                .json(&serde_json::json!({"delta": -1}))
                .send()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::SetHighPower(high) => {
            client
                .put(format!("{}/debug/high-power", base_url))
                .json(&serde_json::json!({"high": *high}))
                .send()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::SetTuner6m(enabled) => {
            client
                .put(format!("{}/debug/tuner-6m", base_url))
                .json(&serde_json::json!({"enabled": *enabled}))
                .send()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::SetBandGap(allowed) => {
            client
                .put(format!("{}/debug/band-gap", base_url))
                .json(&serde_json::json!({"allowed": *allowed}))
                .send()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::FrqDelayUp => {
            client
                .put(format!("{}/debug/frq-delay", base_url))
                .json(&serde_json::json!({"delta": 1}))
                .send()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::FrqDelayDown => {
            client
                .put(format!("{}/debug/frq-delay", base_url))
                .json(&serde_json::json!({"delta": -1}))
                .send()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::AutotuneThresholdUp => {
            client
                .put(format!("{}/debug/autotune-threshold", base_url))
                .json(&serde_json::json!({"delta": 0.2}))
                .send()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::AutotuneThresholdDown => {
            client
                .put(format!("{}/debug/autotune-threshold", base_url))
                .json(&serde_json::json!({"delta": -0.2}))
                .send()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::DacAlcUp => {
            client
                .put(format!("{}/debug/dac-alc", base_url))
                .json(&serde_json::json!({"delta": 10}))
                .send()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::DacAlcDown => {
            client
                .put(format!("{}/debug/dac-alc", base_url))
                .json(&serde_json::json!({"delta": -10}))
                .send()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::ZeroFRAM => {
            client
                .post(format!("{}/debug/zero-fram", base_url))
                .send()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Rf2kCmd::SetDriveConfig { category, band, value } => {
            let cfg_result = fetch_drive_config(client, base_url);
            if let Ok(mut dc) = cfg_result {
                let arr = match category {
                    0 => &mut dc.ssb,
                    1 => &mut dc.am,
                    _ => &mut dc.cont,
                };
                if (*band as usize) < arr.len() {
                    arr[*band as usize] = *value;
                }
                client
                    .put(format!("{}/drive/config", base_url))
                    .json(&serde_json::json!({"ssb": dc.ssb, "am": dc.am, "cont": dc.cont}))
                    .send()
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            } else {
                Err("Failed to fetch current drive config".to_string())
            }
        }
    };

    match result {
        Ok(()) => info!("RF2K-S command {:?} sent", cmd_name(cmd)),
        Err(e) => warn!("RF2K-S command {:?} failed: {}", cmd_name(cmd), e),
    }
}

fn cmd_name(cmd: &Rf2kCmd) -> &'static str {
    match cmd {
        Rf2kCmd::SetOperate(true) => "Operate",
        Rf2kCmd::SetOperate(false) => "Standby",
        Rf2kCmd::Tune => "Tune",
        Rf2kCmd::SetAntenna { .. } => "SetAntenna",
        Rf2kCmd::ErrorReset => "ErrorReset",
        Rf2kCmd::Close => "Close",
        Rf2kCmd::TunerMode(_) => "TunerMode",
        Rf2kCmd::TunerBypass(_) => "TunerBypass",
        Rf2kCmd::TunerReset => "TunerReset",
        Rf2kCmd::TunerStore => "TunerStore",
        Rf2kCmd::TunerLUp => "TunerLUp",
        Rf2kCmd::TunerLDown => "TunerLDown",
        Rf2kCmd::TunerCUp => "TunerCUp",
        Rf2kCmd::TunerCDown => "TunerCDown",
        Rf2kCmd::TunerK => "TunerK",
        Rf2kCmd::DriveUp => "DriveUp",
        Rf2kCmd::DriveDown => "DriveDown",
        Rf2kCmd::SetHighPower(_) => "SetHighPower",
        Rf2kCmd::SetTuner6m(_) => "SetTuner6m",
        Rf2kCmd::SetBandGap(_) => "SetBandGap",
        Rf2kCmd::FrqDelayUp => "FrqDelayUp",
        Rf2kCmd::FrqDelayDown => "FrqDelayDown",
        Rf2kCmd::AutotuneThresholdUp => "AutotuneThresholdUp",
        Rf2kCmd::AutotuneThresholdDown => "AutotuneThresholdDown",
        Rf2kCmd::DacAlcUp => "DacAlcUp",
        Rf2kCmd::DacAlcDown => "DacAlcDown",
        Rf2kCmd::ZeroFRAM => "ZeroFRAM",
        Rf2kCmd::SetDriveConfig { .. } => "SetDriveConfig",
    }
}

// --- Parsing helpers ---

fn parse_u16(v: &serde_json::Value) -> u16 {
    match v {
        serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0) as u16,
        _ => 0,
    }
}

fn parse_f64_x10(v: &serde_json::Value) -> u16 {
    match v {
        serde_json::Value::Number(n) => (n.as_f64().unwrap_or(0.0) * 10.0) as u16,
        _ => 0,
    }
}

fn parse_f64_x100(v: &serde_json::Value) -> u16 {
    match v {
        serde_json::Value::Number(n) => (n.as_f64().unwrap_or(1.0) * 100.0) as u16,
        _ => 100,
    }
}

/// Map band meter values to index: [160,80,60,40,30,20,17,15,12,10,6]
fn parse_band(v: &serde_json::Value) -> u8 {
    let meters = match v {
        serde_json::Value::Number(n) => n.as_u64().unwrap_or(0) as u16,
        _ => 0,
    };
    match meters {
        160 => 0,
        80 => 1,
        60 => 2,
        40 => 3,
        30 => 4,
        20 => 5,
        17 => 6,
        15 => 7,
        12 => 8,
        10 => 9,
        6 => 10,
        _ => 0,
    }
}

fn parse_tuner_mode(mode: &str) -> u8 {
    match mode {
        "OFF" => 0,
        "BYPASS" => 1,
        "MANUAL" => 2,
        "AUTO_TUNING" => 3,
        "AUTO" => 4,
        "AUTO_TUNING_FROM_AUTO" => 5,
        _ => 0,
    }
}

// RF2K-S error states from data.py ErrorState enum
// The REST API returns str(ErrorState.XXX) which uses the custom __str__ method,
// producing human-readable strings like "High SWR", "Overheating", etc.
fn parse_error_state(status_str: &str, s: &mut Rf2kStatus) {
    let (code, text) = match status_str {
        "" => (0, ""),
        "High Antenna Reflection" => (1, "HIGH ANTENNA REFLECTION"),
        "High Current" => (2, "HIGH CURRENT"),
        "High Input Power" => (3, "HIGH INPUT POWER"),
        "Severe Error LPF" => (4, "SEVERE ERROR LPF"),
        "Wrong Frequency" => (5, "WRONG FREQUENCY"),
        "No internal high voltage" => (6, "NO INTERNAL HV"),
        "Overheating" => (7, "OVERHEATING"),
        // NOT_TUNED (8) has no __str__ override, returns "" — indistinguishable from NONE
        "High Output Power" => (9, "HIGH OUTPUT POWER"),
        "High SWR" => (10, "HIGH SWR"),
        other => {
            // Unknown error string — treat as error so Reset button enables
            if !other.is_empty() {
                warn!("RF2K-S unknown error state string: {:?}", other);
                (99, other)
            } else {
                (0, "")
            }
        }
    };
    s.error_state = code;
    s.error_text = text.to_string();
}

fn fetch_debug(client: &reqwest::blocking::Client, base_url: &str) -> Result<DebugResponse, String> {
    get_json(client, &format!("{}/debug", base_url))
}

fn fetch_drive_config(client: &reqwest::blocking::Client, base_url: &str) -> Result<DriveConfigResponse, String> {
    get_json(client, &format!("{}/drive/config", base_url))
}

fn apply_debug(s: &mut Rf2kStatus, dbg: DebugResponse) {
    s.bias_pct_x10 = dbg.bias_pct.map(|v| (v * 10.0) as u16).unwrap_or(0);
    s.psu_source = dbg.psu_source.and_then(|v| v.as_u64()).unwrap_or(0) as u8;
    s.uptime_s = dbg.uptime_s.unwrap_or(0);
    s.tx_time_s = dbg.tx_time_s.unwrap_or(0);
    s.error_count = dbg.error_count.unwrap_or(0);
    s.error_history = dbg.error_history.map(|v| {
        v.into_iter().map(|e| (e.time, e.error)).collect()
    }).unwrap_or_default();
    s.storage_bank = dbg.storage_bank.and_then(|v| v.as_u64()).unwrap_or(0) as u16;
    s.hw_revision = dbg.hw_revision.unwrap_or_default();
    s.frq_delay = dbg.frq_delay.and_then(|v| v.as_u64()).unwrap_or(0) as u16;
    s.autotune_threshold_x10 = dbg.autotune_threshold.map(|v| (v * 10.0) as u16).unwrap_or(0);
    s.dac_alc = dbg.dac_alc.and_then(|v| v.as_u64()).unwrap_or(0) as u16;
    s.high_power = dbg.high_power.unwrap_or(false);
    s.tuner_6m = dbg.tuner_6m.unwrap_or(false);
    s.band_gap_allowed = dbg.band_gap_allowed.unwrap_or(false);
    s.controller_version = dbg.controller_version.unwrap_or(0);
}

fn apply_drive_config(s: &mut Rf2kStatus, dc: &DriveConfigResponse) {
    for (i, &v) in dc.ssb.iter().enumerate().take(11) {
        s.drive_config_ssb[i] = v;
    }
    for (i, &v) in dc.am.iter().enumerate().take(11) {
        s.drive_config_am[i] = v;
    }
    for (i, &v) in dc.cont.iter().enumerate().take(11) {
        s.drive_config_cont[i] = v;
    }
}

/// Format RF2K status as CSV labels for network transmission.
/// Format: operate,ptt,band,freq_khz,temp_x10,volt_x10,curr_x10,fwd_w,ref_w,swr_x100,
///         max_fwd,max_ref,max_swr,error_state,ant_type,ant_nr,
///         tuner_mode,tuner_setup,l_nh,c_pf,tuner_freq_khz,seg_khz,
///         drive_w,modulation,max_power_w,error_text,device_name
/// Format: operate,ptt,band,freq_khz,temp_x10,volt_x10,curr_x10,fwd_w,ref_w,swr_x100,
///         max_fwd,max_ref,max_swr,error_state,ant_type,ant_nr,
///         tuner_mode,tuner_setup,l_nh,c_pf,tuner_freq_khz,seg_khz,
///         drive_w,modulation,max_power_w,error_text,device_name
/// NOTE: active flag [27] is appended by network.rs, debug fields [28+] by debug_labels_string()
pub fn status_labels_string(status: &Rf2kStatus) -> String {
    format!(
        "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
        if status.operate { 1 } else { 0 },
        if status.ptt { 1 } else { 0 },
        status.band,
        status.frequency_khz,
        status.temperature_x10,
        status.voltage_x10,
        status.current_x10,
        status.forward_w,
        status.reflected_w,
        status.swr_x100,
        status.max_forward_w,
        status.max_reflected_w,
        status.max_swr_x100,
        status.error_state,
        status.antenna_type,
        status.antenna_number,
        status.tuner_mode,
        status.tuner_setup,
        status.tuner_l_nh,
        status.tuner_c_pf,
        status.tuner_freq_khz,
        status.segment_size_khz,
        status.drive_w,
        status.modulation,
        status.max_power_w,
        status.error_text,
        status.device_name,
    )
}

/// Debug + drive config fields, appended after the active flag [27].
/// Returns ",field28,field29,...,field46" (leading comma included).
pub fn debug_labels_string(status: &Rf2kStatus) -> String {
    if !status.debug_available {
        return String::new();
    }
    let error_hist: String = status.error_history.iter()
        .map(|(t, e)| format!("{}={}", t, e))
        .collect::<Vec<_>>()
        .join(";");
    let drive_ssb: String = status.drive_config_ssb.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(";");
    let drive_am: String = status.drive_config_am.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(";");
    let drive_cont: String = status.drive_config_cont.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(";");
    format!(
        ",{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
        if status.debug_available { 1 } else { 0 },    // 28
        status.bias_pct_x10,                            // 29
        status.psu_source,                              // 30
        status.uptime_s,                                // 31
        status.tx_time_s,                               // 32
        status.error_count,                             // 33
        error_hist,                                     // 34
        status.storage_bank,                            // 35
        status.hw_revision,                             // 36
        status.frq_delay,                               // 37
        status.autotune_threshold_x10,                  // 38
        status.dac_alc,                                 // 39
        if status.high_power { 1 } else { 0 },         // 40
        if status.tuner_6m { 1 } else { 0 },           // 41
        if status.band_gap_allowed { 1 } else { 0 },   // 42
        status.controller_version,                      // 43
        drive_ssb,                                      // 44
        drive_am,                                       // 45
        drive_cont,                                     // 46
    )
}

/// Band index to name
pub fn band_name(band: u8) -> &'static str {
    match band {
        0 => "160m",
        1 => "80m",
        2 => "60m",
        3 => "40m",
        4 => "30m",
        5 => "20m",
        6 => "17m",
        7 => "15m",
        8 => "12m",
        9 => "10m",
        10 => "6m",
        _ => "?",
    }
}

/// Tuner mode index to name
pub fn tuner_mode_name(mode: u8) -> &'static str {
    match mode {
        0 => "OFF",
        1 => "BYP",
        2 => "MAN",
        3 => "TUNING",
        4 => "AUTO",
        5 => "AUTO",
        _ => "?",
    }
}

// ============================================================================
// Drive-restore observability (Phase B — passive observer)
// ----------------------------------------------------------------------------
// Watches RF2K-S operate-mode transitions against the Thetis-side ZZPC drive
// percentage (shared via `Arc<AtomicU8>`). Purely observational — emits
// transition-only log lines. A `RESTORE FAILED` WARN line is the key signal:
// it means the external restore flow (RF2K-S firmware ↔ Thetis CAT) did NOT
// bring the drive back to its pre-operate value within the timeout window.
// See `docs/patch-briefs/PATCH-rf2k-drive-restore-reliability.md` §1.3.
// ============================================================================

/// Observer timeout: how long after an operate-mode transition we wait for
/// the expected ZZPC movement before declaring a timeout.
const DRIVE_OBSERVER_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rf2kDriveState {
    Standby,
    StandbyToOperate,
    Operate,
    OperateToStandby,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Rf2kDriveEvent {
    Rf2kOperateModeChanged { new_mode: Rf2kMode, observed_zzpc: u8 },
    ZzpcObserved { value: u8 },
    Tick { now: Instant },
}

/// Observer context. Callers feed events through `next_drive_state`; the
/// caller is responsible for updating `transition_start`, `saved_drive` and
/// `last_known_zzpc` after each call (the pure function cannot mutate its
/// own context).
#[derive(Debug, Clone)]
pub(crate) struct DriveObserverCtx {
    pub transition_start: Option<Instant>,
    pub saved_drive: Option<u8>,
    pub last_known_zzpc: u8,
    pub timeout: Duration,
}

impl DriveObserverCtx {
    pub fn new(timeout: Duration) -> Self {
        Self {
            transition_start: None,
            saved_drive: None,
            last_known_zzpc: 0,
            timeout,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum DriveObserverLog {
    Info(String),
    Warn(String),
}

/// Pure state-transition function for the drive-restore observer.
///
/// Returns the new state plus an optional log directive to emit when an
/// actual transition (or a relevant timeout) occurs. Callers must update
/// `ctx` based on the return value — specifically:
///
/// - On `Standby → StandbyToOperate`: set `transition_start=Some(now)` and
///   `saved_drive=Some(observed_zzpc)` from the triggering event.
/// - On `Operate → OperateToStandby`: set `transition_start=Some(now)`.
///   `saved_drive` is preserved from the earlier Standby→StandbyToOperate.
/// - On any transition back to `Standby` or `Operate`: clear
///   `transition_start=None`.
/// - Every `ZzpcObserved { value }` event: update `last_known_zzpc=value`.
pub(crate) fn next_drive_state(
    current: Rf2kDriveState,
    event: Rf2kDriveEvent,
    ctx: &DriveObserverCtx,
) -> (Rf2kDriveState, Option<DriveObserverLog>) {
    use Rf2kDriveEvent::*;
    use Rf2kDriveState::*;

    match (current, event) {
        // ── Standby: watch for RF2K-S entering OPERATE ──────────────────
        (
            Standby,
            Rf2kOperateModeChanged { new_mode: Rf2kMode::Operate, observed_zzpc },
        ) => (
            StandbyToOperate,
            Some(DriveObserverLog::Info(format!(
                "RF2K drive: transition STANDBY\u{2192}OPERATE, last ZZPC={}%",
                observed_zzpc
            ))),
        ),
        (Standby, _) => (Standby, None),

        // ── StandbyToOperate: wait for ZZPC drop or timeout ────────────
        (StandbyToOperate, ZzpcObserved { value })
            if Some(value) != ctx.saved_drive =>
        {
            let saved = ctx.saved_drive.unwrap_or(0);
            let delta_ms = ctx
                .transition_start
                .map(|t| t.elapsed().as_millis() as u64)
                .unwrap_or(0);
            (
                Operate,
                Some(DriveObserverLog::Info(format!(
                    "RF2K drive: observed Thetis ZZPC drop {}%\u{2192}{}% after operate (\u{0394}={}ms)",
                    saved, value, delta_ms
                ))),
            )
        }
        (StandbyToOperate, ZzpcObserved { .. }) => (StandbyToOperate, None),
        (StandbyToOperate, Tick { now }) => {
            if let Some(start) = ctx.transition_start {
                if now.duration_since(start) >= ctx.timeout {
                    return (
                        Operate,
                        Some(DriveObserverLog::Warn(format!(
                            "RF2K drive: no ZZPC change seen after STANDBY\u{2192}OPERATE (kept at {}%)",
                            ctx.saved_drive.unwrap_or(0)
                        ))),
                    );
                }
            }
            (StandbyToOperate, None)
        }
        // Rare: operate-mode flaps back to Standby before first drive change
        // was observed. Treat as start of a restore-watch window so we still
        // capture the edge case.
        (
            StandbyToOperate,
            Rf2kOperateModeChanged { new_mode: Rf2kMode::Standby, observed_zzpc },
        ) => (
            OperateToStandby,
            Some(DriveObserverLog::Info(format!(
                "RF2K drive: rapid cycle STANDBY\u{2192}OPERATE\u{2192}STANDBY, ZZPC={}%, expected restore to {}%",
                observed_zzpc,
                ctx.saved_drive.unwrap_or(0)
            ))),
        ),
        (StandbyToOperate, Rf2kOperateModeChanged { new_mode: Rf2kMode::Operate, .. }) => {
            (StandbyToOperate, None)
        }

        // ── Operate: stable state, watch for STANDBY transition ─────────
        (
            Operate,
            Rf2kOperateModeChanged { new_mode: Rf2kMode::Standby, observed_zzpc },
        ) => {
            let saved = ctx.saved_drive.unwrap_or(0);
            (
                OperateToStandby,
                Some(DriveObserverLog::Info(format!(
                    "RF2K drive: transition OPERATE\u{2192}STANDBY, current ZZPC={}%, expected restore to {}%",
                    observed_zzpc, saved
                ))),
            )
        }
        (Operate, _) => (Operate, None),

        // ── OperateToStandby: watch for restore match or timeout ────────
        (OperateToStandby, ZzpcObserved { value }) => {
            if Some(value) == ctx.saved_drive {
                let saved = ctx.saved_drive.unwrap_or(0);
                let delta_ms = ctx
                    .transition_start
                    .map(|t| t.elapsed().as_millis() as u64)
                    .unwrap_or(0);
                (
                    Standby,
                    Some(DriveObserverLog::Info(format!(
                        "RF2K drive: restore OK \u{2014} Thetis ZZPC={}% matches saved {}% (\u{0394}={}ms)",
                        value, saved, delta_ms
                    ))),
                )
            } else {
                (OperateToStandby, None)
            }
        }
        (OperateToStandby, Tick { now }) => {
            if let Some(start) = ctx.transition_start {
                if now.duration_since(start) >= ctx.timeout {
                    let saved = ctx.saved_drive.unwrap_or(0);
                    return (
                        Standby,
                        Some(DriveObserverLog::Warn(format!(
                            "RF2K drive: RESTORE FAILED \u{2014} saved={}% but Thetis ZZPC={}% (edge case captured)",
                            saved, ctx.last_known_zzpc
                        ))),
                    );
                }
            }
            (OperateToStandby, None)
        }
        (OperateToStandby, Rf2kOperateModeChanged { .. }) => (OperateToStandby, None),
    }
}

/// Mutable observer state tracked by the caller (rf2k poll loop).
struct DriveObserverState {
    state: Rf2kDriveState,
    ctx: DriveObserverCtx,
    prev_operate: bool,
    prev_zzpc: u8,
    /// `true` once we have seen the first poll so we do not emit spurious
    /// operate-transition events before the initial state is known.
    initialized: bool,
}

impl DriveObserverState {
    fn new() -> Self {
        Self {
            state: Rf2kDriveState::Standby,
            ctx: DriveObserverCtx::new(DRIVE_OBSERVER_TIMEOUT),
            prev_operate: false,
            prev_zzpc: 0,
            initialized: false,
        }
    }

    /// Feed a single event through the state machine, update `ctx`, and emit
    /// the transition log if any.
    fn process(&mut self, event: Rf2kDriveEvent) {
        let old_state = self.state;
        let (new_state, log) = next_drive_state(old_state, event, &self.ctx);

        // Update ctx based on event kind (always)
        if let Rf2kDriveEvent::ZzpcObserved { value } = event {
            self.ctx.last_known_zzpc = value;
        }

        // Update ctx based on transition (only when state changed)
        if new_state != old_state {
            match (old_state, new_state) {
                (Rf2kDriveState::Standby, Rf2kDriveState::StandbyToOperate) => {
                    self.ctx.transition_start = Some(Instant::now());
                    if let Rf2kDriveEvent::Rf2kOperateModeChanged { observed_zzpc, .. } = event {
                        self.ctx.saved_drive = Some(observed_zzpc);
                    }
                }
                (Rf2kDriveState::Operate, Rf2kDriveState::OperateToStandby) => {
                    self.ctx.transition_start = Some(Instant::now());
                    // saved_drive preserved from earlier Standby→StandbyToOperate
                }
                (_, Rf2kDriveState::Standby) | (_, Rf2kDriveState::Operate) => {
                    self.ctx.transition_start = None;
                }
                _ => {}
            }
        }

        // Emit log if any
        if let Some(log) = log {
            match log {
                DriveObserverLog::Info(s) => info!("{}", s),
                DriveObserverLog::Warn(s) => warn!("{}", s),
            }
        }
        self.state = new_state;
    }

    /// Called once per poll tick. Compares latest polled values against the
    /// previous snapshot and generates the appropriate events.
    fn tick(&mut self, current_operate: bool, current_zzpc: u8) {
        if !self.initialized {
            self.prev_operate = current_operate;
            self.prev_zzpc = current_zzpc;
            self.ctx.last_known_zzpc = current_zzpc;
            self.state = if current_operate { Rf2kDriveState::Operate } else { Rf2kDriveState::Standby };
            self.initialized = true;
            return;
        }

        // Emit operate-mode-change event first so saved_drive gets the
        // pre-transition ZZPC, before any ZzpcObserved updates.
        if current_operate != self.prev_operate {
            self.process(Rf2kDriveEvent::Rf2kOperateModeChanged {
                new_mode: if current_operate { Rf2kMode::Operate } else { Rf2kMode::Standby },
                observed_zzpc: self.prev_zzpc,
            });
        }
        if current_zzpc != self.prev_zzpc {
            self.process(Rf2kDriveEvent::ZzpcObserved { value: current_zzpc });
        }
        // Always tick for timeout detection
        self.process(Rf2kDriveEvent::Tick { now: Instant::now() });

        self.prev_operate = current_operate;
        self.prev_zzpc = current_zzpc;
    }
}

// ============================================================================
// Tests (Phase B acceptance §1.4 — pure state machine only)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with(saved: Option<u8>, last_zzpc: u8, tr_start: Option<Instant>) -> DriveObserverCtx {
        DriveObserverCtx {
            transition_start: tr_start,
            saved_drive: saved,
            last_known_zzpc: last_zzpc,
            timeout: Duration::from_secs(2),
        }
    }

    fn ctx_default() -> DriveObserverCtx {
        DriveObserverCtx::new(Duration::from_secs(2))
    }

    // §1.4 test #1
    #[test]
    fn standby_to_standby_no_event() {
        let (s, log) = next_drive_state(
            Rf2kDriveState::Standby,
            Rf2kDriveEvent::ZzpcObserved { value: 80 },
            &ctx_default(),
        );
        assert_eq!(s, Rf2kDriveState::Standby);
        assert!(log.is_none());
    }

    // §1.4 test #2
    #[test]
    fn standby_enters_pre_operate_on_rf2k_operate() {
        let (s, log) = next_drive_state(
            Rf2kDriveState::Standby,
            Rf2kDriveEvent::Rf2kOperateModeChanged {
                new_mode: Rf2kMode::Operate,
                observed_zzpc: 100,
            },
            &ctx_default(),
        );
        assert_eq!(s, Rf2kDriveState::StandbyToOperate);
        match log {
            Some(DriveObserverLog::Info(msg)) => {
                assert!(msg.contains("STANDBY"));
                assert!(msg.contains("OPERATE"));
                assert!(msg.contains("100%"));
            }
            _ => panic!("expected Info log"),
        }
    }

    // §1.4 test #3
    #[test]
    fn operate_transition_observed_zzpc_drop() {
        let ctx = ctx_with(Some(100), 100, Some(Instant::now()));
        let (s, log) = next_drive_state(
            Rf2kDriveState::StandbyToOperate,
            Rf2kDriveEvent::ZzpcObserved { value: 10 },
            &ctx,
        );
        assert_eq!(s, Rf2kDriveState::Operate);
        match log {
            Some(DriveObserverLog::Info(msg)) => {
                assert!(msg.contains("100%"));
                assert!(msg.contains("10%"));
                assert!(msg.contains("drop"));
            }
            _ => panic!("expected Info log"),
        }
    }

    // §1.4 test #4
    #[test]
    fn operate_transition_no_zzpc_change_times_out() {
        let start = Instant::now() - Duration::from_millis(2_500);
        let ctx = ctx_with(Some(100), 100, Some(start));
        let (s, log) = next_drive_state(
            Rf2kDriveState::StandbyToOperate,
            Rf2kDriveEvent::Tick { now: Instant::now() },
            &ctx,
        );
        assert_eq!(s, Rf2kDriveState::Operate);
        match log {
            Some(DriveObserverLog::Warn(msg)) => assert!(msg.contains("no ZZPC change")),
            _ => panic!("expected Warn log"),
        }
    }

    // §1.4 test #5
    #[test]
    fn operate_to_standby_triggers_restore_watch() {
        let ctx = ctx_with(Some(100), 10, None);
        let (s, log) = next_drive_state(
            Rf2kDriveState::Operate,
            Rf2kDriveEvent::Rf2kOperateModeChanged {
                new_mode: Rf2kMode::Standby,
                observed_zzpc: 10,
            },
            &ctx,
        );
        assert_eq!(s, Rf2kDriveState::OperateToStandby);
        match log {
            Some(DriveObserverLog::Info(msg)) => {
                assert!(msg.contains("OPERATE"));
                assert!(msg.contains("STANDBY"));
                assert!(msg.contains("expected restore to 100%"));
            }
            _ => panic!("expected Info log"),
        }
    }

    // §1.4 test #6
    #[test]
    fn restore_success_on_zzpc_match() {
        let ctx = ctx_with(Some(100), 10, Some(Instant::now()));
        let (s, log) = next_drive_state(
            Rf2kDriveState::OperateToStandby,
            Rf2kDriveEvent::ZzpcObserved { value: 100 },
            &ctx,
        );
        assert_eq!(s, Rf2kDriveState::Standby);
        match log {
            Some(DriveObserverLog::Info(msg)) => {
                assert!(msg.contains("restore OK"));
                assert!(msg.contains("100%"));
            }
            _ => panic!("expected Info log"),
        }
    }

    // §1.4 test #7 — the critical edge-case capture
    #[test]
    fn restore_failure_on_timeout() {
        let start = Instant::now() - Duration::from_millis(2_500);
        let ctx = ctx_with(Some(100), 10, Some(start));
        let (s, log) = next_drive_state(
            Rf2kDriveState::OperateToStandby,
            Rf2kDriveEvent::Tick { now: Instant::now() },
            &ctx,
        );
        assert_eq!(s, Rf2kDriveState::Standby);
        match log {
            Some(DriveObserverLog::Warn(msg)) => {
                assert!(msg.contains("RESTORE FAILED"));
                assert!(msg.contains("saved=100%"));
                assert!(msg.contains("ZZPC=10%"));
            }
            _ => panic!("expected Warn RESTORE FAILED log"),
        }
    }

    // §1.4 test #8
    #[test]
    fn restore_partial_wrong_value_stays_in_watch() {
        let ctx = ctx_with(Some(100), 50, Some(Instant::now()));
        let (s, log) = next_drive_state(
            Rf2kDriveState::OperateToStandby,
            Rf2kDriveEvent::ZzpcObserved { value: 50 },
            &ctx,
        );
        assert_eq!(s, Rf2kDriveState::OperateToStandby);
        assert!(log.is_none());
    }

    // §1.4 test #9 — end-to-end rapid cycle via DriveObserverState
    #[test]
    fn rapid_operate_standby_cycle_preserves_saved_drive() {
        let mut obs = DriveObserverState::new();
        obs.tick(false, 100); // init: Standby, ZZPC 100
        assert_eq!(obs.state, Rf2kDriveState::Standby);

        obs.tick(true, 100);  // operate on, ZZPC still 100
        assert_eq!(obs.state, Rf2kDriveState::StandbyToOperate);
        assert_eq!(obs.ctx.saved_drive, Some(100));

        obs.tick(true, 10);   // ZZPC drop to 10 — confirms operate
        assert_eq!(obs.state, Rf2kDriveState::Operate);

        obs.tick(false, 10);  // operate off, ZZPC still 10
        assert_eq!(obs.state, Rf2kDriveState::OperateToStandby);
        // saved_drive must still be 100 (from earlier transition)
        assert_eq!(obs.ctx.saved_drive, Some(100));

        obs.tick(false, 100); // ZZPC restored to 100 — restore OK
        assert_eq!(obs.state, Rf2kDriveState::Standby);
    }

    // §1.4 test #10
    #[test]
    fn multiple_zzpc_observations_during_operate_are_silent() {
        let ctx = ctx_with(Some(100), 12, None);
        for v in [12, 11, 10] {
            let (s, log) = next_drive_state(
                Rf2kDriveState::Operate,
                Rf2kDriveEvent::ZzpcObserved { value: v },
                &ctx,
            );
            assert_eq!(s, Rf2kDriveState::Operate);
            assert!(log.is_none(), "ZZPC drift during Operate must be silent");
        }
    }

    // §1.4 test #11 — caller-side ctx update for last_known_zzpc in Standby
    #[test]
    fn saved_drive_updates_on_standby_zzpc_via_caller() {
        let mut obs = DriveObserverState::new();
        obs.tick(false, 80); // init
        assert_eq!(obs.ctx.last_known_zzpc, 80);
        obs.tick(false, 60); // drive changed while in Standby
        assert_eq!(obs.state, Rf2kDriveState::Standby);
        assert_eq!(obs.ctx.last_known_zzpc, 60);
    }

    // Extra: rapid-cycle edge case (StandbyToOperate directly back to Standby)
    #[test]
    fn rapid_cycle_operate_and_standby_before_drive_drop() {
        let mut obs = DriveObserverState::new();
        obs.tick(false, 100);
        obs.tick(true, 100);
        assert_eq!(obs.state, Rf2kDriveState::StandbyToOperate);
        // No drive-drop seen, operate flaps back to standby
        obs.tick(false, 100);
        assert_eq!(obs.state, Rf2kDriveState::OperateToStandby);
        // Now ZZPC stays at 100 (saved was 100) — restore "succeeds" immediately
        // when caller sees ZZPC observation equal to saved.
        obs.tick(false, 100); // no ZZPC change, but saved == 100 so ZzpcObserved not emitted
        // State remains OperateToStandby until either match or timeout. No
        // ZZPC event was emitted because current==prev (both 100). That is the
        // desired passive behavior — timeout will eventually fire.
        assert_eq!(obs.state, Rf2kDriveState::OperateToStandby);
    }

    #[test]
    fn observer_uninitialized_first_tick_sets_baseline_silently() {
        let mut obs = DriveObserverState::new();
        assert!(!obs.initialized);
        obs.tick(true, 50); // first tick: operate=true, zzpc=50
        assert!(obs.initialized);
        assert_eq!(obs.state, Rf2kDriveState::Operate);
        assert_eq!(obs.ctx.last_known_zzpc, 50);
    }
}
