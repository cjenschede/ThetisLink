// SPDX-License-Identifier: GPL-2.0-or-later

//! The direct path over QUIC (phase 3, `OPZET-fase3-quic-direct-pad.md`).
//!
//! Two halves with the shape the relay tunnel already has - a channel of
//! `(address, packet)` in each direction - so neither the server's sessions nor
//! the client engine learn a new transport:
//!
//! - [`QuicServer`]: accepts connections and gives each one a sentinel address
//!   in `198.51.100.0/24` (RFC 5737 TEST-NET-2, next to the relay's TEST-NET-3).
//!   The server sees a client by that address, as it sees a relay client by its
//!   `203.0.113.x` address.
//! - [`QuicClient`]: connects to whatever address the engine sends to, and tags
//!   what comes back with that same address, as a direct UDP socket would.
//!
//! Both can also go through the relay (phase 3b, `route.rs`): quinn then sees
//! each relay client at a fixed address in `192.0.2.0/24` (TEST-NET-1) - a third
//! made-up range, below the sentinels the sessions see. And from there the two
//! punch a hole and carry the same connection directly (phase 3c, `punch.rs`).
//!
//! What travels how: a packet that fits one QUIC datagram goes as a datagram
//! (audio, control - never held back after a loss); anything larger goes on a
//! short stream of its own (spectrum, lists). The rule is the size, not a list of
//! packet types, so there is no list to drift.
//!
//! Each connection also yields a [`Binding`]: 32 bytes both ends derive from the
//! TLS session. The password proof is bound to it (build 109/110), which is what
//! makes an unchecked certificate safe: someone in the middle holds two TLS
//! sessions, two different bindings, and a proof that fits neither.

use std::collections::{HashMap, VecDeque};
use sdr_remote_core::dev_info;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod punch;
mod route;
/// A stand-in relay for tests, here and in the crates that test through it.
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub use route::{relay_peer_for, relay_peer_id, ClientPath, RelayRoute, TcpLine, UdpGrant, RELAY_MTU_CEILING};

use bytes::Bytes;
use log::{debug, info, warn};
use quinn::congestion::{BbrConfig, Controller, ControllerFactory};
use quinn::{Connection, Endpoint, SendDatagramError};
use sdr_remote_core::path::PathKind;
use tokio::sync::{mpsc, watch, Semaphore};

/// The application protocol both ends announce; a QUIC peer that is not
/// ThetisLink fails the handshake instead of reaching the sessions.
pub const ALPN: &[u8] = b"thetislink/1";

/// The label of the TLS keying-material export that becomes the [`Binding`].
pub const BINDING_LABEL: &[u8] = b"EXPORTER-ThetisLink-auth-binding";
pub const BINDING_SIZE: usize = 32;
/// 32 bytes both ends of one QUIC connection derive from its TLS session.
pub type Binding = [u8; BINDING_SIZE];

/// QUIC's own keepalive, as in all three experiments.
pub const KEEPALIVE: Duration = Duration::from_secs(1);
/// How long a silent connection is kept. Longer than the network gaps of the
/// third experiment (13-15 s of airplane mode, after which the connection moved
/// on), and longer than the server's 15 s session timeout, so a connection never
/// dies before the session it carries. The decision of 04-10: "longer than 10 s".
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Room for queued datagrams. quinn's default is 1 MiB, which in the second
/// experiment delivered audio seconds late after a loss; a small queue drops
/// the old instead. About four ticks of a busy stream - to be measured in 3a.
pub const DATAGRAM_QUEUE: usize = 4 * 1024;
/// How often a connection, and a relay leg, writes its measuring line
/// (measurement build 122: where packets on the relay path are lost).
pub(crate) const STATS_EVERY: Duration = Duration::from_secs(10);
/// The largest packet that arrives on a stream (the biggest today are 60 kB list
/// chunks); a stream that says more is cut off, not buffered without end.
pub const MAX_STREAM_PACKET: usize = 128 * 1024;
/// Packets a client keeps while its connection is being made, so the first
/// packets of a session are not thrown away. The oldest go first.
pub const CONNECT_BACKLOG: usize = 32;
/// Large packets (streams) in flight per connection. On a connection that has
/// stopped moving, each would wait for credit with its own copy of the packet
/// until the 30 s silence limit; past this many a new one is dropped instead -
/// what travels on streams (spectrum, lists) is sent again (review 108).
pub const MAX_STREAMS_IN_FLIGHT: usize = 16;

/// Queue `packet` while a connection is made: at most [`CONNECT_BACKLOG`], the
/// oldest dropped first.
fn queue(backlog: &mut VecDeque<Vec<u8>>, packet: Vec<u8>) {
    if backlog.len() == CONNECT_BACKLOG {
        backlog.pop_front();
    }
    backlog.push_back(packet);
}

/// The congestion controller. Not decided (OPZET §7): BBR until 3a measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Congestion {
    /// BBR with the small datagram queue ("qb" in the experiments).
    Bbr,
    /// A fixed, large window ("qc"): for measuring only.
    NoBrake,
}

const SENTINEL_NET: [u8; 3] = [198, 51, 100];

/// True for an address in the QUIC sentinel range (198.51.100.0/24).
pub fn is_quic_sentinel(addr: SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(v4) => v4.octets()[..3] == SENTINEL_NET,
        IpAddr::V6(_) => false,
    }
}

/// The sentinel of the `n`-th connection: the port counts first, then the last
/// octet, so a sentinel is not handed out again for 254 x 65535 connections - a
/// server session left behind by a closed connection can never be picked up by
/// the next one.
fn sentinel(n: u32) -> SocketAddr {
    let port = (n % 65535) as u16 + 1;
    let host = ((n / 65535) % 254) as u8 + 1;
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(SENTINEL_NET[0], SENTINEL_NET[1], SENTINEL_NET[2], host)), port)
}

fn binding_of(conn: &Connection) -> Option<Binding> {
    let mut out = [0u8; BINDING_SIZE];
    conn.export_keying_material(&mut out, BINDING_LABEL, b"").ok().map(|_| out)
}

fn transport(congestion: Congestion, via_relay: bool) -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    if via_relay {
        // Room for the relay's header below a 1500-byte path (route.rs).
        let mut mtu = quinn::MtuDiscoveryConfig::default();
        mtu.upper_bound(RELAY_MTU_CEILING);
        t.mtu_discovery_config(Some(mtu));
    }
    t.keep_alive_interval(Some(KEEPALIVE));
    t.max_idle_timeout(Some(IDLE_TIMEOUT.try_into().expect("idle timeout fits QUIC's varint")));
    t.datagram_send_buffer_size(DATAGRAM_QUEUE);
    match congestion {
        Congestion::Bbr => {
            t.congestion_controller_factory(Arc::new(BbrConfig::default()));
        }
        Congestion::NoBrake => {
            t.congestion_controller_factory(Arc::new(NoBrake));
        }
    }
    Arc::new(t)
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// What `send_packet` did with what it was handed. quinn counts what it sent,
/// not what it dropped from its queue, and nothing of what never reached it:
/// these are the rest, so that "all zero" in the line means "not ours" (review
/// 122: the stream path and a refused datagram were counted nowhere).
#[derive(Default)]
struct LinkCounts {
    /// Datagrams `send_datagram` took.
    offered: AtomicU64,
    /// Datagrams `send_datagram` refused (other than too large).
    refused: AtomicU64,
    /// Packets too large for a datagram, handed to a stream.
    streams: AtomicU64,
    /// Of those, dropped because [`MAX_STREAMS_IN_FLIGHT`] were in flight.
    streams_no_slot: AtomicU64,
    /// Of those, whose stream could not be written.
    streams_failed: AtomicU64,
    /// Packets too large for a datagram, for a connection that was closing.
    closing: AtomicU64,
}

/// A connection and the room it has for streams in flight, and what
/// `send_packet` did on it.
#[derive(Clone)]
struct Link {
    conn: Connection,
    streams: Arc<Semaphore>,
    counts: Arc<LinkCounts>,
}

