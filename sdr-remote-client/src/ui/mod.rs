// SPDX-License-Identifier: GPL-2.0-or-later

#![allow(dead_code)]
mod helpers;
mod meters;
mod spectrum;
mod config;
mod devices;
mod screens;
pub(crate) mod controls;
pub(crate) mod yaesu_memory;
pub(crate) mod yaesu_menu;

pub(crate) use helpers::*;
pub(crate) use meters::*;
pub(crate) use spectrum::*;
pub(crate) use config::{load_window_size, save_config, load_config, NUM_MEMORIES};

use std::time::Instant;

use egui::{Color32, Pos2, RichText, Stroke, TextureHandle, Vec2, ViewportBuilder, ViewportId};
use tokio::sync::{mpsc, watch};

use std::collections::{HashMap, VecDeque};

use sdr_remote_core::protocol::ControlId;
use sdr_remote_logic::commands::Command;
use sdr_remote_logic::state::RadioState;

use crate::LogBuffer;

/// Frequency memory slot
#[derive(Clone, Default)]
pub(crate) struct Memory {
    pub(crate) frequency_hz: Option<u64>,
    pub(crate) mode: Option<u8>,
}

/// VFO identifier for shared RX1/RX2 logic
#[derive(Clone, Copy, PartialEq)]
enum Vfo { A, B }

/// Main screen tab selector
#[derive(Clone, Copy, PartialEq)]
enum Tab { Radio, Devices, Thetis, Server, Midi }

/// Per-band memory: remembers last-used settings when switching bands
#[derive(Clone)]
pub(crate) struct BandMemory {
    pub(crate) frequency_hz: u64,
    pub(crate) mode: u8,
    pub(crate) filter_low_hz: i32,
    pub(crate) filter_high_hz: i32,
    pub(crate) nr_level: u8,
}

/// Waterfall ring buffer: stores full DDC + extracted view rows
pub(crate) struct WaterfallRingBuffer {
    full_rows: Vec<Vec<u16>>,
    full_centers: Vec<u32>,
    view_rows: Vec<Vec<u16>>,
    view_centers: Vec<u32>,
    view_spans: Vec<u32>,
    write_idx: usize,
    count: usize,
    last_seq: u16,
    height: usize,
    texture: Option<TextureHandle>,
}

impl WaterfallRingBuffer {
    fn new(height: usize) -> Self {
        Self {
            full_rows: vec![Vec::new(); height],
            full_centers: vec![0; height],
            view_rows: vec![Vec::new(); height],
            view_centers: vec![0; height],
            view_spans: vec![0; height],
            write_idx: 0,
            count: 0,
            last_seq: 0,
            height,
            texture: None,
        }
    }

    fn push(
        &mut self,
        full_bins: &[u16], full_center_hz: u32, full_span_hz: u32, sequence: u16,
        view_bins: &[u16], view_center_hz: u32, view_span_hz: u32,
    ) {
        if full_bins.is_empty() || full_span_hz == 0 || sequence == self.last_seq {
            return;
        }
        self.last_seq = sequence;
        let idx = self.write_idx;
        self.full_rows[idx] = full_bins.to_vec();
        self.full_centers[idx] = full_center_hz;
        self.view_rows[idx] = view_bins.to_vec();
        self.view_centers[idx] = view_center_hz;
        self.view_spans[idx] = view_span_hz;
        self.write_idx = (idx + 1) % self.height;
        if self.count < self.height {
            self.count += 1;
        }
    }
}

/// egui application — communicates with engine via watch/mpsc channels
pub struct SdrRemoteApp {
    state_rx: watch::Receiver<RadioState>,
    cmd_tx: mpsc::UnboundedSender<Command>,
    /// App-level UI-observability sink. Per-call-site `TracingSink`-constructie
    /// vervangen. In prod altijd `TracingSink`; in test-mode kan `RecordingSink`
    /// gezet worden voor assert-based tests.
    ui_event_sink: std::sync::Arc<dyn controls::UiEventSink>,
    // UI-local state
    server_input: String,
    password_input: String,
    totp_input: String,
    mouse_ptt: bool,
    midi_ptt: bool,
    ptt_toggle_mode: bool,       // false=push-to-talk (momentary), true=toggle (click on/off)
    yaesu_ptt_toggle_mode: bool, // independent Yaesu PTT mode
    yaesu_mouse_ptt: bool,       // tracks local Yaesu momentary PTT button state
    // Audio recording / playback
    recording: bool,
    playing: bool,
    rec_rx1: bool,
    rec_rx2: bool,
    rec_yaesu: bool,
    last_recorded_path: Option<String>,
    midi_ptt_toggle_mode: bool,  // independent MIDI PTT mode
    reboot_confirm: bool,
    diversity_enabled: bool,
    diversity_state_read: bool,
    diversity_ref: u16,        // 0=RX2, 1=RX1
    diversity_source: u16,     // 0=RX1+RX2, 1=RX1, 2=RX2
    audio_mode: u16,           // 0=Mono, 1=BIN, 2=Split
    diversity_gain_rx1: f32,   // 0.000-5.000 (CAT max)
    diversity_gain_rx2: f32,   // 0.000-5.000 (CAT max)
    diversity_gain_multi: f32, // 1.0-10.0 (circle edge = gain_multi)
    diversity_phase_lock: bool,
    diversity_gain_lock: bool,
    // Auto-null state machine
    diversity_auto_active: bool,
    diversity_auto_step: usize,
    diversity_auto_round: usize,
    diversity_auto_best_phase: f32,
    diversity_auto_best_gain: f32,    // linear gain
    diversity_auto_best_smeter: f32,
    diversity_auto_last_set: Instant,
    diversity_auto_start_smeter: f32,
    diversity_auto_overall_best: f32,
    diversity_auto_result: u8,        // 0=idle, 1=searching, 2=improved, 3=no improvement, 4=measuring off, 5=measuring on
    diversity_auto_improvement_db: f32,
    diversity_auto_slow: bool,
    diversity_auto_smart: bool,
    diversity_auto_ultra: bool,
    diversity_auto_eq_gain_db: f32,   // equalized gain in dB from step 1
    // Successive approximation state
    diversity_sa_param: u8,           // 0=phase, 1=gain
    diversity_sa_step: f32,           // current step size (degrees or dB)
    diversity_sa_sub: u8,             // 0=measure center, 1=measure +step, 2=measure -step, 3=decide
    diversity_sa_center_smeter: f32,
    diversity_sa_plus_smeter: f32,
    diversity_sa_minus_smeter: f32,
    diversity_sa_iteration: u8,       // alternation counter (phase→gain→phase→gain)
    diversity_phase: f32,      // -180.0 to +180.0 degrees
    ddc_sample_rate_rx1: u16,  // kHz (0=unknown)
    ddc_sample_rate_rx2: u16,  // kHz (0=unknown)
    freq_step_index: usize,
    memories: [Memory; NUM_MEMORIES],
    save_mode: bool,
    freq_editing: bool,
    freq_edit_text: String,
    tx_profiles: Vec<(u8, String)>,
    input_devices: Vec<String>,
    output_devices: Vec<String>,
    device_refresh_at: Option<Instant>,
    selected_input: String,
    selected_output: String,
    /// Mic device → TX profile name mapping (auto-switch on mic change)
    mic_profile_map: std::collections::HashMap<String, String>,
    // Config values tracked by UI (sent as commands on change)
    rx_volume: f32,       // Thetis ZZLA (for control panel "RX1 Vol")
    vfo_a_volume: f32,    // Client-only VFO A playback volume
    vfo_b_volume: f32,    // Client-only VFO B playback volume
    local_volume: f32,    // Client-only master volume
    tx_gain: f32,
    // Cached state from RadioState (updated each frame)
    connected: bool,
    ptt: bool,
    ptt_denied: bool,
    rtt_ms: u16,
    jitter_ms: f32,
    buffer_depth: u32,
    rx_packets: u64,
    loss_percent: u8,
    capture_level: f32,
    playback_level: f32,
    playback_level_bin_r: f32,
    playback_level_rx2: f32,
    playback_level_yaesu: f32,
    yaesu_mic_level: f32,
    frequency_hz: u64,
    mode: u8,
    smeter: u16,
    smeter_peak: u16,
    smeter_peak_time: Instant,
    power_on: bool,
    power_press_start: Option<Instant>,
    shutdown_sent: bool,
    thetis_tuning: bool,
    tune_pa_was_operate: bool,       // PA was in operate before tune, restore after
    tune_pending_on: Option<Instant>,  // delayed ZZTU1 after PA standby
    tune_pending_restore: Option<Instant>, // delayed PA restore after ZZTU0
    tx_profile: u8,
    nr_level: u8,
    anf_on: bool,
    drive_level: u8,
    audio_error: bool,
    agc_enabled: bool,
    other_tx: bool,
    thetis_swr_x100: u16,
    filter_low_hz: i32,
    filter_high_hz: i32,
    filter_changed_at: Option<Instant>,
    thetis_starting: bool,
    // Spectrum + waterfall
    spectrum_enabled: bool,
    spectrum_bins: Vec<u16>,
    spectrum_center_hz: u32,
    spectrum_span_hz: u32,
    spectrum_ref_level: i8,
    spectrum_db_per_unit: u8,
    last_spectrum_seq: u16,
    // Full DDC spectrum (for waterfall)
    full_spectrum_bins: Vec<u16>,
    full_spectrum_center_hz: u32,
    full_spectrum_span_hz: u32,
    full_spectrum_sequence: u16,
    // Spectrum display settings (local UI)
    spectrum_ref_db: f32,    // Top of display in dB (e.g. -20.0)
    spectrum_range_db: f32,  // dB range from top to bottom (e.g. 100.0)
    // Spectrum zoom/pan (sent to server, server extracts the view)
    spectrum_zoom: f32,      // 1.0 = full span, 2.0 = half span, etc.
    spectrum_pan: f32,       // 0.0 = centered, -0.5..+0.5 = shift fraction
    // Debounce: only send zoom/pan after 100ms stability
    last_sent_zoom: f32,
    last_sent_pan: f32,
    zoom_pan_changed_at: Option<Instant>,
    // Frequency change tracking (prevents bounce: local→server_old→server_new)
    pending_freq: Option<u64>,
    pending_freq_at: Option<Instant>,
    rx2_pending_freq: Option<u64>,
    rx2_pending_freq_at: Option<Instant>,
    rx1_force_full_tuning: bool,
    rx2_force_full_tuning: bool,
    // Waterfall ring buffer
    waterfall: WaterfallRingBuffer,
    waterfall_contrast: f32,  // 0.5 = low contrast, 1.0 = normal, 2.0 = high
    // Auto ref level
    auto_ref_enabled: bool,
    auto_ref_value: f32,
    auto_ref_frames: u32,
    auto_ref_initialized: bool,
    // TX spectrum override
    tx_spectrum_saved_ref_db: Option<f32>,   // saved spectrum_ref_db before TX
    tx_spectrum_saved_range: Option<f32>,     // saved range_db before TX
    tx_spectrum_saved_auto_ref: Option<bool>, // saved auto_ref_enabled before TX
    tx_spectrum_restore_auto_at: Option<std::time::Instant>, // delayed auto_ref restore
    // Per-band WF contrast
    wf_contrast_per_band: HashMap<String, f32>,
    band_mem: HashMap<String, BandMemory>,
    current_band: Option<String>,
    spectrum_max_bins: u16,
    spectrum_fft_size_k: u16,      // FFT size in K (0=auto, 32, 64, 128, 256)
    rx2_spectrum_fft_size_k: u16,  // RX2 FFT size (independent from RX1)
    spectrum_popout: bool,
    // Window size persistence
    window_w: f32,
    window_h: f32,
    // Log panel
    log_buffer: LogBuffer,
    show_log: bool,
    show_about: bool,
    // Devices screen
    active_tab: Tab,
    device_tab: u8, // 0=Amplitec, 1=Tuner, 2=SPE, 3=RF2K, 4=UltraBeam
    amplitec_connected: bool,
    amplitec_switch_a: u8,
    amplitec_switch_b: u8,
    amplitec_labels: String,
    amplitec_log: VecDeque<(String, String)>,  // (timestamp, message)
    // Tuner state
    tuner_connected: bool,
    tuner_state: u8,       // 0=Idle, 1=Tuning, 2=DoneOk, 3=Timeout, 4=Aborted
    tuner_can_tune: bool,
    tuner_tune_freq: u64,  // Frequency at last successful tune (for stale detection)
    // SPE Expert state
    spe_connected: bool,
    spe_state: u8,
    spe_band: u8,
    spe_ptt: bool,
    spe_power_w: u16,
    spe_swr_x10: u16,
    spe_temp: u8,
    spe_warning: u8,
    spe_alarm: u8,
    spe_power_level: u8,
    spe_antenna: u8,
    spe_input: u8,
    spe_voltage_x10: u16,
    spe_current_x10: u16,
    spe_atu_bypassed: bool,
    spe_available: bool,
    spe_active: bool,
    spe_peak_power: u16,
    spe_peak_time: Instant,
    // RF2K-S Amplifier state
    rf2k_connected: bool,
    rf2k_operate: bool,
    rf2k_band: u8,
    rf2k_frequency_khz: u16,
    rf2k_temperature_x10: u16,
    rf2k_voltage_x10: u16,
    rf2k_current_x10: u16,
    rf2k_forward_w: u16,
    rf2k_reflected_w: u16,
    rf2k_swr_x100: u16,
    rf2k_max_forward_w: u16,
    rf2k_max_reflected_w: u16,
    rf2k_max_swr_x100: u16,
    rf2k_error_state: u8,
    rf2k_error_text: String,
    rf2k_antenna_type: u8,
    rf2k_antenna_number: u8,
    rf2k_tuner_mode: u8,
    rf2k_tuner_setup: String,
    rf2k_tuner_l_nh: u16,
    rf2k_tuner_c_pf: u16,
    rf2k_drive_w: u16,
    rf2k_modulation: String,
    rf2k_max_power_w: u16,
    rf2k_device_name: String,
    rf2k_available: bool,
    rf2k_active: bool,
    rf2k_peak_power: u16,
    rf2k_peak_time: Instant,
    // RF2K-S debug (Fase D)
    rf2k_debug_available: bool,
    rf2k_bias_pct_x10: u16,
    rf2k_psu_source: u8,
    rf2k_uptime_s: u32,
    rf2k_tx_time_s: u32,
    rf2k_error_count: u16,
    rf2k_error_history: Vec<(String, String)>,
    rf2k_storage_bank: u16,
    rf2k_hw_revision: String,
    rf2k_frq_delay: u16,
    rf2k_autotune_threshold_x10: u16,
    rf2k_dac_alc: u16,
    rf2k_high_power: bool,
    rf2k_tuner_6m: bool,
    rf2k_band_gap_allowed: bool,
    rf2k_controller_version: u16,
    rf2k_drive_config_ssb: [u8; 11],
    rf2k_drive_config_am: [u8; 11],
    rf2k_drive_config_cont: [u8; 11],
    rf2k_show_debug: bool,
    rf2k_show_drive_config: bool,
    rf2k_confirm_high_power: bool,
    rf2k_confirm_zero_fram: bool,
    rf2k_drive_edit: [[u8; 11]; 3],
    rf2k_drive_loaded: bool,
    rf2k_confirm_fw_close: bool,
    // UltraBeam RCU-06
    ub_connected: bool,
    ub_frequency_khz: u16,
    ub_band: u8,
    ub_direction: u8,
    ub_off_state: bool,
    ub_motors_moving: u8,
    ub_motor_completion: u16,
    ub_fw_major: u8,
    ub_fw_minor: u8,
    ub_available: bool,
    ub_elements_mm: [u16; 6],
    ub_confirm_retract: bool,
    ub_auto_track: bool,
    ub_last_auto_khz: u16,
    // Rotor
    rotor_connected: bool,
    rotor_angle_x10: u16,
    rotor_rotating: bool,
    rotor_target_x10: u16,
    rotor_available: bool,
    // Yaesu FT-991A
    yaesu_connected: bool,
    yaesu_freq_a: u64,
    yaesu_freq_b: u64,
    yaesu_mode: u8,
    yaesu_smeter: u16,
    yaesu_tx_active: bool,
    yaesu_power_on: bool,
    yaesu_volume: f32,
    yaesu_popout: bool,
    yaesu_popout_pos: Option<egui::Pos2>,
    yaesu_popout_size: Option<egui::Vec2>,
    yaesu_popout_first_frame: bool,
    yaesu_enable_sent: bool,
    yaesu_mic_gain: f32, // multiplier for Yaesu USB TX audio (default 20x)
    yaesu_eq_enabled: bool,
    yaesu_eq_gains: [f32; 5], // -12..+12 dB per band
    yaesu_eq_profiles: Vec<(String, bool, [f32; 5])>, // (name, enabled, gains)
    yaesu_eq_active_profile: String,
    yaesu_eq_new_name: String,
    yaesu_squelch: u16,       // 0-255
    yaesu_rf_gain: u16,       // 0-255
    yaesu_radio_mic_gain: u16, // 0-100 (radio's own mic gain)
    yaesu_rf_power: u16,      // 0-100 (TX power)
    yaesu_scan_active: bool,
    yaesu_split_active: bool,
    yaesu_tuner_active: bool,
    yaesu_in_memory_mode: bool,
    yaesu_current_mem_ch: Option<usize>, // index into yaesu_mem_channels
    yaesu_enabled: bool,
    // Yaesu memory channels
    yaesu_mem_channels: Vec<yaesu_memory::YaesuMemoryChannel>,
    yaesu_mem_file: String,
    yaesu_mem_selected: Option<usize>,
    yaesu_mem_filter: String,
    yaesu_mem_dirty: bool,
    yaesu_mem_radio_received: bool,
    yaesu_menu_items: Vec<yaesu_menu::MenuItem>,
    yaesu_menu_received: bool,
    rotor_goto_input: String,
    // DX Cluster spots
    dx_spots: Vec<sdr_remote_logic::state::DxSpotInfo>,
    // Smooth tuning: display center interpolates toward VFO for smooth visual scroll
    smooth_display_center_hz: f64,   // RX1 smoothed display center
    rx2_smooth_display_center_hz: f64, // RX2 smoothed display center
    smooth_alpha: f64,               // shared smoothing alpha for current frame
    last_frame_time: Instant,
    // RX2 / VFO-B
    rx2_enabled: bool,
    rx2_popout: bool,
    popout_joined: bool,
    popout_meter_analog: bool,
    /// Last S-meter rects in popout viewports (screen coords) for A⇔B overlay
    popout_rx1_smeter_rect: egui::Rect,
    popout_rx2_smeter_rect: egui::Rect,
    rx2_volume: f32,
    rx2_af_gain_display: u8, // Thetis ZZLB value for display
    rx2_frequency_hz: u64,
    rx2_mode: u8,
    rx2_smeter: u16,
    rx2_smeter_peak: u16,
    rx2_smeter_peak_time: Instant,
    rx2_filter_low_hz: i32,
    rx2_filter_high_hz: i32,
    rx2_filter_changed_at: Option<Instant>,
    rx2_nr_level: u8,
    rx2_anf_on: bool,
    rx2_freq_step_index: usize,
    /// Inline freq-edit state voor RX2 — symmetrisch met `freq_editing` op RX1
    /// (PATCH-rx2-inline-edit).
    rx2_freq_editing: bool,
    rx2_freq_edit_text: String,
    rx2_spectrum_bins: Vec<u16>,
    rx2_spectrum_center_hz: u32,
    rx2_spectrum_span_hz: u32,
    rx2_last_spectrum_seq: u16,
    rx2_full_spectrum_bins: Vec<u16>,
    rx2_full_spectrum_center_hz: u32,
    rx2_full_spectrum_span_hz: u32,
    rx2_full_spectrum_sequence: u16,
    rx2_spectrum_zoom: f32,
    rx2_spectrum_pan: f32,
    rx2_last_sent_zoom: f32,
    rx2_last_sent_pan: f32,
    rx2_zoom_pan_changed_at: Option<Instant>,
    rx2_waterfall: WaterfallRingBuffer,
    // RX2 spectrum display settings (same as RX1)
    rx2_spectrum_ref_db: f32,
    rx2_spectrum_range_db: f32,
    rx2_auto_ref_enabled: bool,
    rx2_auto_ref_value: f32,
    rx2_auto_ref_frames: u32,
    rx2_auto_ref_initialized: bool,
    rx2_waterfall_contrast: f32,
    vfo_sync: bool,
    mon_on: bool,
    // New TCI controls
    agc_mode: u8,
    agc_gain: u8,
    agc_auto_rx1: bool,
    agc_auto_rx2: bool,
    rit_enable: bool,
    rit_offset: i16,
    xit_enable: bool,
    xit_offset: i16,
    sql_enable: bool,
    sql_level: u8,
    nb_enable: bool,
    nb_level: u8,
    cw_keyer_speed: u8,
    vfo_lock: bool,
    binaural: bool,
    apf_enable: bool,
    // RX2 TCI controls
    rx2_agc_mode: u8,
    rx2_agc_gain: u8,
    rx2_sql_enable: bool,
    rx2_sql_level: u8,
    rx2_nb_enable: bool,
    rx2_nb_level: u8,
    rx2_binaural: bool,
    rx2_apf_enable: bool,
    rx2_vfo_lock: bool,
    // New TCI controls (v2.10.3.13 RC1)
    mute: bool,
    rx_mute: bool,
    nf_enable: bool,
    rx2_nf_enable: bool,
    rx_balance: i8,
    tune_drive: u8,
    mon_volume: i8,
    /// Timestamp of last local TCI control change (suppress server sync for 500ms)
    tci_control_changed_at: Option<Instant>,
    yaesu_control_changed_at: Option<Instant>,
    // MIDI
    midi: crate::midi::MidiManager,
    midi_ports: Vec<String>,
    midi_selected_port: String,
    midi_learn_for: Option<usize>, // index in mapping list being learned, or ALL.len() for new
    midi_learn_action: crate::midi::MidiAction,
    midi_last_event: String, // last received MIDI event description
    midi_encoder_hz: u64,    // Hz per encoder tick for VFO tuning
    midi_last_dir_a: i8,     // last encoder direction for VFO A (-1/+1)
    midi_last_dir_b: i8,     // last encoder direction for VFO B (-1/+1)
    // CatSync (WebSDR browser mute on TX)
    catsync: crate::catsync::CatSync,
}

