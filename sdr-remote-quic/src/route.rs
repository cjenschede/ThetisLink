// SPDX-License-Identifier: GPL-2.0-or-later

//! The route layer under quinn (phase 3b, `OPZET-fase3b-quic-via-relay.md`).
//!
//! quinn runs on a socket of our own ([`quinn::AsyncUdpSocket`]), so one QUIC
//! connection can travel through the relay without quinn knowing. Each packet
//! that goes via the relay is framed with the relay's TLU1 header (the encoder of
//! `sdr-remote-relay`, one for both data forms); the relay forwards the payload
//! unread and the other side receives `[client_id] + payload`.
//!
//! Two rules from quinn decide the shape (both read in quinn-proto 0.11.14):
//! - a client drops every packet that does not come from exactly the address it
//!   connected to (`remote_may_migrate` is false for a client), so the client
//!   reports everything from the relay as coming from the address quinn sends to;
//! - a server follows a known connection to a new address only after a path
//!   check, so the server gives each relay client one fixed address per client
//!   id - [`relay_peer_for`], in 192.0.2.0/24 (TEST-NET-1, next to the QUIC
//!   sentinels in TEST-NET-2 and the old relay's in TEST-NET-3).
//!
//! The relay binds a token to the first source it sees, so the datagrams of one
//! side all leave from one socket - this one; the relay library opens none of its
//! own in this mode. A new relay session (after a network change: a new token)
//! gets a new socket, as the third experiment found a new client socket was
//! needed after a switch. What the relay library hands over for that is a
//! [`UdpGrant`] (its type, one for both crates).

use std::collections::{HashMap, HashSet};
use sdr_remote_core::dev_info;
use std::fmt;
use std::io::{self, IoSliceMut};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use log::{debug, info, warn};
use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use tokio::io::ReadBuf;
use tokio::net::UdpSocket;
use tokio::sync::watch;

pub use sdr_remote_relay::{TcpLine, UdpGrant};
use sdr_remote_relay::{encode_tlu1_raw, tlt1_frame, TLU1_HEADER_LEN, UDP_MAPPING_KEEPALIVE_ID};

use crate::punch::{
    offer_interval, usable_target, DIRECT_SILENCE, GIVE_UP, MAX_OFFERS, OFFER_EVERY_SLOW, PUNCH_EVERY, PUNCH_PROBE,
    PUNCH_PROBES, RELAY_QUIET,
};

/// What a side needs to send via the relay: the grant of the current relay
/// session, as the relay library publishes it (`None` while there is none).
#[derive(Clone, Debug)]
pub struct RelayRoute {
    pub grant: watch::Receiver<Option<UdpGrant>>,
    /// "Relay only" (build 137, the measurement screen): while set, a client
    /// offers no direct path, and one that runs direct goes back to the relay.
    /// Shared, so the switch acts on the running connection. A server's route
    /// ignores it (its own form is `Direct::RelayOnly`).
    pub relay_only: Arc<std::sync::atomic::AtomicBool>,
    /// "Force TCP" (build 146, the measurement screen): while set, a client
    /// sends everything over the relay's TCP line and nothing over UDP, as on a
    /// network that lets no UDP through. It holds "relay only" in it: no punch.
    pub force_tcp: Arc<std::sync::atomic::AtomicBool>,
    /// Which way a client's traffic takes now (build 139, the connection
    /// names): written by its relay leg, read by the engine. A server's route
    /// leaves it alone.
    path: Arc<std::sync::atomic::AtomicU8>,
    /// The relay's TCP line (the TCP fallback route, build 144), taken by the
    /// relay leg that starts first; a clone shares it and takes nothing.
    tcp: Arc<Mutex<Option<TcpLine>>>,
}

/// The way a client through the relay sends now: all through the relay, a
/// direct path being tried, the punched path confirmed, or the server's local
/// address confirmed (L+R, build 142).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientPath {
    Relay,
    Coupling,
    Direct,
    Local,
    /// Everything through the relay over its TCP line (build 146).
    RelayTcp,
}

impl ClientPath {
    fn from_u8(v: u8) -> ClientPath {
        match v {
            1 => ClientPath::Coupling,
            2 => ClientPath::Direct,
            3 => ClientPath::Local,
            4 => ClientPath::RelayTcp,
            _ => ClientPath::Relay,
        }
    }
}

impl RelayRoute {
    /// A route through the relay, with "relay only" off, and the relay's TCP
    /// line for a network that lets no UDP through (`None`: UDP only).
    pub fn new(grant: watch::Receiver<Option<UdpGrant>>, tcp: Option<TcpLine>) -> RelayRoute {
        RelayRoute { grant, relay_only: Arc::default(), force_tcp: Arc::default(), path: Arc::default(), tcp: Arc::new(Mutex::new(tcp)) }
    }

    /// The way the client's traffic takes now, as its relay leg last said:
    /// at every offer tick (250 ms) and at once when a direct path is
    /// confirmed or tried. `Relay` before the first tick. `Local` is a
    /// confirmed path to the server's local address (L+R), `Direct` a punched one.
    pub fn path(&self) -> ClientPath {
        ClientPath::from_u8(self.path.load(Ordering::Relaxed))
    }
}

/// The largest UDP payload quinn may send on a path through the relay: its
/// 43-byte header on top, and 28 of IPv4 and UDP, stay below 1500. quinn starts
/// at 1200 and searches up to this.
pub const RELAY_MTU_CEILING: u16 = 1400;

// The ceiling, the relay's header, and 28 bytes of IPv4 and UDP fit a 1500-byte
// path - checked by the compiler, so a longer header cannot slip past it.
const _: () = assert!(RELAY_MTU_CEILING as usize + TLU1_HEADER_LEN + 28 <= 1500);

/// How often a side sends an empty datagram to hold its NAT mapping and its
/// source binding at the relay - as the relay library does (`UDP_BEACON_INTERVAL`).
const MAPPING_KEEPALIVE: Duration = Duration::from_secs(1);

/// Room for one datagram from the relay: the relay's own receive buffer is 2048.
const RELAY_DATAGRAM_MAX: usize = 2048;

const RELAY_PEER_NET: [u8; 3] = [192, 0, 2];
const RELAY_PEER_PORT: u16 = 1;

/// The fixed address quinn sees for relay client `client_id` on the server.
/// Every id maps and maps back; the relay hands out at most eight, so .0 and
/// .255 (ids 255 and 254) never occur - and these addresses never reach a wire.
pub fn relay_peer_for(client_id: u8) -> SocketAddr {
    let host = client_id.wrapping_add(1);
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(RELAY_PEER_NET[0], RELAY_PEER_NET[1], RELAY_PEER_NET[2], host)), RELAY_PEER_PORT)
}

/// The client id behind an address from [`relay_peer_for`]; `None` for any other.
pub fn relay_peer_id(addr: SocketAddr) -> Option<u8> {
    match addr {
        SocketAddr::V4(v4) if v4.ip().octets()[..3] == RELAY_PEER_NET && v4.port() == RELAY_PEER_PORT => {
            Some(v4.ip().octets()[3].wrapping_sub(1))
        }
        _ => None,
    }
}

/// The live relay session of a side: its socket, the relay address it sends
/// to, and the token of the moment, under one lock, so a send never takes a
/// socket of one session with the token of another (review 117).
///
/// The socket is unconnected (phase 3c): the same socket talks to the relay
/// and, once a punch has worked, directly - so the direct packets leave under
/// the very address the relay saw. What the operating system filtered for a
/// connected socket is filtered here: only `relay` and the punched target get
/// through ([`RelayLeg::poll_recv`]); the rest is dropped and counted.
struct Live {
    session: u64,
    /// When this session's socket stood: UDP not seen to work for
    /// [`RELAY_QUIET`] after it, the way is named TCP (build 146).
    began: Instant,
    socket: Arc<UdpSocket>,
    relay: SocketAddr,
    token: [u8; 32],
    /// This side's address as the relay sees it (`udp-seen`), once it is known.
    public: Option<SocketAddr>,
    /// The client's punching in this session; the server's leg leaves it be.
    punch: ClientPunch,
}

/// Where a client's punching stands in one relay session. A new session (a
/// network change) starts over on its new socket, as the third experiment
/// found it must.
#[derive(Default)]
struct ClientPunch {
    offers: u32,
    last_offer: Option<Instant>,
    direct: Option<Direct>,
    gave_up: bool,
}

/// The server's answer: where to send directly. `confirmed` once a QUIC packet
/// came from exactly there - from then on the client sends directly only, for
/// as long as something keeps coming from there (`last_in`, [`DIRECT_SILENCE`]).
/// `local`: the server's own port at its local address (L+R), not a punch.
struct Direct {
    target: SocketAddr,
    since: Instant,
    confirmed: bool,
    last_in: Instant,
    port_logged: bool,
    local: bool,
}

impl Direct {
    /// The way this answer is, once confirmed.
    fn way(&self) -> ClientPath {
        if self.local {
            ClientPath::Local
        } else {
            ClientPath::Direct
        }
    }

    /// The word in the log.
    fn word(&self) -> &'static str {
        if self.local {
            "local"
        } else {
            "direct"
        }
    }
}

/// The same address, whatever the IPv6 flow and scope fields say.
fn same(a: SocketAddr, b: SocketAddr) -> bool {
    a.ip() == b.ip() && a.port() == b.port()
}

/// The order in which a leg tries the relay's addresses: the IPv4 one beside
/// an IPv6 first one goes first (decision 3, phase 3c). Punching was proven over
/// the phone's IPv4 in the third experiment, and on 5G the relay saw the phone
/// as IPv4 anyway (field test 123). The classic audio path keeps following the
/// family (`connect_relay_udp`).
fn relay_order(g: &UdpGrant) -> Vec<SocketAddr> {
    g.fallback.into_iter().chain(std::iter::once(g.relay)).collect()
}

/// What a relay leg did in one measuring window: datagrams handed to the
/// socket, dropped by our own code because the socket was busy or the send
/// failed, and received from the relay. The busy ones are the drop quinn never
/// sees (it hears "sent"), so only this count shows them (measurement build 122:
/// loss on the relay path, and whether it is ours).
/// `no_session` is what had to go while the leg had no relay session at all - a
/// relay outage takes the grant away, and every datagram to every client then
/// went without a count (review 122).
/// The direct path (phase 3c) is counted the same way, beside it, and so is
/// what the source filter drops: what came from neither the relay nor the
/// punched peer.
#[derive(Default)]
struct LegCounts {
    to_relay: AtomicU64,
    busy: AtomicU64,
    failed: AtomicU64,
    no_session: AtomicU64,
    from_relay: AtomicU64,
    direct_out: AtomicU64,
    direct_dropped: AtomicU64,
    direct_in: AtomicU64,
    strangers: AtomicU64,
    tcp_out: AtomicU64,
    tcp_dropped: AtomicU64,
    tcp_in: AtomicU64,
}

impl LegCounts {
    /// The window so far, and start the next: `[to_relay, busy, failed,
    /// no_session, from_relay, direct_out, direct_dropped, direct_in, strangers,
    /// tcp_out, tcp_dropped, tcp_in]`.
    fn take(&self) -> [u64; 12] {
        [
            &self.to_relay,
            &self.busy,
            &self.failed,
            &self.no_session,
            &self.from_relay,
            &self.direct_out,
            &self.direct_dropped,
            &self.direct_in,
            &self.strangers,
            &self.tcp_out,
            &self.tcp_dropped,
            &self.tcp_in,
        ]
        .map(|c| c.swap(0, Ordering::Relaxed))
    }
}

/// What came in on a leg: a relay datagram for (or from) a client id, the same
/// over the relay's TCP line, or a QUIC packet straight from the punched peer.
/// Each length is in `out`.
enum Arrived {
    Relay(u8, usize),
    Tcp(u8, usize),
    Direct(usize),
}

/// The relay leg: the live session, swapped by a task that follows the grant.
struct RelayLeg {
    live: Mutex<Option<Live>>,
    seq: AtomicU32,
    recv_waker: Mutex<Option<Waker>>,
    counts: LegCounts,
    relay_only: Arc<std::sync::atomic::AtomicBool>,
    path: Arc<std::sync::atomic::AtomicU8>,
    /// The relay's TCP line (build 144): frames out, and what came in.
    tcp_out: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    tcp_in: Mutex<Option<tokio::sync::mpsc::Receiver<(u8, Vec<u8>)>>>,
    /// The relay session in which something came back over UDP from the relay
    /// ([`NO_SESSION`] before that): from then on a client sends no copy over
    /// TCP. Per session, so a network switch starts over - the new network may
    /// be the one that blocks UDP.
    udp_works_in: AtomicU64,
    force_tcp: Arc<std::sync::atomic::AtomicBool>,
    /// The session of each new relay socket, for the client's connect loop: a
    /// connection attempt that went out over a dead route is started again
    /// when the relay is back, instead of waiting out QUIC's back-off (up to
    /// the idle timeout; build 178).
    new_session: watch::Sender<u64>,
}

/// No relay session, for [`RelayLeg::udp_works_in`].
const NO_SESSION: u64 = u64::MAX;

impl fmt::Debug for RelayLeg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let session = self.live.lock().unwrap().as_ref().map(|l| l.session);
        f.debug_struct("RelayLeg").field("session", &session).finish()
    }
}

impl RelayLeg {
    fn start(route: RelayRoute) -> Arc<RelayLeg> {
        let tcp = route.tcp.lock().unwrap().take();
        let (tcp_out, tcp_in) = match tcp {
            Some(line) => (Some(line.to_relay), Some(line.from_relay)),
            None => (None, None),
        };
        let leg = Arc::new(RelayLeg {
            live: Mutex::new(None),
            seq: AtomicU32::new(0),
            recv_waker: Mutex::new(None),
            counts: LegCounts::default(),
            relay_only: route.relay_only,
            force_tcp: route.force_tcp,
            path: route.path,
            tcp_out,
            tcp_in: Mutex::new(tcp_in),
            udp_works_in: AtomicU64::new(NO_SESSION),
            new_session: watch::channel(NO_SESSION).0,
        });
        tokio::spawn(follow(Arc::downgrade(&leg), route.grant));
        leg
    }

    fn socket(&self) -> Option<(Arc<UdpSocket>, SocketAddr)> {
        self.live.lock().unwrap().as_ref().map(|l| (l.socket.clone(), l.relay))
    }

    /// A QUIC packet for `client_id` over the relay's TCP line, as a TLT1 frame
    /// (build 144). A full line, or none, drops it - counted.
    fn send_tcp(&self, client_id: u8, payload: &[u8]) {
        let Some(tx) = &self.tcp_out else { return };
        match tx.try_send(tlt1_frame(client_id, payload)) {
            Ok(()) => self.counts.tcp_out.fetch_add(1, Ordering::Relaxed),
            Err(_) => self.counts.tcp_dropped.fetch_add(1, Ordering::Relaxed),
        };
    }

    /// Something came back over UDP from the relay in `session`.
    fn udp_works(&self, session: u64) {
        if self.udp_works_in.load(Ordering::Relaxed) != session && self.udp_works_in.swap(session, Ordering::Relaxed) != session {
            // Named in the brief before it was built (working rule 11, build 144).
            info!("QUIC via relay: UDP works in session {session}");
        }
    }

    /// This side's public address in the live session, as the relay saw it.
    fn public(&self) -> Option<SocketAddr> {
        self.live.lock().unwrap().as_ref().and_then(|l| l.public)
    }

    /// Frame `payload` for `client_id` and send it. Without a session, or when
    /// the socket is busy, the datagram is dropped: QUIC sends again what it
    /// misses, and a relay leg must never hold up the direct one.
    fn send(&self, client_id: u8, payload: &[u8]) {
        let Some((sock, relay, token)) = self.live.lock().unwrap().as_ref().map(|l| (l.socket.clone(), l.relay, l.token)) else {
            self.counts.no_session.fetch_add(1, Ordering::Relaxed);
            return;
        };
        self.send_framed(&sock, relay, &token, client_id, payload);
    }

