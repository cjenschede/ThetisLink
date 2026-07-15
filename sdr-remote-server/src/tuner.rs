// SPDX-License-Identifier: GPL-2.0-or-later

//! StockCorner tuner driver via Adafruit MCP2221A breakout (replaces the
//! legacy serial-port RTS/CTS flow).
//!
//! Each physical tuner is a [`TunerInstance`]: it owns one MCP2221A bridge
//! plus a background thread that runs the tune sequence. Up to two instances
//! live inside a [`Tuners`] collection - JC-4s and JC-3s share an identical
//! control protocol, so the only per-instance difference is the cosmetic
//! `TunerModel` label, the target MCP2221A USB serial and the optional
//! Amplitec-A position the tuner sits behind.
//!
//! Tune-sequence (per instance) - feedback-driven via the GP1 ADC tune-status
//! line, preserving the PA-standby orchestration from the build-48 flow:
//!   1. PA standby (SPE / RF2K) if either is in Operate.
//!   2. GP2 HIGH  - assert start-button and HOLD until the JC-Control
//!                  acknowledges the press (= yellow drops below
//!                  `threshold - hyst/2`). No ACK within 3 s -> `TUNER_TIMEOUT`.
//!   3. GP2 LOW   - release the start-button now that the tuner has ACK'd.
//!   4. ZZTU1     - Thetis carrier ON, tuner consumes RF to do its job.
//!   5. Wait for the yellow line to go back above `threshold + hyst/2`
//!      (LED off, tune cycle complete). 30 s hard cap.
//!   6. ZZTU0     - Thetis carrier OFF.
//!   7. PA operate restore.
//!
//! Threshold rationale (1 MΩ + 1 MΩ 1:1 divider on the yellow tune-status
//! wire - see `docs/internal/referentie/MCP2221A-JC4s-wiring.md`):
//!   - idle yellow ≈ 4.5 V
//!   - tune-LED on yellow ≈ 0 V
//! Operator sets the switch threshold (default 2.25 V) and hysteresis
//! (default 0.50 V) per-tuner via the Status panel.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{info, warn};

use crate::config::{TunerConfig, TunerModel};
use crate::mcp2221_debug::{Mcp2221Debug, Status as BridgeStatus};
use crate::rf2k::Rf2k;
use crate::spe_expert::SpeExpert;

#[derive(Clone, Debug)]
pub struct TunerStatus {
    /// 0=Idle, 1=Tuning, 2=DoneOk, 3=Timeout, 4=Aborted
    pub state: u8,
    pub connected: bool,
    /// True if DONE_OK but VFO moved >25 kHz from tuned freq
    /// (network tick sets/clears it).
    pub stale: bool,
}

impl Default for TunerStatus {
    fn default() -> Self {
        Self { state: TUNER_IDLE, connected: false, stale: false }
    }
}

pub const TUNER_IDLE: u8 = 0;
pub const TUNER_TUNING: u8 = 1;
pub const TUNER_DONE_OK: u8 = 2;
pub const TUNER_TIMEOUT: u8 = 3;
pub const TUNER_ABORTED: u8 = 4;

/// Hard tune-active-search timeout. If the JC-Control never pulls the yellow
/// line low after asserting GP2, give up after this and report TIMEOUT.
const ACTIVE_SEARCH_TIMEOUT_SECS: u64 = 3;
/// Hard cap on total tune duration; once the LED has been seen ON, the
/// JC-Control should complete within seconds - 30 s is the build-48 number.
const TUNE_COMPLETE_TIMEOUT_SECS: u64 = 30;
/// ADC poll period during the tune sequence.
const ADC_POLL_INTERVAL_MS: u64 = 25;
/// Edge transitions (active/idle) require this many consecutive samples
/// before being believed - single-sample noise rejection on top of the
/// hysteresis window.
const EDGE_CONSECUTIVE: usize = 2;

#[derive(Debug)]
pub enum TunerCmd {
    StartTune,
    AbortTune,
}

/// Backwards-compat alias for callers that pre-date multi-tuner support.
/// New code should use [`TunerInstance`] (and [`Tuners`] for the collection).
pub type Jc4sTuner = TunerInstance;

