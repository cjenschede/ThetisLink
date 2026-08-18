// SPDX-License-Identifier: GPL-2.0-or-later
//
//! The two things that must be remembered between requests: how often a station
//! has written, and what it just wrote.
//!
//! Both live in memory on purpose. They protect against a burst, not against a
//! restart — losing them when the container restarts costs a station one extra
//! allowed message, which is not worth a database write per request on a machine
//! that is also carrying audio.

use std::collections::HashMap;

/// Design §2.2. Twenty a minute is a conversation; more than that is not
/// somebody typing.
pub const MAX_MESSAGES_PER_MINUTE: usize = 20;

const WINDOW_SECS: i64 = 60;

/// How long a message fingerprint is remembered, to swallow an accidental double
/// send.
///
/// Short: this is about a double click or a client retry, not about security.
const RECENT_MEMORY_SECS: i64 = 30;

#[derive(Default)]
pub struct Guard {
    /// station -> the times it wrote inside the current window
    writes: HashMap<i64, Vec<i64>>,
    /// station + exact message -> when it was last seen, to swallow a double send
    recent: HashMap<(i64, String), i64>,
    /// station -> when it was last said out loud that it was turned away
    refusal_said: HashMap<i64, i64>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Too many messages in the last minute.
    TooFast,
    /// The same message again within seconds - a double click or a retry.
    Duplicate,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // These reach the sender as well as the log (design §8): a window that
        // does nothing, with no reason given, is the fault report nobody can
        // place.
        f.write_str(match self {
            Refusal::TooFast => "too many messages in a minute",
            Refusal::Duplicate => "you just sent that",
        })
    }
}

impl Guard {
    /// Claim the right to write once. Anything refused here has not been
    /// counted, so a refusal cannot itself use up somebody's allowance.
    ///
    /// Note what this does NOT do: consume the ticket. An earlier version spent
    /// the ticket's one-time id on the first write, which reads plausibly and
    /// made the chat unusable - a ticket lasts fifteen minutes and is refreshed
    /// every seven and a half, so a station could send exactly one message per
    /// refresh. A ticket is a session credential, not a one-shot coupon, and
    /// reuse by the rightful holder is the whole point of it.
    ///
    /// What is worth catching is a double send: the same words from the same
    /// station within seconds is a double click or a client retry, not somebody
    /// saying it twice.
    pub fn claim_write(&mut self, station_id: i64, body: &str, now: i64) -> Result<(), Refusal> {
        self.forget_old(now);

        let key = (station_id, body.to_string());
        if self.recent.contains_key(&key) {
            return Err(Refusal::Duplicate);
        }

        let times = self.writes.entry(station_id).or_default();
        if times.len() >= MAX_MESSAGES_PER_MINUTE {
            return Err(Refusal::TooFast);
        }

        times.push(now);
        self.recent.insert(key, now);
        Ok(())
    }

    /// Drop what has fallen out of its window. Called on every claim, so neither
    /// map can grow past the traffic of one window.
    fn forget_old(&mut self, now: i64) {
        self.writes.retain(|_, times| {
            times.retain(|t| now - *t < WINDOW_SECS);
            !times.is_empty()
        });
        self.recent.retain(|_, t| now - *t < RECENT_MEMORY_SECS);
    }

