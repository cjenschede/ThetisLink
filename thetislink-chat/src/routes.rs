// SPDX-License-Identifier: GPL-2.0-or-later
//
//! What each endpoint is allowed to do, and what it is refused for.
//!
//! Design §4 asks for the check to happen per action class rather than once at
//! the door: reading, writing and consenting are different decisions, and a
//! ticket that was fine a minute ago may not be fine now. So every handler here
//! starts from the ticket again.
//!
//! Every refusal carries its reason to the caller as well as to the log. A
//! window that simply does nothing is the fault report nobody can place — §7 of
//! the design says it for the ban, and §8 extends it to the rest.

use std::sync::{Arc, Mutex};

use log::{info, warn};

use crate::guard::Guard;
use crate::http::Request;
use crate::store::{Store, MAX_MESSAGE_CHARS};
use crate::ticket::{self, Ticket, TicketError};

/// Which version of the consent text this build shows. Stored with a consent, so
/// it stays knowable later what somebody actually agreed to (design §6.3).
///
/// 2: the screen names the administrator - who they are and where to reach them
/// for access or removal. Version 1 said everything about what is kept and
/// nothing about who keeps it, which nobody noticed while the only person
/// consenting was the administrator.
///
/// NOTE: nothing re-asks on a version change. The number keeps the record
/// truthful about which text a consent was given under, and that is all it does
/// today. With one consent on record that gap costs nothing - leaving and
/// rejoining re-records it - but it is a gap, and it belongs in a release note
/// before there are consents worth migrating.
///
/// Three since the ban clause was added. That clause is not decoration: a
/// removal also closes the postbox, and leaving the chat does not lift it -
/// two things somebody has to know before agreeing rather than after. Existing
/// consents keep saying 2, which is the truth about the text they were given
/// under; nobody is asked again, per the gap above.
pub const CONSENT_TEXT_VERSION: i64 = 3;

pub struct App {
    pub store: Mutex<Store>,
    pub guard: Mutex<Guard>,
    pub keys: Vec<Vec<u8>>,
    /// Browser sessions for the admin page. Empty until somebody logs in, and
    /// nothing else in the service touches them.
    pub sessions: Mutex<crate::admin_session::Sessions>,
    /// Where the relay's admin API answers, so one administrator has one
    /// password. `None` = no relay login, and the token is the only way in.
    pub relay_admin: Option<String>,
    /// The administrator's own credential, from the environment.
    ///
    /// Deliberately not a station ticket. Reading other people's diagnosis
    /// reports is a different kind of authority from being in the chat, and one
    /// must never be reachable by holding the other.
    pub admin_token: Option<String>,
}

pub struct Reply {
    pub status: &'static str,
    pub body: String,
    /// JSON unless something says otherwise - the admin page is the only thing
    /// that does.
    pub content_type: &'static str,
    /// Room for a Set-Cookie, and nothing else so far.
    pub extra_headers: Vec<String>,
}

fn ok(body: String) -> Reply {
    Reply {
        status: "200 OK",
        body,
        content_type: "application/json",
        extra_headers: Vec::new(),
    }
}

/// A refusal the caller can act on: the reason travels, so a client can say why
/// nothing happened instead of showing a window that ignores you.
fn refuse(status: &'static str, reason: &str) -> Reply {
    Reply {
        status,
        body: format!("{{\"error\":{}}}", json_string(reason)),
        content_type: "application/json",
        extra_headers: Vec::new(),
    }
}

/// A refusal a client is meant to recognise rather than merely read.
///
/// The sentence stays, because an old client shows it and that is better than
/// nothing. The code is what lets a new one say the same thing in the reader's
/// own language - and the ban notice was the one message in the whole service
/// that has to be understood, and the only one that was not translated.
fn refuse_coded(status: &'static str, code: &str, reason: &str, detail: Option<&str>) -> Reply {
    let detail = match detail {
        Some(d) if !d.trim().is_empty() => format!(",\"detail\":{}", json_string(d)),
        _ => String::new(),
    };
    Reply {
        status,
        body: format!(
            "{{\"error\":{},\"code\":{}{}}}",
            json_string(reason),
            json_string(code),
            detail
        ),
        content_type: "application/json",
        extra_headers: Vec::new(),
    }
}

/// Escape a string for JSON by hand. Small enough not to want a serialiser, and
/// this is the one place a message body reaches the wire — a quote or a newline
/// in somebody's text must not be able to break the response around it.
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

/// The ticket, or a reply explaining why not.
///
/// The distinction between the errors is kept all the way into the log, because
/// "a clock is wrong" and "somebody is trying something" need different answers
/// from whoever reads it.
/// What a banned station is told, in the one place all three fronts read it
/// from. A refusal nobody can tell from a network fault is the fault the
/// design warns about: they keep typing, then write to the administrator.
pub const BANNED: &str = "you have been removed from the chat by the administrator";

/// The gate: a valid ticket, and not on the ban list.
///
/// One place, so there is no list of routes to keep in step - and the check is
/// per request rather than per ticket, which is what makes a ticket issued
/// before the ban stop working.
fn admit(app: &App, req: &Request, now: i64) -> Result<Ticket, Reply> {
    let t = verify_ticket(app, req, now)?;
    // The refusal says so in as many words. Somebody who is banned and told
    // nothing keeps typing into a window that has gone quiet and writes to the
    // administrator afterwards; the silence is worse than the sentence
    // (design 7).
    if let Some(ban) = app.store.lock().expect("store lock").ban_of(t.station_id).ok().flatten() {
        // Once an hour per station, not once per poll: a banned client keeps
        // polling every three seconds, and a line each time would bury the log
        // it belongs in.
        app.guard.lock().expect("guard lock").note_refusal(t.station_id, now);
        let shared_reason = if ban.shared { ban.reason.as_deref() } else { None };
        return Err(refuse_coded(
            "403 Forbidden",
            "banned",
            &ban.refusal_text(),
            shared_reason,
        ));
    }
    Ok(t)
}

/// The same ticket check WITHOUT the ban.
///
/// Two routes need this and the reasons are different, so both are named here
/// rather than left to a comment at the call site:
///
/// - `POST /chat/leave`, because the owner's decision is that a ban OUTLIVES a
///   withdrawal, not that it PREVENTS one. Behind the ban check, a banned
///   station could no longer take its name out of its messages or have them
///   deleted - the one thing about their own data they must always be able to
///   do. `/chat/consent` stays behind the gate, so coming back is still shut.
/// - `GET /chat/replies`, because an answer to a problem report was written
///   before any of this and is addressed to them. Shutting it also made the
///   log lie: "answer expired without ever being fetched" would have meant
///   "we would not let them fetch it".
fn admit_even_when_banned(app: &App, req: &Request, now: i64) -> Result<Ticket, Reply> {
    verify_ticket(app, req, now)
}

/// Ticket only: signature and expiry, nothing about who may do what.
fn verify_ticket(app: &App, req: &Request, now: i64) -> Result<Ticket, Reply> {
    let token = req.bearer().ok_or_else(|| {
        refuse("401 Unauthorized", "no ticket offered")
    })?;
    match ticket::verify(token, &app.keys, now as u64) {
        Ok(t) => Ok(t),
        Err(e) => {
            // Not at info: a bad signature is worth noticing, and an expired
            // ticket is worth seeing next to it to tell the two apart.
            warn!("ticket refused: {}", e);
            let status = match e {
                TicketError::Expired => "401 Unauthorized",
                _ => "403 Forbidden",
            };
            Err(refuse(status, &e.to_string()))
        }
    }
}

/// The router, with the one route that reaches out to a neighbour.
///
/// Kept apart from `route` so that everything else stays synchronous and
/// testable without a runtime: exactly one endpoint needs the network, and it is
/// the login.
pub async fn route_async(app: &Arc<App>, req: &Request, now: i64) -> Reply {
    match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/chat/admin/api/login") => return admin_login(app, req).await,
        ("GET", "/chat/admin/api/session") => return admin_session_status(app, req).await,
        _ => {}
    }
    route(app, req, now)
}

pub fn route(app: &Arc<App>, req: &Request, now: i64) -> Reply {
    match (req.method.as_str(), req.path.as_str()) {
        // The only endpoint without a ticket: a client has to be able to find
        // out what it is talking to before it has anything to talk with.
        ("GET", "/chat/version") => ok(format!(
            "{{\"service\":\"thetislink-chat\",\"version\":\"{}\",\"protocol\":{},\"phase\":2,\"consent_text_version\":{},\"max_message_chars\":{}}}",
            env!("CARGO_PKG_VERSION"),
            crate::CHAT_PROTOCOL,
            CONSENT_TEXT_VERSION,
            MAX_MESSAGE_CHARS
        )),

        ("GET", "/chat/state") => state(app, req, now),
        ("POST", "/chat/consent") => consent(app, req, now),
        ("POST", "/chat/leave") => leave(app, req, now),
        ("GET", "/chat/messages") => messages(app, req, now),
        ("POST", "/chat/message") => post_message(app, req, now),
        ("POST", "/chat/message/edit") => edit_message(app, req, now),

        // Diagnosis: one way in for a user, the postbox for the administrator.
        ("POST", "/chat/diagnosis") => submit_diagnosis(app, req, now),
        ("GET", "/chat/replies") => replies(app, req, now),
        // The page, and the session it needs. Everything below still takes a
        // bearer token as well, so the script keeps working unchanged.
        ("GET", "/chat/admin") | ("GET", "/chat/admin/") => admin_page(app),
        // Handled before we get here (see route_async): it is the only route
        // that talks to another service, and the only async one.
        ("POST", "/chat/admin/api/login") => {
            refuse("500 Internal Server Error", "login was not routed correctly")
        }
        ("POST", "/chat/admin/api/logout") => admin_logout(app, req),
        // Handled in route_async: it may have to ask the relay.
        ("GET", "/chat/admin/api/session") => {
            refuse("500 Internal Server Error", "session check was not routed correctly")
        }

        ("GET", "/chat/admin/prune-preview") => admin_prune_preview(app, req, now),
        ("GET", "/chat/admin/housekeeping") => admin_housekeeping(app, req),
        ("GET", "/chat/admin/bans") => admin_bans(app, req),
        ("GET", "/chat/admin/recent") => admin_recent(app, req),
        ("POST", "/chat/admin/ban") => admin_ban(app, req, now),
        ("POST", "/chat/admin/unban") => admin_unban(app, req),
        ("GET", "/chat/admin/diagnoses") => admin_list(app, req, now),
        ("GET", "/chat/admin/diagnosis") => admin_claim(app, req, now),
        ("POST", "/chat/admin/release") => admin_release(app, req, now),
        ("POST", "/chat/admin/reply") => admin_reply(app, req, now),

        // One short answer for anything else, whoever asks. This port is
        // reachable from the internet and has nothing to tell a scanner.
        _ => refuse("404 Not Found", "not found"),
    }
}

