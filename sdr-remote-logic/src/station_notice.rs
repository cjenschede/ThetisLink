// SPDX-License-Identifier: GPL-2.0-or-later
//! What the screens say about a state of the station that blocks the operator,
//! and what helps - in one place (working rule 18, owner 2026-10-01).
//!
//! Which notices stand, in which order, in which words in every language, and
//! whether Connect may be offered: all of it here. Both front ends - the desktop
//! and, through the bridge, Android - render the result and decide nothing.
//! Build 59 had the rule in four screens with their own if/else chains; one of
//! them hid the audio error behind the new text, and one screen (the Android
//! wizard) did not have the rule at all (review 59).
//!
//! The kinds (`StationNotice`) and the list of them (`station_notices`) are
//! crate-private: a screen gets the lines and the one yes/no, nothing it could
//! build an if/else chain on. A fourth desktop screen matching on the kinds
//! compiled, with the counts green (review 64); Android never could,
//! because the bridge carries strings.

use crate::i18n::Lang;
use crate::state::RadioState;

/// A state the operator has to know about to understand why the station does
/// not do what is asked, and what cures it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StationNotice {
    /// The engine thread stopped (`RadioState::engine_dead`): nothing this client
    /// sends arrives anywhere. Cure: restart the app.
    EngineStopped,
    /// The server closed transmitting until it is restarted
    /// (`RadioState::server_tx_closed`). Cure: restart the server.
    ServerClosed,
    /// This network lets no UDP through to the relay (`RadioState::udp_blocked`,
    /// build 137): signed in, but no connection is possible here. Cure: another
    /// network. The Dutch is the sentence of the phase 3d design, section 7.
    UdpBlocked,
    /// The Amplitec position is receive-only and Thetis cannot refuse
    /// transmitting by itself (`RadioState::rx_only_unguarded`, build 215): the
    /// server stops only a TCI PTT. Cure: the ThetisLink Thetis with its
    /// extensions on; until then no footswitch or MOX in Thetis.
    RxOnlyUnguarded,
}

impl StationNotice {
    /// Every notice, for tests.
    #[cfg(test)]
    pub(crate) const ALL: [StationNotice; 4] =
        [StationNotice::EngineStopped, StationNotice::ServerClosed, StationNotice::UdpBlocked, StationNotice::RxOnlyUnguarded];

    /// The words, in the operator's language. One line each: what is wrong,
    /// and what helps.
    pub(crate) fn text(self, lang: Lang) -> String {
        match self {
            StationNotice::EngineStopped => lang.pick(
                "Connection stopped by an internal error - restart the app",
                "Verbinding gestopt door een interne fout - start de app opnieuw",
                "Verbindung durch einen internen Fehler gestoppt - App neu starten",
                "Connexion arr\u{ea}t\u{e9}e par une erreur interne - red\u{e9}marrez l'application",
            ),
            StationNotice::ServerClosed => lang.pick(
                "Server in safe mode - transmitting is off until the server is restarted",
                "Server in veilige stand - zenden uit tot de server opnieuw is gestart",
                "Server im sicheren Modus - Senden aus, bis der Server neu gestartet ist",
                "Serveur en mode s\u{fb}r - \u{e9}mission coup\u{e9}e jusqu'au red\u{e9}marrage du serveur",
            ),
            // Since build 146 the TCP fallback route carries the connection: a
            // note on the way, no longer an end.
            StationNotice::UdpBlocked => lang.pick(
                "This network lets no UDP through; ThetisLink uses a fallback route over TCP (more delay)",
                "Dit netwerk laat geen UDP door; ThetisLink gebruikt een noodroute via TCP (meer vertraging)",
                "Dieses Netzwerk l\u{e4}sst kein UDP durch; ThetisLink nutzt einen Notweg \u{fc}ber TCP (mehr Verz\u{f6}gerung)",
                "Ce r\u{e9}seau ne laisse pas passer l'UDP ; ThetisLink utilise une route de secours en TCP (plus de d\u{e9}lai)",
            ),
            // The server's Amplitec window shows the same line (build 217): the
            // words live in core, which both crates use.
            StationNotice::RxOnlyUnguarded => {
                sdr_remote_core::rx_only_notice::text(lang.code()).to_string()
            }
        }
    }
}

/// The notices that stand now, most urgent first. Each is its own line on the
/// screen: none replaces another, and none replaces a text that is not here
/// (the audio error, "PTT blocked").
pub(crate) fn station_notices(s: &RadioState) -> Vec<StationNotice> {
    let mut out = Vec::new();
    if s.engine_dead {
        out.push(StationNotice::EngineStopped);
    }
    // A dead engine has no session, so a closed server cannot also stand -
    // `engine_stopped` clears the session. Not relied on: both would show.
    if s.server_tx_closed {
        out.push(StationNotice::ServerClosed);
    }
    if s.udp_blocked {
        out.push(StationNotice::UdpBlocked);
    }
    if s.rx_only_unguarded {
        out.push(StationNotice::RxOnlyUnguarded);
    }
    out
}

