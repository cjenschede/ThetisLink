// SPDX-License-Identifier: GPL-2.0-or-later

//! A transmitter the server keyed, whose owner is gone.
//!
//! The ownership table is only a table. Releasing a lock does not open a relay:
//! when a client vanishes mid-transmission - Disconnect, a session timeout, a
//! pulled plug - the entry disappears and the radio keeps talking.
//!
//! Thetis is covered by its own dead-man switch: its TX hangs on the audio
//! stream, and `Ptt::check_safety` drops it on a missed heartbeat or after
//! 500 ms without PTT packets. A Yaesu has neither. Its PTT is a command, not a
//! stream, so nothing dries up - the radio transmits until its own time-out
//! timer expires, and on a 991A that timer is often switched off.
//!
//! Two conditions, both needed. "The server keyed it" is what keeps this away
//! from an operator standing at the radio: a front-panel press never sets that
//! flag, so this never takes his transmission away from him. "Nobody holds it"
//! is what makes it an orphan.

/// Watches one transmitter and says when to force it back to RX.
///
/// It waits for the condition to hold twice. A normal release clears the owner
/// and the flag one after the other, not in the same instant, and this loop runs
/// in another task - so a single tick can land in between and see an orphan that
/// is already on its way out. The second look costs one tick and removes that
/// whole class of false alarm.
#[derive(Default, Debug)]
pub struct OrphanWatch {
    seen: u32,
}

impl OrphanWatch {
    /// Feed one tick. Returns true exactly once per orphan, when it is time to
    /// send the release.
    pub fn observe(&mut self, server_keyed: bool, has_holder: bool) -> bool {
        if !(server_keyed && !has_holder) {
            self.seen = 0;
            return false;
        }
        self.seen += 1;
        // Fires on the second consecutive tick, then stays quiet: the caller
        // sends one release, and repeating it every 200 ms would fight an
        // operator who keys the radio by hand straight afterwards.
        self.seen == 2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_transmitter_is_left_alone() {
        let mut w = OrphanWatch::default();
        for _ in 0..10 {
            assert!(!w.observe(true, true));
        }
    }

    #[test]
    fn an_idle_transmitter_is_left_alone() {
        let mut w = OrphanWatch::default();
        for _ in 0..10 {
            assert!(!w.observe(false, false));
        }
    }

    /// The front-panel operator. Nobody holds the radio and it is transmitting,
    /// but the server did not key it - so this must never fire.
    #[test]
    fn a_local_transmission_is_never_taken_away() {
        let mut w = OrphanWatch::default();
        for _ in 0..10 {
            assert!(!w.observe(false, false));
        }
    }

    /// The race in a normal release: for one tick the owner is already gone
    /// while the flag has not been cleared yet.
    #[test]
    fn one_stale_tick_does_not_fire() {
        let mut w = OrphanWatch::default();
        assert!(!w.observe(true, false));
        assert!(!w.observe(false, false));
    }

    #[test]
    fn a_real_orphan_fires_once_on_the_second_tick() {
        let mut w = OrphanWatch::default();
        assert!(!w.observe(true, false));
        assert!(w.observe(true, false));
        // The release is on its way; do not keep shouting.
        for _ in 0..10 {
            assert!(!w.observe(true, false));
        }
    }

    #[test]
    fn it_arms_again_for_the_next_client() {
        let mut w = OrphanWatch::default();
        assert!(!w.observe(true, false));
        assert!(w.observe(true, false));
        // New client takes the radio and leaves the same way.
        assert!(!w.observe(true, true));
        assert!(!w.observe(true, false));
        assert!(w.observe(true, false));
    }
}
