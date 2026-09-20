// SPDX-License-Identifier: GPL-2.0-or-later

//! The key line of a Yaesu: RTS on the radio's second USB COM port, which
//! the radio reads as its CW key when menu PC KEYING is set to RTS.
//!
//! Step 6 of `docs/internal/DESIGN-remote-cw.md`. Measurement 3 (2026-09-18):
//! the FT-991A keys on RTS of the port next to its CAT port, not on the CAT
//! port itself. That has to be so: the CAT port is opened with
//! `FlowControl::Hardware`, which in serialport 4.9 on Windows holds RTS on
//! for as long as the port is open.
//!
//! This port is opened without handshake and with DTR off, so both lines are
//! low from the first moment; they go low again when the line is dropped.
//! A Yaesu has no keying watchdog of its own - an RTS left high transmits -
//! so that low is the last thing that happens to this port, always.

use serialport::{FlowControl, SerialPort, SerialPortType};
use std::time::Duration;

/// A COM port as USB describes it: name, and vendor, product and serial
/// number when it is a USB port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortDesc {
    pub name: String,
    pub usb: Option<(u16, u16, Option<String>)>,
}

/// Every COM port on this machine, as `PortDesc`.
pub fn ports_now() -> Vec<PortDesc> {
    serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|p| PortDesc {
            name: p.port_name,
            usb: match p.port_type {
                SerialPortType::UsbPort(u) => Some((u.vid, u.pid, u.serial_number)),
                _ => None,
            },
        })
        .collect()
}

/// The key port of the radio whose CAT port is `cat`: the other port of the
/// same USB device. A radio's two ports share vendor and product id; the
/// serial number, when Windows reports one for both, must match too. If
/// more than one port fits (two radios of the same model), or none does,
/// there is no answer - guessing a port here could key the wrong radio.
pub fn sibling_port(ports: &[PortDesc], cat: &str) -> Option<String> {
    let cat_desc = ports.iter().find(|p| p.name.eq_ignore_ascii_case(cat))?;
    let (vid, pid, serial) = cat_desc.usb.clone()?;
    let same_device: Vec<&PortDesc> = ports
        .iter()
        .filter(|p| !p.name.eq_ignore_ascii_case(cat))
        .filter(|p| match &p.usb {
            Some((v, d, s)) => *v == vid && *d == pid && (serial.is_none() || s.is_none() || *s == serial),
            None => false,
        })
        .collect();
    match same_device.as_slice() {
        [one] => Some(one.name.clone()),
        _ => None,
    }
}

/// What `cw_output` needs from a key line - a trait so the handling of a line
/// that fails can be tested without a serial port.
pub trait KeyOut {
    fn name(&self) -> &str;
    fn set(&mut self, down: bool) -> anyhow::Result<()>;
    /// Put the line down and say whether that reached the port. `Drop` lowers
    /// too, but it throws its result away and it fires while the handle may
    /// already be broken - so every caller that needs to *know* the line is
    /// down asks here (review 2120A1: the exit line said "lowered" from inside
    /// the `if let`, one statement before the drop that does the writing).
    fn lower_now(&mut self) -> anyhow::Result<()>;
}

impl KeyOut for KeyLine {
    fn name(&self) -> &str {
        KeyLine::name(self)
    }
    fn set(&mut self, down: bool) -> anyhow::Result<()> {
        KeyLine::set(self, down)
    }
    fn lower_now(&mut self) -> anyhow::Result<()> {
        KeyLine::lower_now(self)
    }
}

/// The open key line. Both lines low from the start and at the end.
pub struct KeyLine {
    name: String,
    port: Box<dyn SerialPort>,
}

impl KeyLine {
    /// A line over a port the caller supplies. Only for tests: this module's
    /// four surviving mutants all lived here, in the thinnest code of the
    /// whole chain, because nothing could reach `set` without a real radio
    /// (review 2120A1 - among them "write `!down`", which keys in the
    /// gaps and leaves RTS high after the last key-up).
    #[cfg(test)]
    pub fn over(name: &str, port: Box<dyn SerialPort>) -> Self {
        Self { name: name.to_string(), port }
    }

