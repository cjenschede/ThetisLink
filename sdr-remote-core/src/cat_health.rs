// SPDX-License-Identifier: GPL-2.0-or-later

//! When to tell the operator that the baud rate may be wrong.
//!
//! There has been a hint in the Yaesu poll loop for a long time, and it says
//! the right thing: *check baud radio-menu vs config*. It fires when the radio
//! has said nothing for five seconds.
//!
//! PD0PLK's report 68 is the case it misses. His radio answered — it just
//! answered badly, because the server was on 4800 and the set was not. Writes
//! to the port did not drain, one query came back carrying ten replies at once,
//! and an `IF;` answer arrived twelve bytes long where the 991A defines
//! twenty-seven. Every one of those says "baud" and none of them is silence, so
//! the hint stayed quiet through the whole session.
//!
//! This is the decision about which of those is worth saying out loud, and how
//! often, kept away from the loop that gathers them.

/// What has been seen since the last exchange that went normally.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CatTrouble {
    /// Writes to the serial port that did not complete. The port is not
    /// draining as fast as the configured rate promises.
    pub write_stalls: u32,
    /// Replies to a single query that carried more than one frame. Reads and
    /// writes are out of step with each other.
    pub frames_run_together: u32,
    /// Replies shorter than the command defines them to be.
    pub short_replies: u32,
    /// How long the radio has said nothing at all.
    pub silent_secs: u64,
}

/// Does this look enough like a baud mismatch to say so?
///
/// The thresholds differ per symptom on purpose, and the reason is how much
/// each one could be something else:
///
/// - **Silence** is the case that already fired. Unchanged at five seconds.
/// - **Frames running together** is conclusive on its own. Nothing else makes a
///   radio answer one question with ten answers.
/// - **A stalled write** can be a one-off, so it takes two.
/// - **A short reply** is the weakest: firmware quirks and unsupported commands
///   look the same, so it takes three before it means anything.
pub fn baud_hint_due(t: CatTrouble) -> bool {
    t.silent_secs >= 5 || t.frames_run_together >= 1 || t.write_stalls >= 2 || t.short_replies >= 3
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The case that already worked, and it has to keep working.
    #[test]
    fn a_silent_radio_still_raises_it() {
        assert!(baud_hint_due(CatTrouble { silent_secs: 5, ..Default::default() }));
        assert!(!baud_hint_due(CatTrouble { silent_secs: 4, ..Default::default() }));
    }

    /// PD0PLK, report 68. One query, ten answers:
    ///
    /// ```text
    /// unknown ID code '007077100;MD04;TX0;AG0041;PC050;PS1;?;?;RG0251;...'
    /// ```
    ///
    /// Nothing except a link that is out of step does that, so one is enough.
    #[test]
    fn frames_running_together_are_conclusive_on_their_own() {
        assert!(baud_hint_due(CatTrouble { frames_run_together: 1, ..Default::default() }));
    }

    /// Also his, and the one that drove the reconnect every thirty seconds:
    ///
    /// ```text
    /// S-meter poll failed: failed to write whole buffer
    /// ```
    ///
    /// One can be a hiccup on a busy port. Two in a row is the rate being wrong.
    #[test]
    fn a_second_stalled_write_raises_it_but_the_first_does_not() {
        assert!(!baud_hint_due(CatTrouble { write_stalls: 1, ..Default::default() }));
        assert!(baud_hint_due(CatTrouble { write_stalls: 2, ..Default::default() }));
    }

    /// The weakest signal, and deliberately the slowest to speak. An
    /// unsupported command and a firmware quirk look exactly like a reply that
    /// came back short, and telling someone to check the baud rate over one of
    /// those wastes their evening.
    #[test]
    fn short_replies_take_three_before_they_mean_anything() {
        assert!(!baud_hint_due(CatTrouble { short_replies: 2, ..Default::default() }));
        assert!(baud_hint_due(CatTrouble { short_replies: 3, ..Default::default() }));
    }

    /// A healthy link says nothing.
    #[test]
    fn a_link_that_is_behaving_stays_quiet() {
        assert!(!baud_hint_due(CatTrouble::default()));
        assert!(!baud_hint_due(CatTrouble {
            write_stalls: 1,
            short_replies: 2,
            silent_secs: 4,
            frames_run_together: 0,
        }));
    }
}
