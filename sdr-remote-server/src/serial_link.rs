// SPDX-License-Identifier: GPL-2.0-or-later

//! One serial device that comes back by itself: open the port, run a session
//! on it, and when the port does not open or the session says the link is
//! gone, try again every `RECONNECT_DELAY` until the server stops.
//!
//! Every serial device on the server goes through here (Amplitec, UltraBeam,
//! SPE Expert), so a pulled and re-plugged USB cable recovers on all of them
//! and a device that is off at server start is found when it comes on. Before
//! this block only the Amplitec had it: the UltraBeam and the SPE opened their
//! port once at start and kept the dead handle after a re-plug (found in a
//! field test, 2026-11-06).
//!
//! What is the device's own business stays in the device: when its link
//! counts as gone. The SPE asks for a new port only on a hard I/O error,
//! because opening its port can move RTS, and an RTS pulse switches the
//! amplifier on; an SPE that is off but on USB only times out and keeps its
//! port. The UltraBeam also reopens after a run of failed polls.
//!
//! Logging is per outage: one warning when the link goes or the port does not
//! open, one line when the port opens again while the device has not answered
//! yet (an amplifier that is off: the port is back, the device is not), one
//! line when the device is back, and nothing in between at info level.
//!
//! Not here, on purpose: the Yaesu CAT port, whose reconnect also rebuilds the
//! audio streams and releases the PTT (`yaesu/poll.rs`); the CW key line,
//! opened per keying run and lowered first (`cw_key_line.rs`); and the
//! MCP2221A boards (tuners, rotor), which are HID and not serial ports and
//! have their own retry (`tuner.rs`, `mcp2221_yaesu_rotor.rs`).

use std::sync::mpsc;
use std::time::{Duration, Instant};

use log::{debug, info, warn};
use serialport::SerialPort;

/// How long to wait between two attempts to open a port that is not there.
pub const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// How a device's port is set up: always 8 data bits, no parity, one stop
/// bit; the rest per device.
pub struct PortSpec {
    pub baud: u32,
    pub timeout: Duration,
    pub flow_control: serialport::FlowControl,
}

/// How a session on an open port ended.
pub enum SessionEnd {
    /// The link is gone: close the port and open it again. The text says why,
    /// for the log.
    Reconnect(String),
    /// The command channel closed (the server stops): the thread may end.
    Shutdown,
}

/// What a session tells the block about the link, so the block can log the
/// way back once per outage.
pub struct Link {
    device: String,
    port_name: String,
    outage_logged: bool,
    up_this_session: bool,
    port_back_logged: bool,
}

impl Link {
    /// The device answered on this port: the link is up. Logs "back" once
    /// after an outage that was logged.
    pub fn up(&mut self) {
        self.up_this_session = true;
        self.port_back_logged = false;
        if self.outage_logged {
            info!("{} on {} is back", self.device, self.port_name);
            self.outage_logged = false;
        }
    }

    /// The port opened. During a logged outage, say so once: the port is back
    /// even when the device does not answer yet. Returns whether it logged.
    fn port_opened(&mut self) -> bool {
        if self.outage_logged && !self.port_back_logged {
            info!("{} on {}: port open again, waiting for the device to answer", self.device, self.port_name);
            self.port_back_logged = true;
            return true;
        }
        false
    }

    /// The port did not open: a later open is news again.
    fn port_missing(&mut self) {
        self.port_back_logged = false;
    }
}

/// Open `port_name` with `spec`.
pub fn open(port_name: &str, spec: &PortSpec) -> Result<Box<dyn SerialPort>, String> {
    serialport::new(port_name, spec.baud)
        .data_bits(serialport::DataBits::Eight)
        .parity(serialport::Parity::None)
        .stop_bits(serialport::StopBits::One)
        .flow_control(spec.flow_control)
        .timeout(spec.timeout)
        .open()
        .map_err(|e| e.to_string())
}

/// Run `device` on `port_name` until the server stops, reopening the port
/// whenever it does not open or `session` returns `Reconnect`. `on_down` runs
/// every time the link is down (the device marks itself disconnected there).
/// Commands that arrive while the port is not open are dropped: a switch
/// command from before a power cycle is not one to carry out after it.
pub fn run<C>(
    device: &str,
    port_name: &str,
    spec: PortSpec,
    cmd_rx: &mpsc::Receiver<C>,
    on_down: impl FnMut(),
    session: impl FnMut(Box<dyn SerialPort>, &mut Link) -> SessionEnd,
) {
    run_with(device, port_name, || open(port_name, &spec), RECONNECT_DELAY, cmd_rx, on_down, session)
}

