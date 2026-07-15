// SPDX-License-Identifier: GPL-2.0-or-later

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::Instant;

use log::{info, warn};

/// Conservatieve server-side default voor spectrum max_bins, vóór een client z'n
/// eigen (scherm-passende) waarde stuurt. Vangnet zodat een ongeconfigureerd
/// spectrum nooit op de volle DEFAULT_SPECTRUM_BINS (8192) resolutie streamt en
/// zo de datarate opblaast. Clients die meer willen sturen gewoon hun eigen max_bins.
const SERVER_DEFAULT_MAX_BINS: u16 = 2048;

/// Timeout before considering a client disconnected (15s for mobile resilience)
const SESSION_TIMEOUT_SECS: u64 = 15;

/// Max failed auth attempts before blocking an IP
const MAX_AUTH_FAILURES: u32 = 5;
/// Block duration after too many failures
const AUTH_BLOCK_SECS: u64 = 60;

/// PATCH-2: ringbuffer-capacity for recent connect attempts shown in the
/// server Status panel. 10 entries balances "recent context for support"
/// against memory under brute-force traffic - see decision-log §6.
pub const CONNECT_HISTORY_CAPACITY: usize = 10;

/// Outcome of a single connect attempt - shown in the Status panel so the
/// operator can answer "what does the server see?" in one screenshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectOutcome {
    /// HMAC accepted, no 2FA required -> session active.
    Accepted,
    /// HMAC accepted, 2FA challenge sent - awaiting TOTP code.
    TotpRequired,
    /// HMAC mismatched (wrong password or replay-old nonce).
    WrongPassword,
    /// HMAC ok, 2FA code rejected.
    WrongTotp,
    /// New client started an auth handshake (challenge sent).
    /// Useful diagnostic ("is anything reaching the server?")
    /// even when no AuthResponse follows.
    ChallengeSent,
    /// Magic byte matched but the wire-protocol version did not - typically
    /// an outdated client (e.g. v2.0.2 APK against a build-58+ server).
    /// Without this entry the rejection is logged only and the operator cannot
    /// see in the Status panel that an old client is trying to reconnect.
    ProtocolVersionMismatch { client_version: u8 },
}

impl ConnectOutcome {
    /// Short display label for the Status panel (English; UI is dev-tooling
    /// for the operator, no i18n needed here).
    pub fn label(self) -> String {
        match self {
            ConnectOutcome::Accepted => "Accepted".into(),
            ConnectOutcome::TotpRequired => "2FA required".into(),
            ConnectOutcome::WrongPassword => "Wrong password".into(),
            ConnectOutcome::WrongTotp => "Wrong 2FA".into(),
            ConnectOutcome::ChallengeSent => "Challenge sent".into(),
            ConnectOutcome::ProtocolVersionMismatch { client_version } => {
                format!("Wrong protocol (client v{})", client_version)
            }
        }
    }

    pub fn is_failure(self) -> bool {
        matches!(
            self,
            ConnectOutcome::WrongPassword
                | ConnectOutcome::WrongTotp
                | ConnectOutcome::ProtocolVersionMismatch { .. }
        )
    }
}

/// A single connect attempt record kept in the SessionManager ringbuffer.
/// Carries both `Instant` (cheap relative-time calc) and a wall-clock
/// timestamp (for "17:42:11" UI display) - operator-feedback / review request.
#[derive(Debug, Clone)]
pub struct ConnectAttempt {
    pub instant: Instant,
    pub wall_clock: chrono::DateTime<chrono::Local>,
    pub remote_addr: SocketAddr,
    pub outcome: ConnectOutcome,
}

/// Snapshot of an active client for Status-panel display.
/// Owned-by-value so the UI can release the SessionManager lock immediately.
#[derive(Debug, Clone)]
pub struct ClientSnapshot {
    pub addr: SocketAddr,
    pub last_seen: Instant,
    pub connected_since: Instant,
    pub authenticated: bool,
    pub rtt_ms: u16,
    pub loss_percent: u8,
    pub jitter_ms: u8,
}

