// SPDX-License-Identifier: GPL-2.0-or-later
//! The S reading as text, for every meter on every screen (build 218). Six
//! places used to compute it, with three ways of rounding: the phone said
//! `S9+19dB` where the desktop said `S9+20dB` for the same signal, and the
//! Yaesu bar said S5 where the compact Yaesu label said S6 (review of 217).
//! The desktop uses these functions directly; Android gets `text` through the
//! bridge.
//!
//! Rounding, the owner's choice (2026-10-16): to the nearest, "for everything".
//! Above S9 the dB (19.6 dB over S9 is `S9+20dB`); since build 222 also the S
//! step below S9 - it was rounded down, and with the needle on S8 the spectrum
//! read S7 half the time.

/// S9 in dBm (HF, IARU Region 1); 6 dB per S unit below it, S0 = -127 dBm.
const S9_DBM: f32 = -73.0;
const S0_DBM: f32 = -127.0;

/// The reading for a level in dBm: `S0`..`S9`, then `S9+<n>dB`.
pub fn text(dbm: f32) -> String {
    if dbm <= S9_DBM {
        let s_unit = ((dbm - S0_DBM) / 6.0).round().clamp(0.0, 9.0) as u8;
        format!("S{}", s_unit)
    } else {
        let db_over = (dbm - S9_DBM).round() as i32;
        format!("S9+{}dB", db_over.max(0))
    }
}

/// A Yaesu CAT S-meter value in dBm. The radio's value is already on the 0-228
/// display scale (S0 = 0, S9 = 108, S9+60 = 228): 2 units per dB, like
/// `dbm_to_display`, of which this is the inverse.
pub fn yaesu_raw_to_dbm(raw: u16) -> f32 {
    let disp = (raw as f32).clamp(0.0, 228.0);
    if disp <= 108.0 { disp / 2.0 + S0_DBM } else { (disp - 108.0) / 2.0 + S9_DBM }
}

#[cfg(test)]
mod tests {
    use super::{text, yaesu_raw_to_dbm};

    #[test]
    fn the_s_reading_rounds_the_step_and_the_db_to_nearest() {
        assert_eq!(text(-130.0), "S0");
        assert_eq!(text(-115.0), "S2");
        assert_eq!(text(-109.1), "S3", "S2.98 is S3 (build 222)");
        assert_eq!(text(-79.5), "S8", "S7.92: the needle on S8 read S7 (owner, 2026-10-16)");
        assert_eq!(text(-81.5), "S8", "S7.58");
        assert_eq!(text(-82.5), "S7", "S7.42");
        assert_eq!(text(-74.0), "S9", "S8.83");
        assert_eq!(text(-73.0), "S9");
        assert_eq!(text(-53.4), "S9+20dB", "19.6 dB over S9");
        assert_eq!(text(-52.6), "S9+20dB", "20.4 dB over S9");
        assert_eq!(text(-67.4), "S9+6dB", "5.6 dB over S9: the phone said S9+5");
    }

    #[test]
    fn a_yaesu_value_reads_as_the_same_s_as_its_dbm() {
        assert_eq!(text(yaesu_raw_to_dbm(66)), "S6", "5.5 S units, rounded (build 222)");
        assert_eq!(text(yaesu_raw_to_dbm(64)), "S5", "5.33 S units");
        assert_eq!(text(yaesu_raw_to_dbm(108)), "S9");
        assert_eq!(text(yaesu_raw_to_dbm(148)), "S9+20dB");
        assert_eq!(text(yaesu_raw_to_dbm(255)), "S9+60dB", "the scale ends at S9+60");
        for raw in 0..=228u16 {
            assert_eq!(crate::dbm_to_display(yaesu_raw_to_dbm(raw)), raw, "the inverse of dbm_to_display at {raw}");
        }
    }
}
