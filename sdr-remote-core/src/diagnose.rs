// SPDX-License-Identifier: GPL-2.0-or-later
//
//! Building a problem report: the log tail and the settings, cleaned before they
//! leave the machine.
//!
//! Two different mechanisms, and the difference is the whole design (§1.3,
//! §1.3.1):
//!
//! - **the log gets a denylist.** Nobody can enumerate what a log line contains,
//!   so patterns are all there is.
//! - **the settings get an allowlist.** Those keys *are* enumerable, so only what
//!   is named travels. That matters because the settings file holds `password`,
//!   `totp_secret` and `relay_token` - and the failure modes are not comparable.
//!   A setting added next year would be shipped silently by a denylist and
//!   withheld silently by an allowlist. One costs a question, the other costs the
//!   relay.
//!
//! Everything here fails closed. If the redaction cannot do its work, nothing is
//! sent and the user is told - never a raw log as a fallback.

/// How much of the tail to take. Enough to cover what just went wrong, far below
/// anything the postbox refuses.
/// How much of a log a report carries.
///
/// Two hundred kilobytes was chosen against a postbox that accepted half a
/// megabyte per report. With that ceiling now at four megabytes, the tail was
/// the binding limit and the wrong one: a fault that began an hour before
/// somebody pressed the button had already scrolled out, and the log then shows
/// the symptom without its cause. A megabyte is hours on a normal station and
/// still a quarter of what a report may weigh.
pub const LOG_TAIL_BYTES: u64 = 1024 * 1024;

/// Settings that never travel, whatever else says otherwise.
///
/// Checked first and it wins outright. A name matching one of these is refused
/// even if it also matches a safe family below, so a key called
/// `spectrum_secret_key` cannot slip through on the strength of its prefix.
const NEVER_SEND: &[&str] = &[
    "password", "secret", "token", "key", "url", "instance", "credential", "auth",
];

/// Families of settings that are safe to send, by prefix.
///
/// Per family rather than per name, because naming all hundred and fifty-odd
/// individually was unmaintainable and - worse - the first attempt was written
/// from the server's key names while this reads the client's file, so nine of a
/// hundred and sixty-five got through and a report explained nothing.
///
/// This is still an allowlist: a key that matches no family stays home, so
/// something added next year is withheld rather than shipped. What the families
/// buy is that a new `vrx3_volume` is useful immediately instead of needing this
/// list edited first.
const SAFE_PREFIXES: &[&str] = &[
    // The client's file.
    "spectrum_", "rx1_", "rx2_", "vrx1_", "vrx2_", "band_mem_", "midi_", "collapse_",
    "popout_", "layout_", "yaesu_", "yaesu2_", "catsync_", "amplitec_", "ub_", "mem",
    "thetis_", "relay_", "audio_", "mic_", "meter_", "auto_ref", "full_spectrum",
    "bw_", "device_", "input_", "output_", "vrx_", "waterfall_", "wf_", "window_",
    // The server's file: the accessories are where its problems live, and
    // "my rotor will not turn" is unanswerable without them.
    "rotor", "tuner", "spe_", "ultrabeam_", "rf2k_", "pstrotator_", "mcp2221_",
    "dxcluster_", "main_", "totp_enabled",
];

/// Individual settings outside any family that are worth having.
const SAFE_EXACT: &[&str] = &[
    "language", "theme", "theme_custom", "ui_zoom", "volume", "local_volume", "play_volume",
    "tx_gain", "vfo_a_volume", "vfo_b_volume", "ptt_toggle", "server", "successful_connects",
    "agc_enabled", "main_window_pos", "chat_open",
    // The server's own.
    "tci", "autostart", "active_pa",
];

fn setting_allowed(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    // The veto first, so nothing below can override it.
    if NEVER_SEND.iter().any(|bad| k.contains(bad)) {
        return false;
    }
    SAFE_EXACT.contains(&k.as_str()) || SAFE_PREFIXES.iter().any(|p| k.starts_with(p))
}