/// Authentication state for a client
#[derive(Debug)]
pub enum AuthState {
    /// No password configured - all clients rejected
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
    /// PATCH-2: timestamp when this `ClientSession` was first inserted
    /// (matches the first packet observed from this address). Drives the
    /// "connected for Xm Ys" column in the Status panel.
    pub connected_since: Instant,
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
    /// Dual-radio slot 1 subscription-gate (PATCH-dual-radio-991a-ftx1, Optie
    /// B-prime). Default false -> oude clients (die `Yaesu2Enable` nooit sturen)
    /// krijgen nooit slot-1 state/audio/memory. Dit is de echte back-compat-guard.
    pub yaesu2_enabled: bool,
    /// VRX per-client subscription-gates (hardening fix v2.2.0). Default
    /// false -> clients die `VrxEnable*`/`VrxSpectrumEnable*` nooit sturen
    /// (o.a. oude v2.1.x clients) krijgen nooit `AudioVrx`/`SpectrumVrx`
    /// packet-types. Zelfde back-compat-patroon als `yaesu2_enabled`.
    pub vrx1_audio_enabled: bool,
    pub vrx2_audio_enabled: bool,
    pub vrx1_spectrum_enabled: bool,
    pub vrx2_spectrum_enabled: bool,
    pub vrx1_autotune_enabled: bool,
    pub vrx2_autotune_enabled: bool,
    pub audio_mode: u8, // 255=default(CH0 only), 0=Mono, 1=BIN, 2=Split
    /// DX-cluster spot stream opt-out - default true (= stream actief).
    /// Wanneer false stuurt de server geen Spot-frames meer naar deze
    /// client. Bandbreedte-besparing op metered links.
    pub dx_spots_enabled: bool,
    /// TL2-1 ctun-auto-recenter: per-client setup-vink "Allow zoom below 2x".
    /// false=default (smear-vrij gegarandeerd, zoom-min 2x). true=opt-in (zoom 1x toegestaan).
    /// Server enforces strictest: zolang één client false heeft, server-zoom-min = 2x.
    pub allow_zoom_below_2x: bool,
    /// S-meter source-subscription bitmap (see `ControlId::SmeterSources` doc).
    /// Default 0x22 = RX1 Avg + RX2 Avg - matches pre-multi-source behaviour.
    pub smeter_sources: u16,
    /// Wideband-Thetis-audio opt-in: when true the server encodes
    /// RX1/RX2/BinR via wideband Opus (16 kHz, ~30 kbps/ch) i.p.v.
    /// narrowband (8 kHz, ~14 kbps/ch) en accepteert TX-audio met
    /// `Flags::AUDIO_WIDEBAND` gezet. Default false - opt-in via
    /// `ControlId::ThetisWidebandAudio` from the client.
    pub thetis_wideband_audio: bool,
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
    /// PATCH-2: ringbuffer of recent connect attempts for the Status panel.
    /// Bounded at CONNECT_HISTORY_CAPACITY entries; oldest evicted on overflow.
    connect_history: VecDeque<ConnectAttempt>,
}

impl SessionManager {
    pub fn new(password: Option<String>, totp_secret: Option<String>) -> Self {
        if password.is_some() {
            info!("Authentication enabled (password configured)");
        } else {
            warn!("No password configured - all client connections will be rejected");
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
            connect_history: VecDeque::with_capacity(CONNECT_HISTORY_CAPACITY),
        }
    }

    /// PATCH-2: append a connect-attempt to the bounded ringbuffer.
    /// Oldest entry is evicted at CONNECT_HISTORY_CAPACITY so the buffer
    /// stays bounded even under sustained brute-force traffic.
    pub fn record_connect_attempt(&mut self, addr: SocketAddr, outcome: ConnectOutcome) {
        if self.connect_history.len() == CONNECT_HISTORY_CAPACITY {
            self.connect_history.pop_front();
        }
        self.connect_history.push_back(ConnectAttempt {
            instant: Instant::now(),
            wall_clock: chrono::Local::now(),
            remote_addr: addr,
            outcome,
        });
    }

    /// PATCH-2: snapshot-clone of the connect-attempt ringbuffer for the
    /// Status panel. UI doesn't hold the lock during render - it takes
    /// the clone and releases the lock immediately.
    pub fn recent_connect_attempts(&self) -> Vec<ConnectAttempt> {
        self.connect_history.iter().cloned().collect()
    }

