// SPDX-License-Identifier: GPL-2.0-or-later
//! A refusal from the server, in a shape the interface cannot miss.
//!
//! The server refuses a PTT request, or lets go of one it had granted. Either
//! way this side has to stop believing it is transmitting: the button comes
//! down and the control that was held gets released.
//!
//! Two things went wrong with the flat `bool` this replaces, and they are
//! different faults that happen to share a field.
//!
//! **It was one flag for three transmitters.** Thetis, Yaesu 1 and Yaesu 2 all
//! set it and all cleared it. So a PTT-off on one of them wiped a refusal that
//! belonged to another. On 2026-09-05 that is exactly what happened: the
//! desktop sends a Thetis PTT-off on every frame (`apply_thetis_ptt` was a
//! transition handler being called as a level handler), and it wiped the
//! Yaesu refusal about sixteen milliseconds after it arrived. The server said
//! it had let go, the client wrote the line in its log proving it understood,
//! and the button stayed red for another ten seconds until the operator let go
//! himself.
//!
//! **It was a level where an event was needed.** An interface reads state; it
//! does not see every value that state passed through. The engine publishes on
//! a `watch` channel, which keeps only the newest value, and the interface
//! samples it once a frame - so anything that goes up and back down between
//! two samples never happened as far as the screen is concerned. That is not a
//! timing accident to be tuned away, it is what sampling means.
//!
//! The comment at the old call site already described this class, about a
//! different fault in build 34, and it was right. It was written next to a
//! repair of that one occurrence, and the shape stayed. Hence `seq`: it only
//! ever goes up, so a reader comparing it with its own copy finds out that a
//! refusal happened even if it never saw the state it happened in.
//!
//! `mask` says which transmitters the refusal could be about. The protocol
//! does not carry that yet - `PttDenied` is four bytes of header and no body -
//! so the honest answer is "the ones that were asking", which is exact in the
//! ordinary case of one at a time and deliberately over-broad when several
//! ask at once. Better an answer that admits its own width than a slot number
//! that was guessed. Narrowing it is a protocol change and its own patch.

use sdr_remote_core::protocol::DeniedTarget;

/// Thetis, Yaesu 1, Yaesu 2 - the three things that can transmit.
pub const TRANSMITTERS: usize = 3;

/// A refusal, and what is still standing because of it.
///
/// `Default` is "nothing refused, nothing outstanding", which is also what a
/// fresh interface believes, so the two agree without either being told.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PttDenial {
    /// Rises by one per refusal that applied. Never reset while the engine
    /// lives, not even on a disconnect: a reader that compares it with its own
    /// copy would read a reset as a fresh refusal and let go of a control the
    /// operator is holding, at the moment the link comes back.
    pub seq: u32,
    /// Bit per transmitter the LAST refusal was about. Part of the event, so
    /// it is never taken back: a reader that arrives one frame late still has
    /// to know what to let go of.
    ///
    /// This is the half that was still a level, and a reviewer caught it. The
    /// notification could not be missed any more, but the reader then asked
    /// "which transmitters is it about" of the current situation - and by then
    /// the refusal could already be settled, so the answer was "none" and the
    /// whole block ran as an expensive way of doing nothing. Read through
    /// `was_about`, and only when `seq` has moved.
    covered: u8,
    /// Bit per transmitter still refused right now. This one IS a level, and
    /// that is what it is for: it paints the "PTT blocked" sign.
    ///
    /// It adds up, and that is the point. A refusal about the radio while a
    /// refused Thetis is still being held does not make Thetis un-refused -
    /// both are refused, and overwriting here dropped the older one on the
    /// floor. It is emptied a transmitter at a time, by whoever lets that
    /// transmitter go.
    outstanding: u8,
    /// Whether the server let go by itself (the radio stopped, or its time-out
    /// timer was about to expire) rather than handing the radio to somebody
    /// else. The two need different answers: another operator holding it lets
    /// a held key keep asking, because its turn comes. The radio stopping does
    /// not - asking again walks straight back into the same time-out.
    pub server_released: bool,
}

