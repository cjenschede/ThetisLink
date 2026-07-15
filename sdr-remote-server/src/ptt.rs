// SPDX-License-Identifier: GPL-2.0-or-later

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    /// TCI WebSocket connection - TL2 v2 is TCI-only, no CAT backend.
    pub tci: TciConnection,
    thetis_path: Option<String>,
    pending_power_on: bool,
    thetis_launch_time: Option<Instant>,
    /// Hard TX-gate. `true` = de huidige Amplitec-A positie is in
    /// `config.amplitec_tx_blocked` als RX-only gemarkeerd; alle
    /// server-initiated TX-paden weigeren in dat geval een TX-commando
    /// naar Thetis te sturen. Updated vanuit de broadcast-loop in
    /// `network.rs` op elke positie-status iteratie (en direct bij
    /// positie-wissel via `update_tx_blocked()`).
    tx_blocked: Arc<AtomicBool>,
    /// Throttle voor de WARN-log wanneer een PTT-request wordt
    /// geweigerd - anders krijgt iedere audio-frame (50/sec) een log.
    last_blocked_log: Option<Instant>,
    // --- Latency metrics ---
    ptt_prefill_start: Option<Instant>,
    ptt_release_start: Option<Instant>,
    ptt_prefill_latencies: Vec<u64>,
    ptt_tail_latencies: Vec<u64>,
}

