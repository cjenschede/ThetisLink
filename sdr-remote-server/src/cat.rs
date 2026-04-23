// SPDX-License-Identifier: GPL-2.0-or-later

use std::collections::VecDeque;
use std::time::Instant;

use log::{info, warn};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

/// Default Thetis TCP/IP CAT address (TS-2000 protocol)
const DEFAULT_CAT_ADDR: &str = "127.0.0.1:13013";

/// CAT S-meter poll interval (milliseconds) — fast for responsive S-meter
const CAT_SMETER_POLL_MS: u64 = 100;
/// CAT full query interval (milliseconds) — ZZFA, ZZMD, ZZTX
const CAT_FULL_POLL_MS: u64 = 500;

/// Manages the TCP connection to Thetis CAT and all radio state readback.
pub struct CatConnection {
    stream: Option<TcpStream>,
    addr: String,
    read_buf: String,
    last_smeter_poll: Instant,
    last_full_poll: Instant,
    last_connect_attempt: Instant,
    // Radio state
    pub vfo_a_freq: u64,
    pub vfo_a_mode: u8,
    pub smeter_window: VecDeque<f32>,
    pub power_on: bool,
    pub tx_profile: u8,
    pub nr_level: u8,
    pub anf_on: bool,
    pub drive_level: u8,
    pub rx_af_gain: u8,
    pub tx_active: bool,
    pub fwd_power_watts: f32,
    pub filter_low_hz: i32,
    pub filter_high_hz: i32,
    pub ctun: bool,
    // RX2 / VFO-B state
    pub vfo_b_freq: u64,
    pub vfo_b_mode: u8,
    pub rx2_af_gain: u8,
    pub smeter_rx2_window: VecDeque<f32>,
    pub rx2_nr_level: u8,
    pub rx2_anf_on: bool,
    pub filter_rx2_low_hz: i32,
    pub filter_rx2_high_hz: i32,
    // Filter preset index (ZZFI/ZZFJ)
    pub filter_index: u8,
    pub filter_rx2_index: u8,
    // FM deviation (ZZFD): 0=2500Hz (NFM), 1=5000Hz (WFM)
    pub fm_deviation: u8,
    // TX Monitor (ZZMO)
    pub mon_on: bool,
    // VFO Sync (ZZSY) — readback from Thetis
    pub vfo_sync_on: bool,
    // Step attenuator (ZZRX/ZZRY) — 0 to 31 dB
    pub step_att_rx1: u8,
    pub step_att_rx2: u8,
    /// When true, only poll ZZLA/ZZLE (volume). Set when TCI _ex covers everything else.
    pub volume_only_mode: bool,
}

impl CatConnection {
    pub fn new(addr: Option<&str>) -> Self {
        Self {
            stream: None,
            addr: addr.unwrap_or(DEFAULT_CAT_ADDR).to_string(),
            read_buf: String::new(),
            last_smeter_poll: Instant::now(),
            last_full_poll: Instant::now(),
            last_connect_attempt: Instant::now() - std::time::Duration::from_secs(10),
            vfo_a_freq: 0,
            vfo_a_mode: 0,
            smeter_window: VecDeque::with_capacity(4),
            power_on: false,
            tx_profile: 0,
            nr_level: 0,
            anf_on: false,
            drive_level: 0,
            rx_af_gain: 0,
            tx_active: false,
            fwd_power_watts: 0.0,
            filter_low_hz: 0,
            filter_high_hz: 0,
            ctun: false,
            vfo_b_freq: 0,
            vfo_b_mode: 0,
            rx2_af_gain: 0,
            smeter_rx2_window: VecDeque::with_capacity(4),
            rx2_nr_level: 0,
            rx2_anf_on: false,
            filter_rx2_low_hz: 0,
            filter_rx2_high_hz: 0,
            filter_index: 3,
            filter_rx2_index: 3,
            fm_deviation: 1,
            mon_on: false,
            vfo_sync_on: false,
            step_att_rx1: 0,
            step_att_rx2: 0,
            volume_only_mode: false,
        }
    }

    /// Check if connection attempt is needed. Returns the TCP address if so.
    /// Updates the rate-limit timer.
    pub fn needs_connect(&mut self) -> Option<String> {
        if self.stream.is_some() {
            return None;
        }
        if self.last_connect_attempt.elapsed().as_secs() < 1 {
            return None;
        }
        self.last_connect_attempt = Instant::now();
        Some(self.addr.clone())
    }

    /// Accept an established TCP connection from the background connector.
    pub fn accept_stream(&mut self, stream: TcpStream) {
        info!("Connected to Thetis CAT at {}", self.addr);
        self.stream = Some(stream);
    }

