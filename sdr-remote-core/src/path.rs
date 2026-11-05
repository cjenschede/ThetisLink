// SPDX-License-Identifier: GPL-2.0-or-later
//! Which way the traffic of a connection takes now (2026-10-08): one source for the names on the server, the desktop
//! and the phone. Here the kinds, their short letters and the
//! rule for "local"; the long names, in the operator's language, live in
//! `sdr-remote-logic` beside the other screen texts.

use std::net::IpAddr;

/// The way a connection's traffic takes now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathKind {
    /// Straight, no relay, to an address on the own network.
    Local,
    /// Straight, no relay, to an address on the internet (a forwarded port).
    Direct,
    /// Through the relay: all traffic over the VPS.
    Relay,
    /// Through the relay while a direct path is being tried (punched, not yet
    /// confirmed): still all traffic over the VPS.
    RelayCoupling,
    /// Coupled through the relay, the traffic now straight (punched).
    DirectViaRelay,
    /// Found through the relay, the traffic now over the own network: app and
    /// server behind the same router (build 142).
    LocalViaRelay,
    /// Through the relay over its TCP line: a network that lets no UDP through,
    /// or "force TCP" on the measurement screen (build 146).
    RelayTcp,
    /// D+R with the relay session gone (an outage, or a stopped monitor): the
    /// traffic carries on straight, but chat and problem reports are not there
    /// until the relay is back. The red R (2.13.1 build 3).
    DirectRelayGone,
    /// The same for L+R.
    LocalRelayGone,
}

impl PathKind {
    /// Every kind, for the tests of every crate that shows them: one list, so a
    /// sixth kind cannot be left out of a test that writes them out by hand.
    /// Production code matches without a catch-all instead.
    pub const ALL: [PathKind; 9] = [
        PathKind::Local,
        PathKind::Direct,
        PathKind::Relay,
        PathKind::RelayCoupling,
        PathKind::DirectViaRelay,
        PathKind::LocalViaRelay,
        PathKind::RelayTcp,
        PathKind::DirectRelayGone,
        PathKind::LocalRelayGone,
    ];

    /// The letters, the same in every language: the server's table, the
    /// measurement file. Coupling is still `R` to the eye; the file tells it
    /// apart with [`PathKind::file_word`], the log with [`PathKind::log_word`].
    /// A gone relay keeps its letters: the screens draw that R in red
    /// ([`PathKind::short_parts`]).
    pub fn short(self) -> &'static str {
        match self {
            PathKind::Local => "L",
            PathKind::Direct => "D",
            PathKind::Relay | PathKind::RelayCoupling => "R",
            PathKind::DirectViaRelay | PathKind::DirectRelayGone => "D+R",
            PathKind::LocalViaRelay | PathKind::LocalRelayGone => "L+R",
            PathKind::RelayTcp => "R (TCP)",
        }
    }

    /// The letters in two parts: in the plain colour, and in red. The red part
    /// is the R of a relay session that is gone, empty for every other kind.
    /// The one place that says which letters are red, for the server's table
    /// and the phone (2.13.1 build 3); joined they are
    /// [`PathKind::short`], which the test holds them to.
    pub fn short_parts(self) -> (&'static str, &'static str) {
        match self {
            PathKind::DirectRelayGone => ("D+", "R"),
            PathKind::LocalRelayGone => ("L+", "R"),
            PathKind::Local
            | PathKind::Direct
            | PathKind::Relay
            | PathKind::RelayCoupling
            | PathKind::DirectViaRelay
            | PathKind::LocalViaRelay
            | PathKind::RelayTcp => (self.short(), ""),
        }
    }

    /// The word in the measurement file: the letters, and coupling
    /// as a state of its own.
    pub fn file_word(self) -> &'static str {
        match self {
            PathKind::RelayCoupling => "R-coupling",
            PathKind::RelayTcp => "R-TCP",
            PathKind::DirectRelayGone => "D+R-gone",
            PathKind::LocalRelayGone => "L+R-gone",
            other => other.short(),
        }
    }

    /// The kind in words, for the log line after its letters (build 139):
    /// English, like every log line. The client's and the server's
    /// log both take it from here (2.13.1 build 7, two lists before), so a gone
    /// relay reads the same in both.
    pub fn log_word(self) -> &'static str {
        match self {
            PathKind::Local => "local",
            PathKind::Direct => "direct",
            PathKind::Relay => "relay",
            PathKind::RelayCoupling => "coupling",
            PathKind::DirectViaRelay => "direct, via relay",
            PathKind::LocalViaRelay => "local, via relay",
            PathKind::RelayTcp => "relay over TCP",
            PathKind::DirectRelayGone => "direct, no relay session",
            PathKind::LocalRelayGone => "local, no relay session",
        }
    }

    /// Whether the relay takes part: then chat and problem reports are there
    /// too (they hang on the relay session).
    pub fn has_relay(self) -> bool {
        matches!(
            self,
            PathKind::Relay | PathKind::RelayCoupling | PathKind::DirectViaRelay | PathKind::LocalViaRelay | PathKind::RelayTcp
        )
    }

    /// Whether the R stands for a relay session that is gone: the screens draw
    /// it in red, and chat and problem reports wait for it.
    pub fn relay_gone(self) -> bool {
        matches!(self, PathKind::DirectRelayGone | PathKind::LocalRelayGone)
    }

    /// The kind of a connection without the relay, by the server's address: on
    /// the own network, or on the internet.
    pub fn without_relay(server: IpAddr) -> PathKind {
        if is_local(server) {
            PathKind::Local
        } else {
            PathKind::Direct
        }
    }
}

