// SPDX-License-Identifier: GPL-2.0-or-later

use std::io::{Read, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use log::{debug, info, warn};

// Protocol constants
const STX: u8 = 0xF5;
const ETX: u8 = 0xFA;
const DLE: u8 = 0xF6;

// Commands
const CMD_STATUS: u8 = 1;
const CMD_RETRACT: u8 = 2;
const CMD_SET_FREQ: u8 = 3;
const CMD_READ_ELEMENTS: u8 = 9;
const CMD_MOTOR_PROGRESS: u8 = 10;
const CMD_MODIFY_ELEMENT: u8 = 12;

// Response codes
const UB_OK: u8 = 0;

/// Band center frequencies in kHz for preset buttons.
pub const BAND_PRESETS: [(u8, &str, u16); 8] = [
    (7,  "40m",  7100),
    (6,  "30m",  10125),
    (5,  "20m",  14175),
    (4,  "17m",  18118),
    (3,  "15m",  21225),
    (2,  "12m",  24940),
    (1,  "10m",  28500),
    (0,  "6m",   50150),
];

pub struct UltraBeam {
    cmd_tx: mpsc::Sender<UltraBeamCmd>,
    status: Arc<Mutex<UltraBeamStatus>>,
}

#[derive(Clone, Debug)]
pub struct UltraBeamStatus {
    pub connected: bool,
    pub fw_major: u8,
    pub fw_minor: u8,
    pub operation: u8,       // 0=normal, 2=user_adj, 3=setup
    pub frequency_khz: u16,
    pub band: u8,            // 0-10
    pub direction: u8,       // 0=normal, 1=180, 2=bidir
    pub off_state: bool,
    pub motors_moving: u8,   // bitfield
    pub freq_min_mhz: u8,
    pub freq_max_mhz: u8,
    pub elements_mm: [u16; 6],
    pub motor_distance_mm: u16,
    pub motor_completion: u16, // 0-60
}

impl Default for UltraBeamStatus {
    fn default() -> Self {
        Self {
            connected: false,
            fw_major: 0,
            fw_minor: 0,
            operation: 0,
            frequency_khz: 0,
            band: 0,
            direction: 0,
            off_state: true,
            motors_moving: 0,
            freq_min_mhz: 0,
            freq_max_mhz: 0,
            elements_mm: [0; 6],
            motor_distance_mm: 0,
            motor_completion: 0,
        }
    }
}

pub enum UltraBeamCmd {
    Retract,
    SetFrequency { khz: u16, direction: u8 },
    ReadElements,
    ModifyElement { index: u8, length_mm: u16 },
}

pub fn band_name(band: u8) -> &'static str {
    match band {
        0 => "6m",
        1 => "10m",
        2 => "12m",
        3 => "15m",
        4 => "17m",
        5 => "20m",
        6 => "30m",
        7 => "40m",
        8 => "60m",
        9 => "80m",
        10 => "160m",
        _ => "?",
    }
}

/// Build labels CSV for network broadcast (11 fields + elements).
/// Format: fw_major,fw_minor,operation,frequency_khz,band,direction,off_state,motors_moving,motor_distance_mm,motor_completion,elements(;-sep)
pub fn status_labels_string(s: &UltraBeamStatus) -> String {
    let elems: Vec<String> = s.elements_mm.iter().map(|e| e.to_string()).collect();
    format!(
        "{},{},{},{},{},{},{},{},{},{},{}",
        s.fw_major, s.fw_minor, s.operation, s.frequency_khz, s.band,
        s.direction, s.off_state as u8, s.motors_moving,
        s.motor_distance_mm, s.motor_completion,
        elems.join(";"),
    )
}

// --- Protocol layer ---

/// DLE-escape a single byte: if it's STX, ETX, or DLE, prefix with DLE.
fn quote_byte(byte: u8) -> Vec<u8> {
    if byte == STX || byte == ETX || byte == DLE {
        vec![DLE, byte]
    } else {
        vec![byte]
    }
}

