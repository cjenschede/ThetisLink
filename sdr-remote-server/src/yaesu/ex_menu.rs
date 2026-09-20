// SPDX-License-Identifier: GPL-2.0-or-later
//! FT-991A EX-menu (extended-menu) read/write over CAT and the USB-routing
//! snapshot restore (SSB/AM MIC+PORT SELECT). Extracted verbatim from
//! `yaesu/mod.rs` - pure relocation, no behaviour/CAT/timing change.
//! `use super::*;` pulls in the shared imports (serialport, Duration, info/warn),
//! the CAT query helper and the Ft991aUsbRouting* types; `pub(super)` keeps the
//! read + restore callable from the snapshot capture / poll loop in the parent.

use super::*;

fn parse_ex_menu_value(key: &str, response: &str) -> Option<String> {
    let prefix = format!("EX{}", key);
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
    read_ex_key(port, prefix, &format!("{:03}", menu), label)
}

/// The same read by menu key: three digits on the FT-991A, six on the FTX-1
/// (`EX` + p1p2p3). One shape for both - `EX<key>;` asks, `EX<key><value>;`
/// answers.
pub(super) fn read_ex_key(
    port: &mut Box<dyn serialport::SerialPort>,
    prefix: &str,
    key: &str,
    label: &str,
) -> Option<String> {
    let response = cat_query(port, &format!("EX{};", key));
    if response.trim().is_empty() || response.contains("?;") {
        warn!(
            "{} menu read: EX{} {} read failed ({:?})",
            prefix, key, label, response
        );
        return None;
    }
    let value = parse_ex_menu_value(key, &response);
    if value.is_none() {
        warn!(
            "{} menu read: EX{} {} parse failed ({:?})",
            prefix, key, label, response
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
    write_ex_key(port, prefix, &format!("{:03}", menu), value, label)
}

fn write_ex_key(
    port: &mut Box<dyn serialport::SerialPort>,
    prefix: &str,
    key: &str,
    value: &str,
    label: &str,
) -> bool {
    let cmd = format!("EX{}{};", key, value);
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

/// Where PC KEYING lives per model, and which value is RTS.
///
/// - FT-991A: menu 060, 0 OFF, 1 DAKY, 2 RTS, 3 DTR (CAT OM p. 9, OM p. 128).
/// - FTX-1: CW SETTING > MODE CW item 14, address `020114`, 0 OFF, 1 RTS,
///   2 DTR (CAT OM 2508-C, Table 3). Other values than the 991A's, so this is
///   per model and never per slot.
///
/// Both measured on the owner's station with `yaesu-key-test`: the key is RTS
/// of the port beside the radio's CAT port - 991A COM12 next to COM13
/// (2026-09-18), FTX-1 COM27 next to COM25 (2026-09-20, the TX and PTT lamps
/// following the dots).
pub(super) fn pc_keying_menu(model: RadioModel) -> (&'static str, &'static str) {
    if matches!(model, RadioModel::Ftx1) {
        ("020114", "1")
    } else {
        ("060", "2")
    }
}

/// What to do with menu 060 before a CW run, from what it reads now and the
/// original remembered earlier this server run. The rule of the routing
/// snapshot holds here too: a menu is changed only if its original is known.
/// Returns the original to remember - `Some` only once we have written RTS
/// over something - and whether RTS has to be written now.
///
/// An original is remembered only when RTS is actually written (review 316,
/// the review). It used to be remembered on the first look, so a radio found on
/// RTS remembered "2"; set to OFF by hand after that and prepared again (a
/// USB cable out and in), it got RTS written over the OFF - and at exit
/// there was nothing to put back.
pub(super) fn pc_keying_plan(rts: &str, read_now: Option<&str>, known_original: Option<&str>) -> Result<(Option<String>, bool), &'static str> {
    let now = read_now.ok_or("the PC KEYING menu could not be read; left as it is")?;
    if now == rts {
        // Nothing to write. What we remember stays what it was: an earlier
        // write of ours (after a reconnect this reads our own RTS), or nothing.
        return Ok((known_original.map(String::from), false));
    }
    // We write RTS over `now` - unless an earlier write of ours already
    // recorded what the operator had, which stays the value to go back to.
    let original = known_original.unwrap_or(now).to_string();
    Ok((Some(original), true))
}

/// The value to write back at exit, if any: what we wrote RTS over. `None`
/// when we never wrote (never read, or RTS before us).
pub(super) fn pc_keying_to_restore(original: Option<&str>) -> Option<&str> {
    original
}

/// Read menu 060 and set it to RTS if it is not. Returns the original to
/// remember; on an error nothing was written.
pub(super) fn prepare_pc_keying(
    port: &mut Box<dyn serialport::SerialPort>,
    prefix: &str,
    model: RadioModel,
    spill: &mut String,
    known_original: Option<&str>,
) -> Result<Option<String>, String> {
    let (key, rts) = pc_keying_menu(model);
    // Mid-session, like the late routing snapshot: poll answers may be
    // waiting. They go to the poll loop's buffer, not away - the scan taught
    // that (build 143); this path still threw them out until 149.
    super::memory::spill_waiting(port, spill);
    let now = read_ex_key(port, prefix, key, "PC KEYING");
    let (original, write) = pc_keying_plan(rts, now.as_deref(), known_original).map_err(String::from)?;
    if write {
        if !write_ex_key(port, prefix, key, rts, "PC KEYING") {
            return Err(format!("writing menu {} PC KEYING failed", key));
        }
        info!("{} CW: menu {} PC KEYING set to RTS (was {}); put back at exit", prefix, key, original.as_deref().unwrap_or("?"));
    } else {
        match original.as_deref() {
            Some(o) => info!("{} CW: menu {} PC KEYING already RTS (ours; original {})", prefix, key, o),
            None => info!("{} CW: menu {} PC KEYING already RTS (set before this server; left as it is at exit)", prefix, key),
        }
    }
    Ok(original)
}

/// Put menu 060 back to the operator's value at exit.
pub(super) fn restore_pc_keying(port: &mut Box<dyn serialport::SerialPort>, prefix: &str, model: RadioModel, original: Option<&str>) {
    if let Some(v) = pc_keying_to_restore(original) {
        let (key, _) = pc_keying_menu(model);
        if write_ex_key(port, prefix, key, v, "PC KEYING") {
            info!("{} CW: menu {} PC KEYING put back to {}", prefix, key, v);
        }
    }
}

#[cfg(test)]
mod pc_keying_tests {
    use super::{pc_keying_menu, pc_keying_plan, pc_keying_to_restore};
    use crate::yaesu::RadioModel;

    #[test]
    fn menu_060_is_set_to_rts_only_when_its_original_is_known() {
        assert_eq!(pc_keying_plan("2", Some("0"), None), Ok((Some("0".to_string()), true)), "OFF: remember OFF, write RTS");
        assert_eq!(pc_keying_plan("2", Some("2"), None), Ok((None, false)), "already RTS: nothing written, nothing of ours to remember");
        assert!(pc_keying_plan("2", None, None).is_err(), "not readable: not changed");
        assert!(pc_keying_plan("2", None, Some("0")).is_err(), "not readable now: not written blind, even with a known original");
        // After our own RTS and a reconnect: the remembered original stands.
        assert_eq!(pc_keying_plan("2", Some("2"), Some("1")), Ok((Some("1".to_string()), false)));
        // Changed back by hand since: write RTS again, original unchanged.
        assert_eq!(pc_keying_plan("2", Some("3"), Some("1")), Ok((Some("1".to_string()), true)));
    }

    /// Review 316: every write of RTS must leave something to put back.
    /// The path that broke it: the first run found 060 already on RTS (the
    /// owner's radio after measurement 3) and remembered "2"; the owner set it
    /// to OFF by hand; the next preparation wrote RTS - and at exit there was
    /// nothing to restore, so the radio stayed on RTS.
    #[test]
    fn every_write_of_rts_leaves_something_to_put_back() {
        let values = [None, Some("0"), Some("1"), Some("2"), Some("3")];
        for now in values {
            for known in [None, Some("0"), Some("1"), Some("3")] {
                if let Ok((original, write)) = pc_keying_plan("2", now, known) {
                    if write {
                        assert!(pc_keying_to_restore(original.as_deref()).is_some(), "now {now:?}, known {known:?}: RTS written, nothing to put back");
                    }
                }
            }
        }
        // The owner's path, step by step.
        let (first, wrote) = pc_keying_plan("2", Some("2"), None).unwrap();
        assert!(!wrote && first.is_none(), "already RTS: nothing written, nothing of ours to remember");
        let (second, wrote) = pc_keying_plan("2", Some("0"), first.as_deref()).unwrap();
        assert!(wrote);
        assert_eq!(pc_keying_to_restore(second.as_deref()), Some("0"), "OFF by hand, then our RTS: OFF goes back");
    }

    #[test]
    fn at_exit_menu_060_goes_back_to_what_the_operator_had() {
        assert_eq!(pc_keying_to_restore(Some("0")), Some("0"));
        assert_eq!(pc_keying_to_restore(Some("3")), Some("3"));
        assert_eq!(pc_keying_to_restore(None), None, "never written by us (never read, or RTS before us): leave it");
    }

    /// Per model, never per slot: the FTX-1 keys on `1` of address 020114,
    /// the 991A on `2` of menu 060 (CW design, decision 8; measured
    /// 2026-09-20 on COM27 beside its CAT port).
    #[test]
    fn each_model_has_its_own_pc_keying_menu() {
        assert_eq!(pc_keying_menu(RadioModel::Ft991a), ("060", "2"));
        assert_eq!(pc_keying_menu(RadioModel::Ftx1), ("020114", "1"));
        // The FTX-1 found on DTR (2): RTS is written, and 2 goes back at exit.
        let (original, write) = pc_keying_plan("1", Some("2"), None).unwrap();
        assert!(write);
        assert_eq!(original.as_deref(), Some("2"));
        // And found on its own RTS (1): nothing written, nothing remembered.
        let (original, write) = pc_keying_plan("1", Some("1"), None).unwrap();
        assert!(!write);
        assert_eq!(original, None);
    }
}