/// `run` with the way of opening and the delay given, so a test can drive it.
pub(crate) fn run_with<C>(
    device: &str,
    port_name: &str,
    mut open_port: impl FnMut() -> Result<Box<dyn SerialPort>, String>,
    delay: Duration,
    cmd_rx: &mpsc::Receiver<C>,
    mut on_down: impl FnMut(),
    mut session: impl FnMut(Box<dyn SerialPort>, &mut Link) -> SessionEnd,
) {
    info!("{} serial thread started for {}", device, port_name);
    let mut link = Link {
        device: device.to_string(),
        port_name: port_name.to_string(),
        outage_logged: false,
        up_this_session: false,
        port_back_logged: false,
    };
    loop {
        match open_port() {
            Ok(port) => {
                debug!("{} port {} opened", device, port_name);
                link.port_opened();
                link.up_this_session = false;
                match session(port, &mut link) {
                    SessionEnd::Shutdown => {
                        on_down();
                        info!("{} serial thread stopping ({})", device, port_name);
                        return;
                    }
                    SessionEnd::Reconnect(why) => {
                        on_down();
                        if link.outage_logged {
                            debug!("{} on {} still down: {}", device, port_name, why);
                        } else {
                            warn!("{} on {} lost: {} (reopening, then every {} s)",
                                device, port_name, why, delay.as_secs());
                            link.outage_logged = true;
                        }
                        // A link that worked and went: straight back to open, a
                        // re-plugged cable is there at once and a missing one
                        // fails into the wait below. A port that opened but
                        // never answered (the device off on its USB): wait
                        // first, so this does not turn into a loop of opens.
                        if !link.up_this_session && wait_or_stop(device, port_name, delay, cmd_rx) {
                            return;
                        }
                        continue;
                    }
                }
            }
            Err(e) => {
                on_down();
                link.port_missing();
                if link.outage_logged {
                    debug!("{} on {} still not reachable: {}", device, port_name, e);
                } else {
                    warn!("{} on {} not reachable: {} (retrying every {} s)",
                        device, port_name, e, delay.as_secs());
                    link.outage_logged = true;
                }
                if wait_or_stop(device, port_name, delay, cmd_rx) {
                    return;
                }
            }
        }
    }
}

/// Wait the whole `delay`, dropping every command that comes in meanwhile, and
/// say whether the server stopped. A command does not cut the wait short:
/// otherwise a client that keeps sending (a VFO the antenna follows) sets the
/// pace of opening the port.
fn wait_or_stop<C>(device: &str, port_name: &str, delay: Duration, cmd_rx: &mpsc::Receiver<C>) -> bool {
    let until = Instant::now() + delay;
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        match cmd_rx.recv_timeout(left) {
            Ok(_) => {} // dropped
            Err(mpsc::RecvTimeoutError::Timeout) => return false,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                info!("{} serial thread stopping ({}), command channel closed while waiting", device, port_name);
                return true;
            }
        }
    }
}

#[cfg(test)]
impl Link {
    /// A link for a device's own session test.
    pub(crate) fn for_test() -> Self {
        Link {
            device: "Test".to_string(),
            port_name: "COM99".to_string(),
            outage_logged: false,
            up_this_session: false,
            port_back_logged: false,
        }
    }
}

