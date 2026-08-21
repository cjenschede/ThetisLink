// SPDX-License-Identifier: GPL-2.0-or-later
//! Which language ThetisLink opens in on a machine that has never run it.
//!
//! This is only ever consulted on a *first* start. The moment a config file
//! exists, the language in it is the operator's own choice and wins - a
//! reinstall of Windows in another language must not silently retranslate a
//! station that has been running in Dutch for a year.

/// The languages ThetisLink ships translations for. English is the base and
/// the fallback: anything the OS reports that is not in this list lands there.
pub const UI_LANGUAGES: [&str; 4] = ["en", "nl", "de", "fr"];

/// Narrow an OS language tag to one of [`UI_LANGUAGES`].
///
/// Accepts the shapes the platforms actually hand out - `nl`, `nl-NL`,
/// `nl_NL.UTF-8` - and looks only at the part in front of the region, because
/// ThetisLink translates per language and not per region.
pub fn narrow_to_supported(tag: &str) -> &'static str {
    let primary = tag
        .split(['-', '_', '.'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    UI_LANGUAGES
        .iter()
        .find(|lang| **lang == primary)
        .copied()
        .unwrap_or("en")
}

/// The language the operator reads the rest of their OS in, narrowed to one
/// ThetisLink has. English when the OS speaks something we do not translate.
#[cfg(windows)]
pub fn detect_ui_language() -> &'static str {
    // The *display* language, not the regional format. Someone running an
    // English Windows with a Dutch region reads every other menu on that
    // machine in English and should get ThetisLink in English too;
    // GetUserDefaultLocaleName would answer "nl-NL" there and be wrong.
    let langid = unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() };
    lang_from_langid(langid)
}

/// The translation a Windows LANGID asks for.
///
/// Split from the call that fetches it so the table can be tested: the Windows
/// path does NOT go through [`narrow_to_supported`], so testing that function
/// proved nothing about the machines this actually runs on, and a test that
/// only asks whether the answer is one of the four would still pass if this
/// stopped working and always said English (review finding, 2026-08-20).
#[cfg(windows)]
pub fn lang_from_langid(langid: u16) -> &'static str {
    // The low 10 bits of a LANGID are the primary language. The rest is the
    // sub-language (nl-NL vs nl-BE, de-DE vs de-AT), which picks no different
    // translation here.
    match langid & 0x03FF {
        0x13 => "nl", // LANG_DUTCH
        0x07 => "de", // LANG_GERMAN
        0x0C => "fr", // LANG_FRENCH
        _ => "en",    // includes 0x09 LANG_ENGLISH
    }
}

/// Non-Windows fallback: the POSIX locale environment, in the order a shell
/// resolves it. `C`/`POSIX` mean "no locale chosen" rather than English, but
/// they land on English all the same - that is the base language.
#[cfg(not(windows))]
pub fn detect_ui_language() -> &'static str {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(value) = std::env::var(key) {
            if !value.is_empty() && value != "C" && value != "POSIX" {
                return narrow_to_supported(&value);
            }
        }
    }
    "en"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three shapes a platform hands out for the same language.
    #[test]
    fn a_region_does_not_pick_a_different_translation() {
        assert_eq!(narrow_to_supported("nl"), "nl");
        assert_eq!(narrow_to_supported("nl-NL"), "nl");
        assert_eq!(narrow_to_supported("nl_BE.UTF-8"), "nl");
        assert_eq!(narrow_to_supported("de-AT"), "de");
        assert_eq!(narrow_to_supported("fr-CA"), "fr");
        assert_eq!(narrow_to_supported("EN-GB"), "en");
    }

    /// A language we do not have is not an error and not a blank UI - it is
    /// English, which every screen in ThetisLink has.
    #[test]
    fn an_untranslated_language_lands_on_english() {
        assert_eq!(narrow_to_supported("es-ES"), "en");
        assert_eq!(narrow_to_supported("ja"), "en");
        assert_eq!(narrow_to_supported(""), "en");
        assert_eq!(narrow_to_supported("-"), "en");
    }


    /// The Windows table: the path every Windows station takes, and the one
    /// no test touched. Sub-language bits are deliberately included:
    /// Dutch-Belgian and Austrian German must land on the same translation as
    /// their neighbours, and that is the half a primary-language-only table
    /// gets wrong.
    #[cfg(windows)]
    #[test]
    fn the_windows_language_table_answers_per_language_not_per_region() {
        use super::lang_from_langid;
        assert_eq!(lang_from_langid(0x0413), "nl"); // nl-NL
        assert_eq!(lang_from_langid(0x0813), "nl"); // nl-BE
        assert_eq!(lang_from_langid(0x0407), "de"); // de-DE
        assert_eq!(lang_from_langid(0x0C07), "de"); // de-AT
        assert_eq!(lang_from_langid(0x040C), "fr"); // fr-FR
        assert_eq!(lang_from_langid(0x0C0C), "fr"); // fr-CA
        assert_eq!(lang_from_langid(0x0409), "en"); // en-US
        assert_eq!(lang_from_langid(0x0809), "en"); // en-GB
        // Something we do not translate is English, not a blank screen.
        assert_eq!(lang_from_langid(0x040A), "en"); // es-ES
        assert_eq!(lang_from_langid(0x0411), "en"); // ja-JP
        // And every answer is one the app can actually set.
        for id in [0x0413u16, 0x0407, 0x040C, 0x0409, 0x0000, 0xFFFF] {
            assert!(
                UI_LANGUAGES.contains(&lang_from_langid(id)),
                "LANGID {id:#06x} answered something we do not ship"
            );
        }
    }

    /// Whatever the OS says, the answer is one of the four the app can set.
    #[test]
    fn the_detected_language_is_one_we_ship() {
        assert!(UI_LANGUAGES.contains(&detect_ui_language()));
    }
}
