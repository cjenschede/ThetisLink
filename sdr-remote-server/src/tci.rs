// SPDX-License-Identifier: GPL-2.0-or-later

#![allow(dead_code)]
use std::collections::{HashSet, VecDeque};
use std::time::Instant;

use futures_util::{SinkExt, StreamExt};
use log::{info, warn};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::tci_parser::*;

/// Size of the TX audio ring buffer (samples at 48kHz, ~0.5 sec)
const TX_RING_CAPACITY: usize = 48000;

/// Default TCI address
const DEFAULT_TCI_ADDR: &str = "127.0.0.1:40001";

/// TCI connection that replaces CatConnection.
/// Connects to Thetis TCI WebSocket, receives push-based state updates,
/// audio streams, and IQ data. Sends commands and TX audio.
pub struct TciConnection {
    addr: String,
    /// Channel to send text commands to the WebSocket writer task
    cmd_tx: Option<mpsc::Sender<String>>,
    /// Channel to send binary frames (TX audio) to the WebSocket writer task
    bin_tx: Option<mpsc::Sender<Vec<u8>>>,
    /// Channel to receive parsed text notifications from the reader task
    notify_rx: Option<mpsc::Receiver<TciNotification>>,
    /// Whether WebSocket is connected
    connected: bool,
    /// Connection attempt tracking
    last_connect_attempt: Instant,

    // --- Radio state (same fields as CatConnection) ---
    pub vfo_a_freq: u64,
    pub vfo_a_mode: u8,
    pub smeter_window: VecDeque<f32>,
    /// Direct dBm value from TCI _ex format (bypasses window averaging)
    pub smeter_direct_dbm: Option<f32>,
    pub power_on: bool,
    pub tx_profile: u8,
    pub nr_level: u8,
    pub anf_on: bool,
    pub drive_level: u8,
    pub rx_af_gain: u8,
    pub tx_active: bool,
    pub fwd_power_watts: f32,
    pub swr: f32,
    pub filter_low_hz: i32,
    pub filter_high_hz: i32,
    pub ctun: bool,
    /// DDS center frequency per receiver (from TCI DDS notification)
    pub dds_freq: [u64; 2],
    // RX2 / VFO-B state
    pub vfo_b_freq: u64,
    pub vfo_b_mode: u8,
    pub rx2_af_gain: u8,
    pub smeter_rx2_window: VecDeque<f32>,
    pub smeter_rx2_direct_dbm: Option<f32>,
    pub rx2_nr_level: u8,
    pub rx2_anf_on: bool,
    pub rx2_agc_mode: u8,
    pub rx2_agc_gain: u8,
    pub rx2_sql_enable: bool,
    pub rx2_sql_level: u8,
    pub rx2_nb_enable: bool,
    pub rx2_nb_level: u8,
    pub rx2_binaural: bool,
    pub rx2_apf_enable: bool,
    pub rx2_vfo_lock: bool,
    pub filter_rx2_low_hz: i32,
    pub filter_rx2_high_hz: i32,
    // Compat fields (kept for ptt.rs interface)
    pub filter_index: u8,
    pub filter_rx2_index: u8,
    pub fm_deviation: u8,
    // TX Monitor (MON_ENABLE)
    pub mon_on: bool,
    // VFO Sync readback
    pub vfo_sync_on: bool,
    // New TCI controls (v2.10.3.13)
    pub agc_mode: u8,
    pub agc_gain: u8,
    pub rit_enable: bool,
    pub rit_offset: i32,
    pub xit_enable: bool,
    pub xit_offset: i32,
    pub sql_enable: bool,
    pub sql_level: u8,
    pub nb_enable: bool,
    pub nb_level: u8,  // 0=off, 1=NB1, 2=NB2
    pub agc_auto_rx1: bool,
    pub agc_auto_rx2: bool,
    /// Last diversity sweep result (forwarded to clients)
    pub diversity_sweep_result: Option<(String, Vec<(f32, f32)>)>,
    /// Auto-null progress: (round, total, phase, gain_db, smeter)
    pub diversity_auto_progress: Option<(u32, u32, f32, f32, f32)>,
    /// Auto-null done: (phase, gain_db, improvement_db)
    pub diversity_auto_done: Option<(f32, f32, f32)>,
    pub cw_keyer_speed: u8,
    pub vfo_lock: bool,
    pub binaural: bool,
    pub apf_enable: bool,
    // Mute, RX mute, notch filter, balance
    pub mute: bool,
    pub rx_mute: bool,
    pub nf_enable: bool,
    pub rx2_nf_enable: bool,
    pub rx_balance: i8,
    // Tune state (TCI: tune)
    pub tune_active: bool,
    // Tune drive level (TCI: tune_drive, 0-100)
    pub tune_drive: u8,
    // Monitor volume in dB (TCI: mon_volume, typically -40..0)
    pub mon_volume: i8,
    /// Meter calibration offset per receiver (dB, from calibration_ex field 1)
    /// For ANAN-7000DLE default is ~4.84 dB
    pub meter_cal_offset: [f32; 2],
    /// Transverter gain offset per receiver (dB, 0 when no xvtr)
    pub xvtr_gain_offset: [f32; 2],
    /// 6m LNA gain offset per receiver (dB, 0 unless on 6m)
    pub six_m_gain_offset: [f32; 2],
    /// Raw TCI peakBinDbm for auto-calibration of spectrum offset
    pub smeter_raw_dbm: [Option<f32>; 2],
    // TX Profile names from tx_profiles_ex (ordered list)
    pub tx_profile_names: Vec<String>,
    // Current TX profile name from tx_profile_ex
    pub tx_profile_name: String,

