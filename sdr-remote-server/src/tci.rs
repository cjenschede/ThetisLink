// SPDX-License-Identifier: GPL-2.0-or-later

#![allow(dead_code)]

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use futures_util::{SinkExt, StreamExt};
use log::{debug, info, warn};
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
    enabled: bool,
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
    /// Direct dBm value from TCI _ex format (bypasses window averaging).
    /// This is the "Avg" source - the default S-meter value (true-mean detector).
    pub smeter_direct_dbm: Option<f32>,
    /// Sig source dBm (peak-hold detector, ~100 ms decay). `_ex` format only.
    pub smeter_sig_dbm: Option<f32>,
    /// MaxBin source dBm (single highest FFT bin). _ex format only.
    pub smeter_peakbin_dbm: Option<f32>,
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
    /// Per-RX CTUN state cache, gevuld via TCI `rx_ctun_ex` push notifications.
    /// `ctun` (boven) is voor RX1 (legacy field-naam); `ctun_rx2` voor RX2.
    /// TL2-1 ctun-auto-recenter gebruikt deze om bij VFO-event te checken of CTUN AAN
    /// is - zo niet, eerst ZZCN1/ZZCO1 forceren (per operator-keuze 2026-05-08).
    pub ctun_rx2: bool,
    /// DDS center frequency per receiver (from TCI DDS notification)
    pub dds_freq: [u64; 2],
    // RX2 / VFO-B state
    pub vfo_b_freq: u64,
    pub vfo_b_mode: u8,
    pub rx2_af_gain: u8,
    pub smeter_rx2_window: VecDeque<f32>,
    /// RX2 Avg source dBm (mirror of smeter_direct_dbm for RX1).
    pub smeter_rx2_direct_dbm: Option<f32>,
    /// RX2 Sig source dBm.
    pub smeter_rx2_sig_dbm: Option<f32>,
    /// RX2 MaxBin source dBm.
    pub smeter_rx2_peakbin_dbm: Option<f32>,
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
    // Thetis TX-EQ (ZZET) snapshot for WAV-playback bypass. During a WAV
    // playback to the main radio the client asks us to turn TX-EQ off; we
    // query the operator's real setting first so we can restore the exact
    // prior state afterwards (not just force it on).
    pub thetis_txeq: bool,
    txeq_bypass_saved: Option<bool>,
    txeq_bypass_pending: bool,
    txeq_bypass_active: bool,
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
    /// S9-frequency threshold in Hz. Above this VFO frequency the S-meter
    /// scale shifts to S9 = -93 dBm (IARU VHF/UHF). Updated dynamically from
    /// the fork-extension push `s9_frequency_ex`; default 50_000_000 Hz
    /// matches the IARU Region 1 convention so stock-Thetis / extensions-off
    /// renders correctly without the fork push.
    pub s9_frequency_hz: u64,
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
    /// Stock v2.10.3.14: rx_step_att_enabled_ex per receiver
    pub step_att_enabled_rx1: bool,
    pub step_att_enabled_rx2: bool,
    /// Stock v2.10.3.14: rx_preamp_att_ex (signed; negative = preamp gain)
    pub preamp_att_rx1: i32,
    pub preamp_att_rx2: i32,
    /// Stock v2.10.3.14: tx_filter_band_ex (Hz)
    pub tx_filter_low_hz: i32,
    pub tx_filter_high_hz: i32,
    /// True zodra Thetis ten minste één tx_filter_band_ex heeft gemeld ->
    /// TX-filter is instelbaar (stock 2.10.3.14+ / fork). De client gebruikt
    /// dit (via de TxFilterBand-push) om de TX-bandbreedte-UI te activeren.
    pub tx_filter_known: bool,
    pub diversity_enabled: bool,
    /// Last-known Thetis "Receive only" (RXOnly) state, tracked via the
    /// `rx_only_ex` echo. The TL-server snapshots this before taking over
    /// (RX-only Amplitec position) and restores it on release.
    pub rx_only: bool,
    pub diversity_ref: u8,
    pub diversity_source: u8,
    pub diversity_gain_rx1: u16,
    pub diversity_gain_rx2: u16,
    pub diversity_gain_multi: u16,  // multi × 100 (range 100..1000 = 1.00..10.00); TL2-1 fork-only
    pub diversity_phase: i32,

    // --- DDC sample rate ---
    pub ddc_sample_rate_rx1: u32,
    pub ddc_sample_rate_rx2: u32,
    pub ddc_sample_rates: Vec<u32>,

    // --- Server capabilities ---
    /// Feature flags received from Thetis via tci_caps_ex
    pub server_caps: HashSet<String>,

    /// Last poll for filter preset index (ZZFI/ZZFJ via run_cat_ex)
    /// - stock v2.10.3.14 has no native push for filter preset index.
    last_filter_preset_poll: Instant,

    /// TL2-1 ctun-auto-recenter: per-RX trigger state-machines + flag-clear timers.
    /// Cap-gated via `auto_recenter_ex` (advertised by Thetis fork only when the
    /// extensions vink is on). Serialized via &mut self / single async task -
    /// no separate mutex needed.
    pub ctun_recenter: crate::ctun_recenter::CtunRecenterState,

    /// TL2-1 ctun-auto-recenter: track previous PTT state for off-edge detection
    /// (PTT-off forceert eval). Mirror van self.tx_active.
    prev_tx_active_for_ctun: bool,

    /// TL2-1 ctun-auto-recenter: previous VFO per RX for band-switch heuristiek
    /// (VFO-jump > DDC-bandwidth -> band-switch detected).
    prev_vfo_a_for_ctun: u64,
    prev_vfo_b_for_ctun: u64,

    /// TL2-1 ctun-auto-recenter: cached effective zoom (MIN-aggregatie over alle
    /// verbonden clients per RX). Network.rs updatet deze waarden bij elke
    /// client-zoom-change/connect/disconnect. tci.rs gebruikt ze in trigger-eval
    /// bij vfo-events. None = geen clients met spectrum_enabled voor die RX.
    pub effective_zoom_rx1_cache: Option<f32>,
    pub effective_zoom_rx2_cache: Option<f32>,

    /// TL2-1 ctun-auto-recenter: Thetis-originated TX state uit
    /// `TciNotification::Trx`. `self.tx_active` wordt door PttController gezet
    /// (TL-server PTT-pad). Bij Thetis-zelf, footswitch, etc. update Thetis ons via
    /// TRX-notif maar PttController ziet dat niet -> ctun trigger zou recenter-burst
    /// kunnen starten tijdens TX. Deze flag mirrort het TRX-event zodat de
    /// trigger-eval kan kiezen `tx_active OR thetis_tx_active`.
    pub thetis_tx_active: bool,
    /// Gedeelde "huidige Amplitec-A positie is RX-only" flag (clone van de
    /// flag in `PttController::tx_blocked`). Wordt door de broadcast-loop in
    /// `network.rs` geüpdate; gebruikt in de TRX-notification handler om
    /// Thetis-direct PTT (spatiebalk, hardware-PTT) onmiddellijk te stoppen
    /// - daar pakken de server-initiated PTT-paden niet bij (die hebben hun
    /// eigen preventieve gate). Default: dummy Arc die altijd `false` is.
    tx_blocked: Arc<AtomicBool>,
}

