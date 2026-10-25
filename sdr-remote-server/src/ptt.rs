// SPDX-License-Identifier: GPL-2.0-or-later

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use sdr_remote_core::dev_info;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sdr_remote_core::thetis_power::{forget_shutdown, power_on_step, PowerOnInputs, PowerOnStep};
use log::{error, info, warn};

use crate::tci::TciConnection;

/// Launch an application using Windows ShellExecuteW (works from elevated processes)
fn shell_execute_open(path: &str) -> bool {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    extern "system" {
        fn ShellExecuteW(
            hwnd: *mut std::ffi::c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show_cmd: i32,
        ) -> *mut std::ffi::c_void;
    }

    fn to_wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    let operation = to_wide("open");
    let file = to_wide(path);
    const SW_SHOWNORMAL: i32 = 1;

    let result = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    };

    // ShellExecuteW returns > 32 on success
    (result as usize) > 32
}

/// Check if a process is running using Windows API (no console window)
fn is_process_running(name: &str) -> bool {
    !process_ids(name).is_empty()
}

/// Every Thetis.exe process id on this PC, lowest first. Empty when Thetis
/// runs elsewhere (TCI over the network) or not at all. A Thetis run is a
/// process (build 226, review of 224): a TCI connection that comes back to the
/// same process is not a new run - and only one process says which that is
/// (build 227, review of 226).
pub fn thetis_process_ids() -> Vec<u32> {
    let mut ids = process_ids("Thetis.exe");
    ids.sort_unstable();
    ids
}

/// The ids of every process with this executable name (no console window).
fn process_ids(name: &str) -> Vec<u32> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    #[repr(C)]
    #[allow(non_snake_case)]
    struct PROCESSENTRY32W {
        dwSize: u32,
        cntUsage: u32,
        th32ProcessID: u32,
        th32DefaultHeapID: usize,
        th32ModuleID: u32,
        cntThreads: u32,
        th32ParentProcessID: u32,
        pcPriClassBase: i32,
        dwFlags: u32,
        szExeFile: [u16; 260],
    }

    extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> *mut std::ffi::c_void;
        fn Process32FirstW(snapshot: *mut std::ffi::c_void, entry: *mut PROCESSENTRY32W) -> i32;
        fn Process32NextW(snapshot: *mut std::ffi::c_void, entry: *mut PROCESSENTRY32W) -> i32;
        fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    }

    const TH32CS_SNAPPROCESS: u32 = 0x00000002;
    const INVALID_HANDLE_VALUE: *mut std::ffi::c_void = -1isize as *mut std::ffi::c_void;

    let target: Vec<u16> = OsStr::new(name).encode_wide().collect();

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Vec::new();
        }

        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        let mut ids = Vec::new();
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                let exe_len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(260);
                let exe_name = &entry.szExeFile[..exe_len];
                if exe_name.len() == target.len()
                    && exe_name.iter().zip(target.iter()).all(|(&a, &b)| {
                        // Case-insensitive compare for ASCII range
                        let la = if a >= b'A' as u16 && a <= b'Z' as u16 { a + 32 } else { a };
                        let lb = if b >= b'A' as u16 && b <= b'Z' as u16 { b + 32 } else { b };
                        la == lb
                    })
                {
                    ids.push(entry.th32ProcessID);
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }

        CloseHandle(snapshot);
        ids
    }
}

/// Timeout for Thetis launch (seconds)
const THETIS_LAUNCH_TIMEOUT_S: u64 = 60;

/// Safety timeout: release PTT if no packets received for this duration.
///
/// Raised from 500ms after four observed handovers: walking out of wifi range
/// while transmitting dropped the transmitter every time, because a mobile
/// handover stalls the packet stream for well over half a second even when the
/// connection itself survives - three of those four kept their tunnel alive
/// throughout. Half a second is stricter than the network is reliable.
///
/// Deliberately equal to `HEARTBEAT_TIMEOUT_MS`, and it must never exceed it:
/// that one is the window after which the client counts as gone, and a
/// transmitter may not stay keyed past the moment nobody is known to be holding
/// it. So the worst case - keyed with no contact at all - is 2s either way, and
/// which of the two lines appears says whether the tunnel was still alive.
pub(crate) const PTT_PACKET_TIMEOUT_MS: u64 = 2000;

/// Safety timeout: release PTT if heartbeat lost for this duration.
///
/// The client sends one every 500ms, so this is four missed heartbeats of slack.
const HEARTBEAT_TIMEOUT_MS: u64 = 2000;

/// Minimum PTT tail delay in ms (CAT+VB-Cable mode)
const PTT_TAIL_MIN_MS: u64 = 80;
/// Extra margin on top of jitter buffer depth (CAT+VB-Cable mode)
const PTT_TAIL_MARGIN_MS: u64 = 40;
/// Prefill delay (CAT+VB-Cable mode): audio needs to traverse cpal/VB-Cable pipeline
const PTT_PREFILL_MS: u64 = 60;

/// TCI mode: minimal delays (direct WebSocket, no VB-Cable pipeline)
const PTT_TAIL_MIN_MS_TCI: u64 = 25;
const PTT_TAIL_MARGIN_MS_TCI: u64 = 10;
const PTT_PREFILL_MS_TCI: u64 = 10;

/// How long a clock has been silent, for a log line. A clock that was never set
/// reads as "never", which is a different thing from a long gap and must not print
/// as one.
fn describe_silence(since: Option<Duration>) -> String {
    match since {
        Some(d) => format!("{:.1}s", d.as_secs_f32()),
        None => "never (no reading at all)".to_string(),
    }
}

/// What the safety layer wants done with the transmitter, this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SafetyAction {
    /// Leave it alone.
    None,
    /// Nothing has been heard from the client at all: release, and say so loudly.
    ReleaseHeartbeatLost,
    /// The client is still there but its transmit stream stopped: release.
    ReleaseNoPackets,
}

/// Named fields since build 229 (review of 228): two clocks and two windows of
/// the same type in a row. Equal windows today, so a swap only turned the
/// reason in the log round - but this is the rule that keeps a PTT from
/// hanging (`PTT-NOOIT-HANGEN.md`).
///
/// Decide, from two clocks, whether the transmitter must be unkeyed.
///
/// Free and testable because everything around it talks to a radio. The two tests
/// this file first had asserted a relation and a lower bound between the constants,
/// and a review showed both survive setting each constant to a full minute - so they
/// guarded nothing that this layer exists for. This is what can actually be asserted:
/// what the layer DOES, per tick, for every shape the clocks can take.
///
/// A missing clock counts as expired, not as "no opinion". Both checks used to sit
/// behind `if let Some(..)`, so a clock that was never set silently switched its
/// guard off - and a guard whose failure mode is "stay keyed" has the wrong default.
/// The path that reaches Tx today always records a packet first, so this is a
/// belt-and-braces change rather than a fix for something observed; it costs nothing
/// and it removes a way for a later caller to be surprised.
///
/// Heartbeat before packets, so on a total loss the louder and more informative line
/// wins the tie when both windows are equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SafetyClocks {
    state: PttState,
    since_heartbeat: Option<Duration>,
    since_packet: Option<Duration>,
    heartbeat_timeout: Duration,
    packet_timeout: Duration,
}

impl SafetyClocks {
    fn action(self) -> SafetyAction {
        if self.state != PttState::Tx {
            return SafetyAction::None;
        }
        if self.since_heartbeat.map_or(true, |d| d > self.heartbeat_timeout) {
            return SafetyAction::ReleaseHeartbeatLost;
        }
        if self.since_packet.map_or(true, |d| d > self.packet_timeout) {
            return SafetyAction::ReleaseNoPackets;
        }
        SafetyAction::None
    }
}

/// Did the client's silence end what the transmit buffer holds (build 248)?
/// Past the packet window of the safety, with the same comparison: where the
/// safety lets the transmitter go, the audio waiting in the buffer from before
/// the silence is no longer this over's - played first, with PTT, it keyed
/// Thetis with speech seconds old (round 247). Never a packet: nothing of it
/// is buffered.
pub fn silence_ended_the_over(since_packet: Option<Duration>) -> bool {
    since_packet.is_some_and(|d| d > Duration::from_millis(PTT_PACKET_TIMEOUT_MS))
}

/// The tests spell the clocks by position; built by name here (build 229).
#[cfg(test)]
fn safety_action(
    state: PttState,
    since_heartbeat: Option<Duration>,
    since_packet: Option<Duration>,
    heartbeat_timeout: Duration,
    packet_timeout: Duration,
) -> SafetyAction {
    SafetyClocks { state, since_heartbeat, since_packet, heartbeat_timeout, packet_timeout }.action()
}

/// PTT state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PttState {
    Rx,
    Tx,
}

/// What the `tx_blocked` gate means, for the three places that log it. Two
/// things shut it: an RX-only Amplitec position, and the server's safety task
/// stopping (`network.rs` `safety_task_lost`) - the log used to name only the
/// first (review build 57).
const TX_BLOCKED_WHY: &str =
    "transmitting is blocked (an RX-only Amplitec position, or the safety task stopped)";

/// PTT controller with safety layers.
///
/// Safety layers:
/// 1. PTT state in every audio packet (checked 50x/sec)
/// 2. Burst of 5 packets on PTT state change (handled by client)
/// 3. PTT_PACKET_TIMEOUT_MS without packets -> PTT released
/// 4. 2s heartbeat timeout -> connection lost, PTT released + alarm
/// 5. PTT tail: delay on Tx->Rx to let audio pipeline drain
pub struct PttController {
    state: PttState,
    last_ptt_packet: Option<Instant>,
    last_heartbeat: Option<Instant>,
    pending_activate: Option<Instant>,
    pending_release: Option<Instant>,
    tail_delay_ms: u64,
    ptt_active: Arc<AtomicBool>,
    /// TCI WebSocket connection - TL2 v2 is TCI-only, no CAT backend.
    pub tci: TciConnection,
    thetis_path: Option<String>,
    pending_power_on: bool,
    thetis_launch_time: Option<Instant>,
    /// When we asked Thetis to close, if it has not gone yet.
    ///
    /// `shutdown_ex;` is a request over TCI and closing takes seconds. Without
    /// this, a power-on arriving in that window sees a process on its way out,
    /// calls it "already running", and waits for a connection that cannot come
    /// (PD0PLK, report 67, 2026-09-01).
    thetis_shutdown_time: Option<Instant>,
    /// A power-on waiting for a closing Thetis to actually disappear.
    thetis_relaunch_when_gone: bool,
    /// Hard TX-gate. `true` = the current Amplitec-A position is
    /// marked as RX-only in `config.amplitec_tx_blocked`; in that case all
    /// server-initiated TX paths refuse to send a TX command
    /// to Thetis. Written by the equipment tick in `network.rs`, every 200 ms,
    /// with or without a client (build 218), and set for good by
    /// `safety_task_lost` when that task has died.
    tx_blocked: Arc<AtomicBool>,
    /// The active RX-only position is one Thetis cannot guard by itself
    /// (`RxOnlyGuard::unguarded`). Written by the RX-only loop in `network.rs`,
    /// read by the heartbeat and the server window: one place decides, the
    /// others read it by name (build 217).
    rx_only_unguarded: Arc<AtomicBool>,
    /// Throttle for the WARN log when a PTT request is
    /// refused - otherwise every audio frame (50/sec) generates a log.
    last_blocked_log: Option<Instant>,
    // --- Latency metrics ---
    ptt_prefill_start: Option<Instant>,
    ptt_release_start: Option<Instant>,
    ptt_prefill_latencies: Vec<u64>,
    ptt_tail_latencies: Vec<u64>,
    /// When TRX last went off, waiting for the first RX audio that is not silence.
    rx_back: RxBackWatch,
}

/// The time from "TRX off" to the first block of RX audio from Thetis that is not
/// silence - the part of the turnaround that sits after our tail, measured on one
/// clock (`PLAN-meting-buffers-en-omschakeltijd.md`, 2026-10-04). Shared with the
/// RX audio loop as one atomic: no lock on the audio path, and one load per frame
/// when nothing is waited for.
#[derive(Clone, Debug, Default)]
pub struct RxBackWatch {
    /// ms on `turnaround_clock()` when TRX went off; 0 = not waiting.
    off_at: Arc<AtomicU64>,
    /// Put before the line: empty for Thetis (its line stays as it was),
    /// "Radio 1 " or "Radio 2 " for a Yaesu (build 163).
    prefix: &'static str,
}

/// Peak below which an RX block counts as the silence Thetis sends during TX
/// (-80 dBFS). Band noise on a receiver sits far above it.
const RX_BACK_PEAK: f32 = 1e-4;

/// A wait older than this is dropped without a line: the RX loop skips its blocks
/// while no client is there, and a client returning minutes later would otherwise
/// report minutes as a turnaround.
const RX_BACK_GIVE_UP_MS: u64 = 10_000;

