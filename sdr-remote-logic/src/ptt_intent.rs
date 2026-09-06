// SPDX-License-Identifier: GPL-2.0-or-later

//! What this operator is asking of one transmitter, and every way that asking
//! ends.
//!
//! Lane 6, step 1. Nothing calls this yet - on purpose. It is built and proved
//! before anything is moved onto it, because the thing being replaced is the
//! hot path.
//!
//! # Why this exists
//!
//! "Am I transmitting" is held in four places today, each with its own
//! lifetime: the engine, the desktop UI, the Android ViewModel, and the Compose
//! button. Four copies times six exits is twenty-four places that must all be
//! right, and on 2026-09-02/03 five of them were not. Every one of those
//! defects was the same shape: an exit wired into some of the copies and not the
//! others.
//!
//! So the sources and the exits live in one place, and the exits have names.
//! A new way for a transmission to end is then one method, not a hunt.
//!
//! # What it deliberately does not know
//!
//! - **Whether the radio is transmitting.** That is a different question, it is
//!   just as true when somebody else keys the radio, and it arrives a round trip
//!   late. Painting a button from it is what produced the flash on release and
//!   the slow red on one's own press. See [`crate::ptt_button`].
//! - **Whether the request will be granted.** The server decides; this only
//!   records what was asked. A refusal is an exit, not an input.

/// Where a press comes from.
///
/// The distinction that matters is not the device but where it *lives*: a mouse
/// and a spacebar belong to a window and vanish with it, MIDI and a Bluetooth
/// button do not. Closing a pop-out while the mouse was down used to leave the
/// radio keyed, because the only code that could unkey it lived in the window
/// that had just gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PttSource {
    /// The button under the pointer, in a window.
    Mouse,
    /// The spacebar of a particular window - each pop-out has its own.
    Space,
    /// A MIDI controller. Global: it keeps working with every window shut.
    Midi,
    /// The on-screen button of a phone. Global to the app.
    Screen,
    /// A Bluetooth PTT button. Global, and it can go out of range - which is a
    /// release, not an exit (see `ble_ptt`).
    Bluetooth,
    /// The phone's volume rocker, and the page-turner keys of a BLE HID remote.
    ///
    /// Both arrive as key events to the foreground activity, so they stop
    /// reaching the app the moment the screen goes: surface-bound, despite one
    /// of them being a Bluetooth device.
    VolumeKey,
}

impl PttSource {
    /// Does this source die with the surface it was pressed on?
    ///
    /// On the desktop that surface is a window: close the pop-out and the mouse button
    /// inside it is gone. On the phone it is the screen: let that lock and the button
    /// is gone, and the transmission cannot be stopped without typing a password
    /// first. Same question, so the same answer.
    ///
    /// MIDI and Bluetooth stay outside it: those stay in your hand. A press on one of
    /// those may survive a locked screen, because you can still let go of it (owner,
    /// 2026-09-04 - he found the distinction by putting the phone in his pocket).
    fn dies_with_its_surface(self) -> bool {
        matches!(
            self,
            PttSource::Mouse | PttSource::Space | PttSource::Screen | PttSource::VolumeKey
        )
    }

    const ALL: [PttSource; 6] = [
        PttSource::Mouse,
        PttSource::Space,
        PttSource::Midi,
        PttSource::Screen,
        PttSource::Bluetooth,
        PttSource::VolumeKey,
    ];
}

/// What one operator is asking of one transmitter.
///
/// One of these per transmitter. Two radios means two, and they do not share a
/// flag - a single shared boolean is exactly how letting go of the 991A also
/// cleared an outstanding request for Thetis.
#[derive(Default, Debug, Clone)]
pub struct PttIntent {
    held: [bool; 6],
    /// The radio said no. Wait for a fresh press.
    ///
    /// Clearing the sources is enough for a click, and not for a held key: the
    /// desktop re-reads the mouse and the spacebar every frame, so a cleared
    /// latch is back the next one. With a 991A running into its time-out timer
    /// that produced a loop - key, drop at sixty seconds, key again straight
    /// away - and the operator holding the spacebar saw a transmitter that
    /// would not stay on (owner, 2026-09-04).
    ///
    /// So this outlives the sources. It goes away when everything is released,
    /// which is what "a fresh press" means.
    needs_new_press: bool,
}

impl PttIntent {
    fn idx(source: PttSource) -> usize {
        match source {
            PttSource::Mouse => 0,
            PttSource::Space => 1,
            PttSource::Midi => 2,
            PttSource::Screen => 3,
            PttSource::Bluetooth => 4,
            PttSource::VolumeKey => 5,
        }
    }

    /// Is this operator asking to transmit?
    ///
    /// The only output of this type. A caller compares it with what it last sent
    /// and sends on a change.
    ///
    /// It may still refuse on top of this - the desktop suppresses PTT on an
    /// RX-only antenna position and with no Thetis configured, and drops it when
    /// another client holds the transmitter. Those are the caller's, and an
    /// earlier version of this comment said "must not read anything else", which
    /// was simply untrue of the loop it describes (review finding).
    ///
    /// # Two kinds of control, and they are meant to behave differently
    ///
    /// A **click** is a moment: a toggle, a MIDI button, the button on a phone.
    /// Refuse it and that is the end of it - you press again when the transmitter
    /// is free.
    ///
    /// A **held key** is a continuing statement: the mouse button in momentary mode,
    /// the spacebar. For as long as it is down the operator is saying "I want to
    /// transmit", and the moment that becomes possible it should happen. [`set`]
    /// writes unconditionally and the client re-reads those sources every frame, so
    /// that is exactly what happens: refused, refused, and then - when the other one
    /// lets go - transmitting.
    ///
    /// **That is intended behaviour**, after a test by the owner on 2026-09-04:
    /// Thetis red, hold the spacebar on Yaesu 1, release Thetis, and Yaesu 1 starts.
    /// His verdict: "seems fine to me, the spacebar really is being held down."
    ///
    /// So it is not a missing edge detector but a choice, and it is written here so
    /// that nobody repairs it later as an omission. What must NOT happen, and what
    /// build 52 fixed: a click that stays behind and fires later with nothing being
    /// pressed any more.
    pub fn want_tx(&self) -> bool {
        self.any_held() && !self.needs_new_press
    }

    /// Is any source asking, whatever the wait says?
    pub fn any_held(&self) -> bool {
        self.held.iter().any(|&h| h)
    }

    /// Is a fresh press needed before this can transmit again?
    ///
    /// The raw answer. It was masked by "is anything held" at first, on the idea
    /// that releasing everything answers no - but `radio_left_tx` clears the
    /// sources itself, so the mask threw the flag away at the very moment it was
    /// set. Its own test caught that. Releasing is handled by [`Self::settle`]
    /// instead, which is a thing that happens rather than a thing that is
    /// computed.
    pub fn needs_new_press(&self) -> bool {
        self.needs_new_press
    }

    /// A release was observed: end the wait if the control it was waiting on is
    /// up.
    ///
    /// `resampled` is the same list [`Self::radio_left_tx`] was given - the
    /// controls that report themselves every frame. The wait exists because one
    /// of those was down; it ends when none of them is. Anything else about the
    /// intent is irrelevant to it, which is the point: a rebuild that passes
    /// through "nothing held" must not end it.
    pub fn settle(&mut self, resampled: &[PttSource]) {
        if !resampled.iter().any(|&s| self.is_held(s)) {
            self.needs_new_press = false;
        }
    }

    /// Carry that answer back in, for a caller that rebuilds this every frame.
    pub fn set_needs_new_press(&mut self, v: bool) {
        self.needs_new_press = v;
    }

    /// One source, pressed or let go. A plain assignment, and deliberately so.
    ///
    /// It used to end a wait for a fresh press whenever nothing was left held.
    /// That reads as an event and is not one: callers rebuild an intent from
    /// stored state every frame, and a rebuild that happens to pass through
    /// "nothing held" would end a wait nobody had released. Which of the two
    /// won depended on the order the windows are drawn in - the third time
    /// tonight that this rule died of that. Ending the wait is [`Self::settle`],
    /// called where a real release is observed.
    pub fn set(&mut self, source: PttSource, held: bool) {
        self.held[Self::idx(source)] = held;
    }

    pub fn press(&mut self, source: PttSource) {
        self.set(source, true);
    }

    pub fn release(&mut self, source: PttSource) {
        self.set(source, false);
    }

    /// Is this source being held?
    pub fn is_held(&self, source: PttSource) -> bool {
        self.held[Self::idx(source)]
    }

    // ------------------------------------------------------------------ exits
    //
    // Every one of these ends the asking. They are separate methods rather than
    // one `clear()` because the name is the documentation: a reader looking for
    // "what happens when the link dies" finds `disconnected`, and a reviewer
    // asking "are all the exits wired" has a list to check against.

