// SPDX-License-Identifier: GPL-2.0-or-later

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use log::{debug, info, warn};
use std::collections::HashMap;
use std::env;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, Mutex};
use tokio::time::{interval, timeout, Duration};
use tokio_tungstenite::tungstenite::handshake::server::{Request as HsRequest, Response as HsResponse};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{accept_hdr_async, connect_async};

mod chat_ticket;

/// A one-time id for a ticket, so the same one cannot post twice.
fn new_jti() -> String {
    use rand::Rng;
    let bytes: [u8; 12] = rand::thread_rng().gen();
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}
mod admin_api;
mod store;

const DEFAULT_LISTEN: &str = "0.0.0.0:18080";
const DEFAULT_DB: &str = "stations.db";
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Relay-side keepalive: ping each peer this often and reap it if no frame/pong
/// arrives within PEER_DEAD_TIMEOUT. Reaping frees the peer's client_id slot so a
/// half-open (abruptly dropped) connection cannot occupy a room forever.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
const PEER_DEAD_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Role {
    Station,
    Client,
}

impl Role {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "station" | "server" => Ok(Self::Station),
            "client" => Ok(Self::Client),
            _ => bail!("role must be station or client"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Station => "station",
            Self::Client => "client",
        }
    }
}

#[derive(Debug)]
enum Mode {
    SelfTest,
    Serve {
        listen: String,
    },
    Connect {
        url: String,
        station: String,
        role: Role,
        token: String,
        send_once: Option<String>,
        instance: Option<String>,
        name: Option<String>,
    },
    Station(StationCmd),
    SetAdminPassword { db: String, password: String },
}

/// Admin CLI subcommands for the Fase 1 station registry (SQLite).
#[derive(Debug)]
enum StationCmd {
    Add { db: String, label: String, owner: String },
    List { db: String },
    SetEnabled { db: String, id: i64, enabled: bool },
    Remove { db: String, id: i64 },
}

#[derive(Debug)]
struct Hello {
    station: String,
    role: Role,
    token: String,
    /// Stable per-install id (optional). When present, a reconnecting client with the
    /// same id reclaims its existing slot instead of allocating a new one.
    instance: Option<String>,
    /// Human-readable device label (optional, may contain spaces). Metadata only -
    /// never used for auth/routing; purely for the log/dashboard.
    name: Option<String>,
}

/// Upper bound on simultaneous clients per station room. The sentinel address
/// scheme (203.0.113.1..=254) allows up to 254, but a small cap keeps per-station
/// load low so a single VPS can host more distinct stations, and matches realistic
/// use (a handful of devices per station).
const MAX_CLIENTS: usize = 8;

#[derive(Clone)]
struct Peer {
    tx: mpsc::UnboundedSender<Message>,
    /// The client's per-install id, if it sent one (used for replace-on-reconnect).
    instance: Option<String>,
    /// Lock-free running total of relayed bytes attributed to this peer (uplink for a
    /// client's own frames, plus downlink for frames the station unicasts to it).
    /// Incremented per frame with a relaxed atomic add (no lock, no DB); flushed once
    /// to `devices.bytes_total` on disconnect (hot-path write guard: never a per-frame write).
    bytes: Arc<AtomicU64>,
}

/// Outcome of registering a peer in a room.
enum Registration {
    /// Station slot taken (any previous station was evicted).
    Station,
    /// Client admitted with this assigned client_id.
    Client(u8),
    /// Room already has MAX_CLIENTS clients; the peer was rejected.
    Full,
}

/// A station room: exactly one station peer and up to MAX_CLIENTS client peers,
/// each identified by an assigned `client_id` used to demultiplex TLT1 tunnel
/// frames. `next_id` is a rotating cursor so a freed id is not immediately
/// reused (avoids a new client inheriting a just-departed client's session).
#[derive(Default)]
struct Room {
    station: Option<Peer>,
    clients: HashMap<u8, Peer>,
    next_id: u8,
}

impl Room {
    fn is_empty(&self) -> bool {
        self.station.is_none() && self.clients.is_empty()
    }

    /// Allocate the next free client_id starting from the rotating cursor, or `None`
    /// if the room is at `cap` clients. `cap` is the per-station limit, never above the
    /// sentinel id space (MAX_CLIENTS); ids still range over the full space.
    fn alloc_client_id(&mut self, cap: usize) -> Option<u8> {
        let cap = cap.min(MAX_CLIENTS);
        if self.clients.len() >= cap {
            return None;
        }
        let span = MAX_CLIENTS as u16;
        for offset in 0..span {
            let id = ((u16::from(self.next_id) + offset) % span) as u8;
            if !self.clients.contains_key(&id) {
                self.next_id = ((u16::from(id) + 1) % span) as u8;
                return Some(id);
            }
        }
        None
    }
}

pub(crate) type Rooms = Arc<Mutex<HashMap<String, Room>>>;

/// Serial for the next wss connection. Only ever compared for equality, so
/// wrapping after 2^64 connections is not a concern anyone needs to have.
static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

/// Fase 0 (PATCH-relay-audio-udp): a live UDP audio session, keyed by its capability
/// token. Issued on the wss handshake (S1), bound to the peer's identity (S2), valid
/// only while the wss session lives (S3 lease) and until its TTL (S4). The UDP data
/// path (fase 1) validates against this table before forwarding; `src` is learned from
/// the first valid datagram (source-binding S5).
#[allow(dead_code)] // station_id is metadata for logging/future use
struct UdpSession {
    room_key: String,
    station_id: Option<i64>,
    client_id: Option<u8>,
    role: Role,
    /// Which wss connection issued this token.
    ///
    /// Revoking used to match on (room, role, client_id), and a client id is
    /// deliberately reused: a returning client reclaims its own slot. On a
    /// network change both connections are briefly alive, the new one takes the
    /// id, and when the old socket finally times out its cleanup revoked the
    /// tokens of the connection that had just taken over - so a phone that had
    /// switched networks kept its control channel and lost its audio, with
    /// nothing in any log saying why. A serial per connection cannot be reused
    /// (2026-08-17).
    conn: u64,
    expires_at: Instant,
    /// Learned from the first valid datagram; later datagrams must match (S5).
    src: Option<SocketAddr>,
    /// Replay high-water: highest accepted sequence (S6).
    last_seq: Option<u32>,
    /// Shared with the wss `Peer.bytes` counter for this device, so UDP audio bytes
    /// accumulate into the same per-device total the wss path uses. Drained by the
    /// existing periodic/disconnect flush (swap(0)) - no per-packet DB write (hot-path
    /// write guard). Uplink counts against the sender client, downlink against the target.
    bytes: Arc<AtomicU64>,
}

/// Drop every UDP token issued by one wss connection, and say how many are left.
///
/// By connection, not by client id. A client id is reused on purpose - a
/// returning client reclaims its own slot - so matching on it let a dying
/// connection revoke the tokens of the one that had just taken its place.
fn revoke_tokens_of(toks: &mut HashMap<String, UdpSession>, conn: u64) -> usize {
    toks.retain(|_, s| s.conn != conn);
    toks.len()
}

// --- Fase 1 (PATCH-relay-audio-udp): relay UDP data path ---

/// TLU1 outer header: magic(4) | version(1) | flags(1) | client_id(1) | seq(4 LE) |
/// token(32 raw bytes) | payload. The token is the 256-bit capability from fase 0
/// (looked up hex-encoded); `client_id` is the TARGET client when the sender is a
/// station, ignored otherwise. `seq` drives the replay window.
const TLU1_MAGIC: &[u8; 4] = b"TLU1";
const TLU1_VERSION: u8 = 1;
const TLU1_TOKEN_LEN: usize = 32;
const TLU1_HEADER_LEN: usize = 4 + 1 + 1 + 1 + 4 + TLU1_TOKEN_LEN; // = 43
/// Reorder tolerance for the replay guard: datagrams more than this many sequences
/// behind the high-water are dropped as stale/replay (S6).
const UDP_REPLAY_WINDOW: u32 = 512;

/// True if u32 sequence `a` is before `b` (wrapping compare).
fn seq_before(a: u32, b: u32) -> bool {
    a.wrapping_sub(b) > 0x8000_0000
}

/// Replay check (S6): accept a first packet, reject an exact duplicate of the
/// high-water, accept anything newer, and accept a behind-packet only within the
/// reorder window.
fn seq_ok(last: Option<u32>, seq: u32) -> bool {
    match last {
        None => true,
        Some(last) if seq == last => false,     // exact duplicate = replay
        Some(last) if seq_before(last, seq) => true, // newer
        Some(last) => last.wrapping_sub(seq) <= UDP_REPLAY_WINDOW, // recent reorder
    }
}

/// Parse a TLU1 datagram: `(target_client_id, seq, token_hex, payload)` or `None` on a
/// short/wrong-magic/wrong-version datagram - the cheap first stage of the drop path (S7).
fn parse_tlu1(data: &[u8]) -> Option<(u8, u32, String, &[u8])> {
    if data.len() < TLU1_HEADER_LEN || &data[0..4] != TLU1_MAGIC || data[4] != TLU1_VERSION {
        return None;
    }
    let client_id = data[6];
    let seq = u32::from_le_bytes([data[7], data[8], data[9], data[10]]);
    let token: String = data[11..11 + TLU1_TOKEN_LEN]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Some((client_id, seq, token, &data[TLU1_HEADER_LEN..]))
}

