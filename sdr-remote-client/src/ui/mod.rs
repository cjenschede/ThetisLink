// SPDX-License-Identifier: GPL-2.0-or-later

#![allow(dead_code)]

mod helpers;
mod meters;
mod window_placement;
mod spectrum;
pub(crate) mod theme;
pub(crate) mod config;
mod devices;
mod screens;
mod wizard;
pub(crate) mod controls;
pub(crate) mod yaesu_memory;
pub(crate) mod yaesu_menu;
pub(crate) mod ftx1_ex_chart;

pub(crate) use helpers::*;
pub(crate) use meters::*;
pub(crate) use spectrum::*;

/// Convert RX filter edges (signed Hz) to the TX modulation filter band - a
/// POSITIVE audio passband (Thetis applies the sideband per mode).
/// - one-sided (USB both ≥0, LSB both ≤0) -> min..max of magnitudes;
/// - straddling 0 (AM/SAM/FM/DSB stored as −W..+W) -> 0..max(|lo|,|hi|).
/// Without the straddle case a symmetric AM band collapses to W..W (zero-wide).
pub(crate) fn rx_to_tx_band(low_hz: i32, high_hz: i32) -> (i32, i32) {
    if low_hz < 0 && high_hz > 0 {
        (0, low_hz.abs().max(high_hz.abs()))
    } else {
        (low_hz.abs().min(high_hz.abs()), low_hz.abs().max(high_hz.abs()))
    }
}

pub(super) fn yaesu_mic_gain_to_display(gain: f32) -> f32 {
    (gain / 0.4).clamp(0.05, 1.0)
}

pub(super) fn yaesu_mic_gain_from_display(display: f32) -> f32 {
    (display * 0.4).clamp(0.02, 0.4)
}
pub(crate) use config::{load_window_size, load_window_pos, save_config, load_config, NUM_MEMORIES};

/// Startup check for the MAIN window's saved position: true when a usable part would land
/// on a currently-connected monitor. Lets `main.rs` drop `with_position()` for a position
/// left on a since-disconnected/rearranged monitor, so the main window can never open
/// off-screen (self-heal). Mirrors the per-pop-out check, but uses the system DPI because
/// no egui context exists yet at startup. On non-Windows / query failure it returns true.
pub(crate) fn main_window_pos_visible(pos: [f32; 2], size: [f32; 2]) -> bool {
    window_placement::saved_window_is_visible(
        egui::pos2(pos[0], pos[1]),
        egui::vec2(size[0], size[1]),
        window_placement::system_ppp(),
    )
}

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
    /// Per-row span (Hz) for the `full_rows`. For RX1/RX2 the span is
    /// constant (full DDC) so all entries equal `wf_full_span_hz` passed
    /// to the renderer. For VRX high-res use we push extracted rows
    /// with varying spans (zoom changes width), so the renderer reads
    /// the per-row value instead of one global span.
    pub(crate) full_spans: Vec<u32>,
    view_rows: Vec<Vec<u16>>,
    view_centers: Vec<u32>,
    view_spans: Vec<u32>,
    write_idx: usize,
    count: usize,
    last_seq: u16,
    pub(crate) height: usize,
    texture: Option<TextureHandle>,
}

impl WaterfallRingBuffer {
    fn new(height: usize) -> Self {
        Self {
            full_rows: vec![Vec::new(); height],
            full_centers: vec![0; height],
            full_spans: vec![0; height],
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
        self.full_spans[idx] = full_span_hz;
        self.view_rows[idx] = view_bins.to_vec();
        self.view_centers[idx] = view_center_hz;
        self.view_spans[idx] = view_span_hz;
        self.write_idx = (idx + 1) % self.height;
        if self.count < self.height {
            self.count += 1;
        }
    }

    /// Push only the "full" side (no view). Used for VRX high-res
    /// waterfall where bins are the extracted view directly. Per-row
    /// span is captured so zoom/freq changes during history render
    /// correctly.
    fn push_full_only(&mut self, bins: &[u16], center_hz: u32, span_hz: u32, sequence: u16) {
        if bins.is_empty() || span_hz == 0 || sequence == self.last_seq {
            return;
        }
        self.last_seq = sequence;
        let idx = self.write_idx;
        self.full_rows[idx] = bins.to_vec();
        self.full_centers[idx] = center_hz;
        self.full_spans[idx] = span_hz;
        self.view_rows[idx] = Vec::new();
        self.view_centers[idx] = 0;
        self.view_spans[idx] = 0;
        self.write_idx = (idx + 1) % self.height;
        if self.count < self.height {
            self.count += 1;
        }
    }
}

/// egui application - communicates with engine via watch/mpsc channels
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
    // PTT switch-on spike protection (built-in speaker+mic in one chassis). See config.rs.
    spike_protection: bool,
    mic_gate_delay_thetis_ms: u32,
    mic_gate_delay_yaesu_ms: u32,
    // Audio recording / playback
    recording: bool,
    playing: bool,
    rec_rx1: bool,
    rec_rx2: bool,
    rec_yaesu: bool,
    rec_yaesu2: bool,
    rec_vrx1: bool,
    rec_vrx2: bool,
    last_recorded_path: Option<String>,
    midi_ptt_toggle_mode: bool,  // independent MIDI PTT mode
    /// S-meter source: 0=Sig, 1=Avg (default), 2=MaxBin. Single setting shared
    /// by RX1 and RX2. Translated to a per-RX bitmap by the engine and pushed
    /// via ControlId::SmeterSources whenever it changes.
    smeter_source: u8,
    /// TL2-1 ctun-auto-recenter setup-vink "Allow zoom below 2x (with smear during tune)".
    /// Default false -> zoom-min 2x. True -> zoom-min 1x toegestaan.
    allow_zoom_below_2x: bool,
    /// PATCH-1: UI language for connect-status / connect-error display.
    /// "en" or "nl" from config; defaults to "en".
    ui_language: String,
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
    diversity_sa_iteration: u8,       // alternation counter (phase->gain->phase->gain)
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
    /// Mic device -> TX profile name mapping (auto-switch on mic change)
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
    yaesu_audio_packets: u64,
    yaesu_jitter_ms: f32,
    yaesu_buffer_depth: u32,
    yaesu2_audio_packets: u64,
    yaesu2_jitter_ms: f32,
    yaesu2_buffer_depth: u32,
    vrx1_audio_packets: u64,
    vrx1_jitter_ms: f32,
    vrx1_buffer_depth: u32,
    vrx2_audio_packets: u64,
    vrx2_jitter_ms: f32,
    vrx2_buffer_depth: u32,
    down_kbps: u32,
    up_kbps: u32,
    bw_breakdown: Vec<(u8, u32)>,
    bw_breakdown_expanded: bool,
    loss_percent: u8,
    capture_level: f32,
    playback_level: f32,
    playback_level_bin_r: f32,
    playback_level_rx2: f32,
    playback_level_yaesu: f32,
    playback_level_yaesu2: f32,
    yaesu_mic_level: f32,
    frequency_hz: u64,
    mode: u8,
    /// RX1 S-meter - dBm in RX mode, watts in TX mode (disambiguated by
    /// `self.ptt || self.other_tx`).  `SMETER_NO_DATA_DBM` (-200.0) before
    /// the first sample arrives.
    smeter: f32,
    smeter_peak: f32,
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
    // TX modulation filter (PATCH-tx-modulation-bandwidth) - main-radio TX,
    // not VRX. `follow_rx` mirrors the RX filter 1:1; otherwise the low/high
    // fields set it independently. Greyed unless the server reports support.
    tx_filter_follow_rx: bool,
    tx_filter_low_hz: i32,
    tx_filter_high_hz: i32,
    tx_filter_supported: bool,
    tx_filter_initialized: bool,
    last_tx_follow_sent: Option<(i32, i32)>,
    tx_follow_last_send_at: Option<Instant>,
    thetis_configured: bool,
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
    // Frequency change tracking (prevents bounce: local->server_old->server_new)
    pending_freq: Option<u64>,
    pending_freq_at: Option<Instant>,
    rx2_pending_freq: Option<u64>,
    rx2_pending_freq_at: Option<Instant>,
    yaesu_pending_freq: Option<u64>,
    yaesu_pending_freq_at: Option<Instant>,
    yaesu2_pending_freq: Option<u64>,
    yaesu2_pending_freq_at: Option<Instant>,
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
    /// User-set total height (egui-points) of the spectrum+waterfall block in
    /// the Radio tab. Persisted in config. Range 300..=1200. Popouts ignore.
    spectrum_total_h: f32,
    spectrum_popout: bool,
    // Window size persistence
    window_w: f32,
    window_h: f32,
    // Main-window geometrie gewijzigd (tijdens drag) maar nog niet weggeschreven.
    // Save vuurt pas bij pointer-up (anti-I/O-stall); zonder deze vlag werd de
    // eind-geometrie nooit opgeslagen omdat de laatste wijziging tijdens de drag
    // viel en de release-frame zelf geen wijziging meer toont.
    main_geom_dirty: bool,
    // Log panel
    log_buffer: LogBuffer,
    show_log: bool,
    show_about: bool,
    // VRX joint window - two channels (VRX1 on RX1+VFO-A, VRX2 on
    // RX2+VFO-B), shown side-by-side in one popout viewport.
    show_vrx: bool,
    vrx1_enabled: bool,
    vrx1_freq_hz: u64,
    vrx1_mode: u8, // 0=USB, 1=LSB
    vrx1_volume: f32, // local mix gain 0.0..=2.0
    vrx2_enabled: bool,
    vrx2_freq_hz: u64,
    vrx2_mode: u8,
    vrx2_volume: f32,
    /// SAM auto-tune-to-carrier per VRX (PATCH-vrx-wide-sam-ux). When on
    /// and the mode is SAM, the server follows the carrier and pushes the
    /// new freq; the VRX VFO display tracks it. Persisted.
    vrx1_auto_tune: bool,
    vrx2_auto_tune: bool,
    /// VRX audio-rate mode: 0=NB, 1=WB, 2=Auto. One setting for both VRX
    /// (server-tab dropdown). Persisted.
    vrx_rate_mode: u8,
    vrx_rate_mode2: u8,
    /// Last auto-tune freq applied to the display (per VRX) - to detect
    /// fresh server pushes without re-snapping every frame.
    last_vrx1_autotune_hz: u64,
    last_vrx2_autotune_hz: u64,
    vrx_popout_pos: Option<egui::Pos2>,
    vrx_popout_size: Option<egui::Vec2>,
    vrx_popout_init_applied: bool,
    playback_level_vrx1: f32,
    playback_level_vrx2: f32,
    /// Per-DDC-position remembered VRX freq. Key = DDC center in
    /// units of 100 kHz (= DDC_center_hz / 100_000). Coarse enough to
    /// dedupe within-band CTUN re-centering, fine enough to keep
    /// distinct memories per HF band. Updated when the user changes
    /// VRX freq, restored on DDC-center bucket changes.
    vrx1_freq_by_bucket: std::collections::HashMap<u64, u64>,
    vrx2_freq_by_bucket: std::collections::HashMap<u64, u64>,
    /// Last DDC center we saw - used to detect bucket-switches.
    last_vrx1_ddc_center_hz: u64,
    last_vrx2_ddc_center_hz: u64,
    /// VRX spectrum zoom levels (per VRX). 32× default = ~12 kHz view
    /// at 384 kHz DDC. Range matches RX1/RX2 spectrum (1× to 1024×).
    vrx1_spectrum_zoom: f32,
    vrx2_spectrum_zoom: f32,
    // Per-VRX dB-scale + waterfall contrast (parallel aan
    // self.spectrum_ref_db / range_db / waterfall_contrast voor RX1).
    vrx1_ref_db: f32,
    vrx1_range_db: f32,
    vrx1_wf_contrast: f32,
    vrx1_pan: f32,
    vrx1_auto_ref: bool,
    vrx1_zoom_initialized: bool,
    /// VRX1 SSB filter edges as signed Hz offsets from carrier (UI
    /// convention matching main spectrum). USB: both ≥0, LSB: both ≤0.
    /// Snapped to 62.5 Hz audio bin grid before sending to server.
    vrx1_filter_low_hz: i32,
    vrx1_filter_high_hz: i32,
    /// High-res spectrum toggle per VRX. When on, client sends
    /// VrxSpectrumEnable to server; server emits SpectrumVrx1/2
    /// packets with high-resolution extracted bins centered on
    /// VRX freq. Persisted.
    vrx1_high_res_spectrum: bool,
    /// Latest received extracted-bins frame (mirrored from state).
    vrx1_extracted_bins: Vec<u16>,
    vrx1_extracted_center_hz: u32,
    vrx1_extracted_span_hz: u32,
    vrx1_extracted_sequence: u16,
    /// Last span (kHz) sent to server; resend on change to avoid spam.
    vrx1_high_res_last_span_khz: u16,
    /// Dedicated waterfall ring for VRX1 high-res mode. Fed from
    /// extracted-bin packets; rendered instead of the full-DDC RX1
    /// waterfall when the high-res toggle is on.
    vrx1_hr_waterfall: WaterfallRingBuffer,
    // transient
    vrx1_auto_ref_value: f32,
    vrx1_auto_ref_frames: u32,
    vrx1_auto_ref_initialized: bool,
    vrx1_smeter_dbm: f32,
    vrx1_smeter_initialized: bool,
    vrx1_smeter_peak: f32,
    vrx1_smeter_peak_time: Instant,
    vrx2_ref_db: f32,
    vrx2_range_db: f32,
    vrx2_wf_contrast: f32,
    vrx2_pan: f32,
    vrx2_auto_ref: bool,
    vrx2_zoom_initialized: bool,
    vrx2_filter_low_hz: i32,
    vrx2_filter_high_hz: i32,
    vrx2_high_res_spectrum: bool,
    vrx2_extracted_bins: Vec<u16>,
    vrx2_extracted_center_hz: u32,
    vrx2_extracted_span_hz: u32,
    vrx2_extracted_sequence: u16,
    vrx2_high_res_last_span_khz: u16,
    vrx2_hr_waterfall: WaterfallRingBuffer,
    vrx2_auto_ref_value: f32,
    vrx2_auto_ref_frames: u32,
    vrx2_auto_ref_initialized: bool,
    vrx2_smeter_dbm: f32,
    vrx2_smeter_initialized: bool,
    vrx2_smeter_peak: f32,
    vrx2_smeter_peak_time: Instant,
    /// Texture handles for VRX waterfall rendering - one per VRX so
    /// zoom/pan stays independent. Rebuilt on each render from the
    /// shared RX1/RX2 waterfall ring buffers (no duplicate storage).
    vrx1_waterfall_texture: Option<egui::TextureHandle>,
    vrx2_waterfall_texture: Option<egui::TextureHandle>,
    /// One-shot flag: send the locally-persisted VRX state for BOTH
    /// channels (enable + freq + mode + volume) to the server once
    /// after the first successful connect, then stay quiet until user
    /// interacts. Reset to false on disconnect so a reconnect re-syncs.
    vrx_state_sync_pending: bool,
    // Devices screen
    active_tab: Tab,
    /// PATCH-1 smoke-test fix (2026-05-13): track the previous frame's
    /// connect_status so we can auto-switch to the Server tab on transitions
    /// that demand user attention (AwaitingTotp, Failed). Without this the
    /// user might be on the Radio tab and never see the 2FA prompt or
    /// error message until they manually switch tabs.
    last_connect_status: sdr_remote_logic::state::ConnectStatus,
    /// PATCH-3 mDNS discovery: background browse for servers on the LAN.
    /// `None` if the mDNS daemon failed to start (silent fallback to
    /// manual IP entry).
    mdns_browse: Option<crate::mdns::BrowseHandle>,
    /// Phase A relay monitor: status-only outbound WebSocket path.
    relay_enabled: bool,
    relay_url: String,
    relay_station: String,
    relay_token: String,
    relay_instance_id: String,
    relay_device_name: String,
    relay_udp_enabled: bool,
    relay_monitor: Option<sdr_remote_relay::RelayMonitor>,
    relay_status: Option<sdr_remote_relay::RelayStatusHandle>,
    /// Phase C: relay draait als transport (monitor in main.rs). Dan geen eigen
    /// monitor beheren; config-wijzigingen vereisen een app-herstart.
    relay_external: bool,
    /// PATCH-4 first-run wizard. `Some` while the wizard owns the
    /// viewport; transitions back to `None` on Skip / Finished / when
    /// the operator re-launches the wizard manually.
    wizard_state: Option<wizard::WizardState>,
    device_tab: u8, // 0=Amplitec, 1=Tuner, 2=SPE, 3=RF2K, 4=UltraBeam
    amplitec_available: bool,
    amplitec_connected: bool,
    amplitec_switch_a: u8,
    amplitec_switch_b: u8,
    amplitec_labels: String,
    amplitec_log: VecDeque<(String, String)>,  // (timestamp, message)
    /// Power-cap tabel: actuele waarden van de server (read-only spiegel).
    amplitec_power_max_w: [u16; 6],
    amplitec_power_tx_blocked: [bool; 6],
    amplitec_power_loaded: bool,
    /// Power-cap edit-state. Wordt geïnitialiseerd uit server-waarden zodra
    /// `amplitec_power_loaded` true wordt; daarna alleen door de operator
    /// gewijzigd (DragValue / Checkbox). Save-knop stuurt deze waarden naar
    /// de server.
    amplitec_power_edit_max_w: [u16; 6],
    amplitec_power_edit_tx_blocked: [bool; 6],
    /// Collapsing-section open/dicht. Persisted in client config.
    amplitec_power_show: bool,
    /// Index van de favoriet die nu in inline-edit modus staat (max één
    /// tegelijk). `None` = alle labels read-only/selectable. Bij verlies
    /// van focus of Enter wordt deze terug op `None` gezet en de naam
    /// naar config geschreven.
    websdr_favorite_editing: Option<usize>,
    // Tuner state
    tuner_available: bool,
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
    ub_operation: u8,
    ub_freq_min_mhz: u16,
    ub_freq_max_mhz: u16,
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
    yaesu_smeter_peak: u16,
    yaesu_smeter_peak_time: Instant,
    yaesu_tx_active: bool,
    yaesu_power_on: bool,
    yaesu_volume: f32,
    // Dual-radio slot 1 (PATCH-dual-radio-991a-ftx1). yaesu_model/yaesu2_model =
    // wire-code uit RadioInfo (0=991A,1=FTX1) voor de paneel-naamgeving.
    yaesu_model: u8,
    yaesu2_model: u8,
    yaesu2_connected: bool,
    yaesu2_freq_a: u64,
    yaesu2_freq_b: u64,
    yaesu2_mode: u8,
    yaesu2_smeter: u16,
    yaesu2_smeter_peak: u16,
    yaesu2_smeter_peak_time: Instant,
    yaesu2_tx_active: bool,
    yaesu2_power_on: bool,
    yaesu2_split: bool,
    yaesu2_scan: bool,
    yaesu2_vfo_select: u8,      // 0=VFO, 1=Memory, 2=MemTune
    yaesu2_memory_channel: u16,
    yaesu2_tuner_state: u8, // interne ATU-stand van de radio (0=off,1=on,2=tuning) via AC;-poll
    yaesu2_feature_toggles: u32, // DSP/functie-toggles bitfield (PATCH-yaesu-extra-controls)
    yaesu2_feature_levels: [u8; 16], // multi-state/level-waarden (Fase B: AGC, IPO)
    yaesu2_squelch: u16,
    yaesu2_rf_gain: u16,
    yaesu2_rf_power: u16,
    yaesu2_mic_gain: f32,
    yaesu2_eq_enabled: bool,
    yaesu2_eq_gains: [f32; 5], // -12..+12 dB per band (eigen TX-EQ radio 2)
    yaesu2_eq_profiles: Vec<(String, bool, [f32; 5], f32)>,
    yaesu2_eq_active_profile: String,
    yaesu2_eq_new_name: String,
    collapse_yaesu2_eq: bool,
    collapse_yaesu2_memories: bool,
    yaesu2_control_changed_at: Option<std::time::Instant>, // debounce slider-sync
    yaesu2_volume: f32,
    // Eigen enable + PTT-mode voor radio 2 (los van radio 1), zodat elke Yaesu
    // z'n eigen aan/uit en Push-to-talk/Toggle-keuze heeft (operator-eis dual-radio).
    yaesu2_enabled: bool,
    yaesu2_ptt_toggle_mode: bool,
    yaesu2_enable_sent: bool,
    /// Uitgesteld auto-read-moment van de FTX-1-geheugens: de radio + server zijn
    /// vlak na connect te druk (bring-up/audio/polls) waardoor een directe MR-scan
    /// kanalen mist. Gezet bij enable, vuurt ~1,5s later één keer.
    yaesu2_autoread_at: Option<Instant>,
    // Cooldown-guard voor de auto-swap die HF naar A forceert (FTX-1: HF wint de
    // USB-audio, dus HF moet op de bestuurde/TX-kant A staan, anders split).
    yaesu2_hf_swap_at: Option<std::time::Instant>,
    // Slot-1 (FTX-1) eigen popout-window - los van het 991A-window.
    yaesu2_popout: bool,
    yaesu2_popout_pos: Option<egui::Pos2>,
    yaesu2_popout_size: Option<egui::Vec2>,
    yaesu2_popout_init_applied: bool,
    yaesu2_mouse_ptt: bool,
    yaesu_popout: bool,
    yaesu_popout_pos: Option<egui::Pos2>,
    yaesu_popout_size: Option<egui::Vec2>,
    /// Persisted geometry of the RX1 spectrum popout (separate-mode window).
    /// `None` until the user moves/resizes once; falls back to a 900x600 default.
    spectrum_popout_pos: Option<egui::Pos2>,
    spectrum_popout_size: Option<egui::Vec2>,
    /// Persisted geometry of the RX2 popout window (separate-mode).
    rx2_popout_pos: Option<egui::Pos2>,
    rx2_popout_size: Option<egui::Vec2>,
    /// Persisted geometry of the joined RX1+RX2 popout (when popout_joined=true
    /// and both spectrums are popped out into a single combined window).
    popout_joined_pos: Option<egui::Pos2>,
    popout_joined_size: Option<egui::Vec2>,
    /// Per-popout "init applied" flags. `false` means the next render must
    /// include `with_position()` so the saved geometry takes effect.
    /// Subsequent renders omit `with_position()` to avoid a feedback loop
    /// where every frame egui re-asserts the saved position, the OS rounds
    /// it by a sub-pixel, we read the new value back, save, and re-assert
    /// -> the window appears to jitter / vibrate after a manual move. The
    /// flags reset to `false` when the popout closes so the next reopen
    /// applies the saved position fresh.
    spectrum_popout_init_applied: bool,
    rx2_popout_init_applied: bool,
    popout_joined_init_applied: bool,
    yaesu_popout_init_applied: bool,
    yaesu_popout_first_frame: bool,
    yaesu_enable_sent: bool,
    yaesu_mic_gain: f32, // internal multiplier for Yaesu USB TX audio
    // Client-side TX-compressor (0-100) + AGC-toggle per radio (net als de EQ).
    yaesu_compressor: u8,
    yaesu2_compressor: u8,
    yaesu_tx_agc: bool,
    yaesu2_tx_agc: bool,
    yaesu_eq_enabled: bool,
    yaesu_eq_gains: [f32; 5], // -12..+12 dB per band
    yaesu_eq_profiles: Vec<(String, bool, [f32; 5], f32)>, // (name, enabled, gains, mic_gain)
    yaesu_eq_active_profile: String,
    yaesu_eq_new_name: String,
    yaesu_squelch: u16,       // 0-255
    yaesu_rf_gain: u16,       // 0-255
    yaesu_radio_mic_gain: u16, // 0-100 (radio's own mic gain)
    yaesu_rf_power: u16,      // 0-100 (TX power)
    yaesu_scan_active: bool,
    yaesu_split_active: bool,
    yaesu_tuner_state: u8, // interne ATU-stand van de radio (0=off,1=on,2=tuning) via AC;-poll
    yaesu_feature_toggles: u32, // DSP/functie-toggles bitfield (PATCH-yaesu-extra-controls)
    yaesu_feature_levels: [u8; 16], // multi-state/level-waarden (Fase B: AGC, IPO)
    // Fase C level-sliders (gedebouncet), [slot][NB,DNR,Processor,AMC]. Gesynct uit
    // feature_levels[8..12] als de debounce (yaesu_control_changed_at) verlopen is.
    yaesu_level_sliders: [[i32; 4]; 2],
    // Fase D freq-sliders (gedebouncet), [slot][Contour,APF,Notch]. Uit feature_freqs.
    yaesu_freq_sliders: [[i32; 3]; 2],
    // Clarifier-offset (§15) per slot, signed Hz - display uit feature_freqs[3].
    yaesu_clar_offset: i16,
    yaesu2_clar_offset: i16,
    // Gekozen stapgrootte voor de touch-vriendelijke frequentie-stapper (§16), gedeeld
    // over hoofd-RX en beide Yaesu-VFO's.
    tune_step_hz: i64,
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
    // Slot-1 (FTX-1) geheugen-state (Fase B). render_yaesu_memories wordt gedeeld
    // via mem::swap van deze velden + yaesu_mem_active_slot (voor read/write-cmd's).
    yaesu2_mem_channels: Vec<yaesu_memory::YaesuMemoryChannel>,
    yaesu2_mem_file: String,
    yaesu2_mem_selected: Option<usize>,
    yaesu2_mem_filter: String,
    yaesu2_mem_dirty: bool,
    yaesu2_mem_radio_received: bool,
    yaesu_mem_active_slot: u8,
    yaesu_menu_items: Vec<yaesu_menu::MenuItem>,
    yaesu_menu_received: bool,
    // Slot-1 (FTX-1) EX-menu (Fase C). Hiërarchisch: adres = "p1p2p3" (6 cijfers).
    // C1 = ruwe (adres,waarde)-lijst; C3 maakt er een P1>P2>P3 browser van.
    yaesu2_menu_entries: Vec<(String, String)>,
    yaesu2_menu_received: bool,
    collapse_yaesu2_menu: bool,
    /// Bewerkbare waarde-buffers per EX-adres (lazy gevuld bij render).
    yaesu2_menu_edits: std::collections::HashMap<String, String>,
    yaesu2_menu_filter: String,
    rotor_goto_input: String,
    // DX Cluster spots
    dx_spots: Vec<sdr_remote_logic::state::DxSpotInfo>,
    // Smooth tuning: display center interpolates toward VFO for smooth visual scroll
    smooth_display_center_hz: f64,   // RX1 smoothed display center
    rx2_smooth_display_center_hz: f64, // RX2 smoothed display center
    smooth_alpha: f64,               // shared smoothing alpha for current frame
    last_frame_time: Instant,
    // DX-cluster spot stream - data-saving toggle (Server-tab)
    dx_spots_enabled: bool,
    // RX2 / VFO-B
    rx2_enabled: bool,
    /// Opt-in: Thetis RX1/RX2/BinR audio in wideband Opus (16 kHz)
    /// i.p.v. narrowband (8 kHz). Default false; gestuurd naar server
    /// via `ControlId::ThetisWidebandAudio`. Zie memory
    /// `reference_thetis_audio_paths` voor wanneer welke pad welk
    /// sample-rate gebruikt.
    thetis_wideband_audio: bool,
    rx2_popout: bool,
    popout_joined: bool,
    popout_meter_analog: bool,
    ub_show_menu: bool,
    collapse_diversity: bool,
    collapse_yaesu_eq: bool,
    collapse_yaesu_memories: bool,
    collapse_yaesu_menu: bool,
    yaesu_memories_h: f32,
    /// Persisted main-window outer position; tracked during runtime so a
    /// next launch can re-apply it via `with_position()`. `None` until the
    /// first frame reads `viewport().outer_rect`.
    main_window_pos: Option<egui::Pos2>,
    /// Selected UI theme (switches the egui base visuals). Persisted as `theme=`.
    theme_variant: theme::ThemeVariant,
    /// Editable palette for the Custom theme. Persisted as `theme_custom=`.
    theme_custom: theme::Palette,
    /// Custom-palette edited this interaction; persist on pointer-release (avoids
    /// per-frame disk I/O while dragging the colour picker).
    theme_custom_dirty: bool,
    /// Last S-meter rects in popout viewports (screen coords) for A⇔B overlay
    popout_rx1_smeter_rect: egui::Rect,
    popout_rx2_smeter_rect: egui::Rect,
    rx2_volume: f32,
    rx2_af_gain_display: u8, // Thetis ZZLB value for display
    rx2_frequency_hz: u64,
    rx2_mode: u8,
    rx2_smeter: f32,
    rx2_smeter_peak: f32,
    rx2_smeter_peak_time: Instant,
    rx2_filter_low_hz: i32,
    rx2_filter_high_hz: i32,
    rx2_filter_changed_at: Option<Instant>,
    rx2_nr_level: u8,
    rx2_anf_on: bool,
    rx2_freq_step_index: usize,
    /// Inline freq-edit state voor RX2 - symmetrisch met `freq_editing` op RX1
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
    catsync_target: CatSyncTarget,
}


