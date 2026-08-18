// SPDX-License-Identifier: GPL-2.0-or-later
//
// Lane 3, phase 0: pin the audio measurement points before anything is
// refactored.
//
// Why this exists. The brief asks whoever unifies the channel blocks to prove
// that behaviour did not change, and for the audio path that proof did not
// exist - it would have been "I read the diff and it looks the same". Three
// times this week that reasoning was wrong and only a measurement settled it.
//
// What is pinned here is not a dB value but a set of PROPERTIES, because those
// are what actually broke:
//
//   * a receive level must not move when the operator changes volume
//     (build 58: it did, so a quiet stream looked dead)
//   * two channels fed identical audio must report identical levels
//     (builds 79-86: they did not, and the difference was elsewhere entirely)
//
// Strategy is the one `connect_state_machine.rs` already uses: run the real
// engine against a scripted server on loopback, with a fake audio backend. The
// backend here records what the engine hands it, so the mix can be observed
// without a sound card.

use anyhow::Result;
use sdr_remote_core::codec::OpusEncoder;
use sdr_remote_core::protocol::{
    MultiChannelAudioPacket,
    AudioPacket, Capabilities, Flags, Header, HeartbeatAck, PacketType, ServerStateFlags,
    VrxAudioPacket, YaesuPresencePacket,
};
use sdr_remote_logic::audio::AudioBackend;
use sdr_remote_logic::commands::Command;
use sdr_remote_logic::engine::ClientEngine;
use sdr_remote_logic::state::RadioState;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};

// ---------------- recording audio backend ----------------

/// Audio backend that keeps what was written to playback, so a test can look at
/// the mix. The engine only requires that `write_playback` accepts everything;
/// nothing here influences timing.
#[derive(Clone, Default)]
struct Recorded {
    playback: Arc<Mutex<Vec<f32>>>,
}

struct RecordingAudio {
    rec: Recorded,
    /// Steady sample value handed to the engine as microphone input, so the TX
    /// chain has something with a known amplitude to work on.
    capture_amplitude: f32,
}

