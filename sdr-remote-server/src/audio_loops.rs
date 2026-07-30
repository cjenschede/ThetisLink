// SPDX-License-Identifier: GPL-2.0-or-later

//! Audio encoding/sending loops extracted from network.rs.
//! Provides multi-channel + Yaesu audio bundlers and an IQ consumer loop.

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use log::{info, warn};
use crate::tracked_socket::TrackedSocket;
use tokio::sync::{watch, Mutex};
use tokio::time::{interval, Duration};

use sdr_remote_core::codec::{OpusEncoder, OpusEncoderWideband};
use sdr_remote_core::protocol::*;
use sdr_remote_core::{
    FRAME_SAMPLES, FRAME_SAMPLES_WIDEBAND, MAX_PACKET_SIZE, NETWORK_SAMPLE_RATE,
    NETWORK_SAMPLE_RATE_WIDEBAND,
};

use crate::ptt::PttController;
use crate::session::SessionManager;

// ── VRX experiment: one-shot IQ dump ────────────────────────────────
// Activated by VRX_DUMP=<path> env. Duration via VRX_DUMP_SECONDS
// (default 5 s). File format: u32 LE sample_rate_hz, then interleaved
// f32 LE I, f32 LE Q pairs. Read by `vrx-spike --input <path>`.

struct VrxDumpState {
    writer: std::io::BufWriter<std::fs::File>,
    samples_written: u64,
    samples_target: u64,
    sample_rate: u32,
    header_written: bool,
    finished: bool,
    path: String,
}

