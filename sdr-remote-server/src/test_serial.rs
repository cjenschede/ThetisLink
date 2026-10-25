// SPDX-License-Identifier: GPL-2.0-or-later

//! One fake serial port, for every test in this crate that needs one.
//!
//! It exists because two of the places a radio is actually spoken to had no
//! test at all: `cw_key_line` (four surviving mutants, among them "write the
//! opposite of what the key says") and `yaesu::memory::read_ftx1_ex_value`
//! (where "no answer" turning into "no such address" takes both the abort
//! guard and the retry out of the menu scan, with every scan test still
//! green). Both were untested for the same reason - the code path ends at a
//! `SerialPort`, and there was no way to be one.
//!
//! One module rather than one per test file: the trait has two dozen methods,
//! and a second hand-written copy is a second thing to keep in step
//! (working rule 15). Tests script what the port answers and read back what
//! was written to it.

#![cfg(test)]

use serialport::{FlowControl, SerialPort};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What the port did and what it was told to do, shared with the test so it
/// survives the port being dropped - which is the moment several of these
/// paths care about.
#[derive(Default)]
pub struct SerialLog {
    /// Every byte written to the port, in order.
    pub written: Vec<u8>,
    /// Every `write_request_to_send` level, in order. RTS is the CW key.
    pub rts: Vec<bool>,
    /// Every `write_data_terminal_ready` level, in order.
    pub dtr: Vec<bool>,
    /// What the port still has to hand out, oldest first.
    pub to_read: Vec<u8>,
    /// From this write on, every write fails. `None` = the port stays healthy.
    pub fail_writes_from: Option<usize>,
    /// What the radio answers, one per command written to it. A real radio
    /// speaks only after it is spoken to, and code that clears the line before
    /// asking - which the EX read does, on purpose - would never see an answer
    /// that was already waiting.
    pub replies: std::collections::VecDeque<String>,
    /// Answer after ANY write rather than only after one ending in `;`.
    ///
    /// The semicolon is a Yaesu CAT convention and it was the only rule this
    /// fake knew, which made `replies` useless to a device that speaks binary.
    /// Amplitec sends twelve raw bytes, so its tests could not express "the
    /// answer arrives after the question" - and that is exactly the ordering a
    /// buffer-clearing test has to be able to say.
    pub reply_to_any_write: bool,
    writes: usize,
}

impl SerialLog {
    /// What was written, as text - the form a CAT test wants to assert on.
    pub fn written_text(&self) -> String {
        String::from_utf8_lossy(&self.written).into_owned()
    }
}

/// A port under the test's control. Clones share one log, so a clone can be
/// handed to the code under test while the test keeps reading the original.
#[derive(Clone)]
pub struct FakeSerial(pub Arc<Mutex<SerialLog>>);

impl FakeSerial {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(SerialLog::default())))
    }

    /// A port that answers with this text, once, in order.
    pub fn answering(text: &str) -> Self {
        let f = Self::new();
        f.0.lock().unwrap().to_read = text.as_bytes().to_vec();
        f
    }

    /// A radio that answers each command it is given, in order. A command
    /// beyond the list is met with silence.
    pub fn answering_each(replies: &[&str]) -> Self {
        let f = Self::new();
        f.0.lock().unwrap().replies = replies.iter().map(|r| (*r).to_string()).collect();
        f
    }

    /// A device that answers every command, in order, whatever the command
    /// looks like. For binary protocols, where there is no terminator to
    /// recognise.
    pub fn answering_each_command(replies: &[&str]) -> Self {
        let f = Self::answering_each(replies);
        f.0.lock().unwrap().reply_to_any_write = true;
        f
    }

    /// A port that stops accepting writes after `n` of them.
    pub fn failing_writes_from(n: usize) -> Self {
        let f = Self::new();
        f.0.lock().unwrap().fail_writes_from = Some(n);
        f
    }

    pub fn log(&self) -> std::sync::MutexGuard<'_, SerialLog> {
        self.0.lock().unwrap()
    }

    /// Hand more bytes to the port, as if the radio had just answered.
    pub fn feed(&self, text: &str) {
        self.0.lock().unwrap().to_read.extend_from_slice(text.as_bytes());
    }

    pub fn boxed(&self) -> Box<dyn SerialPort> {
        Box::new(self.clone())
    }

    fn step(&self) -> serialport::Result<()> {
        let mut g = self.0.lock().unwrap();
        g.writes += 1;
        match g.fail_writes_from {
            Some(n) if g.writes > n => Err(serialport::Error::new(
                serialport::ErrorKind::NoDevice,
                "the port went away",
            )),
            _ => Ok(()),
        }
    }
}

