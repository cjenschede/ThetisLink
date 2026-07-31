// SPDX-License-Identifier: GPL-2.0-or-later

use std::net::SocketAddr;
use std::sync::Mutex;

use log::info;
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
    pub capture_level: f32,
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
            capture_level: s.capture_level,
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
pub struct SdrBridge {
    cmd_tx: mpsc::UnboundedSender<Command>,
    state_rx: Mutex<watch::Receiver<RadioState>>,
    shutdown_tx: Mutex<Option<watch::Sender<bool>>>,
    /// PATCH-1: UI language for connect-status / connect-error rendering in
    /// `get_state()`. Set via `set_language("nl" | "en")`. Defaults to "en".
    ui_language: Mutex<String>,
    /// Phase C: houdt de relay-monitor in leven zolang de bridge bestaat (draait op
    /// een eigen thread). `None` in direct-modus.
    _relay_monitor: Mutex<Option<sdr_remote_relay::RelayMonitor>>,
    /// Fase 3c: relay status handle to surface the transport (UDP / wss-fallback) to the
    /// Compose UI. `None` in direct mode.
    relay_status: Option<sdr_remote_relay::RelayStatusHandle>,
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
        let relay_tunnel = if relay_enabled
            && !relay_url.trim().is_empty()
            && !relay_station.trim().is_empty()
            && !relay_token.trim().is_empty()
        {
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
            relay_monitor = Some(sdr_remote_relay::RelayMonitor::start_threaded_tunnel(
                relay_cfg, tunnel,
            ));
            info!("Android transport: relay tunnel (via {})", relay_url);
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
        }
    }

    /// PATCH-1: set the UI language used for connect-status / connect-error
    /// text in `get_state()`. Accepts "nl" or "en"; any other value falls
    /// back to "en". Compose UI should call this once from SharedPreferences
    /// on startup.
    pub fn set_language(&self, lang: String) {
        let normalized = if lang.to_lowercase() == "nl" {
            "nl".to_string()
        } else {
            "en".to_string()
        };
        *self.ui_language.lock().unwrap() = normalized;
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
        let lang = if lang_str == "nl" {
            sdr_remote_logic::i18n::Lang::Nl
        } else {
            sdr_remote_logic::i18n::Lang::En
        };
        let mut bs = bridge_state_from_radio_state(state, lang);
        // Fase 3c: report the live relay transport (UDP vs wss-fallback) to the UI.
        if let Some(h) = &self.relay_status {
            bs.relay_transport_fallback = h.snapshot().transport_fallback;
        }
        bs
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
            monitor.stop();
        }
    }
}