impl Link {
    /// The link, and its measuring line every [`STATS_EVERY`] until the
    /// connection closes. `label` names it without an address: the sentinel
    /// on the server, "to the server" on a client.
    fn new(conn: Connection, label: String) -> Self {
        let link = Link { conn, streams: Arc::new(Semaphore::new(MAX_STREAMS_IN_FLIGHT)), counts: Arc::default() };
        let (conn, counts) = (link.conn.clone(), link.counts.clone());
        tokio::spawn(async move {
            let mut prev = Snapshot::of(&conn, &counts);
            loop {
                tokio::select! {
                    _ = conn.closed() => break,
                    _ = tokio::time::sleep(STATS_EVERY) => {}
                }
                let now = Snapshot::of(&conn, &counts);
                dev_info!("QUIC ({} s) {label}: {}", STATS_EVERY.as_secs(), now.since(&prev));
                prev = now;
            }
        });
        link
    }
}

/// The counters of a connection at one moment: the datagrams handed to quinn,
/// and quinn's own figures.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Snapshot {
    offered: u64,
    refused: u64,
    streams: u64,
    streams_no_slot: u64,
    streams_failed: u64,
    closing: u64,
    datagrams_sent: u64,
    datagrams_received: u64,
    packets_sent: u64,
    packets_lost: u64,
    congestion_events: u64,
    rtt_ms: u64,
    window: u64,
    mtu: u16,
}

impl Snapshot {
    fn of(conn: &Connection, counts: &LinkCounts) -> Snapshot {
        let s = conn.stats();
        Snapshot {
            offered: counts.offered.load(Ordering::Relaxed),
            refused: counts.refused.load(Ordering::Relaxed),
            streams: counts.streams.load(Ordering::Relaxed),
            streams_no_slot: counts.streams_no_slot.load(Ordering::Relaxed),
            streams_failed: counts.streams_failed.load(Ordering::Relaxed),
            closing: counts.closing.load(Ordering::Relaxed),
            datagrams_sent: s.frame_tx.datagram,
            datagrams_received: s.frame_rx.datagram,
            packets_sent: s.path.sent_packets,
            packets_lost: s.path.lost_packets,
            congestion_events: s.path.congestion_events,
            rtt_ms: s.path.rtt.as_millis() as u64,
            window: s.path.cwnd,
            mtu: s.path.current_mtu,
        }
    }

    /// The window from `prev` to this one, as the log line says it.
    /// "Dropped from the queue" is what was handed over and not sent: quinn
    /// drops the oldest when [`DATAGRAM_QUEUE`] is full. A datagram still
    /// queued at the edge of a window counts there and is sent in the next,
    /// so it is exact over time, not per window.
    fn since(&self, prev: &Snapshot) -> String {
        let offered = self.offered.saturating_sub(prev.offered);
        let sent = self.datagrams_sent.saturating_sub(prev.datagrams_sent);
        format!(
            "datagrams offered {offered}, sent {sent}, dropped from the queue {}, refused {}, received {}; large packets on streams {}, dropped (no slot) {}, failed {}, connection closing {}; packets sent {}, lost {}; congestion events {}; rtt {} ms, window {} B, mtu {}",
            offered.saturating_sub(sent),
            self.refused.saturating_sub(prev.refused),
            self.datagrams_received.saturating_sub(prev.datagrams_received),
            self.streams.saturating_sub(prev.streams),
            self.streams_no_slot.saturating_sub(prev.streams_no_slot),
            self.streams_failed.saturating_sub(prev.streams_failed),
            self.closing.saturating_sub(prev.closing),
            self.packets_sent.saturating_sub(prev.packets_sent),
            self.packets_lost.saturating_sub(prev.packets_lost),
            self.congestion_events.saturating_sub(prev.congestion_events),
            self.rtt_ms,
            self.window,
            self.mtu,
        )
    }
}

/// Send one packet on a connection: a datagram when it fits, otherwise a short
/// stream of its own (spawned, so a slow stream holds up nothing else - at most
/// [`MAX_STREAMS_IN_FLIGHT`] of them, and none on a connection that is closing).
fn send_packet(link: &Link, packet: Vec<u8>) {
    let conn = &link.conn;
    let bytes = Bytes::from(packet);
    let fits = conn.max_datagram_size().map_or(false, |max| bytes.len() <= max);
    if fits {
        match conn.send_datagram(bytes.clone()) {
            Ok(()) => {
                link.counts.offered.fetch_add(1, Ordering::Relaxed);
                return;
            }
            // The path shrank between the check and the send: a stream then.
            Err(SendDatagramError::TooLarge) => {}
            Err(e) => {
                link.counts.refused.fetch_add(1, Ordering::Relaxed);
                debug!("QUIC datagram not sent: {e}");
                return;
            }
        }
    }
    if conn.close_reason().is_some() {
        link.counts.closing.fetch_add(1, Ordering::Relaxed);
        return;
    }
    link.counts.streams.fetch_add(1, Ordering::Relaxed);
    let Ok(slot) = link.streams.clone().try_acquire_owned() else {
        link.counts.streams_no_slot.fetch_add(1, Ordering::Relaxed);
        debug!("QUIC: {} streams in flight - a {} B packet dropped", MAX_STREAMS_IN_FLIGHT, bytes.len());
        return;
    };
    let conn = conn.clone();
    let counts = link.counts.clone();
    tokio::spawn(async move {
        let _slot = slot;
        let result = async {
            let mut s = conn.open_uni().await.map_err(|e| e.to_string())?;
            s.write_all(&bytes).await.map_err(|e| e.to_string())?;
            s.finish().map_err(|e| e.to_string())
        }
        .await;
        if let Err(e) = result {
            counts.streams_failed.fetch_add(1, Ordering::Relaxed);
            debug!("QUIC stream packet ({} B) not sent: {e}", bytes.len());
        }
    });
}

/// Deliver everything a connection receives - datagrams and stream packets -
/// into `inbound`, tagged with `tag`. Ends when the connection does.
/// Returns the datagram reader, which ends once the connection is closed AND
/// every datagram it had already received is delivered (quinn drains them
/// before it reports the close). An empty frame is never passed on: no packet
/// is empty (the header alone is four bytes), and the server uses an empty
/// frame to say a connection closed.
/// A punch signal is taken out on the way (`punch::intercept`), for `side`.
fn spawn_readers(
    conn: Connection,
    tag: SocketAddr,
    inbound: mpsc::UnboundedSender<(SocketAddr, Vec<u8>)>,
    side: punch::Side,
) -> tokio::task::JoinHandle<()> {
    let datagrams = {
        let (conn, inbound) = (conn.clone(), inbound.clone());
        tokio::spawn(async move {
            while let Ok(d) = conn.read_datagram().await {
                if d.is_empty() || punch::intercept(&side, &conn, &d) {
                    continue;
                }
                if inbound.send((tag, d.to_vec())).is_err() {
                    break;
                }
            }
        })
    };
    tokio::spawn(async move {
        while let Ok(mut s) = conn.accept_uni().await {
            // Nobody listens any more: stop taking streams, as the datagram
            // reader stops (review 108).
            if inbound.is_closed() {
                break;
            }
            let inbound = inbound.clone();
            tokio::spawn(async move {
                match s.read_to_end(MAX_STREAM_PACKET).await {
                    Ok(p) if p.is_empty() => {}
                    Ok(p) => {
                        let _ = inbound.send((tag, p));
                    }
                    Err(e) => debug!("QUIC stream packet not received: {e}"),
                }
            });
        }
    });
    datagrams
}

// ------------------------------------------------------------------ server

/// One live connection on the server: its binding, the connection itself (for
/// the peer's real address), and whether it came in through the relay. That
/// last is said once, at the accept: the address follows a move to the client's
/// local address (L+R, build 142), and after it no longer tells (review 142).
struct Held {
    binding: Binding,
    conn: Connection,
    via_relay: bool,
}

/// What the server knows of each live connection, by sentinel.
#[derive(Clone, Default)]
pub struct Bindings(Arc<Mutex<HashMap<SocketAddr, Held>>>);

