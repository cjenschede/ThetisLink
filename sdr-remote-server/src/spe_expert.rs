#![allow(dead_code)]
use std::io::{Read, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use log::{debug, info, warn};

/// SPE Expert 1.3K-FA power amplifier USB serial controller.
/// Communicates via 115200 baud, binary command protocol.
/// Status responses are comma-separated ASCII.
///
/// Protocol reference:
///   Command:  [0x55 0x55 0x55] [CNT] [DATA...] [CHK]        (CHK = sum of DATA mod 256)
///   Response: [0xAA 0xAA 0xAA] [CNT] [DATA...] [CHK_LO] [CHK_HI] [,] [CR] [LF]
///   STATUS response DATA is 67 bytes of comma-separated ASCII starting with a leading comma.
pub struct SpeExpert {
    cmd_tx: mpsc::Sender<SpeCmd>,
    status: Arc<Mutex<SpeStatus>>,
}

#[derive(Clone, Debug)]
pub struct SpeStatus {
    pub connected: bool,
    pub state: u8,           // 0=Off, 1=Standby, 2=Operate
    pub ptt: bool,           // true = TX
    pub band: u8,            // band code from PA (0=160m..10=6m, 11=4m)
    pub antenna: u8,         // TX antenna position (1 or 2)
    pub atu_bypassed: bool,  // true = ATU bypassed ('b'), false = ATU active ('a'/'t')
    pub input: u8,           // input selector (1 or 2)
    pub power_level: u8,     // 0=L, 1=M, 2=H
    pub forward_power: u16,  // watts
    pub swr_x10: u16,        // SWR x 10 (e.g. 15 = 1.5:1)
    pub temp: u8,            // max of 3 sensors, degrees C
    pub voltage_x10: u16,    // supply voltage x 10
    pub current_x10: u16,    // supply current x 10
    pub warning: u8,         // warning char as u8 ('N' = none)
    pub alarm: u8,           // alarm char as u8 ('N' = none)
}

impl Default for SpeStatus {
    fn default() -> Self {
        Self {
            connected: false,
            state: 0,
            ptt: false,
            band: 0,
            antenna: 0,
            atu_bypassed: false,
            input: 0,
            power_level: 0,
            forward_power: 0,
            swr_x10: 10,
            temp: 0,
            voltage_x10: 0,
            current_x10: 0,
            warning: b'N',
            alarm: b'N',
        }
    }
}

pub enum SpeCmd {
    ToggleOperate,  // 0x0D
    Tune,           // 0x09
    CycleAntenna,   // 0x04
    CycleInput,     // 0x01
    CyclePower,     // 0x0B
    BandUp,         // 0x03
    BandDown,       // 0x02
    PowerOff,       // 0x0A  (serial command)
    PowerOn,        // RTS pulse (not a serial command)
    DriveDown,      // 0x0F  LEFT_ARROW — decrease drive during TX
    DriveUp,        // 0x10  RIGHT_ARROW — increase drive during TX
}

// SPE response preamble byte
const RESP_PREAMBLE: u8 = 0xAA;

// Single-byte SPE serial commands
const CMD_INPUT: u8 = 0x01;
const CMD_BAND_DOWN: u8 = 0x02;
const CMD_BAND_UP: u8 = 0x03;
const CMD_ANTENNA: u8 = 0x04;
const CMD_TUNE: u8 = 0x09;
const CMD_POWER_OFF: u8 = 0x0A;
const CMD_POWER_LEVEL: u8 = 0x0B;
const CMD_OPERATE: u8 = 0x0D;
const CMD_LEFT_ARROW: u8 = 0x0F;  // Drive down during TX
const CMD_RIGHT_ARROW: u8 = 0x10; // Drive up during TX
const CMD_STATUS: u8 = 0x90;

impl SpeExpert {
    /// Open serial port and start background thread.
    pub fn new(port_name: &str) -> Result<Self, String> {
        let mut port = serialport::new(port_name, 115200)
            .data_bits(serialport::DataBits::Eight)
            .parity(serialport::Parity::None)
            .stop_bits(serialport::StopBits::One)
            .flow_control(serialport::FlowControl::None)
            .timeout(Duration::from_millis(2000))
            .open()
            .map_err(|e| format!("Failed to open {}: {}", port_name, e))?;

        // Set RTS LOW to prevent accidental power-on pulse.
        // DTR doesn't matter for 1.3K-FA.
        if let Err(e) = port.write_request_to_send(false) {
            warn!("SPE Expert: could not set RTS low: {}", e);
        }

        // Flush any stale data in the input buffer
        if let Err(e) = port.clear(serialport::ClearBuffer::Input) {
            warn!("SPE Expert: could not flush input: {}", e);
        }

        let (cmd_tx, cmd_rx) = mpsc::channel::<SpeCmd>();
        let status = Arc::new(Mutex::new(SpeStatus::default()));

        let status_for_thread = status.clone();
        let port_name_owned = port_name.to_string();

        std::thread::Builder::new()
            .name("spe-expert-serial".to_string())
            .spawn(move || {
                spe_thread(port, cmd_rx, status_for_thread, &port_name_owned);
            })
            .map_err(|e| format!("Failed to spawn SPE Expert thread: {}", e))?;

        Ok(Self { cmd_tx, status })
    }

    pub fn send_command(&self, cmd: SpeCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn status(&self) -> SpeStatus {
        self.status.lock().unwrap().clone()
    }
}

fn spe_thread(
    mut port: Box<dyn serialport::SerialPort>,
    cmd_rx: mpsc::Receiver<SpeCmd>,
    status: Arc<Mutex<SpeStatus>>,
    port_name: &str,
) {
    info!("SPE Expert serial thread started on {}", port_name);

    // Brief delay after port open before first query
    std::thread::sleep(Duration::from_millis(200));

    // Flush again just before first query
    let _ = port.clear(serialport::ClearBuffer::Input);

    let mut consecutive_failures: u32 = 0;

    // Initial status query
    match query_status(&mut port) {
        Ok(parsed) => {
            let mut s = status.lock().unwrap();
            *s = parsed;
            s.connected = true;
            consecutive_failures = 0;
            info!("SPE Expert connected, state={}", s.state);
        }
        Err(e) => {
            warn!("SPE Expert initial query failed: {}", e);
            consecutive_failures += 1;
        }
    }

    loop {
        match cmd_rx.recv_timeout(Duration::from_millis(75)) {
            Ok(cmd) => {
                // Handle PowerOn separately (RTS pulse, not a serial command)
                if matches!(cmd, SpeCmd::PowerOn) {
                    info!("SPE Expert: sending Power On RTS pulse");
                    if let Err(e) = power_on_rts_pulse(&mut port) {
                        warn!("SPE Expert power on failed: {}", e);
                    }
                    // After power on, give the PA time to boot, then poll
                    std::thread::sleep(Duration::from_millis(500));
                } else {
                    let cmd_byte = match cmd {
                        SpeCmd::ToggleOperate => CMD_OPERATE,
                        SpeCmd::Tune => CMD_TUNE,
                        SpeCmd::CycleAntenna => CMD_ANTENNA,
                        SpeCmd::CycleInput => CMD_INPUT,
                        SpeCmd::CyclePower => CMD_POWER_LEVEL,
                        SpeCmd::BandUp => CMD_BAND_UP,
                        SpeCmd::BandDown => CMD_BAND_DOWN,
                        SpeCmd::PowerOff => CMD_POWER_OFF,
                        SpeCmd::DriveDown => CMD_LEFT_ARROW,
                        SpeCmd::DriveUp => CMD_RIGHT_ARROW,
                        SpeCmd::PowerOn => unreachable!(),
                    };
                    // Flush input before sending command
                    let _ = port.clear(serialport::ClearBuffer::Input);
                    if let Err(e) = send_single_command(&mut port, cmd_byte) {
                        warn!("SPE Expert command 0x{:02X} failed: {}", cmd_byte, e);
                        consecutive_failures += 1;
                    } else {
                        info!("SPE Expert: sent command 0x{:02X}", cmd_byte);
                        // Read and discard ACK response
                        let _ = read_ack(&mut port);
                    }
                    // Brief delay then poll status to see result
                    std::thread::sleep(Duration::from_millis(200));
                }

                // Poll status after command
                let _ = port.clear(serialport::ClearBuffer::Input);
                match query_status(&mut port) {
                    Ok(parsed) => {
                        let mut s = status.lock().unwrap();
                        *s = parsed;
                        s.connected = true;
                        consecutive_failures = 0;
                    }
                    Err(e) => {
                        warn!("SPE Expert post-command status failed: {}", e);
                        consecutive_failures += 1;
                        if consecutive_failures >= 5 {
                            mark_disconnected(&status);
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Periodic status poll
                let _ = port.clear(serialport::ClearBuffer::Input);
                match query_status(&mut port) {
                    Ok(parsed) => {
                        let mut s = status.lock().unwrap();
                        *s = parsed;
                        s.connected = true;
                        if consecutive_failures > 0 {
                            info!("SPE Expert reconnected after {} failures", consecutive_failures);
                        }
                        consecutive_failures = 0;
                    }
                    Err(e) => {
                        consecutive_failures += 1;
                        if consecutive_failures <= 3 || consecutive_failures % 10 == 0 {
                            warn!(
                                "SPE Expert poll failed ({}x): {}",
                                consecutive_failures, e
                            );
                        }
                        if consecutive_failures >= 5 {
                            mark_disconnected(&status);
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                info!("SPE Expert command channel closed, stopping");
                break;
            }
        }
    }

    mark_disconnected(&status);
    info!("SPE Expert serial thread stopped");
}

fn mark_disconnected(status: &Arc<Mutex<SpeStatus>>) {
    let mut s = status.lock().unwrap();
    s.connected = false;
}

/// Power on via RTS pulse: LOW 100ms -> HIGH 2000ms -> LOW.
/// This toggles power on the 1.3K/1.5K/2K-FA series.
fn power_on_rts_pulse(port: &mut Box<dyn serialport::SerialPort>) -> Result<(), String> {
    port.write_request_to_send(false)
        .map_err(|e| format!("RTS low: {}", e))?;
    std::thread::sleep(Duration::from_millis(100));
    port.write_request_to_send(true)
        .map_err(|e| format!("RTS high: {}", e))?;
    std::thread::sleep(Duration::from_millis(2000));
    port.write_request_to_send(false)
        .map_err(|e| format!("RTS low final: {}", e))?;
    Ok(())
}

/// Send a single-byte command to the SPE Expert.
/// Format: [0x55 0x55 0x55] [0x01] [command] [checksum]
/// For single-byte commands, checksum = command byte.
fn send_single_command(
    port: &mut Box<dyn serialport::SerialPort>,
    cmd: u8,
) -> Result<(), String> {
    let packet = [0x55, 0x55, 0x55, 0x01, cmd, cmd];
    port.write_all(&packet).map_err(|e| format!("write: {}", e))?;
    port.flush().map_err(|e| format!("flush: {}", e))?;
    Ok(())
}

/// Read a short ACK response after a command (non-STATUS).
/// ACK: [0xAA 0xAA 0xAA] [0x01] [echo] [echo]
/// We consume whatever comes back within a short timeout and log it.
fn read_ack(port: &mut Box<dyn serialport::SerialPort>) -> Result<(), String> {
    let mut buf = [0u8; 64];
    // Set a short timeout for ACK
    let _ = port.set_timeout(Duration::from_millis(500));
    match port.read(&mut buf) {
        Ok(n) if n > 0 => {
            debug!("SPE Expert ACK ({} bytes): {:02X?}", n, &buf[..n]);
            // Check if response contains ASCII text (e.g. "PC: xxx")
            if n > 4 {
                let text = String::from_utf8_lossy(&buf[..n]);
                if text.contains("PC") || text.contains(',') {
                    info!("SPE Expert response: {}", text.trim());
                }
            }
        }
        _ => {}
    }
    let _ = port.set_timeout(Duration::from_millis(2000));
    Ok(())
}

/// Send STATUS query (0x90) and parse the response.
fn query_status(port: &mut Box<dyn serialport::SerialPort>) -> Result<SpeStatus, String> {
    send_single_command(port, CMD_STATUS)?;

    // Read response frame
    let data = read_status_response(port)?;

    debug!("SPE Expert raw response ({} bytes): {:?}", data.len(), String::from_utf8_lossy(&data));

    parse_status_response(&data)
}

/// Read STATUS response. The response ends with CR LF, so we read until we see that.
/// Response: [0xAA 0xAA 0xAA] [CNT] [DATA...] [CHK_LO] [CHK_HI] [,] [CR] [LF]
fn read_status_response(port: &mut Box<dyn serialport::SerialPort>) -> Result<Vec<u8>, String> {
    let mut collected = Vec::with_capacity(128);
    let mut buf = [0u8; 128];

    loop {
        match port.read(&mut buf) {
            Ok(0) => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(n) => {
                collected.extend_from_slice(&buf[..n]);

                // Check if we have a complete frame (ends with CR LF)
                if collected.len() >= 2
                    && collected[collected.len() - 2] == 0x0D
                    && collected[collected.len() - 1] == 0x0A
                {
                    return extract_status_data(&collected);
                }

                if collected.len() > 300 {
                    return Err(format!(
                        "response too large ({} bytes), raw: {:02X?}",
                        collected.len(),
                        &collected[..collected.len().min(40)]
                    ));
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                if collected.len() >= 2
                    && collected[collected.len() - 2] == 0x0D
                    && collected[collected.len() - 1] == 0x0A
                {
                    return extract_status_data(&collected);
                }
                return Err(format!(
                    "timeout ({} bytes collected, raw: {:02X?})",
                    collected.len(),
                    &collected[..collected.len().min(40)]
                ));
            }
            Err(e) => return Err(format!("read: {}", e)),
        }
    }
}

/// Extract the CSV data from a complete SPE response frame.
/// Frame: [0xAA 0xAA 0xAA] [CNT] [DATA(CNT bytes)] [CHK_LO] [CHK_HI] [,] [CR] [LF]
fn extract_status_data(raw: &[u8]) -> Result<Vec<u8>, String> {
    // Find preamble 0xAA 0xAA 0xAA
    let mut preamble_pos = None;
    for i in 0..raw.len().saturating_sub(3) {
        if raw[i] == RESP_PREAMBLE && raw[i + 1] == RESP_PREAMBLE && raw[i + 2] == RESP_PREAMBLE {
            preamble_pos = Some(i);
            break;
        }
    }

    let start = preamble_pos.ok_or_else(|| {
        format!("no 0xAA preamble found in {} bytes: {:02X?}", raw.len(), &raw[..raw.len().min(20)])
    })?;

    let cnt_pos = start + 3;
    if cnt_pos >= raw.len() {
        return Err("frame too short for CNT byte".to_string());
    }
    let cnt = raw[cnt_pos] as usize;

    let data_start = cnt_pos + 1;
    let data_end = data_start + cnt;

    // After data: CHK_LO(1) + CHK_HI(1) + separator(1) + CR(1) + LF(1) = 5 bytes
    let frame_end = data_end + 5;
    if frame_end > raw.len() {
        return Err(format!(
            "frame incomplete: need {} bytes but have {}, CNT={}",
            frame_end, raw.len(), cnt
        ));
    }

    // Verify CR LF at end
    if raw[frame_end - 2] != 0x0D || raw[frame_end - 1] != 0x0A {
        return Err(format!(
            "no CRLF at expected position, got {:02X} {:02X}",
            raw[frame_end - 2], raw[frame_end - 1]
        ));
    }

    Ok(raw[data_start..data_end].to_vec())
}

/// Parse the STATUS response CSV data into SpeStatus.
///
/// The 67-byte data starts with a leading comma:
///   ,13K,S,R,x,1,00,1a,0r,L,0000, 0.00, 0.00, 0.0, 0.0, 33, 0, 0,N,N
///
/// Split by comma gives (0-indexed):
///   [0]=""  [1]=model  [2]=state  [3]=ptt  [4]=mem_bank  [5]=input
///   [6]=band  [7]=tx_ant+atu  [8]=rx_ant  [9]=power_level  [10]=fwd_power
///   [11]=swr_atu  [12]=swr_ant  [13]=voltage  [14]=current
///   [15]=temp_upper  [16]=temp_lower  [17]=temp_combiner  [18]=warning  [19]=alarm
fn parse_status_response(data: &[u8]) -> Result<SpeStatus, String> {
    let text = String::from_utf8_lossy(data);
    let fields: Vec<&str> = text.split(',').collect();

    if fields.len() < 18 {
        return Err(format!(
            "too few fields in status: {} (expected >=18), text: {}",
            fields.len(), text
        ));
    }

    let mut s = SpeStatus::default();
    s.connected = true;

    // [2] State: S=Standby, O=Operate
    s.state = match fields[2].trim() {
        "O" => 2,
        "S" => 1,
        _ => 0,
    };

    // [3] PTT: R=RX, T=TX
    s.ptt = fields[3].trim() == "T";

    // [5] Input selector
    s.input = fields[5].trim().parse().unwrap_or(0);

    // [6] Band code (string "00"-"11")
    s.band = fields[6].trim().parse().unwrap_or(0);

    // [7] TX antenna + ATU status (e.g. "1a", "2b", "1t")
    //     first char = antenna number, second char = ATU status (b=bypass, a=active, t=tunable)
    let ant_field = fields[7].trim();
    s.antenna = ant_field.chars().next()
        .and_then(|c| c.to_digit(10))
        .unwrap_or(0) as u8;
    s.atu_bypassed = ant_field.chars().nth(1) == Some('b');

    // [9] Power level: L=0, M=1, H=2
    s.power_level = match fields[9].trim() {
        "L" => 0,
        "M" => 1,
        "H" => 2,
        _ => 0,
    };

    // [10] Forward power (watts)
    s.forward_power = fields[10].trim().parse().unwrap_or(0);

    // [11] SWR (ATU side) — multiply by 10 for integer storage
    if let Ok(swr) = fields[11].trim().parse::<f32>() {
        s.swr_x10 = (swr * 10.0) as u16;
    }

    // [13] Voltage
    if let Ok(v) = fields[13].trim().parse::<f32>() {
        s.voltage_x10 = (v * 10.0) as u16;
    }

    // [14] Current
    if let Ok(i) = fields[14].trim().parse::<f32>() {
        s.current_x10 = (i * 10.0) as u16;
    }

    // [15-17] Temperatures: take max of available sensors
    let t1: u8 = fields[15].trim().parse().unwrap_or(0);
    let t2: u8 = fields.get(16).and_then(|f| f.trim().parse().ok()).unwrap_or(0);
    let t3: u8 = fields.get(17).and_then(|f| f.trim().parse().ok()).unwrap_or(0);
    s.temp = t1.max(t2).max(t3);

    // [18] Warning
    if let Some(f) = fields.get(18) {
        s.warning = f.trim().bytes().next().unwrap_or(b'N');
    }

    // [19] Alarm
    if let Some(f) = fields.get(19) {
        s.alarm = f.trim().bytes().next().unwrap_or(b'N');
    }

    Ok(s)
}

/// Format SPE status as labels CSV string for telemetry transmission.
/// Format: "ptt,power_w,swr_x10,temp,voltage_x10,current_x10,warning,alarm,power_level,antenna,input"
pub fn status_labels_string(status: &SpeStatus) -> String {
    format!(
        "{},{},{},{},{},{},{},{},{},{},{},{}",
        if status.ptt { "T" } else { "R" },
        status.forward_power,
        status.swr_x10,
        status.temp,
        status.voltage_x10,
        status.current_x10,
        status.warning as char,
        status.alarm as char,
        status.power_level,
        status.antenna,
        status.input,
        if status.atu_bypassed { "1" } else { "0" },
    )
}

/// Convert SPE band code to a human-readable band name.
/// Band codes: 00=160m, 01=80m, 02=60m, ..., 10=6m, 11=4m
pub fn band_name(band: u8) -> &'static str {
    match band {
        0 => "160m",
        1 => "80m",
        2 => "60m",
        3 => "40m",
        4 => "30m",
        5 => "20m",
        6 => "17m",
        7 => "15m",
        8 => "12m",
        9 => "10m",
        10 => "6m",
        11 => "4m",
        _ => "?",
    }
}

/// Convert power level code to display string.
pub fn power_level_name(level: u8) -> &'static str {
    match level {
        0 => "L",
        1 => "M",
        2 => "H",
        _ => "?",
    }
}
