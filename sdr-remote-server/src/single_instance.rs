// SPDX-License-Identifier: GPL-2.0-or-later
//! One server per PC.
//!
//! The port is fixed (`DEFAULT_PORT`), so a second server on the same PC can
//! never work - and it used to do harm on its way out: it signed in at the
//! relay as the station before it had the port, and two servers then pushed
//! each other out of the station slot every five seconds, with the client
//! talking to whichever held it ("the ghost of the FT-991A", 2026-08-21). The
//! client has had the same guard since build 33; this is the server's.

/// Held for the life of the process; the OS releases it when the process ends,
/// also after a crash, so a stale claim cannot block the next start. A named
/// mutex, as in the client: `CreateMutexW` reports `ERROR_ALREADY_EXISTS` when
/// another process holds the name.
pub struct Claim {
    #[cfg(windows)]
    handle: Option<windows::Win32::Foundation::HANDLE>,
    /// Why Windows could not be asked, when it could not. The window branch
    /// claims before any logger exists, so it logs this itself once it has one
    /// (build 16); the command line has its logger first and `claim` says it.
    unavailable: Option<String>,
}

impl Claim {
    pub fn unavailable(&self) -> Option<&str> {
        self.unavailable.as_deref()
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
            Ok(h) => Some(Claim { handle: Some(h), unavailable: None }),
            // Windows could not even be asked. Not a reason to refuse a start:
            // the port bind still decides, and says so in the log.
            Err(e) => {
                log::warn!("single-instance check unavailable ({e}); starting without it");
                Some(Claim { handle: None, unavailable: Some(e.to_string()) })
            }
        }
    }
}

/// Elsewhere there is one server per machine by habit, not by a mutex; the
/// port bind is the guard there.
#[cfg(not(windows))]
pub fn claim(_name: &str) -> Option<Claim> {
    Some(Claim { unavailable: None })
}

#[cfg(all(test, windows))]
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
}
