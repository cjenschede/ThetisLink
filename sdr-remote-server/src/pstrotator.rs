// SPDX-License-Identifier: GPL-2.0-or-later

//! PstRotator (YO3DMU) backend for the rotor module. Alternative to the
//! EA7HG Visual-Rotor / Prosistel path in `rotor.rs`. Talks PstRotator's
//! XML UDP protocol:
//!
//! - Outbound (server -> PstRotator on `host:port`, default `:12000`):
//!   - `<PST><AZIMUTH>123.4</AZIMUTH></PST>` - go-to bearing
//!   - `<PST><STOP>1</STOP></PST>`         - stop
//!   - `<PST>AZ?</PST>` / `<PST>EL?</PST>` - position queries
//! - Inbound (PstRotator -> server on `feedback_port`, default `:12001`):
//!   - `AZ:271.5<CR>` / `EL:45.2<CR>` - position replies
//!   - `OK:STOP:1<CR>`, `OK:HOME:1<CR>` - command ACKs (ignored)
//!
//! Source: PstRotator User Manual rev 7.5, pp. 10-12.
//!
//! One UDP socket bound on `feedback_port` doubles as both sender and
//! receiver. PstRotator replies to `listener_port + 1` regardless of the
//! source port - so by binding on `feedback_port = 12001` we both
//! receive replies and send queries from the same socket. The user is
//! expected to allow inbound UDP `feedback_port` in the local firewall.

use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{debug, info, warn};

use crate::rotor::{RotorCmd, RotorStatus};

/// How often we send `AZ?` / `EL?` queries.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// After this many seconds without a reply we mark the rotor offline.
const OFFLINE_TIMEOUT: Duration = Duration::from_secs(5);

/// Reached-target tolerance - when |current − target| < this, we clear
/// `target_x10` and `rotating`. PstRotator only ack's a position-set
/// indirectly (next AZ?/EL? reply); we infer arrival.
const ARRIVAL_TOLERANCE_DEG_X10: u16 = 10; // 1.0°

pub struct PstRotatorConfig {
    /// Numeric IP address of the PstRotator PC. Hostnames are not
    /// resolved - the worker parses `host:port` as a `SocketAddr`,
    /// which only accepts numeric literals.
    pub host: String,
    /// UDP port PstRotator listens on for `<PST>...</PST>` datagrams (default 12000).
    pub port: u16,
    /// Local UDP port we bind for receiving `AZ:`/`EL:` replies (default 12001).
    pub feedback_port: u16,
    /// Whether to poll EL? alongside AZ?. Set `false` for AZ-only rotors.
    pub has_elevation: bool,
}

/// Spawn the PstRotator worker thread. Returns the receiver-end of the
/// `RotorCmd` channel and the shared `RotorStatus` so the caller can
/// wrap them in the existing `Rotor` facade.
pub fn spawn(
    config: PstRotatorConfig,
) -> (mpsc::Sender<RotorCmd>, Arc<Mutex<RotorStatus>>) {
    let (cmd_tx, cmd_rx) = mpsc::channel::<RotorCmd>();
    let status = Arc::new(Mutex::new(RotorStatus::default()));

    let status_for_thread = status.clone();
    std::thread::Builder::new()
        .name("pstrotator-udp".to_string())
        .spawn(move || {
            pstrotator_thread(cmd_rx, status_for_thread, config);
        })
        .expect("Failed to spawn pstrotator thread");

    (cmd_tx, status)
}

fn pstrotator_thread(
    cmd_rx: mpsc::Receiver<RotorCmd>,
    status: Arc<Mutex<RotorStatus>>,
    config: PstRotatorConfig,
) {
    info!(
        "PstRotator thread started: send->{}:{}, listen={}:{}, has_ele={}",
        config.host, config.port, "0.0.0.0", config.feedback_port, config.has_elevation
    );

    // Resolve destination once. PstRotator does DNS for free in most
    // setups but we keep the parse explicit so bad config fails loud.
    let remote: SocketAddr = match format!("{}:{}", config.host, config.port).parse() {
        Ok(a) => a,
        Err(e) => {
            warn!(
                "PstRotator: invalid host/port '{}:{}' ({}); thread exiting",
                config.host, config.port, e
            );
            return;
        }
    };

    let bind_addr = format!("0.0.0.0:{}", config.feedback_port);
    let socket = match UdpSocket::bind(&bind_addr) {
        Ok(s) => s,
        Err(e) => {
            warn!(
                "PstRotator: failed to bind feedback socket on {} ({}); thread exiting",
                bind_addr, e
            );
            return;
        }
    };
    let _ = socket.set_read_timeout(Some(Duration::from_millis(50)));

    let mut last_reply = Instant::now()
        .checked_sub(OFFLINE_TIMEOUT)
        .unwrap_or_else(Instant::now);

    loop {
        // Poll AZ (and optionally EL) once per loop iteration.
        send_xml(&socket, &remote, "AZ?");
        if config.has_elevation {
            send_xml(&socket, &remote, "EL?");
        }

        // Drain all pending replies on this socket.
        loop {
            match read_reply(&socket) {
                Some(line) => {
                    last_reply = Instant::now();
                    {
                        let mut s = status.lock().unwrap();
                        s.connected = true;
                    }
                    parse_reply(&line, &status);
                }
                None => break,
            }
        }

        // Mark offline if PstRotator went silent.
        if last_reply.elapsed() > OFFLINE_TIMEOUT {
            let mut s = status.lock().unwrap();
            if s.connected {
                info!(
                    "PstRotator: no reply for {}s, marking offline",
                    OFFLINE_TIMEOUT.as_secs()
                );
                s.connected = false;
            }
        }

        // Wait for the next command OR the poll interval, whichever comes
        // first. Matches the rhythm of `rotor.rs`.
        match cmd_rx.recv_timeout(POLL_INTERVAL) {
            Ok(cmd) => handle_command(&socket, &remote, &cmd, &status),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                info!("PstRotator: command channel closed, shutting down");
                return;
            }
        }
    }
}

