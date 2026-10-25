// SPDX-License-Identifier: GPL-2.0-or-later
//! Which way the traffic of a connection takes now (`OPZET-verbindingsnamen.md`,
//! approved 2026-10-08): one source for the names on the server, the desktop
//! and the phone (working rule 18). Here the kinds, their short letters and the
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
    /// server behind the same router (build 142, `OPZET-L-plus-R.md`).
    LocalViaRelay,
    /// Through the relay over its TCP line: a network that lets no UDP through,
    /// or "force TCP" on the measurement screen (build 146,
    /// `OPZET-tcp-uitwijkroute.md`).
    RelayTcp,
}

impl PathKind {
    /// Every kind, for the tests of every crate that shows them: one list, so a
    /// sixth kind cannot be left out of a test that writes them out by hand
    /// (review 139). Production code matches without a catch-all instead.
    pub const ALL: [PathKind; 7] = [
        PathKind::Local,
        PathKind::Direct,
        PathKind::Relay,
        PathKind::RelayCoupling,
        PathKind::DirectViaRelay,
        PathKind::LocalViaRelay,
        PathKind::RelayTcp,
    ];

    /// The letters, the same in every language: the server's table, the
    /// measurement file. Coupling is still `R` to the eye; the file and the log
    /// tell it apart with [`PathKind::file_word`].
    pub fn short(self) -> &'static str {
        match self {
            PathKind::Local => "L",
            PathKind::Direct => "D",
            PathKind::Relay | PathKind::RelayCoupling => "R",
            PathKind::DirectViaRelay => "D+R",
            PathKind::LocalViaRelay => "L+R",
            PathKind::RelayTcp => "R (TCP)",
        }
    }

    /// The word in the measurement file and the log: the letters, and coupling
    /// as a state of its own.
    pub fn file_word(self) -> &'static str {
        match self {
            PathKind::RelayCoupling => "R-coupling",
            PathKind::RelayTcp => "R-TCP",
            other => other.short(),
        }
    }

    /// Whether the relay takes part: then chat and problem reports are there
    /// too (they hang on the relay session, owner 2026-10-07).
    pub fn has_relay(self) -> bool {
        matches!(
            self,
            PathKind::Relay | PathKind::RelayCoupling | PathKind::DirectViaRelay | PathKind::LocalViaRelay | PathKind::RelayTcp
        )
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
/// at home, is not either: the address decides, not the router's way (owner
/// 2026-10-08).
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
        // Each IPv6 mask from both ends (review 139: fe80::/10 was tested at its
        // bottom only, so /9 stayed green).
        for local in ["192.168.1.97", "10.0.0.5", "172.16.0.1", "172.31.255.254", "169.254.3.4", "127.0.0.1", "::1", "fd12:3456::1", "fc00::1", "fe80::1", "febf:ffff:ffff:ffff::1"] {
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
        assert_eq!(PathKind::ALL, [Local, Direct, Relay, RelayCoupling, DirectViaRelay, LocalViaRelay, RelayTcp]);
        assert_eq!(PathKind::ALL.map(PathKind::short), ["L", "D", "R", "R", "D+R", "L+R", "R (TCP)"]);
        assert_eq!(PathKind::ALL.map(PathKind::file_word), ["L", "D", "R", "R-coupling", "D+R", "L+R", "R-TCP"]);
        assert_eq!(PathKind::ALL.map(PathKind::has_relay), [false, false, true, true, true, true, true]);
        assert_eq!(PathKind::without_relay(ip("192.168.1.97")), Local);
        assert_eq!(PathKind::without_relay(ip("203.0.113.97")), Direct);
    }
}
