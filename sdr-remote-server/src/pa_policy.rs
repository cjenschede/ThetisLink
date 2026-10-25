// SPDX-License-Identifier: GPL-2.0-or-later
//! Who may amplify, and what the drive falls back to when nobody does.
//!
//! Two problems from the field on 2026-09-16, both about the one number a PA and
//! Thetis share - the drive level:
//!
//! 1. Shut everything down while a PA is in Operate and start again, and Thetis
//!    still carries the PA's drive while the PA is gone. The existing restore
//!    hangs on the Operate -> Standby transition, and that transition happened
//!    while the server was not running.
//! 2. With both PAs in Operate, both write that number. They write different
//!    values, so it flips between them for as long as both are on. A third
//!    writer does not settle that; one of the two has to stop.
//!
//! The rules live here as plain functions so they can be read and tested without
//! a PA on the desk. The caller owns the state and does the talking.

/// PA ids as they appear in `config.active_pa`: 1 = SPE Expert, 2 = RF2K-S.
pub const PA_SPE: u8 = 1;
pub const PA_RF2K: u8 = 2;

/// Drive to fall back to when there is no snapshot of what it was before the PA
/// took over. Configurable (`pa_default_drive`); this is the default default.
pub const DEFAULT_PA_DRIVE: u8 = 100;

/// What to do with a PA that is in Operate while it is not the active one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enforce {
    /// Leave it alone.
    Nothing,
    /// Put it in Standby now.
    Standby,
    /// It has to go to Standby, but not while there is RF going through it.
    WaitForRx,
}

/// May this PA be put into Operate at all?
///
/// The operator marks one PA as the active one, and that mark is a permission:
/// the other one has no business amplifying, and while it is in Operate it fights
/// over the drive. With **no** PA marked active TL has no opinion - it does not
/// know which one the operator means, and refusing both would be a surprise on a
/// station that never set the mark.
pub fn operate_allowed(active_pa: u8, pa_id: u8) -> bool {
    active_pa == 0 || active_pa == pa_id
}

/// Is a PA amplifying right now?
///
/// The two PAs mean different things by "no contact", and the difference is the
/// owner's, measured on his own station (2026-09-16):
///
/// - The **SPE** drops to Standby by itself when its CAT link goes away, and stays
///   there when the link comes back. No contact is therefore *not amplifying*.
/// - The **RF2K-S** keeps going: it stays in Operate, PTT still works, the drive
///   stays where it was, and nothing on it can be changed any more. No contact is
///   therefore *unknown* - unless the last thing we saw was Standby.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Amplifying {
    Yes,
    No,
    /// Out of reach and it may well be amplifying. Nothing that raises power may
    /// happen in this state.
    Unknown,
}

/// The SPE, which puts itself in Standby when it loses its link.
pub fn spe_amplifying(connected: bool, state: u8) -> Amplifying {
    match (connected, state) {
        (false, _) => Amplifying::No,
        (true, 2) => Amplifying::Yes,
        _ => Amplifying::No,
    }
}

/// What the server knows of the RF2K-S. Named fields since build 226 (review
/// of 224): by position, `reachable` and `reports_operate` swapped read an
/// RF2K-S out of reach in Standby as amplifying, and one in Operate as off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rf2kReading {
    /// The server has a fresh reading of it.
    pub reachable: bool,
    /// That reading says Operate.
    pub reports_operate: bool,
    /// What it was last seen doing, kept on disk.
    pub last_seen_operate: Option<bool>,
}

impl Rf2kReading {
    /// The RF2K-S, which carries on without its network.
    pub fn amplifying(self) -> Amplifying {
        if self.reachable {
            return if self.reports_operate { Amplifying::Yes } else { Amplifying::No };
        }
        match self.last_seen_operate {
            Some(false) => Amplifying::No,
            // Seen in Operate, or never seen at all: we cannot tell an amplifier that
            // is switched off from one that is amplifying out of reach.
            _ => Amplifying::Unknown,
        }
    }
}

/// The verdict over both PAs. Yes outranks Unknown outranks No.
pub fn worst(a: Amplifying, b: Amplifying) -> Amplifying {
    match (a, b) {
        (Amplifying::Yes, _) | (_, Amplifying::Yes) => Amplifying::Yes,
        (Amplifying::Unknown, _) | (_, Amplifying::Unknown) => Amplifying::Unknown,
        _ => Amplifying::No,
    }
}

/// Raise the alarm? Only while a PA may be amplifying out of reach and the drive
/// has gone **up** since we lost contact - the one move nobody can undo from the
/// shack, and the PA cannot be told about it either.
///
/// Named fields since build 229 (review of 228): by position, the two drives
/// swapped turned the alarm round - it beeped at a lower drive and kept quiet
/// at a higher one, the one move it exists for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriveAlarm {
    /// What the PA is doing, as far as the server can tell.
    pub state: Amplifying,
    /// The drive when contact was lost (`AlarmBaseline`).
    pub drive_when_lost: u8,
    /// The drive now.
    pub drive_now: u8,
}

