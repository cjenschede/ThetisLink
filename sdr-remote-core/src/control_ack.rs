// SPDX-License-Identifier: GPL-2.0-or-later
//
// Acknowledged controls (phase 2b of the direct connection).
//
// A control the client sends - a setting, a button, a Yaesu PTT - was one UDP
// datagram with nothing behind it: lost on the way, nothing happened and
// nobody knew. Over the relay that was hidden by TCP; over a direct path it is
// not. Here each control carries a sequence number, the server says "got it",
// and the client sends it again until it does.
//
// The whole rule lives in this file, in two small types a test can drive
// without opening the engine's loop or the server's (review 86: the loops are
// where the suite never comes):
//
// - `ControlSender` (client): numbers what goes out, keeps what is not yet
//   acknowledged, says when to send it again and when to give up.
// - `ControlReceiver` (server, one per session): says whether a numbered
//   control is new and is to be applied, or a copy, or older than what was
//   already applied for the same control.
//
// The last one is what keeps the transmitter safe (review 86): a repeated
// "Yaesu PTT on" that arrives after "off" must not key the radio again.
// Removing duplicates by sequence number would not see that - the "on" was
// never applied under that number. So the server applies a control only if
// its number is newer than the last one it applied FOR THE SAME CONTROL.
//
// That rule is for a SETTING, where the last value is the one that counts. A
// few controls are an ACT - a press, a swap, a read - where every send counts
// on its own (reviews 87). Those are keyed by their own number: never replaced
// by a later one, never too old to apply, and a copy of the same number is
// still done once. `is_action` says which, and `numbered_control` applies it
// for the client and the server alike, so the two cannot disagree.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

/// Which control a numbered packet sets. For a setting, a repeat replaces an
/// older packet of the same key and the server orders by it; an act is its own
/// key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ControlKey {
    /// A `ControlPacket` that sets something: its control id.
    Control(u8),
    /// A `YaesuControlPacket`: slot and control.
    Yaesu { slot: u8, control: u8 },
    /// A `ControlPacket` that does something (`is_action`): its id, and the
    /// number of this one send.
    Action { id: u8, seq: u32 },
    /// A `YaesuControlPacket` that does something (`is_yaesu_action`): slot,
    /// control, and the number of this one send.
    YaesuAction { slot: u8, control: u8, seq: u32 },
}

impl ControlKey {
    /// An act, of either packet type: keyed by its own number.
    pub fn is_act(&self) -> bool {
        matches!(self, ControlKey::Action { .. } | ControlKey::YaesuAction { .. })
    }
}

/// A control where every send is an act of its own, not a setting - a second
/// one is a second press. The list of phase 2b (2026-10-01), read again
/// against the protocol in build 88: the swaps, the raw Yaesu buttons, and the
/// reads and writes the server starts on request.
///
/// `YaesuSelectVfo` is a setting for 0 (VFO A) and 1 (VFO B), and an act from
/// 2 up: 2 swaps A and B, 3 is the V/M button on both front ends, which the
/// server turns into a toggle between VFO and memory (`yaesu/poll.rs`). The
/// enum's doc names only 0-2; the value 3 was found in the round on build 88,
/// by reading what the front ends send rather than what the protocol says.
pub fn is_action(id: u8, value: u16) -> bool {
    use crate::protocol::ControlId as C;
    const ACTIONS: [C; 14] = [
        C::VfoSwap,
        C::YaesuReadMemories,
        C::YaesuWriteMemories,
        C::YaesuButton,
        C::YaesuReadMenus,
        C::YaesuSetMenu,
        C::DiversityAutoNull,
        C::DiversityRead,
        C::YaesuReadMemoryTones,
        C::Yaesu2ReadMemories,
        C::Yaesu2WriteMemories,
        C::Yaesu2Button,
        C::Yaesu2ReadMenus,
        C::Yaesu2SetMenu,
    ];
    if ACTIONS.iter().any(|a| *a as u8 == id) {
        return true;
    }
    (id == C::YaesuSelectVfo as u8 || id == C::Yaesu2SelectVfo as u8) && value >= 2
}

