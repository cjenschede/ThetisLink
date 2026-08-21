// SPDX-License-Identifier: GPL-2.0-or-later

use std::net::SocketAddr;
use std::sync::Mutex;

use log::{info, warn};
use tokio::sync::{mpsc, watch};

use sdr_remote_core::protocol::ControlId;
use sdr_remote_logic::audio::AudioBackend;
use sdr_remote_logic::commands::Command;
use sdr_remote_logic::engine::{ClientEngine, ClientRelayTunnel};
use sdr_remote_logic::state::RadioState;

/// Namespace function: returns shared version string (with build number in dev)
pub fn version() -> String {
    sdr_remote_core::version_string()
}

/// Is this relay actually set up to run?
///
/// The same rule the bridge uses to decide whether to build the tunnel, handed
/// to Compose rather than recomputed there. It was recomputed there, with three
/// of the four fields: with a relay switched on, an address and a station name
/// filled in but the token still empty, Kotlin called it configured while the
/// Rust side did not - so filling in the token afterwards changed nothing on
/// screen and the "restart to apply" notice never appeared, at exactly the
/// moment the relay started working (review finding, 2026-08-20).
pub fn relay_is_configured(enabled: bool, url: String, station: String, token: String) -> bool {
    sdr_remote_relay::is_configured(enabled, &url, &station, &token)
}

/// DX cluster spot exposed to Kotlin via uniffi.
pub struct BridgeDxSpot {
    pub callsign: String,
    pub frequency_hz: u64,
    pub mode: String,
    pub spotter: String,
    pub comment: String,
    pub age_seconds: u16,
    pub expiry_seconds: u16,
}

/// One chat message, as Compose draws it.
///
/// The two judgements that need the model to make them - is this ours, and may
/// it still be corrected - are answered here rather than left to Kotlin, so the
/// phone and the desktop cannot disagree about whose message it is.
pub struct BridgeChatMessage {
    pub id: i64,
    /// Unix seconds; the UI turns it into local time.
    pub at: i64,
    /// Empty for somebody who left the chat: their words stay, they do not.
    pub name: String,
    pub body: String,
    /// The message this one answers, or empty. Its author and first words come
    /// along, because a client only asks for what is new and cannot look up a
    /// message it never held.
    pub reply_name: String,
    pub reply_text: String,
    pub edited: bool,
    pub mine: bool,
    pub can_edit: bool,
}

/// One answer from the administrator on a problem report.
pub struct BridgeChatAnswer {
    pub id: i64,
    pub at: i64,
    pub body: String,
}

/// The roger beep, as the phone sets it.
///
/// A mirror of `sdr_remote_logic::roger::RogerBeep` rather than a re-export,
/// because UniFFI needs the type declared in this crate. The values are handed
/// straight over and clamped there, so a phone cannot ask for something the
/// desktop would refuse.
pub struct BridgeRogerBeep {
    pub freq_hz: f32,
    pub volume: f32,
    pub duration_ms: u32,
    pub include_fm: bool,
    pub on_thetis: bool,
    pub on_radio1: bool,
    pub on_radio2: bool,
}

/// The chat as one screen's worth of state.
pub struct BridgeChatState {
    /// 0 = reachable, 1 = no relay configured, 2 = a relay without a chat,
    /// 3 = nothing answering. Three reasons that need three different things
    /// from the user; one word covering all of them sends people to the
    /// maker's mailbox.
    pub offline_reason: u8,
    /// The service has said whether this station is in the chat. Until it has,
    /// neither the consent screen nor the conversation is shown - guessing
    /// wrong means showing somebody a consent form they already filled in.
    pub consent_known: bool,
    pub consented: bool,
    pub display_name: String,
    pub unread: u32,
    /// The service's own words for a refusal, or empty.
    pub error: String,
    /// How many problem reports this station may still send today; -1 when the
    /// service has not said. Known before the form is filled in, because being
    /// told at the send button is being told after the work.
    pub reports_left: i64,
    pub messages: Vec<BridgeChatMessage>,
    pub answers: Vec<BridgeChatAnswer>,
}