/// Relay UDP data path (fase 1): validate each datagram against the token registry and
/// forward it **statelessly, per-datagram** (R1) to the peer's learned UDP address.
/// Invalid datagrams are dropped cheaply with no reply (S7, S9).
async fn serve_udp(socket: UdpSocket, tokens: UdpTokens) {
    let mut buf = vec![0u8; 2048];
    loop {
        let (len, src) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some((target_client_id, seq, token, payload)) = parse_tlu1(&buf[..len]) else {
            continue; // cheap reject
        };
        // Validate + resolve the forward target under the registry lock; send outside it.
        let forward: Option<(SocketAddr, Vec<u8>)> = {
            let mut toks = tokens.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            // 1. token lookup + TTL + source-binding (S5) + replay (S6) - all cheap.
            let (room, sender_role, sender_cid) = match toks.get(&token) {
                Some(s)
                    if s.expires_at > now
                        && s.src.map_or(true, |bound| bound == src)
                        && seq_ok(s.last_seq, seq) =>
                {
                    (s.room_key.clone(), s.role, s.client_id)
                }
                _ => continue, // unknown / expired / wrong source / replay -> drop
            };
            // 2. learn source (first packet) and advance the replay high-water. The token
            //    keeps its HARD TTL (S4) - it is deliberately NOT slid forward by traffic.
            //    Instead the wss side rotates in a fresh token before this one expires
            //    (see the udp_rotate branch), so an active session always holds a valid
            //    capability without any token ever outliving its bounded TTL.
            if let Some(s) = toks.get_mut(&token) {
                if s.src.is_none() {
                    s.src = Some(src);
                }
                if s.last_seq.map_or(true, |last| seq_before(last, seq)) {
                    s.last_seq = Some(seq);
                }
            }
            // 3. resolve the destination's learned UDP address + the client_id to stamp
            //    on the downlink so the receiver can demux (never the token - S10/S11).
            //    Also grab the byte counter to charge: uplink -> the sender client,
            //    downlink -> the target client (never the station), matching the wss path.
            let (dst, cid, counter) = match sender_role {
                // client -> the room's station; stamp + charge the SENDER client.
                Role::Client => (
                    toks.values()
                        .find(|s| s.room_key == room && s.role == Role::Station)
                        .and_then(|s| s.src),
                    sender_cid.unwrap_or(0),
                    toks.get(&token).map(|s| s.bytes.clone()),
                ),
                // station -> the addressed client; stamp + charge the TARGET client.
                Role::Station => {
                    let target = toks.values().find(|s| {
                        s.room_key == room
                            && s.role == Role::Client
                            && s.client_id == Some(target_client_id)
                    });
                    (
                        target.and_then(|s| s.src),
                        target_client_id,
                        target.map(|s| s.bytes.clone()),
                    )
                }
            };
            dst.map(|dst| {
                // Charge the whole datagram (incl. TLU1 overhead) to the client device,
                // once per forwarded packet - the same "one frame, one count" the wss path
                // uses. Only when we actually forward, so drops cost nothing.
                if let Some(c) = &counter {
                    c.fetch_add(len as u64, Ordering::Relaxed);
                }
                // Downlink framing: [client_id(1)] + AudioPacket. No token forwarded.
                let mut framed = Vec::with_capacity(1 + payload.len());
                framed.push(cid);
                framed.extend_from_slice(payload);
                (dst, framed)
            })
        };
        if let Some((dst, framed)) = forward {
            let _ = socket.send_to(&framed, dst).await;
        }
    }
}

/// Token -> session. std Mutex: touched only at wss connect/disconnect (fase 0) and,
/// later, per-datagram validation on the UDP path - never held across an `.await`.
pub(crate) type UdpTokens = Arc<std::sync::Mutex<HashMap<String, UdpSession>>>;

/// Hard TTL for a UDP capability token (S4): the maximum a token can ever be valid,
/// counted from issuance and never extended by traffic. The wss rotation below keeps an
/// active session supplied with fresh tokens well before this ceiling is reached.
const UDP_TOKEN_TTL: Duration = Duration::from_secs(30 * 60);
/// Rotate the UDP token this often over wss (S4) - comfortably under UDP_TOKEN_TTL so the
/// replacement is delivered long before the current token expires.
const UDP_TOKEN_ROTATE: Duration = Duration::from_secs(10 * 60);
/// After a rotation the previous token is force-expired to this short overlap, so an
/// in-flight switch never drops a packet yet a superseded token dies quickly (S4).
const UDP_TOKEN_OVERLAP: Duration = Duration::from_secs(60);

/// 256-bit UDP capability token (S1: >=128 bit, OS-CSPRNG), hex. A temporary capability
/// - never a station/admin secret, never logged in full (S11).
fn gen_udp_token() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Optional shared station registry. `None` = legacy global-token auth only.
/// std Mutex (not tokio's): the lookup is synchronous and never held across `.await`.
type StationStore = Option<Arc<std::sync::Mutex<store::Store>>>;

/// Result of authorizing a hello against the registry / legacy token.
enum AuthOutcome {
    /// Authenticated via the registry; carries the stable station row id (room key).
    Registry(i64),
    /// No active registry - use the legacy global-token result and name+token room.
    Legacy,
    /// Rejected: unknown/disabled station or wrong token.
    Reject,
}

/// Open the station registry if one is present. Migration-safe: with no DB file
/// (and no `THETISLINK_RELAY_DB`), returns `None` so the relay behaves exactly as
/// before (legacy global-token auth; no new file created).
fn open_store() -> StationStore {
    let path = env::var("THETISLINK_RELAY_DB").unwrap_or_else(|_| DEFAULT_DB.to_string());
    if !std::path::Path::new(&path).exists() {
        return None;
    }
    match store::Store::open(&path) {
        Ok(s) => {
            info!("station registry loaded from {path}");
            Some(Arc::new(std::sync::Mutex::new(s)))
        }
        Err(err) => {
            warn!("failed to open station registry {path}: {err:#}; using legacy token auth");
            None
        }
    }
}

/// Decide how to admit a hello. Registry mode applies only when a store exists and
/// holds at least one station - an empty registry falls back to legacy so that
/// creating the DB before adding any station does not lock everyone out. The
/// presented secret is looked up by hash; the plaintext is never logged.
fn authorize(store: &StationStore, hello: &Hello, required_token: &Option<String>) -> AuthOutcome {
    if let Some(store) = store {
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        if guard.count().unwrap_or(0) > 0 {
            return match guard.authenticate(&hello.token) {
                Ok(Some(id)) => AuthOutcome::Registry(id),
                _ => AuthOutcome::Reject,
            };
        }
    }
    if let Some(required) = required_token {
        if &hello.token != required {
            return AuthOutcome::Reject;
        }
    }
    AuthOutcome::Legacy
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    match parse_args(env::args().skip(1).collect())? {
        Mode::SelfTest => run_self_test().await,
        Mode::Serve { listen } => serve(&listen).await,
        Mode::Connect {
            url,
            station,
            role,
            token,
            send_once,
            instance,
            name,
        } => connect_client(&url, &station, role, &token, send_once, instance, name).await,
        Mode::Station(cmd) => run_station_cmd(cmd),
        Mode::SetAdminPassword { db, password } => run_set_admin_password(db, password),
    }
}

/// Set the dashboard admin password (Argon2id). Bootstrap step (security baseline: no public
/// "create admin" endpoint). Run once, e.g. inside the container.
fn run_set_admin_password(db: String, password: String) -> Result<()> {
    if password.trim().len() < 8 {
        anyhow::bail!("choose a password of at least 8 characters");
    }
    let store = store::Store::open(&db)?;
    let hash = store::hash_password(&password)?;
    store.set_admin_password_hash(&hash, unix_now())?;
    println!("Admin password set (db {db}). The dashboard login is now enabled.");
    Ok(())
}

fn unix_now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Run a station-registry admin command against the SQLite store. Synchronous
/// (rusqlite); does not touch the live relay auth path (Fase 1 foundation).
fn run_station_cmd(cmd: StationCmd) -> Result<()> {
    match cmd {
        StationCmd::Add { db, label, owner } => {
            let store = store::Store::open(&db)?;
            let secret = store::generate_secret();
            let id = store.add(&label, &owner, &secret, unix_now())?;
            println!("Station added: id={id} label={label}");
            println!("Secret (store it now - it is not shown again, only its hash is kept):");
            println!("  {secret}");
        }
        StationCmd::List { db } => {
            let store = store::Store::open(&db)?;
            let rows = store.list()?;
            if rows.is_empty() {
                println!("No stations registered in {db}.");
            }
            for r in rows {
                println!(
                    "id={} label={} owner={} enabled={} created_at={}",
                    r.id, r.label, r.owner, r.enabled, r.created_at
                );
            }
        }
        StationCmd::SetEnabled { db, id, enabled } => {
            let store = store::Store::open(&db)?;
            if store.set_enabled(id, enabled)? {
                println!("Station {id} {}.", if enabled { "enabled" } else { "disabled" });
            } else {
                println!("No station with id {id}.");
            }
        }
        StationCmd::Remove { db, id } => {
            let store = store::Store::open(&db)?;
            if store.remove(id)? {
                println!("Station {id} removed.");
            } else {
                println!("No station with id {id}.");
            }
        }
    }
    Ok(())
}

