// SPDX-License-Identifier: GPL-2.0-or-later

//! Outbound relay transport monitor for ThetisLink.
//!
//! Phase C-light establishes the WebSocket relay path and exchanges lightweight
//! peer-status/RTT frames. It still does not route TL audio/control frames, so
//! the existing direct transport is unaffected.

use futures_util::{SinkExt, StreamExt};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::MaybeTlsStream;

const RECONNECT_DELAY: Duration = Duration::from_secs(5);
/// The spread on that wait, either way: after a VPS restart the endpoints do not
/// all knock in step (relay recovery design, 2026-09-08, section 7).
const RECONNECT_SPREAD: f64 = 0.2;
/// A session must have stood this long, counted from its first traffic, before
/// its breaking earns an immediate retry (round 95: a line that passes one frame
/// and dies would otherwise reconnect two or three times a second).
const STOOD_AT_LEAST: Duration = Duration::from_secs(1);
/// After a working session broke, the next attempts that fail INSIDE this device
/// - no route, no DNS yet: the new network is not up - try again on this ladder
/// before the 5 s (measured 2026-10-03, build 95: three of five fast attempts
/// failed within 40-160 ms on exactly that). Such a failure never reaches the
/// relay, so the ladder costs it nothing.
const LOCAL_RETRY_LADDER: [Duration; 4] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];
const PEER_WAIT_PING: Duration = Duration::from_secs(10);
const PEER_STATUS_INTERVAL: Duration = Duration::from_secs(5);
const PEER_RTT_INTERVAL: Duration = Duration::from_secs(15);
/// Revert to "waiting for peer" when no peer frame arrives within this window.
/// The peer sends a status frame every PEER_STATUS_INTERVAL, so this tolerates
/// roughly three missed status heartbeats before the status is downgraded.
const PEER_TIMEOUT: Duration = Duration::from_secs(15);

/// Binary transport probe: prove that Message::Binary frames round-trip
/// peer<->peer through the relay and measure their RTT on an audio-sized
/// payload. Latency is the key open question before any real audio is routed
/// through a WebSocket/TCP relay, so we measure it early. Still status-only:
/// no TL frames are routed.
const BINARY_PROBE_INTERVAL: Duration = Duration::from_secs(1); // 1 Hz sampling for jitter
const BINARY_RTT_WINDOW: usize = 30; // rolling window (~30 s) for min/avg/max
const BINARY_PROBE_LEN: usize = 256; // representative audio-frame payload size
const BINARY_MAGIC: [u8; 4] = *b"TLB1";
const BINARY_KIND_PING: u8 = 0;
const BINARY_KIND_PONG: u8 = 1;

// TLT1 = tunneled TL-frame data (Phase C), distinct from the TLB1 probe.
// Layout: magic(4) | version(1) | client_id(1) | length(2 LE) | payload(TL-bytes).
// A dedicated magic (rather than a bare prefix) is robust because real TL payloads
// are binary too; a binary frame with an unknown magic is dropped non-fatally.
const TLT1_MAGIC: [u8; 4] = *b"TLT1";
const TLT1_VERSION: u8 = 1;
const TLT1_HEADER_LEN: usize = 8;

// TLU1 = tunneled audio over the bare-UDP path (PATCH-relay-audio-udp, phase 2).
// Layout: magic(4) | version(1) | flags(1) | client_id(1) | seq(4 LE) | token(32 raw) |
// AudioPacket. Only audio packet types travel over this; everything else stays on TLT1/wss.
const TLU1_MAGIC: [u8; 4] = *b"TLU1";
const TLU1_VERSION: u8 = 1;
const TLU1_TOKEN_LEN: usize = 32;
/// The TLU1 header in front of every datagram to the relay's UDP port. Public for
/// the QUIC route layer (`sdr-remote-quic`, phase 3b), which frames its packets with
/// [`encode_tlu1_raw`] - one encoder for both data forms.
pub const TLU1_HEADER_LEN: usize = 4 + 1 + 1 + 1 + 4 + TLU1_TOKEN_LEN; // = 43

/// Opus-audio packet types (byte [2] of the TL header) that should travel over
/// the low-latency UDP path. Everything else (control, spectrum, meters) stays
/// on wss. The relay stays dependency-light and does NOT link `sdr-remote-core`
/// (which would pull in audiopus/opus), so this list is a deliberate mirror of
/// `sdr_remote_core::protocol::AUDIO_PACKET_TYPES` — kept honest by a parity
/// test in the server crate (`tracked_socket`/tests), which links both crates
/// and fails the build if this ever drifts from the authoritative enum.
pub const AUDIO_TYPE_BYTES: [u8; 7] = [0x01, 0x0E, 0x16, 0x1A, 0x1B, 0x21, 0x25];

fn is_audio_packet(data: &[u8]) -> bool {
    data.len() >= 4 && AUDIO_TYPE_BYTES.contains(&data[2])
}

/// Decode a 64-char hex token into 32 raw bytes (best-effort; bad chars -> 0).
fn hex32(hex: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        if let Some(pair) = hex.get(i * 2..i * 2 + 2) {
            *byte = u8::from_str_radix(pair, 16).unwrap_or(0);
        }
    }
    out
}

/// Build a TLU1 audio datagram. `client_id` is the sender's id (station stamps the
/// target). `token_hex` is the phase-0 capability from the ready-reply.
fn encode_tlu1(client_id: u8, seq: u32, token_hex: &str, payload: &[u8]) -> Vec<u8> {
    encode_tlu1_raw(client_id, seq, &hex32(token_hex), payload)
}

/// Build a TLU1 datagram from the raw 32-byte token. The relay forwards the payload
/// without reading it; to the station it arrives as `[client_id] + payload`.
pub fn encode_tlu1_raw(client_id: u8, seq: u32, token: &[u8; TLU1_TOKEN_LEN], payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(TLU1_HEADER_LEN + payload.len());
    f.extend_from_slice(&TLU1_MAGIC);
    f.push(TLU1_VERSION);
    f.push(0); // flags (reserved)
    f.push(client_id);
    f.extend_from_slice(&seq.to_le_bytes());
    f.extend_from_slice(token);
    f.extend_from_slice(payload);
    f
}

/// Extract `udp_token=<hex>` from the relay ready-reply (phase 0). The token is the
/// last field, so trimming the remainder is enough.
/// The chat ticket from a relay line, if one is offered.
///
/// Two lines carry it: the ready-reply at registration, and the periodic
/// `chat-ticket-rotate` push. Both are read the same way - by name, so a line
/// that gains another field later still parses, and a relay that offers no
/// ticket at all (no chat behind it) simply yields `None`.
/// Whether a relay is actually set up to run, as opposed to merely written down.
///
/// All four have to be there before anything is tried, so this is the one rule
/// that decides whether a front end builds its tunnel - and, just as importantly,
/// whether the chat has a relay to reach at all. Kept in one place because when
/// the two drifted apart, a client with the relay switched off but an address
/// still in its settings was told "this relay offers no chat", blaming a relay it
/// had never contacted, where the honest answer was that none was configured.
pub fn is_configured(enabled: bool, url: &str, station: &str, token: &str) -> bool {
    enabled && !url.trim().is_empty() && !station.trim().is_empty() && !token.trim().is_empty()
}

/// The relay address the chat may work with: the configured one when the relay is
/// actually set up, and nothing at all otherwise. The raw address stays available
/// separately, because a problem report must redact a host that is written down
/// whether or not it is in use.
pub fn chat_relay_url<'a>(
    enabled: bool,
    url: &'a str,
    station: &str,
    token: &str,
) -> &'a str {
    if is_configured(enabled, url, station, token) {
        url
    } else {
        ""
    }
}

/// Where the chat lives, given a relay address.
///
/// The relay is reached over `wss://`; the chat is the same host over `https://`
/// behind a `/chat` route. Kept here rather than in either front end, because
/// both need it and two spellings of the same host is how one of them ends up
/// talking to nothing.
pub fn chat_endpoint(relay_url: &str) -> Option<String> {
    let t = relay_url.trim();
    if t.is_empty() {
        return None;
    }
    let host = t
        .strip_prefix("wss://")
        .or_else(|| t.strip_prefix("ws://"))
        .or_else(|| t.strip_prefix("https://"))
        .or_else(|| t.strip_prefix("http://"))
        .unwrap_or(t);
    let host = host.split('/').next().unwrap_or(host).trim();
    if host.is_empty() {
        return None;
    }
    Some(format!("https://{host}/chat"))
}

fn parse_chat_ticket(line: &str) -> Option<String> {
    line.split("chat_ticket=")
        .nth(1)
        .map(|s| s.split_whitespace().next().unwrap_or("").to_string())
        // Shape check only: `<hex>.<hex>`. The signature is the chat's business,
        // not ours - this client never verifies it, it only carries it.
        .filter(|t| {
            let mut parts = t.split('.');
            match (parts.next(), parts.next(), parts.next()) {
                (Some(p), Some(sig), None) => {
                    !p.is_empty()
                        && !sig.is_empty()
                        && p.len() % 2 == 0
                        && p.chars().all(|c| c.is_ascii_hexdigit())
                        && sig.chars().all(|c| c.is_ascii_hexdigit())
                }
                _ => false,
            }
        })
}

fn parse_udp_token(ready: &str) -> Option<String> {
    ready
        .split("udp_token=")
        .nth(1)
        .map(|s| s.split_whitespace().next().unwrap_or("").to_string())
        .filter(|t| t.len() == TLU1_TOKEN_LEN * 2)
}

// --- Phase 3: make-before-break UDP<->wss fallback (relay-only) ---

/// Relay-control-frame marker. A client tunnels this over wss (TLT1, so the relay stamps
/// the sender's client_id); the station intercepts it BEFORE forwarding to the engine and
/// flips that client's wss-audio fallback. Distinct from any TL packet: TL payloads start
/// with the TL magic, never with this 4-byte marker.
const RCF_MAGIC: &[u8; 4] = b"RCF1";
/// RCF opcode: request wss-audio fallback on/off for the sender's session (byte 5).
const RCF_FALLBACK: u8 = 0x01;

/// RCF opcode: the sender announces what it can do (byte 5 = a bitmask).
///
/// Deliberately its OWN opcode and not a spare bit in the fallback frame's value
/// byte. `parse_rcf_fallback` reads that byte as `!= 0`, so a capability bit set
/// in an "off" frame would read as "put me on wss" to any station built before
/// this - permanently, and the off frame is exactly the one that has to carry it.
/// With an opcode of its own that parser returns None and an older station ignores
/// the frame without noticing it.
const RCF_CAPS: u8 = 0x02;

/// Capability bit: this endpoint understands a bare downlink beacon and uses it to
/// judge the UDP path. A station only stops sending UDP audio to a client that has
/// said this - not to one it merely beacons at, which is a different fact.
const CAPS_READS_BEACON: u8 = 0x01;
/// Connect-time fallback: if UDP audio NEVER arrives within this window while the tunnel is
/// alive, UDP is likely blocked -> request wss audio.
const FALLBACK_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the DOWNLINK may be silent before the path counts as broken.
///
/// `FALLBACK_STALL` used to sit above this, at 500ms, and the design said to keep it
/// because the audio path still needed it. That was wrong: the audio path uses a bare
/// `Duration::from_millis(500)` of its own for a different question - "has this stream
/// resumed after a quiet spell", for a debug line - and the constant's only real reader
/// was the diagnostic that cried wolf on a healthy link. With that reader gone it was
/// dead code carrying a claim about where it was used, which is worse than no constant
/// at all. Removed; the two literals are a smaller matter, still visible, deliberately
/// not renamed into this one because they mean something else.
///
/// This one guards a
/// beacon that arrives once a second, so anything under a second would call a
/// healthy path broken between two beacons - and anything at exactly a second
/// would do it on the first late one.
///
/// It also has to sit BELOW `FALLBACK_RECOVER`: the healthy run restarts on a gap
/// wider than this, so with a threshold above the recovery window a run could
/// never grow long enough to recover. Beacon 1s < this 2.5s < recover 3s.
const FALLBACK_DOWNLINK_STALL: Duration = Duration::from_millis(2500);

/// How often the UDP keepalive goes out - the client's to its station, the station's
/// to each client it knows, where it doubles as the beacon that proves the downlink
/// is alive.
///
/// A name rather than a literal at the send site, because two tests were guarding the
/// three-way ordering that everything here depends on and BOTH declared their own
/// copy of this number. Putting the real one to three seconds left them green while
/// every beacon would then restart the healthy run and recovery would be impossible -
/// exactly the failure those tests exist to prevent. A review found it; a constant is
/// what makes them able to see it.
const UDP_BEACON_INTERVAL: Duration = Duration::from_secs(1);
/// Recovery: while in fallback, UDP audio must flow healthily for this long before we drop
/// wss and return to UDP-only. Longer than the stall threshold = hysteresis (no flapping).
const FALLBACK_RECOVER: Duration = Duration::from_secs(3);
/// Minimum dwell in a transport state before another flip is allowed (dwell-cap +
/// rate-limit combined). Enforced on BOTH ends so a client that ignores its own dwell
/// still cannot make the station flap.
const FALLBACK_DWELL: Duration = Duration::from_secs(3);
/// Whether UDP has demonstrably recovered: the healthy run must be long enough
/// AND still be running.
///
/// Both halves are needed. `healthy_since` marks when the current run of UDP
/// audio began and is never cleared, so on a path that has simply died it keeps
/// satisfying "at least FALLBACK_RECOVER old" forever - which read as "recovered"
/// on every pass and produced a flip every dwell period without end (observed at
/// 1829 flips, 96 minutes after the last UDP audio frame). The second half asks
/// whether audio is arriving *now*.
fn udp_recovered(
    healthy_since: Option<Instant>,
    last_udp_audio: Option<Instant>,
    recover_after: Duration,
    stall_after: Duration,
) -> bool {
    healthy_since.map_or(false, |t| t.elapsed() >= recover_after)
        && last_udp_audio.map_or(false, |t| t.elapsed() < stall_after)
}

/// Target id for a datagram that exists only to hold the station's NAT mapping
/// open. Delivery is not the point; departure is: the relay validates it and
/// refreshes our source binding.
///
/// Id 0 is NOT unused. An earlier version of this comment said the relay hands
/// out ids 1..=254; it hands out 0..=7 (`alloc_client_id` in the relay's
/// main.rs), so a client can hold 0. The relay then forwards this keepalive to
/// that client as a 1-byte datagram, which the receiver's `n > 1` guard drops.
/// Harmless today - but do not build on 0 meaning "nobody".
pub const UDP_MAPPING_KEEPALIVE_ID: u8 = 0;

/// What one round of UDP keepalives is, for one side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeepaliveKind {
    /// A client's one datagram towards the station.
    Client,
    /// A station's datagrams to its bound clients - the beacons their fallback reads.
    Beacons,
    /// A station with no client bound: one datagram, only to hold the NAT mapping.
    MappingOnly,
}

/// The keepalive datagrams for one round, carrying `token_hex`.
///
/// One place for both callers: the 1 s timer, and a token rotation. The rotation
/// sends them AT ONCE, so the relay learns our source on the new token straight
/// away instead of at the next tick. Until then a new token has no source, and the
/// relay's lookup could pick it and drop the audio - the ~1 s gaps every ten
/// minutes, at the client's connect time + 10n min, in the field log of
/// 2026-09-29.
fn keepalive_datagrams(
    role: RelayRole,
    bound_clients: &HashSet<u8>,
    token_hex: &str,
    udp_seq: &mut u32,
) -> (Vec<Vec<u8>>, KeepaliveKind) {
    let (targets, kind) = match role {
        // A client has one peer, and id 0 means "the station" on the way up.
        RelayRole::Client => (vec![0], KeepaliveKind::Client),
        RelayRole::Station if bound_clients.is_empty() => {
            (station_udp_targets(bound_clients), KeepaliveKind::MappingOnly)
        }
        RelayRole::Station => (station_udp_targets(bound_clients), KeepaliveKind::Beacons),
    };
    let datagrams = targets
        .into_iter()
        .map(|cid| {
            *udp_seq = udp_seq.wrapping_add(1);
            encode_tlu1(cid, *udp_seq, token_hex, &[])
        })
        .collect();
    (datagrams, kind)
}

/// Which ids the station addresses on this UDP tick.
///
/// With clients bound: one datagram each - those are the beacons the client's
/// fallback logic reads.
///
/// With NONE bound: still exactly one. Until 2026-09-23 this sent nothing at all,
/// and that is a trap, because the relay binds a token to the source address it
/// first saw (`main.rs`, S5). A station with no clients stops sending, its NAT
/// mapping expires, and when the next client arrives the station resumes from a
/// NEW source port - which no longer matches the binding. Every beacon is then
/// dropped silently until the token rotates, up to ten minutes later.
///
/// Measured that day: 250 s of station silence, then four minutes in which the
/// server sent 4-5 beacons per five seconds and the client received exactly none,
/// ending two seconds after the relay rotated the token.
fn station_udp_targets(bound_clients: &HashSet<u8>) -> Vec<u8> {
    if bound_clients.is_empty() {
        return vec![UDP_MAPPING_KEEPALIVE_ID];
    }
    let mut ids: Vec<u8> = bound_clients.iter().copied().collect();
    ids.sort_unstable(); // deterministic, so a test can say what it expects
    ids
}

/// Has the downlink stalled - the ENTRY side of the fallback.
///
/// Free and testable on purpose. The exit side above has been a free function since
/// it was written and a test has caught a defect in it twice; this side lived inline
/// in the connection loop, where it could be reverted to the shape that caused the
/// original defect with every test in the workspace still green. That asymmetry ran
/// through the whole dossier and it ends here.
///
/// The argument order deliberately does NOT mirror `udp_recovered`. Two neighbouring
/// functions with the same three trailing types is a swap that compiles.
fn downlink_stalled(
    since_peer_ready: Option<Duration>,
    last_downlink: Option<Instant>,
    connect_timeout: Duration,
    stall_after: Duration,
) -> bool {
    match last_downlink {
        // Nothing has ever come down this path, so only the connect window can
        // decide - but it may only start counting once there IS a peer.
        //
        // It used to count from `ready_at`, the moment the relay tunnel stood.
        // The peer handshake comes after that, and on 2026-09-23 it came six
        // seconds after: the client declared "UDP blocked" at 17:28:48 while the
        // tunnel client was bound at 17:28:52 and the session authenticated at
        // 17:28:52.115. It was measuring a window in which no peer could have
        // sent anything, and it did that on every fresh connect - which is why
        // the owner saw the indicator "continuously".
        //
        // `None` here means there is no peer yet. Then there is nothing to
        // accuse: silence from someone who is not there is not evidence.
        None => since_peer_ready.map_or(false, |d| d >= connect_timeout),
        // Something did: a beacon at its normal spacing must NOT read as a stall.
        Some(t) => t.elapsed() >= stall_after,
    }
}

/// What the absence-reporter should say this pass, if anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AbsenceEvent {
    /// Say nothing.
    Nothing,
    /// Audio has been gone long enough to be worth one line, this long.
    ///
    /// The duration travels WITH the event rather than being worked out again by the
    /// caller: the opening line prints it and the latch is set from it, and those two
    /// drifting apart is how the closing line came to under-report every episode by a
    /// whole threshold.
    Began(Duration),
    /// It is over because audio came back.
    EndedAudioBack,
    /// It is over because the PATH broke, and the rescue is taking it from here.
    ///
    /// Its own variant, because the first version had only one ending and wrote
    /// "UDP audio is back" for both. A review caught it: the path stalling makes the
    /// condition false just as surely as audio returning, so the line claiming audio
    /// was back appeared on the very pass where the fallback fires - two adjacent
    /// lines, the first one the exact opposite of the truth, at the one moment
    /// somebody reads the log.
    EndedPathBroken,
}

/// Whether the absence of audio is worth opening or closing a report on.
///
/// Free and testable because every fault in this area has been in the wiring rather
/// than the comparison, and nothing could reach the wiring: `run_connection` is
/// called once, from production, and no test touches it. So a mutation anywhere in
/// the block this replaces survived the whole suite by construction - which is a
/// stronger statement than any mutation run, and it came from a review.
/// Takes the CLOCKS, not a duration worked out above it, and that is the point.
///
/// The first cut took `without_audio` ready-made, so the projection that produces it -
/// "no audio at all means measure from the start of the session" - stayed above the
/// function where no test could reach it. A review demonstrated it: swapping the audio
/// clock for the downlink clock in that projection turns the reporter into the very
/// thing it was written to find, a branch that is there and can never be reached, and
/// every test stayed green. Here the projection is inside, and that case is the first
/// test below.
fn absence_edge(
    latched: bool,
    stalled: bool,
    since_audio: Option<Duration>,
    since_ready: Duration,
) -> AbsenceEvent {
    // No audio at all in this session: the absence is as old as the session.
    let without_audio = since_audio.unwrap_or(since_ready);
    let explaining = audio_absent_while_path_alive(stalled, without_audio);
    match (explaining, latched) {
        (true, false) => AbsenceEvent::Began(without_audio),
        (false, true) if stalled => AbsenceEvent::EndedPathBroken,
        (false, true) => AbsenceEvent::EndedAudioBack,
        _ => AbsenceEvent::Nothing,
    }
}

/// Is there a situation worth explaining: no audio, while the path says it is alive?
///
/// The threshold is `FALLBACK_DOWNLINK_STALL` and it is deliberately NOT a parameter.
/// This question is defined relative to the decision it explains - below that window
/// the rescue could not have acted either, so there is nothing to explain and a line
/// would only be noise. Leaving the caller free to pass something else is how the
/// previous version came to compare against 500ms, which at 20ms per frame is a single
/// late frame, and it duly cried wolf on a healthy link three times in an afternoon.
///
/// A parameter here also cannot be defended by a test: a test calls this function with
/// the right value while the call site passes the wrong one, and stays green. That
/// exact shape has been found in this file three times now. One caller, one correct
/// threshold, no way to get it wrong.
fn audio_absent_while_path_alive(stalled: bool, without_audio: Duration) -> bool {
    !stalled && without_audio >= FALLBACK_DOWNLINK_STALL
}

/// Which paths a station uses for one client's audio frame: `(udp, wss)`.
///
/// This is the decision the whole repair exists for, and its lease-expiry case is
/// invisible from every parser test - so it is free and tested rather than inline.
///
/// A client is only taken off UDP when it can NOTICE the path coming back, which
/// means reading a beacon. Being SENT a beacon is a different fact: a client built
/// after the fallback but before the beacon would ask for wss and then wait for
/// audio that never returns.
fn station_audio_paths(
    udp_up: bool,
    fallback_on: bool,
    lease_age: Duration,
    lease: Duration,
    reads_beacon: bool,
) -> (bool, bool) {
    let fb = fallback_on && lease_age < lease;
    if fb && reads_beacon {
        (false, true)
    } else {
        (udp_up, fb)
    }
}

/// While in fallback the client re-asserts its request this often (lease refresh), so
/// the station can expire the state of a client that vanished without sending an off.
/// It is also how often a client repeats its capability announcement - see the send
/// site for why that has to be repeated at all.
const FALLBACK_HEARTBEAT: Duration = Duration::from_secs(5);
/// Station drops a client's fallback state if it has not been re-asserted within this
/// window (>2 heartbeats). Prevents a stale flag from being inherited by a reconnecting
/// client that reuses the same relay client_id.
const FALLBACK_LEASE: Duration = Duration::from_secs(15);

/// Station-side per-client fallback state (keyed by relay client_id). `on` is the current
/// wss-audio decision; `last_change` enforces the dwell; `last_seen` drives the lease so a
/// vanished client's flag expires instead of leaking to a reused id.
struct StationFallback {
    on: bool,
    last_change: Instant,
    last_seen: Instant,
    /// When the current fallback episode began, and how many UDP audio frames went
    /// out during it. The line written when the episode ends carries both, so a zero
    /// arrives with the proof that the situation occurred at all. A zero on its own
    /// is an absence, not a measurement.
    on_since: Option<Instant>,
    udp_frames_while_on: u64,
    /// Whether this client told us it reads a bare downlink beacon. Default false,
    /// so a client built before the beacon keeps getting UDP audio during a
    /// fallback - it has no other way of noticing that the path came back.
    reads_beacon: bool,
}

/// Build a relay-control-frame requesting wss-audio fallback on/off.
fn encode_rcf_fallback(on: bool) -> Vec<u8> {
    let mut f = Vec::with_capacity(6);
    f.extend_from_slice(RCF_MAGIC);
    f.push(RCF_FALLBACK);
    f.push(on as u8);
    f
}

/// Build a relay-control-frame announcing our capabilities.
fn encode_rcf_caps(bits: u8) -> Vec<u8> {
    let mut f = Vec::with_capacity(6);
    f.extend_from_slice(RCF_MAGIC);
    f.push(RCF_CAPS);
    f.push(bits);
    f
}

/// Parse a capability announcement: `Some(bits)` when it is one.
fn parse_rcf_caps(payload: &[u8]) -> Option<u8> {
    if payload.len() >= 6 && &payload[0..4] == RCF_MAGIC && payload[4] == RCF_CAPS {
        Some(payload[5])
    } else {
        None
    }
}

/// Is this payload a relay-control-frame of any kind, rather than traffic for the
/// engine?
///
/// Both control frames return early on the station side, before the point where a
/// client id is added to the beacon list - so this is what says "count this client
/// anyway". After a reconnect an RCF frame is easily the first thing a client sends,
/// and the client asking for a fallback is precisely the one that must be beaconed
/// at, or it can never be told that its path came back.
fn is_relay_control_frame(payload: &[u8]) -> bool {
    parse_rcf_caps(payload).is_some() || parse_rcf_fallback(payload).is_some()
}

/// Parse a relay-control-frame: `Some(on)` when it is a wss-audio fallback request.
fn parse_rcf_fallback(payload: &[u8]) -> Option<bool> {
    if payload.len() >= 6 && &payload[0..4] == RCF_MAGIC && payload[4] == RCF_FALLBACK {
        Some(payload[5] != 0)
    } else {
        None
    }
}

/// Audio packet identity for dedup: `(stream, sequence)`. AudioPacket layout is
/// header(4: magic,ver,type,flags) + sequence(4, big-endian) + ...; type is byte[2], the
/// sequence is bytes[4..8]. Different audio types (Audio/AudioRx2/AudioVrx/...) have
/// independent sequence spaces, so the type is part of the key.
///
/// AudioVrx is the exception: BOTH virtual receivers use that one type and carry the
/// channel in the body (`vrx_id`, byte 12), while each keeps its own sequence counter.
/// Keyed on the type alone, VRX2's frame 92 looked like VRX1's frame 92 and was dropped
/// as a duplicate - one channel eating the other, for as long as their counters overlap.
/// The channel therefore joins the stream identity.
fn audio_identity(payload: &[u8]) -> Option<(u16, u32)> {
    if is_audio_packet(payload) && payload.len() >= 8 {
        let seq = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
        let ty = payload[2];
        // VRX audio = type 0x21; the channel byte only exists on that type.
        let stream = if ty == 0x21 && payload.len() > 12 {
            (ty as u16) | ((payload[12] as u16) << 8)
        } else {
            ty as u16
        };
        Some((stream, seq))
    } else {
        None
    }
}

/// Header byte[3] = flags; bit 0 marks TX. This MIRRORS `Flags::PTT` in
/// `sdr-remote-core`, deliberately: the relay stays dependency-light and does not
/// link core. A mirror without a guard is how wire constants drift apart, and this
/// one sits on the PTT path - so `ptt_bit_matches_core` in
/// `sdr-remote-server/src/tracked_socket.rs` (the one crate that links both) pins
/// the two together.
pub const PTT_FLAG_BIT: u8 = 0x01;

/// `true` when an audio frame carries an active PTT flag.
/// Used for TX-always-both (phase 3b section 14.3).
fn audio_ptt_active(payload: &[u8]) -> bool {
    is_audio_packet(payload) && payload.len() > 3 && (payload[3] & PTT_FLAG_BIT) != 0
}

/// Sliding window that drops audio frames already delivered over the other transport, so a
/// frame carried on BOTH paths (make-before-break overlap / TX-always-both) reaches the
/// receiver once. Kept in the transport layer so the jitter estimator is never perturbed
/// by duplicates (pre-buffer dedup); the jitter buffer itself stays unchanged. Only audio is
/// deduped. Keyed by (client_id, packet_type, sequence) so concurrent clients on a
/// station's uplink never collide.
struct DedupWindow {
    order: std::collections::VecDeque<(u8, u16, u32)>,
    seen: std::collections::HashSet<(u8, u16, u32)>,
    /// Highest sequence seen per (client, stream), to recognise a stream that
    /// starts over. A subscription restarts its counter at 0 while the window
    /// still holds the previous run's numbers, and without this every frame of
    /// the new run is taken for a repeat of the old one.
    max_seq: std::collections::HashMap<(u8, u16), u32>,
    /// Frames dropped as duplicates since the last report, per (client, stream).
    ///
    /// A dropped frame used to leave no trace at any log level, which made
    /// "audio is not arriving" and "audio is being thrown away" the same silence.
    /// That distinction cost two rounds of diagnosis in this very release (VRX1
    /// and VRX2 eating each other, and a restarted stream taken for its own
    /// repeat). Counting here and reporting periodically keeps the hot path free
    /// of logging while making the difference visible.
    dropped: std::collections::HashMap<(u8, u16), u64>,
}

impl DedupWindow {
    const CAP: usize = 512; // ~10 s of 50 fps audio; ample across clients + path overlap

