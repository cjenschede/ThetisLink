// SPDX-License-Identifier: GPL-2.0-or-later

//! Dunne wrapper om de client-facing UDP-socket die per client-adres de
//! verzonden (tx: server->client) en ontvangen (rx: client->server) bytes telt,
//! voor de bandbreedte-weergave in het Status-paneel.
//!
//! De methodenamen (`send_to`/`try_send_to`/`recv_from`) zijn identiek aan die
//! van `tokio::net::UdpSocket`, zodat alle bestaande verzend-plekken ongewijzigd
//! blijven - alleen het socket-type wijzigt. Het adres-argument accepteert zowel
//! `SocketAddr` als `&SocketAddr` (via `Borrow`), net als de aanroepen doen.
//!
//! Tellen is lock-vrij per client (`AtomicU64`); de addr->teller-map gebruikt een
//! read-mostly `RwLock` (write alleen wanneer een nieuw client-adres voor het
//! eerst verschijnt). De teller-lookup zit in de 20 ms-netwerktick, niet in de
//! audio-callback.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::sync::Mutex as AsyncMutex;

/// Cumulatieve byte-tellers voor een client-adres. TX (server->client) is
/// uitgesplitst per categorie zodat zichtbaar is wat de bandbreedte opslokt
/// (bv. "alleen audio" dat toch spectrum blijkt te bevatten). RX blijft een teller.
#[derive(Default)]
pub struct ClientBw {
    pub tx_audio: AtomicU64,
    pub tx_spectrum: AtomicU64,
    pub tx_other: AtomicU64,
    pub rx: AtomicU64,
}

/// Pakketcategorie afgeleid uit het type-byte (buf[2], na MAGIC+VERSION).
enum Cat { Audio, Spectrum, Other }

fn classify(buf: &[u8]) -> Cat {
    // 0xAA = MAGIC (protocol.rs). Alleen dan is buf[2] het PacketType-byte.
    if buf.len() < 3 || buf[0] != 0xAA {
        return Cat::Other;
    }
    match buf[2] {
        // Audio: Audio, AudioRx2, AudioYaesu, AudioBinR, AudioMultiCh, AudioVrx, AudioYaesu2
        0x01 | 0x0E | 0x16 | 0x1A | 0x1B | 0x21 | 0x25 => Cat::Audio,
        // Spectrum: Spectrum, FullSpectrum, SpectrumRx2, FullSpectrumRx2, SpectrumVrx1/2
        0x0A | 0x0B | 0x12 | 0x13 | 0x23 | 0x24 => Cat::Spectrum,
        _ => Cat::Other,
    }
}

/// Een sample van de tellers voor een client (cumulatief, bytes).
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

/// Cumulatieve per-client byte-snapshot voor de UI.
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

/// Verwijder tellers van adressen die niet meer actief zijn (voorkomt dat de map
/// groeit bij herverbinden - elke nieuwe bron-poort is een nieuw adres). De UI
/// roept dit periodiek aan met de huidige actieve client-adressen.
pub fn bw_retain(active: &[SocketAddr]) {
    let mut w = registry().write().unwrap();
    w.retain(|a, _| active.contains(a));
}

// --- Relay-sentinel-adressen (Phase C) ---
//
// Een relay-getunnelde client heeft geen echte UDP-SocketAddr. We geven hem een
// synthetische sentinel-addr uit RFC 5737 TEST-NET-3 (203.0.113.0/24): dat bereik
// is gereserveerd voor documentatie en is nooit routeerbaar / nooit de bron van
// een echte peer. Zo werkt alle SocketAddr-gekeyde sessie-/subscriber-/TX-logica
// ongewijzigd (SocketAddr is daar puur een opaque sleutel), terwijl de outbound
// send naar zo'n addr hier wordt afgevangen en naar de relay-uplink gaat i.p.v.
// naar een echte UDP-bestemming.
const RELAY_SENTINEL_PORT: u16 = 1;

