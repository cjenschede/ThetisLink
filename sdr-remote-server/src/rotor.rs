#![allow(dead_code)]
use std::net::UdpSocket;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{debug, info, warn};

const STX: u8 = 0x02;
const CR: u8 = 0x0D;

/// Poll interval for position queries
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// How long without a response before we consider the rotor offline
const OFFLINE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug)]
pub struct RotorStatus {
    pub connected: bool,
    pub angle_x10: u16,     // 0-3600 (0.0°-360.0°)
    pub rotating: bool,     // true when rotor is turning (status B)
    pub target_x10: u16,    // target angle (0 = no target)
}

impl Default for RotorStatus {
    fn default() -> Self {
        Self {
            connected: false,
            angle_x10: 0,
            rotating: false,
            target_x10: 0,
        }
    }
}

pub enum RotorCmd {
    GoTo(u16),  // angle_x10
    Stop,
    Cw,
    Ccw,
}

pub struct Rotor {
    cmd_tx: mpsc::Sender<RotorCmd>,
    status: Arc<Mutex<RotorStatus>>,
}

pub fn status_labels_string(s: &RotorStatus) -> String {
    format!("{},{},{}", s.angle_x10, s.rotating as u8, s.target_x10)
}

impl Rotor {
    /// Connect to Visual Rotor via UDP and start background thread.
    pub fn new(addr: &str) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<RotorCmd>();
        let status = Arc::new(Mutex::new(RotorStatus::default()));

        let status_for_thread = status.clone();
        let addr_owned = addr.to_string();

        std::thread::Builder::new()
            .name("rotor-udp".to_string())
            .spawn(move || {
                rotor_thread(cmd_rx, status_for_thread, &addr_owned);
            })
            .expect("Failed to spawn rotor thread");

        Self { cmd_tx, status }
    }

    pub fn send_command(&self, cmd: RotorCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn status(&self) -> RotorStatus {
        self.status.lock().unwrap().clone()
    }
}

fn rotor_thread(
    cmd_rx: mpsc::Receiver<RotorCmd>,
    status: Arc<Mutex<RotorStatus>>,
    addr: &str,
) {
    info!("Rotor thread started (UDP), target: {}", addr);

    let remote: std::net::SocketAddr = match addr.parse() {
        Ok(a) => a,
        Err(e) => {
            warn!("Rotor: invalid address '{}': {}", addr, e);
            return;
        }
    };

    // Bind to any local port
    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(e) => {
            warn!("Rotor: failed to bind UDP socket: {}", e);
            return;
        }
    };

    // Set read timeout so recv doesn't block forever
    let _ = socket.set_read_timeout(Some(Duration::from_millis(50)));

    info!("Rotor: UDP socket bound, sending to {}", remote);

    let mut last_response = Instant::now() - OFFLINE_TIMEOUT;

    loop {
        // Send position query
        if let Err(e) = send_prosistel(&socket, &remote, "AA?") {
            warn!("Rotor: UDP send failed: {}", e);
        }

        // Read all available responses
        loop {
            match read_response(&socket) {
                Some(resp) => {
                    last_response = Instant::now();
                    let mut s = status.lock().unwrap();
                    s.connected = true;
                    drop(s);
                    parse_status_response(&resp, &status);
                }
                None => break,
            }
        }

        // Check if we've gone offline
        if last_response.elapsed() > OFFLINE_TIMEOUT {
            let mut s = status.lock().unwrap();
            if s.connected {
                info!("Rotor: no response for {}s, marking offline",
                    OFFLINE_TIMEOUT.as_secs());
                s.connected = false;
            }
        }

        // Check for commands (non-blocking with short timeout = poll interval)
        match cmd_rx.recv_timeout(POLL_INTERVAL) {
            Ok(cmd) => {
                handle_command(&socket, &remote, &cmd, &status);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Normal — continue polling
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                info!("Rotor: command channel closed, shutting down");
                return;
            }
        }
    }
}

fn handle_command(
    socket: &UdpSocket,
    remote: &std::net::SocketAddr,
    cmd: &RotorCmd,
    status: &Arc<Mutex<RotorStatus>>,
) {
    let result = match cmd {
        RotorCmd::GoTo(angle_x10) => {
            let angle = *angle_x10 / 10; // Prosistel uses integer degrees
            status.lock().unwrap().target_x10 = *angle_x10;
            send_prosistel(socket, remote, &format!("AAG{:03}", angle))
        }
        RotorCmd::Stop => {
            status.lock().unwrap().target_x10 = 0;
            send_prosistel(socket, remote, "AAG999")
        }
        RotorCmd::Cw => {
            let cur = status.lock().unwrap().angle_x10;
            let target = std::cmp::min(cur + 50, 3600); // +5°
            status.lock().unwrap().target_x10 = target;
            send_prosistel(socket, remote, &format!("AAG{:03}", target / 10))
        }
        RotorCmd::Ccw => {
            let cur = status.lock().unwrap().angle_x10;
            let target = cur.saturating_sub(50); // -5°
            status.lock().unwrap().target_x10 = target;
            send_prosistel(socket, remote, &format!("AAG{:03}", target / 10))
        }
    };
    if let Err(e) = result {
        warn!("Rotor: send command failed: {}", e);
    }
}

/// Send a Prosistel protocol frame via UDP: command + CR (no STX prefix)
fn send_prosistel(socket: &UdpSocket, remote: &std::net::SocketAddr, cmd: &str) -> Result<(), String> {
    let mut frame = Vec::with_capacity(cmd.len() + 1);
    frame.extend_from_slice(cmd.as_bytes());
    frame.push(CR);
    socket.send_to(&frame, remote).map_err(|e| format!("UDP send: {}", e))?;
    Ok(())
}

/// Read a response frame from the UDP socket.
/// Response format: STX A,?,<angle>,<status> CR
fn read_response(socket: &UdpSocket) -> Option<String> {
    let mut buf = [0u8; 128];
    match socket.recv(&mut buf) {
        Ok(n) if n > 0 => {
            let data = &buf[..n];
            // Find STX..CR frame
            let start = data.iter().position(|&b| b == STX)?;
            let end = data[start..].iter().position(|&b| b == CR)?;
            let payload = &data[start + 1..start + end];
            let s = String::from_utf8(payload.to_vec()).ok()?;
            debug!("Rotor: recv: {:?}", s);
            Some(s)
        }
        _ => None,
    }
}

/// Parse Prosistel response: "A,?,<angle>,<status>"
fn parse_status_response(resp: &str, status: &Arc<Mutex<RotorStatus>>) {
    // Expected: "A,?,<angle>,<status>" where status is R (ready) or B (busy)
    let parts: Vec<&str> = resp.split(',').collect();
    if parts.len() >= 4 {
        if let Ok(angle) = parts[2].trim().parse::<f32>() {
            let mut s = status.lock().unwrap();
            s.angle_x10 = (angle * 10.0).round() as u16;
            s.rotating = parts[3].trim() == "B";
            // Clear target when arrived (not rotating anymore)
            if !s.rotating {
                s.target_x10 = 0;
            }
        }
    } else {
        debug!("Rotor: unexpected response format: {:?}", resp);
    }
}

fn cmd_name(cmd: &RotorCmd) -> &'static str {
    match cmd {
        RotorCmd::GoTo(_) => "GoTo",
        RotorCmd::Stop => "Stop",
        RotorCmd::Cw => "CW",
        RotorCmd::Ccw => "CCW",
    }
}