    /// The server said no.
    ///
    /// A refusal is the answer to this request, so the request is over. It does
    /// not mean "try again in a moment" - the operator has to press again, which
    /// is also what makes a late refusal harmless.
    pub fn refused(&mut self) {
        self.clear_all();
    }

    /// Somebody else holds this transmitter.
    ///
    /// Dropped rather than remembered, so a press made while it was busy does
    /// not fire the moment the other operator releases.
    pub fn busy(&mut self) {
        self.clear_all();
    }

    /// There is no longer a server to transmit to.
    ///
    /// This exit did not exist until 2026-09-03. A dying link left the button
    /// red on a client talking to nobody, and on the phone it also left the
    /// ViewModel believing a request was outstanding - so the next press
    /// returned early and did nothing.
    pub fn disconnected(&mut self) {
        self.clear_all();
    }

    /// The radio reports it is no longer transmitting.
    ///
    /// Not the same as us letting go: the radio can stop on its own - a time-out
    /// timer, a front-panel press, a fault. Whatever we still believe, we are
    /// not transmitting.
    pub fn radio_left_tx(&mut self, resampled: &[PttSource]) {
        self.stop_and_wait_for_release(resampled);
    }

    /// The operator pressed a latching control while something was already
    /// asking. That means stop - the same rule the phone obeys, where any
    /// latching control ends what any other started.
    pub fn operator_stop(&mut self, resampled: &[PttSource]) {
        self.stop_and_wait_for_release(resampled);
    }

    /// Stop, and do not let a continuously reported source put it straight back.
    ///
    /// `resampled` is the list the CALLER re-reads every frame. It is a
    /// parameter and not a property of the source, because the same control can
    /// be either: the desktop mouse is read out of the input state in momentary
    /// mode and is a latch flipped by a click in toggle mode. A wait on a latch
    /// would never end, because nothing ever reports it released. The type
    /// cannot see that setting; the caller can, and now has to say.
    fn stop_and_wait_for_release(&mut self, resampled: &[PttSource]) {
        // A held key is NOT cleared here, and that is the whole repair.
        //
        // Clearing it looked right and was not: the key is genuinely down, the
        // desktop reads it again next frame, and the wait - which ends when
        // nothing is held - had already ended inside the same frame. Whether it
        // survived depended on the order in which the windows are drawn, which
        // is not a foundation. The transmitter was let go and keyed again two
        // tenths of a second later (owner, 2026-09-04, twice).
        //
        // So the sources that report edges are cleared, because a click is a
        // moment and its moment is over. The ones that report themselves again
        // stay held - they are held - and the wait keeps them off the air until
        // they are really released.
        let will_return = resampled.iter().any(|&s| self.is_held(s));
        for s in PttSource::ALL {
            if !resampled.contains(&s) {
                self.set(s, false);
            }
        }
        self.needs_new_press = will_return;
    }

    /// The window a press lived in is gone.
    ///
    /// Only the sources that belonged to it. MIDI and Bluetooth are global and
    /// keep working with every window shut - clearing those here would be a
    /// second bug wearing the first one's clothes.
    pub fn window_gone(&mut self) {
        for s in PttSource::ALL {
            if s.dies_with_its_surface() {
                self.release(s);
            }
        }
        // A wait belongs to a control that is still down. The window that held
        // it is gone, so the control is gone with it - and a wait nobody can
        // release is a button that stays dead. See `clear_all`.
        self.needs_new_press = false;
    }

    /// Every source let go, and the asking is over.
    ///
    /// Including the wait for a fresh press. An exit that means "this request
    /// has ended" must not leave a wait behind: nothing is held any more, so
    /// nothing will ever come along to release, and the next press would find a
    /// wait it cannot end. A reviewer walked all five exits and every one of
    /// them left the next press dead (review finding, 2026-09-05).
    ///
    /// The two exits that DO mean "wait" set the flag after calling this - see
    /// `stop_and_wait_for_release`.
    fn clear_all(&mut self) {
        self.needs_new_press = false;
        self.held = [false; 6];
    }
}

// ---------------------------------------------------------------------------
// Lane 6, step 2a: the decision of the desktop loop, exactly as it stands.
//
// This is NOT an improvement and it does not use PttIntent yet. It is the body
// of drive_yaesu_ptt(), word for word, moved somewhere a test can call it -
// because without that, "the behaviour is identical" at step 2b is a claim and
// not a check. Today no client test constructs an SdrRemoteApp, so that loop is
// unreachable from outside (review finding).
//
// The order is the order of the original code, including the fact that the two
// clearing steps come before the sum. That is not incidental: a press made while
// the transmitter was busy must not fire once the other one lets go.

/// The three latches of one transmitter in the desktop client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Latches {
    pub mouse: bool,
    pub space: bool,
    pub midi: bool,
    /// The radio dropped out of TX and nothing has been released since.
    ///
    /// Rides along with the latches because the caller already stores and
    /// threads those; a second field next to them would be a second thing to
    /// forget. See [`PttIntent::needs_new_press`].
    pub needs_new_press: bool,
}

impl Latches {
    /// Everything released, and the wait kept.
    ///
    /// For a caller that has to drop the presses of a slot - it lost the
    /// arbitration, or it was blocked by our own other radio. Use this rather
    /// than `Default::default()`: that resets the whole struct, and it silently
    /// took the wait for a fresh press with it. The radio stopping and this slot
    /// losing are two different things, and only one of them ends the wait.
    ///
    /// Cost of getting it wrong, measured: the desktop let go of the 991A and
    /// keyed it again two tenths of a second later, into the same time-out timer
    /// (owner, 2026-09-04).
    /// A latching control was pressed: it switches the TRANSMISSION, not itself.
    ///
    /// Whatever started it, this ends it - the rule the phone has obeyed since
    /// 2026-09-04, brought over so the two front ends behave the same. On the
    /// desktop a click used to flip its own latch and nothing else, so clicking
    /// while the spacebar was keying looked like it did nothing and then kept
    /// the transmitter on after the spacebar came up (owner, 2026-09-04).
    pub fn latching_press(self, source: PttSource, resampled: &[PttSource]) -> Self {
        let mut i = PttIntent::default();
        i.set(PttSource::Mouse, self.mouse);
        i.set(PttSource::Space, self.space);
        i.set(PttSource::Midi, self.midi);
        i.set_needs_new_press(self.needs_new_press);
        if i.any_held() {
            i.operator_stop(resampled);
        } else {
            i.set(source, true);
        }
        Self {
            mouse: i.is_held(PttSource::Mouse),
            space: i.is_held(PttSource::Space),
            midi: i.is_held(PttSource::Midi),
            needs_new_press: i.needs_new_press(),
        }
    }

    /// A hold-to-talk control, read out of the input state every frame.
    ///
    /// On the rising edge it TAKES OVER: every latching control is released, so
    /// grabbing the spacebar while the screen button or MIDI is latched on
    /// leaves the spacebar holding the transmitter and nothing behind it. Let go
    /// and it stops. That is what "overrule" means for a control you hold - the
    /// alternative, treating a press as a stop, would mean not transmitting
    /// while the key is down, which is the one thing hold-to-talk may not do
    /// (owner, 2026-09-04).
    ///
    /// Only latching controls are released, never another hold control. Two
    /// controls that are both re-read every frame would each clear the other on
    /// its own rising edge, every frame, for as long as both were down.
    pub fn hold(self, source: PttSource, down: bool, resampled: &[PttSource]) -> Self {
        let mut i = PttIntent::default();
        i.set(PttSource::Mouse, self.mouse);
        i.set(PttSource::Space, self.space);
        i.set(PttSource::Midi, self.midi);
        i.set_needs_new_press(self.needs_new_press);

        if down && !i.is_held(source) {
            for s in PttSource::ALL {
                if !resampled.contains(&s) {
                    i.set(s, false);
                }
            }
        }
        i.set(source, down);
        // The only place a release is observed rather than reconstructed.
        i.settle(resampled);

        Self {
            mouse: i.is_held(PttSource::Mouse),
            space: i.is_held(PttSource::Space),
            midi: i.is_held(PttSource::Midi),
            needs_new_press: i.needs_new_press(),
        }
    }

    pub fn released(self) -> Self {
        Self { mouse: false, space: false, midi: false, needs_new_press: self.needs_new_press }
    }
}

/// What the loop has to do after a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    /// The latches as they should stand in the client after this frame.
    pub latches: Latches,
    /// What the operator is asking for, after the clearing steps.
    pub want_tx: bool,
    /// `Some(v)` when a command has to go out, `None` when nothing changed. The loop
    /// only sends a change.
    pub send: Option<bool>,
}

