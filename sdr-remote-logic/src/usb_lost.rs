// SPDX-License-Identifier: GPL-2.0-or-later
//! "The server has lost the cable to this radio while you were transmitting."
//!
//! A radio disappearing from the USB is not a detail to be hidden. The audio
//! rides on the same cable, so from the moment it goes the transmission is a
//! bare carrier: the other station hears a signal and no voice. The operator
//! has to know that, and today the opposite happened - the radio's window was
//! removed the instant it went absent, so the one screen that could have said
//! anything was gone (owner, 2026-09-06, an FTX-1 that took its own USB down
//! with its own RF).
//!
//! Two rules, and the second is the reason this is a module rather than an
//! `if`:
//!
//! * It is shown while the radio is absent **and we are the ones holding it in
//!   transmit**. A radio that is simply switched off is not this.
//! * It stays up for a short while after that, because the outage that most
//!   needs saying is the one-second one: without a minimum the notice would
//!   flash by in a frame or two and the operator would only see a transmission
//!   that stopped for no reason he could name.
//!
//! Not longer than that, though. The notice sits on the transmit control, and
//! an operator who has read it wants to be able to key up again.

use std::time::Duration;

/// How long the notice stays up after the cable is back. Chosen with the owner:
/// long enough to read after a one-second blip, short enough not to stand in
/// the way of the next transmission.
pub const MINIMUM: Duration = Duration::from_secs(3);

/// Should the "USB lost" notice be on screen for this radio?
///
/// `absent` is the server's presence report, `we_hold_tx` is whether this
/// client is the one keying it, and `since_true` is how long ago the two were
/// last both the case - `None` when that has never happened.
pub fn showing(absent: bool, we_hold_tx: bool, since_true: Option<Duration>) -> bool {
    if absent && we_hold_tx {
        return true;
    }
    match since_true {
        Some(d) => d < MINIMUM,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_radio_that_is_merely_absent_says_nothing() {
        assert!(!showing(true, false, None));
    }

    #[test]
    fn absent_while_we_are_transmitting_says_it() {
        assert!(showing(true, true, None));
    }

    /// The one that the notice exists for. A one-second outage is over before
    /// anyone can look, and it is exactly the outage that says the aerial is
    /// coupling into the cable.
    #[test]
    fn it_stays_up_after_a_blink_so_it_can_be_read() {
        assert!(showing(false, true, Some(Duration::from_millis(1_000))));
        assert!(showing(false, false, Some(Duration::from_millis(2_999))));
    }

    /// And it does go away - at three seconds, not at whatever `MINIMUM`
    /// happens to say.
    ///
    /// The first version of this test wrote `Some(MINIMUM)`, which compares the
    /// constant with itself: `d < MINIMUM` is false for any value, so the
    /// notice could have been stretched to thirty seconds and nothing would
    /// have gone red. It sits on the transmit control, and this module argues
    /// on its own first page why it has to be short - so the number is part of
    /// the claim and belongs in the assertion (review finding).
    #[test]
    fn and_it_does_go_away_after_three_seconds() {
        assert!(showing(false, true, Some(Duration::from_millis(2_999))));
        assert!(!showing(false, true, Some(Duration::from_millis(3_001))));
        // Documentation, not a net: the two lines above already catch a change
        // to the value. This one says out loud which number they are about, and
        // a reviewer checked that removing it changes nothing.
        assert_eq!(MINIMUM, Duration::from_secs(3));
    }

    /// While it is still happening the clock does not matter: an outage that
    /// lasts a minute keeps saying so for that minute, not for three seconds.
    #[test]
    fn a_long_outage_keeps_saying_so() {
        assert!(showing(true, true, Some(Duration::from_secs(600))));
    }
}
