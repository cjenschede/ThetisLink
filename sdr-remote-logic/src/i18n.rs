// SPDX-License-Identifier: GPL-2.0-or-later
//
// Centralized text-strings for connect-status / connect-error display.
//
// Single source of truth for both desktop (egui) and Android (Compose via
// UniFFI bridge). UI code never has hard-coded user-visible strings.
//
// # Why every message names all four languages in one place
//
// This used to be `match (err, lang)`: one arm per message per language. At two
// languages that was merely repetitive. At four it would be eighty arms, and
// the failure mode is silent - a `_ =>` or a missing arm hands the reader
// English while the rest of the app is in their own language, which is exactly
// what happened to the whole connect path and the first-run wizard until
// 2026-08-20.
//
// So each message is one arm that names all four texts through `Lang::pick`.
// Adding a fifth language changes that signature, and every message that has
// not been translated is then a compile error rather than a silent fallback.

use crate::state::{ConnectError, ConnectStatus};

/// Display language for connect-status / connect-error strings.
///
/// English is the base and the fallback: [`Lang::from_code`] answers `En` for
/// anything ThetisLink has no translation for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Nl,
    De,
    Fr,
}

impl Default for Lang {
    fn default() -> Self {
        Lang::En
    }
}

impl Lang {
    /// Every language, for tests and for anything that has to cover them all.
    pub const ALL: [Lang; 4] = [Lang::En, Lang::Nl, Lang::De, Lang::Fr];

    /// The code as it is stored in `thetislink-client.conf` and handed over
    /// the Android bridge.
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Nl => "nl",
            Lang::De => "de",
            Lang::Fr => "fr",
        }
    }

    /// Read a stored language code. Anything unknown - or empty - is English.
    ///
    /// Call sites used to write `if code == "nl" { Nl } else { En }` in three
    /// places, which is how German and French readers kept getting English here
    /// long after the rest of the app spoke their language. One function now,
    /// so a new language reaches every screen at once.
    pub fn from_code(code: &str) -> Self {
        match code.trim().to_ascii_lowercase().as_str() {
            "nl" => Lang::Nl,
            "de" => Lang::De,
            "fr" => Lang::Fr,
            _ => Lang::En,
        }
    }

    /// The text for this language, out of all four.
    ///
    /// Taking all four by argument is the point: a message cannot be added
    /// half-translated, and a fifth language is a compile error at every call
    /// site rather than a quiet fallback to English.
    pub fn pick(self, en: &str, nl: &str, de: &str, fr: &str) -> String {
        match self {
            Lang::En => en,
            Lang::Nl => nl,
            Lang::De => de,
            Lang::Fr => fr,
        }
        .to_string()
    }
}

/// Which client UI the text is rendered on. Some hints point at
/// platform-specific UI elements (e.g. the desktop has a "Thetis" tab
/// with a Start button; the Android app puts the Power button on the
/// main Radio screen). Defaulting to `Desktop` matches the original
/// PATCH-1 wording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Desktop,
    Mobile,
}

impl Default for Platform {
    fn default() -> Self {
        Platform::Desktop
    }
}

/// User-visible text for a connect-status.
///
/// Returns `(headline, suggested_action)`. `headline` is the short
/// one-liner. `suggested_action` is an optional hint ("Check the IP
/// and firewall on the server PC") - `None` when nothing useful can
/// be advised (e.g. `Connecting`).
pub fn connect_status_text(
    status: &ConnectStatus,
    lang: Lang,
    platform: Platform,
) -> (String, Option<String>) {
    match status {
        ConnectStatus::Disconnected => (
            lang.pick(
                "Disconnected",
                "Niet verbonden",
                "Nicht verbunden",
                "Déconnecté",
            ),
            None,
        ),
        ConnectStatus::Connecting => (
            lang.pick(
                "Connecting…",
                "Bezig met verbinden…",
                "Verbindung wird hergestellt…",
                "Connexion en cours…",
            ),
            None,
        ),
        ConnectStatus::AwaitingTotp => (
            lang.pick(
                "Enter 2FA code",
                "Voer 2FA-code in",
                "2FA-Code eingeben",
                "Saisissez le code 2FA",
            ),
            Some(lang.pick(
                "Open your authenticator app and enter the 6-digit code.",
                "Open je authenticator-app en voer de 6-cijferige code in.",
                "Öffnen Sie Ihre Authenticator-App und geben Sie den 6-stelligen Code ein.",
                "Ouvrez votre application d'authentification et saisissez le code à 6 chiffres.",
            )),
        ),
        ConnectStatus::Connected => (
            lang.pick("Connected", "Verbonden", "Verbunden", "Connecté"),
            None,
        ),
        ConnectStatus::Failed(err) => connect_error_text(err, lang, platform),
    }
}

