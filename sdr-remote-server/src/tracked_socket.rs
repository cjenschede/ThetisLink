// SPDX-License-Identifier: GPL-2.0-or-later

//! Thin wrapper around the client-facing UDP socket that, per client address,
//! counts the sent (tx: server->client) and received (rx: client->server) bytes,
//! for the bandwidth display in the Status panel.
//!
//! The method names (`send_to`/`try_send_to`/`recv_from`) are identical to those
//! of `tokio::net::UdpSocket`, so that all existing send sites remain unchanged -
//! only the socket type changes. The address argument accepts both
//! `SocketAddr` and `&SocketAddr` (via `Borrow`), just as the callers do.
//!
//! Counting is lock-free per client (`AtomicU64`); the addr->counter map uses a
//! read-mostly `RwLock` (write only when a new client address appears for the
//! first time). The counter lookup sits in the 20 ms network tick, not in the
//! audio callback.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use tokio::net::UdpSocket;
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

// --- Relay sentinel addresses (Phase C) ---
//
// A relay-tunneled client has no real UDP SocketAddr. We assign it a
// synthetic sentinel addr from RFC 5737 TEST-NET-3 (203.0.113.0/24): that range
// is reserved for documentation and is never routable / never the source of
// a real peer. This way all SocketAddr-keyed session/subscriber/TX logic works
// unchanged (SocketAddr is purely an opaque key there), while the outbound
// send to such an addr is intercepted here and goes to the relay uplink instead
// of to a real UDP destination.
const RELAY_SENTINEL_PORT: u16 = 1;

/// True if `addr` falls within the reserved relay sentinel range (203.0.113.0/24).
pub fn is_relay_sentinel(addr: SocketAddr) -> bool {
    match addr.ip() {
        std::net::IpAddr::V4(ip) => {
            let o = ip.octets();
            o[0] == 203 && o[1] == 0 && o[2] == 113
        }
        std::net::IpAddr::V6(_) => false,
    }
}

/// Synthetic sentinel addr for relay client `client_ix` (v1 uses 0).
/// Maps to 203.0.113.(1..=254); never .0 (network) or .255 (broadcast).
pub fn relay_sentinel_for(client_ix: u8) -> SocketAddr {
    let last = 1 + (client_ix % 254);
    SocketAddr::from((Ipv4Addr::new(203, 0, 113, last), RELAY_SENTINEL_PORT))
}

/// UDP socket with per-client byte counting. Same API surface as the parts of
/// `tokio::net::UdpSocket` that the server uses. Optionally transport-aware: if
/// relay channels are attached, sends to sentinel addrs go to the relay uplink
/// and `recv_from` also delivers tunneled inbound frames (Phase C). Without those
/// channels (default via `new`) the behavior is byte-identical to direct UDP.
pub struct TrackedSocket {
    inner: UdpSocket,
    /// Outbound to a sentinel addr is pushed here instead of UDP.
    relay_uplink: Option<mpsc::UnboundedSender<(SocketAddr, Vec<u8>)>>,
    /// Tunneled inbound `(sentinel, tl_bytes)` that the relay task pushes in here.
    relay_inbound: Option<AsyncMutex<mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)>>>,
}

impl TrackedSocket {
    /// Direct-only (no relay). Behavior byte-identical to the bare `UdpSocket`.
    pub fn new(inner: UdpSocket) -> Self {
        Self {
            inner,
            relay_uplink: None,
            relay_inbound: None,
        }
    }

