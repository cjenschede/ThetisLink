// SPDX-License-Identifier: GPL-2.0-or-later
//! Whether this network lets UDP through to the relay (build 137, design phase
//! 3d §7). Since build 133 every connection is QUIC, which is UDP; a network
//! that blocks UDP gets no connection at all, also not through the relay. Over
//! the relay that can be told apart from "the server is down": the sign-in over
//! wss stands (a grant is there), but the relay never says where it sees our
//! UDP (`udp-seen` stays out, `UdpGrant::public` stays `None`), although the
//! route layer sends it a keepalive every second.
//!
//! Without a relay there is nothing to tell it by, and this says nothing.
//! A relay that never sends `udp-seen` (one older than phase 3c) would read as
//! blocked; ours does send it.

use std::time::{Duration, Instant};

use sdr_remote_quic::UdpGrant;

/// How long a relay session may stand without `udp-seen` before UDP counts as
/// blocked. The route layer sends a keepalive every second, so ten without an
/// answer is no lost packet. The VPS counts its own after 30 s; the app says it
/// sooner, because the operator is waiting. Named in the brief before it was
/// built (working rule 11, build 137).
pub const UDP_SEEN_WAIT: Duration = Duration::from_secs(10);

// The band the wait must stay in, checked by the compiler: above a couple of the
// route layer's one-second keepalives, so one lost packet is not a block, and
// below the 30 s the VPS counts with, so the app says it first. The band, not
// the value: 5 s or 20 s still compile and keep every test green (review 137,
// review 138) - the 10 is a choice inside the band.
const _: () = assert!(UDP_SEEN_WAIT.as_secs() >= 2 && UDP_SEEN_WAIT.as_secs() < 30);

/// Follows the engine's relay grant and says when UDP becomes blocked, and
/// when it is seen again.
#[derive(Debug, Default)]
pub struct UdpBlockedWatch {
    session: Option<u64>,
    since: Option<Instant>,
    blocked: bool,
}

impl UdpBlockedWatch {
    /// `Some(blocked)` when that changed, `None` otherwise. `grant` is the
    /// engine's current one (`None`: no relay session).
    ///
    /// - No session: not blocked - the relay itself is out of reach, which is
    ///   another matter and said elsewhere.
    /// - `udp-seen` there: not blocked.
    /// - A session without it for [`UDP_SEEN_WAIT`]: blocked. A new session
    ///   restarts the clock - but a block that stands, stays (`self.blocked ||`
    ///   below, the one place that holds it): a blocked network stays blocked
    ///   over a reconnect, and the notice does not blink.
    pub fn observe(&mut self, grant: Option<&UdpGrant>, now: Instant) -> Option<bool> {
        let blocked = match grant {
            None => {
                self.session = None;
                self.since = None;
                false
            }
            Some(g) if g.public.is_some() => {
                self.session = Some(g.session);
                self.since = None;
                false
            }
            Some(g) => {
                // `since.is_none()`: the same session that lost its `udp-seen`.
                // The relay never takes it back within a session today (it sets
                // `public_seen` once per session), so this is a net for a relay
                // that one day does - then the clock starts there (review 137).
                if self.session != Some(g.session) || self.since.is_none() {
                    self.session = Some(g.session);
                    self.since = Some(now);
                }
                self.blocked || self.since.is_some_and(|t| now.duration_since(t) >= UDP_SEEN_WAIT)
            }
        };
        (blocked != self.blocked).then(|| {
            self.blocked = blocked;
            blocked
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(session: u64, seen: bool) -> UdpGrant {
        UdpGrant {
            relay: "192.0.2.10:443".parse().unwrap(),
            fallback: None,
            public: seen.then(|| "198.51.100.7:40000".parse().unwrap()),
            token: [0; 32],
            session,
        }
    }

    /// Both sides of the threshold: just under it nothing, at it blocked, and
    /// said once.
    #[test]
    fn a_session_without_udp_seen_is_blocked_after_the_wait() {
        let t0 = Instant::now();
        let mut w = UdpBlockedWatch::default();
        let g = grant(1, false);
        assert_eq!(w.observe(Some(&g), t0), None);
        assert_eq!(w.observe(Some(&g), t0 + UDP_SEEN_WAIT - Duration::from_millis(1)), None, "just under");
        assert_eq!(w.observe(Some(&g), t0 + UDP_SEEN_WAIT), Some(true), "at the wait");
        assert_eq!(w.observe(Some(&g), t0 + UDP_SEEN_WAIT * 2), None, "said once");
    }

    /// `udp-seen` arriving clears it, and a session that has it is never blocked.
    #[test]
    fn udp_seen_clears_it_and_prevents_it() {
        let t0 = Instant::now();
        let mut w = UdpBlockedWatch::default();
        w.observe(Some(&grant(1, false)), t0);
        assert_eq!(w.observe(Some(&grant(1, false)), t0 + UDP_SEEN_WAIT), Some(true));
        assert_eq!(w.observe(Some(&grant(1, true)), t0 + UDP_SEEN_WAIT * 2), Some(false), "seen again");
        let mut fresh = UdpBlockedWatch::default();
        assert_eq!(fresh.observe(Some(&grant(2, true)), t0), None);
        assert_eq!(fresh.observe(Some(&grant(2, true)), t0 + UDP_SEEN_WAIT * 3), None, "seen: never blocked");
    }

    /// A new session restarts the clock while not blocked - a reconnect after
    /// nine seconds is not blocked one second later - and keeps a blocked state
    /// over a reconnect, so the notice does not blink.
    #[test]
    fn a_new_session_restarts_the_clock_but_keeps_a_block() {
        let t0 = Instant::now();
        let mut w = UdpBlockedWatch::default();
        w.observe(Some(&grant(1, false)), t0);
        let t1 = t0 + UDP_SEEN_WAIT - Duration::from_secs(1);
        assert_eq!(w.observe(Some(&grant(2, false)), t1), None, "a new session");
        assert_eq!(w.observe(Some(&grant(2, false)), t0 + UDP_SEEN_WAIT), None, "its own clock, not the old one");
        assert_eq!(w.observe(Some(&grant(2, false)), t1 + UDP_SEEN_WAIT), Some(true));
        assert_eq!(w.observe(Some(&grant(3, false)), t1 + UDP_SEEN_WAIT * 2), None, "still blocked after a reconnect");
    }

    /// No session at all: not blocked (the relay is out of reach - another
    /// matter), and a block that stood goes.
    #[test]
    fn without_a_session_nothing_is_blocked() {
        let t0 = Instant::now();
        let mut w = UdpBlockedWatch::default();
        assert_eq!(w.observe(None, t0 + UDP_SEEN_WAIT * 5), None);
        w.observe(Some(&grant(1, false)), t0);
        assert_eq!(w.observe(Some(&grant(1, false)), t0 + UDP_SEEN_WAIT), Some(true));
        assert_eq!(w.observe(None, t0 + UDP_SEEN_WAIT * 2), Some(false));
    }
}
