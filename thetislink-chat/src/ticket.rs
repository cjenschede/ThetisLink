// SPDX-License-Identifier: GPL-2.0-or-later
//
//! The ticket the relay hands out and this service verifies.
//!
//! The point of it is what it lets the two processes *avoid*: they never talk to
//! each other. The relay signs a small statement about who is connecting; the
//! chat checks the signature and believes it. So the chat can admit someone
//! while the relay is unreachable, and — far more important — the relay never
//! waits on the chat for anything (design §2.2, §2.3).
//!
//! The alternative, letting the chat read the relay's `stations.db`, looks free
//! and is not: two processes that update independently would share one database
//! file, so a migration here would sit on the relay's data and a hung chat could
//! lock out the thing carrying audio.
//!
//! Wire form is `<hex payload>.<hex signature>`. Hex rather than base64 to avoid
//! a dependency for something this small; the extra length is irrelevant at this
//! size.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// What a verified ticket says. Nothing more than this is trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    pub station_id: i64,
    pub display_name: String,
    pub issued_at: u64,
    pub expires_at: u64,
    /// One-time id, so the same ticket cannot be replayed for a write.
    pub jti: String,
    pub scopes: Vec<String>,
}

impl Ticket {
    pub fn has_scope(&self, want: &str) -> bool {
        self.scopes.iter().any(|s| s == want)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum TicketError {
    /// Not two hex parts separated by a dot, or not valid hex/JSON.
    Malformed,
    /// The signature does not match — wrong key, or tampering.
    BadSignature,
    /// Correctly signed, but past `expires_at`.
    Expired,
    /// Correctly signed, but `issued_at` lies far in the future: a clock that
    /// disagrees this much is not a ticket we can reason about.
    NotYetValid,
}

impl std::fmt::Display for TicketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // These strings end up in the log, and the design (§8) asks for the
        // distinction between "a clock is wrong" and "somebody is trying
        // something" to survive into it.
        let s = match self {
            TicketError::Malformed => "malformed ticket",
            TicketError::BadSignature => "bad signature",
            TicketError::Expired => "expired",
            TicketError::NotYetValid => "issued in the future (clock skew?)",
        };
        f.write_str(s)
    }
}

/// How far a clock may run ahead before a ticket is refused.
///
/// Generous on purpose: a wrong clock is a support call, not an attack, and the
/// expiry below is what actually bounds a stolen ticket.
const FUTURE_TOLERANCE_SECS: u64 = 300;

/// Verify a ticket against one or more accepted keys.
///
/// Several keys because rotation must not require restarting the relay (design
/// §3.3): during a change-over both the old and the new key are accepted, and
/// only then is the old one dropped.
pub fn verify(token: &str, keys: &[Vec<u8>], now: u64) -> Result<Ticket, TicketError> {
    let (payload_hex, sig_hex) = token.split_once('.').ok_or(TicketError::Malformed)?;
    let payload = from_hex(payload_hex).ok_or(TicketError::Malformed)?;
    let sig = from_hex(sig_hex).ok_or(TicketError::Malformed)?;

    // Signature first, always. Nothing inside an unverified payload may steer a
    // decision — not even which error to report.
    let mut ok = false;
    for key in keys {
        let mut mac = match HmacSha256::new_from_slice(key) {
            Ok(m) => m,
            Err(_) => continue,
        };
        mac.update(&payload);
        if mac.verify_slice(&sig).is_ok() {
            ok = true;
            break;
        }
    }
    if !ok {
        return Err(TicketError::BadSignature);
    }

    let json: serde_json::Value =
        serde_json::from_slice(&payload).map_err(|_| TicketError::Malformed)?;
    let ticket = Ticket {
        station_id: json.get("sid").and_then(|v| v.as_i64()).ok_or(TicketError::Malformed)?,
        display_name: json
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or(TicketError::Malformed)?
            .to_string(),
        issued_at: json.get("iat").and_then(|v| v.as_u64()).ok_or(TicketError::Malformed)?,
        expires_at: json.get("exp").and_then(|v| v.as_u64()).ok_or(TicketError::Malformed)?,
        jti: json
            .get("jti")
            .and_then(|v| v.as_str())
            .ok_or(TicketError::Malformed)?
            .to_string(),
        scopes: json
            .get("scope")
            .and_then(|v| v.as_str())
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default(),
    };

    if now > ticket.expires_at {
        return Err(TicketError::Expired);
    }
    if ticket.issued_at > now.saturating_add(FUTURE_TOLERANCE_SECS) {
        return Err(TicketError::NotYetValid);
    }
    Ok(ticket)
}

