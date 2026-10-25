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

/// The deadline for a client at the 2FA code step. It sends no heartbeats while
/// the operator types (the engine holds them until the code is in) and cannot
/// refresh its session before it is authenticated, so a listener's fifteen
/// seconds reaped it mid-typing - after which its code got, since build 68, no
/// answer at all, and the client waited at the code field for ever (review 70).
/// Two minutes is four TOTP periods.
const TOTP_ENTRY_TIMEOUT_SECS: u64 = 120;

/// How old a client's code step must be before a heartbeat from its address is
/// a knock. The client pauses its heartbeats once it has HEARD it is at the
/// code step - and the server is at that step from the moment it verified the
/// password, so on a slow line the client's next heartbeat (half a second after
/// the one that started the handshake) lands on its own fresh code step and, in
/// build 73, replaced it: a second challenge, the new key thrown away, and a
/// code typed in that window answering nothing (review 73, measured at 600 ms
/// of flight). Two seconds is above one heartbeat interval on any line and far
/// under the time it takes to type a code; a restart or a relay reconnect is
/// seconds later.
const TOTP_KNOCK_GRACE_SECS: u64 = 2;

/// The deadline for a client that is holding a transmitter.
///
/// Fifteen seconds is a reasonable time to miss a listener. It is not a
/// reasonable time to leave a radio keyed with nobody there: a link that dies
/// mid-transmission left a 991A on the air for twenty seconds, measured
/// 2026-09-03. Heartbeats arrive every 500 ms, so three seconds is six of them -
/// past any ordinary hiccup - and it turns that twenty seconds into three.
///
/// Thetis has had this all along in another form: 500 ms without PTT packets and
/// it lets go. This is the same idea, kept slower because a Yaesu's PTT is a
/// command and not a stream, so there is nothing here that arrives every 20 ms.
/// Equal to the CW playout's WATCHDOG by coincidence, not by design: which of the
/// two lifts the key of a client that died is then a race, and both outcomes are
/// right (step-4 brief, the time-out row in section 2.6). The safety order is
/// pinned in ptt.rs: `mox_drops_before_the_transmitter_is_given_away_or_the_key_lifted`.
/// Since build 265 a keyed radio lets a silent holder go at 2 s
/// (`release_silent_radio`), and Thetis since 251; this is what is left for a
/// holder neither of those released - a CW run, a radio not yet keyed.
const TX_HOLDER_TIMEOUT_SECS: u64 = 3;

/// Has a client been silent longer than its deadline? Compared at full
/// precision: `elapsed().as_secs() > N` rounds down and only holds from N + 1
/// seconds on, which made the three-second deadline of a TX holder four and
/// the fifteen of a listener sixteen. The one place both the time-out and the
/// check for an active session ask.
pub fn silent_too_long(silence: std::time::Duration, holds_transmitter: bool, at_the_code_step: bool) -> bool {
    silence > std::time::Duration::from_secs(timeout_secs(holds_transmitter, at_the_code_step))
}

/// How long this client may be silent before it is dropped: a holder soonest,
/// a client typing its 2FA code longest, everyone else the listener's fifteen.
pub fn timeout_secs(holds_transmitter: bool, at_the_code_step: bool) -> u64 {
    if holds_transmitter {
        TX_HOLDER_TIMEOUT_SECS
    } else if at_the_code_step {
        TOTP_ENTRY_TIMEOUT_SECS
    } else {
        SESSION_TIMEOUT_SECS
    }
}

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
    /// Which build this client says it is. `Unknown` until its first heartbeat.
    pub client_build: sdr_remote_core::protocol::PeerBuild,
    /// Which way its traffic takes (build 143); `None` without a [`WayLookup`]
    /// or once its connection is gone.
    pub path: Option<sdr_remote_core::path::PathKind>,
}

/// What the password step answers: the code for the client, and - for a client
/// that sent its nonce - the proof that this server knows the password.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthVerdict {
    pub code: u8,
    pub proof: Option<[u8; 32]>,
}

/// A code the server never sends: the answer to a response with nothing
/// pending for its address (the challenge expired, the entry was reaped, the
/// client is past this step). Nothing goes back - a REJECTED read as "wrong
/// password" on the client's screen, and with a slow answer that was a loop -
/// and the next packet from the address gets a fresh challenge the usual way
/// (review 67).
pub const AUTH_NOT_ANSWERED: u8 = 0xff;

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

/// What an authentication came over: a QUIC connection's binding and the
/// peer's real address. Since build 133 there is always a binding - plain UDP
/// and the classic relay, which had none, are gone - so there is no value
/// without one to pass (no `Default`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthChannel {
    /// The connection's channel binding; the answer, key and proof cover it.
    pub binding: [u8; sdr_remote_core::auth::BINDING_SIZE],
    /// Where the brake on password guessing counts instead of the session's
    /// address (a QUIC sentinel is new for every connection).
    pub peer: Option<SocketAddr>,
}

/// The binding the session tests authenticate over: any value does, as long as
/// the answer and the channel name the same one.
#[cfg(test)]
const TEST_BINDING: [u8; sdr_remote_core::auth::BINDING_SIZE] = [7; sdr_remote_core::auth::BINDING_SIZE];

/// A channel with [`TEST_BINDING`], for the session tests.
#[cfg(test)]
fn bound() -> AuthChannel {
    AuthChannel { binding: TEST_BINDING, peer: None }
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
    /// The key of this session, derived at the password step with a client that
    /// sent its nonce (phase 2 of the direct connection); `None` for an older
    /// client. Nothing reads it before phase 5. Server-owned: it IS the session,
    /// and a new session gets a new one.
    pub session_key: Option<[u8; 32]>,
    /// Which numbered controls this session applied, per control (phase 2b,
    /// `control_ack`). Server-owned: a new session starts empty, and a client
    /// that restarted behind the same address is reset with it.
    pub controls: sdr_remote_core::control_ack::ControlReceiver,
    pub last_heartbeat_seq: u32,
    pub rtt_ms: u16,
    pub loss_percent: u8,
    pub jitter_ms: u8,
    /// Which build this client says it is, judged against this server's.
    ///
    /// Server-owned in the sense of the gate at the bottom of this file: no
    /// control restores it, because the client repeats it on every heartbeat
    /// of its own accord. Kept so the log can say it once instead of twice a
    /// second, and so the Status panel can show it.
    pub client_build: sdr_remote_core::protocol::PeerBuild,
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

/// A transmitter the server can hand audio to.
///
/// One radio has one TX audio channel, one client has one, and the server never
/// sends two channels into one radio - there is no mixing. So a target is an
/// exclusive resource with at most one owner, and the same rule holds for every
/// radio rather than only for Thetis.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TxTarget {
    Thetis,
    /// The Yaesu in slot 0 - a 991A or an FTX-1, whichever is configured there.
    Yaesu1,
    /// The Yaesu in slot 1, for a second radio.
    Yaesu2,
}

/// Why a request for a transmitter was refused - the log's words, and what
/// the refusal tells the client. See `SessionManager::acquire_refusal`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcquireRefusal {
    pub why: &'static str,
    pub denial: sdr_remote_core::protocol::Denial,
}

/// What `SessionManager::close_tx` leaves behind: who held what, and who has
/// to be told. `tell` is every active client, holder or not, taken under the
/// same lock as the close and handed out only by the close - so a client can
/// only be told AFTER the server is closed, and the list cannot be fetched
/// from anywhere else (review 61: the loop that told everyone could be moved
/// before the close, or run over an empty list, with every check green).
pub struct TxClosed {
    pub held: Vec<(TxTarget, SocketAddr)>,
    pub tell: Vec<SocketAddr>,
}

impl TxTarget {
    pub const ALL: [TxTarget; 3] = [TxTarget::Thetis, TxTarget::Yaesu1, TxTarget::Yaesu2];

    /// The name the log gives this transmitter.
    pub fn radio_name(self) -> &'static str {
        match self {
            TxTarget::Thetis => "Thetis",
            TxTarget::Yaesu1 => "Yaesu radio 0",
            TxTarget::Yaesu2 => "Yaesu radio 1",
        }
    }

    /// The transmitter a CW key edge is for, by the number the packet carries
    /// (`CwEdgesPacket::target`: 0 Thetis, 1 and 2 the Yaesu slots). Anything
    /// else is no transmitter at all, and the caller refuses it.
    pub fn from_cw_target(target: u8) -> Option<TxTarget> {
        match target {
            0 => Some(TxTarget::Thetis),
            1 => Some(TxTarget::Yaesu1),
            2 => Some(TxTarget::Yaesu2),
            _ => None,
        }
    }
}

#[cfg(test)]
mod cw_target_tests {
    use super::TxTarget;

    #[test]
    fn a_cw_target_names_one_transmitter_or_none() {
        assert_eq!(TxTarget::from_cw_target(0), Some(TxTarget::Thetis));
        assert_eq!(TxTarget::from_cw_target(1), Some(TxTarget::Yaesu1));
        assert_eq!(TxTarget::from_cw_target(2), Some(TxTarget::Yaesu2));
        assert_eq!(TxTarget::from_cw_target(3), None);
        assert_eq!(TxTarget::from_cw_target(255), None);
    }
}

/// Manages connected client sessions.
/// Supports multiple simultaneous clients with single-TX arbitration.
pub struct SessionManager {
    clients: HashMap<SocketAddr, ClientSession>,
    /// Told when a client is admitted and when its session ends
    /// ([`AdmissionWatch`]). `None` until the network layer sets it.
    admission: Option<std::sync::Arc<dyn AdmissionWatch>>,
    /// Asked which way a client's traffic takes, for the snapshot
    /// ([`WayLookup`]). `None` until the network layer sets it.
    ways: Option<std::sync::Arc<dyn WayLookup>>,
    /// Who holds each transmitter, if anyone.
    ///
    /// Was a single `Option<SocketAddr>` for Thetis. The Yaesu paths had no
    /// owner at all: any client's audio went to the radio while the PTT flag was
    /// up, and any client could raise or drop that flag - so two people could
    /// talk into one radio, and one could end the other's transmission.
    tx_holders: std::collections::HashMap<TxTarget, SocketAddr>,
    /// Transmitting is over for this server run (`close_tx`): every request for
    /// a transmitter is refused until the server is restarted.
    tx_closed: bool,
    /// When a radio's holder was last heard on that radio (build 282): its
    /// PTT rides on the audio, so the 2 s clock watches that audio, as Thetis's
    /// safety watches its PTT packets - not the heartbeat, which goes on while
    /// nothing keys the radio.
    radio_heard: std::collections::HashMap<TxTarget, Instant>,
    /// A radio that let go of its holder by itself - its own time-out timer,
    /// or the radio stopping (build 282). The holder's packets still carry the
    /// flag until it hears of it, and must not key the radio again: they are
    /// ignored until a packet without the flag says it let go.
    let_go_by_radio: std::collections::HashMap<TxTarget, SocketAddr>,
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

/// Who must know when a client is admitted and when its session ends. Over
/// QUIC that is its permit to punch a hole (phase 3c): granted at admission,
/// taken back when the session goes - the session layer knows both moments,
/// so it is the one source (build 127, review 126: the permit used to be
/// granted at three places in the network layer and outlived the session).
///
/// Called with the session lock held: an implementation must not block and
/// must not call back into the session manager (review 127).
pub trait AdmissionWatch: Send + Sync {
    fn admitted(&self, addr: SocketAddr);
    fn gone(&self, addr: SocketAddr);
}

/// Which way a client's traffic takes now (build 143, the connection names):
/// the transport knows it, the session layer hands it to the status panel.
/// Called with the session lock held, like [`AdmissionWatch`].
pub trait WayLookup: Send + Sync {
    fn way(&self, addr: SocketAddr) -> Option<sdr_remote_core::path::PathKind>;
}

impl SessionManager {
    /// Tell `watch` of every admission and every session that ends, from now on.
    pub fn watch_admissions(&mut self, watch: std::sync::Arc<dyn AdmissionWatch>) {
        self.admission = Some(watch);
    }

    /// Ask `ways` which way each client takes, for the snapshot, from now on.
    pub fn see_ways(&mut self, ways: std::sync::Arc<dyn WayLookup>) {
        self.ways = Some(ways);
    }

    /// The one way a session ends: removed, and the watch told.
    fn forget_client(&mut self, addr: SocketAddr) {
        self.clients.remove(&addr);
        self.let_go_by_radio.retain(|_, a| *a != addr);
        if let Some(w) = &self.admission {
            w.gone(addr);
        }
    }

