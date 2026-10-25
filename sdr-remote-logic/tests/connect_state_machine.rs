// SPDX-License-Identifier: GPL-2.0-or-later
//
// Integration tests for the PATCH-1 connect-state-machine.
//
// Strategy: spawn the real engine bound to a 127.0.0.1:0 socket, and run
// a hand-scripted fake-server on another 127.0.0.1:0 socket. Drive the
// state machine by sending `Command::Connect(...)` plus crafted packets;
// observe `RadioState.connect_status` via the `watch::Receiver`.
//
// Covers the 9 connect-states defined in `sdr-remote-logic::state::ConnectStatus`
// plus the v2.0.0-server compatibility edge-case (review finding B3).

mod common;

use anyhow::Result;
use sdr_remote_core::protocol::{
    Capabilities, Flags, HeartbeatAck, PacketType, ServerStateFlags, MAGIC, VERSION,
};
use sdr_remote_logic::audio::AudioBackend;
use sdr_remote_logic::commands::Command;
use sdr_remote_logic::engine::ClientEngine;
use sdr_remote_logic::state::{ConnectError, ConnectStatus, RadioState};
use std::time::{Duration, Instant};
use common::{build_proving_auth_result, FakeServer};
use tokio::sync::{mpsc, watch};

// ------------- Test helpers -------------

/// No-op audio backend so the engine can run without a sound card.
struct MockAudio;

impl AudioBackend for MockAudio {
    fn read_capture(&mut self, _buf: &mut [f32]) -> usize {
        0
    }
    fn write_playback(&mut self, _buf: &[f32]) -> usize {
        0
    }
    fn capture_level(&self) -> f32 {
        0.0
    }
    fn playback_level(&self) -> f32 {
        0.0
    }
    fn has_error(&self) -> bool {
        false
    }
    fn capture_sample_rate(&self) -> u32 {
        48_000
    }
    fn playback_sample_rate(&self) -> u32 {
        48_000
    }
}

fn mock_audio_factory(
    _input: Option<&str>,
    _output: Option<&str>,
) -> Result<Box<dyn AudioBackend>> {
    Ok(Box::new(MockAudio))
}

/// The no-op backend, counting the warning tones it was asked to sound
/// (build 253) - one counter per engine, so tests running side by side do not
/// see each other's tones.
struct ToneCountingAudio(std::sync::Arc<std::sync::atomic::AtomicU32>);

impl AudioBackend for ToneCountingAudio {
    fn read_capture(&mut self, _buf: &mut [f32]) -> usize {
        0
    }
    fn write_playback(&mut self, _buf: &[f32]) -> usize {
        0
    }
    fn capture_level(&self) -> f32 {
        0.0
    }
    fn playback_level(&self) -> f32 {
        0.0
    }
    fn has_error(&self) -> bool {
        false
    }
    fn capture_sample_rate(&self) -> u32 {
        48_000
    }
    fn playback_sample_rate(&self) -> u32 {
        48_000
    }
    fn sound_ptt_dropped(&mut self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Spawn an engine task and return handles. Caller is responsible for
/// `shutdown_tx.send(true)` and `join` cleanup. The engine talks QUIC to the
/// address a test connects to (since build 133 the only way it talks).
fn spawn_engine() -> (
    watch::Receiver<RadioState>,
    mpsc::UnboundedSender<Command>,
    watch::Sender<bool>,
    tokio::task::JoinHandle<Result<()>>,
) {
    let (engine, state_rx, cmd_tx) = ClientEngine::new();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(async move { engine.run(mock_audio_factory, shutdown_rx).await });
    (state_rx, cmd_tx, shutdown_tx, handle)
}

/// Wait until the state names `want` as the way the traffic takes (build 139):
/// the engine sets it on its 20 ms tick, so it follows the connected flag by a
/// tick or two.
async fn wait_for_path(state_rx: &mut watch::Receiver<RadioState>, want: Option<sdr_remote_core::path::PathKind>, what: &str) {
    let start = Instant::now();
    while state_rx.borrow().path != want {
        assert!(start.elapsed() < Duration::from_secs(2), "{what}: the path stayed {:?}, not {want:?}", state_rx.borrow().path);
        let _ = tokio::time::timeout(Duration::from_millis(50), state_rx.changed()).await;
    }
}

/// After a session ends, no name may come back: six engine ticks, then none.
/// "It was None once" is not enough - a name that a broken check puts back
/// 20 ms later passed that one time in four (review 140, measured 15 of 20).
async fn no_path_stays(state_rx: &watch::Receiver<RadioState>, what: &str) {
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(state_rx.borrow().path, None, "{what}: a name came back without a session");
}

/// Poll `state_rx.borrow().connect_status` until `pred(...)` returns true or
/// the deadline elapses. Returns the matching `ConnectStatus` (cloned) on
/// success, panics with the last seen status on timeout.
async fn wait_for_status(
    state_rx: &mut watch::Receiver<RadioState>,
    deadline_ms: u64,
    pred: impl Fn(&ConnectStatus) -> bool,
) -> ConnectStatus {
    let start = Instant::now();
    loop {
        {
            let s = state_rx.borrow();
            if pred(&s.connect_status) {
                return s.connect_status.clone();
            }
        }
        if start.elapsed() > Duration::from_millis(deadline_ms) {
            let s = state_rx.borrow();
            panic!(
                "wait_for_status timed out after {} ms; last status = {:?}",
                deadline_ms, s.connect_status
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Build an AuthChallenge packet; a nonce of zeroes is fine for state-machine tests.
fn build_auth_challenge() -> Vec<u8> {
    let mut buf = [0u8; sdr_remote_core::protocol::AUTH_CHALLENGE_SIZE];
    sdr_remote_core::protocol::serialize_auth_challenge(&mut buf, &[0u8; 16]);
    buf.to_vec()
}

/// Build an AuthResult packet in its old shape (no proof): this test is about
/// the state machine, and it walks the legacy path on purpose.
fn build_auth_result(result_code: u8) -> Vec<u8> {
    let mut buf = [0u8; sdr_remote_core::protocol::AUTH_RESULT_SIZE_WITH_PROOF];
    let n = sdr_remote_core::protocol::serialize_auth_result(&mut buf, result_code, None);
    buf[..n].to_vec()
}


/// The connected flag follows the Connected status by one ack (measured:
/// status at +550 ms, flag at +1090 ms with acks every half second). The
/// deadline is the one number both harnesses use.
async fn wait_for_connected_flag(state_rx: &mut watch::Receiver<RadioState>) {
    let deadline = Instant::now() + Duration::from_millis(common::CONNECTED_FLAG_WAIT_MS);
    while Instant::now() < deadline {
        if state_rx.borrow().connected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the connected flag never came up");
}

/// Ack every heartbeat until `n` of them have been acked; the admission burst
/// (one heartbeat and five controls in 15 ms) is not four heartbeats.
async fn ack_heartbeats(fake_server: &FakeServer, client_addr: std::net::SocketAddr, n: u32) {
    use sdr_remote_core::protocol::Packet;
    let mut buf = [0u8; 1500];
    let mut acked = 0u32;
    while acked < n {
        let Ok((len, _)) = fake_server.recv_from(&mut buf).await else { return };
        if matches!(Packet::deserialize(&buf[..len]), Ok(Packet::Heartbeat(_))) {
            let _ = fake_server
                .send_to(&build_heartbeat_ack_full(0, 0, 0, 0), client_addr)
                .await;
            acked += 1;
        }
    }
}

/// What a refused server hears next: first a Disconnect (within two seconds),
/// then nothing (for a second and a half). Returns (disconnect_seen, silence).
async fn expect_disconnect_then_silence(fake_server: &FakeServer) -> (bool, bool) {
    use sdr_remote_core::protocol::Packet;
    let mut buf = [0u8; 1500];
    let mut disconnect_seen = false;
    let deadline = Instant::now() + Duration::from_millis(2_000);
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, fake_server.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => {
                if matches!(Packet::deserialize(&buf[..n]), Ok(Packet::Disconnect)) {
                    disconnect_seen = true;
                    break;
                }
            }
            _ => break,
        }
    }
    let silence = tokio::time::timeout(Duration::from_millis(1_500), fake_server.recv_from(&mut buf))
        .await
        .is_err();
    (disconnect_seen, silence)
}

/// The TotpChallenge a real server sends right after TOTP_REQUIRED. The client
/// only logs it; the harness sends it so the chain is the real one.
fn build_totp_challenge() -> Vec<u8> {
    let mut buf = [0u8; 4];
    sdr_remote_core::protocol::Header::new(PacketType::TotpChallenge, Flags::NONE).serialize(&mut buf);
    buf.to_vec()
}

/// Build a 20-byte HeartbeatAck with given capabilities + state_flags.
fn build_heartbeat_ack_full(
    echo_seq: u32,
    echo_time: u32,
    capabilities: u32,
    state_flags: u32,
) -> Vec<u8> {
    let ack = HeartbeatAck {
        flags: Flags::NONE,
        echo_sequence: echo_seq,
        echo_time: echo_time,
        capabilities: Capabilities::NONE.with(capabilities),
        state_flags: ServerStateFlags::NONE.with(state_flags),
        subs: None,
        version: None,
    };
    let mut buf = [0u8; HeartbeatAck::SIZE];
    ack.serialize(&mut buf);
    buf.to_vec()
}

/// NOTE since build 2: this helper builds `HeartbeatAck::SIZE` - the 20-byte
/// form, which carries no version tag. Every test using it therefore plays a
/// server too old to say which build it is, and the client under test judges it
/// `PeerBuild::Older` and writes a warning line. That is deliberate (it keeps
/// the old-reader path exercised), but it was not written down anywhere, so a
/// later reader met a client that thinks its server is out of date and no
/// explanation (raised in review). The `Same` branch is walked end
/// to end by the three tests at the bottom of this file.

/// Build an old 16-byte HeartbeatAck (pre-PATCH-1; capabilities-only, no state_flags).
/// This simulates a v2.0.0-release-tag server that doesn't advertise REPORTS_STATE_FLAGS.
fn build_heartbeat_ack_v2_0_0(echo_seq: u32, echo_time: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 16];
    buf[0] = MAGIC;
    buf[1] = VERSION;
    buf[2] = PacketType::HeartbeatAck as u8;
    buf[3] = 0;
    buf[4..8].copy_from_slice(&echo_seq.to_be_bytes());
    buf[8..12].copy_from_slice(&echo_time.to_be_bytes());
    buf[12..16].copy_from_slice(&Capabilities::NONE.0.to_be_bytes());
    buf
}

// ------------- Tests -------------

#[tokio::test]
async fn dns_resolution_fail_surfaces_specific_error() {
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    // Bogus hostname — the resolver will fail.
    cmd_tx
        .send(Command::Connect(
            "definitely-not-a-real-host.invalid:4580".to_string(),
            Some("pw".to_string()),
        ))
        .unwrap();

    let status = wait_for_status(&mut state_rx, 8_000, |s| {
        matches!(s, ConnectStatus::Failed(ConnectError::DnsResolutionFailed { .. }))
    })
    .await;
    match status {
        ConnectStatus::Failed(ConnectError::DnsResolutionFailed { host, .. }) => {
            assert!(host.starts_with("definitely-not-a-real-host"));
        }
        _ => unreachable!(),
    }

    let _ = shutdown_tx.send(true);
}

#[tokio::test]
async fn no_udp_response_after_timeout() {
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    // An address that parses but where nobody listens (the discard port on
    // this machine): the QUIC handshake never completes, so no reply comes.
    cmd_tx
        .send(Command::Connect(
            "127.0.0.1:9".to_string(),
            Some("pw".to_string()),
        ))
        .unwrap();

    // NoUdpResponse triggers after 5s timeout in the engine; wait up to 8s.
    let status = wait_for_status(&mut state_rx, 8_000, |s| {
        matches!(s, ConnectStatus::Failed(ConnectError::NoUdpResponse { .. }))
    })
    .await;
    match status {
        ConnectStatus::Failed(ConnectError::NoUdpResponse { timeout_secs, .. }) => {
            assert!(timeout_secs >= 1);
        }
        _ => unreachable!(),
    }

    let _ = shutdown_tx.send(true);
}

#[tokio::test]
async fn malformed_response_during_connect() {

    // Fake-server replies with raw garbage bytes (no magic).
    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        // Wait for first client heartbeat, reply with malformed bytes.
        if let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await {
            let garbage = b"\xDE\xAD\xBE\xEF\x00\x00\x00\x00";
            let _ = fake_server.send_to(garbage, client_addr).await;
        }
    });

    cmd_tx
        .send(Command::Connect(
            server_addr.to_string(),
            Some("pw".to_string()),
        ))
        .unwrap();

    let status = wait_for_status(&mut state_rx, 4_000, |s| {
        matches!(s, ConnectStatus::Failed(ConnectError::MalformedResponse { .. }))
    })
    .await;
    assert!(matches!(
        status,
        ConnectStatus::Failed(ConnectError::MalformedResponse { .. })
    ));

    let _ = shutdown_tx.send(true);
}

#[tokio::test]
async fn protocol_version_mismatch_surfaces_specific_error() {

    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        if let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await {
            // Reply with valid magic but a future VERSION byte (e.g. 99).
            let mut response = vec![0u8; 8];
            response[0] = MAGIC;
            response[1] = 99; // bogus VERSION
            response[2] = PacketType::AuthChallenge as u8;
            response[3] = 0;
            let _ = fake_server.send_to(&response, client_addr).await;
        }
    });

    cmd_tx
        .send(Command::Connect(
            server_addr.to_string(),
            Some("pw".to_string()),
        ))
        .unwrap();

    let status = wait_for_status(&mut state_rx, 4_000, |s| {
        matches!(
            s,
            ConnectStatus::Failed(ConnectError::ProtocolVersionMismatch { .. })
        )
    })
    .await;
    match status {
        ConnectStatus::Failed(ConnectError::ProtocolVersionMismatch {
            server_version,
            client_version,
        }) => {
            assert_eq!(server_version, 99);
            assert_eq!(client_version, VERSION);
        }
        _ => unreachable!(),
    }

    let _ = shutdown_tx.send(true);
}

#[tokio::test]
async fn wrong_password_before_totp_phase() {

    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        // 1. recv first heartbeat → send AuthChallenge
        if let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await {
            let _ = fake_server
                .send_to(&build_auth_challenge(), client_addr)
                .await;
            // 2. recv AuthResponse → send AUTH_REJECTED
            if let Ok((_n, _client_addr)) = fake_server.recv_auth_response(&mut buf).await {
                let _ = fake_server
                    .send_to(
                        &build_auth_result(sdr_remote_core::protocol::AUTH_REJECTED),
                        client_addr,
                    )
                    .await;
            }
        }
    });

    cmd_tx
        .send(Command::Connect(
            server_addr.to_string(),
            Some("wrong-password".to_string()),
        ))
        .unwrap();

    let status = wait_for_status(&mut state_rx, 4_000, |s| {
        matches!(s, ConnectStatus::Failed(ConnectError::WrongPassword))
    })
    .await;
    assert_eq!(status, ConnectStatus::Failed(ConnectError::WrongPassword));

    let _ = shutdown_tx.send(true);
}

/// Owner's decision F: a server that accepts without proving it knows the
/// password - one from before the mutual check, or a bare ACCEPTED anyone can
/// send - is refused, and the client stays unconnected. Written after a reviewer
/// switched decision F off and every test stayed green (review 68).
#[tokio::test]
async fn a_server_that_proves_nothing_is_refused() {

    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();
    let server_left = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server_silent = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (left, silent, done) = (server_left.clone(), server_silent.clone(), server_done.clone());

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        if let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await {
            let _ = fake_server
                .send_to(&build_auth_challenge(), client_addr)
                .await;
            if let Ok((_n, _)) = fake_server.recv_auth_response(&mut buf).await {
                // The old five-byte shape: accepted, and no proof.
                let _ = fake_server
                    .send_to(
                        &build_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED),
                        client_addr,
                    )
                    .await;
                // A refused server is LEFT: it admitted this client, so it is
                // told, and then it hears nothing more. Until build 77 the
                // heartbeats went on and the server kept the session - and
                // its audio - alive for a client that showed "refused"
                // (review 76, measured).
                let (seen, quiet) = expect_disconnect_then_silence(&fake_server).await;
                left.store(seen, std::sync::atomic::Ordering::Relaxed);
                silent.store(quiet, std::sync::atomic::Ordering::Relaxed);
                done.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    });

    cmd_tx
        .send(Command::Connect(
            server_addr.to_string(),
            Some("pw".to_string()),
        ))
        .unwrap();

    let status = wait_for_status(&mut state_rx, 4_000, |s| {
        matches!(s, ConnectStatus::Failed(ConnectError::ServerProvesNothing))
    })
    .await;
    assert_eq!(status, ConnectStatus::Failed(ConnectError::ServerProvesNothing));
    assert!(!state_rx.borrow().connected, "refused means not connected");
    assert!(state_rx.borrow().auth_rejected, "and the screen is told so");
    while !server_done.load(std::sync::atomic::Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(server_left.load(std::sync::atomic::Ordering::Relaxed), "the refused server was not told");
    assert!(server_silent.load(std::sync::atomic::Ordering::Relaxed), "the refused server still hears the client");

    let _ = shutdown_tx.send(true);
}

