// SPDX-License-Identifier: GPL-2.0-or-later
//
//! Talking to the chat service, off the UI thread.
//!
//! Two rules shape this module, and both come from the design rather than from
//! taste:
//!
//! **Nothing here may ever block anything else** (§2.4). The chat is a
//! convenience next to audio and PTT, so it lives on its own thread, owns its
//! own network calls, and the interface to the UI is a pair of channels. A chat
//! server that is down, slow or missing must be invisible to the rest of
//! ThetisLink — a window that says "offline" and nothing more.
//!
//! **It polls rather than holding a connection open.** A chat may lag a few
//! seconds; that is what "less critical" means in practice, and it removes a
//! whole class of things to get wrong — idle connections, reconnect storms, a
//! socket kept alive across a laptop suspend.

use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

/// How often to ask for new messages while the window is open.
pub const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// And how often while it is closed - just enough to keep the unread number on
/// the Chat button honest (design §1.7). One small request per half minute is
/// the price of a badge that can actually appear; a counter fed only while the
/// window is open can never be seen. Members only, and never on the audio path.
pub const POLL_INTERVAL_CLOSED: Duration = Duration::from_secs(30);

/// Nothing here is worth waiting on. If the chat service does not answer in this
/// long, the answer is "offline" and the UI carries on.
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone)]
pub enum ChatCommand {
    /// The relay handed out a (new) ticket, or withdrew one by disconnecting.
    Ticket(Option<String>),
    /// Base URL of the chat service, derived from the relay address.
    Endpoint(Option<String>),
    /// Ask for the current state: consented, and under which name.
    Refresh,
    /// Agree, under this name, to the consent text of this version.
    ///
    /// The version travels in the command rather than in shared state: this
    /// enum crosses from the UI thread to the worker thread, and anything not
    /// carried across in the command itself is a copy the other thread never
    /// sees. It must be the version from the `State` event - what the user was
    /// actually shown - or the service will refuse the consent (design §6.3).
    Consent { display_name: String, text_version: i64 },
    /// The two buttons of design §6.4.
    Leave { delete_messages: bool },
    Send {
        body: String,
        /// The message being answered, when one is.
        reply_to: Option<i64>,
    },
    /// Correct one's own message. The service is the judge of "own" and of the
    /// window; this only carries the request.
    Edit { id: i64, body: String },
    /// Fetch any answer the administrator has sent back.
    ///
    /// Its own command and not part of the message poll: a reply is not a chat
    /// message, it arrives rarely, and it must reach somebody who never joined
    /// the chat at all - reporting a problem does not require consent.
    Answers,
    /// One problem report, already cleaned and already seen by the user.
    ///
    /// The redaction happens before this, not here: what leaves the machine has
    /// to be what was shown on screen, or the preview is theatre (design 1.3).
    SendDiagnosis { report: String },
    /// Fetch anything after the last id we hold.
    Poll { since: i64 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatMessage {
    pub id: i64,
    pub at: i64,
    /// `None` for somebody who left the chat: their text stays, they do not.
    pub name: Option<String>,
    pub body: String,
    /// What this message answers, when it answers one. The author and the first
    /// words come with it, because a client only asks for what is new and so
    /// cannot look up a message it never held.
    pub reply_name: Option<String>,
    pub reply_text: Option<String>,
    /// Corrected by its author after it was posted. Shown as a marker: a
    /// message that changed after people read it should say so.
    pub edited: bool,
}

/// One answer to one problem report (design section 1.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatAnswer {
    pub id: i64,
    pub at: i64,
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfflineReason {
    /// No relay address at all: this client has no chat, and that is a setting
    /// rather than a fault.
    NoRelay,
    /// A relay, but it handed out no ticket - so there is no chat behind it.
    NoTicket,
    /// Both in hand, and nothing answered.
    Unreachable,
}

#[derive(Debug, Clone)]
pub enum ChatEvent {
    /// Reachable, and whether this station has agreed.
    State {
        consented: bool,
        display_name: Option<String>,
        consent_text_version: i64,
        /// How many problem reports this station may still send today, or -1
        /// when the service is too old to say. Known before the form is filled
        /// in, which is the only useful moment to know it.
        reports_left: i64,
    },
    /// What the poll brought: new messages, and corrections to ones the window
    /// may already hold. Two lists in one event, because they arrive in one
    /// answer and belong to one screen update.
    Messages {
        new: Vec<ChatMessage>,
        edited: Vec<ChatMessage>,
    },
    /// A problem report arrived at the postbox.
    DiagnosisSent,
    /// Everything the administrator has answered, as it stands.
    ///
    /// The whole set every time rather than what is new: there are at most a
    /// handful, one per report, and an answer worth reading twice should not
    /// depend on the window having been open when it arrived.
    Answers(Vec<ChatAnswer>),
    /// Something did not work, in words a user can act on. The design asks for
    /// the reason to reach the sender and not only the log (§8), so the service
    /// sends one and this passes it through unchanged.
    Failed(String),
    /// Not usable, and WHY.
    ///
    /// "Offline" on its own covers three different situations that need three
    /// different things from the user - no relay configured, a relay that offers
    /// no chat, and a service that is not answering. Collapsing them into one
    /// word is exactly the fault this project keeps finding in review: a window
    /// that says nothing useful sends somebody to the maker's mailbox.
    Offline(OfflineReason),
}

/// Where the chat lives, given the relay the client is using.
///
/// Same host, same TLS, the path Caddy routes (design §2.1). Derived rather than
/// configured so there is no second address to keep in step - and a relay
/// without a chat behind it simply answers 404, which reads as offline.
pub fn endpoint_for_relay(relay_url: &str) -> Option<String> {
    sdr_remote_relay::chat_endpoint(relay_url)
}

/// The worker loop. Runs on its own thread and never touches the UI directly.
pub fn run(rx: Receiver<ChatCommand>, tx: Sender<ChatEvent>) {
    let client = match reqwest::blocking::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            // Without a client there is nothing this thread can do, but the UI
            // must still be told rather than left waiting.
            log::warn!("chat: no HTTP client ({e})");
            let _ = tx.send(ChatEvent::Offline(OfflineReason::Unreachable));
            return;
        }
    };

