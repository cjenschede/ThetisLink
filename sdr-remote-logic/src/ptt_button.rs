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

/// How long after this client let go of Thetis's PTT the meter may still
/// follow the server's word that it transmits: the tail (up to ~1 s since
/// build 168) plus the trip back.
pub const METER_TX_AFTER_RELEASE_MS: u64 = 2000;

/// Does the Thetis meter show transmit power (watts) or a signal (dBm)? The
/// meter packet carries watts for as long as the server still transmits, and
/// that outlasts this client's own PTT by the tail - drawn on the dBm scale,
/// 50 W read as +50 dBm and the needle hit the end at every release (owner,
/// 2026-10-11). So: our own PTT, or the server still transmitting shortly
/// after we let go. Not the server's flag alone: it also goes up for a Yaesu
/// (`RadioState::server_tx`), and then the meter carries dBm (build 169).
/// One rule for the desktop and the phone.
pub fn meter_shows_tx(own_ptt: bool, server_tx: bool, since_own_release_ms: Option<u64>) -> bool {
    own_ptt || (server_tx && since_own_release_ms.is_some_and(|ms| ms <= METER_TX_AFTER_RELEASE_MS))
}

/// The window after our own release in which the meter still follows the
/// server's word that it transmits. It opens at our release and closes once
/// the server has said "transmitting" since then and has stopped saying it
/// (build 171). Build 170 closed it on the first "not transmitting", and on a
/// short press that came before the server had even said "transmitting" -
/// the whole transmission then came in drawn as dBm (review of 170). With no
/// "transmitting" at all (a refused press) it is bounded by
/// `METER_TX_AFTER_RELEASE_MS`.
#[derive(Debug, Default)]
pub struct MeterWindow {
    last_ptt: bool,
    released_at_ms: Option<u64>,
    seen_server_tx: bool,
    open: bool,
}

impl MeterWindow {
    /// One tick: does the meter carry watts now?
    pub fn step(&mut self, own_ptt: bool, server_tx: bool, now_ms: u64) -> bool {
        if self.last_ptt && !own_ptt {
            self.released_at_ms = Some(now_ms);
            self.seen_server_tx = false;
        }
        self.last_ptt = own_ptt;
        if !own_ptt && self.released_at_ms.is_some() {
            if server_tx {
                self.seen_server_tx = true;
            } else if self.seen_server_tx {
                self.released_at_ms = None;
            }
        }
        let since = self.released_at_ms.map(|at| now_ms.saturating_sub(at));
        self.open = own_ptt || since.is_some_and(|ms| ms <= METER_TX_AFTER_RELEASE_MS);
        meter_shows_tx(own_ptt, server_tx, since)
    }

    /// Is our transmission still going on as far as the screens know: our
    /// own PTT, or the window after it - also in the gap between a short
    /// press and the server's first "transmitting", where the meter still
    /// shows a signal (build 172; review of 171). What the spectrum and the
    /// mode buttons wait for.
    pub fn open(&self) -> bool {
        self.open
    }
}

/// Whether the automatic reference of the spectra may measure: not while a
/// transmission is on - ours (`MeterWindow::open`) or someone else's - and not
/// for `AUTO_REF_AFTER_TX_MS` after it, when the receive spectrum is only
/// coming back. During a transmission the receive data is nearly empty (-135
/// dB, `veldproef-build174.md`); measured on, the reference would sink there.
/// Paused, it keeps its last receive value and carries on from it, instead of
/// starting over (owner, 2026-10-11, choice A; build 176: the spectrum no
/// longer switches to transmit settings).
#[derive(Debug, Default)]
pub struct AutoRefHold {
    last_busy_ms: Option<u64>,
}