/// Whether `ip` is an address on the own network: private (RFC 1918), link-local
/// or loopback, and for IPv6 unique-local (fc00::/7) and link-local (fe80::/10).
/// A carrier's shared range (100.64/10, CGNAT) is not: it is the mobile network,
/// and a server reached there is on the internet. The own public address, typed
/// at home, is not either: the address decides, not the router's way (by
/// design).
pub fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn the_own_network_is_local() {
        // Each IPv6 mask from both ends (fe80::/10 was tested at its
        // bottom only, so /9 stayed green).
        for local in ["192.168.1.50", "10.0.0.5", "172.16.0.1", "172.31.255.254", "169.254.3.4", "127.0.0.1", "::1", "fd12:3456::1", "fc00::1", "fe80::1", "febf:ffff:ffff:ffff::1"] {
            assert!(is_local(ip(local)), "{local}");
        }
    }

    /// The internet, the edges of the private ranges, and the carrier's range
    /// are not local.
    #[test]
    fn the_internet_and_the_mobile_network_are_not() {
        for public in ["203.0.113.97", "8.8.8.8", "172.15.255.255", "172.32.0.0", "100.64.0.1", "100.127.255.254", "11.0.0.1", "2001:db8::1", "3fff::1","fbff:ffff::1", "fec0::1"] {
            assert!(!is_local(ip(public)), "{public}");
        }
    }

    #[test]
    fn every_kind_has_its_letters_and_its_word() {
        use PathKind::*;
        assert_eq!(PathKind::ALL, [Local, Direct, Relay, RelayCoupling, DirectViaRelay, LocalViaRelay, RelayTcp, DirectRelayGone, LocalRelayGone]);
        assert_eq!(PathKind::ALL.map(PathKind::short), ["L", "D", "R", "R", "D+R", "L+R", "R (TCP)", "D+R", "L+R"]);
        assert_eq!(PathKind::ALL.map(PathKind::file_word), ["L", "D", "R", "R-coupling", "D+R", "L+R", "R-TCP", "D+R-gone", "L+R-gone"]);
        assert_eq!(PathKind::ALL.map(PathKind::has_relay), [false, false, true, true, true, true, true, false, false]);
        assert_eq!(PathKind::ALL.map(PathKind::relay_gone), [false, false, false, false, false, false, false, true, true]);
        for k in PathKind::ALL {
            let (plain, red) = k.short_parts();
            assert_eq!(format!("{plain}{red}"), k.short(), "{k:?}: the parts are the letters");
            assert_eq!(!red.is_empty(), k.relay_gone(), "{k:?}: red is the gone relay, and only that");
        }
        assert_eq!(DirectRelayGone.short_parts(), ("D+", "R"));
        assert_eq!(
            PathKind::ALL.map(PathKind::log_word),
            [
                "local",
                "direct",
                "relay",
                "coupling",
                "direct, via relay",
                "local, via relay",
                "relay over TCP",
                "direct, no relay session",
                "local, no relay session",
            ]
        );
        // Every kind its own words: the letters alone do not tell a gone relay
        // or coupling apart, the log line has to.
        for (i, a) in PathKind::ALL.iter().enumerate() {
            for b in &PathKind::ALL[i + 1..] {
                assert_ne!(a.log_word(), b.log_word(), "{a:?} and {b:?} read the same in the log");
            }
        }
        assert_eq!(PathKind::without_relay(ip("192.168.1.50")), Local);
        assert_eq!(PathKind::without_relay(ip("203.0.113.97")), Direct);
    }
}
