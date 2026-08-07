// SPDX-License-Identifier: GPL-2.0-or-later

//! Reactive RF-power cap per Amplitec-A antenna position.
//!
//! Generic variant of what once started as a JC-3s-specific
//! protection: per Amplitec-A position (1..6) a max FWD-watt can
//! be set via `config.amplitec_max_w`. When the active PA
//! exceeds that max during TX, the controller drives the PA's own
//! `DriveDown` button (the same "−" button as in the SPE/RF2K tab of
//! the client) until the FWD-meter is below the cap.
//!
//! Mode-multipliers (universally applied to the position-max):
//! - SSB/CW (LSB/USB/DSB/CWL/CWU): factor 1.0
//! - AM:                            factor 0.5
//! - FM / digital (DIGU/DIGL/SPEC/SAM/DRM/FM): factor 0.4
//!
//! Example: position A-2 with max 250 W -> SSB-cap 250 W, AM-cap 125 W,
//! FM/DIG-cap 100 W (the old JC-3s values come out neatly from this
//! formula). Position A-3 with max 1000 W -> 1000 / 500 / 400 W. A
//! position without a configured max (`None`) gets **no** cap (PA runs
//! free).
//!
//! **Not** via Thetis ZZPC: the Thetis-drive is a TCI-loop between PA
//! and Thetis. A ZZPC-reduction from the server is pushed back by the PA
//! immediately. The PA's own DriveDown sits outside that loop.
//!
//! Activation conditions - all four must be true:
//! 1. Active Amplitec-A position has `Some(max_w)` in the config.
//! 2. `config.active_pa` is 1 (SPE) or 2 (RF2K-S) - operator has
//!    explicitly marked one PA as the active one in the client.
//! 3. That PA is physically in Operate.
//! 4. The current Thetis-mode has a valid factor (all
//!    standard modes have a factor; modes without a factor give
//!    no cap).
//!
//! Per cap-overshoot the PA-drive goes one step down (one
//! `DriveDown` command). Rate-limit `MIN_ACTION_INTERVAL_MS` between
//! successive steps so the PA-meter can settle.
//!
//! Snapshot/restore lifecycle:
//! - On Amplitec-A switch TO a position with max_w: per-PA
//!   step-counter is at 0.
//! - During cap-cycles: step-counter increases with each `DriveDown`.
//! - On Amplitec-A switch AWAY (to another position, with or without
//!   max_w): per-PA as many `DriveUp` commands are sent to restore the
//!   pre-cap drive-position. Counter is reset to 0.
//!
//! The controller-state is a simple `PowerCapState` that the tick-loop
//! preserves between iterations. One instance per server-runtime, local in
//! the `network.rs` broadcast-task.

use std::time::{Duration, Instant};

use log::info;

/// Minimum interval between successive drive-steps. Must be longer
/// than the PA-meter settle-time so we don't send multiple `DriveDown`
/// commands before the FWD-meter reacts noticeably. Empirically
/// found with SPE + RF2K-S: 1000 ms gives one clear step per
/// second without overshoot.
pub const MIN_ACTION_INTERVAL_MS: u64 = 1000;

/// Which PA is driven by the cap for one tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerCapAction {
    /// Send `SpeCmd::DriveDown` to the SPE Expert.
    SpeDriveDown,
    /// Send `Rf2kCmd::DriveDown` to the RF2K-S.
    Rf2kDriveDown,
}

/// State of the power-cap controller. One instance per server-runtime.
pub struct PowerCapState {
    /// Number of `SpeCmd::DriveDown` commands sent while a
    /// position-cap is active and SPE was the active PA. On switch away
    /// from that position the caller sends as many `SpeCmd::DriveUp` to
    /// restore.
    pub spe_drive_down_count: u32,
    /// Same for RF2K-S.
    pub rf2k_drive_down_count: u32,
    /// Timestamp of the last cap-action (DriveDown or restore-DriveUp).
    /// Used for rate-limiting via `MIN_ACTION_INTERVAL_MS`.
    pub last_action_at: Option<Instant>,
    /// Previous Amplitec-A position; needed to detect switch-transitions
    /// without a second shared state.
    pub prev_amplitec_pos: Option<u8>,
    /// Last logged state-snapshot (pos, mode, pa_in_operate, cap).
    /// Only log on change - prevents periodic "all OK"
    /// spam in the server-log; transitions stay visible.
    pub last_logged_snapshot: Option<(Option<u8>, u8, bool, Option<u16>)>,
}

impl PowerCapState {
    pub fn new() -> Self {
        Self {
            spe_drive_down_count: 0,
            rf2k_drive_down_count: 0,
            last_action_at: None,
            prev_amplitec_pos: None,
            last_logged_snapshot: None,
        }
    }
}

impl Default for PowerCapState {
    fn default() -> Self {
        Self::new()
    }
}

