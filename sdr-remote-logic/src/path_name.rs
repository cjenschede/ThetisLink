// SPDX-License-Identifier: GPL-2.0-or-later
//! The connection names as both apps show them (2026-10-08): the long form, in the operator's language, for the main
//! screen and the connect screen. The kinds and the letters are in
//! `sdr_remote_core::path`; the phone gets these words through the bridge, so no
//! screen keeps a table of its own.

use sdr_remote_core::path::PathKind;

use crate::i18n::Lang;

/// The long name of `kind`.
pub fn long_name(kind: PathKind, lang: Lang) -> String {
    match kind {
        PathKind::Local => lang.pick("Local", "Lokaal", "Lokal", "Local"),
        PathKind::Direct => lang.pick("Direct", "Direct", "Direkt", "Direct"),
        PathKind::Relay => lang.pick("Relay", "Relay", "Relay", "Relais"),
        PathKind::RelayCoupling => lang.pick(
            "Relay (coupling...)",
            "Relay (koppelen...)",
            "Relay (Kopplung...)",
            "Relais (couplage...)",
        ),
        PathKind::DirectViaRelay | PathKind::DirectRelayGone | PathKind::LocalViaRelay | PathKind::LocalRelayGone => {
            let (lead, relay) = via_relay(kind, lang);
            format!("{lead}{relay}")
        }
        PathKind::RelayTcp => lang.pick("Relay via TCP", "Relay via TCP", "Relay \u{fc}ber TCP", "Relais via TCP"),
    }
}

/// The long name of a kind with its relay session gone, in two parts: the
/// words before the relay, and the relay word that the desktop draws in red.
/// `None` for every other kind. The words are those of
/// [`long_name`]: one source, so the red word cannot drift from the name.
pub fn relay_gone_parts(kind: PathKind, lang: Lang) -> Option<(String, String)> {
    kind.relay_gone().then(|| via_relay(kind, lang))
}

/// "Local, via " and "relay", or "Direct, via " and "relay", in the
/// operator's language.
fn via_relay(kind: PathKind, lang: Lang) -> (String, String) {
    let local = matches!(kind, PathKind::LocalViaRelay | PathKind::LocalRelayGone);
    let lead = if local {
        lang.pick("Local, via ", "Lokaal, via ", "Lokal, \u{fc}ber ", "Local, via le ")
    } else {
        lang.pick("Direct, via ", "Direct, via ", "Direkt, \u{fc}ber ", "Direct, via le ")
    };
    (lead, lang.pick("relay", "relay", "Relay", "relais"))
}

/// The sentence that says chat and problem reports are there: in the hint of
/// every kind with a relay session.
fn chat_available(lang: Lang) -> String {
    lang.pick(
        "Chat and problem reports are available.",
        "Chat en probleem melden zijn beschikbaar.",
        "Chat und Problemmeldungen sind verf\u{fc}gbar.",
        "Le chat et les rapports de probl\u{e8}me sont disponibles.",
    )
}