/// The relay stalls; just before the link is declared lost a new challenge
/// gets through and is answered, the link is then declared lost (which forgets
/// the attempt), and only after that does the server's ACCEPTED arrive, with a
/// valid proof. That is an answer to an attempt no longer in flight - not a
/// server that proves nothing. Until build 122 it was read as the latter: the
/// client left the server and stayed on "the server does not prove it knows
/// the password" (field test 2026-10-06 20:15:58, desktop over the relay).
/// The client must ignore it and connect on the next challenge - through that
/// new challenge, not through the late answer: the verdict is pinned, not only
/// the outcome (review 122: with the attempt not forgotten, the late answer
/// verified and the client came back the wrong way, and this stayed green).
#[tokio::test]
async fn a_late_answer_after_the_link_was_lost_is_no_server_without_proof() {
    use sdr_remote_core::protocol::Packet;
    let fresh_challenge_sent = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fresh = fresh_challenge_sent.clone();

    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        // An ack that echoes the heartbeat's own time, so the client's round
        // trip is real and its link time-out is its six-second floor (with a
        // zero echo it reads the time since start as the round trip, and the
        // time-out grows to minutes).
        fn echo(packet: &[u8]) -> Option<Vec<u8>> {
            match Packet::deserialize(packet) {
                Ok(Packet::Heartbeat(hb)) => Some(build_heartbeat_ack_full(hb.sequence, hb.local_time, 0, 0)),
                _ => None,
            }
        }
        // Drain whatever the client sends until `until`.
        async fn drain_until(sock: &FakeServer, until: tokio::time::Instant) {
            let mut buf = [0u8; 1500];
            while tokio::time::timeout_at(until, sock.recv_from(&mut buf)).await.is_ok() {}
        }
        // Round one: connect and ack two heartbeats.
        let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await else { return };
        let _ = fake_server.send_to(&build_auth_challenge(), client_addr).await;
        let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await else { return };
        let accepted = build_proving_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED, &buf[..n], "pw");
        let _ = fake_server.send_to(&accepted, client_addr).await;
        let mut acked = 0;
        while acked < 2 {
            let Ok((n, _)) = fake_server.recv_from(&mut buf).await else { return };
            if let Some(ack) = echo(&buf[..n]) {
                let _ = fake_server.send_to(&ack, client_addr).await;
                acked += 1;
            }
        }
        let last_ack = tokio::time::Instant::now();
        // The stall: no acks. A new challenge gets through just before the
        // client's six seconds run out, and is answered.
        drain_until(&fake_server, last_ack + Duration::from_millis(5_400)).await;
        let _ = fake_server.send_to(&build_auth_challenge(), client_addr).await;
        let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await else { return };
        let late = build_proving_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED, &buf[..n], "pw");
        // The link is declared lost in between; then the answer arrives.
        drain_until(&fake_server, last_ack + Duration::from_millis(7_000)).await;
        let _ = fake_server.send_to(&late, client_addr).await;
        // A second of nothing, so that a client that took the late answer is
        // back before the new challenge goes out.
        drain_until(&fake_server, last_ack + Duration::from_millis(8_000)).await;
        // The server goes on as a real one does: a challenge on the next packet,
        // an accepting answer, acks from then on.
        let Ok((_n, _)) = fake_server.recv_from(&mut buf).await else { return };
        fresh.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = fake_server.send_to(&build_auth_challenge(), client_addr).await;
        let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await else { return };
        let accepted = build_proving_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED, &buf[..n], "pw");
        let _ = fake_server.send_to(&accepted, client_addr).await;
        loop {
            let Ok((n, _)) = fake_server.recv_from(&mut buf).await else { break };
            if let Some(ack) = echo(&buf[..n]) {
                let _ = fake_server.send_to(&ack, client_addr).await;
            }
        }
    });

    cmd_tx.send(Command::Connect(server_addr.to_string(), Some("pw".to_string()))).unwrap();
    wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::Connected)).await;
    wait_for_connected_flag(&mut state_rx).await;
    // Every admitted session counts once. The only new one allowed is the one
    // the fresh challenge opens: one that starts before it came from the late
    // answer. (The connected flag cannot tell: it waits for an acked
    // heartbeat, and the server is silent just then - review 122, mutant B.)
    let first_session = state_rx.borrow().session_generation;
    // The link goes down (the flag drops), and comes back: watched without a
    // gap, so a refusal in between cannot slip past.
    let deadline = Instant::now() + Duration::from_millis(14_000);
    let mut lost = false;
    loop {
        let st = state_rx.borrow().clone();
        assert_ne!(
            st.connect_status,
            ConnectStatus::Failed(ConnectError::ServerProvesNothing),
            "a late answer to an attempt that is no longer in flight was read as a server without proof"
        );
        if !fresh_challenge_sent.load(std::sync::atomic::Ordering::SeqCst) {
            assert_eq!(
                st.session_generation, first_session,
                "a new session before the new challenge - the late answer was taken"
            );
        }
        if !st.connected {
            lost = true;
        } else if lost && st.connect_status == ConnectStatus::Connected {
            assert!(
                fresh_challenge_sent.load(std::sync::atomic::Ordering::SeqCst),
                "back before the new challenge - the late answer was taken"
            );
            break;
        }
        assert!(Instant::now() < deadline, "never back after the late answer (lost: {lost}, {:?})", st.connect_status);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let _ = shutdown_tx.send(true);
}

/// A server that proves itself WRONGLY - a proof under another password, or a
/// forged one - is refused whatever its code says (PROOF_INVALID: the third of
/// the client's own verdicts, and the last to get a test - eight rounds, review 75).
#[tokio::test]
async fn a_server_with_a_wrong_proof_is_refused() {

    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();
    let server_left = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server_silent = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (left, silent, done) = (server_left.clone(), server_silent.clone(), server_done.clone());

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        if let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await {
            let _ = fake_server
                .send_to(&build_auth_challenge(), client_addr)
                .await;
            if let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await {
                // Accepted, with a proof derived from the wrong password.
                let _ = fake_server
                    .send_to(
                        &build_proving_auth_result(
                            sdr_remote_core::protocol::AUTH_ACCEPTED,
                            &buf[..n],
                            "another-pw",
                        ),
                        client_addr,
                    )
                    .await;
                let (seen, quiet) = expect_disconnect_then_silence(&fake_server).await;
                left.store(seen, std::sync::atomic::Ordering::Relaxed);
                silent.store(quiet, std::sync::atomic::Ordering::Relaxed);
                done.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    });

    cmd_tx
        .send(Command::Connect(
            server_addr.to_string(),
            Some("correct-pw".to_string()),
        ))
        .unwrap();

    let status = wait_for_status(&mut state_rx, 4_000, |s| {
        matches!(s, ConnectStatus::Failed(_))
    })
    .await;
    assert_eq!(status, ConnectStatus::Failed(ConnectError::ServerUnverified));
    assert!(!state_rx.borrow().connected, "refused means not connected");
    assert!(state_rx.borrow().auth_rejected, "and the screen is told so");
    while !server_done.load(std::sync::atomic::Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(server_left.load(std::sync::atomic::Ordering::Relaxed), "the refused server was not told");
    assert!(server_silent.load(std::sync::atomic::Ordering::Relaxed), "the refused server still hears the client");

    let _ = shutdown_tx.send(true);
}

/// A duplicate of an earlier refusal, landing after the session was admitted:
/// ignored. Until build 78 it took the live session down - "wrong password",
/// the address dropped, the server still holding the client (review 77).
#[tokio::test]
async fn a_stray_rejection_after_connecting_is_ignored() {
    a_stray_result_after_connecting_is_ignored(sdr_remote_core::protocol::AUTH_REJECTED).await;
}

/// The same with a lone TOTP_REQUIRED and no proof: before build 122 it read as a
/// server that proves nothing, and one such datagram took a verified client off
/// its session (on plain UDP and the classic relay; inside QUIC it cannot be
/// forged). Since 122 it is ignored, as nothing is in flight (review 122).
#[tokio::test]
async fn a_stray_code_request_after_connecting_is_ignored() {
    a_stray_result_after_connecting_is_ignored(sdr_remote_core::protocol::AUTH_TOTP_REQUIRED).await;
}

/// Connect, and after the second heartbeat deliver a result `code` without a
/// proof, belonging to no attempt: the session must stand.
async fn a_stray_result_after_connecting_is_ignored(code: u8) {

    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();
    let server_told = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let told = server_told.clone();

    tokio::spawn(async move {
        use sdr_remote_core::protocol::Packet;
        let mut buf = [0u8; 1500];
        let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await else { return };
        let _ = fake_server.send_to(&build_auth_challenge(), client_addr).await;
        let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await else { return };
        let _ = fake_server
            .send_to(
                &build_proving_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED, &buf[..n], "correct-pw"),
                client_addr,
            )
            .await;
        let mut heartbeats = 0u32;
        loop {
            let Ok((n, _)) = fake_server.recv_from(&mut buf).await else { break };
            match Packet::deserialize(&buf[..n]) {
                Ok(Packet::Disconnect) => {
                    told.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                Ok(Packet::Heartbeat(_)) => {
                    let _ = fake_server
                        .send_to(&build_heartbeat_ack_full(0, 0, 0, 0), client_addr)
                        .await;
                    heartbeats += 1;
                    if heartbeats == 2 {
                        // The result of some earlier attempt, delivered twice.
                        let _ = fake_server.send_to(&build_auth_result(code), client_addr).await;
                    }
                }
                _ => {}
            }
        }
    });

    cmd_tx
        .send(Command::Connect(server_addr.to_string(), Some("correct-pw".to_string())))
        .unwrap();
    wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::Connected)).await;
    wait_for_connected_flag(&mut state_rx).await;
    assert_eq!(state_rx.borrow().route, sdr_remote_logic::measure::Route::Quic, "over QUIC the screen says QUIC");

    let watch_until = Instant::now() + Duration::from_millis(2_500);
    while Instant::now() < watch_until {
        let s = state_rx.borrow().clone();
        assert_eq!(s.connect_status, ConnectStatus::Connected, "a stray result 0x{code:02x} took the session down");
        assert!(s.connected, "a stray result 0x{code:02x} cleared the connected flag");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!server_told.load(std::sync::atomic::Ordering::Relaxed), "the client left over a stray result 0x{code:02x}");

    let _ = shutdown_tx.send(true);
}

/// A server refused on a RE-authentication - connected once, then the server
/// came back as a build that proves nothing - leaves the client disconnected
/// in every sense: the flag the controls read, the session's state, and the
/// address. In build 77 the address went and the rest stayed (review 77).
#[tokio::test]
async fn a_server_refused_on_a_second_session_leaves_nothing_connected() {

    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();
    let server_left = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server_silent = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (left, silent, done) = (server_left.clone(), server_silent.clone(), server_done.clone());

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        // Session one: proven, and four acks (the connected flag follows the status by one ack).
        let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await else { return };
        let _ = fake_server.send_to(&build_auth_challenge(), client_addr).await;
        let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await else { return };
        let _ = fake_server
            .send_to(
                &build_proving_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED, &buf[..n], "correct-pw"),
                client_addr,
            )
            .await;
        ack_heartbeats(&fake_server, client_addr, 4).await;
        // The server "restarted as an older build": a fresh challenge, and an
        // ACCEPTED without a proof.
        let _ = fake_server.send_to(&build_auth_challenge(), client_addr).await;
        let Ok(_) = fake_server.recv_auth_response(&mut buf).await else { return };
        let _ = fake_server
            .send_to(&build_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED), client_addr)
            .await;
        let (seen, quiet) = expect_disconnect_then_silence(&fake_server).await;
        left.store(seen, std::sync::atomic::Ordering::Relaxed);
        silent.store(quiet, std::sync::atomic::Ordering::Relaxed);
        done.store(true, std::sync::atomic::Ordering::Relaxed);
    });

    cmd_tx
        .send(Command::Connect(server_addr.to_string(), Some("correct-pw".to_string())))
        .unwrap();
    wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::Connected)).await;
    wait_for_connected_flag(&mut state_rx).await;

    let status = wait_for_status(&mut state_rx, 6_000, |s| matches!(s, ConnectStatus::Failed(_))).await;
    assert_eq!(status, ConnectStatus::Failed(ConnectError::ServerProvesNothing));
    assert!(!state_rx.borrow().connected, "refused on a second session, and still 'connected'");
    while !server_done.load(std::sync::atomic::Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(server_left.load(std::sync::atomic::Ordering::Relaxed), "the refused server was not told");
    assert!(server_silent.load(std::sync::atomic::Ordering::Relaxed), "the refused server still hears the client");

    let _ = shutdown_tx.send(true);
}

/// What the server said about itself goes down with a refused re-authentication
/// too: the link-down block calls `session_lost!()`, and until build 79 that
/// call was held by a count in fitness and by no behaviour - taking it out of
/// the block left every test green (review 78). Three fields of that one call
/// are read: the server's build, and "single receiver" and "no cluster", all
/// three learned from the first acks and all three gone after the refusal.
/// The chain is one a real server can make: a re-authentication whose
/// password step proves nothing (a server restarted as a build before 66).
/// What this cannot pin is the CALL as such: a replacement that writes these
/// three fields by hand stays green (review 80, measured up to four fields).
/// The call sites are counted by fitness; the field list is held by the unit
/// test of `forget_session`; this holds that the block reaches that list.
#[tokio::test]
async fn a_refused_re_authentication_forgets_what_the_server_said() {
    use sdr_remote_core::protocol::{PeerBuild, Packet};

    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await else { return };
        let _ = fake_server.send_to(&build_auth_challenge(), client_addr).await;
        let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await else { return };
        let _ = fake_server
            .send_to(
                &build_proving_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED, &buf[..n], "correct-pw"),
                client_addr,
            )
            .await;
        // Acks that say which build this server is, and that it has one
        // receiver and no cluster - three things the server says about itself.
        let saying = build_heartbeat_ack_saying_with_flags(
            "2.11.0 (build 93)",
            ServerStateFlags::NONE
                .with(ServerStateFlags::SINGLE_RECEIVER)
                .with(ServerStateFlags::NO_DX_CLUSTER),
        );
        let mut acked = 0u32;
        while acked < 4 {
            let Ok((len, _)) = fake_server.recv_from(&mut buf).await else { return };
            if matches!(Packet::deserialize(&buf[..len]), Ok(Packet::Heartbeat(_))) {
                let _ = fake_server.send_to(&saying, client_addr).await;
                acked += 1;
            }
        }
        // Restarted as a build that proves nothing: refused at the password
        // step (RESULT_NO_PROOF), the chain a real server before build 66 makes.
        let _ = fake_server.send_to(&build_auth_challenge(), client_addr).await;
        let Ok(_) = fake_server.recv_auth_response(&mut buf).await else { return };
        let _ = fake_server
            .send_to(&build_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED), client_addr)
            .await;
        let _ = expect_disconnect_then_silence(&fake_server).await;
    });

    cmd_tx
        .send(Command::Connect(server_addr.to_string(), Some("correct-pw".to_string())))
        .unwrap();
    wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::Connected)).await;
    wait_for_connected_flag(&mut state_rx).await;
    {
        let s = state_rx.borrow();
        assert!(
            matches!(s.server_build, PeerBuild::Different(_)),
            "session one learned the server's build: {:?}",
            s.server_build
        );
        assert!(!s.rx2_present && !s.dx_cluster_available, "session one learned what the server lacks");
    }

    let status = wait_for_status(&mut state_rx, 6_000, |s| matches!(s, ConnectStatus::Failed(_))).await;
    assert_eq!(status, ConnectStatus::Failed(ConnectError::ServerProvesNothing));
    // The block runs on the next tick; give it a few.
    let forgotten = |s: &RadioState| {
        s.server_build == PeerBuild::Unknown && s.rx2_present && s.dx_cluster_available
    };
    let deadline = Instant::now() + Duration::from_millis(1_000);
    while Instant::now() < deadline && !forgotten(&state_rx.borrow()) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let s = state_rx.borrow().clone();
    assert_eq!(s.server_build, PeerBuild::Unknown, "what the refused server said about itself is still on the screen");
    assert!(s.rx2_present, "the refused server's 'single receiver' is still on the screen");
    assert!(s.dx_cluster_available, "the refused server's 'no cluster' is still on the screen");

    let _ = shutdown_tx.send(true);
}

/// The same on a refused PASSWORD on a re-authentication - the server came
/// back with another password: the address goes silently, and nothing stays
/// connected (review 77).
#[tokio::test]
async fn a_refused_password_on_a_second_session_leaves_nothing_connected() {

    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();
    let heard_after = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (heard, done) = (heard_after.clone(), server_done.clone());

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await else { return };
        let _ = fake_server.send_to(&build_auth_challenge(), client_addr).await;
        let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await else { return };
        let _ = fake_server
            .send_to(
                &build_proving_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED, &buf[..n], "correct-pw"),
                client_addr,
            )
            .await;
        ack_heartbeats(&fake_server, client_addr, 4).await;
        // The server came back with another password: challenge, and refuse.
        let _ = fake_server.send_to(&build_auth_challenge(), client_addr).await;
        let Ok(_) = fake_server.recv_auth_response(&mut buf).await else { return };
        let _ = fake_server
            .send_to(&build_auth_result(sdr_remote_core::protocol::AUTH_REJECTED), client_addr)
            .await;
        // Heartbeats already in flight may land; after a short while, nothing.
        tokio::time::sleep(Duration::from_millis(600)).await;
        if tokio::time::timeout(Duration::from_millis(1_200), fake_server.recv_from(&mut buf))
            .await
            .is_ok()
        {
            heard.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        done.store(true, std::sync::atomic::Ordering::Relaxed);
    });

    cmd_tx
        .send(Command::Connect(server_addr.to_string(), Some("correct-pw".to_string())))
        .unwrap();
    wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::Connected)).await;
    wait_for_connected_flag(&mut state_rx).await;

    let status = wait_for_status(&mut state_rx, 6_000, |s| matches!(s, ConnectStatus::Failed(_))).await;
    assert_eq!(status, ConnectStatus::Failed(ConnectError::WrongPassword));
    assert!(!state_rx.borrow().connected, "refused on a second session, and still 'connected'");
    while !server_done.load(std::sync::atomic::Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!heard_after.load(std::sync::atomic::Ordering::Relaxed), "the client kept talking after the refusal");

    let _ = shutdown_tx.send(true);
}

