// SPDX-License-Identifier: GPL-2.0-or-later
//! One server per PC.
//!
//! The port is fixed (`DEFAULT_PORT`), so a second server on the same PC can
//! never work - and it used to do harm on its way out: it signed in at the
//! relay as the station before it had the port, and two servers then pushed
//! each other out of the station slot every five seconds, with the client
//! talking to whichever held it ("the ghost of the FT-991A", 2026-08-21). The
//! client has had the same guard since build 33; this is the server's.

use std::time::{Duration, Instant};

/// Held for the life of the process; the OS releases it when the process ends,
/// also after a crash, so a stale claim cannot block the next start. A named
/// mutex, as in the client: `CreateMutexW` reports `ERROR_ALREADY_EXISTS` when
/// another process holds the name.
pub struct Claim {
    #[cfg(windows)]
    handle: Option<windows::Win32::Foundation::HANDLE>,
    /// The locked file elsewhere; closing it (also by a crash) lets go.
    #[cfg(not(windows))]
    _lock: Option<std::fs::File>,
    /// Why the OS could not be asked (the mutex, or elsewhere the lock file), when it could not. The window branch
    /// claims before any logger exists, so it logs this itself once it has one
    /// (build 16); the command line has its logger first and `claim` says it.
    unavailable: Option<String>,
    /// How long `claim_patiently` waited for another holder to let go; zero
    /// when the name was free at once.
    waited: Duration,
}

impl Claim {
    pub fn unavailable(&self) -> Option<&str> {
        self.unavailable.as_deref()
    }

    pub fn waited(&self) -> Duration {
        self.waited
    }
}

/// How long a start waits for the name before it is refused. The holder can be
/// a server on its way out: the one this start replaces (its own restart
/// spawns the new process before it exits), or a window closed a moment ago,
/// which stops its server for up to 2.5 s first. The port gets the same
/// patience in `bind_with_retry`.
pub const PATIENCE: Duration = Duration::from_secs(5);
pub const PATIENCE_STEP: Duration = Duration::from_millis(250);

/// `claim`, asked again every `step` while another process holds the name,
/// until `patience` is over: `None` only when the name stayed taken all that
/// time.
pub fn claim_patiently(name: &str, patience: Duration, step: Duration) -> Option<Claim> {
    let start = Instant::now();
    let mut refused_before = false;
    loop {
        if let Some(mut c) = claim(name) {
            // Zero unless a step was waited out: the first ask takes
            // microseconds, and that is not a wait.
            if refused_before {
                c.waited = start.elapsed();
            }
            return Some(c);
        }
        refused_before = true;
        if start.elapsed() >= patience {
            return None;
        }
        std::thread::sleep(step);
    }
}

#[cfg(windows)]
impl Drop for Claim {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(h);
            }
        }
    }
}

/// What the log says when a start is refused. "Open", not "running": a
/// settings window with the server stopped holds the claim too, and the line
/// has to be true then as well.
pub const REFUSAL: &str = "another ThetisLink Server is already open on this PC";

/// The name both the guard and its test use: one per port, which today means
/// one per PC.
pub fn name() -> String {
    format!("ThetisLink-Server-Port-{}", sdr_remote_core::DEFAULT_PORT)
}

/// `Some` when this process is the only one holding `name`, `None` when
/// another already does.
#[cfg(windows)]
pub fn claim(name: &str) -> Option<Claim> {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    let name_w = HSTRING::from(name);
    unsafe {
        match CreateMutexW(None, false, PCWSTR(name_w.as_ptr())) {
            Ok(h) if GetLastError() == ERROR_ALREADY_EXISTS => {
                let _ = CloseHandle(h);
                None
            }
            Ok(h) => Some(Claim { handle: Some(h), unavailable: None, waited: Duration::ZERO }),
            // Windows could not even be asked. Not a reason to refuse a start:
            // the port bind still decides, and says so in the log.
            Err(e) => {
                log::warn!("single-instance check unavailable ({e}); starting without it");
                Some(Claim { handle: None, unavailable: Some(e.to_string()), waited: Duration::ZERO })
            }
        }
    }
}

/// Elsewhere an exclusive lock on a file next to the executable, where its
/// conf and log are, named after `name`: the OS drops the lock when the process
/// ends, also after a crash, so the file left behind blocks nothing. Not the
/// temporary directory: that follows `TMPDIR`, and a service with a private
/// `/tmp` would get a lock of its own.
#[cfg(not(windows))]
pub fn claim(name: &str) -> Option<Claim> {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let path = dir.join(format!("{name}.lock"));
    let unavailable = |e: std::io::Error| {
        log::warn!("single-instance check unavailable ({}: {e}); starting without it", path.display());
        Some(Claim { _lock: None, unavailable: Some(format!("{}: {e}", path.display())), waited: Duration::ZERO })
    };
    let file = match std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path) {
        Ok(f) => f,
        Err(e) => return unavailable(e),
    };
    match file.try_lock() {
        Ok(()) => Some(Claim { _lock: Some(file), unavailable: None, waited: Duration::ZERO }),
        Err(std::fs::TryLockError::WouldBlock) => None,
        Err(std::fs::TryLockError::Error(e)) => unavailable(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_claim_on_the_same_name_is_refused() {
        // Unique per run, so a server running on this PC does not interfere.
        let name = format!("ThetisLink-Server-Test-{}", std::process::id());
        let first = claim(&name);
        assert!(first.is_some(), "the first claim must succeed");
        assert!(claim(&name).is_none(), "a second claim while the first is held must be refused");
        drop(first);
        // And the second time round: once the first is let
        // go, as when that server is closed, the next start is let in again.
        let again = claim(&name);
        assert!(again.is_some(), "after the first claim is dropped, a new one must succeed");
        assert!(claim(&name).is_none(), "and it holds the name just like the first did");
    }

    #[test]
    fn a_patient_claim_on_a_free_name_has_not_waited() {
        let name = format!("ThetisLink-Server-Test-Free-{}", std::process::id());
        let c = claim_patiently(&name, Duration::from_secs(5), Duration::from_millis(20)).expect("a free name");
        assert!(c.waited().is_zero(), "nothing was waited for: {:?}", c.waited());
    }

    #[test]
    fn a_patient_claim_gets_the_name_once_the_holder_lets_go() {
        let name = format!("ThetisLink-Server-Test-Patient-{}", std::process::id());
        let first = claim(&name).expect("the first claim must succeed");
        // The waiting start runs beside the holder, as the new process does
        // beside the one it replaces. Only plain values cross the thread.
        let waiter_name = name.clone();
        let waiter = std::thread::spawn(move || {
            let c = claim_patiently(&waiter_name, Duration::from_secs(5), Duration::from_millis(20));
            (c.is_some(), c.map(|c| c.waited()).unwrap_or_default())
        });
        std::thread::sleep(Duration::from_millis(300));
        drop(first);
        let (got, waited) = waiter.join().expect("the waiting thread");
        assert!(got, "the name must come free once the holder lets go");
        assert!(waited >= Duration::from_millis(200), "it waited for the holder: {waited:?}");
        assert!(waited < Duration::from_secs(4), "and not the whole patience: {waited:?}");
    }

    #[test]
    fn a_patient_claim_is_refused_when_the_holder_stays() {
        let name = format!("ThetisLink-Server-Test-Stays-{}", std::process::id());
        let _first = claim(&name).expect("the first claim must succeed");
        let start = Instant::now();
        assert!(claim_patiently(&name, Duration::from_millis(300), Duration::from_millis(20)).is_none());
        assert!(start.elapsed() >= Duration::from_millis(300), "refused only after the patience");
    }
}