impl VrxDumpState {
    fn open(path: &str) -> std::io::Result<Self> {
        let f = std::fs::File::create(path)?;
        let seconds: f32 = std::env::var("VRX_DUMP_SECONDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5.0);
        // samples_target is computed once the first frame arrives so we
        // know the actual rate Thetis is providing.
        info!(
            "VRX dump: capturing ~{} s of RX1 I/Q to {} (will close on completion)",
            seconds, path
        );
        Ok(Self {
            writer: std::io::BufWriter::new(f),
            samples_written: 0,
            samples_target: 0,
            sample_rate: 0,
            header_written: false,
            finished: false,
            path: path.to_string(),
        })
    }

    fn write_batch(&mut self, sample_rate: u32, pairs: &[(f32, f32)]) {
        if self.finished {
            return;
        }
        use std::io::Write;
        if !self.header_written {
            let seconds: f32 = std::env::var("VRX_DUMP_SECONDS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5.0);
            self.sample_rate = sample_rate;
            self.samples_target = (sample_rate as f32 * seconds) as u64;
            if self.writer.write_all(&sample_rate.to_le_bytes()).is_err() {
                self.finished = true;
                return;
            }
            self.header_written = true;
            info!(
                "VRX dump: header written, sample_rate={} Hz, target={} samples",
                sample_rate, self.samples_target
            );
        }
        // Write up to samples_target.
        let remaining = self.samples_target.saturating_sub(self.samples_written);
        let take = (pairs.len() as u64).min(remaining) as usize;
        for &(i, q) in &pairs[..take] {
            if self.writer.write_all(&i.to_le_bytes()).is_err()
                || self.writer.write_all(&q.to_le_bytes()).is_err()
            {
                self.finished = true;
                return;
            }
        }
        self.samples_written += take as u64;
        if self.samples_written >= self.samples_target {
            let _ = self.writer.flush();
            self.finished = true;
            info!(
                "VRX dump: capture complete, {} samples written to {}",
                self.samples_written, self.path
            );
        }
    }
}

// VRX live channelizer + Opus encode + UDP send is volledig
// gedelegeerd aan de `vrx-rs` crate + `vrx_bridge::ThetisVrxSink`.
// `tci_iq_consumer` instantieert per VRX-channel een `VrxRuntime`
// en geeft elke IQ-batch door via `feed()`.

// â"€â"€ Resampling helpers â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€

/// Resample i16 8kHz â†' f32 device rate
pub fn resample_to_device(resampler: &mut impl rubato::Resampler<f32>, pcm_i16: &[i16]) -> Vec<f32> {
    let input_f32: Vec<f32> = pcm_i16.iter().map(|&s| s as f32 / 32768.0).collect();
    match resampler.process(&[input_f32], None) {
        Ok(result) => result.into_iter().next().unwrap_or_default(),
        Err(e) => {
            warn!("resample 8kâ†'device error: {}", e);
            Vec::new()
        }
    }
}

/// Resample f32 device rate â†' f32 8kHz
pub fn resample_to_network(resampler: &mut impl rubato::Resampler<f32>, pcm_f32: &[f32]) -> Vec<f32> {
    match resampler.process(&[pcm_f32.to_vec()], None) {
        Ok(result) => result.into_iter().next().unwrap_or_default(),
        Err(e) => {
            warn!("resample deviceâ†'8k error: {}", e);
            Vec::new()
        }
    }
}

/// Standard high-quality sinc resampler parameters (used by server audio loops)
pub fn hq_sinc_params() -> rubato::SincInterpolationParameters {
    rubato::SincInterpolationParameters {
        sinc_len: 128,
        f_cutoff: 0.95,
        oversampling_factor: 128,
        interpolation: rubato::SincInterpolationType::Cubic,
        window: rubato::WindowFunction::Blackman,
    }
}

// ── Multi-channel audio bundler ─────────────────────────────────────────

/// Multi-channel audio loop that replaces the three separate TCI loops.
/// Always sends L=RX1 (or RX1-L when BIN), R=RX2 (or RX1-R when BIN).
/// The client decides how to play L and R (mono/split/binaural).
pub async fn tci_multichannel_audio_loop(
    socket: Arc<TrackedSocket>,
    session: Arc<Mutex<SessionManager>>,
    ptt: Arc<Mutex<PttController>>,
    mut rx1_audio_rx: Option<tokio::sync::mpsc::Receiver<Vec<f32>>>,
    mut rx2_audio_rx: Option<tokio::sync::mpsc::Receiver<Vec<f32>>>,
    mut bin_r_audio_rx: Option<tokio::sync::mpsc::Receiver<Vec<f32>>>,
    shutdown: &mut watch::Receiver<bool>,
    start: Instant,
    audio_stats: Arc<crate::audio_stats::AudioActivityStats>,
    server_start: Instant,
) -> Result<()> {
    let tci_rate = 48000u32;
    let tci_frame_samples = (tci_rate * 20 / 1000) as usize; // 960

    // Per-channel mono encoders + resamplers - narrowband (8 kHz).
    let mut enc_rx1 = OpusEncoder::new()?;
    let mut enc_bin_r = OpusEncoder::new()?;
    let mut enc_rx2 = OpusEncoder::new()?;
    let mk_resampler = || rubato::SincFixedIn::<f32>::new(
        NETWORK_SAMPLE_RATE as f64 / tci_rate as f64, 1.0,
        hq_sinc_params(), tci_frame_samples, 1,
    );
    let mut res_rx1 = mk_resampler().context("RX1 resampler")?;
    let mut res_bin_r = mk_resampler().context("BinR resampler")?;
    let mut res_rx2 = mk_resampler().context("RX2 resampler")?;

    // Wideband (16 kHz) parallel-encoders - alleen actief gevoed wanneer
    // ten minste één client de Thetis-wideband-audio opt-in heeft staan.
    // De resamplers blijven idle (geen `process()` call) zolang geen
    // client wideband wil - geen merkbare CPU-impact.
    let mut enc_rx1_wb = OpusEncoderWideband::new()?;
    let mut enc_bin_r_wb = OpusEncoderWideband::new()?;
    let mut enc_rx2_wb = OpusEncoderWideband::new()?;
    let mk_resampler_wb = || rubato::SincFixedIn::<f32>::new(
        NETWORK_SAMPLE_RATE_WIDEBAND as f64 / tci_rate as f64, 1.0,
        hq_sinc_params(), tci_frame_samples, 1,
    );
    let mut res_rx1_wb = mk_resampler_wb().context("RX1 WB resampler")?;
    let mut res_bin_r_wb = mk_resampler_wb().context("BinR WB resampler")?;
    let mut res_rx2_wb = mk_resampler_wb().context("RX2 WB resampler")?;

    let mut sequence: u32 = 0;
    let mut rx1_accum: Vec<f32> = Vec::with_capacity(tci_frame_samples * 4);
    let mut rx2_accum: Vec<f32> = Vec::with_capacity(tci_frame_samples * 4);
    let mut bin_r_accum: Vec<f32> = Vec::with_capacity(tci_frame_samples * 4);
    let mut tick = interval(Duration::from_millis(20));
    let mut had_clients = false;

    info!("Stereo audio mixer started");

    loop {
        // Try to acquire missing channels
        if rx1_audio_rx.is_none() || rx2_audio_rx.is_none() || bin_r_audio_rx.is_none() {
            let mut ptt_guard = ptt.lock().await;
            if let Some(tci) = Some(&mut ptt_guard.tci) {
                if rx1_audio_rx.is_none() { rx1_audio_rx = tci.rx1_audio_rx.take(); }
                if rx2_audio_rx.is_none() { rx2_audio_rx = tci.rx2_audio_rx.take(); }
                if bin_r_audio_rx.is_none() { bin_r_audio_rx = tci.bin_r_audio_rx.take(); }
            }
            drop(ptt_guard);
            if rx1_audio_rx.is_none() {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(200)) => continue,
                    _ = shutdown.changed() => break,
                }
            }
        }

        tokio::select! {
            // Wait for tick or shutdown â€" audio is drained non-blocking below
            _ = tick.tick() => {
                // Drain ALL channels non-blocking to prevent select! bias
                fn drain_channel(rx_opt: &mut Option<tokio::sync::mpsc::Receiver<Vec<f32>>>, accum: &mut Vec<f32>) {
                    if let Some(rx) = rx_opt.as_mut() {
                        loop {
                            match rx.try_recv() {
                                Ok(s) => accum.extend_from_slice(&s),
                                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                                    *rx_opt = None;
                                    accum.clear();
                                    break;
                                }
                            }
                        }
                    }
                }
                drain_channel(&mut rx1_audio_rx, &mut rx1_accum);
                drain_channel(&mut rx2_audio_rx, &mut rx2_accum);
                drain_channel(&mut bin_r_audio_rx, &mut bin_r_accum);
                // Cap accumulators
                let max = tci_frame_samples * 10;
                if rx1_accum.len() > max { rx1_accum.drain(..rx1_accum.len() - max); }
                if rx2_accum.len() > max { rx2_accum.drain(..rx2_accum.len() - max); }
                if bin_r_accum.len() > max { bin_r_accum.drain(..bin_r_accum.len() - max); }
                if rx1_accum.len() < tci_frame_samples {
                    continue;
                }

                let addrs = {
                    let sess = session.lock().await;
                    // Yaesu-only clients (Android Yaesu-mode) krijgen geen Thetis-RX-audio
                    // -> databesparing. Herstelt zodra Yaesu-mode uit gaat (spectrum aan).
                    sess.thetis_audio_addrs()
                };
                let has_clients = !addrs.is_empty();
                if !has_clients {
                    had_clients = false;
                    continue;
                }

                // Align accumulators on first tick or when a client (re)connects
                if !had_clients {
                    info!("Multi-ch audio: client connected, aligning accumulators (rx1={} rx2={} binr={})",
                        rx1_accum.len(), rx2_accum.len(), bin_r_accum.len());
                    if rx1_accum.len() > tci_frame_samples {
                        rx1_accum.drain(..rx1_accum.len() - tci_frame_samples);
                    }
                    if rx2_accum.len() > tci_frame_samples {
                        rx2_accum.drain(..rx2_accum.len() - tci_frame_samples);
                    }
                    if bin_r_accum.len() > tci_frame_samples {
                        bin_r_accum.drain(..bin_r_accum.len() - tci_frame_samples);
                    }
                    had_clients = true;
                }

                // Encode each available channel as mono Opus and bundle.
                // Sinds wideband-opt-in: ook een tweede payload-set per
                // channel (16 kHz Opus) wanneer ten minste één actieve
                // client de optie aan heeft staan. NB-pad blijft de
                // default voor alle huidige clients.
                let any_wb = session.lock().await.any_client_wants_thetis_wideband();
                let mut channels_nb: Vec<(u8, Vec<u8>)> = Vec::with_capacity(3);
                let mut channels_wb: Vec<(u8, Vec<u8>)> = Vec::with_capacity(3);

                // Helper: encodeer een 48-kHz frame in beide kanalen
                // (NB altijd, WB conditioneel).
                fn pcm_to_i16(samples: &[f32]) -> Vec<i16> {
                    samples.iter()
                        .map(|&s| (s * 32767.0).clamp(-32768.0, 32767.0) as i16)
                        .collect()
                }

                // CH0: RX1 (always present)
                let rx1_frame: Vec<f32> = rx1_accum.drain(..tci_frame_samples).collect();
                let rx1_8k = resample_to_network(&mut res_rx1, &rx1_frame);
                let rx1_i16 = pcm_to_i16(&rx1_8k);
                if rx1_i16.len() >= FRAME_SAMPLES {
                    if let Ok(opus) = enc_rx1.encode(&rx1_i16[..FRAME_SAMPLES]) {
                        channels_nb.push((0, opus));
                        audio_stats.rx1.tick(server_start);
                    }
                }
                if any_wb {
                    let rx1_16k = resample_to_network(&mut res_rx1_wb, &rx1_frame);
                    let rx1_i16_wb = pcm_to_i16(&rx1_16k);
                    if rx1_i16_wb.len() >= FRAME_SAMPLES_WIDEBAND {
                        if let Ok(opus) = enc_rx1_wb.encode(&rx1_i16_wb[..FRAME_SAMPLES_WIDEBAND]) {
                            channels_wb.push((0, opus));
                        }
                    }
                }

                // CH1: BinR (only when Thetis binaural active)
                if bin_r_accum.len() >= tci_frame_samples {
                    let frame: Vec<f32> = bin_r_accum.drain(..tci_frame_samples).collect();
                    let bin_8k = resample_to_network(&mut res_bin_r, &frame);
                    let bin_i16 = pcm_to_i16(&bin_8k);
                    if bin_i16.len() >= FRAME_SAMPLES {
                        if let Ok(opus) = enc_bin_r.encode(&bin_i16[..FRAME_SAMPLES]) {
                            channels_nb.push((1, opus));
                        }
                    }
                    if any_wb {
                        let bin_16k = resample_to_network(&mut res_bin_r_wb, &frame);
                        let bin_i16_wb = pcm_to_i16(&bin_16k);
                        if bin_i16_wb.len() >= FRAME_SAMPLES_WIDEBAND {
                            if let Ok(opus) = enc_bin_r_wb.encode(&bin_i16_wb[..FRAME_SAMPLES_WIDEBAND]) {
                                channels_wb.push((1, opus));
                            }
                        }
                    }
                }

                // CH2: RX2 (when RX2 audio available)
                if rx2_accum.len() >= tci_frame_samples {
                    let frame: Vec<f32> = rx2_accum.drain(..tci_frame_samples).collect();
                    let rx2_8k = resample_to_network(&mut res_rx2, &frame);
                    let rx2_i16 = pcm_to_i16(&rx2_8k);
                    if rx2_i16.len() >= FRAME_SAMPLES {
                        if let Ok(opus) = enc_rx2.encode(&rx2_i16[..FRAME_SAMPLES]) {
                            channels_nb.push((2, opus));
                            audio_stats.rx2.tick(server_start);
                        }
                    }
                    if any_wb {
                        let rx2_16k = resample_to_network(&mut res_rx2_wb, &frame);
                        let rx2_i16_wb = pcm_to_i16(&rx2_16k);
                        if rx2_i16_wb.len() >= FRAME_SAMPLES_WIDEBAND {
                            if let Ok(opus) = enc_rx2_wb.encode(&rx2_i16_wb[..FRAME_SAMPLES_WIDEBAND]) {
                                channels_wb.push((2, opus));
                            }
                        }
                    }
                }

                // Drain excess accumulators
                if bin_r_accum.len() > tci_frame_samples {
                    bin_r_accum.drain(..bin_r_accum.len() - tci_frame_samples);
                }
                if rx2_accum.len() > tci_frame_samples {
                    rx2_accum.drain(..rx2_accum.len() - tci_frame_samples);
                }

                // Send per-client filtered multi-channel packets
                if !channels_nb.is_empty() {
                    let timestamp = start.elapsed().as_millis() as u32;
                    // Read per-client modes + rx2_enabled flag + WB-opt-in
                    // under short lock, then release. `rx2_enabled` gates
                    // CH2 even when `audio_mode` would otherwise allow it
                    // - the desktop client UI's "RX2 enabled" toggle must
                    // mute the upstream RX2 stream entirely, not just the
                    // local playback (bandwidth bug uncovered 2026-05-13).
                    let client_modes: Vec<(std::net::SocketAddr, u8, bool, bool, bool)> = {
                        let sess = session.lock().await;
                        addrs
                            .iter()
                            .map(|&a| (
                                a,
                                sess.client_audio_mode(a),
                                sess.client_rx1_enabled(a),
                                sess.client_rx2_enabled(a),
                                sess.client_thetis_wideband(a),
                            ))
                            .collect()
                    };

                    for (addr, mode, rx1_enabled, rx2_enabled, want_wb) in &client_modes {
                        // Filter channels based on client's audio mode.
                        // Then drop CH2 (RX2) for clients that have RX2
                        // turned off - those bytes would otherwise reach
                        // the client and be silently mixed into mono
                        // output (or burn data on metered links).
                        // mode 255 (default/Android): CH0 only
                        // mode 0 (Mono): CH0 + CH2  (gated by rx2_enabled)
                        // mode 1 (BIN): CH0 + CH1 + CH2  (CH2 gated)
                        // mode 2 (Split): CH0 + CH2  (CH2 gated)
                        // Kies juiste payload-set: WB als client opt-in heeft
                        // én er een WB-payload beschikbaar is voor dit frame;
                        // anders narrowband (default voor alle huidige clients).
                        let use_wb = *want_wb && !channels_wb.is_empty();
                        let src: &Vec<(u8, Vec<u8>)> = if use_wb { &channels_wb } else { &channels_nb };
                        let client_chs: Vec<(u8, Vec<u8>)> = src.iter()
                            .filter(|(ch_id, _)| {
                                let allowed = match *mode {
                                    255 => *ch_id == 0,                    // Android: RX1 only
                                    0 => *ch_id == 0 || *ch_id == 2,      // Mono: RX1 + RX2
                                    1 => true,                             // BIN: all
                                    2 => *ch_id == 0 || *ch_id == 2,      // Split: RX1 + RX2
                                    _ => *ch_id == 0,
                                };
                                if !allowed { return false; }
                                // RX1-audio-abonnement: CH0 (RX1) + CH1 (RX1 imag/BIN)
                                // vervallen als de client RX1-audio uit heeft (alleen-VRX).
                                if (*ch_id == 0 || *ch_id == 1) && !rx1_enabled { return false; }
                                if *ch_id == 2 && !rx2_enabled { return false; }
                                true
                            })
                            .cloned()
                            .collect();

                        if !client_chs.is_empty() {
                            let packet = sdr_remote_core::protocol::MultiChannelAudioPacket {
                                sequence,
                                timestamp,
                                channels: client_chs,
                                flags: if use_wb { Flags::AUDIO_WIDEBAND } else { Flags::NONE },
                            };
                            let mut send_buf = Vec::with_capacity(MAX_PACKET_SIZE);
                            packet.serialize(&mut send_buf);
                            let _ = socket.send_to(&send_buf, addr).await;
                        }
                    }
                    sequence = sequence.wrapping_add(1);
                }
            }
            _ = shutdown.changed() => break,
        }
    }

    info!("Multi-channel audio bundler stopped");
    Ok(())
}

