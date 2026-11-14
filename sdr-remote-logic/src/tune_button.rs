// SPDX-License-Identifier: GPL-2.0-or-later
//! The Thetis TUNE button, with its PA bypass: one machine for the desktop and
//! Android (the bridge holds one for as long as the app runs, not as long as a
//! screen is shown).
//!
//! What it does:
//!
//! - **On**: a PA in operate goes to standby first, and the TUNE follows
//!   [`PA_STANDBY_WAIT`] later; without one the TUNE goes at once. A start
//!   still waiting after [`PA_STANDBY_GIVE_UP`] (the process was frozen) is
//!   dropped rather than sent late, and so is one that finds a TUNE of
//!   someone else running when it would go.
//! - **Off**: the TUNE goes off at once, and the PAs this press put in standby
//!   go back [`PA_RESTORE_WAIT`] later. Only those, and only while connected
//!   (a link that is away is waited for, see below):
//!   not "the active PA" (with none marked active, the default, none went back),
//!   and never a PA we did not switch. The SPE Expert's one command is a
//!   toggle, so it goes back only while it is in standby: one the operator
//!   put in operate again in the meantime stays there.
//! - **What it shows** follows the server, so after a link that dropped, or a
//!   TUNE switched in Thetis or by another client, it does not claim the wrong
//!   state. For [`OWN_PRESS_HOLD`] after a press of our own it shows the press,
//!   and a server that never said whether Thetis tunes (one from before 2.13.3)
//!   leaves it on the press: following that one would show "off" a second after
//!   "on", and every press would send "on" again. While our start waits for the
//!   PA, the server is not followed at all: a TUNE of someone else in those
//!   500 ms is not ours, and must neither end ours nor confirm it.
//! - **A TUNE that ended elsewhere** (the server's safety, SWR, the Thetis
//!   button, another client) puts the PAs back as our own "off" does. Only once
//!   the server said "on" for it and then "off": a lost "on" from the server
//!   would otherwise look like an end, with the carrier still there. And only
//!   with Thetis on and the link there, at the end and again when the PA goes
//!   back, and not while the server says a TUNE runs then: a Thetis that went
//!   away is said to be off, while stock Thetis keeps the carrier of a TCI
//!   client that vanished.
//! - **A put-back that cannot go** because the server still says TUNE (a lost
//!   "off", or another client tuning) keeps the memory, and the PA goes back
//!   once the server says "off". One that cannot go because the link is away
//!   waits for it: after a session that timed out, the server's state comes
//!   in before the link counts as up, so an end seen without a link is still
//!   an end. The second starts once the link, Thetis and the server's "off"
//!   are all there, every PA it puts back has been reported in this session
//!   (a lost status comes again with the server's resend, seconds later), and
//!   nothing transmits: the server's PTT flag or our own transmission, not a
//!   MOX in Thetis itself, which the client is not told. Each of those gone
//!   when the second is over starts it again. One that cannot go because
//!   Thetis went off (as said by the session) forgets it, and a PA reported
//!   as not connected when it goes is skipped and forgotten: the PA stays in
//!   standby, the safe side, and the client log says why. The log also says
//!   when a put-back starts waiting, and when a press replaces one. The
//!   put-back has no upper bound in time, unlike the start, and lives as long
//!   as the app: it reads the server and the PAs again when it goes, so a
//!   late one is late, not blind.

use std::time::{Duration, Instant};

/// How long a press of our own outweighs what the server says.
pub const OWN_PRESS_HOLD: Duration = Duration::from_millis(1000);
/// From the PA to standby to the TUNE.
pub const PA_STANDBY_WAIT: Duration = Duration::from_millis(500);
/// A start older than this is not sent any more.
pub const PA_STANDBY_GIVE_UP: Duration = Duration::from_millis(2000);
/// From the end of a TUNE to the PA back in operate (Thetis back to RX).
pub const PA_RESTORE_WAIT: Duration = Duration::from_millis(1000);

/// The PAs as the client sees them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pa {
    pub rf2k_operate: bool,
    pub spe_operate: bool,
    pub rf2k_connected: bool,
    pub spe_connected: bool,
    /// The server has reported this PA in this session. A session that ended
    /// cleared `*_connected` too, so without this "not connected" could not
    /// be told from "not said yet".
    pub rf2k_known: bool,
    pub spe_known: bool,
}

impl Pa {
    pub fn of(s: &crate::state::RadioState) -> Self {
        Pa {
            rf2k_operate: s.rf2k_operate,
            spe_operate: s.spe_state == 2,
            rf2k_connected: s.rf2k_connected,
            spe_connected: s.spe_connected,
            rf2k_known: s.rf2k_available,
            spe_known: s.spe_available,
        }
    }
}

/// What the server said, and whether a command can reach Thetis now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Server {
    pub tune: bool,
    pub known: bool,
    pub connected: bool,
    pub power_on: bool,
    /// A transmitter is on: the server's PTT flag, or our own transmission
    /// before the server has said so. Not a MOX or footswitch in Thetis itself:
    /// that state does not travel to the client.
    pub tx: bool,
}

impl Server {
    pub fn of(s: &crate::state::RadioState) -> Self {
        Server {
            tune: s.thetis_tune,
            known: s.thetis_tune_known,
            connected: s.connected,
            power_on: s.power_on,
            tx: s.server_tx || s.tx_window_open,
        }
    }
    fn ready(&self) -> bool {
        self.connected && self.power_on
    }
}

/// What the caller sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Tune(bool),
    Rf2kOperate(bool),
    /// The SPE Expert has one command, a toggle.
    SpeToggle,
}

/// Where the put-back of the PAs stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Restore {
    #[default]
    None,
    /// Our TUNE ended; the PAs go back once the link, Thetis and the server's
    /// "off" are all there.
    Owed,
    /// All there since this moment: the PAs go back [`PA_RESTORE_WAIT`] after it.
    Due(Instant),
}