/// One frame of the desktop loop, as a function.
///
/// `window_open` is whether this transmitter's pop-out is open; `held_by_other`
/// is what the server's ownership table says; `last_sent` is what last went to
/// the server.
pub fn desktop_frame(
    latches: Latches,
    window_open: bool,
    held_by_other: bool,
    last_sent: bool,
) -> Decision {
    // Step 2b: the same decision, now taken by PttIntent.
    //
    // The clearing steps were written out here by hand one more time - that was step
    // 2a, the original literally moved. The type does it now, and every way a
    // request can end lives in ONE place.
    //
    // That this changes nothing about the behaviour is not a promise but a test: the
    // table of sixty-four rows below compares this function with the rules as they
    // stood in ui/update.rs. While that stays green, the replacement is
    // behaviour-neutral.
    // Sources first, then the flag, then settle. The other order loses the flag:
    // an intermediate state in which nothing is held yet clears it.
    let mut i = PttIntent::default();
    i.set(PttSource::Mouse, latches.mouse);
    i.set(PttSource::Space, latches.space);
    i.set(PttSource::Midi, latches.midi);
    i.set_needs_new_press(latches.needs_new_press);

    if !window_open {
        i.window_gone();
    }
    if held_by_other {
        // busy() clears everything, so the `&& !held_by_other` of the original is no
        // longer a separate condition here but a consequence. That is the intent: a
        // request that cannot be granted should not exist, rather than exist and be
        // ignored.
        i.busy();
    }

    let l = Latches {
        mouse: i.is_held(PttSource::Mouse),
        space: i.is_held(PttSource::Space),
        midi: i.is_held(PttSource::Midi),
        needs_new_press: i.needs_new_press(),
    };
    let want_tx = i.want_tx();
    let send = if want_tx == last_sent { None } else { Some(want_tx) };

    Decision { latches: l, want_tx, send }
}

/// Which transmitters may this operator modulate at once?
///
/// There is one microphone. With multi-TX on it goes to every keyed transmitter;
/// with multi-TX off to exactly one, and the rest are refused.
///
/// **Who wins: whoever is already transmitting.** Not the newcomer. That is a
/// choice and it is the owner's choice: a transmission in progress is not
/// interrupted because a button is pressed somewhere else. The second one is
/// shown a refusal, and that refusal has a reason of its own - not "another
/// station is transmitting" but "this client allows only one". Two different
/// things, which should also look different on the screen.
///
/// Without this rule two Yaesus keyed at once while only one got modulation: two
/// red buttons and a bare carrier, and nothing that said so (owner, 2026-09-03).
///
/// Slot order: 0 = Thetis, 1 = Yaesu slot 0, 2 = Yaesu slot 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxArbitration {
    /// Wie er mag zenden.
    pub granted: [bool; 3],
    /// Who asked and does not get it - because this client allows only one.
    pub blocked: [bool; 3],
}

pub fn arbitrate(
    want: [bool; 3],
    last_sent: [bool; 3],
    held_by_other: [bool; 3],
    multi_tx: bool,
) -> TxArbitration {
    // A transmitter that ANOTHER station is holding cannot transmit anyway.
    //
    // So it must not win either: if it did, it would hold up our own other radio for
    // a transmission that never starts. And it does not get "blocked by yourself"
    // here - its button says "busy", which is the more specific and the more correct
    // answer (owner, 2026-09-03).
    let mut want = want;
    for i in 0..3 {
        if held_by_other[i] {
            want[i] = false;
        }
    }

    if multi_tx {
        return TxArbitration { granted: want, blocked: [false; 3] };
    }

    // Who is already transmitting and wants to keep doing so? They keep it.
    let zittend = (0..3).find(|&i| want[i] && last_sent[i]);
    let winnaar = match zittend {
        Some(i) => Some(i),
        // Nobody was there already: the lowest one asking. Arbitrary, but predictable,
        // and it has to fall somewhere.
        None => (0..3).find(|&i| want[i]),
    };

    let mut granted = [false; 3];
    let mut blocked = [false; 3];
    if let Some(w) = winnaar {
        granted[w] = true;
        for i in 0..3 {
            if i != w && want[i] {
                blocked[i] = true;
            }
        }
    }
    TxArbitration { granted, blocked }
}

// ------------------------------------------------------------------- the phone

/// Which transmitter the operator is working.
///
/// The server knows the same three under other names (`TxTarget` in `session.rs`)
/// and the desktop knows them as slot 0, 1 and 2. Three names for the same three
/// transmitters is exactly Theme H, but the server cannot reach `logic` and
/// merging them is a piece of work of its own - this only records that it is
/// known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PttTarget {
    Thetis,
    /// De Yaesu in sleuf 0.
    Yaesu0,
    /// De Yaesu in sleuf 1.
    Yaesu1,
}

impl PttTarget {
    pub const ALL: [PttTarget; 3] = [PttTarget::Thetis, PttTarget::Yaesu0, PttTarget::Yaesu1];

    fn idx(self) -> usize {
        match self {
            PttTarget::Thetis => 0,
            PttTarget::Yaesu0 => 1,
            PttTarget::Yaesu1 => 2,
        }
    }
}

/// The PTT of the phone: three transmitters, but never more than one at a time.
///
/// **Why a type of its own next to `desktop_frame`.** The desktop draws frames and
/// re-reads all its sources every frame; the phone gets events - a finger on the
/// button, a byte from the Bluetooth button, an answer from the server. Same
/// rules, a different shape around them. That is the same division of labour as
/// `BlePttGate`: the rule here, the lock in the bridge, and Kotlin does what this
/// says.
///
/// **One transmitter at a time, and here that is a rule and not an order of
/// steps.** The phone only ever keys one - the owner decided that explicitly. It
/// used to be a sequence of steps in the ViewModel (release the previous one
/// first, then set the new one); a sequence can end up in the wrong order, a rule
/// cannot.
///
/// **What is deliberately NOT here: `window_gone`.** On the desktop a press ends
/// when the window around it closes. The phone has that situation too - app to
/// the background, screen off - and there the transmission is meant to continue:
/// you put it in your pocket and go on transmitting with the Bluetooth button.
/// Decided by the owner on 2026-09-04. It is written here so that nobody comes
/// along later and repairs it as a forgotten exit.
#[derive(Default, Debug, Clone)]
pub struct PhonePtt {
    intents: [PttIntent; 3],
    toggle: bool,
}

impl PhonePtt {
    pub fn new() -> Self {
        Self::default()
    }

    /// Hold to transmit, or one press on and the next off.
    ///
    /// The setting used to be read separately by the on-screen button, the
    /// volume keys and the Bluetooth gate, each keeping its own idea of whether
    /// it was currently latched on. Compose re-applies this on every redraw, so
    /// setting it to what it already is must change nothing.
    pub fn set_toggle_mode(&mut self, toggle: bool) {
        if self.toggle == toggle {
            return;
        }
        self.toggle = toggle;
        // The operator changed what a press means, so nothing may be carried
        // across the change: a latched transmission would have no press left
        // that could end it. This rule used to live in the Bluetooth gate,
        // where it only covered that one button.
        for t in PttTarget::ALL {
            self.intents[t.idx()].clear_all();
        }
    }

    /// Does a press on this control latch, or does it have to be held?
    ///
    /// MIDI always latches, whatever the setting says: a controller button is a
    /// momentary contact and the operator cannot hold it while doing anything
    /// else. That was already true on this phone and on the desktop, and it is
    /// written here rather than in both front ends.
    fn latches(&self, source: PttSource) -> bool {
        source == PttSource::Midi || self.toggle
    }

    /// A control went down.
    ///
    /// This and [`Self::up`] replace three latches that lived in the UI - the
    /// Compose button's `pressed` and `toggled`, and the volume keys' own
    /// `btToggled`. Those were a third copy of "am I transmitting", and the
    /// exits could not reach them: the transmitter stopped and the button stayed
    /// red (owner, build 58). Deciding here means an exit is felt everywhere at
    /// once, because there is only one place left to feel it.
    pub fn down(&mut self, target: PttTarget, source: PttSource) {
        // A latching control switches the TRANSMISSION, not its own press.
        //
        // So whatever started it can be stopped by any of them: the on-screen
        // button, the MIDI button, the Bluetooth button. The operator asked for
        // that symmetry and it is the simpler rule - it also means there is
        // never a control left holding a press that no longer transmits.
        //
        // I had built the narrow version first, where a latching control only
        // answered for itself, on the grounds that pressing PTT is a request to
        // transmit and never a request to stop. That was my reasoning and not a
        // requirement, and it made the three buttons behave differently from
        // each other for no reason the operator could see (2026-09-04).
        if self.latches(source) && self.want_tx(target) {
            self.operator_stop(target);
            return;
        }
        self.set(target, source, true);
    }

    /// A control came up.
    pub fn up(&mut self, target: PttTarget, source: PttSource) {
        if self.latches(source) {
            // The press already said everything; the release says nothing.
            return;
        }
        if source == PttSource::Screen {
            // The on-screen button is the way out, so releasing it ends
            // everything on this transmitter - see `operator_stop`.
            self.operator_stop(target);
            return;
        }
        self.set(target, source, false);
    }

