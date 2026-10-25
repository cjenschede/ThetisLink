// SPDX-License-Identifier: GPL-2.0-or-later

//! How much of what we asked for actually arrived.
//!
//! **Loss is a property of the link, not of a stream.** Every stream to one
//! client travels the same socket to the same address, so any stream that is
//! actually flowing measures the path, and a client needs exactly one figure.
//! The server agrees: eight places read it - four audio error-correction
//! decisions (the Thetis bundle, both radios, both VRX channels) and four
//! spectrum gates.
//!
//! One figure serving eight consumers was never the defect. The defect was that
//! the figure was computed from a stream that had stopped:
//!
//! - only the Thetis audio ever fed it, and the two Yaesu branches marked the
//!   connection alive without touching it;
//! - the Thetis data saving then stopped that stream, and the meter read the
//!   radio link as a hundred percent (build 33) and afterwards as zero
//!   (builds 34-36). Neither measured it;
//! - a VRX-only client had nothing feeding it at all, while its figure gated
//!   its own VRX audio protection and its VRX spectrum.
//!
//! So the rule is one sentence: **every audio packet the client receives feeds
//! the meter.** Five streams do, not because they measure five different
//! things, but because each carries its own sequence numbers and you cannot
//! compute loss on a stream you are not receiving - the per-stream structure
//! exists to give the sum a valid denominator.
//!
//! **And the parts are pooled, not maximised.** Taking the worst of five
//! samples of one quantity is biased upward, and the server latches at two
//! percent - so a link genuinely losing two would report more. Summing what was
//! expected against what arrived is unbiased, simpler to explain, and it
//! separates two cases the old single window could not tell apart: one stream
//! silent while the others flow is not the link (the server stopped sending),
//! whereas every stream silent at once is.
//!
//! The owner asked the question that produced this - "all the audio goes down
//! one connection, so does it matter which channel you measure?" - after two
//! rounds of review had read the per-stream design without anyone asking it. It
//! did not matter. The aggregation did.
//!
//! The second job of this module is the false peak. A reconnect kept the
//! previous session's sequence bookkeeping, so the first windows of the new
//! session were charged for the gap the reconnect itself caused - seventy
//! percent two seconds after connecting, and error correction that then had no
//! reason to release. That is [`LossMeter::reset`], and it is open point 15.

/// One stream's rolling window: fed per packet, closed per heartbeat.
///
/// It reports a COUNT, not a percentage. The percentage is the meter's job,
/// because percentages cannot be added up.
#[derive(Debug, Default, Clone)]
pub struct StreamLoss {
    received: u32,
    max_seq: Option<u32>,
    prev_max_seq: Option<u32>,
    /// How big the last window with traffic was, so a silent window can say how
    /// much is missing rather than only that something is.
    last_expected: u32,
    /// Has this stream ever carried traffic since the last reset?
    ran: bool,
    /// Consecutive silent windows while wanted. Only used to give a stream that
    /// has just been asked back one window to arrive in.
    silent_windows: u32,
    /// Which sequence numbers have already been counted this window.
    ///
    /// Without this the meter counted ARRIVALS where the ratio needs DISTINCT
    /// arrivals, and it is fed before the jitter buffer, so a duplicate could
    /// stand in for a packet that never came: 25 expected, 20 distinct, 5
    /// duplicates reads as a clean link while a fifth is missing (review
    /// 2120A4). Both the relay and plain UDP can duplicate.
    ///
    /// A fixed bitmap, not a set: no allocation, no growth, a handful of integer
    /// operations per packet against an Opus decode of microseconds on the same
    /// path. (I first called this a latency trade-off when the owner asked; it
    /// is not one, and saying so without weighing it was the mistake.)
    seen: [u64; SEEN_WORDS],
    /// The sequence the bitmap's bit 0 stands for - the first one this window.
    seen_base: Option<u32>,
}

/// 256 sequence numbers of duplicate protection per window: four `u64`s, 32
/// bytes a stream. A window is 500 ms and audio arrives in 20 ms frames, so
/// twenty-five is the ordinary count and 256 is ten times the headroom.
/// A sequence outside that span is counted as it always was - the bitmap says
/// nothing about it, and guessing would be worse than the duplicate.
const SEEN_WORDS: usize = 4;

/// A stream we have just asked for again gets this many windows to arrive
/// before its silence counts as loss. One: the resume has to reach the server
/// and the first packet has to come back, and charging for that round trip is
/// how the false peak was built in the first place.
const RESUME_GRACE_WINDOWS: u32 = 1;

/// Weight given to the newest window when smoothing the reported figure.
///
/// Named rather than written into the expression, because it is the number that
/// decides how long the server's error correction STAYS on: the release counter
/// only starts once the client reports a clean zero, and how many clean windows
/// that takes follows from this constant alone. A reviewer showed that pinning
/// the window COUNT is not enough - a band wide enough to be stable is also wide
/// enough to absorb a move from 0.3 to 0.25 without going red.
pub const EMA_NEW: f32 = 0.3;

/// After this many windows in which NO stream had anything to say, the reported
/// figure is no longer a measurement of anything current - it is simply the last
/// one that was taken.
///
/// Forty windows is twenty seconds at the engine's 500 ms heartbeat. Chosen long
/// enough that the ordinary silences do not trip it (a radio change, the
/// resume grace, a few windows between subscriptions) and short enough that it
/// still says something while the operator is at the set.
///
/// **Not related to [`crate::thetis_streams::THETIS_STREAM_GRACE`], which is
/// also twenty seconds.** That one is how long the Thetis streams keep running
/// after a switch, and the owner asked for it; this one is how long a figure may
/// go unrefreshed before the log says so, and it was chosen here. They arrived
/// at the same number from different directions, and a reader who merges them
/// into one constant will make one of the two wrong the next time either moves.
///
/// The figure is deliberately NOT decayed towards zero when this happens:
/// silence is not evidence of a clean link, and "back to zero on silence" is the
/// exact mistake that made a phone in data-saving mode report a perfect link.
/// So the figure stands and the log says that it stands.
pub const STALE_AFTER_WINDOWS: u32 = 40;