/// Has this station agreed, and under which name? The client needs this before
/// it can decide whether to show the chat or the consent screen.
fn state(app: &Arc<App>, req: &Request, now: i64) -> Reply {
    let t = match admit(app, req, now) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let store = app.store.lock().expect("store lock");
    let name = store.consent_of(t.station_id).unwrap_or(None);
    // How many reports this station may still send today. Carried on the state
    // every client already polls, so a form can say "none left" before somebody
    // writes one - being told at the send button, after typing a description
    // and reading a log, is being told too late.
    let left = crate::postbox::remaining_today(&store.conn, t.station_id, now);
    drop(store);
    match name {
        Some(n) => ok(format!(
            "{{\"consented\":true,\"display_name\":{},\"consent_text_version\":{},\"reports_left\":{}}}",
            json_string(&n),
            CONSENT_TEXT_VERSION,
            left
        )),
        None => ok(format!(
            "{{\"consented\":false,\"consent_text_version\":{},\"reports_left\":{}}}",
            CONSENT_TEXT_VERSION,
            left
        )),
    }
}

/// Agreeing. Without this nothing else in the chat works, which is the whole
/// design of §6.1: no consent, no chat, and the rest of ThetisLink untouched.
fn consent(app: &Arc<App>, req: &Request, now: i64) -> Reply {
    let t = match admit(app, req, now) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let v: serde_json::Value = match serde_json::from_str(&req.body) {
        Ok(v) => v,
        Err(_) => return refuse("400 Bad Request", "body is not JSON"),
    };
    let name = v.get("display_name").and_then(|x| x.as_str()).unwrap_or("").trim();
    if name.is_empty() {
        // §6.1: choosing no name is a valid choice, and it means no chat rather
        // than an anonymous one.
        return refuse("400 Bad Request", "a display name is required");
    }
    if name.chars().count() > 32 {
        return refuse("400 Bad Request", "that name is too long");
    }
    // A consent recorded against a text the client has not seen is worthless, so
    // the version has to match rather than be taken on trust.
    let seen = v.get("text_version").and_then(|x| x.as_i64()).unwrap_or(-1);
    if seen != CONSENT_TEXT_VERSION {
        return refuse(
            "409 Conflict",
            "the consent text has changed; please read it again",
        );
    }
    match app
        .store
        .lock()
        .expect("store lock")
        .give_consent(t.station_id, name, CONSENT_TEXT_VERSION, now)
    {
        Ok(()) => ok(format!("{{\"consented\":true,\"display_name\":{}}}", json_string(name))),
        Err(e) => {
            warn!("station {}: consent could not be stored: {}", t.station_id, e);
            refuse("500 Internal Server Error", "could not store that")
        }
    }
}

/// The two buttons of §6.4. Which one ran is in the reply, so the client can say
/// what actually happened rather than guess.
fn leave(app: &Arc<App>, req: &Request, now: i64) -> Reply {
    let t = match admit_even_when_banned(app, req, now) {
        Ok(t) => t,
        Err(r) => return r,
    };
    // Absent means the gentler one. A missing field must never be the more
    // destructive reading.
    let delete = serde_json::from_str::<serde_json::Value>(&req.body)
        .ok()
        .and_then(|v| v.get("delete_messages").and_then(|x| x.as_bool()))
        .unwrap_or(false);

    let store = app.store.lock().expect("store lock");
    // An uncollected report of theirs goes with them. One already collected sits
    // on the administrator's own machine, beyond the reach of anything here -
    // which the consent text says in as many words rather than leaving it to be
    // discovered (design 6.4).
    if let Err(e) = crate::postbox::forget_station(&store.conn, t.station_id) {
        warn!("station {}: could not remove uncollected reports: {}", t.station_id, e);
    }
    let res = if delete {
        store.withdraw_and_delete(t.station_id)
    } else {
        store.withdraw(t.station_id)
    };
    match res {
        Ok(n) => ok(format!(
            "{{\"left\":true,\"messages_deleted\":{},\"messages_anonymised\":{}}}",
            if delete { n } else { 0 },
            if delete { 0 } else { n }
        )),
        Err(e) => {
            warn!("station {}: leaving failed: {}", t.station_id, e);
            refuse("500 Internal Server Error", "could not do that")
        }
    }
}

fn messages(app: &Arc<App>, req: &Request, now: i64) -> Reply {
    let t = match admit(app, req, now) {
        Ok(t) => t,
        Err(r) => return r,
    };
    // Reading is part of the chat, so it needs consent too: somebody who has not
    // agreed has no business reading what others wrote.
    let store = app.store.lock().expect("store lock");
    if store.consent_of(t.station_id).unwrap_or(None).is_none() {
        return refuse("403 Forbidden", "not in the chat");
    }
    let since = req.query_i64("since", 0);
    // Capped so one call cannot ask for the whole store, and so a client that
    // has been away comes back in pages rather than in one lump.
    let limit = req.query_i64("limit", 200).clamp(1, 500);
    match store.since(since, limit) {
        Ok(msgs) => {
            let items: Vec<String> = msgs.iter().map(message_json).collect();
            // Corrections to messages the client already holds ride along on
            // every poll for a while (see `edited_after`): a client asks by id
            // and would otherwise never hear that an old id changed.
            let edited: Vec<String> = store
                .edited_after(now - EDIT_ECHO_SECS, 100)
                .unwrap_or_default()
                .iter()
                .map(message_json)
                .collect();
            ok(format!(
                "{{\"messages\":[{}],\"edited\":[{}]}}",
                items.join(","),
                edited.join(",")
            ))
        }
        Err(e) => {
            warn!("reading messages failed: {}", e);
            refuse("500 Internal Server Error", "could not read those")
        }
    }
}

/// How long a correction keeps riding along on the poll. Longer than the edit
/// window itself, so even a client that was offline for the whole window still
/// sees the final text of anything it may hold.
const EDIT_ECHO_SECS: i64 = 30 * 60;

/// One message as the wire sees it, for both the new and the corrected list.
fn message_json(m: &crate::store::Message) -> String {
    // What is being answered travels with the answer: its author and its first
    // words. A client only ever asks for what is new, so it cannot look up a
    // message it never held.
    format!(
        "{{\"id\":{},\"at\":{},\"name\":{},\"body\":{},\"reply_to\":{},\"reply_name\":{},\"reply_text\":{},\"edited\":{}}}",
        m.id,
        m.at,
        match &m.display_name {
            Some(n) => json_string(n),
            // Somebody who left. The text stays, the person does not (§6.4).
            None => "null".to_string(),
        },
        json_string(&m.body),
        m.reply_to.map(|v| v.to_string()).unwrap_or_else(|| "null".into()),
        match &m.reply_to_name {
            Some(n) => json_string(n),
            None => "null".to_string(),
        },
        match &m.reply_to_excerpt {
            Some(t) => json_string(t),
            None => "null".to_string(),
        },
        m.edited_at.is_some()
    )
}

/// Correct one's own message. Same validations as posting one - a correction
/// is still a message - plus ownership and the window (store::edit).
fn edit_message(app: &Arc<App>, req: &Request, now: i64) -> Reply {
    let t = match admit(app, req, now) {
        Ok(t) => t,
        Err(r) => return r,
    };
    if !t.has_scope("chat:write") {
        return refuse("403 Forbidden", "this ticket may not write");
    }
    let v = match serde_json::from_str::<serde_json::Value>(&req.body) {
        Ok(v) => v,
        Err(_) => return refuse("400 Bad Request", "body is not JSON"),
    };
    let id = v.get("id").and_then(|x| x.as_i64()).unwrap_or(0);
    let body = v.get("body").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    if id <= 0 {
        return refuse("400 Bad Request", "which message?");
    }
    if body.is_empty() {
        return refuse("400 Bad Request", "an empty message is not a message");
    }
    if body.chars().count() > MAX_MESSAGE_CHARS {
        return refuse(
            "413 Payload Too Large",
            "too long for the chat - use 'probleem melden' for a log",
        );
    }
    let store = app.store.lock().expect("store lock");
    if store.consent_of(t.station_id).unwrap_or(None).is_none() {
        return refuse("403 Forbidden", "not in the chat");
    }
    match store.edit(t.station_id, id, &body, now) {
        Ok(Ok(())) => ok("{\"edited\":true}".to_string()),
        Ok(Err(crate::store::EditRefusal::NotYours)) => {
            refuse("403 Forbidden", "not your message")
        }
        Ok(Err(crate::store::EditRefusal::TooOld)) => refuse(
            "409 Conflict",
            "too old to change - others have answered the text as it was",
        ),
        Err(e) => {
            warn!("station {}: edit of message {} failed: {}", t.station_id, id, e);
            refuse("500 Internal Server Error", "could not change that")
        }
    }
}

fn post_message(app: &Arc<App>, req: &Request, now: i64) -> Reply {
    let t = match admit(app, req, now) {
        Ok(t) => t,
        Err(r) => return r,
    };
    if !t.has_scope("chat:write") {
        return refuse("403 Forbidden", "this ticket may not write");
    }

    let body = match serde_json::from_str::<serde_json::Value>(&req.body) {
        Ok(v) => v.get("body").and_then(|x| x.as_str()).unwrap_or("").trim().to_string(),
        Err(_) => return refuse("400 Bad Request", "body is not JSON"),
    };
    if body.is_empty() {
        return refuse("400 Bad Request", "an empty message is not a message");
    }
    if body.chars().count() > MAX_MESSAGE_CHARS {
        // §1.8: this limit exists to keep log fragments out of a channel
        // everybody can read, so the refusal says where they do belong.
        return refuse(
            "413 Payload Too Large",
            "too long for the chat - use 'probleem melden' for a log",
        );
    }

    let store = app.store.lock().expect("store lock");
    let name = match store.consent_of(t.station_id) {
        Ok(Some(n)) => n,
        Ok(None) => return refuse("403 Forbidden", "not in the chat"),
        Err(e) => {
            warn!("station {}: consent lookup failed: {}", t.station_id, e);
            return refuse("500 Internal Server Error", "could not check that");
        }
    };

    // Optional, and validated rather than trusted: a pointer at a message that
    // does not exist would render as an answer to nothing.
    let reply_to = serde_json::from_str::<serde_json::Value>(&req.body)
        .ok()
        .and_then(|v| v.get("reply_to").and_then(|x| x.as_i64()))
        .filter(|id| *id > 0);

    if let Err(r) = app
        .guard
        .lock()
        .expect("guard lock")
        .claim_write(t.station_id, &body, now)
    {
        info!("station {}: message refused - {}", t.station_id, r);
        return refuse("429 Too Many Requests", &r.to_string());
    }

    match store.post(t.station_id, &name, &body, reply_to, now) {
        Ok(id) => ok(format!("{{\"id\":{}}}", id)),
        Err(e) => {
            warn!("station {}: message could not be stored: {}", t.station_id, e);
            refuse("500 Internal Server Error", "could not store that")
        }
    }
}

