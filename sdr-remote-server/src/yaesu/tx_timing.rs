// SPDX-License-Identifier: GPL-2.0-or-later
//
// The transmit chain of a Yaesu radio, timed the way Thetis's is (build 163).
//
// Thetis's chain logs prefill->TRX, the tail and "RX audio back N ms after TRX
// off" per transmission; a Yaesu logged only "Yaesu PTT: TX/RX", so the two
// could not be compared (owner, 2026-10-10: "yaesu 1 991a, yaesu 2 ftx1 en
// thetis naast elkaar"). One of these per radio, kept in its `YaesuState`; the
// PTT request, the CAT write, the transmit audio and the receive audio each
// report to it. Measuring only: nothing here changes what the chain does.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use sdr_remote_core::dev_info;
use std::sync::Arc;

use log::info;

use crate::ptt::{turnaround_clock, RxBackWatch};

#[derive(Clone, Debug, Default)]
pub struct TxTiming {
    /// "Radio 1" or "Radio 2", the start of every line.
    label: &'static str,
    /// When the PTT-on request came in; 0 = nothing waits for TX1.
    requested_at: Arc<AtomicU64>,
    /// When the PTT-off request came in; 0 = nothing waits for TX0.
    released_at: Arc<AtomicU64>,
    /// When the PTT-on request came in, until the first transmit audio after it.
    first_audio_from: Arc<AtomicU64>,
    /// "RX audio back N ms after TRX off", the Thetis watch with the radio's name.
    pub rx_back: RxBackWatch,
    /// `TX1;` is on the wire and `TX0;` is not: the radio transmits, and the
    /// transmit audio may go into the sound card (build 164).
    keyed: Arc<AtomicBool>,
    /// Counts PTT-on requests, so audio held from an earlier transmission is
    /// never sent in a later one (build 164).
    over: Arc<AtomicU64>,
    /// What the sound card filled with silence in the middle of an over: frames
    /// (one sample per channel) and gaps, from its callback (build 263).
    padded_frames: Arc<AtomicU64>,
    gaps: Arc<AtomicU64>,
    /// What stands in the sound card's ring, in ms, after its last callback
    /// (build 278): the level the radio's task rebuilds the lead against.
    ring_ms: Arc<AtomicU64>,
    /// The developer's lead in the ring (build 278): the target in ms, and
    /// whether only every other over gets it. Set at the start from the
    /// settings and live from the server screen.
    lead_setting_ms: Arc<AtomicU64>,
    lead_alternate: Arc<AtomicBool>,
    /// The target of the current over, fixed at its PTT request, so the bridge
    /// and the task use the same one for the whole over.
    over_lead_ms: Arc<AtomicU64>,
    /// An "on" that no "off" and no `TX0;` has ended yet (build 279): a second
    /// "on" inside it is the same transmission, not a new over.
    pressed: Arc<AtomicBool>,
}

/// The sound card's view of a radio's transmission (build 263): whether it is
/// keyed and which over it is, and where to count the silence it fills in the
/// middle of one. Atomics only - it lives in the output callback.
#[derive(Clone, Debug, Default)]
pub struct OutputGaps {
    keyed: Arc<AtomicBool>,
    over: Arc<AtomicU64>,
    padded_frames: Arc<AtomicU64>,
    gaps: Arc<AtomicU64>,
    ring_ms: Arc<AtomicU64>,
}

impl OutputGaps {
    pub fn keyed(&self) -> bool {
        self.keyed.load(Ordering::Relaxed)
    }

    pub fn over(&self) -> u64 {
        self.over.load(Ordering::Relaxed)
    }

    /// A gap of `frames` closed: audio came again after it.
    pub fn add(&self, frames: u64) {
        self.padded_frames.fetch_add(frames, Ordering::Relaxed);
        self.gaps.fetch_add(1, Ordering::Relaxed);
    }

    /// What the ring holds after this callback, in ms (build 278).
    pub fn set_ring_ms(&self, ms: u64) {
        self.ring_ms.store(ms, Ordering::Relaxed);
    }
}

/// The highest lead target (build 278): what the hold before TX1 may keep
/// (`HOLD_MAX_MS`), about Thetis's own queue.
pub const LEAD_MAX_MS: u64 = HOLD_MAX_MS;

/// The lead every over gets (build 294; owner, 2026-10-23): 80 ms, no longer a
/// setting. Measured with the alternating lead of 278: on the evening of
/// 2026-10-23 the overs at 80 ms played without silence, those at 0 ms had
/// 10-30 ms each, with no gap on the network.
pub const LEAD_MS: u64 = 80;

/// The lead target of over `over` (build 278): the setting, or 0 for every
/// even over when it alternates - both settings over the same road.
pub fn lead_for_over(setting_ms: u64, alternate: bool, over: u64) -> u64 {
    if alternate && over % 2 == 0 {
        0
    } else {
        setting_ms
    }
}

/// The zeros that go into the ring ahead of an over's first audio, so its lead
/// stands ready at once (build 278). On the air the same as holding the audio
/// until the lead is there, without a hold that a short press never fills.
#[derive(Debug, Default)]
pub struct LeadStart {
    over: u64,
}

impl LeadStart {
    /// The first block of over `over` to reach the ring, `len` mono samples:
    /// how many zero samples go ahead of it for `lead` to stand ready. 0 for
    /// every later block of the over, and for a block that is long enough.
    pub fn pad(&mut self, over: u64, lead: usize, len: usize) -> usize {
        if over == self.over {
            return 0;
        }
        self.over = over;
        lead.saturating_sub(len)
    }
}

/// The output callback's side of `OutputGaps` (build 263). A zero the ring
/// could not give is silence on the air; it counts once audio comes again in
/// the same over while the radio is keyed. Silence before the first audio of
/// an over and after its last is the start and the tail, not a gap - as
/// Thetis's ring counts ("padded with silence before more audio came").
#[derive(Debug, Default)]
pub struct GapWatch {
    over: u64,
    heard: bool,
    pending: u64,
}

impl GapWatch {
    /// One sample: from the ring (`audio`) or a zero in its place. Returns the
    /// samples of a gap that just closed.
    pub fn sample(&mut self, audio: bool, keyed: bool, over: u64) -> Option<u64> {
        if over != self.over {
            self.over = over;
            self.heard = false;
            self.pending = 0;
        }
        if !keyed {
            // A keying that ends ends what waited, and the next keying starts
            // like an over: its silence before audio is a start (round 263).
            self.pending = 0;
            self.heard = false;
            return None;
        }
        if audio {
            self.heard = true;
            return (self.pending > 0).then(|| std::mem::take(&mut self.pending));
        }
        if self.heard {
            self.pending += 1;
        }
        None
    }
}