    /// PATCH-2: snapshot-clone of active client list for the Status panel.
    /// Each entry is a `(addr, connected_since, last_seen, rtt_ms, loss_pct, jitter_ms)`
    /// tuple - UI-friendly, no SessionManager-internal refs leaked.
    pub fn active_clients_snapshot(&self) -> Vec<ClientSnapshot> {
        self.clients
            .values()
            .map(|c| ClientSnapshot {
                addr: c.addr,
                last_seen: c.last_seen,
                connected_since: c.connected_since,
                authenticated: matches!(c.auth_state, AuthState::Authenticated),
                rtt_ms: c.rtt_ms,
                loss_percent: c.loss_percent,
                jitter_ms: c.jitter_ms,
            })
            .collect()
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
    /// Password is always required - unauthenticated clients are rejected.
    pub fn is_authenticated(&self, addr: SocketAddr) -> bool {
        if self.password.is_none() { return false; }
        matches!(self.get_auth_state(addr), Some(AuthState::Authenticated))
    }

    /// Create a pending challenge for a new client. Returns the nonce.
    pub fn create_challenge(&mut self, addr: SocketAddr) -> [u8; 16] {
        let nonce = sdr_remote_core::auth::generate_nonce();
        let now = Instant::now();
        self.clients.insert(addr, ClientSession {
            addr,
            last_seen: now,
            connected_since: now,
            auth_state: AuthState::PendingChallenge { nonce, sent_at: now },
            last_heartbeat_seq: 0, rtt_ms: 0, loss_percent: 0, jitter_ms: 0,
            spectrum_enabled: false,
            spectrum_fps: sdr_remote_core::DEFAULT_SPECTRUM_FPS,
            spectrum_zoom: 1.0, spectrum_pan: 0.0,
            spectrum_max_bins: SERVER_DEFAULT_MAX_BINS,
            rx2_enabled: false, rx2_spectrum_enabled: false,
            rx2_spectrum_fps: sdr_remote_core::DEFAULT_SPECTRUM_FPS,
            rx2_spectrum_zoom: 1.0, rx2_spectrum_pan: 0.0,
            rx2_spectrum_max_bins: SERVER_DEFAULT_MAX_BINS,
            vfo_sync: false, yaesu_enabled: false, yaesu2_enabled: false, audio_mode: 255,
            dx_spots_enabled: true,
            allow_zoom_below_2x: false,
            smeter_sources: 0x22,
            thetis_wideband_audio: false,
            vrx1_audio_enabled: false, vrx2_audio_enabled: false,
            vrx1_spectrum_enabled: false, vrx2_spectrum_enabled: false,
                vrx1_autotune_enabled: false, vrx2_autotune_enabled: false,
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
                // Don't create full session yet - wait for challenge-response
                return TouchResult::NewClient;
            } else {
                AuthState::NoAuth
            };
            info!("New client connected: {}", addr);
            let now = Instant::now();
            self.clients.insert(addr, ClientSession {
                addr,
                last_seen: now,
                connected_since: now,
                auth_state,
                last_heartbeat_seq: 0,
                rtt_ms: 0,
                loss_percent: 0,
                jitter_ms: 0,
                spectrum_enabled: false,
                spectrum_fps: sdr_remote_core::DEFAULT_SPECTRUM_FPS,
                spectrum_zoom: 1.0,
                spectrum_pan: 0.0,
                spectrum_max_bins: SERVER_DEFAULT_MAX_BINS,
                rx2_enabled: false,
                rx2_spectrum_enabled: false,
                rx2_spectrum_fps: sdr_remote_core::DEFAULT_SPECTRUM_FPS,
                rx2_spectrum_zoom: 1.0,
                rx2_spectrum_pan: 0.0,
                rx2_spectrum_max_bins: SERVER_DEFAULT_MAX_BINS,
                vfo_sync: false,
                yaesu_enabled: false, yaesu2_enabled: false,
                audio_mode: 255, // default: CH0 only until client sends AudioMode
                dx_spots_enabled: true,
                allow_zoom_below_2x: false,
                smeter_sources: 0x22,
                thetis_wideband_audio: false,
                vrx1_audio_enabled: false, vrx2_audio_enabled: false,
                vrx1_spectrum_enabled: false, vrx2_spectrum_enabled: false,
                vrx1_autotune_enabled: false, vrx2_autotune_enabled: false,
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
    /// Excludes Yaesu-only clients (either Yaesu slot on + spectrum off = Android Yaesu mode).
    /// Desktop clients with yaesu+spectrum both on still receive S-meter.
    pub fn smeter_addrs(&self) -> Vec<SocketAddr> {
        self.clients.values()
            .filter(|s| (!(s.yaesu_enabled || s.yaesu2_enabled) || s.spectrum_enabled) && Self::is_active_authed(s))
            .map(|s| s.addr)
            .collect()
    }

    /// Clients that should receive the main Thetis RX-audio stream.
    /// Zelfde gate als smeter_addrs: een Android-client in Yaesu-mode (Yaesu slot on +
    /// spectrum off) luistert naar de Yaesu, niet naar Thetis -> geen Thetis-audio
    /// sturen (databesparing mobiel). Desktop met yaesu+spectrum beide aan blijft
    /// Thetis-audio krijgen. Herstelt vanzelf zodra Yaesu-mode uit gaat (spectrum aan).
    pub fn thetis_audio_addrs(&self) -> Vec<SocketAddr> {
        // Every active, authenticated client receives Thetis RX audio. The old
        // spectrum/Yaesu gate was a data-saving proxy that wrongly cut RX1 audio when a
        // Yaesu-configured desktop client turned spectrum off (spectrum != "wants Thetis
        // audio"). Not needed: a Yaesu-only setup has no Thetis configured -> no Thetis
        // audio stream exists anyway, so there is nothing to save.
        self.clients.values()
            .filter(|s| Self::is_active_authed(s))
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
                SERVER_DEFAULT_MAX_BINS
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

    /// Dual-radio slot 1 subscription-gate (Optie B-prime). Spiegel van
    /// `set_yaesu_enabled`; gezet door de `Yaesu2Enable`-control.
    pub fn set_yaesu2_enabled(&mut self, addr: SocketAddr, enabled: bool) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.yaesu2_enabled = enabled;
        }
    }

    /// VRX per-client audio-subscription (hardening fix). ch 0 = VRX1, anders VRX2.
    pub fn set_vrx_audio(&mut self, addr: SocketAddr, ch: u8, on: bool) {
        if let Some(s) = self.clients.get_mut(&addr) {
            if ch == 0 { s.vrx1_audio_enabled = on; } else { s.vrx2_audio_enabled = on; }
        }
    }

    /// VRX per-client high-res-spectrum-subscription. ch 0 = VRX1, anders VRX2.
    pub fn set_vrx_spectrum(&mut self, addr: SocketAddr, ch: u8, on: bool) {
        if let Some(s) = self.clients.get_mut(&addr) {
            if ch == 0 { s.vrx1_spectrum_enabled = on; } else { s.vrx2_spectrum_enabled = on; }
        }
    }

    /// Subscribers voor `AudioVrx` op kanaal `ch` (0=VRX1, 1=VRX2). Spiegel van
    /// `yaesu2_addrs`: alleen clients die `VrxEnable*` aan hebben gezet - oude
    /// clients krijgen nooit een `AudioVrx` packet-type.
    pub fn vrx_audio_addrs(&self, ch: u8) -> Vec<SocketAddr> {
        self.clients.values()
            .filter(|s| Self::is_active_authed(s)
                && if ch == 0 { s.vrx1_audio_enabled } else { s.vrx2_audio_enabled })
            .map(|s| s.addr)
            .collect()
    }

    /// Subscribers voor `SpectrumVrx1/2` (high-res). ch 0=VRX1, 1=VRX2.
    pub fn vrx_spectrum_addrs(&self, ch: u8) -> Vec<SocketAddr> {
        self.clients.values()
            .filter(|s| Self::is_active_authed(s)
                && if ch == 0 { s.vrx1_spectrum_enabled } else { s.vrx2_spectrum_enabled })
            .map(|s| s.addr)
            .collect()
    }

    /// VRX per-client SAM auto-tune subscription (PATCH-vrx-wide-sam-ux).
    /// ch 0 = VRX1, anders VRX2.
    pub fn set_vrx_autotune(&mut self, addr: SocketAddr, ch: u8, on: bool) {
        if let Some(s) = self.clients.get_mut(&addr) {
            if ch == 0 { s.vrx1_autotune_enabled = on; } else { s.vrx2_autotune_enabled = on; }
        }
    }

    /// Subscribers voor `FrequencyVrxActual` (SAM auto-tune follow). ch 0=VRX1,
    /// 1=VRX2. Alleen clients die `VrxSamAutoTune*` aan hebben gezet - oude
    /// clients krijgen nooit dit packet-type.
    pub fn vrx_autotune_addrs(&self, ch: u8) -> Vec<SocketAddr> {
        self.clients.values()
            .filter(|s| Self::is_active_authed(s)
                && if ch == 0 { s.vrx1_autotune_enabled } else { s.vrx2_autotune_enabled })
            .map(|s| s.addr)
            .collect()
    }

    /// Set the S-meter source-subscription bitmap for a client.
    /// See `ControlId::SmeterSources` for bit layout.
    pub fn set_smeter_sources(&mut self, addr: SocketAddr, mask: u16) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.smeter_sources = mask;
        }
    }

    /// Get a client's S-meter source-subscription bitmap (0x22 if unknown).
    pub fn smeter_sources(&self, addr: SocketAddr) -> u16 {
        self.clients.get(&addr).map(|s| s.smeter_sources).unwrap_or(0x22)
    }

    /// Enable/disable de DX-cluster spot-stream voor een client. Default ON.
    pub fn set_dx_spots_enabled(&mut self, addr: SocketAddr, enabled: bool) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.dx_spots_enabled = enabled;
        }
    }

