// SPDX-License-Identifier: GPL-2.0-or-later

//! The CW keyer: paddle contacts in, key edges out.
//!
//! This is step 1 of `docs/internal/DESIGN-remote-cw.md`. It lives in the
//! shared crate so that every front end keys the same way, and it is pure: the
//! caller samples the contacts (the MCP2221A reads all of them in one
//! transaction, about every 2 ms) and hands them in with a timestamp; the keyer
//! answers with the edges that fell due, each stamped with the moment it was
//! *planned* for, not the moment the sample happened to arrive. A late sample
//! therefore never stretches an element: the next edge is always
//! `previous edge + duration`, and the duration is the PARIS rule,
//! `1200 / wpm` ms per dot, dash = 3 dots, gap = 1 dot.
//!
//! Three things a sampled input has to make up for, and how:
//!
//! - **Bounce**: a contact counts as changed only after it has read the same
//!   way for `debounce_ms`. The paddle's timing does not suffer from that,
//!   because elements are planned, not measured.
//! - **A tap between two elements**: the other paddle, touched while an
//!   element plays, is remembered (dot memory / dash memory) and played after
//!   the gap, even if the contact was open again by the time the gap ended.
//! - **A floating input**: measured on 2026-09-17, an unconnected GP pin
//!   follows its neighbour in every transition. So every input is off until
//!   the operator enables it; a disabled contact is never read.
//!
//! Iambic A and B differ in one place only: how the memory latches. In A
//! (the Curtis 8044) the other paddle is remembered only when it goes from
//! open to closed *during* the element - held from before the element it
//! does not count, so letting a squeeze go stops after the element that is
//! playing. In B (the Accu-Keyer) the other paddle is remembered whenever it
//! is closed during the element, so letting a squeeze go plays one more
//! element of the opposite kind. Source: morsecode.world/iambic.html, which
//! matches the Curtis 8044ABM application note.
//!
//! Edges never share a millisecond: a planned edge and a straight-key edge
//! that fall in the same one cancel (the key simply stays where it was), and
//! an edge that would land on or before the previous one is pushed to the
//! next millisecond. That last case only arises across a `reset`.

/// How the other paddle is remembered while an element plays - which is the
/// whole difference between the two modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaddleMode {
    /// Remembered only when it closes during the element (Curtis 8044): a
    /// squeeze let go stops after the element that is playing.
    IambicA,
    /// Remembered whenever it is closed during the element (Accu-Keyer): a
    /// squeeze let go adds one element of the opposite kind.
    IambicB,
}

/// How long a contact has to read the same way before it counts. One place,
/// used by `Default` and by the tests' grid arithmetic, so the two cannot
/// drift apart (review finding on build 86: on a 2 ms grid, 3 and 4 were
/// indistinguishable and nothing pinned the number).
pub const DEBOUNCE_MS: u32 = 3;

/// The operator's settings. `Default` is what the design chose: iambic B,
/// left = dot, 25 wpm, the paddle on, the straight key off until it is wired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyerSettings {
    pub mode: PaddleMode,
    /// Swap the paddles: contact A makes dashes, contact B makes dots.
    pub reversed: bool,
    /// Words per minute, PARIS. Clamped to 4..=60 when used.
    pub wpm: u8,
    /// Whether contacts A and B are read at all.
    pub paddle_enabled: bool,
    /// Whether the straight-key contact is read at all.
    pub straight_enabled: bool,
    /// A contact has to read the same way for this long before it counts.
    pub debounce_ms: u32,
}

impl Default for KeyerSettings {
    fn default() -> Self {
        Self {
            mode: PaddleMode::IambicB,
            reversed: false,
            wpm: 25,
            paddle_enabled: true,
            straight_enabled: false,
            debounce_ms: DEBOUNCE_MS,
        }
    }
}

/// One sample of the three contacts. `true` = closed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Contacts {
    pub a: bool,
    pub b: bool,
    pub straight: bool,
}

impl Contacts {
    pub const OPEN: Contacts = Contacts { a: false, b: false, straight: false };
    pub const A: Contacts = Contacts { a: true, b: false, straight: false };
    pub const B: Contacts = Contacts { a: false, b: true, straight: false };
    pub const SQUEEZE: Contacts = Contacts { a: true, b: true, straight: false };
    pub const STRAIGHT: Contacts = Contacts { a: false, b: false, straight: true };
}

/// A key edge: the key went down or up at `at_ms`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    pub down: bool,
    pub at_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Element {
    Dot,
    Dash,
}

