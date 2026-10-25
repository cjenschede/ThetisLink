// SPDX-License-Identifier: GPL-2.0-or-later

#![allow(dead_code)]

use std::collections::{HashSet, VecDeque};
use sdr_remote_core::dev_info;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use futures_util::{SinkExt, StreamExt};
use log::{debug, info, warn};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::tci_parser::*;

/// Why a frame could not go to its loop (build 151). Both lose the frame, but
/// "queue full" is a loop that fell behind and "gone" is a loop that is not
/// running at all - the first is a load question, the second a fault, and the
/// line used to call both "queue full" (review of build 150).
fn drop_reason<T>(e: &mpsc::error::TrySendError<T>) -> &'static str {
    match e {
        mpsc::error::TrySendError::Full(_) => "queue full",
        mpsc::error::TrySendError::Closed(_) => "the receive loop is gone",
    }
}

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
    /// The mode last asked of Thetis for VFO A, and when (build 244): a filter
    /// right after a mode change is checked against it (`mode_for_filter`).
    pub asked_mode_a: Option<(u8, Instant)>,
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
    /// Per-RX CTUN state cache, filled via TCI `rx_ctun_ex` push notifications.
    /// `ctun` (above) is for RX1 (legacy field name); `ctun_rx2` for RX2.
    /// TL2-1 ctun-auto-recenter uses these to check on a VFO-event whether CTUN is ON
    /// - if not, force ZZCN1/ZZCO1 first (per operator choice 2026-05-08).
    pub ctun_rx2: bool,
    /// DDS center frequency per receiver (from TCI DDS notification)
    pub dds_freq: [u64; 2],
    // RX2 / VFO-B state
    pub vfo_b_freq: u64,
    pub vfo_b_mode: u8,
    /// As `asked_mode_a`, for VFO B.
    pub asked_mode_b: Option<(u8, Instant)>,
    pub rx2_af_gain: u8,
    pub smeter_rx2_window: VecDeque<f32>,
    /// RX2 Avg source dBm (mirror of smeter_direct_dbm for RX1).
    pub smeter_rx2_direct_dbm: Option<f32>,
    /// RX2 Sig source dBm.
    pub smeter_rx2_sig_dbm: Option<f32>,
    /// RX2 MaxBin source dBm.
    pub smeter_rx2_peakbin_dbm: Option<f32>,
    /// When this connection last got `rx_channel_sensors_ex`. While that is
    /// recent the plain `rx_channel_sensors` is ignored (its instant values
    /// would override the average); once it is not, the plain one counts
    /// again. Per connection and by time - until build 137 a process-wide
    /// flag, set once and never cleared: a Thetis with the extensions off,
    /// after one with them on, left the S-meter frozen.
    sensors_ex_at: Option<Instant>,
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
    /// A bypass that ended while TCI was down: the state to restore at the
    /// next READY (build 224, review of 223).
    txeq_restore_due: Option<bool>,
    /// READY on this connection sent `txeq_restore_due` and kept it for the
    /// answer to a question (build 226, review of 225): with the connection
    /// gone that answer cannot come, so the state is not said again.
    txeq_restore_sent: bool,
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
    /// TUNE on either receiver - RX1 or RX2/VFO-B TX. One flag, because every
    /// reader asks "is Thetis putting out a carrier", not "on which receiver"
    /// (review finding on build 62: only receiver 0 was kept).
    tune_active: bool,
    /// Per receiver, so a TUNE OFF on one does not clear a TUNE ON on the other.
    pub(crate) tune_rx: [bool; 2],
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
    /// What stands in the reader's TX ring, published by the ring itself
    /// (build 193); `None` while there is no reader.
    tx_ring_level: Option<Arc<AtomicUsize>>,
    /// How many times this server sent Thetis TRX off (build 196): the reader
    /// drops the requests that wait when it changes, at once - Thetis's own
    /// `trx:..,false` comes only after its whole switch to RX, and an answer
    /// in between went into Thetis's next MOX (review of 195, home test).
    tx_off_count: Arc<AtomicU64>,
    /// TX audio chunks lost at a full channel to the reader (build 105).
    tx_audio_dropped: u64,
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
    /// True as soon as Thetis has reported at least one tx_filter_band_ex ->
    /// TX-filter is adjustable (stock 2.10.3.14+ / fork). The client uses
    /// this (via the TxFilterBand push) to activate the TX-bandwidth UI.
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
    /// Last refresh of the DDC centre (`dds`). Thetis pushes that only on
    /// change, so a missed push leaves the stored centre wrong indefinitely.
    last_dds_poll: Instant,

    /// TL2-1 ctun-auto-recenter: per-RX trigger state-machines + flag-clear timers.
    /// Cap-gated via `auto_recenter_ex` (advertised by Thetis fork only when the
    /// extensions checkbox is on). Serialized via &mut self / single async task -
    /// no separate mutex needed.
    pub ctun_recenter: crate::ctun_recenter::CtunRecenterState,

    /// TL2-1 ctun-auto-recenter: track previous PTT state for off-edge detection
    /// (PTT-off forces eval). Mirror of self.tx_active.
    prev_tx_active_for_ctun: bool,

    /// TL2-1 ctun-auto-recenter: previous VFO per RX for band-switch heuristic
    /// (VFO-jump > DDC-bandwidth -> band-switch detected).
    prev_vfo_a_for_ctun: u64,
    prev_vfo_b_for_ctun: u64,

    /// TL2-1 ctun-auto-recenter: cached effective zoom (MIN-aggregation over all
    /// connected clients per RX). Network.rs updates these values on every
    /// client-zoom-change/connect/disconnect. tci.rs uses them in trigger-eval
    /// on vfo-events. None = no clients with spectrum_enabled for that RX.
    pub effective_zoom_rx1_cache: Option<f32>,
    pub effective_zoom_rx2_cache: Option<f32>,

    /// TL2-1 ctun-auto-recenter: Thetis-originated TX state from
    /// `TciNotification::Trx`. `self.tx_active` is set by PttController
    /// (TL-server PTT path). On Thetis-self, footswitch, etc. Thetis updates us via
    /// TRX-notif but PttController does not see that -> ctun trigger could start a
    /// recenter-burst during TX. This flag mirrors the TRX-event so the
    /// trigger-eval can choose `tx_active OR thetis_tx_active`.
    thetis_tx_active: bool,
    /// The receiver of Thetis' last "transmitting" report, so the RX-only loop
    /// can stop a TX that was already on there (build 220).
    pub thetis_tx_receiver: u32,
    /// TCI connections made so far; a new one gets the DX spots again (build 220).
    connections: u64,
    /// Thetis answered a `run_cat_ex` in this connection (stock v2.10.3.14+, or
    /// the fork), so CAT `ZZTI1` reaches it (build 221).
    pub answers_run_cat_ex: bool,
    /// The first TRX report per receiver on this connection (build 213): one
    /// that says "transmitting" while this server does not transmit is a PTT
    /// left behind by a connection that went away (`FirstReport`).
    first_trx: FirstReport,
    /// The same for TUNE (build 214): a tune carrier left behind.
    first_tune: FirstReport,
    /// Per receiver: does this server want TUNE on (`TuneIntent`, builds
    /// 214-215)? Recorded in `send`, the one way every TUNE goes out, before the
    /// connection is checked - so like `tx_active` it is our intent, also while
    /// TCI is gone.
    we_tune: TuneIntent,
    /// Shared "current Amplitec-A position is RX-only" flag (clone of the
    /// flag in `PttController::tx_blocked`). Updated by the broadcast-loop in
    /// `network.rs`; used in the TRX-notification handler to
    /// immediately stop Thetis-direct PTT (spacebar, hardware-PTT)
    /// - the server-initiated PTT paths do not catch that (they have their
    /// own preventive gate). Default: dummy Arc that is always `false`.
    tx_blocked: Arc<AtomicBool>,
}

/// Parsed TCI notifications from the reader task
// TciNotification enum defined in tci_parser.rs (imported via `use crate::tci_parser::*`)

/// How long the `_ex` sensor format counts as current after its last message.
/// Thetis sends it several times a second; two seconds without one means it
/// stopped.
pub(crate) const SENSORS_EX_FRESH: std::time::Duration = std::time::Duration::from_secs(2);

pub(crate) fn sensors_ex_fresh(at: Option<Instant>, now: Instant) -> bool {
    at.map_or(false, |t| now.saturating_duration_since(t) < SENSORS_EX_FRESH)
}

/// What a sensor message does - the four ways, in one place (review 318,
/// the decision the S-meter fault came from, until now inline).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SensorUse {
    /// An `_ex` message: cached; `newly` when the format was not current.
    Extended { newly: bool },
    /// A plain message while `_ex` is current: ignored.
    IgnorePlain,
    /// A plain message, `_ex` having stopped: its cached values are forgotten
    /// (once - the stamp is cleared with them) and the plain one counts.
    PlainAfterExtendedStopped,
    /// A plain message, no `_ex` ever on this connection.
    Plain,
}

pub(crate) fn sensor_use(is_ex: bool, ex_at: Option<Instant>, now: Instant) -> SensorUse {
    let fresh = sensors_ex_fresh(ex_at, now);
    if is_ex {
        SensorUse::Extended { newly: !fresh }
    } else if fresh {
        SensorUse::IgnorePlain
    } else if ex_at.is_some() {
        SensorUse::PlainAfterExtendedStopped
    } else {
        SensorUse::Plain
    }
}

/// What the end of a WAV-playback TX-EQ bypass sends: only the undo of this
/// server's own `ZZET0`, which went out once `ZZET` answered with the prior
/// state (build 223, review of 222).
fn txeq_bypass_restore(saved: Option<bool>) -> Option<&'static str> {
    saved.map(|on| if on { "ZZET1" } else { "ZZET0" })
}

/// How long the mode asked of Thetis counts for a filter check: Thetis reports a
/// mode change ~0.1 s after it is asked (field log 2026-10-17), and a band
/// change sends the mode and the filter right after each other (build 244).
pub const ASKED_MODE_HOLD: std::time::Duration = std::time::Duration::from_secs(2);

/// The mode a filter is checked against: the one just asked of Thetis while it is
/// fresh, else the one Thetis reports (build 244).
pub fn mode_for_filter(asked: Option<(u8, Instant)>, reported: u8, now: Instant) -> u8 {
    match asked {
        Some((mode, at)) if now.duration_since(at) < ASKED_MODE_HOLD => mode,
        _ => reported,
    }
}

#[cfg(test)]
mod filter_mode_tests {
    use super::{mode_for_filter, ASKED_MODE_HOLD};
    use std::time::{Duration, Instant};

    /// A filter is checked against the mode just asked of Thetis while it is fresh,
    /// else against the one Thetis reports; both filter arms do so, and send only
    /// what fits (build 244).
    #[test]
    fn a_filter_goes_to_thetis_only_if_it_fits_the_mode() {
        let now = Instant::now();
        assert_eq!(mode_for_filter(None, 1, now), 1, "nothing asked: the reported mode");
        assert_eq!(mode_for_filter(Some((0, now)), 1, now), 0, "just asked: before Thetis confirms it");
        let old = now - ASKED_MODE_HOLD - Duration::from_millis(1);
        assert_eq!(mode_for_filter(Some((0, old)), 1, now), 1, "asked long ago: the reported mode");

        let src = include_str!("network.rs");
        let body = &sdr_remote_core::source_text::production_code(src);
        for (asked, reported, set, line) in [
            ("ptt.tci.asked_mode_a", "ptt.vfo_a_mode()", "ptt.set_filter(low, high).await;",
             "info!(\"Client {} filter: {} .. {} Hz does not fit mode {} - not sent to Thetis\""),
            ("ptt.tci.asked_mode_b", "ptt.vfo_b_mode()", "ptt.set_rx2_filter(low, high).await;",
             "info!(\"Client {} RX2 filter: {} .. {} Hz does not fit mode {} - not sent to Thetis\""),
        ] {
            let check = format!("let mode = crate::tci::mode_for_filter({asked}, {reported}, Instant::now());\n                                    if sdr_remote_core::filter_fit::edges_fit_mode(mode, low, high) {{");
            assert_eq!(body.matches(check.as_str()).count(), 1, "{set}");
            assert_eq!(body.matches(set).count(), 1, "{set}: only behind the check");
            let c = body.find(check.as_str()).unwrap();
            assert!(body[c..].find(set).unwrap() < body[c..].find(line).unwrap(), "{set}: sent when it fits");
        }
        let cmds = include_str!("tci_commands.rs");
        assert_eq!(cmds.matches("self.asked_mode_a = Some((mode, Instant::now()));").count(), 1);
        assert_eq!(cmds.matches("self.asked_mode_b = Some((mode, Instant::now()));").count(), 1);
    }
}

#[cfg(test)]
mod sensors_ex_tests {
    use super::*;

    /// A Thetis that stops sending the _ex sensors (extensions off, or a stock
    /// Thetis after a fork one) gets its plain ones counted again.
    #[test]
    fn the_extended_sensors_count_only_while_they_keep_coming() {
        let t0 = Instant::now();
        assert!(!sensors_ex_fresh(None, t0), "never seen");
        assert!(sensors_ex_fresh(Some(t0), t0 + std::time::Duration::from_millis(1999)));
        assert!(!sensors_ex_fresh(Some(t0), t0 + SENSORS_EX_FRESH), "stopped: the plain ones count");
    }

    /// The window itself: Thetis sends several per second, and a frozen meter
    /// after the format stops is the fault this closes - not seconds of it
    /// (review 318: 60 s stayed green).
    #[test]
    fn the_extended_window_is_short() {
        assert!(SENSORS_EX_FRESH >= std::time::Duration::from_millis(500));
        assert!(SENSORS_EX_FRESH <= std::time::Duration::from_secs(3));
    }

    /// Review 318 (point C): a Thetis without the extensions after one with
    /// them, through a reconnect - the field rule of 137. The disconnect
    /// cleared the stamp but kept the extended values, and the getters prefer
    /// those: the meter stood on the old Thetis's last value until a PTT.
    #[test]
    fn a_thetis_that_drops_out_leaves_nothing_of_what_it_was_doing() {
        let mut t = TciConnection::new(None);
        // Every value forget_sensor_values clears, set to something, plus the
        // two flags that decide whether this station is transmitting. Written
        // out one by one because a hand-written list is exactly as complete as
        // the list, and five of these seven lines could be deleted with the
        // whole suite still green (review 2120A3).
        t.smeter_raw_dbm = [Some(-50.0), Some(-60.0)];
        t.smeter_direct_dbm = Some(-48.0);
        t.smeter_sig_dbm = Some(-47.0);
        t.smeter_peakbin_dbm = Some(-45.0);
        t.smeter_rx2_direct_dbm = Some(-44.0);
        t.smeter_rx2_sig_dbm = Some(-43.0);
        t.smeter_rx2_peakbin_dbm = Some(-42.0);
        t.thetis_tx_active = true;
        t.tune_active = true;

        t.handle_disconnect();

        assert_eq!(t.smeter_raw_dbm, [None, None], "the spectrum calibration of a Thetis that is gone");
        assert!(t.smeter_direct_dbm.is_none());
        assert!(t.smeter_sig_dbm.is_none());
        assert!(t.smeter_peakbin_dbm.is_none());
        assert!(t.smeter_rx2_direct_dbm.is_none());
        assert!(t.smeter_rx2_sig_dbm.is_none());
        assert!(t.smeter_rx2_peakbin_dbm.is_none());
        assert!(
            !t.thetis_tx_active && !t.tune_active,
            "a TX or TUNE that was on at the moment of the drop would hold the PA guard for good"
        );
        assert!(t.sensors_ex_at.is_none(), "and the extended window closed with it");
    }

