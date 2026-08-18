// SPDX-License-Identifier: GPL-2.0-or-later
//! Grouping for the `.conf` files the server and the client write.
//!
//! Both write their settings as `key=value` lines, in whatever order the
//! writing code happens to run. That order is nobody's design - it is the
//! order the fields were added over three years - so related settings sit
//! pages apart and finding one by eye means reading the whole file.
//!
//! This groups the lines that are already being written, under headings, at
//! the moment of writing. It deliberately does not touch the writers: they
//! stay a flat list of pushes, and the layout lives in one place that can be
//! tested. A key nobody has classified is not dropped - it lands under a
//! trailing heading, which is also how you notice it needs a home.
//!
//! Both readers match on `key=value` and ignore anything without an `=`, so
//! the headings and the blank lines between sections are invisible to them.

/// One heading and the key prefixes that belong under it.
pub type Section<'a> = (&'a str, &'a [&'a str]);

/// Which section a key belongs to: the longest matching prefix wins.
///
/// Longest-match is what makes a table like `["rx2_", "rx2_spectrum_"]`
/// behave the way it reads. First-match would put `rx2_spectrum_zoom` under
/// whichever of the two came first in the table, which is an ordering trap
/// nobody would spot until a key went missing from the section it belongs to.
fn section_for(key: &str, sections: &[Section]) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None; // (prefix length, section index)
    for (idx, (_, prefixes)) in sections.iter().enumerate() {
        for prefix in prefixes.iter() {
            if key.starts_with(prefix) {
                let len = prefix.len();
                if best.is_none_or(|(best_len, _)| len > best_len) {
                    best = Some((len, idx));
                }
            }
        }
    }
    best.map(|(_, idx)| idx)
}

/// Regroup `content` under the given headings.
///
/// Every non-empty line of the input comes out exactly once, in its original
/// order within its section. Lines that are already headings or blanks are
/// dropped, so running this over its own output changes nothing.
pub fn group(content: &str, sections: &[Section], trailing_heading: &str) -> String {
    let mut buckets: Vec<Vec<&str>> = vec![Vec::new(); sections.len()];
    let mut leftovers: Vec<&str> = Vec::new();

    for line in content.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        // Everything before the first `=` is the key; a line without one is
        // not a setting and is kept with the unclassified rest rather than
        // silently thrown away.
        match line.split_once('=') {
            Some((key, _)) => match section_for(key.trim(), sections) {
                Some(idx) => buckets[idx].push(line),
                None => leftovers.push(line),
            },
            None => leftovers.push(line),
        }
    }

    let mut out = String::with_capacity(content.len() + sections.len() * 40);
    for (idx, (heading, _)) in sections.iter().enumerate() {
        if buckets[idx].is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&heading_line(heading));
        for line in &buckets[idx] {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !leftovers.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&heading_line(trailing_heading));
        for line in &leftovers {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// A heading carries no `=`, which is what keeps both readers blind to it.
fn heading_line(heading: &str) -> String {
    debug_assert!(!heading.contains('='), "a heading with an = would be read as a setting");
    format!("# ---- {heading} ----\n")
}

#[cfg(test)]
mod conf_layout_tests {
    use super::*;

    const SECTIONS: &[Section] = &[
        ("Connection", &["server", "password"]),
        ("Receiver 2", &["rx2_"]),
        ("Receiver 2 spectrum", &["rx2_spectrum_"]),
    ];

    /// The whole point: a key matching two prefixes goes under the more
    /// specific one, whichever order the table happens to list them in.
    #[test]
    fn the_longest_prefix_decides() {
        let out = group("rx2_volume=1\nrx2_spectrum_zoom=8\n", SECTIONS, "Other");
        let rx2 = out.find("rx2_volume").unwrap();
        let spectrum = out.find("rx2_spectrum_zoom").unwrap();
        assert!(out[..rx2].contains("Receiver 2 ----"));
        assert!(out[..spectrum].contains("Receiver 2 spectrum"));
    }

    /// Nothing may be lost on the way through - this file is the operator's
    /// settings, and a dropped line is a setting that silently reverts.
    #[test]
    fn every_setting_comes_out_exactly_once() {
        let input = "server=1.2.3.4\nrx2_volume=1\nunknown_key=7\npassword=x\n";
        let out = group(input, SECTIONS, "Other");
        for line in input.lines() {
            assert_eq!(out.matches(line).count(), 1, "{line} in {out}");
        }
    }

    #[test]
    fn a_key_with_no_home_is_kept_under_the_trailing_heading() {
        let out = group("unknown_key=7\n", SECTIONS, "Not sorted yet");
        assert!(out.contains("# ---- Not sorted yet ----\nunknown_key=7\n"), "{out}");
    }

    /// Grouping an already grouped file has to be a no-op, or every save
    /// would add another layer of headings.
    #[test]
    fn running_it_twice_changes_nothing() {
        let once = group("server=1\nrx2_volume=2\nstray=3\n", SECTIONS, "Other");
        assert_eq!(group(&once, SECTIONS, "Other"), once);
    }

    /// Within a section the writers' own order is left alone.
    #[test]
    fn the_order_inside_a_section_is_untouched() {
        let out = group("rx2_b=1\nrx2_a=2\n", SECTIONS, "Other");
        assert!(out.find("rx2_b").unwrap() < out.find("rx2_a").unwrap());
    }

    /// An empty section prints no heading, so a client that never wrote a
    /// yaesu key does not get an empty yaesu block.
    #[test]
    fn a_section_with_nothing_in_it_is_not_announced() {
        let out = group("server=1\n", SECTIONS, "Other");
        assert!(!out.contains("Receiver 2"), "{out}");
    }

    /// Headings must stay invisible to both readers, which key off `=`.
    #[test]
    fn no_heading_can_be_mistaken_for_a_setting() {
        let out = group("server=1\nstray=2\n", SECTIONS, "Other");
        for line in out.lines().filter(|l| l.starts_with('#')) {
            assert!(!line.contains('='), "{line}");
        }
    }

    #[test]
    fn an_empty_file_stays_empty() {
        assert_eq!(group("", SECTIONS, "Other"), "");
    }
}