/// Whether a screen may offer Connect. A stopped engine takes no command, so a
/// Connect button would send into a closed channel and do nothing.
pub fn can_offer_connect(s: &RadioState) -> bool {
    !s.engine_dead
}

/// The notices as text, one line each, for every screen - the desktop draws
/// them as they come, Android gets them through the bridge.
pub fn station_notice_lines(s: &RadioState, lang: Lang) -> Vec<String> {
    station_notices(s).into_iter().map(|n| n.text(lang)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_stands_on_a_healthy_station() {
        let s = RadioState::default();
        assert!(station_notices(&s).is_empty());
        assert!(can_offer_connect(&s));
    }

    /// Each notice is its own line: one standing does not hide the other.
    #[test]
    fn every_notice_that_applies_stands_and_none_hides_another() {
        let mut s = RadioState::default();
        s.server_tx_closed = true;
        assert_eq!(station_notices(&s), vec![StationNotice::ServerClosed]);
        assert!(can_offer_connect(&s), "a closed server is no reason to refuse Connect");
        s.engine_dead = true;
        assert_eq!(
            station_notices(&s),
            vec![StationNotice::EngineStopped, StationNotice::ServerClosed],
            "the most urgent first, and both"
        );
    }

    /// An RX-only position Thetis cannot guard is a line of its own, and no
    /// reason to refuse Connect (build 215).
    #[test]
    fn an_unguarded_rx_only_position_stands_on_its_own_line() {
        let mut s = RadioState::default();
        s.rx_only_unguarded = true;
        assert_eq!(station_notices(&s), vec![StationNotice::RxOnlyUnguarded]);
        assert!(can_offer_connect(&s));
        s.udp_blocked = true;
        assert_eq!(station_notices(&s), vec![StationNotice::UdpBlocked, StationNotice::RxOnlyUnguarded]);
        assert_eq!(
            station_notice_lines(&{ let mut s = RadioState::default(); s.rx_only_unguarded = true; s }, Lang::Nl),
            vec!["Deze antennestand is alleen voor ontvangst, en deze Thetis (ouder dan v2.10.3.14, zonder de ThetisLink-uitbreidingen) kan zenden niet zelf weigeren - gebruik in Thetis geen voetschakelaar of MOX".to_string()]
        );
        // Set from the heartbeat, each one: a server that stops saying it clears it.
        let engine = include_str!("engine.rs");
        assert_eq!(
            engine.matches("state.rx_only_unguarded = ack.state_flags.has(").count(),
            1
        );
    }

    /// Blocked UDP is a line of its own, after the others, and no reason to
    /// refuse Connect - a network change may cure it.
    #[test]
    fn blocked_udp_stands_on_its_own_line() {
        let mut s = RadioState::default();
        s.udp_blocked = true;
        assert_eq!(station_notices(&s), vec![StationNotice::UdpBlocked]);
        assert!(can_offer_connect(&s));
        s.server_tx_closed = true;
        assert_eq!(station_notices(&s), vec![StationNotice::ServerClosed, StationNotice::UdpBlocked]);
        assert_eq!(
            station_notice_lines(&{ let mut s = RadioState::default(); s.udp_blocked = true; s }, Lang::Nl),
            vec!["Dit netwerk laat geen UDP door; ThetisLink gebruikt een noodroute via TCP (meer vertraging)".to_string()]
        );
    }

    #[test]
    fn a_dead_engine_is_not_offered_connect() {
        let mut s = RadioState::default();
        s.engine_stopped();
        assert!(!can_offer_connect(&s));
        assert_eq!(station_notices(&s), vec![StationNotice::EngineStopped]);
    }

    /// Every notice says something in every language, and not the same thing
    /// twice.
    #[test]
    fn every_notice_has_its_own_words_in_every_language() {
        for lang in Lang::ALL {
            let texts: Vec<String> = StationNotice::ALL.iter().map(|n| n.text(lang)).collect();
            for t in &texts {
                assert!(!t.trim().is_empty(), "{:?}: empty", lang);
            }
            for i in 0..texts.len() {
                for j in (i + 1)..texts.len() {
                    assert_ne!(texts[i], texts[j], "{:?}: two notices read the same", lang);
                }
            }
        }
        assert_eq!(
            station_notice_lines(&{ let mut s = RadioState::default(); s.server_tx_closed = true; s }, Lang::Nl),
            vec!["Server in veilige stand - zenden uit tot de server opnieuw is gestart".to_string()]
        );
    }
}