// ---- diagnosis ---------------------------------------------------------

/// A user sends one report. One way only: the postbox is not a conversation.
fn submit_diagnosis(app: &Arc<App>, req: &Request, now: i64) -> Reply {
    let t = match admit(app, req, now) {
        Ok(t) => t,
        Err(r) => return r,
    };
    if !t.has_scope("chat:write") {
        return refuse("403 Forbidden", "this ticket may not send");
    }
    let body = match serde_json::from_str::<serde_json::Value>(&req.body) {
        Ok(v) => v.get("report").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        Err(_) => return refuse("400 Bad Request", "body is not JSON"),
    };
    if body.trim().is_empty() {
        return refuse("400 Bad Request", "there is nothing in that report");
    }

    // A second net on top of the client's own redaction (design 1.3). Not
    // because the client is untrusted, but because one layer is too few for
    // something that leaves somebody's machine: a private relay host in a report
    // is exactly what must never travel.
    if forbidden_trace(&body) {
        warn!("station {}: diagnosis refused - a private host name was still in it", t.station_id);
        return refuse(
            "422 Unprocessable Entity",
            "that report still contains something it should not - please update the client",
        );
    }

    let store = app.store.lock().expect("store lock");
    // Consent is NOT required here, and that is deliberate (design section 4,
    // which asks only for a valid ticket and the write scope).
    //
    // Joining the chat means agreeing to a name other people see. Sending a
    // report is a private line to the administrator, and the act of typing it
    // and pressing send after reading what goes is the agreement it needs. To
    // demand the first before allowing the second would force a public identity
    // on somebody for a private message - and would leave the server GUI, which
    // has no chat at all, unable to report anything.
    //
    // The name is attached when there is one, because it makes a reply easier to
    // place. Without it the report still carries its station.
    let name = store.consent_of(t.station_id).unwrap_or(None);
    match crate::postbox::submit(&store.conn, t.station_id, name.as_deref(), &body, now) {
        Ok(id) => ok(format!("{{\"id\":{}}}", id)),
        Err(e) => {
            // The reason travels: "I cannot send anything" is one symptom for
            // three different situations (design section 8).
            info!("station {}: diagnosis refused - {}", t.station_id, e);
            let status = match e {
                crate::postbox::PostboxError::TooLarge => "413 Payload Too Large",
                crate::postbox::PostboxError::TooMany => "429 Too Many Requests",
                _ => "503 Service Unavailable",
            };
            refuse(status, &e.to_string())
        }
    }
}

/// Something the server itself knows must never appear in a report, whatever the
/// client did or failed to do.
fn forbidden_trace(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    std::env::var("THETISLINK_FORBIDDEN_HOSTS")
        .unwrap_or_default()
        .split(';')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .any(|h| lower.contains(&h.to_ascii_lowercase()))
}

/// Any answer waiting for this station (design 1.5).
fn replies(app: &Arc<App>, req: &Request, now: i64) -> Reply {
    let t = match admit_even_when_banned(app, req, now) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let store = app.store.lock().expect("store lock");
    match crate::postbox::replies_for(&store.conn, t.station_id, now) {
        Ok(rs) => {
            let items: Vec<String> = rs
                .iter()
                .map(|(id, text, at)| {
                    format!("{{\"id\":{},\"at\":{},\"body\":{}}}", id, at, json_string(text))
                })
                .collect();
            ok(format!("{{\"replies\":[{}]}}", items.join(",")))
        }
        Err(_) => refuse("500 Internal Server Error", "could not read those"),
    }
}

/// The name of the session cookie. Scoped to the admin path, so it is not sent
/// with the ordinary chat requests that have no use for it.
const COOKIE: &str = "tl_chat_admin";

/// The relay's own session cookie. Read only to hand it straight back to the
/// relay - this service cannot validate it and never tries.
const RELAY_COOKIE: &str = "tl_session";

/// The administrator, by a credential of their own.
///
/// Two ways in, for two callers: a bearer token for the collect script, and a
/// session cookie for the page. The cookie additionally has to bring the CSRF
/// token on anything that changes something - a bearer token does not, because
/// a browser never attaches one by itself, which is the whole of what CSRF is
/// about.
fn admin(app: &Arc<App>, req: &Request) -> Result<(), Reply> {
    let want = app
        .admin_token
        .as_deref()
        .ok_or_else(|| refuse("404 Not Found", "not found"))?;

    if let Some(got) = req.bearer() {
        return if crate::admin_session::secret_eq(got, want) {
            Ok(())
        } else {
            // A wrong token and an unknown path look the same from outside: this
            // port faces the internet and has nothing to confirm to a scanner.
            Err(refuse("404 Not Found", "not found"))
        };
    }

    let token = req.cookie(COOKIE).ok_or_else(|| refuse("404 Not Found", "not found"))?;
    let csrf = app
        .sessions
        .lock()
        .expect("sessions lock")
        .validate(token, std::time::Instant::now())
        .ok_or_else(|| refuse("401 Unauthorized", "your session has expired"))?;

    // Reading is safe to do on a session alone; changing something is not.
    if req.method != "GET" {
        let sent = req.headers.get("x-csrf").map(String::as_str).unwrap_or("");
        if !crate::admin_session::secret_eq(sent, &csrf) {
            warn!("admin request refused - CSRF token missing or wrong");
            return Err(refuse("403 Forbidden", "this request was not made by the page"));
        }
    }
    Ok(())
}

fn admin_page(app: &Arc<App>) -> Reply {
    // No page at all where there is no administrator configured, so a service
    // without one does not advertise a login that cannot succeed.
    if app.admin_token.is_none() {
        return refuse("404 Not Found", "not found");
    }
    // A fresh nonce per response. It is the whole reason the policy can be strict
    // about scripts while the page still carries its own.
    let nonce = crate::admin_session::gen_token();
    Reply {
        status: "200 OK",
        body: crate::admin_page::page(&nonce),
        content_type: "text/html; charset=utf-8",
        extra_headers: crate::admin_page::security_headers(&nonce),
    }
}

/// Trade a password for a session.
///
/// Two things are accepted, in this order, and the order is the design:
///
/// 1. **the relay's administrator password.** One person runs both services, so
///    there is one thing to remember. The relay is asked; it is not copied.
/// 2. **this service's own token.** Still the way a script gets in, and the way
///    a person gets in when the relay is not answering - refusing the postbox
///    because the neighbour is down would be the coupling this avoids.
async fn admin_login(app: &Arc<App>, req: &Request) -> Reply {
    if app.admin_token.is_none() {
        return refuse("404 Not Found", "not found");
    }
    let ip = client_ip(req);
    let now = std::time::Instant::now();

    // Two counters, and the second one is not decoration.
    //
    // A wrong token falls through to the relay for checking. While the relay is
    // unreachable that check cannot happen, so the attempt was answered 503 and
    // counted against nothing - which left the token guessable without limit for
    // exactly as long as the neighbour was down. Attempts that could not be
    // verified now have a counter of their own.
    //
    // Separate rather than shared, so an outage cannot lock out the way in that
    // still works: somebody holding the right token never reaches the relay at
    // all, and is never touched by this.
    let unverified = format!("{ip}|unverified");

    // What was offered, and is it the token? Worked out FIRST, because the two
    // counters below apply to different people and one of them must not apply to
    // this one.
    let given = serde_json::from_str::<serde_json::Value>(&req.body)
        .ok()
        .and_then(|v| v.get("password").and_then(|x| x.as_str()).map(str::to_string))
        .unwrap_or_default();
    let own_token = app.admin_token.clone().unwrap_or_default();
    let by_token = crate::admin_session::secret_eq(&given, &own_token);

    // The lock is taken, checked and dropped before anything is awaited: holding
    // a std Mutex across an await is how a service stops answering.
    {
        let mut sessions = app.sessions.lock().expect("sessions lock");
        // The wrong-password counter always applies.
        //
        // The unverifiable counter does NOT apply to a correct token, and that
        // is the whole point of it being a second counter. Checking it before
        // knowing what was offered undid exactly the guarantee it was added to
        // protect: enough guessing from one address during an outage, and the
        // administrator holding the right token was locked out of the only way
        // in that still worked.
        let blocked = sessions.is_blocked(&ip, now)
            || (!by_token && sessions.is_blocked(&unverified, now));
        if blocked {
            warn!("admin login rate-limited for {ip}");
            return refuse("429 Too Many Requests", "too many attempts - try again later");
        }
    }
    let accepted = if by_token {
        true
    } else {
        let Some(addr) = app.relay_admin.clone() else {
            // No relay to ask: the token is the only credential, which is what
            // this service did before and still does when told nothing else.
            let mut sessions = app.sessions.lock().expect("sessions lock");
            sessions.record_fail(&ip, now);
            warn!("admin login failed for {ip}");
            return refuse("401 Unauthorized", "that is not the administrator password");
        };
        match crate::relay_login::verify(&addr, &given, &ip).await {
            Ok(ok) => ok,
            Err(why) => {
                // Still not counted as a WRONG password - it says nothing about
                // the password, and counting it there would lock somebody out
                // over a neighbour being down. But it is counted as an attempt
                // that could not be checked, which is what stops the token being
                // guessed freely during an outage.
                app.sessions
                    .lock()
                    .expect("sessions lock")
                    .record_fail(&unverified, now);
                warn!("admin login could not be checked with the relay: {why}");
                return refuse(
                    "503 Service Unavailable",
                    &format!("could not check that with the relay ({why}) - the postbox token still works"),
                );
            }
        }
    };

    let mut sessions = app.sessions.lock().expect("sessions lock");
    if !accepted {
        sessions.record_fail(&ip, now);
        warn!("admin login failed for {ip}");
        return refuse("401 Unauthorized", "that is not the administrator password");
    }
    sessions.reset_fails(&ip);
    sessions.reset_fails(&unverified);
    let (token, csrf) = sessions.open(
        crate::admin_session::gen_token(),
        crate::admin_session::gen_token(),
        now,
    );
    info!("admin login OK for {ip}");
    Reply {
        status: "200 OK",
        body: format!("{{\"csrf\":{}}}", json_string(&csrf)),
        content_type: "application/json",
        extra_headers: vec![format!(
            "Set-Cookie: {COOKIE}={token}; HttpOnly; Secure; SameSite=Strict; Path=/chat/admin; Max-Age={}",
            crate::admin_session::SESSION_IDLE_TTL.as_secs()
        )],
    }
}