async fn serve(listen: &str) -> Result<()> {
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding relay listener on {listen}"))?;
    let required_token = env::var("THETISLINK_RELAY_TOKEN").ok();

    info!("ThetisLink relay listening on {listen}");
    let store = open_store();
    if store.is_some() {
        info!("station registry auth enabled (empty registry falls back to token)");
    } else if required_token.is_some() {
        info!("relay token check is enabled");
    } else {
        warn!("relay token check is disabled; use only for local tests");
    }

    // Shared room map: owned here so the admin API can reach live peers (to kick a
    // device the instant it is blocked), not only the accept loop.
    let rooms = Rooms::default();

    // Fase 0 (PATCH-relay-audio-udp): UDP capability-token registry, issued on the wss
    // handshake and revoked on disconnect. Shared with the UDP data path (fase 1).
    let udp_tokens = UdpTokens::default();

    // Fase 1: relay UDP data path. Binds only when THETISLINK_RELAY_UDP_LISTEN is set
    // (e.g. "0.0.0.0:443"), so the change is fully additive - no UDP, no behaviour change.
    if let Ok(udp_listen) = env::var("THETISLINK_RELAY_UDP_LISTEN") {
        match UdpSocket::bind(&udp_listen).await {
            Ok(sock) => {
                info!("relay UDP data path listening on {udp_listen}");
                let udp_toks = Arc::clone(&udp_tokens);
                tokio::spawn(serve_udp(sock, udp_toks));
            }
            Err(e) => warn!("could not bind UDP {udp_listen}: {e:#}"),
        }
    }

    // Fase 2 admin dashboard API: only when a registry (store) exists. Bound to an
    // internal address (default 0.0.0.0:18081, reached only via Caddy over the compose
    // network - not published to the host/internet). SECURITY: 0.0.0.0 is safe ONLY in
    // that compose topology (the port is not host-published and X-Forwarded-For is set by
    // Caddy). A standalone / non-compose deployment MUST set THETISLINK_RELAY_ADMIN_LISTEN
    // to 127.0.0.1:18081 (or keep the API strictly behind a trusted reverse proxy),
    // otherwise the admin API is reachable directly and the XFF client IP is spoofable.
    if let Some(s) = &store {
        let admin_listen = env::var("THETISLINK_RELAY_ADMIN_LISTEN")
            .unwrap_or_else(|_| "0.0.0.0:18081".to_string());
        let admin_store = s.clone();
        let admin_rooms = Arc::clone(&rooms);
        tokio::spawn(async move {
            if let Err(e) = admin_api::serve(admin_store, admin_rooms, &admin_listen).await {
                warn!("admin API stopped: {e:#}");
            }
        });

        // Mid-session usage flush: periodically write active sessions' bytes to the DB
        // so the dashboard is near-live and the monthly cap sees long sessions. Interval
        // overridable via THETISLINK_RELAY_FLUSH_SECS (default 60).
        let flush_secs = env::var("THETISLINK_RELAY_FLUSH_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(USAGE_FLUSH_INTERVAL.as_secs());
        let flush_rooms = Arc::clone(&rooms);
        let flush_store = s.clone();
        tokio::spawn(async move {
            let mut tick = interval(Duration::from_secs(flush_secs));
            tick.tick().await; // discard the immediate first tick
            loop {
                tick.tick().await;
                flush_active(&flush_rooms, &flush_store).await;
            }
        });
    }

    accept_loop(listener, required_token, store, rooms, udp_tokens).await
}

async fn accept_loop(
    listener: TcpListener,
    required_token: Option<String>,
    store: StationStore,
    rooms: Rooms,
    udp_tokens: UdpTokens,
) -> Result<()> {
    loop {
        let (stream, addr) = listener.accept().await?;
        let rooms = Arc::clone(&rooms);
        let required_token = required_token.clone();
        let store = store.clone();
        let udp_tokens = Arc::clone(&udp_tokens);
        tokio::spawn(async move {
            if let Err(err) =
                handle_connection(stream, addr, rooms, required_token, store, udp_tokens).await
            {
                warn!("connection {addr}: {err:#}");
            }
        });
    }
}

async fn handle_connection(
    stream: TcpStream,
    addr: SocketAddr,
    rooms: Rooms,
    required_token: Option<String>,
    store: StationStore,
    udp_tokens: UdpTokens,
) -> Result<()> {
    // Disable Nagle so forwarded frames are not batched: this relay reads from one
    // peer and writes to the other, and Nagle on that write path adds latency/jitter.
    let _ = stream.set_nodelay(true);
    // Capture the real client IP from Caddy's X-Forwarded-For on the upgrade request
    // (the TCP peer is Caddy's container address, not the client). Falls back to the
    // socket peer when the header is absent (direct connection / local test).
    let forwarded: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    let fwd = forwarded.clone();
    let ws = accept_hdr_async(stream, move |req: &HsRequest, resp: HsResponse| {
        if let Some(ip) = forwarded_ip(req.headers()) {
            *fwd.lock().unwrap_or_else(|e| e.into_inner()) = Some(ip);
        }
        Ok(resp)
    })
    .await
    .with_context(|| format!("websocket handshake from {addr}"))?;
    let client_ip = forwarded
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| addr.ip().to_string());
    let (mut ws_tx, mut ws_rx) = ws.split();

    let hello = read_hello(&mut ws_rx).await?;
    // `station_id` is Some only under registry auth; it keys device enrollment and
    // byte accounting (legacy token mode has no station row, so no device tracking).
    let (room_key, station_id) = match authorize(&store, &hello, &required_token) {
        AuthOutcome::Reject => {
            let _ = ws_tx.send(Message::Text("ERR unauthorized".into())).await;
            bail!(
                "unauthorized station={} role={}",
                hello.station,
                hello.role.as_str()
            );
        }
        // Registry auth: room keyed by the stable station id, so a shared name with
        // a different secret is a different room. The name stays a display label.
        AuthOutcome::Registry(id) => (format!("db:{id}"), Some(id)),
        AuthOutcome::Legacy => (room_key(&hello.station, &hello.token), None),
    };

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
    let role = hello.role;
    let station = hello.station.clone();
    // Short, stable per-install label for the logs so a device can be tracked even
    // as its slot id rotates (all clients often share one public IP). Prefix 'a' =
    // Android, 'd' = desktop. '-' = a client that sent no install id.
    let inst_label: String = hello
        .instance
        .as_deref()
        .map(|s| s.chars().take(12).collect())
        .unwrap_or_else(|| "-".to_string());
    // Human device label for the log (already sanitized in parse_hello); omitted
    // entirely when the client sent none.
    let name_label = match &hello.name {
        Some(n) => format!(" name=\"{n}\""),
        None => String::new(),
    };

    // Per-connection byte counter: shared with this peer's Room entry so the station's
    // downlink path can bump it too. Flushed to the DB once on disconnect (below).
    let bytes = Arc::new(AtomicU64::new(0));

    // Device enrollment + admission (registry mode, clients with a stable install id
    // only). A station is not a "device"; a legacy/no-id client is not gated. There is no
    // per-device approval gate: access rests on the station secret, capped by max_devices,
    // with the `enabled` flag as the manual blocklist. The device is upserted first so it
    // shows in the dashboard, then admission is refused if it is blocked or beyond the
    // caps. Enforced BEFORE register_peer so a refused device never occupies a room slot.
    // Runs once per connect (a lifecycle edge, never per frame). `ws_tx` is still owned
    // here, so a rejection can reply on it directly (the writer below takes ownership).
    // Per-station concurrent-client cap (defaults to the global room cap; overridden
    // below by the station's max_clients setting). Applied when a slot is allocated.
    let mut client_cap = MAX_CLIENTS;
    let device_id: Option<i64> = match (role, &store, station_id, hello.instance.as_deref()) {
        (Role::Client, Some(store), Some(st_id), Some(inst)) => {
            let platform = platform_of(inst);
            let now = unix_now();
            let ym = year_month(now);
            // Phase 1 - gather facts (no insert yet). Never hold the std Mutex across
            // the awaits below.
            let (existing, max_dev, max_cli, max_monthly, used, enabled_count) = {
                let guard = store.lock().unwrap_or_else(|e| e.into_inner());
                let existing = guard.device_enabled(st_id, inst).unwrap_or(None);
                let (md, mc, mm) = guard.station_limits(st_id).unwrap_or((None, None, None));
                let used = guard.station_month_bytes(st_id, &ym).unwrap_or(0);
                // Only needed for a brand-new device (the max_devices cap).
                let cnt = if existing.is_none() {
                    guard.count_enabled_devices(st_id).unwrap_or(0)
                } else {
                    0
                };
                (existing, md, mc, mm, used, cnt)
            };
            // Phase 2 - gate. Access rests on the station secret plus the limits, with
            // `enabled` as the manual block. A blocked device is refused; a genuinely new
            // device beyond max_devices is refused.
            if existing == Some(false) {
                let _ = ws_tx.send(Message::Text("ERR device blocked".into())).await;
                bail!("device blocked station={st_id} inst={inst_label}");
            }
            if existing.is_none() {
                if let Some(md) = max_dev {
                    if enabled_count >= md {
                        let _ = ws_tx.send(Message::Text("ERR too many devices".into())).await;
                        bail!("device cap reached station={st_id} count={enabled_count} cap={md}");
                    }
                }
            }
            // Monthly data cap (soft): refuse NEW connections once the station is at/over
            // its cap; sessions already running are left to finish.
            if let Some(cap) = max_monthly {
                if used >= cap {
                    let _ = ws_tx
                        .send(Message::Text("ERR station data limit reached".into()))
                        .await;
                    bail!("monthly data cap reached station={st_id} used={used} cap={cap}");
                }
            }
            // Per-station concurrent cap (on top of the global room cap).
            if let Some(mc) = max_cli {
                if mc > 0 {
                    client_cap = (mc as usize).min(MAX_CLIENTS);
                }
            }
            // Phase 3 - admit: upsert presence (insert new / touch existing), take id.
            let enrolled = {
                let guard = store.lock().unwrap_or_else(|e| e.into_inner());
                guard.enroll_seen(st_id, inst, hello.name.as_deref(), platform, Some(&client_ip), now)
            };
            match enrolled {
                Ok((id, _enabled)) => Some(id),
                Err(err) => {
                    let _ = ws_tx.send(Message::Text("ERR device check failed".into())).await;
                    bail!("device enroll failed station={st_id} inst={inst_label}: {err:#}");
                }
            }
        }
        _ => None,
    };

    // Register before spawning the writer so a full-room rejection can reply on
    // `ws_tx` directly (the writer takes ownership of `ws_tx`).
    let client_id = match register_peer(
        &rooms,
        &room_key,
        role,
        out_tx.clone(),
        hello.instance.clone(),
        bytes.clone(),
        client_cap,
    )
    .await
    {
        Registration::Full => {
            let _ = ws_tx.send(Message::Text("ERR relay full".into())).await;
            bail!("relay room full for station={station}");
        }
        Registration::Station => None,
        Registration::Client(id) => Some(id),
    };

    // The device passed the gate and now holds a slot: count one admitted session.
    if let (Some(store), Some(dev_id)) = (&store, device_id) {
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        let _ = guard.bump_session(dev_id, unix_now());
    }

    let writer = tokio::spawn(async move {
        while let Some(message) = out_rx.recv().await {
            if ws_tx.send(message).await.is_err() {
                break;
            }
        }
    });

    // This connection's serial, stamped on every token it issues so that only
    // this connection can revoke them (see UdpSession::conn).
    let conn = NEXT_CONN.fetch_add(1, Ordering::Relaxed);

    // Fase 0: issue a UDP capability token bound to this session (S1-S4) and hand it to
    // the peer in the ready-reply (over the TLS wss channel). Revoked on disconnect
    // below (S3 lease). The token is never logged in full (S11).
    let udp_token = gen_udp_token();
    let udp_session_count = {
        let now = Instant::now();
        let mut toks = udp_tokens.lock().unwrap_or_else(|e| e.into_inner());
        toks.retain(|_, s| s.expires_at > now); // prune expired (cheap housekeeping)
        toks.insert(
            udp_token.clone(),
            UdpSession {
                room_key: room_key.clone(),
                station_id,
                client_id,
                role,
                conn,
                expires_at: now + UDP_TOKEN_TTL,
                src: None,
                last_seq: None,
                bytes: bytes.clone(),
            },
        );
        toks.len()
    };

    // The chat ticket rides along on the ready-reply (design §3). Additive by
    // construction: an older client looks for `udp_token=` by name and never
    // sees this, so it keeps working exactly as before. Absent when no key is
    // configured, which is simply a relay with no chat behind it.
    let chat_ticket = chat_ticket::signing_key_from_env().and_then(|key| {
        station_id.map(|sid| {
            chat_ticket::issue(&key, sid, &station, &new_jti(), unix_now().max(0) as u64)
        })
    });
    let chat_field = match &chat_ticket {
        Some(t) => format!(" chat_ticket={t}"),
        None => String::new(),
    };
    let _ = out_tx.send(Message::Text(
        format!("OK relay-ready udp_token={udp_token}{chat_field}").into(),
    ));
    info!(
        "{addr} registered station={station} role={} id={client_id:?} ip={client_ip} inst={inst_label}{name_label} (clients now {}, udp sessions {udp_session_count})",
        role.as_str(),
        room_client_count(&rooms, &room_key).await
    );

    // Read loop with keepalive. IMPORTANT: EVERY exit path must fall through to
    // unregister_peer below - otherwise the peer's client_id slot leaks and the room
    // fills up with ghosts (room-full). A WS *error* (abrupt disconnect) therefore
    // breaks instead of propagating with `?`, so cleanup still runs. A peer that goes
    // silent (half-open TCP) is reaped after PEER_DEAD_TIMEOUT.
    let mut last_seen = Instant::now();
    let mut keepalive = interval(KEEPALIVE_INTERVAL);
    keepalive.tick().await; // discard the immediate first tick
    // S4 UDP-token rotation: the token this session currently advertises, and the timer
    // that periodically mints its successor. interval (not sleep) so rotation fires on
    // its own deadline regardless of message load.
    let mut current_udp_token = udp_token.clone();
    let mut udp_rotate = interval(UDP_TOKEN_ROTATE);
    // Comfortably inside the ticket's own lifetime, so a client is never left
    // holding an expired one between rotations.
    let mut chat_rotate = tokio::time::interval(
        Duration::from_secs(chat_ticket::TICKET_TTL_SECS / 2),
    );
    udp_rotate.tick().await; // discard the immediate first tick
    loop {
        tokio::select! {
            maybe = ws_rx.next() => {
                let message = match maybe {
                    Some(Ok(m)) => m,
                    Some(Err(_)) | None => break,
                };
                last_seen = Instant::now();
                match message {
                    Message::Text(text) => {
                        route_message(&rooms, &room_key, role, client_id, &bytes, Message::Text(text)).await
                    }
                    Message::Binary(data) => {
                        route_message(&rooms, &room_key, role, client_id, &bytes, Message::Binary(data)).await
                    }
                    Message::Ping(data) => {
                        let _ = out_tx.send(Message::Pong(data));
                    }
                    Message::Pong(_) => {}
                    Message::Close(_) => break,
                    Message::Frame(_) => {}
                }
            }
            _ = keepalive.tick() => {
                if last_seen.elapsed() > PEER_DEAD_TIMEOUT {
                    warn!(
                        "{addr} keepalive timeout station={station} role={} id={client_id:?}",
                        role.as_str()
                    );
                    break;
                }
                // Prod the peer; a live one answers with Pong (or keeps sending frames).
                let _ = out_tx.send(Message::Ping(Vec::new()));
            }
            _ = chat_rotate.tick() => {
                // A ticket lasts a quarter of an hour (design §3.2), so a fresh one
                // goes out well before that. Same shape as the UDP rotation above,
                // and additive in the same way: a client that does not know this
                // line ignores it, exactly as it ignores any text it was not
                // written for.
                if let (Some(key), Some(sid)) =
                    (chat_ticket::signing_key_from_env(), station_id)
                {
                    let t = chat_ticket::issue(
                        &key, sid, &station, &new_jti(), unix_now().max(0) as u64,
                    );
                    let _ = out_tx.send(Message::Text(
                        format!("chat-ticket-rotate chat_ticket={t}").into(),
                    ));
                }
            }
            _ = udp_rotate.tick() => {
                // S4: mint a fresh UDP token and hand it over wss BEFORE the current one
                // reaches its hard TTL, so an active session never loses a valid
                // capability - and no token outlives UDP_TOKEN_TTL. The superseded token
                // is force-expired to a short overlap so an in-flight switch is seamless
                // while a captured token stops working soon after rotation.
                let now = Instant::now();
                let new_token = gen_udp_token();
                let active_tokens = {
                    let mut toks = udp_tokens.lock().unwrap_or_else(|e| e.into_inner());
                    toks.retain(|_, s| s.expires_at > now); // cheap housekeeping
                    if let Some(old) = toks.get_mut(&current_udp_token) {
                        let overlap = now + UDP_TOKEN_OVERLAP;
                        if old.expires_at > overlap {
                            old.expires_at = overlap;
                        }
                    }
                    toks.insert(
                        new_token.clone(),
                        UdpSession {
                            room_key: room_key.clone(),
                            station_id,
                            client_id,
                            role,
                            conn,
                            expires_at: now + UDP_TOKEN_TTL,
                            src: None,
                            last_seq: None,
                            bytes: bytes.clone(),
                        },
                    );
                    toks.len()
                };
                let _ = out_tx.send(Message::Text(
                    format!("udp-token-rotate udp_token={new_token}").into(),
                ));
                current_udp_token = new_token;
                // Observability: token-free rotation trace so a future drop-around-rotation
                // is diagnosable (never log the token itself, S11). active_tokens doubles as
                // a live token gauge across all sessions.
                info!(
                    "udp token rotated station={station} role={} id={client_id:?} (udp sessions {active_tokens})",
                    role.as_str()
                );
            }
        }
    }

    unregister_peer(&rooms, &room_key, role, client_id, &out_tx).await;
    drop(out_tx);
    writer.abort();

    // Flush this session's relayed-byte total to the device record - a single DB
    // write at the disconnect edge (hot-path write guard: never per frame). Only when the
    // client was enrolled (registry mode + stable id) and actually moved traffic.
    if let (Some(store), Some(dev_id)) = (&store, device_id) {
        // swap(0): take only what the periodic flush has not already written, so the
        // two flush paths never double-count the same bytes.
        let total = bytes.swap(0, Ordering::Relaxed);
        if total > 0 {
            let ym = year_month(unix_now());
            let guard = store.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(err) = guard.add_device_bytes(dev_id, total as i64) {
                warn!("byte flush failed for device={dev_id}: {err:#}");
            }
            // Roll the session into the station's monthly bucket too (data cap +
            // analytics). station_id is Some whenever device_id is (both registry mode).
            if let Some(st_id) = station_id {
                if let Err(err) = guard.add_station_month_bytes(st_id, &ym, total as i64) {
                    warn!("monthly usage flush failed for station={st_id}: {err:#}");
                }
            }
        }
    }

    // Fase 0: revoke this session's UDP token(s) - they are valid only while the wss
    // session lives (S3 lease). Once gone, the UDP data path (fase 1) drops any datagram
    // bearing them. Rotation (S4) can leave more than one live token for this connection
    // (the current one plus a just-superseded one still inside its overlap), so drop ALL
    // tokens matching this exact connection (room + role + client_id), not just the
    // initially-issued value.
    let udp_session_count = {
        let mut toks = udp_tokens.lock().unwrap_or_else(|e| e.into_inner());
        revoke_tokens_of(&mut toks, conn)
    };
    info!(
        "{addr} disconnected station={station} role={} id={client_id:?} inst={inst_label}{name_label} (clients now {}, udp sessions {udp_session_count})",
        role.as_str(),
        room_client_count(&rooms, &room_key).await
    );
    Ok(())
}

