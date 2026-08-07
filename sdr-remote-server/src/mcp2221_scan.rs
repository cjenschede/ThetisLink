// SPDX-License-Identifier: GPL-2.0-or-later

//! MCP2221A USB enumeration helper.
//!
//! Wraps `mcp2221_hal::MCP2221::list_devices()` (a ThetisLink-fork addition)
//! so the rest of the server only sees a small `BoardInfo` struct rather than
//! the broader hidapi listing. The result is what the server UI shows in the
//! "Detected MCP2221A boards" picker, and what the config-file references via
//! `tuner1_mcp_serial` / `tuner2_mcp_serial`.

use anyhow::{anyhow, Result};
use mcp2221_hal::MCP2221;

/// One detected MCP2221A on the local USB bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardInfo {
    /// USB serial number - pass into `Mcp2221Debug::with_serial` /
    /// `TunerBridge::with_serial` to target this exact board. Empty string
    /// if the board has no serial-number assigned yet (factory-default
    /// often collides, requires `usb_change_serial_number` to make unique).
    pub serial_number: String,
    /// USB product string, usually "MCP2221 USB-I2C/UART Combo".
    pub product_string: String,
    /// OS-level HID path - unique per physical board even when the serial
    /// number is empty. Used to target a specific anonymous board for
    /// one-shot serial-number programming.
    pub path: String,
}

impl BoardInfo {
    /// Best-effort display label combining serial and product strings.
    /// Falls back to "(no serial)" when the board has no serial set yet.
    pub fn label(&self) -> String {
        if self.serial_number.is_empty() {
            format!("(no serial) - {}", self.product_string)
        } else {
            format!("{}  -  {}", self.serial_number, self.product_string)
        }
    }

    /// Classify a board based on its (programmed) USB
    /// serial. ThetisLink convention: `tun_<name>` = tuner board,
    /// `rot_<name>` = rotor board, otherwise unprogrammed.
    pub fn kind(&self) -> BoardKind {
        BoardKind::from_serial(&self.serial_number)
    }
}

/// Functional classification of an MCP2221A board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardKind {
    /// Board programmed for tuner control (`tun_*` prefix).
    Tuner,
    /// Board programmed for rotor control (`rot_*` prefix).
    /// Runtime support arrives in phase 4 - the wizard already accepts
    /// the choice but Add is blocked in v2.0.5.
    Rotor,
    /// No `tun_`/`rot_` prefix found. A fresh-from-factory
    /// Adafruit board or a board whose serial was set manually
    /// without following the ThetisLink convention.
    Unprogrammed,
}

impl BoardKind {
    /// Prefix marker for tuners.
    pub const TUNER_PREFIX: &'static str = "tun_";
    /// Prefix marker for rotors.
    pub const ROTOR_PREFIX: &'static str = "rot_";

    pub fn from_serial(serial: &str) -> Self {
        if serial.starts_with(Self::TUNER_PREFIX) {
            BoardKind::Tuner
        } else if serial.starts_with(Self::ROTOR_PREFIX) {
            BoardKind::Rotor
        } else {
            BoardKind::Unprogrammed
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BoardKind::Tuner => "Tuner",
            BoardKind::Rotor => "Rotor",
            BoardKind::Unprogrammed => "Ongeprogrammeerd",
        }
    }
}

/// Enumerate every MCP2221A currently visible on the USB bus.
///
/// Returns an empty Vec if no boards are attached (not an error). Only
/// surfaces an error when the HID layer itself fails to initialise - that
/// usually means the OS HID subsystem is unhappy, not a missing board.
pub fn list_boards() -> Result<Vec<BoardInfo>> {
    let listings = MCP2221::list_devices()?;
    Ok(listings
        .into_iter()
        .map(|l| BoardInfo {
            serial_number: l.serial_number,
            product_string: l.product_string,
            path: l.path,
        })
        .collect())
}

/// Open the board at the given OS-level HID `path`, overwrite its USB
/// serial-number string, then reset the chip so it re-enumerates with the
/// new identity. Used by the UI to make two factory-anonymous boards
/// individually addressable before assigning them to tuner slots.
///
/// The caller is expected to refresh `list_boards()` afterwards (the host
/// needs ~half a second to see the new descriptor).
pub fn program_serial_at_path(path: &str, new_serial: &str) -> Result<()> {
    use mcp2221_hal::settings::DeviceString;
    let new_serial_owned = new_serial.to_string();
    let ds = DeviceString::try_from(new_serial_owned)
        .map_err(|e| anyhow!("invalid serial: {}", e))?;
    let dev = MCP2221::connect_with_path(path)
        .map_err(|e| anyhow!("open by path failed: {:?}", e))?;

    // Critical: the MCP2221A ships with the "CDC serial-number enumeration"
    // flag cleared by default - so even after we write the serial string to
    // flash, USB enumeration won't advertise it and hidapi keeps reporting
    // "(no serial)". Read the chip settings, flip the bit, write back, THEN
    // write the serial. Both go to flash and survive a reset.
    let mut chip = dev
        .flash_read_chip_settings()
        .map_err(|e| anyhow!("flash_read_chip_settings failed: {:?}", e))?;
    if !chip.cdc_serial_number_enumeration_enabled {
        chip.cdc_serial_number_enumeration_enabled = true;
        dev.flash_write_chip_settings(chip)
            .map_err(|e| anyhow!("flash_write_chip_settings failed: {:?}", e))?;
    }

    dev.usb_change_serial_number(&ds)
        .map_err(|e| anyhow!("usb_change_serial_number failed: {:?}", e))?;
    dev.reset()
        .map_err(|e| anyhow!("reset (re-enumerate) failed: {:?}", e))?;
    Ok(())
}