/// Radio state exposed to Kotlin via uniffi.
/// 1:1 mapping with RadioState fields.
pub struct BridgeRadioState {
    pub connected: bool,
    /// Fase 3c: relay audio on the wss TCP-fallback (true) vs low-latency UDP (false).
    /// Always false in direct mode. Overridden in get_state() from the relay status.
    pub relay_transport_fallback: bool,
    pub ptt_denied: bool,
    pub audio_error: bool,
    pub rtt_ms: u16,
    pub jitter_ms: f32,
    pub buffer_depth: u32,
    pub rx_packets: u64,
    pub yaesu_audio_packets: u64,
    pub yaesu_jitter_ms: f32,
    pub yaesu_buffer_depth: u32,
    pub yaesu2_audio_packets: u64,
    pub yaesu2_jitter_ms: f32,
    pub yaesu2_buffer_depth: u32,
    pub vrx1_audio_packets: u64,
    pub vrx1_jitter_ms: f32,
    pub vrx1_buffer_depth: u32,
    pub vrx2_audio_packets: u64,
    pub vrx2_jitter_ms: f32,
    pub vrx2_buffer_depth: u32,
    pub loss_percent: u8,
    pub down_kbps: u32,
    pub up_kbps: u32,
    pub dx_spots_enabled: bool,
    /// Does this server have a DX cluster at all? False only when the server
    /// says so, so an older server that cannot say still counts as having one.
    /// The toggle is hidden without one: a switch that promises a stream the
    /// server can never send is worse than no switch.
    pub dx_cluster_available: bool,
    /// How each Yaesu slot is named on screen: "Yaesu 1: FTX1" once the
    /// server has said what it is, plain "Yaesu 1" until then. Composed here
    /// from the shared rule rather than in Compose, so the phone and the
    /// desktop cannot end up naming the same radio differently.
    pub yaesu_label: String,
    pub yaesu2_label: String,
    pub capture_level: f32,
    /// TX level of the Yaesu chain - measured on the frame as encoded, so EQ,
    /// compressor and AGC are included. One field for both radios: only the
    /// selected one transmits. Without this the Yaesu panel showed the THETIS
    /// capture level, which sits at silence while a Yaesu is being keyed.
    pub yaesu_mic_level: f32,
    pub playback_level: f32,
    pub frequency_hz: u64,
    pub frequency_rx2_hz: u64,
    pub mode: u8,
    /// S-meter — dBm in RX, watts in TX (use `other_tx` / client PTT to
    /// disambiguate, same as the desktop client and wire protocol).
    pub smeter: f32,
    pub power_on: bool,
    pub tx_profile: u8,
    pub nr_level: u8,
    pub anf_on: bool,
    pub nb_level: u8,
    pub diversity_enabled: bool,
    pub diversity_phase: f32,
    pub diversity_gain_rx1: f32,
    pub diversity_gain_rx2: f32,
    pub diversity_ref: u8,
    pub diversity_source: u8,
    pub diversity_autonull_result: u16,
    pub drive_level: u8,
    pub rx_af_gain: u8,
    pub agc_enabled: bool,
    pub other_tx: bool,
    pub filter_low_hz: i32,
    pub filter_high_hz: i32,
    pub thetis_configured: bool,
    pub thetis_starting: bool,
    /// Server explicitly reports Thetis.exe is not running on the server PC.
    /// Drives the Thetis-autostart option; false when the server says nothing
    /// about the process (old server) - see `ConnectStatus::thetis_reported_not_running`.
    pub thetis_not_running: bool,
    pub tx_profile_names: Vec<String>,
    // Spectrum (extracted view)
    pub spectrum_bins: Vec<u8>,
    pub spectrum_center_hz: u32,
    pub spectrum_span_hz: u32,
    pub spectrum_ref_level: i8,
    pub spectrum_db_per_unit: u8,
    pub spectrum_sequence: u16,
    // Full DDC spectrum (for waterfall)
    pub full_spectrum_bins: Vec<u8>,
    pub full_spectrum_center_hz: u32,
    pub full_spectrum_span_hz: u32,
    pub full_spectrum_sequence: u16,
    // External equipment
    pub amplitec_connected: bool,
    pub amplitec_switch_a: u8,
    pub amplitec_switch_b: u8,
    pub amplitec_labels: String,
    // Tuner
    pub tuner_connected: bool,
    pub tuner_state: u8,
    pub tuner_can_tune: bool,
    // SPE Expert
    pub spe_connected: bool,
    pub spe_state: u8,
    pub spe_band: u8,
    pub spe_ptt: bool,
    pub spe_power_w: u16,
    pub spe_swr_x10: u16,
    pub spe_temp: u8,
    pub spe_warning: u8,
    pub spe_alarm: u8,
    pub spe_power_level: u8,
    pub spe_antenna: u8,
    pub spe_input: u8,
    pub spe_voltage_x10: u16,
    pub spe_current_x10: u16,
    pub spe_atu_bypassed: bool,
    pub spe_available: bool,
    pub spe_active: bool,
    // RF2K-S Amplifier
    pub rf2k_connected: bool,
    pub rf2k_operate: bool,
    pub rf2k_band: u8,
    pub rf2k_frequency_khz: u16,
    pub rf2k_temperature_x10: u16,
    pub rf2k_voltage_x10: u16,
    pub rf2k_current_x10: u16,
    pub rf2k_forward_w: u16,
    pub rf2k_reflected_w: u16,
    pub rf2k_swr_x100: u16,
    pub rf2k_max_forward_w: u16,
    pub rf2k_max_reflected_w: u16,
    pub rf2k_max_swr_x100: u16,
    pub rf2k_error_state: u8,
    pub rf2k_error_text: String,
    pub rf2k_antenna_type: u8,
    pub rf2k_antenna_number: u8,
    pub rf2k_tuner_mode: u8,
    pub rf2k_tuner_setup: String,
    pub rf2k_tuner_l_nh: u16,
    pub rf2k_tuner_c_pf: u16,
    pub rf2k_drive_w: u16,
    pub rf2k_modulation: String,
    pub rf2k_max_power_w: u16,
    pub rf2k_device_name: String,
    pub rf2k_available: bool,
    pub rf2k_active: bool,
    // Yaesu FT-991A
    pub yaesu_connected: bool,
    pub yaesu_freq_a: u64,
    pub yaesu_freq_b: u64,
    pub yaesu_mode: u8,
    pub yaesu_smeter: u16,
    pub yaesu_tx_active: bool,
    pub yaesu_power_on: bool,
    pub yaesu_af_gain: u8,
    pub yaesu_tx_power: u8,
    pub yaesu_squelch: u8,
    pub yaesu_rf_gain: u8,
    pub yaesu_mic_gain: u8,
    pub yaesu_vfo_select: u8,
    pub yaesu_memory_channel: u16,
    pub yaesu_split: bool,
    pub yaesu_scan: bool,
    pub playback_level_yaesu: f32,
    pub yaesu_memory_data: String,
    /// EX/menu values, in their OWN field. They used to share the memory field,
    /// told apart by a "MENU:" prefix - which worked only while the two never
    /// arrived together. Since both are pushed on connect they do, and whichever
    /// came second hid the other: the memory list appeared for a moment and then
    /// vanished behind the EX list.
    pub yaesu_menu_data: String,
    pub yaesu_model: u8,
    pub yaesu_tuner_state: u8,
    /// Radio meldt hoge SWR tijdens TX (zelf-wissend).
    pub yaesu_hi_swr: bool,
    /// Max TX-vermogen voor de huidige band (uit EX max-power menus; 0 = onbekend).
    /// De slider klemt hierop, net als de desktop.
    pub yaesu_tx_power_max: u8,
    // Radio 2 (yaesu2_*) — Android toont één radio tegelijk; de selector kiest welke.
    // De selector toont een radio alleen als 'ie connected is (= geconfigureerd + actief).
    pub yaesu2_connected: bool,
    pub yaesu2_model: u8,
    pub yaesu2_tuner_state: u8,
    pub yaesu2_hi_swr: bool,
    pub yaesu2_tx_power_max: u8,
    pub yaesu2_freq_a: u64,
    pub yaesu2_freq_b: u64,
    pub yaesu2_mode: u8,
    pub yaesu2_smeter: u16,
    pub yaesu2_tx_active: bool,
    pub yaesu2_power_on: bool,
    pub yaesu2_af_gain: u8,
    pub yaesu2_tx_power: u8,
    pub yaesu2_squelch: u8,
    pub yaesu2_rf_gain: u8,
    pub yaesu2_mic_gain: u8,
    pub yaesu2_vfo_select: u8,
    pub yaesu2_memory_channel: u16,
    pub yaesu2_split: bool,
    pub yaesu2_scan: bool,
    pub playback_level_yaesu2: f32,
    pub yaesu2_memory_data: String,
    pub yaesu2_menu_data: String,
    // DSP/functie-feature-state (beide slots): toggles bitfield, levels, freqs.
    pub yaesu_feature_toggles: u32,
    pub yaesu_feature_levels: Vec<u8>,
    pub yaesu_feature_freqs: Vec<u16>,
    pub yaesu2_feature_toggles: u32,
    pub yaesu2_feature_levels: Vec<u8>,
    pub yaesu2_feature_freqs: Vec<u16>,
    // UltraBeam RCU-06
    pub ub_connected: bool,
    pub ub_frequency_khz: u16,
    pub ub_band: u8,
    pub ub_direction: u8,
    pub ub_off_state: bool,
    pub ub_motors_moving: u8,
    pub ub_motor_completion: u16,
    pub ub_fw_major: u8,
    pub ub_fw_minor: u8,
    pub ub_available: bool,
    pub ub_elements_mm: Vec<u16>,
    // Rotor
    pub rotor_connected: bool,
    pub rotor_angle_x10: u16,
    pub rotor_rotating: bool,
    pub rotor_target_x10: u16,
    pub rotor_available: bool,
    // DX Cluster spots
    pub dx_spots: Vec<BridgeDxSpot>,
    // Auth (legacy bools — Compose UI should prefer connect_status_* fields below)
    pub auth_rejected: bool,
    pub totp_required: bool,
    // PATCH-1: connect-status as pre-rendered text. Single source of truth in
    // sdr-remote-logic::i18n (NL+EN). Compose UI just renders these strings.
    pub connect_status_headline: String,
    pub connect_status_action: String, // empty if no action hint
    pub connect_status_is_error: bool,
    pub connect_status_is_awaiting_totp: bool,
}

/// PATCH-1: build a BridgeRadioState with connect-status text rendered in the
/// caller-specified language. `From<RadioState>` defaults to English for
/// compatibility; call sites that know the user's language preference should
/// use this helper.
pub fn bridge_state_from_radio_state(
    s: RadioState,
    lang: sdr_remote_logic::i18n::Lang,
) -> BridgeRadioState {
    let (headline, action) = sdr_remote_logic::i18n::connect_status_text(
        &s.connect_status,
        lang,
        sdr_remote_logic::i18n::Platform::Mobile,
    );
    BridgeRadioState {
        connect_status_headline: headline,
        connect_status_action: action.unwrap_or_default(),
        connect_status_is_error: matches!(
            s.connect_status,
            sdr_remote_logic::state::ConnectStatus::Failed(_)
        ),
        connect_status_is_awaiting_totp: matches!(
            s.connect_status,
            sdr_remote_logic::state::ConnectStatus::AwaitingTotp
        ),
        ..BridgeRadioState::from(s)
    }
}