impl Bindings {
    /// The binding of the connection behind `sentinel`, while it lives.
    pub fn get(&self, sentinel: SocketAddr) -> Option<Binding> {
        self.0.lock().unwrap().get(&sentinel).map(|h| h.binding)
    }

    /// The real address of the peer behind `sentinel`, now (it follows a
    /// migration). The server keys what must not restart with every new
    /// connection - the brake on password guessing - on this, not on the
    /// sentinel, which is new for every connection (review 109).
    pub fn peer(&self, sentinel: SocketAddr) -> Option<SocketAddr> {
        self.0.lock().unwrap().get(&sentinel).map(|h| h.conn.remote_address())
    }
}

/// Which way each client's traffic takes, as the server sees it (build 143, the
/// connection names): one answer for the client table and for the log.
#[derive(Clone)]
pub struct Ways {
    bindings: Bindings,
    route: Option<Arc<route::ServerRouteSocket>>,
}

impl Ways {
    /// The way of the client behind `sentinel`, while it lives.
    pub fn of(&self, sentinel: SocketAddr) -> Option<PathKind> {
        let (now, via_relay) = self.bindings.0.lock().unwrap().get(&sentinel).map(|h| (h.conn.remote_address(), h.via_relay))?;
        Some(way_of(
            now,
            via_relay,
            |id| self.route.as_ref().is_some_and(|r| r.direct_only(id)),
            |id| self.route.as_ref().is_some_and(|r| r.tcp_only(id)),
        ))
    }

    /// Every live client and its way.
    fn all(&self) -> Vec<(SocketAddr, PathKind)> {
        let tags: Vec<SocketAddr> = self.bindings.0.lock().unwrap().keys().copied().collect();
        tags.into_iter().filter_map(|t| self.of(t).map(|k| (t, k))).collect()
    }
}

/// The way of a client now at `now`: through the relay it is at its relay
/// address, and D+R once the server sends to it directly only; one that came
/// in through the relay and moved is L+R at a local address; one that came
/// straight is L or D by its address. The server knows no coupling: it says
/// D+R only once nothing came through the relay for `RELAY_QUIET`. At its relay
/// address and heard over the relay's TCP line only, it is `R (TCP)` (build 146).
fn way_of(now: SocketAddr, via_relay: bool, direct_only: impl Fn(u8) -> bool, tcp_only: impl Fn(u8) -> bool) -> PathKind {
    match relay_peer_id(now) {
        Some(id) if direct_only(id) => PathKind::DirectViaRelay,
        Some(id) if tcp_only(id) => PathKind::RelayTcp,
        Some(_) => PathKind::Relay,
        None if via_relay => {
            if sdr_remote_core::path::is_local(now.ip()) {
                PathKind::LocalViaRelay
            } else {
                PathKind::DirectViaRelay
            }
        }
        None => PathKind::without_relay(now.ip()),
    }
}

/// How often the server looks whether a client's way changed, for the log.
const WAY_REPORT_EVERY: Duration = Duration::from_millis(500);

/// The clients whose way to say now: each one seen for the first time, or with
/// another way than last said. `said` forgets the ones that are gone. The log
/// task's decision as a function with a table test (review 143: none of its
/// three branches had a test).
fn ways_to_say(now: &[(SocketAddr, PathKind)], said: &mut HashMap<SocketAddr, PathKind>) -> Vec<(SocketAddr, PathKind)> {
    said.retain(|tag, _| now.iter().any(|(t, _)| t == tag));
    now.iter().filter(|&&(tag, kind)| said.insert(tag, kind) != Some(kind)).copied().collect()
}

/// The server half: accepts QUIC connections on one UDP port. Dropping it closes
/// the endpoint; keep it as long as the server listens.
pub struct QuicServer {
    /// The binding of each live connection.
    pub bindings: Bindings,
    endpoint: Endpoint,
    /// The route layer under the endpoint, when there is a relay.
    route: Option<Arc<route::ServerRouteSocket>>,
    /// Ends the task that logs each client's way, when the server goes.
    _ways_report: tokio::sync::oneshot::Sender<()>,
}

/// Whether a server takes clients straight on its own port, or through the
/// relay only (build 135, "rechtstreeks verbinden toestaan").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direct {
    Allowed,
    RelayOnly,
}

/// Lets a proven client punch ([`QuicServer::allow_punch`]).
#[derive(Clone)]
pub struct PunchGate {
    bindings: Bindings,
    route: Option<Arc<route::ServerRouteSocket>>,
}

impl PunchGate {
    pub fn allow(&self, sentinel: SocketAddr) {
        let Some(route) = &self.route else { return };
        if let Some(h) = self.bindings.0.lock().unwrap().get(&sentinel) {
            route.prove(h.conn.stable_id());
        }
    }

    /// The client behind `sentinel` is no longer admitted (its session ended):
    /// no more punching on its connection until it proves the password again.
    pub fn revoke(&self, sentinel: SocketAddr) {
        let Some(route) = &self.route else { return };
        if let Some(h) = self.bindings.0.lock().unwrap().get(&sentinel) {
            route.revoke(h.conn.stable_id());
        }
    }
}

/// The server's two channels, handed out beside the [`QuicServer`] so the server
/// can own them separately (a type with `Drop` cannot give up a field).
pub struct QuicServerLink {
    /// Everything clients send, tagged with their sentinel - and, as the last
    /// thing from a connection, an EMPTY frame: that connection has closed, its
    /// session is over. On this channel and after the connection's last
    /// datagram, so the close can never overtake a frame that came before it (a
    /// second channel could: a select reads two ready channels in either order).
    /// Without it a session would linger until the server's timeout, holding a
    /// transmitter for up to three seconds (review 110).
    pub inbound_rx: mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)>,
    /// Packets for a client, by its sentinel. Unknown sentinels are dropped.
    pub uplink_tx: mpsc::UnboundedSender<(SocketAddr, Vec<u8>)>,
}

impl QuicServer {
    /// Listen on `bind`. Must be called inside a tokio runtime.
    pub fn start(bind: SocketAddr, congestion: Congestion) -> std::io::Result<(QuicServer, QuicServerLink)> {
        Self::start_with(bind, congestion, None)
    }

    /// Listen on `bind`, and with `relay` also through the relay (phase 3b): one
    /// endpoint for both, a relay client seen at [`relay_peer_for`] its id.
    pub fn start_with(
        bind: SocketAddr,
        congestion: Congestion,
        relay: Option<RelayRoute>,
    ) -> std::io::Result<(QuicServer, QuicServerLink)> {
        Self::start_on(std::net::UdpSocket::bind(bind)?, congestion, relay, Direct::Allowed)
    }

