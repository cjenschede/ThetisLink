// SPDX-License-Identifier: GPL-2.0-or-later

//! The transmit decision for a Bluetooth PTT button.
//!
//! This lives here, and not next to the GATT callback on Android, for one
//! reason: the rule it enforces is the one that puts a licence holder on the
//! air without knowing it, and a rule that can only be checked by a person
//! holding a button inside a metal tin is not checked.
//!
//! Everything here is a pure function of the events that arrive. No threads,
//! no Android, no button.

/// What the button reports on `FFE0`/`FFE1`. Measured, see the patch brief.
const PRESSED: u8 = 0x01;
const RELEASED: u8 = 0x00;

/// What happened on the link, as far as the connection manager can tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BleButtonEvent {
    /// GATT is up and notifications are enabled.
    Connected,
    /// The link is gone, for any reason at all: out of range, Bluetooth off,
    /// the feature switched off, the app tearing down. The reason does not
    /// matter and must not matter.
    Disconnected,
    /// One notification byte, exactly as it arrived.
    Notification(u8),
}

/// What the caller must do. `None` means: nothing changed, touch nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlePttAction {
    /// Start transmitting.
    KeyDown,
    /// The button was let go. What that means - stop, or nothing at all in
    /// toggle mode - is not decided here; see `PhonePtt::up`.
    KeyUp,
    /// This button is no longer holding anything, whatever anyone believed.
    ///
    /// Every link event: a drop, a connect, a new attempt. Unconditional, and
    /// that is the point - in toggle mode the button is not held while the
    /// transmitter is on, so the gate cannot tell from the outside whether
    /// there is something to release. Releasing nothing costs nothing;
    /// releasing nothing when there was something costs four seconds of
    /// unattended carrier.
    Gone,
    /// A byte this button is not known to send. Log it and change nothing —
    /// this device already turned out to send more than was documented.
    Unrecognised(u8),
}

/// What to do with the connection after the link went away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropPolicy {
    /// Keep the client alive and let the platform watch for the button to come
    /// back. The user still wants this button, so waiting costs nothing.
    Reconnect,
    /// Let the client go. Nothing is waiting for this button any more, and a
    /// client left watching is a callback that can still fire later.
    Close,
}

/// Should a dropped link be waited for, or let go?
///
/// One input now. It had two - `wanted` and `same_client` - and a review showed
/// that the second half had quietly become unreachable: the staleness check ran
/// ahead of this call, so `same_client` was true every time it was asked. Two
/// of the four rows were guarded by a test that the scenario could no longer
/// reach, which is worse than no test, because the green counted for something
/// it did not cover.
///
/// Staleness now belongs to [`BlePttState`], which knows the generation. This
/// answers the one question left: the link is gone and it is ours - wait for it,
/// or let the client go?
pub fn on_drop(wanted: bool) -> DropPolicy {
    if wanted {
        DropPolicy::Reconnect
    } else {
        DropPolicy::Close
    }
}

/// What this button is doing, and nothing more.
///
/// It used to decide whether a press latched or had to be held, and to keep its
/// own `transmitting` flag to remember what it had decided. That made it a
/// fourth copy of "am I transmitting" - after the engine, the ViewModel and the
/// Compose button - and the exits could not reach it. Stop the transmitter from
/// the screen while this button was toggled on, and the gate still believed it
/// was on: the next press turned it "off" and did nothing at all, and only the
/// press after that keyed the radio (owner, build 59).
///
/// So it keeps only what nothing else can know: which connection attempt an
/// event belongs to, and whether the physical button is down. What a press
/// means is decided once, in `PhonePtt`.
#[derive(Debug, Default)]
pub struct BlePttState {
    /// Is the physical button down, as far as the notifications say?
    held: bool,
    /// Which connection attempt this state belongs to.
    ///
    /// Events carry the generation they were born in. Anything from an older
    /// one is a message from a connection nobody is listening to any more - the
    /// user switched off, or picked a different button, and a callback was
    /// still in flight. It used to be compared in Kotlin, where no test could
    /// reach it.
    generation: u64,
}