    /// Send a CAT command string to Thetis (e.g. "ZZAG050;")
    pub async fn send(&mut self, cmd: &str) {
        if let Some(ref mut stream) = self.stream {
            if let Err(e) = stream.write_all(cmd.as_bytes()).await {
                warn!("CAT send '{}' failed: {}", cmd, e);
                self.handle_disconnect();
            }
        }
    }

    /// Send a CAT read command and wait for response (with 500ms timeout).
    /// Returns the raw response string (everything between command echo and ';').
    pub async fn query(&mut self, cmd: &str) -> Option<String> {
        self.send(cmd).await;
        let stream = self.stream.as_mut()?;
        let mut buf = [0u8; 256];
        let mut response = String::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
        use tokio::io::AsyncReadExt;
        loop {
            match tokio::time::timeout_at(deadline, stream.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => {
                    if let Ok(s) = std::str::from_utf8(&buf[..n]) {
                        response.push_str(s);
                        if response.contains(';') { break; }
                    }
                }
                _ => break,
            }
        }
        // Extract response value: find the command prefix and strip it
        let prefix = cmd.trim_end_matches(';');
        if let Some(start) = response.find(prefix) {
            let after = &response[start + prefix.len()..];
            if let Some(end) = after.find(';') {
                return Some(after[..end].to_string());
            }
        }
        None
    }

    /// Read CAT responses, parse them, and send periodic polls.
    /// Must be called periodically (e.g. every 100ms from safety check).
    pub async fn poll_and_parse(&mut self) {
        if self.stream.is_none() {
            return;
        }

        // Read available data
        if let Some(ref mut stream) = self.stream {
            let mut read_buf = [0u8; 1024];
            loop {
                match stream.try_read(&mut read_buf) {
                    Ok(0) => {
                        warn!("CAT connection closed by Thetis");
                        self.handle_disconnect();
                        return;
                    }
                    Ok(n) => {
                        if let Ok(s) = std::str::from_utf8(&read_buf[..n]) {
                            self.read_buf.push_str(s);
                        }
                    }
                    Err(_) => break,
                }
            }
        }

        self.parse_responses();

        let now = Instant::now();

        // Fast meter poll (every 100ms) — ZZSM = S-meter (RX), ZZRM5 = fwd power (TX)
        // Skip in volume_only_mode: S-meter comes via TCI rx_channel_sensors_ex
        if !self.volume_only_mode && now.duration_since(self.last_smeter_poll).as_millis() >= CAT_SMETER_POLL_MS as u128 {
            self.last_smeter_poll = now;
            if let Some(ref mut stream) = self.stream {
                let cmd = if self.tx_active { b"ZZRM5;" as &[u8] } else { b"ZZSM0;ZZSM1;" as &[u8] };
                if let Err(e) = stream.write_all(cmd).await {
                    warn!("CAT meter poll failed: {}", e);
                    self.handle_disconnect();
                }
            }
        }

        // Full CAT query (every 500ms): freq, mode, TX state
        // In volume_only_mode: only poll ZZLA/ZZLE (TCI _ex handles everything else)
        if now.duration_since(self.last_full_poll).as_millis() >= CAT_FULL_POLL_MS as u128 {
            self.last_full_poll = now;
            if let Some(ref mut stream) = self.stream {
                let poll = if self.volume_only_mode {
                    &b"ZZLA;ZZLE;"[..]
                } else {
                    &b"ZZFA;ZZMD;ZZTX;ZZPS;ZZTP;ZZNE;ZZNT;ZZPC;ZZLA;ZZLE;ZZFL;ZZFH;ZZFI;ZZFD;ZZCT;ZZFB;ZZME;ZZFS;ZZFR;ZZFJ;ZZNV;ZZNU;ZZMO;ZZSY;ZZRX;ZZRY;"[..]
                };
                if let Err(e) = stream.write_all(poll).await {
                    warn!("CAT keepalive failed: {}", e);
                    self.handle_disconnect();
                }
            }
        }
    }

