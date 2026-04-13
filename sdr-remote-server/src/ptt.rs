#![allow(dead_code)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use log::{error, info, warn};

use crate::cat::CatConnection;
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
            return false;
        }

        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        let mut found = false;
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
                    found = true;
                    break;
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }

        CloseHandle(snapshot);
        found
    }
}

/// Timeout for Thetis launch (seconds)
const THETIS_LAUNCH_TIMEOUT_S: u64 = 60;

/// Safety timeout: release PTT if no packets received for this duration
const PTT_PACKET_TIMEOUT_MS: u64 = 500;

/// Safety timeout: release PTT if heartbeat lost for this duration
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

/// Radio backend: CAT (TCP) or TCI (WebSocket)
pub enum RadioBackend {
    Cat(CatConnection),
    Tci(TciConnection),
}

/// PTT state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PttState {
    Rx,
    Tx,
}

/// PTT controller with safety layers.
///
/// Safety layers:
/// 1. PTT state in every audio packet (checked 50x/sec)
/// 2. Burst of 5 packets on PTT state change (handled by client)
/// 3. 500ms timeout: no packets -> PTT released
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
    /// Radio backend — CAT or TCI
    pub radio: RadioBackend,
    /// Auxiliary TCP CAT connection (runs alongside TCI for commands TCI doesn't support)
    aux_cat: Option<CatConnection>,
    /// Deferred aux CAT address — created lazily when TCI _ex caps are not available
    aux_cat_addr: Option<String>,
    thetis_path: Option<String>,
    pending_power_on: bool,
    thetis_launch_time: Option<Instant>,
    /// Whether using TCI mode (affects timing constants and PTT commands)
    is_tci: bool,
}

impl PttController {
    /// Create with CAT backend (legacy mode)
    pub fn new(cat_addr: Option<&str>, thetis_path: Option<String>) -> Self {
        Self {
            state: PttState::Rx,
            last_ptt_packet: None,
            last_heartbeat: None,
            pending_activate: None,
            pending_release: None,
            tail_delay_ms: PTT_TAIL_MIN_MS,
            ptt_active: Arc::new(AtomicBool::new(false)),
            radio: RadioBackend::Cat(CatConnection::new(cat_addr)),
            aux_cat: None,
            aux_cat_addr: None,
            thetis_path,
            pending_power_on: false,
            thetis_launch_time: None,
            is_tci: false,
        }
    }

    /// Create with TCI backend + auxiliary TCP CAT for commands TCI doesn't support
    pub fn new_tci(tci_addr: Option<&str>, cat_addr: Option<&str>, thetis_path: Option<String>) -> Self {
        // Don't create aux CAT connection yet — wait for TCI cap detection.
        // If _ex caps are available, aux CAT is never needed.
        // If caps are missing (vanilla Thetis), aux CAT is created lazily.
        if cat_addr.is_some() {
            info!("TCI mode: auxiliary CAT address configured (deferred until cap detection)");
        }
        Self {
            state: PttState::Rx,
            last_ptt_packet: None,
            last_heartbeat: None,
            pending_activate: None,
            pending_release: None,
            tail_delay_ms: PTT_TAIL_MIN_MS_TCI,
            ptt_active: Arc::new(AtomicBool::new(false)),
            radio: RadioBackend::Tci(TciConnection::new(tci_addr)),
            aux_cat: None,
            aux_cat_addr: cat_addr.map(|s| s.to_string()),
            thetis_path,
            pending_power_on: false,
            thetis_launch_time: None,
            is_tci: true,
        }
    }

    fn prefill_ms(&self) -> u64 {
        if self.is_tci { PTT_PREFILL_MS_TCI } else { PTT_PREFILL_MS }
    }

    fn tail_min_ms(&self) -> u64 {
        if self.is_tci { PTT_TAIL_MIN_MS_TCI } else { PTT_TAIL_MIN_MS }
    }

    fn tail_margin_ms(&self) -> u64 {
        if self.is_tci { PTT_TAIL_MARGIN_MS_TCI } else { PTT_TAIL_MARGIN_MS }
    }

    pub fn record_ptt_packet(&mut self) {
        let now = Instant::now();
        self.last_ptt_packet = Some(now);
        self.last_heartbeat = Some(now);
    }