impl DriveAlarm {
    pub fn raised(self) -> bool {
        matches!(self.state, Amplifying::Unknown) && self.drive_now > self.drive_when_lost
    }
}

/// May we act on this reading of a PA whose Operate control is a *toggle*?
///
/// The SPE is polled once a second when it is idle, and its Operate command flips
/// rather than sets. Act on a single reading and a front-panel press in that second
/// turns the toggle into the opposite of what was meant: TL would switch the PA
/// **on**. Two readings that agree is not proof - nothing here can be - but it
/// narrows the window to the poll interval and costs one second.
pub fn steady_in_operate(prev: Option<bool>, now: bool) -> bool {
    prev == Some(true) && now
}

/// What the alarm compares against while a PA is out of reach: the drive as it
/// was when contact went away.
///
/// Two ways to get this wrong, both found by re-reading the first version:
///
/// - Recording it **before Thetis has said anything** stores 0 %, and the first
///   real reading then looks like a drive that went up. That is an alarm about
///   nothing, and it would fire on almost every start with the PA unreachable.
/// - **Not clearing it** when contact returns leaves an old value lying about, and
///   the next time contact drops the alarm compares against a number from another
///   session.
///
/// Named fields since build 228 (review of 226): by position, `out_of_reach`
/// and `drive_known` swapped kept a baseline while the PA was in reach, and
/// the alarm later compared against a number from another session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlarmBaseline {
    /// The baseline so far.
    pub prev: Option<u8>,
    /// The RF2K-S cannot be reached and may be amplifying.
    pub out_of_reach: bool,
    /// Thetis has told us the drive on this connection.
    pub drive_known: bool,
    /// The Thetis drive now.
    pub drive_now: u8,
}

impl AlarmBaseline {
    pub fn next(self) -> Option<u8> {
        if !self.out_of_reach {
            return None;
        }
        if !self.drive_known {
            return self.prev;
        }
        self.prev.or(Some(self.drive_now))
    }
}

/// Does the red line stay up? Named fields since build 224 (review of 223):
/// by position, `reachable` swapped with one of the others kept the line away
/// while the PA was out of reach, with every test green.
///
/// The beep and the line answer two different questions, and the owner asked for
/// exactly that split after hearing it in the field (2026-09-16): the **beep** says
/// "the drive is too high *right now*" and stops the moment he puts it back; the
/// **line** says "this happened while the PA was out of reach" and stays until the
/// server can see the PA again - he may not have been in the room when it happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriveWarning {
    /// The red line was up at the previous tick.
    pub was_up: bool,
    /// The drive alarm is on right now.
    pub alarm_now: bool,
    /// The server can reach the PA.
    pub reachable: bool,
}

impl DriveWarning {
    pub fn stays(self) -> bool {
        if self.reachable {
            return false;
        }
        self.was_up || self.alarm_now
    }
}

/// Is the one startup attempt spent on this reading?
///
/// Found by review on build 61: the flag went up as soon as Thetis was known - also
/// on a tick where nothing was restored because the RF2K-S was still out of reach.
/// Start with the SPE online and the RF2K-S ten seconds behind it, last seen in
/// Operate, and the attempt was gone before the RF2K-S could say it was in Standby.
/// The drive then stayed where the PA had left it, which is the report of §1 all
/// over again. `Unknown` is a reading that says nothing, so it spends nothing.
pub fn startup_attempt_used(amplifying: Amplifying, drive_known: bool) -> bool {
    drive_known && !matches!(amplifying, Amplifying::Unknown)
}

/// Every way this station can be putting out RF, named one by one.
///
/// A struct rather than a list of `||`, so that a new source has to be filled in
/// at the one place the tick builds this - the compiler refuses a literal with a
/// field missing. The first version of this had four of the seven and was found
/// wanting twice: the TL PTT machine alone (build 60), then without TUNE and the
/// Yaesu radios (review finding, build 61). Whether the Yaesu radios drive these PAs at all
/// is a question of wiring on the desk; waiting for them is the side that never
/// switches a PA under RF, so they count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TxSources {
    /// The server's PTT machine: a ThetisLink client keyed.
    pub client: bool,
    /// Thetis' own TRX notification: spacebar, footswitch, VOX at the desk.
    pub thetis_trx: bool,
    /// Thetis TUNE: a carrier, sent as its own event and not as TRX.
    pub thetis_tune: bool,
    /// The PTT line as the SPE reports it.
    pub spe_ptt: bool,
    /// The PTT line as the RF2K-S reports it.
    pub rf2k_ptt: bool,
    /// The first Yaesu radio, keyed through the server.
    pub yaesu_ptt: bool,
    /// The second Yaesu radio, keyed through the server.
    pub yaesu2_ptt: bool,
    /// The first Yaesu radio as it reports itself: its own microphone, a
    /// footswitch on the radio, anything the server did not ask for (review finding on
    /// build 62: the flag above is only what ThetisLink requested).
    pub yaesu_tx: bool,
    /// The second Yaesu radio as it reports itself.
    pub yaesu2_tx: bool,
}

