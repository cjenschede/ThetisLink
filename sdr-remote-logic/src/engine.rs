// SPDX-License-Identifier: GPL-2.0-or-later

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use log::{info, warn, debug};
use tokio::net::{ToSocketAddrs, UdpSocket};
use tokio::sync::{mpsc, watch, Mutex as AsyncMutex};
use tokio::time::{interval, Duration};

use sdr_remote_core::codec::OpusEncoderWideband;
use sdr_remote_core::jitter::{BufferedFrame, JitterBuffer, JitterResult};
use sdr_remote_core::protocol::*;
use sdr_remote_core::{FRAME_SAMPLES, FRAME_SAMPLES_WIDEBAND, MAX_PACKET_SIZE, NETWORK_SAMPLE_RATE, NETWORK_SAMPLE_RATE_WIDEBAND};

use crate::audio::AudioBackend;
use crate::commands::Command;
use crate::rx_stream::{channel_opus, recover_or_conceal, Decoded, RxStream};
use crate::state::RadioState;

/// Phase C: channels that connect the client engine to the relay monitor when the
/// client connects via the relay instead of direct-UDP. The client (`sdr-remote-client`)
/// wires these to a `RelayMonitor` with tunnel; the engine only knows the mpsc channels.
pub struct ClientRelayTunnel {
    /// Engine -> monitor: TL frames to send (the address is the server address, ignored by the relay).
    pub uplink_tx: mpsc::UnboundedSender<(SocketAddr, Vec<u8>)>,
    /// Monitor -> engine: received (decoded) TL frames.
    pub inbound_rx: mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)>,
    /// The server address; `recv` tags incoming frames with this, just like direct-UDP.
    pub server_addr: SocketAddr,
}

/// Client transport: direct-UDP (default, byte-identical) or via the relay tunnel.
/// Mimics the `UdpSocket` API surface that the engine uses (`send_to`,
/// `recv_from`, `local_addr`), so all existing call-sites remain unchanged.
enum ClientTransport {
    Direct(UdpSocket),
    Relay {
        uplink_tx: mpsc::UnboundedSender<(SocketAddr, Vec<u8>)>,
        inbound_rx: AsyncMutex<mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)>>,
        server_addr: SocketAddr,
    },
}

impl ClientTransport {
    async fn send_to<A: ToSocketAddrs>(&self, buf: &[u8], addr: A) -> std::io::Result<usize> {
        match self {
            ClientTransport::Direct(s) => s.send_to(buf, addr).await,
            ClientTransport::Relay {
                uplink_tx,
                server_addr,
                ..
            } => {
                // The relay knows the destination; the addr argument (server) is ignored.
                let _ = uplink_tx.send((*server_addr, buf.to_vec()));
                Ok(buf.len())
            }
        }
    }

    async fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        match self {
            ClientTransport::Direct(s) => s.recv_from(buf).await,
            ClientTransport::Relay {
                inbound_rx,
                server_addr,
                ..
            } => {
                let mut rx = inbound_rx.lock().await;
                match rx.recv().await {
                    Some((_addr, data)) => {
                        let n = data.len().min(buf.len());
                        buf[..n].copy_from_slice(&data[..n]);
                        Ok((n, *server_addr))
                    }
                    None => Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "relay inbound channel closed",
                    )),
                }
            }
        }
    }

    /// Unused today. Part of the deliberate mirror of `UdpSocket`'s surface
    /// described above - a wrapper missing one method of the API it stands in
    /// for is a trap for the next call site, not a saving.
    #[allow(dead_code)]
    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        match self {
            ClientTransport::Direct(s) => s.local_addr(),
            ClientTransport::Relay { .. } => Ok(SocketAddr::from(([0, 0, 0, 0], 0))),
        }
    }

    /// True in relay mode. The Connect handler then skips the direct-IP DNS/parse check:
    /// in relay mode there is no direct server IP (the route is url/station/token),
    /// and the Connect address is purely a display label that the Relay transport ignores.
    fn is_relay(&self) -> bool {
        matches!(self, ClientTransport::Relay { .. })
    }
}

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

/// PTT burst count - send this many packets on PTT state change
const PTT_BURST_COUNT: u32 = 5;
/// Heartbeat interval
const HEARTBEAT_INTERVAL_MS: u64 = 500;

/// Minimum connection timeout in ms (dynamic: max(this, rtt*8))
const CONNECTION_TIMEOUT_MIN_MS: u64 = 6000;

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

// --- TX Compressor (speech compressor for the Yaesu USB-TX chain) ---
// The radio's own processor does not work on USB audio; this client-side compressor
// fills that gap. `amount` 0.0..1.0 (0=off): compresses peaks above a threshold
// (ratio) and applies makeup gain → more density/punch.
struct TxCompressor {
    amount: f32,
    env: f32,
}

impl TxCompressor {
    fn new() -> Self { Self { amount: 0.0, env: 0.0 } }

    fn set_amount(&mut self, a: f32) { self.amount = a.clamp(0.0, 1.0); }