impl SdrRemoteApp {
    fn spectrum_target_center_hz(vfo_hz: u64, full_span_hz: u32, pan: f32, fallback_center_hz: u32) -> f64 {
        if full_span_hz > 0 {
            vfo_hz as f64 + pan as f64 * full_span_hz as f64
        } else {
            fallback_center_hz as f64
        }
    }

    fn should_force_full_tuning(
        target_center_hz: f64,
        extracted_center_hz: u32,
        extracted_span_hz: u32,
    ) -> bool {
        if extracted_center_hz == 0 || extracted_span_hz == 0 {
            return false;
        }
        let delta_hz = (target_center_hz - extracted_center_hz as f64).abs();
        let threshold_hz = (extracted_span_hz as f64 * 0.5).clamp(8_000.0, 24_000.0);
        delta_hz > threshold_hz
    }

    fn tuning_latch_active(
        force_full_tuning: bool,
        pending_freq: Option<u64>,
        pending_freq_at: Option<Instant>,
    ) -> bool {
        if !force_full_tuning {
            return false;
        }
        if pending_freq.is_some() {
            return true;
        }
        pending_freq_at.map_or(false, |t| t.elapsed().as_millis() < 250)
    }

    fn set_pending_freq_a(&mut self, freq: u64) {
        let target_center = Self::spectrum_target_center_hz(
            freq,
            self.full_spectrum_span_hz,
            self.spectrum_pan,
            self.spectrum_center_hz,
        );
        self.frequency_hz = freq;
        self.pending_freq = Some(freq);
        self.pending_freq_at = Some(Instant::now());
        self.rx1_force_full_tuning = Self::should_force_full_tuning(
            target_center,
            self.spectrum_center_hz,
            self.spectrum_span_hz,
        );
    }

    fn set_pending_freq_b(&mut self, freq: u64) {
        let target_center = Self::spectrum_target_center_hz(
            freq,
            self.rx2_full_spectrum_span_hz,
            self.rx2_spectrum_pan,
            self.rx2_spectrum_center_hz,
        );
        self.rx2_frequency_hz = freq;
        self.rx2_pending_freq = Some(freq);
        self.rx2_pending_freq_at = Some(Instant::now());
        self.rx2_force_full_tuning = Self::should_force_full_tuning(
            target_center,
            self.rx2_spectrum_center_hz,
            self.rx2_spectrum_span_hz,
        );
    }

    pub fn new(
        state_rx: watch::Receiver<RadioState>,
        cmd_tx: mpsc::UnboundedSender<Command>,
        log_buffer: LogBuffer,
    ) -> Self {
        let config = load_config();

        let input_devices = crate::audio::list_input_devices();
        let output_devices = crate::audio::list_output_devices();

        // Send initial device selections to engine
        if !config.input_device.is_empty() {
            let _ = cmd_tx.send(Command::SetInputDevice(config.input_device.clone()));
        }
        if !config.output_device.is_empty() {
            let _ = cmd_tx.send(Command::SetOutputDevice(config.output_device.clone()));
        }
        let _ = cmd_tx.send(Command::SetRxVolume(config.rx_volume));
        let _ = cmd_tx.send(Command::SetTxGain(config.tx_gain));
        let _ = cmd_tx.send(Command::SetVfoAVolume(config.vfo_a_volume));
        let _ = cmd_tx.send(Command::SetVfoBVolume(config.vfo_b_volume));
        let _ = cmd_tx.send(Command::SetLocalVolume(config.local_volume));
        let _ = cmd_tx.send(Command::SetRx2Volume(config.rx2_volume));
        // Restore EQ from active profile
        if let Some((_, en, gains)) = config.yaesu_eq_profiles.iter()
            .find(|(n, _, _)| n == &config.yaesu_eq_active) {
            let _ = cmd_tx.send(Command::SetYaesuEqEnabled(*en));
            for i in 0..5 { let _ = cmd_tx.send(Command::SetYaesuEqBand(i as u8, gains[i])); }
        }
        let _ = cmd_tx.send(Command::SetAgcEnabled(config.agc_enabled));
        if config.rx2_enabled {
            let _ = cmd_tx.send(Command::SetRx2Enabled(true));
        }
        if config.spectrum_enabled {
            let _ = cmd_tx.send(Command::EnableSpectrum(true));
        }
        if config.spectrum_max_bins != sdr_remote_core::DEFAULT_SPECTRUM_BINS as u16 {
            let _ = cmd_tx.send(Command::SetSpectrumMaxBins(config.spectrum_max_bins));
            let _ = cmd_tx.send(Command::SetControl(ControlId::Rx2SpectrumMaxBins, config.spectrum_max_bins));
        }
        if config.spectrum_fft_size_k != 0 {
            let _ = cmd_tx.send(Command::SetSpectrumFftSize(config.spectrum_fft_size_k));
        }
        if config.rx2_spectrum_fft_size_k != 0 {
            let _ = cmd_tx.send(Command::SetControl(ControlId::Rx2SpectrumFftSize, config.rx2_spectrum_fft_size_k));
        }

        let mut app = Self {
            state_rx,
            cmd_tx,
            ui_event_sink: std::sync::Arc::new(controls::TracingSink),
            server_input: config.server,
            password_input: config.password.clone(),
            totp_input: String::new(),
            mouse_ptt: false,
            ptt_toggle_mode: config.ptt_toggle,
            yaesu_ptt_toggle_mode: config.yaesu_ptt_toggle,
            yaesu_mouse_ptt: false,
            recording: false,
            playing: false,
            rec_rx1: true,
            rec_rx2: false,
            rec_yaesu: false,
            last_recorded_path: None,
            midi_ptt_toggle_mode: config.midi_ptt_toggle,
            reboot_confirm: false,
            diversity_enabled: false,
            diversity_state_read: false,
            diversity_ref: 1,       // RX1 as default reference
            diversity_source: 0,    // RX1+RX2
            audio_mode: 0,          // Mono
            diversity_gain_rx1: 1.5,
            diversity_gain_rx2: 1.5,
            diversity_gain_multi: 5.0,
            diversity_phase_lock: false,
            diversity_gain_lock: false,
            diversity_auto_active: false,
            diversity_auto_step: 0,
            diversity_auto_round: 0,
            diversity_auto_best_phase: 0.0,
            diversity_auto_best_gain: 1.0,
            diversity_auto_best_smeter: 999.0,
            diversity_auto_last_set: Instant::now(),
            diversity_auto_start_smeter: 0.0,
            diversity_auto_overall_best: 999.0,
            diversity_auto_result: 0,
            diversity_auto_improvement_db: 0.0,
            diversity_auto_slow: true,
            diversity_auto_smart: true,
            diversity_auto_ultra: false,
            diversity_auto_eq_gain_db: 0.0,
            diversity_sa_param: 0,
            diversity_sa_step: 90.0,
            diversity_sa_sub: 0,
            diversity_sa_center_smeter: 0.0,
            diversity_sa_plus_smeter: 0.0,
            diversity_sa_minus_smeter: 0.0,
            diversity_sa_iteration: 0,
            diversity_phase: 0.0,
            ddc_sample_rate_rx1: 0,
            ddc_sample_rate_rx2: 0,
            midi_ptt: false,
            freq_step_index: 3, // default 1 kHz
            memories: config.memories,
            save_mode: false,
            freq_editing: false,
            freq_edit_text: String::new(),
            tx_profiles: config.tx_profiles,
            input_devices,
            output_devices,
            device_refresh_at: Some(Instant::now()),
            selected_input: config.input_device,
            mic_profile_map: config.mic_profile_map.clone(),
            selected_output: config.output_device,
            rx_volume: config.rx_volume,
            vfo_a_volume: config.vfo_a_volume,
            vfo_b_volume: config.vfo_b_volume,
            local_volume: config.local_volume,
            tx_gain: config.tx_gain,
            connected: false,
            ptt: false,
            ptt_denied: false,
            rtt_ms: 0,
            jitter_ms: 0.0,
            buffer_depth: 0,
            rx_packets: 0,
            loss_percent: 0,
            capture_level: 0.0,
            playback_level: 0.0,
            playback_level_bin_r: 0.0,
            playback_level_rx2: 0.0,
            playback_level_yaesu: 0.0,
            yaesu_mic_level: 0.0,
            frequency_hz: 0,
            mode: 0,
            smeter: 0,
            smeter_peak: 0,
            smeter_peak_time: Instant::now(),
            power_on: false,
            power_press_start: None,
            shutdown_sent: false,
            thetis_tuning: false,
            tune_pa_was_operate: false,
            tune_pending_on: None,
            tune_pending_restore: None,
            tx_profile: 0,
            nr_level: 0,
            anf_on: false,
            drive_level: 0,
            audio_error: false,
            agc_enabled: config.agc_enabled,
            other_tx: false,
            thetis_swr_x100: 100,
            filter_low_hz: 0,
            filter_high_hz: 0,
            filter_changed_at: None,
            thetis_starting: false,
            spectrum_enabled: config.spectrum_enabled,
            spectrum_bins: Vec::new(),
            spectrum_center_hz: 0,
            spectrum_span_hz: 0,
            spectrum_ref_level: 0,
            spectrum_db_per_unit: 1,
            last_spectrum_seq: 0,
            full_spectrum_bins: Vec::new(),
            full_spectrum_center_hz: 0,
            full_spectrum_span_hz: 0,
            full_spectrum_sequence: 0,
            spectrum_ref_db: config.spectrum_ref_db,
            spectrum_range_db: config.spectrum_range_db,
            spectrum_zoom: 32.0,
            spectrum_pan: 0.0,
            last_sent_zoom: 32.0,
            last_sent_pan: 0.0,
            zoom_pan_changed_at: None,
            pending_freq: None,
            pending_freq_at: None,
            rx2_pending_freq: None,
            rx2_pending_freq_at: None,
            rx1_force_full_tuning: false,
            rx2_force_full_tuning: false,
            waterfall: WaterfallRingBuffer::new(200),
            waterfall_contrast: config.waterfall_contrast,
            auto_ref_enabled: config.auto_ref_enabled,
            auto_ref_value: -30.0,
            auto_ref_frames: 0,
            auto_ref_initialized: false,
            tx_spectrum_saved_ref_db: None,
            tx_spectrum_saved_range: None,
            tx_spectrum_saved_auto_ref: None,
            tx_spectrum_restore_auto_at: None,
            wf_contrast_per_band: config.wf_contrast_per_band,
            band_mem: config.band_mem,
            current_band: None,
            spectrum_max_bins: config.spectrum_max_bins,
            spectrum_fft_size_k: config.spectrum_fft_size_k,
            rx2_spectrum_fft_size_k: config.rx2_spectrum_fft_size_k,
            spectrum_popout: false,
            window_w: config.window_w,
            window_h: config.window_h,
            log_buffer,
            show_log: false,
            show_about: false,
            active_tab: Tab::Radio,
            device_tab: config.device_tab,
            amplitec_connected: false,
            amplitec_switch_a: 0,
            amplitec_switch_b: 0,
            amplitec_labels: String::new(),
            amplitec_log: VecDeque::new(),
            tuner_connected: false,
            tuner_state: 0,
            tuner_can_tune: false,
            tuner_tune_freq: 0,
            spe_connected: false,
            spe_state: 0,
            spe_band: 0,
            spe_ptt: false,
            spe_power_w: 0,
            spe_swr_x10: 10,
            spe_temp: 0,
            spe_warning: b'N',
            spe_alarm: b'N',
            spe_power_level: 0,
            spe_antenna: 0,
            spe_input: 0,
            spe_voltage_x10: 0,
            spe_current_x10: 0,
            spe_atu_bypassed: false,
            spe_available: false,
            spe_active: false,
            spe_peak_power: 0,
            spe_peak_time: Instant::now(),
            rf2k_connected: false,
            rf2k_operate: false,
            rf2k_band: 0,
            rf2k_frequency_khz: 0,
            rf2k_temperature_x10: 0,
            rf2k_voltage_x10: 0,
            rf2k_current_x10: 0,
            rf2k_forward_w: 0,
            rf2k_reflected_w: 0,
            rf2k_swr_x100: 100,
            rf2k_max_forward_w: 0,
            rf2k_max_reflected_w: 0,
            rf2k_max_swr_x100: 100,
            rf2k_error_state: 0,
            rf2k_error_text: String::new(),
            rf2k_antenna_type: 0,
            rf2k_antenna_number: 1,
            rf2k_tuner_mode: 0,
            rf2k_tuner_setup: String::new(),
            rf2k_tuner_l_nh: 0,
            rf2k_tuner_c_pf: 0,
            rf2k_drive_w: 0,
            rf2k_modulation: String::new(),
            rf2k_max_power_w: 0,
            rf2k_device_name: String::new(),
            rf2k_available: false,
            rf2k_active: false,
            rf2k_peak_power: 0,
            rf2k_peak_time: Instant::now(),
            rf2k_debug_available: false,
            rf2k_bias_pct_x10: 0,
            rf2k_psu_source: 0,
            rf2k_uptime_s: 0,
            rf2k_tx_time_s: 0,
            rf2k_error_count: 0,
            rf2k_error_history: Vec::new(),
            rf2k_storage_bank: 0,
            rf2k_hw_revision: String::new(),
            rf2k_frq_delay: 0,
            rf2k_autotune_threshold_x10: 0,
            rf2k_dac_alc: 0,
            rf2k_high_power: false,
            rf2k_tuner_6m: false,
            rf2k_band_gap_allowed: false,
            rf2k_controller_version: 0,
            rf2k_drive_config_ssb: [0; 11],
            rf2k_drive_config_am: [0; 11],
            rf2k_drive_config_cont: [0; 11],
            rf2k_show_debug: false,
            rf2k_show_drive_config: false,
            rf2k_confirm_high_power: false,
            rf2k_confirm_zero_fram: false,
            rf2k_drive_edit: [[0; 11]; 3],
            rf2k_drive_loaded: false,
            rf2k_confirm_fw_close: false,
            ub_connected: false,
            ub_frequency_khz: 0,
            ub_band: 0,
            ub_direction: 0,
            ub_off_state: true,
            ub_motors_moving: 0,
            ub_motor_completion: 0,
            ub_fw_major: 0,
            ub_fw_minor: 0,
            ub_available: false,
            ub_elements_mm: [0; 6],
            ub_confirm_retract: false,
            ub_auto_track: false,
            ub_last_auto_khz: 0,
            rotor_connected: false,
            rotor_angle_x10: 0,
            rotor_rotating: false,
            rotor_target_x10: 0,
            rotor_available: false,
            yaesu_connected: false,
            yaesu_freq_a: 0,
            yaesu_freq_b: 0,
            yaesu_mode: 1,
            yaesu_smeter: 0,
            yaesu_tx_active: false,
            yaesu_power_on: false,
            yaesu_volume: config.yaesu_volume,
            yaesu_popout: config.yaesu_popout,
            yaesu_popout_pos: None,
            yaesu_popout_size: None,
            yaesu_popout_first_frame: true,
            yaesu_enable_sent: false,
            yaesu_mic_gain: 0.5,
            yaesu_eq_enabled: {
                // Load active EQ profile from config
                config.yaesu_eq_profiles.iter()
                    .find(|(n, _, _)| n == &config.yaesu_eq_active)
                    .map(|(_, e, _)| *e).unwrap_or(false)
            },
            yaesu_eq_gains: {
                config.yaesu_eq_profiles.iter()
                    .find(|(n, _, _)| n == &config.yaesu_eq_active)
                    .map(|(_, _, g)| *g).unwrap_or([0.0; 5])
            },
            yaesu_eq_profiles: config.yaesu_eq_profiles.clone(),
            yaesu_eq_active_profile: config.yaesu_eq_active.clone(),
            yaesu_eq_new_name: String::new(),
            yaesu_squelch: 0,
            yaesu_rf_gain: 255,
            yaesu_radio_mic_gain: 50,
            yaesu_rf_power: 50,
            yaesu_scan_active: false,
            yaesu_split_active: false,
            yaesu_tuner_active: false,
            yaesu_in_memory_mode: false,
            yaesu_current_mem_ch: None,
            yaesu_enabled: config.yaesu_enabled,
            yaesu_mem_channels: {
                let mem_file = config.yaesu_mem_file.clone();
                if !mem_file.is_empty() {
                    match yaesu_memory::parse_tab_file(std::path::Path::new(&mem_file)) {
                        Ok(ch) => { log::info!("Loaded {} Yaesu memory channels from {}", ch.len(), mem_file); ch }
                        Err(e) => { log::warn!("Yaesu memory file: {}", e); Vec::new() }
                    }
                } else { Vec::new() }
            },
            yaesu_mem_file: config.yaesu_mem_file.clone(),
            yaesu_mem_selected: None,
            yaesu_mem_filter: String::new(),
            yaesu_mem_dirty: false,
            yaesu_mem_radio_received: false,
            yaesu_menu_items: Vec::new(),
            yaesu_menu_received: false,
            rotor_goto_input: String::new(),
            dx_spots: Vec::new(),
            smooth_display_center_hz: 0.0,
            rx2_smooth_display_center_hz: 0.0,
            smooth_alpha: 1.0,
            last_frame_time: Instant::now(),
            rx2_enabled: config.rx2_enabled,
            rx2_popout: false,
            popout_joined: config.popout_joined,
            popout_meter_analog: config.popout_meter_analog,
            popout_rx1_smeter_rect: egui::Rect::NOTHING,
            popout_rx2_smeter_rect: egui::Rect::NOTHING,
            rx2_volume: config.rx2_volume,
            rx2_af_gain_display: 0,
            rx2_frequency_hz: 0,
            rx2_mode: 0,
            rx2_smeter: 0,
            rx2_smeter_peak: 0,
            rx2_smeter_peak_time: Instant::now(),
            rx2_filter_low_hz: 0,
            rx2_filter_high_hz: 0,
            rx2_filter_changed_at: None,
            rx2_nr_level: 0,
            rx2_anf_on: false,
            rx2_freq_step_index: 3, // default 1 kHz
            rx2_freq_editing: false,
            rx2_freq_edit_text: String::new(),
            rx2_spectrum_bins: Vec::new(),
            rx2_spectrum_center_hz: 0,
            rx2_spectrum_span_hz: 0,
            rx2_last_spectrum_seq: 0,
            rx2_full_spectrum_bins: Vec::new(),
            rx2_full_spectrum_center_hz: 0,
            rx2_full_spectrum_span_hz: 0,
            rx2_full_spectrum_sequence: 0,
            rx2_spectrum_zoom: 32.0,
            rx2_spectrum_pan: 0.0,
            rx2_last_sent_zoom: 0.0,
            rx2_last_sent_pan: 0.0,
            rx2_zoom_pan_changed_at: None,
            rx2_waterfall: WaterfallRingBuffer::new(200),
            rx2_spectrum_ref_db: config.rx2_spectrum_ref_db,
            rx2_spectrum_range_db: config.rx2_spectrum_range_db,
            rx2_auto_ref_enabled: config.rx2_auto_ref_enabled,
            rx2_auto_ref_value: -30.0,
            rx2_auto_ref_frames: 0,
            rx2_auto_ref_initialized: false,
            rx2_waterfall_contrast: config.rx2_waterfall_contrast,
            vfo_sync: false,
            mon_on: false,
            agc_mode: 3,
            agc_gain: 80,
            agc_auto_rx1: false,
            agc_auto_rx2: false,
            rit_enable: false,
            rit_offset: 0,
            xit_enable: false,
            xit_offset: 0,
            sql_enable: false,
            sql_level: 0,
            nb_enable: false,
            nb_level: 0,
            cw_keyer_speed: 20,
            vfo_lock: false,
            binaural: false,
            apf_enable: false,
            rx2_agc_mode: 3,
            rx2_agc_gain: 80,
            rx2_sql_enable: false,
            rx2_sql_level: 0,
            rx2_nb_enable: false,
            rx2_nb_level: 0,
            rx2_binaural: false,
            rx2_apf_enable: false,
            rx2_vfo_lock: false,
            mute: false,
            rx_mute: false,
            nf_enable: false,
            rx2_nf_enable: false,
            rx_balance: 0,
            tune_drive: 0,
            mon_volume: -40,
            tci_control_changed_at: None,
            yaesu_control_changed_at: None,
            midi: crate::midi::MidiManager::new(),
            midi_ports: Vec::new(),
            midi_selected_port: config.midi_device.clone(),
            midi_learn_for: None,
            midi_learn_action: crate::midi::MidiAction::Ptt,
            midi_last_event: String::new(),
            midi_encoder_hz: config.midi_encoder_hz,
            midi_last_dir_a: 0,
            midi_last_dir_b: 0,
            catsync: {
                let mut cs = crate::catsync::CatSync::new();
                cs.enabled = config.catsync_enabled;
                if !config.catsync_url.is_empty() {
                    cs.websdr_url = config.catsync_url;
                }
                cs.favorites = config.catsync_favorites;
                cs
            },
        };

        // Load MIDI mappings from config
        let midi_mappings: Vec<crate::midi::MidiMapping> = config.midi_mappings.iter()
            .filter_map(|s| crate::midi::MidiMapping::from_config(s))
            .collect();
        app.midi.set_mappings(midi_mappings);

        // Auto-connect MIDI if device was saved
        if !config.midi_device.is_empty() {
            app.midi_ports = crate::midi::MidiManager::list_ports();
            if app.midi_ports.contains(&config.midi_device) {
                app.midi.connect(&config.midi_device);
            }
        }

        app
    }

    fn save_ptt_config(&self) {
        if let Ok(exe) = std::env::current_exe() {
            let path = exe.with_file_name("thetislink-client.conf");
            if let Ok(mut content) = std::fs::read_to_string(&path) {
                // Remove old ptt lines
                content = content.lines()
                    .filter(|l| !l.starts_with("ptt_toggle=") && !l.starts_with("midi_ptt_toggle="))
                    .collect::<Vec<_>>().join("\n");
                content.push_str(&format!("\nptt_toggle={}\nyaesu_ptt_toggle={}\nmidi_ptt_toggle={}\n", self.ptt_toggle_mode, self.yaesu_ptt_toggle_mode, self.midi_ptt_toggle_mode));
                let _ = std::fs::write(path, content);
            }
        }
    }

