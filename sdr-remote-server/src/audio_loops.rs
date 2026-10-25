// SPDX-License-Identifier: GPL-2.0-or-later

//! Audio encoding/sending loops extracted from network.rs.
//! Provides multi-channel + Yaesu audio bundlers and an IQ consumer loop.

use std::sync::Arc;
use sdr_remote_core::dev_info;
use std::time::Instant;

use anyhow::{Context, Result};
use log::{info, warn};
use crate::tracked_socket::TrackedSocket;
use tokio::sync::{watch, Mutex};
use tokio::time::{interval, Duration};

use sdr_remote_core::audio_channels::{channel_allowed, receiver_count, worst_receiver};
use sdr_remote_core::codec::{OpusEncoder, OpusEncoderWideband};
use sdr_remote_core::protocol::*;
use sdr_remote_core::resample::{resampler, Resampler, UpFilter};
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
pub fn resample_to_device(resampler: &mut Resampler, pcm_i16: &[i16]) -> Vec<f32> {
    let input_f32: Vec<f32> = pcm_i16.iter().map(|&s| s as f32 / 32768.0).collect();
    match resampler.process(input_f32) {
        Ok(result) => result,
        Err(e) => {
            warn!("resample 8k->device error: {}", e);
            Vec::new()
        }
    }
}

/// Resample f32 device rate -> f32 8kHz
pub fn resample_to_network(resampler: &mut Resampler, pcm_f32: &[f32]) -> Vec<f32> {
    match resampler.process(pcm_f32.to_vec()) {
        Ok(result) => result,
        Err(e) => {
            warn!("resample device->8k error: {}", e);
            Vec::new()
        }
    }
}

