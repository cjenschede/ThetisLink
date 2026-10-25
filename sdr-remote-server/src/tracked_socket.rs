// SPDX-License-Identifier: GPL-2.0-or-later

//! Thin wrapper around the client-facing QUIC endpoint that, per client address,
//! counts the sent (tx: server->client) and received (rx: client->server) bytes,
//! for the bandwidth display in the Status panel.
//!
//! The method names (`send_to`/`try_send_to`/`recv_from`) are those of
//! `tokio::net::UdpSocket`, which this stood in for until build 133, so the send
//! sites read as they did over UDP. The address argument accepts both
//! `SocketAddr` and `&SocketAddr` (via `Borrow`), just as the callers do.
//!
//! Counting is lock-free per client (`AtomicU64`); the addr->counter map uses a
//! read-mostly `RwLock` (write only when a new client address appears for the
//! first time). The counter lookup sits in the 20 ms network tick, not in the
//! audio callback.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use tokio::sync::mpsc;
use tokio::sync::Mutex as AsyncMutex;

/// Cumulative byte counters for a client address. TX (server->client) is
/// broken down per category so it is visible what consumes the bandwidth
/// (e.g. "audio only" that turns out to contain spectrum anyway). RX stays a single counter.
#[derive(Default)]
pub struct ClientBw {
    pub tx_audio: AtomicU64,
    pub tx_spectrum: AtomicU64,
    pub tx_other: AtomicU64,
    pub rx: AtomicU64,
}

/// Packet category derived from the type byte (buf[2], after MAGIC+VERSION).
enum Cat { Audio, Spectrum, Other }

fn classify(buf: &[u8]) -> Cat {
    // Only when MAGIC matches is buf[2] the PacketType byte. Classification
    // uses the single source of truth in core (PacketType::is_audio/is_spectrum)
    // so a new Audio*/Spectrum* variant is categorised correctly without
    // editing a literal byte-set here.
    if buf.len() < 3 || buf[0] != sdr_remote_core::protocol::MAGIC {
        return Cat::Other;
    }
    match sdr_remote_core::protocol::PacketType::from_u8(buf[2]) {
        Some(pt) if pt.is_audio() => Cat::Audio,
        Some(pt) if pt.is_spectrum() => Cat::Spectrum,
        _ => Cat::Other,
    }
}

/// A sample of the counters for a client (cumulative, bytes).
pub struct BwSample {
    pub addr: SocketAddr,
    pub tx_audio: u64,
    pub tx_spectrum: u64,
    pub tx_other: u64,
    pub rx: u64,
}

fn registry() -> &'static RwLock<HashMap<SocketAddr, Arc<ClientBw>>> {
    static R: OnceLock<RwLock<HashMap<SocketAddr, Arc<ClientBw>>>> = OnceLock::new();
    R.get_or_init(|| RwLock::new(HashMap::new()))
}

fn counter(addr: SocketAddr) -> Arc<ClientBw> {
    if let Some(c) = registry().read().unwrap().get(&addr) {
        return c.clone();
    }
    registry().write().unwrap().entry(addr).or_default().clone()
}

fn record_tx(c: &ClientBw, buf: &[u8], n: u64) {
    match classify(buf) {
        Cat::Audio => c.tx_audio.fetch_add(n, Ordering::Relaxed),
        Cat::Spectrum => c.tx_spectrum.fetch_add(n, Ordering::Relaxed),
        Cat::Other => c.tx_other.fetch_add(n, Ordering::Relaxed),
    };
}

/// Cumulative per-client byte snapshot for the UI.
pub fn bw_snapshot() -> Vec<BwSample> {
    registry()
        .read()
        .unwrap()
        .iter()
        .map(|(a, c)| BwSample {
            addr: *a,
            tx_audio: c.tx_audio.load(Ordering::Relaxed),
            tx_spectrum: c.tx_spectrum.load(Ordering::Relaxed),
            tx_other: c.tx_other.load(Ordering::Relaxed),
            rx: c.rx.load(Ordering::Relaxed),
        })
        .collect()
}