    fn send_framed(&self, sock: &UdpSocket, relay: SocketAddr, token: &[u8; 32], client_id: u8, payload: &[u8]) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        match sock.try_send_to(&encode_tlu1_raw(client_id, seq, token, payload), relay) {
            Ok(_) => self.counts.to_relay.fetch_add(1, Ordering::Relaxed),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.counts.busy.fetch_add(1, Ordering::Relaxed),
            Err(e) => {
                debug!("QUIC via relay: send failed: {e}");
                self.counts.failed.fetch_add(1, Ordering::Relaxed)
            }
        };
    }

    /// A QUIC packet straight to `target`, without the relay's header (phase
    /// 3c). Dropped like a relay datagram when it cannot leave.
    fn send_direct(&self, sock: &UdpSocket, target: SocketAddr, payload: &[u8]) {
        match sock.try_send_to(payload, target) {
            Ok(_) => self.counts.direct_out.fetch_add(1, Ordering::Relaxed),
            Err(e) => {
                if e.kind() != io::ErrorKind::WouldBlock {
                    debug!("QUIC direct: send failed: {e}");
                }
                self.counts.direct_dropped.fetch_add(1, Ordering::Relaxed)
            }
        };
    }

    /// The client's send: through the relay until the direct path is
    /// confirmed, and once the server has punched directly as well - both,
    /// until then (QUIC drops the copy), so a punch that fails costs nothing.
    /// And until UDP is seen to work in this relay session, a copy over the
    /// relay's TCP line as well (build 144): on a network that lets no UDP
    /// through, that copy is the connection - without waiting to find out.
    fn send_client(&self, payload: &[u8]) {
        let Some((sock, relay, token, direct, session)) = self.live.lock().unwrap().as_ref().map(|l| {
            (l.socket.clone(), l.relay, l.token, l.punch.direct.as_ref().map(|d| (d.target, d.confirmed)), l.session)
        }) else {
            self.counts.no_session.fetch_add(1, Ordering::Relaxed);
            return;
        };
        // "Force TCP" (build 146): the TCP line only, as on a network that
        // lets no UDP through. The punch is let go at the next offer tick.
        if self.force_tcp.load(Ordering::Relaxed) && self.tcp_out.is_some() {
            self.send_tcp(0, payload);
            return;
        }
        if !direct.is_some_and(|(_, confirmed)| confirmed) {
            // A client sends with its own id unread: the relay stamps the sender.
            self.send_framed(&sock, relay, &token, 0, payload);
            if self.udp_works_in.load(Ordering::Relaxed) != session {
                self.send_tcp(0, payload);
            }
        }
        if let Some((target, _)) = direct {
            self.send_direct(&sock, target, payload);
        }
    }

    /// One datagram from the relay or the punched peer, copied into `out`.
    /// From the relay: `[client_id] + payload`, a single byte being a bare
    /// keepalive, skipped. Anything else goes through [`RelayLeg::from_direct`].
    fn poll_recv(&self, cx: &mut Context, out: &mut [u8]) -> Poll<io::Result<Arrived>> {
        *self.recv_waker.lock().unwrap() = Some(cx.waker().clone());
        let Some((sock, relay, session)) = self.live.lock().unwrap().as_ref().map(|l| (l.socket.clone(), l.relay, l.session)) else {
            return Poll::Pending;
        };
        let mut scratch = [0u8; RELAY_DATAGRAM_MAX];
        loop {
            let mut rb = ReadBuf::new(&mut scratch);
            match sock.poll_recv_from(cx, &mut rb) {
                Poll::Ready(Ok(src)) => {
                    let data = rb.filled();
                    if same(src, relay) {
                        self.udp_works(session);
                        if data.len() <= 1 {
                            continue; // a keepalive proves the path; there is nothing to deliver
                        }
                        let payload = &data[1..];
                        if payload.len() > out.len() {
                            debug!("QUIC via relay: a {} B datagram does not fit - dropped", payload.len());
                            continue;
                        }
                        out[..payload.len()].copy_from_slice(payload);
                        self.counts.from_relay.fetch_add(1, Ordering::Relaxed);
                        return Poll::Ready(Ok(Arrived::Relay(data[0], payload.len())));
                    }
                    if !self.from_direct(src, data) {
                        continue;
                    }
                    if data.len() > out.len() {
                        debug!("QUIC direct: a {} B datagram does not fit - dropped", data.len());
                        continue;
                    }
                    out[..data.len()].copy_from_slice(data);
                    self.counts.direct_in.fetch_add(1, Ordering::Relaxed);
                    return Poll::Ready(Ok(Arrived::Direct(data.len())));
                }
                // A UDP socket reports an ICMP "port unreachable" as an error
                // on the next read; the next datagram is unaffected. This loop
                // ends: one error per datagram of ours that bounced, then
                // `Pending` - measured on WinSock for a connected socket (review
                // 117) and an unconnected one (2026-10-07, before build 125).
                Poll::Ready(Err(e)) => {
                    debug!("QUIC via relay: receive error: {e}");
                    continue;
                }
                Poll::Pending => break,
            }
        }
        self.poll_tcp(cx, out)
    }

    /// One QUIC packet from the relay's TCP line, copied into `out` (build 144).
    fn poll_tcp(&self, cx: &mut Context, out: &mut [u8]) -> Poll<io::Result<Arrived>> {
        let mut line = self.tcp_in.lock().unwrap();
        let Some(rx) = line.as_mut() else {
            return Poll::Pending;
        };
        loop {
            match rx.poll_recv(cx) {
                Poll::Ready(Some((cid, data))) => {
                    if data.len() > out.len() {
                        debug!("QUIC over TCP: a {} B packet does not fit - dropped", data.len());
                        continue;
                    }
                    out[..data.len()].copy_from_slice(&data);
                    self.counts.tcp_in.fetch_add(1, Ordering::Relaxed);
                    return Poll::Ready(Ok(Arrived::Tcp(cid, data.len())));
                }
                // The monitor is gone: no line any more.
                Poll::Ready(None) => {
                    *line = None;
                    return Poll::Pending;
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    /// Whether a datagram from `src` - not the relay - is the punched peer's
    /// QUIC packet. The server's probe is dropped here (it only opens the
    /// server's router), and so is everything from anywhere else, counted.
    fn from_direct(&self, src: SocketAddr, data: &[u8]) -> bool {
        let mut live = self.live.lock().unwrap();
        let direct = live.as_mut().and_then(|l| l.punch.direct.as_mut());
        if data == PUNCH_PROBE {
            if let Some(d) = direct {
                if src.ip() == d.target.ip() && src.port() != d.target.port() && !d.port_logged {
                    d.port_logged = true;
                    info!("QUIC punch: the server's probe came from another port than it said - its router changed the port");
                }
            }
            return false;
        }
        match direct {
            Some(d) if same(src, d.target) => {
                if !d.confirmed {
                    d.confirmed = true;
                    info!("QUIC {} path up after {} ms", d.word(), d.since.elapsed().as_millis());
                    self.path.store(d.way() as u8, Ordering::Relaxed);
                }
                d.last_in = Instant::now();
                true
            }
            _ => {
                self.counts.strangers.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// Say which way the traffic takes now (`RelayRoute::path`), from the punch
    /// state as it stands - and through the relay, whether that is its TCP line
    /// (build 146): "force TCP" on, or UDP not seen to work for [`RELAY_QUIET`]
    /// after the session began. Not at once: the first answers of every session
    /// come over TCP before the first over UDP (review 144, measured).
    fn publish_path(&self, live: &Option<Live>) {
        let path = match live.as_ref().map(|l| (l, l.punch.direct.as_ref())) {
            None => ClientPath::Relay,
            Some((l, None)) => {
                let tcp = self.tcp_out.is_some()
                    && (self.force_tcp.load(Ordering::Relaxed)
                        || (self.udp_works_in.load(Ordering::Relaxed) != l.session && l.began.elapsed() >= RELAY_QUIET));
                if tcp {
                    ClientPath::RelayTcp
                } else {
                    ClientPath::Relay
                }
            }
            Some((_, Some(d))) if d.confirmed => d.way(),
            Some((_, Some(_))) => ClientPath::Coupling,
        };
        self.path.store(path as u8, Ordering::Relaxed);
    }

    /// [`RelayLeg::offer_due_now`], and then the way the traffic takes, as it
    /// stands after whatever this tick changed.
    fn offer_due(&self) -> Option<SocketAddr> {
        let due = self.offer_due_now();
        self.publish_path(&self.live.lock().unwrap());
        due
    }

    /// The address to offer the server now, if an offer is due: this session's
    /// public address is known and no answer came yet. One per [`OFFER_EVERY`]
    /// for the first [`MAX_OFFERS`] - an offer is a datagram, and datagrams get
    /// lost - then one per [`OFFER_EVERY_SLOW`]: the server takes an offer only
    /// once the password is proven, and a code typed by hand can take a minute.
    ///
    /// Also where the client leaves the direct path: a punch that does not
    /// confirm within [`GIVE_UP`] is given up (no copies to an address that
    /// does not answer), and a confirmed path that has been silent for
    /// [`DIRECT_SILENCE`] goes back to both ways and punches again (build 126:
    /// before, it sent into nothing until QUIC gave up after 30 s).
    fn offer_due_now(&self) -> Option<SocketAddr> {
        let mut live = self.live.lock().unwrap();
        let l = live.as_mut()?;
        // "Relay only" (build 137): no offer, and a direct path - confirmed or
        // being tried - is let go at once, so every packet goes through the
        // relay from the next send on. Switched off again, the punch starts
        // over from nothing.
        if self.relay_only.load(Ordering::Relaxed) || self.force_tcp.load(Ordering::Relaxed) {
            if l.punch.direct.is_some() {
                // Named in the brief before it was built (working rule 11,
                // builds 137 and 147): the switch that did it, by its name.
                if self.force_tcp.load(Ordering::Relaxed) {
                    info!("QUIC punch: force TCP - back to the relay");
                } else {
                    info!("QUIC punch: relay only - back to the relay");
                }
            }
            l.punch = ClientPunch::default();
            return None;
        }
        let public = l.public?;
        let p = &mut l.punch;
        if p.gave_up {
            return None;
        }
        if let Some(d) = &p.direct {
            if !d.confirmed && d.since.elapsed() >= GIVE_UP {
                info!("QUIC punch: the {} path did not answer within {} s - staying on the relay", d.word(), GIVE_UP.as_secs());
                p.direct = None;
                p.gave_up = true;
            } else if d.confirmed && d.last_in.elapsed() >= DIRECT_SILENCE {
                info!(
                    "QUIC back to the relay after {} ms of silence on the {} path",
                    d.last_in.elapsed().as_millis(),
                    d.word()
                );
                *p = ClientPunch::default();
            }
            return None;
        }
        if p.last_offer.is_some_and(|t| t.elapsed() < offer_interval(p.offers)) {
            return None;
        }
        if p.offers == MAX_OFFERS {
            info!(
                "QUIC punch: no answer to {MAX_OFFERS} offers - offering every {} s from now on",
                OFFER_EVERY_SLOW.as_secs()
            );
        }
        p.offers = p.offers.saturating_add(1);
        p.last_offer = Some(Instant::now());
        Some(public)
    }

    /// The server's answer: it punched, from `addr`. Taken only in answer to
    /// an offer of this session, and only when this socket can reach it.
    fn ready(&self, addr: SocketAddr) {
        self.answer(addr, false)
    }

    /// The server's other answer (L+R, build 142): it is behind the same
    /// router, at `addr` - its own port at its local address. Taken like a
    /// punch, and only an address on a local network.
    fn ready_local(&self, addr: SocketAddr) {
        self.answer(addr, true)
    }

    fn answer(&self, addr: SocketAddr, local: bool) {
        let mut live = self.live.lock().unwrap();
        let Some(l) = live.as_mut() else { return };
        let p = &mut l.punch;
        if p.offers == 0 || p.gave_up {
            debug!("QUIC punch: an answer to no offer of this session - ignored");
            return;
        }
        if addr.is_ipv4() != l.relay.is_ipv4() {
            info!(
                "QUIC punch: the server is at an {} address and this socket is {} - staying on the relay",
                sdr_remote_relay::family(addr),
                sdr_remote_relay::family(l.relay)
            );
            p.gave_up = true;
            return;
        }
        if !usable_target(addr) {
            info!("QUIC punch: the server's address is no place to send to - staying on the relay");
            p.gave_up = true;
            return;
        }
        if local && !sdr_remote_core::path::is_local(addr.ip()) {
            info!("QUIC local: the server named an address that is not on a local network - staying on the relay");
            p.gave_up = true;
            return;
        }
        if p.direct.as_ref().is_some_and(|d| d.confirmed || same(d.target, addr)) {
            return; // the answer to an offer that crossed it
        }
        if local {
            // Named in the brief before it was built (working rule 11, build 142).
            info!("QUIC local: the server is on this network - trying its own port");
        } else {
            info!("QUIC punch: the server punched ({}) - trying the direct path", sdr_remote_relay::family(addr));
        }
        let now = Instant::now();
        p.direct = Some(Direct { target: addr, since: now, confirmed: false, last_in: now, port_logged: false, local });
        self.path.store(ClientPath::Coupling as u8, Ordering::Relaxed);
    }

    /// Start punching over: a new QUIC connection gets a server socket of its
    /// own; the one of the old connection goes with that connection.
    fn restart_punch(&self) {
        let mut live = self.live.lock().unwrap();
        if let Some(l) = live.as_mut() {
            l.punch = ClientPunch::default();
        }
        // And say so at once: a new connection must not start with the old
        // one's way until the next offer tick (review 139, measured).
        self.publish_path(&live);
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        match self.socket() {
            Some((s, _)) => s.local_addr(),
            None => Ok(SocketAddr::from(([0, 0, 0, 0], 0))),
        }
    }
}

/// Follow the grant: a new session gets a new socket (for that grant's relay
/// address, IPv4 first: [`relay_order`]), a rotation only a new token - and a
/// new public address in it starts the punching over. Sends an empty datagram
/// at once on every change (so the relay binds the source before the first real
/// packet) and then every second. Ends when the socket it serves is gone, or
/// the grant watch closes.
async fn follow(leg: Weak<RelayLeg>, mut grant: watch::Receiver<Option<UdpGrant>>) {
    let mut tick = tokio::time::interval(MAPPING_KEEPALIVE);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // A grant that is already there when the leg starts counts as a change: a
    // watch only reports what comes after it was read.
    let mut first = true;
    // Ticks until the next measuring line (one tick is the keepalive second).
    let report_every = (crate::STATS_EVERY.as_millis() / MAPPING_KEEPALIVE.as_millis()).max(1) as u32;
    let mut ticks = 0u32;
    loop {
        let changed = if std::mem::take(&mut first) {
            true
        } else {
            tokio::select! {
                r = grant.changed() => {
                    if r.is_err() {
                        // The monitor is gone (on the phone it goes with its
                        // bridge): no session any more, so no socket either,
                        // rather than sending on into a dead token (review 118).
                        if let Some(leg) = leg.upgrade() {
                            *leg.live.lock().unwrap() = None;
                            info!("QUIC via relay: the relay monitor stopped");
                        }
                        return;
                    }
                    true
                }
                _ = tick.tick() => false,
            }
        };
        let Some(leg) = leg.upgrade() else {
            return;
        };
        if changed {
            let now = *grant.borrow_and_update();
            let session_now = leg.live.lock().unwrap().as_ref().map(|l| l.session);
            match now {
                None => {
                    *leg.live.lock().unwrap() = None;
                    info!("QUIC via relay: no relay session");
                }
                Some(g) if session_now == Some(g.session) => {
                    // The relay's rotation: the same socket, the new token. Or
                    // the relay's word on our public address: a new one means
                    // the server punched towards an address that is gone.
                    if let Some(l) = leg.live.lock().unwrap().as_mut() {
                        l.token = g.token;
                        if l.public != g.public {
                            if l.public.is_some() {
                                info!("QUIC via relay: session {} has a new public address - punching again", g.session);
                            }
                            l.public = g.public;
                            l.punch = ClientPunch::default();
                        }
                    }
                }
                Some(g) => match sdr_remote_relay::bind_relay_udp(&relay_order(&g)).await {
                    Ok((sock, used)) => {
                        // Until the reactor has seen a fresh socket writable, its
                        // `try_send_to` says "would block" (measured 2026-10-07:
                        // nearly always) - the keepalive below that binds this
                        // source at the relay was dropped as busy, and the
                        // binding waited for the next tick, up to a second, at
                        // every new session since build 117.
                        if let Err(e) = sock.writable().await {
                            debug!("QUIC via relay: the new socket is not writable: {e}");
                        }
                        // The family only, never the socket's own address: on
                        // IPv6 that is the phone's public address, and this log
                        // goes along with a problem report (field test 120).
                        info!(
                            "QUIC via relay: session {} on a new socket ({})",
                            g.session,
                            sdr_remote_relay::family(used)
                        );
                        *leg.live.lock().unwrap() = Some(Live {
                            session: g.session,
                            began: Instant::now(),
                            socket: Arc::new(sock),
                            relay: used,
                            token: g.token,
                            public: g.public,
                            punch: ClientPunch::default(),
                        });
                        leg.new_session.send_replace(g.session);
                    }
                    Err(e) => {
                        warn!("QUIC via relay: no socket for session {}: {e}", g.session);
                        *leg.live.lock().unwrap() = None;
                    }
                },
            }
            // Say the way at once: every branch above may have emptied the punch
            // state (no session, a new public address, a new session), and the
            // next offer tick is up to 250 ms away - the old way stood on screen
            // over a network switch until then (review 140).
            leg.publish_path(&leg.live.lock().unwrap());
            // Whatever waited on the old socket waits on the new one now.
            if let Some(w) = leg.recv_waker.lock().unwrap().take() {
                w.wake();
            }
        }
        // The station holds its mapping with an empty datagram to the keepalive
        // id; a client sends the same, and the relay stamps it as its own.
        leg.send(UDP_MAPPING_KEEPALIVE_ID, &[]);
        // Only the ticks count towards the window: a loop woken by a grant (a
        // rotation, a gap, a new session) is no second, and during a relay outage
        // those come in bunches - the very window the line is read for (review 122).
        if !changed {
            ticks += 1;
        }
        if ticks >= report_every {
            ticks = 0;
            let [to, busy, failed, no_session, from, d_out, d_dropped, d_in, strangers, t_out, t_dropped, t_in] = leg.counts.take();
            let session = if leg.live.lock().unwrap().is_some() { "" } else { " (no relay session now)" };
            dev_info!(
                "QUIC via relay ({} s){session}: {to} datagrams to the relay, {busy} dropped here (socket busy), {failed} failed, {no_session} dropped with no relay session, {from} from the relay; direct: {d_out} out, {d_dropped} dropped here, {d_in} in; {strangers} from an unknown source dropped; over TCP: {t_out} out, {t_dropped} dropped here, {t_in} in",
                crate::STATS_EVERY.as_secs()
            );
        }
    }
}

/// One datagram for quinn. `stride` must stay equal to `len`: quinn cuts a
/// received buffer into pieces of `stride` (quinn endpoint.rs:794-796), and a
/// `stride` of 0 with a `len` above it is a loop that never ends - the endpoint
/// would hang without a word (review 117).
fn fill(meta: &mut RecvMeta, addr: SocketAddr, len: usize) {
    *meta = RecvMeta { addr, len, stride: len, ecn: None, dst_ip: None };
}

// ------------------------------------------------------------------ client

/// The client's socket when it goes via the relay: everything quinn sends goes
/// to the station through the relay (and, once punched, directly); everything
/// that comes back - by either way - is reported as coming from the address
/// quinn sent to, so quinn never sees the path move.
#[derive(Debug)]
pub(crate) struct ClientRelaySocket {
    leg: Arc<RelayLeg>,
    peer: Mutex<Option<SocketAddr>>,
}

impl ClientRelaySocket {
    pub(crate) fn start(route: RelayRoute) -> Arc<ClientRelaySocket> {
        Arc::new(ClientRelaySocket { leg: RelayLeg::start(route), peer: Mutex::new(None) })
    }

    /// Each new relay session's number, as its socket stands (build 178).
    pub(crate) fn new_sessions(&self) -> watch::Receiver<u64> {
        self.leg.new_session.subscribe()
    }

    /// See [`RelayLeg::offer_due`].
    pub(crate) fn offer_due(&self) -> Option<SocketAddr> {
        self.leg.offer_due()
    }

    /// See [`RelayLeg::ready`].
    pub(crate) fn ready(&self, addr: SocketAddr) {
        self.leg.ready(addr)
    }

    /// See [`RelayLeg::ready_local`].
    pub(crate) fn ready_local(&self, addr: SocketAddr) {
        self.leg.ready_local(addr)
    }

    /// See [`RelayLeg::restart_punch`].
    pub(crate) fn restart_punch(&self) {
        self.leg.restart_punch()
    }
}

impl AsyncUdpSocket for ClientRelaySocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(AlwaysWritable)
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        *self.peer.lock().unwrap() = Some(transmit.destination);
        self.leg.send_client(transmit.contents);
        Ok(())
    }

    fn poll_recv(&self, cx: &mut Context, bufs: &mut [IoSliceMut<'_>], meta: &mut [RecvMeta]) -> Poll<io::Result<usize>> {
        loop {
            match self.leg.poll_recv(cx, &mut bufs[0]) {
                Poll::Ready(Ok(Arrived::Relay(_, len) | Arrived::Tcp(_, len) | Arrived::Direct(len))) => {
                    // Nothing was sent yet, so nothing can be an answer.
                    let Some(peer) = *self.peer.lock().unwrap() else { continue };
                    fill(&mut meta[0], peer, len);
                    return Poll::Ready(Ok(1));
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.leg.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        // One datagram per send, said here rather than left to quinn's default:
        // a batch would go out as one relay payload, and the other side would
        // get two QUIC packets in one datagram and drop both (review 117).
        1
    }

    fn may_fragment(&self) -> bool {
        // false keeps quinn's MTU search on (quinn endpoint.rs: allow_mtud);
        // the ceiling of RELAY_MTU_CEILING keeps it below the relay's room.
        false
    }
}

/// Relay sends never wait: a datagram that cannot leave is dropped (see
/// [`RelayLeg::send`]), so the poller is always ready.
#[derive(Debug)]
struct AlwaysWritable;

impl UdpPoller for AlwaysWritable {
    fn poll_writable(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

// ------------------------------------------------------------------ server

/// The server's answer to an offer: a punch from a fresh socket, or - behind
/// the same router as the client - its own port at its local address (L+R).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Answer {
    Punched(SocketAddr),
    Local(SocketAddr),
}

/// Whether `ip` may be given out as this side's local address (L+R): IPv4 (the
/// design is IPv4 only, `OPZET-L-plus-R.md` section 5), on a local network
/// (`is_local` - a server with a public address on its card is not at home), and
/// not loopback unless a test says so: a relay reached over loopback is a test.
/// A rule of its own with a table test: inside `home_addr` only the loopback
/// part could be steered, and the other two stayed green when taken out
/// (reviews 142).
fn is_home(ip: IpAddr, loopback_ok: bool) -> bool {
    ip.is_ipv4() && sdr_remote_core::path::is_local(ip) && (!ip.is_loopback() || loopback_ok)
}

/// This side's IP towards `relay`, as the operating system would send from:
/// read from a UDP socket "connected" there - connecting a UDP socket sends
/// nothing (`OPZET-L-plus-R.md` section 2).
fn own_ip_towards(relay: SocketAddr) -> Option<IpAddr> {
    let any = if relay.is_ipv4() {
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
    } else {
        SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0))
    };
    let s = std::net::UdpSocket::bind(any).ok()?;
    s.connect(relay).ok()?;
    Some(s.local_addr().ok()?.ip())
}

/// One client's punch on the server (phase 3c): a fresh socket of its own (the
/// third experiment: a fresh port per client, against the poisoning), the
/// client's public address it punches to, and when QUIC last came which way.
#[derive(Debug)]
struct Punched {
    socket: UdpSocket,
    target: SocketAddr,
    /// What the server told the client: its public IP and this socket's port.
    ready: SocketAddr,
    /// The QUIC connection it was made for (quinn's `stable_id`).
    owner: usize,
    since: Instant,
    /// When the last QUIC packet of this client came directly, and through the
    /// relay: milliseconds after `since`, plus one - 0 is "never".
    last_direct: AtomicU64,
    last_relay: AtomicU64,
    port_logged: AtomicBool,
    ended: AtomicBool,
}

impl Punched {
    fn stamp(&self, at: &AtomicU64) {
        at.store(self.since.elapsed().as_millis() as u64 + 1, Ordering::Relaxed);
    }

    fn ago(&self, at: &AtomicU64) -> Option<Duration> {
        match at.load(Ordering::Relaxed) {
            0 => None,
            v => Some(self.since.elapsed().saturating_sub(Duration::from_millis(v - 1))),
        }
    }

    /// Directly only: QUIC came directly, and none through the relay for
    /// [`RELAY_QUIET`] - the client has its direct path confirmed and stopped
    /// sending a copy. Until then both ways, so a punch that works in one
    /// direction only costs nothing.
    fn direct_only(&self) -> bool {
        self.ago(&self.last_direct).is_some() && self.ago(&self.last_relay).map_or(true, |a| a >= RELAY_QUIET)
    }
}

/// The server's socket: its own UDP port (method 1, addresses as they are) and,
/// when a relay is set, the relay (each client at [`relay_peer_for`] its id) -
/// and a fresh socket per punched client, reported under that same address.
#[derive(Debug)]
pub(crate) struct ServerRouteSocket {
    direct: Arc<dyn AsyncUdpSocket>,
    relay: Option<Arc<RelayLeg>>,
    punched: Mutex<HashMap<u8, Arc<Punched>>>,
    /// The connections (quinn's `stable_id`) whose client proved the password:
    /// only those may punch (build 126). An offer is taken at the QUIC layer,
    /// before and apart from the session's proof, and an offer makes this side
    /// send to an address of the client's choosing (review 125).
    proven: Mutex<HashSet<usize>>,
    /// The connections already told about in the log: an offer before the proof.
    told_unproven: Mutex<HashSet<usize>>,
    /// The task that waits on quinn's receive, woken for a new punched socket.
    recv_waker: Mutex<Option<Waker>>,
    /// Which source is read first, in turn, so none starves the others - and
    /// which punched socket, within the punched ones (review 125).
    next_source: AtomicU32,
    next_punched: AtomicU32,
    /// The server takes connections through the relay only (build 135): what
    /// arrives on its own port is read and dropped, and an offer to punch is not
    /// answered. Dropped below quinn rather than refused by it, so a relay
    /// client whose relay address changes (a new relay session after a network
    /// switch) still moves with its connection.
    relay_only: bool,
    told_relay_only: Mutex<HashSet<usize>>,
    refused: Mutex<RefusedLog>,
    /// When something last came from each client id over the relay's TCP line
    /// (build 144): milliseconds after `born`, plus one - 0 is "never". The
    /// server sends a client over that line too while this is within
    /// [`RELAY_QUIET`]: it follows what the client does, as with a punch.
    tcp_in_at: Vec<AtomicU64>,
    /// The same for what came over UDP through the relay (build 145): a client
    /// heard over TCP and not over UDP gets no UDP copy - for a client whose
    /// UDP is closed it never arrives, and it doubled the station's uplink for
    /// the whole session (review 144).
    udp_in_at: Vec<AtomicU64>,
    born: Instant,
    /// A test's word that loopback counts as this side's local network.
    /// Loopback never does otherwise: every test runs on loopback, where the
    /// relay sees both ends at 127.0.0.1, so the punch tests would all take
    /// L+R. With this flag one test can take L+R while the others punch.
    #[cfg(test)]
    loopback_is_home: AtomicBool,
}

/// The direct datagrams dropped on a relay-only server, for one log line now
/// and then instead of one per datagram.
#[derive(Debug, Default)]
struct RefusedLog {
    last_line: Option<Instant>,
    since_line: u64,
}

/// How often a relay-only server says it is dropping direct datagrams.
const REFUSED_LOG_EVERY: Duration = Duration::from_secs(10);

impl ServerRouteSocket {
    pub(crate) fn new(direct: Arc<dyn AsyncUdpSocket>, relay: Option<RelayRoute>, relay_only: bool) -> Arc<ServerRouteSocket> {
        Arc::new(ServerRouteSocket {
            relay_only,
            told_relay_only: Mutex::default(),
            refused: Mutex::default(),
            direct,
            relay: relay.map(RelayLeg::start),
            punched: Mutex::default(),
            proven: Mutex::default(),
            told_unproven: Mutex::default(),
            recv_waker: Mutex::new(None),
            next_source: AtomicU32::new(0),
            next_punched: AtomicU32::new(0),
            tcp_in_at: (0..256).map(|_| AtomicU64::new(0)).collect(),
            udp_in_at: (0..256).map(|_| AtomicU64::new(0)).collect(),
            born: Instant::now(),
            #[cfg(test)]
            loopback_is_home: AtomicBool::new(false),
        })
    }

    /// Something came from client `id` over the relay's TCP line now.
    fn tcp_came(&self, id: u8) {
        let now = self.born.elapsed().as_millis() as u64 + 1;
        let before = self.tcp_in_at[id as usize].swap(now, Ordering::Relaxed);
        if before == 0 || now.saturating_sub(before) >= RELAY_QUIET.as_millis() as u64 {
            // Named in the brief before it was built (working rule 11, build 144).
            info!("QUIC via relay: client {id} sends over the relay's TCP line");
        }
    }

    /// Whether client `id` sent over the relay's TCP line within [`RELAY_QUIET`].
    fn tcp_recent(&self, id: u8) -> bool {
        self.heard_within(&self.tcp_in_at, id)
    }

    /// Whether client `id` sent over UDP through the relay within [`RELAY_QUIET`].
    fn udp_recent(&self, id: u8) -> bool {
        self.heard_within(&self.udp_in_at, id)
    }

    /// Whether client `id` is heard over the relay's TCP line and not over UDP:
    /// its way is `R (TCP)` (build 146).
    pub(crate) fn tcp_only(&self, id: u8) -> bool {
        self.tcp_recent(id) && !self.udp_recent(id)
    }

    fn heard_within(&self, at: &[AtomicU64], id: u8) -> bool {
        match at[id as usize].load(Ordering::Relaxed) {
            0 => false,
            t => (self.born.elapsed().as_millis() as u64 + 1).saturating_sub(t) < RELAY_QUIET.as_millis() as u64,
        }
    }

    #[cfg(test)]
    fn loopback_is_home(&self) -> bool {
        self.loopback_is_home.load(Ordering::Relaxed)
    }

    #[cfg(not(test))]
    fn loopback_is_home(&self) -> bool {
        false
    }

    /// This side's own port at its local address ([`is_home`]).
    fn home_addr(&self) -> Option<SocketAddr> {
        let (_, relay) = self.relay.as_ref()?.socket()?;
        let ip = own_ip_towards(relay)?;
        let port = self.direct.local_addr().ok()?.port();
        is_home(ip, self.loopback_is_home()).then(|| SocketAddr::new(ip, port))
    }

    /// Whether the server sends to client `id` directly only now - a punch
    /// whose way the client confirmed ([`Punched::direct_only`]).
    pub(crate) fn direct_only(&self, id: u8) -> bool {
        self.punched.lock().unwrap().get(&id).is_some_and(|p| !p.ended.load(Ordering::Relaxed) && p.direct_only())
    }

    /// Whether this server takes connections through the relay only.
    pub(crate) fn is_relay_only(&self) -> bool {
        self.relay_only
    }

    /// Whether the offer on connection `owner` is the first one this relay-only
    /// server refuses: said once per connection.
    pub(crate) fn first_relay_only_offer(&self, owner: usize) -> bool {
        self.told_relay_only.lock().unwrap().insert(owner)
    }

    /// The client on connection `owner` has proven the password: it may punch.
    pub(crate) fn prove(&self, owner: usize) {
        self.proven.lock().unwrap().insert(owner);
    }

    /// Its session ended while the connection lives on (review 126).
    pub(crate) fn revoke(&self, owner: usize) {
        self.proven.lock().unwrap().remove(&owner);
        // A new "not yet proven" stretch on this connection is said again (review 128).
        self.told_unproven.lock().unwrap().remove(&owner);
    }

    pub(crate) fn is_proven(&self, owner: usize) -> bool {
        self.proven.lock().unwrap().contains(&owner)
    }

    /// The server's half of a punch: client `id` offered `public` on the
    /// connection `owner`. Opens a fresh socket and sends the first probe at
    /// once - before the answer can reach the client, so nothing of the client
    /// arrives before the router holds a mapping (the poisoning of the third
    /// experiment) - and keeps probing ([`keep_punching`]). Returns the answer:
    /// this side's public IP and the fresh port. A repeated offer gets the same
    /// answer, a new address a new socket.
    ///
    /// A client at this side's own public IP is behind the same router (L+R,
    /// build 142): it gets this side's own port at its local address, and no
    /// punch - a punch of its own ends.
    pub(crate) fn punch(self: &Arc<Self>, id: u8, public: SocketAddr, owner: usize) -> Option<Answer> {
        let Some(ours) = self.relay.as_ref().and_then(|l| l.public()) else {
            info!("QUIC punch: client {id} offered, but this side's public address is not known yet");
            return None;
        };
        // A connection that moved to another client id leaves its old punch -
        // whichever answer it gets now (review 142: the L+R answer skipped this).
        self.punched.lock().unwrap().retain(|&k, p| {
            let keep = k == id || p.owner != owner;
            if !keep {
                p.ended.store(true, Ordering::Relaxed);
            }
            keep
        });
        if ours.ip() == public.ip() {
            // Named in the brief before it was built (working rule 11, build 142).
            match self.home_addr() {
                Some(home) => {
                    info!("QUIC punch: client {id} is behind this side's router - answered with the local address (port {})", home.port());
                    let mut map = self.punched.lock().unwrap();
                    if map.get(&id).is_some_and(|p| p.owner == owner) {
                        if let Some(old) = map.remove(&id) {
                            old.ended.store(true, Ordering::Relaxed);
                        }
                    }
                    return Some(Answer::Local(home));
                }
                None => info!(
                    "QUIC punch: client {id} is behind this side's router, but this side has no local address towards the relay - punching"
                ),
            }
        }
        if ours.is_ipv4() != public.is_ipv4() {
            info!(
                "QUIC punch: client {id} is at an {} address and this side at an {} one - staying on the relay",
                sdr_remote_relay::family(public),
                sdr_remote_relay::family(ours)
            );
            return None;
        }
        if !usable_target(public) {
            info!("QUIC punch: client {id} offered an address that is no place to send to - not punched");
            return None;
        }
        {
            let map = self.punched.lock().unwrap();
            if let Some(p) = map.get(&id) {
                // The same offer again: the same answer - unless the path had
                // gone silent, and the client offers because it fell back. Then
                // a fresh socket: the router's mapping of the old one may be gone.
                let silent = p.ago(&p.last_direct).is_some_and(|a| a >= DIRECT_SILENCE);
                if p.owner == owner && same(p.target, public) && !silent {
                    return Some(Answer::Punched(p.ready));
                }
            }
        }
        // The socket is opened outside the lock that every send takes (review 125).
        let any = if public.is_ipv4() {
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
        } else {
            SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0))
        };
        // The first probe goes out on the plain socket, before tokio has it: a
        // socket fresh in tokio refuses its first `try_send_to` as "would block"
        // until the reactor has seen it writable, and the probe was then never
        // sent (found by the test of build 126; build 125 claimed the order).
        let fresh = std::net::UdpSocket::bind(any).and_then(|s| {
            s.set_nonblocking(true)?;
            if let Err(e) = s.send_to(&PUNCH_PROBE, public) {
                debug!("QUIC punch: the first probe to client {id} did not go: {e}");
            }
            UdpSocket::from_std(s)
        });
        let (socket, port) = match fresh.and_then(|s| s.local_addr().map(|a| (s, a.port()))) {
            Ok(sp) => sp,
            Err(e) => {
                warn!("QUIC punch: no socket for client {id}: {e}");
                return None;
            }
        };
        let p = Arc::new(Punched {
            socket,
            target: public,
            ready: SocketAddr::new(ours.ip(), port),
            owner,
            since: Instant::now(),
            last_direct: AtomicU64::new(0),
            last_relay: AtomicU64::new(0),
            port_logged: AtomicBool::new(false),
            ended: AtomicBool::new(false),
        });
        // The offer itself came through the relay.
        p.stamp(&p.last_relay);
        info!("QUIC punch: client {id} at an {} address, punching from port {port}", sdr_remote_relay::family(public));
        if let Some(old) = self.punched.lock().unwrap().insert(id, p.clone()) {
            old.ended.store(true, Ordering::Relaxed);
        }
        tokio::spawn(keep_punching(Arc::downgrade(self), id, p.clone()));
        if let Some(w) = self.recv_waker.lock().unwrap().take() {
            w.wake();
        }
        Some(Answer::Punched(p.ready))
    }

    /// How many clients are punched now.
    #[cfg(test)]
    pub(crate) fn punches(&self) -> usize {
        self.punched.lock().unwrap().len()
    }

    /// Where the punches go now.
    #[cfg(test)]
    pub(crate) fn punch_targets(&self) -> Vec<SocketAddr> {
        self.punched.lock().unwrap().values().map(|p| p.target).collect()
    }

    /// Drop every punch at once - as if the server's punched sockets were gone
    /// - for the fallback test.
    #[cfg(test)]
    pub(crate) fn drop_punches(&self) {
        for (_, p) in self.punched.lock().unwrap().drain() {
            p.ended.store(true, Ordering::Relaxed);
        }
    }

    /// Whether this is the first offer on connection `owner` that came before
    /// the password - said once per connection, so a log at the usual level
    /// tells "offered, not yet answered" from "never offered" (review 127).
    pub(crate) fn first_unproven_offer(&self, owner: usize) -> bool {
        self.told_unproven.lock().unwrap().insert(owner)
    }

    /// The punches of a connection that closed end with it, and so does its proof.
    pub(crate) fn forget(&self, owner: usize) {
        self.proven.lock().unwrap().remove(&owner);
        self.told_unproven.lock().unwrap().remove(&owner);
        self.told_relay_only.lock().unwrap().remove(&owner);
        self.punched.lock().unwrap().retain(|_, p| {
            let keep = p.owner != owner;
            if !keep {
                p.ended.store(true, Ordering::Relaxed);
            }
            keep
        });
    }

    /// The server's own port on a relay-only server: everything that arrives is
    /// read (or it would pile up) and dropped, so no connection starts on it and
    /// none moves onto it. Said in the log at most once per [`REFUSED_LOG_EVERY`].
    ///
    /// One batch per turn: after it, the task is woken again and the turn goes to
    /// the other sources. Reading until the port is empty would let a stream of
    /// datagrams on it keep the relay - the one way in on such a server - from
    /// its turn (review 135).
    fn poll_direct_refused(&self, cx: &mut Context, bufs: &mut [IoSliceMut<'_>], meta: &mut [RecvMeta]) -> Poll<io::Result<usize>> {
        match self.direct.poll_recv(cx, bufs, meta) {
            Poll::Ready(Ok(n)) => {
                let from = meta[0].addr;
                let mut log = self.refused.lock().unwrap();
                log.since_line += n as u64;
                if log.last_line.map_or(true, |t| t.elapsed() >= REFUSED_LOG_EVERY) {
                    // Named in the brief before it was built (working rule 11, build 135).
                    info!(
                        "QUIC: a direct connection from {from} refused - this server takes connections through the relay only ({} datagram(s) dropped)",
                        log.since_line
                    );
                    log.last_line = Some(Instant::now());
                    log.since_line = 0;
                }
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            other => other,
        }
    }

    fn poll_relay(&self, cx: &mut Context, bufs: &mut [IoSliceMut<'_>], meta: &mut [RecvMeta]) -> Poll<io::Result<usize>> {
        let Some(leg) = &self.relay else {
            return Poll::Pending;
        };
        loop {
            match leg.poll_recv(cx, &mut bufs[0]) {
                Poll::Ready(Ok(Arrived::Relay(client_id, len))) => {
                    // Only a client that ever came over TCP needs its UDP
                    // stamped: one atomic load for every other (review 145).
                    if self.tcp_in_at[client_id as usize].load(Ordering::Relaxed) != 0 {
                        self.udp_in_at[client_id as usize].store(self.born.elapsed().as_millis() as u64 + 1, Ordering::Relaxed);
                    }
                    if let Some(p) = self.punched.lock().unwrap().get(&client_id) {
                        p.stamp(&p.last_relay);
                    }
                    fill(&mut meta[0], relay_peer_for(client_id), len);
                    return Poll::Ready(Ok(1));
                }
                // Over the relay's TCP line (build 144): the same client, at the
                // same address for quinn - and from now on answered that way too.
                Poll::Ready(Ok(Arrived::Tcp(client_id, len))) => {
                    self.tcp_came(client_id);
                    if let Some(p) = self.punched.lock().unwrap().get(&client_id) {
                        p.stamp(&p.last_relay);
                    }
                    fill(&mut meta[0], relay_peer_for(client_id), len);
                    return Poll::Ready(Ok(1));
                }
                // The server's leg has no direct target, so nothing comes this
                // way; read on rather than hand quinn an empty datagram.
                Poll::Ready(Ok(Arrived::Direct(_))) => continue,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    /// The punched sockets: a QUIC packet from exactly the client's public
    /// address goes to quinn under the client's relay address - the same
    /// connection, by another way. The rest is dropped; a known IP from another
    /// port is said once (the client's router did not keep its port).
    fn poll_punched(&self, cx: &mut Context, bufs: &mut [IoSliceMut<'_>], meta: &mut [RecvMeta]) -> Poll<io::Result<usize>> {
        let map = self.punched.lock().unwrap();
        let n = map.len();
        if n == 0 {
            return Poll::Pending;
        }
        let start = self.next_punched.fetch_add(1, Ordering::Relaxed) as usize % n;
        for (&id, p) in map.iter().cycle().skip(start).take(n) {
            loop {
                let mut rb = ReadBuf::new(&mut bufs[0]);
                match p.socket.poll_recv_from(cx, &mut rb) {
                    Poll::Ready(Ok(src)) => {
                        let len = rb.filled().len();
                        if same(src, p.target) {
                            if p.ago(&p.last_direct).is_none() {
                                info!("QUIC punch: client {id} reached directly after {} ms", p.since.elapsed().as_millis());
                            }
                            p.stamp(&p.last_direct);
                            self.count(|c| &c.direct_in);
                            fill(&mut meta[0], relay_peer_for(id), len);
                            return Poll::Ready(Ok(1));
                        }
                        if src.ip() == p.target.ip() {
                            if !p.port_logged.swap(true, Ordering::Relaxed) {
                                info!("QUIC punch: client {id} came from another port than the relay saw - its router changed the port");
                            }
                        } else {
                            self.count(|c| &c.strangers);
                        }
                    }
                    // An ICMP error for a probe or a copy that bounced: one per
                    // datagram, then the next read goes on - the same measured
                    // behaviour as in `RelayLeg::poll_recv` (see there).
                    Poll::Ready(Err(e)) => debug!("QUIC punch: receive error: {e}"),
                    Poll::Pending => break,
                }
            }
        }
        Poll::Pending
    }

    fn count(&self, which: impl Fn(&LegCounts) -> &AtomicU64) {
        if let Some(leg) = &self.relay {
            which(&leg.counts).fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Probe every [`PUNCH_EVERY`] until QUIC comes directly, then keep watch. A
/// punch that is not answered within [`GIVE_UP`], or a direct path that has
/// been silent for as long, ends: the client is back on the relay only, and
/// this side stops sending a copy to an address nobody reads.
async fn keep_punching(route: Weak<ServerRouteSocket>, id: u8, p: Arc<Punched>) {
    let mut probes = 1u32;
    loop {
        tokio::time::sleep(PUNCH_EVERY).await;
        if p.ended.load(Ordering::Relaxed) {
            return;
        }
        let why = match p.ago(&p.last_direct) {
            None if probes >= PUNCH_PROBES => Some("was not reached directly"),
            None => {
                let _ = p.socket.try_send_to(&PUNCH_PROBE, p.target);
                probes += 1;
                None
            }
            Some(ago) if ago >= GIVE_UP => Some("went silent on the direct path"),
            Some(_) => None,
        };
        if let Some(why) = why {
            info!("QUIC punch: client {id} {why} for {} s - back to the relay", GIVE_UP.as_secs());
            p.ended.store(true, Ordering::Relaxed);
            if let Some(route) = route.upgrade() {
                route.punched.lock().unwrap().retain(|_, e| !Arc::ptr_eq(e, &p));
            }
            return;
        }
    }
}

impl AsyncUdpSocket for ServerRouteSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        // Writability is the direct socket's; relay and punched sends never wait.
        self.direct.clone().create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        match (relay_peer_id(transmit.destination), &self.relay) {
            (Some(client_id), Some(leg)) => {
                let punched = self.punched.lock().unwrap().get(&client_id).cloned();
                let tcp = self.tcp_recent(client_id);
                // Over UDP unless the client is heard over TCP and not over UDP
                // (build 145): then a UDP copy never arrives. A punched client
                // has UDP by definition.
                if !tcp || punched.is_some() || self.udp_recent(client_id) {
                    match punched {
                        Some(p) => {
                            if !p.direct_only() {
                                leg.send(client_id, transmit.contents);
                            }
                            leg.send_direct(&p.socket, p.target, transmit.contents);
                        }
                        None => leg.send(client_id, transmit.contents),
                    }
                }
                if tcp {
                    leg.send_tcp(client_id, transmit.contents);
                }
                Ok(())
            }
            (Some(_), None) => Ok(()), // no relay set: nowhere to go
            (None, _) => self.direct.try_send(transmit),
        }
    }

    fn poll_recv(&self, cx: &mut Context, bufs: &mut [IoSliceMut<'_>], meta: &mut [RecvMeta]) -> Poll<io::Result<usize>> {
        *self.recv_waker.lock().unwrap() = Some(cx.waker().clone());
        let first = self.next_source.fetch_add(1, Ordering::Relaxed);
        for i in 0..3 {
            let polled = match first.wrapping_add(i) % 3 {
                0 if self.relay_only => self.poll_direct_refused(cx, bufs, meta),
                0 => self.direct.poll_recv(cx, bufs, meta),
                1 => self.poll_relay(cx, bufs, meta),
                _ => self.poll_punched(cx, bufs, meta),
            };
            if polled.is_ready() {
                return polled;
            }
        }
        Poll::Pending
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.direct.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        // One datagram per send: a relay datagram needs its own header, and a
        // batch is sent to one destination - better never to batch at all. This
        // holds for the direct clients too while the relay is on (review 117:
        // a price nobody chose, written down in the register).
        1
    }

    fn max_receive_segments(&self) -> usize {
        // The direct socket may hand over a batch (GRO on Linux); quinn sizes
        // its buffers by this, and a relay datagram fills one stride.
        self.direct.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.direct.may_fragment()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The punch itself, which no test over localhost can see working (review
    /// 125: every probe could go and the suite stayed green): a fresh socket,
    /// its first probe out before the answer is back (well within the 200 ms
    /// of the next one), the answer naming that socket's port and this side's
    /// public IP, the probes repeated, the same answer to the same offer - and
    /// no punch to an address of the other family or one that is no place.
    #[tokio::test]
    async fn the_server_punches_from_a_fresh_socket_and_says_which() {
        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let ours: SocketAddr = "127.0.0.1:7777".parse().unwrap();
        let grant = UdpGrant { relay: relay.local_addr().unwrap(), fallback: None, public: Some(ours), token: [5; 32], session: 1 };
        let (_tx, rx) = watch::channel(Some(grant));
        let runtime = quinn::default_runtime().unwrap();
        let direct = runtime.wrap_udp_socket(std::net::UdpSocket::bind("127.0.0.1:0").unwrap()).unwrap();
        let direct_port = direct.local_addr().unwrap().port();
        let route = ServerRouteSocket::new(direct, Some(RelayRoute::new(rx, None)), false);
        let t0 = std::time::Instant::now();
        while route.relay.as_ref().unwrap().public().is_none() {
            assert!(t0.elapsed() < Duration::from_secs(2), "the leg never took the grant");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let at = client.local_addr().unwrap();
        let ready = route.punch(3, at, 1).expect("an answer");
        let mut buf = [0u8; 16];
        let (n, from) = tokio::time::timeout(Duration::from_millis(100), client.recv_from(&mut buf))
            .await
            .expect("the first probe went out with the answer, not after it")
            .unwrap();
        assert_eq!(&buf[..n], &PUNCH_PROBE);
        // The client's IP is this side's own here (both loopback), yet a punch:
        // loopback is no home network (L+R, build 142).
        assert_eq!(ready, Answer::Punched(SocketAddr::new(ours.ip(), from.port())), "the answer names our IP and the probing socket's port");
        assert_ne!(from.port(), direct_port, "a fresh socket, not the server's own");
        let (n, again) = tokio::time::timeout(Duration::from_millis(500), client.recv_from(&mut buf)).await.expect("probed again").unwrap();
        assert_eq!((&buf[..n], again), (&PUNCH_PROBE[..], from));
        assert_eq!(route.punch(3, at, 1), Some(ready), "the same offer, the same answer");
        assert_eq!(route.punches(), 1);
        for nowhere in ["[::1]:5000", "0.0.0.0:5000", "224.0.0.1:5000", "255.255.255.255:5000", "127.0.0.1:0"] {
            assert_eq!(route.punch(4, nowhere.parse().unwrap(), 2), None, "{nowhere}");
        }
        assert_eq!(route.punches(), 1, "nothing opened for those");
        // A proof ends with its connection: quinn reuses a closed connection's
        // `stable_id`, and a new one must not find the old proof (review 125).
        // An offer before the proof is said once per connection, and again on a
        // connection that comes after (review 127).
        assert!(route.first_unproven_offer(5));
        assert!(!route.first_unproven_offer(5), "once");
        route.forget(5);
        assert!(route.first_unproven_offer(5), "a later connection with the same id is told again");
        route.revoke(5);
        assert!(route.first_unproven_offer(5), "and so is a new stretch after its session ended (review 128)");
        route.prove(7);
        route.revoke(7);
        assert!(!route.is_proven(7), "a permit taken back with its session");
        route.prove(1);
        assert!(route.is_proven(1));
        route.forget(1);
        assert!(!route.is_proven(1), "the proof went with the connection");
        assert_eq!(route.punches(), 0, "and so did the punch");
    }

    /// L+R (build 142): a client at this side's own public IP is behind the
    /// same router, and gets this side's own port at its local address - no
    /// punch, and a punch it had ends. A client at another IP is punched for,
    /// on the same server. Twice (rule 9): the same offer, the same answer.
    #[tokio::test]
    async fn behind_the_same_router_the_answer_is_the_own_port() {
        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let ours: SocketAddr = "127.0.0.1:7777".parse().unwrap();
        let grant = UdpGrant { relay: relay.local_addr().unwrap(), fallback: None, public: Some(ours), token: [6; 32], session: 1 };
        let (_tx, rx) = watch::channel(Some(grant));
        let runtime = quinn::default_runtime().unwrap();
        let direct = runtime.wrap_udp_socket(std::net::UdpSocket::bind("127.0.0.1:0").unwrap()).unwrap();
        let own = direct.local_addr().unwrap();
        let route = ServerRouteSocket::new(direct, Some(RelayRoute::new(rx, None)), false);
        route.loopback_is_home.store(true, Ordering::Relaxed);
        let t0 = std::time::Instant::now();
        while route.relay.as_ref().unwrap().public().is_none() {
            assert!(t0.elapsed() < Duration::from_secs(2), "the leg never took the grant");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let elsewhere: SocketAddr = "127.0.0.2:40000".parse().unwrap();
        assert!(matches!(route.punch(3, elsewhere, 1), Some(Answer::Punched(_))), "another public IP: a punch");
        assert_eq!(route.punches(), 1);
        let at_home: SocketAddr = "127.0.0.1:40001".parse().unwrap();
        for round in 0..2 {
            assert_eq!(route.punch(3, at_home, 1), Some(Answer::Local(own)), "round {round}: the own port at the local address");
            assert_eq!(route.punches(), 0, "round {round}: no punch, and the one it had ended");
        }
        // The same connection under another client id (a network switch where
        // the relay gave it a new one), then home: its old punch ends too
        // (review 142: the L+R answer skipped this).
        assert!(matches!(route.punch(3, elsewhere, 1), Some(Answer::Punched(_))));
        assert_eq!(route.punch(4, at_home, 1), Some(Answer::Local(own)), "home under a new id");
        assert_eq!(route.punches(), 0, "the punch under the old id ended");
    }

    /// Which local address may be given out (reviews 142): every condition of
    /// `is_home` decides one row.
    #[test]
    fn only_a_local_ipv4_address_is_home() {
        let ip = |s: &str| -> IpAddr { s.parse().unwrap() };
        assert!(is_home(ip("192.168.1.97"), false), "the own network");
        assert!(is_home(ip("10.0.0.5"), false));
        assert!(!is_home(ip("203.0.113.97"), false), "a public address on the card");
        assert!(!is_home(ip("fd12::1"), false), "IPv6, even a local one");
        assert!(!is_home(ip("127.0.0.1"), false), "loopback is a test");
        assert!(is_home(ip("127.0.0.1"), true), "unless the test says so");
    }

    #[test]
    fn a_relay_client_keeps_one_address_per_id() {
        for id in [0u8, 1, 7, 253] {
            assert_eq!(relay_peer_id(relay_peer_for(id)), Some(id));
        }
        assert_ne!(relay_peer_for(0), relay_peer_for(1));
        assert_eq!(relay_peer_id("192.0.2.1:4581".parse().unwrap()), None, "another port is a real address");
    }

    /// The three made-up address ranges never overlap: the QUIC sentinels the
    /// server's sessions see, the old relay's sentinels, and the relay peers
    /// quinn sees here. Checked with the functions themselves, so a range that
    /// moves cannot slip past (review 117).
    #[test]
    fn the_three_made_up_ranges_stay_apart() {
        for id in 0..=255u8 {
            let peer = relay_peer_for(id);
            assert!(!crate::is_quic_sentinel(peer), "{peer}");
            assert_eq!(relay_peer_id(sdr_remote_relay::sentinel_for_client_id(id)), None);
            assert_ne!(peer.ip(), sdr_remote_relay::sentinel_for_client_id(id).ip());
        }
        for n in [0u32, 1, 65534, 65535, 1_000_000] {
            let s = crate::sentinel(n);
            assert_eq!(relay_peer_id(s), None, "{s}");
            assert!(crate::is_quic_sentinel(s));
        }
    }
}

#[cfg(test)]
mod leg_counts {
    use super::LegCounts;
    use std::sync::atomic::Ordering;

    /// A window is what came in since the last one, and taking it starts the next.
    #[test]
    fn a_window_counts_and_starts_again() {
        let c = LegCounts::default();
        c.to_relay.fetch_add(5, Ordering::Relaxed);
        c.busy.fetch_add(2, Ordering::Relaxed);
        c.failed.fetch_add(1, Ordering::Relaxed);
        c.no_session.fetch_add(3, Ordering::Relaxed);
        c.from_relay.fetch_add(7, Ordering::Relaxed);
        c.direct_out.fetch_add(11, Ordering::Relaxed);
        c.direct_dropped.fetch_add(13, Ordering::Relaxed);
        c.direct_in.fetch_add(17, Ordering::Relaxed);
        c.strangers.fetch_add(19, Ordering::Relaxed);
        c.tcp_out.fetch_add(23, Ordering::Relaxed);
        c.tcp_dropped.fetch_add(29, Ordering::Relaxed);
        c.tcp_in.fetch_add(31, Ordering::Relaxed);
        assert_eq!(c.take(), [5, 2, 1, 3, 7, 11, 13, 17, 19, 23, 29, 31]);
        assert_eq!(c.take(), [0; 12], "the next window starts empty");
    }

    /// A leg without a relay session counts what it could not send - a relay
    /// outage takes the grant away, and those datagrams went uncounted
    /// (review 122).
    #[tokio::test]
    async fn a_leg_without_a_session_counts_what_it_drops() {
        let (_tx, grant) = tokio::sync::watch::channel(None);
        let leg = super::RelayLeg::start(super::RelayRoute::new(grant, None));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        leg.counts.take();
        leg.send(1, b"for client 0");
        leg.send(2, b"for client 1");
        let [to, _, _, no_session, ..] = leg.counts.take();
        assert_eq!((to, no_session), (0, 2));
    }

    /// The keepalive that binds a new session's socket at the relay goes at
    /// once, at the first session and at the next - not dropped as "busy" and
    /// sent again at the next tick, up to a second later. A socket fresh in
    /// tokio refused its first send (measured 2026-10-07); at the first session
    /// the interval's first tick hid it, at a network change nothing did.
    #[tokio::test]
    async fn a_new_session_binds_its_socket_at_once() {
        use super::{RelayLeg, RelayRoute, UdpGrant};
        use std::sync::atomic::Ordering;
        use std::time::Duration;
        use tokio::net::UdpSocket;
        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let grant = UdpGrant { relay: relay.local_addr().unwrap(), fallback: None, public: None, token: [3; 32], session: 1 };
        let (tx, rx) = tokio::sync::watch::channel(Some(grant));
        let leg = RelayLeg::start(RelayRoute::new(rx, None));
        let mut buf = [0u8; 128];
        let (_, first) = tokio::time::timeout(Duration::from_secs(2), relay.recv_from(&mut buf)).await.unwrap().unwrap();
        // Past the interval's own first tick, so the next session meets a running clock.
        tokio::time::sleep(Duration::from_millis(300)).await;
        while tokio::time::timeout(Duration::from_millis(50), relay.recv_from(&mut buf)).await.is_ok() {}
        tx.send(Some(UdpGrant { token: [4; 32], session: 2, ..grant })).unwrap();
        let t0 = std::time::Instant::now();
        let second = loop {
            let (_, from) = tokio::time::timeout(Duration::from_millis(400), relay.recv_from(&mut buf))
                .await
                .expect("the new session's keepalive went at once, not at the next tick")
                .unwrap();
            if from != first {
                break from;
            }
        };
        assert!(t0.elapsed() < Duration::from_millis(400), "from the new socket: {second}");
        assert_eq!(leg.counts.busy.load(Ordering::Relaxed), 0, "no keepalive dropped as busy");
    }

    /// The offers slow down after ten and never stop (review 126: a 2FA code
    /// typed for longer than ten seconds left the client without a punch, and
    /// nothing saw it): the interval below and at the limit, and a leg that has
    /// offered fifty times offers again.
    #[tokio::test]
    async fn the_offers_slow_down_and_never_stop() {
        use super::{offer_interval, RelayLeg, RelayRoute, UdpGrant, MAX_OFFERS, OFFER_EVERY_SLOW};
        use crate::punch::OFFER_EVERY;
        use std::time::Duration;
        use tokio::net::UdpSocket;
        assert_eq!(offer_interval(0), OFFER_EVERY);
        assert_eq!(offer_interval(MAX_OFFERS - 1), OFFER_EVERY);
        assert_eq!(offer_interval(MAX_OFFERS), OFFER_EVERY_SLOW);
        assert_eq!(offer_interval(u32::MAX), OFFER_EVERY_SLOW);
        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let public: std::net::SocketAddr = "127.0.0.1:6000".parse().unwrap();
        let grant = UdpGrant { relay: relay.local_addr().unwrap(), fallback: None, public: Some(public), token: [8; 32], session: 1 };
        let (_tx, rx) = tokio::sync::watch::channel(Some(grant));
        let leg = RelayLeg::start(RelayRoute::new(rx, None));
        let t0 = std::time::Instant::now();
        while leg.public().is_none() {
            assert!(t0.elapsed() < Duration::from_secs(2));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // The leg waits the slow interval, not one second, once it is there:
        // an offer a second ago is too recent (review 127: the interval itself
        // was pinned, its use was not).
        {
            let mut live = leg.live.lock().unwrap();
            let p = &mut live.as_mut().unwrap().punch;
            p.offers = MAX_OFFERS;
            p.last_offer = Some(std::time::Instant::now() - OFFER_EVERY - Duration::from_millis(50));
        }
        assert_eq!(leg.offer_due(), None, "a second after the tenth: still waiting");
        for offers in [MAX_OFFERS, 50] {
            {
                let mut live = leg.live.lock().unwrap();
                let p = &mut live.as_mut().unwrap().punch;
                p.offers = offers;
                p.last_offer = Some(std::time::Instant::now() - OFFER_EVERY_SLOW);
            }
            assert_eq!(leg.offer_due(), Some(public), "after {offers} offers: one more");
            assert_eq!(leg.offer_due(), None, "and not again at once");
        }
    }

    /// An answer the client's socket cannot use - the other family (review
    /// 125: this check had never run), or no place at all - keeps it on the
    /// relay: no direct target, and no more offers in this session.
    #[tokio::test]
    async fn an_answer_the_socket_cannot_use_keeps_the_client_on_the_relay() {
        use super::{RelayLeg, RelayRoute, UdpGrant};
        use std::time::Duration;
        use tokio::net::UdpSocket;
        for answer in ["[::1]:5000", "0.0.0.0:5000", "127.0.0.1:0"] {
            let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let public: std::net::SocketAddr = "127.0.0.1:6000".parse().unwrap();
            let grant = UdpGrant { relay: relay.local_addr().unwrap(), fallback: None, public: Some(public), token: [9; 32], session: 1 };
            let (_tx, rx) = tokio::sync::watch::channel(Some(grant));
            let leg = RelayLeg::start(RelayRoute::new(rx, None));
            let t0 = std::time::Instant::now();
            while leg.public().is_none() {
                assert!(t0.elapsed() < Duration::from_secs(2));
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(leg.offer_due(), Some(public));
            leg.ready(answer.parse().unwrap());
            let (direct, gave_up) = {
                let live = leg.live.lock().unwrap();
                let p = &live.as_ref().unwrap().punch;
                (p.direct.is_some(), p.gave_up)
            };
            assert_eq!((direct, gave_up), (false, true), "{answer}");
        }
    }

    /// A new public address in the session empties the punch state, and the
    /// leg says the way at once - not at the next offer tick, which this test
    /// never runs (review 140: `follow` emptied it and said nothing, so the old
    /// way stood over a network switch for up to 250 ms).
    #[tokio::test]
    async fn a_new_public_address_says_relay_at_once() {
        use super::{ClientPath, RelayLeg, RelayRoute, UdpGrant};
        use std::time::Duration;
        use tokio::net::UdpSocket;

        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let grant = UdpGrant { relay: relay.local_addr().unwrap(), fallback: None, public: None, token: [7; 32], session: 1 };
        let (tx, rx) = tokio::sync::watch::channel(Some(grant));
        let route = RelayRoute::new(rx, None);
        let leg_says = route.clone();
        let leg = RelayLeg::start(route);
        let mut buf = [0u8; 128];
        let (_, leg_addr) = tokio::time::timeout(Duration::from_secs(2), relay.recv_from(&mut buf)).await.unwrap().unwrap();
        let leg_addr = std::net::SocketAddr::from(([127, 0, 0, 1], leg_addr.port()));
        tx.send(Some(UdpGrant { public: Some(leg_addr), ..grant })).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(leg.offer_due(), Some(leg_addr));
        leg.ready(peer.local_addr().unwrap());
        assert_eq!(leg_says.path(), ClientPath::Coupling);
        // The network changed under the same relay session: a new public address.
        tx.send(Some(UdpGrant { public: Some("127.0.0.1:9".parse().unwrap()), ..grant })).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(leg_says.path(), ClientPath::Relay, "the old way stood after the new address");
        // The other branches, each from a way being tried (review 141: one
        // branch was driven, and half the repair stayed green): a new session
        // - the branch of a network switch - and then no session at all.
        let couple = |what: &str| {
            leg.live.lock().unwrap().as_mut().expect("a session").punch.offers = 1;
            leg.ready(peer.local_addr().unwrap());
            assert_eq!(leg_says.path(), ClientPath::Coupling, "{what}: coupling");
        };
        couple("before the new session");
        tx.send(Some(UdpGrant { session: 2, token: [9; 32], ..grant })).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(leg_says.path(), ClientPath::Relay, "a new session kept the old way");
        couple("before the gap");
        tx.send(None).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(leg_says.path(), ClientPath::Relay, "no session kept the old way");
    }

    /// A local answer (L+R, build 142) is taken like a punch and named local:
    /// coupling while it is tried, `Local` once the server's own port answered
    /// - at once, and at the next offer tick alike. An answer that names no
    /// local address is not taken, and that session stays on the relay.
    #[tokio::test]
    async fn a_local_answer_is_named_local() {
        use super::{Arrived, ClientPath, RelayLeg, RelayRoute, UdpGrant};
        use std::future::poll_fn;
        use std::time::Duration;
        use tokio::net::UdpSocket;

        type Grant = tokio::sync::watch::Sender<Option<UdpGrant>>;
        async fn leg_with_public(relay: &UdpSocket, token: u8) -> (std::sync::Arc<RelayLeg>, RelayRoute, std::net::SocketAddr, Grant) {
            let public: std::net::SocketAddr = "127.0.0.1:6000".parse().unwrap();
            let grant = UdpGrant { relay: relay.local_addr().unwrap(), fallback: None, public: Some(public), token: [token; 32], session: 1 };
            let (tx, rx) = tokio::sync::watch::channel(Some(grant));
            let route = RelayRoute::new(rx, None);
            let leg = RelayLeg::start(route.clone());
            let mut buf = [0u8; 128];
            let (_, at) = tokio::time::timeout(Duration::from_secs(2), relay.recv_from(&mut buf)).await.unwrap().unwrap();
            assert_eq!(leg.offer_due(), Some(public));
            (leg, route, std::net::SocketAddr::from(([127, 0, 0, 1], at.port())), tx)
        }

        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (leg, says, _, _grant) = leg_with_public(&relay, 4).await;
        leg.ready_local("8.8.8.8:4580".parse().unwrap());
        assert_eq!(says.path(), ClientPath::Relay, "an address on the internet is no local answer");
        assert_eq!(leg.offer_due(), None, "and that session offers no more");

        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (leg, says, leg_addr, _grant) = leg_with_public(&relay, 5).await;
        leg.ready_local(server.local_addr().unwrap());
        assert_eq!(says.path(), ClientPath::Coupling, "tried");
        server.send_to(b"quic", leg_addr).await.unwrap();
        let mut out = [0u8; 64];
        let got = tokio::time::timeout(Duration::from_secs(2), poll_fn(|cx| leg.poll_recv(cx, &mut out))).await.unwrap().unwrap();
        assert!(matches!(got, Arrived::Direct(4)), "the server's own port gets through");
        assert_eq!(says.path(), ClientPath::Local, "confirmed: local, not direct");
        assert_eq!(leg.offer_due(), None);
        assert_eq!(says.path(), ClientPath::Local, "and so at the next tick");
    }

    /// The TCP line on a client's leg (build 144): every packet goes over it as
    /// well until something came back over UDP from the relay in this session -
    /// then no more; a new session (a network switch) starts over. What comes in
    /// over the line is handed on, with its client id.
    #[tokio::test]
    async fn a_client_copies_over_tcp_until_udp_works_in_the_session() {
        use super::{Arrived, RelayLeg, RelayRoute, TcpLine, UdpGrant};
        use std::future::poll_fn;
        use std::time::Duration;
        use tokio::net::UdpSocket;

        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let grant = UdpGrant { relay: relay.local_addr().unwrap(), fallback: None, public: None, token: [3; 32], session: 1 };
        let (tx, rx) = tokio::sync::watch::channel(Some(grant));
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(16);
        let (in_tx, in_rx) = tokio::sync::mpsc::channel(16);
        let leg = RelayLeg::start(RelayRoute::new(rx, Some(TcpLine { to_relay: out_tx, from_relay: in_rx, dropped_out: Default::default() })));
        let copy = |out_rx: &mut tokio::sync::mpsc::Receiver<Vec<u8>>| {
            out_rx.try_recv().ok().map(|f| sdr_remote_relay::tlt1_parts(&f).map(|(c, p)| (c, p.to_vec())).unwrap())
        };
        // The leg's socket in `session`, once it stands.
        let socket_of = |session: u64| {
            let leg = leg.clone();
            async move {
                let t0 = std::time::Instant::now();
                loop {
                    let now = leg.live.lock().unwrap().as_ref().map(|l| (l.session, l.socket.local_addr().unwrap().port()));
                    if let Some((s, port)) = now {
                        if s == session {
                            return std::net::SocketAddr::from(([127, 0, 0, 1], port));
                        }
                    }
                    assert!(t0.elapsed() < Duration::from_secs(2), "no socket for session {session}");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        };
        let mut out = [0u8; 64];
        for session in 1..=2u64 {
            let leg_addr = socket_of(session).await;
            leg.send_client(b"before");
            assert_eq!(copy(&mut out_rx), Some((0, b"before".to_vec())), "session {session}: a copy over TCP, as client id 0");
            // A bare keepalive from the relay: UDP works in this session.
            relay.send_to(&[0], leg_addr).await.unwrap();
            let _ = tokio::time::timeout(Duration::from_millis(200), poll_fn(|cx| leg.poll_recv(cx, &mut out))).await;
            leg.send_client(b"after");
            assert_eq!(copy(&mut out_rx), None, "session {session}: no copy once UDP works");
            // A network switch: a new session, and UDP is unproven again.
            tx.send(Some(UdpGrant { session: session + 1, token: [session as u8 + 3; 32], ..grant })).unwrap();
        }
        socket_of(3).await;
        leg.send_client(b"third");
        assert_eq!(copy(&mut out_rx), Some((0, b"third".to_vec())), "the new session copies again");
        in_tx.send((5, b"back".to_vec())).await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(2), poll_fn(|cx| leg.poll_recv(cx, &mut out))).await.unwrap().unwrap();
        assert!(matches!(got, Arrived::Tcp(5, 4)) && &out[..4] == b"back", "what comes in over TCP goes on, with its id");
    }

    /// The client's name for the TCP line (build 146): `R (TCP)` once UDP has
    /// not been seen to work for `RELAY_QUIET` after the session began - not
    /// before, the first answers of every session come over TCP - and back to
    /// `R` when it is; and at once with "force TCP", which also sends nothing
    /// over UDP. A leg without a TCP line never says it.
    #[tokio::test]
    async fn a_client_heard_over_tcp_only_is_named_r_tcp() {
        use super::{ClientPath, RelayLeg, RelayRoute, TcpLine, UdpGrant, RELAY_QUIET, TLU1_HEADER_LEN};
        use std::future::poll_fn;
        use std::time::Duration;
        use tokio::net::UdpSocket;

        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let grant = UdpGrant { relay: relay.local_addr().unwrap(), fallback: None, public: None, token: [6; 32], session: 1 };
        let (_tx, rx) = tokio::sync::watch::channel(Some(grant));
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(16);
        let (_in_tx, in_rx) = tokio::sync::mpsc::channel(16);
        let route = RelayRoute::new(rx, Some(TcpLine { to_relay: out_tx, from_relay: in_rx, dropped_out: Default::default() }));
        let says = route.clone();
        let leg = RelayLeg::start(route);
        let mut buf = [0u8; 128];
        let (_, at) = tokio::time::timeout(Duration::from_secs(2), relay.recv_from(&mut buf)).await.unwrap().unwrap();
        let leg_addr = std::net::SocketAddr::from(([127, 0, 0, 1], at.port()));
        let _ = leg.offer_due();
        assert_eq!(says.path(), ClientPath::Relay, "not at once");
        tokio::time::sleep(RELAY_QUIET + Duration::from_millis(100)).await;
        let _ = leg.offer_due();
        assert_eq!(says.path(), ClientPath::RelayTcp, "no UDP for RELAY_QUIET");
        relay.send_to(&[0], leg_addr).await.unwrap();
        let mut out = [0u8; 64];
        let _ = tokio::time::timeout(Duration::from_millis(200), poll_fn(|cx| leg.poll_recv(cx, &mut out))).await;
        let _ = leg.offer_due();
        assert_eq!(says.path(), ClientPath::Relay, "UDP works");
        // Force TCP: the name at once, the TCP line only, nothing over UDP.
        says.force_tcp.store(true, Ordering::Relaxed);
        let _ = leg.offer_due();
        assert_eq!(says.path(), ClientPath::RelayTcp, "forced");
        while out_rx.try_recv().is_ok() {}
        while tokio::time::timeout(Duration::from_millis(50), relay.recv_from(&mut buf)).await.is_ok() {}
        leg.send_client(b"forced");
        assert!(out_rx.try_recv().is_ok(), "forced: over the TCP line");
        let udp = tokio::time::timeout(Duration::from_millis(100), relay.recv_from(&mut buf)).await;
        assert!(udp.map_or(true, |r| r.unwrap().0 <= TLU1_HEADER_LEN), "forced: nothing but a keepalive over UDP");
        says.force_tcp.store(false, Ordering::Relaxed);
        let _ = leg.offer_due();
        assert_eq!(says.path(), ClientPath::Relay, "unforced");
        // And "force TCP" holds "relay only": no offer to punch while it is on.
        let public: std::net::SocketAddr = "127.0.0.1:6000".parse().unwrap();
        _tx.send_modify(|g| {
            if let Some(g) = g {
                g.public = Some(public);
            }
        });
        let t0 = std::time::Instant::now();
        while leg.public().is_none() {
            assert!(t0.elapsed() < Duration::from_secs(2));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        says.force_tcp.store(true, Ordering::Relaxed);
        assert_eq!(leg.offer_due(), None, "forced: no offer");
        says.force_tcp.store(false, Ordering::Relaxed);
        assert_eq!(leg.offer_due(), Some(public), "unforced: an offer again");

        // Without a TCP line the name never comes.
        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (_tx2, rx2) = tokio::sync::watch::channel(Some(UdpGrant { relay: relay.local_addr().unwrap(), ..grant }));
        let route = RelayRoute::new(rx2, None);
        let says = route.clone();
        let leg = RelayLeg::start(route);
        let _ = tokio::time::timeout(Duration::from_secs(2), relay.recv_from(&mut buf)).await.unwrap().unwrap();
        tokio::time::sleep(RELAY_QUIET + Duration::from_millis(100)).await;
        says.force_tcp.store(true, Ordering::Relaxed);
        let _ = leg.offer_due();
        assert_eq!(says.path(), ClientPath::Relay, "no line, no R (TCP)");
    }

    /// The leg's socket is unconnected (phase 3c), so the source filter is
    /// ours: the relay gets through, and the punched peer once the server has
    /// answered - its probe dropped, its QUIC packet taken; everything else is
    /// dropped and counted. And the client sends both ways until the direct
    /// path answered, then directly only.
    #[tokio::test]
    async fn the_leg_takes_the_relay_and_the_punched_peer_only() {
        use super::{Arrived, RelayLeg, RelayRoute, UdpGrant, PUNCH_PROBE};
        use std::future::poll_fn;
        use std::time::Duration;
        use tokio::net::UdpSocket;

        async fn next(leg: &RelayLeg) -> Option<(bool, Vec<u8>)> {
            let mut out = [0u8; 64];
            let got = tokio::time::timeout(Duration::from_millis(300), poll_fn(|cx| leg.poll_recv(cx, &mut out))).await.ok()?;
            Some(match got.unwrap() {
                Arrived::Relay(_, n) | Arrived::Tcp(_, n) => (true, out[..n].to_vec()),
                Arrived::Direct(n) => (false, out[..n].to_vec()),
            })
        }
        async fn quiet(sock: &UdpSocket, what: &str) {
            let mut buf = [0u8; 128];
            loop {
                match tokio::time::timeout(Duration::from_millis(200), sock.recv_from(&mut buf)).await {
                    Err(_) => return,
                    Ok(r) => assert!(r.unwrap().0 <= sdr_remote_relay::TLU1_HEADER_LEN, "{what}: more than a keepalive"),
                }
            }
        }

        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let stranger = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let grant = UdpGrant { relay: relay.local_addr().unwrap(), fallback: None, public: None, token: [7; 32], session: 1 };
        let (tx, rx) = tokio::sync::watch::channel(Some(grant));
        let route = RelayRoute::new(rx, None);
        let leg_says = route.clone();
        let leg = RelayLeg::start(route);
        let mut buf = [0u8; 128];
        let (_, leg_addr) = tokio::time::timeout(Duration::from_secs(2), relay.recv_from(&mut buf)).await.unwrap().unwrap();
        let leg_addr = std::net::SocketAddr::from(([127, 0, 0, 1], leg_addr.port()));

        // Before any punch: the relay only.
        stranger.send_to(b"from nobody", leg_addr).await.unwrap();
        relay.send_to(b"\x03relayed", leg_addr).await.unwrap();
        assert_eq!(next(&leg).await, Some((true, b"relayed".to_vec())));
        peer.send_to(b"too early", leg_addr).await.unwrap();
        assert_eq!(next(&leg).await, None, "nothing from the peer before the server answered");
        let strangers = leg.counts.strangers.swap(0, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(strangers, 2, "the stranger and the early peer, counted");

        // The relay says where it saw us; an offer goes; the server answers.
        tx.send(Some(UdpGrant { public: Some(leg_addr), ..grant })).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(leg.offer_due(), Some(leg_addr), "an offer of the public address");
        assert_eq!(leg.offer_due(), None, "not twice within a second");
        assert_eq!(leg_says.path(), super::ClientPath::Relay, "an offer alone is still the relay");
        leg.ready(peer.local_addr().unwrap());
        assert_eq!(leg_says.path(), super::ClientPath::Coupling, "the server punched: coupling (build 139)");

        // Not confirmed yet: both ways.
        quiet(&relay, "before the copy").await;
        leg.send_client(b"both");
        let (n, _) = tokio::time::timeout(Duration::from_secs(1), peer.recv_from(&mut buf)).await.unwrap().unwrap();
        assert_eq!(&buf[..n], b"both", "directly, without a header");
        let (n, _) = tokio::time::timeout(Duration::from_secs(1), relay.recv_from(&mut buf)).await.unwrap().unwrap();
        assert!(n > sdr_remote_relay::TLU1_HEADER_LEN && buf[..n].ends_with(b"both"), "and through the relay");

        // The probe is dropped, uncounted; the QUIC packet confirms; a stranger stays out.
        peer.send_to(&PUNCH_PROBE, leg_addr).await.unwrap();
        peer.send_to(b"quic", leg_addr).await.unwrap();
        assert_eq!(next(&leg).await, Some((false, b"quic".to_vec())));
        assert_eq!(leg_says.path(), super::ClientPath::Direct, "the first QUIC packet confirms it");
        stranger.send_to(b"still nobody", leg_addr).await.unwrap();
        assert_eq!(next(&leg).await, None);
        assert_eq!(leg.counts.strangers.load(std::sync::atomic::Ordering::Relaxed), 1, "the probe is not a stranger");

        // Confirmed: directly only.
        leg.send_client(b"direct only");
        let (n, _) = tokio::time::timeout(Duration::from_secs(1), peer.recv_from(&mut buf)).await.unwrap().unwrap();
        assert_eq!(&buf[..n], b"direct only");
        quiet(&relay, "after the confirmation").await;

        // A new connection starts over, and says so at once (review 139).
        leg.restart_punch();
        assert_eq!(leg_says.path(), super::ClientPath::Relay, "a new connection does not keep the old one's way");
    }
}

/// A relay-only station's own port gives up its turn after one batch
/// (review 135). A port that never runs dry stands for a stream of datagrams:
/// one call into the route socket must come back and leave the relay its turn,
/// where reading until empty would never return. Run on a thread of its own so
/// a regression fails on the deadline instead of hanging the suite.
#[cfg(test)]
mod a_flooded_port {
    use super::*;

    /// A socket that always has one more datagram.
    #[derive(Debug)]
    struct NeverDry;

    impl AsyncUdpSocket for NeverDry {
        fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
            Box::pin(AlwaysWritable)
        }
        fn try_send(&self, _transmit: &Transmit) -> io::Result<()> {
            Ok(())
        }
        fn poll_recv(&self, _cx: &mut Context, _bufs: &mut [IoSliceMut<'_>], meta: &mut [RecvMeta]) -> Poll<io::Result<usize>> {
            fill(&mut meta[0], SocketAddr::from(([198, 18, 0, 1], 4580)), 1);
            Poll::Ready(Ok(1))
        }
        fn local_addr(&self) -> io::Result<SocketAddr> {
            Ok(SocketAddr::from(([127, 0, 0, 1], 4580)))
        }
    }

    #[test]
    fn a_flooded_own_port_gives_the_relay_its_turn() {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let polled = rt.block_on(async {
                let (_tx, rx) = watch::channel(None);
                let route = ServerRouteSocket::new(Arc::new(NeverDry), Some(RelayRoute::new(rx, None)), true);
                std::future::poll_fn(|cx| {
                    let mut buf = [0u8; 64];
                    let mut bufs = [IoSliceMut::new(&mut buf)];
                    let mut meta = [RecvMeta::default()];
                    // Every start position of the turn, so the flooded port is
                    // read first at least once.
                    let mut readies = 0;
                    for _ in 0..3 {
                        if route.poll_recv(cx, &mut bufs, &mut meta).is_ready() {
                            readies += 1;
                        }
                    }
                    Poll::Ready(readies)
                })
                .await
            });
            let _ = done_tx.send(polled);
        });
        let readies = done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a call into the route socket did not come back: the flooded port kept its turn");
        assert_eq!(readies, 0, "nothing from the flooded port reached quinn");
    }
}

/// End to end through a relay that does what `thetislink-relay`'s `serve_udp`
/// does: a token per slot, bound to the first source it is used from; a new
/// connection for a slot revokes the tokens of the older one (`revoke_superseded`);
/// the destination is the newest connection's token even before it has a source
/// (`udp_target`); the payload forwarded unread as `[client_id] + payload`. And a
/// source that "dies" the way a NAT mapping does after a network change.
#[cfg(test)]
mod through_a_relay {
    use super::*;
    use crate::testing::FakeRelay;
    use sdr_remote_core::path::PathKind;
    use crate::{Congestion, QuicClient, QuicServer, QuicServerLink};
    use std::time::Instant;
    use tokio::sync::mpsc;

    const WAIT: Duration = Duration::from_secs(5);
    /// What the engine sends to over the relay: an address of its own choosing.
    const SERVER: &str = "203.0.113.1:4581";

    /// A server behind the relay that answers every packet with "re:" + it -
    /// and, with a gate, admits each connection at its first packet the way the
    /// server's session layer does once the password is proven.
    fn echo(link: QuicServerLink, gate: Option<crate::PunchGate>) -> mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)> {
        let QuicServerLink { mut inbound_rx, uplink_tx } = link;
        let (seen_tx, seen_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some((tag, packet)) = inbound_rx.recv().await {
                if packet.is_empty() {
                    continue; // a close
                }
                if let Some(gate) = &gate {
                    gate.allow(tag);
                }
                let _ = seen_tx.send((tag, packet.clone()));
                let mut back = b"re:".to_vec();
                back.extend_from_slice(&packet);
                let _ = uplink_tx.send((tag, back));
            }
        });
        seen_rx
    }

    type Seen = mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)>;

    fn station(relay: &FakeRelay) -> (QuicServer, Seen, watch::Sender<Option<UdpGrant>>, UdpGrant) {
        let grant = relay.connect(true, 0);
        let (tx, rx) = watch::channel(Some(grant));
        let (server, link) =
            QuicServer::start_with(SocketAddr::from(([127, 0, 0, 1], 0)), Congestion::Bbr, Some(RelayRoute::new(rx, None))).unwrap();
        let seen = echo(link, Some(server.punch_gate()));
        (server, seen, tx, grant)
    }

    fn client(relay: &FakeRelay, client_id: u8) -> (QuicClient, watch::Sender<Option<UdpGrant>>, UdpGrant) {
        let grant = relay.connect(false, client_id);
        let (tx, rx) = watch::channel(Some(grant));
        let c = QuicClient::start_with(Congestion::Bbr, Some(RelayRoute::new(rx, None))).unwrap();
        (c, tx, grant)
    }

    /// Send `packet` to the server's address until "re:" + it comes back, tagged
    /// with that same address. Sent again every 300 ms, as the engine keeps
    /// sending: a datagram is not repeated by QUIC, and one sent in the moment a
    /// new socket replaces the old one is lost - by design, as audio would be.
    async fn ask(client: &mut QuicClient, packet: &[u8]) {
        let server: SocketAddr = SERVER.parse().unwrap();
        let t0 = Instant::now();
        loop {
            assert!(t0.elapsed() < WAIT, "no answer to a {} B packet after {:?}", packet.len(), t0.elapsed());
            client.uplink_tx.send((server, packet.to_vec())).unwrap();
            let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
            while let Ok(Some((from, back))) = tokio::time::timeout_at(deadline, client.inbound_rx.recv()).await {
                assert_eq!(from, server, "tagged with the address the engine sent to");
                if back[3..] == *packet {
                    return;
                }
            }
        }
    }

    /// The sentinel the server heard `packet` from (the first time; a packet
    /// may be sent more than once).
    async fn tag_of(seen: &mut Seen, packet: &[u8]) -> SocketAddr {
        loop {
            let (tag, p) = tokio::time::timeout(WAIT, seen.recv()).await.expect("the server heard it").unwrap();
            if p == packet {
                return tag;
            }
        }
    }

    /// A client reaches the station through the relay and back, and the two
    /// ends derive the same binding - the TLS session is end to end, the relay
    /// only carries it. Twice on one connection (working rule 9).
    #[tokio::test]
    async fn a_client_reaches_the_station_through_the_relay() {
        let relay = FakeRelay::start().await;
        let (server, mut seen, _st, _) = station(&relay);
        let (mut c, _ct, _) = client(&relay, 0);
        ask(&mut c, b"hello").await;
        let tag = tag_of(&mut seen, b"hello").await;
        ask(&mut c, b"again").await;
        assert_eq!(tag_of(&mut seen, b"again").await, tag, "one connection, one sentinel");
        let client_binding = c.binding.borrow().expect("a live connection");
        assert_eq!(server.bindings.get(tag), Some(client_binding), "end to end: both derive the same binding");
        assert_eq!(server.bindings.peer(tag), Some(relay_peer_for(0)), "the server sees relay client 0 at its fixed address");
        assert_eq!(server.ways().of(tag), Some(PathKind::Relay), "and names it R");
    }

    /// Two clients through one relay: each its own connection, each answer to
    /// the client that asked.
    #[tokio::test]
    async fn two_clients_through_one_relay_stay_apart() {
        let relay = FakeRelay::start().await;
        let (_server, mut seen, _st, _) = station(&relay);
        let (mut a, _at, _) = client(&relay, 0);
        let (mut b, _bt, _) = client(&relay, 1);
        ask(&mut a, b"from a").await;
        let ta = tag_of(&mut seen, b"from a").await;
        ask(&mut b, b"from b").await;
        let tb = tag_of(&mut seen, b"from b").await;
        assert_ne!(ta, tb);
        ask(&mut a, b"a again").await;
        ask(&mut b, b"b again").await;
    }

    /// The relay's token rotation (same session, a new token, the old one gone
    /// a little later) passes unnoticed on both sides.
    #[tokio::test]
    async fn a_token_rotation_passes_unnoticed() {
        let relay = FakeRelay::start().await;
        let (_server, mut seen, st, sg) = station(&relay);
        let (mut c, ct, cg) = client(&relay, 0);
        ask(&mut c, b"before").await;
        let tag = tag_of(&mut seen, b"before").await;
        ct.send(Some(relay.rotate(cg))).unwrap();
        st.send(Some(relay.rotate(sg))).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        relay.revoke(cg.token);
        relay.revoke(sg.token);
        ask(&mut c, b"after").await;
        assert_eq!(tag_of(&mut seen, b"after").await, tag, "the same connection");
    }

    /// A network change: the old source dies, the relay library gets a new
    /// session (a new token for the same client id), the leg takes a new
    /// socket. The connection goes on; the server sees no new client. Without
    /// the new socket the old dead source would carry the new token, and
    /// nothing would arrive (review 117: this mutant was green before the
    /// dying source).
    #[tokio::test]
    async fn a_network_change_takes_a_new_socket_and_keeps_the_connection() {
        let relay = FakeRelay::start().await;
        let (_server, mut seen, _st, _) = station(&relay);
        let (mut c, ct, first) = client(&relay, 2);
        ask(&mut c, b"on wifi").await;
        let tag = tag_of(&mut seen, b"on wifi").await;
        relay.kill(relay.source_of(first.token).expect("bound"));
        ct.send(Some(relay.connect(false, 2))).unwrap();
        ask(&mut c, b"on 5g").await;
        assert_eq!(tag_of(&mut seen, b"on 5g").await, tag, "the same connection after the change");
    }

    /// A network change that brings a different client id: the server sees a
    /// known connection at a new address, checks the path and moves it. Still
    /// the same connection, so still the same session.
    #[tokio::test]
    async fn a_new_client_id_moves_the_connection() {
        let relay = FakeRelay::start().await;
        let (server, mut seen, _st, _) = station(&relay);
        let (mut c, ct, first) = client(&relay, 0);
        ask(&mut c, b"as client 0").await;
        let tag = tag_of(&mut seen, b"as client 0").await;
        relay.kill(relay.source_of(first.token).expect("bound"));
        relay.revoke(first.token);
        ct.send(Some(relay.connect(false, 3))).unwrap();
        ask(&mut c, b"as client 3").await;
        assert_eq!(tag_of(&mut seen, b"as client 3").await, tag, "the same connection after the move");
        assert_eq!(server.bindings.peer(tag), Some(relay_peer_for(3)), "now at client 3's address");
    }

    /// The station side changes: a new relay session (the relay revokes the
    /// old one's tokens and points at the new connection before it has a
    /// source), a new socket. The station's prompt keepalive binds it, and the
    /// client reaches the same connection again.
    #[tokio::test]
    async fn a_station_change_keeps_its_clients() {
        let relay = FakeRelay::start().await;
        let (_server, mut seen, st, first) = station(&relay);
        let (mut c, _ct, _) = client(&relay, 0);
        ask(&mut c, b"before").await;
        let tag = tag_of(&mut seen, b"before").await;
        relay.kill(relay.source_of(first.token).expect("bound"));
        st.send(Some(relay.connect(true, 0))).unwrap();
        ask(&mut c, b"after").await;
        assert_eq!(tag_of(&mut seen, b"after").await, tag, "the same connection");
    }

    /// No relay session for a while (the wss connection dropped), then a new
    /// one: nothing gets through in between, everything after.
    #[tokio::test]
    async fn a_gap_without_a_session_then_a_new_one() {
        let relay = FakeRelay::start().await;
        let (_server, mut seen, _st, _) = station(&relay);
        let (mut c, ct, _) = client(&relay, 0);
        ask(&mut c, b"before").await;
        let tag = tag_of(&mut seen, b"before").await;
        ct.send(None).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        c.uplink_tx.send((SERVER.parse().unwrap(), b"in the gap".to_vec())).unwrap();
        let heard = tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                if let Some((_, p)) = seen.recv().await {
                    if p == b"in the gap" {
                        return;
                    }
                }
            }
        })
        .await;
        assert!(heard.is_err(), "nothing goes without a session");
        ct.send(Some(relay.connect(false, 0))).unwrap();
        ask(&mut c, b"after").await;
        assert_eq!(tag_of(&mut seen, b"after").await, tag, "the same connection after the gap");
    }

    /// The station loses its relay session while clients hang on it (the
    /// server's wss link to the VPS drops), then gets a new one: nothing gets
    /// through in between, and afterwards every client is back on its own
    /// connection - quinn does not let them expire in a short gap. Twice,
    /// without a restart (working rule 9; review 118: only the client's side
    /// of a gap had been run).
    #[tokio::test]
    async fn a_station_gap_keeps_the_clients_hanging_on_it() {
        let relay = FakeRelay::start().await;
        let (_server, mut seen, st, _) = station(&relay);
        let (mut a, _at, _) = client(&relay, 0);
        let (mut b, _bt, _) = client(&relay, 1);
        ask(&mut a, b"a before").await;
        let ta = tag_of(&mut seen, b"a before").await;
        ask(&mut b, b"b before").await;
        let tb = tag_of(&mut seen, b"b before").await;
        for round in 0..2 {
            st.send(None).unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            a.uplink_tx.send((SERVER.parse().unwrap(), b"in the gap".to_vec())).unwrap();
            let heard = tokio::time::timeout(Duration::from_millis(500), async {
                loop {
                    if let Some((_, p)) = seen.recv().await {
                        if p == b"in the gap" {
                            return;
                        }
                    }
                }
            })
            .await;
            assert!(heard.is_err(), "round {round}: nothing reaches a station without a session");
            st.send(Some(relay.connect(true, 0))).unwrap();
            let (pa, pb) = (format!("a after {round}"), format!("b after {round}"));
            ask(&mut a, pa.as_bytes()).await;
            assert_eq!(tag_of(&mut seen, pa.as_bytes()).await, ta, "round {round}: a keeps its connection");
            ask(&mut b, pb.as_bytes()).await;
            assert_eq!(tag_of(&mut seen, pb.as_bytes()).await, tb, "round {round}: b keeps its connection");
        }
    }

    /// The relay monitor stops (on the phone it goes with its bridge): the
    /// grant's sender is gone, and the leg drops its socket rather than keep
    /// sending on a token nobody holds (review 118).
    #[tokio::test]
    async fn a_stopped_monitor_takes_the_socket_with_it() {
        let relay = FakeRelay::start().await;
        let (_server, mut seen, _st, _) = station(&relay);
        let (mut c, ct, _) = client(&relay, 0);
        ask(&mut c, b"before").await;
        tag_of(&mut seen, b"before").await;
        drop(ct);
        tokio::time::sleep(Duration::from_millis(100)).await;
        c.uplink_tx.send((SERVER.parse().unwrap(), b"after the stop".to_vec())).unwrap();
        let heard = tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                if let Some((_, p)) = seen.recv().await {
                    if p == b"after the stop" {
                        return;
                    }
                }
            }
        })
        .await;
        assert!(heard.is_err(), "no socket once the monitor is gone");
    }

    /// The IPv4 address beside an IPv6 first one carries the connection
    /// (decision 3, phase 3c: IPv4 first for the leg - before build 125 the
    /// first address, with this one as the way back when it took no socket,
    /// field test 119). Both ways, on the first session and the next; the
    /// first address here takes no socket at all, so only the IPv4 one can
    /// carry it. The order itself: `the_leg_tries_ipv4_first`.
    #[tokio::test]
    async fn no_socket_for_the_first_address_goes_over_the_ipv4_one() {
        // A link-local address on an interface that does not exist: no socket
        // connects to it. The relay library's own test uses the same one; a copy,
        // because that one sits in a test module this crate cannot reach.
        let unusable = SocketAddr::V6(std::net::SocketAddrV6::new(
            std::net::Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1),
            443,
            0,
            9999,
        ));
        let relay = FakeRelay::start().await;
        let (_server, mut seen, _st, _) = station(&relay);
        let first = relay.connect(false, 0);
        let (ct, rx) = watch::channel(Some(UdpGrant { relay: unusable, fallback: Some(first.relay), ..first }));
        let mut c = QuicClient::start_with(Congestion::Bbr, Some(RelayRoute::new(rx, None))).unwrap();
        ask(&mut c, b"over ipv4").await;
        let tag = tag_of(&mut seen, b"over ipv4").await;
        let next = relay.connect(false, 0);
        ct.send(Some(UdpGrant { relay: unusable, fallback: Some(next.relay), ..next })).unwrap();
        ask(&mut c, b"next session").await;
        assert_eq!(tag_of(&mut seen, b"next session").await, tag, "the same connection");
    }

    /// The server answers an offer only once the client proved the password
    /// (build 126, review 125): before, the path stays on the relay and works
    /// there; after the admission, it moves. Both states on one connection.
    #[tokio::test]
    async fn before_the_password_the_path_stays_on_the_relay() {
        let relay = FakeRelay::start().await;
        let grant = relay.connect(true, 0);
        let (st, rx) = watch::channel(Some(grant));
        relay.reflect(st.clone());
        let (server, link) =
            QuicServer::start_with(SocketAddr::from(([127, 0, 0, 1], 0)), Congestion::Bbr, Some(RelayRoute::new(rx, None))).unwrap();
        let mut seen = echo(link, None);
        let (mut c, ct, _) = client(&relay, 0);
        relay.reflect(ct.clone());
        ask(&mut c, b"first").await;
        let tag = tag_of(&mut seen, b"first").await;
        // Offers have gone by now (one a second), all unanswered.
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        assert_eq!(server.route.as_ref().unwrap().punches(), 0, "no punch before the password");
        let before = relay.forwarded();
        ask(&mut c, b"still through the relay").await;
        assert!(relay.forwarded() > before, "through the relay");
        server.allow_punch(tag);
        until_the_path_left_the_relay(&relay, &mut c).await;
        // The session behind the connection ends (a time-out, a goodbye) while
        // the connection lives on: the permit goes with the session (build 127,
        // review 126). A new public address makes the client offer again - and
        // now nobody answers: the path goes back to the relay and stays there.
        server.punch_gate().revoke(tag);
        let _ = ct.send_modify(|g| {
            if let Some(g) = g {
                g.public = Some("127.0.0.1:9".parse().unwrap());
            }
        });
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        let nowhere: SocketAddr = "127.0.0.1:9".parse().unwrap();
        assert!(
            !server.route.as_ref().unwrap().punch_targets().contains(&nowhere),
            "no punch to the new address without a permit"
        );
        let before = relay.forwarded();
        ask(&mut c, b"after the session ended").await;
        assert!(relay.forwarded() > before, "through the relay");
    }

    /// A confirmed direct path that goes silent (here: the server's punched
    /// sockets are gone) sends the client back to both ways within
    /// `DIRECT_SILENCE`, and it punches again - instead of sending into
    /// nothing until QUIC gives up after 30 s (review 125). Twice (rule 9).
    #[tokio::test]
    async fn a_silent_direct_path_goes_back_to_the_relay_and_punches_again() {
        let relay = FakeRelay::start().await;
        let (server, mut seen, _st, mut clients, _cts) = reflecting(&relay, &[0]);
        let c = &mut clients[0];
        ask(c, b"first").await;
        let tag = tag_of(&mut seen, b"first").await;
        until_the_path_left_the_relay(&relay, c).await;
        for round in 0..2 {
            server.route.as_ref().unwrap().drop_punches();
            let p = format!("after the silence {round}");
            ask(c, p.as_bytes()).await;
            assert_eq!(tag_of(&mut seen, p.as_bytes()).await, tag, "round {round}: the same connection");
            until_the_path_left_the_relay(&relay, c).await;
        }
    }

    /// L+R (build 142, `OPZET-L-plus-R.md` section 3): a client through the
    /// relay that sends straight to the station's own port moves its
    /// connection there - quinn on the server follows a known connection to a
    /// new address - and the station answers from that port, so the client
    /// confirms the way and stops sending through the relay. Taken away, the
    /// client goes through the relay again and quinn follows it back. Twice
    /// (rule 9). The station here does not know its public address, so it
    /// never punches: the only direct way is the one this test names.
    #[tokio::test]
    async fn a_client_moves_to_the_station_s_own_port_and_back() {
        let relay = FakeRelay::start().await;
        let (server, mut seen, _st, _) = station(&relay);
        let (mut c, ct, _) = client(&relay, 0);
        relay.reflect(ct.clone());
        ask(&mut c, b"through the relay").await;
        let tag = tag_of(&mut seen, b"through the relay").await;
        let own = SocketAddr::from(([127, 0, 0, 1], server.local_addr().unwrap().port()));
        let socket = c.relay_socket.clone().expect("through the relay");
        for round in 0..2 {
            // Told only in answer to an offer of this session: wait for one.
            let t0 = Instant::now();
            while ClientPath::from_u8(socket.leg.path.load(Ordering::Relaxed)) == ClientPath::Relay {
                assert!(t0.elapsed() < WAIT, "round {round}: the way was not taken");
                socket.ready(own);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            until_the_path_left_the_relay(&relay, &mut c).await;
            let p = format!("at home {round}");
            ask(&mut c, p.as_bytes()).await;
            assert_eq!(tag_of(&mut seen, p.as_bytes()).await, tag, "round {round}: the same connection");
            let peer = server.bindings.peer(tag).expect("live");
            assert!(peer.ip().is_loopback() && relay_peer_id(peer).is_none(), "round {round}: moved to the client's own address, not {peer}");
            assert_eq!(ClientPath::from_u8(socket.leg.path.load(Ordering::Relaxed)), ClientPath::Direct);
            socket.restart_punch();
            let before = relay.forwarded();
            let p = format!("away {round}");
            ask(&mut c, p.as_bytes()).await;
            assert_eq!(tag_of(&mut seen, p.as_bytes()).await, tag, "round {round}: the same connection, back on the relay");
            assert!(relay.forwarded() > before, "round {round}: through the relay again");
            let t0 = Instant::now();
            while server.bindings.peer(tag) != Some(relay_peer_for(0)) {
                assert!(t0.elapsed() < WAIT, "round {round}: quinn stayed at {:?}", server.bindings.peer(tag));
                ask(&mut c, b"still away").await;
            }
        }
    }

    /// IPv4 first, then the first address; without an IPv4 one beside it, the
    /// first alone (decision 3, phase 3c).
    #[test]
    fn the_leg_tries_ipv4_first() {
        let v6: SocketAddr = "[2001:db8::21]:443".parse().unwrap();
        let v4: SocketAddr = "192.0.2.33:443".parse().unwrap();
        let g = UdpGrant { relay: v6, fallback: Some(v4), public: None, token: [0; 32], session: 1 };
        assert_eq!(relay_order(&g), vec![v4, v6]);
        assert_eq!(relay_order(&UdpGrant { relay: v4, fallback: None, ..g }), vec![v4]);
    }

    /// A station and a client that both know their public address (the
    /// relay's reflection), for the punching tests.
    fn reflecting(relay: &FakeRelay, client_ids: &[u8]) -> (QuicServer, Seen, watch::Sender<Option<UdpGrant>>, Vec<QuicClient>, Vec<watch::Sender<Option<UdpGrant>>>) {
        let (server, seen, st, _) = station(relay);
        relay.reflect(st.clone());
        let mut clients = Vec::new();
        let mut senders = Vec::new();
        for &id in client_ids {
            let (c, ct, _) = client(relay, id);
            relay.reflect(ct.clone());
            clients.push(c);
            senders.push(ct);
        }
        (server, seen, st, clients, senders)
    }

    /// Ask until a whole round trip passes the relay by: the relay forwarded
    /// nothing with a payload while it went there and back.
    async fn until_the_path_left_the_relay(relay: &FakeRelay, c: &mut QuicClient) {
        let t0 = Instant::now();
        for n in 0.. {
            assert!(t0.elapsed() < Duration::from_secs(10), "the path did not leave the relay");
            let before = relay.forwarded();
            ask(c, format!("probe {n}").as_bytes()).await;
            if relay.forwarded() == before {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// The path moves: both ends learn their public address, the client
    /// offers its own, the server punches, and the same connection goes on
    /// directly - the relay forwards nothing more, and when it stops
    /// forwarding altogether the connection does not notice. Two clients, each
    /// with a socket of its own on the server and each answer to its own.
    #[tokio::test]
    async fn the_path_moves_off_the_relay() {
        let relay = FakeRelay::start().await;
        let (_server, mut seen, _st, mut clients, _cts) = reflecting(&relay, &[0, 1]);
        let mut tags = Vec::new();
        for (i, c) in clients.iter_mut().enumerate() {
            let p = format!("{i} through the relay");
            ask(c, p.as_bytes()).await;
            tags.push(tag_of(&mut seen, p.as_bytes()).await);
        }
        for c in clients.iter_mut() {
            until_the_path_left_the_relay(&relay, c).await;
        }
        // The server says D+R once nothing came through the relay for
        // RELAY_QUIET: the client stopped sending a copy.
        for (i, &tag) in tags.iter().enumerate() {
            let t0 = Instant::now();
            while _server.ways().of(tag) != Some(PathKind::DirectViaRelay) {
                assert!(t0.elapsed() < WAIT, "client {i}: the server still says {:?}", _server.ways().of(tag));
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        relay.stop_forwarding();
        for (i, c) in clients.iter_mut().enumerate() {
            let p = format!("{i} directly");
            ask(c, p.as_bytes()).await;
            assert_eq!(tag_of(&mut seen, p.as_bytes()).await, tags[i], "client {i}: the same connection");
            // Each punched socket reports its own client: quinn did not see a
            // connection move to another client's address. Watched for a
            // while - a move shows only after the next packets.
            for _ in 0..20 {
                assert_eq!(_server.bindings.peer(tags[i]), Some(relay_peer_for(i as u8)), "client {i}: at its own address");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    /// A network change after the path moved, and this time no new punch (the
    /// relay does not say where it sees the new socket): the server hears the
    /// client through the relay again and sends both ways, so the answers come
    /// back through the relay - instead of only to the old address, which is gone.
    #[tokio::test]
    async fn without_a_new_punch_the_server_answers_through_the_relay_again() {
        let relay = FakeRelay::start().await;
        let (_server, mut seen, _st, _) = station(&relay);
        relay.reflect(_st.clone());
        let (mut c, ct, first) = client(&relay, 0);
        let reflection = relay.reflect(ct.clone());
        ask(&mut c, b"first").await;
        let tag = tag_of(&mut seen, b"first").await;
        until_the_path_left_the_relay(&relay, &mut c).await;
        reflection.abort();
        relay.kill(relay.source_of(first.token).expect("bound"));
        ct.send(Some(relay.connect(false, 0))).unwrap();
        ask(&mut c, b"after the change").await;
        assert_eq!(tag_of(&mut seen, b"after the change").await, tag, "the same connection");
    }

    /// L+R end to end (build 142): the relay sees client and station at the
    /// same public IP (both loopback here, which this station is told counts
    /// as home), so the station answers the offer with its own port at its
    /// local address; the client moves there and names the way local, the
    /// relay carries nothing more, and no punch was made. A network change (a
    /// new relay session) goes back through the relay and comes home again -
    /// the same connection throughout. Twice (rule 9).
    #[tokio::test]
    async fn behind_the_same_router_the_client_goes_local() {
        let relay = FakeRelay::start().await;
        let (server, mut seen, _st, mut clients, cts) = reflecting(&relay, &[0]);
        let route = server.route.clone().expect("a relay");
        route.loopback_is_home.store(true, Ordering::Relaxed);
        let c = &mut clients[0];
        let socket = c.relay_socket.clone().expect("through the relay");
        let way = || ClientPath::from_u8(socket.leg.path.load(Ordering::Relaxed));
        ask(c, b"first").await;
        let tag = tag_of(&mut seen, b"first").await;
        for round in 0..2 {
            until_the_path_left_the_relay(&relay, c).await;
            assert_eq!(way(), ClientPath::Local, "round {round}");
            assert_eq!(route.punches(), 0, "round {round}: no punch");
            let peer = server.bindings.peer(tag).expect("live");
            assert!(relay_peer_id(peer).is_none(), "round {round}: quinn followed to the client's own address, not {peer}");
            assert_eq!(server.ways().of(tag), Some(PathKind::LocalViaRelay), "round {round}: the server names it L+R");
            let p = format!("at home {round}");
            ask(c, p.as_bytes()).await;
            assert_eq!(tag_of(&mut seen, p.as_bytes()).await, tag, "round {round}: the same connection");
            cts[0].send(Some(relay.connect(false, 0))).unwrap();
            let p = format!("after the change {round}");
            ask(c, p.as_bytes()).await;
            assert_eq!(tag_of(&mut seen, p.as_bytes()).await, tag, "round {round}: the same connection after the change");
        }
    }

    /// The TCP fallback route end to end (build 144): a client on a network
    /// that lets no UDP through reaches the station over the relay's TCP line,
    /// and the station answers it there - the relay forwards nothing over UDP.
    /// Then a new relay session with UDP open: the same connection, the copies
    /// over TCP stop, and the station stops answering over TCP once the client
    /// has been quiet there for `RELAY_QUIET`. Twice (rule 9).
    ///
    /// Client 1, not 0: the station's own keepalive goes to the keepalive id,
    /// which is 0, and so reaches client 0 as a datagram over UDP every second
    /// (`UDP_MAPPING_KEEPALIVE_ID`, on the real relay too). Client 0 would see
    /// UDP work without the station ever sending it anything, and hide whether
    /// the station answers over UDP (review 145, U5; build 146, W7/W8).
    #[tokio::test]
    async fn without_udp_the_client_goes_over_tcp() {
        use crate::testing::FakeTcp;
        let relay = FakeRelay::start().await;
        let tcp = FakeTcp::new();
        let sg = relay.connect(true, 0);
        let (_st, srx) = watch::channel(Some(sg));
        let (_server, link) =
            QuicServer::start_with(SocketAddr::from(([127, 0, 0, 1], 0)), Congestion::Bbr, Some(RelayRoute::new(srx, Some(tcp.station())))).unwrap();
        let mut seen = echo(link, None);
        let cg = relay.connect(false, 1);
        let (ct, crx) = watch::channel(Some(cg));
        let croute = RelayRoute::new(crx, Some(tcp.client(1)));
        let force = croute.force_tcp.clone();
        let mut c = QuicClient::start_with(Congestion::Bbr, Some(croute)).unwrap();
        let socket = c.relay_socket.clone().expect("through the relay");
        let way = || ClientPath::from_u8(socket.leg.path.load(Ordering::Relaxed));
        let mut tag = None;
        for round in 0..2 {
            let blocked = relay.connect(false, 1);
            relay.block_udp(blocked.token);
            ct.send(Some(blocked)).unwrap();
            let p = format!("over tcp {round}");
            ask(&mut c, p.as_bytes()).await;
            let t = tag_of(&mut seen, p.as_bytes()).await;
            assert_eq!(*tag.get_or_insert(t), t, "round {round}: one connection throughout");
            // Past RELAY_QUIET since the client was last heard over UDP (the
            // round before): from then on the station holds UDP back.
            tokio::time::sleep(RELAY_QUIET + Duration::from_millis(200)).await;
            let before = (relay.forwarded(), tcp.frames(), relay.received(sg.token));
            for n in 0..3 {
                ask(&mut c, format!("still over tcp {round}.{n}").as_bytes()).await;
            }
            let after = (relay.forwarded(), tcp.frames(), relay.received(sg.token));
            assert_eq!(after.0, before.0, "round {round}: nothing over UDP");
            assert!(after.1 .0 > before.1 .0 && after.1 .1 > before.1 .1, "round {round}: both ways over TCP");
            // A client heard over TCP only gets no UDP copy from the station
            // (build 145): it would never arrive, and it doubled the uplink.
            assert_eq!(after.2, before.2, "round {round}: the station sent the TCP client nothing over UDP");
            // And both ends name it so (build 146): UDP unproven past
            // RELAY_QUIET on the client, TCP and no UDP on the server.
            assert_eq!(way(), ClientPath::RelayTcp, "round {round}: the client says R (TCP)");
            assert_eq!(_server.ways().of(t), Some(PathKind::RelayTcp), "round {round}: the server says R (TCP)");
            // UDP opens: a new relay session.
            ct.send(Some(relay.connect(false, 1))).unwrap();
            let p = format!("over udp {round}");
            ask(&mut c, p.as_bytes()).await;
            assert_eq!(tag_of(&mut seen, p.as_bytes()).await, t, "round {round}: the same connection over UDP");
            // The copies stop at the first UDP from the relay in the new
            // session - not at once: the answer to a packet sent before the new
            // socket stood has no UDP address yet (the relay's `udp_target`
            // too), so it comes over TCP alone. Ask until a round trip makes no
            // copy, within a few seconds.
            // Measured: two round trips, about 100 ms (review 144) - a second
            // pins "at most about a second" from the brief, with room to spare.
            let t0 = Instant::now();
            for n in 0.. {
                assert!(t0.elapsed() < Duration::from_secs(1), "round {round}: the client still copies over TCP");
                let before = tcp.frames().0;
                ask(&mut c, format!("until no copy {round}.{n}").as_bytes()).await;
                if tcp.frames().0 == before {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            // Both TCP and UDP are recent here: the server says R, not R (TCP) -
            // the `!udp_recent` half of its name (review 146: no test held
            // it).
            assert_eq!(_server.ways().of(t), Some(PathKind::Relay), "round {round}: R while both TCP and UDP are recent");
            tokio::time::sleep(RELAY_QUIET + Duration::from_millis(500)).await;
            let quiet = (tcp.frames(), relay.received(sg.token));
            ask(&mut c, format!("udp only {round}").as_bytes()).await;
            assert_eq!(tcp.frames(), quiet.0, "round {round}: no copies either way once UDP works");
            // The station sends over UDP again - the stamp of a UDP arrival
            // at work, not a timing (review 145: U5 was 4 of 5 red).
            assert!(relay.received(sg.token) > quiet.1, "round {round}: the station sends over UDP again");
            assert_eq!(way(), ClientPath::Relay, "round {round}: the client says R again");
            assert_eq!(_server.ways().of(t), Some(PathKind::Relay), "round {round}: and so does the server");
            // "Force TCP" (build 146): everything over the TCP line while UDP
            // works - past RELAY_QUIET the station holds UDP back as well.
            force.store(true, Ordering::Relaxed);
            ask(&mut c, format!("forced {round}").as_bytes()).await;
            tokio::time::sleep(RELAY_QUIET + Duration::from_millis(200)).await;
            let before = (relay.forwarded(), tcp.frames());
            ask(&mut c, format!("still forced {round}").as_bytes()).await;
            assert_eq!(relay.forwarded(), before.0, "round {round}: forced - nothing over UDP");
            assert!(tcp.frames().0 > before.1 .0 && tcp.frames().1 > before.1 .1, "round {round}: forced - both ways over TCP");
            assert_eq!(way(), ClientPath::RelayTcp, "round {round}: forced - the client says R (TCP)");
            assert_eq!(_server.ways().of(t), Some(PathKind::RelayTcp), "round {round}: forced - the server says R (TCP)");
            force.store(false, Ordering::Relaxed);
            let before = relay.forwarded();
            ask(&mut c, format!("unforced {round}").as_bytes()).await;
            assert!(relay.forwarded() > before, "round {round}: unforced - over UDP again");
        }
    }

    /// A connection that closes takes its punch with it on the server - no
    /// more probes and no copies to an address that has left.
    #[tokio::test]
    async fn a_closed_connection_takes_its_punch_with_it() {
        let relay = FakeRelay::start().await;
        let (server, mut seen, _st, mut clients, _cts) = reflecting(&relay, &[0]);
        let c = &mut clients[0];
        ask(c, b"first").await;
        tag_of(&mut seen, b"first").await;
        until_the_path_left_the_relay(&relay, c).await;
        let route = server.route.clone().expect("a relay");
        assert_eq!(route.punches(), 1);
        c.stop.stop().await;
        let t0 = Instant::now();
        while route.punches() > 0 {
            assert!(t0.elapsed() < Duration::from_secs(2), "the punch outlived its connection");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The relay sees this side at another public address within one session
    /// (its router moved the mapping): the server punched towards the old one,
    /// so the client goes through the relay again and offers the new address.
    #[tokio::test]
    async fn a_new_public_address_in_the_session_starts_over() {
        let relay = FakeRelay::start().await;
        let (_server, mut seen, _st, mut clients, cts) = reflecting(&relay, &[0]);
        let c = &mut clients[0];
        ask(c, b"first").await;
        tag_of(&mut seen, b"first").await;
        until_the_path_left_the_relay(&relay, c).await;
        cts[0].send_modify(|g| {
            if let Some(g) = g {
                g.public = Some("127.0.0.1:9".parse().unwrap());
            }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let before = relay.forwarded();
        ask(c, b"after the new address").await;
        assert!(relay.forwarded() > before, "through the relay again");
    }

    /// The other state of the punch (working rule 13): the station does not
    /// know its public address, so it cannot answer an offer - the path stays
    /// on the relay, and works there.
    #[tokio::test]
    async fn without_the_station_s_address_the_path_stays_on_the_relay() {
        let relay = FakeRelay::start().await;
        let (_server, mut seen, _st, _) = station(&relay);
        let (mut c, ct, _) = client(&relay, 0);
        relay.reflect(ct.clone());
        ask(&mut c, b"first").await;
        tag_of(&mut seen, b"first").await;
        // Offers have gone by now (one a second; the reflection takes a tick).
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        for n in 0..3 {
            let before = relay.forwarded();
            ask(&mut c, format!("still {n}").as_bytes()).await;
            assert!(relay.forwarded() > before, "round {n}: through the relay");
        }
    }

    /// A network change after the path moved: a new relay session, a new
    /// socket, a new public address - the client goes through the relay again,
    /// punches again, and the path moves again. The same connection all along.
    /// Twice, without a restart (working rule 9).
    #[tokio::test]
    async fn a_network_change_punches_again() {
        let relay = FakeRelay::start().await;
        let (_server, mut seen, _st, mut clients, cts) = reflecting(&relay, &[2]);
        let c = &mut clients[0];
        ask(c, b"first").await;
        let tag = tag_of(&mut seen, b"first").await;
        until_the_path_left_the_relay(&relay, c).await;
        let mut token = cts[0].borrow().unwrap().token;
        for round in 0..2 {
            relay.kill(relay.source_of(token).expect("bound"));
            let next = relay.connect(false, 2);
            token = next.token;
            cts[0].send(Some(next)).unwrap();
            let p = format!("after change {round}");
            ask(c, p.as_bytes()).await;
            assert_eq!(tag_of(&mut seen, p.as_bytes()).await, tag, "round {round}: the same connection");
            until_the_path_left_the_relay(&relay, c).await;
        }
    }

    /// A new connection after the path moved (the engine sends to another
    /// address): the punch of the old one leads to a socket that goes with it,
    /// so the new one starts through the relay and punches for itself - and
    /// still works once the old connection is gone at the server, every round
    /// trip within 1.3 s: without its own start, the fallback on silence (build
    /// 126) would bring it back too, but only after a gap of 1.5 s. 1.3 s and
    /// not 1 s: room for a slow build machine (review 126), below the fallback.
    #[tokio::test]
    async fn a_new_connection_punches_for_itself() {
        let relay = FakeRelay::start().await;
        let (_server, mut seen, _st, mut clients, _cts) = reflecting(&relay, &[0]);
        let c = &mut clients[0];
        ask(c, b"first").await;
        let old = tag_of(&mut seen, b"first").await;
        until_the_path_left_the_relay(&relay, c).await;
        let other: SocketAddr = "203.0.113.2:4581".parse().unwrap();
        let t0 = Instant::now();
        let new = loop {
            assert!(t0.elapsed() < Duration::from_millis(1_300), "the new connection did not come up within 1.3 s");
            c.uplink_tx.send((other, b"to another address".to_vec())).unwrap();
            if let Ok(Some((tag, p))) = tokio::time::timeout(Duration::from_millis(300), seen.recv()).await {
                if p == b"to another address" {
                    break tag;
                }
            }
        };
        assert_ne!(new, old, "a new connection");
        // The old connection's close reaches the server, and its punch goes with it.
        tokio::time::sleep(Duration::from_millis(500)).await;
        for n in 0..3 {
            let p = format!("new {n}");
            let t0 = Instant::now();
            loop {
                assert!(t0.elapsed() < Duration::from_millis(1_300), "round {n}: the new connection went quiet");
                c.uplink_tx.send((other, p.as_bytes().to_vec())).unwrap();
                if let Ok(Some((from, back))) = tokio::time::timeout(Duration::from_millis(300), c.inbound_rx.recv()).await {
                    if from == other && back[3..] == *p.as_bytes() {
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(1_000)).await;
        }
    }

    /// One endpoint serves both: a client direct on the server's port (method 1)
    /// and one through the relay (method 2), at the same time.
    #[tokio::test]
    async fn the_direct_port_works_beside_the_relay() {
        let relay = FakeRelay::start().await;
        let (server, mut seen, _st, _) = station(&relay);
        let port = server.local_addr().unwrap().port();
        let (mut via, _vt, _) = client(&relay, 0);
        let mut direct = QuicClient::start(Congestion::Bbr).unwrap();
        ask(&mut via, b"via the relay").await;
        let tv = tag_of(&mut seen, b"via the relay").await;
        let to = SocketAddr::from(([127, 0, 0, 1], port));
        direct.uplink_tx.send((to, b"direct".to_vec())).unwrap();
        let (from, back) = tokio::time::timeout(WAIT, direct.inbound_rx.recv()).await.unwrap().unwrap();
        assert_eq!((from, &back[..]), (to, &b"re:direct"[..]));
        let td = tag_of(&mut seen, b"direct").await;
        assert_ne!(tv, td);
        assert_eq!(server.bindings.peer(td).map(|a| a.ip()), Some(IpAddr::V4(Ipv4Addr::LOCALHOST)), "a direct client keeps its real address");
        // Both at a loopback address now, apart only by where they came in.
        assert_eq!(server.ways().of(td), Some(PathKind::Local), "straight to a local address: L");
        assert_eq!(server.ways().of(tv), Some(PathKind::Relay), "through the relay: R");
    }

    /// A station that takes connections through the relay only (build 135),
    /// with its public address reflected, ready to be punched to if it would.
    fn relay_only_station(relay: &FakeRelay) -> (QuicServer, Seen, watch::Sender<Option<UdpGrant>>) {
        let grant = relay.connect(true, 0);
        let (tx, rx) = watch::channel(Some(grant));
        let socket = std::net::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let (server, link) =
            QuicServer::start_on(socket, Congestion::Bbr, Some(RelayRoute::new(rx, None)), crate::Direct::RelayOnly).unwrap();
        let seen = echo(link, Some(server.punch_gate()));
        relay.reflect(tx.clone());
        (server, seen, tx)
    }

    /// "Relay only" on the client (build 137), switched while the connection
    /// runs: from a confirmed direct path back to the relay - every round trip
    /// passes the relay again - and, switched off, off the relay once more. The
    /// same connection throughout (working rule 9: on, off, and on again).
    #[tokio::test]
    async fn relay_only_on_the_client_goes_back_to_the_relay_and_off_again() {
        let relay = FakeRelay::start().await;
        let (_server, mut seen, st, _) = station(&relay);
        relay.reflect(st.clone());
        let grant = relay.connect(false, 0);
        let (ct, rx) = watch::channel(Some(grant));
        relay.reflect(ct.clone());
        let route = RelayRoute::new(rx, None);
        let flag = route.relay_only.clone();
        // The way the leg says it takes (build 139), read as the engine reads it.
        let leg_says = route.clone();
        let mut c = QuicClient::start_with(Congestion::Bbr, Some(route)).unwrap();
        ask(&mut c, b"first").await;
        let tag = tag_of(&mut seen, b"first").await;
        for round in 0..2 {
            until_the_path_left_the_relay(&relay, &mut c).await;
            assert_eq!(leg_says.path(), super::ClientPath::Direct, "round {round}: the leg says direct");
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
            // Within a few offer ticks the client lets the direct path go.
            tokio::time::sleep(Duration::from_millis(600)).await;
            assert_eq!(leg_says.path(), super::ClientPath::Relay, "round {round}: and relay again");
            for n in 0..10 {
                let before = relay.forwarded();
                ask(&mut c, format!("round {round} relay only {n}").as_bytes()).await;
                assert!(relay.forwarded() > before, "round {round}: trip {n} passed the relay by with relay only on");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert_eq!(tag_of(&mut seen, format!("round {round} relay only 9").as_bytes()).await, tag, "the same connection");
            flag.store(false, std::sync::atomic::Ordering::Relaxed);
        }
        until_the_path_left_the_relay(&relay, &mut c).await;
    }

    /// A relay-only station: a client through the relay connects and stays on
    /// the relay - its offer is not answered, so every round trip still passes
    /// the relay, well past the time a punch takes (`the_path_moves_off_the_relay`
    /// leaves the relay within a second or two). Both states of the switch: the
    /// same harness with `Direct::Allowed` is that test.
    #[tokio::test]
    async fn a_relay_only_station_answers_no_offer() {
        let relay = FakeRelay::start().await;
        let (_server, mut seen, _st) = relay_only_station(&relay);
        let (mut c, ct, _) = client(&relay, 0);
        relay.reflect(ct.clone());
        ask(&mut c, b"through the relay").await;
        let tag = tag_of(&mut seen, b"through the relay").await;
        let t0 = Instant::now();
        let mut n = 0;
        while t0.elapsed() < Duration::from_secs(4) {
            let before = relay.forwarded();
            ask(&mut c, format!("still relayed {n}").as_bytes()).await;
            assert!(relay.forwarded() > before, "round trip {n} passed the relay by - a punch was answered");
            n += 1;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert_eq!(_server.bindings.peer(tag), Some(relay_peer_for(0)), "still at its relay address");
        // Not only no direct path: no punch at all. A punch puts the station's
        // own address on the wire before any path stands, and that is what
        // relay only is for (review 135: a punch that never completed left
        // this test green).
        let route = _server.route.as_ref().expect("a station with a relay has a route layer");
        assert_eq!(route.punches(), 0, "a relay-only station punched: {:?}", route.punch_targets());
    }

    /// A relay-only station: a client straight on its port gets nothing back -
    /// no handshake, no connection - while a client through the relay beside it
    /// is served as usual.
    #[tokio::test]
    async fn a_relay_only_station_refuses_a_direct_client() {
        let relay = FakeRelay::start().await;
        let (server, mut seen, _st) = relay_only_station(&relay);
        let port = server.local_addr().unwrap().port();
        let (mut via, _vt, _) = client(&relay, 0);
        ask(&mut via, b"via the relay").await;
        tag_of(&mut seen, b"via the relay").await;
        let mut direct = QuicClient::start(Congestion::Bbr).unwrap();
        let to = SocketAddr::from(([127, 0, 0, 1], port));
        for _ in 0..10 {
            direct.uplink_tx.send((to, b"direct".to_vec())).unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(500), direct.inbound_rx.recv()).await.is_err(),
            "a direct client got an answer from a relay-only station"
        );
        assert!(direct.binding.borrow().is_none(), "and no connection stood");
        // And said: the refusal line is the field's only sign of a direct try
        // (review 136: a socket that stopped reading kept every test green).
        let route = server.route.as_ref().expect("a station with a relay has a route layer");
        assert!(route.refused.lock().unwrap().last_line.is_some(), "the direct datagrams were dropped without a word");
        ask(&mut via, b"via the relay again").await;
    }

    /// A packet larger than a datagram (a stream) gets through the relay too.
    #[tokio::test]
    async fn a_large_packet_gets_through_the_relay() {
        let relay = FakeRelay::start().await;
        let (_server, _seen, _st, _) = station(&relay);
        let (mut c, _ct, _) = client(&relay, 0);
        ask(&mut c, &vec![7u8; 9000]).await;
    }

    /// A client that stops gets its close to the server through the relay: the
    /// server reads the close of that connection at once.
    #[tokio::test]
    async fn a_close_gets_through_the_relay() {
        let relay = FakeRelay::start().await;
        let grant = relay.connect(true, 0);
        let (st_tx, st_rx) = watch::channel(Some(grant));
        let (_server, mut link) =
            QuicServer::start_with(SocketAddr::from(([127, 0, 0, 1], 0)), Congestion::Bbr, Some(RelayRoute::new(st_rx, None))).unwrap();
        let (c, _ct, _) = client(&relay, 0);
        c.uplink_tx.send((SERVER.parse().unwrap(), b"hi".to_vec())).unwrap();
        let (tag, _) = tokio::time::timeout(WAIT, link.inbound_rx.recv()).await.unwrap().unwrap();
        c.stop.stop().await;
        let (from, data) = tokio::time::timeout(Duration::from_secs(3), link.inbound_rx.recv())
            .await
            .expect("the close at once, not after the idle timeout")
            .unwrap();
        assert_eq!((from, data.len()), (tag, 0));
        drop(st_tx);
    }
}
