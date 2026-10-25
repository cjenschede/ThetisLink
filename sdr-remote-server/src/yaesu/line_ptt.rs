// SPDX-License-Identifier: GPL-2.0-or-later
//
//! A Yaesu that transmits on a line of its USB port (build 296; owner,
//! 2026-10-24). Both radios can key on RTS or DTR of their USB COM ports, per
//! mode, and Windows raises those lines when it starts: the owner's radios went
//! into transmit while the PC booted, with ThetisLink not even running. The
//! cause was menus 071 and 076 of the FT-991A and every RPTT SELECT of the
//! FTX-1 on RTS. Yaesu warns of it itself (FTX-1 CAT OM 2508-C, p. 2): "When
//! using the USB cable for TX control, the transceiver may switch to the
//! transmit mode when the computer is started." The same table prints RTS in
//! bold - its mark for the factory default - for PRESET1-5 RPTT SELECT, where
//! the text above it says OFF (review of 296).
//!
//! ThetisLink keys by CAT (`TX1;`/`TX0;`) and needs none of these lines - the
//! one exception is PC KEYING, which the server sets to RTS itself for a CW
//! run and puts back at exit (`ex_menu::prepare_pc_keying`). So any of them on
//! RTS or DTR is worth a notice. Read from the menu blob the server already
//! reads once per connect: no extra traffic to the radio.

use super::RadioModel;

/// The line a menu keys the radio on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Line {
    Rts,
    Dtr,
}

impl Line {
    fn name(self) -> &'static str {
        match self {
            Line::Rts => "RTS",
            Line::Dtr => "DTR",
        }
    }
}

/// One menu found keying the radio on a USB line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineKeyed {
    /// The menu's key as the blob has it: `071` (991A), `090116` (FTX-1).
    pub key: &'static str,
    pub name: String,
    pub line: Line,
}

impl LineKeyed {
    /// Whether this is the radio's PC KEYING, which a CW run of this server
    /// sets to RTS itself.
    pub fn is_pc_keying(&self) -> bool {
        self.name.ends_with("PC KEYING")
    }

    /// `menu 071 DATA PTT SELECT = RTS`
    pub fn describe(&self) -> String {
        format!("menu {} {} = {}", self.key, self.name, self.line.name())
    }
}

/// The FT-991A's menus that can key on a line: (menu, name, the value that is
/// RTS, the value that is DTR). CAT OM 1711-D, OM p. 125-127; the same values
/// as `docs/internal/referentie/Yaesu-FT-991A-153-EX-menuparameters-via-CAT.md`.
const FT991A: [(&str, &str, &str, &str); 5] = [
    ("047", "AM PTT SELECT", "1", "2"),
    ("060", "PC KEYING", "2", "3"),
    ("071", "DATA PTT SELECT", "1", "2"),
    ("076", "FM PKT PTT SELECT", "1", "2"),
    ("108", "SSB PTT SELECT", "1", "2"),
];

/// The FTX-1's: every RPTT SELECT and the PC KEYING in its menu chart, where
/// `1` is RTS and `2` is DTR in each of them (CAT OM 2508-C, Table 3). Taken
/// from the chart, so the list cannot drift from it.
fn ftx1_menus() -> Vec<(&'static str, String)> {
    sdr_remote_core::ftx1_ex_chart::EX_CHART
        .iter()
        .filter_map(|(addr, _group, sub, desc)| {
            if desc.starts_with("RPTT SELECT") {
                Some((*addr, format!("{} RPTT SELECT", sub)))
            } else if desc.starts_with("PC KEYING") {
                Some((*addr, "PC KEYING".to_string()))
            } else {
                None
            }
        })
        .collect()
}

fn value_of<'a>(blob: &'a str, key: &str) -> Option<&'a str> {
    blob.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        (k.trim() == key).then(|| v.trim())
    })
}

/// Every menu in `blob` that keys the radio on RTS or DTR.
pub fn line_keyed(model: RadioModel, blob: &str) -> Vec<LineKeyed> {
    let mut found = Vec::new();
    if matches!(model, RadioModel::Ftx1) {
        for (key, name) in ftx1_menus() {
            let line = match value_of(blob, key) {
                Some("1") => Line::Rts,
                Some("2") => Line::Dtr,
                _ => continue,
            };
            found.push(LineKeyed { key, name, line });
        }
    } else {
        for (key, name, rts, dtr) in FT991A {
            let line = match value_of(blob, key) {
                Some(v) if v == rts => Line::Rts,
                Some(v) if v == dtr => Line::Dtr,
                _ => continue,
            };
            found.push(LineKeyed { key, name: name.to_string(), line });
        }
    }
    found
}

/// What the server screen shows: everything, except PC KEYING while this
/// server has set it to RTS itself for CW (`cw_pc_keying_original` is then
/// known) - that one goes back at exit.
pub fn to_show(found: &[LineKeyed], cw_set_pc_keying: bool) -> Vec<&LineKeyed> {
    found.iter().filter(|m| !(cw_set_pc_keying && m.is_pc_keying())).collect()
}