    // --- Audio channels (phase 2) ---
    /// RX1 audio receiver: Vec<f32> chunks at 48kHz mono
    pub rx1_audio_rx: Option<mpsc::Receiver<Vec<f32>>>,
    /// RX2 audio receiver: Vec<f32> chunks at 48kHz mono
    pub rx2_audio_rx: Option<mpsc::Receiver<Vec<f32>>>,
    /// Binaural right channel audio: Vec<f32> chunks at 48kHz mono (channel 1 of stereo TCI audio)
    pub bin_r_audio_rx: Option<mpsc::Receiver<Vec<f32>>>,
    /// TX audio channel: sends audio to the reader task for low-latency TX_CHRONO response
    tx_audio_tx: Option<mpsc::Sender<Vec<f32>>>,
    /// Whether audio streams are active
    audio_started: bool,

    // --- IQ channels (phase 3) ---
    /// IQ RX1 receiver: Vec<(f32, f32)> I/Q pairs
    pub iq_rx1_rx: Option<mpsc::Receiver<(u32, Vec<(f32, f32)>)>>,
    /// IQ RX2 receiver: Vec<(f32, f32)> I/Q pairs
    pub iq_rx2_rx: Option<mpsc::Receiver<(u32, Vec<(f32, f32)>)>>,
    /// Whether IQ streams are active
    pub(crate) iq_started: bool,
    /// Requested IQ sample rate
    pub(crate) iq_sample_rate: u32,

    // --- Extended controls (TCI _ex commands) ---
    pub step_att_rx1: i32,
    pub step_att_rx2: i32,
    pub diversity_enabled: bool,
    pub diversity_ref: u8,
    pub diversity_source: u8,
    pub diversity_gain_rx1: u16,
    pub diversity_gain_rx2: u16,
    pub diversity_phase: i32,

    // --- DDC sample rate ---
    pub ddc_sample_rate_rx1: u32,
    pub ddc_sample_rate_rx2: u32,
    pub ddc_sample_rates: Vec<u32>,

    // --- Server capabilities ---
    /// Feature flags received from Thetis via tci_caps_ex
    pub server_caps: HashSet<String>,
}

/// Parsed TCI notifications from the reader task
// TciNotification enum defined in tci_parser.rs (imported via `use crate::tci_parser::*`)

impl TciConnection {
    pub fn new(addr: Option<&str>) -> Self {
        Self {
            addr: addr.unwrap_or(DEFAULT_TCI_ADDR).to_string(),
            cmd_tx: None,
            bin_tx: None,
            notify_rx: None,
            connected: false,
            last_connect_attempt: Instant::now() - std::time::Duration::from_secs(10),
            vfo_a_freq: 0,
            vfo_a_mode: 0,
            smeter_window: VecDeque::with_capacity(4),
            smeter_direct_dbm: None,
            power_on: false,
            tx_profile: 0,
            nr_level: 0,
            anf_on: false,
            drive_level: 0,
            rx_af_gain: 100,
            tx_active: false,
            fwd_power_watts: 0.0,
            swr: 1.0,
            filter_low_hz: 0,
            filter_high_hz: 0,
            ctun: false,
            dds_freq: [0; 2],
            vfo_b_freq: 0,
            vfo_b_mode: 0,
            rx2_af_gain: 100,
            smeter_rx2_window: VecDeque::with_capacity(4),
            smeter_rx2_direct_dbm: None,
            rx2_nr_level: 0,
            rx2_anf_on: false,
            rx2_agc_mode: 3,
            rx2_agc_gain: 80,
            rx2_sql_enable: false,
            rx2_sql_level: 0,
            rx2_nb_enable: false,
            rx2_nb_level: 0,
            rx2_binaural: false,
            rx2_apf_enable: false,
            rx2_vfo_lock: false,
            filter_rx2_low_hz: 0,
            filter_rx2_high_hz: 0,
            filter_index: 3,
            filter_rx2_index: 3,
            fm_deviation: 1,
            mon_on: false,
            vfo_sync_on: false,
            agc_mode: 3,
            agc_gain: 80,
            rit_enable: false,
            rit_offset: 0,
            xit_enable: false,
            xit_offset: 0,
            sql_enable: false,
            sql_level: 0,
            nb_enable: false,
            nb_level: 0,
            agc_auto_rx1: false,
            agc_auto_rx2: false,
            diversity_sweep_result: None,
            diversity_auto_progress: None,
            diversity_auto_done: None,
            cw_keyer_speed: 20,
            vfo_lock: false,
            binaural: false,
            apf_enable: false,
            mute: false,
            rx_mute: false,
            nf_enable: false,
            rx2_nf_enable: false,
            rx_balance: 0,
            tune_active: false,
            tune_drive: 0,
            mon_volume: -40,
            meter_cal_offset: [0.0; 2],
            xvtr_gain_offset: [0.0; 2],
            six_m_gain_offset: [0.0; 2],
            smeter_raw_dbm: [None; 2],
            tx_profile_names: Vec::new(),
            tx_profile_name: String::new(),
            rx1_audio_rx: None,
            rx2_audio_rx: None,
            bin_r_audio_rx: None,
            tx_audio_tx: None,
            audio_started: false,
            iq_rx1_rx: None,
            iq_rx2_rx: None,
            iq_started: false,
            iq_sample_rate: 384_000,
            step_att_rx1: 0,
            step_att_rx2: 0,
            ddc_sample_rate_rx1: 0,
            ddc_sample_rate_rx2: 0,
            ddc_sample_rates: Vec::new(),
            diversity_enabled: false,
            diversity_ref: 0,
            diversity_source: 0,
            diversity_gain_rx1: 1000,
            diversity_gain_rx2: 1000,
            diversity_phase: 0,
            server_caps: HashSet::new(),
        }
    }

    /// Check if connection attempt is needed. Returns the WS URL if so.
    /// Updates the rate-limit timer.
    pub fn needs_connect_info(&mut self) -> Option<String> {
        if self.connected {
            return None;
        }
        if self.last_connect_attempt.elapsed().as_secs() < 1 {
            return None;
        }
        self.last_connect_attempt = Instant::now();
        Some(format!("ws://{}", self.addr))
    }