pub(crate) fn turnaround_clock() -> u64 {
    static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    BASE.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

impl RxBackWatch {
    /// The same watch for a Yaesu radio, its lines starting with `prefix`.
    pub fn for_radio(prefix: &'static str) -> Self {
        Self { prefix, ..Default::default() }
    }

    pub(crate) fn trx_off(&self) {
        self.off_at.store(turnaround_clock(), Ordering::Relaxed);
    }

    pub(crate) fn trx_on(&self) {
        self.off_at.store(0, Ordering::Relaxed);
    }

    /// No client hears Thetis any more: a wait running now would measure the
    /// gap until one returns, not the turnaround (round 99: a 1.4 s network
    /// switch would have logged as "RX audio back 1400 ms"). Called by the RX
    /// loop when it finds no client.
    pub fn cancel(&self) {
        self.off_at.store(0, Ordering::Relaxed);
    }

    /// One block of RX1 audio as it leaves Thetis; `backlog_samples` is what
    /// still waits behind it in the accumulator. Logs once per turnaround.
    pub fn rx_block(&self, block: &[f32], backlog_samples: usize) {
        let _ = self.rx_block_at(turnaround_clock(), block, backlog_samples);
    }

    /// `rx_block` at a given time on `turnaround_clock()` - the clock as an
    /// argument, so a test can stand a wait in the past. Returns the turnaround
    /// it reported, so a test can tell "reported" from "cancelled" (round 99).
    pub(crate) fn rx_block_at(&self, now_ms: u64, block: &[f32], backlog_samples: usize) -> Option<u64> {
        let off_at = self.off_at.load(Ordering::Relaxed);
        if off_at == 0 {
            return None;
        }
        if now_ms.saturating_sub(off_at) > RX_BACK_GIVE_UP_MS {
            let _ = self.off_at.compare_exchange(off_at, 0, Ordering::Relaxed, Ordering::Relaxed);
            return None;
        }
        let peak = block.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        if peak <= RX_BACK_PEAK {
            return None;
        }
        // Only the first block that sees it logs; a TX in between has cleared it.
        if self.off_at.compare_exchange(off_at, 0, Ordering::Relaxed, Ordering::Relaxed).is_err() {
            return None;
        }
        let ms = now_ms.saturating_sub(off_at);
        // Named in the plan before it was built (working rule 11).
        dev_info!("{}PTT metrics: RX audio back {} ms after TRX off (peak {:.4}, backlog {} ms)", self.prefix, ms, peak, backlog_samples / 48);
        Some(ms)
    }
}

impl PttController {
    /// The watch the RX audio loop reports its blocks to.
    pub fn rx_back_watch(&self) -> RxBackWatch {
        self.rx_back.clone()
    }

    /// Create PTT controller (TL2 v2: TCI-only).
    pub fn new_tci(tci_addr: Option<&str>, thetis_path: Option<String>) -> Self {
        let mut ctl = Self {
            state: PttState::Rx,
            last_ptt_packet: None,
            last_heartbeat: None,
            pending_activate: None,
            pending_release: None,
            tail_delay_ms: PTT_TAIL_MIN_MS_TCI,
            ptt_active: Arc::new(AtomicBool::new(false)),
            tci: TciConnection::new(tci_addr),
            thetis_path,
            pending_power_on: false,
            thetis_launch_time: None,
            thetis_shutdown_time: None,
            thetis_relaunch_when_gone: false,
            tx_blocked: Arc::new(AtomicBool::new(false)),
            rx_only_unguarded: Arc::new(AtomicBool::new(false)),
            last_blocked_log: None,
            ptt_prefill_start: None,
            ptt_release_start: None,
            ptt_prefill_latencies: Vec::new(),
            ptt_tail_latencies: Vec::new(),
            rx_back: RxBackWatch::default(),
        };
        // Share the same tx_blocked Arc with the TCI connection, so that the
        // tci-notification handler can also block Thetis-direct PTT.
        // (Without this: server-initiated paths are closed, but the space bar
        // on the Thetis PC is only caught via the 200 ms broadcast catch-all
        // - too late for RX-amp protection.)
        let blocked_clone = ctl.tx_blocked.clone();
        ctl.tci.set_tx_blocked_handle(blocked_clone);
        ctl
    }

    /// Handle to the TX-blocked flag. Used by `network.rs` to
    /// push the current Amplitec status from the broadcast loop.
    /// Lock-free atomic; all TX paths in `PttController`
    /// read it on the hot path.
    pub fn tx_blocked_handle(&self) -> Arc<AtomicBool> {
        self.tx_blocked.clone()
    }

    /// Is the current Amplitec position marked receive-only?
    pub fn tx_blocked(&self) -> bool {
        self.tx_blocked.load(Ordering::Relaxed)
    }

    /// Handle to the "RX-only and unguarded" flag: the RX-only loop writes it,
    /// the server window reads it.
    pub fn rx_only_unguarded_handle(&self) -> Arc<AtomicBool> {
        self.rx_only_unguarded.clone()
    }

    /// What the heartbeat tells the clients about Thetis, by name (build 217):
    /// four booleans in a tuple went by position, and two of them swapped
    /// stayed green (review of 216).
    pub fn thetis_status(&self) -> ThetisStatus {
        ThetisStatus {
            tci_connected: self.tci_connected(),
            thetis_running: self.thetis_process_running(),
            thetis_starting: self.thetis_starting(),
            rx_only_unguarded: self.rx_only_unguarded.load(Ordering::Relaxed),
        }
    }

    fn prefill_ms(&self) -> u64 {
        if true { PTT_PREFILL_MS_TCI } else { PTT_PREFILL_MS }
    }

    fn tail_min_ms(&self) -> u64 {
        if true { PTT_TAIL_MIN_MS_TCI } else { PTT_TAIL_MIN_MS }
    }

    fn tail_margin_ms(&self) -> u64 {
        if true { PTT_TAIL_MARGIN_MS_TCI } else { PTT_TAIL_MARGIN_MS }
    }

    /// How long since the last packet from the client, for
    /// `silence_ended_the_over` (build 248).
    pub fn packet_silence(&self, now: Instant) -> Option<Duration> {
        self.last_ptt_packet.map(|t| now.saturating_duration_since(t))
    }

    pub fn record_ptt_packet(&mut self) {
        let now = Instant::now();
        self.last_ptt_packet = Some(now);
        self.last_heartbeat = Some(now);
    }

    /// The server's own CW playout is active: a played edge, or a key held
    /// down. That keeps the packet watchdog fed - the playout is what stands
    /// in for the audio stream here - and deliberately NOT the heartbeat: a
    /// heartbeat is a sign of life from the client, and the server cannot
    /// give one on its behalf. A client that dies with the key down is
    /// dropped by the heartbeat watchdog after 2 s like any other; build 103's
    /// first draft fed both and would have held the carrier until the CW
    /// watchdog, a second later.
    pub fn record_cw_activity(&mut self) {
        self.last_ptt_packet = Some(Instant::now());
    }

    #[cfg(test)]
    fn watchdog_stamps(&self) -> (Option<Instant>, Option<Instant>) {
        (self.last_ptt_packet, self.last_heartbeat)
    }

    pub fn activate_from_playout(&mut self) {
        // Hard gate: on an RX-only Amplitec position no TX command may
        // go to Thetis under any circumstances.
        // Drop the PTT request entirely (no prefill, no TRX:0,true;)
        // and log throttled (max 1× per 2 sec) so the audio-frame
        // stream (50/sec) does not cause log spam.
        if self.tx_blocked.load(Ordering::Relaxed) {
            let should_log = self
                .last_blocked_log
                .map(|t| t.elapsed() >= Duration::from_secs(2))
                .unwrap_or(true);
            if should_log {
                warn!("PTT request blocked: {}", TX_BLOCKED_WHY);
                self.last_blocked_log = Some(Instant::now());
            }
            return;
        }
        self.last_blocked_log = None;
        if self.pending_release.take().is_some() {
            info!("PTT re-keyed during tail delay, release cancelled");
        }
        if self.state != PttState::Tx && self.pending_activate.is_none() {
            let now = Instant::now();
            info!("PTT prefill started ({}ms)", self.prefill_ms());
            self.pending_activate = Some(now);
            self.ptt_prefill_start = Some(now);
        }
    }

    /// Key MOX for a CW run, through the same machine as the microphone PTT:
    /// the Amplitec gate, the heartbeat and packet time-outs and the release
    /// all apply as they do to audio. Returns whether Thetis is keyed.
    ///
    /// Why the server keys MOX at all: Thetis's own TCI keyer raises MOX on
    /// the first key-down when nothing else has, and drops it again on the
    /// first key-up - at once, without its break-in delay
    /// (`TCIServer.cs`: `ensureDirectKeyerMoxLocked`,
    /// `captureDirectKeyerMoxReleaseLocked`). Every run began with TX, RX, TX
    /// and lost its first elements in the rise (field test of build 96,
    /// 2026-09-18 00:05). With the TCI PTT active Thetis leaves MOX alone.
    ///
    /// What it does to the microphone's state, on purpose (review 313 asked):
    /// a microphone tail release that was pending is cancelled - the CW run
    /// holds the transmitter now, and the run hands it back; a microphone
    /// prefill that was pending is dropped - MOX goes up here at once, so the
    /// microphone's audio finds the transmitter already on and needs no
    /// prefill of its own. `tail_delay_ms` is left alone: the next microphone
    /// release computes its own. Pinned by `activate_for_cw_takes_over_the_microphones_pending_state`.
    /// A microphone that keys during the run rides on the run's MOX, and the
    /// run's idle release drops it: that is fault 4 of the step-4 brief (§3d),
    /// left so on purpose (the owner, 2026-09-18).
    pub async fn activate_for_cw(&mut self) -> bool {
        if self.tx_blocked.load(Ordering::Relaxed) {
            return false;
        }
        if self.pending_release.take().is_some() {
            info!("PTT re-keyed for CW during tail delay, release cancelled");
        }
        self.pending_activate = None;
        self.record_ptt_packet();
        if self.state != PttState::Tx {
            info!("PTT keyed for a CW run");
            self.set_state(PttState::Tx).await;
        }
        self.state == PttState::Tx
    }

    pub async fn check_prefill(&mut self) {
        if let Some(start) = self.pending_activate {
            if start.elapsed().as_millis() >= self.prefill_ms() as u128 {
                self.pending_activate = None;
                self.set_state(PttState::Tx).await;
            }
        }
    }

    pub fn is_tx_or_prefill(&self) -> bool {
        self.state == PttState::Tx || self.pending_activate.is_some()
    }

    pub fn cancel_prefill(&mut self) {
        if self.pending_activate.take().is_some() {
            info!("PTT prefill cancelled (PTT released before prefill completed)");
        }
    }

    /// Start the tail. Returns whether it started one - false while a tail is
    /// already running (every non-PTT frame in the tail calls this) or when not
    /// transmitting - so the caller sets one timer per tail, not one per frame
    /// (round 103).
    pub fn release_from_playout(&mut self, jitter_depth: usize) -> bool {
        if self.state == PttState::Tx && self.pending_release.is_none() {
            let now = Instant::now();
            let depth_ms = (jitter_depth as u64) * 20;
            self.tail_delay_ms = (depth_ms + self.tail_margin_ms()).max(self.tail_min_ms());
            info!("PTT release from playout, {}ms tail delay (jitter depth={})", self.tail_delay_ms, jitter_depth);
            self.pending_release = Some(now);
            self.ptt_release_start = Some(now);
            return true;
        }
        false
    }

    /// How long the tail waits after a release from playout - for the caller that
    /// sets a timer on it (build 103).
    pub fn tail_delay(&self) -> Duration {
        Duration::from_millis(self.tail_delay_ms)
    }

    /// End a pending tail if its time has come. Called by the timer that
    /// `release_from_playout`'s caller sets, so TX ends when the tail is over, and
    /// by `check_safety` as the net under it. Until build 103 only the 100 ms
    /// safety tick did this, and the tail ended 0-100 ms late (measured 60-140 ms
    /// for a 50 ms tail, tenth measurement). Harmless when called early, twice, or
    /// after a new TX: it acts only on a pending release that is due.
    pub async fn finish_tail_if_due(&mut self) {
        if self.tail_due(Instant::now()) {
            self.pending_release = None;
            self.set_state(PttState::Rx).await;
        }
    }

    /// Whether the pending tail is over at `now` - and only while transmitting: a
    /// tail left behind by another release must never send TRX off to a Thetis
    /// that transmits for another reason (front panel, another TCI client; round
    /// 103). One condition, for the timer and the tick alike.
    fn tail_due(&self, now: Instant) -> bool {
        self.state == PttState::Tx
            && self.pending_release
                .is_some_and(|t| now.duration_since(t).as_millis() >= self.tail_delay_ms as u128)
    }

    pub fn heartbeat_received(&mut self) {
        self.last_heartbeat = Some(Instant::now());
    }

    pub async fn check_safety(&mut self) -> bool {
        let now = Instant::now();

        let since_heartbeat = self.last_heartbeat.map(|t| now.duration_since(t));
        let since_packet = self.last_ptt_packet.map(|t| now.duration_since(t));
        let clocks = SafetyClocks {
            state: self.state,
            since_heartbeat,
            since_packet,
            heartbeat_timeout: Duration::from_millis(HEARTBEAT_TIMEOUT_MS),
            packet_timeout: Duration::from_millis(PTT_PACKET_TIMEOUT_MS),
        };
        match clocks.action() {
            SafetyAction::ReleaseHeartbeatLost => {
                // The measured silence, not the threshold. The threshold printing
                // itself proves nothing: four of these lines in one session never
                // said whether the gap had been 0.6s or thirty, and that is the
                // number that decides whether the window is wide enough.
                error!(
                    "SAFETY: Heartbeat timeout after {}! Releasing PTT.",
                    describe_silence(since_heartbeat)
                );
                self.pending_release = None;
                self.force_release().await;
                return true;
            }
            SafetyAction::ReleaseNoPackets => {
                warn!(
                    "SAFETY: No PTT packets for {} (window {PTT_PACKET_TIMEOUT_MS}ms), \
                     releasing PTT.",
                    describe_silence(since_packet)
                );
                self.pending_release = None;
                self.force_release().await;
                return true;
            }
            SafetyAction::None => {}
        }

        // The net under the tail timer (build 103): normally the timer has ended
        // the tail already. One place ends a tail (round 103).
        self.finish_tail_if_due().await;

        // Poll radio backend
        self.tci.poll_and_parse().await;

        // Auto-launch: if pending_power_on and backend connected
        if self.pending_power_on && self.is_connected() {
            info!("Thetis connected, sending power on");
            self.radio_set_power(true).await;
            self.pending_power_on = false;
            self.thetis_launch_time = None;
            // And the latch, which used to survive this. If Thetis is talking
            // to us, whatever we were waiting for is over - however it got
            // there, including the user starting it by hand. Left armed it
            // fired the next time Thetis was closed on purpose, relaunching it
            // with "launching it now as asked" while nobody had asked. The
            // timeout that would have cleared it begins with pending_power_on,
            // which this line has just made false, so it could never run
            // again (found in review of report 67 - the second pass of the very
            // fault this code was written for).
            self.thetis_relaunch_when_gone = false;
            self.thetis_shutdown_time = None;
        }

        // A power-on that arrived while Thetis was still closing: start it the
        // moment the process is really gone. Nothing else could do this -
        // waiting for it to connect is waiting for a program that is leaving.
        if self.thetis_relaunch_when_gone && !is_process_running("Thetis.exe") {
            self.thetis_relaunch_when_gone = false;
            self.thetis_shutdown_time = None;
            if let Some(path) = self.thetis_path.clone() {
                info!("Thetis has closed; launching it now as asked");
                if shell_execute_open(&path) {
                    info!("Thetis.exe launch initiated");
                } else {
                    error!("Failed to start Thetis via ShellExecute");
                    self.pending_power_on = false;
                    self.thetis_launch_time = None;
                }
            }
        }

        if self.pending_power_on {
            if let Some(launch_time) = self.thetis_launch_time {
                if now.duration_since(launch_time).as_secs() > THETIS_LAUNCH_TIMEOUT_S {
                    // Say which of the two waits gave up. A bare "cancelling"
                    // sent the reporter of #67 looking at the wrong thing.
                    if self.thetis_relaunch_when_gone {
                        warn!(
                            "Thetis was asked to close {}s ago and is still running - giving up on the power-on",
                            THETIS_LAUNCH_TIMEOUT_S
                        );
                    } else {
                        warn!("Thetis launch timeout ({}s), cancelling", THETIS_LAUNCH_TIMEOUT_S);
                    }
                    self.pending_power_on = false;
                    self.thetis_launch_time = None;
                    self.thetis_relaunch_when_gone = false;
                }
            }
        }

        // A shutdown we asked for ends when the process is gone - not when TCI
        // is up, which it still is for a moment after shutdown_ex; and which is
        // what made build 15 forget within one tick. The process scan only runs
        // while a shutdown is actually outstanding, which is rare.
        if let Some(asked) = self.thetis_shutdown_time {
            let waited = now.duration_since(asked).as_secs();
            if forget_shutdown(
                is_process_running("Thetis.exe"),
                waited,
                THETIS_LAUNCH_TIMEOUT_S,
            ) {
                if waited > THETIS_LAUNCH_TIMEOUT_S {
                    warn!(
                        "Thetis was asked to close {}s ago and is still running - no longer treating it as closing",
                        waited
                    );
                }
                self.thetis_shutdown_time = None;
            }
        }

        false
    }

    async fn force_release(&mut self) {
        self.pending_activate = None;
        self.set_state(PttState::Rx).await;
    }

    pub async fn release(&mut self) {
        self.pending_activate = None;
        self.pending_release = None;
        if self.state == PttState::Tx {
            self.set_state(PttState::Rx).await;
        }
    }

    async fn set_state(&mut self, new_state: PttState) {
        // Hard gate: last defensive check before TRX:0,true,tci goes
        // out over the tci-link. `activate_from_playout` already blocks
        // preventively, but a future direct-to-Tx caller passes
        // through here too. When blocked: stay on Rx and send nothing.
        if new_state == PttState::Tx && self.tx_blocked.load(Ordering::Relaxed) {
            warn!("set_state(Tx) blocked: {}", TX_BLOCKED_WHY);
            // Make sure we are Rx (was probably already, since
            // activate_from_playout would have stopped us).
            if self.state == PttState::Tx {
                self.state = PttState::Rx;
                self.ptt_active.store(false, Ordering::Relaxed);
                self.tci.set_tx_active(false);
                self.tci.send("TRX:0,false;").await;
            }
            return;
        }
        let old = self.state;
        self.state = new_state;
        let is_tx = new_state == PttState::Tx;
        self.ptt_active.store(is_tx, Ordering::Relaxed);

        // Latency metrics
        // Measures server-side PTT budget: prefill->TRX-send (our latency contribution).
        // Gap between TRX-send and first TX audio frame is Thetis-side (TX_CHRONO timing)
        // and not measurable without cross-task synchronization overhead in the audio hot path.
        if is_tx {
            if let Some(start) = self.ptt_prefill_start.take() {
                let ms = start.elapsed().as_millis() as u64;
                dev_info!("PTT metrics: prefill->TRX = {}ms (server-side budget)", ms);
                self.ptt_prefill_latencies.push(ms);
            }
        } else if let Some(start) = self.ptt_release_start.take() {
            let ms = start.elapsed().as_millis() as u64;
            dev_info!("PTT metrics: tail latency = {}ms", ms);
            self.ptt_tail_latencies.push(ms);
            // Summary every 10 cycles
            if self.ptt_tail_latencies.len() >= 10 {
                let mut pf = self.ptt_prefill_latencies.clone();
                let mut tl = self.ptt_tail_latencies.clone();
                pf.sort();
                tl.sort();
                let percentile = |v: &[u64], p: usize| -> u64 {
                    if v.is_empty() { return 0; }
                    let idx = (v.len() * p / 100).min(v.len() - 1);
                    v[idx]
                };
                dev_info!("PTT metrics ({} cycles): prefill p50={}ms p95={}ms max={}ms, tail p50={}ms p95={}ms max={}ms",
                    tl.len(),
                    percentile(&pf, 50), percentile(&pf, 95), pf.last().copied().unwrap_or(0),
                    percentile(&tl, 50), percentile(&tl, 95), tl.last().copied().unwrap_or(0));
                self.ptt_prefill_latencies.clear();
                self.ptt_tail_latencies.clear();
            }
        }

        self.tci.set_tx_active(is_tx);
        // TCI: use ,tci source so Thetis takes audio from TCI stream
        let cmd = if is_tx { "TRX:0,true,tci;" } else { "TRX:0,false;" };
        info!("PTT: {:?} -> {:?} (TCI: {})", old, new_state, cmd);
        self.tci.send(cmd).await;
        // After the command has gone, so the turnaround starts at "TRX off sent"
        // and not at the server's decision (round 99).
        if is_tx {
            self.rx_back.trx_on();
        } else if old == PttState::Tx {
            self.rx_back.trx_off();
        }
    }

    fn is_connected(&self) -> bool {
        self.tci.is_connected()
    }

    /// Public accessor for HeartbeatAck `TCI_CONNECTED` flag.
    /// (PATCH-1 client-connect-error-feedback)
    pub fn tci_connected(&self) -> bool {
        self.tci.is_connected()
    }

    /// TCI connections made so far, for the DX spots (build 220).
    pub fn tci_connection_count(&self) -> u64 {
        self.tci.connection_count()
    }

    /// What the RX-only loop reads of the transmitter, by name (build 226,
    /// review of 224: by position, two of the three swapped made the stop of a
    /// TX Thetis was making silently never come).
    pub fn tx_now(&self) -> TxNow {
        TxNow {
            mode: self.vfo_a_mode(),
            ours: self.is_transmitting(),
            thetis: self.tci.thetis_on_the_air(),
        }
    }

    /// What the RX-only loop reads of the TCI link, by name (build 226, review
    /// of 224: by position, "unguarded" never came for a Thetis older than
    /// v2.10.3.14).
    pub fn tci_link_now(&self) -> TciLinkNow {
        TciLinkNow {
            up: self.tci_connected(),
            answers_run_cat_ex: self.answers_run_cat_ex(),
            connections: self.tci_connection_count(),
        }
    }

    /// Thetis reports it is transmitting, whoever keyed it.
    /// Thetis answered a `run_cat_ex` in this TCI connection (build 221).
    pub fn answers_run_cat_ex(&self) -> bool {
        self.tci.answers_run_cat_ex
    }

    /// Our own TUNE is on (build 221, for the log line).
    pub fn tune_is_ours(&self) -> bool {
        self.tci.tune_is_ours()
    }

    /// "Receive only" in stock Thetis through CAT over TCI (build 221).
    pub async fn set_rx_only_cat(&mut self, step: StockRxOnly) {
        self.tci.run_cat(step.zzti()).await
    }

    /// The receiver Thetis last said it transmits on (build 220).
    pub fn thetis_tx_receiver(&self) -> u32 {
        self.tci.thetis_tx_receiver
    }

    /// See `TciConnection::force_thetis_tx_off` (build 220).
    pub async fn force_thetis_tx_off(&mut self) {
        self.tci.force_thetis_tx_off().await
    }

    async fn radio_send(&mut self, cmd: &str) {
        self.tci.send(cmd).await
    }

    async fn radio_set_power(&mut self, on: bool) {
        self.tci.set_power(on).await
    }

    // --- Delegated accessors ---

    pub async fn send_cat(&mut self, cmd: &str) {
        // Hard gate for TX-starting CAT commands when the current
        // Amplitec-A position is marked as RX-only. ZZTX1 keys
        // the transmitter directly, ZZTU1 starts the tune carrier - neither
        // goes to Thetis. ZZTX0/ZZTU0 (switch off) of course keep
        // working so we can end an ongoing TX.
        let trimmed = cmd.trim_end_matches(';');
        if (trimmed.eq_ignore_ascii_case("ZZTX1") || trimmed.eq_ignore_ascii_case("ZZTU1"))
            && self.tx_blocked.load(Ordering::Relaxed)
        {
            warn!("CAT {} blocked: {}", trimmed, TX_BLOCKED_WHY);
            return;
        }
        // ZZ* commands have three levels of handling:
        //   1. Native TCI mapping via `cat_to_tci` (fastest path for the
        //      most-used ZZ commands such as ZZFA/ZZMD/ZZTX).
        //   2. TCI `run_cat_ex:ZZxxx;` passthrough - Thetis runs the ZZ
        //      command on its own internal CAT parser without us
        //      needing a separate CAT connection. Works for any
        //      ZZ command Thetis knows (ZZPC, ZZCN, ZZCO, ZZFI, ...).
        //   3. Formerly also: auxiliary CAT TCP socket. As of v2.0.0 that
        //      path has been removed (TCI-only architecture), so level 3 is
        //      a no-op.
        //
        // Previously level 2 was used only directly from specific call
        // sites (CTUN-recenter, filter-preset polls), and here
        // unknown ZZ commands were dropped with a WARN. That
        // made features using ZZ commands without a `cat_to_tci` mapping
        // silently no-op. Now unknown ZZ commands fall
        // back on `tci.run_cat()` so that all ZZ commands find their path via the same
        // single-TCI link.
        if cmd.starts_with("ZZ") {
            if let Some(tci_cmd) = Self::cat_to_tci(cmd) {
                log::debug!("CAT->TCI: {} -> {}", cmd.trim_end_matches(';'), tci_cmd.trim_end_matches(';'));
                self.radio_send(&tci_cmd).await;
            } else {
                // Fallback: TCI run_cat_ex passthrough. Asks Thetis to
                // run the ZZ command on its own CAT parser.
                let zz = cmd.trim_end_matches(';');
                log::debug!("CAT->TCI run_cat_ex: {}", zz);
                self.tci.run_cat(zz).await;
            }
        } else {
            // TCI command (e.g. TUNE:0,true;) -> directly via WebSocket
            self.radio_send(cmd).await;
        }
    }

    /// Translate a ZZ CAT command to a TCI equivalent. Returns None if unknown.
    fn cat_to_tci(cmd: &str) -> Option<String> {
        let cmd = cmd.trim_end_matches(';');
        // ZZFA00007073000 -> vfo:0,0,7073000 (VFO A freq, 11 digits)
        if cmd.starts_with("ZZFA") && cmd.len() >= 15 {
            if let Ok(hz) = cmd[4..15].parse::<u64>() {
                return Some(format!("vfo:0,0,{};", hz));
            }
        }
        // ZZFB00007073000 -> vfo:1,0,7073000 (VFO B freq)
        if cmd.starts_with("ZZFB") && cmd.len() >= 15 {
            if let Ok(hz) = cmd[4..15].parse::<u64>() {
                return Some(format!("vfo:1,0,{};", hz));
            }
        }
        // ZZMD00 -> modulation:0,LSB (mode VFO A, 2 digit mode number)
        if cmd.starts_with("ZZMD") && cmd.len() >= 6 {
            if let Ok(mode_num) = cmd[4..6].parse::<u8>() {
                let mode_name = cat_mode_to_tci(mode_num);
                return Some(format!("modulation:0,{};", mode_name));
            }
        }
        // ZZME00 -> modulation:1,LSB (mode VFO B)
        if cmd.starts_with("ZZME") && cmd.len() >= 6 {
            if let Ok(mode_num) = cmd[4..6].parse::<u8>() {
                let mode_name = cat_mode_to_tci(mode_num);
                return Some(format!("modulation:1,{};", mode_name));
            }
        }
        // ZZTU1/ZZTU0 -> tune:0,true/false
        if cmd == "ZZTU1" { return Some("tune:0,true;".to_string()); }
        if cmd == "ZZTU0" { return Some("tune:0,false;".to_string()); }
        // ZZTX1/ZZTX0 -> trx:0,true/false
        if cmd == "ZZTX1" { return Some("trx:0,true,tci;".to_string()); }
        if cmd == "ZZTX0" { return Some("trx:0,false;".to_string()); }
        None
    }

}

/// Convert Thetis CAT mode number to TCI modulation name
fn cat_mode_to_tci(mode: u8) -> &'static str {
    match mode {
        0 => "lsb", 1 => "usb", 2 => "dsb", 3 => "cwl", 4 => "cwu",
        5 => "fm", 6 => "am", 7 => "digu", 8 => "spec", 9 => "digl",
        10 => "sam", 11 => "drm",
        _ => "usb",
    }
}

