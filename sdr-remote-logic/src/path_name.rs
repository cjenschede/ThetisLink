// SPDX-License-Identifier: GPL-2.0-or-later
//! The connection names as both apps show them (`OPZET-verbindingsnamen.md`,
//! approved 2026-10-08): the long form, in the operator's language, for the main
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
        PathKind::DirectViaRelay => lang.pick(
            "Direct, via relay",
            "Direct, via relay",
            "Direkt, \u{fc}ber Relay",
            "Direct, via le relais",
        ),
        PathKind::LocalViaRelay => lang.pick(
            "Local, via relay",
            "Lokaal, via relay",
            "Lokal, \u{fc}ber Relay",
            "Local, via le relais",
        ),
        PathKind::RelayTcp => lang.pick("Relay via TCP", "Relay via TCP", "Relay \u{fc}ber TCP", "Relais via TCP"),
    }
}

/// What the name means for the operator, for a hover or a tap: the way, and
/// with the relay in it, that chat and problem reports are there (owner
/// 2026-10-07: the R also says that).
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
        PathKind::RelayTcp => lang.pick(
            "All traffic goes through the relay over TCP: this network lets no UDP through. More delay.",
            "Al het verkeer gaat via de relay over TCP: dit netwerk laat geen UDP door. Meer vertraging.",
            "Der gesamte Verkehr l\u{e4}uft \u{fc}ber das Relay per TCP: dieses Netzwerk l\u{e4}sst kein UDP durch. Mehr Verz\u{f6}gerung.",
            "Tout le trafic passe par le relais en TCP : ce r\u{e9}seau ne laisse pas passer l'UDP. Plus de d\u{e9}lai.",
        ),
    };
    if kind.has_relay() {
        let chat = lang.pick(
            "Chat and problem reports are available.",
            "Chat en probleem melden zijn beschikbaar.",
            "Chat und Problemmeldungen sind verf\u{fc}gbar.",
            "Le chat et les rapports de probl\u{e8}me sont disponibles.",
        );
        format!("{way} {chat}")
    } else {
        way
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [PathKind; 7] = PathKind::ALL;

    /// Every kind has words in every language, the long names are told apart,
    /// and the hint names chat exactly where the relay is.
    #[test]
    fn every_name_in_every_language() {
        for lang in Lang::ALL {
            let names: Vec<String> = ALL.iter().map(|&k| long_name(k, lang)).collect();
            for (i, a) in names.iter().enumerate() {
                assert!(!a.trim().is_empty(), "{lang:?}");
                for b in &names[i + 1..] {
                    assert_ne!(a, b, "{lang:?}: two kinds read the same");
                }
            }
            for k in ALL {
                let h = hint(k, lang);
                let chat_word = lang.pick("Chat", "Chat", "Chat", "chat");
                assert_eq!(h.contains(&chat_word), k.has_relay(), "{lang:?} {k:?}: {h}");
            }
        }
        assert_eq!(long_name(PathKind::DirectViaRelay, Lang::Nl), "Direct, via relay");
        assert_eq!(long_name(PathKind::RelayCoupling, Lang::Nl), "Relay (koppelen...)");
        assert_eq!(long_name(PathKind::LocalViaRelay, Lang::Nl), "Lokaal, via relay");
        assert_eq!(long_name(PathKind::RelayTcp, Lang::Nl), "Relay via TCP");
    }
}