/// Current number of client peers in a room (0 if the room no longer exists).
/// Drives the occupancy figure in the connect/disconnect logs and the dashboard's
/// "connected now" figure (via the admin API).
pub(crate) async fn room_client_count(rooms: &Rooms, key: &str) -> usize {
    rooms
        .lock()
        .await
        .get(key)
        .map(|room| room.clients.len())
        .unwrap_or(0)
}

async fn read_hello<S>(ws_rx: &mut S) -> Result<Hello>
where
    S: StreamExt<Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    let message = timeout(HELLO_TIMEOUT, ws_rx.next())
        .await
        .context("waiting for relay hello")?
        .ok_or_else(|| anyhow!("client disconnected before hello"))??;

    match message {
        Message::Text(text) => parse_hello(&text),
        _ => bail!("first websocket message must be a text hello"),
    }
}

async fn register_peer(
    rooms: &Rooms,
    key: &str,
    role: Role,
    tx: mpsc::UnboundedSender<Message>,
    instance: Option<String>,
    bytes: Arc<AtomicU64>,
    client_cap: usize,
) -> Registration {
    let mut rooms = rooms.lock().await;
    let room = rooms.entry(key.to_string()).or_default();
    match role {
        Role::Station => {
            if let Some(previous) = room.station.replace(Peer { tx, instance, bytes }) {
                warn!("replacing existing station peer in room");
                let _ = previous.tx.send(Message::Close(None));
            }
            Registration::Station
        }
        Role::Client => {
            // Replace-on-reconnect: a returning client with the same per-install id
            // reclaims its own slot instead of consuming a new one, so one device
            // relaunching/backgrounding cannot pile up ghost slots until the room is
            // full. The old connection is closed; its later unregister is a no-op
            // because the slot now holds a different sender (same_channel guard).
            if let Some(inst) = instance.as_deref() {
                let existing = room
                    .clients
                    .iter()
                    .find(|(_, peer)| peer.instance.as_deref() == Some(inst))
                    .map(|(id, peer)| (*id, peer.tx.clone()));
                if let Some((old_id, old_tx)) = existing {
                    warn!("replacing existing client (same instance) id={old_id}");
                    let _ = old_tx.send(Message::Close(None));
                    room.clients.insert(old_id, Peer { tx, instance, bytes });
                    return Registration::Client(old_id);
                }
            }
            match room.alloc_client_id(client_cap) {
                Some(id) => {
                    room.clients.insert(id, Peer { tx, instance, bytes });
                    Registration::Client(id)
                }
                None => Registration::Full,
            }
        }
    }
}