    /// Accept an established WebSocket connection from the background connector.
    pub fn accept_stream(
        &mut self,
        ws_stream: tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) {
        let (ws_write, ws_read) = ws_stream.split();

        let (cmd_tx, cmd_rx) = mpsc::channel::<String>(64);
        let (bin_tx, bin_rx) = mpsc::channel::<Vec<u8>>(32);
        let (notify_tx, notify_rx) = mpsc::channel::<TciNotification>(256);
        let (rx1_audio_tx, rx1_audio_rx) = mpsc::channel::<Vec<f32>>(64);
        let (rx2_audio_tx, rx2_audio_rx) = mpsc::channel::<Vec<f32>>(64);
        let (bin_r_audio_tx, bin_r_audio_rx) = mpsc::channel::<Vec<f32>>(64);
        let (iq_rx1_tx, iq_rx1_rx) = mpsc::channel::<(u32, Vec<(f32, f32)>)>(32);
        let (iq_rx2_tx, iq_rx2_rx) = mpsc::channel::<(u32, Vec<(f32, f32)>)>(32);

        self.cmd_tx = Some(cmd_tx);
        self.bin_tx = Some(bin_tx);
        self.notify_rx = Some(notify_rx);
        self.rx1_audio_rx = Some(rx1_audio_rx);
        self.rx2_audio_rx = Some(rx2_audio_rx);
        self.bin_r_audio_rx = Some(bin_r_audio_rx);
        self.iq_rx1_rx = Some(iq_rx1_rx);
        self.iq_rx2_rx = Some(iq_rx2_rx);
        self.connected = true;

        tokio::spawn(tci_writer_task(ws_write, cmd_rx, bin_rx));

        let (tx_audio_tx, tx_audio_rx) = mpsc::channel::<Vec<f32>>(64);
        self.tx_audio_tx = Some(tx_audio_tx);

        let chrono_bin_tx = self.bin_tx.clone().unwrap();
        tokio::spawn(tci_reader_task(
            ws_read,
            notify_tx,
            rx1_audio_tx,
            rx2_audio_tx,
            bin_r_audio_tx,
            iq_rx1_tx,
            iq_rx2_tx,
            tx_audio_rx,
            chrono_bin_tx,
        ));

        info!("TCI: WebSocket connected to ws://{}", self.addr);
    }

    /// Send a TCI text command (e.g. "VFO:0,0,14200000;")
    pub async fn send(&mut self, cmd: &str) {
        if !self.connected {
            return; // Not connected, skip (connection managed by background task)
        }
        if let Some(ref tx) = self.cmd_tx {
            // Log non-trivial commands (skip high-frequency sensor/audio cmds)
            if !cmd.starts_with("AUDIO") && !cmd.starts_with("IQ")
                && !cmd.starts_with("RX_SENSORS") && !cmd.starts_with("TX_SENSORS")
                && !cmd.starts_with("VOLUME:") && !cmd.starts_with("tx_profiles") {
                info!("TCI send: {}", cmd.trim_end_matches(';'));
            }
            if tx.try_send(cmd.to_string()).is_err() {
                warn!("TCI cmd channel full or closed, disconnecting");
                self.handle_disconnect();
            }
        }
    }

    /// Called periodically (from safety check) to drain notifications and update state.
    /// Replaces CatConnection::poll_and_parse().
    pub async fn poll_and_parse(&mut self) {
        if !self.connected {
            return;
        }

        // Drain all pending notifications into a local vec to avoid double borrow
        let mut notifications = Vec::new();
        if let Some(ref mut rx) = self.notify_rx {
            while let Ok(notif) = rx.try_recv() {
                notifications.push(notif);
            }
        }
        for notif in notifications {
            self.handle_notification(notif).await;
        }
    }

