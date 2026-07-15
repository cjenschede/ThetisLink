// SPDX-License-Identifier: GPL-2.0-or-later

//! Outbound relay transport monitor for ThetisLink.
//!
//! Phase C-light establishes the WebSocket relay path and exchanges lightweight
//! peer-status/RTT frames. It still does not route TL audio/control frames, so
//! the existing direct transport is unaffected.

use futures_util::{SinkExt, StreamExt};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::MaybeTlsStream;

const RECONNECT_DELAY: Duration = Duration::from_secs(5);
const PEER_WAIT_PING: Duration = Duration::from_secs(10);
const PEER_STATUS_INTERVAL: Duration = Duration::from_secs(5);
const PEER_RTT_INTERVAL: Duration = Duration::from_secs(15);
/// Revert to "waiting for peer" when no peer frame arrives within this window.
/// The peer sends a status frame every PEER_STATUS_INTERVAL, so this tolerates
/// roughly three missed status heartbeats before the status is downgraded.
const PEER_TIMEOUT: Duration = Duration::from_secs(15);

/// Binary transport probe: prove that Message::Binary frames round-trip
/// peer<->peer through the relay and measure their RTT on an audio-sized
/// payload. Latency is the key open question before any real audio is routed
/// through a WebSocket/TCP relay, so we measure it early. Still status-only:
/// no TL frames are routed.
const BINARY_PROBE_INTERVAL: Duration = Duration::from_secs(1); // 1 Hz sampling for jitter
const BINARY_RTT_WINDOW: usize = 30; // rolling window (~30 s) for min/avg/max
const BINARY_PROBE_LEN: usize = 256; // representative audio-frame payload size
const BINARY_MAGIC: [u8; 4] = *b"TLB1";
const BINARY_KIND_PING: u8 = 0;
const BINARY_KIND_PONG: u8 = 1;

// TLT1 = getunnelde TL-frame-data (Phase C), onderscheiden van de TLB1-probe.
// Layout: magic(4) | version(1) | client_id(1) | length(2 LE) | payload(TL-bytes).
// Een eigen magic (i.p.v. een losse prefix) is robuust omdat echte TL-payloads
// ook binair zijn; een binary-frame met onbekende magic wordt non-fataal gedropt.
const TLT1_MAGIC: [u8; 4] = *b"TLT1";
const TLT1_VERSION: u8 = 1;
const TLT1_HEADER_LEN: usize = 8;

// TLU1 = getunnelde audio over het kale-UDP-pad (PATCH-relay-audio-udp, fase 2).
// Layout: magic(4) | version(1) | flags(1) | client_id(1) | seq(4 LE) | token(32 raw) |
// AudioPacket. Alleen audio-packettypes gaan hierover; de rest blijft TLT1/wss.
const TLU1_MAGIC: [u8; 4] = *b"TLU1";
const TLU1_VERSION: u8 = 1;
const TLU1_TOKEN_LEN: usize = 32;
const TLU1_HEADER_LEN: usize = 4 + 1 + 1 + 1 + 4 + TLU1_TOKEN_LEN; // = 43

/// Opus-audio packet types (byte [2] of the TL header) that should travel over the
/// low-latency UDP path. Everything else (control, spectrum, meters) stays on wss.
/// Mirrors `sdr_remote_core::protocol::PacketType`: Audio 0x01, AudioRx2 0x0E,
/// AudioYaesu 0x16, AudioBinR 0x1A, AudioMultiCh 0x1B, AudioVrx 0x21, AudioYaesu2 0x25.
fn is_audio_packet(data: &[u8]) -> bool {
    data.len() >= 4
        && matches!(data[2], 0x01 | 0x0E | 0x16 | 0x1A | 0x1B | 0x21 | 0x25)
}

/// Decode a 64-char hex token into 32 raw bytes (best-effort; bad chars -> 0).
fn hex32(hex: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        if let Some(pair) = hex.get(i * 2..i * 2 + 2) {
            *byte = u8::from_str_radix(pair, 16).unwrap_or(0);
        }
    }
    out
}

/// Build a TLU1 audio datagram. `client_id` is the sender's id (station stamps the
/// target). `token_hex` is the fase-0 capability from the ready-reply.
fn encode_tlu1(client_id: u8, seq: u32, token_hex: &str, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(TLU1_HEADER_LEN + payload.len());
    f.extend_from_slice(&TLU1_MAGIC);
    f.push(TLU1_VERSION);
    f.push(0); // flags (reserved)
    f.push(client_id);
    f.extend_from_slice(&seq.to_le_bytes());
    f.extend_from_slice(&hex32(token_hex));
    f.extend_from_slice(payload);
    f
}

/// Extract `udp_token=<hex>` from the relay ready-reply (fase 0). The token is the
/// last field, so trimming the remainder is enough.
fn parse_udp_token(ready: &str) -> Option<String> {
    ready
        .split("udp_token=")
        .nth(1)
        .map(|s| s.split_whitespace().next().unwrap_or("").to_string())
        .filter(|t| t.len() == TLU1_TOKEN_LEN * 2)
}

// --- Fase 3: make-before-break UDP<->wss fallback (relay-only) ---

/// Relay-control-frame marker. A client tunnels this over wss (TLT1, so the relay stamps
/// the sender's client_id); the station intercepts it BEFORE forwarding to the engine and
/// flips that client's wss-audio fallback. Distinct from any TL packet: TL payloads start
/// with the TL magic, never with this 4-byte marker.
const RCF_MAGIC: &[u8; 4] = b"RCF1";
/// RCF opcode: request wss-audio fallback on/off for the sender's session (byte 5).
const RCF_FALLBACK: u8 = 0x01;
/// Connect-time fallback: if UDP audio NEVER arrives within this window while the tunnel is
/// alive, UDP is likely blocked -> request wss audio.
const FALLBACK_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Mid-session fallback: an established UDP audio stream that goes silent this long ->
/// request wss audio (make-before-break; the station keeps sending UDP too so recovery is
/// detectable).
const FALLBACK_STALL: Duration = Duration::from_millis(500);
/// Recovery: while in fallback, UDP audio must flow healthily for this long before we drop
/// wss and return to UDP-only. Longer than the stall threshold = hysteresis (no flapping).
const FALLBACK_RECOVER: Duration = Duration::from_secs(3);
/// Minimum dwell in a transport state before another flip is allowed (dwell-cap +
/// rate-limit combined). Enforced on BOTH ends so a client that ignores its own dwell
/// still cannot make the station flap.
const FALLBACK_DWELL: Duration = Duration::from_secs(3);
/// While in fallback the client re-asserts its request this often (lease refresh), so the
/// station can expire the state of a client that vanished without sending an off.
const FALLBACK_HEARTBEAT: Duration = Duration::from_secs(5);
/// Station drops a client's fallback state if it has not been re-asserted within this
/// window (>2 heartbeats). Prevents a stale flag from being inherited by a reconnecting
/// client that reuses the same relay client_id.
const FALLBACK_LEASE: Duration = Duration::from_secs(15);

/// Station-side per-client fallback state (keyed by relay client_id). `on` is the current
/// wss-audio decision; `last_change` enforces the dwell; `last_seen` drives the lease so a
/// vanished client's flag expires instead of leaking to a reused id.
struct StationFallback {
    on: bool,
    last_change: Instant,
    last_seen: Instant,
}

/// Build a relay-control-frame requesting wss-audio fallback on/off.
fn encode_rcf_fallback(on: bool) -> Vec<u8> {
    let mut f = Vec::with_capacity(6);
    f.extend_from_slice(RCF_MAGIC);
    f.push(RCF_FALLBACK);
    f.push(on as u8);
    f
}

/// Parse a relay-control-frame: `Some(on)` when it is a wss-audio fallback request.
fn parse_rcf_fallback(payload: &[u8]) -> Option<bool> {
    if payload.len() >= 6 && &payload[0..4] == RCF_MAGIC && payload[4] == RCF_FALLBACK {
        Some(payload[5] != 0)
    } else {
        None
    }
}

