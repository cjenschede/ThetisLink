// SPDX-License-Identifier: GPL-2.0-or-later
//
//! Just enough HTTP to serve a handful of endpoints, and no more.
//!
//! A framework would bring a dependency tree for something that answers six
//! paths, on a container capped at 256 MB next to a relay carrying audio. The
//! part that actually needs care is not the routing but the reading: this port
//! is reachable from the internet through Caddy, so every bound here is a bound
//! against a peer that is not a client of ours.

use std::collections::HashMap;

use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

/// Largest request accepted, headers and body together.
///
/// Caddy already caps the body at 8 MB (design §2.2); this is the second bound,
/// because the machine has no swap and a process that buffers whatever arrives
/// is one that can get its neighbour killed instead of itself.
///
/// It MUST stay above `postbox::MAX_REPORT_BYTES`, and there is a test below
/// that says so. At 64 kB it did not: a problem report carries up to 200 kB of
/// log, so anything past the first 64 kB was cut off here and the service then
/// refused what was left as malformed JSON. The reports that had been sent were
/// all small enough to fit, which is exactly why nobody noticed - the first
/// report from a machine that had been running a while would have failed with a
/// message about JSON.
///
/// The same lesson, a size larger: at 1 MB a report carrying both a client and
/// a server log stopped arriving at all, and this layer dropped it without a
/// word (2026-08-12). Both bounds went up, and the drop now says so - see
/// `read_request`.
pub const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: String,
}

impl Request {
    /// The bearer token, if one was offered. Absence is not an error here — the
    /// caller decides which paths need one.
    pub fn bearer(&self) -> Option<&str> {
        self.headers
            .get("authorization")?
            .strip_prefix("Bearer ")
            .map(str::trim)
    }

    /// One named cookie, if the browser sent it.
    ///
    /// Header names arrive lower-cased; cookie NAMES do not, so they are matched
    /// as sent.
    pub fn cookie(&self, name: &str) -> Option<&str> {
        for part in self.headers.get("cookie")?.split(';') {
            let part = part.trim();
            if let Some(v) = part.strip_prefix(name) {
                if let Some(v) = v.strip_prefix('=') {
                    return Some(v);
                }
            }
        }
        None
    }

    pub fn query_i64(&self, key: &str, default: i64) -> i64 {
        self.query
            .get(key)
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(default)
    }
}