    /// Parse all buffered CAT responses.
    /// Each parser drains only its own matched response (start..end) to avoid
    /// destroying data from other commands that may precede it in the buffer
    /// (e.g., tail of previous poll cycle mixed with head of new cycle).
    fn parse_responses(&mut self) {
        // Strip Thetis banner and any non-command data before first "ZZ" prefix
        // Thetis sends "#Thetis TCP/IP Cat - ...#;" on connect and for unknown cmds
        while let Some(hash_pos) = self.read_buf.find('#') {
            if let Some(end_hash) = self.read_buf[hash_pos + 1..].find('#') {
                // Found "#...#" — drain it plus any trailing semicolons
                let mut end = hash_pos + 1 + end_hash + 1;
                while end < self.read_buf.len() && self.read_buf.as_bytes()[end] == b';' {
                    end += 1;
                }
                self.read_buf.drain(hash_pos..end);
            } else {
                break; // incomplete banner
            }
        }

        // Parse ZZFA responses
        while let Some(start) = self.read_buf.find("ZZFA") {
            if self.read_buf.len() < start + 16 {
                break;
            }
            let after = &self.read_buf[start + 4..start + 16];
            if after.len() == 12 && after.ends_with(';') {
                let digits = &after[..11];
                if let Ok(freq) = digits.parse::<u64>() {
                    if freq != self.vfo_a_freq {
                        log::debug!("VFO A: {} Hz", freq);
                        self.vfo_a_freq = freq;
                    }
                }
            }
            self.read_buf.drain(start..start + 16);
        }

        // Parse ZZMD responses
        while let Some(start) = self.read_buf.find("ZZMD") {
            if self.read_buf.len() < start + 7 {
                break;
            }
            let after = &self.read_buf[start + 4..start + 7];
            if after.len() == 3 && after.ends_with(';') {
                let digits = &after[..2];
                if let Ok(mode) = digits.parse::<u8>() {
                    if mode != self.vfo_a_mode {
                        info!("VFO A mode: {}", mode);
                        self.vfo_a_mode = mode;
                    }
                }
            }
            self.read_buf.drain(start..start + 7);
        }

        // Parse ZZPS responses (power on/off) — "ZZPS#;" = 6 chars
        while let Some(start) = self.read_buf.find("ZZPS") {
            if self.read_buf.len() < start + 6 {
                break;
            }
            let ch = self.read_buf.as_bytes()[start + 4];
            if self.read_buf.as_bytes()[start + 5] == b';' {
                let on = ch == b'1';
                if on != self.power_on {
                    info!("Power: {}", if on { "ON" } else { "OFF" });
                    self.power_on = on;
                }
            }
            self.read_buf.drain(start..start + 6);
        }

        // Parse ZZTP responses (TX profile) — "ZZTP##;" = 7 chars
        while let Some(start) = self.read_buf.find("ZZTP") {
            if self.read_buf.len() < start + 7 {
                break;
            }
            let after = &self.read_buf[start + 4..start + 7];
            if after.ends_with(';') {
                if let Ok(idx) = after[..2].parse::<u8>() {
                    if idx != self.tx_profile {
                        info!("TX Profile: {}", idx);
                        self.tx_profile = idx;
                    }
                }
            }
            self.read_buf.drain(start..start + 7);
        }

        // Parse ZZNE responses (noise reduction level) — "ZZNE#;" = 6 chars
        while let Some(start) = self.read_buf.find("ZZNE") {
            if self.read_buf.len() < start + 6 {
                break;
            }
            let ch = self.read_buf.as_bytes()[start + 4];
            if self.read_buf.as_bytes()[start + 5] == b';' && ch >= b'0' && ch <= b'4' {
                let level = ch - b'0';
                if level != self.nr_level {
                    info!("NR: {}", if level == 0 { "OFF".to_string() } else { format!("NR{}", level) });
                    self.nr_level = level;
                }
            }
            self.read_buf.drain(start..start + 6);
        }

        // Parse ZZNT responses (auto notch filter) — "ZZNT#;" = 6 chars
        while let Some(start) = self.read_buf.find("ZZNT") {
            if self.read_buf.len() < start + 6 {
                break;
            }
            let ch = self.read_buf.as_bytes()[start + 4];
            if self.read_buf.as_bytes()[start + 5] == b';' {
                let on = ch == b'1';
                if on != self.anf_on {
                    info!("ANF: {}", if on { "ON" } else { "OFF" });
                    self.anf_on = on;
                }
            }
            self.read_buf.drain(start..start + 6);
        }

        // Parse ZZPC responses (drive level) — "ZZPC###;" = 8 chars
        while let Some(start) = self.read_buf.find("ZZPC") {
            if self.read_buf.len() < start + 8 {
                break;
            }
            let after = &self.read_buf[start + 4..start + 8];
            if after.ends_with(';') {
                if let Ok(level) = after[..3].parse::<u8>() {
                    let level = level.min(100);
                    if level != self.drive_level {
                        log::debug!("Drive: {}%", level);
                        self.drive_level = level;
                    }
                }
            }
            self.read_buf.drain(start..start + 8);
        }

        // Parse ZZLA responses (RX1 AF gain) — flexible: find semicolon
        while let Some(start) = self.read_buf.find("ZZLA") {
            let after_prefix = start + 4;
            if let Some(semi) = self.read_buf[after_prefix..].find(';') {
                let end = after_prefix + semi + 1;
                let payload = self.read_buf[after_prefix..after_prefix + semi].trim();
                if let Ok(level) = payload.parse::<u8>() {
                    let level = level.min(100);
                    if level != self.rx_af_gain {
                        info!("RX AF gain: {}% (raw: '{}')", level, payload);
                        self.rx_af_gain = level;
                    }
                }
                self.read_buf.drain(start..end);
            } else {
                break; // incomplete
            }
        }

        // Parse ZZFL responses (filter low cut Hz) — flexible: find semicolon
        while let Some(start) = self.read_buf.find("ZZFL") {
            let after_prefix = start + 4;
            if let Some(semi) = self.read_buf[after_prefix..].find(';') {
                let end = after_prefix + semi + 1;
                let payload = self.read_buf[after_prefix..after_prefix + semi].trim();
                if let Ok(hz) = payload.parse::<i32>() {
                    if hz != self.filter_low_hz {
                        self.filter_low_hz = hz;
                    }
                }
                self.read_buf.drain(start..end);
            } else {
                break;
            }
        }

        // Parse ZZFH responses (filter high cut Hz) — flexible: find semicolon
        while let Some(start) = self.read_buf.find("ZZFH") {
            let after_prefix = start + 4;
            if let Some(semi) = self.read_buf[after_prefix..].find(';') {
                let end = after_prefix + semi + 1;
                let payload = self.read_buf[after_prefix..after_prefix + semi].trim();
                if let Ok(hz) = payload.parse::<i32>() {
                    if hz != self.filter_high_hz {
                        self.filter_high_hz = hz;
                    }
                }
                self.read_buf.drain(start..end);
            } else {
                break;
            }
        }

        // Parse ZZFI responses (RX1 filter preset index) — "ZZFI##;" = 7 chars
        while let Some(start) = self.read_buf.find("ZZFI") {
            let after_prefix = start + 4;
            if let Some(semi) = self.read_buf[after_prefix..].find(';') {
                let end = after_prefix + semi + 1;
                let payload = self.read_buf[after_prefix..after_prefix + semi].trim();
                if let Ok(idx) = payload.parse::<u8>() {
                    if idx != self.filter_index {
                        info!("RX1 filter index: {} (was {})", idx, self.filter_index);
                        self.filter_index = idx;
                    }
                }
                self.read_buf.drain(start..end);
            } else {
                break;
            }
        }

        // Parse ZZFD responses (FM deviation) — "ZZFD#;" = 6 chars
        while let Some(start) = self.read_buf.find("ZZFD") {
            if self.read_buf.len() < start + 6 {
                break;
            }
            let ch = self.read_buf.as_bytes()[start + 4];
            if self.read_buf.as_bytes()[start + 5] == b';' {
                let dev = ch - b'0';
                if dev <= 1 && dev != self.fm_deviation {
                    info!("FM deviation: {} ({}Hz)", dev, if dev == 0 { 2500 } else { 5000 });
                    self.fm_deviation = dev;
                }
                self.read_buf.drain(start..start + 6);
            } else {
                self.read_buf.drain(start..start + 6);
            }
        }

        // Parse ZZCT responses (CTUN on/off) — "ZZCT#;" = 6 chars
        while let Some(start) = self.read_buf.find("ZZCT") {
            if self.read_buf.len() < start + 6 {
                break;
            }
            let ch = self.read_buf.as_bytes()[start + 4];
            if self.read_buf.as_bytes()[start + 5] == b';' {
                let on = ch == b'1';
                if on != self.ctun {
                    info!("CTUN: {}", if on { "ON" } else { "OFF" });
                    self.ctun = on;
                }
            }
            self.read_buf.drain(start..start + 6);
        }

        // Parse ZZFB responses (VFO-B frequency)
        while let Some(start) = self.read_buf.find("ZZFB") {
            if self.read_buf.len() < start + 16 {
                break;
            }
            let after = &self.read_buf[start + 4..start + 16];
            if after.len() == 12 && after.ends_with(';') {
                let digits = &after[..11];
                if let Ok(freq) = digits.parse::<u64>() {
                    if freq != self.vfo_b_freq {
                        log::debug!("VFO B: {} Hz", freq);
                        self.vfo_b_freq = freq;
                    }
                }
            }
            self.read_buf.drain(start..start + 16);
        }

        // Parse ZZME responses (VFO-B mode)
        while let Some(start) = self.read_buf.find("ZZME") {
            if self.read_buf.len() < start + 7 {
                break;
            }
            let after = &self.read_buf[start + 4..start + 7];
            if after.len() == 3 && after.ends_with(';') {
                let digits = &after[..2];
                if let Ok(mode) = digits.parse::<u8>() {
                    if mode != self.vfo_b_mode {
                        info!("VFO B mode: {}", mode);
                        self.vfo_b_mode = mode;
                    }
                }
            }
            self.read_buf.drain(start..start + 7);
        }

        // Parse ZZLE responses (RX2 AF gain) — flexible: find semicolon
        while let Some(start) = self.read_buf.find("ZZLE") {
            let after_prefix = start + 4;
            if let Some(semi) = self.read_buf[after_prefix..].find(';') {
                let end = after_prefix + semi + 1;
                let payload = self.read_buf[after_prefix..after_prefix + semi].trim();
                if let Ok(level) = payload.parse::<u8>() {
                    let level = level.min(100);
                    if level != self.rx2_af_gain {
                        info!("RX2 AF gain: {}% (raw: '{}')", level, payload);
                        self.rx2_af_gain = level;
                    }
                }
                self.read_buf.drain(start..end);
            } else {
                break;
            }
        }

        // Parse ZZFS responses (RX2 DSP filter low cut Hz)
        while let Some(start) = self.read_buf.find("ZZFS") {
            let after_prefix = start + 4;
            if let Some(semi) = self.read_buf[after_prefix..].find(';') {
                let end = after_prefix + semi + 1;
                let payload = self.read_buf[after_prefix..after_prefix + semi].trim();
                if let Ok(hz) = payload.parse::<i32>() {
                    if hz != self.filter_rx2_low_hz {
                        info!("RX2 filter low: {} Hz", hz);
                        self.filter_rx2_low_hz = hz;
                    }
                }
                self.read_buf.drain(start..end);
            } else {
                break;
            }
        }

        // Parse ZZFR responses (RX2 DSP filter high cut Hz)
        while let Some(start) = self.read_buf.find("ZZFR") {
            let after_prefix = start + 4;
            if let Some(semi) = self.read_buf[after_prefix..].find(';') {
                let end = after_prefix + semi + 1;
                let payload = self.read_buf[after_prefix..after_prefix + semi].trim();
                if let Ok(hz) = payload.parse::<i32>() {
                    if hz != self.filter_rx2_high_hz {
                        info!("RX2 filter high: {} Hz", hz);
                        self.filter_rx2_high_hz = hz;
                    }
                }
                self.read_buf.drain(start..end);
            } else {
                break;
            }
        }

        // Parse ZZFJ responses (RX2 filter preset index) — "ZZFJ##;" = 7 chars
        while let Some(start) = self.read_buf.find("ZZFJ") {
            let after_prefix = start + 4;
            if let Some(semi) = self.read_buf[after_prefix..].find(';') {
                let end = after_prefix + semi + 1;
                let payload = self.read_buf[after_prefix..after_prefix + semi].trim();
                if let Ok(idx) = payload.parse::<u8>() {
                    if idx != self.filter_rx2_index {
                        info!("RX2 filter index: {} (was {})", idx, self.filter_rx2_index);
                        self.filter_rx2_index = idx;
                    }
                }
                self.read_buf.drain(start..end);
            } else {
                break;
            }
        }

        // Parse ZZNV responses (RX2 noise reduction level)
        while let Some(start) = self.read_buf.find("ZZNV") {
            let after_prefix = start + 4;
            if let Some(semi) = self.read_buf[after_prefix..].find(';') {
                let end = after_prefix + semi + 1;
                let payload = self.read_buf[after_prefix..after_prefix + semi].trim();
                if let Ok(val) = payload.parse::<u8>() {
                    if val != self.rx2_nr_level {
                        self.rx2_nr_level = val.min(4);
                    }
                }
                self.read_buf.drain(start..end);
            } else {
                break;
            }
        }

        // Parse ZZNU responses (RX2 auto notch filter)
        while let Some(start) = self.read_buf.find("ZZNU") {
            let after_prefix = start + 4;
            if let Some(semi) = self.read_buf[after_prefix..].find(';') {
                let end = after_prefix + semi + 1;
                let payload = self.read_buf[after_prefix..after_prefix + semi].trim();
                let on = payload == "1";
                if on != self.rx2_anf_on {
                    self.rx2_anf_on = on;
                }
                self.read_buf.drain(start..end);
            } else {
                break;
            }
        }

        // Parse ZZMO responses (TX monitor on/off) — "ZZMO#;" = 6 chars
        while let Some(start) = self.read_buf.find("ZZMO") {
            if self.read_buf.len() < start + 6 {
                break;
            }
            let ch = self.read_buf.as_bytes()[start + 4];
            if self.read_buf.as_bytes()[start + 5] == b';' {
                let on = ch == b'1';
                if on != self.mon_on {
                    info!("MON: {}", if on { "ON" } else { "OFF" });
                    self.mon_on = on;
                }
            }
            self.read_buf.drain(start..start + 6);
        }

        // Parse ZZSY responses (VFO Sync on/off) — "ZZSY#;" = 6 chars
        while let Some(start) = self.read_buf.find("ZZSY") {
            if self.read_buf.len() < start + 6 {
                break;
            }
            let ch = self.read_buf.as_bytes()[start + 4];
            if self.read_buf.as_bytes()[start + 5] == b';' {
                let on = ch == b'1';
                if on != self.vfo_sync_on {
                    info!("VFO Sync: {}", if on { "ON" } else { "OFF" });
                    self.vfo_sync_on = on;
                }
            }
            self.read_buf.drain(start..start + 6);
        }

        // Parse ZZRX responses (RX1 step attenuation) — variable length "ZZRX...;"
        while let Some(start) = self.read_buf.find("ZZRX") {
            let after_prefix = start + 4;
            if let Some(semi) = self.read_buf[after_prefix..].find(';') {
                let end = after_prefix + semi + 1;
                let payload = self.read_buf[after_prefix..after_prefix + semi].trim();
                if let Ok(att) = payload.parse::<u8>() {
                    let att = att.min(31);
                    if att != self.step_att_rx1 {
                        info!("RX1 step ATT: {} dB", att);
                        self.step_att_rx1 = att;
                    }
                } else {
                    info!("ZZRX unparsed: '{}'", payload);
                }
                self.read_buf.drain(start..end);
            } else {
                break; // incomplete
            }
        }

        // Parse ZZRY responses (RX2 step attenuation) — variable length "ZZRY...;"
        while let Some(start) = self.read_buf.find("ZZRY") {
            let after_prefix = start + 4;
            if let Some(semi) = self.read_buf[after_prefix..].find(';') {
                let end = after_prefix + semi + 1;
                let payload = self.read_buf[after_prefix..after_prefix + semi].trim();
                if let Ok(att) = payload.parse::<u8>() {
                    let att = att.min(31);
                    if att != self.step_att_rx2 {
                        info!("RX2 step ATT: {} dB", att);
                        self.step_att_rx2 = att;
                    }
                } else {
                    info!("ZZRY unparsed: '{}'", payload);
                }
                self.read_buf.drain(start..end);
            } else {
                break; // incomplete
            }
        }

        // Parse ZZRM5 responses (forward power during TX)
        // Thetis returns watts with " W" suffix, e.g. "ZZRM583 W;"
        while let Some(start) = self.read_buf.find("ZZRM5") {
            let after_prefix = start + 5;
            if let Some(semi) = self.read_buf[after_prefix..].find(';') {
                let end = after_prefix + semi + 1;
                let payload = &self.read_buf[after_prefix..after_prefix + semi];
                let trimmed = payload.trim();
                let val_str = trimmed.strip_suffix(" dBm").unwrap_or(trimmed).trim();
                let val_str = val_str.strip_suffix(" W").unwrap_or(val_str).trim();
                if let Ok(watts) = val_str.parse::<f32>() {
                    self.fwd_power_watts = watts.clamp(0.0, 200.0);
                }
                self.read_buf.drain(start..end);
            } else {
                break;
            }
        }

        // Parse ZZSM0 (RX1) and ZZSM1 (RX2) S-meter responses.
        // Raw value → dBm: dBm = (raw / 2) - 140
        // Store as linear milliwatt for RMS averaging (0.4 sec window, ~4 samples at 100ms poll).
        // smeter_avg() converts RMS-averaged linear → dBm → display raw (0-260 scale).
        for (prefix, is_rx2) in [("ZZSM0", false), ("ZZSM1", true)] {
            while let Some(start) = self.read_buf.find(prefix) {
                let after_prefix = start + 5;
                if let Some(semi) = self.read_buf[after_prefix..].find(';') {
                    let end = after_prefix + semi + 1;
                    let payload = &self.read_buf[after_prefix..after_prefix + semi];
                    if let Ok(raw) = payload.trim().parse::<f32>() {
                        let dbm = (raw / 2.0) - 140.0;
                        // Store as linear power (mW) for RMS averaging
                        let mw = 10.0_f32.powf(dbm / 10.0);
                        let window = if is_rx2 { &mut self.smeter_rx2_window } else { &mut self.smeter_window };
                        if window.len() >= 4 {
                            window.pop_front();
                        }
                        window.push_back(mw);
                    }
                    self.read_buf.drain(start..end);
                } else {
                    break;
                }
            }
        }

        // Parse ZZTX responses (TX state) — "ZZTX#;" = 6 chars — prevent buffer accumulation
        while let Some(start) = self.read_buf.find("ZZTX") {
            if self.read_buf.len() < start + 6 {
                break;
            }
            if self.read_buf.as_bytes()[start + 5] == b';' {
                // Don't update tx_active here — PTT controller manages that
            }
            self.read_buf.drain(start..start + 6);
        }

        // Parse ZZAG responses (Master AF gain) — "ZZAG###;" = 8 chars — prevent accumulation
        while let Some(start) = self.read_buf.find("ZZAG") {
            if self.read_buf.len() < start + 8 {
                break;
            }
            if self.read_buf.as_bytes()[start + 7] == b';' {
                // Master AF set by us, just drain the echo
            }
            self.read_buf.drain(start..start + 8);
        }

        // Prevent unbounded buffer growth (orphan data from unparsed/unknown commands)
        if self.read_buf.len() > 1024 {
            let drain = self.read_buf.len() - 256;
            self.read_buf.drain(..drain);
        }
    }