    /// The receiver Thetis said it transmits on is kept for the RX-only loop,
    /// which stops a TX Thetis was already making there (build 220); and each
    /// connection is counted, so the DX spots go to a new Thetis again.
    #[test]
    fn thetis_tx_keeps_its_receiver_and_connections_are_counted() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let mut t = TciConnection::new(None);
        rt.block_on(t.handle_notification(TciNotification::Trx { receiver: 1, active: true }));
        assert!(t.thetis_tx_active);
        assert_eq!(t.thetis_tx_receiver, 1);
        rt.block_on(t.handle_notification(TciNotification::Trx { receiver: 0, active: false }));
        assert_eq!(t.thetis_tx_receiver, 1, "an off does not move it");
        rt.block_on(t.handle_notification(TciNotification::Trx { receiver: 0, active: true }));
        assert_eq!(t.thetis_tx_receiver, 0);
        assert_eq!(t.connection_count(), 0);
        // On a position that is already RX-only (review of 220): the TRX
        // arm answers the "on" and returns, but the receiver is kept, so the
        // loop's stop for a TUNE there goes to the same receiver.
        let blocked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        t.set_tx_blocked_handle(blocked);
        rt.block_on(t.handle_notification(TciNotification::Trx { receiver: 1, active: true }));
        rt.block_on(t.handle_notification(TciNotification::Tune { receiver: 1, active: true }));
        assert_eq!(rx_only_stop_commands(t.thetis_tx_receiver, t.tune_rx)[0], "TRX:1,false;");
        // Thetis answering a run_cat_ex is what makes CAT ZZTI1 a guard (build
        // 221); a new connection asks again.
        assert!(!t.answers_run_cat_ex);
        rt.block_on(t.handle_notification(TciNotification::RunCatExResponse {
            cat_cmd: "ZZFI".to_string(), response: "ZZFI03".to_string(),
        }));
        assert!(t.answers_run_cat_ex);
        t.handle_disconnect();
        assert!(!t.answers_run_cat_ex);
        // What goes out (build 220): `TRX:..,false` on the receiver Thetis named
        // - in Thetis that only drops a TCI PTT - and `tune:..,false` for every
        // receiver that reports TUNE, which does stop a TUNE whoever started it.
        assert_eq!(rx_only_stop_commands(1, [false, false]), vec!["TRX:1,false;".to_string()]);
        assert_eq!(
            rx_only_stop_commands(0, [false, true]),
            vec!["TRX:0,false;".to_string(), "tune:1,false;".to_string()]
        );
        assert_eq!(rx_only_stop_commands(0, [true, false])[1], "tune:0,false;");
        // And each one is sent, from where the loop calls it.
        assert_eq!(
            include_str!("tci.rs").matches("for cmd in rx_only_stop_commands(self.thetis_tx_receiver, self.tune_rx) {\n            self.send(&cmd).await;").count(),
            1
        );
        let src = include_str!("tci.rs");
        assert_eq!(src.matches("        self.connected = true;\n        self.connections += 1;\n").count(), 1);
    }

    /// The TX-EQ bypass of a WAV playback turns back on only what it turned
    /// off itself (build 223, review of 222): without an answer to `ZZET` no
    /// `ZZET0` went out, and the end said `ZZET1` anyway - TX-EQ on for an
    /// operator who had it off. An unanswered question goes with its
    /// connection; our own `ZZET0` is kept, so the end still undoes it.
    #[test]
    fn the_txeq_bypass_restores_only_what_it_turned_off() {
        assert_eq!(txeq_bypass_restore(None), None, "no answer: nothing was turned off");
        assert_eq!(txeq_bypass_restore(Some(true)), Some("ZZET1"));
        assert_eq!(txeq_bypass_restore(Some(false)), Some("ZZET0"));

        // With time: READY sends the init commands, which pause between batches.
        let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let mut t = TciConnection::new(None);
        rt.block_on(t.thetis_txeq_bypass_begin());
        assert!(t.txeq_bypass_pending);
        t.handle_disconnect();
        assert!(!t.txeq_bypass_pending, "the question went with its connection");
        // Twice, without a restart: the second begin asks again.
        for _ in 0..2 {
            rt.block_on(t.thetis_txeq_bypass_begin());
            rt.block_on(t.handle_notification(TciNotification::RunCatExResponse {
                cat_cmd: "ZZET".to_string(), response: "ZZET0".to_string(),
            }));
            assert!(t.txeq_bypass_active && t.txeq_bypass_saved == Some(false));
            t.handle_disconnect();
            assert!(t.txeq_bypass_active && t.txeq_bypass_saved == Some(false), "our own ZZET0 is kept");
            rt.block_on(t.thetis_txeq_bypass_end());
            assert!(!t.txeq_bypass_active && !t.txeq_bypass_pending && t.txeq_bypass_saved.is_none());
        }
        // What goes out, connected: the question, and at the end no answer
        // means no ZZET at all.
        let (tx, mut rx) = mpsc::channel::<String>(16);
        t.connected = true;
        t.cmd_tx = Some(tx);
        rt.block_on(t.thetis_txeq_bypass_begin());
        rt.block_on(t.thetis_txeq_bypass_end());
        let mut sent = Vec::new();
        while let Ok(c) = rx.try_recv() {
            sent.push(c);
        }
        assert_eq!(sent.iter().filter(|c| c.contains("ZZET")).count(), 1, "only the question: {sent:?}");
        assert!(!sent.iter().any(|c| c.contains("ZZET1") || c.contains("ZZET0")), "{sent:?}");

        // Build 224 (review of 223): a playback that ends while TCI is down
        // keeps what to restore, and the next connection restores it at READY
        // - once, also when a second READY comes. Twice, without a restart.
        for _ in 0..2 {
            let (tx, mut rx) = mpsc::channel::<String>(256);
            t.connected = true;
            t.cmd_tx = Some(tx);
            rt.block_on(t.thetis_txeq_bypass_begin());
            rt.block_on(t.handle_notification(TciNotification::RunCatExResponse {
                cat_cmd: "ZZET".to_string(), response: "ZZET1".to_string(),
            }));
            t.handle_disconnect();
            rt.block_on(t.thetis_txeq_bypass_end());
            assert_eq!(t.txeq_restore_due, Some(true), "nothing could go out");
            // A connection that drops again before READY keeps it.
            t.connected = true;
            t.handle_disconnect();
            assert_eq!(t.txeq_restore_due, Some(true), "kept until a READY");
            let (tx, mut rx2) = mpsc::channel::<String>(256);
            t.connected = true;
            t.cmd_tx = Some(tx);
            rt.block_on(t.handle_notification(TciNotification::Ready));
            rt.block_on(t.handle_notification(TciNotification::Ready));
            let mut sent = Vec::new();
            while let Ok(c) = rx2.try_recv() {
                sent.push(c);
            }
            assert_eq!(sent.iter().filter(|c| c.contains("ZZET1")).count(), 1, "{sent:?}");
            assert!(!sent.iter().any(|c| c.contains("ZZET0")), "{sent:?}");
            assert_eq!(t.txeq_restore_due, None);
            while rx.try_recv().is_ok() {}
            t.handle_disconnect();
        }
        // Connected at the end: restored at once, nothing kept.
        let (tx, _rx) = mpsc::channel::<String>(256);
        t.connected = true;
        t.cmd_tx = Some(tx);
        rt.block_on(t.thetis_txeq_bypass_begin());
        rt.block_on(t.handle_notification(TciNotification::RunCatExResponse {
            cat_cmd: "ZZET".to_string(), response: "ZZET0".to_string(),
        }));
        rt.block_on(t.thetis_txeq_bypass_end());
        assert_eq!(t.txeq_restore_due, None);
        let src = include_str!("tci.rs");
        // rfind: this test names the same line, above the function.
        let end = &src[src.rfind("pub async fn thetis_txeq_bypass_end(&mut self) {").unwrap()..];
        let end = &end[..end.find("\n    }\n").unwrap()];
        assert_eq!(end.matches("match txeq_bypass_restore(saved) {").count(), 1);
        assert!(!end.contains("unwrap_or"), "no default to on");
    }

    /// A restore that was due when a new playback starts (build 225, review
    /// of 224): Thetis still holds our own ZZET0, so its answer is not the
    /// operator's state - the restore that was due is. Three orders: the
    /// answer before READY, READY before the answer, and a question that went
    /// nowhere.
    #[test]
    fn a_restore_that_was_due_is_the_operators_state_for_the_next_playback() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let answer = |t: &mut TciConnection, rt: &tokio::runtime::Runtime, r: &str| {
            rt.block_on(t.handle_notification(TciNotification::RunCatExResponse {
                cat_cmd: "ZZET".to_string(), response: r.to_string(),
            }));
        };
        let zzet = |rx: &mut mpsc::Receiver<String>| {
            let mut v = Vec::new();
            while let Ok(c) = rx.try_recv() {
                if c.contains("ZZET1") || c.contains("ZZET0") {
                    v.push(if c.contains("ZZET1") { "ZZET1" } else { "ZZET0" });
                }
            }
            v
        };

        // The answer before READY.
        let mut t = TciConnection::new(None);
        let (tx, mut rx) = mpsc::channel::<String>(256);
        t.txeq_restore_due = Some(true);
        t.connected = true;
        t.cmd_tx = Some(tx);
        rt.block_on(t.thetis_txeq_bypass_begin());
        answer(&mut t, &rt, "ZZET0");
        assert_eq!(t.txeq_bypass_saved, Some(true), "the restore that was due, not our own ZZET0");
        assert_eq!(t.txeq_restore_due, None);
        rt.block_on(t.handle_notification(TciNotification::Ready));
        rt.block_on(t.thetis_txeq_bypass_end());
        assert_eq!(zzet(&mut rx), vec!["ZZET0", "ZZET1"], "off for the playback, on at its end");

        // READY before the answer.
        let mut t = TciConnection::new(None);
        let (tx, mut rx) = mpsc::channel::<String>(256);
        t.txeq_restore_due = Some(true);
        t.connected = true;
        t.cmd_tx = Some(tx);
        rt.block_on(t.thetis_txeq_bypass_begin());
        rt.block_on(t.handle_notification(TciNotification::Ready));
        answer(&mut t, &rt, "ZZET0");
        assert_eq!(t.txeq_bypass_saved, Some(true));
        assert_eq!(t.txeq_restore_due, None);
        rt.block_on(t.thetis_txeq_bypass_end());
        assert_eq!(zzet(&mut rx), vec!["ZZET1", "ZZET0", "ZZET1"], "restored at READY, off, on at the end");

        // A question that went nowhere (asked while TCI was down).
        let mut t = TciConnection::new(None);
        t.txeq_restore_due = Some(true);
        rt.block_on(t.thetis_txeq_bypass_begin());
        let (tx, mut rx) = mpsc::channel::<String>(256);
        t.connected = true;
        t.cmd_tx = Some(tx);
        rt.block_on(t.handle_notification(TciNotification::Ready));
        rt.block_on(t.thetis_txeq_bypass_end());
        assert_eq!(zzet(&mut rx), vec!["ZZET1"], "restored at READY and nothing after");
        assert_eq!(t.txeq_restore_due, None, "said once");
        rt.block_on(t.handle_notification(TciNotification::Ready));
        assert!(zzet(&mut rx).is_empty(), "not again at the next READY");
    }

    /// A restore READY already sent, kept for the answer to a question that
    /// went nowhere, falls with its connection (build 226, review of 225):
    /// after a second drop the next READY no longer says it, over whatever
    /// the operator chose in between. One not sent yet stays.
    #[test]
    fn a_restore_sent_at_ready_is_not_said_again_after_a_second_drop() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let zzet = |rx: &mut mpsc::Receiver<String>| {
            let mut v = Vec::new();
            while let Ok(c) = rx.try_recv() {
                if c.contains("ZZET1") || c.contains("ZZET0") {
                    v.push(if c.contains("ZZET1") { "ZZET1" } else { "ZZET0" });
                }
            }
            v
        };
        let connect = |t: &mut TciConnection| {
            let (tx, rx) = mpsc::channel::<String>(256);
            t.connected = true;
            t.cmd_tx = Some(tx);
            rx
        };

        // Sent at READY, then a drop: gone.
        let mut t = TciConnection::new(None);
        t.txeq_restore_due = Some(true);
        rt.block_on(t.thetis_txeq_bypass_begin());
        let mut rx = connect(&mut t);
        rt.block_on(t.handle_notification(TciNotification::Ready));
        assert_eq!(zzet(&mut rx), vec!["ZZET1"], "restored at READY");
        assert_eq!(t.txeq_restore_due, Some(true), "kept for the answer");
        t.handle_disconnect();
        assert_eq!(t.txeq_restore_due, None, "that answer cannot come any more");
        let mut rx = connect(&mut t);
        rt.block_on(t.handle_notification(TciNotification::Ready));
        assert!(zzet(&mut rx).is_empty(), "not said again over the operator");

        // Twice in a row: the drop says it once and the next READY nothing.
        t.handle_disconnect();
        assert_eq!(t.txeq_restore_due, None);

        // Not sent yet (a drop before READY): it stays for the next READY.
        let mut t = TciConnection::new(None);
        t.txeq_restore_due = Some(false);
        rt.block_on(t.thetis_txeq_bypass_begin());
        t.handle_disconnect();
        assert_eq!(t.txeq_restore_due, Some(false), "nothing went out yet");
        let mut rx = connect(&mut t);
        rt.block_on(t.handle_notification(TciNotification::Ready));
        assert_eq!(zzet(&mut rx), vec!["ZZET0"], "said at the first READY that came");
    }

    #[test]
    fn a_new_thetis_without_extensions_after_one_with_moves_the_meter() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let mut t = TciConnection::new(None);
        rt.block_on(t.handle_notification(TciNotification::RxChannelSensors {
            receiver: 0, channel: 0, dbm: -50.0, dbm_sig: Some(-48.0), dbm_peakbin: Some(-45.0),
        }));
        assert!(t.smeter_direct_dbm.is_some() && t.smeter_raw_dbm[0].is_some());
        t.handle_disconnect();
        rt.block_on(t.handle_notification(TciNotification::RxChannelSensors {
            receiver: 0, channel: 0, dbm: -100.0, dbm_sig: None, dbm_peakbin: None,
        }));
        assert!(t.smeter_direct_dbm.is_none(), "the old Thetis's value no longer stands in for the meter");
        assert!(t.smeter_raw_dbm[0].is_none(), "nor for the spectrum calibration");
        // And the new connection counts a plain message at once: the stamp went
        // with the disconnect (review 318 round 2).
        assert_eq!(t.smeter_window.len(), 1, "the plain message was not ignored");
    }

    #[test]
    fn a_sensor_message_does_one_of_four_things() {
        let t0 = Instant::now();
        let later = t0 + SENSORS_EX_FRESH;
        assert_eq!(sensor_use(true, None, t0), SensorUse::Extended { newly: true });
        assert_eq!(sensor_use(true, Some(t0), t0), SensorUse::Extended { newly: false });
        assert_eq!(sensor_use(false, Some(t0), t0), SensorUse::IgnorePlain);
        assert_eq!(sensor_use(false, Some(t0), later), SensorUse::PlainAfterExtendedStopped, "forget the cached values");
        assert_eq!(sensor_use(false, None, later), SensorUse::Plain, "once: the stamp went with them");
    }
}

impl TciConnection {
    /// Inject the shared TX-blocked flag from `PttController`. Used
    /// in the TRX-notification handler to immediately cancel Thetis-direct PTT
    /// on an RX-only position.
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
            asked_mode_a: None,
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
            first_trx: FirstReport::default(),
            first_tune: FirstReport::default(),
            we_tune: TuneIntent::default(),
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
            asked_mode_b: None,
            rx2_af_gain: 100,
            smeter_rx2_window: VecDeque::with_capacity(4),
            smeter_rx2_direct_dbm: None,
            smeter_rx2_sig_dbm: None,
            smeter_rx2_peakbin_dbm: None,
            sensors_ex_at: None,
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
            txeq_restore_due: None,
            txeq_restore_sent: false,
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
            tune_rx: [false; 2],
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
            tx_ring_level: None,
            tx_off_count: Arc::new(AtomicU64::new(0)),
            tx_audio_dropped: 0,
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
            last_dds_poll: Instant::now(),
            ctun_recenter: crate::ctun_recenter::CtunRecenterState::new(),
            prev_tx_active_for_ctun: false,
            prev_vfo_a_for_ctun: 0,
            prev_vfo_b_for_ctun: 0,
            effective_zoom_rx1_cache: None,
            effective_zoom_rx2_cache: None,
            thetis_tx_active: false,
            thetis_tx_receiver: 0,
            connections: 0,
            answers_run_cat_ex: false,
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
        self.connections += 1;

