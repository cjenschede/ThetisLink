// SPDX-License-Identifier: GPL-2.0-or-later

#![allow(dead_code)]
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use log::{info, warn};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};
use tokio::time::{interval, Duration};

use sdr_remote_core::codec::{OpusDecoder, OpusEncoderWideband};
use sdr_remote_core::jitter::{BufferedFrame, JitterBuffer, JitterResult};
use sdr_remote_core::protocol::*;
use sdr_remote_core::{FRAME_SAMPLES, FRAME_SAMPLES_WIDEBAND, MAX_PACKET_SIZE, NETWORK_SAMPLE_RATE, NETWORK_SAMPLE_RATE_WIDEBAND};

use crate::audio::AudioBackend;
use crate::commands::Command;
use crate::state::RadioState;

/// PTT burst count â€" send this many packets on PTT state change
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
const AGC_GATE: f32 = 0.001;     // Noise gate â€" don't boost below this

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
    ) -> Result<()> {
        // Create socket with large recv buffer to prevent packet loss from
        // spectrum packets (4-8KB each) filling the default 8KB Windows buffer.
        let socket = UdpSocket::bind("0.0.0.0:0").await.context("bind client socket")?;
        {
            use socket2::SockRef;
            let sock_ref = SockRef::from(&socket);
            let _ = sock_ref.set_recv_buffer_size(2 * 1024 * 1024);
            let _ = sock_ref.set_send_buffer_size(512 * 1024);
            let recv = sock_ref.recv_buffer_size().unwrap_or(0);
            let send = sock_ref.send_buffer_size().unwrap_or(0);
            info!("Client UDP bound to {} (recv_buf={}KB, send_buf={}KB)",
                socket.local_addr()?, recv / 1024, send / 1024);
        }

        let socket = Arc::new(socket);
        let start = Instant::now();

        // Audio setup â€" use defaults initially, can be reconfigured via commands
        let mut audio: Box<dyn AudioBackend> = audio_factory(None, None)?;
        let mut capture_rate = audio.capture_sample_rate();
        let mut playback_rate = audio.playback_sample_rate();

        let mut capture_frame_samples = (capture_rate * 20 / 1000) as usize;

        info!(
            "Client resamplers: capture {}Hz ({}smp/frame), playback {}Hz",
            capture_rate, capture_frame_samples, playback_rate
        );

        // Codec â€" wideband Opus (16kHz) for TX, stereo (8kHz) for RX decode
        let mut encoder = OpusEncoderWideband::new()?;
        // Per-channel mono decoders for multi-channel audio
        let mut dec_rx1 = OpusDecoder::new()?;
        let mut dec_bin_r = OpusDecoder::new()?;
        let mut dec_rx2 = OpusDecoder::new()?;

        // Yaesu (FT-991A) codec + jitter buffer â€" independent third audio channel
        let mut yaesu_decoder = OpusDecoder::new()?;
        let mut yaesu_jitter_buf = JitterBuffer::new(3, 40);
        let mut yaesu_logged_first = false;

        // Yaesu TX: wideband Opus (16kHz) for USB output
        let mut yaesu_tx_sequence: u32 = 0;
        let mut yaesu_tx_accum: Vec<f32> = Vec::new();
        let mut yaesu_tx_encoder = OpusEncoderWideband::new()?;
        let mut yaesu_tx_resampler = rubato::SincFixedIn::<f32>::new(
            NETWORK_SAMPLE_RATE_WIDEBAND as f64 / capture_rate as f64, 1.0,
            rubato::SincInterpolationParameters {
                sinc_len: 32, f_cutoff: 0.90, oversampling_factor: 32,
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

        let sinc_params_yaesu = rubato::SincInterpolationParameters {
            sinc_len: 32, f_cutoff: 0.90, oversampling_factor: 32,
            interpolation: rubato::SincInterpolationType::Cubic,
            window: rubato::WindowFunction::Blackman,
        };
        let mut yaesu_resampler_out = rubato::SincFixedIn::<f32>::new(
            playback_rate as f64 / NETWORK_SAMPLE_RATE as f64,
            1.0,
            sinc_params_yaesu,
            FRAME_SAMPLES,
            1,
        )
        .context("create Yaesu 8k->device resampler")?;

        let sinc_params_in = rubato::SincInterpolationParameters {
            sinc_len: 32,
            f_cutoff: 0.90,
            oversampling_factor: 32,
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
        .context("create device->8k resampler")?;

        // State
        let mut state = RadioState::default();
        let mut server_addr: Option<String> = None;
        let mut auth_password: Option<String> = None;
        let mut _auth_completed = false;
        let mut yaesu_mem_data_clear_at: Option<Instant> = None;
        let mut tx_sequence: u32 = 0;
        let mut hb_sequence: u32 = 0;
        let mut ptt = false;
        let mut thetis_ptt = false;
        let mut yaesu_ptt = false;
        let mut last_ptt = false;
        let mut ptt_burst_remaining: u32 = 0;
        let mut capture_gate_delay: u32 = 0;
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
        let mut last_sent_volume: u16 = 0;
        let mut rx_volume_synced: bool = false; // Don't send ZZLA until server value received
        let mut agc = TxAgc::new();
        let mut agc_enabled = false;
        let mut rx2_volume: f32 = 0.2;     // Thetis ZZLB sync + RX2 audio gain
        let mut vfo_b_volume: f32 = 1.0;   // Additional client-only RX2 gain (VFO B Vol slider)
        let mut audio_mode: u16 = 0;       // 0=Mono, 1=BIN, 2=Split
        let stereo_output = audio.supports_stereo(); // false on Android

        // Audio recording state
        let mut rec_rx1: Option<crate::wav::WavWriter> = None;
        let mut rec_rx2: Option<crate::wav::WavWriter> = None;
        let mut rec_yaesu: Option<crate::wav::WavWriter> = None;

        // WAV playback state
        let mut playback_wav: Option<Vec<i16>> = None;
        let mut playback_pos: usize = 0;
        let mut playback_is_tx: bool = false;

        let mut yaesu_volume: f32 = 0.5;   // Yaesu audio volume (client-only)
        let mut yaesu_local_mic_gain: f32 = 1.0; // Local Yaesu mic gain (before Opus encoding)
        let mut yaesu_eq = crate::eq::Equalizer::new(48000.0); // EQ at capture rate
        let mut last_sent_rx2_volume: u16 = 0;
        let mut rx2_volume_synced: bool = false; // Don't send ZZLB until server value received
        let mut rx2_volume_user_changed: bool = false; // Only send when user changed slider
        let mut spectrum_enabled = false;
        let mut spectrum_fps: u8 = sdr_remote_core::DEFAULT_SPECTRUM_FPS;
        let mut spectrum_zoom: f32 = 1.0;
        let mut spectrum_pan: f32 = 0.0;
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
            // Process all pending commands (non-blocking)
            while let Ok(cmd) = self.cmd_rx.try_recv() {
                match cmd {
                    Command::Connect(addr, pw) => {
                        server_addr = Some(addr);
                        auth_password = pw;
                        _auth_completed = false;
                    }
                    Command::SendTotpCode(code) => {
                        if let Some(ref addr) = server_addr {
                            let code_bytes = code.as_bytes();
                            let mut buf = vec![0u8; 6 + code_bytes.len()];
                            let header = Header::new(PacketType::TotpResponse, Flags::NONE);
                            header.serialize(&mut buf[..4]);
                            buf[4..6].copy_from_slice(&(code_bytes.len() as u16).to_be_bytes());
                            buf[6..].copy_from_slice(code_bytes);
                            let _ = socket.send_to(&buf, addr.as_str()).await;
                            info!("TOTP code sent");
                        }
                    }
                    Command::Disconnect => {
                        // Send disconnect to server before clearing
                        if let Some(ref addr) = server_addr {
                            let mut buf = [0u8; DisconnectPacket::SIZE];
                            DisconnectPacket::serialize(&mut buf);
                            let _ = socket.send_to(&buf, addr.as_str()).await;
                            info!("Disconnect (ring={}, jbuf={}, jitter={:.1}ms, rtt={}ms, loss={}%)",
                                audio.playback_buffer_level(), jitter_buf.depth(),
                                jitter_buf.jitter_ms(), last_hb_ack_rtt, current_loss_percent);
                        }
                        server_addr = None;
                        jitter_buf.reset();
                        was_connected = false;
                        last_hb_ack_time = None;
                        last_hb_ack_rtt = 0;
                        logged_first_rx = false;
                        logged_first_tx = false;
                        rx_volume_synced = false;
                        rx2_volume_synced = false;
                        state.rx_af_gain = 0;
                        state.connected = false;
                        state.rtt_ms = 0;
                        state.jitter_ms = 0.0;
                        state.buffer_depth = 0;
                        state.rx_packets = 0;
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
                                let ctrl = ControlPacket {
                                    control_id: ControlId::Binaural,
                                    value: bin_val,
                                };
                                let mut buf = [0u8; ControlPacket::SIZE];
                                ctrl.serialize(&mut buf);
                                let _ = socket.send_to(&buf, addr.as_str()).await;
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
                    Command::SetAgcEnabled(enabled) => {
                        agc_enabled = enabled;
                        state.agc_enabled = enabled;
                        info!("TX AGC: {}", if enabled { "ON" } else { "OFF" });
                    }
                    Command::SetFrequency(hz) => {
                        if state.vfo_lock { continue; } // VFO A locked
                        if let Some(ref addr) = server_addr {
                            let pkt = FrequencyPacket { frequency_hz: hz };
                            let mut buf = [0u8; FrequencyPacket::SIZE];
                            pkt.serialize(&mut buf);
                            let _ = socket.send_to(&buf, addr.as_str()).await;
                        }
                        state.frequency_hz = hz;
                        pending_freq = Some(hz);
                        pending_freq_time = Some(Instant::now());
                    }
                    Command::SetMode(mode) => {
                        if let Some(ref addr) = server_addr {
                            let pkt = ModePacket { mode };
                            let mut buf = [0u8; ModePacket::SIZE];
                            pkt.serialize(&mut buf);
                            let _ = socket.send_to(&buf, addr.as_str()).await;
                        }
                        state.mode = mode;
                    }
                    Command::SetControl(id, value) => {
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket { control_id: id, value };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = socket.send_to(&buf, addr.as_str()).await;
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
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mksp(), FRAME_SAMPLES, 1) { res_rx1_out = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mksp(), FRAME_SAMPLES, 1) { res_bin_r_out = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mksp(), FRAME_SAMPLES, 1) { res_rx2_out = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mksp(), FRAME_SAMPLES, 1) { yaesu_resampler_out = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(NETWORK_SAMPLE_RATE_WIDEBAND as f64 / capture_rate as f64, 1.0, mksp(), capture_frame_samples, 1) { resampler_in = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(NETWORK_SAMPLE_RATE_WIDEBAND as f64 / capture_rate as f64, 1.0, mksp(), capture_frame_samples, 1) { yaesu_tx_resampler = r; }
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
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mksp(), FRAME_SAMPLES, 1) { res_rx1_out = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mksp(), FRAME_SAMPLES, 1) { res_bin_r_out = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mksp(), FRAME_SAMPLES, 1) { res_rx2_out = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(playback_rate as f64 / NETWORK_SAMPLE_RATE as f64, 1.0, mksp(), FRAME_SAMPLES, 1) { yaesu_resampler_out = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(NETWORK_SAMPLE_RATE_WIDEBAND as f64 / capture_rate as f64, 1.0, mksp(), capture_frame_samples, 1) { resampler_in = r; }
                                        if let Ok(r) = rubato::SincFixedIn::new(NETWORK_SAMPLE_RATE_WIDEBAND as f64 / capture_rate as f64, 1.0, mksp(), capture_frame_samples, 1) { yaesu_tx_resampler = r; }
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
                                let _ = socket.send_to(&buf, addr.as_str()).await;
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
                                let _ = socket.send_to(&buf, addr.as_str()).await;
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
                                let _ = socket.send_to(&buf, addr.as_str()).await;
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
                                let _ = socket.send_to(&buf, addr.as_str()).await;
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
                                let _ = socket.send_to(&buf, addr.as_str()).await;
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
                                let _ = socket.send_to(&buf, addr.as_str()).await;
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
                            let _ = socket.send_to(&buf, addr.as_str()).await;
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
                            let _ = socket.send_to(&buf, addr.as_str()).await;
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
                            let _ = socket.send_to(&buf, addr.as_str()).await;
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
                            let _ = socket.send_to(&buf, addr.as_str()).await;
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
                            let _ = socket.send_to(&buf, addr.as_str()).await;
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
                            let _ = socket.send_to(&buf, addr.as_str()).await;
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
                            let _ = socket.send_to(&buf, addr.as_str()).await;
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
                            let _ = socket.send_to(&buf, addr.as_str()).await;
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
                            let _ = socket.send_to(&buf, addr.as_str()).await;
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
                            let _ = socket.send_to(&buf, addr.as_str()).await;
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
                            let _ = socket.send_to(&buf, addr.as_str()).await;
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
                            let _ = socket.send_to(&buf, addr.as_str()).await;
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
                            let _ = socket.send_to(&buf, addr.as_str()).await;
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
                            let _ = socket.send_to(&buf, addr.as_str()).await;
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
                            let _ = socket.send_to(&buf, addr.as_str()).await;
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
                            let _ = socket.send_to(&buf, addr.as_str()).await;
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
                            let _ = socket.send_to(&buf, addr.as_str()).await;
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
                            let _ = socket.send_to(&buf, addr.as_str()).await;
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
                            let _ = socket.send_to(&buf, addr.as_str()).await;
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
                            let _ = socket.send_to(&buf, addr.as_str()).await;
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
                            let _ = socket.send_to(&buf, addr.as_str()).await;
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
                            let _ = socket.send_to(&buf, addr.as_str()).await;
                            info!("Server shutdown request sent");
                        }
                    }
                    Command::StartRecording { rx1, rx2, yaesu, path } => {
                        use std::path::Path;
                        let base = Path::new(&path);
                        let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
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
                                Ok(w) => { info!("Recording RX2 to {}", p.display()); rec_rx2 = Some(w); }
                                Err(e) => warn!("Failed to start RX2 recording: {}", e),
                            }
                        }
                        if yaesu {
                            let p = base.join(format!("Yaesu_{}.wav", ts));
                            match crate::wav::WavWriter::new(&p) {
                                Ok(w) => { info!("Recording Yaesu to {}", p.display()); rec_yaesu = Some(w); }
                                Err(e) => warn!("Failed to start Yaesu recording: {}", e),
                            }
                        }
                        state.recording = rx1 || rx2 || yaesu;
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
                            if let Err(e) = w.finalize() { warn!("Yaesu WAV finalize error: {}", e); }
                            else { info!("Yaesu recording stopped ({:.1}s)", dur); }
                        }
                        state.recording = false;
                    }
                    Command::PlayRecording { path } => {
                        match crate::wav::read_wav(std::path::Path::new(&path)) {
                            Ok((_rate, samples)) => {
                                info!("Playback: loaded {} ({:.1}s, {} samples)",
                                    path, samples.len() as f32 / 8000.0, samples.len());
                                playback_wav = Some(samples);
                                playback_pos = 0;
                                playback_is_tx = ptt || yaesu_ptt;
                                state.playing = true;
                            }
                            Err(e) => warn!("Failed to load WAV: {}", e),
                        }
                    }
                    Command::StopPlayback => {
                        playback_wav = None;
                        playback_pos = 0;
                        state.playing = false;
                        info!("Playback stopped");
                    }
                    // RX2 / VFO-B commands
                    Command::SetRx2Enabled(enabled) => {
                        state.rx2_enabled = enabled;
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket { control_id: ControlId::Rx2Enable, value: enabled as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = socket.send_to(&buf, addr.as_str()).await;
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
                    Command::SetYaesuFreq(hz) => {
                        if let Some(ref addr) = server_addr {
                            let pkt = FrequencyPacket { frequency_hz: hz };
                            let mut buf = [0u8; FrequencyPacket::SIZE];
                            pkt.serialize_as_type(&mut buf, PacketType::FrequencyYaesu);
                            let _ = socket.send_to(&buf, addr.as_str()).await;
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
                            let _ = socket.send_to(&send_buf, addr.as_str()).await;
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
                            let _ = socket.send_to(&send_buf, addr.as_str()).await;
                            // Then trigger the write
                            let ctrl = ControlPacket {
                                control_id: ControlId::YaesuWriteMemories, value: 0 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = socket.send_to(&buf, addr.as_str()).await;
                        }
                    }
                    Command::SetYaesuMode(mode) => {
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket { control_id: ControlId::YaesuMode, value: mode as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = socket.send_to(&buf, addr.as_str()).await;
                        }
                    }
                    Command::SetYaesuPtt(on) => {
                        yaesu_ptt = on;
                        // Open mic capture gate (shared hardware resource)
                        if on {
                            audio.set_capture_gate(true);
                        } else if !ptt {
                            audio.set_capture_gate(false);
                        }
                        // Send Yaesu PTT control to server
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket { control_id: ControlId::YaesuPtt, value: on as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = socket.send_to(&buf, addr.as_str()).await;
                        }
                    }
                    Command::SetYaesuTxGain(v) => {
                        // Local Yaesu mic gain (applied before Opus encoding)
                        yaesu_local_mic_gain = v;
                    }
                    Command::SetMonitor(on) => {
                        state.mon_on = on;
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket { control_id: ControlId::MonitorOn, value: on as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = socket.send_to(&buf, addr.as_str()).await;
                        }
                    }
                    Command::ThetisTune(on) => {
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket { control_id: ControlId::ThetisTune, value: on as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = socket.send_to(&buf, addr.as_str()).await;
                        }
                    }
                    Command::CwKey { pressed, duration_ms } => {
                        if let Some(ref addr) = server_addr {
                            let value = (pressed as u16) | (duration_ms << 1);
                            let ctrl = ControlPacket { control_id: ControlId::CwKey, value };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = socket.send_to(&buf, addr.as_str()).await;
                        }
                    }
                    Command::CwMacroStop => {
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket { control_id: ControlId::CwMacroStop, value: 0 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = socket.send_to(&buf, addr.as_str()).await;
                        }
                    }
                    Command::SetVfoSync(enabled) => {
                        state.vfo_sync = enabled;
                        if let Some(ref addr) = server_addr {
                            let ctrl = ControlPacket { control_id: ControlId::VfoSync, value: enabled as u16 };
                            let mut buf = [0u8; ControlPacket::SIZE];
                            ctrl.serialize(&mut buf);
                            let _ = socket.send_to(&buf, addr.as_str()).await;
                        }
                    }
                    Command::SetFrequencyRx2(hz) => {
                        if state.rx2_vfo_lock { continue; } // VFO B locked
                        if let Some(ref addr) = server_addr {
                            let pkt = FrequencyPacket { frequency_hz: hz };
                            let mut buf = [0u8; FrequencyPacket::SIZE];
                            pkt.serialize_as_type(&mut buf, PacketType::FrequencyRx2);
                            let _ = socket.send_to(&buf, addr.as_str()).await;
                        }
                        state.frequency_rx2_hz = hz;
                        pending_freq_rx2 = Some(hz);
                        pending_freq_rx2_time = Some(Instant::now());
                    }
                    Command::SetModeRx2(mode) => {
                        if let Some(ref addr) = server_addr {
                            let pkt = ModePacket { mode };
                            let mut buf = [0u8; ModePacket::SIZE];
                            pkt.serialize_as_type(&mut buf, PacketType::ModeRx2);
                            let _ = socket.send_to(&buf, addr.as_str()).await;
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
                            let _ = socket.send_to(&buf, addr.as_str()).await;
                            info!("RX2 spectrum enable sent: {}", enabled);
                        }
                    }
                    Command::SetRx2SpectrumFps(fps) => {
                        if let Some(ref addr) = server_addr {
                            if was_connected {
                                let ctrl = ControlPacket { control_id: ControlId::Rx2SpectrumFps, value: fps as u16 };
                                let mut buf = [0u8; ControlPacket::SIZE];
                                ctrl.serialize(&mut buf);
                                let _ = socket.send_to(&buf, addr.as_str()).await;
                            }
                        }
                    }
                    Command::SetRx2SpectrumZoom(zoom) => {
                        if let Some(ref addr) = server_addr {
                            if was_connected {
                                let ctrl = ControlPacket { control_id: ControlId::Rx2SpectrumZoom, value: (zoom * 10.0) as u16 };
                                let mut buf = [0u8; ControlPacket::SIZE];
                                ctrl.serialize(&mut buf);
                                let _ = socket.send_to(&buf, addr.as_str()).await;
                            }
                        }
                    }
                    Command::SetRx2SpectrumPan(pan) => {
                        if let Some(ref addr) = server_addr {
                            if was_connected {
                                let ctrl = ControlPacket { control_id: ControlId::Rx2SpectrumPan, value: ((pan + 0.5) * 10000.0) as u16 };
                                let mut buf = [0u8; ControlPacket::SIZE];
                                ctrl.serialize(&mut buf);
                                let _ = socket.send_to(&buf, addr.as_str()).await;
                            }
                        }
                    }
                }
            }

            // Detect disconnect from outside (addr went None without Disconnect cmd)
            let current_addr = server_addr.clone();
            if current_addr.is_none() && last_server_addr.is_some() {
                jitter_buf.reset();
                was_connected = false;
                last_hb_ack_time = None;
                last_hb_ack_rtt = 0;
                logged_first_rx = false;
                logged_first_tx = false;
                rx_volume_synced = false;
                rx2_volume_synced = false;
                state.rx_af_gain = 0;
                state.connected = false;
                state.rtt_ms = 0;
                state.jitter_ms = 0.0;
                state.buffer_depth = 0;
                state.rx_packets = 0;
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
                                        let _ = socket.send_to(&buf, addr.as_str()).await;

                                        let fps_ctrl = ControlPacket {
                                            control_id: ControlId::SpectrumFps,
                                            value: spectrum_fps as u16,
                                        };
                                        fps_ctrl.serialize(&mut buf);
                                        let _ = socket.send_to(&buf, addr.as_str()).await;

                                        // Re-send zoom and pan so server generates correct view
                                        let zoom_ctrl = ControlPacket {
                                            control_id: ControlId::SpectrumZoom,
                                            value: (spectrum_zoom * 10.0) as u16,
                                        };
                                        zoom_ctrl.serialize(&mut buf);
                                        let _ = socket.send_to(&buf, addr.as_str()).await;

                                        let pan_ctrl = ControlPacket {
                                            control_id: ControlId::SpectrumPan,
                                            value: ((spectrum_pan + 0.5) * 10000.0) as u16,
                                        };
                                        pan_ctrl.serialize(&mut buf);
                                        let _ = socket.send_to(&buf, addr.as_str()).await;

                                        let bins_ctrl = ControlPacket {
                                            control_id: ControlId::SpectrumMaxBins,
                                            value: spectrum_max_bins,
                                        };
                                        bins_ctrl.serialize(&mut buf);
                                        let _ = socket.send_to(&buf, addr.as_str()).await;

                                        if spectrum_fft_size_k != 0 {
                                            let fft_ctrl = ControlPacket {
                                                control_id: ControlId::SpectrumFftSize,
                                                value: spectrum_fft_size_k,
                                            };
                                            fft_ctrl.serialize(&mut buf);
                                            let _ = socket.send_to(&buf, addr.as_str()).await;
                                        }
                                    }

                                    // Re-send RX2 state on reconnect
                                    if state.rx2_enabled {
                                        let mut rx2_buf = [0u8; ControlPacket::SIZE];
                                        let ctrl = ControlPacket { control_id: ControlId::Rx2Enable, value: 1 };
                                        ctrl.serialize(&mut rx2_buf);
                                        let _ = socket.send_to(&rx2_buf, addr.as_str()).await;

                                        let ctrl = ControlPacket { control_id: ControlId::Rx2SpectrumEnable, value: 1 };
                                        ctrl.serialize(&mut rx2_buf);
                                        let _ = socket.send_to(&rx2_buf, addr.as_str()).await;

                                        let bins_ctrl = ControlPacket { control_id: ControlId::Rx2SpectrumMaxBins, value: spectrum_max_bins };
                                        bins_ctrl.serialize(&mut rx2_buf);
                                        let _ = socket.send_to(&rx2_buf, addr.as_str()).await;

                                        if rx2_spectrum_fft_size_k != 0 {
                                            let fft_ctrl = ControlPacket { control_id: ControlId::Rx2SpectrumFftSize, value: rx2_spectrum_fft_size_k };
                                            fft_ctrl.serialize(&mut rx2_buf);
                                            let _ = socket.send_to(&rx2_buf, addr.as_str()).await;
                                        }
                                        info!("RX2 state re-sent on reconnect");
                                    }
                                    // Send AudioMode so server knows our channel requirements
                                    let ctrl = ControlPacket { control_id: ControlId::AudioMode, value: audio_mode };
                                    let mut am_buf = [0u8; ControlPacket::SIZE];
                                    ctrl.serialize(&mut am_buf);
                                    let _ = socket.send_to(&am_buf, addr.as_str()).await;
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
                            state.smeter = sm_pkt.level;
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
                            state.smeter_rx2 = sm_pkt.level;
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
                                        let _ = socket.send_to(&buf, addr.as_str()).await;
                                    }
                                }
                                ControlId::DiversityRef => state.diversity_ref = ctrl.value as u8,
                                ControlId::DiversitySource => state.diversity_source = ctrl.value as u8,
                                ControlId::DiversityGainRx1 => state.diversity_gain_rx1 = ctrl.value,
                                ControlId::DiversityGainRx2 => state.diversity_gain_rx2 = ctrl.value,
                                ControlId::DiversityPhase => state.diversity_phase = ctrl.value,
                                ControlId::AgcAutoRx1 => state.agc_auto_rx1 = ctrl.value != 0,
                                ControlId::AgcAutoRx2 => state.agc_auto_rx2 = ctrl.value != 0,
                                ControlId::DdcSampleRateRx1 => state.ddc_sample_rate_rx1 = ctrl.value,
                                ControlId::DdcSampleRateRx2 => state.ddc_sample_rate_rx2 = ctrl.value,
                                ControlId::AudioMode => {} // handled client-side
                            }
                        }
                        Ok(Packet::EquipmentStatus(eq)) => {
                            match eq.device_type {
                                DeviceType::Amplitec6x2 => {
                                    state.amplitec_connected = eq.connected;
                                    state.amplitec_switch_a = eq.switch_a;
                                    state.amplitec_switch_b = eq.switch_b;
                                    if let Some(labels) = eq.labels {
                                        state.amplitec_labels = labels;
                                    }
                                }
                                DeviceType::Tuner => {
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
                                        // Debug fields (Fase D) â€" parts[28..47]
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
                                        // Drive config (Fase D) â€" parts[44..46]
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
                                    // Parse labels CSV: fw_major,fw_minor,operation,frequency_khz,band,direction,off_state,motors_moving,motor_distance_mm,motor_completion,elements(;-sep)
                                    if let Some(labels) = eq.labels {
                                        let parts: Vec<&str> = labels.split(',').collect();
                                        if parts.len() >= 11 {
                                            state.ub_fw_major = parts[0].parse().unwrap_or(0);
                                            state.ub_fw_minor = parts[1].parse().unwrap_or(0);
                                            // parts[2] = operation (not needed in client)
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
                            state.yaesu_connected = true;
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
                            state.yaesu_vfo_select = ys.vfo_select;
                            state.yaesu_memory_channel = ys.memory_channel;
                        }
                        Ok(Packet::FrequencyYaesu(_)) => {} // clientâ†’server only
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
                                yaesu_decoder = OpusDecoder::new().unwrap_or_else(|e| {
                                    warn!("Yaesu decoder reset failed: {}", e);
                                    OpusDecoder::new().unwrap()
                                });
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
                                },
                                arrival_ms,
                            );
                        }
                        Ok(Packet::PttDenied) => {
                            state.ptt_denied = true;
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
                                let _ = socket.send_to(&buf, addr.as_str()).await;
                                info!("Auth response sent");
                            } else {
                                warn!("Auth challenge received but no password configured");
                                state.auth_rejected = true;
                            }
                        }
                        Ok(Packet::AuthResult(result)) => {
                            match result {
                                sdr_remote_core::protocol::AUTH_ACCEPTED => {
                                    info!("Auth accepted");
                                    _auth_completed = true;
                                    state.auth_rejected = false;
                                    state.totp_required = false;
                                }
                                sdr_remote_core::protocol::AUTH_TOTP_REQUIRED => {
                                    info!("Password OK, TOTP required");
                                    state.auth_rejected = false;
                                    state.totp_required = true;
                                }
                                _ => {
                                    warn!("Auth rejected");
                                    state.auth_rejected = true;
                                    _auth_completed = false;
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
                            was_connected = false;
                            last_hb_ack_time = None;
                            last_hb_ack_rtt = 0;
                            rx_volume_synced = false;
                            rx2_volume_synced = false;
                            state.rx_af_gain = 0;
                            state.connected = false;
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
                            warn!("Invalid packet ({}B): {}", len, e);
                        }
                    }

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
                        // Right channel buffer â€" filled from stereo decode
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

                                // Pull multi-channel frame from jitter buffer
                                let frame_data: Option<Vec<u8>> = match jitter_buf.pull() {
                                    JitterResult::Frame(frame) => {
                                        frames_this_tick += 1;
                                        if !frame.opus_data.is_empty() { Some(frame.opus_data) } else { None }
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

                                if let Some(blob) = frame_data {
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
                                            match ch_id {
                                                0 => { rx1_pcm = dec_rx1.decode(opus).ok(); }
                                                1 => { bin_r_pcm = dec_bin_r.decode(opus).ok(); }
                                                2 => { rx2_pcm = dec_rx2.decode(opus).ok(); }
                                                _ => {}
                                            }
                                            pos += 3 + opus_len;
                                        }
                                    }

                                    // Write decoded 8kHz PCM to WAV recorders
                                    if let Some(ref mut w) = rec_rx1 {
                                        if let Some(ref pcm) = rx1_pcm {
                                            let _ = w.write_samples(pcm);
                                        }
                                    }
                                    if let Some(ref mut w) = rec_rx2 {
                                        if let Some(ref pcm) = rx2_pcm {
                                            let _ = w.write_samples(pcm);
                                        }
                                    }

                                    // Resample and route based on audio_mode
                                    // RX1 → always L
                                    let mut left_dev = if let Some(pcm) = rx1_pcm {
                                        let mut dev = resample_to_device(&mut res_rx1_out, &pcm);
                                        apply_volume(&mut dev, rx_volume * vfo_a_volume * local_volume);
                                        let sq: f32 = dev.iter().map(|s| s*s).sum();
                                        rx1_level_accum += sq;
                                        rx1_level_count += dev.len();
                                        dev
                                    } else { Vec::new() };

                                    // Resample RX2 once if available (reused in Mono, BIN, Split)
                                    let rx2_dev = if let Some(pcm) = &rx2_pcm {
                                        let mut dev = resample_to_device(&mut res_rx2_out, pcm);
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
                                            let mut dev = resample_to_device(&mut res_bin_r_out, &pcm);
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
                                    let decoded: Option<Vec<i16>> = match yaesu_jitter_buf.pull() {
                                        JitterResult::Frame(frame) => {
                                            if !frame.opus_data.is_empty() {
                                                match yaesu_decoder.decode(&frame.opus_data) {
                                                    Ok(pcm) => Some(pcm),
                                                    Err(e) => { warn!("Yaesu decode error: {}", e); None }
                                                }
                                            } else { None }
                                        }
                                        JitterResult::Missing => {
                                            match yaesu_decoder.decode_plc() {
                                                Ok(pcm) => Some(pcm),
                                                Err(_) => None,
                                            }
                                        }
                                        JitterResult::NotReady => None,
                                    };
                                    match decoded {
                                        Some(pcm) => {
                                            if let Some(ref mut w) = rec_yaesu {
                                                let _ = w.write_samples(&pcm);
                                            }
                                            let mut resampled = resample_to_device(&mut yaesu_resampler_out, &pcm);
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

                            // BinR level: pure RX1-R only (measured before RX2 mix)
                            if bin_r_level_count > 0 {
                                state.playback_level_bin_r = (bin_r_level_accum / bin_r_level_count as f32).sqrt();
                            } else {
                                state.playback_level_bin_r = 0.0;
                            }

                            // WAV speaker playback (when not TX)
                            if !playback_is_tx && playback_wav.is_some() {
                                let wav = playback_wav.as_ref().unwrap();
                                let samples_per_tick = sdr_remote_core::FRAME_SAMPLES;
                                let remaining = wav.len() - playback_pos;
                                let to_read = samples_per_tick.min(remaining);
                                if to_read > 0 {
                                    let pcm: Vec<i16> = wav[playback_pos..playback_pos + to_read].to_vec();
                                    let resampled = resample_to_device(&mut res_rx1_out, &pcm);
                                    let target = playback_buf.len().max(resampled.len());
                                    playback_buf.resize(target, 0.0);
                                    bin_r_buf.resize(target, 0.0);
                                    for (i, &s) in resampled.iter().enumerate() {
                                        if i < playback_buf.len() {
                                            playback_buf[i] = (playback_buf[i] + s * local_volume).clamp(-1.0, 1.0);
                                            bin_r_buf[i] = (bin_r_buf[i] + s * local_volume).clamp(-1.0, 1.0);
                                        }
                                    }
                                    playback_pos += to_read;
                                }
                                if playback_pos >= wav.len() {
                                    info!("WAV speaker playback finished");
                                    playback_wav = None;
                                    playback_pos = 0;
                                    state.playing = false;
                                }
                            }

                            // Write audio to playback â€" stereo if binaural R available
                            if !playback_buf.is_empty() {
                                // Always write stereo â€" if R is empty, duplicate L
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

                    // playback_level is measured per-channel before mixing (see above)

                    // Connection timeout detection: only disconnect when BOTH
                    // heartbeat ACK and audio packets have been absent for the timeout.
                    // Dynamic timeout: max(6s, rtt*8) â€" accommodates mobile networks.
                    if was_connected {
                        let timeout_ms = (last_hb_ack_rtt as u64 * 8).max(CONNECTION_TIMEOUT_MIN_MS);
                        let hb_timed_out = last_hb_ack_time
                            .map_or(false, |t| t.elapsed().as_millis() > timeout_ms as u128);
                        let audio_timed_out = last_audio_received
                            .map_or(true, |t| t.elapsed().as_millis() > timeout_ms as u128);

                        if hb_timed_out && audio_timed_out {
                            info!("Connection lost (no traffic for {}ms, ring={}, jbuf={}, jitter={:.1}ms)",
                                timeout_ms, audio.playback_buffer_level(), jitter_buf.depth(), jitter_buf.jitter_ms());
                            // Don't reset jitter buffer â€" let it drain via PLC
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
                        let _ = socket.send_to(&buf, addr.as_str()).await;
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
                        let _ = socket.send_to(&buf, addr.as_str()).await;
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
                            // Had packets before, now nothing â€" 100% loss
                            smoothed_loss = smoothed_loss * 0.7 + 100.0 * 0.3;
                            current_loss_percent = smoothed_loss.round() as u8;
                        }
                        state.loss_percent = current_loss_percent;
                        loss_window_received = 0;
                        loss_window_max_seq = None;

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
                        let _ = socket.send_to(&buf, addr.as_str()).await;
                        last_hb_sent = Instant::now();
                    }

                    if ptt != last_ptt {
                        ptt_burst_remaining = PTT_BURST_COUNT;
                        info!("PTT: {}", if ptt { "TX" } else { "RX" });
                        if ptt {
                            // TX start: RX1/RX2 audio is muted in the mix loop (not
                            // via playback_mute) so Yaesu audio keeps playing.
                            // Capture gate opens after delay to let speaker drain.
                            accum_buf.clear();
                            audio.read_capture(&mut read_buf);
                            capture_gate_delay = 2; // 2 ticks Ã— 20ms = 40ms after speaker mute
                        } else {
                            // TX end: close mic gate (unless Yaesu PTT still active).
                            // No jitter buffer reset needed â€" playout kept running
                            // during TX so decoder and buffer are warm with fresh data.
                            if !yaesu_ptt {
                                audio.set_capture_gate(false);
                            }
                        }
                        last_ptt = ptt;
                    }
                    // Delayed capture gate opening: wait for speaker to drain
                    if capture_gate_delay > 0 {
                        capture_gate_delay -= 1;
                        if capture_gate_delay == 0 {
                            audio.set_capture_gate(true);
                            accum_buf.clear();
                            info!("Capture gate opened (speaker drained)");
                        }
                    }

                    // Update capture level
                    state.capture_level = audio.capture_level();

                    // WAV TX playback: bypass mic capture when playing back a TX recording
                    if playback_is_tx && (ptt || yaesu_ptt) && playback_wav.is_some() {
                        let wav = playback_wav.as_ref().unwrap();
                        let samples_per_tick = FRAME_SAMPLES; // 160 samples at 8kHz per 20ms
                        let remaining = wav.len() - playback_pos;
                        let to_read = samples_per_tick.min(remaining);
                        if to_read > 0 {
                            let pcm_8k: Vec<i16> = wav[playback_pos..playback_pos + to_read].to_vec();
                            // Upsample 8kHz -> 16kHz by duplicating each sample
                            let pcm_16k: Vec<i16> = pcm_8k.iter().flat_map(|&s| [s, s]).collect();
                            let pcm_f32: Vec<f32> = pcm_16k.iter()
                                .map(|&s| (s as f32 / 32767.0) * tx_gain)
                                .collect();
                            if pcm_f32.len() >= FRAME_SAMPLES_WIDEBAND {
                                let pcm_i16: Vec<i16> = pcm_f32.iter()
                                    .map(|&s| (s * 32767.0).clamp(-32768.0, 32767.0) as i16)
                                    .collect();
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
                                        let _ = socket.send_to(&buf, addr.as_str()).await;
                                    }
                                    Err(e) => warn!("WAV TX encode error: {}", e),
                                }
                            }
                            playback_pos += to_read;
                            // Also feed to Yaesu TX if Yaesu PTT active
                            if yaesu_ptt {
                                let f32_chunk: Vec<f32> = pcm_8k.iter().map(|&s| s as f32 / 32768.0).collect();
                                yaesu_tx_accum.extend_from_slice(&f32_chunk);
                            }
                        }
                        if playback_pos >= wav.len() {
                            info!("WAV TX playback finished");
                            playback_wav = None;
                            playback_pos = 0;
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
                            if yaesu_ptt {
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
                                    let _ = socket.send_to(&buf, addr.as_str()).await;

                                    if !logged_first_tx {
                                        info!("TX: first audio packet sent to {} (seq={}, accum_remain={})",
                                            addr, tx_sequence, accum_buf.len());
                                        logged_first_tx = true;
                                    }

                                    if ptt_burst_remaining > 0 {
                                        ptt_burst_remaining -= 1;
                                        let _ = socket.send_to(&buf, addr.as_str()).await;
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
                        let _ = socket.send_to(&buf, addr.as_str()).await;
                    }

                    // === Yaesu TX: completely separate mic audio path ===
                    if yaesu_ptt {
                        static YAESU_TX_LOG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                        let n = YAESU_TX_LOG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if n < 5 || n % 500 == 0 {
                            info!("Yaesu TX #{}: accum={} capture_frame={}", n, yaesu_tx_accum.len(), capture_frame_samples);
                        }
                        // Resample to 16kHz, encode wideband Opus
                        while yaesu_tx_accum.len() >= capture_frame_samples {
                            let mut chunk: Vec<f32> = yaesu_tx_accum.drain(..capture_frame_samples).collect();

                            // Apply 5-band EQ at capture rate (before resampling)
                            yaesu_eq.process(&mut chunk);

                            // Measure Yaesu mic level (after EQ)
                            let sum_sq: f32 = chunk.iter().map(|s| s * s).sum();
                            state.yaesu_mic_level = (sum_sq / chunk.len() as f32).sqrt();

                            // Resample to 16kHz and apply TX gain + local mic gain
                            let resampled = resample_to_network(&mut yaesu_tx_resampler, &chunk);
                            let pcm_i16: Vec<i16> = resampled.iter()
                                .map(|&s| (s * tx_gain * yaesu_local_mic_gain * 32767.0).clamp(-32768.0, 32767.0) as i16)
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
                                        pkt.serialize_as_type(&mut buf, PacketType::AudioYaesu);
                                        let _ = socket.send_to(&buf, addr.as_str()).await;
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
                        let mut buf = [0u8; DisconnectPacket::SIZE];
                        DisconnectPacket::serialize(&mut buf);
                        let _ = socket.send_to(&buf, addr.as_str()).await;
                        info!("Sent disconnect to server");
                    }
                    break;
                }
            }
        }

        Ok(())
    }
}

/// Resample i16 8kHz -> f32 device rate
fn resample_to_device(resampler: &mut impl rubato::Resampler<f32>, pcm_i16: &[i16]) -> Vec<f32> {
    let input_f32: Vec<f32> = pcm_i16.iter().map(|&s| s as f32 / 32768.0).collect();
    match resampler.process(&[input_f32], None) {
        Ok(result) => result.into_iter().next().unwrap_or_default(),
        Err(e) => {
            warn!("resample 8k->device error: {}", e);
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