/// A mistyped password, corrected: the client falls silent on the refusal
/// (nothing keeps that address, and a server of v2.13+ would challenge every
/// heartbeat - five answers with the same wrong password block the address
/// for a minute), and the corrected Connect starts from nothing. Measured in
/// build 76 as "server unreachable, check your firewall" after a corrected
/// password, against a server that was there (review 76).
#[tokio::test]
async fn a_corrected_password_connects_without_a_wait() {

    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();
    let heard_after_refusal = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let heard = heard_after_refusal.clone();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        // Attempt one: refused.
        let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await else { return };
        let _ = fake_server.send_to(&build_auth_challenge(), client_addr).await;
        let Ok((_n, _)) = fake_server.recv_auth_response(&mut buf).await else { return };
        let _ = fake_server
            .send_to(&build_auth_result(sdr_remote_core::protocol::AUTH_REJECTED), client_addr)
            .await;
        // Silence for a second: the client has let go of this address.
        if tokio::time::timeout(Duration::from_millis(1_000), fake_server.recv_from(&mut buf))
            .await
            .is_ok()
        {
            heard.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        // Attempt two, from the corrected Connect: a fresh handshake, proven.
        let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await else { return };
        let _ = fake_server.send_to(&build_auth_challenge(), client_addr).await;
        let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await else { return };
        let _ = fake_server
            .send_to(
                &build_proving_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED, &buf[..n], "correct-pw"),
                client_addr,
            )
            .await;
        loop {
            if let Ok((_n, _)) = fake_server.recv_from(&mut buf).await {
                let _ = fake_server
                    .send_to(&build_heartbeat_ack_full(0, 0, 0, 0), client_addr)
                    .await;
            } else {
                break;
            }
        }
    });

    cmd_tx
        .send(Command::Connect(server_addr.to_string(), Some("typo-pw".to_string())))
        .unwrap();
    let status = wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::Failed(_))).await;
    assert_eq!(status, ConnectStatus::Failed(ConnectError::WrongPassword));

    // The operator reads the message, corrects the password, connects again.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert!(
        !heard_after_refusal.load(std::sync::atomic::Ordering::Relaxed),
        "the client kept talking to a server that refused its password"
    );
    cmd_tx
        .send(Command::Connect(server_addr.to_string(), Some("correct-pw".to_string())))
        .unwrap();
    let status = wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::Connected)).await;
    assert_eq!(status, ConnectStatus::Connected, "a corrected password connects at once");

    let _ = shutdown_tx.send(true);
}

/// A refused PASSWORD ends the attempt: an ACCEPTED that proves itself on the
/// nonces of that attempt, arriving afterwards, connects nothing. The other
/// half of the rule that keeps the nonces after a refused code (build 76) -
/// without this, dropping the clearing altogether stayed green.
#[tokio::test]
async fn a_refused_password_ends_the_attempt() {

    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await else { return };
        let _ = fake_server.send_to(&build_auth_challenge(), client_addr).await;
        let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await else { return };
        let _ = fake_server
            .send_to(&build_auth_result(sdr_remote_core::protocol::AUTH_REJECTED), client_addr)
            .await;
        // And then, on the same nonces, a proving ACCEPTED - a server that
        // changed its mind, or a replay of another attempt's answer.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = fake_server
            .send_to(
                &build_proving_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED, &buf[..n], "correct-pw"),
                client_addr,
            )
            .await;
        loop {
            if let Ok((_n, _)) = fake_server.recv_from(&mut buf).await {
                let _ = fake_server
                    .send_to(&build_heartbeat_ack_full(0, 0, 0, 0), client_addr)
                    .await;
            } else {
                break;
            }
        }
    });

    cmd_tx
        .send(Command::Connect(
            server_addr.to_string(),
            Some("correct-pw".to_string()),
        ))
        .unwrap();

    let status = wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::Failed(_))).await;
    assert_eq!(status, ConnectStatus::Failed(ConnectError::WrongPassword));
    let watch_until = Instant::now() + Duration::from_millis(1_500);
    while Instant::now() < watch_until {
        assert_ne!(
            state_rx.borrow().connect_status,
            ConnectStatus::Connected,
            "an ACCEPTED on the nonces of a refused attempt connected"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let _ = shutdown_tx.send(true);
}

/// A wrong code, then the right one, on the same attempt: the server keeps the
/// code step open and admits the second code with a bare ACCEPTED, which only
/// the kept nonces can verify. Until build 76 the refusal cleared them, and the
/// right code ended in "the server proves nothing" while the server held the
/// client as admitted (review 75, measured).
#[tokio::test]
async fn a_wrong_code_then_the_right_one_connects() {

    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await else { return };
        let _ = fake_server.send_to(&build_auth_challenge(), client_addr).await;
        let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await else { return };
        let _ = fake_server
            .send_to(
                &build_proving_auth_result(sdr_remote_core::protocol::AUTH_TOTP_REQUIRED, &buf[..n], "correct-pw"),
                client_addr,
            )
            .await;
        let _ = fake_server.send_to(&build_totp_challenge(), client_addr).await;
        // The first code: refused. The session stays at the code step.
        let Ok((_n, _)) = fake_server.recv_from(&mut buf).await else { return };
        let _ = fake_server
            .send_to(&build_auth_result(sdr_remote_core::protocol::AUTH_REJECTED), client_addr)
            .await;
        // The second code: admitted, bare, as a real server answers a code.
        let Ok((_n, _)) = fake_server.recv_from(&mut buf).await else { return };
        let _ = fake_server
            .send_to(&build_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED), client_addr)
            .await;
        loop {
            if let Ok((_n, _)) = fake_server.recv_from(&mut buf).await {
                let _ = fake_server
                    .send_to(&build_heartbeat_ack_full(0, 0, 0, 0), client_addr)
                    .await;
            } else {
                break;
            }
        }
    });

    cmd_tx
        .send(Command::Connect(
            server_addr.to_string(),
            Some("correct-pw".to_string()),
        ))
        .unwrap();

    wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::AwaitingTotp)).await;
    cmd_tx.send(Command::SendTotpCode("000000".to_string())).unwrap();
    let status = wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::Failed(_))).await;
    assert_eq!(status, ConnectStatus::Failed(ConnectError::WrongTotp));

    // The operator reads the code again and types the right one.
    cmd_tx.send(Command::SendTotpCode("123456".to_string())).unwrap();
    let status = wait_for_status(&mut state_rx, 4_000, |s| {
        matches!(s, ConnectStatus::Connected | ConnectStatus::Failed(ConnectError::ServerProvesNothing))
    })
    .await;
    assert_eq!(status, ConnectStatus::Connected, "the right code after a wrong one is the admission it is");
    assert!(!state_rx.borrow().auth_rejected, "the refusal of the first code is history");

    let _ = shutdown_tx.send(true);
}

/// The 2FA chain to the end: the proof comes with TOTP_REQUIRED, the result after
/// the code carries none, and the client connects on what it verified at the
/// password step. Nothing walked this chain before: removing that carry-over
/// left every test green (review 69) - and no 2FA client could have connected,
/// with a message pointing at the server.
#[tokio::test]
async fn a_2fa_client_connects_on_the_proof_from_the_password_step() {

    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();
    // Whether anything reached the server while the operator "typed": the
    // client pauses its heartbeats at the code step, and the server's two
    // minutes rest on that (review 71: the pause was held by nothing).
    let heard_at_code_step = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let heard = heard_at_code_step.clone();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        if let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await {
            let _ = fake_server
                .send_to(&build_auth_challenge(), client_addr)
                .await;
            // AuthResponse -> TOTP_REQUIRED, with the proof.
            if let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await {
                let _ = fake_server
                    .send_to(
                        &build_proving_auth_result(
                            sdr_remote_core::protocol::AUTH_TOTP_REQUIRED,
                            &buf[..n],
                            "correct-pw",
                        ),
                        client_addr,
                    )
                    .await;
                let _ = fake_server.send_to(&build_totp_challenge(), client_addr).await;
                // Nothing may arrive before the code (the test types it after 800 ms).
                if tokio::time::timeout(Duration::from_millis(700), fake_server.recv_from(&mut buf))
                    .await
                    .is_ok()
                {
                    heard.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                // TotpResponse -> ACCEPTED, without a proof: that is what a real
                // server sends after the code.
                if let Ok((_n, _)) = fake_server.recv_from(&mut buf).await {
                    let _ = fake_server
                        .send_to(
                            &build_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED),
                            client_addr,
                        )
                        .await;
                    let mut acks: u32 = 0;
                    loop {
                        if let Ok((_n, _)) = fake_server.recv_from(&mut buf).await {
                            // Capabilities 0: no REPORTS_STATE_FLAGS, so the client
                            // does not read state_flags and lands on Connected, not
                            // on TciUnreachable (the same shape as the short-ack test).
                            let _ = fake_server
                                .send_to(&build_heartbeat_ack_full(0, 0, 0, 0), client_addr)
                                .await;
                            // Heartbeats only: the admission is followed by a burst of
                            // six packets in 15 ms (one heartbeat, five controls), and a
                            // counter over all of them fires inside that burst, before
                            // this test has seen Connected - a race, both sides measured
                            // (review 75). The second heartbeat comes half a second in.
                            if matches!(
                                sdr_remote_core::protocol::Packet::deserialize(&buf[.._n]),
                                Ok(sdr_remote_core::protocol::Packet::Heartbeat(_))
                            ) {
                                acks += 1;
                            }
                            if acks == 2 {
                                acks += 1; // once
                                // The ACCEPTED once more, as a UDP duplicate would: a
                                // repeated result for the verified session is ignored,
                                // not a server that proves nothing (RESULT_REPEATED -
                                // reachable by no test until build 75, review 74).
                                let _ = fake_server
                                    .send_to(
                                        &build_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED),
                                        client_addr,
                                    )
                                    .await;
                            }
                        } else {
                            break;
                        }
                    }
                }
            }
        }
    });

    cmd_tx
        .send(Command::Connect(
            server_addr.to_string(),
            Some("correct-pw".to_string()),
        ))
        .unwrap();

    wait_for_status(&mut state_rx, 4_000, |s| {
        matches!(s, ConnectStatus::AwaitingTotp)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    cmd_tx
        .send(Command::SendTotpCode("123456".to_string()))
        .unwrap();

    let status = wait_for_status(&mut state_rx, 4_000, |s| {
        matches!(s, ConnectStatus::Connected)
    })
    .await;
    assert_eq!(status, ConnectStatus::Connected);
    assert!(
        !heard_at_code_step.load(std::sync::atomic::Ordering::Relaxed),
        "the client sent something while the operator was typing - the heartbeat pause is gone"
    );
    // And it stays connected: the clock on the code is wiped by the answer, and
    // the wait on it looks at the code step only. Either let go, and a healthy
    // session would be dropped five seconds after it was made (review 71).
    // Watched every 50 ms, not looked at once at the end: a blink to
    // Connecting is undone by the next ack within half a second.
    let watch_until = Instant::now() + Duration::from_millis(6_500);
    while Instant::now() < watch_until {
        let status = state_rx.borrow().connect_status.clone();
        assert_eq!(status, ConnectStatus::Connected, "a verified 2FA session was let go of after the fact");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let _ = shutdown_tx.send(true);
}

/// The code went out and the server says nothing - it reaped the attempt while
/// the operator typed, and it is still there. The client used to wait at the
/// code field for ever; now it lets go after five seconds, its heartbeats
/// resume, the server challenges it again, and a fresh code connects it
/// (review 70, 71).
#[tokio::test]
async fn a_2fa_code_that_gets_no_answer_starts_a_new_attempt() {

    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();
    let challenges = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let challenges_sent = challenges.clone();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        let mut client: Option<std::net::SocketAddr> = None;
        // Round one: challenge, TOTP_REQUIRED with the proof, and then silence
        // on the code - the attempt is gone on this server.
        if let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await {
            client = Some(client_addr);
            let _ = fake_server
                .send_to(&build_auth_challenge(), client_addr)
                .await;
            challenges_sent.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await {
                let _ = fake_server
                    .send_to(
                        &build_proving_auth_result(
                            sdr_remote_core::protocol::AUTH_TOTP_REQUIRED,
                            &buf[..n],
                            "correct-pw",
                        ),
                        client_addr,
                    )
                    .await;
                let _ = fake_server.send_to(&build_totp_challenge(), client_addr).await;
                let _ = fake_server.recv_from(&mut buf).await; // the code: no answer
            }
        }
        // Round two: the client's heartbeats resume, and a real server that
        // forgot the attempt challenges the knock. Then the chain to Connected.
        let Some(client_addr) = client else { return };
        if fake_server.recv_from(&mut buf).await.is_ok() {
            // A moment before the challenge, so the test can see Connecting
            // (on localhost a challenge comes back inside one tick).
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = fake_server
                .send_to(&build_auth_challenge(), client_addr)
                .await;
            challenges_sent.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await {
                let _ = fake_server
                    .send_to(
                        &build_proving_auth_result(
                            sdr_remote_core::protocol::AUTH_TOTP_REQUIRED,
                            &buf[..n],
                            "correct-pw",
                        ),
                        client_addr,
                    )
                    .await;
                let _ = fake_server.send_to(&build_totp_challenge(), client_addr).await;
                if let Ok((_n, _)) = fake_server.recv_from(&mut buf).await {
                    let _ = fake_server
                        .send_to(
                            &build_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED),
                            client_addr,
                        )
                        .await;
                    loop {
                        if let Ok((_n, _)) = fake_server.recv_from(&mut buf).await {
                            let _ = fake_server
                                .send_to(&build_heartbeat_ack_full(0, 0, 0, 0), client_addr)
                                .await;
                        } else {
                            break;
                        }
                    }
                }
            }
        }
    });

    cmd_tx
        .send(Command::Connect(
            server_addr.to_string(),
            Some("correct-pw".to_string()),
        ))
        .unwrap();

    wait_for_status(&mut state_rx, 4_000, |s| {
        matches!(s, ConnectStatus::AwaitingTotp)
    })
    .await;
    cmd_tx
        .send(Command::SendTotpCode("123456".to_string()))
        .unwrap();

    // The wait lets go: Connecting - not Failed, not rejected - then the
    // server challenges the resumed heartbeat and a fresh code field follows.
    // Watched with wait_for_status, not a blind sleep: a Failed in between was
    // overwritten within a second and a blind look never saw it (review 72).
    let status = wait_for_status(&mut state_rx, 7_000, |s| {
        matches!(s, ConnectStatus::Connecting)
    })
    .await;
    assert_eq!(status, ConnectStatus::Connecting, "letting go of the attempt is a new attempt, not a failure");
    assert!(!state_rx.borrow().auth_rejected, "letting go of an attempt is no rejection");
    let status = wait_for_status(&mut state_rx, 4_000, |s| {
        matches!(s, ConnectStatus::AwaitingTotp)
    })
    .await;
    assert_eq!(status, ConnectStatus::AwaitingTotp, "a fresh code field");
    assert_eq!(
        challenges.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "the client did not knock again after the unanswered code"
    );
    cmd_tx
        .send(Command::SendTotpCode("654321".to_string()))
        .unwrap();
    let status = wait_for_status(&mut state_rx, 4_000, |s| {
        matches!(s, ConnectStatus::Connected)
    })
    .await;
    assert_eq!(status, ConnectStatus::Connected);

    let _ = shutdown_tx.send(true);
}

/// The same unanswered code against a server that never challenges the
/// resumed heartbeat either - one from before build 72, which holds the old
/// attempt for its deadline. The client used to sit at Connecting for ever;
/// now the no-reply watchdog ends the attempt out loud (review 72).
#[tokio::test]
async fn a_server_that_never_challenges_again_ends_in_no_reply() {

    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        if let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await {
            let _ = fake_server
                .send_to(&build_auth_challenge(), client_addr)
                .await;
            if let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await {
                let _ = fake_server
                    .send_to(
                        &build_proving_auth_result(
                            sdr_remote_core::protocol::AUTH_TOTP_REQUIRED,
                            &buf[..n],
                            "correct-pw",
                        ),
                        client_addr,
                    )
                    .await;
                let _ = fake_server.send_to(&build_totp_challenge(), client_addr).await;
            }
        }
        // From here on: everything is read and nothing is answered - the code,
        // and every heartbeat after it.
        loop {
            if fake_server.recv_from(&mut buf).await.is_err() {
                break;
            }
        }
    });

    cmd_tx
        .send(Command::Connect(
            server_addr.to_string(),
            Some("correct-pw".to_string()),
        ))
        .unwrap();

    wait_for_status(&mut state_rx, 4_000, |s| {
        matches!(s, ConnectStatus::AwaitingTotp)
    })
    .await;
    cmd_tx
        .send(Command::SendTotpCode("123456".to_string()))
        .unwrap();

    // Five seconds for the code, five for the resumed attempt (the default
    // connect time-out), and a margin.
    let status = wait_for_status(&mut state_rx, 14_000, |s| {
        matches!(s, ConnectStatus::Failed(ConnectError::NoUdpResponse { .. }))
    })
    .await;
    assert!(matches!(status, ConnectStatus::Failed(ConnectError::NoUdpResponse { .. })));

    let _ = shutdown_tx.send(true);
}