        tokio::spawn(tci_writer_task(ws_write, cmd_rx, bin_rx));

        let (tx_audio_tx, tx_audio_rx) = mpsc::channel::<Vec<f32>>(64);
        self.tx_audio_tx = Some(tx_audio_tx);
        let tx_ring_level = Arc::new(AtomicUsize::new(0));
        self.tx_ring_level = Some(tx_ring_level.clone());

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
            tx_ring_level,
            self.tx_off_count.clone(),
        ));

        info!("TCI: WebSocket connected to ws://{}", self.addr);
    }

    /// Send a TCI text command (e.g. "VFO:0,0,14200000;")
    pub async fn send(&mut self, cmd: &str) {
        if is_trx_off(cmd) {
            // Before it goes out: the reader drops what waits (build 196).
            self.tx_off_count.fetch_add(1, Ordering::Relaxed);
        }
        // Our own TUNE, whichever path sent it - and before the connection is
        // checked: a TUNE off said while TCI is gone is still what we want, and
        // left out it let a carrier stand after the reconnect (review of 214).
        self.we_tune.sent(cmd);
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
                && !cmd.starts_with("run_cat_ex:ZZFI") && !cmd.starts_with("run_cat_ex:ZZFJ")
                && !cmd.starts_with("dds:") {
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

    /// End the bypass: restore TX-EQ to the snapshotted prior state. Without
    /// one the query never answered and no `ZZET0` went out, so nothing is
    /// restored (build 223, review of 222: it said `ZZET1` then).
    pub async fn thetis_txeq_bypass_end(&mut self) {
        if !self.txeq_bypass_active && !self.txeq_bypass_pending {
            return;
        }
        let saved = self.txeq_bypass_saved;
        self.txeq_bypass_pending = false;
        self.txeq_bypass_active = false;
        self.txeq_bypass_saved = None;
        // Named in advance (`logregels-build223.md`).
        match txeq_bypass_restore(saved) {
            // Nothing can go out: kept for the next connection (build 224).
            Some(cmd) if !self.connected => {
                self.txeq_restore_due = saved;
                info!(
                    "Thetis TXEQ bypass end: TCI down, TX-EQ back to {} at the next connection",
                    if cmd == "ZZET1" { "on" } else { "off" }
                );
            }
            Some(cmd) => {
                self.run_cat(cmd).await;
                info!("Thetis TXEQ bypass end: restored to {}", if cmd == "ZZET1" { "on" } else { "off" });
            }
            None => {
                // A state kept at READY for this question's answer went out
                // there already (build 225).
                if self.connected {
                    self.txeq_restore_due = None;
                    self.txeq_restore_sent = false;
                }
                info!("Thetis TXEQ bypass end: no answer to ZZET, TX-EQ left as it is")
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
        let mut reader_gone = false;
        if let Some(ref mut rx) = self.notify_rx {
            loop {
                match rx.try_recv() {
                    Ok(notif) => notifications.push(notif),
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    // The reader task is gone, whatever it managed to say on
                    // its way out. This is the disconnect that does not depend
                    // on a message getting through (review finding on build 69: a stream
                    // that ends without a Close frame sent nothing at all).
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        reader_gone = true;
                        break;
                    }
                }
            }
        }
        for notif in notifications {
            self.handle_notification(notif).await;
        }
        // Only if the Disconnected notification in the same drain did not
        // already do it: handle_disconnect() clears `connected`, so this is the
        // "exactly once" a review asked for on build 71, not a second pass.
        if reader_gone && self.connected {
            warn!("TCI: reader task ended without a word - treating it as a disconnect");
            self.handle_disconnect();
        }

        // DDC centre refresh. `dds` is a push-on-change notification, so a push
        // missed at connect - or simply not sent in some states - leaves the
        // stored centre stale for as long as the session lasts. Nothing else
        // corrects it: with CTUN on the centre deliberately does not follow the
        // VFO, so the wrong value survives every retune. The VRX channelizer
        // picks its bins against that centre, which is heard as a channel that
        // is permanently distorted while the spectrum - drawn against the same
        // wrong centre - still looks healthy. Asking for it costs two short
        // commands and makes the state self-correcting.
        if self.power_on && self.last_dds_poll.elapsed() >= std::time::Duration::from_secs(5) {
            self.last_dds_poll = Instant::now();
            self.send("dds:0;").await;
            self.send("dds:1;").await;
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

        // TL2-1 ctun-auto-recenter: PTT-off transition forces eval.
        // Otherwise the user must press buttons to recenter after a long TX period with VFO out of zone.
        if self.detect_ptt_off_transition() {
            log::debug!("TCI: ctun PTT-off transition - forcing trigger-eval");
            let zr1 = self.effective_zoom_rx1_cache;
            let zr2 = self.effective_zoom_rx2_cache;
            self.trigger_eval_and_act_rx1(zr1).await;
            self.trigger_eval_and_act_rx2(zr2).await;
        }
    }

    // ===== TL2-1 ctun-auto-recenter: trigger evaluation + action =====

    /// Force CTUN-ON for RX1 + RX2 if the fork advertises the cap. Idempotent.
    /// Called at init (Ready) and on band-switch detection.
    pub async fn force_ctun_on_if_capable(&mut self) {
        if !self.has_cap("auto_recenter_ex") {
            return;
        }
        if !self.ctun_recenter.fork_active_logged {
            info!("TCI: ThetisLink ctun-extension active - Thetis recenter logic disabled (cap auto_recenter_ex)");
            self.ctun_recenter.fork_active_logged = true;
        }
        self.run_cat("ZZCN1").await;  // CTUN ON for RX1
        self.run_cat("ZZCO1").await;  // CTUN ON for RX2 (ZZCO, NOT ZZCP - ZZCP is compander!)
    }

    /// Lazy-ensure: on a VFO-event on a specific RX, check whether CTUN is ON.
    /// If not (operator manually turned CTUN off, or Thetis-init had it off):
    /// send ZZCN1 (RX1) or ZZCO1 (RX2) and update cache. Per operator choice 2026-05-08.
    /// Idempotent: when CTUN already ON -> no-op. Cap-gated.
    async fn ensure_ctun_on_for_rx(&mut self, rx_index: u8) {
        if !self.has_cap("auto_recenter_ex") {
            return;
        }
        match rx_index {
            0 if !self.ctun => {
                debug!("TCI: ctun rx=0 was off - forcing ON before VFO-event eval");
                self.run_cat("ZZCN1").await;
                self.ctun = true; // optimistic; rx_ctun_ex push confirms later
            }
            1 if !self.ctun_rx2 => {
                debug!("TCI: ctun rx=1 was off - forcing ON before VFO-event eval");
                self.run_cat("ZZCO1").await;
                self.ctun_rx2 = true;
            }
            _ => {}
        }
    }

    /// Trigger-eval + recenter-action for RX1. Called from network.rs after every
    /// vfo-event or zoom-change. `effective_zoom` is None with 0 spectrum-clients.
    /// Combine TL-server PTT with Thetis-originated TRX so that external PTT
    /// (footswitch, VOX, manual TX-button) also blocks recenter.
    pub async fn trigger_eval_and_act_rx1(&mut self, effective_zoom: Option<f32>) {
        let any_tx = self.transmitting_for_ctun();
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

    /// Trigger-eval + recenter-action for RX2.
    pub async fn trigger_eval_and_act_rx2(&mut self, effective_zoom: Option<f32>) {
        let any_tx = self.transmitting_for_ctun();
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

    /// Recenter-burst RX1: ZZCN0 -> 50ms -> ZZCN1. Sets per-RX flag for 200ms.
    /// Note: PTT-on during this burst completes within the flag-window (no
    /// abort-path). Inconsistent CTUN-state between ZZCN0 and ZZCN1 is riskier
    /// than a short ZZCN1-during-TX-glitch. `&mut self`-borrow + tokio::sleep
    /// guarantees that no other recenter-action can interrupt the burst.
    ///
    /// **VFO sync coalesce:** when Thetis-side VFO sync is ON, this
    /// burst also does the RX2-CTUN-toggle interleaved. Both CTUN-ON commands
    /// therefore fire <1 ms apart at Thetis, so both DDCs recenter on
    /// the same "current VFO" snapshot. Without coalesce the second RX-burst
    /// would only start 50 ms later and, on rapid tuning, land on a newer VFO,
    /// causing RX1 and RX2 DDC-centers to end up on different freqs.
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
    /// **Important:** RX2-CTUN uses **ZZCO**, not ZZCP. ZZCP = compander
    /// (audio); ZZCO = CTUN RX2.
    ///
    /// **VFO sync coalesce:** symmetric to `execute_recenter_rx1` - when
    /// VFO sync is ON this burst also does the RX1-CTUN-toggle interleaved.
    /// See the comment there for the motivation.
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

    /// Band-switch heuristic: VFO-jump > DDC-bandwidth -> operator has done a
    /// band-switch. Force CTUN-ON again (Thetis can remember CTUN-state per band).
    /// Called from network.rs after vfo-event-update.
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
                "TCI: ctun band-switch detected (vfo_a jump={} > bw_rx1={} OR vfo_b jump={} > bw_rx2={}) - forcing CTUN on again",
                jump_a, bw_rx1, jump_b, bw_rx2
            );
            self.force_ctun_on_if_capable().await;
        }
        self.prev_vfo_a_for_ctun = self.vfo_a_freq;
        self.prev_vfo_b_for_ctun = self.vfo_b_freq;
    }

    /// Is anything on the air that holds the automatic recenter: our own PTT,
    /// Thetis's TRX (a footswitch, VOX, its own button), or Tune - a carrier
    /// too (build 274, owner's decision). One place for the three checks that
    /// spelled it out, without Tune.
    pub fn transmitting_for_ctun(&self) -> bool {
        self.tx_active || self.thetis_on_the_air()
    }

    /// Both reasons apart, for the PA rule, which keeps them apart on purpose
    /// (`pa_policy::TxSources`, build 272). The fields are private (build
    /// 276): "on the air" is `thetis_on_the_air`, nowhere else.
    pub fn thetis_trx_and_tune(&self) -> (bool, bool) {
        (self.thetis_tx_active, self.tune_active)
    }

    /// Our own Tune on `receiver`, through the same merge as Thetis's
    /// notification, so a TUNE OFF for receiver 0 does not push receiver 1
    /// aside.
    pub(crate) fn tune_from_us(&mut self, receiver: usize, on: bool) {
        self.tune_active = merge_tune(&mut self.tune_rx, receiver, on);
    }

    /// Set what Thetis reports, for a test outside this file.
    #[cfg(test)]
    pub(crate) fn set_on_the_air_for_test(&mut self, trx: bool, tune: bool) {
        self.thetis_tx_active = trx;
        self.tune_active = tune;
    }

    /// Thetis is putting out a carrier: its TRX notification (MOX, a
    /// footswitch, VOX, ThetisLink) or Tune. The one definition (build 275,
    /// round 274): `PttController::tx_now` and `transmitting_for_ctun` ask it.
    pub fn thetis_on_the_air(&self) -> bool {
        self.thetis_tx_active || self.tune_active
    }

    /// PTT-off transition detection: trigger forces eval.
    /// Detect `transmitting_for_ctun` (ours, Thetis's TRX, or Tune) to also let
    /// external-PTT-off (footswitch, VOX, Thetis manual TX) and the end of Tune
    /// through.
    /// Returns true if PTT-off detection just occurred.
    pub fn detect_ptt_off_transition(&mut self) -> bool {
        let any_tx_now = self.transmitting_for_ctun();
        let was_tx = self.prev_tx_active_for_ctun;
        self.prev_tx_active_for_ctun = any_tx_now;
        was_tx && !any_tx_now
    }

    /// Process a single TCI notification
    async fn handle_notification(&mut self, notif: TciNotification) {
        match notif {
            TciNotification::TciCapsEx { caps } => {
                let had_caps = !self.server_caps.is_empty();
                let had_queue_report = self.has_cap("tx_queue_report_ex");
                self.server_caps = caps.into_iter().collect();
                if !had_queue_report && self.has_cap("tx_queue_report_ex") {
                    // Named in advance (`logregels-build212.md`).
                    info!("TCI: Thetis reports its TX queue per over (tx_queue_report_ex)");
                }
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
                // A WAV playback that ended while TCI was down (build 224). With
                // a question still waiting, the state stays for its answer too
                // (build 225): Thetis may answer with our own ZZET0.
                let due = if self.txeq_bypass_pending {
                    self.txeq_restore_due
                } else {
                    self.txeq_restore_due.take()
                };
                if let Some(cmd) = txeq_bypass_restore(due) {
                    self.txeq_restore_sent = self.txeq_restore_due.is_some();
                    self.run_cat(cmd).await;
                    // Named in advance (`logregels-build224.md`).
                    info!(
                        "Thetis TXEQ bypass: restored to {} at the new TCI connection",
                        if cmd == "ZZET1" { "on" } else { "off" }
                    );
                }
                // TL2-1 ctun-auto-recenter: force CTUN-ON at init when cap is present.
                // Idempotent - not harmful to run even when cap is absent.
                self.force_ctun_on_if_capable().await;
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
                            // TL2-1 ctun-auto-recenter: on a VFO-event first ensure
                            // CTUN-on (per operator choice 2026-05-08); then band-switch
                            // detect and trigger-eval. This solves the edge-case where the
                            // operator clicks CTUN-off and then starts tuning - the feature keeps
                            // working without the operator needing to touch the disable checkbox.
                            self.ensure_ctun_on_for_rx(0).await;
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
                            // Same for RX2: ensure CTUN on, then band-switch + trigger-eval.
                            self.ensure_ctun_on_for_rx(1).await;
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
            TciNotification::Trx { receiver, active } => {
                // A PTT left behind (build 213): Thetis keeps a TCI client's PTT
                // when that client's connection goes away (stock Thetis; the fork
                // clears it since TL2-6), so after a server restart Thetis can
                // still be transmitting for nobody. Answered on the receiver it
                // names: `TRX:0` would move a VFO-B transmitter to VFO A.
                // TUNE keys MOX too: our own TUNE is ours, not a PTT left behind (review of 214).
                if self.first_trx.left_behind(receiver, active, self.tx_active || self.we_tune.on()) {
                    // Named in advance (`logregels-build213.md`).
                    warn!(
                        "SAFETY: Thetis was transmitting on receiver {} when the server connected and nothing here holds the PTT - TRX:{},false sent",
                        receiver, receiver
                    );
                    self.send(&format!("TRX:{},false;", receiver)).await;
                }
                // RX-only Amplitec gate: server-initiated PTT paths are already
                // preventively closed off in `PttController`, but Thetis's own
                // PTT (spacebar, footswitch, hardware-PTT on the radio)
                // reaches Thetis without the server seeing it coming. Here
                // we catch the incoming TRX-notif and send
                // `TRX:0,false;` straight back - sub-100 ms reaction time compared
                // to the ~200 ms broadcast-tick check.
                let blocked = self.tx_blocked.load(Ordering::Relaxed);
                debug!("TCI TRX recv: active={} tx_blocked={}", active, blocked);
                // Kept before the RX-only answer returns: a TUNE on a position that
                // is already RX-only is stopped from the loop on this receiver
                // (review of 220).
                if active {
                    self.thetis_tx_receiver = receiver;
                }
                if active && blocked {
                    warn!("Thetis-direct TX detected on RX-only Amplitec position; forcing TRX off (receiver {})", receiver);
                    // On the receiver it names, as for a PTT left behind (build 214):
                    // `TRX:0` would move a VFO-B transmitter to VFO A.
                    self.send(&format!("TRX:{},false;", receiver)).await;
                    // Do not update self.thetis_tx_active - Thetis sends
                    // a TRX:0,false; notif straight after, which this handler
                    // will still react to.
                    return;
                }
                // TL2-1 ctun-auto-recenter: mirror TRX-state in separate
                // `thetis_tx_active` so external-PTT (Thetis-self, footswitch,
                // VOX, etc.) also causes ctun-recenter defer.
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
                let now = Instant::now();
                let use_ = sensor_use(dbm_sig.is_some(), self.sensors_ex_at, now);
                if matches!(use_, SensorUse::Extended { .. }) {
                    if use_ == (SensorUse::Extended { newly: true }) {
                        info!("TCI: rx_channel_sensors_ex active (all 3 sources cached)");
                    }
                    self.sensors_ex_at = Some(now);
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
                } else if use_ == SensorUse::IgnorePlain {
                    // The _ex format is current: the plain one alongside it is
                    // ignored, its instant values would override the average.
                } else {
                    if use_ == SensorUse::PlainAfterExtendedStopped {
                        self.sensors_ex_at = None;
                        // The _ex format stopped (the extensions switched off,
                        // or another Thetis): its last values must not stand
                        // in for the meter, the getters prefer them.
                        info!("TCI: rx_channel_sensors_ex stopped - back to the plain sensor messages");
                        self.forget_sensor_values();
                    }
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
                // Symmetric with the NR-handler (line 553): force level=0 when enabled=false.
                // Thetis sometimes sends the last level back with enabled=false; without this
                // guard the client-side `nb_level` stays stuck at e.g. 2 -> the NB-button does not cycle
                // back to "off". Fix for PATCH-nb-toggle-fix.
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
            TciNotification::Tune { receiver, active } if receiver < 2 => {
                // A tune carrier left behind (build 214): Thetis keeps a TCI
                // client's TUNE when its connection goes away (stock; the fork
                // clears it since TL2-7).
                if tune_report(&mut self.first_tune, &mut self.we_tune, receiver, active) {
                    // Named in advance (`logregels-build214.md`).
                    warn!(
                        "SAFETY: Thetis was tuning on receiver {} when the server connected and nothing here asked for it - tune:{},false sent",
                        receiver, receiver
                    );
                    self.send(&format!("tune:{},false;", receiver)).await;
                }
                let any = merge_tune(&mut self.tune_rx, receiver as usize, active);
                if any != self.tune_active {
                    debug!("TCI: TUNE {} (rx{})", if any { "ON" } else { "OFF" }, receiver);
                    self.tune_active = any;
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
                // Thetis sends the list on connect and again after START, so the
                // same ten names went into the log twice per session. Only a list
                // that actually differs is news.
                if self.tx_profile_names != names {
                    info!("TCI: TX profiles: {:?}", names);
                } else {
                    log::debug!("TCI: TX profiles unchanged ({} entries)", names.len());
                }
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
                // Per-RX cache update - used by ctun-auto-recenter to check on a VFO-event
                // whether CTUN is ON, and if not to force it first.
                let changed = match receiver {
                    0 => { let p = self.ctun; self.ctun = enabled; p != enabled }
                    1 => { let p = self.ctun_rx2; self.ctun_rx2 = enabled; p != enabled }
                    _ => false,
                };
                if changed {
                    info!("TCI: CTUN RX{} = {}", receiver + 1, enabled);
                } else {
                    log::debug!("TCI: CTUN RX{} = {} (unchanged)", receiver + 1, enabled);
                }
            }
            TciNotification::VfoSyncEx { enabled } => {
                if self.vfo_sync_on != enabled {
                    self.vfo_sync_on = enabled;
                    info!("TCI: VFO sync = {}", enabled);
                } else {
                    log::debug!("TCI: VFO sync = {} (unchanged)", enabled);
                }
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
                // Defensive: stock .14/.15 never sends rate=0, but future-Thetis
                // builds or a corrupt push could set state to 0 and the client-UI
                // (`if rate > 0`) would hide the DDC-rate-button. Skip 0.
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
                self.answers_run_cat_ex = true;
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
                            // A restore that was due is the operator's state: Thetis
                            // still holds our own ZZET0 (build 225, review of 224).
                            let due = self.txeq_restore_due.take();
                            self.txeq_restore_sent = false;
                            let prior = due.unwrap_or(on);
                            self.txeq_bypass_saved = Some(prior);
                            self.txeq_bypass_pending = false;
                            self.txeq_bypass_active = true;
                            self.run_cat("ZZET0").await;
                            // Named in advance (`logregels-build225.md`).
                            info!(
                                "Thetis TXEQ bypass: saved prior = {}, TX-EQ off{}",
                                if prior { "on" } else { "off" },
                                if due.is_some() { " (from the restore that was due)" } else { "" }
                            );
                        }
                    }
                } else {
                    log::debug!("TCI: run_cat_ex({}) -> {}", cat_cmd, response);
                }
            }
            TciNotification::DiversityEnableEx { enabled } => {
                // Log the edge, not the state - and for the phase/gain values
                // below, not even the edge while diversity is switched OFF. On a
                // station that never uses it, Thetis still pushes a phase and
                // two gains at every connect, and a number describing a feature
                // that is not running is not information. `Diversity = true`
                // here is what says the values start mattering. Thetis pushes its whole diversity
                // block on connect and again after START, so a station that has
                // never used diversity still got "Diversity = false" six times
                // per session - a line that reports nothing happening. Same rule
                // as the gain handler below, which already had it.
                if self.diversity_enabled != enabled {
                    self.diversity_enabled = enabled;
                    info!("TCI: Diversity = {}", enabled);
                } else {
                    log::debug!("TCI: Diversity = {} (unchanged)", enabled);
                }
            }
            TciNotification::VfoSourceEx { receiver, source } => {
                if self.ctun_recenter.note_vfo_source(receiver, source) {
                    // Named in advance (`logregels-build218.md`).
                    info!(
                        "TCI: VFO source rx={} now {} - {}",
                        receiver,
                        if source == crate::ctun_recenter::VfoSource::Thetis { "thetis" } else { "tl" },
                        if source == crate::ctun_recenter::VfoSource::Thetis { "Thetis scrolls itself" } else { "server recenters" },
                    );
                }
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
                let next = if rx1_ref { 0 } else { 1 };
                if self.diversity_ref != next {
                    self.diversity_ref = next;
                    info!("TCI: Diversity ref = RX{}", if rx1_ref { 1 } else { 2 });
                } else {
                    log::debug!("TCI: Diversity ref = RX{} (unchanged)", if rx1_ref { 1 } else { 2 });
                }
            }
            TciNotification::DiversitySourceEx { source } => {
                if self.diversity_source != source as u8 {
                    self.diversity_source = source as u8;
                    info!("TCI: Diversity source = {}", source);
                } else {
                    log::debug!("TCI: Diversity source = {} (unchanged)", source);
                }
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
                if changed && self.diversity_enabled {
                    info!("TCI: Diversity gain RX{} = {}", receiver + 1, gain);
                } else {
                    log::debug!("TCI: Diversity gain RX{} = {} (unchanged)", receiver + 1, gain);
                }
            }
            TciNotification::DiversityGainMultiEx { multi } => {
                let prev = self.diversity_gain_multi;
                self.diversity_gain_multi = multi;
                if prev != multi && self.diversity_enabled {
                    info!("TCI: Diversity GainMulti = {} (= {:.2}x)", multi, (multi as f32) / 100.0);
                } else {
                    log::debug!("TCI: Diversity GainMulti = {} (unchanged)", multi);
                }
            }
            TciNotification::DdcSampleRateEx { receiver, rate_hz } => {
                // TL2-1 fork per-RX echo. Overrides whatever the global iq_samplerate
                // (which sets both rx fields equal) just wrote, so we end up with the
                // actual per-RX state.
                let changed = if receiver == 0 {
                    let p = self.ddc_sample_rate_rx1;
                    self.ddc_sample_rate_rx1 = rate_hz;
                    p != rate_hz
                } else {
                    let p = self.ddc_sample_rate_rx2;
                    self.ddc_sample_rate_rx2 = rate_hz;
                    p != rate_hz
                };
                if changed {
                    info!("TCI: ddc_sample_rate_ex RX{} = {} Hz", receiver + 1, rate_hz);
                } else {
                    log::debug!("TCI: ddc_sample_rate_ex RX{} = {} Hz (unchanged)", receiver + 1, rate_hz);
                }
            }
            TciNotification::DiversityPhaseEx { phase } => {
                let prev = self.diversity_phase;
                self.diversity_phase = phase;
                if prev != phase && self.diversity_enabled {
                    info!("TCI: Diversity phase = {}", phase);
                } else {
                    log::debug!("TCI: Diversity phase = {} (unchanged)", phase);
                }
            }
            TciNotification::RxAudio { .. } | TciNotification::IqStream { .. } => {
                // These are routed directly to their channels by the reader task
                // They shouldn't arrive here, but ignore if they do
            }
            TciNotification::TxQueueReportEx(report) => {
                // Named in advance (`logregels-build212.md`): what Thetis' own
                // TX queue did in the over that just ended, beside our ring line.
                dev_info!("{}", report.line());
            }
            TciNotification::TxQueueReportUnknownVersion { version } => {
                warn!("TCI: tx_queue_report_ex version {} not understood - ignored", version);
            }
            TciNotification::TxQueueReportUnreadable { line } => {
                // Out loud, so a broken line does not read as "the fork does not send" (review of 212).
                warn!("TCI: tx_queue_report_ex not readable - ignored: {}", line);
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
            // A full channel lost TX audio without a word until build 105 (round 104).
            if tx.try_send(samples.to_vec()).is_err() {
                self.tx_audio_dropped += 1;
                let n = self.tx_audio_dropped;
                if n == 1 || n % 50 == 0 {
                    warn!("TX audio to Thetis dropped (channel full) - {} chunk(s) so far", n);
                }
            }
        }
    }

    /// What stands in the TX ring for Thetis's next request, in samples
    /// (48 kHz mono); `None` without a reader (build 193).
    pub fn tx_ring_level(&self) -> Option<usize> {
        self.tx_ring_level.as_ref().map(|l| l.load(Ordering::Relaxed))
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
        self.tx_ring_level = None;
        self.server_caps.clear();
        // The next Thetis may not send the _ex sensors - and the values this
        // one left must not stand in for the meter (the getters prefer them)
        // until a PTT happens to clear them (review 318).
        self.sensors_ex_at = None;
        self.forget_sensor_values();
        // What Thetis was doing is unknown once it is gone. Left standing, a
        // TX or TUNE that was on at the moment of the drop would hold the PA
        // guard for good - the same class as the Yaesu `connected` gate a review
        // found on build 65, found here by asking the same question of Thetis.
        self.thetis_tx_active = false;
        self.tune_active = false;
        self.tune_rx = [false; 2];
        // The next connection's first reports are first again (builds 213, 214).
        self.first_trx = FirstReport::default();
        self.first_tune = FirstReport::default();
        // Who tuned last is the fork's to say again on the next connection (build 218).
        self.ctun_recenter.forget_vfo_sources();
        // Whether the next Thetis answers run_cat_ex is its own to show (build 221).
        self.answers_run_cat_ex = false;
        // A ZZET question without an answer goes with its connection (build
        // 223). Our own ZZET0 is kept: the end of the playback undoes it.
        self.txeq_bypass_pending = false;
        // A restore READY already sent was kept only for that answer (build
        // 226, review of 225); one not sent yet stays for the next READY.
        if std::mem::take(&mut self.txeq_restore_sent) && self.txeq_restore_due.take().is_some() {
            // Named in advance (`logregels-build226.md`).
            info!("Thetis TXEQ bypass: the restore went out at READY - not said again after this drop");
        }
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

    /// Our own TUNE is on (sent by this server and not yet turned off).
    pub fn tune_is_ours(&self) -> bool {
        self.we_tune.on()
    }

    /// TCI connections made so far (build 220).
    pub fn connection_count(&self) -> u64 {
        self.connections
    }

    /// The link as a test needs it (build 226).
    #[cfg(test)]
    pub(crate) fn set_link_for_test(&mut self, connected: bool, connections: u64) {
        self.connected = connected;
        self.connections = connections;
    }

    /// Stop a TX Thetis was already making when the Amplitec position became
    /// RX-only (build 220). The TRX arm answers a new "on"; this one was on
    /// before the position changed. What goes out: `rx_only_stop_commands`.
    pub async fn force_thetis_tx_off(&mut self) {
        for cmd in rx_only_stop_commands(self.thetis_tx_receiver, self.tune_rx) {
            self.send(&cmd).await;
        }
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

    /// The values of the `_ex` sensor format, all eight: the getters prefer
    /// them over the plain average while they are there, and `smeter_raw_dbm`
    /// feeds the automatic spectrum calibration.
    fn forget_sensor_values(&mut self) {
        self.smeter_raw_dbm = [None; 2];
        self.smeter_direct_dbm = None;
        self.smeter_sig_dbm = None;
        self.smeter_peakbin_dbm = None;
        self.smeter_rx2_direct_dbm = None;
        self.smeter_rx2_sig_dbm = None;
        self.smeter_rx2_peakbin_dbm = None;
    }

    pub fn set_tx_active(&mut self, active: bool) {
        if active != self.tx_active {
            self.tx_active = active;
            self.smeter_window.clear();
            self.smeter_rx2_window.clear();
            self.forget_sensor_values();
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

/// The TX audio waiting for Thetis's next TX_CHRONO request - one place for the
/// three paths that feed it (regel 15). It is filled from the server's jitter
/// buffer, which is not tied to TX (round 104), and emptied by Thetis's
/// requests, which stop at TRX off. What is left when the next over starts was
/// cut from the end of the previous one; since build 105 it is dropped there
/// instead of going out first (owner's decision, twelfth measurement).
/// Reachable in the crate for the replay of the transmit chain (`tx_replay`,
/// build 246).
pub(crate) struct TxRing {
    ring: VecDeque<f32>,
    last_feed: Option<Instant>,
    over: OverStats,
    /// Silence padded since the last audio came in, and in how many requests:
    /// a gap once more audio follows, the over's end when a TRX off comes
    /// first (build 183; the TRX off since build 280).
    pending_pad: (usize, u32),
    /// The ring's length after every feed and take, for the playout tick
    /// (build 193, `pull_extra_for_ring_below`).
    level: Arc<AtomicUsize>,
    /// Requests from Thetis the ring could not answer in full yet, oldest
    /// first (build 195, `request`).
    waiting: VecDeque<Waiting>,
    /// The last TRX-off count of the server seen (build 196, `note_trx_off`).
    trx_off_seen: u64,
    /// How long a request waits before it is padded: `TX_HOLD_MAX`, another
    /// only in the replay (build 255).
    hold_max: std::time::Duration,
}

/// A TX_CHRONO request from Thetis, kept until there is audio for it (build
/// 195): with what is needed to answer it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Waiting {
    pub(crate) n: usize,
    pub(crate) at: Instant,
    pub(crate) sample_rate: u32,
    pub(crate) format: u32,
    pub(crate) channels: u32,
}

/// The longest a request waits for audio before it gets what there is, padded
/// with silence (build 195; owner 13-10: "150 ms is goed"; 100 ms as part of
/// tuning B, owner 2026-10-19, build 258). Thetis counts an
/// open request as audio to come for 250 ms after its last request and then
/// asks again for all of it (`cmaster.cs`, `serviceTCITxProtocol`); this stays
/// well under that, so Thetis never asks twice and no answer lands on top of a
/// second one.
pub(crate) const TX_HOLD_MAX: std::time::Duration = std::time::Duration::from_millis(100);

/// What one over did to the ring, to tell the two hypotheses for a leftover
/// apart (build 105): a lead built up before Thetis started asking (`first_ready`
/// and `lowest` stay high), or audio that came in after Thetis stopped asking
/// (`fed_after_take`). `taken_after_feed` makes a 0 left unambiguous (round 104):
/// nothing left, or fetched after the last audio came in.
#[derive(Default, Debug, Clone, Copy, PartialEq)]
pub(crate) struct OverStats {
    first_ready: Option<usize>,
    lowest: Option<usize>,
    fed_after_take: usize,
    taken_after_feed: usize,
    /// Samples Thetis got as silence because the ring was empty, while more
    /// audio came in afterwards with no TRX off between - and in how many
    /// requests. A hard step to zero and back: the click a listener hears
    /// (build 183; across a split of TX_OVER_GAP since build 280).
    padded_mid: usize,
    short_mid: u32,
    /// Samples pushed out of the full ring by newer audio (build 185; review
    /// of 184).
    overflow: usize,
    /// Audio that came 30-200 ms after the one before instead of ~20: a tick
    /// that brought nothing, and the lead it cost (ms beyond the 20) - build
    /// 192, the measurement of why the lead runs down inside an over.
    feed_gaps: u32,
    feed_gap_late_ms: u64,
    /// The largest TX_CHRONO request, in samples.
    largest_request: usize,
    /// When Thetis first asked in this over, and what stood in the ring at its
    /// first request one second or more after that (build 193: where the lead
    /// of the first request goes; the lead at the last request read 0 in every
    /// over, round 192).
    first_request_at: Option<Instant>,
    lead_1s: Option<usize>,
    /// When the first `FIRST_REQUESTS` requests came, in ms after the first,
    /// and how many did (build 194: does Thetis ask its own queue full in one
    /// burst at the start of MOX? `cmaster.cs`, `serviceTCITxProtocol`).
    first_requests: [u16; FIRST_REQUESTS],
    first_requests_seen: usize,
    /// Requests answered later than they came, the longest wait, those that
    /// got silence at `TX_HOLD_MAX`, and those never answered - dropped at TRX
    /// off or at a new over (build 195).
    held: u32,
    held_max_ms: u64,
    silence_at_limit: u32,
    dropped_waiting: u32,
}

/// How many requests at the start of an over are timed (build 194).
const FIRST_REQUESTS: usize = 8;

/// A feed this much later than the one before counts as a gap in the feeding
/// (build 192); `TX_OVER_GAP` and more is a new over.
const TX_FEED_GAP: std::time::Duration = std::time::Duration::from_millis(30);
/// The pace the server feeds at: one frame per playout tick - the 20 ms of
/// `playout_tick` in `network.rs` (a second place on purpose: the tick is an
/// `interval` there, this only reads it; review of 192).
const TX_FEED_PACE_MS: u64 = 20;

/// Below this many samples in the ring (two 20 ms frames at 48 kHz) when a
/// playout tick begins, the tick pulls a second frame - if the jitter buffer
/// holds more than its target, frames it would otherwise trim away later
/// (build 193; field test 2026-10-13: a late feed became silence at Thetis
/// almost one to one, while the jitter buffer threw more away to catch up than
/// the ring padded). The receive side has the same shape
/// (`frames_for_tick` in sdr-remote-logic); the server does not depend on that
/// crate and the units differ, so the rule is here a second time on purpose.
pub const TX_RING_LOW: usize = 2 * 960;

/// Whether a playout tick pulls a second frame (build 193): the ring stood
/// below `ring_low` when the tick began, and after the first pull the jitter
/// buffer still holds more than its target. Never more than the excess: the
/// buffer's cushion against the line's jitter stays where it is. The total
/// delay (buffer plus ring) does grow, by the ring's cushion: ~20-40 ms, at
/// most ~40-60 ms, and only in overs whose ring ran low - what the trim would
/// have thrown away now stands in the ring (review of 193). That cushion is
/// what covers the next late feed. The mark: Thetis's `TX_RING_LOW` in
/// samples, a Yaesu's lead in ms against its sound card's ring in ms (build
/// 278), the replay's knob (build 255). Asked by `tx_recover::play_tick` for
/// Thetis and the radios alike since build 288.
pub fn pull_extra_for_ring_below(ring_low: usize, ring_at_tick: Option<usize>, jitter_depth: usize, jitter_target: u32) -> bool {
    matches!(ring_at_tick, Some(level) if level < ring_low) && jitter_depth > jitter_target as usize
}

/// The previous over, reported when the next one starts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct OverReport {
    stats: OverStats,
    left: usize,
}

/// No TX audio for this long: the next audio starts a new over.
const TX_OVER_GAP: std::time::Duration = std::time::Duration::from_millis(200);

impl TxRing {
    fn new() -> Self {
        TxRing {
            ring: VecDeque::with_capacity(TX_RING_CAPACITY),
            last_feed: None,
            over: OverStats::default(),
            pending_pad: (0, 0),
            level: Arc::new(AtomicUsize::new(0)),
            waiting: VecDeque::new(),
            trx_off_seen: 0,
            hold_max: TX_HOLD_MAX,
        }
    }

    /// A ring that publishes its length to `level` (build 193).
    pub(crate) fn with_level(level: Arc<AtomicUsize>) -> Self {
        TxRing { level, ..TxRing::new() }
    }

    /// The same ring with another hold than `TX_HOLD_MAX` (the replay, build 255).
    #[cfg_attr(not(any(test, feature = "naspelen")), allow(dead_code))]
    pub(crate) fn with_hold_max(self, hold_max: std::time::Duration) -> Self {
        TxRing { hold_max, ..self }
    }

    /// Add TX audio. When this audio starts a new over (after `TX_OVER_GAP`
    /// without any) it returns the previous over's report and drops what that
    /// over left behind. `None` within an over and for the very first over.
    pub(crate) fn feed(&mut self, samples: &[f32], now: Instant) -> Option<OverReport> {
        let new_over = self.last_feed.map_or(true, |t| now.duration_since(t) >= TX_OVER_GAP);
        // Audio came after silence was handed out: that silence was a gap
        // inside the MOX - also when the gap was long enough to start a new
        // over here; it goes in the over it fell in. A TRX off between made it
        // the over's end and cleared it (`note_trx_off`, build 280; until then
        // a gap of TX_OVER_GAP or more counted nowhere).
        self.over.padded_mid += self.pending_pad.0;
        self.over.short_mid += self.pending_pad.1;
        // Waiting requests stay at a new over (build 196; review of 195): a
        // gap of TX_OVER_GAP inside one MOX starts a new over here, and Thetis
        // still counts them as open - dropped, it asked nothing more for 250
        // ms and played zeros. After MOX off they are gone already.
        let report = if new_over && self.last_feed.is_some() {
            Some(OverReport { stats: self.over, left: self.ring.len() })
        } else {
            None
        };
        if new_over {
            // Old by definition: it belongs to the previous over.
            self.ring.clear();
            self.over = OverStats::default();
        } else {
            // A late feed inside the over (build 192).
            if let Some(last) = self.last_feed {
                let since = now.duration_since(last);
                if since >= TX_FEED_GAP {
                    self.over.feed_gaps += 1;
                    self.over.feed_gap_late_ms += (since.as_millis() as u64).saturating_sub(TX_FEED_PACE_MS);
                }
            }
        }
        self.pending_pad = (0, 0);
        self.last_feed = Some(now);
        if self.over.first_ready.is_some() {
            self.over.fed_after_take += samples.len();
        }
        self.over.taken_after_feed = 0;
        // A chunk bigger than the whole ring keeps its newest part. The loop alone
        // spun forever then - the ring empty and still "too full" - and with it the
        // TCI reader (found by the test of build 104; chunks are 20 ms in practice).
        let cut = samples.len().saturating_sub(TX_RING_CAPACITY);
        self.over.overflow += cut;
        let samples = &samples[cut..];
        while self.ring.len() + samples.len() > TX_RING_CAPACITY {
            self.ring.pop_front();
            self.over.overflow += 1;
        }
        self.ring.extend(samples);
        self.level.store(self.ring.len(), Ordering::Relaxed);
        report
    }

    /// Feed and, at an over's start, say what the previous one did.
    fn feed_and_report(&mut self, samples: &[f32]) {
        if let Some(r) = self.feed(samples, Instant::now()) {
            dev_info!("{}", over_line(&r));
        }
    }

    /// A request answered at once with what there is, zero-padded - the way
    /// every request went until build 195; kept for the tests of the
    /// statistics.
    #[cfg(test)]
    fn take(&mut self, n: usize) -> Vec<f32> {
        self.take_at(n, Instant::now())
    }

    #[cfg(test)]
    fn take_at(&mut self, n: usize, now: Instant) -> Vec<f32> {
        self.arrive(n, now);
        self.answer(n)
    }

    /// A TX_CHRONO request (build 195): answered now if the ring holds all of
    /// it and nothing older waits, else kept until audio comes or it has
    /// waited `TX_HOLD_MAX`. Answering with silence put that silence in
    /// Thetis's own queue behind the speech: at every start of MOX Thetis asks
    /// 100 ms at once, the ring held 40-80 ms of it, and the rest went out as
    /// silence after the first words (field test 2026-10-13, 24 of 34 overs).
    pub(crate) fn request(&mut self, w: Waiting) -> Vec<(Waiting, Vec<f32>)> {
        self.arrive(w.n, w.at);
        self.waiting.push_back(w);
        self.answer_ready(w.at)
    }

    /// The waiting requests that can be answered at `now`, oldest first: in
    /// full from the ring, or padded once they waited `TX_HOLD_MAX`.
    pub(crate) fn answer_ready(&mut self, now: Instant) -> Vec<(Waiting, Vec<f32>)> {
        let mut out = Vec::new();
        while let Some(&w) = self.waiting.front() {
            let full = self.ring.len() >= w.n;
            let waited = now.saturating_duration_since(w.at);
            if !full && waited < self.hold_max {
                break;
            }
            self.waiting.pop_front();
            if waited > std::time::Duration::ZERO {
                self.over.held += 1;
                self.over.held_max_ms = self.over.held_max_ms.max(waited.as_millis() as u64);
            }
            if !full {
                self.over.silence_at_limit += 1;
            }
            out.push((w, self.answer(w.n)));
        }
        out
    }

    /// Thetis ended MOX, or a new over began: the waiting requests belong to a
    /// queue Thetis has emptied, and an answer now would wait in Thetis until
    /// its next MOX (`handleBinaryFrame` does not look at MOX; build 195).
    fn drop_waiting(&mut self) {
        self.over.dropped_waiting += self.waiting.len() as u32;
        self.waiting.clear();
    }

    /// The server's TRX-off count as the reader sees it: a change drops the
    /// waiting requests (build 196), and the silence handed out since the last
    /// audio was the over's end, not a gap (build 280).
    pub(crate) fn note_trx_off(&mut self, count: u64) {
        if count != self.trx_off_seen {
            self.trx_off_seen = count;
            self.drop_waiting();
            self.pending_pad = (0, 0);
        }
    }

    /// When the oldest waiting request reaches `TX_HOLD_MAX`.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.waiting.front().map(|w| w.at + self.hold_max)
    }

    /// What a request tells about the over when it comes: the lead at the
    /// first, a second later, and the time of the first eight (builds 192-194).
    fn arrive(&mut self, n: usize, now: Instant) {
        if self.over.first_ready.is_none() {
            self.over.first_ready = Some(self.ring.len());
            self.over.first_request_at = Some(now);
        } else if self.over.lead_1s.is_none()
            && self.over.first_request_at.is_some_and(|t| now.duration_since(t) >= std::time::Duration::from_secs(1))
        {
            self.over.lead_1s = Some(self.ring.len());
        }
        if let Some(first) = self.over.first_request_at {
            let i = self.over.first_requests_seen;
            if i < FIRST_REQUESTS {
                self.over.first_requests[i] = now.duration_since(first).as_millis().min(u16::MAX as u128) as u16;
                self.over.first_requests_seen += 1;
            }
        }
        self.over.largest_request = self.over.largest_request.max(n);
    }

    /// Up to `n` samples for a TX_CHRONO answer, zero-padded.
    fn answer(&mut self, n: usize) -> Vec<f32> {
        let mut audio = vec![0.0f32; n];
        let available = self.ring.len().min(n);
        if available < n && self.last_feed.is_some() {
            self.pending_pad.0 += n - available;
            self.pending_pad.1 += 1;
        }
        for slot in audio.iter_mut().take(available) {
            *slot = self.ring.pop_front().unwrap_or(0.0);
        }
        let now_len = self.ring.len();
        self.over.lowest = Some(self.over.lowest.map_or(now_len, |l| l.min(now_len)));
        self.over.fed_after_take = 0;
        self.over.taken_after_feed += available;
        self.level.store(self.ring.len(), Ordering::Relaxed);
        audio
    }
}

/// The over line, built in one place so its arguments can be pinned by a test
/// (build 193; review of 192: only the text was pinned, not the order).
fn over_line(r: &OverReport) -> String {
    let ms = |n: usize| n / 48; // 48 kHz mono
    let opt = |n: Option<usize>| n.map_or("-".to_string(), |n| ms(n).to_string());
    let seen = &r.stats.first_requests[..r.stats.first_requests_seen];
    let firsts = if seen.is_empty() {
        "-".to_string()
    } else {
        seen.iter().map(|t| format!("+{}", t)).collect::<Vec<_>>().join(" ")
    };
    // Named in the plan before it was built (working rule 11; builds 105, 183,
    // 185, 192, 193, 194).
    format!(
        "TX ring, previous over: {} ms ready at Thetis's first request, lowest {} ms after a request, \
         {} ms fed after its last request, {} ms taken after the last audio, {} ms left - dropped, \
         {} ms padded with silence before more audio came, in {} request(s), {} ms pushed out by newer audio, \
         {} feed gap(s) {} ms late in all, requests up to {} ms, lead {} ms at the first request and {} ms one second later, first requests at {} ms, \
         held {} request(s) up to {} ms, {} answered with silence at the {} ms limit, {} dropped at TRX off or a new over",
        opt(r.stats.first_ready),
        opt(r.stats.lowest),
        ms(r.stats.fed_after_take),
        ms(r.stats.taken_after_feed),
        ms(r.left),
        ms(r.stats.padded_mid),
        r.stats.short_mid,
        ms(r.stats.overflow),
        r.stats.feed_gaps,
        r.stats.feed_gap_late_ms,
        ms(r.stats.largest_request),
        opt(r.stats.first_ready),
        opt(r.stats.lead_1s),
        firsts,
        r.stats.held,
        r.stats.held_max_ms,
        r.stats.silence_at_limit,
        // The hold as it runs: 150 ms written in the text until build 258.
        TX_HOLD_MAX.as_millis(),
        r.stats.dropped_waiting
    )
}

/// A TRX off for Thetis, on either receiver (build 196). In either case:
/// `ZZTX0` through `send_cat` goes out as `trx:0,false;` (`cat_to_tci`), and
/// Thetis reads both (build 197; review of 196).
fn is_trx_off(cmd: &str) -> bool {
    cmd.get(..4).is_some_and(|head| head.eq_ignore_ascii_case("TRX:"))
        && cmd.trim_end_matches(';').split(',').nth(1).is_some_and(|v| v.eq_ignore_ascii_case("false"))
}

/// Send the answers to Thetis's requests (build 195).
fn send_tx_answers(chrono_bin_tx: &mpsc::Sender<Vec<u8>>, answers: Vec<(Waiting, Vec<f32>)>) {
    for (w, audio) in answers {
        let frame = build_tci_binary_frame(
            0, w.sample_rate, w.format, w.n as u32,
            STREAM_TYPE_TX_AUDIO, w.channels, &audio, w.format,
        );
        let _ = chrono_bin_tx.try_send(frame);
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
    tx_ring_level: Arc<AtomicUsize>,
    tx_off_count: Arc<AtomicU64>,
) {
    // TX audio ring buffer for low-latency TX_CHRONO response
    let mut tx_ring = TxRing::with_level(tx_ring_level);
    tx_ring.trx_off_seen = tx_off_count.load(Ordering::Relaxed);
    let mut audio_debug_rx0 = false;
    let mut audio_debug_rx1 = false;
    let mut audio_debug_bin_r = false;
    // Dropped IQ frames per receiver (index 0 = RX1, 1 = RX2).
    let mut iq_drops: [u64; 2] = [0, 0];
    // Dropped receive audio frames: RX1, RX2, binaural right (build 150).
    let mut audio_drops: [u64; 3] = [0, 0, 0];
    loop {
        // Drain any pending TX audio into ring buffer (non-blocking), and
        // answer the requests it is enough for, or that waited too long.
        while let Ok(samples) = tx_audio_rx.try_recv() {
            tx_ring.feed_and_report(&samples);
        }
        tx_ring.note_trx_off(tx_off_count.load(Ordering::Relaxed));
        send_tx_answers(&chrono_bin_tx, tx_ring.answer_ready(Instant::now()));
        let deadline = tx_ring.next_deadline();

        let msg_result = tokio::select! {
            msg = ws_read.next() => {
                match msg {
                    Some(r) => r,
                    None => break,
                }
            }
            Some(samples) = tx_audio_rx.recv() => {
                // TX audio arrived while waiting for WebSocket message
                tx_ring.feed_and_report(&samples);
                continue; // Go back to draining + waiting
            }
            // The oldest waiting request reached its limit (build 195).
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline.unwrap_or_else(Instant::now))),
                if deadline.is_some() => {
                continue;
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
                            if let TciNotification::Trx { active: false, .. } = notif {
                                // MOX ended: Thetis emptied its queue (build 195).
                                tx_ring.drop_waiting();
                            }
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
                        // Counted and said, as the IQ path below does (build 150).
                        // These were a bare `let _ =`, and a frame lost on one side
                        // of binaural and not the other moves left and right apart.
                        if let Err(e) = tx.try_send(left) {
                            let i = receiver.min(1) as usize;
                            audio_drops[i] += 1;
                            let n = audio_drops[i];
                            if n == 1 || n % 100 == 0 {
                                // Named in advance (working rule 11, builds 150 and 151).
                                warn!(
                                    "TCI RX{} audio frame dropped ({}) - {} so far; this receiver's audio has a hole where they fall",
                                    i + 1, drop_reason(&e), n
                                );
                            }
                        }
                        // Send right channel (binaural) only for RX1 and only when stereo
                        if receiver == 0 && !right.is_empty() {
                            if !audio_debug_bin_r {
                                info!("TCI BinR: first stereo R channel ({} samples)", right.len());
                                audio_debug_bin_r = true;
                            }
                            if let Err(e) = bin_r_audio_tx.try_send(right) {
                                audio_drops[2] += 1;
                                let n = audio_drops[2];
                                if n == 1 || n % 100 == 0 {
                                    // Named in advance (working rule 11, builds 150 and 151).
                                    warn!(
                                        "TCI BinR audio frame dropped ({}) - {} so far; binaural right loses a frame the left keeps",
                                        drop_reason(&e), n
                                    );
                                }
                            }
                        }
                    }
                    STREAM_TYPE_IQ => {
                        let iq_pairs = decode_iq_payload(payload, format, length, channels);
                        let tx = if receiver == 0 { &iq_rx1_tx } else { &iq_rx2_tx };
                        // A full queue means the consumer of this receiver is not
                        // keeping up, and the dropped frame is a hole in the VRX
                        // audio built from this stream. That used to be a bare
                        // `let _ =`: no counter, no line in the log, so a channel
                        // could sound consistently broken with nothing to point at.
                        if let Err(e) = tx.try_send((sample_rate, iq_pairs)) {
                            let i = receiver.min(1) as usize;
                            let n = &mut iq_drops[i];
                            *n += 1;
                            if *n == 1 || *n % 100 == 0 {
                                warn!(
                                    "TCI RX{} IQ frame dropped ({}) - {} so far; the VRX on this receiver loses audio where they fall",
                                    i + 1, drop_reason(&e), n
                                );
                            }
                        }
                    }
                    STREAM_TYPE_TX_CHRONO => {
                        // Drain any pending TX audio into ring buffer
                        while let Ok(samples) = tx_audio_rx.try_recv() {
                            tx_ring.feed_and_report(&samples);
                        }

                        // Answer with audio, now or as soon as it comes; silence
                        // only after TX_HOLD_MAX (build 195).
                        let waiting = Waiting { n: length as usize, at: Instant::now(), sample_rate, format, channels };
                        tx_ring.note_trx_off(tx_off_count.load(Ordering::Relaxed));
                        send_tx_answers(&chrono_bin_tx, tx_ring.request(waiting));
                    }
                    _ => {}
                }
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {}
        }
    }
    // Every way out of the loop ends here - Close, an error, and a stream that
    // simply ends (`None`, which used to break without a word). Sent with a
    // wait for room rather than try_send, so a full queue cannot lose it; and
    // the consumer treats the channel closing as a disconnect regardless.
    let _ = notify_tx.send(TciNotification::Disconnected).await;
}


// Parser functions, binary decoders, and mode conversion moved to tci_parser.rs
/// TUNE as Thetis reports it, one flag over both receivers.
///
/// Thetis sends `tune:1,…` when VFO B is the TX VFO - the same one transmitter,
/// reported on the other receiver. Both writers of the TUNE state go through
/// here (the notification, and the server's own `set_tune`), so neither can
/// leave the other's receiver behind: a review on build 65 found `set_tune` writing
/// the flag directly, and a receiver-1 TUNE could then be pushed away by a
/// server-sent TUNE OFF. Returns whether any receiver is tuning.
pub(crate) fn merge_tune(rx: &mut [bool; 2], receiver: usize, active: bool) -> bool {
    if receiver < 2 {
        rx[receiver] = active;
    }
    rx[0] || rx[1]
}

#[cfg(test)]
mod tx_ring_tests {
    use super::*;
    use std::time::Duration;

    /// One over after another, without a restart: the report of the previous over
    /// and the drop of its leftover (build 105).
    #[test]
    fn an_over_start_reports_the_previous_over_and_drops_its_leftover() {
        let t0 = Instant::now();
        let mut r = TxRing::new();
        assert_eq!(r.feed(&[0.1; 960], t0), None, "the first over ever: nothing to report");
        r.feed(&[0.1; 960], t0 + Duration::from_millis(20));
        // Thetis asks for 1500: 1920 were ready, 420 stay.
        assert_eq!(r.take(1500).len(), 1500);
        // One more chunk after the last request: audio that came after Thetis stopped.
        r.feed(&[0.1; 960], t0 + Duration::from_millis(40));
        let t1 = t0 + Duration::from_millis(40) + TX_OVER_GAP;
        let rep = r.feed(&[0.2; 960], t1).expect("a new over reports the previous one");
        assert_eq!(rep.stats.first_ready, Some(1920));
        assert_eq!(rep.stats.lowest, Some(420));
        assert_eq!(rep.stats.fed_after_take, 960, "fed after Thetis's last request");
        assert_eq!(rep.stats.taken_after_feed, 0);
        assert_eq!(rep.left, 1380);
        // The leftover is dropped: this over starts with its own audio.
        assert_eq!(r.ring.len(), 960);
        assert_eq!(r.take(10)[0], 0.2, "the previous over's audio does not go out");
        // A short pause is the same over: no report, nothing dropped.
        assert_eq!(r.feed(&[0.2; 960], t1 + Duration::from_millis(199)), None);
        // An over that Thetis empties after the last audio: 0 left, and it says
        // it was taken - a 0 that is not ambiguous (round 104).
        r.take(5000);
        let rep = r.feed(&[0.3; 960], t1 + Duration::from_millis(199) + TX_OVER_GAP).unwrap();
        assert_eq!(rep.left, 0);
        assert_eq!(rep.stats.fed_after_take, 0);
        assert_eq!(rep.stats.taken_after_feed, 950 + 960, "taken after the last audio came in");
        // An over Thetis never asked from: no first request, everything left.
        let rep = r.feed(&[0.4; 10], t1 + Duration::from_secs(5)).unwrap();
        assert_eq!((rep.stats.first_ready, rep.left), (None, 960));
    }

    /// A feed 30-200 ms after the one before is a gap in the feeding, with the
    /// lead it cost; the requests and the lead at the first and last are
    /// recorded (build 192, a measurement).
    #[test]
    fn a_late_feed_inside_an_over_is_counted() {
        let t0 = Instant::now();
        let mut r = TxRing::new();
        r.feed(&[0.1; 960], t0);
        r.take(2048);
        r.feed(&[0.1; 960], t0 + Duration::from_millis(20)); // on time
        r.feed(&[0.1; 960], t0 + Duration::from_millis(80)); // 60 ms after: 40 late
        r.feed(&[0.1; 960], t0 + Duration::from_millis(100)); // on time
        r.take(960);
        r.feed(&[0.1; 960], t0 + Duration::from_millis(190)); // 90 ms after: 70 late
        r.take(480);
        let rep = r.feed(&[0.2; 960], t0 + Duration::from_secs(5)).unwrap();
        assert_eq!((rep.stats.feed_gaps, rep.stats.feed_gap_late_ms), (2, 40 + 70));
        assert_eq!(rep.stats.largest_request, 2048);
        assert_eq!(rep.stats.first_ready, Some(960));
    }

    /// The lead one second after the first request: at the first request a
    /// second or more later, once, and never at the first itself; the ring
    /// publishes its length after every feed and take (build 193).
    #[test]
    fn the_lead_one_second_in_is_recorded_once() {
        let t0 = Instant::now();
        let level = Arc::new(AtomicUsize::new(0));
        let mut r = TxRing::with_level(level.clone());
        r.feed(&[0.1; 2880], t0);
        assert_eq!(level.load(Ordering::Relaxed), 2880);
        r.take_at(960, t0);
        assert_eq!(level.load(Ordering::Relaxed), 1920);
        // Keep the over going: nothing for TX_OVER_GAP would start a new one.
        for ms in [150, 300, 450, 600, 750, 900] {
            r.feed(&[], t0 + Duration::from_millis(ms));
        }
        r.take_at(960, t0 + Duration::from_millis(999));
        assert_eq!(r.over.lead_1s, None, "not yet a second");
        r.feed(&[0.1; 960], t0 + Duration::from_millis(1000));
        r.take_at(480, t0 + Duration::from_millis(1000));
        assert_eq!(r.over.lead_1s, Some(1920), "what stood there at that request");
        r.take_at(480, t0 + Duration::from_millis(1100));
        assert_eq!(r.over.lead_1s, Some(1920), "only once");
        assert_eq!(level.load(Ordering::Relaxed), 960);
        let rep = r.feed(&[0.2; 960], t0 + Duration::from_secs(5)).unwrap();
        assert_eq!((rep.stats.first_ready, rep.stats.lead_1s), (Some(2880), Some(1920)));
        // The next over starts clean.
        r.take_at(960, t0 + Duration::from_secs(5));
        assert_eq!((r.over.first_ready, r.over.lead_1s), (Some(960), None));
    }

    /// The first eight requests of an over are timed from the first, the rest
    /// not, and the next over starts again (build 194).
    #[test]
    fn the_first_requests_are_timed() {
        let t0 = Instant::now();
        let mut r = TxRing::new();
        r.feed(&[0.1; 4800], t0);
        for ms in [0, 0, 1, 2, 3, 20, 40, 60, 80, 100] {
            r.take_at(960, t0 + Duration::from_millis(ms));
        }
        assert_eq!(r.over.first_requests_seen, 8, "only the first eight");
        assert_eq!(r.over.first_requests, [0, 0, 1, 2, 3, 20, 40, 60]);
        let rep = r.feed(&[0.2; 960], t0 + Duration::from_secs(5)).unwrap();
        assert!(over_line(&rep).contains("first requests at +0 +0 +1 +2 +3 +20 +40 +60 ms,"));
        r.take_at(960, t0 + Duration::from_secs(5) + Duration::from_millis(7));
        assert_eq!((r.over.first_requests_seen, r.over.first_requests[0]), (1, 0), "the next over starts at +0");
    }

    fn w(n: usize, at: Instant) -> Waiting {
        Waiting { n, at, sample_rate: 48000, format: 3, channels: 1 }
    }

    /// The burst at the start of MOX: what the ring holds is answered at once,
    /// the rest waits for audio instead of going out as silence, oldest first
    /// (build 195).
    #[test]
    fn a_request_waits_for_audio_instead_of_silence() {
        let t0 = Instant::now();
        let mut r = TxRing::new();
        r.feed(&[0.1; 2880], t0); // 60 ms
        let mut now_answered = Vec::new();
        for i in 0..5 {
            now_answered.extend(r.request(w(960, t0 + Duration::from_micros(i))));
        }
        assert_eq!(now_answered.len(), 3, "60 ms answered at once");
        assert!(now_answered.iter().all(|(_, a)| a.iter().all(|&x| x == 0.1)), "with speech, not silence");
        assert_eq!(r.waiting.len(), 2);
        assert_eq!(r.next_deadline(), Some(t0 + Duration::from_micros(3) + TX_HOLD_MAX));
        r.feed(&[0.2; 960], t0 + Duration::from_millis(20));
        let a = r.answer_ready(t0 + Duration::from_millis(20));
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].0.at, t0 + Duration::from_micros(3), "the oldest first");
        assert!(a[0].1.iter().all(|&x| x == 0.2));
        r.feed(&[0.3; 960], t0 + Duration::from_millis(40));
        assert_eq!(r.answer_ready(t0 + Duration::from_millis(40)).len(), 1);
        assert_eq!(r.next_deadline(), None);
        let rep = r.feed(&[0.4; 960], t0 + Duration::from_secs(5)).unwrap();
        assert_eq!((rep.stats.held, rep.stats.held_max_ms, rep.stats.silence_at_limit), (2, 39, 0));
        assert_eq!((rep.stats.padded_mid, rep.stats.short_mid), (0, 0), "no silence at the start");
    }

    /// Without audio a request gets silence at `TX_HOLD_MAX`, not before; a
    /// newer one does not overtake it (build 195).
    #[test]
    fn a_request_gets_silence_at_the_limit() {
        let t0 = Instant::now();
        let mut r = TxRing::new();
        r.feed(&[0.1; 480], t0);
        assert!(r.request(w(960, t0)).is_empty(), "half a request: wait");
        assert!(r.answer_ready(t0 + TX_HOLD_MAX - Duration::from_millis(1)).is_empty());
        let a = r.answer_ready(t0 + TX_HOLD_MAX);
        assert_eq!(a.len(), 1);
        assert_eq!((a[0].1[479], a[0].1[480]), (0.1, 0.0), "what there was, then silence");
        assert_eq!((r.over.silence_at_limit, r.over.held, r.over.held_max_ms), (1, 1, 100));
        // A full request behind an empty older one waits its turn.
        r.feed(&[0.2; 10], t0 + TX_HOLD_MAX);
        assert!(r.request(w(960, t0 + TX_HOLD_MAX)).is_empty());
        assert!(r.request(w(5, t0 + TX_HOLD_MAX)).is_empty(), "no overtaking");
    }

    /// The ring as it runs holds `TX_HOLD_MAX`, 100 ms by value (150 until build
    /// 258); another hold (the replay, build 255) pads at its own limit and not at
    /// 100. The low
    /// mark of the second pull is `TX_RING_LOW` unless the replay names another.
    #[test]
    fn the_hold_and_the_low_mark_as_they_run_and_as_the_replay_sets_them() {
        assert_eq!(TX_HOLD_MAX, Duration::from_millis(100));
        assert_eq!(TxRing::new().hold_max, TX_HOLD_MAX);
        let t0 = Instant::now();
        let mut r = TxRing::new().with_hold_max(Duration::from_millis(150));
        assert!(r.request(w(960, t0)).is_empty());
        assert_eq!(r.next_deadline(), Some(t0 + Duration::from_millis(150)));
        assert!(r.answer_ready(t0 + Duration::from_millis(149)).is_empty());
        assert_eq!(r.answer_ready(t0 + Duration::from_millis(150)).len(), 1, "padded at its own limit");
        assert_eq!(TX_RING_LOW, 2 * 960);
        assert!(pull_extra_for_ring_below(TX_RING_LOW, Some(TX_RING_LOW - 1), 5, 4));
        assert!(!pull_extra_for_ring_below(960, Some(TX_RING_LOW - 1), 5, 4), "a lower mark: no second pull");
        assert!(pull_extra_for_ring_below(960, Some(959), 5, 4));
    }

    /// Waiting requests are dropped when MOX ends - at the server's own TRX
    /// off and at Thetis's `trx:..,false` - and not at a new over of the ring
    /// inside one MOX (builds 195, 196).
    #[test]
    fn waiting_requests_drop_at_trx_off_and_live_through_a_new_over() {
        let body_count = |needle: &str| {
            let src = include_str!("tci.rs");
            sdr_remote_core::source_text::production_code(src).matches(needle).count()
        };
        let t0 = Instant::now();
        let mut r = TxRing::new();
        r.feed(&[0.1; 10], t0);
        r.request(w(960, t0));
        r.drop_waiting();
        assert_eq!((r.waiting.len(), r.over.dropped_waiting), (0, 1));
        assert!(r.answer_ready(t0 + Duration::from_secs(1)).is_empty(), "nothing goes out late");
        // A gap inside one MOX starts a new over of the ring, and the request
        // waiting then gets the new audio (build 196; review of 195).
        r.request(w(960, t0 + Duration::from_millis(200)));
        let rep = r.feed(&[0.2; 960], t0 + Duration::from_millis(250)).unwrap();
        assert_eq!(rep.stats.dropped_waiting, 1, "only the one at TRX off");
        let a = r.answer_ready(t0 + Duration::from_millis(250));
        assert_eq!(a.len(), 1);
        assert!(a[0].1.iter().all(|&x| x == 0.2), "answered with the new audio");
        assert_eq!((r.over.silence_at_limit, r.over.dropped_waiting), (0, 0));
        // The server's own TRX off drops them at once; the same count again
        // does nothing.
        r.request(w(960, t0 + Duration::from_millis(260)));
        r.note_trx_off(0);
        assert_eq!(r.waiting.len(), 1);
        r.note_trx_off(1);
        assert_eq!((r.waiting.len(), r.over.dropped_waiting), (0, 1));
        r.request(w(960, t0 + Duration::from_millis(270)));
        r.note_trx_off(1);
        assert_eq!(r.waiting.len(), 1, "no second drop on the same count");
        assert!(is_trx_off("TRX:0,false;") && is_trx_off("TRX:1,false;"));
        assert!(is_trx_off("trx:0,false;"), "ZZTX0 through send_cat (build 197)");
        assert!(!is_trx_off("TRX:0,true,tci;") && !is_trx_off("TRX:0,true;") && !is_trx_off("TRX:0;"));
        assert_eq!(body_count("tx_ring.note_trx_off(tx_off_count.load(Ordering::Relaxed));"), 2);
        assert_eq!(body_count("if is_trx_off(cmd) {"), 1);
        assert_eq!(body_count("self.tx_off_count.fetch_add(1, Ordering::Relaxed);"), 1);
        // And the count is read before anything is answered, at the top of the
        // loop and before a request (review of 196: after it, the limit's
        // silence went out first - the fault of 195).
        let src = include_str!("tci.rs");
        let body = &sdr_remote_core::source_text::production_code(src);
        let note = "tx_ring.note_trx_off(tx_off_count.load(Ordering::Relaxed));
";
        assert!(body.contains(&format!("        {}        send_tx_answers(&chrono_bin_tx, tx_ring.answer_ready(Instant::now()));", note)));
        assert!(body.contains(&format!("                        {}                        send_tx_answers(&chrono_bin_tx, tx_ring.request(waiting));", note)));
        assert_eq!(body_count("self.tx_off_count.clone(),"), 1);
        // The reader drops them on Thetis's own `trx:..,false`, answers through
        // `request`, and wakes for the limit.
        let src = include_str!("tci.rs");
        let body = &sdr_remote_core::source_text::production_code(src);
        assert_eq!(body.matches("if let TciNotification::Trx { active: false, .. } = notif {").count(), 1);
        assert_eq!(body.matches("tx_ring.drop_waiting();").count(), 1);
        assert_eq!(body.matches("send_tx_answers(&chrono_bin_tx, tx_ring.request(waiting));").count(), 1);
        assert_eq!(body.matches("send_tx_answers(&chrono_bin_tx, tx_ring.answer_ready(Instant::now()));").count(), 1);
        assert_eq!(body.matches("if deadline.is_some() => {").count(), 1);
        assert!(!body.contains("tx_ring.take("), "no answer around the waiting line");
    }

    /// The line's arguments in their order: every field of the report lands in
    /// its own place (build 193; review of 192).
    #[test]
    fn the_over_line_puts_every_field_in_its_place() {
        let stats = OverStats {
            first_ready: Some(48 * 101),
            lowest: Some(48 * 102),
            fed_after_take: 48 * 103,
            taken_after_feed: 48 * 104,
            padded_mid: 48 * 106,
            short_mid: 107,
            overflow: 48 * 108,
            feed_gaps: 109,
            feed_gap_late_ms: 110,
            largest_request: 48 * 111,
            first_request_at: None,
            lead_1s: Some(48 * 112),
            first_requests: [0, 1, 113, 0, 0, 0, 0, 0],
            first_requests_seen: 3,
            held: 114,
            held_max_ms: 115,
            silence_at_limit: 116,
            dropped_waiting: 117,
        };
        let line = over_line(&OverReport { stats, left: 48 * 105 });
        assert_eq!(
            line,
            "TX ring, previous over: 101 ms ready at Thetis's first request, lowest 102 ms after a request, \
             103 ms fed after its last request, 104 ms taken after the last audio, 105 ms left - dropped, \
             106 ms padded with silence before more audio came, in 107 request(s), 108 ms pushed out by newer audio, \
             109 feed gap(s) 110 ms late in all, requests up to 111 ms, lead 101 ms at the first request and 112 ms one second later, first requests at +0 +1 +113 ms, \
             held 114 request(s) up to 115 ms, 116 answered with silence at the 100 ms limit, 117 dropped at TRX off or a new over"
        );
        let none = over_line(&OverReport { stats: OverStats::default(), left: 0 });
        assert!(none.starts_with("TX ring, previous over: - ms ready") && none.ends_with("first requests at - ms, held 0 request(s) up to 0 ms, 0 answered with silence at the 100 ms limit, 0 dropped at TRX off or a new over"));
    }

    /// A second frame only when the ring stood below two frames and the jitter
    /// buffer holds more than its target; never without a reader (build 193).
    #[test]
    fn a_second_frame_only_from_the_excess_and_only_for_a_low_ring() {
        assert!(pull_extra_for_ring_below(TX_RING_LOW, Some(0), 5, 4));
        assert!(pull_extra_for_ring_below(TX_RING_LOW, Some(TX_RING_LOW - 1), 5, 4));
        assert!(!pull_extra_for_ring_below(TX_RING_LOW, Some(TX_RING_LOW), 5, 4), "two frames stand there: enough");
        assert!(!pull_extra_for_ring_below(TX_RING_LOW, Some(0), 4, 4), "the cushion stays");
        assert!(!pull_extra_for_ring_below(TX_RING_LOW, Some(0), 0, 3));
        assert!(!pull_extra_for_ring_below(TX_RING_LOW, None, 9, 3), "no reader");
        // Thetis's tick reads its ring once and hands it, with `TX_RING_LOW`,
        // to the one playout step (build 288); that step pulls at most twice
        // and counts the second. A PTT-only packet does not leave its loop
        // with `continue` (that would pull again without the bound).
        let net = include_str!("network.rs");
        assert_eq!(net.matches("let ring_at_tick = self.ptt.lock().await.tx_ring_level();").count(), 1);
        assert_eq!(net.matches("let ring = ring_at_tick.map(|level| (level as u64, crate::tci::TX_RING_LOW as u64));").count(), 1);
        assert_eq!(net.matches("crate::tx_recover::play_tick(&mut jitter_buf, &mut opus_decoder, &mut thetis_tx_loss, &mut pause_trim, Instant::now(), ring);").count(), 1);
        assert_eq!(net.matches("count_extra()").count(), 0, "the step counts it, not the loop");
        let step = include_str!("tx_recover.rs");
        let step = &step[step.find("pub fn play_tick(").unwrap()..step.find("pub async fn radio_tx_task(").unwrap()];
        assert_eq!(step.matches("for pull in 0..2 {").count(), 1);
        assert_eq!(step.matches("loss.count_extra();").count(), 1);
        assert!(step.contains("crate::tci::pull_extra_for_ring_below(lead as usize, Some(level as usize), jitter.depth(), jitter.target_depth())"));
        // And the level reaches the tick: the reader's ring publishes into the
        // connection's level, and the controller hands it on (review of 193:
        // a reader on `TxRing::new()` left the level at 0 and pulled twice on
        // every excess, unbounded; a connection without the level switched the
        // repair off - both stayed green).
        let src = include_str!("tci.rs");
        let body = &sdr_remote_core::source_text::production_code(src);
        assert_eq!(body.matches("self.tx_ring_level = Some(tx_ring_level.clone());").count(), 1);
        assert_eq!(body.matches("let mut tx_ring = TxRing::with_level(tx_ring_level);").count(), 1);
        assert!(!body.contains("let mut tx_ring = TxRing::new();"));
        assert_eq!(include_str!("ptt.rs").matches("self.tci.tx_ring_level()").count(), 1);
    }

    /// Silence handed out while the ring is empty counts as a gap only when
    /// more audio follows; the silence after the last audio, ended by a TRX
    /// off, is the over's end and does not count (build 183; the TRX off since
    /// build 280).
    #[test]
    fn padding_counts_as_a_gap_only_when_more_audio_follows() {
        let t0 = Instant::now();
        let mut r = TxRing::new();
        r.feed(&[0.1; 960], t0);
        r.take(960);
        r.take(480); // the ring is empty: 480 samples of silence
        r.take(480);
        // More audio within the over: the 960 padded samples were a gap.
        assert_eq!(r.feed(&[0.1; 960], t0 + Duration::from_millis(100)), None);
        r.take(960);
        // A second gap in the same over adds its own, not the first again
        // (review of 183, T6).
        r.take(240);
        assert_eq!(r.feed(&[0.1; 960], t0 + Duration::from_millis(150)), None);
        r.take(960);
        r.take(4800); // the end of the over: silence after the last audio
        r.note_trx_off(1);
        let rep = r.feed(&[0.2; 960], t0 + Duration::from_secs(5)).unwrap();
        assert_eq!((rep.stats.padded_mid, rep.stats.short_mid), (960 + 240, 3));
        // The next over starts clean.
        r.take(960);
        r.take(100);
        r.note_trx_off(2);
        let rep = r.feed(&[0.3; 10], t0 + Duration::from_secs(10)).unwrap();
        assert_eq!((rep.stats.padded_mid, rep.stats.short_mid), (0, 0), "only silence after the last audio");
    }

    /// A gap of TX_OVER_GAP or more inside one MOX starts a new over in the
    /// ring, and the silence handed out in it still counts, in the over it
    /// fell in; with a TRX off between it is that over's end (build 280; the
    /// ride of 2026-10-22: ~21 of ~23 s of silence counted nowhere). Both
    /// ways, and twice without a new ring.
    #[test]
    fn silence_in_a_gap_that_splits_the_over_still_counts() {
        let t0 = Instant::now();
        let mut r = TxRing::new();
        r.feed(&[0.1; 960], t0);
        let mut at = t0;
        for (trx_off, pads, expect) in [(0, 3, 3 * 960), (1, 2, 0), (1, 4, 4 * 960), (2, 1, 0)] {
            r.take(960);
            for _ in 0..pads {
                r.take(960); // the ring is empty: Thetis gets silence
            }
            r.note_trx_off(trx_off);
            at += TX_OVER_GAP + Duration::from_millis(100);
            let rep = r.feed(&[0.1; 960], at).expect("the gap starts a new over");
            assert_eq!(
                (rep.stats.padded_mid, rep.stats.short_mid),
                (expect, if expect > 0 { pads as u32 } else { 0 }),
                "{pads} padded request(s), TRX off count {trx_off}"
            );
        }
    }

    /// `take` pads with zeros; a chunk bigger than the ring keeps its newest part
    /// (round 104, N7) - and ends: run on a thread with a bounded wait, so a
    /// return of the endless trim loop turns this red instead of hanging the
    /// suite (round 104, the same lesson as `run_writer_bounded`).
    #[test]
    fn take_pads_and_an_oversize_chunk_keeps_its_newest_part() {
        let t0 = Instant::now();
        let mut r = TxRing::new();
        r.feed(&[0.3; 10], t0);
        let a = r.take(20);
        assert_eq!((a[9], a[10]), (0.3, 0.0));
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut r = TxRing::new();
            let mut big = vec![0.5f32; TX_RING_CAPACITY + 100];
            big[TX_RING_CAPACITY + 99] = 0.9; // the newest sample
            big[0] = 0.1; // the oldest, which must go
            r.feed(&big, Instant::now());
            let _ = done_tx.send((r.ring.len(), *r.ring.front().unwrap(), *r.ring.back().unwrap()));
        });
        let (len, front, back) = done_rx.recv_timeout(Duration::from_secs(3)).expect("an oversize chunk must not hang the reader");
        assert_eq!(len, TX_RING_CAPACITY);
        assert_eq!((front, back), (0.5, 0.9), "the newest part is kept");
        // What is pushed out is counted: the cut of an oversize chunk and the
        // oldest samples a full ring gives up (build 185).
        let t0 = Instant::now();
        let mut r = TxRing::new();
        r.feed(&vec![0.1; TX_RING_CAPACITY - 10], t0);
        r.feed(&[0.2; 30], t0);
        assert_eq!(r.over.overflow, 20, "the oldest 20 pushed out");
        r.feed(&vec![0.3; TX_RING_CAPACITY + 5], t0);
        assert_eq!(r.over.overflow, 20 + 5 + TX_RING_CAPACITY, "the cut and the whole ring before it");
    }

    /// The three paths that feed the ring all go through `TxRing` (regel 15).
    #[test]
    fn every_tx_audio_path_feeds_the_one_ring() {
        let src = include_str!("tci.rs");
        let body = &sdr_remote_core::source_text::production_code(src);
        let lines: Vec<&str> = body.lines().map(str::trim).collect();
        assert_eq!(lines.iter().filter(|l| **l == "tx_ring.feed_and_report(&samples);").count(), 3);
        assert!(!body.contains("tx_ring.extend("), "no path around the ring");
        assert!(body.contains("TX ring, previous over: {} ms ready at Thetis's first request"));
        assert!(body.contains("info!(\"{}\", over_line(&r));"), "the over line goes out");
    }
}