    /// Process a single TCI notification
    async fn handle_notification(&mut self, notif: TciNotification) {
        match notif {
            TciNotification::TciCapsEx { caps } => {
                self.server_caps = caps.into_iter().collect();
                info!("TCI: server caps = {:?}", self.server_caps);
            }
            TciNotification::Ready => {
                info!("TCI: READY received, sending init commands");
                self.power_on = true;
                self.send_init_commands().await;
            }
            TciNotification::Vfo { receiver, channel, freq } => {
                match (receiver, channel) {
                    (0, 0) => {
                        if freq != self.vfo_a_freq {
                            log::debug!("TCI VFO A: {} Hz", freq);
                            self.vfo_a_freq = freq;
                        }
                    }
                    (0, 1) | (1, 0) => {
                        if freq != self.vfo_b_freq {
                            log::debug!("TCI VFO B: {} Hz", freq);
                            self.vfo_b_freq = freq;
                        }
                    }
                    _ => {}
                }
            }
            TciNotification::Dds { receiver, freq } => {
                let idx = (receiver as usize).min(1);
                if freq != self.dds_freq[idx] {
                    log::debug!("TCI DDS[{}]: {} Hz", receiver, freq);
                    self.dds_freq[idx] = freq;
                }
            }
            TciNotification::Modulation { receiver, mode_str } => {
                let mode = mode_str_to_u8(&mode_str);
                match receiver {
                    0 => {
                        if mode != self.vfo_a_mode {
                            info!("TCI mode A: {} ({})", mode_str, mode);
                            self.vfo_a_mode = mode;
                        }
                    }
                    1 => {
                        if mode != self.vfo_b_mode {
                            info!("TCI mode B: {} ({})", mode_str, mode);
                            self.vfo_b_mode = mode;
                        }
                    }
                    _ => {}
                }
            }
            TciNotification::Trx { receiver: _, active } => {
                // Note: we don't update tx_active here — PTT controller manages that
                // But we log it for debugging
                if active != self.tx_active {
                    info!("TCI TRX: {}", if active { "TX" } else { "RX" });
                }
            }
            TciNotification::Drive { receiver: _, value } => {
                if value != self.drive_level {
                    log::debug!("TCI Drive: {}%", value);
                    self.drive_level = value;
                }
            }
            TciNotification::RxFilterBand { receiver, low, high } => {
                match receiver {
                    0 => {
                        if low != self.filter_low_hz || high != self.filter_high_hz {
                            self.filter_low_hz = low;
                            self.filter_high_hz = high;
                        }
                    }
                    1 => {
                        if low != self.filter_rx2_low_hz || high != self.filter_rx2_high_hz {
                            self.filter_rx2_low_hz = low;
                            self.filter_rx2_high_hz = high;
                        }
                    }
                    _ => {}
                }
            }
            TciNotification::RxChannelSensors { receiver, channel: _, dbm } => {
                if HAS_SENSORS_EX.load(std::sync::atomic::Ordering::Relaxed) {
                    // _ex format: Thetis already provides avgdBm — use directly
                    let idx = (receiver as usize).min(1);
                    self.smeter_raw_dbm[idx] = Some(dbm);
                    if receiver == 0 {
                        self.smeter_direct_dbm = Some(dbm);
                    } else {
                        self.smeter_rx2_direct_dbm = Some(dbm);
                    }
                } else {
                    // Legacy format: do our own RMS averaging
                    let mw = 10.0_f32.powf(dbm / 10.0);
                    let window = if receiver == 0 { &mut self.smeter_window } else { &mut self.smeter_rx2_window };
                    if window.len() >= 4 {
                        window.pop_front();
                    }
                    window.push_back(mw);
                }
            }
            TciNotification::TxSensors { _receiver: _, _mic_dbm: _, power_w, _peak_w: _, swr } => {
                self.fwd_power_watts = power_w.clamp(0.0, 200.0);
                self.swr = swr;
            }
            TciNotification::Start => {
                info!("TCI: START");
                self.power_on = true;
            }
            TciNotification::Stop => {
                info!("TCI: STOP");
                self.power_on = false;
            }
            TciNotification::Volume { .. } => {
                // Master volume — not used for RX AF gain sync
            }
            TciNotification::RxVolume { receiver, db, .. } => {
                // Map dB to 0..100% (Thetis sends -60.0..0.0 dB range)
                let pct = (((db + 60.0) * 100.0 / 60.0).clamp(0.0, 100.0)) as u8;
                if receiver == 0 {
                    self.rx_af_gain = pct;
                } else if receiver == 1 {
                    self.rx2_af_gain = pct;
                }
                info!("TCI: RX{} volume = {:.1} dB ({}%)", receiver + 1, db, pct);
            }
            TciNotification::DdcSampleRateEx { receiver, rate } => {
                let old = if receiver == 0 { self.ddc_sample_rate_rx1 } else { self.ddc_sample_rate_rx2 };
                if receiver == 0 { self.ddc_sample_rate_rx1 = rate; }
                else { self.ddc_sample_rate_rx2 = rate; }
                if old != rate {
                    info!("TCI: RX{} DDC sample rate = {}kHz", receiver + 1, rate / 1000);
                }
            }
            TciNotification::DdcSampleRatesEx { rates } => {
                self.ddc_sample_rates = rates;
                info!("TCI: available DDC rates = {:?}", self.ddc_sample_rates);
            }
            TciNotification::DiversityAutonullProgress { round, total, phase, gain_db, smeter } => {
                info!("TCI: Auto-null progress {}/{}: phase={:.1}° gain={:.1}dB smeter={:.1}dBm", round, total, phase, gain_db, smeter);
                self.diversity_auto_progress = Some((round, total, phase, gain_db, smeter));
                // Update phase from progress; gain is updated via diversity_gain_ex push
                self.diversity_phase = (phase * 100.0) as i32;
            }
            TciNotification::DiversityAutonullDone { phase, gain_db, improvement_db } => {
                info!("TCI: Auto-null done: phase={:.1}° gain={:.1}dB improvement={:.1}dB", phase, gain_db, improvement_db);
                self.diversity_auto_done = Some((phase, gain_db, improvement_db));
            }
            TciNotification::DiversityAutonullError { message } => {
                warn!("TCI: Auto-null error: {}", message);
            }
            TciNotification::DiversitySweepResult { sweep_type, results } => {
                let min = results.iter().min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
                if let Some((best_val, best_dbm)) = min {
                    info!("TCI: Diversity sweep {}: {} points, best={:.1} at {:.1}dBm",
                        sweep_type, results.len(), best_val, best_dbm);
                }
                self.diversity_sweep_result = Some((sweep_type, results));
            }
            TciNotification::AgcAutoEx { receiver, enabled } => {
                if receiver == 0 { self.agc_auto_rx1 = enabled; }
                else { self.agc_auto_rx2 = enabled; }
                info!("TCI: RX{} AGC Auto = {}", receiver + 1, enabled);
            }
            TciNotification::RxAnfEnable { receiver, enabled } => {
                if receiver == 0 { self.anf_on = enabled; }
                else { self.rx2_anf_on = enabled; }
                info!("TCI: RX{} ANF = {}", receiver + 1, enabled);
            }
            TciNotification::RxNrEnable { receiver, enabled, level } => {
                let nr = if enabled { level.max(1) } else { 0 };
                if receiver == 0 { self.nr_level = nr; }
                else { self.rx2_nr_level = nr; }
                info!("TCI: RX{} NR = {} (level {})", receiver + 1, enabled, nr);
            }
            TciNotification::TxChrono { .. } => {
                // Handled directly in reader task for low latency
            }
            TciNotification::RxChannelEnable { receiver: 0, channel: 1, enabled } => {
                info!("TCI: RX2 (channel B) {}", if enabled { "enabled" } else { "disabled" });
                if enabled && self.audio_started {
                    // (Re)start audio and IQ for RX2 when it gets enabled
                    self.send("AUDIO_START:1;").await;
                    self.send("IQ_START:1;").await;
                }
            }
            TciNotification::RxChannelEnable { .. } => {}
            TciNotification::MonEnable { enabled } => {
                if enabled != self.mon_on {
                    info!("TCI: MON {}", if enabled { "ON" } else { "OFF" });
                    self.mon_on = enabled;
                }
            }
            TciNotification::AgcMode { receiver: 0, mode } => { self.agc_mode = mode; }
            TciNotification::AgcMode { receiver: 1, mode } => { self.rx2_agc_mode = mode; }
            TciNotification::AgcMode { .. } => {}
            TciNotification::AgcGain { receiver: 0, gain } => { self.agc_gain = gain; }
            TciNotification::AgcGain { receiver: 1, gain } => { self.rx2_agc_gain = gain; }
            TciNotification::AgcGain { .. } => {}
            TciNotification::RitEnable { receiver: 0, enabled } => { self.rit_enable = enabled; }
            TciNotification::RitEnable { .. } => {}
            TciNotification::RitOffset { receiver: 0, offset } => { self.rit_offset = offset; }
            TciNotification::RitOffset { .. } => {}
            TciNotification::XitEnable { receiver: 0, enabled } => { self.xit_enable = enabled; }
            TciNotification::XitEnable { .. } => {}
            TciNotification::XitOffset { receiver: 0, offset } => { self.xit_offset = offset; }
            TciNotification::XitOffset { .. } => {}
            TciNotification::SqlEnable { receiver: 0, enabled } => { self.sql_enable = enabled; }
            TciNotification::SqlEnable { receiver: 1, enabled } => { self.rx2_sql_enable = enabled; }
            TciNotification::SqlEnable { .. } => {}
            TciNotification::SqlLevel { receiver: 0, level } => { self.sql_level = level; }
            TciNotification::SqlLevel { receiver: 1, level } => { self.rx2_sql_level = level; }
            TciNotification::SqlLevel { .. } => {}
            TciNotification::NbEnable { receiver: 0, enabled, level } => {
                // Symmetrisch met NR-handler (regel 553): forceer level=0 bij enabled=false.
                // Thetis stuurt soms het laatste level terug met enabled=false; zonder deze
                // guard blijft client-side `nb_level` op bv. 2 hangen → NB-knop cycle't niet
                // terug naar "uit". Fix voor PATCH-nb-toggle-fix.
                self.nb_enable = enabled;
                self.nb_level = if enabled { level } else { 0 };
            }
            TciNotification::NbEnable { receiver: 1, enabled, level } => {
                self.rx2_nb_enable = enabled;
                self.rx2_nb_level = if enabled { level } else { 0 };
            }
            TciNotification::NbEnable { .. } => {}
            TciNotification::CwKeyerSpeed { speed } => { self.cw_keyer_speed = speed; }
            TciNotification::VfoLock { enabled } => {
                info!("TCI notify: vfo_lock A = {}", enabled);
                self.vfo_lock = enabled;
            }
            TciNotification::VfoLockB { enabled } => {
                info!("TCI notify: vfo_lock B = {}", enabled);
                self.rx2_vfo_lock = enabled;
            }
            TciNotification::BinEnable { receiver: 0, enabled } => { self.binaural = enabled; }
            TciNotification::BinEnable { receiver: 1, enabled } => { self.rx2_binaural = enabled; }
            TciNotification::BinEnable { .. } => {}
            TciNotification::ApfEnable { receiver: 0, enabled } => {
                info!("TCI notify: apf_enable rx1 = {}", enabled);
                self.apf_enable = enabled;
            }
            TciNotification::ApfEnable { receiver: 1, enabled } => {
                info!("TCI notify: apf_enable rx2 = {}", enabled);
                self.rx2_apf_enable = enabled;
            }
            TciNotification::ApfEnable { .. } => {}
            TciNotification::Mute { enabled } => {
                info!("TCI notify: mute = {}", enabled);
                self.mute = enabled;
            }
            TciNotification::RxMute { receiver: 0, enabled } => {
                info!("TCI notify: rx_mute rx1 = {}", enabled);
                self.rx_mute = enabled;
            }
            TciNotification::RxMute { .. } => {}
            TciNotification::NfEnable { receiver: 0, enabled } => {
                info!("TCI notify: nf_enable rx1 = {}", enabled);
                self.nf_enable = enabled;
            }
            TciNotification::NfEnable { receiver: 1, enabled } => {
                info!("TCI notify: nf_enable rx2 = {}", enabled);
                self.rx2_nf_enable = enabled;
            }
            TciNotification::NfEnable { .. } => {}
            TciNotification::RxBalance { receiver: 0, channel: 0, value } => {
                info!("TCI notify: rx_balance = {}", value);
                self.rx_balance = value.clamp(-40, 40) as i8;
            }
            TciNotification::RxBalance { .. } => {}
            TciNotification::Tune { receiver: 0, active } => {
                if active != self.tune_active {
                    info!("TCI: TUNE {}", if active { "ON" } else { "OFF" });
                    self.tune_active = active;
                }
            }
            TciNotification::Tune { .. } => {}
            TciNotification::TuneDrive { receiver: 0, power } => {
                if power != self.tune_drive {
                    info!("TCI: Tune drive = {}%", power);
                    self.tune_drive = power;
                }
            }
            TciNotification::TuneDrive { .. } => {}
            TciNotification::MonVolume { db } => {
                if db != self.mon_volume {
                    info!("TCI: Mon volume = {} dB", db);
                    self.mon_volume = db;
                }
            }
            TciNotification::CalibrationEx { receiver, meter_cal, xvtr_gain, six_m_gain } => {
                let idx = (receiver as usize).min(1);
                let changed = (meter_cal - self.meter_cal_offset[idx]).abs() > 0.01
                    || (xvtr_gain - self.xvtr_gain_offset[idx]).abs() > 0.01
                    || (six_m_gain - self.six_m_gain_offset[idx]).abs() > 0.01;
                if changed {
                    info!("TCI: calibration_ex rx{} meter_cal={:.2} xvtr={:.1} 6m={:.1}",
                        receiver, meter_cal, xvtr_gain, six_m_gain);
                    self.meter_cal_offset[idx] = meter_cal;
                    self.xvtr_gain_offset[idx] = xvtr_gain;
                    self.six_m_gain_offset[idx] = six_m_gain;
                }
            }
            TciNotification::TxProfilesEx { names } => {
                info!("TCI: TX profiles: {:?}", names);
                self.tx_profile_names = names;
                // Recalculate active index if we already have a profile name
                if !self.tx_profile_name.is_empty() {
                    let idx = self.tx_profile_names.iter()
                        .position(|n| n == &self.tx_profile_name)
                        .unwrap_or(0) as u8;
                    self.tx_profile = idx;
                    info!("TCI: TX profile index recalculated: \"{}\" = {}", self.tx_profile_name, idx);
                }
            }
            TciNotification::TxProfileEx { name } => {
                if name != self.tx_profile_name {
                    let idx = self.tx_profile_names.iter()
                        .position(|n| n == &name)
                        .unwrap_or(0) as u8;
                    info!("TCI: TX profile = \"{}\" (index {})", name, idx);
                    self.tx_profile_name = name;
                    self.tx_profile = idx;
                }
            }
            // ── ThetisLink extended controls (state tracking) ─────────────
            TciNotification::CtunEx { receiver, enabled } => {
                if receiver == 0 { self.ctun = enabled; }
                info!("TCI: CTUN RX{} = {}", receiver + 1, enabled);
            }
            TciNotification::VfoSyncEx { enabled } => {
                self.vfo_sync_on = enabled;
                info!("TCI: VFO sync = {}", enabled);
            }
            TciNotification::FmDeviationEx { receiver, hz } => {
                self.fm_deviation = if hz >= 5000 { 1 } else { 0 };
                log::debug!("TCI: FM deviation RX{} = {} Hz", receiver + 1, hz);
            }
            TciNotification::StepAttenuatorEx { receiver, db } => {
                if receiver == 0 { self.step_att_rx1 = db; }
                else { self.step_att_rx2 = db; }
                info!("TCI: Step ATT RX{} = {} dB", receiver + 1, db);
            }
            TciNotification::DiversityEnableEx { enabled } => {
                self.diversity_enabled = enabled;
                info!("TCI: Diversity = {}", enabled);
            }
            TciNotification::DiversityRefEx { rx1_ref } => {
                self.diversity_ref = if rx1_ref { 0 } else { 1 };
                info!("TCI: Diversity ref = RX{}", if rx1_ref { 1 } else { 2 });
            }
            TciNotification::DiversitySourceEx { source } => {
                self.diversity_source = source as u8;
                info!("TCI: Diversity source = {}", source);
            }
            TciNotification::DiversityGainEx { receiver, gain } => {
                if receiver == 0 { self.diversity_gain_rx1 = gain; }
                else { self.diversity_gain_rx2 = gain; }
                info!("TCI: Diversity gain RX{} = {}", receiver + 1, gain);
            }
            TciNotification::DiversityPhaseEx { phase } => {
                self.diversity_phase = phase;
                info!("TCI: Diversity phase = {}", phase);
            }
            TciNotification::RxAudio { .. } | TciNotification::IqStream { .. } => {
                // These are routed directly to their channels by the reader task
                // They shouldn't arrive here, but ignore if they do
            }
            TciNotification::Disconnected => {
                warn!("TCI: WebSocket disconnected");
                self.handle_disconnect();
            }
        }
    }

