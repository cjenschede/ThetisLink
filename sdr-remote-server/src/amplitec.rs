// SPDX-License-Identifier: GPL-2.0-or-later

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use log::{info, warn};

/// Amplitec 6/2 antenna switch USB serial controller.
/// Communicates via 19200 baud, 12-byte binary commands.
pub struct AmplitecSwitch {
    cmd_tx: mpsc::Sender<AmplitecCmd>,
    status: Arc<Mutex<AmplitecStatus>>,
    /// A Switch A position marked RX-only that a client asked for and that
    /// waits until transmitting is blocked (`switch_a_request`); 0 = none.
    rx_only_target: AtomicU8,
    /// A Switch A position clicked on the server screen, for the broadcast
    /// loop to ask `request_switch_a` with what it knows; 0 = none.
    screen_request: AtomicU8,
}

/// What a request for a Switch A position does, from a client or from the
/// server screen (`AmplitecSwitch::request_switch_a`). A position marked
/// RX-only is never switched to while anything is on the air, and only after
/// transmitting is blocked, so no carrier ever reaches that antenna.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchARequest {
    /// Not RX-only: switch at once.
    Now,
    /// RX-only, nothing on the air: block transmitting first, then switch
    /// (`pending_switch_step`).
    AfterTheBlock,
    /// RX-only while something is on the air: not switched.
    Refused,
}

pub fn switch_a_request(target_rx_only: bool, on_the_air: bool) -> SwitchARequest {
    match (target_rx_only, on_the_air) {
        (false, _) => SwitchARequest::Now,
        (true, true) => SwitchARequest::Refused,
        (true, false) => SwitchARequest::AfterTheBlock,
    }
}

/// How long a switch to an RX-only position waits for the radio software's
/// own receive-only and for the switch to report the new position.
pub const RX_ONLY_SWITCH_WAIT: Duration = Duration::from_secs(3);

/// One tick of a switch to an RX-only position that waits for the block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingSwitch {
    Wait,
    /// The block stands and nothing is on the air: send the switch command.
    Switch,
    /// The switch reports the position: from here the position itself keeps
    /// the block.
    Done,
    /// Not blocked or not arrived in `RX_ONLY_SWITCH_WAIT`: drop the request.
    GiveUp,
}

/// `sent_at_scan`: the switch's scan count when the switch command went out,
/// `None` before. `radio_closed`: `radio_holds_receive_only`. A request whose
/// command went out is given up only once the switch has answered since and
/// is not at the position: until then the relay may have moved, so the block
/// stays.
pub fn pending_switch_step(
    target: u8,
    sent_at_scan: Option<u32>,
    radio_closed: bool,
    on_the_air: bool,
    status: &AmplitecStatus,
    waited: Duration,
) -> PendingSwitch {
    if status.switch_a == target {
        return PendingSwitch::Done;
    }
    match sent_at_scan {
        None if radio_closed && !on_the_air => PendingSwitch::Switch,
        None if waited >= RX_ONLY_SWITCH_WAIT => PendingSwitch::GiveUp,
        Some(at) if status.scans != at && waited >= RX_ONLY_SWITCH_WAIT => PendingSwitch::GiveUp,
        _ => PendingSwitch::Wait,
    }
}

/// Whether the radio software holds its own receive-only, for a switch that
/// waits for it: with `rx_only_ex` the echo says so; on a Thetis that answers
/// `run_cat_ex`, our `ZZTI1` went out; without either there is nothing to wait
/// for, and only the server's own gate stands (reported as unguarded).
pub fn radio_holds_receive_only(has_rx_only_ex: bool, thetis_rx_only: bool, answers_run_cat_ex: bool, we_sent_zzti1: bool) -> bool {
    if has_rx_only_ex {
        thetis_rx_only
    } else if answers_run_cat_ex {
        we_sent_zzti1
    } else {
        true
    }
}