impl From<RadioState> for BridgeRadioState {
    fn from(s: RadioState) -> Self {
        Self {
            connected: s.connected,
            relay_transport_fallback: false, // set in get_state() from the relay status

            ptt_denied: s.ptt_denied,
            audio_error: s.audio_error,
            rtt_ms: s.rtt_ms,
            jitter_ms: s.jitter_ms,
            buffer_depth: s.buffer_depth,
            rx_packets: s.rx_packets,
            yaesu_audio_packets: s.yaesu_audio_packets,
            yaesu_jitter_ms: s.yaesu_jitter_ms,
            yaesu_buffer_depth: s.yaesu_buffer_depth,
            yaesu2_audio_packets: s.yaesu2_audio_packets,
            yaesu2_jitter_ms: s.yaesu2_jitter_ms,
            yaesu2_buffer_depth: s.yaesu2_buffer_depth,
            vrx1_audio_packets: s.vrx1_audio_packets,
            vrx1_jitter_ms: s.vrx1_jitter_ms,
            vrx1_buffer_depth: s.vrx1_buffer_depth,
            vrx2_audio_packets: s.vrx2_audio_packets,
            vrx2_jitter_ms: s.vrx2_jitter_ms,
            vrx2_buffer_depth: s.vrx2_buffer_depth,
            loss_percent: s.loss_percent,
            down_kbps: s.down_kbps,
            up_kbps: s.up_kbps,
            dx_spots_enabled: s.dx_spots_enabled,
            dx_cluster_available: s.dx_cluster_available,
            yaesu_label: sdr_remote_core::protocol::radio_slot_label(0, s.yaesu_model),
            yaesu2_label: sdr_remote_core::protocol::radio_slot_label(1, s.yaesu2_model),
            capture_level: s.capture_level,
            yaesu_mic_level: s.yaesu_mic_level,
            playback_level: s.playback_level,
            frequency_hz: s.frequency_hz,
            frequency_rx2_hz: s.frequency_rx2_hz,
            mode: s.mode,
            smeter: s.smeter,
            power_on: s.power_on,
            tx_profile: s.tx_profile,
            nr_level: s.nr_level,
            anf_on: s.anf_on,
            nb_level: s.nb_level,
            diversity_enabled: s.diversity_enabled,
            diversity_phase: (s.diversity_phase as i32 - 18000) as f32 / 100.0,
            diversity_gain_rx1: s.diversity_gain_rx1 as f32 / 1000.0,
            diversity_gain_rx2: s.diversity_gain_rx2 as f32 / 1000.0,
            diversity_ref: s.diversity_ref,
            diversity_source: s.diversity_source,
            diversity_autonull_result: s.diversity_autonull_result,
            drive_level: s.drive_level,
            rx_af_gain: s.rx_af_gain,
            agc_enabled: s.agc_enabled,
            other_tx: s.other_tx,
            filter_low_hz: s.filter_low_hz,
            filter_high_hz: s.filter_high_hz,
            thetis_configured: s.thetis_configured,
            thetis_starting: s.thetis_starting,
            thetis_not_running: s.connect_status.thetis_reported_not_running(),
            tx_profile_names: s.tx_profile_names,
            spectrum_bins: s.spectrum_bins.iter().map(|v| (v >> 8) as u8).collect(),
            spectrum_center_hz: s.spectrum_center_hz,
            spectrum_span_hz: s.spectrum_span_hz,
            spectrum_ref_level: s.spectrum_ref_level,
            spectrum_db_per_unit: s.spectrum_db_per_unit,
            spectrum_sequence: s.spectrum_sequence,
            full_spectrum_bins: s.full_spectrum_bins.iter().map(|v| (v >> 8) as u8).collect(),
            full_spectrum_center_hz: s.full_spectrum_center_hz,
            full_spectrum_span_hz: s.full_spectrum_span_hz,
            full_spectrum_sequence: s.full_spectrum_sequence,
            amplitec_connected: s.amplitec_connected,
            amplitec_switch_a: s.amplitec_switch_a,
            amplitec_switch_b: s.amplitec_switch_b,
            amplitec_labels: s.amplitec_labels,
            tuner_connected: s.tuner_connected,
            tuner_state: s.tuner_state,
            tuner_can_tune: s.tuner_can_tune,
            spe_connected: s.spe_connected,
            spe_state: s.spe_state,
            spe_band: s.spe_band,
            spe_ptt: s.spe_ptt,
            spe_power_w: s.spe_power_w,
            spe_swr_x10: s.spe_swr_x10,
            spe_temp: s.spe_temp,
            spe_warning: s.spe_warning,
            spe_alarm: s.spe_alarm,
            spe_power_level: s.spe_power_level,
            spe_antenna: s.spe_antenna,
            spe_input: s.spe_input,
            spe_voltage_x10: s.spe_voltage_x10,
            spe_current_x10: s.spe_current_x10,
            spe_atu_bypassed: s.spe_atu_bypassed,
            spe_available: s.spe_available,
            spe_active: s.spe_active,
            rf2k_connected: s.rf2k_connected,
            rf2k_operate: s.rf2k_operate,
            rf2k_band: s.rf2k_band,
            rf2k_frequency_khz: s.rf2k_frequency_khz,
            rf2k_temperature_x10: s.rf2k_temperature_x10,
            rf2k_voltage_x10: s.rf2k_voltage_x10,
            rf2k_current_x10: s.rf2k_current_x10,
            rf2k_forward_w: s.rf2k_forward_w,
            rf2k_reflected_w: s.rf2k_reflected_w,
            rf2k_swr_x100: s.rf2k_swr_x100,
            rf2k_max_forward_w: s.rf2k_max_forward_w,
            rf2k_max_reflected_w: s.rf2k_max_reflected_w,
            rf2k_max_swr_x100: s.rf2k_max_swr_x100,
            rf2k_error_state: s.rf2k_error_state,
            rf2k_error_text: s.rf2k_error_text,
            rf2k_antenna_type: s.rf2k_antenna_type,
            rf2k_antenna_number: s.rf2k_antenna_number,
            rf2k_tuner_mode: s.rf2k_tuner_mode,
            rf2k_tuner_setup: s.rf2k_tuner_setup,
            rf2k_tuner_l_nh: s.rf2k_tuner_l_nh,
            rf2k_tuner_c_pf: s.rf2k_tuner_c_pf,
            rf2k_drive_w: s.rf2k_drive_w,
            rf2k_modulation: s.rf2k_modulation,
            rf2k_max_power_w: s.rf2k_max_power_w,
            rf2k_device_name: s.rf2k_device_name,
            rf2k_available: s.rf2k_available,
            rf2k_active: s.rf2k_active,
            yaesu_connected: s.yaesu_connected,
            yaesu_freq_a: s.yaesu_freq_a,
            yaesu_freq_b: s.yaesu_freq_b,
            yaesu_mode: s.yaesu_mode,
            yaesu_smeter: s.yaesu_smeter,
            yaesu_tx_active: s.yaesu_tx_active,
            yaesu_power_on: s.yaesu_power_on,
            yaesu_af_gain: s.yaesu_af_gain,
            yaesu_tx_power: s.yaesu_tx_power,
            yaesu_squelch: s.yaesu_squelch,
            yaesu_rf_gain: s.yaesu_rf_gain,
            yaesu_mic_gain: s.yaesu_mic_gain,
            yaesu_vfo_select: s.yaesu_vfo_select,
            yaesu_memory_channel: s.yaesu_memory_channel,
            yaesu_split: s.yaesu_split,
            yaesu_scan: s.yaesu_scan,
            playback_level_yaesu: s.playback_level_yaesu,
            yaesu_memory_data: s.yaesu_memory_data.clone().unwrap_or_default(),
            yaesu_menu_data: s.yaesu_menu_data.clone().unwrap_or_default(),
            yaesu_model: s.yaesu_model,
            yaesu_tuner_state: s.yaesu_tuner_state,
            yaesu_hi_swr: s.yaesu_hi_swr,
            yaesu_tx_power_max: s.yaesu_tx_power_max,
            yaesu2_connected: s.yaesu2_connected,
            yaesu2_model: s.yaesu2_model,
            yaesu2_tuner_state: s.yaesu2_tuner_state,
            yaesu2_hi_swr: s.yaesu2_hi_swr,
            yaesu2_tx_power_max: s.yaesu2_tx_power_max,
            yaesu2_freq_a: s.yaesu2_freq_a,
            yaesu2_freq_b: s.yaesu2_freq_b,
            yaesu2_mode: s.yaesu2_mode,
            yaesu2_smeter: s.yaesu2_smeter,
            yaesu2_tx_active: s.yaesu2_tx_active,
            yaesu2_power_on: s.yaesu2_power_on,
            yaesu2_af_gain: s.yaesu2_af_gain,
            yaesu2_tx_power: s.yaesu2_tx_power,
            yaesu2_squelch: s.yaesu2_squelch,
            yaesu2_rf_gain: s.yaesu2_rf_gain,
            yaesu2_mic_gain: s.yaesu2_mic_gain,
            yaesu2_vfo_select: s.yaesu2_vfo_select,
            yaesu2_memory_channel: s.yaesu2_memory_channel,
            yaesu2_split: s.yaesu2_split,
            yaesu2_scan: s.yaesu2_scan,
            playback_level_yaesu2: s.playback_level_yaesu2,
            yaesu2_memory_data: s.yaesu2_memory_data.clone().unwrap_or_default(),
            yaesu2_menu_data: s.yaesu2_menu_data.clone().unwrap_or_default(),
            yaesu_feature_toggles: s.yaesu_feature_toggles,
            yaesu_feature_levels: s.yaesu_feature_levels.to_vec(),
            yaesu_feature_freqs: s.yaesu_feature_freqs.to_vec(),
            yaesu2_feature_toggles: s.yaesu2_feature_toggles,
            yaesu2_feature_levels: s.yaesu2_feature_levels.to_vec(),
            yaesu2_feature_freqs: s.yaesu2_feature_freqs.to_vec(),
            ub_connected: s.ub_connected,
            ub_frequency_khz: s.ub_frequency_khz,
            ub_band: s.ub_band,
            ub_direction: s.ub_direction,
            ub_off_state: s.ub_off_state,
            ub_motors_moving: s.ub_motors_moving,
            ub_motor_completion: s.ub_motor_completion,
            ub_fw_major: s.ub_fw_major,
            ub_fw_minor: s.ub_fw_minor,
            ub_available: s.ub_available,
            ub_elements_mm: s.ub_elements_mm.to_vec(),
            rotor_connected: s.rotor_connected,
            rotor_angle_x10: s.rotor_angle_x10,
            rotor_rotating: s.rotor_rotating,
            rotor_target_x10: s.rotor_target_x10,
            rotor_available: s.rotor_available,
            dx_spots: s.dx_spots.iter().map(|spot| {
                let total_age = spot.age_seconds as u64
                    + spot.received.elapsed().as_secs().min(u16::MAX as u64);
                BridgeDxSpot {
                    callsign: spot.callsign.clone(),
                    frequency_hz: spot.frequency_hz,
                    mode: spot.mode.clone(),
                    spotter: spot.spotter.clone(),
                    comment: spot.comment.clone(),
                    age_seconds: (total_age as u16).min(spot.expiry_seconds),
                    expiry_seconds: spot.expiry_seconds,
                }
            }).collect(),
            auth_rejected: s.auth_rejected,
            totp_required: s.totp_required,
            // PATCH-1: render connect_status text once in Rust so Android Compose
            // UI is automatically lockstep with desktop egui UI on NL/EN strings.
            // TODO(PATCH-1 follow-up): expose Lang as user config + OS-locale detect.
            connect_status_headline: {
                let (h, _) = sdr_remote_logic::i18n::connect_status_text(
                    &s.connect_status,
                    sdr_remote_logic::i18n::Lang::En,
                    sdr_remote_logic::i18n::Platform::Mobile,
                );
                h
            },
            connect_status_action: {
                let (_, a) = sdr_remote_logic::i18n::connect_status_text(
                    &s.connect_status,
                    sdr_remote_logic::i18n::Lang::En,
                    sdr_remote_logic::i18n::Platform::Mobile,
                );
                a.unwrap_or_default()
            },
            connect_status_is_error: matches!(
                s.connect_status,
                sdr_remote_logic::state::ConnectStatus::Failed(_)
            ),
            connect_status_is_awaiting_totp: matches!(
                s.connect_status,
                sdr_remote_logic::state::ConnectStatus::AwaitingTotp
            ),
        }
    }
}

