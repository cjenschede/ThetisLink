// SPDX-License-Identifier: GPL-2.0-or-later

//! When the operator listens to a Yaesu, the Thetis streams should stop.
//!
//! A long-standing requirement, restated on 2026-09-13: switching to a
//! Yaesu may leave the Thetis audio and spectrum running for ten to twenty
//! seconds - so switching straight back costs nothing - and after that they
//! should stop, because on a phone you are paying for a stream you cannot hear.
//!
//! What happened instead: the audio never stopped at all (the switch only muted
//! it locally, so roughly 20 kB/s kept arriving over mobile data), and the
//! spectrum stopped immediately with no grace at all. Both halves wrong, each in
//! the opposite direction. The label beside the switch said "Thetis off" while a
//! full audio stream was running.
//!
//! **Why this lives in the engine and not in the Android screen.** The switch is
//! an Android idea - the desktop has no "listening radio", it shows everything at
//! once - so there is no second front-end to share it with. Two other reasons
//! decide it:
//!
//! - there is no Kotlin test harness in this project, and this is a timing rule
//!   with three states. Here it is a pure function with tests.
//! - the stop travels as a real unsubscribe through the send path, so
//!   [`crate::session_intent`] records it and replays it after a network switch.
//!   A saving that a reconnect silently undoes is not a saving, and that
//!   interlock is the whole reason this waited for the re-subscribe patch.

use std::time::Duration;

/// How long the Thetis streams keep running after the operator switches to a
/// Yaesu. The requirement is "ten to twenty seconds"; the upper end is chosen
/// because the cost of being wrong is asymmetric - twenty seconds of audio is a
/// few tens of kilobytes, while a switch-back that has to re-subscribe is a gap
/// the operator hears.
pub const THETIS_STREAM_GRACE: Duration = Duration::from_secs(20);

/// What the engine should do about the Thetis streams this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThetisStreamAction {
    /// Leave them as they are.
    None,
    /// Unsubscribe: stop paying for audio and spectrum nobody is listening to.
    Stop,
    /// Subscribe again, at once - the operator is back on Thetis and waiting.
    Resume,
}

/// The decision, with no clock and no sockets in it so it can be tested.
///
/// * `yaesu_listening` - the operator has a Yaesu as the radio he is hearing.
/// * `stopped` - the streams are currently unsubscribed.
/// * `since_switch` - how long ago `yaesu_listening` became true; `None` when it
///   is false.
pub fn thetis_stream_action(
    yaesu_listening: bool,
    stopped: bool,
    since_switch: Option<Duration>,
    grace: Duration,
) -> ThetisStreamAction {
    if !yaesu_listening {
        // Back on Thetis. Resuming is immediate and never waits for a grace of
        // its own: the operator is looking at a dead waterfall until it returns.
        return if stopped { ThetisStreamAction::Resume } else { ThetisStreamAction::None };
    }
    if stopped {
        return ThetisStreamAction::None;
    }
    match since_switch {
        Some(d) if d >= grace => ThetisStreamAction::Stop,
        _ => ThetisStreamAction::None,
    }
}