impl BlePttState {
    pub fn new() -> Self {
        Self::default()
    }

    /// A new connection attempt starts, and it is now the only one that counts.
    ///
    /// Releases whatever was held: a new attempt means the previous link is
    /// gone, and a press cannot survive that - the button only reports changes,
    /// so nothing would ever come to end it.
    pub fn begin(&mut self, generation: u64) -> Option<BlePttAction> {
        self.generation = generation;
        self.gone()
    }

    /// Is this event from the connection we are listening to?
    pub fn is_current(&self, generation: u64) -> bool {
        generation == self.generation
    }

    /// Is the physical button down?
    ///
    /// For a log line, not for a decision. Whether that means transmitting is
    /// `PhonePtt`'s to say - in toggle mode this is false while the radio is on
    /// the air.
    pub fn held(&self) -> bool {
        self.held
    }

    pub fn apply(&mut self, generation: u64, event: BleButtonEvent) -> Option<BlePttAction> {
        // A message from a connection nobody is listening to any more: the user
        // switched off or picked a different button, and this callback was
        // still in flight. It changes nothing - not the transmitter, and not
        // the state a later event will be judged against.
        if !self.is_current(generation) {
            return None;
        }
        match event {
            // Says nothing about the button. The state cannot be read back, so
            // the only safe assumption is that it is not held.
            BleButtonEvent::Connected => self.gone(),

            // Every drop releases, whatever the reason and whether or not a
            // release byte ever arrived. This line is the whole point of the
            // module.
            BleButtonEvent::Disconnected => self.gone(),

            // The button went down or came up. That is all this reports; a
            // press is not a start and a release is not a stop until the shared
            // rule says which it is.
            BleButtonEvent::Notification(PRESSED) => self.press(),
            BleButtonEvent::Notification(RELEASED) => self.release(),
            BleButtonEvent::Notification(other) => Some(BlePttAction::Unrecognised(other)),
        }
    }

    /// The button went down. The same byte twice is one press: both channels
    /// of this device fire within two milliseconds of each other.
    fn press(&mut self) -> Option<BlePttAction> {
        if self.held {
            return None;
        }
        self.held = true;
        Some(BlePttAction::KeyDown)
    }

    /// The button came up. A release without a press reports nothing - this
    /// module never speaks about a press it did not see.
    fn release(&mut self) -> Option<BlePttAction> {
        if !self.held {
            return None;
        }
        self.held = false;
        Some(BlePttAction::KeyUp)
    }

    /// The link is gone. Always reported, whether or not the button was down -
    /// see [`BlePttAction::Gone`].
    fn gone(&mut self) -> Option<BlePttAction> {
        self.held = false;
        Some(BlePttAction::Gone)
    }
}

#[cfg(test)]
mod tests {
    use super::BleButtonEvent::*;

    use super::BlePttAction::*;
    use super::*;

    /// Drive a fresh machine through a sequence and collect what it asked for.
    fn run(events: &[BleButtonEvent]) -> (BlePttState, Vec<BlePttAction>) {
        let mut state = BlePttState::new();
        state.begin(1);
        let actions = events.iter().filter_map(|e| state.apply(1, *e)).collect();
        (state, actions)
    }

    /// The rule from section 3 of the brief, and the reason this module exists.
    ///
    /// The link drops while the button is held. The release byte never arrives
    /// — it cannot, the link is gone. If nothing else stops it, the radio keeps
    /// transmitting with nobody reporting it. The supervision timeout is
    /// 4000 ms, so this is four seconds of unattended carrier.
    #[test]
    fn a_link_lost_while_held_stops_the_transmitter() {
        let (state, actions) = run(&[Connected, Notification(PRESSED), Disconnected]);

        assert!(
            !state.held(),
            "the link went away while the button was held and it is still transmitting"
        );
        assert_eq!(
            actions,
            vec![Gone, KeyDown, Gone],
            "a drop mid-press must release, even though no release byte arrived"
        );
    }