impl PttController {
    /// Send a TCI SPOT command to Thetis. Only works in TCI mode.
    pub async fn send_tci_spot(&mut self, callsign: &str, mode: &str, freq_hz: u64, color: u32, text: &str) {
        if let Some(tci) = Some(&mut self.tci) {
            tci.send_spot(callsign, mode, freq_hz, color, text).await;
        }
    }

    pub fn vfo_a_freq(&self) -> u64 {
        self.tci.vfo_a_freq
    }

    pub fn vfo_a_mode(&self) -> u8 {
        self.tci.vfo_a_mode
    }

    pub fn smeter_avg(&self) -> f32 {
        self.tci.smeter_avg()
    }

    pub fn smeter_sig(&self) -> f32 {
        self.tci.smeter_sig()
    }

    pub fn smeter_peakbin(&self) -> f32 {
        self.tci.smeter_peakbin()
    }

    pub fn power_on(&self) -> bool {
        self.tci.power_on
    }

    pub fn tx_profile(&self) -> u8 {
        self.tci.tx_profile
    }

    pub fn nr_level(&self) -> u8 {
        self.tci.nr_level
    }

    pub fn anf_on(&self) -> bool {
        self.tci.anf_on
    }

    pub fn drive_level(&self) -> u8 {
        self.tci.drive_level
    }