/// Platform-specific audio factory.
/// On Android: creates OboeAudioBackend.
/// On other platforms: returns error (for cargo check only).
#[cfg(target_os = "android")]
fn make_audio(
    _input: Option<&str>,
    _output: Option<&str>,
) -> anyhow::Result<Box<dyn AudioBackend>> {
    let audio = crate::audio_oboe::OboeAudioBackend::new()?;
    Ok(Box::new(audio))
}

#[cfg(not(target_os = "android"))]
fn make_audio(
    _input: Option<&str>,
    _output: Option<&str>,
) -> anyhow::Result<Box<dyn AudioBackend>> {
    anyhow::bail!("Audio not available on this platform (Android only)")
}

/// Bridge between Kotlin/uniffi and the Rust ClientEngine.
/// Wraps engine lifecycle, command forwarding, and state polling.
/// The stop handle of the last relay monitor started in this process, so a new
/// one can put the old one down. See the comment where it is set.
static PREVIOUS_MONITOR: Mutex<Option<sdr_remote_relay::RelayStopHandle>> = Mutex::new(None);

pub struct SdrBridge {
    cmd_tx: mpsc::UnboundedSender<Command>,
    state_rx: Mutex<watch::Receiver<RadioState>>,
    shutdown_tx: Mutex<Option<watch::Sender<bool>>>,
    /// UI language for connect-status / connect-error rendering in `get_state()`.
    /// Set via `set_language`, which the app calls once at startup with the language
    /// Android resolved `strings.xml` to, so this side cannot drift from that one.
    /// Defaults to "en" for the window before that call.
    ui_language: Mutex<String>,
    /// Phase C: houdt de relay-monitor in leven zolang de bridge bestaat (draait op
    /// een eigen thread). `None` in direct-modus.
    _relay_monitor: Mutex<Option<sdr_remote_relay::RelayMonitor>>,
    /// Fase 3c: relay status handle to surface the transport (UDP / wss-fallback) to the
    /// Compose UI. `None` in direct mode.
    relay_status: Option<sdr_remote_relay::RelayStatusHandle>,
    /// The relay address this bridge was built with, exactly as configured -
    /// including when the relay is switched off. Used for redaction: the one host
    /// a problem report must never carry is the one that is written down, whether
    /// or not it is in use.
    relay_url: String,
    /// The relay address the chat may work with: the same one, but empty unless
    /// the relay is actually running this session.
    ///
    /// The two are separate because an address that is merely written down is not
    /// a relay. Deriving the chat endpoint from `relay_url` meant that a phone
    /// with the relay switched off but an address still in its settings was told
    /// "this relay offers no chat" - blaming a relay it was not talking to -
    /// where the honest answer is that no relay is configured.
    chat_relay_url: String,
    /// The chat, exactly as the desktop holds it. Compose asks for its state on
    /// a timer and calls the same handful of verbs; the worker thread inside
    /// does the network, so nothing here can get between an operator and PTT.
    chat: Mutex<sdr_remote_chat::ChatModel>,
}

