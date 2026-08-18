// SPDX-License-Identifier: GPL-2.0-or-later
//
//! Logging in to the postbox from a browser.
//!
//! The endpoints underneath take a bearer token, which is right for a script and
//! wrong for a page: a token in a URL ends up in history and in logs, and a
//! token pasted on every visit gets written down somewhere worse. So a browser
//! trades it once for a session cookie.
//!
//! Deliberately the same shape as the relay's own dashboard — HttpOnly, Secure,
//! SameSite=Strict, a sliding idle timeout, a CSRF token required on anything
//! that changes something, and a limit on failed attempts per address. Not
//! because this is more precious than the relay, but because it faces the same
//! internet, and an admin login that is nearly like the other one is the kind of
//! difference nobody remembers when it matters.
//!
//! What it does NOT do is grow a user table. There is one administrator and one
//! secret, already in the container's environment.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Idle timeout. Slides on every authenticated request, so reading a long report
/// does not log you out, but a tab left open on a shared machine does.
pub const SESSION_IDLE_TTL: Duration = Duration::from_secs(30 * 60);

/// How many failures before an address is made to wait, and how long.
const MAX_FAILS: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(15 * 60);

pub struct Session {
    pub csrf: String,
    expires_at: Instant,
}

#[derive(Default)]
pub struct Sessions {
    live: HashMap<String, Session>,
    fails: HashMap<String, (u32, Instant)>,
}

impl Sessions {
    /// Is this address currently being made to wait?
    pub fn is_blocked(&mut self, ip: &str, now: Instant) -> bool {
        match self.fails.get(ip) {
            Some((n, until)) => *n >= MAX_FAILS && *until > now,
            None => false,
        }
    }

    pub fn record_fail(&mut self, ip: &str, now: Instant) {
        let e = self.fails.entry(ip.to_string()).or_insert((0, now));
        e.0 += 1;
        e.1 = now + LOCKOUT;
    }

    pub fn reset_fails(&mut self, ip: &str) {
        self.fails.remove(ip);
    }

    /// Start a session. Returns (cookie value, CSRF token).
    pub fn open(&mut self, token: String, csrf: String, now: Instant) -> (String, String) {
        // Housekeeping on the way in, so an abandoned tab does not sit in memory
        // until the process restarts.
        self.live.retain(|_, s| s.expires_at > now);
        self.live.insert(
            token.clone(),
            Session { csrf: csrf.clone(), expires_at: now + SESSION_IDLE_TTL },
        );
        (token, csrf)
    }

    /// Validate a cookie and slide its timeout. Returns the session's CSRF token.
    pub fn validate(&mut self, token: &str, now: Instant) -> Option<String> {
        let s = self.live.get_mut(token)?;
        if s.expires_at <= now {
            self.live.remove(token);
            return None;
        }
        s.expires_at = now + SESSION_IDLE_TTL;
        Some(s.csrf.clone())
    }

    pub fn close(&mut self, token: &str) {
        self.live.remove(token);
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.live.len()
    }
}

/// Compare two secrets without giving away where they start to differ.
///
/// The difference is unmeasurable across the internet in practice; it costs four
/// lines, and reasoning about when it stops being true is more expensive than
/// just doing it.
pub fn secret_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A random token, from the system's own source.
///
/// Session identifiers are guessable if their source is: seeding a generator
/// from the clock in a container that starts at a predictable moment is how that
/// goes wrong.
pub fn gen_token() -> String {
    let mut buf = [0u8; 32];
    getrandom(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn getrandom(buf: &mut [u8]) {
    // The crate rather than /dev/urandom by hand: the service only ever runs in
    // a Linux container, but its tests run wherever they are written, and a
    // security property that cannot be tested on the machine doing the work is
    // one that stops being tested.
    getrandom::getrandom(buf).expect("the system random source is unavailable");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_slides_while_it_is_used_and_dies_when_it_is_not() {
        let mut s = Sessions::default();
        let t0 = Instant::now();
        s.open("tok".into(), "csrf".into(), t0);
        // Used just before the deadline: it lives on.
        let later = t0 + SESSION_IDLE_TTL - Duration::from_secs(1);
        assert_eq!(s.validate("tok", later).as_deref(), Some("csrf"));
        // And from there it has a fresh full timeout.
        let later2 = later + SESSION_IDLE_TTL - Duration::from_secs(1);
        assert!(s.validate("tok", later2).is_some());
        // Left alone past it: gone, and not merely reported gone.
        let dead = later2 + SESSION_IDLE_TTL + Duration::from_secs(1);
        assert!(s.validate("tok", dead).is_none());
        assert_eq!(s.len(), 0, "an expired session is dropped, not kept");
    }

    #[test]
    fn failed_attempts_make_an_address_wait() {
        let mut s = Sessions::default();
        let now = Instant::now();
        for _ in 0..MAX_FAILS {
            assert!(!s.is_blocked("1.2.3.4", now));
            s.record_fail("1.2.3.4", now);
        }
        assert!(s.is_blocked("1.2.3.4", now));
        // Only that address.
        assert!(!s.is_blocked("5.6.7.8", now));
        // And it lets go again.
        assert!(!s.is_blocked("1.2.3.4", now + LOCKOUT + Duration::from_secs(1)));
    }

    /// A success clears the count, or a busy administrator locks themselves out
    /// on a day they mistyped it a few times.
    #[test]
    fn getting_it_right_clears_the_count() {
        let mut s = Sessions::default();
        let now = Instant::now();
        for _ in 0..MAX_FAILS - 1 {
            s.record_fail("1.2.3.4", now);
        }
        s.reset_fails("1.2.3.4");
        for _ in 0..MAX_FAILS - 1 {
            s.record_fail("1.2.3.4", now);
        }
        assert!(!s.is_blocked("1.2.3.4", now));
    }

    #[test]
    fn secrets_of_different_lengths_are_not_equal() {
        assert!(secret_eq("abc", "abc"));
        assert!(!secret_eq("abc", "abd"));
        assert!(!secret_eq("abc", "abcd"));
        assert!(!secret_eq("", "x"));
    }
}
