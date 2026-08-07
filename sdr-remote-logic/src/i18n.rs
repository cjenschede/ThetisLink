// SPDX-License-Identifier: GPL-2.0-or-later
//
// Centralized text-strings for connect-status / connect-error display.
//
// Single source of truth for both desktop (egui) and Android (Compose via
// UniFFI bridge). Adding a new language means adding a new arm to
// `connect_status_text()` / `connect_error_text()` — UI code never has
// hard-coded user-visible strings.

use crate::state::{ConnectError, ConnectStatus};

/// Display language for connect-status / connect-error strings.
///
/// Defaults to English; client can pass the user-configured language
/// (e.g. from `thetislink-client.conf`) or fall back to OS-locale detect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Nl,
}

impl Default for Lang {
    fn default() -> Self {
        Lang::En
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
/// and firewall on the server PC") — `None` when nothing useful can
/// be advised (e.g. `Connecting`).
pub fn connect_status_text(
    status: &ConnectStatus,
    lang: Lang,
    platform: Platform,
) -> (String, Option<String>) {
    match status {
        ConnectStatus::Disconnected => match lang {
            Lang::En => ("Disconnected".to_string(), None),
            Lang::Nl => ("Niet verbonden".to_string(), None),
        },
        ConnectStatus::Connecting => match lang {
            Lang::En => ("Connecting…".to_string(), None),
            Lang::Nl => ("Bezig met verbinden…".to_string(), None),
        },
        ConnectStatus::AwaitingTotp => match lang {
            Lang::En => (
                "Enter 2FA code".to_string(),
                Some("Open your authenticator app and enter the 6-digit code.".to_string()),
            ),
            Lang::Nl => (
                "Voer 2FA-code in".to_string(),
                Some("Open je authenticator-app en voer de 6-cijferige code in.".to_string()),
            ),
        },
        ConnectStatus::Connected => match lang {
            Lang::En => ("Connected".to_string(), None),
            Lang::Nl => ("Verbonden".to_string(), None),
        },
        ConnectStatus::Failed(err) => connect_error_text(err, lang, platform),
    }
}

/// User-visible text for a `ConnectError`. Returns `(headline, suggested_action)`.
pub fn connect_error_text(
    err: &ConnectError,
    lang: Lang,
    platform: Platform,
) -> (String, Option<String>) {
    match (err, lang) {
        // --- DnsResolutionFailed ---
        (ConnectError::DnsResolutionFailed { host, .. }, Lang::En) => (
            format!("Server name not found: {}", host),
            Some("Check the spelling of the server address.".to_string()),
        ),
        (ConnectError::DnsResolutionFailed { host, .. }, Lang::Nl) => (
            format!("Servernaam niet gevonden: {}", host),
            Some("Controleer of het serveradres correct is gespeld.".to_string()),
        ),

        // --- NoUdpResponse ---
        (ConnectError::NoUdpResponse { addr, .. }, Lang::En) => (
            format!("Server not reachable at {}", addr),
            Some(
                "Check that the server is running, the IP/port is correct, \
                 and the firewall allows UDP traffic on the server PC."
                    .to_string(),
            ),
        ),
        (ConnectError::NoUdpResponse { addr, .. }, Lang::Nl) => (
            format!("Server niet bereikbaar op {}", addr),
            Some(
                "Controleer of de server draait, IP/poort kloppen, \
                 en de firewall UDP-verkeer toestaat op de server-PC."
                    .to_string(),
            ),
        ),

        // --- MalformedResponse ---
        (ConnectError::MalformedResponse { addr, .. }, Lang::En) => (
            format!("Unexpected response from {}", addr),
            Some(
                "The address responds but does not speak the ThetisLink protocol. \
                 Check that the port number is correct."
                    .to_string(),
            ),
        ),
        (ConnectError::MalformedResponse { addr, .. }, Lang::Nl) => (
            format!("Onverwacht antwoord van {}", addr),
            Some(
                "Het adres reageert maar gebruikt geen ThetisLink-protocol. \
                 Controleer of het poortnummer klopt."
                    .to_string(),
            ),
        ),

        // --- WrongPassword ---
        (ConnectError::WrongPassword, Lang::En) => (
            "Wrong password".to_string(),
            Some("Check the password configured on the server.".to_string()),
        ),
        (ConnectError::WrongPassword, Lang::Nl) => (
            "Verkeerd wachtwoord".to_string(),
            Some("Controleer het wachtwoord dat op de server is ingesteld.".to_string()),
        ),

        // --- WrongTotp ---
        (ConnectError::WrongTotp, Lang::En) => (
            "Wrong 2FA code".to_string(),
            Some("Check the 6-digit code in your authenticator app and try again.".to_string()),
        ),
        (ConnectError::WrongTotp, Lang::Nl) => (
            "Verkeerde 2FA-code".to_string(),
            Some("Controleer de 6-cijferige code in je authenticator-app en probeer opnieuw.".to_string()),
        ),

        // --- ProtocolVersionMismatch (client too old) ---
        (
            ConnectError::ProtocolVersionMismatch {
                server_version,
                client_version,
            },
            Lang::En,
        ) if server_version > client_version => (
            "Client is too old".to_string(),
            Some(format!(
                "The server uses protocol version {}, but this client uses version {}. \
                 Please update the client.",
                server_version, client_version
            )),
        ),
        (
            ConnectError::ProtocolVersionMismatch {
                server_version,
                client_version,
            },
            Lang::Nl,
        ) if server_version > client_version => (
            "Client is te oud".to_string(),
            Some(format!(
                "De server gebruikt protocolversie {}, maar deze client gebruikt versie {}. \
                 Update de client.",
                server_version, client_version
            )),
        ),

        // --- ProtocolVersionMismatch (server too old) ---
        (
            ConnectError::ProtocolVersionMismatch {
                server_version,
                client_version,
            },
            Lang::En,
        ) => (
            "Server is too old".to_string(),
            Some(format!(
                "This client uses protocol version {}, but the server uses version {}. \
                 Please update the server.",
                client_version, server_version
            )),
        ),
        (
            ConnectError::ProtocolVersionMismatch {
                server_version,
                client_version,
            },
            Lang::Nl,
        ) => (
            "Server is te oud".to_string(),
            Some(format!(
                "Deze client gebruikt protocolversie {}, maar de server gebruikt versie {}. \
                 Update de server.",
                client_version, server_version
            )),
        ),

        // --- TciUnreachable ---
        // Branch headline + hint on what the server reports about Thetis.exe:
        //   Some(true)  → Thetis runs, TCI is down → check TCI settings
        //   Some(false) → Thetis is not running   → use the client's
        //                 Power control to launch it; phrasing depends
        //                 on the platform (desktop has a Thetis tab,
        //                 Android keeps everything on the Radio screen).
        //   None        → old server, no hint     → generic fallback
        (
            ConnectError::TciUnreachable {
                server_reported_detail,
                thetis_process_running,
                ..
            },
            Lang::En,
        ) => {
            // Per-platform pointer to the Thetis-launch control. Desktop:
            // dedicated "Thetis" tab with a Start button. Android: the
            // Power button on the Radio screen (no extra tab).
            let launch_hint = match platform {
                Platform::Desktop => "Open the Thetis tab in this client and press Start",
                Platform::Mobile => "Tap the Power button on the Radio screen",
            };
            // Headline names Thetis explicitly, never "radio": a station with
            // one or two Yaesu radios attached reads "radio not reachable" as
            // "my Yaesu is down", while this error is only ever about Thetis.
            let (headline, action) = match (thetis_process_running, server_reported_detail) {
                (Some(true), _) => (
                    "Thetis TCI not connected",
                    "Thetis is running on the server PC, but its TCI server is not connected. \
                     In Thetis: open Setup → Network → TCI and make sure the TCI server is enabled."
                        .to_string(),
                ),
                (Some(false), _) => (
                    "Thetis is not running",
                    format!(
                        "Thetis is not running on the server PC. {} to launch Thetis.",
                        launch_hint
                    ),
                ),
                (None, Some(d)) => (
                    "Thetis not reachable",
                    format!(
                        "Server reports: {}. {} to launch Thetis on the server PC, \
                         or check Thetis directly on the server PC.",
                        d, launch_hint
                    ),
                ),
                (None, None) => (
                    "Thetis not reachable",
                    format!(
                        "{} to launch Thetis on the server PC. \
                         If Thetis is already running, check that its TCI server is enabled.",
                        launch_hint
                    ),
                ),
            };
            (headline.to_string(), Some(action))
        }
        (
            ConnectError::TciUnreachable {
                server_reported_detail,
                thetis_process_running,
                ..
            },
            Lang::Nl,
        ) => {
            let launch_hint = match platform {
                Platform::Desktop => "Open de Thetis-tab in deze client en druk op Start",
                Platform::Mobile => "Tik op de Power-knop in het Radio-scherm",
            };
            let (headline, action) = match (thetis_process_running, server_reported_detail) {
                (Some(true), _) => (
                    "Thetis TCI niet verbonden",
                    "Thetis draait op de server-PC, maar de TCI-server is niet verbonden. \
                     In Thetis: open Setup → Network → TCI en zorg dat de TCI-server aan staat."
                        .to_string(),
                ),
                (Some(false), _) => (
                    "Thetis is niet opgestart",
                    format!(
                        "Thetis draait niet op de server-PC. {} om Thetis te starten.",
                        launch_hint
                    ),
                ),
                (None, Some(d)) => (
                    "Thetis niet bereikbaar",
                    format!(
                        "Server meldt: {}. {} om Thetis op de server-PC te starten, \
                         of controleer Thetis rechtstreeks op de server-PC.",
                        d, launch_hint
                    ),
                ),
                (None, None) => (
                    "Thetis niet bereikbaar",
                    format!(
                        "{} om Thetis op de server-PC te starten. \
                         Als Thetis al draait, controleer dan of de TCI-server is ingeschakeld.",
                        launch_hint
                    ),
                ),
            };
            (headline.to_string(), Some(action))
        }

        // --- Other ---
        (ConnectError::Other { message }, Lang::En) => (
            "Connection failed".to_string(),
            Some(message.clone()),
        ),
        (ConnectError::Other { message }, Lang::Nl) => (
            "Verbinding mislukt".to_string(),
            Some(message.clone()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_status_variants_have_text_en() {
        let cases = [
            ConnectStatus::Disconnected,
            ConnectStatus::Connecting,
            ConnectStatus::AwaitingTotp,
            ConnectStatus::Connected,
        ];
        for s in &cases {
            let (h, _) = connect_status_text(s, Lang::En, Platform::Desktop);
            assert!(!h.is_empty(), "missing EN text for {:?}", s);
        }
    }

    #[test]
    fn all_status_variants_have_text_nl() {
        let cases = [
            ConnectStatus::Disconnected,
            ConnectStatus::Connecting,
            ConnectStatus::AwaitingTotp,
            ConnectStatus::Connected,
        ];
        for s in &cases {
            let (h, _) = connect_status_text(s, Lang::Nl, Platform::Desktop);
            assert!(!h.is_empty(), "missing NL text for {:?}", s);
        }
    }

    #[test]
    fn all_error_variants_have_text() {
        let errors = [
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
        ];
        for err in &errors {
            let (h_en_d, _) = connect_error_text(err, Lang::En, Platform::Desktop);
            let (h_nl_d, _) = connect_error_text(err, Lang::Nl, Platform::Desktop);
            let (h_en_m, _) = connect_error_text(err, Lang::En, Platform::Mobile);
            let (h_nl_m, _) = connect_error_text(err, Lang::Nl, Platform::Mobile);
            assert!(!h_en_d.is_empty(), "missing EN/Desktop text for {:?}", err);
            assert!(!h_nl_d.is_empty(), "missing NL/Desktop text for {:?}", err);
            assert!(!h_en_m.is_empty(), "missing EN/Mobile text for {:?}", err);
            assert!(!h_nl_m.is_empty(), "missing NL/Mobile text for {:?}", err);
        }
    }

    #[test]
    fn tci_unreachable_platform_differentiation() {
        // Mobile should NOT point at the desktop's "Thetis tab" because
        // Android keeps everything on the Radio screen.
        let err = ConnectError::TciUnreachable {
            server_addr: "192.168.1.79:4580".to_string(),
            server_reported_detail: None,
            thetis_process_running: Some(false),
        };
        let (_, action_desktop_en) = connect_error_text(&err, Lang::En, Platform::Desktop);
        let (_, action_mobile_en) = connect_error_text(&err, Lang::En, Platform::Mobile);
        let (_, action_desktop_nl) = connect_error_text(&err, Lang::Nl, Platform::Desktop);
        let (_, action_mobile_nl) = connect_error_text(&err, Lang::Nl, Platform::Mobile);

        assert!(action_desktop_en.as_ref().unwrap().contains("Thetis tab"));
        assert!(action_desktop_nl.as_ref().unwrap().contains("Thetis-tab"));
        assert!(!action_mobile_en.as_ref().unwrap().contains("Thetis tab"));
        assert!(!action_mobile_nl.as_ref().unwrap().contains("Thetis-tab"));
        assert!(action_mobile_en.as_ref().unwrap().contains("Power"));
        assert!(action_mobile_nl.as_ref().unwrap().contains("Power"));
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
        for lang in [Lang::En, Lang::Nl] {
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
        let (h1, _) = connect_error_text(&server_newer, Lang::En, Platform::Desktop);
        let (h2, _) = connect_error_text(&client_newer, Lang::En, Platform::Desktop);
        assert_ne!(h1, h2, "too-old vs too-new must have distinct text");
        assert!(h1.contains("Client"));
        assert!(h2.contains("Server"));
    }
}
