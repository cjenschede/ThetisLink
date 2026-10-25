// SPDX-License-Identifier: GPL-2.0-or-later

//! What a transmit audio packet does when it arrives, for every transmitter -
//! Thetis and both Yaesu radios (build 285, step 1a of `KOPPELING` 18). Since
//! build 282 the PTT is the flag on the audio for all of them, but Thetis and
//! the radios each had their own arm deciding what a packet does: take the
//! transmitter, play, or nothing. The filter on late packets (283) was in the
//! radios' arm only. The decision is here now, with a test; what follows it -
//! a radio keyed on arrival, Thetis keyed when its buffer plays, where the
//! audio goes - stays with each transmitter (owner, 2026-10-04: only the
//! handling towards the radio itself may differ).
//!
//! The rule is the radios' arm of build 283, the more complete of the two.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::session::TxTarget;

/// What one packet finds when it arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arrival {
    /// The PTT flag on the packet.
    pub ptt: bool,
    /// The sender holds this transmitter.
    pub from_holder: bool,
    /// The transmitter is keyed for its holder. A radio is keyed on arrival
    /// (its flag); Thetis when its buffer plays, so for Thetis this is
    /// `from_holder`: a holder's packet plays, and the press keys it there.
    pub keyed: bool,
    /// Older than the newest this transmitter had from that sender
    /// (`older_than_the_newest`).
    pub stale: bool,
    /// The radio let this sender go by itself and it still presses
    /// (`SessionManager::let_go_by_the_radio_and_still_pressing`; never for
    /// Thetis).
    pub let_go_by_radio: bool,
}

/// What the packet does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intake {
    /// Without the flag and not from the holder: nothing to play, nothing to
    /// let go (an idle client's packets, or the rest of a burst after the
    /// release).
    Pass,
    /// With the flag, older than the sender's newest: from a press that is
    /// over, it takes nothing.
    Stale,
    /// With the flag, from a sender its radio let go: ignored until it lets go
    /// too.
    LetGoByRadio,
    /// With the flag, and the transmitter is not this sender's yet (or not
    /// keyed): ask for it - the radio's own refusal, then the session's - and
    /// play when granted.
    Claim,
    /// The holder's packet, with the flag or its release: it plays.
    Play,
}

/// The one decision (build 285).
pub fn intake(a: Arrival) -> Intake {
    if !a.ptt && !a.from_holder {
        return Intake::Pass;
    }
    if a.ptt && (!a.from_holder || !a.keyed) {
        if a.stale {
            return Intake::Stale;
        }
        if a.let_go_by_radio {
            return Intake::LetGoByRadio;
        }
        return Intake::Claim;
    }
    Intake::Play
}

/// The line for the first of a run of `Intake::Stale` packets, every
/// transmitter's (`label`: "Thetis", "Radio 1", ...). Its words since build 282.
pub fn stale_line(label: impl std::fmt::Display, addr: SocketAddr, seq: u32) -> String {
    format!("{} PTT: a PTT frame from {} older than its newest (seq {}) - ignored", label, addr, seq)
}

/// The line for the first of a run of `Intake::LetGoByRadio` packets. Its
/// words since build 282.
pub fn let_go_line(label: impl std::fmt::Display, addr: SocketAddr) -> String {
    format!("{} PTT: {} was let go by the server - its PTT frames are ignored until it lets go", label, addr)
}

