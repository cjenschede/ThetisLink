#![allow(dead_code)]
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Instant;

use log::{info, warn};

/// Timeout before considering a client disconnected (15s for mobile resilience)
const SESSION_TIMEOUT_SECS: u64 = 15;

/// Max failed auth attempts before blocking an IP
const MAX_AUTH_FAILURES: u32 = 5;
/// Block duration after too many failures
const AUTH_BLOCK_SECS: u64 = 60;

/// Authentication state for a client
#[derive(Debug)]
pub enum AuthState {
    /// No password configured — all clients rejected
    NoAuth,
    /// Challenge sent, awaiting HMAC response
    PendingChallenge { nonce: [u8; 16], sent_at: Instant },
    /// HMAC verified, awaiting TOTP code
    PendingTotp,
    /// Client authenticated successfully
    Authenticated,
}

/// Tracks failed auth attempts per socket address (IP:port).
/// Per-socket instead of per-IP so clients behind the same NAT don't block each other.
#[derive(Debug)]
struct AuthFailureTracker {
    failures: HashMap<SocketAddr, (u32, Instant)>,
}

impl AuthFailureTracker {
    fn new() -> Self { Self { failures: HashMap::new() } }

    fn is_blocked(&self, addr: &SocketAddr) -> bool {
        if let Some((count, last)) = self.failures.get(addr) {
            *count >= MAX_AUTH_FAILURES && last.elapsed().as_secs() < AUTH_BLOCK_SECS
        } else { false }
    }

    fn record_failure(&mut self, addr: SocketAddr) {
        let entry = self.failures.entry(addr).or_insert((0, Instant::now()));
        entry.0 += 1;
        entry.1 = Instant::now();
        warn!("Auth failure from {} ({}/{})", addr, entry.0, MAX_AUTH_FAILURES);
    }

    fn clear(&mut self, addr: &SocketAddr) {
        self.failures.remove(addr);
    }
}

/// A connected client session
#[derive(Debug)]
pub struct ClientSession {
    pub addr: SocketAddr,
    pub last_seen: Instant,
    pub auth_state: AuthState,
    pub last_heartbeat_seq: u32,
    pub rtt_ms: u16,
    pub loss_percent: u8,
    pub jitter_ms: u8,
    pub spectrum_enabled: bool,
    pub spectrum_fps: u8,
    pub spectrum_zoom: f32,
    pub spectrum_pan: f32,
    pub spectrum_max_bins: u16,
    pub rx2_enabled: bool,
    pub rx2_spectrum_enabled: bool,
    pub rx2_spectrum_fps: u8,
    pub rx2_spectrum_zoom: f32,
    pub rx2_spectrum_pan: f32,
    pub rx2_spectrum_max_bins: u16,
    pub vfo_sync: bool,
    pub yaesu_enabled: bool,
    pub audio_mode: u8, // 255=default(CH0 only), 0=Mono, 1=BIN, 2=Split
}

/// Result of touching a session
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchResult {
    /// Existing active client, just updated last_seen
    Existing,
    /// New client connected
    NewClient,
}

/// Manages connected client sessions.
/// Supports multiple simultaneous clients with single-TX arbitration.
pub struct SessionManager {
    clients: HashMap<SocketAddr, ClientSession>,
    /// Which client currently holds the TX (PTT) lock
    tx_holder: Option<SocketAddr>,
    /// Rate-limit auth failures per IP
    auth_failures: AuthFailureTracker,
    /// Server password (None = no auth required)
    password: Option<String>,
    /// TOTP secret (None = 2FA disabled)
    totp_secret: Option<String>,
}

impl SessionManager {
    pub fn new(password: Option<String>, totp_secret: Option<String>) -> Self {
        if password.is_some() {
            info!("Authentication enabled (password configured)");
        } else {
            warn!("No password configured — all client connections will be rejected");
        }
        if totp_secret.is_some() {
            info!("2FA enabled (TOTP configured)");
        }
        Self {
            clients: HashMap::new(),
            tx_holder: None,
            auth_failures: AuthFailureTracker::new(),
            password,
            totp_secret,
        }
    }

