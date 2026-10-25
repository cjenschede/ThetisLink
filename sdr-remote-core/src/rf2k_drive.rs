// SPDX-License-Identifier: GPL-2.0-or-later

//! The RF2K-S drive table: the drive in watts per band, for SSB, AM and CONT.
//!
//! The server reads it from the amplifier's Pi, sends it to the clients in its
//! status line, and both the server window and the desktop client let the
//! operator edit it and send changed cells back. Every one of those places
//! needs the same order of the three categories, and until build 204 each
//! wrote it out by hand: three fields in the server, three in the shared state,
//! three positions in the status line, a `match category` on the server and two
//! more in the two "Save to Pi" buttons. Here it is once (owner, 2026-10-15:
//! "nu meenemen"). On the wire a row travels as its own command code
//! (`COMMANDS`): the client encodes it with `command_for`, the server decodes
//! it with `category_of` (build 205; review of 204 - until then those were two
//! more hand-written lists). The whole command, code and payload, is built by
//! `encode` and read by `decode` (build 206; review of 205: the order of band
//! and value in the payload was still a comment at both ends). A change itself
//! is a `DriveChange` with named fields from the screen to the Pi (build 207).

use crate::protocol::{CMD_RF2K_SET_DRIVE_AM, CMD_RF2K_SET_DRIVE_CONT, CMD_RF2K_SET_DRIVE_SSB};

/// Bands in a row, in the order of the Pi's own arrays.
pub const BANDS: usize = 11;

/// Rows in the table. The table, its names and its command codes all take
/// their length from here, so a row added to one and not the others does not
/// compile (review of 205: with three separate `3`s, a fourth row would have
/// had no command and its save would have gone nowhere, silently).
pub const ROWS: usize = 3;

/// The rows of the table, in order. The index is the `category` of a drive
/// command.
pub const CATEGORY_NAMES: [&str; ROWS] = ["SSB", "AM", "CONT"];

/// The command code that carries a change to each row, in row order.
pub const COMMANDS: [u8; ROWS] = [CMD_RF2K_SET_DRIVE_SSB, CMD_RF2K_SET_DRIVE_AM, CMD_RF2K_SET_DRIVE_CONT];

/// The command code for a row; `None` for a row that does not exist.
pub fn command_for(category: u8) -> Option<u8> {
    COMMANDS.get(category as usize).copied()
}

/// The row a command code changes; `None` when it is not a drive command.
pub fn category_of(command: u8) -> Option<u8> {
    COMMANDS.iter().position(|&c| c == command).map(|i| i as u8)
}

/// One changed cell: its row, its band and the new value in watts. Named, so
/// that no place unpacks three numbers of one type by position (review of
/// 206: the server's `|(category, band, value)|` compiled the other way round,
/// and so did the two "Save to Pi" loops).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DriveChange {
    pub category: u8,
    pub band: u8,
    pub value: u8,
}

/// A change as it goes on the wire: the row's command code, and the band and
/// the value as its payload. `None` for a row that does not exist.
pub fn encode(change: DriveChange) -> Option<(u8, Vec<u8>)> {
    command_for(change.category).map(|command| (command, vec![change.band, change.value]))
}

/// The change back from a command code and its payload; `None` when it is not
/// a drive command or the payload is short.
pub fn decode(command: u8, data: &[u8]) -> Option<DriveChange> {
    match (category_of(command), data) {
        (Some(category), [band, value, ..]) => Some(DriveChange { category, band: *band, value: *value }),
        _ => None,
    }
}

/// The band of each column.
pub const BAND_NAMES: [&str; BANDS] = ["160m", "80m", "60m", "40m", "30m", "20m", "17m", "15m", "12m", "10m", "6m"];

/// One drive value per category and band, in watts.
pub type DriveTable = [[u8; BANDS]; ROWS];

/// The three fields of the status line, in row order: each row as its values
/// joined by `;`.
pub fn to_wire(table: &DriveTable) -> [String; ROWS] {
    table.map(|row| row.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(";"))
}

/// The table back from the three status-line fields, in row order. A value
/// that does not parse, or a row that is short, reads as 0.
pub fn from_wire(fields: [&str; ROWS]) -> DriveTable {
    fields.map(|field| {
        let mut row = [0u8; BANDS];
        for (cell, value) in row.iter_mut().zip(field.split(';')) {
            *cell = value.parse().unwrap_or(0);
        }
        row
    })
}