/// One over as the bridge writes it into the ring (build 263): when its first
/// audio went in, and what stood in the ring a second later - the radio's
/// counterpart of the lead in Thetis's ring. Since build 278 also the over's
/// lead target and what stood in the ring at its first audio.
#[derive(Debug, Default)]
pub struct OverOutput {
    over: u64,
    first_ms: Option<u64>,
    summary: OverSummary,
}

/// What the over line says about one over (build 278).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OverSummary {
    pub queued_1s: Option<u64>,
    pub lead_ms: u64,
    pub first_queued_ms: u64,
}

impl OverOutput {
    /// A block of over `over` went into the ring at `now_ms`, leaving
    /// `queued_ms` there, with `lead_ms` the over's target. At the first block
    /// of a new over: the previous one's summary, once - its ms one second
    /// after its first audio (`None` when it was shorter), its target, and
    /// what stood ready at its first audio.
    pub fn written(&mut self, over: u64, now_ms: u64, queued_ms: u64, lead_ms: u64) -> Option<OverSummary> {
        let mut report = None;
        if over != self.over {
            if self.first_ms.is_some() {
                report = Some(self.summary);
            }
            *self = OverOutput { over, ..Default::default() };
        }
        match self.first_ms {
            None => {
                self.first_ms = Some(now_ms);
                self.summary.lead_ms = lead_ms;
                self.summary.first_queued_ms = queued_ms;
            }
            Some(first) if self.summary.queued_1s.is_none() && now_ms >= first + 1000 => {
                self.summary.queued_1s = Some(queued_ms)
            }
            Some(_) => {}
        }
        report
    }
}

/// The over line of a radio's sound card (build 263; the lead since 278).
pub fn output_line(label: &str, padded_ms: u64, gaps: u64, s: &OverSummary) -> String {
    format!(
        "{} TX output, previous over: {} ms padded with silence before more audio came, in {} gap(s); {} ms queued one second after the first audio; lead target {} ms, {} ms ready at the first audio",
        label,
        padded_ms,
        gaps,
        s.queued_1s.map_or("-".to_string(), |q| q.to_string()),
        s.lead_ms,
        s.first_queued_ms
    )
}

/// The longest transmit audio held back waiting for `TX1;`: about what
/// Thetis has ready when it starts (40-100 ms), so a slow radio does not carry
/// a long delay through the whole transmission (build 165, owner's choice A;
/// review of 164: at 500 ms a DATA-FM start could carry half a second).
pub const HOLD_MAX_MS: u64 = 150;
/// What an operator's release waits at least for the transmit audio to play
/// out before `TX0;` (build 164); the floor of `tail_limit` since build 168,
/// not the longest. A safety release never waits.
pub const TAIL_MAX_MS: u64 = 250;
/// After the ring is empty: the sound card's own buffer still holds some.
pub const TAIL_MARGIN_MS: u64 = 30;
/// Beyond what was ready at the release, the room the wait gives itself
/// (build 168, owner's choice A: what the buffer held is played out, as
/// Thetis does, instead of cut at 250 ms - `veldproef-build167.md`).
pub const TAIL_EXTRA_MS: u64 = 100;
/// The release never waits longer than this, whatever was ready: the
/// transmitter goes off within a second of the operator letting go.
pub const TAIL_CAP_MS: u64 = 1000;

/// How long a release may wait when `first_ms` of transmit audio was ready
/// at the start: `TAIL_MAX_MS`, or what was ready plus `TAIL_EXTRA_MS` when
/// the jitter buffer had grown, never more than `TAIL_CAP_MS` (build 168).
pub fn tail_limit(first_ms: u64) -> u64 {
    TAIL_MAX_MS.max(first_ms + TAIL_EXTRA_MS).min(TAIL_CAP_MS)
}

/// Whether a release has waited long enough before `TX0;`: the ring is empty,
/// or the wait reached its `limit` (`tail_limit`).
pub fn tail_done(queued_ms: u64, waited_ms: u64, limit_ms: u64) -> bool {
    queued_ms == 0 || waited_ms >= limit_ms
}

/// What a release's wait found: how long it waited, how much was queued when
/// it began, how much more it found after the margin (build 167), and
/// whether it waited for that or was past its bound (build 168).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tail {
    pub waited: u64,
    pub first: u64,
    pub late: u64,
    pub again: bool,
}

/// What the line after the margin says, if anything: whether the release
/// waited for what it found, or was past its bound (build 169; review of 168:
/// the two could be swapped unseen).
pub fn after_margin(tail: &Tail) -> Option<&'static str> {
    (tail.late > 0).then_some(if tail.again { "waited again" } else { "past the bound, TX0 anyway" })
}

/// `wait_for_tail` on the real clock and the real sleep - the one place the
/// poll thread's wait gets them, so it cannot pass another (build 166; review
/// of 165: the clock `poll.rs` passed was untested).
pub fn wait_for_tail_now(queued_ms: impl FnMut() -> u64) -> Tail {
    let started = std::time::Instant::now();
    wait_for_tail(
        queued_ms,
        || started.elapsed().as_millis() as u64,
        |ms| std::thread::sleep(std::time::Duration::from_millis(ms)),
    )
}

/// The wait before an operator's `TX0;`: until `queued_ms()` reads empty or
/// `tail_limit` has passed, then `TAIL_MARGIN_MS` for the sound card's own
/// buffer if there was anything to play. `now_ms` and `pause` are the clock
/// and the sleep, so a test can run it on a ring that never empties (build
/// 165; review of 164: the whole bound of the tail hung on code no test ran).
/// Returns how long it waited and what was queued at the start.
pub fn wait_for_tail(
    mut queued_ms: impl FnMut() -> u64,
    mut now_ms: impl FnMut() -> u64,
    mut pause: impl FnMut(u64),
) -> Tail {
    let started = now_ms();
    let first = queued_ms();
    // The bound follows the most that was ever ready, not only the first
    // reading: frames handed between threads can arrive after a first
    // reading of 0 (build 169; review of 168, a reviewer's probe).
    let mut limit = tail_limit(first);
    let mut most = first;
    fn until_empty(started: u64, limit: &mut u64, most: &mut u64, queued_ms: &mut impl FnMut() -> u64, now_ms: &mut impl FnMut() -> u64, pause: &mut impl FnMut(u64)) {
        loop {
            let waited = now_ms().saturating_sub(started);
            let queued = queued_ms();
            *most = (*most).max(queued);
            *limit = (*limit).max(tail_limit(queued));
            if tail_done(queued, waited, *limit) {
                break;
            }
            pause(5);
        }
    }
    until_empty(started, &mut limit, &mut most, &mut queued_ms, &mut now_ms, &mut pause);
    // The margin whenever anything was ready, also when it came after a
    // first reading of 0.
    if most > 0 {
        pause(TAIL_MARGIN_MS);
    }
    // One more look: the last frames can be on their way between the decode
    // task and the bridge's ring when the counts read empty (review of 166).
    // Past the bound it goes anyway, as for a ring that never empties.
    let late = queued_ms();
    let again = late > 0 && now_ms().saturating_sub(started) < limit;
    if again {
        until_empty(started, &mut limit, &mut most, &mut queued_ms, &mut now_ms, &mut pause);
        pause(TAIL_MARGIN_MS);
    }
    Tail { waited: now_ms().saturating_sub(started), first, late, again }
}

