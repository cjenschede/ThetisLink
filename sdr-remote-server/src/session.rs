// SPDX-License-Identifier: GPL-2.0-or-later

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::Instant;

use log::{info, warn};
use sdr_remote_core::protocol::SubscriptionMask;

/// Conservative server-side default for spectrum max_bins, before a client sends
/// its own (screen-fitting) value. Safety net so an unconfigured
/// spectrum never streams at the full DEFAULT_SPECTRUM_BINS (8192) resolution and
/// thereby inflates the data rate. Clients that want more just send their own max_bins.
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
    /// RX1 audio subscription. Default ON, so old clients (that never
    /// send `Rx1Enable`) keep receiving RX1 audio. A VRX-only client sets
    /// this to false to stop the RX1 audio stream.
    pub rx1_enabled: bool,
    pub rx2_enabled: bool,
    pub rx2_spectrum_enabled: bool,
    pub rx2_spectrum_fps: u8,
    pub rx2_spectrum_zoom: f32,
    pub rx2_spectrum_pan: f32,
    pub rx2_spectrum_max_bins: u16,
    pub vfo_sync: bool,
    pub yaesu_enabled: bool,
    /// Yaesu STATE subscription (freq/s-meter/CAT/feature/memory), SEPARATE from audio.
    /// Set via `YaesuStateEnable` when the control window is open. State goes
    /// to `yaesu_state_addrs` = (yaesu_state_enabled || yaesu_enabled), so a
    /// muted client (audio off, window open) still keeps live state.
    pub yaesu_state_enabled: bool,
    pub yaesu2_state_enabled: bool,
    /// Dual-radio slot 1 subscription-gate (PATCH-dual-radio-991a-ftx1, Option
    /// B-prime). Default false -> old clients (that never send `Yaesu2Enable`)
    /// never get slot-1 state/audio/memory. This is the real back-compat guard.
    pub yaesu2_enabled: bool,
    /// VRX per-client subscription-gates (hardening fix v2.2.0). Default
    /// false -> clients that never send `VrxEnable*`/`VrxSpectrumEnable*`
    /// (among them old v2.1.x clients) never get `AudioVrx`/`SpectrumVrx`
    /// packet-types. Same back-compat pattern as `yaesu2_enabled`.
    pub vrx1_audio_enabled: bool,
    pub vrx2_audio_enabled: bool,
    pub vrx1_spectrum_enabled: bool,
    pub vrx2_spectrum_enabled: bool,
    pub vrx1_autotune_enabled: bool,
    pub vrx2_autotune_enabled: bool,
    pub audio_mode: u8, // 255=default(CH0 only), 0=Mono, 1=BIN, 2=Split
    /// DX-cluster spot stream opt-out - default true (= stream active).
    /// When false the server no longer sends Spot frames to this
    /// client. Bandwidth saving on metered links.
    pub dx_spots_enabled: bool,
    /// Full-DDC spectrum row opt-out - default true (= second row sent).
    /// The extracted view is always sent; this only controls the extra
    /// full-band row that feeds the RX waterfall background. Off roughly
    /// halves the spectrum bandwidth per receiver.
    pub full_spectrum_enabled: bool,
    /// TL2-1 ctun-auto-recenter: per-client setup checkbox "Allow zoom below 2x".
    /// false=default (smear-free guaranteed, zoom-min 2x). true=opt-in (zoom 1x allowed).
    /// Server enforces strictest: as long as one client has false, server-zoom-min = 2x.
    pub allow_zoom_below_2x: bool,
    /// S-meter source-subscription bitmap (see `ControlId::SmeterSources` doc).
    /// Default 0x22 = RX1 Avg + RX2 Avg - matches pre-multi-source behaviour.
    pub smeter_sources: u16,
    /// Wideband-Thetis-audio opt-in: when true the server encodes
    /// RX1/RX2/BinR via wideband Opus (16 kHz, ~30 kbps/ch) instead of
    /// narrowband (8 kHz, ~14 kbps/ch) and accepts TX audio with
    /// `Flags::AUDIO_WIDEBAND` set. Default false - opt-in via
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
    /// Bumped every time a client joins. The push loops keep tick-lists of who
    /// already has each value, and prune them against the active addresses - which
    /// works when a client leaves cleanly, because it is gone from that list. It does
    /// not work when a client drops WITHOUT saying so and comes back on the same
    /// address inside the 15 s session timeout: it never left, so it stays ticked off,
    /// and a freshly started client would sit with an empty memory table and EX menu
    /// until the slow resend came round - up to a minute. Watching this number costs
    /// the loops one comparison per tick and makes "a client that joins gets the
    /// current value" true by construction rather than by inference.
    connect_generation: u64,
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
            connect_generation: 0,
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
        // Deliberately NOT bumping connect_generation here. Issuing a challenge only
        // means someone knocked: anyone reaching the port from a new address would
        // otherwise make the server re-offer the memory list and the EX settings to
        // every connected client. That is traffic an unauthenticated party should not
        // be able to cause. The bump happens where a client is actually admitted.
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
            rx1_enabled: true, // default ON (back-compat old clients)
            rx2_enabled: false, rx2_spectrum_enabled: false,
            rx2_spectrum_fps: sdr_remote_core::DEFAULT_SPECTRUM_FPS,
            rx2_spectrum_zoom: 1.0, rx2_spectrum_pan: 0.0,
            rx2_spectrum_max_bins: SERVER_DEFAULT_MAX_BINS,
            vfo_sync: false, yaesu_enabled: false, yaesu_state_enabled: false, yaesu2_state_enabled: false, yaesu2_enabled: false, audio_mode: 255,
            dx_spots_enabled: true,
            full_spectrum_enabled: true,
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
                    self.mark_admitted();
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
                    self.mark_admitted();
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
            self.mark_admitted();
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
                rx1_enabled: true, // default ON (back-compat old clients)
                rx2_enabled: false,
                rx2_spectrum_enabled: false,
                rx2_spectrum_fps: sdr_remote_core::DEFAULT_SPECTRUM_FPS,
                rx2_spectrum_zoom: 1.0,
                rx2_spectrum_pan: 0.0,
                rx2_spectrum_max_bins: SERVER_DEFAULT_MAX_BINS,
                vfo_sync: false,
                yaesu_enabled: false, yaesu_state_enabled: false, yaesu2_state_enabled: false, yaesu2_enabled: false,
                audio_mode: 255, // default: CH0 only until client sends AudioMode
                dx_spots_enabled: true,
                full_spectrum_enabled: true,
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

    /// How far a heartbeat sequence may run backwards before it means the client
    /// restarted rather than that UDP reordered a few packets. A client counts from
    /// zero, one per heartbeat, so thirty-two is many seconds of traffic - far more
    /// than any reordering, and far less than the wrap of a u32.
    const HEARTBEAT_RESTART_GAP: u32 = 32;

    /// Update heartbeat stats for a client session
    ///
    /// This is also where a SILENT restart is caught. A client that crashes or is
    /// killed sends no Disconnect, so its session stays alive for the 15 s timeout; if
    /// it comes back on the same address inside that window the server sees a known
    /// address and calls it existing. Nothing else distinguishes the two - there is no
    /// connect packet, a client simply starts sending heartbeats. But it starts them
    /// from zero, and that is the tell: a sequence that jumps far backwards is a new
    /// process behind the same address, and it needs the state a fresh subscriber gets.
    pub fn update_heartbeat(&mut self, addr: SocketAddr, seq: u32, rtt: u16, loss: u8, jitter: u8) {
        let restarted = self
            .clients
            .get(&addr)
            .is_some_and(|c| c.last_heartbeat_seq.saturating_sub(seq) > Self::HEARTBEAT_RESTART_GAP);
        if restarted {
            info!("Client {} restarted without disconnecting - serving it as new", addr);
            self.mark_admitted();
        }
        if let Some(session) = self.clients.get_mut(&addr) {
            if restarted {
                session.connected_since = Instant::now();
            }
            session.last_heartbeat_seq = seq;
            session.rtt_ms = rtt;
            session.loss_percent = loss;
            session.jitter_ms = jitter;
        }
    }

    /// Remove a client session (explicit disconnect)
    /// A client is now in and needs what a fresh subscriber gets.
    ///
    /// Exactly four things count as being admitted: a first contact with no password,
    /// an accepted password, an accepted TOTP code, and a silent restart behind an
    /// address that was already known. Issuing a challenge is not one of them - that
    /// is only someone knocking.
    fn mark_admitted(&mut self) {
        self.connect_generation = self.connect_generation.wrapping_add(1);
    }

    /// See `connect_generation`. Changes whenever anyone joins.
    pub fn connect_generation(&self) -> u64 {
        self.connect_generation
    }

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
    /// Same gate as smeter_addrs: an Android client in Yaesu mode (Yaesu slot on +
    /// spectrum off) listens to the Yaesu, not to Thetis -> don't send Thetis audio
    /// (data saving on mobile). Desktop with yaesu+spectrum both on keeps
    /// receiving Thetis audio. Recovers automatically once Yaesu mode goes off (spectrum on).
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

    /// The session behind an address, or a line in the log saying what was lost.
    ///
    /// A setting for an address with no session is dropped - there is nothing
    /// to attach it to. Dropping it *quietly* is what made a picture at the
    /// wrong scale impossible to explain: a client sends its view settings once
    /// on connect, and if they arrive a moment early the server keeps its own
    /// defaults while the client draws to its own (2026-08-14).
    ///
    /// Build 84 gave that warning to the zoom alone, and the review found the
    /// shape of it: five setters that can fall silently, one of them told to
    /// speak up. Zoom and pan go out in the same breath, so if one is dropped
    /// the other is too - and the client's mismatch check compares the *span*,
    /// which the pan does not change, so a lost pan is the one thing it can
    /// never notice by itself. Every view setting goes through here now, which
    /// is cheaper than remembering to add the warning to the next one
    /// (2026-08-15).
    fn view_session_mut(&mut self, addr: SocketAddr, what: &str) -> Option<&mut ClientSession> {
        let session = self.clients.get_mut(&addr);
        if session.is_none() {
            warn!("{} from {} arrived before its session - ignored", what, addr);
        }
        session
    }

    /// Set spectrum zoom for a client
    pub fn set_spectrum_zoom(&mut self, addr: SocketAddr, zoom: f32) {
        if let Some(session) = self.view_session_mut(addr, "RX1 spectrum zoom") {
            session.spectrum_zoom = zoom.clamp(1.0, 1024.0);
        }
    }

    /// Set spectrum pan for a client
    pub fn set_spectrum_pan(&mut self, addr: SocketAddr, pan: f32) {
        if let Some(session) = self.view_session_mut(addr, "RX1 spectrum pan") {
            session.spectrum_pan = pan.clamp(-0.5, 0.5);
        }
    }

    /// Set spectrum max bins for a client (0 = server default)
    pub fn set_spectrum_max_bins(&mut self, addr: SocketAddr, max_bins: u16) {
        if let Some(session) = self.view_session_mut(addr, "RX1 spectrum bin count") {
            session.spectrum_max_bins = if max_bins == 0 {
                SERVER_DEFAULT_MAX_BINS
            } else {
                max_bins.clamp(64, sdr_remote_core::MAX_SPECTRUM_SEND_BINS as u16)
            };
        }
    }

    /// Set RX1 audio subscription for a client
    pub fn set_rx1_enabled(&mut self, addr: SocketAddr, enabled: bool) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.rx1_enabled = enabled;
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

    /// Dual-radio slot 1 subscription-gate (Option B-prime). Mirror of
    /// `set_yaesu_enabled`; set by the `Yaesu2Enable` control.
    pub fn set_yaesu2_enabled(&mut self, addr: SocketAddr, enabled: bool) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.yaesu2_enabled = enabled;
        }
    }

    /// Yaesu STATE subscription (separate from audio), set by `YaesuStateEnable`.
    pub fn set_yaesu_state_enabled(&mut self, addr: SocketAddr, enabled: bool) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.yaesu_state_enabled = enabled;
        }
    }
    pub fn set_yaesu2_state_enabled(&mut self, addr: SocketAddr, enabled: bool) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.yaesu2_state_enabled = enabled;
        }
    }

    /// VRX per-client audio subscription (hardening fix). ch 0 = VRX1, otherwise VRX2.
    pub fn set_vrx_audio(&mut self, addr: SocketAddr, ch: u8, on: bool) {
        if let Some(s) = self.clients.get_mut(&addr) {
            if ch == 0 { s.vrx1_audio_enabled = on; } else { s.vrx2_audio_enabled = on; }
        }
    }

    /// VRX per-client high-res spectrum subscription. ch 0 = VRX1, otherwise VRX2.
    pub fn set_vrx_spectrum(&mut self, addr: SocketAddr, ch: u8, on: bool) {
        if let Some(s) = self.clients.get_mut(&addr) {
            if ch == 0 { s.vrx1_spectrum_enabled = on; } else { s.vrx2_spectrum_enabled = on; }
        }
    }

    /// Subscribers for `AudioVrx` on channel `ch` (0=VRX1, 1=VRX2). Mirror of
    /// `yaesu2_addrs`: only clients that enabled `VrxEnable*` - old
    /// clients never get an `AudioVrx` packet-type.
    pub fn vrx_audio_addrs(&self, ch: u8) -> Vec<SocketAddr> {
        self.clients.values()
            .filter(|s| Self::is_active_authed(s)
                && if ch == 0 { s.vrx1_audio_enabled } else { s.vrx2_audio_enabled })
            .map(|s| s.addr)
            .collect()
    }

    /// Subscribers for `SpectrumVrx1/2` (high-res). ch 0=VRX1, 1=VRX2.
    /// Everyone who should receive the full-DDC row of one receiver chain:
    /// the RX spectrum subscribers plus the VRX subscribers riding that same
    /// DDC (VRX1 on RX1, VRX2 on RX2). One row per client, never one per
    /// window - the client routes the same bytes to every window that wants a
    /// full-band backdrop. Clients that switched the row off are left out.
    /// `ch` 0 = RX1/VRX1, otherwise RX2/VRX2.
    pub fn full_row_clients(&self, ch: u8) -> Vec<(SocketAddr, u16, u8)> {
        self.clients.values()
            .filter(|s| Self::is_active_authed(s) && s.full_spectrum_enabled)
            .filter(|s| if ch == 0 {
                s.spectrum_enabled || s.vrx1_spectrum_enabled
            } else {
                s.rx2_spectrum_enabled || s.vrx2_spectrum_enabled
            })
            .map(|s| (
                s.addr,
                if ch == 0 { s.spectrum_max_bins } else { s.rx2_spectrum_max_bins },
                s.loss_percent,
            ))
            .collect()
    }

    pub fn vrx_spectrum_addrs(&self, ch: u8) -> Vec<SocketAddr> {
        self.clients.values()
            .filter(|s| Self::is_active_authed(s)
                && if ch == 0 { s.vrx1_spectrum_enabled } else { s.vrx2_spectrum_enabled })
            .map(|s| s.addr)
            .collect()
    }

    /// VRX per-client SAM auto-tune subscription (PATCH-vrx-wide-sam-ux).
    /// ch 0 = VRX1, otherwise VRX2.
    pub fn set_vrx_autotune(&mut self, addr: SocketAddr, ch: u8, on: bool) {
        if let Some(s) = self.clients.get_mut(&addr) {
            if ch == 0 { s.vrx1_autotune_enabled = on; } else { s.vrx2_autotune_enabled = on; }
        }
    }

    /// Subscribers for `FrequencyVrxActual` (SAM auto-tune follow). ch 0=VRX1,
    /// 1=VRX2. Only clients that enabled `VrxSamAutoTune*` - old
    /// clients never get this packet-type.
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

    /// What this client is subscribed to, as the mask that rides on the
    /// heartbeat ack.
    ///
    /// The server's own answer to "what do I think you want", so a client can
    /// notice that the two have drifted apart. It is deliberately read here and
    /// not remembered anywhere: the session IS the truth, and a second copy of
    /// it would be one more thing that can go stale.
    pub fn subscription_mask(&self, addr: SocketAddr) -> SubscriptionMask {
        let mut m = SubscriptionMask::default();
        let Some(s) = self.clients.get(&addr) else { return m };
        m.set(SubscriptionMask::RX1_AUDIO, s.rx1_enabled);
        m.set(SubscriptionMask::RX2_AUDIO, s.rx2_enabled);
        m.set(SubscriptionMask::RX1_SPECTRUM, s.spectrum_enabled);
        m.set(SubscriptionMask::RX2_SPECTRUM, s.rx2_spectrum_enabled);
        m.set(SubscriptionMask::VRX1, s.vrx1_audio_enabled);
        m.set(SubscriptionMask::VRX2, s.vrx2_audio_enabled);
        m.set(SubscriptionMask::VRX1_SPECTRUM, s.vrx1_spectrum_enabled);
        m.set(SubscriptionMask::VRX2_SPECTRUM, s.vrx2_spectrum_enabled);
        m.set(SubscriptionMask::YAESU, s.yaesu_enabled);
        m.set(SubscriptionMask::YAESU2, s.yaesu2_enabled);
        m.set(SubscriptionMask::FULL_SPECTRUM, s.full_spectrum_enabled);
        m.set(SubscriptionMask::DX_SPOTS, s.dx_spots_enabled);
        m
    }

    /// Enable/disable the DX-cluster spot-stream for a client. Default ON.
    pub fn set_dx_spots_enabled(&mut self, addr: SocketAddr, enabled: bool) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.dx_spots_enabled = enabled;
        }
    }

    pub fn set_full_spectrum_enabled(&mut self, addr: SocketAddr, enabled: bool) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.full_spectrum_enabled = enabled;
        }
    }


    /// Addresses of clients that want to receive DX-spots.
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

    /// RX1 audio subscription for a client. Default ON (`true`) for an
    /// unknown/half-set-up client, so old clients keep RX1 audio.
    pub fn client_rx1_enabled(&self, addr: SocketAddr) -> bool {
        self.clients.get(&addr).map(|s| s.rx1_enabled).unwrap_or(true)
    }

    pub fn set_audio_mode(&mut self, addr: SocketAddr, mode: u8) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.audio_mode = mode;
        }
    }

    /// Per-client wideband-audio opt-in. Returns false for unknown
    /// addrs (graceful default to narrowband).
    pub fn client_thetis_wideband(&self, addr: SocketAddr) -> bool {
        self.clients.get(&addr).map(|s| s.thetis_wideband_audio).unwrap_or(false)
    }

    pub fn set_thetis_wideband(&mut self, addr: SocketAddr, on: bool) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.thetis_wideband_audio = on;
        }
    }

    /// Server must encode wideband as long as at least one active
    /// client has the option enabled; otherwise the WB-encode branch
    /// is pure CPU overhead.
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

    /// Slot-0 STATE subscribers: window-open (yaesu_state_enabled) OR audio subscriber
    /// (yaesu_enabled). This way a muted client with an open window keeps live state, and
    /// audio subscribers get state anyway (no separate opt-in needed).
    pub fn yaesu_state_addrs(&self) -> Vec<SocketAddr> {
        self.clients.iter()
            .filter(|(_, s)| (s.yaesu_state_enabled || s.yaesu_enabled) && Self::is_active_authed(s))
            .map(|(addr, _)| *addr)
            .collect()
    }
    pub fn yaesu2_state_addrs(&self) -> Vec<SocketAddr> {
        self.clients.iter()
            .filter(|(_, s)| (s.yaesu2_state_enabled || s.yaesu2_enabled) && Self::is_active_authed(s))
            .map(|(addr, _)| *addr)
            .collect()
    }

    /// Slot-1 subscribers (Option B-prime). Mirror of `yaesu_addrs`; only
    /// clients that enabled `Yaesu2Enable` -> old clients never.
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
        if let Some(session) = self.view_session_mut(addr, "RX2 spectrum bin count") {
            session.rx2_spectrum_max_bins = if max_bins == 0 {
                SERVER_DEFAULT_MAX_BINS
            } else {
                max_bins.clamp(64, sdr_remote_core::MAX_SPECTRUM_SEND_BINS as u16)
            };
        }
    }

    /// Set RX2 spectrum zoom for a client
    pub fn set_rx2_spectrum_zoom(&mut self, addr: SocketAddr, zoom: f32) {
        if let Some(session) = self.view_session_mut(addr, "RX2 spectrum zoom") {
            session.rx2_spectrum_zoom = zoom.clamp(1.0, 1024.0);
        }
    }

    /// Set RX2 spectrum pan for a client
    pub fn set_rx2_spectrum_pan(&mut self, addr: SocketAddr, pan: f32) {
        if let Some(session) = self.view_session_mut(addr, "RX2 spectrum pan") {
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

    /// Get RX2 spectrum clients: (addr, zoom, pan, max_bins). Spectrum subscription
    /// is SEPARATE from the RX2 audio subscription (`rx2_enabled`) — a client may want the
    /// RX2 spectrum without RX2 audio (save bandwidth). Phase 3b.
    pub fn rx2_spectrum_clients(&self) -> Vec<(SocketAddr, f32, f32, u16)> {
        self.clients.values()
            .filter(|s| s.rx2_spectrum_enabled && Self::is_active_authed(s))
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

    /// Get addresses of RX2 clients with spectrum enabled (for S-meter gating).
    /// Separate from `rx2_enabled` (audio) — see `rx2_spectrum_clients`. Phase 3b.
    pub fn rx2_spectrum_addrs(&self) -> Vec<SocketAddr> {
        self.clients.values()
            .filter(|s| s.rx2_spectrum_enabled && Self::is_active_authed(s))
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

    /// Is this client still in the opening seconds of its connection?
    ///
    /// A client sends its whole state as soon as it is admitted, so everything
    /// that arrives in that window restates what it came with rather than
    /// reporting a change. Callers use it to keep that dump out of the log at
    /// INFO. An unknown address counts as NOT opening: something arriving
    /// without a session is not a snapshot.
    pub fn opening_burst(&self, addr: SocketAddr) -> bool {
        self.clients
            .get(&addr)
            .map(|c| c.connected_since.elapsed() < std::time::Duration::from_secs(3))
            .unwrap_or(false)
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

    /// TL2-1 ctun-auto-recenter: set per-client allow-zoom-below-2x setup checkbox.
    pub fn set_allow_zoom_below_2x(&mut self, addr: SocketAddr, allow: bool) {
        if let Some(session) = self.clients.get_mut(&addr) {
            session.allow_zoom_below_2x = allow;
        }
    }

    /// TL2-1 ctun-auto-recenter: effective RX1 zoom for trigger-formula.
    /// MIN-aggregation over all spectrum-enabled clients. Returns None if no clients
    /// have RX1 spectrum enabled (no trigger-eval needed).
    ///
    /// Server-side **strictest enforce**: when one or more clients
    /// have allow_zoom_below_2x=false, the effective zoom is clamped to 2.0
    /// regardless of what clients individually push. Prevents a checkbox-on client
    /// with zoom 1.0 from breaking the feature for other clients (formula
    /// self-disables below zoom 1.2).
    pub fn effective_zoom_rx1(&self) -> Option<f32> {
        let raw = self.clients.values()
            .filter(|s| s.spectrum_enabled)
            .map(|s| s.spectrum_zoom)
            .fold(None, |acc, z| Some(acc.map_or(z, |a: f32| a.min(z))));
        let strictest = self.server_enforced_zoom_min();
        raw.map(|z| z.max(strictest))
    }

    /// TL2-1 ctun-auto-recenter: effective RX2 zoom for trigger-formula.
    /// Same strictest-enforce as RX1.
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
    /// Returns 2.0 (strictest) when ≥1 client has the checkbox off (default).
    /// Re-applies on connect/disconnect/checkbox-toggle.
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
            rx1_enabled: true,
            rx2_enabled: rx2_en, rx2_spectrum_enabled: rx2_en,
            rx2_spectrum_fps: 30,
            rx2_spectrum_zoom: rx2_zoom,
            rx2_spectrum_pan: 0.0,
            rx2_spectrum_max_bins: 256,
            vfo_sync: false, yaesu_enabled: false, yaesu_state_enabled: false, yaesu2_state_enabled: false, yaesu2_enabled: false, audio_mode: 255,
            dx_spots_enabled: true,
            full_spectrum_enabled: true,
            allow_zoom_below_2x: allow,
            smeter_sources: 0x22,
            thetis_wideband_audio: false,
            vrx1_audio_enabled: false, vrx2_audio_enabled: false,
            vrx1_spectrum_enabled: false, vrx2_spectrum_enabled: false,
                vrx1_autotune_enabled: false, vrx2_autotune_enabled: false,
        }
    }

    /// Unit-test: effective_zoom MIN-aggregation over multiple clients.
    /// All 3 clients checkbox-on -> strictest=1.0 -> MIN passed through.
    #[test]
    fn effective_zoom_min_aggregation() {
        let mut mgr = SessionManager::new(None, None);
        mgr.clients.insert("127.0.0.1:5001".parse().unwrap(), mk_session("127.0.0.1:5001", true, 8.0, 4.0, true, true));
        mgr.clients.insert("127.0.0.1:5002".parse().unwrap(), mk_session("127.0.0.1:5002", true, 4.0, 8.0, true, true));
        mgr.clients.insert("127.0.0.1:5003".parse().unwrap(), mk_session("127.0.0.1:5003", true, 2.0, 16.0, true, true));
        // All checkbox-on -> strictest=1.0 -> effective = raw MIN
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

    /// Unit-test: checkbox-strictest wins (as long as one client checkbox-off, server zoom-min = 2.0).
    #[test]
    fn vink_strictest_wins() {
        let mut mgr = SessionManager::new(None, None);
        // 2 clients, 1 checkbox-on + 1 checkbox-off -> strictest = 2.0
        mgr.clients.insert("127.0.0.1:5001".parse().unwrap(), mk_session("127.0.0.1:5001", true, 8.0, 4.0, true, true));
        mgr.clients.insert("127.0.0.1:5002".parse().unwrap(), mk_session("127.0.0.1:5002", false, 4.0, 8.0, true, true));
        assert_eq!(mgr.server_enforced_zoom_min(), 2.0);

        // Both checkbox-on -> allowed zoom 1.0
        mgr.clients.get_mut(&"127.0.0.1:5002".parse::<SocketAddr>().unwrap()).unwrap().allow_zoom_below_2x = true;
        assert_eq!(mgr.server_enforced_zoom_min(), 1.0);

        // Reset 1 to checkbox-off -> back to strictest 2.0
        mgr.clients.get_mut(&"127.0.0.1:5001".parse::<SocketAddr>().unwrap()).unwrap().allow_zoom_below_2x = false;
        assert_eq!(mgr.server_enforced_zoom_min(), 2.0);
    }

    #[test]
    fn vink_strictest_no_clients_returns_safe_default() {
        let mgr = SessionManager::new(None, None);
        // No clients -> safe default 2.0
        assert_eq!(mgr.server_enforced_zoom_min(), 2.0);
    }

    /// Unit-test: effective_zoom must clamp itself to strictest-min.
    /// Mix of checkbox-on + checkbox-off with zoom 1.0 must NOT pass 1.0 through.
    #[test]
    fn effective_zoom_clamps_to_strictest_min() {
        let mut mgr = SessionManager::new(None, None);
        // Mix: client A checkbox-off zoom 8, client B checkbox-on zoom 1.0
        mgr.clients.insert("127.0.0.1:5001".parse().unwrap(), mk_session("127.0.0.1:5001", false, 8.0, 8.0, true, true));
        mgr.clients.insert("127.0.0.1:5002".parse().unwrap(), mk_session("127.0.0.1:5002", true, 1.0, 1.0, true, true));
        // Strictest = 2.0 (one client checkbox-off). Raw MIN = 1.0. Clamp -> 2.0.
        assert_eq!(mgr.server_enforced_zoom_min(), 2.0);
        assert_eq!(mgr.effective_zoom_rx1(), Some(2.0));
        assert_eq!(mgr.effective_zoom_rx2(), Some(2.0));

        // Both checkbox-on -> strictest 1.0, raw MIN = 1.0, passed through
        mgr.clients.get_mut(&"127.0.0.1:5001".parse::<SocketAddr>().unwrap()).unwrap().allow_zoom_below_2x = true;
        assert_eq!(mgr.effective_zoom_rx1(), Some(1.0));
        assert_eq!(mgr.effective_zoom_rx2(), Some(1.0));

        // One client zoom 4 + other zoom 1.0 (both checkbox-on) -> MIN 1.0
        mgr.clients.get_mut(&"127.0.0.1:5001".parse::<SocketAddr>().unwrap()).unwrap().spectrum_zoom = 4.0;
        assert_eq!(mgr.effective_zoom_rx1(), Some(1.0));

        // Same but one of two toggles checkbox-off -> clamp to 2.0
        mgr.clients.get_mut(&"127.0.0.1:5002".parse::<SocketAddr>().unwrap()).unwrap().allow_zoom_below_2x = false;
        assert_eq!(mgr.effective_zoom_rx1(), Some(2.0));
    }
}

#[cfg(test)]
mod connect_generation_tests {
    use super::*;

    fn mgr() -> SessionManager {
        SessionManager::new(None, None)
    }

    const A: &str = "127.0.0.1:5001";

    /// The number has to move when a client is admitted, or the push loops have
    /// nothing to notice.
    #[test]
    fn being_admitted_moves_the_generation() {
        let mut m = mgr();
        let before = m.connect_generation();
        assert_eq!(m.touch(A.parse().unwrap()), TouchResult::NewClient);
        assert_ne!(m.connect_generation(), before);
    }

    /// The case this exists for: a client drops WITHOUT saying so and comes back on
    /// the same address before the session times out. It never left, so pruning the
    /// tick-lists against the active addresses cannot see it, and `touch` calls it
    /// existing - but its heartbeat sequence restarts from zero, and that is the tell.
    #[test]
    fn a_silent_restart_on_the_same_address_moves_it_again() {
        let mut m = mgr();
        let a = A.parse().unwrap();
        m.touch(a);
        m.update_heartbeat(a, 400, 0, 0, 0);
        let settled = m.connect_generation();
        m.update_heartbeat(a, 0, 0, 0, 0); // a new process behind the same address
        assert_ne!(
            m.connect_generation(), settled,
            "a restarted client must be served as new"
        );
    }

    /// A few packets arriving out of order are not a restart. Treating them as one
    /// would re-send the memory list and the EX settings to every client over a
    /// reordered heartbeat.
    #[test]
    fn a_reordered_heartbeat_is_not_a_restart() {
        let mut m = mgr();
        let a = A.parse().unwrap();
        m.touch(a);
        m.update_heartbeat(a, 400, 0, 0, 0);
        let settled = m.connect_generation();
        m.update_heartbeat(a, 397, 0, 0, 0);
        assert_eq!(m.connect_generation(), settled);
    }

    /// The opening window covers a client that has just arrived, and nothing
    /// else. An address with no session at all is not "opening" - it has sent
    /// no snapshot, so whatever it sends is a real event and belongs in the log.
    #[test]
    fn only_a_client_that_just_arrived_is_opening() {
        let mut m = SessionManager::new(Some("secret".into()), None);
        let a: std::net::SocketAddr = A.parse().unwrap();
        assert!(!m.opening_burst(a), "no session yet is not an opening burst");
        let _ = m.create_challenge(a);
        assert!(m.opening_burst(a), "a client that just knocked is opening");
        let b: std::net::SocketAddr = "10.0.0.9:1234".parse().unwrap();
        assert!(!m.opening_burst(b), "another address is not covered by it");
    }

    /// Knocking is not joining. Issuing a challenge must not move the number: anyone
    /// reaching the port from a new address could otherwise make the server re-offer
    /// the memory list and the EX settings to every connected client.
    #[test]
    fn knocking_does_not_move_it() {
        let mut m = SessionManager::new(Some("secret".into()), None);
        let settled = m.connect_generation();
        let _ = m.create_challenge(A.parse().unwrap());
        assert_eq!(m.connect_generation(), settled);
    }

    /// An accepted password admits exactly once. It used to bump twice, because the
    /// increment was written out by hand at each site.
    #[test]
    fn an_accepted_password_admits_exactly_once() {
        let mut m = SessionManager::new(Some("secret".into()), None);
        let a = A.parse().unwrap();
        let nonce = m.create_challenge(a);
        let before = m.connect_generation();
        let resp = sdr_remote_core::auth::compute_hmac("secret", &nonce);
        assert_eq!(
            m.verify_auth(a, &resp),
            sdr_remote_core::protocol::AUTH_ACCEPTED
        );
        assert_eq!(m.connect_generation(), before + 1);
    }

    /// And a 2FA client is admitted by the TOTP step, not by the password step - that
    /// path had no bump at all, so a client behind 2FA joined invisibly.
    #[test]
    fn a_totp_client_is_admitted_when_the_code_is_accepted() {
        let secret = sdr_remote_core::auth::generate_totp_secret();
        let mut m = SessionManager::new(Some("secret".into()), Some(secret.clone()));
        let a = A.parse().unwrap();
        let nonce = m.create_challenge(a);
        let resp = sdr_remote_core::auth::compute_hmac("secret", &nonce);
        assert_eq!(
            m.verify_auth(a, &resp),
            sdr_remote_core::protocol::AUTH_TOTP_REQUIRED,
            "the password step must not admit a 2FA client"
        );
        let before = m.connect_generation();
        let code = sdr_remote_core::auth::generate_totp(&secret);
        assert!(m.verify_totp(a, &code));
        assert_eq!(m.connect_generation(), before + 1, "the TOTP step admits");
    }

    /// Leaving is not joining either.
    #[test]
    fn leaving_leaves_it_alone() {
        let mut m = mgr();
        let a = A.parse().unwrap();
        m.touch(a);
        let settled = m.connect_generation();
        m.remove(a);
        assert_eq!(m.connect_generation(), settled);
    }
}