#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatSyncTarget {
    Thetis,
    Yaesu1,
    Yaesu2,
}

/// VRX channel id for the shared VRX controls renderer. VRX1 follows RX1/VFO-A,
/// VRX2 follows RX2/VFO-B. One renderer keeps both channels visually in parity.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum VrxChannel {
    Vrx1,
    Vrx2,
}
impl VrxChannel {
    fn id(self) -> u8 {
        match self {
            VrxChannel::Vrx1 => 0,
            VrxChannel::Vrx2 => 1,
        }
    }
    fn label(self) -> &'static str {
        match self {
            VrxChannel::Vrx1 => "VRX1",
            VrxChannel::Vrx2 => "VRX2",
        }
    }
}

impl SdrRemoteApp {

    fn vrx_send_enabled(&self, ch: VrxChannel, on: bool) -> bool {
        self.cmd_tx.send(match ch {
            VrxChannel::Vrx1 => Command::SetVrxEnabled(on),
            VrxChannel::Vrx2 => Command::SetVrx2Enabled(on),
        }).is_ok()
    }

    fn vrx_send_freq(&self, ch: VrxChannel, hz: u64) -> bool {
        self.cmd_tx.send(match ch {
            VrxChannel::Vrx1 => Command::SetVrxFrequency(hz),
            VrxChannel::Vrx2 => Command::SetVrx2Frequency(hz),
        }).is_ok()
    }

    fn vrx_send_mode(&self, ch: VrxChannel, mode: u8) -> bool {
        self.cmd_tx.send(match ch {
            VrxChannel::Vrx1 => Command::SetVrxMode(mode),
            VrxChannel::Vrx2 => Command::SetVrx2Mode(mode),
        }).is_ok()
    }

    fn vrx_send_volume(&self, ch: VrxChannel, v: f32) -> bool {
        self.cmd_tx.send(match ch {
            VrxChannel::Vrx1 => Command::SetVrxVolume(v),
            VrxChannel::Vrx2 => Command::SetVrx2Volume(v),
        }).is_ok()
    }