#[derive(Debug, Default)]
pub struct TuneButton {
    shown: bool,
    pressed_at: Option<Instant>,
    /// The server said "on" for the TUNE the button shows.
    confirmed: bool,
    /// The PAs our last "on" put in standby, to put back.
    rf2k_bypassed: bool,
    spe_bypassed: bool,
    pending_on: Option<Instant>,
    restore: Restore,
}

impl TuneButton {
    /// "TUNE ON".
    pub fn shown(&self) -> bool {
        self.shown
    }

    /// The PA goes to standby before the TUNE ("PA STBY...").
    pub fn waiting(&self) -> bool {
        self.pending_on.is_some()
    }

    /// Not while the PA goes to standby or back: a TUNE then would switch the
    /// PA under the carrier.
    pub fn can_press(&self, s: Server) -> bool {
        s.ready() && self.pending_on.is_none() && !matches!(self.restore, Restore::Due(_))
    }

    fn bypassed(&self) -> bool {
        self.rf2k_bypassed || self.spe_bypassed
    }

    /// Every PA this press put in standby has been reported in this session,
    /// connected or not: until then "not connected" means "not said yet".
    fn reported(&self, pa: Pa) -> bool {
        (!self.rf2k_bypassed || pa.rf2k_known) && (!self.spe_bypassed || pa.spe_known)
    }

    fn forget_bypass(&mut self) {
        self.rf2k_bypassed = false;
        self.spe_bypassed = false;
        self.restore = Restore::None;
    }

    /// A PA put in standby for the TUNE stays there: said in the log, since
    /// the operator sees only a PA that did not come back.
    fn give_up(&mut self, why: &str) {
        if self.bypassed() {
            log::info!("TUNE button: the PA put in standby for the TUNE stays in standby: {why}");
        }
        self.forget_bypass();
    }

    /// One press of the button (or the MIDI key).
    pub fn press(&mut self, now: Instant, s: Server, pa: Pa) -> Vec<Action> {
        let mut out = Vec::new();
        if !self.can_press(s) {
            return out;
        }
        self.confirmed = false;
        if !self.shown {
            // A put-back still owed is replaced by this press.
            if self.restore == Restore::Owed && self.bypassed() {
                log::info!("TUNE button: a new press replaces the PA put-back that was waiting");
            }
            self.restore = Restore::None;
            self.rf2k_bypassed = pa.rf2k_operate;
            self.spe_bypassed = pa.spe_operate;
            if self.bypassed() {
                if self.rf2k_bypassed {
                    out.push(Action::Rf2kOperate(false));
                }
                if self.spe_bypassed {
                    out.push(Action::SpeToggle);
                }
                self.pending_on = Some(now);
            } else {
                out.push(Action::Tune(true));
                self.shown = true;
                self.pressed_at = Some(now);
            }
        } else {
            out.push(Action::Tune(false));
            self.shown = false;
            self.pressed_at = Some(now);
            if self.bypassed() {
                self.restore = Restore::Due(now);
            }
        }
        out
    }

