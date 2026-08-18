// SPDX-License-Identifier: GPL-2.0-or-later
//
//! One administrator, one password: the postbox asks the relay.
//!
//! There is a single person running both services, so there should be a single
//! thing to remember. Rather than copy the relay's password, or its hash, or its
//! sessions, the chat simply offers what was typed to the relay's own login and
//! believes the answer.
//!
//! # The direction matters
//!
//! The dependency points chat → relay and never the other way (design §2.4).
//! That is the one direction the design permits, and it is the harmless one: a
//! relay that is down means the postbox page cannot be logged into for a while,
//! while reports keep arriving and the relay keeps carrying audio regardless of
//! what happens here. The reverse - a relay that needs the chat - is what must
//! never exist, and this does not create it. The relay knows nothing about this
//! and needs no change to support it.
//!
//! The administrator token still works as a password on the same form. Not as a
//! second-class fallback but as the deliberate way in when the relay is not
//! answering: locking the postbox because the neighbour is down would be exactly
//! the coupling this file is careful to avoid.
//!
//! # Why by hand
//!
//! One plain HTTP POST inside the compose network, to a service in the next
//! container. An HTTP client crate would bring a TLS stack into a 256 MB
//! container to talk to a neighbour over a private bridge, for one request that
//! happens when somebody logs in.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Where the relay's admin API listens inside the compose network.
///
/// Overridable, because "thetislink-relay" is a compose service name and this
/// should not stop working the day the deployment is arranged differently.
pub fn relay_admin_addr() -> String {
    std::env::var("THETISLINK_RELAY_ADMIN")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "thetislink-relay:18081".to_string())
}

/// Long enough for a neighbour under load, short enough that a login form does
/// not appear to hang when the relay is gone.
const TIMEOUT: Duration = Duration::from_secs(4);

/// Ask the relay whether this is its administrator password.
///
/// `Ok(true)` = yes. `Ok(false)` = the relay answered and said no - which is a
/// real answer and must be treated as a failed attempt. `Err` = the relay could
/// not be reached or did not answer sensibly, which is NOT a failed attempt: it
/// says nothing about the password, and counting it would let a relay restart
/// lock the administrator out of the postbox.
pub async fn verify(addr: &str, password: &str, client_ip: &str) -> Result<bool, String> {
    let body = format!("{{\"password\":{}}}", json_string(password));
    let req = format!(
        "POST /admin/api/login HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         X-Forwarded-For: {client_ip}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );

    status_of(&fetch(addr, &req).await?)
}

/// Ask the relay whether one of its own sessions is still good.
///
/// This is what makes a single login enough: the browser already holds the
/// relay's session cookie (it is set for the whole site), so the postbox offers
/// it back to the relay and asks. It never inspects it, never stores it, and
/// could not validate it if it wanted to - the session lives in the relay's
/// memory and nowhere else.
///
/// `Ok(false)` covers both "not logged in" and "expired", which look the same
/// from here and need the same answer: show the login.
pub async fn verify_session(addr: &str, cookie: &str, client_ip: &str) -> Result<bool, String> {
    let req = format!(
        "GET /admin/api/session HTTP/1.1\r\nHost: {addr}\r\nCookie: {cookie}\r\n\
         X-Forwarded-For: {client_ip}\r\nConnection: close\r\n\r\n"
    );
    let resp = fetch(addr, &req).await?;
    // This endpoint answers 200 either way and says so in the body, so the
    // status line is not the answer here.
    match status_of(&resp) {
        Ok(_) => Ok(resp.contains("\"authenticated\":true")),
        Err(e) => Err(e),
    }
}

/// One request, one response, with a deadline.
async fn fetch(addr: &str, req: &str) -> Result<String, String> {
    let work = async {
        let mut s = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
        s.write_all(req.as_bytes()).await.map_err(|e| e.to_string())?;
        s.flush().await.map_err(|e| e.to_string())?;
        let mut buf = Vec::with_capacity(1024);
        let mut chunk = [0u8; 1024];
        loop {
            let n = s.read(&mut chunk).await.map_err(|e| e.to_string())?;
            if n == 0 || buf.len() > 8192 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        Ok::<String, String>(String::from_utf8_lossy(&buf).to_string())
    };
    tokio::time::timeout(TIMEOUT, work)
        .await
        .map_err(|_| "the relay did not answer in time".to_string())?
}

/// Read the verdict out of a raw response.
///
/// Anything that is not a clear yes or a clear no is an error rather than a
/// silent "no": a relay answering 500 is a broken neighbour, and telling the
/// administrator their password is wrong would send them looking in the wrong
/// place entirely.
fn status_of(resp: &str) -> Result<bool, String> {
    let line = resp.lines().next().unwrap_or_default();
    let code = line.split_whitespace().nth(1).unwrap_or_default();
    match code {
        "200" => Ok(true),
        "401" => Ok(false),
        "429" => Err("the relay is refusing logins from here for the moment".to_string()),
        "" => Err("the relay gave no answer".to_string()),
        other => Err(format!("the relay answered {other}")),
    }
}

/// The same hand-rolled escaping the rest of the service uses. A password may
/// contain anything at all, and it is about to travel inside JSON.
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
    fn a_yes_is_a_yes_and_a_no_is_a_no() {
        assert_eq!(status_of("HTTP/1.1 200 OK\r\n\r\n{}"), Ok(true));
        assert_eq!(status_of("HTTP/1.1 401 Unauthorized\r\n\r\n"), Ok(false));
    }

    /// The distinction this file exists for: a broken relay must not be read as
    /// a wrong password, or the administrator goes looking in the wrong place
    /// and every attempt counts against them.
    #[test]
    fn anything_else_is_an_error_and_not_a_refusal() {
        for r in [
            "HTTP/1.1 500 Internal Server Error\r\n\r\n",
            "HTTP/1.1 502 Bad Gateway\r\n\r\n",
            "HTTP/1.1 429 Too Many Requests\r\n\r\n",
            "",
            "not http at all",
        ] {
            assert!(status_of(r).is_err(), "{r:?} should not be a plain refusal");
        }
    }

    /// A password is arbitrary text and is about to be put inside JSON.
    #[test]
    fn a_password_with_quotes_survives_the_journey() {
        assert_eq!(json_string("a\"b\\c"), r#""a\"b\\c""#);
    }
}
