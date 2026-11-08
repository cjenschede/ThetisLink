// SPDX-License-Identifier: GPL-2.0-or-later

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use log::{info, warn, debug};
use sdr_remote_core::dev_info;
use tokio::net::ToSocketAddrs;
use tokio::sync::{mpsc, watch, Mutex as AsyncMutex};
use tokio::time::{interval, Duration};

use sdr_remote_core::codec::OpusEncoderWideband;
use sdr_remote_core::jitter::{BufferedFrame, JitterBuffer, JitterResult};
use sdr_remote_core::protocol::*;
use sdr_remote_core::resample::{resampler, Resampler, UpFilter};
use sdr_remote_core::{FRAME_SAMPLES, FRAME_SAMPLES_WIDEBAND, MAX_PACKET_SIZE, NETWORK_SAMPLE_RATE, NETWORK_SAMPLE_RATE_WIDEBAND};

use crate::audio::AudioBackend;
use crate::commands::Command;
use crate::rx_stream::{channel_opus, recover_or_conceal, recover_or_conceal_one, Decoded, RxStream};
use crate::state::RadioState;

/// The client's one transport since build 133: a QUIC connection, straight to
/// the address the operator typed or through the relay. Plain UDP and the
/// classic relay tunnel are gone.
///
/// Mimics the part of `UdpSocket`'s surface the engine uses (`send_to`,
/// `recv_from`), so the call sites read as they did over UDP.
struct ClientTransport {
    client: AsyncMutex<sdr_remote_quic::QuicClient>,
    // Outside the mutex: the receive side holds that while it waits.
    stop: sdr_remote_quic::QuicStop,
    uplink_tx: mpsc::UnboundedSender<(SocketAddr, Vec<u8>)>,
    binding: tokio::sync::watch::Receiver<Option<sdr_remote_quic::Binding>>,
    /// The address the engine last sent to; what comes back is tagged with it
    /// (`quic_label`).
    engine_addr: std::sync::Mutex<Option<SocketAddr>>,
    /// QUIC via the relay: every packet goes to this one address,
    /// whatever the engine names. The route layer underneath carries it
    /// through the relay.
    relay_peer: Option<SocketAddr>,
}

/// The one address the engine's QUIC connection goes to when it runs via the
/// relay: made up, never on a wire (the route layer frames it for the relay).
const QUIC_RELAY_SERVER: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 1)), 4580);

impl ClientTransport {
    fn new(client: sdr_remote_quic::QuicClient, relay_peer: Option<SocketAddr>) -> Self {
        Self {
            uplink_tx: client.uplink_tx.clone(),
            stop: client.stop.clone(),
            binding: client.binding.clone(),
            client: AsyncMutex::new(client),
            engine_addr: std::sync::Mutex::new(None),
            relay_peer,
        }
    }

    async fn send_to<A: ToSocketAddrs>(&self, buf: &[u8], addr: A) -> std::io::Result<usize> {
        let target = match self.relay_peer {
            Some(peer) => peer,
            None => {
                // A resolved address costs nothing here; a name is looked up -
                // the engine only sends to a name before Connect resolved it.
                // The port is the one in the address: since build 133 the
                // server listens with QUIC on its own port.
                let target = first_ipv4(tokio::net::lookup_host(addr).await?)
                    .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "no IPv4 address"))?;
                *self.engine_addr.lock().unwrap() = Some(target);
                target
            }
        };
        // The result is propagated, not dropped: a caller that judges on it
        // (the Thetis stream saving) must hear that a packet did not leave.
        self.uplink_tx
            .send((target, buf.to_vec()))
            .map(|_| buf.len())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "QUIC client stopped"))
    }

    async fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        loop {
            let Some((from, data)) = self.client.lock().await.inbound_rx.recv().await else {
                return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "QUIC client stopped"));
            };
            // Too large for the buffer: dropped whole, never cut.
            let Some(n) = sdr_remote_core::take_frame(&data, buf) else { continue };
            let tag = quic_label(from, *self.engine_addr.lock().unwrap());
            return Ok((n, tag));
        }
    }

    /// Through the relay: the address the operator connects to is a label,
    /// there is no server IP to resolve or to filter on.
    fn via_relay(&self) -> bool {
        self.relay_peer.is_some()
    }

    /// The route this transport is, for the measurement and the screens.
    fn route(&self) -> crate::measure::Route {
        match self.relay_peer {
            Some(_) => crate::measure::Route::QuicRelay,
            None => crate::measure::Route::Quic,
        }
    }

    /// The channel binding of the live QUIC connection; `None` while no QUIC
    /// connection stands.
    fn binding(&self) -> Option<sdr_remote_quic::Binding> {
        *self.binding.borrow()
    }
}

/// The connection kind of a client through the relay, from what its route
/// layer says. A function of its own, with a table test: the three arms sat in
/// the loop, and swapping two of them kept every test green.
fn kind_through_the_relay(path: sdr_remote_quic::ClientPath) -> sdr_remote_core::path::PathKind {
    use sdr_remote_core::path::PathKind;
    use sdr_remote_quic::ClientPath;
    match path {
        ClientPath::Relay => PathKind::Relay,
        ClientPath::Coupling => PathKind::RelayCoupling,
        ClientPath::Direct => PathKind::DirectViaRelay,
        ClientPath::Local => PathKind::LocalViaRelay,
        ClientPath::RelayTcp => PathKind::RelayTcp,
        ClientPath::DirectRelayGone => PathKind::DirectRelayGone,
        ClientPath::LocalRelayGone => PathKind::LocalRelayGone,
    }
}

/// What a frame that came over QUIC reads as: the IP is the connection's own,
/// only the port is the one the engine sent to (the server's UDP port, which
/// the operator typed). So it passes the receive filter like a UDP frame - and a
/// late frame from a server the engine left can never read as the new one.
fn quic_label(from: SocketAddr, sent_to: Option<SocketAddr>) -> SocketAddr {
    match sent_to {
        Some(e) => SocketAddr::new(from.ip(), e.port()),
        None => from,
    }
}

/// Whether an authentication result belongs to a QUIC connection that is gone:
/// the binding the answer was made with is not the one now. Its proof is about
/// another TLS session, so it neither connects nor refuses - the server of the
/// new connection asks again.
fn result_is_stale(answered_with: Option<sdr_remote_quic::Binding>, now: Option<sdr_remote_quic::Binding>) -> bool {
    answered_with != now
}

/// An authentication in flight (mutual authentication with a session key): what
/// the client sent, and the key it will mean if the server's proof verifies.
/// `verified` is set at the password step when 2FA follows, because the final
/// result after the code carries no proof of its own.
struct PendingAuth {
    server_nonce: [u8; 16],
    client_nonce: [u8; 16],
    key: [u8; 32],
    verified: bool,
    /// The QUIC connection's binding the answer and key were made with (`None`
    /// while no QUIC connection stood).
    binding: Option<sdr_remote_quic::Binding>,
}

/// The address this client can send to and listen from: the first IPv4 one.
/// The socket is bound on 0.0.0.0, so an IPv6 literal or an AAAA-only name is no
/// address for it - `None`, and the Connect arm says so. One rule for the
/// literal and the looked-up way in (the literal way had none).
pub(crate) fn first_ipv4(addrs: impl IntoIterator<Item = std::net::SocketAddr>) -> Option<std::net::SocketAddr> {
    addrs.into_iter().find(|a| a.is_ipv4())
}

#[cfg(test)]
mod server_address_tests {
    use super::first_ipv4;

    /// Each way the route layer names maps to its own kind: all through the
    /// relay is R, a direct path being tried is still R but coupling, and a
    /// confirmed direct path is D+R (swapping coupling and D+R kept
    /// every test green).
    #[test]
    fn the_route_layer_s_way_maps_to_one_kind_each() {
        use sdr_remote_core::path::PathKind;
        use sdr_remote_quic::ClientPath;
        assert_eq!(super::kind_through_the_relay(ClientPath::Relay), PathKind::Relay);
        assert_eq!(super::kind_through_the_relay(ClientPath::Coupling), PathKind::RelayCoupling);
        assert_eq!(super::kind_through_the_relay(ClientPath::Direct), PathKind::DirectViaRelay);
        assert_eq!(super::kind_through_the_relay(ClientPath::Local), PathKind::LocalViaRelay);
        assert_eq!(super::kind_through_the_relay(ClientPath::RelayTcp), PathKind::RelayTcp);
        assert_eq!(super::kind_through_the_relay(ClientPath::DirectRelayGone), PathKind::DirectRelayGone);
        assert_eq!(super::kind_through_the_relay(ClientPath::LocalRelayGone), PathKind::LocalRelayGone);
    }

    /// A QUIC frame keeps the IP it came from and takes the port the engine
    /// sent to - from the server the engine left it stays that server.
    #[test]
    fn a_quic_frame_keeps_its_own_address() {
        use super::quic_label;
        let from = a("192.168.1.50:4581");
        assert_eq!(quic_label(from, Some(a("192.168.1.50:4580"))), a("192.168.1.50:4580"), "the server the operator typed");
        assert_eq!(quic_label(from, Some(a("10.0.0.5:4580"))), a("192.168.1.50:4580"), "a late frame from the server left behind");
        assert_eq!(quic_label(from, None), from, "nothing sent yet");
    }

    /// A result for another QUIC connection is stale; the same binding, or none
    /// on both sides (no connection), is not.
    #[test]
    fn a_result_for_another_connection_is_stale() {
        use super::result_is_stale;
        assert!(!result_is_stale(None, None), "no connection on either side: not stale");
        assert!(!result_is_stale(Some([1; 32]), Some([1; 32])), "same connection");
        assert!(result_is_stale(Some([1; 32]), Some([2; 32])), "a new connection");
        assert!(result_is_stale(Some([1; 32]), None), "the connection is gone");
    }
    use std::net::SocketAddr;

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn only_an_ipv4_address_is_one_for_this_client() {
        assert_eq!(first_ipv4(Some(a("192.168.1.50:4580"))), Some(a("192.168.1.50:4580")));
        assert_eq!(first_ipv4(Some(a("[2001:db8::1]:4580"))), None, "an IPv6 literal");
        assert_eq!(
            first_ipv4([a("[2001:db8::1]:4580"), a("192.168.1.50:4580")]),
            Some(a("192.168.1.50:4580")),
            "a dual-stack name: the AAAA record first, the A record wins"
        );
        assert_eq!(first_ipv4([a("[2001:db8::1]:4580")]), None, "an AAAA-only name");
        assert_eq!(first_ipv4(None), None, "no literal: the lookup decides");
    }
}

/// How long the client waits for the answer to its 2FA code before it says the
/// attempt is lost. A server that still has the attempt answers within one
/// round trip; one that reaped it answers never.
const TOTP_ANSWER_TIMEOUT_SECS: u64 = 5;

/// A result code the server never sends: what an `AuthResult` becomes on this
/// side when its proof does not verify, so the one `match` below handles it as
/// a verdict of its own.
const PROOF_INVALID: u8 = 0xff;
/// Likewise: an ACCEPTED that arrives for a session that is already verified,
/// with nothing in flight - a duplicate or a replay, not a new session.
const RESULT_REPEATED: u8 = 0xfe;
/// Likewise: an accepting answer without a proof - a server that does not prove
/// it knows the password, which this client does not connect to.
const RESULT_NO_PROOF: u8 = 0xfd;
/// Likewise: an accepting answer while no authentication is in flight and no
/// session is verified - typically the answer to an attempt that a lost link
/// forgot a moment before. Nothing to verify it against, so nothing to judge:
/// ignored, and the next challenge starts a new attempt. Until build 122 this
/// read as RESULT_NO_PROOF, and a relay stall left the client on "the server
/// does not prove it knows the password" (field test 2026-10-06 20:15:58).
const RESULT_NOT_IN_FLIGHT: u8 = 0xfc;

/// Pure application of a `YaesuPresence` packet to the client state. Presence is
/// the connected authority (shared to desktop + Android). Returns
/// `(slot0_changed, slot1_changed)` so the caller can do value-change-only logging.
/// Sets connected dynamically true/false.
pub(crate) fn apply_yaesu_presence(state: &mut RadioState, p: &YaesuPresencePacket) -> (bool, bool) {
    let c0 = state.yaesu_connected != p.slot0_present;
    let c1 = state.yaesu2_connected != p.slot1_present;
    state.yaesu_connected = p.slot0_present;
    state.yaesu2_connected = p.slot1_present;
    state.yaesu_model = p.slot0_model;
    state.yaesu2_model = p.slot1_model;
    state.yaesu_port_trouble = p.slot0_trouble;
    state.yaesu2_port_trouble = p.slot1_trouble;
    // What the server has a radio for, on or off. Every client gets this
    // packet; RadioInfo (the first version's source) only reaches clients
    // that asked for slot 1, and the desktop asks only when its own slot-1
    // switch is on (found in the field, 2026-09-17).
    state.yaesu_configured = p.slot0_configured;
    state.yaesu2_configured = p.slot1_configured;
    state.yaesu_rx_levels_tuned = p.rx_levels_tuned;
    state.radios_known = true;
    // An absent radio cannot report high SWR. Without this an old
    // hi_swr flag stays set and the other radio keeps the alarm alive
    // again with every state push.
    if !p.slot0_present {
        state.yaesu_hi_swr = false;
    }
    if !p.slot1_present {
        state.yaesu2_hi_swr = false;
    }
    (c0, c1)
}

/// Heartbeat interval
/// How long a loss window is, because the meter closes one per heartbeat.
/// `pub(crate)` so a test can compare the VALUE rather than scan this file for
/// the text of the line - a scan breaks the wrong way round, calling out a
/// heartbeat that moved when all that happened was `500u64` or a formatter.
pub(crate) const HEARTBEAT_INTERVAL_MS: u64 = 500;

/// Minimum connection timeout in ms (dynamic: max(this, rtt*8))
const CONNECTION_TIMEOUT_MIN_MS: u64 = 6000;

/// While transmitting, the link counts as gone after this long without an
/// answer to a heartbeat (build 265, 2026-10-20: 3 s rather than 6).
/// Longer than the server's own window of 2 s (`ptt.rs`
/// `PTT_PACKET_TIMEOUT_MS`), so the server has let go first, and a link that
/// comes back inside the difference keys the transmitter again with the press
/// still held. The server pins the relation (`session.rs`). Not stretched by
/// the round trip: a link slow enough to need it is no link to transmit on.
pub(crate) const TRANSMIT_LINK_TIMEOUT_MS: u64 = 3000;

/// Has the link gone, and after which window? `since_ack`: since the last
/// answer to a heartbeat (`None`: never answered, which is no verdict).
/// `since_audio`: since the last audio packet.
///
/// Listening: both absent past max(6 s, rtt x 8). Transmitting: the answer
/// alone, past 3 s - audio that still arrives says nothing about whether the
/// server hears this client, and that is what keeps a transmitter keyed (a
/// line dead only towards the server: the server lets go at 2 s and the
/// audio keeps the red up).
pub(crate) fn link_lost(
    transmitting: bool,
    rtt_ms: u16,
    since_ack: Option<Duration>,
    since_audio: Option<Duration>,
) -> Option<u64> {
    let past = |d: Option<Duration>, ms: u64| d.map(|d| d > Duration::from_millis(ms));
    if transmitting {
        return past(since_ack, TRANSMIT_LINK_TIMEOUT_MS)
            .unwrap_or(false)
            .then_some(TRANSMIT_LINK_TIMEOUT_MS);
    }
    let timeout_ms = (rtt_ms as u64 * 8).max(CONNECTION_TIMEOUT_MIN_MS);
    let acks = past(since_ack, timeout_ms).unwrap_or(false);
    let audio = past(since_audio, timeout_ms).unwrap_or(true);
    (acks && audio).then_some(timeout_ms)
}

/// Max samples to drain when not connected (500ms worth at 48kHz)
const RING_DRAIN_SIZE: usize = 48_000 / 2;


// --- TX AGC (Automatic Gain Control) ---

const AGC_TARGET: f32 = 0.25;    // Target peak amplitude (~-12dB)
const AGC_MAX_GAIN: f32 = 10.0;  // +20dB max boost
const AGC_MIN_GAIN: f32 = 0.1;   // -20dB max attenuation
const AGC_ATTACK: f32 = 0.3;     // Fast attack (per 20ms frame)
const AGC_RELEASE: f32 = 0.01;   // Slow release (per 20ms frame)
const AGC_GATE: f32 = 0.001;     // Noise gate - don't boost below this

struct TxAgc {
    gain: f32,
    peak_env: f32,
}

impl TxAgc {
    fn new() -> Self {
        Self { gain: 1.0, peak_env: 0.0 }
    }

    fn process(&mut self, samples: &mut [f32]) {
        let peak = samples.iter().map(|s| s.abs()).fold(0.0f32, f32::max);

        let coeff = if peak > self.peak_env { AGC_ATTACK } else { AGC_RELEASE };
        self.peak_env += (peak - self.peak_env) * coeff;

        if self.peak_env > AGC_GATE {
            let desired = AGC_TARGET / self.peak_env;
            self.gain = desired.clamp(AGC_MIN_GAIN, AGC_MAX_GAIN);
        }

        for s in samples.iter_mut() {
            *s *= self.gain;
        }
    }
}

/// The Yaesu TX levels over ten seconds of live-mic audio, for the field log:
/// the peak before the EQ, at the AGC's input, what the AGC does, and what goes
/// to Opus. All on the scale where 1.0 is full scale; the last one is the mic
/// meter. The window counts transmitted frames, not the clock; switching the
/// AGC, moving the gain or letting go of the PTT closes it, so a line is about
/// one AGC setting, one gain and one over. The averages are over frames with
/// speech in them: in a pause the AGC gain climbs to its maximum, and counting
/// that read as a gain the voice never got.
#[derive(Default)]
struct TxLevelLog {
    frames: u32,
    speech: u32,
    pre_eq_max: f32,
    agc_in_sum: f32,
    agc_in_max: f32,
    agc_gain_sum: f32,
    out_sum: f32,
    out_max: f32,
    limiter_min: f32,
    gain: f32,
    agc_on: bool,
}

/// Ten seconds of 20 ms frames.
const TX_LEVEL_LOG_FRAMES: u32 = 500;
/// A window closed early with less than half a second in it gives no line: a
/// line about 20 ms is noise in the log.
const TX_LEVEL_LOG_MIN_FRAMES: u32 = 25;
/// A frame counts as speech when its peak at the AGC's input is above -40 dBFS.
const TX_LEVEL_SPEECH: f32 = 0.01;

impl TxLevelLog {
    /// One frame. Gives a line when the window is full, and when the AGC was
    /// switched or the gain moved: then the window so far is closed first and a
    /// new one begins. A drag over the slider gives windows under half a
    /// second, which give no line, so the line after it is about the last gain.
    #[allow(clippy::too_many_arguments)]
    fn add(&mut self, radio: &str, gain: f32, pre_eq: f32, agc_in: f32, agc_gain: f32, limiter: f32, out: f32,
           agc_on: bool) -> Option<String> {
        let switched = if self.frames > 0 && (self.agc_on != agc_on || self.gain != gain) { self.close(radio) } else { None };
        if self.frames == 0 { self.limiter_min = 1.0; }
        self.frames += 1;
        self.gain = gain;
        self.limiter_min = self.limiter_min.min(limiter);
        self.pre_eq_max = self.pre_eq_max.max(pre_eq);
        self.agc_in_max = self.agc_in_max.max(agc_in);
        self.out_max = self.out_max.max(out);
        if agc_in > TX_LEVEL_SPEECH {
            self.speech += 1;
            self.agc_in_sum += agc_in;
            self.agc_gain_sum += agc_gain;
            self.out_sum += out;
        }
        self.agc_on = agc_on;
        if self.frames >= TX_LEVEL_LOG_FRAMES {
            return Some(self.take_line(radio));
        }
        switched
    }

    /// The window ends early (an AGC switch, a gain move, the PTT let go): its
    /// line if it holds half a second or more, and a fresh window either way.
    fn close(&mut self, radio: &str) -> Option<String> {
        if self.frames >= TX_LEVEL_LOG_MIN_FRAMES {
            Some(self.take_line(radio))
        } else {
            *self = Self::default();
            None
        }
    }

    /// The line for the window so far, and a fresh window after it.
    fn take_line(&mut self, radio: &str) -> String {
        let n = self.speech.max(1) as f32;
        let db = |v: f32| if v > 0.0001 { 20.0 * v.log10() } else { -80.0 };
        let gain = if self.gain > 0.0 { format!("{:+.0} dB", crate::commands::tx_gain_db(self.gain)) } else { "off".to_string() };
        let line = format!(
            "{radio} TX level ({:.1} s, {:.0}% speech, gain {gain}, AGC {}): before EQ peak max {:.2}, \
             AGC in peak avg {:.2} max {:.2}, AGC gain avg {:.2}, limiter min {:.2}, \
             to Opus peak avg {:.2} max {:.2} ({:.0} dB)",
            self.frames as f32 * 0.02, 100.0 * self.speech as f32 / self.frames.max(1) as f32,
            if self.agc_on { "on" } else { "off" }, self.pre_eq_max,
            self.agc_in_sum / n, self.agc_in_max, self.agc_gain_sum / n, self.limiter_min,
            self.out_sum / n, self.out_max, db(self.out_max));
        *self = Self::default();
        line
    }
}

/// A raw microphone frame this close to full scale has clipped before
/// ThetisLink: the float stream from the operating system stops at 1.0.
const INPUT_CLIP: f32 = 0.99;
/// How long the clip lamp stays on after the last clipped frame, as long as
/// the meter holds its peak.
const INPUT_CLIP_HOLD: std::time::Duration = std::time::Duration::from_millis(1500);

/// Whether the peak of a raw microphone frame (before the EQ, the compressor and
/// the AGC) is a clip. The same test for Thetis and both Yaesu radios.
fn input_clips(raw_peak: f32) -> bool {
    raw_peak >= INPUT_CLIP
}

/// The TX gain at the front of the chain, for a Yaesu radio: the live
/// microphone gets four times the stored mic gain (the boost that used to sit
/// at the end), a WAV the stored gain itself. The same factors as before, so a
/// profile with the compressor and the AGC off sounds as loud as it did.
fn yaesu_front_gain(stored: f32, playback: bool) -> f32 {
    if playback { stored } else { stored * crate::commands::YAESU_MIC_GAIN_SCALE }
}

fn apply_gain(samples: &mut [f32], gain: f32) {
    for s in samples.iter_mut() {
        *s *= gain;
    }
}

/// Just under the encoder's clamp: -1 dB.
const LIMITER_CEILING: f32 = 0.891;
/// How fast the limiter lets go again.
const LIMITER_RELEASE_MS: f32 = 50.0;

/// The last step before the encoder, for Thetis and the Yaesu radios: a sample
/// that would go over the ceiling is brought down to it at once, and the gain
/// comes back over about 50 ms. No look-ahead: latency comes first, so the
/// attack is the sample itself. Below the ceiling it does nothing.
struct TxLimiter {
    gain: f32,
    release: f32,
}

impl TxLimiter {
    fn new() -> Self {
        let samples = LIMITER_RELEASE_MS * 1e-3 * NETWORK_SAMPLE_RATE_WIDEBAND as f32;
        Self { gain: 1.0, release: 1.0 - (-1.0 / samples).exp() }
    }

    /// Limits the frame; returns the lowest gain it used, 1.0 when it did nothing.
    fn process(&mut self, samples: &mut [f32]) -> f32 {
        let mut lowest = 1.0f32;
        for s in samples.iter_mut() {
            let a = s.abs();
            if a * self.gain > LIMITER_CEILING {
                self.gain = LIMITER_CEILING / a;
            }
            *s *= self.gain;
            lowest = lowest.min(self.gain);
            self.gain += (1.0 - self.gain) * self.release;
        }
        lowest
    }
}

// --- TX Compressor (speech compressor for the Yaesu USB-TX chain) ---
// The radio's own processor does not work on USB audio; this client-side compressor
// fills that gap. `amount` 0.0..1.0 (0=off): compresses peaks above a threshold
// (ratio) and applies makeup gain → more density/punch.
// Attack and release are time constants in ms at the wideband rate it runs at.
// The defaults reproduce the former fixed per-sample coefficients (0.30 and 0.02).
struct TxCompressor {
    amount: f32,
    env: f32,
    attack_coeff: f32,
    release_coeff: f32,
}

impl TxCompressor {
    fn new() -> Self {
        let mut c = Self { amount: 0.0, env: 0.0, attack_coeff: 0.0, release_coeff: 0.0 };
        c.set_timing(crate::commands::COMP_ATTACK_DEFAULT_MS, crate::commands::COMP_RELEASE_DEFAULT_MS);
        c
    }

    /// Returns whether the amount changed, so the caller logs only a change.
    fn set_amount(&mut self, a: f32) -> bool {
        let a = a.clamp(0.0, 1.0);
        let changed = a != self.amount;
        self.amount = a;
        changed
    }

    fn set_timing(&mut self, attack_ms: f32, release_ms: f32) {
        let coeff = |ms: f32| {
            let samples = ms.max(0.01) * 1e-3 * NETWORK_SAMPLE_RATE_WIDEBAND as f32;
            1.0 - (-1.0 / samples).exp()
        };
        self.attack_coeff = coeff(attack_ms);
        self.release_coeff = coeff(release_ms);
    }

    fn process(&mut self, samples: &mut [f32]) {
        if self.amount <= 0.001 { return; }
        // Threshold drops, ratio + makeup rise with amount.
        let threshold = 0.30 - 0.22 * self.amount; // 0.30 → 0.08
        let ratio = 1.0 + 5.0 * self.amount;        // 1 → 6
        let makeup = 1.0 + 1.5 * self.amount;       // 1.0 → 2.5
        let exp = 1.0 / ratio - 1.0;                // negative exponent → gain reduction
        for s in samples.iter_mut() {
            let a = s.abs();
            let coeff = if a > self.env { self.attack_coeff } else { self.release_coeff };
            self.env += (a - self.env) * coeff;
            let gain = if self.env > threshold {
                (self.env / threshold).powf(exp)
            } else { 1.0 };
            *s = (*s * gain * makeup).clamp(-1.0, 1.0);
        }
    }
}

/// Client engine: owns all network + audio logic.
/// Communicates with UI via watch (state) and mpsc (commands).
pub struct ClientEngine {
    state_tx: watch::Sender<RadioState>,
    cmd_rx: mpsc::UnboundedReceiver<Command>,
    quic_relay: Option<sdr_remote_quic::RelayRoute>,
    /// The transmit spectrum's copy of what the Yaesu encoder takes
    /// (`tx_tap`), when a screen asked for it.
    tx_tap: Option<Arc<crate::tx_tap::TxTap>>,
}

/// Runs the engine thread's body, and if it panics or returns an error, leaves
/// "no connection" on screen and marks the engine dead (`engine_dead`). The engine owns the state sender and dies with it, so the last state it
/// sent stood for good - connected, and transmitting if it was - on both clients.
/// This holds its own clone of the sender, taken before the
/// engine is moved into its thread. The relay thread has the same guard
/// (`run_reporting_a_panic` in sdr-remote-relay).
pub struct EngineStopReporter {
    state_tx: watch::Sender<RadioState>,
}

impl EngineStopReporter {
    /// Returns whether the engine stopped by a fault: a panic, or an error out
    /// of `ClientEngine::run`. The error is the likelier of the two - `run`
    /// returns one when its socket or its audio device cannot be opened, and
    /// both clients used to log it and leave Connect on a dead engine.
    /// A normal return (shutdown) changes nothing.
    pub fn run(self, body: impl FnOnce() -> Result<()>) -> bool {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
            Ok(Ok(())) => false,
            Ok(Err(e)) => {
                log::error!("Engine stopped by an error: {e:#}");
                self.state_tx.send_modify(|s| s.engine_stopped());
                true
            }
            Err(p) => {
                let what = p
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| p.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_string());
                log::error!("Engine stopped by a panic: {what}");
                self.state_tx.send_modify(|s| s.engine_stopped());
                true
            }
        }
    }
}

impl ClientEngine {
    /// A guard for the thread this engine will run in. Take it before `run`
    /// consumes the engine.
    pub fn stop_reporter(&self) -> EngineStopReporter {
        EngineStopReporter { state_tx: self.state_tx.clone() }
    }

    pub fn new() -> (Self, watch::Receiver<RadioState>, mpsc::UnboundedSender<Command>) {
        let (state_tx, state_rx) = watch::channel(RadioState::default());
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        (Self { state_tx, cmd_rx, quic_relay: None, tx_tap: None }, state_rx, cmd_tx)
    }

    /// Give the transmit spectrum a copy of each Yaesu frame as it goes to the
    /// encoder. The copy is made only while the tap is switched on, and never
    /// holds up the transmit path.
    pub fn with_tx_tap(mut self, tap: Arc<crate::tx_tap::TxTap>) -> Self {
        self.tx_tap = Some(tap);
        self
    }

    /// Connect over QUIC through the relay: `grants` are the relay
    /// library's (`RelayMonitor::start_threaded_quic`), and `tcp` its TCP line
    /// for a network that lets no UDP through (build 144). Chosen once, for the
    /// engine's life; without grants the engine connects straight to the
    /// address the operator types (`Route::asked_for`).
    pub fn with_quic_relay(
        mut self,
        grants: Option<watch::Receiver<Option<sdr_remote_quic::UdpGrant>>>,
        tcp: Option<sdr_remote_quic::TcpLine>,
    ) -> Self {
        self.quic_relay = grants.map(|g| sdr_remote_quic::RelayRoute::new(g, tcp));
        self
    }

    /// Start the engine with a platform-specific audio backend factory.
    /// The factory is called once at start and again for audio error recovery.
    /// Blocks until shutdown signal.
    pub async fn run(
        mut self,
        audio_factory: impl Fn(Option<&str>, Option<&str>) -> Result<Box<dyn AudioBackend>>,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        let tx_tap = self.tx_tap.take();
        // "Relay only" (build 137): the route's own switch, kept here so a
        // command can throw it while the connection runs. `None` without a relay.
        let relay_only_switch = self.quic_relay.as_ref().map(|r| r.relay_only.clone());
        let force_tcp_switch = self.quic_relay.as_ref().map(|r| r.force_tcp.clone());
        // Whether the relay sees our UDP (build 137): the grant the route layer
        // follows, watched here for the notice. `None` without a relay.
        let relay_grants = self.quic_relay.as_ref().map(|r| r.grant.clone());
        // The way the traffic takes (build 139), as the route layer says it.
        let relay_path = self.quic_relay.clone();
        let mut udp_blocked_watch = crate::udp_blocked::UdpBlockedWatch::default();
        let transport = match self.quic_relay.take() {
            Some(route) => {
                let client = sdr_remote_quic::QuicClient::start_with(sdr_remote_quic::Congestion::Bbr, Some(route))
                    .context("start the QUIC client")?;
                info!("Client transport: QUIC via the relay");
                ClientTransport::new(client, Some(QUIC_RELAY_SERVER))
            }
            None => {
                let client = sdr_remote_quic::QuicClient::start(sdr_remote_quic::Congestion::Bbr)
                    .context("start the QUIC client")?;
                // The one place both front ends log the transport.
                info!("Client transport: QUIC straight to the address typed");
                ClientTransport::new(client, None)
            }
        };

        let socket = Arc::new(transport);
        let start = Instant::now();

        // Audio setup - use defaults initially, can be reconfigured via commands
        let mut audio: Box<dyn AudioBackend> = audio_factory(None, None)?;
        let mut capture_rate = audio.capture_sample_rate();
        let mut playback_rate = audio.playback_sample_rate();

        let mut capture_frame_samples = (capture_rate * 20 / 1000) as usize;

        // Codec - wideband Opus (16kHz) for TX, stereo (8kHz) for RX decode
        let mut encoder = OpusEncoderWideband::new()?;
        // One RxStream per receive channel. It owns both decoders and both
        // resamplers, so no call site can pair a wideband frame with the
        // narrowband path or conceal on a decoder that holds no history -
        // the two halves of the fault found on 2026-08-16. Wideband is opt-in
        // (Settings → Audio); as long as the server streams narrowband the
        // wideband half sits idle at no cost.
        let mut st_rx1 = RxStream::new(playback_rate, "RX1")?;
        let mut st_bin_r = RxStream::new(playback_rate, "BinR")?;
        let mut st_rx2 = RxStream::new(playback_rate, "RX2")?;

        // Yaesu (FT-991A) codec + jitter buffer - independent third audio channel.
        // RX bandwidth follows the Thetis wideband toggle (build 122): per packet
        // the AUDIO_WIDEBAND flag determines whether we decode NB (8 kHz) or WB (16 kHz).
        let mut st_yaesu = RxStream::new(playback_rate, "Yaesu")?;
        let mut yaesu_jitter_buf = JitterBuffer::new(3, 40);
        let mut yaesu_logged_first = false;
        // Dual-radio slot 1: own independent
        // channel, exact mirror of slot 0.
        let mut st_yaesu2 = RxStream::new(playback_rate, "Yaesu2")?;
        let mut yaesu2_jitter_buf = JitterBuffer::new(3, 40);
        let mut yaesu2_logged_first = false;

        // Yaesu TX: wideband Opus (16kHz) for USB output. One stream per
        // radio (build 282; `tx_stream` since 291): each radio's buffer on the
        // server sees an unbroken run, also when the microphone moves from one
        // radio to the other.
        let mut yaesu_tx = [crate::tx_stream::TxStream::yaesu(0), crate::tx_stream::TxStream::yaesu(1)];
        let mut yaesu_tx_accum: Vec<f32> = Vec::new();
        let mut yaesu_tx_encoder = OpusEncoderWideband::new()?;
        // May the same microphone go to more than one Yaesu at a time?
        //
        // Off by default. Between Thetis and a Yaesu this always happened - those are two
        // separate encoding paths and nothing excluded them. Between the two Yaesu slots
        // it did not: they shared one path and slot 1 won, so the other transmitted
        // without modulation.
        let mut multi_tx = false;
        let mut yaesu_tx_bitrate_bps: i32 = 24_000;
        // The filter comes from `resample::resampler`, by direction (build
        // 260). A short filter (sinc_len 32) once cut
        // NT-USB content >8 kHz insufficiently, causing those frequencies to
        // alias back into 0-8 kHz during the 48→16 kHz decimation and be
        // audible on the RF output as "weird sounding" high tones (field
        // report, 2026-06-02).
        let mut yaesu_tx_resampler = resampler(capture_rate, NETWORK_SAMPLE_RATE_WIDEBAND, capture_frame_samples, UpFilter::Long)
            .context("create Yaesu TX resampler")?;

        // Jitter buffer for received audio (lower min for LAN, adaptive handles internet)
        let mut jitter_buf = JitterBuffer::new(3, 40);

        // Dedicated WAV playback resamplers, with the short filter of the live
        // streams (measured 2.0 ms delay 8k->48k, `output_delay()`, 2026-10-04).
        // Do not share RX1 live resampler state; recordings played through the
        // Server tab must sound like the file on disk.
        let mut wav_res_out = resampler(NETWORK_SAMPLE_RATE, playback_rate, FRAME_SAMPLES, UpFilter::Short)
            .context("WAV 8k->device resampler")?;
        let mut wav_res_out_wb = resampler(NETWORK_SAMPLE_RATE_WIDEBAND, playback_rate, FRAME_SAMPLES_WIDEBAND, UpFilter::Short)
            .context("WAV 16k->device resampler")?;

        // VRX1 + VRX2: each is a separate jitter buf plus its own RxStream.
        // Server-side FFT-channelizers feed these streams; both get mixed into
        // the main playback alongside RX1/RX2/Yaesu. VRX1 listens on RX1 IQ +
        // VFO-A, VRX2 on RX2 IQ + VFO-B.
        let mut st_vrx1 = RxStream::new(playback_rate, "VRX1")?;
        let mut st_vrx2 = RxStream::new(playback_rate, "VRX2")?;
        // Which auxiliary streams the operator has switched on. A stream that is
        // ON and receiving nothing is a dropout and should be concealed; a stream
        // that was switched OFF has simply stopped and must go quiet at once.
        // The two look identical from the jitter buffer, which is why this is
        // tracked from what the client asked for rather than guessed from timing.
        let mut vrx1_wanted = false;
        let mut vrx2_wanted = false;
        let mut yaesu_wanted = false;
        let mut yaesu2_wanted = false;
        // When each VRX was switched on, so the wait for its first packet can be
        // reported as a number instead of estimated by ear.
        let mut vrx1_enable_at: Option<Instant> = None;
        let mut vrx2_enable_at: Option<Instant> = None;
        let mut vrx1_jitter_buf = JitterBuffer::new(3, 40);
        let mut vrx1_logged_first = false;
        // Start muted (0.0): VRX audio is not attenuated by the master gain and
        // the client only sends the stored VRX volume on connect. Starting at 1.0
        // gave a hard audio peak at startup until that command arrived.
        let mut vrx1_volume: f32 = 0.0;
        let mut vrx2_jitter_buf = JitterBuffer::new(3, 40);
        let mut vrx2_logged_first = false;
        let mut vrx2_volume: f32 = 0.0; // start muted; see vrx1_volume

        // The TX-capture decimation 48 → 16 kHz, with the same filter as
        // yaesu_tx_resampler (`resample::resampler`), wide USB mics
        // (NT-USB and the like) have content up to 16 kHz that otherwise aliases
        // back into 0-8 kHz and becomes audible on the ANAN/Thetis TX output
        // during FM/AM-TX (SSB stays within 3 kHz so it is subtler).
        // The comment name (`device->8k`) stays for historical reasons; the path does
        // encode to 16 kHz wideband Opus (see `OpusEncoderWideband`).
        let mut resampler_in = resampler(capture_rate, NETWORK_SAMPLE_RATE_WIDEBAND, capture_frame_samples, UpFilter::Long)
            .context("create device->16k resampler")?;
        // After the resamplers exist: the line reads them (build 261).
        info!(
            "Client resamplers: capture {}Hz ({}smp/frame), playback {}Hz; {}",
            capture_rate, capture_frame_samples, playback_rate,
            resampler_line(&resampler_in, st_rx1.resamplers())
        );

        // State
        let mut state = RadioState::default();
        state.route = socket.route();
        let mut server_addr: Option<String> = None;
        let mut auth_password: Option<String> = None;
        let mut _auth_completed = false;
        // Mutual authentication: the authentication in flight, and the key of
        // the session it made. Nothing reads the key yet (it is meant for
        // per-packet encryption); it is cleared with the session.
        let mut pending_auth: Option<PendingAuth> = None;
        let mut session_key: Option<[u8; 32]> = None;
        // Numbers every control on its way out and repeats what the
        // server does not acknowledge (sdr_remote_core::control_ack). Declared
        // here, before the macros that use it.
        let mut controls_out = sdr_remote_core::control_ack::ControlSender::new();
        let mut controls_report_at = Instant::now();
        // Measurement mode: off until the operator switches it
        // on; then every audio arrival and heartbeat round trip goes to a file.
        // Declared here, before the macros that use it.
        let mut measure = crate::measure::Recorder::off();
        // When the buffers were last written to the measurement (2026-10-04).
        let mut measure_buffers_at: u64 = 0;
        let mut measure_buffers_at_yaesu: u64 = 0;
        let mut measure_buffers_at_yaesu2: u64 = 0;
        // The RX stream right after a Thetis PTT release: silence is not written
        // into the playout ring above a small cushion (build 106).
        let mut after_tx = crate::after_tx::AfterTx::default();
        // The same rule after a Yaesu transmission, one per radio and each on
        // its own PTT - for the measurement only: during a Yaesu transmission
        // the server keeps sending (silence), so the ring stands at its target
        // at the release and the silence sits in the jitter buffer; skipping it
        // there drained the cushion.
        let mut after_tx_yaesu = crate::after_tx::AfterTx::default();
        let mut after_tx_yaesu2 = crate::after_tx::AfterTx::default();
        // Around a PTT edge the buffers are written at every tick.
        let mut measure_ptt_was = false;
        let mut measure_buffers_burst_until: u64 = 0;
        // What the playout rule decided tick by tick, for the ring line, and when
        // that line was last written (build 157).
        let mut ring_ticks = crate::measure::RingTicks::default();
        // The pull down on the playout ring (build 158).
        let mut ring_settle = crate::after_tx::Settle::default();
        let mut measure_ring_at: u64 = 0;
        // The transmitters as last written to the measurement: compared once per
        // loop, so every place that sets a PTT flag is seen (crate::measure::PttSeen).
        let mut ptt_seen = crate::measure::PttSeen::default();
        // When the 2FA code went out, so that no answer is noticed: a client at
        // the code step sends no heartbeats, so nothing else would.
        let mut totp_code_sent_at: Option<Instant> = None;
        // Set when the wait above let go of a code step, so the no-reply
        // watchdog can name the likelier cause when nothing follows.
        let mut resumed_after_code_step = false;
        // Direct mode: the one address datagrams are accepted from. `None` over
        // the relay, where the transport already names the server.
        let mut server_sockaddr: Option<std::net::SocketAddr> = None;
        let mut stray_said_at: Option<Instant> = None;
        // Test switch: answer a challenge this many seconds late, to see the
        // server's "challenge expired" line in the field.
        let test_auth_delay_secs: u64 = std::env::var("THETISLINK_TEST_AUTH_DELAY_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        // Track when Connect was issued + whether we've ever seen
        // any reply from the server, so we can surface NoUdpResponse after
        // a timeout, and distinguish "never heard anything" from "got bad bytes".
        let mut connect_started_at: Option<Instant> = None;
        let connect_timeout_secs: u32 = 5;
        let mut connect_any_reply_seen: bool = false;
        // What was last said about each of these four streams, so an unchanged
        // repeat is not announced again. The server re-pushes the lists every
        // twenty seconds or so; saying "received 2024B" each time is not an
        // event, and eight such lines per push filled three quarters of a quiet
        // log - and with it three quarters of the tail a problem report carries
        // (2026-08-16).
        let mut said_yaesu_mem: Option<usize> = None;
        let mut said_yaesu_menu: Option<usize> = None;
        let mut said_yaesu2_mem: Option<usize> = None;
        let mut said_yaesu2_menu: Option<usize> = None;
        let mut yaesu_mem_data_clear_at: Option<Instant> = None;
        let mut yaesu2_mem_data_clear_at: Option<Instant> = None;
        let mut yaesu_menu_data_clear_at: Option<Instant> = None;
        // The server's report, arriving in numbered parts. Empty between
        // transfers; the deadline turns a transfer that stalled into a stated
        // failure instead of a wait with no end.
        let mut server_report_parts: Vec<Option<Vec<u8>>> = Vec::new();
        let mut server_report_deadline: Option<Instant> = None;
        let mut yaesu2_menu_data_clear_at: Option<Instant> = None;
        // Thetis's transmit stream: sequence, flag and burst (`tx_stream`,
        // build 291).
        let mut thetis_tx = crate::tx_stream::TxStream::thetis();
        let mut hb_sequence: u32 = 0;
        let mut ptt = false;
        let mut thetis_ptt = false;
        // Is there an outstanding PTT request from this operator, per transmitter?
        //
        // Not the same as the three flags around it. Those say "we believe we
        // are transmitting" and a refusal drops them at once - which is right,
        // but it means they cannot answer "was this refusal meant for us". These
        // only end when the operator lets go.
        //
        // One per transmitter, indexed the same way the roger-beep channel is:
        // 0 = Thetis, 1 = Yaesu slot 0, 2 = Yaesu slot 1. It was a single bool,
        // so letting go of the 991A also cleared an outstanding request for
        // Thetis - and then a refusal aimed at Thetis counted as stale and was
        // thrown away.
        // [Thetis, radio one, radio two] - the same three, in the same order, as
        // ptt_denial::TRANSMITTERS and DeniedTarget::index(). Three places count
        // these and none of them can see the other two.
        let mut ptt_requested = [false; crate::ptt_denial::TRANSMITTERS];
        let mut yaesu_ptt = false;
        // Slot-1 PTT (dual-radio). Mutually exclusive with yaesu_ptt in practice
        // (one mic) → the mic-TX chain picks the packet type based on which is active.
        let mut yaesu2_ptt = false;

        // CW: the edge counter, the last few edges (repeated in every packet
        // so a lost one is caught up), and where the key goes.
        let mut cw_seq: u32 = 0;
        let mut cw_recent: std::collections::VecDeque<sdr_remote_core::protocol::CwEdge> =
            std::collections::VecDeque::with_capacity(sdr_remote_core::protocol::CwEdgesPacket::MAX_EDGES);
        let mut cw_target: u8 = 0;
        // A bit per transmitter the current CW run has keyed, for clearing its
        // refusals when it is over (PttDenial::cw_run_over).
        let mut cw_asked: u8 = 0;
        let mut cw_hang_ms: u16 = 0; // 0 = the server's default, until the UI says
        // A run is "asking" for its transmitter from its first edge until two
        // seconds after its last, so a refusal in between reaches the screen.
        let mut cw_last_edge: Option<Instant> = None;

        // One place for what a lost session must not leave behind - the part that
        // can key a transmitter or hold a sign up: the refusal, the PA warning,
        // what the server had configured, and the transmit flags that ride on
        // every audio packet. Five paths lose a session (the operator's
        // Disconnect, the link going away from outside, the server's Disconnect,
        // the time-out, and a Connect that replaces a live session), and until
        // build 83 four of them spelled this list out by hand and the fifth did
        // not spell it at all. What differs per path - which
        // buffers to drain, which counters to zero - stays with that path.
        // Which build the server last said it runs, so the log writes a line
        // when that CHANGES rather than twice a second. Declared above
        // `session_lost!` because a macro in a function body binds the locals
        // it can see at its definition, not at its call.
        let mut last_server_build = sdr_remote_core::protocol::PeerBuild::Unknown;
        // Leave a server the client has refused (it proved nothing, or proved
        // itself wrongly) or that refused the password: stop talking to it.
        // Without this the heartbeats went on, the server held the session it
        // had admitted and kept sending audio to a client that showed
        // "refused" (eleven packets in six seconds).
        //
        // This drops the ADDRESS and nothing else, on purpose: an address that
        // went None is the link-down signal, and the block at the top of the
        // loop ("Detect disconnect from outside") does the rest on the next
        // tick - the jitter buffers, the connected flag the controls read,
        // `session_lost!()` with the transmit flags and what the server said
        // about itself, the metrics. One door; a second copy of that list here
        // would drift from it (the mutants
        // showed the block already covers it: the second-session tests stay
        // green with and without one). `tell` sends the Disconnect first, for
        // a server that admitted us; `silent` does not - a server that refused
        // the password holds no session. The pair goes together or not at all:
        // only the first is read, the second would leak into the next session.
        macro_rules! leave_server {
            (tell) => {{
                if let Some(ref addr) = server_addr {
                    let mut buf = [0u8; DisconnectPacket::SIZE];
                    DisconnectPacket::serialize(&mut buf);
                    let _ = send_tx!(&buf, addr.as_str());
                    info!("Left the server");
                }
                leave_server!(silent);
            }};
            (silent) => {{
                server_addr = None;
                server_sockaddr = None;
            }};
        }
        // The transmit flags of a session that is gone, cleared in one place:
        // on every way a session is lost (`session_lost!`), and already at a
        // new challenge. The server counts us as signed in once our answer to
        // it is in, so nothing sent after that answer may carry the old press;
        // cleared only at "Auth accepted", the packets in between keyed Thetis
        // for ~30 ms (build 252, field 2026-10-18, wifi to 5G during a press).
        macro_rules! clear_transmit_flags {
            ($why:literal) => {{
                if thetis_ptt || yaesu_ptt || yaesu2_ptt {
                    warn!(concat!($why, " (thetis={} yaesu1={} yaesu2={}): transmit flags cleared"),
                        thetis_ptt, yaesu_ptt, yaesu2_ptt);
                    // Heard, not only seen: the operator may be talking on
                    // without looking at the screen (build 253).
                    audio.sound_ptt_dropped();
                    info!("PTT dropped with the session: warning tone");
                }
                thetis_ptt = false;
                ptt = false;
                yaesu_ptt = false;
                yaesu2_ptt = false;
            }};
        }
        macro_rules! session_lost {
            () => {{
                // Everything the server told us about itself and the radios:
                // RadioState::forget_session, which lives next to those fields
                // so that adding one and forgetting it is a screen apart
                // instead of a file apart. Among what it clears: a report left
                // high would hide the rising edge that clears a refused first
                // packet after the reconnect (PttDenial::cw_keyed_after_all).
                // Its proof is what this macro evaluates to: a link-down hands
                // it to `state.link_down`, a new session lets it go by name.
                let forgotten = state.forget_session();
                // `forget_session` clears what the screen shows about the
                // server's build; this clears what the LOG remembers saying
                // about it, so the next connection states its verdict again.
                // Without it a reconnect to a different server that disagrees
                // in the same words would go by in silence.
                last_server_build = sdr_remote_core::protocol::PeerBuild::Unknown;
                // The recent CW edges would otherwise ride into the next
                // session as "the edges before this one" and key it.
                cw_recent.clear();
                cw_asked = 0;
                // The key belongs to the session that is gone, and so
                // do the nonces of an authentication in flight: a late or
                // replayed result must not verify against them after a
                // Disconnect. The wait on an unanswered code and a
                // refused code are the two places that end an attempt WITHOUT
                // this: there the nonces stay, because the answer to the next
                // code is verified by them.
                if session_key.take().is_some() {
                    debug!("Session key dropped with the session");
                }
                pending_auth = None;
                // And the clock on a code that was out: `forget_session` lets go of
                // the code step itself.
                totp_code_sent_at = None;
                // Nothing of the session that is gone is repeated into the next
                // one, and the next server has to show again that it acknowledges.
                controls_out.session_ended();
                measure.event(start.elapsed().as_millis() as u64, crate::measure::Event::SessionEnd);
                clear_transmit_flags!("Link down while transmitting");
                forgotten
            }};
        }
        // The meter's window after this client lets go of Thetis's PTT.
        let mut meter_window = crate::ptt_button::MeterWindow::default();
        // When the spectra's automatic reference pauses (build 176).
        let mut auto_ref_hold = crate::ptt_button::AutoRefHold::default();
        let mut mic_gate_delay_ms: u32 = 0;
        let mut last_capture_ptt = false;
        // The last playback mute asked for: a new backend starts unmuted, and both
        // front ends send it only on the edge of a press.
        let mut playback_muted = false;
        let mut last_hb_sent = Instant::now();
        let mut last_hb_ack_time: Option<Instant> = None;
        // While RX1 is being concealed: when it started, the first peak, how many
        // frames, and when it was last said out loud.
        let mut conceal_since: Option<Instant> = None;
        let mut conceal_first_peak: f32 = 0.0;
        let mut conceal_frames: u32 = 0;
        // One line per closed gap ate the log. On a lossy mobile link there are
        // hundreds an hour, and a report from a drive on 2026-09-12 held 354 of them
        // out of 469 lines - three quarters of the kept history spent on one message,
        // which is why the file reached back fifty-five minutes instead of hours. The
        // detail was never read per gap; what a reader wants is how bad the last
        // stretch was. So: counted here, reported per window, silent at zero.
        let mut conceal_gaps: u32 = 0;
        let mut conceal_total_frames: u32 = 0;
        let mut conceal_longest = Duration::ZERO;
        let mut last_conceal_report = Instant::now();
        let mut conceal_said = Instant::now();
        // Which bits last disagreed, so the line is written when it changes
        // rather than every second.
        let mut last_subs_differ: u16 = 0;
        let mut last_hb_ack_rtt: u16 = 0;
        let mut was_connected = false;
        let mut logged_first_rx = false;
        let mut logged_first_tx = false;
        let mut rx_volume: f32 = 0.2;     // Thetis ZZLA sync + RX1 audio gain
        let mut vfo_a_volume: f32 = 1.0; // Additional client-only RX1 gain (VFO A Vol slider)
        let mut local_volume: f32 = 1.0; // Master playback gain (client-only)
        let mut tx_gain: f32 = 0.5;
        let mut play_volume: f32 = 1.0; // WAV-playback ('Play') level (client-only)
        let mut last_sent_volume: u16 = 0;
        let mut rx_volume_synced: bool = false; // Don't send ZZLA until server value received
        let mut agc = TxAgc::new();
        let mut yaesu_agc = TxAgc::new(); // separate AGC envelope for the Yaesu TX branch (part B)
        let mut yaesu_tx_agc_enabled = false; // own Yaesu AGC toggle (separate from Thetis agc_enabled)
        let mut yaesu_compressor = TxCompressor::new(); // client-side speech compressor (USB-TX)
        // Radio 2 (FTX-1): own compressor/AGC so the TX chain is configurable per radio
        // (like the per-radio EQ). PTT is mutually exclusive → the right chain per PTT.
        let mut yaesu2_agc = TxAgc::new();
        let mut yaesu_level_log = [TxLevelLog::default(), TxLevelLog::default()];
        let mut input_clip_at: Option<Instant> = None;
        let mut thetis_limiter = TxLimiter::new();
        // One per radio, like the compressor and the AGC.
        let mut yaesu_limiters = [TxLimiter::new(), TxLimiter::new()];
        let mut yaesu2_tx_agc_enabled = false;
        let mut yaesu2_compressor = TxCompressor::new();
        let mut agc_enabled = false;
        let mut rx2_volume: f32 = 0.2;     // Thetis ZZLB sync + RX2 audio gain
        let mut vfo_b_volume: f32 = 1.0;   // Additional client-only RX2 gain (VFO B Vol slider)
        let mut audio_mode: u16 = 0;       // 0=Mono, 1=BIN, 2=Split
        let mut smeter_source: u8 = 1;     // 0=Sig, 1=Avg (default), 2=MaxBin
        // Track last Binaural ControlPacket value sent on PTT-side-effect path.
        // Avoids spamming the server with redundant rx_bin_enable cmds when the
        // PTT-state hasn't actually flipped (an early version sent 38k per session).
        let mut last_sent_bin: Option<u16> = None;
        let stereo_output = audio.supports_stereo(); // false on Android

        // Audio recording state
        let mut rec_rx1: Option<crate::wav::WavWriter> = None;
        let mut rec_rx2: Option<crate::wav::WavWriter> = None;
        let mut rec_yaesu: Option<crate::wav::WavWriter> = None;
        let mut rec_yaesu2: Option<crate::wav::WavWriter> = None;
        let mut rec_vrx1: Option<crate::wav::WavWriter> = None;
        let mut rec_vrx2: Option<crate::wav::WavWriter> = None;

        // WAV playback state
        let mut playback_wav: Option<Vec<i16>> = None;
        let mut playback_wav_rate: u32 = NETWORK_SAMPLE_RATE;
        // A recording on its way to a transmitter, per radio (build 262), and
        // the 16 kHz samples waiting to fill a frame for the Thetis encoder.
        let mut wav_to_thetis: Option<crate::wav_tx::WavToRadio> = None;
        let mut wav_to_yaesu: Option<crate::wav_tx::WavToRadio> = None;
        let mut wav_thetis_accum: Vec<f32> = Vec::new();
        let mut playback_pos: usize = 0;
        let mut playback_is_tx: bool = false;
        // The roger beep. `roger_tone` holds the tone being played and which
        // channel it belongs to; while it runs, that channel's PTT stays keyed
        // and its microphone is not sent - a beep with the operator's chair
        // creaking under it is not a beep.
        let mut roger_cfg = crate::roger::RogerBeep::default();
        let mut roger_tone: Option<(crate::roger::RogerTone, u8, Instant)> = None;
        // Everything ticked beyond the first, each with its own position and
        // its own resamplers - those carry state between calls, so streams
        // taking turns through one would smear into each other.
        struct ExtraPlayback {
            samples: Vec<i16>,
            rate: u32,
            pos: usize,
            res_nb: Resampler,
            res_wb: Resampler,
        }
        let mut playback_extra: Vec<ExtraPlayback> = Vec::new();
        // True as long as we have turned off the Thetis TXEQ for a WAV-playback to the
        // main radio (to restore on stop/PTT-release/end).
        let mut thetis_txeq_bypassed: bool = false;

        let mut yaesu_volume: f32 = 0.5;   // Yaesu audio volume (client-only)
        // Slot-1 volume starts MUTED (0.0); required per lesson from build 88
        // (VRX peak at startup): each new
        // audio channel starts muted until the UI/effective volume has arrived.
        let mut yaesu2_volume: f32 = 0.0;
        let mut yaesu_local_mic_gain: f32 = 0.2; // Local Yaesu mic gain (before Opus encoding)
        // The TX-mic EQ runs on the capture frames, so it is built for the capture
        // rate and follows it in `resync_audio_rates!`.
        let mut yaesu_eq = crate::eq::Equalizer::new(capture_rate as f32);
        // Slot-1 (FTX-1) own TX-mic EQ + gain; applied when transmitting on radio 2
        // (PTT mutually exclusive, so chosen per PTT in the encode chain).
        let mut yaesu2_local_mic_gain: f32 = 0.2;
        let mut yaesu2_eq = crate::eq::Equalizer::new(capture_rate as f32);
        info!("Yaesu TX EQ at the capture rate: {} Hz", capture_rate);
        let mut last_sent_rx2_volume: u16 = 0;
        let mut rx2_volume_synced: bool = false; // Don't send ZZLB until server value received
        let mut rx2_volume_user_changed: bool = false; // Only send when user changed slider
        let mut spectrum_enabled = false;
        let mut spectrum_fps: u8 = sdr_remote_core::DEFAULT_SPECTRUM_FPS;
        let mut spectrum_zoom: f32 = 1.0;
        let mut spectrum_pan: f32 = 0.0;
        let mut rx2_spectrum_zoom: f32 = 1.0;
        let mut rx2_spectrum_pan: f32 = 0.0;
        let mut spectrum_max_bins: u16 = sdr_remote_core::DEFAULT_SPECTRUM_BINS as u16;
        let mut spectrum_fft_size_k: u16 = 0;
        let mut rx2_spectrum_fft_size_k: u16 = 0;
        // Per VRX: its receiver's FFT size went out this session while it was
        // open. The screen re-sends the subscription on every zoom step; only
        // an open carries the size.
        let mut vrx_fft_sent = [false; 2];

        // Pending frequency: prevents stale server CAT values from overwriting local changes
        let mut pending_freq: Option<u64> = None;
        let mut pending_freq_time: Option<Instant> = None;
        let mut pending_freq_rx2: Option<u64> = None;
        let mut pending_freq_rx2_time: Option<Instant> = None;

        // Suppress server power broadcasts after sending a power command
        let mut power_suppress_until = Instant::now();
        // "Start Thetis automatically": the option, and its once-per-run
        // decision (crate::thetis_autostart). Across reconnects on purpose.
        let mut thetis_autostart_enabled = false;
        let mut thetis_autostart = crate::thetis_autostart::ThetisAutostart::default();
        // Commands the engine gives itself (the autostart's power-on), taken
        // before the front end's: the same path as a press on the button.
        let mut own_commands: Vec<Command> = Vec::new();

        // Packet loss, one window per stream, closed on every heartbeat.
        // Three streams because the server protects three and used to decide all
        // of them from a figure that only ever measured one.
        let mut loss_meter = crate::loss_window::LossMeter::new();
        let mut last_worst_stream: Option<crate::loss_window::Stream> = None;
        let mut last_stale_windows: u32 = 0;

        // Bandwidth monitor (down/up Kbit/s) over a rolling ~500 ms window.
        // RX bytes are summed on every recv_from(); TX bytes via the
        // `send_tx!` macro that wraps every send_to call-site. On every
        // window rollover the kbps is computed and written to `state.down_kbps`/
        // `up_kbps`: shown in the Server-tab Statistics grid.
        let mut bw_window_start = Instant::now();
        let mut bw_rx_bytes: u64 = 0;
        let mut bw_tx_bytes: u64 = 0;
        // Per-PacketType byte counter for the RX stream; indexed on
        // the `packet_type` byte (data[2]). Every 5 s a top-5
        // overview is logged to info! so the operator can see which
        // stream consumes the most (without a UI extension).
        let mut bw_by_type: [u64; 256] = [0; 256];
        let mut bw_breakdown_start = Instant::now();
        // Local macro: wraps socket.send_to(buf, addr).await and adds buf bytes
        // to bw_tx_bytes. Replaces the 80+ inline call-sites in this function
        // without per-site instrumentation. Identifiers `socket` and `bw_tx_bytes`
        // are resolved in the current scope at invocation.
        // What this client has asked the server for. Recorded HERE, in the one
        // place every outgoing packet passes, rather than at each of the seventy
        // sites that build a control - a rule spread over seventy places is a rule
        // someone forgets at the seventy-first. `ControlPacket::deserialize`
        // checks the header's packet type, so this recognises a control exactly
        // instead of guessing from the length.
        let mut session_intent = crate::session_intent::SessionIntent::new();
        // What send_control! held back from a server that does not take it,
        // sent by the restore block once one does.
        let mut held_back = crate::session_intent::HeldBack::default();

        // The Thetis streams while a Yaesu is the radio being heard. Kept here
        // rather than in the Android screen: it is a timing rule with three
        // states, there is no Kotlin test harness, and the unsubscribe has to
        // travel through the send path so the restore above replays it after a
        // network switch.
        let mut yaesu_listening = false;
        let mut yaesu_listening_since: Option<Instant> = None;
        let mut thetis_streams_stopped = false;
        // When a stop or resume did not leave the socket, do not hammer at it.
        // The tick runs every 20 ms, so retrying on every one of them is fifty
        // attempts and fifty log lines per second - and the condition where a
        // send fails IS a network switch, which is exactly when the log has to
        // survive to prove this works. The repair that made the flag honest
        // could otherwise erase its own evidence.
        let mut thetis_stream_retry_at: Option<Instant> = None;
        let mut thetis_stream_warned_at: Option<Instant> = None;

        macro_rules! send_tx {
            ($buf:expr, $addr:expr) => {{
                let __buf: &[u8] = $buf;
                bw_tx_bytes = bw_tx_bytes.wrapping_add(__buf.len() as u64);
                crate::session_intent::record_if_control(&mut session_intent, __buf);
                // Direct mode sends to the address resolved at Connect - the one
                // the receive filter expects - instead of resolving the name
                // again for every packet (the two
                // resolutions need not agree, and a name is resolved per send).
                match server_sockaddr {
                    Some(sa) => socket.send_to(__buf, sa).await,
                    None => socket.send_to(__buf, $addr).await,
                }
            }};
        }

        // Every control the client sends goes through here: one place that builds
        // the packet and hands it to send_tx (which records it for the session
        // replay). Each control gets a sequence number and a retry here, and
        // that has to happen in one place, not in sixty (build 86 brought them
        // together; behaviour unchanged). Evaluates to send_tx's Result.
        // An audio packet of `$stream` arrived: the loss meter counts it, and
        // measurement mode (when on) writes it down. One place for both, so a
        // stream the loss meter sees is a stream the measurement sees.
        // `$opus` is the packet's Opus data (for Thetis channel 0): measurement
        // mode reads its redundancy bit, and only when on (build 157).
        macro_rules! audio_arrived {
            ($stream:ident, $which:expr, $seq:expr, $ts:expr, $bytes:expr, $opus:expr) => {{
                loss_meter.$stream.packet($seq);
                measure.audio(start.elapsed().as_millis() as u64, $which, $seq, $ts, $bytes, $opus);
            }};
        }

        // An audio packet left for the server: measurement mode (when on) writes
        // it down. After the send, at every audio send site - like audio_arrived!
        // on the receive side.
        macro_rules! audio_sent {
            ($which:expr, $seq:expr, $bytes:expr) => {{
                measure.tx(start.elapsed().as_millis() as u64, $which, $seq, $bytes);
            }};
        }

        // The spectrum pan goes out twice: the coarse one an older server knows,
        // then the fine one (build 75) a newer server takes instead. One place
        // for the pair, so no send site can forget the fine one. The fine one
        // only reaches a server that knows it (send_control!).
        macro_rules! send_pan {
            ($coarse:expr, $fine:expr, $pan:expr, $addr:expr) => {{
                let __pan: f32 = $pan;
                let _ = send_control!($coarse, sdr_remote_core::protocol::pack_spectrum_pan(__pan), $addr);
                let _ = send_control!($fine, sdr_remote_core::protocol::pack_spectrum_pan_fine(__pan), $addr);
            }};
        }
        macro_rules! send_control {
            ($id:expr, $value:expr, $addr:expr) => {{
                let __id: ControlId = $id;
                if !state.server_takes(__id) {
                    // A server without the fine view cannot parse this one, so
                    // it never acknowledges it: four repeats and a "given up"
                    // in the log for nothing. Nothing sent (Ok(0)); held for
                    // the restore block, so it is not lost either.
                    held_back.hold(__id, $value);
                    Ok(0)
                } else {
                    let mut __cbuf = [0u8; ControlPacket::SIZE];
                    ControlPacket { control_id: __id, value: $value }.serialize(&mut __cbuf);
                    // Numbered here, and kept for repeating once this
                    // session's server has acknowledged a control (control_ack.rs).
                    let __numbered = controls_out.send(
                        &__cbuf,
                        Instant::now(),
                        Duration::from_millis(last_hb_ack_rtt as u64),
                    );
                    send_tx!(&__numbered, $addr)
                }
            }};
        }

        // What send_control! held back, to a server that takes it, once. Two
        // callers in the ack: in the restore block right after the replay (so
        // the hand-written sends of the current values land after it), and after
        // every ack for a session without a restore (a Connect to a server
        // without a password).
        macro_rules! send_held_back {
            ($addr:expr) => {{
                if state.server_fine_spectrum_view {
                    for (id, value) in held_back.take() {
                        let _ = send_control!(id, value, $addr);
                    }
                }
            }};
        }

        // What a transmit stream gives (`tx_stream::Outgoing`), sent as many
        // times as it says, each copy measured - for Thetis and the radios
        // alike (build 291).
        macro_rules! send_out {
            ($out:expr, $which:expr, $addr:expr) => {{
                let __out: crate::tx_stream::Outgoing = $out;
                for _ in 0..__out.copies {
                    let _ = send_tx!(&__out.bytes, $addr);
                    audio_sent!($which, __out.sequence, __out.opus_len);
                }
            }};
        }

        // Track last audio packet arrival for robust timeout detection
        let mut last_audio_received: Option<Instant> = None;

        // A Yaesu's received audio, both radios through this one block (build
        // 293; until then a copy per radio). Its
        // playout is one block too (`mix_yaesu_rx!`).
        macro_rules! yaesu_audio_in {
            ($pkt:expr, $name:literal, $jb:ident, $st:ident, $logged_first:ident, $meter:ident, $stream:expr,
             $packets:ident, $jitter:ident, $depth:ident) => {{
                let pkt = $pkt;
                // Audio arriving is a sign of life, and the timeout is an AND
                // of two independent signals so that neither alone can drop a
                // good link. Without this a Yaesu-only station (Thetis RX off,
                // radio audio on - an arrangement the manual describes) has
                // audio flowing while audio_timed_out stays true, leaving the
                // connection hanging on the heartbeat alone.
                last_audio_received = Some(Instant::now());
                // Detect stream reset (server resets seq to 0 on re-enable)
                if $logged_first && pkt.sequence == 0 {
                    info!("{}: stream reset detected, resetting jitter buffer", $name);
                    $jb.reset();
                    $st.reset();
                    // The loss window too, or the old sequence mark makes the
                    // restart read as total loss.
                    loss_meter.$meter.reset();
                }
                // This stream is measured now. It never was, while the server
                // has always used the client's figure to protect it - see
                // loss_window.
                audio_arrived!($meter, $stream, pkt.sequence, pkt.timestamp, pkt.opus_data.len(), &pkt.opus_data);
                if !$logged_first {
                    info!("{}: first audio packet (seq={}, {}B)", $name, pkt.sequence, pkt.opus_data.len());
                    $logged_first = true;
                }
                let arrival_ms = start.elapsed().as_millis() as u64;
                $jb.push(
                    BufferedFrame {
                        sequence: pkt.sequence,
                        timestamp: pkt.timestamp,
                        opus_data: pkt.opus_data,
                        ptt: false,
                        // RX bandwidth follows the Thetis toggle: the
                        // AUDIO_WIDEBAND flag determines NB (8k) or WB (16k).
                        wideband: pkt.flags.wideband(),
                    },
                    arrival_ms,
                );
                state.$packets += 1;
                state.$jitter = $jb.jitter_ms();
                state.$depth = $jb.depth() as u32;
            }};
        }

        // Audio error recovery
        let mut audio_error_since: Option<Instant> = None;
        let mut audio_retry_interval_ms: u64 = 1000;
        // When the chosen microphone was missing, it is looked for this often
        // (build 188).
        let mut mic_looked_at = Instant::now();
        // How long until the next try, doubled after each failed opening and
        // back to MIC_LOOK_EVERY after one that worked (build 190;
        // a failing opening costs 0.5-0.9 s on the engine's thread).
        let mut mic_try_every = MIC_LOOK_EVERY;

        // Input/output device names for reconnect
        let mut input_device_name = String::new();
        let mut output_device_name = String::new();

        let mut recv_buf = vec![0u8; MAX_PACKET_SIZE];
        let mut drain_buf = vec![0.0f32; RING_DRAIN_SIZE];
        let mut accum_buf = Vec::<f32>::with_capacity(capture_frame_samples * 2);
        let mut read_buf = vec![0.0f32; RING_DRAIN_SIZE];

        let mut audio_tick = interval(Duration::from_millis(20));
        let mut last_server_addr: Option<String> = None;

        // The packet right after a gap in a jitter buffer, if it is there: the one
        // whose redundancy can rebuild the lost frame. One place for all five
        // streams, Thetis included (build 156; the Thetis arm had its own copy
        // and no test looked at it).
        macro_rules! next_after_gap {
            ($jb:ident) => {
                $jb.next_seq_peek().and_then(|seq| $jb.peek_frame(seq))
            };
        }

        // One lost frame of a stream, rebuilt from the packet after the gap in
        // its own jitter buffer, or concealed (build 155). The stream and its
        // buffer go in as a pair, once per stream; a test reads the four calls.
        macro_rules! rebuild_or_conceal {
            ($st:ident, $jb:ident) => {{
                let next = next_after_gap!($jb);
                recover_or_conceal_one(&mut $st, next)
            }};
        }

        // All five receive jitter buffers back to empty, in one list (build
        // 155). There were four hand-written lists, and the one for a change of
        // sound device left out radio 2's.
        macro_rules! reset_jitter_buffers {
            () => {{
                jitter_buf.reset();
                yaesu_jitter_buf.reset();
                yaesu2_jitter_buf.reset();
                vrx1_jitter_buf.reset();
                vrx2_jitter_buf.reset();
            }};
        }

        // Re-read the audio device sample rates and rebuild every resampler + reset the
        // jitter buffers when the rate changed. Used by an explicit input/output device
        // switch AND by the spontaneous audio-error recovery below: a Bluetooth route
        // change (e.g. getting in the car) disconnects the AAudio stream, which reopens
        // on the BT-SCO device at a different rate (8/16 kHz vs 48 kHz). Without this the
        // resamplers keep targeting the old rate -> continuous rate mismatch -> choppy audio.
        macro_rules! resync_audio_rates {
            () => {{
                let new_cap = audio.capture_sample_rate();
                let new_play = audio.playback_sample_rate();
                if new_cap != capture_rate || new_play != playback_rate {
                    capture_rate = new_cap;
                    playback_rate = new_play;
                    capture_frame_samples = (capture_rate * 20 / 1000) as usize;
                    // `resampler` picks the filter from the direction: the microphone
                    // and a headset slower than the stream go down (build 260).
                    if let Ok(r) = resampler(capture_rate, NETWORK_SAMPLE_RATE_WIDEBAND, capture_frame_samples, UpFilter::Long) { resampler_in = r; }
                    if let Ok(r) = resampler(capture_rate, NETWORK_SAMPLE_RATE_WIDEBAND, capture_frame_samples, UpFilter::Long) { yaesu_tx_resampler = r; }
                    // Two statements, so neither call can be short-circuited away.
                    let eq1_moved = yaesu_eq.set_sample_rate(capture_rate as f32);
                    let eq2_moved = yaesu2_eq.set_sample_rate(capture_rate as f32);
                    if eq1_moved || eq2_moved {
                        info!("Yaesu TX EQ at the capture rate: {} Hz", capture_rate);
                    }
                    if let Ok(r) = resampler(NETWORK_SAMPLE_RATE, playback_rate, FRAME_SAMPLES, UpFilter::Short) { wav_res_out = r; }
                    if let Ok(r) = resampler(NETWORK_SAMPLE_RATE_WIDEBAND, playback_rate, FRAME_SAMPLES_WIDEBAND, UpFilter::Short) { wav_res_out_wb = r; }
                    // Every receive stream at once. This list used to be sixteen
                    // separate resamplers and it forgot the four VRX ones, which
                    // on a 44.1 kHz headset kept producing 48 kHz worth of
                    // samples - eight percent too many every second, heard as
                    // stuttering while RX1 beside it stayed clean (2026-08-14).
                    // A stream is one thing now, so this is one line per stream.
                    for s in [&mut st_rx1, &mut st_bin_r, &mut st_rx2,
                              &mut st_yaesu, &mut st_yaesu2,
                              &mut st_vrx1, &mut st_vrx2] {
                        s.set_playback_rate(playback_rate);
                    }
                    info!("Resamplers rebuilt: capture {}Hz, playback {}Hz; {}", capture_rate, playback_rate,
                        resampler_line(&resampler_in, st_rx1.resamplers()));
                }
                // Reset the jitter buffers to prevent stale frame buildup across the
                // switch - all five; radio 2's was missing here until build 155.
                reset_jitter_buffers!();
            }};
        }

        // A new audio backend in place of the old one: a device switch, the
        // recovery after an audio error (a Bluetooth headset joining reopens the
        // stream) and a missing microphone coming back. A new backend starts on
        // its defaults, and two of the settings it carries are only sent on the
        // edge of a press: the capture gate (a press held across the reopen sent
        // nothing more, and the server let the radio go after its packet window
        // while the button stayed red) and the playback mute. Both are set again
        // here. The gate only while a press is still on: a session lost in the same
        // turn of the loop has cleared the flags before the edge has seen it.
        macro_rules! take_new_audio {
            ($new:expr) => {{
                audio = $new;
                state.mic_missing = audio.missing_input();
                mic_try_every = MIC_LOOK_EVERY;
                audio.set_capture_gate_delay_ms(mic_gate_delay_ms);
                audio.set_playback_mute(playback_muted);
                resync_audio_rates!();
                if last_capture_ptt && (ptt || yaesu_ptt || yaesu2_ptt) {
                    accum_buf.clear();
                    yaesu_tx_accum.clear();
                    audio.set_capture_gate(true);
                    info!("Audio reopened during a press: the capture gate is open again");
                }
            }};
        }

        // When a receive error was last reported.
        //
        // A burst of the same error must not be able to bury the log - see the
        // handling at the recv_from arm below.
        let mut last_recv_err: Option<std::time::Instant> = None;
        // How many receive errors in a row.
        //
        // Silencing the log is not the same as stopping the spin, and the first
        // repair only did the first: a transport that is permanently dead and
        // answers something other than BrokenPipe still ran at full speed, just
        // quietly. It took the evidence away for the half of the cases it did
        // not fix.
        let mut recv_err_streak: u32 = 0;

        loop {
            // Process all pending commands (non-blocking).
            // SetFrequency / SetFrequencyRx2 are coalesced: under rapid MIDI-wheel
            // tuning the engine can see dozens of frequency commands in a single
            // drain pass; only the latest matters, so we capture it and emit one
            // UDP packet after the drain. Eliminates VFO command pile-up in
            // Thetis's TCI queue (was visible as A/B drift + late CTUN recenter
            // after the MIDI controller had already stopped).
            let mut deferred_freq: Option<u64> = None;
            let mut deferred_freq_rx2: Option<u64> = None;
            // The two Yaesu slots need the same treatment, and need it more: a
            // Thetis VFO write goes over TCI, but a Yaesu write is a CAT frame on
            // a serial link with a round trip per command. One MIDI sweep queues
            // dozens of them, the radio keeps stepping long after the knob has
            // stopped, and there is no way to call it back.
            let mut deferred_yaesu_freq: Option<u64> = None;
            let mut deferred_yaesu2_freq: Option<u64> = None;
            while let Some(cmd) = own_commands.pop().or_else(|| self.cmd_rx.try_recv().ok()) {
                match cmd {
                    Command::Connect(addr, pw) => {
                        // If we are already in
                        // a forward-progress connect-state (AwaitingTotp or Connected) and
                        // the user clicks Connect again with the SAME server+password, do
                        // not regress the status - the server's session is still alive
                        // server-side. (When this was written a Connecting-status could
                        // never have recovered, as the server did not re-issue a challenge
                        // for an existing session; since v2.13 it does, on a heartbeat at a
                        // code step two seconds old, so this is now a choice, not a
                        // necessity: the code field stays, the operator types the code.)
                        // The user must explicitly Disconnect first to start over.
                        let same_target = server_addr.as_deref() == Some(addr.as_str())
                            && auth_password == pw;
                        let already_progressing = matches!(
                            state.connect_status,
                            crate::state::ConnectStatus::AwaitingTotp
                                | crate::state::ConnectStatus::Connected
                        );
                        if same_target && already_progressing {
                            // Keep current state; no-op connect.
                            continue;
                        }

                        // If we have an
                        // existing server-side session (had passed through any state
                        // beyond Disconnected, including Failed), send a Disconnect
                        // packet to the previous address before starting a new connect.
                        // Otherwise the server's session would stay in a half-auth
                        // state (PendingTotp or similar) and never re-issue an
                        // AuthChallenge for the new attempt.
                        let needs_session_reset = !matches!(
                            state.connect_status,
                            crate::state::ConnectStatus::Disconnected
                        );
                        if needs_session_reset {
                            if let Some(ref old_addr) = server_addr {
                                let mut buf = [0u8; DisconnectPacket::SIZE];
                                DisconnectPacket::serialize(&mut buf);
                                let _ = send_tx!(&buf, old_addr.as_str());
                                // Brief settle delay so the server processes the
                                // disconnect before the new heartbeat arrives.
                                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                            }
                            // The fifth way to lose a session, found on build 82:
                            // it told the old server goodbye and carried
                            // the transmit flags to the new one.
                            session_lost!().replaced_by_a_new_session();
                        }

                        // Up-front DNS / parse check.
                        // If the address is a plain "IP:port" it parses synchronously; no DNS
                        // needed. If it has a hostname, try lookup_host once. Either failure
                        // mode produces a specific ConnectError so the UI can show a precise
                        // message instead of a generic "Disconnected".
                        // In relay mode there is no direct server IP: the address is a
                        // display label (ignored by the Relay transport). Skip the
                        // direct-IP DNS/parse check in that case (security addendum fix).
                        // A literal address goes through the same rule as a looked-up
                        // one: an IPv6 literal is no address for this socket either, and
                        // it fell through to the sender before, which failed every send
                        // in silence and reported "no reply" after the time-out.
                        // Filtered out here, it reaches the lookup below, which says
                        // what is wrong.
                        let mut resolved: Option<std::net::SocketAddr> = first_ipv4(addr.parse().ok());
                        let resolved_ok = if socket.via_relay() || resolved.is_some() {
                            true
                        } else {
                            // Async DNS lookup with a tight timeout; don't block the
                            // command-processing loop forever on a slow resolver.
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(5),
                                tokio::net::lookup_host(addr.as_str()),
                            )
                            .await
                            {
                                Ok(Ok(iter)) => {
                                    // The socket is IPv4 (bound on 0.0.0.0), so an
                                    // AAAA record first would be a dead end - and a
                                    // name with no A record at all is a failure to
                                    // say, like its two neighbours: a silent `false`
                                    // here left the screen as it was.
                                    resolved = first_ipv4(iter);
                                    if resolved.is_none() {
                                        state.connect_status =
                                            crate::state::ConnectStatus::Failed(
                                                crate::state::ConnectError::DnsResolutionFailed {
                                                    host: addr.clone(),
                                                    io_kind: std::io::ErrorKind::AddrNotAvailable,
                                                    message: "no IPv4 address for this host: this client sends over IPv4".to_string(),
                                                },
                                            );
                                    }
                                    resolved.is_some()
                                }
                                Ok(Err(io_err)) => {
                                    state.connect_status =
                                        crate::state::ConnectStatus::Failed(
                                            crate::state::ConnectError::DnsResolutionFailed {
                                                host: addr.clone(),
                                                io_kind: io_err.kind(),
                                                message: format!("{}", io_err),
                                            },
                                        );
                                    false
                                }
                                Err(_) => {
                                    // Timeout on the lookup_host call.
                                    state.connect_status =
                                        crate::state::ConnectStatus::Failed(
                                            crate::state::ConnectError::DnsResolutionFailed {
                                                host: addr.clone(),
                                                io_kind: std::io::ErrorKind::TimedOut,
                                                message: "DNS lookup timed out".to_string(),
                                            },
                                        );
                                    false
                                }
                            }
                        };

                        if resolved_ok {
                            server_sockaddr = if socket.via_relay() { None } else { resolved };
                            match server_sockaddr {
                                Some(to) => info!("Connect: QUIC to {} (typed: {})", to, addr),
                                None => info!("Connect: QUIC via the relay (label: {})", addr),
                            }
                            server_addr = Some(addr);
                            auth_password = pw;
                            _auth_completed = false;
                            connect_started_at = Some(Instant::now());
                            connect_any_reply_seen = false;
                            resumed_after_code_step = false;
                            // Signal "Connecting" so the UI can show progress.
                            // Specific failure modes (NoUdpResponse via timeout, MalformedResponse
                            // via parser, ProtocolVersionMismatch via magic+version check) are
                            // surfaced from the network paths below.
                            state.connect_status =
                                crate::state::ConnectStatus::Connecting;
                            // Broadcast immediately (2026-05-13) so the
                            // UI clears the previous Failed(WrongPassword/...) banner the
                            // moment Connect is pressed; without this the user keeps seeing
                            // "Wrong password" for several seconds until the next packet
                            // event triggers a state-broadcast.
                            let _ = self.state_tx.send(state.clone());
                        } else {
                            // DNS-resolution already set the Failed status above; leave the
                            // password unset so a retry forces a fresh attempt.
                            server_addr = None;
                            server_sockaddr = None;
                            auth_password = None;
                            connect_started_at = None;
                            // Same reasoning as above: the DNS-fail Failed state must
                            // also be broadcast immediately.
                            let _ = self.state_tx.send(state.clone());
                        }
                    }
                    Command::SendTotpCode(code) => {
                        if let Some(ref addr) = server_addr {
                            let code_bytes = code.as_bytes();
                            let mut buf = vec![0u8; 6 + code_bytes.len()];
                            let header = Header::new(PacketType::TotpResponse, Flags::NONE);
                            header.serialize(&mut buf[..4]);
                            buf[4..6].copy_from_slice(&(code_bytes.len() as u16).to_be_bytes());
                            buf[6..].copy_from_slice(code_bytes);
                            let _ = send_tx!(&buf, addr.as_str());
                            info!("TOTP code sent");
                            totp_code_sent_at = Some(Instant::now());
                        }
                    }
                    Command::Disconnect => {
                        // Send disconnect to server before clearing
                        if let Some(ref addr) = server_addr {
                            // Restore Thetis TXEQ if a WAV-playback left it bypassed,
                            // must happen while the server address is still valid.
                            if thetis_txeq_bypassed {
                                let _ = send_control!(ControlId::ThetisTxeq, 1, addr.as_str());
                                thetis_txeq_bypassed = false;
                                info!("Thetis TXEQ restored (disconnect)");
                            }
                            let mut buf = [0u8; DisconnectPacket::SIZE];
                            DisconnectPacket::serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                            info!("Disconnect (ring={}, jbuf={}, jitter={:.1}ms, rtt={}ms, loss={}%)",
                                audio.playback_buffer_level(), jitter_buf.depth(),
                                jitter_buf.jitter_ms(), last_hb_ack_rtt, loss_meter.reported());
                        }
                        server_addr = None;
                        server_sockaddr = None;
                        // All the audio streams, so Yaesu/VRX don't stall on a stale
                        // next_seq after reconnect - explicit here instead of relying
                        // only on the backjump re-baseline.
                        reset_jitter_buffers!();
                        yaesu_logged_first = false;
                        yaesu2_logged_first = false;
                        vrx1_logged_first = false;
                        vrx2_logged_first = false;
                        was_connected = false;
                        last_hb_ack_time = None;
                        last_hb_ack_rtt = 0;
                        logged_first_rx = false;
                        logged_first_tx = false;
                        rx_volume_synced = false;
                        rx2_volume_synced = false;
                        state.rx_af_gain = 0;
                        state.connect_status = crate::state::ConnectStatus::Disconnected;
                        state.rtt_ms = 0;
                        state.jitter_ms = 0.0;
                        state.buffer_depth = 0;
                        state.rx_packets = 0;
                        state.down_kbps = 0;
                        state.up_kbps = 0;
                        state.bw_breakdown.clear();
                        let forgotten = session_lost!();
                        state.link_down(forgotten);
                        // Clear stale spectrum data to prevent artifacts on reconnect
                        state.spectrum_bins.clear();
                        state.full_spectrum_bins.clear();
                        state.spectrum_sequence = 0;
                        state.full_spectrum_sequence = 0;
                        // Clear RX2 spectrum data
                        state.rx2_spectrum_bins.clear();
                        state.rx2_full_spectrum_bins.clear();
                        state.rx2_spectrum_sequence = 0;
                        state.rx2_full_spectrum_sequence = 0;
                        let _ = self.state_tx.send(state.clone());
                    }
                    Command::SetMicGateDelayMs(v) => {
                        mic_gate_delay_ms = v.min(800);
                        audio.set_capture_gate_delay_ms(mic_gate_delay_ms);
                    }
                    Command::SetPlaybackMute(v) => {
                        playback_muted = v;
                        audio.set_playback_mute(v);
                    }
                    Command::SetPtt(v) => {
                        // Releasing PTT does not release it yet when a beep is
                        // due: the tone has to travel before the transmitter
                        // stops, or nobody hears it. The release below runs
                        // when the tone has finished.
                        // Keyed again while the beep is going out: the
                        // operator has more to say and wins. Without this the
                        // tone would finish and release a PTT that is being
                        // held down.
                        // The decision lives in `roger::ptt_verdict`, where it can
                        // be tested - every one of the four faults this feature
                        // shipped with was a wrong answer here, and none was
                        // reachable from a test while it sat inline.
                        let beeping = roger_tone.as_ref().map(|(_, c, _)| *c);
                        match crate::roger::ptt_verdict(
                            &roger_cfg, beeping, 0, v, thetis_ptt, state.mode,
                        ) {
                            crate::roger::PttVerdict::Ignore => continue,
                            crate::roger::PttVerdict::HoldForBeep => {
                                info!("Roger beep: Thetis, {} Hz for {} ms; PTT held until it has gone",
                                    roger_cfg.freq_hz, roger_cfg.duration_ms);
                                roger_tone = Some((
                                    crate::roger::RogerTone::new(NETWORK_SAMPLE_RATE_WIDEBAND, &roger_cfg),
                                    0,
                                    Instant::now(),
                                ));
                                continue;
                            }
                            crate::roger::PttVerdict::Proceed => {
                                if beeping == Some(0) {
                                    roger_tone = None;
                                }
                            }
                        }
                        thetis_ptt = v;
                        ptt_requested[0] = v;
                        ptt = thetis_ptt;
                        if !v {
                            state.ptt_denial.transmitter_off(0);
                        }
                        // Thetis BIN has a side-effect on TX audio quality.
                        // Disable BIN during TX, re-enable on RX if audio_mode=BIN.
                        if audio_mode == 1 {
                            if let Some(ref addr) = server_addr {
                                let bin_val = if v { 0u16 } else { 1u16 }; // TX: off, RX: on
                                if last_sent_bin != Some(bin_val) {
                                    let _ = send_control!(ControlId::Binaural, bin_val, addr.as_str());
                                    last_sent_bin = Some(bin_val);
                                }
                            }
                        }
                    }
                    Command::SetRxVolume(v) => {
                        rx_volume = v;
                    }
                    Command::SetLocalVolume(v) => {
                        // Master gain: applied to EVERY playback channel (RX1, RX2,
                        // VRX1/2 and both Yaesu slots), not only the Thetis RX paths -
                        // otherwise "master" would silently mean "RX only". Levels are
                        // measured BEFORE this gain, so the meters keep showing the link
                        // rather than the slider.
                        local_volume = v;
                    }
                    Command::SetVfoAVolume(v) => {
                        vfo_a_volume = v;
                    }
                    Command::SetTxGain(v) => {
                        tx_gain = crate::commands::clamp_tx_gain(v);
                    }
                    Command::SetRogerBeep(cfg) => {
                        roger_cfg = cfg.clamped();
                        info!(
                            "Roger beep: {} Hz, {} ms, volume {:.2}, FM {}, channels Thetis={} radio1={} radio2={}",
                            roger_cfg.freq_hz, roger_cfg.duration_ms, roger_cfg.volume,
                            if roger_cfg.include_fm { "included" } else { "excluded" },
                            roger_cfg.on_thetis, roger_cfg.on_radio1, roger_cfg.on_radio2,
                        );
                    }
                    Command::SetPlayVolume(v) => {
                        play_volume = v.clamp(0.0, 4.0);
                    }
                    Command::SetAgcEnabled(enabled) => {
                        agc_enabled = enabled;
                        state.agc_enabled = enabled;
                        info!("TX AGC: {}", if enabled { "ON" } else { "OFF" });
                    }
                    Command::SetFrequency(hz) => {
                        deferred_freq = Some(hz);
                    }
                    Command::SetMode(mode) => {
                        if let Some(ref addr) = server_addr {
                            let pkt = ModePacket { mode };
                            let mut buf = [0u8; ModePacket::SIZE];
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                        state.mode = mode;
                    }
                    Command::SetRelayOnly(on) => match &relay_only_switch {
                        Some(switch) => {
                            switch.store(on, std::sync::atomic::Ordering::Relaxed);
                            if on {
                                info!("Relay only (measurement screen): on; no direct path is offered");
                            } else {
                                info!("Relay only (measurement screen): off");
                            }
                            state.relay_only = on;
                            measure.event(start.elapsed().as_millis() as u64, crate::measure::Event::RelayOnly(on));
                        }
                        None => info!("Relay only (measurement screen): no relay in use; nothing to switch"),
                    },
                    Command::SetForceTcp(on) => match &force_tcp_switch {
                        Some(switch) => {
                            switch.store(on, std::sync::atomic::Ordering::Relaxed);
                            if on {
                                info!("Force TCP (measurement screen): on; all traffic over the relay's TCP line");
                            } else {
                                info!("Force TCP (measurement screen): off");
                            }
                            state.force_tcp = on;
                            measure.event(start.elapsed().as_millis() as u64, crate::measure::Event::ForceTcp(on));
                        }
                        None => info!("Force TCP (measurement screen): no relay in use; nothing to switch"),
                    },
                    Command::MeasureNetwork(sample) => {
                        measure.record(crate::measure::Record::Net { mono_ms: start.elapsed().as_millis() as u64, sample });
                    }
                    Command::SetMeasurement(dir) => {
                        match dir {
                            Some(dir) => {
                                let label = sdr_remote_core::version_string();
                                match measure.start(std::path::Path::new(&dir), start, &label) {
                                    Ok(path) => {
                                        // Where things stand at the start: every
                                        // transmitter that is keyed now - the next loop writes its
                                        // "on", so a release later always has its key in the file.
                                        let now = start.elapsed().as_millis() as u64;
                                        // The session that is running, and its way:
                                        // the line a session would have written had the
                                        // measurement been on when it began.
                                        if was_connected {
                                            measure.event(
                                                now,
                                                crate::measure::Event::SessionStart(socket.route()),
                                            );
                                        }
                                        // And relay only, when it stands (build 137): a
                                        // comparison reads which half of the file it is in.
                                        if state.relay_only {
                                            measure.event(now, crate::measure::Event::RelayOnly(true));
                                        }
                                        if state.force_tcp {
                                            measure.event(now, crate::measure::Event::ForceTcp(true));
                                        }
                                        // And the way the traffic takes now (build 139).
                                        if let Some(kind) = state.path {
                                            measure.event(now, crate::measure::Event::Path(kind));
                                        }
                                        ptt_seen = crate::measure::PttSeen::default();
                                        info!("Measurement on: {}", path.display());
                                        state.measurement_file = Some(path.display().to_string());
                                    }
                                    Err(e) => {
                                        warn!("Measurement could not start in {}: {}", dir, e);
                                        state.measurement_file = None;
                                    }
                                }
                            }
                            None => {
                                let path = measure.path().map(|p| p.display().to_string());
                                // A write error in the last drain is said, not logged as
                                // a clean stop: the file ends at the error.
                                if measure.stop(start) {
                                    warn!("Measurement stopped with a write error: the file ends there: {}", path.unwrap_or_default());
                                } else if let Some(p) = path {
                                    info!("Measurement off: {}", p);
                                }
                                state.measurement_file = None;
                            }
                        }
                    }
                    Command::SetControl(id, value) => {
                        if let Some(ref addr) = server_addr {
                            // RX2's FFT size as RX1's: the receiver's for every
                            // client, so live only from one that looks, as in the
                            // restore block.
                            let looks = id != ControlId::Rx2SpectrumFftSize
                                || (was_connected && state.rx2_spectrum_enabled);
                            if looks {
                                let _ = send_control!(id, value, addr.as_str());
                            }
                        }
                        // Track RX2 FFT size locally for reconnect
                        if id == ControlId::Rx2SpectrumFftSize {
                            rx2_spectrum_fft_size_k = value;
                        }
                        // Track audio mode for per-channel volume
                        if id == ControlId::AudioMode {
                            audio_mode = value;
                        }
                        // Slot 0 goes through the generic control; slot 1, VRX1 and
                        // VRX2 have commands of their own further down.
                        if id == ControlId::YaesuEnable {
                            yaesu_wanted = value != 0;
                            // Same as the main path: keeping the decoder history
                            // across a switch-off lets a gap in a later session be
                            // concealed from audio that belonged to this one. The
                            // server does start its sequence over when the last
                            // subscriber leaves, and the seq==0 arm below resets on
                            // that - but only on that edge, so do not depend on it.
                            if !yaesu_wanted { st_yaesu.reset(); }
                        }
                        // Locally update power state immediately so UI reflects the
                        // change even if the server is unreachable (e.g. after ZZBY shutdown).
                        // Note: value=2 is shutdown (ZZBY), NOT power on.
                        // Suppress server power broadcasts briefly to prevent stale
                        // power_on=true from overriding our local state.
                        if id == ControlId::PowerOnOff {
                            state.power_on = value == 1;
                            power_suppress_until = Instant::now() + Duration::from_secs(5);
                            let _ = self.state_tx.send(state.clone());
                        }
                    }
                    Command::SetInputDevice(name) => {
                        if name != input_device_name {
                            input_device_name = name;
                            let in_name = if input_device_name.is_empty() { None } else { Some(input_device_name.as_str()) };
                            let out_name = if output_device_name.is_empty() { None } else { Some(output_device_name.as_str()) };
                            match audio_factory(in_name, out_name) {
                                Ok(new_audio) => {
                                    take_new_audio!(new_audio);
                                    info!("Audio input device switched to {:?}", in_name.unwrap_or("(default)"));
                                    state.audio_error = false;
                                    audio_error_since = None;
                                }
                                Err(e) => {
                                    warn!("Failed to switch audio input device: {}", e);
                                    // The old audio stays, with another microphone than
                                    // the chosen one: refuse to transmit on it
                                    // (build 189).
                                    if !input_device_name.is_empty() {
                                        warn!("Microphone '{}' could not be opened: transmitting is refused", input_device_name);
                                        state.mic_missing = Some(input_device_name.clone());
                                        mic_looked_at = Instant::now();
                                        mic_try_every = MIC_LOOK_EVERY;
                                    }
                                }
                            }
                        }
                    }
                    Command::SetOutputDevice(name) => {
                        if name != output_device_name {
                            output_device_name = name;
                            let in_name = if input_device_name.is_empty() { None } else { Some(input_device_name.as_str()) };
                            let out_name = if output_device_name.is_empty() { None } else { Some(output_device_name.as_str()) };
                            match audio_factory(in_name, out_name) {
                                Ok(new_audio) => {
                                    take_new_audio!(new_audio);
                                    info!("Audio output device switched to {:?}", out_name.unwrap_or("(default)"));
                                    state.audio_error = false;
                                    audio_error_since = None;
                                }
                                Err(e) => {
                                    warn!("Failed to switch audio output device: {}", e);
                                }
                            }
                        }
                    }
                    Command::EnableSpectrum(enabled) => {
                        let was_open = spectrum_enabled;
                        spectrum_enabled = enabled;
                        if let Some(ref addr) = server_addr {
                            if was_connected {
                                // The wish is remembered above; what goes on
                                // the wire also has to respect the saving.
                                // Same reasoning as Rx1Enable below.
                                let on = crate::thetis_streams::stream_value(
                                    enabled, thetis_streams_stopped);
                                let _ = send_control!(ControlId::SpectrumEnable, on as u16, addr.as_str());
                                // A spectrum that opens gets the size its rows show.
                                if on && !was_open && spectrum_fft_size_k != 0 {
                                    let _ = send_control!(ControlId::SpectrumFftSize, spectrum_fft_size_k, addr.as_str());
                                }
                            }
                        }
                    }
                    Command::SetSpectrumFps(fps) => {
                        spectrum_fps = fps;
                        if let Some(ref addr) = server_addr {
                            if was_connected {
                                let _ = send_control!(ControlId::SpectrumFps, fps as u16, addr.as_str());
                            }
                        }
                    }
                    Command::SetSpectrumZoom(zoom) => {
                        spectrum_zoom = zoom;
                        if let Some(ref addr) = server_addr {
                            if was_connected {
                                let _ = send_control!(ControlId::SpectrumZoom, (zoom * 10.0) as u16, addr.as_str());
                            }
                        }
                    }
                    Command::SetSpectrumPan(pan) => {
                        spectrum_pan = pan;
                        // Said on this side too, so a report shows both ends of the
                        // same value: sent here, received there. A pan that moves the
                        // scale but not the trace is one of those two going missing,
                        // and until now neither end wrote it down.
                        log::info!("Spectrum pan sent: {:+.6}", pan);
                        if let Some(ref addr) = server_addr {
                            if was_connected {
                                send_pan!(ControlId::SpectrumPan, ControlId::SpectrumPanFine, pan, addr.as_str());
                            }
                        }
                    }
                    Command::SetSpectrumMaxBins(max_bins) => {
                        spectrum_max_bins = max_bins;
                        if let Some(ref addr) = server_addr {
                            if was_connected {
                                let _ = send_control!(ControlId::SpectrumMaxBins, max_bins, addr.as_str());
                            }
                        }
                    }
                    Command::SetSpectrumFftSize(size_k) => {
                        spectrum_fft_size_k = size_k;
                        if let Some(ref addr) = server_addr {
                            // The receiver's size, for every client: only from one
                            // that looks (the phone asks for 128K on every connect,
                            // also one that changes nothing).
                            if was_connected && crate::thetis_streams::stream_value(
                                spectrum_enabled, thetis_streams_stopped) {
                                let _ = send_control!(ControlId::SpectrumFftSize, size_k, addr.as_str());
                            }
                        }
                    }
                    Command::SetAmplitecSwitchA(pos) => {
                        if let Some(ref addr) = server_addr {
                            let pkt = EquipmentCommandPacket {
                                device_type: DeviceType::Amplitec6x2,
                                command_id: EquipmentCommandPacket::CMD_SET_SWITCH_A,
                                data: vec![pos],
                            };
                            let mut buf = Vec::with_capacity(8);
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::SetAmplitecSwitchB(pos) => {
                        if let Some(ref addr) = server_addr {
                            let pkt = EquipmentCommandPacket {
                                device_type: DeviceType::Amplitec6x2,
                                command_id: EquipmentCommandPacket::CMD_SET_SWITCH_B,
                                data: vec![pos],
                            };
                            let mut buf = Vec::with_capacity(8);
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::TunerTune => {
                        if let Some(ref addr) = server_addr {
                            let pkt = EquipmentCommandPacket {
                                device_type: DeviceType::Tuner,
                                command_id: CMD_TUNE_START,
                                data: vec![],
                            };
                            let mut buf = Vec::with_capacity(8);
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::TunerAbort => {
                        if let Some(ref addr) = server_addr {
                            let pkt = EquipmentCommandPacket {
                                device_type: DeviceType::Tuner,
                                command_id: CMD_TUNE_ABORT,
                                data: vec![],
                            };
                            let mut buf = Vec::with_capacity(8);
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::SpeOperate | Command::SpeTune | Command::SpeAntenna
                    | Command::SpeInput | Command::SpePower | Command::SpeBandUp
                    | Command::SpeBandDown | Command::SpeOff | Command::SpePowerOn
                    | Command::SpeDriveDown | Command::SpeDriveUp => {
                        if let Some(ref addr) = server_addr {
                            let cmd_id = match cmd {
                                Command::SpeOperate => CMD_SPE_OPERATE,
                                Command::SpeTune => CMD_SPE_TUNE,
                                Command::SpeAntenna => CMD_SPE_ANTENNA,
                                Command::SpeInput => CMD_SPE_INPUT,
                                Command::SpePower => CMD_SPE_POWER,
                                Command::SpeBandUp => CMD_SPE_BAND_UP,
                                Command::SpeBandDown => CMD_SPE_BAND_DOWN,
                                Command::SpeOff => CMD_SPE_OFF,
                                Command::SpePowerOn => CMD_SPE_POWER_ON,
                                Command::SpeDriveDown => CMD_SPE_DRIVE_DOWN,
                                Command::SpeDriveUp => CMD_SPE_DRIVE_UP,
                                _ => unreachable!(),
                            };
                            let pkt = EquipmentCommandPacket {
                                device_type: DeviceType::SpeExpert,
                                command_id: cmd_id,
                                data: vec![],
                            };
                            let mut buf = Vec::with_capacity(8);
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::Rf2kOperate(on) => {
                        if let Some(ref addr) = server_addr {
                            let pkt = EquipmentCommandPacket {
                                device_type: DeviceType::Rf2k,
                                command_id: CMD_RF2K_OPERATE,
                                data: vec![on as u8],
                            };
                            let mut buf = Vec::with_capacity(8);
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::Rf2kTunerMode(mode) => {
                        if let Some(ref addr) = server_addr {
                            let pkt = EquipmentCommandPacket {
                                device_type: DeviceType::Rf2k,
                                command_id: CMD_RF2K_TUNER_MODE,
                                data: vec![mode],
                            };
                            let mut buf = Vec::with_capacity(8);
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::Rf2kTunerBypass(on) => {
                        if let Some(ref addr) = server_addr {
                            let pkt = EquipmentCommandPacket {
                                device_type: DeviceType::Rf2k,
                                command_id: CMD_RF2K_TUNER_BYPASS,
                                data: vec![on as u8],
                            };
                            let mut buf = Vec::with_capacity(8);
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::Rf2kTune | Command::Rf2kAnt1 | Command::Rf2kAnt2
                    | Command::Rf2kAnt3 | Command::Rf2kAnt4 | Command::Rf2kAntExt
                    | Command::Rf2kErrorReset | Command::Rf2kClose
                    | Command::Rf2kDriveUp | Command::Rf2kDriveDown
                    | Command::Rf2kTunerReset | Command::Rf2kTunerStore
                    | Command::Rf2kTunerLUp | Command::Rf2kTunerLDown
                    | Command::Rf2kTunerCUp | Command::Rf2kTunerCDown
                    | Command::Rf2kTunerK
                    | Command::Rf2kFrqDelayUp | Command::Rf2kFrqDelayDown
                    | Command::Rf2kAutotuneThresholdUp | Command::Rf2kAutotuneThresholdDown
                    | Command::Rf2kDacAlcUp | Command::Rf2kDacAlcDown
                    | Command::Rf2kZeroFRAM => {
                        if let Some(ref addr) = server_addr {
                            let cmd_id = match cmd {
                                Command::Rf2kTune => CMD_RF2K_TUNE,
                                Command::Rf2kAnt1 => CMD_RF2K_ANT1,
                                Command::Rf2kAnt2 => CMD_RF2K_ANT2,
                                Command::Rf2kAnt3 => CMD_RF2K_ANT3,
                                Command::Rf2kAnt4 => CMD_RF2K_ANT4,
                                Command::Rf2kAntExt => CMD_RF2K_ANT_EXT,
                                Command::Rf2kErrorReset => CMD_RF2K_ERROR_RESET,
                                Command::Rf2kClose => CMD_RF2K_CLOSE,
                                Command::Rf2kDriveUp => CMD_RF2K_DRIVE_UP,
                                Command::Rf2kDriveDown => CMD_RF2K_DRIVE_DOWN,
                                Command::Rf2kTunerReset => CMD_RF2K_TUNER_RESET,
                                Command::Rf2kTunerStore => CMD_RF2K_TUNER_STORE,
                                Command::Rf2kTunerLUp => CMD_RF2K_TUNER_L_UP,
                                Command::Rf2kTunerLDown => CMD_RF2K_TUNER_L_DOWN,
                                Command::Rf2kTunerCUp => CMD_RF2K_TUNER_C_UP,
                                Command::Rf2kTunerCDown => CMD_RF2K_TUNER_C_DOWN,
                                Command::Rf2kTunerK => CMD_RF2K_TUNER_K,
                                Command::Rf2kFrqDelayUp => CMD_RF2K_FRQ_DELAY_UP,
                                Command::Rf2kFrqDelayDown => CMD_RF2K_FRQ_DELAY_DOWN,
                                Command::Rf2kAutotuneThresholdUp => CMD_RF2K_AUTOTUNE_THRESH_UP,
                                Command::Rf2kAutotuneThresholdDown => CMD_RF2K_AUTOTUNE_THRESH_DOWN,
                                Command::Rf2kDacAlcUp => CMD_RF2K_DAC_ALC_UP,
                                Command::Rf2kDacAlcDown => CMD_RF2K_DAC_ALC_DOWN,
                                Command::Rf2kZeroFRAM => CMD_RF2K_ZERO_FRAM,
                                _ => unreachable!(),
                            };
                            let pkt = EquipmentCommandPacket {
                                device_type: DeviceType::Rf2k,
                                command_id: cmd_id,
                                data: vec![],
                            };
                            let mut buf = Vec::with_capacity(8);
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::Rf2kSetHighPower(on) => {
                        if let Some(ref addr) = server_addr {
                            let pkt = EquipmentCommandPacket {
                                device_type: DeviceType::Rf2k,
                                command_id: CMD_RF2K_SET_HIGH_POWER,
                                data: vec![on as u8],
                            };
                            let mut buf = Vec::with_capacity(8);
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::Rf2kSetTuner6m(on) => {
                        if let Some(ref addr) = server_addr {
                            let pkt = EquipmentCommandPacket {
                                device_type: DeviceType::Rf2k,
                                command_id: CMD_RF2K_SET_TUNER_6M,
                                data: vec![on as u8],
                            };
                            let mut buf = Vec::with_capacity(8);
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::Rf2kSetBandGap(on) => {
                        if let Some(ref addr) = server_addr {
                            let pkt = EquipmentCommandPacket {
                                device_type: DeviceType::Rf2k,
                                command_id: CMD_RF2K_SET_BAND_GAP,
                                data: vec![on as u8],
                            };
                            let mut buf = Vec::with_capacity(8);
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::Rf2kSetDriveConfig(change) => {
                        // Code and payload come from the one place the server
                        // decodes with (`rf2k_drive`, build 205-207).
                        let encoded = sdr_remote_core::rf2k_drive::encode(change);
                        if encoded.is_none() {
                            warn!("RF2K-S drive change for row {} has no command; not sent", change.category);
                        }
                        if let (Some(ref addr), Some((cmd_id, data))) = (server_addr.as_ref(), encoded) {
                            let pkt = EquipmentCommandPacket {
                                device_type: DeviceType::Rf2k,
                                command_id: cmd_id,
                                data,
                            };
                            let mut buf = Vec::with_capacity(10);
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::UbRetract => {
                        if let Some(ref addr) = server_addr {
                            let pkt = EquipmentCommandPacket {
                                device_type: DeviceType::UltraBeam,
                                command_id: CMD_UB_RETRACT,
                                data: vec![],
                            };
                            let mut buf = Vec::with_capacity(8);
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::UbSetFrequency(khz, direction) => {
                        if let Some(ref addr) = server_addr {
                            let pkt = EquipmentCommandPacket {
                                device_type: DeviceType::UltraBeam,
                                command_id: CMD_UB_SET_FREQ,
                                data: vec![(khz & 0xFF) as u8, ((khz >> 8) & 0xFF) as u8, direction],
                            };
                            let mut buf = Vec::with_capacity(10);
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::UbReadElements => {
                        if let Some(ref addr) = server_addr {
                            let pkt = EquipmentCommandPacket {
                                device_type: DeviceType::UltraBeam,
                                command_id: CMD_UB_READ_ELEMENTS,
                                data: vec![],
                            };
                            let mut buf = Vec::with_capacity(8);
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::UbModifyElement(index, length_mm) => {
                        if let Some(ref addr) = server_addr {
                            let pkt = EquipmentCommandPacket {
                                device_type: DeviceType::UltraBeam,
                                command_id: CMD_UB_MODIFY_ELEMENT,
                                data: vec![index, (length_mm & 0xFF) as u8, ((length_mm >> 8) & 0xFF) as u8],
                            };
                            let mut buf = Vec::with_capacity(10);
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::RotorGoTo(angle) => {
                        if let Some(ref addr) = server_addr {
                            let pkt = EquipmentCommandPacket {
                                device_type: DeviceType::Rotor,
                                command_id: CMD_ROTOR_GOTO,
                                data: angle.to_le_bytes().to_vec(),
                            };
                            let mut buf = Vec::with_capacity(10);
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::RotorStop => {
                        if let Some(ref addr) = server_addr {
                            let pkt = EquipmentCommandPacket {
                                device_type: DeviceType::Rotor,
                                command_id: CMD_ROTOR_STOP,
                                data: vec![],
                            };
                            let mut buf = Vec::with_capacity(8);
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::RotorCw => {
                        if let Some(ref addr) = server_addr {
                            let pkt = EquipmentCommandPacket {
                                device_type: DeviceType::Rotor,
                                command_id: CMD_ROTOR_CW,
                                data: vec![],
                            };
                            let mut buf = Vec::with_capacity(8);
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::RotorCcw => {
                        if let Some(ref addr) = server_addr {
                            let pkt = EquipmentCommandPacket {
                                device_type: DeviceType::Rotor,
                                command_id: CMD_ROTOR_CCW,
                                data: vec![],
                            };
                            let mut buf = Vec::with_capacity(8);
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::ServerReboot => {
                        if let Some(ref addr) = server_addr {
                            let pkt = EquipmentCommandPacket {
                                device_type: DeviceType::RemoteServer,
                                command_id: sdr_remote_core::protocol::CMD_SERVER_REBOOT,
                                data: vec![],
                            };
                            let mut buf = Vec::with_capacity(8);
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                            info!("Server reboot request sent");
                        }
                    }
                    Command::ServerShutdown => {
                        if let Some(ref addr) = server_addr {
                            let pkt = EquipmentCommandPacket {
                                device_type: DeviceType::RemoteServer,
                                command_id: sdr_remote_core::protocol::CMD_SERVER_SHUTDOWN,
                                data: vec![],
                            };
                            let mut buf = Vec::with_capacity(8);
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                            info!("Server shutdown request sent");
                        }
                    }
                    Command::SetSmeterSource(source) => {
                        // Translate the 0/1/2 source choice into the per-RX bitmap
                        // expected by the server (one bit per RX × source). We apply
                        // the same choice to both RX1 (bits 0-2) and RX2 (bits 4-6).
                        let mask: u16 = match source {
                            0 => 0x11, // Sig: bit 0 (RX1) + bit 4 (RX2)
                            1 => 0x22, // Avg: bit 1 + bit 5  (default)
                            2 => 0x44, // MaxBin: bit 2 + bit 6
                            _ => 0x22,
                        };
                        smeter_source = source;
                        if let Some(ref addr) = server_addr {
                            let _ = send_control!(ControlId::SmeterSources, mask, addr.as_str());
                        }
                    }
                    Command::StartRecording { rx1, rx2, yaesu, yaesu2, vrx1, vrx2, path } => {
                        use std::path::Path;
                        let base = Path::new(&path);
                        let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
                        state.last_recorded.clear();
                        // WAV rate is set by the first write_samples() call; each
                        // source can therefore follow NB/WB decoder rate independently.
                        if rx1 {
                            let p = base.join(format!("RX1_{}.wav", ts));
                            match crate::wav::WavWriter::new(&p) {
                                Ok(w) => {
                                    info!("Recording RX1 to {}", p.display());
                                    state.last_recorded.push(("RX1".to_string(), p.to_string_lossy().to_string()));
                                    rec_rx1 = Some(w);
                                }
                                Err(e) => warn!("Failed to start RX1 recording: {}", e),
                            }
                        }
                        if rx2 {
                            let p = base.join(format!("RX2_{}.wav", ts));
                            match crate::wav::WavWriter::new(&p) {
                                Ok(w) => {
                                    info!("Recording RX2 to {}", p.display());
                                    state.last_recorded.push(("RX2".to_string(), p.to_string_lossy().to_string()));
                                    rec_rx2 = Some(w);
                                }
                                Err(e) => warn!("Failed to start RX2 recording: {}", e),
                            }
                        }
                        if yaesu {
                            let p = base.join(format!("Yaesu1_{}.wav", ts));
                            match crate::wav::WavWriter::new(&p) {
                                Ok(w) => {
                                    info!("Recording Yaesu radio 1 to {}", p.display());
                                    state.last_recorded.push(("Radio 1".to_string(), p.to_string_lossy().to_string()));
                                    rec_yaesu = Some(w);
                                }
                                Err(e) => warn!("Failed to start Yaesu radio 1 recording: {}", e),
                            }
                        }
                        if yaesu2 {
                            let p = base.join(format!("Yaesu2_{}.wav", ts));
                            match crate::wav::WavWriter::new(&p) {
                                Ok(w) => {
                                    info!("Recording Yaesu radio 2 to {}", p.display());
                                    state.last_recorded.push(("Radio 2".to_string(), p.to_string_lossy().to_string()));
                                    rec_yaesu2 = Some(w);
                                }
                                Err(e) => warn!("Failed to start Yaesu radio 2 recording: {}", e),
                            }
                        }
                        if vrx1 {
                            let p = base.join(format!("VRX1_{}.wav", ts));
                            match crate::wav::WavWriter::new(&p) {
                                Ok(w) => {
                                    info!("Recording VRX1 to {}", p.display());
                                    state.last_recorded.push(("VRX1".to_string(), p.to_string_lossy().to_string()));
                                    rec_vrx1 = Some(w);
                                }
                                Err(e) => warn!("Failed to start VRX1 recording: {}", e),
                            }
                        }
                        if vrx2 {
                            let p = base.join(format!("VRX2_{}.wav", ts));
                            match crate::wav::WavWriter::new(&p) {
                                Ok(w) => {
                                    info!("Recording VRX2 to {}", p.display());
                                    state.last_recorded.push(("VRX2".to_string(), p.to_string_lossy().to_string()));
                                    rec_vrx2 = Some(w);
                                }
                                Err(e) => warn!("Failed to start VRX2 recording: {}", e),
                            }
                        }
                        state.recording = rx1 || rx2 || yaesu || yaesu2 || vrx1 || vrx2;
                    }
                    Command::StopRecording => {
                        if let Some(w) = rec_rx1.take() {
                            let dur = w.duration_secs();
                            if let Err(e) = w.finalize() { warn!("RX1 WAV finalize error: {}", e); }
                            else { info!("RX1 recording stopped ({:.1}s)", dur); }
                        }
                        if let Some(w) = rec_rx2.take() {
                            let dur = w.duration_secs();
                            if let Err(e) = w.finalize() { warn!("RX2 WAV finalize error: {}", e); }
                            else { info!("RX2 recording stopped ({:.1}s)", dur); }
                        }
                        if let Some(w) = rec_yaesu.take() {
                            let dur = w.duration_secs();
                            if let Err(e) = w.finalize() { warn!("Yaesu radio 1 WAV finalize error: {}", e); }
                            else { info!("Yaesu radio 1 recording stopped ({:.1}s)", dur); }
                        }
                        if let Some(w) = rec_yaesu2.take() {
                            let dur = w.duration_secs();
                            if let Err(e) = w.finalize() { warn!("Yaesu radio 2 WAV finalize error: {}", e); }
                            else { info!("Yaesu radio 2 recording stopped ({:.1}s)", dur); }
                        }
                        if let Some(w) = rec_vrx1.take() {
                            let dur = w.duration_secs();
                            if let Err(e) = w.finalize() { warn!("VRX1 WAV finalize error: {}", e); }
                            else { info!("VRX1 recording stopped ({:.1}s)", dur); }
                        }
                        if let Some(w) = rec_vrx2.take() {
                            let dur = w.duration_secs();
                            if let Err(e) = w.finalize() { warn!("VRX2 WAV finalize error: {}", e); }
                            else { info!("VRX2 recording stopped ({:.1}s)", dur); }
                        }
                        state.recording = false;
                    }
                    Command::PlayRecording { paths } => {
                        // Everything after the first, loaded first, so a file
                        // that will not read leaves the others playing instead
                        // of turning a working playback into nothing.
                        playback_extra.clear();
                        for extra in paths.iter().skip(1) {
                            match crate::wav::read_wav(std::path::Path::new(extra)) {
                                Ok((rate, samples)) if matches!(rate, NETWORK_SAMPLE_RATE | NETWORK_SAMPLE_RATE_WIDEBAND) => {
                                    let nb = resampler(NETWORK_SAMPLE_RATE, playback_rate, FRAME_SAMPLES, UpFilter::Long);
                                    let wb = resampler(NETWORK_SAMPLE_RATE_WIDEBAND, playback_rate, FRAME_SAMPLES_WIDEBAND, UpFilter::Long);
                                    match (nb, wb) {
                                        (Ok(res_nb), Ok(res_wb)) => {
                                            info!("Playback: also playing {} ({:.1}s, {} Hz)",
                                                extra, samples.len() as f32 / rate.max(1) as f32, rate);
                                            playback_extra.push(ExtraPlayback { samples, rate, pos: 0, res_nb, res_wb });
                                        }
                                        _ => warn!("Playback: no resampler for {}; left out", extra),
                                    }
                                }
                                Ok((other, _)) => warn!("Playback: {} has an unsupported rate ({} Hz); left out", extra, other),
                                Err(e) => warn!("Playback: {} could not be read ({}); left out", extra, e),
                            }
                        }
                        let path = paths.first().cloned().unwrap_or_default();
                        match crate::wav::read_wav(std::path::Path::new(&path)) {
                            Ok((rate, samples)) => match rate {
                                NETWORK_SAMPLE_RATE | NETWORK_SAMPLE_RATE_WIDEBAND => {
                                    info!("Playback: loaded {} ({:.1}s, {} Hz, {} samples)",
                                        path, samples.len() as f32 / rate.max(1) as f32, rate, samples.len());
                                    playback_wav = Some(samples);
                                    playback_wav_rate = rate;
                                    playback_pos = 0;
                                    wav_to_thetis = None;
                                    wav_to_yaesu = None;
                                    wav_thetis_accum.clear();
                                    playback_is_tx = ptt || yaesu_ptt || yaesu2_ptt;
                                    state.playing = true;
                                }
                                other => {
                                    // TL records only 8 k / 16 k. Reject other rates rather
                                    // than misplay them at the wrong speed.
                                    warn!("Playback: unsupported WAV sample rate {} Hz (only 8000/16000 Hz supported); not played", other);
                                }
                            },
                            Err(e) => warn!("Failed to load WAV: {}", e),
                        }
                    }
                    Command::StopPlayback => {
                        playback_wav = None;
                        playback_pos = 0;
                        playback_extra.clear();
                        playback_is_tx = false;
                        state.playing = false;
                        info!("Playback stopped");
                    }
                    Command::SetFullSpectrumEnabled(enabled) => {
                        state.full_spectrum_enabled = enabled;
                        if !enabled {
                            // Drop what is already buffered: without fresh rows the
                            // waterfall would otherwise keep redrawing one frozen
                            // full-band row. The centre/span stay - the VRX tuning
                            // limits are derived from them.
                            state.full_spectrum_bins.clear();
                            state.rx2_full_spectrum_bins.clear();
                        }
                        if let Some(ref addr) = server_addr {
                            let _ = send_control!(ControlId::FullSpectrumEnabled, enabled as u16, addr.as_str());
                            info!("Full-spectrum row enable sent: {}", enabled);
                        }
                    }
                    Command::SetDxSpotsEnabled(enabled) => {
                        state.dx_spots_enabled = enabled;
                        if let Some(ref addr) = server_addr {
                            let _ = send_control!(ControlId::DxSpotsEnabled, enabled as u16, addr.as_str());
                            info!("DX spots enable sent: {}", enabled);
                        }
                        if !enabled {
                            // Clear the local UI cache so old spots don't
                            // linger after opt-out.
                            state.dx_spots.clear();
                        }
                    }
                    Command::SetThetisWidebandAudio(on) => {
                        if let Some(ref addr) = server_addr {
                            let _ = send_control!(ControlId::ThetisWidebandAudio, on as u16, addr.as_str());
                            info!("Thetis wideband audio sent: {}", on);
                        }
                    }
                    Command::SetYaesuIsListeningRadio(on) => {
                        // Only the edge starts the clock: a screen that repeats
                        // its state would otherwise keep pushing the stop away.
                        if on != yaesu_listening {
                            yaesu_listening = on;
                            yaesu_listening_since = if on { Some(Instant::now()) } else { None };
                            info!("Listening radio: {}", if on { "Yaesu" } else { "Thetis" });
                        }
                    }
                    Command::SetRx1Enabled(enabled) => {
                        state.rx1_enabled = enabled;
                        // Switching off ends this stream. Keeping the decoder history
                        // would let a gap minutes later be concealed from audio that
                        // belongs to another listening session. The cost is that a gap
                        // shortly after switching on is silent instead of sounding like
                        // the band - deliberate; the table in `StreamDecoder::conceal`
                        // measures how long that lasts.
                        if !enabled { st_rx1.reset(); st_bin_r.reset(); }
                        if let Some(ref addr) = server_addr {
                            // Not the bare wish. While the grace period has this
                            // stream given up to save data, handing it back here
                            // would defeat the saving silently - and the tick
                            // would not notice, because its flag still says
                            // stopped. Stricter than the one
                            // it follows: that one healed itself on the next
                            // reconnect, this one would not.
                            let _ = send_control!(ControlId::Rx1Enable, crate::thetis_streams::stream_value(
                                    enabled, thetis_streams_stopped) as u16, addr.as_str());
                            info!("RX1 enable sent: {}", enabled);
                        }
                    }
                    // RX2 / VFO-B commands
                    Command::SetRx2Enabled(enabled) => {
                        if !enabled { st_rx2.reset(); }
                        state.rx2_enabled = enabled;
                        if let Some(ref addr) = server_addr {
                            let _ = send_control!(ControlId::Rx2Enable, enabled as u16, addr.as_str());
                            info!("RX2 enable sent: {}", enabled);
                        }
                    }
                    Command::SetMultiTx(on) => {
                        if multi_tx != on {
                            info!("Multi-TX {}", if on { "on: the same microphone to every keyed Yaesu" } else { "off: one transmitter at a time" });
                        }
                        multi_tx = on;
                    }
                    Command::SetYaesuVolume(v) => {
                        if (yaesu_volume - v).abs() > 0.001 {
                            log::info!("Yaesu volume -> {:.3} (local_volume {:.3})", v, local_volume);
                        }
                        yaesu_volume = v;
                    }
                    Command::SetYaesuEqBand(band, gain_db) => {
                        yaesu_eq.set_band_gain(band as usize, gain_db);
                    }
                    Command::SetYaesuEqBandParams(band, params) => {
                        yaesu_eq.set_band(band as usize, params);
                    }
                    Command::SetYaesuEqEnabled(on) => {
                        yaesu_eq.set_enabled(on);
                        info!("Yaesu EQ: {}", if on { "ON" } else { "OFF" });
                    }
                    Command::SetYaesuCompressor(level) => {
                        // Client-side speech compressor radio 1 (0-100 → 0.0-1.0).
                        if yaesu_compressor.set_amount(level as f32 / 100.0) {
                            info!("Yaesu compressor: {}", level);
                        }
                    }
                    // Not logged here: a drag sends a new value every frame. The
                    // client logs the timing when the slider is let go, and once
                    // per session with the rest of the chain.
                    Command::SetYaesuCompTiming(attack_ms, release_ms) => {
                        yaesu_compressor.set_timing(attack_ms, release_ms);
                    }
                    Command::SetYaesu2CompTiming(attack_ms, release_ms) => {
                        yaesu2_compressor.set_timing(attack_ms, release_ms);
                    }
                    Command::SetYaesuTxAgc(on) => {
                        yaesu_tx_agc_enabled = on;
                        info!("Yaesu TX AGC: {}", if on { "ON" } else { "OFF" });
                    }
                    Command::SetYaesu2Compressor(level) => {
                        // Client-side speech compressor radio 2 (FTX-1).
                        if yaesu2_compressor.set_amount(level as f32 / 100.0) {
                            info!("Yaesu2 compressor: {}", level);
                        }
                    }
                    Command::SetYaesu2TxAgc(on) => {
                        yaesu2_tx_agc_enabled = on;
                        info!("Yaesu2 TX AGC: {}", if on { "ON" } else { "OFF" });
                    }
                    Command::SetYaesuFreq(hz) => {
                        if server_addr.is_some() {
                            deferred_yaesu_freq = Some(hz);
                        } else {
                            warn!("Yaesu freq -> {} Hz DROPPED: not connected (server_addr=None)", hz);
                        }
                    }
                    Command::SetYaesuMenu(menu_key, p2_value) => {
                        if let Some(ref addr) = server_addr {
                            // Send menu data as YaesuMemoryData packet with "SETMENU:" prefix
                            let text = format!("SETMENU:{}:{}", menu_key, p2_value);
                            let text_bytes = text.as_bytes();
                            let mut send_buf = Vec::with_capacity(6 + text_bytes.len());
                            let header = sdr_remote_core::protocol::Header::new(
                                sdr_remote_core::protocol::PacketType::YaesuMemoryData,
                                sdr_remote_core::protocol::Flags::NONE);
                            let mut hdr_buf = [0u8; 4];
                            header.serialize(&mut hdr_buf);
                            send_buf.extend_from_slice(&hdr_buf);
                            send_buf.extend_from_slice(&(text_bytes.len() as u16).to_be_bytes());
                            send_buf.extend_from_slice(text_bytes);
                            let _ = send_tx!(&send_buf, addr.as_str());
                        }
                    }
                    Command::RequestServerReport => {
                        if let Some(ref addr) = server_addr {
                            state.server_report = None;
                            state.server_report_failed = None;
                            server_report_parts = Vec::new();
                            // Nothing has arrived yet, so nothing knows how many
                            // parts to expect; the deadline is set here so a
                            // request that is never answered at all still ends.
                            server_report_deadline =
                                Some(Instant::now() + Duration::from_secs(20));
                            let header = sdr_remote_core::protocol::Header::new(
                                sdr_remote_core::protocol::PacketType::ServerReportRequest,
                                sdr_remote_core::protocol::Flags::NONE);
                            let mut buf = [0u8; 4];
                            header.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::WriteYaesuMemories(tab_text) => {
                        if let Some(ref addr) = server_addr {
                            // Send tab data as YaesuMemoryData packet
                            let text_bytes = tab_text.as_bytes();
                            let mut send_buf = Vec::with_capacity(6 + text_bytes.len());
                            let header = sdr_remote_core::protocol::Header::new(
                                sdr_remote_core::protocol::PacketType::YaesuMemoryData,
                                sdr_remote_core::protocol::Flags::NONE);
                            let mut hdr_buf = [0u8; 4];
                            header.serialize(&mut hdr_buf);
                            send_buf.extend_from_slice(&hdr_buf);
                            send_buf.extend_from_slice(&(text_bytes.len() as u16).to_be_bytes());
                            send_buf.extend_from_slice(text_bytes);
                            let _ = send_tx!(&send_buf, addr.as_str());
                            // Then trigger the write
                            let _ = send_control!(ControlId::YaesuWriteMemories, 0, addr.as_str());
                        }
                    }
                    Command::WriteYaesu2Memories(tab_text) => {
                        // Same for radio 2: YaesuMemoryData2 packet + Yaesu2WriteMemories trigger.
                        if let Some(ref addr) = server_addr {
                            let text_bytes = tab_text.as_bytes();
                            let mut send_buf = Vec::with_capacity(6 + text_bytes.len());
                            let header = sdr_remote_core::protocol::Header::new(
                                sdr_remote_core::protocol::PacketType::YaesuMemoryData2,
                                sdr_remote_core::protocol::Flags::NONE);
                            let mut hdr_buf = [0u8; 4];
                            header.serialize(&mut hdr_buf);
                            send_buf.extend_from_slice(&hdr_buf);
                            send_buf.extend_from_slice(&(text_bytes.len() as u16).to_be_bytes());
                            send_buf.extend_from_slice(text_bytes);
                            let _ = send_tx!(&send_buf, addr.as_str());
                            let _ = send_control!(ControlId::Yaesu2WriteMemories, 0, addr.as_str());
                        }
                    }
                    Command::SetYaesuMode(mode) => {
                        if let Some(ref addr) = server_addr {
                            let _ = send_control!(ControlId::YaesuMode, mode as u16, addr.as_str());
                        }
                    }
                    Command::SetYaesuControl(slot, control, value) => {
                        // Typed Yaesu DSP/function control.
                        if let Some(ref addr) = server_addr {
                            let pkt = YaesuControlPacket { slot, control, value };
                            let mut buf = [0u8; YaesuControlPacket::SIZE];
                            pkt.serialize(&mut buf);
                            // Numbered and repeated like a control: it was
                            // the one control with no repair at all - not in the
                            // session replay, and only the radio's read-back shows
                            // that it did not arrive.
                            let numbered = controls_out.send(
                                &buf,
                                Instant::now(),
                                Duration::from_millis(last_hb_ack_rtt as u64),
                            );
                            let _ = send_tx!(&numbered, addr.as_str());
                        }
                    }
                    Command::SetYaesu2Menu(addr_str, value) => {
                        // FTX-1 EX-set: travels as YaesuMemoryData2 with "SETMENU:" prefix
                        // (mirrors the 991A SetYaesuMenu path, but 6-digit address).
                        if let Some(ref addr) = server_addr {
                            let text = format!("SETMENU:{}:{}", addr_str, value);
                            let text_bytes = text.as_bytes();
                            let mut send_buf = Vec::with_capacity(6 + text_bytes.len());
                            let header = sdr_remote_core::protocol::Header::new(
                                sdr_remote_core::protocol::PacketType::YaesuMemoryData2,
                                sdr_remote_core::protocol::Flags::NONE);
                            let mut hdr_buf = [0u8; 4];
                            header.serialize(&mut hdr_buf);
                            send_buf.extend_from_slice(&hdr_buf);
                            send_buf.extend_from_slice(&(text_bytes.len() as u16).to_be_bytes());
                            send_buf.extend_from_slice(text_bytes);
                            let _ = send_tx!(&send_buf, addr.as_str());
                        }
                    }
                    Command::SetYaesuPtt(on) => {
                        // The decision lives in `roger::ptt_verdict`, where it can
                        // be tested - every one of the four faults this feature
                        // shipped with was a wrong answer here, and none was
                        // reachable from a test while it sat inline.
                        let beeping = roger_tone.as_ref().map(|(_, c, _)| *c);
                        match crate::roger::ptt_verdict(
                            &roger_cfg, beeping, 1, on, yaesu_ptt, state.yaesu_mode,
                        ) {
                            crate::roger::PttVerdict::Ignore => continue,
                            crate::roger::PttVerdict::HoldForBeep => {
                                info!("Roger beep: radio 1, {} Hz for {} ms; PTT held until it has gone",
                                    roger_cfg.freq_hz, roger_cfg.duration_ms);
                                roger_tone = Some((
                                    crate::roger::RogerTone::new(NETWORK_SAMPLE_RATE_WIDEBAND, &roger_cfg),
                                    1,
                                    Instant::now(),
                                ));
                                continue;
                            }
                            crate::roger::PttVerdict::Proceed => {
                                if beeping == Some(1) {
                                    roger_tone = None;
                                }
                            }
                        }
                        yaesu_ptt = on;
                        ptt_requested[1] = on;
                        // Letting go clears a standing refusal, exactly as the
                        // Thetis path already did. Without this the flag was set
                        // from the Yaesu side and cleared only from the Thetis
                        // side, so a refused Yaesu press left the button orange
                        // and "TX in use" for good - no press on either client
                        // could take it off.
                        if !on {
                            state.ptt_denial.transmitter_off(1);
                        }
                        // The flag rides on this radio's packets from the next
                        // audio tick, as Thetis's (build 292); mic capture opens
                        // through the shared delayed gate below.
                    }
                    Command::SetYaesuTxGain(v) => {
                        // Local Yaesu mic gain (applied before Opus encoding).
                        // UI display 0.5 maps to the empirically matched internal 0.2.
                        yaesu_local_mic_gain = crate::commands::clamp_yaesu_mic_gain(v);
                    }
                    // --- Dual-radio slot 1 commands ---
                    Command::SetYaesu2Enable(on) => {
                        // Outside the guard, same reason as VRX1 above.
                        if !on { st_yaesu2.reset(); }
                        if let Some(ref addr) = server_addr {
                            yaesu2_wanted = on;
                            let _ = send_control!(ControlId::Yaesu2Enable, on as u16, addr.as_str());
                            info!("[radio1] enable sent: {}", on);
                        }
                    }
                    Command::SetYaesu2Volume(v) => {
                        yaesu2_volume = v;
                    }
                    Command::SetYaesu2Ptt(on) => {
                        // The decision lives in `roger::ptt_verdict`, where it can
                        // be tested - every one of the four faults this feature
                        // shipped with was a wrong answer here, and none was
                        // reachable from a test while it sat inline.
                        let beeping = roger_tone.as_ref().map(|(_, c, _)| *c);
                        match crate::roger::ptt_verdict(
                            &roger_cfg, beeping, 2, on, yaesu2_ptt, state.yaesu2_mode,
                        ) {
                            crate::roger::PttVerdict::Ignore => continue,
                            crate::roger::PttVerdict::HoldForBeep => {
                                info!("Roger beep: radio 2, {} Hz for {} ms; PTT held until it has gone",
                                    roger_cfg.freq_hz, roger_cfg.duration_ms);
                                roger_tone = Some((
                                    crate::roger::RogerTone::new(NETWORK_SAMPLE_RATE_WIDEBAND, &roger_cfg),
                                    2,
                                    Instant::now(),
                                ));
                                continue;
                            }
                            crate::roger::PttVerdict::Proceed => {
                                if beeping == Some(2) {
                                    roger_tone = None;
                                }
                            }
                        }
                        yaesu2_ptt = on;
                        ptt_requested[2] = on;
                        // Same as slot 0 above.
                        if !on {
                            state.ptt_denial.transmitter_off(2);
                        }
                    }
                    Command::SetYaesu2Freq(hz) => {
                        if server_addr.is_some() {
                            deferred_yaesu2_freq = Some(hz);
                        }
                    }
                    Command::SetYaesu2Mode(mode) => {
                        if let Some(ref addr) = server_addr {
                            let _ = send_control!(ControlId::Yaesu2Mode, mode as u16, addr.as_str());
                        }
                    }
                    Command::SetYaesu2TxGain(v) => {
                        // Own mic gain for radio 2 (applied when transmitting on slot 1).
                        yaesu2_local_mic_gain = crate::commands::clamp_yaesu_mic_gain(v);
                    }
                    Command::SetYaesu2EqBand(band, gain_db) => {
                        yaesu2_eq.set_band_gain(band as usize, gain_db);
                    }
                    Command::SetYaesu2EqBandParams(band, params) => {
                        yaesu2_eq.set_band(band as usize, params);
                    }
                    Command::SetYaesu2EqEnabled(on) => {
                        yaesu2_eq.set_enabled(on);
                    }
                    Command::SetVrxEnabled(on) => {
                        // Outside the server_addr guard, like RX1/RX2: switching a
                        // channel off has to forget that decoder whether or not there
                        // is a server to tell about it.
                        if !on { st_vrx1.reset(); }
                        if let Some(ref addr) = server_addr {
                            // Still inside the guard, unlike the reset above: moving it
                            // changes when a subscription is sent, which is the path
                            // behind the VRX start-up trouble. The cost is that
                            // switching off while disconnected leaves this true, so the
                            // concealment gate is open for a channel the operator turned
                            // off - harmless only because the reset above just emptied
                            // that decoder. The gate is correct by its neighbour, not by
                            // itself.
                            vrx1_wanted = on;
                            let _ = send_control!(ControlId::VrxEnable, on as u16, addr.as_str());
                        }
vrx1_enable_at = if on { Some(Instant::now()) } else { None };
                                                // Re-baseline like a reconnect does. Enabling a channel makes
                        // the server build a fresh runtime that starts its sequence
                        // over, while this buffer still expects the numbers from the
                        // previous subscription - so the new frames read as "too late"
                        // and are dropped until the sequence climbs back. That is the
                        // silence of several seconds after switching a VRX on.
                        vrx1_jitter_buf.reset();
                        vrx1_logged_first = false;
                    }
                    Command::SetVrxMode(mode) => {
                        if let Some(ref addr) = server_addr {
                            let _ = send_control!(ControlId::VrxMode, mode as u16, addr.as_str());
                        }
                    }
                    Command::SetVrxFrequency(hz) => {
                        if let Some(ref addr) = server_addr {
                            let pkt = VrxFrequencyPacket { vrx_id: 0, frequency_hz: hz };
                            let mut buf = [0u8; VrxFrequencyPacket::SIZE];
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::SetVrxVolume(v) => {
                        vrx1_volume = v.max(0.0);
                    }
                    Command::SetVrx2Enabled(on) => {
                        // Outside the guard, same reason as VRX1 above.
                        if !on { st_vrx2.reset(); }
                        if let Some(ref addr) = server_addr {
                            vrx2_wanted = on;
                            let _ = send_control!(ControlId::VrxEnable2, on as u16, addr.as_str());
                        }
vrx2_enable_at = if on { Some(Instant::now()) } else { None };
                                                // Same re-baseline as VRX1 - see there.
                        vrx2_jitter_buf.reset();
                        vrx2_logged_first = false;
                    }
                    Command::SetVrx2Mode(mode) => {
                        if let Some(ref addr) = server_addr {
                            let _ = send_control!(ControlId::VrxMode2, mode as u16, addr.as_str());
                        }
                    }
                    Command::SetVrx2Frequency(hz) => {
                        if let Some(ref addr) = server_addr {
                            let pkt = VrxFrequencyPacket { vrx_id: 1, frequency_hz: hz };
                            let mut buf = [0u8; VrxFrequencyPacket::SIZE];
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::SetVrx2Volume(v) => {
                        vrx2_volume = v.max(0.0);
                    }
                    Command::SetVrxRateMode(mode) => {
                        if let Some(ref addr) = server_addr {
                            let _ = send_control!(ControlId::VrxAudioRate, mode as u16, addr.as_str());
                        }
                    }
                    Command::SetVrxRateMode2(mode) => {
                        if let Some(ref addr) = server_addr {
                            let _ = send_control!(ControlId::VrxAudioRate2, mode as u16, addr.as_str());
                        }
                    }
                    Command::SetVrxAutoTune(vrx_id, on) => {
                        if let Some(ref addr) = server_addr {
                            let id = if vrx_id == 0 { ControlId::VrxSamAutoTune } else { ControlId::VrxSamAutoTune2 };
                            let _ = send_control!(id, on as u16, addr.as_str());
                        }
                    }
                    Command::SetVrxFilter(vrx_id, low_hz, high_hz) => {
                        if let Some(ref addr) = server_addr {
                            let (lo_id, hi_id) = if vrx_id == 0 {
                                (ControlId::VrxFilterLow, ControlId::VrxFilterHigh)
                            } else {
                                (ControlId::VrxFilterLow2, ControlId::VrxFilterHigh2)
                            };
                            let _ = send_control!(lo_id, low_hz as i16 as u16, addr.as_str());
                            let _ = send_control!(hi_id, high_hz as i16 as u16, addr.as_str());
                        }
                    }
                    Command::SetTxFilter(low_hz, high_hz) => {
                        if let Some(ref addr) = server_addr {
                            let _ = send_control!(ControlId::TxFilterLow, low_hz as i16 as u16, addr.as_str());
                            let _ = send_control!(ControlId::TxFilterHigh, high_hz as i16 as u16, addr.as_str());
                        }
                    }
                    Command::SetVrxSpectrumPan(vrx_id, offset_hz) => {
                        if let Some(ref addr) = server_addr {
                            let _ = send_control!(if vrx_id == 0 {
                                    ControlId::VrxSpectrumPan
                                } else {
                                    ControlId::VrxSpectrumPan2
                                }, sdr_remote_core::protocol::pack_vrx_pan(offset_hz), addr.as_str());
                        }
                    }
                    Command::SetVrxHighResSpectrum(vrx_id, enabled, span_hz) => {
                        if let Some(ref addr) = server_addr {
                            let (en_id, span_id, fine_id) = if vrx_id == 0 {
                                (ControlId::VrxSpectrumEnable, ControlId::VrxSpectrumSpanKhz, ControlId::VrxSpectrumSpanFine)
                            } else {
                                (ControlId::VrxSpectrumEnable2, ControlId::VrxSpectrumSpanKhz2, ControlId::VrxSpectrumSpanFine2)
                            };
                            let _ = send_control!(en_id, if enabled { 1 } else { 0 }, addr.as_str());
                            if enabled && span_hz > 0 {
                                // Whole kHz for an older server, then the fine
                                // width a newer one takes instead (build 75).
                                let _ = send_control!(span_id, sdr_remote_core::protocol::vrx_span_khz(span_hz), addr.as_str());
                                let _ = send_control!(fine_id, sdr_remote_core::protocol::pack_vrx_span_fine(span_hz), addr.as_str());
                            }
                            // The VRX is cut from its receiver's spectrum: one that
                            // opens gets the size its receiver's rows show, as an
                            // RX spectrum does. Once per open: a zoom step sends
                            // this command too.
                            let (fft_id, fft_k) = if vrx_id == 0 {
                                (ControlId::SpectrumFftSize, spectrum_fft_size_k)
                            } else {
                                (ControlId::Rx2SpectrumFftSize, rx2_spectrum_fft_size_k)
                            };
                            let sent = &mut vrx_fft_sent[(vrx_id as usize).min(1)];
                            if !enabled {
                                *sent = false;
                            } else if was_connected && !*sent && fft_k != 0 {
                                let _ = send_control!(fft_id, fft_k, addr.as_str());
                                *sent = true;
                            }
                        }
                    }
                    Command::SetMonitor(on) => {
                        state.mon_on = on;
                        if let Some(ref addr) = server_addr {
                            let _ = send_control!(ControlId::MonitorOn, on as u16, addr.as_str());
                        }
                    }
                    Command::ThetisTune(on) => {
                        if let Some(ref addr) = server_addr {
                            let _ = send_control!(ControlId::ThetisTune, on as u16, addr.as_str());
                        }
                    }
                    Command::SetThetisAutostart(on) => {
                        thetis_autostart_enabled = on;
                    }
                    Command::SetCwTarget(t) => {
                        let t = t.min(2);
                        if t != cw_target {
                            // The key now goes elsewhere: the run no longer asks
                            // for the old radio, so its refusal comes down.
                            let old = 1u8 << cw_target;
                            state.ptt_denial.cw_run_over(cw_asked & old, ptt_requested);
                            cw_asked &= !old;
                        }
                        cw_target = t;
                    }
                    Command::SetCwHang(ms) => {
                        cw_hang_ms = ms;
                    }
                    Command::CwEdge { down, prev_len_ms, run_start } => {
                        cw_last_edge = Some(Instant::now());
                        cw_asked |= 1u8 << cw_target;
                        if let Some(ref addr) = server_addr {
                            use sdr_remote_core::protocol::{CwEdge, CwEdgesPacket};
                            cw_seq = cw_seq.wrapping_add(1);
                            if cw_recent.len() == CwEdgesPacket::MAX_EDGES {
                                cw_recent.pop_front();
                            }
                            cw_recent.push_back(CwEdge { seq: cw_seq, down, prev_len_ms, run_start });
                            let pkt = CwEdgesPacket { target: cw_target, edges: cw_recent.iter().copied().collect(), hang_ms: cw_hang_ms };
                            let mut buf = [0u8; CwEdgesPacket::MAX_SIZE];
                            let n = pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf[..n], addr.as_str());
                        }
                    }
                    Command::SetVfoSync(enabled) => {
                        state.vfo_sync = enabled;
                        if let Some(ref addr) = server_addr {
                            let _ = send_control!(ControlId::VfoSync, enabled as u16, addr.as_str());
                        }
                    }
                    Command::SetFrequencyRx2(hz) => {
                        deferred_freq_rx2 = Some(hz);
                    }
                    Command::SetModeRx2(mode) => {
                        if let Some(ref addr) = server_addr {
                            let pkt = ModePacket { mode };
                            let mut buf = [0u8; ModePacket::SIZE];
                            pkt.serialize_as_type(&mut buf, PacketType::ModeRx2);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                        state.mode_rx2 = mode;
                    }
                    Command::SetRx2Volume(v) => {
                        rx2_volume = v;
                        rx2_volume_user_changed = true;
                    }
                    Command::SetVfoBVolume(v) => {
                        vfo_b_volume = v;
                    }
                    Command::EnableRx2Spectrum(enabled) => {
                        let was_open = state.rx2_spectrum_enabled;
                        state.rx2_spectrum_enabled = enabled;
                        if let Some(ref addr) = server_addr {
                            let _ = send_control!(ControlId::Rx2SpectrumEnable, enabled as u16, addr.as_str());
                            info!("RX2 spectrum enable sent: {}", enabled);
                            // As RX1's: a spectrum that opens gets the size its rows show.
                            if was_connected && enabled && !was_open && rx2_spectrum_fft_size_k != 0 {
                                let _ = send_control!(ControlId::Rx2SpectrumFftSize, rx2_spectrum_fft_size_k, addr.as_str());
                            }
                        }
                    }
                    Command::SetRx2SpectrumZoom(zoom) => {
                        rx2_spectrum_zoom = zoom;
                        if let Some(ref addr) = server_addr {
                            if was_connected {
                                let _ = send_control!(ControlId::Rx2SpectrumZoom, (zoom * 10.0) as u16, addr.as_str());
                            }
                        }
                    }
                    Command::SetRx2SpectrumPan(pan) => {
                        rx2_spectrum_pan = pan;
                        if let Some(ref addr) = server_addr {
                            if was_connected {
                                send_pan!(ControlId::Rx2SpectrumPan, ControlId::Rx2SpectrumPanFine, pan, addr.as_str());
                            }
                        }
                    }
                }
            }

            // Emit coalesced VFO-A / VFO-B frequency, if any commands accumulated.
            if let Some(hz) = deferred_freq.take() {
                if !state.vfo_lock {
                    if let Some(ref addr) = server_addr {
                        let pkt = FrequencyPacket { frequency_hz: hz };
                        let mut buf = [0u8; FrequencyPacket::SIZE];
                        pkt.serialize(&mut buf);
                        let _ = send_tx!(&buf, addr.as_str());
                    }
                    state.frequency_hz = hz;
                    pending_freq = Some(hz);
                    pending_freq_time = Some(Instant::now());
                }
            }
            if let Some(hz) = deferred_freq_rx2.take() {
                if !state.rx2_vfo_lock {
                    if let Some(ref addr) = server_addr {
                        let pkt = FrequencyPacket { frequency_hz: hz };
                        let mut buf = [0u8; FrequencyPacket::SIZE];
                        pkt.serialize_as_type(&mut buf, PacketType::FrequencyRx2);
                        let _ = send_tx!(&buf, addr.as_str());
                    }
                    state.frequency_rx2_hz = hz;
                    pending_freq_rx2 = Some(hz);
                    pending_freq_rx2_time = Some(Instant::now());
                }
            }

            // Same for the two Yaesu slots: only the last frequency of this drain
            // pass reaches the radio, so a fast sweep costs one CAT write instead
            // of a queue that outlives the gesture.
            if let Some(hz) = deferred_yaesu_freq.take() {
                if let Some(ref addr) = server_addr {
                    let pkt = FrequencyPacket { frequency_hz: hz };
                    let mut buf = [0u8; FrequencyPacket::SIZE];
                    pkt.serialize_as_type(&mut buf, PacketType::FrequencyYaesu);
                    let _ = send_tx!(&buf, addr.as_str());
                }
            }
            if let Some(hz) = deferred_yaesu2_freq.take() {
                if let Some(ref addr) = server_addr {
                    let pkt = FrequencyPacket { frequency_hz: hz };
                    let mut buf = [0u8; FrequencyPacket::SIZE];
                    pkt.serialize_as_type(&mut buf, PacketType::FrequencyYaesu2);
                    let _ = send_tx!(&buf, addr.as_str());
                }
            }

            // Detect disconnect from outside (addr went None without Disconnect cmd)
            let current_addr = server_addr.clone();
            if current_addr.is_none() && last_server_addr.is_some() {
                // All the per-stream audio jitter buffers. Previously only the RX
                // buffer was reset here, so after a server restart the Yaesu
                // buffers kept a stale (high) next_seq and dropped the fresh
                // low-sequence stream as "too late" - audio stayed silent for
                // minutes (until the server sequence climbed back) or until a manual
                // client reconnect. Re-baseline all streams on disconnect.
                reset_jitter_buffers!();
                was_connected = false;
                last_hb_ack_time = None;
                last_hb_ack_rtt = 0;
                logged_first_rx = false;
                logged_first_tx = false;
                yaesu_logged_first = false;
                yaesu2_logged_first = false;
                vrx1_logged_first = false;
                vrx2_logged_first = false;
                rx_volume_synced = false;
                rx2_volume_synced = false;
                state.rx_af_gain = 0;
                // The link is gone, so nothing is outstanding any more. Three of
                // the four link-down paths did not do this, and each one left the
                // "PTT blocked" sign standing over a connection that no longer
                // exists.
                let forgotten = session_lost!();
                state.link_down(forgotten);
                state.rtt_ms = 0;
                state.jitter_ms = 0.0;
                state.buffer_depth = 0;
                state.rx_packets = 0;
                state.yaesu_audio_packets = 0;
                state.yaesu_jitter_ms = 0.0;
                state.yaesu_buffer_depth = 0;
                state.yaesu2_audio_packets = 0;
                state.yaesu2_jitter_ms = 0.0;
                state.yaesu2_buffer_depth = 0;
                state.vrx1_audio_packets = 0;
                state.vrx1_jitter_ms = 0.0;
                state.vrx1_buffer_depth = 0;
                state.vrx2_audio_packets = 0;
                state.vrx2_jitter_ms = 0.0;
                state.vrx2_buffer_depth = 0;
                state.down_kbps = 0;
                state.up_kbps = 0;
                state.bw_breakdown.clear();
                // Clear stale spectrum data to prevent artifacts on reconnect
                state.spectrum_bins.clear();
                state.full_spectrum_bins.clear();
                state.spectrum_sequence = 0;
                state.full_spectrum_sequence = 0;
                let _ = self.state_tx.send(state.clone());
            }
            last_server_addr = current_addr;

            // Measurement mode: a PTT edge, whichever place just set the flag.
            ptt_seen.follow(
                crate::measure::PttSeen { thetis: thetis_ptt, radio1: yaesu_ptt, radio2: yaesu2_ptt },
                start.elapsed().as_millis() as u64,
                &mut measure,
            );

            tokio::select! {
                result = socket.recv_from(&mut recv_buf) => {
                    if server_addr.is_none() {
                        continue;
                    }

                    let (len, from) = match result {
                        Ok(r) => {
                            recv_err_streak = 0;
                            r
                        }
                        Err(e) => {
                            // A transport that is gone never delivers again, so
                            // asking it once more is a spin, not a retry.
                            //
                            // This used to `continue` on every error. When the
                            // relay tunnel closes it answers BrokenPipe every
                            // single time, and the loop then ran as fast as the
                            // processor allowed with a log line per turn: 1.2
                            // million lines in a quarter of an hour on a
                            // test phone, which rolled the ring buffer and
                            // took every other line with it. The loop that made
                            // the fault also destroyed the evidence for it
                            // (2026-09-05).
                            if e.kind() == std::io::ErrorKind::BrokenPipe {
                                warn!("receive path is gone ({e}): ending the network loop");
                                break;
                            }
                            // Everything else may be temporary and must not end
                            // the session. On Windows an ICMP port-unreachable
                            // comes back as ConnectionReset on the next recv,
                            // which happens routinely while a server restarts.
                            // Reported at most once a second.
                            let now = std::time::Instant::now();
                            let quiet = last_recv_err
                                .map(|t| now.duration_since(t) < std::time::Duration::from_secs(1))
                                .unwrap_or(false);
                            if !quiet {
                                last_recv_err = Some(now);
                                warn!("recv_from error: {} (x{})", e, recv_err_streak + 1);
                            }
                            recv_err_streak = recv_err_streak.saturating_add(1);
                            // A run of them is not a hiccup any more. Back off so
                            // a dead transport cannot burn a core and a battery
                            // while it fails; ten in a row is far more than a
                            // server restart produces.
                            if recv_err_streak > 10 {
                                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            }
                            continue;
                        }
                    };
                    // Only the server we connected to is listened to (direct mode;
                    // over the relay the transport already names it). The client
                    // used to read anything that reached its port, and answered
                    // any AuthChallenge from anywhere - HMACs over nonces of a
                    // stranger's choosing, and a broken authentication in flight.
                    // Said once per five minutes.
                    if let Some(expected) = server_sockaddr {
                        if from != expected {
                            if stray_said_at.map_or(true, |t| t.elapsed() > Duration::from_secs(300)) {
                                stray_said_at = Some(Instant::now());
                                warn!("Datagram from {} dropped: not the server ({})", from, expected);
                            }
                            continue;
                        }
                    }
                    bw_rx_bytes += len as u64;
                    if len >= 3 {
                        bw_by_type[recv_buf[2] as usize] = bw_by_type[recv_buf[2] as usize].wrapping_add(len as u64);
                    }
                    let data = &recv_buf[..len];

                    match Packet::deserialize(data) {
                        Ok(Packet::Audio(pkt)) => {
                            if !logged_first_rx {
                                info!("RX: first audio packet received (seq={}, {}B)", pkt.sequence, pkt.opus_data.len());
                                logged_first_rx = true;
                            }

                            last_audio_received = Some(Instant::now());
                            audio_arrived!(thetis, crate::loss_window::Stream::Thetis, pkt.sequence, pkt.timestamp, pkt.opus_data.len(), &pkt.opus_data);

                            // Wrap legacy mono Opus as single-channel blob (CH0=RX1)
                            let mut blob = Vec::with_capacity(4 + pkt.opus_data.len());
                            blob.push(1u8); // 1 channel
                            blob.push(0u8); // CH0 = RX1
                            blob.extend_from_slice(&(pkt.opus_data.len() as u16).to_be_bytes());
                            blob.extend_from_slice(&pkt.opus_data);

                            let arrival_ms = start.elapsed().as_millis() as u64;
                            jitter_buf.push(
                                BufferedFrame {
                                    sequence: pkt.sequence,
                                    timestamp: pkt.timestamp,
                                    opus_data: blob,
                                    ptt: false,
                                    wideband: pkt.flags.wideband(),
                                },
                                arrival_ms,
                            );

                            state.rx_packets += 1;
                            state.jitter_ms = jitter_buf.jitter_ms();
                            state.buffer_depth = jitter_buf.depth() as u32;
                        }
                        Ok(Packet::HeartbeatAck(ack)) => {
                            let now_ms = start.elapsed().as_millis() as u32;
                            let rtt = now_ms.wrapping_sub(ack.echo_time);
                            last_hb_ack_rtt = rtt.min(u16::MAX as u32) as u16;
                            measure.rtt(now_ms as u64, rtt);
                            last_hb_ack_time = Some(Instant::now());

                            state.rtt_ms = last_hb_ack_rtt;

                            // What the server thinks we are subscribed to, held
                            // against what we want. MEASUREMENT ONLY: nothing is
                            // corrected here on purpose. The repair - sending the
                            // difference back - is not built until this line has
                            // shown that the difference is real and when it
                            // appears (2026-08-16, onderzoek reconnect).
                            //
                            // Only on a change, so a disagreement is one line and
                            // not one a second.
                            state.server_subs = ack.subs;
                            if let Some(theirs) = ack.subs {
                                use sdr_remote_core::protocol::SubscriptionMask as M;
                                let mut ours = M::default();
                                ours.set(M::RX1_AUDIO, state.rx1_enabled);
                                ours.set(M::RX2_AUDIO, state.rx2_enabled);
                                ours.set(M::RX2_SPECTRUM, state.rx2_spectrum_enabled);
                                ours.set(M::FULL_SPECTRUM, state.full_spectrum_enabled);
                                ours.set(M::DX_SPOTS, state.dx_spots_enabled);
                                // Only the bits this side actually knows about:
                                // VRX and Yaesu live in the desktop UI, and
                                // comparing them here would report a difference
                                // that means nothing.
                                const KNOWN: u16 = M::RX1_AUDIO | M::RX2_AUDIO
                                    | M::RX2_SPECTRUM | M::FULL_SPECTRUM | M::DX_SPOTS;
                                let differ = (ours.0 ^ theirs.0) & KNOWN;
                                if differ != last_subs_differ {
                                    last_subs_differ = differ;
                                    if differ == 0 {
                                        info!("subscriptions agree again");
                                    } else {
                                        warn!(
                                            "subscriptions disagree on {:?}: we want {:#06x}, the server has {:#06x}",
                                            M::names_of(differ), ours.0 & KNOWN, theirs.0 & KNOWN
                                        );
                                    }
                                }
                            }

                            // Which build answered. All ThetisLink binaries of a
                            // release are meant to be the same one, and without
                            // something that looks, that is an agreement nobody
                            // checks. It only ever reports: nothing here refuses
                            // to work, because being locked out of your own
                            // station at a distance is worse than a mismatch.
                            let server_build =
                                sdr_remote_core::protocol::PeerBuild::of(ack.version);
                            if let Some((bad, line)) =
                                server_build.change_line(&last_server_build, "The server")
                            {
                                if bad {
                                    warn!("{}", line);
                                } else {
                                    info!("{}", line);
                                }
                            }
                            last_server_build = server_build.clone();
                            state.server_build = server_build;
                            // Before the restore below, which sends the fine pan
                            // and width only to a server that says it takes them.
                            state.server_fine_spectrum_view = ack.capabilities.has(
                                sdr_remote_core::protocol::Capabilities::FINE_SPECTRUM_VIEW,
                            );

                            // Only trust state_flags
                            // when the server explicitly advertises REPORTS_STATE_FLAGS.
                            // Old servers (without state flags, e.g. v2.0.0 release tag) leave
                            // both capabilities and state_flags at NONE; interpreting
                            // an absent flag as "TCI down" would false-positive against
                            // a perfectly-working old server.
                            let server_reports_state_flags = ack.capabilities.has(
                                sdr_remote_core::protocol::Capabilities::REPORTS_STATE_FLAGS,
                            );
                            if server_reports_state_flags {
                                let thetis_configured = ack.state_flags.has(
                                    sdr_remote_core::protocol::ServerStateFlags::THETIS_CONFIGURED,
                                );
                                let tci_up = ack.state_flags.has(
                                    sdr_remote_core::protocol::ServerStateFlags::TCI_CONNECTED,
                                );
                                let thetis_proc_running = ack.state_flags.has(
                                    sdr_remote_core::protocol::ServerStateFlags::THETIS_RUNNING,
                                );
                                let report = crate::thetis_autostart::ThetisReport::from_flags(
                                    crate::thetis_autostart::ThetisFlags {
                                        configured: thetis_configured,
                                        starting: ack.state_flags.has(sdr_remote_core::protocol::ServerStateFlags::THETIS_STARTING),
                                        tci_up,
                                        process_running: thetis_proc_running,
                                    },
                                );
                                if thetis_autostart.step(thetis_autostart_enabled, report) {
                                    info!("Thetis autostart: the server's first report after connecting says Thetis is not running; sending power-on");
                                    own_commands.push(Command::SetControl(ControlId::PowerOnOff, 1));
                                }
                                // Suppress TciUnreachable
                                // while the server is in the launch phase (orange Start button).
                                // Showing "TCI not reachable" during the normal 60s startup
                                // grace period is wrong; the launch is still in progress.
                                let thetis_starting_now = ack.state_flags.has(
                                    sdr_remote_core::protocol::ServerStateFlags::THETIS_STARTING,
                                );
                                state.thetis_configured = thetis_configured;
                                // Single-receiver radios advertise SINGLE_RECEIVER;
                                // absent = normal 2-RX default (also for old servers,
                                // which never reach this branch anyway).
                                state.rx2_present = !ack.state_flags.has(
                                    sdr_remote_core::protocol::ServerStateFlags::SINGLE_RECEIVER,
                                );
                                // Same inverted shape: a server with no cluster
                                // says so, and anything that cannot say counts as
                                // having one.
                                state.dx_cluster_available = !ack.state_flags.has(
                                    sdr_remote_core::protocol::ServerStateFlags::NO_DX_CLUSTER,
                                );
                                // An RX-only position Thetis cannot guard by itself:
                                // a line on every screen (build 215).
                                state.rx_only_unguarded = ack.state_flags.has(
                                    sdr_remote_core::protocol::ServerStateFlags::RX_ONLY_UNGUARDED,
                                );
                                // A server with no cluster can never send a spot,
                                // so the subscription comes off. Here rather than
                                // in each front end: the desktop had its own copy
                                // of this rule and the phone had none, which is
                                // why the phone showed the tick ON against a
                                // server with no cluster at all (seen
                                // 2026-08-20). It also lands BEFORE the
                                // just-connected re-send below, which then carries
                                // the corrected value - so the server is told
                                // without a packet of its own.
                                if !state.dx_cluster_available {
                                    state.dx_spots_enabled = false;
                                }
                                if !thetis_configured {
                                    if matches!(
                                        state.connect_status,
                                        crate::state::ConnectStatus::Failed(
                                            crate::state::ConnectError::TciUnreachable { .. }
                                        )
                                    ) {
                                        state.connect_status = crate::state::ConnectStatus::Connected;
                                    }
                                } else if matches!(
                                    state.connect_status,
                                    crate::state::ConnectStatus::Connected
                                ) {
                                    if !tci_up && !thetis_starting_now {
                                        if let Some(ref addr) = server_addr {
                                            state.connect_status =
                                                crate::state::ConnectStatus::Failed(
                                                    crate::state::ConnectError::TciUnreachable {
                                                        server_addr: addr.clone(),
                                                        server_reported_detail: None,
                                                        thetis_process_running: Some(thetis_proc_running),
                                                    },
                                                );
                                        }
                                    }
                                } else if matches!(
                                    state.connect_status,
                                    crate::state::ConnectStatus::Failed(
                                        crate::state::ConnectError::TciUnreachable { .. }
                                    )
                                ) {
                                    // Recover to Connected if TCI is up OR Thetis is in the
                                    // middle of launching (transient; wait for the launch
                                    // to either succeed or timeout before complaining).
                                    if tci_up || thetis_starting_now {
                                        state.connect_status =
                                            crate::state::ConnectStatus::Connected;
                                    } else {
                                        // Still TciUnreachable; refresh the thetis_process_running
                                        // hint so the UI text follows the latest server state.
                                        if let crate::state::ConnectStatus::Failed(
                                            crate::state::ConnectError::TciUnreachable {
                                                thetis_process_running: ref mut tpr,
                                                ..
                                            },
                                        ) = state.connect_status
                                        {
                                            *tpr = Some(thetis_proc_running);
                                        }
                                    }
                                }
                            } else {
                                // Old servers do not report whether Thetis/TCI is configured;
                                // keep the legacy Radio/Thetis UI visible for compatibility.
                                state.thetis_configured = true;
                            }
                            if let Some(ref addr) = server_addr {
                                if !was_connected {
                                    info!("Connected to server (rtt={}ms, ring={})", rtt, audio.playback_buffer_level());
                                    // A new session: the screen's subscription anew is an open.
                                    vrx_fft_sent = [false; 2];
                                    // Reset jitter buffer and codec state on (re)connect so audio starts fresh
                                    jitter_buf.reset();
                                    // Both formats, which the three separate lines
                                    // this replaces did not do: only the narrowband
                                    // decoders were rebuilt, so a wideband stream
                                    // carried its old history into the new session.
                                    st_rx1.reset();
                                    st_bin_r.reset();
                                    st_rx2.reset();
                                    // The other four for the same reason. The price is
                                    // that the first gap after this is silent until the
                                    // codec has decoded for a while - deliberate; the
                                    // table in `StreamDecoder::conceal` measures it, and
                                    // the switch-off arms pay the same price for the
                                    // same reason. VRX had no
                                    // route at all that cleared a decoder except the
                                    // operator's own button - its seq==0 arm resets
                                    // only the jitter buffer - and the Yaesu slots had
                                    // one that depends on the server restarting its
                                    // sequence, which it only does on the edge from no
                                    // subscribers to some. Do not lean on that here.
                                    st_vrx1.reset();
                                    st_vrx2.reset();
                                    st_yaesu.reset();
                                    st_yaesu2.reset();
                                    logged_first_rx = false;
                                    // The other four, for the reason this whole
                                    // block exists. "Radio N: first audio packet" is
                                    // the ONLY line that shows the radio's audio is
                                    // back, and it sits behind these flags - which
                                    // until now were cleared only on the paths a
                                    // long gap takes. A short reconnect therefore
                                    // restored the subscription and logged nothing
                                    // about it, so the field proof for this very
                                    // patch could not have worked: over six seconds
                                    // the line appears, under six seconds it does
                                    // not, and under six seconds is exactly where
                                    // the defect lives.
                                    yaesu_logged_first = false;
                                    yaesu2_logged_first = false;
                                    vrx1_logged_first = false;
                                    vrx2_logged_first = false;
                                                // Clear stale spectrum data on (re)connect
                                    state.spectrum_bins.clear();
                                    state.full_spectrum_bins.clear();
                                    state.spectrum_sequence = 0;
                                    state.full_spectrum_sequence = 0;
                                    // Everything this client had asked for, derived
                                    // from the shared list instead of retyped -
                                    // including both Yaesu audio subscriptions and
                                    // both Yaesu state subscriptions, which nobody
                                    // replayed and which is why the radio went
                                    // silent and the screen stopped following its
                                    // VFO knob after a network switch (measured
                                    // 2026-09-12/13).
                                    //
                                    // FIRST, before the hand-written sends below,
                                    // and that position is deliberate. Those sends
                                    // pass through the same macro, so they are
                                    // recorded as they go out; replaying afterwards
                                    // meant every one of them went twice on every
                                    // single connect, and the count in the log line
                                    // included packets sent a microsecond earlier
                                    // by the block above.
                                    //
                                    // Replaying first also settles which value wins
                                    // when the two disagree: the hand-written sends
                                    // read the current variables, so they are the
                                    // more authoritative of the two and they should
                                    // land last.
                                    let mut replay = session_intent.replay();
                                    // Only what this server takes; the rest is in
                                    // `held_back`, so the count below is of what
                                    // could go.
                                    replay.retain(|(id, _)| state.server_takes(*id));
                                    let mut replay_sent = 0usize;
                                    for (id, value) in &replay {
                                        // The Result is READ here, unlike the sends
                                        // above it. A reconnect is the moment a send
                                        // is most likely to fail, and a line that
                                        // counted what it queued would read green
                                        // through exactly the failure it exists to
                                        // show.
                                        if matches!(send_control!(*id, *value, addr.as_str()), Ok(n) if n > 0) {
                                            replay_sent += 1;
                                        }
                                    }
                                    // This branch sits in a loop
                                    // no assertion can open, so a field log is the
                                    // only place it can be seen. The two Yaesu
                                    // values are spelled out because they are the
                                    // ones an operator can hear.
                                    info!("{}", crate::session_intent::restore_summary(&replay, replay_sent));
                                    // What the gate held back: newer than the replay,
                                    // which may carry the value of the last server
                                    // that took it; older than the hand-written sends
                                    // below, which read the current variables - a pan
                                    // made while the link was down went nowhere, and
                                    // only those carry it.
                                    send_held_back!(addr.as_str());

                                    // Send deferred spectrum settings now that server knows us:
                                    // on or off, and the view either way. Only when on, it
                                    // left a spectrum closed during the gap on (the replay
                                    // carries the last 1 that went out), and a zoom or pan
                                    // made then, with the spectrum off, never arrived.
                                    {
                                        let on = crate::thetis_streams::stream_value(
                                            spectrum_enabled, thetis_streams_stopped);
                                        let _ = send_control!(ControlId::SpectrumEnable, on as u16, addr.as_str());

                                        let _ = send_control!(ControlId::SpectrumFps, spectrum_fps as u16, addr.as_str());

                                        // Re-send zoom and pan so server generates correct view
                                        let _ = send_control!(ControlId::SpectrumZoom, (spectrum_zoom * 10.0) as u16, addr.as_str());

                                        send_pan!(ControlId::SpectrumPan, ControlId::SpectrumPanFine, spectrum_pan, addr.as_str());

                                        let _ = send_control!(ControlId::SpectrumMaxBins, spectrum_max_bins, addr.as_str());

                                        // The FFT size is the receiver's, for every client on the
                                        // server, not a view of this one: only from a client that
                                        // looks at it.
                                        if on && spectrum_fft_size_k != 0 {
                                            let _ = send_control!(ControlId::SpectrumFftSize, spectrum_fft_size_k, addr.as_str());
                                        }
                                    }

                                    // Re-send RX1 audio subscription on reconnect.
                                    // Server default = ON, so only relevant if
                                    // the client wants RX1 OFF; but always sending is
                                    // idempotent and covers both cases.
                                    {
                                        // Not the bare wish: if the grace period has
                                        // given this stream up to save data, handing it
                                        // back here would undo that on every reconnect.
                                        let _ = send_control!(
                                            ControlId::Rx1Enable,
                                            crate::thetis_streams::stream_value(
                                                state.rx1_enabled, thetis_streams_stopped) as u16,
                                            addr.as_str()
                                        );
                                    }

                                    // Re-send RX2 AUDIO subscription on reconnect (separate from spectrum).
                                    if state.rx2_enabled {
                                        let _ = send_control!(ControlId::Rx2Enable, 1, addr.as_str());
                                        info!("RX2 audio re-sent on reconnect");
                                    }
                                    // Re-send RX2 SPECTRUM subscription on reconnect, SEPARATE from the
                                    // audio subscription, otherwise an RX2-spectrum-
                                    // without-audio client gets no spectrum after reconnecting.
                                    // On or off, and the view either way, as RX1 above.
                                    {
                                        let _ = send_control!(ControlId::Rx2SpectrumEnable, state.rx2_spectrum_enabled as u16, addr.as_str());

                                        // RX1's count: both clients send RX2 the same as RX1.
                                        let _ = send_control!(ControlId::Rx2SpectrumMaxBins, spectrum_max_bins, addr.as_str());

                                        let _ = send_control!(ControlId::Rx2SpectrumZoom, (rx2_spectrum_zoom * 10.0) as u16, addr.as_str());

                                        send_pan!(ControlId::Rx2SpectrumPan, ControlId::Rx2SpectrumPanFine, rx2_spectrum_pan, addr.as_str());

                                        // As RX1's: the receiver's for everyone, so only when on.
                                        if state.rx2_spectrum_enabled && rx2_spectrum_fft_size_k != 0 {
                                            let _ = send_control!(ControlId::Rx2SpectrumFftSize, rx2_spectrum_fft_size_k, addr.as_str());
                                        }
                                        info!("RX2 spectrum re-sent on reconnect (on={})", state.rx2_spectrum_enabled);
                                    }
                                    // Send AudioMode so server knows our channel requirements
                                    let _ = send_control!(ControlId::AudioMode, audio_mode, addr.as_str());
                                    // Re-send S-meter source subscription. Server's per-client
                                    // session resets to default 0x22 (Avg-only) on every new
                                    // ClientSession insert, so we must restore the user's choice
                                    // after auth completes.
                                    let mask: u16 = match smeter_source {
                                        0 => 0x11,
                                        1 => 0x22,
                                        2 => 0x44,
                                        _ => 0x22,
                                    };
                                    let _ = send_control!(ControlId::SmeterSources, mask, addr.as_str());
                                    // Re-send DX-spots opt-out: server's ClientSession resets
                                    // to default ON on every new insert, so without this path
                                    // the client would visually show OFF while the server, after
                                    // reconnect, sends Spot frames again.
                                    let _ = send_control!(ControlId::DxSpotsEnabled, state.dx_spots_enabled as u16, addr.as_str());
                                    // Same reasoning for the full-spectrum opt-out.
                                    let _ = send_control!(ControlId::FullSpectrumEnabled, state.full_spectrum_enabled as u16, addr.as_str());
                                }
                                state.connected = true;
                                was_connected = true;
                                // And on every ack, not only in the restore: a
                                // Connect to another server without a password has
                                // no new-session edge, and the restore does not run
                                // there. After a restore this finds nothing.
                                send_held_back!(addr.as_str());
                            }
                        }
                        Ok(Packet::Frequency(freq_pkt)) => {
                            if let Some(pf) = pending_freq {
                                if freq_pkt.frequency_hz == pf {
                                    // Server confirmed our frequency change
                                    pending_freq = None;
                                    pending_freq_time = None;
                                    state.frequency_hz = freq_pkt.frequency_hz;
                                } else if pending_freq_time.map_or(true, |t| t.elapsed().as_secs() > 3) {
                                    // Timeout: accept server freq after 3 seconds
                                    pending_freq = None;
                                    pending_freq_time = None;
                                    state.frequency_hz = freq_pkt.frequency_hz;
                                }
                                // else: ignore stale server freq while our change is pending
                            } else {
                                state.frequency_hz = freq_pkt.frequency_hz;
                            }
                        }
                        Ok(Packet::Mode(mode_pkt)) => {
                            state.mode = mode_pkt.mode;
                        }
                        Ok(Packet::Smeter(sm_pkt)) => {
                            state.smeter = sm_pkt.level as f32 / 10.0;
                            state.other_tx = sm_pkt.flags.held_by_other();
                            state.server_tx = sm_pkt.flags.ptt();
                        }
                        Ok(Packet::Spectrum(sp)) => {
                            state.spectrum_bins = sp.bins;
                            state.spectrum_center_hz = sp.center_freq_hz;
                            state.spectrum_span_hz = sp.span_hz;
                            state.spectrum_ref_level = sp.ref_level;
                            state.spectrum_db_per_unit = sp.db_per_unit;
                            state.spectrum_sequence = sp.sequence;
                        }
                        Ok(Packet::FullSpectrum(sp)) => {
                            state.full_spectrum_bins = sp.bins;
                            state.full_spectrum_center_hz = sp.center_freq_hz;
                            state.full_spectrum_span_hz = sp.span_hz;
                            state.full_spectrum_sequence = sp.sequence;
                        }
                        Ok(Packet::SpectrumVrx1(sp)) => {
                            state.vrx1_extracted_bins = sp.bins;
                            state.vrx1_extracted_center_hz = sp.center_freq_hz;
                            state.vrx1_extracted_span_hz = sp.span_hz;
                            state.vrx1_extracted_sequence = sp.sequence;
                        }
                        Ok(Packet::SpectrumVrx2(sp)) => {
                            state.vrx2_extracted_bins = sp.bins;
                            state.vrx2_extracted_center_hz = sp.center_freq_hz;
                            state.vrx2_extracted_span_hz = sp.span_hz;
                            state.vrx2_extracted_sequence = sp.sequence;
                        }
                        // RX2 packets
                        Ok(Packet::AudioMultiCh(pkt)) => {
                            if let Some(started) = conceal_since.take() {
                                let gap = started.elapsed();
                                conceal_gaps += 1;
                                conceal_total_frames += conceal_frames;
                                if gap > conceal_longest {
                                    conceal_longest = gap;
                                }
                            }
                            if !logged_first_rx {
                                info!("RX: first multi-ch audio ({} channels, seq={})",
                                    pkt.channels.len(), pkt.sequence);
                                logged_first_rx = true;
                            }

                            last_audio_received = Some(Instant::now());
                            let bytes: usize = pkt.channels.iter().map(|(_, d)| d.len()).sum();
                            let rx1_opus = pkt.channels.iter().find(|(id, _)| *id == 0).map_or(&[][..], |(_, d)| d.as_slice());
                            audio_arrived!(thetis, crate::loss_window::Stream::Thetis, pkt.sequence, pkt.timestamp, bytes, rx1_opus);

                            // Serialize channels into opus_data for jitter buffer storage
                            let mut blob = Vec::new();
                            blob.push(pkt.channels.len() as u8);
                            for (ch_id, opus) in &pkt.channels {
                                blob.push(*ch_id);
                                blob.extend_from_slice(&(opus.len() as u16).to_be_bytes());
                                blob.extend_from_slice(opus);
                            }

                            let arrival_ms = start.elapsed().as_millis() as u64;
                            jitter_buf.push(
                                BufferedFrame {
                                    sequence: pkt.sequence,
                                    timestamp: pkt.timestamp,
                                    opus_data: blob,
                                    ptt: false,
                                    wideband: pkt.flags.wideband(),
                                },
                                arrival_ms,
                            );

                            state.rx_packets += 1;
                            state.jitter_ms = jitter_buf.jitter_ms();
                            state.buffer_depth = jitter_buf.depth() as u32;
                        }
                        // Legacy packet types (deprecated, ignored)
                        Ok(Packet::AudioRx2(_)) | Ok(Packet::AudioBinR(_)) => {}

                        Ok(Packet::FrequencyRx2(freq_pkt)) => {
                            if let Some(pf) = pending_freq_rx2 {
                                if freq_pkt.frequency_hz == pf {
                                    // Server confirmed our RX2 frequency change
                                    pending_freq_rx2 = None;
                                    pending_freq_rx2_time = None;
                                    state.frequency_rx2_hz = freq_pkt.frequency_hz;
                                } else if pending_freq_rx2_time.map_or(true, |t| t.elapsed().as_secs() > 3) {
                                    // Timeout: accept server freq after 3 seconds
                                    pending_freq_rx2 = None;
                                    pending_freq_rx2_time = None;
                                    state.frequency_rx2_hz = freq_pkt.frequency_hz;
                                }
                                // else: ignore stale server freq while our RX2 change is pending
                            } else {
                                state.frequency_rx2_hz = freq_pkt.frequency_hz;
                            }
                        }
                        Ok(Packet::ModeRx2(mode_pkt)) => {
                            state.mode_rx2 = mode_pkt.mode;
                        }
                        Ok(Packet::SmeterRx2(sm_pkt)) => {
                            state.smeter_rx2 = sm_pkt.level as f32 / 10.0;
                        }
                        // Alternate S-meter sources. Both the per-source field
                        // AND the primary `state.smeter` / `state.smeter_rx2`
                        // are updated so the existing render path (which reads
                        // `state.smeter`) transparently follows the active
                        // source, the server only sends one source per RX
                        // unless the client subscribes to multiple.
                        Ok(Packet::SmeterSig(sm_pkt)) => {
                            let dbm = sm_pkt.level as f32 / 10.0;
                            state.smeter_sig = dbm;
                            state.smeter = dbm;
                            state.other_tx = sm_pkt.flags.held_by_other();
                        }
                        Ok(Packet::SmeterMaxBin(sm_pkt)) => {
                            let dbm = sm_pkt.level as f32 / 10.0;
                            state.smeter_peakbin = dbm;
                            state.smeter = dbm;
                            state.other_tx = sm_pkt.flags.held_by_other();
                        }
                        Ok(Packet::SmeterRx2Sig(sm_pkt)) => {
                            let dbm = sm_pkt.level as f32 / 10.0;
                            state.smeter_rx2_sig = dbm;
                            state.smeter_rx2 = dbm;
                        }
                        Ok(Packet::SmeterRx2MaxBin(sm_pkt)) => {
                            let dbm = sm_pkt.level as f32 / 10.0;
                            state.smeter_rx2_peakbin = dbm;
                            state.smeter_rx2 = dbm;
                        }
                        Ok(Packet::SpectrumRx2(sp)) => {
                            state.rx2_spectrum_bins = sp.bins;
                            state.rx2_spectrum_center_hz = sp.center_freq_hz;
                            state.rx2_spectrum_span_hz = sp.span_hz;
                            state.rx2_spectrum_ref_level = sp.ref_level;
                            state.rx2_spectrum_db_per_unit = sp.db_per_unit;
                            state.rx2_spectrum_sequence = sp.sequence;
                        }
                        Ok(Packet::FullSpectrumRx2(sp)) => {
                            state.rx2_full_spectrum_bins = sp.bins;
                            state.rx2_full_spectrum_center_hz = sp.center_freq_hz;
                            state.rx2_full_spectrum_span_hz = sp.span_hz;
                            state.rx2_full_spectrum_sequence = sp.sequence;
                        }
                        Ok(Packet::Heartbeat(_)) => {}
                        Ok(Packet::Control(ctrl)) => {
                            match ctrl.control_id {
                                // Client to server only; a server has no reason
                                // to tell a client where its own view sits.
                                ControlId::VrxSpectrumPan | ControlId::VrxSpectrumPan2 => {}
                                ControlId::PowerOnOff => {
                                    // Ignore stale server broadcasts briefly after we sent
                                    // a power command (prevents race with shutdown sequence)
                                    if Instant::now() < power_suppress_until {
                                        // Keep local state, ignore server
                                    } else {
                                        state.power_on = ctrl.value != 0;
                                    }
                                }
                                ControlId::TxProfile => state.tx_profile = ctrl.value as u8,
                                // Client->server only (WAV-playback TXEQ-bypass); the server
                                // never broadcasts it back, so no-op here.
                                ControlId::ThetisTxeq => {}
                                // Client->server only (Yaesu STATE subscription + power on/off); no-op.
                                ControlId::YaesuStateEnable | ControlId::Yaesu2StateEnable
                                | ControlId::YaesuPowerOnOff | ControlId::Yaesu2PowerOnOff
                                // Diagnostic switch handled entirely server-side.
                                | ControlId::YaesuCatMonitor
                                | ControlId::YaesuReadMemoryTones => {}
                                ControlId::NoiseReduction => state.nr_level = ctrl.value.min(4) as u8,
                                ControlId::AutoNotchFilter => state.anf_on = ctrl.value != 0,
                                ControlId::DriveLevel => state.drive_level = ctrl.value.min(100) as u8,
                                ControlId::Rx1AfGain => {
                                    let val = ctrl.value.min(100) as u8;
                                    state.rx_af_gain = val;
                                    rx_volume = val as f32 / 100.0;
                                    last_sent_volume = val as u16;
                                    rx_volume_synced = true;
                                }
                                ControlId::FilterLow => {
                                    state.filter_low_hz = ctrl.value as i16 as i32;
                                }
                                ControlId::FilterHigh => {
                                    state.filter_high_hz = ctrl.value as i16 as i32;
                                }
                                ControlId::ThetisStarting => {
                                    state.thetis_starting = ctrl.value != 0;
                                }
                                ControlId::PaDriveWarning => {
                                    let on = ctrl.value & 1 != 0;
                                    let beep = ctrl.value & 2 != 0;
                                    if on != state.pa_drive_warning {
                                        if on {
                                            log::warn!("PA drive warning from the server: the drive went up while a PA is out of reach");
                                        } else {
                                            log::info!("PA drive warning cleared");
                                        }
                                    }
                                    state.pa_drive_warning = on;
                                    state.pa_drive_alarm = beep;
                                }
                                ControlId::SpectrumEnable | ControlId::SpectrumFps
                                | ControlId::SpectrumZoom | ControlId::SpectrumPan
                                | ControlId::SpectrumPanFine | ControlId::Rx2SpectrumPanFine
                                | ControlId::SpectrumMaxBins | ControlId::SpectrumFftSize
                                | ControlId::SpectrumBinDepth
                                // Client -> server only; the server never echoes it back.
                                | ControlId::FullSpectrumEnabled => {}
                                // RX1/RX2 audio subscription (server-echo, if pushed)
                                ControlId::Rx1Enable => state.rx1_enabled = ctrl.value != 0,
                                // RX2 controls from server
                                ControlId::Rx2Enable => state.rx2_enabled = ctrl.value != 0,
                                ControlId::Rx2AfGain => {
                                    let val = ctrl.value.min(100);
                                    if val as u8 != state.rx2_af_gain {
                                        info!("RX2 AF gain from server: {}% (was {}%)", val, state.rx2_af_gain);
                                    }
                                    state.rx2_af_gain = val as u8;
                                    rx2_volume = val as f32 / 100.0;
                                    last_sent_rx2_volume = val as u16;
                                    rx2_volume_synced = true;
                                }
                                ControlId::Rx2FilterLow => state.filter_rx2_low_hz = ctrl.value as i16 as i32,
                                ControlId::Rx2FilterHigh => state.filter_rx2_high_hz = ctrl.value as i16 as i32,
                                ControlId::Rx2NoiseReduction => state.rx2_nr_level = ctrl.value.min(4) as u8,
                                ControlId::Rx2AutoNotchFilter => state.rx2_anf_on = ctrl.value != 0,
                                ControlId::Rx2AgcMode => state.rx2_agc_mode = ctrl.value as u8,
                                ControlId::Rx2AgcGain => state.rx2_agc_gain = ctrl.value as u8,
                                ControlId::Rx2SqlEnable => state.rx2_sql_enable = ctrl.value != 0,
                                ControlId::Rx2SqlLevel => state.rx2_sql_level = ctrl.value as u8,
                                ControlId::Rx2NoiseBlanker => state.rx2_nb_enable = ctrl.value != 0,
                                ControlId::Rx2Binaural => state.rx2_binaural = ctrl.value != 0,
                                ControlId::Rx2ApfEnable => state.rx2_apf_enable = ctrl.value != 0,
                                ControlId::Rx2VfoLock => state.rx2_vfo_lock = ctrl.value != 0,
                                ControlId::MonitorOn => state.mon_on = ctrl.value != 0,
                                ControlId::AgcMode => state.agc_mode = ctrl.value as u8,
                                ControlId::AgcGain => state.agc_gain = ctrl.value as u8,
                                ControlId::RitEnable => state.rit_enable = ctrl.value != 0,
                                ControlId::RitOffset => state.rit_offset = ctrl.value as i16,
                                ControlId::XitEnable => state.xit_enable = ctrl.value != 0,
                                ControlId::XitOffset => state.xit_offset = ctrl.value as i16,
                                ControlId::SqlEnable => state.sql_enable = ctrl.value != 0,
                                ControlId::SqlLevel => state.sql_level = ctrl.value as u8,
                                ControlId::NoiseBlanker => {
                                    state.nb_enable = ctrl.value != 0;
                                    state.nb_level = ctrl.value as u8;
                                }
                                ControlId::CwKeyerSpeed => state.cw_keyer_speed = ctrl.value as u8,
                                ControlId::VfoLock => state.vfo_lock = ctrl.value != 0,
                                ControlId::Binaural => state.binaural = ctrl.value != 0,
                                ControlId::ApfEnable => state.apf_enable = ctrl.value != 0,
                                ControlId::Mute => state.mute = ctrl.value != 0,
                                ControlId::RxMute => state.rx_mute = ctrl.value != 0,
                                ControlId::ManualNotchFilter => state.nf_enable = ctrl.value != 0,
                                ControlId::Rx2ManualNotchFilter => state.rx2_nf_enable = ctrl.value != 0,
                                ControlId::RxBalance => state.rx_balance = ctrl.value as i16 as i8,
                                ControlId::TuneDrive => state.tune_drive = ctrl.value.min(100) as u8,
                                ControlId::MonitorVolume => state.mon_volume = ctrl.value as i16 as i8,
                                ControlId::ThetisSwr => state.thetis_swr_x100 = ctrl.value,
                                ControlId::VfoSync => state.vfo_sync = ctrl.value != 0,
                                ControlId::DxSpotsEnabled => state.dx_spots_enabled = ctrl.value != 0,
                                ControlId::ThetisWidebandAudio => {} // client→server only; server echoes ignored
                                ControlId::Rx2SpectrumEnable | ControlId::Rx2SpectrumFps
                                | ControlId::Rx2SpectrumZoom | ControlId::Rx2SpectrumPan
                                | ControlId::Rx2SpectrumMaxBins | ControlId::Rx2SpectrumFftSize
                                | ControlId::VfoSwap
                                | ControlId::ThetisTune | ControlId::YaesuEnable
                                | ControlId::YaesuPtt | ControlId::YaesuFreq
                                | ControlId::YaesuMicGain | ControlId::YaesuMode
                                | ControlId::YaesuReadMemories
                                | ControlId::YaesuRecallMemory
                                | ControlId::YaesuWriteMemories
                                | ControlId::YaesuSelectVfo
                                | ControlId::YaesuSquelch | ControlId::YaesuRfGain
                                | ControlId::YaesuRadioMicGain | ControlId::YaesuRfPower
                                | ControlId::YaesuButton
                                | ControlId::YaesuReadMenus | ControlId::YaesuSetMenu
                                | ControlId::DiversityRead
                                | ControlId::CwKey | ControlId::CwMacroStop => {}
                                // Diversity state from server (read response)
                                ControlId::DiversityEnable => state.diversity_enabled = ctrl.value != 0,
                                ControlId::DiversityAutoNull => {
                                    state.diversity_autonull_result = ctrl.value;
                                }
                                ControlId::DiversityRef => state.diversity_ref = ctrl.value as u8,
                                ControlId::DiversitySource => state.diversity_source = ctrl.value as u8,
                                ControlId::DiversityGainRx1 => state.diversity_gain_rx1 = ctrl.value,
                                ControlId::DiversityGainRx2 => state.diversity_gain_rx2 = ctrl.value,
                                ControlId::DiversityGainMulti => state.diversity_gain_multi = ctrl.value,
                                ControlId::DiversityPhase => state.diversity_phase = ctrl.value,
                                ControlId::AgcAutoRx1 => state.agc_auto_rx1 = ctrl.value != 0,
                                ControlId::AgcAutoRx2 => state.agc_auto_rx2 = ctrl.value != 0,
                                ControlId::DdcSampleRateRx1 => state.ddc_sample_rate_rx1 = ctrl.value,
                                ControlId::DdcSampleRateRx2 => state.ddc_sample_rate_rx2 = ctrl.value,
                                ControlId::DdcCentreRx1 => state.ddc_centre_rx1_khz = ctrl.value,
                                ControlId::DdcCentreRx2 => state.ddc_centre_rx2_khz = ctrl.value,
                                ControlId::AudioMode => {} // handled client-side
                                ControlId::AllowZoomBelow2x => {} // handled client-side (setup checkbox)
                                ControlId::SmeterSources => {} // client→server only; server echoes ignored
                                ControlId::VrxEnable => {} // client→server only
                                ControlId::VrxMode => {} // client→server only
                                ControlId::VrxVolume => {} // client→server only
                                ControlId::VrxEnable2 => {} // client→server only
                                ControlId::VrxMode2 => {} // client→server only
                                ControlId::VrxVolume2 => {} // client→server only
                                ControlId::VrxFilterLow => {} // client→server only
                                ControlId::VrxFilterHigh => {} // client→server only
                                ControlId::VrxFilterLow2 => {} // client→server only
                                ControlId::VrxFilterHigh2 => {} // client→server only
                                ControlId::VrxSpectrumEnable => {} // client→server only
                                ControlId::VrxSpectrumEnable2 => {} // client→server only
                                ControlId::VrxSpectrumSpanKhz => {} // client→server only
                                ControlId::VrxSpectrumSpanKhz2 => {} // client→server only
                                ControlId::VrxSpectrumSpanFine | ControlId::VrxSpectrumSpanFine2 => {} // client→server only
                                // Dual-radio slot 1 (Option B-prime): client→server only;
                                // server echoes ignored (same pattern as slot-0 Yaesu + Vrx).
                                ControlId::Yaesu2Enable | ControlId::Yaesu2Ptt
                                | ControlId::Yaesu2Freq | ControlId::Yaesu2MicGain
                                | ControlId::Yaesu2Mode | ControlId::Yaesu2ReadMemories
                                | ControlId::Yaesu2RecallMemory | ControlId::Yaesu2WriteMemories
                                | ControlId::Yaesu2SelectVfo | ControlId::Yaesu2Squelch
                                | ControlId::Yaesu2RfGain | ControlId::Yaesu2RadioMicGain
                                | ControlId::Yaesu2RfPower | ControlId::Yaesu2Button
                                | ControlId::Yaesu2ReadMenus | ControlId::Yaesu2SetMenu => {}
                                // VRX wide / synchronous-AM UX: client→server only.
                                ControlId::VrxAudioRate
                                | ControlId::VrxAudioRate2
                                | ControlId::VrxSamAutoTune
                                | ControlId::VrxSamAutoTune2 => {}
                                // TX modulation filter: client→server only (the
                                // server pushes the current value via TxFilterBand).
                                ControlId::TxFilterLow | ControlId::TxFilterHigh => {}
                            }
                        }
                        Ok(Packet::EquipmentStatus(eq)) => {
                            match eq.device_type {
                                DeviceType::Amplitec6x2 => {
                                    state.amplitec_available = true;
                                    state.amplitec_connected = eq.connected;
                                    state.amplitec_switch_a = eq.switch_a;
                                    state.amplitec_switch_b = eq.switch_b;
                                    if let Some(labels) = eq.labels {
                                        state.amplitec_labels = labels;
                                    }
                                }
                                DeviceType::Tuner => {
                                    state.tuner_available = true;
                                    state.tuner_state = eq.switch_a;
                                    state.tuner_can_tune = eq.switch_b != 0;
                                    state.tuner_connected = eq.connected;
                                }
                                DeviceType::SpeExpert => {
                                    state.spe_connected = eq.connected;
                                    state.spe_state = eq.switch_a;
                                    state.spe_band = eq.switch_b;
                                    state.spe_available = true;
                                    // Parse telemetry from labels CSV
                                    if let Some(labels) = eq.labels {
                                        let parts: Vec<&str> = labels.split(',').collect();
                                        // Format: ptt,power_w,swr_x10,temp,voltage_x10,current_x10,warning,alarm,power_level,antenna,input,atu_bypassed
                                        if parts.len() >= 11 {
                                            state.spe_ptt = parts[0] == "T";
                                            state.spe_power_w = parts[1].parse().unwrap_or(0);
                                            state.spe_swr_x10 = parts[2].parse().unwrap_or(10);
                                            state.spe_temp = parts[3].parse().unwrap_or(0);
                                            state.spe_voltage_x10 = parts[4].parse().unwrap_or(0);
                                            state.spe_current_x10 = parts[5].parse().unwrap_or(0);
                                            state.spe_warning = parts[6].bytes().next().unwrap_or(b'N');
                                            state.spe_alarm = parts[7].bytes().next().unwrap_or(b'N');
                                            state.spe_power_level = parts[8].parse().unwrap_or(0);
                                            state.spe_antenna = parts[9].parse().unwrap_or(0);
                                            state.spe_input = parts[10].parse().unwrap_or(0);
                                        }
                                        if parts.len() >= 12 {
                                            state.spe_atu_bypassed = parts[11] == "1";
                                        }
                                        if parts.len() >= 13 {
                                            state.spe_active = parts[12] == "1";
                                        }
                                    }
                                }
                                DeviceType::Rf2k => {
                                    state.rf2k_connected = eq.connected;
                                    state.rf2k_operate = eq.switch_a != 0;
                                    state.rf2k_band = eq.switch_b;
                                    state.rf2k_available = true;
                                    // Parse telemetry from labels CSV
                                    // Format: operate,ptt,band,freq_khz,temp_x10,volt_x10,curr_x10,fwd_w,ref_w,swr_x100,
                                    //         max_fwd,max_ref,max_swr,error_state,ant_type,ant_nr,
                                    //         tuner_mode,tuner_setup,l_nh,c_pf,tuner_freq_khz,seg_khz,
                                    //         drive_w,modulation,max_power_w,error_text,device_name
                                    if let Some(labels) = eq.labels {
                                        let parts: Vec<&str> = labels.split(',').collect();
                                        if parts.len() >= 27 {
                                            state.rf2k_operate = parts[0] == "1";
                                            // parts[1] = ptt (unused for now)
                                            state.rf2k_band = parts[2].parse().unwrap_or(0);
                                            state.rf2k_frequency_khz = parts[3].parse().unwrap_or(0);
                                            state.rf2k_temperature_x10 = parts[4].parse().unwrap_or(0);
                                            state.rf2k_voltage_x10 = parts[5].parse().unwrap_or(0);
                                            state.rf2k_current_x10 = parts[6].parse().unwrap_or(0);
                                            state.rf2k_forward_w = parts[7].parse().unwrap_or(0);
                                            state.rf2k_reflected_w = parts[8].parse().unwrap_or(0);
                                            state.rf2k_swr_x100 = parts[9].parse().unwrap_or(100);
                                            state.rf2k_max_forward_w = parts[10].parse().unwrap_or(0);
                                            state.rf2k_max_reflected_w = parts[11].parse().unwrap_or(0);
                                            state.rf2k_max_swr_x100 = parts[12].parse().unwrap_or(100);
                                            state.rf2k_error_state = parts[13].parse().unwrap_or(0);
                                            state.rf2k_antenna_type = parts[14].parse().unwrap_or(0);
                                            state.rf2k_antenna_number = parts[15].parse().unwrap_or(1);
                                            state.rf2k_tuner_mode = parts[16].parse().unwrap_or(0);
                                            state.rf2k_tuner_setup = parts[17].to_string();
                                            state.rf2k_tuner_l_nh = parts[18].parse().unwrap_or(0);
                                            state.rf2k_tuner_c_pf = parts[19].parse().unwrap_or(0);
                                            state.rf2k_tuner_freq_khz = parts[20].parse().unwrap_or(0);
                                            state.rf2k_segment_size_khz = parts[21].parse().unwrap_or(0);
                                            state.rf2k_drive_w = parts[22].parse().unwrap_or(0);
                                            state.rf2k_modulation = parts[23].to_string();
                                            state.rf2k_max_power_w = parts[24].parse().unwrap_or(0);
                                            state.rf2k_error_text = parts[25].to_string();
                                            state.rf2k_device_name = parts[26].to_string();
                                        }
                                        if parts.len() >= 28 {
                                            state.rf2k_active = parts[27] == "1";
                                        }
                                        // Debug fields - parts[28..47]
                                        if parts.len() >= 44 {
                                            state.rf2k_debug_available = parts[28] == "1";
                                            state.rf2k_bias_pct_x10 = parts[29].parse().unwrap_or(0);
                                            state.rf2k_psu_source = parts[30].parse().unwrap_or(0);
                                            state.rf2k_uptime_s = parts[31].parse().unwrap_or(0);
                                            state.rf2k_tx_time_s = parts[32].parse().unwrap_or(0);
                                            state.rf2k_error_count = parts[33].parse().unwrap_or(0);
                                            // parts[34] = error history (semicolon-separated "time=error")
                                            state.rf2k_error_history = if parts[34].is_empty() {
                                                Vec::new()
                                            } else {
                                                parts[34].split(';').filter_map(|entry| {
                                                    let mut kv = entry.splitn(2, '=');
                                                    let t = kv.next()?;
                                                    let e = kv.next()?;
                                                    Some((t.to_string(), e.to_string()))
                                                }).collect()
                                            };
                                            state.rf2k_storage_bank = parts[35].parse().unwrap_or(0);
                                            state.rf2k_hw_revision = parts[36].to_string();
                                            state.rf2k_frq_delay = parts[37].parse().unwrap_or(0);
                                            state.rf2k_autotune_threshold_x10 = parts[38].parse().unwrap_or(0);
                                            state.rf2k_dac_alc = parts[39].parse().unwrap_or(0);
                                            state.rf2k_high_power = parts[40] == "1";
                                            state.rf2k_tuner_6m = parts[41] == "1";
                                            state.rf2k_band_gap_allowed = parts[42] == "1";
                                            state.rf2k_controller_version = parts[43].parse().unwrap_or(0);
                                        }
                                        // Drive config - parts[44..46]
                                        if parts.len() >= 47 {
                                            state.rf2k_drive_config =
                                                sdr_remote_core::rf2k_drive::from_wire([parts[44], parts[45], parts[46]]);
                                        }
                                    }
                                }
                                DeviceType::UltraBeam => {
                                    state.ub_connected = eq.connected;
                                    state.ub_available = true;
                                    state.ub_band = eq.switch_b;
                                    state.ub_direction = eq.switch_a;
                                    // Parse labels CSV:
                                    //  v1 (11 fields): fw_major,fw_minor,operation,frequency_khz,band,direction,off_state,motors_moving,motor_distance_mm,motor_completion,elements(;-sep)
                                    //  v2 (13 fields): + freq_min_mhz, freq_max_mhz
                                    if let Some(labels) = eq.labels {
                                        let parts: Vec<&str> = labels.split(',').collect();
                                        if parts.len() >= 11 {
                                            state.ub_fw_major = parts[0].parse().unwrap_or(0);
                                            state.ub_fw_minor = parts[1].parse().unwrap_or(0);
                                            state.ub_operation = parts[2].parse().unwrap_or(0);
                                            state.ub_frequency_khz = parts[3].parse().unwrap_or(0);
                                            state.ub_band = parts[4].parse().unwrap_or(0);
                                            state.ub_direction = parts[5].parse().unwrap_or(0);
                                            state.ub_off_state = parts[6] == "1";
                                            state.ub_motors_moving = parts[7].parse().unwrap_or(0);
                                            // parts[8] = motor_distance_mm (not shown in client)
                                            state.ub_motor_completion = parts[9].parse().unwrap_or(0);
                                            // parts[10] = elements (semicolon-separated)
                                            let elem_parts: Vec<&str> = parts[10].split(';').collect();
                                            for (i, ep) in elem_parts.iter().enumerate().take(6) {
                                                state.ub_elements_mm[i] = ep.parse().unwrap_or(0);
                                            }
                                            if parts.len() >= 13 {
                                                state.ub_freq_min_mhz = parts[11].parse().unwrap_or(0);
                                                state.ub_freq_max_mhz = parts[12].parse().unwrap_or(0);
                                            }
                                        }
                                    }
                                }
                                DeviceType::Rotor => {
                                    state.rotor_connected = eq.connected;
                                    state.rotor_available = true;
                                    state.rotor_rotating = eq.switch_a != 0;
                                    if let Some(labels) = eq.labels {
                                        let parts: Vec<&str> = labels.split(',').collect();
                                        if parts.len() >= 3 {
                                            state.rotor_angle_x10 = parts[0].parse().unwrap_or(0);
                                            state.rotor_rotating = parts[1] == "1";
                                            state.rotor_target_x10 = parts[2].parse().unwrap_or(0);
                                        }
                                    }
                                }
                                DeviceType::RemoteServer => {} // no status updates from server
                            }
                        }
                        Ok(Packet::EquipmentCommand(_)) => {} // client-only packet, ignore from server
                        Ok(Packet::AmplitecPowerTable(table)) => {
                            state.amplitec_power_max_w = table.max_w;
                            state.amplitec_power_tx_blocked = table.tx_blocked;
                            state.amplitec_power_loaded = true;
                        }
                        Ok(Packet::Spot(spot_pkt)) => {
                            let now = std::time::Instant::now();
                            // Update existing spot or add new one
                            if let Some(existing) = state.dx_spots.iter_mut().find(|s| s.callsign == spot_pkt.callsign && s.frequency_hz == spot_pkt.frequency_hz) {
                                existing.age_seconds = spot_pkt.age_seconds;
                                existing.received = now;
                            } else {
                                state.dx_spots.push(crate::state::DxSpotInfo {
                                    callsign: spot_pkt.callsign,
                                    frequency_hz: spot_pkt.frequency_hz,
                                    mode: spot_pkt.mode,
                                    spotter: spot_pkt.spotter,
                                    comment: spot_pkt.comment,
                                    age_seconds: spot_pkt.age_seconds,
                                    expiry_seconds: spot_pkt.expiry_seconds,
                                    received: now,
                                });
                            }
                            // Expire spots not refreshed in 15 seconds (server sends every 200ms, so generous)
                            state.dx_spots.retain(|s| now.duration_since(s.received).as_secs() < 15);
                        }
                        Ok(Packet::TxProfiles(tp)) => {
                            if !tp.names.is_empty() {
                                state.tx_profile_names = tp.names;
                                state.tx_profile = tp.active;
                            }
                        }
                        Ok(Packet::YaesuState(ys)) => {
                            // connected no longer comes from here; presence authority is
                            // now YaesuPresence,
                            // so dropping/appearing is dynamic instead of sticky.
                            state.yaesu_freq_a = ys.freq_a;
                            state.yaesu_freq_b = ys.freq_b;
                            state.yaesu_mode = ys.mode;
                            state.yaesu_smeter = ys.smeter;
                            state.ptt_denial.cw_keyed_after_all(1, &mut state.yaesu_tx_active, ys.tx_active, ys.held_by_other, cw_asked, ptt_requested);
                            state.yaesu_held_by_other = ys.held_by_other;
                            state.yaesu_power_on = ys.power_on;
                            state.yaesu_af_gain = ys.af_gain;
                            state.yaesu_tx_power = ys.tx_power;
                            state.yaesu_tx_power_max = ys.tx_power_max;
                            state.yaesu_loading = ys.loading;
                            state.yaesu_squelch = ys.squelch;
                            state.yaesu_rf_gain = ys.rf_gain;
                            state.yaesu_mic_gain = ys.mic_gain;
                            state.yaesu_split = ys.split;
                            state.yaesu_scan = ys.scan;
                            state.yaesu_tuner_state = ys.tuner_state;
                            state.yaesu_hi_swr = ys.hi_swr;
                            audio.set_swr_alarm(state.yaesu_hi_swr || state.yaesu2_hi_swr);
                            state.yaesu_vfo_select = ys.vfo_select;
                            state.yaesu_memory_channel = ys.memory_channel;
                        }
                        Ok(Packet::FrequencyYaesu(_)) => {} // client->server only
                        Ok(Packet::FrequencyVrx(_)) => {} // client→server only
                        Ok(Packet::FrequencyVrxActual(pkt)) => {
                            // SAM auto-tune: server is following the carrier.
                            // Record the latest freq; the UI moves the VFO.
                            if pkt.vrx_id == 0 {
                                state.vrx1_autotune_freq_hz = pkt.frequency_hz;
                            } else {
                                state.vrx2_autotune_freq_hz = pkt.frequency_hz;
                            }
                        }
                        Ok(Packet::YaesuControl(_)) => {} // client→server only
                        // Client -> server only: a client never receives its own key edges.
                        Ok(Packet::CwEdges(_)) => {}
                        Ok(Packet::YaesuFeature(pkt)) => {
                            // Yaesu DSP/function feature-state feedback.
                            if pkt.slot == 0 {
                                state.yaesu_feature_toggles = pkt.toggles;
                                state.yaesu_feature_levels = pkt.levels;
                                state.yaesu_feature_freqs = pkt.freqs;
                            } else {
                                state.yaesu2_feature_toggles = pkt.toggles;
                                state.yaesu2_feature_levels = pkt.levels;
                                state.yaesu2_feature_freqs = pkt.freqs;
                            }
                        }
                        Ok(Packet::TxFilterBand(pkt)) => {
                            // Server reports the current TX modulation filter band;
                            // its presence means setting it is supported.
                            state.tx_filter_low_hz = pkt.low_hz;
                            state.tx_filter_high_hz = pkt.high_hz;
                            // Once per session, on the edge: the line a field log
                            // shows after a reconnect (build 83).
                            if !state.tx_filter_supported {
                                info!(
                                    "TX filter band reported by the server ({}..{} Hz): setting it is supported",
                                    pkt.low_hz, pkt.high_hz
                                );
                            }
                            state.tx_filter_supported = true;
                        }
                        Ok(Packet::AudioVrx(pkt)) => {
                            // Route on pkt.vrx_id: 0 → VRX1 jitter buf,
                            // 1 → VRX2 jitter buf. Unknown ids dropped.
                            // Touch last_audio_received so the
                            // connection-lost watchdog stays happy even
                            // when RX1 is muted and only VRX is active.
                            last_audio_received = Some(Instant::now());
                            let arrival_ms = start.elapsed().as_millis() as u64;
                            let frame = BufferedFrame {
                                sequence: pkt.sequence,
                                timestamp: pkt.timestamp,
                                opus_data: pkt.opus_data,
                                ptt: false,
                                wideband: pkt.wideband,
                            };
                            match pkt.vrx_id {
                                0 => {
                                    // Stream reset detection: server recreates the
                                    // VRX runtime when the wideband toggle changes,
                                    // restarting sequence at 0. Without this reset
                                    // the jitter buffer would drop new frames as
                                    // "too late".
                                    if vrx1_logged_first && pkt.sequence == 0 {
                                        info!("VRX1: stream reset detected, resetting jitter buffer");
                                        vrx1_jitter_buf.reset();
                                        loss_meter.vrx1.reset();
                                    }
                                    // A VRX-only client has no other stream, and
                                    // its loss figure gates its own audio
                                    // protection and its spectrum.
                                    audio_arrived!(vrx1, crate::loss_window::Stream::Vrx1, pkt.sequence, pkt.timestamp, frame.opus_data.len(), &frame.opus_data);
                                    if !vrx1_logged_first {
                                        debug!(
                                            "VRX1 audio: first packet received (seq={}, opus_bytes={}); {} ms after enable",
                                            pkt.sequence, frame.opus_data.len(),
                                            vrx1_enable_at.map(|t| t.elapsed().as_millis() as i64).unwrap_or(-1)
                                        );
                                        vrx1_logged_first = true;
                                    }
                                    vrx1_jitter_buf.push(frame, arrival_ms);
                                    state.vrx1_audio_packets += 1;
                                    state.vrx1_jitter_ms = vrx1_jitter_buf.jitter_ms();
                                    state.vrx1_buffer_depth = vrx1_jitter_buf.depth() as u32;
                                }
                                1 => {
                                    if vrx2_logged_first && pkt.sequence == 0 {
                                        info!("VRX2: stream reset detected, resetting jitter buffer");
                                        vrx2_jitter_buf.reset();
                                        loss_meter.vrx2.reset();
                                    }
                                    audio_arrived!(vrx2, crate::loss_window::Stream::Vrx2, pkt.sequence, pkt.timestamp, frame.opus_data.len(), &frame.opus_data);
                                    if !vrx2_logged_first {
                                        debug!(
                                            "VRX2 audio: first packet received (seq={}, opus_bytes={}); {} ms after enable",
                                            pkt.sequence, frame.opus_data.len(),
                                            vrx2_enable_at.map(|t| t.elapsed().as_millis() as i64).unwrap_or(-1)
                                        );
                                        vrx2_logged_first = true;
                                    }
                                    vrx2_jitter_buf.push(frame, arrival_ms);
                                    state.vrx2_audio_packets += 1;
                                    state.vrx2_jitter_ms = vrx2_jitter_buf.jitter_ms();
                                    state.vrx2_buffer_depth = vrx2_jitter_buf.depth() as u32;
                                }
                                _ => {}
                            }
                        }
                        Ok(Packet::ServerReportPart(part, parts, bytes)) => {
                            // Collected by number, so a gap is a fact rather
                            // than a shorter report. Only when every part is in
                            // does anything become visible to the UI.
                            if parts == 0 || parts as usize > 4096 {
                                // Nonsense: refuse rather than allocate on it.
                                continue;
                            }
                            if server_report_parts.len() != parts as usize {
                                server_report_parts = vec![None; parts as usize];
                                server_report_deadline =
                                    Some(Instant::now() + Duration::from_secs(20));
                            }
                            if let Some(slot) = server_report_parts.get_mut(part as usize) {
                                *slot = Some(bytes);
                            }
                            let have = server_report_parts.iter().filter(|p| p.is_some()).count();
                            if have == parts as usize {
                                let mut all = Vec::new();
                                for p in server_report_parts.iter().flatten() {
                                    all.extend_from_slice(p);
                                }
                                info!("Server report received: {} bytes in {} parts", all.len(), parts);
                                state.server_report =
                                    Some(String::from_utf8_lossy(&all).to_string());
                                state.server_report_failed = None;
                                server_report_parts = Vec::new();
                                server_report_deadline = None;
                            }
                        }
                        Ok(Packet::ServerReportRequest) => {}
                        // (the deadline is checked below, outside the read)
                        Ok(Packet::YaesuMemoryData(text)) => {
                            // One packet type carries two payloads, told apart by the
                            // prefix. They go into separate fields: both are pushed when
                            // a client subscribes, so they arrive in the same instant and
                            // a shared field loses one of them.
                            if text.starts_with("MENU:") {
                                if said_yaesu_menu != Some(text.len()) {
                                    said_yaesu_menu = Some(text.len());
                                    info!("Received Yaesu EX settings ({}B)", text.len());
                                }
                                state.yaesu_menu_data = Some(text);
                                yaesu_menu_data_clear_at = Some(Instant::now() + Duration::from_millis(500));
                            } else {
                                if said_yaesu_mem != Some(text.len()) {
                                    said_yaesu_mem = Some(text.len());
                                    info!("Received Yaesu memory data ({}B)", text.len());
                                }
                                state.yaesu_memory_data = Some(text);
                                yaesu_mem_data_clear_at = Some(Instant::now() + Duration::from_millis(500));
                            }
                        }
                        Ok(Packet::AudioYaesu(pkt)) => {
                            yaesu_audio_in!(pkt, "Radio 1", yaesu_jitter_buf, st_yaesu, yaesu_logged_first, radio1, crate::loss_window::Stream::Radio1,
                                yaesu_audio_packets, yaesu_jitter_ms, yaesu_buffer_depth);
                        }
                        Ok(Packet::PttDenied { server_released, target, server_closed }) => {
                            // Every one of them, with what we did with it.
                            //
                            // The server released a Yaesu on its time-out timer,
                            // said so, and the desktop did nothing at all - no
                            // reaction, no line, while the phone answered within
                            // fifty milliseconds. Whether the packet arrives and
                            // is then dropped, or never arrives, cannot be told
                            // from the outside.
                            info!(
                                "PTT refused by the server (target={:?}, server_released={}, server_closed={}, asking={:?})",
                                target, server_released, server_closed, ptt_requested
                            );
                            // Before the check below: a closed server is news even
                            // when the refusal itself applies to nothing here.
                            if server_closed && !state.server_tx_closed {
                                warn!("The server closed transmitting until it is restarted");
                                state.server_tx_closed = true;
                            }
                            // The outstanding requests, not the transmit
                            // flags: this handler drops those below, so reading
                            // them here answered "did the refusal we are
                            // handling already take effect" - always no on the
                            // first one and always yes on the rest.
                            //
                            // A refusal that arrives after we let go applies to
                            // nothing and is dropped here.
                            // The array itself, not three elements copied out
                            // in an order written by hand. It is already
                            // [Thetis, radio one, radio two] and that is the
                            // order the refusal counts in, so there is nothing
                            // left here to get the wrong way round.
                            let cw_keying = cw_last_edge.map_or(false, |t| t.elapsed() < Duration::from_secs(2));
                            if !state.ptt_denial.apply_refusal(
                                crate::ptt_denial::asking_with_cw(ptt_requested, cw_keying, cw_target),
                                target,
                                server_released,
                            ) {
                                continue;
                            }
                            // And let go - of the transmitters this refusal is
                            // about, and no others. A refusal means this client
                            // is not transmitting on those, so it must stop
                            // believing it is: otherwise it keeps sending TX
                            // audio at a server that is throwing it away, and
                            // the button stays red over a transmission that
                            // never started.
                            //
                            // Yaesu 2 was missing from this list entirely, so a
                            // refusal on the second radio left the engine
                            // believing it was still keyed.
                            if state.ptt_denial.was_about(0) {
                                thetis_ptt = false;
                                ptt = false;
                            }
                            if state.ptt_denial.was_about(1) {
                                yaesu_ptt = false;
                            }
                            if state.ptt_denial.was_about(2) {
                                yaesu2_ptt = false;
                            }
                        }
                        // Dual-radio slot 1 (Option B-prime): exact mirror of slot 0.
                        Ok(Packet::YaesuState2(ys)) => {
                            // connected no longer comes from here; see YaesuPresence (3c).
                            state.yaesu2_freq_a = ys.freq_a;
                            state.yaesu2_freq_b = ys.freq_b;
                            state.yaesu2_mode = ys.mode;
                            state.yaesu2_smeter = ys.smeter;
                            state.ptt_denial.cw_keyed_after_all(2, &mut state.yaesu2_tx_active, ys.tx_active, ys.held_by_other, cw_asked, ptt_requested);
                            state.yaesu2_held_by_other = ys.held_by_other;
                            state.yaesu2_power_on = ys.power_on;
                            state.yaesu2_af_gain = ys.af_gain;
                            state.yaesu2_tx_power = ys.tx_power;
                            state.yaesu2_tx_power_max = ys.tx_power_max;
                            state.yaesu2_loading = ys.loading;
                            state.yaesu2_squelch = ys.squelch;
                            state.yaesu2_rf_gain = ys.rf_gain;
                            state.yaesu2_mic_gain = ys.mic_gain;
                            state.yaesu2_split = ys.split;
                            state.yaesu2_scan = ys.scan;
                            state.yaesu2_tuner_state = ys.tuner_state;
                            state.yaesu2_hi_swr = ys.hi_swr;
                            audio.set_swr_alarm(state.yaesu_hi_swr || state.yaesu2_hi_swr);
                            state.yaesu2_vfo_select = ys.vfo_select;
                            state.yaesu2_memory_channel = ys.memory_channel;
                        }
                        Ok(Packet::AudioYaesu2(pkt)) => {
                            yaesu_audio_in!(pkt, "Radio 2", yaesu2_jitter_buf, st_yaesu2, yaesu2_logged_first, radio2, crate::loss_window::Stream::Radio2,
                                yaesu2_audio_packets, yaesu2_jitter_ms, yaesu2_buffer_depth);
                        }
                        Ok(Packet::RadioInfo { slot, model }) => {
                            // Per-radio model for panel naming ("991A 1"/"FTX1").
                            if slot == 0 { state.yaesu_model = model; }
                            else if slot == 1 { state.yaesu2_model = model; }
                        }
                        Ok(Packet::YaesuPresence(p)) => {
                            // Presence authority:
                            // connected + model come from here (broadcast to all
                            // clients), NOT from the subscription-gated YaesuState. Pure
                            // application in apply_yaesu_presence (unit-tested).
                            let before = (
                                state.radios_known,
                                [state.yaesu_model, state.yaesu2_model],
                                state.yaesu_rx_levels_tuned,
                            );
                            let (c0, c1) = apply_yaesu_presence(&mut state, &p);
                            // Presence may have cleared the hi_swr flag; the alarm
                            // must follow that drop immediately.
                            audio.set_swr_alarm(state.yaesu_hi_swr || state.yaesu2_hi_swr);
                            // Value-change-only logging (L4): shared, so desktop too.
                            if c0 { info!("[radio0] presence: {}", if p.slot0_present { "connected" } else { "disconnected" }); }
                            if c1 { info!("[radio1] presence: {}", if p.slot1_present { "connected" } else { "disconnected" }); }
                            // Which receive correction each radio gets: once per
                            // session (`radios_known` is cleared with the link) and
                            // whenever the model or the server's choice changes.
                            let models = [p.slot0_model, p.slot1_model];
                            for (slot, (present, came)) in [(p.slot0_present, c0), (p.slot1_present, c1)].into_iter().enumerate() {
                                if present && (came || !before.0 || before.1[slot] != models[slot] || before.2 != p.rx_levels_tuned) {
                                    info!(
                                        "[radio{}] receive correction: {} levels, {:+.1} dB ({})",
                                        slot,
                                        if p.rx_levels_tuned { "tuned" } else { "factory" },
                                        20.0 * yaesu_rx_correction(models[slot], p.rx_levels_tuned).log10(),
                                        sdr_remote_core::protocol::radio_model_name(models[slot]),
                                    );
                                }
                            }
                        }
                        Ok(Packet::FrequencyYaesu2(_)) => {} // client→server only
                        Ok(Packet::YaesuMemoryData2(text)) => {
                            if text.starts_with("MENU:") {
                                if said_yaesu2_menu != Some(text.len()) {
                                    said_yaesu2_menu = Some(text.len());
                                    info!("[radio1] received EX settings ({}B)", text.len());
                                }
                                state.yaesu2_menu_data = Some(text);
                                yaesu2_menu_data_clear_at = Some(Instant::now() + Duration::from_millis(500));
                            } else {
                                if said_yaesu2_mem != Some(text.len()) {
                                    said_yaesu2_mem = Some(text.len());
                                    info!("[radio1] received memory data ({}B)", text.len());
                                }
                                state.yaesu2_memory_data = Some(text);
                                yaesu2_mem_data_clear_at = Some(Instant::now() + Duration::from_millis(500));
                            }
                        }
                        Ok(Packet::AuthChallenge(nonce)) => {
                            info!("Auth challenge received");
                            resumed_after_code_step = false;
                            // A challenge means the server does not know us any
                            // more, and it counts us as signed in once the answer
                            // below is in: the old press goes now, not at "Auth
                            // accepted" (build 252).
                            clear_transmit_flags!("A new session is starting while transmitting");
                            // And nothing of the old session is repeated into the
                            // new one: an unacknowledged `YaesuPtt=1` or tune from
                            // before the switch would be applied by it (build 254).
                            // `session_lost!` does the same again at
                            // "Auth accepted", and finds nothing.
                            let open = controls_out.unacknowledged();
                            if open > 0 {
                                info!("A new session is starting: {} unacknowledged control(s) of the old one are not repeated", open);
                            }
                            controls_out.session_ended();
                            if let (Some(ref addr), Some(ref pw)) = (&server_addr, &auth_password) {
                                if test_auth_delay_secs > 0 {
                                    warn!("THETISLINK_TEST_AUTH_DELAY_SECS is set: answering {} s late; a test, not a fault", test_auth_delay_secs);
                                    tokio::time::sleep(Duration::from_secs(test_auth_delay_secs)).await;
                                }
                                // Over QUIC the answer covers this connection's binding.
                                let binding = socket.binding();
                                let hmac = sdr_remote_core::auth::compute_hmac(pw, &nonce, binding.as_ref());
                                // Our nonce goes along, and the key is derived now;
                                // it becomes the session's once the server's proof
                                // verifies.
                                let client_nonce = sdr_remote_core::auth::generate_client_nonce();
                                pending_auth = Some(PendingAuth {
                                    server_nonce: nonce,
                                    client_nonce,
                                    key: sdr_remote_core::auth::session_key(pw, &nonce, &client_nonce, binding.as_ref()),
                                    verified: false,
                                    binding,
                                });
                                let mut buf = [0u8; AUTH_RESPONSE_SIZE_WITH_NONCE];
                                let n = serialize_auth_response(&mut buf, &hmac, Some(&client_nonce));
                                let _ = send_tx!(&buf[..n], addr.as_str());
                                info!("Auth response sent");
                            } else {
                                warn!("Auth challenge received but no password configured");
                                state.auth_rejected = true;
                                state.connect_status = crate::state::ConnectStatus::Failed(
                                    crate::state::ConnectError::WrongPassword,
                                );
                            }
                        }
                        Ok(Packet::AuthResult { .. })
                            if pending_auth.as_ref().map_or(false, |p| result_is_stale(p.binding, socket.binding())) =>
                        {
                            info!("Auth result for a QUIC connection that is gone: waiting for the new one's challenge");
                            pending_auth = None;
                        }
                        Ok(Packet::AuthResult { code: result, proof }) => {
                            // Whatever it says, it is the answer the code step was waiting
                            // for. Invariant three asymmetries lean on: the only
                            // place that sets `totp_required` is the TOTP_REQUIRED arm below
                            // this wipe, so a new code step always starts with an empty
                            // clock - which is why a wrong code may keep the flag and drop
                            // the clock, and `engine_stopped` the other way round.
                            totp_code_sent_at = None;
                            // Phase-based classification of AUTH_REJECTED.
                            // - If we hadn't yet been told "TOTP required" → reject = WrongPassword
                            // - If we had been told TOTP required and just submitted a code → reject = WrongTotp
                            // `state.totp_required` at this moment functions as the phase indicator.
                            // The code step survives the let-go in `pending_auth`
                            // (`verified` is set at the password step, so a refusal
                            // after it can only be a refused code), not in the screen
                            // flag, which the wait cleared: a late refusal of the code
                            // read as a wrong password (measured).
                            let was_in_totp_phase = state.totp_required
                                || pending_auth.as_ref().map_or(false, |p| p.verified);
                            // The server's proof that it knows the password,
                            // checked before the code is believed. `None` is no proof
                            // at all: an older server, or the result after the TOTP
                            // step (the proof went with the password step).
                            let proof_ok = match (&pending_auth, proof) {
                                (Some(p), Some(proof)) => Some(sdr_remote_core::auth::verify_server_proof(
                                    &p.key, &p.server_nonce, &p.client_nonce, &proof, p.binding.as_ref(),
                                )),
                                _ => None,
                            };
                            // The key this result would make the session's: proven now,
                            // or at the password step when 2FA followed (that result
                            // carries no proof of its own).
                            let verified_key = match (&pending_auth, proof_ok) {
                                (Some(p), Some(true)) => Some(p.key),
                                (Some(p), None) if p.verified => Some(p.key),
                                _ => None,
                            };
                            // Four verdicts of this side's own, so the one `match` below
                            // handles them next to the server's codes:
                            // - a proof that does not verify is a stranger's;
                            // - a second ACCEPTED for the verified session (a UDP
                            //   duplicate, a replay) belongs to no authentication in
                            //   flight; believing it would forget the session, key
                            //   included;
                            // - an accepting answer with nothing in flight and no
                            //   verified session (the answer to an attempt a lost link
                            //   just forgot) has nothing to be judged by: ignored
                            //   (field test 2026-10-06);
                            // - an accepting answer without a proof, to an attempt in
                            //   flight, is a server that
                            //   proves nothing - an older server - and this client
                            //   does not connect to one (since 2026-10-02;
                            //   before that it connected with a warning).
                            // The wire knows three codes. Anything else is not one
                            // of this side's verdicts (chosen below) but a sender's:
                            // refused, and said out loud. Refused rather
                            // than ignored, although the server ignores what answers
                            // nothing: a code this build does not know is more likely
                            // a future meaning than line noise, and silence here
                            // would hang the connect with no message.
                            let result = if matches!(
                                result,
                                sdr_remote_core::protocol::AUTH_REJECTED
                                    | sdr_remote_core::protocol::AUTH_ACCEPTED
                                    | sdr_remote_core::protocol::AUTH_TOTP_REQUIRED
                            ) {
                                result
                            } else {
                                warn!("AuthResult with an unknown code 0x{:02x}; refused", result);
                                sdr_remote_core::protocol::AUTH_REJECTED
                            };
                            let accepting = result == sdr_remote_core::protocol::AUTH_ACCEPTED
                                || result == sdr_remote_core::protocol::AUTH_TOTP_REQUIRED;
                            let result = if proof_ok == Some(false) {
                                PROOF_INVALID
                            } else if accepting && verified_key.is_none() {
                                if pending_auth.is_none() {
                                    if result == sdr_remote_core::protocol::AUTH_ACCEPTED && session_key.is_some() {
                                        RESULT_REPEATED
                                    } else {
                                        RESULT_NOT_IN_FLIGHT
                                    }
                                } else {
                                    // An attempt in flight, and an answer without a proof.
                                    RESULT_NO_PROOF
                                }
                            } else {
                                result
                            };
                            match result {
                                RESULT_REPEATED => {
                                    info!("Auth accepted: repeated result for the verified session, ignored");
                                }
                                RESULT_NOT_IN_FLIGHT => {
                                    info!("Auth accepted: no attempt in flight (a late answer after the link was lost, or a stray one); ignored, the next challenge starts a new attempt");
                                }
                                RESULT_NO_PROOF => {
                                    warn!("Auth accepted by a server that proves nothing (an attempt in flight, no proof in the answer): refused; update the server");
                                    leave_server!(tell);
                                    pending_auth = None;
                                    session_key = None;
                                    state.auth_rejected = true;
                                    _auth_completed = false;
                                    state.connect_status = crate::state::ConnectStatus::Failed(
                                        crate::state::ConnectError::ServerProvesNothing,
                                    );
                                }
                                PROOF_INVALID => {
                                    // Whatever the code says: a server that cannot prove
                                    // it knows the password is not ours - and the key of
                                    // the session it was replacing goes with it.
                                    warn!("Auth rejected: server proof invalid");
                                    leave_server!(tell);
                                    pending_auth = None;
                                    session_key = None;
                                    state.auth_rejected = true;
                                    _auth_completed = false;
                                    state.connect_status = crate::state::ConnectStatus::Failed(
                                        crate::state::ConnectError::ServerUnverified,
                                    );
                                }
                                sdr_remote_core::protocol::AUTH_ACCEPTED => {
                                    info!("Auth accepted");
                                    // Having to authenticate means the server does not
                                    // know us any more: whatever the old session left in
                                    // the state goes, as on every other way a session is
                                    // lost - the transmit flags too. A server restarted
                                    // inside the time-out used to keep all of it: the
                                    // "safe mode" text stood over a server that was open
                                    // again (field test build 59, 2026-10-01 13:31).
                                    // Nothing to clear on a first connect.
                                    session_lost!().replaced_by_a_new_session();
                                    // Whose session is this? Verified now, or at
                                    // the password step (the TOTP result carries no proof).
                                    // Verified above: every path that reaches this arm
                                    // has a key (the others became verdicts of their own).
                                    session_key = verified_key;
                                    info!("Auth accepted: server verified");
                                    // Which way this session runs goes into the file: two
                                    // measurements of one build over two transports must differ.
                                    let route = socket.route();
                                    measure.event(start.elapsed().as_millis() as u64, crate::measure::Event::SessionStart(route));
                                    _auth_completed = true;
                                    state.auth_rejected = false;
                                    state.totp_required = false;
                                    state.connect_status = crate::state::ConnectStatus::Connected;
                                    // Having to authenticate means the other side does
                                    // not know us any more, so "we are connected" is not
                                    // true whatever this side still believed.
                                    //
                                    // Without this, a server that restarted and came back
                                    // INSIDE the connection timeout - which needs both the
                                    // heartbeat and the audio to stay away for
                                    // max(6s, rtt*8) - left `was_connected` standing. The
                                    // restore block below is gated on `!was_connected`, so
                                    // it was skipped entirely: no re-subscription, and the
                                    // server kept the defaults of its fresh session. RX1
                                    // audio is on by default and everything the client has
                                    // to ask for is not, which is exactly the reported
                                    // "only RX1 comes back". It repaired itself a
                                    // timeout later, on the next authentication, which is
                                    // the seventeen to twenty seconds in the log.
                                    //
                                    // The tell: the noise from the
                                    // draining jitter buffer IS that window. Restart
                                    // inside the noise and the client never noticed it had
                                    // been away (2026-08-16).
                                    //
                                    // Idempotent on a first connect, where it is already
                                    // false.
                                    was_connected = false;
                                    // And a number the UI can compare against,
                                    // because clearing the flag above removed the
                                    // only way it used to hear about this at all:
                                    // `state.connected` goes false in the timeout
                                    // path, and that path is exactly what this fix
                                    // makes unnecessary. Fixing the engine's own
                                    // restore while leaving the UI's on a flank
                                    // traded one half-restore for another - RX2
                                    // came back and VRX stopped coming back at all
                                    // (2026-08-16, seen in the field within the
                                    // hour).
                                    state.session_generation =
                                        state.session_generation.wrapping_add(1);
                                    // A new session starts from silence. The gap
                                    // the reconnect just left belongs to the old
                                    // one, and charging this one for it is what
                                    // made the server switch on error correction
                                    // two seconds after every connect and then
                                    // have no reason to let go.
                                    loss_meter.reset();
                                    last_worst_stream = None;
                                    last_stale_windows = 0;
                                }
                                sdr_remote_core::protocol::AUTH_TOTP_REQUIRED => {
                                    info!("Password OK, TOTP required");
                                    // Verified at this step (an answer without a proof
                                    // became RESULT_NO_PROOF above); the result after
                                    // the code carries no proof of its own.
                                    if let Some(p) = pending_auth.as_mut() {
                                        p.verified = true;
                                    }
                                    info!("Server verified at the password step");
                                    state.auth_rejected = false;
                                    state.totp_required = true;
                                    state.connect_status =
                                        crate::state::ConnectStatus::AwaitingTotp;
                                }
                                // A refusal with no attempt in progress answers
                                // nothing: a UDP duplicate of an earlier refusal,
                                // landing on a session that was since admitted. The
                                // accepting side has had this wait since build 67
                                // (RESULT_REPEATED); without it here one late
                                // duplicate took a live session down - "wrong
                                // password", the address gone, and the server still
                                // holding the client as admitted (measured).
                                // The server applies the same rule to a
                                // response with nothing pending.
                                _ if pending_auth.is_none() => {
                                    info!("Auth rejected: no attempt in progress, a repeated or stray result; ignored");
                                }
                                _ => {
                                    warn!("Auth rejected");
                                    // A refused PASSWORD ends the attempt: the next
                                    // one starts with a fresh challenge. A refused
                                    // CODE does not - the server keeps the code step
                                    // open for another code, and the ACCEPTED that
                                    // answers the right one carries no proof of its
                                    // own: it is verified by the nonces kept here.
                                    // Clearing them made that ACCEPTED read as a
                                    // server that proves nothing, with the server
                                    // already holding the client as admitted
                                    // (measured). Third place in the row of the
                                    // wait and `session_lost!()`: the
                                    // attempt ends with the session, not with a
                                    // refused code.
                                    if !was_in_totp_phase {
                                        // And nothing more goes to this address until
                                        // the operator connects again: the server has
                                        // no session to keep alive, and one of v2.13+
                                        // challenges the next packet at once - a
                                        // heartbeat every half second would answer it
                                        // with the same wrong password five times and
                                        // block the address for a minute. No Disconnect:
                                        // there is nothing there to end.
                                        pending_auth = None;
                                        leave_server!(silent);
                                    }
                                    session_key = None;
                                    state.auth_rejected = true;
                                    _auth_completed = false;
                                    state.connect_status = if was_in_totp_phase {
                                        crate::state::ConnectStatus::Failed(
                                            crate::state::ConnectError::WrongTotp,
                                        )
                                    } else {
                                        crate::state::ConnectStatus::Failed(
                                            crate::state::ConnectError::WrongPassword,
                                        )
                                    };
                                }
                            }
                        }
                        Ok(Packet::ControlAck(seq)) => {
                            if !controls_out.server_acknowledges() {
                                info!("The server acknowledges controls: lost ones are sent again from now on");
                            }
                            controls_out.acknowledged(seq, Instant::now());
                        }
                        Ok(Packet::TotpChallenge) => {
                            info!("TOTP challenge received");
                        }
                        Ok(Packet::AuthResponse { .. }) | Ok(Packet::TotpResponse(_)) => {} // server-only
                        // Hole punching. The route layer takes these from
                        // build 124; until then a server that sends one is
                        // ahead of this client, and nothing happens.
                        Ok(Packet::PunchReady(_)) | Ok(Packet::PunchOffer(_)) | Ok(Packet::PunchLocal(_)) => {}
                        Ok(Packet::Disconnect) => {
                            info!("Server sent disconnect");
                            // Re-baseline all audio streams on a server-initiated
                            // disconnect (server restart!) - this is the path the
                            // Yaesu-audio-stall hit; don't lean only on the backjump
                            // heuristic (also fixes the short-session stall).
                            reset_jitter_buffers!();
                            yaesu_logged_first = false;
                            yaesu2_logged_first = false;
                            vrx1_logged_first = false;
                            vrx2_logged_first = false;
                            was_connected = false;
                            last_hb_ack_time = None;
                            last_hb_ack_rtt = 0;
                            rx_volume_synced = false;
                            rx2_volume_synced = false;
                            state.rx_af_gain = 0;
                            // The link is gone, so nothing is outstanding any more. Three of
                            // the four link-down paths did not do this, and each one left the
                            // "PTT blocked" sign standing over a connection that no longer
                            // exists.
                            let forgotten = session_lost!();
                            state.link_down(forgotten);
                            state.connect_status = crate::state::ConnectStatus::Disconnected;
                            state.rtt_ms = 0;
                            state.jitter_ms = 0.0;
                            state.buffer_depth = 0;
                            // Clear stale spectrum data
                            state.spectrum_bins.clear();
                            state.full_spectrum_bins.clear();
                            state.spectrum_sequence = 0;
                            state.full_spectrum_sequence = 0;
                        }
                        Err(e) => {
                            // Distinguish protocol-version mismatch from generic
                            // malformed bytes, but only during the connect phase;
                            // during a normal session we just log and keep running
                            // (single bad packet is not fatal).
                            let is_connecting = matches!(
                                state.connect_status,
                                crate::state::ConnectStatus::Connecting
                            );
                            if is_connecting {
                                let server_addr_str =
                                    server_addr.clone().unwrap_or_default();
                                if data.len() >= 2
                                    && data[0] == sdr_remote_core::protocol::MAGIC
                                    && data[1] != sdr_remote_core::protocol::VERSION
                                {
                                    state.connect_status =
                                        crate::state::ConnectStatus::Failed(
                                            crate::state::ConnectError::ProtocolVersionMismatch {
                                                server_version: data[1],
                                                client_version:
                                                    sdr_remote_core::protocol::VERSION,
                                            },
                                        );
                                } else {
                                    state.connect_status =
                                        crate::state::ConnectStatus::Failed(
                                            crate::state::ConnectError::MalformedResponse {
                                                addr: server_addr_str,
                                                detail: format!("{}", e),
                                            },
                                        );
                                }
                            }
                            warn!("Invalid packet ({}B): {}", len, e);
                        }
                    }

                    // Any reply (even a bad one) means the server-port replied,
                    // useful for distinguishing "wrong bytes" from "no reply at all".
                    connect_any_reply_seen = true;

                    let _ = self.state_tx.send(state.clone());
                }

                _ = audio_tick.tick() => {
                    // The connection name (build 139): only while a session runs.
                    let path = if !state.connected {
                        None
                    } else if let Some(route) = &relay_path {
                        Some(kind_through_the_relay(route.path()))
                    } else {
                        server_sockaddr.map(|a| sdr_remote_core::path::PathKind::without_relay(a.ip()))
                    };
                    if path != state.path {
                        if let Some(kind) = path {
                            info!("Connection: {} ({})", kind.short(), kind.log_word());
                            measure.event(start.elapsed().as_millis() as u64, crate::measure::Event::Path(kind));
                        }
                        state.path = path;
                        let _ = self.state_tx.send(state.clone());
                    }
                    if let Some(grants) = &relay_grants {
                        let changed = udp_blocked_watch.observe(grants.borrow().as_ref(), Instant::now());
                        if let Some(blocked) = changed {
                            if blocked {
                                warn!(
                                    "UDP to the relay blocked: signed in, but no UDP seen by the relay within {} s; the traffic goes over the relay's TCP line",
                                    crate::udp_blocked::UDP_SEEN_WAIT.as_secs()
                                );
                            } else {
                                info!("UDP to the relay seen again: the notice goes");
                            }
                            state.udp_blocked = blocked;
                            let _ = self.state_tx.send(state.clone());
                        }
                    }
                    // A CW run that fell silent stops asking; a refusal that
                    // was about it comes down with it, unless the microphone
                    // is still asking for the same transmitter.
                    if let Some(t) = cw_last_edge {
                        if t.elapsed() >= Duration::from_secs(2) {
                            cw_last_edge = None;
                            state.ptt_denial.cw_run_over(cw_asked, ptt_requested);
                            cw_asked = 0;
                        }
                    }
                    // Keep the PA-drive beep alive from the state, not from whatever
                    // packet happens to arrive. It used to be re-armed beside the SWR
                    // alarm on Yaesu state pushes, and on the phone those come in far
                    // slower: the beep fell silent between them and repeated about
                    // once every ten seconds (heard in the field, build 57). The
                    // watchdog in the backend still ends it if these ticks stop.
                    audio.set_pa_drive_alarm(state.pa_drive_alarm);

                    // Thetis streams off while a Yaesu is being heard, after a
                    // grace so switching straight back costs nothing. The
                    // decision is a pure function with its own tests; this is
                    // only the clock and the sockets.
                    {
                        use crate::thetis_streams::{thetis_stream_action, ThetisStreamAction,
                                                    THETIS_STREAM_GRACE};
                        let since = yaesu_listening_since.map(|t| t.elapsed());
                        let act = thetis_stream_action(
                            yaesu_listening, thetis_streams_stopped, since, THETIS_STREAM_GRACE);
                        let want = act == ThetisStreamAction::Resume;
                        // Nothing to do means no failing run either. Without this
                        // the back-off state outlived the run it belonged to: a
                        // failed stop, the operator switches back, and ten minutes
                        // later a successful stop announces that it "reached the
                        // socket after all" about something long gone.
                        if act == ThetisStreamAction::None {
                            thetis_stream_retry_at = None;
                        }
                        // Resuming never waits. The module says so in as many
                        // words, and the back-off broke that promise for a full
                        // second - precisely while the operator is looking at a
                        // waterfall that has stopped moving.
                        let backing_off = !want
                            && thetis_stream_retry_at.map_or(false, |t| Instant::now() < t);
                        if act != ThetisStreamAction::None && !backing_off {
                            if let Some(ref addr) = server_addr {
                                // Computed once, used for the wire AND for the log
                                // line. The log used to call the shared function
                                // too, which put two non-decisions into a count
                                // that is supposed to count decisions.
                                let rx1_wire =
                                    crate::thetis_streams::stream_value(state.rx1_enabled, !want);
                                let spec_wire =
                                    crate::thetis_streams::stream_value(spectrum_enabled, !want);
                                let rx_ok = send_control!(ControlId::Rx1Enable, rx1_wire as u16, addr.as_str()).is_ok();
                                let sp_ok = send_control!(ControlId::SpectrumEnable, spec_wire as u16, addr.as_str()).is_ok();
                                // The flag follows what LEFT, not what was meant.
                                // It drives the restore block after a new session,
                                // so a flag set on an intent that never reached the
                                // server would keep handing back a stream the
                                // server is still sending - and nothing would say
                                // so - and that is worse here, because the flag
                                // drives the restore block.
                                if rx_ok && sp_ok {
                                    thetis_streams_stopped = !want;
                                    // The spectrum opens again on the wire: the size
                                    // its rows show goes with it, as on an open.
                                    if spec_wire && spectrum_fft_size_k != 0 {
                                        let _ = send_control!(ControlId::SpectrumFftSize, spectrum_fft_size_k, addr.as_str());
                                    }
                                    if thetis_stream_retry_at.take().is_some() {
                                        info!("Thetis stream change reached the socket after all");
                                    }
                                    thetis_stream_warned_at = None;
                                    // Only on success. It used to sit outside this
                                    // branch and announced a stop that had not
                                    // happened - a log line lying about the very
                                    // thing it was added to prove.
                                    info!(
                                        "Thetis streams {} (listening radio is {}), rx1={} spectrum={}",
                                        if want { "resumed" } else { "stopped to save data" },
                                        if yaesu_listening { "Yaesu" } else { "Thetis" },
                                        rx1_wire,
                                        spec_wire,
                                    );
                                } else {
                                    // Once a minute while it keeps failing. Once per
                                    // RUN was the first attempt and it is wrong the
                                    // other way: a fault lasting hours left a single
                                    // line at the very start, in a log that rolls, so
                                    // the operator sees high data use and "Thetis
                                    // off" on screen with nothing to explain it. A
                                    // line a minute is ninety kilobytes over ten
                                    // hours, which the log can carry.
                                    let now = Instant::now();
                                    let due = thetis_stream_warned_at
                                        .map_or(true, |t| now.duration_since(t).as_secs() >= 60);
                                    if due {
                                        warn!(
                                            "Thetis stream change did not leave the socket (rx1_ok={} spectrum_ok={}); retrying every second, reporting once a minute",
                                            rx_ok, sp_ok
                                        );
                                        thetis_stream_warned_at = Some(now);
                                    }
                                    thetis_stream_retry_at = Some(now + Duration::from_secs(1));
                                }
                            }
                        }
                    }
                    // Playout: always pull frames from jitter buffer and decode.
                    // This keeps the decoder warm and jitter buffer healthy during TX.
                    // Only write to playback ring buffer when not in TX (muted callback
                    // drains the ring during TX anyway).
                    {
                        // The ring's target and ceiling in ms, through the one conversion that
                        // knows a desktop frame is two items (build 158; it used to count them
                        // as one, so the desktop ran at half of both).
                        let target_ring_low = crate::measure::ring_items(crate::after_tx::RING_TARGET_MS, playback_rate, stereo_output);
                        let target_ring_high = crate::measure::ring_items(crate::after_tx::RING_CEILING_MS, playback_rate, stereo_output);
                        let ring_level = audio.playback_buffer_level();
                        after_tx.ptt(ptt, Instant::now());
                        after_tx_yaesu.ptt(yaesu_ptt, Instant::now());
                        after_tx_yaesu2.ptt(yaesu2_ptt, Instant::now());
                        // What this tick already added to the ring, for the cushion.
                        let mut after_tx_added_ms: u64 = 0;

                        // Right after a release the ring is meant to stand at the
                        // cushion, so it refills only to there - one frame per tick
                        // once it stands (build 107, `AfterTx::refill_low`).
                        let refill_low = after_tx.refill_low(target_ring_low, playback_rate, stereo_output, Instant::now());
                        let skip_this_tick = !ptt && ring_level > target_ring_high;
                        // How many frames go in this tick, for every stream: Thetis
                        // pulls this many, and a radio or VRX that plays without Thetis
                        // brings this many (build 156; it used to bring one, always).
                        let frames_wanted = crate::after_tx::frames_for_tick(ring_level, refill_low);
                        let frame_samples = (playback_rate as usize * 20) / 1000; // 20 ms
                        // The rule's decision for the measurement, read before anything goes in
                        // (build 157): the `buffer` line reads the ring after the write.
                        let ring_start_ms = crate::measure::ring_ms(ring_level, playback_rate, stereo_output);
                        // A ring that has stood a frame or more above its target for a second
                        // gives this tick's frame back (build 158, `after_tx::Settle`).
                        let drain_this_tick = !skip_this_tick && ring_settle.drain(ring_start_ms, ptt);
                        ring_ticks.tick(ring_start_ms, frames_wanted, skip_this_tick || drain_this_tick);

                        let mut frames_this_tick = 0u32;
                        // Accumulate output samples for mixing with RX2
                        let mut playback_buf: Vec<f32> = Vec::new();
                        // Right channel buffer - filled from stereo decode
                        let mut bin_r_buf: Vec<f32> = Vec::new();
                        let mut rx1_level_accum: f32 = 0.0;
                        let mut rx1_level_count: usize = 0;
                        let mut rx2_level_accum: f32 = 0.0;
                        let mut rx2_level_count: usize = 0;
                        let mut bin_r_level_accum: f32 = 0.0;
                        let mut bin_r_level_count: usize = 0;
                        // Every stream measures its energy BEFORE its volume is applied,
                        // so a bar shows what the link delivers, not what the local
                        // volume slider leaves of it.
                        let mut yaesu_level_accum: f32 = 0.0;
                        let mut yaesu_level_count: usize = 0;
                        let mut yaesu2_level_accum: f32 = 0.0;
                        let mut yaesu2_level_count: usize = 0;
                        let mut vrx1_level_accum: f32 = 0.0;
                        let mut vrx1_level_count: usize = 0;
                        let mut vrx2_level_accum: f32 = 0.0;
                        let mut vrx2_level_count: usize = 0;
                        // RX1's pre-volume energy of the frame being processed, so the
                        // binaural-R fallback (R = copy of L) can report the same level
                        // without measuring the volume-scaled copy.
                        let mut rx1_pre_sq: f32 = 0.0;
                        let mut rx1_pre_len: usize = 0;

                        if !skip_this_tick {
                            loop {
                                // One frame a tick, two while the ring is below the
                                // refill threshold (the rule lives in after_tx, where
                                // its dynamics after a release are simulated).
                                if !crate::after_tx::keep_pulling(frames_this_tick, ring_level, refill_low) {
                                    break;
                                }

                                // Pull one multi-channel frame. Whatever comes back -
                                // received, rebuilt from the redundancy in the next
                                // packet, or filled in - arrives in the same shape and
                                // has already been through the resampler that matches
                                // its own format. The routing below cannot tell them
                                // apart, which is the point: the concealed frame used
                                // to be written straight into the playback buffers,
                                // skipping the audio-mode routing, the recorders and
                                // the level meters (2026-08-16).
                                let mut rx1_d: Option<Decoded> = None;
                                let mut bin_r_d: Option<Decoded> = None;
                                let mut rx2_d: Option<Decoded> = None;
                                let mut nothing_arriving = false;

                                match jitter_buf.pull() {
                                    JitterResult::Frame(frame) => {
                                        frames_this_tick += 1;
                                        let wb = frame.wideband;
                                        let blob = &frame.opus_data;
                                        if !blob.is_empty() {
                                            rx1_d = channel_opus(blob, 0).and_then(|o| st_rx1.decode(o, wb));
                                            bin_r_d = channel_opus(blob, 1).and_then(|o| st_bin_r.decode(o, wb));
                                            rx2_d = channel_opus(blob, 2).and_then(|o| st_rx2.decode(o, wb));
                                        }
                                    }
                                    JitterResult::Missing => {
                                        frames_this_tick += 1;
                                        // One frame lost out of a stream that is still
                                        // arriving. Every channel is asked, so no
                                        // decoder is left without its call on a gap -
                                        // only channel 0 used to get one, and the other
                                        // two picked their history up again mid-stream.
                                        let next = next_after_gap!(jitter_buf);
                                        rx1_d = recover_or_conceal(&mut st_rx1, next, 0);
                                        bin_r_d = recover_or_conceal(&mut st_bin_r, next, 1);
                                        rx2_d = recover_or_conceal(&mut st_rx2, next, 2);
                                    }
                                    JitterResult::NotReady => {
                                        // Nothing arriving at all. Conceal for as long
                                        // as the link still counts as up, so a dropout
                                        // sounds like the band rather than like the
                                        // audio stopping. That sound is Opus' own
                                        // concealment and nothing else: it extrapolates
                                        // the operator's signal, which is why it passes
                                        // for their own receiver. On band noise it does
                                        // not fade - measured across four seconds it
                                        // holds the level.
                                        // ...but only for a channel the operator
                                        // still wants. Switching audio off stops the
                                        // stream at the server, which arrives here as
                                        // the same silence a dropout makes - so the
                                        // concealer kept filling it and muting stayed
                                        // audible. Before the eight-second backstop
                                        // existed it never stopped at all: the session
                                        // is alive while muted, so it ran for minutes.
                                        //
                                        // The loss meter below already knows this
                                        // ("absence is then not loss" - it stops
                                        // counting 100% loss for a VRX-only client).
                                        // The same fact simply never reached the
                                        // concealer. The auxiliary streams gate on
                                        // their own _wanted flags for exactly this;
                                        // the main path was the one that did not.
                                        //
                                        // NOTE: these flags are not the operator's
                                        // alone - the server echoes Rx1Enable/Rx2Enable
                                        // back into them, and a fresh server session
                                        // defaults rx2_enabled to false. So an echo can
                                        // switch RX2 concealment off while the operator's
                                        // button still reads on. That is coherent (no
                                        // subscription, no audio to conceal) but it is
                                        // not only the button.
                                        if was_connected && logged_first_rx {
                                            if state.rx1_enabled {
                                                rx1_d = st_rx1.conceal();
                                                bin_r_d = st_bin_r.conceal();
                                            }
                                            if state.rx2_enabled {
                                                rx2_d = st_rx2.conceal();
                                            }
                                        }
                                        nothing_arriving = true;
                                    }
                                }

                                if rx1_d.is_some() || bin_r_d.is_some() || rx2_d.is_some() {
                                    // Measurement mode: one line per frame in which ANY channel of
                                    // the bundle was filled in - RX1, RX2 or binaural right - so an
                                    // operator listening to RX2 leaves a trace too.
                                    if [&rx1_d, &rx2_d, &bin_r_d].iter().any(|d| d.as_ref().is_some_and(|d| d.concealed)) {
                                        measure.conceal(start.elapsed().as_millis() as u64, crate::loss_window::Stream::Thetis);
                                    }
                                    let concealed = rx1_d.as_ref().map(|d| d.concealed).unwrap_or(false);
                                    if concealed {
                                        let peak = rx1_d.as_ref()
                                            .map(|d| d.dev.iter().fold(0.0f32, |m, x| m.max(x.abs())))
                                            .unwrap_or(0.0);
                                        if conceal_since.is_none() {
                                            conceal_since = Some(Instant::now());
                                            conceal_first_peak = peak;
                                            conceal_frames = 0;
                                            conceal_said = Instant::now();
                                        }
                                        conceal_frames += 1;
                                        // One line a second while a gap lasts. The two
                                        // peaks side by side are the shape of the gap:
                                        // where it started and where it is now. They
                                        // should stay in the same neighbourhood rather
                                        // than walking down to nothing - a gap that
                                        // fades is the tell that concealment is running
                                        // on a decoder without the history.
                                        if conceal_said.elapsed() >= Duration::from_secs(1) {
                                            conceal_said = Instant::now();
                                            info!(
                                                "concealing: {} frame(s) {}, peak {:.4} -> {:.4}, ring={}",
                                                conceal_frames,
                                                if st_rx1.wideband() { "wideband" } else { "narrowband" },
                                                conceal_first_peak, peak,
                                                audio.playback_buffer_level()
                                            );
                                        }
                                    }

                                    // Recorders get the audio at the rate it was
                                    // decoded at, concealed frames included: a
                                    // recording that skips them is shorter than what
                                    // was heard and drifts against the clock.
                                    if let (Some(w), Some(d)) = (rec_rx1.as_mut(), rx1_d.as_ref()) {
                                        let _ = w.write_samples(&d.pcm, d.rate);
                                    }
                                    if let (Some(w), Some(d)) = (rec_rx2.as_mut(), rx2_d.as_ref()) {
                                        let _ = w.write_samples(&d.pcm, d.rate);
                                    }

                                    // RX1 -> always L
                                    let mut left_dev = if let Some(d) = rx1_d {
                                        let concealed = d.concealed;
                                        let mut dev = d.dev;
                                        // Level BEFORE volume: the meter answers "is
                                        // this stream carrying audio", which must not
                                        // move when the operator turns the volume down.
                                        //
                                        // And concealed audio is not arriving audio, so
                                        // it is deliberately left out. The bar is the
                                        // instrument for "is anything still coming in" -
                                        // it is how the VRX start-up problem was finally
                                        // pinned down - and concealment on a dead link
                                        // would have it claiming signal for as long as
                                        // the concealment lasts.
                                        if !concealed {
                                            let sq: f32 = dev.iter().map(|s| s*s).sum();
                                            rx1_level_accum += sq;
                                            rx1_level_count += dev.len();
                                            rx1_pre_sq = sq;
                                            rx1_pre_len = dev.len();
                                        }
                                        apply_volume(&mut dev, rx_volume * vfo_a_volume * local_volume);
                                        dev
                                    } else { Vec::new() };

                                    // RX2 once (reused in Mono, BIN, Split)
                                    let rx2_dev = rx2_d.map(|d| {
                                        let concealed = d.concealed;
                                        let mut dev = d.dev;
                                        if !concealed {
                                            let sq: f32 = dev.iter().map(|s| s*s).sum();
                                            rx2_level_accum += sq;
                                            rx2_level_count += dev.len();
                                        }
                                        let rx2_vol = rx2_volume * vfo_b_volume * local_volume;
                                        apply_volume(&mut dev, rx2_vol);
                                        dev
                                    });

                                    // RX1 audio can be deliberately off (Rx1Enable):
                                    // then left_dev is empty. RX2 is mixed additively into L
                                    // (Mono/BIN) and the output-gate only writes if L
                                    // is not empty - so without this seed RX2 audio drops out
                                    // while the level bar (measured before the mix) does move.
                                    // Seed L with silence at RX2 length so RX2 stays audible
                                    // without RX1 audio. VRX has its own mix path (unaffected).
                                    if left_dev.is_empty() {
                                        if let Some(ref rx2) = rx2_dev {
                                            left_dev = vec![0.0; rx2.len()];
                                        }
                                    }

                                    // In Mono and BIN: mix RX2 additively into L
                                    if (audio_mode == 0 || audio_mode == 1) && stereo_output {
                                        if let Some(ref rx2) = rx2_dev {
                                            for (i, s) in rx2.iter().enumerate() {
                                                if i < left_dev.len() {
                                                    left_dev[i] = (left_dev[i] + s).clamp(-1.0, 1.0);
                                                }
                                            }
                                        }
                                    }

                                    let mut right_dev = if !stereo_output || audio_mode == 0 {
                                        // Android or Mono: L only -> both ears
                                        Vec::new()
                                    } else if audio_mode == 1 {
                                        // BIN: R = binaural right (ch1), volume = RX1.
                                        // Concealed frames land here too, so the stereo
                                        // image no longer collapses to a copy of L for
                                        // the length of every gap.
                                        if let Some(d) = bin_r_d {
                                            let concealed = d.concealed;
                                            let mut dev = d.dev;
                                            // Pre-volume, and before RX2 is mixed in below:
                                            // this bar is the pure RX1-R channel.
                                            if !concealed {
                                                bin_r_level_accum += dev.iter().map(|s| s * s).sum::<f32>();
                                                bin_r_level_count += dev.len();
                                            }
                                            apply_volume(&mut dev, rx_volume * vfo_a_volume * local_volume);
                                            dev
                                        } else {
                                            // Fallback: R is a copy of L, so it carries RX1's level.
                                            bin_r_level_accum += rx1_pre_sq;
                                            bin_r_level_count += rx1_pre_len;
                                            left_dev.clone()
                                        }
                                    } else {
                                        // Split: R = RX2 directly
                                        rx2_dev.clone().unwrap_or_default()
                                    };

                                    // In BIN: also mix RX2 into R channel
                                    if audio_mode == 1 {
                                        if let Some(ref rx2) = rx2_dev {
                                            for (i, s) in rx2.iter().enumerate() {
                                                if i < right_dev.len() {
                                                    right_dev[i] = (right_dev[i] + s).clamp(-1.0, 1.0);
                                                }
                                            }
                                        }
                                    }

                                    // Write to playback buffers - after a PTT release not the
                                    // silence Thetis sends while switching back, above a
                                    // cushion (build 106: it held the first sound back ~80 ms).
                                    let after_tx_write = if !ptt && !left_dev.is_empty() {
                                        let frame_ms = left_dev.len() as u64 * 1000 / playback_rate.max(1) as u64;
                                        let ring_now = crate::measure::ring_ms(ring_level, playback_rate, stereo_output) + after_tx_added_ms;
                                        let sound = crate::after_tx::peak(&left_dev).max(crate::after_tx::peak(&right_dev));
                                        let (write, outcome) = after_tx.frame(sound, ring_now, frame_ms, Instant::now());
                                        match outcome {
                                            // Named before it was built.
                                            // A developer's line (build 295): one per release.
                                            Some(crate::after_tx::Outcome::FirstSound { after_ms, skipped_ms, ring_ms }) => dev_info!(
                                                "RX after TX: first sound {} ms after the release, {} ms of silence skipped, ring {} ms, jitter depth {}",
                                                after_ms, skipped_ms, ring_ms, jitter_buf.depth()
                                            ),
                                            Some(crate::after_tx::Outcome::NoSound { skipped_ms }) => dev_info!(
                                                "RX after TX: no sound within {} ms, {} ms of silence skipped, jitter depth {}",
                                                crate::after_tx::WINDOW.as_millis(), skipped_ms, jitter_buf.depth()
                                            ),
                                            None => {}
                                        }
                                        if write {
                                            after_tx_added_ms += frame_ms;
                                        }
                                        write
                                    } else {
                                        false
                                    };
                                    if after_tx_write {
                                        playback_buf.extend_from_slice(&left_dev);
                                        if right_dev.is_empty() {
                                            bin_r_buf.extend_from_slice(&left_dev); // mono: L to both
                                        } else {
                                            bin_r_buf.extend_from_slice(&right_dev);
                                        }
                                    }
                                }

                                // Nothing in the buffer: one concealed frame per tick is
                                // all that helps - pulling again would only add latency
                                // for audio that is not there.
                                if nothing_arriving {
                                    break;
                                }
                            }

                            // RX1 level (measured per-channel before mono mix)
                            if rx1_level_count > 0 {
                                state.playback_level = (rx1_level_accum / rx1_level_count as f32).sqrt();
                            } else {
                                decay_level(&mut state.playback_level);
                            }
                            // RX2 level (measured per-channel before mono mix)
                            if rx2_level_count > 0 {
                                state.playback_level_rx2 = (rx2_level_accum / rx2_level_count as f32).sqrt();
                            } else {
                                decay_level(&mut state.playback_level_rx2);
                            }

                            // The two Yaesu receive streams, one block for both radios (build
                            // 180; they were two copies, and had already grown apart in the
                            // level bar). Each radio has its own jitter buffer, decoder,
                            // recorder, level, volume, model and PTT; the ring is shared.
                            // A macro and not a function: the block reads a dozen of the
                            // tick's locals, as `rebuild_or_conceal!` does.
                            // Read before the block: a macro sees the locals of where it is written.
                            // Until this session's first presence packet the client does not know
                            // which table the server chose (`forget_session` clears it with the
                            // link), while a radio's audio can already arrive. Then the small one:
                            // at worst a moment too quiet, never 22 dB too loud (build 63).
                            let rx_levels_tuned = state.yaesu_rx_levels_tuned || !state.radios_known;
                            macro_rules! mix_yaesu_rx {
                                ($stream:expr, $name:literal, $logged_first:ident, $jb:ident, $wanted:ident,
                                 $st:ident, $rec:ident, $accum:ident, $count:ident, $volume:ident,
                                 $model:ident, $ptt:ident, $after_tx:ident, $level:ident) => {
                                    // The level write sits inside the guard below, so the
                                    // fall-back lives outside it - same shape as VRX.
                                    if !($logged_first && $jb.depth() > 0) {
                                        decay_level(&mut state.$level);
                                    }
                                    // `|| concealing`: a dry buffer used to skip this block
                                    // entirely, so a switched-on stream had no way to conceal a
                                    // dropout - only a gap with real frames on both sides.
                                    if $logged_first
                                        && ($jb.depth() > 0 || (was_connected && $wanted)) {
                                        // One gain for the speaker, the bar and the recording.
                                        let rx_gain = yaesu_rx_correction(state.$model, rx_levels_tuned);
                                        // One ring: as long as what was written before, or the tick's count
                                        // when nothing was (build 156).
                                        let opened = crate::after_tx::open_tick(&mut playback_buf, frames_wanted, frame_samples);
                                        let target_samples = opened.target;
                                        let mut radio_buf: Vec<f32> = Vec::with_capacity(target_samples);
                                        while radio_buf.len() < target_samples {
                                            let mut dry = false;
                                            let decoded = match $jb.pull() {
                                                JitterResult::Frame(frame) => {
                                                    if frame.opus_data.is_empty() {
                                                        None
                                                    } else {
                                                        $st.decode(&frame.opus_data, frame.wideband)
                                                    }
                                                }
                                                // Rebuilt from the next packet's redundancy when it
                                                // carries some, as on Thetis; concealed otherwise
                                                // (builds 154 and 155).
                                                JitterResult::Missing => rebuild_or_conceal!($st, $jb),
                                                // Nothing arriving at all. Same rule as RX1:
                                                // conceal while the stream is switched on and
                                                // the link still counts as up.
                                                JitterResult::NotReady => {
                                                    dry = true;
                                                    if was_connected && $wanted { $st.conceal() } else { None }
                                                }
                                            };
                                            match decoded {
                                                Some(mut d) => {
                                                    if d.concealed {
                                                        measure.conceal(start.elapsed().as_millis() as u64, $stream);
                                                    }
                                                    // The recording gets the receive correction too,
                                                    // and not the volume: it then sounds as loud as
                                                    // what is heard, like RX1 and VRX (build 62).
                                                    if let Some(ref mut w) = $rec {
                                                        for s in d.pcm.iter_mut() {
                                                            *s = (*s as f32 * rx_gain).round().clamp(-32768.0, 32767.0) as i16;
                                                        }
                                                        let _ = w.write_samples(&d.pcm, d.rate);
                                                    }
                                                    let concealed = d.concealed;
                                                    let mut resampled = d.dev;
                                                    // Pre-volume energy: the bar shows what is received,
                                                    // not where the volume stands. Concealed frames stay
                                                    // out of it - see RX1.
                                                    if !concealed {
                                                        $accum += resampled.iter().map(|s| s * s).sum::<f32>();
                                                        $count += resampled.len();
                                                    }
                                                    // (calibrated to the Thetis RX path below)
                                                    apply_volume(&mut resampled, $volume * rx_gain * local_volume);
                                                    // Measured only after this radio's own release (build 179):
                                                    // every frame goes into the ring.
                                                    if !$ptt && !concealed && !resampled.is_empty() {
                                                        let frame_ms = resampled.len() as u64 * 1000 / playback_rate.max(1) as u64;
                                                        let ring_now = crate::measure::ring_ms(ring_level, playback_rate, stereo_output) + after_tx_added_ms;
                                                        let (_, outcome) = $after_tx.frame(crate::after_tx::peak(&resampled), ring_now, frame_ms, Instant::now());
                                                        match outcome {
                                                            // A developer's line (build 295): one per release.
                                                            Some(crate::after_tx::Outcome::FirstSound { after_ms, ring_ms, .. }) => dev_info!(
                                                                concat!($name, " RX after TX: first sound {} ms after the release, ring {} ms, jitter depth {}"),
                                                                after_ms, ring_ms, $jb.depth()
                                                            ),
                                                            Some(crate::after_tx::Outcome::NoSound { .. }) => dev_info!(
                                                                concat!($name, " RX after TX: no sound within {} ms, jitter depth {}"),
                                                                crate::after_tx::WINDOW.as_millis(), $jb.depth()
                                                            ),
                                                            None => {}
                                                        }
                                                    }
                                                    radio_buf.extend_from_slice(&resampled);
                                                }
                                                None => break,
                                            }
                                            // Nothing in this stream's buffer: when it sets the tick's
                                            // length, one concealed frame and no more, as on Thetis.
                                            if dry && opened.opens() {
                                                break;
                                            }
                                        }
                                        crate::after_tx::close_tick(opened, &mut playback_buf, radio_buf.len(), frame_samples);
                                        // Only real frames move the bar. A tick of concealed frames
                                        // leaves it where it was; it decays only when the buffer was
                                        // empty at the start of the tick (above). Radio 1 always did
                                        // this; radio 2 used to drop it to zero (build 180).
                                        if $count > 0 {
                                            state.$level = rx_gain * ($accum / $count as f32).sqrt();
                                        }
                                        // Mix into both L and R (additive, clamped)
                                        for (i, sample) in radio_buf.iter().enumerate() {
                                            if i < playback_buf.len() {
                                                playback_buf[i] = (playback_buf[i] + sample).clamp(-1.0, 1.0);
                                            }
                                            if i < bin_r_buf.len() {
                                                bin_r_buf[i] = (bin_r_buf[i] + sample).clamp(-1.0, 1.0);
                                            }
                                        }
                                    }
                                };
                            }
                            mix_yaesu_rx!(crate::loss_window::Stream::Radio1, "Radio 1", yaesu_logged_first, yaesu_jitter_buf, yaesu_wanted,
                                st_yaesu, rec_yaesu, yaesu_level_accum, yaesu_level_count, yaesu_volume,
                                yaesu_model, yaesu_ptt, after_tx_yaesu, playback_level_yaesu);
                            mix_yaesu_rx!(crate::loss_window::Stream::Radio2, "Radio 2", yaesu2_logged_first, yaesu2_jitter_buf, yaesu2_wanted,
                                st_yaesu2, rec_yaesu2, yaesu2_level_accum, yaesu2_level_count, yaesu2_volume,
                                yaesu2_model, yaesu2_ptt, after_tx_yaesu2, playback_level_yaesu2);

                            // Mix VRX1 audio (server-side FFT-channelizer
                            // on RX1 IQ + VFO-A). When the jitter buf
                            // runs dry (= server disabled VRX1 or audio
                            // packets stopped arriving), decay the level
                            // bar so the Server-tab doesn't show a stuck
                            // RMS value forever.
                            if !(vrx1_logged_first && vrx1_jitter_buf.depth() > 0) {
                                decay_level(&mut state.playback_level_vrx1);
                            }
                            if vrx1_logged_first
                                && (vrx1_jitter_buf.depth() > 0 || (was_connected && vrx1_wanted)) {
                                // One ring: as long as what was written before, or the tick's count
                                // when nothing was (build 156).
                                let opened = crate::after_tx::open_tick(&mut playback_buf, frames_wanted, frame_samples);
                                let target_samples = opened.target;
                                let mut vrx_buf: Vec<f32> = Vec::with_capacity(target_samples);
                                while vrx_buf.len() < target_samples {
                                    let mut dry = false;
                                    let decoded = match vrx1_jitter_buf.pull() {
                                        JitterResult::Frame(frame) => {
                                            if frame.opus_data.is_empty() {
                                                None
                                            } else {
                                                st_vrx1.decode(&frame.opus_data, frame.wideband)
                                            }
                                        }
                                        // Concealment used to run on the narrowband
                                        // decoder here and then hand the frame on marked
                                        // narrowband, so on a wideband VRX both the
                                        // decoder and the resampler after it were wrong.
                                        // Rebuilt from the next packet's redundancy when it
                                        // carries some, as on Thetis; concealed otherwise
                                        // (builds 154 and 155).
                                        JitterResult::Missing => rebuild_or_conceal!(st_vrx1, vrx1_jitter_buf),
                                        JitterResult::NotReady => {
                                            dry = true;
                                            if was_connected && vrx1_wanted { st_vrx1.conceal() } else { None }
                                        }
                                    };
                                    match decoded {
                                        Some(d) => {
                                            if d.concealed {
                                                measure.conceal(start.elapsed().as_millis() as u64, crate::loss_window::Stream::Vrx1);
                                            }
                                            if let Some(ref mut w) = rec_vrx1 {
                                                let _ = w.write_samples(&d.pcm, d.rate);
                                            }
                                            let concealed = d.concealed;
                                            let mut resampled = d.dev;
                                            if !concealed {
                                                vrx1_level_accum += resampled.iter().map(|s| s * s).sum::<f32>();
                                                vrx1_level_count += resampled.len();
                                            }
                                            apply_volume(&mut resampled, vrx1_volume * local_volume);
                                            vrx_buf.extend_from_slice(&resampled);
                                        }
                                        None => break,
                                    }
                                    // Nothing in this stream's buffer: when it sets the tick's
                                    // length, one concealed frame and no more, as on Thetis.
                                    if dry && opened.opens() {
                                        break;
                                    }
                                }
                                crate::after_tx::close_tick(opened, &mut playback_buf, vrx_buf.len(), frame_samples);
                                if vrx1_level_count > 0 {
                                    state.playback_level_vrx1 =
                                        (vrx1_level_accum / vrx1_level_count as f32).sqrt();
                                }
                                for (i, sample) in vrx_buf.iter().enumerate() {
                                    if i < playback_buf.len() {
                                        playback_buf[i] = (playback_buf[i] + sample).clamp(-1.0, 1.0);
                                    }
                                    if i < bin_r_buf.len() {
                                        bin_r_buf[i] = (bin_r_buf[i] + sample).clamp(-1.0, 1.0);
                                    }
                                }
                            }

                            // Mix VRX2 audio (server-side FFT-channelizer
                            // on RX2 IQ + VFO-B). Same pattern as VRX1.
                            if !(vrx2_logged_first && vrx2_jitter_buf.depth() > 0) {
                                decay_level(&mut state.playback_level_vrx2);
                            }
                            if vrx2_logged_first
                                && (vrx2_jitter_buf.depth() > 0 || (was_connected && vrx2_wanted)) {
                                // One ring: as long as what was written before, or the tick's count
                                // when nothing was (build 156).
                                let opened = crate::after_tx::open_tick(&mut playback_buf, frames_wanted, frame_samples);
                                let target_samples = opened.target;
                                let mut vrx_buf: Vec<f32> = Vec::with_capacity(target_samples);
                                while vrx_buf.len() < target_samples {
                                    let mut dry = false;
                                    let decoded = match vrx2_jitter_buf.pull() {
                                        JitterResult::Frame(frame) => {
                                            if frame.opus_data.is_empty() {
                                                None
                                            } else {
                                                st_vrx2.decode(&frame.opus_data, frame.wideband)
                                            }
                                        }
                                        // Concealment used to run on the narrowband
                                        // decoder here and then hand the frame on marked
                                        // narrowband, so on a wideband VRX both the
                                        // decoder and the resampler after it were wrong.
                                        // Rebuilt from the next packet's redundancy when it
                                        // carries some, as on Thetis; concealed otherwise
                                        // (builds 154 and 155).
                                        JitterResult::Missing => rebuild_or_conceal!(st_vrx2, vrx2_jitter_buf),
                                        JitterResult::NotReady => {
                                            dry = true;
                                            if was_connected && vrx2_wanted { st_vrx2.conceal() } else { None }
                                        }
                                    };
                                    match decoded {
                                        Some(d) => {
                                            if d.concealed {
                                                measure.conceal(start.elapsed().as_millis() as u64, crate::loss_window::Stream::Vrx2);
                                            }
                                            if let Some(ref mut w) = rec_vrx2 {
                                                let _ = w.write_samples(&d.pcm, d.rate);
                                            }
                                            let concealed = d.concealed;
                                            let mut resampled = d.dev;
                                            if !concealed {
                                                vrx2_level_accum += resampled.iter().map(|s| s * s).sum::<f32>();
                                                vrx2_level_count += resampled.len();
                                            }
                                            apply_volume(&mut resampled, vrx2_volume * local_volume);
                                            vrx_buf.extend_from_slice(&resampled);
                                        }
                                        None => break,
                                    }
                                    // Nothing in this stream's buffer: when it sets the tick's
                                    // length, one concealed frame and no more, as on Thetis.
                                    if dry && opened.opens() {
                                        break;
                                    }
                                }
                                crate::after_tx::close_tick(opened, &mut playback_buf, vrx_buf.len(), frame_samples);
                                if vrx2_level_count > 0 {
                                    state.playback_level_vrx2 =
                                        (vrx2_level_accum / vrx2_level_count as f32).sqrt();
                                }
                                for (i, sample) in vrx_buf.iter().enumerate() {
                                    if i < playback_buf.len() {
                                        playback_buf[i] = (playback_buf[i] + sample).clamp(-1.0, 1.0);
                                    }
                                    if i < bin_r_buf.len() {
                                        bin_r_buf[i] = (bin_r_buf[i] + sample).clamp(-1.0, 1.0);
                                    }
                                }
                            }

                            // BinR level: pure RX1-R only (measured before RX2 mix)
                            if bin_r_level_count > 0 {
                                state.playback_level_bin_r = (bin_r_level_accum / bin_r_level_count as f32).sqrt();
                            } else {
                                decay_level(&mut state.playback_level_bin_r);
                            }

                            // WAV speaker playback (when not TX)
                            if !playback_is_tx && playback_wav.is_some() {
                                // Two frames while the ring is low, one when it
                                // is not - the same catch-up the receive path
                                // above gives itself, and for the same reason.
                                //
                                // This used to write exactly one twenty
                                // millisecond frame per twenty millisecond
                                // tick, always. That is the right average and
                                // no cushion at all: the ring hovers at nothing,
                                // every late tick or long device callback
                                // empties it, and there is no mechanism to
                                // catch back up because one frame per tick is
                                // also the ceiling. Playback through the client
                                // broke up while the same file played cleanly
                                // in any ordinary player (2026-08-14), which is
                                // exactly the shape of a ring with no slack.
                                let frames_now = if ring_level < target_ring_low { 2 } else { 1 };
                                let samples_per_tick = if playback_wav_rate == NETWORK_SAMPLE_RATE_WIDEBAND {
                                    FRAME_SAMPLES_WIDEBAND
                                } else {
                                    FRAME_SAMPLES
                                };
                                playback_buf.clear();
                                bin_r_buf.clear();
                                let mut finished = false;
                                // Exactly two ticked go hard left and hard right,
                                // which makes them trivial to tell apart. One
                                // plays to both ears as it always did, and more
                                // than two are mixed - there are only two ears.
                                let split = playback_extra.len() == 1;
                                for _ in 0..frames_now {
                                    let wav = match playback_wav.as_ref() {
                                        Some(w) => w,
                                        None => break,
                                    };
                                    let remaining = wav.len().saturating_sub(playback_pos);
                                    if remaining == 0 {
                                        finished = true;
                                        break;
                                    }
                                    let to_read = samples_per_tick.min(remaining);
                                    let mut pcm: Vec<i16> = wav[playback_pos..playback_pos + to_read].to_vec();
                                    if pcm.len() < samples_per_tick {
                                        pcm.resize(samples_per_tick, 0);
                                    }
                                    let resampled = if playback_wav_rate == NETWORK_SAMPLE_RATE_WIDEBAND {
                                        resample_to_device(&mut wav_res_out_wb, &pcm)
                                    } else {
                                        resample_to_device(&mut wav_res_out, &pcm)
                                    };
                                    // Each extra stream runs out on its own
                                    // schedule: recordings of the same test are
                                    // rarely the same length, and the short ones
                                    // should fall silent rather than cut the rest
                                    // short.
                                    let extras: Vec<Vec<f32>> = playback_extra
                                        .iter_mut()
                                        .map(|e| {
                                            let per_tick = if e.rate == NETWORK_SAMPLE_RATE_WIDEBAND {
                                                FRAME_SAMPLES_WIDEBAND
                                            } else {
                                                FRAME_SAMPLES
                                            };
                                            let rem = e.samples.len().saturating_sub(e.pos);
                                            if rem == 0 {
                                                return Vec::new();
                                            }
                                            let take = per_tick.min(rem);
                                            let mut pr: Vec<i16> = e.samples[e.pos..e.pos + take].to_vec();
                                            if pr.len() < per_tick {
                                                pr.resize(per_tick, 0);
                                            }
                                            e.pos += take;
                                            if e.rate == NETWORK_SAMPLE_RATE_WIDEBAND {
                                                resample_to_device(&mut e.res_wb, &pr)
                                            } else {
                                                resample_to_device(&mut e.res_nb, &pr)
                                            }
                                        })
                                        .collect();
                                    for (i, &s) in resampled.iter().enumerate() {
                                        // play_volume slider on speaker playback too.
                                        let scale = local_volume * play_volume;
                                        let left = (s * scale).clamp(-1.0, 1.0);
                                        if split {
                                            let r = extras[0].get(i).copied().unwrap_or(0.0);
                                            playback_buf.push(left);
                                            bin_r_buf.push((r * scale).clamp(-1.0, 1.0));
                                        } else {
                                            let mixed: f32 = s + extras.iter()
                                                .map(|e| e.get(i).copied().unwrap_or(0.0))
                                                .sum::<f32>();
                                            let both = (mixed * scale).clamp(-1.0, 1.0);
                                            playback_buf.push(both);
                                            bin_r_buf.push(both);
                                        }
                                    }
                                    playback_pos += to_read;
                                    if playback_pos >= wav.len() {
                                        finished = true;
                                        break;
                                    }
                                }
                                if finished {
                                    info!("WAV speaker playback finished");
                                    playback_wav = None;
                                    playback_pos = 0;
                                    playback_extra.clear();
                                    playback_is_tx = false;
                                    state.playing = false;
                                }
                            }

                            // Write audio to playback - stereo if binaural R available. Not on
                            // a tick the ring gives back (build 158).
                            if !playback_buf.is_empty() && !drain_this_tick {
                                // Always write stereo - if R is empty, duplicate L
                                if bin_r_buf.is_empty() {
                                    bin_r_buf = playback_buf.clone();
                                }
                                let len = playback_buf.len().max(bin_r_buf.len());
                                playback_buf.resize(len, 0.0);
                                bin_r_buf.resize(len, 0.0);
                                audio.write_playback_stereo(&playback_buf, &bin_r_buf);
                            }
                        } // if !skip_this_tick

                        // (RX2 mixing is now done server-side)
                    }

                    // Update buffer stats after pull loop so UI shows actual current depth
                    state.buffer_depth = jitter_buf.depth() as u32;
                    state.jitter_ms = jitter_buf.jitter_ms();
                    // Clear yaesu_memory_data after 500ms to avoid cloning 2KB+ every frame
                    if let Some(clear_at) = yaesu_mem_data_clear_at {
                        if Instant::now() >= clear_at {
                            state.yaesu_memory_data = None;
                            yaesu_mem_data_clear_at = None;
                        }
                    }
                    if let Some(clear_at) = yaesu2_mem_data_clear_at {
                        if Instant::now() >= clear_at {
                            state.yaesu2_memory_data = None;
                            yaesu2_mem_data_clear_at = None;
                        }
                    }
                    if let Some(clear_at) = yaesu_menu_data_clear_at {
                        if Instant::now() >= clear_at {
                            state.yaesu_menu_data = None;
                            yaesu_menu_data_clear_at = None;
                        }
                    }
                    // A server-report transfer that stopped halfway says so.
                    // Silence would leave the tickbox waiting for ever and, worse,
                    // invite a second attempt that quietly reuses whatever
                    // arrived the first time.
                    if let Some(t) = server_report_deadline {
                        if Instant::now() >= t {
                            let parts = server_report_parts.len() as u16;
                            let have =
                                server_report_parts.iter().filter(|p| p.is_some()).count() as u16;
                            warn!("Server report incomplete: {} of {} parts", have, parts);
                            state.server_report_failed = Some((have, parts));
                            state.server_report = None;
                            server_report_parts = Vec::new();
                            server_report_deadline = None;
                        }
                    }
                    if let Some(clear_at) = yaesu2_menu_data_clear_at {
                        if Instant::now() >= clear_at {
                            state.yaesu2_menu_data = None;
                            yaesu2_menu_data_clear_at = None;
                        }
                    }

                    // playback_level is measured per-channel before mixing (see above)

                    // Connection timeout detection, decided in `link_lost`:
                    // listening, both heartbeat ACK and audio absent past
                    // max(6s, rtt*8); transmitting, the ACK alone past 3 s
                    // (build 265).
                    if was_connected {
                        let transmitting = thetis_ptt || yaesu_ptt || yaesu2_ptt;
                        let since_ack = last_hb_ack_time.map(|t| t.elapsed());
                        let since_audio = last_audio_received.map(|t| t.elapsed());
                        if let Some(timeout_ms) =
                            link_lost(transmitting, last_hb_ack_rtt, since_ack, since_audio)
                        {
                            if transmitting {
                                info!(
                                    "Connection lost while transmitting (no answer to a heartbeat for {} ms, window {} ms)",
                                    since_ack.map_or(0, |d| d.as_millis()),
                                    timeout_ms
                                );
                                // The flags go below (`session_lost!`), and later in
                                // this tick each radio's "off" burst goes out, as
                                // Thetis's does: when only the line from the server
                                // is dead, it arrives and lets the radio go (build
                                // 282; until then a control, and again when the
                                // server answered - build 267).
                            } else {
                                info!("Connection lost (no traffic for {}ms, ring={}, jbuf={}, jitter={:.1}ms)",
                                    timeout_ms, audio.playback_buffer_level(), jitter_buf.depth(), jitter_buf.jitter_ms());
                            }
                            // Don't reset jitter buffer - let it drain via PLC
                            // so audio resumes smoothly if packets return
                            was_connected = false;
                            last_hb_ack_rtt = 0;
                            logged_first_rx = false;
                            logged_first_tx = false;
                            rx_volume_synced = false;
                            rx2_volume_synced = false;
                            state.rx_af_gain = 0;
                            // The link is gone, so nothing is outstanding any more. Three of
                            // the four link-down paths did not do this, and each one left the
                            // "PTT blocked" sign standing over a connection that no longer
                            // exists.
                            let forgotten = session_lost!();
                            state.link_down(forgotten);
                            state.rtt_ms = 0;
                            // Clear stale spectrum data
                            state.spectrum_bins.clear();
                            state.full_spectrum_bins.clear();
                            state.spectrum_sequence = 0;
                            state.full_spectrum_sequence = 0;
                        }
                    }

                    // Audio device error detection and recovery
                    if audio.has_error() {
                        state.audio_error = true;
                        if audio_error_since.is_none() {
                            warn!("Audio device error detected, will attempt reconnect");
                            audio_error_since = Some(Instant::now());
                        }
                        let since = audio_error_since.unwrap();
                        if since.elapsed().as_millis() >= audio_retry_interval_ms as u128 {
                            info!("Attempting audio reconnect...");
                            let in_name = if input_device_name.is_empty() { None } else { Some(input_device_name.as_str()) };
                            let out_name = if output_device_name.is_empty() { None } else { Some(output_device_name.as_str()) };
                            match audio_factory(in_name, out_name) {
                                Ok(new_audio) => {
                                    take_new_audio!(new_audio);
                                    info!("Audio reconnected successfully");
                                    state.audio_error = false;
                                    audio_error_since = None;
                                    audio_retry_interval_ms = 1000;
                                    accum_buf.clear();
                                }
                                Err(e) => {
                                    warn!("Audio reconnect failed: {}", e);
                                    audio_error_since = Some(Instant::now());
                                    audio_retry_interval_ms = (audio_retry_interval_ms * 2).min(10_000);
                                }
                            }
                        }
                    }

                    // The chosen microphone was missing: look for it now and then,
                    // and open it once it is there - nothing else is opened in its
                    // place (build 188). Only the looking costs, every few seconds.
                    if state.mic_missing.is_some() && mic_looked_at.elapsed() >= mic_try_every {
                        mic_looked_at = Instant::now();
                        // The audio watches its own missing microphone (one load
                        // here); audio that never tried the chosen one - it failed
                        // to open - cannot, so that is tried again (builds 189, 190).
                        let back = audio.missing_input_is_back();
                        if back || audio.missing_input().is_none() {
                            if back {
                                info!("Microphone '{}' is back: opening it", state.mic_missing.as_deref().unwrap_or(""));
                            }
                            let in_name = if input_device_name.is_empty() { None } else { Some(input_device_name.as_str()) };
                            let out_name = if output_device_name.is_empty() { None } else { Some(output_device_name.as_str()) };
                            match audio_factory(in_name, out_name) {
                                Ok(new_audio) => {
                                    take_new_audio!(new_audio);
                                    accum_buf.clear();
                                }
                                Err(e) => {
                                    mic_try_every = next_mic_try(mic_try_every);
                                    warn!(
                                        "Microphone '{}' could not be opened: trying again in {} s ({})",
                                        state.mic_missing.as_deref().unwrap_or(""),
                                        mic_try_every.as_secs(),
                                        e
                                    );
                                }
                            }
                        }
                    }

                    // Measurement mode: the writer gave up (full disk, storage gone).
                    // Said in the log by the writer; here the screen stops saying it
                    // measures. One atomic load per tick.
                    if let Some(p) = measure.take_failure() {
                        warn!("Measurement off after a write error: {}", p.display());
                        state.measurement_file = None;
                    }
                    // Once a second: what stands between arrival and the speaker for
                    // the main stream - the jitter buffer and the playout ring.
                    // A no-op when off.
                    // For a second after each Thetis PTT edge, at every tick: the ring
                    // is not refilled during TX and empties, then refills - the
                    // resting line a second never sees that.
                    let now_ms = start.elapsed().as_millis() as u64;
                    // Any radio's PTT edge opens the burst (build 178).
                    let any_ptt = ptt || yaesu_ptt || yaesu2_ptt;
                    if any_ptt != measure_ptt_was {
                        measure_ptt_was = any_ptt;
                        measure_buffers_burst_until = now_ms + crate::measure::BUFFER_BURST_MS;
                    }
                    measure.buffers(
                        now_ms,
                        &mut measure_buffers_at,
                        now_ms < measure_buffers_burst_until,
                        crate::loss_window::Stream::Thetis,
                        jitter_buf.depth(),
                        jitter_buf.target_depth(),
                        crate::measure::ring_ms(audio.playback_buffer_level(), playback_rate, stereo_output),
                        jitter_buf.jitter_ms(),
                    );
                    // And the two radios' buffers, each on its own clock, so a
                    // Yaesu can be judged as Thetis is (build 178).
                    measure.buffers(
                        now_ms,
                        &mut measure_buffers_at_yaesu,
                        now_ms < measure_buffers_burst_until,
                        crate::loss_window::Stream::Radio1,
                        yaesu_jitter_buf.depth(),
                        yaesu_jitter_buf.target_depth(),
                        crate::measure::ring_ms(audio.playback_buffer_level(), playback_rate, stereo_output),
                        yaesu_jitter_buf.jitter_ms(),
                    );
                    measure.buffers(
                        now_ms,
                        &mut measure_buffers_at_yaesu2,
                        now_ms < measure_buffers_burst_until,
                        crate::loss_window::Stream::Radio2,
                        yaesu2_jitter_buf.depth(),
                        yaesu2_jitter_buf.target_depth(),
                        crate::measure::ring_ms(audio.playback_buffer_level(), playback_rate, stereo_output),
                        yaesu2_jitter_buf.jitter_ms(),
                    );
                    // What the playout rule decided since the last line (build 157).
                    measure.ring(now_ms, &mut measure_ring_at, now_ms < measure_buffers_burst_until, &mut ring_ticks);

                    // When not connected, drain capture buffer and clear accumulator
                    if server_addr.is_none() {
                        audio.read_capture(&mut drain_buf);
                        accum_buf.clear();
                        let _ = self.state_tx.send(state.clone());
                        continue;
                    }
                    let addr = server_addr.as_ref().unwrap();

                    // Send again what the server has not acknowledged, and
                    // say what was given up. One line per 10 s, only when something
                    // was repeated or given up.
                    let due = controls_out.due(Instant::now());
                    for bytes in &due.resend {
                        let _ = send_tx!(bytes, addr.as_str());
                        if let Some((_, seq)) = sdr_remote_core::protocol::numbered_control(bytes) {
                            measure.record(crate::measure::Record::Repeat { mono_ms: start.elapsed().as_millis() as u64, seq });
                        }
                    }
                    for key in &due.given_up {
                        measure.record(crate::measure::Record::GiveUp { mono_ms: start.elapsed().as_millis() as u64, key: *key });
                        warn!("Control {:?} not acknowledged after {} repeats; given up", key, sdr_remote_core::control_ack::MAX_REPEATS);
                    }
                    if controls_report_at.elapsed() >= Duration::from_secs(10) {
                        controls_report_at = Instant::now();
                        let (sent, repeated, given_up) = controls_out.take_counts();
                        if repeated > 0 || given_up > 0 {
                            info!("controls: sent {}, repeated {}, given up {} (10 s)", sent, repeated, given_up);
                        }
                    }

                    let af_gain = (rx_volume * 100.0).round() as u16;

                    // Send RX1 AF gain control when changed (only after initial sync from server)
                    if rx_volume_synced && af_gain != last_sent_volume {
                        let _ = send_control!(ControlId::Rx1AfGain, af_gain, addr.as_str());
                        last_sent_volume = af_gain;
                    }

                    // Send RX2 AF gain control when changed (only after initial sync from server)
                    // Only send when the USER changed the slider (SetRx2Volume command),
                    // not when the server broadcast updated rx2_volume.
                    let rx2_af_gain = (rx2_volume * 100.0).round() as u16;
                    if rx2_volume_synced && rx2_volume_user_changed && rx2_af_gain != last_sent_rx2_volume {
                        info!("Sending RX2 AF gain to server: {}% (was {}%)", rx2_af_gain, last_sent_rx2_volume);
                        let _ = send_control!(ControlId::Rx2AfGain, rx2_af_gain, addr.as_str());
                        last_sent_rx2_volume = rx2_af_gain;
                        rx2_volume_user_changed = false;
                    }

                    // Heartbeat (skip while waiting for TOTP input)
                    // The code went out and nothing came back: the server has
                    // forgotten this attempt (a client at the code step sends no
                    // heartbeats, so its session is reaped - after two minutes
                    // since build 71; or its address changed) and answers a late
                    // code with nothing since build 68. Without this the client
                    // waited at the code field for ever. Letting go
                    // of the code step resumes the heartbeats; the server
                    // challenges the next one, and the client walks the
                    // authentication again up to a fresh code field - which is
                    // what the screen shows, and what a "connect again" told the
                    // operator to do while the engine did it anyway.
                    if state.totp_required {
                        if let Some(sent) = totp_code_sent_at {
                            if sent.elapsed() > Duration::from_secs(TOTP_ANSWER_TIMEOUT_SECS) {
                                warn!(
                                    "No answer to the TOTP code within {} s: the server has forgotten this attempt; a new one starts and the code is asked again",
                                    TOTP_ANSWER_TIMEOUT_SECS
                                );
                                totp_code_sent_at = None;
                                // `pending_auth` stays: the answer may still come, late
                                // instead of never (a slow relay, a server that blocked
                                // for a moment). Its nonces are what make that late
                                // ACCEPTED - which carries no proof of its own - the
                                // verified one it is. Without them it read as a server
                                // that proves nothing on a first session, and as a
                                // duplicate on a second, where it then sat at Connecting
                                // for good (both measured). A new challenge
                                // replaces them anyway. This is the opposite of what
                                // `session_lost!()` does with them, for the opposite
                                // reason: there the session is gone, here only the wait.
                                state.totp_required = false;
                                state.auth_rejected = false;
                                state.connect_status = crate::state::ConnectStatus::Connecting;
                                // A new attempt for the no-reply watchdog too: a server
                                // from before build 72 holds the old code step and does
                                // not challenge the knock, and without this the client sat
                                // at Connecting for ever - the watchdog wants a reply-less
                                // attempt (measured at 21 s).
                                connect_started_at = Some(Instant::now());
                                connect_any_reply_seen = false;
                                resumed_after_code_step = true;
                            }
                        }
                    }
                    if !state.totp_required && last_hb_sent.elapsed().as_millis() > HEARTBEAT_INTERVAL_MS as u128 {
                        // One window per stream, closed together. The client
                        // reports the worst of the streams it is EXPECTING: a
                        // stream it asked us to stop has nothing to say, and a
                        // stream that is running is measured even when the
                        // others are not. Before this, the figure came from the
                        // Thetis stream alone while the server used it to
                        // protect the radio audio as well.
                        // Derived in one place, never spelled out here: a struct
                        // literal at this call site can nail a stream to false
                        // with every test in the module still green, because they
                        // all build their own.
                        state.loss_percent = loss_meter.step(
                            crate::loss_window::Expecting::from_client(
                                audio_mode as u8,
                                crate::thetis_streams::stream_value(
                                    state.rx1_enabled, thetis_streams_stopped),
                                state.rx2_enabled,
                                yaesu_wanted,
                                yaesu2_wanted,
                                vrx1_wanted,
                                vrx2_wanted,
                            ),
                        );
                        // No test watches this threshold, so the proof that
                        // it works is this line in a field log. One line when the
                        // stream that loses the MOST changes - not one per window,
                        // which is how the conceal report once filled three
                        // quarters of the log. Note that a client with a single
                        // stream therefore gets this line once and never again:
                        // there is nothing for it to change to.
                        // The figure stands when no stream has
                        // anything to say, which is right - silence is not proof
                        // of a clean link - but until now nothing said that it
                        // had stopped being current. Eight places in the server
                        // read it and four of them decide whether a spectrum is
                        // still sent, so a number that quietly stops moving is
                        // worth a line.
                        //
                        // On the EDGE, both ways, and never per window: the relay
                        // diagnostic that cried wolf and the per-gap conceal line
                        // were both one-per-event where one-per-change carried
                        // the same meaning.
                        let stale = loss_meter.stale_windows();
                        if stale == crate::loss_window::STALE_AFTER_WINDOWS {
                            let seconds =
                                stale as u64 * HEARTBEAT_INTERVAL_MS / 1000;
                            if loss_meter.has_measured() {
                                info!(
                                    "loss meter: no stream to measure for {} s; the {}% it reports is the last real measurement, not a statement about now",
                                    seconds, state.loss_percent
                                );
                            } else {
                                // Zero because it started there, not because the
                                // link is clean. Calling that "the last real
                                // measurement" is the same mistake as reading
                                // silence as a clean link, one sentence further
                                // along.
                                info!(
                                    "loss meter: nothing has been measured in {} s since this session began; the {}% it reports is a starting value, not a finding",
                                    seconds, state.loss_percent
                                );
                            }
                        }
                        // Every silence writes down its own length, not only the
                        // ones that cross the threshold. That is how the question
                        // "is forty windows the right number" gets answered from
                        // one evening of logs instead of a week of them - and it
                        // is why this compares against the PREVIOUS window rather
                        // than carrying a flag of its own, which could drift from
                        // the meter across a session boundary.
                        if stale == 0 && last_stale_windows > 0 {
                            info!(
                                "loss meter: measuring again ({}%) after {} window(s) with nothing to measure",
                                state.loss_percent, last_stale_windows
                            );
                        }
                        last_stale_windows = stale;
                        if loss_meter.worst() != last_worst_stream {
                            last_worst_stream = loss_meter.worst();
                            if let Some(stream) = last_worst_stream {
                                // The figure is the LINK, pooled over every stream
                                // that is running; the stream named is only where
                                // most of the missing packets were. Saying it
                                // "decides" was left over from when the worst
                                // stream set the figure on its own.
                                info!("loss meter: link at {}%, most of it missing on {}",
                                    state.loss_percent, stream.name());
                            }
                        }
                        // How rough the last stretch was, in one line, and only when
                        // there was something to say. The per-gap lines this replaces
                        // filled three quarters of the kept log and left it reaching
                        // back under an hour - the same shape as the relay diagnostic
                        // that cried wolf: a message per event where one per window
                        // carries the same meaning.
                        if last_conceal_report.elapsed() >= Duration::from_secs(10) {
                            if conceal_gaps > 0 {
                                info!(
                                    "concealing (10 s): {} gap(s), {} frame(s), longest {:.1}s",
                                    conceal_gaps, conceal_total_frames,
                                    conceal_longest.as_secs_f32()
                                );
                            }
                            conceal_gaps = 0;
                            conceal_total_frames = 0;
                            conceal_longest = Duration::ZERO;
                            last_conceal_report = Instant::now();
                        }

                        // Bandwidth-window flush: in sync with the heartbeat
                        // tick (~500 ms). bytes × 8 / window_ms = bits/ms = kbps.
                        let win_ms = bw_window_start.elapsed().as_millis().max(1) as u64;
                        state.down_kbps = (bw_rx_bytes.saturating_mul(8) / win_ms) as u32;
                        state.up_kbps = (bw_tx_bytes.saturating_mul(8) / win_ms) as u32;
                        bw_rx_bytes = 0;
                        bw_tx_bytes = 0;
                        bw_window_start = Instant::now();

                        // Per-PacketType breakdown every 5 s: published to
                        // `state.bw_breakdown` so the Server tab in the UI
                        // can show a drill-down detail without log spam.
                        if bw_breakdown_start.elapsed() >= Duration::from_secs(5) {
                            let win_s = bw_breakdown_start.elapsed().as_secs_f64().max(0.001);
                            let mut by_type: Vec<(u8, u32)> = bw_by_type.iter().enumerate()
                                .filter(|(_, &b)| b > 0)
                                .map(|(t, &b)| {
                                    let kbps = ((b as f64 * 8.0) / (win_s * 1000.0)) as u32;
                                    (t as u8, kbps)
                                })
                                .filter(|(_, kbps)| *kbps > 0)
                                .collect();
                            by_type.sort_by(|a, b| b.1.cmp(&a.1));
                            state.bw_breakdown = by_type;
                            bw_by_type = [0; 256];
                            bw_breakdown_start = Instant::now();
                        }

                        let hb = Heartbeat {
                            flags: Flags::NONE.with_ptt(thetis_ptt),
                            sequence: hb_sequence,
                            local_time: start.elapsed().as_millis() as u32,
                            rtt_ms: last_hb_ack_rtt,
                            loss_percent: loss_meter.reported(),
                            jitter_ms: jitter_buf.jitter_ms().min(255.0) as u8,
                            capabilities: Capabilities::NONE,
                            // What this client calls itself, so the server can
                            // say out loud when the two are not the same build.
                            version: Some(
                                sdr_remote_core::protocol::VersionTag::mine(),
                            ),
                        };
                        hb_sequence = hb_sequence.wrapping_add(1);

                        let mut buf = [0u8; Heartbeat::SIZE_WITH_VERSION];
                        hb.serialize_with_version(&mut buf);
                        let _ = send_tx!(&buf, addr.as_str());
                        last_hb_sent = Instant::now();

                        // NoUdpResponse
                        // watchdog. If we've been "Connecting" for longer than the
                        // timeout and have never seen any reply from the server,
                        // surface a precise error instead of leaving the UI in an
                        // indefinite "Connecting…" state.
                        if matches!(
                            state.connect_status,
                            crate::state::ConnectStatus::Connecting
                        ) && !connect_any_reply_seen
                        {
                            if let Some(started) = connect_started_at {
                                if started.elapsed()
                                    >= std::time::Duration::from_secs(
                                        connect_timeout_secs as u64,
                                    )
                                {
                                    if resumed_after_code_step {
                                        warn!(
                                            "No challenge after the code step was let go: a server from before v2.13 (build 72) holds the old attempt for its deadline, or this address is blocked for a minute after five refused codes (the server log says which); update the server, or connect again later"
                                        );
                                        resumed_after_code_step = false;
                                    }
                                    state.connect_status =
                                        crate::state::ConnectStatus::Failed(
                                            crate::state::ConnectError::NoUdpResponse {
                                                addr: addr.clone(),
                                                timeout_secs: connect_timeout_secs,
                                            },
                                        );
                                }
                            }
                        }
                    }

                    // A change of a transmitter's flag starts its burst, seen here
                    // for all three (`tx_stream`; the radios since build 292, as
                    // Thetis's): every way a flag changes - the button, the roger
                    // beep, a refusal, a lost link - is seen at this tick.
                    if thetis_tx.ptt(ptt) {
                        info!("PTT: {}", if ptt { "TX" } else { "RX" });
                    }
                    yaesu_tx[0].ptt(yaesu_ptt);
                    yaesu_tx[1].ptt(yaesu2_ptt);
                    state.smeter_tx = meter_window.step(ptt, state.server_tx, start.elapsed().as_millis() as u64);
                    state.tx_window_open = meter_window.open();
                    state.auto_ref_paused = auto_ref_hold.step(state.tx_window_open || state.other_tx, start.elapsed().as_millis() as u64);

                    let capture_ptt = ptt || yaesu_ptt || yaesu2_ptt;
                    if capture_ptt != last_capture_ptt {
                        if capture_ptt {
                            audio.set_capture_gate(false);
                            accum_buf.clear();
                            yaesu_tx_accum.clear();
                            let _ = audio.read_capture(&mut read_buf);
                            audio.set_capture_gate(true);
                            if mic_gate_delay_ms > 0 {
                            }
                        } else {
                            audio.set_capture_gate(false);
                            accum_buf.clear();
                            yaesu_tx_accum.clear();
                            // The over ends: its level window with it.
                            for (slot, radio) in [(0, "Yaesu"), (1, "Yaesu2")] {
                                if let Some(line) = yaesu_level_log[slot].close(radio) {
                                    dev_info!("{line}");
                                }
                            }
                            // And the limiters start the next over at unity, not on
                            // the tail of a peak just before the release.
                            thetis_limiter = TxLimiter::new();
                            yaesu_limiters = [TxLimiter::new(), TxLimiter::new()];
                        }
                        last_capture_ptt = capture_ptt;
                    }

                    // TX meters read what actually leaves this client: the level is
                    // taken at the very end of each TX chain, after AGC/compressor/EQ
                    // and the gain trim, right before Opus.
                    //
                    // Only cleared when nothing is being transmitted. A tick is
                    // shorter than the 20 ms frame the chains encode, so clearing
                    // every tick left the bar alternating between a real level and
                    // zero - which reads as audio dropping out. Between frames the
                    // last measured level therefore stands.
                    if !ptt && !yaesu_ptt && !yaesu2_ptt {
                        state.capture_level = 0.0;
                        state.yaesu_mic_level = 0.0;
                    }
                    state.mic_input_clip = input_clip_at.is_some_and(|t| t.elapsed() < INPUT_CLIP_HOLD);

                    // Thetis-TXEQ-bypass during WAV-playback to the MAIN RADIO (Thetis-PTT):
                    // turn off the mic-profile TX-EQ during Play and restore on stop/PTT-release/
                    // end - just like Thetis' own record/playback. Only Thetis (ptt); the
                    // Yaesu EQ is already skipped client-side. Edge-triggered via the flag.
                    let thetis_wav_tx_active = playback_is_tx && ptt && playback_wav.is_some();
                    if thetis_wav_tx_active != thetis_txeq_bypassed {
                        let _ = send_control!(
                            ControlId::ThetisTxeq,
                            if thetis_wav_tx_active { 0 } else { 1 },
                            addr.as_str()
                        );
                        thetis_txeq_bypassed = thetis_wav_tx_active;
                        info!(
                            "Thetis TXEQ {} (WAV playback)",
                            if thetis_wav_tx_active { "off" } else { "restored" }
                        );
                    }

                    // Whether Thetis's stream sent audio this tick - the WAV's or the
                    // microphone's - so the burst sends no packet with only the flag.
                    let mut sent_any = false;
                    // WAV TX playback: bypass mic capture when playing back a TX recording
                    if playback_is_tx && (ptt || yaesu_ptt || yaesu2_ptt) && playback_wav.is_some() {
                        let wav = playback_wav.as_ref().unwrap();
                        // Number of WAV samples per 20 ms at the HEADER rate (8k->160,
                        // 16k->320). Previously this was a fixed FRAME_SAMPLES (8k) plus
                        // a blind sample duplication to "16k"; a 16 kHz recording
                        // therefore played back at half speed, and the Yaesu branch put
                        // raw 8/16k samples into a capture-rate (48k) accumulator
                        // -> 3-6x too slow + stuttering. Now rate-aware.
                        let samples_per_tick = (playback_wav_rate as usize * 20) / 1000;
                        let remaining = wav.len() - playback_pos;
                        let to_read = samples_per_tick.min(remaining);
                        if to_read > 0 {
                            // play_volume slider: scales the recorded WAV, so both TX
                            // branches (Thetis + Yaesu) follow the adjusted level. The
                            // meters are taken at the end of those chains, not here.
                            let src_f32: Vec<f32> = wav[playback_pos..playback_pos + to_read]
                                .iter()
                                .map(|&s| (s as f32 / 32768.0) * play_volume)
                                .collect();

                            // Thetis main-radio TX: resample header-rate -> 16 kHz for
                            // the wideband Opus encoder. Only if Thetis-PTT is active.
                            if ptt {
                                if let Some(w) = crate::wav_tx::for_radio(
                                    &mut wav_to_thetis, playback_wav_rate, NETWORK_SAMPLE_RATE_WIDEBAND, "Thetis",
                                ) {
                                    wav_thetis_accum.extend_from_slice(&w.feed(&src_f32));
                                }
                                while wav_thetis_accum.len() >= FRAME_SAMPLES_WIDEBAND {
                                    let mut f16: Vec<f32> = wav_thetis_accum.drain(..FRAME_SAMPLES_WIDEBAND).collect();
                                    // The TX gain as before, then the limiter. Meter = the frame as encoded.
                                    apply_gain(&mut f16, tx_gain);
                                    thetis_limiter.process(&mut f16);
                                    let (level, pcm_i16) = to_encoder(&f16[..FRAME_SAMPLES_WIDEBAND], 1.0);
                                    state.capture_level = level;
                                    match encoder.encode(&pcm_i16) {
                                        Ok(opus_data) => {
                                            let out = thetis_tx.audio(thetis_ptt, opus_data, start.elapsed().as_millis() as u32);
                                            send_out!(out, thetis_tx.stream(), addr.as_str());
                                            sent_any = true;
                                        }
                                        Err(e) => warn!("WAV TX encode error: {}", e),
                                    }
                                }
                            }

                            // Yaesu TX: feed into yaesu_tx_accum at capture_rate. NO pre-
                            // attenuation: for WAV-playback the drain skips the mic chain
                            // (compressor/AGC + the 4x mic-boost). That 4x + comp/AGC are
                            // meant for a QUIET, dynamic mic; a recorded WAV is
                            // already at line level. The AGC would normalize the WAV back up
                            // after which the 4x would still clip (distortion). Now the WAV goes clean as
                            // line level through the chain: only play_volume + mic_gain.
                            if yaesu_ptt || yaesu2_ptt {
                                if let Some(w) = crate::wav_tx::for_radio(
                                    &mut wav_to_yaesu, playback_wav_rate, capture_rate, "the Yaesu",
                                ) {
                                    yaesu_tx_accum.extend_from_slice(&w.feed(&src_f32));
                                }
                            }

                            playback_pos += to_read;
                        }
                        if playback_pos >= wav.len() {
                            info!("WAV TX playback finished");
                            playback_wav = None;
                            playback_pos = 0;
                            playback_is_tx = false;
                            state.playing = false;
                        }
                        // Drain mic capture to prevent buffer buildup
                        let _ = audio.read_capture(&mut read_buf);
                    } else {
                        // Normal mic capture path
                        // Read all available samples into accumulation buffer
                        let read = audio.read_capture(&mut read_buf);
                        if read > 0 {
                            accum_buf.extend_from_slice(&read_buf[..read]);
                            // Copy mic data for Yaesu TX (separate path)
                            if yaesu_ptt || yaesu2_ptt {
                                yaesu_tx_accum.extend_from_slice(&read_buf[..read]);
                            }
                        }
                        // A roger beep is written into a frame, and the frames
                        // come from the microphone - so a quiet, muted, gated
                        // or absent microphone meant no frame and therefore no
                        // beep at all, which is what happened the moment the
                        // tone moved into this loop (2026-08-14). Keep the
                        // accumulator topped up while a tone is running. The
                        // tone overwrites the frame anyway, so silence is the
                        // right filler, and real capture still sets the pace
                        // whenever there is any.
                        if let Some((_, ch, _)) = roger_tone {
                            let have = if ch == 0 { accum_buf.len() } else { yaesu_tx_accum.len() };
                            let want = capture_frame_samples.saturating_sub(have);
                            if want > 0 {
                                if ch == 0 {
                                    accum_buf.resize(have + want, 0.0);
                                } else {
                                    yaesu_tx_accum.resize(have + want, 0.0);
                                }
                            }
                        }
                    }

                    // Process complete frames from accumulation buffer
                    while accum_buf.len() >= capture_frame_samples {
                        let mut chunk: Vec<f32> = accum_buf.drain(..capture_frame_samples).collect();
                        if input_clips(peak_scaled(&chunk, 1.0)) {
                            input_clip_at = Some(Instant::now());
                        }
                        // The TX gain at the front, so the AGC works on the set level.
                        apply_gain(&mut chunk, tx_gain);
                        let mut pcm_8k = resample_to_network(&mut resampler_in, &chunk);

                        // The AGC is fed by every captured frame (there is capture
                        // during any over, also a Yaesu one), but not while the gain
                        // is off: on silence its gain climbs to the maximum, and the
                        // first frame after opening the slider would come out loud.
                        if agc_enabled && tx_gain > 0.0 {
                            agc.process(&mut pcm_8k);
                        }

                        // Only encode and send Thetis audio when Thetis PTT is active
                        if !ptt {
                            continue;
                        }

                        // The roger beep takes this frame instead of the
                        // microphone, and is paced by the same clock. It used
                        // to be sent from its own block once per timer tick,
                        // which is not the sound card's clock and does not
                        // pretend to be: the beep stuttered and came out short
                        // (2026-08-14). Here it inherits the pacing that speech
                        // has always had.
                        //
                        // Past the AGC on purpose (and past the gain at the front):
                        // those exist to even out a voice; a fixed tone handed to
                        // them comes out swelling, and what is set in the panel
                        // is what should go out.
                        let mut roger_here = false;
                        if let Some((ref mut tone, 0, _)) = roger_tone {
                            tone.fill(&mut pcm_8k, roger_cfg.volume);
                            roger_here = true;
                        }

                        // The limiter last, but not over the beep: what is set in the
                        // panel is what goes out. Meter = what is encoded.
                        if !roger_here {
                            thetis_limiter.process(&mut pcm_8k);
                        }
                        let (level, pcm_i16) = to_encoder(&pcm_8k, 1.0);
                        state.capture_level = level;

                        if pcm_i16.len() >= FRAME_SAMPLES_WIDEBAND {
                            match encoder.encode(&pcm_i16[..FRAME_SAMPLES_WIDEBAND]) {
                                Ok(opus_data) => {
                                    // Twice while the burst lasts.
                                    let out = thetis_tx.audio(thetis_ptt, opus_data, start.elapsed().as_millis() as u32);
                                    send_out!(out, thetis_tx.stream(), addr.as_str());

                                    if !logged_first_tx {
                                        info!("TX: first audio packet sent to {} (seq={}, accum_remain={})",
                                            addr, thetis_tx.next_sequence(), accum_buf.len());
                                        logged_first_tx = true;
                                    }
                                    sent_any = true;
                                }
                                Err(e) => {
                                    warn!("encode error: {}", e);
                                }
                            }
                        }
                    }

                    // Safety: prevent unbounded accumulation
                    if accum_buf.len() > capture_frame_samples * 10 {
                        warn!("Capture accumulator overflow ({}), draining", accum_buf.len());
                        let keep = accum_buf.len() - capture_frame_samples;
                        accum_buf.drain(..keep);
                    }

                    // The beep is sent above, in the loop that paces speech.
                    // What is left here is the release it was holding back.
                    // Done, or long past the point where it should have been.
                    // A tone that cannot be played would otherwise hold the
                    // transmitter and the slot for ever, and take every other
                    // channel's beep down with it - which is precisely what a
                    // stuck one did.
                    let roger_over = match roger_tone {
                        Some((ref t, _, started)) => {
                            let over = crate::roger::beep_is_over(
                                t.finished(),
                                started.elapsed().as_millis() as u64,
                                roger_cfg.duration_ms,
                                crate::roger::OVERDUE_MARGIN_MS,
                            );
                            if over && !t.finished() {
                                warn!("Roger beep did not play out in time: releasing PTT anyway");
                            }
                            over
                        }
                        None => false,
                    };
                    if roger_over {
                        let channel = match roger_tone {
                            Some((_, c, _)) => c,
                            None => 0,
                        };
                        match channel {
                            0 => {
                                thetis_ptt = false;
                                ptt = false;
                                state.ptt_denial.transmitter_off(0);
                                // The request ends here too. This path let go of
                                // the transmitter and left the request standing,
                                // so a late refusal could latch the busy sign
                                // again - the fault build 35 fixed, reopened
                                // through the courtesy beep.
                                ptt_requested[0] = false;
                                if let Some(ref addr) = server_addr {
                                    if audio_mode == 1 && last_sent_bin != Some(1) {
                                        let _ = send_control!(ControlId::Binaural, 1, addr.as_str());
                                        last_sent_bin = Some(1);
                                    }
                                }
                            }
                            1 => {
                                yaesu_ptt = false;
                                // Symmetrical with Thetis above. Letting go of
                                // a transmitter settles what was outstanding
                                // for it, and this branch did not - so a
                                // refusal on a radio could stay standing after
                                // the beep had already released it, with the
                                // busy sign left over.
                                state.ptt_denial.transmitter_off(1);
                                ptt_requested[1] = false;
                            }
                            _ => {
                                yaesu2_ptt = false;
                                state.ptt_denial.transmitter_off(2);
                                ptt_requested[2] = false;
                            }
                        }
                        info!("Roger beep done: PTT released");
                        roger_tone = None;
                    }

                    // PTT burst: send empty PTT-only packets for reliability
                    // (when no audio was sent this tick, e.g. PTT state change)
                    if let Some(out) = thetis_tx.tick_end(thetis_ptt, sent_any, start.elapsed().as_millis() as u32) {
                        send_out!(out, thetis_tx.stream(), addr.as_str());
                    }

                    // === Yaesu TX: completely separate mic audio path ===
                    // Applies to both radios (PTT mutually exclusive); per PTT
                    // the right EQ + mic-gain are chosen below.
                    let mut yaesu_audio_sent = [false; 2];
                    if yaesu_ptt || yaesu2_ptt {
                        // Resample to 16kHz, encode wideband Opus
                        while yaesu_tx_accum.len() >= capture_frame_samples {
                            let mut chunk: Vec<f32> = yaesu_tx_accum.drain(..capture_frame_samples).collect();
                            let pre_eq_peak = if playback_is_tx { 0.0 } else { peak_scaled(&chunk, 1.0) };
                            if !playback_is_tx && input_clips(pre_eq_peak) {
                                input_clip_at = Some(Instant::now());
                            }
                            // The mic gain at the front, so the compressor and the AGC
                            // work on the set level and not on whatever the microphone gives.
                            let mic_gain = if yaesu2_ptt { yaesu2_local_mic_gain } else { yaesu_local_mic_gain };
                            let front_gain = yaesu_front_gain(mic_gain, playback_is_tx);
                            apply_gain(&mut chunk, front_gain);

                            // Apply the 20-channel EQ at capture rate (before resampling).
                            // Per radio: slot-1 PTT → radio-2 EQ, otherwise radio-1 EQ.
                            // ONLY for the live mic: the TX-mic-EQ belongs to the microphone,
                            // not to a direct WAV-playback (which must sound as
                            // recorded). So skip it during playback_is_tx.
                            if !playback_is_tx {
                                if yaesu2_ptt {
                                    yaesu2_eq.process(&mut chunk);
                                } else {
                                    yaesu_eq.process(&mut chunk);
                                }
                            }

                            // Resample to 16kHz.
                            let mut resampled = resample_to_network(&mut yaesu_tx_resampler, &chunk);
                            // Client-side TX chain (radio processing does not work on USB):
                            // compressor → AGC, after the mic gain at the front.
                            // Per radio (like the EQ): slot-1 PTT → radio-2 chain.
                            // Compressor is self-gated on amount; AGC on its own toggle.
                            // Mic chain (compressor + AGC) ONLY for live mic, not for
                            // WAV-playback (which is already line level; comp/AGC would distort it).
                            let mut agc_in_peak = 0.0;
                            let mut agc_gain = 1.0;
                            if !playback_is_tx {
                                if yaesu2_ptt {
                                    yaesu2_compressor.process(&mut resampled);
                                    agc_in_peak = peak_scaled(&resampled, 1.0);
                                    if yaesu2_tx_agc_enabled && front_gain > 0.0 {
                                        yaesu2_agc.process(&mut resampled);
                                        agc_gain = yaesu2_agc.gain;
                                    }
                                } else {
                                    yaesu_compressor.process(&mut resampled);
                                    agc_in_peak = peak_scaled(&resampled, 1.0);
                                    if yaesu_tx_agc_enabled && front_gain > 0.0 {
                                        yaesu_agc.process(&mut resampled);
                                        agc_gain = yaesu_agc.gain;
                                    }
                                }
                            }
                            // The roger beep takes this frame instead of the
                            // microphone, paced by the same clock - see the
                            // Thetis path above for why that matters. It skips
                            // the EQ, compressor and AGC that ran above by
                            // being written over their output, and skips the
                            // gain trim by scaling at unity.
                            let mut roger_here = false;
                            if let Some((ref mut tone, ch, _)) = roger_tone {
                                if ch == 1 || ch == 2 {
                                    tone.fill(&mut resampled, roger_cfg.volume);
                                    roger_here = true;
                                }
                            }
                            // The limiter last, but not over the beep (as on Thetis).
                            let limiter_gain = if roger_here { 1.0 } else { yaesu_limiters[usize::from(yaesu2_ptt)].process(&mut resampled) };
                            let desired_bitrate = yaesu_tx_bitrate_for_mode(if yaesu2_ptt { state.yaesu2_mode } else { state.yaesu_mode });
                            if desired_bitrate != yaesu_tx_bitrate_bps {
                                match yaesu_tx_encoder.set_bitrate_bps(desired_bitrate) {
                                    Ok(()) => {
                                        yaesu_tx_bitrate_bps = desired_bitrate;
                                        info!("Yaesu TX Opus bitrate set to {} bps", desired_bitrate);
                                    }
                                    Err(e) => warn!("Yaesu TX Opus bitrate change failed: {}", e),
                                }
                            }
                            // Meter = the frame as encoded: the gain at the front, EQ,
                            // compressor, AGC and limiter applied. Covers live mic and
                            // WAV-playback, which share this chain.
                            let (level, pcm_i16) = to_encoder(&resampled, 1.0);
                            state.yaesu_mic_level = level;
                            // The transmit spectrum takes the same frame as the meter.
                            if let Some(ref tap) = tx_tap {
                                use crate::tx_tap::Source;
                                let source = if roger_here { Source::Roger } else if playback_is_tx { Source::Wav } else { Source::Mic };
                                tap.push(usize::from(yaesu2_ptt), &pcm_i16, source);
                            }
                            if !playback_is_tx && !roger_here {
                                let slot = usize::from(yaesu2_ptt);
                                let agc_on = if yaesu2_ptt { yaesu2_tx_agc_enabled } else { yaesu_tx_agc_enabled };
                                let radio = if yaesu2_ptt { "Yaesu2" } else { "Yaesu" };
                                if let Some(line) = yaesu_level_log[slot].add(radio, front_gain, pre_eq_peak, agc_in_peak, agc_gain,
                                                                              limiter_gain, level, agc_on) {
                                    dev_info!("{line}");
                                }
                            }

                            if pcm_i16.len() >= FRAME_SAMPLES_WIDEBAND {
                                if let Ok(opus_data) = yaesu_tx_encoder.encode(&pcm_i16[..FRAME_SAMPLES_WIDEBAND]) {
                                    if let Some(ref addr) = server_addr {
                                        let timestamp = start.elapsed().as_millis() as u32;

                                        // Where this frame goes.
                                        //
                                        // A roger beep belongs to one channel and goes only there - otherwise the other
                                        // radio beeps along without anything having been ended.
                                        //
                                        // Otherwise: with multi-TX to EVERY keyed slot, and without it to that one. It
                                        // was always that one, with slot 1 as the winner, and then the other one sat
                                        // transmitting without modulation.
                                        let (to_slot0, to_slot1) = match roger_tone {
                                            Some((_, ch, _)) => (ch == 1, ch == 2),
                                            None if multi_tx => (yaesu_ptt, yaesu2_ptt),
                                            None => (!yaesu2_ptt, yaesu2_ptt),
                                        };
                                        // One encoding, two addresses: no extra computation, but proportionally more
                                        // uplink. Each copy carries its own radio's flag and sequence (build 282), on
                                        // its own stream (build 291).
                                        // The last radio it goes to gets the frame itself, the
                                        // other a copy (before build 292 every radio got a copy).
                                        let mut frame = Some(opus_data);
                                        for (slot, to, keyed, last) in [(0usize, to_slot0, yaesu_ptt, !to_slot1), (1, to_slot1, yaesu2_ptt, true)] {
                                            if !to {
                                                continue;
                                            }
                                            let opus = if last { frame.take().unwrap_or_default() } else { frame.clone().unwrap_or_default() };
                                            let out = yaesu_tx[slot].audio(keyed, opus, timestamp);
                                            send_out!(out, yaesu_tx[slot].stream(), addr.as_str());
                                            yaesu_audio_sent[slot] = true;
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        yaesu_tx_accum.clear();
                    }

                    // Each radio's PTT burst (build 282), outside the audio block
                    // above: after a release no audio comes, and the burst is
                    // what carries the "off" - with the flag of the moment, as
                    // Thetis's (build 292).
                    for slot in 0..2 {
                        if let Some(ref addr) = server_addr {
                            let on = if slot == 0 { yaesu_ptt } else { yaesu2_ptt };
                            if let Some(out) = yaesu_tx[slot].tick_end(on, yaesu_audio_sent[slot], start.elapsed().as_millis() as u32) {
                                send_out!(out, yaesu_tx[slot].stream(), addr.as_str());
                            }
                        }
                    }

                    let _ = self.state_tx.send(state.clone());
                }

                _ = shutdown.changed() => {
                    info!("Client network shutting down");
                    if let Some(ref addr) = server_addr {
                        // Restore Thetis TXEQ if a WAV-playback left it bypassed.
                        if thetis_txeq_bypassed {
                            let _ = send_control!(ControlId::ThetisTxeq, 1, addr.as_str());
                            // The flag is not set back here: nothing reads it
                            // after this point, and a write that only the
                            // compiler notices is a question for whoever reads
                            // this next.
                            info!("Thetis TXEQ restored (shutdown)");
                        }
                        let mut buf = [0u8; DisconnectPacket::SIZE];
                        DisconnectPacket::serialize(&mut buf);
                        // The last send before `break`: its bytes reach no
                        // window, and the compiler flags this one count.
                        #[allow(unused_assignments)]
                        let _ = send_tx!(&buf, addr.as_str());
                        info!("Sent disconnect to server");
                    }
                    break;
                }
            }
        }

        // Over QUIC the close must leave before this engine - and on the phone
        // its runtime - is gone; otherwise the server waits out QUIC's idle
        // timeout (field test, build 111). The goodbye above may be dropped at
        // the close; the server reads the close as that goodbye.
        socket.stop.stop().await;

        Ok(())
    }
}

fn yaesu_tx_bitrate_for_mode(mode: u8) -> i32 {
    match mode {
        5 | 6 => 48_000, // FM and AM benefit most from the wider Opus budget.
        _ => 24_000,
    }
}

/// Resample i16 network-rate PCM -> f32 device rate
fn resample_to_device(resampler: &mut Resampler, pcm_i16: &[i16]) -> Vec<f32> {
    let input_f32: Vec<f32> = pcm_i16.iter().map(|&s| s as f32 / 32768.0).collect();
    match resampler.process(input_f32) {
        Ok(result) => result,
        Err(e) => {
            warn!("resample network->device error: {}", e);
            Vec::new()
        }
    }
}

/// The direction and filter of the capture and playback resamplers, for the
/// start line and the rebuild line, read from the resamplers themselves
/// (build 261).
fn resampler_line(capture: &Resampler, (nb, wb): (&Resampler, &Resampler)) -> String {
    format!("capture {}; playback 8 kHz {}, 16 kHz {}", capture, nb, wb)
}

/// Resample f32 device rate -> f32 8kHz
fn resample_to_network(resampler: &mut Resampler, pcm_f32: &[f32]) -> Vec<f32> {
    match resampler.process(pcm_f32.to_vec()) {
        Ok(result) => result,
        Err(e) => {
            warn!("resample device->8k error: {}", e);
            Vec::new()
        }
    }
}

/// Peak of `samples` as if scaled by `gain`, without touching the buffer.
///
/// The TX meters report the frame as it is encoded (EQ, compressor, AGC and the
/// gain trim included) while the scaling itself stays in the single existing
/// pass that builds the i16 frame - no extra copy on the TX path.
///
/// Peak rather than RMS: on transmit the question is how much headroom is left
/// before clipping, and speech RMS sits 10-15 dB below its peaks - which reads
/// as an alarmingly quiet bar while the audio is fine. Receive meters stay on
/// RMS, where average level is what tells you a stream is carrying signal.
fn peak_scaled(samples: &[f32], gain: f32) -> f32 {
    samples.iter().fold(0.0f32, |m, &s| m.max(s.abs())) * gain.abs()
}

/// The last step before an Opus encoder, the same for Thetis and both Yaesu
/// radios: the frame times `gain` as 16-bit samples, and its peak for the mic
/// meter on that same scale. 1.0 on the meter (0 dB) is the clamp at 32767:
/// no room left on the Opus line.
fn to_encoder(samples: &[f32], gain: f32) -> (f32, Vec<i16>) {
    let pcm = samples.iter().map(|&s| (s * gain * 32767.0).clamp(-32768.0, 32767.0) as i16).collect();
    (peak_scaled(samples, gain), pcm)
}

/// How often the engine asks whether a missing chosen microphone is back
/// (build 188); the looking itself is on the audio's own thread (build 189).
const MIC_LOOK_EVERY: Duration = Duration::from_secs(2);
/// The longest wait between tries to open a microphone that will not open
/// (build 190).
const MIC_TRY_MAX: Duration = Duration::from_secs(30);

/// The wait after a failed try: doubled, up to `MIC_TRY_MAX` (build 190).
fn next_mic_try(wait: Duration) -> Duration {
    (wait * 2).min(MIC_TRY_MAX)
}

/// Input calibration for a Yaesu receive meter, per radio model.
///
/// A Yaesu USB CODEC delivers a markedly lower line level than the Thetis DDC
/// audio, so without calibration the Yaesu bars sit below RX/VRX for the same
/// signal. (The volume path carried a x20 make-up factor for this until build
/// 106; the calibration below replaced it.)
///
/// The two radios are not alike: measured on comparable signals where RX/VRX
/// peak near -10 dB, the FT-991A needed +10 dB and the FTX-1 +24.5 dB (first
/// tried at +27, which read 2-3 dB hot).
/// One shared constant therefore lined up one radio and left the other far
/// behind. Keyed on the model rather than the slot, so it follows the radio
/// when the slots are swapped.
/// Receive calibration per radio model. Used for the level meter, the
/// playback gain and the recording (build 62): the two CODECs deliver line
/// levels about 14 dB apart, so one flat constant cannot serve both radios.
///
/// The audio path used a flat 20.0 (+26 dB) for both until build 106, while the
/// meter had been per-model since build 75/76. That left the 991A about 16 dB
/// hot - audible at the very bottom of the volume slider, where every other
/// channel had already gone quiet.
///
/// Two tables since build 62, chosen by the server (`rx_levels_tuned` in its
/// presence packet). Factory: the radios as they come, the values above. Tuned:
/// the radios set by the installation guide, so the USB CODEC itself runs near
/// -10 dBFS and only a small step is left. Measured 2026-11-02 with a -73 dBm
/// carrier on SSB, 1 kHz off, AGC on, as the mean level of a recording against
/// Thetis RX1 (-10.2 dBFS): FT-991A (OUT LEVEL 100, Windows 0 dB) -16.2 dBFS,
/// so +6.0 dB; FTX-1 (Windows +23 dB, one receiver's channel) -12.2 dBFS, so
/// +2.0 dB.
fn yaesu_rx_correction(model: u8, tuned: bool) -> f32 {
    match (model, tuned) {
        (1, false) => 16.8,  // FTX-1, +24.5 dB
        (_, false) => 3.16,  // FT-991A, +10 dB
        (1, true) => 1.26,   // FTX-1, +2.0 dB
        (_, true) => 2.0,    // FT-991A, +6.0 dB
    }
}

/// Apply volume scaling to audio samples
/// Per playout pass a dried-up meter keeps, i.e. how fast the bar falls once a
/// stream stops arriving. About 0.4 s to zero at 20 ms frames: slow enough to
/// read, fast enough to be believed.
const LEVEL_DECAY: f32 = 0.7;
/// Below this the bar is snapped to zero, so "no signal" is exactly zero and not
/// an ever-smaller number that never arrives.
const LEVEL_SILENCE: f32 = 0.001;

/// What a receive meter does when its stream dries up. ONE rule for all six
/// channels: fall back smoothly.
///
/// There used to be three. RX and Yaesu simply stopped writing the level, so the
/// bar froze on its last value indefinitely; VRX decayed; BinR snapped to zero.
/// Freezing is the one that lies, and it lies about the exact thing this bar is
/// used to diagnose - whether audio is arriving at all. Pinned by
/// `a_channel_that_stops_feeding_returns_to_zero` in
/// `tests/channel_level_parity.rs`.
fn decay_level(level: &mut f32) {
    *level *= LEVEL_DECAY;
    if *level < LEVEL_SILENCE {
        *level = 0.0;
    }
}

fn apply_volume(samples: &mut [f32], volume: f32) {
    if (volume - 1.0).abs() > f32::EPSILON {
        for s in samples.iter_mut() {
            *s *= volume;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every spectrum pan goes out as the pair, coarse then fine, through the one
    /// macro (build 75): no send site of its own, and none that forgets the fine
    /// one a newer server reads. The VRX width the same way.
    #[test]
    fn every_pan_and_vrx_width_goes_out_coarse_then_fine() {
        let src = include_str!("engine.rs");
        let body = &src[..src.find("#[cfg(test)]\nmod tests {").unwrap()];
        assert!(!body.contains("* 10000.0"), "no pan encoded by hand");
        assert_eq!(body.matches("send_pan!(ControlId::SpectrumPan, ControlId::SpectrumPanFine, ").count(), 2, "RX1: on change, on reconnect");
        assert_eq!(body.matches("send_pan!(ControlId::Rx2SpectrumPan, ControlId::Rx2SpectrumPanFine, ").count(), 2, "RX2: the same");
        // Each value under its own number: the whole send, not only the value.
        let mac = &body[body.find("macro_rules! send_pan {").unwrap()..];
        let mac = &mac[..mac.find("}};").unwrap()];
        let coarse = mac.find("send_control!($coarse, sdr_remote_core::protocol::pack_spectrum_pan(__pan), $addr)").expect("coarse");
        let fine = mac.find("send_control!($fine, sdr_remote_core::protocol::pack_spectrum_pan_fine(__pan), $addr)").expect("fine");
        assert!(coarse < fine, "coarse first, so a newer server ends on the fine one");
        let vrx = &body[body.find("Command::SetVrxHighResSpectrum(vrx_id, enabled, span_hz) => {").unwrap()..];
        let vrx = &vrx[..vrx.find("Command::SetMonitor").unwrap()];
        assert!(vrx.contains("(ControlId::VrxSpectrumEnable, ControlId::VrxSpectrumSpanKhz, ControlId::VrxSpectrumSpanFine)"));
        assert!(vrx.contains("(ControlId::VrxSpectrumEnable2, ControlId::VrxSpectrumSpanKhz2, ControlId::VrxSpectrumSpanFine2)"));
        let khz = vrx.find("send_control!(span_id, sdr_remote_core::protocol::vrx_span_khz(span_hz), addr.as_str())").expect("whole kHz");
        let fine = vrx.find("send_control!(fine_id, sdr_remote_core::protocol::pack_vrx_span_fine(span_hz), addr.as_str())").expect("fine");
        assert!(khz < fine);
    }

    /// The fine controls reach only a server that says it takes them (build
    /// 76): an older one fails to parse them, never acknowledges them, and the
    /// log fills with repeats given up. The gate sits in the one macro every
    /// control goes through, so the replay and the restore pass it too; the
    /// flag comes from the ack before the restore reads it, and goes with the
    /// session.
    #[test]
    fn the_fine_view_goes_only_to_a_server_that_takes_it() {
        let src = include_str!("engine.rs");
        let body = &src[..src.find("#[cfg(test)]\nmod tests {").unwrap()];
        let mac = &body[body.find("macro_rules! send_control {").unwrap()..];
        let mac = &mac[..mac.find("}};").unwrap()];
        let gate = mac.find("if !state.server_takes(__id) {").expect("the gate");
        assert!(gate < mac.find("controls_out.send(").unwrap(), "before it is numbered and kept for repeating");
        let held = &mac[gate..mac.find("} else {").unwrap()];
        assert!(held.contains("held_back.hold(__id, $value);\n                    Ok(0)"), "held, and nothing sent");
        // The flag: from the ack, whole, and set nowhere else.
        let flag = "state.server_fine_spectrum_view = ack.capabilities.has(\n                                sdr_remote_core::protocol::Capabilities::FINE_SPECTRUM_VIEW,\n                            );";
        let set = body.find(flag).expect("from the ack");
        assert_eq!(body.matches("state.server_fine_spectrum_view =").count(), 1, "one place sets it");
        let replay = body.find("let mut replay = session_intent.replay();").unwrap();
        assert!(set < replay, "before the restore");
        assert!(body[replay..].starts_with("let mut replay = session_intent.replay();\n                                    // Only what this server takes;"));
        assert!(body.contains("replay.retain(|(id, _)| state.server_takes(*id));"), "the replay offers only what the server takes");
        assert!(body.contains("matches!(send_control!(*id, *value, addr.as_str()), Ok(n) if n > 0)"), "the replay counts what went out");
        // The held values: only to a server that takes them, from one macro.
        let mac_held = &body[body.find("macro_rules! send_held_back {").unwrap()..];
        let mac_held = &mac_held[..mac_held.find("}};").unwrap()];
        assert!(mac_held.contains("if state.server_fine_spectrum_view {\n                    for (id, value) in held_back.take() {\n                        let _ = send_control!(id, value, $addr);"));
        // In the restore: after the replay, before the hand-written sends of the
        // current values (a pan made while the link was down is only in
        // those, and the held one must not land after it).
        let in_restore = body
            .find("info!(\"{}\", crate::session_intent::restore_summary(&replay, replay_sent));\n")
            .expect("the replay summary");
        let held_restore = body[in_restore..].find("send_held_back!(addr.as_str());").unwrap() + in_restore;
        assert!(held_restore < body.find("send_pan!(ControlId::SpectrumPan, ControlId::SpectrumPanFine, spectrum_pan, addr.as_str());").unwrap());
        assert!(held_restore < body.find("send_pan!(ControlId::Rx2SpectrumPan, ControlId::Rx2SpectrumPanFine, rx2_spectrum_pan, addr.as_str());").unwrap());
        // And on every ack, right after the restore block.
        assert!(body.contains("was_connected = true;\n                                // And on every ack, not only in the restore:"));
        let after = body.find("was_connected = true;\n                                // And on every ack").unwrap();
        assert!(body[after..].find("send_held_back!(addr.as_str());").unwrap() < 400);
        assert_eq!(body.matches("send_held_back!(").count(), 2, "the two callers");
        // Held only where the gate holds, and kept across a session: no other
        // use of the name than these.
        assert_eq!(body.matches("held_back.hold(").count(), 1, "only what the gate stopped");
        assert_eq!(body.matches("held_back.take()").count(), 1);
        // The name in the code, not the comments: the declaration, hold, take,
        // the macro and its two calls. A reset in any spelling (`= ...`,
        // `std::mem::take(&mut held_back)`) adds one.
        let in_code: usize = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .map(|l| l.matches("held_back").count())
            .sum();
        assert_eq!(in_code, 6, "never reset");
        assert_eq!(body.matches("let mut held_back").count(), 1);
        assert_eq!(body.matches("held_back.").count(), 2, "hold and take");
        // The restore's call stands outside every spectrum block, right before
        // them: inside the RX1 one, RX1 off would leave RX2 with the held pan last.
        assert!(body.contains("send_held_back!(addr.as_str());\n\n                                    // Send deferred spectrum settings now that server knows us:\n"));
        // The spectrum state goes either way, as Rx1Enable does: on or off, and
        // the view with it, not only when on.
        assert!(body.contains("let _ = send_control!(ControlId::SpectrumEnable, on as u16, addr.as_str());"));
        assert!(body.contains("let _ = send_control!(ControlId::Rx2SpectrumEnable, state.rx2_spectrum_enabled as u16, addr.as_str());"));
        assert!(!body.contains("if state.rx2_spectrum_enabled {\n                                        let _ = send_control!(ControlId::Rx2SpectrumEnable"));
        let st = include_str!("state.rs");
        let forget = &st[st.find("pub fn forget_session").unwrap()..];
        assert!(forget[..forget.find("\n    }").unwrap()].contains("self.server_fine_spectrum_view = false;"));
        assert_eq!(st.matches("self.server_fine_spectrum_view = ").count(), 1, "and nowhere else in the state");
        assert!(st.contains("self.server_fine_spectrum_view || !sdr_remote_core::protocol::needs_fine_spectrum_view(id)"));
    }

    /// The level line gives the four points of the chain over ten seconds of
    /// audio, the averages over the frames with speech, and starts a fresh
    /// window after it.
    #[test]
    fn the_tx_level_line_sums_up_its_window() {
        let mut log = TxLevelLog::default();
        assert_eq!(log.add("Yaesu", 0.8, 0.8, 0.7, 0.4, 1.0, 0.5, true), None);
        for i in 1..TX_LEVEL_LOG_FRAMES - 1 {
            // Every tenth frame a pause: below the speech threshold, gain at its top.
            let (agc_in, gain, out) = if i % 10 == 0 { (0.001, 10.0, 0.01) } else { (0.5, 0.5, 0.1) };
            // The limiter once, early: the line gives the lowest, not the last.
            let limiter = if i == 7 { 0.5 } else { 1.0 };
            assert_eq!(log.add("Yaesu", 0.8, 0.6, agc_in, gain, limiter, out, true), None);
        }
        let line = log.add("Yaesu", 0.8, 0.6, 0.5, 0.5, 1.0, 0.1, true).expect("ten seconds of audio");
        assert_eq!(line, "Yaesu TX level (10.0 s, 90% speech, gain -2 dB, AGC on): before EQ peak max 0.80, \
             AGC in peak avg 0.50 max 0.70, AGC gain avg 0.50, limiter min 0.50, \
             to Opus peak avg 0.10 max 0.50 (-6 dB)");
        assert_eq!(log.frames, 0);
    }

    /// Switching the AGC closes the window with its own line, so a line is
    /// about one setting and a gain below 1.0 never reads "AGC off".
    #[test]
    fn an_agc_switch_closes_the_window() {
        let mut log = TxLevelLog::default();
        for _ in 0..100 {
            assert_eq!(log.add("Yaesu2", 0.8, 0.3, 0.3, 0.4, 1.0, 0.2, true), None);
        }
        let line = log.add("Yaesu2", 0.8, 0.3, 0.3, 1.0, 1.0, 0.3, false).expect("the window so far");
        assert!(line.starts_with("Yaesu2 TX level (2.0 s, 100% speech, gain -2 dB, AGC on):"), "{line}");
        assert!(line.contains("AGC gain avg 0.40"), "{line}");
        assert_eq!((log.frames, log.agc_on), (1, false), "the new window holds the frame after the switch");
    }

    /// Moving the gain closes the window like an AGC switch, so the gain in a
    /// line is the gain of every frame in it; a gain at zero reads "off".
    #[test]
    fn a_gain_move_closes_the_window() {
        let mut log = TxLevelLog::default();
        for _ in 0..100 {
            assert_eq!(log.add("Yaesu", 1.0, 0.05, 0.05, 1.0, 1.0, 0.05, false), None);
        }
        let line = log.add("Yaesu", 10.0, 0.05, 0.5, 1.0, 1.0, 0.5, false).expect("the window so far");
        assert!(line.starts_with("Yaesu TX level (2.0 s, 100% speech, gain +0 dB, AGC off):"), "{line}");
        assert!(line.contains("AGC in peak avg 0.05 max 0.05"), "only the frames at +0 dB: {line}");
        assert_eq!((log.frames, log.gain), (1, 10.0), "the new window holds the frame after the move");
        // Down by one dB at the low end, where a step is small as a factor: the
        // direction and the size the setting procedure uses.
        let (minus5, minus6) = (crate::commands::tx_gain_factor(-5.0), crate::commands::tx_gain_factor(-6.0));
        let mut down = TxLevelLog::default();
        for _ in 0..100 {
            down.add("Yaesu", minus5, 0.2, 0.2, 1.0, 1.0, 0.2, false);
        }
        let line = down.add("Yaesu", minus6, 0.2, 0.2, 1.0, 1.0, 0.2, false).expect("one dB down closes too");
        assert!(line.starts_with("Yaesu TX level (2.0 s, 100% speech, gain -5 dB, AGC off):"), "{line}");
        let mut off = TxLevelLog::default();
        for _ in 0..TX_LEVEL_LOG_MIN_FRAMES {
            off.add("Yaesu2", 0.0, 0.2, 0.0, 1.0, 1.0, 0.0, false);
        }
        assert!(off.close("Yaesu2").unwrap().starts_with("Yaesu2 TX level (0.5 s, 0% speech, gain off, AGC off):"));
    }

    /// The end of an over closes the window; under half a second it gives no
    /// line, and either way the next over starts fresh.
    #[test]
    fn the_end_of_an_over_closes_the_window() {
        let mut log = TxLevelLog::default();
        for _ in 0..TX_LEVEL_LOG_MIN_FRAMES {
            log.add("Yaesu", 0.8, 0.3, 0.3, 1.0, 1.0, 0.3, false);
        }
        assert!(log.close("Yaesu").expect("half a second").starts_with("Yaesu TX level (0.5 s,"));
        for _ in 0..TX_LEVEL_LOG_MIN_FRAMES - 1 {
            log.add("Yaesu", 0.8, 0.3, 0.3, 1.0, 1.0, 0.3, false);
        }
        assert_eq!(log.close("Yaesu"), None, "less than half a second");
        assert_eq!(log.frames, 0);
        let src = include_str!("engine.rs");
        let body = &src[..src.find("#[cfg(test)]\nmod tests {").unwrap()];
        let release = body.find("// The over ends: its level window with it.").expect("the release");
        assert!(body[..release].trim_end().ends_with("yaesu_tx_accum.clear();"));
        assert!(body[release..].contains("for (slot, radio) in [(0, \"Yaesu\"), (1, \"Yaesu2\")] {"));
        let release = &body[release..release + 800];
        assert!(release.contains("thetis_limiter = TxLimiter::new();"), "the limiters start each over fresh");
        assert!(release.contains("yaesu_limiters = [TxLimiter::new(), TxLimiter::new()];"));
    }

    /// The level line comes every ten seconds of an over: a measurement line,
    /// so a developer line. It is built by `TxLevelLog` and written as
    /// `"{line}"`, which the general check on measurement lines cannot see.
    #[test]
    fn the_level_line_is_a_developer_line() {
        let src = include_str!("engine.rs");
        let body = &src[..src.find("#[cfg(test)]\nmod tests {").unwrap()];
        assert_eq!(body.matches("dev_info!(\"{line}\");").count(), 2, "the window's end and the ten seconds");
        assert_eq!(body.matches("info!(\"{line}\");").count(), 2, "and no plain one beside them");
        assert_eq!(body.matches("yaesu_level_log[slot].close(radio)").count(), 1);
        assert_eq!(body.matches("yaesu_level_log[slot].add(radio,").count(), 1);
    }

    /// The window is fed from the Yaesu branch with the four points in their
    /// order, per radio, and only for the live microphone; for both radios the
    /// AGC's input is read before the AGC and the gain reported is the AGC's.
    #[test]
    fn the_level_line_is_fed_in_order_per_radio_from_the_live_mic() {
        let src = include_str!("engine.rs");
        let body = &src[..src.find("#[cfg(test)]\nmod tests {").unwrap()];
        assert!(body.contains("let pre_eq_peak = if playback_is_tx { 0.0 } else { peak_scaled(&chunk, 1.0) };"));
        let at = body.find("yaesu_level_log[slot].add(radio, front_gain, pre_eq_peak, agc_in_peak, agc_gain,").expect("the feed");
        let rest = body[at..].split_once("agc_gain,").unwrap().1.trim_start();
        assert!(rest.starts_with("limiter_gain, level, agc_on)"), "the last three: {}", &rest[..40]);
        let before = &body[..at];
        let guard = before.rfind("if !playback_is_tx && !roger_here {").expect("the guard");
        assert!(before[guard..].contains("let slot = usize::from(yaesu2_ptt);"));
        assert!(before[guard..].contains("let radio = if yaesu2_ptt { \"Yaesu2\" } else { \"Yaesu\" };"));
        for (agc, gain) in [("yaesu2_agc.process(&mut resampled);", "agc_gain = yaesu2_agc.gain;"),
                            ("yaesu_agc.process(&mut resampled);", "agc_gain = yaesu_agc.gain;")] {
            let at_agc = body.find(agc).unwrap();
            let agc_in = body[..at_agc].rfind("agc_in_peak = peak_scaled(&resampled, 1.0);").unwrap();
            assert!(body[agc_in..at_agc].matches("process(").count() <= 1, "the AGC input is read right before {agc}");
            assert!(body[at_agc..].trim_start_matches(agc).trim_start().starts_with(gain), "{gain} right after {agc}");
        }
    }

    /// A raw frame at full scale lights the clip lamp, one just below does not;
    /// Thetis and the Yaesu branch test the raw frame, before the EQ, the same way.
    #[test]
    fn a_microphone_at_full_scale_lights_the_clip_lamp() {
        assert!(input_clips(peak_scaled(&[0.1, -1.0, 0.2], 1.0)));
        assert!(input_clips(0.995));
        assert!(!input_clips(peak_scaled(&[0.5, -0.98], 1.0)));
        let src = include_str!("engine.rs");
        let body = &src[..src.find("#[cfg(test)]\nmod tests {").unwrap()];
        assert!(body.contains("if input_clips(peak_scaled(&chunk, 1.0)) {"), "Thetis");
        let yaesu = body.find("if !playback_is_tx && input_clips(pre_eq_peak) {").expect("the Yaesu branch; a WAV is not the microphone");
        assert!(yaesu < body.find("yaesu2_eq.process(&mut chunk);").unwrap(), "before the EQ");
        assert!(body.contains("state.mic_input_clip = input_clip_at.is_some_and(|t| t.elapsed() < INPUT_CLIP_HOLD);"));
    }

    /// The AGC brings a steady voice to its target: a loud one down, a quiet
    /// one up. Below the gate it leaves the gain where it is.
    #[test]
    fn the_agc_lowers_a_loud_voice_and_lifts_a_quiet_one() {
        let settle = |level: f32| {
            let mut agc = TxAgc::new();
            let mut out = Vec::new();
            for _ in 0..200 {
                out = vec![level; 320];
                agc.process(&mut out);
            }
            (agc.gain, out[0])
        };
        let (gain, out) = settle(0.8);
        assert!(gain < 1.0 && (out - AGC_TARGET).abs() < 0.01, "loud: gain {gain}, out {out}");
        let (gain, out) = settle(0.1);
        assert!(gain > 1.0 && (out - AGC_TARGET).abs() < 0.01, "quiet: gain {gain}, out {out}");
        let (gain, _) = settle(AGC_GATE / 2.0);
        assert_eq!(gain, 1.0, "below the gate");
    }

    /// The gain moved to the front is the factor that was at the end: four
    /// times the stored mic gain for the live microphone, the stored gain for a
    /// WAV. With the compressor and the AGC off a profile sounds as loud as it
    /// did, as the owner decided.
    #[test]
    fn the_gain_at_the_front_is_the_one_that_was_at_the_end() {
        for g in [0.02f32, 0.1, 0.2, 0.4] {
            assert_eq!(yaesu_front_gain(g, false), g * 4.0, "live");
            assert_eq!(yaesu_front_gain(g, true), g, "WAV");
        }
        let mut v = [0.1f32, -0.2];
        apply_gain(&mut v, 2.0);
        assert_eq!(v, [0.2, -0.4], "the gain is applied");
    }

    /// The EQ is linear, so a gain in front of it gives what the same gain
    /// behind it gave: moving the gain changes nothing with comp and AGC off.
    #[test]
    fn the_eq_is_linear_so_the_gain_may_move_to_the_front() {
        let mut bands = crate::eq::default_bands();
        bands[0].gain_db = 6.0;
        bands[6] = crate::eq::EqBand { kind: crate::eq::FilterKind::HighPass, freq_hz: 150.0, q: 0.71, gain_db: 0.0 };
        let make = || {
            let mut e = crate::eq::Equalizer::new(48_000.0);
            e.set_enabled(true);
            for (i, b) in bands.iter().enumerate() { e.set_band(i, *b); }
            e
        };
        let input: Vec<f32> = (0..960).map(|i| (i as f32 * 0.05).sin() * 0.1).collect();
        let k = 3.2;
        let mut front: Vec<f32> = input.iter().map(|s| s * k).collect();
        make().process(&mut front);
        let mut back = input.clone();
        make().process(&mut back);
        assert!(back.iter().zip(input.iter()).any(|(b, i)| (b - i).abs() > 1e-3), "the EQ does something");
        // Within float rounding through twenty biquads: -80 dB of the frame's peak.
        let peak = back.iter().fold(0.0f32, |m, s| m.max(s.abs())) * k;
        for (a, b) in front.iter().zip(back.iter()) {
            assert!((a - b * k).abs() <= 1e-4 * peak, "{a} vs {}", b * k);
        }
    }

    /// Below the ceiling the limiter does nothing; a peak over it is brought to
    /// the ceiling and no sample goes past it; afterwards the gain comes back.
    #[test]
    fn the_limiter_holds_the_ceiling_and_lets_go() {
        assert!((LIMITER_CEILING - 10f32.powf(-1.0 / 20.0)).abs() < 1e-3, "the ceiling is -1 dB, as decided");
        let mut lim = TxLimiter::new();
        let mut quiet = vec![0.5f32; 320];
        assert_eq!(lim.process(&mut quiet), 1.0);
        assert!(quiet.iter().all(|&s| s == 0.5));
        let mut loud: Vec<f32> = (0..320).map(|i| if i % 40 == 0 { 2.0 } else { 0.3 }).collect();
        let lowest = lim.process(&mut loud);
        assert!(loud.iter().all(|s| s.abs() <= LIMITER_CEILING + 1e-6), "past the ceiling");
        assert!((lowest - LIMITER_CEILING / 2.0).abs() < 1e-6, "{lowest}");
        for _ in 0..20 {
            let mut f = vec![0.1f32; 320];
            lim.process(&mut f);
        }
        assert!(lim.gain > 0.99, "it lets go: {}", lim.gain);
    }

    /// The transmit spectrum takes the frame the meter takes: right after
    /// `to_encoder`, before the encoder, on the Yaesu path only, with the roger
    /// beep flagged so the last over leaves it out.
    #[test]
    fn the_tx_spectrum_taps_where_the_meter_does() {
        let src = include_str!("engine.rs");
        let body = &src[..src.find("#[cfg(test)]
mod tests {").unwrap()];
        let pos = |needle: &str| body.find(needle).unwrap_or_else(|| panic!("{needle}"));
        let meter = pos("let (level, pcm_i16) = to_encoder(&resampled, 1.0);");
        let tap = pos("tap.push(usize::from(yaesu2_ptt), &pcm_i16, source);");
        let source = pos("let source = if roger_here { Source::Roger } else if playback_is_tx { Source::Wav } else { Source::Mic };");
        assert!(meter < source && source < tap, "the source is told right at the tap");
        let encode = pos("yaesu_tx_encoder.encode(&pcm_i16[..FRAME_SAMPLES_WIDEBAND])");
        assert!(meter < tap && tap < encode);
        assert_eq!(body.matches("tap.push(").count(), 1, "one tap, on the Yaesu path");
    }

    /// The order of the chain: on the raw frame the clip lamp, then the gain at
    /// the front, then (Yaesu) the EQ; the limiter right before the encoder and
    /// not over the roger beep. Thetis the same, without EQ and compressor.
    #[test]
    fn the_gain_is_in_front_and_the_limiter_last() {
        let src = include_str!("engine.rs");
        let body = &src[..src.find("#[cfg(test)]\nmod tests {").unwrap()];
        let pos = |needle: &str| body.find(needle).unwrap_or_else(|| panic!("{needle}"));
        // Yaesu
        assert!(pos("if !playback_is_tx && input_clips(pre_eq_peak) {") < pos("apply_gain(&mut chunk, front_gain);"));
        assert!(pos("apply_gain(&mut chunk, front_gain);") < pos("yaesu2_eq.process(&mut chunk);"));
        let lim = pos("let limiter_gain = if roger_here { 1.0 } else { yaesu_limiters[usize::from(yaesu2_ptt)].process(&mut resampled) };");
        assert!(lim < pos("let (level, pcm_i16) = to_encoder(&resampled, 1.0);"));
        assert!(pos("tone.fill(&mut resampled, roger_cfg.volume);") < lim, "the beep is in before the limiter is skipped for it");
        // Thetis
        assert!(pos("if input_clips(peak_scaled(&chunk, 1.0)) {") < pos("apply_gain(&mut chunk, tx_gain);"));
        assert!(pos("apply_gain(&mut chunk, tx_gain);") < pos("let mut pcm_8k = resample_to_network(&mut resampler_in, &chunk);"));
        assert!(pos("thetis_limiter.process(&mut pcm_8k);") < pos("let (level, pcm_i16) = to_encoder(&pcm_8k, 1.0);"));
        assert!(body.contains("if !roger_here {\n                            thetis_limiter.process(&mut pcm_8k);"), "not over the beep");
        // The AGCs rest while the gain is off.
        for agc in ["if agc_enabled && tx_gain > 0.0 {", "if yaesu_tx_agc_enabled && front_gain > 0.0 {",
                    "if yaesu2_tx_agc_enabled && front_gain > 0.0 {"] {
            assert!(body.contains(agc), "{agc}");
        }
        assert!(pos("apply_gain(&mut f16, tx_gain);") < pos("thetis_limiter.process(&mut f16);"));
        // No gain left at the end: every path encodes at 1.0.
        for call in ["to_encoder(&f16[..FRAME_SAMPLES_WIDEBAND], 1.0);", "to_encoder(&pcm_8k, 1.0);", "to_encoder(&resampled, 1.0);"] {
            assert!(body.contains(call), "{call}");
        }
        assert!(!body.contains("mic_gain * 4.0"), "the old boost at the end is gone");
    }

    /// The meter and the encoder see the same frame on the same scale: a peak
    /// of 1.0 is the clamp, and beyond it the samples are clamped, not wrapped.
    #[test]
    fn the_meter_reads_what_the_encoder_gets() {
        let (peak, pcm) = to_encoder(&[0.25, -0.6, 0.5], 2.0);
        assert_eq!(peak, 1.2);
        assert_eq!(pcm, [16383, -32768, 32767]);
        let (peak, _) = to_encoder(&[0.5], 1.0);
        assert_eq!(peak, 0.5, "half scale, -6 dB on the meter");
    }

    /// Thetis (live and WAV) and both Yaesu radios go through `to_encoder`,
    /// so their meters measure the room on the Opus line the same way.
    #[test]
    fn every_tx_path_meters_through_to_encoder() {
        let src = include_str!("engine.rs");
        let body = &src[..src.find("#[cfg(test)]\nmod tests {").unwrap()];
        assert_eq!(body.matches("= to_encoder(").count(), 3);
        assert_eq!(body.matches("32767.0).clamp(").count(), 1, "one conversion, in to_encoder");
        assert!(!body.contains("_level = peak_scaled("), "a meter written past to_encoder");
    }

    /// The default compressor timing gives the former fixed coefficients, so
    /// someone who never touches the sliders hears no change.
    #[test]
    fn compressor_default_timing_matches_the_former_coefficients() {
        let c = TxCompressor::new();
        assert!((c.attack_coeff - 0.30).abs() < 0.005, "attack {}", c.attack_coeff);
        assert!((c.release_coeff - 0.02).abs() < 0.0005, "release {}", c.release_coeff);
        let mut slow = TxCompressor::new();
        slow.set_timing(10.0, 200.0);
        assert!(slow.attack_coeff < c.attack_coeff && slow.release_coeff < c.release_coeff);
    }

    /// The compressor arms log only a change of the amount; a slider sends
    /// the same value again on many frames.
    #[test]
    fn the_compressor_reports_only_a_change() {
        let mut c = TxCompressor::new();
        assert!(!c.set_amount(0.0));
        assert!(c.set_amount(0.4));
        assert!(!c.set_amount(0.4));
    }

    /// The TX-mic EQ runs on the capture frames, so both are built for the
    /// capture rate and both follow a device switch, each in a statement of
    /// its own. A test could drive the loop with the engine harness of
    /// `tests/connect_state_machine.rs`; none does yet, so the log line
    /// "Yaesu TX EQ at the capture rate" is the proof.
    #[test]
    fn both_tx_eqs_follow_the_capture_rate() {
        let src = include_str!("engine.rs");
        let src = &src[..src.find("#[cfg(test)]\nmod tests {").unwrap()];
        assert_eq!(src.matches("crate::eq::Equalizer::new(capture_rate as f32)").count(), 2);
        assert_eq!(src.matches("Equalizer::new(").count(), 2);
        let resync = &src[src.find("macro_rules! resync_audio_rates").unwrap()..];
        let resync = &resync[..resync.find("info!(\"Resamplers rebuilt").unwrap()];
        for (moved, eq) in [("eq1_moved", "yaesu_eq"), ("eq2_moved", "yaesu2_eq")] {
            assert!(resync.contains(&format!("let {moved} = {eq}.set_sample_rate(capture_rate as f32);")), "{eq}");
        }
        assert!(resync.contains("if eq1_moved || eq2_moved {"));
    }

    /// Both Yaesu's received audio through one block (build 293; until then a
    /// copy per radio, and radio 2's log line
    /// said "[radio1]"). Each arm calls it once, with its own radio's buffer,
    /// stream, first-packet mark, loss meter, measurement stream and screen
    /// figures - a name of the other radio in an arm is the fault this pins.
    /// The loop, which no test drives.
    #[test]
    fn both_radios_receive_through_one_block() {
        let src = include_str!("engine.rs");
        let src = &src[..src.find("#[cfg(test)]\nmod tests {").unwrap()];
        assert_eq!(src.matches("yaesu_audio_in!(").count(), 2, "two arms");
        for (arm, call) in [
            ("Ok(Packet::AudioYaesu(pkt)) => {", "yaesu_audio_in!(pkt, \"Radio 1\", yaesu_jitter_buf, st_yaesu, yaesu_logged_first, radio1, crate::loss_window::Stream::Radio1,\n                                yaesu_audio_packets, yaesu_jitter_ms, yaesu_buffer_depth);"),
            ("Ok(Packet::AudioYaesu2(pkt)) => {", "yaesu_audio_in!(pkt, \"Radio 2\", yaesu2_jitter_buf, st_yaesu2, yaesu2_logged_first, radio2, crate::loss_window::Stream::Radio2,\n                                yaesu2_audio_packets, yaesu2_jitter_ms, yaesu2_buffer_depth);"),
        ] {
            let at = src.find(arm).unwrap_or_else(|| panic!("{arm}"));
            assert!(src[at..at + 400].contains(call), "{arm} calls the block with its own radio");
        }
        // Nothing else puts a Yaesu's received audio in its buffer.
        assert_eq!(src.matches("yaesu_jitter_buf.push(").count() + src.matches("yaesu2_jitter_buf.push(").count(), 0);
        assert_eq!(src.matches("$jb.push(").count(), 1);
    }

    /// A radio's PTT is the flag on its audio (build 282): no
    /// control keys a Yaesu any more, every copy of a frame carries its own
    /// radio's flag and sequence, and each radio's burst runs outside the audio
    /// block - after a release no audio comes, and the burst carries the "off".
    /// The loop, which no test drives (the field log is the
    /// proof that it runs).
    #[test]
    fn a_radios_ptt_rides_on_its_audio() {
        let src = include_str!("engine.rs");
        let src = &src[..src.find("#[cfg(test)]
mod tests {").unwrap()];
        for gone in ["ControlId::YaesuPtt,", "ControlId::Yaesu2Ptt,", "OwedOff", "owed_off"] {
            assert_eq!(src.matches(gone).count(), 0, "{gone}");
        }
        // Since build 292 every transmitter bursts Thetis's way: the change is
        // seen at the audio tick, for all three, and no packet goes on the
        // button (`tx_stream`).
        assert_eq!(src.matches("yaesu_ptt_changed!").count(), 0, "no packet of its own on the button");
        assert!(src.contains("if thetis_tx.ptt(ptt) {"));
        let thetis_change = src.find("if thetis_tx.ptt(ptt) {").unwrap();
        let radios_change = src.find("yaesu_tx[0].ptt(yaesu_ptt);\n                    yaesu_tx[1].ptt(yaesu2_ptt);").expect("the radios' change, with Thetis's");
        assert!(thetis_change < radios_change && radios_change - thetis_change < 200);
        let audio_tick = src.find("let mut sent_any = false;").unwrap();
        assert!(radios_change < audio_tick, "seen before the audio of the tick, so its first frames double");
        assert!(src.contains("for (slot, to, keyed, last) in [(0usize, to_slot0, yaesu_ptt, !to_slot1), (1, to_slot1, yaesu2_ptt, true)] {"));
        // Each copy on its own radio's stream, with that radio's flag
        // (`tx_stream`, build 291); the last radio without a copy (292).
        assert!(src.contains("let out = yaesu_tx[slot].audio(keyed, opus, timestamp);
                                            send_out!(out, yaesu_tx[slot].stream(), addr.as_str());
                                            yaesu_audio_sent[slot] = true;"));
        // The burst after the audio block's `else`, every tick, with the flag
        // of the moment.
        let gate_end = src.find("                    } else {
                        yaesu_tx_accum.clear();
                    }").unwrap();
        let burst = src.find("if let Some(out) = yaesu_tx[slot].tick_end(on, yaesu_audio_sent[slot], start.elapsed().as_millis() as u32) {").unwrap();
        assert!(gate_end < burst && burst - gate_end < 1_000, "right after the audio block, outside it");
        // Thetis's from its own stream: the microphone and the WAV both as
        // audio (the WAV since 292), and the packet with only the flag.
        assert_eq!(src.matches("let out = thetis_tx.audio(thetis_ptt, opus_data, start.elapsed().as_millis() as u32);").count(), 2, "the microphone and the WAV");
        assert!(src.contains("send_out!(out, thetis_tx.stream(), addr.as_str());
                                            sent_any = true;"), "the WAV counts as audio in the tick");
        assert!(src.contains("if let Some(out) = thetis_tx.tick_end(thetis_ptt, sent_any, start.elapsed().as_millis() as u32) {"));
        // No packet of the microphone's audio is built anywhere else.
        assert_eq!(src.matches("AudioPacket {").count(), 0, "every transmit packet comes from tx_stream");
        assert_eq!(src.matches("send_out!(").count(), 5, "the radios' audio and burst; Thetis's microphone, WAV, flag");
    }

    /// While transmitting the link is gone after 3 s without an answer to a
    /// heartbeat, whatever the audio does; listening keeps max(6 s, rtt x 8)
    /// with both absent (build 265).
    #[test]
    fn a_link_without_answers_ends_a_transmission_after_three_seconds() {
        let ms = |v: u64| Some(Duration::from_millis(v));
        // Transmitting: the answer alone, at 3000/3001.
        assert_eq!(link_lost(true, 0, ms(3_000), ms(3_000)), None);
        assert_eq!(link_lost(true, 0, ms(3_001), None), Some(3_000));
        assert_eq!(link_lost(true, 0, ms(3_001), ms(20)), Some(3_000), "audio still arriving keeps nothing keyed");
        assert_eq!(link_lost(true, 900, ms(3_001), ms(20)), Some(3_000), "not stretched by the round trip");
        assert_eq!(link_lost(true, 0, None, None), None, "never answered is no verdict");
        // Listening: as it was.
        assert_eq!(link_lost(false, 0, ms(3_001), None), None);
        assert_eq!(link_lost(false, 0, ms(6_001), ms(20)), None, "audio keeps a listener");
        assert_eq!(link_lost(false, 0, ms(6_001), None), Some(6_000));
        assert_eq!(link_lost(false, 0, ms(6_001), ms(6_001)), Some(6_000));
        assert_eq!(link_lost(false, 1_000, ms(6_001), None), None, "rtt x 8");
        assert_eq!(link_lost(false, 1_000, ms(8_001), None), Some(8_000));
        assert_eq!(link_lost(false, 0, None, None), None);
        // And the loop asks it, with the three transmit flags.
        let src = include_str!("engine.rs");
        let src = &src[..src.find("#[cfg(test)]\nmod tests {").unwrap()];
        assert_eq!(src.matches("link_lost(transmitting, last_hb_ack_rtt, since_ack, since_audio)").count(), 1);
        assert_eq!(src.matches("let transmitting = thetis_ptt || yaesu_ptt || yaesu2_ptt;").count(), 1);
    }

    /// Every reopen of the audio takes over whether the chosen microphone is
    /// missing, on the next line - a reopen that forgot it would leave the
    /// desktop refusing to transmit with the microphone back, or transmitting
    /// with none (build 188). And the missing one is looked for.
    #[test]
    fn every_reopen_of_the_audio_says_whether_the_microphone_is_missing() {
        let src = include_str!("engine.rs");
        let src = &src[..src.find("#[cfg(test)]
mod tests {").unwrap()];
        let lines: Vec<&str> = src.lines().map(str::trim).collect();
        // All four through one block: the device switches, the recovery and the
        // microphone coming back.
        assert_eq!(src.matches("take_new_audio!(new_audio);").count(), 4);
        assert!(!lines.contains(&"audio = new_audio;"), "a reopen beside the block");
        let at = lines.iter().position(|l| *l == "audio = $new;").expect("the block");
        assert_eq!(lines[at + 1], "state.mic_missing = audio.missing_input();", "a reopen forgets the microphone");
        // And the tries start over at 2 s.
        assert_eq!(lines[at + 2], "mic_try_every = MIC_LOOK_EVERY;", "a reopen keeps an old rhythm");
        assert_eq!(src.matches("if state.mic_missing.is_some() && mic_looked_at.elapsed() >= mic_try_every {").count(), 1);
        // Back, or audio that could not open the chosen one and so cannot watch
        // for it (build 189).
        assert_eq!(src.matches("if back || audio.missing_input().is_none() {").count(), 1);
        // A failed try waits longer, one that worked starts over (build 190).
        assert_eq!(src.matches("mic_try_every = next_mic_try(mic_try_every);").count(), 1);
        assert_eq!(src.matches("mic_try_every = MIC_LOOK_EVERY;").count(), 3, "the start, the reopen block, and a new failure");
        // A chosen microphone that fails to open refuses to transmit too.
        assert_eq!(src.matches("state.mic_missing = Some(input_device_name.clone());").count(), 1);
    }

    /// A press held across a reopen of the audio keeps transmitting: the new
    /// backend's gate is shut and only the edge of a press opens it, so the
    /// reopen block opens it when a press is held, and says so. Until build 42 a
    /// headset joining during an over stopped the audio; the server let the radio
    /// go while the button stayed red.
    #[test]
    fn a_press_held_across_a_reopen_of_the_audio_keeps_its_gate_open() {
        let src = include_str!("engine.rs");
        let src = &src[..src.find("#[cfg(test)]\nmod tests {").unwrap()];
        let block = &src[src.find("macro_rules! take_new_audio").expect("the block")..];
        let block = &block[..block.find("}};").expect("its end")];
        let compact: String = block.split_whitespace().collect::<Vec<_>>().join(" ");
        // The whole block, to its end, so no line of it can move, go or follow.
        assert!(compact.ends_with("($new:expr) => {{ audio = $new; state.mic_missing = audio.missing_input(); \
            mic_try_every = MIC_LOOK_EVERY; audio.set_capture_gate_delay_ms(mic_gate_delay_ms); \
            audio.set_playback_mute(playback_muted); resync_audio_rates!(); \
            if last_capture_ptt && (ptt || yaesu_ptt || yaesu2_ptt) { accum_buf.clear(); yaesu_tx_accum.clear(); \
            audio.set_capture_gate(true); info!(\"Audio reopened during a press: the capture gate is open again\"); }"));
        // The mute it sets is the last one asked for, kept where it is asked.
        let compact_src: String = src.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(compact_src.contains("Command::SetPlaybackMute(v) => { playback_muted = v; audio.set_playback_mute(v);"));
        assert_eq!(src.matches("set_playback_mute(").count(), 2, "the command and the block");
        assert_eq!(src.matches("playback_muted =").count(), 2, "declared, and written by the command only");
        // A backend is made at the start and in the block's four callers, nowhere else.
        assert_eq!(src.matches("audio_factory(").count(), 5);
        // The flag it reads is the one the edge keeps.
        assert_eq!(src.matches("last_capture_ptt = capture_ptt;").count(), 1);
        assert_eq!(src.matches("let capture_ptt = ptt || yaesu_ptt || yaesu2_ptt;").count(), 1);
    }

    /// The tries to open a microphone that will not open come further apart:
    /// 2, 4, 8, 16, 30, 30 s.
    #[test]
    fn tries_to_open_a_microphone_come_further_apart() {
        let mut w = MIC_LOOK_EVERY;
        let mut seen = vec![w.as_secs()];
        for _ in 0..5 {
            w = next_mic_try(w);
            seen.push(w.as_secs());
        }
        assert_eq!(seen, vec![2, 4, 8, 16, 30, 30]);
    }

    /// `ptt` mirrors `thetis_ptt` for the audio side: it keeps RX1 out of the
    /// playout ring while transmitting. Every place that sets `thetis_ptt` sets the
    /// mirror within the next two lines - a fifth link-down that forgets it leaves
    /// `ptt` true, and the operator hears silence.
    #[test]
    fn the_ptt_mirror_follows_thetis_ptt() {
        let src = include_str!("engine.rs");
        let body = &src[..src.find("#[cfg(test)]\nmod tests").unwrap()];
        let lines: Vec<&str> = body.lines().map(str::trim).collect();
        let sets = |l: &str, name: &str| l.starts_with(name) && l[name.len()..].trim_start().starts_with('=') && !l[name.len()..].trim_start().starts_with("==");
        let thetis: Vec<usize> = (0..lines.len()).filter(|&i| sets(lines[i], "thetis_ptt")).collect();
        let mirror: Vec<usize> = (0..lines.len()).filter(|&i| sets(lines[i], "ptt")).collect();
        assert!(!thetis.is_empty());
        assert_eq!(thetis.len(), mirror.len(), "every thetis_ptt assignment has its ptt mirror");
        for &i in &thetis {
            // The value too: `ptt = true;` next to `thetis_ptt = false;`
            // is exactly the silence this test is for.
            let want = if lines[i] == "thetis_ptt = false;" { "ptt = false;" } else { "ptt = thetis_ptt;" };
            assert!(
                (i + 1..=i + 2).any(|j| mirror.contains(&j) && lines[j] == want),
                "`{}` at body line {} without `{want}` in the next two lines",
                lines[i],
                i + 1
            );
        }
    }

    /// The engine thread runs through the reporter, and a panic in it leaves
    /// "no connection" on screen instead of the last state.
    /// A body that returns normally changes nothing - the engine said its own
    /// last word already.
    #[test]
    fn a_panicking_engine_leaves_no_connection_on_screen() {
        let (engine, rx, _cmd) = ClientEngine::new();
        engine.state_tx.send_modify(|s| {
            s.connected = true;
            s.connect_status = crate::state::ConnectStatus::Connected;
            s.server_tx = true;
            s.path = Some(sdr_remote_core::path::PathKind::DirectViaRelay);
        });
        let reporter = engine.stop_reporter();
        assert!(!reporter.run(|| Ok(())), "no panic, nothing to report");
        assert!(rx.borrow().connected, "a normal return must leave the state alone");
        let reporter = engine.stop_reporter();
        assert!(reporter.run(|| panic!("a fault in the engine")));
        let s = rx.borrow().clone();
        assert!(!s.connected, "a dead engine still showed a connection");
        assert!(!s.server_tx, "a dead engine still showed a transmitter");
        assert_eq!(s.path, None, "a dead engine still named a way on two screens");
        assert_eq!(s.connect_status, crate::state::ConnectStatus::Disconnected);
        assert!(s.engine_dead);
    }

    /// An engine that returns an error - its audio device would not open - is
    /// as dead as one that panicked, and the screens have to say so. At first only
    /// a panic counted.
    #[test]
    fn an_engine_that_returns_an_error_is_dead_too() {
        let (engine, rx, _cmd) = ClientEngine::new();
        engine.state_tx.send_modify(|s| s.connected = true);
        let reporter = engine.stop_reporter();
        assert!(reporter.run(|| Err(anyhow::anyhow!("no audio device"))));
        let s = rx.borrow().clone();
        assert!(s.engine_dead, "an engine that returned an error was not marked dead");
        assert!(!s.connected);
    }

    // Regression: the presence flip is the core requirement; presence determines connected,
    // dynamically true/false. Tests the pure apply_yaesu_presence seam.
    #[test]
    fn presence_drives_connected_flip() {
        let mut state = RadioState::default();
        assert!(!state.yaesu_connected);
        assert!(!state.yaesu2_connected);

        // Both present → both connected, models adopted, both changed.
        let (c0, c1) = apply_yaesu_presence(&mut state, &YaesuPresencePacket {
            slot0_present: true, slot0_model: 0, slot1_present: true, slot1_model: 1, slot0_trouble: 0, slot1_trouble: 0, slot0_configured: true, slot1_configured: true, rx_levels_tuned: false,
        });
        assert!(c0 && c1);
        assert!(state.yaesu_connected && state.yaesu2_connected);
        assert_eq!(state.yaesu_model, 0);
        assert_eq!(state.yaesu2_model, 1);

        // Slot 0 drops out → connected flips to false (core of the dynamics),
        // slot 1 unchanged (no change flag).
        let (c0, c1) = apply_yaesu_presence(&mut state, &YaesuPresencePacket {
            slot0_present: false, slot0_model: 0, slot1_present: true, slot1_model: 1, slot0_trouble: 0, slot1_trouble: 0, slot0_configured: true, slot1_configured: true, rx_levels_tuned: false,
        });
        assert!(c0 && !c1);
        assert!(!state.yaesu_connected);
        assert!(state.yaesu2_connected);

        // Idempotent: same presence again → no change flags.
        let (c0, c1) = apply_yaesu_presence(&mut state, &YaesuPresencePacket {
            slot0_present: false, slot0_model: 0, slot1_present: true, slot1_model: 1, slot0_trouble: 0, slot1_trouble: 0, slot0_configured: true, slot1_configured: true, rx_levels_tuned: false,
        });
        assert!(!c0 && !c1);
    }

    // Regression: an absent radio must not leave a hi_swr flag set. Otherwise
    // the other radio keeps renewing the SWR alarm on every state push,
    // the alarm then never goes out.
    #[test]
    fn presence_absence_clears_hi_swr() {
        let mut state = RadioState::default();
        apply_yaesu_presence(&mut state, &YaesuPresencePacket {
            slot0_present: true, slot0_model: 0, slot1_present: true, slot1_model: 1, slot0_trouble: 0, slot1_trouble: 0, slot0_configured: true, slot1_configured: true, rx_levels_tuned: false,
        });
        state.yaesu_hi_swr = true;
        state.yaesu2_hi_swr = true;

        // Slot 1 disappears → only its flag is cleared.
        apply_yaesu_presence(&mut state, &YaesuPresencePacket {
            slot0_present: true, slot0_model: 0, slot1_present: false, slot1_model: 1, slot0_trouble: 0, slot1_trouble: 0, slot0_configured: true, slot1_configured: true, rx_levels_tuned: false,
        });
        assert!(state.yaesu_hi_swr, "the radio that is present keeps its flag");
        assert!(!state.yaesu2_hi_swr, "the radio that is gone loses its flag");

        // Slot 0 disappears too → nothing remains to feed the alarm.
        apply_yaesu_presence(&mut state, &YaesuPresencePacket {
            slot0_present: false, slot0_model: 0, slot1_present: false, slot1_model: 1, slot0_trouble: 0, slot1_trouble: 0, slot0_configured: true, slot1_configured: true, rx_levels_tuned: false,
        });
        assert!(!state.yaesu_hi_swr && !state.yaesu2_hi_swr);
    }

    /// A radio the server has but that is switched off: configured, not
    /// connected. That is the row the Bluetooth PTT target list is built from,
    /// and the row the first version could not see.
    #[test]
    fn presence_carries_what_the_server_has_configured() {
        let mut state = RadioState::default();
        assert!(!state.yaesu_configured && !state.yaesu2_configured);
        apply_yaesu_presence(&mut state, &YaesuPresencePacket {
            slot0_present: false, slot0_model: 0, slot1_present: false, slot1_model: 1,
            slot0_trouble: 0, slot1_trouble: 0, slot0_configured: true, slot1_configured: true, rx_levels_tuned: false,
        });
        assert!(state.yaesu_configured && state.yaesu2_configured, "both configured");
        assert!(!state.yaesu_connected && !state.yaesu2_connected, "neither connected");
        // The server loses slot 1 from its configuration: the flag follows.
        apply_yaesu_presence(&mut state, &YaesuPresencePacket {
            slot0_present: true, slot0_model: 0, slot1_present: false, slot1_model: 1,
            slot0_trouble: 0, slot1_trouble: 0, slot0_configured: true, slot1_configured: false, rx_levels_tuned: false,
        });
        assert!(state.yaesu_configured && !state.yaesu2_configured);
    }
}