/// The first report of a transmitter state (TRX, TUNE) for each receiver on one
/// TCI connection (builds 213, 214). Thetis reports both receivers when a
/// client connects. If the first report for a receiver says "on" and this
/// server did not switch it on, it was switched on through a connection that
/// is gone - stock Thetis keeps a TCI client's PTT and TUNE when its connection
/// drops (`StopSocketListener` leaves `console.TCIPTT` and `TUN` set), and the
/// fork clears them only since TL2-6 and TL2-7. Later reports are Thetis's own
/// (footswitch, console) and are not touched; nor is a first report of
/// something this server has on.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct FirstReport {
    seen: [bool; 2],
    /// One of the first reports said "on" (build 218).
    first_on: bool,
}

/// The receiver and the state of an outgoing `tune:<r>,<true|false>` command,
/// in any case, as `send` sees it (build 214).
fn tune_command(cmd: &str) -> Option<(usize, bool)> {
    let body = cmd.trim().trim_end_matches(';');
    let (head, args) = body.split_once(':')?;
    if !head.eq_ignore_ascii_case("tune") {
        return None;
    }
    let (receiver, state) = args.split_once(',')?;
    let receiver: usize = receiver.trim().parse().ok().filter(|r| *r < 2)?;
    match state.trim().to_ascii_lowercase().as_str() {
        "true" => Some((receiver, true)),
        "false" => Some((receiver, false)),
        _ => None,
    }
}

