// SPDX-License-Identifier: GPL-2.0-or-later

mod bridge;

#[cfg(target_os = "android")]
mod audio_oboe;

pub use bridge::{version, BridgeDxSpot, BridgeRadioState, SdrBridge};

uniffi::include_scaffolding!("sdr_remote");
