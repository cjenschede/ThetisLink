// SPDX-License-Identifier: GPL-2.0-or-later
//! The replay of the transmit chain (step 3 of `PLAN-zendketen-samen-met-thetis.md`,
//! build 246). The arrival of every TX packet of a drive, as the server measured
//! it (`tx_arrivals.rs`), goes through the server's own code - the transmit
//! buffer (`JitterBuffer::for_transmit`), the tick's rule for a second pull
//! (`pull_extra_for_ring_below`) and the ring that answers Thetis (`TxRing`) - and then
//! through a model of Thetis's TX queue (`cmaster.cs`: `serviceTCITxProtocol`
//! and `OnTCITxAudioInSamples`). Per MOX it says what Thetis would have reported
//! (`tx_queue_report_ex` version 2), next to what Thetis did report.
//!
//! Modelled rather than run, each for a reason:
//! - the PTT machine around the tick (`ptt.rs`, `network.rs`): the prefill is
//!   checked at the start of a tick, the tail of `release_from_playout`; the real
//!   one is async and holds a TCI connection;
//! - the decoder: a packet with audio is one 20 ms frame; a gap is rebuilt from
//!   the next packet or concealed once there was audio, up to
//!   `CONCEAL_MAX_FRAMES` (the replay has no Opus data to decode);
//! - TCI between the server and Thetis: a fixed delay each way (same pc);
//! - Thetis: its queue, its requests and its counters.
//!
//! Not modelled at all (round 246, build 255): heartbeats (the measurement has
//! none, so a gap in the packets stands in for both safety clocks); the time-out
//! of a silent holder outside TX (`TX_HOLDER_TIMEOUT_SECS`, 3 s; inside TX the
//! safety takes the ownership first, build 251); Thetis's `"mox"` trigger; TRX
//! on while MOX is still on (a press inside Thetis's switch-off time); and the
//! receiver check before a transmitter is keyed.
//!
//! Stood in for (build 258): catching up in the pauses of speech (`pause_trim.rs`)
//! needs the level of each frame, and a measurement has no sound. The replay
//! catches up as before instead, one frame per `TRIM_EVERY_PULLS` wherever it
//! falls - in trial run 3 the two kept the buffer at the same depth (median target
//! 5, deepest 5 against 6) - so its latency holds and where the drops fall does not.
//!
//! Every frame carries its own number in its samples, so the model knows which
//! frame reached the transmitter and when. A sample of 0 that Thetis copies is
//! silence the ring padded; a sample Thetis could not copy is a zero of its own
//! (an underrun).
//!
//! A developer's tool (owner, 2026-10-12): the program is `--naspelen` with the
//! cargo feature `naspelen`, never in a release build.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sdr_remote_core::jitter::{BufferedFrame, JitterBuffer, JitterResult, Tuning, TX_MAX_DEPTH, TX_MIN_DEPTH};

use crate::tci::{pull_extra_for_ring_below, TxRing, Waiting, TX_RING_LOW};

const MS: u64 = 1_000_000;
/// The server's playout tick (`playout_tick` in `network.rs`).
const TICK_NS: u64 = 20 * MS;
/// One decoded frame at 48 kHz, as the ring gets it.
const FRAME: usize = 960;
const RATE: u64 = 48_000;
/// `PTT_PREFILL_MS_TCI`, `PTT_TAIL_MIN_MS_TCI` and `PTT_TAIL_MARGIN_MS_TCI` in
/// `ptt.rs` - a second place on purpose: the replay models the PTT machine
/// instead of running it. Pinned against `ptt.rs` by a test.
const PREFILL_NS: u64 = 10 * MS;
const TAIL_MIN_MS: u64 = 25;
const TAIL_MARGIN_MS: u64 = 10;
/// `CONCEAL_MAX_FRAMES` in `sdr-remote-core/src/stream.rs`, a second place on
/// purpose (no Opus data here); pinned by a test.
const CONCEAL_MAX_FRAMES: u32 = 400;
/// The safety release after silence from the client (`HEARTBEAT_TIMEOUT_MS`
/// and `PTT_PACKET_TIMEOUT_MS` in `ptt.rs`, both 2 s). The measurement has no
/// heartbeats, so a gap in the packets stands in for both; pinned by a test.
const SAFETY_NS: u64 = 2_000 * MS;
/// How often the safety looks (`safety_tick` in `network.rs`): every fifth
/// playout tick (round 246: the model looked every 20 ms).
const SAFETY_TICK_NS: u64 = 100 * MS;
/// What the server asks per answer (`AUDIO_STREAM_SAMPLES:960` in `tci.rs`).
const REQUEST_SAMPLES: usize = 960;
/// `TCI_TX_MAX_OUTSTANDING` in `cmaster.cs`, and `MAX_TX_AUDIO_QUEUE_BLOCKS` in
/// `TCIServer.cs` (answers that came but were not taken in yet).
const THETIS_MAX_OUTSTANDING: usize = 64;
const THETIS_MAX_DELIVERED: usize = 64;

/// The knobs of one replay. `Default` is the chain as it runs today.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Settings {
    /// `JitterBuffer::for_transmit(min_depth, max_depth)`: `TX_MIN_DEPTH` and `TX_MAX_DEPTH` in `network.rs`.
    pub min_depth: u32,
    pub max_depth: u32,
    /// Where the 20 ms tick falls against the arrivals' clock.
    pub tick_phase_ns: u64,
    /// TCI one way between the server and Thetis.
    pub link_ns: u64,
    /// From TRX on at Thetis to MOX with the DSP running and asking. Not in
    /// the code but in the log: from `PTT prefill started` to Thetis's report,
    /// minus the reported duration and the tail, came out at 88-103 ms on a
    /// drive, of which ~20 ms is our prefill and tick.
    pub mox_delay_ns: u64,
    /// Thetis: `tx_stream_audio_buffering` (stock Thetis keeps it at 50 or more;
    /// the replay does not, so a smaller one can be tried) and
    /// `TCI_TX_EXTRA_BUFFER_MS`.
    pub buffering_ms: u64,
    pub extra_ms: u64,
    /// Thetis's DSP block at 48 kHz (`GetBuffSize`; the report says 64).
    pub block: usize,
    /// The tick's second pull (build 193).
    pub second_pull: bool,
    /// The transmit buffer's numbers (`jitter::Tuning`), the ring's hold
    /// (`TX_HOLD_MAX`) and the low mark of the second pull (`TX_RING_LOW`, in
    /// samples) - the knobs of step C (build 255). The default is the server,
    /// with the slow trim standing in for the pauses (`transmit_stand_in`).
    pub tuning: Tuning,
    pub hold_ms: u64,
    pub ring_low: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            min_depth: TX_MIN_DEPTH,
            max_depth: TX_MAX_DEPTH,
            tick_phase_ns: 0,
            link_ns: MS,
            mox_delay_ns: 80 * MS,
            buffering_ms: 50,
            extra_ms: 50,
            block: 64,
            second_pull: true,
            tuning: transmit_stand_in(),
            hold_ms: 100,
            ring_low: TX_RING_LOW,
        }
    }
}

/// The transmit buffer's numbers (`Tuning::transmit`) with the slow trim back
/// in, standing in for catching up in the pauses (build 258; see the top).
pub(crate) fn transmit_stand_in() -> Tuning {
    Tuning { trim_every_pulls: sdr_remote_core::jitter::TRIM_EVERY_PULLS, ..Tuning::transmit() }
}

/// One TX packet as the server measurement has it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Arrival {
    pub mono_ms: u64,
    pub seq: u32,
    pub ts: u32,
    pub bytes: usize,
    pub ptt: bool,
}

/// What Thetis reports per MOX (`TakeTCITxOverReport`), in ms except the counts;
/// `lowest_ms` is -1 when no audio was heard.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct ThetisReport {
    pub duration_ms: i64,
    pub lead_zero_ms: i64,
    pub lowest_ms: i64,
    pub zero_ms: i64,
    pub underruns: i64,
    pub left_ms: i64,
    pub open_requests: i64,
    pub average_ms: i64,
    pub gaps: i64,
    pub longest_gap_ms: i64,
    pub tail_ms: i64,
}

/// Samples at 48 kHz to whole ms, as Thetis rounds them.
fn ms(samples: u64) -> i64 {
    (samples * 1000 / RATE) as i64
}

/// Thetis's TX queue, from `cmaster.cs`: the queue the DSP callback takes a
/// block from, the requests that keep it at its target, and the counters of
/// the report (TL2-5, TL2-10).
pub(crate) struct ThetisQueue {
    block: usize,
    target: usize,
    predicted: usize,
    stale_ns: u64,
    mox: bool,
    queue: VecDeque<f32>,
    /// Answers that came in and wait for the next service (`m_txAudioQueue`).
    delivered: VecDeque<Vec<f32>>,
    outstanding: usize,
    last_chrono: u64,
    blocks: u64,
    lead_zero: u64,
    heard: bool,
    lowest: usize,
    zeros: u64,
    underruns: u64,
    queued_sum: u64,
    heard_blocks: u64,
    gaps: u64,
    gap_samples: u64,
    longest_gap: u64,
    in_gap: bool,
}