    /// [`QuicServer::start_with`] on a socket the caller has already bound - so
    /// the caller decides how a port that is not yet free is waited for, and
    /// what the socket's buffers and handle flags are (the server, build 133).
    ///
    /// `direct`: whether clients may also reach this server straight, on its own
    /// port, and punch to it (build 135). [`Direct::RelayOnly`] needs a relay;
    /// without one it would refuse everyone, so it is taken as `Allowed` and
    /// said in the log.
    pub fn start_on(
        socket: std::net::UdpSocket,
        congestion: Congestion,
        relay: Option<RelayRoute>,
        direct: Direct,
    ) -> std::io::Result<(QuicServer, QuicServerLink)> {
        let cert = rcgen::generate_simple_self_signed(vec!["thetislink".to_string()])
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().to_vec());
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
        let mut crypto = rustls::ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key.into())
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        crypto.alpn_protocols = vec![ALPN.to_vec()];
        let quic = quinn::crypto::rustls::QuicServerConfig::try_from(crypto)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let mut config = quinn::ServerConfig::with_crypto(Arc::new(quic));
        config.transport_config(transport(congestion, relay.is_some()));
        let mut route = None;
        let runtime = quinn::default_runtime().ok_or_else(|| std::io::Error::other("no async runtime"))?;
        let endpoint = match relay {
            // Without a relay, quinn's own socket handling, as before phase 3b
            // (what `Endpoint::server` does after binding).
            None => {
                if direct == Direct::RelayOnly {
                    warn!("QUIC: relay only was asked without a relay - direct connections stay allowed, or no one could connect");
                }
                Endpoint::new(quinn::EndpointConfig::default(), Some(config), socket, runtime)?
            }
            Some(relay) => {
                let direct_socket = runtime.wrap_udp_socket(socket)?;
                let socket = route::ServerRouteSocket::new(direct_socket, Some(relay), direct == Direct::RelayOnly);
                route = Some(socket.clone());
                Endpoint::new_with_abstract_socket(quinn::EndpointConfig::default(), Some(config), socket, runtime)?
            }
        };

        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
        let (uplink_tx, mut uplink_rx) = mpsc::unbounded_channel::<(SocketAddr, Vec<u8>)>();
        let conns: Arc<Mutex<HashMap<SocketAddr, Link>>> = Arc::default();
        let bindings = Bindings::default();
        let counter = Arc::new(AtomicU32::new(0));

        {
            let (endpoint, conns, bindings, route) = (endpoint.clone(), conns.clone(), bindings.clone(), route.clone());
            tokio::spawn(async move {
                while let Some(incoming) = endpoint.accept().await {
                    let (conns, bindings, inbound_tx, counter, route) =
                        (conns.clone(), bindings.clone(), inbound_tx.clone(), counter.clone(), route.clone());
                    tokio::spawn(async move {
                        let conn = match incoming.await {
                            Ok(c) => c,
                            Err(e) => {
                                debug!("QUIC: a handshake failed: {e}");
                                return;
                            }
                        };
                        let tag = sentinel(counter.fetch_add(1, Ordering::Relaxed));
                        let Some(binding) = binding_of(&conn) else {
                            warn!("QUIC: no binding for {} - connection refused", conn.remote_address());
                            conn.close(1u32.into(), b"no binding");
                            return;
                        };
                        info!("QUIC: connection from {} as {tag}", conn.remote_address());
                        conns.lock().unwrap().insert(tag, Link::new(conn.clone(), tag.to_string()));
                        let via_relay = relay_peer_id(conn.remote_address()).is_some();
                        bindings.0.lock().unwrap().insert(tag, Held { binding, conn: conn.clone(), via_relay });
                        let side = route.clone().map_or(punch::Side::Plain, punch::Side::Server);
                        let datagrams = spawn_readers(conn.clone(), tag, inbound_tx.clone(), side);
                        let why = conn.closed().await;
                        conns.lock().unwrap().remove(&tag);
                        bindings.0.lock().unwrap().remove(&tag);
                        if let Some(route) = &route {
                            route.forget(conn.stable_id());
                        }
                        info!("QUIC: connection {tag} closed ({why})");
                        // Only once every datagram it delivered is in the channel.
                        // A packet on a stream can still come after (a stream is
                        // read whole, in its own task); those are the large ones,
                        // not audio or PTT.
                        let _ = datagrams.await;
                        let _ = inbound_tx.send((tag, Vec::new()));
                    });
                }
            });
        }
        {
            let conns = conns.clone();
            tokio::spawn(async move {
                while let Some((tag, packet)) = uplink_rx.recv().await {
                    let link = conns.lock().unwrap().get(&tag).cloned();
                    if let Some(link) = link {
                        send_packet(&link, packet);
                    }
                }
            });
        }
        let (ways_report, mut stopped) = tokio::sync::oneshot::channel::<()>();
        {
            let ways = Ways { bindings: bindings.clone(), route: route.clone() };
            tokio::spawn(async move {
                let mut said: HashMap<SocketAddr, PathKind> = HashMap::new();
                let mut tick = tokio::time::interval(WAY_REPORT_EVERY);
                loop {
                    tokio::select! {
                        _ = &mut stopped => return,
                        _ = tick.tick() => {}
                    }
                    for (tag, kind) in ways_to_say(&ways.all(), &mut said) {
                        // Named in the brief before it was built (working rule 11, build 143).
                        info!("QUIC client {tag}: connection {}", kind.short());
                    }
                }
            });
        }
        Ok((QuicServer { bindings, endpoint, route, _ways_report: ways_report }, QuicServerLink { inbound_rx, uplink_tx }))
    }

    /// The client behind `sentinel` proved the password: its connection may
    /// punch a hole now (phase 3c). The one way an offer is ever answered - the
    /// server's session layer calls it at each admission.
    pub fn allow_punch(&self, sentinel: SocketAddr) {
        self.punch_gate().allow(sentinel)
    }

    /// [`QuicServer::allow_punch`] as a handle of its own, for whoever must
    /// call it without holding the server.
    pub fn punch_gate(&self) -> PunchGate {
        PunchGate { bindings: self.bindings.clone(), route: self.route.clone() }
    }

    /// Which way each client takes, as a handle of its own (build 143).
    pub fn ways(&self) -> Ways {
        Ways { bindings: self.bindings.clone(), route: self.route.clone() }
    }

    /// The UDP address the server listens on.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    /// Close the connection behind `sentinel`, if it still lives. Its close is
    /// then reported on `inbound_rx` like any other.
    pub fn close(&self, sentinel: SocketAddr) {
        if let Some(h) = self.bindings.0.lock().unwrap().get(&sentinel) {
            h.conn.close(0u32.into(), b"closed by the server");
        }
    }
}

impl Drop for QuicServer {
    fn drop(&mut self) {
        self.endpoint.close(0u32.into(), b"server stopped");
    }
}

// ------------------------------------------------------------------ client

/// How long a stopping client waits for its close to leave. Only at the end of
/// a session, never on a live path. Without it a client that stops exits before
/// the close is sent, and the server waits out `IDLE_TIMEOUT` instead (field
/// test, build 111: 30 s, to the millisecond).
pub const CLOSE_GRACE: Duration = Duration::from_millis(250);

/// Stops a [`QuicClient`] cleanly: the connection is closed, and `stop`
/// returns once the close has left (or after [`CLOSE_GRACE`]). What was queued
/// just before is handed over but may be dropped at the close - the close
/// itself is what the server is sure to hear. Kept apart from the client so it can be called while
/// another task waits on the client's inbound side.
#[derive(Clone)]
pub struct QuicStop {
    tx: mpsc::UnboundedSender<()>,
    done: watch::Receiver<bool>,
}

impl QuicStop {
    pub async fn stop(&self) {
        let _ = self.tx.send(());
        let mut done = self.done.clone();
        let _ = tokio::time::timeout(CLOSE_GRACE + Duration::from_millis(100), done.wait_for(|d| *d)).await;
    }
}

/// The client half: one connection at a time, to the address the engine sends to.
pub struct QuicClient {
    /// Packets to send, with the server address they are for. A new address
    /// makes a new connection (the old one is closed).
    pub uplink_tx: mpsc::UnboundedSender<(SocketAddr, Vec<u8>)>,
    /// What the server sends, tagged with the server address it was sent to.
    pub inbound_rx: mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)>,
    /// The binding of the live connection, `None` while there is none. A new
    /// connection has a new binding - an authentication made on the old one is
    /// worth nothing on it.
    pub binding: watch::Receiver<Option<Binding>>,
    /// To end the client with its close sent ([`QuicStop`]).
    pub stop: QuicStop,
    /// What `send_packet` did on the live connection (`None` before the
    /// first), so a test can see which path a packet took (review 123). Read
    /// by the tests only, for now.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) counts: watch::Receiver<Option<Arc<LinkCounts>>>,
    /// The relay leg's socket, so a test can steer the way as a server's
    /// signal would.
    #[cfg(test)]
    pub(crate) relay_socket: Option<Arc<route::ClientRelaySocket>>,
}

enum Event {
    Packet(SocketAddr, Vec<u8>),
    /// The address, the attempt's number, and its outcome.
    Connected(SocketAddr, u64, Result<Connection, String>, Instant),
    Closed(usize),
    /// The relay has a new session (build 178).
    RelaySession(u64),
}