/// Run a device's session in a thread of its own and give its end, or `None`
/// when it has not ended after 10 s. A session that misses a lost port keeps
/// polling forever; this turns that into a failed test instead of a hang.
#[cfg(test)]
pub(crate) fn session_end_within_10s(session: impl FnOnce() -> SessionEnd + Send + 'static) -> Option<SessionEnd> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(session());
    });
    rx.recv_timeout(Duration::from_secs(10)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_serial::FakeSerial;
    use std::cell::RefCell;

    /// A port that is not there at first, then is, then goes: the block keeps
    /// trying, runs a session each time the port opens, marks the link down
    /// every time, and ends when the session says the server stops.
    #[test]
    fn a_port_that_comes_and_goes_is_opened_again() {
        let (tx, rx) = mpsc::channel::<()>();
        let opens = RefCell::new(0u32);
        let downs = RefCell::new(0u32);
        let sessions = RefCell::new(0u32);
        run_with(
            "Test",
            "COM99",
            || {
                *opens.borrow_mut() += 1;
                match *opens.borrow() {
                    1 | 2 => Err("not there".to_string()),
                    _ => Ok(FakeSerial::new().boxed()),
                }
            },
            Duration::from_millis(1),
            &rx,
            || *downs.borrow_mut() += 1,
            |_port, link| {
                *sessions.borrow_mut() += 1;
                link.up();
                if *sessions.borrow() == 1 { SessionEnd::Reconnect("pulled".to_string()) } else { SessionEnd::Shutdown }
            },
        );
        drop(tx);
        assert_eq!(*opens.borrow(), 4, "two failed opens, one session that lost the link, one that ended");
        assert_eq!(*sessions.borrow(), 2);
        assert_eq!(*downs.borrow(), 4, "down after each failed open, after the lost link and at the end");
    }

    /// The server stopping while the port is missing ends the thread instead
    /// of retrying forever.
    #[test]
    fn a_closed_channel_ends_the_wait_for_a_missing_port() {
        let (tx, rx) = mpsc::channel::<()>();
        drop(tx);
        let opens = RefCell::new(0u32);
        run_with(
            "Test",
            "COM99",
            || {
                *opens.borrow_mut() += 1;
                Err("not there".to_string())
            },
            Duration::from_millis(1),
            &rx,
            || {},
            |_port, _link| SessionEnd::Shutdown,
        );
        assert_eq!(*opens.borrow(), 1);
    }

    /// Commands given while the port is missing are not carried out later.
    #[test]
    fn commands_while_the_port_is_missing_are_dropped() {
        let (tx, rx) = mpsc::channel::<u8>();
        tx.send(1).unwrap();
        tx.send(2).unwrap();
        let opens = RefCell::new(0u32);
        let seen = RefCell::new(Vec::new());
        run_with(
            "Test",
            "COM99",
            || {
                *opens.borrow_mut() += 1;
                if *opens.borrow() == 1 { Err("not there".to_string()) } else { Ok(FakeSerial::new().boxed()) }
            },
            Duration::from_millis(1),
            &rx,
            || {},
            |_port, _link| {
                while let Ok(c) = rx.try_recv() {
                    seen.borrow_mut().push(c);
                }
                SessionEnd::Shutdown
            },
        );
        assert!(seen.borrow().is_empty(), "the commands from before the port came were dropped");
        drop(tx);
    }

    /// Commands that keep coming while the port is missing do not cut the wait
    /// short: the next open comes after the whole delay.
    #[test]
    fn commands_do_not_shorten_the_wait() {
        let (tx, rx) = mpsc::channel::<u8>();
        let sender = std::thread::spawn(move || {
            for i in 0..20u8 {
                if tx.send(i).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let start = Instant::now();
        let opens = RefCell::new(0u32);
        run_with(
            "Test",
            "COM99",
            || {
                *opens.borrow_mut() += 1;
                if *opens.borrow() == 1 { Err("not there".to_string()) } else { Ok(FakeSerial::new().boxed()) }
            },
            Duration::from_millis(150),
            &rx,
            || {},
            |_port, _link| SessionEnd::Shutdown,
        );
        assert!(start.elapsed() >= Duration::from_millis(150), "opened again after {:?}", start.elapsed());
        drop(rx);
        sender.join().unwrap();
    }

    /// "Port open again" is said once per outage: not without an outage, not on
    /// every reopen of a device that stays silent, and again after the port was
    /// missing in between or the device was back.
    #[test]
    fn port_open_again_is_said_once_per_outage() {
        let mut link = Link::for_test();
        assert!(!link.port_opened(), "no outage, nothing to say");
        link.outage_logged = true;
        assert!(link.port_opened(), "the first open in an outage is said");
        assert!(!link.port_opened(), "a silent device reopened: not again");
        link.port_missing();
        assert!(link.port_opened(), "the port was gone in between: said again");
        link.up();
        link.outage_logged = true;
        assert!(link.port_opened(), "a new outage after the device was back: said again");
    }
}