/// Transmit audio that came before the radio transmits, held back until it
/// does - as Thetis's transmit ring holds it until Thetis asks (build 164;
/// `veldproef-build163.md`: on the 991A ~50 ms of the start went into a radio
/// still receiving). One per radio, in the bridge to the sound card.
#[derive(Debug, Default)]
pub struct Hold {
    samples: Vec<f32>,
    over: u64,
    /// Samples thrown away in this over: beyond `max` while not keyed (build 184).
    dropped: usize,
    /// What the previous over threw away, once, for the log line (build 184).
    report: Option<usize>,
}

impl Hold {
    /// A block of transmit audio of transmission `over`. Keyed: what was held
    /// and this block, and how many samples had been held. Not keyed: held,
    /// the oldest dropped beyond `max` samples; nothing to send yet.
    pub fn take(&mut self, samples: Vec<f32>, keyed: bool, over: u64, max: usize) -> Option<(Vec<f32>, usize)> {
        if over != self.over {
            // What still waited when the next over began never went out.
            let thrown = self.dropped + self.samples.len();
            if thrown > 0 {
                self.report = Some(thrown);
            }
            self.dropped = 0;
            self.samples.clear();
            self.over = over;
        }
        if keyed {
            if self.samples.is_empty() {
                return Some((samples, 0));
            }
            let held = self.samples.len();
            let mut out = std::mem::take(&mut self.samples);
            out.extend_from_slice(&samples);
            return Some((out, held));
        }
        self.samples.extend_from_slice(&samples);
        if self.samples.len() > max {
            let extra = self.samples.len() - max;
            self.samples.drain(..extra);
            self.dropped += extra;
        }
        None
    }

    /// Samples the previous over threw away, once, when there were any (build 184).
    pub fn take_report(&mut self) -> Option<usize> {
        self.report.take()
    }
}

/// Samples of one transmission that did not reach the sound card, reported
/// once when the next transmission begins - the same rhythm as `Hold`'s
/// report (build 185; review of 184).
#[derive(Debug, Default)]
pub struct OverTally {
    over: u64,
    count: usize,
}

impl OverTally {
    /// Count `n` samples of transmission `over`. Returns what the previous
    /// transmission lost, once, when this is the first block of a new one.
    pub fn add(&mut self, over: u64, n: usize) -> Option<usize> {
        let mut report = None;
        if over != self.over {
            if self.count > 0 {
                report = Some(self.count);
            }
            self.count = 0;
            self.over = over;
        }
        self.count += n;
        report
    }
}

impl TxTiming {
    /// A radio's timing, with the lead every over gets (`LEAD_MS`) from the
    /// start: the one place the number is set (review of 294).
    pub fn new(slot: u8) -> Self {
        let (label, prefix) = if slot == 0 { ("Radio 1", "Radio 1 ") } else { ("Radio 2", "Radio 2 ") };
        let timing = Self { label, rx_back: RxBackWatch::for_radio(prefix), ..Default::default() };
        timing.set_lead(LEAD_MS, false);
        timing
    }