    fn process(&mut self, samples: &mut [f32]) {
        if self.amount <= 0.001 { return; }
        // Threshold drops, ratio + makeup rise with amount.
        let threshold = 0.30 - 0.22 * self.amount; // 0.30 → 0.08
        let ratio = 1.0 + 5.0 * self.amount;        // 1 → 6
        let makeup = 1.0 + 1.5 * self.amount;       // 1.0 → 2.5
        let exp = 1.0 / ratio - 1.0;                // negative exponent → gain reduction
        for s in samples.iter_mut() {
            let a = s.abs();
            let coeff = if a > self.env { 0.30 } else { 0.02 }; // attack / release
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
}

impl ClientEngine {
    pub fn new() -> (Self, watch::Receiver<RadioState>, mpsc::UnboundedSender<Command>) {
        let (state_tx, state_rx) = watch::channel(RadioState::default());
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        (Self { state_tx, cmd_rx }, state_rx, cmd_tx)
    }

    /// Start the engine with a platform-specific audio backend factory.
    /// The factory is called once at start and again for audio error recovery.
    /// Blocks until shutdown signal.
    pub async fn run(
        mut self,
        audio_factory: impl Fn(Option<&str>, Option<&str>) -> Result<Box<dyn AudioBackend>>,
        mut shutdown: watch::Receiver<bool>,
        relay_tunnel: Option<ClientRelayTunnel>,
    ) -> Result<()> {
        // Client transport: direct-UDP (default, byte-identical) or via the relay
        // tunnel when the client cannot port-forward. Direct binds a UDP socket with
        // a large recv buffer to prevent packet loss from spectrum packets (4-8KB
        // each) filling the default 8KB Windows buffer.
        let transport = match relay_tunnel {
            None => {
                let udp = UdpSocket::bind("0.0.0.0:0").await.context("bind client socket")?;
                {
                    use socket2::SockRef;
                    let sock_ref = SockRef::from(&udp);
                    let _ = sock_ref.set_recv_buffer_size(2 * 1024 * 1024);
                    let _ = sock_ref.set_send_buffer_size(512 * 1024);
                    let recv = sock_ref.recv_buffer_size().unwrap_or(0);
                    let send = sock_ref.send_buffer_size().unwrap_or(0);
                    info!("Client UDP bound to {} (recv_buf={}KB, send_buf={}KB)",
                        udp.local_addr()?, recv / 1024, send / 1024);
                }
                ClientTransport::Direct(udp)
            }
            Some(t) => {
                info!("Client transport: relay tunnel (server {})", t.server_addr);
                ClientTransport::Relay {
                    uplink_tx: t.uplink_tx,
                    inbound_rx: AsyncMutex::new(t.inbound_rx),
                    server_addr: t.server_addr,
                }
            }
        };

        let socket = Arc::new(transport);
        let start = Instant::now();

        // Audio setup - use defaults initially, can be reconfigured via commands
        let mut audio: Box<dyn AudioBackend> = audio_factory(None, None)?;
        let mut capture_rate = audio.capture_sample_rate();
        let mut playback_rate = audio.playback_sample_rate();

        let mut capture_frame_samples = (capture_rate * 20 / 1000) as usize;

        info!(
            "Client resamplers: capture {}Hz ({}smp/frame), playback {}Hz",
            capture_rate, capture_frame_samples, playback_rate
        );

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
        // Dual-radio slot 1 (PATCH-dual-radio-991a-ftx1) — own independent
        // channel, exact mirror of slot 0.
        let mut st_yaesu2 = RxStream::new(playback_rate, "Yaesu2")?;
        let mut yaesu2_jitter_buf = JitterBuffer::new(3, 40);
        let mut yaesu2_logged_first = false;

        // Yaesu TX: wideband Opus (16kHz) for USB output
        let mut yaesu_tx_sequence: u32 = 0;
        let mut yaesu_tx_accum: Vec<f32> = Vec::new();
        let mut yaesu_tx_encoder = OpusEncoderWideband::new()?;
        let mut yaesu_tx_bitrate_bps: i32 = 24_000;
        // Anti-alias filter: sinc_len 128 + f_cutoff 0.95 (identical to the
        // server-side Yaesu TX resampler). The short filter (sinc_len 32)
        // cut NT-USB content >8 kHz insufficiently, causing those
        // frequencies to alias back into 0-8 kHz during the 48→16 kHz decimation
        // and be audible on the RF output as "weird sounding" high tones
        // (operator finding 2026-06-02). ~4 ms extra
        // filter delay — negligible for the mic→Yaesu path.
        let mut yaesu_tx_resampler = rubato::SincFixedIn::<f32>::new(
            NETWORK_SAMPLE_RATE_WIDEBAND as f64 / capture_rate as f64, 1.0,
            rubato::SincInterpolationParameters {
                sinc_len: 128, f_cutoff: 0.95, oversampling_factor: 128,
                interpolation: rubato::SincInterpolationType::Cubic,
                window: rubato::WindowFunction::Blackman,
            },
            capture_frame_samples, 1,
        ).context("create Yaesu TX resampler")?;

        // Jitter buffer for received audio (lower min for LAN, adaptive handles internet)
        let mut jitter_buf = JitterBuffer::new(3, 40);

        // Per-channel resamplers: low-latency sinc (short filter = ~20ms group delay)
        let mk_sinc = || rubato::SincInterpolationParameters {
            sinc_len: 32, f_cutoff: 0.90, oversampling_factor: 32,
            interpolation: rubato::SincInterpolationType::Cubic,
            window: rubato::WindowFunction::Blackman,
        };
        // Dedicated WAV playback resamplers. Do not share RX1 live resampler state;
        // recordings played through the Server tab must sound like the file on disk.
        let mut wav_res_out = rubato::SincFixedIn::<f32>::new(
            playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mk_sinc(), FRAME_SAMPLES, 1,
        ).context("WAV 8k->device resampler")?;
        let mut wav_res_out_wb = rubato::SincFixedIn::<f32>::new(
            playback_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64, 1.0, mk_sinc(), FRAME_SAMPLES_WIDEBAND, 1,
        ).context("WAV 16k->device resampler")?;

        // VRX1 + VRX2 — each is a separate jitter buf plus its own RxStream.
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
        let mut vrx2_volume: f32 = 0.0; // start muted — see vrx1_volume

        // Anti-alias parameters for the TX-capture decimation 48 → 16 kHz.
        // Since build 29: identical to yaesu_tx_resampler — wide USB mics
        // (NT-USB and the like) have content up to 16 kHz that otherwise aliases
        // back into 0-8 kHz and becomes audible on the ANAN/Thetis TX output
        // during FM/AM-TX (SSB stays within 3 kHz so it is subtler).
        // The comment name (`device->8k`) stays for historical reasons; the path does
        // encode to 16 kHz wideband Opus (see `OpusEncoderWideband`).
        let sinc_params_in = rubato::SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            oversampling_factor: 128,
            interpolation: rubato::SincInterpolationType::Cubic,
            window: rubato::WindowFunction::Blackman,
        };
        let mut resampler_in = rubato::SincFixedIn::<f32>::new(
            NETWORK_SAMPLE_RATE_WIDEBAND as f64 / capture_rate as f64,
            1.0,
            sinc_params_in,
            capture_frame_samples,
            1,
        )
        .context("create device->16k resampler")?;

        // State
        let mut state = RadioState::default();
        let mut server_addr: Option<String> = None;
        let mut auth_password: Option<String> = None;
        let mut _auth_completed = false;
        // PATCH-1: track when Connect was issued + whether we've ever seen
        // any reply from the server, so we can surface NoUdpResponse after
        // a timeout, and distinguish "never heard anything" from "got bad bytes".
        let mut connect_started_at: Option<Instant> = None;
        let mut connect_timeout_secs: u32 = 5;
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
        let mut tx_sequence: u32 = 0;
        let mut hb_sequence: u32 = 0;
        let mut ptt = false;
        let mut thetis_ptt = false;
        let mut yaesu_ptt = false;
        // Slot-1 PTT (dual-radio). Mutually exclusive with yaesu_ptt in practice
        // (one mic) → the mic-TX chain picks the packet type based on which is active.
        let mut yaesu2_ptt = false;
        let mut last_ptt = false;
        let mut ptt_burst_remaining: u32 = 0;
        let mut mic_gate_delay_ms: u32 = 0;
        let mut last_capture_ptt = false;
        let mut last_hb_sent = Instant::now();
        let mut last_hb_ack_time: Option<Instant> = None;
        // While RX1 is being concealed: when it started, the first peak, how many
        // frames, and when it was last said out loud.
        let mut conceal_since: Option<Instant> = None;
        let mut conceal_first_peak: f32 = 0.0;
        let mut conceal_frames: u32 = 0;
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
        let mut yaesu2_tx_agc_enabled = false;
        let mut yaesu2_compressor = TxCompressor::new();
        let mut agc_enabled = false;
        let mut rx2_volume: f32 = 0.2;     // Thetis ZZLB sync + RX2 audio gain
        let mut vfo_b_volume: f32 = 1.0;   // Additional client-only RX2 gain (VFO B Vol slider)
        let mut audio_mode: u16 = 0;       // 0=Mono, 1=BIN, 2=Split
        let mut smeter_source: u8 = 1;     // 0=Sig, 1=Avg (default), 2=MaxBin
        // Track last Binaural ControlPacket value sent on PTT-side-effect path.
        // Avoids spamming the server with redundant rx_bin_enable cmds when the
        // PTT-state hasn't actually flipped (alpha-5 testlog: 38k events/session).
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
            res_nb: rubato::SincFixedIn<f32>,
            res_wb: rubato::SincFixedIn<f32>,
        }
        let mut playback_extra: Vec<ExtraPlayback> = Vec::new();
        // True as long as we have turned off the Thetis TXEQ for a WAV-playback to the
        // main radio (to restore on stop/PTT-release/end).
        let mut thetis_txeq_bypassed: bool = false;

        let mut yaesu_volume: f32 = 0.5;   // Yaesu audio volume (client-only)
        // Slot-1 volume starts MUTED (0.0) — required per lesson from build 88
        // (VRX peak at startup, project_audio_stutter_diagnose): each new
        // audio channel starts muted until the UI/effective volume has arrived.
        let mut yaesu2_volume: f32 = 0.0;
        let mut yaesu_local_mic_gain: f32 = 0.2; // Local Yaesu mic gain (before Opus encoding)
        let mut yaesu_eq = crate::eq::Equalizer::new(48000.0); // EQ at capture rate
        // Slot-1 (FTX-1) own TX-mic EQ + gain — applied when transmitting on radio 2
        // (PTT mutually exclusive, so chosen per PTT in the encode chain).
        let mut yaesu2_local_mic_gain: f32 = 0.2;
        let mut yaesu2_eq = crate::eq::Equalizer::new(48000.0);
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

        // Pending frequency: prevents stale server CAT values from overwriting local changes
        let mut pending_freq: Option<u64> = None;
        let mut pending_freq_time: Option<Instant> = None;
        let mut pending_freq_rx2: Option<u64> = None;
        let mut pending_freq_rx2_time: Option<Instant> = None;

        // Suppress server power broadcasts after sending a power command
        let mut power_suppress_until = Instant::now();

        // Packet loss tracking (rolling window per heartbeat interval)
        let mut loss_window_received: u32 = 0;
        let mut loss_window_max_seq: Option<u32> = None;
        let mut loss_prev_max_seq: Option<u32> = None;
        let mut current_loss_percent: u8 = 0;
        let mut smoothed_loss: f32 = 0.0;

        // Bandwidth monitor (down/up Kbit/s) over a rolling ~500 ms window.
        // RX bytes are summed on every recv_from(); TX bytes via the
        // `send_tx!` macro that wraps every send_to call-site. On every
        // window rollover the kbps is computed and written to `state.down_kbps`/
        // `up_kbps` — shown in the Server-tab Statistics grid.
        let mut bw_window_start = Instant::now();
        let mut bw_rx_bytes: u64 = 0;
        let mut bw_tx_bytes: u64 = 0;
        // Per-PacketType byte counter for the RX stream — indexed on
        // the `packet_type` byte (data[2]). Every 5 s a top-5
        // overview is logged to info! so the operator can see which
        // stream consumes the most (without a UI extension).
        let mut bw_by_type: [u64; 256] = [0; 256];
        let mut bw_breakdown_start = Instant::now();
        // Local macro: wraps socket.send_to(buf, addr).await and adds buf bytes
        // to bw_tx_bytes. Replaces the 80+ inline call-sites in this function
        // without per-site instrumentation. Identifiers `socket` and `bw_tx_bytes`
        // are resolved in the current scope at invocation.
        macro_rules! send_tx {
            ($buf:expr, $addr:expr) => {{
                let __buf: &[u8] = $buf;
                bw_tx_bytes = bw_tx_bytes.wrapping_add(__buf.len() as u64);
                socket.send_to(__buf, $addr).await
            }};
        }

        // Track last audio packet arrival for robust timeout detection
        let mut last_audio_received: Option<Instant> = None;

        // Audio error recovery
        let mut audio_error_since: Option<Instant> = None;
        let mut audio_retry_interval_ms: u64 = 1000;

        // Input/output device names for reconnect
        let mut input_device_name = String::new();
        let mut output_device_name = String::new();

        let mut recv_buf = vec![0u8; MAX_PACKET_SIZE];
        let mut drain_buf = vec![0.0f32; RING_DRAIN_SIZE];
        let mut accum_buf = Vec::<f32>::with_capacity(capture_frame_samples * 2);
        let mut read_buf = vec![0.0f32; RING_DRAIN_SIZE];

        let mut audio_tick = interval(Duration::from_millis(20));
        let mut last_server_addr: Option<String> = None;

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
                    let mksp = || rubato::SincInterpolationParameters {
                        sinc_len: 32, f_cutoff: 0.90, oversampling_factor: 32,
                        interpolation: rubato::SincInterpolationType::Cubic,
                        window: rubato::WindowFunction::Blackman,
                    };
                    // Yaesu TX needs a sharper anti-alias filter than the RX resamplers.
                    let mksp_aa = || rubato::SincInterpolationParameters {
                        sinc_len: 128, f_cutoff: 0.95, oversampling_factor: 128,
                        interpolation: rubato::SincInterpolationType::Cubic,
                        window: rubato::WindowFunction::Blackman,
                    };
                    if let Ok(r) = rubato::SincFixedIn::new(NETWORK_SAMPLE_RATE_WIDEBAND as f64 / capture_rate as f64, 1.0, mksp_aa(), capture_frame_samples, 1) { resampler_in = r; }
                    if let Ok(r) = rubato::SincFixedIn::new(NETWORK_SAMPLE_RATE_WIDEBAND as f64 / capture_rate as f64, 1.0, mksp_aa(), capture_frame_samples, 1) { yaesu_tx_resampler = r; }
                    if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mksp(), FRAME_SAMPLES, 1) { wav_res_out = r; }
                    if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64, 1.0, mksp(), FRAME_SAMPLES_WIDEBAND, 1) { wav_res_out_wb = r; }
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
                    info!("Resamplers rebuilt: capture {}Hz, playback {}Hz", capture_rate, playback_rate);
                }
                // Reset the jitter buffers to prevent stale frame buildup across the switch.
                jitter_buf.reset();
                yaesu_jitter_buf.reset();
                // Same reason, same omission: what the old rate left in these is
                // no more use than what it left in the others.
                vrx1_jitter_buf.reset();
                vrx2_jitter_buf.reset();
            }};
        }

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
            while let Ok(cmd) = self.cmd_rx.try_recv() {
                match cmd {
                    Command::Connect(addr, pw) => {
                        // PATCH-1 smoke-test follow-up (2026-05-12): if we are already in
                        // a forward-progress connect-state (AwaitingTotp or Connected) and
                        // the user clicks Connect again with the SAME server+password, do
                        // not regress the status — the server's session is still alive
                        // server-side and a Connecting-status would never recover (server
                        // won't re-issue AuthChallenge for an existing session). The user
                        // must explicitly Disconnect first if they want to start over.
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

                        // PATCH-1 smoke-test follow-up (2026-05-12 #2): if we have an
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
                        }

                        // PATCH-1 review finding (B1, part 1): up-front DNS / parse check.
                        // If the address is a plain "IP:port" it parses synchronously — no DNS
                        // needed. If it has a hostname, try lookup_host once. Either failure
                        // mode produces a specific ConnectError so the UI can show a precise
                        // message instead of a generic "Disconnected".
                        // In relay mode there is no direct server IP: the address is a
                        // display label (ignored by the Relay transport). Skip the
                        // direct-IP DNS/parse check in that case (security addendum fix).
                        let resolved_ok = if socket.is_relay() || addr.parse::<std::net::SocketAddr>().is_ok() {
                            true
                        } else {
                            // Async DNS lookup with a tight timeout — don't block the
                            // command-processing loop forever on a slow resolver.
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(5),
                                tokio::net::lookup_host(addr.as_str()),
                            )
                            .await
                            {
                                Ok(Ok(mut iter)) => iter.next().is_some(),
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
                            server_addr = Some(addr);
                            auth_password = pw;
                            _auth_completed = false;
                            connect_started_at = Some(Instant::now());
                            connect_any_reply_seen = false;
                            // PATCH-1: signal "Connecting" so the UI can show progress.
                            // Specific failure modes (NoUdpResponse via timeout, MalformedResponse
                            // via parser, ProtocolVersionMismatch via magic+version check) are
                            // surfaced from the network paths below.
                            state.connect_status =
                                crate::state::ConnectStatus::Connecting;
                            // Operator-smoke-test fix (2026-05-13): broadcast immediately so the
                            // UI clears the previous Failed(WrongPassword/...) banner the
                            // moment Connect is pressed — without this the user keeps seeing
                            // "Wrong password" for several seconds until the next packet
                            // event triggers a state-broadcast.
                            let _ = self.state_tx.send(state.clone());
                        } else {
                            // DNS-resolution already set the Failed status above; leave the
                            // password unset so a retry forces a fresh attempt.
                            server_addr = None;
                            auth_password = None;
                            connect_started_at = None;
                            // Same reasoning as above — the DNS-fail Failed state must
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
                        }
                    }
                    Command::Disconnect => {
                        // Send disconnect to server before clearing
                        if let Some(ref addr) = server_addr {
                            // Restore Thetis TXEQ if a WAV-playback left it bypassed —
                            // must happen while the server address is still valid.
                            if thetis_txeq_bypassed {
                                let ctrl = ControlPacket {
                                    control_id: ControlId::ThetisTxeq,
                                    value: 1,
                                };
                                let mut cbuf = [0u8; ControlPacket::SIZE];
                                ctrl.serialize(&mut cbuf);
                                let _ = send_tx!(&cbuf, addr.as_str());
                                thetis_txeq_bypassed = false;
                                info!("Thetis TXEQ restored (disconnect)");
                            }
                            let mut buf = [0u8; DisconnectPacket::SIZE];
                            DisconnectPacket::serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                            info!("Disconnect (ring={}, jbuf={}, jitter={:.1}ms, rtt={}ms, loss={}%)",
                                audio.playback_buffer_level(), jitter_buf.depth(),
                                jitter_buf.jitter_ms(), last_hb_ack_rtt, current_loss_percent);
                        }
                        server_addr = None;
                        jitter_buf.reset();
                        // Re-baseline the other audio streams too, so Yaesu/VRX don't
                        // stall on a stale next_seq after reconnect — explicit here
                        // instead of relying only on the backjump re-baseline.
                        yaesu_jitter_buf.reset();
                        yaesu2_jitter_buf.reset();
                        vrx1_jitter_buf.reset();
                        vrx2_jitter_buf.reset();
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
                        state.connected = false;
                        state.connect_status = crate::state::ConnectStatus::Disconnected;
                        state.rtt_ms = 0;
                        state.jitter_ms = 0.0;
                        state.buffer_depth = 0;
                        state.rx_packets = 0;
                        state.down_kbps = 0;
                        state.up_kbps = 0;
                        state.bw_breakdown.clear();
                        state.ptt_denied = false;
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
                                info!("Roger beep: Thetis, {} Hz for {} ms - PTT held until it has gone",
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
                        ptt = thetis_ptt;
                        if !v {
                            state.ptt_denied = false;
                        }
                        // Thetis BIN has a side-effect on TX audio quality.
                        // Disable BIN during TX, re-enable on RX if audio_mode=BIN.
                        if audio_mode == 1 {
                            if let Some(ref addr) = server_addr {
                                let bin_val = if v { 0u16 } else { 1u16 }; // TX: off, RX: on
                                if last_sent_bin != Some(bin_val) {
                                    let ctrl = ControlPacket {
                                        control_id: ControlId::Binaural,
                                        value: bin_val,
                                    };
                                    let mut buf = [0u8; ControlPacket::SIZE];
                                    ctrl.serialize(&mut buf);
                                    let _ = send_tx!(&buf, addr.as_str());
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
                        tx_gain = v;
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
                    Command::SetControl(id, value) => {
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket { control_id: id, value };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
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
                                    audio = new_audio;
                                    audio.set_capture_gate_delay_ms(mic_gate_delay_ms);
                                    resync_audio_rates!();
                                    info!("Audio input device switched to {:?}", in_name.unwrap_or("(default)"));
                                    state.audio_error = false;
                                    audio_error_since = None;
                                }
                                Err(e) => {
                                    warn!("Failed to switch audio input device: {}", e);
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
                                    audio = new_audio;
                                    audio.set_capture_gate_delay_ms(mic_gate_delay_ms);
                                    resync_audio_rates!();
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
                        spectrum_enabled = enabled;
                        if let Some(ref addr) = server_addr {
                            if was_connected {
                                let ctrl = ControlPacket {
                                    control_id: ControlId::SpectrumEnable,
                                    value: enabled as u16,
                                };
                                let mut buf = [0u8; ControlPacket::SIZE];
                                ctrl.serialize(&mut buf);
                                let _ = send_tx!(&buf, addr.as_str());
                            }
                        }
                    }
                    Command::SetSpectrumFps(fps) => {
                        spectrum_fps = fps;
                        if let Some(ref addr) = server_addr {
                            if was_connected {
                                let ctrl = ControlPacket {
                                    control_id: ControlId::SpectrumFps,
                                    value: fps as u16,
                                };
                                let mut buf = [0u8; ControlPacket::SIZE];
                                ctrl.serialize(&mut buf);
                                let _ = send_tx!(&buf, addr.as_str());
                            }
                        }
                    }
                    Command::SetSpectrumZoom(zoom) => {
                        spectrum_zoom = zoom;
                        if let Some(ref addr) = server_addr {
                            if was_connected {
                                let ctrl = ControlPacket {
                                    control_id: ControlId::SpectrumZoom,
                                    value: (zoom * 10.0) as u16,
                                };
                                let mut buf = [0u8; ControlPacket::SIZE];
                                ctrl.serialize(&mut buf);
                                let _ = send_tx!(&buf, addr.as_str());
                            }
                        }
                    }
                    Command::SetSpectrumPan(pan) => {
                        spectrum_pan = pan;
                        // Said on this side too, so a report shows both ends of the
                        // same value: sent here, received there. A pan that moves the
                        // scale but not the trace is one of those two going missing,
                        // and until now neither end wrote it down.
                        log::info!("Spectrum pan sent: {:+.4}", pan);
                        if let Some(ref addr) = server_addr {
                            if was_connected {
                                let ctrl = ControlPacket {
                                    control_id: ControlId::SpectrumPan,
                                    value: ((pan + 0.5) * 10000.0) as u16,
                                };
                                let mut buf = [0u8; ControlPacket::SIZE];
                                ctrl.serialize(&mut buf);
                                let _ = send_tx!(&buf, addr.as_str());
                            }
                        }
                    }
                    Command::SetSpectrumMaxBins(max_bins) => {
                        spectrum_max_bins = max_bins;
                        if let Some(ref addr) = server_addr {
                            if was_connected {
                                let ctrl = ControlPacket {
                                    control_id: ControlId::SpectrumMaxBins,
                                    value: max_bins,
                                };
                                let mut buf = [0u8; ControlPacket::SIZE];
                                ctrl.serialize(&mut buf);
                                let _ = send_tx!(&buf, addr.as_str());
                            }
                        }
                    }
                    Command::SetSpectrumFftSize(size_k) => {
                        spectrum_fft_size_k = size_k;
                        if let Some(ref addr) = server_addr {
                            if was_connected {
                                let ctrl = ControlPacket {
                                    control_id: ControlId::SpectrumFftSize,
                                    value: size_k,
                                };
                                let mut buf = [0u8; ControlPacket::SIZE];
                                ctrl.serialize(&mut buf);
                                let _ = send_tx!(&buf, addr.as_str());
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
                    Command::SetAmplitecPowerTable { max_w, tx_blocked } => {
                        if let Some(ref addr) = server_addr {
                            let mut data = Vec::with_capacity(18);
                            for i in 0..6 {
                                data.extend_from_slice(&max_w[i].to_be_bytes());
                                data.push(tx_blocked[i] as u8);
                            }
                            let pkt = EquipmentCommandPacket {
                                device_type: DeviceType::Amplitec6x2,
                                command_id: sdr_remote_core::protocol::CMD_AMPLITEC_SET_POWER_TABLE,
                                data,
                            };
                            let mut buf = Vec::with_capacity(EquipmentCommandPacket::MIN_SIZE + 18);
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
                    Command::Rf2kSetDriveConfig { category, band, value } => {
                        if let Some(ref addr) = server_addr {
                            let cmd_id = match category {
                                0 => CMD_RF2K_SET_DRIVE_SSB,
                                1 => CMD_RF2K_SET_DRIVE_AM,
                                _ => CMD_RF2K_SET_DRIVE_CONT,
                            };
                            let pkt = EquipmentCommandPacket {
                                device_type: DeviceType::Rf2k,
                                command_id: cmd_id,
                                data: vec![band, value],
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
                            let ctrl = ControlPacket { control_id: ControlId::SmeterSources, value: mask };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
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
                                    let mksp = || rubato::SincInterpolationParameters {
                                        sinc_len: 128, f_cutoff: 0.95, oversampling_factor: 128,
                                        interpolation: rubato::SincInterpolationType::Cubic,
                                        window: rubato::WindowFunction::Blackman,
                                    };
                                    let nb = rubato::SincFixedIn::<f32>::new(
                                        playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mksp(), FRAME_SAMPLES, 1);
                                    let wb = rubato::SincFixedIn::<f32>::new(
                                        playback_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64, 1.0, mksp(), FRAME_SAMPLES_WIDEBAND, 1);
                                    match (nb, wb) {
                                        (Ok(res_nb), Ok(res_wb)) => {
                                            info!("Playback: also playing {} ({:.1}s, {} Hz)",
                                                extra, samples.len() as f32 / rate.max(1) as f32, rate);
                                            playback_extra.push(ExtraPlayback { samples, rate, pos: 0, res_nb, res_wb });
                                        }
                                        _ => warn!("Playback: no resampler for {} - left out", extra),
                                    }
                                }
                                Ok((other, _)) => warn!("Playback: {} has an unsupported rate ({} Hz) - left out", extra, other),
                                Err(e) => warn!("Playback: {} could not be read ({}) - left out", extra, e),
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
                                    playback_is_tx = ptt || yaesu_ptt || yaesu2_ptt;
                                    state.playing = true;
                                }
                                other => {
                                    // TL records only 8 k / 16 k. Reject other rates rather
                                    // than misplay them at the wrong speed.
                                    warn!("Playback: unsupported WAV sample rate {} Hz (only 8000/16000 Hz supported) - not played", other);
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
                            let ctrl = ControlPacket { control_id: ControlId::FullSpectrumEnabled, value: enabled as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                            info!("Full-spectrum row enable sent: {}", enabled);
                        }
                    }
                    Command::SetDxSpotsEnabled(enabled) => {
                        state.dx_spots_enabled = enabled;
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket { control_id: ControlId::DxSpotsEnabled, value: enabled as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
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
                            let ctrl = ControlPacket { control_id: ControlId::ThetisWidebandAudio, value: on as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                            info!("Thetis wideband audio sent: {}", on);
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
                            let ctrl = ControlPacket { control_id: ControlId::Rx1Enable, value: enabled as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                            info!("RX1 enable sent: {}", enabled);
                        }
                    }
                    // RX2 / VFO-B commands
                    Command::SetRx2Enabled(enabled) => {
                        if !enabled { st_rx2.reset(); }
                        state.rx2_enabled = enabled;
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket { control_id: ControlId::Rx2Enable, value: enabled as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                            info!("RX2 enable sent: {}", enabled);
                        }
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
                    Command::SetYaesuEqEnabled(on) => {
                        yaesu_eq.set_enabled(on);
                        info!("Yaesu EQ: {}", if on { "ON" } else { "OFF" });
                    }
                    Command::SetYaesuCompressor(level) => {
                        // Client-side speech compressor radio 1 (0-100 → 0.0-1.0).
                        yaesu_compressor.set_amount(level as f32 / 100.0);
                        info!("Yaesu compressor: {}", level);
                    }
                    Command::SetYaesuTxAgc(on) => {
                        yaesu_tx_agc_enabled = on;
                        info!("Yaesu TX AGC: {}", if on { "ON" } else { "OFF" });
                    }
                    Command::SetYaesu2Compressor(level) => {
                        // Client-side speech compressor radio 2 (FTX-1).
                        yaesu2_compressor.set_amount(level as f32 / 100.0);
                        info!("Yaesu2 compressor: {}", level);
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
                    Command::SetYaesuMenu(menu_num, p2_value) => {
                        if let Some(ref addr) = server_addr {
                            // Send menu data as YaesuMemoryData packet with "SETMENU:" prefix
                            let text = format!("SETMENU:{}:{}", menu_num, p2_value);
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
                            let ctrl = ControlPacket {
                                control_id: ControlId::YaesuWriteMemories, value: 0 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
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
                            let ctrl = ControlPacket {
                                control_id: ControlId::Yaesu2WriteMemories, value: 0 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::SetYaesuMode(mode) => {
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket { control_id: ControlId::YaesuMode, value: mode as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::SetYaesuControl(slot, control, value) => {
                        // Typed Yaesu DSP/function control (PATCH-yaesu-extra-controls).
                        if let Some(ref addr) = server_addr {
                            let pkt = YaesuControlPacket { slot, control, value };
                            let mut buf = [0u8; YaesuControlPacket::SIZE];
                            pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
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
                                info!("Roger beep: radio 1, {} Hz for {} ms - PTT held until it has gone",
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
                        // Send Yaesu PTT immediately; mic capture opens through
                        // the shared delayed gate below.
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket { control_id: ControlId::YaesuPtt, value: on as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::SetYaesuTxGain(v) => {
                        // Local Yaesu mic gain (applied before Opus encoding).
                        // UI display 0.5 maps to the empirically matched internal 0.2.
                        yaesu_local_mic_gain = v.clamp(0.02, 0.4);
                    }
                    // --- Dual-radio slot 1 commands (PATCH-dual-radio-991a-ftx1) ---
                    Command::SetYaesu2Enable(on) => {
                        // Outside the guard, same reason as VRX1 above.
                        if !on { st_yaesu2.reset(); }
                        if let Some(ref addr) = server_addr {
                            yaesu2_wanted = on;
                            let ctrl = ControlPacket { control_id: ControlId::Yaesu2Enable, value: on as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
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
                                info!("Roger beep: radio 2, {} Hz for {} ms - PTT held until it has gone",
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
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket { control_id: ControlId::Yaesu2Ptt, value: on as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::SetYaesu2Freq(hz) => {
                        if server_addr.is_some() {
                            deferred_yaesu2_freq = Some(hz);
                        }
                    }
                    Command::SetYaesu2Mode(mode) => {
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket { control_id: ControlId::Yaesu2Mode, value: mode as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::SetYaesu2TxGain(v) => {
                        // Own mic gain for radio 2 (applied when transmitting on slot 1).
                        yaesu2_local_mic_gain = v.clamp(0.02, 0.4);
                    }
                    Command::SetYaesu2EqBand(band, gain_db) => {
                        yaesu2_eq.set_band_gain(band as usize, gain_db);
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
                            let ctrl = ControlPacket { control_id: ControlId::VrxEnable, value: on as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
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
                            let ctrl = ControlPacket { control_id: ControlId::VrxMode, value: mode as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
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
                            let ctrl = ControlPacket { control_id: ControlId::VrxEnable2, value: on as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
vrx2_enable_at = if on { Some(Instant::now()) } else { None };
                                                // Same re-baseline as VRX1 - see there.
                        vrx2_jitter_buf.reset();
                        vrx2_logged_first = false;
                    }
                    Command::SetVrx2Mode(mode) => {
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket { control_id: ControlId::VrxMode2, value: mode as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
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
                            let ctrl = ControlPacket { control_id: ControlId::VrxAudioRate, value: mode as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::SetVrxRateMode2(mode) => {
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket { control_id: ControlId::VrxAudioRate2, value: mode as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::SetVrxAutoTune(vrx_id, on) => {
                        if let Some(ref addr) = server_addr {
                            let id = if vrx_id == 0 { ControlId::VrxSamAutoTune } else { ControlId::VrxSamAutoTune2 };
                            let ctrl = ControlPacket { control_id: id, value: on as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::SetVrxFilter(vrx_id, low_hz, high_hz) => {
                        if let Some(ref addr) = server_addr {
                            let (lo_id, hi_id) = if vrx_id == 0 {
                                (ControlId::VrxFilterLow, ControlId::VrxFilterHigh)
                            } else {
                                (ControlId::VrxFilterLow2, ControlId::VrxFilterHigh2)
                            };
                            let lo_pkt = ControlPacket { control_id: lo_id, value: low_hz as i16 as u16 };
                            let hi_pkt = ControlPacket { control_id: hi_id, value: high_hz as i16 as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            lo_pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                            hi_pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::SetTxFilter(low_hz, high_hz) => {
                        if let Some(ref addr) = server_addr {
                            let lo_pkt = ControlPacket { control_id: ControlId::TxFilterLow, value: low_hz as i16 as u16 };
                            let hi_pkt = ControlPacket { control_id: ControlId::TxFilterHigh, value: high_hz as i16 as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            lo_pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                            hi_pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::SetVrxSpectrumPan(vrx_id, offset_hz) => {
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket {
                                control_id: if vrx_id == 0 {
                                    ControlId::VrxSpectrumPan
                                } else {
                                    ControlId::VrxSpectrumPan2
                                },
                                value: sdr_remote_core::protocol::pack_vrx_pan(offset_hz),
                            };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::SetVrxHighResSpectrum(vrx_id, enabled, span_khz) => {
                        if let Some(ref addr) = server_addr {
                            let (en_id, span_id) = if vrx_id == 0 {
                                (ControlId::VrxSpectrumEnable, ControlId::VrxSpectrumSpanKhz)
                            } else {
                                (ControlId::VrxSpectrumEnable2, ControlId::VrxSpectrumSpanKhz2)
                            };
                            let en_pkt = ControlPacket { control_id: en_id, value: if enabled { 1 } else { 0 } };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            en_pkt.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                            if enabled && span_khz > 0 {
                                let span_pkt = ControlPacket { control_id: span_id, value: span_khz };
                                span_pkt.serialize(&mut buf);
                                let _ = send_tx!(&buf, addr.as_str());
                            }
                        }
                    }
                    Command::SetMonitor(on) => {
                        state.mon_on = on;
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket { control_id: ControlId::MonitorOn, value: on as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::ThetisTune(on) => {
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket { control_id: ControlId::ThetisTune, value: on as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::CwKey { pressed, duration_ms } => {
                        if let Some(ref addr) = server_addr {
                            let value = (pressed as u16) | (duration_ms << 1);
                            let ctrl = ControlPacket { control_id: ControlId::CwKey, value };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::CwMacroStop => {
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket { control_id: ControlId::CwMacroStop, value: 0 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::SetVfoSync(enabled) => {
                        state.vfo_sync = enabled;
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket { control_id: ControlId::VfoSync, value: enabled as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
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
                        state.rx2_spectrum_enabled = enabled;
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket { control_id: ControlId::Rx2SpectrumEnable, value: enabled as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                            info!("RX2 spectrum enable sent: {}", enabled);
                        }
                    }
                    Command::SetRx2SpectrumFps(fps) => {
                        if let Some(ref addr) = server_addr {
                            if was_connected {
                                let ctrl = ControlPacket { control_id: ControlId::Rx2SpectrumFps, value: fps as u16 };
                                let mut buf = [0u8; ControlPacket::SIZE];
                                ctrl.serialize(&mut buf);
                                let _ = send_tx!(&buf, addr.as_str());
                            }
                        }
                    }
                    Command::SetRx2SpectrumZoom(zoom) => {
                        rx2_spectrum_zoom = zoom;
                        if let Some(ref addr) = server_addr {
                            if was_connected {
                                let ctrl = ControlPacket { control_id: ControlId::Rx2SpectrumZoom, value: (zoom * 10.0) as u16 };
                                let mut buf = [0u8; ControlPacket::SIZE];
                                ctrl.serialize(&mut buf);
                                let _ = send_tx!(&buf, addr.as_str());
                            }
                        }
                    }
                    Command::SetRx2SpectrumPan(pan) => {
                        rx2_spectrum_pan = pan;
                        if let Some(ref addr) = server_addr {
                            if was_connected {
                                let ctrl = ControlPacket { control_id: ControlId::Rx2SpectrumPan, value: ((pan + 0.5) * 10000.0) as u16 };
                                let mut buf = [0u8; ControlPacket::SIZE];
                                ctrl.serialize(&mut buf);
                                let _ = send_tx!(&buf, addr.as_str());
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
                jitter_buf.reset();
                // Reset the other per-stream audio jitter buffers too. Previously
                // only the RX buffer was reset here, so after a server restart the
                // Yaesu buffers kept a stale (high) next_seq and dropped the fresh
                // low-sequence stream as "too late" — audio stayed silent for
                // minutes (until the server sequence climbed back) or until a manual
                // client reconnect. Re-baseline all streams on disconnect.
                yaesu_jitter_buf.reset();
                yaesu2_jitter_buf.reset();
                vrx1_jitter_buf.reset();
                vrx2_jitter_buf.reset();
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
                state.connected = false;
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

            tokio::select! {
                result = socket.recv_from(&mut recv_buf) => {
                    if server_addr.is_none() {
                        continue;
                    }

                    let (len, _addr) = match result {
                        Ok(r) => r,
                        Err(e) => {
                            warn!("recv_from error: {}", e);
                            continue;
                        }
                    };
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
                            loss_window_received += 1;
                            let seq = pkt.sequence;
                            loss_window_max_seq = Some(loss_window_max_seq.map_or(seq, |max| max.max(seq)));

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

                            // PATCH-1 review finding (B3): only trust state_flags
                            // when the server explicitly advertises REPORTS_STATE_FLAGS.
                            // Old servers (pre-PATCH-1, e.g. v2.0.0 release tag) leave
                            // both capabilities and state_flags at NONE — interpreting
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
                                // PATCH-1 operator-feedback (2026-05-13): suppress TciUnreachable
                                // while the server is in the launch phase (orange Start button).
                                // Showing "TCI not reachable" during the normal 60s startup
                                // grace period is wrong — the launch is still in progress.
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
                                    // middle of launching (transient — wait for the launch
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
                                                // Clear stale spectrum data on (re)connect
                                    state.spectrum_bins.clear();
                                    state.full_spectrum_bins.clear();
                                    state.spectrum_sequence = 0;
                                    state.full_spectrum_sequence = 0;
                                    // Send deferred spectrum settings now that server knows us
                                    if spectrum_enabled {
                                        let mut buf = [0u8; ControlPacket::SIZE];

                                        let ctrl = ControlPacket {
                                            control_id: ControlId::SpectrumEnable,
                                            value: 1,
                                        };
                                        ctrl.serialize(&mut buf);
                                        let _ = send_tx!(&buf, addr.as_str());

                                        let fps_ctrl = ControlPacket {
                                            control_id: ControlId::SpectrumFps,
                                            value: spectrum_fps as u16,
                                        };
                                        fps_ctrl.serialize(&mut buf);
                                        let _ = send_tx!(&buf, addr.as_str());

                                        // Re-send zoom and pan so server generates correct view
                                        let zoom_ctrl = ControlPacket {
                                            control_id: ControlId::SpectrumZoom,
                                            value: (spectrum_zoom * 10.0) as u16,
                                        };
                                        zoom_ctrl.serialize(&mut buf);
                                        let _ = send_tx!(&buf, addr.as_str());

                                        let pan_ctrl = ControlPacket {
                                            control_id: ControlId::SpectrumPan,
                                            value: ((spectrum_pan + 0.5) * 10000.0) as u16,
                                        };
                                        pan_ctrl.serialize(&mut buf);
                                        let _ = send_tx!(&buf, addr.as_str());

                                        let bins_ctrl = ControlPacket {
                                            control_id: ControlId::SpectrumMaxBins,
                                            value: spectrum_max_bins,
                                        };
                                        bins_ctrl.serialize(&mut buf);
                                        let _ = send_tx!(&buf, addr.as_str());

                                        if spectrum_fft_size_k != 0 {
                                            let fft_ctrl = ControlPacket {
                                                control_id: ControlId::SpectrumFftSize,
                                                value: spectrum_fft_size_k,
                                            };
                                            fft_ctrl.serialize(&mut buf);
                                            let _ = send_tx!(&buf, addr.as_str());
                                        }
                                    }

                                    // Re-send RX1 audio subscription on reconnect.
                                    // Server default = ON, so only relevant if
                                    // the client wants RX1 OFF — but always sending is
                                    // idempotent and covers both cases.
                                    {
                                        let mut rx1_buf = [0u8; ControlPacket::SIZE];
                                        let ctrl = ControlPacket {
                                            control_id: ControlId::Rx1Enable,
                                            value: state.rx1_enabled as u16,
                                        };
                                        ctrl.serialize(&mut rx1_buf);
                                        let _ = send_tx!(&rx1_buf, addr.as_str());
                                    }

                                    // Re-send RX2 AUDIO subscription on reconnect (separate from spectrum).
                                    if state.rx2_enabled {
                                        let mut rx2_buf = [0u8; ControlPacket::SIZE];
                                        let ctrl = ControlPacket { control_id: ControlId::Rx2Enable, value: 1 };
                                        ctrl.serialize(&mut rx2_buf);
                                        let _ = send_tx!(&rx2_buf, addr.as_str());
                                        info!("RX2 audio re-sent on reconnect");
                                    }
                                    // Re-send RX2 SPECTRUM subscription on reconnect, SEPARATE from the
                                    // audio subscription (phase 3b/4) — otherwise an RX2-spectrum-
                                    // without-audio client gets no spectrum after reconnecting.
                                    if state.rx2_spectrum_enabled {
                                        let mut rx2_buf = [0u8; ControlPacket::SIZE];
                                        let ctrl = ControlPacket { control_id: ControlId::Rx2SpectrumEnable, value: 1 };
                                        ctrl.serialize(&mut rx2_buf);
                                        let _ = send_tx!(&rx2_buf, addr.as_str());

                                        let bins_ctrl = ControlPacket { control_id: ControlId::Rx2SpectrumMaxBins, value: spectrum_max_bins };
                                        bins_ctrl.serialize(&mut rx2_buf);
                                        let _ = send_tx!(&rx2_buf, addr.as_str());

                                        let zoom_ctrl = ControlPacket { control_id: ControlId::Rx2SpectrumZoom, value: (rx2_spectrum_zoom * 10.0) as u16 };
                                        zoom_ctrl.serialize(&mut rx2_buf);
                                        let _ = send_tx!(&rx2_buf, addr.as_str());

                                        let pan_ctrl = ControlPacket { control_id: ControlId::Rx2SpectrumPan, value: ((rx2_spectrum_pan + 0.5) * 10000.0) as u16 };
                                        pan_ctrl.serialize(&mut rx2_buf);
                                        let _ = send_tx!(&rx2_buf, addr.as_str());

                                        if rx2_spectrum_fft_size_k != 0 {
                                            let fft_ctrl = ControlPacket { control_id: ControlId::Rx2SpectrumFftSize, value: rx2_spectrum_fft_size_k };
                                            fft_ctrl.serialize(&mut rx2_buf);
                                            let _ = send_tx!(&rx2_buf, addr.as_str());
                                        }
                                        info!("RX2 spectrum re-sent on reconnect");
                                    }
                                    // Send AudioMode so server knows our channel requirements
                                    let ctrl = ControlPacket { control_id: ControlId::AudioMode, value: audio_mode };
                                    let mut am_buf = [0u8; ControlPacket::SIZE];
                                    ctrl.serialize(&mut am_buf);
                                    let _ = send_tx!(&am_buf, addr.as_str());
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
                                    let ctrl = ControlPacket { control_id: ControlId::SmeterSources, value: mask };
                                    let mut sm_buf = [0u8; ControlPacket::SIZE];
                                    ctrl.serialize(&mut sm_buf);
                                    let _ = send_tx!(&sm_buf, addr.as_str());
                                    // Re-send DX-spots opt-out — server's ClientSession resets
                                    // to default ON on every new insert, so without this path
                                    // the client would visually show OFF while the server, after
                                    // reconnect, sends Spot frames again.
                                    let ctrl = ControlPacket {
                                        control_id: ControlId::DxSpotsEnabled,
                                        value: state.dx_spots_enabled as u16,
                                    };
                                    let mut dx_buf = [0u8; ControlPacket::SIZE];
                                    ctrl.serialize(&mut dx_buf);
                                    let _ = send_tx!(&dx_buf, addr.as_str());
                                    // Same reasoning for the full-spectrum opt-out.
                                    let ctrl = ControlPacket {
                                        control_id: ControlId::FullSpectrumEnabled,
                                        value: state.full_spectrum_enabled as u16,
                                    };
                                    let mut fs_buf = [0u8; ControlPacket::SIZE];
                                    ctrl.serialize(&mut fs_buf);
                                    let _ = send_tx!(&fs_buf, addr.as_str());
                                }
                                state.connected = true;
                                was_connected = true;
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
                            state.other_tx = sm_pkt.flags.ptt() && !ptt && !yaesu_ptt;
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
                                info!(
                                    "concealing ended after {:.1}s and {} frame(s), first peak {:.4}",
                                    started.elapsed().as_secs_f32(), conceal_frames, conceal_first_peak
                                );
                            }
                            if !logged_first_rx {
                                info!("RX: first multi-ch audio ({} channels, seq={})",
                                    pkt.channels.len(), pkt.sequence);
                                logged_first_rx = true;
                            }

                            last_audio_received = Some(Instant::now());
                            loss_window_received += 1;
                            let seq = pkt.sequence;
                            loss_window_max_seq = Some(loss_window_max_seq.map_or(seq, |max| max.max(seq)));

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
                        // source — the server only sends one source per RX
                        // unless the client subscribes to multiple.
                        Ok(Packet::SmeterSig(sm_pkt)) => {
                            let dbm = sm_pkt.level as f32 / 10.0;
                            state.smeter_sig = dbm;
                            state.smeter = dbm;
                            state.other_tx = sm_pkt.flags.ptt() && !ptt && !yaesu_ptt;
                        }
                        Ok(Packet::SmeterMaxBin(sm_pkt)) => {
                            let dbm = sm_pkt.level as f32 / 10.0;
                            state.smeter_peakbin = dbm;
                            state.smeter = dbm;
                            state.other_tx = sm_pkt.flags.ptt() && !ptt && !yaesu_ptt;
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
                                ControlId::SpectrumEnable | ControlId::SpectrumFps
                                | ControlId::SpectrumZoom | ControlId::SpectrumPan
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
                                | ControlId::Rx2SpectrumMaxBins
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
                                ControlId::Rx2SpectrumFftSize => {
                                    rx2_spectrum_fft_size_k = ctrl.value;
                                    // Also forward to server
                                    if let Some(ref addr) = server_addr {
                                        let mut buf = [0u8; ControlPacket::SIZE];
                                        ctrl.serialize(&mut buf);
                                        let _ = send_tx!(&buf, addr.as_str());
                                    }
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
                                        // Debug fields (Fase D) - parts[28..47]
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
                                        // Drive config (Fase D) - parts[44..46]
                                        if parts.len() >= 47 {
                                            fn parse_drive(s: &str) -> [u8; 11] {
                                                let mut arr = [0u8; 11];
                                                for (i, v) in s.split(';').enumerate().take(11) {
                                                    arr[i] = v.parse().unwrap_or(0);
                                                }
                                                arr
                                            }
                                            state.rf2k_drive_config_ssb = parse_drive(parts[44]);
                                            state.rf2k_drive_config_am = parse_drive(parts[45]);
                                            state.rf2k_drive_config_cont = parse_drive(parts[46]);
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
                            // connected no longer comes from here — presence authority is
                            // now YaesuPresence (PATCH-android-yaesu-presence-datasaver),
                            // so dropping/appearing is dynamic instead of sticky.
                            state.yaesu_freq_a = ys.freq_a;
                            state.yaesu_freq_b = ys.freq_b;
                            state.yaesu_mode = ys.mode;
                            state.yaesu_smeter = ys.smeter;
                            state.yaesu_tx_active = ys.tx_active;
                            state.yaesu_power_on = ys.power_on;
                            state.yaesu_af_gain = ys.af_gain;
                            state.yaesu_tx_power = ys.tx_power;
                            state.yaesu_tx_power_max = ys.tx_power_max;
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
                        Ok(Packet::YaesuFeature(pkt)) => {
                            // Yaesu DSP/function feature-state feedback (PATCH-yaesu-extra-controls).
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
                                    }
                                    if !vrx1_logged_first {
                                        debug!(
                                            "VRX1 audio: first packet received (seq={}, opus_bytes={}) - {} ms after enable",
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
                                    }
                                    if !vrx2_logged_first {
                                        debug!(
                                            "VRX2 audio: first packet received (seq={}, opus_bytes={}) - {} ms after enable",
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
                            // Audio arriving is a sign of life, and the
                            // timeout is an AND of two independent signals so
                            // that neither alone can drop a good link. Without
                            // this a Yaesu-only station (Thetis RX off, radio
                            // audio on - an arrangement the manual describes)
                            // has audio flowing while audio_timed_out stays
                            // true, leaving the connection hanging on the
                            // heartbeat alone.
                            last_audio_received = Some(Instant::now());
                            // Detect stream reset (server resets seq to 0 on re-enable)
                            if yaesu_logged_first && pkt.sequence == 0 {
                                info!("Yaesu: stream reset detected, resetting jitter buffer");
                                yaesu_jitter_buf.reset();
                                st_yaesu.reset();
                            }
                            if !yaesu_logged_first {
                                info!("Yaesu: first audio packet (seq={}, {}B)", pkt.sequence, pkt.opus_data.len());
                                yaesu_logged_first = true;
                            }
                            let arrival_ms = start.elapsed().as_millis() as u64;
                            yaesu_jitter_buf.push(
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
                            state.yaesu_audio_packets += 1;
                            state.yaesu_jitter_ms = yaesu_jitter_buf.jitter_ms();
                            state.yaesu_buffer_depth = yaesu_jitter_buf.depth() as u32;
                        }
                        Ok(Packet::PttDenied) => {
                            state.ptt_denied = true;
                        }
                        // Dual-radio slot 1 (Option B-prime) — exact mirror of slot 0.
                        Ok(Packet::YaesuState2(ys)) => {
                            // connected no longer comes from here — see YaesuPresence (3c).
                            state.yaesu2_freq_a = ys.freq_a;
                            state.yaesu2_freq_b = ys.freq_b;
                            state.yaesu2_mode = ys.mode;
                            state.yaesu2_smeter = ys.smeter;
                            state.yaesu2_tx_active = ys.tx_active;
                            state.yaesu2_power_on = ys.power_on;
                            state.yaesu2_af_gain = ys.af_gain;
                            state.yaesu2_tx_power = ys.tx_power;
                            state.yaesu2_tx_power_max = ys.tx_power_max;
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
                            // Audio arriving is a sign of life, and the
                            // timeout is an AND of two independent signals so
                            // that neither alone can drop a good link. Without
                            // this a Yaesu-only station (Thetis RX off, radio
                            // audio on - an arrangement the manual describes)
                            // has audio flowing while audio_timed_out stays
                            // true, leaving the connection hanging on the
                            // heartbeat alone.
                            last_audio_received = Some(Instant::now());
                            if yaesu2_logged_first && pkt.sequence == 0 {
                                info!("[radio1] stream reset detected, resetting jitter buffer");
                                yaesu2_jitter_buf.reset();
                                st_yaesu2.reset();
                            }
                            if !yaesu2_logged_first {
                                info!("[radio1] first audio packet (seq={}, {}B)", pkt.sequence, pkt.opus_data.len());
                                yaesu2_logged_first = true;
                            }
                            let arrival_ms = start.elapsed().as_millis() as u64;
                            yaesu2_jitter_buf.push(
                                BufferedFrame {
                                    sequence: pkt.sequence,
                                    timestamp: pkt.timestamp,
                                    opus_data: pkt.opus_data,
                                    ptt: false,
                                    wideband: pkt.flags.wideband(), // RX follows Thetis toggle
                                },
                                arrival_ms,
                            );
                            state.yaesu2_audio_packets += 1;
                            state.yaesu2_jitter_ms = yaesu2_jitter_buf.jitter_ms();
                            state.yaesu2_buffer_depth = yaesu2_jitter_buf.depth() as u32;
                        }
                        Ok(Packet::RadioInfo { slot, model }) => {
                            // Per-radio model for panel naming ("991A 1"/"FTX1").
                            if slot == 0 { state.yaesu_model = model; }
                            else if slot == 1 { state.yaesu2_model = model; }
                        }
                        Ok(Packet::YaesuPresence(p)) => {
                            // Presence authority (PATCH-android-yaesu-presence-datasaver):
                            // connected + model come from here (broadcast to all
                            // clients), NOT from the subscription-gated YaesuState. Pure
                            // application in apply_yaesu_presence (unit-tested).
                            let (c0, c1) = apply_yaesu_presence(&mut state, &p);
                            // Presence may have cleared the hi_swr flag; the alarm
                            // must follow that drop immediately.
                            audio.set_swr_alarm(state.yaesu_hi_swr || state.yaesu2_hi_swr);
                            // Value-change-only logging (L4) — shared, so desktop too.
                            if c0 { info!("[radio0] presence: {}", if p.slot0_present { "connected" } else { "disconnected" }); }
                            if c1 { info!("[radio1] presence: {}", if p.slot1_present { "connected" } else { "disconnected" }); }
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
                            if let (Some(ref addr), Some(ref pw)) = (&server_addr, &auth_password) {
                                let hmac = sdr_remote_core::auth::compute_hmac(pw, &nonce);
                                let mut buf = [0u8; 36]; // header(4) + hmac(32)
                                let header = Header::new(PacketType::AuthResponse, Flags::NONE);
                                let mut hdr = [0u8; 4];
                                header.serialize(&mut hdr);
                                buf[..4].copy_from_slice(&hdr);
                                buf[4..36].copy_from_slice(&hmac);
                                let _ = send_tx!(&buf, addr.as_str());
                                info!("Auth response sent");
                            } else {
                                warn!("Auth challenge received but no password configured");
                                state.auth_rejected = true;
                                state.connect_status = crate::state::ConnectStatus::Failed(
                                    crate::state::ConnectError::WrongPassword,
                                );
                            }
                        }
                        Ok(Packet::AuthResult(result)) => {
                            // PATCH-1: phase-based classification of AUTH_REJECTED.
                            // - If we hadn't yet been told "TOTP required" → reject = WrongPassword
                            // - If we had been told TOTP required and just submitted a code → reject = WrongTotp
                            // `state.totp_required` at this moment functions as the phase indicator.
                            let was_in_totp_phase = state.totp_required;
                            match result {
                                sdr_remote_core::protocol::AUTH_ACCEPTED => {
                                    info!("Auth accepted");
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
                                    // The owner found the tell: the noise from the
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
                                }
                                sdr_remote_core::protocol::AUTH_TOTP_REQUIRED => {
                                    info!("Password OK, TOTP required");
                                    state.auth_rejected = false;
                                    state.totp_required = true;
                                    state.connect_status =
                                        crate::state::ConnectStatus::AwaitingTotp;
                                }
                                _ => {
                                    warn!("Auth rejected");
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
                        Ok(Packet::TotpChallenge) => {
                            info!("TOTP challenge received");
                        }
                        Ok(Packet::AuthResponse(_)) | Ok(Packet::TotpResponse(_)) => {} // server-only
                        Ok(Packet::Disconnect) => {
                            info!("Server sent disconnect");
                            jitter_buf.reset();
                            // Re-baseline all audio streams on a server-initiated
                            // disconnect (server restart!) — this is the path the
                            // Yaesu-audio-stall hit; don't lean only on the backjump
                            // heuristic (also fixes the short-session stall).
                            yaesu_jitter_buf.reset();
                            yaesu2_jitter_buf.reset();
                            vrx1_jitter_buf.reset();
                            vrx2_jitter_buf.reset();
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
                            state.connected = false;
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
                            // PATCH-1 review finding (B1, parts 2 + 3):
                            // distinguish protocol-version mismatch from generic
                            // malformed bytes, but only during the connect phase —
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

                    // PATCH-1: any reply (even a bad one) means the server-port replied —
                    // useful for distinguishing "wrong bytes" from "no reply at all".
                    connect_any_reply_seen = true;

                    let _ = self.state_tx.send(state.clone());
                }

                _ = audio_tick.tick() => {
                    // Playout: always pull frames from jitter buffer and decode.
                    // This keeps the decoder warm and jitter buffer healthy during TX.
                    // Only write to playback ring buffer when not in TX (muted callback
                    // drains the ring during TX anyway).
                    {
                        let target_ring_low = (playback_rate as usize * 60) / 1000;   // 60ms - refill threshold
                        let target_ring_high = (playback_rate as usize * 200) / 1000; // 200ms - bleed off
                        let ring_level = audio.playback_buffer_level();

                        let max_pull = if ring_level < target_ring_low { 2u32 } else { 1u32 };
                        let skip_this_tick = !ptt && ring_level > target_ring_high;

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
                                if frames_this_tick >= max_pull {
                                    break;
                                }
                                // In refill mode, keep pulling until ring buffer is healthy
                                if frames_this_tick >= 1 && ring_level >= target_ring_low {
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
                                        let next = jitter_buf
                                            .next_seq_peek()
                                            .and_then(|seq| jitter_buf.peek_frame(seq));
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

                                    // RX1 audio can be deliberately off (Rx1Enable, phase 3a):
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

                                    // Write to playback buffers
                                    if !ptt && !left_dev.is_empty() {
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

                            // Mix Yaesu audio (third channel, independent of RX1/RX2)
                            // Only process when there are Yaesu audio packets in the buffer.
                            // The level write sits INSIDE that guard, so the fall-back has to
                            // live outside it - same shape as VRX below.
                            if !(yaesu_logged_first && yaesu_jitter_buf.depth() > 0) {
                                decay_level(&mut state.playback_level_yaesu);
                            }
                            // `|| concealing`: a dry buffer used to skip this block
                            // entirely, so a switched-on stream had no way to conceal a
                            // dropout - only a gap with real frames on both sides.
                            if yaesu_logged_first
                                && (yaesu_jitter_buf.depth() > 0 || (was_connected && yaesu_wanted)) {
                                // If no RX1 audio, create silence buffer for Yaesu-only playback
                                let target_samples = if playback_buf.is_empty() {
                                    let frame_size = (playback_rate as usize * 20) / 1000; // 20ms
                                    playback_buf.resize(frame_size, 0.0);
                                    frame_size
                                } else {
                                    playback_buf.len()
                                };
                                let mut yaesu_buf: Vec<f32> = Vec::with_capacity(target_samples);
                                while yaesu_buf.len() < target_samples {
                                    let decoded = match yaesu_jitter_buf.pull() {
                                        JitterResult::Frame(frame) => {
                                            if frame.opus_data.is_empty() {
                                                None
                                            } else {
                                                st_yaesu.decode(&frame.opus_data, frame.wideband)
                                            }
                                        }
                                        JitterResult::Missing => st_yaesu.conceal(),
                                        // Nothing arriving at all. Same rule as RX1:
                                        // conceal while the stream is switched on and
                                        // the link still counts as up.
                                        JitterResult::NotReady => {
                                            if was_connected && yaesu_wanted { st_yaesu.conceal() } else { None }
                                        }
                                    };
                                    match decoded {
                                        Some(d) => {
                                            if let Some(ref mut w) = rec_yaesu {
                                                let _ = w.write_samples(&d.pcm, d.rate);
                                            }
                                            let concealed = d.concealed;
                                            let mut resampled = d.dev;
                                            // Pre-volume energy: the Yaesu volume carries a
                                            // x20 make-up factor, which would otherwise dominate
                                            // the bar instead of the received signal. Concealed
                                            // frames stay out of it - see RX1.
                                            if !concealed {
                                                yaesu_level_accum += resampled.iter().map(|s| s * s).sum::<f32>();
                                                yaesu_level_count += resampled.len();
                                            }
                                            // (calibrated to the Thetis RX path below)
                                            apply_volume(&mut resampled,
                                                yaesu_volume * yaesu_rx_meter_cal(state.yaesu_model) * local_volume);
                                            yaesu_buf.extend_from_slice(&resampled);
                                        }
                                        None => break,
                                    }
                                }
                                if yaesu_level_count > 0 {
                                    state.playback_level_yaesu = yaesu_rx_meter_cal(state.yaesu_model)
                                        * (yaesu_level_accum / yaesu_level_count as f32).sqrt();
                                }
                                // Mix Yaesu into both L and R (additive, clamped)
                                for (i, sample) in yaesu_buf.iter().enumerate() {
                                    if i < playback_buf.len() {
                                        playback_buf[i] = (playback_buf[i] + sample).clamp(-1.0, 1.0);
                                    }
                                    if i < bin_r_buf.len() {
                                        bin_r_buf[i] = (bin_r_buf[i] + sample).clamp(-1.0, 1.0);
                                    }
                                }
                            }

                            // Mix slot-1 (dual-radio) audio — exact mirror of slot 0,
                            // own jitter-buf/decoder/resampler + muted-start volume.
                            // Slot 1 needs the same fall-back as slot 0, and for the same
                            // reason: its level write sits inside the dry-guard below.
                            if !(yaesu2_logged_first && yaesu2_jitter_buf.depth() > 0) {
                                decay_level(&mut state.playback_level_yaesu2);
                            }
                            if yaesu2_logged_first
                                && (yaesu2_jitter_buf.depth() > 0 || (was_connected && yaesu2_wanted)) {
                                let target_samples = if playback_buf.is_empty() {
                                    let frame_size = (playback_rate as usize * 20) / 1000;
                                    playback_buf.resize(frame_size, 0.0);
                                    frame_size
                                } else {
                                    playback_buf.len()
                                };
                                let mut yaesu2_buf: Vec<f32> = Vec::with_capacity(target_samples);
                                while yaesu2_buf.len() < target_samples {
                                    let decoded = match yaesu2_jitter_buf.pull() {
                                        JitterResult::Frame(frame) => {
                                            if frame.opus_data.is_empty() {
                                                None
                                            } else {
                                                st_yaesu2.decode(&frame.opus_data, frame.wideband)
                                            }
                                        }
                                        JitterResult::Missing => st_yaesu2.conceal(),
                                        JitterResult::NotReady => {
                                            if was_connected && yaesu2_wanted { st_yaesu2.conceal() } else { None }
                                        }
                                    };
                                    match decoded {
                                        Some(d) => {
                                            if let Some(ref mut w) = rec_yaesu2 {
                                                let _ = w.write_samples(&d.pcm, d.rate);
                                            }
                                            let concealed = d.concealed;
                                            let mut resampled = d.dev;
                                            if !concealed {
                                                yaesu2_level_accum += resampled.iter().map(|s| s * s).sum::<f32>();
                                                yaesu2_level_count += resampled.len();
                                            }
                                            apply_volume(&mut resampled,
                                                yaesu2_volume * yaesu_rx_meter_cal(state.yaesu2_model) * local_volume);
                                            yaesu2_buf.extend_from_slice(&resampled);
                                        }
                                        None => break,
                                    }
                                }
                                if !yaesu2_buf.is_empty() {
                                    state.playback_level_yaesu2 = yaesu_rx_meter_cal(state.yaesu2_model)
                                        * (yaesu2_level_accum / yaesu2_level_count.max(1) as f32).sqrt();
                                }
                                for (i, sample) in yaesu2_buf.iter().enumerate() {
                                    if i < playback_buf.len() {
                                        playback_buf[i] = (playback_buf[i] + sample).clamp(-1.0, 1.0);
                                    }
                                    if i < bin_r_buf.len() {
                                        bin_r_buf[i] = (bin_r_buf[i] + sample).clamp(-1.0, 1.0);
                                    }
                                }
                            }

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
                                let target_samples = if playback_buf.is_empty() {
                                    let frame_size = (playback_rate as usize * 20) / 1000;
                                    playback_buf.resize(frame_size, 0.0);
                                    frame_size
                                } else {
                                    playback_buf.len()
                                };
                                let mut vrx_buf: Vec<f32> = Vec::with_capacity(target_samples);
                                while vrx_buf.len() < target_samples {
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
                                        JitterResult::Missing => st_vrx1.conceal(),
                                        JitterResult::NotReady => {
                                            if was_connected && vrx1_wanted { st_vrx1.conceal() } else { None }
                                        }
                                    };
                                    match decoded {
                                        Some(d) => {
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
                                }
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
                                let target_samples = if playback_buf.is_empty() {
                                    let frame_size = (playback_rate as usize * 20) / 1000;
                                    playback_buf.resize(frame_size, 0.0);
                                    frame_size
                                } else {
                                    playback_buf.len()
                                };
                                let mut vrx_buf: Vec<f32> = Vec::with_capacity(target_samples);
                                while vrx_buf.len() < target_samples {
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
                                        JitterResult::Missing => st_vrx2.conceal(),
                                        JitterResult::NotReady => {
                                            if was_connected && vrx2_wanted { st_vrx2.conceal() } else { None }
                                        }
                                    };
                                    match decoded {
                                        Some(d) => {
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
                                }
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

                            // Write audio to playback - stereo if binaural R available
                            if !playback_buf.is_empty() {
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

                    // Connection timeout detection: only disconnect when BOTH
                    // heartbeat ACK and audio packets have been absent for the timeout.
                    // Dynamic timeout: max(6s, rtt*8) - accommodates mobile networks.
                    if was_connected {
                        let timeout_ms = (last_hb_ack_rtt as u64 * 8).max(CONNECTION_TIMEOUT_MIN_MS);
                        let hb_timed_out = last_hb_ack_time
                            .map_or(false, |t| t.elapsed().as_millis() > timeout_ms as u128);
                        let audio_timed_out = last_audio_received
                            .map_or(true, |t| t.elapsed().as_millis() > timeout_ms as u128);

                        if hb_timed_out && audio_timed_out {
                            info!("Connection lost (no traffic for {}ms, ring={}, jbuf={}, jitter={:.1}ms)",
                                timeout_ms, audio.playback_buffer_level(), jitter_buf.depth(), jitter_buf.jitter_ms());
                            // Don't reset jitter buffer - let it drain via PLC
                            // so audio resumes smoothly if packets return
                            was_connected = false;
                            last_hb_ack_rtt = 0;
                            logged_first_rx = false;
                            logged_first_tx = false;
                            rx_volume_synced = false;
                            rx2_volume_synced = false;
                            state.rx_af_gain = 0;
                            state.connected = false;
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
                                    audio = new_audio;
                                    audio.set_capture_gate_delay_ms(mic_gate_delay_ms);
                                    // The reopened stream may be on a different device at a
                                    // different sample rate (a Bluetooth route change reopens
                                    // on the BT-SCO device at 8/16 kHz) -> rebuild the resamplers
                                    // for the new rate, otherwise the audio comes out choppy.
                                    resync_audio_rates!();
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

                    // When not connected, drain capture buffer and clear accumulator
                    if server_addr.is_none() {
                        audio.read_capture(&mut drain_buf);
                        accum_buf.clear();
                        let _ = self.state_tx.send(state.clone());
                        continue;
                    }
                    let addr = server_addr.as_ref().unwrap();

                    let af_gain = (rx_volume * 100.0).round() as u16;

                    // Send RX1 AF gain control when changed (only after initial sync from server)
                    if rx_volume_synced && af_gain != last_sent_volume {
                        let ctrl = ControlPacket {
                            control_id: ControlId::Rx1AfGain,
                            value: af_gain,
                        };
                        let mut buf = [0u8; ControlPacket::SIZE];
                        ctrl.serialize(&mut buf);
                        let _ = send_tx!(&buf, addr.as_str());
                        last_sent_volume = af_gain;
                    }

                    // Send RX2 AF gain control when changed (only after initial sync from server)
                    // Only send when the USER changed the slider (SetRx2Volume command),
                    // not when the server broadcast updated rx2_volume.
                    let rx2_af_gain = (rx2_volume * 100.0).round() as u16;
                    if rx2_volume_synced && rx2_volume_user_changed && rx2_af_gain != last_sent_rx2_volume {
                        info!("Sending RX2 AF gain to server: {}% (was {}%)", rx2_af_gain, last_sent_rx2_volume);
                        let ctrl = ControlPacket {
                            control_id: ControlId::Rx2AfGain,
                            value: rx2_af_gain,
                        };
                        let mut buf = [0u8; ControlPacket::SIZE];
                        ctrl.serialize(&mut buf);
                        let _ = send_tx!(&buf, addr.as_str());
                        last_sent_rx2_volume = rx2_af_gain;
                        rx2_volume_user_changed = false;
                    }

                    // Heartbeat (skip while waiting for TOTP input)
                    if !state.totp_required && last_hb_sent.elapsed().as_millis() > HEARTBEAT_INTERVAL_MS as u128 {
                        if let Some(max) = loss_window_max_seq {
                            let expected = if let Some(prev) = loss_prev_max_seq {
                                max.wrapping_sub(prev) // packets since last window
                            } else {
                                loss_window_received // first window: trust received count
                            };
                            let raw_loss = if expected > 0 && loss_window_received <= expected {
                                (100 * (expected - loss_window_received) / expected) as u8
                            } else {
                                0
                            };
                            // EMA smoothing: slow rise/fall prevents jumpy display
                            smoothed_loss = smoothed_loss * 0.7 + raw_loss as f32 * 0.3;
                            current_loss_percent = smoothed_loss.round() as u8;
                            loss_prev_max_seq = Some(max);
                        } else if loss_prev_max_seq.is_some() {
                            // Had packets before, now nothing. This is only REAL
                            // loss if the client still expects RX audio. For a
                            // VRX-only client (RX1+RX2 audio deliberately off) the
                            // AudioMultiCh stream stops on purpose — absence is then not
                            // loss. Otherwise the loss would climb to 100% and the
                            // server-loss-gate would filter out the VRX spectrum (the VRX
                            // audio/spectrum streams do not feed this meter).
                            if state.rx1_enabled || state.rx2_enabled {
                                smoothed_loss = smoothed_loss * 0.7 + 100.0 * 0.3;
                                current_loss_percent = smoothed_loss.round() as u8;
                            } else {
                                smoothed_loss = 0.0;
                                current_loss_percent = 0;
                                loss_prev_max_seq = None; // clean restart when RX audio resumes
                            }
                        }
                        state.loss_percent = current_loss_percent;
                        loss_window_received = 0;
                        loss_window_max_seq = None;

                        // Bandwidth-window flush — in sync with the heartbeat
                        // tick (~500 ms). bytes × 8 / window_ms = bits/ms = kbps.
                        let win_ms = bw_window_start.elapsed().as_millis().max(1) as u64;
                        state.down_kbps = (bw_rx_bytes.saturating_mul(8) / win_ms) as u32;
                        state.up_kbps = (bw_tx_bytes.saturating_mul(8) / win_ms) as u32;
                        bw_rx_bytes = 0;
                        bw_tx_bytes = 0;
                        bw_window_start = Instant::now();

                        // Per-PacketType breakdown every 5 s — published to
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
                            loss_percent: current_loss_percent,
                            jitter_ms: jitter_buf.jitter_ms().min(255.0) as u8,
                            capabilities: Capabilities::NONE,
                        };
                        hb_sequence = hb_sequence.wrapping_add(1);

                        let mut buf = [0u8; Heartbeat::SIZE];
                        hb.serialize(&mut buf);
                        let _ = send_tx!(&buf, addr.as_str());
                        last_hb_sent = Instant::now();

                        // PATCH-1 review finding (B1, part 4): NoUdpResponse
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

                    if ptt != last_ptt {
                        ptt_burst_remaining = PTT_BURST_COUNT;
                        info!("PTT: {}", if ptt { "TX" } else { "RX" });
                        last_ptt = ptt;
                    }

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

                    // Thetis-TXEQ-bypass during WAV-playback to the MAIN RADIO (Thetis-PTT):
                    // turn off the mic-profile TX-EQ during Play and restore on stop/PTT-release/
                    // end - just like Thetis' own record/playback. Only Thetis (ptt); the
                    // Yaesu EQ is already skipped client-side. Edge-triggered via the flag.
                    let thetis_wav_tx_active = playback_is_tx && ptt && playback_wav.is_some();
                    if thetis_wav_tx_active != thetis_txeq_bypassed {
                        let ctrl = ControlPacket {
                            control_id: ControlId::ThetisTxeq,
                            value: if thetis_wav_tx_active { 0 } else { 1 },
                        };
                        let mut cbuf = [0u8; ControlPacket::SIZE];
                        ctrl.serialize(&mut cbuf);
                        let _ = send_tx!(&cbuf, addr.as_str());
                        thetis_txeq_bypassed = thetis_wav_tx_active;
                        info!(
                            "Thetis TXEQ {} (WAV playback)",
                            if thetis_wav_tx_active { "off" } else { "restored" }
                        );
                    }

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
                                let f16 = resample_linear(
                                    &src_f32,
                                    playback_wav_rate,
                                    NETWORK_SAMPLE_RATE_WIDEBAND,
                                );
                                if f16.len() >= FRAME_SAMPLES_WIDEBAND {
                                    // Meter = the frame as encoded, tx_gain included.
                                    state.capture_level = peak_scaled(
                                        &f16[..FRAME_SAMPLES_WIDEBAND], tx_gain);
                                    let pcm_i16: Vec<i16> = f16[..FRAME_SAMPLES_WIDEBAND]
                                        .iter()
                                        .map(|&s| {
                                            (s * tx_gain * 32767.0).clamp(-32768.0, 32767.0) as i16
                                        })
                                        .collect();
                                    match encoder.encode(&pcm_i16) {
                                        Ok(opus_data) => {
                                            let flags = Flags::NONE.with_ptt(thetis_ptt);
                                            let pkt = AudioPacket {
                                                flags,
                                                sequence: tx_sequence,
                                                timestamp: start.elapsed().as_millis() as u32,
                                                opus_data,
                                            };
                                            tx_sequence = tx_sequence.wrapping_add(1);
                                            let mut buf = Vec::with_capacity(MAX_PACKET_SIZE);
                                            pkt.serialize(&mut buf);
                                            let _ = send_tx!(&buf, addr.as_str());
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
                                let fcap =
                                    resample_linear(&src_f32, playback_wav_rate, capture_rate);
                                yaesu_tx_accum.extend_from_slice(&fcap);
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
                    let mut sent_any = false;
                    while accum_buf.len() >= capture_frame_samples {
                        let chunk: Vec<f32> = accum_buf.drain(..capture_frame_samples).collect();
                        let mut pcm_8k = resample_to_network(&mut resampler_in, &chunk);

                        // AGC: normalize mic level before manual TX gain
                        // (runs always to keep AGC state warm for instant PTT response)
                        if agc_enabled {
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
                        // Past the AGC and the gain trim on purpose. Those
                        // exist to even out a voice; a fixed tone handed to
                        // them comes out swelling, and what is set in the panel
                        // is what should go out.
                        let mut roger_here = false;
                        if let Some((ref mut tone, 0, _)) = roger_tone {
                            tone.fill(&mut pcm_8k, roger_cfg.volume);
                            roger_here = true;
                        }

                        // Meter = what is encoded: after AGC, with tx_gain applied.
                        let level_gain = if roger_here { 1.0 } else { tx_gain };
                        state.capture_level = peak_scaled(&pcm_8k, level_gain);
                        let pcm_i16: Vec<i16> = pcm_8k
                            .iter()
                            .map(|&s| (s * level_gain * 32767.0).clamp(-32768.0, 32767.0) as i16)
                            .collect();

                        if pcm_i16.len() >= FRAME_SAMPLES_WIDEBAND {
                            match encoder.encode(&pcm_i16[..FRAME_SAMPLES_WIDEBAND]) {
                                Ok(opus_data) => {
                                    let flags = Flags::NONE.with_ptt(thetis_ptt);
                                    let pkt = AudioPacket {
                                        flags,
                                        sequence: tx_sequence,
                                        timestamp: start.elapsed().as_millis() as u32,
                                        opus_data,
                                    };
                                    tx_sequence = tx_sequence.wrapping_add(1);

                                    let mut buf = Vec::with_capacity(MAX_PACKET_SIZE);
                                    pkt.serialize(&mut buf);
                                    let _ = send_tx!(&buf, addr.as_str());

                                    if !logged_first_tx {
                                        info!("TX: first audio packet sent to {} (seq={}, accum_remain={})",
                                            addr, tx_sequence, accum_buf.len());
                                        logged_first_tx = true;
                                    }

                                    if ptt_burst_remaining > 0 {
                                        ptt_burst_remaining -= 1;
                                        let _ = send_tx!(&buf, addr.as_str());
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
                                warn!("Roger beep did not play out in time - releasing PTT anyway");
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
                                state.ptt_denied = false;
                                if let Some(ref addr) = server_addr {
                                    if audio_mode == 1 && last_sent_bin != Some(1) {
                                        let ctrl = ControlPacket { control_id: ControlId::Binaural, value: 1 };
                                        let mut buf = [0u8; ControlPacket::SIZE];
                                        ctrl.serialize(&mut buf);
                                        let _ = send_tx!(&buf, addr.as_str());
                                        last_sent_bin = Some(1);
                                    }
                                }
                            }
                            1 => {
                                yaesu_ptt = false;
                                if let Some(ref addr) = server_addr {
                                    let ctrl = ControlPacket { control_id: ControlId::YaesuPtt, value: 0 };
                                    let mut buf = [0u8; ControlPacket::SIZE];
                                    ctrl.serialize(&mut buf);
                                    let _ = send_tx!(&buf, addr.as_str());
                                }
                            }
                            _ => {
                                yaesu2_ptt = false;
                                if let Some(ref addr) = server_addr {
                                    let ctrl = ControlPacket { control_id: ControlId::Yaesu2Ptt, value: 0 };
                                    let mut buf = [0u8; ControlPacket::SIZE];
                                    ctrl.serialize(&mut buf);
                                    let _ = send_tx!(&buf, addr.as_str());
                                }
                            }
                        }
                        info!("Roger beep done - PTT released");
                        roger_tone = None;
                    }

                    // PTT burst: send empty PTT-only packets for reliability
                    // (when no audio was sent this tick, e.g. PTT state change)
                    if !sent_any && ptt_burst_remaining > 0 {
                        let pkt = AudioPacket {
                            flags: Flags::NONE.with_ptt(thetis_ptt),
                            sequence: tx_sequence,
                            timestamp: start.elapsed().as_millis() as u32,
                            opus_data: vec![],
                        };
                        tx_sequence = tx_sequence.wrapping_add(1);
                        ptt_burst_remaining -= 1;

                        let mut buf = Vec::with_capacity(64);
                        pkt.serialize(&mut buf);
                        let _ = send_tx!(&buf, addr.as_str());
                    }

                    // === Yaesu TX: completely separate mic audio path ===
                    // Applies to both radios (PTT mutually exclusive); per PTT
                    // the right EQ + mic-gain are chosen below.
                    if yaesu_ptt || yaesu2_ptt {
                        // Resample to 16kHz, encode wideband Opus
                        while yaesu_tx_accum.len() >= capture_frame_samples {
                            let mut chunk: Vec<f32> = yaesu_tx_accum.drain(..capture_frame_samples).collect();

                            // Apply 5-band EQ at capture rate (before resampling).
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

                            // Resample to 16kHz and apply Yaesu-specific mic gain before Opus.
                            let mic_gain = if yaesu2_ptt { yaesu2_local_mic_gain } else { yaesu_local_mic_gain };
                            let mut resampled = resample_to_network(&mut yaesu_tx_resampler, &chunk);
                            // Client-side TX chain (radio processing does not work on USB):
                            // compressor → AGC, before the manual tx_gain/mic_gain trim.
                            // Per radio (like the EQ): slot-1 PTT → radio-2 chain.
                            // Compressor is self-gated on amount; AGC on its own toggle.
                            // Mic chain (compressor + AGC) ONLY for live mic, not for
                            // WAV-playback (which is already line level; comp/AGC would distort it).
                            if !playback_is_tx {
                                if yaesu2_ptt {
                                    yaesu2_compressor.process(&mut resampled);
                                    if yaesu2_tx_agc_enabled {
                                        yaesu2_agc.process(&mut resampled);
                                    }
                                } else {
                                    yaesu_compressor.process(&mut resampled);
                                    if yaesu_tx_agc_enabled {
                                        yaesu_agc.process(&mut resampled);
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
                            // Live mic gets the 4x quiet-mic boost; WAV-playback goes at
                            // line level (only mic_gain, no 4x) so it does not clip.
                            let final_scale = if roger_here {
                                1.0
                            } else if playback_is_tx {
                                mic_gain
                            } else {
                                mic_gain * 4.0
                            };
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
                            // Meter = the frame as encoded: EQ, compressor and AGC
                            // applied, scaled by the same final gain. Covers live mic
                            // and WAV-playback, which share this chain.
                            state.yaesu_mic_level = peak_scaled(&resampled, final_scale);
                            let pcm_i16: Vec<i16> = resampled.iter()
                                .map(|&s| (s * final_scale * 32767.0).clamp(-32768.0, 32767.0) as i16)
                                .collect();

                            if pcm_i16.len() >= FRAME_SAMPLES_WIDEBAND {
                                if let Ok(opus_data) = yaesu_tx_encoder.encode(&pcm_i16[..FRAME_SAMPLES_WIDEBAND]) {
                                    if let Some(ref addr) = server_addr {
                                        let pkt = AudioPacket {
                                            flags: Flags::NONE,
                                            sequence: yaesu_tx_sequence,
                                            timestamp: start.elapsed().as_millis() as u32,
                                            opus_data,
                                        };
                                        yaesu_tx_sequence = yaesu_tx_sequence.wrapping_add(1);
                                        let mut buf = Vec::with_capacity(256);
                                        // Slot-1 PTT → AudioYaesu2, otherwise slot-0 AudioYaesu.
                                        let to_slot1 = match roger_tone {
                                            Some((_, ch, _)) => ch == 2,
                                            None => yaesu2_ptt,
                                        };
                                        let tx_ptype = if to_slot1 {
                                            PacketType::AudioYaesu2
                                        } else {
                                            PacketType::AudioYaesu
                                        };
                                        pkt.serialize_as_type(&mut buf, tx_ptype);
                                        let _ = send_tx!(&buf, addr.as_str());
                                    }
                                }
                            }
                        }
                    } else {
                        yaesu_tx_accum.clear();
                    }

                    let _ = self.state_tx.send(state.clone());
                }

                _ = shutdown.changed() => {
                    info!("Client network shutting down");
                    if let Some(ref addr) = server_addr {
                        // Restore Thetis TXEQ if a WAV-playback left it bypassed.
                        if thetis_txeq_bypassed {
                            let ctrl = ControlPacket {
                                control_id: ControlId::ThetisTxeq,
                                value: 1,
                            };
                            let mut cbuf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut cbuf);
                            let _ = send_tx!(&cbuf, addr.as_str());
                            // The flag is not set back here: nothing reads it
                            // after this point, and a write that only the
                            // compiler notices is a question for whoever reads
                            // this next.
                            info!("Thetis TXEQ restored (shutdown)");
                        }
                        let mut buf = [0u8; DisconnectPacket::SIZE];
                        DisconnectPacket::serialize(&mut buf);
                        let _ = send_tx!(&buf, addr.as_str());
                        info!("Sent disconnect to server");
                    }
                    break;
                }
            }
        }

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
/// Simple linear resample for WAV-TX-playback (recorded-message replay).
/// Not latency-/HF-critical, so linear interpolation suffices and is
/// stateless (no resampler state per tick). Used to bring a recorded WAV from
/// its header rate (8 or 16 kHz) to the target rate: 16 kHz for the
/// Thetis-TX-encoder, or `capture_rate` for the Yaesu-TX-accumulator.
fn resample_linear(input: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if input.is_empty() || from_rate == 0 || to_rate == 0 || from_rate == to_rate {
        return input.to_vec();
    }
    let ratio = to_rate as f64 / from_rate as f64;
    let out_len = ((input.len() as f64) * ratio).round() as usize;
    let last = input.len() - 1;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f64 / ratio;
        let idx = src.floor() as usize;
        let frac = (src - idx as f64) as f32;
        let a = input[idx.min(last)];
        let b = input[(idx + 1).min(last)];
        out.push(a + (b - a) * frac);
    }
    out
}

fn resample_to_device(resampler: &mut impl rubato::Resampler<f32>, pcm_i16: &[i16]) -> Vec<f32> {
    let input_f32: Vec<f32> = pcm_i16.iter().map(|&s| s as f32 / 32768.0).collect();
    match resampler.process(&[input_f32], None) {
        Ok(result) => result.into_iter().next().unwrap_or_default(),
        Err(e) => {
            warn!("resample network->device error: {}", e);
            Vec::new()
        }
    }
}

/// Resample f32 device rate -> f32 8kHz
fn resample_to_network(resampler: &mut impl rubato::Resampler<f32>, pcm_f32: &[f32]) -> Vec<f32> {
    match resampler.process(&[pcm_f32.to_vec()], None) {
        Ok(result) => result.into_iter().next().unwrap_or_default(),
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

/// Input calibration for a Yaesu receive meter, per radio model.
///
/// A Yaesu USB CODEC delivers a markedly lower line level than the Thetis DDC
/// audio, which is why the volume path carries a x20 make-up factor. That
/// factor is the operator's volume range and is excluded from the meter, so
/// without calibration the Yaesu bars sit below RX/VRX for the same signal.
///
/// The two radios are not alike: measured on comparable signals where RX/VRX
/// peak near -10 dB, the FT-991A needed +10 dB and the FTX-1 +24.5 dB (first
/// tried at +27, which read 2-3 dB hot).
/// One shared constant therefore lined up one radio and left the other far
/// behind. Keyed on the model rather than the slot, so it follows the radio
/// when the slots are swapped.
/// Receive calibration per radio model. Used for BOTH the level meter and the
/// playback gain: the two CODECs deliver line levels about 14 dB apart, so one
/// flat constant cannot serve both radios.
///
/// The audio path used a flat 20.0 (+26 dB) for both until build 106, while the
/// meter had been per-model since build 75/76. That left the 991A about 16 dB
/// hot - audible at the very bottom of the volume slider, where every other
/// channel had already gone quiet.
fn yaesu_rx_meter_cal(model: u8) -> f32 {
    match model {
        1 => 16.8,  // FTX-1, +24.5 dB
        _ => 3.16,  // FT-991A, +10 dB
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

    // Regression: the presence flip is the core requirement — presence determines connected,
    // dynamically true/false. Tests the pure apply_yaesu_presence seam.
    #[test]
    fn presence_drives_connected_flip() {
        let mut state = RadioState::default();
        assert!(!state.yaesu_connected);
        assert!(!state.yaesu2_connected);

        // Both present → both connected, models adopted, both changed.
        let (c0, c1) = apply_yaesu_presence(&mut state, &YaesuPresencePacket {
            slot0_present: true, slot0_model: 0, slot1_present: true, slot1_model: 1, slot0_trouble: 0, slot1_trouble: 0,
        });
        assert!(c0 && c1);
        assert!(state.yaesu_connected && state.yaesu2_connected);
        assert_eq!(state.yaesu_model, 0);
        assert_eq!(state.yaesu2_model, 1);

        // Slot 0 drops out → connected flips to false (core of the dynamics),
        // slot 1 unchanged (no change flag).
        let (c0, c1) = apply_yaesu_presence(&mut state, &YaesuPresencePacket {
            slot0_present: false, slot0_model: 0, slot1_present: true, slot1_model: 1, slot0_trouble: 0, slot1_trouble: 0,
        });
        assert!(c0 && !c1);
        assert!(!state.yaesu_connected);
        assert!(state.yaesu2_connected);

        // Idempotent: same presence again → no change flags.
        let (c0, c1) = apply_yaesu_presence(&mut state, &YaesuPresencePacket {
            slot0_present: false, slot0_model: 0, slot1_present: true, slot1_model: 1, slot0_trouble: 0, slot1_trouble: 0,
        });
        assert!(!c0 && !c1);
    }

    // Regression: an absent radio must not leave a hi_swr flag set. Otherwise
    // the other radio keeps renewing the SWR alarm on every state push —
    // the alarm then never goes out.
    #[test]
    fn presence_absence_clears_hi_swr() {
        let mut state = RadioState::default();
        apply_yaesu_presence(&mut state, &YaesuPresencePacket {
            slot0_present: true, slot0_model: 0, slot1_present: true, slot1_model: 1, slot0_trouble: 0, slot1_trouble: 0,
        });
        state.yaesu_hi_swr = true;
        state.yaesu2_hi_swr = true;

        // Slot 1 disappears → only its flag is cleared.
        apply_yaesu_presence(&mut state, &YaesuPresencePacket {
            slot0_present: true, slot0_model: 0, slot1_present: false, slot1_model: 1, slot0_trouble: 0, slot1_trouble: 0,
        });
        assert!(state.yaesu_hi_swr, "aanwezige radio houdt zijn vlag");
        assert!(!state.yaesu2_hi_swr, "afwezige radio verliest zijn vlag");

        // Slot 0 disappears too → nothing remains to feed the alarm.
        apply_yaesu_presence(&mut state, &YaesuPresencePacket {
            slot0_present: false, slot0_model: 0, slot1_present: false, slot1_model: 1, slot0_trouble: 0, slot1_trouble: 0,
        });
        assert!(!state.yaesu_hi_swr && !state.yaesu2_hi_swr);
    }
}