impl SdrBridge {
    pub fn new(
        relay_enabled: bool,
        relay_url: String,
        relay_station: String,
        relay_token: String,
        relay_instance: String,
        relay_name: String,
        relay_udp_enabled: bool,
    ) -> Self {
        #[cfg(target_os = "android")]
        {
            android_logger::init_once(
                android_logger::Config::default()
                    .with_max_level(log::LevelFilter::Info)
                    .with_tag("ThetisLink"),
            );
        }

        let (engine, state_rx, cmd_tx) = ClientEngine::new();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        // Phase C: relay-transport voor Android-clients die niet kunnen port-forwarden
        // (mobiel achter CGNAT). Is de relay-config compleet, dan tunnel + monitor
        // (rol Client) opzetten; anders direct-UDP (default, byte-identiek).
        let mut relay_monitor: Option<sdr_remote_relay::RelayMonitor> = None;
        // One condition, read twice: it decides both whether the tunnel is built
        // and whether the chat has a relay to reach. Splitting them is how the
        // chat came to blame a relay that was switched off.
        let relay_active =
            sdr_remote_relay::is_configured(relay_enabled, &relay_url, &relay_station, &relay_token);
        let relay_tunnel = if relay_active {
            let (uplink_tx, uplink_rx) = mpsc::unbounded_channel();
            let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
            // Placeholder server-adres (display-label; genegeerd door de Relay-transport).
            let server_placeholder = SocketAddr::from(([203, 0, 113, 1], 4580));
            let relay_cfg = sdr_remote_relay::RelayConfig {
                enabled: true,
                url: relay_url.clone(),
                station: relay_station,
                token: relay_token,
                role: sdr_remote_relay::RelayRole::Client,
                instance: relay_instance,
                name: relay_name,
                // Fase 5: audio + PTT over kale UDP when the user leaves it enabled
                // (default). Off -> audio stays on the encrypted wss channel (the user's
                // latency-vs-encryption choice, matching the desktop toggle). The relay
                // lib handles UDP transparently and falls back to wss if it can't open.
                udp_port: if relay_udp_enabled {
                    Some(sdr_remote_relay::DEFAULT_UDP_PORT)
                } else {
                    None
                },
            };
            let tunnel = sdr_remote_relay::RelayTunnel {
                sentinel: server_placeholder,
                inbound_tx,
                uplink_rx,
            };
            let monitor = sdr_remote_relay::RelayMonitor::start_threaded_tunnel(relay_cfg, tunnel);
            // Only one relay monitor may live in this process. Android can build
            // a second bridge while the first is still running - the phone was
            // seen with two, three seconds apart, after switching network - and
            // both carry the same install id. The relay hands a returning client
            // its own slot back and closes the older connection, so two live
            // monitors evict each other every five seconds for as long as the
            // app runs. Control traffic squeezes through the gaps, which is why
            // the S-meter kept working while audio and spectrum never got a
            // stable path, and why only killing the app helped: that took both
            // monitors with it (2026-08-17).
            //
            // Stopping the older one here rather than trusting the owner of it
            // to do so: whoever forgets, the invariant holds.
            if let Some(previous) = PREVIOUS_MONITOR.lock().unwrap().replace(monitor.stop_handle()) {
                warn!("a relay monitor was still running - stopping it, or the two would evict each other");
                previous.stop();
            }
            relay_monitor = Some(monitor);
            // Not the address itself. It is the one thing a report must never
            // carry, and the relay library already writes <relay> in its own
            // status lines - printing it raw here made the kept log the only
            // place it appeared in plain text. Which relay is configured tells
            // nobody anything they can act on anyway: the recipient of a report
            // sees <relay> either way (2026-08-17).
            info!("Android transport: relay tunnel (via <relay>)");
            Some(ClientRelayTunnel {
                uplink_tx,
                inbound_rx,
                server_addr: server_placeholder,
            })
        } else {
            None
        };

        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
            rt.block_on(async {
                if let Err(e) = engine.run(make_audio, shutdown_rx, relay_tunnel).await {
                    log::error!("Engine error: {}", e);
                }
            });
            info!("Engine thread exited");
        });

        // Capture the status handle before the monitor is moved into the struct, so
        // get_state() can report the live transport (UDP vs wss-fallback).
        let relay_status = relay_monitor.as_ref().map(|m| m.status_handle());
        Self {
            cmd_tx,
            state_rx: Mutex::new(state_rx),
            shutdown_tx: Mutex::new(Some(shutdown_tx)),
            ui_language: Mutex::new("en".to_string()),
            _relay_monitor: Mutex::new(relay_monitor),
            relay_status,
            chat_relay_url: if relay_active {
                relay_url.clone()
            } else {
                String::new()
            },
            relay_url,
            chat: Mutex::new(sdr_remote_chat::ChatModel::default()),
        }
    }

    /// Set the UI language used for connect-status / connect-error text in
    /// `get_state()`. Accepts any code `Lang::from_code` knows - "en", "nl",
    /// "de", "fr" - and stores English for anything else. Compose should call
    /// this once on startup with the language it is itself rendering in.
    ///
    /// It used to keep only "nl" and flatten everything else to "en", so a
    /// German or French phone kept getting English connect text even once
    /// those translations existed (2026-08-20).
    ///
    /// The app calls it once at startup with the language Android resolved
    /// `strings.xml` to, so this side cannot drift from the strings around it.
    pub fn set_language(&self, lang: String) {
        *self.ui_language.lock().unwrap() = sdr_remote_logic::i18n::Lang::from_code(&lang)
            .code()
            .to_string();
    }

    pub fn connect(&self, addr: String, password: String) {
        let pw = if password.is_empty() { None } else { Some(password) };
        let _ = self.cmd_tx.send(Command::Connect(addr, pw));
        // Android: request 128K FFT for faster spectrum refresh
        let _ = self.cmd_tx.send(Command::SetSpectrumFftSize(128));
    }

    pub fn send_totp_code(&self, code: String) {
        let _ = self.cmd_tx.send(Command::SendTotpCode(code));
    }

    pub fn disconnect(&self) {
        let _ = self.cmd_tx.send(Command::Disconnect);
    }

    pub fn set_ptt(&self, active: bool) {
        let _ = self.cmd_tx.send(Command::SetPtt(active));
    }

    /// Hand the beep settings to the engine.
    ///
    /// Everything that makes a roger beep work - the tone, which modes it
    /// belongs in, holding PTT until it has gone out - is in the engine this
    /// app already runs. Nothing of it is duplicated here; this is the way in.
    pub fn set_roger_beep(&self, beep: BridgeRogerBeep) {
        let _ = self.cmd_tx.send(Command::SetRogerBeep(
            sdr_remote_logic::roger::RogerBeep {
                freq_hz: beep.freq_hz,
                volume: beep.volume,
                duration_ms: beep.duration_ms,
                include_fm: beep.include_fm,
                on_thetis: beep.on_thetis,
                on_radio1: beep.on_radio1,
                on_radio2: beep.on_radio2,
            }
            .clamped(),
        ));
    }

    pub fn set_mic_gate_delay_ms(&self, delay_ms: u32) {
        let _ = self.cmd_tx.send(Command::SetMicGateDelayMs(delay_ms));
    }

    pub fn set_playback_mute(&self, mute: bool) {
        let _ = self.cmd_tx.send(Command::SetPlaybackMute(mute));
    }

    pub fn set_dx_spots_enabled(&self, enabled: bool) {
        let _ = self.cmd_tx.send(Command::SetDxSpotsEnabled(enabled));
    }

    pub fn set_rx_volume(&self, volume: f32) {
        let _ = self.cmd_tx.send(Command::SetRxVolume(volume));
    }

    /// Local RX1 playback volume - client-only, independent of the Thetis AF gain
    /// (ZZLA) and of the master. This is how Thetis audio is silenced while a Yaesu
    /// is being listened to: the master would silence the Yaesu along with it.
    pub fn set_vfo_a_volume(&self, volume: f32) {
        let _ = self.cmd_tx.send(Command::SetVfoAVolume(volume));
    }

    /// Local RX2 playback volume, counterpart of `set_vfo_a_volume`.
    pub fn set_vfo_b_volume(&self, volume: f32) {
        let _ = self.cmd_tx.send(Command::SetVfoBVolume(volume));
    }

    pub fn set_local_volume(&self, volume: f32) {
        let _ = self.cmd_tx.send(Command::SetLocalVolume(volume));
    }

    pub fn set_tx_gain(&self, gain: f32) {
        let _ = self.cmd_tx.send(Command::SetTxGain(gain));
    }

    pub fn set_frequency(&self, hz: u64) {
        let _ = self.cmd_tx.send(Command::SetFrequency(hz));
    }

    pub fn set_mode(&self, mode: u8) {
        let _ = self.cmd_tx.send(Command::SetMode(mode));
    }

    pub fn set_agc_enabled(&self, enabled: bool) {
        let _ = self.cmd_tx.send(Command::SetAgcEnabled(enabled));
    }

    pub fn set_control(&self, control_id: u8, value: u16) {
        if let Some(id) = ControlId::from_u8(control_id) {
            let _ = self.cmd_tx.send(Command::SetControl(id, value));
        }
    }

    pub fn enable_spectrum(&self, enabled: bool) {
        let _ = self.cmd_tx.send(Command::EnableSpectrum(enabled));
    }

    pub fn set_spectrum_fps(&self, fps: u8) {
        let _ = self.cmd_tx.send(Command::SetSpectrumFps(fps));
    }

    pub fn set_spectrum_max_bins(&self, bins: u16) {
        let _ = self.cmd_tx.send(Command::SetSpectrumMaxBins(bins));
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::Rx2SpectrumMaxBins, bins));
    }

    pub fn set_spectrum_zoom(&self, zoom: f32) {
        let _ = self.cmd_tx.send(Command::SetSpectrumZoom(zoom));
    }

    pub fn set_spectrum_pan(&self, pan: f32) {
        let _ = self.cmd_tx.send(Command::SetSpectrumPan(pan));
    }

    pub fn set_amplitec_switch_a(&self, pos: u8) {
        let _ = self.cmd_tx.send(Command::SetAmplitecSwitchA(pos));
    }

    pub fn set_amplitec_switch_b(&self, pos: u8) {
        let _ = self.cmd_tx.send(Command::SetAmplitecSwitchB(pos));
    }

    pub fn tuner_tune(&self) {
        let _ = self.cmd_tx.send(Command::TunerTune);
    }

    pub fn tuner_abort(&self) {
        let _ = self.cmd_tx.send(Command::TunerAbort);
    }

    pub fn spe_operate(&self) {
        let _ = self.cmd_tx.send(Command::SpeOperate);
    }

    pub fn spe_tune(&self) {
        let _ = self.cmd_tx.send(Command::SpeTune);
    }

    pub fn spe_antenna(&self) {
        let _ = self.cmd_tx.send(Command::SpeAntenna);
    }

    pub fn spe_input(&self) {
        let _ = self.cmd_tx.send(Command::SpeInput);
    }

    pub fn spe_power(&self) {
        let _ = self.cmd_tx.send(Command::SpePower);
    }

    pub fn spe_band_up(&self) {
        let _ = self.cmd_tx.send(Command::SpeBandUp);
    }

    pub fn spe_band_down(&self) {
        let _ = self.cmd_tx.send(Command::SpeBandDown);
    }

    pub fn spe_off(&self) {
        let _ = self.cmd_tx.send(Command::SpeOff);
    }

    pub fn spe_power_on(&self) {
        let _ = self.cmd_tx.send(Command::SpePowerOn);
    }

    pub fn spe_drive_down(&self) {
        let _ = self.cmd_tx.send(Command::SpeDriveDown);
    }

    pub fn spe_drive_up(&self) {
        let _ = self.cmd_tx.send(Command::SpeDriveUp);
    }

    pub fn rf2k_operate(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::Rf2kOperate(on));
    }

    pub fn rf2k_tune(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kTune);
    }

    pub fn rf2k_ant1(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kAnt1);
    }

    pub fn rf2k_ant2(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kAnt2);
    }

    pub fn rf2k_ant3(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kAnt3);
    }

    pub fn rf2k_ant4(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kAnt4);
    }

    pub fn rf2k_ant_ext(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kAntExt);
    }

    pub fn rf2k_error_reset(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kErrorReset);
    }

    pub fn rf2k_close(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kClose);
    }

    pub fn rf2k_drive_up(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kDriveUp);
    }

    pub fn rf2k_drive_down(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kDriveDown);
    }

    pub fn rf2k_tuner_mode(&self, mode: u8) {
        let _ = self.cmd_tx.send(Command::Rf2kTunerMode(mode));
    }

    pub fn rf2k_tuner_bypass(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::Rf2kTunerBypass(on));
    }

    pub fn rf2k_tuner_reset(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kTunerReset);
    }

    pub fn rf2k_tuner_store(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kTunerStore);
    }

    pub fn rf2k_tuner_l_up(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kTunerLUp);
    }

    pub fn rf2k_tuner_l_down(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kTunerLDown);
    }

    pub fn rf2k_tuner_c_up(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kTunerCUp);
    }

    pub fn rf2k_tuner_c_down(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kTunerCDown);
    }

    pub fn rf2k_tuner_k(&self) {
        let _ = self.cmd_tx.send(Command::Rf2kTunerK);
    }

    pub fn ub_retract(&self) {
        let _ = self.cmd_tx.send(Command::UbRetract);
    }

    pub fn ub_set_frequency(&self, khz: u16, direction: u8) {
        let _ = self.cmd_tx.send(Command::UbSetFrequency(khz, direction));
    }

    pub fn ub_read_elements(&self) {
        let _ = self.cmd_tx.send(Command::UbReadElements);
    }

    pub fn rotor_goto(&self, angle_x10: u16) {
        let _ = self.cmd_tx.send(Command::RotorGoTo(angle_x10));
    }

    pub fn rotor_stop(&self) {
        let _ = self.cmd_tx.send(Command::RotorStop);
    }

    pub fn rotor_cw(&self) {
        let _ = self.cmd_tx.send(Command::RotorCw);
    }

    pub fn rotor_ccw(&self) {
        let _ = self.cmd_tx.send(Command::RotorCcw);
    }

    // Yaesu FT-991A
    pub fn yaesu_enable(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::YaesuEnable, on as u16));
    }

    /// Yaesu radio power on/off (CAT PS). Alleen zinvol op de 991A (PS0=standby,
    /// USB blijft -> remote weer aan); de FTX-1 gaat echt uit -> UI toont er label-only.
    pub fn yaesu_power_on_off(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::YaesuPowerOnOff, on as u16));
    }
    pub fn yaesu2_power_on_off(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::Yaesu2PowerOnOff, on as u16));
    }

    pub fn yaesu_read_memories(&self) {
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::YaesuReadMemories, 0));
    }

    pub fn yaesu_ptt(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::SetYaesuPtt(on));
    }

    pub fn yaesu_volume(&self, vol: f32) {
        let _ = self.cmd_tx.send(Command::SetYaesuVolume(vol));
    }

    pub fn yaesu_select_vfo(&self, vfo: u8) {
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::YaesuSelectVfo, vfo as u16));
    }

    pub fn yaesu_recall_memory(&self, channel: u16) {
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::YaesuRecallMemory, channel));
    }

    pub fn yaesu_freq(&self, hz: u64) {
        let _ = self.cmd_tx.send(Command::SetYaesuFreq(hz));
    }

    pub fn yaesu_mode(&self, mode: u8) {
        let _ = self.cmd_tx.send(Command::SetYaesuMode(mode));
    }

    pub fn yaesu_button(&self, button_id: u16) {
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::YaesuButton, button_id));
    }

    pub fn yaesu_tx_gain(&self, gain: f32) {
        let _ = self.cmd_tx.send(Command::SetYaesuTxGain(gain));
    }

    pub fn yaesu_eq_band(&self, band: u8, gain_db: f32) {
        let _ = self.cmd_tx.send(Command::SetYaesuEqBand(band, gain_db));
    }

    pub fn yaesu_eq_enabled(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::SetYaesuEqEnabled(on));
    }

    /// Client-side spraakcompressor-amount (0-100) voor de Yaesu-TX, radio 1.
    pub fn yaesu_compressor(&self, level: u8) {
        let _ = self.cmd_tx.send(Command::SetYaesuCompressor(level));
    }

    /// Client-side Yaesu-TX AGC aan/uit, radio 1 (eigen toggle, los van Thetis-AGC).
    pub fn yaesu_tx_agc(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::SetYaesuTxAgc(on));
    }

    /// Client-side spraakcompressor-amount (0-100) voor de Yaesu-TX, radio 2 (FTX-1).
    pub fn yaesu2_compressor(&self, level: u8) {
        let _ = self.cmd_tx.send(Command::SetYaesu2Compressor(level));
    }

    /// Client-side Yaesu-TX AGC aan/uit, radio 2 (FTX-1).
    pub fn yaesu2_tx_agc(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::SetYaesu2TxAgc(on));
    }

    /// Getypte DSP/functie-control voor een slot (0=radio1, 1=radio2). Dekt álle
    /// DSP-knoppen + clarifier (YaesuCtrl-index in `control`, waarde in `value`).
    pub fn yaesu_control(&self, slot: u8, control: u8, value: u16) {
        let _ = self.cmd_tx.send(Command::SetYaesuControl(slot, control, value));
    }

    // ── Radio 2 (yaesu2) — spiegel van de yaesu_* functies, geroute naar slot 1 ──
    pub fn yaesu2_enable(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::Yaesu2Enable, on as u16));
    }

    pub fn yaesu2_read_memories(&self) {
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::Yaesu2ReadMemories, 0));
    }

    pub fn yaesu2_ptt(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::SetYaesu2Ptt(on));
    }

    pub fn yaesu2_volume(&self, vol: f32) {
        let _ = self.cmd_tx.send(Command::SetYaesu2Volume(vol));
    }

    pub fn yaesu2_select_vfo(&self, vfo: u8) {
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::Yaesu2SelectVfo, vfo as u16));
    }

    pub fn yaesu2_recall_memory(&self, channel: u16) {
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::Yaesu2RecallMemory, channel));
    }

    pub fn yaesu2_freq(&self, hz: u64) {
        let _ = self.cmd_tx.send(Command::SetYaesu2Freq(hz));
    }

    pub fn yaesu2_mode(&self, mode: u8) {
        let _ = self.cmd_tx.send(Command::SetYaesu2Mode(mode));
    }

    pub fn yaesu2_button(&self, button_id: u16) {
        let _ = self.cmd_tx.send(Command::SetControl(
            sdr_remote_core::protocol::ControlId::Yaesu2Button, button_id));
    }

    pub fn yaesu2_tx_gain(&self, gain: f32) {
        let _ = self.cmd_tx.send(Command::SetYaesu2TxGain(gain));
    }

    pub fn yaesu2_eq_band(&self, band: u8, gain_db: f32) {
        let _ = self.cmd_tx.send(Command::SetYaesu2EqBand(band, gain_db));
    }

    pub fn yaesu2_eq_enabled(&self, on: bool) {
        let _ = self.cmd_tx.send(Command::SetYaesu2EqEnabled(on));
    }

    pub fn server_reboot(&self) {
        let _ = self.cmd_tx.send(Command::ServerReboot);
    }

    pub fn server_shutdown(&self) {
        let _ = self.cmd_tx.send(Command::ServerShutdown);
    }

    pub fn get_state(&self) -> BridgeRadioState {
        let rx = self.state_rx.lock().unwrap();
        let state = rx.borrow().clone();
        let lang_str = self.ui_language.lock().unwrap().clone();
        let lang = sdr_remote_logic::i18n::Lang::from_code(&lang_str);
        let mut bs = bridge_state_from_radio_state(state, lang);
        // Fase 3c: report the live relay transport (UDP vs wss-fallback) to the UI.
        if let Some(h) = &self.relay_status {
            bs.relay_transport_fallback = h.snapshot().transport_fallback;
        }
        bs
    }

    // ---- the chat ---------------------------------------------------------
    //
    // One call does the housekeeping and returns the state, because Compose has
    // a timer and not a frame loop: `chat_state(open)` ticks the model (which
    // schedules its own polling) and hands back everything the screen draws.
    // Everything else is a verb the model already knows.

    /// The chat as it stands. `open` says whether the chat screen is showing:
    /// while it is, the conversation is polled briskly; while it is not, once
    /// every half minute, which is what keeps the unread badge honest.
    pub fn chat_state(&self, open: bool) -> BridgeChatState {
        let ticket = self
            .relay_status
            .as_ref()
            .and_then(|h| h.snapshot().chat_ticket);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut chat = self.chat.lock().unwrap();
        chat.tick(&self.chat_relay_url, ticket.as_deref(), open);
        BridgeChatState {
            offline_reason: match chat.offline {
                None => 0,
                Some(sdr_remote_chat::OfflineReason::NoRelay) => 1,
                Some(sdr_remote_chat::OfflineReason::NoTicket) => 2,
                Some(sdr_remote_chat::OfflineReason::Unreachable) => 3,
            },
            consent_known: chat.consented.is_some(),
            consented: chat.consented.unwrap_or(false),
            display_name: chat.display_name.clone(),
            unread: chat.unread as u32,
            error: chat.error.clone().unwrap_or_default(),
            reports_left: chat.reports_left,
            messages: chat
                .messages
                .iter()
                .map(|m| BridgeChatMessage {
                    id: m.id,
                    at: m.at,
                    // Empty rather than optional: a name that is not there
                    // belongs to somebody who left, and Compose says so in its
                    // own words rather than showing a null.
                    name: m.name.clone().unwrap_or_default(),
                    body: m.body.clone(),
                    reply_name: m.reply_name.clone().unwrap_or_default(),
                    reply_text: m.reply_text.clone().unwrap_or_default(),
                    edited: m.edited,
                    mine: chat.is_mine(m),
                    can_edit: chat.can_edit(m, now),
                })
                .collect(),
            // Only what has not been folded away. The whole list is what the
            // service holds; what is still on screen is this side's business.
            answers: chat
                .unread_answers()
                .iter()
                .map(|a| BridgeChatAnswer {
                    id: a.id,
                    at: a.at,
                    body: a.body.clone(),
                })
                .collect(),
        }
    }

    /// Join, under this name. The service checks the name and the consent-text
    /// version; this only carries the intent.
    pub fn chat_consent(&self, display_name: String) {
        self.chat.lock().unwrap().consent(&display_name);
    }

    /// Say something. `reply_to` is 0 when it answers nothing - Compose has no
    /// use for an optional here and 0 is not a message id.
    pub fn chat_send(&self, body: String, reply_to: i64) {
        let reply = if reply_to > 0 { Some(reply_to) } else { None };
        self.chat.lock().unwrap().send(&body, reply);
    }

    /// Correct one's own message. The service is the judge of "own" and of the
    /// fifteen-minute window; a refusal comes back as the error in `chat_state`.
    pub fn chat_edit(&self, id: i64, body: String) {
        self.chat.lock().unwrap().edit(id, &body);
    }

    /// Leave the chat, with or without taking one's own messages along.
    pub fn chat_leave(&self, delete_messages: bool) {
        self.chat.lock().unwrap().leave(delete_messages);
    }

    /// The unread badge goes out when the screen has been looked at.
    pub fn chat_mark_read(&self) {
        self.chat.lock().unwrap().mark_read();
    }

    /// Clean a log and a settings dump into the attachment the user then reads.
    ///
    /// Android has neither file to point at - the log comes from the system log
    /// and the settings from the framework's preferences - so both arrive here
    /// as text the app already holds. The cleaning is the shared one, rules and
    /// all. Returns the attachment, or a line saying why there is none; either
    /// way what comes back is what the screen shows and what `chat_report`
    /// sends, because the preview is the actual safeguard (design 1.3).
    pub fn chat_build_attachment(&self, log: String, settings: String) -> String {
        match sdr_remote_core::diagnose::build_attachment_from_text(
            &log,
            &settings,
            &self.relay_url,
        ) {
            Ok(a) => a,
            Err(e) => format!("(no attachment: {e})"),
        }
    }

    /// Report a problem, in the reporter's own words, with the attachment the
    /// reporter saw - or none at all.
    ///
    /// `attachment` is passed back exactly as `chat_build_attachment` returned
    /// it and is deliberately not rebuilt here: what leaves the phone has to be
    /// what was on the screen. Empty means the box was not ticked. Joining the
    /// chat is not required (design section 4), so this works for somebody who
    /// never consented.
    /// Fold an administrator answer away.
    ///
    /// The phone had no way to do this at all, and the answers sat unbounded
    /// above the conversation - so a reader with a few of them could not read
    /// the chat any more (two users, 2026-08-20). The remembering lives in the
    /// shared model, so both front ends agree on what has been put aside.
    pub fn chat_dismiss_answer(&self, id: i64) {
        self.chat.lock().unwrap().dismiss_answer(id);
    }

    /// The folded-away ids, for the app to keep in its preferences.
    pub fn chat_seen_ids(&self) -> Vec<i64> {
        self.chat.lock().unwrap().seen_ids()
    }

    /// Take back what the app had kept, at startup.
    pub fn chat_restore_seen(&self, ids: Vec<i64>) {
        self.chat.lock().unwrap().restore_seen(&ids);
    }

    pub fn chat_report(&self, note: String, attachment: String) {
        let full = sdr_remote_core::diagnose::describe(
            &note,
            &self.relay_url,
            sdr_remote_core::version_string().as_str(),
            "android",
            if attachment.trim().is_empty() {
                None
            } else {
                Some(attachment.as_str())
            },
        );
        self.chat.lock().unwrap().send_diagnosis(&full);
    }

    pub fn shutdown(&self) {
        let mut guard = self.shutdown_tx.lock().unwrap();
        if let Some(tx) = guard.take() {
            let _ = tx.send(true);
        }
        drop(guard);
        // Also stop the relay monitor. Otherwise its background thread keeps the
        // relay connection (and its client_id slot) alive after this app instance is
        // gone — and when the next instance starts with the same install id, the two
        // ping-pong the slot (each reclaim closes the other, which reconnects after
        // RECONNECT_DELAY). Stopping the monitor here (ViewModel.onCleared) prevents
        // the zombie connection.
        if let Some(monitor) = self._relay_monitor.lock().unwrap().take() {
            info!("bridge shutting down - stopping its relay monitor");
            monitor.stop();
        }
    }
}