/// What the running figure keeps of itself each window.
pub const EMA_OLD: f32 = 1.0 - EMA_NEW;

impl StreamLoss {
    pub fn new() -> Self {
        Self::default()
    }

    /// A packet arrived on this stream. A second copy of one already counted
    /// this window is not a second packet.
    pub fn packet(&mut self, sequence: u32) {
        if self.already_counted(sequence) {
            return;
        }
        self.received += 1;
        self.max_seq = Some(self.max_seq.map_or(sequence, |m| m.max(sequence)));
    }

    /// Mark this sequence as counted, and say whether it already was.
    ///
    /// `wrapping_sub` because sequences wrap, and a span of 256 either side of
    /// the base is the only thing this can speak about. Outside it: `false`,
    /// which counts the packet exactly as before.
    fn already_counted(&mut self, sequence: u32) -> bool {
        let base = *self.seen_base.get_or_insert(sequence);
        let offset = sequence.wrapping_sub(base) as usize;
        if offset >= SEEN_WORDS * 64 {
            return false;
        }
        let bit = 1u64 << (offset % 64);
        let word = &mut self.seen[offset / 64];
        let was = *word & bit != 0;
        *word |= bit;
        was
    }

    /// A new window starts with no memory of the last one's sequences.
    fn forget_seen(&mut self) {
        self.seen = [0; SEEN_WORDS];
        self.seen_base = None;
    }

    /// Forget everything, including what happened before.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Close the window: how many packets were expected, and how many arrived.
    ///
    /// `expected` is whether the client currently wants this stream at all.
    /// `None` means the stream has nothing to contribute - it never ran, or it
    /// stopped because we asked it to - and a stream with nothing to say must
    /// count neither as clean nor as lost.
    pub fn step(&mut self, expected: bool) -> Option<(u32, u32)> {
        let received = std::mem::take(&mut self.received);
        // The window is over, so its sequence numbers are too. Before any early
        // return below: a window that ends in silence still has to forget, or
        // the next one starts with a base it will never see again.
        self.forget_seen();
        if let Some(max) = self.max_seq.take() {
            // Sequence numbers say how many SHOULD have arrived; the count says
            // how many did. On the first window there is no previous mark, so
            // the count is all there is and the answer is zero by construction.
            let expected_n = match self.prev_max_seq {
                // A stream that starts over - the server resets the sequence
                // when the last subscriber leaves and comes back - would
                // otherwise wrap into a gap of four billion and read as total
                // loss. At fifty packets a second a real wrap is years away, so
                // a number that went backwards is a restart, not a wrap.
                Some(prev) if max < prev => received,
                Some(prev) => max.wrapping_sub(prev),
                None => received,
            };
            self.prev_max_seq = Some(max);
            self.ran = true;
            self.silent_windows = 0;
            if expected_n > 0 {
                self.last_expected = expected_n;
            }
            // More arriving than expected means the sequence went somewhere this
            // does not model. Report clean rather than negative loss.
            let received = received.min(expected_n);
            return Some((expected_n, received));
        }

        // Nothing arrived this window.
        if !self.ran {
            return None; // never carried anything: silent.
        }
        if !expected {
            // We asked it to stop - the data saving, or an audio subscription
            // switched off. Absence is not loss. Wind down the sequence mark,
            // but remember that it ran: asking it back and getting nothing is
            // still a fact about the link.
            self.prev_max_seq = None;
            self.silent_windows = 0;
            return None;
        }
        self.silent_windows += 1;
        if self.prev_max_seq.is_none() && self.silent_windows <= RESUME_GRACE_WINDOWS {
            // Just asked back. Give it a window to arrive before blaming it.
            return None;
        }
        // It was running, or we have waited long enough after asking it back,
        // and nothing came. Everything expected is missing.
        Some((self.last_expected.max(1), 0))
    }
}

/// Which stream contributed the most missing packets this window.
///
/// Named because the figure is one number that five streams feed, and a number
/// without a source is what cost an evening on 2026-09-13 - the server's line
/// said the percentage without saying whose link it was, and this would be the
/// same mistake one layer down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Thetis,
    Radio1,
    Radio2,
    Vrx1,
    Vrx2,
}

impl Stream {
    pub fn name(self) -> &'static str {
        match self {
            Stream::Thetis => "Thetis",
            Stream::Radio1 => "radio 1",
            Stream::Radio2 => "radio 2",
            Stream::Vrx1 => "VRX1",
            Stream::Vrx2 => "VRX2",
        }
    }
}

/// Which streams the client is expecting this window.
///
/// Built by [`Expecting::from_client`] and never by hand at the call site: a
/// literal lets a field be nailed to `false` with every test still green, which
/// is the defect of builds 33 to 36 in a form only a reviewer can catch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expecting {
    pub thetis: bool,
    pub radio1: bool,
    pub radio2: bool,
    pub vrx1: bool,
    pub vrx2: bool,
}

impl Expecting {
    /// What the client is subscribed to, from its own switches.
    ///
    /// * `audio_mode` - the client's `AudioMode` control (255 = the Android
    ///   default, RX1 only).
    /// * `rx1_wish` - the operator's RX1 wish AFTER the Thetis data saving has
    ///   had its say, which is why it is passed in rather than derived here.
    pub fn from_client(
        audio_mode: u8,
        rx1_wish: bool,
        rx2: bool,
        radio1: bool,
        radio2: bool,
        vrx1: bool,
        vrx2: bool,
    ) -> Self {
        Self {
            thetis: sdr_remote_core::audio_channels::receives_any_channel(audio_mode, rx1_wish, rx2),
            radio1,
            radio2,
            vrx1,
            vrx2,
        }
    }
}