#[tokio::test]
async fn wrong_totp_after_totp_phase() {

    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        // Heartbeat → AuthChallenge
        if let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await {
            let _ = fake_server
                .send_to(&build_auth_challenge(), client_addr)
                .await;
            // AuthResponse → AUTH_TOTP_REQUIRED
            if let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await {
                let _ = fake_server
                    .send_to(
                        &build_proving_auth_result(sdr_remote_core::protocol::AUTH_TOTP_REQUIRED, &buf[..n], "correct-pw"),
                        client_addr,
                    )
                    .await;
                // TotpResponse → AUTH_REJECTED
                if let Ok((_n, _)) = fake_server.recv_from(&mut buf).await {
                    let _ = fake_server
                        .send_to(
                            &build_auth_result(sdr_remote_core::protocol::AUTH_REJECTED),
                            client_addr,
                        )
                        .await;
                    // A second code: read, and left unanswered.
                    let _ = fake_server.recv_from(&mut buf).await;
                }
            }
        }
    });

    cmd_tx
        .send(Command::Connect(
            server_addr.to_string(),
            Some("correct-pw".to_string()),
        ))
        .unwrap();

    // First await AwaitingTotp transition
    wait_for_status(&mut state_rx, 4_000, |s| {
        matches!(s, ConnectStatus::AwaitingTotp)
    })
    .await;

    // Send a TOTP code (server will reject)
    cmd_tx
        .send(Command::SendTotpCode("000000".to_string()))
        .unwrap();

    let status = wait_for_status(&mut state_rx, 4_000, |s| {
        matches!(s, ConnectStatus::Failed(ConnectError::WrongTotp))
    })
    .await;
    assert_eq!(status, ConnectStatus::Failed(ConnectError::WrongTotp));
    assert!(state_rx.borrow().auth_rejected);

    // The operator types again and that code gets no answer: letting go of
    // the attempt also lets go of the rejection, or Connecting would stand
    // with "wrong code" still in view (review 73).
    cmd_tx
        .send(Command::SendTotpCode("111111".to_string()))
        .unwrap();
    wait_for_status(&mut state_rx, 7_000, |s| {
        matches!(s, ConnectStatus::Connecting)
    })
    .await;
    assert!(!state_rx.borrow().auth_rejected, "the rejection outlived the attempt it belonged to");

    let _ = shutdown_tx.send(true);
}

/// A late REFUSAL of the code: the client already let go, and the refusal
/// still names the code, not the password - the code step survives the
/// let-go in the kept attempt (review 74, measured as "wrong password").
#[tokio::test]
async fn a_late_refusal_of_the_code_names_the_code() {

    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        if let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await {
            let _ = fake_server
                .send_to(&build_auth_challenge(), client_addr)
                .await;
            if let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await {
                let _ = fake_server
                    .send_to(
                        &build_proving_auth_result(
                            sdr_remote_core::protocol::AUTH_TOTP_REQUIRED,
                            &buf[..n],
                            "correct-pw",
                        ),
                        client_addr,
                    )
                    .await;
                let _ = fake_server.send_to(&build_totp_challenge(), client_addr).await;
                if let Ok((_n, _)) = fake_server.recv_from(&mut buf).await {
                    tokio::time::sleep(Duration::from_millis(6_500)).await;
                    let _ = fake_server
                        .send_to(
                            &build_auth_result(sdr_remote_core::protocol::AUTH_REJECTED),
                            client_addr,
                        )
                        .await;
                }
            }
        }
    });

    cmd_tx
        .send(Command::Connect(
            server_addr.to_string(),
            Some("correct-pw".to_string()),
        ))
        .unwrap();

    wait_for_status(&mut state_rx, 4_000, |s| {
        matches!(s, ConnectStatus::AwaitingTotp)
    })
    .await;
    cmd_tx
        .send(Command::SendTotpCode("000000".to_string()))
        .unwrap();
    wait_for_status(&mut state_rx, 7_000, |s| {
        matches!(s, ConnectStatus::Connecting)
    })
    .await;
    let status = wait_for_status(&mut state_rx, 4_000, |s| {
        matches!(s, ConnectStatus::Failed(_))
    })
    .await;
    assert_eq!(status, ConnectStatus::Failed(ConnectError::WrongTotp), "it was the code, not the password");

    let _ = shutdown_tx.send(true);
}

/// The late answer in a SECOND session: the client already holds a session
/// key, the server challenges again (a restart), the second code is answered
/// late. In build 73 this sat at Connecting for good - the late answer went to
/// the duplicate arm (review 73, probe 2; review 74, probe B).
#[tokio::test]
async fn a_late_answer_in_a_second_session_still_connects() {

    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        // Round one: a full 2FA handshake, answered at once, and two acks.
        let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await else { return };
        let _ = fake_server.send_to(&build_auth_challenge(), client_addr).await;
        let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await else { return };
        let _ = fake_server
            .send_to(
                &build_proving_auth_result(sdr_remote_core::protocol::AUTH_TOTP_REQUIRED, &buf[..n], "correct-pw"),
                client_addr,
            )
            .await;
        let _ = fake_server.send_to(&build_totp_challenge(), client_addr).await;
        let Ok((_n, _)) = fake_server.recv_from(&mut buf).await else { return };
        let _ = fake_server
            .send_to(&build_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED), client_addr)
            .await;
        for _ in 0..2 {
            let Ok((_n, _)) = fake_server.recv_from(&mut buf).await else { return };
            let _ = fake_server
                .send_to(&build_heartbeat_ack_full(0, 0, 0, 0), client_addr)
                .await;
        }
        // Round two: the server restarted - a fresh challenge on the next
        // heartbeat, and the second code answered 6.5 s late.
        let Ok((_n, _)) = fake_server.recv_from(&mut buf).await else { return };
        let _ = fake_server.send_to(&build_auth_challenge(), client_addr).await;
        // Heartbeats already in flight land before the AuthResponse: skip them.
        let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await else { return };
        let _ = fake_server
            .send_to(
                &build_proving_auth_result(sdr_remote_core::protocol::AUTH_TOTP_REQUIRED, &buf[..n], "correct-pw"),
                client_addr,
            )
            .await;
        let _ = fake_server.send_to(&build_totp_challenge(), client_addr).await;
        let Ok((_n, _)) = fake_server.recv_from(&mut buf).await else { return };
        tokio::time::sleep(Duration::from_millis(6_500)).await;
        let _ = fake_server
            .send_to(&build_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED), client_addr)
            .await;
        loop {
            if let Ok((_n, _)) = fake_server.recv_from(&mut buf).await {
                let _ = fake_server
                    .send_to(&build_heartbeat_ack_full(0, 0, 0, 0), client_addr)
                    .await;
            } else {
                break;
            }
        }
    });

    cmd_tx
        .send(Command::Connect(
            server_addr.to_string(),
            Some("correct-pw".to_string()),
        ))
        .unwrap();

    wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::AwaitingTotp)).await;
    cmd_tx.send(Command::SendTotpCode("123456".to_string())).unwrap();
    wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::Connected)).await;

    // The second session: a fresh code field, the code, the let-go, the late answer.
    wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::AwaitingTotp)).await;
    cmd_tx.send(Command::SendTotpCode("654321".to_string())).unwrap();
    wait_for_status(&mut state_rx, 7_000, |s| matches!(s, ConnectStatus::Connecting)).await;
    let status = wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::Connected)).await;
    assert_eq!(status, ConnectStatus::Connected, "the late answer of a second session is a verified one too");

    let _ = shutdown_tx.send(true);
}

/// The answer to the code comes late instead of never - a slow relay, a server
/// that blocked for a moment - after the client already let go of the attempt.
/// It is a verified answer all the same: the nonces of the let-go attempt are
/// kept for it. Without them it read as a server that proves nothing (review
/// 73, measured).
#[tokio::test]
async fn a_late_answer_to_the_code_still_connects() {

    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        if let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await {
            let _ = fake_server
                .send_to(&build_auth_challenge(), client_addr)
                .await;
            if let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await {
                let _ = fake_server
                    .send_to(
                        &build_proving_auth_result(
                            sdr_remote_core::protocol::AUTH_TOTP_REQUIRED,
                            &buf[..n],
                            "correct-pw",
                        ),
                        client_addr,
                    )
                    .await;
                let _ = fake_server.send_to(&build_totp_challenge(), client_addr).await;
                // The code: accepted, but the answer leaves well after the
                // client's five seconds.
                if let Ok((_n, _)) = fake_server.recv_from(&mut buf).await {
                    tokio::time::sleep(Duration::from_millis(6_500)).await;
                    let _ = fake_server
                        .send_to(
                            &build_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED),
                            client_addr,
                        )
                        .await;
                    loop {
                        if let Ok((_n, _)) = fake_server.recv_from(&mut buf).await {
                            let _ = fake_server
                                .send_to(&build_heartbeat_ack_full(0, 0, 0, 0), client_addr)
                                .await;
                        } else {
                            break;
                        }
                    }
                }
            }
        }
    });

    cmd_tx
        .send(Command::Connect(
            server_addr.to_string(),
            Some("correct-pw".to_string()),
        ))
        .unwrap();

    wait_for_status(&mut state_rx, 4_000, |s| {
        matches!(s, ConnectStatus::AwaitingTotp)
    })
    .await;
    cmd_tx
        .send(Command::SendTotpCode("123456".to_string()))
        .unwrap();

    // The client lets go after five seconds...
    wait_for_status(&mut state_rx, 7_000, |s| {
        matches!(s, ConnectStatus::Connecting)
    })
    .await;
    // ...and the late answer still connects it.
    let status = wait_for_status(&mut state_rx, 4_000, |s| {
        matches!(s, ConnectStatus::Connected)
    })
    .await;
    assert_eq!(status, ConnectStatus::Connected);

    let _ = shutdown_tx.send(true);
}

#[tokio::test]
async fn tci_unreachable_via_state_flags() {

    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        // Full auth handshake → HeartbeatAck with REPORTS_STATE_FLAGS but NO TCI_CONNECTED
        if let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await {
            let _ = fake_server
                .send_to(&build_auth_challenge(), client_addr)
                .await;
            if let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await {
                let _ = fake_server
                    .send_to(
                        &build_proving_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED, &buf[..n], "pw"),
                        client_addr,
                    )
                    .await;
                // Subsequent heartbeats → reply with REPORTS_STATE_FLAGS but no TCI_CONNECTED.
                loop {
                    if let Ok((_n, _)) = fake_server.recv_from(&mut buf).await {
                        let ack = build_heartbeat_ack_full(
                            0,
                            0,
                            Capabilities::REPORTS_STATE_FLAGS,
                            // Thetis IS configured but its TCI server is down → TciUnreachable.
                            // (THETIS_CONFIGURED clear would mean "no Thetis at all", which is not this case.)
                            ServerStateFlags::THETIS_CONFIGURED,
                        );
                        let _ = fake_server.send_to(&ack, client_addr).await;
                    } else {
                        break;
                    }
                }
            }
        }
    });

    cmd_tx
        .send(Command::Connect(
            server_addr.to_string(),
            Some("pw".to_string()),
        ))
        .unwrap();

    let status = wait_for_status(&mut state_rx, 6_000, |s| {
        matches!(s, ConnectStatus::Failed(ConnectError::TciUnreachable { .. }))
    })
    .await;
    assert!(matches!(
        status,
        ConnectStatus::Failed(ConnectError::TciUnreachable { .. })
    ));

    let _ = shutdown_tx.send(true);
}

#[tokio::test]
async fn a_short_heartbeat_ack_is_no_false_positive_tci_unreachable() {
    // A HeartbeatAck without state_flags - the 16-byte shape, no REPORTS_STATE_FLAGS
    // cap - is no TCI fault. Written for a v2.0.0 server (review finding B3); since
    // decision F such a server is refused at the handshake, so the scripted one
    // proves itself and only its acks are old. The client MUST stay Connected and
    // NOT misread the absent flag as "TCI down".

    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        if let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await {
            let _ = fake_server
                .send_to(&build_auth_challenge(), client_addr)
                .await;
            if let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await {
                let _ = fake_server
                    .send_to(
                        &build_proving_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED, &buf[..n], "pw"),
                        client_addr,
                    )
                    .await;
                // Reply with 16-byte (pre-PATCH-1) HeartbeatAck for several rounds.
                let mut seq: u32 = 0;
                loop {
                    if let Ok((_n, _)) = fake_server.recv_from(&mut buf).await {
                        let ack = build_heartbeat_ack_v2_0_0(seq, 0);
                        seq += 1;
                        let _ = fake_server.send_to(&ack, client_addr).await;
                    } else {
                        break;
                    }
                }
            }
        }
    });

    cmd_tx
        .send(Command::Connect(
            server_addr.to_string(),
            Some("pw".to_string()),
        ))
        .unwrap();

    // Wait for Connected
    wait_for_status(&mut state_rx, 4_000, |s| {
        matches!(s, ConnectStatus::Connected)
    })
    .await;

    // Now sleep 2 seconds and verify the status did NOT transition to TciUnreachable.
    tokio::time::sleep(Duration::from_millis(2_000)).await;
    let s = state_rx.borrow();
    assert!(
        matches!(s.connect_status, ConnectStatus::Connected),
        "old server triggered false-positive TciUnreachable: {:?}",
        s.connect_status
    );

    let _ = shutdown_tx.send(true);
}

#[tokio::test]
async fn tci_recovery_when_flag_returns() {
    // Once TCI_CONNECTED flips back on, ConnectStatus must recover from
    // TciUnreachable to Connected.

    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();
    let tci_up = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let tci_up_clone = tci_up.clone();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        if let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await {
            let _ = fake_server
                .send_to(&build_auth_challenge(), client_addr)
                .await;
            if let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await {
                let _ = fake_server
                    .send_to(
                        &build_proving_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED, &buf[..n], "pw"),
                        client_addr,
                    )
                    .await;
                loop {
                    if let Ok((_n, _)) = fake_server.recv_from(&mut buf).await {
                        // Thetis is always configured here; only the TCI_CONNECTED bit flips.
                        let tci_bit = ServerStateFlags::THETIS_CONFIGURED
                            | if tci_up_clone.load(std::sync::atomic::Ordering::Relaxed) {
                                ServerStateFlags::TCI_CONNECTED
                            } else {
                                0
                            };
                        let ack = build_heartbeat_ack_full(
                            0,
                            0,
                            Capabilities::REPORTS_STATE_FLAGS,
                            tci_bit,
                        );
                        let _ = fake_server.send_to(&ack, client_addr).await;
                    } else {
                        break;
                    }
                }
            }
        }
    });

    cmd_tx
        .send(Command::Connect(
            server_addr.to_string(),
            Some("pw".to_string()),
        ))
        .unwrap();

    // First we expect TciUnreachable (TCI bit is clear at start)
    wait_for_status(&mut state_rx, 6_000, |s| {
        matches!(s, ConnectStatus::Failed(ConnectError::TciUnreachable { .. }))
    })
    .await;

    // Flip TCI up
    tci_up.store(true, std::sync::atomic::Ordering::Relaxed);

    // Verify recovery to Connected within a few heartbeat cycles
    let status = wait_for_status(&mut state_rx, 5_000, |s| {
        matches!(s, ConnectStatus::Connected)
    })
    .await;
    assert_eq!(status, ConnectStatus::Connected);

    let _ = shutdown_tx.send(true);
}

#[tokio::test]
async fn retry_after_failed_broadcasts_connecting_immediately() {
    // Regression test for the retry-after-WrongPassword UX: after a Failed
    // state, sending Command::Connect again must flip connect_status to
    // Connecting immediately (visible via the watch channel) — without
    // this broadcast the UI keeps the red "Wrong password" banner up
    // for several seconds until the next packet event triggers a state
    // send. Owner reported this during PATCH-2 smoke-testing.

    // Stage 1: reach Failed(WrongPassword) via a fake server.
    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();
    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        if let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await {
            let _ = fake_server
                .send_to(&build_auth_challenge(), client_addr)
                .await;
            if let Ok((_n, _)) = fake_server.recv_auth_response(&mut buf).await {
                let _ = fake_server
                    .send_to(
                        &build_auth_result(sdr_remote_core::protocol::AUTH_REJECTED),
                        client_addr,
                    )
                    .await;
            }
        }
    });
    cmd_tx
        .send(Command::Connect(
            server_addr.to_string(),
            Some("wrong-password".to_string()),
        ))
        .unwrap();
    wait_for_status(&mut state_rx, 4_000, |s| {
        matches!(s, ConnectStatus::Failed(ConnectError::WrongPassword))
    })
    .await;

    // Stage 2: retry with a different password. The status MUST transition
    // to Connecting promptly — checked with a tight deadline (500ms) so a
    // regression where the engine only updates state on the next packet
    // event clearly fails the test.
    cmd_tx
        .send(Command::Connect(
            server_addr.to_string(),
            Some("retry-password".to_string()),
        ))
        .unwrap();
    let status = wait_for_status(&mut state_rx, 500, |s| {
        matches!(s, ConnectStatus::Connecting)
    })
    .await;
    assert_eq!(status, ConnectStatus::Connecting);

    let _ = shutdown_tx.send(true);
}