#[cfg(test)]
mod relay_rule_tests {
    //! The bridge's own view of the relay rule. The rule itself is tested in
    //! `sdr-remote-relay`, and nothing said that the function Compose actually
    //! calls hands the same answer through - so a review went looking for a
    //! test on `relay_is_configured` and found none.

    #[test]
    fn the_bridge_hands_through_the_shared_rule() {
        let (u, s, t) = ("wss://relay.example/ws", "pa0xyz", "secret");
        for (enabled, url, station, token) in [
            (true, u, s, t),
            (false, u, s, t),
            (true, "", s, t),
            (true, u, "", t),
            // The one the phone got wrong: everything but the token.
            (true, u, s, ""),
        ] {
            assert_eq!(
                super::relay_is_configured(
                    enabled,
                    url.to_string(),
                    station.to_string(),
                    token.to_string()
                ),
                sdr_remote_relay::is_configured(enabled, url, station, token),
                "the bridge disagrees with the shared rule for {url:?}/{station:?}/{token:?}"
            );
        }
    }
}

#[cfg(test)]
mod language_resource_tests {
    //! The app hands the Rust side its language by reading `ui_language_code` out
    //! of the very `strings.xml` Android resolved. That only works while every
    //! locale file declares it, and declares its own language - a copy-paste that
    //! left `nl` in the German file would silently put one line in the wrong
    //! language, which is exactly the sort of thing nobody notices by looking.

