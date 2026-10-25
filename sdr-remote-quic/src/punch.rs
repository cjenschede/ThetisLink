// SPDX-License-Identifier: GPL-2.0-or-later

//! Hole punching (phase 3c, `OPZET-fase3c-gaten-prikken.md` §10): app and server
//! find each other through the relay and then carry the same QUIC connection
//! directly. One topic, one crate: the signals live here, the sockets and the
//! choice of way in the route layer ([`crate::route`]). The engine and the
//! server never see a punch signal - their arms for it stay empty.
//!
//! The order, from the third experiment:
//! 1. the client offers its public address (`udp-seen` of the relay) inside the
//!    QUIC connection, which still runs through the relay;
//! 2. the server opens a fresh socket for that client, probes the client's
//!    address from it, and answers with its own public IP and that port;
//! 3. only then does the client send directly - from the socket of its relay
//!    leg, so its router uses the mapping the relay saw. Both sides send every
//!    packet both ways until the direct path has answered, and give a punch up
//!    after [`GIVE_UP`]. A direct path that goes silent for [`DIRECT_SILENCE`]
//!    sends the client back to both ways, and it punches again.
//!
//! The server takes an offer only from a connection whose client proved the
//! password (build 126, review 125): an offer makes the server send to an
//! address the client names, so it is for those who may use the station anyway.
//!
//! L+R (build 142, `OPZET-L-plus-R.md`): when the relay sees the client at this
//! side's own public IP, the two are behind the same router, and a punch fails
//! there (the router does not send its own public address back in). The server
//! answers the offer with its local address and own port instead
//! (`PunchLocal`), and the client sends there the way it sends to a punched
//! address; quinn on the server follows the connection to the client's local
//! address.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use log::{debug, info};
use quinn::Connection;
use sdr_remote_core::protocol::{Header, Packet, PacketType, PunchPacket, MAGIC};

use crate::route::{relay_peer_id, Answer, ClientRelaySocket, ServerRouteSocket};

/// The server's probe: four bytes from its fresh socket to the client's public
/// address, every [`PUNCH_EVERY`], to open its own router. The client drops it.
pub(crate) const PUNCH_PROBE: [u8; 4] = *b"TLP1";

/// How often the server probes until the direct path answers.
pub(crate) const PUNCH_EVERY: Duration = Duration::from_millis(200);

/// How long a punch may take, on both sides, and how long a direct path may be
/// silent on the server before that client is served through the relay only.
pub(crate) const GIVE_UP: Duration = Duration::from_secs(10);

/// The probes in [`GIVE_UP`].
pub(crate) const PUNCH_PROBES: u32 = (GIVE_UP.as_millis() / PUNCH_EVERY.as_millis()) as u32;

/// How often the client offers its address until the server answers: the
/// offer and the answer are datagrams, and datagrams get lost. The first
/// [`MAX_OFFERS`] once a second, then once every [`OFFER_EVERY_SLOW`] - the
/// server answers only after the password, and a code typed by hand can take a
/// while.
pub(crate) const OFFER_EVERY: Duration = Duration::from_secs(1);
pub(crate) const MAX_OFFERS: u32 = 10;
pub(crate) const OFFER_EVERY_SLOW: Duration = Duration::from_secs(10);

/// How long to wait before the next offer, after `offers` of them: once a
/// second for the first [`MAX_OFFERS`], then every [`OFFER_EVERY_SLOW`] - never
/// "stop": a client typing its 2FA code for longer than ten seconds still
/// punches once it is admitted (review 126: giving up after ten went unseen).
pub(crate) fn offer_interval(offers: u32) -> Duration {
    if offers >= MAX_OFFERS {
        OFFER_EVERY_SLOW
    } else {
        OFFER_EVERY
    }
}

/// How long a confirmed direct path may be silent before the client goes back
/// to both ways and punches again (build 126). Above QUIC's keepalive of a
/// second, so a quiet but live path is never taken for a dead one.
pub(crate) const DIRECT_SILENCE: Duration = Duration::from_millis(1500);

const _: () = assert!(DIRECT_SILENCE.as_millis() > crate::KEEPALIVE.as_millis());

/// How often the client looks whether an offer is due.
const OFFER_TICK: Duration = Duration::from_millis(250);