    /// Transport-aware: sends to sentinel addrs go to `uplink`, and
    /// `recv_from` also selects over `inbound`. Direct traffic stays unchanged.
    pub fn with_relay(
        inner: UdpSocket,
        uplink: mpsc::UnboundedSender<(SocketAddr, Vec<u8>)>,
        inbound: mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)>,
    ) -> Self {
        Self {
            inner,
            relay_uplink: Some(uplink),
            relay_inbound: Some(AsyncMutex::new(inbound)),
        }
    }

    pub async fn send_to(&self, buf: &[u8], target: impl Borrow<SocketAddr>) -> std::io::Result<usize> {
        let addr = *target.borrow();
        if is_relay_sentinel(addr) {
            // Never a real UDP send to a sentinel. Byte counting (Hook C) MUST
            // happen here: this goes before and instead of inner.send_to.
            record_tx(&counter(addr), buf, buf.len() as u64);
            if let Some(up) = &self.relay_uplink {
                let _ = up.send((addr, buf.to_vec()));
            }
            return Ok(buf.len());
        }
        let n = self.inner.send_to(buf, addr).await?;
        record_tx(&counter(addr), buf, n as u64);
        Ok(n)
    }

    pub fn try_send_to(&self, buf: &[u8], target: impl Borrow<SocketAddr>) -> std::io::Result<usize> {
        let addr = *target.borrow();
        if is_relay_sentinel(addr) {
            record_tx(&counter(addr), buf, buf.len() as u64);
            if let Some(up) = &self.relay_uplink {
                let _ = up.send((addr, buf.to_vec()));
            }
            return Ok(buf.len());
        }
        let n = self.inner.try_send_to(buf, addr)?;
        record_tx(&counter(addr), buf, n as u64);
        Ok(n)
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        match &self.relay_inbound {
            None => {
                let (n, addr) = self.inner.recv_from(buf).await?;
                counter(addr).rx.fetch_add(n as u64, Ordering::Relaxed);
                Ok((n, addr))
            }
            Some(inbound) => {
                // Only consumer (the network run loop), so the lock has no contention.
                let mut rx = inbound.lock().await;
                tokio::select! {
                    res = self.inner.recv_from(buf) => {
                        let (n, addr) = res?;
                        counter(addr).rx.fetch_add(n as u64, Ordering::Relaxed);
                        Ok((n, addr))
                    }
                    Some((addr, data)) = rx.recv() => {
                        let n = data.len().min(buf.len());
                        buf[..n].copy_from_slice(&data[..n]);
                        counter(addr).rx.fetch_add(n as u64, Ordering::Relaxed);
                        Ok((n, addr))
                    }
                }
            }
        }
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn sentinel_mapping_matches_relay() {
        // The relay owns the sentinel convention; this server mirror
        // (relay_sentinel_for / is_relay_sentinel) must agree with it for every
        // client_id, or tunneled frames route to the wrong client. Guarded here
        // because only this crate links both relay and server.
        for id in 0u8..=255 {
            let server_addr = relay_sentinel_for(id);
            let relay_addr = sdr_remote_relay::sentinel_for_client_id(id);
            assert_eq!(server_addr, relay_addr, "sentinel addr drift for id {id}");
            assert!(is_relay_sentinel(server_addr), "id {id} not seen as sentinel");
            // Relay's inverse recovers the id within the 254-wide range.
            assert_eq!(sdr_remote_relay::client_id_from_sentinel(relay_addr), id % 254);
        }
    }

    #[test]
    fn sentinel_range_no_collision_with_real_peers() {
        // Realistic real peer addrs must NEVER count as a sentinel.
        for a in [
            "192.168.1.50:4580",
            "10.0.0.2:5000",
            "8.8.8.8:53",
            "127.0.0.1:4580",
            "198.51.100.1:1", // TEST-NET-2 (reserved, non-routable), outside the sentinel range
            "[::1]:4580",
        ] {
            assert!(
                !is_relay_sentinel(a.parse().unwrap()),
                "{a} ten onrechte als sentinel geflagd"
            );
        }
        // And the issued sentinels do fall within it.
        assert!(is_relay_sentinel(relay_sentinel_for(0)));
        assert!(is_relay_sentinel(relay_sentinel_for(9)));
        assert!(is_relay_sentinel("203.0.113.200:1".parse().unwrap()));
    }

    #[tokio::test]
    async fn sentinel_outbound_goes_to_uplink_not_udp() {
        let inner = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (up_tx, mut up_rx) = mpsc::unbounded_channel();
        let (_in_tx, in_rx) = mpsc::unbounded_channel();
        let sock = TrackedSocket::with_relay(inner, up_tx, in_rx);

        let sentinel = relay_sentinel_for(0);
        let payload = b"hello-tunnel";
        let n = sock.send_to(payload, sentinel).await.unwrap();
        assert_eq!(n, payload.len());

        // Intercept: the frame appeared on the uplink with the sentinel addr...
        let (addr, data) = up_rx.try_recv().expect("uplink kreeg geen frame");
        assert_eq!(addr, sentinel);
        assert_eq!(&data, payload);
        // no-real-send: nothing more remains in the uplink (exactly one frame).
        assert!(up_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn real_addr_still_goes_over_udp() {
        // Intercept counter-check: a real (non-sentinel) addr goes over UDP.
        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let dest = receiver.local_addr().unwrap();
        let inner = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (up_tx, mut up_rx) = mpsc::unbounded_channel();
        let (_in_tx, in_rx) = mpsc::unbounded_channel();
        let sock = TrackedSocket::with_relay(inner, up_tx, in_rx);

        let payload = b"direct-udp";
        sock.send_to(payload, dest).await.unwrap();

        let mut buf = [0u8; 64];
        let (n, _from) = receiver.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], payload);
        // The uplink got NOTHING (real addr -> UDP, not the tunnel).
        assert!(up_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn relay_inbound_delivered_via_recv_from() {
        // Seam unit-test hook: the test owns the inbound sender and pushes
        // (sentinel, bytes) into it; recv_from delivers them byte-identical + with the sentinel addr.
        let inner = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (up_tx, _up_rx) = mpsc::unbounded_channel();
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let sock = TrackedSocket::with_relay(inner, up_tx, in_rx);

        let sentinel = relay_sentinel_for(0);
        let payload = b"inbound-frame";
        in_tx.send((sentinel, payload.to_vec())).unwrap();

        let mut buf = [0u8; 64];
        let (n, addr) = sock.recv_from(&mut buf).await.unwrap();
        assert_eq!(addr, sentinel);
        assert_eq!(&buf[..n], payload);
    }
}