    fn save_full_config(&self) {
        save_config(
            &self.server_input,
            &self.password_input,
            self.rx_volume,
            self.tx_gain,
            self.vfo_a_volume,
            self.vfo_b_volume,
            self.local_volume,
            self.rx2_volume,
            &self.memories,
            &self.selected_input,
            &self.selected_output,
            self.agc_enabled,
            self.spectrum_enabled,
            self.spectrum_ref_db,
            self.spectrum_range_db,
            self.auto_ref_enabled,
            self.waterfall_contrast,
            self.spectrum_max_bins,
            self.spectrum_fft_size_k,
            self.rx2_spectrum_fft_size_k,
            &self.wf_contrast_per_band,
            self.rx2_spectrum_ref_db,
            self.rx2_spectrum_range_db,
            self.rx2_auto_ref_enabled,
            self.rx2_waterfall_contrast,
            self.rx2_enabled,
            self.popout_joined,
            self.popout_meter_analog,
            self.device_tab,
            self.yaesu_enabled,
            self.yaesu_volume,
            self.yaesu_popout,
            &self.yaesu_eq_active_profile,
            &self.yaesu_eq_profiles,
            &self.yaesu_mem_file,
            &self.band_mem,
            self.window_w,
            self.window_h,
            &self.midi_selected_port,
            &self.midi.get_mappings(),
            self.midi_encoder_hz,
            self.catsync.enabled,
            &self.catsync.websdr_url,
            &self.catsync.favorites,
            &self.mic_profile_map,
        );
    }

    /// Save current RX1 settings for the current band
    fn save_current_band(&mut self, vfo: Vfo) {
        let (freq, mode, flo, fhi, nr) = match vfo {
            Vfo::A => (self.frequency_hz, self.mode, self.filter_low_hz, self.filter_high_hz, self.nr_level),
            Vfo::B => (self.rx2_frequency_hz, self.rx2_mode, self.rx2_filter_low_hz, self.rx2_filter_high_hz, self.rx2_nr_level),
        };
        if freq == 0 { return; }
        let bl = band_label(freq);
        if bl.is_empty() { return; }
        self.band_mem.insert(bl.to_string(), BandMemory {
            frequency_hz: freq, mode, filter_low_hz: flo, filter_high_hz: fhi, nr_level: nr,
        });
    }

    /// Restore band memory for target band, sending all CAT commands
    fn restore_band(&mut self, vfo: Vfo, label: &str, default_freq: u64) {
        if let Some(mem) = self.band_mem.get(label).cloned() {
            let (cur_mode, cur_nr) = match vfo {
                Vfo::A => (self.mode, self.nr_level),
                Vfo::B => (self.rx2_mode, self.rx2_nr_level),
            };
            // Set mode first (filter range depends on mode)
            if mem.mode != cur_mode {
                let _ = self.cmd_tx.send(match vfo {
                    Vfo::A => Command::SetMode(mem.mode),
                    Vfo::B => Command::SetModeRx2(mem.mode),
                });
                match vfo { Vfo::A => self.mode = mem.mode, Vfo::B => self.rx2_mode = mem.mode }
            }
            let _ = self.cmd_tx.send(match vfo {
                Vfo::A => Command::SetFrequency(mem.frequency_hz),
                Vfo::B => Command::SetFrequencyRx2(mem.frequency_hz),
            });
            match vfo {
                Vfo::A => { self.set_pending_freq_a(mem.frequency_hz); }
                Vfo::B => { self.set_pending_freq_b(mem.frequency_hz); }
            }
            // Restore filter
            if mem.filter_low_hz != 0 || mem.filter_high_hz != 0 {
                let (flo_id, fhi_id) = match vfo {
                    Vfo::A => (ControlId::FilterLow, ControlId::FilterHigh),
                    Vfo::B => (ControlId::Rx2FilterLow, ControlId::Rx2FilterHigh),
                };
                let _ = self.cmd_tx.send(Command::SetControl(flo_id, mem.filter_low_hz as i16 as u16));
                let _ = self.cmd_tx.send(Command::SetControl(fhi_id, mem.filter_high_hz as i16 as u16));
                match vfo {
                    Vfo::A => { self.filter_low_hz = mem.filter_low_hz; self.filter_high_hz = mem.filter_high_hz; self.filter_changed_at = Some(Instant::now()); }
                    Vfo::B => { self.rx2_filter_low_hz = mem.filter_low_hz; self.rx2_filter_high_hz = mem.filter_high_hz; }
                }
            }
            // Restore NR
            if mem.nr_level != cur_nr {
                let nr_id = match vfo { Vfo::A => ControlId::NoiseReduction, Vfo::B => ControlId::Rx2NoiseReduction };
                let _ = self.cmd_tx.send(Command::SetControl(nr_id, mem.nr_level as u16));
                match vfo { Vfo::A => self.nr_level = mem.nr_level, Vfo::B => self.rx2_nr_level = mem.nr_level }
            }
        } else {
            let _ = self.cmd_tx.send(match vfo {
                Vfo::A => Command::SetFrequency(default_freq),
                Vfo::B => Command::SetFrequencyRx2(default_freq),
            });
            match vfo {
                Vfo::A => { self.set_pending_freq_a(default_freq); }
                Vfo::B => { self.set_pending_freq_b(default_freq); }
            }
        }
    }

    // ---------------------------------------------------------------------
    // controls-scaffolding — sub-stap 4 writeback-extract
    // ---------------------------------------------------------------------

    /// Bouw een `RxChannelState` snapshot voor RX1. Neemt `freq_edit_text`
    /// per `std::mem::take` (geen clone); de writeback zet hem weer terug.
    fn rx1_snap(&mut self) -> controls::RxChannelState {
        controls::RxChannelState {
            frequency_hz: self.frequency_hz,
            mode: self.mode,
            freq_step_index: self.freq_step_index,
            freq_editing: self.freq_editing,
            freq_edit_text: std::mem::take(&mut self.freq_edit_text),
            pending_freq_hz: None,
        }
    }

    /// Bouw een `RxChannelState` snapshot voor RX2. Draagt nu ook inline-edit
    /// state (PATCH-rx2-inline-edit — symmetrisch met RX1).
    fn rx2_snap(&mut self) -> controls::RxChannelState {
        controls::RxChannelState {
            frequency_hz: self.rx2_frequency_hz,
            mode: self.rx2_mode,
            freq_step_index: self.rx2_freq_step_index,
            freq_editing: self.rx2_freq_editing,
            freq_edit_text: std::mem::take(&mut self.rx2_freq_edit_text),
            pending_freq_hz: None,
        }
    }

    fn shared_snap(&self) -> controls::SharedUiState {
        controls::SharedUiState {
            vfo_sync: false,
            spectrum_enabled: self.spectrum_enabled,
            popout_joined: self.popout_joined,
        }
    }

    /// Schrijf mogelijk gemuteerde snap-velden terug naar `self`.
    /// Idempotent: als de helper niets muteerde zijn de waarden gelijk.
    fn apply_rx_writeback(
        &mut self,
        channel: controls::RxChannel,
        snap: &mut controls::RxChannelState,
    ) {
        match channel {
            controls::RxChannel::Rx1 => {
                self.freq_editing = snap.freq_editing;
                self.freq_edit_text = std::mem::take(&mut snap.freq_edit_text);
                self.freq_step_index = snap.freq_step_index;
            }
            controls::RxChannel::Rx2 => {
                self.rx2_freq_editing = snap.freq_editing;
                self.rx2_freq_edit_text = std::mem::take(&mut snap.freq_edit_text);
                self.rx2_freq_step_index = snap.freq_step_index;
            }
        }
    }

    /// Scaffold voor een control-helper-call: bouwt snap + ControlContext,
    /// roept `action` aan, schrijft snap terug naar `self`.
    ///
    /// Gebruikt de app-level `ui_event_sink`. De sink is `Arc<dyn UiEventSink>`
    /// zodat test-mode `RecordingSink` kan swappen zonder call-sites te raken.
    fn with_rx_ctx<R>(
        &mut self,
        channel: controls::RxChannel,
        density: controls::UiDensity,
        surface: controls::UiSurface,
        action: impl FnOnce(&mut controls::ControlContext) -> R,
    ) -> R {
        let sink = self.ui_event_sink.clone();
        let mut rx_snap = match channel {
            controls::RxChannel::Rx1 => self.rx1_snap(),
            controls::RxChannel::Rx2 => self.rx2_snap(),
        };
        let mut shared_snap = self.shared_snap();
        let connected = self.connected;
        let result = {
            let mut ctx = controls::ControlContext::new(
                connected,
                density,
                surface,
                channel,
                &self.cmd_tx,
                &mut rx_snap,
                &mut shared_snap,
                &*sink,
            );
            action(&mut ctx)
        };
        self.apply_rx_writeback(channel, &mut rx_snap);
        result
    }

    /// Verwerk een band-klik uit `controls::render_band_selector`.
    ///
    /// Band-switch is multi-command (SetMode, SetFrequency, filter-IDs, NR) via
    /// `restore_band`. Daarom wordt niet `ctx.dispatch()` gebruikt (single-command),
    /// maar handmatig bookended: `IntentEmitted` + conditional `CommandSent` /
    /// `CommandBlocked`. Frame-race safety: connected wordt op emit-tijd gelezen,
    /// disconnected-pad slaat `save_current_band`/`restore_band` over om UI-drift
    /// te voorkomen.
    fn handle_band_switch(&mut self, vfo: Vfo, click: controls::BandClick) {
        let sink = self.ui_event_sink.clone();
        let channel = match vfo {
            Vfo::A => controls::RxChannel::Rx1,
            Vfo::B => controls::RxChannel::Rx2,
        };
        let connected = self.connected;
        let intent = controls::UiIntent::SelectBand {
            channel,
            band_hz: click.default_freq_hz,
        };
        let intent_id = sink.record_intent(&intent, connected);
        if connected {
            self.save_current_band(vfo);
            self.restore_band(vfo, click.label, click.default_freq_hz);
            self.save_full_config();
            sink.emit(controls::UiEvent::CommandSent {
                intent_kind: "select_band",
                connected,
                intent_id,
            });
        } else {
            sink.emit(controls::UiEvent::CommandBlocked {
                intent_kind: "select_band",
                reason: controls::CommandBlockReason::Disconnected,
                intent_id,
            });
        }
    }

