// SPDX-License-Identifier: GPL-2.0-or-later

//! PstRotator UDP listener - parallel input source alongside the active
//! `rotor_backend`. Translates incoming PstRotator azimuth broadcasts
//! into `RotorCmd::GoTo` on the shared `Rotor` facade so that a
//! logger such as Log4OM can drive the real rotor hardware via
//! PstRotator - regardless of which backend (EA7HG / PstRotator-outgoing /
//! Adafruit MCP2221A) actually operates that hardware.
//!
//! **Topology:**
//!
//! ```text
//! Win4OM / Log4OM
//!   ↓ (XML over Log4OM rotor logic)
//! PstRotator (Win4OM-PC)
//!   ↓ UDP broadcast to configured endpoints
//! ThetisLink-server (this module)
//!   ↓ RotorCmd::GoTo(angle_x10)
//! Rotor facade
//!   ↓
//! active backend (EA7HG / PstRotator / MCP2221A)
//! ```
//!
//! **Accepted packet formats** - pick one in PstRotator:
//!
//! - **Yaesu GS-232A / GS-232B** (PstRotator -> "Controller: Yaesu
//!   GS-232A/B"; **recommended**): textual ASCII commands, simple
//!   and well documented.
//!   - `M<nnn>\r` - move to azimuth (3-digit 000-450). Example:
//!     `M090\r`.
//!   - `S\r` - stop
//!   - `C\r` - current position query. Reply: `+<nnn>\r` (3-digit).
//!   - `C2\r` - azimuth + elevation query. Reply: `+0aaa+0eee\r`
//!     (we always send elevation 000 - no elevation axis).
//!   - `R\r` / `L\r` - manual rotate (are ignored).
//!   Bidirectional protocol: listener replies to `C` / `C2` with the
//!   current rotor position so that PstRotator can synchronise its
//!   display.
//!
//! - **Prosistel binary (EA7HG variant)** (PstRotator -> "Controller:
//!   EA7HG Visual Rotor"): `\x02AG<nnn>\r` or `AAG<nnn>\r`. Stop is
//!   `\x02AG999\r` or `AAR\r`. Status query `\x02A?\r` or `AA?\r`,
//!   reply `\x02A,?,<nnn>,<R|B>\r`. Works but less standardised
//!   than GS-232A.
//!
//! - **Text-mode broadcast** (PstRotator's reply format as output):
//!   `AZ:nnn.n\r`. One-way traffic; no status replies. Example:
//!   `AZ:271.5\r`.
//!
//! - **XML mode** (PstRotator "Output" forwarding):
//!   `<PST><AZIMUTH>nnn.n</AZIMUTH></PST>`.
//!
//! `EL:...` / `<ELEVATION>` is not implemented in phase 1 -
//! ThetisLink's Rotor facade has no elevation axis. They are silently
//! ignored via `debug!`.
//!
//! **Loop protection:** when `rotor_backend = pstrotator` (the
//! outgoing backend) is also running, we do not accidentally pick up
//! its AZ? replies. PstRotator answers replies on `port + 1`
//! (default 12001) - if the user also listens there a feedback loop
//! arises. The UI notes this; at runtime we ignore packets
//! whose azimuth equals the last outgoing GoTo
//! within `LOOPBACK_DEDUP_WINDOW`.
//!
//! **Rate limit:** PstRotator typically broadcasts every 0.5-1 s even
//! when the azimuth is unchanged. An identical azimuth within
//! `DEDUPE_INTERVAL` is filtered out so that the Adafruit's poll thread
//! does not needlessly re-evaluate a GoTo on every tick.