    #[cfg(test)]
    fn sizes(&self) -> (usize, usize) {
        (self.writes.len(), self.recent.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: i64 = 1_700_000_000;

    #[test]
    fn writing_within_the_limit_is_allowed() {
        let mut g = Guard::default();
        for i in 0..MAX_MESSAGES_PER_MINUTE {
            assert_eq!(g.claim_write(7, &format!("bericht {i}"), T), Ok(()));
        }
    }

    #[test]
    fn the_one_over_the_limit_is_refused() {
        let mut g = Guard::default();
        for i in 0..MAX_MESSAGES_PER_MINUTE {
            g.claim_write(7, &format!("bericht {i}"), T).unwrap();
        }
        assert_eq!(g.claim_write(7, "nog een", T), Err(Refusal::TooFast));
    }

    /// One noisy station must not silence the rest of the channel.
    #[test]
    fn the_limit_is_per_station() {
        let mut g = Guard::default();
        for i in 0..MAX_MESSAGES_PER_MINUTE {
            g.claim_write(7, &format!("a{i}"), T).unwrap();
        }
        assert_eq!(g.claim_write(7, "meer", T), Err(Refusal::TooFast));
        assert_eq!(g.claim_write(8, "b0", T), Ok(()), "another station is unaffected");
    }

    #[test]
    fn the_allowance_returns_after_a_minute() {
        let mut g = Guard::default();
        for i in 0..MAX_MESSAGES_PER_MINUTE {
            g.claim_write(7, &format!("a{i}"), T).unwrap();
        }
        assert_eq!(g.claim_write(7, "meer", T), Err(Refusal::TooFast));
        assert_eq!(g.claim_write(7, "later", T + WINDOW_SECS), Ok(()));
    }

    /// The whole reason this was rewritten: a conversation is many messages on
    /// one ticket. Spending the ticket on the first write let a station say one
    /// thing per refresh, which is not a chat.
    #[test]
    fn a_conversation_of_different_messages_is_allowed() {
        let mut g = Guard::default();
        assert_eq!(g.claim_write(7, "hallo allemaal", T), Ok(()));
        assert_eq!(g.claim_write(7, "hoe gaat het", T + 1), Ok(()));
        assert_eq!(g.claim_write(7, "tot ziens", T + 2), Ok(()));
    }

    /// The same words twice within seconds is a double click, not somebody
    /// saying it again.
    #[test]
    fn the_same_message_twice_in_a_row_is_swallowed() {
        let mut g = Guard::default();
        assert_eq!(g.claim_write(7, "hoi", T), Ok(()));
        assert_eq!(g.claim_write(7, "hoi", T + 1), Err(Refusal::Duplicate));
    }

    /// But saying the same thing again later is perfectly normal - "ja" twice in
    /// one conversation must not be refused.
    #[test]
    fn the_same_message_later_is_fine() {
        let mut g = Guard::default();
        assert_eq!(g.claim_write(7, "ja", T), Ok(()));
        assert_eq!(g.claim_write(7, "ja", T + RECENT_MEMORY_SECS), Ok(()));
    }

    /// Two people may agree with each other.
    #[test]
    fn two_stations_may_say_the_same_thing() {
        let mut g = Guard::default();
        assert_eq!(g.claim_write(7, "ja", T), Ok(()));
        assert_eq!(g.claim_write(8, "ja", T), Ok(()));
    }

    /// A refusal must not spend the allowance it was refused from, or a station
    /// being rate-limited could never recover within its window.
    #[test]
    fn a_refusal_does_not_cost_an_allowance() {
        let mut g = Guard::default();
        g.claim_write(7, "eerste", T).unwrap();
        assert_eq!(g.claim_write(7, "eerste", T), Err(Refusal::Duplicate));
        for i in 0..(MAX_MESSAGES_PER_MINUTE - 1) {
            assert_eq!(g.claim_write(7, &format!("n{i}"), T), Ok(()), "at {i}");
        }
    }

    /// Neither map may grow forever: this runs for months on a small machine.
    #[test]
    fn what_falls_out_of_its_window_is_forgotten() {
        let mut g = Guard::default();
        for i in 0..50 {
            g.claim_write(i, &format!("bericht {i}"), T).unwrap();
        }
        assert_eq!(g.sizes(), (50, 50));
        g.claim_write(999, "vers", T + WINDOW_SECS + 1).unwrap();
        assert_eq!(g.sizes(), (1, 1), "only the fresh one is left");
    }
}

impl Guard {
    /// Say once an hour, per station, that it was turned away.
    ///
    /// Design §8 asks for a refused request to be visible with the station on
    /// it. Saying it every time would drown the log it belongs in: a banned
    /// client keeps polling every three seconds and never stops, because not
    /// polling is how it would fail to notice the ban being lifted.
    pub fn note_refusal(&mut self, station_id: i64, now: i64) {
        let last = self.refusal_said.get(&station_id).copied().unwrap_or(0);
        if now - last >= 3600 {
            self.refusal_said.insert(station_id, now);
            log::info!("station {}: request refused - banned", station_id);
        }
    }
}