    pub fn rx_af_gain(&self) -> u8 {
        self.tci.rx_af_gain
    }

    pub fn filter_low_hz(&self) -> i32 {
        self.tci.filter_low_hz
    }

    pub fn filter_high_hz(&self) -> i32 {
        self.tci.filter_high_hz
    }

    // TX modulation filter (PATCH-tx-modulation-bandwidth)
    pub fn tx_filter_low_hz(&self) -> i32 {
        self.tci.tx_filter_low_hz
    }

    pub fn tx_filter_high_hz(&self) -> i32 {
        self.tci.tx_filter_high_hz
    }

    pub fn tx_filter_known(&self) -> bool {
        self.tci.tx_filter_known
    }

    pub fn ctun(&self) -> bool {
        self.tci.ctun
    }

    pub fn is_transmitting(&self) -> bool {
        self.state == PttState::Tx
    }

    pub fn fwd_power_raw(&self) -> u16 {
        self.tci.fwd_power_raw()
    }

    /// SWR × 100 (e.g. 150 = 1.50:1). Returns 100 when not transmitting.
    pub fn swr_x100(&self) -> u16 {
        (self.tci.swr * 100.0).round() as u16
    }

    pub async fn set_vfo_a_freq(&mut self, hz: u64) {
        self.tci.set_vfo_a_freq(hz).await
    }

    pub async fn set_vfo_a_mode(&mut self, mode: u8) {
        self.tci.set_vfo_a_mode(mode).await
    }

    pub async fn set_power(&mut self, on: bool) {
        info!("set_power({}) called - connected={}, thetis_path={:?}", on, self.is_connected(), self.thetis_path.is_some());
        if !on {
            self.pending_power_on = false;
            self.thetis_launch_time = None;
            self.thetis_relaunch_when_gone = false;
            if self.is_connected() {
                self.radio_set_power(false).await;
            }
            return;
        }

        // The decision itself lives in sdr-remote-logic with its rows written
        // out, because it grew an input that nobody could see it needed until
        // a tester walked into it.
        let step = power_on_step(PowerOnInputs {
            connected: self.is_connected(),
            have_path: self.thetis_path.is_some(),
            launch_pending: self.pending_power_on,
            process_running: is_process_running("Thetis.exe"),
            shutdown_pending: self.thetis_shutdown_time.is_some(),
        });
        info!("power-on: {:?}", step);

        match step {
            PowerOnStep::SendPowerOn => {
                self.radio_set_power(true).await;
                return;
            }
            PowerOnStep::Ignore => {
                info!("Thetis launch already pending, ignoring duplicate POWER ON");
                return;
            }
            PowerOnStep::Launch => {
                let path = self.thetis_path.as_ref().unwrap();
                info!("Launching Thetis: {}", path);
                if shell_execute_open(path) {
                    info!("Thetis.exe launch initiated");
                } else {
                    error!("Failed to start Thetis via ShellExecute");
                    return;
                }
            }
            PowerOnStep::WaitForConnection => {
                info!("Thetis already running, waiting for connection");
            }
            PowerOnStep::WaitForExitThenLaunch => {
                info!("Thetis is still closing; it will be launched as soon as it is gone");
                self.thetis_relaunch_when_gone = true;
            }
        }

        self.pending_power_on = true;
        self.thetis_launch_time = Some(Instant::now());
    }

    /// Ask Thetis to close, and remember that we did.
    ///
    /// The remembering is the point. Without it the next power-on sees a
    /// process that is on its way out and mistakes it for one that is running.
    pub async fn request_thetis_shutdown(&mut self) {
        self.thetis_shutdown_time = Some(Instant::now());
        self.thetis_relaunch_when_gone = false;
        self.send_cat("shutdown_ex;").await;
    }

    pub fn thetis_starting(&self) -> bool {
        self.pending_power_on
    }

    /// Cheap process-table scan: is Thetis.exe currently running on this PC?
    /// Used by the heartbeat-ack handler to broadcast THETIS_RUNNING so the
    /// client can give a smarter "TCI unreachable" hint.
    pub fn thetis_process_running(&self) -> bool {
        is_process_running("Thetis.exe")
    }

    pub async fn set_tx_profile(&mut self, idx: u8) {
        self.tci.set_tx_profile(idx).await
    }

    pub async fn set_nr(&mut self, level: u8) {
        self.tci.set_nr(level).await
    }

    pub async fn set_anf(&mut self, on: bool) {
        self.tci.set_anf(on).await
    }

    pub async fn set_drive(&mut self, level: u8) {
        self.tci.set_drive(level).await
    }

    pub async fn set_filter(&mut self, low_hz: i32, high_hz: i32) {
        self.tci.set_filter(low_hz, high_hz).await
    }

    /// Set the TX modulation filter band (tx_filter_band_ex). Main-radio TX.
    pub async fn set_tx_filter_band(&mut self, low_hz: i32, high_hz: i32) {
        self.tci.set_tx_filter_band(low_hz, high_hz).await
    }

    // --- RX2 / VFO-B ---

    pub fn vfo_b_freq(&self) -> u64 {
        self.tci.vfo_b_freq
    }

    pub fn vfo_b_mode(&self) -> u8 {
        self.tci.vfo_b_mode
    }

    pub fn smeter_rx2_avg(&self) -> f32 {
        self.tci.smeter_rx2_avg()
    }

    pub fn smeter_rx2_sig(&self) -> f32 {
        self.tci.smeter_rx2_sig()
    }

    pub fn smeter_rx2_peakbin(&self) -> f32 {
        self.tci.smeter_rx2_peakbin()
    }

    pub fn rx2_af_gain(&self) -> u8 {
        self.tci.rx2_af_gain
    }

    pub fn filter_rx2_low_hz(&self) -> i32 {
        self.tci.filter_rx2_low_hz
    }

    pub fn filter_rx2_high_hz(&self) -> i32 {
        self.tci.filter_rx2_high_hz
    }

    pub fn rx2_nr_level(&self) -> u8 {
        self.tci.rx2_nr_level
    }

    pub fn rx2_anf_on(&self) -> bool {
        self.tci.rx2_anf_on
    }

    pub fn tx_profile_names(&self) -> &[String] {
        &self.tci.tx_profile_names
    }

    pub fn tx_profile_name(&self) -> &str {
        &self.tci.tx_profile_name
    }

    pub fn mon_on(&self) -> bool {
        self.tci.mon_on
    }

    // New TCI state getters (v2.10.3.13) - TCI-only, return defaults for CAT
    pub fn agc_mode(&self) -> u8 {
        self.tci.agc_mode
    }
    pub fn agc_gain(&self) -> u8 {
        self.tci.agc_gain
    }
    pub fn rit_enable(&self) -> bool {
        self.tci.rit_enable
    }
    pub fn rit_offset(&self) -> i32 {
        self.tci.rit_offset
    }
    pub fn xit_enable(&self) -> bool {
        self.tci.xit_enable
    }
    pub fn xit_offset(&self) -> i32 {
        self.tci.xit_offset
    }
    pub fn sql_enable(&self) -> bool {
        self.tci.sql_enable
    }
    pub fn sql_level(&self) -> u8 {
        self.tci.sql_level
    }
    pub async fn diversity_smartnull(&mut self, params: &[f32]) {
        if let Some(t) = Some(&mut self.tci) {
            if !t.has_cap("diversity_sweep_ex") {
                info!("Diversity smartnull skipped (diversity_sweep_ex cap not available)");
                return;
            }
            t.diversity_auto_done = None;
            let args: Vec<String> = params.iter().map(|v| format!("{:.2}", v)).collect();
            let cmd = format!("diversity_smartnull_ex:{};", args.join(","));
            t.send(&cmd).await;
        }
    }
    pub async fn diversity_ultranull(&mut self, params: &[f32]) {
        if let Some(t) = Some(&mut self.tci) {
            if !t.has_cap("diversity_sweep_ex") {
                info!("Diversity ultranull skipped (diversity_sweep_ex cap not available)");
                return;
            }
            t.diversity_auto_done = None;
            let args: Vec<String> = params.iter().map(|v| format!("{:.2}", v)).collect();
            let cmd = format!("diversity_ultranull_ex:{};", args.join(","));
            t.send(&cmd).await;
        }
    }
    pub async fn diversity_fastsweep(&mut self, start: f32, end: f32, step: f32, settle_ms: u32, meter: u32) {
        if let Some(t) = Some(&mut self.tci) {
            if !t.has_cap("diversity_sweep_ex") {
                info!("Diversity fastsweep skipped (diversity_sweep_ex cap not available)");
                return;
            }
            let cmd = format!("diversity_fastsweep_ex:phase,{:.2},{:.2},{:.2},{},{};", start, end, step, settle_ms, meter);
            t.send(&cmd).await;
        }
    }
    pub async fn diversity_autonull(&mut self, settle_ms: u32, steps: &[(Vec<f32>, bool)]) {
        if let Some(t) = Some(&mut self.tci) {
            t.diversity_auto_done = None; // clear previous result
            t.diversity_autonull(settle_ms, steps).await;
        }
    }
    /// Returns improvement × 10 + 32000 as u16 when done, 0 when not done.
    pub fn diversity_autonull_done(&self) -> u16 {
        if let Some((_, _, improvement)) = self.tci.diversity_auto_done {
            ((improvement * 10.0).clamp(-320.0, 320.0) as i16 as u16).wrapping_add(32000)
        } else { 0 }
    }
    pub fn diversity_phase(&self) -> i32 {
        self.tci.diversity_phase
    }
    pub fn diversity_gain(&self, rx: usize) -> u16 {
        if rx == 0 { self.tci.diversity_gain_rx1 } else { self.tci.diversity_gain_rx2 }
    }
    pub fn diversity_gain_multi(&self) -> u16 {
        self.tci.diversity_gain_multi
    }
    pub fn diversity_enabled(&self) -> bool {
        self.tci.diversity_enabled
    }
    pub fn agc_auto(&self, rx: usize) -> bool {
        if rx == 0 { self.tci.agc_auto_rx1 } else { self.tci.agc_auto_rx2 }
    }
    pub async fn set_agc_auto(&mut self, rx: u32, enabled: bool) {
        // Stock .14/.15 supports agc_auto_ex without advertising the cap.
        let t = &mut self.tci;
        let cmd = format!("agc_auto_ex:{},{};", rx, enabled);
        t.send(&cmd).await;
        if rx == 0 { t.agc_auto_rx1 = enabled; }
        else { t.agc_auto_rx2 = enabled; }
    }
    pub fn nb_level(&self) -> u8 {
        self.tci.nb_level
    }
    pub fn cw_keyer_speed(&self) -> u8 {
        self.tci.cw_keyer_speed
    }
    pub fn vfo_lock(&self) -> bool {
        self.tci.vfo_lock
    }
    pub fn binaural(&self) -> bool {
        self.tci.binaural
    }
    pub fn apf_enable(&self) -> bool {
        self.tci.apf_enable
    }