/// Read one request from the socket.
///
/// Returns `None` for anything that is not a request we can serve — including
/// things that are not HTTP at all, which is most of what an open port receives.
pub async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];

    // Headers first, up to the blank line.
    let head_end = loop {
        if let Some(pos) = find_double_crlf(&buf) {
            break pos;
        }
        if buf.len() >= MAX_REQUEST_BYTES {
            return None;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None; // peer went away mid-header
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let mut parts = lines.next()?.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            // Lowercased keys: header names are case-insensitive and a client
            // that writes "authorization" must not be treated differently from
            // one that writes "Authorization".
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let (path, query) = split_target(&target);

    // Then the body, if the headers promised one. The length is a hint to read
    // by, never a size to trust: it is capped, and a peer that stops early just
    // ends up with a shorter body.
    // A body that will not fit is refused outright rather than trimmed. A
    // truncated body is not a smaller request - it is a broken one, and it comes
    // back to the sender as a complaint about JSON instead of about size.
    let want: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if want > MAX_REQUEST_BYTES {
        // Said out loud, and answered. Dropping the connection here is what made
        // an oversized report look like a network fault: nothing in this log,
        // nothing back to the sender, and a person left guessing whether their
        // report was too big or their relay was down. A 413 costs one line and
        // ends that.
        log::warn!(
            "{} {}: refused a body of {} bytes ({} is the ceiling)",
            method,
            path,
            want,
            MAX_REQUEST_BYTES
        );
        let _ = respond_too_large(stream).await;
        return None;
    }
    let body_start = head_end + 4;
    while buf.len() < body_start + want {
        if buf.len() >= MAX_REQUEST_BYTES {
            break;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let end = (body_start + want).min(buf.len());
    let body = String::from_utf8_lossy(&buf[body_start.min(buf.len())..end]).to_string();

    Some(Request { method, path, query, headers, body })
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn split_target(target: &str) -> (String, HashMap<String, String>) {
    match target.split_once('?') {
        None => (target.to_string(), HashMap::new()),
        Some((p, q)) => {
            let mut map = HashMap::new();
            for pair in q.split('&') {
                if let Some((k, v)) = pair.split_once('=') {
                    map.insert(k.to_string(), v.to_string());
                }
            }
            (p.to_string(), map)
        }
    }
}

/// One JSON response, with the headers a browser and a proxy both need.
pub fn response(status: &str, body: &str) -> Vec<u8> {
    response_with(status, "application/json", &[], body)
}

/// Tell a sender its body is too big, in the same shape as every other refusal,
/// before hanging up on it.
///
/// Written straight to the socket rather than returned as a `Reply`, because
/// this happens while the request is still being read and there is nothing yet
/// to route.
async fn respond_too_large(stream: &mut TcpStream) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let body = format!(
        "{{\"error\":\"that is too large to send in one report ({} kB is the ceiling)\"}}",
        MAX_REQUEST_BYTES / 1024
    );
    stream.write_all(&response("413 Payload Too Large", &body)).await
}

/// The same, for the admin page: a content type of its own and room for a
/// Set-Cookie.
///
/// `no-store` stays on everything without exception. A cached page here is a
/// list of other people's problem reports left in a browser's cache directory.
pub fn response_with(
    status: &str,
    content_type: &str,
    extra_headers: &[String],
    body: &str,
) -> Vec<u8> {
    let mut head = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n",
        status,
        content_type,
        body.len()
    );
    for h in extra_headers {
        head.push_str(h);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two bounds have to be read together, and they were not: this is the
    /// one that would have caught it.
    #[test]
    fn the_reader_accepts_more_than_the_postbox_will_ever_store() {
        assert!(
            MAX_REQUEST_BYTES > crate::postbox::MAX_REPORT_BYTES,
            "a report the postbox would accept must survive being read"
        );
    }

    fn parse(raw: &str) -> Option<Request> {
        // The socket path is exercised by the endpoint tests; here the parsing
        // is checked directly, which is where a malformed request would bite.
        let head_end = find_double_crlf(raw.as_bytes())?;
        let head = &raw[..head_end];
        let mut lines = head.lines();
        let mut parts = lines.next()?.split_whitespace();
        let method = parts.next()?.to_string();
        let target = parts.next()?.to_string();
        let mut headers = HashMap::new();
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }
        let (path, query) = split_target(&target);
        let body = raw[head_end + 4..].to_string();
        Some(Request { method, path, query, headers, body })
    }

    #[test]
    fn a_get_with_a_query_is_split() {
        let r = parse("GET /chat/messages?since=42 HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!(r.method, "GET");
        assert_eq!(r.path, "/chat/messages");
        assert_eq!(r.query_i64("since", 0), 42);
        assert_eq!(r.query_i64("missing", 7), 7, "a default when it is absent");
    }

    /// Header names are case-insensitive, and a client that spells it in lower
    /// case must not be turned away for it.
    #[test]
    fn the_bearer_token_is_found_whatever_the_case() {
        let a = parse("GET / HTTP/1.1\r\nAuthorization: Bearer abc.def\r\n\r\n").unwrap();
        let b = parse("GET / HTTP/1.1\r\nauthorization: Bearer abc.def\r\n\r\n").unwrap();
        assert_eq!(a.bearer(), Some("abc.def"));
        assert_eq!(b.bearer(), Some("abc.def"));
    }

    #[test]
    fn no_token_is_not_an_error_here() {
        let r = parse("GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!(r.bearer(), None);
    }

    /// Another scheme is not our token, and must not be read as one.
    #[test]
    fn a_different_authorization_scheme_is_not_a_bearer_token() {
        let r = parse("GET / HTTP/1.1\r\nAuthorization: Basic dXNlcjpwdw==\r\n\r\n").unwrap();
        assert_eq!(r.bearer(), None);
    }

    #[test]
    fn a_body_survives_intact() {
        let r = parse("POST /chat/message HTTP/1.1\r\nContent-Length: 13\r\n\r\n{\"body\":\"hoi\"}").unwrap();
        assert_eq!(r.method, "POST");
        assert!(r.body.starts_with('{'));
    }

    /// This port is reached by port scanners and stray TLS handshakes. None of it
    /// may panic the process.
    #[test]
    fn rubbish_is_refused_without_panicking() {
        for junk in ["", "\r\n\r\n", "GET\r\n\r\n", "\u{1}\u{2}\r\n\r\n", "GET /"] {
            let _ = parse(junk);
        }
    }
}