impl TxSources {
    /// Any one of them is enough to wait.
    pub fn any(&self) -> bool {
        let TxSources { client, thetis_trx, thetis_tune, spe_ptt, rf2k_ptt, yaesu_ptt, yaesu2_ptt, yaesu_tx, yaesu2_tx } =
            *self;
        client || thetis_trx || thetis_tune || spe_ptt || rf2k_ptt || yaesu_ptt || yaesu2_ptt || yaesu_tx || yaesu2_tx
    }
}

/// What the PA tick reads under the PTT lock (build 226, review of 224: by
/// position, `tx_active` and `known` swapped kept "transmitting" true while
/// Thetis was on, and no PA went to Standby).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThetisReading {
    /// Anything transmits (`TxSources::any`).
    pub tx_active: bool,
    /// The Thetis drive in percent.
    pub drive: u8,
    /// Thetis has said something on this connection.
    pub known: bool,
}

/// Does a new Thetis run begin on this tick?
///
/// The one startup attempt is per Thetis run, not per server run (review finding on build
/// 63): close Thetis with a PA in Operate, switch the PA off, start Thetis again,
/// and Thetis comes back carrying the PA's drive - the report of §1, with the
/// server never having stopped. Thetis going from unknown to known is that
/// boundary, and it costs nothing to check: on the very first tick the attempt
/// has not been spent yet.
///
/// Named fields since build 228 (review of 226): by position, the two swapped
/// fired when Thetis went away instead of when it came back, and the process
/// check of build 226 looked at the wrong moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThetisKnown {
    /// Thetis was known on the previous tick.
    pub before: bool,
    /// Thetis is known now.
    pub now: bool,
}

impl ThetisKnown {
    pub fn became_known(self) -> bool {
        !self.before && self.now
    }
}

/// What Thetis came back as, once it is known again (build 226, owner after
/// the review of 224: "only at a real restart of Thetis"). `handle_disconnect`
/// clears what Thetis said, so every new TCI connection made it known again -
/// and the drive was put back after a short drop too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThetisBackInput<'a> {
    /// Thetis.exe's process id in the run the startup check was made for.
    pub run_pid: Option<u32>,
    /// Every Thetis.exe on this PC now; empty: not on this PC.
    pub pids_now: &'a [u32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThetisBack {
    /// Another process: Thetis was restarted.
    NewProcess { pid: u32, was: Option<u32> },
    /// The same process: TCI came back, Thetis never went away.
    SameProcess { pid: u32 },
    /// Not on this PC: no way to tell, so every new connection counts, as
    /// before build 226.
    NotSeen,
    /// More than one Thetis.exe (build 227, review of 226): an old one still
    /// closing while the new one already talks TCI, or two radios. No way to
    /// tell which, so every new connection counts, as before build 226.
    Several { count: usize },
}

impl ThetisBackInput<'_> {
    pub fn back(self) -> ThetisBack {
        match self.pids_now {
            [] => ThetisBack::NotSeen,
            [pid] if self.run_pid == Some(*pid) => ThetisBack::SameProcess { pid: *pid },
            [pid] => ThetisBack::NewProcess { pid: *pid, was: self.run_pid },
            more => ThetisBack::Several { count: more.len() },
        }
    }

    /// The process id to remember for this run: only when one is sure.
    pub fn run_pid_now(self) -> Option<u32> {
        match self.pids_now {
            [pid] => Some(*pid),
            _ => None,
        }
    }
}

impl ThetisBack {
    /// A new run: the startup check runs again.
    pub fn new_run(self) -> bool {
        !matches!(self, ThetisBack::SameProcess { .. })
    }
}

/// The active-PA mark as it may be used: a mark on a PA this server does not
/// have is no permission to switch off the one it does have. Named fields
/// since build 228 (review of 226): by position, the two presences swapped
/// dropped the mark with only one PA configured, and with it the single-Operate
/// rule and the drive choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveMark {
    /// The mark as stored (`active_pa`, 0: none).
    pub marked: u8,
    /// This server has an SPE configured.
    pub spe_present: bool,
    /// This server has an RF2K-S configured.
    pub rf2k_present: bool,
}

impl ActiveMark {
    pub fn effective(self) -> u8 {
        match self.marked {
            PA_SPE if !self.spe_present => 0,
            PA_RF2K if !self.rf2k_present => 0,
            other => other,
        }
    }
}