    // RX2 TCI state getters
    pub fn rx2_agc_mode(&self) -> u8 {
        self.tci.rx2_agc_mode
    }
    pub fn rx2_agc_gain(&self) -> u8 {
        self.tci.rx2_agc_gain
    }
    pub fn rx2_sql_enable(&self) -> bool {
        self.tci.rx2_sql_enable
    }
    pub fn rx2_sql_level(&self) -> u8 {
        self.tci.rx2_sql_level
    }
    pub fn rx2_nb_enable(&self) -> bool {
        self.tci.rx2_nb_enable
    }
    pub fn rx2_binaural(&self) -> bool {
        self.tci.rx2_binaural
    }
    pub fn rx2_apf_enable(&self) -> bool {
        self.tci.rx2_apf_enable
    }
    pub fn rx2_vfo_lock(&self) -> bool {
        self.tci.rx2_vfo_lock
    }
    pub fn mute(&self) -> bool {
        self.tci.mute
    }
    pub fn rx_mute(&self) -> bool {
        self.tci.rx_mute
    }
    pub fn nf_enable(&self) -> bool {
        self.tci.nf_enable
    }
    pub fn rx2_nf_enable(&self) -> bool {
        self.tci.rx2_nf_enable
    }
    pub fn rx_balance(&self) -> i8 {
        self.tci.rx_balance
    }

    pub fn vfo_sync_on(&self) -> bool {
        self.tci.vfo_sync_on
    }

    pub async fn set_mon(&mut self, on: bool) {
        self.tci.set_mon(on).await
    }

    pub async fn set_vfo_sync_thetis(&mut self, on: bool) {
        // Stock .14/.15 supports vfo_sync_ex without advertising the cap (cap-check is
        // a TL-26 legacy). Use the optimistic _ex setter directly - operator choice 1a:
        // "Plus align the vfo_sync_ex _ex path". Compat statement (alpha-4 ≥ Thetis
        // v2.10.3.14) makes the old run_cat(ZZSY) fallback unnecessary.
        self.tci.set_vfo_sync(on).await;
    }

    // ── Diversity dispatch (TCI _ex with CAT fallback) ────────────────

    pub async fn set_diversity_enable(&mut self, enabled: bool) {
        if let Some(tci) = Some(&mut self.tci) {
            if tci.has_cap("diversity_enable_ex") {
                tci.set_diversity_enable(enabled).await;
                return;
            }
        }
        let cmd = format!("ZZDE{};", if enabled { 1 } else { 0 });
        self.send_cat(&cmd).await;
    }

    /// Push Thetis' "Receive only" preventive transmit-inhibit via the fork's
    /// `rx_only_ex` command. Returns `true` if the fork accepted it (extensions
    /// present) - meaning TX is now preventively blocked at the Thetis source.
    /// Returns `false` when running against stock Thetis (no cap): the caller
    /// must keep using the reactive `ZZTX0` catch-all instead.
    pub async fn set_rx_only(&mut self, rx_only: bool) -> bool {
        self.tci.set_rx_only(rx_only).await
    }

    /// Last-known Thetis "Receive only" state (from the `rx_only_ex` echo).
    /// Used by the broadcast loop to snapshot the pre-takeover value before
    /// forcing RX-only on an RX-only Amplitec position.
    pub fn thetis_rx_only(&self) -> bool {
        self.tci.rx_only
    }

    pub async fn set_diversity_ref(&mut self, rx1_ref: bool) {
        if let Some(tci) = Some(&mut self.tci) {
            if tci.has_cap("diversity_ref_ex") {
                tci.set_diversity_ref(rx1_ref).await;
                return;
            }
        }
        let cmd = format!("ZZDB{};", if rx1_ref { 0 } else { 1 });
        self.send_cat(&cmd).await;
    }

    pub async fn set_diversity_source(&mut self, source: u32) {
        if let Some(tci) = Some(&mut self.tci) {
            if tci.has_cap("diversity_source_ex") {
                tci.set_diversity_source(source).await;
                return;
            }
        }
        let cmd = format!("ZZDH{};", source);
        self.send_cat(&cmd).await;
    }

    pub async fn set_diversity_gain(&mut self, rx: u32, gain: u16) {
        if let Some(tci) = Some(&mut self.tci) {
            if tci.has_cap("diversity_gain_ex") {
                tci.set_diversity_gain(rx, gain).await;
                return;
            }
        }
        let cmd = if rx == 0 {
            format!("ZZDG{:04};", gain.min(9999))
        } else {
            format!("ZZDC{:04};", gain.min(9999))
        };
        self.send_cat(&cmd).await;
    }

    pub async fn set_diversity_phase(&mut self, phase: i32) {
        if let Some(tci) = Some(&mut self.tci) {
            if tci.has_cap("diversity_phase_ex") {
                tci.set_diversity_phase(phase).await;
                return;
            }
        }
        let sign = if phase >= 0 { "+" } else { "-" };
        let cmd = format!("ZZDD{}{:05};", sign, phase.abs());
        self.send_cat(&cmd).await;
    }

    /// TL2-1 fork-only - no CAT fallback (stock Thetis has no ZZ-CAT for GainMulti).
    pub async fn set_diversity_gain_multi(&mut self, multi: u16) {
        if self.tci.has_cap("diversity_gain_multi_ex") {
            self.tci.set_diversity_gain_multi(multi).await;
        }
    }

    /// TL2-1 fork-only - DDC per-RX sample-rate change. No CAT fallback (stock
    /// Thetis has no ZZ-CAT for the per-RX sample rate; only Setup-form UI).
    pub async fn set_ddc_sample_rate(&mut self, rx: u32, rate_hz: u32) {
        if self.tci.has_cap("ddc_sample_rate_ex") {
            self.tci.set_ddc_sample_rate(rx, rate_hz).await;
        }
    }

    pub async fn set_vfo_b_freq(&mut self, hz: u64) {
        self.tci.set_vfo_b_freq(hz).await
    }

    pub async fn set_vfo_b_mode(&mut self, mode: u8) {
        self.tci.set_vfo_b_mode(mode).await
    }

    pub async fn vfo_swap(&mut self) {
        self.tci.vfo_swap().await
    }

    pub async fn set_rx2_af_gain(&mut self, level: u8) {
        self.tci.set_rx2_af_gain(level).await
    }

    pub async fn set_rx2_nr(&mut self, level: u8) {
        self.tci.set_rx2_nr(level).await
    }

    pub async fn set_rx2_anf(&mut self, on: bool) {
        self.tci.set_rx2_anf(on).await
    }

    pub async fn set_rx2_filter(&mut self, low_hz: i32, high_hz: i32) {
        self.tci.set_rx2_filter(low_hz, high_hz).await
    }

    // --- New TCI controls (v2.10.3.13) ---

    pub async fn set_agc_mode(&mut self, mode: u8) {
        if let Some(t) = Some(&mut self.tci) { t.set_agc_mode(mode).await; }
    }
    pub async fn set_agc_gain(&mut self, gain: u8) {
        if let Some(t) = Some(&mut self.tci) { t.set_agc_gain(gain).await; }
    }
    pub async fn set_rit_enable(&mut self, on: bool) {
        if let Some(t) = Some(&mut self.tci) { t.set_rit_enable(on).await; }
    }
    pub async fn set_rit_offset(&mut self, hz: i32) {
        if let Some(t) = Some(&mut self.tci) { t.set_rit_offset(hz).await; }
    }
    pub async fn set_xit_enable(&mut self, on: bool) {
        if let Some(t) = Some(&mut self.tci) { t.set_xit_enable(on).await; }
    }
    pub async fn set_xit_offset(&mut self, hz: i32) {
        if let Some(t) = Some(&mut self.tci) { t.set_xit_offset(hz).await; }
    }
    pub async fn set_sql_enable(&mut self, on: bool) {
        if let Some(t) = Some(&mut self.tci) { t.set_sql_enable(on).await; }
    }
    pub async fn set_sql_level(&mut self, level: i16) {
        if let Some(t) = Some(&mut self.tci) { t.set_sql_level(level).await; }
    }
    pub async fn set_nb(&mut self, level: u8) {
        if let Some(t) = Some(&mut self.tci) { t.set_nb(level).await; }
    }
    pub async fn set_cw_keyer_speed(&mut self, wpm: u8) {
        if let Some(t) = Some(&mut self.tci) { t.set_cw_keyer_speed(wpm).await; }
    }
    pub async fn cw_key(&mut self, pressed: bool, duration_ms: Option<u16>) {
        if let Some(t) = Some(&mut self.tci) { t.cw_key(pressed, duration_ms).await; }
    }
    pub async fn cw_macro_stop(&mut self) {
        if let Some(t) = Some(&mut self.tci) { t.cw_macro_stop().await; }
    }
    pub async fn set_vfo_lock(&mut self, on: bool) {
        if let Some(t) = Some(&mut self.tci) { t.set_vfo_lock(on).await; }
    }
    pub async fn set_binaural(&mut self, on: bool) {
        if let Some(t) = Some(&mut self.tci) { t.set_binaural(on).await; }
    }
    pub async fn set_apf_enable(&mut self, on: bool) {
        if let Some(t) = Some(&mut self.tci) { t.set_apf_enable(on).await; }
    }

    // RX2 TCI control setters
    pub async fn set_rx2_agc_mode(&mut self, mode: u8) {
        if let Some(t) = Some(&mut self.tci) { t.set_rx2_agc_mode(mode).await; }
    }
    pub async fn set_rx2_agc_gain(&mut self, gain: u8) {
        if let Some(t) = Some(&mut self.tci) { t.set_rx2_agc_gain(gain).await; }
    }
    pub async fn set_rx2_sql_enable(&mut self, on: bool) {
        if let Some(t) = Some(&mut self.tci) { t.set_rx2_sql_enable(on).await; }
    }
    pub async fn set_rx2_sql_level(&mut self, level: i16) {
        if let Some(t) = Some(&mut self.tci) { t.set_rx2_sql_level(level).await; }
    }
    pub async fn set_rx2_nb(&mut self, level: u8) {
        if let Some(t) = Some(&mut self.tci) { t.set_rx2_nb(level).await; }
    }
    pub async fn set_rx2_binaural(&mut self, on: bool) {
        if let Some(t) = Some(&mut self.tci) { t.set_rx2_binaural(on).await; }
    }
    pub async fn set_rx2_apf_enable(&mut self, on: bool) {
        if let Some(t) = Some(&mut self.tci) { t.set_rx2_apf_enable(on).await; }
    }
    pub async fn set_rx2_vfo_lock(&mut self, on: bool) {
        if let Some(t) = Some(&mut self.tci) { t.set_rx2_vfo_lock(on).await; }
    }
    pub async fn set_mute(&mut self, on: bool) {
        if let Some(t) = Some(&mut self.tci) { t.set_mute(on).await; }
    }
    pub async fn set_rx_mute(&mut self, on: bool) {
        if let Some(t) = Some(&mut self.tci) { t.set_rx_mute(on).await; }
    }
    pub async fn set_nf_enable(&mut self, on: bool) {
        if let Some(t) = Some(&mut self.tci) { t.set_nf_enable(on).await; }
    }
    pub async fn set_rx2_nf_enable(&mut self, on: bool) {
        if let Some(t) = Some(&mut self.tci) { t.set_rx2_nf_enable(on).await; }
    }
    pub async fn set_rx_balance(&mut self, value: i8) {
        if let Some(t) = Some(&mut self.tci) { t.set_rx_balance(value).await; }
    }

    // Tune, tune drive, monitor volume
    pub fn tune_drive(&self) -> u8 {
        self.tci.tune_drive
    }
    pub fn mon_volume(&self) -> i8 {
        self.tci.mon_volume
    }
    pub async fn set_tune(&mut self, on: bool) {
        if let Some(t) = Some(&mut self.tci) { t.set_tune(on).await; }
    }
    pub async fn set_tune_drive(&mut self, level: u8) {
        if let Some(t) = Some(&mut self.tci) { t.set_tune_drive(level).await; }
    }
    pub async fn set_mon_volume(&mut self, db: i8) {
        if let Some(t) = Some(&mut self.tci) { t.set_mon_volume(db).await; }
    }

    /// Check which connections need to be established (brief, no I/O).
    /// Returns (tci_url,) - TL2 v2: TCI-only, no CAT.
    pub fn needed_connections(&mut self) -> Option<String> {
        self.tci.needs_connect_info()
    }

    /// Accept established TCI connection from the background connector.
    pub fn accept_connections(
        &mut self,
        tci_stream: Option<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        >,
    ) {
        if let Some(stream) = tci_stream {
            self.tci.accept_stream(stream);
        }
    }

    /// DDC sample rate per receiver in Hz. In stock mode both RX1 and RX2 have the same
    /// value (TCI exposes one global `iq_samplerate`); per-RX divergence comes
    /// back via TL2-x fork extensions (Phase 3).
    pub fn ddc_sample_rate(&self, rx: usize) -> u32 {
        if rx == 0 { self.tci.ddc_sample_rate_rx1 } else { self.tci.ddc_sample_rate_rx2 }
    }