impl std::io::Read for FakeSerial {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let mut g = self.0.lock().unwrap();
        if g.to_read.is_empty() {
            // A port with nothing to say times out; it does not return zero,
            // and the difference is exactly what "no answer" means here.
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "nothing waiting"));
        }
        let n = out.len().min(g.to_read.len());
        out[..n].copy_from_slice(&g.to_read[..n]);
        g.to_read.drain(..n);
        Ok(n)
    }
}

impl std::io::Write for FakeSerial {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.step().map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let mut g = self.0.lock().unwrap();
        g.written.extend_from_slice(b);
        // The radio answers what it was asked, if it has an answer left.
        if g.reply_to_any_write || b.ends_with(b";") {
            if let Some(reply) = g.replies.pop_front() {
                g.to_read.extend_from_slice(reply.as_bytes());
            }
        }
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl SerialPort for FakeSerial {
    fn name(&self) -> Option<String> {
        Some("FAKE".into())
    }
    fn baud_rate(&self) -> serialport::Result<u32> {
        Ok(38400)
    }
    fn data_bits(&self) -> serialport::Result<serialport::DataBits> {
        Ok(serialport::DataBits::Eight)
    }
    fn flow_control(&self) -> serialport::Result<FlowControl> {
        Ok(FlowControl::None)
    }
    fn parity(&self) -> serialport::Result<serialport::Parity> {
        Ok(serialport::Parity::None)
    }
    fn stop_bits(&self) -> serialport::Result<serialport::StopBits> {
        Ok(serialport::StopBits::One)
    }
    fn timeout(&self) -> Duration {
        Duration::from_millis(100)
    }
    fn set_baud_rate(&mut self, _: u32) -> serialport::Result<()> {
        Ok(())
    }
    fn set_data_bits(&mut self, _: serialport::DataBits) -> serialport::Result<()> {
        Ok(())
    }
    fn set_flow_control(&mut self, _: FlowControl) -> serialport::Result<()> {
        Ok(())
    }
    fn set_parity(&mut self, _: serialport::Parity) -> serialport::Result<()> {
        Ok(())
    }
    fn set_stop_bits(&mut self, _: serialport::StopBits) -> serialport::Result<()> {
        Ok(())
    }
    fn set_timeout(&mut self, _: Duration) -> serialport::Result<()> {
        Ok(())
    }
    fn write_request_to_send(&mut self, level: bool) -> serialport::Result<()> {
        self.step()?;
        self.0.lock().unwrap().rts.push(level);
        Ok(())
    }
    fn write_data_terminal_ready(&mut self, level: bool) -> serialport::Result<()> {
        self.step()?;
        self.0.lock().unwrap().dtr.push(level);
        Ok(())
    }
    fn read_clear_to_send(&mut self) -> serialport::Result<bool> {
        Ok(false)
    }
    fn read_data_set_ready(&mut self) -> serialport::Result<bool> {
        Ok(false)
    }
    fn read_ring_indicator(&mut self) -> serialport::Result<bool> {
        Ok(false)
    }
    fn read_carrier_detect(&mut self) -> serialport::Result<bool> {
        Ok(false)
    }
    fn bytes_to_read(&self) -> serialport::Result<u32> {
        Ok(self.0.lock().unwrap().to_read.len() as u32)
    }
    fn bytes_to_write(&self) -> serialport::Result<u32> {
        Ok(0)
    }
    fn clear(&self, buffer: serialport::ClearBuffer) -> serialport::Result<()> {
        if matches!(buffer, serialport::ClearBuffer::Input | serialport::ClearBuffer::All) {
            self.0.lock().unwrap().to_read.clear();
        }
        Ok(())
    }
    fn try_clone(&self) -> serialport::Result<Box<dyn SerialPort>> {
        Ok(Box::new(self.clone()))
    }
    fn set_break(&self) -> serialport::Result<()> {
        Ok(())
    }
    fn clear_break(&self) -> serialport::Result<()> {
        Ok(())
    }
}

/// The fake's own tests. It had none, and seven tests in two other files lean
/// on it - so whatever it does wrong, they inherit.
///
/// A reviewer found the hole that made this worth writing: with
/// `if true || b.ends_with(b";")` in the write path, every one of the 382 tests
/// in this workspace stayed green. The OFF state of `reply_to_any_write` was
/// observable by nothing at all, which means nothing was holding the semicolon
/// rule that the Yaesu tests depend on.
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// Without the flag, only a command ending in `;` draws an answer. That is
    /// the Yaesu CAT convention, and three tests in `yaesu/memory.rs` need it:
    /// the EX read clears the line before asking, on purpose, so an answer that
    /// was already waiting would make those tests pass for the wrong reason.
    #[test]
    fn by_default_only_a_semicolon_command_is_answered() {
        let f = FakeSerial::answering_each(&["EX0101012;"]);
        let mut port = f.boxed();

        port.write_all(b"no terminator here").unwrap();
        let mut buf = [0u8; 32];
        assert!(port.read(&mut buf).is_err(), "nothing was asked, so nothing answers");
        assert_eq!(f.log().replies.len(), 1, "and the reply is still queued");

        port.write_all(b"EX0101;").unwrap();
        let n = port.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"EX0101012;");
    }

    /// With the flag, any write draws the next answer. A binary device sends
    /// twelve raw bytes and ends in no terminator at all.
    #[test]
    fn with_the_flag_any_command_is_answered() {
        let f = FakeSerial::answering_each_command(&["AAAACAAE"]);
        let mut port = f.boxed();

        port.write_all(&[0x01, 0x0A, 0x00]).unwrap();
        let mut buf = [0u8; 32];
        let n = port.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"AAAACAAE");
    }

    /// The two constructors differ in exactly that flag and nothing else.
    #[test]
    fn the_two_constructors_differ_only_in_the_flag() {
        let plain = FakeSerial::answering_each(&["x;"]);
        let any = FakeSerial::answering_each_command(&["x;"]);

        assert!(!plain.log().reply_to_any_write);
        assert!(any.log().reply_to_any_write);
        assert_eq!(plain.log().replies, any.log().replies);
    }

    /// Answers are handed out in order, one per command, and silence follows
    /// when the list runs out.
    #[test]
    fn answers_come_in_order_and_then_stop() {
        let f = FakeSerial::answering_each_command(&["first", "second"]);
        let mut port = f.boxed();
        let mut buf = [0u8; 32];

        port.write_all(b"1").unwrap();
        let n = port.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"first");

        port.write_all(b"2").unwrap();
        let n = port.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"second");

        port.write_all(b"3").unwrap();
        assert!(port.read(&mut buf).is_err(), "a command beyond the list is met with silence");
    }

    /// A port with nothing to say times out rather than returning zero bytes.
    /// Several paths under test tell those two apart, and one of them has a
    /// guard that can only fire on the zero it never gets.
    #[test]
    fn an_empty_port_times_out_and_does_not_return_zero() {
        let f = FakeSerial::new();
        let mut port = f.boxed();
        let mut buf = [0u8; 8];

        let e = port.read(&mut buf).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::TimedOut);
    }

    /// Clearing the input throws away what was waiting and nothing else.
    #[test]
    fn clearing_the_input_drops_what_was_waiting() {
        let f = FakeSerial::answering("stale bytes");
        let port = f.boxed();

        assert!(!f.log().to_read.is_empty());
        port.clear(serialport::ClearBuffer::Input).unwrap();
        assert!(f.log().to_read.is_empty());

        // Output-only clearing leaves the input alone; amplitec's drain asks for
        // Input, and a test that cannot tell them apart would pass either way.
        f.feed("more");
        port.clear(serialport::ClearBuffer::Output).unwrap();
        assert_eq!(f.log().to_read, b"more".to_vec());
    }

    /// Writes stop failing only from the number given, and the count is of
    /// writes and not of bytes.
    #[test]
    fn a_failing_port_fails_from_the_write_it_was_told_to() {
        let f = FakeSerial::failing_writes_from(2);
        let mut port = f.boxed();

        assert!(port.write_all(b"one").is_ok());
        assert!(port.write_all(b"two").is_ok());
        assert!(port.write_all(b"three").is_err(), "the third write is past the limit");
    }

    /// Every write is kept, in order - not just the last one.
    ///
    /// Four tests in three files judge `written`, and all four write exactly
    /// once, so keeping everything and keeping only the last write looked the
    /// same to them. A reviewer made the fake overwrite instead of append and all
    /// 436 tests stayed green (2026-09-25). It matters the day a sequence is
    /// tested - and the first one is already on the list: amplitec's session
    /// loop sends a scan and then a switch command.
    #[test]
    fn every_write_is_kept_in_order() {
        let f = FakeSerial::new();
        let mut port = f.boxed();
        port.write_all(b"FIRST;").unwrap();
        port.write_all(&[0x01, 0x0A]).unwrap();
        port.write_all(b"LAST;").unwrap();

        let mut expected = b"FIRST;".to_vec();
        expected.extend_from_slice(&[0x01, 0x0A]);
        expected.extend_from_slice(b"LAST;");
        assert_eq!(f.log().written, expected);
    }

    /// A clone shares the log, which is what lets a test keep reading while the
    /// code under test owns the port.
    #[test]
    fn a_clone_shares_the_log() {
        let f = FakeSerial::new();
        let mut port = f.boxed();
        port.write_all(b"seen by both").unwrap();
        assert_eq!(f.log().written_text(), "seen by both");
    }
}