// â"€â"€ Yaesu audio loop â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€

/// Yaesu USB audio TX loop: receives from cpal, encodes Opus, sends to clients.
pub async fn yaesu_audio_loop(
    socket: Arc<TrackedSocket>,
    session: Arc<Mutex<SessionManager>>,
    mut audio_rx: tokio::sync::mpsc::Receiver<Vec<f32>>,
    sample_rate: u32,
    shutdown: &mut watch::Receiver<bool>,
    start: Instant,
    audio_stats: Arc<crate::audio_stats::AudioActivityStats>,
    server_start: Instant,
    // Dual-radio (Optie B-prime): slot 0 -> yaesu_addrs + AudioYaesu (byte-identiek
    // aan het bestaande pad); slot 1 -> yaesu2_addrs + AudioYaesu2.
    slot: u8,
    // Live radio-status (voor de software-squelch). FTX-1: squelch_open uit de
    // RI-poll gate't de USB-audio. 991A: squelch_open blijft true -> geen effect.
    // std::sync::Mutex (matcht YaesuRadio.status; audio_loops' `Mutex` = tokio).
    status: Arc<std::sync::Mutex<crate::yaesu::YaesuState>>,
) -> Result<()> {
    let audio_ptype = if slot == 0 { PacketType::AudioYaesu } else { PacketType::AudioYaesu2 };
    let frame_samples = (sample_rate * 20 / 1000) as usize;

    // Radio-RX bandbreedte volgt de Thetis-wideband-toggle (build 122):
    // de client kiest in de Server-tab NB (laag dataverbruik) of WB (helder,
    // CELT i.p.v. SILK -> getrouwe ruis). Eén globale knop voor Thetis + beide
    // radio's. We houden daarom beide encoders/resamplers aan en sturen per
    // abonnee het formaat dat die client wil (`client_thetis_wideband`), met
    // de `AUDIO_WIDEBAND`-flag op WB-packets. Mirror van het Thetis-multi-ch-pad.
    let mut enc_nb = OpusEncoder::new_radio_rx()?;      // 8 kHz, DTX-uit
    let mut enc_wb = OpusEncoderWideband::new()?;       // 16 kHz
    let mut res_nb = rubato::SincFixedIn::<f32>::new(
        NETWORK_SAMPLE_RATE as f64 / sample_rate as f64,
        1.0, hq_sinc_params(), frame_samples, 1,
    ).context("create Yaesu NB resampler")?;
    let mut res_wb = rubato::SincFixedIn::<f32>::new(
        NETWORK_SAMPLE_RATE_WIDEBAND as f64 / sample_rate as f64,
        1.0, hq_sinc_params(), frame_samples, 1,
    ).context("create Yaesu WB resampler")?;

    let mut sequence: u32 = 0;
    let mut accumulator: Vec<f32> = Vec::with_capacity(frame_samples * 4);
    let mut tick = interval(Duration::from_millis(20));
    let mut had_clients = false;

    // Software-squelch gate-envelope (FTX-1: dichte squelch -> fade naar stilte;
    // de squelch-knop op de radio is de drempel). 991A: squelch_open=true -> no-op.
    let mut gate_gain: f32 = 1.0;
    let mut sql_closed_frames: u32 = 0;
    const SQL_HANG_FRAMES: u32 = 8;   // ~160 ms hang vóór de gate sluit (anti-flutter)
    const SQL_FADE_STEP: f32 = 0.10;  // ~10 frames ≈ 200 ms volledige fade

    info!("Yaesu audio RX loop started ({}Hz capture, NB+WB op aanvraag)", sample_rate);

    loop {
        tokio::select! {
            result = audio_rx.recv() => {
                match result {
                    Some(samples) => {
                        accumulator.extend_from_slice(&samples);
                        let max_accum = frame_samples * 10;
                        if accumulator.len() > max_accum {
                            accumulator.drain(..accumulator.len() - max_accum);
                        }
                    }
                    None => {
                        info!("Yaesu audio channel closed");
                        break;
                    }
                }
            }
            _ = tick.tick() => {
                // Abonnees + hun WB-voorkeur. RX-bandbreedte volgt de Thetis-toggle
                // per client; TX blijft altijd wideband (zie network.rs).
                let subs: Vec<(std::net::SocketAddr, bool)> = {
                    let s = session.lock().await;
                    let addrs = if slot == 0 { s.yaesu_addrs() } else { s.yaesu2_addrs() };
                    addrs.into_iter().map(|a| (a, s.client_thetis_wideband(a))).collect()
                };
                if subs.is_empty() {
                    accumulator.clear();
                    had_clients = false;
                    continue;
                }

                if !had_clients {
                    match (OpusEncoder::new_radio_rx(), OpusEncoderWideband::new()) {
                        (Ok(n), Ok(w)) => {
                            enc_nb = n;
                            enc_wb = w;
                            sequence = 0;
                            accumulator.clear();
                            had_clients = true;
                            info!("Yaesu audio: client(s) enabled, encoders reset");
                        }
                        _ => {
                            log::error!("Yaesu encoder reset failed - Yaesu audio RX skipped this tick (server blijft draaien)");
                            // had_clients stays false -> retry next tick if clients still present.
                        }
                    }
                    continue;
                }

                if accumulator.len() < frame_samples {
                    continue;
                }
                let mut frame: Vec<f32> = accumulator.drain(..frame_samples).collect();

                // Software-squelch: fade naar stilte bij dichte squelch (alleen FTX-1
                // zet squelch_open=false; 991A blijft open -> start_g/end_g==1.0 -> no-op).
                // ALLEEN in FM-familie (internal mode 5: FM/FM-N/DATA-FM): op SSB/CW/AM/
                // RTTY/data heeft de radio-BUSY (RI P8) geen zinvolle betekenis en meldt
                // hij 'dicht' terwijl er wél audio is -> daar audio altijd doorlaten
                // This prevents LSB from being muted by the USB-side squelch gate.
                let (sql_open, mode, tx_active) = {
                    let s = status.lock().unwrap();
                    (s.squelch_open, s.mode, s.tx_active)
                };
                // TX-mute: tijdens zenden mag RX-audio nooit terugkomen. De 991A dempt
                // zijn USB-RX in hardware tijdens TX; de FTX-1 niet -> daar liep RX-geluid
                // door tijdens PTT (operator-test 2026-07-04). Model-onafhankelijk in software:
                // bij TX direct naar stilte (geen squelch-hang), voor de 991A een no-op
                // omdat er dan toch al (bijna) stilte binnenkomt.
                let effective_open = !tx_active && (mode != 5 || sql_open);
                let target: f32 = if tx_active {
                    sql_closed_frames = 0;
                    0.0
                } else if effective_open {
                    sql_closed_frames = 0;
                    1.0
                } else {
                    sql_closed_frames = sql_closed_frames.saturating_add(1);
                    if sql_closed_frames > SQL_HANG_FRAMES { 0.0 } else { 1.0 }
                };
                let start_g = gate_gain;
                let end_g = if target > gate_gain {
                    (gate_gain + SQL_FADE_STEP).min(target)
                } else {
                    (gate_gain - SQL_FADE_STEP).max(target)
                };
                if !(start_g == 1.0 && end_g == 1.0) {
                    let n = frame.len().max(1) as f32;
                    for (i, s) in frame.iter_mut().enumerate() {
                        let g = start_g + (end_g - start_g) * (i as f32 / n);
                        *s *= g;
                    }
                }
                gate_gain = end_g;
                // Observability: log alleen de fade-randen (geen per-frame spam).
                if start_g > 0.0 && end_g == 0.0 {
                    log::info!("Yaesu squelch: gate dicht - audio gedempt");
                } else if start_g == 0.0 && end_g > 0.0 {
                    log::info!("Yaesu squelch: gate open - audio hervat");
                }

                let need_wb = subs.iter().any(|(_, wb)| *wb);
                let need_nb = subs.iter().any(|(_, wb)| !*wb);
                let timestamp = start.elapsed().as_millis() as u32;

                // Encodeer alléén de gevraagde formaten (meestal maar één).
                let nb_buf: Option<Vec<u8>> = if need_nb {
                    let pcm = resample_to_network(&mut res_nb, &frame);
                    let i16s: Vec<i16> = pcm.iter()
                        .map(|&s| (s * 32767.0).clamp(-32768.0, 32767.0) as i16).collect();
                    if i16s.len() >= FRAME_SAMPLES {
                        match enc_nb.encode(&i16s[..FRAME_SAMPLES]) {
                            Ok(op) => {
                                let p = AudioPacket { flags: Flags::NONE, sequence, timestamp, opus_data: op };
                                let mut b = Vec::with_capacity(MAX_PACKET_SIZE);
                                p.serialize_as_type(&mut b, audio_ptype);
                                Some(b)
                            }
                            Err(e) => { log::warn!("Yaesu NB encode: {}", e); None }
                        }
                    } else { None }
                } else { None };

                let wb_buf: Option<Vec<u8>> = if need_wb {
                    let pcm = resample_to_network(&mut res_wb, &frame);
                    let i16s: Vec<i16> = pcm.iter()
                        .map(|&s| (s * 32767.0).clamp(-32768.0, 32767.0) as i16).collect();
                    if i16s.len() >= FRAME_SAMPLES_WIDEBAND {
                        match enc_wb.encode(&i16s[..FRAME_SAMPLES_WIDEBAND]) {
                            Ok(op) => {
                                let p = AudioPacket { flags: Flags::AUDIO_WIDEBAND, sequence, timestamp, opus_data: op };
                                let mut b = Vec::with_capacity(MAX_PACKET_SIZE);
                                p.serialize_as_type(&mut b, audio_ptype);
                                Some(b)
                            }
                            Err(e) => { log::warn!("Yaesu WB encode: {}", e); None }
                        }
                    } else { None }
                } else { None };

                sequence = sequence.wrapping_add(1);

                for (addr, wb) in &subs {
                    let buf = if *wb { wb_buf.as_ref() } else { nb_buf.as_ref() };
                    if let Some(b) = buf {
                        let _ = socket.send_to(b, addr).await;
                    }
                }
                if nb_buf.is_some() || wb_buf.is_some() {
                    audio_stats.yaesu_rx.tick(server_start);
                }
            }
            _ = shutdown.changed() => break,
        }
    }

    Ok(())
}