/// Mode-multiplier on the position-max-W.
///
/// Returns `None` for modes without a defined factor (controller
/// then does nothing in those modes). The standard Thetis-modes (LSB..DRM)
/// all have a factor.
pub fn mode_factor(mode: u8) -> Option<f32> {
    match mode {
        // LSB (0), USB (1), DSB (2), CWL (3), CWU (4) - SSB + CW: 1.0
        0 | 1 | 2 | 3 | 4 => Some(1.0),
        // AM (6): 0.5 (carrier ~ half of PEP)
        6 => Some(0.5),
        // FM (5), DIGU (7), SPEC (8), DIGL (9), SAM (10), DRM (11): 0.4
        5 | 7 | 8 | 9 | 10 | 11 => Some(0.4),
        _ => None,
    }
}

/// Current cap for a given Amplitec-A position + Thetis-mode.
///
/// Returns `Some(watts)` if the position has a max AND the mode has a
/// factor; otherwise `None` (no cap).
pub fn cap_for(amplitec_pos: u8, max_w_table: &[Option<u16>; 6], mode: u8) -> Option<u16> {
    if !(1..=6).contains(&amplitec_pos) {
        return None;
    }
    let max_w = max_w_table[(amplitec_pos - 1) as usize]?;
    let factor = mode_factor(mode)?;
    Some(((max_w as f32) * factor).round() as u16)
}

/// Per-tick cap-check. Call from the broadcast-loop on every
/// iteration where the PA + Amplitec status have fresh values.
/// Returns `Some(PowerCapAction)` if the PA-drive must go down (caller
/// then sends the correct `SpeCmd::DriveDown` or `Rf2kCmd::DriveDown`
/// command); `None` means "no action".
///
/// Arguments:
/// - `state` - shared controller-state.
/// - `active_pos` - current Amplitec-A position (1..6) or None.
/// - `max_w_table` - `config.amplitec_max_w` (6 Option<u16> values).
/// - `active_pa` - `config.active_pa` (0=none, 1=SPE, 2=RF2K).
/// - `pa_in_operate` - active PA is in Operate.
/// - `pa_fwd_watts` - PA-meter value (None = sensor unknown).
/// - `mode` - current Thetis `vfo_a_mode`.
pub fn tick(
    state: &mut PowerCapState,
    active_pos: Option<u8>,
    max_w_table: &[Option<u16>; 6],
    active_pa: u8,
    pa_in_operate: bool,
    pa_fwd_watts: Option<u16>,
    mode: u8,
) -> Option<PowerCapAction> {
    let cap = active_pos.and_then(|p| cap_for(p, max_w_table, mode));

    // State-change-log: only when (pos, mode, pa_in_operate, cap)
    // has changed relative to the previous tick. Operator wants to see
    // status/history - transitions, not periodic "still the same" idle
    // spam. When idle = no log; on every change = one line with
    // the relevant fields so the timeline stays reconstructable.
    let snapshot = (active_pos, mode, pa_in_operate, cap);
    if state.last_logged_snapshot != Some(snapshot) {
        if cap.is_some() || state.last_logged_snapshot.is_some() {
            info!(
                "PowerCap state: pos={:?} max_w={:?} active_pa={} pa_op={} mode={} cap={:?} spe_down={} rf2k_down={}",
                active_pos,
                active_pos.and_then(|p| max_w_table.get((p - 1) as usize).copied().flatten()),
                active_pa,
                pa_in_operate,
                crate::tci_parser::mode_u8_to_str(mode),
                cap,
                state.spe_drive_down_count,
                state.rf2k_drive_down_count,
            );
        }
        state.last_logged_snapshot = Some(snapshot);
    }

    let cap = cap?;
    if !pa_in_operate {
        return None;
    }
    let fwd = pa_fwd_watts?;
    if fwd <= cap {
        return None;
    }
    // Rate-limit: no more than once per MIN_ACTION_INTERVAL_MS, so
    // the PA-meter and the DriveDown command can settle.
    if let Some(last) = state.last_action_at {
        if last.elapsed() < Duration::from_millis(MIN_ACTION_INTERVAL_MS) {
            return None;
        }
    }
    let action = match active_pa {
        1 => {
            state.spe_drive_down_count = state.spe_drive_down_count.saturating_add(1);
            PowerCapAction::SpeDriveDown
        }
        2 => {
            state.rf2k_drive_down_count = state.rf2k_drive_down_count.saturating_add(1);
            PowerCapAction::Rf2kDriveDown
        }
        _ => {
            // active_pa=0 (none) - cap can't do anything
            return None;
        }
    };
    state.last_action_at = Some(Instant::now());
    info!(
        "PowerCap: pos={:?} + {} W cap exceeded (FWD={} W, mode={}); {:?} (counters spe={} rf2k={})",
        active_pos,
        cap,
        fwd,
        crate::tci_parser::mode_u8_to_str(mode),
        action,
        state.spe_drive_down_count,
        state.rf2k_drive_down_count,
    );
    Some(action)
}