use std::io::{BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use log::{debug, info, warn};

use crate::rotor::{Rotor, RotorCmd};

/// How long we swallow the same azimuth before publishing another GoTo
/// to the Rotor facade. 3 s is well below a typical antenna movement
/// (~2°/sec -> 6° missed worst-case) but absorbs
/// PstRotator's heartbeat broadcasts (~1 Hz when idle) and any
/// bidirectional "AAG repeat" that PstRotator sends
/// when it gets no status reply back.
const DEDUPE_INTERVAL: Duration = Duration::from_secs(3);

/// Read timeout on the UDP socket - determines how quickly we see the
/// shutdown flag when there is no traffic.
const READ_TIMEOUT: Duration = Duration::from_millis(500);

/// Loss tolerance for identical-azimuth dedupe (in tenths of a
/// degree). 0.1° = below the rotor's mechanical resolution.
const AZIMUTH_DEDUPE_EPSILON_X10: u16 = 1;

/// Tolerance within which an incoming GoTo is considered a feedback echo
/// of the current rotor position. PstRotator's "follow rotor"
/// / auto-track mode broadcasts the measured position as a new
/// goto stream (~3 Hz). Without a filter it overwrites every TL2
/// client target within ~300 ms (the orange target bearing disappears
/// from the rotor window). A GoTo to the current position is a
/// no-op for the hardware anyway, so dropping it costs no functionality.
/// 1.5° = well above the mechanical dead-band, well below the smallest
/// real user input (5° steps or more in the UI).
const FEEDBACK_TO_CURRENT_EPSILON_X10: u16 = 15;

/// Time window within which `AZ:nnn` text broadcasts after a real
/// `AG<nnn>` goto are treated as PstRotator simulator feedback
/// and silently dropped. Outside this window `AZ:nnn` is again accepted
/// as a goto (backwards-compat for setups that only broadcast AZ
/// without a Prosistel goto stream).
const AZ_FEEDBACK_AFTER_AG: Duration = Duration::from_secs(30);

/// Configuration for `spawn`. Contains only what the listener thread
/// needs to know; the Rotor facade arrives separately via `rotor`.
pub struct ListenConfig {
    /// UDP port we listen on. Operator choice; default 12001 is
    /// PstRotator's standard feedback port.
    pub port: u16,
}

/// Spawn the listener thread. Returns a shutdown handle that can be set
/// to `true` to let the thread exit cleanly on
/// server stop. The thread binds the UDP port itself; bind errors are
/// logged and return `Err` so the caller can decide whether
/// or not to continue without a listener.
pub fn spawn(config: ListenConfig, rotor: Rotor) -> Result<Arc<AtomicBool>, std::io::Error> {
    let bind_addr: SocketAddr = format!("0.0.0.0:{}", config.port)
        .parse()
        .map_err(|e: std::net::AddrParseError| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string())
        })?;
    // UDP and TCP share the same port - the OS sees them as different
    // protocols. PstRotator clients can choose which transport they
    // use; both threads run in parallel.
    let udp_sock = UdpSocket::bind(bind_addr)?;
    udp_sock.set_read_timeout(Some(READ_TIMEOUT))?;
    let tcp_listener = TcpListener::bind(bind_addr)?;
    tcp_listener.set_nonblocking(true)?;
    let shutdown = Arc::new(AtomicBool::new(false));

    let udp_shutdown = shutdown.clone();
    let udp_rotor = rotor.clone();
    let udp_port = config.port;
    std::thread::Builder::new()
        .name("pstrotator-listen-udp".to_string())
        .spawn(move || {
            run_udp(udp_sock, udp_rotor, udp_shutdown, udp_port);
        })?;

    let tcp_shutdown = shutdown.clone();
    let tcp_rotor = rotor.clone();
    let tcp_port = config.port;
    std::thread::Builder::new()
        .name("pstrotator-listen-tcp".to_string())
        .spawn(move || {
            run_tcp(tcp_listener, tcp_rotor, tcp_shutdown, tcp_port);
        })?;

    info!(
        "PstRotator listener listening on UDP+TCP {} (accepts Yaesu GS-232A `M<nnn>`/`C`, \
         Prosistel `AAG<nnn>`/`A?`, XML `<AZIMUTH>`; `AZ:nn` over UDP is ignored within 30s after \
         an AG-goto as a simulator broadcast)",
        config.port
    );
    Ok(shutdown)
}

