// SPDX-License-Identifier: GPL-2.0-or-later

//! How often to tell a client that its transmit request was turned down.
//!
//! Thetis PTT rides on the audio packets, so one held button is fifty refused
//! packets a second - and the server answered every one of them. Fifty refusals
//! a second is waste on its own, and it is what set off a whole evening of
//! faults: the client cleared the flag once on letting go, and the refusals
//! still in flight set it again with nothing left to clear them.
//!
//! One per attempt is the right number. But the refusal travels over UDP and
//! can be lost, and a client that never hears it keeps its button red over a
//! transmission that never started - so while the request is still coming in,
//! say it again now and then. That is one a second instead of fifty, and it
//! still recovers from a lost packet within a second.

/// How long before the same client is told again, while it keeps asking.
pub const RESEND_MS: u64 = 1000;

/// When an entry may be forgotten altogether.
///
/// The bookkeeping is cleared when a client stops asking, is granted the
/// transmitter, or says goodbye - and a client that simply falls silent does
/// none of those. Its entry then stays, and the next refusal to that address is
/// throttled against an attempt from another session (review finding, round 2).
///
/// Well past the session timeout, so this never expires an attempt that is
/// still running; it only stops the bookkeeping from outliving the client.
pub const FORGET_AFTER_MS: u64 = 60_000;

/// May this entry be dropped?
pub fn forgotten(since_last_ms: u64) -> bool {
    since_last_ms >= FORGET_AFTER_MS
}

/// Should this refusal be sent?
///
/// `since_last_ms` is the time since this client was last told, or `None` if it
/// has not been told about this attempt at all. An attempt ends when the client
/// stops asking - the caller forgets it then, and the next refusal is a first
/// one again.
pub fn denial_due(since_last_ms: Option<u64>) -> bool {
    match since_last_ms {
        None => true,
        Some(ms) => ms >= RESEND_MS,
    }
}

/// Who has been told, and when - with the ways it ends.
///
/// This lived as a bare `HashMap` inside the packet loop, and then only the
/// interval could be tested: `denial_due` was green while the bookkeeping around
/// it leaked an entry for every client that fell silent. A test that fills in
/// its own numbers cannot see that (review finding, round 2).
///
/// So the map and its exits live together, and the exits have names. There are
/// four: the client is granted the transmitter, it stops asking, it says
/// goodbye, or it simply disappears - and the last one is the one that had no
/// name and therefore no code.
///
/// Time is passed in as milliseconds since the server started rather than read
/// from the clock, so a test can age an entry without waiting for it.
#[derive(Default)]
pub struct DenialLog {
    told: std::collections::HashMap<std::net::SocketAddr, u64>,
}

impl DenialLog {
    /// Should this client be told its request was refused? Records it if so.
    ///
    /// Prunes on the way in: that is the exit for a client that vanished, and
    /// putting it here means it cannot be forgotten again.
    pub fn should_send(&mut self, addr: std::net::SocketAddr, now_ms: u64) -> bool {
        self.told.retain(|_, at| !forgotten(now_ms.saturating_sub(*at)));
        let since = self.told.get(&addr).map(|at| now_ms.saturating_sub(*at));
        if denial_due(since) {
            self.told.insert(addr, now_ms);
            true
        } else {
            false
        }
    }

    /// Was this the first refusal of the attempt? Only that one is worth a log
    /// line; the repeats are the same news.
    pub fn is_first(&self, addr: &std::net::SocketAddr) -> bool {
        !self.told.contains_key(addr)
    }

    /// The attempt is over: granted, stopped asking, or gone. All three end it,
    /// and none of them is different from the others here - which is why they
    /// share one method instead of three that could drift.
    pub fn attempt_over(&mut self, addr: &std::net::SocketAddr) {
        self.told.remove(addr);
    }

