// SPDX-License-Identifier: GPL-2.0-or-later

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use log::{info, warn};
use tokio::net::{ToSocketAddrs, UdpSocket};
use tokio::sync::{mpsc, watch, Mutex as AsyncMutex};
use tokio::time::{interval, Duration};

use sdr_remote_core::codec::{OpusDecoder, OpusDecoderWideband, OpusEncoderWideband};
use sdr_remote_core::jitter::{BufferedFrame, JitterBuffer, JitterResult};
use sdr_remote_core::protocol::*;
use sdr_remote_core::{FRAME_SAMPLES, FRAME_SAMPLES_WIDEBAND, MAX_PACKET_SIZE, NETWORK_SAMPLE_RATE, NETWORK_SAMPLE_RATE_WIDEBAND};

use crate::audio::AudioBackend;
use crate::commands::Command;
use crate::state::RadioState;

/// Phase C: kanalen die de client-engine aan de relay-monitor koppelen wanneer de
/// client via de relay verbindt i.p.v. direct-UDP. De client (`sdr-remote-client`)
/// knoopt deze aan een `RelayMonitor` met tunnel; de engine kent alleen de mpsc-kanalen.
pub struct ClientRelayTunnel {
    /// Engine -> monitor: te verzenden TL-frames (het adres is het server-adres, genegeerd door de relay).
    pub uplink_tx: mpsc::UnboundedSender<(SocketAddr, Vec<u8>)>,
    /// Monitor -> engine: ontvangen (gedecodeerde) TL-frames.
    pub inbound_rx: mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)>,
    /// Het server-adres; `recv` tagt inkomende frames hiermee, net als direct-UDP.
    pub server_addr: SocketAddr,
}

/// Client-transport: direct-UDP (default, byte-identiek) of via de relay-tunnel.
/// Bootst het `UdpSocket`-API-oppervlak na dat de engine gebruikt (`send_to`,
/// `recv_from`, `local_addr`), zodat alle bestaande call-sites ongewijzigd blijven.
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
                // De relay kent de bestemming; het addr-argument (server) wordt genegeerd.
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

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        match self {
            ClientTransport::Direct(s) => s.local_addr(),
            ClientTransport::Relay { .. } => Ok(SocketAddr::from(([0, 0, 0, 0], 0))),
        }
    }

    /// True in relay-modus. De Connect-handler slaat dan de directe-IP-DNS/parse-check
    /// over: in relay-modus is er geen direct server-IP (de route is url/station/token),
    /// en het Connect-adres is puur een display-label dat de Relay-transport negeert.
    fn is_relay(&self) -> bool {
        matches!(self, ClientTransport::Relay { .. })
    }
}

