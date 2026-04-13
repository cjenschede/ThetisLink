use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{info, warn};

use crate::rf2k::Rf2k;
use crate::spe_expert::SpeExpert;

/// JC-4s antenna tuner controller.
/// Uses serial RTS/CTS lines only (no data): RTS triggers tune, CTS signals completion.
/// CAT commands (ZZTU1/ZZTU0) are sent via a tokio channel to the network service.
///
/// If an SPE Expert or RF2K-S PA reference is provided, the tuner will automatically put
/// the PA in Standby before tuning and restore it to Operate after tuning completes.
pub struct Jc4sTuner {
    cmd_tx: mpsc::Sender<TunerCmd>,
    status: Arc<Mutex<TunerStatus>>,
}

#[derive(Clone, Debug)]
pub struct TunerStatus {
    /// 0=Idle, 1=Tuning, 2=DoneOk, 3=Timeout, 4=Aborted
    pub state: u8,
    pub connected: bool,
    /// True if DONE_OK but VFO moved >25kHz from tuned freq (set by network tick)
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

pub enum TunerCmd {
    StartTune,
    AbortTune,
}

impl Jc4sTuner {
    /// Open serial port and start background thread.
    /// `cat_tx` sends CAT commands (ZZTU1/ZZTU0) to the network service for forwarding to Thetis.
    /// `spe` is an optional SPE Expert PA reference for safe tune orchestration.
    /// `rf2k` is an optional RF2K-S PA reference for safe tune orchestration.
    pub fn new(
        port_name: &str,
        cat_tx: tokio::sync::mpsc::Sender<String>,
        spe: Option<Arc<SpeExpert>>,
        rf2k: Option<Arc<Rf2k>>,
    ) -> Result<Self, String> {
        let port = serialport::new(port_name, 9600)
            .data_bits(serialport::DataBits::Eight)
            .parity(serialport::Parity::None)
            .stop_bits(serialport::StopBits::One)
            .timeout(Duration::from_millis(100))
            .open()
            .map_err(|e| format!("Failed to open {}: {}", port_name, e))?;

        let (cmd_tx, cmd_rx) = mpsc::channel::<TunerCmd>();
        let status = Arc::new(Mutex::new(TunerStatus::default()));

        let status_for_thread = status.clone();
        let port_name_owned = port_name.to_string();

        std::thread::Builder::new()
            .name("jc4s-tuner".to_string())
            .spawn(move || {
                tuner_thread(port, cmd_rx, status_for_thread, cat_tx, spe, rf2k, &port_name_owned);
            })
            .map_err(|e| format!("Failed to spawn tuner thread: {}", e))?;

        Ok(Self { cmd_tx, status })
    }

