// SPDX-License-Identifier: GPL-2.0-or-later
//
//! The postbox, as a page.
//!
//! One file, no assets, no build step: the container is 256 MB next to a relay
//! carrying audio, and a page that reads a handful of reports does not need a
//! toolchain. Everything is inline, which also means there is nothing for a
//! browser to fetch from anywhere else.
//!
//! It shows the list, opens one report, answers it and throws it away — the same
//! four things `scripts/postbox.sh` does, because they are the same endpoints.
//! The page merely holds a session cookie instead of a bearer token.

/// The page, with a one-off nonce stamped into its two inline blocks.
///
/// A nonce rather than `unsafe-inline`, because this page displays somebody
/// else's log. That text is put on screen with `textContent` and never as
/// markup, so a script in a report cannot run - but a report is the one thing
/// here that comes from outside, and one layer is thin for that. With a nonce
/// the policy refuses any script the page did not itself carry, whatever a
/// future edit does with innerHTML.
pub fn page(nonce: &str) -> String {
    PAGE.replace("{NONCE}", nonce)
}

/// The policy that goes with it, and the rest of the headers a page like this
/// should carry.
///
/// `default-src 'none'`: this page fetches nothing but its own endpoints, loads
/// no fonts, no images and no styles from anywhere. Everything it needs is in
/// the one file.
pub fn security_headers(nonce: &str) -> Vec<String> {
    vec![
        [
            "Content-Security-Policy: default-src 'none';",
            &format!("script-src 'nonce-{nonce}';"),
            &format!("style-src 'nonce-{nonce}';"),
            "connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        ]
        .join(" "),
        // A report is text and must be treated as text, whatever a browser
        // thinks it recognises in it.
        "X-Content-Type-Options: nosniff".to_string(),
        "Referrer-Policy: no-referrer".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A string literal must not run past the end of its line.
    ///
    /// This page is one long constant, and a constant is exactly what gets
    /// edited by a script. One such edit lost a layer of backslashes and left
    /// real newlines inside `"..."`, which is a syntax error - and a syntax
    /// error anywhere in this script means NONE of it runs: every button on the
    /// page goes dead at once, including the login, with nothing reaching the
    /// server to explain why. It looked like a broken login for half an hour.
    ///
    /// The style here has no multi-line strings and no template literals, so
    /// "an odd number of quotes on a line" is a sound check rather than a
    /// guess. If that ever stops being true, this test is the place to say so.
    #[test]
    fn no_string_literal_runs_past_the_end_of_its_line() {
        let script = PAGE
            .split_once("<script nonce=\"{NONCE}\">")
            .and_then(|(_, rest)| rest.split_once("</script>"))
            .map(|(js, _)| js)
            .expect("the page has a script block");

        for (n, line) in script.lines().enumerate() {
            let mut quotes = 0;
            let mut escaped = false;
            for c in line.chars() {
                match c {
                    '\\' if !escaped => escaped = true,
                    '"' if !escaped => quotes += 1,
                    _ => escaped = false,
                }
            }
            assert_eq!(
                quotes % 2,
                0,
                "line {} of the page script leaves a string open: {line}",
                n + 1
            );
        }
    }

    /// The policy and the page have to agree, or the page is blank: a nonce in
    /// the header that is not on the blocks refuses the page's own script.
    #[test]
    fn the_nonce_reaches_both_inline_blocks_and_the_policy() {
        let html = page("abc123");
        assert!(html.contains("<style nonce=\"abc123\">"), "style block");
        assert!(html.contains("<script nonce=\"abc123\">"), "script block");
        assert!(!html.contains("{NONCE}"), "no placeholder is left behind");

        let headers = security_headers("abc123").join(" ");
        assert!(headers.contains("script-src 'nonce-abc123'"), "{headers}");
        assert!(headers.contains("style-src 'nonce-abc123'"), "{headers}");
        assert!(headers.contains("nosniff"), "{headers}");
    }

    /// The handlers are what make the page more than a picture. If an edit ever
    /// drops one, the button it belongs to silently does nothing.
    #[test]
    fn every_button_has_something_wired_to_it() {
        for id in [
            "go", "refresh", "back", "sendreply", "download", "remove", "logout", "fname",
            "keeprefresh", "keepask", "keepdate", "modrefresh",
        ] {
            assert!(
                PAGE.contains(&format!("id=\"{id}\"")),
                "the page has no element with id {id}"
            );
            assert!(
                PAGE.contains(&format!("$(\"{id}\")")),
                "nothing in the script touches {id}"
            );
        }
    }
}

const PAGE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>ThetisLink postbox</title>
<style nonce="{NONCE}">
  :root { color-scheme: dark light; }
  body { margin: 0; font: 15px/1.5 system-ui, sans-serif;
         background: #14181d; color: #dde3ea; }
  header { padding: 14px 18px; border-bottom: 1px solid #2a3138;
           display: flex; align-items: baseline; gap: 12px; }
  h1 { font-size: 17px; margin: 0; font-weight: 600; }
  .sub { color: #8b97a3; font-size: 13px; }
  main { padding: 18px; max-width: 1000px; }
  table { border-collapse: collapse; width: 100%; }
  th, td { text-align: left; padding: 7px 10px; border-bottom: 1px solid #232a31; }
  th { color: #8b97a3; font-weight: 500; font-size: 13px; }
  tr.r:hover { background: #1b2127; cursor: pointer; }
  .tag { font-size: 12px; padding: 1px 7px; border-radius: 9px; background: #2a3138; color: #9fb0c0; }
  .tag.waiting { background: #2d3b2c; color: #a7d3a0; }
  button { font: inherit; padding: 6px 13px; border-radius: 6px; border: 1px solid #39434d;
           background: #222a31; color: #dde3ea; cursor: pointer; }
  button:hover { background: #2b343d; }
  button.danger { border-color: #6b3a3a; color: #e8b4b4; }
  button:disabled { opacity: .5; cursor: default; }
  input, textarea { font: inherit; background: #1b2127; color: #dde3ea;
                    border: 1px solid #39434d; border-radius: 6px; padding: 7px 9px; }
  pre { background: #0f1317; border: 1px solid #232a31; border-radius: 8px;
        padding: 12px; overflow-x: auto; white-space: pre-wrap; word-break: break-word;
        max-height: 60vh; overflow-y: auto; font-size: 13px; }
  .row { display: flex; gap: 10px; align-items: center; flex-wrap: wrap; margin: 12px 0; }
  .msg { color: #e8c07d; min-height: 1.4em; }
  .hide { display: none; }
  a.back { color: #8fb8e0; cursor: pointer; }
  /* The way across to the relay, in the relay's own accent.
     It was a tinted word next to real buttons and read as decoration, while
     the relay's link here is a filled button - so the way over was obvious in
     one direction and nearly invisible in the other. Same colour as the relay
     uses (--accent there is #2d6cb5), because the two pages are one
     administration and the crossing should look the same from both sides. */
  a.cross { font: inherit; padding: 6px 13px; border-radius: 6px; border: 0;
            background: #2d6cb5; color: #fff; text-decoration: none;
            display: inline-block; line-height: normal; cursor: pointer; }
  a.cross:hover { background: #3a7cc9; }
</style>
</head>
<body>
<header>
  <h1>ThetisLink postbox</h1>
  <span class="sub" id="sub"></span>
  <span style="flex:1"></span>
  <!-- The relay is a separate service with its own login; a link is the whole
       of the coupling, which is the point. -->
  <a class="cross" href="/admin"
     title="Relay administration (separate service, same password)">Relay</a>
  <button id="logout" class="hide">Log out</button>
</header>
<main>

<section id="login">
  <p class="sub">The same password as the relay administration. The postbox token
     works too, and is the way in when the relay is not answering.</p>
  <p class="sub">The postbox holds problem reports until they are collected.
     Collecting one removes it from the server.</p>
  <div class="row">
    <input id="pw" type="password" size="44" placeholder="relay administrator password" autofocus>
    <button id="go">Log in</button>
  </div>
  <p class="msg" id="loginmsg"></p>
</section>

<section id="list" class="hide">
  <div class="row">
    <button id="refresh">Refresh</button>
    <span class="sub" id="count"></span>
  </div>
  <table><thead><tr>
    <th>id</th><th>received</th><th>from</th><th>size</th><th>state</th>
  </tr></thead><tbody id="rows"></tbody></table>
  <p class="msg" id="listmsg"></p>
</section>

<section id="keep" class="hide">
  <div class="row">
    <button id="keeprefresh">Refresh</button>
    <span class="sub" id="keephead"></span>
  </div>
  <table><thead><tr>
    <th>round</th><th>messages</th><th>reports</th><th>answers</th><th>markers</th>
  </tr></thead><tbody id="keeprows"></tbody></table>
  <div class="row">
    <span class="sub">What would go if housekeeping ran on</span>
    <input id="keepdate" size="12" placeholder="YYYY-MM-DD">
    <button id="keepask">Work it out</button>
    <span class="sub">Nothing is removed - this only counts.</span>
  </div>
  <pre id="keepwhatif"></pre>
  <p class="msg" id="keepmsg"></p>
</section>

<section id="mod" class="hide">
  <div class="row">
    <button id="modrefresh">Refresh</button>
    <span class="sub" id="modhead"></span>
  </div>
  <table><thead><tr>
    <th>when</th><th>from</th><th>said</th><th></th>
  </tr></thead><tbody id="modrows"></tbody></table>
  <div class="row"><span class="sub">On the ban list - a ban stops the whole service for that station, and it survives them leaving the chat.</span></div>
  <table><thead><tr>
    <th>station</th><th>name</th><th>since</th><th>why</th><th></th>
  </tr></thead><tbody id="banrows"></tbody></table>
  <p class="msg" id="modmsg"></p>
</section>

<section id="one" class="hide">
  <div class="row">
    <a class="back" id="back">&larr; back to the list</a>
    <span class="sub" id="onehead"></span>
  </div>
  <pre id="body"></pre>
  <pre id="sentreply" class="hide"></pre>
  <div class="row">
    <input id="reply" size="60" placeholder="a short answer back to the sender (optional)">
    <button id="sendreply">Send answer</button>
  </div>
  <div class="row">
    <button id="download">Download as a file</button>
    <input id="fname" size="34" title="The name it will be saved under">
    <span class="sub" id="savehint"></span>
  </div>
  <div class="row">
    <button id="remove" class="danger">Collected - remove from the server</button>
    <span class="sub">Removing is final. Save the text first if you want to keep it.</span>
  </div>
  <p class="msg" id="onemsg"></p>
</section>

</main>
<script nonce="{NONCE}">
"use strict";
const $ = (id) => document.getElementById(id);
// current_collected: whether the report on screen has already been collected. It
// decides both the file name offered and what may go into the file - see
// default_name() and bundle_text().
let csrf = null, current = null, current_collected = false;

// Text goes in as text, never as markup: a report is somebody else's log, and
// this page is where it would be read back as HTML.
function show(el, on) { el.classList.toggle("hide", !on); }
function say(el, t) { el.textContent = t || ""; }

async function api(path, opts) {
  opts = opts || {};
  opts.headers = opts.headers || {};
  if (csrf) opts.headers["X-CSRF"] = csrf;
  if (opts.body) opts.headers["Content-Type"] = "application/json";
  const r = await fetch("/chat/admin" + path, opts);
  const text = await r.text();
  let data = {};
  try { data = JSON.parse(text); } catch (e) { data = { error: text }; }
  if (r.status === 401 || r.status === 403) {
    // An expired session used to look like an empty postbox: the list came
    // back refused, the page drew nothing, and an empty postbox and a lost
    // session are the same picture. Say which it is.
    expired();
    throw new Error("your session has expired - log in again");
  }
  if (!r.ok) throw new Error(data.error || ("failed (" + r.status + ")"));
  return data;
}

// Back to the login, with the reason, rather than a page that simply stops
// answering.
function expired() {
  csrf = null;
  if (keepAliveTimer) { clearInterval(keepAliveTimer); keepAliveTimer = null; }
  show($("login"), true);
  show($("logout"), false);
  show($("one"), false);
  show($("list"), false);
  say($("loginmsg"), "Your session expired. The same password as the relay.");
}

// Keep both administrations alive while either one is open.
//
// Two services, two sessions, two idle clocks - and the same person behind
// both. The relay's page has refreshed itself every ten seconds since it was
// written; this one never did, so it quietly ran down its half hour while
// being looked at, and switching over meant logging in again. Both are touched
// here, so working in either keeps both.
//
// Same origin, so the cookies travel by themselves. A failure is ignored on
// purpose: the relay may be unreachable, and that is not a reason to disturb
// somebody reading a report.
// Is somebody actually here? Both services slide their idle clock on any
// request they answer, so a page that pings on a timer keeps itself logged in
// for ever - and an open tab in an empty room was then an open postbox, full
// of other people's logs, half an hour after everyone went home. The lease
// follows the person instead: any sign of life pushes it forward, and half an
// hour without one lets both clocks run out on their own.
// Two hours, not the half hour the services themselves use. Thirty minutes
// is right for the service - it decides when an unused session dies - but
// wrong for this: sitting in the postbox while working in another window
// sends no mouse events at all, and the administrator was logged out of the
// relay while, from where they sat, they had been in the postbox the whole
// time. A forgotten screen still closes; a working evening does not
// (2026-08-16).
let lastSeen = Date.now();
const ACTIVE_FOR = 2 * 60 * 60 * 1000;
["mousemove", "mousedown", "keydown", "wheel", "touchstart", "scroll", "focus"].forEach(
  (e) => window.addEventListener(e, () => { lastSeen = Date.now(); }, { passive: true }));
function present() { return Date.now() - lastSeen < ACTIVE_FOR; }

async function keepAlive() {
  if (!present()) return;
  try { await fetch("/chat/admin/api/session", { method: "GET" }); } catch (e) {}
  try { await fetch("/admin/api/session", { method: "GET" }); } catch (e) {}
}

async function login() {
  say($("loginmsg"), "");
  try {
    const d = await api("/api/login", {
      method: "POST",
      body: JSON.stringify({ password: $("pw").value }),
    });
    csrf = d.csrf;
    $("pw").value = "";
    enter();
  } catch (e) { say($("loginmsg"), e.message); }
}

let keepAliveTimer = null;

function enter() {
  show($("login"), false);
  show($("logout"), true);
  if (!keepAliveTimer) {
    // A minute is far inside the half-hour idle window and costs one small
    // request per service. The point is not the interval but that the clock
    // stops running down while somebody is sitting here reading - and starts
    // again once they are not, which is what `present()` decides.
    keepAliveTimer = setInterval(keepAlive, 60000);
  }
  refresh();
  show($("keep"), true);
  keep_refresh();
  show($("mod"), true);
  mod_refresh();
}

async function refresh() {
  show($("one"), false);
  show($("list"), true);
  show($("keep"), true);
  show($("mod"), true);
  say($("listmsg"), "");
  try {
    const d = await api("/diagnoses");
    const rows = $("rows");
    rows.textContent = "";
    (d.reports || []).forEach((r) => {
      const tr = document.createElement("tr");
      tr.className = "r";
      // Two clocks in one column, so it says which one. A live report shows
      // when it arrived; a collected one has no arrival time kept - the marker
      // holds only the moment it was fetched, and that drives its expiry - so
      // it is labelled rather than passed off as the same thing. The list is
      // ordered by report number, which is the one thing both share (raised in
      // review, 2026-08-18).
      const when = r.collected
        ? "collected " + new Date(r.collected * 1000).toLocaleString()
        : new Date(r.at * 1000).toLocaleString();
      // "collected" is not a lesser "waiting": the report is on the
      // administrator's own computer and no longer here, but it can still be
      // answered - which is exactly why it is on this list at all.
      const state = r.collected
        ? (r.replied ? "collected, replied" : "collected")
        : (r.replied ? "replied" : (r.claimed ? "being read" : "waiting"));
      [String(r.id), when, r.name || "(no name)",
       Math.round(r.bytes / 1024) + " kB"].forEach((v) => {
        const td = document.createElement("td");
        td.textContent = v;
        tr.appendChild(td);
      });
      const td = document.createElement("td");
      const tag = document.createElement("span");
      tag.className = "tag" + (state === "waiting" ? " waiting" : "");
      tag.textContent = state;
      td.appendChild(tag);
      tr.appendChild(td);
      tr.onclick = () => open_one(r.id, r.name, when, r.reply, r.reply_at, r.collected);
      rows.appendChild(tr);
    });
    const n = (d.reports || []).length;
    say($("count"), n === 0 ? "nothing waiting" : n + (n === 1 ? " report" : " reports"));
  } catch (e) { say($("listmsg"), e.message); }
}

// Housekeeping runs hourly and, on a service younger than its shortest
// retention period, removes nothing every time. So the table is mostly zeros -
// and that is the reading: a row per hour means the timer is alive. An empty
// table means it is not, which is the one thing the log could not tell apart.
async function keep_refresh() {
  say($("keepmsg"), "");
  try {
    const d = await api("/housekeeping");
    const rows = $("keeprows");
    rows.textContent = "";
    (d.runs || []).forEach((r) => {
      const tr = document.createElement("tr");
      // A failed round used to look exactly like a quiet one - a row of zeros,
      // which is the normal reading on a young service. That is the very
      // distinction this table exists to make.
      [new Date(r.at * 1000).toLocaleString() + (r.ok ? "" : "  - FAILED"),
       String(r.messages), String(r.reports), String(r.replies),
       String(r.markers)].forEach((v) => {
        const td = document.createElement("td");
        td.textContent = v;
        tr.appendChild(td);
      });
      rows.appendChild(tr);
    });
    const runs = d.runs || [];
    if (runs.length === 0) {
      say($("keephead"), "no round recorded yet - the first one runs at startup");
    } else {
      const last = new Date(runs[0].at * 1000);
      const mins = Math.round((Date.now() - last.getTime()) / 60000);
      say($("keephead"), runs.length + " round(s), last one " + mins + " min ago");
    }
  } catch (e) { say($("keepmsg"), e.message); }
}

async function keep_whatif() {
  say($("keepmsg"), "");
  const raw = $("keepdate").value.trim();
  let q = "";
  if (raw) {
    const t = Date.parse(raw + "T00:00:00");
    if (isNaN(t)) { say($("keepmsg"), "a date reads as YYYY-MM-DD"); return; }
    q = "?at=" + Math.floor(t / 1000);
  }
  try {
    const d = await api("/prune-preview" + q);
    const when = new Date(d.at * 1000).toLocaleString();
    // The lines are joined rather than escaped: a newline written as an escape
    // inside this page has to survive being a Rust string literal first, and
    // getting that wrong produces a page that parses but does not run.
    const NL = String.fromCharCode(10);
    $("keepwhatif").textContent = [
      "on " + when + " housekeeping would remove:",
      "  " + d.messages_by_age + " message(s) past 90 days",
      "  " + d.messages_by_size + " message(s) over the store ceiling",
      "  " + d.reports + " uncollected report(s) past 30 days",
      "  " + d.delivered_replies + " delivered answer(s) past 7 days",
      "  " + d.undelivered_replies + " answer(s) never fetched, past 30 days",
      "  " + d.markers + " collected-report marker(s)",
      "nothing was removed - this was a question, not an instruction",
    ].join(NL);
  } catch (e) { say($("keepmsg"), e.message); }
}

// Moderation. The button sits on the message, because reading back what was
// said is the moment you want to be able to act - a list of bare station
// numbers somewhere else is not that moment.
async function mod_refresh() {
  say($("modmsg"), "");
  try {
    const d = await api("/recent");
    const rows = $("modrows");
    rows.textContent = "";
    (d.messages || []).forEach((m) => {
      const tr = document.createElement("tr");
      [new Date(m.at * 1000).toLocaleString(), m.name || "(left the chat)",
       m.body].forEach((v) => {
        const td = document.createElement("td");
        td.textContent = v;
        tr.appendChild(td);
      });
      const td = document.createElement("td");
      // Nobody left to ban once the author has gone: the text stays, the
      // person does not.
      if (m.station_id !== null) {
        const b = document.createElement("button");
        b.className = "danger";
        b.textContent = "Ban";
        b.onclick = () => ban_station(m.station_id, m.name);
        td.appendChild(b);
      }
      tr.appendChild(td);
      rows.appendChild(tr);
    });
    say($("modhead"), (d.messages || []).length + " recent message(s)");
  } catch (e) { say($("modmsg"), e.message); }
  try {
    const d = await api("/bans");
    const rows = $("banrows");
    rows.textContent = "";
    (d.bans || []).forEach((b) => {
      const tr = document.createElement("tr");
      [String(b.station_id), b.name || "(left the chat)",
       new Date(b.at * 1000).toLocaleString(),
       (b.reason || "") + (b.reason && b.shared ? "  (sent to them)" : "")].forEach((v) => {
        const td = document.createElement("td");
        td.textContent = v;
        tr.appendChild(td);
      });
      const td = document.createElement("td");
      const u = document.createElement("button");
      u.textContent = "Let back in";
      u.onclick = () => unban_station(b.station_id);
      td.appendChild(u);
      tr.appendChild(td);
      rows.appendChild(tr);
    });
  } catch (e) { say($("modmsg"), e.message); }
}

async function ban_station(id, name) {
  const who = name || ("station " + id);
  if (!confirm("Ban " + who + "? They lose the chat and reporting, and are told so. What they wrote stays, and they can still leave the chat and collect an answer they were already sent.")) return;
  const why = prompt("Why? (optional - your own note unless you send it below)") || "";
  // Off unless it is asked for: a note written to remember why is shorter and
  // blunter than what you would say to the person, and the two should not be
  // the same sentence by accident.
  const shared = why.trim() !== "" &&
    confirm("Send this reason to them as well?" + String.fromCharCode(10) +
            String.fromCharCode(10) + why + String.fromCharCode(10) +
            String.fromCharCode(10) + "Cancel keeps it as your own note.");
  try {
    await api("/ban", { method: "POST", body: JSON.stringify({ station_id: id, reason: why, shared: shared }) });
    mod_refresh();
  } catch (e) { say($("modmsg"), e.message); }
}

async function unban_station(id) {
  try {
    await api("/unban", { method: "POST", body: JSON.stringify({ station_id: id }) });
    mod_refresh();
  } catch (e) { say($("modmsg"), e.message); }
}

async function open_one(id, name, when, reply, reply_at, collected) {
  current_collected = !!collected;
  say($("onemsg"), "");
  $("reply").value = "";
  // What was answered, if anything. The page could say THAT one went out and
  // never what it said, so the one person who could not look it up was the one
  // who wrote it - while the reader still has it on their own screen.
  if (reply) {
    const NL = String.fromCharCode(10);
    const when_sent = reply_at ? new Date(reply_at * 1000).toLocaleString() : "";
    $("sentreply").textContent = "Answered " + when_sent + ":" + NL + NL + reply;
    show($("sentreply"), true);
  } else {
    show($("sentreply"), false);
  }
  try {
    current = id;
    if (collected) {
      // Nothing to fetch: collecting is what removes it from here. Say where it
      // went rather than showing an error, and leave the answer box working -
      // answering after collecting is the ordinary way of working.
      const gone = new Date(collected * 1000).toLocaleString();
      say($("onehead"), "#" + id + " from " + (name || "(no name)") + ", " + when);
      $("body").textContent =
        "This report was collected on " + gone + " and is no longer held here." +
        String.fromCharCode(10) +
        "It is on the computer it was fetched to. You can still answer it.";
    } else {
      const d = await api("/diagnosis?id=" + encodeURIComponent(id));
      $("body").textContent = d.report;
      say($("onehead"), "#" + id + " from " + (name || "(no name)") + ", " + when);
    }
    // A suggestion, not a decision: it is filled in fresh for each report so
    // one report is never saved under the previous one's name.
    $("fname").value = default_name();
    say($("savehint"), window.showSaveFilePicker
      ? "you will be asked where to put it"
      : "goes where this browser saves downloads");
    show($("list"), false);
    // Out of the way while a report is being read: this is somebody's fault
    // report, and a table of zeros underneath it is noise.
    show($("keep"), false);
    // The moderation block goes too, for the reason the block above goes: a
    // table of zeros and sixty other people's messages under somebody's fault
    // report is noise. One of the two was doing it and the other was not.
    show($("mod"), false);
    show($("one"), true);
  } catch (e) { say($("listmsg"), e.message); }
}

// The name offered for a saved report: which report, and when it came in.
function default_name() {
  // A different name when the report itself is not here. Saving under the
  // report's name would replace the file that holds the real report with one
  // holding the placeholder below - and that file is the only copy, because
  // collecting is what removed it from the service (raised in review,
  // 2026-08-18).
  return current_collected
    ? "thetislink-answer-" + current + ".txt"
    : "thetislink-report-" + current + ".txt";
}

// The report and the answer in one file.
//
// Apart they are two halves of a conversation filed in different places, and
// three weeks later nobody is asking what they wrote or what was sent back -
// they are asking both at once. The shape matches what the collect script does,
// so an answer sent from here and one sent from the command line produce the
// same SHAPE of file (2026-08-17).
//
// Not the same behaviour: the script reads the existing file and merges, this
// builds from what is on screen and writes the lot. Where the report is not on
// screen - a collected one - the report section is left out and the file gets a
// different name, so a saved answer can never replace the file holding the real
// report. That is the only copy left once a report has been collected (raised
// in review, 2026-08-18).
function bundle_text() {
  const NL = String.fromCharCode(10);
  const parts = ["ThetisLink " + $("onehead").textContent];
  const answered = $("sentreply");
  if (answered && !answered.classList.contains("hide") && answered.textContent) {
    // The answer FIRST. Under the report it lands hundreds of lines down - on
    // line 315 of 357 in a real one - and the question three weeks later is
    // what did I tell them, not what did the log say (2026-08-17).
    parts.push("", answered.textContent.split(NL).map(
      (l) => (l.trim() ? "  " + l : "")).join(NL));
  }
  // Only when the report is actually on screen. For a collected one the body
  // holds an explanation of where it went, and writing that under a MELDING
  // heading turns an explanation into a report. A later `postbox.sh reply`
  // merges the answer into the file that does hold the report.
  if (!current_collected) {
    parts.push("", "===== MELDING =====", "", $("body").textContent);
  }
  return parts.join(NL);
}

// Save what is on screen, exactly as it arrived. The heading is added so the
// file still says which report it was and who sent it - a bare log in a
// downloads folder is a puzzle a second time.
async function download_one() {
  // Built from an array rather than by concatenating escapes: this was written
  // by a script once, lost a layer of backslashes on the way, and put real
  // newlines inside the string literals. That is a syntax error, and a syntax
  // error here means the whole script never runs - so every button on the page
  // went dead, including the login, with nothing reaching the server to explain
  // it.
  const text = bundle_text();
  // Whatever is in the field, which starts as a sensible suggestion. Typed here
  // rather than only in the browser's own dialog, because half the browsers do
  // not have one: this way the name is always a choice, and on the browsers
  // that do have a dialog it arrives there already filled in.
  let name = ($("fname").value || "").trim() || default_name();
  if (!/\.[A-Za-z0-9]+$/.test(name)) name += ".txt";

  await save_text(text, name);
}

// Put text on this computer, asking where when the browser can ask.
//
// Shared by the report and by the answer, so both land in one archive under one
// naming rather than in whichever folder each button happened to pick. Returns
// whether anything was written: the caller may have something to say about that
// (see send_reply, which keeps the text on screen when nothing was kept).
async function save_text(text, name) {
  if (!/\.[A-Za-z0-9]+$/.test(name)) name += ".txt";

  // Chromium browsers have a real save dialog; the rest fall back to the plain
  // download, which lands wherever the browser is set to put things.
  if (window.showSaveFilePicker) {
    try {
      const handle = await window.showSaveFilePicker({
        suggestedName: name,
        types: [{ description: "Text file", accept: { "text/plain": [".txt"] } }],
      });
      const w = await handle.createWritable();
      await w.write(text);
      await w.close();
      say($("onemsg"), "saved as " + handle.name);
      return true;
    } catch (e) {
      // Closing the dialog is a decision, not a failure, and must not then save
      // the file anyway somewhere the user did not choose.
      if (e && (e.name === "AbortError" || e.name === "NotAllowedError")) {
        say($("onemsg"), "not saved");
        return false;
      }
      // Anything else: fall through and at least get the file out.
    }
  }

  const url = URL.createObjectURL(new Blob([text], { type: "text/plain;charset=utf-8" }));
  const a = document.createElement("a");
  a.href = url;
  a.download = name;
  document.body.appendChild(a);
  a.click();
  a.remove();
  // Give the browser a moment to start the download before the blob goes away.
  setTimeout(() => URL.revokeObjectURL(url), 10000);
  say($("onemsg"), "saved to your downloads folder as " + name);
  return true;
}

async function send_reply() {
  const text = $("reply").value.trim();
  if (!text) { say($("onemsg"), "nothing to send"); return; }
  try {
    await api("/reply", { method: "POST", body: JSON.stringify({ id: current, body: text }) });
  } catch (e) { say($("onemsg"), e.message); return; }

  // Sent. Now keep a copy, because the service deletes a delivered answer after
  // seven days - it is a postbox and not an archive - and the one person who
  // could not read back what had been said was the one who wrote it.
  //
  // The same name and the same heading the collect script writes, so an answer
  // sent from this page and one sent from the command line end up in one
  // archive that can be read as a whole.
  // Put it on the page as a sent answer first, so the file that follows is
  // built from exactly what is on screen - the same rule the report download
  // has always followed.
  const NL2 = String.fromCharCode(10);
  const stamp = new Date();
  $("sentreply").textContent =
    "Answered " + stamp.toLocaleString() + ":" + NL2 + NL2 + text;
  show($("sentreply"), true);

  const kept = await save_text(bundle_text(), default_name());
  if (kept) {
    // Only now: while nothing has been kept, the only copy of the wording is
    // the one on screen, and clearing it would throw away the thing this whole
    // step exists to preserve.
    $("reply").value = "";
    say($("onemsg"), "answer sent and saved");
  } else {
    say($("onemsg"), "answer SENT, but nothing was saved - the text is still here to copy");
  }
}

async function remove_one() {
  if (!confirm("Remove report #" + current + " from the server? This cannot be undone.")) return;
  try {
    await api("/release", { method: "POST", body: JSON.stringify({ id: current }) });
    refresh();
  } catch (e) { say($("onemsg"), e.message); }
}

$("go").onclick = login;
$("pw").addEventListener("keydown", (e) => { if (e.key === "Enter") login(); });
$("refresh").onclick = refresh;
$("keeprefresh").onclick = keep_refresh;
$("keepask").onclick = keep_whatif;
$("modrefresh").onclick = mod_refresh;
$("back").onclick = refresh;
$("sendreply").onclick = send_reply;
$("download").onclick = download_one;
$("remove").onclick = remove_one;
$("logout").onclick = async () => {
  try { await api("/api/logout", { method: "POST" }); } catch (e) { /* going anyway */ }
  // And the relay with it. The keep-alive crosses both ways; logging out did
  // not, so pressing this gave back a login screen while the OTHER
  // administration - the one with the stations on it - stayed open in the same
  // browser for up to another half hour. A list of paths with exactly one
  // missing, and the missing one was the safe direction (2026-08-16).
  try { await fetch("/admin/api/logout", { method: "POST" }); } catch (e) {}
  csrf = null;
  show($("logout"), false);
  show($("list"), false);
  show($("keep"), false);
  show($("mod"), false);
  show($("one"), false);
  show($("login"), true);
};

// A session may still be live from a previous visit; the cookie is HttpOnly, so
// asking the server is the only way to find out.
(async () => {
  try {
    const d = await api("/api/session");
    csrf = d.csrf;
    enter();
  } catch (e) { show($("login"), true); }
})();
</script>
</body>
</html>
"##;
