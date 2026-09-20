// SPDX-License-Identifier: GPL-2.0-or-later

//! The CW playout: key edges in, with the length of the state each one ends;
//! key commands out, at exact moments.
//!
//! Step 4 of `docs/internal/DESIGN-remote-cw.md`, section 5.5. The keyer in
//! the client made the timing; the network scrambled the arrival times; this
//! is where the timing is put back. The first edge of a run plays at its
//! arrival plus a fixed delay `D`, every following edge at *the previous
//! playout time plus the length the client measured*, so elements and gaps
//! come out exactly as they were keyed, whatever the packets did on the way.
//! An edge that arrives after its moment plays at once and is counted: that
//! is the only place jitter still reaches the signal, and the log shows it.
//!
//! Thetis protects the elements itself (it schedules the key-up from the
//! key-down it received); what it does not protect is the gap before an
//! element, which is the arrival time of the key-down. This buffer protects
//! the gaps.
//!
//! The scheduling is pure (`CwPlayout`, no clock of its own, no I/O) so it can
//! be tested to the millisecond; the thread around it (`spawn_playout_thread`)
//! sleeps to within a fraction of a millisecond of the next moment and spins
//! the rest, the way Thetis's own keyer thread does, because a tokio timer on
//! Windows may be a dozen milliseconds off and a dot at 30 wpm is forty.
//!
//! Safety that lives here: the watchdog (a key that stays down longer than
//! `WATCHDOG` goes up by itself, a second layer under Thetis's own), and
//! `release` for a client that is gone - key up now, buffer emptied, called
//! from the same places that release the PTT (guarantee 1 on the map).

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use sdr_remote_core::protocol::CwEdgesPacket;

/// How long after its arrival the first edge of a run plays. Fixed to start
/// with; measurement 6 (5G and wifi) says whether it can come down, and it may
/// only ever move between two runs, never inside a character.
pub const PLAYOUT_DELAY: Duration = Duration::from_millis(80);
/// When a run has to raise MOX first, its first edge waits this long instead
/// of `PLAYOUT_DELAY`: the time Thetis and the radio take from `TRX:0,true`
/// to a keyed transmitter, measured 160-225 ms on the ANAN-7000DLE (build 96
/// field test), with room.
pub const MOX_RISE: Duration = Duration::from_millis(250);
/// A key down longer than this goes up by itself. Thetis has the same number.
pub const WATCHDOG: Duration = Duration::from_millis(3000);
/// The longest gap that still continues a run. The client marks a run start
/// after a word gap (seven dots and a bit: 2150 ms at 4 wpm, the slowest the
/// keyer goes), so an edge that claims to continue a run across more than
/// this is not from our keyer. Taken at its word it would plan a key-down up
/// to 65 s ahead, hold the run open that long, and key a transmitter whose
/// MOX the packet watchdog had long since dropped.
pub const MAX_CONTINUE_GAP: Duration = Duration::from_millis(3000);
/// After this much key-up with nothing pending the run is over and the
/// transmitter goes back to receive - the hang time, what a radio calls the
/// semi break-in delay. The client may ask for another value (`set_hang`);
/// this is what a client that says nothing gets.
pub const IDLE_RELEASE: Duration = Duration::from_millis(500);
/// What a client may ask for. Below the floor every word gap would drop MOX
/// and cost the next word the rise; above the ceiling is not a hang time.
pub const HANG_MIN: Duration = Duration::from_millis(200);
pub const HANG_MAX: Duration = Duration::from_millis(3000);
/// How long past the end of all its own clocks a run still counts as holding
/// the transmitter (`holds_transmitter`). The idle release runs on the same
/// 100 ms tick that asks, so a run that ends normally is handed back long
/// before this; it is there for a run whose clocks stopped.
pub const HOLD_GRACE: Duration = Duration::from_millis(1000);
/// The playout thread sleeps until this close to the moment and spins the rest.
pub const SPIN_WINDOW: Duration = Duration::from_micros(1500);

/// An edge that is due (or was): what to send, and when it was meant to go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DueEdge {
    pub seq: u32,
    pub down: bool,
    /// The length of the state this edge ends, for Thetis's `keyer:` command.
    pub len_ms: u16,
    pub planned: Instant,
    /// The run this edge belongs to. A release ends the run; an edge that was
    /// already taken off the timeline but not yet sent must then not go out
    /// after the release's key-up - the sender checks `current_run`.
    pub run: u64,
    /// The radio this edge keys: 0 Thetis, 1 and 2 the Yaesu slots - the
    /// radio of its run.
    pub target: u8,
}

/// What one packet did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Ingest {
    pub accepted: usize,
    pub duplicates: usize,
    /// Edges that arrived after their moment: (seq, milliseconds late).
    pub late: Vec<(u32, u64)>,
    /// The packet was from someone other than the client whose run is
    /// playing; nothing was taken from it.
    pub refused_other_owner: bool,
    /// The packet was for another radio than the run that is playing; nothing
    /// was taken from it (it is seen all the same).
    pub refused_other_target: bool,
    /// New edges that came before a run start in the same packet: they belong
    /// to a run that is over (their own packets were lost) and were dropped.
    pub stale: usize,
}

/// Is `seq` after `last`, allowing for the counter wrapping round?
fn newer(seq: u32, last: Option<u32>) -> bool {
    match last {
        None => true,
        Some(l) => (seq.wrapping_sub(l) as i32) > 0,
    }
}

pub struct CwPlayout {
    delay: Duration,
    hang: Duration,
    /// The client whose run is playing. One at a time: a second client's
    /// edges are refused until this one's run is over.
    owner: Option<SocketAddr>,
    /// The radio of the run that is playing; set with the owner, cleared with it.
    target: Option<u8>,
    /// The last sequence number taken from each client. Kept across the end
    /// of a run: every packet repeats the edges before it, and forgetting the
    /// numbers when a run ended made the next run open with the previous
    /// run's last three edges (field test of build 95, 2026-09-17 23:32: key
    /// on, off, on at the start of every word). Dropped only when the client
    /// itself is gone.
    seen: HashMap<SocketAddr, u32>,
    /// When the last scheduled edge plays - the anchor for the next one.
    last_play_at: Option<Instant>,
    pending: VecDeque<DueEdge>,
    key_down: bool,
    key_down_since: Option<Instant>,
    last_up_at: Option<Instant>,
    /// Counts up on every release, watchdog and forget: what makes an edge
    /// that is already on its way stale.
    run: u64,
    pub played: u64,
    pub late_count: u64,
    /// Measurement 5: how far past its moment the thread took the last edge,
    /// and the worst so far, in microseconds.
    pub last_deviation_us: u64,
    pub max_deviation_us: u64,
}

#[allow(dead_code)] // owner(), key_down(), pending(): read by the tests, and by the status line when it comes
impl CwPlayout {
    pub fn new(delay: Duration) -> Self {
        Self {
            delay,
            hang: IDLE_RELEASE,
            owner: None,
            target: None,
            seen: HashMap::new(),
            last_play_at: None,
            pending: VecDeque::new(),
            key_down: false,
            key_down_since: None,
            last_up_at: None,
            run: 0,
            played: 0,
            late_count: 0,
            last_deviation_us: 0,
            max_deviation_us: 0,
        }
    }

    pub fn owner(&self) -> Option<SocketAddr> {
        self.owner
    }

    /// The radio of the run that is playing.
    pub fn target(&self) -> Option<u8> {
        self.target
    }

