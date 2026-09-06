// SPDX-License-Identifier: GPL-2.0-or-later

//! What a PTT button shows, for every transmitter and every front end.
//!
//! This has been wrong twice in two places, each time the same way: the button
//! was painted from the transmitter's state instead of from the operator's own
//! intent. Those are different questions. "The radio is transmitting" is just as
//! true when somebody else is doing it, and it arrives late - after the command,
//! the radio, the readback and the status packet. An operator pressing his own
//! button should not wait for a round trip to see that he pressed it.
//!
//! So the button reads two things and nothing else: does somebody else hold this
//! transmitter, and did I ask for it. The radio's own TX state belongs on the
//! meter and the status line, where being late costs nothing.

/// What the button should look like.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PttButton {
    /// Another client holds this transmitter. Shown, and not pressable.
    Busy,
    /// We asked for it. Shown the moment we ask, before any confirmation - the
    /// server may still refuse, and then it takes this back (PttDenied).
    Transmitting,
    /// Free, and we are not asking.
    Idle,
    /// We asked, and this client refused: it allows one transmitter at a time
    /// and another one has it.
    ///
    /// A different thing from [`Busy`](PttButton::Busy), and it has to look
    /// different: there the radio is taken by somebody else and waiting is the
    /// answer; here the operator has it himself, one screen over, and letting go
    /// there is the answer. Telling him "in use" would send him looking for a
    /// stranger (eigenaar, 2026-09-03).
    BlockedByOwnTx,
}

impl PttButton {
    /// True while this button cannot key anything - it must be disabled.
    pub fn locked(self) -> bool {
        matches!(self, PttButton::Busy | PttButton::BlockedByOwnTx)
    }
}

/// Decide the button state.
///
/// `held_by_other` comes from the server's ownership table, never from the
/// radio's TX flag. `want_tx` is our own request - the thing we last told the
/// server, not what came back.
///
/// Busy wins over wanting it: with the transmitter taken our request is not
/// going anywhere, and showing red would be a promise the server will not keep.
pub fn ptt_button(held_by_other: bool, want_tx: bool) -> PttButton {
    ptt_button_with_own(held_by_other, want_tx, false)
}

/// The same, with the third answer: our own other transmission is in the way.
///
/// Ordering matters and it is deliberate. Somebody else holding the radio comes
/// first: that is true regardless of what we are doing, and it is the older and
/// more surprising fact. Then our own block, then our own request.
pub fn ptt_button_with_own(
    held_by_other: bool,
    want_tx: bool,
    blocked_by_own_tx: bool,
) -> PttButton {
    if held_by_other {
        PttButton::Busy
    } else if blocked_by_own_tx {
        PttButton::BlockedByOwnTx
    } else if want_tx {
        PttButton::Transmitting
    } else {
        PttButton::Idle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_own_press_shows_at_once() {
        // No radio state in the question at all: this is what "optimistic" means.
        assert_eq!(ptt_button(false, true), PttButton::Transmitting);
        assert!(!ptt_button(false, true).locked());
    }

    #[test]
    fn somebody_else_holding_it_is_busy_and_locked() {
        assert_eq!(ptt_button(true, false), PttButton::Busy);
        assert!(ptt_button(true, false).locked());
    }

    #[test]
    fn busy_beats_our_own_request() {
        assert_eq!(ptt_button(true, true), PttButton::Busy);
    }

    /// The release edge, from both sides.
    ///
    /// Our own release: the request drops in the same breath, so Idle. The other
    /// client's release: their hold drops while the radio still reports TX for
    /// one more poll - and because the radio is not in this decision, that poll
    /// cannot paint the button red. That red tick was the reported fault.
    #[test]
    fn neither_release_edge_flashes() {
        assert_eq!(ptt_button(false, false), PttButton::Idle);
    }
}

#[cfg(test)]
mod own_tx_tests {
    use super::*;

    #[test]
    fn our_own_other_transmission_is_its_own_answer() {
        assert_eq!(ptt_button_with_own(false, true, true), PttButton::BlockedByOwnTx);
        assert!(ptt_button_with_own(false, true, true).locked());
    }

    /// Somebody else holding the transmitter outweighs a block of our own: that is
    /// true whatever we do, and it is the more surprising fact.
    #[test]
    fn somebody_else_holding_it_comes_first() {
        assert_eq!(ptt_button_with_own(true, true, true), PttButton::Busy);
    }

    /// Without a block of our own nothing changes about the old behaviour.
    #[test]
    fn without_an_own_block_nothing_changed() {
        for held in [false, true] {
            for want in [false, true] {
                assert_eq!(
                    ptt_button_with_own(held, want, false),
                    ptt_button(held, want),
                    "held={held} want={want}"
                );
            }
        }
    }
}