    /// One source, pressed or let go.
    ///
    /// Pressing releases the other two transmitters. Not because the previous one
    /// "has to be cleaned up first", but because the phone works one at a time: a
    /// request to Yaesu 0 and a request to Thetis cannot both exist.
    pub fn set(&mut self, target: PttTarget, source: PttSource, held: bool) {
        if held {
            for other in PttTarget::ALL {
                if other != target {
                    self.intents[other.idx()].clear_all();
                }
            }
        }
        self.intents[target.idx()].set(source, held);
    }

    pub fn want_tx(&self, target: PttTarget) -> bool {
        self.intents[target.idx()].want_tx()
    }

    /// Which transmitter is the operator asking for? At most one, by construction.
    pub fn asking(&self) -> Option<PttTarget> {
        PttTarget::ALL.into_iter().find(|&t| self.want_tx(t))
    }

    // ------------------------------------------------------------------ exits
    //
    // The same names as on the desktop, so that "are all the exits wired?" stays a
    // question with a list rather than a search.

    /// The server said no. Applies to whichever request was outstanding.
    pub fn refused(&mut self) {
        for t in PttTarget::ALL {
            self.intents[t.idx()].refused();
        }
    }

    /// Somebody else holds this transmitter.
    ///
    /// **No caller on the phone, and that is not an oversight.** On the desktop
    /// this is reached every frame from the ownership table. The phone keys
    /// optimistically and learns the answer from the server's refusal, which
    /// arrives as [`Self::refused`] - so the case this exit exists for is
    /// already covered there. It stays because the type is shared and the
    /// desktop does use it (via `desktop_frame`).
    pub fn busy(&mut self, target: PttTarget) {
        self.intents[target.idx()].busy();
    }

    /// There is no server left to transmit to.
    pub fn disconnected(&mut self) {
        for t in PttTarget::ALL {
            self.intents[t.idx()].disconnected();
        }
    }

    /// The radio reports it is no longer transmitting - a time-out, the front
    /// panel, a fault. Whatever we believed, we are not transmitting.
    ///
    /// **Reachable from the phone, not yet wired, and I should not have said
    /// otherwise.** The commit that introduced this wrote that the phone "now
    /// has the exit the desktop had"; it has the method and no caller (review
    /// finding). The signal does exist - `yaesu_tx_active` crosses the
    /// bridge - but that flag is true for our own transmission as well, so
    /// wiring it means detecting a falling edge on the path that keys radios.
    /// That is its own piece of work with its own hardware test, not a line
    /// added at the end of a day.
    pub fn radio_left_tx(&mut self, target: PttTarget) {
        self.intents[target.idx()].radio_left_tx(&[]);
    }

    /// The operator switches the on-screen button off.
    ///
    /// **This is more than releasing the screen source: it ends everything that is on
    /// for that transmitter**, including a Bluetooth button that is still being held.
    /// That is deliberate and it is a safety property: there has to be a way out when
    /// an external button jams or misbehaves, and that way out is the button on the
    /// screen.
    ///
    /// The owner first saw the behaviour as a mistake of mine and then judged that it
    /// is in fact preferable (2026-09-04): "if the button misbehaves for a moment,
    /// you can always use the screen PTT."
    ///
    /// That this works depends on the Bluetooth button reporting CHANGES only. After
    /// this stop the transmitter stays off until that button is released and pressed
    /// again - it never says "I am still being held" of its own accord.
    pub fn operator_stop(&mut self, target: PttTarget) {
        self.intents[target.idx()].clear_all();
    }

    /// The Bluetooth button is no longer holding anything.
    ///
    /// Every link event reports this - a drop, a connect, a new attempt - and it
    /// is unconditional on purpose. In toggle mode the button is not held while
    /// the radio is on the air, so nothing outside this type can tell whether
    /// there is a latch to release. Releasing nothing costs nothing; not
    /// releasing costs four seconds of unattended carrier, which is the
    /// supervision timeout of the button the owner uses.
    pub fn bluetooth_gone(&mut self) {
        for t in PttTarget::ALL {
            self.intents[t.idx()].release(PttSource::Bluetooth);
        }
    }

    /// The screen is gone - locked, or the app put in the background.
    ///
    /// Only the sources that die with it. The Bluetooth button stays: it is in your
    /// hand and you can let it go. The on-screen button is not, and a transmitter
    /// left on behind a locked screen can only be switched off after typing a
    /// password.
    ///
    /// This is the same exit as `window_gone` on the desktop, with the same
    /// distinction - see [`PttSource::dies_with_its_surface`].
    pub fn screen_gone(&mut self) {
        for t in PttTarget::ALL {
            self.intents[t.idx()].window_gone();
        }
    }
}

#[cfg(test)]
mod phone_tests {
    use super::*;

    #[test]
    fn een_zender_tegelijk() {
        let mut p = PhonePtt::new();
        p.set(PttTarget::Thetis, PttSource::Screen, true);
        assert_eq!(p.asking(), Some(PttTarget::Thetis));

        // Press another transmitter without releasing the first.
        p.set(PttTarget::Yaesu0, PttSource::Screen, true);
        assert_eq!(p.asking(), Some(PttTarget::Yaesu0));
        assert!(!p.want_tx(PttTarget::Thetis), "the first one should have been released");
    }

    #[test]
    fn twee_bronnen_op_dezelfde_zender_zijn_onafhankelijk() {
        // Releasing the on-screen button while the Bluetooth button is still held must
        // not end the transmission. That is the reason a source has a place of its own
        // rather than a shared boolean.
        let mut p = PhonePtt::new();
        p.set(PttTarget::Yaesu0, PttSource::Bluetooth, true);
        p.set(PttTarget::Yaesu0, PttSource::Screen, true);
        p.set(PttTarget::Yaesu0, PttSource::Screen, false);
        assert!(p.want_tx(PttTarget::Yaesu0), "the Bluetooth button is still down");
    }

    /// The table the architecture review asks for: from every state, every exit must
    /// end at idle. That is exactly the check two earlier commits failed.
    #[test]
    fn elke_uitgang_eindigt_bij_stil() {
        let bronnen = [PttSource::Screen, PttSource::Bluetooth];
        let uitgangen: [(&str, fn(&mut PhonePtt, PttTarget)); 3] = [
            ("refused", |p, _| p.refused()),
            ("busy", |p, t| p.busy(t)),
            ("radio_left_tx", |p, t| p.radio_left_tx(t)),
        ];

        for doel in PttTarget::ALL {
            // Every subset of the sources, including both at once.
            for bits in 0u8..4 {
                for (naam, uitgang) in uitgangen {
                    let mut p = PhonePtt::new();
                    for (i, b) in bronnen.iter().enumerate() {
                        if bits & (1 << i) != 0 {
                            p.set(doel, *b, true);
                        }
                    }
                    uitgang(&mut p, doel);
                    assert!(
                        !p.want_tx(doel),
                        "exit {naam} left a request standing on {doel:?}, sources {bits:02b}"
                    );
                    assert_eq!(p.asking(), None, "exit {naam} left something standing somewhere");
                }
            }
        }
    }

    #[test]
    fn verbinding_weg_laat_alles_los() {
        let mut p = PhonePtt::new();
        p.set(PttTarget::Yaesu1, PttSource::Bluetooth, true);
        p.disconnected();
        assert_eq!(p.asking(), None);
    }

    /// Screen gone: the Bluetooth button stays, the on-screen button does not.
    ///
    /// This is the distinction the owner found by putting the phone in his pocket.
    /// We had first decided that a transmission simply continues when the screen goes
    /// away; that holds for the Bluetooth button, which you can still release. For
    /// the on-screen button it does not: the phone locks and you have to type a
    /// password before you can switch your transmitter off.
    #[test]
    fn scherm_weg_laat_bluetooth_staan_en_het_scherm_niet() {
        let mut p = PhonePtt::new();
        p.set(PttTarget::Yaesu0, PttSource::Bluetooth, true);
        p.screen_gone();
        assert!(p.want_tx(PttTarget::Yaesu0), "the Bluetooth button is in your hand");

        let mut q = PhonePtt::new();
        q.set(PttTarget::Yaesu0, PttSource::Screen, true);
        q.screen_gone();
        assert!(!q.want_tx(PttTarget::Yaesu0), "the on-screen button is gone, so there is no way out left");
    }

    /// Switching the on-screen button off ends everything - including a Bluetooth
    /// button that is still being held. That is the way out when that button jams.
    #[test]
    fn schermknop_uit_eindigt_ook_de_bluetooth_druk() {
        let mut p = PhonePtt::new();
        p.set(PttTarget::Thetis, PttSource::Bluetooth, true);
        p.set(PttTarget::Thetis, PttSource::Screen, true);
        p.operator_stop(PttTarget::Thetis);
        assert!(!p.want_tx(PttTarget::Thetis));
        assert_eq!(p.asking(), None);
    }