/// Whether a new relay session should start the connection again: only while
/// an attempt is under way and nothing is connected. That attempt went out
/// over the route that died, and QUIC repeats its handshake with growing
/// pauses - on 2026-10-11 it waited 13 s and 5 s past the relay's return, and
/// once the full idle timeout (build 178).
fn restart_on_relay_session(live: bool, connecting: Option<SocketAddr>) -> Option<SocketAddr> {
    if live {
        None
    } else {
        connecting
    }
}

/// Whether the close of connection `closed` is the end of the live one. The
/// close of an older connection - the server's end, or the silence limit of one
/// the client already left - arrives at any later moment and must not clear the
/// live one (review 108; a function so it is tested in both states, review 109).
fn is_live(live: Option<usize>, closed: usize) -> bool {
    live == Some(closed)
}

impl QuicClient {
    /// Start the client task. Must be called inside a tokio runtime. Ends when
    /// `uplink_tx` is dropped.
    pub fn start(congestion: Congestion) -> std::io::Result<QuicClient> {
        Self::start_with(congestion, None)
    }

    /// As [`QuicClient::start`], and with `relay` every packet goes through the
    /// relay (phase 3b) - whatever address the engine sends to.
    pub fn start_with(congestion: Congestion, relay: Option<RelayRoute>) -> std::io::Result<QuicClient> {
        let mut crypto = rustls::ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoIssuerCheck(provider())))
            .with_no_client_auth();
        crypto.alpn_protocols = vec![ALPN.to_vec()];
        let quic = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let mut config = quinn::ClientConfig::new(Arc::new(quic));
        config.transport_config(transport(congestion, relay.is_some()));
        let mut route = None;
        let mut endpoint = match relay {
            // IPv4 only, as the direct mode everywhere else (decision 6, 05-10).
            None => Endpoint::client(SocketAddr::from(([0, 0, 0, 0], 0)))?,
            Some(relay) => {
                let runtime = quinn::default_runtime().ok_or_else(|| std::io::Error::other("no async runtime"))?;
                let socket = route::ClientRelaySocket::start(relay);
                route = Some(socket.clone());
                Endpoint::new_with_abstract_socket(quinn::EndpointConfig::default(), None, socket, runtime)?
            }
        };
        endpoint.set_default_client_config(config);

        let (uplink_tx, mut uplink_rx) = mpsc::unbounded_channel::<(SocketAddr, Vec<u8>)>();
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
        let (binding_tx, binding_rx) = watch::channel(None);
        let (counts_tx, counts_rx) = watch::channel(None);
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Event>();
        let (stop_tx, mut stop_rx) = mpsc::unbounded_channel::<()>();
        let (done_tx, done_rx) = watch::channel(false);
        #[cfg(test)]
        let relay_socket = route.clone();

        let mut relay_sessions = route.as_ref().map(|r| r.new_sessions());
        tokio::spawn(async move {
            let mut live: Option<(SocketAddr, Link)> = None;
            let mut connecting: Option<(SocketAddr, u64)> = None;
            let mut attempts: u64 = 0;
            let mut backlog: VecDeque<Vec<u8>> = VecDeque::new();
            let connect = |addr: SocketAddr, attempt: u64| {
                let (endpoint, event_tx) = (endpoint.clone(), event_tx.clone());
                let t0 = Instant::now();
                tokio::spawn(async move {
                    let result = match endpoint.connect(addr, "thetislink") {
                        Ok(c) => c.await.map_err(|e| e.to_string()),
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = event_tx.send(Event::Connected(addr, attempt, result, t0));
                });
            };
            loop {
                let ev = tokio::select! {
                    p = uplink_rx.recv() => match p {
                        Some((addr, packet)) => Event::Packet(addr, packet),
                        None => break,
                    },
                    Some(e) = event_rx.recv() => e,
                    r = async {
                        match relay_sessions.as_mut() {
                            Some(rx) => rx.changed().await.map(|()| *rx.borrow_and_update()),
                            None => std::future::pending().await,
                        }
                    } => match r {
                        Ok(session) => Event::RelaySession(session),
                        Err(_) => {
                            relay_sessions = None;
                            continue;
                        }
                    },
                    Some(()) = stop_rx.recv() => {
                        // What the engine queued before it stopped - its own
                        // goodbye among it - is handed to the live connection.
                        // Quinn may still drop it at the close below; the close
                        // stands for it on the server.
                        while let Ok((addr, packet)) = uplink_rx.try_recv() {
                            if let Some((target, link)) = &live {
                                if *target == addr {
                                    send_packet(link, packet);
                                }
                            }
                        }
                        break;
                    }
                };
                match ev {
                    Event::Packet(addr, packet) => {
                        if let Some((target, link)) = &live {
                            if *target == addr {
                                send_packet(link, packet);
                                continue;
                            }
                        }
                        // The engine left the old server: close it now, not only
                        // once the new connection stands - otherwise a failing
                        // new one keeps the old one alive on keepalives (review 108).
                        if let Some((_, old)) = live.take() {
                            old.conn.close(0u32.into(), b"new server");
                            let _ = binding_tx.send(None);
                        }
                        if connecting.map(|(a, _)| a) != Some(addr) {
                            // A new address: whatever was queued was for another server.
                            backlog.clear();
                            attempts += 1;
                            connecting = Some((addr, attempts));
                            // And a punch made for the old connection leads to
                            // a socket of the old one: the new one goes through
                            // the relay until it has punched for itself.
                            if let Some(route) = &route {
                                route.restart_punch();
                            }
                            connect(addr, attempts);
                        }
                        queue(&mut backlog, packet);
                    }
                    Event::RelaySession(session) => {
                        if let Some(addr) = restart_on_relay_session(live.is_some(), connecting.map(|(a, _)| a)) {
                            // Named in advance (working rule 11, build 178).
                            info!("QUIC: relay session {session} while connecting to {addr} - starting the connection again");
                            attempts += 1;
                            connecting = Some((addr, attempts));
                            connect(addr, attempts);
                        }
                    }
                    Event::Connected(addr, attempt, result, t0) => {
                        if connecting != Some((addr, attempt)) {
                            // Superseded by a newer address while it was being made.
                            if let Ok(conn) = result {
                                conn.close(0u32.into(), b"superseded");
                            }
                            continue;
                        }
                        connecting = None;
                        let conn = match result {
                            Ok(c) => c,
                            Err(e) => {
                                warn!("QUIC: connecting to {addr} failed: {e}");
                                backlog.clear();
                                continue;
                            }
                        };
                        let Some(binding) = binding_of(&conn) else {
                            warn!("QUIC: no binding on the connection to {addr} - closed");
                            conn.close(1u32.into(), b"no binding");
                            backlog.clear();
                            continue;
                        };
                        info!(
                            "QUIC path up after {} ms (rtt {} ms)",
                            t0.elapsed().as_millis(),
                            conn.rtt().as_millis()
                        );
                        // The binding first: a frame the readers deliver is answered
                        // with it, so it must be there before the first one can come
                        // (review 110 - it held by timing and a safety net, now by order).
                        let _ = binding_tx.send(Some(binding));
                        let side = route.clone().map_or(punch::Side::Plain, punch::Side::Client);
                        spawn_readers(conn.clone(), addr, inbound_tx.clone(), side);
                        if let Some(route) = &route {
                            tokio::spawn(punch::client_offers(conn.clone(), route.clone()));
                        }
                        {
                            let (conn, event_tx) = (conn.clone(), event_tx.clone());
                            tokio::spawn(async move {
                                let why = conn.closed().await;
                                info!("QUIC path lost ({why})");
                                let _ = event_tx.send(Event::Closed(conn.stable_id()));
                            });
                        }
                        let link = Link::new(conn, "to the server".to_string());
                        let _ = counts_tx.send(Some(link.counts.clone()));
                        for packet in backlog.drain(..) {
                            send_packet(&link, packet);
                        }
                        live = Some((addr, link));
                    }
                    Event::Closed(id) => {
                        // Only the live connection's own end counts: the close of
                        // an older one arrives late and must not clear the new.
                        if is_live(live.as_ref().map(|(_, l)| l.conn.stable_id()), id) {
                            live = None;
                            let _ = binding_tx.send(None);
                        }
                    }
                }
            }
            if let Some((_, link)) = live {
                link.conn.close(0u32.into(), b"client stopped");
            }
            let _ = binding_tx.send(None);
            // Let the close leave before this task - and with it, often, the
            // whole runtime - is gone. What this wait carries for certain is
            // the order: `done` is sent after it, so `stop()` returning means
            // the close has left - the engine drops its runtime right after.
            // Measured once too (review 112): three runs of twenty stop cycles,
            // each on a runtime dropped at once, beside six busy loops and disk
            // traffic - 3 of 60 closes too late without this wait, 0 of 20 with it, none
            // on an idle machine; with eight busy loops alone 0 of 48 (review
            // 113). No test shows it reliably, so this note is what stands
            // between it and a clean-up.
            let _ = tokio::time::timeout(CLOSE_GRACE, endpoint.wait_idle()).await;
            let _ = done_tx.send(true);
        });
        Ok(QuicClient {
            uplink_tx,
            inbound_rx,
            binding: binding_rx,
            stop: QuicStop { tx: stop_tx, done: done_rx },
            counts: counts_rx,
            #[cfg(test)]
            relay_socket,
        })
    }
}