/// The five streams whose arrival says something about this client's link.
#[derive(Debug, Default, Clone)]
pub struct LossMeter {
    pub thetis: StreamLoss,
    pub radio1: StreamLoss,
    pub radio2: StreamLoss,
    pub vrx1: StreamLoss,
    pub vrx2: StreamLoss,
    smoothed: f32,
    reported: u8,
    worst: Option<Stream>,
    stale_windows: u32,
    ever_measured: bool,
    /// A stream the client still wants that handed over nothing this window,
    /// while others did. Kept out of the link figure and reported on its own -
    /// it is a stopped stream, not a lossy link.
    stopped: Option<Stream>,
}

impl LossMeter {
    pub fn new() -> Self {
        Self::default()
    }

    /// A new session. Everything before this belongs to the previous one.
    pub fn reset(&mut self) {
        self.thetis.reset();
        self.radio1.reset();
        self.radio2.reset();
        self.vrx1.reset();
        self.vrx2.reset();
        self.smoothed = 0.0;
        self.reported = 0;
        self.worst = None;
        self.stale_windows = 0;
        self.ever_measured = false;
        self.stopped = None;
    }

    /// Close every window and return the figure to report.
    ///
    /// Pooled: what was expected across the streams that had something to say,
    /// against what arrived. A window in which no stream spoke leaves the
    /// reported figure where it stands rather than declaring the link clean -
    /// silence is not evidence.
    pub fn step(&mut self, expecting: Expecting) -> u8 {
        let per_stream = [
            (Stream::Thetis, self.thetis.step(expecting.thetis)),
            (Stream::Radio1, self.radio1.step(expecting.radio1)),
            (Stream::Radio2, self.radio2.step(expecting.radio2)),
            (Stream::Vrx1, self.vrx1.step(expecting.vrx1)),
            (Stream::Vrx2, self.vrx2.step(expecting.vrx2)),
        ];
        // A stream that delivered NOTHING while another delivered something is
        // the case this module's own header promises to separate: "one stream
        // silent while the others flow is not the link (the server stopped
        // sending), whereas every stream silent at once is". Until build 161
        // the header said it and the code did not do it - a wanted stream that
        // stopped handed its whole last window in as missing, so a Yaesu losing
        // its USB audio, or Thetis stopping, read as link loss. One of five is
        // already tens of percent, and that figure drives four error-correction
        // decisions, four spectrum gates and the fault report the operator
        // reads (review 2120A4).
        //
        // It cannot be told apart from the link really dropping that one
        // stream - not from here - and that is exactly why it must not be
        // allowed to speak for the link while the other streams are arriving.
        let anything_arrived = per_stream
            .iter()
            .any(|(_, w)| matches!(w, Some((_, received)) if *received > 0));
        let mut total_expected = 0u64;
        let mut total_received = 0u64;
        let mut worst: Option<(Stream, u32)> = None;
        let mut stopped: Option<Stream> = None;
        for (stream, window) in per_stream {
            let Some((expected, received)) = window else { continue };
            if received == 0 && anything_arrived {
                // Its own fact, not the link's: remembered so a log line can
                // name it, and kept out of the sum.
                stopped = stopped.or(Some(stream));
                continue;
            }
            total_expected += expected as u64;
            total_received += received as u64;
            let missing = expected - received;
            // Strictly greater, so a tie keeps the EARLIER stream in this list.
            // Arbitrary between two equal contributors, but pinned: a silent
            // change would make a log line move between streams for no reason
            // anyone could explain afterwards.
            if missing > 0 && worst.map_or(true, |(_, m)| missing > m) {
                worst = Some((stream, missing));
            }
        }
        self.stopped = stopped;
        if total_expected == 0 {
            // Nobody spoke. The figure stands, but it stops being current, and
            // how long that has lasted is the thing nothing used to say.
            self.stale_windows = self.stale_windows.saturating_add(1);
        } else {
            self.stale_windows = 0;
            self.ever_measured = true;
        }
        if total_expected > 0 {
            let raw = (100 * (total_expected - total_received) / total_expected) as u8;
            // EMA, unchanged: a slow rise and fall keeps the figure from jumping
            // about, and the server's hysteresis is tuned to it.
            self.smoothed = self.smoothed * EMA_OLD + raw as f32 * EMA_NEW;
            self.reported = self.smoothed.round() as u8;
            if let Some((stream, _)) = worst {
                self.worst = Some(stream);
            }
        }
        self.reported
    }

    /// A stream the client asked for that delivered nothing this window while
    /// the others did. That is a stream that stopped - the server's side, a
    /// radio's USB audio gone, Thetis closed - and NOT a measurement of the
    /// link, so it is reported here instead of being folded into the figure.
    pub fn stopped(&self) -> Option<Stream> {
        self.stopped
    }

    /// Which stream last contributed the most missing packets, so a log line can
    /// say where the figure came from instead of only how big it is.
    pub fn worst(&self) -> Option<Stream> {
        self.worst
    }

    /// Has anything at all been measured since the last reset?
    ///
    /// Without this the stale line says "the 0% it reports is the last real
    /// measurement" on a session that has never measured anything - a client
    /// that connects and subscribes to nothing, say. Zero is then a starting
    /// value, not a finding, and calling it a measurement is the same class of
    /// mistake as reading silence as a clean link. A reviewer caught it.
    pub fn has_measured(&self) -> bool {
        self.ever_measured
    }

    /// How many windows in a row nothing has been measured.
    ///
    /// Zero means the last window had traffic to judge. Anything above
    /// [`STALE_AFTER_WINDOWS`] means the reported figure is the last real
    /// measurement and not a statement about now - which matters, because eight
    /// places in the server read that figure and four of them decide whether a
    /// spectrum is still sent.
    pub fn stale_windows(&self) -> u32 {
        self.stale_windows
    }

    /// What was last reported, without closing a window.
    pub fn reported(&self) -> u8 {
        self.reported
    }