    fn sync_state(&mut self) {
        let state = self.state_rx.borrow().clone();
        // Send Yaesu enable on first connect if persisted as enabled
        if state.connected && self.yaesu_enabled && !self.yaesu_enable_sent {
            let _ = self.cmd_tx.send(Command::SetControl(
                ControlId::YaesuEnable, 1));
            let _ = self.cmd_tx.send(Command::SetYaesuVolume(self.yaesu_volume));
            // Sync local mic gain to engine
            let _ = self.cmd_tx.send(Command::SetYaesuTxGain(self.yaesu_mic_gain));
            // Auto-read memory channels for channel name info
            self.yaesu_mem_radio_received = false;
            let _ = self.cmd_tx.send(Command::SetControl(
                ControlId::YaesuReadMemories, 0));
            self.yaesu_enable_sent = true;
        }
        if !state.connected {
            self.yaesu_enable_sent = false;
        }
        self.connected = state.connected;
        self.ptt_denied = state.ptt_denied;
        self.rtt_ms = state.rtt_ms;
        self.jitter_ms = state.jitter_ms;
        self.buffer_depth = state.buffer_depth;
        self.rx_packets = state.rx_packets;
        self.loss_percent = state.loss_percent;
        self.capture_level = state.capture_level;
        self.playback_level = state.playback_level;
        self.playback_level_bin_r = state.playback_level_bin_r;
        self.playback_level_rx2 = state.playback_level_rx2;
        self.playback_level_yaesu = state.playback_level_yaesu;
        self.yaesu_mic_level = state.yaesu_mic_level;
        // Clear pending freq: must be at least 200ms old AND (exact match or >1s stale)
        if let Some(pf) = self.pending_freq {
            let age_ms = self.pending_freq_at.map_or(u128::MAX, |t| t.elapsed().as_millis());
            if age_ms >= 200 {
                let server_delta = (state.frequency_hz as i64 - pf as i64).unsigned_abs();
                if server_delta == 0 || age_ms > 1000 {
                    self.pending_freq = None;
                    self.pending_freq_at = None;
                    self.rx1_force_full_tuning = false;
                }
            }
        }
        // Accept server frequency only when no pending change
        if self.pending_freq.is_none() {
            self.frequency_hz = state.frequency_hz;
        }
        // UltraBeam auto-track: send SetFrequency when tracked VFO changes by >= 25 kHz
        if self.ub_auto_track && self.ub_connected {
            let (track_hz, _) = self.ub_track_vfo();
            let track_khz = (track_hz / 1000) as u16;
            let diff = (track_khz as i32 - self.ub_last_auto_khz as i32).unsigned_abs();
            if track_khz >= 1800 && track_khz <= 54000 && diff >= 25 {
                self.ub_last_auto_khz = track_khz;
                let _ = self.cmd_tx.send(Command::UbSetFrequency(track_khz, self.ub_direction));
            }
        }
        // Accept mode from server only if no recent local change
        let mode_accept = self.tci_control_changed_at
            .map_or(true, |t| t.elapsed().as_millis() > 500);
        if mode_accept && state.mode != self.mode {
            self.filter_changed_at = None; // accept new filter values on mode change
            self.mode = state.mode;
        }
        self.smeter = state.smeter;
        if state.smeter >= self.smeter_peak {
            self.smeter_peak = state.smeter;
            self.smeter_peak_time = Instant::now();
        } else if self.smeter_peak_time.elapsed().as_secs_f32() > 2.0 {
            self.smeter_peak = state.smeter;
            self.smeter_peak_time = Instant::now();
        }
        // Reset zoom on reconnect (connected false→true) or power ON
        // Span is reset to 0 so the first spectrum packet triggers zoom calculation.
        let reconnected = state.connected && !self.connected;
        if reconnected || (state.power_on && !self.power_on) {
            self.full_spectrum_span_hz = 0;
            self.spectrum_pan = 0.0;
            self.last_sent_zoom = 0.0;
            self.last_sent_pan = 0.0;
            self.zoom_pan_changed_at = Some(Instant::now());
            self.rx2_full_spectrum_span_hz = 0;
            self.rx2_spectrum_pan = 0.0;
            self.rx2_last_sent_zoom = 0.0;
            self.rx2_last_sent_pan = 0.0;
            self.rx2_zoom_pan_changed_at = Some(Instant::now());
            // Reset TCI control states to defaults — server will push current values
            self.vfo_sync = false;
            self.mon_on = false;
            self.nb_enable = false;
            self.nb_level = 0;
            self.anf_on = false;
            self.rx2_nb_enable = false;
            self.rx2_nb_level = 0;
            self.rx2_anf_on = false;
            self.ddc_sample_rate_rx1 = 0;
            self.ddc_sample_rate_rx2 = 0;
        }
        self.power_on = state.power_on;
        self.tx_profile = state.tx_profile;
        // If server sends TX profile names (TCI mode), override local config
        if !state.tx_profile_names.is_empty() {
            let server_profiles: Vec<(u8, String)> = state.tx_profile_names.iter()
                .enumerate()
                .map(|(i, n)| (i as u8, n.clone()))
                .collect();
            if server_profiles != self.tx_profiles {
                self.tx_profiles = server_profiles;
            }
        }
        self.nr_level = state.nr_level;
        self.anf_on = state.anf_on;
        self.drive_level = state.drive_level;
        if state.rx_af_gain > 0 {
            self.rx_volume = state.rx_af_gain as f32 / 100.0;
        }
        self.audio_error = state.audio_error;
        self.agc_enabled = state.agc_enabled;
        self.other_tx = state.other_tx;
        if !state.playing { self.playing = false; }
        if let Some(ref p) = state.last_recorded_path {
            self.last_recorded_path = Some(p.clone());
        }
        self.thetis_swr_x100 = state.thetis_swr_x100;
        // Once user changes filter locally, client is authoritative until mode changes.
        // filter_changed_at is cleared on mode change (above), so new mode values are accepted.
        if self.filter_changed_at.is_none() {
            self.filter_low_hz = state.filter_low_hz;
            self.filter_high_hz = state.filter_high_hz;
        }
        self.thetis_starting = state.thetis_starting;
        // TCI controls — suppress server sync for 500ms after local change
        let tci_accept = self.tci_control_changed_at
            .map_or(true, |t| t.elapsed().as_millis() > 1000);
        if tci_accept {
            self.tci_control_changed_at = None;
            self.agc_mode = state.agc_mode;
            self.agc_gain = state.agc_gain;
            self.agc_auto_rx1 = state.agc_auto_rx1;
            self.agc_auto_rx2 = state.agc_auto_rx2;
            self.rit_enable = state.rit_enable;
            self.rit_offset = state.rit_offset;
            self.xit_enable = state.xit_enable;
            self.xit_offset = state.xit_offset;
            self.sql_enable = state.sql_enable;
            self.sql_level = state.sql_level;
            self.nb_enable = state.nb_enable;
            self.nb_level = state.nb_level;
            self.cw_keyer_speed = state.cw_keyer_speed;
            self.vfo_lock = state.vfo_lock;
            self.binaural = state.binaural;
            self.apf_enable = state.apf_enable;
            self.rx2_agc_mode = state.rx2_agc_mode;
            self.rx2_agc_gain = state.rx2_agc_gain;
            self.rx2_sql_enable = state.rx2_sql_enable;
            self.rx2_sql_level = state.rx2_sql_level;
            self.rx2_nb_enable = state.rx2_nb_enable;
            self.rx2_nb_level = if state.rx2_nb_enable { self.rx2_nb_level.max(1) } else { 0 };
            self.rx2_binaural = state.rx2_binaural;
            self.rx2_apf_enable = state.rx2_apf_enable;
            self.rx2_vfo_lock = state.rx2_vfo_lock;
            self.mute = state.mute;
            self.rx_mute = state.rx_mute;
            self.nf_enable = state.nf_enable;
            self.rx2_nf_enable = state.rx2_nf_enable;
            self.rx_balance = -state.rx_balance; // Negate: TCI +40=left, slider -40=left
            self.tune_drive = state.tune_drive;
            self.mon_volume = state.mon_volume;
            self.ddc_sample_rate_rx1 = state.ddc_sample_rate_rx1;
            self.ddc_sample_rate_rx2 = state.ddc_sample_rate_rx2;
        }
        // Spectrum
        if state.spectrum_sequence != self.last_spectrum_seq && !state.spectrum_bins.is_empty() {
            self.spectrum_bins = state.spectrum_bins;
            self.spectrum_center_hz = state.spectrum_center_hz;
            self.spectrum_span_hz = state.spectrum_span_hz;
            self.spectrum_ref_level = state.spectrum_ref_level;
            self.spectrum_db_per_unit = state.spectrum_db_per_unit;
            self.last_spectrum_seq = state.spectrum_sequence;
        }
        // Full DDC spectrum (for waterfall)
        if state.full_spectrum_sequence != self.full_spectrum_sequence && !state.full_spectrum_bins.is_empty() {
            // Adjust default zoom when span first becomes known (0 → real value)
            let old_span = self.full_spectrum_span_hz;
            self.full_spectrum_bins = state.full_spectrum_bins;
            self.full_spectrum_center_hz = state.full_spectrum_center_hz;
            self.full_spectrum_span_hz = state.full_spectrum_span_hz;
            self.full_spectrum_sequence = state.full_spectrum_sequence;
            if old_span == 0 && self.full_spectrum_span_hz > 0 {
                self.spectrum_zoom = default_zoom_for_span(self.full_spectrum_span_hz);
                self.spectrum_pan = 0.0;
                self.last_sent_zoom = 0.0;
                self.last_sent_pan = 0.0;
                self.zoom_pan_changed_at = Some(Instant::now());
            }
        }
        // (pending_freq already cleared above, before frequency acceptance)

        // Delayed auto_ref restore after TX→RX transition
        if let Some(at) = self.tx_spectrum_restore_auto_at {
            if Instant::now() >= at {
                if let Some(saved) = self.tx_spectrum_saved_auto_ref.take() {
                    self.auto_ref_enabled = saved;
                    if saved {
                        self.auto_ref_frames = 0;
                        self.auto_ref_initialized = false;
                    }
                }
                self.tx_spectrum_restore_auto_at = None;
            }
        }

        // Auto ref level: compute EMA from average noise floor (excluding RX filter)
        if self.auto_ref_enabled && !self.spectrum_bins.is_empty() && self.spectrum_span_hz > 0 {
            let num_bins = self.spectrum_bins.len() as f32;
            let hz_per_bin = self.spectrum_span_hz as f64 / num_bins as f64;
            let start_hz = self.spectrum_center_hz as f64 - self.spectrum_span_hz as f64 / 2.0;
            // Filter range in bin indices
            let filter_lo_hz = self.frequency_hz as f64 + self.filter_low_hz as f64;
            let filter_hi_hz = self.frequency_hz as f64 + self.filter_high_hz as f64;
            let filter_lo_bin = ((filter_lo_hz - start_hz) / hz_per_bin) as i32;
            let filter_hi_bin = ((filter_hi_hz - start_hz) / hz_per_bin) as i32;

            let mut sum_db = 0.0f64;
            let mut count = 0u32;
            for (i, &val) in self.spectrum_bins.iter().enumerate() {
                let idx = i as i32;
                if idx >= filter_lo_bin && idx <= filter_hi_bin {
                    continue; // skip filter passband
                }
                let db = -150.0 + (val as f64 / 65535.0) * 120.0;
                sum_db += db;
                count += 1;
            }
            if count > 0 {
                let avg_db = sum_db / count as f64;
                let target = avg_db as f32 + self.spectrum_range_db - 2.0;
                if !self.auto_ref_initialized {
                    self.auto_ref_value = target;
                    self.auto_ref_initialized = true;
                } else {
                    let alpha = if self.auto_ref_frames < 45 { 0.10 } else { 0.002 };
                    self.auto_ref_value = alpha * target + (1.0 - alpha) * self.auto_ref_value;
                }
                self.spectrum_ref_db = self.auto_ref_value;
                self.auto_ref_frames += 1;
            }
        }

        // Auto ref level for RX2: same logic as RX1
        if self.rx2_auto_ref_enabled && !self.rx2_spectrum_bins.is_empty() && self.rx2_spectrum_span_hz > 0 {
            let num_bins = self.rx2_spectrum_bins.len() as f32;
            let hz_per_bin = self.rx2_spectrum_span_hz as f64 / num_bins as f64;
            let start_hz = self.rx2_spectrum_center_hz as f64 - self.rx2_spectrum_span_hz as f64 / 2.0;
            let filter_low = if self.rx2_filter_low_hz != 0 || self.rx2_filter_high_hz != 0 {
                self.rx2_filter_low_hz
            } else {
                self.filter_low_hz
            };
            let filter_high = if self.rx2_filter_low_hz != 0 || self.rx2_filter_high_hz != 0 {
                self.rx2_filter_high_hz
            } else {
                self.filter_high_hz
            };
            let filter_lo_hz = self.rx2_frequency_hz as f64 + filter_low as f64;
            let filter_hi_hz = self.rx2_frequency_hz as f64 + filter_high as f64;
            let filter_lo_bin = ((filter_lo_hz - start_hz) / hz_per_bin) as i32;
            let filter_hi_bin = ((filter_hi_hz - start_hz) / hz_per_bin) as i32;

            let mut sum_db = 0.0f64;
            let mut count = 0u32;
            for (i, &val) in self.rx2_spectrum_bins.iter().enumerate() {
                let idx = i as i32;
                if idx >= filter_lo_bin && idx <= filter_hi_bin {
                    continue;
                }
                let db = -150.0 + (val as f64 / 65535.0) * 120.0;
                sum_db += db;
                count += 1;
            }
            if count > 0 {
                let avg_db = sum_db / count as f64;
                let target = avg_db as f32 + self.rx2_spectrum_range_db - 2.0;
                if !self.rx2_auto_ref_initialized {
                    self.rx2_auto_ref_value = target;
                    self.rx2_auto_ref_initialized = true;
                } else {
                    let alpha = if self.rx2_auto_ref_frames < 45 { 0.10 } else { 0.002 };
                    self.rx2_auto_ref_value = alpha * target + (1.0 - alpha) * self.rx2_auto_ref_value;
                }
                self.rx2_spectrum_ref_db = self.rx2_auto_ref_value;
                self.rx2_auto_ref_frames += 1;
            }
        }

        // Per-band WF contrast tracking
        let new_band = freq_to_band(self.frequency_hz);
        if new_band != self.current_band {
            // Save current contrast for old band
            if let Some(ref old) = self.current_band {
                self.wf_contrast_per_band.insert(old.clone(), self.waterfall_contrast);
            }
            // Load contrast for new band (or default 1.2)
            if let Some(ref nb) = new_band {
                self.waterfall_contrast = self.wf_contrast_per_band.get(nb).copied().unwrap_or(1.2);
            }
            // Reset auto-ref to fast convergence on band change
            if self.auto_ref_enabled {
                self.auto_ref_frames = 0;
                self.auto_ref_initialized = false;
            }
            self.current_band = new_band;
        }

        // Amplitec state
        let old_a = self.amplitec_switch_a;
        let old_b = self.amplitec_switch_b;
        let was_connected = self.amplitec_connected;
        self.amplitec_connected = state.amplitec_connected;
        self.amplitec_switch_a = state.amplitec_switch_a;
        self.amplitec_switch_b = state.amplitec_switch_b;
        if !state.amplitec_labels.is_empty() {
            self.amplitec_labels = state.amplitec_labels;
        }
        // Log changes
        let now = chrono_time();
        if state.amplitec_connected && !was_connected {
            self.amplitec_log_push(&now, "Connected");
        } else if !state.amplitec_connected && was_connected {
            self.amplitec_log_push(&now, "Disconnected");
        }
        if state.amplitec_switch_a != old_a && state.amplitec_switch_a > 0 {
            let label = self.amplitec_label_a(state.amplitec_switch_a);
            self.amplitec_log_push(&now, &format!("Switch A -> {} ({})", state.amplitec_switch_a, label));
        }
        if state.amplitec_switch_b != old_b && state.amplitec_switch_b > 0 {
            let label = self.amplitec_label_b(state.amplitec_switch_b);
            self.amplitec_log_push(&now, &format!("Switch B -> {} ({})", state.amplitec_switch_b, label));
        }

        // Tuner state
        let old_tuner_state = self.tuner_state;
        self.tuner_connected = state.tuner_connected;
        self.tuner_state = state.tuner_state;
        self.tuner_can_tune = state.tuner_can_tune;
        // Track tune frequency: on real tune (TUNING → DONE_OK/DONE_ASSUMED) or first
        // done-state after connect (tune_freq still 0). Ignores the fake
        // IDLE → done-state transitions from the server's stale override.
        let tuner_done = state.tuner_state == 2 || state.tuner_state == 5;
        if tuner_done && (old_tuner_state == 1 || self.tuner_tune_freq == 0) {
            self.tuner_tune_freq = self.frequency_hz;
        }

        // SPE Expert state
        self.spe_connected = state.spe_connected;
        self.spe_state = state.spe_state;
        self.spe_band = state.spe_band;
        self.spe_ptt = state.spe_ptt;
        self.spe_power_w = state.spe_power_w;
        self.spe_swr_x10 = state.spe_swr_x10;
        self.spe_temp = state.spe_temp;
        self.spe_warning = state.spe_warning;
        self.spe_alarm = state.spe_alarm;
        self.spe_power_level = state.spe_power_level;
        self.spe_antenna = state.spe_antenna;
        self.spe_input = state.spe_input;
        self.spe_voltage_x10 = state.spe_voltage_x10;
        self.spe_current_x10 = state.spe_current_x10;
        self.spe_atu_bypassed = state.spe_atu_bypassed;
        self.spe_available = state.spe_available;
        self.spe_active = state.spe_active;

        // RF2K-S Amplifier state
        self.rf2k_connected = state.rf2k_connected;
        self.rf2k_operate = state.rf2k_operate;
        self.rf2k_band = state.rf2k_band;
        self.rf2k_frequency_khz = state.rf2k_frequency_khz;
        self.rf2k_temperature_x10 = state.rf2k_temperature_x10;
        self.rf2k_voltage_x10 = state.rf2k_voltage_x10;
        self.rf2k_current_x10 = state.rf2k_current_x10;
        self.rf2k_forward_w = state.rf2k_forward_w;
        self.rf2k_reflected_w = state.rf2k_reflected_w;
        self.rf2k_swr_x100 = state.rf2k_swr_x100;
        self.rf2k_max_forward_w = state.rf2k_max_forward_w;
        self.rf2k_max_reflected_w = state.rf2k_max_reflected_w;
        self.rf2k_max_swr_x100 = state.rf2k_max_swr_x100;
        self.rf2k_error_state = state.rf2k_error_state;
        self.rf2k_error_text = state.rf2k_error_text.clone();
        self.rf2k_antenna_type = state.rf2k_antenna_type;
        self.rf2k_antenna_number = state.rf2k_antenna_number;
        self.rf2k_tuner_mode = state.rf2k_tuner_mode;
        self.rf2k_tuner_setup = state.rf2k_tuner_setup.clone();
        self.rf2k_tuner_l_nh = state.rf2k_tuner_l_nh;
        self.rf2k_tuner_c_pf = state.rf2k_tuner_c_pf;
        self.rf2k_drive_w = state.rf2k_drive_w;
        self.rf2k_modulation = state.rf2k_modulation.clone();
        self.rf2k_max_power_w = state.rf2k_max_power_w;
        self.rf2k_device_name = state.rf2k_device_name.clone();
        self.rf2k_available = state.rf2k_available;
        self.rf2k_active = state.rf2k_active;
        // Debug (Fase D)
        self.rf2k_debug_available = state.rf2k_debug_available;
        self.rf2k_bias_pct_x10 = state.rf2k_bias_pct_x10;
        self.rf2k_psu_source = state.rf2k_psu_source;
        self.rf2k_uptime_s = state.rf2k_uptime_s;
        self.rf2k_tx_time_s = state.rf2k_tx_time_s;
        self.rf2k_error_count = state.rf2k_error_count;
        self.rf2k_error_history = state.rf2k_error_history.clone();
        self.rf2k_storage_bank = state.rf2k_storage_bank;
        self.rf2k_hw_revision = state.rf2k_hw_revision.clone();
        self.rf2k_frq_delay = state.rf2k_frq_delay;
        self.rf2k_autotune_threshold_x10 = state.rf2k_autotune_threshold_x10;
        self.rf2k_dac_alc = state.rf2k_dac_alc;
        self.rf2k_high_power = state.rf2k_high_power;
        self.rf2k_tuner_6m = state.rf2k_tuner_6m;
        self.rf2k_band_gap_allowed = state.rf2k_band_gap_allowed;
        self.rf2k_controller_version = state.rf2k_controller_version;
        self.rf2k_drive_config_ssb = state.rf2k_drive_config_ssb;
        self.rf2k_drive_config_am = state.rf2k_drive_config_am;
        self.rf2k_drive_config_cont = state.rf2k_drive_config_cont;

        // UltraBeam
        self.ub_connected = state.ub_connected;
        self.ub_frequency_khz = state.ub_frequency_khz;
        self.ub_band = state.ub_band;
        self.ub_direction = state.ub_direction;
        self.ub_off_state = state.ub_off_state;
        self.ub_motors_moving = state.ub_motors_moving;
        self.ub_motor_completion = state.ub_motor_completion;
        self.ub_fw_major = state.ub_fw_major;
        self.ub_fw_minor = state.ub_fw_minor;
        self.ub_available = state.ub_available;
        self.ub_elements_mm = state.ub_elements_mm;

        // Rotor
        self.rotor_connected = state.rotor_connected;
        self.rotor_angle_x10 = state.rotor_angle_x10;
        self.rotor_rotating = state.rotor_rotating;
        self.rotor_target_x10 = state.rotor_target_x10;
        self.rotor_available = state.rotor_available;
        // Yaesu
        self.yaesu_connected = state.yaesu_connected;
        self.yaesu_freq_a = state.yaesu_freq_a;
        self.yaesu_freq_b = state.yaesu_freq_b;
        self.yaesu_mode = state.yaesu_mode;
        self.yaesu_smeter = state.yaesu_smeter;
        self.yaesu_tx_active = state.yaesu_tx_active;
        self.yaesu_power_on = state.yaesu_power_on;
        // Sync slider values from radio — debounce 1s after local change
        let yaesu_accept = self.yaesu_control_changed_at
            .map_or(true, |t| t.elapsed().as_millis() > 1000);
        if state.yaesu_connected && yaesu_accept {
            self.yaesu_control_changed_at = None;
            self.yaesu_squelch = state.yaesu_squelch as u16;
            self.yaesu_rf_gain = state.yaesu_rf_gain as u16;
            self.yaesu_rf_power = state.yaesu_tx_power as u16;
        }
        self.yaesu_split_active = state.yaesu_split;
        self.yaesu_scan_active = state.yaesu_scan;
        self.yaesu_in_memory_mode = state.yaesu_vfo_select == 1 || state.yaesu_vfo_select == 2; // 1=Memory, 2=MemTune (not 3=VFO B)
        // Find the current memory channel in our loaded list
        if self.yaesu_in_memory_mode && state.yaesu_memory_channel > 0 {
            self.yaesu_current_mem_ch = self.yaesu_mem_channels.iter()
                .position(|ch| ch.channel_number == state.yaesu_memory_channel);
        }
        // Check for incoming Yaesu data from server (memory or menu)
        if let Some(ref text) = state.yaesu_memory_data {
            if text.starts_with("MENU:") {
                // Menu data
                if !self.yaesu_menu_received {
                    self.yaesu_menu_received = true;
                    let menu_text = &text[5..];
                    let mut items = Vec::new();
                    for line in menu_text.lines() {
                        if let Some((num_str, val)) = line.split_once(':') {
                            if let Ok(num) = num_str.trim().parse::<u16>() {
                                items.push(yaesu_menu::MenuItem { number: num, raw_value: val.to_string() });
                            }
                        }
                    }
                    log::info!("Received {} menu items from radio", items.len());
                    self.yaesu_menu_items = items;
                }
            } else if !self.yaesu_mem_radio_received {
                // Memory channel data
                self.yaesu_mem_radio_received = true;
                match crate::ui::yaesu_memory::parse_tab_string(text) {
                    Ok(mut radio_channels) => {
                        let existing = std::mem::take(&mut self.yaesu_mem_channels);
                        for rch in &mut radio_channels {
                            if rch.name.is_empty() || rch.name.starts_with("CH ") {
                                if let Some(match_ch) = existing.iter().find(|e| e.rx_freq_hz == rch.rx_freq_hz) {
                                    rch.name = match_ch.name.clone();
                                }
                            }
                        }
                        log::info!("Received {} memory channels from radio", radio_channels.len());
                        self.yaesu_mem_channels = radio_channels;
                        self.yaesu_mem_dirty = true;
                    }
                    Err(e) => log::warn!("Parse memory data from radio: {}", e),
                }
            }
        } else {
            self.yaesu_mem_radio_received = false;
            self.yaesu_menu_received = false;
        }
        self.dx_spots = state.dx_spots.clone();

        // RX2 / VFO-B
        self.rx2_enabled = state.rx2_enabled;
        self.vfo_sync = state.vfo_sync;
        self.diversity_enabled = state.diversity_enabled;
        if state.diversity_phase != 0 {
            let decoded = (state.diversity_phase as i32 - 18000) as f32 / 100.0;
            self.diversity_phase = decoded;
        }
        if state.diversity_gain_rx1 != 0 {
            self.diversity_gain_rx1 = state.diversity_gain_rx1 as f32 / 1000.0;
        }
        if state.diversity_gain_rx2 != 0 {
            self.diversity_gain_rx2 = state.diversity_gain_rx2 as f32 / 1000.0;
        }
        self.mon_on = state.mon_on;
        // New TCI controls: client-authoritative (no server broadcast).
        // State is only updated when Thetis pushes TCI notifications via
        // ControlPacket from server, which the engine writes into RadioState.
        // Until then, keep client-local values.
        // Clear RX2 pending freq: must be at least 200ms old AND (exact match or >1s stale)
        if let Some(pf) = self.rx2_pending_freq {
            let age_ms = self.rx2_pending_freq_at.map_or(u128::MAX, |t| t.elapsed().as_millis());
            if age_ms >= 200 {
                let server_delta = (state.frequency_rx2_hz as i64 - pf as i64).unsigned_abs();
                if server_delta == 0 || age_ms > 1000 {
                    self.rx2_pending_freq = None;
                    self.rx2_pending_freq_at = None;
                    self.rx2_force_full_tuning = false;
                }
            }
        }
        // Accept server frequency only when no pending change
        if self.rx2_pending_freq.is_none() {
            self.rx2_frequency_hz = state.frequency_rx2_hz;
        }
        if state.mode_rx2 != self.rx2_mode {
            self.rx2_filter_changed_at = None; // accept new filter values on mode change
        }
        self.rx2_mode = state.mode_rx2;
        self.rx2_smeter = state.smeter_rx2;
        if state.smeter_rx2 >= self.rx2_smeter_peak {
            self.rx2_smeter_peak = state.smeter_rx2;
            self.rx2_smeter_peak_time = Instant::now();
        } else if self.rx2_smeter_peak_time.elapsed().as_secs_f32() > 2.0 {
            self.rx2_smeter_peak = state.smeter_rx2;
            self.rx2_smeter_peak_time = Instant::now();
        }
        // Sync RX2 Vol from Thetis ZZLB (same as RX1 Vol syncs from ZZLA)
        if state.rx2_af_gain != self.rx2_af_gain_display {
            log::info!("UI: RX2 AF gain {} → {}, slider {:.0}% → {:.0}%",
                self.rx2_af_gain_display, state.rx2_af_gain,
                self.rx2_volume * 100.0, state.rx2_af_gain as f32);
            self.rx2_volume = state.rx2_af_gain as f32 / 100.0;
        }
        self.rx2_af_gain_display = state.rx2_af_gain;
        // Once user changes RX2 filter locally, client is authoritative until mode changes.
        if self.rx2_filter_changed_at.is_none() && (state.filter_rx2_low_hz != 0 || state.filter_rx2_high_hz != 0) {
            self.rx2_filter_low_hz = state.filter_rx2_low_hz;
            self.rx2_filter_high_hz = state.filter_rx2_high_hz;
        }
        self.rx2_nr_level = state.rx2_nr_level;
        self.rx2_anf_on = state.rx2_anf_on;
        // RX2 spectrum (view)
        if state.rx2_spectrum_sequence != self.rx2_last_spectrum_seq && !state.rx2_spectrum_bins.is_empty() {
            self.rx2_spectrum_bins = state.rx2_spectrum_bins;
            self.rx2_spectrum_center_hz = state.rx2_spectrum_center_hz;
            self.rx2_spectrum_span_hz = state.rx2_spectrum_span_hz;
            self.rx2_last_spectrum_seq = state.rx2_spectrum_sequence;
        }
        // RX2 full DDC spectrum (for waterfall)
        if state.rx2_full_spectrum_sequence != self.rx2_full_spectrum_sequence && !state.rx2_full_spectrum_bins.is_empty() {
            let old_span = self.rx2_full_spectrum_span_hz;
            self.rx2_full_spectrum_bins = state.rx2_full_spectrum_bins;
            self.rx2_full_spectrum_center_hz = state.rx2_full_spectrum_center_hz;
            self.rx2_full_spectrum_span_hz = state.rx2_full_spectrum_span_hz;
            self.rx2_full_spectrum_sequence = state.rx2_full_spectrum_sequence;
            if old_span == 0 && self.rx2_full_spectrum_span_hz > 0 {
                self.rx2_spectrum_zoom = default_zoom_for_span(self.rx2_full_spectrum_span_hz);
                self.rx2_spectrum_pan = 0.0;
                self.rx2_last_sent_zoom = 0.0;
                self.rx2_last_sent_pan = 0.0;
                self.rx2_zoom_pan_changed_at = Some(Instant::now());
            }
        }
        // (rx2_pending_freq already cleared above, before frequency acceptance)
    }

    fn amplitec_log_push(&mut self, time: &str, msg: &str) {
        if self.amplitec_log.len() >= 100 {
            self.amplitec_log.pop_front();
        }
        self.amplitec_log.push_back((time.to_string(), msg.to_string()));
    }

    /// Determine which VFO the UltraBeam should track based on Amplitec switch position.
    /// If Amplitec switch_b points to the UltraBeam port, use VFO B.
    /// If switch_a points to UltraBeam, use VFO A. Otherwise default to VFO A.
    fn ub_track_vfo(&self) -> (u64, &'static str) {
        // Find UltraBeam port in Amplitec labels (positions 1-6, labels at offset 0-5)
        if !self.amplitec_labels.is_empty() {
            let parts: Vec<&str> = self.amplitec_labels.split(',').collect();
            for i in 0..6usize {
                if i < parts.len() {
                    let lower = parts[i].to_lowercase();
                    if lower.contains("ultrabeam") || lower.contains("ultra beam") || lower.contains("ub") {
                        let ub_pos = (i + 1) as u8;
                        if self.amplitec_switch_b == ub_pos {
                            return (self.rx2_frequency_hz, "VFO B");
                        }
                        if self.amplitec_switch_a == ub_pos {
                            return (self.frequency_hz, "VFO A");
                        }
                        break; // found UltraBeam label but neither switch points to it
                    }
                }
            }
        }
        (self.frequency_hz, "VFO A")
    }

    fn amplitec_label_a(&self, pos: u8) -> String {
        self.amplitec_label(pos, 0)
    }

    fn amplitec_label_b(&self, pos: u8) -> String {
        self.amplitec_label(pos, 6)
    }

    fn amplitec_label(&self, pos: u8, offset: usize) -> String {
        if pos == 0 || pos > 6 { return "?".to_string(); }
        if !self.amplitec_labels.is_empty() {
            let parts: Vec<&str> = self.amplitec_labels.split(',').collect();
            let idx = offset + (pos as usize - 1);
            if idx < parts.len() {
                return parts[idx].to_string();
            }
        }
        format!("{}", pos)
    }



