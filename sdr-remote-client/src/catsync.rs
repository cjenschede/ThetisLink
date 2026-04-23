// SPDX-License-Identifier: GPL-2.0-or-later

//! CatSync — automatically mute browser audio during TX.
//!
//! Uses the Windows Audio Session API (WASAPI) to mute/unmute audio sessions of
//! browsers (Chrome, Firefox, Edge) when PTT is active.
//! Optional: embedded WebView via wry for direct JS control over WebSDR.

use std::time::Instant;

use log::{info, warn};

/// Browser process names to mute during TX
const BROWSER_NAMES: &[&str] = &[
    "chrome.exe",
    "firefox.exe",
    "msedge.exe",
    "opera.exe",
    "brave.exe",
    "vivaldi.exe",
];

/// Default WebSDR URL (Maasbree HF)
const DEFAULT_WEBSDR_URL: &str = "http://sdr.websdrmaasbree.nl:8901/";

/// Debounce delay for freq sync to WebView (ms)
const FREQ_SYNC_DEBOUNCE_MS: u128 = 500;

/// Detect KiwiSDR from URL (port 8073 or "kiwisdr" in name)
pub fn is_kiwi_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.contains(":8073") || lower.contains("kiwisdr")
}

/// Max favorite WebSDR entries
const MAX_FAVORITES: usize = 10;

/// Extract a short label from a URL (hostname without common prefixes)
fn url_to_label(url: &str) -> String {
    let stripped = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://")).unwrap_or(url);
    let host = stripped.split('/').next().unwrap_or(stripped);
    let host = host.split(':').next().unwrap_or(host);
    // Remove common prefixes
    let label = host.strip_prefix("www.").unwrap_or(host);
    let label = label.strip_prefix("sdr.").unwrap_or(label);
    label.to_string()
}

pub struct CatSync {
    pub enabled: bool,
    muted: bool,
    /// Number of audio sessions muted in last operation
    pub sessions_muted: usize,
    /// WebSDR base URL (active)
    pub websdr_url: String,
    /// Favorite WebSDR URLs (label|url pairs)
    pub favorites: Vec<(String, String)>,
    /// Channel to embedded WebView (None = not open)
    websdr_tx: Option<std::sync::mpsc::Sender<crate::websdr::WebSdrCmd>>,
    /// Detected SDR type for JS dispatch
    sdr_type: crate::websdr::SdrType,
    /// WebView mute state (tracked separately from WASAPI)
    webview_muted: bool,
    /// Debounce state for freq sync
    last_synced_freq: u64,
    last_synced_mode: u8,
    freq_changed_at: Option<Instant>,
    pending_freq: u64,
    pending_mode: u8,
}

impl CatSync {
    pub fn new() -> Self {
        Self {
            enabled: false,
            muted: false,
            sessions_muted: 0,
            websdr_url: DEFAULT_WEBSDR_URL.to_string(),
            favorites: Vec::new(),
            websdr_tx: None,
            sdr_type: crate::websdr::SdrType::WebSdr,
            webview_muted: false,
            last_synced_freq: 0,
            last_synced_mode: 255,
            freq_changed_at: None,
            pending_freq: 0,
            pending_mode: 0,
        }
    }

    /// Add current URL as favorite (with auto-label)
    pub fn add_favorite(&mut self) {
        let url = self.websdr_url.trim().to_string();
        if url.is_empty() { return; }
        // Don't add duplicates
        if self.favorites.iter().any(|(_, u)| u == &url) { return; }
        // Auto-generate label from hostname
        let label = url_to_label(&url);
        if self.favorites.len() >= MAX_FAVORITES {
            self.favorites.pop();
        }
        self.favorites.push((label, url));
    }

    /// Remove favorite by index
    pub fn remove_favorite(&mut self, idx: usize) {
        if idx < self.favorites.len() {
            self.favorites.remove(idx);
        }
    }

    /// Select a favorite (sets active URL)
    pub fn select_favorite(&mut self, idx: usize) {
        if let Some((_, url)) = self.favorites.get(idx) {
            self.websdr_url = url.clone();
        }
    }

    /// Build WebSDR URL with frequency and mode
    pub fn websdr_tune_url(&self, freq_hz: u64, mode: u8) -> String {
        if is_kiwi_url(&self.websdr_url) {
            let mode_str = match mode {
                0 => "lsb", 1 => "usb", 5 => "nbfm", 6 => "am", 7 => "cw",
                _ => "usb",
            };
            let freq_khz = freq_hz as f64 / 1000.0;
            format!("{}?f={:.3}{}&z=9", self.websdr_url, freq_khz, mode_str)
        } else {
            let mode_str = match mode {
                0 => "lsb", 1 => "usb", 5 => "fm", 6 => "am",
                _ => "usb",
            };
            let freq_khz = freq_hz as f64 / 1000.0;
            format!("{}?tune={:.2}{}", self.websdr_url, freq_khz, mode_str)
        }
    }

    /// Update mute state based on TX. Only acts when enabled and state changes.
    pub fn update_mute(&mut self, transmitting: bool) {
        // WebView JS mute: always when window is open, regardless of WASAPI checkbox
        if let Some(ref tx) = self.websdr_tx {
            if transmitting != self.webview_muted {
                if tx.send(crate::websdr::WebSdrCmd::Mute(transmitting)).is_ok() {
                    self.webview_muted = transmitting;
                } else {
                    self.websdr_tx = None;
                }
            }
        }

        // WASAPI browser mute: only when enabled
        if !self.enabled {
            if self.muted {
                self.apply_mute(false);
                self.muted = false;
            }
            return;
        }
        if transmitting == self.muted {
            return;
        }
        self.apply_mute(transmitting);
        self.muted = transmitting;
    }