/// The same question for a typed Yaesu control. One today: the clarifier step,
/// whose value is a step in hertz - twice +10 Hz is +20 Hz (round on build 88).
/// `ClarClear` sets the offset to zero, and zero twice is zero: a setting.
pub fn is_yaesu_action(control: u8, _value: u16) -> bool {
    control == crate::protocol::YaesuCtrl::ClarStep as u8
}

/// How often a control is sent again before the client gives up on it.
pub const MAX_REPEATS: u32 = 4;
/// The first repeat waits at least this long, doubling after each.
pub const FIRST_REPEAT_MIN: Duration = Duration::from_millis(100);
/// And at most this long: a round trip measured wrongly - or not yet - must not
/// push a repeat out by seconds.
pub const FIRST_REPEAT_MAX: Duration = Duration::from_millis(500);
/// The longest a control can wait for its last repeat: the waits double from
/// at most `FIRST_REPEAT_MAX`, four times (1 + 2 + 4 + 8 = 15).
pub const REPEAT_WINDOW: Duration = Duration::from_millis(500 * 15);

struct Pending {
    key: ControlKey,
    seq: u32,
    sent_at: Instant,
    bytes: Vec<u8>,
    next_at: Instant,
    wait: Duration,
    repeats: u32,
}

/// What `due` hands back: packets to send again, and keys that were given up.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Due {
    pub resend: Vec<Vec<u8>>,
    pub given_up: Vec<ControlKey>,
}

/// The client side. One per engine; `session_ended` when a session ends.
pub struct ControlSender {
    next_seq: u32,
    /// The first number of this session: an acknowledgement counts only for a
    /// number from here up to `next_seq`.
    session_first: u32,
    pending: Vec<Pending>,
    server_acknowledges: bool,
    /// Counts since the last `take_counts`, for one log line per window.
    sent: u32,
    repeated: u32,
    given_up: u32,
}

impl Default for ControlSender {
    fn default() -> Self {
        Self::new()
    }
}

