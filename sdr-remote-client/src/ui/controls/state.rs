// SPDX-License-Identifier: GPL-2.0-or-later

//! State split: `RxChannelState` (per RX) + `SharedUiState`
//! (cross-channel). Both are passed to render helpers via
//! `ControlContext`.
//!
//! These structs are deliberately small and only contain fields that are
//! read or modified in the control helpers. They initially live alongside the
//! existing `ThetisLinkApp` fields; during migration the app fields are
//! gradually moved over to them.

/// Per-channel state that is read/modified by the control helpers.
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

/// Cross-channel UI state (VFO sync, spectrum-scroll gating,
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