/// What this server wants TUNE to be (builds 214-216): set by every
/// `tune:<r>,<bool>` it sends, and cleared when Thetis reports TUNE off. One
/// flag, not one per receiver: Thetis has one `TUN` and ignores the receiver of
/// a `tune:` (`handleTune`), and it reports TUNE on receiver 1 when VFO B
/// transmits with RX2 on, while we always send `tune:0` (review of 215).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct TuneIntent {
    on: bool,
}

impl TuneIntent {
    /// An outgoing command; anything that is not a `tune:` changes nothing.
    fn sent(&mut self, cmd: &str) {
        if let Some((_receiver, on)) = tune_command(cmd) {
            self.on = on;
        }
    }

    /// Thetis reports TUNE off, on whichever receiver.
    fn reported_off(&mut self) {
        self.on = false;
    }

    fn on(&self) -> bool {
        self.on
    }
}

/// One TUNE report from Thetis, without the connection around it (build 217).
/// True when it is a TUNE left behind, to be answered with "off".
///
/// An "off" clears what we asked only when it is not the first report on its
/// receiver: at connect Thetis reports both receivers, 0 then 1, and with VFO B
/// and RX2 that is `tune:0,false` then `tune:1,true` - the snapshot "off" on 0
/// cleared our own TUNE, and the "on" on 1 then read as left behind (review of
/// 216). A later "off" is Thetis's own (its button, a time-out, a SWR trip).
///
/// When the first reports of both receivers are in and neither said "on", TUNE
/// is off in Thetis, whatever we asked before (review of 217): Thetis reports
/// both at connect, always (`sendTune(0, ..)` and `sendTune(1, ..)`).
fn tune_report(first: &mut FirstReport, intent: &mut TuneIntent, receiver: u32, active: bool) -> bool {
    let was_first = !first.seen(receiver);
    if !active && !was_first {
        intent.reported_off();
    }
    let left_behind = first.left_behind(receiver, active, intent.on());
    // Only on the report that completes the pair: later reports are covered above.
    if was_first && first.seen(0) && first.seen(1) && !first.first_on {
        intent.reported_off();
    }
    left_behind
}

