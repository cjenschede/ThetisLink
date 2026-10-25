// SPDX-License-Identifier: GPL-2.0-or-later
//
// What the two integration harnesses agree on. One number here instead of one
// in each file: review 78 found the wait for the connected flag written twice,
// with a deadline of three seconds in one place and five in the other, so the
// same regression would have been reported differently depending on which
// test met it.

/// How long a test waits for `connected` after the status reached Connected.
/// The flag follows the status by one heartbeat interval (measured: status at
/// +531 ms, flag at +1033 ms, with acks every 500 ms); a deadline much wider
/// than that hides a slow flag. One interval and a margin, no more.
#[allow(dead_code)]
pub const CONNECTED_FLAG_WAIT_MS: u64 = 1_500;

/// A scripted server over QUIC, with the shape the harnesses used for their
/// plain UDP socket: `recv_from`, `send_to`, `local_addr` (phase 3d, step 1:
/// the tests move to QUIC before the plain UDP path goes, so that each one can
/// be seen to say the same on the new carrier). A client is the sentinel its
/// connection has on the server; a connection that closes (an empty frame) is
/// passed over, as a UDP socket would never see it.
#[allow(dead_code)]
#[derive(Clone)]
pub struct FakeServer {
    server: std::sync::Arc<sdr_remote_quic::QuicServer>,
    inbound: std::sync::Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<(std::net::SocketAddr, Vec<u8>)>>>,
    uplink: tokio::sync::mpsc::UnboundedSender<(std::net::SocketAddr, Vec<u8>)>,
}

/// The binding of the connection each AuthResponse came on, by the client's
/// nonce in it: a server proof over QUIC is bound to that connection, and the
/// harnesses build their proof from the response alone. Client nonces are
/// random, so tests that run side by side do not meet each other here.
#[allow(dead_code)]
static BINDING_BY_NONCE: std::sync::Mutex<Option<std::collections::HashMap<[u8; 16], sdr_remote_quic::Binding>>> =
    std::sync::Mutex::new(None);

/// The binding the AuthResponse with client nonce `nonce` came on, if a
/// `FakeServer` received it.
#[allow(dead_code)]
pub fn binding_for_nonce(nonce: &[u8; 16]) -> Option<sdr_remote_quic::Binding> {
    BINDING_BY_NONCE.lock().unwrap().as_ref().and_then(|m| m.get(nonce).copied())
}

#[allow(dead_code)]
impl FakeServer {
    pub async fn start() -> FakeServer {
        Self::start_now()
    }

    /// As [`FakeServer::start`], for a caller that is not async (it must still
    /// run inside a tokio runtime).
    pub fn start_now() -> FakeServer {
        let (server, link) =
            sdr_remote_quic::QuicServer::start("127.0.0.1:0".parse().unwrap(), sdr_remote_quic::Congestion::Bbr).unwrap();
        let server = std::sync::Arc::new(server);
        // Kept until the test's runtime ends. A UDP socket the script dropped
        // after its last send had that datagram on the wire already; a QUIC
        // endpoint closes at its drop and takes what was still queued with it.
        // A test that wants silence simply sends nothing.
        let keep = server.clone();
        tokio::spawn(async move {
            let _keep = keep;
            std::future::pending::<()>().await
        });
        FakeServer {
            server,
            inbound: std::sync::Arc::new(tokio::sync::Mutex::new(link.inbound_rx)),
            uplink: link.uplink_tx,
        }
    }

    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.server.local_addr()
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, std::net::SocketAddr)> {
        let mut inbound = self.inbound.lock().await;
        loop {
            let Some((from, packet)) = inbound.recv().await else {
                return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the QUIC server stopped"));
            };
            if packet.is_empty() {
                continue; // a connection closed - nothing a UDP socket would see
            }
            if let Ok(sdr_remote_core::protocol::Packet::AuthResponse { client_nonce: Some(nc), .. }) =
                sdr_remote_core::protocol::Packet::deserialize(&packet)
            {
                if let Some(b) = self.server.bindings.get(from) {
                    BINDING_BY_NONCE.lock().unwrap().get_or_insert_with(Default::default).insert(nc, b);
                }
            }
            let n = packet.len().min(buf.len());
            buf[..n].copy_from_slice(&packet[..n]);
            return Ok((n, from));
        }
    }

    /// The client's AuthResponse, passing over what comes before it. The engine
    /// knocks with a heartbeat every 500 ms from the moment it has an address,
    /// not from the moment it is signed in, so a challenge answered late finds
    /// another heartbeat in front of the answer. Read by position, that made six
    /// tests fall on a busy machine (build 305).
    pub async fn recv_auth_response(&self, buf: &mut [u8]) -> std::io::Result<(usize, std::net::SocketAddr)> {
        loop {
            let (n, from) = self.recv_from(buf).await?;
            if matches!(
                sdr_remote_core::protocol::Packet::deserialize(&buf[..n]),
                Ok(sdr_remote_core::protocol::Packet::AuthResponse { .. })
            ) {
                return Ok((n, from));
            }
        }
    }

    pub async fn send_to(&self, buf: &[u8], to: std::net::SocketAddr) -> std::io::Result<usize> {
        self.uplink
            .send((to, buf.to_vec()))
            .map(|_| buf.len())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the QUIC server stopped"))
    }
}

/// An AuthResult that proves this server knows `password`, for the AuthResponse
/// in `response` (the client's nonce rides at its tail; the challenge nonce of
/// this harness is all zeroes). Since v2.13 the client refuses a server that
/// proves nothing, so every scripted server that accepts has to prove itself.
#[allow(dead_code)]
pub fn build_proving_auth_result(code: u8, response: &[u8], password: &str) -> Vec<u8> {
    use sdr_remote_core::protocol::Packet;
    let proof = match Packet::deserialize(response) {
        Ok(Packet::AuthResponse { client_nonce: Some(nc), .. }) => {
            // Over QUIC the proof is bound to the connection the answer came on.
            let binding = binding_for_nonce(&nc);
            let key = sdr_remote_core::auth::session_key(password, &[0u8; 16], &nc, binding.as_ref());
            Some(sdr_remote_core::auth::server_proof(&key, &[0u8; 16], &nc, binding.as_ref()))
        }
        Ok(Packet::AuthResponse { client_nonce: None, .. }) => None,
        _ => panic!(
            "the harness expected the client's AuthResponse and got type byte {:?}, {} bytes",
            response.first(),
            response.len()
        ),
    };
    let mut buf = [0u8; sdr_remote_core::protocol::AUTH_RESULT_SIZE_WITH_PROOF];
    let n = sdr_remote_core::protocol::serialize_auth_result(&mut buf, code, proof.as_ref());
    buf[..n].to_vec()
}