    use std::path::PathBuf;

    fn res_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("android/app/src/main/res")
    }

    fn declared_code(dir: &str) -> String {
        let path = res_dir().join(dir).join("strings.xml");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let open = "<string name=\"ui_language_code\">";
        let start = text
            .find(open)
            .unwrap_or_else(|| panic!("{dir}/strings.xml does not declare ui_language_code"))
            + open.len();
        let end = start
            + text[start..]
                .find("</string>")
                .expect("unterminated ui_language_code");
        text[start..end].trim().to_string()
    }

    #[test]
    fn every_locale_declares_its_own_language() {
        for (dir, expected) in [
            ("values", "en"),
            ("values-nl", "nl"),
            ("values-de", "de"),
            ("values-fr", "fr"),
        ] {
            assert_eq!(
                declared_code(dir),
                expected,
                "{dir}/strings.xml declares the wrong language"
            );
        }
    }

    #[test]
    fn the_rust_side_recognises_every_declared_language() {
        use sdr_remote_logic::i18n::Lang;
        for dir in ["values", "values-nl", "values-de", "values-fr"] {
            let code = declared_code(dir);
            let lang = Lang::from_code(&code);
            assert_eq!(
                lang.code(),
                code,
                "{dir} declares {code}, which the Rust side quietly turns into {}",
                lang.code()
            );
        }
    }
}
