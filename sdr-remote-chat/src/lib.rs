// SPDX-License-Identifier: GPL-2.0-or-later
//
//! The ThetisLink chat and problem reporting, as one component.
//!
//! # Why this is a crate and not a screen in each app
//!
//! The server is a station in its own right. It runs beside Thetis, drives the
//! radio, the amplifier, the rotor and the tuner, and plenty of the time nobody
//! has a client open at all. Treating it as an accessory that only needs a
//! report button was wrong: whoever is sitting at the server has exactly the
//! same reasons to reach other users, and exactly the same problems to report.
//!
//! So the window is written once and used by both, and later by Android through
//! the bridge. Not to save typing - to stop the three from drifting. Two copies
//! of a consent text is one copy nobody updates, and a consent text that says
//! something different in two places is worse than no consent text.
//!
//! The translations live in this crate as well, for the same reason. `set_locale`
//! is process-wide, so whichever host sets it steers these strings too.
//!
//! # What the host still owns
//!
//! Everything the chat cannot know by itself, handed in every frame:
//!
//! - the relay address and the ticket that came back on the relay's ready-reply
//! - where its own log and settings live, since client and server keep theirs in
//!   different places and under different names
//! - the window: each app wraps [`ChatPanel::render_body`] in its own viewport,
//!   which is also why this draws with panels rather than working out heights.
//!
//! Nothing in here blocks. The worker does the network on its own thread and
//! this only drains what it sends back, so a chat service that is down or slow
//! cannot make either GUI stutter - which for the server means it cannot get
//! between an operator and their PTT.

rust_i18n::i18n!("locales", fallback = "en");

mod model;
#[cfg(feature = "ui")]
mod panel;
mod worker;

/// The state and the rules, without a UI. Android holds this directly and draws
/// it with Compose; the egui window below holds the same thing. Available
/// without the `ui` feature, which is how a phone gets the chat without
/// building egui for it.
pub use model::{ChatModel, CONSENT_TEXT_VERSION, EDIT_WINDOW_SHOWN_SECS};
#[cfg(feature = "ui")]
pub use panel::{ChatFiles, ChatPanel, ServerSide};
pub use worker::{
    endpoint_for_relay, ChatAnswer, ChatCommand, ChatEvent,
    ChatMessage,
    OfflineReason, POLL_INTERVAL,
};

#[cfg(test)]
mod consent_text_tests {
    /// The version number claims something about the text, and nothing checked
    /// that the claim held. Version 3 is the one that added the clause about
    /// being removed from the chat - and the next time the text changes without
    /// the constant, or the constant without the text, it would be silent
    /// again (raised in review, 2026-08-18).
    ///
    /// This is not a full guard: it cannot know what a future version means. It
    /// binds the one thing that is already promised - that agreement recorded
    /// as v3 was given to a text mentioning the ban - and it fails loudly when
    /// somebody moves one without the other.
    #[test]
    fn the_consent_version_matches_a_text_that_says_what_it_should() {
        let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let yml = std::fs::read_to_string(here.join("locales/app.yml"))
            .expect("the desktop consent text");

        assert_eq!(
            crate::CONSENT_TEXT_VERSION, 3,
            "the consent version changed - decide what the new one promises and              say it here, rather than leaving this test asserting the old one"
        );
        assert!(
            yml.contains("chat_consent_ban"),
            "version 3 is the text that explains being removed from the chat, and              that clause is gone from the text"
        );
    }

    /// A gate on this repository's layout, not a library test: it reaches across
    /// crates by relative path and breaks if a directory moves - loudly, which
    /// is the point. Recorded as deliberate at the review's request.
    ///
    /// The consent text exists twice: here for the desktop, and in the Android
    /// resources. The model is shared and the words are not, and the header of
    /// `locales/app.yml` says what that costs - "two copies of a consent text is
    /// one copy nobody updates".
    ///
    /// It had already drifted when this was written: the desktop warned that a
    /// callsign appears in a public register with name and address, and the
    /// phone asked for a callsign and said nothing. Nobody had forgotten
    /// anything; there was simply nothing that would notice.
    ///
    /// So this notices. A key that carries meaning for the person deciding what
    /// to reveal must exist on both sides. Validation prompts are excluded by
    /// name: Android says those through its own button state rather than as
    /// text, which is a real difference and not a silence.
    #[test]
    fn the_consent_text_says_the_same_things_on_both_platforms() {
        const SAID_THROUGH_THE_UI_INSTEAD: &[&str] =
            &["chat_consent_need_age", "chat_consent_need_name"];

        let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let yml = std::fs::read_to_string(here.join("locales/app.yml"))
            .expect("the desktop consent text");
        let xml_path = here
            .join("../sdr-remote-android/android/app/src/main/res/values/strings.xml");
        let xml = match std::fs::read_to_string(&xml_path) {
            Ok(t) => t,
            // Said out loud rather than skipped: a guard that quietly does
            // nothing is the thing it is guarding against.
            Err(e) => panic!("cannot check the Android consent text at {xml_path:?}: {e}"),
        };

        let desktop: Vec<&str> = yml
            .lines()
            .filter_map(|l| l.strip_suffix(':'))
            .filter(|k| k.starts_with("chat_consent_"))
            .filter(|k| !SAID_THROUGH_THE_UI_INSTEAD.contains(k))
            .collect();
        assert!(!desktop.is_empty(), "no consent keys found - has the file moved?");

        // On SCREEN, not merely in the resources. The first version of this
        // guard checked strings.xml alone and went green in the very commit
        // that added it: one of the two strings had been added and never
        // rendered, so the phone still did not say that refusing costs nothing
        // while the test pointed at yes. Present is not connected - the same
        // fault it exists to catch, one level down (found in review,
        // 2026-08-18).
        let screen_path = here
            .join("../sdr-remote-android/android/app/src/main/java/com/sdrremote/ui/screens/ChatScreen.kt");
        let screen = match std::fs::read_to_string(&screen_path) {
            Ok(t) => t,
            Err(e) => panic!("cannot check the Android consent screen at {screen_path:?}: {e}"),
        };

        let missing: Vec<String> = desktop
            .iter()
            .filter_map(|k| {
                if !xml.contains(&format!("name=\"{k}\"")) {
                    Some(format!("{k} (no string)"))
                } else if !screen.contains(&format!("R.string.{k}")) {
                    Some(format!("{k} (string exists but nothing shows it)"))
                } else {
                    None
                }
            })
            .collect();
        assert!(
            missing.is_empty(),
            "the phone's consent screen is missing what the desktop says: {missing:?}"
        );
    }
}