    /// RMS-averaged S-meter: linear mean of mW values → back to dBm → display
    fn avg_mw_to_display(window: &VecDeque<f32>) -> u16 {
        if window.is_empty() {
            return 0;
        }
        let sum: f32 = window.iter().sum();
        let avg_mw = sum / window.len() as f32;
        let avg_dbm = 10.0 * avg_mw.log10();
        sdr_remote_core::dbm_to_display(avg_dbm)
    }

    /// Get RMS-averaged S-meter level (sliding window, ~0.4 sec)
    pub fn smeter_avg(&self) -> u16 {
        Self::avg_mw_to_display(&self.smeter_window)
    }

    /// Get RMS-averaged RX2 S-meter level
    pub fn smeter_rx2_avg(&self) -> u16 {
        Self::avg_mw_to_display(&self.smeter_rx2_window)
    }

    /// Forward power as u16 (watts * 10 for 0.1W resolution)
    pub fn fwd_power_raw(&self) -> u16 {
        (self.fwd_power_watts * 10.0).round() as u16
    }

    /// Set TX active state — clears meter window on transition
    pub fn set_tx_active(&mut self, active: bool) {
        if active != self.tx_active {
            self.tx_active = active;
            self.smeter_window.clear();
            self.smeter_rx2_window.clear();
            if !active {
                self.fwd_power_watts = 0.0;
            }
        }
    }