#[derive(Clone, Debug)]
pub struct AmplitecStatus {
    pub switch_a: u8,  // 0=unknown, 1-6
    pub switch_b: u8,
    pub connected: bool,
    /// Answers read from the switch so far, so a waiting switch knows the
    /// position it sees is newer than its command.
    pub scans: u32,
}

impl Default for AmplitecStatus {
    fn default() -> Self {
        Self { switch_a: 0, switch_b: 0, connected: false, scans: 0 }
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
    /// Spawn the serial worker thread. Returns immediately; the
    /// thread opens the port asynchronously and retries on failure,
    /// so the caller does not need to handle a connection error here.
    /// Use `status().connected` to observe whether the device is
    /// currently reachable.
    pub fn new(port_name: &str) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<AmplitecCmd>();
        let status = Arc::new(Mutex::new(AmplitecStatus::default()));

        let status_for_thread = status.clone();
        let port_name_owned = port_name.to_string();

        std::thread::Builder::new()
            .name("amplitec-serial".to_string())
            .spawn(move || {
                amplitec_thread(cmd_rx, status_for_thread, port_name_owned);
            })
            .expect("Failed to spawn amplitec thread");

        Self { cmd_tx, status, rx_only_target: AtomicU8::new(0), screen_request: AtomicU8::new(0) }
    }

    pub fn send_command(&self, cmd: AmplitecCmd) {
        if let AmplitecCmd::SetSwitchA(_) = cmd {
            // Any switch of A ends a request that waited for the block.
            self.rx_only_target.store(0, Ordering::Relaxed);
        }
        let _ = self.cmd_tx.send(cmd);
    }

    /// A request for a Switch A position, the one place that decides it: at
    /// once, after the block (the request first, then `gate`, so a tick in
    /// between already sees the request), or refused. `rx_only` is the table of
    /// positions marked RX-only, `on_the_air` whether anything transmits.
    pub fn request_switch_a(&self, pos: u8, rx_only: &[bool; 6], on_the_air: bool, gate: &AtomicBool, from: &str) -> SwitchARequest {
        let target_rx_only = (1..=6).contains(&pos) && rx_only[(pos - 1) as usize];
        let request = switch_a_request(target_rx_only, on_the_air);
        match request {
            SwitchARequest::Now => {
                info!("Amplitec: Switch A -> {} ({})", pos, from);
                self.send_command(AmplitecCmd::SetSwitchA(pos));
            }
            SwitchARequest::AfterTheBlock => {
                self.request_rx_only(pos);
                gate.store(true, Ordering::Relaxed);
                info!("Amplitec: Switch A -> {} ({}) is RX-only; switching once transmitting is blocked", pos, from);
            }
            SwitchARequest::Refused => {
                warn!("Amplitec: Switch A -> {} ({}) refused: it is RX-only and something is on the air", pos, from);
            }
        }
        request
    }

    /// A click on the server screen: the broadcast loop asks
    /// `request_switch_a` with the table and the air (`take_screen_request`).
    pub fn ask_from_screen(&self, pos: u8) {
        self.screen_request.store(pos, Ordering::Relaxed);
    }

    /// The position clicked on the server screen since the last tick, if any.
    pub fn take_screen_request(&self) -> Option<u8> {
        Some(self.screen_request.swap(0, Ordering::Relaxed)).filter(|p| (1..=6).contains(p))
    }

    /// Ask for an RX-only position of Switch A: switched by the broadcast
    /// loop once transmitting is blocked (`pending_switch_step`).
    fn request_rx_only(&self, pos: u8) {
        self.rx_only_target.store(pos, Ordering::Relaxed);
    }

    /// The RX-only position that waits for the block, if any.
    pub fn rx_only_target(&self) -> Option<u8> {
        Some(self.rx_only_target.load(Ordering::Relaxed)).filter(|p| (1..=6).contains(p))
    }

    /// Drop the waiting request, if it is still `pos`.
    pub fn drop_rx_only(&self, pos: u8) {
        let _ = self.rx_only_target.compare_exchange(pos, 0, Ordering::Relaxed, Ordering::Relaxed);
    }