    /// The unrounded running figure. For tests only: the decay that decides how
    /// long the server keeps error correction on happens on THIS value, and a
    /// test that starts from the rounded one is answering a different question.
    #[cfg(test)]
    pub(crate) fn smoothed(&self) -> f32 {
        self.smoothed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: Expecting = Expecting {
        thetis: true,
        radio1: true,
        radio2: true,
        vrx1: true,
        vrx2: true,
    };
    const THETIS_AND_RADIO1: Expecting = Expecting {
        thetis: true,
        radio1: true,
        radio2: false,
        vrx1: false,
        vrx2: false,
    };
    const YAESU_ONLY: Expecting = Expecting {
        thetis: false,
        radio1: true,
        radio2: false,
        vrx1: false,
        vrx2: false,
    };
    const VRX_ONLY: Expecting = Expecting {
        thetis: false,
        radio1: false,
        radio2: false,
        vrx1: true,
        vrx2: false,
    };
    const NOTHING: Expecting = Expecting {
        thetis: false,
        radio1: false,
        radio2: false,
        vrx1: false,
        vrx2: false,
    };

    /// Above this the server switches error correction on. Stated here
    /// deliberately - it lives in the server crate, which this one does not
    /// depend on - so a test can say what a number is FOR.
    const LATCH_AT: u8 = 2;

    /// Feed `n` consecutive packets starting at `from`.
    fn run(s: &mut StreamLoss, from: u32, n: u32) {
        for i in 0..n {
            s.packet(from + i);
        }
    }

    /// Feed `n` packets, dropping every second one, starting at `from`.
    fn run_half(s: &mut StreamLoss, from: u32, n: u32) {
        for i in 0..n {
            s.packet(from + i * 2);
        }
    }

    /// The case the test review wrote out in review 2120A4: 25 expected, 20 distinct, 5
    /// duplicates. It read as a clean link while a fifth of the window was
    /// missing, because the meter counted arrivals and is fed before the jitter
    /// buffer. Both the relay and plain UDP can duplicate.
    #[test]
    fn a_duplicate_does_not_stand_in_for_a_packet_that_never_came() {
        let mut s = StreamLoss::new();
        run(&mut s, 0, 1);
        s.step(true); // a mark to measure the next window against

        // Sequence 25 is the highest, so 25 were expected. Twenty distinct
        // arrive - 1 to 19 and 25 - and five of them arrive a second time, so
        // twenty-five packets reach the meter.
        for i in 1..=19u32 {
            s.packet(i);
        }
        s.packet(25);
        for i in 1..=5u32 {
            s.packet(i); // the same five again
        }
        let (expected, received) = s.step(true).expect("the window carried traffic");
        assert_eq!(expected, 25, "sequence 1 to 25 were expected");
        assert_eq!(received, 20, "and twenty distinct ones arrived - not twenty-five");
    }

    /// And the ordinary window is unchanged: no duplicates, nothing lost.
    #[test]
    fn a_clean_window_is_still_clean() {
        let mut s = StreamLoss::new();
        run(&mut s, 0, 1);
        s.step(true);
        run(&mut s, 1, 25);
        assert_eq!(s.step(true), Some((25, 25)));
    }

    /// The memory is per window, not per stream. A sequence seen in the
    /// previous window and again in this one is a packet of this window - a
    /// memory that outlived its window would drop it.
    #[test]
    fn the_duplicate_memory_does_not_outlive_its_window() {
        let mut s = StreamLoss::new();
        run(&mut s, 1, 10); // 1..=10
        s.step(true);
        // A late copy of 5 from the window before, and one new packet.
        s.packet(5);
        s.packet(20);
        let (_, received) = s.step(true).expect("traffic");
        assert_eq!(received, 2, "both counted: the last window's numbers are not this window's duplicates");
    }

    // ---- one stream ----

    #[test]
    fn a_clean_stream_expects_what_it_got() {
        let mut s = StreamLoss::new();
        run(&mut s, 0, 50);
        assert_eq!(s.step(true), Some((50, 50))); // first window: nothing to compare to
        run(&mut s, 50, 50);
        assert_eq!(s.step(true), Some((50, 50)));
    }

    #[test]
    fn half_the_packets_missing_is_counted_not_averaged() {
        let mut s = StreamLoss::new();
        run(&mut s, 0, 10);
        s.step(true);
        // The previous window ended at seq 9; every other packet arrives up to
        // 109. A hundred expected, fifty received.
        run_half(&mut s, 11, 50);
        assert_eq!(s.step(true), Some((100, 50)));
    }

    #[test]
    fn a_stream_that_starts_over_is_not_read_as_total_loss() {
        // The server resets the sequence when the last subscriber leaves and
        // returns. Subtracting the old mark from the new one wraps to four
        // billion, and a perfectly good link would read as entirely lost.
        let mut s = StreamLoss::new();
        run(&mut s, 5000, 50);
        s.step(true);
        run(&mut s, 0, 50);
        assert_eq!(s.step(true), Some((50, 50)));
        run(&mut s, 50, 50);
        assert_eq!(s.step(true), Some((50, 50)));
    }

    #[test]
    fn a_running_stream_that_falls_silent_loses_a_whole_window() {
        let mut s = StreamLoss::new();
        run(&mut s, 0, 10);
        s.step(true);
        run(&mut s, 10, 40);
        assert_eq!(s.step(true), Some((40, 40)));
        // Now nothing arrives, and it is still wanted.
        assert_eq!(s.step(true), Some((40, 0)));
    }

    #[test]
    fn a_stream_we_asked_to_stop_goes_quiet_instead_of_lost() {
        let mut s = StreamLoss::new();
        run(&mut s, 0, 10);
        s.step(true);
        assert_eq!(s.step(false), None);
        assert_eq!(s.step(false), None);
    }

    /// The blind spot a reviewer found, which my own test had pinned as correct.
    ///
    /// After a wind-down the stream said nothing forever - including when we
    /// asked for it again and it did not come back. That is a resume which never
    /// left the socket, seen twice this week, and it read as a link with no
    /// opinion instead of a link with a problem.
    #[test]
    fn a_stream_asked_back_that_does_not_return_is_lost_after_one_window() {
        let mut s = StreamLoss::new();
        run(&mut s, 0, 10);
        s.step(true);
        run(&mut s, 10, 40);
        s.step(true);
        assert_eq!(s.step(false), None, "wound down on request");
        // Asked back. One window of grace for the round trip...
        assert_eq!(s.step(true), None, "the resume needs a window to arrive in");
        // ...and after that, silence is a fact about the link.
        assert_eq!(s.step(true), Some((40, 0)));
        assert_eq!(s.step(true), Some((40, 0)));
    }

    /// More arriving than the sequence predicted.
    ///
    /// Duplicates and reordering both do this. Ungated until a reviewer worked
    /// through the mutations: `received.min(expected_n)` had no test on its own
    /// reason, and without it the meter subtracts a bigger number from a smaller
    /// one - which on a u32 is not a small error but a panic in a debug build.
    #[test]
    fn more_arriving_than_predicted_reads_as_clean_and_never_as_negative() {
        let mut s = StreamLoss::new();
        run(&mut s, 0, 10); // window ends at seq 9
        s.step(true);
        // Three new sequence numbers, ten packets: seven are duplicates.
        s.packet(10);
        s.packet(11);
        s.packet(12);
        for _ in 0..7 {
            s.packet(10);
        }
        assert_eq!(s.step(true), Some((3, 3)), "received is capped at what was expected");

        // And the meter must survive it rather than underflow.
        let mut m = LossMeter::new();
        run(&mut m.thetis, 0, 10);
        m.step(ALL);
        for _ in 0..6 {
            m.thetis.packet(10);
        }
        assert_eq!(m.step(ALL), 0);
    }

    /// A window in which the sequence did not advance at all.
    ///
    /// One duplicate of the previous window's last packet and nothing else:
    /// expected is zero, and `last_expected` must keep the real window size. The
    /// guard around it was ungated - drop it and a stream that later falls silent
    /// weighs ONE packet in the sum instead of forty-one, which quietly erases it
    /// from a figure that is now an addition.
    #[test]
    fn a_window_that_did_not_advance_does_not_forget_how_big_a_window_is() {
        let mut s = StreamLoss::new();
        run(&mut s, 0, 41); // window ends at seq 40, forty-one expected
        s.step(true);
        s.packet(40); // a duplicate: the sequence does not move
        assert_eq!(s.step(true), Some((0, 0)));
        assert_eq!(
            s.step(true),
            Some((41, 0)),
            "the last real window size must survive a window that did not advance"
        );
    }

    /// Two streams missing exactly as much as each other.
    ///
    /// Pinned deliberately: with `>` the earlier stream in the list keeps the
    /// name, with `>=` the later one takes it. Nothing depends on which, and that
    /// is the reason to fix it - a log line that moves between streams without a
    /// cause is a log line nobody can reason from.
    #[test]
    fn a_tie_keeps_the_earlier_stream() {
        let mut m = LossMeter::new();
        run(&mut m.thetis, 0, 21);
        run(&mut m.radio1, 0, 21);
        m.step(ALL);
        run_half(&mut m.thetis, 21, 10); // 20 expected, 10 arrive
        run_half(&mut m.radio1, 21, 10); // identical
        m.step(ALL);
        assert_eq!(m.worst(), Some(Stream::Thetis), "a tie keeps the earlier stream");
    }

    /// The header of this module promises to tell two cases apart: one stream
    /// silent while the others flow is the SERVER having stopped it, and every
    /// stream silent at once is the link. Until build 161 the code did not make
    /// that distinction - a wanted stream that stopped handed its whole last
    /// window in as missing (review 2120A4).
    #[test]
    fn a_stream_that_stops_while_the_others_flow_is_not_the_link() {
        let mut m = LossMeter::new();
        run(&mut m.thetis, 0, 21);
        run(&mut m.radio1, 0, 21);
        m.step(ALL);
        // Thetis keeps arriving perfectly; the radio's USB audio disappears
        // while the client still wants it.
        run(&mut m.thetis, 21, 21);
        m.step(ALL);
        assert_eq!(m.reported(), 0, "the link carried everything that reached it");
        assert_eq!(m.stopped(), Some(Stream::Radio1), "and the stream that fell silent is named");
    }

    #[test]
    fn a_stream_that_never_ran_says_nothing() {
        let mut s = StreamLoss::new();
        assert_eq!(s.step(true), None);
        assert_eq!(s.step(false), None);
        assert_eq!(s.step(true), None);
    }

    // ---- the meter ----

    #[test]
    fn the_radio_link_is_measured_when_the_thetis_streams_are_stopped() {
        // The defect of builds 34 to 36. Phone on a Yaesu with the data saving
        // active: the Thetis window is silent by request and the radio window is
        // the only one with anything to say. It used to report zero, and the
        // radio audio - the only thing the operator could hear - got no
        // protection at all.
        let mut m = LossMeter::new();
        run(&mut m.thetis, 0, 10);
        run(&mut m.radio1, 0, 10);
        m.step(ALL);
        run_half(&mut m.radio1, 11, 50);
        let reported = m.step(YAESU_ONLY);
        assert!(reported > 0, "the radio link must still be measured, got {reported}");
    }

    #[test]
    fn a_vrx_only_client_is_measured_too() {
        // The reviewer's finding. A client with the Thetis audio off and no
        // Yaesu had NOTHING feeding the meter, while that same figure gated its
        // VRX audio protection and its VRX spectrum.
        let mut m = LossMeter::new();
        run(&mut m.vrx1, 0, 10);
        m.step(VRX_ONLY);
        run_half(&mut m.vrx1, 11, 50);
        let reported = m.step(VRX_ONLY);
        assert!(reported > 0, "a VRX-only client must measure its own link, got {reported}");
        assert_eq!(m.worst(), Some(Stream::Vrx1));
    }

    #[test]
    fn a_stopped_thetis_stream_no_longer_charges_a_hundred_percent() {
        // The defect of build 33, the other half of the same pair.
        let mut m = LossMeter::new();
        run(&mut m.thetis, 0, 10);
        run(&mut m.radio1, 0, 10);
        m.step(ALL);
        run(&mut m.radio1, 10, 10);
        assert_eq!(m.step(YAESU_ONLY), 0, "a clean radio link with Thetis stopped is clean");
    }

    /// The owner's question, as an assertion.
    ///
    /// Every stream shares one socket to one address, so three streams each
    /// losing one percent is a link losing one percent - not a link losing the
    /// worst of three noisy samples of one percent. Taking the maximum biased
    /// the figure upward, and the server latches at two.
    #[test]
    fn streams_are_pooled_and_not_maximised() {
        // The two only part company when the streams are UNEQUAL - three equal
        // streams give the same answer either way, which is how a test that
        // cannot fail gets written. A big clean stream beside a small damaged
        // one is the case that separates them:
        //   pooled: 5 missing out of 110 = 4%
        //   worst:  the small stream alone = 50%
        let mut m = LossMeter::new();
        run(&mut m.thetis, 0, 100);
        run(&mut m.radio1, 0, 10);
        m.step(ALL);
        run(&mut m.thetis, 100, 100); // clean, 100 packets
        run_half(&mut m.radio1, 11, 5); // 10 expected, 5 arrive
        let pooled = m.step(ALL);
        assert!(
            pooled <= 3,
            "a link carrying 105 of 110 packets is not losing fifty percent, got {pooled}"
        );
        // And the EMA of the worst-of would have landed far above this.
        assert!(pooled * 4 < 50, "still distinguishable from taking the worst");
    }

    #[test]
    fn one_silent_stream_beside_healthy_ones_is_not_a_broken_link() {
        // A distinction the single window could not make. Radio 1 stops while
        // the Thetis bundle keeps flowing perfectly: that is the server no
        // longer sending, not the path dropping packets. Pooling says so;
        // taking the worst would have called it a hundred percent.
        let mut m = LossMeter::new();
        run(&mut m.thetis, 0, 100);
        run(&mut m.radio1, 0, 10);
        m.step(THETIS_AND_RADIO1);
        run(&mut m.thetis, 100, 100);
        let reported = m.step(THETIS_AND_RADIO1);
        assert!(
            reported < 20,
            "one stopped stream beside a healthy one must not read as a broken link, got {reported}"
        );
    }

    #[test]
    fn every_stream_silent_at_once_is_a_broken_link() {
        let mut m = LossMeter::new();
        run(&mut m.thetis, 0, 100);
        run(&mut m.radio1, 0, 100);
        m.step(THETIS_AND_RADIO1);
        run(&mut m.thetis, 100, 100);
        run(&mut m.radio1, 100, 100);
        m.step(THETIS_AND_RADIO1);
        let mut last = 0;
        for _ in 0..4 {
            last = m.step(THETIS_AND_RADIO1);
        }
        assert!(last >= 60, "a link with nothing arriving must read as lost, got {last}");
        assert_eq!(
            m.stopped(),
            None,
            "and with nothing arriving anywhere, no stream is excused as one that stopped -              that excuse only applies while the others are still flowing"
        );
    }

    #[test]
    fn the_meter_names_the_stream_that_lost_the_most() {
        let mut m = LossMeter::new();
        run(&mut m.thetis, 0, 10);
        run(&mut m.radio1, 0, 10);
        m.step(ALL);
        run(&mut m.thetis, 10, 10); // clean
        run_half(&mut m.radio1, 11, 50); // fifty missing
        m.step(ALL);
        assert_eq!(m.worst(), Some(Stream::Radio1));
        assert_eq!(Stream::Radio1.name(), "radio 1");
        assert_eq!(Stream::Vrx2.name(), "VRX2");
    }

    #[test]
    fn a_window_in_which_nothing_spoke_holds_the_last_figure() {
        // Silence is not evidence of a clean link. If it read as zero, a client
        // with every stream switched off would tell the server the link is
        // perfect - and the server would let go of protection other listeners
        // may still need.
        let mut m = LossMeter::new();
        run(&mut m.thetis, 0, 10);
        m.step(ALL);
        run_half(&mut m.thetis, 11, 50);
        let after_loss = m.step(ALL);
        assert!(after_loss > 0);
        m.step(NOTHING);
        assert_eq!(m.step(NOTHING), after_loss, "a silent window must not be read as a clean one");
    }

    // ---- open point 18: a figure that stops being refreshed ----

    #[test]
    fn a_measured_window_is_never_stale() {
        let mut m = LossMeter::new();
        run(&mut m.thetis, 0, 50);
        m.step(ALL);
        assert_eq!(m.stale_windows(), 0);
        run(&mut m.thetis, 50, 50);
        m.step(ALL);
        assert_eq!(m.stale_windows(), 0);
    }

    /// The sequence a reviewer built out of six ordinary actions.
    ///
    /// Thetis only, the link drops to forty percent, the operator moves to a
    /// Yaesu, the grace runs out, the radio has not arrived yet, VRX is off. All
    /// five streams are then silent and the reported figure stands at forty -
    /// for minutes or hours, read by eight places in the server, four of which
    /// decide whether a spectrum is still sent. Nothing said so.
    ///
    /// **It works because radio 1 has never run**, and that condition is the
    /// whole point rather than a detail of the setup. A stream that HAS run and
    /// then goes silent while still wanted is charged a full window (100%), so
    /// `total_expected` is not zero and nothing goes stale. Read without this,
    /// the test says "when everything is silent it goes stale", and that is not
    /// true: it goes stale when everything is silent AND nothing is owed.
    ///
    /// The practical consequence, which a reviewer drew out: in the field this
    /// is visible on the FIRST excursion to a radio and not on the second.
    #[test]
    fn a_figure_nobody_refreshes_is_counted_and_eventually_called_stale() {
        let mut m = LossMeter::new();
        run(&mut m.thetis, 0, 100);
        m.step(THETIS_AND_RADIO1);
        run_half(&mut m.thetis, 101, 50);
        let measured = m.step(THETIS_AND_RADIO1);
        assert!(measured > 0);

        // The operator switches away: from here nothing has anything to say.
        for w in 1..=STALE_AFTER_WINDOWS {
            assert_eq!(m.step(NOTHING), measured, "the figure itself must not move");
            assert_eq!(m.stale_windows(), w, "every silent window counts");
        }
        assert!(m.stale_windows() >= STALE_AFTER_WINDOWS, "and now it is stale");

        // And it keeps counting rather than wrapping or resetting itself.
        m.step(NOTHING);
        assert_eq!(m.stale_windows(), STALE_AFTER_WINDOWS + 1);
    }

    #[test]
    fn a_session_that_never_measured_anything_says_so() {
        // A client that connects and subscribes to nothing. The figure is zero
        // because it started there, not because the link is clean, and the log
        // line must not call that a measurement.
        let mut m = LossMeter::new();
        for _ in 0..STALE_AFTER_WINDOWS + 5 {
            m.step(NOTHING);
        }
        assert!(m.stale_windows() > STALE_AFTER_WINDOWS);
        assert!(!m.has_measured(), "nothing was ever measured in this session");
        assert_eq!(m.reported(), 0);

        // One real window and the distinction flips for good.
        run(&mut m.thetis, 0, 50);
        m.step(ALL);
        assert!(m.has_measured());
    }

    #[test]
    fn a_new_session_has_measured_nothing_again() {
        let mut m = LossMeter::new();
        run(&mut m.thetis, 0, 50);
        m.step(ALL);
        assert!(m.has_measured());
        m.reset();
        assert!(!m.has_measured(), "the previous session's measurement is not this one's");
    }

    #[test]
    fn one_measured_window_makes_it_current_again() {
        let mut m = LossMeter::new();
        run(&mut m.thetis, 0, 100);
        m.step(ALL);
        for _ in 0..STALE_AFTER_WINDOWS + 5 {
            m.step(NOTHING);
        }
        assert!(m.stale_windows() > STALE_AFTER_WINDOWS);
        run(&mut m.thetis, 100, 100);
        m.step(ALL);
        assert_eq!(m.stale_windows(), 0, "one real window is enough");
    }

    #[test]
    fn a_new_session_is_not_stale() {
        let mut m = LossMeter::new();
        run(&mut m.thetis, 0, 100);
        m.step(ALL);
        for _ in 0..STALE_AFTER_WINDOWS + 5 {
            m.step(NOTHING);
        }
        m.reset();
        assert_eq!(m.stale_windows(), 0);
        assert_eq!(m.reported(), 0);
    }

    /// Twenty seconds, and the claim is checked rather than asserted in prose.
    ///
    /// The window length is the engine's heartbeat, which lives in another file.
    /// The doc comment on STALE_AFTER_WINDOWS says "twenty seconds at 500 ms", so
    /// if that interval ever moves, this says so instead of the comment quietly
    /// becoming a lie.
    #[test]
    fn the_stale_threshold_is_twenty_seconds_of_windows() {
        // The value, not the text of the line that holds it: scanning engine.rs
        // for `= 500;` breaks the wrong way round - write `500u64`, or let a
        // formatter touch the line, and the suite announces that the heartbeat
        // moved when nothing moved at all.
        //
        // And MULTIPLIED, not divided. Integer division hid a whole window:
        // 41 * 500 / 1000 is also 20, so a threshold of 41 passed. A reviewer
        // found that one by mutating the constant, which is the only way it was
        // ever going to turn up.
        assert_eq!(
            STALE_AFTER_WINDOWS as u64 * crate::engine::HEARTBEAT_INTERVAL_MS,
            20_000,
            "STALE_AFTER_WINDOWS is documented as twenty seconds; at a {} ms heartbeat it is not",
            crate::engine::HEARTBEAT_INTERVAL_MS
        );
    }

    // ---- open point 15: the peak, and the climb back down ----

    #[test]
    fn without_a_reset_a_reconnect_charges_for_its_own_gap() {
        let mut m = LossMeter::new();
        run(&mut m.thetis, 0, 100);
        m.step(ALL);
        run(&mut m.thetis, 100, 100);
        m.step(ALL);
        // The link is down. Four heartbeats pass with nothing arriving.
        let mut peak = 0;
        for _ in 0..4 {
            peak = m.step(ALL);
        }
        assert!(peak >= 60, "expected the historical false peak, got {peak}");
    }

    /// **The number that governs open point 15.**
    ///
    /// A reviewer put it plainly: the climb is not the interesting half. The
    /// server latches at two percent, so the very first empty window already
    /// switches error correction on - pinning how fast it reaches seventy proves
    /// nothing. What decides how long the protection STAYS on is how long the
    /// smoothed figure takes to come back to a reported zero, because only then
    /// does the server's counter of a thousand clean ticks begin.
    ///
    /// That is also the best explanation for the twenty-seven seconds radio 2
    /// took to let go in the 20:06 measurement, where the other two streams
    /// never let go at all.
    ///
    /// Pinned, not changed: the EMA is a number with a purpose, and moving it
    /// without measuring is what working rule 4 forbids.
    #[test]
    fn the_climb_back_down_is_what_keeps_the_protection_on() {
        let mut m = LossMeter::new();
        run(&mut m.thetis, 0, 100);
        m.step(ALL);
        run(&mut m.thetis, 100, 100);
        m.step(ALL);
        for _ in 0..6 {
            m.step(ALL);
        }
        let peak = m.reported();
        let peak_smoothed = m.smoothed();
        assert!(peak >= 70, "set up a real peak first, got {peak}");

        // Now the link is perfect again, every window.
        let mut windows = 0;
        let mut seq = 200;
        while m.reported() > 0 {
            run(&mut m.thetis, seq, 100);
            seq += 100;
            m.step(ALL);
            windows += 1;
            assert!(windows < 100, "the figure never came back down");
        }
        // TWO assertions, because one of them on its own proves nothing.
        //
        // The first simulates the decay instead of solving it in closed form.
        // A closed form starting from the ROUNDED figure answers a slightly
        // different question than the implementation, which decays the
        // unrounded one - a reviewer pointed out the ceil can then land a window
        // either side. This walks the same recursion the engine walks, so it
        // pins the SHAPE of the rule: change the update from an EMA to
        // something else and it goes red.
        //
        // The second pins the CHOICE of constant, and it is the reason this test
        // is worth having at all. Both `windows` and any derivation of it are
        // computed from EMA_OLD, so moving EMA_NEW from 0.3 to 0.25 slides both
        // sides together and stays green - a reviewer caught that the band of
        // the previous round let 0.3 -> 0.25 through, and that my replacement
        // let EVERY value through. A derivation guards the rule; only an
        // equality guards the number.
        let mut sim = peak_smoothed;
        let mut simulated_windows = 0;
        while sim.round() as u8 > 0 {
            sim *= EMA_OLD;
            simulated_windows += 1;
        }
        assert_eq!(
            windows, simulated_windows,
            "the engine took {windows} clean windows to report zero; walking the same decay by hand takes {simulated_windows}. The update rule changed shape."
        );
        assert_eq!(
            EMA_NEW, 0.3,
            "EMA_NEW decides how long the server keeps error correction on after a burst: about {windows} windows from a peak of {peak}, and only then does its counter of a thousand clean ticks start. Moving it is a decision - the simulation above follows any value, so this line is what notices."
        );
        assert!(peak > LATCH_AT);
    }

    #[test]
    fn a_reset_at_a_new_session_starts_from_silence() {
        let mut m = LossMeter::new();
        run(&mut m.thetis, 0, 100);
        m.step(ALL);
        run(&mut m.thetis, 100, 100);
        m.step(ALL);
        for _ in 0..4 {
            m.step(ALL);
        }
        assert!(m.reported() >= 60);
        // AUTH_ACCEPTED: a new session.
        m.reset();
        assert_eq!(m.reported(), 0);
        run(&mut m.thetis, 0, 50);
        assert_eq!(m.step(ALL), 0);
        run(&mut m.thetis, 50, 50);
        assert_eq!(m.step(ALL), 0, "a clean new session must read clean");
    }

    // ---- Expecting ----

    #[test]
    fn expecting_follows_the_clients_own_switches() {
        // Mono, RX1 off, RX2 on: the client still receives CH2, so it expects
        // the bundle. This is the rule that was narrowed to rx1 alone in build
        // 34 and cost an RX2-only listener its error correction.
        let e = Expecting::from_client(0, false, true, false, false, false, false);
        assert!(e.thetis);
        // Android (mode 255) never gets CH2.
        let e = Expecting::from_client(255, false, true, false, false, false, false);
        assert!(!e.thetis);
        // And the other four are passed straight through.
        let e = Expecting::from_client(255, true, false, true, false, false, true);
        assert!(e.thetis && e.radio1 && !e.radio2 && !e.vrx1 && e.vrx2);
    }

    /// Every stream is fed, every reset is wired, and `Expecting` is DERIVED.
    ///
    /// The tests above prove the module. They stay green while the engine feeds
    /// it nothing, and in build 36 they would have stayed green while a call
    /// site nailed `radio1: false` into a struct literal - a reviewer had to
    /// spot that, which is what a test is for. So this one reads engine.rs, and
    /// it forbids the literal.
    #[test]
    fn the_engine_feeds_every_stream_and_derives_what_it_expects() {
        let src = include_str!("engine.rs");
        for (needle, why) in [
            // Since build 90 every arrival goes through `audio_arrived!`, which
            // feeds this meter and measurement mode together; the macro must
            // still feed the meter, and each stream must still be fed.
            ("loss_meter.$stream.packet($seq)", "the macro would stop feeding the loss meter"),
            ("audio_arrived!(thetis,", "the Thetis bundle would stop being measured"),
            // The radios through one block since build 293, which feeds the
            // meter it is handed: each radio hands its own.
            ("audio_arrived!($meter, $stream,", "the radios' block would stop feeding the loss meter"),
            ("yaesu_audio_in!(pkt, \"Radio 1\", yaesu_jitter_buf, st_yaesu, yaesu_logged_first, radio1,", "radio 1 would go back to being protected by a figure about another stream"),
            ("yaesu_audio_in!(pkt, \"Radio 2\", yaesu2_jitter_buf, st_yaesu2, yaesu2_logged_first, radio2,", "same for radio 2"),
            ("audio_arrived!(vrx1,", "a VRX-only client would have nothing feeding its meter"),
            ("audio_arrived!(vrx2,", "same for VRX2"),
            ("loss_meter.reset()", "a reconnect would be charged for its own gap again - open point 15"),
            ("loss_meter.$meter.reset()", "a radio's stream that starts over would read as total loss"),
            ("loss_meter.vrx1.reset()", "same for VRX1"),
            ("loss_meter.vrx2.reset()", "same for VRX2"),
            ("Expecting::from_client(", "the engine would state the subscriptions in its own words"),
            ("loss_meter.stale_windows()", "a figure that stopped being refreshed would go back to being invisible - open point 18"),
            ("loss_meter.has_measured()", "the log would call a starting value of zero a measurement"),
            ("STALE_AFTER_WINDOWS", "the threshold would be spelled out in the engine instead of taken from here"),
        ] {
            assert!(src.contains(needle), "engine.rs no longer contains `{needle}` - {why}");
        }
        // Every mention of the type must be the constructor. Spelling out one
        // form, `loss_window::Expecting {`, missed `Expecting {`, `Expecting{`
        // and a brace on the next line - a reviewer pointed out that a gate
        // written as one literal string only forbids that literal string.
        for (i, _) in src.match_indices("Expecting") {
            assert!(
                src[i..].starts_with("Expecting::from_client("),
                "engine.rs mentions Expecting at byte {i} other than as `Expecting::from_client(`. Built by hand it can nail a stream to false with every test in this module green, because they all build their own: {:?}",
                &src[i..(i + 40).min(src.len())]
            );
        }
        // Both Thetis audio paths - legacy mono Audio and AudioMultiCh - feed the
        // window. A patch that removes one leaves a client measured on some
        // servers and not on others.
        assert_eq!(src.matches("audio_arrived!(thetis,").count(), 2);
        // One place closes the windows: the heartbeat. Two would mean two window
        // lengths, and the percentages would stop meaning the same thing.
        assert_eq!(src.matches("loss_meter.step(").count(), 1);
    }
}