/// The table being edited on a screen, with the server table it was last
/// filled from. One type, so the two cannot be passed the wrong way round
/// (review of 204: `changed_cells(server, edit)` compiled, sent the old values
/// and looked saved).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct DriveEdit {
    /// What the operator sees and changes.
    pub cells: DriveTable,
    shown: DriveTable,
}

impl DriveEdit {
    /// Follow the server's table whenever that changes (review of 200).
    /// Filling it once per run showed zeros when the group was drawn between
    /// the server's two fetches, and the first session's table after a
    /// network change - and "Save to Pi" then wrote those back. An edit of the
    /// operator's stays until the server's table itself changes, which after a
    /// save is the saved table.
    pub fn follow(&mut self, server: &DriveTable) {
        if self.shown != *server {
            self.cells = *server;
            self.shown = *server;
        }
    }

    /// What "Save to Pi" sends: every cell where the edit differs from the
    /// server's table, rows first.
    pub fn changed(&self, server: &DriveTable) -> Vec<DriveChange> {
        let mut changed = Vec::new();
        for (category, (edit_row, server_row)) in self.cells.iter().zip(server).enumerate() {
            for (band, (&new, &old)) in edit_row.iter().zip(server_row).enumerate() {
                if new != old {
                    changed.push(DriveChange { category: category as u8, band: band as u8, value: new });
                }
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn real() -> DriveTable {
        [[50; BANDS], [25; BANDS], [40; BANDS]]
    }

    #[test]
    fn the_status_line_carries_the_table_in_row_order() {
        let mut table = real();
        table[0][0] = 1;
        table[1][5] = 2;
        table[2][10] = 3;
        let wire = to_wire(&table);
        assert!(wire[0].starts_with("1;"));
        assert!(wire[2].ends_with(";3"));
        assert_eq!(from_wire([&wire[0], &wire[1], &wire[2]]), table);
        // A short or unreadable field reads as 0, not as a shifted row.
        assert_eq!(from_wire(["7;x", "", "9"])[0], [7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(CATEGORY_NAMES, ["SSB", "AM", "CONT"]);
    }

    #[test]
    fn each_row_has_its_own_command_both_ways() {
        // The codes by name, so a swap in `COMMANDS` is a swap against this.
        assert_eq!(COMMANDS, [CMD_RF2K_SET_DRIVE_SSB, CMD_RF2K_SET_DRIVE_AM, CMD_RF2K_SET_DRIVE_CONT]);
        for category in 0..ROWS as u8 {
            let command = command_for(category).unwrap();
            assert_eq!(category_of(command), Some(category), "{} does not come back", CATEGORY_NAMES[category as usize]);
        }
        assert_eq!(command_for(ROWS as u8), None);
        assert_eq!(category_of(0x00), None);
    }

    #[test]
    fn a_change_travels_as_band_then_value_both_ways() {
        // The payload by position, as `protocol.rs` documents it
        // (data[0]=band, data[1]=watts).
        assert_eq!(encode(DriveChange { category: 1, band: 3, value: 60 }), Some((CMD_RF2K_SET_DRIVE_AM, vec![3, 60])));
        // Every cell comes back as itself - a value under 11 W too, which
        // read the wrong way round is another band.
        for category in 0..ROWS as u8 {
            for band in 0..BANDS as u8 {
                for value in [0, 1, 10, 11, 200] {
                    let change = DriveChange { category, band, value };
                    let (command, data) = encode(change).unwrap();
                    assert_eq!(decode(command, &data), Some(change));
                }
            }
        }
        assert_eq!(encode(DriveChange { category: ROWS as u8, band: 0, value: 0 }), None);
        assert_eq!(decode(CMD_RF2K_SET_DRIVE_SSB, &[3]), None, "a short payload is not a change");
        assert_eq!(decode(0x00, &[3, 60]), None);
    }

    /// The two ends of the wire use this list: the client's engine encodes
    /// with `encode`, the server decodes with `decode`, and the save applies
    /// the change to the Pi's table. All three sit in loops no test drives, so
    /// the calls are pinned as text (build 205-208).
    #[test]
    fn both_ends_of_the_wire_use_this_list() {
        let engine = include_str!("../../sdr-remote-logic/src/engine.rs");
        assert_eq!(engine.matches("sdr_remote_core::rf2k_drive::encode(change)").count(), 1);
        assert!(!engine.contains("CMD_RF2K_SET_DRIVE_"), "the engine has its own list again");
        assert!(!engine.contains("vec![band, value]"), "the engine builds its own payload again");
        let network = include_str!("../../sdr-remote-server/src/network.rs");
        assert_eq!(network.matches("sdr_remote_core::rf2k_drive::category_of(c)").count(), 1);
        assert_eq!(
            network.matches("sdr_remote_core::rf2k_drive::decode(c, &eq_cmd.data).map(crate::rf2k::Rf2kCmd::SetDriveConfig)").count(),
            1
        );
        assert!(!network.contains("CMD_RF2K_SET_DRIVE_"), "the server has its own list again");
        assert!(!network.contains("band: eq_cmd.data["), "the server reads its own payload again");
        // Without this line a save sends the Pi's table back unchanged, and the
        // screen keeps what was typed: it looks saved (review of 207).
        let rf2k = include_str!("../../sdr-remote-server/src/rf2k.rs");
        assert_eq!(rf2k.matches("dc.apply(*change);").count(), 1);
    }

    #[test]
    fn the_edit_follows_the_servers_table_and_keeps_the_operators_change() {
        let mut edit = DriveEdit::default();
        // The table of zeros the server sends before its drive fetch.
        edit.follow(&[[0; BANDS]; ROWS]);
        assert_eq!(edit.cells, [[0; BANDS]; ROWS]);
        // Then the real one: the edit takes it.
        edit.follow(&real());
        assert_eq!(edit.cells, real());
        // The operator changes a band; the same table again keeps that.
        edit.cells[0][3] = 60;
        edit.follow(&real());
        assert_eq!(edit.cells[0][3], 60);
        // A new session or another client's save: the server's table wins,
        // whichever single cell differs (review of 201 and 202: a comparison
        // of part of the table stayed green when only some rows changed).
        let mut before = real();
        for category in 0..ROWS {
            for band in 0..BANDS {
                let mut after = before;
                after[category][band] = before[category][band].wrapping_add(1);
                edit.follow(&after);
                assert_eq!(edit.cells, after, "cell {category},{band} was not followed");
                before = after;
            }
        }
    }

    #[test]
    fn save_sends_every_changed_cell_and_nothing_else() {
        let server = real();
        let mut edit = DriveEdit::default();
        edit.follow(&server);
        assert!(edit.changed(&server).is_empty(), "an unchanged table sends nothing");
        // Each cell alone, in every row - CONT and 6 m included (review of
        // 202: a loop that skipped CONT sent nothing and looked saved).
        for category in 0..ROWS as u8 {
            for band in 0..BANDS as u8 {
                let mut one = edit;
                one.cells[category as usize][band as usize] = 99;
                assert_eq!(one.changed(&server), vec![DriveChange { category, band, value: 99 }]);
            }
        }
        edit.cells[2][10] = 0;
        edit.cells[0][0] = 100;
        assert_eq!(
            edit.changed(&server),
            vec![DriveChange { category: 0, band: 0, value: 100 }, DriveChange { category: 2, band: 10, value: 0 }]
        );
    }

    /// Android names the RF2K's band in Kotlin, so it cannot use
    /// `BAND_NAMES`. Its list has to say the same, index for index (build
    /// 204: it ran the other way round, the UltraBeam's order).
    #[test]
    fn android_names_the_bands_in_this_order() {
        let kotlin = include_str!(
            "../../sdr-remote-android/android/app/src/main/java/com/sdrremote/ui/screens/ExternalDevicesScreen.kt"
        );
        let start = kotlin.find("private fun rf2kBandName(").expect("rf2kBandName is gone from the Kotlin");
        let body = &kotlin[start..start + kotlin[start..].find("\n}").unwrap()];
        let names: Vec<(usize, &str)> = body
            .lines()
            .filter_map(|line| {
                let (index, name) = line.trim().split_once(" -> ")?;
                Some((index.parse().ok()?, name.trim_matches('"')))
            })
            .collect();
        assert_eq!(names.len(), BANDS);
        for (index, name) in names {
            assert_eq!(BAND_NAMES[index], name, "Android names band {index} differently");
        }
    }
}