async fn unregister_peer(
    rooms: &Rooms,
    key: &str,
    role: Role,
    client_id: Option<u8>,
    tx: &mpsc::UnboundedSender<Message>,
) {
    let mut rooms = rooms.lock().await;
    let remove_room = if let Some(room) = rooms.get_mut(key) {
        match role {
            Role::Station => {
                if room.station.as_ref().is_some_and(|peer| peer.tx.same_channel(tx)) {
                    room.station = None;
                }
            }
            Role::Client => {
                if let Some(id) = client_id {
                    if room.clients.get(&id).is_some_and(|peer| peer.tx.same_channel(tx)) {
                        room.clients.remove(&id);
                    }
                }
            }
        }
        room.is_empty()
    } else {
        false
    };

    if remove_room {
        rooms.remove(key);
    }
}

/// Close any live client peers for a device (matched by station id + install id), so
/// blocking a device in the dashboard takes effect immediately rather
/// than only on its next reconnect. The peer's slot is freed when its read loop sees
/// the close and falls through to `unregister_peer`. Returns how many peers were
/// closed. Called by the admin API.
/// The install-ids with a live peer in a station's room - the station itself
/// and its clients alike. Feeds the dashboard's per-device "connected" mark:
/// `last_seen` freezes at session start, so a server that stays connected for
/// days reads as "1 connection, last seen 56 hours ago" - true, and exactly
/// the kind of true that looks like a fault to whoever administers it.
pub(crate) async fn live_install_ids(
    rooms: &Rooms,
    station_id: i64,
) -> std::collections::HashSet<String> {
    let key = format!("db:{station_id}");
    let rooms = rooms.lock().await;
    let Some(room) = rooms.get(&key) else {
        return Default::default();
    };
    let mut out = std::collections::HashSet::new();
    if let Some(station) = &room.station {
        if let Some(i) = &station.instance {
            out.insert(i.clone());
        }
    }
    for peer in room.clients.values() {
        if let Some(i) = &peer.instance {
            out.insert(i.clone());
        }
    }
    out
}

pub(crate) async fn kick_device(rooms: &Rooms, station_id: i64, install_id: &str) -> usize {
    let key = format!("db:{station_id}");
    let rooms = rooms.lock().await;
    let Some(room) = rooms.get(&key) else {
        return 0;
    };
    let mut closed = 0;
    for peer in room.clients.values() {
        if peer.instance.as_deref() == Some(install_id) {
            let _ = peer.tx.send(Message::Close(None));
            closed += 1;
        }
    }
    closed
}

/// How often the mid-session usage flush runs. Ongoing sessions otherwise only land in
/// the DB on disconnect; a per-minute flush makes the dashboard near-live and lets the
/// monthly cap see long-running sessions - still never a per-frame write (hot-path write guard).
const USAGE_FLUSH_INTERVAL: Duration = Duration::from_secs(60);

/// Flush every active registry client's accumulated bytes to the DB and reset its
/// counter (atomic swap, so the disconnect flush cannot double-count). Only rooms keyed
/// "db:{id}" carry a station/device to attribute to. The rooms lock is held only to
/// snapshot the deltas; the DB writes happen after it is released.
async fn flush_active(rooms: &Rooms, store: &Arc<std::sync::Mutex<store::Store>>) {
    let ym = year_month(unix_now());
    let mut writes: Vec<(i64, String, i64)> = Vec::new(); // (station_id, install_id, delta)
    {
        let rooms = rooms.lock().await;
        for (key, room) in rooms.iter() {
            let Some(sid) = key.strip_prefix("db:").and_then(|s| s.parse::<i64>().ok()) else {
                continue;
            };
            for peer in room.clients.values() {
                if let Some(inst) = &peer.instance {
                    let delta = peer.bytes.swap(0, Ordering::Relaxed);
                    if delta > 0 {
                        writes.push((sid, inst.clone(), delta as i64));
                    }
                }
            }
        }
    }
    if writes.is_empty() {
        return;
    }
    let guard = store.lock().unwrap_or_else(|e| e.into_inner());
    for (sid, inst, delta) in writes {
        let _ = guard.add_device_bytes_by_install(sid, &inst, delta);
        let _ = guard.add_station_month_bytes(sid, &ym, delta);
    }
}

/// Route a frame through the room. TLT1 tunnel frames are addressed per client via
/// their `client_id` byte; every other frame (text status/heartbeat, TLB1 probes)
/// fans out station->all-clients and funnels client->station.
async fn route_message(
    rooms: &Rooms,
    key: &str,
    role: Role,
    client_id: Option<u8>,
    self_bytes: &Arc<AtomicU64>,
    message: Message,
) {
    match role {
        Role::Client => {
            // Uplink counts against this client's own counter (lock-free, no DB).
            self_bytes.fetch_add(msg_len(&message), Ordering::Relaxed);
            // client -> station: stamp the tunnel frame with this client's assigned
            // id so the station can demultiplex it into the right session.
            let message = stamp_if_tlt1(message, client_id.unwrap_or(0));
            let station = {
                let rooms = rooms.lock().await;
                rooms
                    .get(key)
                    .and_then(|room| room.station.as_ref().map(|peer| peer.tx.clone()))
            };
            match station {
                Some(tx) => {
                    if tx.send(message).is_err() {
                        debug!("station peer already disconnected");
                    }
                }
                None => debug!("dropping client frame: no station in room"),
            }
        }
        Role::Station => match &message {
            // TLT1 tunnel frame -> unicast to the addressed client.
            Message::Binary(data) if is_tlt1(data) => {
                let target_id = data[5];
                let len = msg_len(&message);
                // Grab the target's sender AND byte counter in one lookup so the
                // downlink is attributed to the receiving client's device record.
                let target = {
                    let rooms = rooms.lock().await;
                    rooms.get(key).and_then(|room| {
                        room.clients
                            .get(&target_id)
                            .map(|peer| (peer.tx.clone(), peer.bytes.clone()))
                    })
                };
                match target {
                    Some((tx, dst_bytes)) => {
                        dst_bytes.fetch_add(len, Ordering::Relaxed);
                        if tx.send(message).is_err() {
                            debug!("client {target_id} already disconnected");
                        }
                    }
                    None => debug!("dropping station frame for client {target_id}: not connected"),
                }
            }
            // Status/heartbeat/probe -> broadcast to every client in the room.
            _ => {
                let targets: Vec<mpsc::UnboundedSender<Message>> = {
                    let rooms = rooms.lock().await;
                    rooms
                        .get(key)
                        .map(|room| room.clients.values().map(|peer| peer.tx.clone()).collect())
                        .unwrap_or_default()
                };
                for tx in targets {
                    let _ = tx.send(message.clone());
                }
            }
        },
    }
}

/// UTC "YYYY-MM" for a unix timestamp - buckets a session's bytes into the right
/// month. Pure date math (Howard Hinnant's civil_from_days; no chrono dependency),
/// correct across leap years. A new month is simply a new bucket, so caps reset with
/// no scheduled job. Also used by the admin API for the "this month" figure.
pub(crate) fn year_month(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // year of era [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year [0, 365]
    let mp = (5 * doy + 2) / 153; // month, shifted [0, 11]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}")
}

/// Byte length of a data frame for accounting (control frames count as 0). Counts
/// the whole payload, i.e. the actual bytes the relay forwarded.
fn msg_len(message: &Message) -> u64 {
    match message {
        Message::Text(text) => text.len() as u64,
        Message::Binary(data) => data.len() as u64,
        _ => 0,
    }
}

/// First IP in an `X-Forwarded-For` header - the original client, since Caddy (the
/// sole trusted proxy in front of the relay) appends the peer on the right. Trimmed;
/// `None` if the header is absent or empty.
fn forwarded_ip(headers: &tokio_tungstenite::tungstenite::http::HeaderMap) -> Option<String> {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Map an install-id prefix to a platform label for the device record. The client
/// prefixes its stable id with 'a' (Android) or 'd' (desktop); anything else is
/// recorded as unknown rather than rejected (forward-compat).
fn platform_of(instance: &str) -> &'static str {
    match instance.chars().next() {
        Some('a') => "android",
        Some('d') => "desktop",
        _ => "unknown",
    }
}

/// True if `data` is a well-formed TLT1 tunnel frame header (magic + room for the
/// 8-byte header, so `data[5]` (client_id) is safe to read).
fn is_tlt1(data: &[u8]) -> bool {
    data.len() >= 8 && data[0..4] == *b"TLT1"
}

/// Rewrite the client_id byte of a TLT1 frame to `id`; pass other frames through
/// unchanged. Version-agnostic across tungstenite payload types (`to_vec`).
fn stamp_if_tlt1(message: Message, id: u8) -> Message {
    match message {
        Message::Binary(data) if is_tlt1(&data) => {
            let mut data = data.to_vec();
            data[5] = id;
            Message::Binary(data.into())
        }
        other => other,
    }
}