    /// Handle CAT disconnection: reset stream, buffer, and radio state
    fn handle_disconnect(&mut self) {
        self.stream = None;
        self.read_buf.clear();
        self.power_on = false;
    }

    /// Check if connected to CAT
    pub fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    // --- Setter methods (send CAT commands) ---

    pub async fn set_vfo_a_freq(&mut self, hz: u64) {
        let cmd = format!("ZZFA{:011};", hz);
        info!("CAT: set VFO A = {} Hz ({})", hz, cmd);
        self.send(&cmd).await;
    }

    pub async fn set_vfo_a_mode(&mut self, mode: u8) {
        let cmd = format!("ZZMD{:02};", mode);
        info!("CAT: set VFO A mode = {} ({})", mode, cmd);
        self.send(&cmd).await;
    }

    pub async fn set_power(&mut self, on: bool) {
        let cmd = if on { "ZZPS1;" } else { "ZZPS0;" };
        info!("CAT: Power {} ({})", if on { "ON" } else { "OFF" }, cmd);
        self.send(cmd).await;
    }

    pub async fn set_tx_profile(&mut self, idx: u8) {
        let cmd = format!("ZZTP{:02};", idx.min(99));
        info!("CAT: TX Profile = {} ({})", idx, cmd);
        self.send(&cmd).await;
    }