    pub fn send_command(&self, cmd: TunerCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn status(&self) -> TunerStatus {
        self.status.lock().unwrap().clone()
    }

    /// Reset DONE_OK state back to IDLE (e.g. after band change)
    pub fn reset_done(&self) {
        let mut s = self.status.lock().unwrap();
        if s.state == TUNER_DONE_OK {
            s.state = TUNER_IDLE;
        }
    }

    /// Set stale flag (VFO moved >25kHz from tuned freq, set by network tick)
    pub fn set_stale(&self, stale: bool) {
        self.status.lock().unwrap().stale = stale;
    }
}

fn set_state(status: &Arc<Mutex<TunerStatus>>, state: u8) {
    status.lock().unwrap().state = state;
}

fn tuner_thread(
    mut port: Box<dyn serialport::SerialPort>,
    cmd_rx: mpsc::Receiver<TunerCmd>,
    status: Arc<Mutex<TunerStatus>>,
    cat_tx: tokio::sync::mpsc::Sender<String>,
    spe: Option<Arc<SpeExpert>>,
    rf2k: Option<Arc<Rf2k>>,
    port_name: &str,
) {
    info!("JC-4s tuner thread started on {}", port_name);

    // Initialize: DTR HIGH, then RTS HIGH→LOW wake-up pulse for JC-4s
    if let Err(e) = port.write_data_terminal_ready(true) {
        warn!("JC-4s: failed to set DTR HIGH: {}", e);
    }
    // Wake-up pulse: RTS HIGH for 200ms, then LOW — ensures JC-4s is ready
    let _ = port.write_request_to_send(true);
    std::thread::sleep(Duration::from_millis(200));
    if let Err(e) = port.write_request_to_send(false) {
        warn!("JC-4s: failed to set RTS LOW: {}", e);
        status.lock().unwrap().connected = false;
        return;
    }
    std::thread::sleep(Duration::from_millis(200));

    {
        let mut s = status.lock().unwrap();
        s.connected = true;
        s.state = TUNER_IDLE;
    }
    info!("JC-4s: Ready (DTR HIGH, pins: CTS={} DSR={} DCD={} RI={})",
        port.read_clear_to_send().unwrap_or(false),
        port.read_data_set_ready().unwrap_or(false),
        port.read_carrier_detect().unwrap_or(false),
        port.read_ring_indicator().unwrap_or(false),
    );

    loop {
        match cmd_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(TunerCmd::StartTune) => {
                run_tune_sequence(&mut port, &cmd_rx, &status, &cat_tx, &spe, &rf2k);
            }
            Ok(TunerCmd::AbortTune) => {
                // Not currently tuning, ignore
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Idle: verify serial port is still accessible
                if let Err(e) = port.read_clear_to_send() {
                    warn!("JC-4s: serial error during idle check: {}", e);
                    status.lock().unwrap().connected = false;
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                info!("JC-4s command channel closed, stopping");
                break;
            }
        }
    }

    let _ = port.write_request_to_send(false);
    status.lock().unwrap().connected = false;
    info!("JC-4s tuner thread stopped");
}

/// Put SPE PA in standby if it's in Operate. Returns true if PA was in Operate and
/// we need to restore it after tuning.
fn safe_tune_standby(spe: &Option<Arc<SpeExpert>>) -> bool {
    let spe_ref = match spe {
        Some(ref s) => s,
        None => return false,
    };

    let st = spe_ref.status();
    if st.state != 2 {
        // PA not in Operate — no need to toggle
        info!("JC-4s safe tune: PA state={}, skipping standby", st.state);
        return false;
    }

    info!("JC-4s safe tune: PA in Operate, sending Standby...");
    spe_ref.send_command(crate::spe_expert::SpeCmd::ToggleOperate);

    // Wait for PA to reach Standby (max 5 seconds)
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let st = spe_ref.status();
        if st.state <= 1 {
            info!("JC-4s safe tune: PA in Standby (state={}), proceeding with tune", st.state);
            return true;
        }
        if Instant::now() > deadline {
            warn!("JC-4s safe tune: timeout waiting for Standby (state={}), tuning anyway", st.state);
            return true; // Still try to restore after tune
        }
    }
}

/// Put RF2K-S PA in standby if it's in Operate. Returns true if PA was in Operate and
/// we need to restore it after tuning.
fn safe_tune_standby_rf2k(rf2k: &Option<Arc<Rf2k>>) -> bool {
    let rf = match rf2k {
        Some(ref r) => r,
        None => return false,
    };

    let st = rf.status();
    if !st.operate {
        info!("JC-4s safe tune: RF2K-S not in Operate, skipping standby");
        return false;
    }

    info!("JC-4s safe tune: RF2K-S in Operate, sending Standby...");
    rf.send_command(crate::rf2k::Rf2kCmd::SetOperate(false));

    // Wait for PA to reach Standby (max 5 seconds)
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let st = rf.status();
        if !st.operate {
            info!("JC-4s safe tune: RF2K-S in Standby, proceeding with tune");
            return true;
        }
        if Instant::now() > deadline {
            warn!("JC-4s safe tune: timeout waiting for RF2K-S Standby, tuning anyway");
            return true;
        }
    }
}

/// Restore SPE PA to Operate after tuning.
fn safe_tune_operate(spe: &Option<Arc<SpeExpert>>) {
    let spe_ref = match spe {
        Some(ref s) => s,
        None => return,
    };

    // Give PA time to settle after tune sequence before sending Operate
    info!("JC-4s safe tune: waiting 2s before restoring PA to Operate...");
    std::thread::sleep(Duration::from_secs(2));

    let st = spe_ref.status();
    info!("JC-4s safe tune: PA state={} before sending Operate", st.state);

    // If PA already went back to Operate on its own, no need to send command
    if st.state == 2 {
        info!("JC-4s safe tune: PA already in Operate, no command needed");
        return;
    }

    info!("JC-4s safe tune: sending Operate command...");
    spe_ref.send_command(crate::spe_expert::SpeCmd::ToggleOperate);

    // Wait for PA to reach Operate (max 8 seconds)
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        std::thread::sleep(Duration::from_millis(300));
        let st = spe_ref.status();
        info!("JC-4s safe tune: waiting for Operate, current state={}", st.state);
        if st.state == 2 {
            info!("JC-4s safe tune: PA restored to Operate");
            return;
        }
        if Instant::now() > deadline {
            warn!("JC-4s safe tune: timeout waiting for Operate (state={}), sending command again", st.state);
            // Try once more
            spe_ref.send_command(crate::spe_expert::SpeCmd::ToggleOperate);
            std::thread::sleep(Duration::from_secs(2));
            let st = spe_ref.status();
            if st.state == 2 {
                info!("JC-4s safe tune: PA restored to Operate on retry");
            } else {
                warn!("JC-4s safe tune: PA still state={} after retry, giving up", st.state);
            }
            return;
        }
    }
}

