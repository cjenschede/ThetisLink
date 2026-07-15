// SPDX-License-Identifier: GPL-2.0-or-later

//! Reactieve RF-vermogen cap per Amplitec-A antenne-positie.
//!
//! Generieke variant van wat ooit begon als een JC-3s-specifieke
//! beveiliging: per Amplitec-A positie (1..6) kan een max FWD-watt
//! worden ingesteld via `config.amplitec_max_w`. Wanneer de actieve PA
//! tijdens TX boven die max komt, stuurt de controller de PA-eigen
//! `DriveDown` knop (dezelfde "−" knop als in het SPE/RF2K tabblad van
//! de client) totdat de FWD-meter onder de cap zit.
//!
//! Mode-multipliers (universeel toegepast op de positie-max):
//! - SSB/CW (LSB/USB/DSB/CWL/CWU): factor 1.0
//! - AM:                            factor 0.5
//! - FM / digital (DIGU/DIGL/SPEC/SAM/DRM/FM): factor 0.4
//!
//! Voorbeeld: positie A-2 met max 250 W -> SSB-cap 250 W, AM-cap 125 W,
//! FM/DIG-cap 100 W (de oude JC-3s waardes komen netjes uit deze
//! formule). Positie A-3 met max 1000 W -> 1000 / 500 / 400 W. Een
//! positie zonder ingestelde max (`None`) krijgt **geen** cap (PA loopt
//! vrij).
//!
//! **Niet** via Thetis ZZPC: de Thetis-drive is een TCI-loop tussen PA
//! en Thetis. Een ZZPC-verlaging vanuit de server wordt door de PA
//! direct teruggepushed. De PA-eigen DriveDown zit buiten die loop.
//!
//! Activatievoorwaarden - alle vier moeten waar zijn:
//! 1. Actieve Amplitec-A positie heeft `Some(max_w)` in de config.
//! 2. `config.active_pa` is 1 (SPE) of 2 (RF2K-S) - operator heeft één
//!    PA expliciet als de actieve gemarkeerd in de client.
//! 3. Die PA staat fysiek in Operate.
//! 4. De huidige Thetis-mode heeft een geldige factor (alle
//!    standaard-modes hebben een factor; modes zonder factor geven
//!    geen cap).
//!
//! Per cap-overschrijding gaat de PA-drive één stap omlaag (één
//! `DriveDown` commando). Rate-limit `MIN_ACTION_INTERVAL_MS` tussen
//! opeenvolgende stappen zodat de PA-meter kan settelen.
//!
//! Snapshot/restore lifecycle:
//! - Bij Amplitec-A switch NAAR een positie met max_w: per-PA
//!   stap-counter staat op 0.
//! - Tijdens cap-cycli: stap-counter loopt op met elke `DriveDown`.
//! - Bij Amplitec-A switch WEG (naar een andere positie, met of zonder
//!   max_w): per-PA evenveel `DriveUp` commando's gestuurd om de
//!   pre-cap drive-positie te herstellen. Counter wordt naar 0 gereset.
//!
//! De controller-state is een simpele `PowerCapState` die de tick-loop
//! tussen iteraties bewaart. Eén instantie per server-runtime, lokaal in
//! de `network.rs` broadcast-task.

use std::time::{Duration, Instant};

use log::info;

/// Minimaal interval tussen opeenvolgende drive-stappen. Moet langer
/// zijn dan de PA-meter settle-tijd zodat we niet meerdere `DriveDown`
/// commando's sturen voordat de FWD-meter merkbaar reageert. Empirisch
/// bevonden met SPE + RF2K-S: 1000 ms geeft één duidelijke stap per
/// seconde zonder overshoot.
pub const MIN_ACTION_INTERVAL_MS: u64 = 1000;

/// Welke PA wordt aangestuurd door de cap voor één tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerCapAction {
    /// Stuur `SpeCmd::DriveDown` naar de SPE Expert.
    SpeDriveDown,
    /// Stuur `Rf2kCmd::DriveDown` naar de RF2K-S.
    Rf2kDriveDown,
}