    pub fn new(password: Option<String>, totp_secret: Option<String>) -> Self {
        if password.is_some() {
            info!("Authentication enabled (password configured)");
        } else {
            warn!("No password configured - every client that reaches the port is admitted without one");
        }
        if totp_secret.is_some() {
            info!("2FA enabled (TOTP configured)");
        }
        Self {
            clients: HashMap::new(),
            admission: None,
            ways: None,
            tx_holders: std::collections::HashMap::new(),
            tx_closed: false,
            radio_heard: std::collections::HashMap::new(),
            let_go_by_radio: std::collections::HashMap::new(),
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
                client_build: c.client_build.clone(),
                path: self.ways.as_ref().and_then(|w| w.way(c.addr)),
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

    /// Step 4 (build 256, a development build only): the trial client is let in
    /// as authenticated, without a password - its packets come from the server
    /// itself (`trial.rs`), never from the network. It asks for no receive audio.
    /// `true` when it was let in now, `false` when it already was.
    #[cfg(any(test, feature = "naspelen"))]
    pub fn admit_trial_client(&mut self, addr: SocketAddr) -> bool {
        if self.is_authenticated(addr) {
            return false;
        }
        self.create_challenge(addr);
        if let Some(s) = self.clients.get_mut(&addr) {
            s.auth_state = AuthState::Authenticated;
            s.last_seen = Instant::now();
            s.rx1_enabled = false;
        }
        self.mark_admitted(addr);
        true
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

    /// Does this packet from this address get a challenge? A stranger does; a
    /// client whose challenge is pending does not (it is answering it), nor does
    /// an authenticated one. A client at the code step: only for a **heartbeat**
    /// on a code step older than `TOTP_KNOCK_GRACE_SECS`. Such a client sends no
    /// heartbeats while the operator types, so a heartbeat from its address is a
    /// new client - a restart, or over the relay the same fixed address - and
    /// keeping the old attempt locked that address out for the whole code-step
    /// deadline, two minutes since build 71 (review 71). But it does send what
    /// the operator touches meanwhile - a control, the VFO - and in build 72 one
    /// of those counted as a knock and wiped the attempt (review 72, measured);
    /// and its own last heartbeat, sent before it heard of the code step, did
    /// the same in build 73 on a slow line (review 73, measured). The packet
    /// itself is judged here, not a flag the caller derives: a flag let the
    /// wiring in `network.rs` say `true` or `false` with every test green
    /// (review 73). The `NoAuth` arm is defensive: the caller asks only when a
    /// password is set, and `NoAuth` exists only without one.
    ///
    /// A `Disconnect` never does: somebody leaving is not somebody arriving. A
    /// closed QUIC connection reads as one, also after its client has already
    /// said goodbye itself, and that challenge then lingered as a half login
    /// until the session timeout (field test, build 111).
    pub fn wants_challenge(&self, addr: SocketAddr, packet: &sdr_remote_core::protocol::Packet) -> bool {
        if matches!(packet, sdr_remote_core::protocol::Packet::Disconnect) {
            return false;
        }
        match self.clients.get(&addr) {
            None => true,
            Some(s) => match s.auth_state {
                AuthState::NoAuth => true,
                AuthState::PendingTotp => {
                    matches!(packet, sdr_remote_core::protocol::Packet::Heartbeat(_))
                        && s.last_seen.elapsed() >= std::time::Duration::from_secs(TOTP_KNOCK_GRACE_SECS)
                }
                AuthState::PendingChallenge { .. } | AuthState::Authenticated => false,
            },
        }
    }

    /// Create a pending challenge for a new client. Returns the nonce.
    pub fn create_challenge(&mut self, addr: SocketAddr) -> [u8; 16] {
        if matches!(self.get_auth_state(addr), Some(AuthState::PendingTotp)) {
            info!("Client {} knocked again at the code step - the old attempt is replaced by a new challenge", addr);
        }
        // A session this replaces ends the one way a session ends, so its
        // watch hears it - true by itself, not by the gate two layers up that
        // only lets unadmitted clients get here (review 127).
        if self.clients.contains_key(&addr) {
            self.forget_client(addr);
        }
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
            session_key: None,
            controls: sdr_remote_core::control_ack::ControlReceiver::new(),
            last_heartbeat_seq: 0, rtt_ms: 0, loss_percent: 0, jitter_ms: 0,
            client_build: sdr_remote_core::protocol::PeerBuild::Unknown,
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

    /// Verify the HMAC auth response. The code is for the client:
    /// - 0 = rejected
    /// - 1 = accepted (fully authenticated)
    /// - 2 = HMAC ok, TOTP required (pending 2FA)
    ///
    /// With a `client_nonce` (phase 2) the accepted and TOTP-required answers
    /// carry the proof that this server knows the password, and the session
    /// gets its key; without one it is the answer from before.
    ///
    /// `channel` carries the binding of the QUIC connection the response came
    /// in on and the peer's real address (`TrackedSocket::auth_channel`). The
    /// client's HMAC, the key and the proof all cover the binding,
    /// so an answer made on another TLS session - someone in the middle - does
    /// not verify. A session is keyed by its sentinel and every QUIC connection
    /// has its own, so a key never outlives the binding it was made with.
    pub fn verify_auth(
        &mut self,
        addr: SocketAddr,
        hmac: &[u8; 32],
        client_nonce: Option<[u8; 16]>,
        channel: AuthChannel,
    ) -> AuthVerdict {
        let binding = Some(channel.binding);
        // Failures count on the real peer over QUIC, on the address otherwise.
        let throttle = channel.peer.unwrap_or(addr);
        use sdr_remote_core::auth::{server_proof, session_key, verify_hmac, CHALLENGE_TTL_SECS};
        use sdr_remote_core::protocol::{AUTH_ACCEPTED, AUTH_REJECTED, AUTH_TOTP_REQUIRED};
        let refused = AuthVerdict { code: AUTH_REJECTED, proof: None };
        let not_answered = AuthVerdict { code: AUTH_NOT_ANSWERED, proof: None };
        let password = match &self.password {
            Some(p) => p.clone(),
            None => return refused,
        };
        if let Some(session) = self.clients.get_mut(&addr) {
            if let AuthState::PendingChallenge { nonce, sent_at } = &session.auth_state {
                let nonce = *nonce;
                let age = sent_at.elapsed().as_secs();
                if age >= CHALLENGE_TTL_SECS {
                    // A slow answer is not a wrong one: nothing counted, the
                    // entry goes, and the next packet from this address gets a
                    // fresh challenge the usual way. Until phase 2 a challenge
                    // never expired at all.
                    info!("Auth challenge for {} expired after {}s - a new one follows", addr, age);
                    self.forget_client(addr);
                    return not_answered;
                }
                if verify_hmac(&password, &nonce, hmac, binding.as_ref()) {
                    let (key, proof) = match client_nonce {
                        Some(nc) => {
                            let key = session_key(&password, &nonce, &nc, binding.as_ref());
                            (Some(key), Some(server_proof(&key, &nonce, &nc, binding.as_ref())))
                        }
                        None => (None, None),
                    };
                    session.session_key = key;
                    let key_line = match key {
                        Some(_) => "session key established",
                        None => "legacy client, no session key",
                    };
                    if self.totp_secret.is_some() {
                        session.auth_state = AuthState::PendingTotp;
                        info!("Client {} password OK, awaiting TOTP - {}", addr, key_line);
                        return AuthVerdict { code: AUTH_TOTP_REQUIRED, proof };
                    }
                    session.auth_state = AuthState::Authenticated;
                    // Admission is the last we heard from this client: the
                    // listener's deadline runs from here, not from the challenge.
                    session.last_seen = Instant::now();
                    self.mark_admitted(addr);
                    self.auth_failures.clear(&throttle);
                    info!("Client {} authenticated - {}", addr, key_line);
                    return AuthVerdict { code: AUTH_ACCEPTED, proof };
                }
            }
        }
        // Only a wrong answer to a pending challenge is a failure. A response
        // with nothing pending - the entry reaped after the session time-out, a
        // client already past this step, or a packet that answers nothing -
        // cannot be a password guess, so it is not counted: counting it let five
        // forged responses with a victim's source address block that victim for
        // a minute, and made every slow answer a failure (review 66). The next
        // packet from the address gets a fresh challenge the usual way.
        if !matches!(self.get_auth_state(addr), Some(AuthState::PendingChallenge { .. })) {
            log::debug!("Auth response from {} answers no pending challenge - ignored", addr);
            return not_answered;
        }
        self.auth_failures.record_failure(throttle);
        warn!("Authentication failed from {}", addr);
        // A client of v2.13 or later (it sent a nonce) falls silent on a
        // refusal and comes back only with a Connect. Its entry would hold the
        // address at PendingChallenge for the listener's fifteen seconds, and
        // the corrected password then got no challenge and "server
        // unreachable" (review 76, measured). Gone, the next packet gets a
        // fresh challenge. An older client keeps heartbeating through a
        // refusal and answers every challenge with the same wrong password:
        // for it the entry stays, or five of those in three seconds would
        // block its address for a minute.
        if client_nonce.is_some() {
            self.forget_client(addr);
        }
        refused
    }

    /// Verify a TOTP code: `Some(true)` admits, `Some(false)` is a wrong code,
    /// `None` is a code with nothing pending - not answered, like
    /// `AUTH_NOT_ANSWERED` at the password step.
    pub fn verify_totp(&mut self, addr: SocketAddr, code: &str, throttle: SocketAddr) -> Option<bool> {
        let secret = match &self.totp_secret {
            Some(s) => s.clone(),
            // No 2FA here, so nothing is ever pending: nothing to say - a
            // REJECTED would be an answer to nothing, and a free fingerprint
            // ("this server has no 2FA") for anyone who can send a packet.
            None => return None,
        };
        if let Some(session) = self.clients.get_mut(&addr) {
            if matches!(session.auth_state, AuthState::PendingTotp) {
                if sdr_remote_core::auth::verify_totp(&secret, code) {
                    session.auth_state = AuthState::Authenticated;
                    // `last_seen` was the age of the handshake until here (nothing
                    // before admission refreshes it), and the listener's fifteen
                    // seconds are counted from it: a client that took twenty
                    // seconds to type its code was admitted and reaped within the
                    // next tick, and sent back to the code field (review 74,
                    // measured). Admission is the last we heard from it.
                    session.last_seen = Instant::now();
                    self.mark_admitted(addr);
                    self.auth_failures.clear(&throttle);
                    info!("Client {} TOTP verified, fully authenticated", addr);
                    return Some(true);
                }
            }
        }
        // Same rule as the password step: a code with nothing pending is not a
        // wrong code. (A client reaped at the code step - two minutes since build
        // 71 - gets no answer here; its engine lets go after five seconds and the
        // next knock gets a fresh challenge.)
        if !matches!(self.get_auth_state(addr), Some(AuthState::PendingTotp)) {
            log::debug!("TOTP response from {} answers no pending challenge - ignored", addr);
            return None;
        }
        self.auth_failures.record_failure(throttle);
        warn!("TOTP verification failed from {}", addr);
        Some(false)
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
            self.mark_admitted(addr);
            self.clients.insert(addr, ClientSession {
                addr,
                last_seen: now,
                connected_since: now,
                auth_state,
                session_key: None,
                controls: sdr_remote_core::control_ack::ControlReceiver::new(),
                last_heartbeat_seq: 0,
                rtt_ms: 0,
                loss_percent: 0,
                jitter_ms: 0,
                client_build: sdr_remote_core::protocol::PeerBuild::Unknown,
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
            self.mark_admitted(addr);
        }
        if let Some(session) = self.clients.get_mut(&addr) {
            if restarted {
                session.connected_since = Instant::now();
                // Its control numbers start over too.
                session.controls.reset();
            }
            session.last_heartbeat_seq = seq;
            session.rtt_ms = rtt;
            session.loss_percent = loss;
            session.jitter_ms = jitter;
        }
    }

    /// Record which build a client says it is.
    ///
    /// Returns what it was and what it is now, for a caller that decides with
    /// `PeerBuild::change_line` whether that is worth a line. Deciding here as
    /// well would be the same rule in two places, and the client end has it
    /// too.
    ///
    /// `None` only when the address has no session.
    pub fn note_client_build(
        &mut self,
        addr: SocketAddr,
        version: Option<sdr_remote_core::protocol::VersionTag>,
    ) -> Option<(
        sdr_remote_core::protocol::PeerBuild,
        sdr_remote_core::protocol::PeerBuild,
    )> {
        let now = sdr_remote_core::protocol::PeerBuild::of(version);
        let session = self.clients.get_mut(&addr)?;
        let was = std::mem::replace(&mut session.client_build, now.clone());
        Some((was, now))
    }


    /// Remove a client session (explicit disconnect)
    /// A client is now in and needs what a fresh subscriber gets.
    ///
    /// Exactly four things count as being admitted: a first contact with no password,
    /// an accepted password, an accepted TOTP code, and a silent restart behind an
    /// address that was already known. Issuing a challenge is not one of them - that
    /// is only someone knocking. The watch hears all four; the fourth is an address
    /// that was admitted already, so telling it again changes nothing.
    fn mark_admitted(&mut self, addr: SocketAddr) {
        self.connect_generation = self.connect_generation.wrapping_add(1);
        if let Some(w) = &self.admission {
            w.admitted(addr);
        }
    }

    /// Judge a numbered control from `addr` (phase 2b): apply, a repeat, or older
    /// than what was applied for the same control. `None` for an address with no
    /// session - nothing to judge against.
    pub fn judge_control(
        &mut self,
        addr: SocketAddr,
        key: sdr_remote_core::control_ack::ControlKey,
        seq: u32,
    ) -> Option<sdr_remote_core::control_ack::Verdict> {
        self.clients.get_mut(&addr).map(|c| c.controls.judge(key, seq))
    }

    /// See `connect_generation`. Changes whenever anyone joins.
    pub fn connect_generation(&self) -> u64 {
        self.connect_generation
    }

    /// Returns the transmitters this client was holding, so the caller can
    /// take them out of TX now rather than leave that to the watchdogs.
    pub fn remove(&mut self, addr: SocketAddr) -> Vec<TxTarget> {
        self.forget_client(addr);
        // Everything it held, not just Thetis. A client that walks off with a
        // Yaesu would otherwise keep that radio busy for everyone.
        let mut released = Vec::new();
        for target in TxTarget::ALL {
            if self.tx_holders.get(&target).copied() == Some(addr) {
                info!("TX holder {} disconnected, releasing {:?}", addr, target);
                self.tx_holders.remove(&target);
                released.push(target);
            }
        }
        released
    }

    /// Check if a session is active and authenticated
    fn is_active_authed(s: &ClientSession) -> bool {
        // `false` for the code step on purpose: the match below excludes it, so
        // the longer deadline never applies here. Admit `PendingTotp` below and
        // this has to say so too, or the two places that ask one rule drift.
        !silent_too_long(s.last_seen.elapsed(), false, false)
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
    /// Every active, authenticated client: who receives Thetis RX audio, and so
    /// who counts as listening to it.
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
        // Per client, because the one holding a transmitter is missed sooner.
        let holders: std::collections::HashSet<SocketAddr> =
            self.tx_holders.values().copied().collect();
        let timed_out: Vec<SocketAddr> = self.clients.values()
            .filter(|s| silent_too_long(
                s.last_seen.elapsed(),
                holders.contains(&s.addr),
                matches!(s.auth_state, AuthState::PendingTotp),
            ))
            .map(|s| s.addr)
            .collect();

        for &addr in &timed_out {
            // How long it was silent, so a field log shows how close to its
            // deadline (3 s holding a transmitter, 120 s at the code step, else
            // the long one) it went.
            let silent_ms = self.clients.get(&addr).map(|s| s.last_seen.elapsed().as_millis()).unwrap_or(0);
            if self.clients.get(&addr).map_or(false, |s| matches!(s.auth_state, AuthState::PendingTotp)) {
                warn!("Client {} timed out at the code step ({} ms silent) - the code never came", addr, silent_ms);
            } else {
                warn!("Client {} timed out ({} ms silent)", addr, silent_ms);
            }
            self.forget_client(addr);
            // Every transmitter it held. A client whose link died while it
            // had a Yaesu would otherwise keep that radio busy for everyone
            // until the server restarts.
            for target in TxTarget::ALL {
                if self.tx_holders.get(&target).copied() == Some(addr) {
                    info!("TX holder {} timed out, releasing {:?}", addr, target);
                    self.tx_holders.remove(&target);
                }
            }
        }

        // The byte counters of clients that are gone go with them, here and not
        // only when the status panel draws: a server without a screen would
        // keep one counter per address for ever - and over QUIC every new
        // connection is a new address (review 109).
        if !timed_out.is_empty() {
            let left: Vec<SocketAddr> = self.clients.keys().copied().collect();
            crate::tracked_socket::bw_retain(&left);
        }

        timed_out
    }

    /// Try to acquire one transmitter. Returns true if granted.
    /// First-come-first-served: if no one holds it, grant it; otherwise deny.
    pub fn try_acquire_tx(&mut self, target: TxTarget, addr: SocketAddr) -> bool {
        // The one place a transmitter is granted (`acquire_for_attempt` comes
        // through here too), so closing it here closes it everywhere.
        if self.tx_closed {
            return false;
        }
        match self.tx_holders.get(&target).copied() {
            None => {
                info!("TX lock on {:?} acquired by {}", target, addr);
                self.tx_holders.insert(target, addr);
                self.radio_heard.insert(target, Instant::now());
                true
            }
            Some(holder) if holder == addr => true,
            Some(_) => false,
        }
    }

    /// Transmitting is over for this server run: the safety task that releases
    /// a transmitter nobody lets go of has stopped (`network.rs`,
    /// `safety_task_lost`). Every holder is let go and every later request is
    /// refused - there is no way back short of a restart, on purpose: what was
    /// supposed to end a transmission is gone. Returns who held what, and who
    /// has to be told (`TxClosed`).
    pub fn close_tx(&mut self) -> TxClosed {
        self.tx_closed = true;
        TxClosed {
            held: self.tx_holders.drain().collect(),
            tell: self.active_addrs(),
        }
    }

    /// Why a request for a transmitter was not granted, for every place that
    /// refuses one: the log's words, and what the refusal tells the client
    /// (`Denial`). After `close_tx` the server is closed - the client lets go of
    /// a held key and says a restart is needed; otherwise another client holds
    /// it, and asking again is fine.
    pub fn acquire_refusal(&self) -> AcquireRefusal {
        if self.tx_closed {
            AcquireRefusal {
                why: "transmitting is closed until the server is restarted (the safety task stopped)",
                denial: sdr_remote_core::protocol::Denial::Closed,
            }
        } else {
            AcquireRefusal {
                why: "another client holds it",
                denial: sdr_remote_core::protocol::Denial::Refused,
            }
        }
    }

    /// Release one transmitter - only the client holding it may do so.
    /// Whether it was this client's.
    pub fn release_tx(&mut self, target: TxTarget, addr: SocketAddr) -> bool {
        if self.tx_holders.get(&target).copied() == Some(addr) {
            info!("TX lock on {:?} released by {}", target, addr);
            self.tx_holders.remove(&target);
            return true;
        }
        false
    }

    /// The safety released the transmitter (`check_safety`): whoever held
    /// Thetis lets go of it in the same tick, and is named. Until build 251 the
    /// ownership stayed until a release frame played (build 250) or the time-out
    /// of a silent holder - and when the release packets were lost while the
    /// heartbeat came back, until the client's next press; a second client was
    /// refused all that while (rounds 249 and 250). Thetis only: the safety is
    /// Thetis's, a Yaesu has its own.
    pub fn release_after_safety(&mut self) -> Option<SocketAddr> {
        let holder = self.tx_holder(TxTarget::Thetis)?;
        self.release_tx(TxTarget::Thetis, holder);
        Some(holder)
    }

    /// The 2 s clock of a radio (build 265; owner's decision 2026-10-20): its
    /// holder has not been heard on that radio (`heard_on`; since build 282,
    /// until then any packet of the client) for longer than the packet window
    /// of the Thetis safety (`ptt::silence_ended_the_over`, the same `>`). The
    /// holder lets go; the caller unkeys the radio. Its next packet with the
    /// flag takes the radio again, as Thetis's does (build 282; until then a
    /// way back of its own). Thetis has its own (`check_safety`,
    /// `release_after_safety`).
    pub fn release_silent_radio(
        &mut self,
        target: TxTarget,
        now: Instant,
    ) -> Option<(SocketAddr, std::time::Duration)> {
        if target == TxTarget::Thetis {
            return None;
        }
        let holder = self.tx_holder(target)?;
        let heard = match self.radio_heard.get(&target) {
            Some(&t) => t,
            None => self.clients.get(&holder)?.last_seen,
        };
        let silence = now.saturating_duration_since(heard);
        if !crate::ptt::silence_ended_the_over(Some(silence)) {
            return None;
        }
        self.tx_holders.remove(&target);
        Some((holder, silence))
    }

    /// A packet of `addr` for this radio, at `now`: the 2 s clock starts again
    /// when it is the holder's.
    pub fn heard_on(&mut self, target: TxTarget, addr: SocketAddr, now: Instant) {
        if self.tx_holder(target) == Some(addr) {
            self.radio_heard.insert(target, now);
        }
    }

    /// The radio let go of its holder by itself (build 282): the holder goes,
    /// and its packets with the flag do not take the radio again until one
    /// without says it let go too (`let_go_seen`). Whether it held it.
    pub fn let_go_by_the_radio(&mut self, target: TxTarget, addr: SocketAddr) -> bool {
        let held = self.release_tx(target, addr);
        if held {
            self.let_go_by_radio.insert(target, addr);
        }
        held
    }

    /// Is `addr` the holder its radio let go of, still pressing?
    pub fn let_go_by_the_radio_and_still_pressing(&self, target: TxTarget, addr: SocketAddr) -> bool {
        self.let_go_by_radio.get(&target) == Some(&addr)
    }

    /// A packet without the flag from `addr`: it let go, so a radio that let
    /// go of it first may be taken by its next press again.
    pub fn let_go_seen(&mut self, target: TxTarget, addr: SocketAddr) {
        if self.let_go_by_radio.get(&target) == Some(&addr) {
            self.let_go_by_radio.remove(&target);
        }
    }

    /// Take a transmitter for one attempt - a CW packet on its way through the
    /// gates. `None` when another client holds it; otherwise whether this
    /// client held it already (its microphone on that radio, or its own run),
    /// which is what `give_back_attempt` needs to know.
    ///
    /// One pair for both kinds of transmitter. Review 316 found the rule in two
    /// spellings: the Yaesu gate asked "did I hold it before", the Thetis gate
    /// "does a run of mine play" - and the second let a failed CW attempt take
    /// away a lock this client held for its microphone.
    pub fn acquire_for_attempt(&mut self, target: TxTarget, addr: SocketAddr) -> Option<bool> {
        let held_before = self.tx_holder(target) == Some(addr);
        if self.try_acquire_tx(target, addr) {
            Some(held_before)
        } else {
            None
        }
    }

    /// The attempt came to nothing: give back what `acquire_for_attempt` took
    /// for it - and only that. A lock this client held before stays its own.
    pub fn give_back_attempt(&mut self, target: TxTarget, addr: SocketAddr, held_before: bool) {
        if !held_before {
            self.release_tx(target, addr);
        }
    }

    /// Who holds this transmitter, if anyone.
    pub fn tx_holder(&self, target: TxTarget) -> Option<SocketAddr> {
        self.tx_holders.get(&target).copied()
    }

    /// May this client's audio go to this transmitter?
    ///
    /// The named decision. It is small on purpose: what matters is that every
    /// audio path asks *this*, so the refusal is covered by the same test as
    /// the table. It had no name at all on the Yaesu side, which is why nothing
    /// could reach it (review finding, part A).
    pub fn accepts(&self, target: TxTarget, addr: SocketAddr) -> bool {
        self.tx_holders.get(&target).copied() == Some(addr)
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

/// May a frame coming out of the playout buffer key Thetis?
///
/// Only when the frame carries PTT, somebody owns the transmitter, and it is
/// the same client the buffered audio came from. Today the two cannot differ:
/// Thetis is only ever acquired on an audio packet, and that same packet
/// empties the buffer when the owner changes, in the same task as playout. So
/// a client that left with its tail in the buffer cannot key the next owner's
/// transmission - as long as that coupling holds. Comparing identity here
/// keeps the guarantee even when it stops holding, for one comparison (review
/// finding on build 81). `accepts` is the same question asked of an incoming
/// packet.
pub fn playout_keys(frame_ptt: bool, owner: Option<SocketAddr>, audio_from: Option<SocketAddr>) -> bool {
    frame_ptt && owner.is_some() && owner == audio_from
}

/// Does a frame played without PTT hand Thetis back? Asked when nothing is
/// transmitted (build 250): the safety let the transmitter go in a silence
/// (`force_release` sets the PTT to `Rx`, nothing more), the operator let go in
/// it, and the release frame found no transmission to end - the branch that
/// hands the ownership back runs only while transmitting, so the client kept
/// Thetis and a second client was refused (round 249). Only the client the
/// session still holds, whose audio it is: the other side of `playout_keys`.
pub fn playout_hands_back(frame_ptt: bool, owner: Option<SocketAddr>, audio_from: Option<SocketAddr>) -> bool {
    !frame_ptt && owner.is_some() && owner == audio_from
}

/// Has the receive loop's memory of the Thetis holder (`remembered`) outlived
/// the holding? The session lets go in places the loop does not see - a holder
/// that times out (`check_timeout`, in the safety task) - and the loop's
/// buffer still holds that client's last audio, with PTT. Back on the same
/// address, the client was handed the transmitter as new while the loop saw
/// the same holder, kept the buffer, and the old audio keyed Thetis (field
/// 2026-10-16 23:48:42, 181 ms of speech 3 s old; build 247). Asked before the
/// new acquisition: after it the session says "held" again.
pub fn holder_outlived(remembered: Option<SocketAddr>, holder_now: Option<SocketAddr>) -> bool {
    remembered.is_some() && holder_now != remembered
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Step 0 of part A: recording what the TX slot does today.
    //
    // try_acquire_tx and release_tx had no tests at all, while the plan claimed with
    // a tick that it works for Thetis. Both reviewers asked for this before anything
    // changes: starting a refactor on untested behaviour is how a repair quietly
    // breaks something else.
    //
    // These are characterisation tests. They pass straight away, and that is the
    // point.

    fn mgr() -> SessionManager {
        SessionManager::new(None, None)
    }

    fn a(port: u16) -> SocketAddr {
        format!("127.0.0.1:{port}").parse().unwrap()
    }

    /// First come, first served.
    #[test]
    fn the_first_to_ask_gets_tx_and_the_second_does_not() {
        let mut m = mgr();
        assert!(m.try_acquire_tx(TxTarget::Thetis, a(1)), "nobody held it");
        assert!(!m.try_acquire_tx(TxTarget::Thetis, a(2)), "somebody else holds it");
        assert_eq!(m.tx_holder(TxTarget::Thetis), Some(a(1)));
    }

    /// A client that leaves takes its transmitters with it, and says which: the
    /// Disconnect handler unkeys exactly those (build 80). A client that held
    /// nothing returns nothing, and a stranger cannot take another's.
    #[test]
    fn a_leaving_client_hands_back_what_it_held_and_names_it() {
        let mut m = mgr();
        assert!(m.try_acquire_tx(TxTarget::Thetis, a(1)));
        assert!(m.try_acquire_tx(TxTarget::Yaesu2, a(1)));
        assert!(m.try_acquire_tx(TxTarget::Yaesu1, a(2)));
        assert_eq!(m.remove(a(3)), vec![], "held nothing");
        // The client is in the table too, and remove() takes it out: the half
        // a surviving mutant showed the first version of this test could not
        // see, because the table was empty (review finding on build 82).
        m.clients.insert(a(1), mk_session("127.0.0.1:1", true, 8.0, 4.0, true, true));
        assert!(m.clients.contains_key(&a(1)));
        let released = m.remove(a(1));
        assert!(!m.clients.contains_key(&a(1)), "gone from the table");
        assert_eq!(released, vec![TxTarget::Thetis, TxTarget::Yaesu2], "both it held, in slot order");
        assert_eq!(m.tx_holder(TxTarget::Thetis), None);
        assert_eq!(m.tx_holder(TxTarget::Yaesu2), None);
        assert_eq!(m.tx_holder(TxTarget::Yaesu1), Some(a(2)), "the other client keeps its radio");
        assert_eq!(m.remove(a(1)), vec![], "leaving twice hands back nothing more");
    }

    /// Playout keys on identity: the owner, and the same client the audio came
    /// from. Every other combination keys nothing.
    #[test]
    fn playout_keys_only_for_the_owner_whose_audio_it_is() {
        assert!(playout_keys(true, Some(a(1)), Some(a(1))), "owner and audio agree");
        assert!(!playout_keys(false, Some(a(1)), Some(a(1))), "no PTT on the frame");
        assert!(!playout_keys(true, None, Some(a(1))), "nobody owns it: a tail after a goodbye");
        assert!(!playout_keys(true, Some(a(2)), Some(a(1))), "another client owns it: the first one's tail");
        assert!(!playout_keys(true, Some(a(2)), None), "owner, but no audio of it yet");
        assert!(!playout_keys(true, None, None));
    }

    /// A release frame played while nothing is transmitted hands Thetis back -
    /// the safety let the transmitter go in a silence, and the operator let go
    /// in it (build 250; round 249). Only the client the session still holds,
    /// whose audio it is, and only a frame without PTT.
    #[test]
    fn a_release_played_while_nothing_transmits_hands_thetis_back() {
        assert!(playout_hands_back(false, Some(a(1)), Some(a(1))), "the holder let go");
        assert!(!playout_hands_back(true, Some(a(1)), Some(a(1))), "a frame with PTT keeps it");
        assert!(!playout_hands_back(false, None, Some(a(1))), "the session let go already: nothing to hand back");
        assert!(!playout_hands_back(false, Some(a(2)), Some(a(1))), "another client holds it");
        assert!(!playout_hands_back(false, Some(a(1)), None), "no audio of it in the loop");
        // Never both: a frame that keys is no release, and the reverse.
        for ptt in [true, false] {
            for owner in [None, Some(a(1)), Some(a(2))] {
                for from in [None, Some(a(1))] {
                    assert!(!(playout_keys(ptt, owner, from) && playout_hands_back(ptt, owner, from)));
                }
            }
        }
    }

    /// The receive loop's memory of the Thetis holder outlives the holding when
    /// the session let go elsewhere - a time-out of the holder - and then its
    /// buffered audio, with PTT, must go as at a goodbye (build 247; field
    /// 2026-10-16 23:48:42). The time-out itself, then the same client again on
    /// the same address: the session hands it the transmitter as new.
    #[test]
    fn the_loops_holder_outlives_a_time_out_of_the_session() {
        assert!(!holder_outlived(None, None), "nobody remembered");
        assert!(!holder_outlived(None, Some(a(1))), "nobody remembered, someone holds");
        assert!(!holder_outlived(Some(a(1)), Some(a(1))), "still holding: every packet of an over");
        assert!(holder_outlived(Some(a(1)), None), "let go elsewhere: a time-out");
        assert!(holder_outlived(Some(a(1)), Some(a(2))), "and someone else holds it now");
        let mut m = mgr();
        m.touch(a(1));
        assert!(m.try_acquire_tx(TxTarget::Thetis, a(1)));
        let remembered = Some(a(1));
        assert!(!holder_outlived(remembered, m.tx_holder(TxTarget::Thetis)));
        m.clients.get_mut(&a(1)).unwrap().last_seen = std::time::Instant::now() - std::time::Duration::from_secs(4);
        assert_eq!(m.check_timeout(), vec![a(1)], "the holder timed out");
        m.touch(a(1));
        assert!(holder_outlived(remembered, m.tx_holder(TxTarget::Thetis)), "asked before the new acquisition");
        assert!(m.try_acquire_tx(TxTarget::Thetis, a(1)));
        assert!(!holder_outlived(remembered, m.tx_holder(TxTarget::Thetis)), "asked after it, it can no longer tell");
    }

    /// The holder may ask again without losing it - that happens on every audio
    /// packet.
    #[test]
    fn the_holder_may_ask_again() {
        let mut m = mgr();
        assert!(m.try_acquire_tx(TxTarget::Thetis, a(1)));
        assert!(m.try_acquire_tx(TxTarget::Thetis, a(1)));
        assert_eq!(m.tx_holder(TxTarget::Thetis), Some(a(1)));
    }

    /// Only the holder can release it. Otherwise a second client could end the first
    /// one's transmission.
    #[test]
    fn only_the_holder_can_release_it() {
        let mut m = mgr();
        m.try_acquire_tx(TxTarget::Thetis, a(1));

        m.release_tx(TxTarget::Thetis, a(2));
        assert_eq!(m.tx_holder(TxTarget::Thetis), Some(a(1)), "somebody else must not release it");

        m.release_tx(TxTarget::Thetis, a(1));
        assert_eq!(m.tx_holder(TxTarget::Thetis), None);
    }

    /// And after a release the next one may have it.
    #[test]
    fn after_a_release_the_next_one_gets_it() {
        let mut m = mgr();
        m.try_acquire_tx(TxTarget::Thetis, a(1));
        m.release_tx(TxTarget::Thetis, a(1));
        assert!(m.try_acquire_tx(TxTarget::Thetis, a(2)));
    }

    /// A client that disappears releases, or a dropped connection keeps the
    /// transmitter busy for everyone.
    #[test]
    fn a_client_that_disappears_releases_tx() {
        let mut m = mgr();
        m.try_acquire_tx(TxTarget::Thetis, a(1));
        m.remove(a(1));
        assert_eq!(m.tx_holder(TxTarget::Thetis), None);
        assert!(m.try_acquire_tx(TxTarget::Thetis, a(2)));
    }

    // ---- Part A: a table per transmitter, and a decision with a name.

    /// The heart of part A: two transmitters are independent. Client B may hold the
    /// 991A while A holds Thetis - that is what the owner described and what can
    /// already happen today.
    #[test]
    fn two_transmitters_are_held_independently() {
        let mut m = mgr();
        assert!(m.try_acquire_tx(TxTarget::Thetis, a(1)));
        assert!(m.try_acquire_tx(TxTarget::Yaesu1, a(2)));
        assert!(m.try_acquire_tx(TxTarget::Yaesu2, a(3)));

        assert_eq!(m.tx_holder(TxTarget::Thetis), Some(a(1)));
        assert_eq!(m.tx_holder(TxTarget::Yaesu1), Some(a(2)));
        assert_eq!(m.tx_holder(TxTarget::Yaesu2), Some(a(3)));
    }

    /// The two rules the whole repair rests on.
    #[test]
    fn only_the_holder_of_a_transmitter_is_accepted() {
        let mut m = mgr();
        m.try_acquire_tx(TxTarget::Yaesu1, a(1));

        assert!(m.accepts(TxTarget::Yaesu1, a(1)), "the holder may talk");
        assert!(
            !m.accepts(TxTarget::Yaesu1, a(2)),
            "somebody else must not talk into the same radio"
        );
    }

    /// And a claim on one transmitter gives no right to the other.
    #[test]
    fn holding_one_transmitter_grants_nothing_on_another() {
        let mut m = mgr();
        m.try_acquire_tx(TxTarget::Thetis, a(1));
        assert!(!m.accepts(TxTarget::Yaesu1, a(1)));
        assert!(!m.accepts(TxTarget::Yaesu2, a(1)));
    }

    /// With no owner nobody is accepted - not even "for want of anything better".
    /// That is what the Yaesu path does do today.
    #[test]
    fn an_unclaimed_transmitter_accepts_nobody() {
        let m = mgr();
        assert!(!m.accepts(TxTarget::Yaesu1, a(1)));
    }

    /// Releasing touches only your own transmitter.
    #[test]
    fn releasing_one_transmitter_leaves_the_others_alone() {
        let mut m = mgr();
        m.try_acquire_tx(TxTarget::Thetis, a(1));
        m.try_acquire_tx(TxTarget::Yaesu1, a(1));

        m.release_tx(TxTarget::Yaesu1, a(1));
        assert_eq!(m.tx_holder(TxTarget::Yaesu1), None);
        assert_eq!(m.tx_holder(TxTarget::Thetis), Some(a(1)), "Thetis stays with him");
    }

    /// A departing client releases everything it held, not only Thetis.
    #[test]
    fn a_departing_client_releases_every_transmitter_it_held() {
        let mut m = mgr();
        m.try_acquire_tx(TxTarget::Thetis, a(1));
        m.try_acquire_tx(TxTarget::Yaesu1, a(1));
        m.try_acquire_tx(TxTarget::Yaesu2, a(2));

        m.remove(a(1));
        assert_eq!(m.tx_holder(TxTarget::Thetis), None);
        assert_eq!(m.tx_holder(TxTarget::Yaesu1), None);
        assert_eq!(m.tx_holder(TxTarget::Yaesu2), Some(a(2)), "somebody else's is untouched");
    }

    /// But somebody else disappearing does not affect the holder.
    #[test]
    fn another_client_disappearing_leaves_the_holder_alone() {
        let mut m = mgr();
        m.try_acquire_tx(TxTarget::Thetis, a(1));
        m.remove(a(2));
        assert_eq!(m.tx_holder(TxTarget::Thetis), Some(a(1)));
    }

    fn mk_session(addr_str: &str, allow: bool, rx1_zoom: f32, rx2_zoom: f32, rx1_en: bool, rx2_en: bool) -> ClientSession {
        let now = Instant::now();
        ClientSession {
            addr: addr_str.parse().unwrap(),
            last_seen: now,
            connected_since: now,
            auth_state: AuthState::NoAuth,
            session_key: None,
            controls: sdr_remote_core::control_ack::ControlReceiver::new(),
            last_heartbeat_seq: 0, rtt_ms: 0, loss_percent: 0, jitter_ms: 0,
            client_build: sdr_remote_core::protocol::PeerBuild::Unknown,
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
    fn checkbox_strictest_wins() {
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
    fn checkbox_strictest_no_clients_returns_safe_default() {
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

/// One loop's view of `SessionManager::connect_generation`: did anyone join
/// since this loop last looked? Every loop that keeps a "who already has it"
/// list or a previous value per push owns one, and forgets that list when this
/// says yes - a rejoin inside the session timeout keeps the address and the
/// client count, so nothing else can see it.
///
/// One type for one rule (working rule 15): it was written out by hand four
/// times, and the fifth loop that needed it - the equipment status, whose
/// panels a client clears with its session since build 83 - had none, so the
/// panels stood empty for up to ten seconds after a rejoin (review 84). Each
/// loop owns its own: the edge is consumed, and two loops on one value would
/// let the first one swallow it.
pub struct GenerationWatch {
    last: u64,
}

impl GenerationWatch {
    pub fn new() -> Self {
        Self { last: 0 }
    }

    /// True when `generation` differs from what this watch saw last time, and
    /// remember it. Read the generation under the same lock as the address
    /// list the loop is about to serve, or a client admitted in between finds
    /// the edge already used and waits for the safety net.
    pub fn changed(&mut self, generation: u64) -> bool {
        let changed = generation != self.last;
        self.last = generation;
        changed
    }
}

#[cfg(test)]
mod connect_generation_tests {
    use super::*;

    /// The rule the five loops share, in one place where a test can reach it.
    #[test]
    fn a_watch_fires_once_per_admission_and_never_twice() {
        let a: SocketAddr = "10.0.0.7:5000".parse().unwrap();
        let mut m = SessionManager::new(None, None);
        let mut watch = GenerationWatch::new();
        assert!(!watch.changed(m.connect_generation()), "nobody has joined yet");
        m.touch(a);
        assert!(watch.changed(m.connect_generation()), "an admission is an edge");
        assert!(!watch.changed(m.connect_generation()), "and the edge is used up");
        // A rejoin on the same address: the count and the address stay the same,
        // and this is the only thing that sees it (review 83).
        m.remove(a);
        m.touch(a);
        assert!(watch.changed(m.connect_generation()), "a rejoin is an edge too");
        // Two watches on one manager each see the edge once.
        let mut other = GenerationWatch::new();
        assert!(other.changed(m.connect_generation()));
        assert!(!watch.changed(m.connect_generation()));
        // The counter wraps; a watch compares, it does not order.
        let mut w = GenerationWatch { last: u64::MAX };
        assert!(w.changed(0));
    }

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

    /// A restarted client numbers its controls from 1 again. Without the reset
    /// its first controls would read as older than what the previous process
    /// had applied, and none of them would be applied (phase 2b).
    #[test]
    fn a_silent_restart_starts_the_control_numbers_over() {
        use sdr_remote_core::control_ack::{ControlKey, Verdict};
        let mut m = mgr();
        let a = A.parse().unwrap();
        m.touch(a);
        m.update_heartbeat(a, 400, 0, 0, 0);
        let k = ControlKey::Control(7);
        assert_eq!(m.judge_control(a, k, 50), Some(Verdict::Apply));
        assert_eq!(m.judge_control(a, k, 1), Some(Verdict::Stale), "same process: older");
        m.update_heartbeat(a, 0, 0, 0, 0); // a new process behind the same address
        assert_eq!(m.judge_control(a, k, 1), Some(Verdict::Apply));
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
        let resp = sdr_remote_core::auth::compute_hmac("secret", &nonce, Some(&TEST_BINDING));
        assert_eq!(
            m.verify_auth(a, &resp, None, bound()).code,
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
        let resp = sdr_remote_core::auth::compute_hmac("secret", &nonce, Some(&TEST_BINDING));
        assert_eq!(
            m.verify_auth(a, &resp, None, bound()).code,
            sdr_remote_core::protocol::AUTH_TOTP_REQUIRED,
            "the password step must not admit a 2FA client"
        );
        let before = m.connect_generation();
        let code = sdr_remote_core::auth::generate_totp(&secret);
        assert_eq!(m.verify_totp(a, &code, a), Some(true));
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

#[cfg(test)]
mod holder_timeout_tests {
    use super::*;
    use std::net::SocketAddr;

    // ---- and now check_timeout() itself ----
    //
    // The three tests below exercise the function that USES the deadline, not only
    // the one that computes it. That distinction is the finding: a test that fills in
    // its own booleans can never see that the caller passes the wrong ones (review
    // finding, round 2).

    fn a(port: u16) -> SocketAddr {
        format!("127.0.0.1:{port}").parse().unwrap()
    }

    /// Move a client's last sign of life back by this many seconds.
    fn silent_for_ms(m: &mut SessionManager, addr: SocketAddr, ms: u64) {
        let c = m.clients.get_mut(&addr).expect("client exists");
        c.last_seen = std::time::Instant::now() - std::time::Duration::from_millis(ms);
    }

    fn silent_for(m: &mut SessionManager, addr: SocketAddr, secs: u64) {
        let c = m.clients.get_mut(&addr).expect("client exists");
        c.last_seen = std::time::Instant::now() - std::time::Duration::from_secs(secs);
    }

    /// A listener may be quiet for a long time. Four seconds is nothing.
    #[test]
    fn check_timeout_leaves_a_quiet_listener_alone() {
        let mut m = SessionManager::new(None, None);
        m.touch(a(1));
        silent_for(&mut m, a(1), 4);
        assert!(m.check_timeout().is_empty(), "a listener may be quiet for a while");
    }

    /// Whoever holds a transmitter may not. This is the case that on 2026-09-03 left a
    /// 991A on the air for twenty seconds.
    #[test]
    fn check_timeout_drops_a_silent_transmitter_holder_and_frees_the_radio() {
        let mut m = SessionManager::new(None, None);
        m.touch(a(1));
        assert!(m.try_acquire_tx(TxTarget::Yaesu1, a(1)));
        silent_for(&mut m, a(1), 4);

        let gone = m.check_timeout();
        assert_eq!(gone, vec![a(1)], "the holder is gone after four seconds");
        assert_eq!(m.tx_holder(TxTarget::Yaesu1), None, "and the transmitter is free");
    }

    /// Step 4's trial client (build 256): let in as authenticated once, asking
    /// for no receive audio, and a client like any other after that.
    #[test]
    fn the_trial_client_is_let_in_once_and_asks_for_no_audio() {
        let mut m = SessionManager::new(Some("pw".to_string()), None);
        let t = crate::trial::TRIAL_CLIENT;
        assert!(!m.is_authenticated(t));
        assert!(m.admit_trial_client(t), "let in");
        assert!(m.is_authenticated(t));
        assert!(!m.clients[&t].rx1_enabled, "no receive audio asked");
        assert!(!m.admit_trial_client(t), "once: a second call finds it in");
        assert!(m.try_acquire_tx(TxTarget::Thetis, t), "and it can take Thetis like a client");
    }

    /// The safety released the transmitter: whoever held Thetis lets go of it
    /// in the same breath, not at the time-out a second later or at a release
    /// frame that may never come (build 251; rounds 249 and 250). Thetis only -
    /// the safety is Thetis's - and once: a second call finds nobody.
    #[test]
    fn the_safety_takes_thetis_from_its_holder() {
        let mut m = SessionManager::new(None, None);
        m.touch(a(1));
        m.touch(a(2));
        assert!(m.try_acquire_tx(TxTarget::Thetis, a(1)));
        assert!(m.try_acquire_tx(TxTarget::Yaesu1, a(2)));
        assert!(!m.try_acquire_tx(TxTarget::Thetis, a(2)), "held: refused");

        assert_eq!(m.release_after_safety(), Some(a(1)), "the holder is named");
        assert_eq!(m.tx_holder(TxTarget::Thetis), None, "and Thetis is free");
        assert_eq!(m.tx_holder(TxTarget::Yaesu1), Some(a(2)), "a Yaesu is not the safety's");
        assert_eq!(m.release_after_safety(), None, "the second pass finds nobody");
        assert!(m.check_timeout().is_empty(), "the client itself stays connected");

        assert!(m.try_acquire_tx(TxTarget::Thetis, a(2)), "a second client gets Thetis");
        m.release_tx(TxTarget::Thetis, a(2));
        assert!(m.try_acquire_tx(TxTarget::Thetis, a(1)), "and the first one again, on its next press");
    }

    /// The 2 s clock of a radio (build 265): a holder not heard on that radio
    /// past the packet window lets go - heard on the radio, not anywhere: a
    /// heartbeat keeps nothing keyed since the PTT rides on the audio (build
    /// 282). Thetis is not this clock's; a holder that speaks is left alone; and
    /// its next press takes the radio again like anyone's.
    #[test]
    fn a_radio_lets_go_of_a_holder_it_does_not_hear() {
        use std::time::Duration;
        let mut m = SessionManager::new(None, None);
        m.touch(a(1));
        m.touch(a(2));
        assert!(m.try_acquire_tx(TxTarget::Yaesu1, a(1)));
        let t0 = m.radio_heard[&TxTarget::Yaesu1];
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        assert!(m.try_acquire_tx(TxTarget::Thetis, a(1)));
        assert_eq!(m.release_silent_radio(TxTarget::Yaesu1, at(2_000)), None, "inside the window");
        assert_eq!(m.release_silent_radio(TxTarget::Thetis, at(9_000)), None, "Thetis has its own");
        assert_eq!(m.release_silent_radio(TxTarget::Yaesu2, at(9_000)), None, "nobody holds it");
        // Heard on the radio: the clock starts again. Somebody else's packet,
        // or the client's heartbeat, does not.
        m.heard_on(TxTarget::Yaesu1, a(1), at(1_500));
        m.heard_on(TxTarget::Yaesu1, a(2), at(3_000));
        m.touch(a(1));
        assert_eq!(m.release_silent_radio(TxTarget::Yaesu1, at(3_500)), None, "heard at 1.5 s");
        assert_eq!(m.release_silent_radio(TxTarget::Yaesu1, at(3_501)), Some((a(1), Duration::from_millis(2_001))));
        assert_eq!(m.tx_holder(TxTarget::Yaesu1), None, "the holder lets go");
        assert_eq!(m.release_silent_radio(TxTarget::Yaesu1, at(4_000)), None, "once");
        m.heard_on(TxTarget::Yaesu1, a(1), at(4_000));
        assert_eq!(m.tx_holder(TxTarget::Yaesu1), None, "a packet alone takes nothing back");
        // The second pass: its next press takes it, and the clock runs again.
        assert!(m.try_acquire_tx(TxTarget::Yaesu1, a(1)));
        let t1 = m.radio_heard[&TxTarget::Yaesu1];
        assert!(m.release_silent_radio(TxTarget::Yaesu1, t1 + Duration::from_millis(2_000)).is_none());
        assert!(m.release_silent_radio(TxTarget::Yaesu1, t1 + Duration::from_millis(2_001)).is_some());
        assert!(m.try_acquire_tx(TxTarget::Yaesu1, a(2)), "the radio is free for another press");
        // The client itself stays: no sign-in again.
        assert!(m.clients.contains_key(&a(1)));
    }

    /// The radio let go by itself (build 282): its holder's packets still carry
    /// the flag until it hears of it, and are ignored until one without the
    /// flag comes - from that client, on that radio. Twice, and the mark goes
    /// with the client.
    #[test]
    fn a_radio_that_let_go_by_itself_waits_for_its_holder_to_let_go() {
        let mut m = SessionManager::new(None, None);
        m.touch(a(1));
        m.touch(a(2));
        assert!(!m.let_go_by_the_radio(TxTarget::Yaesu1, a(1)), "it held nothing");
        assert!(!m.let_go_by_the_radio_and_still_pressing(TxTarget::Yaesu1, a(1)));
        for pass in 0..2 {
            assert!(m.try_acquire_tx(TxTarget::Yaesu1, a(1)), "pass {pass}");
            assert!(m.let_go_by_the_radio(TxTarget::Yaesu1, a(1)));
            assert_eq!(m.tx_holder(TxTarget::Yaesu1), None);
            assert!(m.let_go_by_the_radio_and_still_pressing(TxTarget::Yaesu1, a(1)));
            assert!(!m.let_go_by_the_radio_and_still_pressing(TxTarget::Yaesu1, a(2)), "only its holder");
            assert!(!m.let_go_by_the_radio_and_still_pressing(TxTarget::Yaesu2, a(1)), "only that radio");
            m.let_go_seen(TxTarget::Yaesu2, a(1));
            m.let_go_seen(TxTarget::Yaesu1, a(2));
            assert!(m.let_go_by_the_radio_and_still_pressing(TxTarget::Yaesu1, a(1)), "still pressing: another client's and another radio's let-go are not its own");
            m.let_go_seen(TxTarget::Yaesu1, a(1));
            assert!(!m.let_go_by_the_radio_and_still_pressing(TxTarget::Yaesu1, a(1)), "pass {pass}: let go");
        }
        // A client that goes takes its mark along.
        assert!(m.try_acquire_tx(TxTarget::Yaesu2, a(2)));
        assert!(m.let_go_by_the_radio(TxTarget::Yaesu2, a(2)));
        m.remove(a(2));
        assert!(!m.let_go_by_the_radio_and_still_pressing(TxTarget::Yaesu2, a(2)));
    }

    /// The two numbers of a press that loses its link (build 265): the server
    /// lets go at 2 s, the client at 3 s - so a link back in between keys the
    /// radio again with the press still held (since build 282 by the flag on
    /// its next packet, until then by a way back of its own).
    #[test]
    fn the_clocks_of_a_lost_press_follow_each_other() {
        let engine = include_str!("../../sdr-remote-logic/src/engine.rs");
        let line = "pub(crate) const TRANSMIT_LINK_TIMEOUT_MS: u64 = ";
        let at = engine.find(line).expect("the client's window") + line.len();
        let client: u64 = engine[at..at + engine[at..].find(';').unwrap()].replace('_', "").parse().unwrap();
        let server = crate::ptt::PTT_PACKET_TIMEOUT_MS;
        assert_eq!((server, client), (2_000, 3_000), "owner's decision 2026-10-20");
        assert!(client > server, "the server lets go first");
    }

    /// The safety task stopped: everyone holding a transmitter lets go, and
    /// nobody gets one again - not the holder, not a newcomer, not through the
    /// CW attempt either (`acquire_for_attempt` comes through the same gate).
    #[test]
    fn closing_tx_lets_every_holder_go_and_refuses_everyone_after() {
        let mut m = SessionManager::new(None, None);
        assert!(m.try_acquire_tx(TxTarget::Thetis, a(1)));
        assert!(m.try_acquire_tx(TxTarget::Yaesu2, a(2)));

        let mut held = m.close_tx().held;
        held.sort_by_key(|(_, addr)| addr.port());
        assert_eq!(held, vec![(TxTarget::Thetis, a(1)), (TxTarget::Yaesu2, a(2))], "who held what");
        for target in TxTarget::ALL {
            assert_eq!(m.tx_holder(target), None, "{:?} is let go", target);
            assert!(!m.accepts(target, a(1)), "no audio goes to {:?}", target);
        }
        assert!(!m.try_acquire_tx(TxTarget::Thetis, a(1)), "not the one who held it");
        assert!(!m.try_acquire_tx(TxTarget::Yaesu1, a(3)), "not a newcomer");
        assert_eq!(m.acquire_for_attempt(TxTarget::Thetis, a(1)), None, "not a CW attempt");
    }

    /// The close says who has to be told: every active client, the listeners
    /// too - not only the holders. Field test of build 60: the phone, listening,
    /// was told nothing and learned of the closed server at its first press.
    #[test]
    fn closing_tx_names_every_active_client_to_tell() {
        let mut m = SessionManager::new(None, None);
        for port in 1..=3 {
            m.touch(a(port));
        }
        assert!(m.try_acquire_tx(TxTarget::Thetis, a(1)));
        let mut tell = m.close_tx().tell;
        tell.sort_by_key(|addr| addr.port());
        assert_eq!(tell, vec![a(1), a(2), a(3)], "the holder and both listeners");
    }

    /// The refusal says why: before the close another client holds it and the
    /// client may ask again; after it the server let go and the client must not.
    /// Field test 2026-10-01 11:00:42: after the close the log still said
    /// "while another client holds it".
    #[test]
    fn a_refusal_after_the_close_says_the_server_let_go() {
        let mut m = SessionManager::new(None, None);
        use sdr_remote_core::protocol::Denial;
        assert_eq!(m.acquire_refusal().denial, Denial::Refused);
        assert_eq!(m.acquire_refusal().why, "another client holds it");
        m.close_tx();
        assert_eq!(m.acquire_refusal().denial, Denial::Closed);
        assert!(m.acquire_refusal().why.contains("closed until the server is restarted"));
    }

    /// The distinction is per client, not per server: in the same round the
    /// holder drops and the listener stays.
    #[test]
    fn check_timeout_judges_each_client_by_its_own_deadline() {
        let mut m = SessionManager::new(None, None);
        m.touch(a(1));
        m.touch(a(2));
        assert!(m.try_acquire_tx(TxTarget::Thetis, a(1)));
        silent_for(&mut m, a(1), 5);
        silent_for(&mut m, a(2), 5);

        let gone = m.check_timeout();
        assert_eq!(gone, vec![a(1)], "only the transmitting one drops");
        assert!(m.clients.contains_key(&a(2)), "the listener stays");
        assert_eq!(m.tx_holder(TxTarget::Thetis), None);
    }

    #[test]
    fn a_failed_attempt_gives_back_only_what_it_took() {
        let mut m = SessionManager::new(None, None);
        m.touch(a(1));
        m.touch(a(2));
        // Nobody held it: taken for the attempt, and given back.
        assert_eq!(m.acquire_for_attempt(TxTarget::Yaesu1, a(1)), Some(false));
        m.give_back_attempt(TxTarget::Yaesu1, a(1), false);
        assert_eq!(m.tx_holder(TxTarget::Yaesu1), None);
        // Held already (the microphone): it stays.
        assert!(m.try_acquire_tx(TxTarget::Thetis, a(1)));
        assert_eq!(m.acquire_for_attempt(TxTarget::Thetis, a(1)), Some(true));
        m.give_back_attempt(TxTarget::Thetis, a(1), true);
        assert_eq!(m.tx_holder(TxTarget::Thetis), Some(a(1)), "its own lock is not taken away");
        // Somebody else holds it: nothing taken, nothing given back.
        assert_eq!(m.acquire_for_attempt(TxTarget::Thetis, a(2)), None);
        assert_eq!(m.tx_holder(TxTarget::Thetis), Some(a(1)));
    }

    #[test]
    fn a_listener_keeps_the_long_deadline() {
        assert_eq!(timeout_secs(false, false), SESSION_TIMEOUT_SECS);
    }

    /// The measurement this exists for: a link that died mid-transmission left
    /// the radio on the air for twenty seconds.
    #[test]
    fn a_client_holding_a_transmitter_is_missed_sooner() {
        assert!(timeout_secs(true, false) < timeout_secs(false, false));
        assert_eq!(timeout_secs(true, false), TX_HOLDER_TIMEOUT_SECS);
    }

    /// The deadline is the number of seconds it names. It used to be one more:
    /// `elapsed().as_secs() > 3` rounds down, so it only held from 4.000 s on,
    /// and the comment on TX_HOLDER_TIMEOUT_SECS promised three (found in the
    /// review of the CW playout, 2026-09-18). The same rounding sat in the
    /// check for an active session, against SESSION_TIMEOUT_SECS.
    #[test]
    fn the_deadline_is_the_number_of_seconds_it_names() {
        let ms = std::time::Duration::from_millis;
        assert!(!silent_too_long(ms(3000), true, false), "exactly three: not yet");
        assert!(silent_too_long(ms(3001), true, false));
        assert!(silent_too_long(ms(3999), true, false), "the second the rounding lost");
        assert!(!silent_too_long(ms(15_000), false, false));
        assert!(silent_too_long(ms(15_001), false, false));
        // And through the session manager, both places.
        let mut m = SessionManager::new(None, None);
        m.touch(a(1));
        m.touch(a(2));
        m.touch(a(3));
        assert!(m.try_acquire_tx(TxTarget::Thetis, a(1)));
        silent_for_ms(&mut m, a(1), 3500);
        silent_for_ms(&mut m, a(2), 15_500);
        silent_for_ms(&mut m, a(3), 3500);
        assert!(!m.active_addrs().contains(&a(2)), "fifteen and a half seconds silent is not active");
        // A listener three and a half seconds quiet is active: its deadline is
        // the listener's, not the holder's (review 314 found nothing pinned it).
        assert!(m.active_addrs().contains(&a(3)), "a quiet listener is still active");
        let mut gone = m.check_timeout();
        gone.sort();
        assert_eq!(gone, vec![a(1), a(2)], "three and a half seconds is past three - for the holder");
    }

    /// A client at the code step is given time to type: four TOTP periods, not
    /// the fifteen seconds of a listener - which reaped it mid-typing, after
    /// which its code got no answer and the client waited for ever (review 70).
    #[test]
    fn a_client_at_the_code_step_gets_time_to_type() {
        let ms = |m: u64| std::time::Duration::from_millis(m);
        assert!(timeout_secs(false, true) > timeout_secs(false, false));
        assert_eq!(timeout_secs(false, true), TOTP_ENTRY_TIMEOUT_SECS);
        assert!(!silent_too_long(ms(16_000), false, true), "sixteen seconds at the code step is not silence");
        assert!(silent_too_long(ms(120_001), false, true));
        assert_eq!(timeout_secs(true, true), TX_HOLDER_TIMEOUT_SECS, "a holder is a holder, whatever step it is at");
    }

    /// Not so soon that an ordinary hiccup drops a transmission: heartbeats come
    /// every 500 ms, so the deadline has to be several of them.
    #[test]
    fn the_short_deadline_still_allows_several_missed_heartbeats() {
        assert!(timeout_secs(true, false) * 1000 >= 4 * 500);
    }
}

/// The per-session fields of `ClientSession`, and the control that restores each.
///
/// Why this exists: the client has to replay every per-session setting after it
/// comes back on a new address, and until 2026-09-13 the two lists that describe
/// that - the defaults here and the sends in the client's restore block - were
/// maintained by hand in two crates. They had drifted to thirteen missing
/// entries, four of which nobody restored at all. Nothing complained, because
/// nothing compared them.
///
/// The test below compares them. It reads this file's own source, so a field
/// added to `ClientSession` without a line here fails the suite rather than
/// waiting for someone to notice their audio is gone after a network switch.
#[cfg(test)]
mod session_scope_tests {
    use sdr_remote_core::protocol::{ControlId, SESSION_SCOPED_CONTROLS};

    /// Per-session state: lost on a new session, so the client must send it again.
    const RESTORED_BY: &[(&str, ControlId)] = &[
        ("spectrum_enabled", ControlId::SpectrumEnable),
        ("spectrum_fps", ControlId::SpectrumFps),
        ("spectrum_zoom", ControlId::SpectrumZoom),
        ("spectrum_pan", ControlId::SpectrumPan),
        ("spectrum_max_bins", ControlId::SpectrumMaxBins),
        ("rx1_enabled", ControlId::Rx1Enable),
        ("rx2_enabled", ControlId::Rx2Enable),
        ("rx2_spectrum_enabled", ControlId::Rx2SpectrumEnable),
        ("rx2_spectrum_fps", ControlId::Rx2SpectrumFps),
        ("rx2_spectrum_zoom", ControlId::Rx2SpectrumZoom),
        ("rx2_spectrum_pan", ControlId::Rx2SpectrumPan),
        ("rx2_spectrum_max_bins", ControlId::Rx2SpectrumMaxBins),
        ("vfo_sync", ControlId::VfoSync),
        ("yaesu_enabled", ControlId::YaesuEnable),
        ("yaesu_state_enabled", ControlId::YaesuStateEnable),
        ("yaesu2_enabled", ControlId::Yaesu2Enable),
        ("yaesu2_state_enabled", ControlId::Yaesu2StateEnable),
        ("vrx1_audio_enabled", ControlId::VrxEnable),
        ("vrx2_audio_enabled", ControlId::VrxEnable2),
        ("vrx1_spectrum_enabled", ControlId::VrxSpectrumEnable),
        ("vrx2_spectrum_enabled", ControlId::VrxSpectrumEnable2),
        ("vrx1_autotune_enabled", ControlId::VrxSamAutoTune),
        ("vrx2_autotune_enabled", ControlId::VrxSamAutoTune2),
        ("audio_mode", ControlId::AudioMode),
        ("dx_spots_enabled", ControlId::DxSpotsEnabled),
        ("full_spectrum_enabled", ControlId::FullSpectrumEnabled),
        ("allow_zoom_below_2x", ControlId::AllowZoomBelow2x),
        ("smeter_sources", ControlId::SmeterSources),
        ("thetis_wideband_audio", ControlId::ThetisWidebandAudio),
    ];

    /// Bookkeeping: the server owns these and a client never asks for them, so
    /// there is nothing to restore. Named one by one on purpose - "everything I
    /// did not think of" is not a category.
    const NOT_SESSION_SCOPED: &[&str] = &[
        "addr",
        "last_seen",
        "connected_since",
        "auth_state",
        "last_heartbeat_seq",
        "rtt_ms",
        "loss_percent",
        "jitter_ms",
        // Observed, not asked for: the client puts it on every heartbeat, so a
        // new session re-learns it within half a second without anyone
        // replaying anything.
        "client_build",
        // It IS the session: derived at the password step, gone with it.
        "session_key",
        // Bookkeeping of which numbered controls were applied (phase 2b).
        "controls",
    ];

    /// The `pub` field names of `ClientSession`, read out of this file's source.
    /// Derived rather than listed, because a list of fields is exactly as complete
    /// as whoever last edited it.
    fn declared_fields() -> Vec<String> {
        let src = include_str!("session.rs");
        let start = src
            .find("pub struct ClientSession {")
            .expect("ClientSession struct not found");
        let body = &src[start..];
        let end = body.find("\n}").expect("end of ClientSession not found");
        body[..end]
            .lines()
            .filter_map(|l| {
                let l = l.trim();
                let rest = l.strip_prefix("pub ")?;
                let name = rest.split(':').next()?.trim();
                if name.is_empty() || !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
                    return None;
                }
                Some(name.to_string())
            })
            .collect()
    }

    #[test]
    fn every_field_of_a_client_session_is_either_restored_or_declared_server_owned() {
        let fields = declared_fields();
        assert!(
            fields.len() > 20,
            "the source scan found only {} fields - it has stopped working, \
             which would make this test pass by seeing nothing",
            fields.len()
        );
        for f in &fields {
            let restored = RESTORED_BY.iter().any(|(n, _)| n == f);
            let server_owned = NOT_SESSION_SCOPED.contains(&f.as_str());
            assert!(
                restored ^ server_owned,
                "ClientSession field `{f}` is in neither list (or in both). A new \
                 per-session field must name the control that restores it in \
                 RESTORED_BY, or be declared server-owned in NOT_SESSION_SCOPED. \
                 Getting this wrong means the setting is silently lost after a \
                 network switch."
            );
        }
    }

    #[test]
    fn every_restorable_field_names_a_control_the_client_replays() {
        for (field, ctrl) in RESTORED_BY {
            assert!(
                SESSION_SCOPED_CONTROLS.contains(ctrl),
                "field `{field}` says it is restored by {ctrl:?}, but that control \
                 is not in SESSION_SCOPED_CONTROLS, so no client will ever send it \
                 again after a new session"
            );
        }
    }

    #[test]
    fn the_shared_list_carries_no_control_that_restores_nothing() {
        for ctrl in SESSION_SCOPED_CONTROLS {
            assert!(
                RESTORED_BY.iter().any(|(_, c)| c == ctrl),
                "{ctrl:?} is in SESSION_SCOPED_CONTROLS but restores no field of \
                 ClientSession. Either it is not per-session state, or a field is \
                 missing from RESTORED_BY."
            );
        }
    }
}

#[cfg(test)]
mod mutual_auth_tests {
    use super::*;
    use sdr_remote_core::auth::{compute_hmac, server_proof, session_key, CHALLENGE_TTL_SECS};
    use sdr_remote_core::protocol::{AUTH_ACCEPTED, AUTH_REJECTED, AUTH_TOTP_REQUIRED};
    use std::time::Duration;

    fn addr() -> SocketAddr {
        "10.0.0.5:5000".parse().unwrap()
    }

    fn addr_other() -> SocketAddr {
        "10.0.0.6:5000".parse().unwrap()
    }

    /// A bare heartbeat, as a client sends every half second.
    fn heartbeat() -> sdr_remote_core::protocol::Packet {
        use sdr_remote_core::protocol::{Flags, Header, Packet, PacketType};
        let mut b = [0u8; 20];
        Header::new(PacketType::Heartbeat, Flags::NONE).serialize(&mut b);
        Packet::deserialize(&b).expect("a bare heartbeat reads")
    }

    /// What the operator touches while typing: a control.
    fn control() -> sdr_remote_core::protocol::Packet {
        use sdr_remote_core::protocol::{ControlId, ControlPacket, Packet};
        Packet::Control(ControlPacket { control_id: ControlId::Rx1AfGain, value: 1 })
    }

    #[test]
    fn a_new_client_gets_a_key_and_a_proof_an_old_client_neither() {
        let a = addr();
        let mut m = SessionManager::new(Some("secret99".into()), None);
        let nonce = m.create_challenge(a);
        let nc = [4u8; 16];
        let v = m.verify_auth(a, &compute_hmac("secret99", &nonce, Some(&TEST_BINDING)), Some(nc), bound());
        assert_eq!(v.code, AUTH_ACCEPTED);
        let key = session_key("secret99", &nonce, &nc, Some(&TEST_BINDING));
        assert_eq!(v.proof, Some(server_proof(&key, &nonce, &nc, Some(&TEST_BINDING))));
        assert_eq!(m.clients.get(&a).unwrap().session_key, Some(key));

        let mut m = SessionManager::new(Some("secret99".into()), None);
        let nonce = m.create_challenge(a);
        let v = m.verify_auth(a, &compute_hmac("secret99", &nonce, Some(&TEST_BINDING)), None, bound());
        assert_eq!(v.code, AUTH_ACCEPTED);
        assert_eq!(v.proof, None, "an old client gets the old answer");
        assert_eq!(m.clients.get(&a).unwrap().session_key, None);
    }

    /// Over QUIC the answer, the key and the proof all cover the connection's
    /// binding (review 108, point 4): an answer made on another TLS session (a
    /// party in the middle) or without the binding is refused, and the proof
    /// the server gives back only verifies with the same binding.
    #[test]
    fn over_quic_only_an_answer_on_the_same_binding_is_admitted() {
        let a = addr();
        let (bound, other) = ([1u8; 32], [2u8; 32]);
        let nc = [4u8; 16];

        let mut m = SessionManager::new(Some("secret99".into()), None);
        let nonce = m.create_challenge(a);
        let quic = |b: [u8; 32]| AuthChannel { binding: b, peer: None };
        let v = m.verify_auth(a, &compute_hmac("secret99", &nonce, Some(&bound)), Some(nc), quic(bound));
        assert_eq!(v.code, AUTH_ACCEPTED);
        let key = session_key("secret99", &nonce, &nc, Some(&bound));
        assert_eq!(v.proof, Some(server_proof(&key, &nonce, &nc, Some(&bound))), "the proof covers the binding");
        assert_eq!(m.clients.get(&a).unwrap().session_key, Some(key), "and so does the key");

        for (what, hmac_binding) in [("another session's binding", Some(&other)), ("no binding", None)] {
            let mut m = SessionManager::new(Some("secret99".into()), None);
            let nonce = m.create_challenge(a);
            let v = m.verify_auth(a, &compute_hmac("secret99", &nonce, hmac_binding), Some(nc), quic(bound));
            assert_eq!(v.code, AUTH_REJECTED, "{what}");
            assert_eq!(v.proof, None, "{what}: no proof for a refused answer");
        }
    }

    /// The byte counters of timed-out clients are dropped where the sessions
    /// are reaped, not only when the status panel draws (review 109: a server
    /// without a screen kept them for ever). A text pin: the counters are a
    /// process-wide table no test owns.
    #[test]
    fn the_reaper_drops_the_byte_counters_of_clients_that_are_gone() {
        let src = include_str!("session.rs");
        let start = src.find("pub fn check_timeout(&mut self)").unwrap();
        let body = &src[start..start + src[start..].find("
    }
").unwrap()];
        assert!(body.contains(concat!("crate::tracked_socket::", "bw_retain(&left)")), "check_timeout drops them");
    }

    /// Over QUIC the brake counts the real peer, not the sentinel: wrong answers
    /// from five connections - five sentinels - block that peer (review 109).
    #[test]
    fn the_brake_counts_the_peer_behind_new_quic_connections() {
        let peer: SocketAddr = "198.18.1.2:40000".parse().unwrap();
        let mut m = SessionManager::new(Some("secret99".into()), None);
        for n in 1..=MAX_AUTH_FAILURES {
            let sentinel: SocketAddr = format!("198.51.100.1:{n}").parse().unwrap();
            let nonce = m.create_challenge(sentinel);
            let channel = AuthChannel { binding: [1u8; 32], peer: Some(peer) };
            let v = m.verify_auth(sentinel, &compute_hmac("guess", &nonce, Some(&[1u8; 32])), Some([4u8; 16]), channel);
            assert_eq!(v.code, AUTH_REJECTED);
        }
        assert!(m.is_blocked(peer), "the peer is blocked after {} wrong answers on new connections", MAX_AUTH_FAILURES);
        assert!(!m.is_blocked("198.51.100.1:1".parse().unwrap()), "not the sentinel");
    }

    #[test]
    fn the_proof_comes_with_totp_required_and_the_key_is_held() {
        let a = addr();
        let secret = sdr_remote_core::auth::generate_totp_secret();
        let mut m = SessionManager::new(Some("secret99".into()), Some(secret.clone()));
        let nonce = m.create_challenge(a);
        let nc = [4u8; 16];
        let v = m.verify_auth(a, &compute_hmac("secret99", &nonce, Some(&TEST_BINDING)), Some(nc), bound());
        assert_eq!(v.code, AUTH_TOTP_REQUIRED);
        let key = session_key("secret99", &nonce, &nc, Some(&TEST_BINDING));
        assert_eq!(v.proof, Some(server_proof(&key, &nonce, &nc, Some(&TEST_BINDING))), "the proof is about the password, not the code");
        assert_eq!(m.clients.get(&a).unwrap().session_key, Some(key));
        assert_eq!(m.verify_totp(a, &sdr_remote_core::auth::generate_totp(&secret), a), Some(true));
        assert_eq!(m.clients.get(&a).unwrap().session_key, Some(key), "the code step keeps the key");
    }

    /// A slow answer is not a wrong one.
    #[test]
    fn an_expired_challenge_is_refused_without_counting_as_a_failure() {
        let a = addr();
        let mut m = SessionManager::new(Some("secret99".into()), None);
        let nonce = m.create_challenge(a);
        m.clients.get_mut(&a).unwrap().auth_state = AuthState::PendingChallenge {
            nonce,
            sent_at: Instant::now() - Duration::from_secs(CHALLENGE_TTL_SECS + 1),
        };
        let v = m.verify_auth(a, &compute_hmac("secret99", &nonce, Some(&TEST_BINDING)), Some([4u8; 16]), bound());
        assert_eq!(v, AuthVerdict { code: AUTH_NOT_ANSWERED, proof: None }, "nothing goes back");
        assert!(m.auth_failures.failures.get(&a).is_none(), "a slow client is not an attacker");
        assert!(
            m.get_auth_state(a).is_none(),
            "the stale challenge is gone, so the next packet from this address gets a fresh one"
        );
    }

    /// The property the whole patch rests on: the server proves itself only to
    /// a client that has already proven itself. Written after a reviewer moved
    /// the proof above the HMAC check and nothing failed (review 66).
    #[test]
    fn a_wrong_password_gets_no_proof_and_no_key() {
        let a = addr();
        let mut m = SessionManager::new(Some("secret99".into()), None);
        let nonce = m.create_challenge(a);
        let v = m.verify_auth(a, &compute_hmac("secret98", &nonce, Some(&TEST_BINDING)), Some([4u8; 16]), bound());
        assert_eq!(v, AuthVerdict { code: AUTH_REJECTED, proof: None });
        assert!(m.clients.get(&a).is_none(), "a refused client that sent a nonce holds no entry (build 77)");
        assert!(m.auth_failures.failures.get(&a).is_some(), "a wrong password IS a failure");
        // An older client keeps its entry, and no key in it.
        let b = addr_other();
        let nonce = m.create_challenge(b);
        let v = m.verify_auth(b, &compute_hmac("secret98", &nonce, Some(&TEST_BINDING)), None, bound());
        assert_eq!(v, AuthVerdict { code: AUTH_REJECTED, proof: None });
        assert_eq!(m.clients.get(&b).unwrap().session_key, None);
    }

    /// A refused password frees the address of a client that falls silent and
    /// comes back with a Connect (v2.13+, it sent a nonce): the next packet
    /// gets a fresh challenge instead of fifteen seconds of nothing. An older
    /// client keeps heartbeating through the refusal and would answer every
    /// challenge with the same wrong password: its entry stays (review 76).
    #[test]
    fn a_refused_password_frees_the_address_of_a_new_client_only() {
        let a = addr();
        let mut m = SessionManager::new(Some("secret99".into()), None);
        let nonce = m.create_challenge(a);
        assert!(!m.wants_challenge(a, &heartbeat()), "pending: no second challenge");
        assert_eq!(m.verify_auth(a, &compute_hmac("wrong", &nonce, Some(&TEST_BINDING)), Some([4u8; 16]), bound()).code, AUTH_REJECTED);
        assert!(m.wants_challenge(a, &heartbeat()), "refused, and the next packet is a fresh start");
        assert!(!m.is_blocked(a), "one failure is counted, not blocked");

        let b = addr_other();
        let nonce = m.create_challenge(b);
        assert_eq!(m.verify_auth(b, &compute_hmac("wrong", &nonce, Some(&TEST_BINDING)), None, bound()).code, AUTH_REJECTED);
        assert!(!m.wants_challenge(b, &heartbeat()), "an older client keeps its entry for the listener's deadline");
    }

    /// A response that answers nothing is not a wrong password: five forged ones
    /// used to block the address they named for a minute (review 66).
    #[test]
    fn an_answer_without_a_pending_challenge_counts_no_failure() {
        let a = addr();
        let mut m = SessionManager::new(Some("secret99".into()), None);
        for _ in 0..MAX_AUTH_FAILURES + 1 {
            let v = m.verify_auth(a, &[0u8; 32], None, bound());
            assert_eq!(v.code, AUTH_NOT_ANSWERED, "nothing pending, nothing said");
        }
        assert!(!m.is_blocked(a), "an address with nothing pending cannot be talked onto the block list");
        assert!(m.auth_failures.failures.get(&a).is_none());
        assert_eq!(m.verify_totp(a, "000000", a), None, "a server without 2FA says nothing to a code");
        // And a client past the password step is not counted either.
        let secret = sdr_remote_core::auth::generate_totp_secret();
        let mut m = SessionManager::new(Some("secret99".into()), Some(secret));
        let nonce = m.create_challenge(a);
        assert_eq!(m.verify_auth(a, &compute_hmac("secret99", &nonce, Some(&TEST_BINDING)), None, bound()).code, AUTH_TOTP_REQUIRED);
        assert_eq!(m.verify_auth(a, &compute_hmac("secret99", &nonce, Some(&TEST_BINDING)), None, bound()).code, AUTH_NOT_ANSWERED);
        assert!(m.auth_failures.failures.get(&a).is_none());
        assert_eq!(m.verify_totp(a, "000000", a), Some(false), "a wrong code with a pending step IS a failure");
        assert_eq!(m.verify_totp(addr_other(), "000000", addr_other()), None, "a code with nothing pending is not");
    }

    /// A knock at the code step is a new client, not the one typing: it gets a
    /// fresh challenge. Until build 72 the address stayed locked for the whole
    /// code-step deadline (review 71).
    #[test]
    fn a_knock_at_the_code_step_gets_a_new_challenge() {
        let a = addr();
        let secret = sdr_remote_core::auth::generate_totp_secret();
        let mut m = SessionManager::new(Some("secret99".into()), Some(secret.clone()));
        assert!(m.wants_challenge(a, &heartbeat()) && m.wants_challenge(a, &control()), "a stranger, whatever it sends");
        assert!(
            !m.wants_challenge(a, &sdr_remote_core::protocol::Packet::Disconnect),
            "but a goodbye is no arrival (field test, build 111)"
        );
        let nonce = m.create_challenge(a);
        assert!(!m.wants_challenge(a, &heartbeat()), "answering a pending challenge");
        assert_eq!(m.verify_auth(a, &compute_hmac("secret99", &nonce, Some(&TEST_BINDING)), Some([4u8; 16]), bound()).code, AUTH_TOTP_REQUIRED);
        assert!(m.clients.get(&a).unwrap().session_key.is_some());
        assert!(
            !m.wants_challenge(a, &heartbeat()),
            "a heartbeat on a FRESH code step is the client's own, still in flight - it cost the attempt in build 73"
        );
        // The grace in absolute numbers too, or a constant measured against
        // itself could grow to a minute with every test green (review 74).
        m.clients.get_mut(&a).unwrap().last_seen = Instant::now() - Duration::from_secs(1);
        assert!(!m.wants_challenge(a, &heartbeat()), "one second: the client's own heartbeat may still be in flight");
        m.clients.get_mut(&a).unwrap().last_seen = Instant::now() - Duration::from_secs(3);
        assert!(m.wants_challenge(a, &heartbeat()), "three seconds: a restart waits no longer than this");
        m.clients.get_mut(&a).unwrap().last_seen = Instant::now() - Duration::from_secs(TOTP_KNOCK_GRACE_SECS + 1);
        assert!(
            !m.wants_challenge(a, &control()),
            "a control from the client that is typing is not a knock - it cost the attempt in build 72"
        );
        assert!(m.wants_challenge(a, &heartbeat()), "at a code step older than the grace, a heartbeat is a new client");
        let nonce = m.create_challenge(a);
        assert!(!m.wants_challenge(a, &heartbeat()), "and the old attempt is gone: a challenge is pending again");
        assert_eq!(m.clients.get(&a).unwrap().session_key, None, "with its key");
        assert_eq!(m.verify_auth(a, &compute_hmac("secret99", &nonce, Some(&TEST_BINDING)), None, bound()).code, AUTH_TOTP_REQUIRED);
        assert_eq!(m.verify_totp(a, &sdr_remote_core::auth::generate_totp(&secret), a), Some(true));
        assert!(!m.wants_challenge(a, &heartbeat()), "authenticated");
    }

    /// The deadline through the reaper, not only through `timeout_secs`: the
    /// code step's two minutes reach `check_timeout` (review 71: that wire was
    /// held by nothing - the flag could be `false` and everything stayed green).
    #[test]
    fn the_reaper_gives_the_code_step_its_time() {
        let a = addr();
        let b = addr_other();
        let secret = sdr_remote_core::auth::generate_totp_secret();
        let mut m = SessionManager::new(Some("secret99".into()), Some(secret));
        let nonce = m.create_challenge(a);
        assert_eq!(m.verify_auth(a, &compute_hmac("secret99", &nonce, Some(&TEST_BINDING)), None, bound()).code, AUTH_TOTP_REQUIRED);
        let mut n = SessionManager::new(Some("secret99".into()), None);
        let nonce_b = n.create_challenge(b);
        assert_eq!(n.verify_auth(b, &compute_hmac("secret99", &nonce_b, Some(&TEST_BINDING)), None, bound()).code, AUTH_ACCEPTED);

        m.clients.get_mut(&a).unwrap().last_seen = Instant::now() - Duration::from_secs(20);
        n.clients.get_mut(&b).unwrap().last_seen = Instant::now() - Duration::from_secs(20);
        assert!(m.check_timeout().is_empty(), "twenty seconds at the code step: still typing");
        assert_eq!(n.check_timeout(), vec![b], "twenty seconds silent and authenticated: gone");

        m.clients.get_mut(&a).unwrap().last_seen = Instant::now() - Duration::from_secs(TOTP_ENTRY_TIMEOUT_SECS + 10);
        assert_eq!(m.check_timeout(), vec![a], "past two minutes the code never came");
    }

    /// Admission restarts the listener's clock. Until build 75 a client that
    /// took twenty seconds to type its code was admitted and reaped within the
    /// next tick, because `last_seen` still said twenty seconds (review 74).
    #[test]
    fn admission_after_a_long_code_step_is_not_reaped() {
        let a = addr();
        let secret = sdr_remote_core::auth::generate_totp_secret();
        let mut m = SessionManager::new(Some("secret99".into()), Some(secret.clone()));
        let nonce = m.create_challenge(a);
        assert_eq!(m.verify_auth(a, &compute_hmac("secret99", &nonce, Some(&TEST_BINDING)), None, bound()).code, AUTH_TOTP_REQUIRED);
        m.clients.get_mut(&a).unwrap().last_seen = Instant::now() - Duration::from_secs(20);
        assert!(m.check_timeout().is_empty(), "still typing");
        assert_eq!(m.verify_totp(a, &sdr_remote_core::auth::generate_totp(&secret), a), Some(true));
        assert!(m.check_timeout().is_empty(), "admitted a moment ago, and already gone");
        assert!(m.is_authenticated(a));

        // And the password step without 2FA, for symmetry.
        let b = addr_other();
        let mut n = SessionManager::new(Some("secret99".into()), None);
        let nonce = n.create_challenge(b);
        n.clients.get_mut(&b).unwrap().last_seen = Instant::now() - Duration::from_secs(CHALLENGE_TTL_SECS - 1);
        assert_eq!(n.verify_auth(b, &compute_hmac("secret99", &nonce, Some(&TEST_BINDING)), None, bound()).code, AUTH_ACCEPTED);
        assert!(n.clients.get(&b).unwrap().last_seen.elapsed() < Duration::from_secs(1), "admission stamps the clock");
    }

    /// The relation build 67 repaired, held by a test instead of a comment: a
    /// challenge must be able to expire before the reaper removes the client,
    /// or the expiry is dead code (review 67: 30 back in and all green).
    #[test]
    fn a_challenge_expires_before_the_reaper_removes_the_client() {
        assert!(
            CHALLENGE_TTL_SECS < timeout_secs(false, false),
            "CHALLENGE_TTL_SECS ({}) must be under the session time-out ({}) or it can never fire",
            CHALLENGE_TTL_SECS,
            timeout_secs(false, false)
        );
    }

    /// And one that is still fresh is answered as before.
    #[test]
    fn a_fresh_challenge_is_still_answerable_just_inside_the_ttl() {
        let a = addr();
        let mut m = SessionManager::new(Some("secret99".into()), None);
        let nonce = m.create_challenge(a);
        m.clients.get_mut(&a).unwrap().auth_state = AuthState::PendingChallenge {
            nonce,
            sent_at: Instant::now() - Duration::from_secs(CHALLENGE_TTL_SECS - 1),
        };
        assert_eq!(m.verify_auth(a, &compute_hmac("secret99", &nonce, Some(&TEST_BINDING)), None, bound()).code, AUTH_ACCEPTED);
    }
}

/// The permit to punch follows the session (build 127, review 126): the watch
/// hears every admission - password, code, a server without a password - and
/// every session that ends - a goodbye, a time-out. Both states of each.
#[cfg(test)]
mod admission_watch_tests {
    use super::*;
    use sdr_remote_core::auth::compute_hmac;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Default)]
    struct Heard(Mutex<Vec<(&'static str, SocketAddr)>>);

    impl AdmissionWatch for Heard {
        fn admitted(&self, addr: SocketAddr) {
            self.0.lock().unwrap().push(("in", addr));
        }
        fn gone(&self, addr: SocketAddr) {
            self.0.lock().unwrap().push(("out", addr));
        }
    }

    fn watched(password: Option<&str>, totp: Option<String>) -> (SessionManager, Arc<Heard>) {
        let mut m = SessionManager::new(password.map(str::to_string), totp);
        let heard = Arc::new(Heard::default());
        m.watch_admissions(heard.clone());
        (m, heard)
    }

    fn a() -> SocketAddr {
        "10.0.0.5:5000".parse().unwrap()
    }

    fn heard(h: &Heard) -> Vec<(&'static str, SocketAddr)> {
        std::mem::take(&mut *h.0.lock().unwrap())
    }

    #[test]
    fn an_accepted_password_admits_and_a_goodbye_ends_it() {
        let (mut m, h) = watched(Some("secret99"), None);
        let nonce = m.create_challenge(a());
        assert_eq!(heard(&h), vec![], "a challenge is only a knock");
        m.verify_auth(a(), &compute_hmac("secret99", &nonce, Some(&TEST_BINDING)), Some([4u8; 16]), bound());
        assert_eq!(heard(&h), vec![("in", a())]);
        m.remove(a());
        assert_eq!(heard(&h), vec![("out", a())]);
    }

    /// The snapshot carries each client's way from the lookup (build 143), and
    /// none without one.
    #[test]
    fn the_snapshot_names_each_client_s_way() {
        struct Fixed;
        impl WayLookup for Fixed {
            fn way(&self, addr: SocketAddr) -> Option<sdr_remote_core::path::PathKind> {
                (addr == a()).then_some(sdr_remote_core::path::PathKind::LocalViaRelay)
            }
        }
        let (mut m, _h) = watched(None, None);
        m.create_challenge(a());
        assert_eq!(m.active_clients_snapshot().iter().map(|c| c.path).collect::<Vec<_>>(), vec![None], "no lookup yet");
        m.see_ways(Arc::new(Fixed));
        assert_eq!(
            m.active_clients_snapshot().iter().map(|c| c.path).collect::<Vec<_>>(),
            vec![Some(sdr_remote_core::path::PathKind::LocalViaRelay)]
        );
    }

    #[test]
    fn a_wrong_password_admits_nothing() {
        let (mut m, h) = watched(Some("secret99"), None);
        let nonce = m.create_challenge(a());
        m.verify_auth(a(), &compute_hmac("wrong", &nonce, Some(&TEST_BINDING)), Some([4u8; 16]), bound());
        assert_eq!(heard(&h), vec![("out", a())], "refused: never in, and the waiting session ends");
    }

    #[test]
    fn with_a_code_the_code_admits_not_the_password() {
        let secret = sdr_remote_core::auth::generate_totp_secret();
        let (mut m, h) = watched(Some("secret99"), Some(secret.clone()));
        let nonce = m.create_challenge(a());
        m.verify_auth(a(), &compute_hmac("secret99", &nonce, Some(&TEST_BINDING)), Some([4u8; 16]), bound());
        assert_eq!(heard(&h), vec![], "the password alone is not the admission");
        assert_eq!(m.verify_totp(a(), &sdr_remote_core::auth::generate_totp(&secret), a()), Some(true));
        assert_eq!(heard(&h), vec![("in", a())]);
    }

    /// A knock again at the code step replaces the waiting session: the one it
    /// replaces ends the one way a session ends, and the watch hears it - by
    /// itself, not by the gate that only lets unadmitted clients get there
    /// (review 127).
    #[test]
    fn a_new_challenge_ends_the_session_it_replaces() {
        let secret = sdr_remote_core::auth::generate_totp_secret();
        let (mut m, h) = watched(Some("secret99"), Some(secret));
        let nonce = m.create_challenge(a());
        m.verify_auth(a(), &compute_hmac("secret99", &nonce, Some(&TEST_BINDING)), Some([4u8; 16]), bound());
        assert_eq!(heard(&h), vec![]);
        m.create_challenge(a());
        assert_eq!(heard(&h), vec![("out", a())], "the session at the code step ended");
        m.create_challenge("10.0.0.9:1".parse().unwrap());
        assert_eq!(heard(&h), vec![], "a first knock replaces nothing");
    }

    #[test]
    fn a_server_without_a_password_admits_at_first_contact_and_a_time_out_ends_it() {
        let (mut m, h) = watched(None, None);
        m.touch(a());
        assert_eq!(heard(&h), vec![("in", a())]);
        m.touch(a());
        assert_eq!(heard(&h), vec![], "once");
        m.clients.get_mut(&a()).unwrap().last_seen = Instant::now() - Duration::from_secs(SESSION_TIMEOUT_SECS + 1);
        assert_eq!(m.check_timeout(), vec![a()]);
        assert_eq!(heard(&h), vec![("out", a())], "the session that timed out takes its permit");
    }
}
