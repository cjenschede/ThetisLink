// SPDX-License-Identifier: GPL-2.0-or-later

//! State-splits: `RxChannelState` (per RX) + `SharedUiState`
//! (kanaal-overstijgend). Beide worden via `ControlContext` aan
//! render-helpers doorgegeven.
//!
//! Deze structs zijn bewust klein en bevatten alleen velden die in de
//! control-helpers gelezen of gewijzigd worden. Ze leven initieel naast de
//! bestaande `ThetisLinkApp`-velden; tijdens migratie worden app-velden er
//! stapsgewijs naartoe verplaatst.

/// Per-kanaal state die door control-helpers gelezen/gewijzigd wordt.
pub(crate) struct RxChannelState {
    pub(crate) frequency_hz: u64,
    pub(crate) mode: u8,
    pub(crate) freq_step_index: usize,
    pub(crate) freq_editing: bool,
    pub(crate) freq_edit_text: String,
    pub(crate) pending_freq_hz: Option<u64>,
}

impl RxChannelState {
    pub(crate) fn new() -> Self {
        Self {
            frequency_hz: 0,
            mode: 0,
            freq_step_index: 3,
            freq_editing: false,
            freq_edit_text: String::new(),
            pending_freq_hz: None,
        }
    }
}

/// Kanaal-overstijgende UI-state (VFO-sync, spectrum-scroll gating,
/// popout-layout flags, etc).
pub(crate) struct SharedUiState {
    pub(crate) vfo_sync: bool,
    pub(crate) spectrum_enabled: bool,
    pub(crate) popout_joined: bool,
}

impl SharedUiState {
    pub(crate) fn new() -> Self {
        Self {
            vfo_sync: false,
            spectrum_enabled: false,
            popout_joined: false,
        }
    }
}