    /// Send initialization commands after READY.
    /// Commands are grouped in batches with short delays to avoid overwhelming Thetis
    /// with a burst of 40+ commands (see Ramdor feedback, TCI prio #16).
    /// VOLUME is NOT set — user retains their own Thetis volume setting.
    async fn send_init_commands(&mut self) {
        let delay = tokio::time::Duration::from_millis(10);

        // Batch 1: Audio + IQ config and start
        self.send("AUDIO_SAMPLERATE:48000;").await;
        self.send("AUDIO_STREAM_SAMPLE_TYPE:float32;").await;
        let ch = if self.binaural { 2 } else { 1 };
        self.send(&format!("AUDIO_STREAM_CHANNELS:{};", ch)).await;
        self.send("AUDIO_STREAM_SAMPLES:960;").await;
        self.send("AUDIO_START:0;").await;
        self.send("AUDIO_START:1;").await;
        self.audio_started = true;
        self.send("IQ_START:0;").await;
        self.send("IQ_START:1;").await;
        self.iq_started = true;
        tokio::time::sleep(delay).await;

        // Batch 2: Sensor enables + VFO/mode queries (needed for spectrum display)
        self.send("RX_SENSORS_ENABLE:true,100;").await;
        self.send("TX_SENSORS_ENABLE:true,100;").await;
        self.send("VFO:0,0;").await;
        self.send("VFO:0,1;").await;
        self.send("VFO:1,0;").await;
        self.send("MODULATION:0;").await;
        self.send("MODULATION:1;").await;
        self.send("DDS:0;").await;
        self.send("DDS:1;").await;
        tokio::time::sleep(delay).await;

        // Batch 3: RX1 control state queries (standard TCI, v2.10.3.13+)
        self.send("agc_mode:0;").await;
        self.send("agc_gain:0;").await;
        self.send("rit_enable:0;").await;
        self.send("rit_offset:0;").await;
        self.send("xit_enable:0;").await;
        self.send("xit_offset:0;").await;
        self.send("sql_enable:0;").await;
        self.send("sql_level:0;").await;
        self.send("rx_nb_enable:0;").await;
        self.send("cw_keyer_speed;").await;
        self.send("vfo_lock:0,0;").await;
        self.send("vfo_lock:1,0;").await;
        self.send("rx_bin_enable:0;").await;
        self.send("rx_apf_enable:0;").await;
        self.send("rx_nf_enable:0;").await;
        self.send("rx_balance:0,0;").await;
        self.send("tune_drive:0;").await;
        self.send("mon_volume;").await;
        self.send("rx_nr_enable:0;").await;
        self.send("rx_anf_enable:0;").await;
        self.send("rx_volume:0,0;").await;
        tokio::time::sleep(delay).await;

        // Batch 4: RX2 control state queries
        self.send("agc_mode:1;").await;
        self.send("agc_gain:1;").await;
        self.send("sql_enable:1;").await;
        self.send("sql_level:1;").await;
        self.send("rx_nb_enable:1;").await;
        self.send("rx_bin_enable:1;").await;
        self.send("rx_apf_enable:1;").await;
        self.send("rx_nf_enable:1;").await;
        tokio::time::sleep(delay).await;

        // Batch 5: Extension queries (capability-gated per command, or fork extensions)
        if self.has_cap("tx_profiles_ex") || self.has_extensions() {
            self.send("tx_profiles_ex;").await;
        }
        if self.has_cap("calibration_ex") || self.has_extensions() {
            self.send("calibration_ex:0;").await;
            self.send("calibration_ex:1;").await;
        }
        if self.has_cap("ctun_ex") {
            self.send("rx_ctun_ex:0;").await;
            self.send("rx_ctun_ex:1;").await;
        }
        if self.has_cap("agc_auto_ex") {
            self.send("agc_auto_ex;").await;
        }
        if self.has_cap("vfo_sync_ex") {
            self.send("vfo_sync_ex;").await;
        }
        if self.has_cap("step_attenuator_ex") {
            self.send("step_attenuator_ex:0;").await;
            self.send("step_attenuator_ex:1;").await;
        }
        if self.has_cap("ddc_sample_rate_ex") {
            self.send("ddc_sample_rates_ex;").await;
            self.send("ddc_sample_rate_ex;").await;
        }
        if self.has_cap("diversity_ex") {
            self.send("diversity_enable_ex;").await;
            self.send("diversity_ref_ex;").await;
            self.send("diversity_source_ex;").await;
            self.send("diversity_gain_ex:0;").await;
            self.send("diversity_gain_ex:1;").await;
            self.send("diversity_phase_ex;").await;
        }

        info!("TCI: init commands sent (audio 48kHz float32, {} batches with {}ms delay)",
            5, delay.as_millis());
    }