    /// Is a run playing that a packet of `addr` for radio `target` cannot
    /// join? Asked by the gates before they take a lock or raise MOX; the
    /// ingest's own refusals stay behind it as a second net.
    pub fn busy_for(&self, addr: SocketAddr, target: u8) -> bool {
        match self.owner {
            Some(o) => o != addr || self.target != Some(target),
            None => false,
        }
    }

    pub fn key_down(&self) -> bool {
        self.key_down
    }

    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// The hang time the client asked for, in milliseconds; 0 keeps what
    /// there is. Clamped to `HANG_MIN..=HANG_MAX`.
    pub fn set_hang(&mut self, ms: u16) {
        if ms != 0 {
            self.hang = Duration::from_millis(u64::from(ms)).clamp(HANG_MIN, HANG_MAX);
        }
    }

    pub fn hang(&self) -> Duration {
        self.hang
    }

    /// The run that is current; an edge with another number is stale.
    pub fn current_run(&self) -> u64 {
        self.run
    }

    /// Does this packet carry an edge this client has not sent before? Asked
    /// before anything is acquired for it: every packet repeats the last few
    /// edges, the relay and UDP both duplicate, and a packet of old news that
    /// arrived after its run was over used to take the Thetis lock, key MOX
    /// for two seconds with nothing to send, and leave the lock and the run
    /// with a client that was doing nothing (found reading the ingest arm
    /// before review 313).
    pub fn has_news(&self, addr: SocketAddr, pkt: &CwEdgesPacket) -> bool {
        let seen = self.seen.get(&addr).copied();
        pkt.edges.iter().any(|e| newer(e.seq, seen))
    }

    /// A packet that is refused - by a gate in front of the playout, or here
    /// because another client's run is playing - is seen all the same. Every
    /// packet repeats the edges before it, so the next packet this client
    /// sends would otherwise bring the refused edges back as news and play
    /// them: in the field test of build 105 an element keyed twelve seconds
    /// earlier, in USB mode, went on the air at the start of the next word.
    /// Takes nothing else: not the run, not a place in the queue.
    pub fn discard(&mut self, addr: SocketAddr, pkt: &CwEdgesPacket) {
        let mut last = self.seen.get(&addr).copied();
        for e in &pkt.edges {
            if newer(e.seq, last) {
                last = Some(e.seq);
            }
        }
        if let Some(l) = last {
            self.seen.insert(addr, l);
        }
    }

    /// Is a run holding the transmitter - from its first packet until it is
    /// handed back, the hang time included? The safety tick keeps the packet
    /// watchdog fed while it is: the hang time may be longer than that
    /// watchdog's window, and the run's own clocks (the idle release, the CW
    /// watchdog) are what end it. A client that is gone is dropped by its
    /// heartbeat, which this does not feed.
    ///
    /// Bounded in time as well: the run's clocks are run by the playout
    /// thread, and a run whose clocks stopped would otherwise hold for ever.
    /// It holds until its last planned edge, the CW watchdog after that and
    /// the hang time after that have all passed, plus `HOLD_GRACE`; then the
    /// packet watchdog takes over.
    pub fn holds_transmitter(&self, now: Instant) -> bool {
        if self.owner.is_none() {
            return false;
        }
        let mut until = self.last_up_at.map(|t| t + self.hang);
        if self.key_down {
            until = until.max(self.key_down_since.map(|t| t + WATCHDOG + self.hang));
        }
        if let Some(last) = self.pending.back() {
            until = until.max(Some(last.planned + WATCHDOG + self.hang));
        }
        until.is_some_and(|u| now < u + HOLD_GRACE)
    }

    /// Schedule the edges of one packet. Edges already seen (by sequence
    /// number) are dropped; the rest are placed on the timeline. `mox_rise`
    /// says MOX was just raised for this run, so a run start waits
    /// `MOX_RISE` rather than the ordinary delay.
    pub fn ingest(&mut self, addr: SocketAddr, pkt: &CwEdgesPacket, now: Instant, mox_rise: bool) -> Ingest {
        let mut out = Ingest::default();
        if matches!(self.owner, Some(o) if o != addr) {
            self.discard(addr, pkt);
            out.refused_other_owner = true;
            return out;
        }
        // One radio per run: a client that switched radios while its run
        // plays gets nothing played on the old radio's line. Seen all the
        // same, so it does not play later either.
        if matches!(self.target, Some(t) if t != pkt.target) {
            self.discard(addr, pkt);
            out.refused_other_target = true;
            return out;
        }
        // The run becomes this client's only if the packet brings something:
        // an owner with nothing planned and no key-up behind it would never
        // be found idle, and never handed back.
        if !self.has_news(addr, pkt) {
            out.duplicates = pkt.edges.len();
            return out;
        }
        self.owner = Some(addr);
        self.target = Some(pkt.target);
        // Whatever comes before the packet's last run start belongs to a run
        // that is over: the client marks a run start only after a word gap.
        // New edges there are edges whose own packets never arrived, and a
        // key-down among them would put an element of the past on the air.
        let run_start_at = pkt.edges.iter().rposition(|e| e.run_start);
        for (i, e) in pkt.edges.iter().enumerate() {
            if !newer(e.seq, self.seen.get(&addr).copied()) {
                out.duplicates += 1;
                continue;
            }
            self.seen.insert(addr, e.seq);
            if run_start_at.is_some_and(|r| i < r) {
                // One exception: the key-up of a key this server still holds
                // down (or will, once what is queued has played). That one
                // goes, or the key would stay down into the new run.
                let key_will_be_down = self.pending.back().map_or(self.key_down, |p| p.down);
                if e.down || !key_will_be_down {
                    out.stale += 1;
                    continue;
                }
            }
            let gap = Duration::from_millis(u64::from(e.prev_len_ms));
            let anchored = match self.last_play_at {
                Some(prev) if !e.run_start && gap <= MAX_CONTINUE_GAP => prev + gap,
                _ => now + self.delay,
            };
            // MOX was down when this packet came in: nothing plays before the
            // rise, whether the client called it a run start or not. The
            // client's idea of a run (a word gap) and the server's (the hang
            // time) are two clocks, and a hang shorter than a word gap drops
            // MOX inside what the client still calls one run.
            let anchored = if mox_rise { anchored.max(now + MOX_RISE) } else { anchored };
            // Edges come off the front of the queue, so it has to stay in time
            // order: a run start that anchors on its arrival while an edge of
            // the previous run is still waiting would wait behind it and go
            // out late (in build 105, 513 ms).
            let anchored = self.last_play_at.map_or(anchored, |prev| anchored.max(prev));
            let planned = if anchored < now {
                out.late.push((e.seq, (now - anchored).as_millis() as u64));
                self.late_count += 1;
                now
            } else {
                anchored
            };
            self.last_play_at = Some(planned);
            self.pending.push_back(DueEdge { seq: e.seq, down: e.down, len_ms: e.prev_len_ms, planned, run: self.run, target: pkt.target });
            out.accepted += 1;
        }
        out
    }

