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

use anyhow::Result;
use sdr_remote_core::protocol::{
    Capabilities, Flags, Header, HeartbeatAck, PacketType, ServerStateFlags, MAGIC, VERSION,
};
use sdr_remote_logic::audio::AudioBackend;
use sdr_remote_logic::commands::Command;
use sdr_remote_logic::engine::ClientEngine;
use sdr_remote_logic::state::{ConnectError, ConnectStatus, RadioState};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
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

/// Spawn an engine task and return handles. Caller is responsible for
/// `shutdown_tx.send(true)` and `join` cleanup.
fn spawn_engine() -> (
    watch::Receiver<RadioState>,
    mpsc::UnboundedSender<Command>,
    watch::Sender<bool>,
    tokio::task::JoinHandle<Result<()>>,
) {
    let (engine, state_rx, cmd_tx) = ClientEngine::new();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(async move { engine.run(mock_audio_factory, shutdown_rx, None).await });
    (state_rx, cmd_tx, shutdown_tx, handle)
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

/// Build an AuthChallenge packet (4-byte header + 16-byte nonce = 20 bytes).
fn build_auth_challenge() -> Vec<u8> {
    let mut buf = vec![0u8; 20];
    let header = Header::new(PacketType::AuthChallenge, Flags::NONE);
    let mut hdr = [0u8; 4];
    header.serialize(&mut hdr);
    buf[..4].copy_from_slice(&hdr);
    // nonce: 16 zero bytes is fine for state-machine tests
    buf
}

/// Build an AuthResult packet (4-byte header + 1-byte result).
fn build_auth_result(result_code: u8) -> Vec<u8> {
    let mut buf = vec![0u8; 5];
    let header = Header::new(PacketType::AuthResult, Flags::NONE);
    let mut hdr = [0u8; 4];
    header.serialize(&mut hdr);
    buf[..4].copy_from_slice(&hdr);
    buf[4] = result_code;
    buf
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
    };
    let mut buf = [0u8; HeartbeatAck::SIZE];
    ack.serialize(&mut buf);
    buf.to_vec()
}

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

    // Bind a UDP socket and immediately drop the listener thread so the
    // address resolves but never replies.
    let fake_server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = fake_server.local_addr().unwrap();
    drop(fake_server); // free the port — guarantee no response

    cmd_tx
        .send(Command::Connect(
            addr.to_string(),
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
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    // Fake-server replies with raw garbage bytes (no magic).
    let fake_server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server_addr = fake_server.local_addr().unwrap();

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
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    let fake_server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server_addr = fake_server.local_addr().unwrap();

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
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    let fake_server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server_addr = fake_server.local_addr().unwrap();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        // 1. recv first heartbeat → send AuthChallenge
        if let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await {
            let _ = fake_server
                .send_to(&build_auth_challenge(), client_addr)
                .await;
            // 2. recv AuthResponse → send AUTH_REJECTED
            if let Ok((_n, _client_addr)) = fake_server.recv_from(&mut buf).await {
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

#[tokio::test]
async fn wrong_totp_after_totp_phase() {
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    let fake_server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server_addr = fake_server.local_addr().unwrap();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        // Heartbeat → AuthChallenge
        if let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await {
            let _ = fake_server
                .send_to(&build_auth_challenge(), client_addr)
                .await;
            // AuthResponse → AUTH_TOTP_REQUIRED
            if let Ok((_n, _)) = fake_server.recv_from(&mut buf).await {
                let _ = fake_server
                    .send_to(
                        &build_auth_result(sdr_remote_core::protocol::AUTH_TOTP_REQUIRED),
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

    let _ = shutdown_tx.send(true);
}

#[tokio::test]
async fn tci_unreachable_via_state_flags() {
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    let fake_server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server_addr = fake_server.local_addr().unwrap();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        // Full auth handshake → HeartbeatAck with REPORTS_STATE_FLAGS but NO TCI_CONNECTED
        if let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await {
            let _ = fake_server
                .send_to(&build_auth_challenge(), client_addr)
                .await;
            if let Ok((_n, _)) = fake_server.recv_from(&mut buf).await {
                let _ = fake_server
                    .send_to(
                        &build_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED),
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
async fn old_v2_0_0_server_no_false_positive_tci_unreachable() {
    // Compatibility regression test (review finding B3): a v2.0.0-release-tag server replies
    // with a 16-byte HeartbeatAck (no state_flags, no REPORTS_STATE_FLAGS cap).
    // The client MUST stay Connected and NOT misread the absent flag as
    // "TCI down".
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    let fake_server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server_addr = fake_server.local_addr().unwrap();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        if let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await {
            let _ = fake_server
                .send_to(&build_auth_challenge(), client_addr)
                .await;
            if let Ok((_n, _)) = fake_server.recv_from(&mut buf).await {
                let _ = fake_server
                    .send_to(
                        &build_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED),
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
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    let fake_server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server_addr = fake_server.local_addr().unwrap();
    let tci_up = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let tci_up_clone = tci_up.clone();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        if let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await {
            let _ = fake_server
                .send_to(&build_auth_challenge(), client_addr)
                .await;
            if let Ok((_n, _)) = fake_server.recv_from(&mut buf).await {
                let _ = fake_server
                    .send_to(
                        &build_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED),
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
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    // Stage 1: reach Failed(WrongPassword) via a fake server.
    let fake_server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server_addr = fake_server.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        if let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await {
            let _ = fake_server
                .send_to(&build_auth_challenge(), client_addr)
                .await;
            if let Ok((_n, _)) = fake_server.recv_from(&mut buf).await {
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
    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    let fake_server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server_addr = fake_server.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        if let Ok((_n, client_addr)) = fake_server.recv_from(&mut buf).await {
            let _ = fake_server
                .send_to(&build_auth_challenge(), client_addr)
                .await;
            if let Ok((_n, _)) = fake_server.recv_from(&mut buf).await {
                let _ = fake_server
                    .send_to(
                        &build_auth_result(sdr_remote_core::protocol::AUTH_TOTP_REQUIRED),
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

    let (mut state_rx, cmd_tx, shutdown_tx, _handle) = spawn_engine();

    let fake_server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server_addr = fake_server.local_addr().unwrap();

    let (found_tx, mut found_rx) = mpsc::unbounded_channel::<bool>();

    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        let mut client: Option<std::net::SocketAddr> = None;

        // Round one: challenge, accept, and one ack so the client settles into
        // "connected" and runs its first restore.
        if let Ok((_n, addr)) = fake_server.recv_from(&mut buf).await {
            client = Some(addr);
            let _ = fake_server.send_to(&build_auth_challenge(), addr).await;
            if fake_server.recv_from(&mut buf).await.is_ok() {
                let _ = fake_server
                    .send_to(
                        &build_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED),
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
        if fake_server.recv_from(&mut buf).await.is_ok() {
            let _ = fake_server
                .send_to(
                    &build_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED),
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