    /// Write TX audio samples to the reader task for TX_CHRONO response
    pub fn write_tx_audio(&mut self, samples: &[f32]) {
        if let Some(ref tx) = self.tx_audio_tx {
            let _ = tx.try_send(samples.to_vec());
        }
    }

    fn handle_disconnect(&mut self) {
        self.connected = false;
        self.cmd_tx = None;
        self.bin_tx = None;
        self.notify_rx = None;
        self.rx1_audio_rx = None;
        self.rx2_audio_rx = None;
        self.bin_r_audio_rx = None;
        self.iq_rx1_rx = None;
        self.iq_rx2_rx = None;
        self.power_on = false;
        self.audio_started = false;
        self.iq_started = false;
        self.tx_audio_tx = None;
        self.server_caps.clear();
    }

    /// Check if connected Thetis advertises a capability
    pub fn has_cap(&self, cap: &str) -> bool {
        self.server_caps.contains(cap)
    }

    /// Check if ThetisLink extensions are active (PA3GHM fork with extensions checkbox ON).
    /// The fork enables all extensions together via a single checkbox. We detect this by
    /// checking for `ctun_ex` which is always present when extensions are enabled.
    /// This is used as a proxy for caps that the fork doesn't individually advertise
    /// (e.g. rx_nr_enable_ex, rx_nb_enable_ex, tx_profiles_ex, calibration_ex).
    pub fn has_extensions(&self) -> bool {
        self.server_caps.contains("ctun_ex")
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    // --- S-meter conversion ---

    fn avg_mw_to_display(window: &VecDeque<f32>) -> u16 {
        if window.is_empty() {
            return 0;
        }
        let sum: f32 = window.iter().sum();
        let avg_mw = sum / window.len() as f32;
        let avg_dbm = 10.0 * avg_mw.log10();
        sdr_remote_core::dbm_to_display(avg_dbm)
    }

    pub fn smeter_avg(&self) -> u16 {
        if let Some(dbm) = self.smeter_direct_dbm {
            sdr_remote_core::dbm_to_display(dbm)
        } else {
            Self::avg_mw_to_display(&self.smeter_window)
        }
    }

    pub fn smeter_rx2_avg(&self) -> u16 {
        if let Some(dbm) = self.smeter_rx2_direct_dbm {
            sdr_remote_core::dbm_to_display(dbm)
        } else {
            Self::avg_mw_to_display(&self.smeter_rx2_window)
        }
    }

    pub fn fwd_power_raw(&self) -> u16 {
        (self.fwd_power_watts * 10.0).round() as u16
    }

    pub fn set_tx_active(&mut self, active: bool) {
        if active != self.tx_active {
            self.tx_active = active;
            self.smeter_window.clear();
            self.smeter_rx2_window.clear();
            self.smeter_direct_dbm = None;
            self.smeter_rx2_direct_dbm = None;
            if !active {
                self.fwd_power_watts = 0.0;
            }
        }
    }


    // Setter methods (set_vfo_a_freq, set_mode, etc.) moved to tci_commands.rs
}

// --- WebSocket writer task ---

async fn tci_writer_task(
    mut ws_write: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
        Message,
    >,
    mut cmd_rx: mpsc::Receiver<String>,
    mut bin_rx: mpsc::Receiver<Vec<u8>>,
) {
    loop {
        tokio::select! {
            Some(cmd) = cmd_rx.recv() => {
                if let Err(e) = ws_write.send(Message::Text(cmd.into())).await {
                    warn!("TCI write error: {}", e);
                    break;
                }
            }
            Some(data) = bin_rx.recv() => {
                if let Err(e) = ws_write.send(Message::Binary(data.into())).await {
                    warn!("TCI binary write error: {}", e);
                    break;
                }
            }
            else => break,
        }
    }
}

// --- WebSocket reader task ---

async fn tci_reader_task(
    mut ws_read: futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    >,
    notify_tx: mpsc::Sender<TciNotification>,
    rx1_audio_tx: mpsc::Sender<Vec<f32>>,
    rx2_audio_tx: mpsc::Sender<Vec<f32>>,
    bin_r_audio_tx: mpsc::Sender<Vec<f32>>,
    iq_rx1_tx: mpsc::Sender<(u32, Vec<(f32, f32)>)>,
    iq_rx2_tx: mpsc::Sender<(u32, Vec<(f32, f32)>)>,
    mut tx_audio_rx: mpsc::Receiver<Vec<f32>>,
    chrono_bin_tx: mpsc::Sender<Vec<u8>>,
) {
    // TX audio ring buffer for low-latency TX_CHRONO response
    let mut tx_ring: VecDeque<f32> = VecDeque::with_capacity(TX_RING_CAPACITY);
    let mut audio_debug_rx0 = false;
    let mut audio_debug_rx1 = false;
    let mut audio_debug_bin_r = false;
    loop {
        // Drain any pending TX audio into ring buffer (non-blocking)
        while let Ok(samples) = tx_audio_rx.try_recv() {
            while tx_ring.len() + samples.len() > TX_RING_CAPACITY {
                tx_ring.pop_front();
            }
            tx_ring.extend(&samples);
        }

        let msg_result = tokio::select! {
            msg = ws_read.next() => {
                match msg {
                    Some(r) => r,
                    None => break,
                }
            }
            Some(samples) = tx_audio_rx.recv() => {
                // TX audio arrived while waiting for WebSocket message
                while tx_ring.len() + samples.len() > TX_RING_CAPACITY {
                    tx_ring.pop_front();
                }
                tx_ring.extend(&samples);
                continue; // Go back to draining + waiting
            }
        };
        match msg_result {
            Ok(Message::Text(text)) => {
                let text_str: &str = &text;
                for line in text_str.split('\n') {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    // TCI commands end with ; but may have multiple per line
                    for cmd in line.split(';') {
                        let cmd = cmd.trim();
                        if cmd.is_empty() {
                            continue;
                        }
                        if let Some(notif) = parse_tci_text(cmd) {
                            let _ = notify_tx.try_send(notif);
                        }
                    }
                }
            }
            Ok(Message::Binary(data)) => {
                if data.len() < TCI_HEADER_SIZE { continue; }
                let receiver = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                let sample_rate = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                let format = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
                let length = u32::from_le_bytes([data[20], data[21], data[22], data[23]]);
                let stream_type = u32::from_le_bytes([data[24], data[25], data[26], data[27]]);
                let channels = u32::from_le_bytes([data[28], data[29], data[30], data[31]]);
                let payload = &data[TCI_HEADER_SIZE..];

                // Log first audio frame per receiver (one-shot diagnostics)
                if stream_type == STREAM_TYPE_RX_AUDIO {
                    if receiver == 0 && !audio_debug_rx0 {
                        audio_debug_rx0 = true;
                        info!("TCI AUDIO RX1: rate={} format={} length={} channels={} payload_bytes={}",
                            sample_rate, format, length, channels, payload.len());
                    }
                    if receiver == 1 && !audio_debug_rx1 {
                        audio_debug_rx1 = true;
                        info!("TCI AUDIO RX2: rate={} format={} length={} channels={} payload_bytes={}",
                            sample_rate, format, length, channels, payload.len());
                    }
                }

                match stream_type {
                    STREAM_TYPE_RX_AUDIO => {
                        let (left, right) = decode_audio_payload_stereo(payload, format, length, channels);
                        let tx = if receiver == 0 { &rx1_audio_tx } else { &rx2_audio_tx };
                        let _ = tx.try_send(left);
                        // Send right channel (binaural) only for RX1 and only when stereo
                        if receiver == 0 && !right.is_empty() {
                            if !audio_debug_bin_r {
                                info!("TCI BinR: first stereo R channel ({} samples)", right.len());
                                audio_debug_bin_r = true;
                            }
                            let _ = bin_r_audio_tx.try_send(right);
                        }
                    }
                    STREAM_TYPE_IQ => {
                        let iq_pairs = decode_iq_payload(payload, format, length, channels);
                        let tx = if receiver == 0 { &iq_rx1_tx } else { &iq_rx2_tx };
                        let _ = tx.try_send((sample_rate, iq_pairs));
                    }
                    STREAM_TYPE_TX_CHRONO => {
                        // Drain any pending TX audio into ring buffer
                        while let Ok(samples) = tx_audio_rx.try_recv() {
                            while tx_ring.len() + samples.len() > TX_RING_CAPACITY {
                                tx_ring.pop_front();
                            }
                            tx_ring.extend(&samples);
                        }

                        // Respond immediately with buffered audio
                        let n = length as usize;
                        let mut audio = vec![0.0f32; n];
                        let available = tx_ring.len().min(n);
                        for i in 0..available {
                            audio[i] = tx_ring.pop_front().unwrap_or(0.0);
                        }

                        let frame = build_tci_binary_frame(
                            0, sample_rate, format, n as u32,
                            STREAM_TYPE_TX_AUDIO, channels, &audio, format,
                        );
                        let _ = chrono_bin_tx.try_send(frame);
                    }
                    _ => {}
                }
            }
            Ok(Message::Close(_)) | Err(_) => {
                let _ = notify_tx.try_send(TciNotification::Disconnected);
                break;
            }
            _ => {}
        }
    }
}


// Parser functions, binary decoders, and mode conversion moved to tci_parser.rs