    pub fn activate_from_playout(&mut self) {
        if self.pending_release.take().is_some() {
            info!("PTT re-keyed during tail delay, release cancelled");
        }
        if self.state != PttState::Tx && self.pending_activate.is_none() {
            info!("PTT prefill started ({}ms)", self.prefill_ms());
            self.pending_activate = Some(Instant::now());
        }
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

    pub fn release_from_playout(&mut self, jitter_depth: usize) {
        if self.state == PttState::Tx && self.pending_release.is_none() {
            let depth_ms = (jitter_depth as u64) * 20;
            self.tail_delay_ms = (depth_ms + self.tail_margin_ms()).max(self.tail_min_ms());
            info!("PTT release from playout, {}ms tail delay (jitter depth={})", self.tail_delay_ms, jitter_depth);
            self.pending_release = Some(Instant::now());
        }
    }

    pub fn heartbeat_received(&mut self) {
        self.last_heartbeat = Some(Instant::now());
    }

    pub async fn check_safety(&mut self) -> bool {
        let now = Instant::now();

        if let Some(last_hb) = self.last_heartbeat {
            if now.duration_since(last_hb).as_millis() > HEARTBEAT_TIMEOUT_MS as u128 {
                if self.state == PttState::Tx {
                    error!("SAFETY: Heartbeat timeout! Releasing PTT.");
                    self.pending_release = None;
                    self.force_release().await;
                    return true;
                }
            }
        }

        if self.state == PttState::Tx {
            if let Some(last_pkt) = self.last_ptt_packet {
                if now.duration_since(last_pkt).as_millis() > PTT_PACKET_TIMEOUT_MS as u128 {
                    warn!("SAFETY: No PTT packets for 500ms, releasing PTT.");
                    self.pending_release = None;
                    self.force_release().await;
                    return true;
                }
            }
        }

        if let Some(release_time) = self.pending_release {
            if now.duration_since(release_time).as_millis() >= self.tail_delay_ms as u128 {
                self.pending_release = None;
                self.set_state(PttState::Rx).await;
            }
        }

        // Poll radio backend
        match &mut self.radio {
            RadioBackend::Cat(cat) => cat.poll_and_parse().await,
            RadioBackend::Tci(tci) => tci.poll_and_parse().await,
        }

        // Auxiliary TCP CAT — deferred creation, only for vanilla Thetis (no _ex caps).
        // With all _ex caps, aux CAT is never created or polled.
        if self.is_tci {
            let all_ex = match &self.radio {
                RadioBackend::Tci(tci) => tci.has_cap("ctun_ex")
                    && tci.has_cap("vfo_sync_ex")
                    && tci.has_cap("step_attenuator_ex")
                    && tci.has_cap("diversity_ex")
                    && tci.has_cap("fm_deviation_ex"),
                _ => false,
            };

            // Only create aux CAT after TCI init is complete (power_on=true, set after ready;)
            // and _ex caps are confirmed missing. Vanilla Thetis has empty server_caps after ready.
            let tci_init_done = match &self.radio {
                RadioBackend::Tci(tci) => tci.power_on,
                _ => false,
            };
            if !all_ex && tci_init_done && self.aux_cat.is_none() && self.aux_cat_addr.is_some() {
                let addr = self.aux_cat_addr.as_ref().unwrap().clone();
                info!("TCI connected but _ex caps missing — creating auxiliary CAT on {}", addr);
                self.aux_cat = Some(CatConnection::new(Some(&addr)));
            }

            if let Some(ref mut cat) = self.aux_cat {
                if all_ex {
                    if !cat.volume_only_mode {
                        info!("All TCI _ex caps available — auxiliary CAT disabled");
                        cat.volume_only_mode = true;
                    }
                } else {
                    if cat.volume_only_mode {
                        info!("TCI _ex caps lost — restoring full CAT polling");
                        cat.volume_only_mode = false;
                    }
                    cat.poll_and_parse().await;
                    if let RadioBackend::Tci(ref mut tci) = self.radio {
                        tci.rx_af_gain = cat.rx_af_gain;
                        tci.rx2_af_gain = cat.rx2_af_gain;
                        if !tci.has_cap("ctun_ex") { tci.ctun = cat.ctun; }
                        if !tci.has_cap("step_attenuator_ex") {
                            tci.step_att_rx1 = cat.step_att_rx1 as i32;
                            tci.step_att_rx2 = cat.step_att_rx2 as i32;
                        }
                        if !tci.has_cap("vfo_sync_ex") { tci.vfo_sync_on = cat.vfo_sync_on; }
                        tci.nr_level = cat.nr_level;
                        tci.anf_on = cat.anf_on;
                        if tci.vfo_b_freq == 0 && cat.vfo_b_freq != 0 {
                            tci.vfo_b_freq = cat.vfo_b_freq;
                        }
                    }
                }
            }
        } else if let Some(ref mut cat) = self.aux_cat {
            cat.poll_and_parse().await;
        }

        // Auto-launch: if pending_power_on and backend connected
        if self.pending_power_on && self.is_connected() {
            info!("Thetis connected, sending power on");
            self.radio_set_power(true).await;
            self.pending_power_on = false;
            self.thetis_launch_time = None;
        }

        if self.pending_power_on {
            if let Some(launch_time) = self.thetis_launch_time {
                if now.duration_since(launch_time).as_secs() > THETIS_LAUNCH_TIMEOUT_S {
                    warn!("Thetis launch timeout ({}s), cancelling", THETIS_LAUNCH_TIMEOUT_S);
                    self.pending_power_on = false;
                    self.thetis_launch_time = None;
                }
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
        let old = self.state;
        self.state = new_state;
        let is_tx = new_state == PttState::Tx;
        self.ptt_active.store(is_tx, Ordering::Relaxed);

        match &mut self.radio {
            RadioBackend::Cat(cat) => {
                cat.set_tx_active(is_tx);
                let cmd = if is_tx { "ZZTX1;" } else { "ZZTX0;" };
                info!("PTT: {:?} -> {:?} (CAT: {})", old, new_state, cmd);
                cat.send(cmd).await;
            }
            RadioBackend::Tci(tci) => {
                tci.set_tx_active(is_tx);
                // TCI: use ,tci source so Thetis takes audio from TCI stream
                let cmd = if is_tx { "TRX:0,true,tci;" } else { "TRX:0,false;" };
                info!("PTT: {:?} -> {:?} (TCI: {})", old, new_state, cmd);
                tci.send(cmd).await;
            }
        }
    }

    fn is_connected(&self) -> bool {
        match &self.radio {
            RadioBackend::Cat(cat) => cat.is_connected(),
            RadioBackend::Tci(tci) => tci.is_connected(),
        }
    }

    async fn radio_send(&mut self, cmd: &str) {
        match &mut self.radio {
            RadioBackend::Cat(cat) => cat.send(cmd).await,
            RadioBackend::Tci(tci) => tci.send(cmd).await,
        }
    }

    async fn radio_set_power(&mut self, on: bool) {
        match &mut self.radio {
            RadioBackend::Cat(cat) => cat.set_power(on).await,
            RadioBackend::Tci(tci) => tci.set_power(on).await,
        }
    }

    // --- Delegated accessors ---

    /// Query a CAT command and return the response value.
    pub async fn query_cat(&mut self, cmd: &str) -> Option<String> {
        if let Some(ref mut cat) = self.aux_cat {
            cat.query(cmd).await
        } else {
            None
        }
    }

    pub async fn send_cat(&mut self, cmd: &str) {
        // In TCI mode: ZZ* commands → auxiliary TCP CAT or TCI translation
        if self.is_tci {
            if cmd.starts_with("ZZ") {
                // Try auxiliary CAT first (if available)
                if let Some(ref mut cat) = self.aux_cat {
                    cat.send(cmd).await;
                    return;
                }
                // Aux CAT not available — translate known ZZ commands to TCI
                if let Some(tci_cmd) = Self::cat_to_tci(cmd) {
                    log::info!("CAT→TCI: {} → {}", cmd.trim_end_matches(';'), tci_cmd.trim_end_matches(';'));
                    self.radio_send(&tci_cmd).await;
                } else {
                    log::warn!("CAT command dropped (no aux CAT, no TCI translation): {}", cmd.trim_end_matches(';'));
                }
                return;
            } else {
                // TCI command (e.g. TUNE:0,true;) → send via WebSocket
                self.radio_send(cmd).await;
                return;
            }
        }
        self.radio_send(cmd).await;
    }

    /// Translate a ZZ CAT command to a TCI equivalent. Returns None if unknown.
    fn cat_to_tci(cmd: &str) -> Option<String> {
        let cmd = cmd.trim_end_matches(';');
        // ZZFA00007073000 → vfo:0,0,7073000 (VFO A freq, 11 digits)
        if cmd.starts_with("ZZFA") && cmd.len() >= 15 {
            if let Ok(hz) = cmd[4..15].parse::<u64>() {
                return Some(format!("vfo:0,0,{};", hz));
            }
        }
        // ZZFB00007073000 → vfo:1,0,7073000 (VFO B freq)
        if cmd.starts_with("ZZFB") && cmd.len() >= 15 {
            if let Ok(hz) = cmd[4..15].parse::<u64>() {
                return Some(format!("vfo:1,0,{};", hz));
            }
        }
        // ZZMD00 → modulation:0,LSB (mode VFO A, 2 digit mode number)
        if cmd.starts_with("ZZMD") && cmd.len() >= 6 {
            if let Ok(mode_num) = cmd[4..6].parse::<u8>() {
                let mode_name = cat_mode_to_tci(mode_num);
                return Some(format!("modulation:0,{};", mode_name));
            }
        }
        // ZZME00 → modulation:1,LSB (mode VFO B)
        if cmd.starts_with("ZZME") && cmd.len() >= 6 {
            if let Ok(mode_num) = cmd[4..6].parse::<u8>() {
                let mode_name = cat_mode_to_tci(mode_num);
                return Some(format!("modulation:1,{};", mode_name));
            }
        }
        // ZZTU1/ZZTU0 → tune:0,true/false
        if cmd == "ZZTU1" { return Some("tune:0,true;".to_string()); }
        if cmd == "ZZTU0" { return Some("tune:0,false;".to_string()); }
        // ZZTX1/ZZTX0 → trx:0,true/false
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
        if let RadioBackend::Tci(ref mut tci) = self.radio {
            tci.send_spot(callsign, mode, freq_hz, color, text).await;
        }
    }

    pub fn vfo_a_freq(&self) -> u64 {
        match &self.radio {
            RadioBackend::Cat(c) => c.vfo_a_freq,
            RadioBackend::Tci(t) => t.vfo_a_freq,
        }
    }

    pub fn vfo_a_mode(&self) -> u8 {
        match &self.radio {
            RadioBackend::Cat(c) => c.vfo_a_mode,
            RadioBackend::Tci(t) => t.vfo_a_mode,
        }
    }

    pub fn smeter_avg(&self) -> u16 {
        match &self.radio {
            RadioBackend::Cat(c) => c.smeter_avg(),
            RadioBackend::Tci(t) => t.smeter_avg(),
        }
    }

    pub fn power_on(&self) -> bool {
        match &self.radio {
            RadioBackend::Cat(c) => c.power_on,
            RadioBackend::Tci(t) => t.power_on,
        }
    }

    pub fn tx_profile(&self) -> u8 {
        match &self.radio {
            RadioBackend::Cat(c) => c.tx_profile,
            RadioBackend::Tci(t) => t.tx_profile,
        }
    }

    pub fn nr_level(&self) -> u8 {
        match &self.radio {
            RadioBackend::Cat(c) => c.nr_level,
            RadioBackend::Tci(t) => t.nr_level,
        }
    }

    pub fn anf_on(&self) -> bool {
        match &self.radio {
            RadioBackend::Cat(c) => c.anf_on,
            RadioBackend::Tci(t) => t.anf_on,
        }
    }

    pub fn drive_level(&self) -> u8 {
        match &self.radio {
            RadioBackend::Cat(c) => c.drive_level,
            RadioBackend::Tci(t) => t.drive_level,
        }
    }

    pub fn rx_af_gain(&self) -> u8 {
        match &self.radio {
            RadioBackend::Cat(c) => c.rx_af_gain,
            RadioBackend::Tci(t) => t.rx_af_gain,
        }
    }

    pub fn set_rx_af_gain(&mut self, val: u8) {
        match &mut self.radio {
            RadioBackend::Cat(c) => c.rx_af_gain = val,
            RadioBackend::Tci(t) => t.rx_af_gain = val,
        }
    }

    pub fn filter_low_hz(&self) -> i32 {
        match &self.radio {
            RadioBackend::Cat(c) => c.filter_low_hz,
            RadioBackend::Tci(t) => t.filter_low_hz,
        }
    }

    pub fn filter_high_hz(&self) -> i32 {
        match &self.radio {
            RadioBackend::Cat(c) => c.filter_high_hz,
            RadioBackend::Tci(t) => t.filter_high_hz,
        }
    }

    pub fn ctun(&self) -> bool {
        match &self.radio {
            RadioBackend::Cat(c) => c.ctun,
            RadioBackend::Tci(t) => t.ctun,
        }
    }

    pub fn is_transmitting(&self) -> bool {
        self.state == PttState::Tx
    }

    pub fn fwd_power_raw(&self) -> u16 {
        match &self.radio {
            RadioBackend::Cat(c) => c.fwd_power_raw(),
            RadioBackend::Tci(t) => t.fwd_power_raw(),
        }
    }

    /// SWR × 100 (e.g. 150 = 1.50:1). Returns 100 when not transmitting.
    pub fn swr_x100(&self) -> u16 {
        match &self.radio {
            RadioBackend::Tci(t) => (t.swr * 100.0).round() as u16,
            _ => 100,
        }
    }

    pub async fn set_vfo_a_freq(&mut self, hz: u64) {
        match &mut self.radio {
            RadioBackend::Cat(c) => c.set_vfo_a_freq(hz).await,
            RadioBackend::Tci(t) => t.set_vfo_a_freq(hz).await,
        }
    }

    pub async fn set_vfo_a_mode(&mut self, mode: u8) {
        match &mut self.radio {
            RadioBackend::Cat(c) => c.set_vfo_a_mode(mode).await,
            RadioBackend::Tci(t) => t.set_vfo_a_mode(mode).await,
        }
    }

    pub async fn set_power(&mut self, on: bool) {
        info!("set_power({}) called — connected={}, thetis_path={:?}", on, self.is_connected(), self.thetis_path.is_some());
        if !on {
            self.pending_power_on = false;
            self.thetis_launch_time = None;
            if self.is_connected() {
                self.radio_set_power(false).await;
            }
            return;
        }

        if self.is_connected() {
            info!("Already connected, sending ZZPS1 directly");
            self.radio_set_power(true).await;
            return;
        }

        // Not connected: try auto-launch
        if self.thetis_path.is_none() {
            info!("No thetis_path configured, sending ZZPS1 anyway (will fail if not connected)");
            self.radio_set_power(true).await;
            return;
        }

        if self.pending_power_on {
            info!("Thetis launch already pending, ignoring duplicate POWER ON");
            return;
        }

        if !is_process_running("Thetis.exe") {
            let path = self.thetis_path.as_ref().unwrap();
            info!("Launching Thetis: {}", path);
            if shell_execute_open(path) {
                info!("Thetis.exe launch initiated");
            } else {
                error!("Failed to start Thetis via ShellExecute");
                return;
            }
        } else {
            info!("Thetis already running, waiting for connection");
        }

        self.pending_power_on = true;
        self.thetis_launch_time = Some(Instant::now());
    }

    pub fn thetis_starting(&self) -> bool {
        self.pending_power_on
    }

    pub async fn set_tx_profile(&mut self, idx: u8) {
        match &mut self.radio {
            RadioBackend::Cat(c) => c.set_tx_profile(idx).await,
            RadioBackend::Tci(t) => t.set_tx_profile(idx).await,
        }
    }

    pub async fn set_nr(&mut self, level: u8) {
        match &mut self.radio {
            RadioBackend::Cat(c) => c.set_nr(level).await,
            RadioBackend::Tci(t) => t.set_nr(level).await,
        }
    }

    pub async fn set_anf(&mut self, on: bool) {
        match &mut self.radio {
            RadioBackend::Cat(c) => c.set_anf(on).await,
            RadioBackend::Tci(t) => t.set_anf(on).await,
        }
    }

    pub async fn set_drive(&mut self, level: u8) {
        match &mut self.radio {
            RadioBackend::Cat(c) => c.set_drive(level).await,
            RadioBackend::Tci(t) => t.set_drive(level).await,
        }
    }

    pub async fn set_filter(&mut self, low_hz: i32, high_hz: i32) {
        match &mut self.radio {
            RadioBackend::Cat(c) => c.set_filter(low_hz, high_hz).await,
            RadioBackend::Tci(t) => t.set_filter(low_hz, high_hz).await,
        }
    }

    // --- RX2 / VFO-B ---

    pub fn vfo_b_freq(&self) -> u64 {
        match &self.radio {
            RadioBackend::Cat(c) => c.vfo_b_freq,
            RadioBackend::Tci(t) => t.vfo_b_freq,
        }
    }

    pub fn vfo_b_mode(&self) -> u8 {
        match &self.radio {
            RadioBackend::Cat(c) => c.vfo_b_mode,
            RadioBackend::Tci(t) => t.vfo_b_mode,
        }
    }

    pub fn smeter_rx2_avg(&self) -> u16 {
        match &self.radio {
            RadioBackend::Cat(c) => c.smeter_rx2_avg(),
            RadioBackend::Tci(t) => t.smeter_rx2_avg(),
        }
    }

    pub fn rx2_af_gain(&self) -> u8 {
        match &self.radio {
            RadioBackend::Cat(c) => c.rx2_af_gain,
            RadioBackend::Tci(t) => t.rx2_af_gain,
        }
    }

    pub fn filter_rx2_low_hz(&self) -> i32 {
        match &self.radio {
            RadioBackend::Cat(c) => c.filter_rx2_low_hz,
            RadioBackend::Tci(t) => t.filter_rx2_low_hz,
        }
    }

    pub fn filter_rx2_high_hz(&self) -> i32 {
        match &self.radio {
            RadioBackend::Cat(c) => c.filter_rx2_high_hz,
            RadioBackend::Tci(t) => t.filter_rx2_high_hz,
        }
    }

    pub fn rx2_nr_level(&self) -> u8 {
        match &self.radio {
            RadioBackend::Cat(c) => c.rx2_nr_level,
            RadioBackend::Tci(t) => t.rx2_nr_level,
        }
    }

    pub fn rx2_anf_on(&self) -> bool {
        match &self.radio {
            RadioBackend::Cat(c) => c.rx2_anf_on,
            RadioBackend::Tci(t) => t.rx2_anf_on,
        }
    }

    pub fn tx_profile_names(&self) -> &[String] {
        match &self.radio {
            RadioBackend::Cat(_) => &[],
            RadioBackend::Tci(t) => &t.tx_profile_names,
        }
    }

    pub fn tx_profile_name(&self) -> &str {
        match &self.radio {
            RadioBackend::Cat(_) => "",
            RadioBackend::Tci(t) => &t.tx_profile_name,
        }
    }

    pub fn mon_on(&self) -> bool {
        match &self.radio {
            RadioBackend::Cat(c) => c.mon_on,
            RadioBackend::Tci(t) => t.mon_on,
        }
    }

    // New TCI state getters (v2.10.3.13) — TCI-only, return defaults for CAT
    pub fn agc_mode(&self) -> u8 {
        match &self.radio { RadioBackend::Tci(t) => t.agc_mode, _ => 3 }
    }
    pub fn agc_gain(&self) -> u8 {
        match &self.radio { RadioBackend::Tci(t) => t.agc_gain, _ => 80 }
    }
    pub fn rit_enable(&self) -> bool {
        match &self.radio { RadioBackend::Tci(t) => t.rit_enable, _ => false }
    }
    pub fn rit_offset(&self) -> i32 {
        match &self.radio { RadioBackend::Tci(t) => t.rit_offset, _ => 0 }
    }
    pub fn xit_enable(&self) -> bool {
        match &self.radio { RadioBackend::Tci(t) => t.xit_enable, _ => false }
    }
    pub fn xit_offset(&self) -> i32 {
        match &self.radio { RadioBackend::Tci(t) => t.xit_offset, _ => 0 }
    }
    pub fn sql_enable(&self) -> bool {
        match &self.radio { RadioBackend::Tci(t) => t.sql_enable, _ => false }
    }
    pub fn sql_level(&self) -> u8 {
        match &self.radio { RadioBackend::Tci(t) => t.sql_level, _ => 0 }
    }
    pub fn nb_enable(&self) -> bool {
        match &self.radio { RadioBackend::Tci(t) => t.nb_enable, _ => false }
    }
    pub async fn diversity_smartnull(&mut self, params: &[f32]) {
        if let RadioBackend::Tci(t) = &mut self.radio {
            t.diversity_auto_done = None;
            let args: Vec<String> = params.iter().map(|v| format!("{:.2}", v)).collect();
            let cmd = format!("diversity_smartnull_ex:{};", args.join(","));
            t.send(&cmd).await;
        }
    }
    pub async fn diversity_ultranull(&mut self, params: &[f32]) {
        if let RadioBackend::Tci(t) = &mut self.radio {
            t.diversity_auto_done = None;
            let args: Vec<String> = params.iter().map(|v| format!("{:.2}", v)).collect();
            let cmd = format!("diversity_ultranull_ex:{};", args.join(","));
            t.send(&cmd).await;
        }
    }
    pub async fn diversity_fastsweep(&mut self, start: f32, end: f32, step: f32, settle_ms: u32, meter: u32) {
        if let RadioBackend::Tci(t) = &mut self.radio {
            let cmd = format!("diversity_fastsweep_ex:phase,{:.2},{:.2},{:.2},{},{};", start, end, step, settle_ms, meter);
            t.send(&cmd).await;
        }
    }
    pub async fn diversity_autonull(&mut self, settle_ms: u32, steps: &[(Vec<f32>, bool)]) {
        if let RadioBackend::Tci(t) = &mut self.radio {
            t.diversity_auto_done = None; // clear previous result
            t.diversity_autonull(settle_ms, steps).await;
        }
    }
    /// Returns improvement × 10 + 32000 as u16 when done, 0 when not done.
    pub fn diversity_autonull_done(&self) -> u16 {
        match &self.radio {
            RadioBackend::Tci(t) => {
                if let Some((_, _, improvement)) = t.diversity_auto_done {
                    ((improvement * 10.0).clamp(-320.0, 320.0) as i16 as u16).wrapping_add(32000)
                } else { 0 }
            }
            _ => 0,
        }
    }
    pub fn diversity_phase(&self) -> i32 {
        match &self.radio {
            RadioBackend::Tci(t) => t.diversity_phase,
            _ => 0,
        }
    }
    pub fn diversity_gain(&self, rx: usize) -> u16 {
        match &self.radio {
            RadioBackend::Tci(t) => if rx == 0 { t.diversity_gain_rx1 } else { t.diversity_gain_rx2 },
            _ => 1000,
        }
    }
    pub fn diversity_enabled(&self) -> bool {
        match &self.radio {
            RadioBackend::Tci(t) => t.diversity_enabled,
            _ => false,
        }
    }
    pub fn agc_auto(&self, rx: usize) -> bool {
        match &self.radio {
            RadioBackend::Tci(t) => if rx == 0 { t.agc_auto_rx1 } else { t.agc_auto_rx2 },
            _ => false,
        }
    }
    pub async fn set_agc_auto(&mut self, rx: u32, enabled: bool) {
        if let RadioBackend::Tci(t) = &mut self.radio {
            if t.has_cap("agc_auto_ex") {
                let cmd = format!("agc_auto_ex:{},{};", rx, enabled);
                t.send(&cmd).await;
                if rx == 0 { t.agc_auto_rx1 = enabled; }
                else { t.agc_auto_rx2 = enabled; }
            }
        }
    }
    pub fn nb_level(&self) -> u8 {
        match &self.radio { RadioBackend::Tci(t) => t.nb_level, _ => 0 }
    }
    pub fn cw_keyer_speed(&self) -> u8 {
        match &self.radio { RadioBackend::Tci(t) => t.cw_keyer_speed, _ => 20 }
    }
    pub fn vfo_lock(&self) -> bool {
        match &self.radio { RadioBackend::Tci(t) => t.vfo_lock, _ => false }
    }
    pub fn binaural(&self) -> bool {
        match &self.radio { RadioBackend::Tci(t) => t.binaural, _ => false }
    }
    pub fn apf_enable(&self) -> bool {
        match &self.radio { RadioBackend::Tci(t) => t.apf_enable, _ => false }
    }

    // RX2 TCI state getters
    pub fn rx2_agc_mode(&self) -> u8 {
        match &self.radio { RadioBackend::Tci(t) => t.rx2_agc_mode, _ => 3 }
    }
    pub fn rx2_agc_gain(&self) -> u8 {
        match &self.radio { RadioBackend::Tci(t) => t.rx2_agc_gain, _ => 80 }
    }
    pub fn rx2_sql_enable(&self) -> bool {
        match &self.radio { RadioBackend::Tci(t) => t.rx2_sql_enable, _ => false }
    }
    pub fn rx2_sql_level(&self) -> u8 {
        match &self.radio { RadioBackend::Tci(t) => t.rx2_sql_level, _ => 0 }
    }
    pub fn rx2_nb_enable(&self) -> bool {
        match &self.radio { RadioBackend::Tci(t) => t.rx2_nb_enable, _ => false }
    }
    pub fn rx2_binaural(&self) -> bool {
        match &self.radio { RadioBackend::Tci(t) => t.rx2_binaural, _ => false }
    }
    pub fn rx2_apf_enable(&self) -> bool {
        match &self.radio { RadioBackend::Tci(t) => t.rx2_apf_enable, _ => false }
    }
    pub fn rx2_vfo_lock(&self) -> bool {
        match &self.radio { RadioBackend::Tci(t) => t.rx2_vfo_lock, _ => false }
    }
    pub fn mute(&self) -> bool {
        match &self.radio { RadioBackend::Tci(t) => t.mute, _ => false }
    }
    pub fn rx_mute(&self) -> bool {
        match &self.radio { RadioBackend::Tci(t) => t.rx_mute, _ => false }
    }
    pub fn nf_enable(&self) -> bool {
        match &self.radio { RadioBackend::Tci(t) => t.nf_enable, _ => false }
    }
    pub fn rx2_nf_enable(&self) -> bool {
        match &self.radio { RadioBackend::Tci(t) => t.rx2_nf_enable, _ => false }
    }
    pub fn rx_balance(&self) -> i8 {
        match &self.radio { RadioBackend::Tci(t) => t.rx_balance, _ => 0 }
    }

    pub fn vfo_sync_on(&self) -> bool {
        match &self.radio {
            RadioBackend::Cat(c) => c.vfo_sync_on,
            RadioBackend::Tci(t) => t.vfo_sync_on,
        }
    }

    pub async fn set_mon(&mut self, on: bool) {
        match &mut self.radio {
            RadioBackend::Cat(c) => c.set_mon(on).await,
            RadioBackend::Tci(t) => t.set_mon(on).await,
        }
    }

    pub async fn set_vfo_sync_thetis(&mut self, on: bool) {
        if self.is_tci {
            // Prefer TCI _ex command if available, fallback to auxiliary CAT
            if let RadioBackend::Tci(ref mut tci) = self.radio {
                if tci.has_cap("vfo_sync_ex") {
                    tci.set_vfo_sync(on).await;
                    return;
                }
            }
            let cmd = if on { "ZZSY1;" } else { "ZZSY0;" };
            if let Some(ref mut cat) = self.aux_cat {
                info!("VFO Sync via aux CAT: {}", cmd);
                cat.send(cmd).await;
            }
        } else {
            match &mut self.radio {
                RadioBackend::Cat(c) => c.set_vfo_sync(on).await,
                RadioBackend::Tci(_) => {}
            }
        }
    }

    // ── Diversity dispatch (TCI _ex with CAT fallback) ────────────────

    pub async fn set_diversity_enable(&mut self, enabled: bool) {
        if let RadioBackend::Tci(ref mut tci) = self.radio {
            if tci.has_cap("diversity_ex") {
                tci.set_diversity_enable(enabled).await;
                return;
            }
        }
        let cmd = format!("ZZDE{};", if enabled { 1 } else { 0 });
        self.send_cat(&cmd).await;
    }

    pub async fn set_diversity_ref(&mut self, rx1_ref: bool) {
        if let RadioBackend::Tci(ref mut tci) = self.radio {
            if tci.has_cap("diversity_ex") {
                tci.set_diversity_ref(rx1_ref).await;
                return;
            }
        }
        let cmd = format!("ZZDB{};", if rx1_ref { 0 } else { 1 });
        self.send_cat(&cmd).await;
    }

    pub async fn set_diversity_source(&mut self, source: u32) {
        if let RadioBackend::Tci(ref mut tci) = self.radio {
            if tci.has_cap("diversity_ex") {
                tci.set_diversity_source(source).await;
                return;
            }
        }
        let cmd = format!("ZZDH{};", source);
        self.send_cat(&cmd).await;
    }

    pub async fn set_diversity_gain(&mut self, rx: u32, gain: u16) {
        if let RadioBackend::Tci(ref mut tci) = self.radio {
            if tci.has_cap("diversity_ex") {
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
        if let RadioBackend::Tci(ref mut tci) = self.radio {
            if tci.has_cap("diversity_ex") {
                tci.set_diversity_phase(phase).await;
                return;
            }
        }
        let sign = if phase >= 0 { "+" } else { "-" };
        let cmd = format!("ZZDD{}{:05};", sign, phase.abs());
        self.send_cat(&cmd).await;
    }

    pub async fn set_vfo_b_freq(&mut self, hz: u64) {
        match &mut self.radio {
            RadioBackend::Cat(c) => c.set_vfo_b_freq(hz).await,
            RadioBackend::Tci(t) => t.set_vfo_b_freq(hz).await,
        }
    }

    pub async fn set_vfo_b_mode(&mut self, mode: u8) {
        match &mut self.radio {
            RadioBackend::Cat(c) => c.set_vfo_b_mode(mode).await,
            RadioBackend::Tci(t) => t.set_vfo_b_mode(mode).await,
        }
    }

    pub async fn vfo_swap(&mut self) {
        match &mut self.radio {
            RadioBackend::Cat(c) => c.vfo_swap().await,
            RadioBackend::Tci(t) => t.vfo_swap().await,
        }
    }

    pub async fn set_rx2_af_gain(&mut self, level: u8) {
        match &mut self.radio {
            RadioBackend::Cat(c) => c.set_rx2_af_gain(level).await,
            RadioBackend::Tci(t) => t.set_rx2_af_gain(level).await,
        }
    }

    pub async fn set_rx2_nr(&mut self, level: u8) {
        match &mut self.radio {
            RadioBackend::Cat(c) => c.set_rx2_nr(level).await,
            RadioBackend::Tci(t) => t.set_rx2_nr(level).await,
        }
    }

    pub async fn set_rx2_anf(&mut self, on: bool) {
        match &mut self.radio {
            RadioBackend::Cat(c) => c.set_rx2_anf(on).await,
            RadioBackend::Tci(t) => t.set_rx2_anf(on).await,
        }
    }

    pub async fn set_rx2_filter(&mut self, low_hz: i32, high_hz: i32) {
        match &mut self.radio {
            RadioBackend::Cat(c) => c.set_rx2_filter(low_hz, high_hz).await,
            RadioBackend::Tci(t) => t.set_rx2_filter(low_hz, high_hz).await,
        }
    }

    // --- New TCI controls (v2.10.3.13) ---

    pub async fn set_agc_mode(&mut self, mode: u8) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_agc_mode(mode).await; }
    }
    pub async fn set_agc_gain(&mut self, gain: u8) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_agc_gain(gain).await; }
    }
    pub async fn set_rit_enable(&mut self, on: bool) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_rit_enable(on).await; }
    }
    pub async fn set_rit_offset(&mut self, hz: i32) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_rit_offset(hz).await; }
    }
    pub async fn set_xit_enable(&mut self, on: bool) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_xit_enable(on).await; }
    }
    pub async fn set_xit_offset(&mut self, hz: i32) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_xit_offset(hz).await; }
    }
    pub async fn set_sql_enable(&mut self, on: bool) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_sql_enable(on).await; }
    }
    pub async fn set_sql_level(&mut self, level: i16) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_sql_level(level).await; }
    }
    pub async fn set_nb_enable(&mut self, on: bool) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_nb_enable(on).await; }
    }
    pub async fn set_nb(&mut self, level: u8) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_nb(level).await; }
    }
    pub async fn set_cw_keyer_speed(&mut self, wpm: u8) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_cw_keyer_speed(wpm).await; }
    }
    pub async fn cw_key(&mut self, pressed: bool, duration_ms: Option<u16>) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.cw_key(pressed, duration_ms).await; }
    }
    pub async fn cw_macro_stop(&mut self) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.cw_macro_stop().await; }
    }
    pub async fn set_vfo_lock(&mut self, on: bool) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_vfo_lock(on).await; }
    }
    pub async fn set_binaural(&mut self, on: bool) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_binaural(on).await; }
    }
    pub async fn set_apf_enable(&mut self, on: bool) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_apf_enable(on).await; }
    }

    // RX2 TCI control setters
    pub async fn set_rx2_agc_mode(&mut self, mode: u8) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_rx2_agc_mode(mode).await; }
    }
    pub async fn set_rx2_agc_gain(&mut self, gain: u8) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_rx2_agc_gain(gain).await; }
    }
    pub async fn set_rx2_sql_enable(&mut self, on: bool) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_rx2_sql_enable(on).await; }
    }
    pub async fn set_rx2_sql_level(&mut self, level: i16) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_rx2_sql_level(level).await; }
    }
    pub async fn set_rx2_nb_enable(&mut self, on: bool) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_rx2_nb_enable(on).await; }
    }
    pub async fn set_rx2_nb(&mut self, level: u8) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_rx2_nb(level).await; }
    }
    pub async fn set_rx2_binaural(&mut self, on: bool) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_rx2_binaural(on).await; }
    }
    pub async fn set_rx2_apf_enable(&mut self, on: bool) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_rx2_apf_enable(on).await; }
    }
    pub async fn set_rx2_vfo_lock(&mut self, on: bool) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_rx2_vfo_lock(on).await; }
    }
    pub async fn set_mute(&mut self, on: bool) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_mute(on).await; }
    }
    pub async fn set_rx_mute(&mut self, on: bool) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_rx_mute(on).await; }
    }
    pub async fn set_nf_enable(&mut self, on: bool) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_nf_enable(on).await; }
    }
    pub async fn set_rx2_nf_enable(&mut self, on: bool) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_rx2_nf_enable(on).await; }
    }
    pub async fn set_rx_balance(&mut self, value: i8) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_rx_balance(value).await; }
    }

    // Tune, tune drive, monitor volume
    pub fn tune_active(&self) -> bool {
        match &self.radio { RadioBackend::Tci(t) => t.tune_active, _ => false }
    }
    pub fn tune_drive(&self) -> u8 {
        match &self.radio { RadioBackend::Tci(t) => t.tune_drive, _ => 0 }
    }
    pub fn mon_volume(&self) -> i8 {
        match &self.radio { RadioBackend::Tci(t) => t.mon_volume, _ => -40 }
    }
    pub async fn set_tune(&mut self, on: bool) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_tune(on).await; }
    }
    pub async fn set_tune_drive(&mut self, level: u8) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_tune_drive(level).await; }
    }
    pub async fn set_mon_volume(&mut self, db: i8) {
        if let RadioBackend::Tci(t) = &mut self.radio { t.set_mon_volume(db).await; }
    }

    /// Check which connections need to be established (brief, no I/O).
    /// Returns (tci_url, cat_addr, aux_cat_addr).
    pub fn needed_connections(&mut self) -> (Option<String>, Option<String>, Option<String>) {
        let tci_url = match &mut self.radio {
            RadioBackend::Tci(tci) => tci.needs_connect_info(),
            RadioBackend::Cat(_) => None,
        };
        let cat_addr = match &mut self.radio {
            RadioBackend::Cat(cat) => cat.needs_connect(),
            RadioBackend::Tci(_) => None,
        };
        // Skip aux CAT connection when TCI _ex covers everything.
        // Check both the cached flag AND live caps to prevent initial connect before caps arrive.
        let all_ex_live = match &self.radio {
            RadioBackend::Tci(tci) => tci.has_cap("ctun_ex")
                && tci.has_cap("vfo_sync_ex")
                && tci.has_cap("step_attenuator_ex")
                && tci.has_cap("diversity_ex")
                && tci.has_cap("fm_deviation_ex"),
            _ => false,
        };
        let aux_addr = if let Some(ref mut cat) = self.aux_cat {
            if cat.volume_only_mode || all_ex_live {
                None
            } else {
                cat.needs_connect()
            }
        } else {
            None
        };
        (tci_url, cat_addr, aux_addr)
    }

    /// Accept established connections from the background connector.
    pub fn accept_connections(
        &mut self,
        tci_stream: Option<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        >,
        cat_stream: Option<tokio::net::TcpStream>,
        aux_stream: Option<tokio::net::TcpStream>,
    ) {
        if let Some(stream) = tci_stream {
            if let RadioBackend::Tci(ref mut tci) = self.radio {
                tci.accept_stream(stream);
            }
        }
        if let Some(stream) = cat_stream {
            if let RadioBackend::Cat(ref mut cat) = self.radio {
                cat.accept_stream(stream);
            }
        }
        if let Some(stream) = aux_stream {
            if let Some(ref mut cat) = self.aux_cat {
                cat.accept_stream(stream);
            }
        }
    }

    /// Whether using TCI backend
    pub fn is_tci_mode(&self) -> bool {
        self.is_tci
    }

    pub async fn set_ddc_sample_rate(&mut self, rx: u32, rate: u32) {
        match &mut self.radio {
            RadioBackend::Tci(t) => t.set_ddc_sample_rate(rx, rate).await,
            _ => {}
        }
    }

    /// DDC sample rate per receiver in Hz (0=unknown)
    pub fn ddc_sample_rate(&self, rx: usize) -> u32 {
        match &self.radio {
            RadioBackend::Tci(t) => if rx == 0 { t.ddc_sample_rate_rx1 } else { t.ddc_sample_rate_rx2 },
            _ => 0,
        }
    }

    /// Step attenuator value per receiver (0=RX1, 1=RX2). Negative dB from TCI _ex.
    pub fn step_att(&self, rx: usize) -> i32 {
        match &self.radio {
            RadioBackend::Tci(t) => if rx == 0 { t.step_att_rx1 } else { t.step_att_rx2 },
            RadioBackend::Cat(c) => if rx == 0 { c.step_att_rx1 as i32 } else { c.step_att_rx2 as i32 },
        }
    }

    /// Check if the connected TCI server advertises a capability
    pub fn has_tci_cap(&self, cap: &str) -> bool {
        match &self.radio {
            RadioBackend::Tci(t) => t.has_cap(cap),
            _ => false,
        }
    }

    /// Borrow the TCI connection (if in TCI mode)
    pub fn tci_ref(&self) -> Option<&crate::tci::TciConnection> {
        match &self.radio {
            RadioBackend::Tci(t) => Some(t),
            _ => None,
        }
    }
    /// Check if Thetis advertises a TCI capability
    pub fn has_cap(&self, cap: &str) -> bool {
        match &self.radio {
            RadioBackend::Tci(t) => t.has_cap(cap),
            _ => false,
        }
    }

    /// TCI DDS center frequency per receiver (0=RX1, 1=RX2). Returns 0 if not in TCI mode.
    pub fn dds_freq(&self, receiver: usize) -> u64 {
        match &self.radio {
            RadioBackend::Tci(t) => t.dds_freq[receiver.min(1)],
            _ => 0,
        }
    }

    /// Static calibration offset (dB) from TCI calibration_ex.
    /// This is meter_cal + xvtr_gain + 6m_gain — everything except step ATT.
    pub fn static_cal_offset(&self, receiver: usize) -> f32 {
        match &self.radio {
            RadioBackend::Tci(t) => {
                let idx = receiver.min(1);
                t.meter_cal_offset[idx] + t.xvtr_gain_offset[idx] + t.six_m_gain_offset[idx]
            }
            _ => 0.0,
        }
    }

    /// Raw TCI S-meter dBm (peakBinDbm) for auto-calibration.
    pub fn smeter_raw_dbm(&self, receiver: usize) -> Option<f32> {
        match &self.radio {
            RadioBackend::Tci(t) => t.smeter_raw_dbm[receiver.min(1)],
            _ => None,
        }
    }

    /// Write TX audio to TCI ring buffer (only in TCI mode, no-op for CAT)
    pub fn write_tx_audio(&mut self, samples: &[f32]) {
        if let RadioBackend::Tci(ref mut tci) = self.radio {
            tci.write_tx_audio(samples);
        }
    }
}
