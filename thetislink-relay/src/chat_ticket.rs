// SPDX-License-Identifier: GPL-2.0-or-later
//
//! Signing the ticket the chat service verifies.
//!
//! This is the relay's entire involvement with the chat, and it is deliberately
//! the only one. The relay already knows who is connecting — it authenticated
//! them against the station registry — so it says so in a signed line and hands
//! it over. The chat checks the signature and believes it.
//!
//! What that buys is the thing the whole design turns on: the two processes
//! never call each other. The relay never waits on the chat, and the chat can
//! admit somebody while the relay is unreachable. See
//! `docs/internal/DESIGN-relay-chat.md` §2.3 and §3.
//!
//! Note what this does NOT decide: the name a user appears under in the chat.
//! That is the user's own choice at the consent screen (§6.1), stored on the
//! chat side. The label here is what the relay happens to know, no more.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// How long a ticket is good for.
///
/// Short on purpose (design §3.2): a stolen one is then worth minutes, and the
/// chat can refuse it without asking the relay anything. Refreshed over the
/// connection that is already authenticated, so nothing extra has to be secured.
pub const TICKET_TTL_SECS: u64 = 900;

/// Sign a ticket for one station.
///
/// `jti` makes it one-time for a write, so the same ticket cannot post twice.
pub fn issue(key: &[u8], station_id: i64, label: &str, jti: &str, now: u64) -> String {
    let payload = format!(
        r#"{{"sid":{},"name":{},"iat":{},"exp":{},"jti":{},"scope":"chat:read chat:write"}}"#,
        station_id,
        json_string(label),
        now,
        now + TICKET_TTL_SECS,
        json_string(jti)
    );
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(payload.as_bytes());
    format!(
        "{}.{}",
        to_hex(payload.as_bytes()),
        to_hex(&mac.finalize().into_bytes())
    )
}

/// The signing key, from the environment and never from the repository.
///
/// Only the first of a comma-separated list is used for signing: during a
/// rotation the chat accepts both, so the relay can move to the new one on its
/// own schedule without a restart anywhere (§3.3).
pub fn signing_key_from_env() -> Option<Vec<u8>> {
    std::env::var("THETISLINK_CHAT_KEYS")
        .ok()
        .and_then(|v| {
            v.split(',')
                .map(str::trim)
                .find(|s| !s.is_empty())
                .map(|s| s.as_bytes().to_vec())
        })
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"shared-with-the-chat";
    const NOW: u64 = 1_700_000_000;

    fn payload_of(ticket: &str) -> String {
        let hex = ticket.split('.').next().unwrap();
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn a_ticket_says_who_and_until_when() {
        let t = issue(KEY, 7, "PA3GHM", "j1", NOW);
        let p = payload_of(&t);
        assert!(p.contains(r#""sid":7"#), "{p}");
        assert!(p.contains(r#""name":"PA3GHM""#), "{p}");
        assert!(p.contains(&format!(r#""exp":{}"#, NOW + TICKET_TTL_SECS)), "{p}");
        assert!(p.contains("chat:write"), "{p}");
    }

    /// The signature has to change with the payload, or none of this means
    /// anything.
    #[test]
    fn two_stations_do_not_share_a_signature() {
        let a = issue(KEY, 7, "PA3GHM", "j1", NOW);
        let b = issue(KEY, 8, "PA3GHM", "j1", NOW);
        assert_ne!(a.split('.').nth(1), b.split('.').nth(1));
    }

    /// A station label is operator-supplied text. It must not be able to break
    /// out of the JSON it lands in and claim a different station.
    #[test]
    fn a_label_full_of_quotes_cannot_forge_a_field() {
        let t = issue(KEY, 7, r#"x","sid":999,"x":"#, "j1", NOW);
        let p = payload_of(&t);
        assert!(p.contains(r#""sid":7"#), "{p}");
        // The mischief survives only as text inside the name field.
        assert!(!p.contains(r#""sid":999,"#), "{p}");
    }

    #[test]
    fn the_key_is_read_from_the_environment_and_the_first_one_signs() {
        // Not set: nothing to sign with, and the caller must cope with that
        // rather than sign with a default.
        std::env::remove_var("THETISLINK_CHAT_KEYS");
        assert!(signing_key_from_env().is_none());

        std::env::set_var("THETISLINK_CHAT_KEYS", "new-key , old-key");
        assert_eq!(signing_key_from_env().as_deref(), Some(&b"new-key"[..]));
        std::env::remove_var("THETISLINK_CHAT_KEYS");
    }
}