    /// Step attenuator value per receiver (0=RX1, 1=RX2). Positive dB from stock v2.10.3.14 rx_step_att_ex.
    pub fn step_att(&self, rx: usize) -> i32 {
        if rx == 0 { self.tci.step_att_rx1 } else { self.tci.step_att_rx2 }
    }

    /// Whether the step attenuator is currently enabled for the given receiver
    /// (stock v2.10.3.14 rx_step_att_enabled_ex).
    pub fn step_att_enabled(&self, rx: usize) -> bool {
        if rx == 0 { self.tci.step_att_enabled_rx1 } else { self.tci.step_att_enabled_rx2 }
    }

    /// Check if the connected TCI server advertises a capability
    pub fn has_tci_cap(&self, cap: &str) -> bool {
        self.tci.has_cap(cap)
    }

    /// Borrow the TCI connection (TL2 v2 always TCI).
    pub fn tci_ref(&self) -> Option<&crate::tci::TciConnection> {
        Some(&self.tci)
    }
    /// Check if Thetis advertises a TCI capability
    pub fn has_cap(&self, cap: &str) -> bool {
        self.tci.has_cap(cap)
    }

    /// TCI DDS center frequency per receiver (0=RX1, 1=RX2). Returns 0 if not in TCI mode.
    pub fn dds_freq(&self, receiver: usize) -> u64 {
        self.tci.dds_freq[receiver.min(1)]
    }

    /// Static calibration offset (dB) from TCI calibration_ex.
    /// This is meter_cal + xvtr_gain + 6m_gain - everything except step ATT.
    pub fn static_cal_offset(&self, receiver: usize) -> f32 {
        let idx = receiver.min(1);
        self.tci.meter_cal_offset[idx] + self.tci.xvtr_gain_offset[idx] + self.tci.six_m_gain_offset[idx]
    }

    /// Raw TCI S-meter dBm (peakBinDbm) for auto-calibration.
    pub fn smeter_raw_dbm(&self, receiver: usize) -> Option<f32> {
        self.tci.smeter_raw_dbm[receiver.min(1)]
    }

    /// What stands in the TX ring for Thetis, in samples; `None` without a
    /// TCI reader (build 193).
    pub fn tx_ring_level(&self) -> Option<usize> {
        self.tci.tx_ring_level()
    }

    /// Write TX audio to TCI ring buffer (only in TCI mode, no-op for CAT)
    pub fn write_tx_audio(&mut self, samples: &[f32]) {
        if let Some(tci) = Some(&mut self.tci) {
            tci.write_tx_audio(samples);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The turnaround watch waits only after TRX off, ignores the silence Thetis
    /// sends during TX, fires once on the first block with sound, and a TX in
    /// between cancels the wait (2026-10-04).
    #[test]
    fn rx_back_fires_once_on_the_first_sound_after_trx_off() {
        let w = RxBackWatch::default();
        let quiet = vec![0.0f32; 960];
        let mut sound = vec![0.0f32; 960];
        sound[10] = 0.01;
        let t = 1_000;
        assert_eq!(w.rx_block_at(t, &sound, 0), None, "not waiting: nothing to report");
        w.off_at.store(t, Ordering::Relaxed); // TRX off at t
        assert_eq!(w.rx_block_at(t + 100, &quiet, 0), None, "silence does not end the wait");
        assert_eq!(w.rx_block_at(t + 180, &sound, 960), Some(180), "the first sound reports");
        assert_eq!(w.rx_block_at(t + 200, &sound, 0), None, "and only once");
        // Second pass without a restart (working rule 9): off, then TX again before
        // any sound - cancelled, never reported.
        w.trx_off();
        assert_ne!(w.off_at.load(Ordering::Relaxed), 0, "trx_off arms the wait");
        w.off_at.store(t, Ordering::Relaxed); // on the made-up clock from here (round 100: B4)
        w.trx_on();
        assert_eq!(w.rx_block_at(t + 300, &sound, 0), None, "a TX in between cancels the wait");
        // The RX loop found no client: the same.
        w.off_at.store(t, Ordering::Relaxed);
        w.cancel();
        assert_eq!(w.rx_block_at(t + 50, &sound, 0), None, "no client: cancelled");
        // A third turnaround still reports.
        w.off_at.store(t, Ordering::Relaxed);
        assert_eq!(w.rx_block_at(t + 160, &sound, 0), Some(160));
        // A wait older than the give-up is dropped, not reported.
        w.off_at.store(5, Ordering::Relaxed);
        assert_eq!(w.rx_block_at(5 + RX_BACK_GIVE_UP_MS, &quiet, 0), None);
        assert_eq!(w.off_at.load(Ordering::Relaxed), 5, "at the limit it still waits");
        assert_eq!(w.rx_block_at(5 + RX_BACK_GIVE_UP_MS + 1, &sound, 0), None, "a stale wait goes, unreported");
        assert_eq!(w.off_at.load(Ordering::Relaxed), 0);
    }

    /// What no unit test can drive: the RX loop feeds the watch and cancels it
    /// without clients, `set_state` arms it only on Tx -> Rx and after the command
    /// has gone, and the named log line exists (round 99: B5, B7, B8 stayed green).
    #[test]
    fn the_turnaround_watch_is_wired_where_it_must_be() {
        let ptt = include_str!("ptt.rs");
        let ptt = &sdr_remote_core::source_text::production_code(ptt);
        // Per trimmed line, so a commented-out call does not count (round 100: D10),
        // and within the Thetis RX loop, not the first such branch in the file.
        let lp = include_str!("audio_loops.rs");
        let lp = &lp[lp.find("pub async fn tci_multichannel_audio_loop(").unwrap()..];
        let lp = &lp[..lp.find("\npub async fn ").unwrap_or(lp.len())];
        let has_line = |s: &str, l: &str| s.lines().filter(|x| x.trim() == l).count();
        // Build 151: the buffers moved into ThetisBuffers, and the two lines with them.
        assert_eq!(has_line(lp, "rx_back.rx_block(&rx1_frame, step.rx1_left);"), 1);
        let no_clients = &lp[lp.find("if step.no_listener {").unwrap()..];
        let no_clients = &no_clients[..no_clients.find("continue;").unwrap()];
        assert_eq!(has_line(no_clients, "rx_back.cancel();"), 1, "no client: the wait goes");
        let set_state = &ptt[ptt.find("async fn set_state(").unwrap()..];
        let set_state = &set_state[..set_state.find("\n    fn is_connected").unwrap()];
        let sent = set_state.find("self.tci.send(cmd).await;").unwrap();
        let armed = set_state.find("} else if old == PttState::Tx {\n            self.rx_back.trx_off();").unwrap();
        assert!(armed > sent, "armed after the command, and only from Tx");
        assert!(ptt.contains("PTT metrics: RX audio back {} ms after TRX off"));
    }

    /// The tail is due exactly when its time is over, not before, and only while
    /// a release is pending (build 103: the timer ends it, not the 100 ms tick).
    #[tokio::test]
    async fn the_tail_ends_when_its_time_is_over() {
        let mut p = PttController::new_tci(None, None);
        assert!(!p.tail_due(Instant::now()), "nothing pending");
        p.state = PttState::Tx;
        p.release_from_playout(2);
        let start = p.pending_release.unwrap();
        assert_eq!(p.tail_delay(), Duration::from_millis(50), "2 frames x 20 + 10");
        assert!(!p.tail_due(start + Duration::from_millis(49)));
        assert!(p.tail_due(start + Duration::from_millis(50)));
        // Called early: nothing; a new TX in between: nothing to end.
        p.pending_release = Some(Instant::now());
        p.finish_tail_if_due().await;
        assert!(p.state == PttState::Tx && p.pending_release.is_some(), "early: still transmitting");
        p.pending_release = Some(Instant::now() - Duration::from_millis(60));
        p.finish_tail_if_due().await;
        assert!(p.state == PttState::Rx && p.pending_release.is_none(), "due: back to RX");
        p.finish_tail_if_due().await;
        assert!(p.state == PttState::Rx, "twice: harmless");
        // Not transmitting: a tail left behind ends nothing (round 103, M6).
        p.pending_release = Some(Instant::now() - Duration::from_millis(500));
        assert!(!p.tail_due(Instant::now()), "Rx: never due");
        // One tail per release: a second call while one runs starts nothing.
        let mut p = PttController::new_tci(None, None);
        p.state = PttState::Tx;
        assert!(p.release_from_playout(2));
        assert!(!p.release_from_playout(3), "a tail is running: no second one");
        assert_eq!(p.tail_delay(), Duration::from_millis(50), "and its length unchanged");
        // Keyed again during the tail: the tail goes (round 103, M5 - the microphone half).
        p.activate_from_playout();
        assert!(p.pending_release.is_none() && p.state == PttState::Tx, "re-keyed: still transmitting, no tail");
        // The floor: depth 0 is 25 ms, not 10 (round 103, J7).
        let mut p = PttController::new_tci(None, None);
        p.state = PttState::Tx;
        p.release_from_playout(0);
        assert_eq!(p.tail_delay(), Duration::from_millis(25));
    }

    /// The server sets the timer where it starts the tail (build 103) - a tail
    /// left to the 100 ms tick again would still pass the unit test above.
    #[test]
    fn the_tail_timer_is_set_where_the_release_starts() {
        let src = include_str!("network.rs");
        let body = &sdr_remote_core::source_text::production_code(src);
        let lines: Vec<&str> = body.lines().map(str::trim).collect();
        let at = lines.iter().position(|l| *l == "let started = ptt.release_from_playout(depth);").unwrap();
        let after = &lines[at..(at + 25).min(lines.len())];
        assert!(after.contains(&"let tail = ptt.tail_delay();"), "{after:?}");
        assert!(after.contains(&"tokio::time::sleep(tail).await;"), "it waits (round 103, J5): {after:?}");
        assert!(after.contains(&"ptt.lock().await.finish_tail_if_due().await;"), "{after:?}");
    }

    /// The Thetis transmit buffer is a transmit buffer (build 237): the release
    /// is played out (`release_from_playout`), and a receive buffer would hold
    /// a release that came in a short burst until the safety net.
    #[test]
    fn the_thetis_transmit_buffer_does_not_wait_with_the_release_in_hand() {
        let src = include_str!("network.rs");
        let body = &sdr_remote_core::source_text::production_code(src);
        assert_eq!(body.matches("let mut jitter_buf = JitterBuffer::for_transmit(TX_MIN_DEPTH, TX_MAX_DEPTH);").count(), 1);
        assert_eq!(body.matches("JitterBuffer::new(").count(), 0, "no receive buffer here");
    }

    /// The CW playout stands in for the audio stream, never for the client:
    /// its activity feeds the packet watchdog and leaves the heartbeat alone,
    /// so a client that dies with the key down is still dropped at 2 s.
    #[test]
    fn cw_activity_feeds_the_packet_watchdog_and_not_the_heartbeat() {
        let mut p = PttController::new_tci(None, None);
        assert_eq!(p.watchdog_stamps(), (None, None));
        p.record_cw_activity();
        let (packet, heartbeat) = p.watchdog_stamps();
        assert!(packet.is_some(), "the packet watchdog is fed");
        assert!(heartbeat.is_none(), "the heartbeat is the client's to give");
        p.record_ptt_packet();
        let (packet, heartbeat) = p.watchdog_stamps();
        assert!(packet.is_some() && heartbeat.is_some(), "a packet from the client is both");
        // And the decision that follows from it: fresh packets, a heartbeat
        // that went silent - released.
        assert_eq!(
            safety_action(
                PttState::Tx,
                Some(Duration::from_millis(HEARTBEAT_TIMEOUT_MS + 1)),
                Some(Duration::from_millis(10)),
                Duration::from_millis(HEARTBEAT_TIMEOUT_MS),
                Duration::from_millis(PTT_PACKET_TIMEOUT_MS),
            ),
            SafetyAction::ReleaseHeartbeatLost
        );
    }

    /// `activate_for_cw` had no test at all (review 313). What it does: the
    /// Amplitec gate refuses and changes nothing; otherwise MOX goes up now, a
    /// pending microphone prefill is dropped (MOX is up already, the
    /// microphone needs none), a pending tail release is cancelled (the run
    /// holds the transmitter), and the tail length is left for the next
    /// microphone release to compute.
    #[tokio::test]
    async fn activate_for_cw_takes_over_the_microphones_pending_state() {
        // The gate.
        let mut p = PttController::new_tci(None, None);
        p.activate_from_playout(); // a microphone prefill is pending
        p.tx_blocked_handle().store(true, Ordering::Relaxed);
        assert!(!p.activate_for_cw().await, "receive-only: refused");
        assert!(p.state != PttState::Tx && p.pending_activate.is_some(), "and nothing touched");
        // A pending prefill: MOX up at once, the prefill gone.
        let mut p = PttController::new_tci(None, None);
        p.activate_from_playout();
        assert!(p.pending_activate.is_some() && p.state != PttState::Tx);
        assert!(p.activate_for_cw().await);
        assert!(p.state == PttState::Tx && p.pending_activate.is_none());
        // A pending tail release: cancelled, the tail length untouched.
        p.release_from_playout(2);
        let tail = p.tail_delay_ms;
        assert!(p.pending_release.is_some());
        assert!(p.activate_for_cw().await);
        assert!(p.pending_release.is_none() && p.state == PttState::Tx);
        assert_eq!(p.tail_delay_ms, tail);
    }

    /// The order of the deadlines when a link dies, which the safety of a
    /// transmitter rests on and which until review 314 stood only in prose:
    /// MOX drops first (heartbeat, 2 s), and only then is the transmitter
    /// given away (the holder's session time-out) or the CW key lifted by the
    /// playout's own watchdog. The packet watchdog may never outlast the
    /// heartbeat (its comment says so).
    #[test]
    fn mox_drops_before_the_transmitter_is_given_away_or_the_key_lifted() {
        assert!(PTT_PACKET_TIMEOUT_MS <= HEARTBEAT_TIMEOUT_MS, "the packet watchdog outlasts the heartbeat");
        assert!(HEARTBEAT_TIMEOUT_MS < crate::session::timeout_secs(true, false) * 1000, "the lock goes before MOX");
        assert!(
            Duration::from_millis(HEARTBEAT_TIMEOUT_MS) < crate::cw_playout::WATCHDOG,
            "the CW key is lifted before MOX drops"
        );
    }

    /// The ceiling. This is the property the whole layer exists for.
    ///
    /// The first version of these tests asserted that the packet guard does not
    /// outlive the heartbeat guard, and that both leave room for jitter. A review
    /// showed that setting BOTH constants to a full minute passes all of it: a
    /// relation and a lower bound say nothing about how long a transmitter may run
    /// on. Mutating one constant was not the proof; mutating both together was.
    ///
    /// Two seconds is the owner's decision, in his words "de ptt of na 2 sec is echt
    /// snel genoeg", and that decision is what this number is anchored to. Raising it
    /// is allowed - by changing this line, deliberately, with a reason.
    #[test]
    fn a_transmitter_is_never_left_keyed_longer_than_the_owner_allowed() {
        const OWNER_CEILING_MS: u64 = 2000;
        assert!(
            HEARTBEAT_TIMEOUT_MS <= OWNER_CEILING_MS,
            "a transmitter may not run on longer than the owner decided"
        );
        assert!(
            PTT_PACKET_TIMEOUT_MS <= OWNER_CEILING_MS,
            "a transmitter may not run on longer than the owner decided"
        );
    }

    /// And the packet guard may not outlive the guard that decides the client is gone.
    #[test]
    fn a_transmitter_is_never_keyed_past_the_moment_its_holder_is_gone() {
        assert!(
            PTT_PACKET_TIMEOUT_MS <= HEARTBEAT_TIMEOUT_MS,
            "the packet guard must not outlive the guard that decides the client is gone"
        );
    }

    /// Both must still leave room for ordinary jitter, or they fire on nothing.
    ///
    /// The client heartbeat is every 500ms and transmit packets arrive 50 times a
    /// second. The packet guard was 500ms until four measured handovers showed a
    /// mobile network stalls longer than that while the connection survives.
    #[test]
    fn the_guards_leave_room_for_ordinary_jitter() {
        const CLIENT_HEARTBEAT_INTERVAL_MS: u64 = 500;
        assert!(
            HEARTBEAT_TIMEOUT_MS >= CLIENT_HEARTBEAT_INTERVAL_MS * 3,
            "fewer than three missed heartbeats is jitter, not a lost client"
        );
        assert!(
            PTT_PACKET_TIMEOUT_MS >= CLIENT_HEARTBEAT_INTERVAL_MS * 2,
            "a network handover stalls longer than this; measured, not assumed"
        );
    }

    const HB: Duration = Duration::from_millis(HEARTBEAT_TIMEOUT_MS);
    const PKT: Duration = Duration::from_millis(PTT_PACKET_TIMEOUT_MS);

    fn ms(v: u64) -> Option<Duration> {
        Some(Duration::from_millis(v))
    }

    /// A silence inside a press ends what the buffer holds exactly where the
    /// safety ends the transmission: past the same packet window, with the same
    /// comparison (build 248). A clock that was never set ends nothing - there
    /// is no audio of it to throw away.
    #[test]
    fn a_silence_ends_the_buffered_audio_where_the_safety_ends_the_over() {
        for v in [0, 500, 1_000, 1_999, 2_000, 2_001, 2_500, 30_000] {
            let since = ms(v);
            let safety = safety_action(PttState::Tx, ms(0), since, HB, PKT) == SafetyAction::ReleaseNoPackets;
            assert_eq!(silence_ended_the_over(since), safety, "{v} ms");
        }
        assert!(!silence_ended_the_over(ms(2_000)), "the window itself still plays");
        assert!(silence_ended_the_over(ms(2_001)));
        assert!(!silence_ended_the_over(None), "never a packet: nothing buffered from it");
        let mut p = PttController::new_tci(None, None);
        assert_eq!(p.packet_silence(Instant::now()), None);
        p.record_ptt_packet();
        let later = Instant::now() + Duration::from_millis(2_500);
        assert!(p.packet_silence(later).is_some_and(|d| d >= Duration::from_millis(2_500)));
    }

    /// Not transmitting: the guards have no business doing anything.
    #[test]
    fn a_receiver_is_never_touched_by_the_safety_layer() {
        assert_eq!(
            safety_action(PttState::Rx, None, None, HB, PKT),
            SafetyAction::None
        );
        assert_eq!(
            safety_action(PttState::Rx, ms(60_000), ms(60_000), HB, PKT),
            SafetyAction::None
        );
    }

    /// Transmitting with both clocks fresh: leave it alone.
    #[test]
    fn a_healthy_stream_keeps_the_transmitter() {
        assert_eq!(
            safety_action(PttState::Tx, ms(100), ms(20), HB, PKT),
            SafetyAction::None
        );
    }

    /// A missing clock releases rather than disabling its guard.
    ///
    /// Both checks used to sit behind `if let Some(..)`. A clock that was never set
    /// switched its guard off entirely and the transmitter stayed keyed - a guard
    /// whose failure mode is "stay keyed" has its default the wrong way round.
    #[test]
    fn a_clock_that_was_never_set_releases_rather_than_going_quiet() {
        assert_eq!(
            safety_action(PttState::Tx, None, ms(20), HB, PKT),
            SafetyAction::ReleaseHeartbeatLost
        );
        assert_eq!(
            safety_action(PttState::Tx, ms(100), None, HB, PKT),
            SafetyAction::ReleaseNoPackets
        );
    }

    /// The stream stopped while the client is still there: the packet guard, and it
    /// must be that one - the distinction is the whole diagnostic value of the line.
    #[test]
    fn a_stalled_stream_under_a_live_client_is_the_packet_guard() {
        assert_eq!(
            safety_action(
                PttState::Tx,
                ms(100),
                ms(PTT_PACKET_TIMEOUT_MS + 1),
                HB,
                PKT
            ),
            SafetyAction::ReleaseNoPackets
        );
    }

    /// Total loss: the heartbeat guard wins the tie, because both windows are equal
    /// and its line says more.
    #[test]
    fn a_total_loss_reports_the_lost_client_not_the_stalled_stream() {
        assert_eq!(
            safety_action(
                PttState::Tx,
                ms(HEARTBEAT_TIMEOUT_MS + 1),
                ms(PTT_PACKET_TIMEOUT_MS + 1),
                HB,
                PKT
            ),
            SafetyAction::ReleaseHeartbeatLost
        );
    }

    /// Exactly on the window is not yet over it.
    #[test]
    fn the_boundary_itself_does_not_release() {
        assert_eq!(
            safety_action(
                PttState::Tx,
                ms(HEARTBEAT_TIMEOUT_MS),
                ms(PTT_PACKET_TIMEOUT_MS),
                HB,
                PKT
            ),
            SafetyAction::None
        );
    }

    /// A stale packet clock releases on the very first tick after Tx is re-entered.
    ///
    /// This documents a real sequence rather than a wish. A safety release does not
    /// clear `last_ptt_packet`, and the jitter buffer can still hold transmit frames
    /// that put the state back to Tx a tick later - so the transmitter re-keys and is
    /// dropped again immediately, once per buffered frame, and the log shows two
    /// SAFETY lines close together. Whether that sequence is reached in the field is
    /// not established; this is what happens when it is, and it is the shape to look
    /// for in the next log.
    #[test]
    fn a_stale_packet_clock_releases_again_at_once() {
        let long_ago = ms(30_000);
        assert_eq!(
            safety_action(PttState::Tx, ms(100), long_ago, HB, PKT),
            SafetyAction::ReleaseNoPackets
        );
    }

    /// A clock that was never set says "never", and never reads as a measured gap.
    #[test]
    fn a_silence_that_was_never_measured_says_so() {
        assert_eq!(describe_silence(ms(2500)), "2.5s");
        assert!(describe_silence(None).contains("never"));
        assert!(
            !describe_silence(None).contains("0.0"),
            "no reading must not print as a gap of zero"
        );
    }
}

/// The Thetis part of a heartbeat acknowledgement (build 217). All false when
/// no Thetis is configured.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ThetisStatus {
    pub tci_connected: bool,
    pub thetis_running: bool,
    pub thetis_starting: bool,
    /// From the RX-only loop, not computed again here.
    pub rx_only_unguarded: bool,
}

/// "Receive only" in stock Thetis through CAT `ZZTI` over `run_cat_ex`
/// (build 221), where the fork has `rx_only_ex`. `ZZTI` has no read form, so
/// this server only ever turns off what it turned on itself (build 222): a
/// "Receive only" from a transverter band or set by hand stays untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StockRxOnly {
    /// The position became RX-only, or a new TCI connection on one.
    TakeOver,
    /// Still RX-only: said again, as stock does not report it back.
    Reassert,
    /// The position is no longer RX-only, and the "Receive only" is ours.
    Release,
}