    fn new() -> Self {
        Self {
            order: std::collections::VecDeque::with_capacity(Self::CAP + 1),
            seen: std::collections::HashSet::with_capacity(Self::CAP + 1),
            max_seq: std::collections::HashMap::new(),
            dropped: std::collections::HashMap::new(),
        }
    }

    /// Take the drop tally and reset it. Returns an empty string when nothing was
    /// dropped, so the caller can stay silent in the ordinary case.
    fn take_drop_report(&mut self) -> String {
        if self.dropped.is_empty() {
            return String::new();
        }
        let mut parts: Vec<String> = self
            .dropped
            .iter()
            .map(|((cid, ty), n)| format!("client{cid}/0x{ty:04x}:{n}"))
            .collect();
        parts.sort();
        let total: u64 = self.dropped.values().sum();
        let streams = self.dropped.len();
        self.dropped.clear();
        format!("dropped {total} duplicate frame(s) across {streams} stream(s) ({})", parts.join(", "))
    }

    /// `true` if this (client_id, audio frame) is a duplicate already delivered (drop it);
    /// `false` for a fresh frame (records it) or any non-audio payload (never deduped).
    fn is_duplicate(&mut self, cid: u8, payload: &[u8]) -> bool {
        let Some((ty, seq)) = audio_identity(payload) else {
            return false;
        };
        let stream_key = (cid, ty);
        // A stream that jumps far back has restarted (a channel switched off and
        // on begins at 0 again). Forget what we remember of it, or its fresh
        // frames are dropped as repeats of the previous run - which cost exactly
        // as much audio as that run was long.
        const RESTART_BACKJUMP: u32 = 64;
        match self.max_seq.get(&stream_key).copied() {
            Some(max) if max.wrapping_sub(seq) > RESTART_BACKJUMP
                && max.wrapping_sub(seq) < u32::MAX / 2 =>
            {
                self.order.retain(|(c, s, _)| !(*c == cid && *s == ty));
                self.seen.retain(|(c, s, _)| !(*c == cid && *s == ty));
                self.max_seq.insert(stream_key, seq);
            }
            Some(max) if seq.wrapping_sub(max) < u32::MAX / 2 => {
                self.max_seq.insert(stream_key, seq);
            }
            None => {
                self.max_seq.insert(stream_key, seq);
            }
            _ => {}
        }
        let key = (cid, ty, seq);
        if !self.seen.insert(key) {
            *self.dropped.entry(stream_key).or_insert(0) += 1;
            return true;
        }
        self.order.push_back(key);
        if self.order.len() > Self::CAP {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        false
    }
}

/// Host portion of a relay URL (`wss://host[:port][/path]` -> `host`).
fn url_host(url: &str) -> Option<String> {
    let s = url.trim();
    let s = s
        .strip_prefix("wss://")
        .or_else(|| s.strip_prefix("ws://"))
        .unwrap_or(s);
    let host = s.split(['/', ':']).next()?;
    (!host.is_empty()).then(|| host.to_string())
}

/// The relay's UDP data port: the host of the wss url, at `port`. One lookup for
/// both data forms (the audio path below, the QUIC grant).
/// The relay's UDP address: the first one its name gives, as the wss
/// connection uses it, and beside it the first IPv4 one when the first is not
/// (`first_and_ipv4`).
async fn resolve_udp(url: &str, port: u16) -> Result<(SocketAddr, Option<SocketAddr>), String> {
    let host = url_host(url).ok_or_else(|| "bad relay url".to_string())?;
    let addrs = tokio::net::lookup_host((host.as_str(), port)).await.map_err(|e| e.to_string())?;
    first_and_ipv4(addrs).ok_or_else(|| "relay UDP address not resolved".to_string())
}

/// The first address, and the first IPv4 one when that is another. On a mobile
/// net with IPv6 only, the name of a relay without an IPv6 address of its own
/// came back as an IPv6 address first (field test 119, 5G); the phone reaches
/// IPv4 there through its own translation too, which is what the direct mode
/// uses (`first_ipv4` in the engine).
fn first_and_ipv4(addrs: impl IntoIterator<Item = SocketAddr>) -> Option<(SocketAddr, Option<SocketAddr>)> {
    let mut addrs = addrs.into_iter();
    let first = addrs.next()?;
    let ipv4 = if first.is_ipv4() { None } else { addrs.find(|a| a.is_ipv4()) };
    Some((first, ipv4))
}

/// A UDP socket connected to `addr`, bound in its family. Opening it on IPv4
/// for an IPv6 address fails with "address family not supported" (os error 97
/// on Android), which is how QUIC via the relay never came up on 5G in build
/// 119: its route layer kept a copy of this that always bound IPv4.
///
/// A plain socket, on purpose: no GRO, no GSO, so every datagram carries its
/// own TLU1 header and is sent and read one by one (the QUIC route layer counts
/// on that: `max_transmit_segments` 1, `stride == len`).
pub async fn connect_udp(addr: SocketAddr) -> std::io::Result<UdpSocket> {
    let sock = bind_for(addr).await?;
    sock.connect(addr).await?;
    Ok(sock)
}

/// An unconnected UDP socket in `addr`'s family, on any port - the one place
/// that picks the family, for [`connect_udp`] and [`bind_relay_udp`].
async fn bind_for(addr: SocketAddr) -> std::io::Result<UdpSocket> {
    let bind = if addr.is_ipv6() {
        SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0))
    } else {
        SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, 0))
    };
    UdpSocket::bind(bind).await
}

/// An UNconnected socket for the relay's UDP port, for a side that talks to
/// the relay and, after punching (phase 3c), directly from the same socket -
/// so the direct packets leave under the very address the relay saw. The first
/// address in `order` that a socket can be connected to wins: binding alone
/// never fails for a family the system lacks or a network it cannot reach (the
/// failure would only come at the first send), so a throwaway connected socket
/// tries the address first, as [`connect_relay_udp`] does. Returns the address
/// it chose; the caller sends there with `send_to`.
pub async fn bind_relay_udp(order: &[SocketAddr]) -> std::io::Result<(UdpSocket, SocketAddr)> {
    let mut last = std::io::Error::other("no relay address");
    for (i, &addr) in order.iter().enumerate() {
        match connect_udp(addr).await {
            Ok(probe) => {
                drop(probe);
                return bind_for(addr).await.map(|sock| (sock, addr));
            }
            Err(e) => {
                if let Some(next) = order.get(i + 1) {
                    log::warn!(
                        "Relay UDP: no socket for the {} address ({e}) - trying the {} one",
                        family(addr),
                        family(*next)
                    );
                }
                last = e;
            }
        }
    }
    Err(last)
}

/// The one way to the relay's UDP port, for both forms: `first`, and when no
/// socket can be made for it, `fallback` (the IPv4 address beside it). Returns
/// the address it reached. A socket that opens but whose packets go nowhere is
/// not caught here; the field log is the proof for that.
pub async fn connect_relay_udp(
    first: SocketAddr,
    fallback: Option<SocketAddr>,
) -> std::io::Result<(UdpSocket, SocketAddr)> {
    match connect_udp(first).await {
        Ok(sock) => Ok((sock, first)),
        Err(e) => match fallback {
            Some(v4) => {
                log::warn!("Relay UDP: no socket for the {} address ({e}) - trying its IPv4 address", family(first));
                connect_udp(v4).await.map(|sock| (sock, v4))
            }
            None => Err(e),
        },
    }
}

/// "IPv4" or "IPv6", for log lines that must not carry the address itself.
pub fn family(addr: SocketAddr) -> &'static str {
    if addr.is_ipv4() {
        "IPv4"
    } else {
        "IPv6"
    }
}

/// Open + connect a UDP socket to the relay's UDP data port (phase 2). Best-effort;
/// on any failure the caller falls back to audio-over-wss.
async fn open_udp(url: &str, port: u16) -> Result<UdpSocket, String> {
    let (first, fallback) = resolve_udp(url, port).await?;
    let (sock, used) = connect_relay_udp(first, fallback).await.map_err(|e| e.to_string())?;
    log::info!("Relay UDP: over {}", family(used));
    Ok(sock)
}

/// Await a datagram on the UDP audio socket, or pend forever when UDP is not up (so
/// the `select!` branch is inert without busy-spinning).
async fn recv_udp(sock: &Option<UdpSocket>, buf: &mut [u8]) -> Option<usize> {
    match sock {
        Some(s) => s.recv(buf).await.ok(),
        None => std::future::pending().await,
    }
}

// Relay sentinel convention for the classic tunnel. The server mirrored it
// (`tracked_socket::relay_sentinel_for`, with a parity test) until build 133,
// which removed the classic tunnel from the server; this side goes with the
// rest of the tunnel code (phase 3d, build 3).
// Mapping: client_id N <-> 203.0.113.(N+1):1.
/// Strip the relay address out of a log line, keeping what is diagnostically
/// useful (the scheme, so ws:// vs wss:// is still visible). The host is private
/// to the operator and logs are the artefact people share when asking for help.
fn redact_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, _)) => format!("{scheme}://<relay>"),
        None => "<relay>".to_string(),
    }
}

pub const RELAY_SENTINEL_PORT: u16 = 1;

/// Synthetic per-client sentinel address for relay `client_id`.
pub fn sentinel_for_client_id(id: u8) -> SocketAddr {
    let last = 1u16 + (u16::from(id) % 254);
    SocketAddr::from((Ipv4Addr::new(203, 0, 113, last as u8), RELAY_SENTINEL_PORT))
}

