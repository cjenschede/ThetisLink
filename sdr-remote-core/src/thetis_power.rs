// SPDX-License-Identifier: GPL-2.0-or-later

//! What to do when someone asks for Thetis to be switched on.
//!
//! Small, and it lives here for the same reason the Bluetooth button's rule
//! does: it is a decision, it has more inputs than fit comfortably in the head,
//! and where it used to live - inside an async method that talks to a process
//! table and a websocket - nothing could reach it.
//!
//! The case it exists for is PD0PLK's, reported 2026-09-01: press off, press on
//! half a second later, and nothing happens for a minute.

/// What the caller should do about a power-on request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerOnStep {
    /// Thetis is there and talking. Just tell the radio.
    SendPowerOn,
    /// No process. Start one.
    Launch,
    /// A process is there and nobody asked it to leave: it is starting up, or
    /// it was started by hand. Wait for it to connect.
    WaitForConnection,
    /// A process is there **and we asked it to quit**. It is on its way out, so
    /// waiting for it to connect is waiting for something that cannot happen.
    /// Wait for it to disappear, then launch.
    WaitForExitThenLaunch,
    /// A launch is already under way. Do not start a second one.
    Ignore,
}

/// The inputs, named, because five booleans in a row is how the wrong one gets
/// passed.
#[derive(Clone, Copy, Debug)]
pub struct PowerOnInputs {
    /// TCI is up: Thetis is running and talking to us.
    pub connected: bool,
    /// A path to Thetis.exe is configured, so launching is possible at all.
    pub have_path: bool,
    /// A launch is already pending from an earlier request.
    pub launch_pending: bool,
    /// Thetis.exe is in the process table.
    pub process_running: bool,
    /// We asked Thetis to shut down and it has not gone yet.
    pub shutdown_pending: bool,
}

pub fn power_on_step(i: PowerOnInputs) -> PowerOnStep {
    if i.connected {
        return PowerOnStep::SendPowerOn;
    }
    if !i.have_path {
        // Nothing to launch. Send it anyway and let it fail loudly rather than
        // silently doing nothing.
        return PowerOnStep::SendPowerOn;
    }
    if i.launch_pending {
        return PowerOnStep::Ignore;
    }
    if i.process_running {
        // The row that was missing. A program we asked to quit is still in the
        // process table while it closes, and waiting for it to connect is
        // waiting for something that cannot happen.
        if i.shutdown_pending {
            return PowerOnStep::WaitForExitThenLaunch;
        }
        return PowerOnStep::WaitForConnection;
    }
    PowerOnStep::Launch
}