    pub fn status(&self) -> AmplitecStatus {
        self.status.lock().unwrap().clone()
    }
}

/// Polling interval during an active session. A SCAN command every 2 sec
/// for position updates + heartbeat so a silent USB fault
/// is detected quickly.
const AMPLITEC_POLL_INTERVAL: Duration = Duration::from_secs(2);

fn amplitec_thread(
    cmd_rx: mpsc::Receiver<AmplitecCmd>,
    status: Arc<Mutex<AmplitecStatus>>,
    port_name: String,
) {
    let spec = crate::serial_link::PortSpec {
        baud: 19200,
        timeout: Duration::from_millis(500),
        flow_control: serialport::FlowControl::None,
    };
    crate::serial_link::run("Amplitec", &port_name, spec, &cmd_rx, || mark_disconnected(&status), |mut port, link| {
        // `send_and_scan` marks the switch connected once it answers; marking it
        // here would flash "connected" for a switch that is off on its USB.
        if let Err(e) = send_and_scan(&mut port, &CMD_SCAN, &status) {
            return crate::serial_link::SessionEnd::Reconnect(format!("initial scan failed: {}", e));
        }
        link.up();
        match run_amplitec_session(&mut port, &cmd_rx, &status) {
            SessionOutcome::Reconnect => crate::serial_link::SessionEnd::Reconnect("a switch or poll command failed".to_string()),
            SessionOutcome::Shutdown => crate::serial_link::SessionEnd::Shutdown,
        }
    });
}

enum SessionOutcome {
    /// Serial fault occurred - close the port and open it again.
    Reconnect,
    /// Command channel closed (server stop). Thread may end.
    Shutdown,
}