/// State van de power-cap controller. Eén instantie per server-runtime.
pub struct PowerCapState {
    /// Aantal `SpeCmd::DriveDown` commando's gestuurd terwijl een
    /// position-cap actief is en SPE de actieve PA was. Bij switch weg
    /// van die positie stuurt de caller evenveel `SpeCmd::DriveUp` om
    /// te herstellen.
    pub spe_drive_down_count: u32,
    /// Idem voor RF2K-S.
    pub rf2k_drive_down_count: u32,
    /// Tijdstip van de laatste cap-actie (DriveDown of restore-DriveUp).
    /// Gebruikt voor rate-limiting via `MIN_ACTION_INTERVAL_MS`.
    pub last_action_at: Option<Instant>,
    /// Vorige Amplitec-A positie; nodig om switch-transities te
    /// detecteren zonder een tweede shared state.
    pub prev_amplitec_pos: Option<u8>,
    /// Laatst gelogde state-snapshot (pos, mode, pa_in_operate, cap).
    /// Alleen loggen bij verandering - voorkomt periodieke "alles oké"
    /// spam in het server-log; transities blijven zichtbaar.
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

/// Mode-multiplier op de positie-max-W.
///
/// Returns `None` voor modes zonder gedefinieerde factor (controller
/// doet dan niets in die modes). De standaard Thetis-modes (LSB..DRM)
/// hebben allemaal een factor.
pub fn mode_factor(mode: u8) -> Option<f32> {
    match mode {
        // LSB (0), USB (1), DSB (2), CWL (3), CWU (4) - SSB + CW: 1.0
        0 | 1 | 2 | 3 | 4 => Some(1.0),
        // AM (6): 0.5 (carrier ~ half van PEP)
        6 => Some(0.5),
        // FM (5), DIGU (7), SPEC (8), DIGL (9), SAM (10), DRM (11): 0.4
        5 | 7 | 8 | 9 | 10 | 11 => Some(0.4),
        _ => None,
    }
}

/// Actuele cap voor een gegeven Amplitec-A positie + Thetis-mode.
///
/// Returns `Some(watts)` als de positie een max heeft EN de mode een
/// factor heeft; anders `None` (geen cap).
pub fn cap_for(amplitec_pos: u8, max_w_table: &[Option<u16>; 6], mode: u8) -> Option<u16> {
    if !(1..=6).contains(&amplitec_pos) {
        return None;
    }
    let max_w = max_w_table[(amplitec_pos - 1) as usize]?;
    let factor = mode_factor(mode)?;
    Some(((max_w as f32) * factor).round() as u16)
}

/// Per-tick cap-check. Roep aan vanuit de broadcast-loop bij elke
/// iteratie waarin de PA + Amplitec status verse waardes hebben.
/// Returnt `Some(PowerCapAction)` als de PA-drive omlaag moet (caller
/// stuurt dan het juiste `SpeCmd::DriveDown` of `Rf2kCmd::DriveDown`
/// commando); `None` betekent "geen actie".
///
/// Argumenten:
/// - `state` - gedeelde controller-state.
/// - `active_pos` - actuele Amplitec-A positie (1..6) of None.
/// - `max_w_table` - `config.amplitec_max_w` (6 Option<u16> waardes).
/// - `active_pa` - `config.active_pa` (0=none, 1=SPE, 2=RF2K).
/// - `pa_in_operate` - actieve PA staat in Operate.
/// - `pa_fwd_watts` - PA-meter waarde (None = sensor onbekend).
/// - `mode` - actuele Thetis `vfo_a_mode`.
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

    // State-change-log: alleen wanneer (pos, mode, pa_in_operate, cap)
    // is veranderd t.o.v. vorige tick. Operator wil status/geschiedenis
    // zien - transities, niet periodieke "alles nog steeds zo" rust-
    // spam. Bij rust = geen log; bij elke wijziging = één regel met
    // de relevante velden zodat de timeline reconstrueerbaar blijft.
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
    // Rate-limit: niet vaker dan eens per MIN_ACTION_INTERVAL_MS, zodat
    // de PA-meter en het DriveDown commando kunnen settelen.
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
            // active_pa=0 (none) - cap kan niets doen
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

/// Restore-directive bij Amplitec-A switch. Bevat het aantal `DriveUp`
/// commando's dat de caller naar elke PA moet sturen om de pre-cap
/// drive-positie te herstellen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerCapRestore {
    pub spe_drive_up: u32,
    pub rf2k_drive_up: u32,
}

/// Amplitec-A switch detector. Roep aan op elke broadcast-tick met de
/// actuele Amplitec-A positie. Returnt `Some(PowerCapRestore)` met
/// counters wanneer we wegschakelen van een positie waar tijdens deze
/// sessie DriveDown commando's zijn gestuurd. Bij switch (in elke
/// richting): de actieve cap-cyclus wordt afgesloten en herstart vers
/// op de nieuwe positie.
pub fn on_position_change(
    state: &mut PowerCapState,
    new_pos: Option<u8>,
) -> Option<PowerCapRestore> {
    let prev_pos = state.prev_amplitec_pos;
    state.prev_amplitec_pos = new_pos;
    if prev_pos == new_pos {
        return None;
    }
    // Iedere positie-wissel sluit de huidige cap-cyclus af. Restore
    // alle DriveDowns die voor de OUDE positie gestuurd waren.
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
        // Onbekend -> None
        assert_eq!(mode_factor(12), None);
        assert_eq!(mode_factor(255), None);
    }

    #[test]
    fn cap_for_handles_invalid_pos_and_no_cap() {
        let table: [Option<u16>; 6] = [Some(1000), None, Some(250), None, None, None];
        // Pos 0 en >6: geen panic, None
        assert_eq!(cap_for(0, &table, 0), None);
        assert_eq!(cap_for(7, &table, 0), None);
        // Pos zonder max_w
        assert_eq!(cap_for(2, &table, 0), None);
        // Pos met max, SSB
        assert_eq!(cap_for(1, &table, 0), Some(1000));
        // Pos met max, AM (factor 0.5)
        assert_eq!(cap_for(1, &table, 6), Some(500));
        // Pos met max, FM (factor 0.4)
        assert_eq!(cap_for(1, &table, 5), Some(400));
        // Mode zonder factor -> None
        assert_eq!(cap_for(1, &table, 12), None);
    }

    #[test]
    fn on_position_change_no_counters_returns_some_zero_restore() {
        // Switch zonder voorafgaande DriveDowns: returnt None
        // (counter-blok logt "no DriveDowns to restore"-pad).
        let mut state = PowerCapState::new();
        state.prev_amplitec_pos = Some(1);
        let result = on_position_change(&mut state, Some(2));
        assert!(result.is_none());
        assert_eq!(state.prev_amplitec_pos, Some(2));
    }

    #[test]
    fn on_position_change_returns_counters_and_resets() {
        // Counters opgebouwd op pos 1; switch naar pos 2 returnt restore
        // met identieke counts en reset de state.
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
        // Counters intact gebleven
        assert_eq!(state.spe_drive_down_count, 2);
    }
}