impl ThetisQueue {
    pub(crate) fn new(s: &Settings) -> Self {
        let block = s.block.max(1);
        // `serviceTCITxProtocol`: the size it counts per open request, and the
        // level it keeps the queue at.
        let predicted = block.max(REQUEST_SAMPLES);
        let target = (block * 4).max((((s.buffering_ms + s.extra_ms) * RATE) as usize).div_ceil(1000));
        ThetisQueue {
            block,
            target,
            predicted,
            stale_ns: 250.max(s.buffering_ms * 4) * MS,
            mox: false,
            queue: VecDeque::new(),
            delivered: VecDeque::new(),
            outstanding: 0,
            last_chrono: 0,
            blocks: 0,
            lead_zero: 0,
            heard: false,
            lowest: usize::MAX,
            zeros: 0,
            underruns: 0,
            queued_sum: 0,
            heard_blocks: 0,
            gaps: 0,
            gap_samples: 0,
            longest_gap: 0,
            in_gap: false,
        }
    }

    pub(crate) fn block_ns(&self) -> f64 {
        self.block as f64 * 1e9 / RATE as f64
    }

    /// An answer from the server arrives (`handleBinaryFrame`): it waits for the
    /// next service, also outside MOX - Thetis does not look at MOX there.
    pub(crate) fn deliver(&mut self, audio: Vec<f32>) {
        if self.delivered.len() >= THETIS_MAX_DELIVERED {
            self.delivered.pop_front();
        }
        self.delivered.push_back(audio);
    }

    pub(crate) fn mox_on(&mut self) {
        self.mox = true;
    }

    /// The client's TRX off: the report first, then the queue emptied
    /// (`TakeTCITxOverReport("trx")`, `clearQueuedTxAudio`, `resetTCITxState`).
    /// Returns the report and what was still queued.
    pub(crate) fn mox_off(&mut self) -> (ThetisReport, Vec<f32>) {
        let r = ThetisReport {
            duration_ms: ms(self.blocks * self.block as u64),
            lead_zero_ms: ms(self.lead_zero),
            lowest_ms: if self.heard { ms(self.lowest as u64) } else { -1 },
            zero_ms: ms(self.zeros),
            underruns: self.underruns as i64,
            left_ms: ms(self.queue.len() as u64),
            open_requests: self.outstanding as i64,
            average_ms: if self.heard_blocks > 0 { ms(self.queued_sum / self.heard_blocks) } else { 0 },
            gaps: self.gaps as i64,
            longest_gap_ms: ms(self.longest_gap),
            tail_ms: if self.in_gap { ms(self.gap_samples) } else { 0 },
        };
        let left: Vec<f32> = self.queue.drain(..).collect();
        self.delivered.clear();
        self.outstanding = 0;
        self.last_chrono = 0;
        self.mox = false;
        self.blocks = 0;
        self.lead_zero = 0;
        self.heard = false;
        self.lowest = usize::MAX;
        self.zeros = 0;
        self.underruns = 0;
        self.queued_sum = 0;
        self.heard_blocks = 0;
        self.gaps = 0;
        self.gap_samples = 0;
        self.longest_gap = 0;
        self.in_gap = false;
        (r, left)
    }

    /// One DSP callback (`OnTCITxAudioInSamples`): a block out of the queue,
    /// the rest zeros; the samples it copied go to `out`. Then the service it
    /// wakes (`serviceTCITxProtocol`); returns how many requests that sent.
    pub(crate) fn callback(&mut self, now: u64, out: &mut Vec<f32>) -> usize {
        out.clear();
        let queued_at_entry = self.queue.len();
        while out.len() < self.block {
            match self.queue.pop_front() {
                Some(v) => out.push(v),
                None => break,
            }
        }
        let copied = out.len();
        let n = self.block;
        self.blocks += 1;
        if !self.heard {
            if copied == 0 {
                self.lead_zero += n as u64;
            } else {
                self.heard = true;
                self.lowest = queued_at_entry;
            }
        } else if queued_at_entry < self.lowest {
            self.lowest = queued_at_entry;
        }
        if self.heard && copied < n {
            self.zeros += (n - copied) as u64;
            self.underruns += 1;
        }
        if self.heard {
            self.queued_sum += queued_at_entry as u64;
            self.heard_blocks += 1;
            if copied < n {
                if !self.in_gap {
                    self.gaps += 1;
                    self.gap_samples = 0;
                    self.in_gap = true;
                }
                self.gap_samples += (n - copied) as u64;
                self.longest_gap = self.longest_gap.max(self.gap_samples);
            } else {
                self.in_gap = false;
            }
        }
        self.service(now)
    }

    /// `serviceTCITxProtocol` under MOX: take in what came, forget requests not
    /// answered for `max(250, 4 x buffering)` ms after the last one, and ask for
    /// as many 20 ms answers as bring the queue plus what is open to the target.
    fn service(&mut self, now: u64) -> usize {
        while let Some(a) = self.delivered.pop_front() {
            if self.outstanding > 0 {
                self.outstanding -= 1;
            }
            self.queue.extend(a);
        }
        if self.outstanding > 0 && now.saturating_sub(self.last_chrono) > self.stale_ns {
            self.outstanding = 0;
        }
        let future = self.queue.len() + self.outstanding * self.predicted;
        let mut needed = if future < self.target { (self.target - future).div_ceil(self.predicted) } else { 0 };
        let mut sent = 0;
        while needed > 0 && self.outstanding < THETIS_MAX_OUTSTANDING {
            self.outstanding += 1;
            self.last_chrono = now;
            sent += 1;
            needed -= 1;
        }
        sent
    }
}

/// One MOX of the replay: what Thetis would report, and what our side did.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MoxResult {
    /// TRX on and off at Thetis, on the arrivals' clock.
    pub on_ms: u64,
    pub off_ms: u64,
    pub thetis: ThetisReport,
    /// Requests the first service after MOX on sent (the salvo).
    pub salvo: usize,
    /// Frames that reached the transmitter, and the ms from the packet's
    /// arrival at the server to its first sample at the transmitter, sorted.
    pub frames_out: usize,
    pub latency_ms: Vec<u32>,
    /// Silence the ring padded that Thetis sent.
    pub silence_ms: i64,
    /// The part of it in the middle of the speech: padded silence with audio
    /// after it in the same MOX (build 255). With Thetis's zeros it is the dead
    /// air a listener hears; the zeros alone are only the part where Thetis had
    /// nothing - the "53 ms ceiling" of the drives.
    pub mid_silence_ms: i64,
    /// Frames given to the ring in this over that never went out.
    pub frames_lost: usize,
}

impl MoxResult {
    pub(crate) fn latency_at(&self, q: f64) -> Option<u32> {
        percentile(&self.latency_ms, q)
    }
}

fn percentile(sorted: &[u32], q: f64) -> Option<u32> {
    if sorted.is_empty() {
        return None;
    }
    let i = ((sorted.len() - 1) as f64 * q).round() as usize;
    Some(sorted[i.min(sorted.len() - 1)])
}

/// A whole replay.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Replay {
    pub moxes: Vec<MoxResult>,
    /// The transmit buffer's counters at the end.
    pub trimmed: u64,
    pub cleared: u64,
    pub late: u64,
    pub lost: u64,
    pub concealed: u64,
    /// Thrown away on arrival above the cap, and skipped (build 255).
    pub capped: u64,
    pub skipped: u64,
    /// The safety took the ownership (build 251), the next press found the
    /// holder outlived and the buffer started over (247), and a silence past
    /// the packet window emptied the buffer (249).
    pub safety_takes: u64,
    pub forgets: u64,
    pub silence_discards: u64,
    /// Per playout tick: time, the buffer's target and depth, the ring's
    /// length - to lay next to the server's 10 s lines.
    pub trace: Vec<TickTrace>,
}

/// One playout tick of the replay (build 255).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TickTrace {
    pub ms: u64,
    pub target: u32,
    pub depth: usize,
    pub ring: usize,
}

/// A frame given to the ring: the arrival of its packet (none when concealed),
/// when it reached the transmitter, and its over.
#[derive(Debug, Clone, Copy)]
struct Tag {
    arrival_ns: Option<u64>,
    out_ns: Option<u64>,
    over: usize,
}