/// May we forget that we asked Thetis to close?
///
/// The flag means one thing: *we asked, and it has not gone yet*. So it ends
/// when the process is actually gone, and not a moment earlier.
///
/// The first version cleared it whenever TCI was connected, with a comment
/// saying "Thetis is talking again, so whatever we asked of it is settled".
/// Right after `shutdown_ex;` Thetis is still talking - the websocket takes a
/// moment to drop - so a tick in that gap wiped the flag, and a power-on one
/// second later had no idea a shutdown was under way. The comment described an
/// intention the condition did not implement, and the owner's own test caught
/// it: `power-on: WaitForConnection` where the whole repair exists to say
/// `WaitForExitThenLaunch`.
///
/// The second row is the safety net. A Thetis that refuses to close - a dialog
/// asking to confirm - would otherwise leave us believing a shutdown is under
/// way for as long as the server runs. `waited_secs` uses the same patience the
/// launch already has, rather than a new number invented here.
pub fn forget_shutdown(process_running: bool, waited_secs: u64, patience_secs: u64) -> bool {
    !process_running || waited_secs > patience_secs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> PowerOnInputs {
        PowerOnInputs {
            connected: false,
            have_path: true,
            launch_pending: false,
            process_running: false,
            shutdown_pending: false,
        }
    }

    /// PD0PLK, 2026-09-01, and the reason this module exists.
    ///
    /// From his server log:
    ///
    /// ```text
    /// 21:18:19.147  Client requested Thetis shutdown
    /// 21:18:19.683  set_power(true) called - connected=false
    /// 21:18:19.688  Thetis already running, waiting for connection
    /// 21:19:20.700  Thetis launch timeout (60s), cancelling
    /// ```
    ///
    /// Half a second after asking Thetis to close, he pressed on. The process
    /// was still there - a program on its way out is still in the process
    /// table - so the server decided it was already running and waited a
    /// minute for a connection that could never arrive, because that process
    /// was busy disappearing.
    #[test]
    fn pressing_on_while_thetis_is_still_closing_waits_for_it_to_go() {
        let step = power_on_step(PowerOnInputs {
            process_running: true,
            shutdown_pending: true,
            ..inputs()
        });

        assert_eq!(
            step,
            PowerOnStep::WaitForExitThenLaunch,
            "a process we asked to quit will never connect - waiting for that is the bug"
        );
    }

    /// The owner's test of build 15, and what it caught.
    ///
    /// A second after asking Thetis to close, the process is still there and
    /// TCI may still be up. Neither of those means the shutdown is over.
    #[test]
    fn a_shutdown_is_not_over_while_the_process_is_still_there() {
        assert!(
            !forget_shutdown(true, 1, 60),
            "a process that is still closing has not closed"
        );
    }

    /// It is over when the process has gone. That is the whole definition.
    #[test]
    fn a_shutdown_is_over_when_the_process_is_gone() {
        assert!(forget_shutdown(false, 1, 60));
    }

    /// And a Thetis that will not close must not leave us believing a shutdown
    /// is under way for the rest of the session.
    #[test]
    fn a_shutdown_that_never_happens_is_given_up_on() {
        assert!(!forget_shutdown(true, 60, 60));
        assert!(forget_shutdown(true, 61, 60));
    }

    /// The same picture without the shutdown: a process nobody asked to leave
    /// is starting up, or was started by hand. Then waiting is right.
    #[test]
    fn a_process_nobody_asked_to_leave_is_waited_for() {
        let step = power_on_step(PowerOnInputs {
            process_running: true,
            ..inputs()
        });
        assert_eq!(step, PowerOnStep::WaitForConnection);
    }

    #[test]
    fn no_process_means_launch() {
        assert_eq!(power_on_step(inputs()), PowerOnStep::Launch);
    }

    /// Connected wins over everything: Thetis is there and talking, so there is
    /// nothing to start and nothing to wait for.
    #[test]
    fn connected_just_sends_the_command() {
        for (process_running, shutdown_pending) in
            [(false, false), (true, false), (true, true)]
        {
            let step = power_on_step(PowerOnInputs {
                connected: true,
                process_running,
                shutdown_pending,
                ..inputs()
            });
            assert_eq!(step, PowerOnStep::SendPowerOn, "connected must win");
        }
    }

    /// A second press while a launch is under way must not start a second
    /// Thetis. This one held before and still has to.
    #[test]
    fn a_second_press_during_a_launch_is_ignored() {
        let step = power_on_step(PowerOnInputs {
            launch_pending: true,
            ..inputs()
        });
        assert_eq!(step, PowerOnStep::Ignore);

        // Even with a shutdown still settling: the launch already queued will
        // deal with it, and two launches is worse than one that waits.
        let step = power_on_step(PowerOnInputs {
            launch_pending: true,
            process_running: true,
            shutdown_pending: true,
            ..inputs()
        });
        assert_eq!(step, PowerOnStep::Ignore);
    }

    /// Without a configured path there is nothing to launch, whatever the
    /// process table says. Send it and let it fail where someone can see it.
    #[test]
    fn without_a_path_it_sends_anyway() {
        let step = power_on_step(PowerOnInputs {
            have_path: false,
            process_running: true,
            shutdown_pending: true,
            ..inputs()
        });
        assert_eq!(step, PowerOnStep::SendPowerOn);
    }
}