/// Recover the relay `client_id` from a sentinel address (inverse of the above).
pub fn client_id_from_sentinel(addr: SocketAddr) -> u8 {
    match addr.ip() {
        IpAddr::V4(ip) => ip.octets()[3].saturating_sub(1),
        IpAddr::V6(_) => 0,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelayRole {
    Station,
    Client,
}

impl RelayRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Station => "station",
            Self::Client => "client",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelayConfig {
    pub enabled: bool,
    pub url: String,
    pub station: String,
    pub token: String,
    pub role: RelayRole,
    /// Stable per-install id sent to the relay so a reconnecting client reclaims
    /// its own slot instead of consuming a new one (prevents room-full pile-up from
    /// one device relaunching/backgrounding). Empty = omitted (relay allocates a new
    /// slot, legacy behavior).
    pub instance: String,
    /// Optional human device label (may contain spaces, e.g. "Surface Pro"), sent as
    /// the LAST hello field. Metadata only - the relay uses it just for logs. Empty
    /// = omitted.
    pub name: String,
    /// UDP port of the relay's audio data path (PATCH-relay-audio-udp, phase 2).
    /// `Some(port)` = route audio over bare UDP to `host(url):port`; `None` = all
    /// traffic over wss as before (additive). Falls back to wss if UDP can't open.
    pub udp_port: Option<u16>,
    /// QUIC via the relay (phase 3b): this side carries one QUIC connection
    /// through the relay's UDP path, framed by the QUIC crate's route layer. The
    /// monitor then only signs in (with `proto=quic`), hands the UDP capability on
    /// as a [`UdpGrant`], and keeps the chat ticket: no UDP socket of its own, no
    /// tunnel over wss, no fallback, nothing that reads a packet. Started with
    /// [`RelayMonitor::start_threaded_quic`]. `udp_port` is the relay's UDP port
    /// here too (`None` reads as [`DEFAULT_UDP_PORT`]: QUIC has no wss route).
    pub quic: bool,
}

/// What the QUIC route layer needs from a relay session: where the relay's UDP
/// port is, the capability token, and the session it belongs to. A new
/// `session` is a new wss session (a new socket on the other side); the same
/// `session` with another token is the relay's rotation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UdpGrant {
    pub relay: SocketAddr,
    /// The relay's first IPv4 address when `relay` is not one: the way back
    /// when no socket can be made for `relay` (see [`connect_relay_udp`]).
    pub fallback: Option<SocketAddr>,
    /// This side's public UDP address as the relay sees it (`udp-seen`, phase
    /// 3c): what the other side punches to. `None` until the relay has said it,
    /// and from a relay that does not.
    pub public: Option<SocketAddr>,
    pub token: [u8; TLU1_TOKEN_LEN],
    pub session: u64,
}

/// Default relay UDP audio port (the proven, firewall-friendly 443).
pub const DEFAULT_UDP_PORT: u16 = 443;

/// UDP audio-path port from the environment (`THETISLINK_RELAY_UDP_PORT`). An override
/// for testing/non-standard ports; unset -> falls through to the config setting.
pub fn relay_udp_port_env() -> Option<u16> {
    std::env::var("THETISLINK_RELAY_UDP_PORT")
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Resolve the UDP audio port: the env var wins (testing override); otherwise the
/// config toggle decides (`true` -> DEFAULT_UDP_PORT, `false` -> off = wss only).
pub fn relay_udp_port_resolve(config_enabled: bool) -> Option<u16> {
    if let Some(p) = relay_udp_port_env() {
        Some(p)
    } else if config_enabled {
        Some(DEFAULT_UDP_PORT)
    } else {
        None
    }
}

impl RelayConfig {
    pub fn is_ready(&self) -> bool {
        self.enabled
            && !self.url.trim().is_empty()
            && !self.station.trim().is_empty()
            && !self.token.trim().is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RelayPhase {
    Disabled,
    WaitingForConfig,
    Connecting,
    Authenticated,
    WaitingForPeer,
    Disconnected,
    Error,
}

/// Rolling statistics for the binary transport probe (Message::Binary round-trip
/// through the relay on an audio-sized payload). Distinct from `peer_rtt_ms`,
/// which measures the small text heartbeat.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BinaryProbeStats {
    pub last_ms: u32,
    pub min_ms: u32,
    pub max_ms: u32,
    pub avg_ms: u32,
    pub samples: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelayStatus {
    pub phase: RelayPhase,
    pub message: String,
    pub connected: bool,
    pub peer_seen: bool,
    pub last_error: Option<String>,
    pub peer_rtt_ms: Option<u32>,
    pub binary: Option<BinaryProbeStats>,
    pub generation: u64,
    /// Phase 3c: true while audio is on the wss TCP-fallback (UDP degraded/blocked); false
    /// on the normal low-latency UDP path. Drives the client transport indicator.
    pub transport_fallback: bool,
    /// The chat ticket the relay last handed out, if it offers one at all.
    ///
    /// Carried here rather than in a channel of its own because that is exactly
    /// what it is: a fact about the current relay connection, refreshed while it
    /// lasts and meaningless without it. `None` means a relay with no chat
    /// behind it, which is a normal state and not an error.
    ///
    /// This client never verifies it - only the chat service does (design §3).
    pub chat_ticket: Option<String>,
}

impl RelayStatus {
    pub fn disabled() -> Self {
        Self {
            phase: RelayPhase::Disabled,
            message: "Relay disabled".to_string(),
            connected: false,
            peer_seen: false,
            last_error: None,
            peer_rtt_ms: None,
            binary: None,
            generation: 0,
            transport_fallback: false,
            chat_ticket: None,
        }
    }

    pub fn label(&self) -> String {
        match self.phase {
            RelayPhase::Disabled => "Disabled".to_string(),
            RelayPhase::WaitingForConfig => "Waiting for configuration".to_string(),
            RelayPhase::Connecting => "Connecting".to_string(),
            RelayPhase::Authenticated => {
                let mut label = match self.peer_rtt_ms {
                    Some(ms) => format!("Peer online (RTT {ms} ms)"),
                    None => "Peer online".to_string(),
                };
                if let Some(b) = self.binary {
                    label.push_str(&format!(
                        ", binary {} ms (min {} / avg {} / max {}, n={})",
                        b.last_ms, b.min_ms, b.avg_ms, b.max_ms, b.samples
                    ));
                }
                label
            }
            RelayPhase::WaitingForPeer => "Connected, waiting for peer".to_string(),
            RelayPhase::Disconnected => "Disconnected".to_string(),
            RelayPhase::Error => "Error".to_string(),
        }
    }
}

#[derive(Clone)]
pub struct RelayStatusHandle {
    state: Arc<Mutex<RelayStatus>>,
}

impl RelayStatusHandle {
    pub fn snapshot(&self) -> RelayStatus {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Tunnel coupling for Phase C: carries tunneled TL-frame bytes between this
/// relay peer and the local endpoint (server: `TrackedSocket`; client: engine).
/// - `inbound_tx`: the monitor pushes decoded incoming `TLT1` payloads here
///   as `(sentinel, tl_bytes)`.
/// - `uplink_rx`: the monitor pulls outgoing `(sentinel, tl_bytes)` from here and
///   wraps them in `TLT1` over the WebSocket.
/// - `sentinel`: the synthetic client address that inbound traffic is tagged with
///   (v1: a single client). On the client side this is simply the server address.
pub struct RelayTunnel {
    pub sentinel: SocketAddr,
    pub inbound_tx: mpsc::UnboundedSender<(SocketAddr, Vec<u8>)>,
    pub uplink_rx: mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)>,
}

/// How many QUIC packets wait for the TCP line, each way. A full line drops:
/// latency before completeness, and QUIC sends again what it needs - as a busy
/// UDP socket does (the TCP fallback route, build 144).
pub const TCP_LINE_DEPTH: usize = 64;

/// The route layer's end of the TCP line (the TCP fallback route, build 144,
/// `OPZET-tcp-uitwijkroute.md`): QUIC packets as TLT1 frames on the monitor's
/// wss connection, for a network that lets no UDP through. The relay on the VPS
/// forwards TLT1 by client id already (it stamps a client's id on the way to
/// the station, and sends a station's frame to the client it names), so the
/// relay itself does not change.
#[derive(Debug)]
pub struct TcpLine {
    /// TLT1 frames to send on the wss connection ([`tlt1_frame`]).
    pub to_relay: mpsc::Sender<Vec<u8>>,
    /// What came in as TLT1: the client id and the QUIC packet.
    pub from_relay: mpsc::Receiver<(u8, Vec<u8>)>,
    /// Frames the monitor dropped because one was still on its way out
    /// (build 146: the count that shows "one at a time" bounds, review 145).
    pub dropped_out: Arc<std::sync::atomic::AtomicU64>,
}

/// The monitor's end of the TCP line.
pub struct TcpLineMonitor {
    to_relay: mpsc::Receiver<Vec<u8>>,
    from_relay: mpsc::Sender<(u8, Vec<u8>)>,
    dropped_out: Arc<std::sync::atomic::AtomicU64>,
}

/// A TCP line: one end for the route layer, one for the QUIC monitor
/// ([`RelayMonitor::start_threaded_quic`]).
pub fn tcp_line() -> (TcpLine, TcpLineMonitor) {
    let (out_tx, out_rx) = mpsc::channel(TCP_LINE_DEPTH);
    let (in_tx, in_rx) = mpsc::channel(TCP_LINE_DEPTH);
    let dropped_out = Arc::new(std::sync::atomic::AtomicU64::new(0));
    (
        TcpLine { to_relay: out_tx, from_relay: in_rx, dropped_out: dropped_out.clone() },
        TcpLineMonitor { to_relay: out_rx, from_relay: in_tx, dropped_out },
    )
}

/// A QUIC packet for client `client_id` as a TLT1 frame (a client sends id 0:
/// the relay stamps its own).
pub fn tlt1_frame(client_id: u8, payload: &[u8]) -> Vec<u8> {
    encode_tlt1(client_id, payload)
}

/// The client id and the payload of a TLT1 frame - for a test's stand-in of the
/// relay.
pub fn tlt1_parts(frame: &[u8]) -> Option<(u8, &[u8])> {
    parse_tlt1(frame)
}

/// The next frame for the TCP line, or never: without a line, and once the
/// route layer's end is gone (a status-only monitor drops it at once).
async fn recv_tcp_line(line: &mut Option<&mut TcpLineMonitor>) -> Vec<u8> {
    if let Some(l) = line {
        if let Some(frame) = l.to_relay.recv().await {
            return frame;
        }
    }
    std::future::pending().await
}

// The monitor runs on its own thread, always. A tokio-task variant (`start`,
// `RelayTask::Tokio`) had no caller, stopped differently (abort in Drop) and did
// not report a panic; removed (reviewers, rounds 44-45, owner's decision).
pub struct RelayMonitor {
    status: RelayStatusHandle,
    config_tx: watch::Sender<RelayConfig>,
    shutdown_tx: watch::Sender<bool>,
    /// Counts "the device has a network again", from whoever can see that (the
    /// Android connectivity callback). See `network_available`.
    network_tx: watch::Sender<u64>,
    /// Counts "the device's default network is another one now". See
    /// `network_changed` (build 186).
    change_tx: watch::Sender<u64>,
}

impl RelayMonitor {
    /// Status-only monitor on its own thread (no TL tunnel).
    ///
    /// The classic form, whatever `initial.quic` says: QUIC mode without a grant
    /// receiver would sign in as QUIC and publish nothing, a silent dead end. The
    /// constructor decides the form, not the caller (review 118).
    pub fn start_threaded(initial: RelayConfig) -> Self {
        Self::start_threaded_inner(RelayConfig { quic: false, ..initial }, None, None, None)
    }

    /// Monitor on its own thread that also tunnels TL frames (Phase C). The
    /// classic form, like [`RelayMonitor::start_threaded`].
    pub fn start_threaded_tunnel(initial: RelayConfig, tunnel: RelayTunnel) -> Self {
        Self::start_threaded_inner(RelayConfig { quic: false, ..initial }, Some(tunnel), None, None)
    }

    /// Monitor for QUIC via the relay (phase 3b): signs in with `proto=quic` and
    /// publishes each session's [`UdpGrant`] on `grants` (`None` between
    /// sessions). The data goes through the QUIC crate's route layer, not here
    /// - over UDP, and over `tcp` on the wss connection when there is one (the
    /// TCP fallback route, build 144). A monitor for status only passes `None`.
    pub fn start_threaded_quic(
        initial: RelayConfig,
        grants: watch::Sender<Option<UdpGrant>>,
        tcp: Option<TcpLineMonitor>,
    ) -> Self {
        Self::start_threaded_inner(RelayConfig { quic: true, ..initial }, None, Some(grants), tcp)
    }

    fn start_threaded_inner(
        initial: RelayConfig,
        tunnel: Option<RelayTunnel>,
        grants: Option<watch::Sender<Option<UdpGrant>>>,
        tcp: Option<TcpLineMonitor>,
    ) -> Self {
        let status = RelayStatusHandle {
            state: Arc::new(Mutex::new(RelayStatus::disabled())),
        };
        let (config_tx, config_rx) = watch::channel(initial);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (network_tx, network_rx) = watch::channel(0_u64);
        let (change_tx, change_rx) = watch::channel(0_u64);
        let task_status = status.clone();
        let spawn_status = status.clone();
        // A monitor stopped by a panic is stopped: `is_stopped()` reads this
        // signal, and a panic never set it (reviewer, round 45).
        let panic_stop = shutdown_tx.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("thetislink-relay-monitor".to_string())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(rt) => {
                        if run_reporting_a_panic(&task_status, || {
                            rt.block_on(monitor_loop(config_rx, shutdown_rx, network_rx, change_rx, task_status.clone(), tunnel, grants, tcp))
                        }) {
                            let _ = panic_stop.send(true);
                        }
                    }
                    Err(e) => set_status(
                        &task_status,
                        RelayPhase::Error,
                        format!("Relay runtime error: {e}"),
                        false,
                        false,
                        Some(e.to_string()),
                        0,
                    ),
                }
            })
        {
            set_status(
                &spawn_status,
                RelayPhase::Error,
                format!("Relay thread error: {e}"),
                false,
                false,
                Some(e.to_string()),
                0,
            );
        }
        Self {
            status,
            config_tx,
            shutdown_tx,
            network_tx,
            change_tx,
        }
    }

    pub fn status_handle(&self) -> RelayStatusHandle {
        self.status.clone()
    }

    pub fn update_config(&self, config: RelayConfig) {
        let _ = self.config_tx.send(config);
    }

    pub fn stop(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// The device has a network again. If the monitor is waiting because the
    /// last attempt failed inside this device (no route, no DNS: the new network
    /// was not up yet), it tries now instead of at the next step of the ladder;
    /// any other wait it leaves alone.
    ///
    /// Measured 2026-10-04: after a switch to mobile the network was ready
    /// somewhere between the attempt at +0.25 s and the one at +0.75 s, so the
    /// ladder waited up to half a second too long. Only the phone knows when the
    /// network is there; the ladder stays as the net under this.
    pub fn network_available(&self) {
        self.network_tx.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// The device's default network is another one now. A session that is
    /// running still runs over the old one - which may be going away - and
    /// would only notice when that fails: 7.4 s and 11.8 s in the field on
    /// 2026-10-12. It is ended at once, and the next one starts on the new
    /// network (build 186).
    pub fn network_changed(&self) {
        self.change_tx.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// A handle that can stop this monitor from somewhere it is not owned.
    ///
    /// One process must never run two of these against the same station: the
    /// relay gives a returning client its own slot back and closes the older
    /// connection, so two live monitors sharing an install id take turns
    /// evicting each other every reconnect delay, for as long as the app runs.
    /// The caller that creates monitors keeps the previous handle and stops it
    /// (2026-08-17).
    pub fn stop_handle(&self) -> RelayStopHandle {
        RelayStopHandle { shutdown_tx: self.shutdown_tx.clone() }
    }
}

/// Stops a monitor without owning it. Cloning it is cheap and stopping twice is
/// harmless.
#[derive(Clone)]
pub struct RelayStopHandle {
    shutdown_tx: watch::Sender<bool>,
}

impl RelayStopHandle {
    pub fn stop(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// Whether this monitor has already been told to stop.
    ///
    /// The caller that keeps the previous handle (the Android bridge) had no way
    /// to tell a monitor that is still running from one that stopped a second
    /// ago, so it warned about both. In a field log of 2026-09-24 that warning
    /// fired on both app restarts, 0.9 s after "Relay monitor stopped" two lines
    /// above it - the handle was stale, not live. A warning that is untrue every
    /// ordinary time is one nobody reads when the real case comes back, and the
    /// real case here is two live monitors evicting each other for as long as
    /// the app runs.
    ///
    /// The flag is the shutdown signal itself, so this cannot drift away from
    /// what `stop()` does.
    pub fn is_stopped(&self) -> bool {
        *self.shutdown_tx.borrow()
    }

    /// Whether these two handles stop the same monitor.
    ///
    /// The Android bridge keeps the previous monitor's handle in a static slot
    /// and needs to empty that slot when its own monitor shuts down - but only
    /// if the slot still holds ITS handle. On a network switch a newer bridge
    /// has already replaced it, and emptying blindly would throw away the
    /// handle the duplicate guard exists to use. That case is the whole reason
    /// the slot is there (2026-08-17).
    pub fn same_monitor(&self, other: &Self) -> bool {
        self.shutdown_tx.same_channel(&other.shutdown_tx)
    }

    /// Empty `slot` if it holds `mine`; leave it alone if it holds anyone
    /// else's. Returns whether it emptied it.
    ///
    /// The decision lives here and not inline in the Android bridge because the
    /// bridge cannot build a handle to test it with, and this is the line a
    /// careless change turns into `slot.take()` - which is correct on every
    /// ordinary shutdown and wrong on exactly the one the slot exists for, a
    /// network switch where the next bridge's handle is already in it.
    pub fn clear_slot_if_mine(slot: &mut Option<Self>, mine: &Self) -> bool {
        if slot.as_ref().is_some_and(|h| h.same_monitor(mine)) {
            *slot = None;
            true
        } else {
            false
        }
    }
}

impl Drop for RelayMonitor {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
    }
}

async fn monitor_loop(
    mut config_rx: watch::Receiver<RelayConfig>,
    mut shutdown_rx: watch::Receiver<bool>,
    mut network_rx: watch::Receiver<u64>,
    mut change_rx: watch::Receiver<u64>,
    status: RelayStatusHandle,
    mut tunnel: Option<RelayTunnel>,
    grants: Option<watch::Sender<Option<UdpGrant>>>,
    mut tcp: Option<TcpLineMonitor>,
) {
    let mut generation = 0_u64;
    let mut recovery = Recovery::default();
    loop {
        if *shutdown_rx.borrow() {
            set_status(
                &status,
                RelayPhase::Disabled,
                "Relay monitor stopped".to_string(),
                false,
                false,
                None,
                generation,
            );
            return;
        }

        let config = config_rx.borrow().clone();
        // Here, before any branch, and unconditional: every round, also a failed
        // or disabled one, gets a number of its own. The QUIC route layer tells a
        // new session from a rotation by this number alone - a `None` between two
        // sessions can be swallowed by the watch - so two sessions under one
        // number would keep it on the dead socket (review 118).
        generation = generation.wrapping_add(1);

        if !config.enabled {
            set_status(
                &status,
                RelayPhase::Disabled,
                "Relay disabled".to_string(),
                false,
                false,
                None,
                generation,
            );
            wait_for_change(&mut config_rx, &mut shutdown_rx).await;
            continue;
        }

        if !config.is_ready() {
            set_status(
                &status,
                RelayPhase::WaitingForConfig,
                "Relay enabled, but URL/station/token is incomplete".to_string(),
                false,
                false,
                None,
                generation,
            );
            wait_for_change(&mut config_rx, &mut shutdown_rx).await;
            continue;
        }

        set_status(
            &status,
            RelayPhase::Connecting,
            // The relay host is deliberately not published (it is kept out of the
            // source via THETISLINK_FORBIDDEN_HOSTS), and a log is the one artefact
            // that gets mailed around when something breaks. Log the role and the
            // scheme, not the address.
            format!("Connecting to {} as {}", redact_url(&config.url), config.role.as_str()),
            false,
            false,
            None,
            generation,
        );

        // A network that came up before this attempt is this attempt's network:
        // only one that comes up after it may cut a wait short.
        network_rx.borrow_and_update();
        // The same for a change of network: one before this attempt is the
        // network it runs on (build 186).
        change_rx.borrow_and_update();
        let mut outcome = SessionOutcome::default();
        // The whole session - building it and running it - against a change of
        // network: a build that hangs on a network going away would otherwise
        // see the change only when it gives up (build 187; review of 186).
        let (run_result, switched) = until_network_switch(
            run_connection(
                &config,
                &status,
                generation,
                &mut config_rx,
                &mut shutdown_rx,
                tunnel.as_mut(),
                grants.as_ref(),
                tcp.as_mut(),
                &mut outcome,
            ),
            &mut change_rx,
        )
        .await;
        outcome.network_changed = switched;
        // The session is over, and with it its token (the relay revokes it with
        // the connection): no grant until the next one.
        if let Some(g) = &grants {
            g.send_replace(None);
        }
        if *shutdown_rx.borrow() {
            continue;
        }
        if config_rx.borrow().clone() != config {
            continue;
        }

        let ended_on_error = run_result.is_err();
        if let Err(err) = run_result {
            // How this session ended, in one line. The log said what went wrong
            // and never whether the session had been working, so a refusal and a
            // network switch read the same.
            log::info!(
                "Relay session ended: {} (carried_traffic={} rejected={} closed_by_relay={} local={})",
                short_error(&err),
                outcome.carried_traffic,
                outcome.rejected.as_deref().unwrap_or("-"),
                outcome.closed_by_relay,
                outcome.failed_locally,
            );
            let msg = short_error(&err);
            set_status(
                &status,
                RelayPhase::Error,
                format!("Relay error: {msg}"),
                false,
                false,
                Some(msg),
                generation,
            );
        }

        let wait = recovery.next_wait(&outcome, ended_on_error, Instant::now(), spread_now());
        if wait.is_zero() && outcome.network_changed {
            // Named in advance (working rule 11, build 186).
            log::info!("Relay: reconnecting at once on the new network");
        } else if wait.is_zero() {
            // Named in the plan before it was built (working rule 11).
            log::info!("Relay: reconnecting at once - the session carried traffic and ended on a transport error");
        } else if wait < RECONNECT_DELAY.mul_f64(1.0 - RECONNECT_SPREAD) {
            // Named in the brief before it was built (working rule 11).
            log::info!("Relay: the network is not up yet - trying again in {} ms", wait.as_millis());
        }
        let waiting_for_the_network = !wait.is_zero() && recovery.waits_for_the_network();
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = network_came_up(&mut network_rx, waiting_for_the_network) => {
                // Named in the brief before it was built (working rule 11).
                log::info!("Relay: the network came up - trying now");
            }
            _ = config_rx.changed() => {}
            _ = shutdown_rx.changed() => {}
        }
    }
}

/// Runs one session until it ends, or until the device's default network
/// changes - whichever comes first, also while the session is still being
/// built. The session is dropped then; `run_connection` has nothing to tidy
/// after its loop, so dropping it is the same as it returning. Returns the
/// session's result and whether a change ended it (builds 186, 187).
async fn until_network_switch<F>(session: F, change_rx: &mut watch::Receiver<u64>) -> (Result<(), String>, bool)
where
    F: std::future::Future<Output = Result<(), String>>,
{
    tokio::select! {
        r = session => (r, false),
        Ok(()) = change_rx.changed() => {
            // Named in advance (working rule 11, builds 186 and 187).
            log::info!("Relay: the device's network changed - ending this session to start one on the new network");
            (Err("the device's network changed".to_string()), true)
        }
    }
}

/// Resolves when the device reports a network, but only while `waiting` - the
/// wait is a step of the local-retry ladder. Any other wait (a refusal, the
/// relay closing us) is not cut short by a network event: the network was never
/// the problem there.
async fn network_came_up(rx: &mut watch::Receiver<u64>, waiting: bool) {
    if !waiting || rx.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Poll the tunnel uplink receiver, or stay pending forever when there is no tunnel
/// (so the select! branch never fires in that case).
async fn recv_uplink(
    rx: &mut Option<&mut mpsc::UnboundedReceiver<(SocketAddr, Vec<u8>)>>,
) -> Option<(SocketAddr, Vec<u8>)> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

/// Install a rustls crypto provider (ring) exactly once, process-wide. Without this,
/// rustls 0.23 panics on the first wss:// connect ("no process-level CryptoProvider"),
/// since tokio-tungstenite does not select one. Belt-and-suspenders alongside the
/// `ring` crate feature; the Once makes repeat calls (per reconnect) harmless.
fn ensure_crypto_provider() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// A path is worth reporting on once it has been quiet this long. Below it a gap
/// is jitter, and logging that would drown the case worth reading.
///
/// Above the beacon interval on purpose: at one beacon a second a threshold of a
/// second reports a path "returning" on a perfectly healthy line, forever.
const PATH_QUIET_REPORT: Duration = Duration::from_millis(2500);

/// Note a packet on a path and return how long that path had been silent, raw.
///
/// It used to filter on `PATH_QUIET_REPORT` itself, which was convenient for its one
/// caller and wrong the moment a second question was asked of the same gap: the
/// fallback restarts its healthy run on a gap wider than `FALLBACK_DOWNLINK_STALL`,
/// and those two thresholds are the same 2500 ms today by coincidence, not by
/// design. A filter inside here would have tied them together silently. The caller
/// compares against whichever threshold it means.
fn note_path_packet(last: &mut Option<Instant>, now: Instant) -> Option<Duration> {
    let gap = last.map(|t| now.saturating_duration_since(t));
    *last = Some(now);
    gap
}

/// What one inbound datagram does to the downlink's two clocks, and how long the path
/// had been quiet before it.
///
/// Free, because the only question this spot has ever got wrong is "does a keepalive
/// count", and until now there was nothing that could ask it. Twice the answer was
/// no and twice it was the defect: first the fallback would not fire on a path
/// carrying only beacons, then the station reported an outage every time its operator
/// stopped transmitting. Both times the fix was to read the whole path rather than
/// the audio on it - and both times the reasoning lived in a comment because there
/// was nowhere to put an assertion.
///
/// `healthy_since` restarts only on a gap wider than `stall`, so a beacon at its
/// normal spacing continues the run instead of starting a new one. Getting that
/// wrong makes recovery unreachable on a path that is demonstrably alive.
fn note_downlink(
    last_downlink: &mut Option<Instant>,
    healthy_since: &mut Option<Instant>,
    now: Instant,
    stall: Duration,
) -> Option<Duration> {
    let gap = note_path_packet(last_downlink, now);
    if gap.map_or(true, |g| g > stall) {
        *healthy_since = Some(now);
    }
    gap
}

/// What became of one relay session, beside the error that ended it.
///
/// The retry policy needs to tell apart two things that look identical today: a
/// relay that refused us, and a socket that died under a session which was
/// working. Both come back as `Err(String)`, and by then a refusal has been
/// translated into a sentence meant for a human - so the only way to tell them
/// apart at the call site would be to compare translated text.
///
/// Rather than retype seventeen exit paths, the session reports alongside its
/// error. `reconnect_wait` decides on it (step 3 of the design, build 95).
#[derive(Default, Debug)]
pub(crate) struct SessionOutcome {
    /// The relay refused us, with the wire code (`ERR device blocked`) and not
    /// the sentence. All six refusals are permanent; retrying one of them faster
    /// only knocks harder, and one of them is how a station operator says no.
    pub rejected: Option<String>,
    /// Peer traffic was seen before this connection ended. A session that stood,
    /// carried something and then failed at transport level is the network-switch
    /// case - the only case a fast retry is meant for.
    pub carried_traffic: bool,
    /// When the first peer traffic arrived: how long the session stood is
    /// counted from here (round 95).
    pub traffic_since: Option<Instant>,
    /// The attempt failed inside this device before reaching the relay: the
    /// relay's name did not resolve, or there was no route (`NetworkUnreachable`
    /// and kin). The new network is not up yet (build 96).
    pub failed_locally: bool,
    /// The relay ended the connection itself - a Close frame, or the stream
    /// simply ending - rather than the transport failing under it. That is what
    /// a second instance with the same install id does to this one (`replacing
    /// existing client`), and a fast retry there would turn the known ping-pong
    /// from every 5 s into a busy loop (build 95).
    pub closed_by_relay: bool,
    /// The device moved to another default network while the session ran: it
    /// was ended on purpose, so the next one starts on the new network at once,
    /// however short this one stood (build 186).
    pub network_changed: bool,
}

impl SessionOutcome {
    /// Peer traffic arrived. The one place that says so - three receive paths
    /// call it (a text frame, a binary frame, a UDP datagram).
    pub(crate) fn mark_traffic(&mut self) {
        self.carried_traffic = true;
        if self.traffic_since.is_none() {
            self.traffic_since = Some(Instant::now());
        }
    }
}

/// Whether a failed websocket connect failed inside this device: a local kind
/// of I/O error, or a relay name that does not resolve now. The connect looks
/// the name up itself, and a lookup failure there has no kind of its own: in the
/// field (2026-10-08, a switch from wifi to 5G) it read as "the relay failed",
/// the wait was the plain 5 s, and the phone's "network is up" did not end it.
/// The lookup before the connect had just passed (build 141).
async fn connect_failed_locally(io: &std::io::Error, url: &str) -> bool {
    if is_local_failure(io) {
        return true;
    }
    match relay_host_port(url) {
        Some((host, port)) => tokio::net::lookup_host((host.as_str(), port)).await.is_err(),
        None => false,
    }
}

/// An I/O error that means "this device has no way out right now" rather than
/// "the relay said no": no route, network down, or no address to send from.
fn is_local_failure(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(e.kind(), NetworkUnreachable | HostUnreachable | NetworkDown | AddrNotAvailable)
}

/// What the next connection waits for, remembered across attempts: where on the
/// local-retry ladder we are after a working session broke.
#[derive(Debug, Default)]
pub(crate) struct Recovery {
    step: Option<usize>,
}

impl Recovery {
    /// Whether the wait just decided is a step of the local-retry ladder - the
    /// network was not there yet, so a network coming up ends it.
    pub(crate) fn waits_for_the_network(&self) -> bool {
        self.step.is_some()
    }

    /// How long to wait before the next connection.
    ///
    /// - At once after a session that stood (`STOOD_AT_LEAST` of traffic), was
    ///   not refused, was not closed by the relay, and ENDED ON AN ERROR - the
    ///   network-switch case (measured 2026-10-03: of 5.4 s silence, 5.0 s was the
    ///   fixed wait). A session that ends without an error (settings saved
    ///   unchanged) is not that case (round 95).
    /// - After that, attempts that fail locally walk `LOCAL_RETRY_LADDER`, and
    ///   stay on its last step for as long as they fail locally.
    /// - Everything else - a refusal, the relay closing us, a connection that
    ///   carried nothing or failed at the relay, the end of the ladder - keeps the
    ///   5 s, spread a little either way. `spread` is in [0, 1).
    pub(crate) fn next_wait(&mut self, outcome: &SessionOutcome, ended_on_error: bool, now: Instant, spread: f64) -> Duration {
        if outcome.network_changed {
            // On the ladder from here: the new network may not be up yet.
            self.step = Some(0);
            return Duration::ZERO;
        }
        let stood = outcome.traffic_since.is_some_and(|t| now.saturating_duration_since(t) >= STOOD_AT_LEAST);
        if ended_on_error && stood && outcome.rejected.is_none() && !outcome.closed_by_relay {
            self.step = Some(0);
            return Duration::ZERO;
        }
        if let Some(i) = self.step {
            if outcome.failed_locally {
                // On the last step it stays while the failure is local - every 2 s,
                // never slower than the plain wait and still nothing at the relay
                // (round 96: a net ready between 3.9 and 5 s would otherwise wait
                // longer than without the ladder).
                let step = i.min(LOCAL_RETRY_LADDER.len() - 1);
                self.step = Some(i + 1);
                return LOCAL_RETRY_LADDER[step];
            }
        }
        self.step = None;
        let factor = 1.0 - RECONNECT_SPREAD + 2.0 * RECONNECT_SPREAD * spread.clamp(0.0, 1.0);
        RECONNECT_DELAY.mul_f64(factor)
    }
}

/// The relay's host and port from its URL, for resolving it before connecting.
fn relay_host_port(url: &str) -> Option<(String, u16)> {
    let uri: tokio_tungstenite::tungstenite::http::Uri = url.parse().ok()?;
    let host = uri.host()?.to_string();
    let port = uri.port_u16().unwrap_or(if uri.scheme_str() == Some("ws") { 80 } else { 443 });
    Some((host, port))
}

/// A number in [0, 1) that differs from endpoint to endpoint and from moment to
/// moment - enough to spread a wait, without a random-number crate.
fn spread_now() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    (nanos % 1_000) as f64 / 1_000.0
}

async fn run_connection(
    config: &RelayConfig,
    status: &RelayStatusHandle,
    generation: u64,
    config_rx: &mut watch::Receiver<RelayConfig>,
    shutdown_rx: &mut watch::Receiver<bool>,
    tunnel: Option<&mut RelayTunnel>,
    grants: Option<&watch::Sender<Option<UdpGrant>>>,
    mut tcp: Option<&mut TcpLineMonitor>,
    outcome: &mut SessionOutcome,
) -> Result<(), String> {
    // Split the tunnel into independent pieces so the select! branches do not get
    // in each other's way with borrows: sentinel (Copy), inbound_tx (cheap clone),
    // and a reborrow of uplink_rx (&mut, preserved across reconnects).
    let (tunnel_sentinel, tunnel_inbound, mut tunnel_uplink) = match tunnel {
        Some(t) => (Some(t.sentinel), Some(t.inbound_tx.clone()), Some(&mut t.uplink_rx)),
        None => (None, None, None),
    };
    // Client-ids currently bound through this connection (station side: one per
    // remote client; client side: just the single server). Cleared on peer timeout.
    let mut bound_clients: HashSet<u8> = HashSet::new();

    ensure_crypto_provider();
    // Resolve first, on its own: a name that does not resolve here means the
    // network is not up yet, not that the relay refused - and the recovery
    // ladder treats the two differently (build 96).
    if let Some((host, port)) = relay_host_port(&config.url) {
        if let Err(e) = tokio::net::lookup_host((host.as_str(), port)).await {
            outcome.failed_locally = true;
            return Err(format!("relay address not resolvable here: {e}"));
        }
    }
    let (ws, _) = match connect_async(config.url.as_str()).await {
        Ok(v) => v,
        Err(e) => {
            if let tokio_tungstenite::tungstenite::Error::Io(io) = &e {
                if connect_failed_locally(io, &config.url).await {
                    outcome.failed_locally = true;
                }
            }
            return Err(e.to_string());
        }
    };
    // Disable Nagle: the monitor sends several small frames close together, and
    // Nagle batching adds latency/jitter to a low-rate control path like this.
    // Disable Nagle on the underlying TCP for both plain (ws) and TLS (wss) - the
    // low-latency relay path depends on TCP_NODELAY; without handling the TLS variant
    // a wss:// connection would silently keep Nagle on.
    match ws.get_ref() {
        MaybeTlsStream::Plain(tcp) => {
            let _ = tcp.set_nodelay(true);
        }
        MaybeTlsStream::Rustls(tls) => {
            let _ = tls.get_ref().0.set_nodelay(true);
        }
        _ => {}
    }
    let (mut write, mut read) = ws.split();
    let mut hello = if config.instance.trim().is_empty() {
        format!(
            "TLR1 station={} role={} token={}",
            config.station.trim(),
            config.role.as_str(),
            config.token.trim()
        )
    } else {
        format!(
            "TLR1 station={} role={} token={} instance={}",
            config.station.trim(),
            config.role.as_str(),
            config.token.trim(),
            config.instance.trim()
        )
    };
    // QUIC via the relay says so: today's relay ignores the key, a relay for
    // v2.13 can refuse a hello without it (OPZET-fase3b §2.7).
    if config.quic {
        hello.push_str(" proto=quic");
    }
    // `name=` MUST be last: it is free-form and may contain spaces, so the relay
    // parses it as the remainder of the line. Omitted when empty.
    let name = config.name.trim();
    if !name.is_empty() {
        hello.push_str(" name=");
        hello.push_str(name);
    }
    write
        .send(Message::Text(hello.into()))
        .await
        .map_err(|e| e.to_string())?;

    // The relay's answer is OK or ERR. A peer's frame can come before it: the
    // relay puts this connection in the room before it sends the OK, and the
    // other side's status or probe in that moment arrives first (review 124,
    // measured). That is no answer - and since build 124 a peer cannot pass an
    // OK or ERR through the relay, so waiting for one of those is safe.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let first = loop {
        let message = tokio::time::timeout_at(deadline, read.next())
            .await
            .map_err(|_| "relay hello timed out".to_string())?
            .ok_or_else(|| "relay closed before authentication".to_string())?
            .map_err(|e| e.to_string())?;
        match &message {
            Message::Text(text) if !text.starts_with(RELAY_OK) && !text.starts_with(RELAY_ERR) => {
                log::debug!("Relay: a peer frame before the relay's answer - skipped");
            }
            Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => {}
            _ => break message,
        }
    };

    // Phase 2: the ready-reply carries the UDP capability token ("OK relay-ready
    // udp_token=..."); capture it to open the audio-over-UDP path below. Assigned on
    // the OK arm; the other arms diverge (return), so it is always initialised after.
    // `mut`: the relay rotates this token over wss before its hard TTL; the loop
    // swaps in each fresh token for outgoing TLU1 packets (see the Text arm below).
    let mut udp_token: Option<String>;
    match first {
        Message::Text(text) if text.starts_with(RELAY_OK) && text[RELAY_OK.len()..].starts_with(' ') => {
            // A relay with a chat behind it offers a ticket here; one without
            // simply does not, and that is a normal state (design §2.4 - the
            // chat being absent may never disturb anything else).
            set_chat_ticket(status, parse_chat_ticket(&text));
            udp_token = parse_udp_token(&text);
            set_status(
                status,
                RelayPhase::WaitingForPeer,
                "Relay authenticated; waiting for peer handshake".to_string(),
                true,
                false,
                None,
                generation,
            );
        }
        Message::Text(text) => {
            // Keep the wire code before it becomes a sentence for a human.
            outcome.rejected = Some(text.trim().to_string());
            return Err(reject_reason(&text));
        }
        other => return Err(format!("unexpected relay hello: {other:?}")),
    }

    // Phase 2: open the bare-UDP audio path when configured and a token was issued.
    // Best-effort - on any failure we simply keep audio on wss (additive fallback).
    // QUIC via the relay: the route layer owns the one UDP socket to the relay
    // (the relay binds a token to the first source it sees), so none here.
    if config.quic {
        publish_grant(config, &udp_token, None, grants, generation).await;
    }
    let udp_socket: Option<UdpSocket> = match (config.udp_port, &udp_token) {
        _ if config.quic => None,
        (Some(port), Some(_)) => match open_udp(&config.url, port).await {
            Ok(sock) => {
                log::info!("Relay UDP audio path up (<relay>:{port})");
                Some(sock)
            }
            Err(e) => {
                log::warn!("Relay UDP audio path unavailable, using wss: {e}");
                None
            }
        },
        _ => None,
    };
    let mut udp_seq: u32 = 0;
    let mut udp_buf = vec![0u8; 2048];
    // Punch our UDP source to the relay right away so it learns where to send our
    // downlink audio BEFORE we transmit anything (source-binding bootstrap). An
    // empty-payload TLU1 keepalive: the relay learns the src, and the 1-byte framed
    // forward is ignored by the peer (its `n > 1` guard). Repeated on a timer below to
    // keep the NAT mapping alive.
    if let (Some(sock), Some(tok)) = (&udp_socket, &udp_token) {
        udp_seq = udp_seq.wrapping_add(1);
        let _ = sock.send(&encode_tlu1(0, udp_seq, tok, &[])).await;
    }
    let mut last_udp_keepalive = Instant::now();
    // Last audio frame seen PER STREAM, for the resume log. Per stream on purpose:
    // RX audio runs continuously, so a single "any audio" timestamp never goes
    // quiet and the moment a VRX stream resumes stays invisible inside it.
    let mut last_stream_seen: std::collections::HashMap<u16, Instant> =
        std::collections::HashMap::new();

    write
        .send(Message::Text(peer_frame("hello", config.role, config.quic).into()))
        .await
        .map_err(|e| e.to_string())?;

    let mut last_ping = Instant::now();
    let mut last_status = Instant::now();
    let mut last_rtt_ping = Instant::now();
    let mut rtt_seq = 0_u64;
    let mut pending_rtt: Option<(u64, Instant)> = None;
    let mut peer_active = false;
    // When the peer became reachable. The UDP-connect window measures from here,
    // not from the moment the tunnel stood - see `downlink_stalled`.
    let mut peer_active_since: Option<Instant> = None;
    let mut last_peer_activity = Instant::now();
    let mut last_binary_probe = Instant::now();
    let mut binary_seq = 0_u64;
    let mut pending_binary: Option<(u64, Instant)> = None;
    let mut binary_samples: VecDeque<u32> = VecDeque::new();

    // Phase 3a: make-before-break UDP<->wss fallback (relay-only, additive - inert while the
    // fallback stays off, so the phase-2 UDP path is byte-identical in the normal case).
    // Client side: watch UDP-audio health and request wss fallback if it never arrives.
    let ready_at = Instant::now();
    // Say what we can do as soon as the session stands. The station needs to know from
    // second zero whether this client reads a bare beacon, because that decides whether
    // it may stop sending UDP audio during a fallback - and the first fallback can
    // arrive within two seconds.
    //
    // Its own opcode, so a station built before this cannot mistake it for a fallback
    // request (which a spare bit in the fallback frame would have been). What such a
    // station does instead: `parse_rcf_fallback` returns None, the frame falls through
    // to the tunnel path like any other payload, and the engine rejects it at the magic
    // byte - `RCF1` starts with 0x52 and a TL packet with 0xAA. Harmless, and the same
    // route the fallback frames themselves already take against a station older than
    // phase 3a. Not "silently ignored at this layer", which is what the design said and
    // a review rightly would not accept on trust.
    //
    // Repeated below on the heartbeat, and that is not belt and braces: the station
    // keeps this fact inside the per-client fallback entry, and that entry is dropped a
    // lease after the last control frame. Announced once, it is gone fifteen seconds
    // into every session - long before any realistic network switch.
    // Not in QUIC mode: there is no fallback to have capabilities for, and
    // nothing travels as a TLT1 frame.
    if config.role == RelayRole::Client && !config.quic {
        let _ = write
            .send(Message::Binary(
                encode_tlt1(0, &encode_rcf_caps(CAPS_READS_BEACON)).into(),
            ))
            .await;
    }
    let mut last_caps_sent = Instant::now();
    // Test hook (client only): THETISLINK_TEST_DROP_UDP=<secs> drops inbound UDP audio for
    // the first N seconds after ready, simulating a UDP-blocked network so the fallback +
    // recovery cycle can be exercised deterministically (no OS-firewall fight). 0 = off.
    // Capped at 60s: while it runs, audio falls back to wss and stays there, so a mistyped
    // value (3600 for 36) would cost latency for an hour on a variable nobody notices is set.
    let test_drop_udp_secs: u64 = if config.role == RelayRole::Client {
        std::env::var("THETISLINK_TEST_DROP_UDP")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0)
            .min(60)
    } else {
        0
    };
    if test_drop_udp_secs > 0 {
        let asked: u64 = std::env::var("THETISLINK_TEST_DROP_UDP")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        if asked > test_drop_udp_secs {
            log::warn!(
                "TEST HOOK: dropping inbound UDP audio for the first {test_drop_udp_secs}s \
                 (asked for {asked}s, capped at 60)"
            );
        } else {
            log::warn!("TEST HOOK: dropping inbound UDP audio for the first {test_drop_udp_secs}s");
        }
    }
    let mut last_udp_audio: Option<Instant> = None; // last inbound audio frame over UDP
    // Said once per session in the log; the screen says it on every frame.
    let mut said_form_mismatch = false;
    // This side's public UDP address, as the relay last said it (phase 3c). Per
    // session: a new session brings a new socket, and so a new address.
    let mut public_seen: Option<SocketAddr> = None;
    // A token rotation whose new token the relay has not yet bound (and said).
    let mut rotation_unconfirmed = false;
    // Whether the peer's last frame carried the other form. While it did, nothing
    // else paints the status green: a binary probe answer or bare traffic set
    // "authenticated, no error" over the warning, and the screen flipped between
    // red and green every few seconds (field test 2026-10-06, review 121).
    let mut form_mismatch = false;
    // Last inbound datagram over UDP of ANY kind - audio or a bare beacon. This is
    // what both edges of the fallback read, and the difference from
    // `last_udp_audio` is the whole point: a path that carries beacons is alive
    // even while no channel is switched on, and the old test could not say so.
    let mut last_udp_downlink: Option<Instant> = None;
    // `last_any_audio` used to live here. It existed for one reader - the guard that
    // asked whether audio was flowing before believing the UDP path had stalled - and
    // that guard is gone: the question is now whether the DOWNLINK carries anything,
    // which a station with every channel off answers with a beacon. What the comment
    // here used to warn about, flipping on a channel toggle, cannot happen for the
    // same reason.
    // How long has each path been quiet? `bound_clients` already logs the FIRST
    // packet per path out of a HashSet insert, so after a reconnect it says
    // nothing at all, and it never carried a duration. These two do - and they
    // are what turns the eleven seconds between "tunnel back" and "audio back"
    // into a number.
    // When "no audio while the path says it is alive" began, or None while that is
    // not the case. A latch rather than a clock: the report it drives is an edge, so
    // it needs to know whether it has already spoken, not when it last spoke.
    //
    // It holds the moment AUDIO STOPPED, not the moment the report fired - those are
    // a threshold apart and the closing line carries the difference.
    //
    // CONVENTION, because it is cheaper than the alternative: an opening line and its
    // closing line are a PAIR WITHIN ONE SESSION. A session that ends while this is
    // set writes no closing line, and an unclosed opening line therefore means "the
    // session ended", not "it never came back". Closing it properly would mean a line
    // on each of seventeen exit paths - the same reason `SessionOutcome` exists rather
    // than each of those paths reporting for itself.
    //
    // `last_fallback_report` used to live here and was a brake on a level-triggered
    // line - one per dwell, forever, for as long as the condition held. An edge needs
    // no brake, and the latch doubles as the start of the episode so the closing line
    // can carry its length.
    let mut audio_absent_since: Option<Instant> = None;
    let mut last_in_tunnel: Option<Instant> = None;
    // `last_in_udp` used to live here, next to `last_udp_downlink`, and the two held
    // the same fact one line apart. The UDP path reports its quiet time from
    // `last_udp_downlink` itself now - one variable, read for two questions with two
    // thresholds, instead of two variables that could drift.
    //
    // Both are local to one connection, so a reconnect starts them at None and the
    // first datagram after it reports no gap. An outage that spans a reconnect is
    // therefore never reported on the UDP path - noted so that the absence of a line
    // is not later read as evidence that nothing was wrong.
    // Same, for inbound frames.
    let mut last_stream_in: std::collections::HashMap<u16, Instant> =
        std::collections::HashMap::new();
    let mut udp_healthy_since: Option<Instant> = None; // start of the current healthy UDP run
    let mut fallback_requested = false; // client: are we in wss-audio fallback?
    let mut last_fallback_flip = ready_at - FALLBACK_DWELL; // allow an immediate first flip
    let mut last_fallback_heartbeat = ready_at; // client: last RCF lease refresh
    let mut declared_initial_transport = false; // client: sent the connect-time RCF-off yet?
    let mut fallback_flips: u32 = 0; // observability: count transport flips
    let mut dedup = DedupWindow::new(); // drop duplicate audio across both paths
    // Station side: per-client fallback state with dwell + lease (per-session scope + rate-limit).
    let mut station_fallback: HashMap<u8, StationFallback> = HashMap::new();
    let mut udp_audio_while_fallback: u64 = 0;
    // Audio frames sent while the client asking for wss is not known to read a beacon.
    // That is the failure itself rather than its cost, and it is what stayed silent for
    // a whole review round: the capability arrived once and was thrown away with the
    // lease fifteen seconds later.
    let mut fallback_without_beacon_reader: u64 = 0;
    // Beacons the station sends, so the extra traffic this repair adds to an idle line
    // is a number somebody can read rather than an estimate in a document.
    let mut beacons_sent: u64 = 0;
    // Datagrams sent purely to hold the NAT mapping open while no client is bound.
    // Counted separately: they are not beacons and calling them that would hide
    // exactly the silence this was built to end.
    let mut mapping_keepalives: u64 = 0;
    // And the other end of that: datagrams the client actually received. It settles a
    // question the station log cannot answer on its own. A fallback of 393 seconds was
    // observed while the station sent 330 beacons, and nothing said whether the client
    // got almost none of them (the path really was dead, which is accepted) or plenty
    // with gaps just over the threshold (recovery stricter than "usable", which would
    // be a second subject). One counter tells those apart.
    let mut downlink_received: u64 = 0;

    // Send one round of keepalives on the token GIVEN, count them, and restart the
    // keepalive clock. The token is an argument and not read from `udp_token`: at a
    // rotation the caller hands over the token it just parsed, so the round cannot go
    // out on the old one whatever order the lines are in - the reviewers showed that a
    // read from the local could, and that no test sees it (round 53). The clock is
    // restarted here for the same reason: a caller cannot forget it. A macro and not a
    // function because the socket and the counters are locals of this loop; the round
    // itself is `keepalive_datagrams`. Counted in fitness.ps1 ($KEEPALIVE_ROUND_SITES).
    macro_rules! send_keepalives {
        ($tok:expr) => {{
            if let Some(sock) = &udp_socket {
                let (datagrams, kind) =
                    keepalive_datagrams(config.role, &bound_clients, $tok, &mut udp_seq);
                for d in &datagrams {
                    let _ = sock.send(d).await;
                    match kind {
                        KeepaliveKind::Beacons => beacons_sent += 1,
                        KeepaliveKind::MappingOnly => mapping_keepalives += 1,
                        KeepaliveKind::Client => {}
                    }
                }
            }
            last_udp_keepalive = Instant::now();
        }};
    }

    // Persistent 250 ms ticker, created ONCE before the loop. A `sleep()` re-created
    // each select iteration gets reset by every audio packet (<250 ms apart) and never
    // elapses under continuous audio - which starved the UDP keepalive + peer-status +
    // probes. An interval keeps its own deadline, so the timer branch fires reliably
    // regardless of audio load. Skip missed ticks (no catch-up burst).
    let mut timer = tokio::time::interval(Duration::from_millis(250));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The TCP line's frame on its way out (build 145): one at a time. While it
    // has not left - the wss side is full - a new frame is dropped rather than
    // waited for, so the loop never stands still on writing: "full is gone",
    // and QUIC sends again what it needs. Its push goes on in an arm of its own.
    //
    // `tcp_frame_out` is what keeps it at one frame (review 145).
    // `SplitSink::poll_ready` answers "ready" at once while its one slot is
    // empty - without asking the wss side - and `start_send` only fills that
    // slot. Tungstenite under it buffers up to its `write_buffer_size` (128 KiB)
    // before it even tries the socket; only a socket that is full then makes
    // its `poll_ready` wait. Without the flag up to 128 KiB would queue here -
    // about a second on a 1 Mbit/s line - before anything was dropped. (Build
    // 146: without the flag a flood still gets frames dropped, so no test that
    // counts drops can see it; read in tungstenite, not measured.) The
    // `poll_ready` below is there for the `Sink` contract.
    //
    // "Not left yet" is asked of the socket, not of the loop (build 147): a new
    // frame first tries the push of the one before it, once. The push arm alone
    // left that to `select!`, and a frame that came in before the loop chose the
    // push arm was dropped on a free line - 160 of 200 in a burst, 480 to 3170
    // per 10 s on the station with two clients on TCP (field test of build 146).
    let mut tcp_frame_out = false;
    let mut tcp_dropped = 0u64;
    let mut tcp_dropped_said: Option<Instant> = None;
    loop {
        tokio::select! {
            _ = config_rx.changed() => return Ok(()),
            _ = shutdown_rx.changed() => return Ok(()),
            maybe_message = read.next() => {
                match maybe_message {
                    Some(Ok(Message::Text(text))) => {
                        // Bounded-TTL token rotation: the relay pushes a fresh UDP capability
                        // token ("udp-token-rotate udp_token=...") before the current
                        // one hits its hard TTL. Swap it in for outgoing TLU1; the old
                        // token stays valid during the relay's short overlap, so no audio
                        // drops. Relay-infrastructure message -> not peer activity.
                        if text.starts_with(RELAY_CHAT_TICKET_ROTATE) {
                            // Refreshed well inside the ticket's own lifetime, so
                            // the chat window never finds itself holding an
                            // expired one. Relay infrastructure, not peer
                            // activity - so it does not count as the peer being
                            // alive, exactly like the token rotation below.
                            if let Some(t) = parse_chat_ticket(&text) {
                                set_chat_ticket(&status, Some(t));
                                log::debug!("Relay chat ticket refreshed");
                            }
                        } else if text.starts_with(RELAY_UDP_TOKEN_ROTATE) {
                            if let Some(t) = parse_udp_token(&text) {
                                // At once, not at the next tick, and on the token just
                                // received: until the relay has seen our source on the
                                // new token, its lookup can pick it and find no address.
                                send_keepalives!(&t);
                                udp_token = Some(t);
                                // The relay says the address again once the new
                                // token is bound; that line confirms the rotation
                                // in the log (review 123).
                                rotation_unconfirmed = true;
                                log::info!("Relay UDP token rotated - keepalive sent on the new token");
                                if config.quic {
                                    publish_grant(config, &udp_token, public_seen, grants, generation).await;
                                }
                            }
                            continue;
                        } else if let Some(seen) = text.strip_prefix(RELAY_UDP_SEEN).and_then(|r| r.strip_prefix(' ')) {
                            // Phase 3c: the relay says where our UDP comes from -
                            // the address the other side punches to. Into the
                            // grant of this session; never into the log (the
                            // family only: on IPv6 it is the device's own).
                            if let Ok(addr) = seen.trim().parse::<SocketAddr>() {
                                if public_seen != Some(addr) {
                                    log::info!("Relay: our public UDP address is known ({})", family(addr));
                                } else if std::mem::take(&mut rotation_unconfirmed) {
                                    log::info!(
                                        "Relay: public UDP address confirmed after the token rotation ({})",
                                        family(addr)
                                    );
                                }
                                rotation_unconfirmed = false;
                                public_seen = Some(addr);
                                if let Some(g) = grants {
                                    g.send_if_modified(|grant| match grant {
                                        Some(grant) if grant.public != Some(addr) => {
                                            grant.public = Some(addr);
                                            true
                                        }
                                        _ => false,
                                    });
                                }
                            }
                            continue;
                        }
                        peer_active = true;
                        outcome.mark_traffic();
                        last_peer_activity = Instant::now();
                        if let Some(frame) = parse_peer_frame(&text) {
                            // The two sides must carry the same data form: QUIC
                            // packets mean nothing to a TL peer and back. Said on
                            // screen, every frame, so a heartbeat cannot paint
                            // over it; the log says it once.
                            form_mismatch = frame.quic != config.quic;
                            if form_mismatch {
                                if !said_form_mismatch {
                                    said_form_mismatch = true;
                                    log::warn!(
                                        "Relay: the {} uses {}, this side {} - the two run different versions",
                                        frame.role,
                                        form_name(frame.quic),
                                        form_name(config.quic)
                                    );
                                }
                                set_status(
                                    status,
                                    RelayPhase::Error,
                                    form_mismatch_message(config.quic),
                                    true,
                                    false,
                                    Some("form mismatch".to_string()),
                                    generation,
                                );
                                continue;
                            }
                            match frame.kind.as_str() {
                                "ping" => {
                                    if let Some(seq) = frame.seq {
                                        write
                                            .send(Message::Text(peer_frame_with_seq("pong", config.role, seq, config.quic).into()))
                                            .await
                                            .map_err(|e| e.to_string())?;
                                    }
                                    set_status(
                                        status,
                                        RelayPhase::Authenticated,
                                        peer_frame_message(&frame),
                                        true,
                                        true,
                                        None,
                                        generation,
                                    );
                                }
                                "pong" => {
                                    if let (Some(seq), Some((pending_seq, sent_at))) = (frame.seq, pending_rtt) {
                                        if seq == pending_seq {
                                            pending_rtt = None;
                                            let rtt_ms = sent_at.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
                                            set_peer_rtt(
                                                status,
                                                peer_frame_message_with_rtt(&frame, rtt_ms),
                                                rtt_ms,
                                                generation,
                                            );
                                        } else {
                                            set_status(
                                                status,
                                                RelayPhase::Authenticated,
                                                peer_frame_message(&frame),
                                                true,
                                                true,
                                                None,
                                                generation,
                                            );
                                        }
                                    } else {
                                        set_status(
                                            status,
                                            RelayPhase::Authenticated,
                                            peer_frame_message(&frame),
                                            true,
                                            true,
                                            None,
                                            generation,
                                        );
                                    }
                                }
                                _ => {
                                    set_status(
                                        status,
                                        RelayPhase::Authenticated,
                                        peer_frame_message(&frame),
                                        true,
                                        true,
                                        None,
                                        generation,
                                    );
                                }
                            }
                        } else if !form_mismatch {
                            set_status(
                                status,
                                RelayPhase::Authenticated,
                                format!("Relay peer traffic received: {} bytes", text.len()),
                                true,
                                true,
                                None,
                                generation,
                            );
                        }
                    }
                    Some(Ok(Message::Binary(data))) => {
                        peer_active = true;
                        outcome.mark_traffic();
                        last_peer_activity = Instant::now();
                        // The TCP fallback route (build 144): in QUIC mode a TLT1
                        // frame is a QUIC packet for the route layer - or for
                        // nobody, on a monitor without a line (a status monitor
                        // can get one when the relay hands it a client id that a
                        // TCP client had a moment ago). Either way it ends here:
                        // further down it would set the status line to "peer
                        // traffic received", once per packet (review 144).
                        if config.quic {
                            if let Some((cid, payload)) = parse_tlt1(&data) {
                                if let Some(line) = tcp.as_ref() {
                                    // A full line drops: QUIC sends again what it needs.
                                    let _ = line.from_relay.try_send((cid, payload.to_vec()));
                                }
                                continue;
                            }
                        }
                        // TLT1 = tunneled TL-frame data -> deliver payload to the local endpoint.
                        if let Some(inbound) = &tunnel_inbound {
                            if let Some((cid, payload)) = parse_tlt1(&data) {
                                if let Some((stream, seq)) = audio_identity(payload) {
                                    let now = Instant::now();
                                    let quiet = last_stream_in
                                        .get(&stream)
                                        .map_or(true, |t: &Instant| t.elapsed() > Duration::from_millis(500));
                                    if quiet {
                                        log::debug!(
                                            "Relay {:?} RECV wss: stream 0x{:04x} resumes at seq={}",
                                            config.role, stream, seq
                                        );
                                    }
                                    last_stream_in.insert(stream, now);
                                }
                                // Phase 3a: station intercepts a client's wss-audio fallback
                                // request (per-session scope, idempotent) instead of
                                // forwarding it to the engine.
                                if config.role == RelayRole::Station {
                                    if is_relay_control_frame(payload)
                                        && bound_clients.insert(cid)
                                    {
                                        log::info!(
                                            "Relay tunnel client bound: id={cid} (control frame)"
                                        );
                                    }
                                    if let Some(bits) = parse_rcf_caps(payload) {
                                        let now = Instant::now();
                                        let e = station_fallback.entry(cid).or_insert(
                                            StationFallback {
                                                on: false,
                                                last_change: now
                                                    .checked_sub(FALLBACK_DWELL)
                                                    .unwrap_or(now),
                                                last_seen: now,
                                                reads_beacon: false,
                                                on_since: None,
                                                udp_frames_while_on: 0,
                                            },
                                        );
                                        e.reads_beacon = bits & CAPS_READS_BEACON != 0;
                                        e.last_seen = now;
                                        log::info!(
                                            "Relay client {cid} capabilities: reads_beacon={}",
                                            e.reads_beacon
                                        );
                                        continue;
                                    }
                                    if let Some(on) = parse_rcf_fallback(payload) {
                                        let now = Instant::now();
                                        let e = station_fallback.entry(cid).or_insert(
                                            StationFallback {
                                                on: false,
                                                // Backdated so the first real flip is not
                                                // dwell-blocked by the entry's creation.
                                                last_change: now
                                                    .checked_sub(FALLBACK_DWELL)
                                                    .unwrap_or(now),
                                                last_seen: now,
                                                reads_beacon: false,
                                                on_since: None,
                                                udp_frames_while_on: 0,
                                            },
                                        );
                                        e.last_seen = now; // lease refresh (flip or heartbeat)
                                        if e.on != on {
                                            // Dwell on the station too, so a client that
                                            // ignores its own dwell cannot make us flap.
                                            if e.last_change.elapsed() >= FALLBACK_DWELL {
                                                e.on = on;
                                                e.last_change = now;
                                                if on {
                                                    e.on_since = Some(now);
                                                    e.udp_frames_while_on = 0;
                                                    log::info!(
                                                        "Relay station: client {cid} \
                                                         wss-fallback ON (reads_beacon={})",
                                                        e.reads_beacon
                                                    );
                                                } else {
                                                    // The episode carries its own evidence:
                                                    // how long it lasted and how many frames
                                                    // still went out over UDP. Zero frames
                                                    // only means something next to a real
                                                    // duration.
                                                    let secs = e
                                                        .on_since
                                                        .map(|t| t.elapsed().as_secs_f32())
                                                        .unwrap_or(0.0);
                                                    log::info!(
                                                        "Relay station: client {cid} \
                                                         wss-fallback OFF after {secs:.1}s, \
                                                         {} duplicate UDP frames \
                                                         (reads_beacon={})",
                                                        e.udp_frames_while_on,
                                                        e.reads_beacon
                                                    );
                                                    e.on_since = None;
                                                }
                                            } else {
                                                log::debug!(
                                                    "Relay station: client {cid} fallback flip within dwell, ignored"
                                                );
                                            }
                                        }
                                        continue;
                                    }
                                }
                                // Phase 3: during path overlap / TX-always-both a frame may
                                // arrive on BOTH wss and UDP; drop the duplicate before the
                                // jitter buffer. Applies to both roles (client RX downlink and
                                // station TX uplink).
                                if dedup.is_duplicate(cid, payload) {
                                    continue;
                                }
                                // Station demultiplexes on client_id into a per-client
                                // sentinel; the client has only one peer (the server) and
                                // uses its fixed sentinel address.
                                let sentinel = match config.role {
                                    RelayRole::Station => sentinel_for_client_id(cid),
                                    RelayRole::Client => {
                                        tunnel_sentinel.unwrap_or_else(|| sentinel_for_client_id(cid))
                                    }
                                };
                                if bound_clients.insert(cid) {
                                    log::info!("Relay tunnel client bound: id={cid} sentinel={sentinel}");
                                }
                                if let Some(gap) =
                                    note_path_packet(&mut last_in_tunnel, Instant::now())
                                        .filter(|g| *g >= PATH_QUIET_REPORT)
                                {
                                    log::info!(
                                        "Relay path back: tunnel after {:.1}s of silence",
                                        gap.as_secs_f32(),
                                    );
                                }
                                let _ = inbound.send((sentinel, payload.to_vec()));
                                continue;
                            }
                        }
                        match parse_binary_probe(&data) {
                            Some((BINARY_KIND_PING, seq)) => {
                                // Echo the probe straight back at the same size.
                                write
                                    .send(Message::Binary(
                                        binary_probe_frame(BINARY_KIND_PONG, config.role, seq, data.len()).into(),
                                    ))
                                    .await
                                    .map_err(|e| e.to_string())?;
                            }
                            Some((BINARY_KIND_PONG, seq)) => {
                                if let Some((pending_seq, sent_at)) = pending_binary {
                                    if seq == pending_seq {
                                        pending_binary = None;
                                        let rtt_ms =
                                            sent_at.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
                                        binary_samples.push_back(rtt_ms);
                                        while binary_samples.len() > BINARY_RTT_WINDOW {
                                            binary_samples.pop_front();
                                        }
                                        if !form_mismatch {
                                            set_binary_stats(
                                                status,
                                                binary_stats(&binary_samples, rtt_ms),
                                                generation,
                                            );
                                        }
                                    }
                                }
                            }
                            _ if form_mismatch => {}
                            _ => {
                                set_status(
                                    status,
                                    RelayPhase::Authenticated,
                                    format!("Relay peer traffic received: {} bytes", data.len()),
                                    true,
                                    true,
                                    None,
                                    generation,
                                );
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = write.send(Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) => {
                        outcome.closed_by_relay = true;
                        return Err("relay closed connection".to_string());
                    }
                    Some(Ok(Message::Frame(_))) => {}
                    Some(Err(e)) => return Err(e.to_string()),
                    None => {
                        outcome.closed_by_relay = true;
                        return Err("relay disconnected".to_string());
                    }
                }
            }
            frame = recv_tcp_line(&mut tcp) => {
                if tcp_frame_out {
                    match std::future::poll_fn(|cx| std::task::Poll::Ready(write.poll_flush_unpin(cx))).await {
                        std::task::Poll::Ready(Ok(())) => tcp_frame_out = false,
                        std::task::Poll::Ready(Err(e)) => return Err(e.to_string()),
                        std::task::Poll::Pending => {}
                    }
                }
                let ready = !tcp_frame_out
                    && matches!(std::future::poll_fn(|cx| std::task::Poll::Ready(write.poll_ready_unpin(cx))).await, std::task::Poll::Ready(Ok(())));
                if ready {
                    write.start_send_unpin(Message::Binary(frame.into())).map_err(|e| e.to_string())?;
                    tcp_frame_out = true;
                } else {
                    tcp_dropped += 1;
                    if let Some(line) = tcp.as_ref() {
                        line.dropped_out.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    if tcp_dropped_said.map_or(true, |t| t.elapsed() >= Duration::from_secs(10)) {
                        // Named in the brief before it was built (working rule 11, build 145).
                        log::info!("Relay: {tcp_dropped} QUIC packets for the TCP line dropped - the line to the relay was busy");
                        tcp_dropped = 0;
                        tcp_dropped_said = Some(Instant::now());
                    }
                }
            }
            pushed = std::future::poll_fn(|cx| write.poll_flush_unpin(cx)), if tcp_frame_out => {
                pushed.map_err(|e| e.to_string())?;
                tcp_frame_out = false;
            }
            maybe_out = recv_uplink(&mut tunnel_uplink) => {
                match maybe_out {
                    Some((addr, bytes)) => {
                        // Station addresses the right client via the client_id from the
                        // sentinel; the client sends id 0 (the relay stamps in the real
                        // number based on which WS connection the frame came from).
                        let client_id = match config.role {
                            RelayRole::Station => client_id_from_sentinel(addr),
                            RelayRole::Client => 0,
                        };
                        // Phase 3b transport policy (additive; both flags off = phase-2
                        // behaviour, UDP-only for audio + wss for the rest). Per role:
                        //  - Client uplink (TX): UDP unless in fallback; wss when in fallback
                        //    OR always during PTT (TX-always-both, safety-critical path section 14.3).
                        //  - Station downlink (RX): always UDP (doubles as the recovery probe
                        //    during fallback); wss additionally while this client is in fallback.
                        // The receiving end de-dups, so a frame on both paths plays once.
                        let audio = is_audio_packet(&bytes);
                        let ptt = audio && audio_ptt_active(&bytes);
                        // First audio frame after a quiet spell: the moment a stream
                        // resumes. Logged on both ends so a delay can be pinned to a
                        // hop instead of inferred - endpoint, transport or network.
                        if let Some((stream, seq)) = audio_identity(&bytes) {
                            let now = Instant::now();
                            let quiet = last_stream_seen
                                .get(&stream)
                                .map_or(true, |t| t.elapsed() > Duration::from_millis(500));
                            if quiet {
                                log::debug!(
                                    "Relay {:?} SEND: stream 0x{:04x} resumes at seq={} (udp={} wss={})",
                                    config.role, stream, seq,
                                    udp_socket.is_some() && udp_token.is_some(),
                                    fallback_requested
                                );
                            }
                            last_stream_seen.insert(stream, now);
                        }
                        let udp_up = udp_socket.is_some() && udp_token.is_some();
                        let (do_udp, do_wss) = if !audio {
                            (false, true) // control/spectrum: wss only
                        } else {
                            match config.role {
                                RelayRole::Client => {
                                    (udp_up && !fallback_requested, fallback_requested || ptt)
                                }
                                RelayRole::Station => {
                                    let e = station_fallback.get(&client_id);
                                    station_audio_paths(
                                        udp_up,
                                        e.map_or(false, |e| e.on),
                                        // No entry at all reads as an expired lease.
                                        e.map_or(FALLBACK_LEASE, |e| e.last_seen.elapsed()),
                                        FALLBACK_LEASE,
                                        e.map_or(false, |e| e.reads_beacon),
                                    )
                                }
                            }
                        };
                        let mut sent_any = false;
                        // The numbers that say whether the saving actually happens. Build
                        // the condition above wrong and every frame goes out twice exactly
                        // as it did before, while every other number looks healthy and
                        // nothing makes a sound.
                        if config.role == RelayRole::Station {
                            if let Some(e) = station_fallback.get_mut(&client_id) {
                                let in_fallback = e.on && e.last_seen.elapsed() < FALLBACK_LEASE;
                                // The defect itself, separate from its consequence: a
                                // client asking for wss that the station does not know
                                // reads a beacon. It tells "the saving did not work" apart
                                // from "the saving never came up".
                                if in_fallback && !e.reads_beacon {
                                    fallback_without_beacon_reader += 1;
                                }
                                if do_udp && in_fallback {
                                    // Per episode, so the line at the end of one means
                                    // something; and session-wide, because that is the
                                    // number that has to read zero.
                                    e.udp_frames_while_on += 1;
                                    udp_audio_while_fallback += 1;
                                    if udp_audio_while_fallback % 500 == 1 {
                                        log::warn!(
                                            "Relay still sending UDP audio to client {client_id} \
                                             while it is on wss ({udp_audio_while_fallback} frames) \
                                             - the duplicate stream this repair exists to remove"
                                        );
                                    }
                                }
                            }
                        }
                        if do_udp {
                            if let (Some(sock), Some(tok)) = (&udp_socket, &udp_token) {
                                udp_seq = udp_seq.wrapping_add(1);
                                if sock
                                    .send(&encode_tlu1(client_id, udp_seq, tok, &bytes))
                                    .await
                                    .is_ok()
                                {
                                    sent_any = true;
                                }
                            }
                        }
                        // wss when requested, or as the fallback when the UDP send did not go.
                        if do_wss || !sent_any {
                            write
                                .send(Message::Binary(encode_tlt1(client_id, &bytes).into()))
                                .await
                                .map_err(|e| e.to_string())?;
                        }
                    }
                    None => {
                        // Uplink channel closed -> disable the branch (no busy-spin).
                        tunnel_uplink = None;
                    }
                }
            }
            maybe_udp = recv_udp(&udp_socket, &mut udp_buf) => {
                // Phase 2 downlink: relay delivers [client_id(1)] + AudioPacket over UDP.
                if let Some(n) = maybe_udp {
                    // Test hook: pretend the network drops inbound UDP for the first N s.
                    let test_drop = test_drop_udp_secs > 0
                        && ready_at.elapsed().as_secs() < test_drop_udp_secs;
                    // A bare beacon arrives as a single byte: the relay forwards
                    // [client_id] and an empty payload. It proves one thing and one
                    // thing only - that this UDP path carries traffic right now -
                    // and it is deliberately kept out of everything else. It is not
                    // peer liveness (that is the tunnel), it is not traffic for the
                    // retry policy to reason about, and it is certainly nothing for
                    // the engine or the dedup window (review finding: an empty
                    // payload must never reach either).
                    if !test_drop && n >= 1 {
                        let now = Instant::now();
                        downlink_received += 1;
                        let gap = note_downlink(
                            &mut last_udp_downlink,
                            &mut udp_healthy_since,
                            now,
                            FALLBACK_DOWNLINK_STALL,
                        );
                        if gap.is_some_and(|g| g >= PATH_QUIET_REPORT) {
                            log::info!(
                                "Relay path back: udp after {:.1}s of silence",
                                gap.unwrap_or_default().as_secs_f32(),
                            );
                        }
                    }
                    if !test_drop && n > 1 {
                        peer_active = true;
                        outcome.mark_traffic();
                        last_peer_activity = Instant::now();
                        let cid = udp_buf[0];
                        let payload = &udp_buf[1..n];
                        // Phase 3: UDP-audio health (client fallback monitor + recovery run).
                        // A gap longer than the stall threshold (re)starts the healthy run,
                        // so recovery only fires after continuous UDP audio (hysteresis).
                        if let Some((stream, seq)) = audio_identity(payload) {
                            let now = Instant::now();
                            // `udp_healthy_since` is fed above, from the downlink,
                            // so that a beacon counts as well as an audio frame.
                            last_udp_audio = Some(now);
                            let quiet = last_stream_in
                                .get(&stream)
                                .map_or(true, |t: &Instant| t.elapsed() > Duration::from_millis(500));
                            if quiet {
                                log::debug!(
                                    "Relay {:?} RECV udp: stream 0x{:04x} resumes at seq={}",
                                    config.role, stream, seq
                                );
                            }
                            last_stream_in.insert(stream, now);
                        }
                        // Phase 3: drop a frame already delivered over wss during overlap.
                        let is_dup = dedup.is_duplicate(cid, payload);
                        if !is_dup {
                            if let Some(inbound) = &tunnel_inbound {
                                let sentinel = match config.role {
                                    RelayRole::Station => sentinel_for_client_id(cid),
                                    RelayRole::Client => {
                                        tunnel_sentinel.unwrap_or_else(|| sentinel_for_client_id(cid))
                                    }
                                };
                                if bound_clients.insert(cid) {
                                    log::info!("Relay UDP client bound: id={cid} sentinel={sentinel}");
                                }
                                let _ = inbound.send((sentinel, payload.to_vec()));
                            }
                        }
                    }
                }
            }
            _ = timer.tick() => {
                if peer_active && last_peer_activity.elapsed() >= PEER_TIMEOUT {
                    peer_active = false;
                    pending_rtt = None;
                    if !bound_clients.is_empty() {
                        bound_clients.clear();
                        log::info!("Relay tunnel clients unbound (peer timeout)");
                    }
                    set_status(
                        status,
                        RelayPhase::WaitingForPeer,
                        "Relay peer timed out; waiting for peer".to_string(),
                        true,
                        false,
                        None,
                        generation,
                    );
                }

                // Phase 3b transport state machine (client, relay-only). Two states:
                //  - UDP (fallback off): request wss if nothing arrived down the path within
                //    the connect window, or the downlink went quiet past
                //    FALLBACK_DOWNLINK_STALL.
                //  - Fallback (on): return to UDP-only once UDP audio has flowed healthily for
                //    FALLBACK_RECOVER (> stall = hysteresis). The station keeps sending UDP
                //    during fallback, so recovery is detectable.
                // A minimum dwell between flips (dwell-cap + rate-limit) prevents flapping. The flip
                // request tunnels an RCF over wss (id 0; the relay stamps the real client id).
                // Since when there is a peer at all. Derived here, on the tick, and
                // deliberately NOT stamped at the three places that set
                // `peer_active`: three places that must agree about one fact drift,
                // and this one only has to be accurate to within a tick while the
                // window it feeds is two seconds (working rule 15).
                if peer_active {
                    peer_active_since.get_or_insert_with(Instant::now);
                } else {
                    peer_active_since = None;
                }
                if config.role == RelayRole::Client && udp_socket.is_some() && peer_active {
                    let now = Instant::now();
                    // Declare the initial transport (UDP) once the tunnel is up, so a
                    // station carrying a stale fallback flag for a reused client_id clears
                    // it. Sent once; a real fallback follows within the connect window if
                    // UDP never arrives.
                    if !declared_initial_transport {
                        let _ = write
                            .send(Message::Binary(
                                encode_tlt1(0, &encode_rcf_fallback(false)).into(),
                            ))
                            .await;
                        declared_initial_transport = true;
                    }
                    // Transport state machine (flip only after the dwell).
                    if last_fallback_flip.elapsed() >= FALLBACK_DWELL {
                        if !fallback_requested {
                            // Is the UDP path carrying anything at all?
                            //
                            // This used to ask whether AUDIO had arrived in the last
                            // 500ms, and to sit behind a second guard asking the same
                            // thing of any audio at all. Both were false exactly when
                            // the path had died, which is the moment this exists for -
                            // the branch written for "UDP audio never arrived" could not
                            // be reached at all. Six review rounds began with that.
                            //
                            // Now it asks the only question that has an answer while the
                            // path is broken: has ANYTHING come down it, beacon or audio.
                            // A station with every channel switched off still beacons, so
                            // "silent because nobody asked for audio" and "silent because
                            // the path is gone" stopped being the same observation, and no
                            // knowledge of channels is needed here.
                            //
                            // A free function, because inline it could be reverted to the
                            // old shape with every test still green - which a review
                            // demonstrated rather than suspected.
                            let stalled = downlink_stalled(
                                peer_active_since.map(|t| t.elapsed()),
                                last_udp_downlink,
                                FALLBACK_CONNECT_TIMEOUT,
                                FALLBACK_DOWNLINK_STALL,
                            );
                            // Why the rescue did NOT fire while there is no audio.
                            //
                            // It exists because the entry was once unreachable and
                            // nothing said so. But it was a LEVEL against a 500ms
                            // threshold, and audio frames arrive every 20ms - so a
                            // single late frame satisfied it, and on a perfectly
                            // healthy link it announced "UDP audio is missing" while
                            // fifty frames a second were arriving. Observed three
                            // times in one afternoon.
                            //
                            // Now it is an EDGE, and the threshold is the one the
                            // decision itself uses. Below that window there is
                            // nothing to explain: the rescue could not have acted
                            // yet either. Above it, with the path reporting alive,
                            // the situation is worth exactly one line - and one
                            // more when it clears, so the episode carries its own
                            // length instead of leaving a reader to subtract
                            // timestamps.
                            //
                            // An edge also survives the case a level could not: all
                            // channels switched off keeps audio absent for as long
                            // as the operator likes, and that must not become a line
                            // every dwell forever.
                            match absence_edge(
                                audio_absent_since.is_some(),
                                stalled,
                                last_udp_audio.map(|t| t.elapsed()),
                                ready_at.elapsed(),
                            ) {
                                AbsenceEvent::Began(without_audio) => {
                                    // The moment audio stopped, not the moment this
                                    // noticed - the two are a threshold apart, and the
                                    // closing line would otherwise under-report every
                                    // episode by exactly that much.
                                    audio_absent_since =
                                        Some(now.checked_sub(without_audio).unwrap_or(now));
                                    log::info!(
                                        "Relay fallback NOT requested: no UDP audio for {:.1}s \
                                         while the path reports alive \
                                         (downlink {:?}ms ago, since_ready={:.1}s)",
                                        without_audio.as_secs_f32(),
                                        last_udp_downlink.map(|t| t.elapsed().as_millis()),
                                        ready_at.elapsed().as_secs_f32(),
                                    );
                                }
                                AbsenceEvent::EndedAudioBack => {
                                    if let Some(began) = audio_absent_since.take() {
                                        log::info!(
                                            "Relay UDP audio is back after {:.1}s",
                                            began.elapsed().as_secs_f32(),
                                        );
                                    }
                                }
                                AbsenceEvent::EndedPathBroken => {
                                    if let Some(began) = audio_absent_since.take() {
                                        log::info!(
                                            "Relay audio was absent for {:.1}s and now the path \
                                             is gone too - the fallback takes it from here",
                                            began.elapsed().as_secs_f32(),
                                        );
                                    }
                                }
                                AbsenceEvent::Nothing => {}
                            }
                            if stalled {
                                let _ = write
                                    .send(Message::Binary(
                                        encode_tlt1(0, &encode_rcf_fallback(true)).into(),
                                    ))
                                    .await;
                                fallback_requested = true;
                                last_fallback_flip = now;
                                last_fallback_heartbeat = now;
                                fallback_flips += 1;
                                set_relay_transport(status, true);
                                // In the terms the DECISION used. It reported the audio
                                // clock while deciding on the downlink clock, so on a
                                // beacon-carrying path the reason it printed was not the
                                // reason it acted on. Both are here now; a log that
                                // describes a different variable than the branch is worse
                                // than no log.
                                let gap_ms = last_udp_downlink.map(|t| t.elapsed().as_millis());
                                let audio_gap_ms = last_udp_audio.map(|t| t.elapsed().as_millis());
                                let reason = if last_udp_downlink.is_none() {
                                    "connect-timeout"
                                } else {
                                    "downlink-stall"
                                };
                                log::info!(
                                    "Relay fallback ON (reason={reason} gap_ms={gap_ms:?} \
                                     audio_gap_ms={audio_gap_ms:?} flips={fallback_flips}) \
                                     - requesting wss audio"
                                );
                            }
                        } else if udp_recovered(
                            udp_healthy_since,
                            last_udp_downlink,
                            FALLBACK_RECOVER,
                            FALLBACK_DOWNLINK_STALL,
                        ) {
                            let _ = write
                                .send(Message::Binary(
                                    encode_tlt1(0, &encode_rcf_fallback(false)).into(),
                                ))
                                .await;
                            fallback_requested = false;
                            last_fallback_flip = now;
                            fallback_flips += 1;
                            set_relay_transport(status, false);
                            log::info!(
                                "Relay fallback OFF (reason=udp-recovered flips={fallback_flips}) - back to UDP"
                            );
                        }
                    }
                    // Say again what we can do, whether or not we are in a fallback.
                    //
                    // The station keeps `reads_beacon` inside the per-client fallback
                    // entry, and that entry is dropped a lease after the last control
                    // frame it heard. Announced once at connect, the fact is therefore
                    // gone fifteen seconds in - and every fallback after that finds a
                    // fresh entry defaulting to "does not read beacons", keeps the
                    // duplicate stream, and the saving this whole patch exists for never
                    // happens. That is not a second-pass fault; it bites on the first
                    // one, fifteen seconds after connecting.
                    //
                    // Repeating it also restores the fact after a STATION restart, which
                    // wipes its per-client state while the client's own session runs on
                    // and would otherwise never mention its capability again.
                    if last_caps_sent.elapsed() >= FALLBACK_HEARTBEAT {
                        let _ = write
                            .send(Message::Binary(
                                encode_tlt1(0, &encode_rcf_caps(CAPS_READS_BEACON)).into(),
                            ))
                            .await;
                        last_caps_sent = now;
                    }
                    // Lease refresh while in fallback, so the station keeps the flag alive
                    // and can expire it if we vanish.
                    if fallback_requested && last_fallback_heartbeat.elapsed() >= FALLBACK_HEARTBEAT {
                        let _ = write
                            .send(Message::Binary(
                                encode_tlt1(0, &encode_rcf_fallback(true)).into(),
                            ))
                            .await;
                        last_fallback_heartbeat = now;
                    }
                }

                // Station: expire per-client fallback state whose lease lapsed (a client
                // that vanished without an off), so it never leaks to a reused client_id.
                if config.role == RelayRole::Station && !station_fallback.is_empty() {
                    // Say so when it happens to a client that is still here. A lease
                    // lapsing under a client that vanished is the mechanism working; one
                    // lapsing under a client that is still bound means the station has
                    // just forgotten something it will need - and it forgets
                    // `reads_beacon` with it.
                    //
                    // This is the third of the three numbers the design asked for and the
                    // only one that costs nothing to observe: it happens every session,
                    // to every client, without anyone having to provoke a network switch.
                    // It was not built, and a whole review round is what found instead
                    // what it would have said out loud.
                    station_fallback.retain(|cid, e| {
                        let alive = e.last_seen.elapsed() < FALLBACK_LEASE;
                        if !alive && bound_clients.contains(cid) {
                            log::warn!(
                                "Relay station: fallback lease expired for client {cid} \
                                 while it is still bound (on={} reads_beacon={}) \
                                 - the station has forgotten what this client can do",
                                e.on,
                                e.reads_beacon
                            );
                        }
                        alive
                    });
                }

                if last_status.elapsed() >= PEER_STATUS_INTERVAL {
                    write
                        .send(Message::Text(peer_frame("status", config.role, config.quic).into()))
                        .await
                        .map_err(|e| e.to_string())?;
                    last_status = Instant::now();
                    // Ride the existing status tick: one line only when frames were
                    // actually discarded, so a healthy link stays quiet and a link that
                    // is throwing audio away says so instead of looking like silence.
                    let report = dedup.take_drop_report();
                    if !report.is_empty() {
                        log::info!("Relay {:?} dedup: {}", config.role, report);
                    }
                    // The client's side of the beacon: how many datagrams the downlink
                    // actually delivered in this window, against the roughly five a
                    // station sends. Reported while a fallback is running, which is when
                    // the question is asked - "was the path dead, or merely gappy".
                    if config.role == RelayRole::Client && fallback_requested {
                        log::info!(
                            "Relay client: {downlink_received} downlink datagrams in the \
                             last window while on wss"
                        );
                    }
                    downlink_received = 0;
                    // What the beacon costs and what it is failing to buy. Both stay
                    // silent at zero, so a station without clients and a session where
                    // the saving works say nothing at all.
                    if config.role == RelayRole::Station
                        && (beacons_sent > 0 || mapping_keepalives > 0)
                    {
                        log::info!(
                            "Relay station: {beacons_sent} beacons to {} client(s), \
                             {mapping_keepalives} mapping keepalive(s) while idle, \
                             {fallback_without_beacon_reader} audio frames to a client on \
                             wss that is not known to read one",
                            bound_clients.len()
                        );
                        beacons_sent = 0;
                        mapping_keepalives = 0;
                        fallback_without_beacon_reader = 0;
                    }
                }

                if last_rtt_ping.elapsed() >= PEER_RTT_INTERVAL {
                    rtt_seq = rtt_seq.wrapping_add(1);
                    let now = Instant::now();
                    write
                        .send(Message::Text(peer_frame_with_seq("ping", config.role, rtt_seq, config.quic).into()))
                        .await
                        .map_err(|e| e.to_string())?;
                    pending_rtt = Some((rtt_seq, now));
                    last_rtt_ping = now;
                }

                // Not in QUIC mode: the probe measures the wss route with an
                // audio-sized frame, and in that mode wss carries no data - it
                // would be 256 bytes a second through the relay for nothing.
                if !config.quic && last_binary_probe.elapsed() >= BINARY_PROBE_INTERVAL {
                    binary_seq = binary_seq.wrapping_add(1);
                    let now = Instant::now();
                    write
                        .send(Message::Binary(
                            binary_probe_frame(BINARY_KIND_PING, config.role, binary_seq, BINARY_PROBE_LEN)
                                .into(),
                        ))
                        .await
                        .map_err(|e| e.to_string())?;
                    pending_binary = Some((binary_seq, now));
                    last_binary_probe = now;
                }

                if last_ping.elapsed() >= PEER_WAIT_PING {
                    let _ = write.send(Message::Ping(Vec::new().into())).await;
                    last_ping = Instant::now();
                }

                // Keep our UDP source-binding + NAT mapping alive so downlink audio
                // keeps reaching us even during long RX-only / idle periods.
                if last_udp_keepalive.elapsed() >= UDP_BEACON_INTERVAL {
                    // A station addresses every bound client, so its keepalives double
                    // as the beacons the client's fallback reads - sent CONTINUOUSLY,
                    // not only during a fallback: the entry test needs beacons already
                    // flowing before the path breaks, or it reads their absence as a
                    // broken path and flaps. With no client bound it still sends one,
                    // to hold its NAT mapping (see `station_udp_targets`).
                    match udp_token.as_deref() {
                        Some(tok) => send_keepalives!(tok),
                        None => last_udp_keepalive = Instant::now(),
                    }
                }
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PeerFrame {
    kind: String,
    role: String,
    seq: Option<u64>,
    /// `proto=quic`: the peer carries QUIC via the relay. Absent on a peer of
    /// before phase 3b, which carries TL frames.
    quic: bool,
}

fn proto_suffix(quic: bool) -> &'static str {
    if quic {
        " proto=quic"
    } else {
        ""
    }
}

fn peer_frame(kind: &str, role: RelayRole, quic: bool) -> String {
    format!("TLR2 kind={kind} role={}{}", role.as_str(), proto_suffix(quic))
}

fn peer_frame_with_seq(kind: &str, role: RelayRole, seq: u64, quic: bool) -> String {
    format!("TLR2 kind={kind} role={} seq={seq}{}", role.as_str(), proto_suffix(quic))
}

fn form_name(quic: bool) -> &'static str {
    if quic {
        "QUIC via the relay"
    } else {
        "the classic relay tunnel"
    }
}

/// The relay's own lines this library reads, by their first word. A peer's text
/// starting with one would read as the relay's, so the relay refuses to forward
/// such a text (`RELAY_LINES` in `thetislink-relay`, which must hold every word
/// here - pinned by `the_relay_refuses_every_line_this_library_reads`, which
/// reads these constants from this file: a new one is pinned without a list).
const RELAY_OK: &str = "OK";
const RELAY_ERR: &str = "ERR";
const RELAY_CHAT_TICKET_ROTATE: &str = "chat-ticket-rotate";
const RELAY_UDP_TOKEN_ROTATE: &str = "udp-token-rotate";
const RELAY_UDP_SEEN: &str = "udp-seen";

/// What the screen says when the two sides do not carry the same form. Since
/// 2.13 every program carries QUIC and there is no switch left, so the other
/// form is another version: that is what the user can act on.
fn form_mismatch_message(own_quic: bool) -> String {
    if own_quic {
        "The other side runs an older version of ThetisLink (before 2.13) - update it to this version".to_string()
    } else {
        "The other side runs a newer version of ThetisLink (2.13 or later) - update this side".to_string()
    }
}

/// Resolve the relay's UDP port and publish the session's grant. Without a
/// token or an address there is none, and QUIC via the relay cannot run: said
/// in the log, the route layer stays without a session.
async fn publish_grant(
    config: &RelayConfig,
    token: &Option<String>,
    public: Option<SocketAddr>,
    grants: Option<&watch::Sender<Option<UdpGrant>>>,
    session: u64,
) {
    let Some(grants) = grants else { return };
    let Some(tok) = token else {
        log::warn!("Relay offers no UDP token - QUIC via the relay cannot run on this relay");
        grants.send_replace(None);
        return;
    };
    let port = config.udp_port.unwrap_or(DEFAULT_UDP_PORT);
    match resolve_udp(&config.url, port).await {
        Ok((relay, fallback)) => {
            grants.send_replace(Some(UdpGrant { relay, fallback, public, token: hex32(tok), session }));
            log::info!(
                "Relay: QUIC grant for session {session} (<relay>:{port}, {}{})",
                family(relay),
                if fallback.is_some() { ", IPv4 beside it" } else { "" }
            );
        }
        Err(e) => {
            log::warn!("Relay UDP address not resolved - QUIC via the relay waits: {e}");
            grants.send_replace(None);
        }
    }
}

fn parse_peer_frame(text: &str) -> Option<PeerFrame> {
    let mut parts = text.split_whitespace();
    if parts.next()? != "TLR2" {
        return None;
    }

    let mut kind = None;
    let mut role = None;
    let mut seq = None;
    let mut quic = false;
    for part in parts {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key {
            "kind" => kind = Some(value.to_string()),
            "role" => role = Some(value.to_string()),
            "seq" => seq = value.parse::<u64>().ok(),
            "proto" => quic = value == "quic",
            _ => {}
        }
    }

    Some(PeerFrame {
        kind: kind.unwrap_or_else(|| "status".to_string()),
        role: role.unwrap_or_else(|| "peer".to_string()),
        seq,
        quic,
    })
}

fn peer_frame_message(frame: &PeerFrame) -> String {
    match frame.kind.as_str() {
        "hello" => format!("Relay peer online: {} handshake received", frame.role),
        "status" => format!("Relay peer online: {} heartbeat received", frame.role),
        "ping" => format!("Relay peer online: {} RTT probe received", frame.role),
        "pong" => format!("Relay peer online: {} RTT reply received", frame.role),
        other => format!("Relay peer online: {} sent {other}", frame.role),
    }
}

fn peer_frame_message_with_rtt(frame: &PeerFrame, rtt_ms: u32) -> String {
    format!("Relay peer online: {} RTT {} ms", frame.role, rtt_ms)
}

/// Build a binary probe frame: magic (4) + kind (1) + role (1) + seq (8 LE),
/// padded with zeros to `len` (an audio-sized payload). Header is 14 bytes.
fn binary_probe_frame(kind: u8, role: RelayRole, seq: u64, len: usize) -> Vec<u8> {
    let total = len.max(14);
    let mut buf = Vec::with_capacity(total);
    buf.extend_from_slice(&BINARY_MAGIC);
    buf.push(kind);
    buf.push(match role {
        RelayRole::Station => 0,
        RelayRole::Client => 1,
    });
    buf.extend_from_slice(&seq.to_le_bytes());
    buf.resize(total, 0);
    buf
}

/// Parse a binary probe frame, returning (kind, seq) if the magic matches.
fn parse_binary_probe(data: &[u8]) -> Option<(u8, u64)> {
    if data.len() < 14 || data[0..4] != BINARY_MAGIC {
        return None;
    }
    let kind = data[4];
    let seq = u64::from_le_bytes(data[6..14].try_into().ok()?);
    Some((kind, seq))
}

/// Wrap TL-frame bytes in a TLT1 tunnel frame (payload is clamped to u16).
fn encode_tlt1(client_id: u8, payload: &[u8]) -> Vec<u8> {
    let len = payload.len().min(u16::MAX as usize);
    let mut buf = Vec::with_capacity(TLT1_HEADER_LEN + len);
    buf.extend_from_slice(&TLT1_MAGIC);
    buf.push(TLT1_VERSION);
    buf.push(client_id);
    buf.extend_from_slice(&(len as u16).to_le_bytes());
    buf.extend_from_slice(&payload[..len]);
    buf
}

/// Parse a TLT1 tunnel frame -> (client_id, payload) if the magic matches and the
/// frame is complete. Other magics (e.g. TLB1) return None (non-fatal).
fn parse_tlt1(data: &[u8]) -> Option<(u8, &[u8])> {
    if data.len() < TLT1_HEADER_LEN || data[0..4] != TLT1_MAGIC {
        return None;
    }
    let client_id = data[5];
    let len = u16::from_le_bytes([data[6], data[7]]) as usize;
    let end = TLT1_HEADER_LEN + len;
    if data.len() < end {
        return None;
    }
    Some((client_id, &data[TLT1_HEADER_LEN..end]))
}

/// Compute rolling min/avg/max over the recent binary-probe RTT samples.
fn binary_stats(samples: &VecDeque<u32>, last_ms: u32) -> BinaryProbeStats {
    let n = samples.len().max(1) as u32;
    let min_ms = samples.iter().copied().min().unwrap_or(last_ms);
    let max_ms = samples.iter().copied().max().unwrap_or(last_ms);
    let sum: u64 = samples.iter().map(|&v| u64::from(v)).sum();
    let avg_ms = (sum / u64::from(n)) as u32;
    BinaryProbeStats {
        last_ms,
        min_ms,
        max_ms,
        avg_ms,
        samples: n,
    }
}

/// Record binary-probe stats. Keeps the existing status message (which tracks
/// the text heartbeat) and only updates the binary stats; logs once on the first
/// successful round-trip so the log is not spammed every probe.
fn set_binary_stats(handle: &RelayStatusHandle, stats: BinaryProbeStats, generation: u64) {
    let mut status = handle.state.lock().unwrap_or_else(|e| e.into_inner());
    let first = status.binary.is_none();
    status.phase = RelayPhase::Authenticated;
    status.connected = true;
    status.peer_seen = true;
    status.last_error = None;
    status.binary = Some(stats);
    status.generation = generation;
    if first {
        log::info!("Relay binary probe OK: RTT {} ms", stats.last_ms);
    }
}

async fn wait_for_change(
    config_rx: &mut watch::Receiver<RelayConfig>,
    shutdown_rx: &mut watch::Receiver<bool>,
) {
    tokio::select! {
        _ = config_rx.changed() => {}
        _ = shutdown_rx.changed() => {}
    }
}

fn set_status(
    handle: &RelayStatusHandle,
    phase: RelayPhase,
    message: String,
    connected: bool,
    peer_seen: bool,
    last_error: Option<String>,
    generation: u64,
) {
    let mut status = handle.state.lock().unwrap_or_else(|e| e.into_inner());
    let next_rtt = if connected && peer_seen {
        status.peer_rtt_ms
    } else {
        None
    };
    let next_binary = if connected && peer_seen {
        status.binary
    } else {
        None
    };
    let changed = status.phase != phase
        || status.message != message
        || status.connected != connected
        || status.peer_seen != peer_seen
        || status.last_error != last_error
        || status.peer_rtt_ms != next_rtt
        || status.binary != next_binary;
    let log_changed = status.phase != phase
        || status.connected != connected
        || status.peer_seen != peer_seen
        || status.last_error != last_error;
    status.phase = phase;
    status.message = message;
    status.connected = connected;
    status.peer_seen = peer_seen;
    status.last_error = last_error;
    status.peer_rtt_ms = next_rtt;
    status.binary = next_binary;
    status.generation = generation;
    if changed && log_changed {
        log::info!("Relay status: {}", status.message);
    }
    // Like the RTT and the probe above: without a session there is no transport to
    // show. The flag lives in this handle, which outlives every session and the
    // operator's own disconnect; the session's own `fallback_requested` starts again
    // at false, so the branch that turns the flag off never ran again, and Android
    // showed "TCP fallback" from 08:36 until the app restarted at 11:36 (field,
    // 2026-09-28). Every call inside a session passes connected=true, every call
    // outside one false (reviewers, round 43).
    if !connected && std::mem::replace(&mut status.transport_fallback, false) {
        log::info!("Relay transport: fallback indicator cleared - no relay session now");
    }
}

/// Run the relay thread's body, and if it panics, say that there is no session.
///
/// Nothing wrote the status after a panic in the relay thread: it stayed on what
/// the last session had set - connected, with the fallback on if it was - for good,
/// on both screens (reviewer, round 44). The two failures before the loop starts
/// (no runtime, no thread) already report themselves the same way.
fn run_reporting_a_panic(handle: &RelayStatusHandle, body: impl FnOnce()) -> bool {
    if let Err(p) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        let what = p
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_string());
        log::error!("Relay monitor stopped by a panic: {what}");
        set_status(
            handle,
            RelayPhase::Error,
            format!("Relay monitor stopped: {what}"),
            false,
            false,
            Some(what),
            0,
        );
        return true;
    }
    false
}

/// Phase 3c: update the transport-fallback flag shown by the client indicator. Mutates only
/// that field, so it survives a set_status refresh within a session; `set_status` clears
/// it when there is no session.
fn set_relay_transport(handle: &RelayStatusHandle, fallback: bool) {
    handle
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .transport_fallback = fallback;
}

/// Store the chat ticket the relay just handed out.
///
/// Mutates only that field, so it survives the next `set_status` refresh - same
/// reason as the transport flag above.
fn set_chat_ticket(handle: &RelayStatusHandle, ticket: Option<String>) {
    handle
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .chat_ticket = ticket;
}

fn set_peer_rtt(
    handle: &RelayStatusHandle,
    message: String,
    rtt_ms: u32,
    generation: u64,
) {
    let mut status = handle.state.lock().unwrap_or_else(|e| e.into_inner());
    let log_changed = status.phase != RelayPhase::Authenticated
        || !status.connected
        || !status.peer_seen
        || status.last_error.is_some();
    status.phase = RelayPhase::Authenticated;
    status.message = message;
    status.connected = true;
    status.peer_seen = true;
    status.last_error = None;
    status.peer_rtt_ms = Some(rtt_ms);
    status.generation = generation;
    if log_changed {
        log::info!("Relay status: {}", status.message);
    }
}

/// Turn the relay's `ERR ...` hello rejection into a clear reason for the UI, so the
/// user sees *why* a connection was refused (device/connection/data limits, block,
/// bad key) instead of a raw code. Unknown codes fall back to the raw text.
fn reject_reason(err: &str) -> String {
    let reason = match err.trim() {
        "ERR relay full" => "too many simultaneous connections for this station",
        "ERR too many devices" => "too many devices for this station",
        "ERR station data limit reached" => "this station's monthly data limit has been reached",
        "ERR device blocked" => "this device has been blocked by the administrator",
        "ERR device pending approval" => "this device is waiting for administrator approval",
        "ERR unauthorized" => "invalid key or no access to this station",
        "ERR device check failed" => "could not verify this device, please try again",
        other => return format!("relay rejected connection: {other}"),
    };
    format!("Relay rejected: {reason}")
}

fn short_error(err: &str) -> String {
    const MAX: usize = 180;
    let mut chars = err.chars();
    let mut out: String = chars.by_ref().take(MAX).collect();
    if chars.next().is_some() {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod recovery_tests {
    use super::udp_recovered;
    use std::time::{Duration, Instant};

    const RECOVER: Duration = Duration::from_secs(3);
    const STALL: Duration = Duration::from_millis(500);

    /// The failure that produced 1829 transport flips: a UDP path that died
    /// leaves `healthy_since` frozen, and "old enough" alone reads as recovered
    /// forever.
    #[test]
    fn a_dead_udp_path_does_not_read_as_recovered() {
        // The field case was 96 minutes. On Windows an Instant counts from boot,
        // so on a machine up for less than that the plain subtraction panics
        // ("overflow when subtracting duration from instant") and this test
        // failed for the first hour and a half after every restart. Ten seconds
        // is long ago too: the stall limit is half a second.
        let now = Instant::now();
        // The fallback checked too: a machine up for less than ten seconds has
        // no "long ago" to give, and then there is nothing to test (review 83).
        let Some(long_ago) = now
            .checked_sub(Duration::from_secs(5760))
            .or_else(|| now.checked_sub(Duration::from_secs(10)))
        else {
            // Said out loud, so a skip does not read like a pass (review 84).
            eprintln!("SKIPPED a_dead_udp_path_does_not_read_as_recovered: machine up for under ten seconds");
            return;
        };
        assert!(
            !udp_recovered(Some(long_ago), Some(long_ago), RECOVER, STALL),
            "a healthy run that ended 96 minutes ago is not a recovery"
        );
    }

    #[test]
    fn a_run_that_is_long_enough_and_still_live_is_a_recovery() {
        let started = Instant::now() - Duration::from_secs(4);
        let just_now = Instant::now();
        assert!(udp_recovered(Some(started), Some(just_now), RECOVER, STALL));
    }

    #[test]
    fn a_fresh_run_is_not_yet_a_recovery() {
        // Audio is arriving, but not for long enough - that is the hysteresis.
        let started = Instant::now() - Duration::from_millis(200);
        assert!(!udp_recovered(Some(started), Some(Instant::now()), RECOVER, STALL));
    }

    #[test]
    fn no_udp_audio_at_all_is_never_a_recovery() {
        assert!(!udp_recovered(None, None, RECOVER, STALL));
        assert!(!udp_recovered(Some(Instant::now() - Duration::from_secs(9)), None, RECOVER, STALL));
    }
}

/// The trust under wss:// (eMudhra, 2026-10-17). Eight eMudhra intermediate keys
/// could be recovered from their public certificates, and the bundled Mozilla roots
/// carry no revocation, so the old build trusted a relay certificate made with one
/// of them. rustls-webpki 0.103.16 refuses every chain that holds one of those keys.
#[cfg(test)]
mod tls_trust_tests {
    use rustls::client::danger::ServerCertVerifier;
    use rustls::client::WebPkiServerVerifier;
    use rustls::pki_types::{ServerName, UnixTime};
    use std::sync::Arc;

    /// EM-DV-TLS-CA-G3B: its public key (P-384, uncompressed point), one of the eight
    /// in the distrust list of rustls-webpki 0.103.16.
    const EM_DV_TLS_CA_G3B_KEY: &str = concat!(
        "04e79ed9c04a9773c4735e51111da85ac2bd5e64ab374b80e12cbc2038ec1e4f0e3b812fc9859e1e67afb04e2f8bc97f",
        "7485ba65aa911ee8a4e45b62d5cc03d56be678b3c38cb4dc937ca2460e238453c892935d29d44f91e15606b63ccedab468",
    );

    struct PublicKey(Vec<u8>);

    impl rcgen::PublicKeyData for PublicKey {
        fn der_bytes(&self) -> &[u8] {
            &self.0
        }
        fn algorithm(&self) -> &rcgen::SignatureAlgorithm {
            &rcgen::PKCS_ECDSA_P384_SHA384
        }
    }

    /// A relay certificate for `key`, issued by a root of our own that the verifier
    /// trusts, checked the way the wss:// connect checks it.
    fn verify(key: &impl rcgen::PublicKeyData) -> Result<(), rustls::Error> {
        let root_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let mut root = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        root.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let root = root.self_signed(&root_key).unwrap();
        let relay = rcgen::CertificateParams::new(vec!["relay.example".to_string()])
            .unwrap()
            .signed_by(key, &root, &root_key)
            .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(root.der().clone()).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider).build().unwrap();
        let name = ServerName::try_from("relay.example").unwrap();
        verifier.verify_server_cert(relay.der(), &[], &name, &[], UnixTime::now()).map(|_| ())
    }

    #[test]
    fn a_relay_certificate_with_a_leaked_emudhra_key_is_refused() {
        let ordinary = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
        assert!(verify(&ordinary).is_ok(), "the same chain with an ordinary key must pass");
        let hex = EM_DV_TLS_CA_G3B_KEY;
        let leaked = PublicKey((0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect());
        let refused = verify(&leaked);
        assert!(format!("{refused:?}").contains("CertDistrusted"), "{refused:?}");
    }
}

#[cfg(test)]
mod tests {
    fn worked(stood: Duration) -> SessionOutcome {
        SessionOutcome {
            carried_traffic: true,
            traffic_since: Some(Instant::now() - stood),
            ..Default::default()
        }
    }

    /// At once only for a session that stood, broke on an error, and was not
    /// refused or closed by the relay (builds 95, 96).
    #[test]
    fn only_a_working_session_that_broke_reconnects_at_once() {
        let now = Instant::now();
        let mut r = Recovery::default();
        assert_eq!(r.next_wait(&worked(Duration::from_secs(30)), true, now, 0.5), Duration::ZERO, "the network switch");
        for (o, err, what) in [
            (SessionOutcome::default(), true, "no traffic"),
            (worked(Duration::from_millis(300)), true, "stood 0.3 s: a line that passes one frame and dies"),
            (worked(Duration::from_secs(30)), false, "ended without an error: settings saved unchanged"),
            (SessionOutcome { rejected: Some("ERR device blocked".into()), ..worked(Duration::from_secs(30)) }, true, "refused"),
            (SessionOutcome { closed_by_relay: true, ..worked(Duration::from_secs(30)) }, true, "closed by the relay"),
        ] {
            let mut r = Recovery::default();
            assert_eq!(r.next_wait(&o, err, now, 0.5), RECONNECT_DELAY, "{what}: the plain 5 s");
        }
    }

    /// A session that hangs - building or running - ends on a change of
    /// network; a change from before it started does not end it; a session
    /// that ends by itself gives its own result (builds 186, 187; review of
    /// 186: that the change fires was seen by no test).
    #[tokio::test]
    async fn a_change_of_network_ends_a_session_even_while_it_is_built() {
        let (tx, mut rx) = watch::channel(0_u64);
        // A session that never ends, and a change while it runs.
        let hanging = std::future::pending::<Result<(), String>>();
        let change = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            tx.send_modify(|n| *n += 1);
        };
        let ((result, switched), ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(until_network_switch(hanging, &mut rx), change)
        })
        .await
        .expect("a change of network must end a hanging session");
        assert!(switched);
        assert_eq!(result, Err("the device's network changed".to_string()));
        // A change from before the session, marked seen: it runs on that network.
        tx.send_modify(|n| *n += 1);
        rx.borrow_and_update();
        let ends = async { Ok::<(), String>(()) };
        assert_eq!(until_network_switch(ends, &mut rx).await, (Ok(()), false));
        // A session that fails by itself keeps its own error.
        let fails = async { Err::<(), String>("os error 103".to_string()) };
        assert_eq!(until_network_switch(fails, &mut rx).await, (Err("os error 103".to_string()), false));
    }

    /// A session ended because the device changed networks starts again at
    /// once, however short it stood - and a new network that is not up yet
    /// walks the ladder (build 186).
    #[test]
    fn a_change_of_network_starts_a_new_session_at_once() {
        let now = Instant::now();
        let mut r = Recovery::default();
        let changed = SessionOutcome { network_changed: true, ..Default::default() };
        assert_eq!(r.next_wait(&changed, true, now, 0.5), Duration::ZERO, "no traffic yet: still at once");
        let local = SessionOutcome { failed_locally: true, ..Default::default() };
        assert_eq!(r.next_wait(&local, true, now, 0.5), LOCAL_RETRY_LADDER[0], "the new network not up yet");
    }

    /// After a working session broke, attempts that fail inside the device walk
    /// the ladder, then the 5 s; a failure that reached the relay leaves it at once.
    #[test]
    fn local_failures_after_a_switch_walk_the_ladder() {
        let now = Instant::now();
        let local = SessionOutcome { failed_locally: true, ..Default::default() };
        let mut r = Recovery::default();
        assert_eq!(r.next_wait(&worked(Duration::from_secs(30)), true, now, 0.5), Duration::ZERO);
        for step in LOCAL_RETRY_LADDER {
            assert_eq!(r.next_wait(&local, true, now, 0.5), step);
        }
        let last = LOCAL_RETRY_LADDER[LOCAL_RETRY_LADDER.len() - 1];
        assert_eq!(r.next_wait(&local, true, now, 0.5), last, "stays on the last step while local");
        assert_eq!(r.next_wait(&local, true, now, 0.5), last);
        let at_relay = SessionOutcome::default();
        assert_eq!(r.next_wait(&at_relay, true, now, 0.5), RECONNECT_DELAY, "until a failure reaches the relay");

        let mut r = Recovery::default();
        r.next_wait(&worked(Duration::from_secs(30)), true, now, 0.5);
        assert_eq!(r.next_wait(&local, true, now, 0.5), LOCAL_RETRY_LADDER[0]);
        assert_eq!(r.next_wait(&at_relay, true, now, 0.5), RECONNECT_DELAY, "reached the relay: the 5 s");
        assert_eq!(r.next_wait(&local, true, now, 0.5), RECONNECT_DELAY, "the ladder is left");

        let mut fresh = Recovery::default();
        assert_eq!(fresh.next_wait(&local, true, now, 0.5), RECONNECT_DELAY, "no ladder without a session that broke");
    }

    /// A relay name that does not resolve marks the attempt as failed locally -
    /// the path a phone takes when the new network has no DNS yet. `.invalid` is
    /// reserved and never resolves (RFC 6761), with or without a network.
    #[test]
    fn an_unresolvable_relay_name_is_a_local_failure() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let config = RelayConfig {
                enabled: true,
                url: "wss://relay.invalid/ws".into(),
                station: "ST".into(),
                token: "t".into(),
                role: RelayRole::Client,
                instance: String::new(),
                name: String::new(),
                udp_port: None,
                quic: false,
            };
            let status = RelayStatusHandle { state: Arc::new(Mutex::new(RelayStatus::disabled())) };
            let (_ctx, mut config_rx) = watch::channel(config.clone());
            let (_stx, mut shutdown_rx) = watch::channel(false);
            let mut outcome = SessionOutcome::default();
            let r = run_connection(&config, &status, 0, &mut config_rx, &mut shutdown_rx, None, None, None, &mut outcome).await;
            assert!(r.is_err());
            assert!(outcome.failed_locally, "{r:?}");
            assert!(!outcome.carried_traffic);
        });
    }

    /// The local kinds of I/O error, and one that is not.
    #[test]
    fn a_local_failure_is_told_from_the_rest() {
        use std::io::{Error, ErrorKind};
        assert!(is_local_failure(&Error::from(ErrorKind::NetworkUnreachable)));
        assert!(is_local_failure(&Error::from(ErrorKind::HostUnreachable)));
        assert!(is_local_failure(&Error::from(ErrorKind::NetworkDown)));
        assert!(is_local_failure(&Error::from(ErrorKind::AddrNotAvailable)), "the old network's source address gone");
        assert!(!is_local_failure(&Error::from(ErrorKind::ConnectionRefused)), "the relay host said no");
    }

    /// A connect that failed with an I/O error of no local kind still counts as
    /// local when the relay's name does not resolve now - the lookup failure
    /// inside the connect (build 141). A name that resolves, with the same
    /// error, does not: then the relay failed. A local kind is local either way.
    #[test]
    fn a_connect_that_cannot_find_the_relay_failed_locally() {
        use std::io::{Error, ErrorKind};
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let no_kind = Error::other("failed to lookup address information");
            assert!(connect_failed_locally(&no_kind, "wss://relay.invalid/ws").await, "the name does not resolve");
            assert!(!connect_failed_locally(&no_kind, "ws://127.0.0.1/ws").await, "it resolves: the relay failed");
            let refused = Error::from(ErrorKind::ConnectionRefused);
            assert!(!connect_failed_locally(&refused, "ws://127.0.0.1/ws").await, "the relay host said no");
            let unreachable = Error::from(ErrorKind::NetworkUnreachable);
            assert!(connect_failed_locally(&unreachable, "ws://127.0.0.1/ws").await, "a local kind is local");
        });
    }

    #[test]
    fn the_relay_host_and_port_are_read_from_its_url() {
        assert_eq!(relay_host_port("wss://relay.example:443/x"), Some(("relay.example".into(), 443)));
        assert_eq!(relay_host_port("wss://relay.example/x"), Some(("relay.example".into(), 443)));
        assert_eq!(relay_host_port("ws://10.0.0.1/x"), Some(("10.0.0.1".into(), 80)));
    }

    /// The wiring, which no test drives (there is no test relay): both ways the
    /// relay ends a connection mark it, and the loop waits what the decision
    /// says. That they are there, not that they run - the proof is the log line
    /// "reconnecting at once" in a measurement over a network switch.
    #[test]
    fn the_loop_and_the_relay_side_endings_use_the_decision() {
        let src = include_str!("lib.rs");
        let body = &src[..src.find("#[cfg(test)]
mod tests").unwrap()];
        assert_eq!(body.matches("outcome.closed_by_relay = true;").count(), 2, "Close and end of stream");
        assert_eq!(body.matches("recovery.next_wait(&outcome").count(), 1);
        // The connect's own failure is judged by the rule that also looks the
        // name up again (build 141), not by the kind of error alone.
        assert_eq!(body.matches("connect_failed_locally(io, &config.url).await").count(), 1);
        assert_eq!(body.matches("outcome.mark_traffic();").count(), 3, "the three receive paths");
        assert!(!body.contains("sleep(RECONNECT_DELAY)"), "no wait outside the decision");
        assert_eq!(body.matches("tokio::time::sleep(wait)").count(), 1, "the loop waits what was decided");
        // The two log lines named before the build (working rule 11): the field
        // proof of this patch, so their disappearing should not go unnoticed.
        assert!(body.contains("Relay: reconnecting at once - the session carried traffic and ended on a transport error"));
        assert!(body.contains("Relay: the network is not up yet - trying again in {} ms"));
        assert!(body.contains("Relay: the network came up - trying now"));
        // The phone's network signal ends a ladder wait, and is read as seen at
        // the start of every attempt (build 98).
        assert_eq!(body.matches("network_came_up(&mut network_rx, waiting_for_the_network)").count(), 1);
        assert_eq!(body.matches("network_rx.borrow_and_update();").count(), 1);
        // A change of network ends a running session and is read as seen at
        // the start of every attempt (build 186).
        // Seen before the session, and the session against it (review of 186,
        // W5b: after the session a change during the wait went unseen).
        assert_eq!(body.matches("change_rx.borrow_and_update();").count(), 1);
        let order = body.find("change_rx.borrow_and_update();").unwrap();
        assert!(order < body.find("let mut outcome = SessionOutcome::default();").unwrap());
        assert!(order < body.find("let (run_result, switched) = until_network_switch(").unwrap());
        assert_eq!(body.matches("let (run_result, switched) = until_network_switch(").count(), 1);
        assert_eq!(body.matches("outcome.network_changed = switched;").count(), 1);
        assert!(body.contains("Relay: the device's network changed - ending this session to start one on the new network"));
        assert!(body.contains("Relay: reconnecting at once on the new network"));
    }

    /// Only a ladder wait - the network was not there - is one a network coming
    /// up may end: not the plain 5 s, and not before a session broke.
    #[test]
    fn only_the_ladder_waits_for_the_network() {
        let now = Instant::now();
        let mut r = Recovery::default();
        let local = SessionOutcome { failed_locally: true, ..Default::default() };
        r.next_wait(&local, true, now, 0.5);
        assert!(!r.waits_for_the_network(), "no ladder without a session that broke");
        let mut stood = SessionOutcome::default();
        stood.traffic_since = Some(now - Duration::from_secs(30));
        assert_eq!(r.next_wait(&stood, true, now, 0.5), Duration::ZERO);
        assert_eq!(r.next_wait(&local, true, now, 0.5), LOCAL_RETRY_LADDER[0]);
        assert!(r.waits_for_the_network(), "a ladder step");
        r.next_wait(&SessionOutcome::default(), true, now, 0.5);
        assert!(!r.waits_for_the_network(), "the plain 5 s");
    }

    /// The signal ends a wait only while waiting for the network, and a signal
    /// that came before is not taken for a new one.
    #[tokio::test]
    async fn a_network_coming_up_ends_only_a_ladder_wait() {
        let (tx, mut rx) = watch::channel(0_u64);
        let short = Duration::from_millis(50);
        tx.send_modify(|n| *n += 1);
        rx.borrow_and_update();
        assert!(tokio::time::timeout(short, network_came_up(&mut rx, true)).await.is_err(), "an old signal");
        tx.send_modify(|n| *n += 1);
        assert!(tokio::time::timeout(short, network_came_up(&mut rx, false)).await.is_err(), "not waiting");
        assert!(tokio::time::timeout(short, network_came_up(&mut rx, true)).await.is_ok(), "a new signal");
        drop(tx);
        assert!(tokio::time::timeout(short, network_came_up(&mut rx, true)).await.is_err(), "no sender: never");
    }

    /// The spread keeps the wait within 4 to 6 s, and never at zero.
    #[test]
    fn the_spread_stays_around_five_seconds() {
        let o = SessionOutcome::default();
        let now = Instant::now();
        assert_eq!(Recovery::default().next_wait(&o, true, now, 0.0), Duration::from_secs(4));
        assert!(Recovery::default().next_wait(&o, true, now, 0.999) < Duration::from_secs(6));
        assert!(Recovery::default().next_wait(&o, true, now, 0.999) > Duration::from_millis(5_990));
        for _ in 0..50 {
            let s = spread_now();
            assert!((0.0..1.0).contains(&s), "{s}");
        }
    }

    use super::*;

    /// Without a relay session there is no transport to show.
    ///
    /// The fallback flag lives in the status handle, which outlives a session -
    /// and the operator's own disconnect. Only the session's `fallback_requested`
    /// started again at false, so a session that ended in fallback left the
    /// indicator on "TCP fallback" until the app was restarted, while the audio ran
    /// over UDP (field, 2026-09-28: from 08:36 to 11:36, through a manual reconnect).
    ///
    /// Cleared where the status says there is no session - in `set_status`, like the
    /// RTT and the probe beside it - rather than at the start of the next one: in
    /// between, while reconnecting, after the operator's own disconnect, or with the
    /// relay switched off, Android showed "TCP fallback" on a disconnected screen
    /// (reviewers, round 43). And a test reaches `set_status`; a call inside
    /// `run_connection` it could not.
    #[test]
    fn no_session_means_no_fallback_shown() {
        let h = RelayStatusHandle {
            state: Arc::new(Mutex::new(RelayStatus::disabled())),
        };
        let fallback = |h: &RelayStatusHandle| h.snapshot().transport_fallback;
        // Inside a session a status change leaves it alone.
        set_relay_transport(&h, true);
        set_status(&h, RelayPhase::Authenticated, "up".into(), true, true, None, 1);
        assert!(fallback(&h), "a status change inside a session must not clear it");
        // Nor the peer timeout: still connected, the peer gone, audio still on wss.
        // The RTT beside it clears on `peer_seen` too; this one must not (reviewer,
        // round 44), so a tidy-up that makes the two conditions alike fails here.
        set_status(&h, RelayPhase::WaitingForPeer, "peer gone".into(), true, false, None, 1);
        assert!(fallback(&h), "the peer timeout is inside the session");
        // The session ends: every state without one clears it.
        for phase in [RelayPhase::Error, RelayPhase::Connecting, RelayPhase::Disabled] {
            set_relay_transport(&h, true);
            set_status(&h, phase.clone(), "down".into(), false, false, None, 1);
            assert!(!fallback(&h), "{phase:?} without a session still showed the fallback");
            // A second time, with nothing left to clear.
            set_status(&h, phase, "down".into(), false, false, None, 1);
            assert!(!fallback(&h));
        }
    }

    /// A panic in the relay thread leaves no session behind, and says so.
    ///
    /// Nothing wrote the status after one: it stayed on whatever the last session
    /// had set - connected, and the fallback on if it was - for good, on both
    /// screens (reviewer, round 44).
    #[test]
    fn a_panicking_monitor_leaves_no_session_on_screen() {
        let h = RelayStatusHandle {
            state: Arc::new(Mutex::new(RelayStatus::disabled())),
        };
        set_status(&h, RelayPhase::Authenticated, "up".into(), true, true, None, 1);
        set_relay_transport(&h, true);
        assert!(run_reporting_a_panic(&h, || panic!("a fault somewhere in the session")));
        let s = h.snapshot();
        assert_eq!(s.phase, RelayPhase::Error);
        assert!(!s.connected, "a dead monitor still showed a connection");
        assert!(!s.transport_fallback, "a dead monitor still showed the fallback");
        assert!(s.message.contains("a fault somewhere in the session"), "{}", s.message);
        // A formatted panic carries a String, not a &str (reviewer, round 45).
        let n = 7;
        assert!(run_reporting_a_panic(&h, || panic!("fault number {n}")));
        assert!(h.snapshot().message.contains("fault number 7"), "{}", h.snapshot().message);
        // And a body that returns normally changes nothing.
        set_status(&h, RelayPhase::Authenticated, "up".into(), true, true, None, 1);
        assert!(!run_reporting_a_panic(&h, || {}));
        assert!(h.snapshot().connected);
    }

    /// A stop handle knows whether it has been stopped, and says so in both
    /// directions.
    ///
    /// This is what the Android bridge lacked. It keeps the previous monitor's
    /// handle in a static slot to stop it if it is still running, but the slot
    /// is never emptied on a clean shutdown - so every restart found a stale
    /// handle and warned "a relay monitor was still running" about one that had
    /// stopped a second earlier. Two restarts in the field log of 2026-09-24,
    /// two untrue warnings.
    #[test]
    fn a_stop_handle_knows_whether_it_has_been_stopped() {
        let (tx, _rx) = watch::channel(false);
        let h = RelayStopHandle { shutdown_tx: tx };

        assert!(!h.is_stopped(), "a fresh handle belongs to a running monitor");
        h.stop();
        assert!(h.is_stopped(), "and it notices its own stop");

        // Stopping twice is documented as harmless and must stay that way: the
        // bridge calls stop() on the previous handle whatever the answer above.
        h.stop();
        assert!(h.is_stopped());
    }

    /// Two handles of the same monitor recognise each other; handles of two
    /// monitors do not.
    ///
    /// This is what lets the bridge empty its slot safely. Emptying it on every
    /// shutdown is wrong in exactly one case, and it is the case the slot was
    /// built for: on a network switch the next bridge has already put its own
    /// handle there, and throwing that away would leave two live monitors
    /// evicting each other with nothing watching.
    #[test]
    fn stop_handles_know_whether_they_belong_to_the_same_monitor() {
        let (tx_a, _rx_a) = watch::channel(false);
        let (tx_b, _rx_b) = watch::channel(false);
        let a = RelayStopHandle { shutdown_tx: tx_a };
        let b = RelayStopHandle { shutdown_tx: tx_b };

        assert!(a.same_monitor(&a.clone()), "a clone is the same monitor");
        assert!(!a.same_monitor(&b), "two monitors are not");
        assert!(!b.same_monitor(&a), "and it does not depend on the order");
    }

    /// Handles taken from a REAL monitor through `stop_handle()` belong to it, and
    /// stopping through one reaches the monitor itself.
    ///
    /// Every other test here builds its handles with a struct literal, so none of
    /// them ever called `stop_handle()` - and a reviewer made it hand out a fresh
    /// channel instead of a clone with all 1205 tests still green (2026-09-25).
    /// That one line would undo three builds at once: `clear_slot_if_mine` never
    /// finds its own handle, `is_stopped()` stays false, and worst, the duplicate
    /// guard's `previous.stop()` sends on a dead channel and stops nothing - the
    /// exact failure that guard exists for. My own mutations were all on the
    /// condition, none on what the condition compares.
    #[test]
    fn stop_handle_hands_out_the_monitors_own_channel() {
        let cfg = RelayConfig {
            enabled: false,
            url: String::new(),
            station: String::new(),
            token: String::new(),
            role: RelayRole::Client,
            instance: String::new(),
            name: String::new(),
            udp_port: None,
            quic: false,
        };
        let a = RelayMonitor::start_threaded(cfg.clone());
        let b = RelayMonitor::start_threaded(cfg);

        let a1 = a.stop_handle();
        let a2 = a.stop_handle();
        assert!(a1.same_monitor(&a2), "two handles of one monitor are the same monitor");
        assert!(!a1.same_monitor(&b.stop_handle()), "a handle of another monitor is not");

        assert!(!*a.shutdown_tx.borrow(), "a fresh monitor has not been told to stop");
        a1.stop();
        assert!(
            *a.shutdown_tx.borrow(),
            "stopping through a handle reaches the monitor's OWN channel"
        );
        assert!(a2.is_stopped(), "and every other handle of it sees that");
        assert!(!b.stop_handle().is_stopped(), "the other monitor is untouched");
    }

    /// Dropping a monitor stops it, and its handles see that.
    ///
    /// The bridge never calls `stop()` on its own monitor: it takes it out of
    /// its slot and lets it go out of scope, so `Drop` is the only stop that
    /// path has. A reviewer took the stop signal out of `Drop` with 47 tests
    /// still green (2026-09-25): the monitor thread would then run on after
    /// every disconnect, and `is_stopped()` would tell the next start that a
    /// live monitor is still there.
    #[test]
    fn dropping_a_monitor_stops_it() {
        let cfg = RelayConfig {
            enabled: false,
            url: String::new(),
            station: String::new(),
            token: String::new(),
            role: RelayRole::Client,
            instance: String::new(),
            name: String::new(),
            udp_port: None,
            quic: false,
        };
        let m = RelayMonitor::start_threaded(cfg);
        let h = m.stop_handle();
        assert!(!h.is_stopped(), "a running monitor is not stopped");
        drop(m);
        assert!(h.is_stopped(), "dropping the monitor stops it");
    }

    /// The shutdown path empties the slot only when the slot is still its own.
    ///
    /// All three states, because each one is a different mistake:
    /// - its own handle: must be emptied, or the next start finds a stale one
    ///   (the untrue warning of the 2026-09-24 field log);
    /// - someone else's: must be KEPT - that is a newer bridge after a network
    ///   switch, and dropping it leaves two live monitors unwatched;
    /// - nothing: must stay nothing.
    #[test]
    fn the_slot_is_emptied_only_when_it_holds_this_monitor() {
        let (tx_mine, _r1) = watch::channel(false);
        let (tx_next, _r2) = watch::channel(false);
        let mine = RelayStopHandle { shutdown_tx: tx_mine };
        let next = RelayStopHandle { shutdown_tx: tx_next };

        let mut slot = Some(mine.clone());
        assert!(RelayStopHandle::clear_slot_if_mine(&mut slot, &mine));
        assert!(slot.is_none(), "our own stale handle goes");

        let mut slot = Some(next.clone());
        assert!(!RelayStopHandle::clear_slot_if_mine(&mut slot, &mine));
        assert!(
            slot.as_ref().is_some_and(|h| h.same_monitor(&next)),
            "a newer bridge's handle must survive our shutdown"
        );

        let mut slot: Option<RelayStopHandle> = None;
        assert!(!RelayStopHandle::clear_slot_if_mine(&mut slot, &mine));
        assert!(slot.is_none());
    }

    /// A clone reports the same thing, because the bridge stores a clone while
    /// the monitor keeps its own.
    #[test]
    fn a_cloned_stop_handle_sees_the_same_stop() {
        let (tx, _rx) = watch::channel(false);
        let h = RelayStopHandle { shutdown_tx: tx };
        let copy = h.clone();

        assert!(!copy.is_stopped());
        h.stop();
        assert!(copy.is_stopped(), "stopping one is stopping the monitor");
    }

    #[test]
    fn a_relay_that_is_only_written_down_is_not_configured() {
        let (u, s, t) = ("wss://relay.example/ws", "pa0xyz", "secret");
        assert!(is_configured(true, u, s, t));
        // The tick off is the case that went wrong: the address is still there,
        // and the chat used to derive an endpoint from it anyway.
        assert!(!is_configured(false, u, s, t));
        assert!(!is_configured(true, "   ", s, t));
        assert!(!is_configured(true, u, "", t));
        assert!(!is_configured(true, u, s, " "));
    }

    #[test]
    fn the_chat_only_gets_an_address_it_may_use() {
        let (u, s, t) = ("wss://relay.example/ws", "pa0xyz", "secret");
        assert_eq!(chat_relay_url(true, u, s, t), u);
        assert_eq!(chat_relay_url(false, u, s, t), "");
        // And an empty address is what makes the chat say "no relay" rather than
        // "this relay has no chat" - the two are one step apart.
        assert!(chat_endpoint(chat_relay_url(false, u, s, t)).is_none());
        assert!(chat_endpoint(chat_relay_url(true, u, s, t)).is_some());
    }

    #[test]
    fn parse_udp_token_extracts_last_field() {
        let tok = "a".repeat(64);
        assert_eq!(
            parse_udp_token(&format!("OK relay-ready udp_token={tok}")).as_deref(),
            Some(tok.as_str())
        );
        assert!(parse_udp_token("OK relay-ready").is_none()); // no token
        assert!(parse_udp_token("OK relay-ready udp_token=short").is_none()); // wrong length
    }

    #[test]
    fn rcf_fallback_roundtrip_and_reject() {
        assert_eq!(parse_rcf_fallback(&encode_rcf_fallback(true)), Some(true));
        assert_eq!(parse_rcf_fallback(&encode_rcf_fallback(false)), Some(false));
        // An audio packet must never be mistaken for a control frame.
        assert_eq!(parse_rcf_fallback(&[0xEE, 1, 0x01, 0, 0, 0, 0, 5]), None);
        assert_eq!(parse_rcf_fallback(b"RCF"), None); // too short
    }

    #[test]
    fn audio_identity_reads_type_and_be_sequence() {
        // header(4) + sequence(4 BE) + ...: type=0x01, seq=0x00010203.
        let pkt = [0xEE, 1, 0x01, 0, 0x00, 0x01, 0x02, 0x03, 9, 9];
        assert_eq!(audio_identity(&pkt), Some((0x01, 0x0001_0203)));
        assert_eq!(audio_identity(&[0, 0, 0x04, 0, 0, 0, 0, 0]), None); // not audio
        assert_eq!(audio_identity(&[0, 0, 0x01, 0]), None); // too short for seq
    }

    #[test]
    fn dedup_drops_repeat_keeps_fresh_and_non_audio() {
        let mut d = DedupWindow::new();
        let frame = |ty: u8, seq: u32| {
            let mut v = vec![0xEE, 1, ty, 0];
            v.extend_from_slice(&seq.to_be_bytes());
            v
        };
        assert!(!d.is_duplicate(0, &frame(0x01, 1))); // fresh
        assert!(d.is_duplicate(0, &frame(0x01, 1))); // exact repeat -> dropped
        assert!(!d.is_duplicate(0, &frame(0x01, 2))); // next seq -> fresh
        assert!(!d.is_duplicate(0, &frame(0x1B, 1))); // same seq, other type -> fresh
        assert!(!d.is_duplicate(1, &frame(0x01, 1))); // same frame, other client -> fresh
        // Non-audio is never deduped (control/spectrum must always pass).
        assert!(!d.is_duplicate(0, &[0, 0, 0x04, 0]));
        assert!(!d.is_duplicate(0, &[0, 0, 0x04, 0]));
    }

    #[test]
    fn audio_ptt_flag_detected() {
        assert!(audio_ptt_active(&[0xEE, 1, 0x01, 0x01, 0, 0, 0, 0])); // PTT bit set
        assert!(!audio_ptt_active(&[0xEE, 1, 0x01, 0x00, 0, 0, 0, 0])); // PTT clear
        assert!(!audio_ptt_active(&[0, 0, 0x04, 0x01])); // not an audio packet
    }

    #[test]
    fn is_audio_packet_only_audio_types() {
        assert!(is_audio_packet(&[0, 0, 0x01, 0])); // Audio
        assert!(is_audio_packet(&[0, 0, 0x1B, 0])); // AudioMultiCh
        assert!(is_audio_packet(&[0, 0, 0x21, 0])); // AudioVrx
        assert!(!is_audio_packet(&[0, 0, 0x0A, 0])); // Spectrum -> wss
        assert!(!is_audio_packet(&[0, 0, 0x04, 0])); // Control -> wss
        assert!(!is_audio_packet(&[0, 0, 0x01])); // too short -> not audio
    }

    #[test]
    fn vrx_channels_do_not_dedup_each_other() {
        // Both virtual receivers use packet type 0x21 with their own sequence
        // counters, so keyed on the type alone VRX2's frame N looked like VRX1's
        // frame N and was dropped - one channel silently eating the other.
        let mut w = DedupWindow::new();
        let mut frame = |vrx_id: u8, seq: u32| {
            let mut p = vec![0u8, 0, 0x21, 0];
            p.extend_from_slice(&seq.to_be_bytes());
            p.extend_from_slice(&[0, 0, 0, 0]); // timestamp
            p.push(vrx_id);
            p.extend_from_slice(&[0, 0]); // opus length
            p
        };
        assert!(!w.is_duplicate(1, &frame(0, 92)), "VRX1 frame is fresh");
        assert!(!w.is_duplicate(1, &frame(1, 92)), "same seq on VRX2 is a different stream");
        assert!(w.is_duplicate(1, &frame(0, 92)), "a real repeat is still a duplicate");
    }

    #[test]
    fn a_restarted_stream_is_not_its_own_duplicate() {
        // Switching a channel off and on restarts its sequence at 0 while the
        // window still holds the previous run. Measured on air: the engine only
        // saw seq=126 after 2568 ms, because 0..125 were dropped as repeats -
        // exactly as long as the previous session had been.
        let mut w = DedupWindow::new();
        let mut frame = |vrx_id: u8, seq: u32| {
            let mut p = vec![0u8, 0, 0x21, 0];
            p.extend_from_slice(&seq.to_be_bytes());
            p.extend_from_slice(&[0, 0, 0, 0]);
            p.push(vrx_id);
            p.extend_from_slice(&[0, 0]);
            p
        };
        for seq in 0..126u32 {
            assert!(!w.is_duplicate(1, &frame(1, seq)));
        }
        // Same channel starts over.
        assert!(!w.is_duplicate(1, &frame(1, 0)), "a restarted stream must pass");
        assert!(!w.is_duplicate(1, &frame(1, 1)));
        // And within the new run a genuine repeat is still caught.
        assert!(w.is_duplicate(1, &frame(1, 1)), "repeats inside the new run still drop");
    }

    #[test]
    fn a_keepalive_round_carries_the_token_it_is_given() {
        // The rotation path hands the NEW token to the same round the timer uses;
        // what goes out must carry exactly that token, one datagram per target,
        // with a fresh sequence number each.
        let new_tok = "cd".repeat(32);
        let mut seq = 10u32;

        let (client, kind) =
            keepalive_datagrams(RelayRole::Client, &HashSet::new(), &new_tok, &mut seq);
        assert_eq!(kind, KeepaliveKind::Client);
        assert_eq!(client.len(), 1);
        assert_eq!(client[0], encode_tlu1(0, 11, &new_tok, &[]));

        let bound: HashSet<u8> = [3u8, 1].into_iter().collect();
        let (station, kind) = keepalive_datagrams(RelayRole::Station, &bound, &new_tok, &mut seq);
        assert_eq!(kind, KeepaliveKind::Beacons);
        assert_eq!(station, vec![encode_tlu1(1, 12, &new_tok, &[]), encode_tlu1(3, 13, &new_tok, &[])]);

        let (idle, kind) =
            keepalive_datagrams(RelayRole::Station, &HashSet::new(), &new_tok, &mut seq);
        assert_eq!(kind, KeepaliveKind::MappingOnly);
        assert_eq!(idle, vec![encode_tlu1(UDP_MAPPING_KEEPALIVE_ID, 14, &new_tok, &[])]);
        assert_eq!(seq, 14);
    }

    #[test]
    fn encode_tlu1_layout_and_hex32() {
        let tok = "bb".repeat(32); // 64 hex chars -> 32 bytes of 0xbb
        let frame = encode_tlu1(3, 0x0102_0304, &tok, b"xy");
        assert_eq!(&frame[0..4], b"TLU1");
        assert_eq!(frame[4], TLU1_VERSION);
        assert_eq!(frame[6], 3); // client_id
        assert_eq!(&frame[7..11], &0x0102_0304u32.to_le_bytes());
        assert_eq!(&frame[11..43], &[0xbbu8; 32]); // token raw bytes
        assert_eq!(&frame[43..], b"xy"); // payload after the 43-byte header
        assert_eq!(frame.len(), TLU1_HEADER_LEN + 2);
    }

    #[test]
    fn url_host_parses_scheme_port_path() {
        assert_eq!(
            url_host("wss://relay.example.com").as_deref(),
            Some("relay.example.com")
        );
        assert_eq!(url_host("wss://host:443/path").as_deref(), Some("host"));
        assert_eq!(url_host("ws://1.2.3.4:18080").as_deref(), Some("1.2.3.4"));
    }

    /// The one that protects every station built before the beacon.
    ///
    /// A capability announcement must NOT read as a fallback request. The obvious
    /// design was a spare bit in the fallback frame's value byte, and it would have
    /// been quietly catastrophic: that parser reads the byte as `!= 0`, so a
    /// capability bit set in an OFF frame - the very frame a client sends first -
    /// says "put me on wss" to an older station, and it never says otherwise again.
    #[test]
    fn a_capability_frame_is_not_a_fallback_request() {
        let caps = encode_rcf_caps(CAPS_READS_BEACON);
        assert_eq!(parse_rcf_caps(&caps), Some(CAPS_READS_BEACON));
        assert_eq!(
            parse_rcf_fallback(&caps),
            None,
            "an older station must ignore this frame, not read it as a request"
        );
        // And the other way round, so the two opcodes can never be confused.
        let off = encode_rcf_fallback(false);
        assert_eq!(parse_rcf_fallback(&off), Some(false));
        assert_eq!(parse_rcf_caps(&off), None);
    }

    /// Both control frames have to be recognised as control frames.
    ///
    /// They are what puts a client on the beacon list when it has sent nothing else,
    /// which after a reconnect is the normal case - and the client in a fallback,
    /// the one that most needs a beacon, may send nothing else at all.
    #[test]
    fn both_control_frames_count_as_control_frames() {
        assert!(is_relay_control_frame(&encode_rcf_caps(CAPS_READS_BEACON)));
        assert!(is_relay_control_frame(&encode_rcf_fallback(true)));
        assert!(is_relay_control_frame(&encode_rcf_fallback(false)));
        // Audio must not be mistaken for one, or it would take the early return.
        assert!(!is_relay_control_frame(&[0u8; 8]));
        assert!(!is_relay_control_frame(&[]));
    }

    /// A capability frame with no bits set still parses, and grants nothing.
    #[test]
    fn an_empty_capability_frame_grants_nothing() {
        let none = encode_rcf_caps(0);
        assert_eq!(parse_rcf_caps(&none), Some(0));
        assert_eq!(parse_rcf_caps(&none).unwrap() & CAPS_READS_BEACON, 0);
    }

    /// Recovery has to be reachable on a path carrying nothing but beacons.
    ///
    /// This is the shape that a review found in the first repair: move the healthy
    /// run onto the downlink but leave its restart threshold at the audio value, and
    /// every beacon starts a fresh run - so the run never grows, and a path that is
    /// demonstrably alive never recovers. The numbers here are the real ones.
    #[test]
    fn a_beacon_only_path_can_recover() {
        // The real one, not a copy of it. Two tests used to declare their own and
        // neither would have noticed the send site changing underneath them.
        let beacon_interval = UDP_BEACON_INTERVAL;
        assert!(
            beacon_interval < FALLBACK_DOWNLINK_STALL,
            "a normal gap between beacons must not restart the healthy run"
        );
        assert!(
            FALLBACK_DOWNLINK_STALL < FALLBACK_RECOVER,
            "a run must be able to outgrow the restart threshold, or recovery is unreachable"
        );
        // And there has to be room for a beacon to go missing. At one a second against
        // a 2500ms threshold that is exactly one lost beacon of slack, which is thin
        // but deliberate; what must not happen is it silently becoming none.
        assert!(
            beacon_interval * 2 < PATH_QUIET_REPORT,
            "a single lost beacon must not read as a path outage"
        );
        assert!(
            beacon_interval * 2 < FALLBACK_DOWNLINK_STALL,
            "a single lost beacon must not trigger a fallback"
        );

        // A run that began four seconds ago, last beacon half a second back.
        let began = Instant::now() - Duration::from_secs(4);
        let last = Instant::now() - Duration::from_millis(500);
        assert!(udp_recovered(
            Some(began),
            Some(last),
            FALLBACK_RECOVER,
            FALLBACK_DOWNLINK_STALL
        ));

        // Beacons stopped three seconds ago: not recovered, whatever the run says.
        let stale = Instant::now() - Duration::from_secs(3);
        assert!(!udp_recovered(
            Some(began),
            Some(stale),
            FALLBACK_RECOVER,
            FALLBACK_DOWNLINK_STALL
        ));
    }

    /// The gap belongs to the caller, not to the function that measures it.
    ///
    /// It used to filter on `PATH_QUIET_REPORT` before returning, which was fine while
    /// one caller asked one question. The fallback now reads the same gap against
    /// `FALLBACK_DOWNLINK_STALL`, and those two are the same 2500 ms by coincidence -
    /// so a filter in here would have tied a reporting threshold to a decision
    /// threshold where nobody would ever look for the knot.
    #[test]
    fn a_measured_gap_comes_back_unfiltered() {
        let mut last: Option<Instant> = None;
        let t0 = Instant::now();

        // Nothing before it: no gap, and the clock starts.
        assert_eq!(note_path_packet(&mut last, t0), None);
        assert_eq!(last, Some(t0));

        // A gap far below any reporting threshold still comes back, so a caller with a
        // smaller threshold than PATH_QUIET_REPORT can see it.
        let small = Duration::from_millis(40);
        let gap = note_path_packet(&mut last, t0 + small).expect("a second packet has a gap");
        assert_eq!(gap, small);
        assert!(
            gap < PATH_QUIET_REPORT,
            "the point of this case is that it is below the reporting threshold"
        );

        // And the clock moved with it, so gaps do not accumulate.
        assert_eq!(last, Some(t0 + small));
    }

    /// A path carrying nothing but keepalives is not a quiet path.
    ///
    /// This is the sentence the code could not express until the pre-step became a
    /// function. Both defects at this spot were the same mistake in different clothes:
    /// the fallback would not fire on a beacon-only path, and then the station called
    /// every listening turn an outage. Both were "we measured the audio and called it
    /// the path".
    #[test]
    fn a_path_carrying_only_keepalives_is_never_quiet() {
        let mut last: Option<Instant> = None;
        let mut healthy: Option<Instant> = None;
        let t0 = Instant::now();

        // First datagram of the session: no gap to report, and the run starts.
        assert_eq!(
            note_downlink(&mut last, &mut healthy, t0, FALLBACK_DOWNLINK_STALL),
            None
        );
        assert_eq!(healthy, Some(t0));

        // Sixty beacons at their normal spacing, no audio at all - a station with every
        // channel switched off, or an operator who is listening.
        let mut t = t0;
        for _ in 0..60 {
            t += UDP_BEACON_INTERVAL;
            let gap = note_downlink(&mut last, &mut healthy, t, FALLBACK_DOWNLINK_STALL)
                .expect("every beacon after the first closes a gap");
            assert!(
                gap < PATH_QUIET_REPORT,
                "a beacon at its normal spacing must never be reported as an outage"
            );
        }

        // And the run has been running the whole minute, so recovery is reachable.
        assert_eq!(
            healthy,
            Some(t0),
            "a beacon at its normal spacing must not restart the healthy run"
        );
        // Which is what makes recovery reachable: the run has grown past the window.
        // Stated as a span rather than through `udp_recovered`, because that one reads
        // the wall clock via `elapsed()` and this timeline is built forwards from now -
        // `a_beacon_only_path_can_recover` covers it from the other side, with instants
        // in the past.
        assert!(
            t.saturating_duration_since(healthy.expect("the run is running")) >= FALLBACK_RECOVER,
            "a minute of beacons must leave a run long enough to recover on"
        );

        // Now the beacons stop for longer than the threshold: the run restarts, and the
        // gap is worth a line.
        t += FALLBACK_DOWNLINK_STALL + Duration::from_millis(1);
        let gap = note_downlink(&mut last, &mut healthy, t, FALLBACK_DOWNLINK_STALL)
            .expect("a gap this long is measured");
        assert!(gap >= PATH_QUIET_REPORT);
        assert_eq!(healthy, Some(t), "a real gap must restart the healthy run");
    }

    /// The whole episode, driven as a sequence rather than as one comparison.
    ///
    /// Everything that ever went wrong here was in the wiring, and no test could
    /// reach it: `run_connection` is called once, from production, and no test calls
    /// it - so a mutation anywhere in that block survived the entire suite by
    /// construction. A review made that argument and it is stronger than the three
    /// mutation runs offered against it.
    #[test]
    fn an_absence_opens_once_and_closes_once() {
        let quiet = FALLBACK_DOWNLINK_STALL + Duration::from_millis(1);
        let flowing = Duration::from_millis(20);

        // A long-running session throughout; the session age is only consulted when
        // there has been no audio at all, which is the test after this one.
        let old = Duration::from_secs(600);

        // Healthy: nothing to say, latched or not.
        assert_eq!(
            absence_edge(false, false, Some(flowing), old),
            AbsenceEvent::Nothing
        );

        // Audio has been gone long enough, path alive: open, once, carrying the gap.
        assert_eq!(
            absence_edge(false, false, Some(quiet), old),
            AbsenceEvent::Began(quiet)
        );
        // Still gone, already spoken: silence. This is the level-versus-edge fault
        // that put fourteen lines in one log.
        assert_eq!(
            absence_edge(true, false, Some(quiet), old),
            AbsenceEvent::Nothing
        );

        // Audio returns: close, saying so.
        assert_eq!(
            absence_edge(true, false, Some(flowing), old),
            AbsenceEvent::EndedAudioBack
        );
    }

    /// No audio at all in this session - the case that came out of a measurement
    /// instead of out of a test, because it lived above the cut.
    ///
    /// This is a client that has started, has its relay transport up and has not been
    /// connected to a server yet: the path is demonstrably alive and there has never
    /// been an audio frame. The absence is then as old as the session, and the report
    /// is worth exactly one line - not one every three seconds, which is what a log of
    /// 2026-09-11 contained fourteen times.
    #[test]
    fn a_session_that_never_had_audio_measures_from_its_own_start() {
        let young = FALLBACK_DOWNLINK_STALL - Duration::from_millis(1);
        let old = FALLBACK_DOWNLINK_STALL + Duration::from_secs(40);

        // Just started: too early to say anything.
        assert_eq!(absence_edge(false, false, None, young), AbsenceEvent::Nothing);

        // Long enough: one line, and the age it reports is the session's own.
        assert_eq!(
            absence_edge(false, false, None, old),
            AbsenceEvent::Began(old),
            "with no audio ever, the absence is as old as the session"
        );

        // And then it stays quiet, however long that lasts - the operator decides
        // when to connect.
        assert_eq!(
            absence_edge(true, false, None, old + Duration::from_secs(3600)),
            AbsenceEvent::Nothing
        );
    }

    /// The path breaking is NOT audio coming back, and the log must not say it is.
    ///
    /// The first version had one ending for both. A stalled path makes the condition
    /// false exactly as audio returning does, so "UDP audio is back" was written on
    /// the very pass where the fallback fires - the two lines land next to each other
    /// in the log, the first one the opposite of the truth. Found in review.
    #[test]
    fn a_broken_path_does_not_report_audio_returning() {
        let quiet = FALLBACK_DOWNLINK_STALL + Duration::from_millis(1);

        let old = Duration::from_secs(600);

        assert_eq!(
            absence_edge(true, true, Some(quiet), old),
            AbsenceEvent::EndedPathBroken,
            "the rescue is taking over; claiming audio returned would be a lie"
        );
        assert_ne!(
            absence_edge(true, true, Some(quiet), old),
            AbsenceEvent::EndedAudioBack
        );

        // And with audio flowing again while the path is also stalled - the order the
        // two facts arrive in must not decide what gets written.
        assert_eq!(
            absence_edge(true, true, Some(Duration::from_millis(20)), old),
            AbsenceEvent::EndedPathBroken
        );
    }

    /// Two episodes in one session, no restart between them.
    ///
    /// Working rule 9. The second episode has to behave exactly like the first: open
    /// once, stay quiet, close once.
    #[test]
    fn a_second_absence_behaves_like_the_first() {
        let quiet = FALLBACK_DOWNLINK_STALL + Duration::from_millis(1);
        let flowing = Duration::from_millis(20);

        let mut latched = false;
        let mut opened = 0;
        let mut closed = 0;

        // Two rounds of: gone, gone, gone, back, back.
        for _ in 0..2 {
            for without in [quiet, quiet, quiet, flowing, flowing] {
                match absence_edge(latched, false, Some(without), Duration::from_secs(600)) {
                    AbsenceEvent::Began(_) => {
                        latched = true;
                        opened += 1;
                    }
                    AbsenceEvent::EndedAudioBack | AbsenceEvent::EndedPathBroken => {
                        latched = false;
                        closed += 1;
                    }
                    AbsenceEvent::Nothing => {}
                }
            }
        }

        assert_eq!(opened, 2, "one opening line per episode, not per pass");
        assert_eq!(closed, 2, "and one closing line per episode");
        assert!(!latched, "and the second episode leaves nothing standing");
    }

    /// A healthy stream must never make the diagnostic speak.
    ///
    /// The line exists to explain why the rescue did not fire, and it compared against
    /// a 500ms threshold while audio frames arrive every 20ms - so one late frame
    /// satisfied it. On a link carrying fifty frames a second it announced "UDP audio
    /// is missing", three times in one afternoon. The first case below is that link.
    #[test]
    fn a_healthy_audio_stream_never_trips_the_diagnostic() {
        let frame_interval = Duration::from_millis(20);

        // A stream running normally, even with a frame or two late.
        assert!(!audio_absent_while_path_alive(false, frame_interval * 30));
        assert!(
            frame_interval * 30 > Duration::from_millis(500),
            "this case has to be past the OLD threshold, or it proves nothing"
        );

        // Audio gone for longer than the rescue's own window, path reporting alive:
        // that is the situation worth a line.
        assert!(audio_absent_while_path_alive(false, FALLBACK_DOWNLINK_STALL));

        // But not while the path is stalled - then the rescue is acting and there is
        // nothing to explain.
        assert!(!audio_absent_while_path_alive(true, Duration::from_secs(60)));
    }

    /// Silence from a peer that is not there yet is not evidence.
    ///
    /// The window used to count from the moment the relay tunnel stood, and the
    /// peer handshake lands after that - six seconds after, in the field log of
    /// 2026-09-23. So every fresh connect declared "UDP blocked" two seconds in,
    /// before anything could have sent, and the owner saw the fallback indicator
    /// on every start.
    #[test]
    fn without_a_peer_there_is_nothing_to_accuse() {
        // No peer yet, and the window has long passed: still not a stall.
        assert!(!downlink_stalled(
            None,
            None,
            FALLBACK_CONNECT_TIMEOUT,
            FALLBACK_DOWNLINK_STALL
        ));
        // The peer arrives: now the clock starts, and inside the window we wait.
        assert!(!downlink_stalled(
            Some(FALLBACK_CONNECT_TIMEOUT - Duration::from_millis(1)),
            None,
            FALLBACK_CONNECT_TIMEOUT,
            FALLBACK_DOWNLINK_STALL
        ));
        // Past it with a peer present and still nothing down: now it counts.
        assert!(downlink_stalled(
            Some(FALLBACK_CONNECT_TIMEOUT),
            None,
            FALLBACK_CONNECT_TIMEOUT,
            FALLBACK_DOWNLINK_STALL
        ));
        // And a peerless client whose path HAS delivered still judges on the gap -
        // the second arm must not be affected by the first.
        assert!(downlink_stalled(
            None,
            Some(Instant::now() - FALLBACK_DOWNLINK_STALL),
            FALLBACK_CONNECT_TIMEOUT,
            FALLBACK_DOWNLINK_STALL
        ));
    }

    /// A station with no clients must still put one datagram on the wire.
    ///
    /// This is the fault of 2026-09-23, and it is invisible from either endpoint:
    /// the station believed it was beaconing, the relay believed it was forwarding,
    /// and the client received nothing for four minutes. The cause was silence
    /// while idle - the NAT mapping expired, the station came back on a new source
    /// port, and the relay dropped every datagram against a stale source binding
    /// until the token rotated.
    #[test]
    fn an_idle_station_keeps_its_mapping_open() {
        // No clients: exactly one datagram, to an id no client can hold.
        let none: HashSet<u8> = HashSet::new();
        assert_eq!(station_udp_targets(&none), vec![UDP_MAPPING_KEEPALIVE_ID]);
        assert_eq!(
            station_udp_targets(&none).len(),
            1,
            "silence is what broke it; one datagram is the whole point"
        );

        // With clients: one beacon each and NOT the keepalive - the beacon already
        // keeps the mapping open, and an extra datagram would be waste.
        let some: HashSet<u8> = [3u8, 1, 2].into_iter().collect();
        assert_eq!(station_udp_targets(&some), vec![1, 2, 3]);
        assert!(!station_udp_targets(&some).contains(&UDP_MAPPING_KEEPALIVE_ID));

        // The id itself must stay outside what the relay hands out (1..=254).
        assert_eq!(UDP_MAPPING_KEEPALIVE_ID, 0);
    }

    /// The entry, which for six rounds had no test at all.
    ///
    /// The one that matters is the third: a beacon at its normal spacing must NOT read
    /// as a stall. Put the old guard back in front of this decision and that case is
    /// what breaks, which is the whole reason the function was pulled out of the loop.
    #[test]
    fn a_beacon_at_normal_spacing_is_not_a_stall() {
        let beacon_interval = UDP_BEACON_INTERVAL;

        // 1. nothing has ever arrived, still inside the connect window: wait.
        assert!(!downlink_stalled(
            Some(Duration::from_millis(500)),
            None,
            FALLBACK_CONNECT_TIMEOUT,
            FALLBACK_DOWNLINK_STALL
        ));

        // 2. nothing has ever arrived and the connect window has passed: UDP is blocked.
        assert!(downlink_stalled(
            Some(FALLBACK_CONNECT_TIMEOUT + Duration::from_millis(1)),
            None,
            FALLBACK_CONNECT_TIMEOUT,
            FALLBACK_DOWNLINK_STALL
        ));

        // 3. the load-bearing one: a beacon one second ago, no audio at all, every
        //    channel switched off. This must not be a stall, and nothing else in the
        //    workspace says so.
        assert!(!downlink_stalled(
            Some(Duration::from_secs(60)),
            Some(Instant::now() - beacon_interval),
            FALLBACK_CONNECT_TIMEOUT,
            FALLBACK_DOWNLINK_STALL
        ));

        // 4. the beacons themselves stopped: the path is gone.
        assert!(downlink_stalled(
            Some(Duration::from_secs(60)),
            Some(Instant::now() - FALLBACK_DOWNLINK_STALL - Duration::from_millis(1)),
            FALLBACK_CONNECT_TIMEOUT,
            FALLBACK_DOWNLINK_STALL
        ));
    }

    /// The saving itself, including the case that made it never happen.
    ///
    /// A station stops sending UDP audio only to a client it knows reads a beacon. That
    /// fact reached the station once at connect and was dropped with the fallback entry
    /// a lease later, so every realistic fallback found the default - and the duplicate
    /// stream this patch exists to remove was never removed. The third case below is
    /// that situation; it passes here because the repair is that the client keeps
    /// saying it, not that the station stops forgetting.
    #[test]
    fn a_station_only_drops_udp_for_a_client_that_reads_beacons() {
        let fresh = Duration::from_secs(1);
        let expired = FALLBACK_LEASE + Duration::from_secs(1);

        // No fallback: UDP only, as before this patch existed.
        assert_eq!(
            station_audio_paths(true, false, fresh, FALLBACK_LEASE, true),
            (true, false)
        );

        // Fallback, client reads beacons: wss only. This is the saving.
        assert_eq!(
            station_audio_paths(true, true, fresh, FALLBACK_LEASE, true),
            (false, true)
        );

        // Fallback, but the station does not know this client reads beacons: BOTH, on
        // purpose. It has no other way of noticing that its path came back.
        assert_eq!(
            station_audio_paths(true, true, fresh, FALLBACK_LEASE, false),
            (true, true)
        );

        // The lease lapsed: the station no longer believes the client is in a fallback,
        // so UDP resumes and wss stops - even though `reads_beacon` still says true.
        assert_eq!(
            station_audio_paths(true, true, expired, FALLBACK_LEASE, true),
            (true, false)
        );

        // No UDP transport at all: wss carries it whatever the flags say.
        assert_eq!(
            station_audio_paths(false, true, fresh, FALLBACK_LEASE, true),
            (false, true)
        );
    }

    #[test]
    fn reject_reason_maps_known_codes_and_passes_through() {
        assert!(reject_reason("ERR too many devices").contains("too many devices"));
        assert!(reject_reason("ERR relay full").contains("simultaneous connections"));
        assert!(reject_reason("ERR station data limit reached").contains("monthly data limit"));
        assert!(reject_reason("ERR device blocked").contains("blocked"));
        assert!(reject_reason("ERR unauthorized").contains("invalid key"));
        // Unknown code falls back to the raw text.
        assert!(reject_reason("ERR something new").contains("something new"));
    }

    #[test]
    fn tlt1_roundtrip_preserves_payload() {
        let payload = b"\xaa\x00\x01 the quick brown TL frame \xff\x00";
        let frame = encode_tlt1(0, payload);
        let (client_id, out) = parse_tlt1(&frame).expect("must parse");
        assert_eq!(client_id, 0);
        assert_eq!(out, payload);
    }

    #[test]
    fn tlt1_is_distinct_from_tlb1_probe() {
        // A TLB1 probe must NOT parse as TLT1, and vice versa.
        let probe = binary_probe_frame(BINARY_KIND_PING, RelayRole::Station, 7, BINARY_PROBE_LEN);
        assert!(parse_tlt1(&probe).is_none(), "a TLB1 probe parsed as TLT1");

        let tunnel = encode_tlt1(0, b"hello");
        assert!(
            parse_binary_probe(&tunnel).is_none(),
            "a TLT1 tunnel parsed as a TLB1 probe"
        );
    }

    #[test]
    fn tlt1_rejects_truncated_and_wrong_magic() {
        // Unknown magic -> None (non-fatal drop).
        assert!(parse_tlt1(b"XXXX\x01\x00\x00\x00").is_none());
        // Too short for the header -> None.
        assert!(parse_tlt1(b"TLT1").is_none());
        // Header claims more payload than present -> None.
        let mut bad = encode_tlt1(0, b"abcd");
        bad.truncate(TLT1_HEADER_LEN + 2); // header says 4, but 2 present
        assert!(parse_tlt1(&bad).is_none());
    }

    #[test]
    fn sentinel_client_id_roundtrip() {
        for id in [0u8, 1, 2, 15, 100, 253] {
            let s = sentinel_for_client_id(id);
            assert_eq!(client_id_from_sentinel(s), id, "roundtrip failed for id {id}");
        }
        // client 0 -> 203.0.113.1 (the classic tunnel's convention; see "Relay sentinel convention" above).
        assert_eq!(sentinel_for_client_id(0).ip().to_string(), "203.0.113.1");
        assert_eq!(sentinel_for_client_id(1).ip().to_string(), "203.0.113.2");
    }

    #[test]
    fn tlt1_empty_payload_roundtrips() {
        let frame = encode_tlt1(3, b"");
        let (id, out) = parse_tlt1(&frame).expect("an empty payload must parse");
        assert_eq!(id, 3);
        assert!(out.is_empty());
    }
}

#[cfg(test)]
mod chat_ticket_tests {
    use super::parse_chat_ticket;

    /// The ready-reply carries both fields. Reading by name means neither cares
    /// about the order, or about a field added later.
    #[test]
    fn the_ready_reply_yields_the_ticket() {
        let line = format!(
            "OK relay-ready udp_token={} chat_ticket=616263.646566",
            "a".repeat(64)
        );
        assert_eq!(parse_chat_ticket(&line).as_deref(), Some("616263.646566"));
    }

    #[test]
    fn the_rotation_push_yields_it_too() {
        assert_eq!(
            parse_chat_ticket("chat-ticket-rotate chat_ticket=616263.646566").as_deref(),
            Some("616263.646566")
        );
    }

    /// A relay with no chat behind it offers none, and that is not an error.
    #[test]
    fn a_line_without_one_is_simply_none() {
        assert!(parse_chat_ticket("OK relay-ready udp_token=abc").is_none());
        assert!(parse_chat_ticket("").is_none());
    }

    /// Shape only - the signature is the chat's business. But rubbish should not
    /// travel: a malformed ticket would be refused later with a confusing reason.
    #[test]
    fn something_that_is_not_a_ticket_is_refused_here() {
        for junk in [
            "chat_ticket=notatoken",
            "chat_ticket=.",
            "chat_ticket=abc.",
            "chat_ticket=.abc",
            "chat_ticket=zz.zz",
            "chat_ticket=abc.def.ghi",
            "chat_ticket=abcd", // no dot at all
        ] {
            assert!(parse_chat_ticket(junk).is_none(), "{junk}");
        }
    }

    /// Odd-length payload hex cannot be bytes, so it cannot be a payload.
    #[test]
    fn an_odd_payload_length_is_refused() {
        assert!(parse_chat_ticket("chat_ticket=abc.abcd").is_none());
    }
}

/// QUIC via the relay (phase 3b): what the monitor does in that mode, against a
/// relay played over a real ws connection on loopback.
#[cfg(test)]
mod quic_mode {
    use super::*;
    use tokio::net::TcpListener;

    fn quic_config(url: String, udp_port: u16) -> RelayConfig {
        RelayConfig {
            enabled: true,
            url,
            station: "ST".into(),
            token: "secret".into(),
            role: RelayRole::Client,
            instance: "inst".into(),
            name: "Phone One".into(),
            udp_port: Some(udp_port),
            quic: true,
        }
    }

    /// The relay's UDP port, to see whether anything arrives there.
    async fn relay_udp() -> (UdpSocket, u16) {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = sock.local_addr().unwrap().port();
        (sock, port)
    }

    /// One session of a relay: answers the hello with a token, pushes a rotation,
    /// then sends `peer` as the peer's hello - and reports what it got. With
    /// `close`, it then ends the session from its side.
    async fn play_relay(peer: &'static str) -> (String, mpsc::UnboundedReceiver<Message>) {
        play_relay_then(peer, false).await
    }

    async fn play_relay_then(peer: &'static str, close: bool) -> (String, mpsc::UnboundedReceiver<Message>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/ws", listener.local_addr().unwrap());
        let (seen_tx, seen_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let (mut write, mut read) = ws.split();
            let hello = read.next().await.unwrap().unwrap();
            let _ = seen_tx.send(hello);
            let first = "11".repeat(32);
            write.send(Message::Text(format!("OK relay-ready udp_token={first}").into())).await.unwrap();
            tokio::time::sleep(Duration::from_millis(150)).await;
            let second = "22".repeat(32);
            write.send(Message::Text(format!("udp-token-rotate udp_token={second}").into())).await.unwrap();
            tokio::time::sleep(Duration::from_millis(150)).await;
            write.send(Message::Text(peer.into())).await.unwrap();
            if close {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let _ = write.send(Message::Close(None)).await;
                return;
            }
            while let Some(Ok(m)) = read.next().await {
                let _ = seen_tx.send(m);
            }
        });
        (url, seen_rx)
    }

    /// The TCP line through the monitor (build 144): in QUIC mode a TLT1 frame
    /// from the relay reaches the route layer with its client id, a binary frame
    /// that is no TLT1 does not, and a frame the route layer hands over goes out
    /// on the wss connection as it is.
    #[tokio::test]
    async fn quic_mode_carries_the_tcp_line_both_ways() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/ws", listener.local_addr().unwrap());
        let (seen_tx, mut seen) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let (mut write, mut read) = ws.split();
            let _hello = read.next().await;
            write.send(Message::Text(format!("OK relay-ready udp_token={}", "11".repeat(32)).into())).await.unwrap();
            write.send(Message::Binary(b"XXXX not a tunnel frame".to_vec().into())).await.unwrap();
            write.send(Message::Binary(tlt1_frame(4, b"hi").into())).await.unwrap();
            while let Some(Ok(m)) = read.next().await {
                let _ = seen_tx.send(m);
            }
        });
        let (_udp, udp_port) = relay_udp().await;
        let status = RelayStatusHandle { state: Arc::new(Mutex::new(RelayStatus::disabled())) };
        let config = quic_config(url, udp_port);
        let (_ctx, mut config_rx) = watch::channel(config.clone());
        let (stx, mut shutdown_rx) = watch::channel(false);
        let (grant_tx, _grant_rx) = watch::channel(None);
        let (mut line, mut line_monitor) = tcp_line();
        let task = {
            let status = status.clone();
            tokio::spawn(async move {
                let mut outcome = SessionOutcome::default();
                run_connection(&config, &status, 1, &mut config_rx, &mut shutdown_rx, None, Some(&grant_tx), Some(&mut line_monitor), &mut outcome)
                    .await
            })
        };
        let got = tokio::time::timeout(Duration::from_secs(5), line.from_relay.recv()).await.unwrap().unwrap();
        assert_eq!(got, (4, b"hi".to_vec()), "the TLT1 frame, with its client id");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(line.from_relay.try_recv().is_err(), "nothing that is no TLT1 reaches the line");
        // The status is still the one the junk frame before it set (23 bytes):
        // the TLT1 frame after it ended at the line.
        let said = status.state.lock().unwrap().message.clone();
        assert_eq!(said, "Relay peer traffic received: 23 bytes", "a TLT1 frame ends at the line, not in the status");
        // Twice (rule 9): the second frame goes too once the first has left.
        for word in [&b"up"[..], &b"up again"[..]] {
            line.to_relay.send(tlt1_frame(0, word)).await.unwrap();
            let out = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some(Message::Binary(b)) = seen.recv().await {
                        return b.to_vec();
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(out, tlt1_frame(0, word), "out on the wss connection as handed over");
        }
        let _ = stx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    /// A QUIC monitor without a TCP line (a status monitor) drops a TLT1 frame
    /// quietly: the relay can hand it a client id a TCP client had a moment
    /// before, and each frame used to set the status line (review 144).
    #[tokio::test]
    async fn a_quic_monitor_without_a_line_drops_tlt1_quietly() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/ws", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let (mut write, mut read) = ws.split();
            let _hello = read.next().await;
            write.send(Message::Text(format!("OK relay-ready udp_token={}", "11".repeat(32)).into())).await.unwrap();
            for _ in 0..3 {
                write.send(Message::Binary(tlt1_frame(4, b"not for you").into())).await.unwrap();
            }
            while let Some(Ok(_)) = read.next().await {}
        });
        let (_udp, udp_port) = relay_udp().await;
        let status = RelayStatusHandle { state: Arc::new(Mutex::new(RelayStatus::disabled())) };
        let config = quic_config(url, udp_port);
        let (_ctx, mut config_rx) = watch::channel(config.clone());
        let (stx, mut shutdown_rx) = watch::channel(false);
        let (grant_tx, mut grant_rx) = watch::channel(None);
        let task = {
            let status = status.clone();
            tokio::spawn(async move {
                let mut outcome = SessionOutcome::default();
                run_connection(&config, &status, 1, &mut config_rx, &mut shutdown_rx, None, Some(&grant_tx), None, &mut outcome).await
            })
        };
        let _ = tokio::time::timeout(Duration::from_secs(5), grant_rx.wait_for(|g| g.is_some())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let said = status.state.lock().unwrap().message.clone();
        assert!(!said.contains("peer traffic received"), "{said}");
        let _ = stx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    /// A burst on a free line loses nothing (build 147): a relay that reads,
    /// twenty bursts of ten frames, and every frame arrives. Before, the next
    /// frame of a burst was dropped whenever the loop had not yet chosen the
    /// push of the one before it - 160 of 200 here, on a free line (field test
    /// of build 146; the flood test below could not see it, it wants drops).
    #[tokio::test]
    async fn a_burst_on_a_free_line_loses_nothing() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/ws", listener.local_addr().unwrap());
        let (seen_tx, mut seen) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let (mut write, mut read) = ws.split();
            let _hello = read.next().await;
            write.send(Message::Text(format!("OK relay-ready udp_token={}", "11".repeat(32)).into())).await.unwrap();
            while let Some(Ok(m)) = read.next().await {
                if let Message::Binary(_) = m {
                    let _ = seen_tx.send(());
                }
            }
        });
        let (_udp, udp_port) = relay_udp().await;
        let status = RelayStatusHandle { state: Arc::new(Mutex::new(RelayStatus::disabled())) };
        let config = quic_config(url, udp_port);
        let (_ctx, mut config_rx) = watch::channel(config.clone());
        let (stx, mut shutdown_rx) = watch::channel(false);
        let (grant_tx, mut grant_rx) = watch::channel(None);
        let (line, mut line_monitor) = tcp_line();
        let task = {
            let status = status.clone();
            tokio::spawn(async move {
                let mut outcome = SessionOutcome::default();
                run_connection(&config, &status, 1, &mut config_rx, &mut shutdown_rx, None, Some(&grant_tx), Some(&mut line_monitor), &mut outcome)
                    .await
            })
        };
        let _ = tokio::time::timeout(Duration::from_secs(5), grant_rx.wait_for(|g| g.is_some())).await.unwrap();
        let mut sent = 0u64;
        for _ in 0..20 {
            for _ in 0..10 {
                if line.to_relay.try_send(tlt1_frame(0, &[0x55; 300])).is_ok() {
                    sent += 1;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let t0 = Instant::now();
        let mut got = 0u64;
        while got < sent && t0.elapsed() < Duration::from_secs(2) {
            if tokio::time::timeout(Duration::from_millis(200), seen.recv()).await.is_ok_and(|m| m.is_some()) {
                got += 1;
            }
        }
        let dropped = line.dropped_out.load(std::sync::atomic::Ordering::Relaxed);
        let _ = stx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        assert_eq!(sent, 200, "the line took every frame");
        assert_eq!((got, dropped), (200, 0), "every frame of a burst arrives on a free line");
    }

    /// A relay that stops reading does not stop the monitor (build 145): the
    /// TCP line's frames are dropped while one is still on its way out, so the
    /// loop goes on reading - here, TLT1 frames keep reaching the line while
    /// the line out is flooded. Before, `write.send(..).await` stood still on a
    /// full socket, and so did everything else (reviews 144).
    #[tokio::test]
    async fn a_relay_that_does_not_read_does_not_stop_the_monitor() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/ws", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let (mut write, mut read) = ws.split();
            let _hello = read.next().await;
            write.send(Message::Text(format!("OK relay-ready udp_token={}", "11".repeat(32)).into())).await.unwrap();
            // From here on it never reads again - and keeps sending.
            let _read = read;
            for n in 0u32.. {
                if write.send(Message::Binary(tlt1_frame(2, &n.to_le_bytes()).into())).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let (_udp, udp_port) = relay_udp().await;
        let status = RelayStatusHandle { state: Arc::new(Mutex::new(RelayStatus::disabled())) };
        let config = quic_config(url, udp_port);
        let (_ctx, mut config_rx) = watch::channel(config.clone());
        let (stx, mut shutdown_rx) = watch::channel(false);
        let (grant_tx, _grant_rx) = watch::channel(None);
        let (mut line, mut line_monitor) = tcp_line();
        let task = {
            let status = status.clone();
            tokio::spawn(async move {
                let mut outcome = SessionOutcome::default();
                run_connection(&config, &status, 1, &mut config_rx, &mut shutdown_rx, None, Some(&grant_tx), Some(&mut line_monitor), &mut outcome)
                    .await
            })
        };
        let flood = {
            let out = line.to_relay.clone();
            tokio::spawn(async move {
                let frame = tlt1_frame(0, &[0x55; 1200]);
                loop {
                    let _ = out.try_send(frame.clone());
                    tokio::task::yield_now().await;
                }
            })
        };
        // Over three seconds of flooding - long past any socket buffer on
        // loopback - every frame from the relay still arrives in good time.
        let t0 = Instant::now();
        let mut last = Instant::now();
        while t0.elapsed() < Duration::from_secs(3) {
            let got = tokio::time::timeout(Duration::from_millis(500), line.from_relay.recv()).await;
            assert!(got.is_ok(), "the monitor stopped reading after {:?} of flooding", t0.elapsed());
            assert!(last.elapsed() < Duration::from_millis(500));
            last = Instant::now();
        }
        flood.abort();
        // And it bounded: frames were dropped rather than piled up in the
        // wss side's buffer (review 145: without "one at a time" every test
        // stayed green while that buffer grew).
        assert!(line.dropped_out.load(std::sync::atomic::Ordering::Relaxed) > 0, "nothing dropped under the flood");
        let _ = stx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    /// In QUIC mode the monitor signs in with `proto=quic` (before `name=`, which
    /// must stay last), publishes the session's grant - the relay's UDP port, the
    /// token, the session - and the rotation as a new token in the same session,
    /// and sends no TLT1 frame (no capability frame, no tunnel). And nothing over
    /// UDP of its own: the relay binds a token to the first source it sees, and
    /// that has to be the route layer's socket.
    #[tokio::test]
    async fn quic_mode_signs_in_and_publishes_the_grant() {
        let (url, mut seen) = play_relay("TLR2 kind=hello role=station proto=quic").await;
        let (udp, udp_port) = relay_udp().await;
        let status = RelayStatusHandle { state: Arc::new(Mutex::new(RelayStatus::disabled())) };
        let config = quic_config(url, udp_port);
        let (_ctx, mut config_rx) = watch::channel(config.clone());
        let (stx, mut shutdown_rx) = watch::channel(false);
        let (grant_tx, mut grant_rx) = watch::channel(None);
        let task = {
            let status = status.clone();
            tokio::spawn(async move {
                let mut outcome = SessionOutcome::default();
                run_connection(&config, &status, 7, &mut config_rx, &mut shutdown_rx, None, Some(&grant_tx), None, &mut outcome).await
            })
        };
        let hello = match tokio::time::timeout(Duration::from_secs(5), seen.recv()).await.unwrap().unwrap() {
            Message::Text(t) => t.to_string(),
            other => panic!("{other:?}"),
        };
        assert!(hello.contains(" proto=quic name=Phone One"), "proto before the free-form name: {hello}");

        let first = tokio::time::timeout(Duration::from_secs(5), grant_rx.wait_for(|g| g.is_some())).await.unwrap().unwrap().unwrap();
        assert_eq!(first.relay, SocketAddr::from(([127, 0, 0, 1], udp_port)), "the host of the url, the UDP port");
        assert_eq!(first.token, [0x11; 32]);
        assert_eq!(first.session, 7);
        let second = tokio::time::timeout(Duration::from_secs(5), grant_rx.wait_for(|g| g.is_some_and(|g| g.token == [0x22; 32])))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(second.session, 7, "a rotation stays in its session");

        // The peer's hello says QUIC too: no complaint.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_ne!(status.state.lock().unwrap().last_error.as_deref(), Some("form mismatch"));
        // Past the library's own keepalive interval: nothing on the relay's UDP port.
        let mut buf = [0u8; 64];
        let heard = tokio::time::timeout(Duration::from_millis(1500), udp.recv_from(&mut buf)).await;
        assert!(heard.is_err(), "the monitor sent UDP of its own in QUIC mode: {heard:?}");
        let _ = stx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        while let Ok(m) = seen.try_recv() {
            assert!(!matches!(m, Message::Binary(_)), "no TLT1 frame in QUIC mode: {m:?}");
        }
    }

    /// A peer of the classic form (no `proto=quic`) against this QUIC side: the
    /// screen says the other side runs an older version.
    #[tokio::test]
    async fn a_peer_of_the_other_form_is_named() {
        mismatch_is_named("TLR2 kind=hello role=station", true, "runs an older version of ThetisLink").await;
    }

    /// And the other way round: a QUIC peer against this classic side (review
    /// 118: the check had run in one direction).
    #[tokio::test]
    async fn a_quic_peer_is_named_on_the_classic_side() {
        mismatch_is_named("TLR2 kind=hello role=station proto=quic", false, "runs a newer version of ThetisLink").await;
    }

    /// The relay's `udp-seen` line (phase 3c) lands in the grant of the session
    /// as this side's public address, and it is relay infrastructure: no peer
    /// traffic, so the status does not turn "peer online" on it.
    #[tokio::test]
    async fn the_public_address_the_relay_sees_goes_into_the_grant() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/ws", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let (mut write, mut read) = ws.split();
            let _hello = read.next().await;
            write.send(Message::Text(format!("OK relay-ready udp_token={}", "11".repeat(32)).into())).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            write.send(Message::Text("udp-seen 203.0.113.77:40000".into())).await.unwrap();
            while read.next().await.is_some() {}
        });
        let status = RelayStatusHandle { state: Arc::new(Mutex::new(RelayStatus::disabled())) };
        let config = quic_config(url, 4999);
        let (_ctx, mut config_rx) = watch::channel(config.clone());
        let (stx, mut shutdown_rx) = watch::channel(false);
        let (grant_tx, mut grant_rx) = watch::channel(None);
        let s = status.clone();
        let task = tokio::spawn(async move {
            let mut outcome = SessionOutcome::default();
            run_connection(&config, &s, 3, &mut config_rx, &mut shutdown_rx, None, Some(&grant_tx), None, &mut outcome).await
        });
        let want: SocketAddr = "203.0.113.77:40000".parse().unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), grant_rx.wait_for(|g| g.map_or(false, |g| g.public.is_some())))
            .await
            .expect("the public address never reached the grant")
            .unwrap()
            .unwrap();
        assert_eq!((got.public, got.session), (Some(want), 3), "this session's grant, with the address");
        assert!(!status.state.lock().unwrap().peer_seen, "the relay's line read as the peer");
        let _ = stx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    /// A peer's frame that arrives before the relay's OK is no answer: the
    /// relay puts the connection in the room before it sends the OK, so the
    /// other side's status or probe can come first (review 124, measured). The
    /// sign-in waits for the OK and the session runs - the public address still
    /// reaches the grant - instead of reading the frame as a rejection.
    #[tokio::test]
    async fn a_peer_frame_before_the_ok_is_no_rejection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/ws", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let (mut write, mut read) = ws.split();
            let _hello = read.next().await;
            write.send(Message::Text("TLR2 kind=status role=station".into())).await.unwrap();
            write.send(Message::Binary(vec![1u8, 2, 3, 4, 5].into())).await.unwrap();
            write.send(Message::Text(format!("OK relay-ready udp_token={}", "11".repeat(32)).into())).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            write.send(Message::Text("udp-seen 203.0.113.77:40000".into())).await.unwrap();
            while read.next().await.is_some() {}
        });
        let status = RelayStatusHandle { state: Arc::new(Mutex::new(RelayStatus::disabled())) };
        let config = quic_config(url, 4999);
        let (_ctx, mut config_rx) = watch::channel(config.clone());
        let (stx, mut shutdown_rx) = watch::channel(false);
        let (grant_tx, mut grant_rx) = watch::channel(None);
        let s = status.clone();
        let task = tokio::spawn(async move {
            let mut outcome = SessionOutcome::default();
            let result = run_connection(&config, &s, 1, &mut config_rx, &mut shutdown_rx, None, Some(&grant_tx), None, &mut outcome).await;
            (result, outcome.rejected)
        });
        tokio::time::timeout(Duration::from_secs(5), grant_rx.wait_for(|g| g.map_or(false, |g| g.public.is_some())))
            .await
            .expect("the session never ran: the peer frame was read as the relay's answer")
            .unwrap();
        let _ = stx.send(true);
        let (_, rejected) = tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
        assert_eq!(rejected, None, "the peer frame was taken for a rejection");
    }

    /// The warning stays while the forms disagree: bare traffic from the peer
    /// after its frame of the other form does not paint the status green again
    /// (field test 2026-10-06: it flipped between red and green), and neither
    /// does the answer to this side's binary probe, which goes out once a
    /// second - so the test watches for longer than that (review 122: the probe
    /// branch was run by no test).
    #[tokio::test]
    async fn the_form_warning_is_not_painted_over() {
        let (probes_tx, mut probes_rx) = mpsc::unbounded_channel::<()>();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/ws", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let (mut write, mut read) = ws.split();
            let _hello = read.next().await;
            write.send(Message::Text(format!("OK relay-ready udp_token={}", "11".repeat(32)).into())).await.unwrap();
            write.send(Message::Text("TLR2 kind=hello role=station proto=quic".into())).await.unwrap();
            for _ in 0..5 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                write.send(Message::Binary(vec![1u8, 2, 3, 4, 5].into())).await.unwrap();
                write.send(Message::Text("not a peer frame".into())).await.unwrap();
            }
            // Answer every probe, as the peer would.
            while let Some(Ok(m)) = read.next().await {
                if let Message::Binary(data) = m {
                    if let Some((BINARY_KIND_PING, seq)) = parse_binary_probe(&data) {
                        let pong = binary_probe_frame(BINARY_KIND_PONG, RelayRole::Station, seq, data.len());
                        let _ = write.send(Message::Binary(pong.into())).await;
                        let _ = probes_tx.send(());
                    }
                }
            }
        });
        let status = RelayStatusHandle { state: Arc::new(Mutex::new(RelayStatus::disabled())) };
        let config = RelayConfig { quic: false, udp_port: None, ..quic_config(url, 4999) };
        let (_ctx, mut config_rx) = watch::channel(config.clone());
        let (stx, mut shutdown_rx) = watch::channel(false);
        let s = status.clone();
        let task = tokio::spawn(async move {
            let mut outcome = SessionOutcome::default();
            run_connection(&config, &s, 1, &mut config_rx, &mut shutdown_rx, None, None, None, &mut outcome).await
        });
        // Sampled through the whole burst of traffic and a little after it.
        let t0 = Instant::now();
        let mut seen = false;
        while t0.elapsed() < Duration::from_millis(1_700) {
            {
                let st = status.state.lock().unwrap();
                if st.last_error.as_deref() == Some("form mismatch") {
                    seen = true;
                } else if seen {
                    panic!("the warning was painted over: {:?} {}", st.phase, st.message);
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(seen, "the warning never came");
        assert!(probes_rx.try_recv().is_ok(), "no probe was answered in the window - the probe branch was not run");
        let (phase, error) = {
            let st = status.state.lock().unwrap();
            (st.phase.clone(), st.last_error.clone())
        };
        assert_eq!((phase, error.as_deref()), (RelayPhase::Error, Some("form mismatch")));
        let _ = stx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    async fn mismatch_is_named(peer: &'static str, own_quic: bool, says: &str) {
        let (url, _seen) = play_relay(peer).await;
        let status = RelayStatusHandle { state: Arc::new(Mutex::new(RelayStatus::disabled())) };
        // The classic side without UDP of its own: the frame is the point here.
        let config = RelayConfig { quic: own_quic, udp_port: own_quic.then_some(4999), ..quic_config(url, 4999) };
        let (_ctx, mut config_rx) = watch::channel(config.clone());
        let (stx, mut shutdown_rx) = watch::channel(false);
        let (grant_tx, _grant_rx) = watch::channel(None);
        let s = status.clone();
        let task = tokio::spawn(async move {
            let mut outcome = SessionOutcome::default();
            let grants = own_quic.then_some(&grant_tx);
            run_connection(&config, &s, 1, &mut config_rx, &mut shutdown_rx, None, grants, None, &mut outcome).await
        });
        let t0 = Instant::now();
        loop {
            {
                let st = status.state.lock().unwrap();
                if st.last_error.as_deref() == Some("form mismatch") {
                    assert_eq!(st.phase, RelayPhase::Error);
                    assert!(st.message.contains(says), "{}", st.message);
                    break;
                }
            }
            assert!(t0.elapsed() < Duration::from_secs(5), "no mismatch said");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = stx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    /// Every relay line this library reads is one the relay refuses to forward
    /// from a peer: the word stands in `thetislink-relay` as a `LINE_` constant,
    /// and that constant is in its `RELAY_LINES`. Two programs, so a test between
    /// them (working rule 15; review 123).
    #[test]
    fn the_relay_refuses_every_line_this_library_reads() {
        let relay = include_str!("../../thetislink-relay/src/main.rs");
        let list = relay
            .lines()
            .find(|l| l.starts_with("const RELAY_LINES"))
            .expect("the relay has its RELAY_LINES");
        // The words this library reads, from its own source - every
        // `const RELAY_<...>: &str = "<word>";` - not from a second list that
        // could leave one out (review 124: such a list kept the suite green with
        // udp-seen gone from both sides).
        let words: Vec<&str> = include_str!("lib.rs")
            .lines()
            .filter(|l| l.starts_with("const RELAY_") && l.contains(": &str = \""))
            .map(|l| l.split('"').nth(1).unwrap())
            .collect();
        assert!(words.len() >= 5, "the library's relay words were not found: {words:?}");
        for word in words {
            let decl = relay
                .lines()
                .find(|l| l.starts_with("const LINE_") && l.ends_with(&format!("= \"{word}\";")))
                .unwrap_or_else(|| panic!("the relay has no LINE_ constant for {word:?}"));
            let name = decl.trim_start_matches("const ").split(':').next().unwrap();
            assert!(list.contains(name), "{name} ({word:?}) is not in the relay's RELAY_LINES");
        }
    }

    /// The TLU1 header is defined twice: here (the apps) and in the relay on the
    /// VPS (`thetislink-relay`, its own program). Since build 117 the QUIC
    /// route layer's MTU ceiling counts on its length, so the two are compared:
    /// the relay's source is read and its constants must be these (review 117).
    #[test]
    fn the_tlu1_header_matches_the_relay_on_the_vps() {
        let relay = include_str!("../../thetislink-relay/src/main.rs");
        for line in [
            "const TLU1_MAGIC: &[u8; 4] = b\"TLU1\";",
            "const TLU1_VERSION: u8 = 1;",
            "const TLU1_TOKEN_LEN: usize = 32;",
            "const TLU1_HEADER_LEN: usize = 4 + 1 + 1 + 1 + 4 + TLU1_TOKEN_LEN; // = 43",
        ] {
            assert!(relay.contains(line), "the relay on the VPS no longer says: {line}");
        }
        assert_eq!((&TLU1_MAGIC, TLU1_VERSION, TLU1_TOKEN_LEN, TLU1_HEADER_LEN), (b"TLU1", 1, 32, 43));
    }

    /// The monitor's grant follows its sessions: there while one stands, gone
    /// (`None`) the moment it ends - its token goes with the connection.
    #[test]
    fn the_grant_is_withdrawn_when_the_session_ends() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let url = rt.block_on(async { play_relay_then("TLR2 kind=hello role=station proto=quic", true).await.0 });
        let (grant_tx, mut grant_rx) = watch::channel(None);
        let monitor = RelayMonitor::start_threaded_quic(quic_config(url, 4999), grant_tx, None);
        rt.block_on(async {
            let got = tokio::time::timeout(Duration::from_secs(5), grant_rx.wait_for(|g| g.is_some())).await.is_ok();
            assert!(got, "a grant while the session stands");
            let gone = tokio::time::timeout(Duration::from_secs(5), grant_rx.wait_for(|g| g.is_none())).await;
            assert!(gone.is_ok(), "no grant after the session ended");
        });
        drop(monitor);
    }

    /// The first address, and the IPv4 one beside it only when the first is not
    /// IPv4 (field test 119: a 5G net gave the relay's name an IPv6 address first).
    #[test]
    fn the_first_address_and_an_ipv4_one_beside_it() {
        let v6: SocketAddr = "[2001:db8::21]:443".parse().unwrap();
        let v4: SocketAddr = "192.0.2.33:443".parse().unwrap();
        assert_eq!(first_and_ipv4([v6, v4]), Some((v6, Some(v4))));
        assert_eq!(first_and_ipv4([v4, v6]), Some((v4, None)), "an IPv4 first needs nothing beside it");
        assert_eq!(first_and_ipv4([v6]), Some((v6, None)), "no IPv4 to fall back to");
        assert_eq!(first_and_ipv4(Vec::new()), None);
        // Two IPv4 addresses: the second is no fallback for the first (review 120).
        let v4b: SocketAddr = "192.0.2.34:443".parse().unwrap();
        assert_eq!(first_and_ipv4([v4, v4b, v6]), Some((v4, None)));
        // The word the log lines carry - the field test reads the hypothesis of
        // 119 off it, so a wrong one would give a wrong verdict (review 120).
        assert_eq!((family(v4), family(v6)), ("IPv4", "IPv6"));
    }

    /// A socket for an IPv6 address is opened in IPv6, and data goes both ways
    /// (build 119 bound IPv4 for it: os error 97 on the phone). And IPv4 still.
    #[tokio::test]
    async fn a_socket_follows_the_family_of_its_address() {
        for local in ["[::1]:0", "127.0.0.1:0"] {
            let relay = UdpSocket::bind(local).await.unwrap();
            let addr = relay.local_addr().unwrap();
            let sock = connect_udp(addr).await.unwrap_or_else(|e| panic!("{}: {e}", family(addr)));
            sock.send(b"ping").await.unwrap();
            let mut buf = [0u8; 8];
            let (n, from) = tokio::time::timeout(Duration::from_secs(2), relay.recv_from(&mut buf)).await.unwrap().unwrap();
            assert_eq!(&buf[..n], b"ping");
            relay.send_to(b"pong", from).await.unwrap();
            let n = tokio::time::timeout(Duration::from_secs(2), sock.recv(&mut buf)).await.unwrap().unwrap();
            assert_eq!(&buf[..n], b"pong", "{}", family(addr));
        }
    }

    /// No socket for the first address: the IPv4 one beside it is used, and
    /// said. Without one beside it, the failure is the answer.
    #[tokio::test]
    async fn no_socket_for_the_first_address_falls_back_to_ipv4() {
        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let v4 = relay.local_addr().unwrap();
        let (sock, used) = connect_relay_udp(UNREACHABLE_FOR_A_SOCKET, Some(v4)).await.expect("the IPv4 address");
        assert_eq!(used, v4);
        sock.send(b"via ipv4").await.unwrap();
        let mut buf = [0u8; 16];
        let n = tokio::time::timeout(Duration::from_secs(2), relay.recv(&mut buf)).await.unwrap().unwrap();
        assert_eq!(&buf[..n], b"via ipv4");
        assert!(connect_relay_udp(UNREACHABLE_FOR_A_SOCKET, None).await.is_err(), "nothing to fall back to");
        // The first address wins when it works: the fallback is only the way back.
        let (_, used) = connect_relay_udp(v4, Some("127.0.0.1:9".parse().unwrap())).await.unwrap();
        assert_eq!(used, v4);
    }

    /// The unconnected socket for the QUIC leg: the first address in the order
    /// that a socket can be connected to, and data goes both ways - to the
    /// relay and back, and to any other address from the same socket (the
    /// direct path after punching).
    #[tokio::test]
    async fn the_unconnected_relay_socket_takes_the_first_usable_address() {
        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let other = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let v4 = relay.local_addr().unwrap();
        let (sock, used) = bind_relay_udp(&[UNREACHABLE_FOR_A_SOCKET, v4]).await.expect("the IPv4 address");
        assert_eq!(used, v4, "an address no socket reaches is passed over");
        let (sock2, used2) = bind_relay_udp(&[v4, UNREACHABLE_FOR_A_SOCKET]).await.unwrap();
        assert_eq!(used2, v4, "the first wins when it works");
        drop(sock2);
        assert!(bind_relay_udp(&[UNREACHABLE_FOR_A_SOCKET]).await.is_err(), "nothing usable");
        assert!(bind_relay_udp(&[]).await.is_err(), "nothing at all");
        let mut buf = [0u8; 16];
        for (to, name) in [(&relay, "the relay"), (&other, "another address")] {
            sock.send_to(name.as_bytes(), to.local_addr().unwrap()).await.unwrap();
            let (n, from) = tokio::time::timeout(Duration::from_secs(2), to.recv_from(&mut buf)).await.unwrap().unwrap();
            assert_eq!(&buf[..n], name.as_bytes());
            to.send_to(b"back", from).await.unwrap();
            let (n, src) = tokio::time::timeout(Duration::from_secs(2), sock.recv_from(&mut buf)).await.unwrap().unwrap();
            assert_eq!((&buf[..n], src), (&b"back"[..], to.local_addr().unwrap()), "{name}");
        }
    }

    /// An address no socket can be connected to: a link-local IPv6 address on a
    /// network interface that does not exist (measured on Windows: "unreachable
    /// network", 10051). Port 0 does not do: Windows connects to it. A copy sits
    /// in the QUIC route layer's test (`no_socket_for_the_first_address_goes_over_the_ipv4_one`);
    /// change both.
    const UNREACHABLE_FOR_A_SOCKET: SocketAddr = SocketAddr::V6(std::net::SocketAddrV6::new(
        std::net::Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1),
        443,
        0,
        9999,
    ));

    /// The two classic constructors sign in in the classic form, whatever the
    /// config says: QUIC mode without a grant receiver would publish nothing
    /// (review 118).
    #[test]
    fn the_classic_constructors_sign_in_classic() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        for tunnel in [false, true] {
            let (url, mut seen) = rt.block_on(play_relay("TLR2 kind=hello role=station"));
            let config = RelayConfig { udp_port: None, ..quic_config(url, 4999) };
            assert!(config.quic, "asked for QUIC");
            let monitor = if tunnel {
                let (inbound_tx, _inbound_rx) = mpsc::unbounded_channel();
                let (_uplink_tx, uplink_rx) = mpsc::unbounded_channel();
                let sentinel = SocketAddr::from(([203, 0, 113, 1], 4580));
                RelayMonitor::start_threaded_tunnel(config, RelayTunnel { sentinel, inbound_tx, uplink_rx })
            } else {
                RelayMonitor::start_threaded(config)
            };
            let hello = rt.block_on(async {
                match tokio::time::timeout(Duration::from_secs(5), seen.recv()).await.unwrap().unwrap() {
                    Message::Text(t) => t.to_string(),
                    other => panic!("{other:?}"),
                }
            });
            assert!(!hello.contains("proto=quic"), "tunnel {tunnel}: signed in classic: {hello}");
            drop(monitor);
        }
    }

    /// The peer frames carry the form both ways, and a frame without it reads
    /// as the classic form.
    #[test]
    fn the_peer_frames_carry_the_form() {
        let q = parse_peer_frame(&peer_frame("hello", RelayRole::Station, true)).unwrap();
        assert!(q.quic);
        let q = parse_peer_frame(&peer_frame_with_seq("ping", RelayRole::Client, 5, true)).unwrap();
        assert!(q.quic && q.seq == Some(5));
        let c = parse_peer_frame(&peer_frame("status", RelayRole::Client, false)).unwrap();
        assert!(!c.quic);
        assert!(!parse_peer_frame("TLR2 kind=hello role=station").unwrap().quic, "a peer from before 3b");
    }
}