impl FirstReport {
    /// A report on this receiver came in already on this connection.
    fn seen(&self, receiver: u32) -> bool {
        self.seen.get(receiver as usize).copied().unwrap_or(false)
    }

    /// True when this report is a PTT or TUNE left behind and should be
    /// answered with "off" on its receiver.
    fn left_behind(&mut self, receiver: u32, active: bool, we_transmit: bool) -> bool {
        let Some(seen) = self.seen.get_mut(receiver as usize) else {
            return false;
        };
        let first = !*seen;
        *seen = true;
        self.first_on |= first && active;
        first && active && !we_transmit
    }
}

/// The commands that stop a TX Thetis was already making on a position that
/// became RX-only (build 220). `TRX:<r>,false` on the receiver Thetis named
/// (`TRX:0` would move a VFO-B transmitter to VFO A); in Thetis it only drops a
/// TCI PTT (`TCIPTT = false`), so a MOX by the spacebar, the button or a
/// footswitch goes on - with stock Thetis that stays the warning of 215. A TUNE
/// does stop: `tune:<r>,false` sets `TUN = false`, whoever started it.
fn rx_only_stop_commands(trx_receiver: u32, tune_rx: [bool; 2]) -> Vec<String> {
    let mut cmds = vec![format!("TRX:{},false;", trx_receiver)];
    for (rx, on) in tune_rx.iter().enumerate() {
        if *on {
            cmds.push(format!("tune:{},false;", rx));
        }
    }
    cmds
}