/// True als `addr` in het gereserveerde relay-sentinel-bereik valt (203.0.113.0/24).
pub fn is_relay_sentinel(addr: SocketAddr) -> bool {
    match addr.ip() {
        std::net::IpAddr::V4(ip) => {
            let o = ip.octets();
            o[0] == 203 && o[1] == 0 && o[2] == 113
        }
        std::net::IpAddr::V6(_) => false,
    }
}

/// Synthetische sentinel-addr voor relay-client `client_ix` (v1 gebruikt 0).
/// Mapt naar 203.0.113.(1..=254); nooit .0 (netwerk) of .255 (broadcast).
pub fn relay_sentinel_for(client_ix: u8) -> SocketAddr {
    let last = 1 + (client_ix % 254);
    SocketAddr::from((Ipv4Addr::new(203, 0, 113, last), RELAY_SENTINEL_PORT))
}

/// UDP-socket met per-client byte-telling. Zelfde API-oppervlak als de delen van
/// `tokio::net::UdpSocket` die de server gebruikt. Optioneel transport-bewust: als
/// relay-kanalen zijn gekoppeld gaan sends naar sentinel-addrs naar de relay-uplink
/// en levert `recv_from` ook getunnelde inbound frames af (Phase C). Zonder die
/// kanalen (default via `new`) is het gedrag byte-identiek aan direct-UDP.
pub struct TrackedSocket {
    inner: UdpSocket,
    /// Outbound naar een sentinel-addr wordt hierheen geduwd i.p.v. UDP.
    relay_uplink: Option<mpsc::UnboundedSender<(SocketAddr, Vec<u8>)>>,
    /// Getunnelde inbound `(sentinel, tl_bytes)` die de relay-taak hierin duwt.
    relay_inbound: Option<AsyncMutex<mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)>>>,
}

impl TrackedSocket {
    /// Direct-only (geen relay). Gedrag byte-identiek aan de kale `UdpSocket`.
    pub fn new(inner: UdpSocket) -> Self {
        Self {
            inner,
            relay_uplink: None,
            relay_inbound: None,
        }
    }

    /// Transport-bewust: sends naar sentinel-addrs gaan naar `uplink`, en
    /// `recv_from` selecteert ook over `inbound`. Direct-verkeer blijft ongewijzigd.
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
            // Nooit een echte UDP-send naar een sentinel. Byte-telling (Hook C) MOET
            // hier gebeuren: dit gaat voor en i.p.v. inner.send_to.
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
                // Enige consument (de netwerk-run-loop), dus de lock kent geen contentie.
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

    #[test]
    fn sentinel_range_no_collision_with_real_peers() {
        // Realistische echte peer-addrs mogen NOOIT als sentinel gelden.
        for a in [
            "192.168.1.50:4580",
            "10.0.0.2:5000",
            "8.8.8.8:53",
            "127.0.0.1:4580",
            "203.0.114.1:1", // net buiten TEST-NET-3
            "[::1]:4580",
        ] {
            assert!(
                !is_relay_sentinel(a.parse().unwrap()),
                "{a} ten onrechte als sentinel geflagd"
            );
        }
        // En de uitgegeven sentinels vallen er wel in.
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

        // Intercept: het frame verscheen op de uplink met de sentinel-addr...
        let (addr, data) = up_rx.try_recv().expect("uplink kreeg geen frame");
        assert_eq!(addr, sentinel);
        assert_eq!(&data, payload);
        // no-real-send: er staat niets meer in de uplink (precies een frame).
        assert!(up_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn real_addr_still_goes_over_udp() {
        // Intercept-tegenproef: een echt (niet-sentinel) addr gaat via UDP.
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
        // De uplink kreeg NIETS (echt addr -> UDP, niet de tunnel).
        assert!(up_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn relay_inbound_delivered_via_recv_from() {
        // Seam unit-test hook: de test bezit de inbound-sender en duwt
        // (sentinel, bytes) erin; recv_from levert ze byte-identiek + met sentinel-addr.
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