/// The server's side around the tick, as `network.rs` and `ptt.rs` do it.
struct Ours {
    jitter: JitterBuffer,
    ring: TxRing,
    ring_level: Arc<AtomicUsize>,
    /// `tx_holder_addr` and `line_learned_from` of the receive loop, and the
    /// session's holder of Thetis. The two holders differ only after the safety
    /// took the ownership (build 251) until the next press forgets the loop's
    /// (build 247).
    holder: bool,
    learned: bool,
    session_holder: bool,
    /// The decoder: audio seen, and concealed frames in a row.
    has_history: bool,
    conceal_run: u32,
    concealed: u64,
    /// `PttState::Tx`, `pending_activate` and `pending_release` (start, tail).
    tx: bool,
    pending_activate: Option<u64>,
    pending_release: Option<(u64, u64)>,
    trx_off_count: u64,
    /// Overs started (a prefill), for `Tag::over`.
    overs: usize,
    /// The last PTT packet's arrival: the safety's packet clock
    /// (`record_ptt_packet`), and the silence of 249.
    last_ptt_arrival: Option<u64>,
    safety_takes: u64,
    forgets: u64,
    silence_discards: u64,
    trace: Vec<TickTrace>,
    /// When each packet arrived, by sequence number: the buffer keeps the
    /// phone's timestamp, not the arrival.
    arrival_of: HashMap<u32, u64>,
}

/// Messages in flight between the server and Thetis.
#[derive(Default)]
struct Wire {
    /// TRX on (true) or off, arriving at Thetis.
    trx: VecDeque<(u64, bool, usize)>,
    /// TX_CHRONO requests arriving at the server.
    requests: VecDeque<u64>,
    /// Answers arriving at Thetis.
    answers: VecDeque<(u64, Vec<f32>)>,
}

impl Wire {
    /// Thetis takes TRX on and off in the order they were sent: an off that
    /// comes while MOX is still being switched on waits for it.
    fn send_trx(&mut self, at: u64, on: bool, over: usize) {
        let at = self.trx.back().map_or(at, |b| at.max(b.0));
        self.trx.push_back((at, on, over));
    }
}

/// Replay `arrivals` (sorted by arrival) with `s`.
pub(crate) fn replay(arrivals: &[Arrival], s: &Settings) -> Replay {
    let base = Instant::now();
    let at = |t: u64| base + Duration::from_nanos(t);
    let level = Arc::new(AtomicUsize::new(0));
    let mut ours = Ours {
        jitter: JitterBuffer::for_transmit(s.min_depth, s.max_depth).with_tuning(s.tuning),
        ring: TxRing::with_level(level.clone()).with_hold_max(Duration::from_millis(s.hold_ms)),
        ring_level: level,
        holder: false,
        learned: false,
        session_holder: false,
        has_history: false,
        conceal_run: 0,
        concealed: 0,
        tx: false,
        pending_activate: None,
        pending_release: None,
        trx_off_count: 0,
        overs: 0,
        arrival_of: HashMap::new(),
        last_ptt_arrival: None,
        safety_takes: 0,
        forgets: 0,
        silence_discards: 0,
        trace: Vec::new(),
    };
    let mut thetis = ThetisQueue::new(s);
    let mut wire = Wire::default();
    let mut tags: Vec<Tag> = Vec::new();
    let mut moxes: Vec<MoxResult> = Vec::new();
    // The MOX at Thetis now: its start, its over, the salvo, frames out,
    // latencies and silence so far.
    let mut current: Option<(u64, usize, Option<usize>, usize, Vec<u32>, u64)> = None;
    // Padded silence since the last audio of this MOX, and the part of it that
    // had audio after it (build 255).
    let (mut pending_silence, mut mid_silence) = (0u64, 0u64);
    let mut next_arrival = 0usize;
    let mut next_tick = s.tick_phase_ns % TICK_NS;
    let mut next_cb: Option<u64> = None;
    let mut cb_index: u64 = 0;
    let block_ns = thetis.block_ns();
    let end = arrivals.last().map_or(0, |a| a.mono_ms * MS) + 3_000 * MS;
    let mut out = Vec::with_capacity(s.block);

    loop {
        let deadline = ours.ring.next_deadline().map(|d| d.saturating_duration_since(base).as_nanos() as u64);
        let candidates = [
            arrivals.get(next_arrival).map(|a| a.mono_ms * MS),
            wire.answers.front().map(|a| a.0),
            wire.requests.front().copied(),
            deadline,
            ours.pending_release.map(|(start, tail)| start + tail),
            wire.trx.front().map(|m| m.0),
            if next_tick <= end || ours.tx || ours.pending_activate.is_some() { Some(next_tick) } else { None },
            next_cb,
        ];
        let Some(t) = candidates.iter().flatten().copied().min() else { break };

        while next_arrival < arrivals.len() && arrivals[next_arrival].mono_ms * MS == t {
            arrive(&mut ours, &arrivals[next_arrival]);
            next_arrival += 1;
        }
        while wire.answers.front().is_some_and(|a| a.0 == t) {
            let (_, audio) = wire.answers.pop_front().unwrap();
            thetis.deliver(audio);
        }
        while wire.requests.front() == Some(&t) {
            wire.requests.pop_front();
            ours.ring.note_trx_off(ours.trx_off_count);
            let w = Waiting { n: REQUEST_SAMPLES, at: at(t), sample_rate: RATE as u32, format: 3, channels: 1 };
            for (_, audio) in ours.ring.request(w) {
                wire.answers.push_back((t + s.link_ns, audio));
            }
        }
        if deadline == Some(t) {
            for (_, audio) in ours.ring.answer_ready(at(t)) {
                wire.answers.push_back((t + s.link_ns, audio));
            }
        }
        if let Some((start, tail)) = ours.pending_release {
            if start + tail == t {
                // `finish_tail_if_due`: TRX off; the reader drops what waits.
                ours.pending_release = None;
                ours.tx = false;
                ours.trx_off_count += 1;
                ours.ring.note_trx_off(ours.trx_off_count);
                wire.send_trx(t + s.link_ns, false, ours.overs);
            }
        }
        while wire.trx.front().is_some_and(|m| m.0 == t) {
            let (_, on, over) = wire.trx.pop_front().unwrap();
            if on && !thetis.mox {
                thetis.mox_on();
                cb_index = (t as f64 / block_ns).ceil() as u64;
                next_cb = Some((cb_index as f64 * block_ns) as u64);
                current = Some((t, over, None, 0, Vec::new(), 0));
            } else if !on && thetis.mox {
                let (report, _left) = thetis.mox_off();
                next_cb = None;
                let (on_ns, over, salvo, frames_out, mut latency, silence) = current.take().unwrap();
                let mid = std::mem::take(&mut mid_silence);
                pending_silence = 0;
                latency.sort_unstable();
                let frames_lost = tags.iter().filter(|g| g.over == over && g.out_ns.is_none()).count();
                moxes.push(MoxResult {
                    on_ms: on_ns / MS,
                    off_ms: t / MS,
                    thetis: report,
                    salvo: salvo.unwrap_or(0),
                    frames_out,
                    latency_ms: latency,
                    silence_ms: ms(silence),
                    mid_silence_ms: ms(mid),
                    frames_lost,
                });
            }
        }
        if next_tick == t {
            tick(&mut ours, &mut wire, &mut tags, s, t, at(t));
            next_tick += TICK_NS;
        }
        if next_cb == Some(t) {
            let sent = thetis.callback(t, &mut out);
            if let Some(cur) = current.as_mut() {
                for &v in &out {
                    if v > 0.0 {
                        let g = &mut tags[v as usize - 1];
                        if g.out_ns.is_none() {
                            g.out_ns = Some(t);
                            if cur.3 > 0 {
                                mid_silence += pending_silence;
                            }
                            pending_silence = 0;
                            cur.3 += 1;
                            if let Some(a) = g.arrival_ns {
                                cur.4.push(((t - a) / MS) as u32);
                            }
                        }
                    } else {
                        cur.5 += 1;
                        pending_silence += 1;
                    }
                }
                if cur.2.is_none() {
                    cur.2 = Some(sent);
                }
            }
            for _ in 0..sent {
                wire.requests.push_back(t + s.link_ns);
            }
            cb_index += 1;
            next_cb = Some((cb_index as f64 * block_ns) as u64);
        }
        if t > end && !ours.tx && ours.pending_activate.is_none() && !thetis.mox && wire.trx.is_empty() {
            break;
        }
    }
    Replay {
        moxes,
        trimmed: ours.jitter.stats_trimmed,
        cleared: ours.jitter.stats_cleared,
        late: ours.jitter.stats_late,
        lost: ours.jitter.stats_lost,
        concealed: ours.concealed,
        capped: ours.jitter.stats_capped,
        skipped: ours.jitter.stats_skipped,
        safety_takes: ours.safety_takes,
        forgets: ours.forgets,
        silence_discards: ours.silence_discards,
        trace: ours.trace,
    }
}

