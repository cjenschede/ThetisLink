// SPDX-License-Identifier: GPL-2.0-or-later

#![allow(dead_code)]
use std::io::{Read, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{debug, info, warn};

/// SPE Expert 1.3K-FA / 1.5K-FA / 2K-FA power amplifier USB serial controller.
/// Communicates via 115200 baud 8N1, binary command protocol.
///
/// Protocol reference: SPE Application Programmer's Guide Rev. 1.1 (15.10.2015)
/// — zie `docs/referentie/SPE_Application_Programmers_Guide.pdf`.
///
///   Command (host → PA):  [0x55 0x55 0x55] [CNT] [DATA...] [CHK]
///                           CHK = sum(DATA) mod 256
///   Response (PA → host): [0xAA 0xAA 0xAA] [CNT] [DATA...] [CHK_LO] [CHK_HI] [,] [CR] [LF]
///                           CHK_LO = sum(DATA) % 256
///                           CHK_HI = sum(DATA) / 256
///   STATUS response DATA is 67 bytes of comma-separated ASCII (19 fields).
///
/// Hardware: SPE has USB + RS-232 as PC-interface ports. Both speak this binary
/// protocol and cannot be used simultaneously. The amplifier's CAT connection
/// to the transceiver is a separate physical cable using Kenwood CAT — not
/// this protocol.
pub struct SpeExpert {
    cmd_tx: mpsc::Sender<SpeCmd>,
    status: Arc<Mutex<SpeStatus>>,
}

#[derive(Clone, Debug)]
pub struct SpeStatus {
    pub connected: bool,
    pub state: u8,           // 0=Off, 1=Standby, 2=Operate
    pub ptt: bool,           // true = TX
    pub band: u8,            // band code from PA (0=160m..10=6m, 11=4m)
    pub antenna: u8,         // TX antenna position (1 or 2)
    pub atu_bypassed: bool,  // true = ATU bypassed ('b'), false = ATU active ('a'/'t')
    pub input: u8,           // input selector (1 or 2)
    pub power_level: u8,     // 0=L, 1=M, 2=H
    pub forward_power: u16,  // watts
    pub swr_x10: u16,        // SWR x 10 (e.g. 15 = 1.5:1)
    pub temp: u8,            // max of 3 sensors, degrees C
    pub voltage_x10: u16,    // supply voltage x 10
    pub current_x10: u16,    // supply current x 10
    pub warning: u8,         // warning char as u8 ('N' = none)
    pub alarm: u8,           // alarm char as u8 ('N' = none)
}

impl Default for SpeStatus {
    fn default() -> Self {
        Self {
            connected: false,
            state: 0,
            ptt: false,
            band: 0,
            antenna: 0,
            atu_bypassed: false,
            input: 0,
            power_level: 0,
            forward_power: 0,
            swr_x10: 10,
            temp: 0,
            voltage_x10: 0,
            current_x10: 0,
            warning: b'N',
            alarm: b'N',
        }
    }
}

pub enum SpeCmd {
    ToggleOperate,  // 0x0D
    Tune,           // 0x09
    CycleAntenna,   // 0x04
    CycleInput,     // 0x01
    CyclePower,     // 0x0B
    BandUp,         // 0x03
    BandDown,       // 0x02
    PowerOff,       // 0x0A  (serial command)
    PowerOn,        // RTS pulse (not a serial command)
    DriveDown,      // 0x0F  LEFT_ARROW — decrease drive during TX
    DriveUp,        // 0x10  RIGHT_ARROW — increase drive during TX
}

// SPE response preamble byte
const RESP_PREAMBLE: u8 = 0xAA;

// Single-byte SPE serial commands
const CMD_INPUT: u8 = 0x01;
const CMD_BAND_DOWN: u8 = 0x02;
const CMD_BAND_UP: u8 = 0x03;
const CMD_ANTENNA: u8 = 0x04;
const CMD_TUNE: u8 = 0x09;
const CMD_POWER_OFF: u8 = 0x0A;
const CMD_POWER_LEVEL: u8 = 0x0B;
const CMD_OPERATE: u8 = 0x0D;
const CMD_LEFT_ARROW: u8 = 0x0F;  // Drive down during TX
const CMD_RIGHT_ARROW: u8 = 0x10; // Drive up during TX
const CMD_STATUS: u8 = 0x90;

impl SpeExpert {
    /// Open serial port and start background thread.
    pub fn new(port_name: &str) -> Result<Self, String> {
        let mut port = serialport::new(port_name, 115200)
            .data_bits(serialport::DataBits::Eight)
            .parity(serialport::Parity::None)
            .stop_bits(serialport::StopBits::One)
            .flow_control(serialport::FlowControl::None)
            .timeout(Duration::from_millis(2000))
            .open()
            .map_err(|e| format!("Failed to open {}: {}", port_name, e))?;

        // Set RTS LOW to prevent accidental power-on pulse.
        // DTR doesn't matter for 1.3K-FA.
        if let Err(e) = port.write_request_to_send(false) {
            warn!("SPE Expert: could not set RTS low: {}", e);
        }

        // Flush any stale data in the input buffer
        if let Err(e) = port.clear(serialport::ClearBuffer::Input) {
            warn!("SPE Expert: could not flush input: {}", e);
        }

        let (cmd_tx, cmd_rx) = mpsc::channel::<SpeCmd>();
        let status = Arc::new(Mutex::new(SpeStatus::default()));

        let status_for_thread = status.clone();
        let port_name_owned = port_name.to_string();

        std::thread::Builder::new()
            .name("spe-expert-serial".to_string())
            .spawn(move || {
                spe_thread(port, cmd_rx, status_for_thread, &port_name_owned);
            })
            .map_err(|e| format!("Failed to spawn SPE Expert thread: {}", e))?;

        Ok(Self { cmd_tx, status })
    }

    pub fn send_command(&self, cmd: SpeCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn status(&self) -> SpeStatus {
        self.status.lock().unwrap().clone()
    }
}

// ============================================================================
// Polling state machine (PATCH-spe-expert-polling-state)
// ----------------------------------------------------------------------------
// Replaces the legacy linear `consecutive_failures` counter with explicit
// connection states so that:
//   - transition-only logging eliminates per-poll warn spam
//   - warm-up tolerance prevents false `mark_disconnected` on UART startup
//   - adaptive polling runs fast (75ms) during TX and slow (1s / 2s) in RX
//     and offline modes
// See `docs/patch-briefs/PATCH-spe-expert-polling-state.md` §1.1–§1.7.
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpeConnState {
    Unknown,
    WarmingUp,
    Online,
    Offline,
    CatMisconfigured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PollIoKind {
    Timeout,
    Read,
    Parse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PollFailureKind {
    TimeoutNoData,
    TimeoutNullOnly,
    TimeoutPartial,
    OverflowWithPreamble,
    CatPortMismatch,
    OverflowUnknown,
    IoError,
    ParseError,
}

/// Classify a failed status read into a `PollFailureKind`. Pure function:
/// inspects only the collected bytes and the IO kind that produced the error.
pub(crate) fn classify_poll_failure(collected: &[u8], io_kind: PollIoKind) -> PollFailureKind {
    match io_kind {
        PollIoKind::Read => PollFailureKind::IoError,
        PollIoKind::Parse => PollFailureKind::ParseError,
        PollIoKind::Timeout => classify_timeout_payload(collected),
    }
}

fn classify_timeout_payload(collected: &[u8]) -> PollFailureKind {
    if collected.is_empty() {
        return PollFailureKind::TimeoutNoData;
    }
    let has_preamble = collected.len() >= 3
        && collected[0] == RESP_PREAMBLE
        && collected[1] == RESP_PREAMBLE
        && collected[2] == RESP_PREAMBLE;
    if has_preamble {
        if collected.len() > 128 {
            return PollFailureKind::OverflowWithPreamble;
        }
        return PollFailureKind::TimeoutPartial;
    }
    if collected.len() > 64 {
        let preview_len = collected.len().min(64);
        let preview = &collected[..preview_len];
        let printable = preview
            .iter()
            .filter(|&&b| (0x20..0x7F).contains(&b))
            .count();
        let looks_ascii = printable * 10 >= preview.len() * 9;
        let has_cat_sep = preview.iter().any(|&b| b == b';' || b == b',');
        if looks_ascii && has_cat_sep {
            return PollFailureKind::CatPortMismatch;
        }
        return PollFailureKind::OverflowUnknown;
    }
    if collected.iter().all(|&b| b == 0) {
        return PollFailureKind::TimeoutNullOnly;
    }
    PollFailureKind::TimeoutPartial
}

#[derive(Debug, Clone)]
pub(crate) struct TransitionCtx {
    pub consecutive_fails: u32,
    pub offline_since: Option<Instant>,
    pub tx_active: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SpeEvent {
    InitialOk { state: u8 },
    InitialFail,
    PollOk,
    PollFail(PollFailureKind),
}

#[derive(Debug, Clone)]
pub(crate) enum TransitionLog {
    Info(String),
    Warn(String),
    /// CAT mismatch detected — caller must render the full warning by calling
    /// `format_unexpected_data_error(&collected)` since the classifier drops
    /// the raw bytes after classification.
    WarnCatMisconfigured,
}

/// Pure state-transition function. Returns the new state plus an optional log
/// directive that should only be emitted on actual transitions (not self-loops).
pub(crate) fn next_state(
    current: SpeConnState,
    event: SpeEvent,
    ctx: &TransitionCtx,
) -> (SpeConnState, Option<TransitionLog>) {
    use PollFailureKind::*;
    use SpeConnState::*;
    use SpeEvent::*;

    const WARMUP_HARD_FAIL_THRESHOLD: u32 = 3;
    const ONLINE_FAIL_THRESHOLD: u32 = 5;

    match (current, event) {
        (Unknown, InitialOk { state }) => (
            WarmingUp,
            Some(TransitionLog::Info(format!(
                "SPE Expert connected, state={}",
                state
            ))),
        ),
        (Unknown, InitialFail) => (Offline, None),

        (WarmingUp, PollOk) => (
            Online,
            Some(TransitionLog::Info("SPE Expert online".to_string())),
        ),
        (WarmingUp, PollFail(kind)) => match kind {
            CatPortMismatch => (CatMisconfigured, Some(TransitionLog::WarnCatMisconfigured)),
            IoError => (
                Offline,
                Some(TransitionLog::Warn(
                    "SPE Expert IO error — going offline".to_string(),
                )),
            ),
            OverflowWithPreamble | OverflowUnknown | ParseError => {
                if ctx.consecutive_fails + 1 >= WARMUP_HARD_FAIL_THRESHOLD {
                    (
                        Offline,
                        Some(TransitionLog::Warn(
                            "SPE Expert did not come online — check connection".to_string(),
                        )),
                    )
                } else {
                    (WarmingUp, None)
                }
            }
            // Warm-up tolerance: null/empty/partial timeouts are UART startup noise
            TimeoutNoData | TimeoutNullOnly | TimeoutPartial => (WarmingUp, None),
        },

        (Online, PollOk) => (Online, None),
        (Online, PollFail(kind)) => match kind {
            CatPortMismatch => (CatMisconfigured, Some(TransitionLog::WarnCatMisconfigured)),
            IoError => (
                Offline,
                Some(TransitionLog::Info(
                    "SPE Expert offline — polling continues silently".to_string(),
                )),
            ),
            _ => {
                if ctx.consecutive_fails + 1 >= ONLINE_FAIL_THRESHOLD {
                    (
                        Offline,
                        Some(TransitionLog::Info(
                            "SPE Expert offline — polling continues silently".to_string(),
                        )),
                    )
                } else {
                    (Online, None)
                }
            }
        },

        (Offline, PollOk) => {
            let msg = match ctx.offline_since {
                Some(t) => format!("SPE Expert back online after {}s", t.elapsed().as_secs()),
                None => "SPE Expert back online".to_string(),
            };
            (Online, Some(TransitionLog::Info(msg)))
        }
        // CAT mismatch can promote from Offline → user may hot-swap cables and
        // wants a one-time warning explaining why the port isn't responding.
        (Offline, PollFail(CatPortMismatch)) => {
            (CatMisconfigured, Some(TransitionLog::WarnCatMisconfigured))
        }
        (Offline, PollFail(_)) => (Offline, None),
        (Offline, InitialOk { state }) => (
            WarmingUp,
            Some(TransitionLog::Info(format!(
                "SPE Expert connected, state={}",
                state
            ))),
        ),
        (Offline, InitialFail) => (Offline, None),

        (CatMisconfigured, PollOk) => (
            Online,
            Some(TransitionLog::Info(
                "SPE Expert CAT mismatch resolved, now online".to_string(),
            )),
        ),
        (CatMisconfigured, PollFail(_)) => (CatMisconfigured, None),
        (CatMisconfigured, InitialOk { .. }) => (CatMisconfigured, None),
        (CatMisconfigured, InitialFail) => (CatMisconfigured, None),

        // Reinit paths after thread restart — not expected in practice, keep silent.
        (WarmingUp, InitialOk { .. }) | (WarmingUp, InitialFail) => (WarmingUp, None),
        (Online, InitialOk { .. }) | (Online, InitialFail) => (Online, None),

        // Unknown should be followed by an Initial* event first; polling events
        // in Unknown state are a defensive no-op.
        (Unknown, PollOk) | (Unknown, PollFail(_)) => (Unknown, None),
    }
}

/// Whether a given failure kind counts toward the threshold-counter in the
/// given state. Matches the tel-beleid table in brief §1.4.
///
/// - `IoError` and `CatPortMismatch` never count — they trigger immediate
///   transitions in `next_state` and do not accumulate.
/// - In `WarmingUp`, only true protocol errors count. UART-startup noise
///   (`TimeoutNoData`/`NullOnly`/`Partial`) is ignored so a successful
///   `state=N` initial query cannot be undone by driver/UART warm-up.
/// - In `Online`, any non-directe kind counts normally.
/// - `Offline`/`CatMisconfigured`/`Unknown` do not accumulate — polling
///   stays silent until a success event.
pub(crate) fn counts_toward_threshold(state: SpeConnState, kind: PollFailureKind) -> bool {
    match state {
        SpeConnState::WarmingUp => matches!(
            kind,
            PollFailureKind::OverflowWithPreamble
                | PollFailureKind::OverflowUnknown
                | PollFailureKind::ParseError
        ),
        SpeConnState::Online => !matches!(
            kind,
            PollFailureKind::IoError | PollFailureKind::CatPortMismatch
        ),
        SpeConnState::Offline | SpeConnState::CatMisconfigured | SpeConnState::Unknown => false,
    }
}

/// Adaptive polling interval. Fast during TX (real-time SWR/power meters),
/// slow in RX idle, slower in offline/CAT states (CPU + log zuinig).
pub(crate) fn poll_interval(state: SpeConnState, tx_active: bool) -> Duration {
    match state {
        SpeConnState::Unknown | SpeConnState::WarmingUp => Duration::from_millis(500),
        SpeConnState::Online if tx_active => Duration::from_millis(75),
        SpeConnState::Online => Duration::from_millis(1000),
        SpeConnState::Offline | SpeConnState::CatMisconfigured => Duration::from_millis(2000),
    }
}

fn state_is_connected(state: SpeConnState) -> bool {
    matches!(state, SpeConnState::WarmingUp | SpeConnState::Online)
}

/// Emit a transition log line. For `WarnCatMisconfigured` the original
/// `format_unexpected_data_error` string is rendered from the collected bytes
/// so the existing Kenwood-CAT hint text is preserved verbatim.
fn emit_transition_log(log: TransitionLog, collected_bytes: Option<&[u8]>) {
    match log {
        TransitionLog::Info(msg) => info!("{}", msg),
        TransitionLog::Warn(msg) => warn!("{}", msg),
        TransitionLog::WarnCatMisconfigured => {
            if let Some(bytes) = collected_bytes {
                warn!("{}", format_unexpected_data_error(bytes));
            } else {
                warn!("SPE Expert CAT mismatch detected (no payload)");
            }
        }
    }
}

/// Poll-failure signal including the raw bytes so transition logging can
/// reuse `format_unexpected_data_error` for CAT-mismatch warnings.
struct PollFailure {
    kind: PollIoKind,
    collected: Vec<u8>,
}

fn query_status_ex(port: &mut Box<dyn serialport::SerialPort>) -> Result<SpeStatus, PollFailure> {
    if let Err(_) = send_single_command(port, CMD_STATUS) {
        return Err(PollFailure {
            kind: PollIoKind::Read,
            collected: Vec::new(),
        });
    }
    let data = read_status_response_ex(port)?;
    parse_status_response(&data).map_err(|_| PollFailure {
        kind: PollIoKind::Parse,
        collected: data,
    })
}

fn read_status_response_ex(
    port: &mut Box<dyn serialport::SerialPort>,
) -> Result<Vec<u8>, PollFailure> {
    let mut collected = Vec::with_capacity(128);
    let mut buf = [0u8; 128];

    loop {
        match port.read(&mut buf) {
            Ok(0) => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(n) => {
                collected.extend_from_slice(&buf[..n]);

                if collected.len() >= 2
                    && collected[collected.len() - 2] == 0x0D
                    && collected[collected.len() - 1] == 0x0A
                {
                    return extract_status_data(&collected).map_err(|_| PollFailure {
                        kind: PollIoKind::Parse,
                        collected: collected.clone(),
                    });
                }

                // Preamble-aware overflow check — see `read_status_response` comment.
                let has_preamble = collected.len() >= 3
                    && collected[0] == RESP_PREAMBLE
                    && collected[1] == RESP_PREAMBLE
                    && collected[2] == RESP_PREAMBLE;
                let threshold = if has_preamble { 128 } else { 64 };
                if collected.len() > threshold {
                    return Err(PollFailure {
                        kind: PollIoKind::Timeout,
                        collected,
                    });
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                if collected.len() >= 2
                    && collected[collected.len() - 2] == 0x0D
                    && collected[collected.len() - 1] == 0x0A
                {
                    return extract_status_data(&collected).map_err(|_| PollFailure {
                        kind: PollIoKind::Parse,
                        collected: collected.clone(),
                    });
                }
                return Err(PollFailure {
                    kind: PollIoKind::Timeout,
                    collected,
                });
            }
            Err(_) => {
                return Err(PollFailure {
                    kind: PollIoKind::Read,
                    collected,
                });
            }
        }
    }
}

fn spe_thread(
    mut port: Box<dyn serialport::SerialPort>,
    cmd_rx: mpsc::Receiver<SpeCmd>,
    status: Arc<Mutex<SpeStatus>>,
    port_name: &str,
) {
    info!("SPE Expert serial thread started on {}", port_name);

    // Brief delay after port open before first query
    std::thread::sleep(Duration::from_millis(200));

    // Flush again just before first query
    let _ = port.clear(serialport::ClearBuffer::Input);

    let mut conn_state = SpeConnState::Unknown;
    let mut consecutive_fails: u32 = 0;
    let mut offline_since: Option<Instant> = None;

    // Initial status query
    let (event, collected_bytes): (SpeEvent, Option<Vec<u8>>) = match query_status_ex(&mut port) {
        Ok(parsed) => {
            let state_val = parsed.state;
            {
                let mut s = status.lock().unwrap();
                *s = parsed;
            }
            (SpeEvent::InitialOk { state: state_val }, None)
        }
        Err(pf) => (SpeEvent::InitialFail, Some(pf.collected)),
    };
    let ctx = TransitionCtx {
        consecutive_fails,
        offline_since,
        tx_active: false,
    };
    let (new_state, log_line) = next_state(conn_state, event, &ctx);
    apply_transition(
        &status,
        conn_state,
        new_state,
        log_line,
        collected_bytes.as_deref(),
        &mut offline_since,
    );
    conn_state = new_state;

    loop {
        let tx_active = status.lock().map(|s| s.ptt).unwrap_or(false);
        let interval = poll_interval(conn_state, tx_active);

        match cmd_rx.recv_timeout(interval) {
            Ok(cmd) => {
                // Handle PowerOn separately (RTS pulse, not a serial command)
                if matches!(cmd, SpeCmd::PowerOn) {
                    info!("SPE Expert: sending Power On RTS pulse");
                    if let Err(e) = power_on_rts_pulse(&mut port) {
                        warn!("SPE Expert power on failed: {}", e);
                    }
                    std::thread::sleep(Duration::from_millis(500));
                } else {
                    let cmd_byte = match cmd {
                        SpeCmd::ToggleOperate => CMD_OPERATE,
                        SpeCmd::Tune => CMD_TUNE,
                        SpeCmd::CycleAntenna => CMD_ANTENNA,
                        SpeCmd::CycleInput => CMD_INPUT,
                        SpeCmd::CyclePower => CMD_POWER_LEVEL,
                        SpeCmd::BandUp => CMD_BAND_UP,
                        SpeCmd::BandDown => CMD_BAND_DOWN,
                        SpeCmd::PowerOff => CMD_POWER_OFF,
                        SpeCmd::DriveDown => CMD_LEFT_ARROW,
                        SpeCmd::DriveUp => CMD_RIGHT_ARROW,
                        SpeCmd::PowerOn => unreachable!(),
                    };
                    let _ = port.clear(serialport::ClearBuffer::Input);
                    if let Err(e) = send_single_command(&mut port, cmd_byte) {
                        warn!("SPE Expert command 0x{:02X} failed: {}", cmd_byte, e);
                    } else {
                        info!("SPE Expert: sent command 0x{:02X}", cmd_byte);
                        let _ = read_ack(&mut port);
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }

                let _ = port.clear(serialport::ClearBuffer::Input);
                run_poll_cycle(
                    &mut port,
                    &status,
                    &mut conn_state,
                    &mut consecutive_fails,
                    &mut offline_since,
                    tx_active,
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = port.clear(serialport::ClearBuffer::Input);
                run_poll_cycle(
                    &mut port,
                    &status,
                    &mut conn_state,
                    &mut consecutive_fails,
                    &mut offline_since,
                    tx_active,
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                info!("SPE Expert command channel closed, stopping");
                break;
            }
        }
    }

    mark_disconnected(&status);
    info!("SPE Expert serial thread stopped");
}

fn run_poll_cycle(
    port: &mut Box<dyn serialport::SerialPort>,
    status: &Arc<Mutex<SpeStatus>>,
    conn_state: &mut SpeConnState,
    consecutive_fails: &mut u32,
    offline_since: &mut Option<Instant>,
    tx_active: bool,
) {
    match query_status_ex(port) {
        Ok(parsed) => {
            {
                let mut s = status.lock().unwrap();
                *s = parsed;
            }
            let ctx = TransitionCtx {
                consecutive_fails: *consecutive_fails,
                offline_since: *offline_since,
                tx_active,
            };
            let (new_state, log_line) = next_state(*conn_state, SpeEvent::PollOk, &ctx);
            *consecutive_fails = 0;
            apply_transition(status, *conn_state, new_state, log_line, None, offline_since);
            *conn_state = new_state;
        }
        Err(pf) => {
            let kind = classify_poll_failure(&pf.collected, pf.kind);
            let old_state = *conn_state;
            // Pass the PRE-fail counter; `next_state` applies the +1 in its
            // threshold check so we do not double-count.
            let ctx = TransitionCtx {
                consecutive_fails: *consecutive_fails,
                offline_since: *offline_since,
                tx_active,
            };
            let (new_state, log_line) =
                next_state(old_state, SpeEvent::PollFail(kind), &ctx);
            // Only increment when we stayed in the same state AND this kind
            // counts per brief §1.4 tel-beleid. Any actual transition resets.
            if old_state != new_state {
                *consecutive_fails = 0;
            } else if counts_toward_threshold(old_state, kind) {
                *consecutive_fails = consecutive_fails.saturating_add(1);
            }
            apply_transition(
                status,
                old_state,
                new_state,
                log_line,
                Some(&pf.collected),
                offline_since,
            );
            *conn_state = new_state;
        }
    }
}

fn apply_transition(
    status: &Arc<Mutex<SpeStatus>>,
    from: SpeConnState,
    to: SpeConnState,
    log: Option<TransitionLog>,
    collected_bytes: Option<&[u8]>,
    offline_since: &mut Option<Instant>,
) {
    {
        let mut s = status.lock().unwrap();
        s.connected = state_is_connected(to);
    }

    let became_offline_like = matches!(to, SpeConnState::Offline | SpeConnState::CatMisconfigured);
    let was_offline_like = matches!(from, SpeConnState::Offline | SpeConnState::CatMisconfigured);
    if became_offline_like && !was_offline_like {
        *offline_since = Some(Instant::now());
    } else if !became_offline_like && was_offline_like {
        *offline_since = None;
    }

    if from != to {
        if let Some(line) = log {
            emit_transition_log(line, collected_bytes);
        }
    }
}

fn mark_disconnected(status: &Arc<Mutex<SpeStatus>>) {
    let mut s = status.lock().unwrap();
    s.connected = false;
}

/// Power on via RTS pulse: LOW 100ms -> HIGH 2000ms -> LOW.
/// This toggles power on the 1.3K/1.5K/2K-FA series.
fn power_on_rts_pulse(port: &mut Box<dyn serialport::SerialPort>) -> Result<(), String> {
    port.write_request_to_send(false)
        .map_err(|e| format!("RTS low: {}", e))?;
    std::thread::sleep(Duration::from_millis(100));
    port.write_request_to_send(true)
        .map_err(|e| format!("RTS high: {}", e))?;
    std::thread::sleep(Duration::from_millis(2000));
    port.write_request_to_send(false)
        .map_err(|e| format!("RTS low final: {}", e))?;
    Ok(())
}

/// Send a single-byte command to the SPE Expert.
/// Format: [0x55 0x55 0x55] [0x01] [command] [checksum]
/// For single-byte commands, checksum = command byte.
fn send_single_command(
    port: &mut Box<dyn serialport::SerialPort>,
    cmd: u8,
) -> Result<(), String> {
    let packet = [0x55, 0x55, 0x55, 0x01, cmd, cmd];
    port.write_all(&packet).map_err(|e| format!("write: {}", e))?;
    port.flush().map_err(|e| format!("flush: {}", e))?;
    Ok(())
}

/// Read a short ACK response after a command (non-STATUS).
/// ACK: [0xAA 0xAA 0xAA] [0x01] [echo] [echo]
/// We consume whatever comes back within a short timeout and log it.
fn read_ack(port: &mut Box<dyn serialport::SerialPort>) -> Result<(), String> {
    let mut buf = [0u8; 64];
    // Set a short timeout for ACK
    let _ = port.set_timeout(Duration::from_millis(500));
    match port.read(&mut buf) {
        Ok(n) if n > 0 => {
            debug!("SPE Expert ACK ({} bytes): {:02X?}", n, &buf[..n]);
            // Check if response contains ASCII text (e.g. "PC: xxx")
            if n > 4 {
                let text = String::from_utf8_lossy(&buf[..n]);
                if text.contains("PC") || text.contains(',') {
                    info!("SPE Expert response: {}", text.trim());
                }
            }
        }
        _ => {}
    }
    let _ = port.set_timeout(Duration::from_millis(2000));
    Ok(())
}

/// Send STATUS query (0x90) and parse the response.
fn query_status(port: &mut Box<dyn serialport::SerialPort>) -> Result<SpeStatus, String> {
    send_single_command(port, CMD_STATUS)?;

    // Read response frame
    let data = read_status_response(port)?;

    debug!("SPE Expert raw response ({} bytes): {:?}", data.len(), String::from_utf8_lossy(&data));

    parse_status_response(&data)
}

/// Read STATUS response. The response ends with CR LF, so we read until we see that.
/// Response: [0xAA 0xAA 0xAA] [CNT] [DATA...] [CHK_LO] [CHK_HI] [,] [CR] [LF]
fn read_status_response(port: &mut Box<dyn serialport::SerialPort>) -> Result<Vec<u8>, String> {
    let mut collected = Vec::with_capacity(128);
    let mut buf = [0u8; 128];

    loop {
        match port.read(&mut buf) {
            Ok(0) => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(n) => {
                collected.extend_from_slice(&buf[..n]);

                // Check if we have a complete frame (ends with CR LF)
                if collected.len() >= 2
                    && collected[collected.len() - 2] == 0x0D
                    && collected[collected.len() - 1] == 0x0A
                {
                    return extract_status_data(&collected);
                }

                // Preamble-aware overflow check.
                //
                // Valid SPE STATUS frames zijn ~77 bytes incl. CR/LF terminator.
                // Serial reads kunnen in chunks arriveren: read 1 kan 65+ bytes
                // opleveren zonder CR/LF terwijl de rest nog komt. Rauwe cutoff
                // op 64 bytes zou valide frames afbreken.
                //
                // Strategie: check of de collected-buffer start met SPE preamble
                // `0xAA 0xAA 0xAA`. Met preamble: ruime threshold (128 bytes) om
                // chunked reads op te vangen. Zonder preamble: fast-fail op 64
                // bytes — we hebben geen SPE-connectie en kunnen diagnostiek
                // loggen (hint naar verkeerde COM-poort).
                let has_preamble = collected.len() >= 3
                    && collected[0] == RESP_PREAMBLE
                    && collected[1] == RESP_PREAMBLE
                    && collected[2] == RESP_PREAMBLE;
                let threshold = if has_preamble { 128 } else { 64 };
                if collected.len() > threshold {
                    return Err(format_unexpected_data_error(&collected));
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                if collected.len() >= 2
                    && collected[collected.len() - 2] == 0x0D
                    && collected[collected.len() - 1] == 0x0A
                {
                    return extract_status_data(&collected);
                }
                return Err(format!(
                    "timeout ({} bytes collected, raw: {:02X?})",
                    collected.len(),
                    &collected[..collected.len().min(40)]
                ));
            }
            Err(e) => return Err(format!("read: {}", e)),
        }
    }
}

/// Bouw een error-message met ASCII-preview en pattern-sniff voor overflow in
/// `read_status_response`. Gedraagt zich anders afhankelijk van of de data
/// met SPE preamble `0xAA 0xAA 0xAA` start (valid frame dat geen CRLF vond
/// binnen budget) of niet (vreemd verkeer op poort — typisch CAT-data).
fn format_unexpected_data_error(collected: &[u8]) -> String {
    let preview_len = collected.len().min(64);
    let preview = &collected[..preview_len];

    // ASCII-preview (vervang non-printable door '.').
    let ascii: String = preview
        .iter()
        .map(|&b| if (0x20..0x7F).contains(&b) { b as char } else { '.' })
        .collect();

    let has_preamble = collected.len() >= 3
        && collected[0] == RESP_PREAMBLE
        && collected[1] == RESP_PREAMBLE
        && collected[2] == RESP_PREAMBLE;

    if has_preamble {
        return format!(
            "response overflow ({} bytes with SPE preamble but no CRLF terminator). \
             Frame may be malformed or cable noise. \
             ASCII preview: {:?}. Raw first 64 bytes: {:02X?}",
            collected.len(),
            ascii,
            preview
        );
    }

    // Geen preamble → vermoedelijk verkeerde COM-poort.
    let printable_count = preview
        .iter()
        .filter(|&&b| (0x20..0x7F).contains(&b))
        .count();
    let looks_ascii = printable_count * 10 >= preview.len() * 9;
    let has_cat_separator = preview.iter().any(|&b| b == b';' || b == b',');

    if looks_ascii && has_cat_separator {
        return "SPE CAT port detected — configure ThetisLink on SPE's PC-control \
                port instead (the port used by the SPE Windows app)"
            .to_string();
    }

    // Onbekend verkeer — diagnostiek behouden zodat we het later kunnen analyseren.
    format!(
        "response overflow ({} bytes, no SPE preamble 0xAA 0xAA 0xAA seen). \
         ASCII preview: {:?}. Raw first 64 bytes: {:02X?}",
        collected.len(),
        ascii,
        preview
    )
}

/// Extract the CSV data from a complete SPE response frame.
/// Frame: [0xAA 0xAA 0xAA] [CNT] [DATA(CNT bytes)] [CHK_LO] [CHK_HI] [,] [CR] [LF]
fn extract_status_data(raw: &[u8]) -> Result<Vec<u8>, String> {
    // Find preamble 0xAA 0xAA 0xAA
    let mut preamble_pos = None;
    for i in 0..raw.len().saturating_sub(3) {
        if raw[i] == RESP_PREAMBLE && raw[i + 1] == RESP_PREAMBLE && raw[i + 2] == RESP_PREAMBLE {
            preamble_pos = Some(i);
            break;
        }
    }

    let start = preamble_pos.ok_or_else(|| {
        format!("no 0xAA preamble found in {} bytes: {:02X?}", raw.len(), &raw[..raw.len().min(20)])
    })?;

    let cnt_pos = start + 3;
    if cnt_pos >= raw.len() {
        return Err("frame too short for CNT byte".to_string());
    }
    let cnt = raw[cnt_pos] as usize;

    let data_start = cnt_pos + 1;
    let data_end = data_start + cnt;

    // After data: CHK_LO(1) + CHK_HI(1) + separator(1) + CR(1) + LF(1) = 5 bytes
    let frame_end = data_end + 5;
    if frame_end > raw.len() {
        return Err(format!(
            "frame incomplete: need {} bytes but have {}, CNT={}",
            frame_end, raw.len(), cnt
        ));
    }

    // Verify CR LF at end
    if raw[frame_end - 2] != 0x0D || raw[frame_end - 1] != 0x0A {
        return Err(format!(
            "no CRLF at expected position, got {:02X} {:02X}",
            raw[frame_end - 2], raw[frame_end - 1]
        ));
    }

    Ok(raw[data_start..data_end].to_vec())
}

/// Parse the STATUS response CSV data into SpeStatus.
///
/// The 67-byte data starts with a leading comma:
///   ,13K,S,R,x,1,00,1a,0r,L,0000, 0.00, 0.00, 0.0, 0.0, 33, 0, 0,N,N
///
/// Split by comma gives (0-indexed):
///   [0]=""  [1]=model  [2]=state  [3]=ptt  [4]=mem_bank  [5]=input
///   [6]=band  [7]=tx_ant+atu  [8]=rx_ant  [9]=power_level  [10]=fwd_power
///   [11]=swr_atu  [12]=swr_ant  [13]=voltage  [14]=current
///   [15]=temp_upper  [16]=temp_lower  [17]=temp_combiner  [18]=warning  [19]=alarm
fn parse_status_response(data: &[u8]) -> Result<SpeStatus, String> {
    let text = String::from_utf8_lossy(data);
    let fields: Vec<&str> = text.split(',').collect();

    if fields.len() < 18 {
        return Err(format!(
            "too few fields in status: {} (expected >=18), text: {}",
            fields.len(), text
        ));
    }

    let mut s = SpeStatus::default();
    s.connected = true;

    // [2] State: S=Standby, O=Operate
    s.state = match fields[2].trim() {
        "O" => 2,
        "S" => 1,
        _ => 0,
    };

    // [3] PTT: R=RX, T=TX
    s.ptt = fields[3].trim() == "T";

    // [5] Input selector
    s.input = fields[5].trim().parse().unwrap_or(0);

    // [6] Band code (string "00"-"11")
    s.band = fields[6].trim().parse().unwrap_or(0);

    // [7] TX antenna + ATU status (e.g. "1a", "2b", "1t")
    //     first char = antenna number, second char = ATU status (b=bypass, a=active, t=tunable)
    let ant_field = fields[7].trim();
    s.antenna = ant_field.chars().next()
        .and_then(|c| c.to_digit(10))
        .unwrap_or(0) as u8;
    s.atu_bypassed = ant_field.chars().nth(1) == Some('b');

    // [9] Power level: L=0, M=1, H=2
    s.power_level = match fields[9].trim() {
        "L" => 0,
        "M" => 1,
        "H" => 2,
        _ => 0,
    };

    // [10] Forward power (watts)
    s.forward_power = fields[10].trim().parse().unwrap_or(0);

    // [11] SWR (ATU side) — multiply by 10 for integer storage
    if let Ok(swr) = fields[11].trim().parse::<f32>() {
        s.swr_x10 = (swr * 10.0) as u16;
    }

    // [13] Voltage
    if let Ok(v) = fields[13].trim().parse::<f32>() {
        s.voltage_x10 = (v * 10.0) as u16;
    }

    // [14] Current
    if let Ok(i) = fields[14].trim().parse::<f32>() {
        s.current_x10 = (i * 10.0) as u16;
    }

    // [15-17] Temperatures: take max of available sensors
    let t1: u8 = fields[15].trim().parse().unwrap_or(0);
    let t2: u8 = fields.get(16).and_then(|f| f.trim().parse().ok()).unwrap_or(0);
    let t3: u8 = fields.get(17).and_then(|f| f.trim().parse().ok()).unwrap_or(0);
    s.temp = t1.max(t2).max(t3);

    // [18] Warning
    if let Some(f) = fields.get(18) {
        s.warning = f.trim().bytes().next().unwrap_or(b'N');
    }

    // [19] Alarm
    if let Some(f) = fields.get(19) {
        s.alarm = f.trim().bytes().next().unwrap_or(b'N');
    }

    Ok(s)
}

/// Format SPE status as labels CSV string for telemetry transmission.
/// Format: "ptt,power_w,swr_x10,temp,voltage_x10,current_x10,warning,alarm,power_level,antenna,input"
pub fn status_labels_string(status: &SpeStatus) -> String {
    format!(
        "{},{},{},{},{},{},{},{},{},{},{},{}",
        if status.ptt { "T" } else { "R" },
        status.forward_power,
        status.swr_x10,
        status.temp,
        status.voltage_x10,
        status.current_x10,
        status.warning as char,
        status.alarm as char,
        status.power_level,
        status.antenna,
        status.input,
        if status.atu_bypassed { "1" } else { "0" },
    )
}

/// Convert SPE band code to a human-readable band name.
/// Band codes: 00=160m, 01=80m, 02=60m, ..., 10=6m, 11=4m
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
        11 => "4m",
        _ => "?",
    }
}

/// Convert power level code to display string.
pub fn power_level_name(level: u8) -> &'static str {
    match level {
        0 => "L",
        1 => "M",
        2 => "H",
        _ => "?",
    }
}

// ============================================================================
// Tests — classifier, transition function, adaptive polling, replay sessions.
// See PATCH-spe-expert-polling-state §1.8 and §1.11.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(fails: u32) -> TransitionCtx {
        TransitionCtx {
            consecutive_fails: fails,
            offline_since: None,
            tx_active: false,
        }
    }

    // --- classifier: minimum 8 cases, one per variant ------------------------

    #[test]
    fn classifier_empty_timeout_is_no_data() {
        assert_eq!(
            classify_poll_failure(&[], PollIoKind::Timeout),
            PollFailureKind::TimeoutNoData
        );
    }

    #[test]
    fn classifier_null_only_timeout() {
        assert_eq!(
            classify_poll_failure(&[0u8; 10], PollIoKind::Timeout),
            PollFailureKind::TimeoutNullOnly
        );
    }

    #[test]
    fn classifier_partial_preamble_is_timeout_partial() {
        let bytes = [0xAA, 0xAA, 0xAA, 0x05, 0x01];
        assert_eq!(
            classify_poll_failure(&bytes, PollIoKind::Timeout),
            PollFailureKind::TimeoutPartial
        );
    }

    #[test]
    fn classifier_overflow_with_preamble() {
        let bytes = [0xAAu8; 130];
        assert_eq!(
            classify_poll_failure(&bytes, PollIoKind::Timeout),
            PollFailureKind::OverflowWithPreamble
        );
    }

    #[test]
    fn classifier_cat_port_mismatch() {
        let cat = b"PC;IF;PC;IF;PC;IF;PC;IF;PC;IF;PC;IF;PC;IF;PC;IF;PC;IF;PC;IF;PC;IF;";
        assert_eq!(
            classify_poll_failure(cat, PollIoKind::Timeout),
            PollFailureKind::CatPortMismatch
        );
    }

    #[test]
    fn classifier_overflow_unknown_non_ascii() {
        let bytes = [0xFFu8; 80];
        assert_eq!(
            classify_poll_failure(&bytes, PollIoKind::Timeout),
            PollFailureKind::OverflowUnknown
        );
    }

    #[test]
    fn classifier_io_error() {
        assert_eq!(
            classify_poll_failure(&[0x01, 0x02], PollIoKind::Read),
            PollFailureKind::IoError
        );
    }

    #[test]
    fn classifier_parse_error() {
        // Any payload with Parse kind → ParseError regardless of content
        assert_eq!(
            classify_poll_failure(&[0xAA, 0xAA, 0xAA, 0x01, 0x00], PollIoKind::Parse),
            PollFailureKind::ParseError
        );
    }

    // --- state transitions: minimum 10 cases, one per arrow in §1.2 ----------

    #[test]
    fn unknown_initial_ok_goes_to_warmingup_with_log() {
        let (s, log) = next_state(
            SpeConnState::Unknown,
            SpeEvent::InitialOk { state: 1 },
            &ctx(0),
        );
        assert_eq!(s, SpeConnState::WarmingUp);
        assert!(matches!(log, Some(TransitionLog::Info(ref m)) if m.contains("state=1")));
    }

    #[test]
    fn unknown_initial_fail_goes_offline_silently() {
        let (s, log) = next_state(SpeConnState::Unknown, SpeEvent::InitialFail, &ctx(0));
        assert_eq!(s, SpeConnState::Offline);
        assert!(log.is_none());
    }

    #[test]
    fn warmingup_poll_ok_becomes_online() {
        let (s, log) = next_state(SpeConnState::WarmingUp, SpeEvent::PollOk, &ctx(0));
        assert_eq!(s, SpeConnState::Online);
        assert!(matches!(log, Some(TransitionLog::Info(ref m)) if m.contains("online")));
    }

    #[test]
    fn warmingup_three_null_timeouts_stays_warmingup_no_log() {
        let mut state = SpeConnState::WarmingUp;
        let mut fails = 0;
        for _ in 0..3 {
            let (s, log) = next_state(
                state,
                SpeEvent::PollFail(PollFailureKind::TimeoutNullOnly),
                &ctx(fails),
            );
            state = s;
            fails += 1;
            assert!(log.is_none(), "null-only timeouts must be silent in WarmingUp");
        }
        assert_eq!(state, SpeConnState::WarmingUp);
    }

    #[test]
    fn warmingup_hard_fail_threshold_goes_offline() {
        let (s, log) = next_state(
            SpeConnState::WarmingUp,
            SpeEvent::PollFail(PollFailureKind::OverflowWithPreamble),
            &ctx(2), // +1 = 3, hits threshold
        );
        assert_eq!(s, SpeConnState::Offline);
        assert!(matches!(log, Some(TransitionLog::Warn(ref m)) if m.contains("did not come online")));
    }

    #[test]
    fn online_five_fails_goes_offline() {
        let (s, log) = next_state(
            SpeConnState::Online,
            SpeEvent::PollFail(PollFailureKind::TimeoutNoData),
            &ctx(4), // +1 = 5, hits threshold
        );
        assert_eq!(s, SpeConnState::Offline);
        assert!(matches!(log, Some(TransitionLog::Info(ref m)) if m.contains("offline")));
    }

    #[test]
    fn offline_poll_ok_back_online() {
        let (s, log) = next_state(SpeConnState::Offline, SpeEvent::PollOk, &ctx(0));
        assert_eq!(s, SpeConnState::Online);
        assert!(matches!(log, Some(TransitionLog::Info(ref m)) if m.contains("back online")));
    }

    #[test]
    fn online_cat_mismatch_transitions_once() {
        let (s, log) = next_state(
            SpeConnState::Online,
            SpeEvent::PollFail(PollFailureKind::CatPortMismatch),
            &ctx(0),
        );
        assert_eq!(s, SpeConnState::CatMisconfigured);
        assert!(matches!(log, Some(TransitionLog::WarnCatMisconfigured)));
    }

    #[test]
    fn cat_misconfigured_poll_ok_resolves() {
        let (s, log) = next_state(SpeConnState::CatMisconfigured, SpeEvent::PollOk, &ctx(0));
        assert_eq!(s, SpeConnState::Online);
        assert!(matches!(log, Some(TransitionLog::Info(ref m)) if m.contains("resolved")));
    }

    #[test]
    fn online_io_error_goes_offline_immediately() {
        let (s, log) = next_state(
            SpeConnState::Online,
            SpeEvent::PollFail(PollFailureKind::IoError),
            &ctx(0),
        );
        assert_eq!(s, SpeConnState::Offline);
        assert!(matches!(log, Some(TransitionLog::Info(_))));
    }

    #[test]
    fn offline_poll_fail_stays_silent() {
        let (s, log) = next_state(
            SpeConnState::Offline,
            SpeEvent::PollFail(PollFailureKind::TimeoutNoData),
            &ctx(100),
        );
        assert_eq!(s, SpeConnState::Offline);
        assert!(log.is_none());
    }

    #[test]
    fn cat_misconfigured_repeat_is_silent() {
        let (s, log) = next_state(
            SpeConnState::CatMisconfigured,
            SpeEvent::PollFail(PollFailureKind::CatPortMismatch),
            &ctx(50),
        );
        assert_eq!(s, SpeConnState::CatMisconfigured);
        assert!(log.is_none());
    }

    // --- adaptive polling intervals -----------------------------------------

    #[test]
    fn poll_interval_tx_is_fast() {
        assert_eq!(
            poll_interval(SpeConnState::Online, true),
            Duration::from_millis(75)
        );
    }

    #[test]
    fn poll_interval_rx_online_slow() {
        assert_eq!(
            poll_interval(SpeConnState::Online, false),
            Duration::from_millis(1000)
        );
    }

    #[test]
    fn poll_interval_offline_slowest() {
        assert_eq!(
            poll_interval(SpeConnState::Offline, false),
            Duration::from_millis(2000)
        );
        assert_eq!(
            poll_interval(SpeConnState::CatMisconfigured, true),
            Duration::from_millis(2000)
        );
    }

    #[test]
    fn poll_interval_warmup_moderate() {
        assert_eq!(
            poll_interval(SpeConnState::WarmingUp, false),
            Duration::from_millis(500)
        );
    }

    // --- replay tests (§1.11 acceptance criteria 3 & 4) ---------------------

    /// Drives the state machine through an event list, collecting transitions
    /// and log lines for assertion.
    fn drive(events: &[SpeEvent]) -> (SpeConnState, Vec<(SpeConnState, SpeConnState, bool)>) {
        let mut state = SpeConnState::Unknown;
        let mut fails: u32 = 0;
        let mut transitions = Vec::new();
        for ev in events {
            let ctx = TransitionCtx {
                consecutive_fails: fails,
                offline_since: None,
                tx_active: false,
            };
            let (new, log) = next_state(state, *ev, &ctx);
            match ev {
                SpeEvent::PollOk | SpeEvent::InitialOk { .. } => fails = 0,
                _ => fails += 1,
            }
            let logged = log.is_some();
            if state != new {
                transitions.push((state, new, logged));
            }
            state = new;
        }
        (state, transitions)
    }

    #[test]
    fn replay_session_1_warmup_noise_then_online() {
        // sessie-1: state=1, 2× timeout 0 bytes, 1× null-only, then Ok.
        // Expected: WarmingUp → Online, no false disconnect, 2 transitions total
        // (Unknown→WarmingUp, WarmingUp→Online), both logged.
        let events = vec![
            SpeEvent::InitialOk { state: 1 },
            SpeEvent::PollFail(PollFailureKind::TimeoutNoData),
            SpeEvent::PollFail(PollFailureKind::TimeoutNoData),
            SpeEvent::PollFail(PollFailureKind::TimeoutNullOnly),
            SpeEvent::PollFail(PollFailureKind::TimeoutNullOnly),
            SpeEvent::PollFail(PollFailureKind::TimeoutNullOnly),
            SpeEvent::PollFail(PollFailureKind::TimeoutNullOnly),
            SpeEvent::PollOk,
        ];
        let (end, transitions) = drive(&events);
        assert_eq!(end, SpeConnState::Online);
        assert_eq!(
            transitions.len(),
            2,
            "expected exactly 2 transitions (Unknown→WarmingUp, WarmingUp→Online), got {:?}",
            transitions
        );
        assert!(
            !transitions
                .iter()
                .any(|(_, to, _)| *to == SpeConnState::Offline),
            "must not transition to Offline during warm-up"
        );
    }

    #[test]
    fn replay_session_3_cat_port_once_then_silent() {
        // sessie-3: CAT on COM6, first poll triggers CatPortMismatch.
        // Expected: one transition to CatMisconfigured (logged), subsequent
        // repeats stay silent.
        let events = vec![
            SpeEvent::InitialFail,
            SpeEvent::PollFail(PollFailureKind::CatPortMismatch),
            SpeEvent::PollFail(PollFailureKind::CatPortMismatch),
            SpeEvent::PollFail(PollFailureKind::CatPortMismatch),
            SpeEvent::PollFail(PollFailureKind::CatPortMismatch),
        ];
        let (end, transitions) = drive(&events);
        assert_eq!(end, SpeConnState::CatMisconfigured);
        // Unknown → Offline (silent), Offline → CatMisconfigured (logged).
        // Only 1 CAT-warn transition expected.
        let cat_transitions: Vec<_> = transitions
            .iter()
            .filter(|(_, to, _)| *to == SpeConnState::CatMisconfigured)
            .collect();
        assert_eq!(
            cat_transitions.len(),
            1,
            "expected exactly 1 transition into CatMisconfigured"
        );
        assert!(cat_transitions[0].2, "CAT transition must be logged");
    }

    // Regression guard: verify counter semantics at the call-site.
    // The pure state-machine adds +1 inside its threshold check, so the caller
    // MUST pass the pre-fail counter and only increment for kinds that count.
    #[test]
    fn counts_toward_threshold_matches_brief_1_4() {
        use PollFailureKind::*;
        use SpeConnState::*;

        // WarmingUp: only hard protocol errors count
        assert!(!counts_toward_threshold(WarmingUp, TimeoutNoData));
        assert!(!counts_toward_threshold(WarmingUp, TimeoutNullOnly));
        assert!(!counts_toward_threshold(WarmingUp, TimeoutPartial));
        assert!(counts_toward_threshold(WarmingUp, OverflowWithPreamble));
        assert!(counts_toward_threshold(WarmingUp, OverflowUnknown));
        assert!(counts_toward_threshold(WarmingUp, ParseError));
        assert!(!counts_toward_threshold(WarmingUp, IoError));
        assert!(!counts_toward_threshold(WarmingUp, CatPortMismatch));

        // Online: all non-directe count
        assert!(counts_toward_threshold(Online, TimeoutNoData));
        assert!(counts_toward_threshold(Online, TimeoutNullOnly));
        assert!(counts_toward_threshold(Online, OverflowWithPreamble));
        assert!(!counts_toward_threshold(Online, IoError));
        assert!(!counts_toward_threshold(Online, CatPortMismatch));

        // Offline / CatMisconfigured / Unknown: nothing counts
        for k in [
            TimeoutNoData,
            TimeoutNullOnly,
            TimeoutPartial,
            OverflowWithPreamble,
            OverflowUnknown,
            ParseError,
            IoError,
            CatPortMismatch,
        ] {
            assert!(!counts_toward_threshold(Offline, k));
            assert!(!counts_toward_threshold(CatMisconfigured, k));
            assert!(!counts_toward_threshold(Unknown, k));
        }
    }

    /// Simulates the call-site counter logic end-to-end so we catch the exact
    /// off-by-one: WarmingUp→Offline after the 3rd hard fail, not the 2nd.
    fn drive_with_callsite_counter(
        start: SpeConnState,
        events: &[SpeEvent],
    ) -> (SpeConnState, Vec<(SpeConnState, SpeConnState)>) {
        let mut state = start;
        let mut fails: u32 = 0;
        let mut transitions = Vec::new();
        for ev in events {
            let ctx = TransitionCtx {
                consecutive_fails: fails,
                offline_since: None,
                tx_active: false,
            };
            let (new, _log) = next_state(state, *ev, &ctx);
            match ev {
                SpeEvent::PollOk | SpeEvent::InitialOk { .. } => fails = 0,
                SpeEvent::PollFail(kind) => {
                    if state != new {
                        fails = 0;
                    } else if counts_toward_threshold(state, *kind) {
                        fails = fails.saturating_add(1);
                    }
                }
                SpeEvent::InitialFail => fails = 0,
            }
            if state != new {
                transitions.push((state, new));
            }
            state = new;
        }
        (state, transitions)
    }

    #[test]
    fn warmup_hard_fails_transition_on_third_not_second() {
        use PollFailureKind::*;

        // 2 hard fails → stays WarmingUp
        let (s2, t2) = drive_with_callsite_counter(
            SpeConnState::WarmingUp,
            &[
                SpeEvent::PollFail(OverflowWithPreamble),
                SpeEvent::PollFail(OverflowWithPreamble),
            ],
        );
        assert_eq!(s2, SpeConnState::WarmingUp);
        assert!(t2.is_empty(), "2 hard fails must not transition");

        // 3 hard fails → Offline
        let (s3, t3) = drive_with_callsite_counter(
            SpeConnState::WarmingUp,
            &[
                SpeEvent::PollFail(OverflowWithPreamble),
                SpeEvent::PollFail(OverflowWithPreamble),
                SpeEvent::PollFail(OverflowWithPreamble),
            ],
        );
        assert_eq!(s3, SpeConnState::Offline);
        assert_eq!(t3.len(), 1);
    }

    #[test]
    fn online_fails_transition_on_fifth_not_fourth() {
        use PollFailureKind::*;

        // 4 fails → stays Online
        let (s4, t4) = drive_with_callsite_counter(
            SpeConnState::Online,
            &[SpeEvent::PollFail(TimeoutNoData); 4],
        );
        assert_eq!(s4, SpeConnState::Online);
        assert!(t4.is_empty(), "4 fails must not transition");

        // 5 fails → Offline
        let (s5, t5) = drive_with_callsite_counter(
            SpeConnState::Online,
            &[SpeEvent::PollFail(TimeoutNoData); 5],
        );
        assert_eq!(s5, SpeConnState::Offline);
        assert_eq!(t5.len(), 1);
    }

    #[test]
    fn warmup_soft_timeouts_do_not_prime_the_counter() {
        use PollFailureKind::*;

        // 100 soft timeouts followed by 2 hard fails: must still stay WarmingUp
        // because soft timeouts don't count, and 2 hard < threshold 3.
        let mut events: Vec<SpeEvent> = Vec::new();
        for _ in 0..100 {
            events.push(SpeEvent::PollFail(TimeoutNullOnly));
        }
        events.push(SpeEvent::PollFail(OverflowWithPreamble));
        events.push(SpeEvent::PollFail(OverflowWithPreamble));

        let (s, t) = drive_with_callsite_counter(SpeConnState::WarmingUp, &events);
        assert_eq!(s, SpeConnState::WarmingUp);
        assert!(t.is_empty(), "soft timeouts must not prime the threshold");
    }

    #[test]
    fn replay_spe_stays_off_silent_after_threshold() {
        // SPE uit scenario: initial fail → Offline (silent), poll fails forever
        // → stay Offline with no further logs.
        let mut events = vec![SpeEvent::InitialFail];
        for _ in 0..30 {
            events.push(SpeEvent::PollFail(PollFailureKind::TimeoutNoData));
        }
        let (end, transitions) = drive(&events);
        assert_eq!(end, SpeConnState::Offline);
        // Only one transition (Unknown→Offline), and it was silent.
        assert_eq!(transitions.len(), 1);
        assert!(!transitions[0].2, "Unknown→Offline must be silent");
    }
}