    /// Connecting says nothing about whether the button is down. The current
    /// state cannot be read back — notifications only carry changes — so the
    /// only safe assumption is that it is not.
    #[test]
    fn connecting_never_starts_a_transmission() {
        let (state, actions) = run(&[Connected]);

        assert!(!state.held());
        assert_eq!(actions, vec![Gone], "connecting must release, never key");
    }

    /// The same byte twice is one press, not two. Both channels of this button
    /// fire within two milliseconds of each other, and a duplicate must not
    /// key a second time.
    #[test]
    fn a_repeated_press_keys_once() {
        let (state, actions) = run(&[Connected, Notification(PRESSED), Notification(PRESSED)]);

        assert!(state.held());
        assert_eq!(actions, vec![Gone, KeyDown], "the second press must change nothing");
    }

    /// A release with nothing to release. Emitting a stop here would clear a
    /// transmission started somewhere else — the screen button, the volume
    /// keys — which is not this module's to end.
    #[test]
    fn a_release_without_a_press_does_nothing() {
        let (state, actions) = run(&[Connected, Notification(RELEASED)]);

        assert!(!state.held());
        assert_eq!(
            actions,
            vec![Gone],
            "the connect releases; the stray byte itself says nothing"
        );
    }

    /// After a drop the machine is idle. A release that arrives late, or on
    /// the next connection, must not be mistaken for anything.
    #[test]
    fn a_release_after_reconnecting_does_not_transmit() {
        let mut state = BlePttState::new();
        state.begin(1);
        state.apply(1, Connected);
        state.apply(1, Notification(PRESSED));

        // Stepped, not collected. Naive code that ignores the drop and stops
        // on the late release ends up with the same list of actions as correct
        // code; only the moment tells them apart.
        assert_eq!(
            state.apply(1, Disconnected),
            Some(Gone),
            "the stop belongs to the drop, not to whatever arrives later"
        );
        assert!(!state.held());

        assert_eq!(state.apply(1, Connected), Some(Gone));
        assert_eq!(
            state.apply(1, Notification(RELEASED)),
            None,
            "the late release has nothing left to release"
        );
        assert!(!state.held());
    }

    /// This device sends bytes nobody documented — that is how the second
    /// notification channel was found. An unknown byte is worth a log line and
    /// nothing else; guessing at it is how a transmitter gets keyed by
    /// accident.
    #[test]
    fn an_unknown_byte_changes_nothing() {
        let (state, actions) = run(&[Connected, Notification(0xAB), Notification(0x7F)]);

        assert!(!state.held());
        assert_eq!(actions, vec![Gone, Unrecognised(0xAB), Unrecognised(0x7F)]);
    }

    /// And an unknown byte in the middle of a press leaves the press alone.
    #[test]
    fn an_unknown_byte_does_not_interrupt_a_press() {
        let (state, actions) = run(&[Connected, Notification(PRESSED), Notification(0xCD)]);

        assert!(state.held(), "the button is still held");
        assert_eq!(actions, vec![Gone, KeyDown, Unrecognised(0xCD)]);
    }

    /// A connect reported without a disconnect in front of it. The manager may
    /// hand up a fresh connection callback without ever calling the drop, and
    /// then this is the only line that clears a press left standing.
    #[test]
    fn reconnecting_without_a_reported_drop_still_releases() {
        let mut state = BlePttState::new();
        state.begin(1);
        state.apply(1, Connected);
        state.apply(1, Notification(PRESSED));

        assert_eq!(
            state.apply(1, Connected),
            Some(Gone),
            "a new connection starts in not transmitting, whatever came before"
        );
        assert!(!state.held());
    }

    /// What is left of the drop rule once staleness moved where it belongs.
    #[test]
    fn a_wanted_button_is_waited_for_and_a_switched_off_one_is_not() {
        use super::{on_drop, DropPolicy};
        assert_eq!(on_drop(true), DropPolicy::Reconnect);
        assert_eq!(on_drop(false), DropPolicy::Close);
    }