    /// Check if TOTP 2FA is enabled
    pub fn totp_enabled(&self) -> bool {
        self.totp_secret.is_some()
    }

    /// Check if authentication is required
    pub fn auth_required(&self) -> bool {
        self.password.is_some()
    }

    /// Check if an IP is blocked due to too many auth failures
    pub fn is_blocked(&self, addr: SocketAddr) -> bool {
        self.auth_failures.is_blocked(&addr)
    }

    /// Get the auth state for an address (None if unknown)
    pub fn get_auth_state(&self, addr: SocketAddr) -> Option<&AuthState> {
        self.clients.get(&addr).map(|s| &s.auth_state)
    }

    /// Check if a client is authenticated.
    /// Password is always required — unauthenticated clients are rejected.
    pub fn is_authenticated(&self, addr: SocketAddr) -> bool {
        if self.password.is_none() { return false; }
        matches!(self.get_auth_state(addr), Some(AuthState::Authenticated))
    }

    /// Create a pending challenge for a new client. Returns the nonce.
    pub fn create_challenge(&mut self, addr: SocketAddr) -> [u8; 16] {
        let nonce = sdr_remote_core::auth::generate_nonce();
        self.clients.insert(addr, ClientSession {
            addr,
            last_seen: Instant::now(),
            auth_state: AuthState::PendingChallenge { nonce, sent_at: Instant::now() },
            last_heartbeat_seq: 0, rtt_ms: 0, loss_percent: 0, jitter_ms: 0,
            spectrum_enabled: false,
            spectrum_fps: sdr_remote_core::DEFAULT_SPECTRUM_FPS,
            spectrum_zoom: 1.0, spectrum_pan: 0.0,
            spectrum_max_bins: sdr_remote_core::DEFAULT_SPECTRUM_BINS as u16,
            rx2_enabled: false, rx2_spectrum_enabled: false,
            rx2_spectrum_fps: sdr_remote_core::DEFAULT_SPECTRUM_FPS,
            rx2_spectrum_zoom: 1.0, rx2_spectrum_pan: 0.0,
            rx2_spectrum_max_bins: sdr_remote_core::DEFAULT_SPECTRUM_BINS as u16,
            vfo_sync: false, yaesu_enabled: false, audio_mode: 255,
        });
        info!("Auth challenge sent to {}", addr);
        nonce
    }

    /// Verify an auth response. Returns true if accepted.
    /// Verify HMAC auth response. Returns:
    /// - 0 = rejected
    /// - 1 = accepted (fully authenticated)
    /// - 2 = HMAC ok, TOTP required (pending 2FA)
    pub fn verify_auth(&mut self, addr: SocketAddr, hmac: &[u8; 32]) -> u8 {
        let password = match &self.password {
            Some(p) => p.clone(),
            None => return sdr_remote_core::protocol::AUTH_REJECTED,
        };
        if let Some(session) = self.clients.get_mut(&addr) {
            if let AuthState::PendingChallenge { nonce, .. } = &session.auth_state {
                let nonce = *nonce;
                if sdr_remote_core::auth::verify_hmac(&password, &nonce, hmac) {
                    if self.totp_secret.is_some() {
                        session.auth_state = AuthState::PendingTotp;
                        info!("Client {} password OK, awaiting TOTP", addr);
                        return sdr_remote_core::protocol::AUTH_TOTP_REQUIRED;
                    }
                    session.auth_state = AuthState::Authenticated;
                    self.auth_failures.clear(&addr);
                    info!("Client {} authenticated", addr);
                    return sdr_remote_core::protocol::AUTH_ACCEPTED;
                }
            }
        }
        self.auth_failures.record_failure(addr);
        warn!("Authentication failed from {}", addr);
        sdr_remote_core::protocol::AUTH_REJECTED
    }

    /// Verify TOTP code. Returns true if code is valid.
    pub fn verify_totp(&mut self, addr: SocketAddr, code: &str) -> bool {
        let secret = match &self.totp_secret {
            Some(s) => s.clone(),
            None => return false,
        };
        if let Some(session) = self.clients.get_mut(&addr) {
            if matches!(session.auth_state, AuthState::PendingTotp) {
                if sdr_remote_core::auth::verify_totp(&secret, code) {
                    session.auth_state = AuthState::Authenticated;
                    self.auth_failures.clear(&addr);
                    info!("Client {} TOTP verified, fully authenticated", addr);
                    return true;
                }
            }
        }
        self.auth_failures.record_failure(addr);
        warn!("TOTP verification failed from {}", addr);
        false
    }

