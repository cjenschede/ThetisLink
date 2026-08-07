// SPDX-License-Identifier: GPL-2.0-or-later
//! FT-991A EX-menu (extended-menu) read/write over CAT and the USB-routing
//! snapshot restore (SSB/AM MIC+PORT SELECT). Extracted verbatim from
//! `yaesu/mod.rs` - pure relocation, no behaviour/CAT/timing change.
//! `use super::*;` pulls in the shared imports (serialport, Duration, info/warn),
//! the CAT query helper and the Ft991aUsbRouting* types; `pub(super)` keeps the
//! read + restore callable from the snapshot capture / poll loop in the parent.

use super::*;

fn parse_ex_menu_value(menu: u16, response: &str) -> Option<String> {
    let prefix = format!("EX{:03}", menu);
    let start = response.find(&prefix)?;
    let rest = &response[start + prefix.len()..];
    let end = rest.find(';')?;
    let value = rest[..end].trim();
    if value.is_empty() { None } else { Some(value.to_string()) }
}

pub(super) fn read_ex_menu_value(
    port: &mut Box<dyn serialport::SerialPort>,
    prefix: &str,
    menu: u16,
    label: &str,
) -> Option<String> {
    let response = cat_query(port, &format!("EX{:03};", menu));
    if response.trim().is_empty() || response.contains("?;") {
        warn!(
            "{} 991A USB routing snapshot: EX{:03} {} read failed ({:?})",
            prefix, menu, label, response
        );
        return None;
    }
    let value = parse_ex_menu_value(menu, &response);
    if value.is_none() {
        warn!(
            "{} 991A USB routing snapshot: EX{:03} {} parse failed ({:?})",
            prefix, menu, label, response
        );
    }
    value
}

fn write_ex_menu_value(
    port: &mut Box<dyn serialport::SerialPort>,
    prefix: &str,
    menu: u16,
    value: &str,
    label: &str,
) -> bool {
    let cmd = format!("EX{:03}{};", menu, value);
    match port.write_all(cmd.as_bytes()) {
        Ok(()) => {
            log::debug!("{} restored {} via {}", prefix, label, cmd);
            std::thread::sleep(Duration::from_millis(30));
            true
        }
        Err(e) => {
            warn!("{} restore {} failed via {}: {}", prefix, label, cmd, e);
            false
        }
    }
}

pub(super) fn restore_991a_usb_routing_snapshot(
    port: &mut Box<dyn serialport::SerialPort>,
    prefix: &str,
    snapshot: Option<&Ft991aUsbRoutingSnapshot>,
    scope: Ft991aUsbRoutingScope,
    reason: &str,
) {
    let Some(snapshot) = snapshot else {
        warn!("{} cannot restore 991A USB routing for {}: no session snapshot", prefix, reason);
        return;
    };

    let mut restored = 0usize;
    let mut skipped = 0usize;

    if matches!(scope, Ft991aUsbRoutingScope::Ssb | Ft991aUsbRoutingScope::All) {
        if let Some(value) = snapshot.ssb_mic_select.as_deref() {
            if write_ex_menu_value(port, prefix, 106, value, "SSB MIC SELECT") { restored += 1; }
        } else { skipped += 1; }
        if let Some(value) = snapshot.ssb_port_select.as_deref() {
            if write_ex_menu_value(port, prefix, 109, value, "SSB PORT SELECT") { restored += 1; }
        } else { skipped += 1; }
    }

    if matches!(scope, Ft991aUsbRoutingScope::Am | Ft991aUsbRoutingScope::All) {
        if let Some(value) = snapshot.am_mic_select.as_deref() {
            if write_ex_menu_value(port, prefix, 45, value, "AM MIC SELECT") { restored += 1; }
        } else { skipped += 1; }
        if let Some(value) = snapshot.am_port_select.as_deref() {
            if write_ex_menu_value(port, prefix, 48, value, "AM PORT SELECT") { restored += 1; }
        } else { skipped += 1; }
    }

    if skipped > 0 {
        warn!(
            "{} 991A USB routing restore for {} partial: restored {}, skipped {} missing snapshot values",
            prefix, reason, restored, skipped
        );
    } else {
        info!("{} 991A USB routing restored from session snapshot ({})", prefix, reason);
    }
}
