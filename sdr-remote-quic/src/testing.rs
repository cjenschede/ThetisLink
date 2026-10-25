// SPDX-License-Identifier: GPL-2.0-or-later

//! A relay for tests (phase 3b): it does what `thetislink-relay`'s `serve_udp`
//! does - a token per slot, bound to the first source it is used from; a new
//! connection for a slot revokes the tokens of the older one
//! (`revoke_superseded`); the destination is the newest connection's token even
//! before it has a source (`udp_target`); the payload forwarded unread as
//! `[client_id] + payload`. And a source that "dies" the way a NAT mapping does
//! after a network change. One stand-in for every crate that tests through the
//! relay (feature `testing`), not one per test file.
//!
//! For punching (phase 3c) it also does what the relay's `udp-seen` does - it
//! tells a side the source it saw ([`FakeRelay::reflect`]) - and it counts what
//! it forwarded, so a test can see the path leave it.
//!
//! For the TCP fallback route (build 144): a network that lets no UDP through
//! ([`FakeRelay::block_udp`]), and the relay's wss side for TLT1 frames
//! ([`FakeTcp`]), as `route_message` does it.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use sdr_remote_relay::{tlt1_parts, TcpLine, UdpGrant, TCP_LINE_DEPTH, TLU1_HEADER_LEN};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};

struct Slot {
    station: bool,
    client_id: u8,
    conn: u64,
    src: Option<SocketAddr>,
    last_seq: Option<u32>,
    order: u64,
}

impl Slot {
    fn same_slot(&self, station: bool, client_id: u8) -> bool {
        self.station == station && (station || self.client_id == client_id)
    }
}

#[derive(Default)]
struct Book {
    slots: HashMap<[u8; 32], Slot>,
    dead: HashSet<SocketAddr>,
    /// Tokens whose side is on a network that lets no UDP through.
    blocked: HashSet<[u8; 32]>,
    /// Datagrams with a payload that arrived per token, delivered or not.
    received: HashMap<[u8; 32], u64>,
    next: u64,
    /// Datagrams forwarded with a payload - keepalives left out, they go on
    /// after the path has moved, by design.
    forwarded: u64,
    stopped: bool,
}

#[derive(Clone)]
pub struct FakeRelay {
    pub addr: SocketAddr,
    book: Arc<Mutex<Book>>,
}

