// SPDX-License-Identifier: GPL-2.0-or-later

mod bridge;
mod logging;

#[cfg(target_os = "android")]
mod audio_oboe;

pub use logging::{init_logging, log_line, log_tail};

pub use bridge::{
    ble_drop_policy, ptt_button, relay_is_configured, version, BleDropPolicy, BlePttAction,
    BlePttGate, PhonePtt, PttButton, PttSource, PttTarget, BridgeChatAnswer, BridgeChatMessage, BridgeChatState, BridgeDxSpot, BridgeRadioState, BridgeRogerBeep,
    SdrBridge,
};

uniffi::include_scaffolding!("sdr_remote");
