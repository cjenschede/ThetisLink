// SPDX-License-Identifier: GPL-2.0-or-later

use std::io::{Read, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use log::{debug, info, warn};

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

        Self { cmd_tx, status }
    }

    pub fn send_command(&self, cmd: AmplitecCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn status(&self) -> AmplitecStatus {
        self.status.lock().unwrap().clone()
    }
}

/// Reconnect interval when the device is offline. 5 sec is a reasonable balance
/// between "noticing that it is back on" and "no log spam during
/// longer outages". The wait is aborted as soon as the
/// command channel closes (server stop), so the thread exits cleanly.
const AMPLITEC_RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Polling interval during an active session. A SCAN command every 2 sec
/// for position updates + heartbeat so a silent USB fault
/// is detected quickly.
const AMPLITEC_POLL_INTERVAL: Duration = Duration::from_secs(2);

fn amplitec_thread(
    cmd_rx: mpsc::Receiver<AmplitecCmd>,
    status: Arc<Mutex<AmplitecStatus>>,
    port_name: String,
) {
    info!("Amplitec serial thread started for {}", port_name);
    // Log state per outage so we get only one warn per disconnect event
    // instead of one per retry attempt (5 sec).
    let mut have_logged_offline = false;
    loop {
        // ── Try to (re)open ─────────────────────────────────────────
        let port = serialport::new(&port_name, 19200)
            .data_bits(serialport::DataBits::Eight)
            .parity(serialport::Parity::None)
            .stop_bits(serialport::StopBits::One)
            .timeout(Duration::from_millis(500))
            .open();

        let mut port = match port {
            Ok(p) => {
                info!("Amplitec port {} opened", port_name);
                have_logged_offline = false;
                {
                    let mut s = status.lock().unwrap();
                    s.connected = true;
                }
                p
            }
            Err(e) => {
                if !have_logged_offline {
                    warn!("Amplitec {} not reachable: {} (will retry every {}s)",
                        port_name, e, AMPLITEC_RECONNECT_DELAY.as_secs());
                    have_logged_offline = true;
                } else {
                    debug!("Amplitec {} still offline: {}", port_name, e);
                }
                mark_disconnected(&status);
                // Drop pending commands - sending a stale switch cmd after a
                // power-cycle would be undesirable.
                while cmd_rx.try_recv().is_ok() {}
                // Wait for retry or for shutdown.
                match cmd_rx.recv_timeout(AMPLITEC_RECONNECT_DELAY) {
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        info!("Amplitec command channel closed during reconnect-wait, stopping");
                        return;
                    }
                    // On Timeout and on any cmd that came in
                    // (drain after retry) -> try to open again.
                    _ => continue,
                }
            }
        };

        // ── Initial scan after successful open ──────────────────────
        if let Err(e) = send_and_scan(&mut port, &CMD_SCAN, &status) {
            warn!("Amplitec initial scan on {} failed: {}", port_name, e);
            mark_disconnected(&status);
            drop(port);
            continue; // back to (re)open
        }

        // ── Command/poll loop until a fault occurs ─────────────────
        let session_outcome = run_amplitec_session(&mut port, &cmd_rx, &status);
        drop(port);
        mark_disconnected(&status);
        match session_outcome {
            SessionOutcome::Reconnect => {
                warn!("Amplitec {} disconnected, retrying", port_name);
                have_logged_offline = false;
                // continue loop: open again
            }
            SessionOutcome::Shutdown => {
                info!("Amplitec serial thread stopping ({})", port_name);
                return;
            }
        }
    }
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
    // yaesu/cat, yaesu/mod); this one was the exception, found in review.
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
    /// software, and until now held by nothing: a reviewer replaced `CMD_SCAN`
    /// and a whole row of `CMD_SWITCH_A` with `DE AD BE EF` and all 380 tests
    /// stayed green. The test that looked like it covered this compared what was
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
    /// A reviewer made switch A send B's command and all five tests passed.
    /// **That selection is still untested** (open point 6); the name no longer
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
    /// behaviour decision and not this patch's to make (open point 6).
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
    /// This is the defect a reviewer traced on 2026-09-24, and it starts with
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
    /// behaviour decision and not this patch's to make (open point 6).
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
}
