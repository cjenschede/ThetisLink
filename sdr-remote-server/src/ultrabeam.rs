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

/// Build labels CSV for network broadcast.
/// Format (v1, 11 fields): fw_major,fw_minor,operation,frequency_khz,band,direction,off_state,motors_moving,motor_distance_mm,motor_completion,elements(;-sep)
/// Format (v2, 13 fields): + freq_min_mhz, freq_max_mhz (appended; older clients ignore trailing fields)
pub fn status_labels_string(s: &UltraBeamStatus) -> String {
    let elems: Vec<String> = s.elements_mm.iter().map(|e| e.to_string()).collect();
    format!(
        "{},{},{},{},{},{},{},{},{},{},{},{},{}",
        s.fw_major, s.fw_minor, s.operation, s.frequency_khz, s.band,
        s.direction, s.off_state as u8, s.motors_moving,
        s.motor_distance_mm, s.motor_completion,
        elems.join(";"),
        s.freq_min_mhz, s.freq_max_mhz,
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

    // Read with timeout - we have ~2s port timeout set
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
                    // DLE-quoted byte - take as literal data
                    payload.push(b);
                    dle_pending = false;
                } else if b == DLE {
                    dle_pending = true;
                } else if b == ETX {
                    // End of packet - verify checksum
                    // Payload contains: SEQ, COM, DAT..., CHK
                    if payload.len() < 3 {
                        return Err("Packet too short".to_string());
                    }
                    // Verify: computing checksum over all bytes including CHK should yield 1.
                    // Note: ~every 84-148s the RCU-06 sends an unsolicited broadcast frame
                    // (3 bursts) with a deviating checksum-init that makes verify=0x81. These
                    // are reported by read_packet as "Checksum mismatch (got 129)"
                    // and the poll-loop recovers after ~30s. No functional impact -
                    // motor position is tracked by the RCU-06 itself.
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
    // byte 7 bit 0 = "any motor moving" / busy. Field name stays `off_state`
    // (even though that is misleading) to avoid breaking the current API; the UI uses
    // bit 0 of `motors_moving` for per-motor display.
    let flags = data[7];
    s.off_state = (flags & 0x01) != 0;
    // byte 8 is a controller-state byte (no motion info); currently not
    // used by the UI. byte 9 is the per-motor moving bitfield:
    // bit 0 = motor 1, bit 1 = motor 2.
    s.motors_moving = data[9];
    s.freq_max_mhz = data[10];
    // data[11..] = trailing bytes, not interpreted
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

    // Initial element-lengths read at connect-time, so the Menu panel can
    // render Element Lengths immediately when the operator opens the
    // UltraBeam window with `ultrabeam_show_menu = true` from config.
    // Without this the elements_mm[] stays zeroed until the operator
    // toggles Menu off and on (which sends a ReadElements as side-effect).
    match send_and_receive(&mut port, seq, CMD_READ_ELEMENTS, &[]) {
        Ok((_s, _c, data)) => {
            if data.len() >= 12 {
                let mut elements = [0u16; 6];
                for i in 0..6 {
                    elements[i] = u16::from_le_bytes([data[i * 2], data[i * 2 + 1]]);
                }
                let mut s = status.lock().unwrap();
                s.elements_mm = elements;
                debug!("UltraBeam initial elements: {:?}", elements);
            } else {
                warn!("UltraBeam initial elements: short response ({} bytes)", data.len());
            }
        }
        Err(e) => warn!("UltraBeam initial elements read failed: {}", e),
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
                                    // Non-freq command lost - acceptable trade-off vs queue explosion.
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
                        info!("UltraBeam: ModifyElement {} -> {} mm", index, length_mm);
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
                // Regular poll cycle - query status
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
                        // Preserve elements_mm and motor_progress across status polls -
                        // these are filled by separate CMD_READ_ELEMENTS / CMD_MOTOR_PROGRESS
                        // commands and must not be reset to 0 by every status
                        // poll.
                        let preserved_elements = s.elements_mm;
                        let preserved_motor_dist = s.motor_distance_mm;
                        let preserved_motor_compl = s.motor_completion;
                        *s = parsed;
                        s.elements_mm = preserved_elements;
                        s.motor_distance_mm = preserved_motor_dist;
                        s.motor_completion = preserved_motor_compl;
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

        // Per-motor moving bitfield (motors_moving) contains bit 0 = motor 1, bit 1 =
        // motor 2. Both zero = nobody moving; no progress poll needed.
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
                    Err(e) => {
                        debug!("UltraBeam motor_progress query failed: {}", e);
                    }
                }
                seq = seq.wrapping_add(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_serial::FakeSerial;

    /// A packet this code builds is a packet this code can read back, and that
    /// holds for a payload full of the three bytes that have to be escaped.
    ///
    /// This is the test the file was missing most: quoting, the checksum and
    /// the reader's state machine only mean anything together, and each of the
    /// three could be wrong in a way the other two hide. 0xF5/0xFA/0xF6 in the
    /// data are exactly the bytes that end a packet early if the quoting is
    /// dropped on either side.
    #[test]
    fn a_packet_survives_being_written_and_read_back() {
        let data = [0x01, STX, 0x02, ETX, DLE, 0x03];
        let fake = FakeSerial::new();
        fake.log().to_read = build_packet(0x2a, CMD_STATUS, &data);
        let mut port = fake.boxed();

        let (seq, com, got) = read_packet(&mut port).expect("a packet we built ourselves");

        assert_eq!(seq, 0x2a);
        assert_eq!(com, CMD_STATUS);
        assert_eq!(got, data, "the escaped bytes come back as themselves");
    }

    /// The three special bytes are escaped and nothing else is.
    #[test]
    fn only_the_three_framing_bytes_are_escaped() {
        for b in [STX, ETX, DLE] {
            assert_eq!(quote_byte(b), vec![DLE, b], "{b:#04x} has to be quoted");
        }
        for b in [0x00, 0x01, 0x55, 0xF4, 0xF7, 0xF9, 0xFB, 0xFF] {
            assert_eq!(quote_byte(b), vec![b], "{b:#04x} must be left alone");
        }
    }

    /// The checksum is defined so that running it over the payload INCLUDING
    /// the checksum byte yields 1. The reader leans on that; if the rule ever
    /// changes on one side only, this is what says so.
    #[test]
    fn the_checksum_closes_on_one() {
        let raw = [0x2a, CMD_STATUS, 0xAA, 0xBB];
        let chk = compute_checksum(&raw);
        let mut with_chk = raw.to_vec();
        with_chk.push(chk);
        assert_eq!(compute_checksum(&with_chk), 1);
        assert_eq!(compute_checksum(&[]), 0x55, "and it starts at 0x55");
    }

    /// One flipped byte in the body is caught.
    #[test]
    fn a_corrupted_packet_is_refused() {
        let mut pkt = build_packet(1, CMD_STATUS, &[0x10, 0x20, 0x30]);
        let i = pkt.len() - 3; // inside the body, clear of STX/CHK/ETX
        pkt[i] ^= 0xFF;
        let fake = FakeSerial::new();
        fake.log().to_read = pkt;
        let mut port = fake.boxed();

        let err = read_packet(&mut port).unwrap_err();
        assert!(err.starts_with("Checksum mismatch"), "got: {err}");
    }

    /// A second STX before the end throws away what came before it. That is
    /// what makes the reader recover from a half packet instead of carrying
    /// its bytes into the next one.
    #[test]
    fn a_new_start_byte_discards_the_half_packet() {
        let mut stream = vec![STX, 0x99, 0x99]; // a packet that never finished
        stream.extend(build_packet(7, CMD_STATUS, &[0x42]));
        let fake = FakeSerial::new();
        fake.log().to_read = stream;
        let mut port = fake.boxed();

        let (seq, _, data) = read_packet(&mut port).expect("the second packet");
        assert_eq!((seq, data), (7, vec![0x42]));
    }

    /// A port with nothing to say is a timeout, not an error and not a packet.
    /// The poll loop tells those apart.
    #[test]
    fn silence_is_a_timeout() {
        let fake = FakeSerial::new();
        let mut port = fake.boxed();
        assert_eq!(read_packet(&mut port).unwrap_err(), "Timeout");
    }

    /// Sending writes the framed packet before it reads, and reads the answer.
    ///
    /// Two things this test had to be beaten into being able to catch, both
    /// found by mutating the code under it rather than by reading it:
    ///
    /// 1. The expected bytes are written out, not expressed as
    ///    `build_packet(..)`. Comparing what was written against the function
    ///    that wrote it passes whatever that function does. Same hole
    ///    `the_hold_is_one_second` in `filter_width.rs` exists to close.
    /// 2. The payload is `STX` on purpose. With an ordinary byte the frame
    ///    contains nothing that needs escaping, so the literal expectation
    ///    still survived `quote_byte` being mutated to escape nothing.
    ///
    /// Checksum by hand over SEQ, COM, DAT: 0x55, (^3)+1 = 0x57, (^1)+1 = 0x57,
    /// (^0xF5)+1 = 0xA3. The 0xF5 in the data is sent as DLE 0xF5; the checksum
    /// 0xA3 is not special and travels bare.
    #[test]
    fn send_and_receive_writes_the_frame_then_reads() {
        let fake = FakeSerial::new();
        fake.log().to_read = build_packet(3, CMD_STATUS, &[0x01]);
        let mut port = fake.boxed();

        let (seq, com, data) = send_and_receive(&mut port, 3, CMD_STATUS, &[STX]).expect("answered");

        assert_eq!(
            fake.log().written,
            vec![STX, 0x03, 0x01, DLE, STX, 0xA3, ETX],
            "the bytes on the wire, spelled out"
        );
        assert_eq!((seq, com, data), (3, CMD_STATUS, vec![0x01]));
    }

    /// Which byte of the status means what. Written out with a distinct value
    /// per position, so a field reading its neighbour cannot pass.
    ///
    /// Note byte 8: it is skipped. `motors_moving` is byte 9, and the comment
    /// above the code says byte 8 carries no motion information. If that ever
    /// turns out to be wrong, this test is where the off-by-one would have been
    /// caught.
    ///
    /// The first version of this said "distinct value per position" and was not.
    /// It carried 0x02 at both 0 and 9, 0x03 at both 2 and 4, and 0x01 at both 6
    /// and 7, so three of the swaps it claims to catch went straight through -
    /// `direction` reading byte 7, `motors_moving` reading byte 0 and
    /// `operation` reading byte 4 all survived a reviewer's mutations. The
    /// comment was the promise; the array was the test.
    #[test]
    fn every_status_byte_lands_in_its_own_field() {
        //              0     1     2     3     4     5     6     7     8     9    10    11
        let data = [0x11, 0x22, 0x33, 0xE8, 0x03, 0x55, 0x66, 0x77, 0xEE, 0x99, 0xAA, 0x00];
        let s = parse_status(&data).expect("twelve bytes is enough");

        assert!(s.connected, "a parsed status is a connected one");
        assert_eq!((s.fw_major, s.fw_minor), (0x11, 0x22));
        assert_eq!(s.operation, 0x33);
        assert_eq!(s.frequency_khz, 0x03E8, "little-endian over bytes 3 and 4");
        assert_eq!(s.band, 0x55);
        assert_eq!(s.direction, 0x66);
        assert!(s.off_state, "bit 0 of byte 7 (0x77)");
        assert_eq!(s.motors_moving, 0x99, "byte 9, not byte 8 (0xEE)");
        assert_eq!(s.freq_max_mhz, 0xAA);
    }

    /// Eleven bytes is refused, twelve is accepted. The boundary itself, since
    /// every field above is read by index and a short slice would panic.
    #[test]
    fn a_status_shorter_than_twelve_bytes_is_refused() {
        assert!(parse_status(&[0u8; 11]).is_err());
        assert!(parse_status(&[0u8; 12]).is_ok());
        assert!(parse_status(&[0u8; 20]).is_ok(), "and trailing bytes are ignored");
    }

    /// `off_state` is false when bit 0 is clear, whatever the other bits do.
    #[test]
    fn the_busy_flag_reads_bit_zero_only() {
        let mut data = [0u8; 12];
        data[7] = 0xFE; // every bit but the lowest
        assert!(!parse_status(&data).unwrap().off_state);
        data[7] = 0x01;
        assert!(parse_status(&data).unwrap().off_state);
    }
}