#[derive(Debug, PartialEq, Eq)]
pub enum DiagnoseError {
    /// No log to send. Not a failure of redaction, but there is nothing to do.
    NoLog(String),
    /// The redaction could not be trusted, so nothing was produced. Never falls
    /// back to sending the raw text.
    Unsafe(String),
}

impl std::fmt::Display for DiagnoseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiagnoseError::NoLog(p) => write!(f, "no log file found at {p}"),
            DiagnoseError::Unsafe(why) => write!(f, "nothing was sent: {why}"),
        }
    }
}

/// The optional attachment: settings and the tail of the log, cleaned.
///
/// Optional on purpose. The report itself is what the operator writes; this is
/// evidence they choose to add, so it is built only once they ask for it and a
/// failure here costs them the attachment, never the report.
pub fn build_attachment(
    log_path: &str,
    settings_path: &str,
    relay_url: &str,
) -> Result<String, DiagnoseError> {
    let host = relay_host(relay_url);
    // A relay is configured but its host cannot be worked out: the one thing
    // that must never travel is then unknown, so nothing goes.
    if !relay_url.trim().is_empty() && host.is_none() {
        return Err(DiagnoseError::Unsafe(
            "the relay address could not be read, so it could not be removed".to_string(),
        ));
    }

    let raw = read_tail(log_path).ok_or_else(|| DiagnoseError::NoLog(log_path.to_string()))?;
    let settings = match std::fs::read_to_string(settings_path) {
        Ok(text) => text,
        // Settings are helpful, not essential. A missing file is worth saying so
        // in the report rather than failing the whole thing.
        Err(_) => String::new(),
    };
    build_attachment_from_text(&raw, &settings, relay_url)
}

/// The same attachment, for a platform that has neither file to point at.
///
/// Android keeps its log in the system log and its settings in the framework's
/// own preferences, so both arrive as text the app already holds. Cleaning them
/// is the same job with the same rules, and it must stay the same job: two
/// redactors would be one redactor and one that quietly falls behind.
///
/// An empty settings text is said rather than left blank, exactly as a missing
/// file is - a report that explains little should not be mistaken for one that
/// arrived damaged.
pub fn build_attachment_from_text(
    raw_log: &str,
    raw_settings: &str,
    relay_url: &str,
) -> Result<String, DiagnoseError> {
    let host = relay_host(relay_url);
    if !relay_url.trim().is_empty() && host.is_none() {
        return Err(DiagnoseError::Unsafe(
            "the relay address could not be read, so it could not be removed".to_string(),
        ));
    }
    // The station's own address comes out of the settings on their way past.
    // With no settings there is no name to hide - and no `server=` line to
    // leak either, so the two absences cancel. The log can still mention it,
    // which is said in the attachment rather than left to be discovered.
    let own = server_host(raw_settings);
    let mut hosts: Vec<(&str, &str)> = Vec::new();
    if let Some(h) = host.as_deref() {
        hosts.push((h, "<relay>"));
    }
    if let Some(h) = own.as_deref() {
        hosts.push((h, "<server>"));
    }
    // The tail, and only the tail: the same ceiling a file gets, so a chatty
    // platform cannot turn a report into a transfer.
    let tail = tail_of(raw_log, LOG_TAIL_BYTES as usize);
    let log = redact(tail, &hosts);
    let settings = if raw_settings.trim().is_empty() {
        "(no settings were available)".to_string()
    } else {
        allowed_settings(raw_settings, &hosts)
    };
    Ok(format!(
        "--- settings ---\n{settings}\n--- log (last {} kB, cleaned) ---\n{log}",
        LOG_TAIL_BYTES / 1024
    ))
}

/// The last `max` bytes of a text, cut at a line boundary so the first line of
/// the attachment is a whole line rather than half of one.
fn tail_of(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let start = text.len() - max;
    match text[start..].find('\n') {
        Some(i) => &text[start + i + 1..],
        None => &text[start..],
    }
}