    /// EEn gedeelde renderer voor VRX1 EN VRX2 controls (parity door constructie,
    /// conform docs/internal/UI-STYLE-GUIDE.md). Top-row = status (mode amber / BW weak),
    /// bediening via gedeelde segmented selectors en theme-toggle/action-helpers met
    /// verplichte hover. Dispatch-discipline (Besluit 8) gelijk aan RX: controls zijn
    /// alleen actief wanneer verbonden, en lokale state wordt pas gemuteerd nadat het
    /// commando daadwerkelijk is verstuurd.
    fn render_vrx_channel_controls(
        &mut self,
        ui: &mut egui::Ui,
        ch: VrxChannel,
        ddc_center: u64,
        ddc_min: u64,
        ddc_max: u64,
    ) {
        let id = ch.id();
        let connected = self.connected;
        let enabled = match ch { VrxChannel::Vrx1 => self.vrx1_enabled, VrxChannel::Vrx2 => self.vrx2_enabled };
        let freq_hz = match ch { VrxChannel::Vrx1 => self.vrx1_freq_hz, VrxChannel::Vrx2 => self.vrx2_freq_hz };
        let mode = match ch { VrxChannel::Vrx1 => self.vrx1_mode, VrxChannel::Vrx2 => self.vrx2_mode };
        let filter_low = match ch { VrxChannel::Vrx1 => self.vrx1_filter_low_hz, VrxChannel::Vrx2 => self.vrx2_filter_low_hz };
        let filter_high = match ch { VrxChannel::Vrx1 => self.vrx1_filter_high_hz, VrxChannel::Vrx2 => self.vrx2_filter_high_hz };
        let mut volume = match ch { VrxChannel::Vrx1 => self.vrx1_volume, VrxChannel::Vrx2 => self.vrx2_volume };
        let high_res = match ch { VrxChannel::Vrx1 => self.vrx1_high_res_spectrum, VrxChannel::Vrx2 => self.vrx2_high_res_spectrum };
        let smeter = match ch { VrxChannel::Vrx1 => self.vrx1_smeter_dbm, VrxChannel::Vrx2 => self.vrx2_smeter_dbm };
        let smeter_peak = match ch { VrxChannel::Vrx1 => self.vrx1_smeter_peak, VrxChannel::Vrx2 => self.vrx2_smeter_peak };
        let source_freq = match ch { VrxChannel::Vrx1 => self.frequency_hz, VrxChannel::Vrx2 => self.rx2_frequency_hz };
        let cur_bw = (filter_high - filter_low).abs();
        let analog = self.popout_meter_analog;
        let (freq_prefix, en_hover, copy_hover, mode_hover, bw_hover, vol_hover) = match ch {
            VrxChannel::Vrx1 => ("A:", "Enable/disable VRX1 audio and spectrum.", "Copy VFO-A to VRX1.", "Set VRX1 demodulation mode.", "Set VRX1 audio filter bandwidth.", "Set VRX1 mix volume."),
            VrxChannel::Vrx2 => ("B:", "Enable/disable VRX2 audio and spectrum.", "Copy VFO-B to VRX2.", "Set VRX2 demodulation mode.", "Set VRX2 audio filter bandwidth.", "Set VRX2 mix volume."),
        };

        // Header: VRXn + enable toggle + DDC range (status, niet klikbaar)
        ui.horizontal(|ui| {
            ui.label(RichText::new(ch.label()).size(theme::TL_CHANNEL_HEADER_FONT).strong());
            if theme::tl_toggle_button(ui, "Enable", enabled, connected, theme::TL_SEGMENT_FONT, en_hover).clicked() {
                let now = !enabled;
                if self.vrx_send_enabled(ch, now) {
                    match ch { VrxChannel::Vrx1 => self.vrx1_enabled = now, VrxChannel::Vrx2 => self.vrx2_enabled = now };
                    let last_span = match ch { VrxChannel::Vrx1 => self.vrx1_high_res_last_span_khz, VrxChannel::Vrx2 => self.vrx2_high_res_last_span_khz };
                    let _ = self.cmd_tx.send(Command::SetVrxHighResSpectrum(id, now && high_res, last_span.max(1)));
                    if !now {
                        match ch { VrxChannel::Vrx1 => self.vrx1_extracted_bins.clear(), VrxChannel::Vrx2 => self.vrx2_extracted_bins.clear() };
                    }
                    self.save_full_config();
                }
            }
            ui.label(RichText::new(format!(
                "{} | DDC {:.3} MHz | range {:.3}-{:.3} MHz",
                band_label(ddc_center),
                ddc_center as f64 / 1_000_000.0,
                ddc_min as f64 / 1_000_000.0,
                ddc_max as f64 / 1_000_000.0,
            )).size(theme::TL_BW_STATUS_FONT).color(Color32::GRAY));
            // Oranje "buiten band"-waarschuwing: VRX-frequentie valt buiten het
            // huidige DDC-venster (RX staat op een andere band). VRX werkt dan
            // niet zinvol / zeer lage gevoeligheid. Amber = status (style guide).
            if freq_hz < ddc_min || freq_hz > ddc_max {
                ui.label(
                    RichText::new("OUT OF BAND")
                        .size(theme::TL_BW_STATUS_FONT)
                        .strong()
                        .color(theme::TL_AMBER_TEXT),
                )
                .on_hover_text(
                    "VRX frequency is outside the current receiver window (RX is on another band). Very low sensitivity until the RX band matches the VRX frequency.",
                );
            }
        });

        // Frequentie + mode/BW status + Copy VFO
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{}  ", freq_prefix)).size(theme::TL_FREQ_FONT).strong());
            if let Some(delta) = render_freq_scroll(ui, freq_hz) {
                let next = (freq_hz as i64 + delta).clamp(ddc_min as i64, ddc_max as i64) as u64;
                if connected && self.vrx_send_freq(ch, next) {
                    match ch { VrxChannel::Vrx1 => self.vrx1_freq_hz = next, VrxChannel::Vrx2 => self.vrx2_freq_hz = next };
                    self.save_full_config();
                }
            }
            ui.label(RichText::new(vrx_mode_label(mode)).size(theme::TL_MODE_STATUS_FONT).color(theme::TL_AMBER_TEXT));
            ui.label(RichText::new(format_bandwidth(cur_bw, false)).size(theme::TL_BW_STATUS_FONT).weak());
            if theme::tl_action_button(ui, "Copy VFO", connected, theme::TL_SEGMENT_FONT, copy_hover).clicked()
                && self.vrx_send_freq(ch, source_freq)
            {
                match ch { VrxChannel::Vrx1 => self.vrx1_freq_hz = source_freq, VrxChannel::Vrx2 => self.vrx2_freq_hz = source_freq };
                self.save_full_config();
            }
        });

        // S-meter (default 392x120 conform style guide)
        if analog {
            smeter_analog_sized(ui, smeter, smeter_peak, false, false, Some((392.0, 120.0)));
        } else {
            smeter_bar_popout(ui, smeter, smeter_peak, false, false, 100);
        }

        // Volume
        ui.horizontal(|ui| {
            ui.label("Volume:");
            let resp = ui.add_enabled(connected, egui::Slider::new(&mut volume, 0.001..=1.0)
                .logarithmic(true)
                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)))
                .on_hover_text(vol_hover);
            if resp.changed() && self.vrx_send_volume(ch, volume) {
                match ch { VrxChannel::Vrx1 => self.vrx1_volume = volume, VrxChannel::Vrx2 => self.vrx2_volume = volume };
                self.save_full_config();
            }
        });

        // Mode (gedeelde segmented selector; enige bediening)
        ui.horizontal(|ui| {
            ui.label("Mode:");
            if let Some(mode_val) = theme::tl_segmented_selector(
                ui,
                VRX_MODES.iter().map(|&(m, l)| (m, l.to_string())),
                mode, connected, theme::TL_SEGMENT_FONT, mode_hover,
            ) {
                if self.vrx_send_mode(ch, mode_val) {
                    match ch { VrxChannel::Vrx1 => self.vrx1_mode = mode_val, VrxChannel::Vrx2 => self.vrx2_mode = mode_val };
                    let (lo, hi) = vrx_mode_default_filter(mode_val, cur_bw);
                    // Default-filter bij deze mode: lokale filter-state + persist alleen
                    // committen na een bevestigde send (dispatch-return-discipline -
                    // geen UI/server-drift bij een gefaalde send).
                    if self.cmd_tx.send(Command::SetVrxFilter(id, lo, hi)).is_ok() {
                        match ch {
                            VrxChannel::Vrx1 => { self.vrx1_filter_low_hz = lo; self.vrx1_filter_high_hz = hi; }
                            VrxChannel::Vrx2 => { self.vrx2_filter_low_hz = lo; self.vrx2_filter_high_hz = hi; }
                        };
                        self.save_full_config();
                    }
                }
            }
        });

        // SAM auto-tune-to-carrier (alleen zinvol in SAM, mode 3)
        if mode == 3 {
            ui.horizontal(|ui| {
                let mut at = match ch { VrxChannel::Vrx1 => self.vrx1_auto_tune, VrxChannel::Vrx2 => self.vrx2_auto_tune };
                if ui.add_enabled(connected, egui::Checkbox::new(&mut at, "Auto-tune to carrier"))
                    .on_hover_text("SAM: follow the AM carrier - the VFO snaps onto the carrier so the filter stays symmetric around it. Locks within ±3 kHz.")
                    .changed()
                    && self.cmd_tx.send(Command::SetVrxAutoTune(id, at)).is_ok()
                {
                    match ch { VrxChannel::Vrx1 => self.vrx1_auto_tune = at, VrxChannel::Vrx2 => self.vrx2_auto_tune = at };
                }
            });
        }

        // BW (gedeelde segmented selector; mode-afhankelijke presets)
        ui.horizontal(|ui| {
            ui.label("BW:");
            if let Some(p) = theme::tl_segmented_selector(
                ui,
                vrx_filter_presets(mode).iter().map(|&p| (p, format_bandwidth(p, false))),
                cur_bw, connected, theme::TL_SEGMENT_FONT, bw_hover,
            ) {
                let (lo, hi) = vrx_filter_from_preset(mode, p);
                if self.cmd_tx.send(Command::SetVrxFilter(id, lo, hi)).is_ok() {
                    match ch {
                        VrxChannel::Vrx1 => { self.vrx1_filter_low_hz = lo; self.vrx1_filter_high_hz = hi; }
                        VrxChannel::Vrx2 => { self.vrx2_filter_low_hz = lo; self.vrx2_filter_high_hz = hi; }
                    };
                    self.save_full_config();
                }
            }
        });

        // High-res spectrum toggle (server-side extracted view)
        ui.horizontal(|ui| {
            let mut hr = high_res;
            if ui.add_enabled(connected, egui::Checkbox::new(&mut hr, "High-res spectrum"))
                .on_hover_text("Request a server-extracted high-resolution VRX spectrum. Uses extra bandwidth.")
                .changed()
            {
                let (source_span, zoom) = match ch {
                    VrxChannel::Vrx1 => (self.full_spectrum_span_hz, self.vrx1_spectrum_zoom),
                    VrxChannel::Vrx2 => (self.rx2_full_spectrum_span_hz, self.vrx2_spectrum_zoom),
                };
                let span_hz = (source_span as f32 / zoom.max(1.0)) as u32;
                let span_khz = ((span_hz / 1000).max(1)) as u16;
                if self.cmd_tx.send(Command::SetVrxHighResSpectrum(id, enabled && hr, span_khz)).is_ok() {
                    match ch { VrxChannel::Vrx1 => self.vrx1_high_res_spectrum = hr, VrxChannel::Vrx2 => self.vrx2_high_res_spectrum = hr };
                    match ch { VrxChannel::Vrx1 => self.vrx1_high_res_last_span_khz = span_khz, VrxChannel::Vrx2 => self.vrx2_high_res_last_span_khz = span_khz };
                    if !hr {
                        match ch { VrxChannel::Vrx1 => self.vrx1_extracted_bins.clear(), VrxChannel::Vrx2 => self.vrx2_extracted_bins.clear() };
                    }
                    self.save_full_config();
                }
            }
        });
    }
    /// Gedeelde renderer voor EEN VRX-spectrum-paneel (VRX1 of VRX2): de
    /// Ref/Auto/Range- en Zoom/Pan/WF-knoprijen + de spectrum/waterval-strip.
    /// Parity door constructie: VRX1 en VRX2 delen exact deze code (conform
    /// docs/internal/UI-STYLE-GUIDE.md). Spectrum-display-instellingen zijn lokaal
    /// (geen server-command); click-to-tune volgt de dispatch-discipline (Besluit 8).
    fn render_vrx_spectrum_panel(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        ch: VrxChannel,
        vrx_min: u64,
        vrx_max: u64,
    ) {
        let connected = self.connected;
        let rx_label = match ch { VrxChannel::Vrx1 => "RX1", VrxChannel::Vrx2 => "RX2" };
        let allow_zoom_below_2x = self.allow_zoom_below_2x;
        let fft_size_k = self.spectrum_fft_size_k;
        // Geen ui.group()-frame: RX-spectrumpanelen hebben er ook geen (parity).
        {
            // Row 1: Ref + Auto + Range
            ui.horizontal(|ui| {
                ui.spacing_mut().slider_width = theme::TL_SLIDER_WIDTH;
                ui.label("Ref:");
                let mut changed = false;
                let auto = match ch { VrxChannel::Vrx1 => self.vrx1_auto_ref, VrxChannel::Vrx2 => self.vrx2_auto_ref };
                if auto {
                    let mut disp = match ch { VrxChannel::Vrx1 => self.vrx1_ref_db, VrxChannel::Vrx2 => self.vrx2_ref_db };
                    ui.add_enabled(false, egui::Slider::new(&mut disp, -90.0..=0.0).suffix(" dB").step_by(5.0))
                        .on_hover_text("Spectrum reference level in dB.");
                } else if ui.add(egui::Slider::new(
                        match ch { VrxChannel::Vrx1 => &mut self.vrx1_ref_db, VrxChannel::Vrx2 => &mut self.vrx2_ref_db },
                        -90.0..=0.0).suffix(" dB").step_by(5.0))
                    .on_hover_text("Spectrum reference level in dB.")
                    .changed()
                {
                    changed = true;
                }
                if ui.checkbox(
                        match ch { VrxChannel::Vrx1 => &mut self.vrx1_auto_ref, VrxChannel::Vrx2 => &mut self.vrx2_auto_ref },
                        "Auto")
                    .on_hover_text("Automatically track the reference level.")
                    .changed()
                {
                    match ch {
                        VrxChannel::Vrx1 => if self.vrx1_auto_ref { self.vrx1_auto_ref_frames = 0; self.vrx1_auto_ref_initialized = false; },
                        VrxChannel::Vrx2 => if self.vrx2_auto_ref { self.vrx2_auto_ref_frames = 0; self.vrx2_auto_ref_initialized = false; },
                    }
                    self.save_full_config();
                }
                ui.label("Range:");
                if ui.add(egui::Slider::new(
                        match ch { VrxChannel::Vrx1 => &mut self.vrx1_range_db, VrxChannel::Vrx2 => &mut self.vrx2_range_db },
                        20.0..=130.0).suffix(" dB").step_by(5.0))
                    .on_hover_text("Spectrum vertical range in dB.")
                    .changed()
                {
                    match ch {
                        VrxChannel::Vrx1 => if self.vrx1_auto_ref { self.vrx1_auto_ref_frames = 0; self.vrx1_auto_ref_initialized = false; },
                        VrxChannel::Vrx2 => if self.vrx2_auto_ref { self.vrx2_auto_ref_frames = 0; self.vrx2_auto_ref_initialized = false; },
                    }
                    changed = true;
                }
                if changed { self.save_full_config(); }
            });
            // Row 2: Zoom + Allow<2x (disabled stub) + Pan + WF + FFT (disabled stub)
            ui.horizontal(|ui| {
                ui.spacing_mut().slider_width = theme::TL_SLIDER_WIDTH;
                let mut changed = false;
                ui.label("Zoom:");
                let zoom_min: f32 = if allow_zoom_below_2x { 1.0 } else { 2.0 };
                match ch {
                    VrxChannel::Vrx1 => if self.vrx1_spectrum_zoom < zoom_min { self.vrx1_spectrum_zoom = zoom_min; },
                    VrxChannel::Vrx2 => if self.vrx2_spectrum_zoom < zoom_min { self.vrx2_spectrum_zoom = zoom_min; },
                }
                if ui.add(egui::Slider::new(
                        match ch { VrxChannel::Vrx1 => &mut self.vrx1_spectrum_zoom, VrxChannel::Vrx2 => &mut self.vrx2_spectrum_zoom },
                        zoom_min..=1024.0).logarithmic(true).custom_formatter(|v, _| format!("{:.0}x", v)))
                    .on_hover_text("Spectrum zoom factor.")
                    .changed()
                {
                    match ch {
                        VrxChannel::Vrx1 => { let mp = (0.5 - 0.5 / self.vrx1_spectrum_zoom) * 0.05; self.vrx1_pan = self.vrx1_pan.clamp(-mp, mp); self.vrx1_zoom_initialized = true; }
                        VrxChannel::Vrx2 => { let mp = (0.5 - 0.5 / self.vrx2_spectrum_zoom) * 0.05; self.vrx2_pan = self.vrx2_pan.clamp(-mp, mp); self.vrx2_zoom_initialized = true; }
                    }
                    changed = true;
                }
                let mut allow_2x = allow_zoom_below_2x;
                ui.add_enabled(false, egui::Checkbox::new(&mut allow_2x, "Allow zoom <2x"))
                    .on_disabled_hover_text(format!("Server-wide setting - manage via {} spectrum panel.", rx_label));
                ui.label("Pan:");
                let zoom_now = match ch { VrxChannel::Vrx1 => self.vrx1_spectrum_zoom, VrxChannel::Vrx2 => self.vrx2_spectrum_zoom };
                let max_pan = if zoom_now > 1.01 { (0.5 - 0.5 / zoom_now) * 0.05 } else { 0.0 };
                changed |= ui.add(egui::Slider::new(
                        match ch { VrxChannel::Vrx1 => &mut self.vrx1_pan, VrxChannel::Vrx2 => &mut self.vrx2_pan },
                        -max_pan..=max_pan).custom_formatter(|v, _| format!("{:+.2}", v)))
                    .on_hover_text("Pan inside the zoomed spectrum view.")
                    .changed();
                ui.label("WF:");
                changed |= ui.add(egui::Slider::new(
                        match ch { VrxChannel::Vrx1 => &mut self.vrx1_wf_contrast, VrxChannel::Vrx2 => &mut self.vrx2_wf_contrast },
                        0.3..=3.0).logarithmic(true).custom_formatter(|v, _| format!("{:.1}", v)))
                    .on_hover_text("Waterfall contrast.")
                    .changed();
                let ddc_sr = match ch { VrxChannel::Vrx1 => self.ddc_sample_rate_rx1, VrxChannel::Vrx2 => self.ddc_sample_rate_rx2 };
                let ddc_rate = if ddc_sr > 0 { ddc_sr as u32 * 1000 } else { 384_000 };
                let auto_k = sdr_remote_core::ddc_fft_size(ddc_rate) / 1024;
                let fft_label = if fft_size_k == 0 { format!("FFT: Auto ({}K)", auto_k) } else { format!("FFT: {}K", fft_size_k) };
                ui.add_enabled(false, egui::Button::new(&fft_label))
                    .on_disabled_hover_text(format!("Server-wide setting - manage via {} spectrum panel.", rx_label));
                if changed { self.save_full_config(); }
            });
            ui.separator();
            let remaining = ui.available_height();
            // Plot en waterval even hoog (operator-wens); 16 px = de label-strip in het
            // spectrum, TL_INNER_GAP_Y = ruimte tussen plot en waterval.
            let spec_h = ((remaining - 16.0 - theme::TL_INNER_GAP_Y) / 2.0).max(40.0);
            let wf_h = spec_h;
            let new_freq = match ch {
                VrxChannel::Vrx1 => {
                    let extracted = self.vrx1_high_res_spectrum && !self.vrx1_extracted_bins.is_empty() && self.vrx1_extracted_span_hz > 0;
                    let (b, c, s): (&[u16], u32, u32) = if extracted {
                        (&self.vrx1_extracted_bins, self.vrx1_extracted_center_hz, self.vrx1_extracted_span_hz)
                    } else {
                        (&self.full_spectrum_bins, self.full_spectrum_center_hz, self.full_spectrum_span_hz)
                    };
                    spectrum::render_vrx_strip(
                        ui, ctx, "vrx1",
                        self.vrx1_freq_hz, self.vrx1_spectrum_zoom, self.vrx1_pan,
                        self.vrx1_ref_db, self.vrx1_range_db, self.vrx1_wf_contrast,
                        spec_h, wf_h, self.vrx1_mode == 1,
                        self.vrx1_filter_low_hz, self.vrx1_filter_high_hz,
                        self.vrx1_smeter_dbm, self.vrx1_enabled,
                        b, c, s, self.full_spectrum_span_hz, extracted,
                        if extracted { &self.vrx1_hr_waterfall } else { &self.waterfall },
                        &mut self.vrx1_waterfall_texture, vrx_min, vrx_max,
                    )
                }
                VrxChannel::Vrx2 => {
                    let extracted = self.vrx2_high_res_spectrum && !self.vrx2_extracted_bins.is_empty() && self.vrx2_extracted_span_hz > 0;
                    let (b, c, s): (&[u16], u32, u32) = if extracted {
                        (&self.vrx2_extracted_bins, self.vrx2_extracted_center_hz, self.vrx2_extracted_span_hz)
                    } else {
                        (&self.rx2_full_spectrum_bins, self.rx2_full_spectrum_center_hz, self.rx2_full_spectrum_span_hz)
                    };
                    spectrum::render_vrx_strip(
                        ui, ctx, "vrx2",
                        self.vrx2_freq_hz, self.vrx2_spectrum_zoom, self.vrx2_pan,
                        self.vrx2_ref_db, self.vrx2_range_db, self.vrx2_wf_contrast,
                        spec_h, wf_h, self.vrx2_mode == 1,
                        self.vrx2_filter_low_hz, self.vrx2_filter_high_hz,
                        self.vrx2_smeter_dbm, self.vrx2_enabled,
                        b, c, s, self.rx2_full_spectrum_span_hz, extracted,
                        if extracted { &self.vrx2_hr_waterfall } else { &self.rx2_waterfall },
                        &mut self.vrx2_waterfall_texture, vrx_min, vrx_max,
                    )
                }
            };
            if let Some(f) = new_freq {
                if connected && self.vrx_send_freq(ch, f) {
                    match ch { VrxChannel::Vrx1 => self.vrx1_freq_hz = f, VrxChannel::Vrx2 => self.vrx2_freq_hz = f };
                    self.save_full_config();
                }
            }
        }
    }


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
    fn set_pending_yaesu_freq(&mut self, slot: u8, freq: u64) {
        let now = Instant::now();
        if slot == 0 {
            self.yaesu_freq_a = freq;
            self.yaesu_pending_freq = Some(freq);
            self.yaesu_pending_freq_at = Some(now);
        } else {
            self.yaesu2_freq_a = freq;
            self.yaesu2_pending_freq = Some(freq);
            self.yaesu2_pending_freq_at = Some(now);
        }
    }

    fn accept_yaesu_freq(
        current: &mut u64,
        pending: &mut Option<u64>,
        pending_at: &mut Option<Instant>,
        state_freq: u64,
    ) {
        if let Some(target) = *pending {
            let age_ms = pending_at.map_or(u128::MAX, |t| t.elapsed().as_millis());
            if age_ms >= 120 {
                let delta = (state_freq as i64 - target as i64).unsigned_abs();
                if delta == 0 || age_ms > 1000 {
                    *pending = None;
                    *pending_at = None;
                }
            }
        }
        if pending.is_none() {
            *current = state_freq;
        }
    }

    pub fn new(
        state_rx: watch::Receiver<RadioState>,
        cmd_tx: mpsc::UnboundedSender<Command>,
        log_buffer: LogBuffer,
        // Phase C: als de relay als transport wordt gebruikt draait de monitor in
        // main.rs; dan tonen we alleen zijn status (geen tweede relay-verbinding).
        external_relay_status: Option<sdr_remote_relay::RelayStatusHandle>,
    ) -> Self {
        let config = load_config();

        let input_devices = crate::audio::list_input_devices();
        let output_devices = crate::audio::list_output_devices();
        let relay_initial = sdr_remote_relay::RelayConfig {
            enabled: config.relay_enabled,
            url: config.relay_url.clone(),
            station: config.relay_station.clone(),
            token: config.relay_token.clone(),
            role: sdr_remote_relay::RelayRole::Client,
            instance: config.relay_instance_id.clone(),
            name: config.relay_device_name.clone(),
            udp_port: sdr_remote_relay::relay_udp_port_resolve(config.relay_udp_enabled),
        };
        let (relay_monitor, relay_status, relay_external) = if let Some(ext) = external_relay_status {
            // Relay-transport: monitor draait in main.rs. Toon alleen status; geen
            // eigen monitor (er mag maar een relay-verbinding per client zijn).
            (None, Some(ext), true)
        } else if relay_initial.enabled {
            let m = sdr_remote_relay::RelayMonitor::start_threaded(relay_initial);
            let s = m.status_handle();
            (Some(m), Some(s), false)
        } else {
            (None, None, false)
        };

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
        let _ = cmd_tx.send(Command::SetThetisWidebandAudio(config.thetis_wideband_audio));
        // Restore EQ + mic gain from active profile (radio 1)
        if let Some((_, en, gains, mic_gain)) = config.yaesu_eq_profiles.iter()
            .find(|(n, _, _, _)| n == &config.yaesu_eq_active) {
            let _ = cmd_tx.send(Command::SetYaesuEqEnabled(*en));
            for i in 0..5 { let _ = cmd_tx.send(Command::SetYaesuEqBand(i as u8, gains[i])); }
            let _ = cmd_tx.send(Command::SetYaesuTxGain(*mic_gain));
        }
        // Idem radio 2 (FTX-1): engine krijgt de geladen EQ + mic-gain.
        if let Some((_, en, gains, mic_gain)) = config.yaesu2_eq_profiles.iter()
            .find(|(n, _, _, _)| n == &config.yaesu2_eq_active) {
            let _ = cmd_tx.send(Command::SetYaesu2EqEnabled(*en));
            for i in 0..5 { let _ = cmd_tx.send(Command::SetYaesu2EqBand(i as u8, gains[i])); }
            let _ = cmd_tx.send(Command::SetYaesu2TxGain(*mic_gain));
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
            server_input: config.server.clone(),
            password_input: config.password.clone(),
            totp_input: String::new(),
            mouse_ptt: false,
            ptt_toggle_mode: config.ptt_toggle,
            yaesu_ptt_toggle_mode: config.yaesu_ptt_toggle,
            yaesu_mouse_ptt: false,
            spike_protection: config.spike_protection,
            mic_gate_delay_thetis_ms: config.mic_gate_delay_thetis_ms,
            mic_gate_delay_yaesu_ms: config.mic_gate_delay_yaesu_ms,
            recording: false,
            playing: false,
            rec_rx1: true,
            rec_rx2: false,
            rec_yaesu: false,
            rec_yaesu2: false,
            rec_vrx1: false,
            rec_vrx2: false,
            last_recorded_path: None,
            midi_ptt_toggle_mode: config.midi_ptt_toggle,
            smeter_source: config.smeter_source,
            allow_zoom_below_2x: config.allow_zoom_below_2x,
            ui_language: config.language.clone(),
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
            yaesu_audio_packets: 0,
            yaesu_jitter_ms: 0.0,
            yaesu_buffer_depth: 0,
            yaesu2_audio_packets: 0,
            yaesu2_jitter_ms: 0.0,
            yaesu2_buffer_depth: 0,
            vrx1_audio_packets: 0,
            vrx1_jitter_ms: 0.0,
            vrx1_buffer_depth: 0,
            vrx2_audio_packets: 0,
            vrx2_jitter_ms: 0.0,
            vrx2_buffer_depth: 0,
            down_kbps: 0,
            up_kbps: 0,
            bw_breakdown: Vec::new(),
            bw_breakdown_expanded: false,
            loss_percent: 0,
            capture_level: 0.0,
            playback_level: 0.0,
            playback_level_bin_r: 0.0,
            playback_level_rx2: 0.0,
            playback_level_yaesu: 0.0,
            playback_level_yaesu2: 0.0,
            yaesu_mic_level: 0.0,
            frequency_hz: 0,
            mode: 0,
            smeter: sdr_remote_logic::state::SMETER_NO_DATA_DBM,
            smeter_peak: sdr_remote_logic::state::SMETER_NO_DATA_DBM,
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
            tx_filter_follow_rx: true,
            tx_filter_low_hz: 0,
            tx_filter_high_hz: 0,
            tx_filter_supported: false,
            tx_filter_initialized: false,
            last_tx_follow_sent: None,
            tx_follow_last_send_at: None,
            thetis_configured: true,
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
            yaesu_pending_freq: None,
            yaesu_pending_freq_at: None,
            yaesu2_pending_freq: None,
            yaesu2_pending_freq_at: None,
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
            spectrum_total_h: config.spectrum_total_h,
            spectrum_popout: config.spectrum_popout,
            window_w: config.window_w,
            window_h: config.window_h,
            main_geom_dirty: false,
            log_buffer,
            show_log: false,
            show_about: false,
            show_vrx: false,
            vrx1_enabled: config.vrx1_enabled.unwrap_or(false),
            vrx1_freq_hz: config.vrx1_freq_hz.unwrap_or(0),
            vrx1_mode: config.vrx1_mode.unwrap_or(0),
            vrx1_volume: config.vrx1_volume.unwrap_or(1.0),
            vrx2_enabled: config.vrx2_enabled.unwrap_or(false),
            vrx2_freq_hz: config.vrx2_freq_hz.unwrap_or(0),
            vrx2_mode: config.vrx2_mode.unwrap_or(0),
            vrx2_volume: config.vrx2_volume.unwrap_or(1.0),
            vrx1_auto_tune: false,
            vrx2_auto_tune: false,
            vrx_rate_mode: 2, // default Auto
            vrx_rate_mode2: 2, // default Auto (VRX2, per-client per-VRX rate)
            last_vrx1_autotune_hz: 0,
            last_vrx2_autotune_hz: 0,
            vrx_popout_pos: config.vrx_popout_pos.map(|(x, y)| egui::pos2(x, y)),
            vrx_popout_size: config.vrx_popout_size.map(|(w, h)| egui::vec2(w, h)),
            vrx_popout_init_applied: false,
            playback_level_vrx1: 0.0,
            playback_level_vrx2: 0.0,
            vrx1_freq_by_bucket: std::collections::HashMap::new(),
            vrx2_freq_by_bucket: std::collections::HashMap::new(),
            last_vrx1_ddc_center_hz: 0,
            last_vrx2_ddc_center_hz: 0,
            vrx1_spectrum_zoom: config.vrx1_spectrum_zoom.unwrap_or(32.0),
            vrx2_spectrum_zoom: config.vrx2_spectrum_zoom.unwrap_or(32.0),
            vrx1_ref_db: config.vrx1_ref_db.unwrap_or(-20.0),
            vrx1_range_db: config.vrx1_range_db.unwrap_or(100.0),
            vrx1_wf_contrast: config.vrx1_wf_contrast.unwrap_or(1.0),
            vrx1_pan: config.vrx1_pan.unwrap_or(0.0),
            vrx1_auto_ref: config.vrx1_auto_ref.unwrap_or(false),
            // If user has persisted a zoom we trust it; otherwise let
            // first DDC-span arrival pick default_zoom_for_span.
            vrx1_zoom_initialized: config.vrx1_spectrum_zoom.is_some(),
            vrx1_filter_low_hz: config.vrx1_filter_low_hz.unwrap_or_else(|| {
                if config.vrx1_mode.unwrap_or(0) == 1 { -3000 } else { 0 }
            }),
            vrx1_filter_high_hz: config.vrx1_filter_high_hz.unwrap_or_else(|| {
                if config.vrx1_mode.unwrap_or(0) == 1 { 0 } else { 3000 }
            }),
            vrx1_high_res_spectrum: config.vrx1_high_res_spectrum.unwrap_or(false),
            vrx1_extracted_bins: Vec::new(),
            vrx1_extracted_center_hz: 0,
            vrx1_extracted_span_hz: 0,
            vrx1_extracted_sequence: 0,
            vrx1_high_res_last_span_khz: 0,
            vrx1_hr_waterfall: WaterfallRingBuffer::new(200),
            vrx1_auto_ref_value: -20.0,
            vrx1_auto_ref_frames: 0,
            vrx1_auto_ref_initialized: false,
            vrx1_smeter_dbm: -127.0,
            vrx1_smeter_initialized: false,
            vrx1_smeter_peak: -127.0,
            vrx1_smeter_peak_time: Instant::now(),
            vrx2_ref_db: config.vrx2_ref_db.unwrap_or(-20.0),
            vrx2_range_db: config.vrx2_range_db.unwrap_or(100.0),
            vrx2_wf_contrast: config.vrx2_wf_contrast.unwrap_or(1.0),
            vrx2_pan: config.vrx2_pan.unwrap_or(0.0),
            vrx2_auto_ref: config.vrx2_auto_ref.unwrap_or(false),
            vrx2_zoom_initialized: config.vrx2_spectrum_zoom.is_some(),
            vrx2_filter_low_hz: config.vrx2_filter_low_hz.unwrap_or_else(|| {
                if config.vrx2_mode.unwrap_or(0) == 1 { -3000 } else { 0 }
            }),
            vrx2_filter_high_hz: config.vrx2_filter_high_hz.unwrap_or_else(|| {
                if config.vrx2_mode.unwrap_or(0) == 1 { 0 } else { 3000 }
            }),
            vrx2_high_res_spectrum: config.vrx2_high_res_spectrum.unwrap_or(false),
            vrx2_extracted_bins: Vec::new(),
            vrx2_extracted_center_hz: 0,
            vrx2_extracted_span_hz: 0,
            vrx2_extracted_sequence: 0,
            vrx2_high_res_last_span_khz: 0,
            vrx2_hr_waterfall: WaterfallRingBuffer::new(200),
            vrx2_auto_ref_value: -20.0,
            vrx2_auto_ref_frames: 0,
            vrx2_auto_ref_initialized: false,
            vrx2_smeter_dbm: -127.0,
            vrx2_smeter_initialized: false,
            vrx2_smeter_peak: -127.0,
            vrx2_smeter_peak_time: Instant::now(),
            vrx1_waterfall_texture: None,
            vrx2_waterfall_texture: None,
            vrx_state_sync_pending: true,
            active_tab: Tab::Radio,
            last_connect_status: sdr_remote_logic::state::ConnectStatus::Disconnected,
            // PATCH-3: kick off the mDNS browse on app start. Daemon-init failure
            // is caught inside `BrowseHandle::start` and surfaces as an empty
            // dropdown - manual IP entry stays available.
            mdns_browse: Some(crate::mdns::BrowseHandle::start()),
            relay_enabled: config.relay_enabled,
            relay_url: config.relay_url.clone(),
            relay_station: config.relay_station.clone(),
            relay_token: config.relay_token.clone(),
            relay_instance_id: config.relay_instance_id.clone(),
            relay_device_name: config.relay_device_name.clone(),
            relay_udp_enabled: config.relay_udp_enabled,
            relay_monitor,
            relay_status,
            relay_external,
            // PATCH-4: arm the first-run wizard if the user has never had a
            // successful connect (counter==0). Seeds with whatever
            // server/password the config already has - wizard still walks
            // through the steps but the fields are pre-populated.
            wizard_state: if crate::ui::config::is_first_run() {
                Some(wizard::WizardState::new(
                    config.server.clone(),
                    config.password.clone(),
                ))
            } else {
                None
            },
            device_tab: config.device_tab,
            amplitec_available: false,
            amplitec_connected: false,
            amplitec_switch_a: 0,
            amplitec_switch_b: 0,
            amplitec_labels: String::new(),
            amplitec_log: VecDeque::new(),
            amplitec_power_max_w: [0; 6],
            amplitec_power_tx_blocked: [false; 6],
            amplitec_power_loaded: false,
            amplitec_power_edit_max_w: [0; 6],
            amplitec_power_edit_tx_blocked: [false; 6],
            amplitec_power_show: false,
            websdr_favorite_editing: None,
            tuner_available: false,
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
            ub_operation: 0,
            ub_freq_min_mhz: 0,
            ub_freq_max_mhz: 0,
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
            yaesu_smeter_peak: 0,
            yaesu_smeter_peak_time: Instant::now(),
            yaesu_tx_active: false,
            yaesu_power_on: false,
            yaesu_volume: config.yaesu_volume,
            yaesu_model: 0,
            yaesu2_model: 1,
            yaesu2_connected: false,
            yaesu2_freq_a: 0,
            yaesu2_freq_b: 0,
            yaesu2_mode: 1,
            yaesu2_smeter: 0,
            yaesu2_smeter_peak: 0,
            yaesu2_smeter_peak_time: Instant::now(),
            yaesu2_tx_active: false,
            yaesu2_power_on: false,
            yaesu2_split: false,
            yaesu2_scan: false,
            yaesu2_vfo_select: 0,
            yaesu2_memory_channel: 0,
            yaesu2_tuner_state: 0,
            yaesu2_feature_toggles: 0,
            yaesu2_feature_levels: [0u8; 16],
            yaesu2_squelch: 0,
            yaesu2_rf_gain: 0,
            yaesu2_rf_power: 0,
            // Standalone persistente mic-gain (laatste schuif-waarde), los van
            // het EQ-profiel zodat de waarde altijd onthouden wordt.
            yaesu2_mic_gain: config.yaesu2_mic_gain,
            yaesu2_eq_enabled: config.yaesu2_eq_profiles.iter()
                .find(|(n, _, _, _)| n == &config.yaesu2_eq_active)
                .map(|(_, e, _, _)| *e).unwrap_or(false),
            yaesu2_eq_gains: config.yaesu2_eq_profiles.iter()
                .find(|(n, _, _, _)| n == &config.yaesu2_eq_active)
                .map(|(_, _, g, _)| *g).unwrap_or([0.0; 5]),
            yaesu2_eq_profiles: config.yaesu2_eq_profiles.clone(),
            yaesu2_eq_active_profile: config.yaesu2_eq_active.clone(),
            yaesu2_eq_new_name: String::new(),
            collapse_yaesu2_eq: false,
            collapse_yaesu2_memories: false,
            yaesu2_control_changed_at: None,
            yaesu2_volume: config.yaesu2_volume,
            yaesu2_enabled: config.yaesu2_enabled,
            yaesu2_ptt_toggle_mode: config.yaesu2_ptt_toggle,
            yaesu2_enable_sent: false,
            yaesu2_autoread_at: None,
            yaesu2_hf_swap_at: None,
            yaesu2_popout: config.yaesu2_popout,
            yaesu2_popout_pos: config.yaesu2_popout_pos.map(|(x, y)| egui::pos2(x, y)),
            yaesu2_popout_size: config.yaesu2_popout_size.map(|(w, h)| egui::vec2(w, h)),
            yaesu2_popout_init_applied: false,
            yaesu2_mouse_ptt: false,
            yaesu_popout: config.yaesu_popout,
            yaesu_popout_pos: config.yaesu_popout_pos.map(|(x, y)| egui::pos2(x, y)),
            yaesu_popout_size: config.yaesu_popout_size.map(|(w, h)| egui::vec2(w, h)),
            spectrum_popout_pos: config.spectrum_popout_pos.map(|(x, y)| egui::pos2(x, y)),
            spectrum_popout_size: config.spectrum_popout_size.map(|(w, h)| egui::vec2(w, h)),
            rx2_popout_pos: config.rx2_popout_pos.map(|(x, y)| egui::pos2(x, y)),
            rx2_popout_size: config.rx2_popout_size.map(|(w, h)| egui::vec2(w, h)),
            popout_joined_pos: config.popout_joined_pos.map(|(x, y)| egui::pos2(x, y)),
            popout_joined_size: config.popout_joined_size.map(|(w, h)| egui::vec2(w, h)),
            spectrum_popout_init_applied: false,
            rx2_popout_init_applied: false,
            popout_joined_init_applied: false,
            yaesu_popout_init_applied: false,
            yaesu_popout_first_frame: true,
            yaesu_enable_sent: false,
            // Standalone persistente mic-gain (laatste schuif-waarde), los van
            // het EQ-profiel zodat de waarde altijd onthouden wordt.
            yaesu_mic_gain: config.yaesu_mic_gain,
            yaesu_compressor: config.yaesu_compressor,
            yaesu2_compressor: config.yaesu2_compressor,
            yaesu_tx_agc: config.yaesu_tx_agc,
            yaesu2_tx_agc: config.yaesu2_tx_agc,
            yaesu_eq_enabled: {
                // Load active EQ profile from config
                config.yaesu_eq_profiles.iter()
                    .find(|(n, _, _, _)| n == &config.yaesu_eq_active)
                    .map(|(_, e, _, _)| *e).unwrap_or(false)
            },
            yaesu_eq_gains: {
                config.yaesu_eq_profiles.iter()
                    .find(|(n, _, _, _)| n == &config.yaesu_eq_active)
                    .map(|(_, _, g, _)| *g).unwrap_or([0.0; 5])
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
            yaesu_tuner_state: 0,
            yaesu_feature_toggles: 0,
            yaesu_feature_levels: [0u8; 16],
            yaesu_level_sliders: [[0i32; 4]; 2],
            yaesu_freq_sliders: [[0i32; 3]; 2],
            yaesu_clar_offset: 0,
            yaesu2_clar_offset: 0,
            tune_step_hz: 1_000,
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
            yaesu2_mem_channels: Vec::new(),
            yaesu2_mem_file: "ftx1_memories.tab".to_string(),
            yaesu2_mem_selected: None,
            yaesu2_mem_filter: String::new(),
            yaesu2_mem_dirty: false,
            yaesu2_mem_radio_received: false,
            yaesu_mem_active_slot: 0,
            yaesu_menu_items: Vec::new(),
            yaesu_menu_received: false,
            yaesu2_menu_entries: Vec::new(),
            yaesu2_menu_received: false,
            collapse_yaesu2_menu: false,
            yaesu2_menu_edits: std::collections::HashMap::new(),
            yaesu2_menu_filter: String::new(),
            rotor_goto_input: String::new(),
            dx_spots: Vec::new(),
            smooth_display_center_hz: 0.0,
            rx2_smooth_display_center_hz: 0.0,
            smooth_alpha: 1.0,
            last_frame_time: Instant::now(),
            dx_spots_enabled: true,
            rx2_enabled: config.rx2_enabled,
            thetis_wideband_audio: config.thetis_wideband_audio,
            rx2_popout: config.rx2_popout,
            popout_joined: config.popout_joined,
            popout_meter_analog: config.popout_meter_analog,
            ub_show_menu: config.ub_show_menu,
            collapse_diversity: config.collapse_diversity,
            collapse_yaesu_eq: config.collapse_yaesu_eq,
            collapse_yaesu_memories: config.collapse_yaesu_memories,
            collapse_yaesu_menu: config.collapse_yaesu_menu,
            yaesu_memories_h: config.yaesu_memories_h,
            main_window_pos: config.main_window_pos.map(|(x, y)| egui::pos2(x, y)),
            theme_variant: theme::ThemeVariant::from_str(&config.theme),
            theme_custom: theme::Palette::from_config_string(&config.theme_custom)
                .unwrap_or_else(theme::Palette::slate),
            theme_custom_dirty: false,
            popout_rx1_smeter_rect: egui::Rect::NOTHING,
            popout_rx2_smeter_rect: egui::Rect::NOTHING,
            rx2_volume: config.rx2_volume,
            rx2_af_gain_display: 0,
            rx2_frequency_hz: 0,
            rx2_mode: 0,
            rx2_smeter: sdr_remote_logic::state::SMETER_NO_DATA_DBM,
            rx2_smeter_peak: sdr_remote_logic::state::SMETER_NO_DATA_DBM,
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
            catsync_target: CatSyncTarget::Thetis,
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

    /// Apply persisted popout geometry to a ViewportBuilder. Both
    /// `with_position()` AND `with_inner_size()` are only included on the
    /// first frame after the popout opens. Subsequent frames omit both so
    /// the OS keeps the window wherever the user dragged or resized it -
    /// without that gating the OS gets a fresh "go to this rect" request
    /// every frame and the window oscillates between successive committed
    /// rects during an active move / resize gesture.
    /// Scale factor (physical px per logical point) of the main viewport, used
    /// to compare saved logical positions against OS monitor rects (physical).
    fn viewport_native_ppp(ctx: &egui::Context) -> f32 {
        ctx.input(|i| i.viewport().native_pixels_per_point)
            .unwrap_or_else(|| ctx.pixels_per_point())
    }

    fn apply_popout_geometry(
        builder: ViewportBuilder,
        pos: Option<egui::Pos2>,
        size: Option<egui::Vec2>,
        default_size: [f32; 2],
        native_ppp: f32,
        init_applied: &mut bool,
    ) -> ViewportBuilder {
        let mut b = builder;
        if !*init_applied {
            let sz = size.map(|v| [v.x, v.y]).unwrap_or(default_size);
            b = b.with_inner_size(sz);
            if let Some(p) = pos {
                // Validate the persisted position against the live monitor
                // layout. A previously-attached second monitor leaves
                // coordinates that fall outside any current display; egui would
                // then open the viewport off-screen (= invisible). Query the OS
                // work-areas and only re-apply the position when a usable part
                // of the window lands on a connected monitor - otherwise omit
                // with_position() and let it open on the primary monitor. On
                // non-Windows / query failure this returns true (trust the
                // saved value). Manual "Recenter windows" stays as a fallback.
                if window_placement::saved_window_is_visible(
                    p,
                    egui::vec2(sz[0], sz[1]),
                    native_ppp,
                ) {
                    b = b.with_position(p);
                } else {
                    log::warn!(
                        "popout: saved pos ({}, {}) valt buiten alle aangesloten monitoren - open op primair scherm",
                        p.x, p.y
                    );
                }
            }
            *init_applied = true;
        }
        b
    }

    /// Read the current viewport pos+size from egui and update the supplied
    /// state slots. Returns `true` when anything changed by more than 5 px
    /// AND the user is not actively dragging/resizing (i.e. mouse button is
    /// up). Per-frame `save_full_config()` during a drag causes ~5-20 ms of
    /// blocking disk-I/O which stalls the input thread; Windows then shows
    /// the window oscillating between successive committed positions. By
    /// gating the save on "pointer released" the move/resize stays smooth
    /// during the gesture and persists on release.
    fn track_popout_geometry(
        ctx: &egui::Context,
        pos: &mut Option<egui::Pos2>,
        size: &mut Option<egui::Vec2>,
    ) -> bool {
        // Grootte via screen_rect (egui's canvas - beweegt betrouwbaar mee bij
        // resize; viewport().inner_rect bleek bevroren). Positie via outer_rect
        // (enige bron voor venster-positie).
        let outer = ctx.input(|i| i.viewport().outer_rect);
        let ns = ctx.screen_rect().size();
        let mut state_changed = false;
        if let Some(o) = outer {
            let np = o.min;
            if pos.map_or(true, |p| (p.x - np.x).abs() > 5.0 || (p.y - np.y).abs() > 5.0) {
                *pos = Some(np);
                state_changed = true;
            }
        }
        if size.map_or(true, |s| (s.x - ns.x).abs() > 5.0 || (s.y - ns.y).abs() > 5.0) {
            *size = Some(ns);
            state_changed = true;
        }
        state_changed && !ctx.input(|i| i.pointer.any_down())
    }

    /// Bring every pop-out window back onto the main window's monitor.
    /// Recovery for a pop-out left on a now-disconnected second monitor:
    /// egui/eframe never tell the app which monitors are currently connected,
    /// so the restored absolute position cannot be auto-validated against the
    /// live monitor layout. This is the manual escape hatch - it replaces
    /// hand-editing the `*_popout_pos` lines in the .conf and restarting.
    /// Positions are anchored just inside the main window (always visible) and
    /// staggered so stacked pop-outs don't perfectly overlap. Clearing each
    /// `*_init_applied` flag makes `apply_popout_geometry` re-apply the new
    /// position on the next frame, so an open pop-out moves immediately - no
    /// restart needed.
    fn recenter_popouts(&mut self, ctx: &egui::Context) {
        let anchor = ctx.input(|i| i.viewport().outer_rect)
            .map(|r| r.min)
            .unwrap_or(egui::pos2(80.0, 80.0));
        let base = egui::pos2(anchor.x + 40.0, anchor.y + 40.0);
        let place = |pos: &mut Option<egui::Pos2>, init: &mut bool, dx: f32, dy: f32| {
            *pos = Some(egui::pos2(base.x + dx, base.y + dy));
            *init = false;
        };
        place(&mut self.vrx_popout_pos, &mut self.vrx_popout_init_applied, 0.0, 0.0);
        place(&mut self.spectrum_popout_pos, &mut self.spectrum_popout_init_applied, 24.0, 24.0);
        place(&mut self.rx2_popout_pos, &mut self.rx2_popout_init_applied, 48.0, 48.0);
        place(&mut self.yaesu_popout_pos, &mut self.yaesu_popout_init_applied, 72.0, 72.0);
        place(&mut self.yaesu2_popout_pos, &mut self.yaesu2_popout_init_applied, 96.0, 96.0);
        // The joined RX1+RX2 spectrum popout also restores via apply_popout_geometry.
        place(&mut self.popout_joined_pos, &mut self.popout_joined_init_applied, 120.0, 120.0);
        self.save_full_config();
        log::info!("Pop-out windows recentered onto main monitor at {:?}", base);
    }

    fn render_split_join_button(
        &mut self,
        ui: &mut egui::Ui,
        label: &str,
        target_state: bool,
        size: Option<egui::Vec2>,
        rounding: egui::Rounding,
    ) {
        let active = target_state == self.popout_joined;
        let mut btn = if active {
            egui::Button::new(RichText::new(label).strong())
                .fill(Color32::from_rgb(100, 160, 230))
        } else {
            egui::Button::new(label)
        };
        btn = btn.rounding(rounding);
        if let Some(s) = size { btn = btn.min_size(s); }
        if ui.add(btn).on_hover_text("Popouts split / joined.").clicked() && !active {
            self.popout_joined = target_state;
            self.save_full_config();
        }
    }

    /// Render Split/Join as a single segmented toggle (two halves of one
    /// rounded rectangle). Outer corners rounded, inner edge flat, zero
    /// spacing between halves. Active = blue fill.
    fn render_split_join_segmented(
        &mut self,
        ui: &mut egui::Ui,
        vertical: bool,
        size: Option<egui::Vec2>,
    ) {
        let r = 2.0;
        let (split_round, join_round) = if vertical {
            (
                egui::Rounding { nw: r, ne: r, sw: 0.0, se: 0.0 },
                egui::Rounding { nw: 0.0, ne: 0.0, sw: r, se: r },
            )
        } else {
            (
                egui::Rounding { nw: r, ne: 0.0, sw: r, se: 0.0 },
                egui::Rounding { nw: 0.0, ne: r, sw: 0.0, se: r },
            )
        };
        let split_first = matches!(
            ui.layout().main_dir(),
            egui::Direction::TopDown | egui::Direction::LeftToRight
        );
        let stroke = egui::Stroke::new(2.0, ui.visuals().widgets.noninteractive.bg_stroke.color);
        egui::Frame::none()
            .stroke(stroke)
            .rounding(egui::Rounding::same(r))
            .inner_margin(egui::Margin::same(0.0))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
                if split_first {
                    self.render_split_join_button(ui, "Split", false, size, split_round);
                    self.render_split_join_button(ui, "Join", true, size, join_round);
                } else {
                    self.render_split_join_button(ui, "Join", true, size, join_round);
                    self.render_split_join_button(ui, "Split", false, size, split_round);
                }
            });
    }

    /// Mic gate-delay (ms) for the given PTT path, honoring spike-protection.
    /// Returns 0 when protection is off - no added latency for isolated/headset audio.
    fn ptt_gate_delay_ms(&self, yaesu: bool) -> u32 {
        if !self.spike_protection {
            0
        } else if yaesu {
            self.mic_gate_delay_yaesu_ms
        } else {
            self.mic_gate_delay_thetis_ms
        }
    }

    /// PTT side-effects that protect a built-in speaker+mic from the switch-on
    /// spike: instantly mute local playback, and on keyup set the per-path mic
    /// gate-delay so the spike decays before mic audio reaches TX. Sent right
    /// before the PTT command so the engine applies the delay before it opens the
    /// capture gate. Playback-mute is unconditional (correct for any TX, half-duplex);
    /// the gate-delay is 0 unless spike-protection is on. Call edge-triggered only.
    fn apply_ptt_spike_protection(&self, yaesu: bool, active: bool) {
        if active {
            let _ = self
                .cmd_tx
                .send(Command::SetMicGateDelayMs(self.ptt_gate_delay_ms(yaesu)));
            let _ = self.cmd_tx.send(Command::SetPlaybackMute(true));
        } else {
            let _ = self.cmd_tx.send(Command::SetPlaybackMute(false));
        }
    }

    fn save_ptt_config(&self) {
        if let Ok(exe) = std::env::current_exe() {
            let path = exe.with_file_name("thetislink-client.conf");
            if let Ok(mut content) = std::fs::read_to_string(&path) {
                // Remove old ptt lines
                content = content.lines()
                    .filter(|l| !l.starts_with("ptt_toggle=") && !l.starts_with("midi_ptt_toggle=")
                        && !l.starts_with("yaesu_ptt_toggle=")
                        && !l.starts_with("yaesu2_enabled=") && !l.starts_with("yaesu2_ptt_toggle=")
                        && !l.starts_with("yaesu2_popout=")
                        && !l.starts_with("yaesu_mic_gain=") && !l.starts_with("yaesu2_mic_gain=")
                        && !l.starts_with("yaesu_compressor=") && !l.starts_with("yaesu2_compressor=")
                        && !l.starts_with("yaesu_tx_agc=") && !l.starts_with("yaesu2_tx_agc="))
                    .collect::<Vec<_>>().join("\n");
                content.push_str(&format!(
                    "\nptt_toggle={}\nyaesu_ptt_toggle={}\nmidi_ptt_toggle={}\nyaesu2_enabled={}\nyaesu2_ptt_toggle={}\nyaesu2_popout={}\nyaesu_mic_gain={:.3}\nyaesu2_mic_gain={:.3}\nyaesu_compressor={}\nyaesu2_compressor={}\nyaesu_tx_agc={}\nyaesu2_tx_agc={}\n",
                    self.ptt_toggle_mode, self.yaesu_ptt_toggle_mode, self.midi_ptt_toggle_mode,
                    self.yaesu2_enabled, self.yaesu2_ptt_toggle_mode,
                    self.yaesu2_popout, self.yaesu_mic_gain, self.yaesu2_mic_gain,
                    self.yaesu_compressor, self.yaesu2_compressor, self.yaesu_tx_agc, self.yaesu2_tx_agc));
                let _ = std::fs::write(path, content);
            }
        }
    }

    fn relay_config(&self) -> sdr_remote_relay::RelayConfig {
        sdr_remote_relay::RelayConfig {
            enabled: self.relay_enabled,
            url: self.relay_url.trim().to_string(),
            station: self.relay_station.trim().to_string(),
            token: self.relay_token.clone(),
            role: sdr_remote_relay::RelayRole::Client,
            instance: self.relay_instance_id.clone(),
            name: self.relay_device_name.clone(),
            udp_port: sdr_remote_relay::relay_udp_port_resolve(self.relay_udp_enabled),
        }
    }

    fn sync_relay_monitor(&mut self) {
        if self.relay_external {
            // Relay draait als transport (monitor in main.rs); een live restart hier
            // zou een tweede relay-verbinding maken. Config-wijziging = app-herstart.
            return;
        }
        let cfg = self.relay_config();
        if !cfg.enabled {
            if let Some(monitor) = self.relay_monitor.take() {
                monitor.stop();
            }
            self.relay_status = None;
            return;
        }
        if let Some(monitor) = self.relay_monitor.as_ref() {
            monitor.update_config(cfg);
        } else {
            let monitor = sdr_remote_relay::RelayMonitor::start_threaded(cfg);
            self.relay_status = Some(monitor.status_handle());
            self.relay_monitor = Some(monitor);
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
            self.spectrum_total_h,
            self.spectrum_popout_pos.map(|p| (p.x, p.y)),
            self.spectrum_popout_size.map(|v| (v.x, v.y)),
            self.rx2_popout_pos.map(|p| (p.x, p.y)),
            self.rx2_popout_size.map(|v| (v.x, v.y)),
            self.popout_joined_pos.map(|p| (p.x, p.y)),
            self.popout_joined_size.map(|v| (v.x, v.y)),
            self.yaesu_popout_pos.map(|p| (p.x, p.y)),
            self.yaesu_popout_size.map(|v| (v.x, v.y)),
            self.yaesu2_popout_pos.map(|p| (p.x, p.y)),
            self.yaesu2_popout_size.map(|v| (v.x, v.y)),
            self.vrx_popout_pos.map(|p| (p.x, p.y)),
            self.vrx_popout_size.map(|v| (v.x, v.y)),
            Some(self.vrx1_volume),
            Some(self.vrx1_enabled),
            Some(self.vrx1_freq_hz),
            Some(self.vrx1_mode),
            Some(self.vrx2_volume),
            Some(self.vrx2_enabled),
            Some(self.vrx2_freq_hz),
            Some(self.vrx2_mode),
            Some(self.vrx1_spectrum_zoom),
            Some(self.vrx2_spectrum_zoom),
            Some(self.vrx1_ref_db),
            Some(self.vrx1_range_db),
            Some(self.vrx1_wf_contrast),
            Some(self.vrx1_pan),
            Some(self.vrx1_auto_ref),
            Some(self.vrx1_filter_low_hz),
            Some(self.vrx1_filter_high_hz),
            Some(self.vrx1_high_res_spectrum),
            Some(self.vrx2_ref_db),
            Some(self.vrx2_range_db),
            Some(self.vrx2_wf_contrast),
            Some(self.vrx2_pan),
            Some(self.vrx2_auto_ref),
            Some(self.vrx2_filter_low_hz),
            Some(self.vrx2_filter_high_hz),
            Some(self.vrx2_high_res_spectrum),
            &self.wf_contrast_per_band,
            self.rx2_spectrum_ref_db,
            self.rx2_spectrum_range_db,
            self.rx2_auto_ref_enabled,
            self.rx2_waterfall_contrast,
            self.rx2_enabled,
            self.thetis_wideband_audio,
            self.popout_joined,
            self.popout_meter_analog,
            self.spectrum_popout,
            self.rx2_popout,
            self.main_window_pos.map(|p| (p.x, p.y)),
            self.ub_show_menu,
            self.collapse_diversity,
            self.collapse_yaesu_eq,
            self.collapse_yaesu_memories,
            self.collapse_yaesu_menu,
            self.yaesu_memories_h,
            self.device_tab,
            self.yaesu_enabled,
            self.yaesu_volume,
            self.yaesu2_volume,
            self.yaesu_popout,
            &self.yaesu_eq_active_profile,
            &self.yaesu_eq_profiles,
            &self.yaesu2_eq_active_profile,
            &self.yaesu2_eq_profiles,
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
            self.theme_variant.as_str(),
            &self.theme_custom.to_config_string(),
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
            // Set mode first (filter range depends on mode). Skip a VFO-A (TX)
            // mode change during own TX: the server drops it (Thetis-bug
            // workaround) and Thetis won't echo, so an optimistic update would
            // leave the indication stuck out of sync. RX2 is RX-only.
            let block_mode = matches!(vfo, Vfo::A) && self.ptt;
            if mem.mode != cur_mode && !block_mode {
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
    // controls-scaffolding - sub-stap 4 writeback-extract
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
    /// state (PATCH-rx2-inline-edit - symmetrisch met RX1).
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
            // Sync client-side TX-keten (compressor + AGC) radio 1.
            let _ = self.cmd_tx.send(Command::SetYaesuCompressor(self.yaesu_compressor));
            let _ = self.cmd_tx.send(Command::SetYaesuTxAgc(self.yaesu_tx_agc));
            // Auto-read memory channels for channel name info
            self.yaesu_mem_radio_received = false;
            let _ = self.cmd_tx.send(Command::SetControl(
                ControlId::YaesuReadMemories, 0));
            self.yaesu_enable_sent = true;
        }
        // Dual-radio slot 1: dezelfde Yaesu-enable schakelt ook de 2e radio in.
        // De client is dual-radio-bewust -> stuurt Yaesu2Enable=1 (komt op
        // yaesu2_addrs, krijgt RadioInfo + slot-1 data) en zet het muted-gestarte
        // slot-1 volume op de UI-waarde (unmute, build-88-les).
        if state.connected && self.yaesu2_enabled && !self.yaesu2_enable_sent {
            let _ = self.cmd_tx.send(Command::SetYaesu2Enable(true));
            let _ = self.cmd_tx.send(Command::SetYaesu2Volume(self.yaesu2_volume));
            // Sync lokale mic-gain naar engine (zoals 991A).
            let _ = self.cmd_tx.send(Command::SetYaesu2TxGain(self.yaesu2_mic_gain));
            // Sync client-side TX-keten (compressor + AGC) radio 2 (FTX-1).
            let _ = self.cmd_tx.send(Command::SetYaesu2Compressor(self.yaesu2_compressor));
            let _ = self.cmd_tx.send(Command::SetYaesu2TxAgc(self.yaesu2_tx_agc));
            // FT0 = MAIN als actieve RX/TX -> audio volgt A/MAIN (springt niet naar SUB).
            let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button, 11));
            // Auto-read geheugens NIET direct: radio+server zijn vlak na connect te
            // druk -> MR-scan mist kanalen. Uitstellen ~1,5s (vuurt hieronder).
            self.yaesu2_autoread_at = Some(Instant::now() + std::time::Duration::from_millis(1500));
            self.yaesu2_enable_sent = true;
        }
        // Uitgestelde geheugen-auto-read: pas als de FTX-1 echt verbonden is én de
        // settle-tijd verstreken is. Eén keer; daarna gewist.
        if let Some(t) = self.yaesu2_autoread_at {
            if state.yaesu2_connected && Instant::now() >= t {
                self.yaesu2_mem_radio_received = false;
                let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2ReadMemories, 0));
                self.yaesu2_autoread_at = None;
            }
        }
        if !state.connected {
            self.yaesu_enable_sent = false;
            self.yaesu2_enable_sent = false;
            self.yaesu2_autoread_at = None;
        }
        // Forceer HF naar A (FTX-1): HF wint altijd de USB-audio. Staat HF op B
        // terwijl A VHF/UHF is, dan zou je op A zenden (FT0) maar HF op B horen =
        // split. Een A/B-swap (SV) brengt HF naar A/MAIN -> audio + TX + bediening
        // weer samen op A. (<100 MHz = HF/laag incl. 6m/4m = de SDR-ontvanger;
        // >100 MHz = VHF/UHF.) 2s cooldown voorkomt herhaald swappen vóór de
        // state-update binnen is.
        if state.yaesu2_connected {
            let a_hf = state.yaesu2_freq_a > 0 && state.yaesu2_freq_a < 100_000_000;
            let b_hf = state.yaesu2_freq_b > 0 && state.yaesu2_freq_b < 100_000_000;
            let cooldown_ok = self.yaesu2_hf_swap_at
                .map_or(true, |t| t.elapsed().as_millis() > 2000);
            if b_hf && !a_hf && cooldown_ok {
                let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2SelectVfo, 2));
                self.yaesu2_hf_swap_at = Some(std::time::Instant::now());
            }
        }
        // VRX1+VRX2 state sync: when the server reconnects, push the
        // locally-persisted state for BOTH channels (enable + freq +
        // mode + volume) so the server matches what the client thinks
        // it has. Without this, a client-only restart would leave the
        // GUI showing defaults while the server still ran the previous
        // session's VRX state (or vice versa).
        if state.connected && self.vrx_state_sync_pending {
            // Re-send wideband audio toggle on (re)connect. App::new fires
            // it before server_addr is set, which drops the command -
            // server then defaults to NB until user toggles. Resending
            // here after handshake fixes the startup mismatch.
            let _ = self.cmd_tx.send(Command::SetThetisWidebandAudio(self.thetis_wideband_audio));
            let _ = self.cmd_tx.send(Command::SetVrxMode(self.vrx1_mode));
            if self.vrx1_freq_hz > 0 {
                let _ = self.cmd_tx.send(Command::SetVrxFrequency(self.vrx1_freq_hz));
            }
            let _ = self.cmd_tx.send(Command::SetVrxVolume(self.vrx1_volume));
            let _ = self.cmd_tx.send(Command::SetVrxEnabled(self.vrx1_enabled));

            let _ = self.cmd_tx.send(Command::SetVrx2Mode(self.vrx2_mode));
            if self.vrx2_freq_hz > 0 {
                let _ = self.cmd_tx.send(Command::SetVrx2Frequency(self.vrx2_freq_hz));
            }
            let _ = self.cmd_tx.send(Command::SetVrx2Volume(self.vrx2_volume));
            let _ = self.cmd_tx.send(Command::SetVrx2Enabled(self.vrx2_enabled));
            let _ = self.cmd_tx.send(Command::SetVrxFilter(0, self.vrx1_filter_low_hz, self.vrx1_filter_high_hz));
            let _ = self.cmd_tx.send(Command::SetVrxFilter(1, self.vrx2_filter_low_hz, self.vrx2_filter_high_hz));
            // VRX rate-mode + SAM auto-tune (PATCH-vrx-wide-sam-ux) - resync.
            let _ = self.cmd_tx.send(Command::SetVrxRateMode(self.vrx_rate_mode));
            let _ = self.cmd_tx.send(Command::SetVrxRateMode2(self.vrx_rate_mode2));
            let _ = self.cmd_tx.send(Command::SetVrxAutoTune(0, self.vrx1_auto_tune));
            let _ = self.cmd_tx.send(Command::SetVrxAutoTune(1, self.vrx2_auto_tune));
            // Re-send effective high-res spectrum on (re)connect.
            // Effective = VRX enabled AND high-res toggle on.
            if self.vrx1_enabled && self.vrx1_high_res_spectrum {
                let span_hz = (self.full_spectrum_span_hz as f32 / self.vrx1_spectrum_zoom.max(1.0)) as u32;
                let span_khz = ((span_hz / 1000).max(1)) as u16;
                self.vrx1_high_res_last_span_khz = span_khz;
                let _ = self.cmd_tx.send(Command::SetVrxHighResSpectrum(0, true, span_khz));
            }
            if self.vrx2_enabled && self.vrx2_high_res_spectrum {
                let span_hz = (self.rx2_full_spectrum_span_hz as f32 / self.vrx2_spectrum_zoom.max(1.0)) as u32;
                let span_khz = ((span_hz / 1000).max(1)) as u16;
                self.vrx2_high_res_last_span_khz = span_khz;
                let _ = self.cmd_tx.send(Command::SetVrxHighResSpectrum(1, true, span_khz));
            }
            self.vrx_state_sync_pending = false;
        }
        if !state.connected {
            self.vrx_state_sync_pending = true;
        }
        // TL2-1 ctun-auto-recenter: snapshot vorige connected-state vóór mutation,
        // anders detecteren we false->true reconnects niet (een oudere
        // `state.connected && !self.connected` check werkte nooit omdat
        // self.connected hier al gemuteerd zou zijn). Bug was latent - de
        // zoom-reset-block op reconnect werd waarschijnlijk al langer overgeslagen.
        let was_connected = self.connected;
        self.connected = state.connected;
        self.ptt_denied = state.ptt_denied;
        self.rtt_ms = state.rtt_ms;
        self.jitter_ms = state.jitter_ms;
        self.buffer_depth = state.buffer_depth;
        self.rx_packets = state.rx_packets;
        self.yaesu_audio_packets = state.yaesu_audio_packets;
        self.yaesu_jitter_ms = state.yaesu_jitter_ms;
        self.yaesu_buffer_depth = state.yaesu_buffer_depth;
        self.yaesu2_audio_packets = state.yaesu2_audio_packets;
        self.yaesu2_jitter_ms = state.yaesu2_jitter_ms;
        self.yaesu2_buffer_depth = state.yaesu2_buffer_depth;
        self.vrx1_audio_packets = state.vrx1_audio_packets;
        self.vrx1_jitter_ms = state.vrx1_jitter_ms;
        self.vrx1_buffer_depth = state.vrx1_buffer_depth;
        self.vrx2_audio_packets = state.vrx2_audio_packets;
        self.vrx2_jitter_ms = state.vrx2_jitter_ms;
        self.vrx2_buffer_depth = state.vrx2_buffer_depth;
        self.down_kbps = state.down_kbps;
        self.up_kbps = state.up_kbps;
        self.bw_breakdown = state.bw_breakdown.clone();
        self.loss_percent = state.loss_percent;
        self.capture_level = state.capture_level;
        self.playback_level = state.playback_level;
        self.playback_level_bin_r = state.playback_level_bin_r;
        self.playback_level_rx2 = state.playback_level_rx2;
        self.playback_level_yaesu = state.playback_level_yaesu;
        self.playback_level_yaesu2 = state.playback_level_yaesu2;
        self.playback_level_vrx1 = state.playback_level_vrx1;
        self.playback_level_vrx2 = state.playback_level_vrx2;
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
        // Reset zoom on reconnect (connected false->true) or power ON
        // Span is reset to 0 so the first spectrum packet triggers zoom calculation.
        // Use `was_connected` snapshot - see comment above on connected-state mutation.
        let reconnected = state.connected && !was_connected;
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
            // Reset TCI control states to defaults - server will push current values
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
            // TL2-1 ctun-auto-recenter: push allow_zoom_below_2x bij elke
            // (re)connect zodat server-strictest-vink-policy correct kan worden
            // berekend ZONDER te wachten op handmatige toggle.
            let _ = self.cmd_tx.send(Command::SetControl(
                sdr_remote_core::protocol::ControlId::AllowZoomBelow2x,
                if self.allow_zoom_below_2x { 1 } else { 0 },
            ));
            // Also push S-meter source choice on (re)connect - engine state
            // and UI state must agree so the right subscription mask is sent.
            let _ = self.cmd_tx.send(Command::SetSmeterSource(self.smeter_source));
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
        self.thetis_configured = state.thetis_configured;
        // Once user changes filter locally, client is authoritative until mode changes.
        // filter_changed_at is cleared on mode change (above), so new mode values are accepted.
        if self.filter_changed_at.is_none() {
            self.filter_low_hz = state.filter_low_hz;
            self.filter_high_hz = state.filter_high_hz;
        }
        self.thetis_starting = state.thetis_starting;
        // TCI controls - suppress server sync for 500ms after local change
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
        if state.vrx1_extracted_sequence != self.vrx1_extracted_sequence && !state.vrx1_extracted_bins.is_empty() {
            self.vrx1_extracted_bins = state.vrx1_extracted_bins.clone();
            self.vrx1_extracted_center_hz = state.vrx1_extracted_center_hz;
            self.vrx1_extracted_span_hz = state.vrx1_extracted_span_hz;
            self.vrx1_extracted_sequence = state.vrx1_extracted_sequence;
            self.vrx1_hr_waterfall.push_full_only(
                &self.vrx1_extracted_bins,
                self.vrx1_extracted_center_hz,
                self.vrx1_extracted_span_hz,
                self.vrx1_extracted_sequence,
            );
        }
        if state.vrx2_extracted_sequence != self.vrx2_extracted_sequence && !state.vrx2_extracted_bins.is_empty() {
            self.vrx2_extracted_bins = state.vrx2_extracted_bins.clone();
            self.vrx2_extracted_center_hz = state.vrx2_extracted_center_hz;
            self.vrx2_extracted_span_hz = state.vrx2_extracted_span_hz;
            self.vrx2_extracted_sequence = state.vrx2_extracted_sequence;
            self.vrx2_hr_waterfall.push_full_only(
                &self.vrx2_extracted_bins,
                self.vrx2_extracted_center_hz,
                self.vrx2_extracted_span_hz,
                self.vrx2_extracted_sequence,
            );
        }
        // SAM auto-tune: mirror the server-followed carrier freq onto the VRX
        // VFO display (display-only; we do NOT echo it back as a tune command,
        // so there's no feedback loop with the server's AFC).
        if state.vrx1_autotune_freq_hz != 0
            && state.vrx1_autotune_freq_hz != self.last_vrx1_autotune_hz
        {
            self.last_vrx1_autotune_hz = state.vrx1_autotune_freq_hz;
            self.vrx1_freq_hz = state.vrx1_autotune_freq_hz;
        }
        if state.vrx2_autotune_freq_hz != 0
            && state.vrx2_autotune_freq_hz != self.last_vrx2_autotune_hz
        {
            self.last_vrx2_autotune_hz = state.vrx2_autotune_freq_hz;
            self.vrx2_freq_hz = state.vrx2_autotune_freq_hz;
        }
        // TX modulation filter: mirror server-reported support + value. Seed the
        // editable fields once from the first push so they show the real band.
        self.tx_filter_supported = state.tx_filter_supported;
        if state.tx_filter_supported && !self.tx_filter_initialized {
            self.tx_filter_initialized = true;
            self.tx_filter_low_hz = state.tx_filter_low_hz;
            self.tx_filter_high_hz = state.tx_filter_high_hz;
        }
        // Follow-RX: while enabled, keep TX = RX filter. The TX filter is a
        // POSITIVE audio passband (Thetis applies the sideband per mode);
        // `rx_to_tx_band` converts the RX edges, incl. the straddle-zero case
        // for AM/SAM/FM so a symmetric band doesn't collapse to zero width.
        // Rate-limited to ~7/s so a spectrum drag doesn't spam Thetis with
        // tx_filter_band_ex; the dedupe still guarantees the final value lands.
        if self.tx_filter_follow_rx && self.tx_filter_supported {
            let cur = (self.filter_low_hz, self.filter_high_hz);
            if self.last_tx_follow_sent != Some(cur) {
                let ready = self.tx_follow_last_send_at
                    .map_or(true, |t| t.elapsed().as_millis() >= 150);
                if ready {
                    self.last_tx_follow_sent = Some(cur);
                    self.tx_follow_last_send_at = Some(Instant::now());
                    let (tlo, thi) = rx_to_tx_band(cur.0, cur.1);
                    let _ = self.cmd_tx.send(Command::SetTxFilter(tlo, thi));
                }
            }
        }
        if state.full_spectrum_sequence != self.full_spectrum_sequence && !state.full_spectrum_bins.is_empty() {
            // Adjust default zoom when span first becomes known (0 -> real value)
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
                if !self.vrx1_zoom_initialized {
                    self.vrx1_spectrum_zoom = default_zoom_for_span(self.full_spectrum_span_hz);
                    self.vrx1_pan = 0.0;
                }
            }
        }

        // Per-VRX auto-ref (skip SSB passband ±3 kHz around VRX freq)
        if self.vrx1_auto_ref && !self.full_spectrum_bins.is_empty() && self.full_spectrum_span_hz > 0 {
            let nbins = self.full_spectrum_bins.len();
            let hz_per_bin = self.full_spectrum_span_hz as f64 / nbins as f64;
            let start_hz = self.full_spectrum_center_hz as f64 - self.full_spectrum_span_hz as f64 / 2.0;
            let pass_lo = self.vrx1_freq_hz as f64 - 3_000.0;
            let pass_hi = self.vrx1_freq_hz as f64 + 3_000.0;
            let lo_bin = ((pass_lo - start_hz) / hz_per_bin) as i32;
            let hi_bin = ((pass_hi - start_hz) / hz_per_bin) as i32;
            let mut sum_db = 0.0f64;
            let mut count = 0u32;
            for (i, &val) in self.full_spectrum_bins.iter().enumerate() {
                let idx = i as i32;
                if idx >= lo_bin && idx <= hi_bin { continue; }
                let db = -150.0 + (val as f64 / 65535.0) * 120.0;
                sum_db += db;
                count += 1;
            }
            if count > 0 {
                let avg_db = sum_db / count as f64;
                let target = avg_db as f32 + self.vrx1_range_db - 2.0;
                if !self.vrx1_auto_ref_initialized {
                    self.vrx1_auto_ref_value = target;
                    self.vrx1_auto_ref_initialized = true;
                } else {
                    let alpha = if self.vrx1_auto_ref_frames < 45 { 0.10 } else { 0.002 };
                    self.vrx1_auto_ref_value = alpha * target + (1.0 - alpha) * self.vrx1_auto_ref_value;
                }
                self.vrx1_ref_db = self.vrx1_auto_ref_value;
                self.vrx1_auto_ref_frames += 1;
            }
        }
        if self.vrx2_auto_ref && !self.rx2_full_spectrum_bins.is_empty() && self.rx2_full_spectrum_span_hz > 0 {
            let nbins = self.rx2_full_spectrum_bins.len();
            let hz_per_bin = self.rx2_full_spectrum_span_hz as f64 / nbins as f64;
            let start_hz = self.rx2_full_spectrum_center_hz as f64 - self.rx2_full_spectrum_span_hz as f64 / 2.0;
            let pass_lo = self.vrx2_freq_hz as f64 - 3_000.0;
            let pass_hi = self.vrx2_freq_hz as f64 + 3_000.0;
            let lo_bin = ((pass_lo - start_hz) / hz_per_bin) as i32;
            let hi_bin = ((pass_hi - start_hz) / hz_per_bin) as i32;
            let mut sum_db = 0.0f64;
            let mut count = 0u32;
            for (i, &val) in self.rx2_full_spectrum_bins.iter().enumerate() {
                let idx = i as i32;
                if idx >= lo_bin && idx <= hi_bin { continue; }
                let db = -150.0 + (val as f64 / 65535.0) * 120.0;
                sum_db += db;
                count += 1;
            }
            if count > 0 {
                let avg_db = sum_db / count as f64;
                let target = avg_db as f32 + self.vrx2_range_db - 2.0;
                if !self.vrx2_auto_ref_initialized {
                    self.vrx2_auto_ref_value = target;
                    self.vrx2_auto_ref_initialized = true;
                } else {
                    let alpha = if self.vrx2_auto_ref_frames < 45 { 0.10 } else { 0.002 };
                    self.vrx2_auto_ref_value = alpha * target + (1.0 - alpha) * self.vrx2_auto_ref_value;
                }
                self.vrx2_ref_db = self.vrx2_auto_ref_value;
                self.vrx2_auto_ref_frames += 1;
            }
        }

        // ── Per-VRX S-meter: integrate spectrum power across SSB passband,
        //    partial-bin weighting at edges, EMA ballistic. Uses the same
        //    server calibration as render_vrx_strip (-150 .. -30 dBm scale).
        let compute_vrx_smeter = |bins: &[u16], center_hz: u32, span_hz: u32,
                                  vrx_freq_hz: u64, filt_low_hz: i32, filt_high_hz: i32,
                                  cal_offset_db: f32| -> Option<f32> {
            if bins.is_empty() || span_hz == 0 { return None; }
            let hz_per_bin = span_hz as f64 / bins.len() as f64;
            let start_hz = center_hz as f64 - span_hz as f64 / 2.0;
            let lo_hz = vrx_freq_hz as f64 + filt_low_hz as f64;
            let hi_hz = vrx_freq_hz as f64 + filt_high_hz as f64;
            if hi_hz <= lo_hz { return None; }
            let lo_bin_f = (lo_hz - start_hz) / hz_per_bin;
            let hi_bin_f = (hi_hz - start_hz) / hz_per_bin;
            let lo_bin = lo_bin_f.floor() as i32;
            let hi_bin = hi_bin_f.ceil() as i32;
            if hi_bin <= 0 || lo_bin >= bins.len() as i32 { return None; }
            let mut power_mw = 0.0_f64;
            for i in lo_bin.max(0)..hi_bin.min(bins.len() as i32) {
                // Partial-bin weighting: full=1.0, edge=fraction inside passband
                let bin_lo = i as f64;
                let bin_hi = (i + 1) as f64;
                let overlap_lo = bin_lo.max(lo_bin_f);
                let overlap_hi = bin_hi.min(hi_bin_f);
                let frac = (overlap_hi - overlap_lo).max(0.0).min(1.0);
                if frac <= 0.0 { continue; }
                let val = bins[i as usize];
                let db = -150.0 + (val as f64 / 65535.0) * 120.0;
                power_mw += frac * 10.0_f64.powf(db / 10.0);
            }
            if power_mw > 0.0 {
                // Empirical calibration to match main RX1/RX2 S-meter.
                // VRX1 needs more offset than VRX2 because RX1 spectrum
                // bins are scaled slightly differently in Thetis.
                Some(10.0 * power_mw.log10() as f32 + cal_offset_db)
            } else {
                None
            }
        };

        // EMA ballistic constants (per-frame; spectrum runs at ~15 FPS so
        // Δt ≈ 66 ms): fast attack (~15ms τ), slow decay (~400ms τ).
        let smeter_alpha_attack = 1.0_f32 - (-0.066_f32 / 0.015).exp();
        let smeter_alpha_decay = 1.0_f32 - (-0.066_f32 / 0.400).exp();
        let apply_smeter_ema = |new_dbm: f32, cur: f32, initialized: bool| -> f32 {
            if !initialized { return new_dbm; }
            let alpha = if new_dbm > cur { smeter_alpha_attack } else { smeter_alpha_decay };
            alpha * new_dbm + (1.0 - alpha) * cur
        };

        if !self.vrx1_enabled {
            // VRX1 disabled -> freeze S-meter at -127 dBm (visual "no signal")
            self.vrx1_smeter_dbm = -127.0;
            self.vrx1_smeter_peak = -127.0;
            self.vrx1_smeter_initialized = false;
        } else if let Some(dbm) = compute_vrx_smeter(
            &self.full_spectrum_bins, self.full_spectrum_center_hz, self.full_spectrum_span_hz,
            self.vrx1_freq_hz, self.vrx1_filter_low_hz, self.vrx1_filter_high_hz,
            10.0,
        ) {
            self.vrx1_smeter_dbm = apply_smeter_ema(dbm, self.vrx1_smeter_dbm, self.vrx1_smeter_initialized);
            self.vrx1_smeter_initialized = true;
            if self.vrx1_smeter_dbm >= self.vrx1_smeter_peak {
                self.vrx1_smeter_peak = self.vrx1_smeter_dbm;
                self.vrx1_smeter_peak_time = Instant::now();
            } else if self.vrx1_smeter_peak_time.elapsed().as_secs_f32() > 2.0 {
                self.vrx1_smeter_peak = self.vrx1_smeter_dbm;
                self.vrx1_smeter_peak_time = Instant::now();
            }
        }
        if !self.vrx2_enabled {
            self.vrx2_smeter_dbm = -127.0;
            self.vrx2_smeter_peak = -127.0;
            self.vrx2_smeter_initialized = false;
        } else if let Some(dbm) = compute_vrx_smeter(
            &self.rx2_full_spectrum_bins, self.rx2_full_spectrum_center_hz, self.rx2_full_spectrum_span_hz,
            self.vrx2_freq_hz, self.vrx2_filter_low_hz, self.vrx2_filter_high_hz,
            5.0,
        ) {
            self.vrx2_smeter_dbm = apply_smeter_ema(dbm, self.vrx2_smeter_dbm, self.vrx2_smeter_initialized);
            self.vrx2_smeter_initialized = true;
            if self.vrx2_smeter_dbm >= self.vrx2_smeter_peak {
                self.vrx2_smeter_peak = self.vrx2_smeter_dbm;
                self.vrx2_smeter_peak_time = Instant::now();
            } else if self.vrx2_smeter_peak_time.elapsed().as_secs_f32() > 2.0 {
                self.vrx2_smeter_peak = self.vrx2_smeter_dbm;
                self.vrx2_smeter_peak_time = Instant::now();
            }
        }
        // High-res VRX spectrum: when toggle on AND VRX enabled, re-send
        // the span to the server whenever the visible window changes
        // (driven by zoom). VRX Enable=OFF kills the high-res stream so
        // a disabled VRX channel costs zero bandwidth.
        if self.vrx1_enabled && self.vrx1_high_res_spectrum && self.full_spectrum_span_hz > 0 {
            let span_hz = (self.full_spectrum_span_hz as f32 / self.vrx1_spectrum_zoom.max(1.0)) as u32;
            let span_khz = ((span_hz / 1000).max(1)) as u16;
            if span_khz != self.vrx1_high_res_last_span_khz {
                self.vrx1_high_res_last_span_khz = span_khz;
                let _ = self.cmd_tx.send(Command::SetVrxHighResSpectrum(0, true, span_khz));
            }
        }
        if self.vrx2_enabled && self.vrx2_high_res_spectrum && self.rx2_full_spectrum_span_hz > 0 {
            let span_hz = (self.rx2_full_spectrum_span_hz as f32 / self.vrx2_spectrum_zoom.max(1.0)) as u32;
            let span_khz = ((span_hz / 1000).max(1)) as u16;
            if span_khz != self.vrx2_high_res_last_span_khz {
                self.vrx2_high_res_last_span_khz = span_khz;
                let _ = self.cmd_tx.send(Command::SetVrxHighResSpectrum(1, true, span_khz));
            }
        }
        // (pending_freq already cleared above, before frequency acceptance)

        // Delayed auto_ref restore after TX->RX transition
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
        self.amplitec_available = state.amplitec_available;
        self.amplitec_connected = state.amplitec_connected;
        self.amplitec_switch_a = state.amplitec_switch_a;
        self.amplitec_switch_b = state.amplitec_switch_b;
        if !state.amplitec_labels.is_empty() {
            self.amplitec_labels = state.amplitec_labels;
        }
        // Power-cap tabel: bij eerste push van server initialiseren we
        // de edit-state. Daarna alleen de read-only spiegel updaten -
        // de operator's in-progress edits worden niet overschreven.
        let was_loaded = self.amplitec_power_loaded;
        self.amplitec_power_max_w = state.amplitec_power_max_w;
        self.amplitec_power_tx_blocked = state.amplitec_power_tx_blocked;
        self.amplitec_power_loaded = state.amplitec_power_loaded;
        if state.amplitec_power_loaded && !was_loaded {
            self.amplitec_power_edit_max_w = state.amplitec_power_max_w;
            self.amplitec_power_edit_tx_blocked = state.amplitec_power_tx_blocked;
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
        self.tuner_available = state.tuner_available;
        self.tuner_connected = state.tuner_connected;
        self.tuner_state = state.tuner_state;
        self.tuner_can_tune = state.tuner_can_tune;
        // Track tune frequency: on real tune (TUNING -> DONE_OK/DONE_ASSUMED) or first
        // done-state after connect (tune_freq still 0). Ignores the fake
        // IDLE -> done-state transitions from the server's stale override.
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
        self.ub_operation = state.ub_operation;
        self.ub_freq_min_mhz = state.ub_freq_min_mhz;
        self.ub_freq_max_mhz = state.ub_freq_max_mhz;

        // Rotor
        self.rotor_connected = state.rotor_connected;
        self.rotor_angle_x10 = state.rotor_angle_x10;
        self.rotor_rotating = state.rotor_rotating;
        self.rotor_target_x10 = state.rotor_target_x10;
        self.rotor_available = state.rotor_available;
        // Yaesu
        self.yaesu_connected = state.yaesu_connected;
        Self::accept_yaesu_freq(
            &mut self.yaesu_freq_a,
            &mut self.yaesu_pending_freq,
            &mut self.yaesu_pending_freq_at,
            state.yaesu_freq_a,
        );
        self.yaesu_freq_b = state.yaesu_freq_b;
        self.yaesu_mode = state.yaesu_mode;
        self.yaesu_smeter = state.yaesu_smeter;
        if state.yaesu_smeter >= self.yaesu_smeter_peak {
            self.yaesu_smeter_peak = state.yaesu_smeter;
            self.yaesu_smeter_peak_time = Instant::now();
        } else if self.yaesu_smeter_peak_time.elapsed().as_secs_f32() > 2.0 {
            self.yaesu_smeter_peak = state.yaesu_smeter;
            self.yaesu_smeter_peak_time = Instant::now();
        }
        self.yaesu_tx_active = state.yaesu_tx_active;
        self.yaesu_power_on = state.yaesu_power_on;
        // Dual-radio slot 1
        self.yaesu_model = state.yaesu_model;
        self.yaesu2_model = state.yaesu2_model;
        self.yaesu2_connected = state.yaesu2_connected;
        Self::accept_yaesu_freq(
            &mut self.yaesu2_freq_a,
            &mut self.yaesu2_pending_freq,
            &mut self.yaesu2_pending_freq_at,
            state.yaesu2_freq_a,
        );
        self.yaesu2_freq_b = state.yaesu2_freq_b;
        self.yaesu2_mode = state.yaesu2_mode;
        self.yaesu2_smeter = state.yaesu2_smeter;
        if state.yaesu2_smeter >= self.yaesu2_smeter_peak {
            self.yaesu2_smeter_peak = state.yaesu2_smeter;
            self.yaesu2_smeter_peak_time = Instant::now();
        } else if self.yaesu2_smeter_peak_time.elapsed().as_secs_f32() > 2.0 {
            self.yaesu2_smeter_peak = state.yaesu2_smeter;
            self.yaesu2_smeter_peak_time = Instant::now();
        }
        self.yaesu2_tx_active = state.yaesu2_tx_active;
        self.yaesu2_power_on = state.yaesu2_power_on;
        self.yaesu2_split = state.yaesu2_split;
        self.yaesu2_scan = state.yaesu2_scan;
        self.yaesu2_tuner_state = state.yaesu2_tuner_state;
        self.yaesu2_feature_levels = state.yaesu2_feature_levels;
        self.yaesu2_clar_offset = state.yaesu2_feature_freqs[3] as i16; // clarifier-offset (§15)
        // yaesu2_feature_toggles wordt achter de debounce gesynct (optimistische toggles).
        self.yaesu2_vfo_select = state.yaesu2_vfo_select;
        self.yaesu2_memory_channel = state.yaesu2_memory_channel;
        // Slider-sync met debounce (1s na lokale wijziging), zoals slot 0.
        let yaesu2_accept = self.yaesu2_control_changed_at
            .map_or(true, |t| t.elapsed().as_millis() > 1000);
        if state.yaesu2_connected && yaesu2_accept {
            self.yaesu2_control_changed_at = None;
            self.yaesu2_squelch = state.yaesu2_squelch as u16;
            self.yaesu2_rf_gain = state.yaesu2_rf_gain as u16;
            self.yaesu2_rf_power = state.yaesu2_tx_power as u16;
            for j in 0..4 { self.yaesu_level_sliders[1][j] = state.yaesu2_feature_levels[8 + j] as i32; }
            for j in 0..3 { self.yaesu_freq_sliders[1][j] = state.yaesu2_feature_freqs[j] as i32; }
            self.yaesu2_feature_toggles = state.yaesu2_feature_toggles;
        }
        // Sync slider values from radio - debounce 1s after local change
        let yaesu_accept = self.yaesu_control_changed_at
            .map_or(true, |t| t.elapsed().as_millis() > 1000);
        if state.yaesu_connected && yaesu_accept {
            self.yaesu_control_changed_at = None;
            self.yaesu_squelch = state.yaesu_squelch as u16;
            self.yaesu_rf_gain = state.yaesu_rf_gain as u16;
            self.yaesu_rf_power = state.yaesu_tx_power as u16;
            for j in 0..4 { self.yaesu_level_sliders[0][j] = state.yaesu_feature_levels[8 + j] as i32; }
            for j in 0..3 { self.yaesu_freq_sliders[0][j] = state.yaesu_feature_freqs[j] as i32; }
            self.yaesu_feature_toggles = state.yaesu_feature_toggles;
        }
        self.yaesu_split_active = state.yaesu_split;
        self.yaesu_scan_active = state.yaesu_scan;
        self.yaesu_tuner_state = state.yaesu_tuner_state;
        self.yaesu_feature_levels = state.yaesu_feature_levels;
        self.yaesu_clar_offset = state.yaesu_feature_freqs[3] as i16; // clarifier-offset (§15)
        // yaesu_feature_toggles wordt achter de debounce gesynct (optimistische toggles).
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
        // Slot-1 (FTX-1) geheugen-dump -> yaesu2_mem_channels (Fase B). Geen MENU-tak
        // (EX-menu = Fase C). FTX-1 MT/MR-formaat te verifiëren op hardware.
        if let Some(ref text) = state.yaesu2_memory_data {
            if let Some(menu_body) = text.strip_prefix("MENU:") {
                // FTX-1 EX-menu dump (Fase C): "p1p2p3:value"-regels.
                if !self.yaesu2_menu_received {
                    self.yaesu2_menu_received = true;
                    self.yaesu2_menu_entries = menu_body.lines()
                        .filter_map(|l| l.split_once(':')
                            .map(|(a, v)| (a.trim().to_string(), v.trim().to_string())))
                        .filter(|(a, _)| a.len() == 6)
                        .collect();
                    self.yaesu2_menu_edits.clear(); // verse waarden -> bewerk-buffers resetten
                    log::info!("[radio1] received {} EX menu values", self.yaesu2_menu_entries.len());
                }
            } else if !self.yaesu2_mem_radio_received {
                self.yaesu2_mem_radio_received = true;
                match crate::ui::yaesu_memory::parse_tab_string(text) {
                    Ok(mut radio_channels) => {
                        let existing = std::mem::take(&mut self.yaesu2_mem_channels);
                        for rch in &mut radio_channels {
                            if rch.name.is_empty() || rch.name.starts_with("CH ") {
                                if let Some(match_ch) = existing.iter().find(|e| e.rx_freq_hz == rch.rx_freq_hz) {
                                    rch.name = match_ch.name.clone();
                                }
                            }
                        }
                        log::info!("[radio1] received {} memory channels", radio_channels.len());
                        self.yaesu2_mem_channels = radio_channels;
                        self.yaesu2_mem_dirty = true;
                    }
                    Err(e) => log::warn!("[radio1] parse memory data: {}", e),
                }
            }
        } else {
            self.yaesu2_mem_radio_received = false;
            self.yaesu2_menu_received = false;
        }
        self.dx_spots = state.dx_spots.clone();

        // RX2 / VFO-B
        self.dx_spots_enabled = state.dx_spots_enabled;
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
        if state.diversity_gain_multi != 0 {
            self.diversity_gain_multi = state.diversity_gain_multi as f32 / 100.0;
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
            log::info!("UI: RX2 AF gain {} -> {}, slider {:.0}% -> {:.0}%",
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
                if !self.vrx2_zoom_initialized {
                    self.vrx2_spectrum_zoom = default_zoom_for_span(self.rx2_full_spectrum_span_hz);
                    self.vrx2_pan = 0.0;
                }
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
            ).on_hover_text("Spectrum reference level in dB.").changed() {
                self.save_full_config();
            }
            if ui.checkbox(&mut self.auto_ref_enabled, "Auto").on_hover_text("Automatically track the reference level.").changed() {
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
            ).on_hover_text("Spectrum vertical range in dB.").changed() {
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
            // TL2-1 ctun-auto-recenter: zoom-min = 2.0 default (anti-smear feature werkbaar);
            // 1.0 toegestaan via setup-vink "Allow zoom <2x" (smear-trade-off).
            let zoom_min: f32 = if self.allow_zoom_below_2x { 1.0 } else { 2.0 };
            // Clamp huidige zoom naar nieuwe minimum (bij vink-toggle naar uit)
            if self.spectrum_zoom < zoom_min {
                self.spectrum_zoom = zoom_min;
            }
            let zoom_changed = ui.add(egui::Slider::new(&mut self.spectrum_zoom, zoom_min..=1024.0)
                .logarithmic(true)
                .custom_formatter(|v, _| format!("{:.0}x", v))
            ).on_hover_text("Spectrum zoom factor.").changed();
            if zoom_changed {
                let max_pan = (0.5 - 0.5 / self.spectrum_zoom) * 0.05;
                self.spectrum_pan = self.spectrum_pan.clamp(-max_pan, max_pan);
            }
            // TL2-1 ctun-auto-recenter setup-vink. Persist + push naar server bij toggle.
            // Server enforced strictest: zolang één client vink-uit, server klemt op 2x.
            if ui.checkbox(&mut self.allow_zoom_below_2x, "Allow zoom <2x")
                .on_hover_text("Toestaan om uit te zoomen onder 2× (waterval-smear bij tunen tussen 1× en 1.2×). Default uit voor smear-vrije ervaring.")
                .changed()
            {
                crate::ui::config::save_allow_zoom_below_2x(self.allow_zoom_below_2x);
                let _ = self.cmd_tx.send(Command::SetControl(
                    sdr_remote_core::protocol::ControlId::AllowZoomBelow2x,
                    if self.allow_zoom_below_2x { 1 } else { 0 },
                ));
            }
            ui.label("Pan:");
            let max_pan = if self.spectrum_zoom > 1.01 { (0.5 - 0.5 / self.spectrum_zoom) * 0.05 } else { 0.0 };
            let pan_changed = ui.add(egui::Slider::new(&mut self.spectrum_pan, -max_pan..=max_pan)
                .custom_formatter(|v, _| format!("{:+.2}", v))
            ).on_hover_text("Pan inside the zoomed spectrum view.").changed();
            ui.label("WF:");
            if ui.add(egui::Slider::new(&mut self.waterfall_contrast, 0.3..=3.0)
                .logarithmic(true)
                .custom_formatter(|v, _| format!("{:.1}", v))
            ).on_hover_text("Waterfall contrast.").changed() {
                // Update per-band storage
                if let Some(ref band) = self.current_band {
                    self.wf_contrast_per_band.insert(band.clone(), self.waterfall_contrast);
                }
                self.save_full_config();
            }

            // FFT size selector - labels computed from current DDC sample rate
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
            // Height slider - only in the main Radio tab. Popouts fill the
            // whole window and ignore this setting.
            if !is_popout {
                ui.label("H:");
                if ui.add(egui::Slider::new(&mut self.spectrum_total_h, 300.0..=1200.0)
                    .custom_formatter(|v, _| format!("{:.0}", v))
                ).on_hover_text("Total height of the spectrum + waterfall block in pixels. The page becomes scrollable when content overflows.").changed() {
                    self.save_full_config();
                }
            }
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
        let tau = 0.02; // 20ms time constant - fast but smooth
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
        // VFO marker follows the smooth center (minus pan offset) - stays perfectly stationary
        let smooth_vfo = (self.smooth_display_center_hz
            - self.spectrum_pan as f64 * self.full_spectrum_span_hz as f64) as u64;

        // Spectrum + waterfall area sizing.
        // - Popout: fills the popout window (dynamic from available_height).
        // - Main Radio tab: fixed `self.spectrum_total_h` so the rest of the
        //   tab (Diversity etc.) can expand below into a scrollable area
        //   instead of pushing the spectrum off-screen.
        let spec_area = if is_popout {
            let available = ui.available_height();
            (available - reserve_bottom).max(200.0)
        } else {
            self.spectrum_total_h.clamp(300.0, 1200.0)
        };
        let spec_h = (spec_area * 0.50).max(100.0);
        let wf_h = (spec_area * 0.50).max(80.0);

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
        // Drag-handle for resizing spectrum + waterfall height in the main
        // Radio tab. Popouts skip - they always fill their window.
        if !is_popout {
            let handle_h = 6.0;
            let (rect, response) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), handle_h),
                egui::Sense::drag(),
            );
            let visuals = ui.visuals();
            let fill = if response.hovered() || response.dragged() {
                visuals.widgets.hovered.bg_fill
            } else {
                visuals.widgets.inactive.bg_fill
            };
            ui.painter().rect_filled(rect, 2.0, fill);
            // Small visual "grip" - three short horizontal lines centred.
            let grip_color = visuals.widgets.inactive.fg_stroke.color;
            let cx = rect.center().x;
            let cy = rect.center().y;
            for dx in [-12.0, 0.0, 12.0] {
                ui.painter().line_segment(
                    [egui::pos2(cx + dx - 4.0, cy), egui::pos2(cx + dx + 4.0, cy)],
                    egui::Stroke::new(1.0, grip_color),
                );
            }
            if response.dragged() {
                let dy = response.drag_delta().y;
                if dy.abs() > 0.01 {
                    self.spectrum_total_h = (self.spectrum_total_h + dy).clamp(300.0, 1200.0);
                }
            }
            if response.drag_stopped() {
                self.save_full_config();
            }
            if response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
            }
        }
    }

    /// Render RX1 controls only (VFO, S-meter, band, mode, freq step, filter, NR, ANF).
    /// `surface` bepaalt welke UI-oppervlakte dit is (MainTab, PopoutSeparate,
    /// PopoutJoined) - meegegeven aan de controls-helpers voor coverage en events.
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
                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
            if ui.add(vol_slider).on_hover_text("Set mix volume.").changed() {
                let _ = self.cmd_tx.send(Command::SetVfoAVolume(self.vfo_a_volume));
                self.save_full_config();
            }
            ui.separator();
            let meter_label = if self.popout_meter_analog { "S: Analog" } else { "S: Bar" };
            if ui.small_button(meter_label).on_hover_text("Toggle S-meter style (bar / analog).").clicked() {
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
        // Disabled during own TX: a mid-TX mode change is dropped by the server
        // (Thetis-bug workaround), so block it in the UI too to avoid a misleading
        // indication.
        let ptt_active = self.ptt;
        let mode_action = self.with_rx_ctx(
            controls::RxChannel::Rx1,
            controls::UiDensity::Extended,
            surface,
            |ctx| {
                ui.add_enabled_ui(!ptt_active, |ui| controls::render_mode_selector(ui, ctx)).inner.map(|c| {
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
        // (niet bij Disconnected / SendFailed) - anders drift UI-state vs. server-state.
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
        // Alleen pending_freq updaten als dispatch slaagde - anders UI-drift.
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
                if ui.add_enabled(idx > 0, minus_btn).on_hover_text("Narrow the filter one step.").clicked() {
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
                if ui.add_enabled(idx < presets.len() - 1, plus_btn).on_hover_text("Widen the filter one step.").clicked() {
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
                if ui.add(nr_btn).on_hover_text("Noise Reduction (click to cycle level).").clicked() {
                    let new_val = if self.nr_level >= 4 { 0 } else { self.nr_level + 1 };
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::NoiseReduction, new_val as u16));
                    self.nr_level = new_val;
                }

                // NB cycle: OFF -> NB1 -> NB2 (extensions) -> OFF
                let nb_label = match self.nb_level { 1 => "NB1".to_string(), 2 => "NB2".to_string(), _ => "NB".to_string() };
                let nb_btn = if self.nb_level > 0 {
                    egui::Button::new(RichText::new(&nb_label).strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else {
                    egui::Button::new(&nb_label)
                };
                if ui.add(nb_btn).on_hover_text("Noise Blanker (click to cycle).").clicked() {
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
                if ui.add(anf_btn).on_hover_text("Auto Notch Filter.").clicked() {
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
                if ui.add(agc_btn).on_hover_text("Microphone AGC on/off.").clicked() {
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
                if ui.add(mon_btn).on_hover_text("TX Monitor.").clicked() {
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
        // -- Spectrum + waterfall (placeholder when no bins yet, mirrors RX2) --
        if !self.spectrum_bins.is_empty() {
            self.render_spectrum_content(ui, ctx, 0.0, true);
        } else {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("Waiting for RX1 spectrum data...").weak());
            });
        }
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

            // Reserve a slim column for the Split button so the analog meter
            // can render at its natural size (matching RX1's meter).
            let split_w: f32 = if show_split_button { 60.0 } else { 0.0 };
            let meter_w = (controls_h * 2.0).min(total_w - 480.0).max(0.0);
            let gap_left = if meter_w > 0.0 { 8.0 } else { 0.0 };
            let split_gap = if split_w > 0.0 { 4.0 } else { 0.0 };
            let controls_w = (total_w - meter_w - split_w - split_gap - gap_left).max(0.0);

            let controls_rect = egui::Rect::from_min_size(start, egui::vec2(controls_w, 500.0));
            let mut left = ui.new_child(egui::UiBuilder::new().max_rect(controls_rect).layout(egui::Layout::top_down(egui::Align::LEFT)));
            self.render_rx2_controls_inner(&mut left, show_split_button, is_popout, surface);

            if show_split_button {
                let btn_h: f32 = 24.0;
                let pair_h: f32 = btn_h * 2.0 + 4.0;
                let split_x = start.x + controls_w + split_gap;
                // Top-align with the meter so RX1 (A<>B) and RX2 (Split/Join)
                // buttons share the same visual baseline.
                let btn_rect = egui::Rect::from_min_size(
                    egui::pos2(split_x, start.y),
                    egui::vec2(split_w, pair_h),
                );
                let mut btn_ui = ui.new_child(
                    egui::UiBuilder::new().max_rect(btn_rect).layout(egui::Layout::top_down(egui::Align::Center))
                );
                let sz = Some(egui::vec2(split_w, btn_h));
                self.render_split_join_segmented(&mut btn_ui, true, sz);
            }

            if meter_w > 80.0 {
                let meter_x = start.x + controls_w + split_gap + split_w + gap_left;
                let meter_rect = egui::Rect::from_min_size(
                    egui::pos2(meter_x, start.y),
                    egui::vec2(meter_w, controls_h),
                );
                let mut right = ui.new_child(
                    egui::UiBuilder::new().max_rect(meter_rect).layout(egui::Layout::top_down(egui::Align::LEFT))
                );
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
        // (klik VFO B label -> edit -> Enter -> dispatch). Scroll-wheel blijft
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
                        self.render_split_join_segmented(ui, false, None);
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
            if ui.add_enabled(self.connected, sync_btn).on_hover_text("VFO B follows VFO A.").clicked() {
                self.vfo_sync = !self.vfo_sync;
                let _ = self.cmd_tx.send(Command::SetVfoSync(self.vfo_sync));
            }

            ui.separator();

            ui.label("VFO B:");
            let vol_slider = egui::Slider::new(&mut self.vfo_b_volume, 0.001..=1.0)
                .logarithmic(true)
                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
            if ui.add(vol_slider).on_hover_text("Set mix volume.").changed() {
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
            // PATCH-tl2-rx2-mode-switch-filter-restore: spiegel RX1-gedrag
            // (regel 2786-2790). Bij click op een RX2-mode-knop accepteren
            // we server-side filter-updates weer (server's rx_filter_band
            // na mode-switch overschrijft de lokaal-aangepaste filter-
            // grenzen die voor het oude mode-profiel golden). Zonder deze
            // reset valt de sync-loop-compare op `state.mode_rx2 !=
            // self.rx2_mode` false omdat de optimistic `self.rx2_mode =
            // click.mode` hierboven al was uitgevoerd; daardoor blijft
            // `rx2_filter_changed_at = Some(...)` staan en negeert de
            // sync-loop de inkomende Rx2FilterLow/High ControlIds.
            self.rx2_filter_changed_at = None;
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
                if ui.add_enabled(idx > 0, minus_btn).on_hover_text("Narrow the filter one step.").clicked() {
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
                if ui.add_enabled(idx < presets.len() - 1, plus_btn).on_hover_text("Widen the filter one step.").clicked() {
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
                if ui.add(nr_btn).on_hover_text("Noise Reduction (click to cycle level).").clicked() {
                    let new_val = if self.rx2_nr_level >= 4 { 0 } else { self.rx2_nr_level + 1 };
                    let _ = self.cmd_tx.send(Command::SetControl(ControlId::Rx2NoiseReduction, new_val as u16));
                    self.rx2_nr_level = new_val;
                }

                // NB cycle: OFF -> NB1 -> NB2 (extensions) -> OFF
                let rx2_nb_label = match self.rx2_nb_level { 1 => "NB1".to_string(), 2 => "NB2".to_string(), _ => "NB".to_string() };
                let nb_btn = if self.rx2_nb_level > 0 {
                    egui::Button::new(RichText::new(&rx2_nb_label).strong())
                        .fill(Color32::from_rgb(100, 160, 230))
                } else {
                    egui::Button::new(&rx2_nb_label)
                };
                if ui.add(nb_btn).on_hover_text("Noise Blanker (click to cycle).").clicked() {
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
                if ui.add(anf_btn).on_hover_text("Auto Notch Filter.").clicked() {
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
        // Filter edge drag - always send both low+high (server expects pair)
        {
            use sdr_remote_core::protocol::ControlId;
            let drag_lo: Option<i32> = ctx.memory(|mem| mem.data.get_temp(egui::Id::new("spectrum_filter_low")));
            let drag_hi: Option<i32> = ctx.memory(|mem| mem.data.get_temp(egui::Id::new("spectrum_filter_high")));
            // In symmetric modes (AM/SAM/DSB/FM/DRM) Thetis forces ±W around the
            // carrier - dragging one edge there must mirror to both, otherwise
            // TL2 shows an asymmetric band Thetis can't honour and narrowing one
            // edge appears to do nothing (Thetis keeps the wider side).
            let sym = is_symmetric_mode(self.mode);
            if let Some(hz) = drag_lo {
                if sym {
                    let w = hz.abs();
                    self.filter_low_hz = -w;
                    self.filter_high_hz = w;
                } else {
                    self.filter_low_hz = hz;
                }
                let _ = self.cmd_tx.send(Command::SetControl(ControlId::FilterLow, self.filter_low_hz as i16 as u16));
                let _ = self.cmd_tx.send(Command::SetControl(ControlId::FilterHigh, self.filter_high_hz as i16 as u16));
                self.filter_changed_at = Some(std::time::Instant::now());
                ctx.memory_mut(|mem| { mem.data.remove::<i32>(egui::Id::new("spectrum_filter_low")); });
            }
            if let Some(hz) = drag_hi {
                if sym {
                    let w = hz.abs();
                    self.filter_low_hz = -w;
                    self.filter_high_hz = w;
                } else {
                    self.filter_high_hz = hz;
                }
                let _ = self.cmd_tx.send(Command::SetControl(ControlId::FilterLow, self.filter_low_hz as i16 as u16));
                let _ = self.cmd_tx.send(Command::SetControl(ControlId::FilterHigh, self.filter_high_hz as i16 as u16));
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
        // PATCH-tl2-rx2-spectrum-filter-drag-isolation: RX2 filter-edge drag
        // reader, spiegelt het RX1-pad (regel ~3290). Schreef voorheen naar
        // dezelfde globale "spectrum_filter_low/high" keys waardoor de
        // RX1-reader de update als RX1-filter doorgaf; nu per-channel
        // keys via SpectrumPlotConfig. Server expects de andere edge mee
        // in elke filter-update (low+high pair).
        use sdr_remote_core::protocol::ControlId;
        let drag_lo: Option<i32> = ctx.memory(|mem| mem.data.get_temp(egui::Id::new("rx2_spectrum_filter_low")));
        let drag_hi: Option<i32> = ctx.memory(|mem| mem.data.get_temp(egui::Id::new("rx2_spectrum_filter_high")));
        // Symmetric modes mirror a dragged edge to both sides (see RX1 above).
        let sym = is_symmetric_mode(self.rx2_mode);
        if let Some(hz) = drag_lo {
            if sym {
                let w = hz.abs();
                self.rx2_filter_low_hz = -w;
                self.rx2_filter_high_hz = w;
            } else {
                self.rx2_filter_low_hz = hz;
            }
            let _ = self.cmd_tx.send(Command::SetControl(ControlId::Rx2FilterLow, self.rx2_filter_low_hz as i16 as u16));
            let _ = self.cmd_tx.send(Command::SetControl(ControlId::Rx2FilterHigh, self.rx2_filter_high_hz as i16 as u16));
            self.rx2_filter_changed_at = Some(std::time::Instant::now());
            ctx.memory_mut(|mem| { mem.data.remove::<i32>(egui::Id::new("rx2_spectrum_filter_low")); });
        }
        if let Some(hz) = drag_hi {
            if sym {
                let w = hz.abs();
                self.rx2_filter_low_hz = -w;
                self.rx2_filter_high_hz = w;
            } else {
                self.rx2_filter_high_hz = hz;
            }
            let _ = self.cmd_tx.send(Command::SetControl(ControlId::Rx2FilterLow, self.rx2_filter_low_hz as i16 as u16));
            let _ = self.cmd_tx.send(Command::SetControl(ControlId::Rx2FilterHigh, self.rx2_filter_high_hz as i16 as u16));
            self.rx2_filter_changed_at = Some(std::time::Instant::now());
            ctx.memory_mut(|mem| { mem.data.remove::<i32>(egui::Id::new("rx2_spectrum_filter_high")); });
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
                ).on_hover_text("Spectrum reference level in dB.").changed() {
                    self.save_full_config();
                }
                if ui.checkbox(&mut self.rx2_auto_ref_enabled, "Auto").on_hover_text("Automatically track the reference level.").changed() {
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
                ).on_hover_text("Spectrum vertical range in dB.").changed() {
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
                // TL2-1 ctun-auto-recenter: same zoom-min logic als RX1 (per-RX onafhankelijk
                // zoom maar zelfde vink-clamp policy).
                let zoom_min_rx2: f32 = if self.allow_zoom_below_2x { 1.0 } else { 2.0 };
                if self.rx2_spectrum_zoom < zoom_min_rx2 {
                    self.rx2_spectrum_zoom = zoom_min_rx2;
                }
                let zoom_changed = ui.add(egui::Slider::new(&mut self.rx2_spectrum_zoom, zoom_min_rx2..=1024.0)
                    .logarithmic(true)
                    .custom_formatter(|v, _| format!("{:.0}x", v))
                ).on_hover_text("Spectrum zoom factor.").changed();
                if zoom_changed {
                    let max_pan = (0.5 - 0.5 / self.rx2_spectrum_zoom) * 0.05;
                    self.rx2_spectrum_pan = self.rx2_spectrum_pan.clamp(-max_pan, max_pan);
                }
                ui.label("Pan:");
                let max_pan = if self.rx2_spectrum_zoom > 1.01 { (0.5 - 0.5 / self.rx2_spectrum_zoom) * 0.05 } else { 0.0 };
                let pan_changed = ui.add(egui::Slider::new(&mut self.rx2_spectrum_pan, -max_pan..=max_pan)
                    .custom_formatter(|v, _| format!("{:+.2}", v))
                ).on_hover_text("Pan inside the zoomed spectrum view.").changed();
                ui.label("WF:");
                if ui.add(egui::Slider::new(&mut self.rx2_waterfall_contrast, 0.3..=3.0)
                    .logarithmic(true)
                    .custom_formatter(|v, _| format!("{:.1}", v))
                ).on_hover_text("Waterfall contrast.").changed() {
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
            let spec_h = (spec_area * 0.50).max(100.0);
            let wf_h = (spec_area * 0.50).max(80.0);

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
        // Bump UI frame id - stempelt alle UiEvents met monotone frame-id voor
        // timeline-correlatie.
        controls::begin_frame();

        // Clear per-frame flags
        ctx.memory_mut(|mem| mem.data.remove::<bool>(egui::Id::new("freq_scroll_consumed")));

        // Base egui visuals per selected theme (step 1). Classic reproduces the original
        // light-grey scheme; Dark is the tuned dark scheme. Colours that read from
        // ui.visuals() follow automatically; hard-coded ones are migrated in later steps.
        theme::apply_visuals(ctx, self.theme_variant, &self.theme_custom);

        self.sync_state();
        self.process_midi_events();

        // PATCH-4: first-run / re-launched wizard owns the viewport
        // while present. Render it edge-to-edge and short-circuit the
        // rest of update() - none of the regular UI panes are valid
        // until the wizard exits.
        if self.wizard_state.is_some() {
            let lang = if self.ui_language == "nl" {
                sdr_remote_logic::i18n::Lang::Nl
            } else {
                sdr_remote_logic::i18n::Lang::En
            };
            let outcome = egui::CentralPanel::default()
                .show(ctx, |ui| {
                    let st = self.wizard_state.as_mut().expect("checked above");
                    wizard::render_wizard(
                        ui,
                        st,
                        &self.cmd_tx,
                        &self.state_rx,
                        self.mdns_browse.as_ref(),
                        lang,
                    )
                })
                .inner;
            match outcome {
                wizard::WizardOutcome::Continue => {}
                wizard::WizardOutcome::SkipToManual => {
                    // No config write - wizard re-arms on next launch.
                    self.wizard_state = None;
                }
                wizard::WizardOutcome::Finished => {
                    // Connect succeeded; persist server+password and bump
                    // successful_connects so we don't re-arm next launch.
                    if let Some(ref st) = self.wizard_state {
                        self.server_input = st.server_input.clone();
                        self.password_input = st.password_input.clone();
                    }
                    self.save_full_config();
                    crate::ui::config::mark_successful_connect();
                    self.wizard_state = None;
                }
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
            return;
        }

        // PATCH-4: bump the successful-connect counter the first time the
        // user reaches Connected (incl. when they skipped the wizard).
        // Idempotent - mark_successful_connect() no-ops past 0.
        if self.connected {
            crate::ui::config::mark_successful_connect();
        }

        // Sync frequency and mute state to the selected WebSDR target.
        let (websdr_freq_hz, websdr_mode) = self.catsync_target_freq_mode();
        self.catsync.sync_freq(websdr_freq_hz, websdr_mode);
        self.catsync.update_mute(self.catsync_target_tx_active());

        // (TX spectrum override sets ref/range directly in PTT handler)

        // Track main-window geometry for persistence. Position is gated
        // on pointer-up like popouts to avoid disk-I/O stalls during a drag
        // (which causes the OS to oscillate the window between frames).
        // Grootte via screen_rect (altijd beschikbaar; i.viewport().inner_rect kan
        // None zijn op Windows/eframe -> capture vuurde dan nooit).
        {
            let sr = ctx.screen_rect();
            let (w, h) = (sr.width(), sr.height());
            if (w - self.window_w).abs() > 5.0 || (h - self.window_h).abs() > 5.0 {
                self.window_w = w;
                self.window_h = h;
                self.main_geom_dirty = true;
            }
        }
        if let Some(outer) = ctx.input(|i| i.viewport().outer_rect) {
            let np = outer.min;
            if self.main_window_pos.map_or(true, |p| (p.x - np.x).abs() > 5.0 || (p.y - np.y).abs() > 5.0) {
                self.main_window_pos = Some(np);
                self.main_geom_dirty = true;
            }
        }
        // Schrijf de eind-geometrie weg zodra de muis los is (niet tijdens de drag:
        // per-frame save tijdens slepen geeft I/O-stalls + window-oscillatie). De
        // dirty-vlag overbrugt het gat: de wijziging valt tijdens pointer-down, de
        // save vuurt op de eerste pointer-up frame daarna.
        if self.main_geom_dirty && !ctx.input(|i| i.pointer.any_down()) {
            self.save_full_config();
            self.main_geom_dirty = false;
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
            // VFO A popout without VFO B -> mute VFO B
            if self.spectrum_popout && !self.rx2_popout && self.vfo_b_volume > 0.001 {
                self.vfo_b_volume = 0.001;
                let _ = self.cmd_tx.send(Command::SetVfoBVolume(0.001));
            }
        }

        // Push new waterfall data (always, before rendering).
        // Tuning-latch op view-data verwijderd (experiment 2026-05-06): de
        // per-row absolute mapping in render_waterfall (spectrum.rs:646)
        // gebruikt elke row's eigen center_hz + span_hz. Late detail-FFT-rows
        // worden dus automatisch op hun werkelijke freq-positie gerenderd -
        // 1-2 rows kort op verschoven plek tijdens snel tunen, daarna self-
        // correcting. Voorheen werd view-data weggegooid tijdens tuning-
        // latch zodat alleen full-DDC (grover) in de row kwam.
        self.waterfall.push(
            &self.full_spectrum_bins, self.full_spectrum_center_hz,
            self.full_spectrum_span_hz, self.full_spectrum_sequence,
            &self.spectrum_bins, self.spectrum_center_hz, self.spectrum_span_hz,
        );

        // Push RX2 waterfall data - zelfde principe als RX1.
        self.rx2_waterfall.push(
            &self.rx2_full_spectrum_bins, self.rx2_full_spectrum_center_hz,
            self.rx2_full_spectrum_span_hz, self.rx2_full_spectrum_sequence,
            &self.rx2_spectrum_bins, self.rx2_spectrum_center_hz, self.rx2_spectrum_span_hz,
        );

        // Sticky top panel: PTT button + local volume (always visible)
        egui::TopBottomPanel::top("ptt_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                // Is de actuele Amplitec-A positie RX-only? De client weet dit
                // zelf uit de power-cap tabel (switch_a + tx_blocked), dus geen
                // server-push nodig. Op zo'n positie blokkeren we de PTT-knop
                // én onderdrukken we spatiebalk/MIDI-PTT (zie new_ptt hieronder).
                let current_pos_rx_only = (1..=6u8).contains(&self.amplitec_switch_a)
                    && self.amplitec_power_tx_blocked
                        [self.amplitec_switch_a.saturating_sub(1) as usize];

                // PTT button (compact for top bar)
                let (ptt_color, ptt_text, ptt_locked) = if current_pos_rx_only {
                    (Color32::from_rgb(120, 50, 50), "RX only", true)
                } else if self.other_tx {
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
                if current_pos_rx_only {
                    response.clone().on_hover_text(
                        "Active Amplitec antenna is RX-only - transmit is blocked",
                    );
                }

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
                // RX-only Amplitec positie: onderdruk de client-PTT volledig -
                // ook spatiebalk en MIDI. Reset bovendien de toggle-states
                // (mouse/MIDI), anders blijft een tijdens RX-only ingedrukte
                // toggle "aan" hangen en gaat 'ie alsnog zenden zodra we van
                // de RX-only positie wegschakelen. Spatiebalk is real-time
                // (key_down) en wordt door de `&&`-conditie afgevangen.
                if current_pos_rx_only {
                    self.mouse_ptt = false;
                    self.midi_ptt = false;
                }
                let new_ptt = (self.mouse_ptt || space_held || self.midi_ptt)
                    && !current_pos_rx_only;
                if new_ptt != self.ptt {
                    self.midi.send_led(crate::midi::MidiAction::Ptt, new_ptt);
                    // TX spectrum override
                    if new_ptt {
                        // Entering TX: save ref, range, auto - then set TX defaults
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
                self.apply_ptt_spike_protection(false, new_ptt);
                let _ = self.cmd_tx.send(Command::SetPtt(new_ptt));
                self.ptt = new_ptt;

                // Tune button (visible when tuner available on the active antenna).
                // Multi-tuner note: stale-detection is handled SERVER-side now -
                // the server tracks per-tuner tune frequency and already broadcasts
                // state = IDLE when the VFO has drifted >25 kHz from the active
                // tuner's last-tune freq. Client-side stale check used to apply a
                // second filter on top, but its `tuner_tune_freq` only updated on
                // TUNING->DONE_OK transitions, so an Amplitec switch (which sees
                // IDLE->DONE_OK without TUNING) left the client comparing against
                // the wrong tuner's freq. Drop the client-side gate and trust the
                // server state directly.
                if self.tuner_can_tune && self.tuner_connected {
                    let olive_green = Color32::from_rgb(120, 160, 40);
                    let (tune_color, tune_text) = match self.tuner_state {
                        1 => (Color32::from_rgb(60, 120, 220), "Tune..."),  // Tuning = blue
                        2 => (Color32::from_rgb(50, 180, 50), "Tune OK"),  // Done OK = green
                        5 => (olive_green, "Tune ~"),  // Done assumed = olive green
                        3 | 4 => (Color32::from_rgb(220, 160, 40), "Tune X"),  // Timeout/Aborted = orange
                        _ => (Color32::from_rgb(80, 80, 80), "Tune"),  // Idle = grey
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

        // Thetis volume/RX2 controls + Connect row (between PTT bar and tabs).
        egui::TopBottomPanel::top("vol_rx2_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if self.thetis_configured {
                    // Volume slider: role depends on popout state.
                    let both_popout = self.spectrum_popout && self.rx2_popout;
                    if both_popout {
                        ui.label("Master:");
                        let slider = egui::Slider::new(&mut self.local_volume, 0.001..=1.0)
                            .logarithmic(true)
                            .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
                        if ui.add(slider).changed() {
                            let _ = self.cmd_tx.send(Command::SetLocalVolume(self.local_volume));
                            self.save_full_config();
                        }
                    } else {
                        ui.label("VFO A:");
                        let slider = egui::Slider::new(&mut self.vfo_a_volume, 0.001..=1.0)
                            .logarithmic(true)
                            .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
                        if ui.add(slider).changed() {
                            let _ = self.cmd_tx.send(Command::SetVfoAVolume(self.vfo_a_volume));
                            self.save_full_config();
                        }
                    }

                    ui.separator();

                    // RX2 toggle.
                    let rx2_btn = if self.rx2_enabled {
                        egui::Button::new(RichText::new("RX2").size(12.0).strong())
                            .fill(Color32::from_rgb(100, 160, 230))
                    } else {
                        egui::Button::new(RichText::new("RX2").size(12.0))
                    };
                    if ui.add(rx2_btn).on_hover_text("Enable/disable RX2.").clicked() {
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
                } else {
                    self.rx2_popout = false;
                    self.show_vrx = false;
                }

                // Connect/Disconnect button + status
                if self.connected {
                    if ui.button("Disconnect").clicked() {
                        let _ = self.cmd_tx.send(Command::Disconnect);
                        self.connected = false;
                        self.catsync.force_unmute();
                    }
                    ui.colored_label(Color32::GREEN, "Connected");
                } else {
                    // PATCH-1 smoke-test fix (2026-05-12): when we are mid-auth in
                    // AwaitingTotp, Connect must not be clickable - the user should
                    // use the Verify-button on the 2FA input. A second Connect-press
                    // would otherwise pull the engine back to "Connecting..." while
                    // the server still has an active PendingTotp session, which
                    // would never recover.
                    let in_awaiting_totp = matches!(
                        self.state_rx.borrow().connect_status,
                        sdr_remote_logic::state::ConnectStatus::AwaitingTotp
                    );
                    let in_connecting = matches!(
                        self.state_rx.borrow().connect_status,
                        sdr_remote_logic::state::ConnectStatus::Connecting
                    );
                    let can_connect = !self.password_input.is_empty()
                        && !in_awaiting_totp
                        && !in_connecting;
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
                        // In relay-modus is er geen server-IP: stuur een placeholder-label
                        // (de Relay-transport negeert het adres; de engine skipt de DNS-check).
                        let connect_addr = if self.relay_external {
                            format!("relay:{}", self.relay_station)
                        } else {
                            self.server_input.clone()
                        };
                        let _ = self.cmd_tx.send(Command::Connect(connect_addr, pw));
                        self.save_full_config();
                    }
                    ui.colored_label(Color32::RED, "Disconnected");
                }
                if self.audio_error {
                    ui.colored_label(Color32::from_rgb(255, 165, 0), "Audio error");
                }
                // PATCH-1 smoke-test fix (2026-05-13): auto-switch to the
                // Server tab when connect_status transitions into a state
                // that demands user attention (AwaitingTotp, Failed). Without
                // this the user can be on the Radio tab and never see the
                // 2FA prompt or error message until they manually navigate.
                {
                    use sdr_remote_logic::state::ConnectStatus;
                    let current = self.state_rx.borrow().connect_status.clone();
                    let needs_attention = matches!(
                        current,
                        ConnectStatus::AwaitingTotp | ConnectStatus::Failed(_)
                    );
                    let was_attention = matches!(
                        self.last_connect_status,
                        ConnectStatus::AwaitingTotp | ConnectStatus::Failed(_)
                    );
                    if needs_attention && !was_attention {
                        self.active_tab = Tab::Server;
                    }
                    self.last_connect_status = current;
                }
                // PATCH-1 smoke-test fix (2026-05-12 #2): show connect-status
                // error globally in the top bar so users on any tab see it,
                // not just on the Server tab. Avoids the "press Connect on
                // Radio tab -> silent failure" UX gap.
                {
                    use sdr_remote_logic::i18n::{connect_status_text, Lang};
                    use sdr_remote_logic::state::ConnectStatus;
                    let connect_status = self.state_rx.borrow().connect_status.clone();
                    let lang = if self.ui_language == "nl" { Lang::Nl } else { Lang::En };
                    // PATCH-1 smoke-test fix (2026-05-13): top-bar headline 16pt bold
                    // so it stands out - operator feedback: was smaller than other UI.
                    match &connect_status {
                        ConnectStatus::Failed(_) => {
                            let (headline, _) = connect_status_text(&connect_status, lang, sdr_remote_logic::i18n::Platform::Desktop);
                            ui.label(egui::RichText::new(headline).size(16.0).strong().color(Color32::from_rgb(220, 40, 40)));
                        }
                        ConnectStatus::AwaitingTotp => {
                            let (headline, _) = connect_status_text(&connect_status, lang, sdr_remote_logic::i18n::Platform::Desktop);
                            ui.label(egui::RichText::new(headline).size(16.0).strong().color(Color32::from_rgb(255, 165, 0)));
                        }
                        ConnectStatus::Connecting => {
                            let (headline, _) = connect_status_text(&connect_status, lang, sdr_remote_logic::i18n::Platform::Desktop);
                            ui.label(egui::RichText::new(headline).size(16.0).strong().color(Color32::from_rgb(180, 180, 60)));
                        }
                        _ => {}
                    }
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if !self.thetis_configured && matches!(self.active_tab, Tab::Radio | Tab::Thetis) {
                self.active_tab = Tab::Devices;
            }
            ui.horizontal(|ui| {
                if self.thetis_configured {
                    ui.selectable_value(&mut self.active_tab, Tab::Radio, "Radio");
                    ui.selectable_value(&mut self.active_tab, Tab::Thetis, "Thetis");
                }
                ui.selectable_value(&mut self.active_tab, Tab::Server, "Server");
                ui.selectable_value(&mut self.active_tab, Tab::Devices, "Devices");
                ui.selectable_value(&mut self.active_tab, Tab::Midi, "MIDI");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button(RichText::new("About").size(11.0)).clicked() {
                        self.show_about = !self.show_about;
                    }
                    ui.toggle_value(&mut self.show_log, RichText::new("Log").size(11.0));
                    if self.thetis_configured {
                        let prev_show_vrx = self.show_vrx;
                        ui.toggle_value(&mut self.show_vrx, RichText::new("VRX").size(11.0));
                        if prev_show_vrx != self.show_vrx {
                            log::info!("VRX popout toggle: {} -> {}", prev_show_vrx, self.show_vrx);
                        }
                        if prev_show_vrx && !self.show_vrx {
                            // Toggle-driven close mirrors the X-button close path
                            // so window geometry persists either way.
                            self.vrx_popout_init_applied = false;
                            self.save_full_config();
                        }
                    } else {
                        self.show_vrx = false;
                    }
                });
            });
            ui.separator();

            if self.active_tab == Tab::Devices {
                self.render_devices_screen(ui);
            } else if self.active_tab == Tab::Thetis {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    self.render_thetis_screen(ui);
                });
            } else if self.active_tab == Tab::Server {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    self.render_server_screen(ui);
                });
            } else if self.active_tab == Tab::Midi {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    self.render_midi_screen(ui);
                });
            } else {
            // Wrap the Radio tab content in a ScrollArea so the panel stays
            // usable when the user expands the Diversity panel (or sets a
            // tall spectrum_total_h) and the total content height exceeds
            // the window. Spectrum height itself is fixed at
            // `self.spectrum_total_h` (set via the H: slider / drag-handle
            // below the waterfall) - without the ScrollArea wrapper any
            // overflow would push content off-screen.
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {

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

            // Mode buttons (via controls::render_mode_selector - Basic density = 4 modes)
            // Tab::Radio mode-block had voorheen `ui.add(btn)` zonder
            // connected-guard.
            let ptt_active = self.ptt;
            let mode_action = self.with_rx_ctx(
                controls::RxChannel::Rx1,
                controls::UiDensity::Basic,
                controls::UiSurface::MainTab,
                |ctx| {
                    ui.add_enabled_ui(!ptt_active, |ui| controls::render_mode_selector(ui, ctx)).inner.map(|c| {
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
                    if ui.add_enabled(idx > 0, minus_btn).on_hover_text("Narrow the filter one step.").clicked() {
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
                    if ui.add_enabled(idx < presets.len() - 1, plus_btn).on_hover_text("Widen the filter one step.").clicked() {
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
                if ui.add(nr_btn).on_hover_text("Noise Reduction (click to cycle level).").clicked() {
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
                if ui.add(anf_btn).on_hover_text("Auto Notch Filter.").clicked() {
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
                if ui.add(agc_btn).on_hover_text("Microphone AGC on/off.").clicked() {
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
            if helpers::chevron_label(
                ui,
                self.collapse_diversity,
                RichText::new("Diversity").strong().size(14.0),
            )
            .clicked()
            {
                self.collapse_diversity = !self.collapse_diversity;
                self.save_full_config();
            }
            if self.collapse_diversity {
                ui.indent("diversity_body", |ui| {
                    self.render_diversity(ui);
                });
            }

            }); // end of Radio-tab ScrollArea
            } // end of Radio tab
        });

        // Pop-out viewports: joined or separate. Both gates intentionally
        // omit a bins/connected check so the popout opens at its saved
        // geometry from the very first frame - same UX as the Yaesu popout.
        // The popout's content gates internally on bin-availability and
        // shows a "Waiting for ..." placeholder pre-connect.
        let show_rx1_popout = self.spectrum_popout && self.spectrum_enabled;
        let show_rx2_popout = self.rx2_popout && self.rx2_enabled;

        if show_rx1_popout && show_rx2_popout && self.popout_joined {
            // Joined mode: single combined window with RX1 on top, RX2 below
            let vb = Self::apply_popout_geometry(
                ViewportBuilder::default().with_title("ThetisLink - RX1 + RX2"),
                self.popout_joined_pos,
                self.popout_joined_size,
                [900.0, 900.0],
                Self::viewport_native_ppp(ctx),
                &mut self.popout_joined_init_applied,
            );
            ctx.show_viewport_immediate(
                ViewportId::from_hash_of("spectrum_popout"),
                vb,
                |ctx, _class| {
                    if ctx.input(|i| i.viewport().close_requested()) {
                        self.spectrum_popout = false;
                        self.rx2_popout = false;
                        self.popout_joined_init_applied = false;
                        return;
                    }
                    if Self::track_popout_geometry(ctx, &mut self.popout_joined_pos, &mut self.popout_joined_size) {
                        self.save_full_config();
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

                        // Spectrums stacked: RX1 on top, RX2 below. Pre-connect both
                        // halves show their respective "Waiting for ..." placeholder so
                        // the layout is recognisable as two stacked RX-panes even before
                        // any spectrum bins arrive (RX2 already had this gate inside
                        // render_rx2_spectrum_only; RX1 gets it here to mirror that).
                        let total_w = ui.available_width();
                        let available = ui.available_height();
                        let half = (available - 4.0) / 2.0;
                        ui.allocate_ui(egui::vec2(total_w, half), |ui| {
                            if !self.spectrum_bins.is_empty() {
                                self.render_spectrum_content(ui, ctx, 0.0, true);
                            } else {
                                ui.centered_and_justified(|ui| {
                                    ui.label(RichText::new("Waiting for RX1 spectrum data...").weak());
                                });
                            }
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
                        if self.popout_meter_analog {
                            // Analog: 60×28 button just left of RX1 meter,
                            // top-aligned - mirrors the Split button on RX2.
                            // Default styling (no fill) - blue is reserved for
                            // toggled-on state in this app; A<>B is momentary.
                            // Use a centered child-UI so the button text is
                            // horizontally centered (matches Split).
                            let btn_w = 60.0_f32;
                            let btn_h = 28.0_f32;
                            let pos = egui::pos2(r1.left() - btn_w - 4.0, r1.top());
                            egui::Area::new(egui::Id::new("vfo_swap_joined_analog"))
                                .fixed_pos(pos)
                                .order(egui::Order::Foreground)
                                .interactable(true)
                                .show(ctx, |ui| {
                                    let btn_rect = egui::Rect::from_min_size(pos, egui::vec2(btn_w, btn_h));
                                    let mut btn_ui = ui.new_child(
                                        egui::UiBuilder::new()
                                            .max_rect(btn_rect)
                                            .layout(egui::Layout::top_down(egui::Align::Center))
                                    );
                                    let btn = egui::Button::new(RichText::new("A<>B").strong())
                                        .min_size(egui::vec2(btn_w, btn_h));
                                    if btn_ui.add_enabled(self.connected, btn).on_hover_text("Swap VFO A and VFO B.").clicked() {
                                        let _ = self.cmd_tx.send(Command::SetControl(ControlId::VfoSwap, 2));
                                    }
                                });
                        } else {
                            // Bar mode: keep original between-meters placement.
                            // Separate Area ID from the analog one to avoid
                            // egui caching a previous frame's larger size
                            // constraint and triggering character-wrap of
                            // "A<>B" when toggling Analog -> Bar.
                            let center_x = (r1.right() + r2.left()) / 2.0;
                            let center_y = (r1.center().y + r2.center().y) / 2.0;
                            let pos = egui::pos2(center_x - 23.0, center_y - 10.0);
                            egui::Area::new(egui::Id::new("vfo_swap_joined_bar"))
                                .fixed_pos(pos)
                                .order(egui::Order::Foreground)
                                .interactable(true)
                                .show(ctx, |ui| {
                                    let btn = egui::Button::new(RichText::new("A<>B").size(10.0))
                                        .min_size(egui::vec2(40.0, 18.0));
                                    if ui.add_enabled(self.connected, btn).on_hover_text("Swap VFO A and VFO B.").clicked() {
                                        let _ = self.cmd_tx.send(Command::SetControl(ControlId::VfoSwap, 2));
                                    }
                                });
                        }
                    }
                },
            );
        } else {
            // Separate mode: individual windows
            if show_rx1_popout {
                let vb = Self::apply_popout_geometry(
                    ViewportBuilder::default().with_title("ThetisLink - RX1 / VFO-A"),
                    self.spectrum_popout_pos,
                    self.spectrum_popout_size,
                    [900.0, 600.0],
                    Self::viewport_native_ppp(ctx),
                    &mut self.spectrum_popout_init_applied,
                );
                ctx.show_viewport_immediate(
                    ViewportId::from_hash_of("spectrum_popout"),
                    vb,
                    |ctx, _class| {
                        if ctx.input(|i| i.viewport().close_requested()) {
                            self.spectrum_popout = false;
                            self.spectrum_popout_init_applied = false;
                            return;
                        }
                        if Self::track_popout_geometry(ctx, &mut self.spectrum_popout_pos, &mut self.spectrum_popout_size) {
                            self.save_full_config();
                        }
                        egui::CentralPanel::default().show(ctx, |ui| {
                            // Split/Join segmented right-aligned (only visible when RX2 is also popped out)
                            if show_rx2_popout {
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                                    self.render_split_join_segmented(ui, false, None);
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
                                            .on_hover_text("Swap VFO A and VFO B.")
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
                let vb = Self::apply_popout_geometry(
                    ViewportBuilder::default().with_title("ThetisLink - RX2 / VFO-B"),
                    self.rx2_popout_pos,
                    self.rx2_popout_size,
                    [900.0, 600.0],
                    Self::viewport_native_ppp(ctx),
                    &mut self.rx2_popout_init_applied,
                );
                ctx.show_viewport_immediate(
                    ViewportId::from_hash_of("rx2_popout"),
                    vb,
                    |ctx, _class| {
                        if ctx.input(|i| i.viewport().close_requested()) {
                            self.rx2_popout = false;
                            self.rx2_popout_init_applied = false;
                            return;
                        }
                        if Self::track_popout_geometry(ctx, &mut self.rx2_popout_pos, &mut self.rx2_popout_size) {
                            self.save_full_config();
                        }
                        egui::CentralPanel::default().show(ctx, |ui| {
                            // Split/Join segmented right-aligned (only visible when RX1 is also popped out)
                            if show_rx1_popout {
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                                    self.render_split_join_segmented(ui, false, None);
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
                                            .on_hover_text("Swap VFO A and VFO B.")
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
            let title = self.yaesu_window_title(0);
            let vb = Self::apply_popout_geometry(
                ViewportBuilder::default().with_title(title),
                self.yaesu_popout_pos,
                self.yaesu_popout_size,
                [465.0, 335.0],
                Self::viewport_native_ppp(ctx),
                &mut self.yaesu_popout_init_applied,
            );
            ctx.show_viewport_immediate(
                ViewportId::from_hash_of("yaesu_popout"),
                vb,
                |ctx, _class| {
                    if ctx.input(|i| i.viewport().close_requested()) {
                        self.yaesu_popout = false;
                        self.yaesu_popout_init_applied = false;
                        self.save_full_config();
                        return;
                    }
                    if Self::track_popout_geometry(ctx, &mut self.yaesu_popout_pos, &mut self.yaesu_popout_size) {
                        self.save_full_config();
                    }
                    // Fixed PTT button at bottom
                    egui::TopBottomPanel::bottom("yaesu_ptt_panel").show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            // PTT button - locked when other client is transmitting
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
                                    self.apply_ptt_spike_protection(true, new_tx);
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
                                    self.apply_ptt_spike_protection(true, pressing);
                                    let _ = self.cmd_tx.send(Command::SetYaesuPtt(pressing));
                                }
                            }

                            ui.separator();

                            ui.label("Volume:");
                            let slider = egui::Slider::new(&mut self.yaesu_volume, 0.001..=1.0)
                                .logarithmic(true)
                                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
                            let resp = ui.add_sized([140.0, 16.0], slider);
                            if resp.changed() {
                                let _ = self.cmd_tx.send(Command::SetYaesuVolume(self.yaesu_volume));
                            }
                            if resp.drag_stopped() {
                                self.save_full_config();
                            }
                        });
                    });
                    egui::CentralPanel::default().show(ctx, |ui| {
                        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                            self.render_yaesu_popout(ui);
                        });
                    });
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                },
            );
        }

        // Slot-1 (FTX-1) eigen popout-window - los van het 991A-window, geroute
        // naar slot 1. Verschijnt zodra slot 1 *enabled* is (config), net als het
        // 991A-window op yaesu_enabled - dus direct bij opstart, ook zonder connect.
        if self.yaesu2_popout && (self.yaesu2_enabled || self.yaesu2_connected) {
            let title = self.yaesu_window_title(1);
            let vb = Self::apply_popout_geometry(
                ViewportBuilder::default().with_title(title),
                self.yaesu2_popout_pos,
                self.yaesu2_popout_size,
                [465.0, 335.0],
                Self::viewport_native_ppp(ctx),
                &mut self.yaesu2_popout_init_applied,
            );
            ctx.show_viewport_immediate(
                ViewportId::from_hash_of("yaesu2_popout"),
                vb,
                |ctx, _class| {
                    if ctx.input(|i| i.viewport().close_requested()) {
                        self.yaesu2_popout = false;
                        self.yaesu2_popout_init_applied = false;
                        self.save_ptt_config(); // persist window dicht-stand
                        return;
                    }
                    if Self::track_popout_geometry(ctx, &mut self.yaesu2_popout_pos, &mut self.yaesu2_popout_size) {
                        self.save_full_config();
                    }
                    egui::TopBottomPanel::bottom("yaesu2_ptt_panel").show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            let (ptt_color, ptt_text, ptt_locked) = if self.other_tx {
                                (Color32::from_rgb(200, 120, 0), "TX in use", true)
                            } else if self.yaesu2_tx_active {
                                (Color32::RED, "TX", false)
                            } else {
                                (Color32::from_rgb(60, 60, 60), "PTT", false)
                            };
                            let ptt_btn = egui::Button::new(
                                RichText::new(ptt_text).size(18.0).color(Color32::WHITE).strong(),
                            ).fill(ptt_color).min_size(egui::vec2(80.0, 40.0));
                            let response = ui.add_enabled(!ptt_locked, ptt_btn);
                            if self.yaesu2_ptt_toggle_mode {
                                if response.clicked() {
                                    let new_tx = !self.yaesu2_tx_active;
                                    self.apply_ptt_spike_protection(true, new_tx);
                                    let _ = self.cmd_tx.send(Command::SetYaesu2Ptt(new_tx));
                                }
                            } else {
                                let pressing = ui.input(|i| {
                                    i.pointer.primary_down()
                                        && response.rect.contains(i.pointer.interact_pos().unwrap_or(egui::Pos2::ZERO))
                                });
                                if pressing != self.yaesu2_mouse_ptt {
                                    self.yaesu2_mouse_ptt = pressing;
                                    self.apply_ptt_spike_protection(true, pressing);
                                    let _ = self.cmd_tx.send(Command::SetYaesu2Ptt(pressing));
                                }
                            }
                            ui.separator();
                            ui.label("Volume:");
                            let slider = egui::Slider::new(&mut self.yaesu2_volume, 0.001..=1.0)
                                .logarithmic(true)
                                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
                            let resp = ui.add_sized([140.0, 16.0], slider);
                            if resp.changed() {
                                let _ = self.cmd_tx.send(Command::SetYaesu2Volume(self.yaesu2_volume));
                            }
                            if resp.drag_stopped() {
                                self.save_full_config();
                            }
                        });
                    });
                    egui::CentralPanel::default().show(ctx, |ui| {
                        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                            self.render_yaesu2_panel(ui);
                        });
                    });
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                },
            );
        }

        // Handle spectrum interaction keys (fallback for main-window spectrum;
        // popout viewports handle their own keys inside the viewport closure)
        self.handle_rx2_spectrum_keys(ctx);
        self.handle_rx1_spectrum_keys(ctx);

        // VRX DDC-bucket tracking: detect ANY DDC center change of
        // ≥100 kHz (= bucket size), not just amateur-band boundaries.
        // CTUN re-centering within the same band still trips the
        // detector. When the bucket changes, save the previous freq
        // under the OLD bucket and restore the LAST-USED freq for the
        // new bucket. Defaults to VFO if no memory or restored value
        // falls outside the new DDC range.
        const BUCKET_HZ: u64 = 100_000;
        {
            let vrx1_center = if self.full_spectrum_center_hz > 0 {
                self.full_spectrum_center_hz as u64
            } else { self.frequency_hz };
            let vrx1_span = if self.full_spectrum_span_hz > 0 {
                self.full_spectrum_span_hz as u64
            } else { 384_000 };
            if vrx1_center > 0 {
                let cur_bucket = vrx1_center / BUCKET_HZ;
                let last_bucket = self.last_vrx1_ddc_center_hz / BUCKET_HZ;
                if cur_bucket != last_bucket && self.last_vrx1_ddc_center_hz > 0 {
                    if self.vrx1_freq_hz > 0 {
                        self.vrx1_freq_by_bucket.insert(last_bucket, self.vrx1_freq_hz);
                    }
                    let new_min = vrx1_center.saturating_sub(vrx1_span / 2);
                    let new_max = vrx1_center + vrx1_span / 2;
                    let restored = self.vrx1_freq_by_bucket.get(&cur_bucket).copied()
                        .filter(|&f| f >= new_min && f <= new_max)
                        .unwrap_or(self.frequency_hz);
                    // Commit de hersteld-freq alleen na een bevestigde send (geen drift).
                    if self.cmd_tx.send(Command::SetVrxFrequency(restored)).is_ok() {
                        self.vrx1_freq_hz = restored;
                    }
                }
                self.last_vrx1_ddc_center_hz = vrx1_center;
            }
        }
        {
            let vrx2_center = if self.rx2_full_spectrum_center_hz > 0 {
                self.rx2_full_spectrum_center_hz as u64
            } else { self.rx2_frequency_hz };
            let vrx2_span = if self.rx2_full_spectrum_span_hz > 0 {
                self.rx2_full_spectrum_span_hz as u64
            } else { 384_000 };
            if vrx2_center > 0 {
                let cur_bucket = vrx2_center / BUCKET_HZ;
                let last_bucket = self.last_vrx2_ddc_center_hz / BUCKET_HZ;
                if cur_bucket != last_bucket && self.last_vrx2_ddc_center_hz > 0 {
                    if self.vrx2_freq_hz > 0 {
                        self.vrx2_freq_by_bucket.insert(last_bucket, self.vrx2_freq_hz);
                    }
                    let new_min = vrx2_center.saturating_sub(vrx2_span / 2);
                    let new_max = vrx2_center + vrx2_span / 2;
                    let restored = self.vrx2_freq_by_bucket.get(&cur_bucket).copied()
                        .filter(|&f| f >= new_min && f <= new_max)
                        .unwrap_or(self.rx2_frequency_hz);
                    // Commit de hersteld-freq alleen na een bevestigde send (geen drift).
                    if self.cmd_tx.send(Command::SetVrx2Frequency(restored)).is_ok() {
                        self.vrx2_freq_hz = restored;
                    }
                }
                self.last_vrx2_ddc_center_hz = vrx2_center;
            }
        }

        // VRX joint popout window - VRX1 (left) listens on RX1 IQ +
        // VFO-A, VRX2 (right) on RX2 IQ + VFO-B. Audio is mixed into
        // the main playback alongside RX1 / RX2 / Yaesu.
        if self.show_vrx {
            // Seed absolute-freq fields with the current VFOs on first
            // open so the user sees the actual listen frequency instead
            // of "0.000000".
            if self.vrx1_freq_hz == 0 && self.frequency_hz > 0 {
                self.vrx1_freq_hz = self.frequency_hz;
                let _ = self.cmd_tx.send(Command::SetVrxFrequency(self.vrx1_freq_hz));
            }
            if self.vrx2_freq_hz == 0 && self.rx2_frequency_hz > 0 {
                self.vrx2_freq_hz = self.rx2_frequency_hz;
                let _ = self.cmd_tx.send(Command::SetVrx2Frequency(self.vrx2_freq_hz));
            }
            let vb = Self::apply_popout_geometry(
                ViewportBuilder::default().with_title("ThetisLink - VRX"),
                self.vrx_popout_pos,
                self.vrx_popout_size,
                [680.0, 520.0],
                Self::viewport_native_ppp(ctx),
                &mut self.vrx_popout_init_applied,
            );
            ctx.show_viewport_immediate(
                ViewportId::from_hash_of("vrx_popout"),
                vb,
                |ctx, _class| {
                    if ctx.input(|i| i.viewport().close_requested()) {
                        log::info!("VRX popout: close_requested fired -> show_vrx=false");
                        self.show_vrx = false;
                        self.vrx_popout_init_applied = false;
                        self.vrx1_waterfall_texture = None;
                        self.vrx2_waterfall_texture = None;
                        self.save_full_config();
                        return;
                    }
                    if Self::track_popout_geometry(ctx, &mut self.vrx_popout_pos, &mut self.vrx_popout_size) {
                        self.save_full_config();
                    }
                    egui::CentralPanel::default().show(ctx, |ui| {
                        {
                            // Compute DDC ranges per channel. center+span
                            // come from the spectrum packets - fall back
                            // to VFO ± 192 kHz before any spectrum has
                            // arrived so the input range is non-empty.
                            let vrx1_ddc_center: u64 = if self.full_spectrum_center_hz > 0 {
                                self.full_spectrum_center_hz as u64
                            } else { self.frequency_hz };
                            let vrx1_ddc_span: u64 = if self.full_spectrum_span_hz > 0 {
                                self.full_spectrum_span_hz as u64
                            } else { 384_000 };
                            let vrx1_min = vrx1_ddc_center.saturating_sub(vrx1_ddc_span / 2);
                            let vrx1_max = vrx1_ddc_center + vrx1_ddc_span / 2;
                            let vrx2_ddc_center: u64 = if self.rx2_full_spectrum_center_hz > 0 {
                                self.rx2_full_spectrum_center_hz as u64
                            } else { self.rx2_frequency_hz };
                            let vrx2_ddc_span: u64 = if self.rx2_full_spectrum_span_hz > 0 {
                                self.rx2_full_spectrum_span_hz as u64
                            } else { 384_000 };
                            let vrx2_min = vrx2_ddc_center.saturating_sub(vrx2_ddc_span / 2);
                            let vrx2_max = vrx2_ddc_center + vrx2_ddc_span / 2;

                            // ── Top: VRX1 + VRX2 controls naast elkaar (2 columns) ──
                            ui.columns(2, |cols| {
                                self.render_vrx_channel_controls(&mut cols[0], VrxChannel::Vrx1, vrx1_ddc_center, vrx1_min, vrx1_max);
                                self.render_vrx_channel_controls(&mut cols[1], VrxChannel::Vrx2, vrx2_ddc_center, vrx2_min, vrx2_max);
                            });

                            ui.separator();

                            // -- Bottom: VRX1 spectrum boven VRX2 spectrum (gedeelde renderer) --
                            let outer_w = ui.available_width();
                            let avail_h = ui.available_height();
                            let panel_h = ((avail_h - theme::TL_PANEL_GAP_Y) / 2.0).max(120.0);
                            ui.allocate_ui(egui::vec2(outer_w, panel_h), |ui| {
                                self.render_vrx_spectrum_panel(ui, ctx, VrxChannel::Vrx1, vrx1_min, vrx1_max);
                            });
                            ui.add_space(theme::TL_PANEL_GAP_Y);
                            ui.allocate_ui(egui::vec2(outer_w, panel_h), |ui| {
                                self.render_vrx_spectrum_panel(ui, ctx, VrxChannel::Vrx2, vrx2_min, vrx2_max);
                            });
                        }
                    });
                    // Read filter-edge memory writes from render_vrx_strip and dispatch
                    for (salt, vrx_id, lo_field, hi_field) in [
                        ("vrx1", 0u8, false, false),
                        ("vrx2", 1u8, false, false),
                    ] {
                        let _ = (lo_field, hi_field);
                        let lo_key = egui::Id::new(format!("{}_filter_low_hz", salt));
                        let hi_key = egui::Id::new(format!("{}_filter_high_hz", salt));
                        let new_lo: Option<i32> = ctx.memory(|m| m.data.get_temp(lo_key));
                        let new_hi: Option<i32> = ctx.memory(|m| m.data.get_temp(hi_key));
                        if new_lo.is_some() || new_hi.is_some() {
                            let cur_lo = if vrx_id == 0 { self.vrx1_filter_low_hz } else { self.vrx2_filter_low_hz };
                            let cur_hi = if vrx_id == 0 { self.vrx1_filter_high_hz } else { self.vrx2_filter_high_hz };
                            let final_lo = new_lo.unwrap_or(cur_lo);
                            let final_hi = new_hi.unwrap_or(cur_hi);
                            // Lokale filter-state + persist + het wissen van de pending
                            // drag-waarde alleen na een bevestigde send (dispatch-return-
                            // discipline). Faalt de send, dan blijft de drag-waarde staan
                            // voor een nieuwe poging i.p.v. stille UI/server-drift.
                            if self.cmd_tx.send(Command::SetVrxFilter(vrx_id, final_lo, final_hi)).is_ok() {
                                if vrx_id == 0 {
                                    self.vrx1_filter_low_hz = final_lo;
                                    self.vrx1_filter_high_hz = final_hi;
                                } else {
                                    self.vrx2_filter_low_hz = final_lo;
                                    self.vrx2_filter_high_hz = final_hi;
                                }
                                ctx.memory_mut(|m| { m.data.remove::<i32>(lo_key); m.data.remove::<i32>(hi_key); });
                                self.save_full_config();
                            }
                        }
                    }
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                },
            );
        }

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
                            ui.label("Remote control for Thetis SDR + Yaesu FT-991A / FTX-1 (dual-radio)");
                        });
                        ui.add_space(8.0);
                        ui.separator();

                        ui.label(RichText::new("Author").size(13.0).strong());
                        ui.label("Chiron van der Burgt - PA3GHM");

                        ui.add_space(6.0);
                        ui.label(RichText::new("Special Thanks").size(13.0).strong());
                        ui.label("Richie (ramdor) - Thetis SDR development, TCI protocol extensions");

                        ui.add_space(6.0);
                        ui.label(RichText::new("Protocols & External Services").size(13.0).strong());
                        ui.label("TCI - Expert Electronics / Thetis");
                        ui.label("DX Spider - DX cluster telnet protocol");
                        ui.label("HPSDR / OpenHPSDR Protocol 2");
                        ui.label("WebSDR (PA3FWM) / KiwiSDR - CatSync targets");
                        ui.label("ThetisLink Relay - self-hosted WebSocket + UDP relay (internet remote)");

                        ui.add_space(6.0);
                        ui.label(RichText::new("Hardware Support").size(13.0).strong());
                        egui::Grid::new("hw_grid").num_columns(2).spacing([12.0, 2.0]).show(ui, |ui| {
                            for (dev, iface) in [
                                ("ANAN 7000DLE", "TCI (via Thetis)"),
                                ("Yaesu FT-991A", "Serial CAT + USB Audio"),
                                ("Yaesu FTX-1", "Serial CAT + USB Audio"),
                                ("RF2K-S PA", "HTTP API"),
                                ("SPE Expert 1.3K-FA", "Serial"),
                                ("StockCorner JC-4s / JC-3s Tuner", "MCP2221A USB-HID"),
                                ("UltraBeam RCU-06", "Serial"),
                                ("Amplitec 6/2", "Serial"),
                                ("EA7HG Visual Rotor", "UDP"),
                                ("Yaesu G-1000DXC Rotor", "MCP2221A USB-HID"),
                                ("PstRotator (any supported rotor)", "XML over UDP"),
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
                            ("mcp2221-hal", "MCP2221A USB-HID (tuners, rotor)"),
                            ("rustls", "Relay TLS (wss)"),
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
                        ui.label("Based on the Thetis SDR lineage - see ATTRIBUTION.md");
                        ui.label("Third-party licenses & SBOM: see NOTICE.md, THIRD-PARTY-LICENSES.html");

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


