/// The list for a log line or the screen: `menu 071 DATA PTT SELECT = RTS, ...`
pub fn list(found: &[&LineKeyed]) -> String {
    found.iter().map(|m| m.describe()).collect::<Vec<_>>().join(", ")
}

/// What the screen and the log show of a radio's list: `to_show` with this
/// server's own CW setting of PC KEYING. The one place that flag is read.
pub fn shown(st: &super::YaesuState) -> Vec<&LineKeyed> {
    to_show(&st.line_keyed, st.cw_pc_keying_original.is_some())
}

/// What changed the server's menu copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuRead {
    /// A full read: at every connect (the 991A), or when the FTX-1's scan is
    /// done. Says the list each time, so also on a second connect to the same
    /// radio - `forget_previous_radio` runs only when the model changes
    /// (review of 296).
    Full,
    /// One value written from a client's menu screen: says the list only when
    /// it changed.
    OneValue,
}

/// The server's copy of what keys the radio on a line, after its menu blob
/// changed: the one place that sets `line_keyed`, and the one place that says
/// so in the log.
pub(super) fn refresh(st: &mut super::YaesuState, model: RadioModel, prefix: &str, read: MenuRead) {
    if let Some(menus) = update(st, model, read) {
        // Named in advance (`logregels-build296.md`, `logregels-build297.md`).
        log::warn!(
            "{} PTT on a USB line: {} - the radio transmits when Windows raises the line (at start-up, for example); ThetisLink keys by CAT and does not need it",
            prefix,
            menus
        );
    }
}