fn admin_logout(app: &Arc<App>, req: &Request) -> Reply {
    if let Some(token) = req.cookie(COOKIE) {
        app.sessions.lock().expect("sessions lock").close(token);
    }
    Reply {
        status: "200 OK",
        body: "{}".to_string(),
        content_type: "application/json",
        extra_headers: vec![format!(
            "Set-Cookie: {COOKIE}=; HttpOnly; Secure; SameSite=Strict; Path=/chat/admin; Max-Age=0"
        )],
    }
}

/// Is there still a session, and what is its CSRF token?
///
/// The cookie is HttpOnly, so the page cannot see it and cannot know on its own
/// whether a previous visit is still good for anything.
async fn admin_session_status(app: &Arc<App>, req: &Request) -> Reply {
    // Our own session first, when there is one.
    if let Some(token) = req.cookie(COOKIE) {
        if let Some(csrf) = app
            .sessions
            .lock()
            .expect("sessions lock")
            .validate(token, std::time::Instant::now())
        {
            return ok(format!("{{\"csrf\":{}}}", json_string(&csrf)));
        }
    }

    // Otherwise: is the visitor already logged in to the relay? One person runs
    // both, so arriving from the relay's own page should not ask again. The
    // relay's cookie is offered back to the relay and it decides - this service
    // cannot read it and does not keep it.
    let (Some(addr), Some(relay_cookie)) = (app.relay_admin.clone(), req.cookie(RELAY_COOKIE))
    else {
        return refuse("401 Unauthorized", "not logged in");
    };
    let ip = client_ip(req);
    let pair = format!("{RELAY_COOKIE}={relay_cookie}");
    match crate::relay_login::verify_session(&addr, &pair, &ip).await {
        Ok(true) => {}
        // Not logged in there either, or the relay cannot say. Either way the
        // page shows its login, which still works on the password or the token.
        Ok(false) => return refuse("401 Unauthorized", "not logged in"),
        Err(why) => {
            warn!("could not check the relay session: {why}");
            return refuse("401 Unauthorized", "not logged in");
        }
    }

    let now = std::time::Instant::now();
    let (token, csrf) = app.sessions.lock().expect("sessions lock").open(
        crate::admin_session::gen_token(),
        crate::admin_session::gen_token(),
        now,
    );
    info!("admin session accepted from the relay's own login for {ip}");
    Reply {
        status: "200 OK",
        body: format!("{{\"csrf\":{}}}", json_string(&csrf)),
        content_type: "application/json",
        extra_headers: vec![format!(
            "Set-Cookie: {COOKIE}={token}; HttpOnly; Secure; SameSite=Strict; Path=/chat/admin; Max-Age={}",
            crate::admin_session::SESSION_IDLE_TTL.as_secs()
        )],
    }
}