fn run_udp(sock: UdpSocket, rotor: Rotor, shutdown: Arc<AtomicBool>, port: u16) {
    let mut buf = [0u8; 256];
    let mut last_az_x10: Option<u16> = None;
    let mut last_dispatch_at: Option<Instant> = None;
    let mut packet_count: u64 = 0;
    let mut parse_fail_count: u64 = 0;
    // Operator finding 2026-06-05: PstRotator broadcasts its own
    // simulator position as `AZ:nnn` in parallel with the real `AG<nnn>`
    // goto stream. Without distinction, every AZ overwrote the goto and
    // the needle wobbled step by step. Track when the last real
    // AG-goto arrived so we can drop AZ within the window
    // (simulator noise), but still accept it outside the window
    // (backwards-compat for AZ-only setups).
    let mut last_ag_at: Option<Instant> = None;
    while !shutdown.load(Ordering::Relaxed) {
        let (n, peer) = match sock.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => {
                warn!("PstRotator listener recv error on port {}: {}", port, e);
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        packet_count += 1;
        let payload = &buf[..n];
        // Diagnostics: log every incoming packet so we see what
        // PstRotator actually broadcasts (command vs status reply vs
        // simulator position). This stays until we have found the source of
        // the "target jumps away" issue; after that it can go back
        // to debug level.
        let raw_preview: String = payload
            .iter()
            .map(|b| {
                if (0x20..=0x7e).contains(b) {
                    (*b as char).to_string()
                } else {
                    format!("\\x{:02x}", b)
                }
            })
            .collect();
        debug!(
            "PstRotator listen RX from {} ({} bytes): {:?}",
            peer, n, raw_preview
        );
        let text = match std::str::from_utf8(payload) {
            Ok(s) => s,
            Err(_) => {
                parse_fail_count += 1;
                if parse_fail_count <= 3 || parse_fail_count.is_multiple_of(100) {
                    let preview: String = payload
                        .iter()
                        .take(40)
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join(" ");
                    warn!(
                        "PstRotator listener: non-UTF8 packet from {} ({} bytes, hex preview: {}), parse-fail #{}",
                        peer, n, preview, parse_fail_count
                    );
                }
                continue;
            }
        };
        let az_deg = match parse_packet(text) {
            Packet::GoTo(v) => {
                // Mark that we recently had a real AG-goto so that
                // AZ broadcasts within the feedback window are ignored.
                last_ag_at = Some(Instant::now());
                v
            }
            Packet::GoToAz(v) => {
                // Operator finding 2026-06-05: PstRotator's simulator
                // broadcasts `AZ:nnn` in parallel with the goto stream.
                // Within the feedback window after a real AG: drop
                // it as simulator noise. Outside it: treat as a goto
                // (AZ-only setups keep working).
                let within_window = last_ag_at
                    .map(|t| t.elapsed() < AZ_FEEDBACK_AFTER_AG)
                    .unwrap_or(false);
                if within_window {
                    debug!(
                        "PstRotator listen: AZ:{:.1}° from {} dropped - simulator broadcast within {}s of AG-goto",
                        v, peer, AZ_FEEDBACK_AFTER_AG.as_secs()
                    );
                    continue;
                }
                v
            }
            Packet::StatusQuery(proto) => {
                // Bidirectional: PstRotator polls regularly. Without a reply
                // it stays in polling mode and does not forward our goto
                // commands. The reply format must match the
                // protocol the query used.
                let status = rotor.status();
                let angle_int = (status.angle_x10 as f32 / 10.0).round() as u16;
                let reply = match proto {
                    QueryProtocol::Gs232C => format!("+{:03}\r", angle_int),
                    QueryProtocol::Gs232C2 => {
                        // GS-232A C2 reply: `+0aaa+0eee\r`; we have
                        // no elevation axis, so el=000.
                        format!("+0{:03}+0000\r", angle_int)
                    }
                    QueryProtocol::Prosistel => {
                        let rb = if status.rotating { 'B' } else { 'R' };
                        format!("\u{0002}A,?,{:03},{}\r", angle_int, rb)
                    }
                    QueryProtocol::PstXml => {
                        // PstRotator native reply: `AZ:nnn.n<CR>` - what
                        // Log4OM expects in the PstRotator-emulation path.
                        let angle_deg = status.angle_x10 as f32 / 10.0;
                        format!("AZ:{:.1}\r", angle_deg)
                    }
                };
                if let Err(e) = sock.send_to(reply.as_bytes(), peer) {
                    warn!(
                        "PstRotator listen: reply to {} failed: {}",
                        peer, e
                    );
                } else {
                    debug!(
                        "PstRotator listen: {:?} reply -> {} angle={}",
                        proto, peer, angle_int
                    );
                }
                continue;
            }
            Packet::Stop => {
                // CHANGELOG / Manual claim Stop support per protocol.
                // An external Stop (Yaesu `S\r`, Prosistel `\x02AR\r` /
                // `AG999`, PST-XML `<STOP>`) is passed through to the
                // rotor facade.
                info!("PstRotator listen: Stop from {} -> RotorCmd::Stop", peer);
                rotor.send_command(RotorCmd::Stop);
                continue;
            }
            Packet::ManualRotate => {
                info!("PstRotator listen: manual rotate R/L from {} (ignored - no continuous-rotate API)", peer);
                continue;
            }
            Packet::Elevation => {
                debug!("PstRotator listen: EL/elevation packet (ignored)");
                continue;
            }
            Packet::Metadata => {
                debug!(
                    "PstRotator listen: metadata-tag from {} (ignored): {:?}",
                    peer,
                    text.trim()
                );
                continue;
            }
            Packet::Unknown => {
                parse_fail_count += 1;
                if parse_fail_count <= 3 || parse_fail_count.is_multiple_of(100) {
                    warn!(
                        "PstRotator listener: unrecognised packet from {}: {:?} (parse-fail #{})",
                        peer,
                        text.trim(),
                        parse_fail_count
                    );
                }
                continue;
            }
        };
        // Compass azimuth (0..360°) -> mechanical rotor target. For
        // overlap rotors (`max_deg > 360`) there exist two mechanical
        // positions for compass 0..(max-360)° (X and X+360); pick the one
        // closest to the current rotor position. Operator scenario:
        // max_deg=450, so compass 0..90° can be at mech 0..90° or mech
        // 360..450°.
        let base_az_x10 = (az_deg.clamp(0.0, 360.0) * 10.0).round() as u16;
        let max_deg_x10 = rotor.max_deg_x10();
        let current_x10 = rotor.status().angle_x10;
        let az_x10 = pick_mechanical_target(base_az_x10, max_deg_x10, current_x10);
        let now = Instant::now();
        let is_duplicate = match (last_az_x10, last_dispatch_at) {
            (Some(prev), Some(t)) => {
                let diff = az_x10.abs_diff(prev);
                diff <= AZIMUTH_DEDUPE_EPSILON_X10 && now.duration_since(t) < DEDUPE_INTERVAL
            }
            _ => false,
        };
        if is_duplicate {
            debug!(
                "PstRotator listener: deduped {:.1}° (same as last within {:?})",
                az_deg, DEDUPE_INTERVAL
            );
            continue;
        }
        // Auto-track feedback filter: an incoming GoTo whose
        // mech target is within FEEDBACK_TO_CURRENT_EPSILON_X10 of
        // the current rotor position is a no-op echo from an external
        // controller following the rotor (PstRotator "follow rotor"
        // mode). Without a filter that echo overwrites every fresh
        // TL2 client target within ~300 ms.
        if az_x10.abs_diff(current_x10) <= FEEDBACK_TO_CURRENT_EPSILON_X10 {
            debug!(
                "PstRotator listen UDP: dropped feedback-echo {:.1}° (current {:.1}°) from {}",
                az_x10 as f32 / 10.0,
                current_x10 as f32 / 10.0,
                peer
            );
            continue;
        }
        // Dispatch GoTo via the Rotor facade. `send_command` swallows the
        // channel-closed result itself, so we cannot detect
        // that the cmd does not arrive - on a channel disconnect the
        // rotor backend would already be down anyway. The info log below
        // documents the attempt.
        rotor.send_command(RotorCmd::GoTo(az_x10));
        last_az_x10 = Some(az_x10);
        last_dispatch_at = Some(now);
        info!(
            "PstRotator listen: compass {:.1}° -> mech {:.1}° from {} (cur={:.1}°, max={:.1}°, packets={}, parse-fail={})",
            az_deg,
            az_x10 as f32 / 10.0,
            peer,
            current_x10 as f32 / 10.0,
            max_deg_x10 as f32 / 10.0,
            packet_count,
            parse_fail_count
        );
    }
    info!(
        "PstRotator listener stopping (port {}, total packets={}, parse-fail={})",
        port, packet_count, parse_fail_count
    );
}

/// TCP listener accepts connections from PstRotator TCP clients and
/// spawns a handler thread per client that reads line-delimited commands,
/// parses them, and dispatches GoTo's. Connections stay persistent
/// until the client disconnects.
fn run_tcp(listener: TcpListener, rotor: Rotor, shutdown: Arc<AtomicBool>, port: u16) {
    info!("PstRotator TCP listener active on port {}", port);
    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, peer)) => {
                info!("PstRotator TCP client connected from {}", peer);
                let r = rotor.clone();
                let s = shutdown.clone();
                std::thread::Builder::new()
                    .name(format!("pstrotator-tcp-{}", peer))
                    .spawn(move || {
                        handle_tcp_client(stream, r, s, peer);
                    })
                    .ok();
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                warn!("PstRotator TCP accept error: {}", e);
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
    info!("PstRotator TCP listener stopping (port {})", port);
}