    /// And the other way round, because otherwise "ends everything" would be the
    /// ordinary path too: releasing the Bluetooth button while the screen is still
    /// held does NOT end the transmission.
    #[test]
    fn bluetooth_loslaten_eindigt_een_schermdruk_niet() {
        let mut p = PhonePtt::new();
        p.set(PttTarget::Thetis, PttSource::Screen, true);
        p.set(PttTarget::Thetis, PttSource::Bluetooth, true);
        p.set(PttTarget::Thetis, PttSource::Bluetooth, false);
        assert!(p.want_tx(PttTarget::Thetis), "the screen is still being held");
    }

    /// Hold to transmit: down keys it, up stops it.
    #[test]
    fn momentary_screen_press() {
        let mut p = PhonePtt::new();
        p.down(PttTarget::Thetis, PttSource::Screen);
        assert!(p.want_tx(PttTarget::Thetis));
        p.up(PttTarget::Thetis, PttSource::Screen);
        assert!(!p.want_tx(PttTarget::Thetis));
    }

    /// One tap on, the next off - and the release in between says nothing.
    ///
    /// The second tap is what the phone got wrong: the UI held its own `toggled`
    /// flag, so after an exit had already stopped the transmitter the flag was
    /// still on and the next tap only turned it off again. The owner had to tap
    /// a third time (build 58).
    #[test]
    fn toggle_takes_two_taps_and_no_more() {
        let mut p = PhonePtt::new();
        p.set_toggle_mode(true);

        p.down(PttTarget::Thetis, PttSource::Screen);
        p.up(PttTarget::Thetis, PttSource::Screen);
        assert!(p.want_tx(PttTarget::Thetis), "one tap latches it on");

        p.down(PttTarget::Thetis, PttSource::Screen);
        p.up(PttTarget::Thetis, PttSource::Screen);
        assert!(!p.want_tx(PttTarget::Thetis), "the next tap turns it off");

        p.down(PttTarget::Thetis, PttSource::Screen);
        assert!(p.want_tx(PttTarget::Thetis), "and the one after starts again");
    }

    /// MIDI latches whatever the setting says, because a controller button
    /// cannot be held while doing anything else.
    #[test]
    fn midi_latches_even_in_momentary_mode() {
        let mut p = PhonePtt::new();
        p.set_toggle_mode(false);
        p.down(PttTarget::Thetis, PttSource::Midi);
        p.up(PttTarget::Thetis, PttSource::Midi);
        assert!(p.want_tx(PttTarget::Thetis), "the release of a MIDI button says nothing");
        p.down(PttTarget::Thetis, PttSource::Midi);
        assert!(!p.want_tx(PttTarget::Thetis));
    }

    /// After an exit has stopped it, one tap starts it again - not two.
    ///
    /// This is the defect the owner found, stated as a property: no control may
    /// remember a state the transmitter no longer has.
    #[test]
    fn an_exit_leaves_no_control_out_of_step() {
        for toggle in [false, true] {
            for source in [PttSource::Screen, PttSource::Midi, PttSource::VolumeKey] {
                let mut p = PhonePtt::new();
                p.set_toggle_mode(toggle);
                p.down(PttTarget::Yaesu0, source);
                assert!(p.want_tx(PttTarget::Yaesu0), "{source:?} toggle={toggle}");

                p.disconnected();
                assert!(!p.want_tx(PttTarget::Yaesu0));

                // One press, and it transmits again.
                p.down(PttTarget::Yaesu0, source);
                assert!(
                    p.want_tx(PttTarget::Yaesu0),
                    "{source:?} toggle={toggle}: needed a second press after an exit"
                );
            }
        }
    }

    /// A press stopped from the screen leaves nothing latched anywhere.
    ///
    /// This is the defect the owner found in build 59, as a property. The
    /// Bluetooth gate kept its own idea of being on; stopping from the screen
    /// could not reach it, so the next press turned that idea off and did
    /// nothing, and only the press after keyed the radio. Now there is one
    /// latch, so there is nothing left to go out of step.
    #[test]
    fn stopping_from_the_screen_leaves_the_bluetooth_button_ready() {
        let mut p = PhonePtt::new();
        p.set_toggle_mode(true);

        p.down(PttTarget::Thetis, PttSource::Screen);
        assert!(p.want_tx(PttTarget::Thetis));

        // Any latching control stops it, not only the one that started it.
        p.down(PttTarget::Thetis, PttSource::Bluetooth);
        assert!(!p.want_tx(PttTarget::Thetis), "the Bluetooth button did not stop it");

        // And ONE press starts it again - nothing is left out of step.
        p.down(PttTarget::Thetis, PttSource::Bluetooth);
        assert!(p.want_tx(PttTarget::Thetis), "the button had to be pressed twice");
    }

    /// Every latching control can stop what any other started, in both
    /// directions. The operator's rule, as a table.
    #[test]
    fn any_latching_control_stops_what_another_started() {
        let latching = [PttSource::Screen, PttSource::Midi, PttSource::Bluetooth];
        for start in latching {
            for stop in latching {
                let mut p = PhonePtt::new();
                p.set_toggle_mode(true);
                p.down(PttTarget::Yaesu0, start);
                assert!(p.want_tx(PttTarget::Yaesu0), "{start:?} did not start it");
                p.down(PttTarget::Yaesu0, stop);
                assert!(
                    !p.want_tx(PttTarget::Yaesu0),
                    "{start:?} started it and {stop:?} could not stop it"
                );
            }
        }
    }

    /// A link lost while toggled on still stops the transmitter.
    ///
    /// The safety rule of the Bluetooth gate, on the side where the latch now
    /// lives. Nothing is being held, so nothing physical says stop.
    #[test]
    fn a_lost_bluetooth_link_stops_a_latched_transmission() {
        let mut p = PhonePtt::new();
        p.set_toggle_mode(true);
        p.down(PttTarget::Yaesu1, PttSource::Bluetooth);
        p.up(PttTarget::Yaesu1, PttSource::Bluetooth);
        assert!(p.want_tx(PttTarget::Yaesu1), "toggled on, button not held");

        p.bluetooth_gone();
        assert!(!p.want_tx(PttTarget::Yaesu1));
    }

    /// Changing what a press means may not carry a transmission across.
    #[test]
    fn changing_the_mode_releases() {
        let mut p = PhonePtt::new();
        p.down(PttTarget::Thetis, PttSource::Screen);
        p.set_toggle_mode(true);
        assert!(!p.want_tx(PttTarget::Thetis));
        // And setting the mode it is already in changes nothing - Compose
        // re-applies this on every redraw.
        p.down(PttTarget::Thetis, PttSource::Screen);
        p.set_toggle_mode(true);
        assert!(p.want_tx(PttTarget::Thetis), "a redraw ended a transmission");
    }