/// Remove counters for addresses that are no longer active (prevents the map
/// from growing on reconnect - each new source port is a new address). The UI
/// calls this periodically with the currently active client addresses.
pub fn bw_retain(active: &[SocketAddr]) {
    let mut w = registry().write().unwrap();
    w.retain(|a, _| active.contains(a));
}

/// The server's client-facing socket: the QUIC endpoint, straight and through
/// the relay (since build 133 the only one; plain UDP and the classic relay
/// tunnel are gone). Each QUIC connection is seen at a sentinel address of its
/// own (`198.51.100.0/24`), so all `SocketAddr`-keyed session, subscriber and
/// TX logic works unchanged.
pub struct TrackedSocket {
    quic: QuicPath,
    /// Step 4 (build 256, a development build only): what `trial.rs` plays in,
    /// read by `recv_from` beside the QUIC endpoint.
    #[cfg(any(test, feature = "naspelen"))]
    trial: Option<AsyncMutex<mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)>>>,
}

/// Which way a client over QUIC takes (build 143), for the status panel.
pub struct WaysOf(pub sdr_remote_quic::Ways);

impl crate::session::WayLookup for WaysOf {
    fn way(&self, addr: SocketAddr) -> Option<sdr_remote_core::path::PathKind> {
        self.0.of(addr)
    }
}

/// The permit to punch a hole (phase 3c) for a client over QUIC: granted when
/// the session layer admits it, taken back when its session ends.
pub struct PunchPermits(pub sdr_remote_quic::PunchGate);

impl crate::session::AdmissionWatch for PunchPermits {
    fn admitted(&self, addr: SocketAddr) {
        if sdr_remote_quic::is_quic_sentinel(addr) {
            self.0.allow(addr);
        }
    }

    fn gone(&self, addr: SocketAddr) {
        if sdr_remote_quic::is_quic_sentinel(addr) {
            self.0.revoke(addr);
        }
    }
}

/// The QUIC listener and its channels. The listener is kept here because
/// dropping it closes the port.
struct QuicPath {
    server: sdr_remote_quic::QuicServer,
    uplink: mpsc::UnboundedSender<(SocketAddr, Vec<u8>)>,
    inbound: AsyncMutex<mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)>>,
}

/// Copy a frame into the receive buffer, or say it does not fit: a frame
/// larger than the buffer is dropped whole, never cut off into a packet that
/// parses as something else (review 109).
use sdr_remote_core::take_frame;

/// What a closed QUIC connection reads as: its client's own goodbye, so the one
/// Disconnect path ends the session, releases its transmitter and its CW key -
/// at once, not after the session timeout with a ghost holding the radio
/// (review 110).
/// `None` when the buffer cannot hold it - checked like `take_frame` (review 111).
fn goodbye(buf: &mut [u8]) -> Option<usize> {
    let header = sdr_remote_core::protocol::Header::new(
        sdr_remote_core::protocol::PacketType::Disconnect,
        sdr_remote_core::protocol::Flags::NONE,
    );
    let mut hdr = [0u8; 4];
    header.serialize(&mut hdr);
    buf.get_mut(..4)?.copy_from_slice(&hdr);
    Some(4)
}

impl TrackedSocket {
    pub fn new(server: sdr_remote_quic::QuicServer, link: sdr_remote_quic::QuicServerLink) -> Self {
        Self {
            quic: QuicPath {
                server,
                uplink: link.uplink_tx,
                inbound: AsyncMutex::new(link.inbound_rx),
            },
            #[cfg(any(test, feature = "naspelen"))]
            trial: None,
        }
    }