/// A packet arrives (the audio arm of `network.rs`, in its order):
/// - a PTT packet first asks whether the session still has the loop's holder;
///   if the safety took it (build 251), the loop forgets it and the buffer
///   starts over (`forget_thetis_holder_if_outlived!`, build 247);
/// - a silence past the packet window empties what the buffer holds, keeping
///   what it learned (build 249) - for any packet of the loop's holder;
/// - a PTT packet from a client that does not hold the transmitter starts a
///   new holder: the first ever resets the buffer, a later one keeps what it
///   learned (build 175), and a new decoder;
/// - a packet without PTT goes in only while the session has the sender.
fn arrive(o: &mut Ours, a: &Arrival) {
    let t = a.mono_ms * MS;
    if a.ptt && o.holder && !o.session_holder {
        o.jitter.reset();
        o.holder = false;
        o.learned = false;
        o.forgets += 1;
    }
    if o.holder && o.last_ptt_arrival.is_some_and(|last| t - last > SAFETY_NS) {
        o.jitter.discard();
        o.silence_discards += 1;
    }
    if a.ptt {
        o.session_holder = true;
        if !o.holder {
            if o.learned {
                o.jitter.discard();
            } else {
                o.jitter.reset();
                o.learned = true;
            }
            o.has_history = false;
            o.conceal_run = 0;
            o.holder = true;
        }
        o.last_ptt_arrival = Some(t);
    } else if !o.session_holder {
        return;
    }
    o.arrival_of.insert(a.seq, t);
    // Every row of the measurement went into the buffer: it is written right
    // before the push.
    o.jitter.push(
        BufferedFrame {
            sequence: a.seq,
            timestamp: a.ts,
            opus_data: vec![1u8; a.bytes],
            ptt: a.ptt,
            wideband: true,
        },
        a.mono_ms,
    );
}

/// One playout tick (`network.rs`): the prefill first, then one pull, a second
/// when `pull_extra_for_ring_below` says so.
fn tick(o: &mut Ours, wire: &mut Wire, tags: &mut Vec<Tag>, s: &Settings, t: u64, now: Instant) {
    // `check_safety`, every 100 ms: no PTT packet for more than 2 s while
    // transmitting - TRX off at once, and the ownership goes with it (build
    // 251); the loop forgets its holder at the next press.
    let safety_looks = (t.saturating_sub(s.tick_phase_ns % TICK_NS) / TICK_NS) % (SAFETY_TICK_NS / TICK_NS) == 0;
    if safety_looks && o.tx && o.last_ptt_arrival.map_or(true, |last| t - last > SAFETY_NS) {
        o.pending_release = None;
        o.tx = false;
        o.trx_off_count += 1;
        o.ring.note_trx_off(o.trx_off_count);
        wire.send_trx(t + s.link_ns, false, o.overs);
        if o.session_holder {
            o.session_holder = false;
            o.safety_takes += 1;
        }
    }
    if let Some(p) = o.pending_activate {
        if t - p >= PREFILL_NS {
            o.pending_activate = None;
            o.tx = true;
            wire.send_trx(t + s.link_ns + s.mox_delay_ns, true, o.overs);
        }
    }
    let ring_at_tick = Some(o.ring_level.load(Ordering::Relaxed));
    o.trace.push(TickTrace { ms: t / MS, target: o.jitter.target_depth(), depth: o.jitter.depth(), ring: o.ring_level.load(Ordering::Relaxed) });
    let mut pulls = 0;
    loop {
        pulls += 1;
        match o.jitter.pull() {
            JitterResult::Frame(f) => {
                if f.ptt && o.session_holder {
                    // `activate_from_playout`
                    o.pending_release = None;
                    if !o.tx && o.pending_activate.is_none() {
                        o.pending_activate = Some(t);
                        o.overs += 1;
                    }
                } else if o.tx || o.pending_activate.is_some() {
                    // `cancel_prefill`, `release_from_playout`, the holder let go.
                    o.pending_activate = None;
                    if o.tx && o.pending_release.is_none() {
                        let tail_ms = (o.jitter.depth() as u64 * 20 + TAIL_MARGIN_MS).max(TAIL_MIN_MS);
                        o.pending_release = Some((t, tail_ms * MS));
                    }
                    o.holder = false;
                    o.session_holder = false;
                }
                if !f.opus_data.is_empty() {
                    o.has_history = true;
                    o.conceal_run = 0;
                    let arrival = o.arrival_of.get(&f.sequence).copied();
                    feed(o, wire, tags, s, t, now, arrival);
                }
            }
            JitterResult::Missing => {
                // `fill_gap`: rebuilt from the next packet, or concealed.
                let next = o.jitter.next_seq_peek().and_then(|q| o.jitter.peek_frame(q)).is_some_and(|(d, _)| !d.is_empty());
                let filled = if next {
                    o.has_history = true;
                    o.conceal_run = 0;
                    true
                } else if o.has_history && o.conceal_run < CONCEAL_MAX_FRAMES {
                    o.conceal_run += 1;
                    true
                } else {
                    false
                };
                if filled {
                    o.concealed += 1;
                    feed(o, wire, tags, s, t, now, None);
                }
            }
            JitterResult::NotReady => {}
        }
        if pulls >= 2 || !(s.second_pull && pull_extra_for_ring_below(s.ring_low, ring_at_tick, o.jitter.depth(), o.jitter.target_depth())) {
            break;
        }
    }
}

/// A frame to the ring, numbered; and the answers that makes ready.
fn feed(o: &mut Ours, wire: &mut Wire, tags: &mut Vec<Tag>, s: &Settings, t: u64, now: Instant, arrival: Option<u64>) {
    tags.push(Tag { arrival_ns: arrival, out_ns: None, over: o.overs });
    let id = tags.len() as f32;
    o.ring.feed(&[id; FRAME], now);
    for (_, audio) in o.ring.answer_ready(now) {
        wire.answers.push_back((t + s.link_ns, audio));
    }
}

/// The rows of a server measurement (`tl-servermeting-*.csv`): `tx_arrival`
/// rows for Thetis, header and comments skipped, sorted by arrival. Since build
/// 263 a row says its transmitter; a file from before has no such column and is
/// all Thetis.
pub(crate) fn parse_measurement(text: &str) -> Vec<Arrival> {
    let mut rows: Vec<Arrival> = text
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let f: Vec<&str> = l.trim().split(',').collect();
            if f.len() < 7 || f[2] != "tx_arrival" || f.get(7).is_some_and(|s| *s != "thetis") {
                return None;
            }
            Some(Arrival {
                mono_ms: f[1].parse().ok()?,
                seq: f[3].parse().ok()?,
                ts: f[4].parse().ok()?,
                bytes: f[5].parse().ok()?,
                ptt: f[6] == "1",
            })
        })
        .collect();
    rows.sort_by_key(|a| a.mono_ms);
    rows
}

/// Thetis's reports in a server log (version 2 of `tx_queue_report_ex`, as
/// the server writes them), with the log's time of day.
pub(crate) fn parse_reports(log: &str) -> Vec<(String, ThetisReport)> {
    const HEAD: &str = "Thetis TX queue, previous over (";
    log.lines()
        .filter_map(|l| {
            let i = l.find(HEAD)?;
            let time = l.find('[').and_then(|b| l.get(b + 1..b + 13)).unwrap_or("").to_string();
            let rest = &l[i + HEAD.len()..];
            let rest = &rest[rest.find(')')? + 1..];
            let n: Vec<i64> = rest
                .split(|c: char| !(c.is_ascii_digit() || c == '-'))
                .filter_map(|w| w.parse().ok())
                .collect();
            if n.len() < 13 {
                return None;
            }
            Some((
                time,
                ThetisReport {
                    duration_ms: n[0],
                    lead_zero_ms: n[1],
                    lowest_ms: n[2],
                    zero_ms: n[3],
                    underruns: n[4],
                    left_ms: n[5],
                    open_requests: n[6],
                    average_ms: n[7],
                    gaps: n[8],
                    longest_gap_ms: n[9],
                    tail_ms: n[10],
                },
            ))
        })
        .collect()
}