    pub async fn set_nr(&mut self, level: u8) {
        let level = level.min(4);
        let cmd = format!("ZZNE{};", level);
        info!("CAT: NR = {} ({})", if level == 0 { "OFF".to_string() } else { format!("NR{}", level) }, cmd);
        self.send(&cmd).await;
    }

    pub async fn set_anf(&mut self, on: bool) {
        let cmd = if on { "ZZNT1;" } else { "ZZNT0;" };
        info!("CAT: ANF {} ({})", if on { "ON" } else { "OFF" }, cmd);
        self.send(cmd).await;
    }

    pub async fn set_drive(&mut self, level: u8) {
        let level = level.min(100);
        let cmd = format!("ZZPC{:03};", level);
        info!("CAT: Drive = {}% ({})", level, cmd);
        self.send(&cmd).await;
    }

    /// Set RX1 filter. FM mode uses ZZFD (deviation toggle) instead of
    /// ZZFL/ZZFH which crashes Thetis.
    pub async fn set_filter(&mut self, low_hz: i32, high_hz: i32) {
        if self.vfo_a_mode == 5 {
            // FM mode: toggle deviation (ZZFD 0=2500Hz NFM, 1=5000Hz WFM)
            let new_bw = high_hz - low_hz;
            let new_dev = if new_bw >= 8000 { 1u8 } else { 0u8 };
            if new_dev != self.fm_deviation {
                let cmd = format!("ZZFD{};", new_dev);
                info!("CAT: FM deviation {} -> {} ({})", self.fm_deviation, new_dev, cmd);
                self.fm_deviation = new_dev;
                self.send(&cmd).await;
            }
            return;
        }
        let cmd = format!("ZZFL{};ZZFH{};", fmt_cat_hz(low_hz), fmt_cat_hz(high_hz));
        info!("CAT: Filter = {} .. {} Hz ({})", low_hz, high_hz, cmd);
        self.send(&cmd).await;
    }