/// Is sequence `a` after `b`, across the wrap (build 282)?
pub fn sequence_after(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// Is a packet from `addr` with sequence `seq`, at `now`, older than the
/// newest this transmitter had from that address (`newest`: address,
/// sequence, when)? Only inside the packet window after that newest one
/// (build 283): the filter is for the late packets of a press that just ended.
/// A client that restarted behind the same address - over the relay it can -
/// begins at sequence 0 again, and was kept off its radio until its count
/// caught up (round 282). Past the window the safety catches a late packet
/// anyway.
pub fn older_than_the_newest(newest: Option<(SocketAddr, u32, Instant)>, addr: SocketAddr, seq: u32, now: Instant) -> bool {
    newest.is_some_and(|(from, n, at)| {
        from == addr
            && now.saturating_duration_since(at) <= Duration::from_millis(crate::ptt::PTT_PACKET_TIMEOUT_MS)
            && !sequence_after(seq, n)
    })
}

/// Per transmitter: the newest packet it had from its sender and when, and
/// how many packets with the flag it ignored since it was last taken (one log
/// line per run of them). One for all three (build 285; the radios had two
/// arrays of their own since 282, Thetis none).
#[derive(Debug, Default)]
pub struct Senders {
    newest: [Option<(SocketAddr, u32, Instant)>; 3],
    ignored: [u64; 3],
}

fn index(target: TxTarget) -> usize {
    match target {
        TxTarget::Thetis => 0,
        TxTarget::Yaesu1 => 1,
        TxTarget::Yaesu2 => 2,
    }
}

impl Senders {
    /// Is this packet older than the transmitter's newest from its sender?
    pub fn stale(&self, target: TxTarget, addr: SocketAddr, seq: u32, now: Instant) -> bool {
        older_than_the_newest(self.newest[index(target)], addr, seq, now)
    }

    /// One more packet ignored; true for the first of a run, the one to log.
    pub fn ignored(&mut self, target: TxTarget) -> bool {
        let n = &mut self.ignored[index(target)];
        *n += 1;
        *n == 1
    }

    /// The transmitter was taken: the next ignored packet starts a new run.
    pub fn taken(&mut self, target: TxTarget) {
        self.ignored[index(target)] = 0;
    }

    /// A packet that plays, and was not stale: the newest now.
    pub fn played(&mut self, target: TxTarget, addr: SocketAddr, seq: u32, now: Instant) {
        self.newest[index(target)] = Some((addr, seq, now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every input, as the radios' arm of 283 and the Thetis arm before 285
    /// decided it - a radio (`keyed` its own input) and Thetis (`keyed` =
    /// `from_holder`) both.
    #[test]
    fn what_a_packet_does_for_a_radio_and_for_thetis() {
        let mut seen = 0;
        for ptt in [false, true] {
            for from_holder in [false, true] {
                for radio_keyed in [false, true] {
                    for stale in [false, true] {
                        for let_go_by_radio in [false, true] {
                            for thetis in [false, true] {
                                let keyed = if thetis { from_holder } else { radio_keyed };
                                let let_go_by_radio = let_go_by_radio && !thetis;
                                let a = Arrival { ptt, from_holder, keyed, stale, let_go_by_radio };
                                let want = if !ptt && !from_holder {
                                    Intake::Pass
                                } else if !ptt {
                                    Intake::Play
                                } else if from_holder && keyed {
                                    Intake::Play
                                } else if stale {
                                    Intake::Stale
                                } else if let_go_by_radio {
                                    Intake::LetGoByRadio
                                } else {
                                    Intake::Claim
                                };
                                assert_eq!(intake(a), want, "{a:?}");
                                seen += 1;
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(seen, 64);
    }

    /// Thetis, as the arm before 285 decided, case by case - and the one
    /// change: a late packet with the flag no longer takes Thetis again.
    #[test]
    fn thetis_takes_what_it_took_and_no_late_packet() {
        let thetis = |ptt, from_holder, stale| intake(Arrival { ptt, from_holder, keyed: from_holder, stale, let_go_by_radio: false });
        assert_eq!(thetis(true, false, false), Intake::Claim, "a press asks for Thetis");
        assert_eq!(thetis(true, true, false), Intake::Play, "the holder's press plays");
        assert_eq!(thetis(true, true, true), Intake::Play, "the holder's own repeat plays - the buffer drops a duplicate");
        assert_eq!(thetis(false, true, false), Intake::Play, "the holder's release plays");
        assert_eq!(thetis(false, false, false), Intake::Pass, "nobody's release: nothing");
        assert_eq!(thetis(true, false, true), Intake::Stale, "new in 285: a late press after the release takes nothing");
    }

    /// A radio: keyed on arrival, so the holder's press on a radio that is not
    /// keyed (a radio its holder had, now off) asks again.
    #[test]
    fn a_radio_not_keyed_is_claimed_again_by_its_holder() {
        let radio = |from_holder, keyed| intake(Arrival { ptt: true, from_holder, keyed, stale: false, let_go_by_radio: false });
        assert_eq!(radio(true, false), Intake::Claim);
        assert_eq!(radio(true, true), Intake::Play);
        assert_eq!(radio(false, true), Intake::Claim, "another sender asks, and is refused by the session");
        // Golden rows of their own (round 285: the table test computes its
        // answer with a second copy of the rule).
        let late = intake(Arrival { ptt: true, from_holder: false, keyed: true, stale: true, let_go_by_radio: true });
        assert_eq!(late, Intake::Stale, "a late press of an over that ended takes nothing, before anything else is asked");
        let let_go = intake(Arrival { ptt: true, from_holder: false, keyed: false, stale: false, let_go_by_radio: true });
        assert_eq!(let_go, Intake::LetGoByRadio, "the radio let it go, and it still presses: it waits for its own release");
        let release = intake(Arrival { ptt: false, from_holder: false, keyed: false, stale: false, let_go_by_radio: true });
        assert_eq!(release, Intake::Pass, "its release then is nothing to play");
    }

    /// The table per transmitter: Thetis's newest is not a radio's. Twice, the
    /// second run after the transmitter was taken again.
    #[test]
    fn each_transmitter_keeps_its_own_senders() {
        let a: SocketAddr = "198.51.100.1:2".parse().unwrap();
        let t0 = Instant::now();
        let mut s = Senders::default();
        for run in 0..2 {
            s.played(TxTarget::Thetis, a, 100, t0);
            assert!(s.stale(TxTarget::Thetis, a, 90, t0), "run {run}: late on Thetis");
            assert!(!s.stale(TxTarget::Yaesu1, a, 90, t0), "run {run}: not on radio 1");
            assert!(!s.stale(TxTarget::Yaesu2, a, 90, t0), "run {run}: not on radio 2");
            assert!(s.ignored(TxTarget::Thetis), "run {run}: the first ignored is logged");
            assert!(!s.ignored(TxTarget::Thetis), "run {run}: the second is not");
            assert!(s.ignored(TxTarget::Yaesu1), "run {run}: a radio counts its own");
            s.taken(TxTarget::Thetis);
            s.taken(TxTarget::Yaesu1);
        }
    }

    /// Sequences across the wrap (build 282): what keys a radio has to be
    /// newer than its sender's newest.
    #[test]
    fn a_sequence_after_another_across_the_wrap() {
        assert!(sequence_after(5, 4));
        assert!(!sequence_after(4, 4), "the same one is not after");
        assert!(!sequence_after(3, 4));
        assert!(sequence_after(0, u32::MAX), "across the wrap");
        assert!(!sequence_after(u32::MAX, 0));
    }

    /// The filter on late packets holds only inside the packet window, and only
    /// for the same address (build 283): a client that restarted behind the
    /// same address begins at 0 again and keys its radio (round 282).
    /// Twice, the second over a newest that moved on.
    #[test]
    fn a_late_packet_is_old_only_inside_the_window() {
        let a: SocketAddr = "198.51.100.1:2".parse().unwrap();
        let b: SocketAddr = "198.51.100.1:3".parse().unwrap();
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        assert!(!older_than_the_newest(None, a, 0, t0), "nothing seen yet");
        for newest in [500u32, 9_000] {
            let n = Some((a, newest, t0));
            assert!(older_than_the_newest(n, a, newest - 3, at(100)), "{newest}: late, inside the window");
            assert!(older_than_the_newest(n, a, newest, at(2_000)), "{newest}: the same one, at the window's edge");
            assert!(!older_than_the_newest(n, a, newest + 1, at(100)), "{newest}: newer");
            assert!(!older_than_the_newest(n, b, 0, at(100)), "{newest}: another address");
            assert!(!older_than_the_newest(n, a, 0, at(2_001)), "{newest}: a restart past the window keys");
        }
    }
}