    /// Register activity from a client address.
    /// Returns TouchResult indicating if this is a new or existing client.
    pub fn touch(&mut self, addr: SocketAddr) -> TouchResult {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.last_seen = Instant::now();
            TouchResult::Existing
        } else {
            let auth_state = if self.password.is_some() {
                // Don't create full session yet — wait for challenge-response
                return TouchResult::NewClient;
            } else {
                AuthState::NoAuth
            };
            info!("New client connected: {}", addr);
            self.clients.insert(addr, ClientSession {
                addr,
                last_seen: Instant::now(),
                auth_state,
                last_heartbeat_seq: 0,
                rtt_ms: 0,
                loss_percent: 0,
                jitter_ms: 0,
                spectrum_enabled: false,
                spectrum_fps: sdr_remote_core::DEFAULT_SPECTRUM_FPS,
                spectrum_zoom: 1.0,
                spectrum_pan: 0.0,
                spectrum_max_bins: sdr_remote_core::DEFAULT_SPECTRUM_BINS as u16,
                rx2_enabled: false,
                rx2_spectrum_enabled: false,
                rx2_spectrum_fps: sdr_remote_core::DEFAULT_SPECTRUM_FPS,
                rx2_spectrum_zoom: 1.0,
                rx2_spectrum_pan: 0.0,
                rx2_spectrum_max_bins: sdr_remote_core::DEFAULT_SPECTRUM_BINS as u16,
                vfo_sync: false,
                yaesu_enabled: false,
                audio_mode: 255, // default: CH0 only until client sends AudioMode
            });
            TouchResult::NewClient
        }
    }

    /// Update heartbeat stats for a client session
    pub fn update_heartbeat(&mut self, addr: SocketAddr, seq: u32, rtt: u16, loss: u8, jitter: u8) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.last_heartbeat_seq = seq;
            session.rtt_ms = rtt;
            session.loss_percent = loss;
            session.jitter_ms = jitter;
        }
    }

    /// Remove a client session (explicit disconnect)
    pub fn remove(&mut self, addr: SocketAddr) {
        self.clients.remove(&addr);
        if self.tx_holder == Some(addr) {
            info!("TX holder {} disconnected, releasing TX lock", addr);
            self.tx_holder = None;
        }
    }

    /// Check if a session is active and authenticated
    fn is_active_authed(s: &ClientSession) -> bool {
        s.last_seen.elapsed().as_secs() <= SESSION_TIMEOUT_SECS
            && matches!(s.auth_state, AuthState::NoAuth | AuthState::Authenticated)
    }

    /// Get all active, authenticated client addresses
    pub fn active_addrs(&self) -> Vec<SocketAddr> {
        self.clients.values()
            .filter(|s| Self::is_active_authed(s))
            .map(|s| s.addr)
            .collect()
    }

    /// Clients that should receive Thetis S-meter.
    /// Excludes Yaesu-only clients (yaesu on + spectrum off = Android Yaesu mode).
    /// Desktop clients with yaesu+spectrum both on still receive S-meter.
    pub fn smeter_addrs(&self) -> Vec<SocketAddr> {
        self.clients.values()
            .filter(|s| (!s.yaesu_enabled || s.spectrum_enabled) && Self::is_active_authed(s))
            .map(|s| s.addr)
            .collect()
    }

    /// Check for timed-out sessions. Returns addresses of removed clients.
    pub fn check_timeout(&mut self) -> Vec<SocketAddr> {
        let timed_out: Vec<SocketAddr> = self.clients.values()
            .filter(|s| s.last_seen.elapsed().as_secs() > SESSION_TIMEOUT_SECS)
            .map(|s| s.addr)
            .collect();

        for &addr in &timed_out {
            warn!("Client {} timed out", addr);
            self.clients.remove(&addr);
            if self.tx_holder == Some(addr) {
                info!("TX holder {} timed out, releasing TX lock", addr);
                self.tx_holder = None;
            }
        }

        timed_out
    }

    /// Try to acquire the TX lock for a client. Returns true if granted.
    /// First-come-first-served: if no one holds TX, grant it; otherwise deny.
    pub fn try_acquire_tx(&mut self, addr: SocketAddr) -> bool {
        match self.tx_holder {
            None => {
                info!("TX lock acquired by {}", addr);
                self.tx_holder = Some(addr);
                true
            }
            Some(holder) if holder == addr => true,
            Some(_) => false,
        }
    }

    /// Release the TX lock (only if held by this client)
    pub fn release_tx(&mut self, addr: SocketAddr) {
        if self.tx_holder == Some(addr) {
            info!("TX lock released by {}", addr);
            self.tx_holder = None;
        }
    }

    /// Get the current TX holder address
    pub fn tx_holder(&self) -> Option<SocketAddr> {
        self.tx_holder
    }

    /// Set spectrum enabled for a client
    pub fn set_spectrum_enabled(&mut self, addr: SocketAddr, enabled: bool) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.spectrum_enabled = enabled;
        }
    }

    /// Set spectrum FPS for a client
    pub fn set_spectrum_fps(&mut self, addr: SocketAddr, fps: u8) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.spectrum_fps = fps.clamp(5, 30);
        }
    }

    /// Set spectrum zoom for a client
    pub fn set_spectrum_zoom(&mut self, addr: SocketAddr, zoom: f32) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.spectrum_zoom = zoom.clamp(1.0, 1024.0);
        }
    }

    /// Set spectrum pan for a client
    pub fn set_spectrum_pan(&mut self, addr: SocketAddr, pan: f32) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.spectrum_pan = pan.clamp(-0.5, 0.5);
        }
    }

    /// Set spectrum max bins for a client (0 = server default)
    pub fn set_spectrum_max_bins(&mut self, addr: SocketAddr, max_bins: u16) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.spectrum_max_bins = if max_bins == 0 {
                sdr_remote_core::DEFAULT_SPECTRUM_BINS as u16
            } else {
                max_bins.clamp(64, sdr_remote_core::MAX_SPECTRUM_SEND_BINS as u16)
            };
        }
    }

    /// Set RX2 enabled for a client
    pub fn set_rx2_enabled(&mut self, addr: SocketAddr, enabled: bool) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.rx2_enabled = enabled;
        }
    }

    pub fn set_yaesu_enabled(&mut self, addr: SocketAddr, enabled: bool) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.yaesu_enabled = enabled;
        }
    }

    pub fn client_audio_mode(&self, addr: SocketAddr) -> u8 {
        self.clients.get(&addr).map(|s| s.audio_mode).unwrap_or(255)
    }

    pub fn set_audio_mode(&mut self, addr: SocketAddr, mode: u8) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.audio_mode = mode;
        }
    }

    /// Resolve effective audio mode across all active clients.
    /// BIN (1) only if ALL clients want BIN. Otherwise use the highest non-BIN mode.
    /// Priority: Mono(0) < Split(2) < BIN(1). BIN requires unanimity.
    pub fn resolved_audio_mode(&self) -> u8 {
        let active: Vec<u8> = self.clients.values()
            .filter(|s| Self::is_active_authed(s))
            .map(|s| s.audio_mode)
            .collect();
        if active.is_empty() { return 0; }
        // BIN only if all clients agree
        if active.iter().all(|&m| m == 1) { return 1; }
        // Otherwise use highest non-BIN mode (Split=2 > Mono=0)
        *active.iter().filter(|&&m| m != 1).max().unwrap_or(&0)
    }

    pub fn yaesu_addrs(&self) -> Vec<SocketAddr> {
        self.clients.iter()
            .filter(|(_, s)| s.yaesu_enabled && Self::is_active_authed(s))
            .map(|(addr, _)| *addr)
            .collect()
    }

    /// Set RX2 spectrum enabled for a client
    pub fn set_rx2_spectrum_enabled(&mut self, addr: SocketAddr, enabled: bool) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.rx2_spectrum_enabled = enabled;
        }
    }

    /// Set RX2 spectrum FPS for a client
    pub fn set_rx2_spectrum_fps(&mut self, addr: SocketAddr, fps: u8) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.rx2_spectrum_fps = fps.clamp(5, 30);
        }
    }

    /// Set RX2 spectrum max bins for a client
    pub fn set_rx2_spectrum_max_bins(&mut self, addr: SocketAddr, max_bins: u16) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.rx2_spectrum_max_bins = if max_bins == 0 {
                sdr_remote_core::DEFAULT_SPECTRUM_BINS as u16
            } else {
                max_bins.clamp(64, sdr_remote_core::MAX_SPECTRUM_SEND_BINS as u16)
            };
        }
    }

    /// Set RX2 spectrum zoom for a client
    pub fn set_rx2_spectrum_zoom(&mut self, addr: SocketAddr, zoom: f32) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.rx2_spectrum_zoom = zoom.clamp(1.0, 1024.0);
        }
    }

    /// Set RX2 spectrum pan for a client
    pub fn set_rx2_spectrum_pan(&mut self, addr: SocketAddr, pan: f32) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.rx2_spectrum_pan = pan.clamp(-0.5, 0.5);
        }
    }

    /// Set VFO sync for a client
    pub fn set_vfo_sync(&mut self, addr: SocketAddr, enabled: bool) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.vfo_sync = enabled;
        }
    }

    /// Check if any active client has VFO sync enabled
    pub fn any_vfo_sync(&self) -> bool {
        self.clients.values()
            .any(|s| s.vfo_sync && Self::is_active_authed(s))
    }

    /// Get RX2 spectrum clients: (addr, zoom, pan, max_bins)
    pub fn rx2_spectrum_clients(&self) -> Vec<(SocketAddr, f32, f32, u16)> {
        self.clients.values()
            .filter(|s| s.rx2_enabled && s.rx2_spectrum_enabled && Self::is_active_authed(s))
            .map(|s| (s.addr, s.rx2_spectrum_zoom, s.rx2_spectrum_pan, s.rx2_spectrum_max_bins))
            .collect()
    }

    /// Get addresses of clients that have RX2 enabled (for audio/freq broadcast)
    pub fn rx2_addrs(&self) -> Vec<SocketAddr> {
        self.clients.values()
            .filter(|s| s.rx2_enabled && Self::is_active_authed(s))
            .map(|s| s.addr)
            .collect()
    }

    /// Get addresses of RX2 clients with spectrum enabled (for S-meter gating)
    pub fn rx2_spectrum_addrs(&self) -> Vec<SocketAddr> {
        self.clients.values()
            .filter(|s| s.rx2_enabled && s.rx2_spectrum_enabled && Self::is_active_authed(s))
            .map(|s| s.addr)
            .collect()
    }

    /// Get addresses of clients that have spectrum enabled
    pub fn spectrum_addrs(&self) -> Vec<SocketAddr> {
        self.clients.values()
            .filter(|s| s.spectrum_enabled && Self::is_active_authed(s))
            .map(|s| s.addr)
            .collect()
    }

    /// Get spectrum clients: (addr, zoom, pan, max_bins)
    pub fn spectrum_clients(&self) -> Vec<(SocketAddr, f32, f32, u16)> {
        self.clients.values()
            .filter(|s| s.spectrum_enabled && Self::is_active_authed(s))
            .map(|s| (s.addr, s.spectrum_zoom, s.spectrum_pan, s.spectrum_max_bins))
            .collect()
    }

    /// Get the loss percentage for a client (for spectrum throttling)
    pub fn client_loss(&self, addr: SocketAddr) -> u8 {
        self.clients.get(&addr).map_or(0, |s| s.loss_percent)
    }

    /// Get the maximum spectrum FPS across all spectrum-enabled clients.
    /// Server generates at the fastest rate any client needs; slower clients skip frames.
    pub fn spectrum_max_fps(&self) -> u8 {
        self.clients.values()
            .filter(|s| s.spectrum_enabled)
            .map(|s| s.spectrum_fps)
            .max()
            .unwrap_or(sdr_remote_core::DEFAULT_SPECTRUM_FPS)
    }
}
