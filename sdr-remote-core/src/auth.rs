// SPDX-License-Identifier: GPL-2.0-or-later

//! Challenge-response authentication using HMAC-SHA256 with a pre-shared key (PSK),
//! mutual since phase 2 of the direct connection.
//!
//! Flow:
//! 1. Client sends Heartbeat → Server sends AuthChallenge(nonce_s)
//! 2. Client computes HMAC-SHA256(PSK, nonce_s) → sends AuthResponse(hmac, nonce_c)
//! 3. Server verifies the HMAC, derives K = HKDF(PSK, nonce_s ‖ nonce_c) → sends
//!    AuthResult(code, proof = HMAC(K, label ‖ nonce_s ‖ nonce_c))
//! 4. Client derives the same K and verifies the proof. A server that cannot
//!    prove it knows the PSK is not connected to.
//!
//! A peer from before phase 2 sends and reads both packets without the tails
//! (`nonce_c`, `proof`). An older client is still admitted by the server, without
//! a key; an older server gives no proof, and the client refuses it (owner's
//! decision F, 2026-10-02).
//!
//! Over QUIC (phase 3) every step also covers the connection's channel binding:
//! 32 bytes both ends export from the TLS session. The client's HMAC, the session
//! key and the server's proof all take it, so a party in the middle - who holds
//! two TLS sessions and therefore two bindings - has a proof that fits neither
//! end. Without a binding (UDP, the relay) every value is what it was before.
//!
//! After authentication, all packets from the client's IP:port are accepted -
//! still: the key is not used before phase 5 (per-packet encryption).
//! The PSK never travels over the network.
//!
//! K is one HKDF pass over the PSK, with no work factor: it makes the password
//! no harder to guess offline than the client's HMAC already did - one hash
//! pair per guess, from one overheard authentication. What would is a PAKE or
//! a stretched derivation, to be decided before phase 5, when the derivation
//! becomes part of the wire format (design §5.5; review 66).

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Size of the random nonce in bytes
pub const NONCE_SIZE: usize = 16;

/// Size of the HMAC-SHA256 output
pub const HMAC_SIZE: usize = 32;

/// Size of a channel binding (the QUIC connection's TLS export).
pub const BINDING_SIZE: usize = 32;

/// Marks where a channel binding enters a MAC, so a binding can never be read as
/// part of a nonce.
const BINDING_LABEL: &[u8] = b"ThetisLink channel binding v1";

/// Feed `binding` into `mac` - the one place that decides how. Nothing at all
/// without one, so every value without a binding is what it was before phase 3.
fn mac_binding(mac: &mut HmacSha256, binding: Option<&[u8; BINDING_SIZE]>) {
    if let Some(b) = binding {
        mac.update(BINDING_LABEL);
        mac.update(b);
    }
}

/// Generate a random nonce for the auth challenge.
pub fn generate_nonce() -> [u8; NONCE_SIZE] {
    use rand::RngCore;
    let mut nonce = [0u8; NONCE_SIZE];
    rand::thread_rng().fill_bytes(&mut nonce);
    nonce
}