/// User-visible text for a `ConnectError`. Returns `(headline, suggested_action)`.
pub fn connect_error_text(
    err: &ConnectError,
    lang: Lang,
    platform: Platform,
) -> (String, Option<String>) {
    match err {
        ConnectError::DnsResolutionFailed { host, .. } => (
            lang.pick(
                &format!("Server name not found: {host}"),
                &format!("Servernaam niet gevonden: {host}"),
                &format!("Servername nicht gefunden: {host}"),
                &format!("Nom du serveur introuvable : {host}"),
            ),
            Some(lang.pick(
                "Check the spelling of the server address.",
                "Controleer of het serveradres correct is gespeld.",
                "Prüfen Sie die Schreibweise der Serveradresse.",
                "Vérifiez l'orthographe de l'adresse du serveur.",
            )),
        ),

        ConnectError::NoUdpResponse { addr, .. } => (
            lang.pick(
                &format!("Server not reachable at {addr}"),
                &format!("Server niet bereikbaar op {addr}"),
                &format!("Server unter {addr} nicht erreichbar"),
                &format!("Serveur injoignable à {addr}"),
            ),
            Some(lang.pick(
                "Check that the server is running, the IP/port is correct, \
                 and the firewall allows UDP traffic on the server PC.",
                "Controleer of de server draait, IP/poort kloppen, \
                 en de firewall UDP-verkeer toestaat op de server-PC.",
                "Prüfen Sie, ob der Server läuft, ob IP und Port stimmen \
                 und ob die Firewall auf dem Server-PC UDP-Verkehr zulässt.",
                "Vérifiez que le serveur fonctionne, que l'IP et le port sont corrects, \
                 et que le pare-feu du PC serveur autorise le trafic UDP.",
            )),
        ),

        ConnectError::MalformedResponse { addr, .. } => (
            lang.pick(
                &format!("Unexpected response from {addr}"),
                &format!("Onverwacht antwoord van {addr}"),
                &format!("Unerwartete Antwort von {addr}"),
                &format!("Réponse inattendue de {addr}"),
            ),
            Some(lang.pick(
                "The address responds but does not speak the ThetisLink protocol. \
                 Check that the port number is correct.",
                "Het adres reageert maar gebruikt geen ThetisLink-protocol. \
                 Controleer of het poortnummer klopt.",
                "Die Adresse antwortet, spricht aber nicht das ThetisLink-Protokoll. \
                 Prüfen Sie, ob die Portnummer stimmt.",
                "L'adresse répond mais ne parle pas le protocole ThetisLink. \
                 Vérifiez que le numéro de port est correct.",
            )),
        ),

        ConnectError::WrongPassword => (
            lang.pick(
                "Wrong password",
                "Verkeerd wachtwoord",
                "Falsches Passwort",
                "Mot de passe incorrect",
            ),
            Some(lang.pick(
                "Check the password configured on the server.",
                "Controleer het wachtwoord dat op de server is ingesteld.",
                "Prüfen Sie das auf dem Server eingestellte Passwort.",
                "Vérifiez le mot de passe configuré sur le serveur.",
            )),
        ),

        ConnectError::WrongTotp => (
            lang.pick(
                "Wrong 2FA code",
                "Verkeerde 2FA-code",
                "Falscher 2FA-Code",
                "Code 2FA incorrect",
            ),
            Some(lang.pick(
                "Check the 6-digit code in your authenticator app and try again.",
                "Controleer de 6-cijferige code in je authenticator-app en probeer opnieuw.",
                "Prüfen Sie den 6-stelligen Code in Ihrer Authenticator-App und \
                 versuchen Sie es erneut.",
                "Vérifiez le code à 6 chiffres dans votre application d'authentification \
                 et réessayez.",
            )),
        ),

        // Which side is behind is the whole message here, so the two cases get
        // their own headline rather than one "version mismatch" for both.
        ConnectError::ProtocolVersionMismatch {
            server_version,
            client_version,
        } if server_version > client_version => (
            lang.pick(
                "Client is too old",
                "Client is te oud",
                "Client ist zu alt",
                "Le client est trop ancien",
            ),
            Some(lang.pick(
                &format!(
                    "The server uses protocol version {server_version}, but this client \
                     uses version {client_version}. Please update the client."
                ),
                &format!(
                    "De server gebruikt protocolversie {server_version}, maar deze client \
                     gebruikt versie {client_version}. Update de client."
                ),
                &format!(
                    "Der Server verwendet Protokollversion {server_version}, dieser Client \
                     jedoch Version {client_version}. Bitte aktualisieren Sie den Client."
                ),
                &format!(
                    "Le serveur utilise la version {server_version} du protocole, mais ce \
                     client utilise la version {client_version}. Veuillez mettre à jour le client."
                ),
            )),
        ),

        ConnectError::ProtocolVersionMismatch {
            server_version,
            client_version,
        } => (
            lang.pick(
                "Server is too old",
                "Server is te oud",
                "Server ist zu alt",
                "Le serveur est trop ancien",
            ),
            Some(lang.pick(
                &format!(
                    "This client uses protocol version {client_version}, but the server \
                     uses version {server_version}. Please update the server."
                ),
                &format!(
                    "Deze client gebruikt protocolversie {client_version}, maar de server \
                     gebruikt versie {server_version}. Update de server."
                ),
                &format!(
                    "Dieser Client verwendet Protokollversion {client_version}, der Server \
                     jedoch Version {server_version}. Bitte aktualisieren Sie den Server."
                ),
                &format!(
                    "Ce client utilise la version {client_version} du protocole, mais le \
                     serveur utilise la version {server_version}. Veuillez mettre à jour le serveur."
                ),
            )),
        ),

        // Headline + hint branch on what the server reports about Thetis.exe:
        //   Some(true)  -> Thetis runs, TCI is down -> check TCI settings
        //   Some(false) -> Thetis is not running    -> use the client's launch
        //                  control; where that control is depends on platform
        //   None        -> old server, no hint      -> generic fallback
        ConnectError::TciUnreachable {
            server_reported_detail,
            thetis_process_running,
            ..
        } => {
            // Per-platform pointer to the Thetis-launch control. Desktop:
            // dedicated "Thetis" tab with a Start button. Android: the
            // Power button on the Radio screen (no extra tab).
            let launch_hint = match platform {
                Platform::Desktop => lang.pick(
                    "Open the Thetis tab in this client and press Start",
                    "Open de Thetis-tab in deze client en druk op Start",
                    "Öffnen Sie den Thetis-Tab in diesem Client und drücken Sie Start",
                    "Ouvrez l'onglet Thetis dans ce client et appuyez sur Start",
                ),
                Platform::Mobile => lang.pick(
                    "Tap the Power button on the Radio screen",
                    "Tik op de Power-knop in het Radio-scherm",
                    "Tippen Sie im Radio-Bildschirm auf die Power-Taste",
                    "Touchez le bouton Power sur l'écran Radio",
                ),
            };
            // The headline names Thetis explicitly, never "radio": a station
            // with one or two Yaesu radios attached reads "radio not reachable"
            // as "my Yaesu is down", while this error is only ever about Thetis.
            let (headline, action) = match (thetis_process_running, server_reported_detail) {
                (Some(true), _) => (
                    lang.pick(
                        "Thetis TCI not connected",
                        "Thetis TCI niet verbonden",
                        "Thetis-TCI nicht verbunden",
                        "TCI Thetis non connecté",
                    ),
                    lang.pick(
                        "Thetis is running on the server PC, but its TCI server is not \
                         connected. In Thetis: open Setup → Network → TCI and make sure \
                         the TCI server is enabled.",
                        "Thetis draait op de server-PC, maar de TCI-server is niet \
                         verbonden. In Thetis: open Setup → Network → TCI en zorg dat de \
                         TCI-server aan staat.",
                        "Thetis läuft auf dem Server-PC, aber sein TCI-Server ist nicht \
                         verbunden. In Thetis: Setup → Network → TCI öffnen und \
                         sicherstellen, dass der TCI-Server aktiviert ist.",
                        "Thetis fonctionne sur le PC serveur, mais son serveur TCI n'est \
                         pas connecté. Dans Thetis : ouvrez Setup → Network → TCI et \
                         assurez-vous que le serveur TCI est activé.",
                    ),
                ),
                (Some(false), _) => (
                    lang.pick(
                        "Thetis is not running",
                        "Thetis is niet opgestart",
                        "Thetis läuft nicht",
                        "Thetis n'est pas démarré",
                    ),
                    lang.pick(
                        &format!("Thetis is not running on the server PC. {launch_hint} to launch Thetis."),
                        &format!("Thetis draait niet op de server-PC. {launch_hint} om Thetis te starten."),
                        &format!("Thetis läuft nicht auf dem Server-PC. {launch_hint}, um Thetis zu starten."),
                        &format!("Thetis ne fonctionne pas sur le PC serveur. {launch_hint} pour lancer Thetis."),
                    ),
                ),
                (None, Some(d)) => (
                    lang.pick(
                        "Thetis not reachable",
                        "Thetis niet bereikbaar",
                        "Thetis nicht erreichbar",
                        "Thetis injoignable",
                    ),
                    lang.pick(
                        &format!(
                            "Server reports: {d}. {launch_hint} to launch Thetis on the \
                             server PC, or check Thetis directly on the server PC."
                        ),
                        &format!(
                            "Server meldt: {d}. {launch_hint} om Thetis op de server-PC te \
                             starten, of controleer Thetis rechtstreeks op de server-PC."
                        ),
                        &format!(
                            "Server meldet: {d}. {launch_hint}, um Thetis auf dem Server-PC \
                             zu starten, oder prüfen Sie Thetis direkt auf dem Server-PC."
                        ),
                        &format!(
                            "Le serveur signale : {d}. {launch_hint} pour lancer Thetis sur \
                             le PC serveur, ou vérifiez Thetis directement sur le PC serveur."
                        ),
                    ),
                ),
                (None, None) => (
                    lang.pick(
                        "Thetis not reachable",
                        "Thetis niet bereikbaar",
                        "Thetis nicht erreichbar",
                        "Thetis injoignable",
                    ),
                    lang.pick(
                        &format!(
                            "{launch_hint} to launch Thetis on the server PC. If Thetis is \
                             already running, check that its TCI server is enabled."
                        ),
                        &format!(
                            "{launch_hint} om Thetis op de server-PC te starten. Als Thetis \
                             al draait, controleer dan of de TCI-server is ingeschakeld."
                        ),
                        &format!(
                            "{launch_hint}, um Thetis auf dem Server-PC zu starten. Falls \
                             Thetis bereits läuft, prüfen Sie, ob sein TCI-Server aktiviert ist."
                        ),
                        &format!(
                            "{launch_hint} pour lancer Thetis sur le PC serveur. Si Thetis \
                             fonctionne déjà, vérifiez que son serveur TCI est activé."
                        ),
                    ),
                ),
            };
            (headline, Some(action))
        }

        ConnectError::Other { message } => (
            lang.pick(
                "Connection failed",
                "Verbinding mislukt",
                "Verbindung fehlgeschlagen",
                "Échec de la connexion",
            ),
            Some(message.clone()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every error this module can be handed, for the tests that have to cover
    /// the lot. Kept in one place so a new `ConnectError` variant is added here
    /// once rather than forgotten in four tests.
    /// Compile-time guard for the hand-written list below.
    ///
    /// The list cannot enumerate itself, so a new `ConnectError` variant used to
    /// slip in with no coverage and nothing to complain about. This match has no
    /// catch-all arm: add a variant and it stops compiling, which is the only
    /// reminder that actually arrives (review finding, 2026-08-20).
    #[allow(dead_code)]
    fn every_error_is_listed(e: &ConnectError) {
        match e {
            ConnectError::DnsResolutionFailed { .. } => {}
            ConnectError::NoUdpResponse { .. } => {}
            ConnectError::MalformedResponse { .. } => {}
            ConnectError::WrongPassword => {}
            ConnectError::WrongTotp => {}
            ConnectError::ProtocolVersionMismatch { .. } => {}
            ConnectError::TciUnreachable { .. } => {}
            ConnectError::Other { .. } => {}
        }
    }

    /// The same guard for the status enum.
    #[allow(dead_code)]
    fn every_status_is_listed(s: &ConnectStatus) {
        match s {
            ConnectStatus::Disconnected => {}
            ConnectStatus::Connecting => {}
            ConnectStatus::AwaitingTotp => {}
            ConnectStatus::Connected => {}
            ConnectStatus::Failed(_) => {}
        }
    }

    fn every_error() -> Vec<ConnectError> {
        vec![
            ConnectError::DnsResolutionFailed {
                host: "example.local".to_string(),
                io_kind: std::io::ErrorKind::NotFound,
                message: "lookup failed".to_string(),
            },
            ConnectError::NoUdpResponse {
                addr: "192.168.1.79:4580".to_string(),
                timeout_secs: 5,
            },
            ConnectError::MalformedResponse {
                addr: "192.168.1.79:4580".to_string(),
                detail: "unknown packet type 0x42".to_string(),
            },
            ConnectError::WrongPassword,
            ConnectError::WrongTotp,
            ConnectError::ProtocolVersionMismatch {
                server_version: 2,
                client_version: 1,
            },
            ConnectError::ProtocolVersionMismatch {
                server_version: 2,
                client_version: 3,
            },
            ConnectError::TciUnreachable {
                server_addr: "192.168.1.79:4580".to_string(),
                server_reported_detail: None,
                thetis_process_running: None,
            },
            ConnectError::Other {
                message: "io error".to_string(),
            },
        ]
    }

    #[test]
    fn all_status_variants_have_text_in_every_language() {
        // Five variants, not four: `Failed` was missing, and it is the one
        // that carries the text an operator reads when something goes wrong.
        let mut cases = vec![
            ConnectStatus::Disconnected,
            ConnectStatus::Connecting,
            ConnectStatus::AwaitingTotp,
            ConnectStatus::Connected,
        ];
        cases.extend(every_error().into_iter().map(ConnectStatus::Failed));
        for lang in Lang::ALL {
            for platform in [Platform::Desktop, Platform::Mobile] {
                for s in &cases {
                    let (h, _) = connect_status_text(s, lang, platform);
                    assert!(!h.is_empty(), "missing {:?} text for {:?}", lang, s);
                }
            }
        }
    }

    #[test]
    fn all_error_variants_have_text_in_every_language() {
        for lang in Lang::ALL {
            for platform in [Platform::Desktop, Platform::Mobile] {
                for err in &every_error() {
                    let (h, a) = connect_error_text(err, lang, platform);
                    assert!(
                        !h.is_empty(),
                        "missing {:?}/{:?} headline for {:?}",
                        lang,
                        platform,
                        err
                    );
                    assert!(
                        a.map(|a| !a.is_empty()).unwrap_or(true),
                        "empty {:?}/{:?} action for {:?}",
                        lang,
                        platform,
                        err
                    );
                }
            }
        }
    }

    /// The failure this whole module exists to prevent: a message that quietly
    /// hands English to a reader whose app is in another language. Every
    /// headline must actually differ from the English one.
    ///
    /// If a future message is genuinely identical in two languages, that is the
    /// moment to think about it rather than to loosen this - a proper noun on
    /// its own would be the only honest reason.
    #[test]
    fn no_language_silently_falls_back_to_english() {
        // Both platforms. The English leak this test exists for was on MOBILE
        // (the connect status line stayed English on a Dutch phone), and mobile
        // was the platform it did not run.
        for platform in [Platform::Desktop, Platform::Mobile] {
            for err in &every_error() {
                let (en, _) = connect_error_text(err, Lang::En, platform);
                let _ = &en;
                let (en_head, en_action) = connect_error_text(err, Lang::En, platform);
                for lang in [Lang::Nl, Lang::De, Lang::Fr] {
                    let (other, other_action) = connect_error_text(err, lang, platform);
                    assert_ne!(
                        en_head, other,
                        "{:?} is still the English headline for {:?} on {:?}",
                        lang, err, platform
                    );
                    // The action line too. It is the string that differs per
                    // platform, so an English one left standing on mobile is
                    // exactly the leak this test exists for - and comparing only
                    // the headline could not see it.
                    // `Other` passes the underlying error message through
                    // verbatim; that is not a translatable string and being the
                    // same in every language is correct there.
                    let passthrough = matches!(err, ConnectError::Other { .. });
                    if let (false, Some(a), Some(b)) =
                        (passthrough, en_action.as_ref(), other_action.as_ref())
                    {
                        assert_ne!(
                            a, b,
                            "{:?} still has the English action for {:?} on {:?}",
                            lang, err, platform
                        );
                    }
                }
            }
        }
    }

    /// A stored code that means nothing must land on English, not panic and not
    /// on whichever language happens to be first.
    #[test]
    fn an_unknown_language_code_is_english() {
        assert_eq!(Lang::from_code("nl"), Lang::Nl);
        assert_eq!(Lang::from_code("DE"), Lang::De);
        assert_eq!(Lang::from_code(" fr "), Lang::Fr);
        assert_eq!(Lang::from_code("es"), Lang::En);
        assert_eq!(Lang::from_code(""), Lang::En);
        for lang in Lang::ALL {
            assert_eq!(Lang::from_code(lang.code()), lang, "code round-trip");
        }
    }

    #[test]
    fn tci_unreachable_platform_differentiation() {
        // Mobile must NOT point at the desktop's "Thetis tab" because Android
        // keeps everything on the Radio screen. Checked in every language: the
        // hint is the one string here that is built per platform, so a
        // translation that drops the distinction is the likely mistake.
        let err = ConnectError::TciUnreachable {
            server_addr: "192.168.1.79:4580".to_string(),
            server_reported_detail: None,
            thetis_process_running: Some(false),
        };
        for lang in Lang::ALL {
            let (_, desktop) = connect_error_text(&err, lang, Platform::Desktop);
            let (_, mobile) = connect_error_text(&err, lang, Platform::Mobile);
            let desktop = desktop.unwrap();
            let mobile = mobile.unwrap();
            assert!(
                desktop.contains("Start"),
                "{:?} desktop hint must name the Start button: {:?}",
                lang,
                desktop
            );
            assert!(
                mobile.contains("Power"),
                "{:?} mobile hint must name the Power button: {:?}",
                lang,
                mobile
            );
            assert_ne!(desktop, mobile, "{:?} says the same on both platforms", lang);
        }
        // The two wordings that were asserted before the other languages
        // existed, kept verbatim so this stays the same test it was.
        let (_, action_desktop_en) = connect_error_text(&err, Lang::En, Platform::Desktop);
        let (_, action_desktop_nl) = connect_error_text(&err, Lang::Nl, Platform::Desktop);
        let (_, action_mobile_en) = connect_error_text(&err, Lang::En, Platform::Mobile);
        let (_, action_mobile_nl) = connect_error_text(&err, Lang::Nl, Platform::Mobile);
        assert!(action_desktop_en.as_ref().unwrap().contains("Thetis tab"));
        assert!(action_desktop_nl.as_ref().unwrap().contains("Thetis-tab"));
        assert!(!action_mobile_en.as_ref().unwrap().contains("Thetis tab"));
        assert!(!action_mobile_nl.as_ref().unwrap().contains("Thetis-tab"));
    }

    #[test]
    fn tci_unreachable_headline_names_thetis_not_the_radio() {
        // A station with one or two Yaesu radios attached must not read this
        // as "my Yaesu is down" - the headline says Thetis, and it says which
        // of the two failure modes applies.
        let mk = |running: Option<bool>| ConnectError::TciUnreachable {
            server_addr: "192.168.1.79:4580".to_string(),
            server_reported_detail: None,
            thetis_process_running: running,
        };
        for lang in Lang::ALL {
            for running in [None, Some(true), Some(false)] {
                let (h, _) = connect_error_text(&mk(running), lang, Platform::Desktop);
                assert!(h.contains("Thetis"), "headline must name Thetis: {:?}", h);
                assert!(
                    !h.to_lowercase().contains("radio"),
                    "headline must not blame the radio: {:?}",
                    h
                );
            }
            // Not-running vs TCI-down are distinct headlines.
            let (h_off, _) = connect_error_text(&mk(Some(false)), lang, Platform::Desktop);
            let (h_tci, _) = connect_error_text(&mk(Some(true)), lang, Platform::Desktop);
            assert_ne!(h_off, h_tci);
        }
        let (h_nl, _) = connect_error_text(&mk(Some(false)), Lang::Nl, Platform::Desktop);
        assert_eq!(h_nl, "Thetis is niet opgestart");
    }

    #[test]
    fn version_mismatch_distinguishes_too_old_vs_too_new() {
        let server_newer = ConnectError::ProtocolVersionMismatch {
            server_version: 3,
            client_version: 2,
        };
        let client_newer = ConnectError::ProtocolVersionMismatch {
            server_version: 1,
            client_version: 2,
        };
        for lang in Lang::ALL {
            let (h1, _) = connect_error_text(&server_newer, lang, Platform::Desktop);
            let (h2, _) = connect_error_text(&client_newer, lang, Platform::Desktop);
            assert_ne!(
                h1, h2,
                "{:?}: too-old vs too-new must have distinct text",
                lang
            );
        }
        let (h1, _) = connect_error_text(&server_newer, Lang::En, Platform::Desktop);
        let (h2, _) = connect_error_text(&client_newer, Lang::En, Platform::Desktop);
        assert!(h1.contains("Client"));
        assert!(h2.contains("Server"));
    }
}