/// The report: what the operator says is wrong, with the attachment if they
/// chose to add one.
///
/// The description leads because it is the part only they can supply. A log
/// without it is a puzzle with no picture on the box, and an attachment is
/// evidence for a story that has to be told first.
///
/// Kept apart from `build_attachment` on purpose: the attachment is read from
/// disk once and then shown, and what is sent has to be what was on screen
/// (design 1.1). Rebuilding it at send time would re-read a log that has moved
/// on since.
///
/// The description is redacted with the same rules as the log. Not to police
/// what somebody writes about their own problem, but because the relay host is
/// refused at the far end: a description mentioning it would come back as
/// "please update the client", which explains nothing to the person who typed it.
pub fn describe(
    note: &str,
    relay_url: &str,
    version: &str,
    platform: &str,
    attachment: Option<&str>,
) -> String {
    // Only the relay here. The station's own address is not known at this
    // point - it comes from the settings, and a report may carry none - and
    // the operator writing their own address in their own sentence is a
    // different thing from a log line carrying it without them noticing.
    let relay = relay_host(relay_url);
    let hosts: Vec<(&str, &str)> = match relay.as_deref() {
        Some(h) => vec![(h, "<relay>")],
        None => Vec::new(),
    };
    let note = redact(note.trim(), &hosts);
    let mut out = format!("--- what is wrong ---\n{note}\n\nThetisLink {version} on {platform}\n");
    match attachment {
        Some(a) => {
            out.push('\n');
            out.push_str(a);
        }
        // Said rather than left blank, so a report that explains little is not
        // mistaken for one that arrived damaged.
        None => out.push_str("\n(no log or settings were attached)\n"),
    }
    out
}

/// The station's own server address, taken from the settings being attached.
///
/// Derived here rather than passed in, and that is the point: every caller
/// already hands over the settings, so there is no way to build an attachment
/// while forgetting to say which host to hide. A parameter would have to be
/// remembered by the desktop client, the server's own GUI and Android alike,
/// and the one that forgot would leak silently.
///
/// An IP was never the problem - those go as `<ip>` like any other. A station
/// reachable under a name is: `mystation.duckdns.org` is a house, it is not a
/// number, and nothing in the denylist recognised it (2026-08-15).
pub fn server_host(settings: &str) -> Option<String> {
    settings
        .lines()
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| SERVER_KEYS.iter().any(|n| k.trim().eq_ignore_ascii_case(n)))
        .and_then(|(_, v)| relay_host(v))
        .filter(|h| !h.chars().all(|c| c.is_ascii_digit() || c == '.'))
}

/// What the address is called in each platform's settings.
///
/// Deriving the host instead of passing it in was meant to make forgetting
/// impossible - and it moved the forgetting one step along: the desktop writes
/// `server`, Android writes `server_addr`, and matching one name exactly meant
/// the phone quietly found nothing. A silent `None` is the same leak as a
/// forgotten parameter, in the shape the comment above thought it had ruled out
/// (found in review, 2026-08-18).
///
/// This is a hand-kept list and nothing detects a platform that is missing from
/// it: the test below checks the spellings named here, so a third client with a
/// third key name is covered only once somebody adds it. Said plainly because
/// the review asked twice whether this is structure or a convention wearing its
/// clothes - it is a convention, and a smaller one than before.
const SERVER_KEYS: [&str; 2] = ["server", "server_addr"];

/// The host part of a relay address, for removal.
pub fn relay_host(relay_url: &str) -> Option<String> {
    let t = relay_url.trim();
    if t.is_empty() {
        return None;
    }
    let h = t
        .strip_prefix("wss://")
        .or_else(|| t.strip_prefix("ws://"))
        .or_else(|| t.strip_prefix("https://"))
        .or_else(|| t.strip_prefix("http://"))
        .unwrap_or(t);
    let h = h.split('/').next().unwrap_or(h);
    let h = h.split(':').next().unwrap_or(h).trim();
    if h.is_empty() {
        None
    } else {
        Some(h.to_string())
    }
}