/// Restore-directive on Amplitec-A switch. Contains the number of `DriveUp`
/// commands that the caller must send to each PA to restore the pre-cap
/// drive-position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerCapRestore {
    pub spe_drive_up: u32,
    pub rf2k_drive_up: u32,
}

/// Amplitec-A switch detector. Call on every broadcast-tick with the
/// current Amplitec-A position. Returns `Some(PowerCapRestore)` with
/// counters when we switch away from a position where DriveDown commands
/// were sent during this session. On switch (in either
/// direction): the active cap-cycle is closed and restarted fresh
/// on the new position.
pub fn on_position_change(
    state: &mut PowerCapState,
    new_pos: Option<u8>,
) -> Option<PowerCapRestore> {
    let prev_pos = state.prev_amplitec_pos;
    state.prev_amplitec_pos = new_pos;
    if prev_pos == new_pos {
        return None;
    }
    // Every position-change closes the current cap-cycle. Restore
    // all DriveDowns that were sent for the OLD position.
    let spe_up = state.spe_drive_down_count;
    let rf2k_up = state.rf2k_drive_down_count;
    state.spe_drive_down_count = 0;
    state.rf2k_drive_down_count = 0;
    if spe_up == 0 && rf2k_up == 0 {
        info!(
            "PowerCap: Amplitec-A switched {:?} -> {:?}, no DriveDowns to restore",
            prev_pos, new_pos
        );
        return None;
    }
    state.last_action_at = Some(Instant::now());
    info!(
        "PowerCap: Amplitec-A switched {:?} -> {:?}, restoring drive: {} × SpeDriveUp, {} × Rf2kDriveUp",
        prev_pos, new_pos, spe_up, rf2k_up
    );
    Some(PowerCapRestore {
        spe_drive_up: spe_up,
        rf2k_drive_up: rf2k_up,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_factor_covers_thetis_modes() {
        // SSB + CW: 1.0
        for m in [0u8, 1, 2, 3, 4] {
            assert_eq!(mode_factor(m), Some(1.0), "mode {} expected 1.0", m);
        }
        // AM: 0.5
        assert_eq!(mode_factor(6), Some(0.5));
        // FM/DIGU/SPEC/DIGL/SAM/DRM: 0.4
        for m in [5u8, 7, 8, 9, 10, 11] {
            assert_eq!(mode_factor(m), Some(0.4), "mode {} expected 0.4", m);
        }
        // Unknown -> None
        assert_eq!(mode_factor(12), None);
        assert_eq!(mode_factor(255), None);
    }

    #[test]
    fn cap_for_handles_invalid_pos_and_no_cap() {
        let table: [Option<u16>; 6] = [Some(1000), None, Some(250), None, None, None];
        // Pos 0 and >6: no panic, None
        assert_eq!(cap_for(0, &table, 0), None);
        assert_eq!(cap_for(7, &table, 0), None);
        // Pos without max_w
        assert_eq!(cap_for(2, &table, 0), None);
        // Pos with max, SSB
        assert_eq!(cap_for(1, &table, 0), Some(1000));
        // Pos with max, AM (factor 0.5)
        assert_eq!(cap_for(1, &table, 6), Some(500));
        // Pos with max, FM (factor 0.4)
        assert_eq!(cap_for(1, &table, 5), Some(400));
        // Mode without factor -> None
        assert_eq!(cap_for(1, &table, 12), None);
    }

    #[test]
    fn on_position_change_no_counters_returns_some_zero_restore() {
        // Switch without preceding DriveDowns: returns None
        // (counter-block logs the "no DriveDowns to restore" path).
        let mut state = PowerCapState::new();
        state.prev_amplitec_pos = Some(1);
        let result = on_position_change(&mut state, Some(2));
        assert!(result.is_none());
        assert_eq!(state.prev_amplitec_pos, Some(2));
    }

    #[test]
    fn on_position_change_returns_counters_and_resets() {
        // Counters built up on pos 1; switch to pos 2 returns restore
        // with identical counts and resets the state.
        let mut state = PowerCapState::new();
        state.prev_amplitec_pos = Some(1);
        state.spe_drive_down_count = 3;
        state.rf2k_drive_down_count = 0;
        let restore = on_position_change(&mut state, Some(2))
            .expect("expected restore");
        assert_eq!(restore.spe_drive_up, 3);
        assert_eq!(restore.rf2k_drive_up, 0);
        assert_eq!(state.spe_drive_down_count, 0);
        assert_eq!(state.rf2k_drive_down_count, 0);
        assert_eq!(state.prev_amplitec_pos, Some(2));
    }

    #[test]
    fn on_position_change_same_pos_is_noop() {
        let mut state = PowerCapState::new();
        state.prev_amplitec_pos = Some(1);
        state.spe_drive_down_count = 2;
        assert!(on_position_change(&mut state, Some(1)).is_none());
        // Counters stayed intact
        assert_eq!(state.spe_drive_down_count, 2);
    }
}
