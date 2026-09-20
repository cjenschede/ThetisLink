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
use tokio::task::JoinHandle;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::MaybeTlsStream;

const RECONNECT_DELAY: Duration = Duration::from_secs(5);
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

// TLU1 = tunneled audio over the bare-UDP path (PATCH-relay-audio-udp, fase 2).
// Layout: magic(4) | version(1) | flags(1) | client_id(1) | seq(4 LE) | token(32 raw) |
// AudioPacket. Only audio packet types travel over this; everything else stays on TLT1/wss.
const TLU1_MAGIC: [u8; 4] = *b"TLU1";
const TLU1_VERSION: u8 = 1;
const TLU1_TOKEN_LEN: usize = 32;
const TLU1_HEADER_LEN: usize = 4 + 1 + 1 + 1 + 4 + TLU1_TOKEN_LEN; // = 43

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
/// target). `token_hex` is the fase-0 capability from the ready-reply.
fn encode_tlu1(client_id: u8, seq: u32, token_hex: &str, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(TLU1_HEADER_LEN + payload.len());
    f.extend_from_slice(&TLU1_MAGIC);
    f.push(TLU1_VERSION);
    f.push(0); // flags (reserved)
    f.push(client_id);
    f.extend_from_slice(&seq.to_le_bytes());
    f.extend_from_slice(&hex32(token_hex));
    f.extend_from_slice(payload);
    f
}

/// Extract `udp_token=<hex>` from the relay ready-reply (fase 0). The token is the
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

