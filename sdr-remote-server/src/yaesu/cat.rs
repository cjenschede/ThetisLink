// SPDX-License-Identifier: GPL-2.0-or-later
//! Low-level Yaesu CAT serial helpers: send-a-command-read-until-`;` query
//! (with timeout / diagnostic variants), the startup ID probe and the response
//! payload strip. Extracted verbatim from `yaesu/mod.rs` - pure relocation, no
//! behaviour/CAT/timing change. `use super::*;` pulls in the shared imports
//! (serialport, Duration/Instant, info/warn, RadioModel); `pub(super)` keeps the
//! query helpers callable from the poll loop, parse layer and memory module.

use super::*;

/// List available serial ports (reuse for UI combo box).
/// Send a CAT command and read response until `;` or timeout.
/// One CAT exchange: send a command, return whatever the radio answered.
///
/// Exists so command sequences can be exercised without a radio attached. The
/// FTX-1 memory/tone path in particular is written from its CAT manual and
/// cannot be tried on hardware here, and a wrong channel-recall form is
/// invisible in review but fatal on the wire - so the sequence is pinned by
/// tests through this trait instead.
pub(super) trait CatPort {
    fn query(&mut self, cmd: &str) -> String;

    /// Send a command that the radio does not answer (a "set"), without waiting
    /// for a reply. Using `query` for those burns the full read timeout on every
    /// call - 300 ms each, which is where a channel walk spends most of its time.
    /// Default: fall back to `query` so a test double only has to implement one
    /// method and still records the command.
    fn send(&mut self, cmd: &str) {
        let _ = self.query(cmd);
    }

    /// Discard whatever is still waiting in the input buffer.
    ///
    /// `cat_query` deliberately does NOT flush per call - that discards slow or
    /// in-flight answers and broke both radios' memory reads. But a walk that
    /// validates each answer against the channel it asked for has the opposite
    /// problem: anything already in the buffer predates the walk and can only
    /// mislead it. Draining ONCE, before such a walk starts, is safe for exactly
    /// that reason - there is nothing pending that we could still want.
    ///
    /// Default: no-op, so a test double keeps its scripted exchange.
    fn drain(&mut self) {}
}

impl CatPort for Box<dyn serialport::SerialPort> {
    fn query(&mut self, cmd: &str) -> String {
        cat_query(self, cmd)
    }

    fn drain(&mut self) {
        let mut buf = [0u8; 512];
        let mut dropped = 0usize;
        // Bounded: a runaway radio must not turn this into a spin. 64 reads is
        // far more than the backlog a bulk memory read can leave behind.
        for _ in 0..64 {
            match self.read(&mut buf) {
                Ok(n) if n > 0 => dropped += n,
                _ => break,
            }
        }
        if dropped > 0 {
            log::debug!("CAT drain: discarded {} stale byte(s) before a channel walk", dropped);
        }
    }

    fn send(&mut self, cmd: &str) {
        use std::io::Write;
        if let Err(e) = self.write_all(cmd.as_bytes()) {
            warn!("CAT send '{}' failed: {}", cmd, e);
        }
    }
}

pub(super) fn cat_query(port: &mut Box<dyn serialport::SerialPort>, cmd: &str) -> String {
    cat_query_impl(port, cmd, Duration::from_millis(300), false)
}

pub(super) fn cat_query_with_timeout(
    port: &mut Box<dyn serialport::SerialPort>,
    cmd: &str,
    timeout: Duration,
) -> String {
    cat_query_impl(port, cmd, timeout, false)
}

/// Like `cat_query_with_timeout` but logs WHY an empty response happened
/// (write error / read error kind / timeout). Used by the FTX-1 memory scan to
/// diagnose the "first read returns nothing" case.
pub(super) fn cat_query_diag(
    port: &mut Box<dyn serialport::SerialPort>,
    cmd: &str,
    timeout: Duration,
) -> String {
    cat_query_impl(port, cmd, timeout, true)
}

