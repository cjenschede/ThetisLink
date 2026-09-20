// SPDX-License-Identifier: GPL-2.0-or-later
//! "Start Thetis automatically": the one rule, for the desktop and the phone.
//!
//! The option means "launch Thetis if it is not running when I connect". It
//! is decided once per client run, on the first report the server gives about
//! Thetis. Until build 138 each front end fired it once, whenever it first saw
//! "not running" - so a client that connected while Thetis ran kept the shot
//! loaded, and the operator closing Thetis on purpose later had it launched
//! again; with the desktop and the phone both connected, twice (field test
//! 2026-09-19 17:28 and 17:29). And the rule was written twice, in Rust and in
//! Kotlin.

/// The state flags of the server's push that speak about Thetis - named, not
/// five booleans in a row (review 318). Only a server that reports its
/// state flags gives them; an old one never reaches the rule.
#[derive(Debug, Default, Clone, Copy)]
pub struct ThetisFlags {
    pub configured: bool,
    pub starting: bool,
    pub tci_up: bool,
    pub process_running: bool,
}

/// What the server's state push says about Thetis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThetisReport {
    /// This server has no Thetis configured.
    NotConfigured,
    /// It is launching Thetis now.
    Starting,
    /// TCI is up: Thetis runs.
    Up,
    /// The process runs, TCI does not answer (yet).
    ProcessRunning,
    /// The process does not run.
    NotRunning,
}

impl ThetisReport {
    /// From the state flags of the server's push.
    pub fn from_flags(f: ThetisFlags) -> Self {
        if !f.configured {
            ThetisReport::NotConfigured
        } else if f.starting {
            ThetisReport::Starting
        } else if f.tci_up {
            ThetisReport::Up
        } else if f.process_running {
            ThetisReport::ProcessRunning
        } else {
            ThetisReport::NotRunning
        }
    }
}

/// The decision, once per client run (kept across reconnects: a network blip
/// must not launch a Thetis the operator closed on purpose).
#[derive(Debug, Default)]
pub struct ThetisAutostart {
    decided: bool,
}

impl ThetisAutostart {
    /// A report arrived. True when the power-on is to be sent now.
    ///
    /// - Option off: nothing, and nothing decided - switching it on later in
    ///   the run decides on the next report.
    /// - No Thetis configured, or a launch under way: wait.
    /// - Thetis runs (TCI up, or the process at least): decided, no launch.
    /// - Thetis does not run: decided, launch.
    pub fn step(&mut self, enabled: bool, report: ThetisReport) -> bool {
        if self.decided || !enabled {
            return false;
        }
        match report {
            ThetisReport::NotConfigured | ThetisReport::Starting => false,
            ThetisReport::Up | ThetisReport::ProcessRunning => {
                self.decided = true;
                false
            }
            ThetisReport::NotRunning => {
                self.decided = true;
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The field test: connected while Thetis ran, then Thetis closed on
    /// purpose - it stays closed.
    #[test]
    fn a_thetis_closed_after_connecting_stays_closed() {
        let mut a = ThetisAutostart::default();
        assert!(!a.step(true, ThetisReport::Up), "running at connect: nothing to do");
        assert!(!a.step(true, ThetisReport::NotRunning), "closed later on purpose: stays closed");
        assert!(!a.step(true, ThetisReport::NotRunning), "and again");
    }

    #[test]
    fn not_running_at_connect_launches_once() {
        let mut a = ThetisAutostart::default();
        assert!(a.step(true, ThetisReport::NotRunning));
        assert!(!a.step(true, ThetisReport::Starting));
        assert!(!a.step(true, ThetisReport::NotRunning), "a failed launch is not repeated");
    }

    #[test]
    fn it_waits_for_a_real_report() {
        let mut a = ThetisAutostart::default();
        for r in [ThetisReport::NotConfigured, ThetisReport::Starting] {
            assert!(!a.step(true, r), "{r:?}");
        }
        assert!(a.step(true, ThetisReport::NotRunning), "still undecided until then");
        let mut a = ThetisAutostart::default();
        assert!(!a.step(true, ThetisReport::ProcessRunning), "the process runs: decided");
        assert!(!a.step(true, ThetisReport::NotRunning));
    }

    #[test]
    fn the_option_off_decides_nothing() {
        let mut a = ThetisAutostart::default();
        assert!(!a.step(false, ThetisReport::NotRunning));
        assert!(a.step(true, ThetisReport::NotRunning), "switched on later in the run");
    }

    #[test]
    fn the_report_follows_the_flags() {
        use ThetisReport::*;
        let f = |configured, starting, tci_up, process_running| {
            ThetisReport::from_flags(ThetisFlags { configured, starting, tci_up, process_running })
        };
        assert_eq!(f(false, false, true, true), NotConfigured);
        assert_eq!(f(true, true, false, false), Starting);
        assert_eq!(f(true, false, true, true), Up);
        assert_eq!(f(true, false, false, true), ProcessRunning);
        assert_eq!(f(true, false, false, false), NotRunning);
    }
}