/// Pairs each replayed MOX with a report of Thetis, keeping the order: the most
/// pairs whose durations are within 10% (at least 100 ms) of each other. The
/// log's clock and the measurement's are not the same, so order and duration
/// are what there is; a log may reach back before the measurement, and a MOX
/// can be in one and not the other (a blip of TX the replay merges).
pub(crate) fn pair_reports(moxes: &[MoxResult], reports: &[(String, ThetisReport)]) -> Vec<Option<usize>> {
    let close = |a: i64, b: i64| (a - b).abs() <= 100.max(b / 10);
    let (n, m) = (moxes.len(), reports.len());
    // best[i][j]: the most pairs among moxes[i..] and reports[j..].
    let mut best = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            let skip = best[i + 1][j].max(best[i][j + 1]);
            let take = if close(moxes[i].thetis.duration_ms, reports[j].1.duration_ms) { best[i + 1][j + 1] + 1 } else { 0 };
            best[i][j] = skip.max(take);
        }
    }
    let mut out = vec![None; n];
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if close(moxes[i].thetis.duration_ms, reports[j].1.duration_ms) && best[i][j] == best[i + 1][j + 1] + 1 {
            out[i] = Some(j);
            i += 1;
            j += 1;
        } else if best[i + 1][j] >= best[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// Where the per-over and per-tick files go (`--csv`, `--trace`; build 255).
#[derive(Debug, Default, PartialEq)]
struct Outputs {
    csv: Option<String>,
    trace: Option<String>,
}

fn parse_settings(args: &[String]) -> Result<(Settings, Vec<String>, Outputs), String> {
    let mut s = Settings::default();
    let mut files = Vec::new();
    let mut out = Outputs::default();
    let mut i = 0;
    while i < args.len() {
        let val = |i: &mut usize| -> Result<u64, String> {
            *i += 1;
            args.get(*i).and_then(|v| v.parse().ok()).ok_or_else(|| format!("{} needs a number", args[*i - 1]))
        };
        let real = |i: &mut usize| -> Result<f32, String> {
            *i += 1;
            args.get(*i)
                .and_then(|v| v.parse::<f32>().ok())
                .filter(|v| v.is_finite() && *v > 0.0)
                .ok_or_else(|| format!("{} needs a number above 0", args[*i - 1]))
        };
        let path = |i: &mut usize| -> Result<String, String> {
            *i += 1;
            args.get(*i).cloned().ok_or_else(|| format!("{} needs a file", args[*i - 1]))
        };
        match args[i].as_str() {
            "--min-depth" => s.min_depth = val(&mut i)? as u32,
            "--max-depth" => s.max_depth = val(&mut i)? as u32,
            "--phase-ms" => s.tick_phase_ns = val(&mut i)? * MS,
            "--link-ms" => s.link_ns = val(&mut i)? * MS,
            "--mox-delay-ms" => s.mox_delay_ns = val(&mut i)? * MS,
            "--buffering-ms" => s.buffering_ms = val(&mut i)?,
            "--extra-ms" => s.extra_ms = val(&mut i)?,
            "--block" => s.block = val(&mut i)? as usize,
            "--no-second-pull" => s.second_pull = false,
            "--ms-per-frame" => s.tuning.ms_per_frame = real(&mut i)?,
            "--target-add" => s.tuning.target_add = val(&mut i)? as u32,
            "--spike-decay" => s.tuning.spike_decay_packets = real(&mut i)?,
            "--cap-above-max" => s.tuning.cap_above_max = val(&mut i)? as u32,
            "--trim-above" => s.tuning.trim_above_target = val(&mut i)? as i32,
            "--trim-every" => s.tuning.trim_every_pulls = (val(&mut i)? as u32).max(1),
            "--fast-trim-above" => s.tuning.fast_trim_above = val(&mut i)? as i32,
            "--hold-ms" => s.hold_ms = val(&mut i)?,
            "--ring-low-ms" => s.ring_low = val(&mut i)? as usize * (RATE / 1000) as usize,
            "--csv" => out.csv = Some(path(&mut i)?),
            "--trace" => out.trace = Some(path(&mut i)?),
            other if other.starts_with("--") => return Err(format!("unknown option {}", other)),
            file => files.push(file.to_string()),
        }
        i += 1;
    }
    Ok((s, files, out))
}

/// `ThetisLink-Server --naspelen <measurement.csv> <server.log> [options]`.
/// Compiled for the tests too, so it cannot rot; called only with the feature.
#[cfg_attr(not(feature = "naspelen"), allow(dead_code))]
pub(crate) fn run_cli(args: &[String]) -> i32 {
    let (s, files, outputs) = match parse_settings(args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}", e);
            return 2;
        }
    };
    if files.len() != 2 {
        eprintln!(
            "usage: --naspelen <tl-servermeting.csv> <server.log> [--min-depth N] [--max-depth N] [--phase-ms N] \
             [--link-ms N] [--mox-delay-ms N] [--buffering-ms N] [--extra-ms N] [--block N] [--no-second-pull] \
             [--ms-per-frame X] [--target-add N] [--spike-decay X] [--cap-above-max N] [--trim-above N] \
             [--trim-every N] [--fast-trim-above N] [--hold-ms N] [--ring-low-ms N] [--csv <file>] [--trace <file>]"
        );
        return 2;
    }
    let read = |p: &str| std::fs::read_to_string(p).map_err(|e| eprintln!("{}: {}", p, e)).ok();
    let (Some(csv), Some(log)) = (read(&files[0]), read(&files[1])) else { return 1 };
    let arrivals = parse_measurement(&csv);
    let reports = parse_reports(&log);
    let r = replay(&arrivals, &s);
    let pairs = pair_reports(&r.moxes, &reports);
    println!("{:?}", s);
    println!("{} packets, {} MOX replayed, {} reports in the log", arrivals.len(), r.moxes.len(), reports.len());
    println!(
        "MOX   on_ms   dur | model: avg low gaps longest zeros salvo silence mid lost lat50 lat95 | thetis: time avg low gaps longest zeros"
    );
    let mut sum = [0i64; 4];
    let (mut tx_ms, mut mid_ms, mut inner_zero_ms) = (0i64, 0i64, 0i64);
    let mut csv = String::from(
        "mox,on_ms,off_ms,duration_ms,zero_ms,gaps,longest_gap_ms,average_ms,lowest_ms,silence_ms,mid_silence_ms,frames_out,frames_lost,lat50_ms,lat95_ms,thetis_time,thetis_duration_ms,thetis_zero_ms,thetis_gaps,thetis_longest_gap_ms,thetis_average_ms\n",
    );
    let mut lat: Vec<u32> = Vec::new();
    for (i, (m, p)) in r.moxes.iter().zip(&pairs).enumerate() {
        let t = &m.thetis;
        lat.extend(&m.latency_ms);
        sum[0] += t.zero_ms;
        sum[1] += t.gaps;
        tx_ms += t.duration_ms;
        mid_ms += m.mid_silence_ms;
        // Zeros after the last audio of the over are no speech missed.
        inner_zero_ms += t.zero_ms - t.tail_ms;
        let real = p.map(|j| &reports[j]);
        let pick = |v: Option<u32>| v.map_or(String::new(), |v| v.to_string());
        csv += &format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},",
            i + 1, m.on_ms, m.off_ms, t.duration_ms, t.zero_ms, t.gaps, t.longest_gap_ms, t.average_ms, t.lowest_ms,
            m.silence_ms, m.mid_silence_ms, m.frames_out, m.frames_lost, pick(m.latency_at(0.5)), pick(m.latency_at(0.95))
        );
        csv += &match real {
            Some((time, rr)) => format!("{},{},{},{},{},{}\n", time, rr.duration_ms, rr.zero_ms, rr.gaps, rr.longest_gap_ms, rr.average_ms),
            None => ",,,,,\n".to_string(),
        };
        if let Some((_, rr)) = real {
            sum[2] += rr.zero_ms;
            sum[3] += rr.gaps;
        }
        println!(
            "{:3} {:8} {:5} | {:4} {:4} {:4} {:4} {:5} {:3} {:5} {:5} {:4} {:5} {:5} | {}",
            i + 1,
            m.on_ms,
            t.duration_ms,
            t.average_ms,
            t.lowest_ms,
            t.gaps,
            t.longest_gap_ms,
            t.zero_ms,
            m.salvo,
            m.silence_ms,
            m.mid_silence_ms,
            m.frames_lost,
            m.latency_at(0.5).map_or("-".into(), |v| v.to_string()),
            m.latency_at(0.95).map_or("-".into(), |v| v.to_string()),
            real.map_or("-".to_string(), |(time, rr)| format!(
                "{} {:4} {:4} {:4} {:4} {:5}",
                time, rr.average_ms, rr.lowest_ms, rr.gaps, rr.longest_gap_ms, rr.zero_ms
            )),
        );
    }
    lat.sort_unstable();
    println!(
        "total: model {} ms zeros in {} gaps; thetis (paired) {} ms in {} gaps; {} of {} paired",
        sum[0], sum[1], sum[2], sum[3], pairs.iter().flatten().count(), r.moxes.len()
    );
    println!(
        "dead air (model): {} ms zeros + {} ms silence in the middle of the speech = {} ms of {} ms on the air ({:.2}%)",
        inner_zero_ms, mid_ms, inner_zero_ms + mid_ms, tx_ms,
        if tx_ms > 0 { (inner_zero_ms + mid_ms) as f64 * 100.0 / tx_ms as f64 } else { 0.0 }
    );
    println!(
        "arrival to transmitter: median {} ms, 95% {} ms, max {} ms; buffer trimmed {}, cleared {}, late {}, lost {}, concealed {}, capped {}, skipped {}",
        percentile(&lat, 0.5).unwrap_or(0),
        percentile(&lat, 0.95).unwrap_or(0),
        lat.last().copied().unwrap_or(0),
        r.trimmed, r.cleared, r.late, r.lost, r.concealed, r.capped, r.skipped
    );
    println!(
        "the safety took the ownership {} time(s), the next press started over {} time(s), a silence emptied the buffer {} time(s)",
        r.safety_takes, r.forgets, r.silence_discards
    );
    let write = |p: &Option<String>, text: String| match p {
        Some(p) => std::fs::write(p, text).map_err(|e| eprintln!("{}: {}", p, e)).is_ok(),
        None => true,
    };
    let mut trace = String::from("ms,target,depth,ring_samples\n");
    if outputs.trace.is_some() {
        for t in &r.trace {
            trace += &format!("{},{},{},{}\n", t.ms, t.target, t.depth, t.ring);
        }
    }
    if !(write(&outputs.csv, csv) && write(&outputs.trace, trace)) {
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The chain as it ran until build 257: depth 3-20, the receive numbers and
    /// a hold of 150 ms. The real over below was measured with it, and the
    /// mechanics pinned here were worked out on it.
    fn until_257() -> Settings {
        Settings { min_depth: 3, max_depth: 20, tuning: Tuning::default(), hold_ms: 150, ..Settings::default() }
    }

    /// Packets every 20 ms from `start_ms`: `n` with PTT, then three without.
    fn steady_over(start_ms: u64, first_seq: u32, n: u32) -> Vec<Arrival> {
        (0..n + 3)
            .map(|i| Arrival {
                mono_ms: start_ms + i as u64 * 20,
                seq: first_seq + i,
                ts: (start_ms + i as u64 * 20) as u32,
                bytes: if i == 0 || i >= n { 0 } else { 60 },
                ptt: i < n,
            })
            .collect()
    }

    /// Thetis's queue on its own (`cmaster.cs`): at MOX it asks its target of
    /// 100 ms in one salvo of five 20 ms answers; a queue that runs dry gives
    /// exactly the zeros it lacks, as one gap; requests nobody answers are
    /// forgotten after 250 ms and asked again (build 246).
    #[test]
    fn thetis_asks_its_queue_full_and_counts_a_gap_of_known_length() {
        let s = Settings::default();
        let mut q = ThetisQueue::new(&s);
        let mut out = Vec::new();
        q.mox_on();
        let blk = q.block_ns() as u64;
        let mut t = 0;
        assert_eq!(q.callback(t, &mut out), 5, "the salvo: (50 + 50) ms in 20 ms answers");
        for _ in 0..5 {
            q.deliver(vec![1.0; REQUEST_SAMPLES]);
        }
        t += blk;
        assert_eq!(q.callback(t, &mut out), 0, "the answers are taken in after the block: the queue is at its target");
        assert!(out.is_empty(), "that block was still a zero before the first audio");
        // 4800 samples last 75 blocks; then 30 blocks with nothing.
        let mut asked = 0;
        for _ in 0..75 + 30 {
            t += blk;
            asked += q.callback(t, &mut out);
        }
        assert_eq!(asked, 5, "a 20 ms answer per 20 ms taken, none answered");
        q.deliver(vec![1.0; REQUEST_SAMPLES]);
        t += blk;
        q.callback(t, &mut out);
        t += blk;
        q.callback(t, &mut out);
        assert_eq!(out.len(), 64, "audio again");
        let (r, left) = q.mox_off();
        // The answer that came is taken in after the block it came in: 31
        // blocks without audio.
        assert_eq!(r.zero_ms, ms(31 * 64), "31 blocks of 64 samples");
        assert_eq!((r.gaps, r.longest_gap_ms, r.tail_ms, r.underruns), (1, 41, 0, 31));
        assert_eq!(r.lead_zero_ms, 2, "two blocks before the first audio");
        assert_eq!(r.lowest_ms, 0);
        assert_eq!(left.len(), REQUEST_SAMPLES - 64);
        assert_eq!(r.duration_ms, ms(109 * 64));
        // MOX off empties it all.
        assert_eq!((q.queue.len(), q.outstanding, q.delivered.len()), (0, 0, 0));
        // Requests nobody answers count as audio to come for 250 ms after the
        // last one, then they are asked again.
        q.mox_on();
        let mut asked = Vec::new();
        for i in 0..200u64 {
            let n = q.callback(i * blk, &mut out);
            if n > 0 {
                asked.push((i * blk / MS, n));
            }
        }
        assert_eq!(asked, vec![(0, 5), (250, 5)], "asked again just after 250 ms");
    }

    /// An answer still waiting at Thetis when MOX ends is emptied with the queue
    /// (`clearQueuedTxAudio`); one that comes after MOX off waits, and is the
    /// first audio of the next MOX (`handleBinaryFrame` does not look at MOX).
    /// Round 246, paths A and E.
    #[test]
    fn a_late_answer_after_trx_off_stands_first_at_the_next_mox() {
        let s = Settings::default();
        let mut q = ThetisQueue::new(&s);
        let mut out = Vec::new();
        q.mox_on();
        q.callback(0, &mut out);
        q.deliver(vec![1.0; REQUEST_SAMPLES]);
        q.mox_off();
        q.deliver(vec![2.0; REQUEST_SAMPLES]);
        q.mox_on();
        let blk = q.block_ns() as u64;
        q.callback(0, &mut out);
        assert!(out.is_empty(), "taken in after the first block");
        q.callback(blk, &mut out);
        assert!(!out.is_empty() && out.iter().all(|&v| v == 2.0), "the late answer first, the cleared one never: {:?}", &out[..4]);
    }

    /// A steady stream reaches the transmitter without a gap inside the over,
    /// and the second over - without a restart in between - starts as the
    /// first: the same salvo, the same report, nothing of the first left in
    /// the ring, at Thetis or in the requests (build 246).
    #[test]
    fn a_steady_stream_has_no_gap_and_a_second_over_starts_like_the_first() {
        let mut arrivals = steady_over(1_000, 0, 100);
        arrivals.extend(steady_over(6_000, 103, 100));
        // Tuning B (build 258) waits for 2 frames, not 3: 20 ms sooner.
        let b = replay(&arrivals, &Settings::default());
        assert!(b.moxes.iter().all(|m| m.latency_ms.iter().all(|&l| l == 105)), "{:?}", b.moxes[0].latency_ms);
        let r = replay(&arrivals, &until_257());
        assert_eq!(r.moxes.len(), 2);
        for m in &r.moxes {
            assert_eq!(m.salvo, 5);
            assert_eq!(m.thetis.zero_ms, m.thetis.tail_ms, "zeros only at the very end: {:?}", m.thetis);
            assert_eq!(m.silence_ms, 0, "the ring never ran dry");
            // The tail (`release_from_playout`) is shorter than what stands at
            // Thetis: the end of an over is cut ("left at the end").
            assert_eq!(m.frames_out + m.frames_lost, 99, "every frame with audio went out or was cut at the end");
            assert!(m.thetis.left_ms > 0);
            let spread = m.latency_at(1.0).unwrap() - m.latency_at(0.0).unwrap();
            assert!(spread <= 2, "a steady delay: {:?}", m.latency_ms);
        }
        assert_eq!(r.moxes[0].thetis, r.moxes[1].thetis, "the second over is not coloured by the first");
        assert_eq!(r.moxes[0].latency_ms, r.moxes[1].latency_ms);
        // The delay itself, pinned: from the arrival at the server to the
        // transmitter, 125 ms for every frame of a steady stream - our buffer
        // waits for its 3 frames, then Thetis switches to TX (80 ms) and keeps
        // its queue at ~92 ms. A step of the chain that comes a tick later
        // shows here, and nowhere else.
        assert!(r.moxes[0].latency_ms.iter().all(|&l| l == 125), "{:?}", r.moxes[0].latency_ms);
        assert_eq!((r.moxes[0].thetis.average_ms, r.moxes[0].thetis.lowest_ms), (92, 34));
        // And its end: the tail after the release (`depth x 20 + 10` ms) ends
        // MOX with 33 ms still at Thetis.
        assert_eq!((r.moxes[0].thetis.duration_ms, r.moxes[0].thetis.left_ms), (1950, 33));
    }

    /// A pause in the packets: one shorter than the cushion of the chain (our
    /// buffer, the ring, Thetis's queue) gives no zeros inside the over, a
    /// longer one does. And no gap at Thetis is longer than the ring's hold
    /// (150 ms until build 257) minus Thetis's target (100 ms) and a block: then
    /// the ring answers with silence - the ~53 ms ceiling of four drives. With
    /// a target of 50 ms the ceiling rises with it (build 246); with the hold of
    /// 100 ms of build 258 it is a block.
    #[test]
    fn a_pause_gives_zeros_only_beyond_the_cushion_and_gaps_up_to_the_hold_minus_the_target() {
        let late = |delay: u64, s: Settings| {
            let mut a = steady_over(1_000, 0, 150);
            // Nothing comes for `delay` ms from packet 60 on; then what was
            // held back comes at once.
            for p in a.iter_mut().filter(|p| p.seq >= 60) {
                p.mono_ms = p.mono_ms.max(1_000 + 60 * 20 + delay);
            }
            a.sort_by_key(|p| p.mono_ms);
            let m = &replay(&a, &s).moxes[0];
            (m.thetis, m.mid_silence_ms)
        };
        let short = late(40, until_257());
        assert_eq!(short.0.zero_ms, short.0.tail_ms, "{short:?}");
        assert_eq!(short.1, 0, "no silence in the middle: {short:?}");
        let long = late(400, until_257());
        assert!(long.0.zero_ms > long.0.tail_ms, "{long:?}");
        // Past the hold the ring answers with silence, and that silence is
        // dead air too - in the middle of the speech, not counted by Thetis
        // (build 255).
        assert!(long.1 >= 100, "the ring's silence in the middle: {long:?}");
        let long = long.0;
        assert!((50..=53).contains(&long.longest_gap_ms), "{long:?}");
        let small_target = late(400, Settings { extra_ms: 0, ..until_257() }).0;
        assert!((100..=103).contains(&small_target.longest_gap_ms), "{small_target:?}");
        // A shorter hold lowers the ceiling with it: 100 - 100 + a block, as the
        // server runs since build 258.
        let short_hold = late(400, Settings::default()).0;
        assert!(short_hold.longest_gap_ms <= 4, "{short_hold:?}");
    }

    /// A client that goes silent in the middle of an over (no release packet)
    /// is let go by the safety more than 2 s after its last packet, at its
    /// 100 ms tick, as `check_safety` does (build 246; round 246). The safety
    /// takes the ownership (251); the next press finds the loop's holder
    /// outlived and starts the buffer over (247) - and the next over starts as
    /// usual.
    #[test]
    fn a_client_that_goes_silent_is_released_after_two_seconds_and_starts_over() {
        let mut arrivals: Vec<Arrival> = steady_over(1_040, 0, 100).into_iter().filter(|a| a.ptt).collect();
        let last = arrivals.last().unwrap().mono_ms;
        assert_eq!(last, 3_020);
        arrivals.extend(steady_over(8_000, 103, 50));
        let r = replay(&arrivals, &Settings::default());
        assert_eq!(r.moxes.len(), 2);
        // More than 2 s after 3020 is 5040 on the 20 ms tick, 5100 on the
        // safety's 100 ms tick; TRX off reaches Thetis 1 ms later.
        assert_eq!(r.moxes[0].off_ms, 5_101, "last packet at {last}");
        assert_eq!((r.safety_takes, r.forgets, r.silence_discards), (1, 1, 0));
        assert_eq!(r.moxes[1].salvo, 5);
        assert_eq!(r.moxes[1].thetis.zero_ms, r.moxes[1].thetis.tail_ms, "the next over is whole");
    }

    /// A silence past the packet window while the buffer still waits for its
    /// target - not transmitting, so no safety - empties what it holds (249):
    /// the lone packet from before the silence never reaches the transmitter.
    /// What the buffer learned stays, and the over that follows is whole.
    #[test]
    fn a_silence_past_the_window_empties_the_buffer_but_keeps_what_it_learned() {
        let mut arrivals = vec![Arrival { mono_ms: 1_000, seq: 0, ts: 1_000, bytes: 60, ptt: true }];
        arrivals.extend(steady_over(3_500, 1, 50).into_iter().map(|mut a| {
            a.ts += 1_000;
            a
        }));
        let r = replay(&arrivals, &Settings::default());
        assert_eq!((r.safety_takes, r.forgets, r.silence_discards), (0, 0, 1));
        assert_eq!(r.moxes.len(), 1);
        assert_eq!(r.cleared, 1, "the one packet from before the silence");
        assert_eq!(r.moxes[0].thetis.zero_ms, r.moxes[0].thetis.tail_ms);
        // At exactly the window it stays: the comparison is `>`, as in
        // `silence_ended_the_over`.
        let mut at_window = vec![Arrival { mono_ms: 1_000, seq: 0, ts: 1_000, bytes: 60, ptt: true }];
        at_window.extend(steady_over(3_000, 1, 50).into_iter().map(|mut a| {
            a.ts += 1_000;
            a
        }));
        assert_eq!(replay(&at_window, &Settings::default()).silence_discards, 0);
    }

    /// Release packets that come after the release played find no holder in
    /// the session and stay out of the buffer, as in `network.rs` (`else if
    /// session.tx_holder(Thetis) == Some(addr)`): the next press finds nothing
    /// of them to clear (build 255).
    #[test]
    fn release_packets_after_the_release_stay_out_of_the_buffer() {
        let mut first = steady_over(1_000, 0, 100);
        for a in first.iter_mut().filter(|a| a.seq >= 101) {
            a.mono_ms += 500;
        }
        let mut arrivals = first;
        arrivals.extend(steady_over(6_000, 103, 50));
        arrivals.sort_by_key(|a| a.mono_ms);
        let r = replay(&arrivals, &Settings::default());
        assert_eq!(r.moxes.len(), 2);
        assert_eq!(r.cleared, 0, "the two late release packets never went in");
        // Nor do they stand in the buffer for a tick: from after the first
        // over's last frames (~3060 ms) to the next press it is empty, also when
        // they come (3520 and 3540 ms).
        let between = r.trace.iter().filter(|t| (3_200..5_900).contains(&t.ms));
        let seen: Vec<(u64, usize)> = between.clone().filter(|t| t.depth > 0).map(|t| (t.ms, t.depth)).take(8).collect();
        assert!(between.clone().count() > 100 && between.clone().all(|t| t.depth == 0), "the buffer between the overs: {seen:?} of {}", between.clone().count());
    }

    /// A packet missing inside an over is rebuilt from the next one: one frame
    /// concealed, no zeros in the middle (round 246, path C).
    #[test]
    fn a_missing_packet_is_rebuilt_from_the_next_one() {
        let arrivals: Vec<Arrival> = steady_over(1_000, 0, 100).into_iter().filter(|a| a.seq != 50).collect();
        let r = replay(&arrivals, &Settings::default());
        assert_eq!(r.moxes.len(), 1);
        // The buffer counts it missing (`stats_lost`); the decoder rebuilds it.
        assert_eq!((r.concealed, r.lost), (1, 1));
        let m = &r.moxes[0];
        assert_eq!(m.thetis.zero_ms, m.thetis.tail_ms, "{:?}", m.thetis);
        assert_eq!(m.frames_out + m.frames_lost, 99, "98 with audio and the rebuilt one");
    }

    /// The replay is Thetis's chain: a radio's rows (build 263) stay out, and a
    /// file from before the stream column is all Thetis.
    #[test]
    fn the_replay_reads_only_thetis_rows() {
        let text = "# x
wall_ms,mono_ms,kind,seq,ts,bytes,ptt,stream
                    1,20,tx_arrival,1,0,30,1,thetis
                    2,30,tx_arrival,7,0,30,1,radio1
                    3,40,tx_arrival,8,0,30,1,radio2
                    4,50,tx_arrival,2,20,30,0
";
        let rows = parse_measurement(text);
        assert_eq!(rows.iter().map(|a| a.seq).collect::<Vec<_>>(), vec![1, 2]);
    }

    /// Pins the model on one real over of a drive (server 231): replayed with
    /// the defaults it comes close to what Thetis reported for it - duration
    /// within 50 ms, the average queue within 10 ms, the zeros within 20 ms,
    /// the longest gap within two blocks. One over, not a drive: how well the
    /// model covers a whole drive is in the reports of step B and of the drive
    /// of 2026-10-18 (round 246). The number of gaps is not held: the model
    /// counts fewer (3 against 5). And the tick's second pull, off, gives more
    /// zeros (build 246).
    #[test]
    fn the_replay_is_pinned_on_one_real_over() {
        let text = include_str!("../testdata/tx-replay-one-over.txt");
        let arrivals = parse_measurement(text);
        assert_eq!(arrivals.len(), 232);
        let reports = parse_reports(text);
        assert_eq!(reports.len(), 1);
        let real = reports[0].1;
        assert_eq!(real.zero_ms, 158);
        let r = replay(&arrivals, &until_257());
        assert_eq!(r.moxes.len(), 1);
        assert_eq!(pair_reports(&r.moxes, &reports), vec![Some(0)]);
        let m = r.moxes[0].thetis;
        assert!((m.duration_ms - real.duration_ms).abs() <= 50, "{m:?}");
        assert!((m.average_ms - real.average_ms).abs() <= 10, "{m:?}");
        assert!((m.zero_ms - real.zero_ms).abs() <= 20, "{m:?}");
        assert!((m.longest_gap_ms - real.longest_gap_ms).abs() <= 3, "{m:?}");
        let without = replay(&arrivals, &Settings { second_pull: false, ..until_257() });
        assert!(without.moxes[0].thetis.zero_ms > m.zero_ms + 20, "{:?}", without.moxes[0].thetis);
    }

    /// Thetis's report as the server logs it, and the pairing of a replay with
    /// the log: in order, on the duration, skipping what is in only one.
    #[test]
    fn reports_are_read_and_paired_in_order_on_their_duration() {
        let line = "[12:00:00.000 INFO ] Thetis TX queue, previous over (trx): 4834 ms, 2 ms of zeros before the first audio, \
                    lowest -1 ms queued, 158 ms of zeros in 119 underrun(s), 46 ms left at the end, 3 request(s) open; average 83 ms \
                    queued, 5 gap(s), longest 52 ms, 7 ms of it at the end; blocks of 64 samples at 48000 Hz";
        let r = parse_reports(line);
        assert_eq!(r[0].0, "12:00:00.000");
        assert_eq!(
            r[0].1,
            ThetisReport {
                duration_ms: 4834,
                lead_zero_ms: 2,
                lowest_ms: -1,
                zero_ms: 158,
                underruns: 119,
                left_ms: 46,
                open_requests: 3,
                average_ms: 83,
                gaps: 5,
                longest_gap_ms: 52,
                tail_ms: 7,
            }
        );
        let mox = |d: i64| MoxResult {
            on_ms: 0,
            off_ms: 0,
            thetis: ThetisReport { duration_ms: d, ..Default::default() },
            salvo: 5,
            frames_out: 0,
            latency_ms: Vec::new(),
            silence_ms: 0,
            mid_silence_ms: 0,
            frames_lost: 0,
        };
        let rep = |d: i64| (String::new(), ThetisReport { duration_ms: d, ..Default::default() });
        let reports = vec![rep(9000), rep(1500), rep(186), rep(4000), rep(2500)];
        let moxes = vec![mox(1450), mox(4050), mox(7000), mox(2600)];
        assert_eq!(pair_reports(&moxes, &reports), vec![Some(1), Some(3), None, Some(4)]);
    }

    /// The program's options set the knobs; a file is a file, an unknown
    /// option or a missing number is refused.
    #[test]
    fn the_options_set_the_knobs() {
        let args = |v: &[&str]| v.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        let (s, files, out) = parse_settings(&args(&["m.csv", "--min-depth", "2", "--extra-ms", "25", "--no-second-pull", "s.log"])).unwrap();
        assert_eq!(files, vec!["m.csv", "s.log"]);
        assert_eq!(s, Settings { min_depth: 2, extra_ms: 25, second_pull: false, ..Settings::default() });
        assert_eq!(out, Outputs::default());
        // The knobs of step C (build 255), and the two files.
        let (s, _, out) = parse_settings(&args(&[
            "--ms-per-frame", "30", "--target-add", "1", "--spike-decay", "115", "--cap-above-max", "4", "--trim-above", "3",
            "--trim-every", "6", "--fast-trim-above", "5", "--hold-ms", "100", "--ring-low-ms", "20", "--csv", "o.csv", "--trace", "t.csv",
        ]))
        .unwrap();
        let t = s.tuning;
        assert_eq!((t.ms_per_frame, t.target_add, t.spike_decay_packets, t.cap_above_max), (30.0, 1, 115.0, 4));
        assert_eq!((t.trim_above_target, t.trim_every_pulls, t.fast_trim_above), (3, 6, 5));
        assert_eq!((s.hold_ms, s.ring_low), (100, 960));
        assert_eq!(out, Outputs { csv: Some("o.csv".into()), trace: Some("t.csv".into()) });
        assert!(parse_settings(&args(&["--ms-per-frame", "0"])).is_err(), "a divisor of 0 is refused");
        assert!(parse_settings(&args(&["--csv"])).is_err());
        let (s, _, _) = parse_settings(&args(&["--mox-delay-ms", "60", "--phase-ms", "5", "--link-ms", "2", "--buffering-ms", "20", "--block", "128", "--max-depth", "10"])).unwrap();
        assert_eq!((s.mox_delay_ns, s.tick_phase_ns, s.link_ns, s.buffering_ms, s.block, s.max_depth), (60 * MS, 5 * MS, 2 * MS, 20, 128, 10));
        assert!(parse_settings(&args(&["--min-depth"])).is_err());
        assert!(parse_settings(&args(&["--fast"])).is_err());
    }

    /// The numbers the replay copies from the server are still the server's
    /// (working rule 15: a second place, with a test that catches drift).
    #[test]
    fn the_replay_keeps_the_servers_numbers() {
        let ptt = include_str!("ptt.rs");
        assert!(ptt.contains("const PTT_PREFILL_MS_TCI: u64 = 10;"));
        assert!(ptt.contains("const PTT_TAIL_MIN_MS_TCI: u64 = 25;"));
        assert!(ptt.contains("const PTT_TAIL_MARGIN_MS_TCI: u64 = 10;"));
        assert!(ptt.contains("const HEARTBEAT_TIMEOUT_MS: u64 = 2000;"));
        assert!(ptt.contains("const PTT_PACKET_TIMEOUT_MS: u64 = 2000;"));
        assert!(ptt.contains("let depth_ms = (jitter_depth as u64) * 20;"));
        assert_eq!((PREFILL_NS, TAIL_MIN_MS, TAIL_MARGIN_MS, SAFETY_NS), (10 * MS, 25, 10, 2_000 * MS));
        let stream = include_str!("../../sdr-remote-core/src/stream.rs");
        assert!(stream.contains(&format!("const CONCEAL_MAX_FRAMES: u32 = {};", CONCEAL_MAX_FRAMES)));
        let tci = include_str!("tci.rs");
        assert!(tci.contains(&format!("AUDIO_STREAM_SAMPLES:{};", REQUEST_SAMPLES)));
        let net = include_str!("network.rs");
        let s = Settings::default();
        assert!(net.contains("JitterBuffer::for_transmit(TX_MIN_DEPTH, TX_MAX_DEPTH);"));
        assert_eq!((s.min_depth, s.max_depth), (TX_MIN_DEPTH, TX_MAX_DEPTH));
        assert!(net.contains(&format!("interval(Duration::from_millis({}))", TICK_NS / MS)));
        // Thetis's tick is the one playout step since build 288, with its ring mark.
        assert!(net.contains("let ring = ring_at_tick.map(|level| (level as u64, crate::tci::TX_RING_LOW as u64));"));
        let step = include_str!("tx_recover.rs");
        assert!(step.contains("crate::tci::pull_extra_for_ring_below(lead as usize, Some(level as usize), jitter.depth(), jitter.target_depth())"));
        // The knobs default to the server's numbers (build 255).
        assert_eq!((s.tuning, s.hold_ms, s.ring_low), (transmit_stand_in(), 100, TX_RING_LOW));
        assert_eq!(s.tuning, Tuning { trim_every_pulls: 12, ..Tuning::transmit() }, "the slow trim stands in for the pauses");
        assert!(tci.contains("const TX_HOLD_MAX: std::time::Duration = std::time::Duration::from_millis(100);"));
        // What the stand-in stands in for (build 258): the tick lets a quiet frame go.
        assert!(step.contains("if trim.let_go(&pcm, backlog, !let_go, now) {"));
        // The holder rules the replay models (round 246; builds 175, 247, 249,
        // 251), and the safety's tick.
        assert!(net.contains("let mut safety_tick = interval(Duration::from_millis(100));"));
        assert_eq!(SAFETY_TICK_NS, 100 * MS);
        assert!(net.contains("info!(\"TX holder {} again, keeping what the jitter buffer learned\", addr);\n                                    jitter_buf.discard();"));
        assert!(net.contains("info!(\"New TX holder {}, resetting jitter buffer and decoder\", addr);\n                                    jitter_buf.reset();"));
        assert!(net.contains("() => {\n                jitter_buf.reset();\n                tx_holder_addr = None;\n                line_learned_from = None;\n            };"));
        assert!(net.contains("if tx_holder_addr == Some(addr) && crate::ptt::silence_ended_the_over(silence) {"));
        assert!(net.contains("if let Some(holder) = session.lock().await.release_after_safety() {"));
        // A release plays only from the holder: the decision since build 285
        // (`tx_intake`). Not modelled here: a late packet with the flag no
        // longer takes Thetis (`Intake::Stale`, build 285).
        let intake = include_str!("tx_intake.rs");
        assert!(intake.contains("if !a.ptt && !a.from_holder {\n        return Intake::Pass;"));
        assert!(net.contains("keyed: holder == Some(addr),"));
        assert!(ptt.contains("since_packet.is_some_and(|d| d > Duration::from_millis(PTT_PACKET_TIMEOUT_MS))"));
        assert!(ptt.contains("if self.since_packet.map_or(true, |d| d > self.packet_timeout) {"));
    }
}