async fn run_self_test() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(accept_loop(
        listener,
        Some("secret".to_string()),
        None,
        Rooms::default(),
        UdpTokens::default(),
    ));
    let url = format!("ws://{addr}");

    let (station_ws, _) = connect_async(&url).await?;
    let (mut station_tx, mut station_rx) = station_ws.split();
    station_tx
        .send(Message::Text(
            "TLR1 station=selftest role=station token=secret".into(),
        ))
        .await?;
    expect_ok_ready(&mut station_rx).await?;

    // Two clients join the SAME room; the second must NOT evict the first
    // (multi-client). Allocation starts at 0, so A gets id 0 and B gets id 1.
    let (client_a_ws, _) = connect_async(&url).await?;
    let (mut client_a_tx, mut client_a_rx) = client_a_ws.split();
    client_a_tx
        .send(Message::Text(
            "TLR1 station=selftest role=client token=secret".into(),
        ))
        .await?;
    expect_ok_ready(&mut client_a_rx).await?;

    let (client_b_ws, _) = connect_async(&url).await?;
    let (mut client_b_tx, mut client_b_rx) = client_b_ws.split();
    client_b_tx
        .send(Message::Text(
            "TLR1 station=selftest role=client token=secret".into(),
        ))
        .await?;
    expect_ok_ready(&mut client_b_rx).await?;

    // Text from the station fans out to BOTH clients.
    station_tx
        .send(Message::Text("from-station".into()))
        .await?;
    expect_text(&mut client_a_rx, "from-station").await?;
    expect_text(&mut client_b_rx, "from-station").await?;

    // Text from a client funnels to the station.
    client_a_tx.send(Message::Text("from-client".into())).await?;
    expect_text(&mut station_rx, "from-client").await?;

    // TLT1 magic must stay distinct from the TLB1 probe magic.
    if &tlt1_frame(0, b"x")[0..4] == b"TLB1" {
        bail!("TLT1 magic collides with TLB1 probe magic");
    }

    // Per-client addressing: a station frame for id 1 reaches ONLY client B...
    station_tx
        .send(Message::Binary(tlt1_frame(1, b"to-B-only").into()))
        .await?;
    expect_binary(&mut client_b_rx, &tlt1_frame(1, b"to-B-only")).await?;
    // ...and a following frame for id 0 reaches client A unchanged. If A had also
    // received the id-1 frame (cross-talk), that stray frame would surface here first.
    station_tx
        .send(Message::Binary(tlt1_frame(0, b"to-A-final").into()))
        .await?;
    expect_binary(&mut client_a_rx, &tlt1_frame(0, b"to-A-final")).await?;

    // Client->station frames are STAMPED with the sender's assigned id: both clients
    // send with placeholder id 0, the station must see 0 (A) and 1 (B).
    client_a_tx
        .send(Message::Binary(tlt1_frame(0, b"up-from-A").into()))
        .await?;
    expect_binary(&mut station_rx, &tlt1_frame(0, b"up-from-A")).await?;
    client_b_tx
        .send(Message::Binary(tlt1_frame(0, b"up-from-B").into()))
        .await?;
    expect_binary(&mut station_rx, &tlt1_frame(1, b"up-from-B")).await?;

    let _ = station_tx.close().await;
    let _ = client_a_tx.close().await;
    let _ = client_b_tx.close().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    server.abort();
    println!("self-test OK (multi-client)");
    Ok(())
}

/// Build a TLT1 tunnel frame: magic(4) | version(1) | client_id(1) | len(2 LE) | payload.
fn tlt1_frame(client_id: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(8 + payload.len());
    f.extend_from_slice(b"TLT1");
    f.push(1); // version
    f.push(client_id);
    f.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    f.extend_from_slice(payload);
    f
}

async fn expect_text<S>(rx: &mut S, expected: &str) -> Result<()>
where
    S: StreamExt<Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    let message = timeout(Duration::from_secs(2), rx.next())
        .await
        .context("waiting for self-test relay message")?
        .ok_or_else(|| anyhow!("self-test peer disconnected"))??;

    match message {
        Message::Text(text) if text == expected => Ok(()),
        other => bail!(
            "expected text {expected:?}, got {}",
            printable_message(other)
        ),
    }
}

/// Like `expect_text` but matches the ready-reply prefix, since it now carries a
/// per-session `udp_token=...` suffix (fase 0).
async fn expect_ok_ready<S>(rx: &mut S) -> Result<()>
where
    S: StreamExt<Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    let message = timeout(Duration::from_secs(2), rx.next())
        .await
        .context("waiting for self-test ready reply")?
        .ok_or_else(|| anyhow!("self-test peer disconnected"))??;
    match message {
        Message::Text(text) if text.starts_with("OK relay-ready") => Ok(()),
        other => bail!("expected OK relay-ready, got {}", printable_message(other)),
    }
}

async fn expect_binary<S>(rx: &mut S, expected: &[u8]) -> Result<()>
where
    S: StreamExt<Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    let message = timeout(Duration::from_secs(2), rx.next())
        .await
        .context("waiting for self-test binary message")?
        .ok_or_else(|| anyhow!("self-test peer disconnected"))??;

    match message {
        Message::Binary(data) if data.as_slice() == expected => Ok(()),
        other => bail!(
            "expected {} binary bytes, got {}",
            expected.len(),
            printable_message(other)
        ),
    }
}
async fn connect_client(
    url: &str,
    station: &str,
    role: Role,
    token: &str,
    send_once: Option<String>,
    instance: Option<String>,
    name: Option<String>,
) -> Result<()> {
    let (ws, _) = connect_async(url)
        .await
        .with_context(|| format!("connecting to relay {url}"))?;
    let (mut write, mut read) = ws.split();

    let mut hello = format!(
        "TLR1 station={} role={} token={}",
        station,
        role.as_str(),
        token
    );
    if let Some(inst) = &instance {
        hello.push_str(&format!(" instance={inst}"));
    }
    // name= must stay LAST (the relay parses it as the free-form line remainder).
    if let Some(n) = &name {
        hello.push_str(&format!(" name={n}"));
    }
    write.send(Message::Text(hello.into())).await?;

    if let Some(reply) = read.next().await {
        println!("{}", printable_message(reply?));
    }

    if let Some(text) = send_once {
        write.send(Message::Text(text.into())).await?;
        if let Ok(Some(reply)) = timeout(Duration::from_secs(5), read.next()).await {
            println!("{}", printable_message(reply?));
        }
        return Ok(());
    }

    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();

    loop {
        tokio::select! {
            line = lines.next_line() => {
                match line? {
                    Some(line) => write.send(Message::Text(line.into())).await?,
                    None => break,
                }
            }
            message = read.next() => {
                match message {
                    Some(Ok(message)) => println!("{}", printable_message(message)),
                    Some(Err(err)) => return Err(err.into()),
                    None => break,
                }
            }
        }
    }

    Ok(())
}

fn parse_hello(text: &str) -> Result<Hello> {
    // `name=` (optional) is a free-form LAST field that may contain spaces (device
    // labels like "Surface Pro"), so pull it off the end first; the rest is parsed
    // as whitespace-separated key=value. The leading space in " name=" prevents a
    // false match inside another value.
    let (fields, name) = match text.find(" name=") {
        Some(pos) => (
            &text[..pos],
            sanitize_name(&text[pos + " name=".len()..]),
        ),
        None => (text, None),
    };

    let mut parts = fields.split_whitespace();
    match parts.next() {
        Some("TLR1") => {}
        _ => bail!("hello must start with TLR1"),
    }

    let mut station = None;
    let mut role = None;
    let mut token = None;
    let mut instance = None;

    for part in parts {
        // Tolerate stray tokens without '=' (forward-compat), same spirit as the
        // ignore-unknown-key rule below.
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key {
            "station" => station = Some(value.to_string()),
            "role" => role = Some(Role::parse(value)?),
            "token" => token = Some(value.to_string()),
            "instance" => instance = Some(value.to_string()),
            // Ignore unknown fields for forward-compat: a newer client adding a hello
            // field must not be rejected by an older relay (that was the cause of the
            // 'instance' skew). Known fields above are still parsed.
            _ => {}
        }
    }

    let station = station.ok_or_else(|| anyhow!("hello missing station"))?;
    if station.is_empty() || station.len() > 64 || station.contains('\0') {
        bail!("invalid station name");
    }

    Ok(Hello {
        station,
        role: role.ok_or_else(|| anyhow!("hello missing role"))?,
        token: token.ok_or_else(|| anyhow!("hello missing token"))?,
        instance: instance.filter(|s| !s.is_empty() && s.len() <= 128),
        name,
    })
}

/// Sanitize a device name from the hello for safe logging: trim, drop control
/// chars and quotes/backslashes (log-injection guard), cap at 40 chars. Spaces are
/// kept on purpose (e.g. "Surface Pro"). Empty result -> None (fall back to inst=).
fn sanitize_name(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|c| !c.is_control() && *c != '"' && *c != '\\')
        .take(40)
        .collect();
    let cleaned = cleaned.trim_end().to_string();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

fn room_key(station: &str, token: &str) -> String {
    format!("{station}\0{token}")
}

fn printable_message(message: Message) -> String {
    match message {
        Message::Text(text) => format!("text: {text}"),
        Message::Binary(data) => format!("binary: {} bytes", data.len()),
        Message::Ping(_) => "ping".to_string(),
        Message::Pong(_) => "pong".to_string(),
        Message::Close(_) => "close".to_string(),
        Message::Frame(_) => "frame".to_string(),
    }
}

fn parse_args(args: Vec<String>) -> Result<Mode> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        std::process::exit(0);
    }

    if args.iter().any(|arg| arg == "--self-test") {
        return Ok(Mode::SelfTest);
    }

    if args.first().map(String::as_str) == Some("station") {
        return parse_station_args(&args);
    }

    if args.first().map(String::as_str) == Some("set-admin-password") {
        let db = value_after(&args, "--db")
            .or_else(|| env::var("THETISLINK_RELAY_DB").ok())
            .unwrap_or_else(|| DEFAULT_DB.to_string());
        let password = value_after(&args, "--password")
            .or_else(|| args.get(1).filter(|a| !a.starts_with("--")).cloned())
            .ok_or_else(|| anyhow!("set-admin-password: give the password as an argument or --password"))?;
        return Ok(Mode::SetAdminPassword { db, password });
    }

    if let Some(url) = value_after(&args, "--connect") {
        let station = value_after(&args, "--station").unwrap_or_else(|| "test".to_string());
        let role = Role::parse(
            &value_after(&args, "--role").ok_or_else(|| anyhow!("--role is required"))?,
        )?;
        let token = value_after(&args, "--token")
            .or_else(|| env::var("THETISLINK_RELAY_TOKEN").ok())
            .ok_or_else(|| anyhow!("--token or THETISLINK_RELAY_TOKEN is required"))?;
        let send_once = value_after(&args, "--send");
        let instance = value_after(&args, "--instance");
        let name = value_after(&args, "--name");
        return Ok(Mode::Connect {
            url,
            station,
            role,
            token,
            send_once,
            instance,
            name,
        });
    }

    let listen = value_after(&args, "--listen")
        .or_else(|| env::var("THETISLINK_RELAY_LISTEN").ok())
        .unwrap_or_else(|| DEFAULT_LISTEN.to_string());
    Ok(Mode::Serve { listen })
}

