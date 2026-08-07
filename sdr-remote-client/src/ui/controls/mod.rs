// SPDX-License-Identifier: GPL-2.0-or-later

#![allow(dead_code, unused_imports)]
//! Unified control rendering module.
//!
//! Keeps a single source of truth per control group (band, mode, frequency, VFO).
//! All render paths (Tab::Radio, RX1/RX2 popout, joined popout) call the same
//! helpers with different `ControlContext` fields.
//!
//! Status: infrastructure (step 1 of PATCH-client-controls-refactor). No
//! render helpers implemented yet.

pub(crate) mod band;
pub(crate) mod context;
pub(crate) mod coverage;
pub(crate) mod events;
pub(crate) mod frequency;
pub(crate) mod mode;
pub(crate) mod state;

pub(crate) use band::{render_band_selector, BandClick, BANDS};
pub(crate) use frequency::{
    render_freq_step_controls, render_frequency_display, FreqStepAction,
    FrequencyDisplayAction, FREQ_STEPS, FREQ_STEP_LABELS,
};
pub(crate) use mode::{render_mode_selector, ModeClick, MODES_BASIC, MODES_EXTENDED};

pub(crate) use context::ControlContext;
pub(crate) use events::{
    begin_frame, current_frame, mono_ns_since_start, CommandBlockReason, IntentId, StampedEvent,
    TracingSink, UiEvent, UiEventSink, UiIntent,
};
#[cfg(any(test, feature = "ui-test"))]
pub(crate) use events::RecordingSink;
pub(crate) use state::{RxChannelState, SharedUiState};

/// Which RX channel a control addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RxChannel {
    Rx1,
    Rx2,
}

impl RxChannel {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            RxChannel::Rx1 => "Rx1",
            RxChannel::Rx2 => "Rx2",
        }
    }
}

/// Feature level: main screen (minimal controls, minimal data usage) vs
/// popout (superset with extended controls).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum UiDensity {
    Basic,
    Extended,
}

impl UiDensity {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            UiDensity::Basic => "Basic",
            UiDensity::Extended => "Extended",
        }
    }
}

/// Layout surface on which a control is rendered. Orthogonal to
/// `UiDensity` - the same density can occur on different surfaces
/// (e.g. a popout overlay above the main screen).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum UiSurface {
    MainTab,
    PopoutSeparate,
    PopoutJoined,
}

impl UiSurface {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            UiSurface::MainTab => "MainTab",
            UiSurface::PopoutSeparate => "PopoutSeparate",
            UiSurface::PopoutJoined => "PopoutJoined",
        }
    }
}
