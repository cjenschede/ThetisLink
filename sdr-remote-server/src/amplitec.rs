// SPDX-License-Identifier: GPL-2.0-or-later

use std::io::{Read, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use log::{info, warn};

/// Amplitec 6/2 antenna switch USB serial controller.
/// Communicates via 19200 baud, 12-byte binary commands.
pub struct AmplitecSwitch {
    cmd_tx: mpsc::Sender<AmplitecCmd>,
    status: Arc<Mutex<AmplitecStatus>>,
}

#[derive(Clone, Debug)]
pub struct AmplitecStatus {
    pub switch_a: u8,  // 0=unknown, 1-6
    pub switch_b: u8,
    pub connected: bool,
}

impl Default for AmplitecStatus {
    fn default() -> Self {
        Self { switch_a: 0, switch_b: 0, connected: false }
    }
}

pub enum AmplitecCmd {
    SetSwitchA(u8),  // 1-6
    SetSwitchB(u8),  // 1-6
    Query,
}

// 12-byte command lookup tables (from Amplitec PC Control Pro 2 Node.js source)
const CMD_SCAN: [u8; 12] = [0x01, 0x0A, 0x00, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00];

// Command bytes from original Amplitec PC Control Pro 2 Node.js source
const CMD_SWITCH_A: [[u8; 12]; 6] = [
    [0x01, 0x0a, 0x01, 0x82, 0x01, 0x20, 0x80, 0x06, 0x00, 0x00, 0x00, 0x00], // A1
    [0x01, 0x0a, 0x02, 0x82, 0x02, 0x20, 0x80, 0x06, 0x00, 0x00, 0x00, 0x00], // A2
    [0x01, 0x0a, 0x04, 0x82, 0x03, 0x01, 0x80, 0x01, 0x00, 0x00, 0x00, 0x00], // A3
    [0x01, 0x0a, 0x08, 0x82, 0x04, 0x01, 0x80, 0x01, 0x00, 0x00, 0x00, 0x00], // A4
    [0x01, 0x0a, 0x10, 0x82, 0x05, 0x20, 0x80, 0x06, 0x00, 0x00, 0x00, 0x00], // A5
    [0x01, 0x0a, 0x20, 0x02, 0x06, 0x20, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00], // A6
];

const CMD_SWITCH_B: [[u8; 12]; 6] = [
    [0x01, 0x0a, 0x20, 0x80, 0x06, 0x01, 0x82, 0x01, 0x00, 0x00, 0x00, 0x00], // B1
    [0x01, 0x0a, 0x20, 0x00, 0x06, 0x02, 0x02, 0x02, 0x00, 0x00, 0x00, 0x00], // B2
    [0x01, 0x0a, 0x20, 0x80, 0x06, 0x04, 0x82, 0x03, 0x00, 0x00, 0x00, 0x00], // B3
    [0x01, 0x0a, 0x20, 0x80, 0x06, 0x08, 0x82, 0x04, 0x00, 0x00, 0x00, 0x00], // B4
    [0x01, 0x0a, 0x20, 0x80, 0x06, 0x10, 0x82, 0x05, 0x00, 0x00, 0x00, 0x00], // B5
    [0x01, 0x0a, 0x08, 0x80, 0x04, 0x20, 0x82, 0x06, 0x00, 0x00, 0x00, 0x00], // B6
];

impl AmplitecSwitch {
    /// Open serial port and start background thread.
    pub fn new(port_name: &str) -> Result<Self, String> {
        let port = serialport::new(port_name, 19200)
            .data_bits(serialport::DataBits::Eight)
            .parity(serialport::Parity::None)
            .stop_bits(serialport::StopBits::One)
            .timeout(Duration::from_millis(500))
            .open()
            .map_err(|e| format!("Failed to open {}: {}", port_name, e))?;

        let (cmd_tx, cmd_rx) = mpsc::channel::<AmplitecCmd>();
        let status = Arc::new(Mutex::new(AmplitecStatus::default()));

        let status_for_thread = status.clone();
        let port_name_owned = port_name.to_string();

        std::thread::Builder::new()
            .name("amplitec-serial".to_string())
            .spawn(move || {
                amplitec_thread(port, cmd_rx, status_for_thread, &port_name_owned);
            })
            .map_err(|e| format!("Failed to spawn amplitec thread: {}", e))?;

        // Initial query
        let _ = cmd_tx.send(AmplitecCmd::Query);

        Ok(Self { cmd_tx, status })
    }

    pub fn send_command(&self, cmd: AmplitecCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn status(&self) -> AmplitecStatus {
        self.status.lock().unwrap().clone()
    }
}

fn amplitec_thread(
    mut port: Box<dyn serialport::SerialPort>,
    cmd_rx: mpsc::Receiver<AmplitecCmd>,
    status: Arc<Mutex<AmplitecStatus>>,
    port_name: &str,
) {
    info!("Amplitec serial thread started on {}", port_name);
    {
        let mut s = status.lock().unwrap();
        s.connected = true;
    }

    // Initial scan
    if let Err(e) = send_and_scan(&mut port, &CMD_SCAN, &status) {
        warn!("Amplitec initial scan failed: {}", e);
    }

    loop {
        // Check for commands (non-blocking with short timeout)
        match cmd_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(AmplitecCmd::SetSwitchA(pos)) => {
                if pos >= 1 && pos <= 6 {
                    let cmd = CMD_SWITCH_A[(pos - 1) as usize];
                    if let Err(e) = send_and_scan(&mut port, &cmd, &status) {
                        warn!("Amplitec set A{} failed: {}", pos, e);
                        mark_disconnected(&status);
                        break;
                    }
                    info!("Amplitec: Switch A → {}", pos);
                }
            }
            Ok(AmplitecCmd::SetSwitchB(pos)) => {
                if pos >= 1 && pos <= 6 {
                    let cmd = CMD_SWITCH_B[(pos - 1) as usize];
                    if let Err(e) = send_and_scan(&mut port, &cmd, &status) {
                        warn!("Amplitec set B{} failed: {}", pos, e);
                        mark_disconnected(&status);
                        break;
                    }
                    info!("Amplitec: Switch B → {}", pos);
                }
            }
            Ok(AmplitecCmd::Query) => {
                if let Err(e) = send_and_scan(&mut port, &CMD_SCAN, &status) {
                    warn!("Amplitec query failed: {}", e);
                    mark_disconnected(&status);
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Periodic poll every 2s
                if let Err(e) = send_and_scan(&mut port, &CMD_SCAN, &status) {
                    warn!("Amplitec periodic poll failed: {}", e);
                    mark_disconnected(&status);
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                info!("Amplitec command channel closed, stopping");
                break;
            }
        }
    }

    mark_disconnected(&status);
    info!("Amplitec serial thread stopped");
}

fn mark_disconnected(status: &Arc<Mutex<AmplitecStatus>>) {
    let mut s = status.lock().unwrap();
    s.connected = false;
}

/// Send a command and read status response. All commands (including switch commands)
/// start with 0x01 0x0a and the device responds with current switch positions.
fn send_and_scan(
    port: &mut Box<dyn serialport::SerialPort>,
    cmd: &[u8; 12],
    status: &Arc<Mutex<AmplitecStatus>>,
) -> Result<(), String> {
    // Send the command
    port.write_all(cmd).map_err(|e| format!("write: {}", e))?;
    port.flush().map_err(|e| format!("flush: {}", e))?;

    // Wait for device to process and respond
    std::thread::sleep(Duration::from_millis(200));
    let mut resp = [0u8; 256];
    let n = port.read(&mut resp).map_err(|e| format!("read: {}", e))?;

    if n == 0 {
        return Err("empty response".to_string());
    }

    // Response is hex string; chars at index 9 and 15 are switch A and B positions
    let hex_str: String = resp[..n].iter().map(|b| format!("{:02x}", b)).collect();

    if hex_str.len() >= 16 {
        let a_char = hex_str.chars().nth(9);
        let b_char = hex_str.chars().nth(15);

        let switch_a = a_char
            .and_then(|c| c.to_digit(16))
            .map(|d| d as u8)
            .unwrap_or(0);
        let switch_b = b_char
            .and_then(|c| c.to_digit(16))
            .map(|d| d as u8)
            .unwrap_or(0);

        let mut s = status.lock().unwrap();
        s.switch_a = switch_a;
        s.switch_b = switch_b;
        s.connected = true;
    }

    Ok(())
}

/// List available serial ports (for UI dropdown)
pub fn available_ports() -> Vec<String> {
    serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.port_name)
        .collect()
}