impl Element {
    fn opposite(self) -> Element {
        match self {
            Element::Dot => Element::Dash,
            Element::Dash => Element::Dot,
        }
    }
    fn len_dots(self) -> u64 {
        match self {
            Element::Dot => 1,
            Element::Dash => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    /// The key is down for `kind`, until `ends_at`.
    Element { kind: Element, ends_at: u64 },
    /// The one-dot pause after `after`, until `ends_at`.
    Gap { after: Element, ends_at: u64 },
}

/// One contact, debounced.
#[derive(Clone, Copy, Debug, Default)]
struct Debounced {
    stable: bool,
    candidate: bool,
    since_ms: u64,
}

impl Debounced {
    fn feed(&mut self, raw: bool, now_ms: u64, hold_ms: u64) -> bool {
        if raw != self.candidate {
            self.candidate = raw;
            self.since_ms = now_ms;
        }
        if self.candidate != self.stable && now_ms.saturating_sub(self.since_ms) >= hold_ms {
            self.stable = self.candidate;
        }
        self.stable
    }
}

/// The keyer. Feed it samples; collect edges.
#[derive(Clone, Debug)]
pub struct Keyer {
    settings: KeyerSettings,
    a: Debounced,
    b: Debounced,
    straight: Debounced,
    phase: Phase,
    /// The other paddle was seen during the current element or gap - on a
    /// transition (A) or at any level (B), see `PaddleMode`.
    mem_dot: bool,
    mem_dash: bool,
    /// The debounced paddle levels of the previous sample, for the transition.
    prev_dot: bool,
    prev_dash: bool,
    keyer_down: bool,
    straight_down: bool,
    out_down: bool,
    /// The last millisecond an edge occupied - handed out, or cancelled in
    /// place - so no edge ever lands on or before it.
    last_edge_ms: Option<u64>,
}

impl Keyer {
    pub fn new(settings: KeyerSettings) -> Self {
        Self {
            settings,
            a: Debounced::default(),
            b: Debounced::default(),
            straight: Debounced::default(),
            phase: Phase::Idle,
            mem_dot: false,
            mem_dash: false,
            prev_dot: false,
            prev_dash: false,
            keyer_down: false,
            straight_down: false,
            out_down: false,
            last_edge_ms: None,
        }
    }

    pub fn settings(&self) -> &KeyerSettings {
        &self.settings
    }

    /// Change the settings. Takes effect at the next element; the one that is
    /// playing keeps the length it was planned with.
    pub fn set_settings(&mut self, settings: KeyerSettings) {
        if settings.reversed != self.settings.reversed {
            // The remembered levels belong to the paddles, not to "dot" and
            // "dash". Left as they were, a paddle held across the swap read
            // as one that had just closed, and iambic A latched a memory
            // nobody had keyed.
            std::mem::swap(&mut self.prev_dot, &mut self.prev_dash);
        }
        self.settings = settings;
    }

    /// Whether the key is down right now, as far as the last sample knows.
    pub fn is_down(&self) -> bool {
        self.out_down
    }

    /// Length of one dot at the current speed.
    pub fn dot_ms(&self) -> u64 {
        1200 / u64::from(self.settings.wpm.clamp(4, 60))
    }

    /// Drop everything: memories, the element in flight, the debouncers. If
    /// the key was down, the returned edge lifts it at `now_ms`. For the day
    /// the link, the radio or the operator goes away mid-character.
    pub fn reset(&mut self, now_ms: u64) -> Option<Edge> {
        let was_down = self.out_down;
        let last = self.last_edge_ms;
        *self = Keyer::new(self.settings);
        if was_down {
            let at_ms = Self::after(last, now_ms);
            self.last_edge_ms = Some(at_ms);
            Some(Edge { down: false, at_ms })
        } else {
            self.last_edge_ms = last;
            None
        }
    }

    /// `at_ms`, or the millisecond after the last edge if that is later.
    fn after(last: Option<u64>, at_ms: u64) -> u64 {
        match last {
            Some(l) if at_ms <= l => l + 1,
            _ => at_ms,
        }
    }

    /// One sample of the contacts at `now_ms`. Returns the edges that fell
    /// due, oldest first. Usually none or one; more when the caller was late.
    pub fn sample(&mut self, contacts: Contacts, now_ms: u64) -> Vec<Edge> {
        let hold = u64::from(self.settings.debounce_ms);
        let (raw_a, raw_b) = if self.settings.paddle_enabled {
            (contacts.a, contacts.b)
        } else {
            (false, false)
        };
        let raw_s = self.settings.straight_enabled && contacts.straight;
        let a = self.a.feed(raw_a, now_ms, hold);
        let b = self.b.feed(raw_b, now_ms, hold);
        let straight = self.straight.feed(raw_s, now_ms, hold);
        let (dot_paddle, dash_paddle) = if self.settings.reversed { (b, a) } else { (a, b) };
        // What counts as "seen" for the memory: in A a paddle that closed
        // since the last sample, in B a paddle that is closed.
        let (dot_seen, dash_seen) = match self.settings.mode {
            PaddleMode::IambicA => (dot_paddle && !self.prev_dot, dash_paddle && !self.prev_dash),
            PaddleMode::IambicB => (dot_paddle, dash_paddle),
        };
        self.prev_dot = dot_paddle;
        self.prev_dash = dash_paddle;

        let mut edges = Vec::new();

        // First play out everything that ended at or before now, at its
        // planned time. Each decision at a gap's end reads the paddles as
        // this sample sees them: every phase end handled here lies after the
        // previous sample, so a press first seen now may have been there.
        // Only after that is the sample read against the phase that is
        // current *now* - never against one that had already ended when the
        // sample arrived (review finding on build 86: a late sample used to
        // latch a fresh press as "touched during" an element that was over).
        loop {
            match self.phase {
                Phase::Element { kind, ends_at } if ends_at <= now_ms => {
                    self.set_keyer(false, ends_at, &mut edges);
                    self.phase = Phase::Gap { after: kind, ends_at: ends_at + self.dot_ms() };
                }
                Phase::Gap { after, ends_at } if ends_at <= now_ms => {
                    match self.next_after(after, dot_paddle, dash_paddle) {
                        Some(kind) => self.start(kind, ends_at, &mut edges),
                        None => self.phase = Phase::Idle,
                    }
                }
                _ => break,
            }
        }

        // The other paddle, seen while an element or its gap plays, is
        // remembered, so a tap that is over before the gap ends still counts.
        // The gap is included on purpose: the source only speaks of the
        // element, but a tap that falls entirely inside a 48 ms gap is a
        // deliberate press and losing it would read as a missed element.
        let playing = match self.phase {
            Phase::Element { kind, .. } => Some(kind),
            Phase::Gap { after, .. } => Some(after),
            Phase::Idle => None,
        };
        if let Some(current) = playing {
            if dot_seen && current == Element::Dash {
                self.mem_dot = true;
            }
            if dash_seen && current == Element::Dot {
                self.mem_dash = true;
            }
        }

        // From rest, a paddle starts an element now. Both at once: dot first.
        if self.phase == Phase::Idle {
            if dot_paddle {
                self.start(Element::Dot, now_ms, &mut edges);
            } else if dash_paddle {
                self.start(Element::Dash, now_ms, &mut edges);
            }
        }

        // The straight key goes around the keyer; the two are OR-ed.
        if straight != self.straight_down {
            self.straight_down = straight;
            self.emit(now_ms, &mut edges);
        }

        edges
    }

    /// What follows the gap after `after`: the remembered opposite element if
    /// there is one, else the opposite of `after` for a squeeze, else the
    /// paddle that is held, else nothing. The same in A and B - the modes
    /// differ only in what got remembered (see `sample`).
    fn next_after(&mut self, after: Element, dot_paddle: bool, dash_paddle: bool) -> Option<Element> {
        let remembered = if self.mem_dot && after == Element::Dash {
            Some(Element::Dot)
        } else if self.mem_dash && after == Element::Dot {
            Some(Element::Dash)
        } else {
            None
        };
        let next = if remembered.is_some() {
            remembered
        } else if dot_paddle && dash_paddle {
            Some(after.opposite())
        } else if dot_paddle {
            Some(Element::Dot)
        } else if dash_paddle {
            Some(Element::Dash)
        } else {
            None
        };
        if next.is_none() {
            self.mem_dot = false;
            self.mem_dash = false;
        }
        next
    }

    fn start(&mut self, kind: Element, at_ms: u64, edges: &mut Vec<Edge>) {
        self.mem_dot = false;
        self.mem_dash = false;
        self.phase = Phase::Element { kind, ends_at: at_ms + kind.len_dots() * self.dot_ms() };
        self.set_keyer(true, at_ms, edges);
    }

    fn set_keyer(&mut self, down: bool, at_ms: u64, edges: &mut Vec<Edge>) {
        self.keyer_down = down;
        self.emit(at_ms, edges);
    }

    /// The output is the OR of the keyer and the straight key; an edge only
    /// when that changes. Two edges in the same millisecond are, by
    /// alternation, opposite, so they cancel: the key stays where it was.
    fn emit(&mut self, at_ms: u64, edges: &mut Vec<Edge>) {
        let down = self.keyer_down || self.straight_down;
        if down == self.out_down {
            return;
        }
        self.out_down = down;
        if let Some(last) = edges.last() {
            if last.at_ms == at_ms {
                edges.pop();
                self.last_edge_ms = edges.last().map(|e| e.at_ms).or(self.last_edge_ms);
                return;
            }
        }
        let at_ms = Self::after(self.last_edge_ms, at_ms);
        self.last_edge_ms = Some(at_ms);
        edges.push(Edge { down, at_ms });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEP: u64 = 2; // the measured MCP2221A read interval, roughly
    const DOT: u64 = 48; // 25 wpm
    /// The first sample on the grid at which a press from t=0 has been held
    /// for the debounce: 4 at STEP 2, 3 at STEP 1, 7 at STEP 7. Every
    /// expectation below is written against this, so the tests say the same
    /// thing at any sampling rate (green at STEP 1 through 8; above that the
    /// 10 ms test presses themselves fall between samples).
    const D: u64 = STEP * ((DEBOUNCE_MS as u64 + STEP - 1) / STEP);

    fn keyer() -> Keyer {
        Keyer::new(KeyerSettings::default())
    }

    fn with(mode: PaddleMode) -> Keyer {
        Keyer::new(KeyerSettings { mode, ..KeyerSettings::default() })
    }

    /// Hold `contacts` from `from` up to (not including) `to`, sampling every
    /// STEP ms, and collect the edges.
    fn hold(k: &mut Keyer, contacts: Contacts, from: u64, to: u64) -> Vec<Edge> {
        let mut out = Vec::new();
        let mut t = from;
        while t < to {
            out.extend(k.sample(contacts, t));
            t += STEP;
        }
        out
    }

    /// The edges as (down, at) pairs, easier to read in an assertion.
    fn pairs(edges: &[Edge]) -> Vec<(bool, u64)> {
        edges.iter().map(|e| (e.down, e.at_ms)).collect()
    }

    /// Down/up strictly alternating, starting with down, never going back in time.
    fn well_formed(edges: &[Edge]) {
        let mut expect_down = true;
        let mut last = 0;
        for e in edges {
            assert_eq!(e.down, expect_down, "edges must alternate: {:?}", pairs(edges));
            assert!(e.at_ms >= last, "edges must not go back in time: {:?}", pairs(edges));
            expect_down = !expect_down;
            last = e.at_ms;
        }
    }

    #[test]
    fn a_dot_is_one_dot_long_whatever_the_sampling_did() {
        let mut k = keyer();
        // Press A at t=100 (seen at the first sample), let go well before the dot ends.
        let mut edges = hold(&mut k, Contacts::A, 100, 110);
        edges.extend(hold(&mut k, Contacts::OPEN, 110, 400));
        well_formed(&edges);
        // Debounce (3 ms) means the press counts D ms after it was first seen.
        assert_eq!(pairs(&edges), vec![(true, 100 + D), (false, 100 + D + DOT)]);
    }

    #[test]
    fn a_dash_is_three_dots() {
        let mut k = keyer();
        let mut edges = hold(&mut k, Contacts::B, 0, 10);
        edges.extend(hold(&mut k, Contacts::OPEN, 10, 400));
        assert_eq!(pairs(&edges), vec![(true, D), (false, D + 3 * DOT)]);
    }

    #[test]
    fn a_held_paddle_repeats_with_one_dot_gaps() {
        let mut k = keyer();
        let edges = hold(&mut k, Contacts::A, 0, 5 * DOT);
        well_formed(&edges);
        let d = D;
        assert_eq!(
            pairs(&edges),
            vec![(true, d), (false, d + DOT), (true, d + 2 * DOT), (false, d + 3 * DOT), (true, d + 4 * DOT)]
        );
    }

    #[test]
    fn a_squeeze_alternates_and_starts_with_a_dot() {
        let mut k = keyer();
        let edges = hold(&mut k, Contacts::SQUEEZE, 0, 8 * DOT);
        well_formed(&edges);
        let d = D;
        // dot (1) gap (1) dash (3) gap (1) dot ...
        assert_eq!(
            pairs(&edges),
            vec![(true, d), (false, d + DOT), (true, d + 2 * DOT), (false, d + 5 * DOT), (true, d + 6 * DOT), (false, d + 7 * DOT)]
        );
    }

    #[test]
    fn a_squeeze_that_began_on_the_dash_side_starts_with_a_dash() {
        let mut k = keyer();
        let mut edges = hold(&mut k, Contacts::B, 0, 20);
        edges.extend(hold(&mut k, Contacts::SQUEEZE, 20, 6 * DOT));
        well_formed(&edges);
        let d = D;
        assert_eq!(pairs(&edges)[..4], [(true, d), (false, d + 3 * DOT), (true, d + 4 * DOT), (false, d + 5 * DOT)]);
    }

    /// The dot memory: a tap on A while a dash plays, over long before the gap
    /// ends, still produces the dot after the gap.
    #[test]
    fn a_tap_during_a_dash_is_remembered_and_played_after_the_gap_in_both_modes() {
        for mode in [PaddleMode::IambicA, PaddleMode::IambicB] {
            a_tap_during_a_dash_is_remembered(mode);
        }
    }

    fn a_tap_during_a_dash_is_remembered(mode: PaddleMode) {
        let mut k = with(mode);
        let mut edges = hold(&mut k, Contacts::B, 0, 20);
        // Dash runs 4..148. Tap A for 10 ms in the middle, B let go too.
        edges.extend(hold(&mut k, Contacts::OPEN, 20, 60));
        edges.extend(hold(&mut k, Contacts::A, 60, 70));
        edges.extend(hold(&mut k, Contacts::OPEN, 70, 400));
        well_formed(&edges);
        let d = D;
        assert_eq!(
            pairs(&edges),
            vec![(true, d), (false, d + 3 * DOT), (true, d + 4 * DOT), (false, d + 5 * DOT)],
            "{mode:?}"
        );
    }

    #[test]
    fn a_tap_during_the_gap_counts_too_in_both_modes() {
        for mode in [PaddleMode::IambicA, PaddleMode::IambicB] {
            a_tap_during_the_gap_counts(mode);
        }
    }

    fn a_tap_during_the_gap_counts(mode: PaddleMode) {
        let mut k = with(mode);
        let mut edges = hold(&mut k, Contacts::A, 0, 10);
        // Dot D..D+48, gap to D+96. Tap B for 20 ms inside the gap: a tap is
        // seen when it spans one sample plus the debounce hold, so a real
        // tap (20 ms and up) is safe at any sampling rate up to about 15 ms.
        edges.extend(hold(&mut k, Contacts::OPEN, 10, 60));
        edges.extend(hold(&mut k, Contacts::B, 60, 80));
        edges.extend(hold(&mut k, Contacts::OPEN, 80, 400));
        let d = D;
        assert_eq!(pairs(&edges), vec![(true, d), (false, d + DOT), (true, d + 2 * DOT), (false, d + 5 * DOT)], "{mode:?}");
    }

    /// Iambic A: let go of both mid-dash, the dash finishes and that is all.
    #[test]
    fn iambic_a_stops_after_the_element_in_flight() {
        let mut k = with(PaddleMode::IambicA);
        let mut edges = hold(&mut k, Contacts::SQUEEZE, 0, 80); // dot 4..52, dash starts 100
        edges.extend(hold(&mut k, Contacts::SQUEEZE, 80, 120)); // into the dash
        edges.extend(hold(&mut k, Contacts::OPEN, 120, 600));
        well_formed(&edges);
        let d = D;
        assert_eq!(pairs(&edges), vec![(true, d), (false, d + DOT), (true, d + 2 * DOT), (false, d + 5 * DOT)]);
    }

    /// Iambic A keeps the tap memory as long as a paddle is still held: hold
    /// B, tap A during the dash, keep holding B - dot, then dash again.
    #[test]
    fn iambic_a_keeps_the_dot_memory_while_a_paddle_is_still_held() {
        let mut k = with(PaddleMode::IambicA);
        let mut edges = hold(&mut k, Contacts::B, 0, 60); // dash 4..148
        edges.extend(hold(&mut k, Contacts::SQUEEZE, 60, 70)); // the tap
        edges.extend(hold(&mut k, Contacts::B, 70, 10 * DOT));
        well_formed(&edges);
        let d = D;
        assert_eq!(
            pairs(&edges)[..6],
            [(true, d), (false, d + 3 * DOT), (true, d + 4 * DOT), (false, d + 5 * DOT), (true, d + 6 * DOT), (false, d + 9 * DOT)]
        );
    }

    /// Iambic A, the case the source singles out: the dot paddle held from
    /// before the dash does not latch; touched during the dash it does.
    #[test]
    fn iambic_a_remembers_a_touch_but_not_a_paddle_held_from_before() {
        // Held from before: squeeze from rest, let go during the dash -> stop.
        let mut k = with(PaddleMode::IambicA);
        let mut edges = hold(&mut k, Contacts::SQUEEZE, 0, 120);
        edges.extend(hold(&mut k, Contacts::OPEN, 120, 600));
        let d = D;
        assert_eq!(pairs(&edges), vec![(true, d), (false, d + DOT), (true, d + 2 * DOT), (false, d + 5 * DOT)]);
        // Touched during: hold B, tap A mid-dash, let everything go -> the dot still comes.
        let mut k = with(PaddleMode::IambicA);
        let mut edges = hold(&mut k, Contacts::B, 0, 60);
        edges.extend(hold(&mut k, Contacts::SQUEEZE, 60, 70));
        edges.extend(hold(&mut k, Contacts::OPEN, 70, 600));
        well_formed(&edges);
        assert_eq!(pairs(&edges), vec![(true, d), (false, d + 3 * DOT), (true, d + 4 * DOT), (false, d + 5 * DOT)]);
    }

    /// The straight key closing in the very millisecond a planned element ends:
    /// no edges at all, the key simply stays down.
    #[test]
    fn a_planned_edge_and_a_straight_edge_in_the_same_millisecond_cancel() {
        let mut k = Keyer::new(KeyerSettings { straight_enabled: true, debounce_ms: 0, ..KeyerSettings::default() });
        let mut edges = k.sample(Contacts::A, 0); // dot 0..48
        edges.extend(k.sample(Contacts::A, 20));
        edges.extend(k.sample(Contacts { a: true, b: false, straight: true }, 48)); // up at 48 meets down at 48
        assert_eq!(pairs(&edges), vec![(true, 0)]);
        assert!(k.is_down());
        edges.extend(k.sample(Contacts::STRAIGHT, 60)); // A let go; gap ends 96, nothing follows
        edges.extend(k.sample(Contacts::OPEN, 120));
        well_formed(&edges);
        assert_eq!(pairs(&edges), vec![(true, 0), (false, 120)]);
    }

    /// Across a reset, an edge that would share the millisecond is pushed on by one.
    #[test]
    fn no_edge_lands_on_or_before_the_previous_one_across_a_reset() {
        let mut k = Keyer::new(KeyerSettings { debounce_ms: 0, ..KeyerSettings::default() });
        let mut edges = k.sample(Contacts::A, 10);
        edges.extend(k.reset(20));
        edges.extend(k.sample(Contacts::A, 20));
        well_formed(&edges);
        assert_eq!(pairs(&edges), vec![(true, 10), (false, 20), (true, 21)]);
    }

    /// Iambic B: the same release adds one dot after the dash.
    #[test]
    fn iambic_b_adds_one_opposite_element_after_a_squeeze_ends() {
        let mut k = with(PaddleMode::IambicB);
        let mut edges = hold(&mut k, Contacts::SQUEEZE, 0, 120);
        edges.extend(hold(&mut k, Contacts::OPEN, 120, 600));
        well_formed(&edges);
        let d = D;
        assert_eq!(
            pairs(&edges),
            vec![(true, d), (false, d + DOT), (true, d + 2 * DOT), (false, d + 5 * DOT), (true, d + 6 * DOT), (false, d + 7 * DOT)]
        );
    }

    /// Mode B completes only a squeeze: a single paddle let go mid-element
    /// adds nothing.
    #[test]
    fn iambic_b_does_not_add_anything_after_a_single_paddle() {
        let mut k = with(PaddleMode::IambicB);
        let mut edges = hold(&mut k, Contacts::B, 0, 60);
        edges.extend(hold(&mut k, Contacts::OPEN, 60, 600));
        assert_eq!(pairs(&edges), vec![(true, D), (false, D + 3 * DOT)]);
    }

    #[test]
    fn reversed_paddles_swap_dot_and_dash() {
        let mut k = Keyer::new(KeyerSettings { reversed: true, ..KeyerSettings::default() });
        let mut edges = hold(&mut k, Contacts::A, 0, 10);
        edges.extend(hold(&mut k, Contacts::OPEN, 10, 400));
        assert_eq!(pairs(&edges), vec![(true, D), (false, D + 3 * DOT)], "A is now the dash");
    }

    #[test]
    fn the_straight_key_goes_straight_through_after_the_debounce() {
        let mut k = Keyer::new(KeyerSettings { straight_enabled: true, ..KeyerSettings::default() });
        let mut edges = hold(&mut k, Contacts::STRAIGHT, 0, 70);
        edges.extend(hold(&mut k, Contacts::OPEN, 70, 100));
        well_formed(&edges);
        assert_eq!(pairs(&edges), vec![(true, D), (false, 70 + D)]);
    }

    #[test]
    fn a_disabled_straight_key_is_never_read() {
        let mut k = keyer(); // default: straight off
        let edges = hold(&mut k, Contacts::STRAIGHT, 0, 200);
        assert!(edges.is_empty(), "a floating GP2 must not key: {:?}", pairs(&edges));
    }

    #[test]
    fn a_disabled_paddle_is_never_read() {
        let mut k = Keyer::new(KeyerSettings { paddle_enabled: false, ..KeyerSettings::default() });
        let edges = hold(&mut k, Contacts::SQUEEZE, 0, 200);
        assert!(edges.is_empty());
    }

    #[test]
    fn straight_key_and_paddle_are_or_ed() {
        let mut k = Keyer::new(KeyerSettings { straight_enabled: true, ..KeyerSettings::default() });
        // Straight key held the whole time; a dot in the middle changes nothing.
        let mut edges = hold(&mut k, Contacts::STRAIGHT, 0, 100);
        edges.extend(hold(&mut k, Contacts { a: true, b: false, straight: true }, 100, 110));
        edges.extend(hold(&mut k, Contacts::STRAIGHT, 110, 300));
        edges.extend(hold(&mut k, Contacts::OPEN, 300, 320));
        well_formed(&edges);
        assert_eq!(pairs(&edges), vec![(true, D), (false, 300 + D)]);
    }

    /// A single wrong sample (2 ms) is bounce, not a press.
    #[test]
    fn one_sample_of_bounce_is_ignored() {
        let mut k = keyer();
        let mut edges = k.sample(Contacts::A, 0);
        edges.extend(hold(&mut k, Contacts::OPEN, 2, 200));
        assert!(edges.is_empty(), "{:?}", pairs(&edges));
    }

    /// A late sample (the reader stalled) still yields the planned edges at
    /// their planned times, in order.
    #[test]
    fn a_late_sample_yields_the_planned_edges_at_their_planned_times() {
        let mut k = keyer();
        let mut edges = hold(&mut k, Contacts::A, 0, 10); // dot starts at 4
        // Nothing sampled for 200 ms, then one sample with A still held.
        edges.extend(k.sample(Contacts::A, 210));
        well_formed(&edges);
        let d = D;
        // dot 4..52, gap to 100, dot 100..148, gap to 196, dot at 196 (still playing at 210)
        assert_eq!(
            pairs(&edges),
            vec![(true, d), (false, d + DOT), (true, d + 2 * DOT), (false, d + 3 * DOT), (true, d + 4 * DOT)]
        );
    }

    #[test]
    fn a_speed_change_applies_to_the_next_element() {
        let mut k = keyer();
        let mut edges = hold(&mut k, Contacts::A, 0, 30); // dot 4..52 at 25 wpm
        let mut s = *k.settings();
        s.wpm = 50; // dot = 24
        k.set_settings(s);
        edges.extend(hold(&mut k, Contacts::A, 30, 200));
        well_formed(&edges);
        let d = D;
        // The running dot keeps 48; the gap (planned when the dot ended) is 24; next dot 24.
        assert_eq!(pairs(&edges)[..4], [(true, d), (false, d + 48), (true, d + 48 + 24), (false, d + 48 + 48)]);
    }

    #[test]
    fn speed_is_the_paris_rule_and_clamped() {
        let mut s = KeyerSettings::default();
        s.wpm = 25;
        assert_eq!(Keyer::new(s).dot_ms(), 48);
        s.wpm = 60;
        assert_eq!(Keyer::new(s).dot_ms(), 20);
        s.wpm = 0;
        assert_eq!(Keyer::new(s).dot_ms(), 300, "clamped to 4 wpm");
        s.wpm = 200;
        assert_eq!(Keyer::new(s).dot_ms(), 20, "clamped to 60 wpm");
    }

    #[test]
    fn reset_lifts_the_key_and_forgets_everything() {
        let mut k = keyer();
        let _ = hold(&mut k, Contacts::SQUEEZE, 0, 30);
        assert!(k.is_down());
        assert_eq!(k.reset(31), Some(Edge { down: false, at_ms: 31 }));
        assert!(!k.is_down());
        assert_eq!(k.reset(32), None, "nothing to lift the second time");
        // And the squeeze memory is gone: nothing plays by itself.
        assert!(hold(&mut k, Contacts::OPEN, 32, 400).is_empty());
    }

    /// A planned edge is handed out at the first sample on or after its planned
    /// time - never before it, and never a whole sample interval after it. This
    /// is the bound the reader's "late N ms" log figure lives by, and it is a
    /// property of the keyer, not of the board (review finding on build 90).
    #[test]
    fn no_edge_is_handed_out_later_than_one_sample_interval() {
        for step in [1u64, 2, 5, 7] {
            let mut k = keyer();
            let (mut worst, mut t) = (0u64, 0u64);
            while t < 20 * DOT {
                for e in k.sample(Contacts::SQUEEZE, t) {
                    assert!(e.at_ms <= t, "step {step}: an edge from the future at {t}: {e:?}");
                    worst = worst.max(t - e.at_ms);
                }
                t += step;
            }
            assert!(worst < step, "step {step}: an edge was {worst} ms late");
        }
    }

    /// The straight key switched off while it is held down: the key comes up
    /// after the debounce and stays up. An input that is off is not read.
    #[test]
    fn switching_the_straight_key_off_while_it_is_down_lifts_it() {
        let mut k = Keyer::new(KeyerSettings { straight_enabled: true, ..KeyerSettings::default() });
        let mut edges = hold(&mut k, Contacts::STRAIGHT, 0, 50);
        let mut s = *k.settings();
        s.straight_enabled = false;
        k.set_settings(s);
        edges.extend(hold(&mut k, Contacts::STRAIGHT, 50, 300));
        well_formed(&edges);
        assert_eq!(pairs(&edges), vec![(true, D), (false, 50 + D)]);
    }

    /// The paddles swapped while one is held: no element nobody keyed. In
    /// iambic A the held paddle must not read as one that just closed.
    #[test]
    fn swapping_the_paddles_while_one_is_held_makes_nothing_up() {
        let mut k = with(PaddleMode::IambicA);
        let mut edges = hold(&mut k, Contacts::A, 0, 30); // a dot, D..D+48
        let mut s = *k.settings();
        s.reversed = true;
        k.set_settings(s);
        edges.extend(hold(&mut k, Contacts::A, 30, 40)); // still held, now the dash paddle
        edges.extend(hold(&mut k, Contacts::OPEN, 40, 600));
        well_formed(&edges);
        assert_eq!(pairs(&edges), vec![(true, D), (false, D + DOT)], "the dot in flight, and nothing after it");
    }

    /// Iambic B switched to A while a squeeze is held: the paddles were down
    /// from before, so in A they latch nothing, and letting go stops after
    /// the element in flight. What this pins is that the paddle levels are
    /// remembered in BOTH modes - tracking them in A only looks like a tidy-up
    /// and makes a held paddle read as one that just closed (review finding).
    #[test]
    fn switching_from_b_to_a_while_squeezing_makes_nothing_up() {
        let mut k = with(PaddleMode::IambicB);
        // The switch lands on the very sample where the dash begins. Anywhere
        // else proves nothing: in B the level latch has already set the memory,
        // so a wrongly-seen "just closed" adds nothing to it. Only the sample
        // that starts an element has just cleared the memory (the first version
        // of this test switched mid-gap, and the mutant it was written for
        // walked straight through it).
        let switch_at = D + 2 * DOT;
        let mut edges = hold(&mut k, Contacts::SQUEEZE, 0, switch_at);
        let mut s = *k.settings();
        s.mode = PaddleMode::IambicA;
        k.set_settings(s);
        edges.extend(hold(&mut k, Contacts::SQUEEZE, switch_at, D + 2 * DOT + 20)); // into the dash
        edges.extend(hold(&mut k, Contacts::OPEN, D + 2 * DOT + 20, 800));
        well_formed(&edges);
        let d = D;
        assert_eq!(pairs(&edges), vec![(true, d), (false, d + DOT), (true, d + 2 * DOT), (false, d + 5 * DOT)]);
    }

    /// The design's defaults, in one place, so a change is a visible diff.
    #[test]
    fn the_defaults_are_what_the_owner_chose() {
        let s = KeyerSettings::default();
        assert_eq!(s.mode, PaddleMode::IambicB);
        assert!(!s.reversed, "left = dot");
        assert!(s.paddle_enabled);
        assert!(!s.straight_enabled, "off until GP2 is wired: a floating input follows its neighbour");
        assert_eq!(s.wpm, 25);
        assert_eq!(s.debounce_ms, DEBOUNCE_MS);
        assert_eq!(DEBOUNCE_MS, 3, "on a 2 ms grid 3 and 4 look alike; this is what pins it");
    }

    /// A press first seen by a sample that arrived long after an element and
    /// its gap had ended is read against what is current now, not latched as
    /// "touched during" the element that was over. The dot still comes at the
    /// gap's end - the paddle is down at the decision - but nothing is
    /// remembered that nobody saw.
    #[test]
    fn a_late_sample_does_not_attribute_a_press_to_an_element_that_had_ended() {
        for mode in [PaddleMode::IambicA, PaddleMode::IambicB] {
            let mut k = Keyer::new(KeyerSettings { mode, debounce_ms: 0, ..KeyerSettings::default() });
            let mut edges = k.sample(Contacts::B, 0); // dash 0..144, gap to 192
            edges.extend(k.sample(Contacts::A, 200)); // nothing sampled in between
            well_formed(&edges);
            assert_eq!(pairs(&edges), vec![(true, 0), (false, 144), (true, 192)], "{mode:?}");
            assert!(!k.mem_dot && !k.mem_dash, "{mode:?}: no memory made up for an element that was over");
            // And the dot is the last thing: A let go, nothing follows it.
            edges.extend(k.sample(Contacts::OPEN, 300));
            assert_eq!(pairs(&edges), vec![(true, 0), (false, 144), (true, 192), (false, 240)], "{mode:?}");
        }
    }

    /// The rule a sampled input lives by: a tap is seen when a sample falls
    /// inside it at or after the debounce has elapsed, so a tap of D + 1 ms
    /// is seen and a tap of D ms is not - at any sampling rate.
    #[test]
    fn a_tap_is_seen_when_it_outlasts_the_debounce_on_the_grid() {
        for mode in [PaddleMode::IambicA, PaddleMode::IambicB] {
            for (len, seen) in [(D + 1, true), (D, false)] {
                let mut k = with(mode);
                let mut edges = hold(&mut k, Contacts::B, 0, 60); // dash D..D+144
                edges.extend(hold(&mut k, Contacts::SQUEEZE, 60, 60 + len)); // the tap on A
                edges.extend(hold(&mut k, Contacts::B, 60 + len, 60 + len + 1));
                edges.extend(hold(&mut k, Contacts::OPEN, 60 + len + 1, 600));
                well_formed(&edges);
                let d = D;
                let expected = if seen {
                    vec![(true, d), (false, d + 3 * DOT), (true, d + 4 * DOT), (false, d + 5 * DOT)]
                } else {
                    vec![(true, d), (false, d + 3 * DOT)]
                };
                assert_eq!(pairs(&edges), expected, "{mode:?}, tap of {len} ms, seen = {seen}");
            }
        }
    }
}