    pub fn label(&self) -> &'static str {
        self.label
    }

    /// The radio transmits: `TX1;` written, `TX0;` not yet.
    pub fn keyed(&self) -> bool {
        self.keyed.load(Ordering::Relaxed)
    }

    /// The current transmission, for `Hold`.
    pub fn over(&self) -> u64 {
        self.over.load(Ordering::Relaxed)
    }

    /// The release now being written was asked for by the operator (a safety
    /// release has no request behind it, and does not wait).
    pub fn asked_release(&self) -> bool {
        self.released_at.load(Ordering::Relaxed) != 0
    }

    /// What the sound card's callback needs to count its gaps (build 263).
    pub fn output_gaps(&self) -> OutputGaps {
        OutputGaps {
            keyed: self.keyed.clone(),
            over: self.over.clone(),
            padded_frames: self.padded_frames.clone(),
            gaps: self.gaps.clone(),
            ring_ms: self.ring_ms.clone(),
        }
    }

    /// The ring's level for the lead's rebuild (build 278), in ms: `None`
    /// while the radio is not keyed - the audio then waits in `Hold` and not
    /// in the ring, and a second frame a tick would push the start of the
    /// speech out of the hold. `None` is "no reader" to the rule.
    pub fn ring_ms(&self) -> Option<u64> {
        self.keyed().then(|| self.ring_ms.load(Ordering::Relaxed))
    }

    /// A new lead setting (build 278), from the settings at the start or the
    /// server screen; the next over takes it.
    pub fn set_lead(&self, ms: u64, alternate: bool) {
        let ms = ms.min(LEAD_MAX_MS);
        self.lead_setting_ms.store(ms, Ordering::Relaxed);
        self.lead_alternate.store(alternate, Ordering::Relaxed);
        // Named in advance (working rule 11, build 278).
        dev_info!(
            "{} TX lead target: {} ms (every other over: {})",
            self.label,
            ms,
            if alternate { "yes" } else { "no" }
        );
    }

    pub fn lead_setting(&self) -> (u64, bool) {
        (self.lead_setting_ms.load(Ordering::Relaxed), self.lead_alternate.load(Ordering::Relaxed))
    }

    /// The lead target of the current over, in ms (build 278).
    pub fn lead_ms(&self) -> u64 {
        self.over_lead_ms.load(Ordering::Relaxed)
    }

    /// The gaps the sound card counted since the last look: (frames, gaps).
    pub fn take_gaps(&self) -> (u64, u64) {
        (self.padded_frames.swap(0, Ordering::Relaxed), self.gaps.swap(0, Ordering::Relaxed))
    }

    /// A new session of the poll loop: a release that was asked for but never
    /// written (the port went away) is forgotten, so the safety release that
    /// follows goes at once (build 165; review of 164).
    pub fn forget_release(&self) {
        self.released_at.store(0, Ordering::Relaxed);
        // Nothing transmits after a new session either: the next "on" is a new over.
        self.pressed.store(false, Ordering::Relaxed);
    }

    /// Transmit audio that waited for `TX1;` went out.
    pub fn held(&self, ms: u64) {
        dev_info!("{} PTT metrics: held {} ms of TX audio until TX1, then sent it", self.label, ms);
    }

    /// The release waited for the transmit audio to play out.
    pub fn tail_waited(&self, tail: Tail) {
        if let Some(what) = after_margin(&tail) {
            // Named in advance (working rule 11, builds 167 and 168).
            dev_info!("{} PTT metrics: TX0 found {} ms more TX audio after the margin - {}", self.label, tail.late, what);
        }
        dev_info!("{} PTT metrics: TX0 waited {} ms for {} ms of TX audio to play out", self.label, tail.waited, tail.first);
    }

    /// The operator's PTT request, as the server received it.
    pub fn request(&self, on: bool) {
        self.request_at(turnaround_clock(), on);
    }

    fn request_at(&self, now: u64, on: bool) {
        if on {
            // A second "on" with no "off" and no TX0 since the first - a resend,
            // or a press again during the roger beep - is the same transmission:
            // a new over would empty the buffer, put the lead's zeros in the
            // middle of the speech and flip "every other over" (review of 278).
            // The radio is keyed as before; only the count stays.
            if self.pressed.swap(true, Ordering::Relaxed) {
                // Named in advance (working rule 11, build 279).
                info!("{} PTT: on again while still pressed - the same transmission, no new over", self.label);
                return;
            }
            self.requested_at.store(now, Ordering::Relaxed);
            self.first_audio_from.store(now, Ordering::Relaxed);
            // The over's lead is fixed before its number is, so no block of
            // the new over can read the old target (build 278).
            let over = self.over.load(Ordering::Relaxed) + 1;
            let (setting, alternate) = self.lead_setting();
            self.over_lead_ms.store(lead_for_over(setting, alternate, over), Ordering::Relaxed);
            self.over.fetch_add(1, Ordering::Relaxed);
        } else {
            self.pressed.store(false, Ordering::Relaxed);
            self.released_at.store(now, Ordering::Relaxed);
        }
    }

    /// A packet of transmit audio arrived: the first after a request is timed.
    pub fn tx_audio(&self) {
        let _ = self.tx_audio_at(turnaround_clock());
    }

    fn tx_audio_at(&self, now: u64) -> Option<u64> {
        let from = self.first_audio_from.swap(0, Ordering::Relaxed);
        if from == 0 {
            return None;
        }
        let ms = now.saturating_sub(from);
        // Named in advance (working rule 11, build 163).
        dev_info!("{} PTT metrics: first TX audio {} ms after the request", self.label, ms);
        Some(ms)
    }

    /// `TX1;` or `TX0;` has gone out on the serial port, with `queued_ms` of
    /// transmit audio standing in the sound card's ring at that moment.
    pub fn written(&self, on: bool, queued_ms: u64) {
        let _ = self.written_at(turnaround_clock(), on, queued_ms);
    }

    fn written_at(&self, now: u64, on: bool, queued_ms: u64) -> Option<u64> {
        self.keyed.store(on, Ordering::Relaxed);
        if on {
            self.rx_back.trx_on();
            let at = self.requested_at.swap(0, Ordering::Relaxed);
            if at == 0 {
                return None;
            }
            let ms = now.saturating_sub(at);
            dev_info!(
                "{} PTT metrics: TX1 written {} ms after the request, {} ms of TX audio queued",
                self.label, ms, queued_ms
            );
            Some(ms)
        } else {
            self.rx_back.trx_off();
            self.first_audio_from.store(0, Ordering::Relaxed);
            // The radio is off, whoever let go - the operator, a watchdog or the
            // 2 s clock (build 265): the next "on" is a new over (build 279).
            self.pressed.store(false, Ordering::Relaxed);
            // A release that was not asked for (a watchdog) still starts the
            // RX-back watch, but has no request to time.
            let at = self.released_at.swap(0, Ordering::Relaxed);
            if at == 0 {
                return None;
            }
            let ms = now.saturating_sub(at);
            dev_info!(
                "{} PTT metrics: TX0 written {} ms after the release, {} ms of TX audio still queued",
                self.label, ms, queued_ms
            );
            Some(ms)
        }
    }
}