    /// The next moment the thread has to be awake for: the first pending
    /// edge, or the watchdog.
    pub fn next_deadline(&self) -> Option<Instant> {
        let edge = self.pending.front().map(|e| e.planned);
        let dog = if self.key_down { self.key_down_since.map(|t| t + WATCHDOG) } else { None };
        match (edge, dog) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Everything due at `now`, in order. Keeps the key state.
    pub fn take_due(&mut self, now: Instant) -> Vec<DueEdge> {
        let mut due = Vec::new();
        while let Some(e) = self.pending.front() {
            if e.planned > now {
                break;
            }
            let e = self.pending.pop_front().expect("front was there");
            let dev = now.saturating_duration_since(e.planned).as_micros() as u64;
            self.last_deviation_us = dev;
            self.max_deviation_us = self.max_deviation_us.max(dev);
            // The key state is anchored on the planned moment, not on when the
            // thread got round to it: the idle clock and the watchdog count
            // from the timing the operator made.
            self.apply(e.down, e.planned);
            self.played += 1;
            due.push(e);
        }
        due
    }

    fn apply(&mut self, down: bool, now: Instant) {
        if down && !self.key_down {
            self.key_down_since = Some(now);
        }
        if !down {
            self.last_up_at = Some(now);
        }
        self.key_down = down;
    }

    /// A key that has been down for `WATCHDOG` goes up now; whatever was
    /// still pending goes with it (a run whose key-up never came is not a
    /// run to continue).
    pub fn watchdog(&mut self, now: Instant) -> Option<DueEdge> {
        let since = self.key_down_since?;
        if !self.key_down || now < since + WATCHDOG {
            return None;
        }
        self.pending.clear();
        self.last_play_at = None;
        self.run += 1;
        self.apply(false, now);
        Some(DueEdge { seq: 0, down: false, len_ms: WATCHDOG.as_millis() as u16, planned: now, run: self.run, target: self.target.unwrap_or(0) })
    }

    /// This client is gone (goodbye or timed out): nothing of it may stay in
    /// the buffer, and if its key was down it comes up now. Returns the
    /// key-up to send, if there is one. Called from the same places that
    /// release the PTT.
    pub fn release(&mut self, addr: SocketAddr, now: Instant) -> Option<DueEdge> {
        if self.owner != Some(addr) {
            return None;
        }
        self.owner = None;
        let target = self.target.take().unwrap_or(0);
        self.seen.remove(&addr);
        self.last_play_at = None;
        self.pending.clear();
        self.run += 1;
        if self.key_down {
            self.apply(false, now);
            Some(DueEdge { seq: 0, down: false, len_ms: 0, planned: now, run: self.run, target })
        } else {
            None
        }
    }

    /// The owner whose run has been over for `IDLE_RELEASE`: key up, nothing
    /// pending, and quiet since. The caller hands the transmitter back and
    /// calls `forget_owner`.
    pub fn idle_owner(&self, now: Instant) -> Option<SocketAddr> {
        let owner = self.owner?;
        if self.key_down || !self.pending.is_empty() {
            return None;
        }
        let quiet_since = self.last_up_at?;
        (now >= quiet_since + self.hang).then_some(owner)
    }

    /// The run is over; the next packet from anyone starts a new one.
    pub fn forget_owner(&mut self) {
        self.owner = None;
        self.target = None;
        self.last_play_at = None;
        self.last_up_at = None;
        self.run += 1;
    }
}

/// The playout shared between the receive path (which schedules) and the
/// playout thread (which waits and sends). `wake` is signalled on every
/// ingest, so a thread asleep until a far deadline wakes for a nearer one.
pub struct Shared {
    pub inner: Mutex<CwPlayout>,
    pub wake: Condvar,
}

pub type Handle = Arc<Shared>;

pub fn new_handle() -> Handle {
    Arc::new(Shared { inner: Mutex::new(CwPlayout::new(PLAYOUT_DELAY)), wake: Condvar::new() })
}

/// Lock the playout, poisoned or not - a panic elsewhere must not stop the
/// key from going up.
pub fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, CwPlayout> {
    shared.inner.lock().unwrap_or_else(|e| e.into_inner())
}

/// The thread that turns moments into sends. It hands each due edge to `out`
/// the instant it is due; the receiving task sends the TCI command. It ends
/// when `out` has no receiver any more.
pub fn spawn_playout_thread(shared: Handle, out: tokio::sync::mpsc::UnboundedSender<DueEdge>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("cw-playout".into())
        .spawn(move || loop {
            if out.is_closed() {
                return;
            }
            // The deadline is read under the lock and the wait releases that
            // lock atomically, so an ingest can never slip between the two and
            // go unnoticed until the next timeout (the classic lost wake-up;
            // it cost the first edge of a run up to 100 ms in the first draft).
            let g = lock(&shared);
            let Some(deadline) = g.next_deadline() else {
                let _ = shared.wake.wait_timeout(g, Duration::from_millis(100));
                continue;
            };
            drop(g);
            // Sleep towards the moment in short steps, looking again each
            // time in case something nearer was scheduled, then spin the last
            // stretch: a plain sleep on Windows can be milliseconds coarse.
            loop {
                let now = Instant::now();
                if deadline <= now + SPIN_WINDOW {
                    break;
                }
                let step = (deadline - now - SPIN_WINDOW).min(Duration::from_millis(2));
                std::thread::sleep(step);
                if lock(&shared).next_deadline().map_or(true, |d| d < deadline) {
                    break; // something nearer, or nothing at all: start over
                }
            }
            if lock(&shared).next_deadline() != Some(deadline) {
                continue;
            }
            while Instant::now() < deadline {
                std::hint::spin_loop();
            }
            let (due, dog) = {
                let mut g = lock(&shared);
                let now = Instant::now();
                (g.take_due(now), g.watchdog(now))
            };
            for e in due {
                let _ = out.send(e);
            }
            if let Some(e) = dog {
                let _ = out.send(e);
            }
        })
        .expect("spawn the CW playout thread")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdr_remote_core::protocol::CwEdge;

    fn a(n: u8) -> SocketAddr {
        format!("127.0.0.{n}:1").parse().unwrap()
    }
    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }
    fn edge(seq: u32, down: bool, prev_len_ms: u16, run_start: bool) -> CwEdge {
        CwEdge { seq, down, prev_len_ms, run_start }
    }
    fn pkt(edges: &[CwEdge]) -> CwEdgesPacket {
        CwEdgesPacket { target: 0, edges: edges.to_vec(), hang_ms: 0 }
    }
    fn fresh() -> (CwPlayout, Instant) {
        (CwPlayout::new(ms(80)), Instant::now())
    }

    #[test]
    fn the_first_edge_of_a_run_plays_at_arrival_plus_the_delay() {
        let (mut p, t0) = fresh();
        let r = p.ingest(a(1), &pkt(&[edge(1, true, 0, true)]), t0, false);
        assert_eq!(r.accepted, 1);
        assert_eq!(p.next_deadline(), Some(t0 + ms(80)));
        assert!(p.take_due(t0 + ms(79)).is_empty(), "not yet");
        let due = p.take_due(t0 + ms(80));
        assert_eq!(due.len(), 1);
        assert!(due[0].down && p.key_down());
    }