/// Accepts any server certificate - there is no issuer to check it against.
/// The signature check below is the standard TLS one and proves nothing about
/// WHO the server is: a party in the middle with its own certificate and key
/// passes it too. The only thing that does is the password proof bound to this
/// session's [`Binding`] (OPZET §3; review 108 pointed out the old wording).
#[derive(Debug)]
struct NoIssuerCheck(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for NoIssuerCheck {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        // TLS 1.2 is not offered (QUIC is TLS 1.3 only).
        Err(rustls::Error::General("TLS 1.2 is not used".into()))
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// A controller that never brakes: a fixed, large window. Only for measuring
/// ([`Congestion::NoBrake`]); a real sender should not do this on a congested link.
struct NoBrake;
const NO_BRAKE_WINDOW: u64 = 4 * 1024 * 1024;

impl ControllerFactory for NoBrake {
    fn build(self: Arc<Self>, _now: Instant, _mtu: u16) -> Box<dyn Controller> {
        Box::new(NoBrakeController)
    }
}

#[derive(Clone)]
struct NoBrakeController;

impl Controller for NoBrakeController {
    fn on_congestion_event(&mut self, _: Instant, _: Instant, _: bool, _: u64) {}
    fn on_mtu_update(&mut self, _: u16) {}
    fn window(&self) -> u64 {
        NO_BRAKE_WINDOW
    }
    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }
    fn initial_window(&self) -> u64 {
        NO_BRAKE_WINDOW
    }
    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}

#[cfg(test)]
mod snapshot {
    use super::Snapshot;

    /// The line counts the window, not the totals, and what was handed over but
    /// not sent is the queue's drop.
    #[test]
    fn a_window_is_the_difference_and_the_unsent_are_dropped() {
        let prev = Snapshot { offered: 100, refused: 1, streams: 10, streams_no_slot: 2, streams_failed: 1, closing: 0, datagrams_sent: 100, datagrams_received: 40, packets_sent: 150, packets_lost: 3, congestion_events: 1, ..Snapshot::default() };
        let now = Snapshot {
            offered: 600,
            refused: 3,
            streams: 70,
            streams_no_slot: 6,
            streams_failed: 1,
            closing: 2,
            datagrams_sent: 520,
            datagrams_received: 90,
            packets_sent: 700,
            packets_lost: 33,
            congestion_events: 4,
            rtt_ms: 31,
            window: 12_000,
            mtu: 1400,
        };
        assert_eq!(
            now.since(&prev),
            "datagrams offered 500, sent 420, dropped from the queue 80, refused 2, received 50; large packets on streams 60, dropped (no slot) 4, failed 0, connection closing 2; packets sent 550, lost 30; congestion events 3; rtt 31 ms, window 12000 B, mtu 1400"
        );
        // A datagram sent in this window that was handed over in the last: no negative drop.
        let later = Snapshot { offered: 600, datagrams_sent: 530, ..now };
        assert!(later.since(&now).contains("dropped from the queue 0"), "{}", later.since(&now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAIT: Duration = Duration::from_secs(5);

    /// The log task's decision (build 144, review 143): a client's way is said
    /// when first seen and at each change, not at every look; one that is gone
    /// is forgotten, and said again if it comes back.
    #[test]
    fn a_client_s_way_is_said_once_per_change() {
        let (a, b): (SocketAddr, SocketAddr) = ("198.51.100.1:1".parse().unwrap(), "198.51.100.1:2".parse().unwrap());
        let mut said = HashMap::new();
        assert_eq!(ways_to_say(&[(a, PathKind::Relay)], &mut said), vec![(a, PathKind::Relay)], "first seen: said");
        assert_eq!(ways_to_say(&[(a, PathKind::Relay)], &mut said), vec![], "unchanged: quiet");
        assert_eq!(
            ways_to_say(&[(a, PathKind::LocalViaRelay), (b, PathKind::Relay)], &mut said),
            vec![(a, PathKind::LocalViaRelay), (b, PathKind::Relay)],
            "changed, and a new one: both said"
        );
        assert_eq!(ways_to_say(&[(b, PathKind::Relay)], &mut said), vec![], "one gone: nothing to say");
        assert_eq!(said.len(), 1, "and forgotten");
        assert_eq!(ways_to_say(&[(a, PathKind::LocalViaRelay), (b, PathKind::Relay)], &mut said), vec![(a, PathKind::LocalViaRelay)], "back: said again");
    }

    /// The server's name for a client's way (build 143), one row per branch:
    /// at its relay address R, or D+R once sent to directly only; came in
    /// through the relay and moved, L+R at a local address; came straight, L
    /// or D by its address.
    #[test]
    fn a_client_s_way_by_where_it_is_and_where_it_came_in() {
        let at = |s: &str| -> SocketAddr { s.parse().unwrap() };
        let relay_2 = relay_peer_for(2);
        let no = |_: u8| false;
        assert_eq!(way_of(relay_2, true, no, no), PathKind::Relay);
        assert_eq!(way_of(relay_2, true, |id| id == 2, no), PathKind::DirectViaRelay);
        assert_eq!(way_of(relay_2, true, |id| id == 3, no), PathKind::Relay, "another client's punch");
        assert_eq!(way_of(relay_2, true, no, |id| id == 2), PathKind::RelayTcp, "heard over TCP only");
        assert_eq!(way_of(relay_2, true, no, |id| id == 3), PathKind::Relay, "another client's TCP");
        assert_eq!(way_of(relay_2, true, |id| id == 2, |id| id == 2), PathKind::DirectViaRelay, "a confirmed punch first");
        assert_eq!(way_of(at("192.168.1.50:50000"), true, |_| true, no), PathKind::LocalViaRelay);
        assert_eq!(way_of(at("203.0.113.97:50000"), true, |_| true, no), PathKind::DirectViaRelay);
        assert_eq!(way_of(at("192.168.1.50:50000"), false, |_| true, no), PathKind::Local);
        assert_eq!(way_of(at("203.0.113.97:50000"), false, |_| true, no), PathKind::Direct);
    }

    /// A server that sends every packet straight back to whoever sent it.
    fn echo_server(congestion: Congestion) -> (SocketAddr, Bindings, mpsc::UnboundedReceiver<SocketAddr>) {
        let (server, mut link) = QuicServer::start(SocketAddr::from(([127, 0, 0, 1], 0)), congestion).unwrap();
        let addr = server.local_addr().unwrap();
        let bindings = server.bindings.clone();
        let (seen_tx, seen_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let _server = server; // listens as long as this task runs
            let mut seen = std::collections::HashSet::new();
            while let Some((tag, packet)) = link.inbound_rx.recv().await {
                if seen.insert(tag) {
                    let _ = seen_tx.send(tag);
                }
                let _ = link.uplink_tx.send((tag, packet));
            }
        });
        (addr, bindings, seen_rx)
    }

    /// Send `packet` to `to` and wait for it to come back, tagged with `to`. A
    /// failure says how long it waited and for what (review 109: an unexplained
    /// failure must be readable the next time).
    async fn round_trip(client: &mut QuicClient, to: SocketAddr, packet: Vec<u8>) {
        client.uplink_tx.send((to, packet.clone())).unwrap();
        let t0 = Instant::now();
        let (from, back) = tokio::time::timeout(WAIT, client.inbound_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("no echo of a {} B packet after {:?} (to {to})", packet.len(), t0.elapsed()))
            .unwrap();
        assert_eq!(from, to, "tagged with the address it was sent to");
        assert_eq!(back, packet);
    }

    async fn binding_now(client: &mut QuicClient) -> Binding {
        tokio::time::timeout(WAIT, client.binding.wait_for(|b| b.is_some())).await.unwrap().unwrap().unwrap()
    }

    /// A small packet goes and comes back - the very first one too, though it
    /// was sent before there was a connection (the backlog). Twice, without a
    /// restart (working rule 9).
    #[tokio::test]
    async fn a_packet_goes_and_comes_back() {
        let (server, _, _) = echo_server(Congestion::Bbr);
        let mut client = QuicClient::start(Congestion::Bbr).unwrap();
        round_trip(&mut client, server, b"first, before the connection".to_vec()).await;
        round_trip(&mut client, server, b"second".to_vec()).await;
        // A small packet is a datagram, and counted as one (review 123).
        let counts = client.counts.borrow().clone().expect("a live connection");
        assert!(counts.offered.load(Ordering::Relaxed) >= 2, "the datagrams were not counted");
        assert_eq!(counts.streams.load(Ordering::Relaxed), 0, "a small packet took a stream");
    }

    /// A packet too large for one datagram travels on a stream, both ways, intact.
    #[tokio::test]
    async fn a_large_packet_travels_on_a_stream() {
        let (server, _, _) = echo_server(Congestion::Bbr);
        let mut client = QuicClient::start(Congestion::Bbr).unwrap();
        round_trip(&mut client, server, b"open".to_vec()).await;
        let big: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        round_trip(&mut client, server, big).await;
        // ... and the large one is counted on the stream path (review 123).
        let counts = client.counts.borrow().clone().expect("a live connection");
        assert_eq!(counts.streams.load(Ordering::Relaxed), 1, "the large packet was not counted as a stream");
        assert_eq!(counts.streams_no_slot.load(Ordering::Relaxed), 0);
    }

    /// Both ends derive the same binding, a second connection a different one,
    /// and the server forgets a binding when its connection is gone.
    #[tokio::test]
    async fn both_ends_hold_the_same_binding_and_each_connection_its_own() {
        let (server, bindings, mut seen) = echo_server(Congestion::Bbr);
        let mut a = QuicClient::start(Congestion::Bbr).unwrap();
        let mut b = QuicClient::start(Congestion::Bbr).unwrap();
        round_trip(&mut a, server, b"a".to_vec()).await;
        round_trip(&mut b, server, b"b".to_vec()).await;
        let tag_a = seen.recv().await.unwrap();
        let tag_b = seen.recv().await.unwrap();
        assert_ne!(tag_a, tag_b, "two clients, two sentinels");
        assert!(is_quic_sentinel(tag_a) && is_quic_sentinel(tag_b));
        let (ba, bb) = (binding_now(&mut a).await, binding_now(&mut b).await);
        assert_ne!(ba, bb, "two connections, two bindings");
        assert_eq!(bindings.get(tag_a), Some(ba), "client a and the server agree");
        assert_eq!(bindings.peer(tag_a).map(|p| p.ip()), Some("127.0.0.1".parse().unwrap()), "the real peer, not the sentinel");
        assert_eq!(bindings.get(tag_b), Some(bb), "client b and the server agree");
        drop(a);
        let t0 = Instant::now();
        while bindings.get(tag_a).is_some() {
            assert!(t0.elapsed() < WAIT, "the server forgets a closed connection's binding");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(bindings.get(tag_b), Some(bb), "and only that one");
    }

    /// Another server address closes the old connection and makes a new one;
    /// back to the first makes yet another, with a new binding - a session never
    /// carries over (working rule 9: the second pass).
    #[tokio::test]
    async fn a_new_address_is_a_new_connection_with_a_new_binding() {
        let (one, _, _) = echo_server(Congestion::Bbr);
        let (two, _, _) = echo_server(Congestion::Bbr);
        let mut client = QuicClient::start(Congestion::Bbr).unwrap();
        round_trip(&mut client, one, b"to one".to_vec()).await;
        let first = binding_now(&mut client).await;
        round_trip(&mut client, two, b"to two".to_vec()).await;
        round_trip(&mut client, one, b"one again".to_vec()).await;
        let again = binding_now(&mut client).await;
        assert_ne!(first, again, "a new connection to the same server is a new binding");
    }

    /// A new relay session starts the connection again only while an attempt
    /// is under way and nothing is connected; and the loop is wired to it, a
    /// later outcome of the old attempt counting as superseded (build 178; a
    /// text guard for the wiring: the loop runs in no test with a dead route).
    #[test]
    fn a_new_relay_session_starts_a_hanging_attempt_again() {
        let addr: SocketAddr = "192.0.2.10:4580".parse().unwrap();
        assert_eq!(restart_on_relay_session(false, Some(addr)), Some(addr), "an attempt under way: again");
        assert_eq!(restart_on_relay_session(true, Some(addr)), None, "connected: leave it");
        assert_eq!(restart_on_relay_session(false, None), None, "nothing to start again");
        let code = include_str!("lib.rs");
        let code = &code[..code.find("mod tests {").unwrap()];
        assert!(code.contains("Ok(session) => Event::RelaySession(session),"));
        // The arm that waits for the session runs, not a pending (review of 178, Q3).
        assert_eq!(code.matches("Some(rx) => rx.changed().await.map(|()| *rx.borrow_and_update()),").count(), 1);
        assert!(code.contains("if let Some(addr) = restart_on_relay_session(live.is_some(), connecting.map(|(a, _)| a)) {"));
        assert!(code.contains("attempts += 1;\n                            connecting = Some((addr, attempts));\n                            connect(addr, attempts);\n                        }\n                    }\n                    Event::Connected(addr, attempt, result, t0) => {\n                        if connecting != Some((addr, attempt)) {"));
        let route = include_str!("route.rs");
        assert_eq!(route.matches("leg.new_session.send_replace(g.session);").count(), 1, "every new relay socket says so");
        // In the branch that made the socket, not where it failed (review of 178, Q4).
        assert!(route.contains("punch: ClientPunch::default(),\n                        });\n                        leg.new_session.send_replace(g.session);"));
    }

    /// Only the live connection's own close ends it, in both states.
    #[test]
    fn only_the_live_connections_close_ends_it() {
        assert!(is_live(Some(7), 7), "the live one closed");
        assert!(!is_live(Some(7), 3), "an older one closed late");
        assert!(!is_live(None, 3), "nothing live");
    }

    /// Sending to another address closes the old connection at once, even when
    /// the new one never comes (review 108: a failing new connection kept the
    /// old one alive on keepalives).
    #[tokio::test]
    async fn leaving_a_server_closes_its_connection_even_if_the_next_fails() {
        let (one, bindings, mut seen) = echo_server(Congestion::Bbr);
        let mut client = QuicClient::start(Congestion::Bbr).unwrap();
        round_trip(&mut client, one, b"to one".to_vec()).await;
        let tag = seen.recv().await.unwrap();
        assert!(bindings.get(tag).is_some());
        // Nobody listens here: this connection is never made.
        let nowhere = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        client.uplink_tx.send((nowhere, b"to nowhere".to_vec())).unwrap();
        let t0 = Instant::now();
        while bindings.get(tag).is_some() {
            assert!(t0.elapsed() < Duration::from_secs(2), "the old connection is closed, not kept for the next");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(*client.binding.borrow(), None, "and the client says it has none");
    }

    /// A closed connection is reported by its sentinel, as an empty frame on
    /// the inbound channel, so the server can end its session at once - and
    /// nothing from that connection comes after it (review 110). The client's
    /// own empty datagram is not passed on: it would read as a close.
    #[tokio::test]
    async fn a_closed_connection_is_reported_after_its_frames() {
        let (server, mut link) = QuicServer::start(SocketAddr::from(([127, 0, 0, 1], 0)), Congestion::Bbr).unwrap();
        let addr = server.local_addr().unwrap();
        let client = QuicClient::start(Congestion::Bbr).unwrap();
        client.uplink_tx.send((addr, b"hello".to_vec())).unwrap();
        let (tag, first) = tokio::time::timeout(WAIT, link.inbound_rx.recv()).await.unwrap().unwrap();
        assert_eq!(first, b"hello");
        client.uplink_tx.send((addr, Vec::new())).unwrap();
        for i in 0..20u8 {
            client.uplink_tx.send((addr, vec![i; 8])).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(client);
        let mut frames = 0;
        loop {
            let (from, data) = tokio::time::timeout(WAIT, link.inbound_rx.recv()).await.expect("a close in time").unwrap();
            assert_eq!(from, tag);
            if data.is_empty() {
                break;
            }
            frames += 1;
        }
        assert_eq!(frames, 20, "every frame before the close, the empty one not among them");
        let after = tokio::time::timeout(Duration::from_millis(300), link.inbound_rx.recv()).await;
        assert!(after.is_err(), "nothing after the close: {after:?}");
        drop(server);
    }

    /// The close comes after every frame of its connection also when the
    /// connection goes in the middle of a burst - on a runtime with several
    /// workers, as the server's. The close and the frames come from two tasks
    /// (the connection's and its datagram reader) on one channel, and a channel
    /// keeps the order per sender, not between senders: without the wait for the
    /// reader, frames land after the close here (review 111 measured 5 of 10
    /// cycles; on one thread, never). How many frames arrive is not fixed; that
    /// none comes after the close is.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn no_frame_comes_after_the_close_of_its_connection() {
        let (server, mut link) = QuicServer::start(SocketAddr::from(([127, 0, 0, 1], 0)), Congestion::Bbr).unwrap();
        let addr = server.local_addr().unwrap();
        for cycle in 0..10 {
            let client = QuicClient::start(Congestion::Bbr).unwrap();
            client.uplink_tx.send((addr, b"hello".to_vec())).unwrap();
            let (tag, _) = tokio::time::timeout(WAIT, link.inbound_rx.recv()).await.unwrap().unwrap();
            for i in 0..200u32 {
                client.uplink_tx.send((addr, i.to_le_bytes().repeat(2))).unwrap();
            }
            drop(client);
            loop {
                let (from, data) = tokio::time::timeout(WAIT, link.inbound_rx.recv()).await.expect("a close in time").unwrap();
                assert_eq!(from, tag, "cycle {cycle}");
                if data.is_empty() {
                    break;
                }
            }
            let after = tokio::time::timeout(Duration::from_millis(200), link.inbound_rx.recv()).await;
            assert!(after.is_err(), "cycle {cycle}: a frame after the close: {after:?}");
        }
        drop(server);
    }

    /// A client that stops - and whose runtime goes right after, as an app's
    /// does - gets its close to the server: the server hears it at once, not
    /// after `IDLE_TIMEOUT` (field test, build 111). A datagram queued just
    /// before the close does NOT arrive - quinn drops what is unsent at a close -
    /// so the client's own goodbye may be lost; the close stands for it on the
    /// server (it reads as that client's Disconnect).
    #[tokio::test]
    async fn a_stopping_client_gets_its_close_out() {
        let (server, mut link) = QuicServer::start(SocketAddr::from(([127, 0, 0, 1], 0)), Congestion::Bbr).unwrap();
        let addr = server.local_addr().unwrap();
        let (connected_tx, connected_rx) = tokio::sync::oneshot::channel::<()>();
        let client_thread = std::thread::spawn(move || {
            // One thread, on purpose: with a second worker the client task could
            // still close the connection after `block_on` returned, and a stop
            // signal that never arrives would pass (review 114: 1 of 5 green
            // on two workers; a stop arm that does not break stays red).
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let client = QuicClient::start(Congestion::Bbr).unwrap();
                client.uplink_tx.send((addr, b"hello".to_vec())).unwrap();
                connected_rx.await.unwrap();
                client.stop.stop().await;
            });
            // The runtime goes here, as the app's does when its engine ends.
        });
        let (tag, first) = tokio::time::timeout(WAIT, link.inbound_rx.recv()).await.unwrap().unwrap();
        assert_eq!(first, b"hello");
        connected_tx.send(()).unwrap();
        let (from, close) = tokio::time::timeout(Duration::from_secs(3), link.inbound_rx.recv())
            .await
            .expect("the close at once, not after the idle timeout")
            .unwrap();
        assert_eq!((from, close.is_empty()), (tag, true));
        client_thread.join().unwrap();
        drop(server);
    }

    /// Through the relay quinn searches no higher than RELAY_MTU_CEILING, so its
    /// header fits; direct, quinn's own ceiling stays (1452). Read from quinn's
    /// own description of the settings - it has no getter.
    #[test]
    fn the_relay_path_has_a_lower_mtu_ceiling() {
        let via = format!("{:?}", transport(Congestion::Bbr, true));
        let direct = format!("{:?}", transport(Congestion::Bbr, false));
        assert!(via.contains(&format!("upper_bound: {RELAY_MTU_CEILING}")), "{via}");
        assert!(direct.contains("upper_bound: 1452"), "{direct}");
    }

    /// The backlog while connecting: bounded, and the oldest goes first.
    #[test]
    fn the_backlog_keeps_the_newest_packets() {
        let mut b = VecDeque::new();
        for i in 0..(CONNECT_BACKLOG as u32 + 5) {
            queue(&mut b, i.to_le_bytes().to_vec());
        }
        assert_eq!(b.len(), CONNECT_BACKLOG);
        assert_eq!(b.front().unwrap(), &5u32.to_le_bytes().to_vec(), "the five oldest are gone");
        assert_eq!(b.back().unwrap(), &(CONNECT_BACKLOG as u32 + 4).to_le_bytes().to_vec());
    }

    /// The signature check in the verifier is the standard one; it proves no
    /// identity, but it keeps the handshake honest and costs nothing. A text pin
    /// (review 108: no test saw it go).
    #[test]
    fn the_verifier_still_checks_the_signature() {
        let src = include_str!("lib.rs");
        // Split, so this line does not count itself.
        let call = concat!("rustls::crypto::", "verify_tls13_signature(message, cert, dss");
        assert_eq!(src.matches(call).count(), 1);
    }

    /// The measuring controller works end to end too (both settings, rule 13).
    #[tokio::test]
    async fn the_no_brake_setting_carries_packets_too() {
        let (server, _, _) = echo_server(Congestion::NoBrake);
        let mut client = QuicClient::start(Congestion::NoBrake).unwrap();
        round_trip(&mut client, server, b"no brake".to_vec()).await;
        let big = vec![7u8; 9_000];
        round_trip(&mut client, server, big).await;
    }

    /// Sentinels: in their own range, distinct from the relay's, and not handed
    /// out twice within a long run.
    #[test]
    fn sentinels_are_their_own_range_and_do_not_repeat() {
        assert!(is_quic_sentinel(sentinel(0)));
        assert!(!is_quic_sentinel("203.0.113.1:1".parse().unwrap()), "the relay's range is not ours");
        assert!(!is_quic_sentinel("192.168.1.97:4580".parse().unwrap()));
        let mut seen = std::collections::HashSet::new();
        for n in (0..300_000u32).step_by(7) {
            assert!(seen.insert(sentinel(n)), "sentinel {n} repeated");
            assert_ne!(sentinel(n).port(), 0);
        }
        assert_eq!(sentinel(65535).ip(), "198.51.100.2".parse::<IpAddr>().unwrap(), "the host moves on after the ports");
    }
}