// â"€â"€ TCI IQ consumer â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€â"€

/// Drains IQ channels from TCI and feeds spectrum processors (RX1 + RX2).
/// Also runs the VRX channelizer on the RX1 IQ stream and emits VrxAudioPacket
/// UDP frames to subscribed clients (separate-channel VRX audio).
/// One per-client VRX channelizer runtime + its shared control-state Arc and the
/// current resolved wideband flag. Owned by the audio-loop task (not the manager).
struct ClientRt {
    runtime: vrx_rs::VrxRuntime,
    control: Arc<std::sync::Mutex<vrx_rs::VrxControlState>>,
    current_wb: bool,
}

/// Resolve the per-VRX wideband flag from the client's NB/WB/Auto mode + filter
/// width. Auto switches up at ≥4 kHz audio BW and back below ~3.75 kHz (hysteresis
/// against rebuild-thrash while dragging the filter edge). Rate-mode is now
/// per-client (PATCH-vrx-per-client), passed in rather than read from a global.
fn vrx_desired_wb(mode_u8: u8, low_hz: i32, high_hz: i32, current_wb: bool) -> bool {
    let mode = vrx_rs::VrxRateMode::from_u8(mode_u8);
    vrx_rs::rate_mode_wants_wideband(mode, low_hz, high_hz, current_wb)
}