/// What the single-Operate rule decides on, for one PA. Named fields since
/// build 224 (review of 223): by position, `in_operate` or `connected` swapped
/// with `tx_active` gave `Nothing` outside TX, so a PA that is not the active
/// one stayed in Operate, with every test green.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnforceInput {
    /// The PA marked active for this antenna position (0: none).
    pub active_pa: u8,
    /// The PA this reading is of.
    pub pa_id: u8,
    /// The server has a fresh reading of it.
    pub connected: bool,
    /// It reports Operate.
    pub in_operate: bool,
    /// Anything transmits (`TxSources::any`).
    pub tx_active: bool,
}

impl EnforceInput {
    /// What to do with a PA we have a fresh reading of.
    pub fn decide(self) -> Enforce {
        // No reading is not a reason to act: an unreachable PA is the case where TL
        // knows least, and switching on a guess is worse than leaving it.
        if !self.connected || !self.in_operate {
            return Enforce::Nothing;
        }
        if operate_allowed(self.active_pa, self.pa_id) {
            return Enforce::Nothing;
        }
        if self.tx_active {
            // Taking a PA out of Operate with power going through it is the kind of
            // switching this project exists to avoid.
            return Enforce::WaitForRx;
        }
        Enforce::Standby
    }
}

/// The drive the PA took over from, if this poll is the moment it took over.
///
/// Called with the previous reading and the current one; `None` on the first
/// poll, because then there is no transition to see - only a state.
///
/// A drive of 0 % is what the server holds before Thetis has told it anything.
/// Saving that would replace a real snapshot with a value that can never be
/// restored (see `startup_restore`), so it is not a snapshot.
pub fn snapshot_on_operate(prev_operate: Option<bool>, now_operate: bool, thetis_drive: u8) -> Option<u8> {
    match prev_operate {
        Some(false) if now_operate && thetis_drive > 0 => Some(thetis_drive),
        _ => None,
    }
}

/// The drive to put back when no PA is amplifying: what it was before the PA
/// took over, or the configured default when that is not known.
pub fn restore_target(saved: Option<u8>, default_drive: u8) -> u8 {
    saved.unwrap_or(default_drive)
}