/// The caller's address, as far as it can be known.
///
/// Caddy sits in front and adds X-Forwarded-For; without it every attempt would
/// count against the proxy and one wrong guess would lock out the world.
fn client_ip(req: &Request) -> String {
    req.headers
        .get("x-forwarded-for")
        .and_then(|v| v.split(',').next())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Who is on the ban list.
fn admin_bans(app: &Arc<App>, req: &Request) -> Reply {
    if let Err(r) = admin(app, req) {
        return r;
    }
    let store = app.store.lock().expect("store lock");
    match store.bans() {
        Ok(bans) => {
            let items: Vec<String> = bans
                .iter()
                .map(|b| {
                    format!(
                        "{{\"station_id\":{},\"at\":{},\"shared\":{},\"name\":{},\"reason\":{}}}",
                        b.station_id,
                        b.at,
                        b.shared,
                        match &b.display_name {
                            Some(n) => json_string(n),
                            None => "null".to_string(),
                        },
                        match &b.reason {
                            Some(r) => json_string(r),
                            None => "null".to_string(),
                        },
                    )
                })
                .collect();
            ok(format!("{{\"bans\":[{}]}}", items.join(",")))
        }
        Err(_) => refuse("500 Internal Server Error", "could not read those"),
    }
}

/// The last stretch of conversation, so a ban can be decided where it is
/// earned. The design asks for a button on the message itself: reading back
/// what was said is the moment you want to be able to act, and a list of bare
/// station numbers elsewhere is not that moment.
fn admin_recent(app: &Arc<App>, req: &Request) -> Reply {
    if let Err(r) = admin(app, req) {
        return r;
    }
    let store = app.store.lock().expect("store lock");
    match store.recent_with_stations(60) {
        Ok(rows) => {
            let items: Vec<String> = rows
                .iter()
                .map(|(m, station)| {
                    format!(
                        "{{\"id\":{},\"at\":{},\"name\":{},\"body\":{},\"station_id\":{}}}",
                        m.id,
                        m.at,
                        match &m.display_name {
                            Some(n) => json_string(n),
                            None => "null".to_string(),
                        },
                        json_string(&m.body),
                        match station {
                            Some(s) => s.to_string(),
                            None => "null".to_string(),
                        },
                    )
                })
                .collect();
            ok(format!("{{\"messages\":[{}]}}", items.join(",")))
        }
        Err(_) => refuse("500 Internal Server Error", "could not read those"),
    }
}

/// Put a station on the list.
///
/// The messages stay: a ban is an order-keeping measure and not a withdrawal
/// of consent, and coupling the two would make it irreversible (design 7).
/// Removing somebody's messages as well is a separate act.
fn admin_ban(app: &Arc<App>, req: &Request, now: i64) -> Reply {
    if let Err(r) = admin(app, req) {
        return r;
    }
    let body = serde_json::from_str::<serde_json::Value>(&req.body).ok();
    let Some(station_id) = body.as_ref().and_then(|v| v.get("station_id")).and_then(|x| x.as_i64())
    else {
        return refuse("400 Bad Request", "which station?");
    };
    let reason = body
        .as_ref()
        .and_then(|v| v.get("reason"))
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    // Whether that note travels with the refusal. Off unless the page says so:
    // a note written to remember why is not automatically a message to the
    // person it is about.
    let shared = body
        .as_ref()
        .and_then(|v| v.get("shared"))
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let store = app.store.lock().expect("store lock");
    match store.ban(station_id, reason.as_deref(), shared, now) {
        Ok(()) => ok("{}".to_string()),
        Err(_) => refuse("500 Internal Server Error", "could not write that"),
    }
}

/// And off it again.
fn admin_unban(app: &Arc<App>, req: &Request) -> Reply {
    if let Err(r) = admin(app, req) {
        return r;
    }
    let Some(station_id) = serde_json::from_str::<serde_json::Value>(&req.body)
        .ok()
        .and_then(|v| v.get("station_id").and_then(|x| x.as_i64()))
    else {
        return refuse("400 Bad Request", "which station?");
    };
    let store = app.store.lock().expect("store lock");
    match store.unban(station_id) {
        Ok(true) => ok("{}".to_string()),
        Ok(false) => refuse("404 Not Found", "that station is not on the list"),
        Err(_) => refuse("500 Internal Server Error", "could not write that"),
    }
}

/// The recent housekeeping rounds, newest first.
///
/// The rounds are written down as they happen precisely so this page can show
/// them: a round that removed nothing is the normal case on a service younger
/// than its shortest retention period, and "nothing happened" has to be
/// distinguishable from "nothing ran".
fn admin_housekeeping(app: &Arc<App>, req: &Request) -> Reply {
    if let Err(r) = admin(app, req) {
        return r;
    }
    let store = app.store.lock().expect("store lock");
    match store.housekeeping_runs(48) {
        Ok(runs) => {
            let items: Vec<String> = runs
                .iter()
                .map(|r| {
                    format!(
                        "{{\"at\":{},\"messages\":{},\"reports\":{},\"replies\":{},\"markers\":{},\"ok\":{}}}",
                        r.at, r.messages, r.reports, r.replies, r.markers, r.ok
                    )
                })
                .collect();
            ok(format!("{{\"runs\":[{}]}}", items.join(",")))
        }
        Err(_) => refuse("500 Internal Server Error", "could not read those"),
    }
}

/// What housekeeping would remove, at a date of the caller's choosing.
///
/// The shortest of these periods is thirty days, so on a young service there
/// is nothing to watch and nothing to wait for. This answers the question
/// without waiting and without deleting: `?at=` is a unix time, and the
/// counts come from running the real expiry inside a transaction that is then
/// rolled back. Left out, it answers for right now.
fn admin_prune_preview(app: &Arc<App>, req: &Request, now: i64) -> Reply {
    if let Err(r) = admin(app, req) {
        return r;
    }
    let at = req.query_i64("at", now);
    let store = app.store.lock().expect("store lock");
    let messages = store.preview(at);
    let postbox = crate::postbox::preview(&store.conn, at);
    match (messages, postbox) {
        (Ok((by_age, by_size)), Ok(c)) => ok(format!(
            "{{\"at\":{},\"messages_by_age\":{},\"messages_by_size\":{},\"reports\":{},\"delivered_replies\":{},\"undelivered_replies\":{},\"markers\":{}}}",
            at, by_age, by_size, c.reports, c.delivered_replies, c.undelivered_replies, c.markers
        )),
        _ => refuse("500 Internal Server Error", "could not work that out"),
    }
}

fn admin_list(app: &Arc<App>, req: &Request, now: i64) -> Reply {
    if let Err(r) = admin(app, req) {
        return r;
    }
    let store = app.store.lock().expect("store lock");
    match crate::postbox::list(&store.conn, now) {
        Ok(rs) => {
            let items: Vec<String> = rs
                .iter()
                .map(|r| {
                    format!(
                        "{{\"id\":{},\"at\":{},\"name\":{},\"bytes\":{},\"claimed\":{},\"replied\":{},\"reply\":{},\"reply_at\":{},\"collected\":{}}}",
                        r.id,
                        r.at,
                        match &r.display_name {
                            Some(n) => json_string(n),
                            None => "null".to_string(),
                        },
                        r.bytes,
                        r.claimed_until.is_some(),
                        r.replied,
                        match &r.reply {
                            Some(b) => json_string(b),
                            None => "null".to_string(),
                        },
                        match r.reply_at {
                            Some(t) => t.to_string(),
                            None => "null".to_string(),
                        },
                        match r.collected_at {
                            Some(t) => t.to_string(),
                            None => "null".to_string(),
                        }
                    )
                })
                .collect();
            ok(format!("{{\"reports\":[{}]}}", items.join(",")))
        }
        Err(_) => refuse("500 Internal Server Error", "could not read those"),
    }
}

/// Claim and read in one step - they are one intent, and splitting them would
/// let a second administrator read what the first is already handling.
fn admin_claim(app: &Arc<App>, req: &Request, now: i64) -> Reply {
    if let Err(r) = admin(app, req) {
        return r;
    }
    let id = req.query_i64("id", 0);
    let store = app.store.lock().expect("store lock");
    match crate::postbox::claim(&store.conn, id, now) {
        Ok(body) => ok(format!("{{\"id\":{},\"report\":{}}}", id, json_string(&body))),
        Err(e) => refuse("409 Conflict", &e.to_string()),
    }
}

/// Say what happened to it. The only thing that removes a report, which is what
/// makes a failed download harmless (design 1.4.1).
fn admin_release(app: &Arc<App>, req: &Request, now: i64) -> Reply {
    if let Err(r) = admin(app, req) {
        return r;
    }
    let id = serde_json::from_str::<serde_json::Value>(&req.body)
        .ok()
        .and_then(|v| v.get("id").and_then(|x| x.as_i64()))
        .unwrap_or(0);
    let store = app.store.lock().expect("store lock");
    match crate::postbox::release(&store.conn, id, now) {
        Ok(()) => ok("{\"released\":true}".to_string()),
        Err(e) => refuse("409 Conflict", &e.to_string()),
    }
}

fn admin_reply(app: &Arc<App>, req: &Request, now: i64) -> Reply {
    if let Err(r) = admin(app, req) {
        return r;
    }
    let v = match serde_json::from_str::<serde_json::Value>(&req.body) {
        Ok(v) => v,
        Err(_) => return refuse("400 Bad Request", "body is not JSON"),
    };
    let id = v.get("id").and_then(|x| x.as_i64()).unwrap_or(0);
    let text = v.get("body").and_then(|x| x.as_str()).unwrap_or("").trim();
    if text.is_empty() {
        return refuse("400 Bad Request", "an empty reply is not a reply");
    }
    if text.chars().count() > MAX_MESSAGE_CHARS {
        return refuse("413 Payload Too Large", "too long for a reply");
    }
    let store = app.store.lock().expect("store lock");
    match crate::postbox::reply(&store.conn, id, text, now) {
        Ok(station) => ok(format!("{{\"replied_to\":{}}}", station)),
        Err(e) => refuse("409 Conflict", &e.to_string()),
    }
}

#[cfg(test)]
pub(crate) mod tests_support {
    pub use super::tests::*;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    pub(crate) const KEY: &[u8] = b"test-key";
    pub(crate) const NOW: i64 = 1_700_000_000;

    pub(crate) fn app() -> Arc<App> {
        Arc::new(App {
            store: Mutex::new(Store::open(":memory:").unwrap()),
            guard: Mutex::new(Guard::default()),
            keys: vec![KEY.to_vec()],
            sessions: Mutex::new(crate::admin_session::Sessions::default()),
            // No relay in the tests unless one is deliberately started.
            relay_admin: None,
            admin_token: Some("test-admin".to_string()),
        })
    }

    pub(crate) fn token(station: i64, jti: &str, scope: &str) -> String {
        ticket::sign(
            format!(
                r#"{{"sid":{},"name":"PA0ABC","iat":{},"exp":{},"jti":"{}","scope":"{}"}}"#,
                station,
                NOW,
                NOW + 600,
                jti,
                scope
            )
            .as_bytes(),
            KEY,
        )
    }

    pub(crate) fn req(method: &str, path: &str, token: Option<&str>, body: &str) -> Request {
        let mut headers = HashMap::new();
        if let Some(t) = token {
            headers.insert("authorization".to_string(), format!("Bearer {t}"));
        }
        let (p, query) = match path.split_once('?') {
            None => (path.to_string(), HashMap::new()),
            Some((p, q)) => {
                let mut m = HashMap::new();
                for pair in q.split('&') {
                    if let Some((k, v)) = pair.split_once('=') {
                        m.insert(k.to_string(), v.to_string());
                    }
                }
                (p.to_string(), m)
            }
        };
        Request {
            method: method.to_string(),
            path: p,
            query,
            headers,
            body: body.to_string(),
        }
    }

    pub(crate) fn agree(app: &Arc<App>, station: i64, jti: &str) {
        let r = route(
            app,
            &req(
                "POST",
                "/chat/consent",
                Some(&token(station, jti, "chat:read chat:write")),
                &format!(r#"{{"display_name":"PA0ABC","text_version":{}}}"#, CONSENT_TEXT_VERSION),
            ),
            NOW,
        );
        assert_eq!(r.status, "200 OK", "{}", r.body);
    }

    pub(crate) fn agree_as(app: &Arc<App>, station: i64, jti: &str, name: &str) {
        let r = route(
            app,
            &req(
                "POST",
                "/chat/consent",
                Some(&token(station, jti, "chat:read chat:write")),
                &format!(r#"{{"display_name":"{}","text_version":{}}}"#, name, CONSENT_TEXT_VERSION),
            ),
            NOW,
        );
        assert_eq!(r.status, "200 OK", "{}", r.body);
    }

    #[test]
    fn the_version_endpoint_needs_no_ticket() {
        let r = route(&app(), &req("GET", "/chat/version", None, ""), NOW);
        assert_eq!(r.status, "200 OK");
        assert!(r.body.contains("thetislink-chat"));
    }

    /// Everything else does. This is the door.
    #[test]
    fn everything_else_needs_a_ticket() {
        let a = app();
        for (m, p) in [
            ("GET", "/chat/state"),
            ("GET", "/chat/messages"),
            ("POST", "/chat/message"),
            ("POST", "/chat/consent"),
            ("POST", "/chat/leave"),
        ] {
            let r = route(&a, &req(m, p, None, "{}"), NOW);
            assert_eq!(r.status, "401 Unauthorized", "{m} {p}");
        }
    }

    #[test]
    fn a_forged_ticket_is_refused() {
        let a = app();
        let forged = ticket::sign(br#"{"sid":7,"name":"x","iat":0,"exp":9999999999,"jti":"j","scope":"chat:write"}"#, b"wrong-key");
        let r = route(&a, &req("GET", "/chat/state", Some(&forged), ""), NOW);
        assert_eq!(r.status, "403 Forbidden");
    }

    /// §6.1: no consent, no chat. Not a read either - somebody who has not
    /// agreed has no business reading what others wrote.
    #[test]
    fn without_consent_there_is_no_reading_and_no_writing() {
        let a = app();
        let t = token(7, "j1", "chat:read chat:write");
        assert_eq!(route(&a, &req("GET", "/chat/messages", Some(&t), ""), NOW).status, "403 Forbidden");
        let r = route(&a, &req("POST", "/chat/message", Some(&token(7, "j2", "chat:write")), r#"{"body":"hoi"}"#), NOW);
        assert_eq!(r.status, "403 Forbidden");
    }

    #[test]
    fn consenting_then_writing_then_reading_works() {
        let a = app();
        agree(&a, 7, "c1");
        let r = route(&a, &req("POST", "/chat/message", Some(&token(7, "m1", "chat:write")), r#"{"body":"hallo allemaal"}"#), NOW);
        assert_eq!(r.status, "200 OK", "{}", r.body);
        let r = route(&a, &req("GET", "/chat/messages?since=0", Some(&token(7, "r1", "chat:read")), ""), NOW);
        assert!(r.body.contains("hallo allemaal"), "{}", r.body);
        assert!(r.body.contains("PA0ABC"));
    }

    /// A consent against a text the client never saw is worthless.
    #[test]
    fn consent_for_another_text_version_is_refused() {
        let a = app();
        let r = route(
            &a,
            &req("POST", "/chat/consent", Some(&token(7, "c1", "chat:write")), r#"{"display_name":"X","text_version":99}"#),
            NOW,
        );
        assert_eq!(r.status, "409 Conflict");
    }

    /// A typo is the author's to fix, within the window; the correction rides
    /// the poll with its marker so every reader ends on the final text.
    #[test]
    fn editing_your_own_message_works_and_travels_marked() {
        let a = app();
        agree(&a, 7, "c1");
        let r = route(&a, &req("POST", "/chat/message", Some(&token(7, "m1", "chat:write")), r#"{"body":"hallo alemaal"}"#), NOW);
        assert_eq!(r.status, "200 OK", "{}", r.body);
        let r = route(
            &a,
            &req("POST", "/chat/message/edit", Some(&token(7, "e1", "chat:write")), r#"{"id":1,"body":"hallo allemaal"}"#),
            NOW + 60,
        );
        assert_eq!(r.status, "200 OK", "{}", r.body);
        let r = route(&a, &req("GET", "/chat/messages?since=0", Some(&token(7, "r1", "chat:read")), ""), NOW + 61);
        assert!(r.body.contains("hallo allemaal"), "{}", r.body);
        assert!(!r.body.contains("hallo alemaal"), "{}", r.body);
        assert!(r.body.contains("\"edited\":true"), "{}", r.body);
        // And the correction is in the separate edited list, for clients that
        // already held id 1.
        assert!(r.body.contains("\"edited\":["), "{}", r.body);
    }

    /// Somebody else's message is somebody else's, whatever the ticket.
    #[test]
    fn editing_another_stations_message_is_refused() {
        let a = app();
        agree(&a, 7, "c1");
        agree_as(&a, 8, "c2", "PD9XYZ");
        let r = route(&a, &req("POST", "/chat/message", Some(&token(7, "m1", "chat:write")), r#"{"body":"van mij"}"#), NOW);
        assert_eq!(r.status, "200 OK", "{}", r.body);
        let r = route(
            &a,
            &req("POST", "/chat/message/edit", Some(&token(8, "e1", "chat:write")), r#"{"id":1,"body":"van mij toch"}"#),
            NOW + 10,
        );
        assert_eq!(r.status, "403 Forbidden");
    }

    /// After the window the text belongs to the conversation that answered it.
    #[test]
    fn editing_after_the_window_is_refused() {
        let a = app();
        agree(&a, 7, "c1");
        // Planted straight into the store: a message this old cannot be posted
        // through the route, because the ticket that would carry it has the
        // same fifteen-minute lifetime as the edit window itself.
        a.store
            .lock()
            .unwrap()
            .post(7, "PA0ABC", "tekst", None, NOW - crate::store::EDIT_WINDOW_SECS - 1)
            .unwrap();
        let r = route(
            &a,
            &req("POST", "/chat/message/edit", Some(&token(7, "e1", "chat:write")), r#"{"id":1,"body":"andere tekst"}"#),
            NOW,
        );
        assert_eq!(r.status, "409 Conflict");
    }

    /// A ticket without the write scope may read and nothing more.
    #[test]
    fn a_read_only_ticket_cannot_write() {
        let a = app();
        agree(&a, 7, "c1");
        let r = route(&a, &req("POST", "/chat/message", Some(&token(7, "m1", "chat:read")), r#"{"body":"hoi"}"#), NOW);
        assert_eq!(r.status, "403 Forbidden");
    }

    /// The limit that keeps log fragments out of a channel everybody reads, with
    /// a refusal that says where they do belong.
    #[test]
    fn an_overlong_message_is_refused_and_points_somewhere_else() {
        let a = app();
        agree(&a, 7, "c1");
        let long = "x".repeat(MAX_MESSAGE_CHARS + 1);
        let r = route(&a, &req("POST", "/chat/message", Some(&token(7, "m1", "chat:write")), &format!(r#"{{"body":"{long}"}}"#)), NOW);
        assert_eq!(r.status, "413 Payload Too Large");
        assert!(r.body.contains("probleem melden"), "{}", r.body);
    }

    /// §6.4, end to end: the text survives, the person does not.
    #[test]
    fn leaving_the_chat_anonymises_and_the_default_is_the_gentler_one() {
        let a = app();
        agree(&a, 7, "c1");
        route(&a, &req("POST", "/chat/message", Some(&token(7, "m1", "chat:write")), r#"{"body":"ik heb dit ook"}"#), NOW);

        // No field at all: a missing value must never mean the destructive one.
        let r = route(&a, &req("POST", "/chat/leave", Some(&token(7, "l1", "chat:write")), "{}"), NOW);
        assert_eq!(r.status, "200 OK");
        assert!(r.body.contains("\"messages_anonymised\":1"), "{}", r.body);

        agree(&a, 8, "c2");
        let r = route(&a, &req("GET", "/chat/messages?since=0", Some(&token(8, "r1", "chat:read")), ""), NOW);
        assert!(r.body.contains("ik heb dit ook"), "the text stays");
        assert!(r.body.contains("\"name\":null"), "the person does not: {}", r.body);
    }

    #[test]
    fn the_second_button_removes_the_messages() {
        let a = app();
        agree(&a, 7, "c1");
        route(&a, &req("POST", "/chat/message", Some(&token(7, "m1", "chat:write")), r#"{"body":"hier PA0ABC"}"#), NOW);
        let r = route(&a, &req("POST", "/chat/leave", Some(&token(7, "l1", "chat:write")), r#"{"delete_messages":true}"#), NOW);
        assert!(r.body.contains("\"messages_deleted\":1"), "{}", r.body);

        agree(&a, 8, "c2");
        let r = route(&a, &req("GET", "/chat/messages?since=0", Some(&token(8, "r1", "chat:read")), ""), NOW);
        assert!(!r.body.contains("PA0ABC"), "nothing of it should be left: {}", r.body);
    }

    /// A quote or a newline in somebody's text must not break the response
    /// around it - this is the one place a message reaches the wire.
    #[test]
    fn a_message_full_of_punctuation_survives_the_round_trip() {
        let a = app();
        agree(&a, 7, "c1");
        let nasty = r#"hij zei \"hoi\" en toen een regel"#;
        let r = route(&a, &req("POST", "/chat/message", Some(&token(7, "m1", "chat:write")), &format!(r#"{{"body":"{nasty}"}}"#)), NOW);
        assert_eq!(r.status, "200 OK", "{}", r.body);
        let r = route(&a, &req("GET", "/chat/messages?since=0", Some(&token(7, "r1", "chat:read")), ""), NOW);
        // The reply must still be valid JSON, which is the actual claim here.
        let parsed: serde_json::Value = serde_json::from_str(&r.body).expect("valid JSON out");
        assert_eq!(parsed["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn writing_too_fast_is_refused_with_a_reason() {
        let a = app();
        agree(&a, 7, "c1");
        // Different words each time: the same text twice in a row is a double
        // click, which is a separate refusal (guard::Refusal::Duplicate).
        for i in 0..crate::guard::MAX_MESSAGES_PER_MINUTE {
            let body = format!(r#"{{"body":"bericht {i}"}}"#);
            let r = route(&a, &req("POST", "/chat/message", Some(&token(7, &format!("m{i}"), "chat:write")), &body), NOW);
            assert_eq!(r.status, "200 OK", "at {i}: {}", r.body);
        }
        let r = route(&a, &req("POST", "/chat/message", Some(&token(7, "one-more", "chat:write")), r#"{"body":"nog een"}"#), NOW);
        assert_eq!(r.status, "429 Too Many Requests");
        assert!(r.body.contains("minute"), "the reason travels: {}", r.body);
    }

    /// The bug this endpoint shipped with for one evening: a ticket lasts fifteen
    /// minutes and is refreshed every seven and a half, and spending it on the
    /// first write let a station say exactly one thing per refresh. A
    /// conversation is many messages on one ticket.
    #[test]
    fn one_ticket_carries_a_whole_conversation() {
        let a = app();
        agree(&a, 7, "c1");
        let t = token(7, "same-ticket", "chat:write");
        for line in ["hallo allemaal", "hoe gaat het", "tot ziens"] {
            let body = format!(r#"{{"body":"{line}"}}"#);
            let r = route(&a, &req("POST", "/chat/message", Some(&t), &body), NOW);
            assert_eq!(r.status, "200 OK", "{line}: {}", r.body);
        }
    }

    /// An unknown path says the same short thing to everyone.
    #[test]
    fn an_unknown_path_says_nothing_useful() {
        let r = route(&app(), &req("GET", "/chat/../etc/passwd", None, ""), NOW);
        assert_eq!(r.status, "404 Not Found");
    }
}

#[cfg(test)]
mod diagnosis_tests {
    use super::tests_support::*;
    use super::*;

    /// A report does not require chat consent (design section 4).
    ///
    /// Joining the chat means agreeing to a name others see; sending a report is
    /// a private line to the administrator. Demanding the first for the second
    /// would force a public identity on somebody for a private message - and
    /// would leave the server GUI, which has no chat at all, unable to report.
    #[test]
    fn a_report_goes_in_without_joining_the_chat() {
        let a = app();
        let t = token(7, "d1", "chat:write");
        let r = route(&a, &req("POST", "/chat/diagnosis", Some(&t), r#"{"report":"logregel"}"#), NOW);
        assert_eq!(r.status, "200 OK", "{}", r.body);

        // Nameless, but not traceless: the station is on it either way, so a
        // reply still has somewhere to go.
        let rs = crate::postbox::list(&a.store.lock().unwrap().conn, NOW).unwrap();
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].display_name, None);
    }

    /// In the chat, the chosen name rides along - it makes a reply easier to
    /// place than a bare number.
    #[test]
    fn a_report_from_a_member_carries_their_name() {
        let a = app();
        agree(&a, 7, "c1");
        let t = token(7, "d1", "chat:write");
        let r = route(&a, &req("POST", "/chat/diagnosis", Some(&t), r#"{"report":"logregel"}"#), NOW);
        assert_eq!(r.status, "200 OK", "{}", r.body);
        let rs = crate::postbox::list(&a.store.lock().unwrap().conn, NOW).unwrap();
        assert!(rs[0].display_name.is_some(), "{:?}", rs[0].display_name);
    }

    /// A request with a cookie but no CSRF token is what a hostile page on
    /// another site can produce; one with the token is not. So reading is
    /// allowed on the session alone and changing is not.
    #[test]
    fn a_session_alone_may_read_but_may_not_change_anything() {
        let a = app();
        let (cookie, csrf) = login(&a);

        let mut r = req("GET", "/chat/admin/diagnoses", None, "");
        r.headers.insert("cookie".into(), cookie.clone());
        assert_eq!(route(&a, &r, NOW).status, "200 OK", "reading is fine");

        let mut r = req("POST", "/chat/admin/release", None, r#"{"id":1}"#);
        r.headers.insert("cookie".into(), cookie.clone());
        assert_eq!(route(&a, &r, NOW).status, "403 Forbidden", "without the token");

        let mut r = req("POST", "/chat/admin/release", None, r#"{"id":1}"#);
        r.headers.insert("cookie".into(), cookie);
        r.headers.insert("x-csrf".into(), csrf);
        assert_ne!(route(&a, &r, NOW).status, "403 Forbidden", "with it");
    }

    /// The script's way in must keep working, and without a CSRF token: a
    /// browser never attaches a bearer header by itself, which is the whole of
    /// what CSRF is about.
    #[test]
    fn a_bearer_token_still_works_and_needs_no_csrf() {
        let a = app();
        let r = req("POST", "/chat/admin/release", Some("test-admin"), r#"{"id":1}"#);
        assert_ne!(route(&a, &r, NOW).status, "403 Forbidden");
        assert_ne!(route(&a, &r, NOW).status, "404 Not Found");
    }

    /// A wrong token is refused and counted; a scanner gets nothing back that
    /// distinguishes a real path from an imaginary one.
    #[test]
    fn a_wrong_token_gets_nowhere() {
        let a = app();
        let r = req("POST", "/chat/admin/api/login", None, r#"{"password":"nope"}"#);
        assert_eq!(go(&a, &r).status, "401 Unauthorized");

        let r = req("GET", "/chat/admin/diagnoses", Some("nope"), "");
        assert_eq!(route(&a, &r, NOW).status, "404 Not Found");

        let r = req("GET", "/chat/admin/diagnoses", None, "");
        assert_eq!(route(&a, &r, NOW).status, "404 Not Found", "and no cookie either");
    }

    /// Six wrong guesses and the address waits. Counted per address, so one
    /// clumsy visitor cannot lock out another.
    #[test]
    fn repeated_wrong_guesses_are_made_to_wait() {
        let a = app();
        let mut last = String::new();
        for _ in 0..6 {
            let mut r = req("POST", "/chat/admin/api/login", None, r#"{"password":"nope"}"#);
            r.headers.insert("x-forwarded-for".into(), "203.0.113.9".into());
            last = go(&a, &r).status.to_string();
        }
        assert_eq!(last, "429 Too Many Requests");

        let mut r = req("POST", "/chat/admin/api/login", None, r#"{"password":"test-admin"}"#);
        r.headers.insert("x-forwarded-for".into(), "198.51.100.7".into());
        assert_eq!(go(&a, &r).status, "200 OK", "a different address is unaffected");
    }

    /// Logging out ends it, rather than merely telling the page to forget.
    #[test]
    fn logging_out_ends_the_session_on_the_server() {
        let a = app();
        let (cookie, _) = login(&a);
        let mut r = req("POST", "/chat/admin/api/logout", None, "");
        r.headers.insert("cookie".into(), cookie.clone());
        assert_eq!(route(&a, &r, NOW).status, "200 OK");

        // 401 and not 404: only a successful login can produce a cookie, so
        // saying "expired" to whoever holds one tells a scanner nothing it did
        // not already have, and lets the page show its login again instead of
        // looking broken.
        let mut r = req("GET", "/chat/admin/diagnoses", None, "");
        r.headers.insert("cookie".into(), cookie);
        assert_eq!(route(&a, &r, NOW).status, "401 Unauthorized");
    }

    /// Drive one request the way the service does, login included.
    ///
    /// A runtime per call is wasteful and irrelevant here: these are unit tests,
    /// and the alternative is a second code path that is not the one that runs.
    pub(crate) fn go(a: &Arc<App>, r: &Request) -> Reply {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(route_async(a, r, NOW))
    }

    /// Log in and return (cookie header value, CSRF token).
    fn login(a: &Arc<App>) -> (String, String) {
        let r = req("POST", "/chat/admin/api/login", None, r#"{"password":"test-admin"}"#);
        let reply = go(a, &r);
        assert_eq!(reply.status, "200 OK", "{}", reply.body);
        let set = reply.extra_headers.first().expect("a cookie was set").clone();
        assert!(set.contains("HttpOnly"), "{set}");
        assert!(set.contains("Secure"), "{set}");
        assert!(set.contains("SameSite=Strict"), "{set}");
        let cookie = set
            .trim_start_matches("Set-Cookie: ")
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let csrf = reply
            .body
            .split("\"csrf\":\"")
            .nth(1)
            .and_then(|t| t.split('"').next())
            .expect("a csrf token")
            .to_string();
        (cookie, csrf)
    }

    /// One administrator, one password: what the relay accepts, the postbox
    /// accepts. The relay here is a socket that answers a fixed status, which is
    /// all this side can observe of it anyway.
    #[test]
    fn the_relays_password_opens_the_postbox() {
        let a = app();
        let addr = fake_relay("HTTP/1.1 200 OK
Content-Length: 0

");
        Arc::get_mut(&mut { a.clone() });
        let a = app_with_relay(addr);
        let r = req("POST", "/chat/admin/api/login", None, r#"{"password":"whatever"}"#);
        assert_eq!(go(&a, &r).status, "200 OK", "the relay said yes");
    }

    #[test]
    fn a_password_the_relay_rejects_is_rejected_here() {
        let a = app_with_relay(fake_relay("HTTP/1.1 401 Unauthorized
Content-Length: 0

"));
        let r = req("POST", "/chat/admin/api/login", None, r#"{"password":"whatever"}"#);
        assert_eq!(go(&a, &r).status, "401 Unauthorized");
    }

    /// A relay that is down must not read as a wrong password: that sends the
    /// administrator looking in the wrong place and counts against them. It says
    /// so instead, and mentions the token that still works.
    #[test]
    fn a_relay_that_is_down_says_so_rather_than_blaming_the_password() {
        // Nothing is listening on this port; connecting fails at once.
        let a = app_with_relay("127.0.0.1:1".to_string());
        let r = req("POST", "/chat/admin/api/login", None, r#"{"password":"whatever"}"#);
        let reply = go(&a, &r);
        assert_eq!(reply.status, "503 Service Unavailable", "{}", reply.body);
        assert!(reply.body.contains("token"), "and points at the way in: {}", reply.body);
    }

    /// Found in review: while the relay is down, a wrong token could be tried
    /// for ever, because an unverifiable attempt was answered 503 and counted
    /// against nothing.
    #[test]
    fn guessing_is_still_limited_while_the_relay_is_down() {
        let a = app_with_relay("127.0.0.1:1".to_string());
        let mut last = String::new();
        for _ in 0..6 {
            let mut r = req("POST", "/chat/admin/api/login", None, r#"{"password":"guess"}"#);
            r.headers.insert("x-forwarded-for".into(), "203.0.113.44".into());
            last = go(&a, &r).status.to_string();
        }
        assert_eq!(last, "429 Too Many Requests", "the guessing is stopped");
    }

    /// Found in the re-check: the guard was consulted before it was known what
    /// had been offered, so the counter meant to stop guessing also shut out the
    /// correct token - from the SAME address, which is the case that matters and
    /// which the earlier test missed by using a second address.
    #[test]
    fn the_token_still_works_from_an_address_that_has_been_guessing() {
        let a = app_with_relay("127.0.0.1:1".to_string());
        for _ in 0..6 {
            let mut r = req("POST", "/chat/admin/api/login", None, r#"{"password":"guess"}"#);
            r.headers.insert("x-forwarded-for".into(), "203.0.113.77".into());
            go(&a, &r);
        }
        let mut r = req("POST", "/chat/admin/api/login", None, r#"{"password":"test-admin"}"#);
        r.headers.insert("x-forwarded-for".into(), "203.0.113.77".into());
        let reply = go(&a, &r);
        assert_eq!(reply.status, "200 OK", "same address, right token: {}", reply.body);
    }

    /// And the one who holds the token is never caught by that: they never
    /// reach the relay, so an outage cannot lock them out of the way in that
    /// still works.
    #[test]
    fn the_token_holder_is_untouched_by_that_limit() {
        let a = app_with_relay("127.0.0.1:1".to_string());
        for _ in 0..6 {
            let mut r = req("POST", "/chat/admin/api/login", None, r#"{"password":"guess"}"#);
            r.headers.insert("x-forwarded-for".into(), "198.51.100.9".into());
            go(&a, &r);
        }
        // Blocked for guessing - and yet the token still opens it, from a
        // different address, which is the case that matters.
        let mut r = req("POST", "/chat/admin/api/login", None, r#"{"password":"test-admin"}"#);
        r.headers.insert("x-forwarded-for".into(), "198.51.100.10".into());
        assert_eq!(go(&a, &r).status, "200 OK");
    }

    /// Which is exactly why the token still works with the relay unreachable.
    #[test]
    fn the_token_still_gets_in_when_the_relay_does_not_answer() {
        let a = app_with_relay("127.0.0.1:1".to_string());
        let r = req("POST", "/chat/admin/api/login", None, r#"{"password":"test-admin"}"#);
        assert_eq!(go(&a, &r).status, "200 OK");
    }

    /// Arriving from the relay's own page, already logged in there: no second
    /// login. This is the whole of the single sign-on, and it is one request.
    #[test]
    fn a_live_relay_session_is_enough_to_get_in() {
        let body = "{\"authenticated\":true,\"csrf\":\"x\"}";
        let resp: &'static str = Box::leak(
            format!("HTTP/1.1 200 OK
Content-Length: {}

{body}", body.len())
                .into_boxed_str(),
        );
        let a = app_with_relay(fake_relay(resp));
        let mut r = req("GET", "/chat/admin/api/session", None, "");
        r.headers.insert("cookie".into(), "tl_session=whatever".into());
        let reply = go(&a, &r);
        assert_eq!(reply.status, "200 OK", "{}", reply.body);
        assert!(
            reply.extra_headers.iter().any(|h| h.contains("tl_chat_admin=")),
            "and a session of our own is opened: {:?}",
            reply.extra_headers
        );
    }

    /// Logged out over there means logged out here. The relay decides; this
    /// service only asks.
    #[test]
    fn a_relay_session_that_is_not_live_gets_nothing() {
        let body = "{\"authenticated\":false}";
        let resp: &'static str = Box::leak(
            format!("HTTP/1.1 200 OK
Content-Length: {}

{body}", body.len())
                .into_boxed_str(),
        );
        let a = app_with_relay(fake_relay(resp));
        let mut r = req("GET", "/chat/admin/api/session", None, "");
        r.headers.insert("cookie".into(), "tl_session=whatever".into());
        assert_eq!(go(&a, &r).status, "401 Unauthorized");
    }

    /// A socket that answers one canned response and stops. Returns its address.
    fn fake_relay(response: &'static str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        let addr = listener.local_addr().expect("an address").to_string();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
                let _ = s.write_all(response.as_bytes());
            }
        });
        addr
    }

    /// The same App the other tests use, with a relay to ask.
    fn app_with_relay(addr: String) -> Arc<App> {
        Arc::new(App {
            store: Mutex::new(Store::open(":memory:").unwrap()),
            guard: Mutex::new(Guard::default()),
            keys: vec![KEY.to_vec()],
            sessions: Mutex::new(crate::admin_session::Sessions::default()),
            relay_admin: Some(addr),
            admin_token: Some("test-admin".to_string()),
        })
    }

    /// The administrator side is a separate authority. Holding a perfectly good
    /// station ticket must not open it - and it must not even admit to being
    /// there, since this port faces the internet.
    #[test]
    fn a_station_ticket_does_not_open_the_postbox() {
        let a = app();
        agree(&a, 7, "c1");
        let t = token(7, "d1", "chat:write");
        for (m, p) in [
            ("GET", "/chat/admin/diagnoses"),
            ("GET", "/chat/admin/diagnosis?id=1"),
            ("POST", "/chat/admin/release"),
            ("POST", "/chat/admin/reply"),
        ] {
            let r = route(&a, &req(m, p, Some(&t), r#"{"id":1,"body":"x"}"#), NOW);
            assert_eq!(r.status, "404 Not Found", "{m} {p}");
            assert!(r.body.contains("not found"), "and says nothing else: {}", r.body);
        }
    }

    #[test]
    fn the_administrator_can_read_it_and_only_then_remove_it() {
        let a = app();
        agree(&a, 7, "c1");
        route(&a, &req("POST", "/chat/diagnosis", Some(&token(7, "d1", "chat:write")), r#"{"report":"de log"}"#), NOW);

        let adm = "test-admin";
        let r = route(&a, &req("GET", "/chat/admin/diagnoses", Some(adm), ""), NOW);
        assert!(r.body.contains("\"claimed\":false"), "{}", r.body);

        // Releasing without claiming must not remove anything: that is what
        // makes a failed download harmless.
        let r = route(&a, &req("POST", "/chat/admin/release", Some(adm), r#"{"id":1}"#), NOW);
        assert_eq!(r.status, "409 Conflict", "{}", r.body);

        let r = route(&a, &req("GET", "/chat/admin/diagnosis?id=1", Some(adm), ""), NOW);
        assert!(r.body.contains("de log"), "{}", r.body);

        let r = route(&a, &req("POST", "/chat/admin/release", Some(adm), r#"{"id":1}"#), NOW);
        assert_eq!(r.status, "200 OK", "{}", r.body);
        let r = route(&a, &req("GET", "/chat/admin/diagnoses", Some(adm), ""), NOW);
        assert!(!r.body.contains("de log"), "gone from the postbox: {}", r.body);
    }

    #[test]
    fn a_reply_reaches_the_sender_and_nobody_else() {
        let a = app();
        agree(&a, 7, "c1");
        agree(&a, 8, "c2");
        route(&a, &req("POST", "/chat/diagnosis", Some(&token(7, "d1", "chat:write")), r#"{"report":"de log"}"#), NOW);
        let r = route(&a, &req("POST", "/chat/admin/reply", Some("test-admin"), r#"{"id":1,"body":"kijk eens naar je audio"}"#), NOW);
        assert_eq!(r.status, "200 OK", "{}", r.body);

        let mine = route(&a, &req("GET", "/chat/replies", Some(&token(7, "r1", "chat:read")), ""), NOW);
        assert!(mine.body.contains("kijk eens naar je audio"), "{}", mine.body);
        let theirs = route(&a, &req("GET", "/chat/replies", Some(&token(8, "r2", "chat:read")), ""), NOW);
        assert!(!theirs.body.contains("kijk eens"), "not somebody else's: {}", theirs.body);
    }

    /// Leaving the chat takes an uncollected report with it (design 6.4).
    #[test]
    fn leaving_the_chat_removes_an_uncollected_report() {
        let a = app();
        agree(&a, 7, "c1");
        route(&a, &req("POST", "/chat/diagnosis", Some(&token(7, "d1", "chat:write")), r#"{"report":"de log"}"#), NOW);
        route(&a, &req("POST", "/chat/leave", Some(&token(7, "l1", "chat:write")), "{}"), NOW);
        let r = route(&a, &req("GET", "/chat/admin/diagnoses", Some("test-admin"), ""), NOW);
        assert!(!r.body.contains("de log"), "{}", r.body);
    }

    #[test]
    fn an_empty_report_is_not_a_report() {
        let a = app();
        agree(&a, 7, "c1");
        let r = route(&a, &req("POST", "/chat/diagnosis", Some(&token(7, "d1", "chat:write")), r#"{"report":"   "}"#), NOW);
        assert_eq!(r.status, "400 Bad Request");
    }

    /// The requirement the review put hardest (A5): a ticket that was valid
    /// before the ban must stop working, and for reading as much as for
    /// writing. The ticket here is issued first and used after.
    #[test]
    fn a_ticket_from_before_the_ban_opens_nothing_after_it() {
        let app = app();
        let t = token(7, "j1", "chat");
        // It works to begin with.
        let before = route(&app, &req("GET", "/chat/state", Some(&t), ""), NOW);
        assert_eq!(before.status, "200 OK", "{}", before.body);

        app.store.lock().unwrap().ban(7, Some("shouting"), false, NOW).unwrap();

        // The same ticket, unexpired, now opens nothing at all.
        for (method, path) in [
            ("GET", "/chat/state"),
            ("GET", "/chat/messages"),
        ] {
            let r = route(&app, &req(method, path, Some(&t), ""), NOW);
            assert_eq!(r.status, "403 Forbidden", "{path} -> {}", r.body);
            assert!(r.body.contains("removed from the chat"), "{path} -> {}", r.body);
        }
    }

    /// And it says which of the two it is. A refusal nobody can tell from a
    /// network fault leaves somebody typing into a quiet window.
    #[test]
    fn the_refusal_says_what_happened() {
        let app = app();
        app.store.lock().unwrap().ban(7, None, false, NOW).unwrap();
        let r = route(&app, &req("GET", "/chat/state", Some(&token(7, "j2", "chat")), ""), NOW);
        assert!(r.body.contains(BANNED), "{}", r.body);
    }

    /// One station on the list is one station off the service - and nobody
    /// else.
    #[test]
    fn a_ban_reaches_that_station_and_no_other() {
        let app = app();
        app.store.lock().unwrap().ban(7, None, false, NOW).unwrap();
        let mine = route(&app, &req("GET", "/chat/state", Some(&token(7, "a", "chat")), ""), NOW);
        let theirs = route(&app, &req("GET", "/chat/state", Some(&token(8, "b", "chat")), ""), NOW);
        assert_eq!(mine.status, "403 Forbidden");
        assert_eq!(theirs.status, "200 OK", "{}", theirs.body);
    }

    /// The owner's decision: a ban closes the whole service, the postbox with
    /// it. Reporting a problem is not a way back in.
    #[test]
    fn a_banned_station_cannot_reach_the_postbox_either() {
        let app = app();
        app.store.lock().unwrap().ban(7, None, false, NOW).unwrap();
        let r = route(
            &app,
            &req("POST", "/chat/diagnosis", Some(&token(7, "c", "diagnose")), r#"{"report":"x"}"#),
            NOW,
        );
        assert_eq!(r.status, "403 Forbidden", "{}", r.body);
        // The reason matters: a refusal over a scope or a malformed body would
        // pass a status-only check and prove nothing about the ban.
        assert!(r.body.contains(BANNED), "{}", r.body);
    }

    /// A ban is a measure, not a verdict: it comes off again, and the station
    /// is simply back.
    #[test]
    fn lifting_a_ban_lets_them_straight_back_in() {
        let app = app();
        app.store.lock().unwrap().ban(7, None, false, NOW).unwrap();
        assert!(app.store.lock().unwrap().unban(7).unwrap());
        let r = route(&app, &req("GET", "/chat/state", Some(&token(7, "d", "chat")), ""), NOW);
        assert_eq!(r.status, "200 OK", "{}", r.body);
    }

    /// The messages stay. Coupling a ban to removing what somebody wrote makes
    /// it irreversible, and it is not (design 7).
    #[test]
    fn a_ban_leaves_what_was_said_where_it_was() {
        let app = app();
        {
            let store = app.store.lock().unwrap();
            store.give_consent(7, "PA0ABC", 1, NOW).unwrap();
            store.post(7, "PA0ABC", "hello", None, NOW).unwrap();
            store.ban(7, None, false, NOW).unwrap();
        }
        let left = app.store.lock().unwrap().since(0, 100).unwrap();
        assert_eq!(left.len(), 1, "the conversation is not a punishment");
    }

    /// The decision that needed saying out loud: withdrawing consent does not
    /// take the ban with it, or the ban would be liftable by the one it is
    /// against - withdraw, agree again, back in.
    #[test]
    fn leaving_the_chat_does_not_lift_a_ban() {
        let app = app();
        {
            let store = app.store.lock().unwrap();
            store.give_consent(7, "PA0ABC", 1, NOW).unwrap();
            store.ban(7, None, false, NOW).unwrap();
            store.withdraw(7).unwrap();
            assert!(store.is_banned(7).unwrap(), "the ban survives the withdrawal");
        }
        let r = route(&app, &req("GET", "/chat/state", Some(&token(7, "e", "chat")), ""), NOW);
        assert_eq!(r.status, "403 Forbidden", "{}", r.body);
    }

    /// And they lose their name on the list, because the consent row is what
    /// held it. The number stays - that is the price of the decision above.
    #[test]
    fn the_list_keeps_the_station_and_loses_the_name() {
        let app = app();
        let store = app.store.lock().unwrap();
        store.give_consent(7, "PA0ABC", 1, NOW).unwrap();
        store.ban(7, None, false, NOW).unwrap();
        assert_eq!(store.bans().unwrap()[0].display_name.as_deref(), Some("PA0ABC"));
        store.withdraw(7).unwrap();
        assert_eq!(store.bans().unwrap()[0].display_name, None);
        assert_eq!(store.bans().unwrap()[0].station_id, 7);
    }

    /// C1. The owner's decision is that a ban OUTLIVES a withdrawal, not that
    /// it PREVENTS one. Behind the gate, a banned station could no longer take
    /// its name out of its own messages - the one thing about their own data
    /// nobody may be shut out of.
    ///
    /// Through `route()` on purpose. The test that was here called
    /// `store.withdraw()` directly, so it proved the ban survives a withdrawal
    /// and never touched the question of whether a withdrawal is possible.
    #[test]
    fn a_banned_station_can_still_leave_the_chat() {
        let app = app();
        {
            let store = app.store.lock().unwrap();
            store.give_consent(7, "PA0ABC", CONSENT_TEXT_VERSION, NOW).unwrap();
            store.post(7, "PA0ABC", "hello", None, NOW).unwrap();
            store.ban(7, Some("shouting"), false, NOW).unwrap();
        }
        let r = route(
            &app,
            &req("POST", "/chat/leave", Some(&token(7, "l1", "chat")), r#"{"delete_messages":false}"#),
            NOW,
        );
        assert_eq!(r.status, "200 OK", "{}", r.body);

        let store = app.store.lock().unwrap();
        assert_eq!(store.consent_of(7).unwrap(), None, "the consent record is gone");
        assert!(store.is_banned(7).unwrap(), "and the ban is not");
    }

    /// And coming back is still shut, which is what makes the exception safe.
    #[test]
    fn leaving_does_not_open_the_door_again() {
        let app = app();
        app.store.lock().unwrap().ban(7, None, false, NOW).unwrap();
        let body = format!(r#"{{"display_name":"PA0ABC","text_version":{CONSENT_TEXT_VERSION}}}"#);
        let r = route(&app, &req("POST", "/chat/consent", Some(&token(7, "l2", "chat")), &body), NOW);
        assert_eq!(r.status, "403 Forbidden", "{}", r.body);
    }

    /// C2. An answer to a problem report was written before any of this and is
    /// addressed to them. Shutting it also made the log lie: "expired without
    /// ever being fetched" would have meant "we would not let them fetch it".
    #[test]
    fn a_banned_station_can_still_collect_an_answer() {
        let app = app();
        app.store.lock().unwrap().ban(7, None, false, NOW).unwrap();
        let r = route(&app, &req("GET", "/chat/replies", Some(&token(7, "r1", "chat")), ""), NOW);
        assert_eq!(r.status, "200 OK", "{}", r.body);
    }

    /// Everything else stays shut, so the two exceptions are exceptions.
    #[test]
    fn the_exceptions_are_only_those_two() {
        let app = app();
        app.store.lock().unwrap().ban(7, None, false, NOW).unwrap();
        for (method, path, body) in [
            ("GET", "/chat/state", ""),
            ("GET", "/chat/messages", ""),
            ("POST", "/chat/message", r#"{"body":"hoi"}"#),
            ("POST", "/chat/diagnosis", r#"{"report":"x"}"#),
        ] {
            let r = route(&app, &req(method, path, Some(&token(7, "x", "chat")), body), NOW);
            assert_eq!(r.status, "403 Forbidden", "{path} -> {}", r.body);
        }
    }

    /// C4. The note is the administrator's own unless they say otherwise, and
    /// the refusal carries it only then.
    #[test]
    fn the_reason_travels_only_when_it_is_shared() {
        let app = app();
        app.store.lock().unwrap().ban(7, Some("shouting"), false, NOW).unwrap();
        let quiet = route(&app, &req("GET", "/chat/state", Some(&token(7, "s1", "chat")), ""), NOW);
        assert!(!quiet.body.contains("shouting"), "a private note stayed private: {}", quiet.body);

        app.store.lock().unwrap().ban(7, Some("shouting"), true, NOW).unwrap();
        let told = route(&app, &req("GET", "/chat/state", Some(&token(7, "s2", "chat")), ""), NOW);
        assert!(told.body.contains("shouting"), "{}", told.body);
        assert!(told.body.contains("\"code\":\"banned\""), "and it is recognisable: {}", told.body);
    }

    /// A second ban without a note keeps the first note. The history of why is
    /// the only thing here that cannot be reconstructed afterwards.
    #[test]
    fn banning_again_without_a_reason_keeps_the_old_one() {
        let app = app();
        let store = app.store.lock().unwrap();
        store.ban(7, Some("shouting"), false, NOW).unwrap();
        store.ban(7, None, false, NOW + 60).unwrap();
        assert_eq!(store.ban_of(7).unwrap().unwrap().reason.as_deref(), Some("shouting"));
    }
}
