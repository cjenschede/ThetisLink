// SPDX-License-Identifier: GPL-2.0-or-later

//! Challenge-response authentication using HMAC-SHA256 with a pre-shared key (PSK).
//!
//! Flow:
//! 1. Client sends Heartbeat → Server sends AuthChallenge(nonce)
//! 2. Client computes HMAC-SHA256(PSK, nonce) → sends AuthResponse(hmac)
//! 3. Server verifies HMAC → sends AuthResult(accepted/rejected)
//!
//! After authentication, all packets from the client's IP:port are accepted.
//! The PSK never travels over the network.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Size of the random nonce in bytes
pub const NONCE_SIZE: usize = 16;

/// Size of the HMAC-SHA256 output
pub const HMAC_SIZE: usize = 32;

/// Generate a random nonce for the auth challenge.
pub fn generate_nonce() -> [u8; NONCE_SIZE] {
    use rand::RngCore;
    let mut nonce = [0u8; NONCE_SIZE];
    rand::thread_rng().fill_bytes(&mut nonce);
    nonce
}

/// Compute HMAC-SHA256(password, nonce).
pub fn compute_hmac(password: &str, nonce: &[u8; NONCE_SIZE]) -> [u8; HMAC_SIZE] {
    let mut mac = HmacSha256::new_from_slice(password.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(nonce);
    let result = mac.finalize();
    let mut out = [0u8; HMAC_SIZE];
    out.copy_from_slice(&result.into_bytes());
    out
}

/// Verify an HMAC response against the expected password + nonce.
/// Returns true if the HMAC is valid.
pub fn verify_hmac(password: &str, nonce: &[u8; NONCE_SIZE], hmac_response: &[u8; HMAC_SIZE]) -> bool {
    let expected = compute_hmac(password, nonce);
    // Constant-time comparison to prevent timing attacks
    constant_time_eq(&expected, hmac_response)
}

/// Constant-time byte comparison (prevents timing side-channel attacks).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() { return false; }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Validate password strength.
/// Returns Ok(()) if the password meets requirements, or Err with a description.
pub fn validate_password_strength(password: &str) -> Result<(), &'static str> {
    if password.len() < 8 {
        return Err("Wachtwoord moet minimaal 8 tekens lang zijn");
    }
    let has_letter = password.chars().any(|c| c.is_alphabetic());
    let has_digit = password.chars().any(|c| c.is_ascii_digit());
    if !has_letter || !has_digit {
        return Err("Wachtwoord moet zowel letters als cijfers bevatten");
    }
    Ok(())
}

/// Obfuscate a password for storage in config files.
/// Not true encryption — prevents casual reading of the password.
/// Uses XOR with a fixed key + base64 encoding.
const OBFUSCATE_KEY: &[u8] = b"ThetisLink-PSK-2026";

pub fn obfuscate_password(password: &str) -> String {
    let bytes: Vec<u8> = password.as_bytes().iter().enumerate()
        .map(|(i, &b)| b ^ OBFUSCATE_KEY[i % OBFUSCATE_KEY.len()])
        .collect();
    // Simple base64-like encoding using hex
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

pub fn deobfuscate_password(encoded: &str) -> Option<String> {
    let bytes: Vec<u8> = (0..encoded.len())
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(&encoded[i..i+2], 16).ok())
        .collect();
    let decoded: Vec<u8> = bytes.iter().enumerate()
        .map(|(i, &b)| b ^ OBFUSCATE_KEY[i % OBFUSCATE_KEY.len()])
        .collect();
    String::from_utf8(decoded).ok()
}

// --- TOTP 2FA ---

/// TOTP configuration
const TOTP_PERIOD: u64 = 30; // seconds
const TOTP_DIGITS: u32 = 6;

/// Generate a random 20-byte TOTP secret, returned as base32 string.
pub fn generate_totp_secret() -> String {
    use rand::RngCore;
    let mut secret = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut secret);
    base32::encode(base32::Alphabet::Rfc4648 { padding: false }, &secret)
}

/// Generate an otpauth:// URI for QR code generation.
/// Format: otpauth://totp/ThetisLink?secret=BASE32SECRET&issuer=ThetisLink
pub fn totp_uri(secret_base32: &str) -> String {
    format!(
        "otpauth://totp/ThetisLink?secret={}&issuer=ThetisLink&digits={}&period={}",
        secret_base32, TOTP_DIGITS, TOTP_PERIOD
    )
}

/// Verify a TOTP code. Accepts current period and one period before/after (clock skew tolerance).
pub fn verify_totp(secret_base32: &str, code: &str) -> bool {
    if code.len() != TOTP_DIGITS as usize { return false; }
    let Some(secret_bytes) = base32::decode(base32::Alphabet::Rfc4648 { padding: false }, secret_base32) else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // Check current time and ±1 period for clock skew
    // totp_custom expects raw unix timestamp, it divides by step internally
    for offset in [0i64, -(TOTP_PERIOD as i64), TOTP_PERIOD as i64] {
        let time = (now as i64 + offset) as u64;
        let expected: String = totp_lite::totp_custom::<totp_lite::Sha1>(
            TOTP_PERIOD, TOTP_DIGITS, &secret_bytes, time,
        );
        if expected == code {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_roundtrip() {
        let nonce = generate_nonce();
        let password = "test_password_123";

        let hmac = compute_hmac(password, &nonce);
        assert!(verify_hmac(password, &nonce, &hmac));
    }

    #[test]
    fn wrong_password_rejected() {
        let nonce = generate_nonce();
        let hmac = compute_hmac("correct_password", &nonce);
        assert!(!verify_hmac("wrong_password", &nonce, &hmac));
    }

    #[test]
    fn wrong_nonce_rejected() {
        let nonce1 = generate_nonce();
        let nonce2 = generate_nonce();
        let hmac = compute_hmac("password", &nonce1);
        assert!(!verify_hmac("password", &nonce2, &hmac));
    }

    #[test]
    fn password_strength_valid() {
        assert!(validate_password_strength("MyPass12").is_ok());
        assert!(validate_password_strength("ham2radio").is_ok());
        assert!(validate_password_strength("Str0ngP@ssw0rd!").is_ok());
    }

    #[test]
    fn password_strength_too_short() {
        assert!(validate_password_strength("Ab1").is_err());
        assert!(validate_password_strength("Pass12").is_err());
        assert!(validate_password_strength("").is_err());
    }

    #[test]
    fn password_strength_missing_digits() {
        assert!(validate_password_strength("OnlyLetters").is_err());
    }

    #[test]
    fn password_strength_missing_letters() {
        assert!(validate_password_strength("12345678").is_err());
    }

    #[test]
    fn obfuscate_roundtrip() {
        let password = "MySecretHamKey!123";
        let encoded = obfuscate_password(password);
        assert_ne!(encoded, password); // Must not be plaintext
        let decoded = deobfuscate_password(&encoded).unwrap();
        assert_eq!(decoded, password);
    }

    #[test]
    fn obfuscate_empty() {
        let encoded = obfuscate_password("");
        let decoded = deobfuscate_password(&encoded).unwrap();
        assert_eq!(decoded, "");
    }
}