/// Should TL push a drive at startup, and which one?
///
/// Only on a positive reading that the PA is **not** amplifying. Off, unplugged
/// or unreachable all mean "not known", and then TL does nothing: putting the
/// drive back to 100 % while a PA is in fact in Operate would be the one mistake
/// this must never make.
pub fn startup_restore(
    amplifying: Amplifying,
    drive_known: bool,
    saved: Option<u8>,
    default_drive: u8,
    thetis_drive: u8,
) -> Option<u8> {
    // Thetis not there yet is not "the drive is 0": pushing then would write into
    // a connection that does not exist, and the one attempt this makes per run
    // would be spent. The caller keeps asking until Thetis answers.
    //
    // And only on `No`. `Unknown` is an RF2K-S we cannot reach that may be
    // amplifying; raising the drive there is the mistake this must never make.
    if !matches!(amplifying, Amplifying::No) || !drive_known {
        return None;
    }
    let target = restore_target(saved, default_drive);
    if target == 0 || target == thetis_drive {
        return None;
    }
    Some(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The cases below are spelled by position; the inputs are built here by
    // name, as the loop does (build 224).
    fn enforce(active_pa: u8, pa_id: u8, connected: bool, in_operate: bool, tx_active: bool) -> Enforce {
        EnforceInput { active_pa, pa_id, connected, in_operate, tx_active }.decide()
    }

    fn warning_stays(was_up: bool, alarm_now: bool, reachable: bool) -> bool {
        DriveWarning { was_up, alarm_now, reachable }.stays()
    }

    fn drive_alarm(state: Amplifying, drive_when_lost: u8, drive_now: u8) -> bool {
        DriveAlarm { state, drive_when_lost, drive_now }.raised()
    }

    fn alarm_baseline(prev: Option<u8>, out_of_reach: bool, drive_known: bool, drive_now: u8) -> Option<u8> {
        AlarmBaseline { prev, out_of_reach, drive_known, drive_now }.next()
    }

    fn new_thetis_run(before: bool, now: bool) -> bool {
        ThetisKnown { before, now }.became_known()
    }

    fn effective_active_pa(marked: u8, spe_present: bool, rf2k_present: bool) -> u8 {
        ActiveMark { marked, spe_present, rf2k_present }.effective()
    }

    /// The five callers of the mark, the baseline and the Thetis run build
    /// their inputs by name (build 228, review of 226).
    #[test]
    fn the_callers_hand_the_mark_the_baseline_and_the_run_over_by_name() {
        let net = include_str!("network.rs");
        let ui = include_str!("ui/update.rs");
        for (src, line, n) in [
            (net, "spe_present: spe.is_some(),", 1),
            (net, "rf2k_present: rf2k.is_some(),", 1),
            (net, "spe_present: self.spe.is_some(),", 2),
            (net, "rf2k_present: self.rf2k.is_some(),", 2),
            (ui, "spe_present: true,", 1),
            (ui, "rf2k_present: self.rf2k.is_some(),", 1),
            (ui, "spe_present: self.spe.is_some(),", 1),
            (ui, "rf2k_present: true,", 1),
            (net, "prev: pa_drive_when_lost,", 1),
            // The two drives (build 229, review of 228, K1).
            (net, "pa_policy::DriveAlarm { state: rf_amp, drive_when_lost: base, drive_now }.raised()", 1),
            (net, "drive_known: thetis_known,", 1),
            (net, "(pa_policy::ThetisKnown { before: pa_prev_thetis_known, now: thetis_known }).became_known()", 1),
        ] {
            assert_eq!(src.matches(line).count(), n, "{line}");
        }
        assert_eq!(net.matches("ActiveMark {").count(), 3);
        assert_eq!(ui.matches("ActiveMark {").count(), 2);
    }

    /// The loop hands its readings over by name (build 224, review of 223):
    /// by position, two booleans swapped kept a PA that is not the active one
    /// in Operate, or kept the red line away while the PA was out of reach,
    /// with every test here green.
    #[test]
    fn the_loop_hands_the_pa_readings_over_by_name() {
        let net = include_str!("network.rs");
        for line in [
            "connected: spe_seen,",
            "in_operate: spe_operate,",
            "connected: rf_seen,",
            "in_operate: rf_operate,",
            "was_up: pa_drive_warning,",
            "alarm_now: pa_drive_alarm_now,",
            "reachable: !out_of_reach,",
            // A copied row with the wrong PA (review of 224, P1).
            "pa_id: PA_SPE,",
            "pa_id: PA_RF2K,",
            // The tick's own readings (build 226, review of 224).
            "pa_policy::ThetisReading { tx_active: tx.any(), drive: g.drive_level(), known: g.power_on() }",
            "out_of_reach,\n                                drive_known: thetis_known,",
            "let pa_policy::ThetisReading { tx_active: is_tx, drive: drive_now, known: thetis_known } = {",
            "reachable: rf_seen,",
            "reports_operate: rf_operate,",
            "last_seen_operate: pa_rf2k_last_seen,",
            "let back_input = pa_policy::ThetisBackInput { run_pid: pa_thetis_pid, pids_now: &pids_now };",
            // The run's process id is kept (review of 226, PD1).
            "pa_thetis_pid = back_input.run_pid_now();",
        ] {
            assert_eq!(net.matches(line).count(), 1, "{line}");
        }
        assert_eq!(net.matches("tx_active: is_tx,").count(), 3, "both PAs, and the reading it comes from");
    }

    // ---- the report: everything off while the PA was in Operate -------------

    // ---- what "no contact" means per PA ------------------------------------

    /// The SPE puts itself in Standby when its link goes away, so no contact is a
    /// fact and not a gap.
    #[test]
    fn an_unreachable_spe_is_not_amplifying() {
        assert_eq!(spe_amplifying(false, 2), Amplifying::No, "last seen in Operate, but it drops out by itself");
        assert_eq!(spe_amplifying(true, 2), Amplifying::Yes);
        assert_eq!(spe_amplifying(true, 1), Amplifying::No);
        assert_eq!(spe_amplifying(true, 0), Amplifying::No);
    }

    /// The RF2K-S keeps amplifying without its network, and cannot be told
    /// anything any more.
    #[test]
    fn an_unreachable_rf2k_is_unknown_unless_it_was_in_standby() {
        let rf2k_amplifying = |reachable, reports_operate, last_seen_operate| {
            Rf2kReading { reachable, reports_operate, last_seen_operate }.amplifying()
        };
        assert_eq!(rf2k_amplifying(false, true, Some(false)), Amplifying::No, "a stale Operate out of reach");
        assert_eq!(rf2k_amplifying(false, false, Some(true)), Amplifying::Unknown);
        assert_eq!(rf2k_amplifying(false, false, None), Amplifying::Unknown, "never seen is not the same as off");
        assert_eq!(rf2k_amplifying(false, false, Some(false)), Amplifying::No);
        assert_eq!(rf2k_amplifying(true, true, Some(false)), Amplifying::Yes, "contact wins over the memory");
        assert_eq!(rf2k_amplifying(true, false, Some(true)), Amplifying::No);
    }

    #[test]
    fn the_worst_of_the_two_decides() {
        use Amplifying::*;
        assert_eq!(worst(No, No), No);
        assert_eq!(worst(No, Unknown), Unknown);
        assert_eq!(worst(Unknown, No), Unknown);
        assert_eq!(worst(Unknown, Yes), Yes);
        assert_eq!(worst(Yes, No), Yes);
    }

    // ---- the alarm ---------------------------------------------------------

    /// A toggle needs two readings that agree, or a front-panel press in the poll
    /// gap makes TL switch the PA on instead of off.
    #[test]
    fn a_toggle_waits_for_two_readings() {
        assert!(steady_in_operate(Some(true), true));
        assert!(!steady_in_operate(None, true), "first reading of the run");
        assert!(!steady_in_operate(Some(false), true), "it only just appeared in Operate");
        assert!(!steady_in_operate(Some(true), false), "already gone");
    }

    /// The value the alarm compares against is only taken once Thetis has spoken,
    /// and it is dropped the moment the PA is back in reach.
    #[test]
    fn the_baseline_waits_for_thetis_and_is_dropped_on_contact() {
        // Server starts, PA out of reach, Thetis not connected yet: nothing recorded.
        assert_eq!(alarm_baseline(None, true, false, 0), None);
        // Thetis reports: now it counts.
        assert_eq!(alarm_baseline(None, true, true, 6), Some(6));
        // And it stays what it was, whatever the drive does afterwards.
        assert_eq!(alarm_baseline(Some(6), true, true, 20), Some(6));
        // Thetis goes away while the PA is still out of reach: the drive reads 0
        // again, and that must not replace what we knew.
        assert_eq!(alarm_baseline(Some(6), true, false, 0), Some(6));
        // Contact with the PA: gone, so the next loss starts fresh.
        assert_eq!(alarm_baseline(Some(6), false, true, 20), None);
        assert_eq!(alarm_baseline(None, false, true, 20), None);
        assert_eq!(alarm_baseline(Some(6), false, false, 0), None);
    }

    /// The first reading after a start is not a raise. Without this the alarm
    /// fired on a drive that had simply never been read yet.
    #[test]
    fn a_first_reading_is_not_a_raise() {
        let base = alarm_baseline(None, true, false, 0);
        assert!(!drive_alarm(Amplifying::Unknown, base.unwrap_or(0), 0), "nothing known yet");
        let base = alarm_baseline(base, true, true, 6);
        assert_eq!(base, Some(6));
        assert!(!drive_alarm(Amplifying::Unknown, base.unwrap(), 6), "the first reading itself");
        assert!(drive_alarm(Amplifying::Unknown, base.unwrap(), 20), "and then it really goes up");
    }

    /// The red line outlives the beep: putting the drive back stops the sound but
    /// not the notice, and only contact with the PA clears it.
    #[test]
    fn the_red_line_stays_until_the_pa_is_back_in_reach() {
        assert!(warning_stays(false, true, false), "raised while out of reach");
        assert!(warning_stays(true, false, false), "drive put back, still out of reach");
        assert!(!warning_stays(false, false, false), "nothing happened");
        assert!(!warning_stays(true, true, true), "the PA can be reached again");
        assert!(!warning_stays(true, false, true), "reachable clears it, raised or not");
    }

    #[test]
    fn the_alarm_is_for_a_drive_that_goes_up_out_of_reach() {
        use Amplifying::*;
        assert!(drive_alarm(Unknown, 6, 20), "drive raised while the PA is out of reach");
        assert!(!drive_alarm(Unknown, 6, 6), "unchanged");
        assert!(!drive_alarm(Unknown, 20, 6), "lowered is never dangerous");
        assert!(!drive_alarm(No, 6, 100), "nobody is amplifying");
        assert!(!drive_alarm(Yes, 6, 100), "in reach: the cap and the operator can still act");
    }

    #[test]
    fn a_standby_pa_at_startup_gets_the_drive_back() {
        // The default is written out here, not expressed in terms of itself: a
        // constant that only appears as its own name can be anything and the suite
        // stays green (a review caught exactly that in the hold of build 47).
        assert_eq!(DEFAULT_PA_DRIVE, 100);
        // Snapshot from a previous session: before Operate the drive was 100 %.
        assert_eq!(startup_restore(Amplifying::No, true, Some(100), DEFAULT_PA_DRIVE, 6), Some(100));
        // No snapshot at all - the configured default.
        assert_eq!(startup_restore(Amplifying::No, true, None, DEFAULT_PA_DRIVE, 6), Some(100));
        // A station that wants something else there.
        assert_eq!(startup_restore(Amplifying::No, true, None, 80, 6), Some(80));
    }

    /// The snapshot is what comes back, not the default, when both are there. A review
    /// on build 61: every earlier test had the two equal, so a `restore_target`
    /// that ignored the snapshot stayed green.
    #[test]
    fn the_snapshot_wins_over_the_default() {
        assert_eq!(restore_target(Some(80), DEFAULT_PA_DRIVE), 80);
        assert_eq!(startup_restore(Amplifying::No, true, Some(80), DEFAULT_PA_DRIVE, 6), Some(80));
    }

    /// The attempt is spent on a reading, not on a gap.
    #[test]
    fn the_startup_attempt_is_not_spent_on_an_unreachable_pa() {
        assert!(!startup_attempt_used(Amplifying::Unknown, true), "RF2K-S not yet answering");
        assert!(!startup_attempt_used(Amplifying::No, false), "Thetis not yet answering");
        assert!(startup_attempt_used(Amplifying::No, true), "restored, or nothing to restore");
        assert!(startup_attempt_used(Amplifying::Yes, true), "a PA is amplifying: leave it, and done");
    }

    /// A run is a process (build 226): TCI back to the same Thetis.exe leaves
    /// the drive; another process, or none on this PC, runs the check again.
    /// And the loop spends the check only on a new run.
    #[test]
    fn a_tci_drop_without_a_thetis_restart_is_not_a_new_run() {
        let back = |run_pid, pids_now: &[u32]| ThetisBackInput { run_pid, pids_now }.back();
        assert_eq!(back(Some(10), &[10]), ThetisBack::SameProcess { pid: 10 });
        assert!(!back(Some(10), &[10]).new_run(), "a TCI drop, Thetis still running");
        assert_eq!(back(Some(10), &[11]), ThetisBack::NewProcess { pid: 11, was: Some(10) });
        assert!(back(Some(10), &[11]).new_run(), "Thetis was restarted");
        assert_eq!(back(None, &[11]), ThetisBack::NewProcess { pid: 11, was: None });
        assert!(back(None, &[11]).new_run(), "the first run seen with a process");
        assert_eq!(back(Some(10), &[]), ThetisBack::NotSeen);
        assert!(back(Some(10), &[]).new_run(), "Thetis on another PC: as before 226");
        assert!(back(None, &[]).new_run());
        // The old one still closing while the new one talks (review of 226):
        // not "the same", whichever number is lower.
        assert_eq!(back(Some(10), &[10, 11]), ThetisBack::Several { count: 2 });
        assert!(back(Some(10), &[10, 11]).new_run());
        assert!(back(Some(11), &[10, 11]).new_run());
        // What the run remembers: one process, or nothing.
        let keep = |pids_now: &[u32]| ThetisBackInput { run_pid: Some(10), pids_now }.run_pid_now();
        assert_eq!(keep(&[12]), Some(12));
        assert_eq!(keep(&[]), None);
        assert_eq!(keep(&[10, 11]), None);
        let net = include_str!("network.rs");
        assert_eq!(net.matches("if back.new_run() {").count(), 1);
        assert_eq!(
            net.matches("                                        pa_startup_done = false;\n").count(),
            1,
            "only a new run spends it again"
        );
    }

    /// Thetis going from unknown to known is a new run, and only that.
    #[test]
    fn a_thetis_run_begins_when_it_becomes_known() {
        assert!(new_thetis_run(false, true), "Thetis just came up");
        assert!(!new_thetis_run(true, true), "still the same run");
        assert!(!new_thetis_run(true, false), "Thetis went away - not a start");
        assert!(!new_thetis_run(false, false), "still away");
    }

    /// The ids as they stand in `active_pa` on disk. Written out, because four
    /// other places spell them as bare numbers and no test stood between them.
    #[test]
    fn the_pa_ids_are_the_ones_in_the_config() {
        assert_eq!(PA_SPE, 1);
        assert_eq!(PA_RF2K, 2);
    }

    #[test]
    fn nothing_to_do_when_the_drive_is_already_right() {
        assert_eq!(startup_restore(Amplifying::No, true, Some(100), DEFAULT_PA_DRIVE, 100), None);
    }

    /// The mistake this must never make.
    #[test]
    fn a_pa_that_is_amplifying_is_left_alone() {
        assert_eq!(startup_restore(Amplifying::Yes, true, Some(100), DEFAULT_PA_DRIVE, 6), None);
    }

    /// An RF2K-S we cannot reach may well be amplifying.
    #[test]
    fn without_a_reading_tl_does_not_touch_the_drive() {
        assert_eq!(startup_restore(Amplifying::Unknown, true, Some(100), DEFAULT_PA_DRIVE, 6), None);
    }

    /// Thetis not connected yet: the drive reads 0 because nothing has said
    /// otherwise. Pushing then would spend the one attempt per run on a
    /// connection that is not there.
    #[test]
    fn without_thetis_the_drive_is_not_pushed_yet() {
        assert_eq!(startup_restore(Amplifying::No, false, Some(100), DEFAULT_PA_DRIVE, 0), None);
    }

    /// And the same 0 must not be saved as a snapshot, or the real one is lost.
    #[test]
    fn a_drive_of_zero_is_not_a_snapshot() {
        assert_eq!(snapshot_on_operate(Some(false), true, 0), None);
        assert_eq!(snapshot_on_operate(Some(false), true, 1), Some(1));
    }

    #[test]
    fn a_default_of_zero_is_not_pushed() {
        assert_eq!(startup_restore(Amplifying::No, true, None, 0, 6), None);
    }

    // ---- the snapshot ------------------------------------------------------

    #[test]
    fn the_drive_is_remembered_at_the_moment_the_pa_takes_over() {
        assert_eq!(snapshot_on_operate(Some(false), true, 100), Some(100));
        assert_eq!(snapshot_on_operate(Some(true), true, 6), None, "already in Operate");
        assert_eq!(snapshot_on_operate(Some(true), false, 6), None, "going to Standby");
        assert_eq!(snapshot_on_operate(Some(false), false, 6), None, "was in Standby, still is");
        assert_eq!(snapshot_on_operate(None, true, 6), None, "first poll is a state, not a transition");
    }

    // ---- two PAs in Operate ------------------------------------------------

    /// The second report: both PAs in Operate write the drive, with different
    /// values, and it keeps flipping. The one that is not the active PA goes to
    /// Standby.
    #[test]
    fn the_pa_that_is_not_the_active_one_goes_to_standby() {
        assert_eq!(enforce(PA_RF2K, PA_SPE, true, true, false), Enforce::Standby);
        assert_eq!(enforce(PA_SPE, PA_RF2K, true, true, false), Enforce::Standby);
    }

    #[test]
    fn the_active_pa_is_never_switched_off() {
        assert_eq!(enforce(PA_RF2K, PA_RF2K, true, true, false), Enforce::Nothing);
        assert_eq!(enforce(PA_SPE, PA_SPE, true, true, false), Enforce::Nothing);
    }

    /// Never with power going through it.
    #[test]
    fn not_while_transmitting() {
        assert_eq!(enforce(PA_RF2K, PA_SPE, true, true, true), Enforce::WaitForRx);
    }

    /// Each source on its own holds the switch; none of them is decoration. The
    /// list is spelled out here a second time on purpose: the day a field is added
    /// to the struct, this test must be told about it too.
    #[test]
    fn every_tx_source_alone_holds_the_switch() {
        assert!(!TxSources::default().any(), "quiet station");
        let one_each = [
            TxSources { client: true, ..Default::default() },
            TxSources { thetis_trx: true, ..Default::default() },
            TxSources { thetis_tune: true, ..Default::default() },
            TxSources { spe_ptt: true, ..Default::default() },
            TxSources { rf2k_ptt: true, ..Default::default() },
            TxSources { yaesu_ptt: true, ..Default::default() },
            TxSources { yaesu2_ptt: true, ..Default::default() },
            TxSources { yaesu_tx: true, ..Default::default() },
            TxSources { yaesu2_tx: true, ..Default::default() },
        ];
        // One bool per field, so the size of the struct IS the number of fields:
        // a tenth field added to the struct and to any() but not to this list
        // turns this red (review finding on build 63 - a literal 7 here did not).
        assert_eq!(std::mem::size_of::<TxSources>(), one_each.len(), "one entry per field");
        for src in one_each {
            assert!(src.any(), "{:?}", src);
            assert_eq!(enforce(PA_RF2K, PA_SPE, true, true, src.any()), Enforce::WaitForRx, "{:?}", src);
        }
    }

    #[test]
    fn nothing_to_enforce_without_a_reading_or_in_standby() {
        assert_eq!(enforce(PA_RF2K, PA_SPE, false, true, false), Enforce::Nothing, "unreachable");
        assert_eq!(enforce(PA_RF2K, PA_SPE, true, false, false), Enforce::Nothing, "already in Standby");
    }

    /// With no active PA marked, TL has no opinion - about switching off, and
    /// about switching on.
    #[test]
    fn without_an_active_pa_tl_has_no_opinion() {
        assert_eq!(enforce(0, PA_SPE, true, true, false), Enforce::Nothing);
        assert_eq!(enforce(0, PA_RF2K, true, true, false), Enforce::Nothing);
        assert!(operate_allowed(0, PA_SPE));
        assert!(operate_allowed(0, PA_RF2K));
    }

    #[test]
    fn a_mark_on_a_pa_that_is_not_there_counts_for_nothing() {
        assert_eq!(effective_active_pa(PA_SPE, false, true), 0, "marked SPE, no SPE configured");
        assert_eq!(effective_active_pa(PA_RF2K, true, false), 0, "marked RF2K-S, none configured");
        assert_eq!(effective_active_pa(PA_SPE, true, true), PA_SPE);
        assert_eq!(effective_active_pa(PA_RF2K, true, true), PA_RF2K);
        assert_eq!(effective_active_pa(0, true, true), 0);
    }

    // ---- the permission ----------------------------------------------------

    /// `active_pa` decides whether a PA may go into Operate at all, so a request
    /// for the other one is refused rather than carried out and undone.
    #[test]
    fn only_the_active_pa_may_be_switched_to_operate() {
        assert!(operate_allowed(PA_SPE, PA_SPE));
        assert!(!operate_allowed(PA_SPE, PA_RF2K));
        assert!(operate_allowed(PA_RF2K, PA_RF2K));
        assert!(!operate_allowed(PA_RF2K, PA_SPE));
    }
}