fn cat_query_impl(
    port: &mut Box<dyn serialport::SerialPort>,
    cmd: &str,
    timeout: Duration,
    diag: bool,
) -> String {
    // NB: do NOT flush the input buffer here. An input flush discards responses
    // that are slow (cold radio) or still in-flight, which broke BOTH radios'
    // memory reads (991A returned a partial list, FTX-1 returned nothing).
    // Misalignment on the FTX-1 is instead handled where it matters, by
    // echo-validating the response against the queried channel (read_all_memories_ftx1):
    // a leftover/wrong-channel response is rejected and re-read, so a lagged
    // response is captured on the retry instead of being thrown away.
    let mut raw_buf = [0u8; 512];
    if let Err(e) = port.write_all(cmd.as_bytes()) {
        if diag { warn!("cat_query {}: write error: {:?} ({})", cmd.trim_end_matches(';'), e.kind(), e); }
        return String::new();
    }
    let mut response = String::new();
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() > deadline {
            if diag && response.is_empty() {
                warn!("cat_query {}: TIMEOUT after {:?} - radio sent no bytes", cmd.trim_end_matches(';'), timeout);
            }
            break;
        }
        match port.read(&mut raw_buf) {
            Ok(n) if n > 0 => {
                if let Ok(s) = std::str::from_utf8(&raw_buf[..n]) {
                    response.push_str(s);
                    if response.contains(';') { break; }
                }
            }
            Ok(_) => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                if diag { warn!("cat_query {}: read error: {:?} ({}) - aborting query", cmd.trim_end_matches(';'), e.kind(), e); }
                break;
            }
        }
    }
    response
}

/// Startup probe: read `ID;` once and warn if the connected model does not match
/// the configured slot model. Normal releases avoid raw CAT dumps; detailed CAT
/// traces should use debug logging around the specific command path being tested.
/// Returns the model the radio itself reports, when its `ID;` answer names one
/// this build knows. The caller adopts it: the assumed model is a stand-in for
/// exactly as long as the radio has not spoken for itself.
pub(super) fn bringup_probe(
    port: &mut Box<dyn serialport::SerialPort>,
    prefix: &str,
    model: RadioModel,
) -> Option<RadioModel> {
    // Drained and asked three times, exactly like `detect_model` does at
    // startup - and for the same measured reason: the first read after opening
    // a port routinely comes back empty or partial. Asking once was enough to
    // lose a whole session. A station with the FTX-1 switched off at server
    // start (report #12, 2026-08-12) reconnected two minutes later, this probe
    // read "" on its single attempt, and the radio was driven as an FT-991A
    // for the rest of the evening: no memory channels, no menu values, V/M and
    // Mem+/- dead. The port has no conversation in flight at this point, so
    // draining here is safe (see the note in `cat_query` about why it is NOT
    // safe there).
    let _ = port.clear(serialport::ClearBuffer::Input);
    let mut id_resp = String::new();
    let mut id_code = String::new();
    for attempt in 0..3 {
        id_resp = cat_query(port, "ID;");
        id_code = resp_payload("ID", &id_resp);
        if RadioModel::from_id_code(&id_code).is_some() {
            break;
        }
        if attempt < 2 {
            log::debug!("{} ID; attempt {} gave {:?} - asking again", prefix, attempt + 1, id_resp);
            std::thread::sleep(Duration::from_millis(150));
        }
    }
    let detected = RadioModel::from_id_code(&id_code);

    if id_code.is_empty() {
        warn!("{} autodetect: ID; returned no valid response ({:?}); check cable/baud radio-menu vs config; falling back to shared Yaesu parser", prefix, id_resp);
    } else if detected.is_none() {
        warn!("{} unknown ID code '{}'; assuming Yaesu-compatible CAT dialect", prefix, id_code);
    } else if detected != Some(model) {
        // Detected model differs from the assumed slot model - a radio that was
        // off during startup detection, or a COM/USB enumeration swap. Loud,
        // because the audio-device assignment cannot be corrected from here.
        warn!(
            "{} model mismatch: assumed={:?}, radio reports {:?} (ID {}); adopting the radio's own dialect - if the audio sounds wrong, check the slot assignment",
            prefix, model, detected.unwrap(), id_code
        );
    }
    detected
}

/// Strip a 2-char CAT command echo + trailing `;` from a response, returning the
/// payload. `resp_payload("ID", "ID0670;")` -> `"0670"`. Tolerant of empty /
/// malformed input (returns what it can, never panics).
pub(super) fn resp_payload(cmd: &str, resp: &str) -> String {
    let t = resp.trim().trim_end_matches(';');
    t.strip_prefix(cmd).unwrap_or(t).to_string()
}

#[allow(dead_code)] // helper for UI port-picker; not yet used by config flow
pub fn available_ports() -> Vec<String> {
    serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.port_name)
        .collect()
}