/// What the name means for the operator, for a hover or a tap: the way, and
/// with the relay in it, that chat and problem reports are there
/// (2026-10-07: the R also says that).
pub fn hint(kind: PathKind, lang: Lang) -> String {
    let way = match kind {
        PathKind::Local => lang.pick(
            "Straight to the server on your own network.",
            "Rechtstreeks naar de server op je eigen netwerk.",
            "Direkt zum Server im eigenen Netzwerk.",
            "Directement vers le serveur sur votre propre r\u{e9}seau.",
        ),
        PathKind::Direct => lang.pick(
            "Straight to the server over the internet.",
            "Rechtstreeks naar de server over internet.",
            "Direkt zum Server \u{fc}ber das Internet.",
            "Directement vers le serveur via Internet.",
        ),
        PathKind::Relay => lang.pick(
            "All traffic goes through the relay.",
            "Al het verkeer gaat via de relay.",
            "Der gesamte Verkehr l\u{e4}uft \u{fc}ber das Relay.",
            "Tout le trafic passe par le relais.",
        ),
        PathKind::RelayCoupling => lang.pick(
            "Through the relay, while a direct path is being tried.",
            "Via de relay, terwijl een rechtstreekse weg geprobeerd wordt.",
            "\u{dc}ber das Relay, w\u{e4}hrend ein direkter Weg versucht wird.",
            "Via le relais, pendant qu'un chemin direct est essay\u{e9}.",
        ),
        PathKind::DirectViaRelay => lang.pick(
            "Coupled through the relay; the traffic now goes straight.",
            "Via de relay gekoppeld; het verkeer gaat nu rechtstreeks.",
            "\u{dc}ber das Relay gekoppelt; der Verkehr l\u{e4}uft jetzt direkt.",
            "Coupl\u{e9} via le relais ; le trafic passe maintenant en direct.",
        ),
        PathKind::LocalViaRelay => lang.pick(
            "Found through the relay; the traffic goes over your own network.",
            "Via de relay gevonden; het verkeer gaat over je eigen netwerk.",
            "\u{dc}ber das Relay gefunden; der Verkehr l\u{e4}uft \u{fc}ber das eigene Netzwerk.",
            "Trouv\u{e9} via le relais ; le trafic passe par votre propre r\u{e9}seau.",
        ),
        PathKind::DirectRelayGone => lang.pick(
            "Straight to the server over the internet; the relay cannot be reached now. Chat and problem reports wait until it is back.",
            "Rechtstreeks naar de server over internet; de relay is nu niet bereikbaar. Chat en probleem melden wachten tot hij terug is.",
            "Direkt zum Server \u{fc}ber das Internet; das Relay ist gerade nicht erreichbar. Chat und Problemmeldungen warten, bis es wieder da ist.",
            "Directement vers le serveur via Internet ; le relais est injoignable pour le moment. Le chat et les rapports de probl\u{e8}me attendent son retour.",
        ),
        PathKind::LocalRelayGone => lang.pick(
            "Over your own network; the relay cannot be reached now. Chat and problem reports wait until it is back.",
            "Over je eigen netwerk; de relay is nu niet bereikbaar. Chat en probleem melden wachten tot hij terug is.",
            "\u{dc}ber das eigene Netzwerk; das Relay ist gerade nicht erreichbar. Chat und Problemmeldungen warten, bis es wieder da ist.",
            "Par votre propre r\u{e9}seau ; le relais est injoignable pour le moment. Le chat et les rapports de probl\u{e8}me attendent son retour.",
        ),
        PathKind::RelayTcp => lang.pick(
            "All traffic goes through the relay over TCP: this network lets no UDP through. More delay.",
            "Al het verkeer gaat via de relay over TCP: dit netwerk laat geen UDP door. Meer vertraging.",
            "Der gesamte Verkehr l\u{e4}uft \u{fc}ber das Relay per TCP: dieses Netzwerk l\u{e4}sst kein UDP durch. Mehr Verz\u{f6}gerung.",
            "Tout le trafic passe par le relais en TCP : ce r\u{e9}seau ne laisse pas passer l'UDP. Plus de d\u{e9}lai.",
        ),
    };
    if kind.has_relay() {
        let chat = chat_available(lang);
        format!("{way} {chat}")
    } else {
        way
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [PathKind; 9] = PathKind::ALL;

    /// Every kind has words in every language, the long names are told apart -
    /// except a gone relay, which keeps its name and is told apart by its red
    /// word and its hint - and the hint says chat is there exactly where the
    /// relay is.
    #[test]
    fn every_name_in_every_language() {
        for lang in Lang::ALL {
            let names: Vec<String> = ALL.iter().filter(|k| !k.relay_gone()).map(|&k| long_name(k, lang)).collect();
            for (i, a) in names.iter().enumerate() {
                assert!(!a.trim().is_empty(), "{lang:?}");
                for b in &names[i + 1..] {
                    assert_ne!(a, b, "{lang:?}: two kinds read the same");
                }
            }
            for k in ALL {
                let h = hint(k, lang);
                assert_eq!(h.contains(&chat_available(lang)), k.has_relay(), "{lang:?} {k:?}: {h}");
                match relay_gone_parts(k, lang) {
                    Some((lead, relay)) => {
                        assert_eq!(format!("{lead}{relay}"), long_name(k, lang), "{lang:?} {k:?}: the parts are the name");
                        assert_eq!(long_name(k, lang), long_name(healthy(k), lang), "{lang:?} {k:?}: the name it has with the relay");
                        assert_ne!(h, hint(healthy(k), lang), "{lang:?} {k:?}: the hint tells a gone relay apart");
                        assert!(!h.contains(&chat_available(lang)), "{lang:?} {k:?}: no chat while the relay is gone");
                    }
                    None => assert!(!k.relay_gone(), "{lang:?} {k:?}"),
                }
            }
            assert_ne!(
                hint(PathKind::DirectRelayGone, lang),
                hint(PathKind::LocalRelayGone, lang),
                "{lang:?}: the hint is all that tells the two gone kinds apart"
            );
        }
        assert_eq!(long_name(PathKind::DirectViaRelay, Lang::Nl), "Direct, via relay");
        assert_eq!(long_name(PathKind::RelayCoupling, Lang::Nl), "Relay (koppelen...)");
        assert_eq!(long_name(PathKind::LocalViaRelay, Lang::Nl), "Lokaal, via relay");
        assert_eq!(long_name(PathKind::RelayTcp, Lang::Nl), "Relay via TCP");
        assert_eq!(relay_gone_parts(PathKind::LocalRelayGone, Lang::Nl), Some(("Lokaal, via ".to_string(), "relay".to_string())));
        assert_eq!(relay_gone_parts(PathKind::DirectRelayGone, Lang::De), Some(("Direkt, \u{fc}ber ".to_string(), "Relay".to_string())));
    }

    /// The kind with the relay session, for one without it.
    fn healthy(k: PathKind) -> PathKind {
        match k {
            PathKind::DirectRelayGone => PathKind::DirectViaRelay,
            PathKind::LocalRelayGone => PathKind::LocalViaRelay,
            other => other,
        }
    }
}