/// The ms of transmit audio standing in a radio's output ring: two items per
/// frame (the bridge writes mono to both channels), at the rate the output
/// stream runs - which is the ring's capacity over two, since the ring is made
/// as `sample_rate * 2` (build 164; review of 163: 48 kHz was assumed).
pub fn queued_ms(items: usize, capacity: usize) -> u64 {
    let rate = (capacity / 2).max(1) as u64;
    (items / 2) as u64 * 1000 / rate
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One transmission, timed once at each end; a write nobody asked for is
    /// not a measurement; the first audio is timed once (build 163).
    #[test]
    fn a_transmission_is_timed_once_at_each_end() {
        let t = TxTiming::new(0);
        t.request_at(1000, true);
        assert_eq!(t.tx_audio_at(1015), Some(15));
        assert_eq!(t.tx_audio_at(1035), None, "only the first audio");
        assert_eq!(t.written_at(1040, true, 20), Some(40));
        assert_eq!(t.written_at(1041, true, 20), None, "TX1 twice is one transmission");
        t.request_at(4000, false);
        assert_eq!(t.written_at(4010, false, 60), Some(10));
        assert_eq!(t.written_at(9000, false, 0), None, "a release nobody asked for");
        // A transmission that ends before its first audio: no audio line later.
        t.request_at(10_000, true);
        assert_eq!(t.written_at(10_030, true, 0), Some(30));
        t.request_at(10_500, false);
        assert_eq!(t.written_at(10_510, false, 0), Some(10));
        assert_eq!(t.tx_audio_at(10_600), None);
        assert_eq!(queued_ms(9600, 96_000), 100, "two items per frame, at capacity over two");
        assert_eq!(queued_ms(4410, 88_200), 50, "a 44.1 kHz ring");
    }

    /// The labels, and the RX-back watch the writes drive: TX0 starts it, TX1
    /// stops it (build 164; review of 163: neither was tested).
    #[test]
    fn each_radio_has_its_name_and_its_rx_back_follows_the_writes() {
        assert_eq!(TxTiming::new(0).label(), "Radio 1");
        assert_eq!(TxTiming::new(1).label(), "Radio 2");
        let t = TxTiming::new(1);
        assert!(format!("{:?}", t.rx_back).contains("Radio 2 "));
        let loud = [0.5f32; 4];
        t.request_at(100, false);
        t.written_at(110, false, 0);
        assert!(!t.keyed());
        assert!(t.rx_back.rx_block_at(crate::ptt::turnaround_clock(), &loud, 0).is_some(), "TX0 starts the wait");
        t.written_at(200, true, 0);
        assert!(t.keyed());
        assert!(t.rx_back.rx_block_at(crate::ptt::turnaround_clock(), &loud, 0).is_none(), "TX1 ends it");
    }

    /// The sound card counts a zero only between audio and audio of one keyed
    /// over: not before the first audio, not after the last, not while the
    /// radio is not keyed, and a new over starts from nothing (build 263).
    #[test]
    fn the_sound_card_counts_silence_between_audio_only() {
        let mut w = GapWatch::default();
        let mut feed = |w: &mut GapWatch, audio: bool, keyed: bool, over: u64, n: usize| {
            (0..n).filter_map(|_| w.sample(audio, keyed, over)).collect::<Vec<_>>()
        };
        assert!(feed(&mut w, false, true, 1, 50).is_empty(), "the start of an over is not a gap");
        assert!(feed(&mut w, true, true, 1, 10).is_empty());
        assert!(feed(&mut w, false, true, 1, 30).is_empty(), "counted only when audio comes again");
        assert_eq!(feed(&mut w, true, true, 1, 5), vec![30], "a gap of 30 samples");
        assert!(feed(&mut w, false, true, 1, 40).is_empty(), "the tail...");
        assert!(feed(&mut w, false, false, 1, 10).is_empty());
        assert!(feed(&mut w, true, true, 2, 1).is_empty(), "...is dropped at the next over");
        // Not keyed: a zero is not on the air.
        assert!(feed(&mut w, false, false, 2, 20).is_empty());
        assert!(feed(&mut w, true, true, 2, 1).is_empty(), "nothing waited while not keyed");
        // Silence that ran into an unkeyed stretch was a tail, also when the
        // radio is keyed again within the same over.
        assert!(feed(&mut w, false, true, 2, 15).is_empty());
        assert!(feed(&mut w, false, false, 2, 5).is_empty());
        assert!(feed(&mut w, true, true, 2, 1).is_empty(), "the tail before the unkeying is forgotten");
        // Keyed again within the over: the silence before its first audio is a
        // start, not a gap (round 263: a probe gave 30).
        assert!(feed(&mut w, false, false, 2, 20).is_empty());
        assert!(feed(&mut w, false, true, 2, 30).is_empty());
        assert!(feed(&mut w, true, true, 2, 1).is_empty(), "a new keying starts like an over");
        // And through the shared counters.
        let g = TxTiming::new(0);
        g.output_gaps().add(480);
        g.output_gaps().add(96);
        assert_eq!(g.take_gaps(), (576, 2));
        assert_eq!(g.take_gaps(), (0, 0), "taken once");
    }

    /// A second "on" before any "off" or TX0 is the same transmission (build
    /// 279; review of 278): no new over, no new lead, no new timing. An "off",
    /// a TX0 written by anyone (the 2 s clock of build 265, a watchdog), or a
    /// new session of the poll loop ends it, and the next "on" is a new over.
    #[test]
    fn a_second_on_inside_a_press_is_the_same_over() {
        let t = TxTiming::new(1);
        t.set_lead(80, true);
        t.request_at(100, true);
        let over = t.over();
        let lead = t.lead_ms();
        t.request_at(150, true);
        assert_eq!((t.over(), t.lead_ms()), (over, lead), "a resend: the same over and its lead");
        t.written_at(160, true, 0);
        t.request_at(170, true);
        assert_eq!(t.over(), over, "keyed and still pressed: the same over");
        assert_eq!(t.written_at(171, true, 0), None, "and its request is not timed twice");
        // The operator lets go: the next "on" is a new over, with the other lead.
        t.request_at(200, false);
        t.request_at(300, true);
        assert_eq!(t.over(), over + 1);
        assert_ne!(t.lead_ms(), lead, "every other over");
        // TX0 without an "off" - the 2 s clock let the radio go - then back
        // inside the press: a new over, the radio was off.
        t.written_at(400, true, 0);
        t.written_at(500, false, 0);
        t.request_at(600, true);
        assert_eq!(t.over(), over + 2, "the radio was off: a new over");
        // A new session of the poll loop forgets the press too.
        t.forget_release();
        t.request_at(700, true);
        assert_eq!(t.over(), over + 3);
    }

    /// The over line: the ring one second after the first audio, said once at
    /// the first block of the next over; "-" for an over shorter than that.
    #[test]
    fn the_over_line_says_the_ring_one_second_in() {
        let mut o = OverOutput::default();
        assert_eq!(o.written(1, 1_000, 80, 80), None);
        assert_eq!(o.written(1, 1_500, 60, 0), None, "the target of the first block counts");
        assert_eq!(o.written(1, 2_000, 40, 0), None, "one second after the first audio");
        assert_eq!(o.written(1, 2_020, 20, 0), None, "only the first look counts");
        let first = OverSummary { queued_1s: Some(40), lead_ms: 80, first_queued_ms: 80 };
        assert_eq!(o.written(2, 9_000, 100, 0), Some(first), "said at the next over");
        assert_eq!(o.written(2, 9_020, 100, 0), None, "once");
        assert_eq!(
            o.written(3, 9_500, 0, 0),
            Some(OverSummary { queued_1s: None, lead_ms: 0, first_queued_ms: 100 }),
            "an over shorter than a second"
        );
        assert_eq!(
            output_line("Radio 1", 120, 3, &first),
            "Radio 1 TX output, previous over: 120 ms padded with silence before more audio came, in 3 gap(s); 40 ms queued one second after the first audio; lead target 80 ms, 80 ms ready at the first audio"
        );
        // The start of the line stays the same: the ride's script looks for it.
        assert!(output_line("Radio 2", 0, 0, &OverSummary::default())
            .starts_with("Radio 2 TX output, previous over: 0 ms padded with silence before more audio came, in 0 gap(s); - ms queued one second after the first audio; "));
    }

    /// The lead (build 278): every over takes the setting at its request,
    /// every even over 0 when it alternates; the bridge pads the first block
    /// of an over only, and only to the target; the task sees no ring before
    /// TX1. At target 0 nothing is padded (both settings, working rule 13).
    #[test]
    fn the_lead_is_fixed_per_over_and_padded_once() {
        assert_eq!((lead_for_over(80, false, 1), lead_for_over(80, false, 2)), (80, 80));
        assert_eq!((lead_for_over(80, true, 1), lead_for_over(80, true, 2), lead_for_over(80, true, 3)), (80, 0, 80));
        assert_eq!(lead_for_over(0, true, 1), 0);
        let t = TxTiming::new(0);
        assert_eq!(t.lead_ms(), 0, "nothing set: no lead");
        t.set_lead(80, true);
        t.request_at(1, true);
        let first = t.over();
        assert_eq!(t.lead_ms(), lead_for_over(80, true, first), "the over's own number decides");
        t.request_at(2, false);
        t.request_at(2, true);
        assert_eq!(t.lead_ms(), lead_for_over(80, true, first + 1));
        assert_ne!(lead_for_over(80, true, first), lead_for_over(80, true, first + 1), "alternating");
        t.set_lead(80, false);
        assert_eq!(t.lead_ms(), lead_for_over(80, true, first + 1), "a new setting waits for the next over");
        t.request_at(3, false);
        t.request_at(3, true);
        assert_eq!(t.lead_ms(), 80);
        t.set_lead(5000, false);
        assert_eq!(t.lead_setting(), (LEAD_MAX_MS, false), "never beyond the hold's bound");
        // The ring's level reaches the task only while keyed.
        t.output_gaps().set_ring_ms(60);
        assert_eq!(t.ring_ms(), None, "not keyed: no reader");
        t.written_at(4, true, 0);
        assert_eq!(t.ring_ms(), Some(60));
        // The pad: once per over, up to the target.
        let mut s = LeadStart::default();
        assert_eq!(s.pad(1, 3840, 960), 2880, "80 ms at 48 kHz, 20 ms already there");
        assert_eq!(s.pad(1, 3840, 960), 0, "only the first block");
        assert_eq!(s.pad(2, 3840, 7200), 0, "held audio beyond the target: nothing to add");
        assert_eq!(s.pad(3, 0, 960), 0, "target 0: never");
        assert_eq!(s.pad(4, 0, 0), 0);
    }

    /// The callback and the bridge are wired to it, which no test drives
    /// (build 263): every output stream gets the radio's counters, the callback
    /// counts each sample, and the bridge says the line.
    #[test]
    fn the_sound_card_and_the_bridge_are_wired_to_the_over_line() {
        let audio = include_str!("audio.rs");
        assert!(audio.contains("if let Some(n) = watch.sample(popped.is_some(), keyed, over) {"));
        assert!(audio.contains("gaps.add(n / channels.max(1) as u64);"));
        assert!(audio.contains("let (keyed, over) = (gaps.keyed(), gaps.over());"));
        let all = [include_str!("mod.rs"), include_str!("poll.rs")].concat();
        assert_eq!(all.matches("build_output_stream(out_dev, tx_producer.clone(), gaps, ").count(), 4);
        assert_eq!(all.matches(".output_gaps();").count(), 4);
        let bridge = include_str!("mod.rs");
        assert!(bridge.contains("if let Some(summary) = over_output.written(timing.over(), crate::ptt::turnaround_clock(), queued_ms, timing.lead_ms()) {"));
        assert!(bridge.contains("tx_timing::output_line(timing.label(), frames * 1000 / tx_rate.max(1) as u64, gaps, &summary)"));
        // The lead (build 278): the callback publishes the ring, the bridge
        // pads the start of an over, the task rebuilds against it, and the
        // settings seed it at the start.
        assert_eq!(audio.matches("gaps.set_ring_ms(super::tx_timing::queued_ms(consumer.occupied_len(), consumer.capacity().get()));").count(), 1);
        assert_eq!(bridge.matches("let pad = lead_start.pad(timing.over(), (timing.lead_ms() * tx_rate as u64 / 1000) as usize, samples.len());").count(), 1);
        // And the zeros are pushed, under the same lock, before the block
        // (review of 278: without it the pad could be computed and dropped).
        assert_eq!(bridge.matches("for _ in 0..pad {\n                                    let _ = prod.try_push(0.0);\n                                    let _ = prod.try_push(0.0);\n                                }\n                                for &s in &samples {").count(), 1);
        let task = include_str!("../tx_recover.rs");
        let task = &task[..task.find("#[cfg(test)]").unwrap()];
        assert_eq!(task.matches("let ring = timing.as_ref().and_then(|t| t.ring_ms().map(|level| (level, t.lead_ms())));").count(), 1);
        assert_eq!(task.matches("play_tick(&mut jitter, &mut decoder, &mut loss, &mut trim, Instant::now(), ring)").count(), 1);
        // The same lead for every radio and every over, for everyone (build 294),
        // set where each radio's timing is made (295) and nowhere else.
        assert_eq!(include_str!("../main.rs").matches("set_lead(").count(), 0);
        let own = include_str!("tx_timing.rs");
        let own = &own[..own.find("#[cfg(test)]\nmod tests").unwrap()];
        assert_eq!(own.matches("timing.set_lead(LEAD_MS, false);").count(), 1);
        assert_eq!(own.matches(".set_lead(").count(), 1);
    }

    /// Every radio's timing starts with 80 ms and gives it to every over, not
    /// every other one (build 294, owner, 2026-10-23; the value is pinned here
    /// since the review of 294 found `LEAD_MS = 0` stayed green).
    #[test]
    fn every_radio_starts_with_the_80_ms_lead() {
        for slot in [0, 1] {
            let t = TxTiming::new(slot);
            assert_eq!(t.lead_setting(), (80, false));
            for _ in 0..2 {
                t.request(true);
                assert_eq!(t.lead_ms(), 80, "every over");
                t.request(false);
                t.forget_release();
            }
        }
    }

    /// What one transmission could not write is reported once, when the next
    /// begins, and not before (build 185).
    #[test]
    fn a_transmissions_unwritten_audio_is_reported_once_at_the_next() {
        let mut t = OverTally::default();
        assert_eq!(t.add(1, 0), None);
        assert_eq!(t.add(1, 40), None, "not within the transmission");
        assert_eq!(t.add(1, 2), None);
        assert_eq!(t.add(2, 0), Some(42), "at the next one");
        assert_eq!(t.add(2, 0), None, "once");
        assert_eq!(t.add(3, 0), None, "a transmission that wrote everything says nothing");
    }

    /// What the hold throws away - beyond its limit while not keyed, and what
    /// still waited when the next over began - is reported once, for the
    /// over it belonged to (build 184; review of 183).
    #[test]
    fn the_hold_reports_what_it_threw_away_once() {
        let mut h = Hold::default();
        assert_eq!(h.take(vec![0.1; 100], false, 1, 150), None);
        assert_eq!(h.take(vec![0.1; 100], false, 1, 150), None); // 50 over the limit
        assert_eq!(h.take_report(), None, "nothing to report within the over");
        // The next over begins with 150 still held: 50 + 150 thrown away.
        let _ = h.take(vec![0.2; 10], true, 2, 150);
        assert_eq!(h.take_report(), Some(200));
        assert_eq!(h.take_report(), None, "once");
        // An over that sent everything reports nothing.
        let _ = h.take(vec![0.3; 10], true, 3, 150);
        assert_eq!(h.take_report(), None);
    }

    /// Held until keyed, then out in one piece with the count; the oldest goes
    /// beyond the limit; audio of an earlier transmission never reaches a later
    /// one (build 164).
    #[test]
    fn transmit_audio_waits_for_the_radio_to_transmit() {
        let mut h = Hold::default();
        assert_eq!(h.take(vec![1.0; 3], false, 1, 10), None);
        assert_eq!(h.take(vec![2.0; 3], false, 1, 10), None);
        let (out, held) = h.take(vec![3.0; 2], true, 1, 10).unwrap();
        assert_eq!((out.len(), held), (8, 6));
        assert_eq!(&out[..3], &[1.0; 3], "oldest first");
        assert_eq!(h.take(vec![4.0; 2], true, 1, 10), Some((vec![4.0; 2], 0)), "keyed and nothing held: straight through");
        // Beyond the limit the oldest goes.
        assert_eq!(h.take(vec![5.0; 8], false, 1, 10), None);
        assert_eq!(h.take(vec![6.0; 4], false, 1, 10), None);
        let (out, held) = h.take(vec![], true, 1, 10).unwrap();
        assert_eq!((out.len(), held), (10, 10));
        assert_eq!(out[0], 5.0);
        // A new transmission: what an earlier one left is dropped.
        assert_eq!(h.take(vec![7.0; 3], false, 1, 10), None);
        assert_eq!(h.take(vec![8.0; 2], true, 2, 10), Some((vec![8.0; 2], 0)));
        // The tail: done when the ring is empty or the wait is long enough.
        assert!(tail_done(0, 0, TAIL_MAX_MS));
        assert!(!tail_done(20, 100, TAIL_MAX_MS));
        assert!(tail_done(20, TAIL_MAX_MS, TAIL_MAX_MS));
        assert!(!tail_done(20, 249, 250) && tail_done(20, 250, 250));
        assert_eq!((HOLD_MAX_MS, TAIL_MAX_MS, TAIL_MARGIN_MS), (150, 250, 30), "the bounds, pinned");
        // The bound grows with what was ready, and stops at the cap (build 168).
        assert_eq!((TAIL_EXTRA_MS, TAIL_CAP_MS), (100, 1000));
        assert_eq!(tail_limit(0), 250);
        assert_eq!(tail_limit(150), 250);
        assert_eq!(tail_limit(440), 540);
        assert_eq!(tail_limit(900), 1000);
        assert_eq!(tail_limit(5000), 1000);
        // A request counts the transmission; a write keys and unkeys.
        let t = TxTiming::new(0);
        let before = t.over();
        t.request_at(1, true);
        assert_eq!(t.over(), before + 1);
        t.request_at(2, false);
        assert!(t.asked_release());
        t.forget_release();
        assert!(!t.asked_release(), "a new session forgets an unwritten release");
    }

    /// The tail wait on a fake clock: a ring that never empties is given up on
    /// at `TAIL_MAX_MS` plus the margin and one step; an empty ring does not
    /// wait at all; a ring that empties stops the wait there (build 165).
    #[test]
    fn the_tail_wait_is_bounded_on_a_ring_that_never_empties() {
        use std::cell::Cell;
        let clock = Cell::new(1000u64);
        let pause = |ms: u64| clock.set(clock.get() + ms);
        let Tail { waited, first, .. } = wait_for_tail(|| 40, || clock.get(), pause);
        assert_eq!(first, 40);
        assert!(waited >= TAIL_MAX_MS + TAIL_MARGIN_MS && waited <= TAIL_MAX_MS + TAIL_MARGIN_MS + 5, "waited {waited}");
        let tail = wait_for_tail(|| 0, || clock.get(), |ms| clock.set(clock.get() + ms));
        assert_eq!(tail, Tail { waited: 0, first: 0, late: 0, again: false }, "nothing queued: no wait, no margin");
        let left = Cell::new(30u64);
        let Tail { waited, .. } = wait_for_tail(
            || left.get(),
            || clock.get(),
            |ms| {
                clock.set(clock.get() + ms);
                left.set(left.get().saturating_sub(ms));
            },
        );
        assert_eq!(waited, 30 + TAIL_MARGIN_MS, "until empty, then the margin");
    }

    /// Frames that land in the ring during the margin - handed over between
    /// two threads after the counts read empty - are waited for too, within
    /// the bound (build 167; review of 166).
    #[test]
    fn the_tail_looks_again_after_the_margin() {
        use std::cell::Cell;
        let clock = Cell::new(0u64);
        let left = Cell::new(40u64);
        let margins = Cell::new(0);
        let tail = wait_for_tail(
            || left.get(),
            || clock.get(),
            |ms| {
                clock.set(clock.get() + ms);
                left.set(left.get().saturating_sub(ms));
                if ms == TAIL_MARGIN_MS {
                    margins.set(margins.get() + 1);
                    if margins.get() == 1 {
                        left.set(20);
                    }
                }
            },
        );
        assert_eq!(tail, Tail { waited: 40 + TAIL_MARGIN_MS + 20 + TAIL_MARGIN_MS, first: 40, late: 20, again: true });
        // Full again after the margin and staying full: the second wait ends
        // at the same bound, counted from the same start (review of 167, K12).
        clock.set(0);
        left.set(40);
        margins.set(0);
        let full = Cell::new(false);
        let tail = wait_for_tail(
            || if full.get() { 40 } else { left.get() },
            || clock.get(),
            |ms| {
                clock.set(clock.get() + ms);
                left.set(left.get().saturating_sub(ms));
                if ms == TAIL_MARGIN_MS {
                    full.set(true);
                }
            },
        );
        assert!(tail.again);
        assert!(tail.waited <= TAIL_MAX_MS + TAIL_MARGIN_MS + 5, "waited {}", tail.waited);
        assert_eq!(after_margin(&tail), Some("waited again"));
        assert_eq!(after_margin(&Tail { waited: 285, first: 40, late: 40, again: false }), Some("past the bound, TX0 anyway"));
        assert_eq!(after_margin(&Tail { waited: 70, first: 40, late: 0, again: false }), None);
        // The line uses it, and has no words of its own (review of 169, T7).
        let code = include_str!("tx_timing.rs");
        let code = &code[..code.find("#[cfg(test)]").unwrap()];
        assert_eq!(code.matches("if let Some(what) = after_margin(&tail) {").count(), 1);
        assert_eq!(code.matches("\"past the bound, TX0 anyway\"").count(), 1);
        assert_eq!(code.matches("\"waited again\"").count(), 1);
        // A grown buffer: the second look works against the grown bound,
        // not 250 (review of 168, L1/L2).
        clock.set(0);
        left.set(440);
        margins.set(0);
        let tail = wait_for_tail(
            || left.get(),
            || clock.get(),
            |ms| {
                clock.set(clock.get() + ms);
                left.set(left.get().saturating_sub(ms));
                if ms == TAIL_MARGIN_MS {
                    margins.set(margins.get() + 1);
                    if margins.get() == 1 {
                        left.set(20);
                    }
                }
            },
        );
        assert_eq!(tail, Tail { waited: 440 + TAIL_MARGIN_MS + 20 + TAIL_MARGIN_MS, first: 440, late: 20, again: true });
        // Nothing at the first reading, then 440 ms reaching the ring: the
        // bound grows with it and nothing is cut (review of 168, a reviewer's probe).
        clock.set(0);
        left.set(0);
        let reads = Cell::new(0);
        let tail = wait_for_tail(
            || {
                reads.set(reads.get() + 1);
                if reads.get() == 2 {
                    left.set(440);
                }
                left.get()
            },
            || clock.get(),
            |ms| {
                clock.set(clock.get() + ms);
                left.set(left.get().saturating_sub(ms));
            },
        );
        assert_eq!(left.get(), 0, "nothing left when TX0 goes: {tail:?}");
        assert_eq!(tail.waited, 440 + TAIL_MARGIN_MS, "played out, and the margin after it");
    }

    /// A grown jitter buffer is played out at the release, not cut at 250 ms
    /// (build 168, owner's choice A; `veldproef-build167.md`: 440 ms ready,
    /// 160 ms cut).
    #[test]
    fn a_grown_buffer_is_played_out_at_the_release() {
        use std::cell::Cell;
        let clock = Cell::new(0u64);
        let left = Cell::new(440u64);
        let tail = wait_for_tail(
            || left.get(),
            || clock.get(),
            |ms| {
                clock.set(clock.get() + ms);
                left.set(left.get().saturating_sub(ms));
            },
        );
        assert_eq!(tail, Tail { waited: 440 + TAIL_MARGIN_MS, first: 440, late: 0, again: false });
        assert_eq!(left.get(), 0, "nothing left when TX0 goes");
    }

    /// The one production clock: a ring that never empties lets the real
    /// wait go at the bound plus the margin, on the real sleep (build 167;
    /// review of 166 - a frozen clock here would hang the release).
    #[test]
    fn the_real_tail_wait_ends_at_its_bound() {
        let started = std::time::Instant::now();
        // On its own thread, so a clock that never moves fails here instead
        // of hanging the suite.
        let (done, result) = std::sync::mpsc::channel();
        std::thread::spawn(move || done.send(wait_for_tail_now(|| 40)));
        let tail = result.recv_timeout(std::time::Duration::from_secs(2)).expect("the wait never came back");
        let wall = started.elapsed().as_millis() as u64;
        assert!(tail.waited >= TAIL_MAX_MS + TAIL_MARGIN_MS, "waited {}", tail.waited);
        assert!(wall < 600, "the wait came back after {wall} ms");
        assert_eq!(tail.late, 40, "still full after the margin");
        assert!(!tail.again, "past the bound: no second wait");
    }

    /// The four places report to it: the request for both radios, the first
    /// transmit audio in both decode tasks, the CAT write, and the radio's
    /// receive frames - a text guard, it pins that the calls are there, not
    /// that they run (build 163).
    #[test]
    fn the_transmit_chain_reports_to_its_timing() {
        let network = include_str!("../network.rs");
        let network = &sdr_remote_core::source_text::production_code(network);
        let poll = include_str!("poll.rs");
        let loops = include_str!("../audio_loops.rs");
        // Both radios through one pair since build 282: keyed by the first
        // packet with the flag, let go when its release frame plays.
        assert_eq!(network.matches("st.tx_timing.request(true);").count(), 1, "the request, both radios");
        assert_eq!(network.matches("st.tx_timing.request(false);").count(), 1, "the release, both radios");
        // Build 166: both radios run one task, which times the first audio.
        assert_eq!(include_str!("../tx_recover.rs").matches("t.tx_audio();").count(), 1, "the first audio, in the one radio task");
        assert_eq!(poll.matches("timing.written(on, crate::yaesu::tx_timing::queued_ms(items, capacity));").count(), 1);
        assert_eq!(poll.matches("let (items, capacity) = tx_ring_fill(tx_producer);").count(), 1, "the write reads the ring");
        assert_eq!(poll.matches("crate::yaesu::tx_timing::wait_for_tail_now(").count(), 1, "the tail waits through the tested function and its clock");
        // Since build 282 the release frame comes out of the jitter buffer
        // behind the last word: only the ring is left to wait for.
        assert!(poll.contains("let tail = crate::yaesu::tx_timing::wait_for_tail_now(|| {\n                        let (items, capacity) = tx_ring_fill_now(tx_producer);\n                        crate::yaesu::tx_timing::queued_ms(items, capacity)\n                    });"), "the tail waits for the ring");
        // Build 165: the loop does not wait on the port for a command, and a
        // new session forgets an unwritten release.
        assert_eq!(poll.matches("if port.bytes_to_read().map_or(true, |n| n > 0) {").count(), 1);
        assert_eq!(poll.matches("timing_of(status).forget_release();").count(), 1);
        assert_eq!(loops.matches("rx_back.rx_block(&frame, buf.samples.len());").count(), 1);
        assert!(
            loops.contains("RadioTick::Idle => {\n                        rx_back.cancel();"),
            "the radio cancels its turnaround wait when nobody listens"
        );
        assert!(network.contains("let timing = yaesu2.as_ref().map(|y| y.status_arc().lock().unwrap().tx_timing.clone());"));
        // Build 164: the bridge holds until keyed, and only an operator's
        // release waits for the tail.
        let module = include_str!("mod.rs");
        assert_eq!(module.matches("let taken = hold.take(samples, timing.keyed(), timing.over(), hold_max);").count(), 1);
        // And what it threw away reaches the log (build 184).
        assert_eq!(module.matches("if let Some(n) = hold.take_report() {").count(), 1);
        // And what the bridge could not write, every block (build 185).
        assert_eq!(module.matches("if let Some(n) = unwritten.add(timing.over(), samples.len() - written) {").count(), 1);
        // And a push that worked counts as written: without it every
        // transmission would report its whole length (review of 185, V3).
        assert_eq!(module.matches("if prod.try_push(s).is_ok() {
                                        written += 1;
                                    }").count(), 1);
        assert_eq!(module.matches("tx_timing::HOLD_MAX_MS / 1000").count(), 1, "the bound is the constant");
        assert_eq!(poll.matches("if ptt_cmd == Some(false) && timing_of(status).asked_release() {").count(), 1);
    }
}