/// A single physical StockCorner tuner driven through one MCP2221A board.
/// Constructed via [`TunerInstance::new`]; held inside [`Tuners`].
///
/// (Renamed from `Jc4sTuner` once JC-3s support landed - the protocol is
/// identical so one struct covers both models. `TunerModel` is just a label.)
pub struct TunerInstance {
    cmd_tx: mpsc::Sender<TunerCmd>,
    status: Arc<Mutex<TunerStatus>>,
    bridge: Arc<Mcp2221Debug>,
    model: TunerModel,
    slot_index: usize,
    label: String,
}

impl TunerInstance {
    /// Open the MCP2221A board (by USB serial - empty string falls back to
    /// "first available board"), spawn the worker thread.
    pub fn new(
        slot_index: usize,
        config: TunerConfig,
        cat_tx: tokio::sync::mpsc::Sender<String>,
        spe: Option<Arc<SpeExpert>>,
        rf2k: Option<Arc<Rf2k>>,
    ) -> Result<Self, String> {
        // Operator-friendly label uses the USB serial (e.g. "JC-4s loop") rather
        // than the cosmetic model enum, because the serial is what the operator
        // actively chose when programming each Adafruit board and is unique
        // per physical tuner.
        let display_name = if config.mcp_serial.is_empty() {
            config.model.label().to_string()
        } else {
            config.mcp_serial.clone()
        };
        let label = format!("Tuner{} ({})", slot_index + 1, display_name);
        let target_serial = if config.mcp_serial.is_empty() {
            None
        } else {
            Some(config.mcp_serial.clone())
        };
        let bridge = Mcp2221Debug::with_target_serial(target_serial);
        // Apply persisted tune-detector thresholds before the first sample
        // is taken so the bridge uses the operator's saved switch level
        // immediately.
        bridge.set_threshold_v(config.threshold_v);
        bridge.set_hysteresis_v(config.hysteresis_v);
        bridge.reconnect();
        let initial = bridge.snapshot();
        let connected = matches!(initial.status, BridgeStatus::Connected);
        if !connected {
            warn!(
                "{}: MCP2221A not yet connected at init ({:?}). Will retry on tune.",
                label, initial.status
            );
        }

        let (cmd_tx, cmd_rx) = mpsc::channel::<TunerCmd>();
        let status = Arc::new(Mutex::new(TunerStatus {
            state: TUNER_IDLE,
            connected,
            stale: false,
        }));

        let status_for_thread = status.clone();
        let bridge_for_thread = bridge.clone();
        let label_for_thread = label.clone();

        std::thread::Builder::new()
            .name(format!("tuner-{}", slot_index + 1))
            .spawn(move || {
                tuner_thread(
                    cmd_rx,
                    status_for_thread,
                    bridge_for_thread,
                    cat_tx,
                    spe,
                    rf2k,
                    label_for_thread,
                );
            })
            .map_err(|e| format!("Failed to spawn tuner thread: {}", e))?;

        Ok(Self {
            cmd_tx,
            status,
            bridge,
            model: config.model,
            slot_index,
            label,
        })
    }