    /// Every frame (desktop) or every poll (Android).
    pub fn tick(&mut self, now: Instant, s: Server, pa: Pa) -> Vec<Action> {
        let mut out = Vec::new();
        if let Some(t) = self.pending_on {
            let waited = now.saturating_duration_since(t);
            if !s.ready() || waited > PA_STANDBY_GIVE_UP {
                // The link or Thetis went before the TUNE, or the process was
                // frozen: none goes, and the PA stays in standby.
                self.pending_on = None;
                self.give_up("the TUNE did not start");
            } else if waited >= PA_STANDBY_WAIT {
                self.pending_on = None;
                if s.known && s.tune {
                    // A TUNE of someone else began in our 500 ms and still
                    // runs: ours is not sent, and the PA stays in standby.
                    // Sent, it would take that TUNE's "on" as its own.
                    self.give_up("another TUNE ran when ours would start");
                } else {
                    out.push(Action::Tune(true));
                    self.shown = true;
                    self.pressed_at = Some(now);
                }
            }
        }
        if self.pending_on.is_none() {
            if s.known && s.tune {
                self.confirmed = true;
            }
            let holding = self.pressed_at.is_some_and(|t| now.saturating_duration_since(t) <= OWN_PRESS_HOLD);
            if !holding && s.known {
                self.pressed_at = None;
                let ended = self.shown && self.confirmed && !s.tune;
                self.shown = s.tune;
                if ended {
                    self.confirmed = false;
                    if self.bypassed() && self.restore == Restore::None {
                        log::info!(
                            "TUNE button: the TUNE ended; the PA goes back to operate once the link, Thetis, \
                             the server's \"off\" and the PA's status are there and nothing transmits"
                        );
                        self.restore = Restore::Owed;
                    }
                }
            }
        }
        if self.restore == Restore::Owed {
            if s.connected && s.known && !s.power_on {
                self.give_up("Thetis is off");
            } else if s.ready() && s.known && !s.tune && !s.tx && self.reported(pa) {
                self.restore = Restore::Due(now);
            }
            // Otherwise the link is not back yet, a TUNE or a transmission
            // runs, or a PA has not been reported in this session: wait.
        }
        if let Restore::Due(t) = self.restore {
            if now.saturating_duration_since(t) >= PA_RESTORE_WAIT {
                if !s.connected || !s.known || s.tune || s.tx || !self.reported(pa) {
                    // The link went in that second, a new session has not said
                    // yet whether Thetis tunes or what the PA is, or a TUNE or
                    // a transmission runs (a lost "off", another client, the
                    // operator on the air): again once all is there.
                    self.restore = Restore::Owed;
                } else if !s.power_on {
                    self.give_up("Thetis went off before the PA went back");
                } else {
                    if self.rf2k_bypassed && pa.rf2k_connected {
                        out.push(Action::Rf2kOperate(true));
                    } else if self.rf2k_bypassed {
                        log::info!("TUNE button: the RF2K-S is not connected, so it is not put back in operate");
                    }
                    if self.spe_bypassed && pa.spe_connected && !pa.spe_operate {
                        out.push(Action::SpeToggle);
                    } else if self.spe_bypassed && !pa.spe_connected {
                        log::info!("TUNE button: the SPE is not connected, so it is not put back in operate");
                    }
                    self.forget_bypass();
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }
    fn up(tune: bool) -> Server {
        Server { tune, known: true, connected: true, power_on: true, tx: false }
    }
    // A station without an SPE: it is never reported (2.13.3 build 12).
    const RF2K_ON: Pa = Pa { rf2k_operate: true, spe_operate: false, rf2k_connected: true, spe_connected: false, rf2k_known: true, spe_known: false };
    const RF2K_STBY: Pa = Pa { rf2k_operate: false, spe_operate: false, rf2k_connected: true, spe_connected: false, rf2k_known: true, spe_known: false };

    /// Press with the RF2K-S in operate, until the carrier is on and the server
    /// said so. Returns the time of the confirmation.
    fn tune_on_with_the_pa(b: &mut TuneButton, t0: Instant) -> Instant {
        assert_eq!(b.press(t0, up(false), RF2K_ON), vec![Action::Rf2kOperate(false)]);
        assert!(b.waiting() && !b.shown());
        assert!(b.tick(t0 + ms(499), up(false), RF2K_STBY).is_empty(), "the PA has half a second");
        assert_eq!(b.tick(t0 + ms(500), up(false), RF2K_STBY), vec![Action::Tune(true)]);
        assert!(b.shown() && !b.waiting());
        let t = t0 + ms(600);
        assert!(b.tick(t, up(true), RF2K_STBY).is_empty());
        t
    }

    #[test]
    fn a_tune_ended_elsewhere_puts_the_pa_back_once_and_twice() {
        let mut b = TuneButton::default();
        let mut t0 = Instant::now();
        for _ in 0..2 {
            let t = tune_on_with_the_pa(&mut b, t0);
            // The server's safety ends it.
            let end = t + ms(5000);
            assert!(b.tick(end, up(false), RF2K_STBY).is_empty());
            assert!(!b.shown());
            assert!(!b.can_press(up(false)), "closed while the PA goes back");
            assert!(b.tick(end + ms(999), up(false), RF2K_STBY).is_empty());
            assert_eq!(b.tick(end + ms(1000), up(false), RF2K_STBY), vec![Action::Rf2kOperate(true)]);
            assert!(b.tick(end + ms(3000), up(false), RF2K_ON).is_empty(), "once");
            // A TUNE of someone else after it, ending: the memory went with the
            // put-back, so nothing is switched again.
            assert!(b.tick(end + ms(4000), up(true), RF2K_ON).is_empty());
            assert!(b.tick(end + ms(6000), up(false), RF2K_ON).is_empty());
            assert!(b.tick(end + ms(7000), up(false), RF2K_ON).is_empty(), "once, also after a later end");
            t0 = end + ms(10_000);
        }
    }

    #[test]
    fn a_new_tune_needs_its_own_confirmation() {
        // On, confirmed, our own off; then on again, and that "on" from the
        // server is lost: the old confirmation does not count for it.
        let mut b = TuneButton::default();
        let t = tune_on_with_the_pa(&mut b, Instant::now());
        b.press(t + ms(1000), up(true), RF2K_STBY);
        b.tick(t + ms(2000), up(false), RF2K_STBY);
        let t2 = t + ms(5000);
        assert_eq!(b.press(t2, up(false), RF2K_ON), vec![Action::Rf2kOperate(false)]);
        assert_eq!(b.tick(t2 + ms(500), up(false), RF2K_STBY), vec![Action::Tune(true)]);
        assert!(b.tick(t2 + ms(1600), up(false), RF2K_STBY).is_empty());
        assert!(b.tick(t2 + ms(4000), up(false), RF2K_STBY).is_empty(), "no PA under a carrier the server never confirmed");
    }

    #[test]
    fn a_tune_of_someone_else_still_on_when_our_start_would_go_stops_ours() {
        // Their TUNE begins in our 500 ms and is still reported on when ours
        // would go. Sent, ours would take their "on" as its own; with ours
        // landing after their "off" and its own "on" lost, the PA would go
        // back under our carrier.
        for last_on in [500u64, 533] {
            let mut b = TuneButton::default();
            let t0 = Instant::now();
            b.press(t0, up(false), RF2K_ON);
            assert!(b.tick(t0 + ms(300), up(true), RF2K_STBY).is_empty());
            assert!(b.tick(t0 + ms(500), up(true), RF2K_STBY).is_empty(), "no TUNE of ours");
            assert!(!b.waiting() && b.shown(), "the button shows their TUNE");
            if last_on > 500 {
                assert!(b.tick(t0 + ms(last_on), up(true), RF2K_STBY).is_empty());
            }
            for at in [600, 1600, 2700, 6000] {
                assert!(b.tick(t0 + ms(at), up(false), RF2K_STBY).is_empty(), "the PA stays in standby at +{at} ms");
            }
            assert!(!b.shown());
        }
    }

    #[test]
    fn a_lost_on_from_the_server_is_no_end_and_the_pa_stays_in_standby() {
        // The press reached the server, its "on" did not reach us: the server
        // still reads "off" to us while the carrier is there.
        let mut b = TuneButton::default();
        let t0 = Instant::now();
        b.press(t0, up(false), RF2K_ON);
        assert_eq!(b.tick(t0 + ms(500), up(false), RF2K_STBY), vec![Action::Tune(true)]);
        assert!(b.tick(t0 + ms(1400), up(false), RF2K_STBY).is_empty(), "the press holds a second");
        assert!(b.tick(t0 + ms(1600), up(false), RF2K_STBY).is_empty());
        assert!(!b.shown(), "the button shows the server");
        assert!(b.tick(t0 + ms(5000), up(false), RF2K_STBY).is_empty(), "and the PA is never switched under the carrier");
    }

    #[test]
    fn a_tune_elsewhere_while_our_pa_goes_to_standby_is_not_ours() {
        // Someone else tunes on and off inside our 500 ms.
        // It must not end our TUNE, nor confirm it.
        let mut b = TuneButton::default();
        let t0 = Instant::now();
        b.press(t0, up(false), RF2K_ON);
        assert!(b.tick(t0 + ms(100), up(true), RF2K_STBY).is_empty());
        assert!(b.tick(t0 + ms(200), up(false), RF2K_STBY).is_empty());
        assert!(b.waiting() && !b.shown(), "still our own start, not their TUNE");
        assert_eq!(b.tick(t0 + ms(500), up(false), RF2K_STBY), vec![Action::Tune(true)]);
        // Our own "on" from the server is lost: nothing may go back under the carrier.
        for at in [1200, 1600, 3000, 6000] {
            assert!(b.tick(t0 + ms(at), up(false), RF2K_STBY).is_empty(), "no PA under our carrier at +{at} ms");
        }

        // The same, but our "on" arrives: the end of our own TUNE puts the PA back.
        let mut c = TuneButton::default();
        c.press(t0, up(false), RF2K_ON);
        c.tick(t0 + ms(100), up(true), RF2K_STBY);
        c.tick(t0 + ms(200), up(false), RF2K_STBY);
        c.tick(t0 + ms(500), up(false), RF2K_STBY);
        assert!(c.tick(t0 + ms(1200), up(true), RF2K_STBY).is_empty());
        assert_eq!(c.press(t0 + ms(5000), up(true), RF2K_STBY), vec![Action::Tune(false)]);
        assert_eq!(c.tick(t0 + ms(6000), up(false), RF2K_STBY), vec![Action::Rf2kOperate(true)]);
    }

    #[test]
    fn thetis_that_went_away_does_not_put_the_pa_back() {
        // TCI gone: the server says TUNE off and power off, and stock Thetis
        // keeps the carrier.
        let mut b = TuneButton::default();
        let t = tune_on_with_the_pa(&mut b, Instant::now());
        let gone = Server { tune: false, known: true, connected: true, power_on: false, tx: false };
        assert!(b.tick(t + ms(2000), gone, RF2K_STBY).is_empty());
        assert!(b.tick(t + ms(3000), up(false), RF2K_STBY).is_empty(), "nor when Thetis is back within the second");
        assert!(b.tick(t + ms(6000), up(false), RF2K_STBY).is_empty(), "nor later");
        // And the memory is gone: a TUNE of someone else, later, ending, does
        // not switch a PA that has long been where the operator left it.
        assert!(b.tick(t + ms(8000), up(true), RF2K_STBY).is_empty());
        assert!(b.tick(t + ms(9000), up(false), RF2K_STBY).is_empty());
        assert!(b.tick(t + ms(11_000), up(false), RF2K_STBY).is_empty());
    }

    #[test]
    fn thetis_going_in_the_second_before_the_pa_goes_back_stops_it() {
        let mut b = TuneButton::default();
        let t = tune_on_with_the_pa(&mut b, Instant::now());
        b.tick(t + ms(2000), up(false), RF2K_STBY);
        let gone = Server { tune: false, known: true, connected: true, power_on: false, tx: false };
        assert!(b.tick(t + ms(3000), gone, RF2K_STBY).is_empty());
        assert!(b.tick(t + ms(4000), up(false), RF2K_STBY).is_empty());
        // The memory is gone too: a later TUNE of someone else, ending, does
        // not switch the PA.
        assert!(b.tick(t + ms(6000), up(true), RF2K_STBY).is_empty());
        assert!(b.tick(t + ms(8000), up(false), RF2K_STBY).is_empty());
        assert!(b.tick(t + ms(9000), up(false), RF2K_STBY).is_empty());
    }

    #[test]
    fn a_second_end_while_the_pa_goes_back_sends_it_back_once() {
        let mut b = TuneButton::default();
        let t = tune_on_with_the_pa(&mut b, Instant::now());
        let end = t + ms(2000);
        assert!(b.tick(end, up(false), RF2K_STBY).is_empty());
        // Another client tunes on and off inside that second.
        assert!(b.tick(end + ms(200), up(true), RF2K_STBY).is_empty());
        assert!(b.tick(end + ms(500), up(false), RF2K_STBY).is_empty());
        assert_eq!(b.tick(end + ms(1000), up(false), RF2K_STBY), vec![Action::Rf2kOperate(true)]);
        assert!(b.tick(end + ms(1500), up(false), RF2K_ON).is_empty());
        assert!(b.tick(end + ms(3000), up(false), RF2K_ON).is_empty(), "once");
    }

    #[test]
    fn another_tune_in_the_second_before_the_pa_goes_back_holds_it_until_that_one_ends() {
        let mut b = TuneButton::default();
        let t = tune_on_with_the_pa(&mut b, Instant::now());
        b.tick(t + ms(2000), up(false), RF2K_STBY);
        assert!(b.tick(t + ms(3000), up(true), RF2K_STBY).is_empty(), "another client tunes: no PA under that carrier");
        assert!(b.shown(), "the button shows their TUNE");
        assert!(b.tick(t + ms(4000), up(true), RF2K_STBY).is_empty());
        assert!(b.tick(t + ms(4500), up(false), RF2K_STBY).is_empty(), "their TUNE ends");
        assert!(b.tick(t + ms(5499), up(false), RF2K_STBY).is_empty(), "the second counts from their end");
        assert_eq!(b.tick(t + ms(5500), up(false), RF2K_STBY), vec![Action::Rf2kOperate(true)], "and then ours goes back");
    }

    #[test]
    fn a_lost_off_from_the_server_puts_the_pa_back_after_the_resend() {
        // Our own "off" reached the server, its "off" did not reach us: when
        // the PA would go back the server still reads "on". The resend (every
        // 10 s) brings the end, and then the PA goes back.
        let mut b = TuneButton::default();
        let t = tune_on_with_the_pa(&mut b, Instant::now());
        let off = t + ms(3000);
        assert_eq!(b.press(off, up(true), RF2K_STBY), vec![Action::Tune(false)]);
        assert!(b.tick(off + ms(1000), up(true), RF2K_STBY).is_empty(), "not while the server says TUNE");
        assert!(b.tick(off + ms(2000), up(true), RF2K_STBY).is_empty());
        let resend = off + ms(11_000);
        assert!(b.tick(resend, up(false), RF2K_STBY).is_empty());
        assert_eq!(b.tick(resend + ms(1000), up(false), RF2K_STBY), vec![Action::Rf2kOperate(true)]);
    }

    #[test]
    fn a_session_lost_then_the_server_saying_off_is_an_end() {
        // The field case: the link drops, the safety ends the TUNE, and the
        // client hears it on the next session.
        let mut b = TuneButton::default();
        let t = tune_on_with_the_pa(&mut b, Instant::now());
        let lost = Server { tune: false, known: false, connected: false, power_on: true, tx: false };
        assert!(b.tick(t + ms(1000), lost, RF2K_STBY).is_empty());
        assert!(b.shown(), "an unknown state is no end");
        let end = t + ms(20_000);
        assert!(b.tick(end, up(false), RF2K_STBY).is_empty());
        assert_eq!(b.tick(end + ms(1000), up(false), RF2K_STBY), vec![Action::Rf2kOperate(true)]);
    }

    #[test]
    fn the_state_of_a_new_session_before_the_link_counts_as_up_puts_the_pa_back_once_and_twice() {
        // The field order after a session that timed out: the server's "off"
        // comes in some 400 ms before the first heartbeat answer marks the link
        // up, and the PA status a little later still.
        let mut b = TuneButton::default();
        let mut t0 = Instant::now();
        let lost = Server { tune: false, known: false, connected: false, power_on: true, tx: false };
        let state_first = Server { tune: false, known: true, connected: false, power_on: true, tx: false };
        for pass in 0..2 {
            let t = tune_on_with_the_pa(&mut b, t0);
            assert!(b.tick(t + ms(2000), lost, Pa::default()).is_empty());
            let back = t + ms(20_000);
            assert!(b.tick(back, state_first, Pa::default()).is_empty(), "pass {pass}: no link yet");
            assert!(!b.shown());
            assert!(b.tick(back + ms(400), up(false), Pa::default()).is_empty(), "pass {pass}: link up, PA not reported");
            assert!(b.tick(back + ms(600), up(false), RF2K_STBY).is_empty(), "pass {pass}: the second starts");
            assert!(b.tick(back + ms(1599), up(false), RF2K_STBY).is_empty());
            assert_eq!(
                b.tick(back + ms(1600), up(false), RF2K_STBY),
                vec![Action::Rf2kOperate(true)],
                "pass {pass}: the PA goes back a second after its status is in"
            );
            assert!(b.tick(back + ms(3000), up(false), RF2K_ON).is_empty(), "pass {pass}: once");
            t0 = back + ms(10_000);
        }
    }

    #[test]
    fn a_pa_reported_after_the_second_still_goes_back() {
        // The first PA status of the new session was lost: the next copy comes
        // with the server's resend, seconds later. Not reported is not absent.
        let mut b = TuneButton::default();
        let t = tune_on_with_the_pa(&mut b, Instant::now());
        let lost = Server { tune: false, known: false, connected: false, power_on: true, tx: false };
        let state_first = Server { tune: false, known: true, connected: false, power_on: true, tx: false };
        assert!(b.tick(t + ms(2000), lost, Pa::default()).is_empty());
        let back = t + ms(20_000);
        assert!(b.tick(back, state_first, Pa::default()).is_empty());
        for at in [400, 1400, 2500] {
            assert!(b.tick(back + ms(at), up(false), Pa::default()).is_empty(), "+{at} ms: the PA not reported yet");
        }
        assert!(b.tick(back + ms(3000), up(false), RF2K_STBY).is_empty(), "the second starts with the report");
        assert!(b.tick(back + ms(3999), up(false), RF2K_STBY).is_empty());
        assert_eq!(b.tick(back + ms(4000), up(false), RF2K_STBY), vec![Action::Rf2kOperate(true)]);
    }

    #[test]
    fn a_transmission_holds_the_put_back_until_it_ends_once_and_twice() {
        // The operator is back on the air before the PA went back: not under
        // that carrier, and the second counts from its end.
        let mut b = TuneButton::default();
        let mut t0 = Instant::now();
        let on_air = Server { tx: true, ..up(false) };
        for pass in 0..2 {
            let t = tune_on_with_the_pa(&mut b, t0);
            let end = t + ms(2000);
            assert!(b.tick(end, up(false), RF2K_STBY).is_empty());
            assert!(b.tick(end + ms(500), on_air, RF2K_STBY).is_empty());
            for at in [1000, 3000, 8000, 8500] {
                assert!(b.tick(end + ms(at), on_air, RF2K_STBY).is_empty(), "pass {pass}: no PA under the carrier at +{at} ms");
            }
            assert!(b.tick(end + ms(9000), up(false), RF2K_STBY).is_empty(), "pass {pass}: the second starts at the end");
            assert!(b.tick(end + ms(9500), up(false), RF2K_STBY).is_empty());
            assert!(b.tick(end + ms(9999), up(false), RF2K_STBY).is_empty());
            assert_eq!(b.tick(end + ms(10_000), up(false), RF2K_STBY), vec![Action::Rf2kOperate(true)], "pass {pass}");
            t0 = end + ms(20_000);
        }
    }

    #[test]
    fn a_link_that_goes_in_the_second_before_the_pa_goes_back_is_waited_for() {
        let mut b = TuneButton::default();
        let t = tune_on_with_the_pa(&mut b, Instant::now());
        let end = t + ms(2000);
        assert!(b.tick(end, up(false), RF2K_STBY).is_empty());
        // Without the link the power flag says nothing: off here is not
        // "Thetis is off".
        let lost = Server { tune: false, known: false, connected: false, power_on: false, tx: false };
        assert!(b.tick(end + ms(500), lost, Pa::default()).is_empty());
        // The new session's state already in, the link not yet up.
        let state_first = Server { tune: false, known: true, connected: false, power_on: false, tx: false };
        assert!(b.tick(end + ms(1000), state_first, RF2K_STBY).is_empty(), "not without the link");
        assert!(b.tick(end + ms(1200), state_first, RF2K_STBY).is_empty(), "and no give-up without it");
        assert!(b.tick(end + ms(5000), lost, Pa::default()).is_empty());
        let back = end + ms(30_000);
        assert!(b.tick(back, up(false), RF2K_STBY).is_empty());
        assert_eq!(b.tick(back + ms(1000), up(false), RF2K_STBY), vec![Action::Rf2kOperate(true)]);
    }

    #[test]
    fn a_new_session_that_has_not_said_whether_thetis_tunes_holds_the_pa() {
        // Link up, but the TUNE state of the new session not in yet: a carrier
        // there is as likely as not.
        let mut b = TuneButton::default();
        let t = tune_on_with_the_pa(&mut b, Instant::now());
        let end = t + ms(2000);
        assert!(b.tick(end, up(false), RF2K_STBY).is_empty());
        let unsaid = Server { tune: false, known: false, connected: true, power_on: true, tx: false };
        assert!(b.tick(end + ms(1000), unsaid, RF2K_STBY).is_empty(), "not while unknown");
        // The power flag of a session that has not said its state says nothing
        // either: not "Thetis is off".
        let unsaid_off = Server { power_on: false, ..unsaid };
        assert!(b.tick(end + ms(1100), unsaid_off, RF2K_STBY).is_empty());
        // The second counts from the "off", not from the link.
        assert!(b.tick(end + ms(1500), up(false), RF2K_STBY).is_empty());
        assert!(b.tick(end + ms(2499), up(false), RF2K_STBY).is_empty());
        assert_eq!(b.tick(end + ms(2500), up(false), RF2K_STBY), vec![Action::Rf2kOperate(true)]);
    }

    #[test]
    fn a_press_while_a_put_back_is_owed_replaces_it() {
        // The end came in before the link was up, and the link went again; on
        // the next session the put-back waits for it to say whether Thetis
        // tunes. Meanwhile the operator puts the PA in operate and tunes again,
        // and the server's "on" of that TUNE is lost. The old put-back must
        // not go under the new carrier.
        let mut b = TuneButton::default();
        let t = tune_on_with_the_pa(&mut b, Instant::now());
        let lost = Server { tune: false, known: false, connected: false, power_on: true, tx: false };
        let state_first = Server { tune: false, known: true, connected: false, power_on: true, tx: false };
        assert!(b.tick(t + ms(2000), lost, Pa::default()).is_empty());
        assert!(b.tick(t + ms(20_000), state_first, Pa::default()).is_empty());
        assert!(!b.shown(), "the end is in");
        assert!(b.tick(t + ms(20_200), lost, Pa::default()).is_empty());
        let unsaid = Server { tune: false, known: false, connected: true, power_on: true, tx: false };
        let back = t + ms(40_000);
        assert!(b.tick(back, unsaid, RF2K_ON).is_empty());
        assert_eq!(b.press(back + ms(100), unsaid, RF2K_ON), vec![Action::Rf2kOperate(false)]);
        assert_eq!(b.tick(back + ms(600), up(false), RF2K_STBY), vec![Action::Tune(true)]);
        for at in [700, 1600, 2700, 6000] {
            assert!(b.tick(back + ms(at), up(false), RF2K_STBY).is_empty(), "no PA under the carrier at +{at} ms");
        }
    }

    #[test]
    fn our_own_off_puts_the_pa_back_once() {
        let mut b = TuneButton::default();
        let t = tune_on_with_the_pa(&mut b, Instant::now());
        let off = t + ms(3000);
        assert_eq!(b.press(off, up(true), RF2K_STBY), vec![Action::Tune(false)]);
        assert!(b.tick(off + ms(500), up(false), RF2K_STBY).is_empty());
        assert_eq!(b.tick(off + ms(1000), up(false), RF2K_STBY), vec![Action::Rf2kOperate(true)]);
        assert!(b.tick(off + ms(2000), up(false), RF2K_ON).is_empty(), "the server agreeing is no second end");
    }

    #[test]
    fn the_spe_is_put_in_standby_and_back_with_its_toggle() {
        // The RF2K-S is connected but in standby: it was not switched, so it
        // does not go to operate on the way back.
        let spe_on = Pa { rf2k_operate: false, spe_operate: true, rf2k_connected: true, spe_connected: true, rf2k_known: true, spe_known: true };
        let spe_stby = Pa { spe_operate: false, ..spe_on };
        let mut b = TuneButton::default();
        let t0 = Instant::now();
        assert_eq!(b.press(t0, up(false), spe_on), vec![Action::SpeToggle]);
        assert_eq!(b.tick(t0 + ms(500), up(false), spe_stby), vec![Action::Tune(true)]);
        b.tick(t0 + ms(600), up(true), spe_stby);
        assert_eq!(b.press(t0 + ms(3000), up(true), spe_stby), vec![Action::Tune(false)]);
        assert_eq!(b.tick(t0 + ms(4000), up(false), spe_stby), vec![Action::SpeToggle]);
    }

    #[test]
    fn an_spe_the_operator_put_back_in_operate_stays_there() {
        // SPE in operate, TUNE, the link drops and the safety ends the TUNE;
        // meanwhile the operator puts the SPE in operate again. Its one command
        // is a toggle: sent now, it would put the SPE in standby.
        let spe_on = Pa { rf2k_operate: false, spe_operate: true, rf2k_connected: false, spe_connected: true, rf2k_known: true, spe_known: true };
        let spe_stby = Pa { spe_operate: false, ..spe_on };
        let mut b = TuneButton::default();
        let t0 = Instant::now();
        b.press(t0, up(false), spe_on);
        b.tick(t0 + ms(500), up(false), spe_stby);
        b.tick(t0 + ms(600), up(true), spe_stby);
        let lost = Server { tune: false, known: false, connected: false, power_on: true, tx: false };
        assert!(b.tick(t0 + ms(2000), lost, spe_stby).is_empty());
        let back = t0 + ms(40_000);
        assert!(b.tick(back, up(false), spe_on).is_empty());
        assert!(b.tick(back + ms(1000), up(false), spe_on).is_empty(), "no toggle on an SPE in operate");
    }

    #[test]
    fn an_spe_no_longer_connected_is_not_sent_back() {
        let spe_on = Pa { rf2k_operate: false, spe_operate: true, rf2k_connected: false, spe_connected: true, rf2k_known: true, spe_known: true };
        let spe_stby = Pa { spe_operate: false, ..spe_on };
        let mut b = TuneButton::default();
        let t0 = Instant::now();
        b.press(t0, up(false), spe_on);
        b.tick(t0 + ms(500), up(false), spe_stby);
        b.tick(t0 + ms(600), up(true), spe_stby);
        b.press(t0 + ms(3000), up(true), spe_stby);
        let gone = Pa { spe_connected: false, ..spe_stby };
        assert!(b.tick(t0 + ms(4000), up(false), gone).is_empty());
    }

    #[test]
    fn two_pas_in_operate_both_go_to_standby_and_both_go_back() {
        let both_on = Pa { rf2k_operate: true, spe_operate: true, rf2k_connected: true, spe_connected: true, rf2k_known: true, spe_known: true };
        let both_stby = Pa { rf2k_operate: false, spe_operate: false, ..both_on };
        let mut b = TuneButton::default();
        let t0 = Instant::now();
        assert_eq!(b.press(t0, up(false), both_on), vec![Action::Rf2kOperate(false), Action::SpeToggle]);
        assert_eq!(b.tick(t0 + ms(500), up(false), both_stby), vec![Action::Tune(true)]);
        b.tick(t0 + ms(600), up(true), both_stby);
        assert_eq!(b.press(t0 + ms(3000), up(true), both_stby), vec![Action::Tune(false)]);
        assert_eq!(b.tick(t0 + ms(4000), up(false), both_stby), vec![Action::Rf2kOperate(true), Action::SpeToggle]);
    }

    #[test]
    fn only_the_pa_the_press_put_in_standby_goes_back() {
        // The RF2K-S in operate, the SPE connected in standby: the SPE is not
        // touched, on the way in nor on the way back (its command is a toggle,
        // and would switch it ON). And no "active PA" is needed: with none
        // marked, the default, the RF2K-S still goes back.
        let both = Pa { rf2k_operate: true, spe_operate: false, rf2k_connected: true, spe_connected: true, rf2k_known: true, spe_known: true };
        let both_stby = Pa { rf2k_operate: false, ..both };
        let mut b = TuneButton::default();
        let t0 = Instant::now();
        assert_eq!(b.press(t0, up(false), both), vec![Action::Rf2kOperate(false)]);
        b.tick(t0 + ms(500), up(false), both_stby);
        b.tick(t0 + ms(600), up(true), both_stby);
        b.press(t0 + ms(3000), up(true), both_stby);
        assert_eq!(b.tick(t0 + ms(4000), up(false), both_stby), vec![Action::Rf2kOperate(true)]);
    }

    #[test]
    fn a_pa_no_longer_connected_is_not_sent_back() {
        let mut b = TuneButton::default();
        let t = tune_on_with_the_pa(&mut b, Instant::now());
        b.press(t + ms(3000), up(true), RF2K_STBY);
        let gone = Pa { rf2k_connected: false, ..RF2K_STBY };
        assert!(b.tick(t + ms(4000), up(false), gone).is_empty());
    }

    #[test]
    fn without_a_pa_in_operate_the_tune_goes_at_once_and_nothing_goes_back() {
        let mut b = TuneButton::default();
        let t0 = Instant::now();
        assert_eq!(b.press(t0, up(false), RF2K_STBY), vec![Action::Tune(true)]);
        b.tick(t0 + ms(100), up(true), RF2K_STBY);
        assert!(b.tick(t0 + ms(3000), up(false), RF2K_STBY).is_empty());
        assert!(b.tick(t0 + ms(5000), up(false), RF2K_STBY).is_empty());
    }

    #[test]
    fn a_press_holds_a_second_then_the_server_counts_again() {
        let mut b = TuneButton::default();
        let t0 = Instant::now();
        b.press(t0, up(false), Pa::default());
        b.tick(t0 + OWN_PRESS_HOLD, up(false), Pa::default());
        assert!(b.shown(), "the hold includes its last millisecond");
        b.tick(t0 + OWN_PRESS_HOLD + ms(1), up(false), Pa::default());
        assert!(!b.shown());
    }

    #[test]
    fn a_server_that_never_says_leaves_the_button_on_its_own_press() {
        let old = Server { tune: false, known: false, connected: true, power_on: true, tx: false };
        let mut b = TuneButton::default();
        let t0 = Instant::now();
        assert_eq!(b.press(t0, old, Pa::default()), vec![Action::Tune(true)]);
        b.tick(t0 + ms(300), old, Pa::default());
        b.tick(t0 + ms(5000), old, Pa::default());
        assert!(b.shown(), "it stays on, so the next press sends off");
        assert_eq!(b.press(t0 + ms(6000), old, Pa::default()), vec![Action::Tune(false)]);
    }

    #[test]
    fn the_button_shows_a_tune_of_someone_else() {
        let mut b = TuneButton::default();
        b.tick(Instant::now(), up(true), Pa::default());
        assert!(b.shown());
    }

    #[test]
    fn no_press_while_the_pa_switches_or_without_thetis() {
        let mut b = TuneButton::default();
        let t0 = Instant::now();
        b.press(t0, up(false), RF2K_ON);
        assert!(b.press(t0 + ms(100), up(false), RF2K_STBY).is_empty(), "not while the PA goes to standby");
        let mut c = TuneButton::default();
        let off = Server { tune: false, known: true, connected: true, power_on: false, tx: false };
        assert!(c.press(t0, off, RF2K_ON).is_empty(), "not with Thetis off");
    }

    #[test]
    fn a_link_that_goes_before_the_tune_sends_none() {
        let mut b = TuneButton::default();
        let t0 = Instant::now();
        b.press(t0, up(false), RF2K_ON);
        let lost = Server { tune: false, known: false, connected: false, power_on: true, tx: false };
        assert!(b.tick(t0 + ms(200), lost, RF2K_STBY).is_empty());
        assert!(b.tick(t0 + ms(600), up(false), RF2K_STBY).is_empty(), "no TUNE after the link is back");
        assert!(!b.waiting());
    }

    #[test]
    fn a_start_that_waited_too_long_is_dropped() {
        // The process froze during the 500 ms: a TUNE asked for long ago is not
        // sent on waking.
        let mut b = TuneButton::default();
        let t0 = Instant::now();
        b.press(t0, up(false), RF2K_ON);
        assert!(b.tick(t0 + PA_STANDBY_GIVE_UP + ms(1), up(false), RF2K_STBY).is_empty());
        assert!(!b.waiting() && !b.shown());
        assert!(b.tick(t0 + ms(5000), up(false), RF2K_STBY).is_empty(), "and nothing goes back either");
    }

    #[test]
    fn a_start_with_a_late_first_tick_still_goes_up_to_the_limit() {
        // A slow frame or poll: the first tick after the 500 ms comes later.
        for first in [ms(700), PA_STANDBY_GIVE_UP] {
            let mut b = TuneButton::default();
            let t0 = Instant::now();
            b.press(t0, up(false), RF2K_ON);
            assert_eq!(b.tick(t0 + first, up(false), RF2K_STBY), vec![Action::Tune(true)], "first tick at {first:?}");
        }
    }

    #[test]
    fn our_own_off_before_the_pa_is_reported_waits_for_it() {
        // A new session; the operator ends the TUNE before the first PA status
        // of that session is in.
        let mut b = TuneButton::default();
        let t = tune_on_with_the_pa(&mut b, Instant::now());
        let lost = Server { tune: false, known: false, connected: false, power_on: true, tx: false };
        assert!(b.tick(t + ms(2000), lost, Pa::default()).is_empty());
        let unsaid = Server { connected: true, ..lost };
        let off = t + ms(20_000);
        assert_eq!(b.press(off, unsaid, Pa::default()), vec![Action::Tune(false)]);
        assert!(b.tick(off + ms(1000), up(false), Pa::default()).is_empty(), "not reported: no give-up");
        assert!(b.tick(off + ms(2000), up(false), RF2K_STBY).is_empty());
        assert_eq!(b.tick(off + ms(3000), up(false), RF2K_STBY), vec![Action::Rf2kOperate(true)]);
    }

    #[test]
    fn an_spe_not_reported_yet_is_waited_for() {
        let spe_on = Pa { rf2k_operate: false, spe_operate: true, rf2k_connected: false, spe_connected: true, rf2k_known: true, spe_known: true };
        let spe_stby = Pa { spe_operate: false, ..spe_on };
        let mut b = TuneButton::default();
        let t0 = Instant::now();
        b.press(t0, up(false), spe_on);
        b.tick(t0 + ms(500), up(false), spe_stby);
        b.tick(t0 + ms(600), up(true), spe_stby);
        let lost = Server { tune: false, known: false, connected: false, power_on: true, tx: false };
        assert!(b.tick(t0 + ms(2000), lost, Pa::default()).is_empty());
        // The new session has reported the RF2K-S (absent), not the SPE.
        let rf2k_only = Pa { rf2k_known: true, ..Pa::default() };
        let back = t0 + ms(30_000);
        for at in [0, 1000, 2500] {
            assert!(b.tick(back + ms(at), up(false), rf2k_only).is_empty(), "+{at} ms: the SPE not reported yet");
        }
        assert!(b.tick(back + ms(3000), up(false), spe_stby).is_empty());
        assert_eq!(b.tick(back + ms(4000), up(false), spe_stby), vec![Action::SpeToggle]);
    }

    #[test]
    fn the_machine_reads_the_radio_state_field_by_field() {
        // Eleven booleans of one type, copied by hand: a swap would compile.
        let mut s = crate::state::RadioState::default();
        let read = |s: &crate::state::RadioState| (Pa::of(s), Server::of(s));
        assert_eq!(read(&s), (Pa::default(), Server::default()));
        s.rf2k_operate = true;
        assert_eq!(read(&s).0, Pa { rf2k_operate: true, ..Pa::default() });
        s.rf2k_operate = false;
        s.spe_state = 2;
        assert_eq!(read(&s).0, Pa { spe_operate: true, ..Pa::default() });
        s.spe_state = 1;
        assert_eq!(read(&s).0, Pa::default(), "SPE standby is not operate");
        s.rf2k_connected = true;
        assert_eq!(read(&s).0, Pa { rf2k_connected: true, ..Pa::default() });
        s.rf2k_connected = false;
        s.spe_connected = true;
        assert_eq!(read(&s).0, Pa { spe_connected: true, ..Pa::default() });
        s.spe_connected = false;
        s.rf2k_available = true;
        assert_eq!(read(&s).0, Pa { rf2k_known: true, ..Pa::default() });
        s.rf2k_available = false;
        s.spe_available = true;
        assert_eq!(read(&s).0, Pa { spe_known: true, ..Pa::default() });
        s.spe_available = false;
        s.thetis_tune = true;
        assert_eq!(read(&s).1, Server { tune: true, ..Server::default() });
        s.thetis_tune = false;
        s.thetis_tune_known = true;
        assert_eq!(read(&s).1, Server { known: true, ..Server::default() });
        s.thetis_tune_known = false;
        s.connected = true;
        assert_eq!(read(&s).1, Server { connected: true, ..Server::default() });
        s.connected = false;
        s.power_on = true;
        assert_eq!(read(&s).1, Server { power_on: true, ..Server::default() });
        s.power_on = false;
        s.server_tx = true;
        assert_eq!(read(&s).1, Server { tx: true, ..Server::default() });
        s.server_tx = false;
        s.tx_window_open = true;
        assert_eq!(read(&s).1, Server { tx: true, ..Server::default() }, "our own transmission before the server says so");
    }
}