/// Service one VRX channel for every currently audio-subscribed client: prune
/// runtimes whose client dropped the subscription, lazily create a runtime per
/// client (with that client's shared control), resolve per-client NB/WB, feed each
/// runtime and route its audio to that client's address only. Runtimes live in
/// `rts` (owned by the caller) - `feed()`/Opus/UDP never run under the manager lock;
/// the manager lock is only taken for short control/rate reads.
#[allow(clippy::too_many_arguments)]
fn service_vrx_channel(
    ch: u8,
    frame_rate: u32,
    iq_pairs: &[(f32, f32)],
    vfo_hz: u64,
    ddc_center_hz: u64,
    audio_addrs: &[std::net::SocketAddr],
    auto_addrs: &[std::net::SocketAddr],
    tx_active: bool,
    timestamp_ms: u32,
    rts: &mut std::collections::HashMap<std::net::SocketAddr, ClientRt>,
    mgr: &Arc<std::sync::Mutex<crate::vrx_manager::PerClientVrxManager>>,
    sink: &mut crate::vrx_bridge::ThetisVrxSink,
    timer: &mut crate::vrx_bridge::VrxFeedTimer,
) {
    // No set allocation on the hot path: the subscriber list is tiny (one entry
    // per client), so a linear `contains` is cheaper than building a HashSet.
    let before = rts.len();
    rts.retain(|a, _| audio_addrs.contains(a));
    if rts.len() != before {
        log::info!(
            "VRX destroy ch={} reason=disable removed={} - active_runtimes(ch)={}",
            ch + 1, before - rts.len(), rts.len()
        );
    }
    // Mute during TX: keep the runtimes (state intact), skip DSP/Opus/UDP.
    if tx_active {
        return;
    }
    let mut total_feed = std::time::Duration::ZERO;
    for &addr in audio_addrs {
        if !rts.contains_key(&addr) {
            let control = mgr.lock().unwrap().control(addr, ch);
            let runtime = vrx_rs::VrxRuntime::new(
                vrx_rs::VrxRuntimeOptions { vrx_id: ch, wav_dir: None, wav_segment_sec: 10, wideband: false },
                control.clone(),
            );
            rts.insert(addr, ClientRt { runtime, control, current_wb: false });
            log::info!("VRX create client={} ch={} - active_runtimes(ch)={}", addr, ch + 1, rts.len());
        }
        let rate_mode = mgr.lock().unwrap().rate_mode(&addr, ch);
        let rt = rts.get_mut(&addr).unwrap();
        let (lo, hi) = {
            let s = rt.control.lock().unwrap();
            (s.filter_low_hz, s.filter_high_hz)
        };
        let want_wb = vrx_desired_wb(rate_mode, lo, hi, rt.current_wb);
        if want_wb != rt.current_wb {
            log::info!(
                "VRX rate {} client={} ch={} (afc preserved)",
                if want_wb { "NB->WB" } else { "WB->NB" }, addr, ch + 1
            );
            let (afc_o, afc_b) = rt.runtime.afc_state();
            rt.runtime = vrx_rs::VrxRuntime::new(
                vrx_rs::VrxRuntimeOptions { vrx_id: ch, wav_dir: None, wav_segment_sec: 10, wideband: want_wb },
                rt.control.clone(),
            );
            rt.runtime.restore_afc_state(afc_o, afc_b);
            rt.current_wb = want_wb;
        }
        let auto = auto_addrs.contains(&addr);
        rt.control.lock().unwrap().sam_auto_tune = auto;
        sink.addrs.clear();
        sink.addrs.push(addr);
        sink.autotune_addrs.clear();
        if auto {
            sink.autotune_addrs.push(addr);
        }
        sink.wideband = rt.current_wb;
        sink.timestamp_ms = timestamp_ms;
        let t0 = std::time::Instant::now();
        rt.runtime.feed(frame_rate, iq_pairs, vfo_hz, ddc_center_hz, sink);
        total_feed += t0.elapsed();
    }
    timer.record(total_feed, iq_pairs.len(), frame_rate, rts.len());
}