/// Compute HMAC-SHA256(password, nonce [, binding]).
pub fn compute_hmac(
    password: &str,
    nonce: &[u8; NONCE_SIZE],
    binding: Option<&[u8; BINDING_SIZE]>,
) -> [u8; HMAC_SIZE] {
    let mut mac = HmacSha256::new_from_slice(password.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(nonce);
    mac_binding(&mut mac, binding);
    let result = mac.finalize();
    let mut out = [0u8; HMAC_SIZE];
    out.copy_from_slice(&result.into_bytes());
    out
}

/// Verify an HMAC response against the expected password + nonce.
/// Returns true if the HMAC is valid.
pub fn verify_hmac(
    password: &str,
    nonce: &[u8; NONCE_SIZE],
    hmac_response: &[u8; HMAC_SIZE],
    binding: Option<&[u8; BINDING_SIZE]>,
) -> bool {
    let expected = compute_hmac(password, nonce, binding);
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

// --- The session key and the server's proof (phase 2 of the direct connection) ---

/// Size of the nonce the client adds to its `AuthResponse`.
pub const CLIENT_NONCE_SIZE: usize = 16;
/// Size of the key both sides derive from the password and the two nonces.
pub const SESSION_KEY_SIZE: usize = 32;
/// Size of the server's proof in `AuthResult`.
pub const PROOF_SIZE: usize = 32;
/// How long a challenge stays answerable. A slower answer gets a new challenge,
/// not a failure. Under the server's session time-out (15 s): a client that has
/// not authenticated never refreshes its session, so a longer TTL could never
/// fire - the reaper was always first (review 66, both reviewers).
pub const CHALLENGE_TTL_SECS: u64 = 10;

const SESSION_KEY_INFO: &[u8] = b"ThetisLink session key v1";
const SERVER_PROOF_LABEL: &[u8] = b"ThetisLink server proof";

/// HKDF-SHA256 (RFC 5869): extract with `salt`, expand with `info`, into `out`.
/// Private: the one caller asks for 32 bytes, far under HKDF's limit of 255
/// hash lengths, past which `expand` fails.
fn hkdf_sha256(ikm: &[u8], salt: &[u8], info: &[u8], out: &mut [u8]) {
    hkdf::Hkdf::<Sha256>::new(Some(salt), ikm)
        .expand(info, out)
        .expect("output length is within HKDF's limit");
}

/// The nonce the client adds to its response, so the key binds both sides.
pub fn generate_client_nonce() -> [u8; CLIENT_NONCE_SIZE] {
    use rand::RngCore;
    let mut nonce = [0u8; CLIENT_NONCE_SIZE];
    rand::thread_rng().fill_bytes(&mut nonce);
    nonce
}

/// The key of one session: the password and both nonces, so no two sessions
/// share one and neither side can choose it alone. Computed on both sides.
pub fn session_key(
    password: &str,
    server_nonce: &[u8; NONCE_SIZE],
    client_nonce: &[u8; CLIENT_NONCE_SIZE],
    binding: Option<&[u8; BINDING_SIZE]>,
) -> [u8; SESSION_KEY_SIZE] {
    let mut salt = Vec::with_capacity(NONCE_SIZE + CLIENT_NONCE_SIZE + BINDING_LABEL.len() + BINDING_SIZE);
    salt.extend_from_slice(server_nonce);
    salt.extend_from_slice(client_nonce);
    if let Some(b) = binding {
        salt.extend_from_slice(BINDING_LABEL);
        salt.extend_from_slice(b);
    }
    let mut key = [0u8; SESSION_KEY_SIZE];
    hkdf_sha256(password.as_bytes(), &salt, SESSION_KEY_INFO, &mut key);
    key
}

/// What the server sends back to show it knows the password: a MAC under the
/// session key over both nonces. Only a holder of the password can derive the
/// key, and the nonces keep it from being replayed.
pub fn server_proof(
    key: &[u8; SESSION_KEY_SIZE],
    server_nonce: &[u8; NONCE_SIZE],
    client_nonce: &[u8; CLIENT_NONCE_SIZE],
    binding: Option<&[u8; BINDING_SIZE]>,
) -> [u8; PROOF_SIZE] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(SERVER_PROOF_LABEL);
    mac.update(server_nonce);
    mac.update(client_nonce);
    mac_binding(&mut mac, binding);
    let mut out = [0u8; PROOF_SIZE];
    out.copy_from_slice(&mac.finalize().into_bytes());
    out
}

/// The client's side of `server_proof`.
pub fn verify_server_proof(
    key: &[u8; SESSION_KEY_SIZE],
    server_nonce: &[u8; NONCE_SIZE],
    client_nonce: &[u8; CLIENT_NONCE_SIZE],
    proof: &[u8; PROOF_SIZE],
    binding: Option<&[u8; BINDING_SIZE]>,
) -> bool {
    constant_time_eq(&server_proof(key, server_nonce, client_nonce, binding), proof)
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
/// Not true encryption — prevents casual reading of the password. The key is in
/// this file, so anyone with the source can reverse it: treat an obfuscated
/// value as readable, never as protected.
/// Uses XOR with a fixed key, hex-encoded.
const OBFUSCATE_KEY: &[u8] = b"ThetisLink-PSK-2026";

pub fn obfuscate_password(password: &str) -> String {
    let bytes: Vec<u8> = password.as_bytes().iter().enumerate()
        .map(|(i, &b)| b ^ OBFUSCATE_KEY[i % OBFUSCATE_KEY.len()])
        .collect();
    // Hex, two characters per byte.
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// The value [`obfuscate_password`] wrote, or `None` for anything else: an odd
/// length, a character that is not a hex digit, or bytes that are no text. The
/// callers then take the value as typed - a password put into the file by hand
/// (build 137: this panicked on an odd length and turned non-hex pairs into
/// noise). A typed value that is hex and decodes to text (`1234`, read as
/// `F\`) still reads as stored; nothing in the file tells the two apart.
pub fn deobfuscate_password(encoded: &str) -> Option<String> {
    if encoded.len() % 2 != 0 || !encoded.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let bytes: Vec<u8> = (0..encoded.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&encoded[i..i + 2], 16))
        .collect::<Result<_, _>>()
        .ok()?;
    let decoded: Vec<u8> = bytes.iter().enumerate()
        .map(|(i, &b)| b ^ OBFUSCATE_KEY[i % OBFUSCATE_KEY.len()])
        .collect();
    String::from_utf8(decoded).ok()
}

// --- TOTP 2FA ---

/// TOTP configuration
const TOTP_PERIOD: u64 = 30; // seconds
const TOTP_DIGITS: u32 = 6;

/// The alphabet of every 2FA key: the one place that says it, for the key that
/// is made and every place that reads one (review 130: it was written out four
/// times, and one word changed in one of them let a key through that no code
/// could be made from, with the suite green).
fn totp_alphabet() -> base32::Alphabet {
    base32::Alphabet::Rfc4648 { padding: false }
}

/// The bytes of a 2FA key, or `None` when it is no key: it does not decode, or
/// it is empty - an empty key makes codes anyone can work out (review 128). The
/// one answer to "is this a usable key", for the start gate and the verifier
/// alike.
fn totp_key_bytes(secret_base32: &str) -> Option<Vec<u8>> {
    base32::decode(totp_alphabet(), secret_base32).filter(|bytes| !bytes.is_empty())
}

/// Generate a random 20-byte TOTP secret, returned as base32 string.
pub fn generate_totp_secret() -> String {
    use rand::RngCore;
    let mut secret = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut secret);
    base32::encode(totp_alphabet(), &secret)
}

/// Whether `secret_base32` is a 2FA key that codes can be made from at all. One
/// that is not starts a server that accepts no code - nobody gets in, the owner
/// neither, and nothing says why (review 129).
pub fn totp_secret_decodes(secret_base32: &str) -> bool {
    totp_key_bytes(secret_base32).is_some()
}

/// Generate an otpauth:// URI for QR code generation.
/// Format: otpauth://totp/ThetisLink?secret=BASE32SECRET&issuer=ThetisLink
pub fn totp_uri(secret_base32: &str) -> String {
    format!(
        "otpauth://totp/ThetisLink?secret={}&issuer=ThetisLink&digits={}&period={}",
        secret_base32, TOTP_DIGITS, TOTP_PERIOD
    )
}

/// The code `verify_totp` expects right now.
///
/// The counterpart to `verify_totp`, and the reason it exists is testing: without a
/// generator nothing could exercise the accepted-code path, and a missing step there
/// went unnoticed until a reviewer read it. Returns an empty string for a secret that
/// does not decode, which `verify_totp` rejects.
pub fn generate_totp(secret_base32: &str) -> String {
    let Some(secret_bytes) = totp_key_bytes(secret_base32) else {
        return String::new();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    totp_lite::totp_custom::<totp_lite::Sha1>(TOTP_PERIOD, TOTP_DIGITS, &secret_bytes, now)
}

/// Verify a TOTP code. Accepts current period and one period before/after (clock skew tolerance).
pub fn verify_totp(secret_base32: &str, code: &str) -> bool {
    if code.len() != TOTP_DIGITS as usize { return false; }
    // An empty key is refused here too, not only at the start gate: the
    // verifier protects itself (review 130).
    let Some(secret_bytes) = totp_key_bytes(secret_base32) else {
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

    /// Without a binding every value is what it was before phase 3 - pinned
    /// against a second, independent computation, not against itself.
    #[test]
    fn without_a_binding_nothing_changes() {
        let nonce = [3u8; NONCE_SIZE];
        let mut mac = HmacSha256::new_from_slice(b"password1").unwrap();
        mac.update(&nonce);
        let plain: [u8; HMAC_SIZE] = mac.finalize().into_bytes().into();
        assert_eq!(compute_hmac("password1", &nonce, None), plain);
        let nc = [5u8; CLIENT_NONCE_SIZE];
        let mut salt = [0u8; NONCE_SIZE + CLIENT_NONCE_SIZE];
        salt[..NONCE_SIZE].copy_from_slice(&nonce);
        salt[NONCE_SIZE..].copy_from_slice(&nc);
        let mut key = [0u8; SESSION_KEY_SIZE];
        hkdf::Hkdf::<Sha256>::new(Some(&salt), b"password1").expand(SESSION_KEY_INFO, &mut key).unwrap();
        assert_eq!(session_key("password1", &nonce, &nc, None), key);
    }

    /// A binding enters all three values, and only the same binding verifies:
    /// a party in the middle holds another one (phase 3, review 108 point 4).
    #[test]
    fn a_binding_enters_every_value_and_only_the_same_one_verifies() {
        let (nonce, nc) = ([3u8; NONCE_SIZE], [5u8; CLIENT_NONCE_SIZE]);
        let (a, b) = ([1u8; BINDING_SIZE], [2u8; BINDING_SIZE]);
        let h = compute_hmac("password1", &nonce, Some(&a));
        assert!(verify_hmac("password1", &nonce, &h, Some(&a)));
        assert!(!verify_hmac("password1", &nonce, &h, Some(&b)), "another binding");
        assert!(!verify_hmac("password1", &nonce, &h, None), "no binding");
        assert!(!verify_hmac("password1", &nonce, &compute_hmac("password1", &nonce, None), Some(&a)), "an unbound answer on a bound channel");
        let (ka, kb, kn) = (
            session_key("password1", &nonce, &nc, Some(&a)),
            session_key("password1", &nonce, &nc, Some(&b)),
            session_key("password1", &nonce, &nc, None),
        );
        assert!(ka != kb && ka != kn, "the key depends on the binding");
        let p = server_proof(&ka, &nonce, &nc, Some(&a));
        assert!(verify_server_proof(&ka, &nonce, &nc, &p, Some(&a)));
        assert!(!verify_server_proof(&ka, &nonce, &nc, &p, Some(&b)), "proof for another binding");
        assert!(!verify_server_proof(&ka, &nonce, &nc, &p, None), "proof without the binding");
        assert_ne!(server_proof(&ka, &nonce, &nc, Some(&a)), server_proof(&ka, &nonce, &nc, None), "the proof itself covers it");
    }

    /// The wire format, pinned against values computed by a separate
    /// implementation (Python's hmac and a hand-written HKDF), not by this code:
    /// label, order and the shape of the salt. A server of one build and a client
    /// of another must agree on these bytes (review 109).
    #[test]
    fn the_derivations_match_known_answers() {
        fn hex(b: &[u8]) -> String {
            b.iter().map(|x| format!("{x:02x}")).collect()
        }
        let pw = "kat-password1";
        let ns: [u8; NONCE_SIZE] = std::array::from_fn(|i| i as u8);
        let nc: [u8; CLIENT_NONCE_SIZE] = std::array::from_fn(|i| (16 + i) as u8);
        let b: [u8; BINDING_SIZE] = std::array::from_fn(|i| (32 + i) as u8);
        assert_eq!(hex(&compute_hmac(pw, &ns, None)), "f0a4b00f30df6f63f62304066c740b74333a0f543ba192149a5c62f63faadacb");
        assert_eq!(hex(&compute_hmac(pw, &ns, Some(&b))), "5a7234aad13879845b688f8d10428f7c0e2b51a5d60717091f4f83f5c26114ed");
        let (k0, kb) = (session_key(pw, &ns, &nc, None), session_key(pw, &ns, &nc, Some(&b)));
        assert_eq!(hex(&k0), "889564c573e87ce78da03477620a574970e306177a611d93acce9cf08204eb96");
        assert_eq!(hex(&kb), "a1ed21bb10d333faa6bd72bd0f972e47fb75ff819ec1a03e364044069cbddc35");
        assert_eq!(hex(&server_proof(&k0, &ns, &nc, None)), "780d3bbe03e8a8abab09998a15f3845b0f3eae76ca492f18cc5ab5182836d4a0");
        assert_eq!(hex(&server_proof(&kb, &ns, &nc, Some(&b))), "3eacd230de3eba4ee70c267f7730d0733e3ce888ce819c4fa6cfd3cc7b912d6a");
    }

    #[test]
    fn hmac_roundtrip() {
        let nonce = generate_nonce();
        let password = "test_password_123";

        let hmac = compute_hmac(password, &nonce, None);
        assert!(verify_hmac(password, &nonce, &hmac, None));
    }

    #[test]
    fn wrong_password_rejected() {
        let nonce = generate_nonce();
        let hmac = compute_hmac("correct_password", &nonce, None);
        assert!(!verify_hmac("wrong_password", &nonce, &hmac, None));
    }

    #[test]
    fn wrong_nonce_rejected() {
        let nonce1 = generate_nonce();
        let nonce2 = generate_nonce();
        let hmac = compute_hmac("password", &nonce1, None);
        assert!(!verify_hmac("password", &nonce2, &hmac, None));
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

    /// A value that is not a stored (obfuscated) one is `None`, never a panic and
    /// never a few bytes of it: every caller then takes the value as typed, as
    /// their comment says - a password put into the file by hand. Until build
    /// 137 an odd length or a non-ASCII character panicked at start, and an even
    /// length with non-hex pairs came back as noise (`secret99` as one byte).
    #[test]
    fn a_value_that_is_not_obfuscated_is_none() {
        for typed in ["probe-token", "secret99", "wachtwoord", "caf\u{e9}", "abc", "zz", " 12", "12 "] {
            assert_eq!(deobfuscate_password(typed), None, "{typed:?}");
        }
        // Hex that does not decode to text is not a stored value either - the
        // last of the three checks, which no input above reached (review 137).
        assert_eq!(deobfuscate_password("cafe1234"), None, "hex that is no text");
        // The case that remains, kept in sight: a typed value that is hex AND
        // decodes to text reads as stored. Nothing in the file tells them apart.
        assert_eq!(deobfuscate_password("1234").as_deref(), Some("F\\"));
        // And what this file writes still reads back, whatever it holds.
        for stored in ["MySecretHamKey!123", "caf\u{e9} \u{2014} ok", "x"] {
            assert_eq!(deobfuscate_password(&obfuscate_password(stored)).as_deref(), Some(stored));
        }
    }
}

#[cfg(test)]
mod session_key_tests {
    use super::*;

    /// RFC 5869, test case 1: the one vector that proves this is HKDF and not
    /// something that merely looks like it.
    #[test]
    fn hkdf_matches_rfc5869_case_1() {
        let ikm = [0x0bu8; 22];
        let salt: Vec<u8> = (0x00u8..=0x0c).collect();
        let info: Vec<u8> = (0xf0u8..=0xf9).collect();
        let mut okm = [0u8; 42];
        hkdf_sha256(&ikm, &salt, &info, &mut okm);
        let hex: String = okm.iter().map(|b| format!("{:02x}", b)).collect();
        assert_eq!(
            hex,
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
        );
    }

    /// The proof is worthless unless it binds everything the key binds.
    #[test]
    fn the_proof_binds_both_nonces_and_the_password() {
        let ns = [1u8; NONCE_SIZE];
        let nc = [2u8; CLIENT_NONCE_SIZE];
        let key = session_key("secret99", &ns, &nc, None);
        let proof = server_proof(&key, &ns, &nc, None);
        assert!(verify_server_proof(&key, &ns, &nc, &proof, None));
        assert!(
            !verify_server_proof(&session_key("secret98", &ns, &nc, None), &ns, &nc, &proof, None),
            "another password"
        );
        assert!(!verify_server_proof(&key, &[3u8; NONCE_SIZE], &nc, &proof, None), "another server nonce");
        assert!(!verify_server_proof(&key, &ns, &[3u8; CLIENT_NONCE_SIZE], &proof, None), "another client nonce");
    }

    /// Both sides compute the same key, and no two sessions share one.
    #[test]
    fn a_session_key_is_new_for_every_pair_of_nonces() {
        let ns = [1u8; NONCE_SIZE];
        assert_eq!(session_key("pw", &ns, &[2u8; 16], None), session_key("pw", &ns, &[2u8; 16], None));
        assert_ne!(session_key("pw", &ns, &[2u8; 16], None), session_key("pw", &ns, &[3u8; 16], None));
        assert_ne!(session_key("pw", &ns, &[2u8; 16], None), session_key("pw", &[9u8; 16], &[2u8; 16], None));
        assert_ne!(generate_client_nonce(), generate_client_nonce());
    }
}

/// The start gate's answer and the verifier's agree, for every kind of key an
/// operator can have (review 130: the two were apart, and the suite was green).
#[cfg(test)]
mod totp_key_tests {
    use super::{generate_totp, generate_totp_secret, totp_secret_decodes, verify_totp};

    #[test]
    fn the_gate_and_the_verifier_agree_on_every_kind_of_key() {
        let generated = generate_totp_secret();
        let keys = [
            generated.clone(),
            generated.to_lowercase(),
            "1".repeat(32),
            "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567".to_string(),
            "ABCDEFGHIJKLMNOPQRSTUVWXYZ23456=".to_string(),
            String::new(),
        ];
        for key in &keys {
            let code = generate_totp(key);
            let usable = !code.is_empty() && verify_totp(key, &code);
            assert_eq!(totp_secret_decodes(key), usable, "{key:?}");
        }
        assert!(totp_secret_decodes(&generated), "a generated key is usable");
        // The alphabet itself, which the app shares with the authenticator app
        // through `otpauth://`: base32 without padding (review 131 - the rows
        // above only hold that the gate and the verifier agree).
        assert!(!generated.contains('='), "a generated key carries no padding");
        assert!(!totp_secret_decodes("ABCDEFGHIJKLMNOPQRSTUVWXYZ23456="), "a padded key is refused");
        assert!(!verify_totp("", &generate_totp("")), "an empty key gives no code anyone can use");
        assert!(!verify_totp("", "000000"), "and no code passes for it");
    }
}
