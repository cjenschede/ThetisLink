// SPDX-License-Identifier: GPL-2.0-or-later
//! The words for an RX-only antenna position that Thetis cannot guard by
//! itself (builds 215-217; since 221 only a Thetis older than v2.10.3.14
//! without the fork's extensions), in one place for every screen that shows them: the
//! clients (through `sdr_remote_logic::station_notice`) and the Amplitec window
//! of the server, which does not depend on that crate.

use crate::oslang::UI_LANGUAGES;

/// One line per language, in the order of [`UI_LANGUAGES`]: what is wrong, and
/// what helps. The array length is that list's length, so a fifth language is a
/// compile error here.
pub const TEXT: [&str; UI_LANGUAGES.len()] = [
    "This antenna position is receive-only, and this Thetis (older than v2.10.3.14, without the ThetisLink extensions) cannot refuse transmitting itself - do not use the footswitch or MOX in Thetis",
    "Deze antennestand is alleen voor ontvangst, en deze Thetis (ouder dan v2.10.3.14, zonder de ThetisLink-uitbreidingen) kan zenden niet zelf weigeren - gebruik in Thetis geen voetschakelaar of MOX",
    "Diese Antennenstellung ist nur f\u{fc}r Empfang, und dieses Thetis (\u{e4}lter als v2.10.3.14, ohne die ThetisLink-Erweiterungen) kann Senden nicht selbst verweigern - in Thetis keinen Fu\u{df}schalter und kein MOX benutzen",
    "Cette position d'antenne est en r\u{e9}ception seule, et ce Thetis (ant\u{e9}rieur \u{e0} v2.10.3.14, sans les extensions ThetisLink) ne peut pas refuser l'\u{e9}mission lui-m\u{ea}me - n'utilisez ni p\u{e9}dale ni MOX dans Thetis",
];

/// The line for a language code ("en", "nl", "de", "fr"); anything else is
/// English, like the rest of the UI.
pub fn text(lang_code: &str) -> &'static str {
    let code = lang_code.trim().to_ascii_lowercase();
    UI_LANGUAGES
        .iter()
        .position(|l| *l == code)
        .map(|i| TEXT[i])
        .unwrap_or(TEXT[0])
}

#[cfg(test)]
mod tests {
    use super::{text, TEXT};

    #[test]
    fn every_language_has_its_own_line_and_unknown_is_english() {
        assert!(text("en").starts_with("This antenna position"));
        assert!(text("nl").starts_with("Deze antennestand"));
        assert!(text("de").starts_with("Diese Antennenstellung"));
        assert!(text("fr").starts_with("Cette position"));
        assert_eq!(text(" NL "), TEXT[1]);
        assert_eq!(text("xx"), TEXT[0]);
        assert_eq!(text(""), TEXT[0]);
    }
}
