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

/// How long after the hang time the CW sign may still stand: the rise of MOX
/// at the start of a run, the playout delay and the way back of the server's
/// word all lie between the key and the confirmation.
pub const CW_SIGN_MARGIN_MS: u64 = 500;

/// Is this client keying CW on the air right now - for the sign on the PTT
/// button? Two things have to be true, and neither is enough alone.
///
/// **The server says the transmitter is on** (`server_tx`, the PTT flag on the
/// S-meter packets): the sign is a confirmation, not a wish. A key that is
/// refused, a server that does not know the packet, a link that is down - the
/// button stays grey, and that is the information.
///
/// **And this client is in a CW run**: the key is down, or the last edge is no
/// longer ago than the hang time plus a margin. Without this the sign would
/// also go up for a transmission that is not ours to claim.
pub fn cw_on_air(server_tx: bool, key_down: bool, since_last_edge_ms: Option<u64>, hang_ms: u16) -> bool {
    let in_run = key_down || since_last_edge_ms.map_or(false, |ms| ms <= u64::from(hang_ms) + CW_SIGN_MARGIN_MS);
    server_tx && in_run
}

/// What a PTT button shows over its ordinary face, strongest first - one rule
/// for all three buttons (Thetis in the main window, the two Yaesu windows).
/// Build 132 exists because a Yaesu face was wrong twice ("TX elsewhere" for a
/// server refusal, then grey while the 991A keyed); review 317 moved that
/// decision here, and the Thetis button, which had its own order inline in
/// the client, followed (owner's decision, 2026-09-19).
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum PttFace {
    /// There is no such transmitter on this server (Thetis not configured).
    Unavailable,
    /// The antenna position is receive-only (Amplitec).
    RxOnly,
    /// The USB cable is gone while we key the radio.
    UsbLost,
    /// A CW run of ours keys this transmitter (`cw_on_air`).
    Cw,
    /// The server still reads this radio's settings: it starts no
    /// transmission until that is done (`YaesuState::tx_start_refusal`).
    Loading,
    /// The server refused this transmitter for us a moment ago.
    Refused,
    /// What `ptt_button_with_own` says.
    Normal,
}

/// What can override a PTT button's ordinary face. A button leaves at false
/// what does not apply to it (a Yaesu has no receive-only antenna position,
/// Thetis no USB cable).
#[derive(Debug, Default, Clone, Copy)]
pub struct PttFaceInputs {
    pub unavailable: bool,
    pub rx_only: bool,
    pub usb_lost: bool,
    pub cw_sign: bool,
    /// The radio is still loading (`YaesuStatePacket::loading` != 0).
    pub loading: bool,
    /// Our own microphone transmission on it: a transmission already on air
    /// is shown as such, never hidden under "loading".
    pub own_tx: bool,
    pub refused: bool,
}

pub fn ptt_face(i: PttFaceInputs) -> PttFace {
    if i.unavailable {
        PttFace::Unavailable
    } else if i.rx_only {
        PttFace::RxOnly
    } else if i.usb_lost {
        PttFace::UsbLost
    } else if i.cw_sign {
        PttFace::Cw
    } else if i.loading && !i.own_tx {
        PttFace::Loading
    } else if i.refused {
        PttFace::Refused
    } else {
        PttFace::Normal
    }
}

#[cfg(test)]
mod ptt_face_tests {
    use super::*;

    fn face(unavailable: bool, rx_only: bool, usb_lost: bool, cw_sign: bool, refused: bool) -> PttFace {
        ptt_face(PttFaceInputs { unavailable, rx_only, usb_lost, cw_sign, refused, ..Default::default() })
    }

    #[test]
    fn a_loading_radio_says_so_unless_it_already_transmits() {
        let loading = PttFaceInputs { loading: true, ..Default::default() };
        assert_eq!(ptt_face(loading), PttFace::Loading);
        assert_eq!(ptt_face(PttFaceInputs { refused: true, ..loading }), PttFace::Loading, "the refusal is the loading");
        assert_eq!(ptt_face(PttFaceInputs { own_tx: true, ..loading }), PttFace::Normal, "on air: the ordinary red TX");
        assert_eq!(ptt_face(PttFaceInputs { cw_sign: true, ..loading }), PttFace::Cw, "keyed: say CW");
        assert_eq!(ptt_face(PttFaceInputs { usb_lost: true, ..loading }), PttFace::UsbLost);
    }

    #[test]
    fn the_yaesu_button_says_the_strongest_thing_first() {
        assert_eq!(face(false, false, true, true, true), PttFace::UsbLost);
        assert_eq!(face(false, false, false, true, true), PttFace::Cw, "keyed after all: the refusal is old news");
        assert_eq!(face(false, false, false, true, false), PttFace::Cw, "the ordinary case: keying, nothing refused (review 317 r2)");
        assert_eq!(face(false, false, false, false, true), PttFace::Refused);
        assert_eq!(face(false, false, false, false, false), PttFace::Normal);
    }

    /// The Thetis button's order, as it was inline in update.rs: no Thetis on
    /// this server, then a receive-only antenna, then the CW sign.
    #[test]
    fn the_thetis_button_keeps_its_order() {
        assert_eq!(face(true, true, false, true, false), PttFace::Unavailable);
        assert_eq!(face(false, true, false, true, false), PttFace::RxOnly, "receive-only beats the CW sign");
        assert_eq!(face(false, false, false, true, false), PttFace::Cw);
        assert_eq!(face(false, false, false, false, false), PttFace::Normal);
    }
}

#[cfg(test)]
mod cw_sign_tests {
    use super::*;

    #[test]
    fn the_cw_sign_needs_the_servers_word_and_a_run_of_our_own() {
        assert!(cw_on_air(true, true, None, 500), "key down, confirmed");
        assert!(cw_on_air(true, false, Some(300), 500), "between elements, confirmed");
        assert!(cw_on_air(true, false, Some(500 + CW_SIGN_MARGIN_MS), 500), "the last moment of the margin");
        assert!(!cw_on_air(true, false, Some(500 + CW_SIGN_MARGIN_MS + 1), 500), "the run is over: somebody else's transmission");
        assert!(!cw_on_air(true, false, None, 500), "never keyed: not ours");
        assert!(!cw_on_air(false, true, Some(0), 500), "keying, but the server does not say TX: grey");
        assert!(!cw_on_air(false, false, None, 500));
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