impl PttDenial {
    /// A refusal arrived. Returns whether it applied to anything.
    ///
    /// Not `refused`: `PttIntent` already has an exit by that name, and one
    /// verb for two different things costs more than the extra word. A fitness
    /// counter walked into it first - it counts PttIntent exits by name, and
    /// with both spellings identical it was quietly counting two things as
    /// one.
    ///
    /// Only while we are asking: a refusal is the answer to a request, and the
    /// answer cannot outlive the question.
    ///
    /// It did once, and that is a busy sign that will not go away. Thetis PTT
    /// rides on the audio packets, so a held button sends tens per second and
    /// the server refuses each one separately. The client let go after the
    /// first refusal - which settles it once - and then the rest of the
    /// refusals arrived and raised it again, with no second release left to
    /// settle them. The button stayed "TX in use" with nobody transmitting
    /// until the app was restarted.
    /// `target` is the transmitter the server named, or `None` when it did not
    /// say - which every server before this one does, and which is why the
    /// fallback below has to stay.
    ///
    /// Named: exactly that transmitter, and only while we are asking for it. A
    /// refusal about something we are not asking for is an answer to a question
    /// somebody else asked.
    ///
    /// Not named: everything we are asking for, because that is all the client
    /// can know. With two radios keyed that is both of them, so one radio's
    /// refusal releases the other - and no arrangement of the client can tell
    /// them apart. That is not a shortcoming of the fallback; it is the reason
    /// the server now says which.
    pub fn apply_refusal(
        &mut self,
        asking: [bool; TRANSMITTERS],
        target: DeniedTarget,
        server_released: bool,
    ) -> bool {
        // The packet's own type, not an index worked out by the caller. That
        // step had nowhere to be tested - it sat in the engine loop, and a
        // caller that quietly passed `None` would have thrown the server's
        // answer away with every test still green. Twice now a reviewer has
        // found exactly that shape: a hop between the packet and the rule that
        // nothing exercised.
        let mask = match target.index() {
            Some(i) if i < TRANSMITTERS && asking[i] => 1u8 << i,
            Some(_) => 0,
            None => asking
                .iter()
                .enumerate()
                .fold(0u8, |m, (i, &a)| if a { m | (1 << i) } else { m }),
        };
        if mask == 0 {
            return false;
        }
        self.seq = self.seq.wrapping_add(1);
        self.covered = mask;
        self.outstanding |= mask;
        self.server_released = server_released;
        true
    }

    /// This transmitter is no longer transmitting, so whatever was outstanding
    /// for it is settled. Only for it: the whole point of the mask.
    pub fn transmitter_off(&mut self, which: usize) {
        if which < TRANSMITTERS {
            self.outstanding &= !(1u8 << which);
        }
    }

    /// The link went away. Nothing is outstanding any more, but `seq` stays -
    /// see the field.
    pub fn clear(&mut self) {
        self.outstanding = 0;
    }

    /// Something is still refused - this is what paints the "PTT blocked" sign.
    pub fn active(&self) -> bool {
        self.outstanding != 0
    }

    /// Whether the LAST refusal was about this transmitter.
    ///
    /// Past tense on purpose. This answers "what did that refusal mean", not
    /// "what is refused now" - so read it when `seq` has moved and not
    /// otherwise, or it will describe a refusal that is long settled.
    pub fn was_about(&self, which: usize) -> bool {
        which < TRANSMITTERS && self.covered & (1u8 << which) != 0
    }

    /// What a reader that has seen `missed` refusals go by has to let go of.
    ///
    /// Thin on purpose: the rule itself is `transmitters_to_release`, which is
    /// a free function so it can be tested without building a record for every
    /// case.
    pub fn to_release(&self, missed: u32) -> u8 {
        transmitters_to_release(missed, self.covered, self.outstanding)
    }

    /// The same answer, per transmitter.
    pub fn releases(&self, missed: u32, which: usize) -> bool {
        which < TRANSMITTERS && self.to_release(missed) & (1u8 << which) != 0
    }