fn value_after(args: &[String], key: &str) -> Option<String> {
    args.windows(2)
        .find_map(|pair| (pair[0] == key).then(|| pair[1].clone()))
}

fn parse_station_args(args: &[String]) -> Result<Mode> {
    let db = value_after(args, "--db")
        .or_else(|| env::var("THETISLINK_RELAY_DB").ok())
        .unwrap_or_else(|| DEFAULT_DB.to_string());
    match args.get(1).map(String::as_str) {
        Some("add") => {
            let label = value_after(args, "--label")
                .ok_or_else(|| anyhow!("station add: --label is required"))?;
            let owner = value_after(args, "--owner").unwrap_or_default();
            Ok(Mode::Station(StationCmd::Add { db, label, owner }))
        }
        Some("list") => Ok(Mode::Station(StationCmd::List { db })),
        Some(sub @ ("enable" | "disable")) => Ok(Mode::Station(StationCmd::SetEnabled {
            db,
            id: positional_id(args, 2)?,
            enabled: sub == "enable",
        })),
        Some("rm" | "remove") => Ok(Mode::Station(StationCmd::Remove {
            db,
            id: positional_id(args, 2)?,
        })),
        _ => bail!(
            "station: expected  add --label X [--owner Y] | list | enable <id> | disable <id> | rm <id>  [--db PATH]"
        ),
    }
}

/// First numeric argument at or after `from` (skips flags like `--db PATH`).
fn positional_id(args: &[String], from: usize) -> Result<i64> {
    args.iter()
        .skip(from)
        .find_map(|a| a.parse::<i64>().ok())
        .ok_or_else(|| anyhow!("expected a numeric station id"))
}

#[cfg(test)]
mod udp_lease_tests {
    use super::*;