    /// Render spectrum controls + plot + waterfall (used by both inline and pop-out)
    fn render_spectrum_content(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, reserve_bottom: f32, is_popout: bool) {
        // Row 1: Ref + Auto checkbox + Range
        ui.horizontal(|ui| {
            ui.spacing_mut().slider_width = 80.0;
            ui.label("Ref:");
            if self.auto_ref_enabled {
                // Show value but non-interactive
                let mut display_val = self.spectrum_ref_db;
                ui.add_enabled(false, egui::Slider::new(&mut display_val, -90.0..=0.0)
                    .suffix(" dB")
                    .step_by(5.0)
                );
            } else if ui.add(egui::Slider::new(&mut self.spectrum_ref_db, -90.0..=0.0)
                .suffix(" dB")
                .step_by(5.0)
            ).changed() {
                self.save_full_config();
            }
            if ui.checkbox(&mut self.auto_ref_enabled, "Auto").changed() {
                if self.auto_ref_enabled {
                    self.auto_ref_frames = 0;
                    self.auto_ref_initialized = false;
                }
                self.save_full_config();
            }
            ui.label("Range:");
            if ui.add(egui::Slider::new(&mut self.spectrum_range_db, 20.0..=130.0)
                .suffix(" dB")
                .step_by(5.0)
            ).changed() {
                if self.auto_ref_enabled {
                    self.auto_ref_frames = 0;
                    self.auto_ref_initialized = false;
                }
                self.save_full_config();
            }
        });
        // Row 2: Zoom + Pan + WF Contrast
        ui.horizontal(|ui| {
            ui.spacing_mut().slider_width = 80.0;
            ui.label("Zoom:");
            let zoom_changed = ui.add(egui::Slider::new(&mut self.spectrum_zoom, 1.0..=1024.0)
                .logarithmic(true)
                .custom_formatter(|v, _| format!("{:.0}x", v))
            ).changed();
            if zoom_changed {
                let max_pan = (0.5 - 0.5 / self.spectrum_zoom) * 0.05;
                self.spectrum_pan = self.spectrum_pan.clamp(-max_pan, max_pan);
            }
            ui.label("Pan:");
            let max_pan = if self.spectrum_zoom > 1.01 { (0.5 - 0.5 / self.spectrum_zoom) * 0.05 } else { 0.0 };
            let pan_changed = ui.add(egui::Slider::new(&mut self.spectrum_pan, -max_pan..=max_pan)
                .custom_formatter(|v, _| format!("{:+.2}", v))
            ).changed();
            ui.label("WF:");
            if ui.add(egui::Slider::new(&mut self.waterfall_contrast, 0.3..=3.0)
                .logarithmic(true)
                .custom_formatter(|v, _| format!("{:.1}", v))
            ).changed() {
                // Update per-band storage
                if let Some(ref band) = self.current_band {
                    self.wf_contrast_per_band.insert(band.clone(), self.waterfall_contrast);
                }
                self.save_full_config();
            }

            // FFT size selector — labels computed from current DDC sample rate
            let ddc_rate = if self.ddc_sample_rate_rx1 > 0 { self.ddc_sample_rate_rx1 as u32 * 1000 } else { 384_000 };
            let auto_fft = sdr_remote_core::ddc_fft_size(ddc_rate);
            let auto_k = auto_fft / 1024;
            let fft_label = if self.spectrum_fft_size_k == 0 {
                format!("FFT: Auto ({}K)", auto_k)
            } else {
                format!("FFT: {}K", self.spectrum_fft_size_k)
            };
            // Build options: Auto + fixed sizes that make sense for this sample rate
            let hop = |fft_k: u32| -> u32 { let fft = fft_k * 1024; ddc_rate / (fft / 8) };
            let options: Vec<(u16, String)> = {
                let mut opts = vec![(0u16, format!("Auto ({}K, ~{} FFT/s)", auto_k, hop(auto_k as u32)))];
                for &k in &[32u16, 64, 128, 256, 512, 1024] {
                    let fft = k as u32 * 1024;
                    if fft <= ddc_rate * 4 { // reasonable range
                        let fps = hop(k as u32);
                        if fps > 0 {
                            opts.push((k, format!("{}K (~{} FFT/s)", k, fps)));
                        }
                    }
                }
                opts
            };
            egui::ComboBox::from_id_salt("fft_size")
                .selected_text(&fft_label)
                .width(80.0)
                .show_ui(ui, |ui| {
                    for (k, label) in &options {
                        if ui.selectable_label(self.spectrum_fft_size_k == *k, label).clicked() {
                            self.spectrum_fft_size_k = *k;
                            let _ = self.cmd_tx.send(Command::SetSpectrumFftSize(*k));
                            self.save_full_config();
                        }
                    }
                });
            if zoom_changed || pan_changed {
                self.zoom_pan_changed_at = Some(Instant::now());
            }
        });

        // Debounce: send zoom/pan + dynamic bins to server after 100ms stability
        if let Some(changed_at) = self.zoom_pan_changed_at {
            if changed_at.elapsed().as_millis() >= 100 {
                let zoom_diff = (self.spectrum_zoom - self.last_sent_zoom).abs();
                let pan_diff = (self.spectrum_pan - self.last_sent_pan).abs();
                if zoom_diff > 0.01 {
                    let _ = self.cmd_tx.send(Command::SetSpectrumZoom(self.spectrum_zoom));
                    self.last_sent_zoom = self.spectrum_zoom;
                }
                if pan_diff > 0.001 {
                    let _ = self.cmd_tx.send(Command::SetSpectrumPan(self.spectrum_pan));
                    self.last_sent_pan = self.spectrum_pan;
                }
                // Dynamic bins: screen_width × zoom, capped at MAX_SPECTRUM_SEND_BINS
                let pixel_width = ui.available_width().max(100.0) as u32;
                let dynamic_bins = ((pixel_width as f32 * self.spectrum_zoom) as u32)
                    .clamp(512, sdr_remote_core::MAX_SPECTRUM_SEND_BINS as u32) as u16;
                if dynamic_bins != self.spectrum_max_bins {
                    self.spectrum_max_bins = dynamic_bins;
                    let _ = self.cmd_tx.send(Command::SetSpectrumMaxBins(dynamic_bins));
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Rx2SpectrumMaxBins, dynamic_bins));
                }
                self.zoom_pan_changed_at = None;
            }
        }

        // Smooth display center: interpolate toward target for smooth tuning
        let target_center = Self::spectrum_target_center_hz(
            self.frequency_hz,
            self.full_spectrum_span_hz,
            self.spectrum_pan,
            self.spectrum_center_hz,
        );
        let rx1_tuning_active = Self::tuning_latch_active(
            self.rx1_force_full_tuning,
            self.pending_freq,
            self.pending_freq_at,
        );
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame_time).as_secs_f64();
        self.last_frame_time = now;
        // Exponential smoothing: ~90% of the way in ~50ms (alpha = 1 - e^(-dt/tau))
        let tau = 0.02; // 20ms time constant — fast but smooth
        let alpha = (1.0 - (-dt / tau).exp()).clamp(0.0, 1.0);
        self.smooth_alpha = alpha;
        if self.pending_freq.is_some() {
            self.smooth_display_center_hz = target_center.round();
        } else if self.smooth_display_center_hz == 0.0 {
            self.smooth_display_center_hz = target_center;
        } else {
            self.smooth_display_center_hz += (target_center - self.smooth_display_center_hz) * alpha;
        }
        // Snap when very close (< 1 Hz) to avoid perpetual drift
        if (self.smooth_display_center_hz - target_center).abs() < 1.0 {
            self.smooth_display_center_hz = target_center;
        }
        let smooth_center = self.smooth_display_center_hz as u64;
        // VFO marker follows the smooth center (minus pan offset) — stays perfectly stationary
        let smooth_vfo = (self.smooth_display_center_hz
            - self.spectrum_pan as f64 * self.full_spectrum_span_hz as f64) as u64;

        // Dynamic spectrum + waterfall height based on available space
        let available = ui.available_height();
        let spec_area = (available - reserve_bottom).max(200.0);
        let spec_h = (spec_area * 0.45).max(100.0);
        let wf_h = (spec_area * 0.55).max(80.0);

        let (plot_bins, plot_center_hz, plot_span_hz) = if !rx1_tuning_active {
            (&self.spectrum_bins, self.spectrum_center_hz, self.spectrum_span_hz)
        } else {
            (&self.full_spectrum_bins, self.full_spectrum_center_hz, self.full_spectrum_span_hz)
        };

        spectrum_plot(
            ui,
            plot_bins,
            plot_center_hz,
            plot_span_hz,
            smooth_center,
            smooth_vfo,
            self.frequency_hz,
            self.spectrum_ref_db,
            self.spectrum_range_db,
            self.smeter,
            self.ptt,
            self.other_tx,
            self.filter_low_hz,
            self.filter_high_hz,
            self.rit_offset as i32,
            self.rit_enable,
            spec_h,
            &SpectrumPlotConfig { is_popout, ..RX1_PLOT_CONFIG },
            &self.dx_spots,
        );
        render_waterfall(
            ui,
            ctx,
            &mut self.waterfall,
            self.full_spectrum_span_hz,
            smooth_center,
            self.frequency_hz,
            self.spectrum_zoom,
            self.waterfall_contrast,
            self.spectrum_ref_db,
            self.spectrum_range_db,
            wf_h,
            &SpectrumPlotConfig { is_popout, ..RX1_PLOT_CONFIG },
        );
    }

    /// Render RX1 controls only (VFO, S-meter, band, mode, freq step, filter, NR, ANF).
    /// `surface` bepaalt welke UI-oppervlakte dit is (MainTab, PopoutSeparate,
    /// PopoutJoined) — meegegeven aan de controls-helpers voor coverage en events.
    fn render_rx1_controls(&mut self, ui: &mut egui::Ui, surface: controls::UiSurface) {
        if self.popout_meter_analog {
            let total_w = ui.available_width();
            let start = ui.cursor().min;

            // First pass: measure controls natural height at full width
            let measure_rect = egui::Rect::from_min_size(start, egui::vec2(total_w, 500.0));
            let mut measure = ui.new_child(egui::UiBuilder::new().max_rect(measure_rect).layout(egui::Layout::top_down(egui::Align::LEFT)));
            self.render_rx1_controls_inner(&mut measure, surface);
            let controls_h = measure.min_rect().height();

            // Meter width: max 2x height, and leave at least 480px for controls
            let meter_w = (controls_h * 2.0).min(total_w - 480.0).max(0.0);
            let controls_w = total_w - meter_w - if meter_w > 0.0 { 8.0 } else { 0.0 };

            // Left: actual controls render
            let controls_rect = egui::Rect::from_min_size(start, egui::vec2(controls_w, 500.0));
            let mut left = ui.new_child(egui::UiBuilder::new().max_rect(controls_rect).layout(egui::Layout::top_down(egui::Align::LEFT)));
            self.render_rx1_controls_inner(&mut left, surface);

            // Right: analog meter (only if there's room)
            if meter_w > 80.0 {
                let meter_pos = egui::pos2(start.x + controls_w + 4.0, start.y);
                let meter_rect = egui::Rect::from_min_size(meter_pos, egui::vec2(meter_w, controls_h));
                let mut right = ui.new_child(egui::UiBuilder::new().max_rect(meter_rect).layout(egui::Layout::top_down(egui::Align::LEFT)));
                self.popout_rx1_smeter_rect = smeter_analog_sized(&mut right, self.smeter, self.smeter_peak, self.ptt, self.other_tx, Some((meter_w, controls_h)));
            }

            ui.advance_cursor_after_rect(egui::Rect::from_min_size(start, egui::vec2(total_w, controls_h)));
        } else {
            self.render_rx1_controls_inner(ui, surface);
        }
    }

    fn render_rx1_controls_inner(&mut self, ui: &mut egui::Ui, surface: controls::UiSurface) {
        let amber = Color32::from_rgb(255, 170, 40);

        // -- Top bar: frequency + mode (via controls::render_frequency_display) --
        ui.horizontal(|ui| {
            let action = self.with_rx_ctx(
                controls::RxChannel::Rx1,
                controls::UiDensity::Extended,
                surface,
                |ctx| {
                    controls::render_frequency_display(ui, ctx).map(|a| match a {
                        controls::FrequencyDisplayAction::Submit { hz } => {
                            let intent = controls::UiIntent::InlineFreqEdit {
                                channel: controls::RxChannel::Rx1,
                                hz,
                            };
                            let dispatched = ctx.dispatch(intent, Command::SetFrequency(hz));
                            (hz, dispatched)
                        }
                        controls::FrequencyDisplayAction::ScrollTune { delta_hz } => {
                            let new_freq = (ctx.rx_state.frequency_hz as i64 + delta_hz).max(0) as u64;
                            let intent = controls::UiIntent::TuneByDelta {
                                channel: controls::RxChannel::Rx1,
                                delta_hz,
                            };
                            let dispatched = ctx.dispatch(intent, Command::SetFrequency(new_freq));
                            (new_freq, dispatched)
                        }
                    })
                },
            );
            if let Some((new_freq, true)) = action {
                self.set_pending_freq_a(new_freq);
            }

            let mode_label = match self.mode {
                0 => "LSB", 1 => "USB", 2 => "DSB", 3 => "CW-L", 4 => "CW-U",
                5 => "FM", 6 => "AM", 7 => "DIGU", 8 => "SPEC", 9 => "DIGL",
                10 => "SAM", 11 => "DRM", _ => "?",
            };
            ui.label(RichText::new(mode_label).size(16.0).color(amber));

            let bw = self.filter_high_hz - self.filter_low_hz;
            let bw_text = if bw >= 1000 {
                format!("{:.1}k", bw as f32 / 1000.0)
            } else {
                format!("{} Hz", bw)
            };
            ui.label(RichText::new(bw_text).size(12.0).weak());
        });

        // S-meter bar (only in bar mode)
        if !self.popout_meter_analog {
            self.popout_rx1_smeter_rect = smeter_bar_popout(ui, self.smeter, self.smeter_peak, self.ptt, self.other_tx, self.thetis_swr_x100);
        }

        // -- Controls row: VFO A Volume + meter toggle --
        ui.horizontal(|ui| {
            ui.label("VFO A:");
            let vol_slider = egui::Slider::new(&mut self.vfo_a_volume, 0.001..=1.0)
                .logarithmic(true)
                .show_value(false)
                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
            if ui.add(vol_slider).changed() {
                let _ = self.cmd_tx.send(Command::SetVfoAVolume(self.vfo_a_volume));
                self.save_full_config();
            }
            ui.separator();
            let meter_label = if self.popout_meter_analog { "S: Analog" } else { "S: Bar" };
            if ui.small_button(meter_label).clicked() {
                self.popout_meter_analog = !self.popout_meter_analog;
                self.save_full_config();
            }
        });

        // -- Band buttons (via controls::render_band_selector) --
        let band_click = self.with_rx_ctx(
            controls::RxChannel::Rx1,
            controls::UiDensity::Extended,
            surface,
            |ctx| controls::render_band_selector(ui, ctx),
        );
        if let Some(click) = band_click {
            self.handle_band_switch(Vfo::A, click);
        }

        // -- Mode selector (via controls::render_mode_selector) --
        let mode_action = self.with_rx_ctx(
            controls::RxChannel::Rx1,
            controls::UiDensity::Extended,
            surface,
            |ctx| {
                controls::render_mode_selector(ui, ctx).map(|c| {
                    let intent = controls::UiIntent::SelectMode {
                        channel: controls::RxChannel::Rx1,
                        mode: c.mode,
                    };
                    let dispatched = ctx.dispatch(intent, Command::SetMode(c.mode));
                    (c, dispatched)
                })
            },
        );
        // Alleen lokale state muteren als dispatch daadwerkelijk een command stuurde
        // (niet bij Disconnected / SendFailed) — anders drift UI-state vs. server-state.
        if let Some((click, true)) = mode_action {
            self.mode = click.mode;
            self.filter_changed_at = None;
            self.tci_control_changed_at = Some(Instant::now());
        }

        // -- Frequency step buttons (via controls::render_freq_step_controls) --
        let step_action = self.with_rx_ctx(
            controls::RxChannel::Rx1,
            controls::UiDensity::Extended,
            surface,
            |ctx| {
                controls::render_freq_step_controls(ui, ctx).map(|step| {
                    let delta = step.delta_hz(ctx.rx_state.freq_step_index);
                    let new_freq = (ctx.rx_state.frequency_hz as i64 + delta).max(0) as u64;
                    let intent = controls::UiIntent::TuneByDelta {
                        channel: controls::RxChannel::Rx1,
                        delta_hz: delta,
                    };
                    let dispatched = ctx.dispatch(intent, Command::SetFrequency(new_freq));
                    (new_freq, dispatched)
                })
            },
        );
        // Alleen pending_freq updaten als dispatch slaagde — anders UI-drift.
        if let Some((new_freq, true)) = step_action {
            self.set_pending_freq_a(new_freq);
        }

        // -- Filter + NR + ANF --
        {
            let presets = filter_presets_for_mode(self.mode);
            let cw = is_cw_mode(self.mode);
            let is_fm = self.mode == 5;
            let current_bw = self.filter_high_hz - self.filter_low_hz;
            let idx = closest_preset_index(presets, current_bw);

            ui.horizontal(|ui| {
                ui.label("Filter:");
                let minus_btn = egui::Button::new(RichText::new(" - ").size(14.0));
                if ui.add_enabled(idx > 0, minus_btn).clicked() {
                    let (low, high) = calc_filter_edges(
                        self.mode, self.filter_low_hz, self.filter_high_hz, presets[idx - 1]);
                    let _ = self.cmd_tx.send(Command::SetControl(
                        ControlId::FilterLow, low as i16 as u16));
                    let _ = self.cmd_tx.send(Command::SetControl(
                        ControlId::FilterHigh, high as i16 as u16));
                    self.filter_low_hz = low;
                    self.filter_high_hz = high;
                    self.filter_changed_at = Some(Instant::now());
                }

                if is_fm {
                    // FM: show actual bandwidth from Thetis + deviation label
                    let dev_label = if current_bw <= 6000 { "NFM" } else { "WFM" };
                    let bw_text = format!("{} {}", format_bandwidth(current_bw, false), dev_label);
                    ui.label(RichText::new(bw_text).strong().size(14.0));
                } else {
                    ui.label(RichText::new(format_bandwidth(presets[idx], cw)).strong().size(14.0));
                }

                let plus_btn = egui::Button::new(RichText::new(" + ").size(14.0));
                if ui.add_enabled(idx < presets.len() - 1, plus_btn).clicked() {
                    let (low, high) = calc_filter_edges(
                        self.mode, self.filter_low_hz, self.filter_high_hz, presets[idx + 1]);
                    let _ = self.cmd_tx.send(Command::SetControl(
                        ControlId::FilterLow, low as i16 as u16));
                    let _ = self.cmd_tx.send(Command::SetControl(
                        ControlId::FilterHigh, high as i16 as u16));
                    self.filter_low_hz = low;
                    self.filter_high_hz = high;
                    self.filter_changed_at = Some(Instant::now());
                }

                ui.add_space(10.0);

                // NR cycle
                let nr_label = if self.nr_level == 0 { "NR".to_string() } else { format!("NR{}", self.nr_level) };
                let nr_btn = if self.nr_level > 0 {
                    egui::Button::new(RichText::new(&nr_label).strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else {
                    egui::Button::new(&nr_label)
                };
                if ui.add(nr_btn).clicked() {
                    let new_val = if self.nr_level >= 4 { 0 } else { self.nr_level + 1 };
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::NoiseReduction, new_val as u16));
                    self.nr_level = new_val;
                }

                // NB cycle: OFF → NB1 → NB2 (extensions) → OFF
                let nb_label = match self.nb_level { 1 => "NB1".to_string(), 2 => "NB2".to_string(), _ => "NB".to_string() };
                let nb_btn = if self.nb_level > 0 {
                    egui::Button::new(RichText::new(&nb_label).strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else {
                    egui::Button::new(&nb_label)
                };
                if ui.add(nb_btn).clicked() {
                    let max_nb: u8 = if self.ddc_sample_rate_rx1 > 0 { 2 } else { 1 };
                    let new_val = if self.nb_level >= max_nb { 0 } else { self.nb_level + 1 };
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::NoiseBlanker, new_val as u16));
                    self.nb_level = new_val;
                }

                // ANF toggle
                let anf_btn = if self.anf_on {
                    egui::Button::new(RichText::new("ANF").strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else {
                    egui::Button::new("ANF")
                };
                if ui.add(anf_btn).clicked() {
                    let new_val = !self.anf_on;
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::AutoNotchFilter, new_val as u16));
                    self.anf_on = new_val;
                }

                // Mic AGC toggle
                let agc_btn = if self.agc_enabled {
                    egui::Button::new(RichText::new("Mic AGC").strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else {
                    egui::Button::new("Mic AGC")
                };
                if ui.add(agc_btn).clicked() {
                    let new_val = !self.agc_enabled;
                    let _ = self.cmd_tx.send(Command::SetAgcEnabled(new_val));
                    self.agc_enabled = new_val;
                    self.save_full_config();
                }

                // MON (TX Monitor) toggle
                let mon_btn = if self.mon_on {
                    egui::Button::new(RichText::new("MON").strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else {
                    egui::Button::new("MON")
                };
                if ui.add(mon_btn).clicked() {
                    let new_val = !self.mon_on;
                    let _ = self.cmd_tx.send(Command::SetMonitor(new_val));
                    self.mon_on = new_val;
                }
            });
        }
    }

    fn render_rx1_popout_content(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.render_rx1_controls(ui, controls::UiSurface::PopoutSeparate);
        ui.separator();
        // -- Spectrum + waterfall --
        self.render_spectrum_content(ui, ctx, 0.0, true);
    }

    /// Render RX2 controls only (VFO, S-meter, band, mode, freq step, filter, NR, ANF)
    /// If `show_split_button` is true, a Split button is shown right-aligned on the S-meter row.
    fn render_rx2_controls_with_split(&mut self, ui: &mut egui::Ui, show_split_button: bool, is_popout: bool, surface: controls::UiSurface) {
        if is_popout && self.popout_meter_analog {
            let total_w = ui.available_width();
            let start = ui.cursor().min;

            // Measure controls height
            let measure_rect = egui::Rect::from_min_size(start, egui::vec2(total_w, 500.0));
            let mut measure = ui.new_child(egui::UiBuilder::new().max_rect(measure_rect).layout(egui::Layout::top_down(egui::Align::LEFT)));
            self.render_rx2_controls_inner(&mut measure, show_split_button, is_popout, surface);
            let controls_h = measure.min_rect().height();

            let meter_w = (controls_h * 2.0).min(total_w - 480.0).max(0.0);
            let controls_w = total_w - meter_w - if meter_w > 0.0 { 8.0 } else { 0.0 };

            let controls_rect = egui::Rect::from_min_size(start, egui::vec2(controls_w, 500.0));
            let mut left = ui.new_child(egui::UiBuilder::new().max_rect(controls_rect).layout(egui::Layout::top_down(egui::Align::LEFT)));
            self.render_rx2_controls_inner(&mut left, show_split_button, is_popout, surface);

            if meter_w > 80.0 {
                let meter_pos = egui::pos2(start.x + controls_w + 4.0, start.y);
                let meter_rect = egui::Rect::from_min_size(meter_pos, egui::vec2(meter_w, controls_h));
                let mut right = ui.new_child(egui::UiBuilder::new().max_rect(meter_rect).layout(egui::Layout::top_down(egui::Align::LEFT)));
                self.popout_rx2_smeter_rect = smeter_analog_sized(&mut right, self.rx2_smeter, self.rx2_smeter_peak, false, false, Some((meter_w, controls_h)));
            }

            ui.advance_cursor_after_rect(egui::Rect::from_min_size(start, egui::vec2(total_w, controls_h)));
        } else {
            self.render_rx2_controls_inner(ui, show_split_button, is_popout, surface);
        }
    }

    fn render_rx2_controls_inner(&mut self, ui: &mut egui::Ui, show_split_button: bool, is_popout: bool, surface: controls::UiSurface) {
        let amber = Color32::from_rgb(255, 170, 40);

        // -- Top bar: frequency + mode (via render_frequency_display) --
        // PATCH-rx2-inline-edit: RX2 krijgt nu dezelfde inline-edit UX als RX1
        // (klik VFO B label → edit → Enter → dispatch). Scroll-wheel blijft
        // werken (Extended density is niet scroll-gated).
        let _ = is_popout; // parameter blijft voor signature-compat; not-popout is dode code.
        ui.horizontal(|ui| {
            let action = self.with_rx_ctx(
                controls::RxChannel::Rx2,
                controls::UiDensity::Extended,
                surface,
                |ctx| {
                    controls::render_frequency_display(ui, ctx).map(|a| match a {
                        controls::FrequencyDisplayAction::Submit { hz } => {
                            let intent = controls::UiIntent::InlineFreqEdit {
                                channel: controls::RxChannel::Rx2,
                                hz,
                            };
                            let dispatched = ctx.dispatch(intent, Command::SetFrequencyRx2(hz));
                            (hz, dispatched)
                        }
                        controls::FrequencyDisplayAction::ScrollTune { delta_hz } => {
                            let new_freq = (ctx.rx_state.frequency_hz as i64 + delta_hz).max(0) as u64;
                            let intent = controls::UiIntent::TuneByDelta {
                                channel: controls::RxChannel::Rx2,
                                delta_hz,
                            };
                            let dispatched = ctx.dispatch(intent, Command::SetFrequencyRx2(new_freq));
                            (new_freq, dispatched)
                        }
                    })
                },
            );
            if let Some((new_freq, true)) = action {
                self.set_pending_freq_b(new_freq);
            }

            ui.separator();

            let mode_label = match self.rx2_mode {
                0 => "LSB", 1 => "USB", 2 => "DSB", 3 => "CW-L", 4 => "CW-U",
                5 => "FM", 6 => "AM", 7 => "DIGU", 8 => "SPEC", 9 => "DIGL",
                10 => "SAM", 11 => "DRM", _ => "?",
            };
            ui.label(RichText::new(mode_label).size(16.0).color(amber));

            let bw = self.rx2_filter_high_hz - self.rx2_filter_low_hz;
            let bw_text = if bw >= 1000 {
                format!("{:.1}k", bw as f32 / 1000.0)
            } else {
                format!("{} Hz", bw)
            };
            ui.label(RichText::new(bw_text).size(12.0).weak());
        });

        // S-meter bar for RX2 (hidden when analog meter is shown in popout wrapper)
        if !(is_popout && self.popout_meter_analog) {
            if show_split_button {
                ui.horizontal(|ui| {
                    self.popout_rx2_smeter_rect = if is_popout {
                        smeter_bar_popout(ui, self.rx2_smeter, self.rx2_smeter_peak, false, false, 100)
                    } else {
                        smeter_bar(ui, self.rx2_smeter, self.rx2_smeter_peak, false, false, 100)
                    };
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let join_btn = egui::Button::new(RichText::new("Split").strong())
                            .fill(Color32::from_rgb(100, 160, 230));
                        if ui.add(join_btn).clicked() {
                            self.popout_joined = false;
                            self.save_full_config();
                        }
                    });
                });
            } else {
                self.popout_rx2_smeter_rect = if is_popout {
                    smeter_bar_popout(ui, self.rx2_smeter, self.rx2_smeter_peak, false, false, 100)
                } else {
                    smeter_bar(ui, self.rx2_smeter, self.rx2_smeter_peak, false, false, 100)
                };
            }
        }

        // -- Controls row: VFO Sync, Volume --
        ui.horizontal(|ui| {
            let sync_btn = if self.vfo_sync {
                egui::Button::new(RichText::new("VFO Sync").size(12.0).strong())
                    .fill(Color32::from_rgb(100, 160, 230))
            } else {
                egui::Button::new(RichText::new("VFO Sync").size(12.0))
            };
            if ui.add_enabled(self.connected, sync_btn).clicked() {
                self.vfo_sync = !self.vfo_sync;
                let _ = self.cmd_tx.send(Command::SetVfoSync(self.vfo_sync));
            }

            ui.separator();

            ui.label("VFO B:");
            let vol_slider = egui::Slider::new(&mut self.vfo_b_volume, 0.001..=1.0)
                .logarithmic(true)
                .show_value(false)
                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
            if ui.add(vol_slider).changed() {
                let _ = self.cmd_tx.send(Command::SetVfoBVolume(self.vfo_b_volume));
                self.save_full_config();
            }
        });

        // -- Band buttons (via controls::render_band_selector) --
        let band_click = self.with_rx_ctx(
            controls::RxChannel::Rx2,
            controls::UiDensity::Extended,
            surface,
            |ctx| controls::render_band_selector(ui, ctx),
        );
        if let Some(click) = band_click {
            self.handle_band_switch(Vfo::B, click);
        }

        // -- Mode selector (via controls::render_mode_selector) --
        let mode_action = self.with_rx_ctx(
            controls::RxChannel::Rx2,
            controls::UiDensity::Extended,
            surface,
            |ctx| {
                controls::render_mode_selector(ui, ctx).map(|c| {
                    let intent = controls::UiIntent::SelectMode {
                        channel: controls::RxChannel::Rx2,
                        mode: c.mode,
                    };
                    let dispatched = ctx.dispatch(intent, Command::SetModeRx2(c.mode));
                    (c, dispatched)
                })
            },
        );
        if let Some((click, true)) = mode_action {
            self.rx2_mode = click.mode;
        }

        // -- Frequency step buttons (via controls::render_freq_step_controls) --
        // ± knoppen hadden hier voor RX2 popout geen connected-guard
        // (raw `ui.button(...)`).
        let step_action = self.with_rx_ctx(
            controls::RxChannel::Rx2,
            controls::UiDensity::Extended,
            surface,
            |ctx| {
                controls::render_freq_step_controls(ui, ctx).map(|step| {
                    let delta = step.delta_hz(ctx.rx_state.freq_step_index);
                    let new_freq = (ctx.rx_state.frequency_hz as i64 + delta).max(0) as u64;
                    let intent = controls::UiIntent::TuneByDelta {
                        channel: controls::RxChannel::Rx2,
                        delta_hz: delta,
                    };
                    let dispatched = ctx.dispatch(intent, Command::SetFrequencyRx2(new_freq));
                    (new_freq, dispatched)
                })
            },
        );
        if let Some((new_freq, true)) = step_action {
            self.set_pending_freq_b(new_freq);
        }

        // -- Filter bandwidth control --
        {
            let presets = filter_presets_for_mode(self.rx2_mode);
            let cw = is_cw_mode(self.rx2_mode);
            let is_fm = self.rx2_mode == 5;
            // Fallback to RX1 filter if RX2 filter not available (Thetis ZZRL/ZZRH may not respond)
            let (fl, fh) = if self.rx2_filter_low_hz != 0 || self.rx2_filter_high_hz != 0 {
                (self.rx2_filter_low_hz, self.rx2_filter_high_hz)
            } else {
                (self.filter_low_hz, self.filter_high_hz)
            };
            let current_bw = fh - fl;
            let idx = closest_preset_index(presets, current_bw);

            ui.horizontal(|ui| {
                ui.label("Filter:");
                let minus_btn = egui::Button::new(RichText::new(" - ").size(14.0));
                if ui.add_enabled(idx > 0, minus_btn).clicked() {
                    let (low, high) = calc_filter_edges(
                        self.rx2_mode, fl, fh, presets[idx - 1]);
                    let _ = self.cmd_tx.send(Command::SetControl(
                        ControlId::Rx2FilterLow, low as i16 as u16));
                    let _ = self.cmd_tx.send(Command::SetControl(
                        ControlId::Rx2FilterHigh, high as i16 as u16));
                    self.rx2_filter_low_hz = low;
                    self.rx2_filter_high_hz = high;
                    self.rx2_filter_changed_at = Some(Instant::now());
                }

                if is_fm {
                    let dev_label = if current_bw <= 6000 { "NFM" } else { "WFM" };
                    let bw_text = format!("{} {}", format_bandwidth(current_bw, false), dev_label);
                    ui.label(RichText::new(bw_text).strong().size(14.0));
                } else {
                    ui.label(RichText::new(format_bandwidth(presets[idx], cw)).strong().size(14.0));
                }

                let plus_btn = egui::Button::new(RichText::new(" + ").size(14.0));
                if ui.add_enabled(idx < presets.len() - 1, plus_btn).clicked() {
                    let (low, high) = calc_filter_edges(
                        self.rx2_mode, fl, fh, presets[idx + 1]);
                    let _ = self.cmd_tx.send(Command::SetControl(
                        ControlId::Rx2FilterLow, low as i16 as u16));
                    let _ = self.cmd_tx.send(Command::SetControl(
                        ControlId::Rx2FilterHigh, high as i16 as u16));
                    self.rx2_filter_low_hz = low;
                    self.rx2_filter_high_hz = high;
                    self.rx2_filter_changed_at = Some(Instant::now());
                }

                ui.add_space(10.0);

                // NR cycle: OFF -> NR1 -> NR2 -> NR3 -> NR4 -> OFF
                let nr_label = if self.rx2_nr_level == 0 { "NR".to_string() } else { format!("NR{}", self.rx2_nr_level) };
                let nr_btn = if self.rx2_nr_level > 0 {
                    egui::Button::new(RichText::new(&nr_label).strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else {
                    egui::Button::new(&nr_label)
                };
                if ui.add(nr_btn).clicked() {
                    let new_val = if self.rx2_nr_level >= 4 { 0 } else { self.rx2_nr_level + 1 };
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Rx2NoiseReduction, new_val as u16));
                    self.rx2_nr_level = new_val;
                }

                // NB cycle: OFF → NB1 → NB2 (extensions) → OFF
                let rx2_nb_label = match self.rx2_nb_level { 1 => "NB1".to_string(), 2 => "NB2".to_string(), _ => "NB".to_string() };
                let nb_btn = if self.rx2_nb_level > 0 {
                    egui::Button::new(RichText::new(&rx2_nb_label).strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else {
                    egui::Button::new(&rx2_nb_label)
                };
                if ui.add(nb_btn).clicked() {
                    let max_nb: u8 = if self.ddc_sample_rate_rx1 > 0 { 2 } else { 1 };
                    let new_val = if self.rx2_nb_level >= max_nb { 0 } else { self.rx2_nb_level + 1 };
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Rx2NoiseBlanker, new_val as u16));
                    self.rx2_nb_level = new_val;
                }

                // ANF toggle
                let anf_btn = if self.rx2_anf_on {
                    egui::Button::new(RichText::new("ANF").strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else {
                    egui::Button::new("ANF")
                };
                if ui.add(anf_btn).clicked() {
                    let new_val = !self.rx2_anf_on;
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Rx2AutoNotchFilter, new_val as u16));
                    self.rx2_anf_on = new_val;
                }
            });
        }
    }

    /// Read RX1 spectrum interaction temp keys and send frequency commands.
    /// Must be called inside the same viewport that rendered the spectrum.
    fn handle_rx1_spectrum_keys(&mut self, ctx: &egui::Context) {
        for key in ["spectrum_scroll_freq", "spectrum_click_freq", "spectrum_drag_freq"] {
            let freq: Option<u64> = ctx.memory(|mem| {
                mem.data.get_temp(egui::Id::new(key))
            });
            if let Some(freq) = freq {
                let _ = self.cmd_tx.send(Command::SetFrequency(freq));
                self.set_pending_freq_a(freq);
                ctx.memory_mut(|mem| {
                    mem.data.remove::<u64>(egui::Id::new(key));
                });
            }
        }
        // Filter edge drag — always send both low+high (server expects pair)
        {
            use sdr_remote_core::protocol::ControlId;
            let drag_lo: Option<i32> = ctx.memory(|mem| mem.data.get_temp(egui::Id::new("spectrum_filter_low")));
            let drag_hi: Option<i32> = ctx.memory(|mem| mem.data.get_temp(egui::Id::new("spectrum_filter_high")));
            if let Some(hz) = drag_lo {
                self.filter_low_hz = hz;
                let _ = self.cmd_tx.send(Command::SetControl(ControlId::FilterLow, hz as i16 as u16));
                let _ = self.cmd_tx.send(Command::SetControl(ControlId::FilterHigh, self.filter_high_hz as i16 as u16));
                self.filter_changed_at = Some(std::time::Instant::now());
                ctx.memory_mut(|mem| { mem.data.remove::<i32>(egui::Id::new("spectrum_filter_low")); });
            }
            if let Some(hz) = drag_hi {
                self.filter_high_hz = hz;
                let _ = self.cmd_tx.send(Command::SetControl(ControlId::FilterLow, self.filter_low_hz as i16 as u16));
                let _ = self.cmd_tx.send(Command::SetControl(ControlId::FilterHigh, hz as i16 as u16));
                self.filter_changed_at = Some(std::time::Instant::now());
                ctx.memory_mut(|mem| { mem.data.remove::<i32>(egui::Id::new("spectrum_filter_high")); });
            }
        }
    }

    /// Read RX2 spectrum interaction temp keys and send frequency commands.
    /// Must be called inside the same viewport that rendered the spectrum.
    fn handle_rx2_spectrum_keys(&mut self, ctx: &egui::Context) {
        for key in ["rx2_spectrum_scroll_freq", "rx2_spectrum_click_freq", "rx2_spectrum_drag_freq"] {
            let freq: Option<u64> = ctx.memory(|mem| {
                mem.data.get_temp(egui::Id::new(key))
            });
            if let Some(freq) = freq {
                let _ = self.cmd_tx.send(Command::SetFrequencyRx2(freq));
                self.set_pending_freq_b(freq);
                ctx.memory_mut(|mem| {
                    mem.data.remove::<u64>(egui::Id::new(key));
                });
            }
        }
    }

    fn render_rx2_content(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.render_rx2_controls_with_split(ui, false, true, controls::UiSurface::PopoutSeparate);
        ui.separator();
        self.render_rx2_spectrum_only(ui, ctx);
    }

    /// Render RX2 spectrum + waterfall only (no controls)
    fn render_rx2_spectrum_only(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {

        if !self.rx2_spectrum_bins.is_empty() {
            // Row 1: Ref + Auto checkbox + Range (same as RX1)
            ui.horizontal(|ui| {
                ui.spacing_mut().slider_width = 80.0;
                ui.label("Ref:");
                if self.rx2_auto_ref_enabled {
                    let mut display_val = self.rx2_spectrum_ref_db;
                    ui.add_enabled(false, egui::Slider::new(&mut display_val, -90.0..=0.0)
                        .suffix(" dB")
                        .step_by(5.0)
                    );
                } else if ui.add(egui::Slider::new(&mut self.rx2_spectrum_ref_db, -90.0..=0.0)
                    .suffix(" dB")
                    .step_by(5.0)
                ).changed() {
                    self.save_full_config();
                }
                if ui.checkbox(&mut self.rx2_auto_ref_enabled, "Auto").changed() {
                    if self.rx2_auto_ref_enabled {
                        self.rx2_auto_ref_frames = 0;
                        self.rx2_auto_ref_initialized = false;
                    }
                    self.save_full_config();
                }
                ui.label("Range:");
                if ui.add(egui::Slider::new(&mut self.rx2_spectrum_range_db, 20.0..=130.0)
                    .suffix(" dB")
                    .step_by(5.0)
                ).changed() {
                    if self.rx2_auto_ref_enabled {
                        self.rx2_auto_ref_frames = 0;
                        self.rx2_auto_ref_initialized = false;
                    }
                    self.save_full_config();
                }
            });
            // Row 2: Zoom/Pan controls
            ui.horizontal(|ui| {
                ui.spacing_mut().slider_width = 80.0;
                ui.label("Zoom:");
                let zoom_changed = ui.add(egui::Slider::new(&mut self.rx2_spectrum_zoom, 1.0..=1024.0)
                    .logarithmic(true)
                    .custom_formatter(|v, _| format!("{:.0}x", v))
                ).changed();
                if zoom_changed {
                    let max_pan = (0.5 - 0.5 / self.rx2_spectrum_zoom) * 0.05;
                    self.rx2_spectrum_pan = self.rx2_spectrum_pan.clamp(-max_pan, max_pan);
                }
                ui.label("Pan:");
                let max_pan = if self.rx2_spectrum_zoom > 1.01 { (0.5 - 0.5 / self.rx2_spectrum_zoom) * 0.05 } else { 0.0 };
                let pan_changed = ui.add(egui::Slider::new(&mut self.rx2_spectrum_pan, -max_pan..=max_pan)
                    .custom_formatter(|v, _| format!("{:+.2}", v))
                ).changed();
                ui.label("WF:");
                if ui.add(egui::Slider::new(&mut self.rx2_waterfall_contrast, 0.3..=3.0)
                    .logarithmic(true)
                    .custom_formatter(|v, _| format!("{:.1}", v))
                ).changed() {
                    self.save_full_config();
                }

                // RX2 FFT size selector
                let rx2_ddc_rate = if self.ddc_sample_rate_rx2 > 0 { self.ddc_sample_rate_rx2 as u32 * 1000 } else { 384_000 };
                let rx2_auto_fft = sdr_remote_core::ddc_fft_size(rx2_ddc_rate);
                let rx2_auto_k = rx2_auto_fft / 1024;
                let rx2_fft_label = if self.rx2_spectrum_fft_size_k == 0 {
                    format!("FFT: Auto ({}K)", rx2_auto_k)
                } else {
                    format!("FFT: {}K", self.rx2_spectrum_fft_size_k)
                };
                let rx2_hop = |fft_k: u32| -> u32 { let fft = fft_k * 1024; rx2_ddc_rate / (fft / 8) };
                let rx2_fft_options: Vec<(u16, String)> = {
                    let mut opts = vec![(0u16, format!("Auto ({}K, ~{} FFT/s)", rx2_auto_k, rx2_hop(rx2_auto_k as u32)))];
                    for &k in &[32u16, 64, 128, 256, 512, 1024] {
                        let fft = k as u32 * 1024;
                        if fft <= rx2_ddc_rate * 4 {
                            let fps = rx2_hop(k as u32);
                            if fps > 0 { opts.push((k, format!("{}K (~{} FFT/s)", k, fps))); }
                        }
                    }
                    opts
                };
                egui::ComboBox::from_id_salt("rx2_fft_size")
                    .selected_text(&rx2_fft_label)
                    .width(80.0)
                    .show_ui(ui, |ui| {
                        for (k, label) in &rx2_fft_options {
                            if ui.selectable_label(self.rx2_spectrum_fft_size_k == *k, label).clicked() {
                                self.rx2_spectrum_fft_size_k = *k;
                                let _ = self.cmd_tx.send(Command::SetControl(ControlId::Rx2SpectrumFftSize, *k));
                                self.save_full_config();
                            }
                        }
                    });

                if zoom_changed || pan_changed {
                    self.rx2_zoom_pan_changed_at = Some(Instant::now());
                }
            });

            // Debounce: send zoom/pan to server after 100ms
            if let Some(changed_at) = self.rx2_zoom_pan_changed_at {
                if changed_at.elapsed().as_millis() >= 100 {
                    let zoom_diff = (self.rx2_spectrum_zoom - self.rx2_last_sent_zoom).abs();
                    let pan_diff = (self.rx2_spectrum_pan - self.rx2_last_sent_pan).abs();
                    if zoom_diff > 0.01 {
                        let _ = self.cmd_tx.send(Command::SetRx2SpectrumZoom(self.rx2_spectrum_zoom));
                        self.rx2_last_sent_zoom = self.rx2_spectrum_zoom;
                    }
                    if pan_diff > 0.001 {
                        let _ = self.cmd_tx.send(Command::SetRx2SpectrumPan(self.rx2_spectrum_pan));
                        self.rx2_last_sent_pan = self.rx2_spectrum_pan;
                    }
                    self.rx2_zoom_pan_changed_at = None;
                }
            }

            // Dynamic spectrum + waterfall layout
            let available = ui.available_height();
            let spec_area = available.max(200.0);
            let spec_h = (spec_area * 0.45).max(100.0);
            let wf_h = (spec_area * 0.55).max(80.0);

            // Smooth RX2 display center (same algorithm as RX1)
            let rx2_target_center = Self::spectrum_target_center_hz(
                self.rx2_frequency_hz,
                self.rx2_full_spectrum_span_hz,
                self.rx2_spectrum_pan,
                self.rx2_spectrum_center_hz,
            );
            let rx2_tuning_active = Self::tuning_latch_active(
                self.rx2_force_full_tuning,
                self.rx2_pending_freq,
                self.rx2_pending_freq_at,
            );
            // Use same alpha as RX1 (computed earlier in this frame)
            let alpha_rx2 = self.smooth_alpha;
            if self.rx2_pending_freq.is_some() {
                self.rx2_smooth_display_center_hz = rx2_target_center.round();
            } else if self.rx2_smooth_display_center_hz == 0.0 {
                self.rx2_smooth_display_center_hz = rx2_target_center;
            } else {
                self.rx2_smooth_display_center_hz += (rx2_target_center - self.rx2_smooth_display_center_hz) * alpha_rx2;
            }
            if (self.rx2_smooth_display_center_hz - rx2_target_center).abs() < 1.0 {
                self.rx2_smooth_display_center_hz = rx2_target_center;
            }
            let rx2_smooth_center = self.rx2_smooth_display_center_hz as u64;
            let rx2_smooth_vfo = (self.rx2_smooth_display_center_hz
                - self.rx2_spectrum_pan as f64 * self.rx2_full_spectrum_span_hz as f64) as u64;
            let (rx2_plot_bins, rx2_plot_center_hz, rx2_plot_span_hz) = if !rx2_tuning_active {
                (&self.rx2_spectrum_bins, self.rx2_spectrum_center_hz, self.rx2_spectrum_span_hz)
            } else {
                (&self.rx2_full_spectrum_bins, self.rx2_full_spectrum_center_hz, self.rx2_full_spectrum_span_hz)
            };
            spectrum_plot(
                ui,
                rx2_plot_bins,
                rx2_plot_center_hz,
                rx2_plot_span_hz,
                rx2_smooth_center,
                rx2_smooth_vfo,
                self.rx2_frequency_hz,
                self.rx2_spectrum_ref_db,
                self.rx2_spectrum_range_db,
                self.rx2_smeter,
                false,
                false,
                // Fallback to RX1 filter values if RX2 filter not available (ZZRL/ZZRH unsupported)
                if self.rx2_filter_low_hz != 0 || self.rx2_filter_high_hz != 0 {
                    self.rx2_filter_low_hz
                } else {
                    self.filter_low_hz
                },
                if self.rx2_filter_low_hz != 0 || self.rx2_filter_high_hz != 0 {
                    self.rx2_filter_high_hz
                } else {
                    self.filter_high_hz
                },
                0, // RX2 has no RIT
                false,
                spec_h,
                &RX2_PLOT_CONFIG,
                &self.dx_spots,
            );
            render_waterfall(
                ui,
                ctx,
                &mut self.rx2_waterfall,
                self.rx2_full_spectrum_span_hz,
                rx2_smooth_center,
                self.rx2_frequency_hz,
                self.rx2_spectrum_zoom,
                self.rx2_waterfall_contrast,
                self.rx2_spectrum_ref_db,
                self.rx2_spectrum_range_db,
                wf_h,
                &RX2_PLOT_CONFIG,
            );
        } else {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("Waiting for RX2 spectrum data...").weak());
            });
        }
    }
}

