// SPDX-License-Identifier: GPL-2.0-or-later
//
//! Where an FTX-1's memory tones live between sessions.
//!
//! The FTX-1 cannot store a CTCSS tone in a memory channel over CAT - writing
//! its memory resets every touched channel to 100.0 Hz and there is no command
//! to put it back. ThetisLink's answer is that the server's list is the truth
//! for that radio: the tone is held there and applied to the set whenever the
//! channel is recalled.
//!
//! That answer was only true for as long as the server ran. The list lived in
//! memory and nowhere else, so a restart read the radio again, found no tones,
//! and the operator's work was gone - demonstrated with two problem reports,
//! one before and one after a restart (2026-08-12). A truth that evaporates is
//! not a truth; this file is where it now goes.
//!
//! Deliberately only the tones, and deliberately only for the radio that needs
//! it. Frequencies, names and modes are stored properly by both radios, so for
//! those the radio remains what it always was: the thing that knows. Reading
//! them back from a file could only introduce a second opinion.

use std::path::PathBuf;

use log::{info, warn};

use super::RadioModel;

/// One tone as it is held in the list: channel, label ("77.0" or "D023N"), and
/// whether that label is a DCS code rather than a CTCSS frequency.
pub(super) type Tone = (u16, String, bool);

/// Beside the server's own configuration, one file per slot.
///
/// Per slot rather than per radio: a slot is what the operator configures and
/// what the log prefixes talk about. The model is written into the file so a
/// different radio in the same slot cannot inherit its predecessor's tones.
fn path_for(slot: u8) -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    Some(dir.join(format!("thetislink-tones-radio{slot}.txt")))
}

/// Write the tones this list holds, replacing whatever was there.
///
/// Failure is logged and otherwise ignored: not being able to save a tone must
/// never break the radio the operator is using. It costs them the tone at the
/// next restart, which the log then explains.
pub(super) fn save(slot: u8, model: RadioModel, tones: &[Tone], prefix: &str) {
    if !matches!(model, RadioModel::Ftx1) {
        return;
    }
    let Some(path) = path_for(slot) else { return };
    let mut out = String::from("# ThetisLink FTX-1 memory tones - the radio cannot store these itself\n");
    out.push_str(&format!("model\t{}\n", model.label()));
    for (ch, label, is_dcs) in tones {
        out.push_str(&format!(
            "{}\t{}\t{}\n",
            ch,
            label,
            if *is_dcs { "dcs" } else { "ctcss" }
        ));
    }
    match std::fs::write(&path, out) {
        Ok(()) => info!("{} kept {} memory tone(s) for the next start", prefix, tones.len()),
        Err(e) => warn!(
            "{} could not keep its memory tones ({}): they will be gone after a restart",
            prefix, e
        ),
    }
}

/// Read back what the last session held, for this slot and this model.
///
/// An empty result is the ordinary case on a first run and needs no comment.
pub(super) fn load(slot: u8, model: RadioModel, prefix: &str) -> Vec<Tone> {
    if !matches!(model, RadioModel::Ftx1) {
        return Vec::new();
    }
    let Some(path) = path_for(slot) else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new() };

    let mut out = Vec::new();
    let mut file_model: Option<String> = None;
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.first() == Some(&"model") {
            file_model = f.get(1).map(|m| m.trim().to_string());
            continue;
        }
        let (Some(ch), Some(label)) = (
            f.first().and_then(|c| c.trim().parse::<u16>().ok()),
            f.get(1).map(|l| l.trim().to_string()),
        ) else {
            continue;
        };
        if ch == 0 || label.is_empty() {
            continue;
        }
        out.push((ch, label, f.get(2).map(|k| k.trim() == "dcs").unwrap_or(false)));
    }

    // A different radio in this slot keeps its own tones. Applying the previous
    // one's would put a tone on a repeater channel nobody chose it for.
    if file_model.as_deref() != Some(model.label()) {
        warn!(
            "{} kept tones are for {} and this is {} - not applying them",
            prefix,
            file_model.as_deref().unwrap_or("an unknown radio"),
            model.label()
        );
        return Vec::new();
    }
    if !out.is_empty() {
        info!("{} {} memory tone(s) restored from the last session", prefix, out.len());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What is written comes back, tone kinds and all.
    #[test]
    fn a_round_trip_keeps_every_field() {
        let tones: Vec<Tone> = vec![
            (5, "77.0".into(), false),
            (16, "D023N".into(), true),
        ];
        // The parsing half, without touching the filesystem: build the text the
        // way `save` does and read it the way `load` does.
        let mut text = String::from("# comment\nmodel\tFTX1\n");
        for (ch, label, is_dcs) in &tones {
            text.push_str(&format!("{}\t{}\t{}\n", ch, label, if *is_dcs { "dcs" } else { "ctcss" }));
        }
        let parsed: Vec<Tone> = text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .filter(|l| !l.starts_with("model\t"))
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                Some((
                    f.first()?.trim().parse::<u16>().ok()?,
                    f.get(1)?.trim().to_string(),
                    f.get(2).map(|k| k.trim() == "dcs").unwrap_or(false),
                ))
            })
            .collect();
        assert_eq!(parsed, tones);
    }

    /// The 991A stores its own tones; keeping a second copy could only start an
    /// argument about which one is right.
    #[test]
    fn nothing_is_kept_or_read_for_the_991a() {
        save(0, RadioModel::Ft991a, &[(1, "77.0".into(), false)], "[test]");
        assert!(load(0, RadioModel::Ft991a, "[test]").is_empty());
    }
}