/// Audio packet identity for dedup: `(packet_type, sequence)`. AudioPacket layout is
/// header(4: magic,ver,type,flags) + sequence(4, big-endian) + ...; type is byte[2], the
/// sequence is bytes[4..8]. Different audio types (Audio/AudioRx2/AudioVrx/...) have
/// independent sequence spaces, so the type is part of the key.
fn audio_identity(payload: &[u8]) -> Option<(u8, u32)> {
    if is_audio_packet(payload) && payload.len() >= 8 {
        let seq = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
        Some((payload[2], seq))
    } else {
        None
    }
}

/// `true` when an audio frame carries an active PTT flag. Header byte[3] = flags; bit 0
/// (AudioFlags::PTT = 0x01) marks TX. Used for TX-always-both (fase 3b section 14.3).
fn audio_ptt_active(payload: &[u8]) -> bool {
    is_audio_packet(payload) && payload.len() > 3 && (payload[3] & 0x01) != 0
}

/// Sliding window that drops audio frames already delivered over the other transport, so a
/// frame carried on BOTH paths (make-before-break overlap / TX-always-both) reaches the
/// receiver once. Kept in the transport layer so the jitter estimator is never perturbed
/// by duplicates (pre-buffer dedup); the jitter buffer itself stays unchanged. Only audio is
/// deduped. Keyed by (client_id, packet_type, sequence) so concurrent clients on a
/// station's uplink never collide.
struct DedupWindow {
    order: std::collections::VecDeque<(u8, u8, u32)>,
    seen: std::collections::HashSet<(u8, u8, u32)>,
}

impl DedupWindow {
    const CAP: usize = 512; // ~10 s of 50 fps audio; ample across clients + path overlap

    fn new() -> Self {
        Self {
            order: std::collections::VecDeque::with_capacity(Self::CAP + 1),
            seen: std::collections::HashSet::with_capacity(Self::CAP + 1),
        }
    }

    /// `true` if this (client_id, audio frame) is a duplicate already delivered (drop it);
    /// `false` for a fresh frame (records it) or any non-audio payload (never deduped).
    fn is_duplicate(&mut self, cid: u8, payload: &[u8]) -> bool {
        let Some((ty, seq)) = audio_identity(payload) else {
            return false;
        };
        let key = (cid, ty, seq);
        if !self.seen.insert(key) {
            return true;
        }
        self.order.push_back(key);
        if self.order.len() > Self::CAP {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        false
    }
}

/// Host portion of a relay URL (`wss://host[:port][/path]` -> `host`).
fn url_host(url: &str) -> Option<String> {
    let s = url.trim();
    let s = s
        .strip_prefix("wss://")
        .or_else(|| s.strip_prefix("ws://"))
        .unwrap_or(s);
    let host = s.split(['/', ':']).next()?;
    (!host.is_empty()).then(|| host.to_string())
}

/// Open + connect a UDP socket to the relay's UDP data port (fase 2). Best-effort;
/// on any failure the caller falls back to audio-over-wss.
async fn open_udp(url: &str, port: u16) -> Result<UdpSocket, String> {
    let host = url_host(url).ok_or_else(|| "bad relay url".to_string())?;
    let addr = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| e.to_string())?
        .next()
        .ok_or_else(|| "relay UDP address not resolved".to_string())?;
    let bind = if addr.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" };
    let sock = UdpSocket::bind(bind).await.map_err(|e| e.to_string())?;
    sock.connect(addr).await.map_err(|e| e.to_string())?;
    Ok(sock)
}

/// Await a datagram on the UDP audio socket, or pend forever when UDP is not up (so
/// the `select!` branch is inert without busy-spinning).
async fn recv_udp(sock: &Option<UdpSocket>, buf: &mut [u8]) -> Option<usize> {
    match sock {
        Some(s) => s.recv(buf).await.ok(),
        None => std::future::pending().await,
    }
}

// Relay sentinel convention, mirrored from the server crate's
// `tracked_socket::relay_sentinel_for`. The server is the downstream consumer of
// these synthetic addresses, so the convention cannot be imported here without a
// dependency cycle; it is kept in sync by hand. Mapping: client_id N <-> 203.0.113.(N+1):1.
const RELAY_SENTINEL_PORT: u16 = 1;

/// Synthetic per-client sentinel address for relay `client_id`.
fn sentinel_for_client_id(id: u8) -> SocketAddr {
    let last = 1u16 + (u16::from(id) % 254);
    SocketAddr::from((Ipv4Addr::new(203, 0, 113, last as u8), RELAY_SENTINEL_PORT))
}