/// Parsed TCI notifications from the reader task
// TciNotification enum defined in tci_parser.rs (imported via `use crate::tci_parser::*`)

impl TciConnection {
    /// Injecteer de gedeelde TX-blocked flag van `PttController`. Wordt
    /// gebruikt in de TRX-notification handler om Thetis-direct PTT op
    /// een RX-only positie onmiddellijk te annuleren.
    pub fn set_tx_blocked_handle(&mut self, flag: Arc<AtomicBool>) {
        self.tx_blocked = flag;
    }

    pub fn new(addr: Option<&str>) -> Self {
        // Cold-boot defensiveness: on Windows, Instant::now() reflects QPC ticks
        // since system boot. If the server starts within ~60s of cold-boot, the
        // raw `Instant::now() - Duration::from_secs(N)` operator panics with
        // "overflow when subtracting duration from instant" (memory:
        // project_tl2_coldboot_bind_fail.md, fix verified 2026-05-05). Use
        // checked_sub with Instant::now() fallback to keep the field safely-set
        // before any real timing-comparison happens at runtime.
        Self {
            addr: addr.unwrap_or(DEFAULT_TCI_ADDR).to_string(),
            enabled: addr.is_some(),
            cmd_tx: None,
            bin_tx: None,
            notify_rx: None,
            connected: false,
            last_connect_attempt: Instant::now()
                .checked_sub(std::time::Duration::from_secs(10))
                .unwrap_or_else(Instant::now),
            vfo_a_freq: 0,
            vfo_a_mode: 0,
            smeter_window: VecDeque::with_capacity(4),
            smeter_direct_dbm: None,
            smeter_sig_dbm: None,
            smeter_peakbin_dbm: None,
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
            ctun_rx2: false,
            dds_freq: [0; 2],
            vfo_b_freq: 0,
            vfo_b_mode: 0,
            rx2_af_gain: 100,
            smeter_rx2_window: VecDeque::with_capacity(4),
            smeter_rx2_direct_dbm: None,
            smeter_rx2_sig_dbm: None,
            smeter_rx2_peakbin_dbm: None,
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
            thetis_txeq: true,
            txeq_bypass_saved: None,
            txeq_bypass_pending: false,
            txeq_bypass_active: false,
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
            s9_frequency_hz: 50_000_000, // IARU Region 1 default; overridden by s9_frequency_ex push when the fork is active
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
            step_att_enabled_rx1: false,
            step_att_enabled_rx2: false,
            preamp_att_rx1: 0,
            preamp_att_rx2: 0,
            tx_filter_low_hz: 0,
            tx_filter_high_hz: 0,
            tx_filter_known: false,
            ddc_sample_rate_rx1: 0,
            ddc_sample_rate_rx2: 0,
            ddc_sample_rates: Vec::new(),
            diversity_enabled: false,
            rx_only: false,
            diversity_ref: 0,
            diversity_source: 0,
            diversity_gain_rx1: 1000,
            diversity_gain_rx2: 1000,
            diversity_gain_multi: 100,  // 1.00x default (matches DiversityForm spinner default)
            diversity_phase: 0,
            server_caps: HashSet::new(),
            last_filter_preset_poll: Instant::now()
                .checked_sub(std::time::Duration::from_secs(60))
                .unwrap_or_else(Instant::now),
            ctun_recenter: crate::ctun_recenter::CtunRecenterState::new(),
            prev_tx_active_for_ctun: false,
            prev_vfo_a_for_ctun: 0,
            prev_vfo_b_for_ctun: 0,
            effective_zoom_rx1_cache: None,
            effective_zoom_rx2_cache: None,
            thetis_tx_active: false,
            tx_blocked: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Check if connection attempt is needed. Returns the WS URL if so.
    /// Updates the rate-limit timer.
    pub fn needs_connect_info(&mut self) -> Option<String> {
        if !self.enabled {
            return None;
        }
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
            // Log non-trivial commands (skip high-frequency sensor/audio cmds
            // and the 2-second ZZFI/ZZFJ filter-preset polls which would
            // otherwise dominate the log).
            if !cmd.starts_with("AUDIO") && !cmd.starts_with("IQ")
                && !cmd.starts_with("RX_SENSORS") && !cmd.starts_with("TX_SENSORS")
                && !cmd.starts_with("VOLUME:") && !cmd.starts_with("tx_profiles")
                && !cmd.starts_with("run_cat_ex:ZZFI") && !cmd.starts_with("run_cat_ex:ZZFJ") {
                debug!("TCI send: {}", cmd.trim_end_matches(';'));
            }
            if tx.try_send(cmd.to_string()).is_err() {
                warn!("TCI cmd channel full or closed, disconnecting");
                self.handle_disconnect();
            }
        }
    }

    /// Begin the Thetis TX-EQ bypass for WAV-playback to the main radio.
    /// Queries the current ZZET state; the RunCatExResponse handler snapshots
    /// it and then turns TX-EQ off. Idempotent while a bypass is active/pending.
    pub async fn thetis_txeq_bypass_begin(&mut self) {
        if self.txeq_bypass_active || self.txeq_bypass_pending {
            return;
        }
        self.txeq_bypass_pending = true;
        self.run_cat("ZZET").await;
    }

    /// End the bypass: restore TX-EQ to the snapshotted prior state (defaults
    /// to on if we never captured one, e.g. the query never answered).
    pub async fn thetis_txeq_bypass_end(&mut self) {
        if !self.txeq_bypass_active && !self.txeq_bypass_pending {
            return;
        }
        let restore_on = self.txeq_bypass_saved.unwrap_or(true);
        self.txeq_bypass_pending = false;
        self.txeq_bypass_active = false;
        self.txeq_bypass_saved = None;
        self.run_cat(if restore_on { "ZZET1" } else { "ZZET0" }).await;
        info!("Thetis TXEQ bypass end: restored to {}", if restore_on { "on" } else { "off" });
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

        // Filter preset index (ZZFI/ZZFJ) is not pushed by stock v2.10.3.14.
        // Poll every 2 s via run_cat_ex over TCI - replaces the legacy aux CAT poll.
        // Skip when fork advertises rx_filter_preset_ex: native push covers the same data.
        if self.power_on
            && !self.has_cap("rx_filter_preset_ex")
            && self.last_filter_preset_poll.elapsed() >= std::time::Duration::from_secs(2)
        {
            self.last_filter_preset_poll = Instant::now();
            self.run_cat("ZZFI").await;
            self.run_cat("ZZFJ").await;
        }

        // TL2-1 ctun-auto-recenter: tick flag-clear timers. Cheap, runs every poll.
        self.ctun_recenter.tick_flag_clear(Instant::now());

        // TL2-1 ctun-auto-recenter: PTT-off transition forceert eval.
        // Anders moet user knoppen voor recenter na lange TX-periode met VFO buiten zone.
        if self.detect_ptt_off_transition() {
            log::debug!("TCI: ctun PTT-off transition - forcing trigger-eval");
            let zr1 = self.effective_zoom_rx1_cache;
            let zr2 = self.effective_zoom_rx2_cache;
            self.trigger_eval_and_act_rx1(zr1).await;
            self.trigger_eval_and_act_rx2(zr2).await;
        }
    }

    // ===== TL2-1 ctun-auto-recenter: trigger evaluation + actie =====

    /// Force CTUN-AAN voor RX1 + RX2 indien fork de cap adverteert. Idempotent.
    /// Aangeroepen bij init (Ready) en bij band-switch detection.
    pub async fn force_ctun_aan_if_capable(&mut self) {
        if !self.has_cap("auto_recenter_ex") {
            return;
        }
        if !self.ctun_recenter.fork_active_logged {
            info!("TCI: ThetisLink ctun-extension active - Thetis recenter logic disabled (cap auto_recenter_ex)");
            self.ctun_recenter.fork_active_logged = true;
        }
        self.run_cat("ZZCN1").await;  // CTUN AAN voor RX1
        self.run_cat("ZZCO1").await;  // CTUN AAN voor RX2 (ZZCO, NIET ZZCP - ZZCP is compander!)
    }

    /// Lazy-ensure: bij VFO-event op een specifieke RX, check of CTUN AAN staat.
    /// Zo niet (operator heeft handmatig CTUN-uit gezet of Thetis-init had het uit):
    /// stuur ZZCN1 (RX1) of ZZCO1 (RX2) en update cache. Per operator-keuze 2026-05-08.
    /// Idempotent: bij CTUN al AAN -> no-op. Cap-gated.
    async fn ensure_ctun_aan_for_rx(&mut self, rx_index: u8) {
        if !self.has_cap("auto_recenter_ex") {
            return;
        }
        match rx_index {
            0 if !self.ctun => {
                debug!("TCI: ctun rx=0 was off - forcing ON before VFO-event eval");
                self.run_cat("ZZCN1").await;
                self.ctun = true; // optimistic; rx_ctun_ex push bevestigt later
            }
            1 if !self.ctun_rx2 => {
                debug!("TCI: ctun rx=1 was off - forcing ON before VFO-event eval");
                self.run_cat("ZZCO1").await;
                self.ctun_rx2 = true;
            }
            _ => {}
        }
    }

    /// Trigger-eval + recenter-actie voor RX1. Aangeroepen vanuit network.rs na elke
    /// vfo-event of zoom-change. `effective_zoom` is None bij 0 spectrum-clients.
    /// Combineer TL-server PTT met Thetis-originated TRX zodat externe PTT
    /// (footswitch, VOX, manual TX-button) ook recenter blokkeert.
    pub async fn trigger_eval_and_act_rx1(&mut self, effective_zoom: Option<f32>) {
        let any_tx = self.tx_active || self.thetis_tx_active;
        let result = crate::ctun_recenter::evaluate_trigger(
            0,
            self.has_cap("auto_recenter_ex"),
            effective_zoom,
            self.vfo_a_freq,
            self.dds_freq[0],
            self.ddc_sample_rate_rx1,
            any_tx,
            &self.ctun_recenter.rx1,
        );
        self.ctun_recenter.rx1.last_eval = Some(result);
        if result.decision == crate::ctun_recenter::TriggerDecision::Trigger {
            self.execute_recenter_rx1().await;
        }
    }

    /// Trigger-eval + recenter-actie voor RX2.
    pub async fn trigger_eval_and_act_rx2(&mut self, effective_zoom: Option<f32>) {
        let any_tx = self.tx_active || self.thetis_tx_active;
        let result = crate::ctun_recenter::evaluate_trigger(
            1,
            self.has_cap("auto_recenter_ex"),
            effective_zoom,
            self.vfo_b_freq,
            self.dds_freq[1],
            self.ddc_sample_rate_rx2,
            any_tx,
            &self.ctun_recenter.rx2,
        );
        self.ctun_recenter.rx2.last_eval = Some(result);
        if result.decision == crate::ctun_recenter::TriggerDecision::Trigger {
            self.execute_recenter_rx2().await;
        }
    }

    /// Recenter-burst RX1: ZZCN0 -> 50ms -> ZZCN1. Sets per-RX flag voor 200ms.
    /// Note: PTT-on tijdens deze burst voltooit binnen flag-window (geen
    /// abort-pad). Inconsistente CTUN-state tussen ZZCN0 en ZZCN1 is risicovoller
    /// dan een korte ZZCN1-tijdens-TX-glitch. `&mut self`-borrow + tokio::sleep
    /// garandeert dat geen andere recenter-actie de burst kan onderbreken.
    ///
    /// **VFO sync coalesce:** wanneer Thetis-side VFO sync ON staat, doet deze
    /// burst óók de RX2-CTUN-toggle interleaved. Beide CTUN-AAN bevelen vuren
    /// daardoor <1 ms van elkaar bij Thetis, dus beide DDCs recentreren op
    /// dezelfde "huidige VFO" snapshot. Zonder coalesce zou de tweede RX-burst
    /// pas 50 ms later starten en bij rapid tuning op een nieuwere VFO landen,
    /// waardoor RX1 en RX2 DDC-centers op verschillende freqs eindigen.
    async fn execute_recenter_rx1(&mut self) {
        let coupled = self.vfo_sync_on;
        let flag_until = Instant::now() + std::time::Duration::from_millis(200);
        self.ctun_recenter.rx1.recentering = true;
        self.ctun_recenter.rx1.flag_clear_at = Some(flag_until);
        if coupled {
            self.ctun_recenter.rx2.recentering = true;
            self.ctun_recenter.rx2.flag_clear_at = Some(flag_until);
        }
        debug!(
            "TCI: ctun recenter rx=0 START vfo={} coupled={}",
            self.vfo_a_freq, coupled
        );
        self.run_cat("ZZCN0").await;
        if coupled {
            self.run_cat("ZZCO0").await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        self.run_cat("ZZCN1").await;
        if coupled {
            self.run_cat("ZZCO1").await;
        }
        // Optimistic DDS-cache update: after ZZCx1 Thetis has forced
        // DDC center = current VFO. Don't wait for the upstream `dds:` push -
        // that adds 100-1000 ms of stale spectrum on the client. If Thetis
        // later pushes a different value (rare, rounding), the equality check
        // in the Dds notification handler covers it.
        self.dds_freq[0] = self.vfo_a_freq;
        if coupled {
            self.dds_freq[1] = self.vfo_b_freq;
        }
        debug!("TCI: ctun recenter rx=0 END coupled={}", coupled);
    }

    /// Recenter-burst RX2: ZZCO0 -> 50ms -> ZZCO1.
    /// **Belangrijk:** RX2-CTUN gebruikt **ZZCO**, niet ZZCP. ZZCP = compander
    /// (audio); ZZCO = CTUN RX2.
    ///
    /// **VFO sync coalesce:** symmetrisch aan `execute_recenter_rx1` - wanneer
    /// VFO sync ON staat doet deze burst óók de RX1-CTUN-toggle interleaved.
    /// Zie commentaar daar voor de motivatie.
    async fn execute_recenter_rx2(&mut self) {
        let coupled = self.vfo_sync_on;
        let flag_until = Instant::now() + std::time::Duration::from_millis(200);
        self.ctun_recenter.rx2.recentering = true;
        self.ctun_recenter.rx2.flag_clear_at = Some(flag_until);
        if coupled {
            self.ctun_recenter.rx1.recentering = true;
            self.ctun_recenter.rx1.flag_clear_at = Some(flag_until);
        }
        debug!(
            "TCI: ctun recenter rx=1 START vfo={} coupled={}",
            self.vfo_b_freq, coupled
        );
        self.run_cat("ZZCO0").await;
        if coupled {
            self.run_cat("ZZCN0").await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        self.run_cat("ZZCO1").await;
        if coupled {
            self.run_cat("ZZCN1").await;
        }
        debug!("TCI: ctun recenter rx=1 END coupled={}", coupled);
    }

    /// Band-switch heuristiek: VFO-jump > DDC-bandwidth -> operator heeft band-switch
    /// gedaan. Forceer CTUN-AAN opnieuw (Thetis kan CTUN-state per band onthouden).
    /// Aangeroepen vanuit network.rs na vfo-event-update.
    pub async fn detect_band_switch_and_reforce(&mut self) {
        if !self.has_cap("auto_recenter_ex") {
            self.prev_vfo_a_for_ctun = self.vfo_a_freq;
            self.prev_vfo_b_for_ctun = self.vfo_b_freq;
            return;
        }
        let bw_rx1 = self.ddc_sample_rate_rx1 as u64;
        let bw_rx2 = self.ddc_sample_rate_rx2 as u64;
        let jump_a = (self.vfo_a_freq as i128 - self.prev_vfo_a_for_ctun as i128).unsigned_abs() as u64;
        let jump_b = (self.vfo_b_freq as i128 - self.prev_vfo_b_for_ctun as i128).unsigned_abs() as u64;
        let band_switch = (bw_rx1 > 0 && jump_a > bw_rx1 && self.prev_vfo_a_for_ctun > 0)
            || (bw_rx2 > 0 && jump_b > bw_rx2 && self.prev_vfo_b_for_ctun > 0);
        if band_switch {
            debug!(
                "TCI: ctun band-switch detected (vfo_a jump={} > bw_rx1={} OR vfo_b jump={} > bw_rx2={}) - reforcing CTUN-AAN",
                jump_a, bw_rx1, jump_b, bw_rx2
            );
            self.force_ctun_aan_if_capable().await;
        }
        self.prev_vfo_a_for_ctun = self.vfo_a_freq;
        self.prev_vfo_b_for_ctun = self.vfo_b_freq;
    }

    /// PTT-off transition detectie: trigger forceert eval.
    /// Detecteer combined `tx_active OR thetis_tx_active` om óók externe-PTT-off
    /// (footswitch, VOX, Thetis manual TX) door te laten.
    /// Returns true als PTT-off detectie zojuist plaatsvond.
    pub fn detect_ptt_off_transition(&mut self) -> bool {
        let any_tx_now = self.tx_active || self.thetis_tx_active;
        let was_tx = self.prev_tx_active_for_ctun;
        self.prev_tx_active_for_ctun = any_tx_now;
        was_tx && !any_tx_now
    }

    /// Process a single TCI notification
    async fn handle_notification(&mut self, notif: TciNotification) {
        match notif {
            TciNotification::TciCapsEx { caps } => {
                let had_caps = !self.server_caps.is_empty();
                self.server_caps = caps.into_iter().collect();
                let has_caps = !self.server_caps.is_empty();
                info!("TCI: server caps = {:?}", self.server_caps);
                // Caps dropped to empty (extensions toggled OFF in Thetis mid-
                // session): reset state-machines that piggy-back on the cap
                // surface so the next toggle ON starts clean.
                if had_caps && !has_caps {
                    info!("TCI: ThetisLink extensions disabled - releasing _ex feature surface");
                    self.ctun_recenter.fork_active_logged = false;
                    // Without the fork pushing s9_frequency_ex we have no
                    // way to know the user's setting, so fall back to the
                    // IARU Region 1 convention (S9 = -93 dBm above 50 MHz).
                    self.s9_frequency_hz = 50_000_000;
                }
                // Claim auto-recenter ownership when caps transition from
                // empty -> non-empty (initial connect, or toggle ON after a
                // previous OFF). Skipping when caps were already populated
                // avoids re-spamming a claim on every refresh frame.
                if !had_caps && has_caps && self.has_cap("auto_recenter_owner_ex") {
                    info!("TCI: claiming auto-recenter ownership");
                    self.send("auto_recenter_owner_ex:true;").await;
                }
            }
            TciNotification::Ready => {
                info!("TCI: READY received, sending init commands");
                self.power_on = true;
                self.send_init_commands().await;
                // TL2-1 ctun-auto-recenter: forceer CTUN-AAN bij init wanneer cap aanwezig.
                // Idempotent - niet schadelijk om ook te runnen wanneer cap absent.
                self.force_ctun_aan_if_capable().await;
            }
            TciNotification::Vfo { receiver, channel, freq } => {
                match (receiver, channel) {
                    (0, 0) => {
                        if freq != self.vfo_a_freq {
                            log::debug!("TCI VFO A: {} Hz", freq);
                            self.vfo_a_freq = freq;
                            // VFO-sync optimistic mirror: when Thetis has VFO sync ON,
                            // the matching `vfo:0,1` push for the sync'd RX may arrive
                            // 100-1000 ms later than `vfo:0,0`. Mirror immediately so
                            // the periodic broadcast (10 Hz) sends both VFOs to the
                            // client at the same tick - avoids "synced VFO marker lags
                            // behind active VFO marker on the client". When Thetis's
                            // delayed push eventually arrives with the same value the
                            // equality check below silently absorbs it.
                            if self.vfo_sync_on && freq != self.vfo_b_freq {
                                log::debug!("TCI VFO A->B mirror (sync ON): {} Hz", freq);
                                self.vfo_b_freq = freq;
                            }
                            // TL2-1 ctun-auto-recenter: bij VFO-event eerst CTUN-aan
                            // garanderen (per operator-keuze 2026-05-08); daarna band-switch
                            // detect en trigger-eval. Dit lost het edge-geval op waar
                            // operator CTUN-uit klikt en daarna gaat tunen - feature blijft
                            // werken zonder dat operator naar de vink-uit-checkbox hoeft.
                            self.ensure_ctun_aan_for_rx(0).await;
                            self.detect_band_switch_and_reforce().await;
                            let z = self.effective_zoom_rx1_cache;
                            self.trigger_eval_and_act_rx1(z).await;
                        }
                    }
                    (0, 1) | (1, 0) => {
                        if freq != self.vfo_b_freq {
                            log::debug!("TCI VFO B: {} Hz", freq);
                            self.vfo_b_freq = freq;
                            // VFO-sync optimistic mirror, symmetric to the VFO-A path.
                            if self.vfo_sync_on && freq != self.vfo_a_freq {
                                log::debug!("TCI VFO B->A mirror (sync ON): {} Hz", freq);
                                self.vfo_a_freq = freq;
                            }
                            // Idem RX2: ensure CTUN aan, daarna band-switch + trigger-eval.
                            self.ensure_ctun_aan_for_rx(1).await;
                            self.detect_band_switch_and_reforce().await;
                            let z = self.effective_zoom_rx2_cache;
                            self.trigger_eval_and_act_rx2(z).await;
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
                // RX-only Amplitec gate: server-initiated PTT-paden zijn al
                // preventief afgesloten in `PttController`, maar Thetis-eigen
                // PTT (spatiebalk, footswitch, hardware-PTT op de radio)
                // bereikt Thetis zonder dat de server het ziet aankomen. Hier
                // pakken we de incoming TRX-notif en sturen direct
                // `TRX:0,false;` terug - sub-100 ms reactietijd vergeleken
                // met de ~200 ms broadcast-tick check.
                let blocked = self.tx_blocked.load(Ordering::Relaxed);
                debug!("TCI TRX recv: active={} tx_blocked={}", active, blocked);
                if active && blocked {
                    warn!("Thetis-direct TX detected on RX-only Amplitec position; forcing TRX off");
                    self.send("TRX:0,false;").await;
                    // Niet self.thetis_tx_active updaten - Thetis stuurt
                    // direct hierna een TRX:0,false; notif waar deze handler
                    // alsnog op reageert.
                    return;
                }
                // TL2-1 ctun-auto-recenter: mirror TRX-state in separate
                // `thetis_tx_active` so external-PTT (Thetis-self, footswitch,
                // VOX, etc.) ook ctun-recenter defer veroorzaakt.
                if active != self.thetis_tx_active {
                    self.thetis_tx_active = active;
                }
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
            TciNotification::S9FrequencyEx { mhz } => {
                let new_hz = (mhz * 1_000_000.0).round() as u64;
                if new_hz != self.s9_frequency_hz {
                    info!("TCI: S9 frequency threshold = {:.3} MHz (was {:.3} MHz)",
                          mhz, self.s9_frequency_hz as f64 / 1_000_000.0);
                    self.s9_frequency_hz = new_hz;
                }
            }
            TciNotification::RxChannelSensors { receiver, channel: _, dbm, dbm_sig, dbm_peakbin } => {
                if HAS_SENSORS_EX.load(std::sync::atomic::Ordering::Relaxed) {
                    // _ex format: Thetis provides all three sources per tick
                    // (Sig=peak-hold, Avg=true-mean, MaxBin=peak FFT bin) so
                    // each client can subscribe via SmeterSources. Cache all
                    // three; the broadcast loop in network.rs picks the right
                    // one per client.
                    let idx = (receiver as usize).min(1);
                    self.smeter_raw_dbm[idx] = Some(dbm);
                    if receiver == 0 {
                        self.smeter_direct_dbm = Some(dbm);
                        self.smeter_sig_dbm = dbm_sig;
                        self.smeter_peakbin_dbm = dbm_peakbin;
                    } else {
                        self.smeter_rx2_direct_dbm = Some(dbm);
                        self.smeter_rx2_sig_dbm = dbm_sig;
                        self.smeter_rx2_peakbin_dbm = dbm_peakbin;
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
                // Master volume - not used for RX AF gain sync
            }
            TciNotification::RxVolume { receiver, db, .. } => {
                // Map dB to 0..100% (Thetis sends -60.0..0.0 dB range)
                let pct = (((db + 60.0) * 100.0 / 60.0).clamp(0.0, 100.0)) as u8;
                if receiver == 0 {
                    self.rx_af_gain = pct;
                } else if receiver == 1 {
                    self.rx2_af_gain = pct;
                }
                debug!("TCI: RX{} volume = {:.1} dB ({}%)", receiver + 1, db, pct);
            }
            TciNotification::DiversityAutonullProgress { round, total, phase, gain_db, smeter } => {
                debug!("TCI: Auto-null progress {}/{}: phase={:.1}° gain={:.1}dB smeter={:.1}dBm", round, total, phase, gain_db, smeter);
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
                debug!("TCI: RX{} AGC Auto = {}", receiver + 1, enabled);
            }
            TciNotification::RxAnfEnable { receiver, enabled } => {
                if receiver == 0 { self.anf_on = enabled; }
                else { self.rx2_anf_on = enabled; }
                debug!("TCI: RX{} ANF = {}", receiver + 1, enabled);
            }
            TciNotification::RxNrEnable { receiver, enabled, level } => {
                let nr = if enabled { level.max(1) } else { 0 };
                if receiver == 0 { self.nr_level = nr; }
                else { self.rx2_nr_level = nr; }
                debug!("TCI: RX{} NR = {} (level {})", receiver + 1, enabled, nr);
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
                    debug!("TCI: MON {}", if enabled { "ON" } else { "OFF" });
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
                // guard blijft client-side `nb_level` op bv. 2 hangen -> NB-knop cycle't niet
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
                debug!("TCI notify: vfo_lock A = {}", enabled);
                self.vfo_lock = enabled;
            }
            TciNotification::VfoLockB { enabled } => {
                debug!("TCI notify: vfo_lock B = {}", enabled);
                self.rx2_vfo_lock = enabled;
            }
            TciNotification::BinEnable { receiver: 0, enabled } => { self.binaural = enabled; }
            TciNotification::BinEnable { receiver: 1, enabled } => { self.rx2_binaural = enabled; }
            TciNotification::BinEnable { .. } => {}
            TciNotification::ApfEnable { receiver: 0, enabled } => {
                debug!("TCI notify: apf_enable rx1 = {}", enabled);
                self.apf_enable = enabled;
            }
            TciNotification::ApfEnable { receiver: 1, enabled } => {
                debug!("TCI notify: apf_enable rx2 = {}", enabled);
                self.rx2_apf_enable = enabled;
            }
            TciNotification::ApfEnable { .. } => {}
            TciNotification::Mute { enabled } => {
                debug!("TCI notify: mute = {}", enabled);
                self.mute = enabled;
            }
            TciNotification::RxMute { receiver: 0, enabled } => {
                debug!("TCI notify: rx_mute rx1 = {}", enabled);
                self.rx_mute = enabled;
            }
            TciNotification::RxMute { .. } => {}
            TciNotification::NfEnable { receiver: 0, enabled } => {
                debug!("TCI notify: nf_enable rx1 = {}", enabled);
                self.nf_enable = enabled;
            }
            TciNotification::NfEnable { receiver: 1, enabled } => {
                debug!("TCI notify: nf_enable rx2 = {}", enabled);
                self.rx2_nf_enable = enabled;
            }
            TciNotification::NfEnable { .. } => {}
            TciNotification::RxBalance { receiver: 0, channel: 0, value } => {
                debug!("TCI notify: rx_balance = {}", value);
                self.rx_balance = value.clamp(-40, 40) as i8;
            }
            TciNotification::RxBalance { .. } => {}
            TciNotification::Tune { receiver: 0, active } => {
                if active != self.tune_active {
                    debug!("TCI: TUNE {}", if active { "ON" } else { "OFF" });
                    self.tune_active = active;
                }
            }
            TciNotification::Tune { .. } => {}
            TciNotification::TuneDrive { receiver: 0, power } => {
                if power != self.tune_drive {
                    debug!("TCI: Tune drive = {}%", power);
                    self.tune_drive = power;
                }
            }
            TciNotification::TuneDrive { .. } => {}
            TciNotification::MonVolume { db } => {
                if db != self.mon_volume {
                    debug!("TCI: Mon volume = {} dB", db);
                    self.mon_volume = db;
                }
            }
            TciNotification::CalibrationEx { receiver, meter_cal, xvtr_gain, six_m_gain } => {
                let idx = (receiver as usize).min(1);
                let changed = (meter_cal - self.meter_cal_offset[idx]).abs() > 0.01
                    || (xvtr_gain - self.xvtr_gain_offset[idx]).abs() > 0.01
                    || (six_m_gain - self.six_m_gain_offset[idx]).abs() > 0.01;
                if changed {
                    debug!("TCI: calibration_ex rx{} meter_cal={:.2} xvtr={:.1} 6m={:.1}",
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
                    debug!("TCI: TX profile index recalculated: \"{}\" = {}", self.tx_profile_name, idx);
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
                // Per-RX cache update - gebruikt door ctun-auto-recenter om bij VFO-event
                // te checken of CTUN AAN is, zo niet eerst forceren.
                match receiver {
                    0 => self.ctun = enabled,
                    1 => self.ctun_rx2 = enabled,
                    _ => {}
                }
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
            // Stock v2.10.3.14+: iq_samplerate is the global IQ stream / DDC sample rate (Hz).
            // Primary source for DDC rate state. Both RX1 and RX2 receive the same value
            // because stock TCI exposes one global rate (DDC protocol 2 hardware does support
            // per-RX rates - that comes back via TL2-x fork extensions in Phase 3).
            TciNotification::IqSamplerate { rate } => {
                // Defensief: stock .14/.15 stuurt nooit rate=0, maar future-Thetis
                // builds of corrupte push zouden state op 0 zetten en client-UI
                // (`if rate > 0`) zou de DDC-rate-knop verbergen. Skip 0.
                if rate == 0 {
                    log::debug!("TCI: iq_samplerate = 0 Hz, skipped (preserving last known rate)");
                    return;
                }
                if self.ddc_sample_rate_rx1 != rate || self.ddc_sample_rate_rx2 != rate {
                    info!("TCI: iq_samplerate = {} Hz", rate);
                }
                self.ddc_sample_rate_rx1 = rate;
                self.ddc_sample_rate_rx2 = rate;
            }
            // Stock v2.10.3.14+: if_limits gives the IF range; sample rate ≈ high - low.
            // Used as FALLBACK only - applied when iq_samplerate has not yet populated state.
            TciNotification::IfLimits { low, high } => {
                let derived = high.saturating_sub(low).max(0) as u32;
                if self.ddc_sample_rate_rx1 == 0 && derived > 0 {
                    info!("TCI: if_limits fallback -> ddc_sample_rate = {} Hz (low={}, high={})",
                        derived, low, high);
                    self.ddc_sample_rate_rx1 = derived;
                    self.ddc_sample_rate_rx2 = derived;
                } else {
                    log::debug!("TCI: if_limits = {} .. {} Hz (iq_samplerate already set)", low, high);
                }
            }
            TciNotification::RxStepAttEx { receiver, db } => {
                let db = db as i32;
                if receiver == 0 { self.step_att_rx1 = db; }
                else { self.step_att_rx2 = db; }
                debug!("TCI: rx_step_att_ex RX{} = {} dB", receiver + 1, db);
            }
            TciNotification::RxStepAttEnabledEx { receiver, enabled } => {
                if receiver == 0 { self.step_att_enabled_rx1 = enabled; }
                else { self.step_att_enabled_rx2 = enabled; }
                debug!("TCI: rx_step_att_enabled_ex RX{} = {}", receiver + 1, enabled);
            }
            TciNotification::RxPreampAttEx { receiver, db } => {
                if receiver == 0 { self.preamp_att_rx1 = db; }
                else { self.preamp_att_rx2 = db; }
                debug!("TCI: rx_preamp_att_ex RX{} = {} dB (signed)", receiver + 1, db);
            }
            TciNotification::TxFilterBandEx { low, high } => {
                self.tx_filter_low_hz = low;
                self.tx_filter_high_hz = high;
                self.tx_filter_known = true;
                log::debug!("TCI: tx_filter_band_ex = {} .. {} Hz", low, high);
            }
            TciNotification::TxFrequencyEx { freq, band, rx2_enabled, tx_vfob } => {
                log::debug!("TCI: tx_frequency_ex = {} Hz (band={}, rx2={}, tx_vfob={})",
                    freq, band, rx2_enabled, tx_vfob);
            }
            TciNotification::RunCatExResponse { cat_cmd, response } => {
                let cmd_upper = cat_cmd.to_uppercase();
                let resp_upper = response.to_uppercase();
                // Filter preset index responses (ZZFI##/ZZFJ##) replace the aux CAT poll.
                if cmd_upper.starts_with("ZZFI") {
                    if let Some(idx_str) = resp_upper.strip_prefix("ZZFI") {
                        if let Ok(idx) = idx_str.parse::<u8>() {
                            if idx != self.filter_index {
                                info!("TCI: filter preset RX1 = {} (was {})", idx, self.filter_index);
                                self.filter_index = idx;
                            }
                        }
                    }
                } else if cmd_upper.starts_with("ZZFJ") {
                    if let Some(idx_str) = resp_upper.strip_prefix("ZZFJ") {
                        if let Ok(idx) = idx_str.parse::<u8>() {
                            if idx != self.filter_rx2_index {
                                info!("TCI: filter preset RX2 = {} (was {})", idx, self.filter_rx2_index);
                                self.filter_rx2_index = idx;
                            }
                        }
                    }
                } else if cmd_upper.starts_with("ZZET") {
                    // TX-EQ (ZZET) query/echo: response is "ZZET1"/"ZZET0".
                    // Cache the state; if a WAV-playback bypass is pending,
                    // snapshot the operator's real setting and then turn
                    // TX-EQ off so we can restore it exactly afterwards.
                    if let Some(c) = resp_upper.strip_prefix("ZZET").and_then(|s| s.chars().next()) {
                        let on = c == '1';
                        self.thetis_txeq = on;
                        if self.txeq_bypass_pending {
                            self.txeq_bypass_saved = Some(on);
                            self.txeq_bypass_pending = false;
                            self.txeq_bypass_active = true;
                            self.run_cat("ZZET0").await;
                            info!("Thetis TXEQ bypass: saved prior = {}, TX-EQ off", if on { "on" } else { "off" });
                        }
                    }
                } else {
                    log::debug!("TCI: run_cat_ex({}) -> {}", cat_cmd, response);
                }
            }
            TciNotification::DiversityEnableEx { enabled } => {
                self.diversity_enabled = enabled;
                info!("TCI: Diversity = {}", enabled);
            }
            TciNotification::RxOnlyEx { rx_only } => {
                // Dedup: TL2-3 fork pushes on every transition AND the SET-handler
                // still echoes inline (handleRxOnlyEx, TCIServer.cs ~2867), so the
                // TL-server can see two notifies for the same value on a SET. Log
                // only when the value actually changes; the broadcast vs handler-
                // echo race remains harmless because both carry identical state.
                if self.rx_only != rx_only {
                    self.rx_only = rx_only;
                    info!("TCI: RXOnly = {}", rx_only);
                }
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
                // Thetis pushes this on every diversity tick (~10-20 Hz),
                // not just on change. Only log INFO on a real edge to keep
                // the log usable; intermediate same-value pushes go to
                // debug (suppressed at the default log level).
                let changed = if receiver == 0 {
                    let prev = self.diversity_gain_rx1;
                    self.diversity_gain_rx1 = gain;
                    prev != gain
                } else {
                    let prev = self.diversity_gain_rx2;
                    self.diversity_gain_rx2 = gain;
                    prev != gain
                };
                if changed {
                    info!("TCI: Diversity gain RX{} = {}", receiver + 1, gain);
                } else {
                    log::debug!("TCI: Diversity gain RX{} = {} (unchanged)", receiver + 1, gain);
                }
            }
            TciNotification::DiversityGainMultiEx { multi } => {
                let prev = self.diversity_gain_multi;
                self.diversity_gain_multi = multi;
                if prev != multi {
                    info!("TCI: Diversity GainMulti = {} (= {:.2}x)", multi, (multi as f32) / 100.0);
                } else {
                    log::debug!("TCI: Diversity GainMulti = {} (unchanged)", multi);
                }
            }
            TciNotification::DdcSampleRateEx { receiver, rate_hz } => {
                // TL2-1 fork per-RX echo. Overrides whatever the global iq_samplerate
                // (which sets both rx fields equal) just wrote, so we end up with the
                // actual per-RX state.
                if receiver == 0 { self.ddc_sample_rate_rx1 = rate_hz; }
                else { self.ddc_sample_rate_rx2 = rate_hz; }
                info!("TCI: ddc_sample_rate_ex RX{} = {} Hz", receiver + 1, rate_hz);
            }
            TciNotification::DiversityPhaseEx { phase } => {
                let prev = self.diversity_phase;
                self.diversity_phase = phase;
                if prev != phase {
                    info!("TCI: Diversity phase = {}", phase);
                } else {
                    log::debug!("TCI: Diversity phase = {} (unchanged)", phase);
                }
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
    /// VOLUME is NOT set - user retains their own Thetis volume setting.
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
        // Stock .14/.15 supports tx_profiles_ex without advertising the cap. Query is
        // harmless in fork-mode (Thetis re-pushes the same names list).
        self.send("tx_profiles_ex;").await;
        if self.has_cap("calibration_ex") || self.has_extensions() {
            self.send("calibration_ex:0;").await;
            self.send("calibration_ex:1;").await;
        }
        // Stock .14/.15 supports these _ex queries without advertising the caps.
        // Pushes echo back via rx_ctun_ex / agc_auto_ex / vfo_sync_ex notifications.
        self.send("rx_ctun_ex:0;").await;
        self.send("rx_ctun_ex:1;").await;
        self.send("agc_auto_ex;").await;
        self.send("vfo_sync_ex;").await;
        // Read the current TX modulation filter band at connect so the desktop
        // "Follow RX bandwidth" feature is available immediately. Without this
        // the server only learns the band when the operator first changes it in
        // Thetis (tx_filter_known stays false -> no push -> follow disabled until
        // a server restart happened to catch a push). Stock v2.10.3.14+; echoes
        // back via the tx_filter_band_ex notification.
        self.send("tx_filter_band_ex;").await;
        // Sentinel cap: enable_ex is canonical "diversity-suite present" indicator on the
        // TL2-fork (advertises per-command caps; presence of enable implies the rest).
        if self.has_cap("diversity_enable_ex") {
            self.send("diversity_enable_ex;").await;
            self.send("diversity_ref_ex;").await;
            self.send("diversity_source_ex;").await;
            self.send("diversity_gain_ex:0;").await;
            self.send("diversity_gain_ex:1;").await;
            self.send("diversity_phase_ex;").await;
        }
        if self.has_cap("diversity_gain_multi_ex") {
            self.send("diversity_gain_multi_ex;").await;
        }
        // Read Thetis' current RXOnly state so the TL-server knows the
        // pre-takeover value before it ever sets RX-only for an antenna.
        if self.has_cap("rx_only_ex") {
            self.send("rx_only_ex;").await;
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

    /// Sentinel "no signal" dBm value sent when the windowed-mW source has no
    /// samples yet. Lies well below S0 (-127 dBm), so the client clamps it to
    /// the bottom of the arc with no risk of looking like a real reading.
    const SMETER_NO_DATA_DBM: f32 = -200.0;

    fn avg_mw_to_dbm(&self, window: &VecDeque<f32>, freq_hz: u64) -> f32 {
        if window.is_empty() {
            return Self::SMETER_NO_DATA_DBM;
        }
        let sum: f32 = window.iter().sum();
        let avg_mw = sum / window.len() as f32;
        let avg_dbm = 10.0 * avg_mw.log10();
        self.apply_s9_band_shift(avg_dbm, freq_hz)
    }

    /// Thetis "above-S9-frequency" convention:
    ///  - VFO < `s9_frequency_hz`: S9 = -73 dBm (HF scale, no shift)
    ///  - VFO ≥ `s9_frequency_hz`: S9 = -93 dBm (VHF/UHF - add 20 dB to dBm)
    /// The threshold is fork-pushed via `s9_frequency_ex` (user-configurable
    /// in Thetis Setup, Thetis default 30 MHz). When the fork isn't present
    /// the field defaults to 50 MHz (IARU Region 1) so stock Thetis behaves
    /// like a typical ham-rig out of the box.
    fn apply_s9_band_shift(&self, dbm: f32, freq_hz: u64) -> f32 {
        if freq_hz >= self.s9_frequency_hz { dbm + 20.0 } else { dbm }
    }

    pub fn smeter_avg(&self) -> f32 {
        // TCI sensor payload is already calibrated by Thetis before emission
        // (meter_cal + xvtr + 6m gain + preamp are baked into the pushed dBm),
        // so we pass the value straight through. Applying a second offset
        // would double-count and shift the S-meter ~1-2 S-units high.
        if let Some(dbm) = self.smeter_direct_dbm {
            self.apply_s9_band_shift(dbm, self.vfo_a_freq)
        } else {
            self.avg_mw_to_dbm(&self.smeter_window, self.vfo_a_freq)
        }
    }

    pub fn smeter_rx2_avg(&self) -> f32 {
        if let Some(dbm) = self.smeter_rx2_direct_dbm {
            self.apply_s9_band_shift(dbm, self.vfo_b_freq)
        } else {
            self.avg_mw_to_dbm(&self.smeter_rx2_window, self.vfo_b_freq)
        }
    }

    /// RX1 Sig source - peak-hold detector (~100 ms decay).
    /// Returns Avg as fallback when Thetis hasn't pushed the _ex format yet.
    pub fn smeter_sig(&self) -> f32 {
        if let Some(dbm) = self.smeter_sig_dbm {
            self.apply_s9_band_shift(dbm, self.vfo_a_freq)
        } else {
            self.smeter_avg()
        }
    }

    /// RX1 MaxBin source - highest single FFT bin in passband.
    pub fn smeter_peakbin(&self) -> f32 {
        if let Some(dbm) = self.smeter_peakbin_dbm {
            self.apply_s9_band_shift(dbm, self.vfo_a_freq)
        } else {
            self.smeter_avg()
        }
    }

    pub fn smeter_rx2_sig(&self) -> f32 {
        if let Some(dbm) = self.smeter_rx2_sig_dbm {
            self.apply_s9_band_shift(dbm, self.vfo_b_freq)
        } else {
            self.smeter_rx2_avg()
        }
    }

    pub fn smeter_rx2_peakbin(&self) -> f32 {
        if let Some(dbm) = self.smeter_rx2_peakbin_dbm {
            self.apply_s9_band_shift(dbm, self.vfo_b_freq)
        } else {
            self.smeter_rx2_avg()
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
            self.smeter_sig_dbm = None;
            self.smeter_peakbin_dbm = None;
            self.smeter_rx2_direct_dbm = None;
            self.smeter_rx2_sig_dbm = None;
            self.smeter_rx2_peakbin_dbm = None;
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
                    // run_cat_ex payload contains embedded `;` (wrapped CAT command + response).
                    // Splitting on `;` would truncate it, so handle as single command first.
                    if line.to_lowercase().starts_with("run_cat_ex:") {
                        if let Some(notif) = parse_tci_text(line) {
                            let _ = notify_tx.try_send(notif);
                        }
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