    pub fn send_command(&self, cmd: TunerCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn status(&self) -> TunerStatus {
        self.status.lock().unwrap().clone()
    }

    /// Reset DONE_OK state back to IDLE (e.g. after band change).
    pub fn reset_done(&self) {
        let mut s = self.status.lock().unwrap();
        if s.state == TUNER_DONE_OK {
            s.state = TUNER_IDLE;
        }
    }

    /// Set stale flag (VFO moved >25 kHz from tuned freq - driven by network tick).
    pub fn set_stale(&self, stale: bool) {
        self.status.lock().unwrap().stale = stale;
    }

    pub fn model(&self) -> TunerModel {
        self.model
    }

    pub fn slot_index(&self) -> usize {
        self.slot_index
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// Underlying MCP2221A bridge - exposed so the UI can render per-tuner
    /// debug info (GP2 toggle / GP1 ADC) without going through the worker
    /// thread.
    pub fn bridge(&self) -> &Arc<Mcp2221Debug> {
        &self.bridge
    }
}

/// Collection of all enabled tuners (0, 1 or 2). Constructed from
/// `ServerConfig::tuners`; routes the client's Tune button to the right
/// instance based on the active Amplitec-A position.
pub struct Tuners {
    instances: Vec<Arc<TunerInstance>>,
    amplitec_mappings: Vec<Option<u8>>,
}

impl Tuners {
    /// Construct from config. Each enabled slot tries to open its MCP2221A;
    /// failures are logged and the slot is skipped (we don't fail the whole
    /// server start because one tuner is unplugged).
    pub fn new(
        configs: &[TunerConfig],
        cat_tx: tokio::sync::mpsc::Sender<String>,
        spe: Option<Arc<SpeExpert>>,
        rf2k: Option<Arc<Rf2k>>,
    ) -> Self {
        let mut instances = Vec::new();
        let mut amplitec_mappings = Vec::new();
        for (i, cfg) in configs.iter().enumerate() {
            if !cfg.enabled {
                continue;
            }
            match TunerInstance::new(i, cfg.clone(), cat_tx.clone(), spe.clone(), rf2k.clone()) {
                Ok(inst) => {
                    info!(
                        "Tuner{} ({}) ready, MCP serial=\"{}\", amplitec_pos={:?}",
                        i + 1,
                        cfg.model.label(),
                        cfg.mcp_serial,
                        cfg.amplitec_pos
                    );
                    instances.push(Arc::new(inst));
                    amplitec_mappings.push(cfg.amplitec_pos);
                }
                Err(e) => {
                    warn!("Tuner{} init failed: {}", i + 1, e);
                }
            }
        }
        Self { instances, amplitec_mappings }
    }

    pub fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }

    pub fn instances(&self) -> &[Arc<TunerInstance>] {
        &self.instances
    }

    /// First enabled tuner. Used as the legacy/default reference by code that
    /// pre-dates multi-tuner support (macros, settings UI, single-tuner
    /// status panel). Returns None when no tuner is enabled.
    pub fn primary(&self) -> Option<Arc<TunerInstance>> {
        self.instances.first().cloned()
    }

    /// Find the tuner bound to a specific Amplitec-A position. Used by the
    /// network handler when the user presses Tune: pick the tuner that
    /// physically sits behind the active antenna.
    pub fn for_amplitec_pos(&self, pos: u8) -> Option<Arc<TunerInstance>> {
        for (i, mapping) in self.amplitec_mappings.iter().enumerate() {
            if *mapping == Some(pos) {
                return self.instances.get(i).cloned();
            }
        }
        None
    }
}

// ============================================================================
// Worker thread + tune sequence
// ============================================================================

fn set_state(status: &Arc<Mutex<TunerStatus>>, state: u8) {
    status.lock().unwrap().state = state;
}

fn check_abort(cmd_rx: &mpsc::Receiver<TunerCmd>) -> bool {
    matches!(cmd_rx.try_recv(), Ok(TunerCmd::AbortTune))
}

/// Reset state to Idle after 3 s - same heuristic as build-48 so the UI
/// banner clears itself after the user has had a chance to read it.
fn schedule_idle_reset(status: &Arc<Mutex<TunerStatus>>) {
    let status = status.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(3));
        let mut s = status.lock().unwrap();
        if s.state != TUNER_TUNING && s.state != TUNER_IDLE {
            s.state = TUNER_IDLE;
        }
    });
}