/// Recover the relay `client_id` from a sentinel address (inverse of the above).
fn client_id_from_sentinel(addr: SocketAddr) -> u8 {
    match addr.ip() {
        IpAddr::V4(ip) => ip.octets()[3].saturating_sub(1),
        IpAddr::V6(_) => 0,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelayRole {
    Station,
    Client,
}

impl RelayRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Station => "station",
            Self::Client => "client",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelayConfig {
    pub enabled: bool,
    pub url: String,
    pub station: String,
    pub token: String,
    pub role: RelayRole,
    /// Stable per-install id sent to the relay so a reconnecting client reclaims
    /// its own slot instead of consuming a new one (prevents room-full pile-up from
    /// one device relaunching/backgrounding). Empty = omitted (relay allocates a new
    /// slot, legacy behavior).
    pub instance: String,
    /// Optional human device label (may contain spaces, e.g. "Surface Pro"), sent as
    /// the LAST hello field. Metadata only - the relay uses it just for logs. Empty
    /// = omitted.
    pub name: String,
    /// UDP port of the relay's audio data path (PATCH-relay-audio-udp, fase 2).
    /// `Some(port)` = route audio over kale UDP to `host(url):port`; `None` = all
    /// traffic over wss as before (additive). Falls back to wss if UDP can't open.
    pub udp_port: Option<u16>,
}

/// Default relay UDP audio port (the proven, firewall-friendly 443).
pub const DEFAULT_UDP_PORT: u16 = 443;

/// UDP audio-path port from the environment (`THETISLINK_RELAY_UDP_PORT`). An override
/// for testing/non-standard ports; unset -> falls through to the config setting.
pub fn relay_udp_port_env() -> Option<u16> {
    std::env::var("THETISLINK_RELAY_UDP_PORT")
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Resolve the UDP audio port: the env var wins (testing override); otherwise the
/// config toggle decides (`true` -> DEFAULT_UDP_PORT, `false` -> off = wss only).
pub fn relay_udp_port_resolve(config_enabled: bool) -> Option<u16> {
    if let Some(p) = relay_udp_port_env() {
        Some(p)
    } else if config_enabled {
        Some(DEFAULT_UDP_PORT)
    } else {
        None
    }
}

impl RelayConfig {
    pub fn is_ready(&self) -> bool {
        self.enabled
            && !self.url.trim().is_empty()
            && !self.station.trim().is_empty()
            && !self.token.trim().is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RelayPhase {
    Disabled,
    WaitingForConfig,
    Connecting,
    Authenticated,
    WaitingForPeer,
    Disconnected,
    Error,
}

/// Rolling statistics for the binary transport probe (Message::Binary round-trip
/// through the relay on an audio-sized payload). Distinct from `peer_rtt_ms`,
/// which measures the small text heartbeat.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BinaryProbeStats {
    pub last_ms: u32,
    pub min_ms: u32,
    pub max_ms: u32,
    pub avg_ms: u32,
    pub samples: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelayStatus {
    pub phase: RelayPhase,
    pub message: String,
    pub connected: bool,
    pub peer_seen: bool,
    pub last_error: Option<String>,
    pub peer_rtt_ms: Option<u32>,
    pub binary: Option<BinaryProbeStats>,
    pub generation: u64,
    /// Fase 3c: true while audio is on the wss TCP-fallback (UDP degraded/blocked); false
    /// on the normal low-latency UDP path. Drives the client transport indicator.
    pub transport_fallback: bool,
}

impl RelayStatus {
    pub fn disabled() -> Self {
        Self {
            phase: RelayPhase::Disabled,
            message: "Relay disabled".to_string(),
            connected: false,
            peer_seen: false,
            last_error: None,
            peer_rtt_ms: None,
            binary: None,
            generation: 0,
            transport_fallback: false,
        }
    }

    pub fn label(&self) -> String {
        match self.phase {
            RelayPhase::Disabled => "Disabled".to_string(),
            RelayPhase::WaitingForConfig => "Waiting for configuration".to_string(),
            RelayPhase::Connecting => "Connecting".to_string(),
            RelayPhase::Authenticated => {
                let mut label = match self.peer_rtt_ms {
                    Some(ms) => format!("Peer online (RTT {ms} ms)"),
                    None => "Peer online".to_string(),
                };
                if let Some(b) = self.binary {
                    label.push_str(&format!(
                        ", binary {} ms (min {} / avg {} / max {}, n={})",
                        b.last_ms, b.min_ms, b.avg_ms, b.max_ms, b.samples
                    ));
                }
                label
            }
            RelayPhase::WaitingForPeer => "Connected, waiting for peer".to_string(),
            RelayPhase::Disconnected => "Disconnected".to_string(),
            RelayPhase::Error => "Error".to_string(),
        }
    }
}

#[derive(Clone)]
pub struct RelayStatusHandle {
    state: Arc<Mutex<RelayStatus>>,
}

impl RelayStatusHandle {
    pub fn snapshot(&self) -> RelayStatus {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Tunnel-koppeling voor Phase C: draagt getunnelde TL-frame-bytes tussen deze
/// relay-peer en het lokale endpoint (server: `TrackedSocket`; client: engine).
/// - `inbound_tx`: hierheen duwt de monitor gedecodeerde inkomende `TLT1`-payloads
///   als `(sentinel, tl_bytes)`.
/// - `uplink_rx`: hieruit trekt de monitor te-verzenden `(sentinel, tl_bytes)` en
///   verpakt ze in `TLT1` over de WebSocket.
/// - `sentinel`: het synthetische client-adres waarmee inbound getagd wordt
///   (v1: een client). Op de client-kant is dit gewoon het server-adres.
pub struct RelayTunnel {
    pub sentinel: SocketAddr,
    pub inbound_tx: mpsc::UnboundedSender<(SocketAddr, Vec<u8>)>,
    pub uplink_rx: mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)>,
}

pub struct RelayMonitor {
    status: RelayStatusHandle,
    config_tx: watch::Sender<RelayConfig>,
    shutdown_tx: watch::Sender<bool>,
    task: RelayTask,
}

enum RelayTask {
    Tokio(JoinHandle<()>),
    Thread,
}

impl RelayMonitor {
    pub fn start(initial: RelayConfig) -> Self {
        let status = RelayStatusHandle {
            state: Arc::new(Mutex::new(RelayStatus::disabled())),
        };
        let (config_tx, config_rx) = watch::channel(initial);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task_status = status.clone();
        let task = tokio::spawn(async move {
            monitor_loop(config_rx, shutdown_rx, task_status, None).await;
        });
        Self {
            status,
            config_tx,
            shutdown_tx,
            task: RelayTask::Tokio(task),
        }
    }

    /// Status-only monitor op een eigen thread (geen TL-tunnel).
    pub fn start_threaded(initial: RelayConfig) -> Self {
        Self::start_threaded_inner(initial, None)
    }

    /// Monitor op een eigen thread die tevens TL-frames tunnelt (Phase C).
    pub fn start_threaded_tunnel(initial: RelayConfig, tunnel: RelayTunnel) -> Self {
        Self::start_threaded_inner(initial, Some(tunnel))
    }

    fn start_threaded_inner(initial: RelayConfig, tunnel: Option<RelayTunnel>) -> Self {
        let status = RelayStatusHandle {
            state: Arc::new(Mutex::new(RelayStatus::disabled())),
        };
        let (config_tx, config_rx) = watch::channel(initial);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task_status = status.clone();
        let spawn_status = status.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("thetislink-relay-monitor".to_string())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(rt) => rt.block_on(monitor_loop(config_rx, shutdown_rx, task_status.clone(), tunnel)),
                    Err(e) => set_status(
                        &task_status,
                        RelayPhase::Error,
                        format!("Relay runtime error: {e}"),
                        false,
                        false,
                        Some(e.to_string()),
                        0,
                    ),
                }
            })
        {
            set_status(
                &spawn_status,
                RelayPhase::Error,
                format!("Relay thread error: {e}"),
                false,
                false,
                Some(e.to_string()),
                0,
            );
        }
        Self {
            status,
            config_tx,
            shutdown_tx,
            task: RelayTask::Thread,
        }
    }

    pub fn status_handle(&self) -> RelayStatusHandle {
        self.status.clone()
    }

    pub fn update_config(&self, config: RelayConfig) {
        let _ = self.config_tx.send(config);
    }

    pub fn stop(&self) {
        let _ = self.shutdown_tx.send(true);
    }
}

impl Drop for RelayMonitor {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
        if let RelayTask::Tokio(task) = &self.task {
            task.abort();
        }
    }
}

async fn monitor_loop(
    mut config_rx: watch::Receiver<RelayConfig>,
    mut shutdown_rx: watch::Receiver<bool>,
    status: RelayStatusHandle,
    mut tunnel: Option<RelayTunnel>,
) {
    let mut generation = 0_u64;
    loop {
        if *shutdown_rx.borrow() {
            set_status(
                &status,
                RelayPhase::Disabled,
                "Relay monitor stopped".to_string(),
                false,
                false,
                None,
                generation,
            );
            return;
        }

        let config = config_rx.borrow().clone();
        generation = generation.wrapping_add(1);

        if !config.enabled {
            set_status(
                &status,
                RelayPhase::Disabled,
                "Relay disabled".to_string(),
                false,
                false,
                None,
                generation,
            );
            wait_for_change(&mut config_rx, &mut shutdown_rx).await;
            continue;
        }

        if !config.is_ready() {
            set_status(
                &status,
                RelayPhase::WaitingForConfig,
                "Relay enabled, but URL/station/token is incomplete".to_string(),
                false,
                false,
                None,
                generation,
            );
            wait_for_change(&mut config_rx, &mut shutdown_rx).await;
            continue;
        }

        set_status(
            &status,
            RelayPhase::Connecting,
            format!("Connecting to {} as {}", config.url, config.role.as_str()),
            false,
            false,
            None,
            generation,
        );

        let run_result = run_connection(
            &config,
            &status,
            generation,
            &mut config_rx,
            &mut shutdown_rx,
            tunnel.as_mut(),
        )
        .await;
        if *shutdown_rx.borrow() {
            continue;
        }
        if config_rx.borrow().clone() != config {
            continue;
        }

        if let Err(err) = run_result {
            let msg = short_error(&err);
            set_status(
                &status,
                RelayPhase::Error,
                format!("Relay error: {msg}"),
                false,
                false,
                Some(msg),
                generation,
            );
        }

        tokio::select! {
            _ = tokio::time::sleep(RECONNECT_DELAY) => {}
            _ = config_rx.changed() => {}
            _ = shutdown_rx.changed() => {}
        }
    }
}

/// Poll de tunnel-uplink-receiver, of blijf eeuwig pending als er geen tunnel is
/// (zodat de select!-tak dan nooit vuurt).
async fn recv_uplink(
    rx: &mut Option<&mut mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)>>,
) -> Option<(SocketAddr, Vec<u8>)> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

/// Install a rustls crypto provider (ring) exactly once, process-wide. Without this,
/// rustls 0.23 panics on the first wss:// connect ("no process-level CryptoProvider"),
/// since tokio-tungstenite does not select one. Belt-and-suspenders alongside the
/// `ring` crate feature; the Once makes repeat calls (per reconnect) harmless.
fn ensure_crypto_provider() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

async fn run_connection(
    config: &RelayConfig,
    status: &RelayStatusHandle,
    generation: u64,
    config_rx: &mut watch::Receiver<RelayConfig>,
    shutdown_rx: &mut watch::Receiver<bool>,
    tunnel: Option<&mut RelayTunnel>,
) -> Result<(), String> {
    // Split de tunnel in onafhankelijke stukken zodat de select!-takken elkaars
    // borrows niet in de weg zitten: sentinel (Copy), inbound_tx (goedkope clone),
    // en een reborrow van uplink_rx (&mut, blijft over reconnects behouden).
    let (tunnel_sentinel, tunnel_inbound, mut tunnel_uplink) = match tunnel {
        Some(t) => (Some(t.sentinel), Some(t.inbound_tx.clone()), Some(&mut t.uplink_rx)),
        None => (None, None, None),
    };
    // Client-ids currently bound through this connection (station side: one per
    // remote client; client side: just the single server). Cleared on peer timeout.
    let mut bound_clients: HashSet<u8> = HashSet::new();

    ensure_crypto_provider();
    let (ws, _) = connect_async(config.url.as_str())
        .await
        .map_err(|e| e.to_string())?;
    // Disable Nagle: the monitor sends several small frames close together, and
    // Nagle batching adds latency/jitter to a low-rate control path like this.
    // Disable Nagle on the underlying TCP for both plain (ws) and TLS (wss) - the
    // low-latency relay path depends on TCP_NODELAY; without handling the TLS variant
    // a wss:// connection would silently keep Nagle on.
    match ws.get_ref() {
        MaybeTlsStream::Plain(tcp) => {
            let _ = tcp.set_nodelay(true);
        }
        MaybeTlsStream::Rustls(tls) => {
            let _ = tls.get_ref().0.set_nodelay(true);
        }
        _ => {}
    }
    let (mut write, mut read) = ws.split();
    let mut hello = if config.instance.trim().is_empty() {
        format!(
            "TLR1 station={} role={} token={}",
            config.station.trim(),
            config.role.as_str(),
            config.token.trim()
        )
    } else {
        format!(
            "TLR1 station={} role={} token={} instance={}",
            config.station.trim(),
            config.role.as_str(),
            config.token.trim(),
            config.instance.trim()
        )
    };
    // `name=` MUST be last: it is free-form and may contain spaces, so the relay
    // parses it as the remainder of the line. Omitted when empty.
    let name = config.name.trim();
    if !name.is_empty() {
        hello.push_str(" name=");
        hello.push_str(name);
    }
    write
        .send(Message::Text(hello.into()))
        .await
        .map_err(|e| e.to_string())?;

    let first = tokio::time::timeout(Duration::from_secs(10), read.next())
        .await
        .map_err(|_| "relay hello timed out".to_string())?
        .ok_or_else(|| "relay closed before authentication".to_string())?
        .map_err(|e| e.to_string())?;

    // Fase 2: the ready-reply carries the UDP capability token ("OK relay-ready
    // udp_token=..."); capture it to open the audio-over-UDP path below. Assigned on
    // the OK arm; the other arms diverge (return), so it is always initialised after.
    // `mut`: the relay rotates this token over wss before its hard TTL; the loop
    // swaps in each fresh token for outgoing TLU1 packets (see the Text arm below).
    let mut udp_token: Option<String>;
    match first {
        Message::Text(text) if text.starts_with("OK ") => {
            udp_token = parse_udp_token(&text);
            set_status(
                status,
                RelayPhase::WaitingForPeer,
                "Relay authenticated; waiting for peer handshake".to_string(),
                true,
                false,
                None,
                generation,
            );
        }
        Message::Text(text) => return Err(reject_reason(&text)),
        other => return Err(format!("unexpected relay hello: {other:?}")),
    }

    // Fase 2: open the kale-UDP audio path when configured and a token was issued.
    // Best-effort - on any failure we simply keep audio on wss (additive fallback).
    let udp_socket: Option<UdpSocket> = match (config.udp_port, &udp_token) {
        (Some(port), Some(_)) => match open_udp(&config.url, port).await {
            Ok(sock) => {
                log::info!(
                    "Relay UDP audio path up ({}:{port})",
                    url_host(&config.url).unwrap_or_default()
                );
                Some(sock)
            }
            Err(e) => {
                log::warn!("Relay UDP audio path unavailable, using wss: {e}");
                None
            }
        },
        _ => None,
    };
    let mut udp_seq: u32 = 0;
    let mut udp_buf = vec![0u8; 2048];
    // Punch our UDP source to the relay right away so it learns where to send our
    // downlink audio BEFORE we transmit anything (source-binding bootstrap). An
    // empty-payload TLU1 keepalive: the relay learns the src, and the 1-byte framed
    // forward is ignored by the peer (its `n > 1` guard). Repeated on a timer below to
    // keep the NAT mapping alive.
    if let (Some(sock), Some(tok)) = (&udp_socket, &udp_token) {
        udp_seq = udp_seq.wrapping_add(1);
        let _ = sock.send(&encode_tlu1(0, udp_seq, tok, &[])).await;
    }
    let mut last_udp_keepalive = Instant::now();

    write
        .send(Message::Text(peer_frame("hello", config.role).into()))
        .await
        .map_err(|e| e.to_string())?;

    let mut last_ping = Instant::now();
    let mut last_status = Instant::now();
    let mut last_rtt_ping = Instant::now();
    let mut rtt_seq = 0_u64;
    let mut pending_rtt: Option<(u64, Instant)> = None;
    let mut peer_active = false;
    let mut last_peer_activity = Instant::now();
    let mut last_binary_probe = Instant::now();
    let mut binary_seq = 0_u64;
    let mut pending_binary: Option<(u64, Instant)> = None;
    let mut binary_samples: VecDeque<u32> = VecDeque::new();

    // Fase 3a: make-before-break UDP<->wss fallback (relay-only, additive - inert while the
    // fallback stays off, so the fase-2 UDP path is byte-identical in the normal case).
    // Client side: watch UDP-audio health and request wss fallback if it never arrives.
    let ready_at = Instant::now();
    // Test hook (client only): THETISLINK_TEST_DROP_UDP=<secs> drops inbound UDP audio for
    // the first N seconds after ready, simulating a UDP-blocked network so the fallback +
    // recovery cycle can be exercised deterministically (no OS-firewall fight). 0 = off.
    let test_drop_udp_secs: u64 = if config.role == RelayRole::Client {
        std::env::var("THETISLINK_TEST_DROP_UDP")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    } else {
        0
    };
    if test_drop_udp_secs > 0 {
        log::warn!("TEST HOOK: dropping inbound UDP audio for the first {test_drop_udp_secs}s");
    }
    let mut last_udp_audio: Option<Instant> = None; // last inbound audio frame over UDP
    let mut udp_healthy_since: Option<Instant> = None; // start of the current healthy UDP run
    let mut fallback_requested = false; // client: are we in wss-audio fallback?
    let mut last_fallback_flip = ready_at - FALLBACK_DWELL; // allow an immediate first flip
    let mut last_fallback_heartbeat = ready_at; // client: last RCF lease refresh
    let mut declared_initial_transport = false; // client: sent the connect-time RCF-off yet?
    let mut fallback_flips: u32 = 0; // observability: count transport flips
    let mut dedup = DedupWindow::new(); // drop duplicate audio across both paths
    // Station side: per-client fallback state with dwell + lease (per-session scope + rate-limit).
    let mut station_fallback: HashMap<u8, StationFallback> = HashMap::new();

    // Persistent 250 ms ticker, created ONCE before the loop. A `sleep()` re-created
    // each select iteration gets reset by every audio packet (<250 ms apart) and never
    // elapses under continuous audio - which starved the UDP keepalive + peer-status +
    // probes. An interval keeps its own deadline, so the timer branch fires reliably
    // regardless of audio load. Skip missed ticks (no catch-up burst).
    let mut timer = tokio::time::interval(Duration::from_millis(250));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = config_rx.changed() => return Ok(()),
            _ = shutdown_rx.changed() => return Ok(()),
            maybe_message = read.next() => {
                match maybe_message {
                    Some(Ok(Message::Text(text))) => {
                        // Bounded-TTL token rotation: the relay pushes a fresh UDP capability
                        // token ("udp-token-rotate udp_token=...") before the current
                        // one hits its hard TTL. Swap it in for outgoing TLU1; the old
                        // token stays valid during the relay's short overlap, so no audio
                        // drops. Relay-infrastructure message -> not peer activity.
                        if text.starts_with("udp-token-rotate") {
                            if let Some(t) = parse_udp_token(&text) {
                                udp_token = Some(t);
                                log::debug!("Relay UDP token rotated");
                            }
                            continue;
                        }
                        peer_active = true;
                        last_peer_activity = Instant::now();
                        if let Some(frame) = parse_peer_frame(&text) {
                            match frame.kind.as_str() {
                                "ping" => {
                                    if let Some(seq) = frame.seq {
                                        write
                                            .send(Message::Text(peer_frame_with_seq("pong", config.role, seq).into()))
                                            .await
                                            .map_err(|e| e.to_string())?;
                                    }
                                    set_status(
                                        status,
                                        RelayPhase::Authenticated,
                                        peer_frame_message(&frame),
                                        true,
                                        true,
                                        None,
                                        generation,
                                    );
                                }
                                "pong" => {
                                    if let (Some(seq), Some((pending_seq, sent_at))) = (frame.seq, pending_rtt) {
                                        if seq == pending_seq {
                                            pending_rtt = None;
                                            let rtt_ms = sent_at.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
                                            set_peer_rtt(
                                                status,
                                                peer_frame_message_with_rtt(&frame, rtt_ms),
                                                rtt_ms,
                                                generation,
                                            );
                                        } else {
                                            set_status(
                                                status,
                                                RelayPhase::Authenticated,
                                                peer_frame_message(&frame),
                                                true,
                                                true,
                                                None,
                                                generation,
                                            );
                                        }
                                    } else {
                                        set_status(
                                            status,
                                            RelayPhase::Authenticated,
                                            peer_frame_message(&frame),
                                            true,
                                            true,
                                            None,
                                            generation,
                                        );
                                    }
                                }
                                _ => {
                                    set_status(
                                        status,
                                        RelayPhase::Authenticated,
                                        peer_frame_message(&frame),
                                        true,
                                        true,
                                        None,
                                        generation,
                                    );
                                }
                            }
                        } else {
                            set_status(
                                status,
                                RelayPhase::Authenticated,
                                format!("Relay peer traffic received: {} bytes", text.len()),
                                true,
                                true,
                                None,
                                generation,
                            );
                        }
                    }
                    Some(Ok(Message::Binary(data))) => {
                        peer_active = true;
                        last_peer_activity = Instant::now();
                        // TLT1 = getunnelde TL-frame-data -> lever payload aan het lokale endpoint.
                        if let Some(inbound) = &tunnel_inbound {
                            if let Some((cid, payload)) = parse_tlt1(&data) {
                                // Fase 3a: station intercepts a client's wss-audio fallback
                                // request (per-session scope, idempotent) instead of
                                // forwarding it to the engine.
                                if config.role == RelayRole::Station {
                                    if let Some(on) = parse_rcf_fallback(payload) {
                                        let now = Instant::now();
                                        let e = station_fallback.entry(cid).or_insert(
                                            StationFallback {
                                                on: false,
                                                // Backdated so the first real flip is not
                                                // dwell-blocked by the entry's creation.
                                                last_change: now
                                                    .checked_sub(FALLBACK_DWELL)
                                                    .unwrap_or(now),
                                                last_seen: now,
                                            },
                                        );
                                        e.last_seen = now; // lease refresh (flip or heartbeat)
                                        if e.on != on {
                                            // Dwell on the station too, so a client that
                                            // ignores its own dwell cannot make us flap.
                                            if e.last_change.elapsed() >= FALLBACK_DWELL {
                                                e.on = on;
                                                e.last_change = now;
                                                log::info!(
                                                    "Relay station: client {cid} wss-fallback {}",
                                                    if on { "ON" } else { "OFF" }
                                                );
                                            } else {
                                                log::debug!(
                                                    "Relay station: client {cid} fallback flip within dwell, ignored"
                                                );
                                            }
                                        }
                                        continue;
                                    }
                                }
                                // Fase 3: during path overlap / TX-always-both a frame may
                                // arrive on BOTH wss and UDP; drop the duplicate before the
                                // jitter buffer. Applies to both roles (client RX downlink and
                                // station TX uplink).
                                if dedup.is_duplicate(cid, payload) {
                                    continue;
                                }
                                // Station demultiplext op client_id naar een per-client
                                // sentinel; de client heeft maar een peer (de server) en
                                // gebruikt zijn vaste sentinel-adres.
                                let sentinel = match config.role {
                                    RelayRole::Station => sentinel_for_client_id(cid),
                                    RelayRole::Client => {
                                        tunnel_sentinel.unwrap_or_else(|| sentinel_for_client_id(cid))
                                    }
                                };
                                if bound_clients.insert(cid) {
                                    log::info!("Relay tunnel client bound: id={cid} sentinel={sentinel}");
                                }
                                let _ = inbound.send((sentinel, payload.to_vec()));
                                continue;
                            }
                        }
                        match parse_binary_probe(&data) {
                            Some((BINARY_KIND_PING, seq)) => {
                                // Echo the probe straight back at the same size.
                                write
                                    .send(Message::Binary(
                                        binary_probe_frame(BINARY_KIND_PONG, config.role, seq, data.len()).into(),
                                    ))
                                    .await
                                    .map_err(|e| e.to_string())?;
                            }
                            Some((BINARY_KIND_PONG, seq)) => {
                                if let Some((pending_seq, sent_at)) = pending_binary {
                                    if seq == pending_seq {
                                        pending_binary = None;
                                        let rtt_ms =
                                            sent_at.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
                                        binary_samples.push_back(rtt_ms);
                                        while binary_samples.len() > BINARY_RTT_WINDOW {
                                            binary_samples.pop_front();
                                        }
                                        set_binary_stats(
                                            status,
                                            binary_stats(&binary_samples, rtt_ms),
                                            generation,
                                        );
                                    }
                                }
                            }
                            _ => {
                                set_status(
                                    status,
                                    RelayPhase::Authenticated,
                                    format!("Relay peer traffic received: {} bytes", data.len()),
                                    true,
                                    true,
                                    None,
                                    generation,
                                );
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = write.send(Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) => return Err("relay closed connection".to_string()),
                    Some(Ok(Message::Frame(_))) => {}
                    Some(Err(e)) => return Err(e.to_string()),
                    None => return Err("relay disconnected".to_string()),
                }
            }
            maybe_out = recv_uplink(&mut tunnel_uplink) => {
                match maybe_out {
                    Some((addr, bytes)) => {
                        // Station addresseert de juiste client via het client_id uit de
                        // sentinel; de client stuurt id 0 (de relay stempelt het echte
                        // nummer erin op basis van welke WS-verbinding het frame kwam).
                        let client_id = match config.role {
                            RelayRole::Station => client_id_from_sentinel(addr),
                            RelayRole::Client => 0,
                        };
                        // Fase 3b transport policy (additive; both flags off = fase-2
                        // behaviour, UDP-only for audio + wss for the rest). Per role:
                        //  - Client uplink (TX): UDP unless in fallback; wss when in fallback
                        //    OR always during PTT (TX-always-both, safety-critical path section 14.3).
                        //  - Station downlink (RX): always UDP (doubles as the recovery probe
                        //    during fallback); wss additionally while this client is in fallback.
                        // The receiving end de-dups, so a frame on both paths plays once.
                        let audio = is_audio_packet(&bytes);
                        let ptt = audio && audio_ptt_active(&bytes);
                        let udp_up = udp_socket.is_some() && udp_token.is_some();
                        let (do_udp, do_wss) = if !audio {
                            (false, true) // control/spectrum: wss only
                        } else {
                            match config.role {
                                RelayRole::Client => {
                                    (udp_up && !fallback_requested, fallback_requested || ptt)
                                }
                                RelayRole::Station => {
                                    // wss only while this client's fallback is on AND its
                                    // lease is fresh (a vanished client's flag never leaks).
                                    let fb = station_fallback.get(&client_id).map_or(false, |e| {
                                        e.on && e.last_seen.elapsed() < FALLBACK_LEASE
                                    });
                                    (udp_up, fb)
                                }
                            }
                        };
                        let mut sent_any = false;
                        if do_udp {
                            if let (Some(sock), Some(tok)) = (&udp_socket, &udp_token) {
                                udp_seq = udp_seq.wrapping_add(1);
                                if sock
                                    .send(&encode_tlu1(client_id, udp_seq, tok, &bytes))
                                    .await
                                    .is_ok()
                                {
                                    sent_any = true;
                                }
                            }
                        }
                        // wss when requested, or as the fallback when the UDP send did not go.
                        if do_wss || !sent_any {
                            write
                                .send(Message::Binary(encode_tlt1(client_id, &bytes).into()))
                                .await
                                .map_err(|e| e.to_string())?;
                        }
                    }
                    None => {
                        // Uplink-kanaal gesloten -> tak uitschakelen (geen busy-spin).
                        tunnel_uplink = None;
                    }
                }
            }
            maybe_udp = recv_udp(&udp_socket, &mut udp_buf) => {
                // Fase 2 downlink: relay levert [client_id(1)] + AudioPacket over UDP.
                if let Some(n) = maybe_udp {
                    // Test hook: pretend the network drops inbound UDP for the first N s.
                    let test_drop = test_drop_udp_secs > 0
                        && ready_at.elapsed().as_secs() < test_drop_udp_secs;
                    if !test_drop && n > 1 {
                        peer_active = true;
                        last_peer_activity = Instant::now();
                        let cid = udp_buf[0];
                        let payload = &udp_buf[1..n];
                        // Fase 3: UDP-audio health (client fallback monitor + recovery run).
                        // A gap longer than the stall threshold (re)starts the healthy run,
                        // so recovery only fires after continuous UDP audio (hysteresis).
                        if audio_identity(payload).is_some() {
                            let now = Instant::now();
                            if last_udp_audio.map_or(true, |t| now.duration_since(t) > FALLBACK_STALL) {
                                udp_healthy_since = Some(now);
                            }
                            last_udp_audio = Some(now);
                        }
                        // Fase 3: drop a frame already delivered over wss during overlap.
                        let is_dup = dedup.is_duplicate(cid, payload);
                        if !is_dup {
                            if let Some(inbound) = &tunnel_inbound {
                                let sentinel = match config.role {
                                    RelayRole::Station => sentinel_for_client_id(cid),
                                    RelayRole::Client => {
                                        tunnel_sentinel.unwrap_or_else(|| sentinel_for_client_id(cid))
                                    }
                                };
                                if bound_clients.insert(cid) {
                                    log::info!("Relay UDP client bound: id={cid} sentinel={sentinel}");
                                }
                                let _ = inbound.send((sentinel, payload.to_vec()));
                            }
                        }
                    }
                }
            }
            _ = timer.tick() => {
                if peer_active && last_peer_activity.elapsed() >= PEER_TIMEOUT {
                    peer_active = false;
                    pending_rtt = None;
                    if !bound_clients.is_empty() {
                        bound_clients.clear();
                        log::info!("Relay tunnel clients unbound (peer timeout)");
                    }
                    set_status(
                        status,
                        RelayPhase::WaitingForPeer,
                        "Relay peer timed out; waiting for peer".to_string(),
                        true,
                        false,
                        None,
                        generation,
                    );
                }

                // Fase 3b transport state machine (client, relay-only). Two states:
                //  - UDP (fallback off): request wss if UDP audio never arrived within the
                //    connect window, or an established stream stalled past FALLBACK_STALL.
                //  - Fallback (on): return to UDP-only once UDP audio has flowed healthily for
                //    FALLBACK_RECOVER (> stall = hysteresis). The station keeps sending UDP
                //    during fallback, so recovery is detectable.
                // A minimum dwell between flips (dwell-cap + rate-limit) prevents flapping. The flip
                // request tunnels an RCF over wss (id 0; the relay stamps the real client id).
                if config.role == RelayRole::Client && udp_socket.is_some() && peer_active {
                    let now = Instant::now();
                    // Declare the initial transport (UDP) once the tunnel is up, so a
                    // station carrying a stale fallback flag for a reused client_id clears
                    // it. Sent once; a real fallback follows within the connect window if
                    // UDP never arrives.
                    if !declared_initial_transport {
                        let _ = write
                            .send(Message::Binary(
                                encode_tlt1(0, &encode_rcf_fallback(false)).into(),
                            ))
                            .await;
                        declared_initial_transport = true;
                    }
                    // Transport state machine (flip only after the dwell).
                    if last_fallback_flip.elapsed() >= FALLBACK_DWELL {
                        if !fallback_requested {
                            let stalled = match last_udp_audio {
                                None => ready_at.elapsed() >= FALLBACK_CONNECT_TIMEOUT,
                                Some(t) => t.elapsed() >= FALLBACK_STALL,
                            };
                            if stalled {
                                let _ = write
                                    .send(Message::Binary(
                                        encode_tlt1(0, &encode_rcf_fallback(true)).into(),
                                    ))
                                    .await;
                                fallback_requested = true;
                                last_fallback_flip = now;
                                last_fallback_heartbeat = now;
                                fallback_flips += 1;
                                set_relay_transport(status, true);
                                let gap_ms = last_udp_audio.map(|t| t.elapsed().as_millis());
                                let reason = if last_udp_audio.is_none() { "connect-timeout" } else { "udp-stall" };
                                log::info!(
                                    "Relay fallback ON (reason={reason} gap_ms={gap_ms:?} flips={fallback_flips}) - requesting wss audio"
                                );
                            }
                        } else if udp_healthy_since.map_or(false, |t| t.elapsed() >= FALLBACK_RECOVER) {
                            let _ = write
                                .send(Message::Binary(
                                    encode_tlt1(0, &encode_rcf_fallback(false)).into(),
                                ))
                                .await;
                            fallback_requested = false;
                            last_fallback_flip = now;
                            fallback_flips += 1;
                            set_relay_transport(status, false);
                            log::info!(
                                "Relay fallback OFF (reason=udp-recovered flips={fallback_flips}) - back to UDP"
                            );
                        }
                    }
                    // Lease refresh while in fallback, so the station keeps the flag alive
                    // and can expire it if we vanish.
                    if fallback_requested && last_fallback_heartbeat.elapsed() >= FALLBACK_HEARTBEAT {
                        let _ = write
                            .send(Message::Binary(
                                encode_tlt1(0, &encode_rcf_fallback(true)).into(),
                            ))
                            .await;
                        last_fallback_heartbeat = now;
                    }
                }

                // Station: expire per-client fallback state whose lease lapsed (a client
                // that vanished without an off), so it never leaks to a reused client_id.
                if config.role == RelayRole::Station && !station_fallback.is_empty() {
                    station_fallback.retain(|_, e| e.last_seen.elapsed() < FALLBACK_LEASE);
                }

                if last_status.elapsed() >= PEER_STATUS_INTERVAL {
                    write
                        .send(Message::Text(peer_frame("status", config.role).into()))
                        .await
                        .map_err(|e| e.to_string())?;
                    last_status = Instant::now();
                }

                if last_rtt_ping.elapsed() >= PEER_RTT_INTERVAL {
                    rtt_seq = rtt_seq.wrapping_add(1);
                    let now = Instant::now();
                    write
                        .send(Message::Text(peer_frame_with_seq("ping", config.role, rtt_seq).into()))
                        .await
                        .map_err(|e| e.to_string())?;
                    pending_rtt = Some((rtt_seq, now));
                    last_rtt_ping = now;
                }

                if last_binary_probe.elapsed() >= BINARY_PROBE_INTERVAL {
                    binary_seq = binary_seq.wrapping_add(1);
                    let now = Instant::now();
                    write
                        .send(Message::Binary(
                            binary_probe_frame(BINARY_KIND_PING, config.role, binary_seq, BINARY_PROBE_LEN)
                                .into(),
                        ))
                        .await
                        .map_err(|e| e.to_string())?;
                    pending_binary = Some((binary_seq, now));
                    last_binary_probe = now;
                }

                if last_ping.elapsed() >= PEER_WAIT_PING {
                    let _ = write.send(Message::Ping(Vec::new().into())).await;
                    last_ping = Instant::now();
                }

                // Keep our UDP source-binding + NAT mapping alive so downlink audio
                // keeps reaching us even during long RX-only / idle periods.
                if last_udp_keepalive.elapsed() >= Duration::from_secs(1) {
                    if let (Some(sock), Some(tok)) = (&udp_socket, &udp_token) {
                        udp_seq = udp_seq.wrapping_add(1);
                        let _ = sock.send(&encode_tlu1(0, udp_seq, tok, &[])).await;
                    }
                    last_udp_keepalive = Instant::now();
                }
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PeerFrame {
    kind: String,
    role: String,
    seq: Option<u64>,
}

fn peer_frame(kind: &str, role: RelayRole) -> String {
    format!("TLR2 kind={kind} role={}", role.as_str())
}

fn peer_frame_with_seq(kind: &str, role: RelayRole, seq: u64) -> String {
    format!("TLR2 kind={kind} role={} seq={seq}", role.as_str())
}

fn parse_peer_frame(text: &str) -> Option<PeerFrame> {
    let mut parts = text.split_whitespace();
    if parts.next()? != "TLR2" {
        return None;
    }

    let mut kind = None;
    let mut role = None;
    let mut seq = None;
    for part in parts {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key {
            "kind" => kind = Some(value.to_string()),
            "role" => role = Some(value.to_string()),
            "seq" => seq = value.parse::<u64>().ok(),
            _ => {}
        }
    }

    Some(PeerFrame {
        kind: kind.unwrap_or_else(|| "status".to_string()),
        role: role.unwrap_or_else(|| "peer".to_string()),
        seq,
    })
}

fn peer_frame_message(frame: &PeerFrame) -> String {
    match frame.kind.as_str() {
        "hello" => format!("Relay peer online: {} handshake received", frame.role),
        "status" => format!("Relay peer online: {} heartbeat received", frame.role),
        "ping" => format!("Relay peer online: {} RTT probe received", frame.role),
        "pong" => format!("Relay peer online: {} RTT reply received", frame.role),
        other => format!("Relay peer online: {} sent {other}", frame.role),
    }
}

fn peer_frame_message_with_rtt(frame: &PeerFrame, rtt_ms: u32) -> String {
    format!("Relay peer online: {} RTT {} ms", frame.role, rtt_ms)
}

/// Build a binary probe frame: magic (4) + kind (1) + role (1) + seq (8 LE),
/// padded with zeros to `len` (an audio-sized payload). Header is 14 bytes.
fn binary_probe_frame(kind: u8, role: RelayRole, seq: u64, len: usize) -> Vec<u8> {
    let total = len.max(14);
    let mut buf = Vec::with_capacity(total);
    buf.extend_from_slice(&BINARY_MAGIC);
    buf.push(kind);
    buf.push(match role {
        RelayRole::Station => 0,
        RelayRole::Client => 1,
    });
    buf.extend_from_slice(&seq.to_le_bytes());
    buf.resize(total, 0);
    buf
}

/// Parse a binary probe frame, returning (kind, seq) if the magic matches.
fn parse_binary_probe(data: &[u8]) -> Option<(u8, u64)> {
    if data.len() < 14 || data[0..4] != BINARY_MAGIC {
        return None;
    }
    let kind = data[4];
    let seq = u64::from_le_bytes(data[6..14].try_into().ok()?);
    Some((kind, seq))
}

/// Verpak TL-frame-bytes in een TLT1-tunnelframe (payload wordt geclamped op u16).
fn encode_tlt1(client_id: u8, payload: &[u8]) -> Vec<u8> {
    let len = payload.len().min(u16::MAX as usize);
    let mut buf = Vec::with_capacity(TLT1_HEADER_LEN + len);
    buf.extend_from_slice(&TLT1_MAGIC);
    buf.push(TLT1_VERSION);
    buf.push(client_id);
    buf.extend_from_slice(&(len as u16).to_le_bytes());
    buf.extend_from_slice(&payload[..len]);
    buf
}

/// Parse een TLT1-tunnelframe -> (client_id, payload) als de magic klopt en het
/// frame compleet is. Andere magics (bv. TLB1) geven None (non-fataal).
fn parse_tlt1(data: &[u8]) -> Option<(u8, &[u8])> {
    if data.len() < TLT1_HEADER_LEN || data[0..4] != TLT1_MAGIC {
        return None;
    }
    let client_id = data[5];
    let len = u16::from_le_bytes([data[6], data[7]]) as usize;
    let end = TLT1_HEADER_LEN + len;
    if data.len() < end {
        return None;
    }
    Some((client_id, &data[TLT1_HEADER_LEN..end]))
}

/// Compute rolling min/avg/max over the recent binary-probe RTT samples.
fn binary_stats(samples: &VecDeque<u32>, last_ms: u32) -> BinaryProbeStats {
    let n = samples.len().max(1) as u32;
    let min_ms = samples.iter().copied().min().unwrap_or(last_ms);
    let max_ms = samples.iter().copied().max().unwrap_or(last_ms);
    let sum: u64 = samples.iter().map(|&v| u64::from(v)).sum();
    let avg_ms = (sum / u64::from(n)) as u32;
    BinaryProbeStats {
        last_ms,
        min_ms,
        max_ms,
        avg_ms,
        samples: n,
    }
}

/// Record binary-probe stats. Keeps the existing status message (which tracks
/// the text heartbeat) and only updates the binary stats; logs once on the first
/// successful round-trip so the log is not spammed every probe.
fn set_binary_stats(handle: &RelayStatusHandle, stats: BinaryProbeStats, generation: u64) {
    let mut status = handle.state.lock().unwrap_or_else(|e| e.into_inner());
    let first = status.binary.is_none();
    status.phase = RelayPhase::Authenticated;
    status.connected = true;
    status.peer_seen = true;
    status.last_error = None;
    status.binary = Some(stats);
    status.generation = generation;
    if first {
        log::info!("Relay binary probe OK: RTT {} ms", stats.last_ms);
    }
}

async fn wait_for_change(
    config_rx: &mut watch::Receiver<RelayConfig>,
    shutdown_rx: &mut watch::Receiver<bool>,
) {
    tokio::select! {
        _ = config_rx.changed() => {}
        _ = shutdown_rx.changed() => {}
    }
}

fn set_status(
    handle: &RelayStatusHandle,
    phase: RelayPhase,
    message: String,
    connected: bool,
    peer_seen: bool,
    last_error: Option<String>,
    generation: u64,
) {
    let mut status = handle.state.lock().unwrap_or_else(|e| e.into_inner());
    let next_rtt = if connected && peer_seen {
        status.peer_rtt_ms
    } else {
        None
    };
    let next_binary = if connected && peer_seen {
        status.binary
    } else {
        None
    };
    let changed = status.phase != phase
        || status.message != message
        || status.connected != connected
        || status.peer_seen != peer_seen
        || status.last_error != last_error
        || status.peer_rtt_ms != next_rtt
        || status.binary != next_binary;
    let log_changed = status.phase != phase
        || status.connected != connected
        || status.peer_seen != peer_seen
        || status.last_error != last_error;
    status.phase = phase;
    status.message = message;
    status.connected = connected;
    status.peer_seen = peer_seen;
    status.last_error = last_error;
    status.peer_rtt_ms = next_rtt;
    status.binary = next_binary;
    status.generation = generation;
    if changed && log_changed {
        log::info!("Relay status: {}", status.message);
    }
}

/// Fase 3c: update the transport-fallback flag shown by the client indicator. Mutates only
/// that field, so it survives the next set_status refresh.
fn set_relay_transport(handle: &RelayStatusHandle, fallback: bool) {
    handle
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .transport_fallback = fallback;
}

fn set_peer_rtt(
    handle: &RelayStatusHandle,
    message: String,
    rtt_ms: u32,
    generation: u64,
) {
    let mut status = handle.state.lock().unwrap_or_else(|e| e.into_inner());
    let log_changed = status.phase != RelayPhase::Authenticated
        || !status.connected
        || !status.peer_seen
        || status.last_error.is_some();
    status.phase = RelayPhase::Authenticated;
    status.message = message;
    status.connected = true;
    status.peer_seen = true;
    status.last_error = None;
    status.peer_rtt_ms = Some(rtt_ms);
    status.generation = generation;
    if log_changed {
        log::info!("Relay status: {}", status.message);
    }
}

/// Turn the relay's `ERR ...` hello rejection into a clear reason for the UI, so the
/// user sees *why* a connection was refused (device/connection/data limits, block,
/// bad key) instead of a raw code. Unknown codes fall back to the raw text.
fn reject_reason(err: &str) -> String {
    let reason = match err.trim() {
        "ERR relay full" => "too many simultaneous connections for this station",
        "ERR too many devices" => "too many devices for this station",
        "ERR station data limit reached" => "this station's monthly data limit has been reached",
        "ERR device blocked" => "this device has been blocked by the administrator",
        "ERR device pending approval" => "this device is waiting for administrator approval",
        "ERR unauthorized" => "invalid key or no access to this station",
        "ERR device check failed" => "could not verify this device, please try again",
        other => return format!("relay rejected connection: {other}"),
    };
    format!("Relay rejected: {reason}")
}

fn short_error(err: &str) -> String {
    const MAX: usize = 180;
    let mut chars = err.chars();
    let mut out: String = chars.by_ref().take(MAX).collect();
    if chars.next().is_some() {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_udp_token_extracts_last_field() {
        let tok = "a".repeat(64);
        assert_eq!(
            parse_udp_token(&format!("OK relay-ready udp_token={tok}")).as_deref(),
            Some(tok.as_str())
        );
        assert!(parse_udp_token("OK relay-ready").is_none()); // no token
        assert!(parse_udp_token("OK relay-ready udp_token=short").is_none()); // wrong length
    }

    #[test]
    fn rcf_fallback_roundtrip_and_reject() {
        assert_eq!(parse_rcf_fallback(&encode_rcf_fallback(true)), Some(true));
        assert_eq!(parse_rcf_fallback(&encode_rcf_fallback(false)), Some(false));
        // An audio packet must never be mistaken for a control frame.
        assert_eq!(parse_rcf_fallback(&[0xEE, 1, 0x01, 0, 0, 0, 0, 5]), None);
        assert_eq!(parse_rcf_fallback(b"RCF"), None); // too short
    }

    #[test]
    fn audio_identity_reads_type_and_be_sequence() {
        // header(4) + sequence(4 BE) + ...: type=0x01, seq=0x00010203.
        let pkt = [0xEE, 1, 0x01, 0, 0x00, 0x01, 0x02, 0x03, 9, 9];
        assert_eq!(audio_identity(&pkt), Some((0x01, 0x0001_0203)));
        assert_eq!(audio_identity(&[0, 0, 0x04, 0, 0, 0, 0, 0]), None); // not audio
        assert_eq!(audio_identity(&[0, 0, 0x01, 0]), None); // too short for seq
    }

    #[test]
    fn dedup_drops_repeat_keeps_fresh_and_non_audio() {
        let mut d = DedupWindow::new();
        let frame = |ty: u8, seq: u32| {
            let mut v = vec![0xEE, 1, ty, 0];
            v.extend_from_slice(&seq.to_be_bytes());
            v
        };
        assert!(!d.is_duplicate(0, &frame(0x01, 1))); // fresh
        assert!(d.is_duplicate(0, &frame(0x01, 1))); // exact repeat -> dropped
        assert!(!d.is_duplicate(0, &frame(0x01, 2))); // next seq -> fresh
        assert!(!d.is_duplicate(0, &frame(0x1B, 1))); // same seq, other type -> fresh
        assert!(!d.is_duplicate(1, &frame(0x01, 1))); // same frame, other client -> fresh
        // Non-audio is never deduped (control/spectrum must always pass).
        assert!(!d.is_duplicate(0, &[0, 0, 0x04, 0]));
        assert!(!d.is_duplicate(0, &[0, 0, 0x04, 0]));
    }

    #[test]
    fn audio_ptt_flag_detected() {
        assert!(audio_ptt_active(&[0xEE, 1, 0x01, 0x01, 0, 0, 0, 0])); // PTT bit set
        assert!(!audio_ptt_active(&[0xEE, 1, 0x01, 0x00, 0, 0, 0, 0])); // PTT clear
        assert!(!audio_ptt_active(&[0, 0, 0x04, 0x01])); // not an audio packet
    }

    #[test]
    fn is_audio_packet_only_audio_types() {
        assert!(is_audio_packet(&[0, 0, 0x01, 0])); // Audio
        assert!(is_audio_packet(&[0, 0, 0x1B, 0])); // AudioMultiCh
        assert!(is_audio_packet(&[0, 0, 0x21, 0])); // AudioVrx
        assert!(!is_audio_packet(&[0, 0, 0x0A, 0])); // Spectrum -> wss
        assert!(!is_audio_packet(&[0, 0, 0x04, 0])); // Control -> wss
        assert!(!is_audio_packet(&[0, 0, 0x01])); // too short -> not audio
    }

    #[test]
    fn encode_tlu1_layout_and_hex32() {
        let tok = "bb".repeat(32); // 64 hex chars -> 32 bytes of 0xbb
        let frame = encode_tlu1(3, 0x0102_0304, &tok, b"xy");
        assert_eq!(&frame[0..4], b"TLU1");
        assert_eq!(frame[4], TLU1_VERSION);
        assert_eq!(frame[6], 3); // client_id
        assert_eq!(&frame[7..11], &0x0102_0304u32.to_le_bytes());
        assert_eq!(&frame[11..43], &[0xbbu8; 32]); // token raw bytes
        assert_eq!(&frame[43..], b"xy"); // payload after the 43-byte header
        assert_eq!(frame.len(), TLU1_HEADER_LEN + 2);
    }

    #[test]
    fn url_host_parses_scheme_port_path() {
        assert_eq!(
            url_host("wss://relay.example.com").as_deref(),
            Some("relay.example.com")
        );
        assert_eq!(url_host("wss://host:443/path").as_deref(), Some("host"));
        assert_eq!(url_host("ws://1.2.3.4:18080").as_deref(), Some("1.2.3.4"));
    }

    #[test]
    fn reject_reason_maps_known_codes_and_passes_through() {
        assert!(reject_reason("ERR too many devices").contains("too many devices"));
        assert!(reject_reason("ERR relay full").contains("simultaneous connections"));
        assert!(reject_reason("ERR station data limit reached").contains("monthly data limit"));
        assert!(reject_reason("ERR device blocked").contains("blocked"));
        assert!(reject_reason("ERR unauthorized").contains("invalid key"));
        // Unknown code falls back to the raw text.
        assert!(reject_reason("ERR something new").contains("something new"));
    }

    #[test]
    fn tlt1_roundtrip_preserves_payload() {
        let payload = b"\xaa\x00\x01 the quick brown TL frame \xff\x00";
        let frame = encode_tlt1(0, payload);
        let (client_id, out) = parse_tlt1(&frame).expect("moet parsen");
        assert_eq!(client_id, 0);
        assert_eq!(out, payload);
    }

    #[test]
    fn tlt1_is_distinct_from_tlb1_probe() {
        // Een TLB1-probe mag NIET als TLT1 parsen, en andersom.
        let probe = binary_probe_frame(BINARY_KIND_PING, RelayRole::Station, 7, BINARY_PROBE_LEN);
        assert!(parse_tlt1(&probe).is_none(), "TLB1-probe parseerde als TLT1");

        let tunnel = encode_tlt1(0, b"hello");
        assert!(
            parse_binary_probe(&tunnel).is_none(),
            "TLT1-tunnel parseerde als TLB1-probe"
        );
    }

    #[test]
    fn tlt1_rejects_truncated_and_wrong_magic() {
        // Onbekende magic -> None (non-fataal drop).
        assert!(parse_tlt1(b"XXXX\x01\x00\x00\x00").is_none());
        // Te kort voor de header -> None.
        assert!(parse_tlt1(b"TLT1").is_none());
        // Header claimt meer payload dan aanwezig -> None.
        let mut bad = encode_tlt1(0, b"abcd");
        bad.truncate(TLT1_HEADER_LEN + 2); // header zegt 4, maar 2 aanwezig
        assert!(parse_tlt1(&bad).is_none());
    }

    #[test]
    fn sentinel_client_id_roundtrip() {
        for id in [0u8, 1, 2, 15, 100, 253] {
            let s = sentinel_for_client_id(id);
            assert_eq!(client_id_from_sentinel(s), id, "roundtrip failed for id {id}");
        }
        // client 0 -> 203.0.113.1 (matches server tracked_socket::relay_sentinel_for(0)).
        assert_eq!(sentinel_for_client_id(0).ip().to_string(), "203.0.113.1");
        assert_eq!(sentinel_for_client_id(1).ip().to_string(), "203.0.113.2");
    }

    #[test]
    fn tlt1_empty_payload_roundtrips() {
        let frame = encode_tlt1(3, b"");
        let (id, out) = parse_tlt1(&frame).expect("leeg payload moet parsen");
        assert_eq!(id, 3);
        assert!(out.is_empty());
    }
}