/// What a Thetis stream control should carry ON THE WIRE.
///
/// One place, because there are three: the tick that stops and resumes, and the
/// two sends in the engine's restore block after a new session. The restore block
/// sent the operator's wish unconditionally and ran AFTER the replay, so every
/// reconnect switched the saved stream back on and left it on - the exact failure
/// the grace period exists to prevent, reintroduced one layer down. Found by
/// trying to state the remaining doubt honestly.
pub fn stream_value(operator_wants: bool, thetis_streams_stopped: bool) -> bool {
    operator_wants && !thetis_streams_stopped
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRACE: Duration = THETIS_STREAM_GRACE;

    #[test]
    fn switching_to_the_yaesu_does_not_stop_anything_yet() {
        // The whole point of the grace: switch back within it and there is
        // nothing to re-subscribe, so no gap to hear.
        assert_eq!(
            thetis_stream_action(true, false, Some(Duration::from_secs(5)), GRACE),
            ThetisStreamAction::None
        );
    }

    #[test]
    fn after_the_grace_the_streams_stop() {
        assert_eq!(
            thetis_stream_action(true, false, Some(GRACE), GRACE),
            ThetisStreamAction::Stop
        );
    }

    #[test]
    fn stopping_happens_once_and_not_every_tick() {
        // The engine runs this per tick. Without the `stopped` guard it would
        // send an unsubscribe fifty times a second for as long as the operator
        // stays on the Yaesu.
        assert_eq!(
            thetis_stream_action(true, true, Some(Duration::from_secs(600)), GRACE),
            ThetisStreamAction::None
        );
    }

    #[test]
    fn switching_back_resumes_immediately_even_inside_the_grace() {
        assert_eq!(
            thetis_stream_action(false, true, None, GRACE),
            ThetisStreamAction::Resume
        );
    }

    #[test]
    fn switching_back_when_nothing_was_stopped_does_nothing() {
        // Switched away and back within the grace: nothing was ever unsubscribed,
        // so there is nothing to ask for again. Sending a redundant subscribe
        // here would restart the server's sequence and cost the gap the grace
        // exists to avoid.
        assert_eq!(
            thetis_stream_action(false, false, None, GRACE),
            ThetisStreamAction::None
        );
    }

    #[test]
    fn a_switch_with_no_clock_yet_waits_rather_than_stops() {
        // Defensive: if the caller has not recorded when the switch happened,
        // the safe reading is "not yet", not "long ago". Stopping a stream on a
        // missing measurement is how a silence gets blamed on the radio.
        assert_eq!(
            thetis_stream_action(true, false, None, GRACE),
            ThetisStreamAction::None
        );
    }

    #[test]
    fn a_saved_stream_stays_saved_across_a_reconnect() {
        // The restore after a new session must not hand back what the grace
        // period just gave up. Operator wants RX1, but we stopped it to save
        // data: the wire value is off until he switches back to Thetis.
        assert_eq!(stream_value(true, true), false);
    }

    #[test]
    fn a_stream_the_operator_switched_off_stays_off_too() {
        assert_eq!(stream_value(false, false), false);
    }

    #[test]
    fn a_wanted_stream_that_was_never_stopped_is_on() {
        assert_eq!(stream_value(true, false), true);
    }

    /// How many places in the engine DECIDE a Thetis stream value.
    ///
    /// A presence test - does engine.rs mention stream_value at all - stayed
    /// green while two call sites bypassed it with the bare wish. The defect was
    /// a COUNT, not an absence.
    ///
    /// Eight, and all eight are decisions: two in the tick, two in the restore
    /// after a new session, two in the command handlers, one in the loss
    /// window, and one for the FFT size, which goes to the server only from a
    /// client whose spectrum the server feeds. The loss-window one added after it turned out that the client charged
    /// a hundred percent loss for a stream it had itself asked to stop, which the
    /// server then turned into maximum protection for every other listener.
    /// The log line used to call it
    /// as well, which put two NON-decisions into a count meant to count
    /// decisions - so tidying the log would have turned this red and adding a
    /// log line could have masked a decision being removed. To
    /// remove that compensating pair, the tick now computes the two values once and the
    /// log reuses them.
    #[test]
    fn every_place_that_decides_a_thetis_stream_value_uses_this_function() {
        let src = include_str!("engine.rs");
        let uses = src.matches("stream_value(").count();
        assert_eq!(
            uses, 8,
            "engine.rs uses stream_value {uses} times, expected 8. Fewer means a place decides a Thetis stream value on its own, which is how the saving was silently defeated once already. More means a new decision point nobody has classified. Look first, then move this number deliberately."
        );
    }

    /// The FFT size goes with the stream the saving leaves on the wire: one
    /// the saving opens again gets the size its rows show, as one the operator
    /// opens (a size chosen while it was stopped did not go live), and an open
    /// while it is stopped sends none (nobody looks).
    #[test]
    fn the_fft_size_follows_the_saving() {
        let src = include_str!("engine.rs");
        let src = &src[..src.find("#[cfg(test)]\nmod tests {").unwrap()];
        let tick = &src[src.find("let sp_ok = send_control!(ControlId::SpectrumEnable, spec_wire as u16").unwrap()..];
        let tick = &tick[..tick.find("} else {").unwrap()];
        assert!(tick.contains("if rx_ok && sp_ok {\n                                    thetis_streams_stopped = !want;\n"));
        assert!(tick.contains("if spec_wire && spectrum_fft_size_k != 0 {\n                                        let _ = send_control!(ControlId::SpectrumFftSize, spectrum_fft_size_k, addr.as_str());"));
        let open = &src[src.find("Command::EnableSpectrum(enabled) => {").unwrap()..];
        let open = &open[..open.find("Command::SetSpectrumFps").unwrap()];
        assert!(open.contains("let on = crate::thetis_streams::stream_value(\n                                    enabled, thetis_streams_stopped);"));
        assert!(open.contains("if on && !was_open && spectrum_fft_size_k != 0 {"));
    }

    #[test]
    fn the_grace_stays_between_ten_and_twenty_seconds() {
        // Pinned deliberately: the request was "ten to twenty seconds", and a
        // constant that drifts out of that range without anyone deciding is the
        // kind of change this project has been caught by before.
        assert!(THETIS_STREAM_GRACE >= Duration::from_secs(10));
        assert!(THETIS_STREAM_GRACE <= Duration::from_secs(20));
    }
}