fn handle_tcp_client(
    stream: TcpStream,
    rotor: Rotor,
    shutdown: Arc<AtomicBool>,
    peer: SocketAddr,
) {
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .ok();
    let writer = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            warn!("PstRotator TCP: cannot clone stream from {}: {}", peer, e);
            return;
        }
    };
    let mut writer = writer;
    let reader = BufReader::new(stream);
    let mut packet_count: u64 = 0;
    let mut parse_fail_count: u64 = 0;
    // Target-sync state: remember which target PstRotator itself
    // sent (M/AG) and which target we last pushed to
    // PstRotator. When rotor.target_x10 changes
    // to a value that is neither, the GoTo came from
    // TL2 (server UI or TCI client) and we push it so that
    // PstRotator's compass also shows the new target.
    let mut last_received_target_x10: Option<u16> = None;
    let mut last_pushed_target_x10: Option<u16> = None;
    // Detect which protocol PstRotator uses so the push has the
    // matching format. Default Prosistel (operator's EA7HG mode
    // is the most common) until we hear otherwise via incoming
    // commands or queries.
    let mut peer_protocol: ProtocolKind = ProtocolKind::Prosistel;
    // PstRotator typically sends `\r`-terminated commands. BufRead::lines()
    // splits only on `\n` so we read byte-by-byte and split ourselves on
    // CR or LF to support both formats.
    let mut buf = Vec::with_capacity(64);
    let mut byte = [0u8; 1];
    use std::io::Read;
    let mut reader = reader;
    while !shutdown.load(Ordering::Relaxed) {
        match reader.read(&mut byte) {
            Ok(0) => {
                info!("PstRotator TCP client {} closed connection", peer);
                break;
            }
            Ok(_) => {
                if byte[0] == b'\r' || byte[0] == b'\n' {
                    if buf.is_empty() {
                        continue;
                    }
                    let raw_preview: String = buf
                        .iter()
                        .map(|b| {
                            if (0x20..=0x7e).contains(b) {
                                (*b as char).to_string()
                            } else {
                                format!("\\x{:02x}", b)
                            }
                        })
                        .collect();
                    packet_count += 1;
                    debug!(
                        "PstRotator TCP RX from {} ({} bytes): {:?}",
                        peer,
                        buf.len(),
                        raw_preview
                    );
                    let text = match std::str::from_utf8(&buf) {
                        Ok(s) => s.to_string(),
                        Err(_) => {
                            parse_fail_count += 1;
                            warn!(
                                "PstRotator TCP: non-UTF8 line from {} (parse-fail #{})",
                                peer, parse_fail_count
                            );
                            buf.clear();
                            continue;
                        }
                    };
                    buf.clear();
                    match parse_packet(&text) {
                        Packet::GoTo(v) | Packet::GoToAz(v) => {
                            // TCP is connection-oriented, no simulator
                            // broadcasts; always treat AZ over TCP as
                            // a real goto (unlike the UDP path).
                            let max_deg_x10 = rotor.max_deg_x10();
                            let current_x10 = rotor.status().angle_x10;
                            let base_x10 = (v.clamp(0.0, 360.0) * 10.0).round() as u16;
                            let chosen = pick_mechanical_target(base_x10, max_deg_x10, current_x10);
                            // Auto-track feedback filter (see UDP path);
                            // PstRotator's "follow rotor" / auto-track
                            // mode echoes the measured position as a new
                            // goto and can thereby overwrite a fresh
                            // TL2 client target within ~300 ms.
                            if chosen.abs_diff(current_x10) <= FEEDBACK_TO_CURRENT_EPSILON_X10 {
                                debug!(
                                    "PstRotator TCP: dropped feedback-echo {:.1}° (current {:.1}°) from {}",
                                    chosen as f32 / 10.0,
                                    current_x10 as f32 / 10.0,
                                    peer
                                );
                                if let Some(k) = protocol_kind_of_goto_text(&text) {
                                    peer_protocol = k;
                                }
                                continue;
                            }
                            info!(
                                "PstRotator TCP: GoTo compass {:.1}° -> mech {:.1}° from {} (cur={:.1}°)",
                                v,
                                chosen as f32 / 10.0,
                                peer,
                                current_x10 as f32 / 10.0
                            );
                            rotor.send_command(RotorCmd::GoTo(chosen));
                            // Remember for target-sync: this GoTo
                            // came from PstRotator itself, so do not
                            // push back M<target> when the rotor
                            // status later shows this value.
                            last_received_target_x10 = Some(chosen);
                            // Protocol detection from incoming goto
                            // (Prosistel AAG/AG vs GS-232 M).
                            if let Some(k) = protocol_kind_of_goto_text(&text) {
                                peer_protocol = k;
                            }
                        }
                        Packet::StatusQuery(proto) => {
                            peer_protocol = protocol_kind_of_query(proto);
                            let status = rotor.status();
                            let angle_int = (status.angle_x10 as f32 / 10.0).round() as u16;
                            let reply = match proto {
                                QueryProtocol::Gs232C => format!("+{:03}\r", angle_int),
                                QueryProtocol::Gs232C2 => format!("+0{:03}+0000\r", angle_int),
                                QueryProtocol::Prosistel => {
                                    let rb = if status.rotating { 'B' } else { 'R' };
                                    format!("\u{0002}A,?,{:03},{}\r", angle_int, rb)
                                }
                                QueryProtocol::PstXml => {
                                    let angle_deg = status.angle_x10 as f32 / 10.0;
                                    format!("AZ:{:.1}\r", angle_deg)
                                }
                            };
                            if let Err(e) = writer.write_all(reply.as_bytes()) {
                                warn!(
                                    "PstRotator TCP: reply to {} failed: {}",
                                    peer, e
                                );
                                break;
                            }
                        }
                        Packet::Stop => {
                            // CHANGELOG / Manual claim Stop support
                            // - same semantics as the UDP path.
                            info!("PstRotator TCP: Stop from {} -> RotorCmd::Stop", peer);
                            rotor.send_command(RotorCmd::Stop);
                        }
                        Packet::ManualRotate => {
                            info!("PstRotator TCP: manual rotate from {} (ignored)", peer);
                        }
                        Packet::Elevation => {
                            debug!("PstRotator TCP: elevation packet from {} (ignored)", peer);
                        }
                        Packet::Metadata => {
                            debug!(
                                "PstRotator TCP: metadata-tag from {} (ignored): {:?}",
                                peer,
                                text.trim()
                            );
                        }
                        Packet::Unknown => {
                            parse_fail_count += 1;
                            warn!(
                                "PstRotator TCP: unrecognised packet from {}: {:?} (parse-fail #{})",
                                peer, text.trim(), parse_fail_count
                            );
                        }
                    }
                } else {
                    buf.push(byte[0]);
                    if buf.len() > 256 {
                        warn!(
                            "PstRotator TCP: oversized line from {} (>{} bytes), resetting buffer",
                            peer, 256
                        );
                        buf.clear();
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // Idle moment - use this to push TL2-originated targets
                // through to PstRotator so that its compass
                // also sees the target pointer (otherwise it would only
                // follow the current position via its polling).
                let cur_target_x10 = rotor.status().target_x10;
                if cur_target_x10 != 0
                    && Some(cur_target_x10) != last_received_target_x10
                    && Some(cur_target_x10) != last_pushed_target_x10
                {
                    // New target that did not come from PstRotator itself
                    // and has not been pushed before. Translate to the
                    // protocol that PstRotator uses on this connection
                    // (detected from earlier queries /
                    // gotos; default Prosistel for EA7HG).
                    // 3 digits, modulo 360 for overlap rotors so that
                    // PstRotator's compass shows the compass azimuth,
                    // not the mech position.
                    let compass = ((cur_target_x10 as u32) / 10) % 360;
                    let push = match peer_protocol {
                        ProtocolKind::Prosistel => {
                            format!("\u{0002}AG{:03}\r", compass)
                        }
                        ProtocolKind::Gs232 => format!("M{:03}\r", compass),
                    };
                    if let Err(e) = writer.write_all(push.as_bytes()) {
                        warn!(
                            "PstRotator TCP: target push to {} failed: {}",
                            peer, e
                        );
                        break;
                    }
                    info!(
                        "PstRotator TCP: pushed TL2-origin target {}° -> {} via {:?} (mech {}°)",
                        compass,
                        peer,
                        peer_protocol,
                        cur_target_x10 / 10
                    );
                    last_pushed_target_x10 = Some(cur_target_x10);
                }
                continue;
            }
            Err(e) => {
                warn!("PstRotator TCP read error from {}: {}", peer, e);
                break;
            }
        }
    }
    info!(
        "PstRotator TCP client {} disconnected (packets={}, parse-fail={})",
        peer, packet_count, parse_fail_count
    );
}

/// Which protocol format does PstRotator use on this connection? Is
/// detected from incoming commands/queries and determines the format
/// of outgoing target pushes (TL2 -> PstRotator UI sync).
#[derive(Debug, PartialEq, Clone, Copy)]
enum ProtocolKind {
    /// Prosistel binary (EA7HG controller): goto = `\x02AG<nnn>\r`,
    /// query = `\x02A?\r` / `AA?\r`. Default for unknown clients.
    Prosistel,
    /// Yaesu GS-232A/B text: goto = `M<nnn>\r`, query = `C\r` / `C2\r`.
    Gs232,
}

/// Which protocol family the query packet used. Determines the
/// reply format so the sender can parse the string.
#[derive(Debug, PartialEq, Clone, Copy)]
enum QueryProtocol {
    /// Yaesu GS-232A `C\r` -> reply `+<nnn>\r`.
    Gs232C,
    /// Yaesu GS-232A `C2\r` -> reply `+0aaa+0eee\r`.
    Gs232C2,
    /// Prosistel `\x02A?\r` or `AA?\r` -> reply `\x02A,?,<nnn>,<R|B>\r`.
    Prosistel,
    /// PstRotator XML `<PST>AZ?</PST>` (Log4OM emulation path) -> reply
    /// `AZ:<nnn.n>\r`. Log4OM sends this to PstRotator's host/port;
    /// TL2 catches it as a drop-in replacement for PstRotator.
    PstXml,
}

/// Classification of a PstRotator packet.
#[derive(Debug, PartialEq)]
enum Packet {
    /// Real goto command from an AG/M stream (Prosistel binary or
    /// Yaesu GS-232A). Is always dispatched.
    GoTo(f32),
    /// Goto from an AZ:nnn text broadcast. Is also used by PstRotator
    /// for its own simulator position updates, so within
    /// `AZ_FEEDBACK_AFTER_AG` after a real AG-goto we treat this
    /// as feedback (silent drop). Outside it as a real goto (for
    /// AZ-only setups).
    GoToAz(f32),
    /// Status query; listener replies with the current rotor position in
    /// the corresponding reply format.
    StatusQuery(QueryProtocol),
    /// Stop command (`AAR`, `\x02AR\r`, `AG999` in the EA7HG variant, `S\r`
    /// in GS-232A, or `<STOP>` in PstRotator-XML). Is forwarded
    /// as `RotorCmd::Stop` so that an external controller can stop the
    /// rotor (CHANGELOG promises this per protocol).
    Stop,
    /// Manual rotate button (`R\r` / `L\r` in GS-232A). No target,
    /// we cannot meaningfully forward it - just log it.
    ManualRotate,
    /// Elevation reply / XML - skipped, no elevation axis.
    Elevation,
    /// Metadata tags that Log4OM sends alongside the azimuth: callsign,
    /// name, QTH, frequency, mode, etc. PstRotator uses them for
    /// its display; TL2 ignores them silently (no warn spam).
    /// Examples: `<PST><CALL>PA0XYZ</CALL></PST>`,
    /// `<PST><NAME>...</NAME></PST>`, `<PST><QTH>...</QTH></PST>`.
    Metadata,
    /// Unknown format.
    Unknown,
}

/// Parse a PstRotator packet. Accepts (in order):
/// 1. Prosistel binary single-A: `\x02A?\r` (query), `\x02AG<nnn>\r` (goto),
///    `\x02AR\r` (stop). STX optional.
/// 2. Prosistel binary double-A: `AA?` / `AAG<nnn>\r` / `AAR\r`
///    (EA7HG/PstRotator alternative encoding).
/// 3. Text reply format: `AZ:nnn.n\r` (PstRotator's reply broadcast).
/// 4. XML mode: `<PST><AZIMUTH>nnn.n</AZIMUTH>...</PST>`.
///
/// `EL:` / `<ELEVATION>` skip with `Elevation`. `AAG999` (park position)
/// skip with `Park` - no mapping.
fn parse_packet(text: &str) -> Packet {
    let stripped: &str = text.trim_start_matches('\u{0002}');
    let trimmed = stripped
        .trim()
        .trim_end_matches(|c: char| c == '\r' || c == '\n');
    // Yaesu GS-232A - text protocol, single-char commands.
    if trimmed == "C" {
        return Packet::StatusQuery(QueryProtocol::Gs232C);
    }
    if trimmed == "C2" {
        return Packet::StatusQuery(QueryProtocol::Gs232C2);
    }
    if trimmed == "S" {
        return Packet::Stop;
    }
    if trimmed == "R" || trimmed == "L" {
        return Packet::ManualRotate;
    }
    // GS-232A move: `M<nnn>` (3 digits, optionally with spaces).
    if let Some(rest) = trimmed.strip_prefix('M') {
        let digits: String = rest
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if !digits.is_empty() {
            if let Ok(n) = digits.parse::<u16>() {
                return Packet::GoTo(n as f32);
            }
        }
    }
    // Prosistel binary: strip 1-2 leading `A`'s; what remains begins
    // with the action letter (G/R/?). Operator's PstRotator sends `\x02A?\r`
    // (single-A), the existing rotor.rs backend sends `AA?` (double-A).
    let prosistel_rest = trimmed
        .strip_prefix("AA")
        .or_else(|| trimmed.strip_prefix(['A', 'a']));
    if let Some(rest) = prosistel_rest {
        if let Some(digits) = rest.strip_prefix(['G', 'g']) {
            let digits: String = digits.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                if let Ok(n) = digits.parse::<u16>() {
                    if n == 999 {
                        // Operator finding (2026-06-05): in EA7HG-UDP mode
                        // PstRotator sends `AG999` as a STOP signal, not
                        // as "park to 999°". Classify as Stop.
                        return Packet::Stop;
                    }
                    return Packet::GoTo(n as f32);
                }
            }
        }
        if rest.starts_with(['R', 'r']) {
            return Packet::Stop;
        }
        if rest.starts_with('?') {
            return Packet::StatusQuery(QueryProtocol::Prosistel);
        }
    }
    // Text format: AZ:nnn.n (case-insensitive). Treat this as a GoTo command;
    // feedback-only handling is intentionally limited to the explicit feedback
    // packet forms above so simulator streams keep their legacy behaviour.
    if let Some(rest) = strip_prefix_ci(trimmed, "AZ:") {
        if let Ok(v) = rest.trim().parse::<f32>() {
            return Packet::GoToAz(v);
        }
    }
    if strip_prefix_ci(trimmed, "EL:").is_some() {
        return Packet::Elevation;
    }
    // XML format (PstRotator native, also used by Log4OM in the
    // PstRotator-emulation path):
    //   `<PST><AZIMUTH>nnn.n</AZIMUTH>...</PST>`   - goto
    //   `<PST>AZ?</PST>`                           - query
    //   `<PST><STOP>1</STOP></PST>`                - stop
    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("<pst>") && lower.contains("az?") {
        return Packet::StatusQuery(QueryProtocol::PstXml);
    }
    if lower.contains("<stop>") {
        return Packet::Stop;
    }
    if let Some(open) = lower.find("<azimuth>") {
        let after = &trimmed[open + "<azimuth>".len()..];
        if let Some(close) = after.to_ascii_lowercase().find("</azimuth>") {
            if let Ok(v) = after[..close].trim().parse::<f32>() {
                return Packet::GoTo(v);
            }
        }
    }
    if lower.contains("<elevation>") {
        return Packet::Elevation;
    }
    // PstRotator-XML metadata tags (Log4OM sends these along with every
    // spot click): call sign, name, QTH, frequency, mode, country, etc.
    // Drop silently so they do not fill up "unrecognised packet" warns.
    if lower.contains("<pst>")
        && (lower.contains("<call>")
            || lower.contains("<name>")
            || lower.contains("<qth>")
            || lower.contains("<country>")
            || lower.contains("<frequency>")
            || lower.contains("<freq>")
            || lower.contains("<mode>")
            || lower.contains("<grid>")
            || lower.contains("<locator>")
            || lower.contains("<comment>")
            || lower.contains("<continent>"))
    {
        return Packet::Metadata;
    }
    Packet::Unknown
}