impl ControlSender {
    /// Numbers start at the clock, in milliseconds, and never reset for the
    /// life of the engine. A server that kept a session the client did not
    /// know it kept still sees newer numbers - and so does a server that kept
    /// the session of a PREVIOUS process behind the same address without
    /// noticing the restart: starting at 1 would make every control of the
    /// new process older than what the old one had applied, and none of them
    /// would be applied. Over its life a process sends far fewer than one
    /// control per millisecond, and a restart takes seconds, so the next one
    /// starts ahead. (A clock set back between the two runs undoes that; the
    /// server's restart detection on the heartbeat is still there behind it.)
    pub fn new() -> Self {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |d| d.as_millis() as u32);
        Self::starting_at(ms)
    }

    /// A sender whose first number is `seq` (zero is never sent).
    pub fn starting_at(seq: u32) -> Self {
        let first = seq.max(1);
        Self {
            next_seq: first,
            session_first: first,
            pending: Vec::new(),
            server_acknowledges: false,
            sent: 0,
            repeated: 0,
            given_up: 0,
        }
    }

    /// Number a control on its way out. `packet` is the control as it would go
    /// without a number - a `ControlPacket` or a `YaesuControlPacket`; the
    /// four-byte number is appended (an older server reads the first bytes and
    /// ignores the rest). The returned bytes are what to send. The key is read
    /// back from those bytes with `numbered_control`, the function the server
    /// uses, so a setting and an act are told apart in one place.
    ///
    /// Always kept, but repeated only once this session's server has shown it
    /// acknowledges - before that a repeat could do a button press twice on a
    /// server that never says "got it". Kept all the same, because the opening
    /// burst of a session (the subscriptions after a reconnect) goes out BEFORE
    /// the first acknowledgement can have arrived, and it is exactly what a lost
    /// packet hurts most. One entry per setting, so with a server that never
    /// acknowledges this stays as small as the number of controls; an act is
    /// kept until its repeats are over, and dropped then all the same.
    pub fn send(&mut self, packet: &[u8], now: Instant, rtt: Duration) -> Vec<u8> {
        let seq = self.next_seq;
        let mut bytes = Vec::with_capacity(packet.len() + 4);
        bytes.extend_from_slice(packet);
        bytes.extend_from_slice(&seq.to_be_bytes());
        let Some((key, _)) = crate::protocol::numbered_control(&bytes) else {
            // Not a control: sent as it is, without a number, and nothing to
            // repeat (round on build 88: it used to keep the number it was given).
            return packet.to_vec();
        };
        self.next_seq = self.next_seq.wrapping_add(1).max(1);
        self.sent += 1;
        // The latest value of a setting replaces an unacknowledged older one:
        // last value wins, and the older one need not be repeated. An act has a
        // key of its own, so nothing replaces it.
        self.pending.retain(|p| p.key != key);
        let wait = first_wait(rtt);
        self.pending.push(Pending { key, seq, sent_at: now, bytes: bytes.clone(), next_at: now + wait, wait, repeats: 0 });
        bytes
    }

    /// The server acknowledged `seq`. The first acknowledgement of a session
    /// switches repeating on; what was sent before it gets its first wait again
    /// from now, so the acknowledgements still on their way are not overtaken
    /// by repeats of the same burst.
    ///
    /// Only for a number this session sent (reviews 87): an acknowledgement
    /// still on its way from the previous session must not switch repeating on
    /// for a server that does not acknowledge - to that one a repeat is a
    /// second press.
    pub fn acknowledged(&mut self, seq: u32, now: Instant) {
        if !self.sent_this_session(seq) {
            return;
        }
        if !self.server_acknowledges {
            self.server_acknowledges = true;
            for p in &mut self.pending {
                p.next_at = now + p.wait;
            }
        }
        self.pending.retain(|p| p.seq != seq);
    }

    /// What has to go again now, and what is given up after `MAX_REPEATS`.
    /// Nothing while this session's server has not acknowledged anything.
    ///
    /// In that time an act older than `REPEAT_WINDOW` is let go: unlike a
    /// setting, nothing replaces it, so against a server that never
    /// acknowledges the queue would otherwise grow with every press. Past that
    /// window it would have been given up anyway.
    pub fn due(&mut self, now: Instant) -> Due {
        let mut due = Due::default();
        if !self.server_acknowledges {
            self.pending.retain(|p| {
                !p.key.is_act()
                    || now.saturating_duration_since(p.sent_at) < REPEAT_WINDOW
            });
            return due;
        }
        let mut keep = Vec::with_capacity(self.pending.len());
        for mut p in self.pending.drain(..) {
            if now < p.next_at {
                keep.push(p);
            } else if p.repeats >= MAX_REPEATS {
                due.given_up.push(p.key);
            } else {
                due.resend.push(p.bytes.clone());
                p.repeats += 1;
                p.wait *= 2;
                p.next_at = now + p.wait;
                keep.push(p);
            }
        }
        self.pending = keep;
        self.repeated += due.resend.len() as u32;
        self.given_up += due.given_up.len() as u32;
        due
    }

    /// The session is gone: nothing of it is repeated into the next one, and
    /// the next server has to show again that it acknowledges (review 86: this
    /// is per-session state in loop locals, which the session proof does not
    /// cover - `session_lost!()` calls this).
    pub fn session_ended(&mut self) {
        self.pending.clear();
        self.server_acknowledges = false;
        self.session_first = self.next_seq;
    }

    /// `seq` lies in [`session_first`, `next_seq`), counted with wrap.
    fn sent_this_session(&self, seq: u32) -> bool {
        seq.wrapping_sub(self.session_first) < self.next_seq.wrapping_sub(self.session_first)
    }

    pub fn server_acknowledges(&self) -> bool {
        self.server_acknowledges
    }

    pub fn unacknowledged(&self) -> usize {
        self.pending.len()
    }

    /// (sent, repeated, given up) since the last call, and reset.
    pub fn take_counts(&mut self) -> (u32, u32, u32) {
        let c = (self.sent, self.repeated, self.given_up);
        self.sent = 0;
        self.repeated = 0;
        self.given_up = 0;
        c
    }
}

/// The first wait before a repeat: one and a half round trips, between
/// `FIRST_REPEAT_MIN` and `FIRST_REPEAT_MAX` - over 4G a fixed 100 ms would
/// repeat what is merely on its way.
pub fn first_wait(rtt: Duration) -> Duration {
    (rtt * 3 / 2).clamp(FIRST_REPEAT_MIN, FIRST_REPEAT_MAX)
}