/// The server sends a client's packets directly only once nothing of that
/// client came through the relay for this long: the client has confirmed its
/// direct path and stopped sending a copy. Above QUIC's keepalive of a second,
/// so a quiet client that still sends both ways is never taken for one that
/// stopped.
pub(crate) const RELAY_QUIET: Duration = Duration::from_secs(2);

const _: () = assert!(RELAY_QUIET.as_millis() > crate::KEEPALIVE.as_millis());

/// Whether `addr` is a place a punch may go to at all: a port, one host - not
/// "any", not a group, not everyone on a network (review 125). Loopback and
/// private addresses stay allowed: the tests run on loopback, and a relay in
/// the server's own network reports a private address in good faith.
pub(crate) fn usable_target(addr: SocketAddr) -> bool {
    let ip = addr.ip();
    let broadcast = matches!(ip, std::net::IpAddr::V4(v4) if v4.is_broadcast());
    addr.port() != 0 && !ip.is_unspecified() && !ip.is_multicast() && !broadcast
}

/// Which side of a connection takes the punch signals: a client through the
/// relay, the server with a relay, or neither (the direct method 1, where
/// there is nothing to punch).
#[derive(Clone)]
pub(crate) enum Side {
    Plain,
    Client(Arc<ClientRelaySocket>),
    Server(Arc<ServerRouteSocket>),
}

/// Whether datagram `d` is a punch signal - then it is handled here and goes no
/// further. A length and two bytes checked for every other datagram, nothing more.
pub(crate) fn intercept(side: &Side, conn: &Connection, d: &[u8]) -> bool {
    let is_punch = d.len() >= Header::SIZE
        && d[0] == MAGIC
        && (d[2] == PacketType::PunchOffer as u8 || d[2] == PacketType::PunchReady as u8 || d[2] == PacketType::PunchLocal as u8);
    if !is_punch {
        return false;
    }
    match (Packet::deserialize(d), side) {
        (Ok(Packet::PunchOffer(public)), Side::Server(route)) => match relay_peer_id(conn.remote_address()) {
            // A relay-only server punches for no one (build 135): it would send
            // from a socket of its own, straight to the client.
            Some(id) if route.is_relay_only() => {
                if route.first_relay_only_offer(conn.stable_id()) {
                    // Named in the brief before it was built (working rule 11, build 135).
                    info!("QUIC punch: client {id} offered a direct path - not answered, this server takes the relay only");
                }
            }
            Some(id) if !route.is_proven(conn.stable_id()) => {
                if route.first_unproven_offer(conn.stable_id()) {
                    info!("QUIC punch: client {id} offered before its password was proven - answered once it is");
                } else {
                    debug!("QUIC punch: an offer before the password was proven - not answered");
                }
            }
            Some(id) => match route.punch(id, public, conn.stable_id()) {
                Some(Answer::Punched(ready)) => signal(conn, PacketType::PunchReady, ready),
                Some(Answer::Local(home)) => signal(conn, PacketType::PunchLocal, home),
                None => {}
            },
            None => debug!("QUIC punch: an offer from a client that is not on the relay - nothing to punch"),
        },
        (Ok(Packet::PunchReady(from)), Side::Client(socket)) => socket.ready(from),
        (Ok(Packet::PunchLocal(home)), Side::Client(socket)) => socket.ready_local(home),
        (Ok(_), _) => debug!("QUIC punch: a signal for the other side - dropped"),
        (Err(e), _) => debug!("QUIC punch: a malformed signal - dropped: {e}"),
    }
    true
}

/// The client's offers on one connection, for as long as it lives.
pub(crate) async fn client_offers(conn: Connection, socket: Arc<ClientRelaySocket>) {
    let mut tick = tokio::time::interval(OFFER_TICK);
    loop {
        tokio::select! {
            _ = conn.closed() => return,
            _ = tick.tick() => {}
        }
        if let Some(public) = socket.offer_due() {
            signal(&conn, PacketType::PunchOffer, public);
        }
    }
}

fn signal(conn: &Connection, kind: PacketType, addr: SocketAddr) {
    let mut buf = [0u8; PunchPacket::MAX_SIZE];
    let n = PunchPacket::serialize(kind, addr, &mut buf);
    if let Err(e) = conn.send_datagram(Bytes::copy_from_slice(&buf[..n])) {
        debug!("QUIC punch: signal not sent: {e}");
    }
}