/// Backwards-compatible wrapper for the unit tests. Returns Some for
/// every kind of goto extraction (AG/M or AZ).
#[cfg(test)]
fn parse_azimuth(text: &str) -> Option<f32> {
    match parse_packet(text) {
        Packet::GoTo(v) | Packet::GoToAz(v) => Some(v),
        _ => None,
    }
}

/// Pick the mechanical target closest to `current` for a given
/// compass azimuth. When `max_deg_x10 > 3600` (overlap rotor such as Yaesu
/// G-1000DXC with max=450°) the compass azimuth `base` is also reachable
/// as `base + 3600` as long as it stays within max - pick the variant with
/// the shortest mechanical travel from `current`.
fn pick_mechanical_target(base_x10: u16, max_deg_x10: u16, current_x10: u16) -> u16 {
    let primary = base_x10.min(max_deg_x10);
    // Alternative only relevant if `base + 360°` also falls within range.
    if max_deg_x10 > 3600 && (base_x10 as u32) + 3600 <= max_deg_x10 as u32 {
        let alt = base_x10 + 3600;
        let dist_primary = (primary as i32 - current_x10 as i32).unsigned_abs();
        let dist_alt = (alt as i32 - current_x10 as i32).unsigned_abs();
        if dist_alt < dist_primary {
            return alt;
        }
    }
    primary
}