impl AutoRefHold {
    /// One tick: whether a transmission is on, and the clock. Whether the
    /// automatic reference pauses now.
    pub fn step(&mut self, busy: bool, now_ms: u64) -> bool {
        if busy {
            self.last_busy_ms = Some(now_ms);
            return true;
        }
        self.last_busy_ms
            .is_some_and(|at| now_ms.saturating_sub(at) < crate::state::AUTO_REF_AFTER_TX_MS)
    }
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
    /// The chosen microphone is missing: this client sends no voice PTT until
    /// it is back (builds 188, 191).
    NoMic,
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
    /// The chosen microphone is missing (desktop, build 191).
    pub mic_missing: bool,
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
    } else if i.mic_missing {
        // After CW: a CW run needs no microphone and keeps its sign.
        PttFace::NoMic
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

    /// No microphone: every button says so, over everything but the faces that
    /// say there is no transmitter at all, and under a CW run (build 191).
    #[test]
    fn a_missing_microphone_shows_on_the_button() {
        let no_mic = PttFaceInputs { mic_missing: true, ..Default::default() };
        assert_eq!(ptt_face(no_mic), PttFace::NoMic);
        assert_eq!(ptt_face(PttFaceInputs { cw_sign: true, ..no_mic }), PttFace::Cw, "CW needs no microphone");
        assert_eq!(ptt_face(PttFaceInputs { refused: true, ..no_mic }), PttFace::NoMic);
        assert_eq!(ptt_face(PttFaceInputs { loading: true, ..no_mic }), PttFace::NoMic);
        assert_eq!(ptt_face(PttFaceInputs { unavailable: true, ..no_mic }), PttFace::Unavailable);
        assert_eq!(ptt_face(PttFaceInputs { rx_only: true, ..no_mic }), PttFace::RxOnly);
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

    /// A short press: released before the server said "transmitting". The
    /// window stays open for the transmission that follows and closes when
    /// it ends (review of 170: build 170 closed it at once). A press the
    /// server never answers is bounded by the 2 s (build 171).
    #[test]
    fn a_short_press_keeps_the_window_for_its_own_transmission() {
        let mut w = MeterWindow::default();
        assert!(w.step(true, false, 0), "our press");
        assert!(!w.step(false, false, 60), "released before the server said anything - no watts yet");
        assert!(w.step(false, true, 120), "the server's transmission arrives: watts");
        assert!(w.step(false, true, 300), "its tail: still watts");
        assert!(!w.step(false, false, 320), "it stopped: a signal again");
        assert!(!w.step(false, true, 400), "a later rise is someone else's");
        // The second pass on the same window (working rule 9; review of 171):
        // an ordinary press, then a short one - the short one's own
        // transmission must still count.
        assert!(w.step(true, true, 1000));
        assert!(w.step(false, true, 1100), "ordinary release: the tail");
        assert!(!w.step(false, false, 1200), "closed");
        assert!(w.step(true, false, 2000), "a short press");
        assert!(w.open(), "our own press is open (review of 172, Z5)");
        assert!(!w.step(false, false, 2050), "released before the server said anything");
        assert!(w.open(), "but the window is open for the transmission that follows");
        assert!(w.step(false, true, 2100), "that transmission: watts, not closed by the last one's leftovers");
        assert!(!w.step(false, false, 2300));
        assert!(!w.open(), "and closed when it ends");
        // The ordinary press: transmitting already at the release.
        let mut w = MeterWindow::default();
        w.step(true, true, 0);
        assert!(w.step(false, true, 1000));
        assert!(!w.step(false, false, 1100));
        // A refused press: never transmitting; closed by the bound.
        let mut w = MeterWindow::default();
        w.step(true, false, 0);
        w.step(false, false, 20);
        assert!(w.step(false, true, 20 + METER_TX_AFTER_RELEASE_MS), "within the bound a rise still counts");
        let mut w = MeterWindow::default();
        w.step(true, false, 0);
        w.step(false, false, 20);
        assert!(!w.step(false, true, 21 + METER_TX_AFTER_RELEASE_MS), "past the bound it does not");
        assert!(!w.open(), "nor is the window open");
    }

    /// The automatic reference pauses during a transmission and for
    /// `AUTO_REF_AFTER_TX_MS` after it, and again for a second one on the same
    /// hold (working rule 9; build 176).
    #[test]
    fn the_auto_reference_pauses_through_a_transmission() {
        let mut h = AutoRefHold::default();
        assert!(!h.step(false, 0), "never transmitted: it measures");
        assert!(h.step(true, 100), "transmitting: paused");
        assert!(h.step(false, 599), "just after: still paused");
        assert!(!h.step(false, 100 + crate::state::AUTO_REF_AFTER_TX_MS), "after the wait: it measures again");
        // The second transmission, same hold.
        assert!(h.step(true, 5000));
        assert!(h.step(false, 5000 + crate::state::AUTO_REF_AFTER_TX_MS - 1));
        assert!(!h.step(false, 5000 + crate::state::AUTO_REF_AFTER_TX_MS));
    }

    /// The meter stays on watts while the server still transmits after our
    /// release, and only then; a Yaesu's flag alone never turns it (build 169).
    #[test]
    fn the_meter_follows_the_tail_of_our_own_transmission() {
        assert!(meter_shows_tx(true, false, None), "our PTT: at once, before the server says so");
        assert!(meter_shows_tx(false, true, Some(80)), "the tail after our release: still watts");
        assert!(!meter_shows_tx(false, false, Some(80)), "the server stopped: a signal again");
        assert!(!meter_shows_tx(false, true, None), "a transmitter we never keyed - a Yaesu - is not ours");
        assert!(meter_shows_tx(false, true, Some(METER_TX_AFTER_RELEASE_MS)));
        assert!(!meter_shows_tx(false, true, Some(METER_TX_AFTER_RELEASE_MS + 1)), "long after: someone else's");
        assert_eq!(METER_TX_AFTER_RELEASE_MS, 2000);
        // The engine feeds it, from our own release (a text guard: it pins
        // that the calls are there, not that they run).
        let engine = include_str!("engine.rs");
        // The whole wiring, every tick and not only on the edge (reviews of
        // 169 and 170).
        assert_eq!(engine.matches("\
                    yaesu_tx[1].ptt(yaesu2_ptt);
                    state.smeter_tx = meter_window.step(ptt, state.server_tx, start.elapsed().as_millis() as u64);
                    state.tx_window_open = meter_window.open();
                    state.auto_ref_paused = auto_ref_hold.step(state.tx_window_open || state.other_tx, start.elapsed().as_millis() as u64);").count(), 1);
        assert_eq!(engine.matches("state.tx_window_open = ").count(), 1);
        assert_eq!(engine.matches("state.smeter_tx = ").count(), 1);
        // Every Thetis meter on the desktop draws its scale from it, and the
        // phone's PTT flag is left to what is not a meter.
        for (name, ui) in [
            ("update.rs", include_str!("../../sdr-remote-client/src/ui/update.rs")),
            ("rx_controls.rs", include_str!("../../sdr-remote-client/src/ui/rx_controls.rs")),
        ] {
            assert_eq!(ui.matches(", self.ptt, self.other_tx").count(), 0, "{name}");
        }
        let content = include_str!("../../sdr-remote-client/src/ui/spectrum_content.rs");
        assert!(content.contains("self.smeter,
            self.smeter_tx,"));
        let main = include_str!("../../sdr-remote-android/android/app/src/main/java/com/sdrremote/ui/screens/MainScreen.kt");
        assert_eq!(main.matches("transmitting = state.transmitting").count(), 1, "only the PA panel");
        assert_eq!(main.matches("transmitting = state.meterTx").count(), 2);
        // The phone no longer switches its spectrum at TX, and its automatic
        // reference pauses on the engine's word (build 176).
        assert_eq!(main.matches("spectrumRangeState.floatValue = 120f").count(), 0, "no transmit settings for the spectrum");
        assert_eq!(main.matches("if (!autoRefEnabled || state.autoRefPaused) return@LaunchedEffect").count(), 1);
        // And the phone is handed the engine's word (review of 176, H5).
        let view_model = include_str!("../../sdr-remote-android/android/app/src/main/java/com/sdrremote/viewmodel/SdrViewModel.kt");
        assert_eq!(view_model.matches("autoRefPaused = s.autoRefPaused,").count(), 1);
        // The desktop's peak needle and spectrum take their cue from the meter
        // (review of 170: P3-P7).
        let sync = include_str!("../../sdr-remote-client/src/ui/sync_state.rs");
        assert!(sync.contains("            state.smeter,\n            state.smeter_tx || state.other_tx,\n        );"), "the peak needle's cue");
        // The desktop no longer switches its spectrum at TX; all four
        // automatic references pause on the engine's word (build 176).
        assert_eq!(sync.matches("self.auto_ref_paused = state.auto_ref_paused;").count(), 1);
        assert_eq!(sync.matches("&& !self.auto_ref_paused").count(), 4, "RX1, RX2, VRX1, VRX2");
        assert!(sync.contains("self.tx_window_open = state.tx_window_open;"));
        // The mode buttons wait for the end of the transmission (build 172).
        let update_rs = include_str!("../../sdr-remote-client/src/ui/update.rs");
        assert!(update_rs.contains("    pub(crate) fn thetis_mode_locked(&self) -> bool {\n        self.ptt || self.tx_window_open || self.other_tx\n    }"));
        assert_eq!(update_rs.matches("let ptt_active = self.thetis_mode_locked();").count(), 1);
        let rx_controls = include_str!("../../sdr-remote-client/src/ui/rx_controls.rs");
        assert_eq!(rx_controls.matches("let ptt_active = self.thetis_mode_locked();").count(), 1);
        // And the lock is used, not only computed (review of 172, Z7/Z8).
        for (name, ui) in [("rx_controls.rs", rx_controls), ("update.rs", update_rs)] {
            assert_eq!(ui.matches("ui.add_enabled_ui(!ptt_active, |ui| controls::render_mode_selector(ui, ctx))").count(), 1, "{name}");
        }
        let persistence = include_str!("../../sdr-remote-client/src/ui/persistence.rs");
        assert_eq!(persistence.matches("let block_mode = matches!(vfo, Vfo::A) && self.thetis_mode_locked();").count(), 1);
        let update = include_str!("../../sdr-remote-client/src/ui/update.rs");
        assert_eq!(update.matches("tx_spectrum_saved").count(), 0, "no transmit settings to save any more (build 176)");
        // The phone's copies of the spectrum defaults and the auto-ref wait:
        // two languages, so held equal here (build 171, working rule 15).
        use crate::state::{SPECTRUM_AUTO_REF_DEFAULT, SPECTRUM_RANGE_DEFAULT_DB};
        assert_eq!(main.matches(&format!("prefs.getFloat(\"spectrum_range\", {}f)", SPECTRUM_RANGE_DEFAULT_DB as i32)).count(), 1);
        assert_eq!(main.matches(&format!("prefs.getBoolean(\"auto_ref_enabled\", {})", SPECTRUM_AUTO_REF_DEFAULT)).count(), 1);

    }

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