/// The denylist pass over a log.
///
/// Callsigns are deliberately left alone: they may be somebody's chosen name and
/// they are often what makes a report readable.
pub fn redact(text: &str, hosts: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let mut l = line.to_string();
        // A list rather than one host, because there are two kinds and they are
        // not interchangeable: the relay belongs to everybody using it, the
        // station's own address belongs to whoever lives there. Each keeps its
        // own label so a report still reads.
        for (h, label) in hosts {
            if !h.is_empty() {
                l = replace_ignore_case(&l, h, label);
            }
        }
        l = strip_ips(&l);
        l = strip_emails(&l);
        l = strip_user_paths(&l);
        l = strip_secrets(&l);
        out.push_str(&l);
        out.push('\n');
    }
    out
}

fn replace_ignore_case(hay: &str, needle: &str, with: &str) -> String {
    let (lh, ln) = (hay.to_ascii_lowercase(), needle.to_ascii_lowercase());
    let mut out = String::with_capacity(hay.len());
    let mut i = 0;
    while let Some(pos) = lh[i..].find(&ln) {
        let at = i + pos;
        out.push_str(&hay[i..at]);
        out.push_str(with);
        i = at + needle.len();
    }
    out.push_str(&hay[i..]);
    out
}

/// Dotted quads. Loopback stays: it says something useful and identifies nobody.
///
/// Two things keep this from eating text that is not an address. A group above
/// 255 is not one, and anything directly preceded by a letter or digit is part
/// of something else - which is what saves a version number like `v2.10.3.15`
/// from being mangled into `v<ip>` and the log with it.
fn strip_ips(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let bytes: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            let mut groups = 0;
            let mut j = i;
            loop {
                let ds = j;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j == ds || j - ds > 3 {
                    break;
                }
                groups += 1;
                if groups == 4 {
                    break;
                }
                if j < bytes.len() && bytes[j] == '.' {
                    j += 1;
                } else {
                    break;
                }
            }
            let preceded_by_word = start > 0
                && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == '_');
            let groups_in_range = {
                let candidate: String = bytes[start..j].iter().collect();
                candidate
                    .split('.')
                    .all(|g| g.parse::<u16>().map(|n| n <= 255).unwrap_or(false))
            };
            if groups == 4 && !preceded_by_word && groups_in_range {
                let found: String = bytes[start..j].iter().collect();
                if found.starts_with("127.") {
                    out.push_str(&found);
                } else {
                    out.push_str("<ip>");
                }
                i = j;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

fn strip_emails(line: &str) -> String {
    line.split_whitespace()
        .map(|w| {
            let looks_like = w.contains('@')
                && w.split('@').count() == 2
                && w.split('@').nth(1).map(|d| d.contains('.')).unwrap_or(false);
            if looks_like {
                "<email>".to_string()
            } else {
                w.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `C:\Users\<name>\...` and `/home/<name>/...`: the folder name is the person.
fn strip_user_paths(line: &str) -> String {
    let mut l = line.to_string();
    for marker in ["\\Users\\", "/Users/", "/home/"] {
        let sep = if marker.contains('\\') { '\\' } else { '/' };
        let needle = marker.to_ascii_lowercase();
        // Walk forward through the line. The cursor only ever advances past what
        // was just replaced, which is both why a second path on the same line is
        // still found and why this cannot spin.
        let mut from = 0;
        while let Some(rel) = l[from..].to_ascii_lowercase().find(&needle) {
            let at = from + rel;
            let after = at + marker.len();
            let end = l[after..]
                .find(sep)
                .map(|e| after + e)
                .unwrap_or_else(|| {
                    l[after..]
                        .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
                        .map(|e| after + e)
                        .unwrap_or(l.len())
                });
            if end <= after {
                // A marker with nothing behind it. Step over it rather than
                // replacing an empty span for ever.
                from = after;
                continue;
            }
            l.replace_range(after..end, "<user>");
            from = after + "<user>".len();
            if from >= l.len() {
                break;
            }
        }
    }
    l
}

/// Anything that looks like it was meant to stay secret.
fn strip_secrets(line: &str) -> String {
    const MARKERS: &[&str] = &["password", "secret", "token", "key=", "apikey", "totp"];
    let lower = line.to_ascii_lowercase();
    if MARKERS.iter().any(|m| lower.contains(m)) {
        // The whole line goes, not just the value: the shapes these appear in are
        // too varied to trust a narrower cut.
        return "<line removed: it mentioned a password, key or token>".to_string();
    }
    line.to_string()
}

/// The allowlist pass over a settings file.
pub fn allowed_settings(text: &str, hosts: &[(&str, &str)]) -> String {
    let mut kept: Vec<String> = Vec::new();
    let mut withheld = 0usize;
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let key = k.trim();
        if setting_allowed(key) {
            // The value gets the same treatment as a log line. Half of what
            // explains a server problem is an address - the TCI host, the
            // amplifier, the rotor - and withholding those keys outright would
            // cost more than it protects. Scrubbed, the useful half survives:
            // that it is set, and on which port.
            kept.push(format!("{key}={}", scrub_value(v.trim(), hosts)));
        } else {
            withheld += 1;
        }
    }
    if withheld > 0 {
        // Said out loud, so nobody wonders whether the list is complete: what is
        // missing was withheld on purpose.
        kept.push(format!("({withheld} other setting(s) not sent)"));
    }
    kept.join("\n")
}

/// A setting's value, cleaned the way a log line is.
///
/// Deliberately the same three passes and not a fourth idea of its own: two
/// notions of what counts as an address is one too many, and the one that gets
/// forgotten is the one that leaks.
fn scrub_value(v: &str, hosts: &[(&str, &str)]) -> String {
    let mut out = v.to_string();
    for (h, label) in hosts {
        if !h.is_empty() {
            out = replace_ignore_case(&out, h, label);
        }
    }
    strip_secrets(&strip_user_paths(&strip_emails(&strip_ips(&out))))
}

/// The last [`LOG_TAIL_BYTES`] of a log file, starting on a whole line.
///
/// Public because Android keeps its own log file now and must read it the same
/// way. Two tail readers would be one tail reader and one that quietly falls
/// behind it - the same argument that keeps the redaction in one place.
pub fn read_tail(path: &str) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let from = len.saturating_sub(LOG_TAIL_BYTES);
    f.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).to_string();
    // A cut at a byte offset lands in the middle of a line. Drop that fragment,
    // so a report does not open on the tail end of a sentence and read as a
    // damaged file.
    if from == 0 {
        return Some(text);
    }
    Some(match text.find('\n') {
        Some(i) => text[i + 1..].to_string(),
        None => text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_relay_host_is_removed_whatever_its_case() {
        let out = redact("connecting to Relay.Example.ORG:443 now", &[("relay.example.org", "<relay>")]);
        assert!(out.contains("<relay>"), "{out}");
        assert!(!out.to_lowercase().contains("relay.example.org"), "{out}");
    }

    /// Every platform's name for its own server address must be recognised.
    ///
    /// The derivation was built so that no caller could forget to say which
    /// host to hide - and then matched one key name exactly, while Android
    /// writes another. That is not a forgotten parameter, it is a silent
    /// `None`: the same leak in the shape the design thought it had ruled out.
    /// A new platform adds its name to SERVER_KEYS, and this fails until it
    /// does (found in review, 2026-08-18).
    #[test]
    fn every_platform_spelling_of_the_server_address_is_found() {
        for key in ["server", "server_addr", "SERVER", "Server_Addr"] {
            let settings = format!("volume=0.5
{key}=mystation.duckdns.org:4580
language=nl");
            assert_eq!(
                server_host(&settings).as_deref(),
                Some("mystation.duckdns.org"),
                "settings written with `{key}` left the station's own name in the log"
            );
        }
    }

    /// And the whole point of finding it: it comes out of the log.
    #[test]
    fn the_stations_own_name_is_removed_for_a_phone_too() {
        let settings = "server_addr=mystation.duckdns.org:4580
language=nl";
        let log = "connecting to mystation.duckdns.org:4580 - timed out";
        let out = build_attachment_from_text(log, settings, "wss://relay.example.org").unwrap();
        assert!(!out.contains("mystation.duckdns.org"), "own name still in the report:
{out}");
        assert!(out.contains("<server>"), "and it should say what was taken out");
    }

    /// The one thing that must never travel. If it cannot be identified, nothing
    /// goes at all - a report is not worth leaking the address for.
    #[test]
    fn an_unreadable_relay_address_stops_the_whole_report() {
        let e = build_attachment("nofile.log", "nofile.conf", "wss://");
        assert!(matches!(e, Err(DiagnoseError::Unsafe(_))), "{e:?}");
    }

    #[test]
    fn addresses_of_other_people_go_but_loopback_stays() {
        let out = redact("peer 77.63.41.34 and local 127.0.0.1 talking", &[]);
        assert!(out.contains("<ip>"), "{out}");
        assert!(!out.contains("77.63.41.34"), "{out}");
        assert!(out.contains("127.0.0.1"), "loopback identifies nobody: {out}");
    }

    /// A version number is not an address, and mangling it would make logs
    /// unreadable.
    #[test]
    fn a_version_number_is_not_an_address() {
        let out = redact("Thetis v2.10.3.15 started", &[]);
        assert!(out.contains("2.10.3.15"), "{out}");
    }

    /// 300 is not an octet, so this is not an address either.
    #[test]
    fn a_number_out_of_range_is_not_an_address() {
        let out = redact("counts 300.1.2.3 items", &[]);
        assert!(out.contains("300.1.2.3"), "{out}");
    }

    #[test]
    fn the_user_name_in_a_path_goes() {
        let out = redact(r"reading C:\Users\chiron\AppData\file.log now", &[]);
        assert!(out.contains("<user>"), "{out}");
        assert!(!out.contains("chiron"), "{out}");
        assert!(out.contains("AppData"), "the rest of the path is useful: {out}");
    }

    #[test]
    fn two_paths_on_one_line_are_both_handled() {
        let out = redact(r"from C:\Users\anna\a.txt to C:\Users\bob\b.txt", &[]);
        assert!(!out.contains("anna"), "{out}");
        assert!(!out.contains("bob"), "{out}");
    }

    #[test]
    fn an_email_address_goes() {
        let out = redact("mail to iemand@example.org about it", &[]);
        assert!(out.contains("<email>"), "{out}");
        assert!(!out.contains("example.org"), "{out}");
    }

    /// The whole line goes: the shapes a secret appears in are too varied to
    /// trust a narrower cut.
    #[test]
    fn a_line_mentioning_a_secret_is_dropped_entirely() {
        for line in [
            "relay_token=abc123",
            "using password hunter2",
            "TOTP secret is XYZ",
            "api key=zzz",
        ] {
            let out = redact(line, &[]);
            assert!(out.contains("line removed"), "{line} -> {out}");
        }
    }

    /// The description leads, because it is what makes the rest legible.
    #[test]
    fn the_users_own_words_come_first() {
        let out = describe(
            "no audio after band change",
            "",
            "v2.8.0",
            "windows",
            Some("--- settings ---
language=en
"),
        );
        assert!(
            out.starts_with("--- what is wrong ---
no audio after band change"),
            "{out}"
        );
        assert!(out.contains("language=en"), "{out}");
    }

    /// A report with nothing attached is still a report, and says so - a silent
    /// gap reads as something that went wrong on the way.
    #[test]
    fn a_report_without_an_attachment_stands_on_its_own() {
        let out = describe("PTT sticks on 60m", "", "v2.8.0", "windows", None);
        assert!(out.contains("PTT sticks on 60m"), "{out}");
        assert!(out.contains("no log or settings were attached"), "{out}");
    }

    /// The version travels either way: it is the first thing worth knowing and
    /// costs nothing to include.
    #[test]
    fn the_version_travels_whether_or_not_anything_is_attached() {
        for attached in [None, Some("--- settings ---
")] {
            let out = describe("x", "", "v2.8.0", "linux", attached);
            assert!(out.contains("ThetisLink v2.8.0 on linux"), "{out}");
        }
    }

    /// Typed into the description, the relay host would still be refused at the
    /// far end - and the refusal would name the client, not the sentence.
    #[test]
    fn the_relay_host_is_removed_from_the_description_too() {
        let out = describe(
            "cannot reach relay.example.org at all",
            "wss://relay.example.org/tunnel",
            "v2.8.0",
            "windows",
            None,
        );
        assert!(!out.contains("relay.example.org"), "{out}");
        assert!(out.contains("<relay>"), "{out}");
    }

    /// The server's accessories are where its problems live, and both files go
    /// through the same list - so this pins the half that was measured against
    /// the server's own config rather than assumed.
    #[test]
    fn the_servers_own_settings_travel_too() {
        let conf = concat!(
            "tci=127.0.0.1:40001
",
            "rotor1_enabled=true
",
            "tuner2_model=UltraBeam
",
            "spe_port=COM9
",
            "ultrabeam_enabled=false
",
            "rf2k_enabled=true
",
            "totp_enabled=true
",
        );
        let out = allowed_settings(conf, &[]);
        for want in ["rotor1_enabled=true", "tuner2_model=UltraBeam", "spe_port=COM9"] {
            assert!(out.contains(want), "{want} missing from {out}");
        }
        // Whether two-factor is on explains a login problem; the secret behind
        // it is a different key and is refused by name.
        assert!(out.contains("totp_enabled=true"), "{out}");
        assert!(!out.contains("other setting"), "nothing should be withheld: {out}");
    }

    /// A value gets the same scrubbing as a log line, which is what lets an
    /// address-shaped setting travel at all: that it is set and on which port is
    /// the useful half, and the address itself is not.
    #[test]
    fn an_address_in_a_setting_is_scrubbed_not_withheld() {
        let out = allowed_settings("tci=192.168.1.97:40001
rf2k_addr=10.0.0.5
", &[]);
        assert!(!out.contains("192.168.1.97"), "{out}");
        assert!(!out.contains("10.0.0.5"), "{out}");
        assert!(out.contains("tci=<ip>:40001"), "the port survives: {out}");
        assert!(out.contains("rf2k_addr="), "the key survives: {out}");
    }

    /// The secret the server keeps is not the one named in the veto list, so
    /// this pins it by the name it actually has in the file.
    #[test]
    fn the_servers_own_crown_jewels_never_travel() {
        let conf = "password=hunter2
totp_secret=JBSWY3DP
relay_token=abc
";
        let out = allowed_settings(conf, &[]);
        for leak in ["hunter2", "JBSWY3DP", "abc"] {
            assert!(!out.contains(leak), "{leak} leaked: {out}");
        }
    }

    #[test]
    fn the_settings_that_explain_a_problem_travel() {
        let conf = "yaesu_port=COM7\nspectrum_ref_db=-110\nvrx1_mode=USB\nlanguage=en\nmidi_device=X-Touch\n";
        let out = allowed_settings(conf, &[]);
        for want in ["yaesu_port=COM7", "spectrum_ref_db=-110", "vrx1_mode=USB", "language=en"] {
            assert!(out.contains(want), "{want} missing from {out}");
        }
    }

    /// The seven that must never leave, and each of them would have matched a
    /// safe family on its prefix - which is exactly why the veto is checked
    /// first and wins outright.
    #[test]
    fn the_sensitive_ones_never_travel_even_though_their_family_is_safe() {
        let conf = concat!(
            "password=hunter2\n",
            "relay_token=abc\n",
            "relay_url=wss://secret.example.org\n",
            "relay_instance_id=xyz\n",
            "catsync_url=http://elders.example\n",
            "catsync_url_y1=http://elders.example\n",
            "catsync_url_y2=http://elders.example\n",
        );
        let out = allowed_settings(conf, &[]);
        for leak in ["hunter2", "abc", "secret.example.org", "xyz", "elders.example"] {
            assert!(!out.contains(leak), "{leak} leaked: {out}");
        }
        assert!(out.contains("7 other setting"), "and it says so: {out}");
    }

    /// A prefix cannot buy its way past the veto.
    #[test]
    fn a_safe_family_does_not_launder_a_secret() {
        let out = allowed_settings("spectrum_secret_key=zzz\nrelay_password=q\n", &[]);
        assert!(!out.contains("zzz"), "{out}");
        assert!(!out.contains("q"), "{out}");
    }

    /// The point of an allowlist: something nobody thought of stays home, and
    /// the report says so rather than pretending it is complete.
    #[test]
    fn an_unfamiliar_key_stays_home_and_is_counted() {
        let out = allowed_settings("brand_new_thing=1\nlanguage=nl\n", &[]);
        assert!(!out.contains("brand_new_thing"), "{out}");
        assert!(out.contains("1 other setting"), "{out}");
    }

    #[test]
    fn the_host_is_taken_from_any_spelling_of_the_relay_address() {
        for url in [
            "wss://relay.example.org",
            "wss://relay.example.org:443",
            "https://relay.example.org/",
            "relay.example.org",
        ] {
            assert_eq!(relay_host(url).as_deref(), Some("relay.example.org"), "{url}");
        }
        assert_eq!(relay_host("   "), None);
    }

    /// The gap this was written for: an IP was always removed, a name never
    /// was. A station on a DDNS name was posting its own address, in the
    /// settings and in every log line that mentioned it.
    #[test]
    fn a_station_reachable_by_name_does_not_post_its_address() {
        let settings = "server=mystation.duckdns.org:4580\nlanguage=en\n";
        let log = "connecting to mystation.duckdns.org:4580 now";
        let out = build_attachment_from_text(log, settings, "wss://relay.example.org").unwrap();
        assert!(!out.contains("mystation.duckdns.org"), "{out}");
        assert!(out.contains("<server>"), "{out}");
        // The port survives, the same way it does for an address-shaped
        // setting: which port it is on explains half the problem.
        assert!(out.contains("<server>:4580"), "{out}");
    }

    /// And the relay keeps its own label, so a report still says which of the
    /// two a line was about.
    #[test]
    fn the_two_kinds_of_host_stay_apart() {
        let settings = "server=mystation.duckdns.org:4580\n";
        let log = "relay.example.org refused, falling back to mystation.duckdns.org";
        let out = build_attachment_from_text(log, settings, "wss://relay.example.org").unwrap();
        assert!(out.contains("<relay>"), "{out}");
        assert!(out.contains("<server>"), "{out}");
    }

    /// An address is still an address: nothing changes for a station on a
    /// number, which is the common case and was never leaking.
    #[test]
    fn a_station_on_a_number_is_handled_as_before() {
        let settings = "server=192.168.1.79:4580\n";
        let out = build_attachment_from_text("talking to 192.168.1.79", settings, "").unwrap();
        assert!(!out.contains("192.168.1.79"), "{out}");
        assert!(out.contains("<ip>"), "{out}");
        assert!(!out.contains("<server>"), "an IP needs no second label: {out}");
    }

    /// No settings, no name to hide - and no `server=` line to leak either.
    #[test]
    fn without_settings_there_is_nothing_to_derive_and_nothing_to_lose() {
        let out = build_attachment_from_text("just a log line", "", "").unwrap();
        assert!(out.contains("no settings were available"), "{out}");
    }

    #[test]
    fn the_server_host_is_read_from_the_settings_whatever_else_is_in_them() {
        let settings = "volume=0.2\nlanguage=nl\nserver=box.example.net:4580\n";
        assert_eq!(server_host(settings).as_deref(), Some("box.example.net"));
        assert_eq!(server_host("volume=0.2\n").as_deref(), None);
        assert_eq!(server_host("server=192.168.1.79:4580\n").as_deref(), None);
    }
}