impl StockRxOnly {
    pub fn zzti(self) -> &'static str {
        match self {
            StockRxOnly::TakeOver | StockRxOnly::Reassert => "ZZTI1",
            StockRxOnly::Release => "ZZTI0",
        }
    }

    /// After this step the "Receive only" in Thetis is this server's.
    pub fn ours(self) -> bool {
        match self {
            StockRxOnly::TakeOver | StockRxOnly::Reassert => true,
            StockRxOnly::Release => false,
        }
    }
}

/// How often `ZZTI1` is said again on an RX-only position (build 221).
pub const STOCK_RX_ONLY_REASSERT: std::time::Duration = std::time::Duration::from_secs(1);

/// What the stock "Receive only" step decides on. Named fields (build 223,
/// review of 222): four booleans in a row went by position, and two of them
/// swapped brought back the `ZZTI0` of 221 with every test green.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StockRxOnlyInput {
    /// A new TCI connection since the last tick.
    pub new_conn: bool,
    /// The Amplitec position changed since the last tick.
    pub pos_changed: bool,
    /// The active position is RX-only.
    pub pos_is_blocked: bool,
    /// This server sent `ZZTI1` and has not turned it off since.
    pub we_set_it: bool,
}

impl StockRxOnlyInput {
    /// The step for this tick; `since_said` is the time since `ZZTI` was last
    /// sent.
    pub fn step(self, since_said: std::time::Duration) -> Option<StockRxOnly> {
        if self.pos_is_blocked {
            if self.new_conn || self.pos_changed {
                Some(StockRxOnly::TakeOver)
            } else if since_said >= STOCK_RX_ONLY_REASSERT {
                Some(StockRxOnly::Reassert)
            } else {
                None
            }
        } else if self.we_set_it {
            Some(StockRxOnly::Release)
        } else {
            None
        }
    }
}

/// Which TX the RX-only loop stops on an RX-only Amplitec position (build 220).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RxOnlyTxOff {
    /// ThetisLink's own PTT: `ZZTX0`, as before.
    Ours,
    /// A TX or TUNE Thetis was already making itself when the position
    /// changed: the TRX arm only answers a new "on", so this one is stopped
    /// from the loop. What that can stop in stock Thetis (a TCI PTT and a
    /// TUNE, not a MOX by button or footswitch): `rx_only_stop_commands`.
    Thetis,
}

/// What the RX-only loop's TX stop decides on (build 220). Named fields since
/// build 223 (review of 222, the same class as the stock step): with the
/// position and our TX swapped, a TX on an RX-only position was silently never
/// stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RxOnlyTxInput {
    /// The active Amplitec position is RX-only.
    pub pos_is_blocked: bool,
    /// ThetisLink's own PTT is on.
    pub our_tx: bool,
    /// Thetis reports TX or TUNE.
    pub thetis_tx: bool,
}

impl RxOnlyTxInput {
    /// The decision, kept out of the loop so a test can read it. Our own TX is
    /// reported by Thetis too, so it comes first.
    pub fn tx_off(self) -> Option<RxOnlyTxOff> {
        if !self.pos_is_blocked {
            None
        } else if self.our_tx {
            Some(RxOnlyTxOff::Ours)
        } else if self.thetis_tx {
            Some(RxOnlyTxOff::Thetis)
        } else {
            None
        }
    }
}

/// The transmitter as the RX-only loop reads it (build 226).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxNow {
    /// VFO-A mode as Thetis reports it.
    pub mode: u8,
    /// ThetisLink's own PTT is on.
    pub ours: bool,
    /// Thetis reports TX or TUNE, whoever keyed it.
    pub thetis: bool,
}