impl FakeRelay {
    pub async fn start() -> FakeRelay {
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let relay = FakeRelay { addr: sock.local_addr().unwrap(), book: Arc::default() };
        let r = relay.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            loop {
                let Ok((n, src)) = sock.recv_from(&mut buf).await else { continue };
                let d = &buf[..n];
                if n < TLU1_HEADER_LEN || &d[0..4] != b"TLU1" || d[4] != 1 {
                    continue;
                }
                let target = d[6];
                let seq = u32::from_le_bytes([d[7], d[8], d[9], d[10]]);
                let mut token = [0u8; 32];
                token.copy_from_slice(&d[11..43]);
                let fwd = {
                    let mut book = r.book.lock().unwrap();
                    if n > TLU1_HEADER_LEN {
                        *book.received.entry(token).or_default() += 1;
                    }
                    if book.dead.contains(&src) {
                        continue; // the mapping is gone: nothing from here arrives
                    }
                    if book.stopped {
                        continue;
                    }
                    if book.blocked.contains(&token) {
                        continue; // never arrives, so never bound: no udp-seen either
                    }
                    let Some(s) = book.slots.get_mut(&token) else { continue };
                    if s.src.is_some_and(|b| b != src) || s.last_seq == Some(seq) {
                        continue;
                    }
                    s.src.get_or_insert(src);
                    s.last_seq = Some(seq);
                    let (station, cid) = (s.station, s.client_id);
                    // As `udp_target`: the newest connection first, then a known
                    // source, then the latest token - and only then the source.
                    let target_of = |want_station: bool, want_cid: u8| {
                        book.slots
                            .values()
                            .filter(|o| o.same_slot(want_station, want_cid))
                            .max_by_key(|o| (o.conn, o.src.is_some(), o.order))
                            .and_then(|o| o.src)
                    };
                    let dst = if station { target_of(false, target) } else { target_of(true, 0) };
                    let fwd = dst.filter(|d| !book.dead.contains(d)).map(|d| (d, if station { target } else { cid }));
                    if fwd.is_some() && n > TLU1_HEADER_LEN {
                        book.forwarded += 1;
                    }
                    fwd
                };
                if let Some((dst, cid)) = fwd {
                    let mut framed = vec![cid];
                    framed.extend_from_slice(&d[TLU1_HEADER_LEN..]);
                    let _ = sock.send_to(&framed, dst).await;
                }
            }
        });
        relay
    }

    fn token_for(book: &mut Book, station: bool, client_id: u8, conn: u64) -> [u8; 32] {
        book.next += 1;
        let mut token = [0u8; 32];
        token[..8].copy_from_slice(&book.next.to_le_bytes());
        token[8] = if station { 0xAA } else { 0xCC };
        let order = book.next;
        book.slots.insert(token, Slot { station, client_id, conn, src: None, last_seq: None, order });
        token
    }

    /// A new wss connection for (station or client id): the tokens of the
    /// older connections for that slot go at once. Returns the grant.
    pub fn connect(&self, station: bool, client_id: u8) -> UdpGrant {
        let mut book = self.book.lock().unwrap();
        book.next += 1;
        let conn = book.next;
        book.slots.retain(|_, s| !(s.same_slot(station, client_id) && s.conn < conn));
        let token = Self::token_for(&mut book, station, client_id, conn);
        UdpGrant { relay: self.addr, fallback: None, public: None, token, session: conn }
    }

    /// The relay's rotation: a new token on the same connection; the old one
    /// stays until `revoke`.
    pub fn rotate(&self, grant: UdpGrant) -> UdpGrant {
        let mut book = self.book.lock().unwrap();
        let (station, client_id) = {
            let s = &book.slots[&grant.token];
            (s.station, s.client_id)
        };
        let token = Self::token_for(&mut book, station, client_id, grant.session);
        UdpGrant { token, ..grant }
    }

    pub fn revoke(&self, token: [u8; 32]) {
        self.book.lock().unwrap().slots.remove(&token);
    }

    /// The source a token is bound to.
    pub fn source_of(&self, token: [u8; 32]) -> Option<SocketAddr> {
        self.book.lock().unwrap().slots.get(&token).and_then(|s| s.src)
    }

    /// A source that is gone, as the NAT mapping of the old network after a
    /// switch: nothing from it arrives, nothing to it is delivered.
    ///
    /// A model, with the field's outcome and not its mechanism: on the real
    /// relay the old socket's packets come from a new address after the
    /// switch, and the token's binding to its first source refuses them. Either
    /// way the old socket reaches nobody, which is what the tests need
    /// (review 118).
    pub fn kill(&self, src: SocketAddr) {
        self.book.lock().unwrap().dead.insert(src);
    }

    /// The side holding `token` is on a network that lets no UDP through:
    /// nothing it sends arrives, and with no source bound nothing reaches it.
    pub fn block_udp(&self, token: [u8; 32]) {
        self.book.lock().unwrap().blocked.insert(token);
    }

    /// How many datagrams with a payload arrived with `token`, delivered or not.
    pub fn received(&self, token: [u8; 32]) -> u64 {
        self.book.lock().unwrap().received.get(&token).copied().unwrap_or(0)
    }

    /// How many datagrams with a payload it forwarded so far.
    pub fn forwarded(&self) -> u64 {
        self.book.lock().unwrap().forwarded
    }

    /// From now on nothing at all is forwarded - what still arrives goes
    /// another way.
    pub fn stop_forwarding(&self) {
        self.book.lock().unwrap().stopped = true;
    }

    /// The relay's `udp-seen`: once the token of the grant in `grant` has a
    /// source, that source goes into the grant as `public` - for every grant
    /// that comes after too, as the relay library publishes it (by modifying
    /// the grant of the session). Ends when nobody reads the grant any more,
    /// or when the handle it returns is aborted.
    pub fn reflect(&self, grant: watch::Sender<Option<UdpGrant>>) -> tokio::task::JoinHandle<()> {
        let relay = self.clone();
        tokio::spawn(async move {
            while grant.receiver_count() > 0 {
                let now = *grant.borrow();
                if let Some(g) = now {
                    if let Some(src) = relay.source_of(g.token) {
                        grant.send_if_modified(|cur| match cur {
                            Some(c) if c.token == g.token && c.public != Some(src) => {
                                c.public = Some(src);
                                true
                            }
                            _ => false,
                        });
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
    }
}

/// The relay's wss side for TLT1 frames (the TCP fallback route, build 144), as
/// the relay's `route_message` does it: a client's frame goes to the station
/// with the client's id stamped in, and the station's frame goes to the client
/// it names. One station, clients by id. It counts the frames each way.
#[derive(Clone, Default)]
pub struct FakeTcp {
    inner: Arc<Mutex<TcpBook>>,
}

#[derive(Default)]
struct TcpBook {
    station: Option<mpsc::Sender<(u8, Vec<u8>)>>,
    clients: HashMap<u8, mpsc::Sender<(u8, Vec<u8>)>>,
    to_station: u64,
    to_clients: u64,
}

impl FakeTcp {
    pub fn new() -> FakeTcp {
        FakeTcp::default()
    }

    /// The station's TCP line.
    pub fn station(&self) -> TcpLine {
        let (line, mut out, inbound) = Self::line();
        self.inner.lock().unwrap().station = Some(inbound);
        let book = self.inner.clone();
        tokio::spawn(async move {
            while let Some(frame) = out.recv().await {
                let Some((cid, payload)) = tlt1_parts(&frame) else { continue };
                let to = {
                    let mut b = book.lock().unwrap();
                    b.to_clients += 1;
                    b.clients.get(&cid).cloned()
                };
                if let Some(to) = to {
                    let _ = to.try_send((cid, payload.to_vec()));
                }
            }
        });
        line
    }

    /// The TCP line of client `client_id`.
    pub fn client(&self, client_id: u8) -> TcpLine {
        let (line, mut out, inbound) = Self::line();
        self.inner.lock().unwrap().clients.insert(client_id, inbound);
        let book = self.inner.clone();
        tokio::spawn(async move {
            while let Some(frame) = out.recv().await {
                // The relay stamps the sender's id, whatever the client wrote.
                let Some((_, payload)) = tlt1_parts(&frame) else { continue };
                let to = {
                    let mut b = book.lock().unwrap();
                    b.to_station += 1;
                    b.station.clone()
                };
                if let Some(to) = to {
                    let _ = to.try_send((client_id, payload.to_vec()));
                }
            }
        });
        line
    }

    /// Frames from clients to the station, and from the station to clients.
    pub fn frames(&self) -> (u64, u64) {
        let b = self.inner.lock().unwrap();
        (b.to_station, b.to_clients)
    }

    fn line() -> (TcpLine, mpsc::Receiver<Vec<u8>>, mpsc::Sender<(u8, Vec<u8>)>) {
        let (out_tx, out_rx) = mpsc::channel(TCP_LINE_DEPTH);
        let (in_tx, in_rx) = mpsc::channel(TCP_LINE_DEPTH);
        (TcpLine { to_relay: out_tx, from_relay: in_rx, dropped_out: Default::default() }, out_rx, in_tx)
    }
}