#[tokio::test]
async fn retry_same_target_during_awaiting_totp_is_noop() {
    // Regression test for the same-target-during-AwaitingTotp behaviour:
    // a second Command::Connect with identical server+password while the
    // engine is in AwaitingTotp must NOT regress the status to Connecting
    // — the server's PendingTotp session would not re-issue AuthChallenge
    // and the user would be stuck.

    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();
    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        if let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await {
            let _ = fake_server
                .send_to(&build_auth_challenge(), client_addr)
                .await;
            if let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await {
                let _ = fake_server
                    .send_to(
                        &build_proving_auth_result(sdr_remote_core::protocol::AUTH_TOTP_REQUIRED, &buf[..n], "correct-pw"),
                        client_addr,
                    )
                    .await;
            }
        }
    });
    cmd_tx
        .send(Command::Connect(
            server_addr.to_string(),
            Some("correct-pw".to_string()),
        ))
        .unwrap();
    wait_for_status(&mut state_rx, 4_000, |s| {
        matches!(s, ConnectStatus::AwaitingTotp)
    })
    .await;

    // Send the same Connect again — should be a no-op.
    cmd_tx
        .send(Command::Connect(
            server_addr.to_string(),
            Some("correct-pw".to_string()),
        ))
        .unwrap();

    // Wait a bit and assert the status hasn't regressed.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let cur = state_rx.borrow().connect_status.clone();
    assert!(
        matches!(cur, ConnectStatus::AwaitingTotp),
        "expected AwaitingTotp to be preserved, got {:?}",
        cur
    );

    let _ = shutdown_tx.send(true);
}

/// A server that restarts and comes back INSIDE the connection timeout must
/// still get the client's subscriptions back.
///
/// This is the fault the owner found by ear: the noise from the draining
/// jitter buffer is the window in which the client still believes it is
/// connected. Restart inside it and the client re-authenticates without ever
/// having considered itself away - and the restore block is gated on exactly
/// that belief, so it was skipped and the server kept the defaults of its
/// fresh session. RX1 audio is on by default and everything the client has to
/// ask for is not, which is what "only RX1 comes back" means.
///
/// The test drives it the way it happens: authenticate, settle, then send a
/// SECOND auth challenge with no disconnect in between, and require that the
/// client asks for its subscriptions again.
#[tokio::test]
async fn a_second_authentication_restores_the_subscriptions() {
    use sdr_remote_core::protocol::{ControlId, ControlPacket};


    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    let (found_tx, mut found_rx) = mpsc::unbounded_channel::<bool>();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        let mut client: Option<std::net::SocketAddr> = None;

        // Round one: challenge, accept, and one ack so the client settles into
        // "connected" and runs its first restore.
        if let Ok((_n, addr)) = fake_server.recv_from(&mut buf).await {
            client = Some(addr);
            let _ = fake_server.send_to(&build_auth_challenge(), addr).await;
            if let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await {
                let _ = fake_server
                    .send_to(
                        &build_proving_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED, &buf[..n], "pw"),
                        addr,
                    )
                    .await;
                let _ = fake_server
                    .send_to(&build_heartbeat_ack_full(0, 0, 0, 0), addr)
                    .await;
            }
        }

        // Drain whatever the first restore sent, briefly.
        let settle = Instant::now();
        while settle.elapsed() < Duration::from_millis(600) {
            let _ = tokio::time::timeout(
                Duration::from_millis(50),
                fake_server.recv_from(&mut buf),
            )
            .await;
        }

        // Round two: the restart. A fresh challenge, no disconnect at all -
        // the client still believes it is connected.
        let Some(addr) = client else { let _ = found_tx.send(false); return };
        let _ = fake_server.send_to(&build_auth_challenge(), addr).await;
        if let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await {
            let _ = fake_server
                .send_to(
                    &build_proving_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED, &buf[..n], "pw"),
                    addr,
                )
                .await;
            let _ = fake_server
                .send_to(&build_heartbeat_ack_full(1, 1, 0, 0), addr)
                .await;
        }

        // Does it ask for its subscriptions again? Rx1Enable is in the restore
        // block and is sent unconditionally, so it is the cheapest thing to
        // look for.
        let deadline = Instant::now();
        let mut seen = false;
        while deadline.elapsed() < Duration::from_secs(3) && !seen {
            if let Ok(Ok((n, _))) = tokio::time::timeout(
                Duration::from_millis(100),
                fake_server.recv_from(&mut buf),
            )
            .await
            {
                if n >= ControlPacket::SIZE {
                    if let Ok(c) = ControlPacket::deserialize(&buf[..n]) {
                        if c.control_id == ControlId::Rx1Enable {
                            seen = true;
                        }
                    }
                }
            }
        }
        let _ = found_tx.send(seen);
    });

    cmd_tx
        .send(Command::Connect(
            server_addr.to_string(),
            Some("pw".to_string()),
        ))
        .unwrap();

    let _ = wait_for_status(&mut state_rx, 4_000, |s| {
        matches!(s, ConnectStatus::Connected)
    })
    .await;

    let asked_again = tokio::time::timeout(Duration::from_secs(10), found_rx.recv())
        .await
        .expect("the fake server should have reported")
        .unwrap_or(false);
    assert!(
        asked_again,
        "after a second authentication the client must ask for its subscriptions again -          without it the server keeps the defaults of its fresh session and only RX1 returns"
    );

    let _ = shutdown_tx.send(true);
}

/// A press does not run into a new session (field 2026-10-18 13:37, wifi to
/// 5G during a press). The server counts a client as signed in once the answer
/// to its challenge is in, and a PTT packet from a signed-in client keys the
/// transmitter. Until build 252 the client cleared its transmit flags only at
/// "Auth accepted": everything it sent between its answer and that acceptance
/// still carried the press from the session that was gone, and the server keyed
/// Thetis for ~30 ms before the release came. The acceptance is held back here
/// as a relay holds it back on the road.
#[tokio::test]
async fn a_press_does_not_run_into_a_new_session() {
    use sdr_remote_core::protocol::Packet;
    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    // The tones the engine asked for (build 253): none at the first connect,
    // one when the press goes at the second challenge.
    let tones = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let (engine, mut state_rx, cmd_tx) = ClientEngine::new();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let factory_tones = tones.clone();
    tokio::spawn(async move {
        engine
            .run(
                move |_: Option<&str>, _: Option<&str>| -> Result<Box<dyn AudioBackend>> {
                    Ok(Box::new(ToneCountingAudio(factory_tones.clone())))
                },
                shutdown_rx,
            )
            .await
    });
    let (go_tx, mut go_rx) = mpsc::unbounded_channel::<()>();
    // (keyed before the second challenge, packets after its answer, keyed among them)
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel::<(bool, usize, usize)>();
    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        let Ok((_n, client)) = fake_server.recv_from(&mut buf).await else { return };
        let _ = fake_server.send_to(&build_auth_challenge(), client).await;
        let mut second = false;
        let mut answered_at: Option<Instant> = None;
        let mut answer: Vec<u8> = Vec::new();
        let (mut keyed_before, mut after, mut keyed_after) = (false, 0usize, 0usize);
        loop {
            if let Some(t) = answered_at {
                if t.elapsed() >= Duration::from_millis(500) {
                    let _ = seen_tx.send((keyed_before, after, keyed_after));
                    let ok = build_proving_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED, &answer, "pw");
                    let _ = fake_server.send_to(&ok, client).await;
                    return;
                }
            }
            if !second && go_rx.try_recv().is_ok() {
                second = true;
                let _ = fake_server.send_to(&build_auth_challenge(), client).await;
            }
            let Ok(Ok((n, _))) = tokio::time::timeout(Duration::from_millis(20), fake_server.recv_from(&mut buf)).await
            else { continue };
            let keyed = match Packet::deserialize(&buf[..n]) {
                Ok(Packet::AuthResponse { .. }) => {
                    if !second {
                        let ok = build_proving_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED, &buf[..n], "pw");
                        let _ = fake_server.send_to(&ok, client).await;
                    } else if answered_at.is_none() {
                        answered_at = Some(Instant::now());
                        answer = buf[..n].to_vec();
                    }
                    continue;
                }
                Ok(Packet::Heartbeat(hb)) => {
                    let ack = build_heartbeat_ack_full(hb.sequence, hb.local_time, 0, 0);
                    let _ = fake_server.send_to(&ack, client).await;
                    hb.flags.ptt()
                }
                Ok(Packet::Audio(a)) => a.flags.ptt(),
                _ => continue,
            };
            if answered_at.is_some() {
                after += 1;
                if keyed {
                    keyed_after += 1;
                }
            } else if !second && keyed {
                keyed_before = true;
            }
        }
    });
    cmd_tx.send(Command::Connect(server_addr.to_string(), Some("pw".to_string()))).unwrap();
    wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::Connected)).await;
    wait_for_connected_flag(&mut state_rx).await;
    cmd_tx.send(Command::SetPtt(true)).unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let tones_before = tones.load(std::sync::atomic::Ordering::SeqCst);
    let _ = go_tx.send(());
    let (keyed_before, after, keyed_after) = tokio::time::timeout(Duration::from_secs(5), seen_rx.recv())
        .await
        .expect("the fake server reports")
        .expect("a report");
    // Past the acceptance too: `session_lost!` there finds nothing on any more.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let tones_after = tones.load(std::sync::atomic::Ordering::SeqCst);
    let _ = shutdown_tx.send(true);
    assert!(keyed_before, "the press was on the wire in the first session");
    assert!(after > 0, "packets went out after the answer to the second challenge");
    assert_eq!(keyed_after, 0, "none of the {after} packets after the answer carries the old press");
    assert_eq!(tones_before, 0, "no tone at a connect with nothing on");
    assert_eq!(tones_after, 1, "one tone when the press was let go, and not a second one at the acceptance");
}

/// A radio's PTT rides on its audio (build 282, open point 39): a press sends
/// packets with the flag - from the next audio tick since build 292, as
/// Thetis's; until then the first went at once on the button - and a release
/// sends the burst without it, on the radio's own sequence. No control keys a
/// Yaesu any more. Twice per radio without a restart (working rule 9), and the
/// second radio on a sequence of its own.
#[tokio::test]
async fn a_radios_ptt_goes_on_its_audio_packets() {
    use sdr_remote_core::protocol::{ControlId, Packet};
    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();
    // (radio slot, PTT flag, sequence, empty, when) per Yaesu audio packet, and
    // the Yaesu PTT controls seen.
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel::<(u8, bool, u32, bool, Instant)>();
    let controls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let controls_seen = controls.clone();
    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        let Ok((_n, client)) = fake_server.recv_from(&mut buf).await else { return };
        let _ = fake_server.send_to(&build_auth_challenge(), client).await;
        loop {
            let Ok((n, _)) = fake_server.recv_from(&mut buf).await else { return };
            match Packet::deserialize(&buf[..n]) {
                Ok(Packet::AuthResponse { .. }) => {
                    let ok = build_proving_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED, &buf[..n], "pw");
                    let _ = fake_server.send_to(&ok, client).await;
                }
                Ok(Packet::Heartbeat(hb)) => {
                    let ack = build_heartbeat_ack_full(hb.sequence, hb.local_time, 0, 0);
                    let _ = fake_server.send_to(&ack, client).await;
                }
                Ok(Packet::AudioYaesu(a)) => {
                    let _ = seen_tx.send((0, a.flags.ptt(), a.sequence, a.opus_data.is_empty(), Instant::now()));
                }
                Ok(Packet::AudioYaesu2(a)) => {
                    let _ = seen_tx.send((1, a.flags.ptt(), a.sequence, a.opus_data.is_empty(), Instant::now()));
                }
                Ok(Packet::Control(c)) if matches!(c.control_id, ControlId::YaesuPtt | ControlId::Yaesu2Ptt) => {
                    controls_seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                _ => {}
            }
        }
    });
    cmd_tx.send(Command::Connect(server_addr.to_string(), Some("pw".to_string()))).unwrap();
    wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::Connected)).await;
    wait_for_connected_flag(&mut state_rx).await;
    let mut packets = Vec::new();
    for (slot, press) in [(0u8, 1), (0, 2), (1, 1), (1, 2)] {
        let set = |on: bool| if slot == 0 { Command::SetYaesuPtt(on) } else { Command::SetYaesu2Ptt(on) };
        let pressed = Instant::now();
        cmd_tx.send(set(true)).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        cmd_tx.send(set(false)).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut this = Vec::new();
        while let Ok(p) = seen_rx.try_recv() {
            this.push(p);
        }
        let what = format!("radio {} press {press}", slot + 1);
        assert!(this.iter().all(|p| p.0 == slot), "{what}: only its own radio's packets: {this:?}");
        let on: Vec<_> = this.iter().filter(|p| p.1).collect();
        let off: Vec<_> = this.iter().filter(|p| !p.1).collect();
        // No microphone here, so the flag rides on packets of its own: five
        // for the press, five for the release.
        assert_eq!((on.len(), off.len()), (5, 5), "{what}: {this:?}");
        assert!(this.iter().all(|p| p.3), "{what}: no audio, only the flag");
        assert!(on.iter().all(|p| off.iter().all(|q| p.2 < q.2)), "{what}: the release comes after the press: {this:?}");
        // At once or a tick later is not told apart here (a tick is 20 ms);
        // that the change is seen at the tick (build 292) is pinned in the
        // engine's text guard. What is checked: the burst does not wait for
        // the release.
        assert!(off[0].4.duration_since(on[4].4) > Duration::from_millis(100), "{what}: the press's five went before the release");
        assert!(on[0].4.duration_since(pressed) < Duration::from_millis(150), "{what}");
        packets.extend(this);
    }
    let _ = shutdown_tx.send(true);
    // Each radio on an unbroken run of its own, across both presses.
    for slot in 0..2u8 {
        let seqs: Vec<u32> = packets.iter().filter(|p| p.0 == slot).map(|p| p.2).collect();
        assert_eq!(seqs, (0..20).collect::<Vec<u32>>(), "radio {}: {seqs:?}", slot + 1);
    }
    assert_eq!(controls.load(std::sync::atomic::Ordering::SeqCst), 0, "no control keys a Yaesu any more");
}

// ------------- Which build answered (build 3) -------------
//
// The version tag is judged in the engine, from bytes that came off a socket.
// The unit tests in `protocol.rs` prove the judgement; these prove the whole
// path - packed, sent, read, judged, and put in the state a screen samples.
//
// Added because the two integration tests that existed built the SHORT ack, so
// since build 2 every one of them exercised the `Older` branch and the `Same`
// branch - the normal case, the one where a false complaint would reach every
// station - was never walked end to end (raised in review).

/// An ack that says it comes from `version`, or the older short form when
/// `None`. Deliberately NOT `serialize_with_version` for the `None` case: that
/// stamps our own build, which is the opposite of a server too old to say.
fn build_heartbeat_ack_saying(version: Option<&str>) -> Vec<u8> {
    use sdr_remote_core::protocol::{SubscriptionMask, VersionTag};
    let ack = HeartbeatAck {
        flags: Flags::NONE,
        echo_sequence: 0,
        echo_time: 0,
        capabilities: Capabilities::NONE,
        state_flags: ServerStateFlags::NONE,
        subs: Some(SubscriptionMask(0)),
        version: version.map(VersionTag::from_text),
    };
    if version.is_some() {
        let mut buf = [0u8; HeartbeatAck::SIZE_WITH_VERSION];
        ack.serialize_with_version(&mut buf);
        buf.to_vec()
    } else {
        let mut buf = [0u8; HeartbeatAck::SIZE_WITH_SUBS];
        ack.serialize_with_subs(&mut buf);
        buf.to_vec()
    }
}

/// The same ack, saying `version` and reporting `state_flags` as a server of
/// this build does (REPORTS_STATE_FLAGS set, so the client reads them).
fn build_heartbeat_ack_saying_with_flags(version: &str, state_flags: ServerStateFlags) -> Vec<u8> {
    use sdr_remote_core::protocol::{SubscriptionMask, VersionTag};
    let ack = HeartbeatAck {
        flags: Flags::NONE,
        echo_sequence: 0,
        echo_time: 0,
        capabilities: Capabilities::NONE.with(Capabilities::REPORTS_STATE_FLAGS),
        state_flags,
        subs: Some(SubscriptionMask(0)),
        version: Some(VersionTag::from_text(version)),
    };
    let mut buf = [0u8; HeartbeatAck::SIZE_WITH_VERSION];
    ack.serialize_with_version(&mut buf);
    buf.to_vec()
}