    fn client_session(conn: u64, client_id: u8) -> UdpSession {
        UdpSession {
            room_key: "db:1".into(),
            station_id: Some(1),
            client_id: Some(client_id),
            role: Role::Client,
            conn,
            expires_at: Instant::now() + Duration::from_secs(60),
            src: None,
            last_seq: None,
            bytes: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The fault a phone showed after switching between WiFi and mobile data:
    /// the control channel kept working and audio never came back, with nothing
    /// in any log saying why.
    ///
    /// Both connections are briefly alive after a network change. The new one
    /// reclaims the same client id - deliberately, so a device cannot pile up
    /// slots - and then the old socket times out. Its cleanup must not take the
    /// live connection's token with it.
    #[test]
    fn a_dying_connection_does_not_revoke_the_token_that_replaced_it() {
        let mut toks: HashMap<String, UdpSession> = HashMap::new();
        toks.insert("old".into(), client_session(7, 2)); // on the old network
        toks.insert("new".into(), client_session(8, 2)); // took the id over

        let left = revoke_tokens_of(&mut toks, 7);

        assert_eq!(left, 1, "the live token should have survived");
        assert!(toks.contains_key("new"), "the live connection lost its token");
        assert!(!toks.contains_key("old"), "the dead connection kept its token");
    }

    /// And a connection does clean up after itself: every token it issued goes,
    /// not just the current one. Rotation leaves a superseded token alive for
    /// the length of its overlap window.
    #[test]
    fn a_connection_takes_all_of_its_own_tokens_with_it() {
        let mut toks: HashMap<String, UdpSession> = HashMap::new();
        toks.insert("current".into(), client_session(3, 0));
        toks.insert("superseded".into(), client_session(3, 0));
        toks.insert("someone else".into(), client_session(4, 1));

        assert_eq!(revoke_tokens_of(&mut toks, 3), 1);
        assert!(toks.contains_key("someone else"));
    }
}

#[cfg(test)]
mod auth_tests {
    use super::*;

    fn hello(token: &str) -> Hello {
        Hello {
            station: "PA3GHM".to_string(),
            role: Role::Client,
            token: token.to_string(),
            instance: None,
            name: None,
        }
    }

    fn store_with(secrets: &[&str]) -> StationStore {
        let s = store::Store::open(":memory:").unwrap();
        for (i, sec) in secrets.iter().enumerate() {
            s.add(&format!("st{i}"), "", sec, i as i64).unwrap();
        }
        Some(Arc::new(std::sync::Mutex::new(s)))
    }

    #[test]
    fn registry_match_returns_room_id() {
        let store = store_with(&["good-secret"]);
        assert!(matches!(
            authorize(&store, &hello("good-secret"), &None),
            AuthOutcome::Registry(_)
        ));
    }

    #[test]
    fn registry_bad_secret_rejected_even_with_matching_legacy_token() {
        let store = store_with(&["good-secret"]);
        assert!(matches!(
            authorize(&store, &hello("wrong"), &None),
            AuthOutcome::Reject
        ));
        // A non-empty registry cannot be bypassed by the legacy token.
        assert!(matches!(
            authorize(&store, &hello("wrong"), &Some("wrong".to_string())),
            AuthOutcome::Reject
        ));
    }

    #[test]
    fn empty_registry_falls_back_to_legacy() {
        let store = store_with(&[]); // DB present but no stations yet
        assert!(matches!(
            authorize(&store, &hello("anything"), &None),
            AuthOutcome::Legacy
        ));
        assert!(matches!(
            authorize(&store, &hello("tok"), &Some("tok".to_string())),
            AuthOutcome::Legacy
        ));
        assert!(matches!(
            authorize(&store, &hello("bad"), &Some("tok".to_string())),
            AuthOutcome::Reject
        ));
    }

    #[test]
    fn no_store_uses_legacy_token() {
        let store: StationStore = None;
        assert!(matches!(
            authorize(&store, &hello("tok"), &Some("tok".to_string())),
            AuthOutcome::Legacy
        ));
        assert!(matches!(
            authorize(&store, &hello("bad"), &Some("tok".to_string())),
            AuthOutcome::Reject
        ));
        // No token configured at all -> open (today's default behavior).
        assert!(matches!(
            authorize(&store, &hello("x"), &None),
            AuthOutcome::Legacy
        ));
    }

    fn zero() -> Arc<AtomicU64> {
        Arc::new(AtomicU64::new(0))
    }

    #[tokio::test]
    async fn same_instance_reclaims_slot_new_instance_gets_new_slot() {
        let rooms = Rooms::default();
        let key = "room";
        let (tx1, _rx1) = mpsc::unbounded_channel();
        let id1 = match register_peer(&rooms, key, Role::Client, tx1, Some("dev-A".to_string()), zero(), MAX_CLIENTS).await {
            Registration::Client(id) => id,
            _ => panic!("expected Client"),
        };
        // Same install id reconnects -> same slot, still one client (no pile-up).
        let (tx2, _rx2) = mpsc::unbounded_channel();
        let id2 = match register_peer(&rooms, key, Role::Client, tx2, Some("dev-A".to_string()), zero(), MAX_CLIENTS).await {
            Registration::Client(id) => id,
            _ => panic!("expected Client"),
        };
        assert_eq!(id1, id2);
        assert_eq!(room_client_count(&rooms, key).await, 1);
        // A different install id -> a genuinely new slot.
        let (tx3, _rx3) = mpsc::unbounded_channel();
        assert!(matches!(
            register_peer(&rooms, key, Role::Client, tx3, Some("dev-B".to_string()), zero(), MAX_CLIENTS).await,
            Registration::Client(_)
        ));
        assert_eq!(room_client_count(&rooms, key).await, 2);
        // A client with no instance always allocates a new slot (legacy).
        let (tx4, _rx4) = mpsc::unbounded_channel();
        assert!(matches!(
            register_peer(&rooms, key, Role::Client, tx4, None, zero(), MAX_CLIENTS).await,
            Registration::Client(_)
        ));
        assert_eq!(room_client_count(&rooms, key).await, 3);
    }

    #[tokio::test]
    async fn route_message_counts_uplink_and_downlink_bytes() {
        let rooms = Rooms::default();
        let key = "room";
        // One station, one client.
        let (stx, mut _srx) = mpsc::unbounded_channel();
        register_peer(&rooms, key, Role::Station, stx, None, zero(), MAX_CLIENTS).await;
        let client_bytes = zero();
        let (ctx, mut _crx) = mpsc::unbounded_channel();
        let cid = match register_peer(&rooms, key, Role::Client, ctx, Some("dev".into()), client_bytes.clone(), MAX_CLIENTS).await {
            Registration::Client(id) => id,
            _ => panic!("expected Client"),
        };

        // Uplink: a client's own frame counts on its own counter (10 bytes).
        route_message(&rooms, key, Role::Client, Some(cid), &client_bytes, Message::Binary(vec![0u8; 10].into())).await;
        assert_eq!(client_bytes.load(Ordering::Relaxed), 10);

        // Downlink: a station TLT1 frame for this client (8-byte header + 5 payload =
        // 13) is attributed to the CLIENT's counter, not the station's own.
        let station_bytes = zero();
        let frame = tlt1_frame(cid, b"hello");
        route_message(&rooms, key, Role::Station, None, &station_bytes, Message::Binary(frame.into())).await;
        assert_eq!(client_bytes.load(Ordering::Relaxed), 10 + 13);
        assert_eq!(station_bytes.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn live_install_ids_sees_station_and_clients_of_the_right_room() {
        let rooms = Rooms::default();
        let (stx, _srx) = mpsc::unbounded_channel();
        register_peer(&rooms, "db:7", Role::Station, stx, Some("srv-dev".into()), zero(), MAX_CLIENTS).await;
        let (ctx, _crx) = mpsc::unbounded_channel();
        register_peer(&rooms, "db:7", Role::Client, ctx, Some("cli-dev".into()), zero(), MAX_CLIENTS).await;

        let live = live_install_ids(&rooms, 7).await;
        assert!(live.contains("srv-dev"), "the station's own session counts too");
        assert!(live.contains("cli-dev"));
        assert_eq!(live.len(), 2);
        // Another station's room is another station's business.
        assert!(live_install_ids(&rooms, 8).await.is_empty());
    }

    #[tokio::test]
    async fn kick_device_closes_only_matching_peer() {
        let rooms = Rooms::default();
        let key = "db:7"; // station_id 7 -> kick_device builds this same key
        let (tx, mut rx) = mpsc::unbounded_channel();
        match register_peer(&rooms, key, Role::Client, tx, Some("a-dev".into()), zero(), MAX_CLIENTS).await {
            Registration::Client(_) => {}
            _ => panic!("expected Client"),
        };
        // A non-matching install id closes nothing.
        assert_eq!(kick_device(&rooms, 7, "other-dev").await, 0);
        // The matching device is closed and its peer receives a Close frame.
        assert_eq!(kick_device(&rooms, 7, "a-dev").await, 1);
        assert!(matches!(rx.recv().await, Some(Message::Close(_))));
    }

    #[tokio::test]
    async fn client_cap_limits_room_and_allows_reclaim() {
        let rooms = Rooms::default();
        let key = "room";
        // Cap of 2: two distinct devices fit.
        for i in 0..2 {
            let (tx, _rx) = mpsc::unbounded_channel();
            assert!(matches!(
                register_peer(&rooms, key, Role::Client, tx, Some(format!("dev-{i}")), zero(), 2).await,
                Registration::Client(_)
            ));
        }
        // A third distinct device is refused at the cap.
        let (tx, _rx) = mpsc::unbounded_channel();
        assert!(matches!(
            register_peer(&rooms, key, Role::Client, tx, Some("dev-x".into()), zero(), 2).await,
            Registration::Full
        ));
        // A reconnect of an existing device still reclaims its own slot at the cap.
        let (tx, _rx) = mpsc::unbounded_channel();
        assert!(matches!(
            register_peer(&rooms, key, Role::Client, tx, Some("dev-0".into()), zero(), 2).await,
            Registration::Client(_)
        ));
        assert_eq!(room_client_count(&rooms, key).await, 2);
    }

    #[test]
    fn year_month_known_timestamps() {
        assert_eq!(year_month(0), "1970-01"); // epoch
        assert_eq!(year_month(1_783_699_200), "2026-07"); // 2026-07-09 00:00 UTC
        assert_eq!(year_month(1_704_067_199), "2023-12"); // 2023-12-31 23:59:59 UTC
        assert_eq!(year_month(1_709_251_200), "2024-03"); // 2024-03-01 (leap year)
    }

    #[test]
    fn udp_token_is_256bit_hex_and_unique() {
        let a = gen_udp_token();
        let b = gen_udp_token();
        assert_eq!(a.len(), 64); // 32 bytes hex = 256 bit (S1: >=128)
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn seq_ok_replay_window() {
        assert!(seq_ok(None, 5)); // first packet
        assert!(seq_ok(Some(5), 6)); // newer
        assert!(!seq_ok(Some(5), 5)); // exact duplicate = replay
        assert!(seq_ok(Some(500), 400)); // recent reorder (within window)
        assert!(!seq_ok(Some(1000), 100)); // far behind = stale/replay
        assert!(seq_ok(Some(u32::MAX), 0)); // wrap: 0 is newer than u32::MAX
    }

    fn build_tlu1(target_client_id: u8, seq: u32, token: &[u8; 32], payload: &[u8]) -> Vec<u8> {
        let mut f = Vec::with_capacity(TLU1_HEADER_LEN + payload.len());
        f.extend_from_slice(TLU1_MAGIC);
        f.push(TLU1_VERSION);
        f.push(0); // flags
        f.push(target_client_id);
        f.extend_from_slice(&seq.to_le_bytes());
        f.extend_from_slice(token);
        f.extend_from_slice(payload);
        f
    }
    fn token_hex(token: &[u8; 32]) -> String {
        token.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[tokio::test]
    async fn udp_path_forwards_valid_and_drops_invalid() {
        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = relay.local_addr().unwrap();
        let station = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let station_addr = station.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let station_tok = [0xaau8; 32];
        let client_tok = [0xbbu8; 32];
        let client_bytes = Arc::new(AtomicU64::new(0));
        let tokens = UdpTokens::default();
        {
            let mut t = tokens.lock().unwrap();
            t.insert(
                token_hex(&station_tok),
                UdpSession {
                    room_key: "db:1".into(), station_id: Some(1), client_id: None,
                    role: Role::Station, conn: 1, expires_at: Instant::now() + Duration::from_secs(60),
                    src: Some(station_addr), last_seq: None,
                    bytes: Arc::new(AtomicU64::new(0)),
                },
            );
            t.insert(
                token_hex(&client_tok),
                UdpSession {
                    room_key: "db:1".into(), station_id: Some(1), client_id: Some(0),
                    role: Role::Client, conn: 2, expires_at: Instant::now() + Duration::from_secs(60),
                    src: None, last_seq: None,
                    bytes: client_bytes.clone(),
                },
            );
        }
        tokio::spawn(serve_udp(relay, tokens.clone()));

        // Valid client datagram -> forwarded to the station's learned src, framed with
        // the sending client's id ([cid] + payload).
        client.send_to(&build_tlu1(0, 1, &client_tok, b"hello-udp"), relay_addr).await.unwrap();
        let mut buf = [0u8; 128];
        let (n, _) = timeout(Duration::from_secs(2), station.recv_from(&mut buf))
            .await.unwrap().unwrap();
        assert_eq!(buf[0], 0); // stamped with the sending client's id
        assert_eq!(&buf[1..n], b"hello-udp");

        // Unknown token -> dropped, nothing forwarded.
        client.send_to(&build_tlu1(0, 2, &[0x11u8; 32], b"nope"), relay_addr).await.unwrap();
        assert!(timeout(Duration::from_millis(300), station.recv_from(&mut buf)).await.is_err());

        // Replay of the high-water sequence (exact duplicate) -> dropped.
        client.send_to(&build_tlu1(0, 1, &client_tok, b"dup"), relay_addr).await.unwrap();
        assert!(timeout(Duration::from_millis(300), station.recv_from(&mut buf)).await.is_err());

        // Byte accounting: only the one valid forwarded datagram is charged to the
        // sending client (whole datagram incl. TLU1 overhead); the dropped unknown-token
        // and replay packets cost nothing.
        let expected = build_tlu1(0, 1, &client_tok, b"hello-udp").len() as u64;
        assert_eq!(client_bytes.load(Ordering::Relaxed), expected);
    }

    #[test]
    fn platform_of_maps_prefix() {
        assert_eq!(platform_of("a-1234"), "android");
        assert_eq!(platform_of("d-5678"), "desktop");
        assert_eq!(platform_of("x-9"), "unknown");
        assert_eq!(platform_of(""), "unknown");
    }

    #[test]
    fn forwarded_ip_takes_first_and_trims() {
        use tokio_tungstenite::tungstenite::http::HeaderMap;
        let mut h = HeaderMap::new();
        assert_eq!(forwarded_ip(&h), None);
        // Caddy appends the immediate peer on the right; the client is left-most.
        h.insert("x-forwarded-for", "203.0.113.7, 10.0.0.2".parse().unwrap());
        assert_eq!(forwarded_ip(&h).as_deref(), Some("203.0.113.7"));
        let mut blank = HeaderMap::new();
        blank.insert("x-forwarded-for", "   ".parse().unwrap());
        assert_eq!(forwarded_ip(&blank), None);
    }

    // additivity tests: name= is optional metadata and must not break parsing.
    #[test]
    fn parse_hello_without_name_ok() {
        let h = parse_hello("TLR1 station=X role=client token=t instance=a123").unwrap();
        assert_eq!(h.station, "X");
        assert_eq!(h.instance.as_deref(), Some("a123"));
        assert!(h.name.is_none());
    }

    #[test]
    fn parse_hello_with_name_including_spaces() {
        let h = parse_hello("TLR1 station=X role=client token=t instance=a123 name=Surface Pro")
            .unwrap();
        assert_eq!(h.name.as_deref(), Some("Surface Pro"));
        assert_eq!(h.instance.as_deref(), Some("a123"));
        assert_eq!(h.station, "X");
    }

    #[test]
    fn parse_hello_ignores_unknown_field() {
        let h = parse_hello("TLR1 station=X role=client token=t future=xyz instance=a123").unwrap();
        assert_eq!(h.station, "X");
        assert_eq!(h.instance.as_deref(), Some("a123"));
        assert!(h.name.is_none());
    }

    #[test]
    fn sanitize_name_trims_caps_strips() {
        assert_eq!(sanitize_name("  Pixel 8  ").as_deref(), Some("Pixel 8"));
        assert!(sanitize_name("   ").is_none());
        // quotes, backslash and control chars are stripped (log-injection guard).
        assert_eq!(sanitize_name("a\"b\\c\u{7}d").as_deref(), Some("abcd"));
        assert_eq!(sanitize_name(&"x".repeat(100)).unwrap().chars().count(), 40);
    }
}

fn print_help() {
    println!(
        "ThetisLink relay prototype\n\n\
         Serve:\n  thetislink-relay --listen 0.0.0.0:18080\n\n\
         Self-test:\n  thetislink-relay --self-test\n\n\
         Connect test client:\n  thetislink-relay --connect ws://127.0.0.1:18080 --station test --role station --token secret\n\n\
         Station registry (Fase 1 admin):\n  thetislink-relay station add --label PA3GHM [--owner name] [--db PATH]\n  thetislink-relay station list\n  thetislink-relay station disable <id>\n  thetislink-relay station enable <id>\n  thetislink-relay station rm <id>\n\n\
         Environment:\n  THETISLINK_RELAY_TOKEN   Optional required token for server mode\n  THETISLINK_RELAY_LISTEN  Optional listen address for server mode\n  THETISLINK_RELAY_DB      Path to the station registry db (default stations.db)"
    );
}