    /// Force unmute (e.g., on disconnect)
    pub fn force_unmute(&mut self) {
        if self.muted {
            self.apply_mute(false);
            self.muted = false;
        }
        if let Some(ref tx) = self.websdr_tx {
            let _ = tx.send(crate::websdr::WebSdrCmd::Mute(false));
        }
    }

    pub fn is_muted(&self) -> bool {
        self.muted
    }

    /// Open embedded WebSDR window. Returns true if opened successfully.
    pub fn open_websdr_window(&mut self, freq_hz: u64, mode: u8) -> bool {
        if self.websdr_tx.is_some() {
            return true; // already open
        }
        self.sdr_type = crate::websdr::SdrType::detect(&self.websdr_url);
        let url = self.websdr_tune_url(freq_hz, mode);
        let tx = crate::websdr::spawn_websdr_window(&url, self.sdr_type);
        self.websdr_tx = Some(tx);
        self.last_synced_freq = freq_hz;
        self.last_synced_mode = mode;
        true
    }

    /// Close embedded WebSDR window.
    pub fn close_websdr_window(&mut self) {
        if let Some(tx) = self.websdr_tx.take() {
            let _ = tx.send(crate::websdr::WebSdrCmd::Close);
        }
    }

    /// Is the embedded WebView window open?
    pub fn webview_open(&self) -> bool {
        self.websdr_tx.is_some()
    }

    /// Sync frequency to WebView (debounced). Call every frame.
    pub fn sync_freq(&mut self, freq_hz: u64, mode: u8) {
        let tx = match self.websdr_tx {
            Some(ref tx) => tx,
            None => return,
        };

        let freq_delta_khz = (freq_hz as i64 - self.last_synced_freq as i64).unsigned_abs() / 1000;
        let mode_changed = mode != self.last_synced_mode;

        if freq_delta_khz == 0 && !mode_changed {
            self.freq_changed_at = None;
            return;
        }

        // Track pending change
        if self.pending_freq != freq_hz || self.pending_mode != mode {
            self.pending_freq = freq_hz;
            self.pending_mode = mode;
            self.freq_changed_at = Some(Instant::now());
            return;
        }

        // Check debounce
        if let Some(changed_at) = self.freq_changed_at {
            if changed_at.elapsed().as_millis() >= FREQ_SYNC_DEBOUNCE_MS {
                if tx.send(crate::websdr::WebSdrCmd::SetFreq(freq_hz, mode)).is_ok() {
                    self.last_synced_freq = freq_hz;
                    self.last_synced_mode = mode;
                    self.freq_changed_at = None;
                    info!("WebSDR freq sync: {} Hz, mode {}", freq_hz, mode);
                } else {
                    // Window was closed
                    self.websdr_tx = None;
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn apply_mute(&mut self, mute: bool) {
        match mute_browser_sessions(mute) {
            Ok(count) => {
                self.sessions_muted = count;
                if count > 0 {
                    info!("CatSync: {} {} browser audio session(s)",
                        if mute { "muted" } else { "unmuted" }, count);
                }
            }
            Err(e) => {
                warn!("CatSync: audio session control failed: {}", e);
                self.sessions_muted = 0;
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn apply_mute(&mut self, _mute: bool) {
        self.sessions_muted = 0;
    }
}

// ── Windows Audio Session API implementation ──────────────────────────────

#[cfg(target_os = "windows")]
fn mute_browser_sessions(mute: bool) -> Result<usize, String> {
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::*;
    use windows::core::*;

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|e| format!("CoCreateInstance MMDeviceEnumerator: {}", e))?;

        let device = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia)
            .map_err(|e| format!("GetDefaultAudioEndpoint: {}", e))?;

        let session_mgr: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)
            .map_err(|e| format!("Activate IAudioSessionManager2: {}", e))?;

        let session_enum = session_mgr.GetSessionEnumerator()
            .map_err(|e| format!("GetSessionEnumerator: {}", e))?;

        let count = session_enum.GetCount()
            .map_err(|e| format!("GetCount: {}", e))?;

        let mut muted_count = 0usize;

        for i in 0..count {
            let session = match session_enum.GetSession(i) {
                Ok(s) => s,
                Err(_) => continue,
            };

            let session2: IAudioSessionControl2 = match session.cast() {
                Ok(s) => s,
                Err(_) => continue,
            };

            let pid = match session2.GetProcessId() {
                Ok(p) => p,
                Err(_) => continue,
            };

            if pid == 0 { continue; }

            let name = process_name(pid);
            let is_browser = BROWSER_NAMES.iter()
                .any(|b| name.eq_ignore_ascii_case(b));

            if is_browser {
                let volume: ISimpleAudioVolume = match session.cast() {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let _ = volume.SetMute(mute, std::ptr::null());
                muted_count += 1;
            }
        }

        Ok(muted_count)
    }
}

#[cfg(target_os = "windows")]
fn process_name(pid: u32) -> String {
    use windows::Win32::System::Threading::*;
    use windows::Win32::Foundation::CloseHandle;

    unsafe {
        let handle = match OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            pid,
        ) {
            Ok(h) => h,
            Err(_) => return String::new(),
        };

        let mut buf = [0u16; 260];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);

        if ok.is_ok() && len > 0 {
            let path = String::from_utf16_lossy(&buf[..len as usize]);
            path.rsplit('\\').next().unwrap_or("").to_string()
        } else {
            String::new()
        }
    }
}