    /// "The server let go by itself, and it still stands."
    ///
    /// Both front ends need this and neither should assemble it itself - the
    /// desktop reads the event, an interface that only reads levels needs the
    /// two ANDed or it keeps showing the last refusal's reason long after that
    /// refusal was settled. `server_released` belongs to the event and is not
    /// taken back by `transmitter_off`, which is exactly why this exists.
    pub fn released_by_server_now(&self) -> bool {
        self.active() && self.server_released
    }
}

/// Which transmitters a reader has to let go of, having seen `missed` refusals
/// go by since it last looked.
///
/// One refusal: exactly what it was about, and nothing else.
///
/// More than one: the record only carries the newest one's payload, so the
/// earlier ones are gone. `outstanding` is what saves it - a refusal that has
/// not been settled is still standing there, whichever refusal raised it. So
/// the answer is "the last one, plus everything still refused", which covers
/// the missed refusals that still matter and leaves alone the transmitters
/// that were never involved.
///
/// Releasing everything on a jump was the first answer and it was too broad. A
/// held Thetis that is being refused produces refusals at audio rate, so two
/// in one frame is ordinary rather than exotic - and it let go of both radios
/// every time, for refusals that were only ever about Thetis (review finding).
pub fn transmitters_to_release(missed: u32, covered: u8, outstanding: u8) -> u8 {
    if missed <= 1 {
        covered
    } else {
        covered | outstanding
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THETIS: usize = 0;
    const YAESU1: usize = 1;
    const YAESU2: usize = 2;

    #[test]
    fn a_refusal_applies_only_while_we_are_asking() {
        let mut d = PttDenial::default();
        assert!(!d.apply_refusal([false, false, false], DeniedTarget::NotStated, false), "nothing outstanding");
        assert_eq!(d.seq, 0);
        assert!(!d.active());

        assert!(d.apply_refusal([false, true, false], DeniedTarget::NotStated, true));
        assert_eq!(d.seq, 1);
        assert!(d.active());
        assert!(d.was_about(YAESU1));
        assert!(!d.was_about(THETIS));
        assert!(d.server_released);
    }

    /// The fault of 2026-09-05, in one test.
    ///
    /// The desktop sent a Thetis PTT-off on every frame. With one flag for
    /// three transmitters that wiped the Yaesu refusal before the screen could
    /// read it, and the button stayed red through a transmission that had
    /// already been stopped at the other end.
    #[test]
    fn a_thetis_ptt_off_leaves_a_yaesu_refusal_alone() {
        let mut d = PttDenial::default();
        d.apply_refusal([false, true, false], DeniedTarget::NotStated, true);

        d.transmitter_off(THETIS);

        assert!(d.active(), "a Thetis PTT-off cleared a Yaesu refusal");
        assert!(d.was_about(YAESU1));
    }

    #[test]
    fn its_own_ptt_off_does_settle_it() {
        let mut d = PttDenial::default();
        d.apply_refusal([false, true, false], DeniedTarget::NotStated, true);
        d.transmitter_off(YAESU1);
        assert!(!d.active());
    }

    /// The old-server path: no transmitter in the packet, so a refusal that
    /// arrives while two are asking covers both. Over-broad on purpose: releasing a control
    /// nobody refused is a nuisance, believing a refusal was handled when it
    /// was not is the fault this module exists for.
    #[test]
    fn two_asking_at_once_makes_the_refusal_cover_both() {
        let mut d = PttDenial::default();
        d.apply_refusal([false, true, true], DeniedTarget::NotStated, false);
        assert!(d.was_about(YAESU1) && d.was_about(YAESU2));

        d.transmitter_off(YAESU1);
        assert!(d.active(), "the other one is still outstanding");
        d.transmitter_off(YAESU2);
        assert!(!d.active());
    }

    /// The one the old flat flag could not pass, and the reason for `seq`.
    ///
    /// The interface does not see every published state - it samples. Three
    /// readers watch the same twenty publishes at different rates, and the
    /// point is the difference between them:
    ///
    ///   * `by_counter` compares `seq` with its own copy - the new way.
    ///   * `by_level` watches `active()` go up - the old way.
    ///   * `could_act` is the one that matters: having noticed, does the
    ///     reader still know which transmitter to let go of?
    ///
    /// The first version of this test had only the first reader, so it proved
    /// nothing about the difference: with no counter in the old code it failed
    /// at every sample rate for the same reason, and N=1 and N=10 did exactly
    /// the same thing. A reviewer put the level reader beside it and the table
    /// appeared - `by_level` sees the refusal at N=1 and misses it from N=2,
    /// and `could_act` was 0 from N=2 as well, because the reader was asking
    /// what is refused now instead of what that refusal was about.
    #[test]
    fn a_refusal_survives_an_interface_that_only_samples() {
        // (sample rate, by_counter, by_level, could_act)
        let expected = [(1usize, 1, 1, 1), (2, 1, 0, 1), (10, 1, 0, 1)];

        for (sample_every, want_counter, want_level, want_act) in expected {
            let mut d = PttDenial::default();
            let (mut by_counter, mut by_level, mut could_act) = (0, 0, 0);
            let mut seen_seq = d.seq;
            let mut seen_active = d.active();

            for publish in 1..=20usize {
                match publish {
                    // The refusal arrives ...
                    5 => {
                        d.apply_refusal([false, true, false], DeniedTarget::NotStated, true);
                    }
                    // ... and is settled again before the next look.
                    6 => d.transmitter_off(YAESU1),
                    _ => {}
                }
                if publish % sample_every != 0 {
                    continue;
                }
                if d.seq != seen_seq {
                    seen_seq = d.seq;
                    by_counter += 1;
                    if d.was_about(YAESU1) {
                        could_act += 1;
                    }
                }
                if d.active() && !seen_active {
                    by_level += 1;
                }
                seen_active = d.active();
            }

            assert_eq!(
                (by_counter, by_level, could_act),
                (want_counter, want_level, want_act),
                "sampling every {} publishes",
                sample_every
            );
        }
    }

    /// What a refusal was about is history and stays readable; what is still
    /// refused is a level and does not.
    #[test]
    fn a_settled_refusal_still_says_what_it_was_about() {
        let mut d = PttDenial::default();
        d.apply_refusal([false, true, false], DeniedTarget::NotStated, true);
        d.transmitter_off(YAESU1);

        assert!(!d.active(), "nothing outstanding any more");
        assert!(d.was_about(YAESU1), "but a late reader still knows what to let go of");
    }

    /// An interface that reads levels must not keep showing the last refusal's
    /// reason after that refusal is settled. `server_released` belongs to the
    /// event, so the two have to be read together.
    #[test]
    fn the_reason_does_not_outlive_the_refusal_for_a_level_reader() {
        let mut d = PttDenial::default();
        d.apply_refusal([false, true, false], DeniedTarget::NotStated, true);
        assert!(d.released_by_server_now());

        d.transmitter_off(YAESU1);
        assert!(!d.released_by_server_now(), "sticky until the disconnect");
    }

    /// A refusal replaces the one before it - it does not add to it.
    ///
    /// `covered |= mask` instead of `covered = mask` is one character and the
    /// whole suite stayed green, because every test until now started from
    /// `default()` and did exactly one refusal. It would put back the fault
    /// this module was written for: `covered` only ever grows, so after one
    /// refusal about radio 1 that latch is released on every later refusal,
    /// whatever it was about (review finding).
    #[test]
    fn a_second_refusal_replaces_the_first() {
        let mut d = PttDenial::default();
        d.apply_refusal([false, true, false], DeniedTarget::NotStated, true);
        d.transmitter_off(YAESU1);

        d.apply_refusal([true, false, false], DeniedTarget::NotStated, false);

        assert!(d.was_about(THETIS));
        assert!(!d.was_about(YAESU1), "an old refusal is still releasing that latch");
        assert!(!d.was_about(YAESU2));
    }

    /// And so does its reason. The desktop reads `server_released` straight
    /// off the record when `seq` moves, so it has to describe THAT refusal:
    /// setting it only when it is true leaves the previous answer standing,
    /// and a held key is then released for a reason that has passed
    /// (review finding).
    #[test]
    fn the_reason_belongs_to_the_refusal_that_carries_it() {
        let mut d = PttDenial::default();
        d.apply_refusal([false, true, false], DeniedTarget::NotStated, true);
        assert!(d.server_released);

        // Now somebody else simply has the radio.
        d.apply_refusal([false, true, false], DeniedTarget::NotStated, false);
        assert!(!d.server_released, "the previous refusal's reason is still standing");
    }

    /// The event carries the newest refusal, not a running total.
    ///
    /// The test above settles the first refusal before the second arrives, so
    /// `outstanding` is empty there and `covered = outstanding | mask` cannot
    /// be told apart from `covered = mask`. That is the wrong case to prove it
    /// on: the ordinary way to be refused twice is to still be holding the
    /// first (review finding).
    #[test]
    fn a_second_refusal_replaces_the_first_while_the_first_still_stands() {
        let mut d = PttDenial::default();
        d.apply_refusal([true, false, false], DeniedTarget::NotStated, false);
        // No settling: the operator is still holding Thetis down.
        d.apply_refusal([false, true, false], DeniedTarget::NotStated, true);

        assert!(d.was_about(YAESU1));
        assert!(!d.was_about(THETIS), "the event is carrying an older refusal too");
        // But both are still refused, and the sign has to say so.
        assert!(d.active());
    }

    /// A refusal that applies to nothing must not erase the one before it.
    ///
    /// Late refusals are ordinary - `apply_refusal` says so itself, about
    /// Thetis PTT riding on the audio packets. If such a refusal cleared the
    /// record, the payload would be gone before the reader had looked, while
    /// `seq` had already moved. That is the same hole as reading the level,
    /// through a third door (review finding).
    #[test]
    fn a_refusal_that_applies_to_nothing_erases_nothing() {
        let mut d = PttDenial::default();
        d.apply_refusal([false, true, false], DeniedTarget::NotStated, true);
        let seq_after_the_real_one = d.seq;

        assert!(!d.apply_refusal([false, false, false], DeniedTarget::NotStated, false), "nothing outstanding");

        assert_eq!(d.seq, seq_after_the_real_one, "and it is not a refusal to count");
        assert!(d.was_about(YAESU1), "the reader had not looked yet");
        assert!(d.server_released);
    }

    /// What a reader that missed one has to let go of.
    #[test]
    fn a_missed_refusal_reaches_what_is_still_refused_and_no_further() {
        const THETIS_BIT: u8 = 1 << THETIS;
        const YAESU1_BIT: u8 = 1 << YAESU1;
        const YAESU2_BIT: u8 = 1 << YAESU2;

        // Saw them all: exactly this refusal, whatever else stands.
        assert_eq!(transmitters_to_release(1, YAESU1_BIT, THETIS_BIT | YAESU1_BIT), YAESU1_BIT);
        assert_eq!(transmitters_to_release(0, 0, THETIS_BIT), 0);

        // Missed one. A held Thetis being refused over and over is the ordinary
        // case, and it must not drag the radios along.
        assert_eq!(transmitters_to_release(2, THETIS_BIT, THETIS_BIT), THETIS_BIT);

        // But a radio that ran into its time-out while Thetis was being refused
        // has to come along, even though the newest refusal was not about it.
        assert_eq!(
            transmitters_to_release(2, THETIS_BIT, THETIS_BIT | YAESU1_BIT),
            THETIS_BIT | YAESU1_BIT
        );
        assert_eq!(transmitters_to_release(3, YAESU2_BIT, 0), YAESU2_BIT);
    }

    /// A refusal about one transmitter does not un-refuse another.
    ///
    /// `outstanding |= mask` back to `= mask` left all thirteen tests green,
    /// and that one character is the whole of what the level is for. The test
    /// above ends on `active()`, which is still true with the newer refusal's
    /// bit alone - so it could not tell the two apart. This one settles the
    /// newer refusal and asks whether the older one is still standing, which
    /// nothing but accumulation can answer (review finding).
    #[test]
    fn a_standing_refusal_survives_a_refusal_about_something_else() {
        let mut d = PttDenial::default();
        d.apply_refusal([true, false, false], DeniedTarget::NotStated, false);
        d.apply_refusal([false, true, false], DeniedTarget::NotStated, true);

        // Only the radio is let go of. Nobody has let go of Thetis.
        d.transmitter_off(YAESU1);

        assert!(d.active(), "the older refusal was overwritten by the newer one");
        // And letting go of Thetis as well does end it.
        d.transmitter_off(THETIS);
        assert!(!d.active());
    }

    /// What the interface actually reads, read the way it reads it.
    ///
    /// Nothing exercised `to_release` or `releases` - the two the client calls.
    /// Swapping the helper's arguments survived, and so did a `releases` that
    /// ignores which transmitter it is asked about, which is "let go of
    /// everything": the answer round three took out (review finding).
    #[test]
    fn what_the_interface_reads_off_a_real_record() {
        let mut d = PttDenial::default();
        d.apply_refusal([true, false, false], DeniedTarget::NotStated, false);
        d.apply_refusal([false, true, false], DeniedTarget::NotStated, true);

        // Saw both go by: exactly the newest one.
        assert!(d.releases(1, YAESU1));
        assert!(!d.releases(1, THETIS), "an older refusal is being replayed");
        assert!(!d.releases(1, YAESU2));

        // Missed one: the newest plus whatever is still refused, and no more.
        assert!(d.releases(2, YAESU1));
        assert!(d.releases(2, THETIS));
        assert!(!d.releases(2, YAESU2), "a transmitter that was never refused at all");
    }

    /// The point of the whole protocol change, in one test.
    ///
    /// Both radios keyed, and the server refuses one of them. Told which, the
    /// other one is left alone. Not told, both go - and that was the ceiling
    /// the client could not get above, whatever it did with the refusal
    /// afterwards.
    #[test]
    fn a_named_transmitter_leaves_the_others_keyed() {
        let asking = [true, true, true];

        let mut told = PttDenial::default();
        told.apply_refusal(asking, DeniedTarget::Radio1, true);
        assert!(told.was_about(YAESU1));
        assert!(!told.was_about(YAESU2), "the other radio has nothing to do with it");
        assert!(!told.was_about(THETIS));

        let mut not_told = PttDenial::default();
        not_told.apply_refusal(asking, DeniedTarget::NotStated, true);
        assert!(not_told.was_about(YAESU1) && not_told.was_about(YAESU2) && not_told.was_about(THETIS));
    }

    /// A refusal about something we are not asking for is somebody else's
    /// answer. It used to be impossible to notice: without a transmitter in the
    /// packet, "are we asking at all" was the only question that could be
    /// asked.
    #[test]
    fn a_refusal_about_a_transmitter_we_are_not_asking_for_applies_to_nothing() {
        let mut d = PttDenial::default();
        assert!(!d.apply_refusal([false, true, false], DeniedTarget::Radio2, true));
        assert_eq!(d.seq, 0);
        assert!(!d.active());
    }

    /// And with a named transmitter the jump case stops being an approximation:
    /// `outstanding` now holds transmitters that were really refused, not
    /// everything that happened to be keyed at the time.
    #[test]
    fn a_missed_refusal_is_exact_once_the_server_names_them() {
        let mut d = PttDenial::default();
        // Thetis refused while everything is keyed ...
        d.apply_refusal([true, true, true], DeniedTarget::Thetis, false);
        // ... and then the radio runs into its time-out.
        d.apply_refusal([true, true, true], DeniedTarget::Radio1, true);

        // A reader that saw neither go by lets go of exactly those two.
        assert!(d.releases(2, THETIS));
        assert!(d.releases(2, YAESU1));
        assert!(!d.releases(2, YAESU2), "never refused, so never released");
    }

    /// A disconnect must not read as a refusal when the link returns. `seq`
    /// going back to zero would do exactly that, and it would let go of a key
    /// the operator is holding at the worst possible moment.
    #[test]
    fn a_disconnect_is_not_a_refusal() {
        let mut d = PttDenial::default();
        d.apply_refusal([false, true, false], DeniedTarget::NotStated, true);
        let seen_seq = d.seq;

        d.clear();

        assert!(!d.active());
        assert!(!d.released_by_server_now());
        assert_eq!(d.seq, seen_seq, "a reader would read this as a new refusal");
    }
}