impl eframe::App for SdrRemoteApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Bump UI frame id — stempelt alle UiEvents met monotone frame-id voor
        // timeline-correlatie.
        controls::begin_frame();

        // Clear per-frame flags
        ctx.memory_mut(|mem| mem.data.remove::<bool>(egui::Id::new("freq_scroll_consumed")));

        // Light grey background, lighter widget fills for contrast
        let mut visuals = ctx.style().visuals.clone();
        let light_grey = Color32::from_rgb(230, 230, 230);
        visuals.panel_fill = light_grey;
        visuals.window_fill = light_grey;
        visuals.widgets.inactive.bg_fill = Color32::from_rgb(210, 210, 215);
        visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(210, 210, 215);
        visuals.widgets.hovered.bg_fill = Color32::from_rgb(195, 195, 200);
        visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(195, 195, 200);
        visuals.widgets.active.bg_fill = Color32::from_rgb(180, 180, 190);
        visuals.widgets.active.weak_bg_fill = Color32::from_rgb(180, 180, 190);
        ctx.set_visuals(visuals);

        self.sync_state();
        self.process_midi_events();

        // Sync frequency to embedded WebSDR (debounced)
        self.catsync.sync_freq(self.frequency_hz, self.mode);

        // (TX spectrum override sets ref/range directly in PTT handler)

        // Track window size for persistence
        if let Some(rect) = ctx.input(|i| i.viewport().inner_rect) {
            let w = rect.width();
            let h = rect.height();
            if (w - self.window_w).abs() > 5.0 || (h - self.window_h).abs() > 5.0 {
                self.window_w = w;
                self.window_h = h;
                self.save_full_config();
            }
        }

        // Volume routing based on popout state:
        // - Both popout: master slider in main, VFO A/B sliders in popouts
        // - Only VFO A popout: master=100%, VFO B=0%, main slider=VFO A
        // - Only VFO B popout: master=100%, main slider=VFO A
        // - No popout: master=100%, main slider=VFO A
        let both_popout = self.spectrum_popout && self.rx2_popout;
        if !both_popout {
            // Force master to 100% when not in dual-popout mode
            if self.local_volume < 1.0 {
                self.local_volume = 1.0;
                let _ = self.cmd_tx.send(Command::SetLocalVolume(1.0));
            }
            // VFO A popout without VFO B → mute VFO B
            if self.spectrum_popout && !self.rx2_popout && self.vfo_b_volume > 0.001 {
                self.vfo_b_volume = 0.001;
                let _ = self.cmd_tx.send(Command::SetVfoBVolume(0.001));
            }
        }

        let rx1_tuning_active = Self::tuning_latch_active(
            self.rx1_force_full_tuning,
            self.pending_freq,
            self.pending_freq_at,
        );

        // Push new waterfall data (always, before rendering)
        self.waterfall.push(
            &self.full_spectrum_bins, self.full_spectrum_center_hz,
            self.full_spectrum_span_hz, self.full_spectrum_sequence,
            if !rx1_tuning_active { &self.spectrum_bins } else { &[] },
            if !rx1_tuning_active { self.spectrum_center_hz } else { 0 },
            if !rx1_tuning_active { self.spectrum_span_hz } else { 0 },
        );

        let rx2_tuning_active = Self::tuning_latch_active(
            self.rx2_force_full_tuning,
            self.rx2_pending_freq,
            self.rx2_pending_freq_at,
        );

        // Push RX2 waterfall data
        self.rx2_waterfall.push(
            &self.rx2_full_spectrum_bins, self.rx2_full_spectrum_center_hz,
            self.rx2_full_spectrum_span_hz, self.rx2_full_spectrum_sequence,
            if !rx2_tuning_active { &self.rx2_spectrum_bins } else { &[] },
            if !rx2_tuning_active { self.rx2_spectrum_center_hz } else { 0 },
            if !rx2_tuning_active { self.rx2_spectrum_span_hz } else { 0 },
        );

        // Sticky top panel: PTT button + local volume (always visible)
        egui::TopBottomPanel::top("ptt_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                // PTT button (compact for top bar)
                let (ptt_color, ptt_text, ptt_locked) = if self.other_tx {
                    (Color32::from_rgb(200, 120, 0), "TX in use", true)
                } else if self.ptt {
                    (Color32::RED, "TX", false)
                } else {
                    (Color32::from_rgb(60, 60, 60), "PTT", false)
                };

                let button = egui::Button::new(
                    RichText::new(ptt_text).size(20.0).color(Color32::WHITE),
                )
                .fill(ptt_color)
                .min_size(Vec2::new(80.0, 36.0));

                let response = ui.push_id("ptt_button", |ui| {
                    ui.add_enabled(!ptt_locked, button)
                }).inner;

                // PTT button: toggle or momentary (push-to-talk) mode
                if self.ptt_toggle_mode {
                    // Toggle: click to switch on/off
                    if response.clicked() {
                        self.mouse_ptt = !self.mouse_ptt;
                    }
                } else {
                    // Momentary: hold to TX, release to RX
                    let pointer_on_btn = ui.input(|i| {
                        i.pointer.primary_down()
                            && response.rect.contains(i.pointer.interact_pos().unwrap_or(Pos2::ZERO))
                    });
                    self.mouse_ptt = pointer_on_btn;
                }

                let space_held = ui.input(|i| i.key_down(egui::Key::Space));
                let new_ptt = self.mouse_ptt || space_held || self.midi_ptt;
                if new_ptt != self.ptt {
                    self.midi.send_led(crate::midi::MidiAction::Ptt, new_ptt);
                    self.catsync.update_mute(new_ptt);
                    // TX spectrum override
                    if new_ptt {
                        // Entering TX: save ref, range, auto — then set TX defaults
                        self.tx_spectrum_saved_ref_db = Some(self.spectrum_ref_db);
                        self.tx_spectrum_saved_range = Some(self.spectrum_range_db);
                        self.tx_spectrum_saved_auto_ref = Some(self.auto_ref_enabled);
                        self.tx_spectrum_restore_auto_at = None;
                        self.auto_ref_enabled = false;
                        self.spectrum_ref_db = -30.0;
                        self.spectrum_range_db = 120.0;
                    } else {
                        // Leaving TX: restore ref+range immediately, auto_ref after 200ms
                        if let Some(saved) = self.tx_spectrum_saved_ref_db.take() {
                            self.spectrum_ref_db = saved;
                        }
                        if let Some(saved) = self.tx_spectrum_saved_range.take() {
                            self.spectrum_range_db = saved;
                        }
                        if self.tx_spectrum_saved_auto_ref.is_some() {
                            self.tx_spectrum_restore_auto_at = Some(std::time::Instant::now() + std::time::Duration::from_millis(200));
                        }
                    }
                }
                // Auto-switch TX profile for current mic before PTT on
                if new_ptt {
                    let mic = if self.selected_input.is_empty() { "(Default)" } else { &self.selected_input };
                    if let Some(profile_name) = self.mic_profile_map.get(mic).or_else(|| self.mic_profile_map.get("(Default)")) {
                        if let Some((idx, _)) = self.tx_profiles.iter().find(|(_, n)| n == profile_name) {
                            if *idx != self.tx_profile {
                                let _ = self.cmd_tx.send(Command::SetControl(sdr_remote_core::protocol::ControlId::TxProfile, *idx as u16));
                                self.tx_profile = *idx;
                            }
                        }
                    }
                }
                let _ = self.cmd_tx.send(Command::SetPtt(new_ptt));
                self.ptt = new_ptt;

                // Tune button (visible when tuner available on JC-4s antenna)
                if self.tuner_can_tune && self.tuner_connected {
                    let freq_delta = if self.tuner_tune_freq > 0 && self.frequency_hz > 0 {
                        (self.frequency_hz as i64 - self.tuner_tune_freq as i64).unsigned_abs()
                    } else {
                        u64::MAX // Never tuned = always stale
                    };
                    let stale = freq_delta > 25_000; // >25kHz = needs retune

                    let olive_green = Color32::from_rgb(120, 160, 40);
                    let (tune_color, tune_text) = match self.tuner_state {
                        1 => (Color32::from_rgb(60, 120, 220), "Tune..."),  // Tuning = blue
                        2 if !stale => (Color32::from_rgb(50, 180, 50), "Tune OK"),  // Done OK + in range = green
                        5 if !stale => (olive_green, "Tune ~"),  // Done assumed + in range = olive green
                        3 | 4 => (Color32::from_rgb(220, 160, 40), "Tune X"),  // Timeout/Aborted = orange
                        _ => (Color32::from_rgb(80, 80, 80), "Tune"),  // Idle or stale = grey
                    };

                    let tune_btn = egui::Button::new(
                        RichText::new(tune_text).size(16.0).color(Color32::WHITE),
                    )
                    .fill(tune_color)
                    .min_size(Vec2::new(70.0, 36.0));

                    if ui.add(tune_btn).clicked() {
                        if self.tuner_state == 1 {
                            let _ = self.cmd_tx.send(Command::TunerAbort);
                        } else {
                            let _ = self.cmd_tx.send(Command::TunerTune);
                        }
                    }
                }

                // SPE Expert compact status (only when active PA)
                if self.spe_connected && self.spe_active {
                    ui.separator();
                    // Operate/Standby toggle button with status text
                    let (btn_text, btn_color) = match self.spe_state {
                        2 => ("SPE OPR", Color32::from_rgb(0, 150, 0)),
                        1 => ("SPE STBY", Color32::from_rgb(255, 170, 40)),
                        _ => ("SPE OFF", Color32::GRAY),
                    };
                    let spe_btn = egui::Button::new(RichText::new(btn_text).size(11.0).strong().color(Color32::WHITE))
                        .fill(btn_color)
                        .min_size(Vec2::new(70.0, 20.0));
                    if ui.add(spe_btn).clicked() {
                        let _ = self.cmd_tx.send(Command::SpeOperate);
                    }

                    if self.spe_ptt {
                        ui.label(RichText::new(format!("{}W", self.spe_power_w)).size(12.0));
                        let swr = self.spe_swr_x10 as f32 / 10.0;
                        let swr_color = if swr > 3.0 { Color32::from_rgb(255, 80, 80) }
                            else if swr > 2.0 { Color32::from_rgb(255, 170, 40) }
                            else { ui.visuals().text_color() };
                        ui.colored_label(swr_color, RichText::new(format!("{:.1}", swr)).size(12.0));
                    }
                    ui.label(RichText::new(format!("{}°C", self.spe_temp)).size(11.0).weak());

                    // Warning/alarm indicator
                    if self.spe_alarm != b'N' && self.spe_alarm != 0 {
                        ui.colored_label(Color32::from_rgb(255, 80, 80), RichText::new("ALM").size(11.0).strong());
                    } else if self.spe_warning != b'N' && self.spe_warning != 0 {
                        ui.colored_label(Color32::from_rgb(255, 170, 40), RichText::new("WRN").size(11.0).strong());
                    }
                }

                // RF2K-S compact status (only when active PA)
                if self.rf2k_connected && self.rf2k_active {
                    ui.separator();
                    if self.rf2k_error_state != 0 {
                        // Error: red reset button + error text
                        let err = if self.rf2k_error_text.is_empty() {
                            format!("ERR {}", self.rf2k_error_state)
                        } else {
                            self.rf2k_error_text.clone()
                        };
                        let reset_btn = egui::Button::new(RichText::new("RF2K-S Reset").size(11.0).strong().color(Color32::WHITE))
                            .fill(Color32::from_rgb(200, 40, 40))
                            .min_size(Vec2::new(80.0, 20.0));
                        if ui.add(reset_btn).clicked() {
                            let _ = self.cmd_tx.send(Command::Rf2kErrorReset);
                        }
                        ui.colored_label(Color32::from_rgb(255, 80, 80), RichText::new(err).size(11.0).strong());
                    } else {
                        // Normal: Operate/Standby toggle + telemetry
                        let (btn_text, btn_color) = if self.rf2k_operate {
                            ("RF2K-S OPR", Color32::from_rgb(0, 150, 0))
                        } else {
                            ("RF2K-S STBY", Color32::from_rgb(255, 170, 40))
                        };
                        let rf2k_btn = egui::Button::new(RichText::new(btn_text).size(11.0).strong().color(Color32::WHITE))
                            .fill(btn_color)
                            .min_size(Vec2::new(80.0, 20.0));
                        if ui.add(rf2k_btn).clicked() {
                            let _ = self.cmd_tx.send(Command::Rf2kOperate(!self.rf2k_operate));
                        }

                        if self.rf2k_forward_w > 0 {
                            ui.label(RichText::new(format!("{}W", self.rf2k_forward_w)).size(12.0));
                            let swr = self.rf2k_swr_x100 as f32 / 100.0;
                            if swr > 1.0 {
                                let swr_color = if swr > 3.0 { Color32::from_rgb(255, 80, 80) }
                                    else if swr > 2.0 { Color32::from_rgb(255, 170, 40) }
                                    else { ui.visuals().text_color() };
                                ui.colored_label(swr_color, RichText::new(format!("{:.1}", swr)).size(12.0));
                            }
                        }
                        let temp = self.rf2k_temperature_x10 as f32 / 10.0;
                        ui.label(RichText::new(format!("{:.0}°C", temp)).size(11.0).weak());
                    }
                }

                if self.ptt_denied {
                    ui.colored_label(Color32::from_rgb(255, 165, 0), "PTT blocked");
                }
            });
        });

        // Volume + RX2 + Connect row (between PTT bar and tabs)
        egui::TopBottomPanel::top("vol_rx2_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                // Volume slider: role depends on popout state
                let both_popout = self.spectrum_popout && self.rx2_popout;
                if both_popout {
                    ui.label("Master:");
                    let slider = egui::Slider::new(&mut self.local_volume, 0.001..=1.0)
                        .logarithmic(true)
                        .show_value(false)
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
                    if ui.add(slider).changed() {
                        let _ = self.cmd_tx.send(Command::SetLocalVolume(self.local_volume));
                        self.save_full_config();
                    }
                } else {
                    ui.label("VFO A:");
                    let slider = egui::Slider::new(&mut self.vfo_a_volume, 0.001..=1.0)
                        .logarithmic(true)
                        .show_value(false)
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
                    if ui.add(slider).changed() {
                        let _ = self.cmd_tx.send(Command::SetVfoAVolume(self.vfo_a_volume));
                        self.save_full_config();
                    }
                }

                ui.separator();

                // RX2 toggle
                let rx2_btn = if self.rx2_enabled {
                    egui::Button::new(RichText::new("RX2").size(12.0).strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else {
                    egui::Button::new(RichText::new("RX2").size(12.0))
                };
                if ui.add(rx2_btn).clicked() {
                    self.rx2_enabled = !self.rx2_enabled;
                    let _ = self.cmd_tx.send(Command::SetRx2Enabled(self.rx2_enabled));
                    if self.rx2_enabled {
                        let _ = self.cmd_tx.send(Command::EnableRx2Spectrum(true));
                        self.rx2_last_sent_zoom = 0.0;
                        self.rx2_last_sent_pan = 0.0;
                        self.rx2_zoom_pan_changed_at = Some(Instant::now());
                        if self.spectrum_popout {
                            self.rx2_popout = true;
                        }
                    } else {
                        let _ = self.cmd_tx.send(Command::EnableRx2Spectrum(false));
                        self.rx2_popout = false;
                    }
                    self.save_full_config();
                }

                ui.separator();

                // Connect/Disconnect button + status
                if self.connected {
                    if ui.button("Disconnect").clicked() {
                        let _ = self.cmd_tx.send(Command::Disconnect);
                        self.connected = false;
                        self.catsync.force_unmute();
                    }
                    ui.colored_label(Color32::GREEN, "Connected");
                } else {
                    let can_connect = !self.password_input.is_empty();
                    if ui.add_enabled(can_connect, egui::Button::new("Connect")).clicked() {
                        // Reset span to 0 so first spectrum packet triggers zoom calculation
                        self.full_spectrum_span_hz = 0;
                        self.spectrum_pan = 0.0;
                        self.last_sent_zoom = 0.0;
                        self.last_sent_pan = 0.0;
                        self.zoom_pan_changed_at = Some(Instant::now());
                        self.rx2_full_spectrum_span_hz = 0;
                        self.rx2_spectrum_pan = 0.0;
                        self.rx2_last_sent_zoom = 0.0;
                        self.rx2_last_sent_pan = 0.0;
                        self.rx2_zoom_pan_changed_at = Some(Instant::now());
                        let pw = if self.password_input.is_empty() { None } else { Some(self.password_input.clone()) };
                        let _ = self.cmd_tx.send(Command::Connect(self.server_input.clone(), pw));
                        self.save_full_config();
                    }
                    ui.colored_label(Color32::RED, "Disconnected");
                }
                if self.audio_error {
                    ui.colored_label(Color32::from_rgb(255, 165, 0), "Audio error");
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.active_tab, Tab::Radio, "Radio");
                ui.selectable_value(&mut self.active_tab, Tab::Thetis, "Thetis");
                ui.selectable_value(&mut self.active_tab, Tab::Server, "Server");
                ui.selectable_value(&mut self.active_tab, Tab::Devices, "Devices");
                ui.selectable_value(&mut self.active_tab, Tab::Midi, "MIDI");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button(RichText::new("About").size(11.0)).clicked() {
                        self.show_about = !self.show_about;
                    }
                    ui.toggle_value(&mut self.show_log, RichText::new("Log").size(11.0));
                });
            });
            ui.separator();

            if self.active_tab == Tab::Devices {
                self.render_devices_screen(ui);
            } else if self.active_tab == Tab::Thetis {
                self.render_thetis_screen(ui);
            } else if self.active_tab == Tab::Server {
                self.render_server_screen(ui);
            } else if self.active_tab == Tab::Midi {
                self.render_midi_screen(ui);
            } else {

            // VFO A frequency
            ui.separator();
            {
                // -- Inline freq display + edit + scroll (via render_frequency_display) --
                // Combineert scroll-tuning + inline-edit in één helper.
                let freq_action = self.with_rx_ctx(
                    controls::RxChannel::Rx1,
                    controls::UiDensity::Basic,
                    controls::UiSurface::MainTab,
                    |ctx| {
                        controls::render_frequency_display(ui, ctx).map(|a| match a {
                            controls::FrequencyDisplayAction::Submit { hz } => {
                                let intent = controls::UiIntent::InlineFreqEdit {
                                    channel: controls::RxChannel::Rx1,
                                    hz,
                                };
                                let dispatched = ctx.dispatch(intent, Command::SetFrequency(hz));
                                (hz, dispatched)
                            }
                            controls::FrequencyDisplayAction::ScrollTune { delta_hz } => {
                                let new_freq = (ctx.rx_state.frequency_hz as i64 + delta_hz).max(0) as u64;
                                let intent = controls::UiIntent::TuneByDelta {
                                    channel: controls::RxChannel::Rx1,
                                    delta_hz,
                                };
                                let dispatched = ctx.dispatch(intent, Command::SetFrequency(new_freq));
                                (new_freq, dispatched)
                            }
                        })
                    },
                );
                if let Some((new_freq, true)) = freq_action {
                    self.set_pending_freq_a(new_freq);
                }

                // S-meter / TX power / other TX
                smeter_bar(ui, self.smeter, self.smeter_peak, self.ptt, self.other_tx, self.thetis_swr_x100);

                // -- Frequency step buttons (via controls::render_freq_step_controls) --
                // ± knoppen hadden hier in Tab::Radio geen connected-guard
                // (raw `ui.button(...)`).
                let step_action = self.with_rx_ctx(
                    controls::RxChannel::Rx1,
                    controls::UiDensity::Basic,
                    controls::UiSurface::MainTab,
                    |ctx| {
                        controls::render_freq_step_controls(ui, ctx).map(|step| {
                            let delta = step.delta_hz(ctx.rx_state.freq_step_index);
                            let new_freq = (ctx.rx_state.frequency_hz as i64 + delta).max(0) as u64;
                            let intent = controls::UiIntent::TuneByDelta {
                                channel: controls::RxChannel::Rx1,
                                delta_hz: delta,
                            };
                            let dispatched = ctx.dispatch(intent, Command::SetFrequency(new_freq));
                            (new_freq, dispatched)
                        })
                    },
                );
                if let Some((new_freq, true)) = step_action {
                    self.set_pending_freq_a(new_freq);
                }

                // Scroll-wheel tuning zit in render_frequency_display hierboven;
                // de Basic-density helper gate't zichzelf op !spectrum_enabled.
            }

            // Spectrum toggle + display
            {
                ui.horizontal(|ui| {
                    let spectrum_btn = if self.spectrum_enabled {
                        egui::Button::new(RichText::new("Spectrum").strong())
                            .fill(Color32::from_rgb(100, 160, 230))
                    } else {
                        egui::Button::new("Spectrum")
                    };
                    if ui.add(spectrum_btn).clicked() {
                        self.spectrum_enabled = !self.spectrum_enabled;
                        let _ = self.cmd_tx.send(Command::EnableSpectrum(self.spectrum_enabled));
                        self.save_full_config();
                    }

                    if self.spectrum_enabled {
                        let popout_label = if self.spectrum_popout { "Pop-in" } else { "Pop-out" };
                        if ui.button(popout_label).clicked() {
                            self.spectrum_popout = !self.spectrum_popout;
                            // Also open/close RX2 popout when RX2 is enabled
                            if self.rx2_enabled {
                                self.rx2_popout = self.spectrum_popout;
                                if self.rx2_popout {
                                    let _ = self.cmd_tx.send(Command::EnableRx2Spectrum(true));
                                }
                            }
                        }
                    }
                });

                if self.spectrum_enabled && !self.spectrum_bins.is_empty() && !self.spectrum_popout {
                    self.render_spectrum_content(ui, ctx, 300.0, false);
                }
            }

            // Mode buttons (via controls::render_mode_selector — Basic density = 4 modes)
            // Tab::Radio mode-block had voorheen `ui.add(btn)` zonder
            // connected-guard.
            let mode_action = self.with_rx_ctx(
                controls::RxChannel::Rx1,
                controls::UiDensity::Basic,
                controls::UiSurface::MainTab,
                |ctx| {
                    controls::render_mode_selector(ui, ctx).map(|c| {
                        let intent = controls::UiIntent::SelectMode {
                            channel: controls::RxChannel::Rx1,
                            mode: c.mode,
                        };
                        let dispatched = ctx.dispatch(intent, Command::SetMode(c.mode));
                        (c, dispatched)
                    })
                },
            );
            if let Some((click, true)) = mode_action {
                self.mode = click.mode;
                self.filter_changed_at = None;
                self.tci_control_changed_at = Some(Instant::now());
            }

            // Filter bandwidth control
            {
                let presets = filter_presets_for_mode(self.mode);
                let cw = is_cw_mode(self.mode);
                let current_bw = self.filter_high_hz - self.filter_low_hz;
                let idx = closest_preset_index(presets, current_bw);

                ui.horizontal(|ui| {
                    ui.label("Filter:");
                    let minus_btn = egui::Button::new(RichText::new(" - ").size(14.0));
                    if ui.add_enabled(idx > 0, minus_btn).clicked() {
                        let (low, high) = calc_filter_edges(
                            self.mode, self.filter_low_hz, self.filter_high_hz, presets[idx - 1]);
                        let _ = self.cmd_tx.send(Command::SetControl(
                            ControlId::FilterLow, low as i16 as u16));
                        let _ = self.cmd_tx.send(Command::SetControl(
                            ControlId::FilterHigh, high as i16 as u16));
                        self.filter_low_hz = low;
                        self.filter_high_hz = high;
                        self.filter_changed_at = Some(Instant::now());
                    }

                    ui.label(RichText::new(format_bandwidth(presets[idx], cw)).strong().size(14.0));

                    let plus_btn = egui::Button::new(RichText::new(" + ").size(14.0));
                    if ui.add_enabled(idx < presets.len() - 1, plus_btn).clicked() {
                        let (low, high) = calc_filter_edges(
                            self.mode, self.filter_low_hz, self.filter_high_hz, presets[idx + 1]);
                        let _ = self.cmd_tx.send(Command::SetControl(
                            ControlId::FilterLow, low as i16 as u16));
                        let _ = self.cmd_tx.send(Command::SetControl(
                            ControlId::FilterHigh, high as i16 as u16));
                        self.filter_low_hz = low;
                        self.filter_high_hz = high;
                        self.filter_changed_at = Some(Instant::now());
                    }
                });
            }

            // Memory buttons
            ui.horizontal(|ui| {
                for i in 0..NUM_MEMORIES {
                    let label = if let Some(hz) = self.memories[i].frequency_hz {
                        let band = band_label(hz);
                        if band.is_empty() {
                            format!("M{}", i + 1)
                        } else {
                            band.to_string()
                        }
                    } else {
                        format!("M{}", i + 1)
                    };

                    // Highlight: save mode (orange), current band match (blue), default
                    let is_current_band = self.memories[i].frequency_hz
                        .map(|hz| {
                            let mem_band = band_label(hz);
                            let cur_band = band_label(self.frequency_hz);
                            !mem_band.is_empty() && mem_band == cur_band
                        })
                        .unwrap_or(false);

                    let btn = if self.save_mode {
                        egui::Button::new(RichText::new(&label))
                            .fill(Color32::from_rgb(120, 80, 30))
                    } else if is_current_band {
                        egui::Button::new(RichText::new(&label).strong())
                            .fill(Color32::from_rgb(100, 160, 230))
                    } else {
                        egui::Button::new(&label)
                    };

                    if ui.add(btn).clicked() {
                        if self.save_mode {
                            if self.frequency_hz > 0 {
                                self.memories[i] = Memory {
                                    frequency_hz: Some(self.frequency_hz),
                                    mode: Some(self.mode),
                                };
                                self.save_full_config();
                            }
                            self.save_mode = false;
                        } else if let Some(hz) = self.memories[i].frequency_hz {
                            let _ = self.cmd_tx.send(Command::SetFrequency(hz));
                                self.set_pending_freq_a(hz);
                            if let Some(mode) = self.memories[i].mode {
                                let _ = self.cmd_tx.send(Command::SetMode(mode));
                                self.mode = mode;
                                self.filter_changed_at = None;
                            }
                        }
                    }
                }

                let save_btn = if self.save_mode {
                    egui::Button::new(RichText::new("Save").strong())
                        .fill(Color32::from_rgb(150, 60, 30))
                } else {
                    egui::Button::new("Save")
                };
                if ui.add(save_btn).clicked() {
                    self.save_mode = !self.save_mode;
                }
            });


            ui.horizontal(|ui| {
                // NR cycle: OFF -> NR1 -> NR2 -> NR3 -> NR4 -> OFF
                let nr_label = if self.nr_level == 0 { "NR".to_string() } else { format!("NR{}", self.nr_level) };
                let nr_btn = if self.nr_level > 0 {
                    egui::Button::new(RichText::new(&nr_label).strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else {
                    egui::Button::new(&nr_label)
                };
                if ui.add(nr_btn).clicked() {
                    let new_val = if self.nr_level >= 4 { 0 } else { self.nr_level + 1 };
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::NoiseReduction, new_val as u16));
                    self.nr_level = new_val;
                }

                // ANF toggle
                let anf_btn = if self.anf_on {
                    egui::Button::new(RichText::new("ANF").strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else {
                    egui::Button::new("ANF")
                };
                if ui.add(anf_btn).clicked() {
                    let new_val = !self.anf_on;
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::AutoNotchFilter, new_val as u16));
                    self.anf_on = new_val;
                }

                // Mic AGC toggle
                let agc_btn = if self.agc_enabled {
                    egui::Button::new(RichText::new("Mic AGC").strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else {
                    egui::Button::new("Mic AGC")
                };
                if ui.add(agc_btn).clicked() {
                    let new_val = !self.agc_enabled;
                    let _ = self.cmd_tx.send(Command::SetAgcEnabled(new_val));
                    self.agc_enabled = new_val;
                    self.save_full_config();
                }

                ui.separator();

                // Drive level slider (inline)
                ui.label("Drive:");
                let mut drive_f32 = self.drive_level as f32;
                let slider = egui::Slider::new(&mut drive_f32, 0.0..=100.0)
                    .show_value(false)
                    .custom_formatter(|v, _| format!("{:.0}%", v));
                if ui.add(slider).changed() {
                    let new_val = drive_f32.round() as u8;
                    if new_val != self.drive_level {
                        let _ = self.cmd_tx.send(Command::SetControl(ControlId::DriveLevel, new_val as u16));
                        self.drive_level = new_val;
                    }
                }
            });

            // Diversity
            ui.separator();
            egui::CollapsingHeader::new(RichText::new("Diversity").strong().size(14.0))
                .default_open(false)
                .show(ui, |ui| {
                    self.render_diversity(ui);
                });

            } // end of Radio tab
        });

        // Pop-out viewports: joined or separate
        let show_rx1_popout = self.spectrum_popout && self.spectrum_enabled && !self.spectrum_bins.is_empty();
        let show_rx2_popout = self.rx2_popout && self.rx2_enabled;

        if show_rx1_popout && show_rx2_popout && self.popout_joined {
            // Joined mode: single combined window with RX1 on top, RX2 below
            ctx.show_viewport_immediate(
                ViewportId::from_hash_of("spectrum_popout"),
                ViewportBuilder::default()
                    .with_title("ThetisLink - RX1 + RX2")
                    .with_inner_size([900.0, 900.0]),
                |ctx, _class| {
                    if ctx.input(|i| i.viewport().close_requested()) {
                        self.spectrum_popout = false;
                        self.rx2_popout = false;
                        return;
                    }
                    egui::CentralPanel::default().show(ctx, |ui| {
                        // Controls side by side: RX1 left, RX2 right
                        ui.columns(2, |cols| {
                            // Left column: RX1 controls
                            self.render_rx1_controls(&mut cols[0], controls::UiSurface::PopoutJoined);

                            // Right column: RX2 controls with Split button on S-meter row
                            self.render_rx2_controls_with_split(&mut cols[1], true, true, controls::UiSurface::PopoutJoined);
                        });

                        ui.separator();

                        // Spectrums stacked: RX1 on top, RX2 below
                        let total_w = ui.available_width();
                        let available = ui.available_height();
                        let half = (available - 4.0) / 2.0;
                        ui.allocate_ui(egui::vec2(total_w, half), |ui| {
                            self.render_spectrum_content(ui, ctx, 0.0, true);
                        });
                        ui.add_space(2.0);
                        self.render_rx2_spectrum_only(ui, ctx);

                        // Read RX2 spectrum interaction keys inside viewport
                        self.handle_rx2_spectrum_keys(ctx);
                    });

                    // Read RX1 spectrum interaction keys inside viewport
                    self.handle_rx1_spectrum_keys(ctx);
                    ctx.request_repaint_after(std::time::Duration::from_millis(33));

                    // Floating A⇔B overlay
                    let r1 = self.popout_rx1_smeter_rect;
                    let r2 = self.popout_rx2_smeter_rect;
                    if r1.is_positive() && r2.is_positive() {
                        let pos = if self.popout_meter_analog {
                            // Bottom-left of RX1 analog meter
                            egui::pos2(r1.left() + 4.0, r1.max.y - 24.0)
                        } else {
                            // Centered between the two bar S-meters
                            let center_x = (r1.right() + r2.left()) / 2.0;
                            let center_y = (r1.center().y + r2.center().y) / 2.0;
                            egui::pos2(center_x - 23.0, center_y - 10.0)
                        };
                        egui::Area::new(egui::Id::new("vfo_swap_joined"))
                            .fixed_pos(pos)
                            .order(egui::Order::Foreground)
                            .interactable(true)
                            .show(ctx, |ui| {
                                if ui.add_enabled(self.connected, egui::Button::new(RichText::new("A<>B").size(10.0))).clicked() {
                                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::VfoSwap, 2));
                                }
                            });
                    }
                },
            );
        } else {
            // Separate mode: individual windows
            if show_rx1_popout {
                ctx.show_viewport_immediate(
                    ViewportId::from_hash_of("spectrum_popout"),
                    ViewportBuilder::default()
                        .with_title("ThetisLink - RX1 / VFO-A")
                        .with_inner_size([900.0, 600.0]),
                    |ctx, _class| {
                        if ctx.input(|i| i.viewport().close_requested()) {
                            self.spectrum_popout = false;
                            return;
                        }
                        egui::CentralPanel::default().show(ctx, |ui| {
                            // Join button right-aligned (only visible when RX2 is also popped out)
                            if show_rx2_popout {
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                                    if ui.button("Join").clicked() {
                                        self.popout_joined = true;
                                        self.save_full_config();
                                    }
                                });
                            }
                            self.render_rx1_popout_content(ui, ctx);
                            // Read spectrum interaction keys inside this viewport
                            self.handle_rx1_spectrum_keys(ctx);
                            ctx.request_repaint_after(std::time::Duration::from_millis(33));
                            // A⇔B overlay
                            if show_rx2_popout {
                                let r = self.popout_rx1_smeter_rect;
                                if r.is_positive() {
                                    let btn_pos = if self.popout_meter_analog {
                                        egui::pos2(r.left() + 27.0, r.max.y - 12.0)
                                    } else {
                                        let panel_right = ui.max_rect().right() - 4.0;
                                        egui::pos2(panel_right - 23.0, r.center().y)
                                    };
                                    let btn_rect = egui::Rect::from_center_size(
                                        btn_pos,
                                        egui::vec2(46.0, 20.0),
                                    );
                                    let resp = ui.add_enabled_ui(self.connected, |ui| {
                                        ui.put(btn_rect, egui::Button::new(RichText::new("A<>B").size(10.0)))
                                    }).inner;
                                    if resp.clicked() {
                                        let _ = self.cmd_tx.send(Command::SetControl(ControlId::VfoSwap, 2));
                                    }
                                }
                            }
                        });
                    },
                );
            }

            if show_rx2_popout {
                ctx.show_viewport_immediate(
                    ViewportId::from_hash_of("rx2_popout"),
                    ViewportBuilder::default()
                        .with_title("ThetisLink - RX2 / VFO-B")
                        .with_inner_size([900.0, 600.0]),
                    |ctx, _class| {
                        if ctx.input(|i| i.viewport().close_requested()) {
                            self.rx2_popout = false;
                            return;
                        }
                        egui::CentralPanel::default().show(ctx, |ui| {
                            // Join button right-aligned (only visible when RX1 is also popped out)
                            if show_rx1_popout {
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                                    if ui.button("Join").clicked() {
                                        self.popout_joined = true;
                                        self.save_full_config();
                                    }
                                });
                            }
                            self.render_rx2_content(ui, ctx);
                            // Read spectrum interaction keys inside this viewport
                            self.handle_rx2_spectrum_keys(ctx);
                            ctx.request_repaint_after(std::time::Duration::from_millis(33));
                            // A⇔B overlay
                            if show_rx1_popout {
                                let r = self.popout_rx2_smeter_rect;
                                if r.is_positive() {
                                    let btn_pos = if self.popout_meter_analog {
                                        egui::pos2(r.left() + 27.0, r.max.y - 12.0)
                                    } else {
                                        let panel_right = ui.max_rect().right() - 4.0;
                                        egui::pos2(panel_right - 23.0, r.center().y)
                                    };
                                    let btn_rect = egui::Rect::from_center_size(
                                        btn_pos,
                                        egui::vec2(46.0, 20.0),
                                    );
                                    let resp = ui.add_enabled_ui(self.connected, |ui| {
                                        ui.put(btn_rect, egui::Button::new(RichText::new("A<>B").size(10.0)))
                                    }).inner;
                                    if resp.clicked() {
                                        let _ = self.cmd_tx.send(Command::SetControl(ControlId::VfoSwap, 2));
                                    }
                                }
                            }
                        });
                    },
                );
            }
        }

        // Yaesu popout window
        if self.yaesu_popout && self.yaesu_enabled {
            ctx.show_viewport_immediate(
                ViewportId::from_hash_of("yaesu_popout"),
                ViewportBuilder::default()
                    .with_title("ThetisLink - Yaesu FT-991A")
                    .with_inner_size([465.0, 335.0]),
                |ctx, _class| {
                    if ctx.input(|i| i.viewport().close_requested()) {
                        self.yaesu_popout = false;
                        self.save_full_config();
                        return;
                    }
                    // Fixed PTT button at bottom
                    egui::TopBottomPanel::bottom("yaesu_ptt_panel").show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            // PTT button — locked when other client is transmitting
                            let (ptt_color, ptt_text, ptt_locked) = if self.other_tx {
                                (Color32::from_rgb(200, 120, 0), "TX in use", true)
                            } else if self.yaesu_tx_active {
                                (Color32::RED, "TX", false)
                            } else {
                                (Color32::from_rgb(60, 60, 60), "PTT", false)
                            };
                            let ptt_btn = egui::Button::new(
                                RichText::new(ptt_text).size(18.0).color(Color32::WHITE).strong(),
                            ).fill(ptt_color).min_size(egui::vec2(80.0, 40.0));
                            let response = ui.add_enabled(!ptt_locked, ptt_btn);
                            if self.yaesu_ptt_toggle_mode {
                                if response.clicked() {
                                    let new_tx = !self.yaesu_tx_active;
                                    let _ = self.cmd_tx.send(Command::SetYaesuPtt(new_tx));
                                }
                            } else {
                                // Momentary: only send on state changes from THIS button
                                let pressing = ui.input(|i| {
                                    i.pointer.primary_down()
                                        && response.rect.contains(i.pointer.interact_pos().unwrap_or(egui::Pos2::ZERO))
                                });
                                if pressing != self.yaesu_mouse_ptt {
                                    self.yaesu_mouse_ptt = pressing;
                                    let _ = self.cmd_tx.send(Command::SetYaesuPtt(pressing));
                                }
                            }

                            ui.separator();

                            // Mic gain slider for Yaesu USB TX audio
                            ui.label("Mic gain:");
                            let slider = egui::Slider::new(&mut self.yaesu_mic_gain, 0.05..=3.0)
                                .logarithmic(true)
                                .custom_formatter(|v, _| format!("{:.2}x", v));
                            if ui.add(slider).changed() {
                                let _ = self.cmd_tx.send(Command::SetYaesuTxGain(self.yaesu_mic_gain));
                            }
                        });
                    });
                    egui::CentralPanel::default().show(ctx, |ui| {
                        self.render_yaesu_popout(ui);
                    });
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                },
            );
        }

        // Handle spectrum interaction keys (fallback for main-window spectrum;
        // popout viewports handle their own keys inside the viewport closure)
        self.handle_rx2_spectrum_keys(ctx);
        self.handle_rx1_spectrum_keys(ctx);

        // About window
        if self.show_about {
            egui::Window::new("About ThetisLink")
                .collapsible(false)
                .resizable(true)
                .default_size([420.0, 500.0])
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new("ThetisLink").size(22.0).strong());
                            ui.label(RichText::new(format!("v{}", sdr_remote_core::version_string())).size(14.0));
                            ui.add_space(4.0);
                            ui.label("Remote control for Thetis SDR + Yaesu FT-991A");
                        });
                        ui.add_space(8.0);
                        ui.separator();

                        ui.label(RichText::new("Author").size(13.0).strong());
                        ui.label("Chiron van der Burgt — PA3GHM");

                        ui.add_space(6.0);
                        ui.label(RichText::new("Special Thanks").size(13.0).strong());
                        ui.label("Richie (ramdor) — Thetis SDR development, TCI protocol extensions");

                        ui.add_space(6.0);
                        ui.label(RichText::new("Protocols & External Services").size(13.0).strong());
                        ui.label("TCI — Expert Electronics / Thetis");
                        ui.label("DX Spider — DX cluster telnet protocol");
                        ui.label("HPSDR / OpenHPSDR Protocol 2");
                        ui.label("WebSDR (PA3FWM) / KiwiSDR — CatSync targets");

                        ui.add_space(6.0);
                        ui.label(RichText::new("Hardware Support").size(13.0).strong());
                        egui::Grid::new("hw_grid").num_columns(2).spacing([12.0, 2.0]).show(ui, |ui| {
                            for (dev, iface) in [
                                ("ANAN 7000DLE", "TCI (via Thetis)"),
                                ("Yaesu FT-991A", "Serial CAT + USB Audio"),
                                ("RF2K-S PA", "HTTP API"),
                                ("SPE Expert 1.3K-FA", "Serial"),
                                ("JC-4s Tuner", "Serial (DTR)"),
                                ("UltraBeam RCU-06", "Serial"),
                                ("Amplitec 6/2", "Serial"),
                                ("EA7HG Visual Rotor", "UDP"),
                            ] {
                                ui.label(dev);
                                ui.label(RichText::new(iface).color(Color32::GRAY));
                                ui.end_row();
                            }
                        });

                        ui.add_space(6.0);
                        ui.label(RichText::new("Open Source Libraries").size(13.0).strong());
                        let libs = [
                            ("tokio", "Async runtime"),
                            ("eframe / egui", "Desktop GUI"),
                            ("cpal", "Audio I/O"),
                            ("audiopus", "Opus codec"),
                            ("rubato", "Resampling"),
                            ("rustfft", "FFT spectrum"),
                            ("ringbuf", "Lock-free buffers"),
                            ("tokio-tungstenite", "TCI WebSocket"),
                            ("serialport", "Yaesu CAT"),
                            ("midir", "MIDI controller"),
                            ("wry", "WebView (CatSync)"),
                        ];
                        egui::Grid::new("lib_grid").num_columns(2).spacing([12.0, 1.0]).show(ui, |ui| {
                            for (lib, purpose) in libs {
                                ui.label(RichText::new(lib).size(11.0));
                                ui.label(RichText::new(purpose).size(11.0).color(Color32::GRAY));
                                ui.end_row();
                            }
                        });

                        ui.add_space(6.0);
                        ui.label(RichText::new("License").size(13.0).strong());
                        ui.label("GPL-2.0-or-later (see LICENSE)");
                        ui.label("Copyright © 2025-2026 Chiron van der Burgt");
                        ui.horizontal(|ui| {
                            ui.label("Source:");
                            ui.hyperlink("https://github.com/cjenschede/ThetisLink");
                        });
                        ui.label("Based on the Thetis SDR lineage — see ATTRIBUTION.md");

                        ui.add_space(12.0);
                        ui.vertical_centered(|ui| {
                            if ui.button("Close").clicked() {
                                self.show_about = false;
                            }
                        });
                    });
                });
        }

        // Log panel (collapsible, bottom of window)
        egui::TopBottomPanel::bottom("log_panel").show_animated(ctx, self.show_log, |ui| {
            ui.set_max_height(150.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Log").strong().size(11.0));
                if ui.small_button("Clear").clicked() {
                    if let Ok(mut buf) = self.log_buffer.lock() {
                        buf.clear();
                    }
                }
            });
            ui.separator();
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .max_height(120.0)
                .show(ui, |ui| {
                    if let Ok(buf) = self.log_buffer.lock() {
                        for line in buf.iter() {
                            ui.label(RichText::new(line).monospace().size(9.0).color(Color32::from_rgb(180, 180, 180)));
                        }
                    }
                });
        });

        // Adaptive repaint rate: 30fps when active, 2fps when idle
        let needs_fast_repaint = self.connected
            || self.spectrum_popout
            || self.rx2_popout;
        let repaint_ms = if needs_fast_repaint { 33 } else { 500 };
        ctx.request_repaint_after(std::time::Duration::from_millis(repaint_ms));
    }
}


