impl PttController {
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
            tx_blocked: Arc::new(AtomicBool::new(false)),
            last_blocked_log: None,
            ptt_prefill_start: None,
            ptt_release_start: None,
            ptt_prefill_latencies: Vec::new(),
            ptt_tail_latencies: Vec::new(),
        };
        // Share dezelfde tx_blocked Arc met de TCI-connection, zodat de
        // tci-notification handler óók Thetis-direct PTT kan blokkeren.
        // (Zonder dit: server-initiated paden zijn dicht, maar spatiebalk
        // op Thetis-PC wordt pas via de 200 ms broadcast catch-all
        // afgevangen - te laat voor RX-amp bescherming.)
        let blocked_clone = ctl.tx_blocked.clone();
        ctl.tci.set_tx_blocked_handle(blocked_clone);
        ctl
    }

    /// Handle naar de TX-blocked flag. Gebruikt door `network.rs` om
    /// vanuit de broadcast-loop de actuele Amplitec-status door te
    /// pushen. Lock-vrije atomic; alle TX-paden in `PttController`
    /// lezen 'm uit op de hot-path.
    pub fn tx_blocked_handle(&self) -> Arc<AtomicBool> {
        self.tx_blocked.clone()
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

    pub fn record_ptt_packet(&mut self) {
        let now = Instant::now();
        self.last_ptt_packet = Some(now);
        self.last_heartbeat = Some(now);
    }

    pub fn activate_from_playout(&mut self) {
        // Hard gate: bij een RX-only Amplitec-positie mag er onder
        // geen enkele omstandigheid een TX-commando naar Thetis gaan.
        // Drop de PTT-request volledig (geen prefill, geen TRX:0,true;)
        // en log throttled (max 1× per 2 sec) zodat de audio-frame
        // stream (50/sec) geen log-spam veroorzaakt.
        if self.tx_blocked.load(Ordering::Relaxed) {
            let should_log = self
                .last_blocked_log
                .map(|t| t.elapsed() >= Duration::from_secs(2))
                .unwrap_or(true);
            if should_log {
                warn!("PTT request blocked: current Amplitec position is RX-only");
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
            let now = Instant::now();
            let depth_ms = (jitter_depth as u64) * 20;
            self.tail_delay_ms = (depth_ms + self.tail_margin_ms()).max(self.tail_min_ms());
            info!("PTT release from playout, {}ms tail delay (jitter depth={})", self.tail_delay_ms, jitter_depth);
            self.pending_release = Some(now);
            self.ptt_release_start = Some(now);
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
        self.tci.poll_and_parse().await;

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
        // Hard gate: laatste defensieve check vóór TRX:0,true,tci de
        // tci-link op gaat. `activate_from_playout` blokkeert al
        // preventief, maar een toekomstige direct-naar-Tx caller komt
        // hier ook door. Bij blocked: blijf op Rx en stuur niets.
        if new_state == PttState::Tx && self.tx_blocked.load(Ordering::Relaxed) {
            warn!("set_state(Tx) blocked: current Amplitec position is RX-only");
            // Zorg dat we Rx zijn (was vermoedelijk al, want
            // activate_from_playout zou ons hebben tegengehouden).
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
                info!("PTT metrics: prefill->TRX = {}ms (server-side budget)", ms);
                self.ptt_prefill_latencies.push(ms);
            }
        } else if let Some(start) = self.ptt_release_start.take() {
            let ms = start.elapsed().as_millis() as u64;
            info!("PTT metrics: tail latency = {}ms", ms);
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
                info!("PTT metrics ({} cycles): prefill p50={}ms p95={}ms max={}ms, tail p50={}ms p95={}ms max={}ms",
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
    }

    fn is_connected(&self) -> bool {
        self.tci.is_connected()
    }

    /// Public accessor for HeartbeatAck `TCI_CONNECTED` flag.
    /// (PATCH-1 client-connect-error-feedback)
    pub fn tci_connected(&self) -> bool {
        self.tci.is_connected()
    }

    async fn radio_send(&mut self, cmd: &str) {
        self.tci.send(cmd).await
    }

    async fn radio_set_power(&mut self, on: bool) {
        self.tci.set_power(on).await
    }

    // --- Delegated accessors ---

    pub async fn send_cat(&mut self, cmd: &str) {
        // Hard gate voor TX-startende CAT-commando's wanneer de actuele
        // Amplitec-A positie als RX-only is gemarkeerd. ZZTX1 schakelt
        // direct de zender, ZZTU1 start de tune-carrier - beide gaan
        // niet naar Thetis. ZZTX0/ZZTU0 (uitschakelen) blijven uiteraard
        // wel werken zodat we een lopende TX kunnen afsluiten.
        let trimmed = cmd.trim_end_matches(';');
        if (trimmed.eq_ignore_ascii_case("ZZTX1") || trimmed.eq_ignore_ascii_case("ZZTU1"))
            && self.tx_blocked.load(Ordering::Relaxed)
        {
            warn!("CAT {} blocked: current Amplitec position is RX-only", trimmed);
            return;
        }
        // ZZ* commando's hebben drie niveaus van bediening:
        //   1. Native TCI mapping via `cat_to_tci` (snelste pad voor de
        //      meest-gebruikte ZZ-commando's zoals ZZFA/ZZMD/ZZTX).
        //   2. TCI `run_cat_ex:ZZxxx;` passthrough - Thetis voert het ZZ
        //      commando uit op zijn eigen interne CAT-parser zonder dat
        //      we een aparte CAT-verbinding nodig hebben. Werkt voor elk
        //      ZZ-commando dat Thetis kent (ZZPC, ZZCN, ZZCO, ZZFI, ...).
        //   3. Vroeger ook: auxiliary CAT TCP-socket. Vanaf v2.0.0 is dat
        //      pad verwijderd (TCI-only architectuur), dus niveau 3 is
        //      een no-op.
        //
        // Voorheen werd niveau 2 alleen direct vanuit specifieke call
        // sites (CTUN-recenter, filter-preset polls) gebruikt, en hier
        // werden onbekende ZZ-commando's gedropt met een WARN. Dat
        // maakte features die ZZ-commando's zonder `cat_to_tci`-mapping
        // gebruiken stilzwijgend no-op. Nu vallen onbekende ZZ-cmds
        // terug op `tci.run_cat()` zodat álle ZZ-commando's via dezelfde
        // single-TCI link hun pad vinden.
        if cmd.starts_with("ZZ") {
            if let Some(tci_cmd) = Self::cat_to_tci(cmd) {
                log::debug!("CAT->TCI: {} -> {}", cmd.trim_end_matches(';'), tci_cmd.trim_end_matches(';'));
                self.radio_send(&tci_cmd).await;
            } else {
                // Fallback: TCI run_cat_ex passthrough. Vraagt Thetis om
                // het ZZ-commando op zijn eigen CAT-parser uit te voeren.
                let zz = cmd.trim_end_matches(';');
                log::debug!("CAT->TCI run_cat_ex: {}", zz);
                self.tci.run_cat(zz).await;
            }
        } else {
            // TCI command (bv. TUNE:0,true;) -> direct via WebSocket
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

    /// Set the TX modulation filter band (tx_filter_band_ex). Hoofdradio-TX.
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
        // a TL-26 erfgoed). Use the optimistic _ex setter directly - operator-keuze 1a:
        // "Plus vfo_sync_ex _ex pad uitlijnen". Compat-statement (alpha-4 ≥ Thetis
        // v2.10.3.14) maakt de oude run_cat(ZZSY) fallback overbodig.
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

    /// DDC sample rate per receiver in Hz. In stock-mode beide RX1 en RX2 dezelfde
    /// waarde (TCI exposes één globale `iq_samplerate`); per-RX divergence komt
    /// terug via TL2-x fork extensions (Phase 3).
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

    /// Write TX audio to TCI ring buffer (only in TCI mode, no-op for CAT)
    pub fn write_tx_audio(&mut self, samples: &[f32]) {
        if let Some(tci) = Some(&mut self.tci) {
            tci.write_tx_audio(samples);
        }
    }
}
