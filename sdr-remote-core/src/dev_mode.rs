// SPDX-License-Identifier: GPL-2.0-or-later
//! The developer mode (owner decision 2026-10-23, on the way to a release):
//! one program for everyone, and the measurement tools and the extra log lines
//! only after a code typed in the settings. The one place that knows what the
//! mode is: the code, the log target of a developer line, and the rule every
//! logger applies. Each program (server, desktop, app) keeps its own switch and
//! stores it with its own settings - three processes, so three stores, on
//! purpose; what they share is this module.

use std::sync::atomic::{AtomicBool, Ordering};

/// The log target of a developer line: a figure that comes back in ordinary
/// use and that a user can do nothing with - the lines every 10 s, the lines
/// per over or press, the start lines of the transmit buffer. Written with
/// [`dev_info!`](crate::dev_info); a line that only appears when something
/// goes wrong stays an ordinary line.
pub const LOG_TARGET: &str = "thetislink::dev";

/// The code that switches the mode on. It stands in the public source: this
/// hides the tools from an ordinary user, it does not protect anything (owner,
/// 2026-10-23).
pub const CODE: &str = "PA3GHM73";

static ON: AtomicBool = AtomicBool::new(false);

/// Is the developer mode on in this process?
pub fn is_on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Switch it, for this process. The caller stores it with its settings.
pub fn set(on: bool) {
    ON.store(on, Ordering::Relaxed);
}

/// Does what was typed open the developer mode? Spaces around it and the case
/// of the letters do not matter.
pub fn code_matches(typed: &str) -> bool {
    typed.trim().eq_ignore_ascii_case(CODE)
}

/// May a log record with this target be written? Every record, except a
/// developer line outside the developer mode. The one rule for every logger:
/// the server's, the desktop's and the app's call this, before anything is
/// written anywhere.
pub fn record_passes(target: &str) -> bool {
    record_passes_in(target, is_on())
}

fn record_passes_in(target: &str, on: bool) -> bool {
    on || target != LOG_TARGET
}

/// The words a log says when the mode changes, and at the start when it is on:
/// an ordinary line, so every log says in which mode it was written.
pub fn state_line(on: bool) -> &'static str {
    if on {
        "Developer mode: on"
    } else {
        "Developer mode: off"
    }
}