impl AudioBackend for RecordingAudio {
    fn read_capture(&mut self, buf: &mut [f32]) -> usize {
        if self.capture_amplitude == 0.0 {
            return 0;
        }
        for (i, s) in buf.iter_mut().enumerate() {
            let t = i as f32 / 48_000.0;
            *s = (t * 1000.0 * std::f32::consts::TAU).sin() * self.capture_amplitude;
        }
        buf.len()
    }
    fn write_playback(&mut self, buf: &[f32]) -> usize {
        self.rec.playback.lock().unwrap().extend_from_slice(buf);
        buf.len()
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

fn spawn_engine(rec: Recorded) -> (
    watch::Receiver<RadioState>,
    mpsc::UnboundedSender<Command>,
    watch::Sender<bool>,
) {
    spawn_engine_with_mic(rec, 0.0)
}

fn spawn_engine_with_mic(rec: Recorded, capture_amplitude: f32) -> (
    watch::Receiver<RadioState>,
    mpsc::UnboundedSender<Command>,
    watch::Sender<bool>,
) {
    let (engine, state_rx, cmd_tx) = ClientEngine::new();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let factory = move |_i: Option<&str>, _o: Option<&str>| -> Result<Box<dyn AudioBackend>> {
        Ok(Box::new(RecordingAudio { rec: rec.clone(), capture_amplitude }))
    };
    tokio::spawn(async move { engine.run(factory, shutdown_rx, None).await });
    (state_rx, cmd_tx, shutdown_tx)
}

// ---------------- scripted server ----------------

fn header(t: PacketType) -> [u8; 4] {
    let mut hdr = [0u8; 4];
    Header::new(t, Flags::NONE).serialize(&mut hdr);
    hdr
}

fn build_auth_challenge() -> Vec<u8> {
    let mut buf = vec![0u8; 20];
    buf[..4].copy_from_slice(&header(PacketType::AuthChallenge));
    buf
}

fn build_auth_result(code: u8) -> Vec<u8> {
    let mut buf = vec![0u8; 5];
    buf[..4].copy_from_slice(&header(PacketType::AuthResult));
    buf[4] = code;
    buf
}

fn build_heartbeat_ack() -> Vec<u8> {
    let ack = HeartbeatAck {
        flags: Flags::NONE,
        echo_sequence: 0,
        echo_time: 0,
        capabilities: Capabilities::NONE.with(Capabilities::REPORTS_STATE_FLAGS),
        state_flags: ServerStateFlags::NONE.with(
            ServerStateFlags::THETIS_CONFIGURED | ServerStateFlags::TCI_CONNECTED,
        ),
        subs: None,
    };
    let mut buf = [0u8; HeartbeatAck::SIZE];
    ack.serialize(&mut buf);
    buf.to_vec()
}

/// One 20 ms frame of a steady tone at `amplitude`, Opus-encoded at 8 kHz.
/// Steady on purpose: a level meter reading a constant signal is easy to reason
/// about, and Opus reproduces it closely enough for a ratio comparison.
fn opus_frame(enc: &mut OpusEncoder, amplitude: f32) -> Vec<u8> {
    let pcm: Vec<i16> = (0..sdr_remote_core::FRAME_SAMPLES)
        .map(|i| {
            let t = i as f32 / sdr_remote_core::NETWORK_SAMPLE_RATE as f32;
            ((t * 1000.0 * std::f32::consts::TAU).sin() * amplitude * 32767.0) as i16
        })
        .collect();
    enc.encode(&pcm).expect("opus encode")
}

fn build_vrx_audio(vrx_id: u8, sequence: u32, opus: Vec<u8>) -> Vec<u8> {
    let pkt = VrxAudioPacket {
        sequence,
        timestamp: sequence * 20,
        vrx_id,
        opus_data: opus,
        wideband: false,
    };
    let mut buf = Vec::new();
    pkt.serialize(&mut buf);
    buf
}

/// RX1, BinR and RX2 do not arrive as separate packet types on the playback
/// path - they ride one multi-channel blob, keyed by channel id (0 = RX1,
/// 1 = BinR, 2 = RX2). A test that sends `PacketType::AudioRx2` is decoded by
/// nobody and reads as "channel never produced a level".
fn build_multich_audio(channels: Vec<(u8, Vec<u8>)>, sequence: u32) -> Vec<u8> {
    let pkt = MultiChannelAudioPacket {
        sequence,
        timestamp: sequence * 20,
        channels,
        flags: Flags::NONE,
    };
    let mut buf = Vec::new();
    pkt.serialize(&mut buf);
    buf
}

fn build_yaesu_audio(slot1: bool, sequence: u32, opus: Vec<u8>) -> Vec<u8> {
    let pkt = AudioPacket {
        flags: Flags::NONE,
        sequence,
        timestamp: sequence * 20,
        opus_data: opus,
    };
    let ty = if slot1 { PacketType::AudioYaesu2 } else { PacketType::AudioYaesu };
    // serialize_as_type APPENDS, like the VRX builder above - starting from a
    // pre-sized buffer would put the packet behind a run of zero bytes.
    let mut buf = Vec::new();
    pkt.serialize_as_type(&mut buf, ty);
    buf
}

/// Tell the client which radio sits in each slot. The receive meters calibrate
/// per model (build 75/76: the two CODECs differ by about 14 dB), so a test
/// about that calibration has to set the models first.
fn build_yaesu_presence(slot0_model: u8, slot1_model: u8) -> Vec<u8> {
    let pkt = YaesuPresencePacket {
        slot0_present: true,
        slot0_model,
        slot1_present: true,
        slot1_model,
        slot0_trouble: 0,
        slot1_trouble: 0,
    };
    let mut buf = [0u8; YaesuPresencePacket::SIZE];
    pkt.serialize(&mut buf);
    buf.to_vec()
}

/// Bring the engine to Connected against a scripted server, then hand the
/// caller the socket + client address so it can push audio.
async fn connect(
    cmd_tx: &mpsc::UnboundedSender<Command>,
    state_rx: &mut watch::Receiver<RadioState>,
) -> (UdpSocket, std::net::SocketAddr) {
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server_addr = server.local_addr().unwrap();
    cmd_tx
        .send(Command::Connect(server_addr.to_string(), Some("pw".into())))
        .unwrap();

    let mut buf = [0u8; 2048];
    let (_n, client) = server.recv_from(&mut buf).await.unwrap();
    server.send_to(&build_auth_challenge(), client).await.unwrap();
    let (_n, _) = server.recv_from(&mut buf).await.unwrap();
    server
        .send_to(
            &build_auth_result(sdr_remote_core::protocol::AUTH_ACCEPTED),
            client,
        )
        .await
        .unwrap();
    // One ack so the engine reaches Connected; heartbeats after this are
    // answered opportunistically by the tests via `pump`.
    let (_n, _) = server.recv_from(&mut buf).await.unwrap();
    server.send_to(&build_heartbeat_ack(), client).await.unwrap();

    let start = Instant::now();
    while !state_rx.borrow().connected {
        assert!(start.elapsed() < Duration::from_secs(5), "engine never connected");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (server, client)
}

/// Wait until `f` reports a level above zero, answering heartbeats meanwhile so
/// the connection is not dropped underneath the test.
async fn wait_for_level(
    state_rx: &watch::Receiver<RadioState>,
    server: &UdpSocket,
    client: std::net::SocketAddr,
    f: impl Fn(&RadioState) -> f32,
) -> f32 {
    let start = Instant::now();
    let mut buf = [0u8; 2048];
    loop {
        let lvl = f(&state_rx.borrow());
        if lvl > 0.0 {
            return lvl;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "level stayed at zero"
        );
        // Drain and answer whatever the engine sent, without blocking.
        if tokio::time::timeout(Duration::from_millis(10), server.recv_from(&mut buf))
            .await
            .is_ok()
        {
            let _ = server.send_to(&build_heartbeat_ack(), client).await;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The counterpart of `wait_for_level`: wait until the level has returned to
/// zero after the stream stopped. Returns false on timeout, so the caller can
/// report WHICH channel stayed frozen instead of just failing.
async fn wait_for_zero(
    state_rx: &watch::Receiver<RadioState>,
    server: &UdpSocket,
    client: std::net::SocketAddr,
    f: impl Fn(&RadioState) -> f32,
) -> bool {
    let start = Instant::now();
    let mut buf = [0u8; 2048];
    // Generous: the jitter buffer first plays out what it still holds, and only
    // then does the meter start falling. The frozen case never falls at all, so
    // a wide window costs nothing in precision.
    while start.elapsed() < Duration::from_secs(3) {
        if f(&state_rx.borrow()) == 0.0 {
            return true;
        }
        if tokio::time::timeout(Duration::from_millis(10), server.recv_from(&mut buf))
            .await
            .is_ok()
        {
            let _ = server.send_to(&build_heartbeat_ack(), client).await;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

// ---------------- the pinned properties ----------------

#[tokio::test]
async fn vrx_level_does_not_follow_the_volume_slider() {
    // Build 58 moved the receive meters to before the volume, because a stream
    // that was simply turned down read as a dead link. Pinning the property, not
    // the number: the same audio at two very different volumes must report the
    // same level.
    let mut enc = OpusEncoder::new().unwrap();
    let frames: Vec<Vec<u8>> = (0..25).map(|_| opus_frame(&mut enc, 0.5)).collect();

    let mut levels = Vec::new();
    for volume in [1.0_f32, 0.05] {
        let rec = Recorded::default();
        let (mut state_rx, cmd_tx, shutdown_tx) = spawn_engine(rec);
        let (server, client) = connect(&cmd_tx, &mut state_rx).await;

        cmd_tx.send(Command::SetVrxVolume(volume)).unwrap();
        cmd_tx.send(Command::SetVrxEnabled(true)).unwrap();
        for (i, f) in frames.iter().enumerate() {
            let _ = server
                .send_to(&build_vrx_audio(0, i as u32, f.clone()), client)
                .await;
        }
        levels.push(
            wait_for_level(&state_rx, &server, client, |s| s.playback_level_vrx1).await,
        );
        let _ = shutdown_tx.send(true);
    }

    let (loud, quiet) = (levels[0], levels[1]);
    let ratio = quiet / loud;
    assert!(
        (0.5..2.0).contains(&ratio),
        "receive level must be measured before volume: full volume {loud:.4}, \
         volume 0.05 {quiet:.4} (ratio {ratio:.2}); a ratio near 0.05 means the \
         meter moved with the slider again"
    );
}

#[tokio::test]
async fn both_vrx_channels_report_the_same_level_for_the_same_audio() {
    // The question that cost most of a day: "why does channel 1 sound different
    // from channel 2" - while every setting was identical and the difference was
    // elsewhere. With this pinned, that question is answered by a test run
    // instead of by an evening of listening.
    let mut enc = OpusEncoder::new().unwrap();
    let frames: Vec<Vec<u8>> = (0..25).map(|_| opus_frame(&mut enc, 0.5)).collect();

    let rec = Recorded::default();
    let (mut state_rx, cmd_tx, shutdown_tx) = spawn_engine(rec);
    let (server, client) = connect(&cmd_tx, &mut state_rx).await;

    for cmd in [
        Command::SetVrxVolume(1.0),
        Command::SetVrx2Volume(1.0),
        Command::SetVrxEnabled(true),
        Command::SetVrx2Enabled(true),
    ] {
        cmd_tx.send(cmd).unwrap();
    }
    for (i, f) in frames.iter().enumerate() {
        let _ = server
            .send_to(&build_vrx_audio(0, i as u32, f.clone()), client)
            .await;
        let _ = server
            .send_to(&build_vrx_audio(1, i as u32, f.clone()), client)
            .await;
    }

    let l1 = wait_for_level(&state_rx, &server, client, |s| s.playback_level_vrx1).await;
    let l2 = wait_for_level(&state_rx, &server, client, |s| s.playback_level_vrx2).await;
    let _ = shutdown_tx.send(true);

    let ratio = l2 / l1;
    assert!(
        (0.7..1.4).contains(&ratio),
        "identical audio must read identically on both VRX channels: \
         VRX1 {l1:.4}, VRX2 {l2:.4} (ratio {ratio:.2})"
    );
}

#[tokio::test]
async fn the_two_yaesu_radios_are_calibrated_apart_on_purpose() {
    // Build 75/76: one calibration constant for "the Yaesu" did not hold - the
    // FT-991A and the FTX-1 deliver line levels about 14 dB apart, so the meter
    // corrects per MODEL, not per slot. That makes this the one place where two
    // channels must NOT read alike on identical audio, and where a well-meant
    // unification would silently undo an operator-measured correction.
    let mut enc = OpusEncoder::new().unwrap();
    let frames: Vec<Vec<u8>> = (0..25).map(|_| opus_frame(&mut enc, 0.5)).collect();

    let rec = Recorded::default();
    let (mut state_rx, cmd_tx, shutdown_tx) = spawn_engine(rec);
    let (server, client) = connect(&cmd_tx, &mut state_rx).await;

    // Slot 0 = FT-991A (model 0), slot 1 = FTX-1 (model 1).
    let _ = server.send_to(&build_yaesu_presence(0, 1), client).await;
    for cmd in [Command::SetYaesuVolume(1.0), Command::SetYaesu2Volume(1.0)] {
        cmd_tx.send(cmd).unwrap();
    }
    for (i, f) in frames.iter().enumerate() {
        let _ = server
            .send_to(&build_yaesu_audio(false, i as u32, f.clone()), client)
            .await;
        let _ = server
            .send_to(&build_yaesu_audio(true, i as u32, f.clone()), client)
            .await;
    }

    let l1 = wait_for_level(&state_rx, &server, client, |s| s.playback_level_yaesu).await;
    let l2 = wait_for_level(&state_rx, &server, client, |s| s.playback_level_yaesu2).await;
    let _ = shutdown_tx.send(true);

    // Documented constants: +10 dB for the 991A, +24.5 dB for the FTX-1.
    let expected = 16.8_f32 / 3.16;
    let ratio = l2 / l1;
    assert!(
        (ratio / expected).abs() > 0.7 && (ratio / expected).abs() < 1.4,
        "the FTX-1 meter is calibrated {expected:.2}x above the 991A on identical \
         audio: 991A {l1:.4}, FTX-1 {l2:.4} (ratio {ratio:.2}). A ratio near 1.0 \
         means the per-model calibration was collapsed into one constant again"
    );
}

#[tokio::test]
async fn the_transmit_meter_follows_the_gain_it_is_measured_after() {
    // Build 65: the TX meters read the frame as it is ENCODED - after AGC and
    // the gain trim - because on transmit the question is headroom, not average
    // level. Pinning that with the property that survives a refactor: doubling
    // tx_gain must show up in the meter. Measured before the gain, it would not.
    let mut levels = Vec::new();
    for gain in [0.25_f32, 0.75] {
        let rec = Recorded::default();
        let (mut state_rx, cmd_tx, shutdown_tx) = spawn_engine_with_mic(rec, 0.3);
        let (server, client) = connect(&cmd_tx, &mut state_rx).await;

        cmd_tx.send(Command::SetAgcEnabled(false)).unwrap();
        cmd_tx.send(Command::SetTxGain(gain)).unwrap();
        cmd_tx.send(Command::SetPtt(true)).unwrap();

        levels.push(wait_for_level(&state_rx, &server, client, |s| s.capture_level).await);
        cmd_tx.send(Command::SetPtt(false)).unwrap();
        let _ = shutdown_tx.send(true);
    }

    let (low, high) = (levels[0], levels[1]);
    let ratio = high / low;
    assert!(
        (2.0..4.5).contains(&ratio),
        "the transmit meter sits after the gain, so tripling tx_gain must show: \
         gain 0.25 -> {low:.4}, gain 0.75 -> {high:.4} (ratio {ratio:.2}). A ratio \
         near 1.0 means it moved back to before the gain"
    );
}

#[tokio::test]
async fn a_channel_that_stops_feeding_returns_to_zero() {
    // The four properties above all describe how HIGH the bar must stand while
    // audio is arriving. None of them describes what it must do when the audio
    // stops - which is why three different rules could live side by side unseen:
    // RX and Yaesu froze on their last value forever, VRX decayed to zero, BinR
    // snapped to zero.
    //
    // Freezing is the one that lies. The level bar is the instrument used to see
    // whether a stream is arriving at all (it is how the VRX start-up problem was
    // finally pinned down), and on a frozen channel it keeps claiming signal long
    // after the link went quiet.
    let mut enc = OpusEncoder::new().unwrap();
    let frames: Vec<Vec<u8>> = (0..25).map(|_| opus_frame(&mut enc, 0.5)).collect();

    // All six, deliberately. An earlier version of this test checked three, and
    // the one channel it happened to skip - Yaesu slot 1 - was exactly the one
    // that still froze. A per-channel rule has to be checked per channel.
    let mut frozen: Vec<String> = Vec::new();
    for channel in ["RX1", "RX2", "VRX1", "VRX2", "Yaesu 1", "Yaesu 2"] {
        let rec = Recorded::default();
        let (mut state_rx, cmd_tx, shutdown_tx) = spawn_engine(rec);
        let (server, client) = connect(&cmd_tx, &mut state_rx).await;

        match channel {
            "RX1" => cmd_tx.send(Command::SetRx1Enabled(true)).unwrap(),
            "RX2" => cmd_tx.send(Command::SetRx2Enabled(true)).unwrap(),
            "VRX1" => cmd_tx.send(Command::SetVrxEnabled(true)).unwrap(),
            "VRX2" => cmd_tx.send(Command::SetVrx2Enabled(true)).unwrap(),
            _ => {
                // Slot 0 = FT-991A, slot 1 = FTX-1; the meter calibrates per model.
                let _ = server.send_to(&build_yaesu_presence(0, 1), client).await;
                cmd_tx.send(Command::SetYaesuVolume(1.0)).unwrap();
                cmd_tx.send(Command::SetYaesu2Volume(1.0)).unwrap();
            }
        }

        for (i, f) in frames.iter().enumerate() {
            let pkt = match channel {
                "RX1" => build_multich_audio(vec![(0, f.clone())], i as u32),
                "RX2" => build_multich_audio(vec![(2, f.clone())], i as u32),
                "VRX1" => build_vrx_audio(0, i as u32, f.clone()),
                "VRX2" => build_vrx_audio(1, i as u32, f.clone()),
                "Yaesu 1" => build_yaesu_audio(false, i as u32, f.clone()),
                _ => build_yaesu_audio(true, i as u32, f.clone()),
            };
            let _ = server.send_to(&pkt, client).await;
        }

        let pick = |s: &RadioState| match channel {
            "RX1" => s.playback_level,
            "RX2" => s.playback_level_rx2,
            "VRX1" => s.playback_level_vrx1,
            "VRX2" => s.playback_level_vrx2,
            "Yaesu 1" => s.playback_level_yaesu,
            _ => s.playback_level_yaesu2,
        };

        let peak = wait_for_level(&state_rx, &server, client, pick).await;
        assert!(peak > 0.0, "{channel} never registered a level to begin with");

        // Nothing more is sent from here: the stream has dried up.
        if !wait_for_zero(&state_rx, &server, client, pick).await {
            frozen.push(format!("{channel} (stuck at {peak:.4})"));
        }
        let _ = shutdown_tx.send(true);
    }

    assert!(
        frozen.is_empty(),
        "every channel must let its level bar fall back to zero once its stream \
         stops; these kept showing signal that is no longer arriving: {}",
        frozen.join(", ")
    );
}

#[tokio::test]
async fn the_yaesu_playback_gain_follows_the_same_per_model_calibration_as_the_meter() {
    // The meter has calibrated per model since build 75/76; the playback gain
    // kept one flat constant for both radios until build 106. The result was a
    // 991A running about 16 dB hot - inaudible at normal settings, obvious at the
    // bottom of the volume slider where every other channel had gone silent.
    //
    // Identical audio into both slots must therefore come out of the mix with the
    // SAME ratio the meter uses. One radio per run, because both mix into one
    // playback buffer.
    let mut enc = OpusEncoder::new().unwrap();
    let frames: Vec<Vec<u8>> = (0..25).map(|_| opus_frame(&mut enc, 0.5)).collect();

    let mut rms = Vec::new();
    for slot1 in [false, true] {
        let rec = Recorded::default();
        let (mut state_rx, cmd_tx, shutdown_tx) = spawn_engine(rec.clone());
        let (server, client) = connect(&cmd_tx, &mut state_rx).await;

        // Slot 0 = FT-991A (model 0), slot 1 = FTX-1 (model 1).
        let _ = server.send_to(&build_yaesu_presence(0, 1), client).await;
        // Low volume on purpose: at full volume the FTX-1 gain (16.8) drives the
        // 0.5-amplitude tone far past full scale, both radios clip, and the ratio
        // collapses to ~1 - which would make this test pass on saturation instead
        // of on calibration.
        cmd_tx.send(Command::SetYaesuVolume(0.03)).unwrap();
        cmd_tx.send(Command::SetYaesu2Volume(0.03)).unwrap();
        for (i, f) in frames.iter().enumerate() {
            let _ = server
                .send_to(&build_yaesu_audio(slot1, i as u32, f.clone()), client)
                .await;
        }
        let pick = |s: &RadioState| if slot1 { s.playback_level_yaesu2 } else { s.playback_level_yaesu };
        let _ = wait_for_level(&state_rx, &server, client, pick).await;

        let played = rec.playback.lock().unwrap().clone();
        let energy: f32 = played.iter().map(|s| s * s).sum();
        rms.push((energy / played.len().max(1) as f32).sqrt());
        let _ = shutdown_tx.send(true);
    }

    let (a991, ftx1) = (rms[0], rms[1]);
    assert!(a991 > 0.0 && ftx1 > 0.0, "both radios must have produced audio");
    let expected = 16.8_f32 / 3.16;
    let ratio = ftx1 / a991;
    assert!(
        ratio / expected > 0.7 && ratio / expected < 1.4,
        "playback gain must use the per-model calibration, like the meter: \
         991A {a991:.4}, FTX-1 {ftx1:.4} (ratio {ratio:.2}, expected {expected:.2}). \
         A ratio near 1.0 means one flat constant is back on the audio path"
    );
}