fn run_amplitec_session(
    port: &mut Box<dyn serialport::SerialPort>,
    cmd_rx: &mpsc::Receiver<AmplitecCmd>,
    status: &Arc<Mutex<AmplitecStatus>>,
) -> SessionOutcome {
    loop {
        match cmd_rx.recv_timeout(AMPLITEC_POLL_INTERVAL) {
            Ok(AmplitecCmd::SetSwitchA(pos)) => {
                if (1..=6).contains(&pos) {
                    let cmd = CMD_SWITCH_A[(pos - 1) as usize];
                    if let Err(e) = send_and_scan(port, &cmd, status) {
                        warn!("Amplitec set A{} failed: {}", pos, e);
                        return SessionOutcome::Reconnect;
                    }
                    info!("Amplitec: Switch A -> {}", pos);
                }
            }
            Ok(AmplitecCmd::SetSwitchB(pos)) => {
                if (1..=6).contains(&pos) {
                    let cmd = CMD_SWITCH_B[(pos - 1) as usize];
                    if let Err(e) = send_and_scan(port, &cmd, status) {
                        warn!("Amplitec set B{} failed: {}", pos, e);
                        return SessionOutcome::Reconnect;
                    }
                    info!("Amplitec: Switch B -> {}", pos);
                }
            }
            Ok(AmplitecCmd::Query) | Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Err(e) = send_and_scan(port, &CMD_SCAN, status) {
                    warn!("Amplitec poll failed: {}", e);
                    return SessionOutcome::Reconnect;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return SessionOutcome::Shutdown;
            }
        }
    }
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
    // Drop whatever is still in the port before asking. Nothing is in flight
    // here - the previous exchange has finished, successfully or not - so this
    // is the safe moment, the same one `yaesu/cat.rs` describes.
    //
    // Without it a reply that arrived in two reads leaves its tail behind, and
    // the next poll reads that tail first: it passes the sixteen-character
    // length test on the wrong bytes and reports `connected` with switch
    // positions that are one answer behind, for as long as the thread lives.
    // Every other serial device here already drained (spe_expert, ultrabeam,
    // yaesu/cat, yaesu/mod); this one was the exception.
    let _ = port.clear(serialport::ClearBuffer::Input);

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
        s.scans = s.scans.wrapping_add(1);
    }

    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_serial::FakeSerial;

    /// The response is hex-formatted and read at character 9 and 15, so the
    /// switch positions are the LOW nibbles of bytes 4 and 7. Spelling that out
    /// once here beats repeating the arithmetic in every test: 'C' is 0x43 and
    /// 'E' is 0x45, so this answer means switch A = 3, switch B = 5.
    /// A reply that means switch A = 3, switch B = 5. It is handed to the
    /// fake as an ANSWER to a command rather than parked in the port, because
    /// the port is drained before every question - a test that pre-loads it is
    /// modelling a device that speaks before it is spoken to.
    const ANSWER_A3_B5: &str = "AAAACAAE";

    fn status() -> Arc<Mutex<AmplitecStatus>> {
        Arc::new(Mutex::new(AmplitecStatus::default()))
    }

    /// The command really reaches the port, and the answer really lands in the
    /// status. Before this test the whole file had none, so "write the wrong
    /// command" and "read the wrong character" were both free.
    #[test]
    fn a_scan_writes_the_command_and_reads_both_switches() {
        let fake = FakeSerial::answering_each_command(&[ANSWER_A3_B5]);
        let mut port = fake.boxed();
        let st = status();

        send_and_scan(&mut port, &CMD_SCAN, &st).expect("the port answered");

        assert_eq!(fake.log().written, CMD_SCAN.to_vec(), "the scan command, verbatim");
        let s = st.lock().unwrap();
        assert_eq!((s.switch_a, s.switch_b), (3, 5));
        assert!(s.connected, "an answer is what makes it connected");
    }

    /// The bytes that move the antenna, written out.
    ///
    /// Seventy-eight of them, read out of the original Amplitec PC Control Pro 2
    /// software, and until now held by nothing: replacing `CMD_SCAN` and a
    /// whole row of `CMD_SWITCH_A` with `DE AD BE EF` left all 380 tests
    /// green. The test that looked like it covered this compared what was
    /// written against the very constant that was written - the same tautology
    /// found and repaired one file over, in this same commit.
    ///
    /// There is no second source for these: no datasheet, no protocol document.
    /// So this test is not a duplicate of the truth, it IS the record of it, and
    /// a change here has to be a deliberate one.
    #[test]
    fn the_command_bytes_are_the_ones_read_from_the_vendor_software() {
        assert_eq!(CMD_SCAN, [0x01, 0x0A, 0x00, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(CMD_SWITCH_A, [
            [0x01, 0x0a, 0x01, 0x82, 0x01, 0x20, 0x80, 0x06, 0x00, 0x00, 0x00, 0x00],
            [0x01, 0x0a, 0x02, 0x82, 0x02, 0x20, 0x80, 0x06, 0x00, 0x00, 0x00, 0x00],
            [0x01, 0x0a, 0x04, 0x82, 0x03, 0x01, 0x80, 0x01, 0x00, 0x00, 0x00, 0x00],
            [0x01, 0x0a, 0x08, 0x82, 0x04, 0x01, 0x80, 0x01, 0x00, 0x00, 0x00, 0x00],
            [0x01, 0x0a, 0x10, 0x82, 0x05, 0x20, 0x80, 0x06, 0x00, 0x00, 0x00, 0x00],
            [0x01, 0x0a, 0x20, 0x02, 0x06, 0x20, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00],
        ]);
        assert_eq!(CMD_SWITCH_B, [
            [0x01, 0x0a, 0x20, 0x80, 0x06, 0x01, 0x82, 0x01, 0x00, 0x00, 0x00, 0x00],
            [0x01, 0x0a, 0x20, 0x00, 0x06, 0x02, 0x02, 0x02, 0x00, 0x00, 0x00, 0x00],
            [0x01, 0x0a, 0x20, 0x80, 0x06, 0x04, 0x82, 0x03, 0x00, 0x00, 0x00, 0x00],
            [0x01, 0x0a, 0x20, 0x80, 0x06, 0x08, 0x82, 0x04, 0x00, 0x00, 0x00, 0x00],
            [0x01, 0x0a, 0x20, 0x80, 0x06, 0x10, 0x82, 0x05, 0x00, 0x00, 0x00, 0x00],
            [0x01, 0x0a, 0x08, 0x80, 0x04, 0x20, 0x82, 0x06, 0x00, 0x00, 0x00, 0x00],
        ]);
    }

    /// Whatever command it is handed reaches the port unchanged, and the two
    /// tables are not the same bytes.
    ///
    /// Renamed. It used to be called `switch_a_and_switch_b_send_from_their_own_table`,
    /// which promised something it never reached: choosing the table from the
    /// position happens in `run_amplitec_session`, and no test here calls that.
    /// Making switch A send B's command left all five tests passing.
    /// **That selection is still untested**; the name no longer
    /// says otherwise.
    #[test]
    fn whatever_command_it_is_given_is_what_goes_out() {
        for (table, name) in [(&CMD_SWITCH_A, "A"), (&CMD_SWITCH_B, "B")] {
            let fake = FakeSerial::answering_each_command(&[ANSWER_A3_B5]);
            let mut port = fake.boxed();
            send_and_scan(&mut port, &table[3], &status()).expect("answered");
            assert_eq!(fake.log().written, table[3].to_vec(), "switch {name} position 4");
        }
        assert_ne!(CMD_SWITCH_A[3], CMD_SWITCH_B[3], "the tables must differ, or the loop above proves nothing");
    }

    /// A port that answers nothing is an error, and the status is NOT marked
    /// connected. That is the difference that makes the caller reconnect.
    ///
    /// It arrives as a READ error, not as the "empty response" branch below it.
    /// A serial port with nothing to say times out; it does not hand back zero
    /// bytes. So `if n == 0` in `send_and_scan` is a guard that cannot fire
    /// against a port that behaves like one, and this test says so rather than
    /// asserting a message that never appears. Removing the branch is a
    /// behaviour decision that is still open.
    #[test]
    fn silence_is_an_error_and_leaves_it_disconnected() {
        let fake = FakeSerial::new(); // answers nothing at all
        let mut port = fake.boxed();
        let st = status();

        let err = send_and_scan(&mut port, &CMD_SCAN, &st).unwrap_err();
        assert!(err.starts_with("read:"), "got: {err}");
        assert!(!err.contains("empty response"), "the n == 0 branch is not the one that fires");
        assert!(!st.lock().unwrap().connected);
    }

    /// A write that fails is an error before anything is read.
    #[test]
    fn a_dead_port_is_an_error_on_the_write() {
        let fake = FakeSerial::failing_writes_from(0);
        let mut port = fake.boxed();
        let st = status();

        let err = send_and_scan(&mut port, &CMD_SCAN, &st).unwrap_err();
        assert!(err.starts_with("write:"), "got: {err}");
        assert!(!st.lock().unwrap().connected);
    }

    /// Bytes left over from a previous answer do not become this one.
    ///
    /// This is the defect traced on 2026-09-24, and it starts with
    /// the short answer pinned below. A reply that arrives in two reads leaves
    /// its tail in the port; nothing here ever drained it, while `spe_expert`,
    /// `ultrabeam`, `yaesu/cat` and `yaesu/mod` all do. The next poll then reads
    /// the tail FIRST, sails past sixteen hex characters on the wrong bytes, and
    /// reports `connected` with switch positions that belong to no switch -
    /// permanently, because every poll after it starts one answer behind.
    ///
    /// The stale bytes here are chosen to be readable rather than realistic:
    /// 'B' is 0x42, so without the drain this test sees A=2, B=2.
    #[test]
    fn a_leftover_from_the_last_answer_is_not_read_as_this_one() {
        let fake = FakeSerial::answering_each_command(&[ANSWER_A3_B5]);
        fake.log().to_read = b"BBBBBBBB".to_vec(); // the tail of something older
        let mut port = fake.boxed();
        let st = status();

        send_and_scan(&mut port, &CMD_SCAN, &st).expect("the device answered");

        let s = st.lock().unwrap();
        assert_eq!(
            (s.switch_a, s.switch_b),
            (3, 5),
            "read the answer to THIS command, not what was still in the port"
        );
    }

    /// An answer too short to hold both positions is accepted as success and
    /// changes nothing - not even `connected`.
    ///
    /// This test pins what the code does today, and it is written down as a
    /// question rather than as approval: a reply of seven bytes returns `Ok`,
    /// so the caller does not reconnect, while the switch positions stay at
    /// whatever they were and the device never counts as connected. A partial
    /// read of a good answer looks exactly like this. Changing it is a
    /// behaviour decision that is still open.
    #[test]
    fn a_short_answer_is_silently_accepted_and_changes_nothing() {
        let fake = FakeSerial::answering_each_command(&["AAAACAA"]); // one byte short
        let mut port = fake.boxed();
        let st = status();

        send_and_scan(&mut port, &CMD_SCAN, &st).expect("today this is Ok, not an error");

        let s = st.lock().unwrap();
        assert_eq!((s.switch_a, s.switch_b), (0, 0), "untouched");
        assert!(!s.connected, "and it never became connected");
    }

    /// A position marked RX-only is not switched to while anything is on
    /// the air, and otherwise only after the block; any other position at once.
    #[test]
    fn an_rx_only_position_waits_for_the_block() {
        assert_eq!(switch_a_request(false, false), SwitchARequest::Now);
        assert_eq!(switch_a_request(false, true), SwitchARequest::Now);
        assert_eq!(switch_a_request(true, true), SwitchARequest::Refused);
        assert_eq!(switch_a_request(true, false), SwitchARequest::AfterTheBlock);
        let (s, w) = (Duration::ZERO, RX_ONLY_SWITCH_WAIT);
        let at = |pos: u8, scans: u32| AmplitecStatus { switch_a: pos, switch_b: 0, connected: true, scans };
        // Not yet sent: out only when the radio holds its receive-only and nothing is on the air.
        assert_eq!(pending_switch_step(4, None, true, false, &at(1, 7), s), PendingSwitch::Switch);
        assert_eq!(pending_switch_step(4, None, false, false, &at(1, 7), s), PendingSwitch::Wait);
        assert_eq!(pending_switch_step(4, None, true, true, &at(1, 7), s), PendingSwitch::Wait);
        assert_eq!(pending_switch_step(4, None, false, false, &at(1, 7), w), PendingSwitch::GiveUp);
        assert_eq!(pending_switch_step(4, None, true, true, &at(1, 7), w), PendingSwitch::GiveUp);
        // Already at the position: done, without a command.
        assert_eq!(pending_switch_step(4, None, false, true, &at(4, 7), s), PendingSwitch::Done);
        // Sent: done when the switch reports this position, not another.
        assert_eq!(pending_switch_step(4, Some(7), true, false, &at(4, 8), s), PendingSwitch::Done);
        assert_eq!(pending_switch_step(4, Some(7), true, false, &at(3, 8), s), PendingSwitch::Wait);
        // Sent and no answer since: the relay may have moved, the block stays.
        assert_eq!(pending_switch_step(4, Some(7), true, false, &at(1, 7), w), PendingSwitch::Wait);
        // Sent, answered since at another position, and the wait is over: given up.
        assert_eq!(pending_switch_step(4, Some(7), true, false, &at(1, 8), w), PendingSwitch::GiveUp);
        assert_eq!(RX_ONLY_SWITCH_WAIT, Duration::from_secs(3));
        // What the radio software holds: the echo with rx_only_ex, our ZZTI1 on
        // a Thetis that answers run_cat_ex, nothing to wait for without either.
        assert!(radio_holds_receive_only(true, true, true, false));
        assert!(!radio_holds_receive_only(true, false, true, true));
        assert!(radio_holds_receive_only(false, false, true, true));
        assert!(!radio_holds_receive_only(false, true, true, false));
        assert!(radio_holds_receive_only(false, false, false, false));
    }

    fn switch() -> (AmplitecSwitch, mpsc::Receiver<AmplitecCmd>) {
        let (cmd_tx, cmd_rx) = mpsc::channel::<AmplitecCmd>();
        let amp = AmplitecSwitch {
            cmd_tx,
            status: Arc::new(Mutex::new(AmplitecStatus::default())),
            rx_only_target: AtomicU8::new(0),
            screen_request: AtomicU8::new(0),
        };
        (amp, cmd_rx)
    }

    /// The request is kept on the switch itself: any switch of A ends it, a
    /// drop only ends the position it names. Twice, without a restart.
    #[test]
    fn the_rx_only_request_lives_on_the_switch() {
        let (amp, _cmd_rx) = switch();
        for _ in 0..2 {
            assert_eq!(amp.rx_only_target(), None);
            amp.request_rx_only(4);
            assert_eq!(amp.rx_only_target(), Some(4));
            amp.drop_rx_only(3);
            assert_eq!(amp.rx_only_target(), Some(4), "another position");
            amp.send_command(AmplitecCmd::SetSwitchA(2));
            assert_eq!(amp.rx_only_target(), None, "a switch of A ends it");
            amp.request_rx_only(5);
            amp.send_command(AmplitecCmd::SetSwitchB(1));
            assert_eq!(amp.rx_only_target(), Some(5), "switch B does not");
            amp.drop_rx_only(5);
            // A click on the screen is taken once.
            amp.ask_from_screen(6);
            assert_eq!(amp.take_screen_request(), Some(6));
            assert_eq!(amp.take_screen_request(), None);
        }
    }

    /// The one decision, with what it reads: the position in the table (its
    /// own index, not another), the air, and the gate. Twice, without a restart.
    #[test]
    fn a_switch_request_reads_its_own_row_and_the_air() {
        let (amp, cmd_rx) = switch();
        let gate = AtomicBool::new(false);
        let mut rx_only = [false; 6];
        rx_only[3] = true; // position 4
        for pass in 0..2 {
            assert_eq!(amp.request_switch_a(3, &rx_only, true, &gate, "test"), SwitchARequest::Now, "pass {pass}");
            assert!(matches!(cmd_rx.try_recv(), Ok(AmplitecCmd::SetSwitchA(3))));
            assert!(!gate.load(Ordering::Relaxed));
            assert_eq!(amp.request_switch_a(4, &rx_only, true, &gate, "test"), SwitchARequest::Refused, "pass {pass}");
            assert!(cmd_rx.try_recv().is_err() && !gate.load(Ordering::Relaxed) && amp.rx_only_target().is_none());
            assert_eq!(amp.request_switch_a(4, &rx_only, false, &gate, "test"), SwitchARequest::AfterTheBlock, "pass {pass}");
            assert!(cmd_rx.try_recv().is_err(), "pass {pass}: not switched yet");
            assert!(gate.load(Ordering::Relaxed) && amp.rx_only_target() == Some(4));
            amp.drop_rx_only(4);
            gate.store(false, Ordering::Relaxed);
        }
        assert_eq!(amp.request_switch_a(0, &[true; 6], false, &gate, "test"), SwitchARequest::Now, "no position, nothing to guard");
    }

    /// The wiring, as text over the whole crate: Switch A is switched in two
    /// places only, the decision (`request_switch_a`) and the loop after the
    /// block; the client and the server screen both go through the decision.
    #[test]
    fn the_switch_to_an_rx_only_position_is_wired_through_the_block() {
        fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let path = e.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|x| x == "rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    out.push((path.display().to_string(), sdr_remote_core::source_text::production_code(&text).to_string()));
                }
            }
        }
        let mut files = Vec::new();
        walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
        let count = |pat: &str| files.iter().map(|(_, t)| t.matches(pat).count()).sum::<usize>();
        let switches: Vec<&str> = files.iter().filter(|(_, t)| t.contains("send_command(") && t.contains("SetSwitchA(")).map(|(p, _)| p.as_str()).collect();
        let sent = count("send_command(AmplitecCmd::SetSwitchA(") + count("send_command(crate::amplitec::AmplitecCmd::SetSwitchA(");
        assert_eq!(sent, 2, "the decision and the loop after the block: {switches:?}");
        assert_eq!(count(".request_switch_a("), 2, "the client and the server screen");
        assert_eq!(count(".ask_from_screen(pos)"), 1, "the screen asks, it does not switch");
        let network = &files.iter().find(|(p, _)| p.ends_with("network.rs")).unwrap().1;
        let screen = network.find("amp.take_screen_request()").unwrap();
        let widened = network.find("let pos_is_blocked = pos_is_blocked || rx_only_switch.is_some();").unwrap();
        assert!(screen < widened, "a screen request blocks in the same tick");
        let stored = network[widened..].find("tx_blocked_gate.store(").unwrap();
        assert!(stored < 200, "the requested position blocks in the same tick");
        assert_eq!(
            network.matches("pos_is_blocked || blocked_for_good.load(std::sync::atomic::Ordering::Relaxed),\n").count(),
            1,
            "what the safety shut stays shut, the whole value"
        );
        assert_eq!(network.matches("rx_only_position_gate.store(pos_is_blocked, std::sync::atomic::Ordering::Relaxed);").count(), 1);
        // The position is stored before anything lifts Receive only in the
        // same tick: below it, `send` refuses the lift on leaving an RX-only
        // position, and that lift is edge-triggered, it does not come again.
        let position = network.find("rx_only_position_gate.store(pos_is_blocked, std::sync::atomic::Ordering::Relaxed);").unwrap();
        for lifter in ["ptt.lock().await.set_rx_only_cat(step).await;", "p.set_rx_only(prev).await;", "p.set_rx_only(false).await;"] {
            assert_eq!(network.matches(lifter).count(), 1, "{lifter}");
            assert!(position < network.find(lifter).unwrap(), "{lifter} after the position is stored");
        }
        // What the two callers feed the decision, whole: the table, the air, the gate.
        let screen = "amp.request_switch_a(pos, &live_config.amplitec_tx_blocked, tx_now.on_the_air(), &tx_blocked_gate, \"server screen\")";
        let client = "amp.request_switch_a(pos, &rx_only, on_the_air, &p.tx_blocked_handle(), \"client\")";
        assert_eq!((count(screen), count(client)), (1, 1));
        assert_eq!(network.matches("let rx_only = crate::config::load().amplitec_tx_blocked;").count(), 1);
        assert_eq!(network.matches("let on_the_air = p.tx_now().on_the_air();").count(), 1);
        // Every spelling of the command, not only a direct send: the guard in
        // `send_command`, the decision, the thread that reads it, the loop.
        assert_eq!(count("AmplitecCmd::SetSwitchA("), 4, "{switches:?}");
        let step = network.find("crate::amplitec::pending_switch_step(target, sent, radio_closed, tx_now.on_the_air(), &switch, since.elapsed())").unwrap();
        // All three feed the decision the one answer (`TxNow::on_the_air`), our
        // TUNE while TCI is down included.
        assert_eq!(count("tx_active || thetis_tx"), 0, "the air without the unseen TUNE");
        assert!(widened < step);
        assert!(network[step..].find("SetSwitchA(target)").is_some());
        let done = step + network[step..].find("crate::amplitec::PendingSwitch::Done => {").unwrap();
        let give_up = step + network[step..].find("crate::amplitec::PendingSwitch::GiveUp => {").unwrap();
        assert!(network[done..give_up].contains("amp.drop_rx_only(target);"), "a request that is there ends");
    }
}