    /// The volume keys and the page-turner die with the screen; the GATT button
    /// does not. Both are Bluetooth devices - what decides is how the press
    /// reaches the app.
    #[test]
    fn volume_keys_are_surface_bound_and_the_gatt_button_is_not() {
        let mut p = PhonePtt::new();
        p.down(PttTarget::Thetis, PttSource::VolumeKey);
        p.screen_gone();
        assert!(!p.want_tx(PttTarget::Thetis), "key events stop at the foreground");

        let mut q = PhonePtt::new();
        q.down(PttTarget::Thetis, PttSource::Bluetooth);
        q.screen_gone();
        assert!(q.want_tx(PttTarget::Thetis), "the GATT button keeps reporting");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every exit that means "this is over", by name.
    fn total_exits() -> Vec<(&'static str, fn(&mut PttIntent))> {
        vec![
            ("refused", PttIntent::refused as fn(&mut PttIntent)),
            ("busy", PttIntent::busy),
            ("disconnected", PttIntent::disconnected),
            ("radio_left_tx", |i: &mut PttIntent| i.radio_left_tx(&[])),
        ]
    }

    #[test]
    fn nothing_pressed_is_not_transmitting() {
        assert!(!PttIntent::default().want_tx());
    }

    #[test]
    fn any_source_is_enough() {
        for s in PttSource::ALL {
            let mut i = PttIntent::default();
            i.press(s);
            assert!(i.want_tx(), "{s:?} should key it");
        }
    }

    /// Two hands on the same transmitter: letting go of one is not letting go.
    #[test]
    fn releasing_one_source_while_another_is_held_keeps_transmitting() {
        let mut i = PttIntent::default();
        i.press(PttSource::Mouse);
        i.press(PttSource::Midi);
        i.release(PttSource::Mouse);
        assert!(i.want_tx(), "MIDI is still held");
        i.release(PttSource::Midi);
        assert!(!i.want_tx());
    }

    /// **The table.** From every state, every total exit must end at rest.
    ///
    /// This is the check the old code could not answer, and it is the whole
    /// reason this type exists: five sources times four exits is twenty
    /// combinations, and the defects of 2026-09-02/03 were exits wired into some
    /// copies and not others. Here there is one copy and the test is exhaustive.
    #[test]
    fn every_exit_ends_every_state() {
        for (naam, exit) in total_exits() {
            // Each source alone.
            for s in PttSource::ALL {
                let mut i = PttIntent::default();
                i.press(s);
                exit(&mut i);
                assert!(!i.want_tx(), "{naam} left {s:?} held");
            }
            // And everything at once, which is the state a real fault reaches.
            let mut i = PttIntent::default();
            for s in PttSource::ALL {
                i.press(s);
            }
            exit(&mut i);
            assert!(!i.want_tx(), "{naam} left something held");
        }
    }

    /// An exit that arrives when nothing is happening changes nothing.
    #[test]
    fn an_exit_on_an_idle_intent_is_harmless() {
        for (naam, exit) in total_exits() {
            let mut i = PttIntent::default();
            exit(&mut i);
            assert!(!i.want_tx(), "{naam} on an idle intent");
        }
    }

    /// The window exit is deliberately partial - and that is the point.
    ///
    /// `Screen` moved sides on 2026-09-04. It used to be listed here as global,
    /// on the reasoning that a phone has no windows. It does have a surface, and
    /// that surface locks: a press left standing there keeps a transmitter on
    /// the air behind a password. The question is not "is there a window" but
    /// "can the operator still reach the control that started this" - and that
    /// is the same question on both front ends.
    #[test]
    fn a_closing_window_takes_its_own_presses_and_leaves_the_global_ones() {
        let mut i = PttIntent::default();
        for s in PttSource::ALL {
            i.press(s);
        }
        i.window_gone();
        assert!(!i.is_held(PttSource::Mouse), "the mouse lived in that window");
        assert!(!i.is_held(PttSource::Space), "so did its spacebar");
        assert!(!i.is_held(PttSource::Screen), "the screen it was drawn on is gone too");
        assert!(i.is_held(PttSource::Midi), "MIDI is global");
        assert!(i.is_held(PttSource::Bluetooth), "and so is the Bluetooth button");
        assert!(i.want_tx(), "still transmitting on the global ones");
    }

    /// Closing a window with only a mouse press in it does end the transmission
    /// - the case that left a radio keyed until build 29.
    ///
    /// Both window-bound sources on their own, because build 29 was the mouse
    /// and the spacebar is the one nobody tried (review finding).
    #[test]
    fn a_closing_window_ends_a_press_that_only_lived_there() {
        for s in [PttSource::Mouse, PttSource::Space] {
            let mut i = PttIntent::default();
            i.press(s);
            i.window_gone();
            assert!(!i.want_tx(), "{s:?} lived in that window");
        }
    }

    /// One transmitter per intent: they must not share.
    #[test]
    fn two_transmitters_do_not_share_a_request() {
        let mut thetis = PttIntent::default();
        let mut yaesu = PttIntent::default();
        thetis.press(PttSource::Mouse);
        yaesu.press(PttSource::Midi);
        yaesu.release(PttSource::Midi);
        assert!(thetis.want_tx(), "letting go of one radio is not letting go of the other");
        assert!(!yaesu.want_tx());
    }

    // ------------------------------------------------------------------------
    // And now the other half: tested against a caller.
    //
    // A pure function with its own tests is half the work. The fault in build 33
    // was in the ARGUMENT, not in the function - refusal_applies was pure, had
    // four tests, was green, and was handed the wrong value. A test that fills
    // in its own inputs cannot see that.
    //
    // So below is a caller: the send-on-change loop every front end runs. It is
    // deliberately the smallest thing that behaves like drive_yaesu_ptt, and the
    // assertions are about what went out on the wire.

    /// The loop under test: compare the intent with what was last sent, and send
    /// only a change.
    ///
    /// **What this does and does not prove.** It is a contract test on
    /// `want_tx()`: press once, off once, and an exit reaches the wire. That is
    /// worth having and it stays.
    ///
    /// It is *not* the "tested against its caller" half. This is a model of
    /// `drive_yaesu_ptt` written by the same hand as the type, and it leaves out
    /// three things the real loop does: the per-frame suppressors, the clearing
    /// of latches inside the tick, and the level-sampled spacebar. A model that
    /// omits exactly the parts where the faults live proves less than it looks
    /// like it does (review finding, Lane 6 stap 1).
    ///
    /// The real thing needs `drive_yaesu_ptt` to be reachable from a test, and
    /// today no client test constructs an `SdrRemoteApp` at all. That is the
    /// first move of step 2, not something to fake here.
    struct Driver {
        intent: PttIntent,
        last_sent: bool,
        sent: Vec<bool>,
    }

    impl Driver {
        fn new() -> Self {
            Driver { intent: PttIntent::default(), last_sent: false, sent: Vec::new() }
        }

        /// One frame.
        fn tick(&mut self) {
            let want = self.intent.want_tx();
            if want != self.last_sent {
                self.last_sent = want;
                self.sent.push(want);
            }
        }
    }

    #[test]
    fn the_caller_sends_a_press_once_and_a_release_once() {
        let mut d = Driver::new();
        d.intent.press(PttSource::Mouse);
        d.tick();
        d.tick();
        d.tick();
        d.intent.release(PttSource::Mouse);
        d.tick();
        d.tick();
        assert_eq!(d.sent, vec![true, false], "one on, one off, however many frames");
    }

    /// The release edge that mattered: an exit must produce an off on the wire,
    /// exactly like letting go does. This is what a missing exit looked like -
    /// the radio stayed keyed because nothing ever sent the off.
    #[test]
    fn every_exit_puts_an_off_on_the_wire() {
        for (naam, exit) in total_exits() {
            let mut d = Driver::new();
            d.intent.press(PttSource::Mouse);
            d.tick();
            exit(&mut d.intent);
            d.tick();
            assert_eq!(d.sent, vec![true, false], "{naam} did not reach the wire");
        }
    }

    /// And it does not shout: a repeated exit is not a second off.
    #[test]
    fn a_repeated_exit_does_not_send_again() {
        let mut d = Driver::new();
        d.intent.press(PttSource::Midi);
        d.tick();
        d.intent.refused();
        d.tick();
        d.intent.refused();
        d.tick();
        d.intent.disconnected();
        d.tick();
        assert_eq!(d.sent, vec![true, false]);
    }

    /// A window closing while MIDI is held must NOT send an off - the operator
    /// is still transmitting. Getting this wrong would cut a transmission
    /// because a window was tidied away.
    #[test]
    fn closing_a_window_does_not_cut_a_midi_transmission() {
        let mut d = Driver::new();
        d.intent.press(PttSource::Mouse);
        d.intent.press(PttSource::Midi);
        d.tick();
        d.intent.window_gone();
        d.tick();
        assert_eq!(d.sent, vec![true], "still transmitting on MIDI");
        d.intent.release(PttSource::Midi);
        d.tick();
        assert_eq!(d.sent, vec![true, false]);
    }
}

#[cfg(test)]
mod arbitrate_tests {
    use super::arbitrate;

    #[test]
    fn multi_tx_grants_everything_and_blocks_nothing() {
        let a = arbitrate([true, true, true], [false; 3], [false; 3], true);
        assert_eq!(a.granted, [true, true, true]);
        assert_eq!(a.blocked, [false; 3]);
    }

    #[test]
    fn one_asker_is_simply_granted() {
        for i in 0..3 {
            let mut want = [false; 3];
            want[i] = true;
            let a = arbitrate(want, [false; 3], [false; 3], false);
            assert_eq!(a.granted, want, "sleuf {i}");
            assert_eq!(a.blocked, [false; 3], "sleuf {i}");
        }
    }

    /// Whoever is already transmitting keeps it. A press elsewhere does not interrupt
    /// a transmission in progress - that is the heart of the choice.
    #[test]
    fn the_one_already_transmitting_keeps_it() {
        let a = arbitrate([true, true, false], [false, true, false], [false; 3], false);
        assert_eq!(a.granted, [false, true, false], "sleuf 1 zat er al");
        assert_eq!(a.blocked, [true, false, false], "slot 0 gets the refusal");
    }

    #[test]
    fn a_third_press_is_blocked_too() {
        let a = arbitrate([true, true, true], [false, false, true], [false; 3], false);
        assert_eq!(a.granted, [false, false, true]);
        assert_eq!(a.blocked, [true, true, false]);
    }

    /// Two in the same frame with nobody sitting there already: the lowest wins.
    #[test]
    fn two_at_once_falls_to_the_lowest() {
        let a = arbitrate([true, true, false], [false; 3], [false; 3], false);
        assert_eq!(a.granted, [true, false, false]);
        assert_eq!(a.blocked, [false, true, false]);
    }