/// What the receive resamplers from `from_hz` do, for the start line, read
/// from the resamplers themselves (build 261).
fn down_line(from_hz: u32, nb: &Resampler, wb: &Resampler) -> String {
    format!(
        "{} Hz to {} Hz {}; to {} Hz {}",
        from_hz, NETWORK_SAMPLE_RATE, nb, NETWORK_SAMPLE_RATE_WIDEBAND, wb
    )
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

/// How a receive loop is being fed, summarised every ten seconds.
///
/// One source for the line both receive loops print, so the Thetis stream and
/// the radios are measured the same way and read with one search. How full the
/// buffer is when a frame is taken, how often there was nothing to take, and
/// how often it was cut back at its ceiling of ten frames - which is where a
/// source clock running slightly ahead of the 20 ms tick would leave 200 ms of
/// standing delay (review of the receive path, 2026-10-09).
struct FeedStats {
    ticks: u32,
    starved: u32,
    trimmed: u32,
    fill_sum: u64,
    fill_min: usize,
    fill_max: usize,
    /// How often a side channel (RX2, the binaural right) was brought back to
    /// one frame - after a tick, or when the loop lines everything up after a
    /// pause. Apart from `trimmed` (build 150), so that `backlogs dropped`
    /// means the ceiling everywhere and this has a value of its own.
    cut: u32,
}

impl FeedStats {
    fn new() -> Self {
        Self { ticks: 0, starved: 0, trimmed: 0, fill_sum: 0, fill_min: usize::MAX, fill_max: 0, cut: 0 }
    }

    /// A tick, with what was in the buffer before a frame was taken.
    fn tick(&mut self, fill: usize) {
        self.ticks += 1;
        self.fill_sum += fill as u64;
        self.fill_min = self.fill_min.min(fill);
        self.fill_max = self.fill_max.max(fill);
    }

    fn starved(&mut self) {
        self.starved += 1;
    }

    fn trimmed(&mut self) {
        self.trimmed += 1;
    }

    fn cut(&mut self) {
        self.cut += 1;
    }

    fn cuts(&self) -> u32 {
        self.cut
    }

    fn ticks(&self) -> u32 {
        self.ticks
    }

    /// Whether the channel brought anything in this window. RX2 and the
    /// binaural right channel are only there when switched on, and a line of
    /// zeros for a channel that is off says nothing.
    fn delivered(&self) -> bool {
        self.fill_max > 0 || self.trimmed > 0 || self.cut > 0
    }

    /// The text for RX2 and the binaural right: the shared tail, and how often
    /// the channel was cut to one frame after a tick (build 151: pinned here
    /// rather than put together at the call).
    fn side_summary(&self, frame_samples: usize) -> String {
        format!("{}; {} cut to one frame", self.summary(frame_samples), self.cut)
    }

    /// The text after the loop's own name. Unchanged from the RX1 line it was
    /// taken from, so old and new logs read the same.
    fn summary(&self, frame_samples: usize) -> String {
        let per = |n: f64| n / frame_samples.max(1) as f64;
        let ticks = self.ticks.max(1) as f64;
        format!(
            "{} ticks, {} with nothing to send, {} backlogs dropped; buffer frames avg {:.2} min {:.2} max {:.2}",
            self.ticks,
            self.starved,
            self.trimmed,
            per(self.fill_sum as f64 / ticks),
            per(if self.ticks == 0 { 0.0 } else { self.fill_min as f64 }),
            per(self.fill_max as f64),
        )
    }
}

/// Everything the Thetis receive loop counts per ten-second window, so that
/// starting a new window is one statement (build 150). It used to be nine
/// variables reset in two places - after the report and when a listener
/// comes back - and a tenth added to one list and not the other would have
/// measured across a gap again, which is the fault build 149 repaired.
struct RxWindow {
    /// How this loop is being fed.
    ///
    /// For a fault nobody could otherwise see: RX1 audio that is clean from a
    /// fresh server and slightly rough after the first PTT, and stays rough
    /// until the server is restarted - while VRX1, which is made from IQ inside
    /// this process, stays perfect throughout. That points at how Thetis's audio
    /// arrives rather than at what is done with it, and this is the smallest
    /// thing that can tell the two apart: how full the buffer is when a frame is
    /// taken, how often there was nothing to take, and how often a backlog had
    /// to be thrown away. A handful of counters and one line per ten seconds -
    /// nothing per frame, nothing in the path itself.
    feed: FeedStats,
    /// RX2 and the binaural right channel get their own count (build 149): they
    /// have the same ceiling as RX1 and are cut to one frame after every tick,
    /// and until then nothing showed either. Binaural left and right have to
    /// stay one unit; these lines are how to see that they do.
    feed_rx2: FeedStats,
    feed_bin_r: FeedStats,
    /// What the audio itself looks like, at the two places that can differ.
    ///
    /// The feed counters say the feed is healthy, and rebuilding the wideband
    /// resampler and encoder on resume did not cure the roughness, so the fault
    /// is not in how much arrives nor in stale filter state. Three numbers say
    /// what is left. Peak and the number of samples at full scale catch a level
    /// that grew after transmitting and now clips - which sounds exactly like
    /// this and hides from the narrowband stream, whose band edge sits below
    /// where clipping puts its harmonics. The roughness ratio (mean step
    /// between neighbouring samples over mean level) rises when high-frequency
    /// content is added, whatever adds it.
    ///
    /// Taken on Thetis's own samples and again after the wideband resampler, so
    /// the two can be told apart: if the input is unchanged and the output is
    /// not, the resampler is doing it; if both change together, it arrives that
    /// way and Thetis is where to look.
    m_in: LevelStats,
    m_wb: LevelStats,
    /// The narrowband encoder's own input, for the level line. Its decoded
    /// counterpart used to be measured here too, along with a subtraction that
    /// put a dB figure on what each codec did. Both are gone: under a speech
    /// codec the subtraction measured phase rather than quality - it read 4 dB
    /// on a stream that sounded fine - and the arithmetic behind it was the
    /// worst thing this loop did all second. A loop whose first rule is latency
    /// does not carry an instrument that cannot answer its question.
    m_nb: LevelStats,
    /// How long a tick takes to do its work, worst and total.
    ///
    /// Because the last attempt at this fault killed every audio stream on the
    /// server at once - not only the one it touched - and the most likely way
    /// for a change here to do that is to make this loop too slow to keep its
    /// twenty milliseconds. Everything else on this server shares the same
    /// runtime, so a bundler that overruns starves the lot. If that is what
    /// happened it is here in plain numbers, and if it is not, that is worth
    /// knowing too before anything else is blamed.
    m_work_max: Duration,
    m_work_sum: Duration,
    /// When this window began.
    m_since: Instant,
}

impl RxWindow {
    fn new() -> Self {
        Self {
            feed: FeedStats::new(),
            feed_rx2: FeedStats::new(),
            feed_bin_r: FeedStats::new(),
            m_in: LevelStats::default(),
            m_wb: LevelStats::default(),
            m_nb: LevelStats::default(),
            m_work_max: Duration::ZERO,
            m_work_sum: Duration::ZERO,
            m_since: Instant::now(),
        }
    }
}

/// The three receive buffers of the Thetis loop, and everything that happens
/// to them in one tick apart from the audio work itself (build 151). The loop
/// calls `step` and does the encoding and sending with what comes back; the
/// tests call the same `step`, so a reading rule for the feed lines is checked
/// against the arithmetic the loop runs and not against what it is expected
/// to do - three review rounds in a row found a rule that was not.
///
/// RX1 and the binaural right come from one TCI frame (`decode_audio_payload_stereo`
/// fills both in one loop), so the two buffers grow by the same amount. They
/// only part when the right is cut to one frame after a tick, which needs at
/// least three frames in the buffer.
struct ThetisBuffers {
    rx1: Vec<f32>,
    rx2: Vec<f32>,
    bin_r: Vec<f32>,
    /// Samples per frame of 20 ms.
    frame: usize,
    /// Someone was listening last tick: counting is on, and a new listener
    /// is one that finds this false.
    listening: bool,
    /// Set when a tick found nothing to send; cleared on the first tick that
    /// has audio again, which is where the backlog is dropped - and, after a
    /// pause of `PAUSE_TICKS` or more, the wideband chain rebuilt (build 160).
    resume_pending: bool,
    /// Empty ticks in a row, up to the one that has audio again.
    starved_ticks: u32,
    backlog: Backlog,
}

/// Empty ticks in a row before the Thetis loop calls it a pause and rebuilds
/// the wideband chain (build 160): a transmission or a real break, not one
/// block of Thetis audio that came a tick late. Until then every empty tick
/// rebuilt it - 708 times in one day's log (`logregels-build160.md`).
const PAUSE_TICKS: u32 = 5;

/// Ticks a receive buffer is watched before it may give anything back (one
/// second).
const BACKLOG_TICKS: u32 = 50;

/// Seconds a receive buffer remembers its lowest fill (build 161): a late
/// block once in ten seconds keeps its reserve.
const BACKLOG_MEMORY: usize = 10;

/// The standing backlog of a receive buffer, in both loops. Thetis could keep a
/// whole frame standing (`avg 1.9x`) and the 991A half of one (`avg 1.50`), and
/// with a sound card that runs ahead of the tick it climbed to the ceiling
/// (160 ms); until build 160 nothing took it away except a pause (Thetis) or
/// nothing at all (radio).
///
/// What may go is only what lay above one frame at every tick of the last
/// `BACKLOG_MEMORY` seconds - the reserve that caught a late block in that time
/// stays (review of build 160: taking every second's spare turned each later
/// late block into a gap). And it goes one frame at most per second, so a big
/// backlog leaves in steps of 20 ms rather than one jump (owner, 2026-10-10:
/// "200ms weggooien is wel hoorbaar ... stapje kleiner, maar daardoor wat
/// vaker"). An empty tick is a dip to nothing. The client's playout ring has a
/// rule of the same family (`after_tx::Settle`, build 159) but a different
/// question - how far a reading swings, in ms, at the speaker - so the two are
/// kept apart on purpose (working rule 15).
#[derive(Default)]
struct Backlog {
    ticks: u32,
    lowest: usize,
    lows: [usize; BACKLOG_MEMORY],
    next: usize,
}

impl Backlog {
    /// One tick, with the buffer's fill before a frame is taken. At the end of
    /// a second: how much may go - what lay above one frame through the whole
    /// memory, at most one frame, and only if it is at least half a frame.
    fn tick(&mut self, fill: usize, frame: usize) -> usize {
        if self.ticks == 0 || fill < self.lowest {
            self.lowest = fill;
        }
        self.ticks += 1;
        if self.ticks < BACKLOG_TICKS {
            return 0;
        }
        self.ticks = 0;
        self.lows[self.next] = self.lowest;
        self.next = (self.next + 1) % BACKLOG_MEMORY;
        let floor = self.lows.iter().copied().min().unwrap_or(0);
        let spare = floor.saturating_sub(frame);
        if spare < frame / 2 {
            return 0;
        }
        // At most one frame, and the memory moves down with it: it holds
        // fills, and after the step every one of them would read a step too
        // high - an old dip that used the reserve would then let the reserve
        // go too (build 162; review of 161: 161 left this out on the claim that
        // the minimum already covered it, which held only for a dip down to one
        // frame).
        let take = spare.min(frame);
        for low in &mut self.lows {
            *low = low.saturating_sub(take);
        }
        take
    }

    /// A tick with nothing to send: this second dipped to nothing, and the
    /// memory keeps that for `BACKLOG_MEMORY` seconds.
    fn dip(&mut self) {
        self.lowest = 0;
        self.ticks = self.ticks.max(1);
    }

    /// A new listener or a new stream: nothing remembered, so nothing goes for
    /// the first `BACKLOG_MEMORY` seconds.
    fn reset(&mut self) {
        *self = Backlog::default();
    }
}

/// What one tick of `ThetisBuffers::step` came to.
#[derive(Default)]
struct ThetisTick {
    /// RX1 had less than a frame: nothing to send this tick.
    starved: bool,
    /// The first tick with audio after a pause of `PAUSE_TICKS` or more: the
    /// wideband chain is rebuilt (build 160: it was after every empty tick).
    resumed: bool,
    /// The pause that ended: its length in ticks, and the RX1 samples
    /// dropped beyond the one frame kept (build 160: the line used to say
    /// "a 2 frame backlog" when one frame went).
    carried: Option<(u32, usize)>,
    /// RX1 samples dropped because the last seconds never needed them (builds
    /// 160, 161, `Backlog`).
    settled: Option<usize>,
    /// Nobody listens: nothing is taken.
    no_listener: bool,
    /// A listener arrived: the buffer lengths before they were lined up.
    aligned: Option<(usize, usize, usize)>,
    /// The frames to encode, and what RX1 kept after its own.
    rx1: Option<Vec<f32>>,
    rx1_left: usize,
    bin_r: Option<Vec<f32>>,
    rx2: Option<Vec<f32>>,
}

impl ThetisBuffers {
    fn new(frame: usize) -> Self {
        Self {
            rx1: Vec::with_capacity(frame * 4),
            rx2: Vec::with_capacity(frame * 4),
            bin_r: Vec::with_capacity(frame * 4),
            frame,
            listening: false,
            resume_pending: false,
            starved_ticks: 0,
            backlog: Backlog::default(),
        }
    }

    /// One tick, after the channels were drained into the buffers.
    ///
    /// The order is the loop's own and it matters: the ceiling, then the
    /// count, then nothing-to-send, then the end of a pause, then the
    /// listener check, then lining up for a new listener, then the frames,
    /// then cutting RX2 and the binaural right back to one frame.
    ///
    /// Back after a pause: Thetis hands over what it held in one burst, and
    /// this loop only ever takes one frame per 20 ms tick, so without the
    /// line-up the whole burst stays in the buffer - permanently. Measured at a
    /// station: one frame before a transmission, two after the first, five
    /// after the next, and it never came down again (2026-08-13). That is
    /// 80 ms of latency bought with nothing, on a project whose first rule is
    /// latency. Dropping back to a single frame costs the tail of a pause
    /// nobody was listening to, and hands the operator their twenty
    /// milliseconds back. The other channels are on the same stream and pause
    /// with it; left as they are they would drift apart from RX1.
    fn step(&mut self, win: &mut RxWindow, has_listener: bool) -> ThetisTick {
        let f = self.frame;
        let max = f * 10;
        // Counted only while someone listens (build 149). With nobody there
        // Thetis keeps delivering and nothing is taken, so the buffer sits at
        // its ceiling and is cut every tick - which read exactly like a clock
        // running ahead (review of build 148).
        let listening = self.listening;
        if self.rx1.len() > max {
            if listening { win.feed.trimmed(); }
            self.rx1.drain(..self.rx1.len() - max);
        }
        if self.rx2.len() > max {
            if listening { win.feed_rx2.trimmed(); }
            self.rx2.drain(..self.rx2.len() - max);
        }
        if self.bin_r.len() > max {
            if listening { win.feed_bin_r.trimmed(); }
            self.bin_r.drain(..self.bin_r.len() - max);
        }
        // Measured before the frame is taken, so it says what was there to
        // work with. Nothing-to-send is counted in the same tick, so it lands
        // in the same window as its tick (build 151).
        if listening {
            win.feed.tick(self.rx1.len());
            win.feed_rx2.tick(self.rx2.len());
            if self.rx2.len() < f { win.feed_rx2.starved(); }
            win.feed_bin_r.tick(self.bin_r.len());
            if self.bin_r.len() < f { win.feed_bin_r.starved(); }
        }
        let mut out = ThetisTick::default();
        if self.rx1.len() < f {
            // Nothing came in: Thetis has paused its RX audio, which is what it
            // does while transmitting. Remember it, so the burst that arrives
            // when it resumes is not carried as latency for the rest of the
            // session.
            if listening { win.feed.starved(); }
            self.starved_ticks += 1;
            self.resume_pending = true;
            self.backlog.dip();
            out.starved = true;
            return out;
        }
        let paused = std::mem::take(&mut self.starved_ticks);
        if self.resume_pending {
            // Every empty tick still lines all three up to one frame, so the
            // binaural right never runs ahead of the left after a late block -
            // that is how binaural has always worked, and it stays so (owner,
            // 2026-10-10). Only a real pause rebuilds the wideband chain and
            // gets a line (build 160).
            self.resume_pending = false;
            out.resumed = paused >= PAUSE_TICKS;
            if self.rx1.len() > f {
                if out.resumed {
                    out.carried = Some((paused, self.rx1.len() - f));
                }
                self.rx1.drain(..self.rx1.len() - f);
            }
            // Not counted as a cut (build 151): it brings left and right back
            // together rather than apart, and it is in the RX1 resume line.
            if self.rx2.len() > f {
                self.rx2.drain(..self.rx2.len() - f);
            }
            if self.bin_r.len() > f {
                self.bin_r.drain(..self.bin_r.len() - f);
            }
        }
        if !has_listener {
            self.listening = false;
            out.no_listener = true;
            return out;
        }
        if !self.listening {
            out.aligned = Some((self.rx1.len(), self.rx2.len(), self.bin_r.len()));
            if self.rx1.len() > f {
                self.rx1.drain(..self.rx1.len() - f);
            }
            if self.rx2.len() > f {
                self.rx2.drain(..self.rx2.len() - f);
            }
            if self.bin_r.len() > f {
                self.bin_r.drain(..self.bin_r.len() - f);
            }
            self.listening = true;
            self.backlog.reset();
            // A fresh ten-second window from the first listener on, as the
            // radio loop does (build 149): a window across the gap mixed ticks
            // with nobody listening into the reading.
            *win = RxWindow::new();
        }
        // What the last seconds never needed goes, oldest first, at most a frame,
        // from all three together so the binaural right stays with the left
        // (builds 160, 161).
        let spare = self.backlog.tick(self.rx1.len(), f);
        if spare > 0 {
            self.rx1.drain(..spare);
            let side = |b: &mut Vec<f32>| {
                let n = spare.min(b.len().saturating_sub(f));
                b.drain(..n);
            };
            side(&mut self.bin_r);
            side(&mut self.rx2);
            out.settled = Some(spare);
        }
        let rx1: Vec<f32> = self.rx1.drain(..f).collect();
        out.rx1_left = self.rx1.len();
        out.rx1 = Some(rx1);
        if self.bin_r.len() >= f {
            out.bin_r = Some(self.bin_r.drain(..f).collect());
        }
        if self.rx2.len() >= f {
            out.rx2 = Some(self.rx2.drain(..f).collect());
        }
        // RX1 keeps what is left after its frame and these do not, so a cut
        // here moves the binaural right a frame ahead of the left until the
        // next line-up after a pause (builds 149-151).
        if self.bin_r.len() > f {
            win.feed_bin_r.cut();
            self.bin_r.drain(..self.bin_r.len() - f);
        }
        if self.rx2.len() > f {
            win.feed_rx2.cut();
            self.rx2.drain(..self.rx2.len() - f);
        }
        out
    }
}

/// The receive buffer of a radio loop, and what happens to it in one tick
/// apart from the audio work (build 151), for the same reason as
/// `ThetisBuffers`: the loop and the tests run the same arithmetic.
struct RadioBuffer {
    samples: Vec<f32>,
    /// Samples per frame of 20 ms, at the sound card's rate.
    frame: usize,
    /// Someone was listening last tick and the encoders were made for them.
    listening: bool,
    backlog: Backlog,
    /// Samples dropped this tick because the last seconds never needed them
    /// (builds 160, 161); taken by the loop for its log line.
    settled: Option<usize>,
}

/// What one tick of `RadioBuffer::step` came to.
enum RadioTick {
    /// Nobody listens, or the encoders for a new listener could not be made.
    Idle,
    /// A listener arrived and the encoders were made: the next tick counts.
    Started,
    /// Less than a frame: nothing to send.
    Starved,
    /// A frame to send.
    Frame(Vec<f32>),
}

impl RadioBuffer {
    fn new(frame: usize) -> Self {
        Self { samples: Vec::with_capacity(frame * 4), frame, listening: false, backlog: Backlog::default(), settled: None }
    }

    fn push(&mut self, samples: &[f32]) {
        self.samples.extend_from_slice(samples);
    }

    /// One tick. `start` makes the encoders for a new listener and says
    /// whether that worked.
    ///
    /// The ceiling of ten frames comes first, once per tick and before any
    /// branch can return, as in the Thetis loop (build 149): it used to sit in
    /// the receive arm and counted every delivery of the sound card - about
    /// two per tick - so the two lines could not be compared. Counted only
    /// while someone listens.
    fn step(&mut self, feed: &mut FeedStats, has_listener: bool, start: impl FnOnce() -> bool) -> RadioTick {
        let max = self.frame * 10;
        if self.samples.len() > max {
            if self.listening { feed.trimmed(); }
            self.samples.drain(..self.samples.len() - max);
        }
        if !has_listener {
            self.samples.clear();
            self.listening = false;
            return RadioTick::Idle;
        }
        if !self.listening {
            if start() {
                self.samples.clear();
                self.listening = true;
                self.backlog.reset();
                return RadioTick::Started;
            }
            // Not cleared: the next tick tries again, and the ceiling holds it.
            return RadioTick::Idle;
        }
        // Measured before the frame is taken, as in the Thetis loop.
        feed.tick(self.samples.len());
        if self.samples.len() < self.frame {
            feed.starved();
            self.backlog.dip();
            return RadioTick::Starved;
        }
        // The same rule as the Thetis loop (build 160): until then nothing here
        // ever took a standing backlog away.
        let spare = self.backlog.tick(self.samples.len(), self.frame);
        if spare > 0 {
            self.samples.drain(..spare);
            self.settled = Some(spare);
        }
        RadioTick::Frame(self.samples.drain(..self.frame).collect())
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
    // Turnaround: the first RX1 block after TRX off that is not silence.
    let rx_back = ptt.lock().await.rx_back_watch();

    // Per-channel mono encoders + resamplers - narrowband (8 kHz).
    // Silence suppression off, and nothing else changed. A receiver is never
    // silent - band noise between the signals is the signal too - and the
    // voice-activity detector calls that silence and lets the far end invent
    // something in its place. VRX, on the same voice configuration but with
    // this one switch off, is the stream nobody calls rough.
    let mut enc_rx1 = OpusEncoder::new_rx_continuous()?;
    let mut enc_bin_r = OpusEncoder::new_rx_continuous()?;
    let mut enc_rx2 = OpusEncoder::new_rx_continuous()?;
    let mk_resampler = || resampler(tci_rate, NETWORK_SAMPLE_RATE, tci_frame_samples, UpFilter::Long);
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
    let mk_resampler_wb = || resampler(tci_rate, NETWORK_SAMPLE_RATE_WIDEBAND, tci_frame_samples, UpFilter::Long);
    let mut res_rx1_wb = mk_resampler_wb().context("RX1 WB resampler")?;
    let mut res_bin_r_wb = mk_resampler_wb().context("BinR WB resampler")?;
    let mut res_rx2_wb = mk_resampler_wb().context("RX2 WB resampler")?;
    info!("RX resamplers: {}", down_line(tci_rate, &res_rx1, &res_rx1_wb));

    let mut sequence: u32 = 0;
    // RX1, RX2 and the binaural right, and what a tick does to them (build 151).
    let mut bufs = ThetisBuffers::new(tci_frame_samples);
    let mut tick = interval(Duration::from_millis(20));

    // Everything counted per ten-second window: how the loop is fed, what the
    // audio looks like, how long a tick takes (see `RxWindow`).
    let mut win = RxWindow::new();
    // Whether the receive encoders are currently spending bits on redundancy.
    let mut loss_protect = LossProtection::default();

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
                drain_channel(&mut rx1_audio_rx, &mut bufs.rx1);
                drain_channel(&mut rx2_audio_rx, &mut bufs.rx2);
                drain_channel(&mut bin_r_audio_rx, &mut bufs.bin_r);
                let addrs = {
                    let sess = session.lock().await;
                    // Every active, authenticated client - see thetis_audio_addrs.
                    sess.thetis_audio_addrs()
                };
                // The ceiling, the count, a pause ending, a listener arriving and
                // the frames to send, in one place the tests run too (build 151).
                let step = bufs.step(&mut win, !addrs.is_empty());
                if win.m_since.elapsed() >= Duration::from_secs(10) {
                    if win.feed.ticks() > 0 {
                        dev_info!("RX1 audio feed (10 s): {}", win.feed.summary(tci_frame_samples));
                        // Named in advance (working rule 11, builds 149 and 150).
                        if win.feed_rx2.delivered() {
                            dev_info!("RX2 audio feed (10 s): {}", win.feed_rx2.side_summary(tci_frame_samples));
                        }
                        if win.feed_bin_r.delivered() {
                            dev_info!("BinR audio feed (10 s): {}", win.feed_bin_r.side_summary(tci_frame_samples));
                        }
                        // Second line, so the first stays as it was and the
                        // two can be read against each other over a session.
                        dev_info!(
                            "RX1 audio level (10 s): from Thetis peak {:.3} rms {:.4} rough {:.4} clipped {}; wideband out peak {:.3} rms {:.4} rough {:.4} clipped {}",
                            win.m_in.peak, win.m_in.rms(), win.m_in.roughness(), win.m_in.clipped,
                            win.m_wb.peak, win.m_wb.rms(), win.m_wb.roughness(), win.m_wb.clipped,
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
                        let avg_ms = win.m_work_sum.as_secs_f64() * 1000.0 / win.feed.ticks() as f64;
                        let worst_ms = win.m_work_max.as_secs_f64() * 1000.0;
                        const TICK_MS: f64 = 20.0;
                        const ALARM_AT: f64 = TICK_MS * 2.0 / 3.0;
                        if worst_ms >= ALARM_AT {
                            warn!(
                                "RX1 bundler work (10 s): avg {:.2} ms, worst {:.2} ms of the {:.0} ms tick - past {:.1} ms this loop starts starving every other audio stream",
                                avg_ms, worst_ms, TICK_MS, ALARM_AT,
                            );
                        } else {
                            dev_info!(
                                "RX1 bundler work (10 s): avg {:.2} ms, worst {:.2} ms of the {:.0} ms tick",
                                avg_ms, worst_ms, TICK_MS,
                            );
                        }
                    }
                    win = RxWindow::new();
                }
                if step.starved {
                    let work = work_started.elapsed();
                    win.m_work_sum += work;
                    if work > win.m_work_max { win.m_work_max = work; }
                    continue;
                }
                if let Some(n) = step.settled {
                    // Named in advance (working rule 11, build 160).
                    info!("RX1 audio: dropped {} ms of standing backlog the last {} s never needed", n * 20 / tci_frame_samples, BACKLOG_MEMORY);
                }
                if step.resumed {
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
                    if let Some((paused, dropped)) = step.carried {
                        info!(
                            "RX1 audio resumed after a pause of {} ms: dropped {} ms backlog rather than carry it",
                            paused * 20,
                            dropped * 20 / tci_frame_samples
                        );
                    }
                }
                if step.no_listener {
                    // Nobody hears Thetis: a turnaround wait would measure the gap
                    // until a client returns (round 99).
                    rx_back.cancel();
                    continue;
                }
                if let Some((rx1, rx2, binr)) = step.aligned {
                    info!("Multi-ch audio: client connected, aligning accumulators (rx1={} rx2={} binr={})",
                        rx1, rx2, binr);
                }
                let Some(rx1_frame) = step.rx1 else { continue; };

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
                rx_back.rx_block(&rx1_frame, step.rx1_left);
                win.m_in.feed(&rx1_frame);
                let rx1_8k = resample_to_network(&mut res_rx1, &rx1_frame);
                win.m_nb.feed(&rx1_8k);
                let rx1_i16 = pcm_to_i16(&rx1_8k);
                if rx1_i16.len() >= FRAME_SAMPLES {
                    if let Ok(opus) = enc_rx1.encode(&rx1_i16[..FRAME_SAMPLES]) {
                        channels_nb.push((0, opus));
                        audio_stats.rx1.tick(server_start);
                    }
                }
                if any_wb {
                    let rx1_16k = resample_to_network(&mut res_rx1_wb, &rx1_frame);
                    win.m_wb.feed(&rx1_16k);
                    let rx1_i16_wb = pcm_to_i16(&rx1_16k);
                    if rx1_i16_wb.len() >= FRAME_SAMPLES_WIDEBAND {
                        if let Ok(opus) = enc_rx1_wb.encode(&rx1_i16_wb[..FRAME_SAMPLES_WIDEBAND]) {
                            channels_wb.push((0, opus));
                        }
                    }
                }

                // CH1: BinR (only when Thetis binaural active)
                if let Some(frame) = step.bin_r {
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
                if let Some(frame) = step.rx2 {
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

                // Send per-client filtered multi-channel packets
                if !channels_nb.is_empty() {
                    let timestamp = start.elapsed().as_millis() as u32;
                    // Read per-client modes + rx2_enabled flag + WB-opt-in
                    // under short lock, then release. `rx2_enabled` gates
                    // CH2 even when `audio_mode` would otherwise allow it
                    // - the desktop client UI's "RX2 enabled" toggle must
                    // mute the upstream RX2 stream entirely, not just the
                    // local playback (bandwidth bug uncovered 2026-05-13).
                    let (client_modes, worst_pair, n_receiving): (
                        Vec<(std::net::SocketAddr, u8, bool, bool, bool)>,
                        Option<(std::net::SocketAddr, u8)>,
                        usize,
                    ) = {
                        let sess = session.lock().await;
                        let modes: Vec<(std::net::SocketAddr, u8, bool, bool, bool)> = addrs
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
                        // WHO is the worst, not just how bad. One set of encoders
                        // feeds everybody, so a single phone on a poor network gives
                        // a desktop on a perfect cable twenty percent redundancy -
                        // and until now the line said the percentage without saying
                        // whose it was. That cost an evening of looking at the wrong
                        // client (2026-09-13, open point 15a).
                        // Only clients we are actually SENDING to. thetis_audio_addrs
                        // returns every authenticated client, including one that has
                        // switched its Thetis audio off - and the loss such a client
                        // reports is about a stream it is not receiving. Letting that
                        // decide the protection for everybody is how a phone in
                        // data-saving mode could pin maximum redundancy on a desktop.
                        //
                        // "Receiving" is the same decision the channel filter below
                        // makes, taken from one place. Reading it as `rx1_enabled`
                        // alone was wrong in the opposite direction: a desktop in
                        // Mono, BIN or Split with RX1 off and RX2 on still gets CH2,
                        // and silently lost the protection its link was asking for.
                        let candidates: Vec<(std::net::SocketAddr, u8, bool, bool, u8)> = modes
                            .iter()
                            .map(|&(a, mode, rx1, rx2, _)| (a, mode, rx1, rx2, sess.client_loss(a)))
                            .collect();
                        let worst_pair = worst_receiver(&candidates);
                        let n_receiving = receiver_count(&candidates);
                        (modes, worst_pair, n_receiving)
                    };
                    let worst_loss = worst_pair.map_or(0, |(_, l)| l);
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
                            let who = worst_pair
                                .map_or_else(|| "?".to_string(), |(a, _)| a.to_string());
                            info!(
                                "RX audio: packet loss {}% at {} (worst of {} receiving client(s)) - error correction on at {}% for all of them, narrowband at {} kbps",
                                worst_loss, who,
                                n_receiving,
                                pct,
                                sdr_remote_core::codec::NB_PROTECTED_BITRATE / 1000
                            );
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
                                channel_allowed(*mode, *ch_id, *rx1_enabled, *rx2_enabled)
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
                win.m_work_sum += work;
                if work > win.m_work_max { win.m_work_max = work; }
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
    // The radio's turnaround watch, as Thetis has one (build 163).
    let rx_back = status.lock().unwrap().tx_timing.rx_back.clone();

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
    let mut res_nb = resampler(sample_rate, NETWORK_SAMPLE_RATE, frame_samples, UpFilter::Long)
        .context("create Yaesu NB resampler")?;
    let mut res_wb = resampler(sample_rate, NETWORK_SAMPLE_RATE_WIDEBAND, frame_samples, UpFilter::Long)
        .context("create Yaesu WB resampler")?;
    info!("Yaesu slot {} RX resamplers: {}", slot, down_line(sample_rate, &res_nb, &res_wb));

    let mut sequence: u32 = 0;
    // The buffer and what a tick does to it (build 151).
    let mut buf = RadioBuffer::new(frame_samples);
    let mut tick = interval(Duration::from_millis(20));

    // Software-squelch gate-envelope (FTX-1: closed squelch -> fade to silence;
    // the squelch-knob on the radio is the threshold). 991A: squelch_open=true -> no-op.
    let mut gate_gain: f32 = 1.0;
    let mut sql_closed_frames: u32 = 0;
    const SQL_HANG_FRAMES: u32 = 8;   // ~160 ms hang before the gate closes (anti-flutter)
    const SQL_FADE_STEP: f32 = 0.10;  // ~10 frames ≈ 200 ms full fade

    // The same feed line as the Thetis loop, while someone listens (step 1 of
    // the receive-path proposal, 2026-10-09: measure before changing anything).
    let mut feed = FeedStats::new();
    let mut feed_since = Instant::now();

    info!("Yaesu audio RX loop started ({}Hz capture, NB+WB on demand)", sample_rate);

    loop {
        tokio::select! {
            result = audio_rx.recv() => {
                match result {
                    Some(samples) => {
                        buf.push(&samples);
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
                let (subs, worst_pair): (
                    Vec<(std::net::SocketAddr, bool)>,
                    Option<(std::net::SocketAddr, u8)>,
                ) = {
                    let s = session.lock().await;
                    let addrs = if slot == 0 { s.yaesu_addrs() } else { s.yaesu2_addrs() };
                    // Same reasoning as the RX path above: name the listener whose
                    // link decides this for everybody.
                    let worst = addrs.iter().map(|&a| (a, s.client_loss(a))).max_by_key(|(_, l)| *l);
                    (addrs.into_iter().map(|a| (a, s.client_thetis_wideband(a))).collect(), worst)
                };
                let worst_loss = worst_pair.map_or(0, |(_, l)| l);
                loss_protect.update(worst_loss);
                if let Some((on, pct)) = loss_protect.take_change() {
                    if enc_nb.set_loss_protection(on, pct).is_err()
                        || enc_wb.set_loss_protection(on, pct).is_err()
                    {
                        warn!("Radio {} audio: an encoder refused the error-correction change - it keeps its previous setting", slot + 1);
                    }
                    if on {
                        let who = worst_pair
                            .map_or_else(|| "?".to_string(), |(a, _)| a.to_string());
                        info!(
                            "Radio {} audio: packet loss {}% at {} (worst of {} listener(s)) - error correction on at {}% for all of them, narrowband at {} kbps",
                            slot + 1, worst_loss, who, subs.len(), pct,
                            sdr_remote_core::codec::NB_PROTECTED_BITRATE / 1000
                        );
                    } else {
                        info!("Radio {} audio: link clean again - error correction off", slot + 1);
                    }
                }
                let step = buf.step(&mut feed, !subs.is_empty(), || {
                    match (OpusEncoder::new_rx_continuous(), OpusEncoderWideband::new_rx_continuous()) {
                        (Ok(n), Ok(w)) => {
                            enc_nb = n;
                            enc_wb = w;
                            sequence = 0;
                            loss_protect = LossProtection::default();
                            info!("Yaesu audio: client(s) enabled, encoders reset");
                            true
                        }
                        _ => {
                            log::error!("Yaesu encoder reset failed - Yaesu audio RX skipped this tick (server keeps running)");
                            false
                        }
                    }
                });
                if feed_since.elapsed() >= Duration::from_secs(10) {
                    if feed.ticks() > 0 {
                        // Named in advance (working rule 11, build 148).
                        dev_info!("Radio {} audio feed (10 s): {}", slot + 1, feed.summary(frame_samples));
                    }
                    feed = FeedStats::new();
                    feed_since = Instant::now();
                }
                if let Some(n) = buf.settled.take() {
                    // Named in advance (working rule 11, build 160).
                    info!("Radio {} audio: dropped {} ms of standing backlog the last {} s never needed", slot + 1, n * 20 / frame_samples, BACKLOG_MEMORY);
                }
                let mut frame = match step {
                    RadioTick::Frame(frame) => {
                        // The radio's "RX audio back" after TX0 (build 163).
                        rx_back.rx_block(&frame, buf.samples.len());
                        frame
                    }
                    RadioTick::Started => {
                        feed = FeedStats::new();
                        feed_since = Instant::now();
                        continue;
                    }
                    // Nobody hears the radio: a wait running now would time the
                    // gap until someone returns, not the turnaround (as Thetis).
                    RadioTick::Idle => {
                        rx_back.cancel();
                        continue;
                    }
                    RadioTick::Starved => continue,
                };

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
                    audio_stats.yaesu_rx[(slot as usize).min(1)].tick(server_start);
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
                // Named in advance (working rule 11, build 153).
                log::info!(
                    "VRX{} for {}: packet loss {}% - error correction on at {}%, narrowband at {} kbps",
                    ch + 1, addr, loss, pct, vrx_rs::opus::NB_PROTECTED_BITRATE / 1000
                );
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
    // The band-jump generation each window holds (`BandJump`, build 236).
    let mut rx1_window_gen: u32 = 0;
    let mut rx2_window_gen: u32 = 0;
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
                // A band jump: the window's samples are of the old band (build 236).
                if let Some(jump) = spectrum.lock().await.band_jump_since(rx1_window_gen) {
                    // Named in advance (`logregels-build236.md`).
                    info!("Spectrum RX1: band jump {} -> {} kHz, {} old samples dropped",
                        jump.from_hz / 1000, jump.to_hz / 1000, rx1_accum.len());
                    rx1_window_gen = jump.generation;
                    rx1_accum.clear();
                }
                while rx1_accum.len() >= fft_size {
                    let frame: Vec<(f32, f32)> = rx1_accum[..fft_size].to_vec();
                    rx1_accum.drain(..hop_size);
                    spectrum.lock().await.process_ddc_frame(&frame, rx1_window_gen);
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
                // As on RX1 (build 236).
                if let Some(jump) = rx2_spectrum.lock().await.band_jump_since(rx2_window_gen) {
                    // Named in advance (`logregels-build236.md`).
                    info!("Spectrum RX2: band jump {} -> {} kHz, {} old samples dropped",
                        jump.from_hz / 1000, jump.to_hz / 1000, rx2_accum.len());
                    rx2_window_gen = jump.generation;
                    rx2_accum.clear();
                }
                while rx2_accum.len() >= rx2_fft_size {
                    let frame: Vec<(f32, f32)> = rx2_accum[..rx2_fft_size].to_vec();
                    rx2_accum.drain(..rx2_hop_size);
                    rx2_spectrum.lock().await.process_ddc_frame(&frame, rx2_window_gen);
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
    use super::{FeedStats, LevelStats};

    /// The feed line keeps the exact text of the RX1 line it came from, so a
    /// search over old logs still finds the new ones (build 148).
    #[test]
    fn the_feed_line_reads_as_it_always_did() {
        let mut f = FeedStats::new();
        for fill in [960, 1920, 0] {
            f.tick(fill);
        }
        f.starved();
        f.trimmed();
        f.trimmed();
        assert_eq!(f.ticks(), 3);
        assert_eq!(
            f.summary(960),
            "3 ticks, 1 with nothing to send, 2 backlogs dropped; buffer frames avg 1.00 min 0.00 max 2.00"
        );
    }

    /// A channel that brought nothing in a window does not get a line; one
    /// that brought a frame, or was cut, does (build 149).
    #[test]
    fn a_silent_channel_has_nothing_to_say() {
        let mut f = FeedStats::new();
        f.tick(0);
        f.starved();
        assert!(!f.delivered());
        f.tick(960);
        assert!(f.delivered());
        let mut g = FeedStats::new();
        g.tick(0);
        g.trimmed();
        assert!(g.delivered());
    }

    /// Bringing a side channel back to one frame is counted apart from the
    /// ceiling, and a channel that was only cut still gets its line (build 150).
    #[test]
    fn a_cut_is_not_a_backlog() {
        let mut f = FeedStats::new();
        f.tick(0);
        f.cut();
        f.cut();
        assert_eq!(f.cuts(), 2);
        assert!(f.delivered());
        assert_eq!(
            f.summary(960),
            "1 ticks, 0 with nothing to send, 0 backlogs dropped; buffer frames avg 0.00 min 0.00 max 0.00"
        );
    }

    /// A new window starts empty in every part, so a report after a listener
    /// comes back carries nothing from before the gap (build 150).
    #[test]
    fn a_new_window_starts_empty() {
        let w = super::RxWindow::new();
        for f in [&w.feed, &w.feed_rx2, &w.feed_bin_r] {
            assert_eq!((f.ticks(), f.cuts(), f.delivered()), (0, 0, false));
        }
        assert_eq!((w.m_work_max, w.m_work_sum), (std::time::Duration::ZERO, std::time::Duration::ZERO));
        assert_eq!((w.m_in.peak, w.m_wb.peak, w.m_nb.peak), (0.0, 0.0, 0.0));
        assert!(w.m_since.elapsed() < std::time::Duration::from_secs(1));
    }

    /// One frame of `F` samples per unit; small so the runs are quick.
    const F: usize = 10;

    /// Run the Thetis buffers through `arrivals` (frames per tick), RX1, RX2
    /// and - when `stereo` - the binaural right getting the same each tick, as
    /// they do from one TCI frame.
    fn thetis_run(b: &mut super::ThetisBuffers, w: &mut super::RxWindow, arrivals: &[usize], stereo: bool, listener: bool) {
        for &n in arrivals {
            for _ in 0..n {
                push_frame(b, stereo);
            }
            let _ = b.step(w, listener);
        }
    }

    thread_local! {
        /// Every sample a value of its own, rising, so a test can see which
        /// frame left and in what order (build 153: with zeros, taking the
        /// newest frame instead of the oldest stayed green - review of 152).
        static NEXT: std::cell::Cell<f32> = std::cell::Cell::new(1.0);
    }

    fn next_frame() -> Vec<f32> {
        NEXT.with(|n| {
            let start = n.get();
            n.set(start + F as f32);
            (0..F).map(|i| start + i as f32).collect()
        })
    }

    /// One TCI frame: the same samples to RX1, RX2 and - when stereo - the
    /// binaural right, as they come from one payload.
    fn push_frame(b: &mut super::ThetisBuffers, stereo: bool) {
        let f = next_frame();
        b.rx1.extend_from_slice(&f);
        b.rx2.extend_from_slice(&f);
        if stereo {
            b.bin_r.extend_from_slice(&f);
        }
    }

    /// One late block of Thetis audio is not a pause: it is lined up as
    /// always (binaural unchanged), but no rebuild and no line. A real pause
    /// (five empty ticks) rebuilds, and its line says how long it was and what
    /// went (build 160; until then every empty tick rebuilt, 708 times a day).
    #[test]
    fn a_late_tick_is_not_a_pause_but_five_are() {
        let mut b = super::ThetisBuffers::new(F);
        let mut w = super::RxWindow::new();
        thetis_run(&mut b, &mut w, &[1, 1, 1], true, true);
        thetis_run(&mut b, &mut w, &[0], true, true);
        push_frame(&mut b, true);
        push_frame(&mut b, true);
        let t = b.step(&mut w, true);
        assert!(!t.resumed && t.carried.is_none(), "one late tick is not a pause");
        assert_eq!((b.rx1.len(), b.bin_r.len(), b.rx2.len()), (0, 0, 0), "lined up as always");
        thetis_run(&mut b, &mut w, &[0, 0, 0, 0, 0], true, true);
        for _ in 0..4 {
            push_frame(&mut b, true);
        }
        let t = b.step(&mut w, true);
        assert!(t.resumed);
        assert_eq!(t.carried, Some((5, 3 * F)), "five ticks paused, three frames dropped");
        thetis_run(&mut b, &mut w, &[1, 0, 0, 0, 0], true, true);
        push_frame(&mut b, true);
        push_frame(&mut b, true);
        let t = b.step(&mut w, true);
        assert!(!t.resumed && t.carried.is_none());
        assert_eq!(super::PAUSE_TICKS, 5, "100 ms");
    }

    /// A standing backlog goes in steps of one frame at most, one a second,
    /// and only once the whole memory saw it standing; on every tick that
    /// gives one back the right goes with the left, oldest first, so they come
    /// out as one (builds 160, 161; review of 160: no test looked at that tick,
    /// and "drop all above one frame" read the same as the rule).
    #[test]
    fn a_standing_backlog_leaves_in_steps_of_one_frame_with_left_and_right_together() {
        let mut b = super::ThetisBuffers::new(F);
        let mut w = super::RxWindow::new();
        thetis_run(&mut b, &mut w, &[1, 1, 1], true, true);
        // A burst of four extra frames, no empty tick.
        for _ in 0..5 {
            push_frame(&mut b, true);
        }
        let _ = b.step(&mut w, true);
        assert_eq!(b.rx1.len(), 4 * F);
        let mut steps = Vec::new();
        let first = |v: &Option<Vec<f32>>| v.as_ref().map(|f| f[0]).unwrap();
        let (mut prev_l, mut prev_r) = (0.0, 0.0);
        for tick in 0..super::BACKLOG_TICKS as usize * 20 {
            push_frame(&mut b, true);
            let before = b.rx1.len();
            let t = b.step(&mut w, true);
            let (l, r) = (first(&t.rx1), first(&t.bin_r));
            if let Some(n) = t.settled {
                assert_eq!(n, F, "one frame per step");
                assert_eq!(b.rx1.len(), before - F - F, "the step took one frame besides the one sent");
                assert!(t.bin_r.is_some(), "the right sent a frame on the step");
                // On the step the left skips exactly the one frame given back,
                // its oldest. The right, cut ahead at the burst, plays on or
                // skips with it - but never repeats a frame or goes back
                // (review of 160: taking the right's newest samples instead
                // stayed green).
                let (one, two) = (F as f32, 2.0 * F as f32);
                assert_eq!(l - prev_l, two, "tick {tick}: the left skips one frame");
                assert!(r - prev_r == one || r - prev_r == two, "tick {tick}: the right moved {} samples", r - prev_r);
                steps.push(tick / super::BACKLOG_TICKS as usize);
            }
            (prev_l, prev_r) = (l, r);
        }
        assert_eq!(steps, vec![10, 11, 12, 13], "not while the memory holds the second before the burst, then one a second");
        push_frame(&mut b, true);
        let t = b.step(&mut w, true);
        assert_eq!(t.rx1, t.bin_r, "left and right one again");
        // One extra frame on all three, level: on its step both skip it.
        push_frame(&mut b, true);
        push_frame(&mut b, true);
        let _ = b.step(&mut w, true);
        let mut level_steps = 0;
        let (mut prev_l, mut prev_r) = (0.0, 0.0);
        for tick in 0..super::BACKLOG_TICKS as usize * 12 {
            push_frame(&mut b, true);
            let t = b.step(&mut w, true);
            let (l, r) = (first(&t.rx1), first(&t.bin_r));
            if t.settled.is_some() {
                level_steps += 1;
                assert_eq!((l - prev_l, r - prev_r), (2.0 * F as f32, 2.0 * F as f32), "tick {tick}: both skip it");
            }
            (prev_l, prev_r) = (l, r);
        }
        assert_eq!(level_steps, 1);
        // A reserve that caught a late block within the memory stays.
        push_frame(&mut b, true);
        push_frame(&mut b, true);
        let _ = b.step(&mut w, true);
        let mut kept = true;
        for tick in 0..super::BACKLOG_TICKS as usize * 30 {
            // A late block every eight seconds: the tick reads one frame.
            let arrive = if tick % 400 == 0 { 0 } else if tick % 400 == 1 { 2 } else { 1 };
            for _ in 0..arrive {
                push_frame(&mut b, true);
            }
            kept &= b.step(&mut w, true).settled.is_none();
        }
        assert!(kept, "a reserve used every eight seconds was taken");
        assert_eq!(super::BACKLOG_MEMORY, 10, "ten seconds");
    }

    /// A reserve of two frames whose first frame is used every eight seconds:
    /// one step takes the frame that is never used, and then nothing - no
    /// second step, no empty tick. The memory moves down with the step, or an
    /// old dip reads a frame too high and the reserve goes too (build 162;
    /// review of 161: build 161 took both, and then every eighth second was a
    /// gap).
    #[test]
    fn a_reserve_used_now_and_then_stays_after_a_step() {
        // The late block at every place within its second, the deciding tick
        // included (build 163; review of 162: no phase put it in the second
        // that decides the step).
        for phase in 0..super::BACKLOG_TICKS as usize {
            reserve_run(phase);
        }
        // ...and in every second of its eight, so it also falls in the second
        // that decides a step (build 164; review of 163: C5 was still open).
        for phase in (0..398).step_by(13) {
            reserve_run(phase);
        }
    }

    fn reserve_run(phase: usize) {
        let mut b = super::ThetisBuffers::new(F);
        let mut w = super::RxWindow::new();
        thetis_run(&mut b, &mut w, &[1, 1, 1], true, true);
        // Three frames in one tick: two standing.
        for _ in 0..3 {
            push_frame(&mut b, true);
        }
        let _ = b.step(&mut w, true);
        assert_eq!(b.rx1.len(), 2 * F);
        let (mut steps, mut empty) = (0, 0);
        for tick in 0..super::BACKLOG_TICKS as usize * 40 {
            // Every eight seconds a block comes a tick late: that tick reads
            // one frame less, the next brings two.
            // The late tick at `phase` within its second, the catch-up right
            // after it - never a catch-up without its late tick before it.
            let arrive = match tick % 400 { t if t == phase => 0, t if t == phase + 1 => 2, _ => 1 };
            for _ in 0..arrive {
                push_frame(&mut b, true);
            }
            let t = b.step(&mut w, true);
            steps += t.settled.is_some() as u32;
            empty += t.starved as u32;
        }
        assert_eq!((steps, empty), (1, 0), "phase {phase}: one step for the frame never used, and no gap after it");
    }

    /// The memory itself: a dip on the very first tick of a second is kept
    /// (`ticks.max(1)`), and a reset forgets everything, so nothing goes for
    /// the next ten seconds (build 162; review of 161: neither was tested).
    #[test]
    fn the_backlog_memory_keeps_a_dip_on_a_seconds_first_tick_and_forgets_on_reset() {
        let f = F;
        let ticks = super::BACKLOG_TICKS as usize;
        // Two frames standing for ten seconds: one step.
        let mut m = super::Backlog::default();
        let mut taken = 0;
        for _ in 0..ticks * 10 {
            taken += m.tick(2 * f, f);
        }
        assert_eq!(taken, f);
        // A dip right after a decision, then a full second standing: kept.
        m.dip();
        for _ in 0..ticks - 1 {
            assert_eq!(m.tick(3 * f, f), 0);
        }
        assert_eq!(m.tick(3 * f, f), 0, "the dip on the first tick of the second was lost");
        for _ in 0..ticks * 9 {
            assert_eq!(m.tick(3 * f, f), 0, "the dip is remembered for ten seconds");
        }
        // A reset: nothing for ten seconds, then a step.
        let mut m = super::Backlog::default();
        for _ in 0..ticks * 10 {
            m.tick(3 * f, f);
        }
        m.reset();
        let mut first = None;
        for tick in 0..ticks * 12 {
            if m.tick(3 * f, f) > 0 && first.is_none() {
                first = Some(tick / ticks);
            }
        }
        assert_eq!(first, Some(9), "the first step on the tenth second after the reset");
    }

    /// The radio after a second start (a listener gone and back): the old
    /// memory is gone, so a standing half frame waits ten seconds again
    /// (build 162; review of 161).
    #[test]
    fn a_radio_that_starts_again_waits_ten_seconds() {
        let mut f = super::FeedStats::new();
        let mut r = super::RadioBuffer::new(F);
        assert!(matches!(r.step(&mut f, true, || true), super::RadioTick::Started));
        r.push(&next_frame()[..F / 2]);
        // A first session shorter than the memory (build 163; review of 162:
        // with twelve seconds a step had already lowered the old memory, and
        // the test held without the reset too).
        for _ in 0..super::BACKLOG_TICKS * 5 {
            r.push(&next_frame());
            let _ = r.step(&mut f, true, || true);
            assert!(r.settled.take().is_none());
        }
        assert!(matches!(r.step(&mut f, false, || true), super::RadioTick::Idle));
        assert!(matches!(r.step(&mut f, true, || true), super::RadioTick::Started));
        r.push(&next_frame()[..F / 2]);
        let mut first = None;
        for tick in 0..super::BACKLOG_TICKS as usize * 12 {
            r.push(&next_frame());
            let _ = r.step(&mut f, true, || true);
            if r.settled.take().is_some() && first.is_none() {
                first = Some(tick / super::BACKLOG_TICKS as usize);
            }
        }
        assert_eq!(first, Some(9), "not before the tenth second of the new start");
    }

    /// The radio loop under the same rule: a standing half frame (the 991A's
    /// `avg 1.50`) goes once the memory saw it for ten seconds, once; less than
    /// half a frame stays; an empty tick within the memory keeps the reserve
    /// (builds 160, 161; review of 160: without the memory a shifted chunk
    /// after the step became an empty tick, a whole frame, and a step again).
    #[test]
    fn a_radio_buffer_lets_a_standing_backlog_go_only_when_the_memory_never_needed_it() {
        let mut f = super::FeedStats::new();
        let mut r = super::RadioBuffer::new(F);
        assert!(matches!(r.step(&mut f, true, || true), super::RadioTick::Started));
        r.push(&next_frame()[..F / 2]);
        let mut settled = Vec::new();
        for _ in 0..super::BACKLOG_TICKS * 25 {
            r.push(&next_frame());
            assert!(matches!(r.step(&mut f, true, || true), super::RadioTick::Frame(fr) if fr.len() == F));
            settled.extend(r.settled.take());
        }
        assert_eq!(settled, vec![F / 2], "half a frame, once");
        // A standing quarter frame stays: not worth a skip.
        r.push(&next_frame()[..F / 4]);
        for _ in 0..super::BACKLOG_TICKS * 25 {
            r.push(&next_frame());
            let _ = r.step(&mut f, true, || true);
            assert!(r.settled.take().is_none());
        }
        // A standing frame, but an empty tick every eight seconds: it stays.
        r.push(&next_frame());
        for t in 0..super::BACKLOG_TICKS * 30 {
            if t % 400 == 399 {
                let lost = r.samples.len();
                r.samples.clear();
                let _ = r.step(&mut f, true, || true);
                r.push(&vec![0.0; lost]);
            }
            r.push(&next_frame());
            let _ = r.step(&mut f, true, || true);
            assert!(r.settled.take().is_none(), "tick {t}");
        }
    }

    /// VRX has its own encoder and its own copy of the two narrowband
    /// bitrates, because `vrx-rs` does not depend on the core crate. This
    /// server builds both, so it keeps them the same (build 153, working rule
    /// 15).
    #[test]
    fn the_narrowband_bitrates_agree() {
        assert_eq!(sdr_remote_core::codec::NB_RX_BITRATE, vrx_rs::opus::NB_RX_BITRATE);
        assert_eq!(sdr_remote_core::codec::NB_PROTECTED_BITRATE, vrx_rs::opus::NB_PROTECTED_BITRATE);
    }

    /// The oldest frame leaves first, on all three channels and on the radio
    /// (build 153; review of 152 found no test that looked at which samples
    /// left).
    #[test]
    fn the_oldest_frame_leaves_first() {
        let (mut b, mut w) = (super::ThetisBuffers::new(F), super::RxWindow::new());
        thetis_run(&mut b, &mut w, &[1; 3], true, true);
        push_frame(&mut b, true);
        let older = b.rx1[b.rx1.len() - F];
        push_frame(&mut b, true);
        let t = b.step(&mut w, true);
        assert_eq!(t.rx1.as_ref().unwrap()[0], older);
        assert_eq!(t.bin_r.as_ref().unwrap()[0], older);
        assert_eq!(t.rx2.as_ref().unwrap()[0], older);
        let (mut r, mut f) = (super::RadioBuffer::new(F), FeedStats::new());
        assert!(matches!(r.step(&mut f, true, || true), super::RadioTick::Started));
        let a = next_frame();
        r.push(&a);
        r.push(&next_frame());
        assert!(matches!(r.step(&mut f, true, || true), super::RadioTick::Frame(fr) if fr[0] == a[0]));
    }

    /// The right channel missing one frame that the left got shows as one
    /// more tick with nothing to send on BinR than on RX1 - the reading rule
    /// for a frame lost on one side (build 153; review of 152).
    #[test]
    fn a_frame_lost_on_the_right_shows_as_a_starved_tick_on_binr() {
        let (mut b, mut w) = (super::ThetisBuffers::new(F), super::RxWindow::new());
        thetis_run(&mut b, &mut w, &[1; 50], true, true);
        let f = next_frame();
        b.rx1.extend_from_slice(&f);
        b.rx2.extend_from_slice(&f);
        let _ = b.step(&mut w, true);
        thetis_run(&mut b, &mut w, &[1; 50], true, true);
        assert_eq!(w.feed_bin_r.starved, w.feed.starved + 1);
        assert_ne!(w.feed_bin_r.summary(F), w.feed.summary(F));
    }

    /// The reading rules of build 151, as the loop's own arithmetic: at one
    /// frame per tick everything sits at one frame, and left and right agree.
    #[test]
    fn steady_feed_reads_one_frame_everywhere() {
        let (mut b, mut w) = (super::ThetisBuffers::new(F), super::RxWindow::new());
        thetis_run(&mut b, &mut w, &[1; 500], true, true);
        let line = "499 ticks, 0 with nothing to send, 0 backlogs dropped; buffer frames avg 1.00 min 1.00 max 1.00";
        assert_eq!(w.feed.summary(F), line);
        assert_eq!(w.feed_bin_r.side_summary(F), format!("{}; 0 cut to one frame", line));
        assert_eq!(w.feed_rx2.side_summary(F), format!("{}; 0 cut to one frame", line));
    }

    /// A burst leaves a backlog at the ceiling, and the binaural right is cut
    /// once, in the window of the burst. Until build 160 the 9 frames (180 ms)
    /// then stood for good, with the left 8 frames behind the right. Since 161
    /// they go a frame a second once the memory has seen them for ten seconds,
    /// from left and right together, and then read one frame and come out as
    /// one.
    #[test]
    fn a_burst_is_cut_once_and_its_backlog_goes_after_a_second() {
        let (mut b, mut w) = (super::ThetisBuffers::new(F), super::RxWindow::new());
        thetis_run(&mut b, &mut w, &[1; 100], true, true);
        thetis_run(&mut b, &mut w, &[12], true, true);
        assert_eq!((w.feed.trimmed, w.feed_bin_r.cuts()), (1, 1), "the burst: one cut at the ceiling, one cut of the right");
        // The side channels have the same ceiling and are cut the same way
        // (build 152: RX2 had no test).
        assert_eq!((w.feed_rx2.trimmed, w.feed_bin_r.trimmed), (1, 1));
        assert_eq!(w.feed_rx2.cuts(), 1);
        w = super::RxWindow::new();
        thetis_run(&mut b, &mut w, &[1; 1000], true, true);
        w = super::RxWindow::new();
        thetis_run(&mut b, &mut w, &[1; 500], true, true);
        assert_eq!(w.feed.summary(F), "500 ticks, 0 with nothing to send, 0 backlogs dropped; buffer frames avg 1.00 min 1.00 max 1.00");
        assert_eq!(w.feed_bin_r.side_summary(F), "500 ticks, 0 with nothing to send, 0 backlogs dropped; buffer frames avg 1.00 min 1.00 max 1.00; 0 cut to one frame");
        assert_eq!(w.feed_rx2.side_summary(F), w.feed_bin_r.side_summary(F));
        push_frame(&mut b, true);
        let t = b.step(&mut w, true);
        assert_eq!(t.rx1, t.bin_r, "left and right one again");
    }

    /// Ordinary jitter - an empty tick, then two frames - is lined up after
    /// every empty tick, keeps left and right together, and is not a cut.
    #[test]
    fn jitter_is_lined_up_and_is_not_a_cut() {
        let (mut b, mut w) = (super::ThetisBuffers::new(F), super::RxWindow::new());
        thetis_run(&mut b, &mut w, &[1; 10], true, true);
        w = super::RxWindow::new();
        let pattern: Vec<usize> = (0..500).map(|t| if t % 2 == 0 { 0 } else { 2 }).collect();
        thetis_run(&mut b, &mut w, &pattern, true, true);
        let line = "500 ticks, 250 with nothing to send, 0 backlogs dropped; buffer frames avg 1.00 min 0.00 max 2.00";
        assert_eq!(w.feed.summary(F), line);
        assert_eq!(w.feed_bin_r.side_summary(F), format!("{}; 0 cut to one frame", line));
    }

    /// Over many random feeds of zero to three frames a tick:
    /// - `max` never passes the ceiling of ten frames;
    /// - `min 0.00` always comes with at least one tick with nothing to send;
    /// - while RX1 never held more than two frames, the binaural right reads
    ///   exactly as RX1 and was never cut - left and right cannot part.
    #[test]
    fn the_reading_rules_hold_over_random_feeds() {
        let mut seed = 0x2545_f491_u32;
        let mut parted_only_above_two = 0;
        let mut stayed_at_two = 0;
        for run in 0..300 {
            let (mut b, mut w) = (super::ThetisBuffers::new(F), super::RxWindow::new());
            // Half the runs low enough to stay at two frames - an empty tick
            // now and then, two frames only right after one - so the binaural
            // rule below is checked in its own state and not only skipped
            // (review of build 151: 0 of 300 runs reached it).
            let low = run % 2 == 0;
            let mut last = 1usize;
            let feed: Vec<usize> = (0..400)
                .map(|_| {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                    let r = (seed >> 16) % 8;
                    let n = if low {
                        match r { 0 => 0, _ if last == 0 && r < 4 => 2, _ => 1 }
                    } else {
                        match r { 0 => 0, 1 | 2 | 3 | 4 | 5 => 1, 6 => 2, _ => 3 }
                    };
                    last = n;
                    n
                })
                .collect();
            thetis_run(&mut b, &mut w, &feed, true, true);
            for f in [&w.feed, &w.feed_rx2, &w.feed_bin_r] {
                assert!(f.fill_max <= 10 * F, "max above the ceiling");
                if f.ticks > 0 && f.fill_min == 0 {
                    assert!(f.starved > 0, "min 0.00 without nothing-to-send");
                }
            }
            if w.feed.fill_max <= 2 * F {
                assert_eq!(w.feed_bin_r.summary(F), w.feed.summary(F));
                assert_eq!(w.feed_bin_r.cuts(), 0);
                stayed_at_two += 1;
            } else if w.feed_bin_r.summary(F) != w.feed.summary(F) {
                parted_only_above_two += 1;
            }
        }
        // Both states, or the test proves nothing about one of them.
        assert!(parted_only_above_two > 0, "the feeds never held three frames");
        assert!(stayed_at_two > 0, "the feeds never stayed at two frames");
    }

    /// Binaural left and right are one unit: a pause that ends with a burst
    /// lines both up together, so they come out of it the same length. Taking
    /// the right out of the line-up makes this red (build 151).
    #[test]
    fn a_pause_lines_left_and_right_up_together() {
        let f = 6usize;
        let mut b = super::ThetisBuffers::new(f);
        let mut w = super::RxWindow::new();
        for &k in &[1usize, 1, 1, 0, 3, 1, 1, 0, 4, 1, 1, 1] {
            for _ in 0..k * f {
                b.rx1.push(0.0);
                b.rx2.push(0.0);
                b.bin_r.push(0.0);
            }
            let _ = b.step(&mut w, true);
            assert_eq!(b.rx1.len(), b.bin_r.len());
            // RX2 is on the same stream and is lined up with it (build 152).
            assert_eq!(b.rx1.len(), b.rx2.len());
        }
    }

    /// A listener that leaves and comes back starts a fresh window, and finds
    /// the buffers - full to the ceiling while nobody took from them - lined
    /// up to one frame on all three, then emptied by the frame it gets
    /// (build 152: neither had a test).
    #[test]
    fn a_returning_listener_gets_a_fresh_window_and_lined_up_buffers() {
        let (mut b, mut w) = (super::ThetisBuffers::new(F), super::RxWindow::new());
        thetis_run(&mut b, &mut w, &[1; 100], true, true);
        assert_eq!(w.feed.ticks(), 99);
        // The listener leaves on a tick that brings eleven frames: the ceiling
        // cuts while `listening` is still true, so the old window counts it
        // (build 153: before, nothing was ever cut while counting, and the
        // check below could not fail - review of 152).
        thetis_run(&mut b, &mut w, &[11], true, false);
        assert_eq!(w.feed.trimmed, 1, "the cut on the way out is counted");
        thetis_run(&mut b, &mut w, &[1; 19], true, false);
        assert_eq!((b.rx1.len(), b.rx2.len(), b.bin_r.len()), (10 * F, 10 * F, 10 * F), "at the ceiling with nobody taking");
        push_frame(&mut b, true);
        let t = b.step(&mut w, true);
        assert_eq!(t.aligned, Some((10 * F, 10 * F, 10 * F)));
        assert_eq!((t.rx1.is_some(), t.rx2.is_some(), t.bin_r.is_some()), (true, true, true));
        assert_eq!((b.rx1.len(), b.rx2.len(), b.bin_r.len()), (0, 0, 0), "lined up to one frame, then taken");
        assert_eq!((w.feed.ticks(), w.feed_rx2.ticks(), w.feed_bin_r.ticks()), (0, 0, 0), "a fresh window");
        assert_eq!(w.feed.trimmed, 0, "nothing of the gap carried over");
    }

    /// Nobody listening: nothing is counted and nothing is taken.
    #[test]
    fn nothing_counts_without_a_listener() {
        let (mut b, mut w) = (super::ThetisBuffers::new(F), super::RxWindow::new());
        for _ in 0..50 {
            b.rx1.extend(std::iter::repeat(0.0).take(F));
            let t = b.step(&mut w, false);
            assert!(t.rx1.is_none());
        }
        assert_eq!((w.feed.ticks(), w.feed.trimmed), (0, 0));
    }

    /// Binaural off: the right channel never delivers, so it gets no line.
    #[test]
    fn a_mono_feed_gives_no_binaural_line() {
        let (mut b, mut w) = (super::ThetisBuffers::new(F), super::RxWindow::new());
        thetis_run(&mut b, &mut w, &[1; 100], false, true);
        assert!(w.feed.delivered());
        assert!(!w.feed_bin_r.delivered());
    }

    /// The radio buffer: half a frame per delivery, two deliveries a tick.
    fn radio_run(b: &mut super::RadioBuffer, f: &mut FeedStats, ticks: usize, extra: usize) {
        for t in 0..ticks {
            b.push(&[0.0; F / 2]);
            b.push(&[0.0; F / 2]);
            if t == 0 {
                for _ in 0..extra {
                    b.push(&[0.0; F]);
                }
            }
            let _ = b.step(f, true, || true);
        }
    }

    /// The radio reads one frame at a steady feed, and a burst fills it to the
    /// same ceiling as on Thetis - which, since builds 160 and 161, goes again a
    /// frame a second after ten seconds, as on Thetis.
    #[test]
    fn the_radio_buffer_reads_as_the_thetis_one() {
        let (mut b, mut f) = (super::RadioBuffer::new(F), FeedStats::new());
        radio_run(&mut b, &mut f, 101, 0);
        assert_eq!(f.summary(F), "100 ticks, 0 with nothing to send, 0 backlogs dropped; buffer frames avg 1.00 min 1.00 max 1.00");
        radio_run(&mut b, &mut f, 1, 12);
        f = FeedStats::new();
        radio_run(&mut b, &mut f, 1000, 0);
        f = FeedStats::new();
        radio_run(&mut b, &mut f, 500, 0);
        assert_eq!(f.summary(F), "500 ticks, 0 with nothing to send, 0 backlogs dropped; buffer frames avg 1.00 min 1.00 max 1.00");
    }

    /// Less than a frame: nothing to send, and counted - without this branch
    /// the take would panic on a short buffer (build 152: no test reached it).
    #[test]
    fn a_short_radio_buffer_starves_and_says_so() {
        let (mut b, mut f) = (super::RadioBuffer::new(F), FeedStats::new());
        assert!(matches!(b.step(&mut f, true, || true), super::RadioTick::Started));
        b.push(&[0.0; F / 2]);
        assert!(matches!(b.step(&mut f, true, || true), super::RadioTick::Starved));
        assert_eq!((f.ticks(), f.starved), (1, 1));
        b.push(&[0.0; F / 2]);
        assert!(matches!(b.step(&mut f, true, || true), super::RadioTick::Frame(fr) if fr.len() == F));
    }

    /// Encoders that cannot be made: nothing is counted, the buffer is kept
    /// under its ceiling, and the next tick tries again.
    #[test]
    fn a_failed_start_keeps_trying_under_the_ceiling() {
        let (mut b, mut f) = (super::RadioBuffer::new(F), FeedStats::new());
        for _ in 0..30 {
            b.push(&[0.0; F]);
            assert!(matches!(b.step(&mut f, true, || false), super::RadioTick::Idle));
            assert!(b.samples.len() <= 10 * F);
        }
        assert!(matches!(b.step(&mut f, true, || true), super::RadioTick::Started));
        assert_eq!(f.ticks(), 0);
        b.push(&[0.0; F]);
        assert!(matches!(b.step(&mut f, false, || true), super::RadioTick::Idle));
        assert!(b.samples.is_empty(), "nobody listening clears it");
    }

    /// A window without a single tick says zero, not the sentinel minimum.
    #[test]
    fn an_empty_window_says_zero() {
        assert_eq!(
            FeedStats::new().summary(960),
            "0 ticks, 0 with nothing to send, 0 backlogs dropped; buffer frames avg 0.00 min 0.00 max 0.00"
        );
    }

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