fn handle_command(
    socket: &UdpSocket,
    remote: &SocketAddr,
    cmd: &RotorCmd,
    status: &Arc<Mutex<RotorStatus>>,
) {
    match cmd {
        RotorCmd::GoTo(angle_x10) => {
            // Always send integer degrees in the XML (same behaviour as the
            // EA7HG/Prosistel backend). Decimals caused some rotor drivers
            // in PstRotator to make a wrong short-path choice because they
            // use the difference with the current position (incl. the
            // decimal) for CW/CCW.
            status.lock().unwrap().target_x10 = *angle_x10;
            let deg = *angle_x10 / 10;
            info!("Rotor (PstRotator) GoTo {}°", deg);
            debug!("PstRotator GoTo: {} deg -> <PST><AZIMUTH>{}</AZIMUTH></PST>", deg, deg);
            send_xml(socket, remote, &format!("AZIMUTH>{}</AZIMUTH", deg));
            // `send_xml` wraps with <PST>...</PST>; we passed only the inner
            // element so the helper stays generic for STOP and AZ? too.
        }
        RotorCmd::Stop => {
            status.lock().unwrap().target_x10 = 0;
            send_xml(socket, remote, "STOP>1</STOP");
        }
        RotorCmd::Cw => {
            // PstRotator XML has no jog tag - emulate with relative AZIMUTH.
            let cur = status.lock().unwrap().angle_x10;
            let target = (cur + 50).min(3600); // +5°
            status.lock().unwrap().target_x10 = target;
            let deg = target / 10;
            send_xml(socket, remote, &format!("AZIMUTH>{}</AZIMUTH", deg));
        }
        RotorCmd::Ccw => {
            let cur = status.lock().unwrap().angle_x10;
            let target = cur.saturating_sub(50); // -5°
            status.lock().unwrap().target_x10 = target;
            let deg = target / 10;
            send_xml(socket, remote, &format!("AZIMUTH>{}</AZIMUTH", deg));
        }
    }
}

/// Wrap an inner-XML fragment with `<PST>...</PST>` and send as a single
/// UDP datagram. Examples: `"AZ?"` -> `<PST>AZ?</PST>`,
/// `"STOP>1</STOP"` -> `<PST><STOP>1</STOP></PST>`,
/// `"AZIMUTH>123.4</AZIMUTH"` -> `<PST><AZIMUTH>123.4</AZIMUTH></PST>`.
///
/// Queries (`AZ?` / `EL?`) are short tokens without their own element
/// wrapper; commands carry a closing tag in the inner fragment. PstRotator
/// accepts both forms inside `<PST>...</PST>`.
fn send_xml(socket: &UdpSocket, remote: &SocketAddr, inner: &str) {
    let is_query = !inner.contains('<') && !inner.contains('>');
    let frame = if is_query {
        format!("<PST>{}</PST>", inner)
    } else {
        format!("<PST><{}></PST>", inner)
    };
    debug!("PstRotator: send -> {}: {}", remote, frame);
    if let Err(e) = socket.send_to(frame.as_bytes(), remote) {
        warn!("PstRotator: UDP send failed: {}", e);
    }
}

/// Read one PstRotator reply line. Replies are `AZ:nnn.n<CR>`,
/// `EL:nn.n<CR>`, `OK:STOP:1<CR>`, etc. A single datagram may carry
/// more than one CR-terminated line.
fn read_reply(socket: &UdpSocket) -> Option<String> {
    let mut buf = [0u8; 256];
    match socket.recv(&mut buf) {
        Ok(n) if n > 0 => {
            let data = String::from_utf8_lossy(&buf[..n]).trim().to_string();
            if data.is_empty() {
                None
            } else {
                debug!("PstRotator: recv: {:?}", data);
                Some(data)
            }
        }
        _ => None,
    }
}

fn parse_reply(line: &str, status: &Arc<Mutex<RotorStatus>>) {
    // A datagram may carry multiple CR-separated entries; iterate.
    for entry in line.split('\r').map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(rest) = entry.strip_prefix("AZ:") {
            if let Ok(deg) = rest.trim().parse::<f32>() {
                let mut s = status.lock().unwrap();
                s.angle_x10 = (deg * 10.0).round().clamp(0.0, 3600.0) as u16;
                // Arrival heuristic - PstRotator has no busy/ready bit.
                if s.target_x10 != 0 {
                    let diff = (s.angle_x10 as i32 - s.target_x10 as i32).abs() as u16;
                    if diff < ARRIVAL_TOLERANCE_DEG_X10 {
                        s.target_x10 = 0;
                        s.rotating = false;
                    } else {
                        s.rotating = true;
                    }
                } else {
                    s.rotating = false;
                }
            }
        } else if let Some(rest) = entry.strip_prefix("EL:") {
            // Elevation comes through the same status field is RX-only at
            // present (no client-side ELE control yet). Logged for future
            // use but not stored in RotorStatus until we add an ele_x10.
            if let Ok(_deg) = rest.trim().parse::<f32>() {
                // intentionally nothing - TODO when elevation lands in RotorStatus
            }
        } else if entry.starts_with("OK:") {
            // Command ACK (OK:STOP:1, OK:HOME:1). No state change needed.
        } else {
            debug!("PstRotator: unrecognised reply: {:?}", entry);
        }
    }
}