    pub async fn set_vfo_b_freq(&mut self, hz: u64) {
        let cmd = format!("ZZFB{:011};", hz);
        info!("CAT: set VFO B = {} Hz ({})", hz, cmd);
        self.send(&cmd).await;
    }

    pub async fn set_vfo_b_mode(&mut self, mode: u8) {
        let cmd = format!("ZZME{:02};", mode);
        info!("CAT: set VFO B mode = {} ({})", mode, cmd);
        self.send(&cmd).await;
    }

    pub async fn vfo_swap(&mut self) {
        info!("CAT: VFO A<>B swap (ZZVS2)");
        self.send("ZZVS2;").await;
    }

    pub async fn set_rx2_af_gain(&mut self, level: u8) {
        let level = level.min(100);
        let cmd = format!("ZZLE{:03};", level);
        info!("CAT: RX2 AF gain = {}% ({})", level, cmd);
        self.send(&cmd).await;
    }

    pub async fn set_rx2_nr(&mut self, level: u8) {
        let level = level.min(4);
        let cmd = format!("ZZNV{};", level);
        info!("CAT: RX2 NR = {} ({})", if level == 0 { "OFF".to_string() } else { format!("NR{}", level) }, cmd);
        self.send(&cmd).await;
    }

    pub async fn set_rx2_anf(&mut self, on: bool) {
        let cmd = if on { "ZZNU1;" } else { "ZZNU0;" };
        info!("CAT: RX2 ANF {} ({})", if on { "ON" } else { "OFF" }, cmd);
        self.send(cmd).await;
    }