    /// The properties that actually matter, over the WHOLE input space.
    ///
    /// That is 1024: three wants, three last_sent, three held_by_other and the
    /// multi-TX switch. The first version walked sixty-four of them and held
    /// held_by_other at [false;3] - exactly the axis the fault of build 51 lay
    /// on. A reviewer put that fault back and this loop stayed green; only the
    /// two tests from the repair commit itself fell over. Exhaustive over a
    /// subspace is not exhaustive, and it read as though it were (review
    /// finding).
    #[test]
    fn never_two_never_invented_never_silent() {
        for bits in 0u16..1024 {
            let want = [bits & 1 != 0, bits & 2 != 0, bits & 4 != 0];
            let last = [bits & 8 != 0, bits & 16 != 0, bits & 32 != 0];
            let held = [bits & 64 != 0, bits & 128 != 0, bits & 256 != 0];
            let multi = bits & 512 != 0;
            let a = arbitrate(want, last, held, multi);

            // The property that was missing, and that would have caught build 51: whoever is
            // granted must also be able to transmit. A slot another station is holding
            // cannot.
            for i in 0..3 {
                assert!(!(a.granted[i] && held[i]), "slot {i} granted while another station holds it, at {bits}");
                assert!(!(a.blocked[i] && held[i]), "slot {i} refused by ourselves while another station holds it, at {bits}");
            }
            if multi {
                assert_eq!(a.blocked, [false; 3], "multi-TX never refuses, at {bits}");
                continue;
            }

            assert!(a.granted.iter().filter(|&&g| g).count() <= 1, "two at once, at {bits}");
            for i in 0..3 {
                assert!(!a.granted[i] || want[i], "slot {i} out of nowhere, at {bits}");
                assert!(!a.blocked[i] || want[i], "slot {i} refused without asking, at {bits}");
                assert!(!(a.granted[i] && a.blocked[i]), "slot {i} both at once, at {bits}");
                // Whoever asked and COULD have got it, gets an answer. Whoever finds another
                // station in front of them hears nothing here: their button says "busy", and
                // that is the more specific answer.
                if want[i] && !held[i] {
                    assert!(a.granted[i] || a.blocked[i], "slot {i} got no answer, at {bits}");
                }
            }
        }
    }

    /// A transmitter another station holds does not win, and does not block ours.
    #[test]
    fn a_radio_held_by_someone_else_never_wins_and_never_blocks_ours() {
        // Thetis belongs to somebody else; we want Thetis and Yaesu 1.
        let a = arbitrate([true, true, false], [false; 3], [true, false, false], false);
        assert_eq!(a.granted, [false, true, false], "Yaesu 1 mag gewoon");
        assert_eq!(a.blocked, [false; 3], "and nothing is refused by ourselves");
    }

    /// Even if it was already transmitting: the slot loses it to another station and
    /// must not go on blocking our other radio.
    #[test]
    fn losing_it_to_another_station_does_not_keep_blocking_us() {
        let a = arbitrate([true, true, false], [true, false, false], [true, false, false], false);
        assert_eq!(a.granted, [false, true, false]);
        assert_eq!(a.blocked, [false; 3]);
    }

    /// And with multi-TX on nothing is ever refused. (Covered by the test above as
    /// well, but this is the case by name.)
    #[test]
    fn multi_tx_never_blocks() {
        for bits in 0u8..64 {
            let want = [bits & 1 != 0, bits & 2 != 0, bits & 4 != 0];
            let last = [bits & 8 != 0, bits & 16 != 0, bits & 32 != 0];
            let a = arbitrate(want, last, [false; 3], true);
            assert_eq!(a.blocked, [false; 3], "at {bits}");
            assert_eq!(a.granted, want, "at {bits}");
        }
    }
}

#[cfg(test)]
mod desktop_frame_tests {
    use super::{desktop_frame, Decision, Latches, PttIntent, PttSource};

    /// The rules as they stood in `ui/update.rs` before the extraction, written out
    /// independently from that code - not from `desktop_frame`.
    ///
    /// That is what makes the comparison below a real equivalence test and not an
    /// echo: if the extraction does anything differently from the original, these two
    /// diverge.
    fn zoals_het_was(
        mouse: bool,
        space: bool,
        midi: bool,
        window_open: bool,
        held: bool,
        last_sent: bool,
    ) -> Decision {
        let (mut m, mut s, mut d) = (mouse, space, midi);
        if !window_open {
            m = false;
            s = false;
        }
        if held {
            m = false;
            s = false;
            d = false;
        }
        let want = (m || s || d) && !held;
        Decision {
            latches: Latches { mouse: m, space: s, midi: d, needs_new_press: false },
            want_tx: want,
            send: if want == last_sent { None } else { Some(want) },
        }
    }

    /// **Today's truth table, pinned down.**
    ///
    /// All sixty-four input combinations: three latches, the window, busy, and what
    /// was last sent. This records what the client does at this moment - it does not
    /// say that that is right. It says step 2b may not change it unnoticed, and that
    /// is exactly what was asked for before the hottest loop is touched.
    #[test]
    fn the_extraction_matches_the_loop_it_came_from() {
        let mut gecontroleerd = 0;
        for bits in 0u8..64 {
            let mouse = bits & 1 != 0;
            let space = bits & 2 != 0;
            let midi = bits & 4 != 0;
            let window_open = bits & 8 != 0;
            let held = bits & 16 != 0;
            let last_sent = bits & 32 != 0;

            let got = desktop_frame(
                Latches { mouse, space, midi, needs_new_press: false },
                window_open,
                held,
                last_sent,
            );
            let expected = zoals_het_was(mouse, space, midi, window_open, held, last_sent);
            assert_eq!(
                got, expected,
                "rij {bits}: mouse={mouse} space={space} midi={midi} \
                 window={window_open} held={held} last_sent={last_sent}"
            );
            gecontroleerd += 1;
        }
        assert_eq!(gecontroleerd, 64, "all rows walked");
    }

    /// And the four properties the table carries, named separately - so that a
    /// failing table does not only say "row 37" but also what is broken.

    #[test]
    fn a_closed_window_forgets_its_own_presses_and_keeps_midi() {
        let d = desktop_frame(
            Latches { mouse: true, space: true, midi: true, needs_new_press: false },
            false,
            false,
            false,
        );
        assert!(!d.latches.mouse && !d.latches.space, "those lived in the window");
        assert!(d.latches.midi, "MIDI is global");
        assert!(d.want_tx, "still transmitting on MIDI");
    }

    #[test]
    fn busy_drops_everything_including_midi() {
        let d = desktop_frame(
            Latches { mouse: true, space: true, midi: true, needs_new_press: false },
            true,
            true,
            false,
        );
        assert_eq!(d.latches, Latches::default());
        assert!(!d.want_tx);
    }

    /// Grabbing the spacebar while a latching control is on takes it over.
    #[test]
    fn a_hold_control_takes_over_from_a_latch() {
        let getoggeld = Latches { midi: true, ..Default::default() };
        let na = getoggeld.hold(PttSource::Space, true, &[PttSource::Space]);

        assert!(!na.midi, "the latch stayed behind the spacebar");
        assert!(na.space);

        // Letting go stops it - there is nothing left holding it up.
        let los = na.hold(PttSource::Space, false, &[PttSource::Space]);
        assert!(!los.space && !los.midi);
        let d = desktop_frame(los, true, false, true);
        assert!(!d.want_tx);
        assert_eq!(d.send, Some(false));
    }

    /// Two hold controls do not fight. Both are re-read every frame, so if each
    /// cleared the other on its rising edge they would do it every frame for as
    /// long as both were down.
    #[test]
    fn two_hold_controls_can_be_down_together() {
        let both = [PttSource::Space, PttSource::Mouse];
        let mut l = Latches::default();
        for _ in 0..3 {
            l = l.hold(PttSource::Mouse, true, &both);
            l = l.hold(PttSource::Space, true, &both);
        }
        assert!(l.mouse && l.space, "they cleared each other");
    }

    /// A latching press while something else is keying means stop, not "add me".
    ///
    /// The desktop case the owner found: the spacebar is keying, you click the
    /// PTT button, nothing appears to happen - and when you let the spacebar go
    /// the transmitter stays on, because the click had quietly set its own
    /// latch. Now the click ends the transmission, like it does on the phone.
    #[test]
    fn a_latching_press_while_another_source_keys_means_stop() {
        let keying = Latches { space: true, ..Default::default() };
        let na = keying.latching_press(PttSource::Mouse, &[PttSource::Space]);

        assert!(!na.mouse, "the click armed its own latch instead of stopping");
        assert!(na.needs_new_press, "the spacebar is still down, so it has to wait");

        // And letting the spacebar go leaves nothing behind. Released through
        // hold(), because that is where a release is observed - building a
        // struct with space: false is a reconstruction and says nothing.
        let los = na.hold(PttSource::Space, false, &[PttSource::Space]);
        let d = desktop_frame(los, true, false, false);
        assert!(!d.want_tx, "the transmitter came back on its own");
        assert!(!d.latches.needs_new_press);
    }