/// Restore RF2K-S PA to Operate after tuning.
fn safe_tune_operate_rf2k(rf2k: &Option<Arc<Rf2k>>) {
    let rf = match rf2k {
        Some(ref r) => r,
        None => return,
    };

    info!("JC-4s safe tune: waiting 2s before restoring RF2K-S to Operate...");
    std::thread::sleep(Duration::from_secs(2));

    let st = rf.status();
    if st.operate {
        info!("JC-4s safe tune: RF2K-S already in Operate, no command needed");
        return;
    }

    info!("JC-4s safe tune: sending RF2K-S Operate command...");
    rf.send_command(crate::rf2k::Rf2kCmd::SetOperate(true));

    // Wait for PA to reach Operate (max 8 seconds)
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        std::thread::sleep(Duration::from_millis(300));
        let st = rf.status();
        if st.operate {
            info!("JC-4s safe tune: RF2K-S restored to Operate");
            return;
        }
        if Instant::now() > deadline {
            warn!("JC-4s safe tune: timeout waiting for RF2K-S Operate, sending command again");
            rf.send_command(crate::rf2k::Rf2kCmd::SetOperate(true));
            std::thread::sleep(Duration::from_secs(2));
            let st = rf.status();
            if st.operate {
                info!("JC-4s safe tune: RF2K-S restored to Operate on retry");
            } else {
                warn!("JC-4s safe tune: RF2K-S still in Standby after retry, giving up");
            }
            return;
        }
    }
}