fn tuner_thread(
    cmd_rx: mpsc::Receiver<TunerCmd>,
    status: Arc<Mutex<TunerStatus>>,
    bridge: Arc<Mcp2221Debug>,
    cat_tx: tokio::sync::mpsc::Sender<String>,
    spe: Option<Arc<SpeExpert>>,
    rf2k: Option<Arc<Rf2k>>,
    label: String,
) {
    info!("{}: thread started", label);

    // Auto-reconnect throttle: when the USB cable / Adafruit board drops out,
    // retry every `RECONNECT_INTERVAL_SECS` so a re-plug recovers without the
    // operator having to restart the server. Reset to `None` on every successful
    // connection so the next disconnect retries immediately.
    const RECONNECT_INTERVAL_SECS: u64 = 5;
    let mut last_reconnect_at: Option<Instant> = None;

    loop {
        match cmd_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(TunerCmd::StartTune) => {
                // Ensure the bridge is open before driving GP2/GP1; reconnect
                // transparently if the cable was re-plugged since init.
                let snap = bridge.snapshot();
                if !matches!(snap.status, BridgeStatus::Connected) {
                    bridge.reconnect();
                    last_reconnect_at = Some(Instant::now());
                }
                let post = bridge.snapshot();
                if !matches!(post.status, BridgeStatus::Connected) {
                    warn!(
                        "{}: tune requested but MCP2221A not connected ({:?})",
                        label, post.status
                    );
                    status.lock().unwrap().connected = false;
                    set_state(&status, TUNER_TIMEOUT);
                    schedule_idle_reset(&status);
                    continue;
                }
                status.lock().unwrap().connected = true;
                run_tune_sequence(&bridge, &cmd_rx, &status, &cat_tx, &spe, &rf2k, &label);
            }
            Ok(TunerCmd::AbortTune) => {
                // Idle abort - nothing to cancel.
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Idle tick: refresh connected flag from the bridge so the UI
                // shows a red dot when the board is unplugged, and attempt a
                // throttled auto-reconnect while we're disconnected.
                let snap = bridge.snapshot();
                let connected = matches!(snap.status, BridgeStatus::Connected);
                {
                    let mut s = status.lock().unwrap();
                    if s.connected != connected {
                        info!("{}: bridge connected={}", label, connected);
                    }
                    s.connected = connected;
                }
                if connected {
                    last_reconnect_at = None;
                } else {
                    let due = match last_reconnect_at {
                        None => true,
                        Some(t) => t.elapsed() >= Duration::from_secs(RECONNECT_INTERVAL_SECS),
                    };
                    if due {
                        info!(
                            "{}: bridge disconnected - attempting reconnect",
                            label
                        );
                        bridge.reconnect();
                        last_reconnect_at = Some(Instant::now());
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                info!("{}: command channel closed, stopping", label);
                break;
            }
        }
    }

    // Defensive: make sure GP2 is LOW on exit so the JC-Control does not see
    // a held "start" line during server shutdown.
    bridge.set_gp2(false);
    status.lock().unwrap().connected = false;
    info!("{}: thread stopped", label);
}

/// Drive a full tune-cycle. See module-level docs for the step sequence.
fn run_tune_sequence(
    bridge: &Arc<Mcp2221Debug>,
    cmd_rx: &mpsc::Receiver<TunerCmd>,
    status: &Arc<Mutex<TunerStatus>>,
    cat_tx: &tokio::sync::mpsc::Sender<String>,
    spe: &Option<Arc<SpeExpert>>,
    rf2k: &Option<Arc<Rf2k>>,
    label: &str,
) {
    info!("{}: starting tune sequence", label);
    set_state(status, TUNER_TUNING);

    // Step 0 - PA(s) to Standby.
    let restore_spe = safe_tune_standby(spe, label);
    let restore_rf2k = safe_tune_standby_rf2k(rf2k, label);
    if restore_spe || restore_rf2k {
        info!("{}: waiting 500ms for PA settle after standby", label);
        std::thread::sleep(Duration::from_millis(500));
    }

    // Step 1 - GP2 HIGH, HOLD until JC-Control acknowledges the press.
    // Feedback-driven: no fixed 150 ms wait - we keep the start-button
    // asserted for as long as it takes the tuner to register, then react.
    bridge.set_gp2(true);
    info!("{}: GP2 HIGH (start asserted, holding until ADC ACK)", label);

    // Step 2 - Wait for tune-active edge while GP2 is still HIGH.
    // No ACK within ACTIVE_SEARCH_TIMEOUT_SECS -> TIMEOUT.
    //
    // Note: snapshot() rate-limits the USB ADC poll to ADC_POLL_MIN_MS (100 ms)
    // while this loop polls every ADC_POLL_INTERVAL_MS (25 ms). To avoid
    // counting the same cached sample multiple times toward EDGE_CONSECUTIVE
    // we dedup on the bridge-internal sample timestamp: a consecutive edge
    // only counts when last_adc_at has advanced since the previous iteration.
    let start = Instant::now();
    let mut consecutive_active = 0usize;
    let mut last_seen_at: Option<Instant> = None;
    loop {
        if check_abort(cmd_rx) {
            info!("{}: tune aborted at {:.2}s (waiting for ACK)", label, start.elapsed().as_secs_f32());
            bridge.set_gp2(false);
            set_state(status, TUNER_ABORTED);
            schedule_idle_reset(status);
            if restore_spe { safe_tune_operate(spe, label); }
            if restore_rf2k { safe_tune_operate_rf2k(rf2k, label); }
            return;
        }
        if start.elapsed() > Duration::from_secs(ACTIVE_SEARCH_TIMEOUT_SECS) {
            warn!(
                "{}: ADC never went tune-active while GP2 HIGH for {} s - TIMEOUT",
                label, ACTIVE_SEARCH_TIMEOUT_SECS
            );
            bridge.set_gp2(false);
            set_state(status, TUNER_TIMEOUT);
            schedule_idle_reset(status);
            if restore_spe { safe_tune_operate(spe, label); }
            if restore_rf2k { safe_tune_operate_rf2k(rf2k, label); }
            return;
        }
        let snap = bridge.snapshot();
        if let (Some(raw), Some(at)) = (snap.last_adc_raw, snap.last_adc_at) {
            if last_seen_at != Some(at) {
                last_seen_at = Some(at);
                if bridge.is_tune_active(raw) {
                    consecutive_active += 1;
                    if consecutive_active >= EDGE_CONSECUTIVE {
                        info!(
                            "{}: tuner ACK at {:.2}s (raw={})",
                            label,
                            start.elapsed().as_secs_f32(),
                            raw
                        );
                        break;
                    }
                } else {
                    consecutive_active = 0;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(ADC_POLL_INTERVAL_MS));
    }

    // Step 3 - Release the start-button now that the tuner has ACK'd.
    bridge.set_gp2(false);
    info!("{}: GP2 LOW (start released after tuner ACK)", label);

    // Step 4 - Carrier ON so the tuner has RF to chew on.
    if cat_tx.blocking_send("ZZTU1;".to_string()).is_err() {
        warn!("{}: failed to send ZZTU1 after ACK, aborting", label);
        set_state(status, TUNER_ABORTED);
        schedule_idle_reset(status);
        if restore_spe { safe_tune_operate(spe, label); }
        if restore_rf2k { safe_tune_operate_rf2k(rf2k, label); }
        return;
    }
    info!("{}: tune carrier ON (ZZTU1) - waiting for ADC to return to idle", label);

    // Step 5 - wait for tune complete (ADC back above the idle threshold).
    // Same dedup as Step 2: only count a consecutive idle edge when
    // last_adc_at advances, so rate-limited cached samples don't double-count.
    let mut consecutive_idle = 0usize;
    let mut last_seen_at: Option<Instant> = None;
    loop {
        if check_abort(cmd_rx) {
            info!("{}: tune aborted at {:.2}s (waiting for idle)", label, start.elapsed().as_secs_f32());
            let _ = cat_tx.blocking_send("ZZTU0;".to_string());
            set_state(status, TUNER_ABORTED);
            schedule_idle_reset(status);
            if restore_spe { safe_tune_operate(spe, label); }
            if restore_rf2k { safe_tune_operate_rf2k(rf2k, label); }
            return;
        }
        if start.elapsed() > Duration::from_secs(TUNE_COMPLETE_TIMEOUT_SECS) {
            warn!("{}: tune complete timeout at {} s", label, TUNE_COMPLETE_TIMEOUT_SECS);
            let _ = cat_tx.blocking_send("ZZTU0;".to_string());
            set_state(status, TUNER_TIMEOUT);
            schedule_idle_reset(status);
            if restore_spe { safe_tune_operate(spe, label); }
            if restore_rf2k { safe_tune_operate_rf2k(rf2k, label); }
            return;
        }
        let snap = bridge.snapshot();
        if let (Some(raw), Some(at)) = (snap.last_adc_raw, snap.last_adc_at) {
            if last_seen_at != Some(at) {
                last_seen_at = Some(at);
                if bridge.is_tune_idle(raw) {
                    consecutive_idle += 1;
                    if consecutive_idle >= EDGE_CONSECUTIVE {
                        info!("{}: tune COMPLETE at {:.2}s (raw={})", label, start.elapsed().as_secs_f32(), raw);
                        let _ = cat_tx.blocking_send("ZZTU0;".to_string());
                        set_state(status, TUNER_DONE_OK);
                        if restore_spe { safe_tune_operate(spe, label); }
                        if restore_rf2k { safe_tune_operate_rf2k(rf2k, label); }
                        return;
                    }
                } else {
                    consecutive_idle = 0;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(ADC_POLL_INTERVAL_MS));
    }
}

// ============================================================================
// PA orchestration - semantics preserved from the build-48 serial-port flow.
// ============================================================================

fn safe_tune_standby(spe: &Option<Arc<SpeExpert>>, label: &str) -> bool {
    let spe_ref = match spe { Some(s) => s, None => return false };
    let st = spe_ref.status();
    if st.state != 2 {
        info!("{}: safe tune: SPE PA state={}, skipping standby", label, st.state);
        return false;
    }
    info!("{}: safe tune: SPE PA in Operate, sending Standby", label);
    spe_ref.send_command(crate::spe_expert::SpeCmd::ToggleOperate);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let st = spe_ref.status();
        if st.state <= 1 {
            info!("{}: safe tune: SPE PA in Standby (state={})", label, st.state);
            return true;
        }
        if Instant::now() > deadline {
            warn!("{}: safe tune: timeout waiting for SPE Standby (state={}), tuning anyway", label, st.state);
            return true;
        }
    }
}

fn safe_tune_standby_rf2k(rf2k: &Option<Arc<Rf2k>>, label: &str) -> bool {
    let rf = match rf2k { Some(r) => r, None => return false };
    let st = rf.status();
    if !st.operate {
        info!("{}: safe tune: RF2K-S not in Operate, skipping", label);
        return false;
    }
    info!("{}: safe tune: RF2K-S in Operate, sending Standby", label);
    rf.send_command(crate::rf2k::Rf2kCmd::SetOperate(false));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let st = rf.status();
        if !st.operate {
            info!("{}: safe tune: RF2K-S in Standby", label);
            return true;
        }
        if Instant::now() > deadline {
            warn!("{}: safe tune: timeout waiting for RF2K-S Standby, tuning anyway", label);
            return true;
        }
    }
}

fn safe_tune_operate(spe: &Option<Arc<SpeExpert>>, label: &str) {
    let spe_ref = match spe { Some(s) => s, None => return };
    info!("{}: safe tune: waiting 2s before restoring SPE PA to Operate", label);
    std::thread::sleep(Duration::from_secs(2));
    let st = spe_ref.status();
    if st.state == 2 {
        info!("{}: safe tune: SPE PA already in Operate", label);
        return;
    }
    info!("{}: safe tune: sending SPE Operate command", label);
    spe_ref.send_command(crate::spe_expert::SpeCmd::ToggleOperate);
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        std::thread::sleep(Duration::from_millis(300));
        let st = spe_ref.status();
        if st.state == 2 {
            info!("{}: safe tune: SPE PA restored to Operate", label);
            return;
        }
        if Instant::now() > deadline {
            warn!("{}: safe tune: timeout waiting for SPE Operate, sending command again", label);
            spe_ref.send_command(crate::spe_expert::SpeCmd::ToggleOperate);
            std::thread::sleep(Duration::from_secs(2));
            let st = spe_ref.status();
            if st.state == 2 {
                info!("{}: safe tune: SPE PA restored to Operate on retry", label);
            } else {
                warn!("{}: safe tune: SPE PA still state={} after retry, giving up", label, st.state);
            }
            return;
        }
    }
}

fn safe_tune_operate_rf2k(rf2k: &Option<Arc<Rf2k>>, label: &str) {
    let rf = match rf2k { Some(r) => r, None => return };
    info!("{}: safe tune: waiting 2s before restoring RF2K-S to Operate", label);
    std::thread::sleep(Duration::from_secs(2));
    let st = rf.status();
    if st.operate {
        info!("{}: safe tune: RF2K-S already in Operate", label);
        return;
    }
    info!("{}: safe tune: sending RF2K-S Operate command", label);
    rf.send_command(crate::rf2k::Rf2kCmd::SetOperate(true));
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        std::thread::sleep(Duration::from_millis(300));
        let st = rf.status();
        if st.operate {
            info!("{}: safe tune: RF2K-S restored to Operate", label);
            return;
        }
        if Instant::now() > deadline {
            warn!("{}: safe tune: timeout waiting for RF2K-S Operate, retrying once", label);
            rf.send_command(crate::rf2k::Rf2kCmd::SetOperate(true));
            std::thread::sleep(Duration::from_secs(2));
            let st = rf.status();
            if st.operate {
                info!("{}: safe tune: RF2K-S restored to Operate on retry", label);
            } else {
                warn!("{}: safe tune: RF2K-S still in Standby after retry, giving up", label);
            }
            return;
        }
    }
}