/// Mappers from QueryProtocol -> ProtocolKind for protocol detection
/// from incoming status queries.
fn protocol_kind_of_query(p: QueryProtocol) -> ProtocolKind {
    match p {
        QueryProtocol::Prosistel => ProtocolKind::Prosistel,
        QueryProtocol::Gs232C | QueryProtocol::Gs232C2 => ProtocolKind::Gs232,
        // PstXml uses an entirely different format; handled as a
        // Prosistel equivalent for target push (the PstRotator-native
        // command is `<PST><AZIMUTH>` but that is not a "controller
        // sends to rotor" use in Log4OM emulation).
        QueryProtocol::PstXml => ProtocolKind::Prosistel,
    }
}

/// Detect protocol from a parsed goto packet text. Prosistel
/// commands contain an `AA` / `\x02A` prefix, GS-232 starts with `M`.
/// When in doubt (AZ text) returns None - lets the caller keep its
/// default value.
fn protocol_kind_of_goto_text(text: &str) -> Option<ProtocolKind> {
    let s = text.trim_start_matches('\u{0002}').trim();
    if s.starts_with("AA") || s.starts_with('A') && (s.starts_with("AG") || s.starts_with("Ag")) {
        Some(ProtocolKind::Prosistel)
    } else if s.starts_with('M') || s.starts_with('m') {
        Some(ProtocolKind::Gs232)
    } else {
        None
    }
}