pub async fn tci_iq_consumer(
    ptt: Arc<Mutex<PttController>>,
    spectrum: Arc<Mutex<crate::spectrum::SpectrumProcessor>>,
    rx2_spectrum: Arc<Mutex<crate::spectrum::Rx2SpectrumProcessor>>,
    shutdown: &mut watch::Receiver<bool>,
    socket: Arc<TrackedSocket>,
    session: Arc<Mutex<SessionManager>>,
    vrx_mgr: Arc<std::sync::Mutex<crate::vrx_manager::PerClientVrxManager>>,
) {
    let mut iq_rx1: Option<tokio::sync::mpsc::Receiver<(u32, Vec<(f32, f32)>)>> = None;
    let mut iq_rx2: Option<tokio::sync::mpsc::Receiver<(u32, Vec<(f32, f32)>)>> = None;

    // Local epoch for VRX packet timestamp stamping. Doesn't have to
    // match `audio_stats.tick` callsites since VRX is a separate
    // monotonic counter on the wire.
    let server_start = Instant::now();

    // VRX channelizer experiment - one-shot RX1 I/Q dump to file when
    // VRX_DUMP=<path> env is set. Captures ~5 s of complex I/Q (or
    // VRX_DUMP_SECONDS if set), then closes the file. File format:
    //   u32 LE  sample_rate_hz
    //   then interleaved f32 I, f32 Q pairs (little-endian)
    // Loaded by `vrx-spike --input <path>` for offline processing.
    let mut vrx_dump: Option<VrxDumpState> = std::env::var("VRX_DUMP")
        .ok()
        .map(|path| VrxDumpState::open(&path).expect("VRX_DUMP: failed to open"));

    // VRX live channelizers. Two instances: VRX1 on the RX1 IQ stream
    // + VFO-A (vrx_id=0), VRX2 on the RX2 IQ stream + VFO-B (vrx_id=1).
    // Both gated at runtime by their own `VrxControlState` slot. The
    // optional `VRX_LIVE_DIR=<dir>` env still produces WAV captures -
    // VRX1 writes to the configured dir as before; VRX2 is wav-less to
    // avoid filename collisions (acceptable for dev tooling, can grow
    // a per-channel sub-dir later if needed).
    let vrx_dir = std::env::var("VRX_LIVE_DIR").ok();
    // VRX output rate is now per-VRX, decoupled from the global Thetis-WB
    // toggle (PATCH-vrx-wide-sam-ux): each VRX follows the NB/WB/Auto
    // dropdown (VRX_AUDIO_RATE_MODE) resolved against its own filter width.
    // Start narrowband; the loop bumps to WB on the first batch if needed.
    // Per-client VRX runtimes (PATCH-vrx-per-client). The audio loop owns these
    // maps; each client with a VRX audio subscription gets its own channelizer per
    // channel, created lazily in service_vrx_channel(). Per-client WAV capture is
    // disabled to avoid file collisions (§3f); the VRX_DUMP one-shot still works.
    let _ = &vrx_dir;
    let mut vrx1_rts: std::collections::HashMap<std::net::SocketAddr, ClientRt> =
        std::collections::HashMap::new();
    let mut vrx2_rts: std::collections::HashMap<std::net::SocketAddr, ClientRt> =
        std::collections::HashMap::new();
    let mut vrx1_sink = crate::vrx_bridge::ThetisVrxSink::new(socket.clone());
    let mut vrx2_sink = crate::vrx_bridge::ThetisVrxSink::new(socket.clone());
    let mut vrx1_timer = crate::vrx_bridge::VrxFeedTimer::new("RX1");
    let mut vrx2_timer = crate::vrx_bridge::VrxFeedTimer::new("RX2");

    let mut fft_size = spectrum.lock().await.ddc_fft_size();
    let mut rx2_fft_size = rx2_spectrum.lock().await.ddc_fft_size();
    let mut hop_size = sdr_remote_core::ddc_hop_size(fft_size);
    let mut rx2_hop_size = sdr_remote_core::ddc_hop_size(rx2_fft_size);
    let mut rx1_accum: Vec<(f32, f32)> = Vec::with_capacity(fft_size * 2);
    let mut rx2_accum: Vec<(f32, f32)> = Vec::with_capacity(rx2_fft_size * 2);
    let mut rx1_iq_rate: u32 = 0; // Detected from RX1 IQ frame headers
    let mut rx2_iq_rate: u32 = 0; // Detected from RX2 IQ frame headers (can differ from RX1)

    loop {
        if iq_rx1.is_none() || iq_rx2.is_none() {
            let mut ptt_guard = ptt.lock().await;
            if let Some(tci) = Some(&mut ptt_guard.tci) {
                if iq_rx1.is_none() { iq_rx1 = tci.iq_rx1_rx.take(); }
                if iq_rx2.is_none() { iq_rx2 = tci.iq_rx2_rx.take(); }
            }
            drop(ptt_guard);
            if iq_rx1.is_none() && iq_rx2.is_none() {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(200)) => continue,
                    _ = shutdown.changed() => break,
                }
            }
        }

        tokio::select! {
            result = async {
                if let Some(rx) = iq_rx1.as_mut() { rx.recv().await } else { std::future::pending().await }
            } => {
                let (frame_rate, iq_pairs) = match result {
                    Some(p) => p,
                    None => {
                        iq_rx1 = None;
                        rx1_accum.clear();
                        continue;
                    }
                };
                // Dynamic IQ sample rate detection from RX1 binary frame header
                if frame_rate != rx1_iq_rate && frame_rate > 0 {
                    info!("TCI RX1 IQ sample rate: {}kHz (was {}kHz)",
                        frame_rate / 1000, if rx1_iq_rate > 0 { rx1_iq_rate / 1000 } else { 0 });
                    rx1_iq_rate = frame_rate;
                    spectrum.lock().await.update_sample_rate(frame_rate);
                    fft_size = spectrum.lock().await.ddc_fft_size();
                    hop_size = sdr_remote_core::ddc_hop_size(fft_size);
                    rx1_accum.clear();
                }
                rx1_accum.extend_from_slice(&iq_pairs);
                if let Some(dump) = vrx_dump.as_mut() {
                    if !dump.finished {
                        dump.write_batch(frame_rate, &iq_pairs);
                    }
                }
                {
                    // VRX1 per client (RX1 IQ + VFO-A). Snapshot spectrum + the
                    // per-client subscription/autotune sets, then service each
                    // client's own runtime (see service_vrx_channel). TX-mute +
                    // per-client NB/WB rate handled inside the helper.
                    let (vfo_hz, ddc_center_hz) = {
                        let spec = spectrum.lock().await;
                        (spec.vfo_freq_hz(), spec.ddc_center_hz())
                    };
                    let (audio_addrs, auto_addrs, active) = {
                        let sess = session.lock().await;
                        (sess.vrx_audio_addrs(0), sess.vrx_autotune_addrs(0), sess.active_addrs())
                    };
                    // Cleanup vangnet (§3a): drop per-client control-state for clients
                    // no longer active-authed - covers both explicit disconnect and
                    // session timeout/stale (is_active_authed). Runtimes auto-prune in
                    // service_vrx_channel via the subscriber set.
                    {
                        let (dropped, count) = {
                            let mut m = vrx_mgr.lock().unwrap();
                            (m.retain_active(&active), m.client_count())
                        };
                        if dropped > 0 {
                            info!("VRX retain_active: dropped {} stale client(s) - vrx_clients={}", dropped, count);
                        }
                    }
                    let tx_active = ptt.lock().await.is_tx_or_prefill();
                    let ts = server_start.elapsed().as_millis() as u32;
                    service_vrx_channel(
                        0, frame_rate, &iq_pairs, vfo_hz, ddc_center_hz,
                        &audio_addrs, &auto_addrs, tx_active, ts,
                        &mut vrx1_rts, &vrx_mgr, &mut vrx1_sink, &mut vrx1_timer,
                    );
                }
                let cur_fft = spectrum.lock().await.ddc_fft_size();
                if cur_fft != fft_size {
                    fft_size = cur_fft;
                    hop_size = sdr_remote_core::ddc_hop_size(fft_size);
                    rx1_accum.clear();
                }
                while rx1_accum.len() >= fft_size {
                    let frame: Vec<(f32, f32)> = rx1_accum[..fft_size].to_vec();
                    rx1_accum.drain(..hop_size);
                    spectrum.lock().await.process_ddc_frame(&frame);
                    tokio::task::yield_now().await;
                }
            }
            result = async {
                if let Some(rx) = iq_rx2.as_mut() { rx.recv().await } else { std::future::pending().await }
            } => {
                let (frame_rate, iq_pairs) = match result {
                    Some(p) => p,
                    None => {
                        iq_rx2 = None;
                        rx2_accum.clear();
                        continue;
                    }
                };
                // Dynamic IQ sample rate detection from RX2 binary frame header
                if frame_rate != rx2_iq_rate && frame_rate > 0 {
                    info!("TCI RX2 IQ sample rate: {}kHz (was {}kHz)",
                        frame_rate / 1000, if rx2_iq_rate > 0 { rx2_iq_rate / 1000 } else { 0 });
                    rx2_iq_rate = frame_rate;
                    rx2_spectrum.lock().await.update_sample_rate(frame_rate);
                    rx2_fft_size = rx2_spectrum.lock().await.ddc_fft_size();
                    rx2_hop_size = sdr_remote_core::ddc_hop_size(rx2_fft_size);
                    rx2_accum.clear();
                }
                rx2_accum.extend_from_slice(&iq_pairs);
                {
                    // VRX2 per client (RX2 IQ + VFO-B). See VRX1 above.
                    let (vfo_hz, ddc_center_hz) = {
                        let spec = rx2_spectrum.lock().await;
                        (spec.vfo_freq_hz(), spec.ddc_center_hz())
                    };
                    let (audio_addrs, auto_addrs) = {
                        let sess = session.lock().await;
                        (sess.vrx_audio_addrs(1), sess.vrx_autotune_addrs(1))
                    };
                    let tx_active = ptt.lock().await.is_tx_or_prefill();
                    let ts = server_start.elapsed().as_millis() as u32;
                    service_vrx_channel(
                        1, frame_rate, &iq_pairs, vfo_hz, ddc_center_hz,
                        &audio_addrs, &auto_addrs, tx_active, ts,
                        &mut vrx2_rts, &vrx_mgr, &mut vrx2_sink, &mut vrx2_timer,
                    );
                }
                let cur_fft = rx2_spectrum.lock().await.ddc_fft_size();
                if cur_fft != rx2_fft_size {
                    rx2_fft_size = cur_fft;
                    rx2_hop_size = sdr_remote_core::ddc_hop_size(rx2_fft_size);
                    rx2_accum.clear();
                }
                while rx2_accum.len() >= rx2_fft_size {
                    let frame: Vec<(f32, f32)> = rx2_accum[..rx2_fft_size].to_vec();
                    rx2_accum.drain(..rx2_hop_size);
                    rx2_spectrum.lock().await.process_ddc_frame(&frame);
                    tokio::task::yield_now().await;
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(500)), if iq_rx1.is_none() || iq_rx2.is_none() => {
                continue;
            }
            _ = shutdown.changed() => break,
        }
    }
}