#[cfg(test)]
mod first_trx_tests {
    use super::{tune_command, tune_report, FirstReport as FirstTrx, TuneIntent};

    /// Only the first report per receiver counts, only "transmitting", and not
    /// while this server transmits; a new connection starts over (build 213).
    #[test]
    fn a_ptt_left_behind_is_the_first_report_saying_transmit_while_we_do_not() {
        let mut f = FirstTrx::default();
        assert!(f.left_behind(0, true, false), "keyed for nobody at connect");
        assert!(!f.left_behind(0, true, false), "a second report is Thetis's own");
        assert!(!f.left_behind(1, false, false), "receiver 1 is not transmitting");
        assert!(!f.left_behind(1, true, false), "receiver 1's first report was already seen");
        let mut f = FirstTrx::default();
        assert!(!f.left_behind(0, true, true), "we hold it ourselves");
        assert!(!f.left_behind(0, true, false), "and it was the first");
        assert!(FirstTrx::default().left_behind(1, true, false), "receiver 1 alone");
        assert!(!FirstTrx::default().left_behind(2, true, false), "no receiver 2");
        // Wired where the loop runs, which no test drives: the check in the TRX
        // arm with this server's own state, the answer on that receiver, and a
        // fresh start on every disconnect.
        let src = include_str!("tci.rs");
        let body = &src[..src.find("#[cfg(test)]\nmod first_trx_tests").unwrap()];
        assert_eq!(
            body.matches("if self.first_trx.left_behind(receiver, active, self.tx_active || self.we_tune.on()) {").count(),
            1
        );
        assert_eq!(body.matches("self.send(&format!(\"TRX:{},false;\", receiver)).await;").count(), 2, "the PTT left behind and the RX-only gate");
        assert_eq!(body.matches("self.first_trx = FirstReport::default();").count(), 1);
    }