    /// Whatever the packets did on the way: each edge plays at the previous
    /// playout time plus the length the client measured.
    #[test]
    fn every_following_edge_is_anchored_on_the_previous_playout_not_on_arrival() {
        let (mut p, t0) = fresh();
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true)]), t0, false); // plays at 80
        // The key-up arrives early (dot of 48 ms, arrives at +30) ...
        p.ingest(a(1), &pkt(&[edge(2, false, 48, false)]), t0 + ms(30), false);
        // ... and the next key-down late (gap of 48, arrives at +170, after its moment 176? no: 128+48 = 176, so at 170 it is early still)
        p.ingest(a(1), &pkt(&[edge(3, true, 48, false)]), t0 + ms(170), false);
        let due = p.take_due(t0 + ms(200));
        let planned: Vec<Duration> = due.iter().map(|e| e.planned - t0).collect();
        assert_eq!(planned, vec![ms(80), ms(128), ms(176)]);
        assert_eq!(p.late_count, 0);
        // And what goes to Thetis with each edge: the length of the state it
        // ends, which is what Thetis gives the element (review 313: no test
        // looked at the payload, only at the timing).
        assert_eq!(due.iter().map(|e| e.len_ms).collect::<Vec<_>>(), vec![0, 48, 48]);
    }

    /// Every edge carries the run it belongs to, and that is the current run
    /// - also after an earlier run ended, when the number is no longer 0. An
    /// edge with a wrong number is dropped by the sender as stale, and the
    /// key never goes on the air (review 313: `run: 0` passed every test,
    /// because every test ran in run 0).
    #[test]
    fn every_edge_carries_its_own_run() {
        let (mut p, t0) = fresh();
        p.forget_owner();
        p.forget_owner(); // run 2: not the number an edge gets by default
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true), edge(2, false, 48, false)]), t0, false);
        let due = p.take_due(t0 + ms(200));
        assert!(due.iter().all(|e| e.run == p.current_run()), "{due:?} in run {}", p.current_run());
        // The key-ups the server makes itself carry the new run: they are the
        // last word of the old one.
        p.ingest(a(1), &pkt(&[edge(3, true, 1000, true)]), t0 + ms(1200), false);
        p.take_due(t0 + ms(1280));
        let dog = p.watchdog(t0 + ms(1280) + WATCHDOG).expect("lifted");
        assert_eq!((dog.run, dog.len_ms), (p.current_run(), WATCHDOG.as_millis() as u16));
        p.ingest(a(1), &pkt(&[edge(4, true, 5000, true)]), t0 + ms(9000), false);
        p.take_due(t0 + ms(9080));
        let up = p.release(a(1), t0 + ms(9100)).expect("key was down");
        assert_eq!((up.run, up.len_ms), (p.current_run(), 0), "the key-up of a client that is gone: no element length");
    }

    #[test]
    fn an_edge_that_arrives_after_its_moment_plays_at_once_and_is_counted() {
        let (mut p, t0) = fresh();
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true)]), t0, false); // 80
        p.take_due(t0 + ms(80));
        // The key-up (element 48 ms, due at 128) only arrives at 150.
        let r = p.ingest(a(1), &pkt(&[edge(2, false, 48, false)]), t0 + ms(150), false);
        assert_eq!(r.late, vec![(2, 22)]);
        assert_eq!(p.late_count, 1, "and counted, for the log line every hundred edges");
        assert_eq!(p.next_deadline(), Some(t0 + ms(150)), "plays now");
        // And the one after it is anchored on that late moment, so the gap is kept.
        p.ingest(a(1), &pkt(&[edge(3, true, 48, false)]), t0 + ms(151), false);
        assert_eq!(p.take_due(t0 + ms(200)).last().unwrap().planned, t0 + ms(198));
    }

    /// The packet carries the last four edges; the ones already scheduled are
    /// dropped by sequence number, so a lost packet is caught up by the next
    /// without doubling anything.
    #[test]
    fn repeated_edges_are_taken_once() {
        let (mut p, t0) = fresh();
        let e1 = edge(1, true, 0, true);
        let e2 = edge(2, false, 48, false);
        let e3 = edge(3, true, 48, false);
        assert_eq!(p.ingest(a(1), &pkt(&[e1]), t0, false).accepted, 1);
        // The packet with e2 was lost; the one with e2 and e3 arrives.
        let r = p.ingest(a(1), &pkt(&[e1, e2, e3]), t0 + ms(100), false);
        assert_eq!((r.accepted, r.duplicates), (2, 1));
        assert_eq!(p.pending(), 3);
        let r = p.ingest(a(1), &pkt(&[e1, e2, e3]), t0 + ms(101), false);
        assert_eq!((r.accepted, r.duplicates), (0, 3));
    }

    #[test]
    fn a_new_run_starts_from_its_own_arrival() {
        let (mut p, t0) = fresh();
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true), edge(2, false, 48, false)]), t0, false);
        p.take_due(t0 + ms(200));
        // Two seconds of silence, then a new run: not anchored on the old one.
        let t1 = t0 + ms(2000);
        p.ingest(a(1), &pkt(&[edge(3, true, 1872, true)]), t1, false);
        assert_eq!(p.next_deadline(), Some(t1 + ms(80)));
    }

    #[test]
    fn the_watchdog_lifts_a_key_that_stays_down() {
        let (mut p, t0) = fresh();
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true)]), t0, false);
        p.take_due(t0 + ms(80));
        assert_eq!(p.watchdog(t0 + ms(80) + WATCHDOG - ms(1)), None);
        let up = p.watchdog(t0 + ms(80) + WATCHDOG).expect("lifted");
        assert!(!up.down && !p.key_down());
        assert_eq!(p.watchdog(t0 + ms(80) + WATCHDOG + ms(1)), None, "once");
        assert_eq!(p.next_deadline(), None);
    }

    /// The watchdog ends the run it lifts: what was still planned goes, and an
    /// edge already on its way to the sender is stale. A key-down scheduled
    /// after the watchdog's key-up would key the transmitter again with
    /// nobody there (six mutants survived before these tests; all six sat in
    /// the corner that keeps a transmitter from hanging).
    #[test]
    fn the_watchdog_takes_the_rest_of_the_run_with_it() {
        let (mut p, t0) = fresh();
        // Key down at 80, and a key-up the client says comes 3 s later (the
        // longest a run may continue across), then a down.
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true), edge(2, false, 3000, false), edge(3, true, 100, false)]), t0, false);
        p.take_due(t0 + ms(80));
        let run = p.current_run();
        assert_eq!(p.pending(), 2);
        assert!(p.watchdog(t0 + ms(80) + WATCHDOG).is_some());
        assert_eq!(p.pending(), 0, "nothing left to play");
        assert_ne!(p.current_run(), run, "and what was already taken is stale");
        assert!(p.take_due(t0 + ms(10_000)).is_empty());
        assert!(!p.key_down());
    }

    /// The run is over only when the key is up AND nothing is planned.
    #[test]
    fn a_run_is_not_over_while_the_key_is_down_or_an_edge_is_planned() {
        // Key held down, long after an earlier key-up: not idle.
        let (mut p, t0) = fresh();
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true), edge(2, false, 48, false), edge(3, true, 48, false)]), t0, false);
        p.take_due(t0 + ms(200)); // down, up, down: the key is down now
        assert!(p.key_down());
        assert_eq!(p.idle_owner(t0 + ms(2000)), None, "a held key is not an idle run");
        // Key up, but a key-down still planned far ahead: not idle either.
        let (mut p, t0) = fresh();
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true), edge(2, false, 48, false), edge(3, true, 2000, false)]), t0, false);
        p.take_due(t0 + ms(200)); // down, up; the second down waits until 2128
        assert!(!p.key_down() && p.pending() == 1);
        assert_eq!(p.idle_owner(t0 + ms(128) + IDLE_RELEASE + ms(100)), None, "an edge is still to come");
    }

    /// Handing the transmitter back ends the run too: an edge taken just
    /// before must not be sent after MOX has gone down.
    #[test]
    fn forgetting_the_owner_makes_edges_on_their_way_stale() {
        let (mut p, t0) = fresh();
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true), edge(2, false, 48, false)]), t0, false);
        let taken = p.take_due(t0 + ms(200));
        p.forget_owner();
        assert_ne!(taken[0].run, p.current_run());
    }

    #[test]
    fn the_watchdog_is_the_deadline_while_the_key_is_down() {
        let (mut p, t0) = fresh();
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true)]), t0, false);
        p.take_due(t0 + ms(80));
        assert_eq!(p.next_deadline(), Some(t0 + ms(80) + WATCHDOG));
    }

    #[test]
    fn a_client_that_is_gone_gets_its_key_lifted_and_its_buffer_emptied() {
        let (mut p, t0) = fresh();
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true), edge(2, false, 48, false), edge(3, true, 48, false)]), t0, false);
        p.take_due(t0 + ms(80)); // key down, two pending
        assert_eq!(p.release(a(2), t0 + ms(90)), None, "not its run");
        let up = p.release(a(1), t0 + ms(90)).expect("key was down");
        assert!(!up.down);
        assert_eq!((p.pending(), p.key_down(), p.owner()), (0, false, None));
        assert_eq!(p.release(a(1), t0 + ms(91)), None, "nothing left to lift");
    }

    #[test]
    fn one_run_at_a_time_a_second_client_is_refused_until_it_is_over() {
        let (mut p, t0) = fresh();
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true), edge(2, false, 48, false)]), t0, false);
        let r = p.ingest(a(2), &pkt(&[edge(1, true, 0, true)]), t0 + ms(10), false);
        assert!(r.refused_other_owner && r.accepted == 0);
        p.take_due(t0 + ms(200));
        assert_eq!(p.idle_owner(t0 + ms(200)), None, "quiet, but not for long enough");
        assert_eq!(p.idle_owner(t0 + ms(128) + IDLE_RELEASE), Some(a(1)));
        p.forget_owner();
        // Its next word is taken; the refused edge riding along is not
        // (build 106: refused is seen, see edges_refused_for_another_clients_run_are_not_played_later).
        let r = p.ingest(a(2), &pkt(&[edge(1, true, 0, true), edge(2, true, 1300, true)]), t0 + ms(1300), false);
        assert_eq!((r.accepted, r.duplicates), (1, 1));
        assert_eq!(p.owner(), Some(a(2)));
    }

    /// An edge taken off the timeline just before a release is stale: the
    /// release's key-up must be the last word.
    #[test]
    fn an_edge_taken_before_a_release_is_stale_afterwards() {
        let (mut p, t0) = fresh();
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true)]), t0, false);
        let taken = p.take_due(t0 + ms(80));
        assert_eq!(taken[0].run, p.current_run());
        p.release(a(1), t0 + ms(81));
        assert_ne!(taken[0].run, p.current_run(), "the sender drops it");
    }

    /// The edges before a run's first one ride along in its packet; they were
    /// played in the previous run and must not be played again just because
    /// that run is over (field test of build 95). Seen to fail on the old
    /// code: with the numbers forgotten at the end of a run, the three old
    /// edges were accepted again.
    #[test]
    fn the_previous_runs_edges_are_not_replayed_when_a_new_run_starts() {
        let (mut p, t0) = fresh();
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true), edge(2, false, 100, false), edge(3, true, 100, false), edge(4, false, 100, false)]), t0, false);
        p.take_due(t0 + ms(1000));
        assert_eq!(p.idle_owner(t0 + ms(2000)), Some(a(1)));
        p.forget_owner();
        // Sixty seconds later the next word: its packet still carries 2, 3, 4.
        let t1 = t0 + ms(60_000);
        let r = p.ingest(a(1), &pkt(&[edge(2, false, 100, false), edge(3, true, 100, false), edge(4, false, 100, false), edge(5, true, 59_600, true)]), t1, false);
        assert_eq!((r.accepted, r.duplicates), (1, 3));
        let due = p.take_due(t1 + ms(80));
        assert_eq!(due.len(), 1);
        assert!(due[0].down && due[0].seq == 5, "only the new key-down: {due:?}");
        // A client that is gone starts afresh, though: its numbers go with it.
        p.release(a(1), t1 + ms(100));
        assert_eq!(p.ingest(a(1), &pkt(&[edge(1, true, 0, true)]), t1 + ms(200), false).accepted, 1);
    }

    /// A run that had to raise MOX first waits for the rise; one that found
    /// MOX up plays after the ordinary delay.
    #[test]
    fn a_run_that_raised_mox_plays_its_first_edge_after_the_rise() {
        let (mut p, t0) = fresh();
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true), edge(2, false, 100, false)]), t0, true);
        assert_eq!(p.next_deadline(), Some(t0 + MOX_RISE));
        assert_eq!(p.take_due(t0 + ms(400)).last().unwrap().planned, t0 + MOX_RISE + ms(100), "anchored on the first");
        p.forget_owner();
        p.ingest(a(1), &pkt(&[edge(3, true, 2000, true)]), t0 + ms(3000), false);
        assert_eq!(p.next_deadline(), Some(t0 + ms(3080)));
    }

    #[test]
    fn the_hang_time_is_what_the_client_asked_for_within_bounds() {
        let (mut p, t0) = fresh();
        assert_eq!(p.hang(), IDLE_RELEASE);
        p.set_hang(0);
        assert_eq!(p.hang(), IDLE_RELEASE, "0 keeps what there is");
        p.set_hang(50);
        assert_eq!(p.hang(), HANG_MIN);
        p.set_hang(u16::MAX);
        assert_eq!(p.hang(), HANG_MAX);
        p.set_hang(300);
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true), edge(2, false, 48, false)]), t0, false);
        p.take_due(t0 + ms(200));
        assert_eq!(p.idle_owner(t0 + ms(128 + 299)), None);
        assert_eq!(p.idle_owner(t0 + ms(128 + 300)), Some(a(1)));
    }

    /// MOX down and an edge the client does not call a run start: it waits
    /// for the rise all the same, and what follows keeps its spacing.
    #[test]
    fn an_edge_that_finds_mox_down_waits_for_the_rise_whatever_the_client_called_it() {
        let (mut p, t0) = fresh();
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true), edge(2, false, 48, false)]), t0, false);
        p.take_due(t0 + ms(200));
        p.forget_owner(); // the hang time ran out: MOX is down
        let t1 = t0 + ms(700);
        p.ingest(a(1), &pkt(&[edge(3, true, 572, false), edge(4, false, 48, false)]), t1, true);
        let due = p.take_due(t1 + ms(400));
        assert_eq!(due[0].planned, t1 + MOX_RISE);
        assert_eq!(due[1].planned, t1 + MOX_RISE + ms(48));
    }

    /// A packet of old news takes nothing: not the run, not a place in the
    /// queue. After a run is over its last packet may well arrive again.
    #[test]
    fn a_packet_with_nothing_new_takes_nothing() {
        let (mut p, t0) = fresh();
        let edges = [edge(1, true, 0, true), edge(2, false, 48, false)];
        p.ingest(a(1), &pkt(&edges), t0, false);
        p.take_due(t0 + ms(200));
        p.forget_owner();
        assert!(!p.has_news(a(1), &pkt(&edges)));
        let r = p.ingest(a(1), &pkt(&edges), t0 + ms(900), true);
        assert_eq!((r.accepted, r.duplicates), (0, 2));
        assert_eq!((p.owner(), p.pending()), (None, 0), "the run is still nobody's");
        assert!(p.has_news(a(1), &pkt(&[edge(2, false, 48, false), edge(3, true, 900, true)])));
        assert!(p.has_news(a(2), &pkt(&edges)), "another client's numbers are its own");
    }

    /// An edge that continues a run across a gap no keyer of ours makes is
    /// treated as the start of a new run, not planned a minute ahead.
    #[test]
    fn a_gap_no_keyer_makes_does_not_hold_the_run_open() {
        let (mut p, t0) = fresh();
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true), edge(2, false, 48, false)]), t0, false);
        p.take_due(t0 + ms(200));
        let t1 = t0 + ms(300);
        p.ingest(a(1), &pkt(&[edge(3, true, u16::MAX, false)]), t1, false);
        assert_eq!(p.next_deadline(), Some(t1 + ms(80)), "anchored on its arrival");
        // At the limit it still continues the run.
        let (mut p, t0) = fresh();
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true), edge(2, false, 48, false)]), t0, false);
        p.take_due(t0 + ms(200));
        p.ingest(a(1), &pkt(&[edge(3, true, 3000, false)]), t0 + ms(300), false);
        assert_eq!(p.next_deadline(), Some(t0 + ms(128 + 3000)));
    }

    /// Field test of build 105, 2026-09-18 15:20: keyed in USB mode, refused
    /// three times; twelve seconds later, in CW, the next word's first packet
    /// still carried the refused edges 24-26. Nothing had marked them seen, so
    /// they were played - an element keyed twelve seconds earlier went on the
    /// air - and the run's own first edge waited behind them, 513 ms late.
    #[test]
    fn edges_that_were_refused_are_never_played_later() {
        let (mut p, t0) = fresh();
        // The refused attempt: the server took nothing, but it has seen them.
        let refused = pkt(&[edge(23, true, 66, false), edge(24, false, 198, false), edge(25, true, 315, false), edge(26, false, 198, false)]);
        p.discard(a(1), &refused);
        assert!(!p.has_news(a(1), &refused), "a repeat of a refused packet is old news");
        assert_eq!((p.owner(), p.pending()), (None, 0), "and discarding takes nothing");
        // The next word.
        let t1 = t0 + ms(12_000);
        let r = p.ingest(a(1), &pkt(&[edge(24, false, 198, false), edge(25, true, 315, false), edge(26, false, 198, false), edge(27, true, 12_565, true)]), t1, true);
        assert_eq!((r.accepted, r.duplicates), (1, 3));
        let due = p.take_due(t1 + ms(1000));
        assert_eq!(due.len(), 1, "only the run's own edge: {due:?}");
        assert_eq!((due[0].seq, due[0].planned), (27, t1 + MOX_RISE));
    }

    /// The same edges, never seen because their packets were lost rather
    /// than refused: whatever precedes a run start in the same packet belongs
    /// to a run that is over. A key-down among them would put an element of
    /// the past on the air; they are dropped and counted.
    #[test]
    fn new_edges_before_a_run_start_are_stale_and_not_played() {
        let (mut p, t0) = fresh();
        p.ingest(a(1), &pkt(&[edge(22, true, 0, true), edge(23, false, 66, false)]), t0, false);
        p.take_due(t0 + ms(500));
        p.forget_owner();
        let t1 = t0 + ms(12_000);
        let r = p.ingest(a(1), &pkt(&[edge(24, true, 198, false), edge(25, false, 315, false), edge(26, true, 198, false), edge(27, false, 12_565, false), edge(28, true, 3000, true)]), t1, true);
        assert_eq!((r.accepted, r.stale), (1, 4));
        let due = p.take_due(t1 + ms(1000));
        assert_eq!(due.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![28]);
        assert_eq!(due[0].planned, t1 + MOX_RISE);
        // Two short words in one packet, the first never arrived: it is the
        // last run start that counts, the earlier word is the past as well.
        p.ingest(a(1), &pkt(&[edge(29, false, 66, false)]), t1 + ms(500), false);
        p.take_due(t1 + ms(1000));
        p.forget_owner(); // the key is up, the run handed back
        let t2 = t1 + ms(10_000);
        let r = p.ingest(a(1), &pkt(&[edge(30, true, 2500, true), edge(31, false, 66, false), edge(32, true, 2500, true)]), t2, true);
        assert_eq!((r.accepted, r.stale), (1, 2));
        assert_eq!(p.take_due(t2 + ms(1000)).iter().map(|e| e.seq).collect::<Vec<_>>(), vec![32]);
    }

    /// Stale, but not to be dropped: the key-up of a key the server still
    /// holds down. It goes at once, and the run start after it keeps its own
    /// anchor.
    #[test]
    fn a_stale_key_up_still_lifts_a_key_that_is_down() {
        let (mut p, t0) = fresh();
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true)]), t0, false);
        p.take_due(t0 + ms(80));
        assert!(p.key_down());
        // The key-up was lost; a second later the next run's packet carries it.
        let t1 = t0 + ms(1000);
        let r = p.ingest(a(1), &pkt(&[edge(2, false, 66, false), edge(3, true, 800, true)]), t1, false);
        assert_eq!((r.accepted, r.stale), (2, 0));
        let due = p.take_due(t1 + ms(200));
        assert_eq!(due.iter().map(|e| (e.seq, e.down)).collect::<Vec<_>>(), vec![(2, false), (3, true)]);
        assert_eq!(due[0].planned, t1, "the key comes up now");
        assert_eq!(due[1].planned, t1 + ms(80));
        // Only one key-up for it, however many the stale stretch holds.
        let (mut p, t0) = fresh();
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true)]), t0, false);
        p.take_due(t0 + ms(80));
        let r = p.ingest(a(1), &pkt(&[edge(2, false, 66, false), edge(3, true, 66, false), edge(4, false, 66, false), edge(5, true, 800, true)]), t0 + ms(1000), false);
        assert_eq!((r.accepted, r.stale), (2, 2), "up, then the run start; the down and its up are the past");
        // A refusal in the middle of a run (the mode changed, the Amplitec
        // went receive-only) leaves the server's key down and the refused
        // key-up seen: the stream no longer alternates from the server's point
        // of view. A stale key-down then meets a key that is down, and it is
        // the past all the same.
        let (mut p, t0) = fresh();
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true)]), t0, false);
        p.take_due(t0 + ms(80));
        p.discard(a(1), &pkt(&[edge(1, true, 0, true), edge(2, false, 66, false)]));
        let r = p.ingest(a(1), &pkt(&[edge(2, false, 66, false), edge(3, true, 66, false), edge(4, false, 66, false), edge(5, true, 800, true)]), t0 + ms(1000), false);
        assert_eq!((r.accepted, r.stale), (2, 1), "the stale down is dropped, the up after it lifts the key");
        assert_eq!(p.take_due(t0 + ms(2000)).iter().map(|e| (e.seq, e.down)).collect::<Vec<_>>(), vec![(4, false), (5, true)]);
    }

    /// Edges come off the front of the queue, so the queue has to be in time
    /// order: a run start that anchors earlier than an edge still waiting in
    /// front of it would wait behind that edge and go out late.
    #[test]
    fn the_queue_stays_in_time_order() {
        let (mut p, t0) = fresh();
        // A long gap the run may still continue across: the down plays at 80+48+3000.
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true), edge(2, false, 48, false), edge(3, true, 3000, false)]), t0, false);
        // Its key-up and a run start arrive early (a burst after a stall).
        p.ingest(a(1), &pkt(&[edge(4, false, 48, false), edge(5, true, 2500, true)]), t0 + ms(200), false);
        let due = p.take_due(t0 + ms(10_000));
        let planned: Vec<Instant> = due.iter().map(|e| e.planned).collect();
        assert!(planned.windows(2).all(|w| w[0] <= w[1]), "{:?}", due.iter().map(|e| (e.seq, e.planned - t0)).collect::<Vec<_>>());
        assert_eq!(due.len(), 5);
    }

    /// A client refused because another client's run is playing: what it
    /// sent is seen all the same, so it does not play once the run is over.
    #[test]
    fn edges_refused_for_another_clients_run_are_not_played_later() {
        let (mut p, t0) = fresh();
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true), edge(2, false, 48, false)]), t0, false);
        let r = p.ingest(a(2), &pkt(&[edge(7, true, 0, true), edge(8, false, 48, false)]), t0 + ms(10), false);
        assert!(r.refused_other_owner);
        p.take_due(t0 + ms(200));
        p.forget_owner();
        let r = p.ingest(a(2), &pkt(&[edge(7, true, 0, true), edge(8, false, 48, false), edge(9, true, 2000, true)]), t0 + ms(2000), false);
        assert_eq!((r.accepted, r.duplicates), (1, 2));
    }

    /// While a run is this client's, the transmitter is held for it - also in
    /// the hang time after the last key-up, which may be longer than the
    /// packet watchdog's two seconds (field test of build 105: hang time 3000,
    /// MOX dropped by "No PTT packets for 2.0s" a second before the run was
    /// over). The safety tick feeds the watchdog while this says so.
    #[test]
    fn a_run_holds_the_transmitter_until_it_is_handed_back() {
        let (mut p, t0) = fresh();
        assert!(!p.holds_transmitter(t0));
        p.set_hang(3000);
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true), edge(2, false, 48, false)]), t0, false);
        assert!(p.holds_transmitter(t0), "from the first packet, before the first edge plays");
        p.take_due(t0 + ms(200));
        assert!(p.holds_transmitter(t0 + ms(128 + 2999)), "in the hang time");
        assert_eq!(p.idle_owner(t0 + ms(128 + 2999)), None);
        assert_eq!(p.idle_owner(t0 + ms(128 + 3000)), Some(a(1)));
        p.forget_owner();
        assert!(!p.holds_transmitter(t0 + ms(128 + 3000)), "handed back");
        // A packet of old news does not make it hold anything.
        p.ingest(a(1), &pkt(&[edge(2, false, 48, false)]), t0 + ms(4000), false);
        assert!(!p.holds_transmitter(t0 + ms(4000)));
    }

    /// The duty below rests on the playout thread running the run's clocks.
    /// Should that thread ever stop, nothing would play, the key would never
    /// come up and the run would never go idle - and the safety tick would
    /// feed the packet watchdog for ever. So holding has a time limit of its
    /// own: once every clock of the run has run out (the last planned edge,
    /// the CW watchdog after it, the hang time after that) plus `HOLD_GRACE`,
    /// the run no longer holds, and the packet watchdog drops MOX.
    #[test]
    fn a_run_whose_clocks_stopped_stops_holding() {
        let (mut p, t0) = fresh();
        p.set_hang(3000);
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true), edge(2, false, 48, false), edge(3, true, 2000, false)]), t0, false);
        // Nothing is ever taken off the timeline: the thread is dead.
        let last_planned = t0 + ms(80 + 48 + 2000);
        let limit = last_planned + WATCHDOG + ms(3000) + HOLD_GRACE;
        assert!(p.holds_transmitter(limit - ms(1)));
        assert!(!p.holds_transmitter(limit), "its clocks ran out: the packet watchdog takes over");
        // A key held down with nothing planned: the watchdog's moment, the hang, the grace.
        let (mut p, t0) = fresh();
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true)]), t0, false);
        p.take_due(t0 + ms(80));
        let limit = t0 + ms(80) + WATCHDOG + IDLE_RELEASE + HOLD_GRACE;
        assert!(p.holds_transmitter(limit - ms(1)));
        assert!(!p.holds_transmitter(limit));
        // A key-up with nothing planned: the hang time and the grace.
        let (mut p, t0) = fresh();
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true), edge(2, false, 48, false)]), t0, false);
        p.take_due(t0 + ms(200));
        let limit = t0 + ms(128) + IDLE_RELEASE + HOLD_GRACE;
        assert!(p.holds_transmitter(limit - ms(1)));
        assert!(!p.holds_transmitter(limit));
    }

    /// The duty `holds_transmitter` carries since build 106: the safety tick
    /// feeds the packet watchdog while a run holds the transmitter, so every
    /// run has to end by its own clocks - a run that never became idle would
    /// keep MOX up for ever. Every shape a packet can have, played out with
    /// nothing more arriving: the run is handed back within the watchdog plus
    /// the longest gap plus the longest hang time.
    #[test]
    fn every_run_ends_by_itself() {
        let shapes: Vec<Vec<CwEdge>> = vec![
            vec![edge(1, true, 0, true), edge(2, false, 48, false)],
            vec![edge(1, true, 0, true)], // the key-up never comes
            vec![edge(1, true, 0, true), edge(2, false, 48, false), edge(3, true, 3000, false)],
            vec![edge(1, false, 66, false), edge(2, true, 198, false), edge(3, true, 900, true)], // stale before the start
            vec![edge(1, true, u16::MAX, false)], // a gap no keyer makes
            vec![edge(1, false, 66, false)], // a lone key-up
        ];
        for (n, edges) in shapes.iter().enumerate() {
            let (mut p, t0) = fresh();
            p.set_hang(u16::MAX); // the longest hang there is
            p.ingest(a(1), &pkt(edges), t0, true);
            assert!(p.holds_transmitter(t0), "shape {n}");
            let mut t = t0;
            let end = t0 + WATCHDOG + MAX_CONTINUE_GAP + HANG_MAX + ms(1000);
            let mut handed_back = false;
            while t <= end {
                p.take_due(t);
                p.watchdog(t);
                if p.idle_owner(t) == Some(a(1)) {
                    p.forget_owner();
                    handed_back = true;
                    break;
                }
                t += ms(10);
            }
            assert!(handed_back && !p.holds_transmitter(t), "shape {n} never ended: pending {}, key down {}", p.pending(), p.key_down());
        }
    }

    /// Step 6: a run belongs to one radio - Thetis (0) or a Yaesu slot (1, 2) -
    /// and every edge of it says which, so the sender keys the right line.
    /// The key-ups the playout makes itself (watchdog, release) go to the
    /// run's radio too: a Yaesu has no watchdog of its own, and a key-up sent
    /// to Thetis instead would leave its RTS high.
    #[test]
    fn every_edge_goes_to_the_radio_of_its_run() {
        let (mut p, t0) = fresh();
        let yaesu = |edges: &[CwEdge]| CwEdgesPacket { target: 1, edges: edges.to_vec(), hang_ms: 0 };
        p.ingest(a(1), &yaesu(&[edge(1, true, 0, true), edge(2, false, 48, false), edge(3, true, 48, false)]), t0, false);
        assert_eq!(p.target(), Some(1));
        let due = p.take_due(t0 + ms(200));
        assert!(due.iter().all(|e| e.target == 1), "{due:?}");
        let dog = p.watchdog(t0 + ms(176) + WATCHDOG).expect("the key was down");
        assert_eq!(dog.target, 1, "the watchdog lifts the Yaesu's line, not Thetis's");
        // A release of a key that is down goes to the run's radio as well.
        let (mut p, t0) = fresh();
        p.ingest(a(1), &yaesu(&[edge(1, true, 0, true)]), t0, false);
        p.take_due(t0 + ms(80));
        assert_eq!(p.release(a(1), t0 + ms(100)).expect("down").target, 1);
        assert_eq!(p.target(), None, "the run is over with its client");
    }

    /// The client switches radios while its run plays: the new radio's edges
    /// are refused (and seen), not played on the old radio's line. Once the
    /// run is over, the next run may go to the other radio.
    #[test]
    fn a_run_does_not_change_radios_halfway() {
        let (mut p, t0) = fresh();
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true), edge(2, false, 48, false)]), t0, false);
        let other = CwEdgesPacket { target: 2, edges: vec![edge(3, true, 48, false)], hang_ms: 0 };
        let r = p.ingest(a(1), &other, t0 + ms(100), false);
        assert!(r.refused_other_target && r.accepted == 0);
        assert!(!p.has_news(a(1), &other), "refused is seen");
        p.take_due(t0 + ms(200));
        p.forget_owner();
        assert_eq!(p.target(), None);
        let next = CwEdgesPacket { target: 2, edges: vec![edge(4, true, 2000, true)], hang_ms: 0 };
        assert_eq!(p.ingest(a(1), &next, t0 + ms(2200), false).accepted, 1);
        assert_eq!(p.target(), Some(2));
    }

    /// Asked before anything is taken for a packet (a transmitter lock, MOX):
    /// is a run playing that this packet cannot join - another client's, or
    /// this client's on another radio? Without it the gates took the lock
    /// (and for Thetis raised MOX) and only the ingest refused, leaving the
    /// lock with a client that had no run (found reading step 6).
    #[test]
    fn a_packet_that_cannot_join_the_playing_run_is_known_before_anything_is_taken() {
        let (mut p, t0) = fresh();
        assert!(!p.busy_for(a(1), 0), "nothing plays");
        assert!(!p.busy_for(a(1), 1));
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true)]), t0, false); // a Thetis run of client 1
        assert!(!p.busy_for(a(1), 0), "its own run on its own radio");
        assert!(p.busy_for(a(1), 1), "its own run, but another radio");
        assert!(p.busy_for(a(2), 0), "another client, same radio");
        assert!(p.busy_for(a(2), 2), "another client, another radio");
        p.take_due(t0 + ms(100));
        p.release(a(1), t0 + ms(200));
        assert!(!p.busy_for(a(2), 1), "the run is over");
    }

    /// The thread's own lateness is measured, edge by edge, for the log.
    #[test]
    fn the_lateness_of_every_played_edge_is_measured() {
        let (mut p, t0) = fresh();
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true), edge(2, false, 48, false)]), t0, false);
        p.take_due(t0 + ms(80) + Duration::from_micros(300));
        assert_eq!((p.last_deviation_us, p.max_deviation_us), (300, 300));
        p.take_due(t0 + ms(128) + Duration::from_micros(100));
        assert_eq!((p.last_deviation_us, p.max_deviation_us), (100, 300), "the worst is kept");
    }

    #[test]
    fn sequence_numbers_survive_the_wrap() {
        assert!(newer(0, Some(u32::MAX)));
        assert!(!newer(u32::MAX, Some(0)));
        assert!(newer(5, Some(4)) && !newer(4, Some(4)) && !newer(3, Some(4)));
    }

    /// What the thread sleeps towards is the edge's own moment - the decision
    /// under "handed on at its moment", taken without a clock so that a loaded
    /// machine cannot make it flicker.
    ///
    /// This is the PLAN, not the waiting. It catches a deadline computed wrong;
    /// it does not catch a notify that never arrives, because `next_deadline`
    /// answers the same either way. Said plainly here because the comment that
    /// replaced the old ceiling claimed this test covered that, and it does not
    /// (review 2120A1 round 2).
    #[test]
    fn the_next_deadline_is_the_edges_own_moment() {
        let (mut p, t0) = fresh();
        assert_eq!(p.next_deadline(), None, "nothing planned, nothing to wake for");
        p.ingest(a(1), &pkt(&[edge(1, true, 0, true)]), t0, false);
        assert_eq!(
            p.next_deadline(),
            Some(t0 + PLAYOUT_DELAY),
            "the edge's own moment, and PLAYOUT_DELAY is what makes it that moment"
        );
        // A key held down brings the watchdog forward as the earlier of the two.
        let due = p.take_due(t0 + PLAYOUT_DELAY);
        assert_eq!(due.len(), 1);
        assert_eq!(
            p.next_deadline(),
            Some(t0 + PLAYOUT_DELAY + WATCHDOG),
            "nothing left to play, but a key that is down has a deadline of its own"
        );
    }

    /// The thread itself: an edge is handed on at its moment and not before.
    #[test]
    fn the_thread_hands_an_edge_on_at_its_moment() {
        let shared = new_handle();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let thread = spawn_playout_thread(Arc::clone(&shared), tx);
        let t0 = Instant::now();
        {
            let mut g = lock(&shared);
            g.ingest(a(1), &pkt(&[edge(1, true, 0, true)]), t0, false);
        }
        shared.wake.notify_all();
        // Bounded: a thread that never hands the edge on must fail this test,
        // not hang the suite (a mutant that made has_news say "never" did
        // exactly that to a build-105 mutation run, for twenty minutes).
        let e = loop {
            match rx.try_recv() {
                Ok(e) => break e,
                Err(_) if t0.elapsed() < Duration::from_secs(2) => std::thread::sleep(Duration::from_micros(200)),
                Err(_) => panic!("no edge within two seconds"),
            }
        };
        let took = Instant::now() - t0;
        assert!(e.down && e.planned == t0 + ms(80));
        assert!(took >= ms(80), "early: {took:?}");
        // And no ceiling here. There was one - 95 ms - and it was the third
        // flaky test of this cycle: the test review measured 8 failures in 15 runs under
        // CPU load, at 115-142 ms, and ruled out the Windows timer (a 2 ms
        // sleep measures 2.4 ms here). It is thread scheduling, which this
        // test does not control and cannot promise.
        //
        // BE HONEST ABOUT WHAT WENT WITH IT. That ceiling was also the only
        // thing that would have caught a LOST WAKE-UP - the condvar handing
        // back the lock without the notify arriving, so the edge waits for the
        // 100 ms poll instead of its own moment. No test covers that any more,
        // and a clockless one cannot: the defect costs ~100 ms and the
        // scheduling noise under load reaches 62 ms, so a single time
        // assertion is either flaky or blind (review 2120A1 round 2, the review,
        // who looked for one and did not find it either).
        //
        // What covers it instead is a line in a field log, named here so it
        // can be looked for rather than remembered (working rule 11):
        //
        //     cw playout: N edges played, worst deviation so far X.XX ms
        //
        // written by the send task in network.rs. In the field that reads 0.28
        // ms; a lost wake-up would put it past 100. `next_deadline` - the
        // decision the thread sleeps towards - is pinned without a clock in
        // `the_next_deadline_is_the_edges_own_moment` below, but that is the
        // plan, not the waiting, and the wake-up lives in the waiting.
        //
        // The bound that keeps a stalled thread from hanging the suite is the
        // two seconds in the loop above.
        drop(rx);
        shared.wake.notify_all();
        let _ = thread.join();
    }
}