fn run_tune_sequence(
    port: &mut Box<dyn serialport::SerialPort>,
    cmd_rx: &mpsc::Receiver<TunerCmd>,
    status: &Arc<Mutex<TunerStatus>>,
    cat_tx: &tokio::sync::mpsc::Sender<String>,
    spe: &Option<Arc<SpeExpert>>,
    rf2k: &Option<Arc<Rf2k>>,
) {
    info!("JC-4s: Starting tune sequence");
    set_state(status, TUNER_TUNING);

    // Safe tune: put PA(s) in Standby first if needed
    let restore_spe = safe_tune_standby(spe);
    let restore_rf2k = safe_tune_standby_rf2k(rf2k);

    // Extra settle time after PA standby before tune power
    if restore_spe || restore_rf2k {
        info!("JC-4s: waiting 500ms for PA settle after standby");
        std::thread::sleep(Duration::from_millis(500));
    }

    // Step 1: RTS HIGH — signal tuner to prepare
    if let Err(e) = port.write_request_to_send(true) {
        warn!("JC-4s: RTS HIGH failed: {}", e);
        abort_cleanup(port, status, cat_tx);
        if restore_spe { safe_tune_operate(spe); }
        if restore_rf2k { safe_tune_operate_rf2k(rf2k); }
        return;
    }

    // Step 2: Wait 150ms for tuner initialization
    std::thread::sleep(Duration::from_millis(150));

    // Step 3: ZZTU1 — Thetis tune carrier ON
    if let Err(e) = cat_tx.blocking_send("ZZTU1;".to_string()) {
        warn!("JC-4s: failed to send ZZTU1: {}", e);
        abort_cleanup(port, status, cat_tx);
        if restore_spe { safe_tune_operate(spe); }
        if restore_rf2k { safe_tune_operate_rf2k(rf2k); }
        return;
    }
    info!("JC-4s: Tune carrier ON (ZZTU1)");

    // DDutil sequence: DDRT1; DDLY150; ZZTU1; DDRT0;
    // Wait for carrier to actually start (async channel adds latency), then RTS LOW
    std::thread::sleep(Duration::from_millis(500));
    let _ = port.write_request_to_send(false);
    info!("JC-4s: RTS LOW (after 500ms)");

    // After RTS LOW, CTS goes TRUE quickly. When tune completes, CTS goes FALSE.
    // So we wait for CTS TRUE first, then wait for CTS FALSE = tune complete.
    let start = Instant::now();
    let timeout = Duration::from_secs(30);
    let mut last_cts = port.read_clear_to_send().unwrap_or(false);
    info!("JC-4s: CTS = {} after RTS LOW, waiting for CTS FALSE (tune complete)...", last_cts);

    // Wait for CTS TRUE first (should happen quickly after RTS LOW)
    if !last_cts {
        loop {
            if check_abort(cmd_rx) {
                info!("JC-4s: Tune aborted at {:.1}s", start.elapsed().as_secs_f32());
                let _ = cat_tx.blocking_send("ZZTU0;".to_string());
                set_state(status, TUNER_ABORTED);
                schedule_idle_reset(status);
                if restore_spe { safe_tune_operate(spe); }
                if restore_rf2k { safe_tune_operate_rf2k(rf2k); }
                return;
            }
            if start.elapsed() > Duration::from_secs(3) {
                warn!("JC-4s: CTS never went TRUE after RTS LOW");
                let _ = cat_tx.blocking_send("ZZTU0;".to_string());
                set_state(status, TUNER_TIMEOUT);
                schedule_idle_reset(status);
                if restore_spe { safe_tune_operate(spe); }
                if restore_rf2k { safe_tune_operate_rf2k(rf2k); }
                return;
            }
            match port.read_clear_to_send() {
                Ok(cts) => {
                    if cts != last_cts {
                        info!("JC-4s: CTS {} -> {} at {:.1}s", last_cts, cts, start.elapsed().as_secs_f32());
                        last_cts = cts;
                    }
                    if cts { break; }
                }
                Err(e) => {
                    warn!("JC-4s: CTS read error: {}", e);
                    let _ = cat_tx.blocking_send("ZZTU0;".to_string());
                    status.lock().unwrap().connected = false;
                    set_state(status, TUNER_ABORTED);
                    if restore_spe { safe_tune_operate(spe); }
                    if restore_rf2k { safe_tune_operate_rf2k(rf2k); }
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    // Now CTS is TRUE — wait for CTS FALSE = tune complete
    info!("JC-4s: CTS TRUE, waiting for CTS FALSE (tune complete)...");
    loop {
        if check_abort(cmd_rx) {
            info!("JC-4s: Tune aborted at {:.1}s", start.elapsed().as_secs_f32());
            let _ = cat_tx.blocking_send("ZZTU0;".to_string());
            set_state(status, TUNER_ABORTED);
            schedule_idle_reset(status);
            if restore_spe { safe_tune_operate(spe); }
            if restore_rf2k { safe_tune_operate_rf2k(rf2k); }
            return;
        }

        if start.elapsed() > timeout {
            warn!("JC-4s: Tune timeout (30s), CTS={}", last_cts);
            let _ = cat_tx.blocking_send("ZZTU0;".to_string());
            set_state(status, TUNER_TIMEOUT);
            schedule_idle_reset(status);
            if restore_spe { safe_tune_operate(spe); }
            if restore_rf2k { safe_tune_operate_rf2k(rf2k); }
            return;
        }

        match port.read_clear_to_send() {
            Ok(cts) => {
                if cts != last_cts {
                    info!("JC-4s: CTS {} -> {} at {:.1}s", last_cts, cts, start.elapsed().as_secs_f32());
                    last_cts = cts;
                }
                if !cts {
                    let elapsed = start.elapsed();
                    info!("JC-4s: Tune complete ({:.1}s)", elapsed.as_secs_f32());
                    let _ = cat_tx.blocking_send("ZZTU0;".to_string());
                    info!("JC-4s: Tune carrier OFF (ZZTU0)");
                    set_state(status, TUNER_DONE_OK);
                    // No idle reset — stays green until next tune
                    if restore_spe { safe_tune_operate(spe); }
                    if restore_rf2k { safe_tune_operate_rf2k(rf2k); }
                    return;
                }
            }
            Err(e) => {
                warn!("JC-4s: CTS read error: {}", e);
                let _ = cat_tx.blocking_send("ZZTU0;".to_string());
                status.lock().unwrap().connected = false;
                set_state(status, TUNER_ABORTED);
                if restore_spe { safe_tune_operate(spe); }
                if restore_rf2k { safe_tune_operate_rf2k(rf2k); }
                return;
            }
        }

        std::thread::sleep(Duration::from_millis(25));
    }
}

fn check_abort(cmd_rx: &mpsc::Receiver<TunerCmd>) -> bool {
    matches!(cmd_rx.try_recv(), Ok(TunerCmd::AbortTune))
}

fn abort_cleanup(
    port: &mut Box<dyn serialport::SerialPort>,
    status: &Arc<Mutex<TunerStatus>>,
    cat_tx: &tokio::sync::mpsc::Sender<String>,
) {
    let _ = cat_tx.blocking_send("ZZTU0;".to_string());
    let _ = port.write_request_to_send(false);
    set_state(status, TUNER_ABORTED);
    schedule_idle_reset(status);
}

/// Reset state to Idle after 3 seconds (in a short-lived thread)
fn schedule_idle_reset(status: &Arc<Mutex<TunerStatus>>) {
    let status = status.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(3));
        let mut s = status.lock().unwrap();
        // Only reset if not currently tuning (could have started a new tune)
        if s.state != TUNER_TUNING && s.state != TUNER_IDLE {
            s.state = TUNER_IDLE;
        }
    });
}