/// Run a whole connect against a fake server that keeps answering with `ack`,
/// and report what the client concluded about the server's build.
async fn build_the_client_sees(ack: Vec<u8>) -> sdr_remote_core::protocol::PeerBuild {
    use sdr_remote_core::protocol::PeerBuild;

    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        if let Ok((_n, addr)) = fake_server.recv_from(&mut buf).await {
            let _ = fake_server.send_to(&build_auth_challenge(), addr).await;
            if let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await {
                let _ = fake_server
                    .send_to(
                        &build_proving_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED, &buf[..n], "pw"),
                        addr,
                    )
                    .await;
                // Keep answering: the judgement must hold over a run of acks,
                // not read right once on a lucky first packet.
                for _ in 0..40 {
                    let _ = fake_server.send_to(&ack, addr).await;
                    if tokio::time::timeout(
                        Duration::from_millis(100),
                        fake_server.recv_from(&mut buf),
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });

    cmd_tx
        .send(Command::Connect(
            server_addr.to_string(),
            Some("pw".to_string()),
        ))
        .unwrap();
    let _ = wait_for_status(&mut state_rx, 4_000, |s| {
        matches!(s, ConnectStatus::Connected)
    })
    .await;

    let start = Instant::now();
    let mut seen = PeerBuild::Unknown;
    while start.elapsed() < Duration::from_secs(4) {
        seen = state_rx.borrow().server_build.clone();
        if seen != PeerBuild::Unknown {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let _ = shutdown_tx.send(true);
    seen
}

/// The normal case, and the one that had no end-to-end cover: a server of this
/// build must read as the same build. If this ever broke, every station would
/// see a warning about a server it exactly matches.
#[tokio::test]
async fn a_server_of_this_build_reads_as_the_same_build() {
    use sdr_remote_core::protocol::PeerBuild;
    let ack = build_heartbeat_ack_saying(Some(&sdr_remote_core::version_string()));
    assert_eq!(build_the_client_sees(ack).await, PeerBuild::Same);
}

/// And the other state of the same check (working rule 13): a server that names
/// a different build is named back, over the whole path.
#[tokio::test]
async fn a_server_of_another_build_is_named_over_the_wire() {
    use sdr_remote_core::protocol::PeerBuild;
    let ack = build_heartbeat_ack_saying(Some("2.11.0 (build 93)"));
    assert_eq!(
        build_the_client_sees(ack).await,
        PeerBuild::Different("2.11.0 (build 93)".to_string())
    );
}

/// A server from before the field says nothing, and saying nothing is itself a
/// difference - a server of this build would have said which one it is.
#[tokio::test]
async fn a_server_too_old_to_say_reads_as_older() {
    use sdr_remote_core::protocol::PeerBuild;
    let ack = build_heartbeat_ack_saying(None);
    assert_eq!(build_the_client_sees(ack).await, PeerBuild::Older);
}

// ---------------------------------------------------------------- phase 2b

use sdr_remote_core::control_ack::ControlKey;

/// The key without its number: an act (`ControlKey::Action`) has a key of its
/// own per send, and the scripted servers count and drop by control.
fn plain(key: ControlKey) -> ControlKey {
    match key {
        ControlKey::Action { id, .. } => ControlKey::Control(id),
        ControlKey::YaesuAction { slot, control, .. } => ControlKey::Yaesu { slot, control },
        other => other,
    }
}

/// What the scripted server does with one numbered control.
#[derive(Clone, Copy)]
enum AckPolicy {
    /// Acknowledge everything.
    All,
    /// Acknowledge nothing: a server from before phase 2b.
    Nothing,
    /// Acknowledge everything except the first `n` copies of `key`.
    DropFirstCopies { key: ControlKey, n: u32 },
    /// Acknowledge everything except `key`, ever.
    NeverAck { key: ControlKey },
}

/// A server that authenticates, acks heartbeats, applies `policy` to numbered
/// controls, and reports every copy of a control it receives as (key, seq) -
/// a `ControlPacket` and a `YaesuControlPacket` alike. A control WITHOUT a
/// number is reported with seq 0 (never a real number): a send that skipped
/// the numbering would otherwise be invisible here, and its repeat - which
/// does carry the number - would make the count look right (reviews 87).
async fn ack_server(
    fake_server: FakeServer,
    policy: AckPolicy,
    seen: mpsc::UnboundedSender<(ControlKey, u32)>,
) {
    use sdr_remote_core::protocol::{numbered_control, ControlAckPacket, Packet, PacketType};
    let mut buf = [0u8; 1500];
    let Ok((_n, client)) = fake_server.recv_from(&mut buf).await else { return };
    let _ = fake_server.send_to(&build_auth_challenge(), client).await;
    let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await else { return };
    let response = buf[..n].to_vec();
    let _ = fake_server
        .send_to(
            &build_proving_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED, &response, "correct-pw"),
            client,
        )
        .await;
    let mut copies: std::collections::HashMap<ControlKey, u32> = std::collections::HashMap::new();
    loop {
        let Ok((n, _)) = fake_server.recv_from(&mut buf).await else { return };
        let data = &buf[..n];
        // Echo the heartbeat so the engine measures a real (local) round trip;
        // the first repeat waits on it.
        if let Ok(Packet::Heartbeat(hb)) = Packet::deserialize(data) {
            let ack = build_heartbeat_ack_full(hb.sequence, hb.local_time, 0, 0);
            let _ = fake_server.send_to(&ack, client).await;
            continue;
        }
        let Some((key, seq)) = numbered_control(data).map(|(k, n)| (plain(k), n)) else {
            let unnumbered = match data.get(2).copied() {
                Some(t) if t == PacketType::Control as u8 && data.len() >= 7 => Some(ControlKey::Control(data[4])),
                Some(t) if t == PacketType::YaesuControl as u8 && data.len() >= 8 => {
                    Some(ControlKey::Yaesu { slot: data[4], control: data[5] })
                }
                _ => None,
            };
            if let Some(key) = unnumbered {
                let _ = seen.send((key, 0));
            }
            continue;
        };
        let _ = seen.send((key, seq));
        let count = copies.entry(key).or_insert(0);
        *count += 1;
        let ack = match policy {
            AckPolicy::All => true,
            AckPolicy::Nothing => false,
            AckPolicy::DropFirstCopies { key: drop, n } => key != drop || *count > n,
            AckPolicy::NeverAck { key: never } => key != never,
        };
        if ack {
            let mut a = [0u8; ControlAckPacket::SIZE];
            ControlAckPacket::serialize(seq, &mut a);
            let _ = fake_server.send_to(&a, client).await;
        }
    }
}

const VFO_SWAP: ControlKey = ControlKey::Control(sdr_remote_core::protocol::ControlId::VfoSwap as u8);
/// Slot 0, control 2 (NAR on the Yaesu panel).
const YAESU_NAR: ControlKey = ControlKey::Yaesu { slot: 0, control: 2 };

/// Connect against `ack_server`, let the opening burst settle, send `command`,
/// and return every copy of `key` the server saw within `window`.
async fn copies_of(policy: AckPolicy, command: Command, key: ControlKey, window: Duration) -> Vec<u32> {
    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    tokio::spawn(ack_server(fake_server, policy, seen_tx));
    cmd_tx
        .send(Command::Connect(server_addr.to_string(), Some("correct-pw".to_string())))
        .unwrap();
    wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::Connected)).await;
    // The opening burst goes out on the first heartbeat ack (the connected
    // flag). Let it and its acknowledgements settle, so the copies counted below
    // are of `command` alone.
    wait_for_connected_flag(&mut state_rx).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    while seen_rx.try_recv().is_ok() {}
    cmd_tx.send(command).unwrap();
    let until = Instant::now() + window;
    let mut seqs = Vec::new();
    while Instant::now() < until {
        if let Ok(Some((k, seq))) =
            tokio::time::timeout(Duration::from_millis(50), seen_rx.recv()).await
        {
            if k == key {
                seqs.push(seq);
            }
        }
    }
    let _ = shutdown_tx.send(true);
    seqs
}

async fn vfo_swap_copies(policy: AckPolicy, window: Duration) -> Vec<u32> {
    let swap = Command::SetControl(sdr_remote_core::protocol::ControlId::VfoSwap, 1);
    copies_of(policy, swap, VFO_SWAP, window).await
}

/// A control the server did not acknowledge is sent again, with the same
/// number, once this session's server has shown it acknowledges (phase 2b).
#[tokio::test]
async fn a_lost_control_is_sent_again_with_the_same_number() {
    let seqs = vfo_swap_copies(AckPolicy::DropFirstCopies { key: VFO_SWAP, n: 1 }, Duration::from_millis(1_500)).await;
    assert_eq!(seqs.len(), 2, "the lost copy and one repeat, then the ack stops it: {seqs:?}");
    assert_ne!(seqs[0], 0, "the first copy carries a number: {seqs:?}");
    assert_eq!(seqs[0], seqs[1], "a repeat carries the number of the original");
}

/// The same for a typed Yaesu control (`Command::SetYaesuControl`): before
/// build 87 the one control with no repair at all, and it goes to a radio that
/// can transmit (reviews 87).
#[tokio::test]
async fn a_lost_yaesu_control_is_sent_again_with_the_same_number() {
    let nar = Command::SetYaesuControl(0, 2, 1);
    let seqs =
        copies_of(AckPolicy::DropFirstCopies { key: YAESU_NAR, n: 1 }, nar, YAESU_NAR, Duration::from_millis(1_500)).await;
    assert_eq!(seqs.len(), 2, "the lost copy and one repeat: {seqs:?}");
    assert_ne!(seqs[0], 0, "the first copy carries a number: {seqs:?}");
    assert_eq!(seqs[0], seqs[1], "a repeat carries the number of the original");
}

/// A control left unacknowledged in the old session is not repeated into the
/// new one (build 254, rounds 252). The server counts a client as signed in once
/// the answer to its challenge is in, and its new session applies whatever
/// numbered control comes next - a repeated `YaesuPtt=1` or `ThetisTune` from
/// before the switch would key a transmitter. Until 254 the repeats stopped only
/// at "Auth accepted", and the acceptance is held back here as a relay holds it
/// back on the road.
#[tokio::test]
async fn an_unacknowledged_control_is_not_repeated_into_a_new_session() {
    use sdr_remote_core::protocol::{numbered_control, ControlAckPacket, Packet};
    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();
    let (go_tx, mut go_rx) = mpsc::unbounded_channel::<()>();
    // (copies before the second challenge, copies after its answer)
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel::<(usize, usize)>();
    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        let Ok((_n, client)) = fake_server.recv_from(&mut buf).await else { return };
        let _ = fake_server.send_to(&build_auth_challenge(), client).await;
        let mut second = false;
        let mut answered_at: Option<Instant> = None;
        let mut answer: Vec<u8> = Vec::new();
        let (mut before, mut after) = (0usize, 0usize);
        loop {
            if let Some(t) = answered_at {
                if t.elapsed() >= Duration::from_millis(1_500) {
                    let _ = seen_tx.send((before, after));
                    let ok = build_proving_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED, &answer, "pw");
                    let _ = fake_server.send_to(&ok, client).await;
                    return;
                }
            }
            if !second && go_rx.try_recv().is_ok() {
                second = true;
                let _ = fake_server.send_to(&build_auth_challenge(), client).await;
            }
            let Ok(Ok((n, _))) = tokio::time::timeout(Duration::from_millis(20), fake_server.recv_from(&mut buf)).await
            else { continue };
            let data = &buf[..n];
            match Packet::deserialize(data) {
                Ok(Packet::AuthResponse { .. }) => {
                    if !second {
                        let ok = build_proving_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED, data, "pw");
                        let _ = fake_server.send_to(&ok, client).await;
                    } else if answered_at.is_none() {
                        answered_at = Some(Instant::now());
                        answer = data.to_vec();
                    }
                    continue;
                }
                Ok(Packet::Heartbeat(hb)) => {
                    let ack = build_heartbeat_ack_full(hb.sequence, hb.local_time, 0, 0);
                    let _ = fake_server.send_to(&ack, client).await;
                    continue;
                }
                _ => {}
            }
            let Some((key, seq)) = numbered_control(data).map(|(k, s)| (plain(k), s)) else { continue };
            if key == VFO_SWAP {
                // Never acknowledged: it stays pending, and is repeated.
                if answered_at.is_some() {
                    after += 1;
                } else if !second {
                    before += 1;
                }
            } else {
                // Everything else is: the server has shown it acknowledges.
                let mut a = [0u8; ControlAckPacket::SIZE];
                ControlAckPacket::serialize(seq, &mut a);
                let _ = fake_server.send_to(&a, client).await;
            }
        }
    });
    cmd_tx.send(Command::Connect(server_addr.to_string(), Some("pw".to_string()))).unwrap();
    wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::Connected)).await;
    wait_for_connected_flag(&mut state_rx).await;
    // The opening burst and its acknowledgements: repeating is on.
    tokio::time::sleep(Duration::from_millis(300)).await;
    cmd_tx.send(Command::SetControl(sdr_remote_core::protocol::ControlId::VfoSwap, 1)).unwrap();
    // Before its first repeat (at least 100 ms): the switch comes now.
    tokio::time::sleep(Duration::from_millis(30)).await;
    let _ = go_tx.send(());
    let (before, after) = tokio::time::timeout(Duration::from_secs(6), seen_rx.recv())
        .await
        .expect("the fake server reports")
        .expect("a report");
    let _ = shutdown_tx.send(true);
    assert_eq!(before, 1, "the control went out once in the first session");
    assert_eq!(after, 0, "and is not repeated after the answer to the second challenge");
}

/// Against a server that never acknowledges - one from before phase 2b - a
/// control goes once, as before: a repeat there could press a button twice.
#[tokio::test]
async fn a_server_that_never_acknowledges_gets_each_control_once() {
    let seqs = vfo_swap_copies(AckPolicy::Nothing, Duration::from_millis(1_500)).await;
    assert_eq!(seqs.len(), 1, "{seqs:?}");
}

/// A control the server never acknowledges is repeated four times and then
/// given up - it does not go on for ever.
#[tokio::test]
async fn an_unacknowledged_control_is_given_up_after_four_repeats() {
    let seqs = vfo_swap_copies(AckPolicy::NeverAck { key: VFO_SWAP }, Duration::from_millis(4_000)).await;
    assert_eq!(seqs.len(), 1 + sdr_remote_core::control_ack::MAX_REPEATS as usize, "{seqs:?}");
    assert!(seqs.iter().all(|s| *s == seqs[0]));
}

/// The opening burst goes out before the first acknowledgement can be back. A
/// subscription lost from it is still repeated, with its own number, once the
/// server has acknowledged the rest - a reconnect is exactly where a lost one
/// hurts.
#[tokio::test]
async fn a_subscription_lost_from_the_opening_burst_is_sent_again() {
    let rx1 = ControlKey::Control(sdr_remote_core::protocol::ControlId::Rx1Enable as u8);
    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    tokio::spawn(ack_server(fake_server, AckPolicy::DropFirstCopies { key: rx1, n: 1 }, seen_tx));
    cmd_tx
        .send(Command::Connect(server_addr.to_string(), Some("correct-pw".to_string())))
        .unwrap();
    wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::Connected)).await;
    tokio::time::sleep(Duration::from_millis(2_000)).await;
    let _ = shutdown_tx.send(true);
    let mut copies = Vec::new();
    while let Ok((key, seq)) = seen_rx.try_recv() {
        if key == rx1 {
            copies.push(seq);
        }
    }
    assert_eq!(copies.len(), 2, "lost once, sent again once: {copies:?}");
    assert_eq!(copies[0], copies[1], "the same number: {copies:?}");
}

/// Send `command()` twice, with the first copy of `key` lost, and return every
/// copy of `key` the server saw.
async fn two_presses_first_lost(command: impl Fn() -> Command, key: ControlKey) -> Vec<u32> {
    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    tokio::spawn(ack_server(fake_server, AckPolicy::DropFirstCopies { key, n: 1 }, seen_tx));
    cmd_tx
        .send(Command::Connect(server_addr.to_string(), Some("correct-pw".to_string())))
        .unwrap();
    wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::Connected)).await;
    wait_for_connected_flag(&mut state_rx).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    while seen_rx.try_recv().is_ok() {}
    cmd_tx.send(command()).unwrap();
    cmd_tx.send(command()).unwrap();
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let _ = shutdown_tx.send(true);
    let mut seqs = Vec::new();
    while let Ok((k, seq)) = seen_rx.try_recv() {
        if k == key {
            seqs.push(seq);
        }
    }
    seqs
}

fn assert_both_presses_arrive(seqs: &[u32]) {
    assert_eq!(seqs.len(), 3, "first (lost), second, first again: {seqs:?}");
    assert_ne!(seqs[0], 0, "numbered: {seqs:?}");
    assert_ne!(seqs[0], seqs[1], "two presses, two numbers: {seqs:?}");
    assert_eq!(seqs[2], seqs[0], "the lost first one is the one repeated: {seqs:?}");
}

/// Two swaps, the first one lost (reviews 87): the second does not push the
/// first out - both arrive, the first by its repeat, each under its own number.
/// Before build 88 the first was replaced by the second and never repeated.
#[tokio::test]
async fn a_second_swap_does_not_replace_a_lost_first_one() {
    let swap = || Command::SetControl(sdr_remote_core::protocol::ControlId::VfoSwap, 1);
    assert_both_presses_arrive(&two_presses_first_lost(swap, VFO_SWAP).await);
}

/// The V/M button, value 3 of a VFO select: a toggle, so an act (round on
/// build 88 - before build 89 it was a setting, and the second press replaced
/// a lost first one).
#[tokio::test]
async fn a_second_v_m_press_does_not_replace_a_lost_first_one() {
    use sdr_remote_core::protocol::ControlId;
    let vm = || Command::SetControl(ControlId::YaesuSelectVfo, 3);
    let key = ControlKey::Control(ControlId::YaesuSelectVfo as u8);
    assert_both_presses_arrive(&two_presses_first_lost(vm, key).await);
}

/// Two clarifier steps of +10 Hz, the first lost (round on build 88): +20 Hz,
/// not +10. A typed Yaesu control, so the other packet type.
#[tokio::test]
async fn a_second_clarifier_step_does_not_replace_a_lost_first_one() {
    let step = sdr_remote_core::protocol::YaesuCtrl::ClarStep as u8;
    let ten = || Command::SetYaesuControl(0, step, 10);
    assert_both_presses_arrive(&two_presses_first_lost(ten, ControlKey::Yaesu { slot: 0, control: step }).await);
}

/// Acknowledged at once: nothing is repeated.
#[tokio::test]
async fn an_acknowledged_control_goes_once() {
    let seqs = vfo_swap_copies(AckPolicy::All, Duration::from_millis(1_500)).await;
    assert_eq!(seqs.len(), 1, "{seqs:?}");
}

// ------------- What goes over the wire, per session (build 87) -------------
//
// The controls the engine sends on its own - the opening burst of a session and
// the replay after a second authentication - recorded as (id, value) and compared
// with a fixed list. Copies of one number count once. A send that disappears, is
// doubled or swaps place turns this red; nothing else in the suite reads the
// whole burst.