fn strip_prefix_ci<'a>(haystack: &'a str, prefix: &str) -> Option<&'a str> {
    if haystack.len() < prefix.len() {
        return None;
    }
    if haystack[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&haystack[prefix.len()..])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_format() {
        // AZ:nn -> GoTo (revert from build 8's feedback-only to the
        // operator-known-working state; see parse_packet comment).
        assert_eq!(parse_azimuth("AZ:123.4"), Some(123.4));
        assert_eq!(parse_azimuth("az:0.0\r"), Some(0.0));
        assert_eq!(parse_azimuth("  AZ:359.9  \r\n"), Some(359.9));
    }

    #[test]
    fn parses_xml_format() {
        assert_eq!(
            parse_azimuth("<PST><AZIMUTH>271.5</AZIMUTH></PST>"),
            Some(271.5)
        );
        assert_eq!(
            parse_azimuth("<pst><azimuth>0</azimuth></pst>"),
            Some(0.0)
        );
    }

    #[test]
    fn ignores_elevation() {
        assert_eq!(parse_azimuth("EL:45.2"), None);
        assert_eq!(parse_azimuth("<PST><ELEVATION>30</ELEVATION></PST>"), None);
    }

    #[test]
    fn ignores_junk() {
        assert_eq!(parse_azimuth("hello world"), None);
        assert_eq!(parse_azimuth("AZIMUTH 90"), None);
        assert_eq!(parse_azimuth(""), None);
    }

    #[test]
    fn parses_prosistel_aag() {
        // PstRotator EA7HG UDP mode sends `AAG<nnn>\r`.
        assert_eq!(parse_azimuth("AAG090\r"), Some(90.0));
        assert_eq!(parse_azimuth("AAG000"), Some(0.0));
        assert_eq!(parse_azimuth("AAG359\r\n"), Some(359.0));
        assert_eq!(parse_azimuth("AAG270"), Some(270.0));
        // 4 digits supported for future rotors > 360
        assert_eq!(parse_azimuth("AAG0450"), Some(450.0));
    }

    #[test]
    fn ignores_prosistel_non_goto() {
        // AG999/AAG999 is classified as STOP (Packet::Stop)
        // - operator finding 2026-06-05. parse_azimuth returns None
        // because it is not a GoTo, regardless of whether it is Stop or Park.
        assert_eq!(parse_azimuth("AAG999"), None);
        // Stop and query are skipped.
        assert_eq!(parse_azimuth("AAR\r"), None);
        assert_eq!(parse_azimuth("AA?\r"), None);
    }

    #[test]
    fn picks_overlap_when_closer() {
        // Yaesu G-1000DXC with max=450°.
        // Compass 30° (base=300) - primary mech target = 300, alt = 3900.
        // At current position mech 350° (3500): alt 3900 is 400 away, primary
        // is 3200 away -> pick alt (overlap route).
        assert_eq!(pick_mechanical_target(300, 4500, 3500), 3900);
        // From mech 0° (0): primary 300 (distance 300), alt 3900 (distance 3900) -> primary.
        assert_eq!(pick_mechanical_target(300, 4500, 0), 300);
        // Compass 91° (base=910): no alt possible (910+3600=4510 > 4500) -> primary.
        assert_eq!(pick_mechanical_target(910, 4500, 4400), 910);
    }

    #[test]
    fn no_overlap_for_360_rotors() {
        // max_deg=360 -> no alternative possible, always primary.
        assert_eq!(pick_mechanical_target(300, 3600, 100), 300);
        assert_eq!(pick_mechanical_target(0, 3600, 3500), 0);
    }

    #[test]
    fn handles_stx_prefix() {
        // Prosistel replies sometimes come with an STX(0x02) prefix.
        assert_eq!(parse_azimuth("\u{0002}AAG180\r"), Some(180.0));
    }

    #[test]
    fn classifies_status_query() {
        // EA7HG/Prosistel single-A query with STX prefix.
        assert_eq!(parse_packet("\u{0002}A?\r"), Packet::StatusQuery(QueryProtocol::Prosistel));
        // Double-A variant.
        assert_eq!(parse_packet("AA?\r"), Packet::StatusQuery(QueryProtocol::Prosistel));
        assert_eq!(parse_packet("\u{0002}AA?\r"), Packet::StatusQuery(QueryProtocol::Prosistel));
    }

    #[test]
    fn classifies_gs232a_protocol() {
        // Yaesu GS-232A commands.
        assert_eq!(parse_packet("M090\r"), Packet::GoTo(90.0));
        assert_eq!(parse_packet("M000"), Packet::GoTo(0.0));
        assert_eq!(parse_packet("M450"), Packet::GoTo(450.0));
        assert_eq!(parse_packet("S\r"), Packet::Stop);
        assert_eq!(parse_packet("C\r"), Packet::StatusQuery(QueryProtocol::Gs232C));
        assert_eq!(parse_packet("C2\r"), Packet::StatusQuery(QueryProtocol::Gs232C2));
        assert_eq!(parse_packet("R\r"), Packet::ManualRotate);
        assert_eq!(parse_packet("L\r"), Packet::ManualRotate);
    }

    #[test]
    fn classifies_stop_and_goto_single_a() {
        // Single-A variants.
        assert_eq!(parse_packet("\u{0002}AR\r"), Packet::Stop);
        assert_eq!(parse_packet("\u{0002}AG090\r"), Packet::GoTo(90.0));
        // Double-A variants.
        assert_eq!(parse_packet("AAR\r"), Packet::Stop);
        assert_eq!(parse_packet("AAG090\r"), Packet::GoTo(90.0));
    }

    #[test]
    fn classifies_other_packet_kinds() {
        // Operator finding 2026-06-05: AG999 = STOP signal in PstRotator
        // EA7HG-UDP, not "park to 999°".
        assert_eq!(parse_packet("AAG999"), Packet::Stop);
        assert_eq!(parse_packet("\u{0002}AG999\r"), Packet::Stop);
        assert_eq!(parse_packet("EL:45.0"), Packet::Elevation);
        assert_eq!(parse_packet("hello"), Packet::Unknown);
    }
}