/// `refresh` without the log: sets the list, and returns what to say - what
/// the screen shows, after a full read or a change, and nothing when that is
/// empty.
fn update(st: &mut super::YaesuState, model: RadioModel, read: MenuRead) -> Option<String> {
    let found = st.last_menu_blob.as_deref().map(|blob| line_keyed(model, blob)).unwrap_or_default();
    let changed = found != st.line_keyed;
    st.line_keyed = found;
    let menus = list(&shown(st));
    (!menus.is_empty() && (read == MenuRead::Full || changed)).then_some(menus)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The owner's two radios as they were (2026-10-24): 071 and 076 on RTS
    /// on the 991A, every RPTT SELECT on 1 on the FTX-1.
    #[test]
    fn the_owners_radios_are_found_as_they_were() {
        let a = "046:0\n047:0\n060:0\n071:1\n076:1\n108:0\n109:1";
        let found = line_keyed(RadioModel::Ft991a, a);
        assert_eq!(found.iter().map(|m| (m.key, m.line)).collect::<Vec<_>>(), [("071", Line::Rts), ("076", Line::Rts)]);
        assert_eq!(list(&to_show(&found, false)), "menu 071 DATA PTT SELECT = RTS, menu 076 FM PKT PTT SELECT = RTS");

        let rptt = ["010115", "010215", "010314", "010415", "010512", "020112", "090116", "090216", "090316", "090416", "090516"];
        let x: String = rptt.iter().map(|k| format!("{k}:1\n")).collect::<String>() + "020114:0\n010116:1";
        let found = line_keyed(RadioModel::Ftx1, &x);
        assert_eq!(found.iter().map(|m| m.key).collect::<Vec<_>>(), rptt, "every RPTT SELECT, and nothing beside them");
        assert!(found.iter().all(|m| m.line == Line::Rts && m.name.ends_with("RPTT SELECT")));
        assert!(found.iter().any(|m| m.name == "PRESET3 RPTT SELECT"));
    }

    /// Both lines and the values that are neither, per model (the 991A's PC
    /// KEYING counts differently from its PTT SELECTs).
    #[test]
    fn rts_and_dtr_are_read_per_menu() {
        let found = line_keyed(RadioModel::Ft991a, "047:2\n060:2\n071:0\n108:3");
        assert_eq!(found.iter().map(|m| (m.key, m.line)).collect::<Vec<_>>(), [("047", Line::Dtr), ("060", Line::Rts)]);
        assert_eq!(line_keyed(RadioModel::Ft991a, "060:3")[0].line, Line::Dtr);
        assert_eq!(line_keyed(RadioModel::Ft991a, "108:1")[0].line, Line::Rts);
        assert_eq!(line_keyed(RadioModel::Ft991a, "108:2")[0].line, Line::Dtr);
        assert!(line_keyed(RadioModel::Ft991a, "060:1\n071:\n076:x").is_empty(), "DAKY, empty and unreadable say nothing");
        let found = line_keyed(RadioModel::Ftx1, "020114:2\n090116:3\n010115:0");
        assert_eq!(found.iter().map(|m| (m.key, m.line)).collect::<Vec<_>>(), [("020114", Line::Dtr)], "3 is the rear jack");
        assert!(line_keyed(RadioModel::Ftx1, "").is_empty());
    }

    /// Where the list is kept up to date: after a full menu read (both models,
    /// `apply_menu_result`) and after one value written (`note_menu_value`),
    /// emptied with the rest of a previous radio, and set nowhere else. The
    /// serial thread runs in no test.
    #[test]
    fn the_list_follows_every_change_of_the_menu_copy() {
        let poll = include_str!("poll.rs");
        assert_eq!(poll.matches("        s.last_menu_blob = Some(data.clone());\n        s.tot_minutes = tot;\n        super::line_ptt::refresh(&mut s, model, prefix, super::line_ptt::MenuRead::Full);").count(), 1);
        let radio = include_str!("mod.rs");
        assert_eq!(radio.matches("        line_ptt::refresh(&mut st, model, &format!(\"[{}]\", model.label()), line_ptt::MenuRead::OneValue);\n        drop(st);\n        *self.menu_data.lock().unwrap() = Some(format!(\"MENU:{}\", merged));").count(), 1);
        assert_eq!(radio.matches("        self.last_menu_blob = None;\n        self.line_keyed.clear();").count(), 1);
        for (file, src) in [("poll.rs", poll), ("mod.rs", radio)] {
            assert_eq!(src.matches("last_menu_blob = Some(").count(), 1 + usize::from(file == "mod.rs"), "{file}: a new writer of the copy - refresh the list there too");
        }
        let own = include_str!("line_ptt.rs");
        let own = &own[..own.find("#[cfg(test)]\nmod tests").unwrap()];
        assert_eq!(own.matches("st.line_keyed = found;").count(), 1);
        assert_eq!(radio.matches(".line_keyed = ").count(), 0, "set only by refresh");
        // The screen takes what to show from `shown`, with this server's CW
        // flag in it (review of 296: `false` there stayed green).
        let panel = include_str!("../ui/status_panel.rs");
        assert_eq!(panel.matches("    let shown = line_ptt::shown(&st);").count(), 1);
        assert_eq!(panel.matches("to_show(").count(), 0);
        assert_eq!(own.matches("to_show(&st.line_keyed, st.cw_pc_keying_original.is_some())").count(), 1);
        // The advice names OFF for 060, where DAKY is the rear jack (review of 296).
        assert_eq!(include_str!("../../locales/app.yml").matches("DAKY (FT-991A; 060 PC KEYING: OFF)").count(), 4);
    }

    /// When the log says it: after every full read, so also on a second
    /// connect to the same radio; after one value written only when the list
    /// changed; and never what the screen leaves out (review of 296).
    #[test]
    fn a_full_read_says_it_each_time_and_one_value_only_when_it_changes() {
        let mut st = crate::yaesu::YaesuState::default();
        let read = |st: &mut crate::yaesu::YaesuState, blob: &str, how| {
            st.last_menu_blob = Some(blob.to_string());
            update(st, RadioModel::Ft991a, how)
        };
        let both = Some("menu 071 DATA PTT SELECT = RTS, menu 076 FM PKT PTT SELECT = RTS".to_string());
        assert_eq!(read(&mut st, "071:1\n076:1", MenuRead::Full), both);
        assert_eq!(read(&mut st, "071:1\n076:1", MenuRead::Full), both, "a second connect to the same radio");
        assert_eq!(read(&mut st, "071:1\n076:1", MenuRead::OneValue), None, "a value written that changes nothing");
        assert_eq!(read(&mut st, "071:0\n076:1", MenuRead::OneValue).as_deref(), Some("menu 076 FM PKT PTT SELECT = RTS"));
        assert_eq!(read(&mut st, "071:0\n076:0", MenuRead::OneValue), None);
        assert!(st.line_keyed.is_empty(), "set to DAKY from a client: the screen clears");
        assert_eq!(read(&mut st, "071:0\n076:0", MenuRead::Full), None, "nothing on a line says nothing");

        st.cw_pc_keying_original = Some("0".to_string());
        assert_eq!(read(&mut st, "060:2", MenuRead::Full), None, "this server's own PC KEYING: not in the log either");
        assert_eq!(st.line_keyed.len(), 1, "but kept in the list");
        st.cw_pc_keying_original = None;
        assert_eq!(read(&mut st, "060:2", MenuRead::Full).as_deref(), Some("menu 060 PC KEYING = RTS"));
    }

    /// PC KEYING on RTS is left out of the screen while this server set it for
    /// CW, and shown when it did not (left behind, or set by hand).
    #[test]
    fn pc_keying_is_left_out_only_while_this_server_set_it() {
        let found = line_keyed(RadioModel::Ft991a, "060:2\n071:1");
        assert_eq!(to_show(&found, false).len(), 2);
        assert_eq!(list(&to_show(&found, true)), "menu 071 DATA PTT SELECT = RTS");
        let found = line_keyed(RadioModel::Ftx1, "020114:1");
        assert!(found[0].is_pc_keying());
        assert!(to_show(&found, true).is_empty());
    }
}