/// Pure toepassing van een `YaesuPresence`-pakket op de client-state. Presence is
/// de connected-autoriteit (gedeeld naar desktop + Android). Retourneert
/// `(slot0_changed, slot1_changed)` zodat de caller value-change-only kan loggen.
/// Zet connected dynamisch true/false.
pub(crate) fn apply_yaesu_presence(state: &mut RadioState, p: &YaesuPresencePacket) -> (bool, bool) {
    let c0 = state.yaesu_connected != p.slot0_present;
    let c1 = state.yaesu2_connected != p.slot1_present;
    state.yaesu_connected = p.slot0_present;
    state.yaesu2_connected = p.slot1_present;
    state.yaesu_model = p.slot0_model;
    state.yaesu2_model = p.slot1_model;
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

// --- TX Compressor (spraakcompressor voor de Yaesu USB-TX-keten) ---
// De radio-eigen processor werkt niet op USB-audio; deze client-side compressor
// vult dat gat. `amount` 0.0..1.0 (0=uit): comprimeert pieken boven een drempel
// (ratio) en past makeup-gain toe → meer dichtheid/punch.
struct TxCompressor {
    amount: f32,
    env: f32,
}

impl TxCompressor {
    fn new() -> Self { Self { amount: 0.0, env: 0.0 } }

    fn set_amount(&mut self, a: f32) { self.amount = a.clamp(0.0, 1.0); }

    fn process(&mut self, samples: &mut [f32]) {
        if self.amount <= 0.001 { return; }
        // Drempel zakt, ratio + makeup stijgen met amount.
        let threshold = 0.30 - 0.22 * self.amount; // 0.30 → 0.08
        let ratio = 1.0 + 5.0 * self.amount;        // 1 → 6
        let makeup = 1.0 + 1.5 * self.amount;       // 1.0 → 2.5
        let exp = 1.0 / ratio - 1.0;                // negatieve exponent → gain-reductie
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
        // Per-channel mono decoders for multi-channel audio
        let mut dec_rx1 = OpusDecoder::new()?;
        let mut dec_bin_r = OpusDecoder::new()?;
        let mut dec_rx2 = OpusDecoder::new()?;
        // Wideband parallel-decoders. Worden gebruikt zodra een
        // multi-channel packet binnenkomt met `Flags::AUDIO_WIDEBAND`
        // (opt-in via Settings → Audio). Default unused; geen
        // runtime-impact zolang server NB streamt.
        let mut dec_rx1_wb = OpusDecoderWideband::new()?;
        let mut dec_bin_r_wb = OpusDecoderWideband::new()?;
        let mut dec_rx2_wb = OpusDecoderWideband::new()?;

        // Yaesu (FT-991A) codec + jitter buffer - independent third audio channel.
        // RX-bandbreedte volgt de Thetis-wideband-toggle (build 122): per packet
        // bepaalt de AUDIO_WIDEBAND-flag of we NB (8 kHz) of WB (16 kHz) decoderen.
        // Beide decoders/resamplers blijven aan; `*_last_wb` onthoudt het laatste
        // formaat voor PLC (Missing-frames dragen geen flag).
        let mut yaesu_decoder_nb = OpusDecoder::new()?;
        let mut yaesu_decoder_wb = OpusDecoderWideband::new()?;
        let mut yaesu_last_wb = false;
        let mut yaesu_jitter_buf = JitterBuffer::new(3, 40);
        let mut yaesu_logged_first = false;
        // Dual-radio slot 1 (PATCH-dual-radio-991a-ftx1) — eigen onafhankelijk
        // kanaal, exacte spiegel van slot 0.
        let mut yaesu2_decoder_nb = OpusDecoder::new()?;
        let mut yaesu2_decoder_wb = OpusDecoderWideband::new()?;
        let mut yaesu2_last_wb = false;
        let mut yaesu2_jitter_buf = JitterBuffer::new(3, 40);
        let mut yaesu2_logged_first = false;

        // Yaesu TX: wideband Opus (16kHz) for USB output
        let mut yaesu_tx_sequence: u32 = 0;
        let mut yaesu_tx_accum: Vec<f32> = Vec::new();
        let mut yaesu_tx_encoder = OpusEncoderWideband::new()?;
        let mut yaesu_tx_bitrate_bps: i32 = 24_000;
        // Anti-alias filter: sinc_len 128 + f_cutoff 0.95 (identiek aan
        // server-side Yaesu TX resampler). De korte filter (sinc_len 32)
        // liet NT-USB content >8 kHz onvoldoende afsnijden, waardoor die
        // frequenties bij de 48→16 kHz decimatie terug-aliassen in
        // 0-8 kHz en op de RF-uitgang als "raar klinkende" hoge tonen
        // hoorbaar waren (operator-bevinding 2026-06-02). ~4 ms extra
        // filter delay — verwaarloosbaar voor mic→Yaesu pad.
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
        let mut res_rx1_out = rubato::SincFixedIn::<f32>::new(
            playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mk_sinc(), FRAME_SAMPLES, 1,
        ).context("RX1 8k->device resampler")?;
        let mut res_bin_r_out = rubato::SincFixedIn::<f32>::new(
            playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mk_sinc(), FRAME_SAMPLES, 1,
        ).context("BinR 8k->device resampler")?;
        let mut res_rx2_out = rubato::SincFixedIn::<f32>::new(
            playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mk_sinc(), FRAME_SAMPLES, 1,
        ).context("RX2 8k->device resampler")?;
        // Wideband (16 kHz → playback_rate) parallel-resamplers voor de
        // opt-in WB Thetis-audio pad. Idle zolang geen WB-getagde packet
        // binnenkomt.
        let mut res_rx1_out_wb = rubato::SincFixedIn::<f32>::new(
            playback_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64, 1.0, mk_sinc(), FRAME_SAMPLES_WIDEBAND, 1,
        ).context("RX1 16k->device WB resampler")?;
        let mut res_bin_r_out_wb = rubato::SincFixedIn::<f32>::new(
            playback_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64, 1.0, mk_sinc(), FRAME_SAMPLES_WIDEBAND, 1,
        ).context("BinR 16k->device WB resampler")?;
        let mut res_rx2_out_wb = rubato::SincFixedIn::<f32>::new(
            playback_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64, 1.0, mk_sinc(), FRAME_SAMPLES_WIDEBAND, 1,
        ).context("RX2 16k->device WB resampler")?;
        // Dedicated WAV playback resamplers. Do not share RX1 live resampler state;
        // recordings played through the Server tab must sound like the file on disk.
        let mut wav_res_out = rubato::SincFixedIn::<f32>::new(
            playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mk_sinc(), FRAME_SAMPLES, 1,
        ).context("WAV 8k->device resampler")?;
        let mut wav_res_out_wb = rubato::SincFixedIn::<f32>::new(
            playback_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64, 1.0, mk_sinc(), FRAME_SAMPLES_WIDEBAND, 1,
        ).context("WAV 16k->device resampler")?;

        // Yaesu RX-resamplers per formaat (NB 8k→device, WB 16k→device); per packet
        // gekozen op de wideband-flag (build 122). SincInterpolationParameters is niet
        // Clone → closure die per gebruik een vers literal maakt.
        let mk_yaesu_sinc = || rubato::SincInterpolationParameters {
            sinc_len: 32, f_cutoff: 0.90, oversampling_factor: 32,
            interpolation: rubato::SincInterpolationType::Cubic,
            window: rubato::WindowFunction::Blackman,
        };
        let mut yaesu_res_nb = rubato::SincFixedIn::<f32>::new(
            playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mk_yaesu_sinc(), FRAME_SAMPLES, 1,
        ).context("create Yaesu NB resampler")?;
        let mut yaesu_res_wb = rubato::SincFixedIn::<f32>::new(
            playback_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64, 1.0, mk_yaesu_sinc(), FRAME_SAMPLES_WIDEBAND, 1,
        ).context("create Yaesu WB resampler")?;
        let mut yaesu2_res_nb = rubato::SincFixedIn::<f32>::new(
            playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mk_yaesu_sinc(), FRAME_SAMPLES, 1,
        ).context("create Yaesu2 NB resampler")?;
        let mut yaesu2_res_wb = rubato::SincFixedIn::<f32>::new(
            playback_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64, 1.0, mk_yaesu_sinc(), FRAME_SAMPLES_WIDEBAND, 1,
        ).context("create Yaesu2 WB resampler")?;

        // VRX1 + VRX2 — each is a separate jitter buf + 8 kHz NB Opus
        // decoder + resampler. Server-side FFT-channelizers feed these
        // streams; both get mixed into the main playback alongside
        // RX1/RX2/Yaesu. VRX1 listens on RX1 IQ + VFO-A, VRX2 on RX2
        // IQ + VFO-B.
        let mut vrx1_decoder = OpusDecoder::new()?;
        let mut vrx1_jitter_buf = JitterBuffer::new(3, 40);
        let mut vrx1_logged_first = false;
        // Start gedempt (0.0): VRX-audio wordt niet door de master-gain gedempt en
        // de client stuurt het opgeslagen VRX-volume pas op connect. Op 1.0 starten
        // gaf een harde geluids-piek bij opstart tot dat commando binnen was.
        let mut vrx1_volume: f32 = 0.0;
        let mut vrx2_decoder = OpusDecoder::new()?;
        let mut vrx2_jitter_buf = JitterBuffer::new(3, 40);
        let mut vrx2_logged_first = false;
        let mut vrx2_volume: f32 = 0.0; // gedempt starten — zie vrx1_volume
        let mk_sinc_params_vrx = || rubato::SincInterpolationParameters {
            sinc_len: 32, f_cutoff: 0.90, oversampling_factor: 32,
            interpolation: rubato::SincInterpolationType::Cubic,
            window: rubato::WindowFunction::Blackman,
        };
        let mut vrx1_resampler_out = rubato::SincFixedIn::<f32>::new(
            playback_rate as f64 / NETWORK_SAMPLE_RATE as f64,
            1.0,
            mk_sinc_params_vrx(),
            FRAME_SAMPLES,
            1,
        )
        .context("create VRX1 8k->device resampler")?;
        let mut vrx2_resampler_out = rubato::SincFixedIn::<f32>::new(
            playback_rate as f64 / NETWORK_SAMPLE_RATE as f64,
            1.0,
            mk_sinc_params_vrx(),
            FRAME_SAMPLES,
            1,
        )
        .context("create VRX2 8k->device resampler")?;
        // Wideband VRX path (16 kHz Opus). Switched per-frame based on
        // VrxAudioPacket.wideband flag.
        let mut vrx1_decoder_wb = OpusDecoderWideband::new()?;
        let mut vrx2_decoder_wb = OpusDecoderWideband::new()?;
        let mut vrx1_resampler_out_wb = rubato::SincFixedIn::<f32>::new(
            playback_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64,
            1.0,
            mk_sinc_params_vrx(),
            FRAME_SAMPLES_WIDEBAND,
            1,
        )
        .context("create VRX1 16k->device resampler")?;
        let mut vrx2_resampler_out_wb = rubato::SincFixedIn::<f32>::new(
            playback_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64,
            1.0,
            mk_sinc_params_vrx(),
            FRAME_SAMPLES_WIDEBAND,
            1,
        )
        .context("create VRX2 16k->device resampler")?;

        // Anti-alias parameters voor de TX-capture decimatie 48 → 16 kHz.
        // Sinds build 29: identiek aan yaesu_tx_resampler — brede USB-mics
        // (NT-USB e.d.) hebben content tot 16 kHz die anders terug-alias
        // in 0-8 kHz en op de ANAN/Thetis TX-uitgang hoorbaar wordt
        // bij FM/AM-TX (SSB blijft binnen 3 kHz dus subtieler).
        // Comment-naam (`device->8k`) blijft historisch; pad encodeert
        // wel naar 16 kHz wideband Opus (zie `OpusEncoderWideband`).
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
        let mut yaesu_mem_data_clear_at: Option<Instant> = None;
        let mut yaesu2_mem_data_clear_at: Option<Instant> = None;
        let mut tx_sequence: u32 = 0;
        let mut hb_sequence: u32 = 0;
        let mut ptt = false;
        let mut thetis_ptt = false;
        let mut yaesu_ptt = false;
        // Slot-1 PTT (dual-radio). Mutueel exclusief met yaesu_ptt in de praktijk
        // (één mic) → de mic-TX-keten kiest het packet-type op basis van welke aan staat.
        let mut yaesu2_ptt = false;
        let mut last_ptt = false;
        let mut ptt_burst_remaining: u32 = 0;
        let mut mic_gate_delay_ms: u32 = 0;
        let mut last_capture_ptt = false;
        let mut last_hb_sent = Instant::now();
        let mut last_hb_ack_time: Option<Instant> = None;
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
        let mut yaesu_agc = TxAgc::new(); // aparte AGC-envelope voor de Yaesu-TX-tak (deel B)
        let mut yaesu_tx_agc_enabled = false; // eigen Yaesu-AGC-toggle (los van Thetis agc_enabled)
        let mut yaesu_compressor = TxCompressor::new(); // client-side spraakcompressor (USB-TX)
        // Radio 2 (FTX-1): eigen compressor/AGC zodat de TX-keten per radio instelbaar is
        // (net als de per-radio EQ). PTT is mutueel exclusief → per-PTT de juiste keten.
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
        // True zolang we voor een WAV-playback naar de hoofdradio de Thetis-TXEQ
        // hebben uitgezet (om te herstellen bij stop/PTT-los/afloop).
        let mut thetis_txeq_bypassed: bool = false;

        let mut yaesu_volume: f32 = 0.5;   // Yaesu audio volume (client-only)
        // Slot-1 volume start GEDEMPT (0.0) — verplicht per les uit build 88
        // (VRX-piek bij opstart, project_audio_stutter_diagnose): elk nieuw
        // audiokanaal start muted tot de UI/effectieve volume binnen is.
        let mut yaesu2_volume: f32 = 0.0;
        let mut yaesu_local_mic_gain: f32 = 0.2; // Local Yaesu mic gain (before Opus encoding)
        let mut yaesu_eq = crate::eq::Equalizer::new(48000.0); // EQ at capture rate
        // Slot-1 (FTX-1) eigen TX-mic-EQ + gain — toegepast wanneer op radio 2
        // wordt gezonden (PTT mutueel exclusief, dus per-PTT gekozen in de encode-keten).
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

        // Bandbreedte-monitor (down/up Kbit/s) over een rolling ~500 ms venster.
        // RX-bytes worden bij elke recv_from() opgeteld; TX-bytes via de
        // `send_tx!`-macro die elke send_to-call-site omhult. Bij elke
        // window-rollover wordt de kbps berekend en in `state.down_kbps`/
        // `up_kbps` geschreven — weergegeven in de Server-tab Statistics-grid.
        let mut bw_window_start = Instant::now();
        let mut bw_rx_bytes: u64 = 0;
        let mut bw_tx_bytes: u64 = 0;
        // Per-PacketType byte-counter voor de RX-stream — geïndexeerd op
        // de `packet_type` byte (data[2]). Elke 5 s wordt een top-5
        // overzicht naar info! gelogd zodat de operator kan zien welke
        // stream het meeste verbruikt (zonder UI-uitbreiding).
        let mut bw_by_type: [u64; 256] = [0; 256];
        let mut bw_breakdown_start = Instant::now();
        // Lokale macro: wraps socket.send_to(buf, addr).await en telt buf-bytes
        // bij bw_tx_bytes. Vervangt de 80+ inline call-sites in deze functie
        // zonder per-site instrumentatie. Identifiers `socket` en `bw_tx_bytes`
        // worden bij invocation in de huidige scope geresolveerd.
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
                        // In relay-modus is er geen direct server-IP: het adres is een
                        // display-label (genegeerd door de Relay-transport). Sla de
                        // directe-IP-DNS/parse-check dan over (security addendum fix).
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
                        local_volume = v;
                    }
                    Command::SetVfoAVolume(v) => {
                        vfo_a_volume = v;
                    }
                    Command::SetTxGain(v) => {
                        tx_gain = v;
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
                                    // Rebuild resamplers with new sample rates
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
                                        // Yaesu TX heeft een scherpere anti-alias filter nodig
                                        // dan de RX-resamplers: brede USB-mics (NT-USB) hebben
                                        // content tot 16 kHz die anders terug-alias in 0-8 kHz.
                                        // Zie initial create van yaesu_tx_resampler boven.
                                        let mksp_aa = || rubato::SincInterpolationParameters {
                                            sinc_len: 128, f_cutoff: 0.95, oversampling_factor: 128,
                                            interpolation: rubato::SincInterpolationType::Cubic,
                                            window: rubato::WindowFunction::Blackman,
                                        };
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mksp(), FRAME_SAMPLES, 1) { res_rx1_out = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mksp(), FRAME_SAMPLES, 1) { res_bin_r_out = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mksp(), FRAME_SAMPLES, 1) { res_rx2_out = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mksp(), FRAME_SAMPLES, 1) { yaesu_res_nb = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64, 1.0, mksp(), FRAME_SAMPLES_WIDEBAND, 1) { yaesu_res_wb = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mksp(), FRAME_SAMPLES, 1) { yaesu2_res_nb = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64, 1.0, mksp(), FRAME_SAMPLES_WIDEBAND, 1) { yaesu2_res_wb = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(NETWORK_SAMPLE_RATE_WIDEBAND as f64 / capture_rate as f64, 1.0, mksp_aa(), capture_frame_samples, 1) { resampler_in = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(NETWORK_SAMPLE_RATE_WIDEBAND as f64 / capture_rate as f64, 1.0, mksp_aa(), capture_frame_samples, 1) { yaesu_tx_resampler = r; }
                                        // Rebuild WB Thetis-RX resamplers (opt-in pad).
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64, 1.0, mksp(), FRAME_SAMPLES_WIDEBAND, 1) { res_rx1_out_wb = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64, 1.0, mksp(), FRAME_SAMPLES_WIDEBAND, 1) { res_bin_r_out_wb = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64, 1.0, mksp(), FRAME_SAMPLES_WIDEBAND, 1) { res_rx2_out_wb = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mksp(), FRAME_SAMPLES, 1) { wav_res_out = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64, 1.0, mksp(), FRAME_SAMPLES_WIDEBAND, 1) { wav_res_out_wb = r; }
                                        info!("Resamplers rebuilt: capture {}Hz, playback {}Hz", capture_rate, playback_rate);
                                    }
                                    // Reset all jitter buffers to prevent stale frame buildup
                                    jitter_buf.reset();
                                    yaesu_jitter_buf.reset();
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
                                        // Yaesu TX scherpere anti-alias (zie initial create).
                                        let mksp_aa = || rubato::SincInterpolationParameters {
                                            sinc_len: 128, f_cutoff: 0.95, oversampling_factor: 128,
                                            interpolation: rubato::SincInterpolationType::Cubic,
                                            window: rubato::WindowFunction::Blackman,
                                        };
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mksp(), FRAME_SAMPLES, 1) { res_rx1_out = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mksp(), FRAME_SAMPLES, 1) { res_bin_r_out = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mksp(), FRAME_SAMPLES, 1) { res_rx2_out = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mksp(), FRAME_SAMPLES, 1) { yaesu_res_nb = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64, 1.0, mksp(), FRAME_SAMPLES_WIDEBAND, 1) { yaesu_res_wb = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mksp(), FRAME_SAMPLES, 1) { yaesu2_res_nb = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64, 1.0, mksp(), FRAME_SAMPLES_WIDEBAND, 1) { yaesu2_res_wb = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(NETWORK_SAMPLE_RATE_WIDEBAND as f64 / capture_rate as f64, 1.0, mksp_aa(), capture_frame_samples, 1) { resampler_in = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(NETWORK_SAMPLE_RATE_WIDEBAND as f64 / capture_rate as f64, 1.0, mksp_aa(), capture_frame_samples, 1) { yaesu_tx_resampler = r; }
                                        // Rebuild WB Thetis-RX resamplers (opt-in pad).
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64, 1.0, mksp(), FRAME_SAMPLES_WIDEBAND, 1) { res_rx1_out_wb = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64, 1.0, mksp(), FRAME_SAMPLES_WIDEBAND, 1) { res_bin_r_out_wb = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64, 1.0, mksp(), FRAME_SAMPLES_WIDEBAND, 1) { res_rx2_out_wb = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mksp(), FRAME_SAMPLES, 1) { wav_res_out = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE_WIDEBAND as f64, 1.0, mksp(), FRAME_SAMPLES_WIDEBAND, 1) { wav_res_out_wb = r; }
                                        info!("Resamplers rebuilt: capture {}Hz, playback {}Hz", capture_rate, playback_rate);
                                    }
                                    jitter_buf.reset();
                                    yaesu_jitter_buf.reset();
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
                        // WAV rate is set by the first write_samples() call; each
                        // source can therefore follow NB/WB decoder rate independently.
                        if rx1 {
                            let p = base.join(format!("RX1_{}.wav", ts));
                            match crate::wav::WavWriter::new(&p) {
                                Ok(w) => {
                                    info!("Recording RX1 to {}", p.display());
                                    state.last_recorded_path = Some(p.to_string_lossy().to_string());
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
                                    state.last_recorded_path = Some(p.to_string_lossy().to_string());
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
                                    state.last_recorded_path = Some(p.to_string_lossy().to_string());
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
                                    state.last_recorded_path = Some(p.to_string_lossy().to_string());
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
                                    state.last_recorded_path = Some(p.to_string_lossy().to_string());
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
                                    state.last_recorded_path = Some(p.to_string_lossy().to_string());
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
                    Command::PlayRecording { path } => {
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
                        playback_is_tx = false;
                        state.playing = false;
                        info!("Playback stopped");
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
                            // Lokale UI-cache wissen zodat oude spots niet
                            // blijven hangen na opt-out.
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
                    // RX2 / VFO-B commands
                    Command::SetRx2Enabled(enabled) => {
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
                        // Client-side spraakcompressor radio 1 (0-100 → 0.0-1.0).
                        yaesu_compressor.set_amount(level as f32 / 100.0);
                        info!("Yaesu compressor: {}", level);
                    }
                    Command::SetYaesuTxAgc(on) => {
                        yaesu_tx_agc_enabled = on;
                        info!("Yaesu TX AGC: {}", if on { "ON" } else { "OFF" });
                    }
                    Command::SetYaesu2Compressor(level) => {
                        // Client-side spraakcompressor radio 2 (FTX-1).
                        yaesu2_compressor.set_amount(level as f32 / 100.0);
                        info!("Yaesu2 compressor: {}", level);
                    }
                    Command::SetYaesu2TxAgc(on) => {
                        yaesu2_tx_agc_enabled = on;
                        info!("Yaesu2 TX AGC: {}", if on { "ON" } else { "OFF" });
                    }
                    Command::SetYaesuFreq(hz) => {
                        if let Some(ref addr) = server_addr {
                            let pkt = FrequencyPacket { frequency_hz: hz };
                            let mut buf = [0u8; FrequencyPacket::SIZE];
                            pkt.serialize_as_type(&mut buf, PacketType::FrequencyYaesu);
                            let _ = send_tx!(&buf, addr.as_str());
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
                        // Idem radio 2: YaesuMemoryData2-packet + Yaesu2WriteMemories-trigger.
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
                        // FTX-1 EX-set: reist als YaesuMemoryData2 met "SETMENU:"-prefix
                        // (spiegelt het 991A SetYaesuMenu-pad, maar 6-cijferig adres).
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
                        if let Some(ref addr) = server_addr {
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
                        yaesu2_ptt = on;
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket { control_id: ControlId::Yaesu2Ptt, value: on as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
                    }
                    Command::SetYaesu2Freq(hz) => {
                        if let Some(ref addr) = server_addr {
                            let pkt = FrequencyPacket { frequency_hz: hz };
                            let mut buf = [0u8; FrequencyPacket::SIZE];
                            pkt.serialize_as_type(&mut buf, PacketType::FrequencyYaesu2);
                            let _ = send_tx!(&buf, addr.as_str());
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
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket { control_id: ControlId::VrxEnable, value: on as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
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
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket { control_id: ControlId::VrxEnable2, value: on as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = send_tx!(&buf, addr.as_str());
                        }
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
                                    dec_rx1 = OpusDecoder::new()?;
                                    dec_bin_r = OpusDecoder::new()?;
                                    dec_rx2 = OpusDecoder::new()?;
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

                                    // Re-send RX2 state on reconnect
                                    if state.rx2_enabled {
                                        let mut rx2_buf = [0u8; ControlPacket::SIZE];
                                        let ctrl = ControlPacket { control_id: ControlId::Rx2Enable, value: 1 };
                                        ctrl.serialize(&mut rx2_buf);
                                        let _ = send_tx!(&rx2_buf, addr.as_str());

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
                                        info!("RX2 state re-sent on reconnect");
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
                                    // to default ON on every new insert, dus zonder dit pad
                                    // zou de client visueel OFF tonen terwijl de server na
                                    // reconnect weer Spot-frames stuurt.
                                    let ctrl = ControlPacket {
                                        control_id: ControlId::DxSpotsEnabled,
                                        value: state.dx_spots_enabled as u16,
                                    };
                                    let mut dx_buf = [0u8; ControlPacket::SIZE];
                                    ctrl.serialize(&mut dx_buf);
                                    let _ = send_tx!(&dx_buf, addr.as_str());
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
                                // Client->server only (WAV-playback TXEQ-bypass); de server
                                // broadcast 'm nooit terug, dus hier no-op.
                                ControlId::ThetisTxeq => {}
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
                                | ControlId::SpectrumBinDepth => {}
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
                                ControlId::AllowZoomBelow2x => {} // handled client-side (setup-vink)
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
                                // Dual-radio slot 1 (Optie B-prime): client→server only;
                                // server echoes ignored (zelfde patroon als slot-0 Yaesu + Vrx).
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
                            // connected komt NIET meer hieruit — presence-authority is
                            // nu YaesuPresence (PATCH-android-yaesu-presence-datasaver),
                            // zodat wegvallen/bijkomen dynamisch is i.p.v. sticky.
                            state.yaesu_freq_a = ys.freq_a;
                            state.yaesu_freq_b = ys.freq_b;
                            state.yaesu_mode = ys.mode;
                            state.yaesu_smeter = ys.smeter;
                            state.yaesu_tx_active = ys.tx_active;
                            state.yaesu_power_on = ys.power_on;
                            state.yaesu_af_gain = ys.af_gain;
                            state.yaesu_tx_power = ys.tx_power;
                            state.yaesu_squelch = ys.squelch;
                            state.yaesu_rf_gain = ys.rf_gain;
                            state.yaesu_mic_gain = ys.mic_gain;
                            state.yaesu_split = ys.split;
                            state.yaesu_scan = ys.scan;
                            state.yaesu_tuner_state = ys.tuner_state;
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
                                        info!(
                                            "VRX1 audio: first packet received (seq={}, opus_bytes={})",
                                            pkt.sequence, frame.opus_data.len()
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
                                        info!(
                                            "VRX2 audio: first packet received (seq={}, opus_bytes={})",
                                            pkt.sequence, frame.opus_data.len()
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
                        Ok(Packet::YaesuMemoryData(text)) => {
                            info!("Received Yaesu memory data ({}B)", text.len());
                            state.yaesu_memory_data = Some(text);
                            yaesu_mem_data_clear_at = Some(Instant::now() + Duration::from_millis(500));
                        }
                        Ok(Packet::AudioYaesu(pkt)) => {
                            // Detect stream reset (server resets seq to 0 on re-enable)
                            if yaesu_logged_first && pkt.sequence == 0 {
                                info!("Yaesu: stream reset detected, resetting jitter buffer");
                                yaesu_jitter_buf.reset();
                                yaesu_decoder_nb = OpusDecoder::new().unwrap_or_else(|e| {
                                    warn!("Yaesu decoder reset failed: {}", e);
                                    OpusDecoder::new().unwrap()
                                });
                                yaesu_decoder_wb = OpusDecoderWideband::new()
                                    .unwrap_or_else(|_| OpusDecoderWideband::new().unwrap());
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
                                    // RX-bandbreedte volgt de Thetis-toggle: de
                                    // AUDIO_WIDEBAND-flag bepaalt NB (8k) of WB (16k).
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
                        // Dual-radio slot 1 (Optie B-prime) — exacte spiegel van slot 0.
                        Ok(Packet::YaesuState2(ys)) => {
                            // connected komt NIET meer hieruit — zie YaesuPresence (3c).
                            state.yaesu2_freq_a = ys.freq_a;
                            state.yaesu2_freq_b = ys.freq_b;
                            state.yaesu2_mode = ys.mode;
                            state.yaesu2_smeter = ys.smeter;
                            state.yaesu2_tx_active = ys.tx_active;
                            state.yaesu2_power_on = ys.power_on;
                            state.yaesu2_af_gain = ys.af_gain;
                            state.yaesu2_tx_power = ys.tx_power;
                            state.yaesu2_squelch = ys.squelch;
                            state.yaesu2_rf_gain = ys.rf_gain;
                            state.yaesu2_mic_gain = ys.mic_gain;
                            state.yaesu2_split = ys.split;
                            state.yaesu2_scan = ys.scan;
                            state.yaesu2_tuner_state = ys.tuner_state;
                            state.yaesu2_vfo_select = ys.vfo_select;
                            state.yaesu2_memory_channel = ys.memory_channel;
                        }
                        Ok(Packet::AudioYaesu2(pkt)) => {
                            if yaesu2_logged_first && pkt.sequence == 0 {
                                info!("[radio1] stream reset detected, resetting jitter buffer");
                                yaesu2_jitter_buf.reset();
                                yaesu2_decoder_nb = OpusDecoder::new().unwrap_or_else(|e| {
                                    warn!("[radio1] decoder reset failed: {}", e);
                                    OpusDecoder::new().unwrap()
                                });
                                yaesu2_decoder_wb = OpusDecoderWideband::new()
                                    .unwrap_or_else(|_| OpusDecoderWideband::new().unwrap());
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
                                    wideband: pkt.flags.wideband(), // RX volgt Thetis-toggle
                                },
                                arrival_ms,
                            );
                            state.yaesu2_audio_packets += 1;
                            state.yaesu2_jitter_ms = yaesu2_jitter_buf.jitter_ms();
                            state.yaesu2_buffer_depth = yaesu2_jitter_buf.depth() as u32;
                        }
                        Ok(Packet::RadioInfo { slot, model }) => {
                            // Per-radio model voor paneel-naamgeving ("991A 1"/"FTX1").
                            if slot == 0 { state.yaesu_model = model; }
                            else if slot == 1 { state.yaesu2_model = model; }
                        }
                        Ok(Packet::YaesuPresence(p)) => {
                            // Presence-authority (PATCH-android-yaesu-presence-datasaver):
                            // connected + model komen hiervandaan (broadcast naar álle
                            // clients), NIET uit de subscription-gated YaesuState. Pure
                            // toepassing in apply_yaesu_presence (unit-tested).
                            let (c0, c1) = apply_yaesu_presence(&mut state, &p);
                            // Value-change-only logging (L4) — gedeeld, dus ook desktop.
                            if c0 { info!("[radio0] presence: {}", if p.slot0_present { "connected" } else { "disconnected" }); }
                            if c1 { info!("[radio1] presence: {}", if p.slot1_present { "connected" } else { "disconnected" }); }
                        }
                        Ok(Packet::FrequencyYaesu2(_)) => {} // client→server only
                        Ok(Packet::YaesuMemoryData2(text)) => {
                            info!("[radio1] received memory data ({}B)", text.len());
                            state.yaesu2_memory_data = Some(text);
                            yaesu2_mem_data_clear_at = Some(Instant::now() + Duration::from_millis(500));
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

                        if !skip_this_tick {
                            loop {
                                if frames_this_tick >= max_pull {
                                    break;
                                }
                                // In refill mode, keep pulling until ring buffer is healthy
                                if frames_this_tick >= 1 && ring_level >= target_ring_low {
                                    break;
                                }

                                // Pull multi-channel frame from jitter buffer.
                                // Tuple-payload `(blob, wideband)` zodat de decoder bij
                                // pop weet op welk pad het frame thuishoort (WB = 16 kHz
                                // Opus i.p.v. 8 kHz). Default false voor frames die
                                // door FEC/PLC zijn opgevuld — die paden blijven NB.
                                let frame_data: Option<(Vec<u8>, bool)> = match jitter_buf.pull() {
                                    JitterResult::Frame(frame) => {
                                        frames_this_tick += 1;
                                        if !frame.opus_data.is_empty() { Some((frame.opus_data, frame.wideband)) } else { None }
                                    }
                                    JitterResult::Missing => {
                                        frames_this_tick += 1;
                                        // FEC recovery: peek at the NEXT frame's CH0 opus data
                                        // to reconstruct the lost frame via in-band FEC.
                                        let next_seq = jitter_buf.next_seq_peek();
                                        let fec_data = next_seq.and_then(|s| jitter_buf.peek_opus_data(s));
                                        let rx1_fec_opus = fec_data.and_then(|blob| {
                                            // Extract CH0 opus from multi-channel blob
                                            if blob.is_empty() { return None; }
                                            let ch_count = blob[0] as usize;
                                            let mut pos = 1usize;
                                            for _ in 0..ch_count {
                                                if pos + 3 > blob.len() { break; }
                                                let ch_id = blob[pos];
                                                let len = u16::from_be_bytes([blob[pos+1], blob[pos+2]]) as usize;
                                                if ch_id == 0 && pos + 3 + len <= blob.len() {
                                                    return Some(&blob[pos+3..pos+3+len]);
                                                }
                                                pos += 3 + len;
                                            }
                                            None
                                        });

                                        let pcm = if let Some(fec_opus) = rx1_fec_opus {
                                            dec_rx1.decode_fec(fec_opus).ok()
                                        } else {
                                            dec_rx1.decode_plc().ok()
                                        };
                                        if let Some(pcm) = pcm {
                                            let resampled = resample_to_device(&mut res_rx1_out, &pcm);
                                            let mut dev = resampled;
                                            apply_volume(&mut dev, rx_volume * vfo_a_volume * local_volume);
                                            if !ptt { playback_buf.extend_from_slice(&dev); bin_r_buf.extend_from_slice(&dev); }
                                        }
                                        None
                                    }
                                    JitterResult::NotReady => {
                                        if was_connected && logged_first_rx {
                                            if let Ok(pcm) = dec_rx1.decode_plc() {
                                                let resampled = resample_to_device(&mut res_rx1_out, &pcm);
                                                let mut dev = resampled;
                                                apply_volume(&mut dev, rx_volume * vfo_a_volume * local_volume);
                                                if !ptt { playback_buf.extend_from_slice(&dev); bin_r_buf.extend_from_slice(&dev); }
                                            }
                                        }
                                        break;
                                    }
                                };

                                if let Some((blob, is_wb)) = frame_data {
                                    // Deserialize multi-channel blob
                                    let mut rx1_pcm: Option<Vec<i16>> = None;
                                    let mut bin_r_pcm: Option<Vec<i16>> = None;
                                    let mut rx2_pcm: Option<Vec<i16>> = None;

                                    if !blob.is_empty() {
                                        let ch_count = blob[0] as usize;
                                        let mut pos = 1usize;
                                        for _ in 0..ch_count {
                                            if pos + 3 > blob.len() { break; }
                                            let ch_id = blob[pos];
                                            let opus_len = u16::from_be_bytes([blob[pos+1], blob[pos+2]]) as usize;
                                            if pos + 3 + opus_len > blob.len() { break; }
                                            let opus = &blob[pos+3..pos+3+opus_len];
                                            // Decoder-pad keuze op basis van WB-flag van dit packet.
                                            // Yaesu-RX heeft een eigen jitter-buf (geen WB-pad daar).
                                            match ch_id {
                                                0 => {
                                                    rx1_pcm = if is_wb {
                                                        dec_rx1_wb.decode(opus).ok()
                                                    } else {
                                                        dec_rx1.decode(opus).ok()
                                                    };
                                                }
                                                1 => {
                                                    bin_r_pcm = if is_wb {
                                                        dec_bin_r_wb.decode(opus).ok()
                                                    } else {
                                                        dec_bin_r.decode(opus).ok()
                                                    };
                                                }
                                                2 => {
                                                    rx2_pcm = if is_wb {
                                                        dec_rx2_wb.decode(opus).ok()
                                                    } else {
                                                        dec_rx2.decode(opus).ok()
                                                    };
                                                }
                                                _ => {}
                                            }
                                            pos += 3 + opus_len;
                                        }
                                    }

                                    // Write decoded 8kHz PCM to WAV recorders
                                    let rec_rate = if is_wb { NETWORK_SAMPLE_RATE_WIDEBAND } else { NETWORK_SAMPLE_RATE };
                                    if let Some(ref mut w) = rec_rx1 {
                                        if let Some(ref pcm) = rx1_pcm {
                                            let _ = w.write_samples(pcm, rec_rate);
                                        }
                                    }
                                    if let Some(ref mut w) = rec_rx2 {
                                        if let Some(ref pcm) = rx2_pcm {
                                            let _ = w.write_samples(pcm, rec_rate);
                                        }
                                    }

                                    // Resample and route based on audio_mode.
                                    // Per-channel kiezen we de juiste resampler op basis
                                    // van het WB-vlag-pad (16k vs 8k input). Output is
                                    // altijd op playback_rate.
                                    // RX1 → always L
                                    let mut left_dev = if let Some(pcm) = rx1_pcm {
                                        let mut dev = if is_wb {
                                            resample_to_device(&mut res_rx1_out_wb, &pcm)
                                        } else {
                                            resample_to_device(&mut res_rx1_out, &pcm)
                                        };
                                        apply_volume(&mut dev, rx_volume * vfo_a_volume * local_volume);
                                        let sq: f32 = dev.iter().map(|s| s*s).sum();
                                        rx1_level_accum += sq;
                                        rx1_level_count += dev.len();
                                        dev
                                    } else { Vec::new() };

                                    // Resample RX2 once if available (reused in Mono, BIN, Split)
                                    let rx2_dev = if let Some(pcm) = &rx2_pcm {
                                        let mut dev = if is_wb {
                                            resample_to_device(&mut res_rx2_out_wb, pcm)
                                        } else {
                                            resample_to_device(&mut res_rx2_out, pcm)
                                        };
                                        let rx2_vol = rx2_volume * vfo_b_volume * local_volume;
                                        apply_volume(&mut dev, rx2_vol);
                                        let sq: f32 = dev.iter().map(|s| s*s).sum();
                                        rx2_level_accum += sq;
                                        rx2_level_count += dev.len();
                                        Some(dev)
                                    } else { None };

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
                                        // Android or Mono: L only → both ears
                                        Vec::new()
                                    } else if audio_mode == 1 {
                                        // BIN: R = binaural right (ch1), volume = RX1
                                        if let Some(pcm) = bin_r_pcm {
                                            let mut dev = if is_wb {
                                                resample_to_device(&mut res_bin_r_out_wb, &pcm)
                                            } else {
                                                resample_to_device(&mut res_bin_r_out, &pcm)
                                            };
                                            apply_volume(&mut dev, rx_volume * vfo_a_volume * local_volume);
                                            dev
                                        } else { left_dev.clone() } // fallback mono
                                    } else {
                                        // Split: R = RX2 directly
                                        rx2_dev.clone().unwrap_or_default()
                                    };

                                    // Measure BinR level BEFORE RX2 mix (pure RX1-R only)
                                    if audio_mode == 1 && !right_dev.is_empty() {
                                        let sq: f32 = right_dev.iter().map(|s| s * s).sum();
                                        bin_r_level_accum += sq;
                                        bin_r_level_count += right_dev.len();
                                    }

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
                                } // if let Some(blob)
                            }

                            // RX1 level (measured per-channel before mono mix)
                            if rx1_level_count > 0 {
                                state.playback_level = (rx1_level_accum / rx1_level_count as f32).sqrt();
                            }
                            // RX2 level (measured per-channel before mono mix)
                            if rx2_level_count > 0 {
                                state.playback_level_rx2 = (rx2_level_accum / rx2_level_count as f32).sqrt();
                            }

                            // Mix Yaesu audio (third channel, independent of RX1/RX2)
                            // Only process when there are Yaesu audio packets in the buffer
                            if yaesu_logged_first && yaesu_jitter_buf.depth() > 0 {
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
                                    // (pcm, wideband) — formaat per frame uit de flag; PLC
                                    // gebruikt het laatst-bekende formaat (yaesu_last_wb).
                                    let decoded: Option<(Vec<i16>, bool)> = match yaesu_jitter_buf.pull() {
                                        JitterResult::Frame(frame) => {
                                            if !frame.opus_data.is_empty() {
                                                yaesu_last_wb = frame.wideband;
                                                let r = if frame.wideband {
                                                    yaesu_decoder_wb.decode(&frame.opus_data)
                                                } else {
                                                    yaesu_decoder_nb.decode(&frame.opus_data)
                                                };
                                                match r {
                                                    Ok(pcm) => Some((pcm, frame.wideband)),
                                                    Err(e) => { warn!("Yaesu decode error: {}", e); None }
                                                }
                                            } else { None }
                                        }
                                        JitterResult::Missing => {
                                            let r = if yaesu_last_wb {
                                                yaesu_decoder_wb.decode_plc()
                                            } else {
                                                yaesu_decoder_nb.decode_plc()
                                            };
                                            match r { Ok(pcm) => Some((pcm, yaesu_last_wb)), Err(_) => None }
                                        }
                                        JitterResult::NotReady => None,
                                    };
                                    match decoded {
                                        Some((pcm, wb)) => {
                                            if let Some(ref mut w) = rec_yaesu {
                                                let rate = if wb { NETWORK_SAMPLE_RATE_WIDEBAND } else { NETWORK_SAMPLE_RATE };
                                                let _ = w.write_samples(&pcm, rate);
                                            }
                                            let mut resampled = if wb {
                                                resample_to_device(&mut yaesu_res_wb, &pcm)
                                            } else {
                                                resample_to_device(&mut yaesu_res_nb, &pcm)
                                            };
                                            apply_volume(&mut resampled, yaesu_volume * 20.0);
                                            yaesu_buf.extend_from_slice(&resampled);
                                        }
                                        None => break,
                                    }
                                }
                                // Measure Yaesu level before mixing
                                if !yaesu_buf.is_empty() {
                                    let sum_sq: f32 = yaesu_buf.iter().map(|s| s * s).sum();
                                    state.playback_level_yaesu = (sum_sq / yaesu_buf.len() as f32).sqrt();
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

                            // Mix slot-1 (dual-radio) audio — exacte spiegel van slot 0,
                            // eigen jitter-buf/decoder/resampler + muted-start volume.
                            if yaesu2_logged_first && yaesu2_jitter_buf.depth() > 0 {
                                let target_samples = if playback_buf.is_empty() {
                                    let frame_size = (playback_rate as usize * 20) / 1000;
                                    playback_buf.resize(frame_size, 0.0);
                                    frame_size
                                } else {
                                    playback_buf.len()
                                };
                                let mut yaesu2_buf: Vec<f32> = Vec::with_capacity(target_samples);
                                while yaesu2_buf.len() < target_samples {
                                    let decoded: Option<(Vec<i16>, bool)> = match yaesu2_jitter_buf.pull() {
                                        JitterResult::Frame(frame) => {
                                            if !frame.opus_data.is_empty() {
                                                yaesu2_last_wb = frame.wideband;
                                                let r = if frame.wideband {
                                                    yaesu2_decoder_wb.decode(&frame.opus_data)
                                                } else {
                                                    yaesu2_decoder_nb.decode(&frame.opus_data)
                                                };
                                                match r {
                                                    Ok(pcm) => Some((pcm, frame.wideband)),
                                                    Err(e) => { warn!("[radio1] decode error: {}", e); None }
                                                }
                                            } else { None }
                                        }
                                        JitterResult::Missing => {
                                            let r = if yaesu2_last_wb {
                                                yaesu2_decoder_wb.decode_plc()
                                            } else {
                                                yaesu2_decoder_nb.decode_plc()
                                            };
                                            match r { Ok(pcm) => Some((pcm, yaesu2_last_wb)), Err(_) => None }
                                        }
                                        JitterResult::NotReady => None,
                                    };
                                    match decoded {
                                        Some((pcm, wb)) => {
                                            if let Some(ref mut w) = rec_yaesu2 {
                                                let rate = if wb { NETWORK_SAMPLE_RATE_WIDEBAND } else { NETWORK_SAMPLE_RATE };
                                                let _ = w.write_samples(&pcm, rate);
                                            }
                                            let mut resampled = if wb {
                                                resample_to_device(&mut yaesu2_res_wb, &pcm)
                                            } else {
                                                resample_to_device(&mut yaesu2_res_nb, &pcm)
                                            };
                                            apply_volume(&mut resampled, yaesu2_volume * 20.0);
                                            yaesu2_buf.extend_from_slice(&resampled);
                                        }
                                        None => break,
                                    }
                                }
                                if !yaesu2_buf.is_empty() {
                                    let sum_sq: f32 = yaesu2_buf.iter().map(|s| s * s).sum();
                                    state.playback_level_yaesu2 = (sum_sq / yaesu2_buf.len() as f32).sqrt();
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
                                state.playback_level_vrx1 *= 0.7;
                                if state.playback_level_vrx1 < 0.001 {
                                    state.playback_level_vrx1 = 0.0;
                                }
                            }
                            if vrx1_logged_first && vrx1_jitter_buf.depth() > 0 {
                                let target_samples = if playback_buf.is_empty() {
                                    let frame_size = (playback_rate as usize * 20) / 1000;
                                    playback_buf.resize(frame_size, 0.0);
                                    frame_size
                                } else {
                                    playback_buf.len()
                                };
                                let mut vrx_buf: Vec<f32> = Vec::with_capacity(target_samples);
                                while vrx_buf.len() < target_samples {
                                    let decoded: Option<(Vec<i16>, bool)> = match vrx1_jitter_buf.pull() {
                                        JitterResult::Frame(frame) => {
                                            if !frame.opus_data.is_empty() {
                                                let res = if frame.wideband {
                                                    vrx1_decoder_wb.decode(&frame.opus_data)
                                                } else {
                                                    vrx1_decoder.decode(&frame.opus_data)
                                                };
                                                match res {
                                                    Ok(pcm) => Some((pcm, frame.wideband)),
                                                    Err(e) => { warn!("VRX1 decode error: {}", e); None }
                                                }
                                            } else { None }
                                        }
                                        JitterResult::Missing => {
                                            match vrx1_decoder.decode_plc() {
                                                Ok(pcm) => Some((pcm, false)),
                                                Err(_) => None,
                                            }
                                        }
                                        JitterResult::NotReady => None,
                                    };
                                    match decoded {
                                        Some((pcm, is_wb)) => {
                                            if let Some(ref mut w) = rec_vrx1 {
                                                let rate = if is_wb { NETWORK_SAMPLE_RATE_WIDEBAND } else { NETWORK_SAMPLE_RATE };
                                                let _ = w.write_samples(&pcm, rate);
                                            }
                                            let mut resampled = if is_wb {
                                                resample_to_device(&mut vrx1_resampler_out_wb, &pcm)
                                            } else {
                                                resample_to_device(&mut vrx1_resampler_out, &pcm)
                                            };
                                            apply_volume(&mut resampled, vrx1_volume);
                                            vrx_buf.extend_from_slice(&resampled);
                                        }
                                        None => break,
                                    }
                                }
                                if !vrx_buf.is_empty() {
                                    let sum_sq: f32 = vrx_buf.iter().map(|s| s * s).sum();
                                    state.playback_level_vrx1 = (sum_sq / vrx_buf.len() as f32).sqrt();
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
                                state.playback_level_vrx2 *= 0.7;
                                if state.playback_level_vrx2 < 0.001 {
                                    state.playback_level_vrx2 = 0.0;
                                }
                            }
                            if vrx2_logged_first && vrx2_jitter_buf.depth() > 0 {
                                let target_samples = if playback_buf.is_empty() {
                                    let frame_size = (playback_rate as usize * 20) / 1000;
                                    playback_buf.resize(frame_size, 0.0);
                                    frame_size
                                } else {
                                    playback_buf.len()
                                };
                                let mut vrx_buf: Vec<f32> = Vec::with_capacity(target_samples);
                                while vrx_buf.len() < target_samples {
                                    let decoded: Option<(Vec<i16>, bool)> = match vrx2_jitter_buf.pull() {
                                        JitterResult::Frame(frame) => {
                                            if !frame.opus_data.is_empty() {
                                                let res = if frame.wideband {
                                                    vrx2_decoder_wb.decode(&frame.opus_data)
                                                } else {
                                                    vrx2_decoder.decode(&frame.opus_data)
                                                };
                                                match res {
                                                    Ok(pcm) => Some((pcm, frame.wideband)),
                                                    Err(e) => { warn!("VRX2 decode error: {}", e); None }
                                                }
                                            } else { None }
                                        }
                                        JitterResult::Missing => {
                                            match vrx2_decoder.decode_plc() {
                                                Ok(pcm) => Some((pcm, false)),
                                                Err(_) => None,
                                            }
                                        }
                                        JitterResult::NotReady => None,
                                    };
                                    match decoded {
                                        Some((pcm, is_wb)) => {
                                            if let Some(ref mut w) = rec_vrx2 {
                                                let rate = if is_wb { NETWORK_SAMPLE_RATE_WIDEBAND } else { NETWORK_SAMPLE_RATE };
                                                let _ = w.write_samples(&pcm, rate);
                                            }
                                            let mut resampled = if is_wb {
                                                resample_to_device(&mut vrx2_resampler_out_wb, &pcm)
                                            } else {
                                                resample_to_device(&mut vrx2_resampler_out, &pcm)
                                            };
                                            apply_volume(&mut resampled, vrx2_volume);
                                            vrx_buf.extend_from_slice(&resampled);
                                        }
                                        None => break,
                                    }
                                }
                                if !vrx_buf.is_empty() {
                                    let sum_sq: f32 = vrx_buf.iter().map(|s| s * s).sum();
                                    state.playback_level_vrx2 = (sum_sq / vrx_buf.len() as f32).sqrt();
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
                                state.playback_level_bin_r = 0.0;
                            }

                            // WAV speaker playback (when not TX)
                            if !playback_is_tx && playback_wav.is_some() {
                                let wav = playback_wav.as_ref().unwrap();
                                let samples_per_tick = if playback_wav_rate == NETWORK_SAMPLE_RATE_WIDEBAND {
                                    FRAME_SAMPLES_WIDEBAND
                                } else {
                                    FRAME_SAMPLES
                                };
                                let remaining = wav.len() - playback_pos;
                                let to_read = samples_per_tick.min(remaining);
                                if to_read > 0 {
                                    let mut pcm: Vec<i16> = wav[playback_pos..playback_pos + to_read].to_vec();
                                    if pcm.len() < samples_per_tick {
                                        pcm.resize(samples_per_tick, 0);
                                    }
                                    let resampled = if playback_wav_rate == NETWORK_SAMPLE_RATE_WIDEBAND {
                                        resample_to_device(&mut wav_res_out_wb, &pcm)
                                    } else {
                                        resample_to_device(&mut wav_res_out, &pcm)
                                    };
                                    let target = resampled.len();
                                    playback_buf.clear();
                                    bin_r_buf.clear();
                                    playback_buf.resize(target, 0.0);
                                    bin_r_buf.resize(target, 0.0);
                                    for (i, &s) in resampled.iter().enumerate() {
                                        // play_volume-schuif ook op speaker-playback.
                                        let sample = (s * local_volume * play_volume).clamp(-1.0, 1.0);
                                        playback_buf[i] = sample;
                                        bin_r_buf[i] = sample;
                                    }
                                    playback_pos += to_read;
                                }
                                if playback_pos >= wav.len() {
                                    info!("WAV speaker playback finished");
                                    playback_wav = None;
                                    playback_pos = 0;
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
                            // Had packets before, now nothing - 100% loss
                            smoothed_loss = smoothed_loss * 0.7 + 100.0 * 0.3;
                            current_loss_percent = smoothed_loss.round() as u8;
                        }
                        state.loss_percent = current_loss_percent;
                        loss_window_received = 0;
                        loss_window_max_seq = None;

                        // Bandbreedte-window flush — synchroon met de heartbeat-
                        // tick (~500 ms). bytes × 8 / venster_ms = bits/ms = kbps.
                        let win_ms = bw_window_start.elapsed().as_millis().max(1) as u64;
                        state.down_kbps = (bw_rx_bytes.saturating_mul(8) / win_ms) as u32;
                        state.up_kbps = (bw_tx_bytes.saturating_mul(8) / win_ms) as u32;
                        bw_rx_bytes = 0;
                        bw_tx_bytes = 0;
                        bw_window_start = Instant::now();

                        // Per-PacketType breakdown elke 5 s — gepublishd naar
                        // `state.bw_breakdown` zodat de Server-tab in de UI
                        // een uitklap-detail kan tonen zonder logspam.
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

                    // Update capture level
                    state.capture_level = audio.capture_level();

                    // Thetis-TXEQ-bypass tijdens WAV-playback naar de HOOFDRADIO (Thetis-PTT):
                    // zet de mic-profiel-TX-EQ uit tijdens Play en herstel bij stop/PTT-los/
                    // afloop - net als Thetis' eigen record/playback. Alleen Thetis (ptt); de
                    // Yaesu-EQ wordt al client-side overgeslagen. Edge-getriggerd via de flag.
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
                        // Aantal WAV-samples per 20 ms bij de HEADER-rate (8k->160,
                        // 16k->320). Voorheen stond hier vast FRAME_SAMPLES (8k) plus
                        // een blinde sample-duplicatie naar "16k"; een 16 kHz-opname
                        // speelde daardoor half zo snel af, en de Yaesu-tak stopte
                        // ruwe 8/16k-samples in een capture-rate (48k) accumulator
                        // -> 3-6x te langzaam + hakkelen. Nu rate-aware.
                        let samples_per_tick = (playback_wav_rate as usize * 20) / 1000;
                        let remaining = wav.len() - playback_pos;
                        let to_read = samples_per_tick.min(remaining);
                        if to_read > 0 {
                            // play_volume-schuif: schaalt de opgenomen WAV. Toegepast op
                            // src_f32 zodat zowel de meter (RMS hieronder) als beide
                            // TX-takken (Thetis + Yaesu) het aangepaste niveau volgen.
                            let src_f32: Vec<f32> = wav[playback_pos..playback_pos + to_read]
                                .iter()
                                .map(|&s| (s as f32 / 32768.0) * play_volume)
                                .collect();

                            // Meter tijdens Play: toon het niveau van de UITGEZONDEN WAV
                            // (niet de gemute mic). RMS van dit 20ms-blok naar de juiste
                            // bar (Thetis of Yaesu). De drain hieronder overschrijft
                            // yaesu_mic_level anders met de gemute mic / geschaalde WAV -
                            // dat is gegate op !playback_is_tx.
                            let wav_rms = if src_f32.is_empty() {
                                0.0
                            } else {
                                (src_f32.iter().map(|s| s * s).sum::<f32>()
                                    / src_f32.len() as f32)
                                    .sqrt()
                            };
                            if ptt {
                                state.capture_level = wav_rms;
                            }
                            if yaesu_ptt || yaesu2_ptt {
                                state.yaesu_mic_level = wav_rms;
                            }

                            // Thetis-hoofdradio TX: resample header-rate -> 16 kHz voor
                            // de wideband Opus-encoder. Alleen als Thetis-PTT actief is.
                            if ptt {
                                let f16 = resample_linear(
                                    &src_f32,
                                    playback_wav_rate,
                                    NETWORK_SAMPLE_RATE_WIDEBAND,
                                );
                                if f16.len() >= FRAME_SAMPLES_WIDEBAND {
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

                            // Yaesu TX: voer op capture_rate in yaesu_tx_accum. GEEN pre-
                            // attenuatie: de drain slaat voor WAV-playback de mic-keten over
                            // (compressor/AGC + de 4x mic-boost). Die 4x + comp/AGC zijn
                            // bedoeld voor een STILLE, dynamische mic; een opgenomen WAV zit
                            // al op lijn-niveau. De AGC zou de WAV terug omhoog normaliseren
                            // waarna de 4x alsnog clipt (vervorming). Nu gaat de WAV clean als
                            // lijn-niveau door de keten: alleen play_volume + mic_gain.
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

                        let pcm_i16: Vec<i16> = pcm_8k
                            .iter()
                            .map(|&s| (s * tx_gain * 32767.0).clamp(-32768.0, 32767.0) as i16)
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
                    // Geldt voor beide radio's (PTT mutueel exclusief); per-PTT
                    // worden hieronder de juiste EQ + mic-gain gekozen.
                    if yaesu_ptt || yaesu2_ptt {
                        // Resample to 16kHz, encode wideband Opus
                        while yaesu_tx_accum.len() >= capture_frame_samples {
                            let mut chunk: Vec<f32> = yaesu_tx_accum.drain(..capture_frame_samples).collect();

                            // Apply 5-band EQ at capture rate (before resampling).
                            // Per-radio: slot-1 PTT → radio-2 EQ, anders radio-1 EQ.
                            // ALLEEN voor de live mic: de TX-mic-EQ hoort bij de microfoon,
                            // niet bij een directe WAV-playback (die moet klinken zoals
                            // opgenomen). Dus overslaan tijdens playback_is_tx.
                            if !playback_is_tx {
                                if yaesu2_ptt {
                                    yaesu2_eq.process(&mut chunk);
                                } else {
                                    yaesu_eq.process(&mut chunk);
                                }
                            }

                            // Measure Yaesu mic level (after EQ) for the UI meter.
                            // Tijdens WAV-Play niet overschrijven: dan toont de meter het
                            // uitgezonden WAV-niveau (in het WAV-blok gezet), niet de
                            // gemute mic / de 1/4-geschaalde WAV-chunk.
                            if !playback_is_tx {
                                let level_sum_sq: f32 = chunk.iter().map(|s| s * s).sum();
                                state.yaesu_mic_level = if chunk.is_empty() {
                                    0.0
                                } else {
                                    (level_sum_sq / chunk.len() as f32).sqrt()
                                };
                            }

                            // Resample to 16kHz and apply Yaesu-specific mic gain before Opus.
                            let mic_gain = if yaesu2_ptt { yaesu2_local_mic_gain } else { yaesu_local_mic_gain };
                            let mut resampled = resample_to_network(&mut yaesu_tx_resampler, &chunk);
                            // Client-side TX-keten (radio-processing werkt niet op USB):
                            // compressor → AGC, vóór de handmatige tx_gain/mic_gain-trim.
                            // Per radio (net als de EQ): slot-1 PTT → radio-2 keten.
                            // Compressor werkt zelf-gated op amount; AGC op de eigen toggle.
                            // Mic-keten (compressor + AGC) ALLEEN voor live mic, niet voor
                            // WAV-playback (die is al lijn-niveau; comp/AGC zou 'm vervormen).
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
                            // Live mic krijgt de 4x stille-mic-boost; WAV-playback gaat op
                            // lijn-niveau (alleen mic_gain, geen 4x) zodat 'ie niet clipt.
                            let final_scale = if playback_is_tx { mic_gain } else { mic_gain * 4.0 };
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
                                        // Slot-1 PTT → AudioYaesu2, anders slot-0 AudioYaesu.
                                        let tx_ptype = if yaesu2_ptt {
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
                            thetis_txeq_bypassed = false;
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
/// Simpele lineaire resample voor WAV-TX-playback (recorded-message replay).
/// Niet latency-/HF-kritisch, dus lineaire interpolatie volstaat en is
/// stateless (geen resampler-state per tick). Gebruikt om een opgenomen WAV van
/// zijn header-rate (8 of 16 kHz) naar de doel-rate te brengen: 16 kHz voor de
/// Thetis-TX-encoder, of `capture_rate` voor de Yaesu-TX-accumulator.
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

/// Apply volume scaling to audio samples
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

    // Regression: de presence-flip is de kern-eis — presence bepaalt connected,
    // dynamisch true/false. Test de pure apply_yaesu_presence-seam.
    #[test]
    fn presence_drives_connected_flip() {
        let mut state = RadioState::default();
        assert!(!state.yaesu_connected);
        assert!(!state.yaesu2_connected);

        // Beide aanwezig → beide connected, models overgenomen, beide changed.
        let (c0, c1) = apply_yaesu_presence(&mut state, &YaesuPresencePacket {
            slot0_present: true, slot0_model: 0, slot1_present: true, slot1_model: 1,
        });
        assert!(c0 && c1);
        assert!(state.yaesu_connected && state.yaesu2_connected);
        assert_eq!(state.yaesu_model, 0);
        assert_eq!(state.yaesu2_model, 1);

        // Slot 0 valt weg → connected flipt naar false (kern van de dynamiek),
        // slot 1 ongewijzigd (geen change-flag).
        let (c0, c1) = apply_yaesu_presence(&mut state, &YaesuPresencePacket {
            slot0_present: false, slot0_model: 0, slot1_present: true, slot1_model: 1,
        });
        assert!(c0 && !c1);
        assert!(!state.yaesu_connected);
        assert!(state.yaesu2_connected);

        // Idempotent: zelfde presence nogmaals → geen change-flags.
        let (c0, c1) = apply_yaesu_presence(&mut state, &YaesuPresencePacket {
            slot0_present: false, slot0_model: 0, slot1_present: true, slot1_model: 1,
        });
        assert!(!c0 && !c1);
    }
}