// --- Fase 3: make-before-break UDP<->wss fallback (relay-only) ---

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
    since_ready: Duration,
    last_downlink: Option<Instant>,
    connect_timeout: Duration,
    stall_after: Duration,
) -> bool {
    match last_downlink {
        // Nothing has ever come down this path: only the connect window can decide.
        None => since_ready >= connect_timeout,
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
/// Used for TX-always-both (fase 3b section 14.3).
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

/// Open + connect a UDP socket to the relay's UDP data port (fase 2). Best-effort;
/// on any failure the caller falls back to audio-over-wss.
async fn open_udp(url: &str, port: u16) -> Result<UdpSocket, String> {
    let host = url_host(url).ok_or_else(|| "bad relay url".to_string())?;
    let addr = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| e.to_string())?
        .next()
        .ok_or_else(|| "relay UDP address not resolved".to_string())?;
    let bind = if addr.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" };
    let sock = UdpSocket::bind(bind).await.map_err(|e| e.to_string())?;
    sock.connect(addr).await.map_err(|e| e.to_string())?;
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

// Relay sentinel convention. The relay is the leaf crate, so it OWNS this
// mapping; the server (`tracked_socket::relay_sentinel_for`) mirrors it because
// it cannot be imported the other way without a dependency cycle. A parity test
// in the server crate (which links both) fails the build if the two drift.
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
    /// UDP port of the relay's audio data path (PATCH-relay-audio-udp, fase 2).
    /// `Some(port)` = route audio over kale UDP to `host(url):port`; `None` = all
    /// traffic over wss as before (additive). Falls back to wss if UDP can't open.
    pub udp_port: Option<u16>,
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
    /// Fase 3c: true while audio is on the wss TCP-fallback (UDP degraded/blocked); false
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

pub struct RelayMonitor {
    status: RelayStatusHandle,
    config_tx: watch::Sender<RelayConfig>,
    shutdown_tx: watch::Sender<bool>,
    task: RelayTask,
}

enum RelayTask {
    Tokio(JoinHandle<()>),
    Thread,
}

impl RelayMonitor {
    pub fn start(initial: RelayConfig) -> Self {
        let status = RelayStatusHandle {
            state: Arc::new(Mutex::new(RelayStatus::disabled())),
        };
        let (config_tx, config_rx) = watch::channel(initial);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task_status = status.clone();
        let task = tokio::spawn(async move {
            monitor_loop(config_rx, shutdown_rx, task_status, None).await;
        });
        Self {
            status,
            config_tx,
            shutdown_tx,
            task: RelayTask::Tokio(task),
        }
    }

    /// Status-only monitor on its own thread (no TL tunnel).
    pub fn start_threaded(initial: RelayConfig) -> Self {
        Self::start_threaded_inner(initial, None)
    }

    /// Monitor on its own thread that also tunnels TL frames (Phase C).
    pub fn start_threaded_tunnel(initial: RelayConfig, tunnel: RelayTunnel) -> Self {
        Self::start_threaded_inner(initial, Some(tunnel))
    }

    fn start_threaded_inner(initial: RelayConfig, tunnel: Option<RelayTunnel>) -> Self {
        let status = RelayStatusHandle {
            state: Arc::new(Mutex::new(RelayStatus::disabled())),
        };
        let (config_tx, config_rx) = watch::channel(initial);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task_status = status.clone();
        let spawn_status = status.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("thetislink-relay-monitor".to_string())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(rt) => rt.block_on(monitor_loop(config_rx, shutdown_rx, task_status.clone(), tunnel)),
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
            task: RelayTask::Thread,
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
}

impl Drop for RelayMonitor {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
        if let RelayTask::Tokio(task) = &self.task {
            task.abort();
        }
    }
}

async fn monitor_loop(
    mut config_rx: watch::Receiver<RelayConfig>,
    mut shutdown_rx: watch::Receiver<bool>,
    status: RelayStatusHandle,
    mut tunnel: Option<RelayTunnel>,
) {
    let mut generation = 0_u64;
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

        let mut outcome = SessionOutcome::default();
        let run_result = run_connection(
            &config,
            &status,
            generation,
            &mut config_rx,
            &mut shutdown_rx,
            tunnel.as_mut(),
            &mut outcome,
        )
        .await;
        if *shutdown_rx.borrow() {
            continue;
        }
        if config_rx.borrow().clone() != config {
            continue;
        }

        if let Err(err) = run_result {
            // How this session ended, in one line. The log said what went wrong
            // and never whether the session had been working, so a refusal and a
            // network switch read the same.
            log::info!(
                "Relay session ended: {} (carried_traffic={} rejected={})",
                short_error(&err),
                outcome.carried_traffic,
                outcome.rejected.as_deref().unwrap_or("-"),
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

        tokio::select! {
            _ = tokio::time::sleep(RECONNECT_DELAY) => {}
            _ = config_rx.changed() => {}
            _ = shutdown_rx.changed() => {}
        }
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
/// error. Nothing here changes behaviour: both fields are written and logged and
/// nothing branches on them. Step 3 of the design is what will.
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
}

async fn run_connection(
    config: &RelayConfig,
    status: &RelayStatusHandle,
    generation: u64,
    config_rx: &mut watch::Receiver<RelayConfig>,
    shutdown_rx: &mut watch::Receiver<bool>,
    tunnel: Option<&mut RelayTunnel>,
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
    let (ws, _) = connect_async(config.url.as_str())
        .await
        .map_err(|e| e.to_string())?;
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

    let first = tokio::time::timeout(Duration::from_secs(10), read.next())
        .await
        .map_err(|_| "relay hello timed out".to_string())?
        .ok_or_else(|| "relay closed before authentication".to_string())?
        .map_err(|e| e.to_string())?;

    // Fase 2: the ready-reply carries the UDP capability token ("OK relay-ready
    // udp_token=..."); capture it to open the audio-over-UDP path below. Assigned on
    // the OK arm; the other arms diverge (return), so it is always initialised after.
    // `mut`: the relay rotates this token over wss before its hard TTL; the loop
    // swaps in each fresh token for outgoing TLU1 packets (see the Text arm below).
    let mut udp_token: Option<String>;
    match first {
        Message::Text(text) if text.starts_with("OK ") => {
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

    // Fase 2: open the kale-UDP audio path when configured and a token was issued.
    // Best-effort - on any failure we simply keep audio on wss (additive fallback).
    let udp_socket: Option<UdpSocket> = match (config.udp_port, &udp_token) {
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
        .send(Message::Text(peer_frame("hello", config.role).into()))
        .await
        .map_err(|e| e.to_string())?;

    let mut last_ping = Instant::now();
    let mut last_status = Instant::now();
    let mut last_rtt_ping = Instant::now();
    let mut rtt_seq = 0_u64;
    let mut pending_rtt: Option<(u64, Instant)> = None;
    let mut peer_active = false;
    let mut last_peer_activity = Instant::now();
    let mut last_binary_probe = Instant::now();
    let mut binary_seq = 0_u64;
    let mut pending_binary: Option<(u64, Instant)> = None;
    let mut binary_samples: VecDeque<u32> = VecDeque::new();

    // Fase 3a: make-before-break UDP<->wss fallback (relay-only, additive - inert while the
    // fallback stays off, so the fase-2 UDP path is byte-identical in the normal case).
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
    // fase 3a. Not "silently ignored at this layer", which is what the design said and
    // a review rightly would not accept on trust.
    //
    // Repeated below on the heartbeat, and that is not belt and braces: the station
    // keeps this fact inside the per-client fallback entry, and that entry is dropped a
    // lease after the last control frame. Announced once, it is gone fifteen seconds
    // into every session - long before any realistic network switch.
    if config.role == RelayRole::Client {
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
    // And the other end of that: datagrams the client actually received. It settles a
    // question the station log cannot answer on its own. A fallback of 393 seconds was
    // observed while the station sent 330 beacons, and nothing said whether the client
    // got almost none of them (the path really was dead, which is accepted) or plenty
    // with gaps just over the threshold (recovery stricter than "usable", which would
    // be a second subject). One counter tells those apart.
    let mut downlink_received: u64 = 0;

    // Persistent 250 ms ticker, created ONCE before the loop. A `sleep()` re-created
    // each select iteration gets reset by every audio packet (<250 ms apart) and never
    // elapses under continuous audio - which starved the UDP keepalive + peer-status +
    // probes. An interval keeps its own deadline, so the timer branch fires reliably
    // regardless of audio load. Skip missed ticks (no catch-up burst).
    let mut timer = tokio::time::interval(Duration::from_millis(250));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
                        if text.starts_with("chat-ticket-rotate") {
                            // Refreshed well inside the ticket's own lifetime, so
                            // the chat window never finds itself holding an
                            // expired one. Relay infrastructure, not peer
                            // activity - so it does not count as the peer being
                            // alive, exactly like the token rotation below.
                            if let Some(t) = parse_chat_ticket(&text) {
                                set_chat_ticket(&status, Some(t));
                                log::debug!("Relay chat ticket refreshed");
                            }
                        } else if text.starts_with("udp-token-rotate") {
                            if let Some(t) = parse_udp_token(&text) {
                                udp_token = Some(t);
                                log::debug!("Relay UDP token rotated");
                            }
                            continue;
                        }
                        peer_active = true;
                        outcome.carried_traffic = true;
                        last_peer_activity = Instant::now();
                        if let Some(frame) = parse_peer_frame(&text) {
                            match frame.kind.as_str() {
                                "ping" => {
                                    if let Some(seq) = frame.seq {
                                        write
                                            .send(Message::Text(peer_frame_with_seq("pong", config.role, seq).into()))
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
                        } else {
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
                        outcome.carried_traffic = true;
                        last_peer_activity = Instant::now();
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
                                // Fase 3a: station intercepts a client's wss-audio fallback
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
                                // Fase 3: during path overlap / TX-always-both a frame may
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
                                        set_binary_stats(
                                            status,
                                            binary_stats(&binary_samples, rtt_ms),
                                            generation,
                                        );
                                    }
                                }
                            }
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
                    Some(Ok(Message::Close(_))) => return Err("relay closed connection".to_string()),
                    Some(Ok(Message::Frame(_))) => {}
                    Some(Err(e)) => return Err(e.to_string()),
                    None => return Err("relay disconnected".to_string()),
                }
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
                        // Fase 3b transport policy (additive; both flags off = fase-2
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
                // Fase 2 downlink: relay delivers [client_id(1)] + AudioPacket over UDP.
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
                        outcome.carried_traffic = true;
                        last_peer_activity = Instant::now();
                        let cid = udp_buf[0];
                        let payload = &udp_buf[1..n];
                        // Fase 3: UDP-audio health (client fallback monitor + recovery run).
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
                        // Fase 3: drop a frame already delivered over wss during overlap.
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

                // Fase 3b transport state machine (client, relay-only). Two states:
                //  - UDP (fallback off): request wss if nothing arrived down the path within
                //    the connect window, or the downlink went quiet past
                //    FALLBACK_DOWNLINK_STALL.
                //  - Fallback (on): return to UDP-only once UDP audio has flowed healthily for
                //    FALLBACK_RECOVER (> stall = hysteresis). The station keeps sending UDP
                //    during fallback, so recovery is detectable.
                // A minimum dwell between flips (dwell-cap + rate-limit) prevents flapping. The flip
                // request tunnels an RCF over wss (id 0; the relay stamps the real client id).
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
                                ready_at.elapsed(),
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
                        .send(Message::Text(peer_frame("status", config.role).into()))
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
                    if config.role == RelayRole::Station && beacons_sent > 0 {
                        log::info!(
                            "Relay station: {beacons_sent} beacons to {} client(s), \
                             {fallback_without_beacon_reader} audio frames to a client on \
                             wss that is not known to read one",
                            bound_clients.len()
                        );
                        beacons_sent = 0;
                        fallback_without_beacon_reader = 0;
                    }
                }

                if last_rtt_ping.elapsed() >= PEER_RTT_INTERVAL {
                    rtt_seq = rtt_seq.wrapping_add(1);
                    let now = Instant::now();
                    write
                        .send(Message::Text(peer_frame_with_seq("ping", config.role, rtt_seq).into()))
                        .await
                        .map_err(|e| e.to_string())?;
                    pending_rtt = Some((rtt_seq, now));
                    last_rtt_ping = now;
                }

                if last_binary_probe.elapsed() >= BINARY_PROBE_INTERVAL {
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
                    if let (Some(sock), Some(tok)) = (&udp_socket, &udp_token) {
                        match config.role {
                            // A client has one peer, and id 0 means "the station" on
                            // the way up. Unchanged.
                            RelayRole::Client => {
                                udp_seq = udp_seq.wrapping_add(1);
                                let _ = sock.send(&encode_tlu1(0, udp_seq, tok, &[])).await;
                            }
                            // A station has many, and the relay routes downstream on an
                            // exact client id - so a keepalive addressed to 0 reached
                            // whichever client happened to hold that id, and nobody at
                            // all when none did. That is why no client has ever seen a
                            // downlink beacon.
                            //
                            // Sent CONTINUOUSLY, not only during a fallback: the entry
                            // test needs beacons already flowing before the path breaks,
                            // or it reads their absence as a broken path and flaps.
                            RelayRole::Station => {
                                for cid in bound_clients.iter().copied() {
                                    udp_seq = udp_seq.wrapping_add(1);
                                    let _ =
                                        sock.send(&encode_tlu1(cid, udp_seq, tok, &[])).await;
                                    beacons_sent += 1;
                                }
                            }
                        }
                    }
                    last_udp_keepalive = Instant::now();
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
}

fn peer_frame(kind: &str, role: RelayRole) -> String {
    format!("TLR2 kind={kind} role={}", role.as_str())
}

fn peer_frame_with_seq(kind: &str, role: RelayRole, seq: u64) -> String {
    format!("TLR2 kind={kind} role={} seq={seq}", role.as_str())
}

fn parse_peer_frame(text: &str) -> Option<PeerFrame> {
    let mut parts = text.split_whitespace();
    if parts.next()? != "TLR2" {
        return None;
    }

    let mut kind = None;
    let mut role = None;
    let mut seq = None;
    for part in parts {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key {
            "kind" => kind = Some(value.to_string()),
            "role" => role = Some(value.to_string()),
            "seq" => seq = value.parse::<u64>().ok(),
            _ => {}
        }
    }

    Some(PeerFrame {
        kind: kind.unwrap_or_else(|| "status".to_string()),
        role: role.unwrap_or_else(|| "peer".to_string()),
        seq,
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
}

/// Fase 3c: update the transport-fallback flag shown by the client indicator. Mutates only
/// that field, so it survives the next set_status refresh.
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
        let long_ago = Instant::now() - Duration::from_secs(5760);
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

#[cfg(test)]
mod tests {
    use super::*;

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
            Duration::from_millis(500),
            None,
            FALLBACK_CONNECT_TIMEOUT,
            FALLBACK_DOWNLINK_STALL
        ));

        // 2. nothing has ever arrived and the connect window has passed: UDP is blocked.
        assert!(downlink_stalled(
            FALLBACK_CONNECT_TIMEOUT + Duration::from_millis(1),
            None,
            FALLBACK_CONNECT_TIMEOUT,
            FALLBACK_DOWNLINK_STALL
        ));

        // 3. the load-bearing one: a beacon one second ago, no audio at all, every
        //    channel switched off. This must not be a stall, and nothing else in the
        //    workspace says so.
        assert!(!downlink_stalled(
            Duration::from_secs(60),
            Some(Instant::now() - beacon_interval),
            FALLBACK_CONNECT_TIMEOUT,
            FALLBACK_DOWNLINK_STALL
        ));

        // 4. the beacons themselves stopped: the path is gone.
        assert!(downlink_stalled(
            Duration::from_secs(60),
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
        let (client_id, out) = parse_tlt1(&frame).expect("moet parsen");
        assert_eq!(client_id, 0);
        assert_eq!(out, payload);
    }

    #[test]
    fn tlt1_is_distinct_from_tlb1_probe() {
        // A TLB1 probe must NOT parse as TLT1, and vice versa.
        let probe = binary_probe_frame(BINARY_KIND_PING, RelayRole::Station, 7, BINARY_PROBE_LEN);
        assert!(parse_tlt1(&probe).is_none(), "TLB1-probe parseerde als TLT1");

        let tunnel = encode_tlt1(0, b"hello");
        assert!(
            parse_binary_probe(&tunnel).is_none(),
            "TLT1-tunnel parseerde als TLB1-probe"
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
        // client 0 -> 203.0.113.1 (matches server tracked_socket::relay_sentinel_for(0)).
        assert_eq!(sentinel_for_client_id(0).ip().to_string(), "203.0.113.1");
        assert_eq!(sentinel_for_client_id(1).ip().to_string(), "203.0.113.2");
    }

    #[test]
    fn tlt1_empty_payload_roundtrips() {
        let frame = encode_tlt1(3, b"");
        let (id, out) = parse_tlt1(&frame).expect("leeg payload moet parsen");
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
