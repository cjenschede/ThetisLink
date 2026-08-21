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

// VRX live channelizer + Opus encode + UDP send is fully
// delegated to the `vrx-rs` crate + `vrx_bridge::ThetisVrxSink`.
// `tci_iq_consumer` instantiates a `VrxRuntime` per VRX channel
// and passes each IQ batch on via `feed()`.

// ---- Resampling helpers ----

/// Resample i16 8kHz -> f32 device rate
pub fn resample_to_device(resampler: &mut impl rubato::Resampler<f32>, pcm_i16: &[i16]) -> Vec<f32> {
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
pub fn resample_to_network(resampler: &mut impl rubato::Resampler<f32>, pcm_f32: &[f32]) -> Vec<f32> {
    match resampler.process(&[pcm_f32.to_vec()], None) {
        Ok(result) => result.into_iter().next().unwrap_or_default(),
        Err(e) => {
            warn!("resample device->8k error: {}", e);
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

/// What a stretch of audio looks like, in the three numbers that separate
/// "clean" from "rough" without storing the audio itself.
///
/// `peak` and `clipped` find a signal that outgrew its headroom. `roughness()`
/// compares the energy in the difference between neighbouring samples against
/// the energy in the signal itself - which is a high-pass filter and its
/// output level, written as two running sums. Low for a signal whose energy
/// sits well below half the sample rate, higher the more of it sits up near
/// the top, and anything that adds harmonics up there - clipping, aliasing, a
/// codec pushed past its bitrate - raises it. Being a ratio of energies it
/// does not move when the operator turns the volume up, which is the whole
/// point of it.
#[derive(Default, Clone, Copy)]
struct LevelStats {
    peak: f32,
    clipped: u32,
    sum_sq: f64,
    sum_step_sq: f64,
    n: u64,
}

impl LevelStats {
    fn feed(&mut self, samples: &[f32]) {
        // The step across a frame boundary is not carried; one sample in every
        // 960 contributes nothing, which is below the precision of anything
        // this is used for.
        let mut prev = samples.first().copied().unwrap_or(0.0);
        for &s in samples {
            let a = s.abs();
            if a > self.peak { self.peak = a; }
            if a >= 0.999 { self.clipped += 1; }
            self.sum_sq += (s as f64) * (s as f64);
            let d = (s - prev) as f64;
            self.sum_step_sq += d * d;
            prev = s;
        }
        self.n += samples.len() as u64;
    }

    fn rms(&self) -> f64 {
        if self.n == 0 { 0.0 } else { (self.sum_sq / self.n as f64).sqrt() }
    }

    /// Zero for silence, where the ratio would be meaningless rather than
    /// large.
    fn roughness(&self) -> f64 {
        if self.sum_sq < 1e-12 { 0.0 } else { (self.sum_step_sq / self.sum_sq).sqrt() }
    }
}

/// When the receive encoders should spend bits on repeating themselves.
///
/// In-band FEC is not a free upgrade: Opus takes the redundancy out of the
/// same budget as the sound, and at 12.8 kbps that is audible - a station
/// heard it as receive audio that was rough where the same server's VRX, which
/// has never carried FEC, was clean (2026-08-13). Nor is it free to go
/// without: on a link that drops packets, no redundancy means holes.
///
/// So neither answer is right all the time, and this picks per link from what
/// the clients report rather than guessing once. It turns on readily and lets
/// go slowly: a burst of loss should be covered before it is understood, and a
/// link that has just misbehaved has not proved anything by being quiet for
/// one second.
#[derive(Default)]
struct LossProtection {
    on: bool,
    applied_pct: u8,
    clean_ticks: u32,
    pending: Option<(bool, u8)>,
}

impl LossProtection {
    /// Above this, protect. Below one percent, start counting towards letting
    /// go. Between the two, leave whatever is already in force.
    const ON_AT_PCT: u8 = 2;
    /// Twenty seconds of a clean link at 20 ms per tick.
    const CLEAN_TICKS_TO_RELEASE: u32 = 1000;

    fn update(&mut self, loss_pct: u8) {
        if loss_pct >= Self::ON_AT_PCT {
            self.clean_ticks = 0;
            // Told to Opus a little high: the figure it is given is what it
            // budgets for, and a burst that averages two percent does not
            // arrive spread evenly.
            let want = loss_pct.saturating_add(3).clamp(5, 20);
            if !self.on || want.abs_diff(self.applied_pct) >= 5 {
                self.on = true;
                self.applied_pct = want;
                self.pending = Some((true, want));
            }
            return;
        }
        if !self.on {
            return;
        }
        if loss_pct == 0 {
            self.clean_ticks += 1;
            if self.clean_ticks >= Self::CLEAN_TICKS_TO_RELEASE {
                self.on = false;
                self.applied_pct = 0;
                self.clean_ticks = 0;
                self.pending = Some((false, 0));
            }
        } else {
            self.clean_ticks = 0;
        }
    }

    /// The change to apply, once, if there is one.
    fn take_change(&mut self) -> Option<(bool, u8)> {
        self.pending.take()
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
    // Silence suppression off, and nothing else changed. A receiver is never
    // silent - band noise between the signals is the signal too - and the
    // voice-activity detector calls that silence and lets the far end invent
    // something in its place. VRX, on the same voice configuration but with
    // this one switch off, is the stream nobody calls rough.
    let mut enc_rx1 = OpusEncoder::new_rx_continuous()?;
    let mut enc_bin_r = OpusEncoder::new_rx_continuous()?;
    let mut enc_rx2 = OpusEncoder::new_rx_continuous()?;
    let mk_resampler = || rubato::SincFixedIn::<f32>::new(
        NETWORK_SAMPLE_RATE as f64 / tci_rate as f64, 1.0,
        hq_sinc_params(), tci_frame_samples, 1,
    );
    let mut res_rx1 = mk_resampler().context("RX1 resampler")?;
    let mut res_bin_r = mk_resampler().context("BinR resampler")?;
    let mut res_rx2 = mk_resampler().context("RX2 resampler")?;

    // Wideband (16 kHz) parallel-encoders - only actively fed when
    // at least one client has the Thetis-wideband-audio opt-in enabled.
    // The resamplers stay idle (no `process()` call) as long as no
    // client wants wideband - no noticeable CPU impact.
    let mut enc_rx1_wb = OpusEncoderWideband::new_rx_continuous()?;
    let mut enc_bin_r_wb = OpusEncoderWideband::new_rx_continuous()?;
    let mut enc_rx2_wb = OpusEncoderWideband::new_rx_continuous()?;
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

    // How this loop is being fed, summarised every ten seconds.
    //
    // For a fault nobody could otherwise see: RX1 audio that is clean from a
    // fresh server and slightly rough after the first PTT, and stays rough
    // until the server is restarted - while VRX1, which is made from IQ inside
    // this process, stays perfect throughout. That points at how Thetis's audio
    // arrives rather than at what is done with it, and this is the smallest
    // thing that can tell the two apart: how full the buffer is when a frame is
    // taken, how often there was nothing to take, and how often a backlog had
    // to be thrown away. A handful of counters and one line per ten seconds -
    // nothing per frame, nothing in the path itself.
    let mut m_ticks: u32 = 0;
    let mut m_starved: u32 = 0;
    let mut m_trimmed: u32 = 0;
    let mut m_fill_sum: u64 = 0;
    let mut m_fill_min: usize = usize::MAX;
    let mut m_fill_max: usize = 0;
    let mut m_since = Instant::now();
    // What the audio itself looks like, at the two places that can differ.
    //
    // The counters above say the feed is healthy, and rebuilding the wideband
    // resampler and encoder on resume did not cure the roughness, so the fault
    // is not in how much arrives nor in stale filter state. Three numbers say
    // what is left. Peak and the number of samples at full scale catch a level
    // that grew after transmitting and now clips - which sounds exactly like
    // this and hides from the narrowband stream, whose band edge sits below
    // where clipping puts its harmonics. The roughness ratio (mean step
    // between neighbouring samples over mean level) rises when high-frequency
    // content is added, whatever adds it.
    //
    // Taken on Thetis's own samples and again after the wideband resampler, so
    // the two can be told apart: if the input is unchanged and the output is
    // not, the resampler is doing it; if both change together, it arrives that
    // way and Thetis is where to look.
    let mut m_in = LevelStats::default();
    let mut m_wb = LevelStats::default();
    // The narrowband encoder's own input, for the level line below. Its
    // decoded counterpart used to be measured here too, along with a
    // subtraction that put a dB figure on what each codec did. Both are gone:
    // under a speech codec the subtraction measured phase rather than quality
    // - it read 4 dB on a stream that sounded fine - and the arithmetic behind
    // it was the worst thing this loop did all second. A loop whose first rule
    // is latency does not carry an instrument that cannot answer its question.
    let mut m_nb = LevelStats::default();
    // How long a tick takes to do its work, worst and average.
    //
    // Because the last attempt at this fault killed every audio stream on the
    // server at once - not only the one it touched - and the most likely way
    // for a change here to do that is to make this loop too slow to keep its
    // twenty milliseconds. Everything else on this server shares the same
    // runtime, so a bundler that overruns starves the lot. If that is what
    // happened it is here in plain numbers, and if it is not, that is worth
    // knowing too before anything else is blamed.
    // Whether the receive encoders are currently spending bits on redundancy.
    let mut loss_protect = LossProtection::default();

    let mut m_work_max = Duration::ZERO;
    let mut m_work_sum = Duration::ZERO;

    // Set when a tick found nothing to send; cleared on the first tick that
    // has audio again, which is where the backlog is dropped.
    let mut resume_pending = false;

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
            // Wait for tick or shutdown -- audio is drained non-blocking below
            _ = tick.tick() => {
                let work_started = Instant::now();
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
                if rx1_accum.len() > max { m_trimmed += 1; rx1_accum.drain(..rx1_accum.len() - max); }
                if rx2_accum.len() > max { rx2_accum.drain(..rx2_accum.len() - max); }
                if bin_r_accum.len() > max { bin_r_accum.drain(..bin_r_accum.len() - max); }
                // Measured before the frame is taken, so it says what was there
                // to work with.
                m_ticks += 1;
                let fill = rx1_accum.len();
                m_fill_sum += fill as u64;
                m_fill_min = m_fill_min.min(fill);
                m_fill_max = m_fill_max.max(fill);
                if m_since.elapsed() >= Duration::from_secs(10) {
                    if m_ticks > 0 {
                        info!(
                            "RX1 audio feed (10 s): {} ticks, {} with nothing to send, {} backlogs dropped; buffer frames avg {:.2} min {:.2} max {:.2}",
                            m_ticks,
                            m_starved,
                            m_trimmed,
                            m_fill_sum as f64 / m_ticks as f64 / tci_frame_samples as f64,
                            m_fill_min as f64 / tci_frame_samples as f64,
                            m_fill_max as f64 / tci_frame_samples as f64,
                        );
                        // Second line, so the first stays as it was and the
                        // two can be read against each other over a session.
                        info!(
                            "RX1 audio level (10 s): from Thetis peak {:.3} rms {:.4} rough {:.4} clipped {}; wideband out peak {:.3} rms {:.4} rough {:.4} clipped {}",
                            m_in.peak, m_in.rms(), m_in.roughness(), m_in.clipped,
                            m_wb.peak, m_wb.rms(), m_wb.roughness(), m_wb.clipped,
                        );
                        // Third line: what each codec does to its own input.
                        // The two paths are at different sample rates, so their
                        // numbers do not compare across - but each path's
                        // in-to-out change does, and that is the comparison the
                        // complaint is about.
                        // Above the threshold this is a warning, not a
                        // reading. Overrunning the tick is the one thing known
                        // to silence every audio stream on this server, and a
                        // number that looks identical whether it says 4 ms or
                        // 19 ms is an instrument, not an alarm. Two thirds of
                        // the budget leaves room to notice before it bites.
                        let avg_ms = m_work_sum.as_secs_f64() * 1000.0 / m_ticks as f64;
                        let worst_ms = m_work_max.as_secs_f64() * 1000.0;
                        const TICK_MS: f64 = 20.0;
                        const ALARM_AT: f64 = TICK_MS * 2.0 / 3.0;
                        if worst_ms >= ALARM_AT {
                            warn!(
                                "RX1 bundler work (10 s): avg {:.2} ms, worst {:.2} ms of the {:.0} ms tick - past {:.1} ms this loop starts starving every other audio stream",
                                avg_ms, worst_ms, TICK_MS, ALARM_AT,
                            );
                        } else {
                            info!(
                                "RX1 bundler work (10 s): avg {:.2} ms, worst {:.2} ms of the {:.0} ms tick",
                                avg_ms, worst_ms, TICK_MS,
                            );
                        }
                    }
                    m_work_max = Duration::ZERO;
                    m_work_sum = Duration::ZERO;
                    m_ticks = 0; m_starved = 0; m_trimmed = 0;
                    m_fill_sum = 0; m_fill_min = usize::MAX; m_fill_max = 0;
                    m_in = LevelStats::default();
                    m_wb = LevelStats::default();
                    m_nb = LevelStats::default();
                    m_since = Instant::now();
                }
                if rx1_accum.len() < tci_frame_samples {
                    m_starved += 1;
                    let work = work_started.elapsed();
                    m_work_sum += work;
                    if work > m_work_max { m_work_max = work; }
                    // Nothing came in: Thetis has paused its RX audio, which is
                    // what it does while transmitting. Remember it, so the
                    // burst that arrives when it resumes is not carried as
                    // latency for the rest of the session.
                    resume_pending = true;
                    continue;
                }

                // Back after a pause. Thetis hands over what it held in one
                // burst, and this loop only ever takes one frame per 20 ms
                // tick, so without this the whole burst stays in the buffer -
                // permanently. Measured at a station: one frame before a
                // transmission, two after the first, five after the next, and
                // it never came down again (2026-08-13). That is 80 ms of
                // latency bought with nothing, on a project whose first rule is
                // latency. Dropping back to a single frame costs the tail of a
                // pause nobody was listening to, and hands the operator their
                // twenty milliseconds back.
                if resume_pending {
                    resume_pending = false;
                    // Start the wideband chain afresh.
                    //
                    // Measured at a station on 2026-08-13: receive audio is
                    // clean from a fresh server, slightly rough from the first
                    // transmission onwards, and clean again the moment the
                    // server restarts - while a phone, which listens to the
                    // narrowband stream, never hears it at all, and switching
                    // the wideband option off makes it clean instantly. So the
                    // fault is in these two objects and nowhere else: they are
                    // the only thing that a server restart renews and that a
                    // client restart, or toggling the option, does not.
                    //
                    // A sinc resampler and a predictive codec both carry state
                    // across the pause a transmission makes, and neither was
                    // ever told the stream had stopped. Rebuilding them is what
                    // the server restart does, minus the restart. It costs the
                    // first frame after a pause - the resampler needs one to
                    // fill its window - which lands exactly where nobody was
                    // listening anyway.
                    //
                    // Tried at the station and it did not cure it: the
                    // roughness is still there after a transmission. Kept
                    // anyway, because resuming a sinc window and a predictive
                    // codec across a gap is wrong whether or not it is this
                    // fault, and it costs a frame nobody hears. What it does
                    // settle is that the cause is not stale state in these two
                    // - which is why the level statistics above were added, to
                    // say whether the audio arrives changed or is changed here.
                    if let (Ok(r), Ok(e)) = (mk_resampler_wb(), OpusEncoderWideband::new_rx_continuous()) {
                        res_rx1_wb = r;
                        enc_rx1_wb = e;
                        if let (Ok(r2), Ok(e2)) = (mk_resampler_wb(), OpusEncoderWideband::new_rx_continuous()) {
                            res_rx2_wb = r2;
                            enc_rx2_wb = e2;
                        }
                        if let (Ok(r3), Ok(e3)) = (mk_resampler_wb(), OpusEncoderWideband::new_rx_continuous()) {
                            res_bin_r_wb = r3;
                            enc_bin_r_wb = e3;
                        }
                        info!("Wideband audio chain rebuilt after the pause");
                    }
                    if rx1_accum.len() > tci_frame_samples {
                        let carried = rx1_accum.len() / tci_frame_samples;
                        rx1_accum.drain(..rx1_accum.len() - tci_frame_samples);
                        info!(
                            "RX1 audio resumed after a pause: dropped a {} frame ({} ms) backlog rather than carry it",
                            carried,
                            carried * 20
                        );
                    }
                    // The other channels are on the same stream and pause with
                    // it; left as they are they would drift apart from RX1.
                    if rx2_accum.len() > tci_frame_samples {
                        rx2_accum.drain(..rx2_accum.len() - tci_frame_samples);
                    }
                    if bin_r_accum.len() > tci_frame_samples {
                        bin_r_accum.drain(..bin_r_accum.len() - tci_frame_samples);
                    }
                }

                let addrs = {
                    let sess = session.lock().await;
                    // Yaesu-only clients (Android Yaesu-mode) get no Thetis-RX-audio
                    // -> data savings. Restores as soon as Yaesu-mode turns off (spectrum on).
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
                // Since wideband-opt-in: also a second payload-set per
                // channel (16 kHz Opus) when at least one active
                // client has the option enabled. NB path remains the
                // default for all current clients.
                let any_wb = session.lock().await.any_client_wants_thetis_wideband();
                let mut channels_nb: Vec<(u8, Vec<u8>)> = Vec::with_capacity(3);
                let mut channels_wb: Vec<(u8, Vec<u8>)> = Vec::with_capacity(3);

                // Helper: encode a 48-kHz frame in both channels
                // (NB always, WB conditional).
                fn pcm_to_i16(samples: &[f32]) -> Vec<i16> {
                    samples.iter()
                        .map(|&s| (s * 32767.0).clamp(-32768.0, 32767.0) as i16)
                        .collect()
                }

                // CH0: RX1 (always present)
                let rx1_frame: Vec<f32> = rx1_accum.drain(..tci_frame_samples).collect();
                m_in.feed(&rx1_frame);
                let rx1_8k = resample_to_network(&mut res_rx1, &rx1_frame);
                m_nb.feed(&rx1_8k);
                let rx1_i16 = pcm_to_i16(&rx1_8k);
                if rx1_i16.len() >= FRAME_SAMPLES {
                    if let Ok(opus) = enc_rx1.encode(&rx1_i16[..FRAME_SAMPLES]) {
                        channels_nb.push((0, opus));
                        audio_stats.rx1.tick(server_start);
                    }
                }
                if any_wb {
                    let rx1_16k = resample_to_network(&mut res_rx1_wb, &rx1_frame);
                    m_wb.feed(&rx1_16k);
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
                    let (client_modes, worst_loss): (Vec<(std::net::SocketAddr, u8, bool, bool, bool)>, u8) = {
                        let sess = session.lock().await;
                        let modes = addrs
                            .iter()
                            .map(|&a| (
                                a,
                                sess.client_audio_mode(a),
                                sess.client_rx1_enabled(a),
                                sess.client_rx2_enabled(a),
                                sess.client_thetis_wideband(a),
                            ))
                            .collect();
                        // The worst link decides. One set of encoders feeds
                        // everybody, so protection cannot be granted per
                        // listener - and the listener who needs it is the one
                        // whose audio falls apart without it, not the one who
                        // would rather have the last decibel.
                        let worst = addrs.iter().map(|&a| sess.client_loss(a)).max().unwrap_or(0);
                        (modes, worst)
                    };
                    loss_protect.update(worst_loss);
                    if let Some((on, pct)) = loss_protect.take_change() {
                        // Checked, not discarded. If the encoder refuses, the
                        // protection simply does not happen - and the whole
                        // point of it is the link where its absence is audible.
                        let mut failed = 0usize;
                        for e in [&mut enc_rx1, &mut enc_bin_r, &mut enc_rx2] {
                            if e.set_loss_protection(on, pct).is_err() { failed += 1; }
                        }
                        for e in [&mut enc_rx1_wb, &mut enc_bin_r_wb, &mut enc_rx2_wb] {
                            if e.set_loss_protection(on, pct).is_err() { failed += 1; }
                        }
                        if failed > 0 {
                            warn!("RX audio: {} of 6 encoders refused the error-correction change - they keep their previous setting", failed);
                        }
                        if on {
                            info!("RX audio: packet loss {}% - error correction on at {}%", worst_loss, pct);
                        } else {
                            info!("RX audio: link clean again - error correction off, all bits to the sound");
                        }
                    }

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
                        // Pick the right payload-set: WB if the client has opt-in
                        // and a WB-payload is available for this frame;
                        // otherwise narrowband (default for all current clients).
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
                                // RX1-audio-subscription: CH0 (RX1) + CH1 (RX1 imag/BIN)
                                // are dropped if the client has RX1-audio off (VRX-only).
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
                let work = work_started.elapsed();
                m_work_sum += work;
                if work > m_work_max { m_work_max = work; }
            }
            _ = shutdown.changed() => break,
        }
    }

    info!("Multi-channel audio bundler stopped");
    Ok(())
}

// ---- Yaesu audio loop ----

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
    // Dual-radio (Option B-prime): slot 0 -> yaesu_addrs + AudioYaesu (byte-identical
    // to the existing path); slot 1 -> yaesu2_addrs + AudioYaesu2.
    slot: u8,
    // Live radio-status (for the software-squelch). FTX-1: squelch_open from the
    // RI-poll gates the USB-audio. 991A: squelch_open stays true -> no effect.
    // std::sync::Mutex (matches YaesuRadio.status; audio_loops' `Mutex` = tokio).
    status: Arc<std::sync::Mutex<crate::yaesu::YaesuState>>,
) -> Result<()> {
    let audio_ptype = if slot == 0 { PacketType::AudioYaesu } else { PacketType::AudioYaesu2 };
    let frame_samples = (sample_rate * 20 / 1000) as usize;

    // Radio-RX bandwidth follows the Thetis-wideband-toggle (build 122):
    // the client picks NB (low data usage) or WB (clear, CELT instead of
    // SILK -> faithful noise) in the Server-tab. One global button for Thetis
    // + both radios. We therefore keep both encoders/resamplers running and
    // send each subscriber the format that client wants (`client_thetis_wideband`),
    // with the `AUDIO_WIDEBAND`-flag on WB-packets. Mirror of the Thetis-multi-ch-path.
    // The same receive configuration as the Thetis path and VRX: no silence
    // suppression, and redundancy only when the link is losing packets. These
    // two used to differ from each other and from everything else - the
    // narrowband one on the Audio model with fixed protection, the wideband one
    // still on the voice defaults with silence suppression on, which is exactly
    // the setting an operator heard as rough on the Thetis path. Four receive
    // configurations meant a fault could sit in one of them unnoticed while
    // the others were being fixed.
    let mut enc_nb = OpusEncoder::new_rx_continuous()?;
    let mut enc_wb = OpusEncoderWideband::new_rx_continuous()?;
    let mut loss_protect = LossProtection::default();
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

    // Software-squelch gate-envelope (FTX-1: closed squelch -> fade to silence;
    // the squelch-knob on the radio is the threshold). 991A: squelch_open=true -> no-op.
    let mut gate_gain: f32 = 1.0;
    let mut sql_closed_frames: u32 = 0;
    const SQL_HANG_FRAMES: u32 = 8;   // ~160 ms hang before the gate closes (anti-flutter)
    const SQL_FADE_STEP: f32 = 0.10;  // ~10 frames ≈ 200 ms full fade

    info!("Yaesu audio RX loop started ({}Hz capture, NB+WB on demand)", sample_rate);

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
                // Subscribers + their WB-preference. RX-bandwidth follows the Thetis-toggle
                // per client; TX always stays wideband (see network.rs).
                let (subs, worst_loss): (Vec<(std::net::SocketAddr, bool)>, u8) = {
                    let s = session.lock().await;
                    let addrs = if slot == 0 { s.yaesu_addrs() } else { s.yaesu2_addrs() };
                    let worst = addrs.iter().map(|&a| s.client_loss(a)).max().unwrap_or(0);
                    (addrs.into_iter().map(|a| (a, s.client_thetis_wideband(a))).collect(), worst)
                };
                loss_protect.update(worst_loss);
                if let Some((on, pct)) = loss_protect.take_change() {
                    if enc_nb.set_loss_protection(on, pct).is_err()
                        || enc_wb.set_loss_protection(on, pct).is_err()
                    {
                        warn!("Radio {} audio: an encoder refused the error-correction change - it keeps its previous setting", slot + 1);
                    }
                    if on {
                        info!("Radio {} audio: packet loss {}% - error correction on at {}%", slot + 1, worst_loss, pct);
                    } else {
                        info!("Radio {} audio: link clean again - error correction off", slot + 1);
                    }
                }
                if subs.is_empty() {
                    accumulator.clear();
                    had_clients = false;
                    continue;
                }

                if !had_clients {
                    match (OpusEncoder::new_rx_continuous(), OpusEncoderWideband::new_rx_continuous()) {
                        (Ok(n), Ok(w)) => {
                            enc_nb = n;
                            enc_wb = w;
                            sequence = 0;
                            accumulator.clear();
                            had_clients = true;
                            loss_protect = LossProtection::default();
                            info!("Yaesu audio: client(s) enabled, encoders reset");
                        }
                        _ => {
                            log::error!("Yaesu encoder reset failed - Yaesu audio RX skipped this tick (server keeps running)");
                            // had_clients stays false -> retry next tick if clients still present.
                        }
                    }
                    continue;
                }

                if accumulator.len() < frame_samples {
                    continue;
                }
                let mut frame: Vec<f32> = accumulator.drain(..frame_samples).collect();

                // Software-squelch: fade to silence on closed squelch (only FTX-1
                // sets squelch_open=false; 991A stays open -> start_g/end_g==1.0 -> no-op).
                // ONLY in the FM-family (internal mode 5: FM/FM-N/DATA-FM): on SSB/CW/AM/
                // RTTY/data the radio-BUSY (RI P8) has no meaningful value and reports
                // 'closed' while there IS audio -> always pass audio through there.
                // This prevents LSB from being muted by the USB-side squelch gate.
                let (sql_open, mode, tx_active) = {
                    let s = status.lock().unwrap();
                    (s.squelch_open, s.mode, s.tx_active)
                };
                // TX-mute: during transmit RX-audio must never come back. The 991A mutes
                // its USB-RX in hardware during TX; the FTX-1 does not -> there RX-sound
                // continued during PTT (operator-test 2026-07-04). Model-independent in software:
                // on TX go straight to silence (no squelch-hang), for the 991A a no-op
                // because (almost) silence is coming in there anyway.
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
                // Debug, not info. "Only the fade-edges, no per-frame spam" was
                // right about the frames and wrong about the rate: this gate
                // follows the signal, so on a busy channel it flips several
                // times a SECOND - 62 ms between two edges in one operator's
                // log, 52 lines out of 627 from this one pair. What the gate is
                // doing is not an event; the radio's own squelch state is, and
                // that is logged separately and rarely (`squelch: OPEN (BUSY)`).
                if start_g > 0.0 && end_g == 0.0 {
                    log::debug!("Yaesu squelch: gate closed - audio muted");
                } else if start_g == 0.0 && end_g > 0.0 {
                    log::debug!("Yaesu squelch: gate open - audio resumed");
                }

                let need_wb = subs.iter().any(|(_, wb)| *wb);
                let need_nb = subs.iter().any(|(_, wb)| !*wb);
                let timestamp = start.elapsed().as_millis() as u32;

                // Encode only the requested formats (usually just one).
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

// ---- TCI IQ consumer ----

/// Drains IQ channels from TCI and feeds spectrum processors (RX1 + RX2).
/// Also runs the VRX channelizer on the RX1 IQ stream and emits VrxAudioPacket
/// UDP frames to subscribed clients (separate-channel VRX audio).
/// One per-client VRX channelizer runtime + its shared control-state Arc and the
/// current resolved wideband flag. Owned by the audio-loop task (not the manager).
struct ClientRt {
    runtime: vrx_rs::VrxRuntime,
    control: Arc<std::sync::Mutex<vrx_rs::VrxControlState>>,
    current_wb: bool,
    /// When this runtime was built, for the first-frame timing line.
    created: std::time::Instant,
    first_frame_logged: bool,
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
    // Reported loss per listener, in the same order as `audio_addrs`.
    losses: &[u8],
    protect: &mut std::collections::HashMap<std::net::SocketAddr, LossProtection>,
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
    protect.retain(|a, _| audio_addrs.contains(a));
    // Mute during TX: keep the runtimes (state intact), skip DSP/Opus/UDP.
    if tx_active {
        return;
    }
    // Unlike the two shared streams, a VRX runtime belongs to one listener, so
    // it can be protected on that listener's own link rather than on the worst
    // in the room.
    for (addr, loss) in audio_addrs.iter().zip(losses.iter()) {
        let p = protect.entry(*addr).or_default();
        p.update(*loss);
        if let Some((on, pct)) = p.take_change() {
            if let Some(rt) = rts.get_mut(addr) {
                rt.runtime.set_loss_protection(on, pct);
            }
            if on {
                log::info!("VRX{} for {}: packet loss {}% - error correction on at {}%", ch + 1, addr, loss, pct);
            } else {
                log::info!("VRX{} for {}: link clean again - error correction off", ch + 1, addr);
            }
        }
    }
    let mut total_feed = std::time::Duration::ZERO;
    for &addr in audio_addrs {
        if !rts.contains_key(&addr) {
            let control = mgr.lock().unwrap().control(addr, ch);
            // Start at the rate this channel's filter actually calls for. Creating
            // narrowband unconditionally meant every enable of a wideband channel
            // built a runtime, ran a few frames through it, and then tore it down
            // again for the NB->WB switch below - audible as a late, distorted
            // start right after switching the audio on.
            let rate_mode = mgr.lock().unwrap().rate_mode(&addr, ch);
            let (lo, hi) = {
                let s = control.lock().unwrap();
                (s.filter_low_hz, s.filter_high_hz)
            };
            let wb = vrx_desired_wb(rate_mode, lo, hi, false);
            let runtime = vrx_rs::VrxRuntime::new(
                vrx_rs::VrxRuntimeOptions { vrx_id: ch, wav_dir: None, wav_segment_sec: 10, wideband: wb },
                control.clone(),
            );
            rts.insert(addr, ClientRt { runtime, control, current_wb: wb, created: std::time::Instant::now(), first_frame_logged: false });
            log::info!("VRX create client={} ch={} wideband={} - active_runtimes(ch)={}",
                addr, ch + 1, wb, rts.len());
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
        let sent_before = sink.frames_sent;
        rt.runtime.feed(frame_rate, iq_pairs, vfo_hz, ddc_center_hz, sink);
        total_feed += t0.elapsed();
        // How long the client waited between switching this channel on and the
        // first audio frame leaving the server - the half of the delay that is
        // ours. Anything beyond this is transport or client side.
        if !rt.first_frame_logged && sink.frames_sent > sent_before {
            rt.first_frame_logged = true;
            log::info!(
                "VRX ch={} first frame sent {} ms after create",
                ch + 1,
                rt.created.elapsed().as_millis()
            );
        }
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
    let mut vrx1_protect: std::collections::HashMap<std::net::SocketAddr, LossProtection> =
        std::collections::HashMap::new();
    let mut vrx2_protect: std::collections::HashMap<std::net::SocketAddr, LossProtection> =
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
                    let (audio_addrs, auto_addrs, active, losses) = {
                        let sess = session.lock().await;
                        let a = sess.vrx_audio_addrs(0);
                        let l: Vec<u8> = a.iter().map(|&x| sess.client_loss(x)).collect();
                        (a, sess.vrx_autotune_addrs(0), sess.active_addrs(), l)
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
                        &losses, &mut vrx1_protect,
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
                    let (audio_addrs, auto_addrs, losses) = {
                        let sess = session.lock().await;
                        let a = sess.vrx_audio_addrs(1);
                        let l: Vec<u8> = a.iter().map(|&x| sess.client_loss(x)).collect();
                        (a, sess.vrx_autotune_addrs(1), l)
                    };
                    let tx_active = ptt.lock().await.is_tx_or_prefill();
                    let ts = server_start.elapsed().as_millis() as u32;
                    service_vrx_channel(
                        1, frame_rate, &iq_pairs, vfo_hz, ddc_center_hz,
                        &audio_addrs, &auto_addrs, tx_active, ts,
                        &mut vrx2_rts, &vrx_mgr, &mut vrx2_sink, &mut vrx2_timer,
                        &losses, &mut vrx2_protect,
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

#[cfg(test)]
mod tests {
    use super::LevelStats;

    /// A clean tone and a distorted one at the same level have to come out
    /// with different roughness, or the number is not worth logging.
    #[test]
    fn roughness_rises_with_added_high_frequency() {
        let n = 4800;
        let clean: Vec<f32> = (0..n)
            .map(|i| (i as f32 * std::f32::consts::TAU * 700.0 / 48000.0).sin() * 0.5)
            .collect();
        // Same tone, hard-limited: same peak region, extra harmonics.
        let squared: Vec<f32> = clean.iter().map(|&s| if s > 0.0 { 0.5 } else { -0.5 }).collect();

        let mut a = LevelStats::default();
        a.feed(&clean);
        let mut b = LevelStats::default();
        b.feed(&squared);

        assert!(b.roughness() > a.roughness() * 2.0,
            "clean {} vs squared {}", a.roughness(), b.roughness());
        // And the number does not follow the volume knob: the same tone at a
        // quarter of the level has to read the same.
        let quiet: Vec<f32> = clean.iter().map(|&s| s * 0.25).collect();
        let mut c = LevelStats::default();
        c.feed(&quiet);
        assert!((c.roughness() - a.roughness()).abs() < 1e-6);
    }

    /// Protection has to arrive on the first bad report and leave only after
    /// the link has stayed clean for a while - the other way round would
    /// switch on and off through every burst, which sounds worse than either
    /// setting held steady.
    #[test]
    fn loss_protection_arrives_fast_and_leaves_slow() {
        let mut p = super::LossProtection::default();

        p.update(0);
        assert_eq!(p.take_change(), None, "a clean link needs no change");

        p.update(4);
        let (on, pct) = p.take_change().expect("loss must switch protection on");
        assert!(on);
        assert!(pct >= 5, "asked for more than the average, got {pct}");
        assert_eq!(p.take_change(), None, "a change is applied once");

        // Still lossy: no churn while it stays in the same region.
        for _ in 0..50 {
            p.update(4);
            assert_eq!(p.take_change(), None);
        }

        // One clean second is not proof; twenty is.
        for _ in 0..50 {
            p.update(0);
            assert_eq!(p.take_change(), None, "let go too early");
        }
        for _ in 0..super::LossProtection::CLEAN_TICKS_TO_RELEASE {
            p.update(0);
        }
        assert_eq!(p.take_change(), Some((false, 0)), "never let go");
    }

    /// A single lost packet in the middle of a quiet spell must not reset the
    /// wait, but it must not be treated as clean either.
    #[test]
    fn loss_protection_holds_through_a_blip() {
        let mut p = super::LossProtection::default();
        p.update(6);
        assert!(p.take_change().unwrap().0);

        for _ in 0..(super::LossProtection::CLEAN_TICKS_TO_RELEASE - 1) {
            p.update(0);
        }
        p.update(1); // not enough to protect against, not clean either
        p.update(0);
        assert_eq!(p.take_change(), None, "the blip should have restarted the wait");
    }

    #[test]
    fn silence_is_not_infinitely_rough() {
        let mut s = LevelStats::default();
        s.feed(&[0.0; 960]);
        assert_eq!(s.roughness(), 0.0);
        assert_eq!(s.peak, 0.0);
    }

    #[test]
    fn full_scale_samples_are_counted() {
        let mut s = LevelStats::default();
        s.feed(&[0.5, 1.0, -1.0, 0.2, 0.9989]);
        assert_eq!(s.clipped, 2);
        assert!((s.peak - 1.0).abs() < 1e-6);
    }
}