    pub async fn set_mon(&mut self, on: bool) {
        let cmd = if on { "ZZMO1;" } else { "ZZMO0;" };
        info!("CAT: MON {} ({})", if on { "ON" } else { "OFF" }, cmd);
        self.send(cmd).await;
    }

    pub async fn set_vfo_sync(&mut self, on: bool) {
        let cmd = if on { "ZZSY1;" } else { "ZZSY0;" };
        info!("CAT: VFO Sync {} ({})", if on { "ON" } else { "OFF" }, cmd);
        self.send(cmd).await;
    }

    /// Set RX2 filter edges. FM mode blocked (same Thetis CAT bug).
    pub async fn set_rx2_filter(&mut self, low_hz: i32, high_hz: i32) {
        if self.vfo_b_mode == 5 {
            // FM mode: toggle deviation via ZZFD (shared with RX1)
            let new_bw = high_hz - low_hz;
            let new_dev = if new_bw >= 8000 { 1u8 } else { 0u8 };
            if new_dev != self.fm_deviation {
                let cmd = format!("ZZFD{};", new_dev);
                info!("CAT: FM deviation (RX2) {} -> {} ({})", self.fm_deviation, new_dev, cmd);
                self.fm_deviation = new_dev;
                self.send(&cmd).await;
            }
            return;
        }
        let cmd = format!("ZZFS{};ZZFR{};", fmt_cat_hz(low_hz), fmt_cat_hz(high_hz));
        info!("CAT: RX2 Filter = {} .. {} Hz ({})", low_hz, high_hz, cmd);
        self.send(&cmd).await;
    }
}

/// Format Hz value for Thetis CAT: always 5 characters total.
/// Positive: 5-digit zero-padded, e.g. 100 → "00100", 3400 → "03400"
/// Negative: minus + 4-digit zero-padded, e.g. -100 → "-0100", -3700 → "-3700"
fn fmt_cat_hz(v: i32) -> String {
    if v >= 0 {
        format!("{:05}", v)
    } else {
        format!("-{:04}", v.unsigned_abs())
    }
}