    /// A TUNE left behind is the same check, against what this server sent
    /// itself; `send` reads every outgoing `tune:` in any case (build 214).
    #[test]
    fn a_tune_left_behind_is_the_first_report_saying_tune_while_we_did_not_ask() {
        assert_eq!(tune_command("tune:0,true;"), Some((0, true)));
        assert_eq!(tune_command("TUNE:1,false;"), Some((1, false)));
        assert_eq!(tune_command("  Tune:0,TRUE"), Some((0, true)));
        assert_eq!(tune_command("tune:2,true;"), None, "no receiver 2");
        assert_eq!(tune_command("tune:0;"), None, "a query");
        assert_eq!(tune_command("tune_drive:0,50;"), None);
        assert_eq!(tune_command("TRX:0,true,tci;"), None);
        let mut f = FirstTrx::default();
        assert!(f.left_behind(0, true, false), "tuning for nobody at connect");
        assert!(!f.left_behind(0, true, false), "a second report is Thetis's own");
        assert!(!FirstTrx::default().left_behind(0, true, true), "we asked for it");
        // Wired where the loop runs: the record in `send`, the check in the TUNE
        // arm against it, the answer on that receiver, a fresh start on every
        // disconnect - and the RX-only gate on its receiver too.
        // The intent: on, off, and off when Thetis says so - and anything that is
        // not a TUNE leaves it alone (review of 214: "never record an off" stayed green).
        let mut w = TuneIntent::default();
        w.sent("tune:0,true;");
        assert!(w.on());
        w.sent("TRX:1,false;");
        assert!(w.on(), "not a TUNE");
        w.sent("tune:0,false;");
        assert!(!w.on(), "our own off");
        // One TUN in Thetis: an off reported on receiver 1 (VFO B transmitting
        // with RX2 on) clears what we asked with `tune:0` (review of 215).
        w.sent("tune:0,true;");
        w.reported_off();
        assert!(!w.on(), "Thetis says it is off");
        let src = include_str!("tci.rs");
        let body = &src[..src.find("#[cfg(test)]\nmod first_trx_tests").unwrap()];
        // Recorded before the connection is checked (review of 214).
        let send = &body[body.find("pub async fn send(&mut self, cmd: &str) {").unwrap()..];
        let recorded = send.find("self.we_tune.sent(cmd);").expect("recorded in send");
        let checked = send.find("if !self.connected {").unwrap();
        assert!(recorded < checked, "the TUNE is recorded only while connected");
        assert_eq!(body.matches("self.we_tune.sent(cmd);").count(), 1);
        assert_eq!(body.matches("intent.reported_off();").count(), 2, "only in tune_report");
        assert_eq!(
            body.matches("if tune_report(&mut self.first_tune, &mut self.we_tune, receiver, active) {").count(),
            1
        );
        assert_eq!(body.matches("first.left_behind(receiver, active, intent.on())").count(), 1);
        assert_eq!(
            body.matches("if self.first_trx.left_behind(receiver, active, self.tx_active || self.we_tune.on()) {").count(),
            1
        );
        assert_eq!(body.matches("self.send(&format!(\"tune:{},false;\", receiver)).await;").count(), 1);
        assert_eq!(body.matches("self.first_tune = FirstReport::default();").count(), 1);
        assert_eq!(body.matches("self.send(\"TRX:0,false;\")").count(), 0, "a TRX off on receiver 0 by hand");
        assert_eq!(body.matches("self.send(&format!(\"TRX:{},false;\", receiver)).await;").count(), 2);
    }
}

#[cfg(test)]
mod tune_report_tests {
    use super::{tune_report, FirstReport, TuneIntent};

    /// Our own TUNE survives the snapshot Thetis sends when we connect, with
    /// VFO A and with VFO B, and a later "off" still clears it (review of 216).
    #[test]
    fn the_connect_snapshot_does_not_clear_our_own_tune() {
        // VFO B with RX2: tune:0,false then tune:1,true.
        let mut first = FirstReport::default();
        let mut intent = TuneIntent::default();
        intent.sent("tune:0,true;");
        assert!(!tune_report(&mut first, &mut intent, 0, false), "nothing to answer");
        assert!(intent.on(), "the snapshot off on 0 is not Thetis turning it off");
        assert!(!tune_report(&mut first, &mut intent, 1, true), "ours, not left behind");
        assert!(intent.on());
        // VFO A: tune:0,true then tune:1,false.
        let mut first = FirstReport::default();
        let mut intent = TuneIntent::default();
        intent.sent("tune:0,true;");
        assert!(!tune_report(&mut first, &mut intent, 0, true), "ours");
        assert!(!tune_report(&mut first, &mut intent, 1, false));
        assert!(intent.on(), "the snapshot off on 1 is not Thetis turning it off");
        // During operation: Thetis turns it off (button, time-out, SWR trip).
        assert!(!tune_report(&mut first, &mut intent, 0, false));
        assert!(!intent.on(), "a later off clears it");
        // A TUNE left behind at connect while we asked nothing: answered.
        let mut first = FirstReport::default();
        let mut intent = TuneIntent::default();
        assert!(!tune_report(&mut first, &mut intent, 0, false));
        assert!(tune_report(&mut first, &mut intent, 1, true), "left behind on receiver 1");
        // A second connection starts over: the first report is a snapshot again.
        let mut intent = TuneIntent::default();
        intent.sent("tune:0,true;");
        let mut first = FirstReport::default();
        assert!(!tune_report(&mut first, &mut intent, 0, false));
        assert!(intent.on(), "second connection, same snapshot rule");
        // Both first reports off: TUNE is off in Thetis (review of 217) - in
        // either order, and only once both are in.
        assert!(!tune_report(&mut first, &mut intent, 1, false));
        assert!(!intent.on(), "both receivers said off at connect");
        let mut intent = TuneIntent::default();
        intent.sent("tune:0,true;");
        let mut first = FirstReport::default();
        assert!(!tune_report(&mut first, &mut intent, 1, false));
        assert!(intent.on(), "one receiver is not both");
        assert!(!tune_report(&mut first, &mut intent, 0, false));
        assert!(!intent.on(), "1 then 0, both off");
        // And after that we switch TUNE on ourselves: Thetis's "on" leaves it on.
        intent.sent("tune:0,true;");
        assert!(!tune_report(&mut first, &mut intent, 0, true), "ours, a later report");
        assert!(intent.on(), "a later on does not clear what we asked");
        // A third connection on the same intent, sent while disconnected.
        intent.sent("tune:0,true;");
        let mut first = FirstReport::default();
        assert!(!tune_report(&mut first, &mut intent, 0, true));
        assert!(!tune_report(&mut first, &mut intent, 1, false));
        assert!(intent.on(), "one of them said on");
    }
}

#[cfg(test)]
mod tune_tests {
    use super::merge_tune;

    /// A TUNE OFF on one receiver does not clear a TUNE ON on the other, in
    /// either order, and an unknown receiver changes nothing.
    #[test]
    fn tune_on_either_receiver_holds_until_both_are_off() {
        let mut rx = [false; 2];
        assert!(!merge_tune(&mut rx, 0, false), "quiet");
        assert!(merge_tune(&mut rx, 1, true), "VFO B tuning");
        assert!(merge_tune(&mut rx, 0, false), "a server TUNE OFF on rx0 does not end it");
        assert!(!merge_tune(&mut rx, 1, false), "until rx1 says so");
        assert!(merge_tune(&mut rx, 0, true), "rx0 tuning");
        assert!(merge_tune(&mut rx, 1, false), "rx1 off does not end it");
        assert!(!merge_tune(&mut rx, 0, false));
        assert!(!merge_tune(&mut rx, 7, true), "a receiver Thetis does not have changes nothing");
        assert_eq!(rx, [false, false]);
    }
}

#[cfg(test)]
mod drop_reason_tests {
    use super::*;

    /// A full queue and a gone loop are told apart in the line (build 151;
    /// the test came with 152).
    #[test]
    fn a_dropped_frame_says_why() {
        let (tx, rx) = mpsc::channel::<u8>(1);
        tx.try_send(1).unwrap();
        assert_eq!(drop_reason(&tx.try_send(2).unwrap_err()), "queue full");
        drop(rx);
        assert_eq!(drop_reason(&tx.try_send(3).unwrap_err()), "the receive loop is gone");
    }
}

#[cfg(test)]
mod ctun_tx_tests {
    use super::TciConnection;

    /// "Transmitting" for the automatic recenter has one definition: our own
    /// PTT, Thetis's TRX, or Tune (build 274; owner's decision 2026-10-20,
    /// "ja, Tune ook"). The end of Tune lets recentering go on, as the end of
    /// a transmission does.
    #[test]
    fn tune_holds_the_recenter_like_transmitting() {
        let mut t = TciConnection::new(None);
        assert!(!t.transmitting_for_ctun());
        t.tune_active = true;
        assert!(t.transmitting_for_ctun(), "Tune is a carrier");
        assert!(!t.detect_ptt_off_transition());
        t.tune_active = false;
        assert!(t.detect_ptt_off_transition(), "the end of Tune");
        assert!(!t.detect_ptt_off_transition(), "once");
        for (ours, trx, tune) in [(true, false, false), (false, true, false), (false, false, true)] {
            let mut t = TciConnection::new(None);
            t.tx_active = ours;
            t.thetis_tx_active = trx;
            t.tune_active = tune;
            assert!(t.transmitting_for_ctun(), "{ours} {trx} {tune}");
        }
        let src = include_str!("tci.rs");
        let src = &src[..src.find("#[cfg(test)]\nmod ctun_tx_tests").unwrap()];
        assert_eq!(src.matches("self.tx_active || self.thetis_on_the_air()").count(), 1, "ours, or Thetis on the air");
        assert_eq!(src.matches(concat!("self.thetis_tx_active", " || self.tune_active")).count(), 1, "spelled out once");
        assert_eq!(src.matches("let any_tx = self.transmitting_for_ctun();").count(), 2, "RX1 and RX2");
        assert_eq!(src.matches("let any_tx_now = self.transmitting_for_ctun();").count(), 1, "the transition");
        // Private since build 276 (round 275): outside this file nobody can
        // spell "on the air" out of the fields; the compiler keeps it.
        assert_eq!(src.matches(concat!("pub ", "thetis_tx_active: bool")).count(), 0);
        assert_eq!(src.matches(concat!("pub ", "tune_active: bool")).count(), 0);
        assert!(src.contains("    thetis_tx_active: bool,") && src.contains("    tune_active: bool,"));
        // The PA reads both apart, by one reader.
        let mut t = TciConnection::new(None);
        t.thetis_tx_active = true;
        assert_eq!(t.thetis_trx_and_tune(), (true, false));
        t.tune_from_us(1, true);
        assert_eq!(t.thetis_trx_and_tune(), (true, true), "our own Tune, through the same merge");
        t.tune_from_us(0, false);
        assert!(t.tune_active, "an off on receiver 0 leaves receiver 1's Tune");
    }
}