    /// The scenario a code review named, and the reason the generation exists.
    ///
    /// A press arrives from a connection that has already been replaced - the
    /// user picked a different button, or switched off, while a callback was
    /// still in flight. It must not key the transmitter.
    ///
    /// This used to be decided in Kotlin, in `stale()`, where no test could
    /// reach it: disabling that check entirely broke nothing anywhere in the
    /// suite. It came here so that it would. Then it was deleted along with the
    /// mode tests it had nothing to do with, and disabling `is_current` again
    /// broke nothing again - found by a reviewer who checked exactly that
    /// (review finding, 2026-09-04). The file warns about this on its own line 66.
    #[test]
    fn a_press_from_a_replaced_connection_does_not_transmit() {
        let mut state = BlePttState::new();
        state.begin(1);
        state.apply(1, Connected);

        // A different button is picked. Generation 1 is history.
        state.begin(2);

        assert_eq!(
            state.apply(1, Notification(PRESSED)),
            None,
            "a press from a replaced connection must not key the transmitter"
        );
        assert!(!state.held());
    }

    /// And the other direction: a stale drop must not release a transmission
    /// the current connection started. Letting an old callback stop the radio
    /// is its own fault, and a quieter one.
    #[test]
    fn a_drop_from_a_replaced_connection_does_not_release() {
        let mut state = BlePttState::new();
        state.begin(1);
        state.begin(2);
        state.apply(2, Connected);
        assert_eq!(state.apply(2, Notification(PRESSED)), Some(KeyDown));

        assert_eq!(
            state.apply(1, Disconnected),
            None,
            "an old connection must not end the current transmission"
        );
        assert!(state.held(), "the button is still held");
    }

    /// Starting a new attempt releases whatever was held. A press cannot
    /// survive it: the button only reports changes, so nothing would ever
    /// arrive to end it.
    #[test]
    fn beginning_a_new_attempt_releases_what_was_held() {
        let mut state = BlePttState::new();
        state.begin(1);
        state.apply(1, Connected);
        state.apply(1, Notification(PRESSED));

        assert_eq!(state.begin(2), Some(Gone));
        assert!(!state.held());
    }

    /// Two drops in a row now report twice, and that is a deliberate change.
    ///
    /// It used to ask to stop only once - the second drop found nothing held
    /// and said nothing. That was safe while this module knew whether the
    /// transmitter was on. It does not any more: in toggle mode the button is
    /// not held while the radio is on the air, so "nothing held" stopped
    /// meaning "nothing to release". Reporting twice costs nothing, because
    /// releasing an already-released source changes nothing.
    #[test]
    fn a_second_disconnect_reports_again_and_that_is_harmless() {
        let (state, actions) = run(&[
            Connected,
            Notification(PRESSED),
            Disconnected,
            Disconnected,
        ]);

        assert_eq!(actions, vec![Gone, KeyDown, Gone, Gone]);
        assert!(!state.held());
    }

    /// The mode is not this module's any more, and neither is the flag that
    /// remembered what it had decided.
    ///
    /// Four tests about the mode stood here - toggle keys on the press, toggle
    /// starts over after a reconnect, changing the mode releases, and setting
    /// the same mode twice does not. They tested a real rule that is now in
    /// `PhonePtt`, where the on-screen button and the volume keys obey the same
    /// one.
    ///
    /// **Five more went with them that had nothing to do with the mode**, and
    /// that was a mistake, not a decision: I cut from the first mode test to the
    /// end of the file. The two generation tests, the drop-policy test and the
    /// repeated-drop test are back above; a reviewer found them missing by
    /// disabling `is_current` and watching all 92 tests pass (review finding).
    #[test]
    fn the_mode_moved_out_and_the_safety_rule_did_not() {
        let (state, actions) = run(&[Connected, Notification(PRESSED), Disconnected]);
        assert!(!state.held());
        assert_eq!(actions, vec![Gone, KeyDown, Gone]);
    }

}