    /// How many clients are being remembered.
    ///
    /// Only the tests read this, and that is on purpose: the question they ask
    /// with it - does this grow without bound - has no other way to be asked.
    #[cfg(test)]
    pub fn remembered(&self) -> usize {
        self.told.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{denial_due, forgotten, FORGET_AFTER_MS, RESEND_MS};

    #[test]
    fn the_first_refusal_of_an_attempt_is_always_sent() {
        assert!(denial_due(None));
    }

    /// The flood: a held button refused fifty times a second.
    #[test]
    fn the_next_fifty_are_not() {
        for ms in [0, 20, 40, 200, 999] {
            assert!(!denial_due(Some(ms)), "{ms} ms after the last one");
        }
    }

    /// But a lost refusal must not leave the operator with a red button, so
    /// while the request keeps coming in it is repeated.
    /// A client that falls silent leaves an entry behind; it has to expire.
    #[test]
    fn an_entry_is_forgotten_long_after_the_last_refusal() {
        assert!(!forgotten(0));
        assert!(!forgotten(RESEND_MS));
        assert!(!forgotten(FORGET_AFTER_MS - 1));
        assert!(forgotten(FORGET_AFTER_MS));
    }

    /// And not so soon that it expires an attempt that is still running: the
    /// session timeout for a client holding a transmitter is 3 s, and 15 s for
    /// one that is only listening.
    #[test]
    fn forgetting_happens_well_after_a_session_would_have_timed_out() {
        assert!(!forgotten(15_000));
    }

    #[test]
    fn a_client_that_keeps_asking_is_told_again() {
        assert!(denial_due(Some(RESEND_MS)));
        assert!(denial_due(Some(RESEND_MS + 500)));
    }
}

#[cfg(test)]
mod log_tests {
    use super::{DenialLog, FORGET_AFTER_MS, RESEND_MS};
    use std::net::SocketAddr;

    fn a(port: u16) -> SocketAddr {
        format!("127.0.0.1:{port}").parse().unwrap()
    }

    #[test]
    fn the_first_refusal_is_sent_and_the_next_fifty_are_not() {
        let mut l = DenialLog::default();
        assert!(l.should_send(a(1), 0));
        for ms in [20, 40, 200, 999] {
            assert!(!l.should_send(a(1), ms), "{ms} ms");
        }
        assert!(l.should_send(a(1), RESEND_MS));
    }

    #[test]
    fn the_first_of_an_attempt_is_the_one_worth_logging() {
        let mut l = DenialLog::default();
        assert!(l.is_first(&a(1)));
        l.should_send(a(1), 0);
        assert!(!l.is_first(&a(1)));
        l.attempt_over(&a(1));
        assert!(l.is_first(&a(1)), "a new attempt starts over");
    }

    /// Granted, stopped asking, or said goodbye: the next refusal is a first
    /// one again.
    #[test]
    fn an_attempt_that_ends_starts_the_count_over() {
        let mut l = DenialLog::default();
        assert!(l.should_send(a(1), 0));
        assert!(!l.should_send(a(1), 100));
        l.attempt_over(&a(1));
        assert!(l.should_send(a(1), 150), "a new attempt is told at once");
    }

    /// The exit that had no name: a client that simply falls silent. Nothing
    /// calls attempt_over for it, so its entry has to expire on its own - and
    /// until it does, a NEW session on the same address is throttled against it.
    #[test]
    fn a_client_that_vanished_is_forgotten_and_stops_throttling() {
        let mut l = DenialLog::default();
        assert!(l.should_send(a(1), 0));
        assert_eq!(l.remembered(), 1);

        // Same address, a whole new session, long after the old one died.
        assert!(l.should_send(a(1), FORGET_AFTER_MS));
        assert_eq!(l.remembered(), 1, "forgotten and re-recorded, not two entries");
    }

    /// And the map does not grow without bound while clients come and go.
    #[test]
    fn the_bookkeeping_does_not_keep_growing() {
        let mut l = DenialLog::default();
        for i in 0..50u16 {
            l.should_send(a(i + 1), i as u64);
        }
        assert_eq!(l.remembered(), 50);
        // One later refusal from one client prunes everything that has aged out.
        l.should_send(a(1), FORGET_AFTER_MS + 100);
        assert_eq!(l.remembered(), 1);
    }
}