    pub fn open(name: &str) -> anyhow::Result<Self> {
        let port = serialport::new(name, 9600)
            .flow_control(FlowControl::None)
            .dtr_on_open(false)
            .timeout(Duration::from_millis(100))
            .open()?;
        let mut line = Self { name: name.to_string(), port };
        line.both_low();
        Ok(line)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Key down or up. A failed write leaves the line as the driver has it -
    /// and on a key-UP that is a transmitting radio, so the caller lowers
    /// explicitly with `lower_now` before it gives up on the handle. (Until
    /// build 152 this comment said "the drop tries again"; the drop does
    /// write, but on the same broken handle and it discards the result, so
    /// "again" was a promise nothing kept.)
    pub fn set(&mut self, down: bool) -> anyhow::Result<()> {
        self.port.write_request_to_send(down)?;
        Ok(())
    }

    /// Both lines down, and say so. RTS first: that is the one that keys.
    pub fn lower_now(&mut self) -> anyhow::Result<()> {
        let rts = self.port.write_request_to_send(false);
        let dtr = self.port.write_data_terminal_ready(false);
        rts?;
        dtr?;
        Ok(())
    }

    fn both_low(&mut self) {
        let _ = self.lower_now();
    }
}

impl Drop for KeyLine {
    fn drop(&mut self) {
        // The last net, and a silent one: by here the caller has had its turn
        // with `lower_now` and there is nobody left to tell.
        self.both_low();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_serial::FakeSerial;

    /// The whole point of this module, and until build 152 no test could see
    /// it: key down puts RTS HIGH and key up puts it LOW. A mutant writing
    /// `!down` survived 340 tests (review 2120A1) - it keys in the gaps
    /// and, worse, leaves the line high after the last key-up, which on a
    /// Yaesu is a radio that keeps transmitting.
    #[test]
    fn the_key_follows_the_paddle_and_not_its_opposite() {
        let fake = FakeSerial::new();
        let mut line = KeyLine::over("FAKE", fake.boxed());
        line.set(true).unwrap();
        line.set(false).unwrap();
        line.set(true).unwrap();
        assert_eq!(fake.log().rts, [true, false, true]);
        assert!(fake.log().dtr.is_empty(), "keying is RTS alone - DTR is not a key");
    }

    /// Dropping the line leaves nothing keyed, whatever it was doing.
    #[test]
    fn a_dropped_line_leaves_both_lines_low() {
        let fake = FakeSerial::new();
        {
            let mut line = KeyLine::over("FAKE", fake.boxed());
            line.set(true).unwrap();
        }
        assert_eq!(fake.log().rts, [true, false]);
        assert_eq!(fake.log().dtr, [false], "DTR down as well: some radios read it");
    }

    /// `lower_now` says what the port said. A silent lower is what made the
    /// exit line untrustworthy: it reported "lowered" from a write whose
    /// result was thrown away.
    #[test]
    fn lowering_reports_what_the_port_did() {
        let good = FakeSerial::new();
        let mut line = KeyLine::over("FAKE", good.boxed());
        assert!(line.lower_now().is_ok());
        assert_eq!(good.log().rts, [false]);

        // The first write is the key down; the port goes away after it.
        let gone = FakeSerial::failing_writes_from(1);
        let mut line = KeyLine::over("FAKE", gone.boxed());
        line.set(true).unwrap();
        assert!(line.lower_now().is_err(), "a lower that did not reach the port is not a lower");
        assert_eq!(gone.log().rts, [true], "and the line is still high - that is the radio transmitting");
    }

    fn usb(name: &str, vid: u16, pid: u16, serial: Option<&str>) -> PortDesc {
        PortDesc { name: name.into(), usb: Some((vid, pid, serial.map(String::from))) }
    }

    #[test]
    fn the_key_port_is_the_other_port_of_the_same_device() {
        let ports = vec![
            usb("COM6", 0x04D8, 0x00DD, Some("6")), // the paddle board
            usb("COM12", 0x10C4, 0xEA70, Some("0102")),
            usb("COM13", 0x10C4, 0xEA70, Some("0102")),
            PortDesc { name: "COM1".into(), usb: None },
        ];
        assert_eq!(sibling_port(&ports, "COM13").as_deref(), Some("COM12"));
        assert_eq!(sibling_port(&ports, "com13").as_deref(), Some("COM12"), "Windows port names are not case-sensitive");
        assert_eq!(sibling_port(&ports, "COM12").as_deref(), Some("COM13"));
    }

    /// The owner's server PC, 2026-09-18 (`yaesu-key-test list`): the
    /// FT-991A on COM13 (Enhanced, CAT) and COM12 (Standard), a second CP2105
    /// radio on COM25/COM27, and a pair of MCP2221A boards that report the
    /// same serial number - which is why the serial alone is not trusted to
    /// mean "same device" across vendors.
    #[test]
    fn the_owners_server_pc() {
        let ports = vec![
            usb("COM7", 0x0403, 0x6010, Some("7")),
            PortDesc { name: "COM29".into(), usb: None },
            usb("COM8", 0x0403, 0x6010, Some("7")),
            usb("COM12", 0x10C4, 0xEA70, Some("008DE7E0")),
            usb("COM13", 0x10C4, 0xEA70, Some("008DE7E0")),
            usb("COM26", 0x26AA, 0x0030, Some("0000000000001")),
            usb("COM9", 0x0403, 0x6001, Some("AB0MWYOVA")),
            usb("COM5", 0x04D8, 0x00DD, Some("ROT_YAESU")),
            usb("COM23", 0x04D8, 0x00DD, Some("TUN_JC")),
            usb("COM25", 0x10C4, 0xEA70, Some("01C7B758")),
            usb("COM24", 0x04D8, 0x00DD, Some("TUN_JC")),
            PortDesc { name: "COM3".into(), usb: None },
            usb("COM27", 0x10C4, 0xEA70, Some("01C7B758")),
            PortDesc { name: "COM4".into(), usb: None },
            PortDesc { name: "COM28".into(), usb: None },
            usb("COM6", 0x0483, 0x5740, Some("205C31604753")),
            usb("COM11", 0x0403, 0x6001, Some("AI040UUFA")),
        ];
        assert_eq!(sibling_port(&ports, "COM13").as_deref(), Some("COM12"), "the 991A: measured, it keys on COM12");
        assert_eq!(sibling_port(&ports, "COM25").as_deref(), Some("COM27"), "the other CP2105 radio");
    }

    #[test]
    fn two_radios_of_one_model_are_told_apart_by_serial_or_not_at_all() {
        let ports = vec![
            usb("COM12", 0x10C4, 0xEA70, Some("A")),
            usb("COM13", 0x10C4, 0xEA70, Some("A")),
            usb("COM20", 0x10C4, 0xEA70, Some("B")),
            usb("COM21", 0x10C4, 0xEA70, Some("B")),
        ];
        assert_eq!(sibling_port(&ports, "COM13").as_deref(), Some("COM12"));
        assert_eq!(sibling_port(&ports, "COM21").as_deref(), Some("COM20"));
        // Without serial numbers four ports of one kind cannot be paired: no answer.
        let blind: Vec<PortDesc> = ports.iter().map(|p| PortDesc { name: p.name.clone(), usb: p.usb.clone().map(|(v, d, _)| (v, d, None)) }).collect();
        assert_eq!(sibling_port(&blind, "COM13"), None);
    }

    #[test]
    fn no_answer_is_better_than_a_guess() {
        let ports = vec![usb("COM13", 0x10C4, 0xEA70, Some("A")), usb("COM6", 0x04D8, 0x00DD, None)];
        assert_eq!(sibling_port(&ports, "COM13"), None, "no second port of that device");
        assert_eq!(sibling_port(&ports, "COM99"), None, "the CAT port is not there at all");
        let not_usb = vec![PortDesc { name: "COM1".into(), usb: None }, PortDesc { name: "COM2".into(), usb: None }];
        assert_eq!(sibling_port(&not_usb, "COM1"), None, "a port USB knows nothing about has no sibling");
    }
}