    /// The same socket with a second way in, for the trial (build 256): what goes
    /// into the returned sender is read as if it came off the network.
    #[cfg(any(test, feature = "naspelen"))]
    pub fn with_trial_inbound(mut self) -> (Self, mpsc::UnboundedSender<(SocketAddr, Vec<u8>)>) {
        let (tx, rx) = mpsc::unbounded_channel();
        self.trial = Some(AsyncMutex::new(rx));
        (self, tx)
    }

    /// The channel an authentication from `addr` came on: the live QUIC
    /// connection's binding and the peer's real address. `Err` when there is no
    /// such connection (gone, or never one) - an answer from there verifies
    /// against nothing and is not answered.
    pub fn auth_channel(&self, addr: SocketAddr) -> Result<crate::session::AuthChannel, ()> {
        let binding = self.quic.server.bindings.get(addr).ok_or(())?;
        Ok(crate::session::AuthChannel { binding, peer: self.quic.server.bindings.peer(addr) })
    }

    /// The address the brake on password guessing counts for `addr`: the real
    /// peer behind a QUIC sentinel (a new sentinel per connection would restart
    /// the count), the address itself when that is not known. One source: the
    /// same lookup as `auth_channel`.
    pub fn throttle_key(&self, addr: SocketAddr) -> SocketAddr {
        self.auth_channel(addr).ok().and_then(|c| c.peer).unwrap_or(addr)
    }