    /// With nothing keying, a latching press is simply a press.
    #[test]
    fn a_latching_press_on_a_quiet_slot_keys_it() {
        let d = Latches::default().latching_press(PttSource::Mouse, &[PttSource::Space]);
        assert!(d.mouse);
        assert!(!d.needs_new_press);
    }

    /// Dropping the presses of a slot does not end a wait for a fresh press.
    #[test]
    fn released_keeps_the_wait() {
        let l = Latches { mouse: true, space: true, midi: true, needs_new_press: true };
        let r = l.released();
        assert!(!r.mouse && !r.space && !r.midi, "the presses go");
        assert!(r.needs_new_press, "and the wait stays");
        assert!(!Latches::default().released().needs_new_press);
    }

    /// Whatever order the caller works in, the stored state says the same.
    ///
    /// The caller reads the spacebar out of the input state and runs the exit
    /// from the state sync, and those two are not ordered with respect to each
    /// other. So both orders have to end in the same place.
    #[test]
    fn the_wait_does_not_depend_on_the_order_of_the_frame() {
        // Exit first, key re-read afterwards.
        let mut a = PttIntent::default();
        a.press(PttSource::Space);
        a.radio_left_tx(&[PttSource::Space]);
        a.set(PttSource::Space, true);

        // Key re-read first, exit afterwards.
        let mut b = PttIntent::default();
        b.press(PttSource::Space);
        b.set(PttSource::Space, true);
        b.radio_left_tx(&[PttSource::Space]);

        for (naam, i) in [("exit first", &a), ("re-read first", &b)] {
            assert!(!i.want_tx(), "{naam}: it keyed the radio again");
            assert!(i.needs_new_press(), "{naam}: the wait was lost");
            assert!(i.is_held(PttSource::Space), "{naam}: the key is still down");
        }
    }

    /// An edge-driven control does not get a wait, and must not: it never
    /// reports a release, so the wait would never end.
    ///
    /// The phone's toggle button is the case. It sends a press and nothing
    /// else; if the radio dropping out of TX left a wait behind, that button
    /// would be dead for the rest of the session.
    #[test]
    fn an_edge_driven_control_is_free_again_at_once() {
        for source in [PttSource::Screen, PttSource::Midi, PttSource::Bluetooth] {
            let mut i = PttIntent::default();
            i.press(source);
            i.radio_left_tx(&[PttSource::Space]);
            assert!(!i.needs_new_press(), "{source:?} would have been stuck");

            i.press(source);
            assert!(i.want_tx(), "{source:?} could not transmit again");
        }
    }

    /// Closure: after an exit that ends the asking, one fresh press transmits.
    ///
    /// The property the nine tests of builds 65-72 did not have. A reviewer
    /// walked all five exits and found each of them leaving the next press dead,
    /// because the wait had exactly one way out and it lived in a caller
    /// (review finding, 2026-09-05).
    ///
    /// `radio_left_tx` is deliberately not in this list: that one MEANS "wait
    /// for a release", and the test above covers it. These four mean the asking
    /// is over, and then a wait is a state with nobody left to leave it.
    ///
    /// No release step here, and that is the point. My first version of this
    /// test released everything through `settle()` first - which clears the wait
    /// itself, so the test would have passed with the defect still in. It tested
    /// its own tidying up.
    #[test]
    fn an_exit_that_ends_the_asking_leaves_no_wait_behind() {
        let exits: [(&str, fn(&mut PttIntent)); 4] = [
            ("refused", |i| i.refused()),
            ("busy", |i| i.busy()),
            ("disconnected", |i| i.disconnected()),
            ("window_gone", |i| i.window_gone()),
        ];

        for (naam, exit) in exits {
            // A wait is running: the radio stopped while the key was held.
            let mut i = PttIntent::default();
            i.press(PttSource::Space);
            i.radio_left_tx(&[PttSource::Space]);
            assert!(i.needs_new_press(), "{naam}: the setup did not arm a wait");

            exit(&mut i);
            assert!(
                !i.needs_new_press(),
                "{naam}: left a wait behind with nothing held to release"
            );

            i.press(PttSource::Space);
            assert!(i.want_tx(), "{naam}: a fresh press did not transmit");
        }
    }

    /// A held key does not put the transmitter straight back after the radio
    /// dropped out of TX by itself.
    ///
    /// The case the owner found: a 991A with its time-out timer at a minute,
    /// the spacebar held down. Clearing the latch is not enough - the desktop
    /// re-reads the spacebar every frame, so it was back immediately and the
    /// radio was keyed again, into the same time-out. Round and round.
    #[test]
    fn a_held_key_waits_for_a_release_after_the_radio_dropped() {
        let mut i = PttIntent::default();
        i.press(PttSource::Space);
        assert!(i.want_tx());

        i.radio_left_tx(&[PttSource::Space]);
        assert!(!i.want_tx(), "the radio said no");

        // The key stays held, and that is what makes this survive the frame it
        // was set in. Clearing it here meant "nothing is held", which ends the
        // wait - so whether it lasted depended on whether the window that reads
        // the spacebar happened to be drawn before or after this. It did not
        // last, twice.
        assert!(i.is_held(PttSource::Space), "the key is down; saying otherwise loses the wait");
        assert!(i.needs_new_press());

        // The next frame reads the spacebar again - still held.
        i.set(PttSource::Space, true);
        assert!(!i.want_tx(), "a held key must not put it straight back");

        // Let go, and it is over - but only where a release is observed.
        i.set(PttSource::Space, false);
        i.settle(&[PttSource::Space]);
        assert!(!i.needs_new_press(), "releasing everything ends the wait");

        // A fresh press transmits.
        i.press(PttSource::Space);
        assert!(i.want_tx(), "a fresh press must work");
    }

    /// And the same through the desktop frame, where the flag has to survive
    /// being rebuilt every frame.
    #[test]
    fn the_desktop_frame_carries_the_wait_across_frames() {
        // Transmitting on the spacebar.
        let held = Latches { space: true, ..Default::default() };
        let d = desktop_frame(held, true, false, false);
        assert!(d.want_tx);

        // The radio drops out of TX: the caller runs the exit and stores what
        // comes out of it.
        let mut i = PttIntent::default();
        i.set(PttSource::Space, true);
        i.radio_left_tx(&[PttSource::Space]);
        let na = Latches {
            mouse: i.is_held(PttSource::Mouse),
            space: i.is_held(PttSource::Space),
            midi: i.is_held(PttSource::Midi),
            needs_new_press: i.needs_new_press(),
        };

        // Next frame, spacebar still down.
        let d2 = desktop_frame(Latches { space: true, ..na }, true, false, true);
        assert!(!d2.want_tx, "it keyed the radio again while the key was still down");
        assert_eq!(d2.send, Some(false), "and it has to tell the server once");

        // Released - through hold(), the one place that observes it.
        let los = d2.latches.hold(PttSource::Space, false, &[PttSource::Space]);
        let d3 = desktop_frame(los, true, false, false);
        assert!(!d3.latches.needs_new_press, "the wait ends on release");

        // Fresh press.
        let opnieuw = d3.latches.hold(PttSource::Space, true, &[PttSource::Space]);
        let d4 = desktop_frame(opnieuw, true, false, false);
        assert!(d4.want_tx, "a fresh press must work");
    }

    /// A press made while busy does not fire once the other one lets go: it is
    /// cleared, not remembered.
    ///
    /// **What this test is about: a CLICK.** It checks that a momentary press which is
    /// refused does not stay behind to fire later.
    ///
    /// A held key is a different thing and is meant to behave differently: it goes on
    /// saying "I want to transmit" and fires as soon as it can. See the rule at
    /// `want_tx` - that is intended behaviour, not a gap in this test.
    #[test]
    fn a_press_made_while_busy_does_not_fire_afterwards() {
        let tijdens = desktop_frame(Latches { mouse: true, ..Default::default() }, true, true, false);
        assert!(!tijdens.want_tx);
        // The other one lets go; the latches are what the previous frame left of them.
        let erna = desktop_frame(tijdens.latches, true, false, false);
        assert!(!erna.want_tx, "the press made while busy is gone");
        assert_eq!(erna.send, None);
    }

    #[test]
    fn only_a_change_goes_out() {
        let aan = desktop_frame(Latches { midi: true, ..Default::default() }, true, false, false);
        assert_eq!(aan.send, Some(true));
        let nog_steeds = desktop_frame(Latches { midi: true, ..Default::default() }, true, false, true);
        assert_eq!(nog_steeds.send, None, "niets veranderd, niets sturen");
        let uit = desktop_frame(Latches::default(), true, false, true);
        assert_eq!(uit.send, Some(false));
    }
}