/// Sign a payload. Lives here so the test can produce a ticket the way the relay
/// will, rather than testing the verifier against its own idea of correct.
/// The issuing half, which lives in the relay in production - here it is the
/// counterpart the tests below sign with, so this crate can check its own
/// verifier without the relay present. Unused outside tests by design.
#[allow(dead_code)]
pub fn sign(payload: &[u8], key: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(payload);
    format!("{}.{}", to_hex(payload), to_hex(&mac.finalize().into_bytes()))
}

#[allow(dead_code)]
fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"a-shared-secret-between-relay-and-chat";
    const NOW: u64 = 1_000_000;

    fn payload(exp: u64, iat: u64) -> Vec<u8> {
        format!(
            r#"{{"sid":7,"name":"PA0ABC","iat":{},"exp":{},"jti":"abc123","scope":"chat:read chat:write"}}"#,
            iat, exp
        )
        .into_bytes()
    }

    #[test]
    fn a_valid_ticket_is_accepted_and_says_what_it_says() {
        let t = sign(&payload(NOW + 600, NOW), KEY);
        let got = verify(&t, &[KEY.to_vec()], NOW).expect("accepted");
        assert_eq!(got.station_id, 7);
        assert_eq!(got.display_name, "PA0ABC");
        assert_eq!(got.jti, "abc123");
        assert!(got.has_scope("chat:write"));
        assert!(!got.has_scope("chat:admin"));
    }

    /// The whole separation rests on this: a payload nobody signed is worth
    /// nothing, however well-formed it looks.
    #[test]
    fn an_unsigned_payload_is_refused() {
        let forged = format!("{}.{}", to_hex(&payload(NOW + 600, NOW)), to_hex(&[0u8; 32]));
        assert_eq!(verify(&forged, &[KEY.to_vec()], NOW), Err(TicketError::BadSignature));
    }

    /// Changing one character of a signed payload must not survive.
    #[test]
    fn tampering_with_a_signed_payload_is_caught() {
        let t = sign(&payload(NOW + 600, NOW), KEY);
        let (p, s) = t.split_once('.').unwrap();
        let mut bytes = from_hex(p).unwrap();
        let i = bytes.iter().position(|b| *b == b'7').unwrap();
        bytes[i] = b'8'; // station 7 -> 8
        let tampered = format!("{}.{}", to_hex(&bytes), s);
        assert_eq!(verify(&tampered, &[KEY.to_vec()], NOW), Err(TicketError::BadSignature));
    }

    #[test]
    fn an_expired_ticket_is_refused() {
        let t = sign(&payload(NOW - 1, NOW - 900), KEY);
        assert_eq!(verify(&t, &[KEY.to_vec()], NOW), Err(TicketError::Expired));
    }

    /// A clock that disagrees by minutes is a support call; one that disagrees by
    /// hours is not something to reason about.
    #[test]
    fn small_clock_skew_is_tolerated_and_large_skew_is_not() {
        let near = sign(&payload(NOW + 3600, NOW + 60), KEY);
        assert!(verify(&near, &[KEY.to_vec()], NOW).is_ok());
        let far = sign(&payload(NOW + 99_999, NOW + 10_000), KEY);
        assert_eq!(verify(&far, &[KEY.to_vec()], NOW), Err(TicketError::NotYetValid));
    }

    /// Rotation must not need a relay restart, so both keys are live at once.
    #[test]
    fn either_key_is_accepted_during_a_rotation() {
        let old = b"old-key".to_vec();
        let new = b"new-key".to_vec();
        let signed_with_old = sign(&payload(NOW + 600, NOW), &old);
        let signed_with_new = sign(&payload(NOW + 600, NOW), &new);
        let keys = vec![new.clone(), old.clone()];
        assert!(verify(&signed_with_old, &keys, NOW).is_ok());
        assert!(verify(&signed_with_new, &keys, NOW).is_ok());
        // And once the old one is dropped, tickets signed with it stop working.
        assert_eq!(
            verify(&signed_with_old, &[new], NOW),
            Err(TicketError::BadSignature)
        );
    }

    /// This endpoint faces the internet. None of these may panic it.
    #[test]
    fn rubbish_is_refused_without_panicking() {
        for junk in ["", ".", "xx.yy", "zz.zz", "abc", "6162.", ".6162", "616263"] {
            assert!(verify(junk, &[KEY.to_vec()], NOW).is_err(), "{junk:?}");
        }
    }

    /// A correctly signed but non-JSON payload must not be mistaken for a
    /// signature problem - the two mean different things in the log.
    #[test]
    fn signed_rubbish_reports_malformed_and_not_bad_signature() {
        let t = sign(b"not json at all", KEY);
        assert_eq!(verify(&t, &[KEY.to_vec()], NOW), Err(TicketError::Malformed));
    }
}