/// The TCI link as the RX-only loop reads it (build 226).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TciLinkNow {
    /// The TCI connection to Thetis is up.
    pub up: bool,
    /// Thetis answered a `run_cat_ex` in this connection.
    pub answers_run_cat_ex: bool,
    /// TCI connections made so far.
    pub connections: u64,
}

/// What decides whether an RX-only Amplitec position is guarded by Thetis
/// itself (builds 215-216). Named fields: three booleans in a row went by
/// position, and two of them swapped made the warning never come (review of 215).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RxOnlyGuard {
    /// The active Amplitec position is marked receive-only.
    pub position_rx_only: bool,
    /// The TCI connection to Thetis is up.
    pub tci_connected: bool,
    /// Thetis advertises `rx_only_ex` (the fork with its extensions on).
    pub has_rx_only_ex: bool,
    /// Thetis answered a `run_cat_ex` in this TCI connection: stock
    /// v2.10.3.14+, where CAT `ZZTI1` sets "Receive only" (build 221).
    pub answers_run_cat_ex: bool,
}

impl RxOnlyGuard {
    /// An RX-only position that Thetis cannot guard by itself: receive-only,
    /// TCI up, no `rx_only_ex`, and no answer to `run_cat_ex` (a Thetis older
    /// than v2.10.3.14; since build 221 a newer stock gets CAT `ZZTI1`). Then
    /// the server can stop only a TCI PTT and a TUNE, not a footswitch or the
    /// MOX button in Thetis. With TCI down there is nothing to say about Thetis
    /// at all - a choice of the owner (2026-10-16): the screens already say
    /// Thetis is not reachable.
    pub fn unguarded(self) -> bool {
        self.position_rx_only && self.tci_connected && !self.has_rx_only_ex && !self.answers_run_cat_ex
    }
}

#[cfg(test)]
mod rx_only_unguarded_tests {
    /// Only all three together: an RX-only position, TCI up, and no
    /// `rx_only_ex` - and the heartbeat and the log line both ask this one
    /// rule (build 215).
    #[test]
    fn an_rx_only_position_is_unguarded_only_without_rx_only_ex_while_tci_is_up() {
        use super::RxOnlyGuard;
        let u = |position_rx_only, tci_connected, has_rx_only_ex, answers_run_cat_ex| {
            RxOnlyGuard { position_rx_only, tci_connected, has_rx_only_ex, answers_run_cat_ex }.unguarded()
        };
        assert!(u(true, true, false, false), "a Thetis without rx_only_ex and without run_cat_ex");
        assert!(!u(true, true, true, false), "the fork refuses transmitting itself");
        assert!(!u(true, true, false, true), "stock v2.10.3.14+: CAT ZZTI1 sets Receive only (build 221)");
        assert!(!u(false, true, false, false), "not an RX-only position");
        assert!(!u(true, false, false, false), "TCI down: nothing to say about Thetis");
        // One place computes it (the RX-only loop) and stores it; the heartbeat
        // reads it by name (review of 216).
        let net = include_str!("network.rs");
        assert_eq!(net.matches("crate::ptt::RxOnlyGuard {").count(), 1, "the RX-only loop only");
        assert_eq!(net.matches("rx_only_unguarded_gate.store(unguarded_now, Ordering::Relaxed);").count(), 1);
        assert_eq!(net.matches("ptt.thetis_status()").count(), 1, "the heartbeat");
        assert_eq!(net.matches(".with(sdr_remote_core::protocol::ServerStateFlags::RX_ONLY_UNGUARDED);").count(), 1);
    }

    /// The protection per antenna position runs whether or not a client is
    /// connected (review of 217): the equipment tick skips the rest of its work
    /// without clients, and the TX gate, "Receive only" in the fork, the reactive
    /// ZZTX0 and the power cap sat behind that skip. Build 219: also the drive
    /// restore on a position change, the DX spots to Thetis, and no other
    /// `continue` above the skip (review of 218, P1 and P2). Build 220: the
    /// tuner's stale check, the spots' TCI bookkeeping, and the stop of a TX
    /// Thetis was already making (review of 219).
    #[test]
    fn the_protection_per_antenna_position_runs_before_the_no_client_skip() {
        let net = include_str!("network.rs");
        let tick = &net[net.find("_ = equipment_tick.tick() => {").expect("the equipment tick")..];
        let skip = tick.find("if eq_addrs.is_empty() { continue; }").expect("the no-client skip");
        for what in [
            "tx_blocked_gate.store(",
            "rx_only_unguarded_gate.store(unguarded_now, Ordering::Relaxed);",
            "p.set_rx_only(true).await;",
            "ptt.lock().await.send_cat(\"ZZTX0\").await;",
            "crate::power_cap::tick(",
            "crate::power_cap::on_position_change(",
            "ptt_guard.send_tci_spot(",
            "tuner_ref.set_stale(is_stale);",
            "tci_spots_sent.clear();",
            "crate::ptt::RxOnlyTxInput {",
            ".force_thetis_tx_off().await;",
            "crate::ptt::StockRxOnlyInput {",
            "ptt.lock().await.set_rx_only_cat(step).await;",
        ] {
            let at = tick.find(what).unwrap_or_else(|| panic!("{what} in the equipment tick"));
            assert!(at < skip, "{what} runs only with a client");
        }
        let above = &tick[..skip];
        let mut words = above.split(|c: char| !(c.is_alphanumeric() || c == '_'));
        assert!(!words.any(|w| w == "continue"), "a second skip above the protection");
        // The server window gets the flag (review of 217, B1).
        let main = include_str!("main.rs");
        assert_eq!(main.matches(".set(ptt.lock().await.rx_only_unguarded_handle());").count(), 1);
    }

    /// "Receive only" in stock Thetis through CAT `ZZTI` (build 221): on at a
    /// new RX-only position or a new TCI connection on one, and again every
    /// second while RX-only, because stock does not report it back. Off only to
    /// undo this server's own `ZZTI1` (build 222, review of 221): a "Receive
    /// only" set by a transverter band or by hand is never touched, with or
    /// without an Amplitec.
    #[test]
    fn stock_receive_only_follows_the_position_through_cat() {
        use super::{StockRxOnly, StockRxOnlyInput};
        use std::time::Duration;
        let s = |new_conn, pos_changed, pos_is_blocked, we_set_it, since_ms| {
            StockRxOnlyInput { new_conn, pos_changed, pos_is_blocked, we_set_it }.step(Duration::from_millis(since_ms))
        };
        assert_eq!(s(false, true, true, false, 0), Some(StockRxOnly::TakeOver));
        assert_eq!(s(true, false, true, true, 0), Some(StockRxOnly::TakeOver), "a new connection on an RX-only position");
        assert_eq!(s(false, false, true, true, 999), None, "within the second");
        assert_eq!(s(false, false, true, true, 1000), Some(StockRxOnly::Reassert));
        assert_eq!(s(false, true, false, true, 0), Some(StockRxOnly::Release), "ours, undone");
        assert_eq!(s(true, false, false, true, 0), Some(StockRxOnly::Release), "ours, after a new connection");
        // Never off without our own on first.
        for new_conn in [false, true] {
            for pos_changed in [false, true] {
                for since in [0, 5000] {
                    assert_eq!(s(new_conn, pos_changed, false, false, since), None, "not ours: {new_conn} {pos_changed} {since}");
                }
            }
        }
        assert_eq!(s(false, false, false, true, 5000), Some(StockRxOnly::Release), "ours stands on a TX position");
        assert_eq!(StockRxOnly::TakeOver.zzti(), "ZZTI1");
        assert_eq!(StockRxOnly::Reassert.zzti(), "ZZTI1");
        assert_eq!(StockRxOnly::Release.zzti(), "ZZTI0");
        assert!(StockRxOnly::TakeOver.ours() && StockRxOnly::Reassert.ours() && !StockRxOnly::Release.ours());
        // The loop keeps what it sent, and hands it over by name (review of
        // 222: four booleans in a row, two swapped stayed green).
        let net = include_str!("network.rs");
        assert_eq!(net.matches("stock_rx_only_ours = step.ours();").count(), 1);
        assert_eq!(net.matches("we_set_it: stock_rx_only_ours,").count(), 1);
        // Only without the fork's cap, with TCI up (the loop, which no test drives).
        let net = include_str!("network.rs");
        assert_eq!(net.matches("if !has_rx_only_cap && tci_up_now {").count(), 1);
        let ptt = include_str!("ptt.rs");
        assert_eq!(ptt.matches("        self.tci.run_cat(step.zzti()).await\n").count(), 1, "and it is sent");
    }

    /// Which TX the RX-only loop stops (build 220): ours by `ZZTX0`, as before;
    /// a TX or TUNE Thetis was already making itself - the TRX arm only sees a
    /// new "on".
    #[test]
    fn the_rx_only_loop_stops_ours_and_one_thetis_was_already_making() {
        use super::{RxOnlyTxInput, RxOnlyTxOff};
        // Named, like the stock step (review of 222, place B): two of three
        // booleans swapped made this stop silently never come.
        let rx_only_tx_off = |pos_is_blocked, our_tx, thetis_tx| {
            RxOnlyTxInput { pos_is_blocked, our_tx, thetis_tx }.tx_off()
        };
        assert_eq!(rx_only_tx_off(false, true, true), None, "not an RX-only position");
        assert_eq!(rx_only_tx_off(true, false, false), None, "nobody transmits");
        assert_eq!(rx_only_tx_off(true, true, false), Some(RxOnlyTxOff::Ours));
        assert_eq!(rx_only_tx_off(true, true, true), Some(RxOnlyTxOff::Ours), "ours is reported by Thetis too");
        assert_eq!(rx_only_tx_off(true, false, true), Some(RxOnlyTxOff::Thetis));
        // Thetis' own TUNE counts as its TX, also when no TRX report says so.
        let net = include_str!("network.rs");
        assert_eq!(net.matches("our_tx: tx_active,").count(), 1);
    }

    /// The safety check hands its clocks and windows over by name (build 229,
    /// review of 228): each window with its own constant.
    #[test]
    fn the_safety_check_hands_its_clocks_over_by_name() {
        let src = include_str!("ptt.rs");
        let src = &sdr_remote_core::source_text::production_code(src);
        for line in [
            "heartbeat_timeout: Duration::from_millis(HEARTBEAT_TIMEOUT_MS),",
            "packet_timeout: Duration::from_millis(PTT_PACKET_TIMEOUT_MS),",
            "        match clocks.action() {",
        ] {
            assert_eq!(src.matches(line).count(), 1, "{line}");
        }
    }

    /// The RX-only loop reads the transmitter and the TCI link by name (build
    /// 226, review of 224): each field from its own source, and the loop takes
    /// them apart by name too.
    #[test]
    fn the_rx_only_loop_reads_its_state_by_name() {
        use super::{PttController, PttState, TciLinkNow, TxNow};
        let mut p = PttController::new_tci(None, None);
        p.tci.vfo_a_mode = 7;
        assert_eq!(p.tx_now(), TxNow { mode: 7, ours: false, thetis: false });
        p.state = PttState::Tx;
        assert_eq!(p.tx_now(), TxNow { mode: 7, ours: true, thetis: false }, "ours is our PTT");
        p.state = PttState::Rx;
        p.tci.set_on_the_air_for_test(true, false);
        assert_eq!(p.tx_now(), TxNow { mode: 7, ours: false, thetis: true }, "a TX Thetis makes");
        p.tci.set_on_the_air_for_test(false, true);
        assert_eq!(p.tx_now(), TxNow { mode: 7, ours: false, thetis: true }, "Thetis' own TUNE counts");

        assert_eq!(p.tci_link_now(), TciLinkNow { up: false, answers_run_cat_ex: false, connections: 0 });
        p.tci.set_link_for_test(true, 3);
        assert_eq!(p.tci_link_now(), TciLinkNow { up: true, answers_run_cat_ex: false, connections: 3 });
        p.tci.answers_run_cat_ex = true;
        p.tci.set_link_for_test(false, 3);
        assert_eq!(p.tci_link_now(), TciLinkNow { up: false, answers_run_cat_ex: true, connections: 3 });

        let net = include_str!("network.rs");
        for line in [
            "let crate::ptt::TxNow { mode: mode_u8, ours: tx_active, thetis: thetis_tx } = ptt.lock().await.tx_now();",
            "let crate::ptt::TciLinkNow { up: tci_up_now, answers_run_cat_ex, connections: tci_conn_now } =",
        ] {
            assert_eq!(net.matches(line).count(), 1, "{line}");
        }
    }

    /// The heartbeat's status carries what the loop stored, under its own name.
    #[test]
    fn the_heartbeat_status_reads_the_flag_the_rx_only_loop_wrote() {
        use super::PttController;
        use std::sync::atomic::Ordering;
        let p = PttController::new_tci(None, None);
        assert!(!p.thetis_status().rx_only_unguarded);
        p.rx_only_unguarded_handle().store(true, Ordering::Relaxed);
        let s = p.thetis_status();
        assert!(s.rx_only_unguarded);
        assert!(!s.tci_connected && !s.thetis_starting, "no other field follows it");
        p.rx_only_unguarded_handle().store(false, Ordering::Relaxed);
        assert!(!p.thetis_status().rx_only_unguarded, "and back");
    }
}
