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
        if b.ends_with(b";") {
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