    let mut ticket: Option<String> = None;
    let mut base: Option<String> = None;

    while let Ok(cmd) = rx.recv() {
        match cmd {
            ChatCommand::Ticket(t) => ticket = t,
            ChatCommand::Endpoint(b) => base = b,
            other => {
                let (Some(b), Some(t)) = (base.as_deref(), ticket.as_deref()) else {
                    // Which of the two is missing decides what the window says.
                    let why = if base.is_none() {
                        OfflineReason::NoRelay
                    } else {
                        OfflineReason::NoTicket
                    };
                    let _ = tx.send(ChatEvent::Offline(why));
                    continue;
                };
                let event = perform(&client, b, t, &other);
                let _ = tx.send(event);
            }
        }
    }
}

fn perform(
    client: &reqwest::blocking::Client,
    base: &str,
    ticket: &str,
    cmd: &ChatCommand,
) -> ChatEvent {
    let auth = format!("Bearer {ticket}");
    let result = match cmd {
        ChatCommand::Refresh => client
            .get(format!("{base}/state"))
            .header("Authorization", &auth)
            .send(),
        ChatCommand::Poll { since } => client
            .get(format!("{base}/messages?since={since}"))
            .header("Authorization", &auth)
            .send(),
        ChatCommand::Consent { display_name, text_version } => client
            .post(format!("{base}/consent"))
            .header("Authorization", &auth)
            .body(format!(
                r#"{{"display_name":{},"text_version":{}}}"#,
                json_string(display_name),
                // Sent back so the service can refuse a consent recorded against
                // a text this build never showed (design §6.3).
                text_version
            ))
            .send(),
        ChatCommand::Leave { delete_messages } => client
            .post(format!("{base}/leave"))
            .header("Authorization", &auth)
            .body(format!(r#"{{"delete_messages":{delete_messages}}}"#))
            .send(),
        ChatCommand::Send { body, reply_to } => client
            .post(format!("{base}/message"))
            .header("Authorization", &auth)
            .body(match reply_to {
                Some(id) => format!(r#"{{"body":{},"reply_to":{}}}"#, json_string(body), id),
                None => format!(r#"{{"body":{}}}"#, json_string(body)),
            })
            .send(),
        ChatCommand::Edit { id, body } => client
            .post(format!("{base}/message/edit"))
            .header("Authorization", &auth)
            .body(format!(r#"{{"id":{},"body":{}}}"#, id, json_string(body)))
            .send(),
        ChatCommand::Answers => client
            .get(format!("{base}/replies"))
            .header("Authorization", &auth)
            .send(),
        ChatCommand::SendDiagnosis { report } => client
            .post(format!("{base}/diagnosis"))
            .header("Authorization", &auth)
            .body(format!(r#"{{"report":{}}}"#, json_string(report)))
            .send(),
        // Handled by the caller.
        ChatCommand::Ticket(_) | ChatCommand::Endpoint(_) => {
            return ChatEvent::Offline(OfflineReason::Unreachable)
        }
    };

    let resp = match result {
        Ok(r) => r,
        // Allowed to be down; the window says which kind of quiet this is.
        Err(_) => return ChatEvent::Offline(OfflineReason::Unreachable),
    };

    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);

    if !status.is_success() {
        // A refusal the client is meant to recognise says so in a code, and
        // then it is said in the reader's own language. This is the one
        // message in the service that HAS to be understood - everything around
        // it, including the clause on the consent screen that announces it, is
        // translated, and this was not.
        if json.get("code").and_then(|c| c.as_str()) == Some("banned") {
            let mut line = rust_i18n::t!("chat_banned").to_string();
            if let Some(why) = json.get("detail").and_then(|d| d.as_str()) {
                if !why.trim().is_empty() {
                    line.push_str(": ");
                    line.push_str(why);
                }
            }
            return ChatEvent::Failed(line);
        }
        // Anything else: the service explains its refusals in words meant for
        // a person; pass them through rather than inventing our own.
        let msg = json
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("that did not work")
            .to_string();
        return ChatEvent::Failed(msg);
    }

    match cmd {
        // Sending a report answers with an id, not with a state. Reading it as
        // one would flip the window to the consent screen on success.
        ChatCommand::SendDiagnosis { .. } => ChatEvent::DiagnosisSent,
        ChatCommand::Answers => {
            let answers = json
                .get("replies")
                .and_then(|m| m.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| {
                            Some(ChatAnswer {
                                id: m.get("id")?.as_i64()?,
                                at: m.get("at").and_then(|v| v.as_i64()).unwrap_or(0),
                                body: m.get("body")?.as_str()?.to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            ChatEvent::Answers(answers)
        }
        ChatCommand::Poll { .. } => ChatEvent::Messages {
            new: parse_message_list(&json, "messages"),
            edited: parse_message_list(&json, "edited"),
        },
        _ => ChatEvent::State {
            consented: json
                .get("consented")
                .and_then(|v| v.as_bool())
                // After consenting or leaving the service answers with what it
                // did; "left" means no longer in the chat.
                .unwrap_or_else(|| !json.get("left").and_then(|v| v.as_bool()).unwrap_or(false)),
            display_name: json
                .get("display_name")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            consent_text_version: json
                .get("consent_text_version")
                .and_then(|v| v.as_i64())
                .unwrap_or(1),
            // Absent on a service that predates the allowance being told: -1
            // means "unknown", and a window that does not know says nothing
            // rather than guessing a number.
            reports_left: json.get("reports_left").and_then(|v| v.as_i64()).unwrap_or(-1),
        },
    }
}

/// One list of messages out of a poll answer; an absent key is an empty list
/// (an older service does not send `edited` at all).
fn parse_message_list(json: &serde_json::Value, key: &str) -> Vec<ChatMessage> {
    json.get(key)
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    Some(ChatMessage {
                        id: m.get("id")?.as_i64()?,
                        at: m.get("at").and_then(|v| v.as_i64()).unwrap_or(0),
                        name: m.get("name").and_then(|v| v.as_str()).map(str::to_string),
                        body: m.get("body")?.as_str()?.to_string(),
                        reply_name: m
                            .get("reply_name")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        reply_text: m
                            .get("reply_text")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        edited: m.get("edited").and_then(|v| v.as_bool()).unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chat_lives_on_the_relay_host_over_https() {
        assert_eq!(
            endpoint_for_relay("wss://example.org").as_deref(),
            Some("https://example.org/chat")
        );
        assert_eq!(
            endpoint_for_relay("https://example.org/").as_deref(),
            Some("https://example.org/chat")
        );
    }

    /// A relay address with a path or a trailing slash must still yield one
    /// spelling of the endpoint, or the client would talk to two places.
    #[test]
    fn a_path_on_the_relay_address_is_ignored() {
        assert_eq!(
            endpoint_for_relay("wss://example.org/relay/ws").as_deref(),
            Some("https://example.org/chat")
        );
    }

    /// No relay configured is not an error - it is a client that has no chat,
    /// and it must not be turned into one.
    #[test]
    fn no_relay_means_no_endpoint() {
        assert!(endpoint_for_relay("").is_none());
        assert!(endpoint_for_relay("   ").is_none());
        assert!(endpoint_for_relay("wss://").is_none());
    }

    /// The one place a user's own words reach the wire. A quote or a newline in
    /// somebody's message must not be able to break the request around it.
    #[test]
    fn a_message_full_of_punctuation_stays_inside_its_field() {
        let body = format!(r#"{{"body":{}}}"#, json_string("hij zei \"hoi\"\nen ging weg"));
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(parsed["body"], "hij zei \"hoi\"\nen ging weg");
    }

    #[test]
    fn a_name_with_a_backslash_survives() {
        let body = format!(r#"{{"display_name":{}}}"#, json_string(r"PA0\ABC"));
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(parsed["display_name"], r"PA0\ABC");
    }
}