/// `info!` for a developer line: the same words, with [`LOG_TARGET`].
#[macro_export]
macro_rules! dev_info {
    ($($arg:tt)+) => {
        $crate::__log::info!(target: $crate::dev_mode::LOG_TARGET, $($arg)+)
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_text::{production_code, rust_files};

    /// Both states (working rule 13): a developer line passes only in the
    /// mode, every other line always.
    #[test]
    fn a_developer_line_passes_only_in_the_mode() {
        assert!(!record_passes_in(LOG_TARGET, false));
        assert!(record_passes_in(LOG_TARGET, true));
        for target in ["sdr_remote_server::network", "thetislink::devices", "thetislink", ""] {
            assert!(record_passes_in(target, false), "{target}");
            assert!(record_passes_in(target, true), "{target}");
        }
    }

    #[test]
    fn the_code_ignores_case_and_spaces_and_nothing_else() {
        assert!(code_matches("PA3GHM73"));
        assert!(code_matches(" pa3ghm73\n"));
        for wrong in ["", "PA3GHM", "PA3GHM74", "PA3GHM 73", "PA3GHM73x"] {
            assert!(!code_matches(wrong), "{wrong}");
        }
    }

    /// The macro writes with the developer target. A text check: catching the
    /// record would mean installing a logger, and one test that does fights
    /// every other test in the crate.
    #[test]
    fn the_macro_uses_the_developer_target() {
        let src = include_str!("dev_mode.rs");
        let body = &src[src.find("macro_rules! dev_info").unwrap()..];
        assert!(body[..200].contains("info!(target: $crate::dev_mode::LOG_TARGET, $($arg)+)"));
    }

    /// What a line's text says about how often it comes (review of 294): every
    /// 10 s, per over, per press, per release.
    const MEASURING: [&str; 5] = ["PTT metrics", "previous over", "per over", "RX after TX", "heartbeat: feed"];

    /// The plain `info!` lines that carry such a text and stay ordinary, each
    /// with its reason: (file, a piece of the line).
    const ORDINARY: [(&str, &str); 6] = [
        // Only when a frame was lost; the server's only trace of loss on the
        // way to it (owner, 2026-10-24).
        ("tx_recover.rs", "TX audio (10 s): {} lost frame(s)"),
        // Only when the receive audio had gaps - the same rule.
        ("engine.rs", "concealing (10 s)"),
        // Only when a control had to be repeated or was given up.
        ("engine.rs", "controls: sent {}, repeated {}, given up {} (10 s)"),
        // Only when a radio's transmit audio was thrown away, or did not reach
        // its sound card (builds 184 and 185).
        ("mod.rs", "ms held while the radio was not keyed thrown away (previous over)"),
        ("mod.rs", "ms not written to the sound card (previous over)"),
        // Once, when Thetis says it can.
        ("tci.rs", "TCI: Thetis reports its TX queue per over"),
    ];

    /// Does the text say "every N s" - `(10 s)`, `(10s)`, `({} s)`, `({:.1} s)`,
    /// `({secs} s)`? A placeholder takes the space (review of 295): without
    /// it, `({:.1}s)` is the length of a recording, said once, and stays out.
    fn says_every_n_seconds(text: &str) -> bool {
        text.match_indices('(').any(|(at, _)| {
            let rest = &text[at + 1..];
            let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
            if digits > 0 {
                let after = &rest[digits..];
                return after.starts_with(" s)") || after.starts_with("s)");
            }
            rest.strip_prefix('{')
                .and_then(|r| r.find('}').map(|end| &r[end + 1..]))
                .is_some_and(|after| after.starts_with(" s)"))
        })
    }

    /// The text of a macro call from just after its `(` to its matching `)`,
    /// with string literals skipped while counting.
    fn call_text(src: &str) -> &str {
        let bytes = src.as_bytes();
        let (mut depth, mut i, mut in_str) = (1usize, 0usize, false);
        while i < bytes.len() {
            match (in_str, bytes[i]) {
                (true, b'\\') => i += 1,
                (true, b'"') => in_str = false,
                (false, b'"') => in_str = true,
                (false, b'(') => depth += 1,
                (false, b')') => {
                    depth -= 1;
                    if depth == 0 {
                        return &src[..i];
                    }
                }
                _ => {}
            }
            i += 1;
        }
        src
    }

    /// A measurement line written with a plain `info!` lands in every user's
    /// log, and the count of `dev_info!` in `fitness.ps1` cannot see it - it
    /// only sees a label that goes away (review of 294). Here every plain
    /// `info!` in the programs' own code whose text says how often it comes
    /// must be on the list above, with its reason. Not seen: a line built by a
    /// function and logged as `"{}"`.
    #[test]
    fn a_measurement_line_goes_through_the_developer_macro() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let mut files = Vec::new();
        for krate in ["sdr-remote-server", "sdr-remote-quic", "sdr-remote-logic", "sdr-remote-client", "sdr-remote-android"] {
            rust_files(&root.join(krate).join("src"), &mut files);
        }
        assert!(files.len() > 100, "the sources were found");
        let mut used = [0usize; ORDINARY.len()];
        let mut plain = Vec::new();
        for path in &files {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let src = std::fs::read_to_string(path).unwrap().replace("\r\n", "\n");
            let code = &production_code(&src);
            for (at, _) in code.match_indices("info!(") {
                if code[..at].ends_with("dev_") {
                    continue;
                }
                let text = call_text(&code[at + "info!(".len()..]);
                if !(says_every_n_seconds(text) || MEASURING.iter().any(|m| text.contains(m))) {
                    continue;
                }
                match ORDINARY.iter().position(|(file, piece)| *file == name && text.contains(piece)) {
                    Some(i) => used[i] += 1,
                    None => plain.push(format!("{name}: {}", text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or(""))),
                }
            }
        }
        assert!(plain.is_empty(), "a measurement line with a plain info! - make it dev_info!, or put it on ORDINARY with the reason: {plain:#?}");
        for (i, (file, piece)) in ORDINARY.iter().enumerate() {
            assert_eq!(used[i], 1, "{file}: `{piece}` is on the list but not found once - take it off, or fix the piece");
        }
    }

    /// The reading of "every N s" in both directions.
    #[test]
    fn every_n_seconds_is_read_from_the_text() {
        for yes in ["QUIC (10 s) to", "x ({} s): y", "(1 s)", "(10s)", "({:.1} s)", "({secs} s)"] {
            assert!(says_every_n_seconds(yes), "{yes}");
        }
        for no in ["stopped ({:.1}s)", "(seq={})", "( s)", "(10 ms)", "{} s)", "({} ms)", "({ s)"] {
            assert!(!says_every_n_seconds(no), "{no}");
        }
        assert_eq!(call_text("\"a (b\" , f(x)) rest"), "\"a (b\" , f(x)");
    }
}