/// Accept, ack every heartbeat and every numbered control, and record each
/// control once per number. A message on `rechallenge` starts a second
/// authentication on the same address; the recording is split there.
async fn recording_server(
    fake_server: FakeServer,
    mut rechallenge: mpsc::UnboundedReceiver<()>,
    seen: mpsc::UnboundedSender<Option<(ControlKey, u16)>>,
    ack_after_rechallenge: bool,
) {
    use sdr_remote_core::protocol::{numbered_control, ControlAckPacket, ControlPacket, Packet, YaesuControlPacket};
    let mut buf = [0u8; 1500];
    let Ok((_n, client)) = fake_server.recv_from(&mut buf).await else { return };
    let _ = fake_server.send_to(&build_auth_challenge(), client).await;
    let mut numbers = std::collections::HashSet::new();
    let mut acking = true;
    loop {
        let n = tokio::select! {
            r = fake_server.recv_from(&mut buf) => match r { Ok((n, _)) => n, Err(_) => return },
            Some(()) = rechallenge.recv() => {
                let _ = seen.send(None);
                acking = ack_after_rechallenge;
                let _ = fake_server.send_to(&build_auth_challenge(), client).await;
                continue;
            }
        };
        let data = &buf[..n];
        match Packet::deserialize(data) {
            Ok(Packet::AuthResponse { .. }) => {
                let ok = build_proving_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED, data, "pw");
                let _ = fake_server.send_to(&ok, client).await;
                continue;
            }
            Ok(Packet::Heartbeat(hb)) => {
                let ack = build_heartbeat_ack_full(hb.sequence, hb.local_time, 0, 0);
                let _ = fake_server.send_to(&ack, client).await;
                continue;
            }
            _ => {}
        }
        let Some((key, seq)) = numbered_control(data).map(|(k, n)| (plain(k), n)) else { continue };
        if acking {
            let mut a = [0u8; ControlAckPacket::SIZE];
            ControlAckPacket::serialize(seq, &mut a);
            let _ = fake_server.send_to(&a, client).await;
        }
        // With acknowledgements off, every copy counts: the test asks how often
        // a control went, not which ones. A Yaesu control is recorded too.
        if numbers.insert(seq) || !acking {
            let value = match key {
                ControlKey::Control(_) | ControlKey::Action { .. } => {
                    ControlPacket::deserialize(data).map(|c| c.value)
                }
                ControlKey::Yaesu { .. } | ControlKey::YaesuAction { .. } => {
                    YaesuControlPacket::deserialize(data).map(|c| c.value)
                }
            };
            if let Ok(v) = value {
                let _ = seen.send(Some((key, v)));
            }
        }
    }
}

#[tokio::test]
async fn the_controls_of_a_session_and_its_replay_go_over_the_wire_once_in_order() {
    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();
    let (re_tx, re_rx) = mpsc::unbounded_channel();
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    tokio::spawn(recording_server(fake_server, re_rx, seen_tx, true));
    cmd_tx
        .send(Command::Connect(server_addr.to_string(), Some("pw".to_string())))
        .unwrap();
    wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::Connected)).await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let _ = re_tx.send(());
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let _ = shutdown_tx.send(true);

    let mut first = Vec::new();
    let mut second = Vec::new();
    let mut in_second = false;
    while let Ok(item) = seen_rx.try_recv() {
        match item {
            None => in_second = true,
            Some(c) if in_second => second.push(c),
            Some(c) => first.push(c),
        }
    }
    use sdr_remote_core::protocol::ControlId::*;
    // The hand-written sends after an acceptance, with this client's defaults.
    let c = |id: sdr_remote_core::protocol::ControlId| ControlKey::Control(id as u8);
    let burst: Vec<(ControlKey, u16)> = vec![
        (c(Rx1Enable), 1),
        (c(AudioMode), 0),
        (c(SmeterSources), 0x22),
        (c(DxSpotsEnabled), 1),
        (c(FullSpectrumEnabled), 1),
    ];
    assert_eq!(first, burst, "the first session");
    // The second session replays what the first one recorded, and THEN sends the
    // same block by hand - so these five go twice. Deliberate since the replay
    // was moved first (the hand-written values read the current variables and
    // should land last); pinned here as it is, not as it might be.
    let replayed_then_sent: Vec<(ControlKey, u16)> = burst.iter().chain(burst.iter()).copied().collect();
    assert_eq!(second, replayed_then_sent, "the second session");
}

/// The second session goes to a server that does not acknowledge (an older
/// server behind the same address after a restart). The first one did. What
/// the first session learnt must not carry over: a control in the second goes
/// exactly once - repeated, it would press the button again on a server that
/// cannot tell a copy from a new press.
#[tokio::test]
async fn a_second_session_with_a_server_that_does_not_acknowledge_repeats_nothing() {
    use sdr_remote_core::protocol::ControlId;
    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();
    let (re_tx, re_rx) = mpsc::unbounded_channel();
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    tokio::spawn(recording_server(fake_server, re_rx, seen_tx, false));
    cmd_tx
        .send(Command::Connect(server_addr.to_string(), Some("pw".to_string())))
        .unwrap();
    wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::Connected)).await;
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    let _ = re_tx.send(());
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    cmd_tx.send(Command::SetControl(ControlId::VfoSwap, 1)).unwrap();
    tokio::time::sleep(Duration::from_millis(3_000)).await;
    let _ = shutdown_tx.send(true);
    let mut in_second = false;
    let mut swaps = 0;
    while let Ok(item) = seen_rx.try_recv() {
        match item {
            None => in_second = true,
            Some((key, _)) if in_second && key == VFO_SWAP => swaps += 1,
            _ => {}
        }
    }
    assert_eq!(swaps, 1, "once, as to any server that does not acknowledge");
}

// ------------- Measurement mode (build 90) -------------