/// What the server does with a numbered control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// New: apply it.
    Apply,
    /// This exact number was applied already (a repeat whose ack was lost).
    Repeat,
    /// Older than what was applied for the same control: a repeat that
    /// arrived after a newer value. Not applied - this is the rule that keeps
    /// a late "PTT on" from keying the radio after "off". Never for an act.
    Stale,
}

/// How many acts a session remembers to recognise a repeat. A repeat comes
/// within the give-up time (at most 7.5 s); this many acts inside that time is
/// far beyond what an operator does.
pub const ACTS_REMEMBERED: usize = 64;

/// The server side, one per client session.
#[derive(Debug, Default, Clone)]
pub struct ControlReceiver {
    last_applied: HashMap<ControlKey, u32>,
    /// The numbers of the last acts applied, oldest first, and the same as a set.
    acts: VecDeque<u32>,
    acts_seen: HashSet<u32>,
}

impl ControlReceiver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Judge a numbered control, and remember it if it is to be applied. The
    /// server acknowledges in every case: the client wants to stop repeating.
    ///
    /// "Newer" is serial-number arithmetic (as RFC 1982): within half the
    /// number space ahead, so the numbers may wrap - they start at the clock
    /// and pass `u32::MAX` every 49 days.
    ///
    /// An act is not ordered: done once per number, whatever came before it.
    pub fn judge(&mut self, key: ControlKey, seq: u32) -> Verdict {
        if key.is_act() {
            if self.acts_seen.contains(&seq) {
                return Verdict::Repeat;
            }
            if self.acts.len() == ACTS_REMEMBERED {
                if let Some(old) = self.acts.pop_front() {
                    self.acts_seen.remove(&old);
                }
            }
            self.acts.push_back(seq);
            self.acts_seen.insert(seq);
            return Verdict::Apply;
        }
        match self.last_applied.get(&key) {
            Some(&last) if seq == last => Verdict::Repeat,
            Some(&last) if (seq.wrapping_sub(last) as i32) < 0 => Verdict::Stale,
            _ => {
                self.last_applied.insert(key, seq);
                Verdict::Apply
            }
        }
    }

    /// The client behind this address started again (its numbers start over).
    pub fn reset(&mut self) {
        self.last_applied.clear();
        self.acts.clear();
        self.acts_seen.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::protocol::{ControlId, ControlPacket};

    const RTT: Duration = Duration::from_millis(20);
    /// A setting (RX1 AF gain), and a second one (drive) for "another control".
    const K: ControlKey = ControlKey::Control(ControlId::Rx1AfGain as u8);

    fn ctl(id: ControlId, value: u16) -> [u8; ControlPacket::SIZE] {
        let mut b = [0u8; ControlPacket::SIZE];
        ControlPacket { control_id: id, value }.serialize(&mut b);
        b
    }

    fn seq_of(bytes: &[u8]) -> u32 {
        u32::from_be_bytes(bytes[bytes.len() - 4..].try_into().unwrap())
    }

    #[test]
    fn nothing_is_repeated_before_the_server_has_acknowledged_once() {
        let t = Instant::now();
        let mut s = ControlSender::new();
        let b = s.send(&ctl(ControlId::Rx1AfGain, 1), t, RTT);
        assert_eq!(b.len(), 11);
        assert!(s.due(t + Duration::from_secs(5)).resend.is_empty(), "an older server: one send, as before");
        // Kept, one per control, so a server that never acknowledges costs no
        // more than the number of controls.
        s.send(&ctl(ControlId::Rx1AfGain, 2), t, RTT);
        assert_eq!(s.unacknowledged(), 1);
    }

    /// The opening burst of a session goes out before any acknowledgement can
    /// be back. One of it lost, another acknowledged: the lost one is repeated,
    /// a first wait after that acknowledgement - not at once, which would
    /// overtake the acknowledgements of the rest of the burst still on the way.
    #[test]
    fn a_control_lost_before_the_first_ack_is_repeated_after_it() {
        let t = Instant::now();
        let mut s = ControlSender::new();
        let lost = s.send(&ctl(ControlId::Rx1AfGain, 1), t, RTT);
        let arrives = s.send(&ctl(ControlId::DriveLevel, 2), t, RTT);
        let later = t + Duration::from_secs(2);
        s.acknowledged(seq_of(&arrives), later);
        assert!(s.due(later).resend.is_empty(), "not at the moment of the first ack");
        assert_eq!(s.due(later + first_wait(RTT)).resend, vec![lost]);
    }

    #[test]
    fn after_the_first_ack_a_lost_control_is_repeated_and_then_given_up() {
        let t = Instant::now();
        let mut s = ControlSender::new();
        let first = s.send(&ctl(ControlId::Rx1AfGain, 9), t, RTT);
        s.acknowledged(seq_of(&first), t);
        let b = s.send(&ctl(ControlId::Rx1AfGain, 1), t, RTT);
        assert!(s.due(t + Duration::from_millis(99)).resend.is_empty(), "not before the first wait");
        let mut at = t;
        let mut wait = first_wait(RTT);
        for n in 1..=MAX_REPEATS {
            at += wait;
            let d = s.due(at);
            assert_eq!(d.resend, vec![b.clone()], "repeat {n} is the same packet, same number");
            wait *= 2;
        }
        at += wait;
        let d = s.due(at);
        assert_eq!(d.given_up, vec![K]);
        assert_eq!(s.unacknowledged(), 0);
    }

    #[test]
    fn an_ack_stops_the_repeats() {
        let t = Instant::now();
        let mut s = ControlSender::new();
        let b = s.send(&ctl(ControlId::Rx1AfGain, 1), t, RTT);
        s.acknowledged(seq_of(&b), t);
        assert!(s.server_acknowledges());
        assert!(s.due(t + Duration::from_secs(5)).resend.is_empty());
    }

    #[test]
    fn a_newer_value_replaces_an_unacknowledged_older_one() {
        let t = Instant::now();
        let mut s = ControlSender::new();
        let other = s.send(&ctl(ControlId::DriveLevel, 0), t, RTT);
        s.acknowledged(seq_of(&other), t);
        s.send(&ctl(ControlId::Rx1AfGain, 1), t, RTT);
        let newer = s.send(&ctl(ControlId::Rx1AfGain, 2), t, RTT);
        assert_eq!(s.unacknowledged(), 1);
        let d = s.due(t + Duration::from_secs(1));
        assert_eq!(d.resend, vec![newer]);
    }

    #[test]
    fn a_lost_session_takes_its_pending_controls_and_the_ack_flag_with_it() {
        let t = Instant::now();
        let mut s = ControlSender::new();
        let a = s.send(&ctl(ControlId::Rx1AfGain, 1), t, RTT);
        s.acknowledged(seq_of(&a), t);
        let a = s.send(&ctl(ControlId::Rx1AfGain, 1), t, RTT);
        s.session_ended();
        assert_eq!(s.unacknowledged(), 0);
        assert!(!s.server_acknowledges());
        // Numbers go on: a server that kept the session still sees newer ones.
        let b = s.send(&ctl(ControlId::Rx1AfGain, 1), t, RTT);
        assert!(seq_of(&b) > seq_of(&a));
    }

    #[test]
    fn a_numbered_control_is_read_and_an_unnumbered_one_is_not() {
        use crate::protocol::{numbered_control, YaesuControlPacket};
        let c = ctl(ControlId::Rx1AfGain, 40);
        assert_eq!(numbered_control(&c), None, "an older client: no number");
        let t = Instant::now();
        let mut s = ControlSender::starting_at(1);
        let n = s.send(&c, t, RTT);
        assert_eq!(numbered_control(&n), Some((K, 1)));
        // And the number does not disturb a reader of the first seven bytes.
        assert_eq!(ControlPacket::deserialize(&n).unwrap().value, 40);
        let mut y = [0u8; YaesuControlPacket::SIZE];
        YaesuControlPacket { slot: 1, control: 9, value: 3 }.serialize(&mut y);
        let n = s.send(&y, t, RTT);
        assert_eq!(numbered_control(&n), Some((ControlKey::Yaesu { slot: 1, control: 9 }, 2)));
        assert_eq!(YaesuControlPacket::deserialize(&n).unwrap().value, 3);
    }

    #[test]
    fn the_first_wait_follows_the_round_trip() {
        assert_eq!(first_wait(Duration::from_millis(10)), FIRST_REPEAT_MIN);
        assert_eq!(first_wait(Duration::from_millis(200)), Duration::from_millis(300));
        assert_eq!(first_wait(Duration::from_secs(9)), FIRST_REPEAT_MAX);
    }

    #[test]
    fn the_server_applies_each_control_once_and_never_an_older_one() {
        let mut r = ControlReceiver::new();
        assert_eq!(r.judge(K, 5), Verdict::Apply);
        assert_eq!(r.judge(K, 5), Verdict::Repeat);
        assert_eq!(r.judge(K, 4), Verdict::Stale);
        assert_eq!(r.judge(K, 6), Verdict::Apply);
        // Another control has its own order.
        assert_eq!(r.judge(ControlKey::Control(ControlId::DriveLevel as u8), 3), Verdict::Apply);
        let act = ControlKey::Action { id: ControlId::VfoSwap as u8, seq: 9 };
        assert_eq!(r.judge(act, 9), Verdict::Apply);
        r.reset();
        assert_eq!(r.judge(K, 1), Verdict::Apply, "a restarted client starts over");
        assert_eq!(r.judge(act, 9), Verdict::Apply, "its acts too");
    }

    /// An acknowledgement of the previous session, still on its way when a new
    /// one began, does not switch repeating on - the new server may be one that
    /// does not acknowledge, and to it a repeat is a second press (reviews 87).
    /// Nor does a number this client never sent.
    #[test]
    fn only_an_ack_of_this_session_switches_repeating_on() {
        let t = Instant::now();
        let mut s = ControlSender::starting_at(u32::MAX - 1);
        let old = s.send(&ctl(ControlId::Rx1AfGain, 1), t, RTT);
        s.session_ended();
        s.acknowledged(seq_of(&old), t);
        assert!(!s.server_acknowledges(), "late, from the previous session");
        let lost = s.send(&ctl(ControlId::Rx1AfGain, 2), t, RTT);
        // The next number to go out (zero is skipped at the wrap, never sent).
        s.acknowledged(seq_of(&lost).wrapping_add(1).max(1), t);
        assert!(!s.server_acknowledges(), "a number not sent yet");
        assert!(s.due(t + Duration::from_secs(5)).resend.is_empty(), "so nothing is repeated");
        s.acknowledged(seq_of(&lost), t);
        assert!(s.server_acknowledges(), "this session's own number, across the wrap");
    }

    /// The numbers wrap: just past `u32::MAX` is newer, just before is older.
    #[test]
    fn newer_survives_the_wrap() {
        let mut r = ControlReceiver::new();
        assert_eq!(r.judge(K, u32::MAX - 1), Verdict::Apply);
        assert_eq!(r.judge(K, 2), Verdict::Apply, "past the wrap");
        assert_eq!(r.judge(K, u32::MAX), Verdict::Stale, "before it");
    }

    /// A new process behind the same address, and a server that did not notice
    /// the restart (no reset): its first control is still newer, because the
    /// numbers start at the clock.
    #[test]
    fn a_restarted_sender_is_newer_without_a_reset() {
        let t = Instant::now();
        let mut old = ControlSender::new();
        let mut r = ControlReceiver::new();
        for _ in 0..50 {
            let b = old.send(&ctl(ControlId::Rx1AfGain, 1), t, RTT);
            assert_eq!(r.judge(K, seq_of(&b)), Verdict::Apply);
        }
        // 50 controls; a restart takes far longer than 50 ms.
        std::thread::sleep(Duration::from_millis(100));
        let mut new = ControlSender::new();
        let b = new.send(&ctl(ControlId::Rx1AfGain, 1), t, RTT);
        assert_eq!(r.judge(K, seq_of(&b)), Verdict::Apply);
    }

    /// The case that makes the per-control order necessary (review 86): PTT on
    /// is lost, PTT off goes through, then the repeat of "on" arrives.
    #[test]
    fn a_late_repeat_of_ptt_on_does_not_key_the_radio_after_off() {
        let ptt = ControlKey::Control(crate::protocol::ControlId::YaesuPtt as u8);
        let mut r = ControlReceiver::new();
        // on = 10 (lost), off = 11 (arrives), on = 10 again (the repeat).
        assert_eq!(r.judge(ptt, 11), Verdict::Apply, "off");
        assert_eq!(r.judge(ptt, 10), Verdict::Stale, "the late on is not applied");
    }

    /// Which controls are an act: the fourteen, and a VFO select only when it
    /// swaps. A setting next to them stays a setting.
    #[test]
    fn an_act_is_told_from_a_setting() {
        assert!(is_action(ControlId::VfoSwap as u8, 1));
        assert!(is_action(ControlId::YaesuButton as u8, 17));
        assert!(is_action(ControlId::Yaesu2ReadMenus as u8, 0));
        assert!(is_action(ControlId::YaesuSelectVfo as u8, 2), "swap");
        assert!(is_action(ControlId::YaesuSelectVfo as u8, 3), "V/M, a toggle");
        assert!(is_action(ControlId::Yaesu2SelectVfo as u8, 3), "and on slot 1");
        assert!(!is_action(ControlId::Yaesu2SelectVfo as u8, 0), "select A is a setting");
        use crate::protocol::YaesuCtrl;
        assert!(is_yaesu_action(YaesuCtrl::ClarStep as u8, 10), "a clarifier step");
        assert!(!is_yaesu_action(YaesuCtrl::ClarClear as u8, 0), "clearing twice is clearing once");
        assert!(!is_yaesu_action(YaesuCtrl::Narrow as u8, 1));
        assert!(!is_action(ControlId::YaesuSelectVfo as u8, 1), "select B is a setting");
        assert!(!is_action(ControlId::YaesuPtt as u8, 1), "PTT is a setting: last value wins");
        assert!(!is_action(ControlId::CwMacroStop as u8, 0), "stopping twice is stopping once");
    }

    /// Two different presses of a Yaesu button, the first lost (reviews 87):
    /// the second does not push the first out of the queue - both are
    /// repeated, each with its own number.
    #[test]
    fn a_second_press_does_not_replace_a_lost_first_one() {
        let t = Instant::now();
        let mut s = ControlSender::new();
        let warm = s.send(&ctl(ControlId::Rx1AfGain, 1), t, RTT);
        s.acknowledged(seq_of(&warm), t);
        let a = s.send(&ctl(ControlId::YaesuButton, 3), t, RTT);
        let b = s.send(&ctl(ControlId::YaesuButton, 4), t, RTT);
        assert_eq!(s.unacknowledged(), 2);
        s.acknowledged(seq_of(&b), t);
        assert_eq!(s.due(t + Duration::from_secs(1)).resend, vec![a]);
    }

    fn yaesu(slot: u8, control: u8, value: u16) -> [u8; crate::protocol::YaesuControlPacket::SIZE] {
        let mut b = [0u8; crate::protocol::YaesuControlPacket::SIZE];
        crate::protocol::YaesuControlPacket { slot, control, value }.serialize(&mut b);
        b
    }

    /// Two clarifier steps, the first lost (round on build 88): both stay, and
    /// the first is repeated - +10 and +10 is +20, not +10.
    #[test]
    fn a_second_clarifier_step_does_not_replace_a_lost_first_one() {
        use crate::protocol::{numbered_control, YaesuCtrl};
        let t = Instant::now();
        let mut s = ControlSender::starting_at(70);
        let warm = s.send(&ctl(ControlId::Rx1AfGain, 1), t, RTT);
        s.acknowledged(seq_of(&warm), t);
        let step = YaesuCtrl::ClarStep as u8;
        let a = s.send(&yaesu(0, step, 10), t, RTT);
        let b = s.send(&yaesu(0, step, 10), t, RTT);
        assert_eq!(numbered_control(&a), Some((ControlKey::YaesuAction { slot: 0, control: step, seq: 71 }, 71)));
        assert_eq!(s.unacknowledged(), 2);
        s.acknowledged(seq_of(&b), t);
        assert_eq!(s.due(t + Duration::from_secs(1)).resend, vec![a]);
        let mut r = ControlReceiver::new();
        let (kb, nb) = numbered_control(&b).unwrap();
        let (ka, na) = numbered_control(&s.send(&yaesu(0, step, 10), t, RTT)).unwrap();
        assert_eq!(r.judge(ka, na), Verdict::Apply);
        assert_eq!(r.judge(kb, nb), Verdict::Apply, "an older step is still its own step");
    }

    /// A packet that is not a control goes out as it came, without a number
    /// (round on build 88), and the numbering does not skip.
    #[test]
    fn a_packet_that_is_not_a_control_gets_no_number() {
        let t = Instant::now();
        let mut s = ControlSender::starting_at(5);
        let other = [crate::protocol::MAGIC, 0, crate::protocol::PacketType::Disconnect as u8, 0];
        assert_eq!(s.send(&other, t, RTT), other.to_vec());
        assert_eq!(s.unacknowledged(), 0);
        assert_eq!(seq_of(&s.send(&ctl(ControlId::Rx1AfGain, 1), t, RTT)), 5);
    }

    /// Two swaps: two acts, even with the same value.
    #[test]
    fn two_swaps_are_two_acts() {
        let t = Instant::now();
        let mut s = ControlSender::new();
        let warm = s.send(&ctl(ControlId::Rx1AfGain, 1), t, RTT);
        s.acknowledged(seq_of(&warm), t);
        s.send(&ctl(ControlId::VfoSwap, 1), t, RTT);
        s.send(&ctl(ControlId::VfoSwap, 1), t, RTT);
        assert_eq!(s.unacknowledged(), 2);
    }

    /// The server does an act once per number, and an act that arrives late -
    /// after a later one - is still done: it is not an older value of anything.
    #[test]
    fn the_server_does_each_act_once_in_any_order() {
        let a = ControlKey::Action { id: ControlId::YaesuButton as u8, seq: 10 };
        let b = ControlKey::Action { id: ControlId::YaesuButton as u8, seq: 11 };
        let mut r = ControlReceiver::new();
        assert_eq!(r.judge(b, 11), Verdict::Apply);
        assert_eq!(r.judge(a, 10), Verdict::Apply, "late, but its own act");
        assert_eq!(r.judge(a, 10), Verdict::Repeat, "and only once");
        assert_eq!(r.judge(b, 11), Verdict::Repeat);
    }

    /// Against a server that never acknowledges, acts do not pile up: each is
    /// let go once its repeat window is over. Settings stay one per control.
    #[test]
    fn acts_do_not_pile_up_without_acknowledgements() {
        let t = Instant::now();
        let mut s = ControlSender::new();
        for _ in 0..10 {
            s.send(&ctl(ControlId::VfoSwap, 1), t, RTT);
        }
        s.send(&ctl(ControlId::Rx1AfGain, 1), t, RTT);
        assert_eq!(s.unacknowledged(), 11);
        assert!(s.due(t + REPEAT_WINDOW - Duration::from_millis(1)).resend.is_empty());
        assert_eq!(s.unacknowledged(), 11, "inside the window: kept, for a first ack still to come");
        s.due(t + REPEAT_WINDOW);
        assert_eq!(s.unacknowledged(), 1, "the setting stays, the acts are let go");
    }

    /// The window is the sum of the doubling waits from the longest first one.
    #[test]
    fn the_repeat_window_covers_every_repeat() {
        let total: Duration = (0..MAX_REPEATS).map(|n| FIRST_REPEAT_MAX * 2u32.pow(n)).sum();
        assert_eq!(total, REPEAT_WINDOW);
    }

    /// The memory of acts is bounded, oldest out first.
    #[test]
    fn the_server_remembers_a_bounded_number_of_acts() {
        let mut r = ControlReceiver::new();
        let act = |seq| ControlKey::Action { id: ControlId::VfoSwap as u8, seq };
        for seq in 1..=(ACTS_REMEMBERED as u32 + 1) {
            assert_eq!(r.judge(act(seq), seq), Verdict::Apply);
        }
        assert_eq!(r.judge(act(2), 2), Verdict::Repeat, "still remembered");
        assert_eq!(r.judge(act(1), 1), Verdict::Apply, "the oldest has gone");
    }

    /// Client and server read the key from the same bytes: a swap is an act
    /// keyed by its number on both sides.
    #[test]
    fn an_act_is_keyed_by_its_number_on_the_wire() {
        use crate::protocol::numbered_control;
        let t = Instant::now();
        let mut s = ControlSender::starting_at(41);
        let n = s.send(&ctl(ControlId::VfoSwap, 1), t, RTT);
        assert_eq!(
            numbered_control(&n),
            Some((ControlKey::Action { id: ControlId::VfoSwap as u8, seq: 41 }, 41))
        );
    }
}