/// Compute UltraBeam checksum over a slice of bytes.
/// Init 0x55, for each byte: chk = (chk ^ byte).wrapping_add(1)
fn compute_checksum(data: &[u8]) -> u8 {
    let mut chk: u8 = 0x55;
    for &b in data {
        chk = (chk ^ b).wrapping_add(1);
    }
    chk
}

/// Build a complete packet: STX + quoted(SEQ, COM, DAT..., CHK) + ETX
fn build_packet(seq: u8, com: u8, data: &[u8]) -> Vec<u8> {
    // Collect raw payload for checksum: SEQ + COM + DATA
    let mut raw = Vec::with_capacity(2 + data.len());
    raw.push(seq);
    raw.push(com);
    raw.extend_from_slice(data);
    let chk = compute_checksum(&raw);

    let mut pkt = Vec::with_capacity(4 + raw.len() * 2);
    pkt.push(STX);
    for &b in &raw {
        pkt.extend(quote_byte(b));
    }
    pkt.extend(quote_byte(chk));
    pkt.push(ETX);
    pkt
}

/// Read a complete packet from the serial port.
/// Returns (seq, com, data_bytes) or error.
/// Uses a state machine: wait for STX, collect DLE-unquoted bytes until ETX,
/// verify checksum.
fn read_packet(port: &mut Box<dyn serialport::SerialPort>) -> Result<(u8, u8, Vec<u8>), String> {
    let mut buf = [0u8; 1];
    let mut payload: Vec<u8> = Vec::with_capacity(128);
    let mut in_packet = false;
    let mut dle_pending = false;

    // Read with timeout — we have ~2s port timeout set
    loop {
        match port.read(&mut buf) {
            Ok(0) => return Err("EOF".to_string()),
            Ok(_) => {
                let b = buf[0];
                if !in_packet {
                    if b == STX {
                        in_packet = true;
                        payload.clear();
                        dle_pending = false;
                    }
                    continue;
                }
                // Inside packet
                if dle_pending {
                    // DLE-quoted byte — take as literal data
                    payload.push(b);
                    dle_pending = false;
                } else if b == DLE {
                    dle_pending = true;
                } else if b == ETX {
                    // End of packet — verify checksum
                    // Payload contains: SEQ, COM, DAT..., CHK
                    if payload.len() < 3 {
                        return Err("Packet too short".to_string());
                    }
                    // Verify: computing checksum over all bytes including CHK should yield 1
                    let verify = compute_checksum(&payload);
                    if verify != 1 {
                        return Err(format!("Checksum mismatch (got {})", verify));
                    }
                    let seq = payload[0];
                    let com = payload[1];
                    let data = payload[2..payload.len() - 1].to_vec(); // exclude CHK
                    return Ok((seq, com, data));
                } else if b == STX {
                    // Restart packet
                    payload.clear();
                    dle_pending = false;
                } else {
                    payload.push(b);
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                return Err("Timeout".to_string());
            }
            Err(e) => return Err(format!("Read error: {}", e)),
        }
    }
}

/// Send a packet and read the response.
fn send_and_receive(
    port: &mut Box<dyn serialport::SerialPort>,
    seq: u8,
    com: u8,
    data: &[u8],
) -> Result<(u8, u8, Vec<u8>), String> {
    let pkt = build_packet(seq, com, data);
    port.write_all(&pkt).map_err(|e| format!("Write error: {}", e))?;
    port.flush().map_err(|e| format!("Flush error: {}", e))?;
    read_packet(port)
}

/// Parse a CMD_STATUS response (12+ bytes).
fn parse_status(data: &[u8]) -> Result<UltraBeamStatus, String> {
    if data.len() < 12 {
        return Err(format!("Status response too short: {} bytes", data.len()));
    }
    let mut s = UltraBeamStatus::default();
    s.connected = true;
    s.fw_major = data[0];
    s.fw_minor = data[1];
    s.operation = data[2];
    s.frequency_khz = u16::from_le_bytes([data[3], data[4]]);
    s.band = data[5];
    s.direction = data[6];
    let flags = data[7];
    s.off_state = (flags & 0x01) != 0;
    s.motors_moving = data[8];
    s.freq_min_mhz = data[9];
    s.freq_max_mhz = data[10];
    // data[11] may be additional flags or padding
    Ok(s)
}

impl UltraBeam {
    /// Open serial port and start background thread.
    pub fn new(port_name: &str) -> Result<Self, String> {
        let port = serialport::new(port_name, 19200)
            .data_bits(serialport::DataBits::Eight)
            .parity(serialport::Parity::None)
            .stop_bits(serialport::StopBits::One)
            .flow_control(serialport::FlowControl::None)
            .timeout(Duration::from_millis(2000))
            .open()
            .map_err(|e| format!("Failed to open {}: {}", port_name, e))?;

        let (cmd_tx, cmd_rx) = mpsc::channel::<UltraBeamCmd>();
        let status = Arc::new(Mutex::new(UltraBeamStatus::default()));

        let status_for_thread = status.clone();
        let port_name_owned = port_name.to_string();

        std::thread::Builder::new()
            .name("ultrabeam-serial".to_string())
            .spawn(move || {
                ultrabeam_thread(port, cmd_rx, status_for_thread, &port_name_owned);
            })
            .map_err(|e| format!("Failed to spawn UltraBeam thread: {}", e))?;

        Ok(Self { cmd_tx, status })
    }

    pub fn send_command(&self, cmd: UltraBeamCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn status(&self) -> UltraBeamStatus {
        self.status.lock().unwrap().clone()
    }
}

fn ultrabeam_thread(
    mut port: Box<dyn serialport::SerialPort>,
    cmd_rx: mpsc::Receiver<UltraBeamCmd>,
    status: Arc<Mutex<UltraBeamStatus>>,
    port_name: &str,
) {
    info!("UltraBeam serial thread started on {}", port_name);

    std::thread::sleep(Duration::from_millis(200));
    let _ = port.clear(serialport::ClearBuffer::Input);

    let mut seq: u8 = 0;
    let mut consecutive_failures: u32 = 0;

    // Initial status query
    match send_and_receive(&mut port, seq, CMD_STATUS, &[]) {
        Ok((_rseq, _rcom, data)) => {
            match parse_status(&data) {
                Ok(parsed) => {
                    let mut s = status.lock().unwrap();
                    *s = parsed;
                    s.connected = true;
                    info!("UltraBeam connected, FW {}.{}", s.fw_major, s.fw_minor);
                }
                Err(e) => warn!("UltraBeam initial parse failed: {}", e),
            }
        }
        Err(e) => {
            warn!("UltraBeam initial query failed: {}", e);
            consecutive_failures += 1;
        }
    }
    seq = seq.wrapping_add(1);

    loop {
        // Check for user commands (non-blocking with 500ms timeout for polling)
        match cmd_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(cmd) => {
                match cmd {
                    UltraBeamCmd::Retract => {
                        info!("UltraBeam: Retract");
                        match send_and_receive(&mut port, seq, CMD_RETRACT, &[]) {
                            Ok((_s, _c, data)) => {
                                if data.first() == Some(&UB_OK) {
                                    debug!("UltraBeam retract OK");
                                } else {
                                    warn!("UltraBeam retract response: {:?}", data);
                                }
                            }
                            Err(e) => warn!("UltraBeam retract failed: {}", e),
                        }
                        seq = seq.wrapping_add(1);
                    }
                    UltraBeamCmd::SetFrequency { mut khz, mut direction } => {
                        // Drain queue: skip intermediate SetFrequency, only send the last one
                        let mut skipped = 0u32;
                        while let Ok(next) = cmd_rx.try_recv() {
                            match next {
                                UltraBeamCmd::SetFrequency { khz: k, direction: d } => {
                                    khz = k;
                                    direction = d;
                                    skipped += 1;
                                }
                                _other => {
                                    // Non-freq command lost — acceptable trade-off vs queue explosion.
                                    // Retract/ReadElements/ModifyElement are rare during rapid stepping.
                                }
                            }
                        }
                        if skipped > 0 {
                            debug!("UltraBeam: skipped {} queued SetFrequency, sending {} kHz", skipped, khz);
                        }
                        let freq_lo = (khz & 0xFF) as u8;
                        let freq_hi = ((khz >> 8) & 0xFF) as u8;
                        let data = [freq_lo, freq_hi, direction];
                        debug!("UltraBeam: SetFrequency {} kHz, dir={}", khz, direction);
                        match send_and_receive(&mut port, seq, CMD_SET_FREQ, &data) {
                            Ok((_s, _c, resp)) => {
                                if resp.first() == Some(&UB_OK) {
                                    debug!("UltraBeam set freq OK");
                                } else {
                                    warn!("UltraBeam set freq response: {:?}", resp);
                                }
                            }
                            Err(e) => warn!("UltraBeam set freq failed: {}", e),
                        }
                        seq = seq.wrapping_add(1);
                    }
                    UltraBeamCmd::ReadElements => {
                        match send_and_receive(&mut port, seq, CMD_READ_ELEMENTS, &[]) {
                            Ok((_s, _c, data)) => {
                                if data.len() >= 12 {
                                    let mut elements = [0u16; 6];
                                    for i in 0..6 {
                                        elements[i] = u16::from_le_bytes([data[i * 2], data[i * 2 + 1]]);
                                    }
                                    let mut s = status.lock().unwrap();
                                    s.elements_mm = elements;
                                    debug!("UltraBeam elements: {:?}", elements);
                                } else {
                                    warn!("UltraBeam read elements: short response ({} bytes)", data.len());
                                }
                            }
                            Err(e) => warn!("UltraBeam read elements failed: {}", e),
                        }
                        seq = seq.wrapping_add(1);
                    }
                    UltraBeamCmd::ModifyElement { index, length_mm } => {
                        let len_lo = (length_mm & 0xFF) as u8;
                        let len_hi = ((length_mm >> 8) & 0xFF) as u8;
                        let data = [index, 0, len_lo, len_hi];
                        info!("UltraBeam: ModifyElement {} → {} mm", index, length_mm);
                        match send_and_receive(&mut port, seq, CMD_MODIFY_ELEMENT, &data) {
                            Ok((_s, _c, resp)) => {
                                if resp.first() == Some(&UB_OK) {
                                    debug!("UltraBeam modify element OK");
                                } else {
                                    warn!("UltraBeam modify element response: {:?}", resp);
                                }
                            }
                            Err(e) => warn!("UltraBeam modify element failed: {}", e),
                        }
                        seq = seq.wrapping_add(1);
                    }
                }
                // After any command, give the RCU time to process before polling status
                std::thread::sleep(Duration::from_millis(100));
                let _ = port.clear(serialport::ClearBuffer::Input);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Regular poll cycle — query status
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                info!("UltraBeam command channel closed, exiting");
                break;
            }
        }

        // Poll status
        match send_and_receive(&mut port, seq, CMD_STATUS, &[]) {
            Ok((_rseq, _rcom, data)) => {
                match parse_status(&data) {
                    Ok(parsed) => {
                        let mut s = status.lock().unwrap();
                        *s = parsed;
                        s.connected = true;
                        consecutive_failures = 0;
                    }
                    Err(e) => {
                        debug!("UltraBeam status parse error: {}", e);
                        consecutive_failures += 1;
                    }
                }
            }
            Err(e) => {
                debug!("UltraBeam status query failed: {}", e);
                consecutive_failures += 1;
                if consecutive_failures > 5 {
                    let mut s = status.lock().unwrap();
                    s.connected = false;
                }
            }
        }
        seq = seq.wrapping_add(1);

        // If motors are moving, also poll motor progress
        {
            let moving = status.lock().unwrap().motors_moving;
            if moving != 0 {
                match send_and_receive(&mut port, seq, CMD_MOTOR_PROGRESS, &[]) {
                    Ok((_s, _c, data)) => {
                        if data.len() >= 4 {
                            let dist = u16::from_le_bytes([data[0], data[1]]);
                            let compl = u16::from_le_bytes([data[2], data[3]]);
                            let mut s = status.lock().unwrap();
                            s.motor_distance_mm = dist;
                            s.motor_completion = compl;
                        }
                    }
                    Err(_) => {}
                }
                seq = seq.wrapping_add(1);
            }
        }
    }
}