/// Measurement on, a session with a few audio packets and heartbeats, off
/// again: the file holds the header, the session start, every audio packet the
/// engine received (stream, sequence, sender timestamp, size), the round trips,
/// and the stop - the whole path from the command to the disk.
#[tokio::test]
async fn measurement_mode_writes_the_arrivals_of_a_session() {
    use sdr_remote_core::protocol::{AudioPacket, Flags, Packet};
    let dir = std::env::temp_dir().join(format!("tl-measure-harness-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();
    cmd_tx.send(Command::SetMeasurement(Some(dir.to_string_lossy().to_string()))).unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        let Ok((_n, client)) = fake_server.recv_from(&mut buf).await else { return };
        let _ = fake_server.send_to(&build_auth_challenge(), client).await;
        let mut sent_audio = false;
        loop {
            let Ok((n, _)) = fake_server.recv_from(&mut buf).await else { return };
            match Packet::deserialize(&buf[..n]) {
                Ok(Packet::AuthResponse { .. }) => {
                    let ok = build_proving_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED, &buf[..n], "pw");
                    let _ = fake_server.send_to(&ok, client).await;
                }
                Ok(Packet::Heartbeat(hb)) => {
                    let ack = build_heartbeat_ack_full(hb.sequence, hb.local_time, 0, 0);
                    let _ = fake_server.send_to(&ack, client).await;
                    if !sent_audio {
                        sent_audio = true;
                        for seq in 0..5u32 {
                            let mut a = Vec::new();
                            AudioPacket { flags: Flags::NONE, sequence: seq, timestamp: seq * 20, opus_data: vec![0u8; 40] }
                                .serialize(&mut a);
                            let _ = fake_server.send_to(&a, client).await;
                        }
                    }
                }
                _ => {}
            }
        }
    });
    cmd_tx.send(Command::Connect(server_addr.to_string(), Some("pw".to_string()))).unwrap();
    wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::Connected)).await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    // The send side (build 93): key and release, so the file holds both edges
    // and the packets that went out meanwhile.
    cmd_tx.send(Command::SetPtt(true)).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    cmd_tx.send(Command::SetPtt(false)).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let file = state_rx.borrow().measurement_file.clone().expect("the screen sees the file name");
    cmd_tx.send(Command::SetMeasurement(None)).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(state_rx.borrow().measurement_file.is_none(), "and sees it stop");
    let _ = shutdown_tx.send(true);
    let text = std::fs::read_to_string(&file).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[1], sdr_remote_logic::measure::HEADER);
    assert!(lines.iter().any(|l| l.contains(",session_start,quic,")), "the route is written: {lines:?}");
    for seq in 0..5u32 {
        // The last column is the redundancy bit (build 157); these zeros carry none.
        let want = format!(",audio,thetis,{},{},40,0", seq, seq * 20);
        assert!(lines.iter().any(|l| l.ends_with(&want)), "packet {seq}: {lines:?}");
    }
    assert!(lines.iter().filter(|l| l.contains(",rtt,")).count() >= 1, "{lines:?}");
    let on = lines.iter().position(|l| l.ends_with(",ptt,thetis,1,,,")).expect("PTT on is written");
    let off = lines.iter().position(|l| l.ends_with(",ptt,thetis,0,,,")).expect("PTT off is written");
    assert!(on < off);
    assert!(lines[on..off].iter().any(|l| l.contains(",tx,thetis,")), "packets sent while keyed: {lines:?}");
    assert!(lines.last().unwrap().contains(",stop,"), "{lines:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A measurement that starts in the middle of things (rounds 93, 94): a
/// session is running and the transmitter is already keyed. The file still
/// says where it started - the session and its way right after `start`, and a
/// key before the release.
#[tokio::test]
async fn a_measurement_started_mid_session_writes_where_things_stand() {
    let dir = std::env::temp_dir().join(format!("tl-measure-mid-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let fake_server = FakeServer::start().await;
    let server_addr = fake_server.local_addr().unwrap();
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();
    let (seen_tx, _seen_rx) = mpsc::unbounded_channel();
    tokio::spawn(ack_server(fake_server, AckPolicy::All, seen_tx));
    cmd_tx.send(Command::Connect(server_addr.to_string(), Some("correct-pw".to_string()))).unwrap();
    wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::Connected)).await;
    // The session counts as running once the connected flag is up.
    wait_for_connected_flag(&mut state_rx).await;
    cmd_tx.send(Command::SetPtt(true)).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    cmd_tx.send(Command::SetMeasurement(Some(dir.to_string_lossy().to_string()))).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    cmd_tx.send(Command::SetPtt(false)).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let file = state_rx.borrow().measurement_file.clone().expect("measuring");
    cmd_tx.send(Command::SetMeasurement(None)).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let _ = shutdown_tx.send(true);
    let text = std::fs::read_to_string(&file).unwrap();
    let lines: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
    assert!(lines[1].contains(",start,"), "{lines:?}");
    assert!(lines[2].ends_with(",session_start,quic,,,,"), "the running session right after the start: {lines:?}");
    let on = lines.iter().position(|l| l.ends_with(",ptt,thetis,1,,,")).expect("the key that was already down");
    let off = lines.iter().position(|l| l.ends_with(",ptt,thetis,0,,,")).expect("the release");
    assert!(on < off, "{lines:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A measurement with no session behind it writes no `session_start`: the file
/// would say a session ran that never did. Started while a connect is still in
/// flight, so the engine has an address and is trying - but nobody answers.
/// Until build 133 this was half of the classic relay's test (review 111 had
/// found the arm without a test once already; review 133 found it again when
/// that test went with the relay).
#[tokio::test]
async fn a_measurement_without_a_session_writes_no_session_start() {
    let dir = std::env::temp_dir().join(format!("tl-measure-no-session-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();
    // Nobody listens on the discard port: the handshake never completes.
    cmd_tx.send(Command::Connect("127.0.0.1:9".to_string(), Some("pw".to_string()))).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    cmd_tx.send(Command::SetMeasurement(Some(dir.to_string_lossy().to_string()))).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let file = state_rx.borrow().measurement_file.clone().expect("measuring");
    assert!(!state_rx.borrow().connected, "no session came up");
    cmd_tx.send(Command::SetMeasurement(None)).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let _ = shutdown_tx.send(true);
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.contains(",start,"), "the file was written: {text}");
    assert!(!text.contains(",session_start,"), "no session, no session_start: {text}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------- QUIC (phase 3a, build 110) -------------

/// A QUIC server scripted like the UDP ones above: it challenges every new
/// connection, checks the answer against THIS connection's binding (and that the
/// same answer would not verify without it), proves itself over that binding,
/// and acks heartbeats. `bind_proof` false proves without the binding - a server
/// in the middle would have to.
fn spawn_quic_server(password: &'static str, bind_proof: bool) -> (std::net::SocketAddr, std::sync::Arc<std::sync::atomic::AtomicU32>) {
    spawn_quic_server_with(password, bind_proof, false, None)
}

/// `stale_once`: the first answer is checked and its result made, but not sent:
/// the server closes that connection instead and hands the result to the
/// client's NEXT connection, as the first thing on it - a result for a
/// connection that is gone (requirement 4 of review 108). Not admitted, so the
/// counter only counts admissions the client could rightly act on.
fn spawn_quic_server_with(
    password: &'static str,
    bind_proof: bool,
    stale_once: bool,
    relay: Option<sdr_remote_quic::RelayRoute>,
) -> (std::net::SocketAddr, std::sync::Arc<std::sync::atomic::AtomicU32>) {
    use sdr_remote_core::protocol::Packet;
    let (server, mut link) = sdr_remote_quic::QuicServer::start_with(
        "127.0.0.1:0".parse().unwrap(),
        sdr_remote_quic::Congestion::Bbr,
        relay,
    )
    .unwrap();
    let addr = server.local_addr().unwrap();
    let admitted = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let count = admitted.clone();
    tokio::spawn(async move {
        let _server = server;
        // A fresh nonce per challenge, so a second round proves a new
        // authentication, not a repeat of the first (review 110).
        let mut nonce = [7u8; 16];
        // Like the real server: admitted until the client says Disconnect -
        // per connection, as every QUIC connection is its own session.
        let mut in_session = std::collections::HashSet::new();
        let mut stale_pending = stale_once;
        let mut held: Option<(std::net::SocketAddr, Vec<u8>)> = None;
        while let Some((from, packet)) = link.inbound_rx.recv().await {
            let binding = server_binding(&_server, from);
            // The held result goes out first on the next connection, alone - a
            // challenge beside it could overtake it and change what it meets.
            if let Some((_, result)) = held.take_if(|(closed, _)| *closed != from) {
                let _ = link.uplink_tx.send((from, result));
                continue;
            }
            match Packet::deserialize(&packet) {
                Ok(Packet::AuthResponse { hmac, client_nonce: Some(nc) }) => {
                    let b = binding.expect("a live connection has a binding");
                    assert!(sdr_remote_core::auth::verify_hmac(password, &nonce, &hmac, Some(&b)), "the answer covers this connection's binding");
                    assert!(!sdr_remote_core::auth::verify_hmac(password, &nonce, &hmac, None), "and would not verify without it");
                    let used = if bind_proof { Some(&b) } else { None };
                    let key = sdr_remote_core::auth::session_key(password, &nonce, &nc, used);
                    let proof = sdr_remote_core::auth::server_proof(&key, &nonce, &nc, used);
                    let mut buf = [0u8; sdr_remote_core::protocol::AUTH_RESULT_SIZE_WITH_PROOF];
                    let n = sdr_remote_core::protocol::serialize_auth_result(&mut buf, sdr_remote_core::protocol::AUTH_ACCEPTED, Some(&proof));
                    if stale_pending {
                        stale_pending = false;
                        held = Some((from, buf[..n].to_vec()));
                        _server.close(from);
                        continue;
                    }
                    let _ = link.uplink_tx.send((from, buf[..n].to_vec()));
                    count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    in_session.insert(from);
                }
                Ok(Packet::Disconnect) => {
                    in_session.remove(&from);
                }
                Ok(Packet::Heartbeat(_)) if in_session.contains(&from) => {
                    let _ = link.uplink_tx.send((from, build_heartbeat_ack_full(0, 0, 0, 0)));
                }
                Ok(Packet::Heartbeat(_)) => {
                    nonce[0] = nonce[0].wrapping_add(1);
                    let mut buf = [0u8; sdr_remote_core::protocol::AUTH_CHALLENGE_SIZE];
                    sdr_remote_core::protocol::serialize_auth_challenge(&mut buf, &nonce);
                    let _ = link.uplink_tx.send((from, buf.to_vec()));
                }
                _ => {}
            }
        }
    });
    (addr, admitted)
}

fn server_binding(server: &sdr_remote_quic::QuicServer, sentinel: std::net::SocketAddr) -> Option<sdr_remote_quic::Binding> {
    server.bindings.get(sentinel)
}

fn spawn_quic_engine() -> (
    watch::Receiver<RadioState>,
    mpsc::UnboundedSender<Command>,
    watch::Sender<bool>,
) {
    let (state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();
    (state_rx, cmd_tx, shutdown_tx)
}

/// Over QUIC the client answers bound to its connection and checks the
/// server's proof over the same binding: it connects - twice, with a
/// Disconnect between, without a restart (working rule 9). The operator types
/// the server's address with its port; QUIC goes there (build 133) and what
/// comes back reads as the server the operator named.
#[tokio::test]
async fn over_quic_the_client_authenticates_bound_to_its_connection() {
    let (server, admitted) = spawn_quic_server("correct-pw", true);
    let (mut state_rx, cmd_tx, shutdown_tx) = spawn_quic_engine();
    for round in 1..=2u32 {
        cmd_tx
            .send(Command::Connect(format!("127.0.0.1:{}", server.port()), Some("correct-pw".into())))
            .unwrap();
        wait_for_status(&mut state_rx, 5_000, |s| matches!(s, ConnectStatus::Connected)).await;
        wait_for_connected_flag(&mut state_rx).await;
        assert!(admitted.load(std::sync::atomic::Ordering::Relaxed) >= round, "round {round}: the server admitted the bound answer");
        assert_eq!(state_rx.borrow().route, sdr_remote_logic::measure::Route::Quic, "round {round}: the screen says QUIC");
        // The connection name (build 139): no relay, to an address on this
        // machine - local.
        wait_for_path(&mut state_rx, Some(sdr_remote_core::path::PathKind::Local), &format!("round {round}")).await;
        cmd_tx.send(Command::Disconnect).unwrap();
        wait_for_status(&mut state_rx, 3_000, |s| !matches!(s, ConnectStatus::Connected)).await;
        no_path_stays(&state_rx, &format!("round {round}, after Disconnect")).await;
    }
    let _ = shutdown_tx.send(true);
}

/// A result made for a QUIC connection that has since closed reaches the
/// client on its next connection. It is ignored - not a refusal, not a
/// connection - and the client authenticates afresh on the new one
/// (requirement 4 of review 108; the wiring, review 110). Without the stale
/// arm the client would verify that result against the binding it answered
/// with, find it good, and call itself connected on a connection the server
/// never admitted.
#[tokio::test]
async fn over_quic_a_result_for_a_closed_connection_is_ignored() {
    let (server, admitted) = spawn_quic_server_with("correct-pw", true, true, None);
    let (mut state_rx, cmd_tx, shutdown_tx) = spawn_quic_engine();
    cmd_tx
        .send(Command::Connect(format!("127.0.0.1:{}", server.port()), Some("correct-pw".into())))
        .unwrap();
    let status = wait_for_status(&mut state_rx, 8_000, |s| matches!(s, ConnectStatus::Connected | ConnectStatus::Failed(_))).await;
    let admissions = admitted.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(status, ConnectStatus::Connected, "not refused over a result for another connection");
    assert!(admissions >= 1, "connected only once the server admitted the new connection");
    let _ = shutdown_tx.send(true);
}

/// An engine that stops over QUIC gets its close to the server before it is
/// gone - also when its runtime goes right after, as the phone's does. The
/// server hears the close at once instead of after QUIC's idle timeout (30 s;
/// field test, build 111).
#[tokio::test]
async fn over_quic_a_stopping_engine_gets_its_close_out() {
    let (server, mut link) = sdr_remote_quic::QuicServer::start(
        "127.0.0.1:0".parse().unwrap(),
        sdr_remote_quic::Congestion::Bbr,
    )
    .unwrap();
    let port = server.local_addr().unwrap().port();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let engine_thread = std::thread::spawn(move || {
        // One thread, on purpose: with a second worker the client task could
        // still send the close after `run` returned, and this test went green
        // with the `stop()` call removed - 2 of 5 runs, and in a full run
        // (review 112). On one thread only that call can explain the close.
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let (engine, _state_rx, cmd_tx) = ClientEngine::new();
            cmd_tx.send(Command::Connect(format!("127.0.0.1:{port}"), Some("pw".into()))).unwrap();
            let _ = engine.run(mock_audio_factory, shutdown_rx).await;
            drop(cmd_tx);
        });
        // The runtime goes here, with whatever it was still doing.
    });
    let (tag, _) = tokio::time::timeout(Duration::from_secs(5), link.inbound_rx.recv())
        .await
        .expect("the engine knocks")
        .unwrap();
    let _ = shutdown_tx.send(true);
    loop {
        let (from, data) = tokio::time::timeout(Duration::from_secs(3), link.inbound_rx.recv())
            .await
            .expect("the close at once, not after the idle timeout")
            .unwrap();
        if from == tag && data.is_empty() {
            break;
        }
    }
    engine_thread.join().unwrap();
    drop(server);
}

/// Through the relay (phase 3b): the engine's QUIC connection goes through a
/// stand-in relay to the server, authenticates bound to that connection - the
/// relay carries the TLS session, it does not take part - and does so twice
/// without a restart (working rule 9). The operator connects to a label, as in
/// relay mode; the screen says "QUIC via relay".
#[tokio::test]
async fn over_quic_via_the_relay_the_client_authenticates() {
    use sdr_remote_quic::testing::FakeRelay;
    use sdr_remote_quic::RelayRoute;
    let relay = FakeRelay::start().await;
    let (_st, st_rx) = watch::channel(Some(relay.connect(true, 0)));
    let (_server, admitted) = spawn_quic_server_with("correct-pw", true, false, Some(RelayRoute::new(st_rx, None)));
    let (_ct, ct_rx) = watch::channel(Some(relay.connect(false, 0)));
    let (engine, mut state_rx, cmd_tx) = ClientEngine::new();
    let engine = engine.with_quic_relay(Some(ct_rx), None);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move { engine.run(mock_audio_factory, shutdown_rx).await });
    for round in 1..=2u32 {
        cmd_tx.send(Command::Connect("relay:ST".into(), Some("correct-pw".into()))).unwrap();
        wait_for_status(&mut state_rx, 8_000, |s| matches!(s, ConnectStatus::Connected)).await;
        wait_for_connected_flag(&mut state_rx).await;
        assert!(admitted.load(std::sync::atomic::Ordering::Relaxed) >= round, "round {round}: the server admitted the bound answer");
        assert_eq!(state_rx.borrow().route, sdr_remote_logic::measure::Route::QuicRelay, "round {round}: the screen says QUIC via relay");
        // The stand-in relay reports no public address, so nothing is punched:
        // all traffic through the relay (build 139).
        wait_for_path(&mut state_rx, Some(sdr_remote_core::path::PathKind::Relay), &format!("round {round}")).await;
        // And none once the session is gone: with a relay the route layer
        // always names a way, so only the engine's session check takes it
        // away (review 139, mutant F).
        cmd_tx.send(Command::Disconnect).unwrap();
        wait_for_status(&mut state_rx, 3_000, |s| !matches!(s, ConnectStatus::Connected)).await;
        no_path_stays(&state_rx, &format!("round {round}, after Disconnect")).await;
    }
    let _ = shutdown_tx.send(true);
}

/// A fake server that signs the client in and answers every heartbeat with
/// these flags; when `refuse_closed`, it also sends one unasked refusal saying
/// transmitting is closed, after the first heartbeat.
fn spawn_flag_server(fake_server: FakeServer, flags: ServerStateFlags, refuse_closed: bool) {
    tokio::spawn(async move {
        use sdr_remote_core::protocol::{Denial, DeniedTarget, Packet, PttDeniedPacket};
        let mut buf = [0u8; 1500];
        let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await else { return };
        let _ = fake_server.send_to(&build_auth_challenge(), client_addr).await;
        let Ok((n, _)) = fake_server.recv_auth_response(&mut buf).await else { return };
        let _ = fake_server
            .send_to(
                &build_proving_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED, &buf[..n], "correct-pw"),
                client_addr,
            )
            .await;
        let saying = build_heartbeat_ack_saying_with_flags("2.11.0 (build 93)", flags);
        let mut refused = false;
        loop {
            let Ok((len, _)) = fake_server.recv_from(&mut buf).await else { return };
            if matches!(Packet::deserialize(&buf[..len]), Ok(Packet::Heartbeat(_))) {
                let _ = fake_server.send_to(&saying, client_addr).await;
                if refuse_closed && !refused {
                    refused = true;
                    let mut pkt = [0u8; PttDeniedPacket::SIZE];
                    PttDeniedPacket::serialize(&mut pkt, DeniedTarget::Thetis, Denial::Closed);
                    let _ = fake_server.send_to(&pkt, client_addr).await;
                }
            }
        }
    });
}

/// Wait until a heartbeat ack of this session has been read: NO_DX_CLUSTER is
/// in every flag set these tests send, and a session starts with the cluster
/// available (`forget_session`).
async fn wait_for_an_ack_with_flags(state_rx: &mut watch::Receiver<RadioState>, what: &str) {
    let t0 = Instant::now();
    while state_rx.borrow().dx_cluster_available {
        assert!(t0.elapsed() < Duration::from_secs(5), "{what}: no ack with flags was read");
        let _ = tokio::time::timeout(Duration::from_millis(100), state_rx.changed()).await;
    }
}

/// The server's RX_ONLY_UNGUARDED flag reaches the screen (build 216; review
/// of 215: reading the flag with another constant stayed green). A heartbeat
/// with the flag gives the warning line; one with every other flag but that
/// one does not. Both on the same engine, with Disconnect and Connect between
/// them (review of 216): the second session must not keep the first one's line.
#[tokio::test]
async fn an_unguarded_rx_only_position_from_the_server_stands_on_the_screen() {
    use sdr_remote_logic::i18n::Lang;
    use sdr_remote_logic::station_notice::station_notice_lines;
    let warning = "Deze antennestand is alleen voor ontvangst";
    let others = ServerStateFlags::NONE
        .with(ServerStateFlags::TCI_CONNECTED)
        .with(ServerStateFlags::THETIS_RUNNING)
        .with(ServerStateFlags::THETIS_STARTING)
        .with(ServerStateFlags::THETIS_CONFIGURED)
        .with(ServerStateFlags::NO_DX_CLUSTER);
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();
    for (round, (flags, want)) in [(others.with(ServerStateFlags::RX_ONLY_UNGUARDED), true), (others, false)]
        .into_iter()
        .enumerate()
    {
        let what = format!("round {}, flags {:#x}", round + 1, flags.0);
        let fake_server = FakeServer::start().await;
        let server_addr = fake_server.local_addr().unwrap();
        spawn_flag_server(fake_server, flags, false);
        cmd_tx
            .send(Command::Connect(server_addr.to_string(), Some("correct-pw".to_string())))
            .unwrap();
        wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::Connected)).await;
        wait_for_connected_flag(&mut state_rx).await;
        wait_for_an_ack_with_flags(&mut state_rx, &what).await;
        let lines = station_notice_lines(&state_rx.borrow(), Lang::Nl);
        assert_eq!(lines.iter().any(|l| l.starts_with(warning)), want, "{what}: {lines:?}");
        cmd_tx.send(Command::Disconnect).unwrap();
        wait_for_status(&mut state_rx, 3_000, |s| !matches!(s, ConnectStatus::Connected)).await;
        // The session is forgotten, so the next round's wait is a real one.
        let t0 = Instant::now();
        while !state_rx.borrow().dx_cluster_available {
            assert!(t0.elapsed() < Duration::from_secs(3), "{what}: the session was not forgotten");
            let _ = tokio::time::timeout(Duration::from_millis(100), state_rx.changed()).await;
        }
        let lines = station_notice_lines(&state_rx.borrow(), Lang::Nl);
        assert!(!lines.iter().any(|l| l.starts_with(warning)), "{what}: after Disconnect: {lines:?}");
    }
    let _ = shutdown_tx.send(true);
}

/// A refusal saying transmitting is closed reaches the screen (build 217;
/// review of 216: only the rule was tested, not the way from the packet). The
/// server sends it unasked, as `safety_task_lost` does; the line "safe mode"
/// stands. A second session on the same engine, with a server that is open,
/// does not show it.
#[tokio::test]
async fn a_server_closed_refusal_stands_on_the_screen() {
    use sdr_remote_logic::i18n::Lang;
    use sdr_remote_logic::station_notice::station_notice_lines;
    let closed = "Server in veilige stand";
    let flags = ServerStateFlags::NONE.with(ServerStateFlags::NO_DX_CLUSTER);
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();
    for (round, refuse_closed) in [true, false].into_iter().enumerate() {
        let what = format!("round {}, closed {refuse_closed}", round + 1);
        let fake_server = FakeServer::start().await;
        let server_addr = fake_server.local_addr().unwrap();
        spawn_flag_server(fake_server, flags, refuse_closed);
        cmd_tx
            .send(Command::Connect(server_addr.to_string(), Some("correct-pw".to_string())))
            .unwrap();
        wait_for_status(&mut state_rx, 4_000, |s| matches!(s, ConnectStatus::Connected)).await;
        wait_for_connected_flag(&mut state_rx).await;
        wait_for_an_ack_with_flags(&mut state_rx, &what).await;
        // The refusal follows the ack on the same heartbeat; give it the time
        // of a few heartbeats when it is sent, and the same time when it is not.
        let t0 = Instant::now();
        let mut shown = false;
        while t0.elapsed() < Duration::from_millis(1_500) {
            shown = station_notice_lines(&state_rx.borrow(), Lang::Nl).iter().any(|l| l.starts_with(closed));
            if shown {
                break;
            }
            let _ = tokio::time::timeout(Duration::from_millis(100), state_rx.changed()).await;
        }
        assert_eq!(shown, refuse_closed, "{what}");
        cmd_tx.send(Command::Disconnect).unwrap();
        wait_for_status(&mut state_rx, 3_000, |s| !matches!(s, ConnectStatus::Connected)).await;
        let t0 = Instant::now();
        while !state_rx.borrow().dx_cluster_available {
            assert!(t0.elapsed() < Duration::from_secs(3), "{what}: the session was not forgotten");
            let _ = tokio::time::timeout(Duration::from_millis(100), state_rx.changed()).await;
        }
        assert!(
            !station_notice_lines(&state_rx.borrow(), Lang::Nl).iter().any(|l| l.starts_with(closed)),
            "{what}: a closed server is the session's, not the engine's"
        );
    }
    let _ = shutdown_tx.send(true);
}

/// The engine watches its relay grant (build 137): signed in, but the relay
/// never says it sees our UDP - after `UDP_SEEN_WAIT` the state says UDP is
/// blocked, and the station notice stands; `udp-seen` arriving takes it away.
/// The branch sits in the engine loop; this drives it end to end.
#[tokio::test]
async fn the_engine_says_when_the_relay_sees_no_udp() {
    use sdr_remote_logic::udp_blocked::UDP_SEEN_WAIT;
    use sdr_remote_quic::testing::FakeRelay;
    let relay = FakeRelay::start().await;
    let grant = relay.connect(false, 0);
    assert!(grant.public.is_none(), "the stand-in relay reports no udp-seen unless asked to");
    let (gt, g_rx) = watch::channel(Some(grant));
    let (engine, mut state_rx, _cmd_tx) = ClientEngine::new();
    let engine = engine.with_quic_relay(Some(g_rx), None);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move { engine.run(mock_audio_factory, shutdown_rx).await });
    tokio::time::sleep(UDP_SEEN_WAIT - Duration::from_secs(2)).await;
    assert!(!state_rx.borrow().udp_blocked, "not before the wait");
    let t0 = Instant::now();
    while !state_rx.borrow().udp_blocked {
        assert!(t0.elapsed() < Duration::from_secs(5), "blocked UDP never said");
        let _ = tokio::time::timeout(Duration::from_millis(200), state_rx.changed()).await;
    }
    assert!(
        sdr_remote_logic::station_notice::station_notice_lines(&state_rx.borrow(), sdr_remote_logic::i18n::Lang::En)
            .iter()
            .any(|l| l.contains("no UDP")),
        "and the notice stands"
    );
    gt.send(Some(sdr_remote_quic::UdpGrant { public: Some("198.51.100.7:40000".parse().unwrap()), ..grant })).unwrap();
    let t1 = Instant::now();
    while state_rx.borrow().udp_blocked {
        assert!(t1.elapsed() < Duration::from_secs(2), "udp-seen did not take the notice away");
        let _ = tokio::time::timeout(Duration::from_millis(100), state_rx.changed()).await;
    }
    let _ = shutdown_tx.send(true);
}

/// "Relay only" (build 137) and "force TCP" (build 146) from the measurement
/// screen: with a relay the state follows each switch both ways; without one
/// there is nothing to switch and the state stays off.
#[tokio::test]
async fn relay_only_follows_the_switch_only_with_a_relay() {
    use sdr_remote_quic::testing::FakeRelay;
    let relay = FakeRelay::start().await;
    let (_gt, g_rx) = watch::channel(Some(relay.connect(false, 0)));
    let (engine, state_rx, cmd_tx) = ClientEngine::new();
    let engine = engine.with_quic_relay(Some(g_rx), None);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move { engine.run(mock_audio_factory, shutdown_rx).await });
    for on in [true, false, true] {
        cmd_tx.send(Command::SetRelayOnly(on)).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(state_rx.borrow().relay_only, on, "with a relay: {on}");
        cmd_tx.send(Command::SetForceTcp(on)).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(state_rx.borrow().force_tcp, on, "force TCP with a relay: {on}");
    }
    let _ = shutdown_tx.send(true);
    let (state_rx, cmd_tx, shutdown_tx, _h) = spawn_engine();
    cmd_tx.send(Command::SetRelayOnly(true)).unwrap();
    cmd_tx.send(Command::SetForceTcp(true)).unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!state_rx.borrow().relay_only, "without a relay nothing is switched");
    assert!(!state_rx.borrow().force_tcp, "nor is force TCP");
    let _ = shutdown_tx.send(true);
}

/// A server whose proof leaves the binding out - what a party in the middle,
/// with its own TLS session, would have to send - is refused.
#[tokio::test]
async fn over_quic_a_proof_without_the_binding_is_refused() {
    let (server, _) = spawn_quic_server("correct-pw", false);
    let (mut state_rx, cmd_tx, shutdown_tx) = spawn_quic_engine();
    cmd_tx
        .send(Command::Connect(format!("127.0.0.1:{}", server.port()), Some("correct-pw".into())))
        .unwrap();
    let status = wait_for_status(&mut state_rx, 5_000, |s| matches!(s, ConnectStatus::Failed(_))).await;
    assert_eq!(status, ConnectStatus::Failed(ConnectError::ServerUnverified), "refused because the proof does not verify");
    assert!(!state_rx.borrow().connected);
    let _ = shutdown_tx.send(true);
}