    fn send(&self, buf: &[u8], addr: SocketAddr) -> std::io::Result<usize> {
        if !sdr_remote_quic::is_quic_sentinel(addr) {
            return Err(std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "not a QUIC client"));
        }
        // Byte counting (Hook C) for what goes to the connection.
        record_tx(&counter(addr), buf, buf.len() as u64);
        let _ = self.quic.uplink.send((addr, buf.to_vec()));
        Ok(buf.len())
    }

    pub async fn send_to(&self, buf: &[u8], target: impl Borrow<SocketAddr>) -> std::io::Result<usize> {
        self.send(buf, *target.borrow())
    }

    pub fn try_send_to(&self, buf: &[u8], target: impl Borrow<SocketAddr>) -> std::io::Result<usize> {
        self.send(buf, *target.borrow())
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        // Only consumer (the network run loop), so the lock has no contention.
        let mut inbound = self.quic.inbound.lock().await;
        #[cfg(any(test, feature = "naspelen"))]
        let mut trial = match &self.trial {
            Some(t) => Some(t.lock().await),
            None => None,
        };
        loop {
            #[cfg(any(test, feature = "naspelen"))]
            let next = match trial.as_mut() {
                Some(t) => tokio::select! {
                    q = inbound.recv() => q,
                    Some((addr, data)) = t.recv() => {
                        let Some(n) = take_frame(&data, buf) else { continue };
                        return Ok((n, addr));
                    }
                },
                None => inbound.recv().await,
            };
            #[cfg(not(any(test, feature = "naspelen")))]
            let next = inbound.recv().await;
            let Some((addr, data)) = next else {
                return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "QUIC endpoint stopped"));
            };
            // Empty: that connection closed, after its last frame.
            if data.is_empty() {
                log::info!("QUIC: connection {addr} closed - its session ends now");
                let Some(n) = goodbye(buf) else { continue };
                return Ok((n, addr));
            }
            let Some(n) = take_frame(&data, buf) else { continue };
            counter(addr).rx.fetch_add(n as u64, Ordering::Relaxed);
            return Ok((n, addr));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The goodbye a closed QUIC connection reads as: a Disconnect, and only
    /// when the buffer holds it - never a panic in a receive path (review 111).
    #[test]
    fn a_goodbye_fits_or_is_not_written() {
        let mut small = [0u8; 3];
        assert_eq!(goodbye(&mut small), None);
        let mut buf = [0u8; 4];
        assert_eq!(goodbye(&mut buf), Some(4));
        assert!(matches!(
            sdr_remote_core::protocol::Packet::deserialize(&buf),
            Ok(sdr_remote_core::protocol::Packet::Disconnect)
        ));
    }

    /// The relay reads the PTT flag straight out of the header byte with its own
    /// bit mask, because it does not link core. That mirror had no guard, and it
    /// sits on the PTT path: if core ever moves the flag, the relay would keep
    /// looking at the old bit and TX-always-both would quietly stop working for
    /// the frames that matter most.
    #[test]
    fn ptt_bit_matches_core() {
        assert_eq!(
            sdr_remote_relay::PTT_FLAG_BIT,
            sdr_remote_core::protocol::Flags::PTT.0,
            "relay PTT bit drifted from core Flags::PTT"
        );
        // And it must be exactly one bit - a widened mask would make unrelated
        // flags read as TX.
        assert_eq!(
            sdr_remote_relay::PTT_FLAG_BIT.count_ones(),
            1,
            "the PTT mask must select a single bit"
        );
    }

    #[test]
    fn relay_audio_types_match_core() {
        // The relay stays dependency-light and mirrors the audio type-byte set
        // as a literal (sdr_remote_relay::AUDIO_TYPE_BYTES) instead of linking
        // core. This test — in the one crate that links BOTH relay and core —
        // is the guard: it fails the build if that mirror ever drifts from the
        // authoritative sdr_remote_core::protocol::AUDIO_PACKET_TYPES.
        let mut relay_set = sdr_remote_relay::AUDIO_TYPE_BYTES.to_vec();
        let mut core_set = sdr_remote_core::protocol::AUDIO_PACKET_TYPES.to_vec();
        relay_set.sort_unstable();
        core_set.sort_unstable();
        assert_eq!(relay_set, core_set, "relay audio type-bytes drifted from core");

        // And every possible byte routes the same way on both sides.
        for b in 0u8..=255 {
            let relay_audio = sdr_remote_relay::AUDIO_TYPE_BYTES.contains(&b);
            let core_audio = sdr_remote_core::protocol::PacketType::from_u8(b)
                .map_or(false, |pt| pt.is_audio());
            assert_eq!(relay_audio, core_audio, "routing mismatch for 0x{b:02X}");
        }
    }

    /// The trial's way in (build 256): what goes into it is read as a packet from
    /// the trial client, beside the QUIC endpoint; without it nothing changes.
    #[tokio::test]
    async fn the_trial_way_in_is_read_beside_quic() {
        let (server, link) = sdr_remote_quic::QuicServer::start(
            "127.0.0.1:0".parse().unwrap(),
            sdr_remote_quic::Congestion::Bbr,
        )
        .unwrap();
        let (sock, to_server) = TrackedSocket::new(server, link).with_trial_inbound();
        to_server.send((crate::trial::TRIAL_CLIENT, b"played in".to_vec())).unwrap();
        let mut buf = [0u8; 64];
        let (n, from) = tokio::time::timeout(std::time::Duration::from_secs(5), sock.recv_from(&mut buf))
            .await
            .expect("in time")
            .unwrap();
        assert_eq!((&buf[..n], from), (&b"played in"[..], crate::trial::TRIAL_CLIENT));
        // Sending to it goes nowhere: it is no QUIC client.
        assert!(sock.try_send_to(b"x", crate::trial::TRIAL_CLIENT).is_err());
    }

    /// QUIC end to end through the socket: a QUIC client's packet arrives with a
    /// sentinel of its own range, a reply to that sentinel goes back over its
    /// connection, the binding is the connection's - and an address that is no
    /// QUIC client has no channel and gets nothing (build 133: no plain UDP).
    #[tokio::test]
    async fn quic_clients_arrive_and_are_answered_through_the_socket() {
        let (server, link) = sdr_remote_quic::QuicServer::start(
            "127.0.0.1:0".parse().unwrap(),
            sdr_remote_quic::Congestion::Bbr,
        )
        .unwrap();
        let quic_addr = server.local_addr().unwrap();
        let sock = TrackedSocket::new(server, link);

        let mut client = sdr_remote_quic::QuicClient::start(sdr_remote_quic::Congestion::Bbr).unwrap();
        client.uplink_tx.send((quic_addr, b"hello over quic".to_vec())).unwrap();
        let mut buf = [0u8; 64];
        let (n, from) = tokio::time::timeout(std::time::Duration::from_secs(5), sock.recv_from(&mut buf))
            .await
            .expect("the QUIC packet in time")
            .unwrap();
        assert_eq!(&buf[..n], b"hello over quic");
        assert!(sdr_remote_quic::is_quic_sentinel(from), "tagged with a QUIC sentinel, not {from}");

        let client_binding = *client.binding.wait_for(|b| b.is_some()).await.unwrap();
        let channel = sock.auth_channel(from).unwrap();
        assert_eq!(Some(channel.binding), client_binding, "the connection's binding");
        let peer = channel.peer.expect("the real peer is known");
        assert_eq!(peer.ip(), "127.0.0.1".parse::<std::net::IpAddr>().unwrap(), "the real peer, not the sentinel");
        assert_eq!(sock.throttle_key(from), peer, "the brake counts the real peer");
        let udp: SocketAddr = "192.168.1.97:4580".parse().unwrap();
        assert!(sock.auth_channel(udp).is_err(), "no QUIC connection, no channel");
        assert_eq!(sock.throttle_key(udp), udp, "an unknown address counts itself");
        assert!(sock.auth_channel("198.51.100.200:7".parse().unwrap()).is_err(), "a QUIC sentinel with no connection");
        assert!(sock.send_to(b"x", udp).await.is_err(), "nothing goes to an address that is no QUIC client");

        sock.send_to(b"reply", from).await.unwrap();
        let (tag, back) = tokio::time::timeout(std::time::Duration::from_secs(5), client.inbound_rx.recv())
            .await
            .expect("the reply in time")
            .unwrap();
        assert_eq!((tag, back.as_slice()), (quic_addr, &b"reply"[..]));

        // The QUIC client leaves: its connection closes, and the socket reads
        // that as the client's own Disconnect, from its sentinel (review 110).
        drop(client);
        let (n, gone) = tokio::time::timeout(std::time::Duration::from_secs(5), sock.recv_from(&mut buf))
            .await
            .expect("the close in time")
            .unwrap();
        assert_eq!(gone, from, "the session that ends is that connection's");
        assert!(
            matches!(sdr_remote_core::protocol::Packet::deserialize(&buf[..n]), Ok(sdr_remote_core::protocol::Packet::Disconnect)),
            "read as a Disconnect"
        );
    }

    /// The authentication in the network loop takes its binding from this
    /// socket, for the address the response came from - not a stored one, and
    /// not skipped (review 108, point 4.2). A text pin: the call sits in the
    /// receive loop, which no test drives.
    #[test]
    fn the_auth_takes_the_binding_of_the_connection_it_came_on() {
        let src = include_str!("network.rs");
        let lines: Vec<&str> = src.lines().map(str::trim).collect();
        let at = |l: &str| lines.iter().position(|x| *x == l);
        let fetch = at("let Ok(channel) = self.socket.auth_channel(addr) else {").expect("the channel is fetched");
        let verify = at("let verdict = session.verify_auth(addr, hmac, *client_nonce, channel);").expect("and handed over");
        assert!(fetch < verify && verify - fetch <= 4, "fetched right before the verification");
        assert_eq!(src.matches(".verify_auth(").count(), 1, "one place verifies");
    }

    /// What travels on a QUIC stream has room for any packet the protocol
    /// makes; a frame that does not fit the buffer is dropped whole, not cut
    /// off (review 109; `take_frame`).
    #[test]
    fn a_stream_holds_any_packet() {
        assert!(sdr_remote_quic::MAX_STREAM_PACKET >= sdr_remote_core::MAX_PACKET_SIZE);
    }
}