    /// Addresses van clients die DX-spots willen ontvangen.
    pub fn dx_spots_addrs(&self) -> Vec<SocketAddr> {
        self.clients.iter()
            .filter(|(_, s)| s.dx_spots_enabled && Self::is_active_authed(s))
            .map(|(addr, _)| *addr)
            .collect()
    }

    pub fn client_audio_mode(&self, addr: SocketAddr) -> u8 {
        self.clients.get(&addr).map(|s| s.audio_mode).unwrap_or(255)
    }

    /// Per-client RX2 enable flag - defaults to `false` for unknown
    /// addrs so a half-set-up client never gets RX2 audio it didn't
    /// ask for.
    pub fn client_rx2_enabled(&self, addr: SocketAddr) -> bool {
        self.clients.get(&addr).map(|s| s.rx2_enabled).unwrap_or(false)
    }

    pub fn set_audio_mode(&mut self, addr: SocketAddr, mode: u8) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.audio_mode = mode;
        }
    }

    /// Per-client wideband-audio opt-in. Returns false voor unknown
    /// addrs (graceful default to narrowband).
    pub fn client_thetis_wideband(&self, addr: SocketAddr) -> bool {
        self.clients.get(&addr).map(|s| s.thetis_wideband_audio).unwrap_or(false)
    }

    pub fn set_thetis_wideband(&mut self, addr: SocketAddr, on: bool) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.thetis_wideband_audio = on;
        }
    }

    /// Server moet wideband encoderen zolang ten minste één actieve
    /// client de optie aan heeft staan; anders is de WB-encode-tak
    /// pure CPU-overhead.
    pub fn any_client_wants_thetis_wideband(&self) -> bool {
        self.clients.values()
            .any(|s| s.thetis_wideband_audio && Self::is_active_authed(s))
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

    /// Slot-1 subscribers (Optie B-prime). Spiegel van `yaesu_addrs`; alleen
    /// clients die `Yaesu2Enable` aan hebben gezet -> oude clients nooit.
    pub fn yaesu2_addrs(&self) -> Vec<SocketAddr> {
        self.clients.iter()
            .filter(|(_, s)| s.yaesu2_enabled && Self::is_active_authed(s))
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
                SERVER_DEFAULT_MAX_BINS
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

    /// TL2-1 ctun-auto-recenter: set per-client allow-zoom-below-2x setup-vink.
    pub fn set_allow_zoom_below_2x(&mut self, addr: SocketAddr, allow: bool) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.allow_zoom_below_2x = allow;
        }
    }

    /// TL2-1 ctun-auto-recenter: effective RX1 zoom for trigger-formula.
    /// MIN-aggregation over all spectrum-enabled clients. Returns None if no clients
    /// have RX1 spectrum enabled (no trigger-eval needed).
    ///
    /// Server-side **strictest enforce**: wanneer één of meer clients
    /// allow_zoom_below_2x=false hebben, klemt de effectieve zoom op 2.0
    /// ongeacht wat clients individueel pushen. Voorkomt dat een vink-aan-client
    /// met zoom 1.0 de feature voor andere clients kapot maakt (formule
    /// self-disabled onder zoom 1.2).
    pub fn effective_zoom_rx1(&self) -> Option<f32> {
        let raw = self.clients.values()
            .filter(|s| s.spectrum_enabled)
            .map(|s| s.spectrum_zoom)
            .fold(None, |acc, z| Some(acc.map_or(z, |a: f32| a.min(z))));
        let strictest = self.server_enforced_zoom_min();
        raw.map(|z| z.max(strictest))
    }

    /// TL2-1 ctun-auto-recenter: effective RX2 zoom for trigger-formula.
    /// Idem strictest-enforce als RX1.
    pub fn effective_zoom_rx2(&self) -> Option<f32> {
        let raw = self.clients.values()
            .filter(|s| s.rx2_spectrum_enabled)
            .map(|s| s.rx2_spectrum_zoom)
            .fold(None, |acc, z| Some(acc.map_or(z, |a: f32| a.min(z))));
        let strictest = self.server_enforced_zoom_min();
        raw.map(|z| z.max(strictest))
    }

    /// TL2-1 ctun-auto-recenter: server-enforced zoom-min for clients.
    /// Returns 1.0 only if ALL connected clients have allow_zoom_below_2x=true.
    /// Returns 2.0 (strictest) when ≥1 client has the vink uit (default).
    /// Re-applies on connect/disconnect/vink-toggle.
    pub fn server_enforced_zoom_min(&self) -> f32 {
        if self.clients.is_empty() {
            return 2.0; // no clients connected -> safe default
        }
        if self.clients.values().all(|s| s.allow_zoom_below_2x) {
            1.0
        } else {
            2.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_session(addr_str: &str, allow: bool, rx1_zoom: f32, rx2_zoom: f32, rx1_en: bool, rx2_en: bool) -> ClientSession {
        let now = Instant::now();
        ClientSession {
            addr: addr_str.parse().unwrap(),
            last_seen: now,
            connected_since: now,
            auth_state: AuthState::NoAuth,
            last_heartbeat_seq: 0, rtt_ms: 0, loss_percent: 0, jitter_ms: 0,
            spectrum_enabled: rx1_en,
            spectrum_fps: 30,
            spectrum_zoom: rx1_zoom,
            spectrum_pan: 0.0,
            spectrum_max_bins: 256,
            rx2_enabled: rx2_en, rx2_spectrum_enabled: rx2_en,
            rx2_spectrum_fps: 30,
            rx2_spectrum_zoom: rx2_zoom,
            rx2_spectrum_pan: 0.0,
            rx2_spectrum_max_bins: 256,
            vfo_sync: false, yaesu_enabled: false, yaesu2_enabled: false, audio_mode: 255,
            dx_spots_enabled: true,
            allow_zoom_below_2x: allow,
            smeter_sources: 0x22,
            thetis_wideband_audio: false,
            vrx1_audio_enabled: false, vrx2_audio_enabled: false,
            vrx1_spectrum_enabled: false, vrx2_spectrum_enabled: false,
                vrx1_autotune_enabled: false, vrx2_autotune_enabled: false,
        }
    }

    /// Unit-test: effective_zoom MIN-aggregation over multiple clients.
    /// Alle 3 clients vink-aan -> strictest=1.0 -> MIN doorgelaten.
    #[test]
    fn effective_zoom_min_aggregation() {
        let mut mgr = SessionManager::new(None, None);
        mgr.clients.insert("127.0.0.1:5001".parse().unwrap(), mk_session("127.0.0.1:5001", true, 8.0, 4.0, true, true));
        mgr.clients.insert("127.0.0.1:5002".parse().unwrap(), mk_session("127.0.0.1:5002", true, 4.0, 8.0, true, true));
        mgr.clients.insert("127.0.0.1:5003".parse().unwrap(), mk_session("127.0.0.1:5003", true, 2.0, 16.0, true, true));
        // Alle vink-aan -> strictest=1.0 -> effective = raw MIN
        // RX1: min(8, 4, 2) = 2; RX2: min(4, 8, 16) = 4
        assert_eq!(mgr.server_enforced_zoom_min(), 1.0);
        assert_eq!(mgr.effective_zoom_rx1(), Some(2.0));
        assert_eq!(mgr.effective_zoom_rx2(), Some(4.0));
    }

    #[test]
    fn effective_zoom_none_when_no_spectrum_enabled() {
        let mut mgr = SessionManager::new(None, None);
        mgr.clients.insert("127.0.0.1:5001".parse().unwrap(), mk_session("127.0.0.1:5001", true, 8.0, 4.0, false, false));
        assert_eq!(mgr.effective_zoom_rx1(), None);
        assert_eq!(mgr.effective_zoom_rx2(), None);
    }

    /// Unit-test: vink-strictest wins (zolang één client vink-uit, server zoom-min = 2.0).
    #[test]
    fn vink_strictest_wins() {
        let mut mgr = SessionManager::new(None, None);
        // 2 clients, 1 vink-aan + 1 vink-uit -> strictest = 2.0
        mgr.clients.insert("127.0.0.1:5001".parse().unwrap(), mk_session("127.0.0.1:5001", true, 8.0, 4.0, true, true));
        mgr.clients.insert("127.0.0.1:5002".parse().unwrap(), mk_session("127.0.0.1:5002", false, 4.0, 8.0, true, true));
        assert_eq!(mgr.server_enforced_zoom_min(), 2.0);

        // Beide vink-aan -> toegestaan zoom 1.0
        mgr.clients.get_mut(&"127.0.0.1:5002".parse::<SocketAddr>().unwrap()).unwrap().allow_zoom_below_2x = true;
        assert_eq!(mgr.server_enforced_zoom_min(), 1.0);

        // Reset 1 naar vink-uit -> terug naar strictest 2.0
        mgr.clients.get_mut(&"127.0.0.1:5001".parse::<SocketAddr>().unwrap()).unwrap().allow_zoom_below_2x = false;
        assert_eq!(mgr.server_enforced_zoom_min(), 2.0);
    }

    #[test]
    fn vink_strictest_no_clients_returns_safe_default() {
        let mgr = SessionManager::new(None, None);
        // Geen clients -> safe default 2.0
        assert_eq!(mgr.server_enforced_zoom_min(), 2.0);
    }

    /// Unit-test: effective_zoom moet zelf clampen op strictest-min.
    /// Mix van vink-aan + vink-uit met zoom 1.0 mag NIET 1.0 doorlaten.
    #[test]
    fn effective_zoom_clamps_to_strictest_min() {
        let mut mgr = SessionManager::new(None, None);
        // Mix: client A vink-uit zoom 8, client B vink-aan zoom 1.0
        mgr.clients.insert("127.0.0.1:5001".parse().unwrap(), mk_session("127.0.0.1:5001", false, 8.0, 8.0, true, true));
        mgr.clients.insert("127.0.0.1:5002".parse().unwrap(), mk_session("127.0.0.1:5002", true, 1.0, 1.0, true, true));
        // Strictest = 2.0 (één client vink-uit). Raw MIN = 1.0. Clamp -> 2.0.
        assert_eq!(mgr.server_enforced_zoom_min(), 2.0);
        assert_eq!(mgr.effective_zoom_rx1(), Some(2.0));
        assert_eq!(mgr.effective_zoom_rx2(), Some(2.0));

        // Beide vink-aan -> strictest 1.0, raw MIN = 1.0, doorgelaten
        mgr.clients.get_mut(&"127.0.0.1:5001".parse::<SocketAddr>().unwrap()).unwrap().allow_zoom_below_2x = true;
        assert_eq!(mgr.effective_zoom_rx1(), Some(1.0));
        assert_eq!(mgr.effective_zoom_rx2(), Some(1.0));

        // Eén client zoom 4 + ander zoom 1.0 (beide vink-aan) -> MIN 1.0
        mgr.clients.get_mut(&"127.0.0.1:5001".parse::<SocketAddr>().unwrap()).unwrap().spectrum_zoom = 4.0;
        assert_eq!(mgr.effective_zoom_rx1(), Some(1.0));

        // Zelfde maar één van twee toggle vink-uit -> clamp naar 2.0
        mgr.clients.get_mut(&"127.0.0.1:5002".parse::<SocketAddr>().unwrap()).unwrap().allow_zoom_below_2x = false;
        assert_eq!(mgr.effective_zoom_rx1(), Some(2.0));
    }
}
