// SPDX-License-Identifier: GPL-2.0-or-later

#![allow(dead_code)]

mod helpers;
mod meters;
mod window_placement;
mod arranger;
mod popouts;
mod app_state;
mod update;
mod sync_state;
mod vrx;
mod rx_controls;
mod spectrum_content;
mod persistence;
mod tuning;
mod spectrum;
mod channel_spectrum;
pub(crate) mod theme;
pub(crate) mod config;
mod devices;
mod yaesu_panel;
mod screens;
mod midi_screen;
mod server_screen;
mod diversity_screen;
mod thetis_screen;
mod wizard;
pub(crate) mod controls;
pub(crate) mod yaesu_memory;
pub(crate) mod yaesu_menu;
pub(crate) mod ftx1_ex_chart;

pub(crate) use helpers::*;
pub(crate) use meters::*;
pub(crate) use spectrum::*;
pub(crate) use arranger::{LayoutGrid, LayoutMemory, SnapWindow, LAYOUT_MEM_SLOTS};

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

use egui::{Color32, Pos2, RichText, Stroke, TextureHandle, Vec2};
use tokio::sync::{mpsc, watch};

use std::collections::{HashMap, VecDeque};

use sdr_remote_core::protocol::ControlId;
use sdr_remote_logic::commands::Command;
use sdr_remote_logic::state::RadioState;

use channel_spectrum::{ChannelId, ChannelSpectrum, SpectrumSnapshot};

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
    /// App-level UI-observability sink. Replaces the per-call-site `TracingSink`
    /// construction. Always `TracingSink` in prod; in test mode a `RecordingSink`
    /// can be set for assert-based tests.
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
    /// Last PTT state sent from the Yaesu pop-out (mouse OR spacebar combined),
    /// so the spacebar-in-window PTT and the mouse PTT don't fight each other.
    yaesu_ptt_last_sent: bool,
    yaesu2_ptt_last_sent: bool,
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
    /// The files the last Start wrote, as (source name, path), and which of
    /// them the Play button refers to.
    last_recorded: Vec<(String, String)>,
    /// Which of `last_recorded` the Play button will sound, one flag per
    /// entry. Everything ticked plays together.
    play_ticked: Vec<bool>,
    midi_ptt_toggle_mode: bool,  // independent MIDI PTT mode
    /// Diagnostic CAT monitor toggle (server logs every radio frame). Session
    /// state on purpose: it is for one investigation, not a saved preference.
    /// S-meter source: 0=Sig, 1=Avg (default), 2=MaxBin. Single setting shared
    /// by RX1 and RX2. Translated to a per-RX bitmap by the engine and pushed
    /// via ControlId::SmeterSources whenever it changes.
    smeter_source: u8,
    /// Checkbox in the Thetis tab: launch Thetis on the server PC when this
    /// client starts and the server reports Thetis is not running. Persistent.
    thetis_autostart: bool,
    /// One-shot latch for `thetis_autostart`: set once the launch has been
    /// sent this client run. Keeps a later deliberate power-off (or a failed
    /// launch) from being overridden on every reconnect.
    thetis_autostart_fired: bool,
    /// TL2-1 ctun-auto-recenter setup checkbox "Allow zoom below 2x (with smear during tune)".
    /// Default false -> zoom-min 2x. True -> zoom-min 1x allowed.
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
    play_volume: f32,     // Client-only WAV-playback ('Play') volume
    vfo_a_volume: f32,    // Client-only VFO A playback volume
    vfo_b_volume: f32,    // Client-only VFO B playback volume
    local_volume: f32,    // Client-only master volume
    /// UI scale (egui zoom factor), separate from the OS display scaling.
    ui_zoom: f32,
    /// Set when OUR value must be pushed into egui (startup, or the picker was
    /// used). Without it the update loop cannot tell "the user pressed Ctrl+-"
    /// from "we have not applied our stored value yet".
    ui_zoom_pending: bool,
    /// Master volume changed but not yet written to disk - saved on pointer-up.
    master_volume_dirty: bool,
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
    /// Server-reported radio capability: does the Thetis radio have a second
    /// receiver? Default true; false = server set to single-receiver -> RX2 +
    /// VRX2 shown nowhere (VRX1 hangs off RX1 and stays).
    rx2_present: bool,
    /// Does this server have a DX cluster at all? Server-reported; `true` until
    /// told otherwise, which is what an older server implies.
    dx_cluster_available: bool,
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
    // Main-window geometry changed (during drag) but not yet written out.
    // Save only fires on pointer-up (anti-I/O-stall); without this flag the
    // final geometry was never saved because the last change fell during the
    // drag and the release frame itself no longer shows a change.
    main_geom_dirty: bool,
    // Log panel
    log_buffer: LogBuffer,
    show_log: bool,
    show_about: bool,
    // VRX detached windows - VRX1 (on RX1+VFO-A) and VRX2 (on RX2+VFO-B) each in
    // their own popout viewport, independently placeable (like RX1/RX2).
    vrx1_popout: bool,
    vrx2_popout: bool,
    // "Arrange windows" matrix placer: opens a window (show_layout_arranger) +
    // a grid PER monitor in which you "paint" windows across cells. Session
    // state only, not persistent.
    show_layout_arranger: bool,
    /// Placement matrix PER monitor (index = monitor). You pick a screen, a
    /// grid size (e.g. 2x3) and paint the open windows into the cells;
    /// "Apply" snaps each window over its enclosing cell rectangle.
    layout_grid_per_monitor: Vec<LayoutGrid>,
    /// Stored arrangements (LAYOUT_MEM_SLOTS entries, empty = free slot).
    layout_memories: Vec<LayoutMemory>,
    /// Windows a recall wanted but could not place yet, because they were not
    /// on offer at that moment (no connection -> no radios). Carried out as soon
    /// as they appear. Not persisted: it is an intent within this session.
    layout_pending: Vec<(SnapWindow, egui::Pos2, egui::Vec2)>,
    /// The currently selected window in the palette (painted when
    /// clicking/dragging in the grid). None = nothing selected (clear only).
    layout_active_item: Option<SnapWindow>,
    /// Anchor cell of an in-progress rectangle drag in the placement grid (first
    /// cell where the drag began); on release the whole block anchor..current
    /// is filled with the active window. None = no drag in progress.
    layout_drag_anchor: Option<(usize, usize)>,
    /// Target monitor for "Apply" (index into monitor_work_areas_px). Default =
    /// the monitor where the main window is; overridable in the Arrange window.
    layout_target_monitor: usize,
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
    // VRX1 window position (reuses the existing vrx_popout_* config keys).
    vrx_popout_pos: Option<egui::Pos2>,
    vrx_popout_size: Option<egui::Vec2>,
    vrx_popout_init_applied: bool,
    // VRX2 window position (own config keys vrx2_popout_*).
    vrx2_popout_pos: Option<egui::Pos2>,
    vrx2_popout_size: Option<egui::Vec2>,
    vrx2_popout_init_applied: bool,
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
    // Per-VRX dB-scale + waterfall contrast (parallel to
    // self.spectrum_ref_db / range_db / waterfall_contrast for RX1).
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
    /// Last span (kHz) sent to server; resend on change to avoid spam.
    vrx1_high_res_last_span_khz: u16,
    /// Last VRX pan offset in Hz handed to the server, per channel. The server
    /// cuts its window there; resending an unchanged value every frame would
    /// be a packet per frame for nothing.
    vrx1_last_sent_pan_hz: i32,
    vrx2_last_sent_pan_hz: i32,
    /// When the VRX pan last moved, so the send can wait for it to settle.
    vrx_pan_changed_at: Option<Instant>,
    /// VRX1's independent channel spectrum: own bins/center/span, waterfall,
    /// s-meter and auto-ref derivation — all from its own data stream. Replaces
    /// the separate `vrx1_extracted_*` / `vrx1_hr_waterfall` / `vrx1_smeter_*` /
    /// `vrx1_auto_ref_*` fields (REFACTOR-audio-spectrum-per-channel §6.1).
    /// RX1/RX2 spectrum state in the same channel type as VRX. Rendering still
    /// reads the flat fields; what lives here is the derivation (auto-ref) plus
    /// the identity that travels with the bins.
    rx1_spectrum: ChannelSpectrum,
    rx2_spectrum: ChannelSpectrum,
    vrx1_spectrum: ChannelSpectrum,
    vrx2_ref_db: f32,
    vrx2_range_db: f32,
    vrx2_wf_contrast: f32,
    vrx2_pan: f32,
    vrx2_auto_ref: bool,
    vrx2_zoom_initialized: bool,
    vrx2_filter_low_hz: i32,
    vrx2_filter_high_hz: i32,
    vrx2_high_res_spectrum: bool,
    vrx2_high_res_last_span_khz: u16,
    /// VRX2's independent channel spectrum (see `vrx1_spectrum`).
    vrx2_spectrum: ChannelSpectrum,
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
    /// Whether the main spectrum's zoom has been matched to the receiver's
    /// width this session. VRX has had this since it was written; RX1 waited
    /// for the full-band spectrum to arrive, which never happens if it is off.
    rx_zoom_initialized: bool,
    /// The same for RX2. It had no flag of its own and used "the full-band
    /// span just went from nothing to something" instead, which says nothing
    /// about whether the zoom was ever matched - and fires again after every
    /// reset that puts that span back to zero, over an operator's own setting.
    rx2_zoom_initialized: bool,
    /// Which subscription bits last disagreed with the server, so the line is
    /// written when it changes instead of once a frame.
    subs_differ_seen: u16,
    /// The session number this client last re-subscribed for. A counter rather
    /// than a flank, so a short interruption cannot slip past between frames.
    session_generation_seen: u64,
    /// The frequency the view numbers were last written down at, so tuning
    /// leaves a trail without a line per frame.
    rx1_logged_freq_hz: u64,
    /// When that trail last wrote a line. A kilohertz is the smallest step
    /// there is, so spinning a knob produced a line every thirty
    /// milliseconds - two hundred in a minute, which crowds out the hour
    /// before the fault in a report that carries only the last megabyte.
    /// One a second is still every step worth seeing.
    rx1_logged_at: Option<Instant>,
    rx2_logged_at: Option<Instant>,
    /// The same for RX2, which had no view line at all. Its zoom could only be
    /// read off a slider, so the build that gave RX2 its width could not be
    /// checked the way RX1's was.
    rx2_logged_freq_hz: u64,
    /// When the view was last found to disagree with what this client asked
    /// for, so putting it right cannot turn into a packet per frame.
    view_mismatch_at: Option<Instant>,
    /// When zoom or pan was last sent. Between that and the packet that
    /// reflects it the two ends legitimately differ, and calling that a
    /// disagreement is crying wolf at the operator's own zoom slider.
    view_sent_at: Option<Instant>,
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
    /// Phase C: relay runs as transport (monitor in main.rs). In that case don't
    /// manage our own monitor; config changes require an app restart.
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
    /// Power-cap table: current values from the server (read-only mirror).
    amplitec_power_max_w: [u16; 6],
    amplitec_power_tx_blocked: [bool; 6],
    amplitec_power_loaded: bool,
    /// Power-cap edit state. Initialized from server values as soon as
    /// `amplitec_power_loaded` becomes true; after that only changed by the
    /// operator (DragValue / Checkbox). The Save button sends these values to
    /// the server.
    /// Collapsing-section open/closed. Persisted in client config.
    amplitec_power_show: bool,
    /// Index of the favorite currently in inline-edit mode (max one at a
    /// time). `None` = all labels read-only/selectable. On loss of
    /// focus or Enter this is reset to `None` and the name is written
    /// to config.
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
    /// Why an absent radio is absent (PORT_TROUBLE_* wire code, 0 = nothing to
    /// say). Shown as a line under the radio in the devices tab - the most
    /// common value names the most common field problem: another control
    /// program holding the radio's COM port.
    yaesu_port_trouble: u8,
    yaesu2_port_trouble: u8,
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
    // wire code from RadioInfo (0=991A,1=FTX1) for the panel naming.
    yaesu_model: u8,
    yaesu2_model: u8,
    yaesu2_connected: bool,
    /// Optimistic display presence per Yaesu slot: seeded from the persisted
    /// last-known presence so a radio present last session shows at once (pre-
    /// connect), then set to the real server presence while connected. The chips
    /// and pop-out gate on THIS (not yaesu*_connected), so Yaesu is optimistic
    /// like RX/VRX; yaesu*_connected stays the raw server value for the memory-
    /// read rising edge and other "actually connected now" logic. See sync_state.
    yaesu_present_last: bool,
    yaesu2_present_last: bool,
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
    yaesu2_tuner_state: u8, // internal ATU state of the radio (0=off,1=on,2=tuning) via AC;-poll
    yaesu2_hi_swr: bool,    // radio reports high SWR during TX (self-clearing)
    yaesu2_feature_toggles: u32, // DSP/function toggles bitfield (PATCH-yaesu-extra-controls)
    yaesu2_feature_levels: [u8; 16], // multi-state/level values (Phase B: AGC, IPO)
    yaesu2_squelch: u16,
    yaesu2_rf_gain: u16,
    yaesu2_rf_power: u16,
    yaesu2_mic_gain: f32,
    yaesu2_eq_enabled: bool,
    yaesu2_eq_gains: [f32; 5], // -12..+12 dB per band (own TX-EQ radio 2)
    yaesu2_eq_profiles: Vec<(String, bool, [f32; 5], f32)>,
    yaesu2_eq_active_profile: String,
    yaesu2_eq_new_name: String,
    collapse_yaesu2_eq: bool,
    collapse_yaesu2_memories: bool,
    yaesu2_control_changed_at: Option<std::time::Instant>, // debounce slider-sync
    yaesu2_volume: f32,
    // Own enable + PTT-mode for radio 2 (separate from radio 1), so each Yaesu
    // has its own on/off and Push-to-talk/Toggle choice (operator requirement dual-radio).
    yaesu2_enabled: bool,
    yaesu2_ptt_toggle_mode: bool,
    yaesu2_enable_sent: bool,
    /// Deferred auto-read moment for the FTX-1 memories: the radio + server are
    /// too busy right after connect (bring-up/audio/polls) so a direct MR-scan
    /// misses channels. Set on enable, fires once ~1.5s later.
    yaesu2_autoread_at: Option<Instant>,
    // Cooldown guard for the auto-swap that forces HF to A (FTX-1: HF wins the
    // USB-audio, so HF must be on the controlled/TX side A, otherwise split).
    yaesu2_hf_swap_at: Option<std::time::Instant>,
    // Slot-1 (FTX-1) own popout window - separate from the 991A window.
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
    /// Last Yaesu STATE subscription sent to the server (window open?), separate
    /// from the audio checkbox. None = not yet sent / reset on disconnect -> resend.
    yaesu_state_sent: Option<bool>,
    yaesu2_state_sent: Option<bool>,
    yaesu_mic_gain: f32, // internal multiplier for Yaesu USB TX audio
    // Client-side TX-compressor (0-100) + AGC-toggle per radio (like the EQ).
    yaesu_compressor: u8,
    yaesu2_compressor: u8,
    yaesu_tx_agc: bool,
    /// The roger beep, as shown in the Server tab. One set of numbers, a tick
    /// per channel.
    roger: sdr_remote_logic::roger::RogerBeep,
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
    // Max TX power (watt) for the current band (PATCH-yaesu-power-scaling). Drives
    // the slider range 5..=max. 0 = old server/unknown -> fall back to 100.
    yaesu_tx_power_max: u16,
    yaesu2_tx_power_max: u16,
    // Confirm-based PWR-sync: last-sent value + time. The slider only accepts the
    // radio readback once it confirms this value (or after timeout) -> no bounce.
    yaesu_power_pending: Option<u16>,
    yaesu_power_pending_at: Option<Instant>,
    yaesu2_power_pending: Option<u16>,
    yaesu2_power_pending_at: Option<Instant>,
    yaesu_scan_active: bool,
    yaesu_split_active: bool,
    yaesu_tuner_state: u8, // internal ATU state of the radio (0=off,1=on,2=tuning) via AC;-poll
    yaesu_hi_swr: bool,    // radio reports high SWR during TX (self-clearing)
    yaesu_feature_toggles: u32, // DSP/function toggles bitfield (PATCH-yaesu-extra-controls)
    yaesu_feature_levels: [u8; 16], // multi-state/level values (Phase B: AGC, IPO)
    // Phase C level-sliders (debounced), [slot][NB,DNR,Processor,AMC]. Synced from
    // feature_levels[8..12] once the debounce (yaesu_control_changed_at) has expired.
    yaesu_level_sliders: [[i32; 4]; 2],
    // Phase D freq-sliders (debounced), [slot][Contour,APF,Notch]. From feature_freqs.
    yaesu_freq_sliders: [[i32; 3]; 2],
    // Clarifier-offset (§15) per slot, signed Hz - display from feature_freqs[3].
    yaesu_clar_offset: i16,
    yaesu2_clar_offset: i16,
    // Chosen step size for the touch-friendly frequency stepper (§16), shared
    // across the main RX and both Yaesu VFOs.
    tune_step_hz: i64,
    yaesu_in_memory_mode: bool,
    yaesu_current_mem_ch: Option<usize>, // index into yaesu_mem_channels
    /// Which channel the memory TABLE marks, per radio, as a channel NUMBER rather
    /// than a row index - the two lists are different lengths, so an index means a
    /// different channel in each. Sticky: it survives tuning away into VFO, which is
    /// what makes "the one you left" findable.
    // ---- chat (docs/internal/DESIGN-relay-chat.md) ----
    chat_popout_pos: Option<egui::Pos2>,
    chat_popout_size: Option<egui::Vec2>,
    chat_popout_init_applied: bool,
    pub(crate) chat_open: bool,
    /// The chat and problem reporting, shared with the server GUI.
    ///
    /// A component rather than two dozen fields, because the server needs the
    /// same window and painting it twice is how two consent texts end up saying
    /// different things.
    pub(crate) chat: sdr_remote_chat::ChatPanel,
    yaesu_mem_active_ch: Option<u16>,
    yaesu2_mem_active_ch: Option<u16>,
    /// Whether the radio is on that channel right now (as opposed to having left it).
    yaesu_mem_active_live: bool,
    yaesu2_mem_active_live: bool,
    yaesu_enabled: bool,
    // Yaesu memory channels
    yaesu_mem_channels: Vec<yaesu_memory::YaesuMemoryChannel>,
    yaesu_mem_file: String,
    yaesu_mem_selected: Option<usize>,
    yaesu_mem_filter: String,
    yaesu_mem_dirty: bool,
    /// A pushed list was held back because the table has unsaved edits. Only to
    /// keep the log to one line instead of one per frame.
    yaesu_mem_push_deferred: bool,
    /// The operator asked the radio for a list, so the next one that arrives is
    /// the answer to that and must land even though the table is open. Without
    /// this, pressing "Read radio" while looking at the table did nothing
    /// visible: the hold-back that protects an edit also swallowed the reply.
    yaesu_mem_expect_push: bool,
    yaesu2_mem_expect_push: bool,
    yaesu2_mem_push_deferred: bool,
    /// Show the "row removed locally only" popup after clicking delete (x).
    yaesu_mem_radio_received: bool,
    /// Hash of the last memory list taken from the server, per slot. The list is
    /// PUSHED now (initial snapshot on subscribe, then on change), so it arrives
    /// unannounced and repeatedly; parsing only when the content actually differs
    /// keeps a later push working without re-parsing the same list every frame.
    yaesu_mem_blob_hash: Option<u64>,
    // Slot-1 (FTX-1) memory state (Phase B). render_yaesu_memories is shared
    // via mem::swap of these fields + yaesu_mem_active_slot (for read/write cmds).
    yaesu2_mem_channels: Vec<yaesu_memory::YaesuMemoryChannel>,
    yaesu2_mem_file: String,
    yaesu2_mem_selected: Option<usize>,
    yaesu2_mem_filter: String,
    yaesu2_mem_dirty: bool,
    yaesu2_mem_radio_received: bool,
    yaesu2_mem_blob_hash: Option<u64>,
    yaesu_mem_active_slot: u8,
    // ── The EX / radio-settings menu, per radio slot ──────────────────────
    //
    // Indexed by SLOT, shaped by MODEL. Which of the two shapes a slot carries
    // follows the radio that is in it, not the slot number: an FT-991A addresses
    // its menu by number, an FTX-1 by a six-digit address. These used to be two
    // separate sets of fields, one per slot, each hard-wired to one shape - so
    // an FTX-1 in slot 1 was read with the 991A parser (a third of its settings
    // silently dropped, the rest shown in the wrong view) and a 991A in slot 2
    // came out empty (2026-08-20).
    //
    // Exactly one of `menu_items` / `menu_entries` is filled for a given slot;
    // the parser clears the other, so there is never a stale second answer to
    // "what is in this menu".
    /// FT-991A shape: menu numbers 1..153, laid out by `yaesu_menu::MENU_DEFS`.
    menu_items: [Vec<yaesu_menu::MenuItem>; 2],
    /// FTX-1 shape: six-digit EX addresses, laid out by `ftx1_ex_chart`.
    menu_entries: [Vec<(String, String)>; 2],
    /// Hash of the last EX list taken from the server, per slot - same reason as
    /// the memory list: these are PUSHED, so "have I seen this" is a question
    /// about the content, not a one-shot latch.
    menu_blob_hash: [Option<u64>; 2],
    /// The model code the current parse was made with.
    ///
    /// The blob and the model arrive on their own schedules, and the blob can be
    /// first. Without this the wrong-shaped parse would be cached and the hash
    /// would then say "already seen" forever, so the menu stayed wrong even
    /// after the radio had named itself.
    menu_parsed_as: [u8; 2],
    /// Editable value buffers per EX key (lazily filled on render).
    menu_edits: [std::collections::HashMap<String, String>; 2],
    menu_filter: [String; 2],
    collapse_yaesu2_menu: bool,
    rotor_goto_input: String,
    // DX Cluster spots
    dx_spots: Vec<sdr_remote_logic::state::DxSpotInfo>,
    // Smooth tuning: display center interpolates toward VFO for smooth visual scroll
    smooth_display_center_hz: f64,   // RX1 smoothed display center
    rx2_smooth_display_center_hz: f64, // RX2 smoothed display center
    /// The VFO marker's own smoothed position, per receiver.
    ///
    /// Smoothed on the same clock as the centre so the marker and the trace
    /// move together while tuning, but derived from the VFO itself rather than
    /// from `centre - pan x full_span`. That subtraction is only the VFO while
    /// the full span is known, and it is known only once the full-band row has
    /// been on: with it zero the marker collapsed onto the middle of the view
    /// and sat there while the spectrum panned underneath it (2026-08-13).
    smooth_vfo_hz: f64,
    rx2_smooth_vfo_hz: f64,
    smooth_alpha: f64,               // shared smoothing alpha for current frame
    last_frame_time: Instant,
    // DX-cluster spot stream - data-saving toggle (Server-tab)
    dx_spots_enabled: bool,
    /// RX1 audio subscription (default ON). Off = no RX1 audio stream from the
    /// server (save bandwidth for VRX-only use).
    rx1_enabled: bool,
    // RX2 / VFO-B
    rx2_enabled: bool,
    /// Optimistic-enable safety net for the server-backed audio toggles (RX1/RX2):
    /// Some((want, since)) = the client requested `want` and shows it immediately;
    /// the server sync keeps that optimistic value until it confirms `want` OR the
    /// grace window elapses (then the server value wins, so an enable the server
    /// can't/didn't honour turns back off). None = no pending request, server is
    /// authoritative. Prevents the server's pre-request default from clobbering a
    /// just-made toggle (RX2 flipped off-then-on for ~1 s on startup). RX1 + RX2
    /// share this one path so they behave identically. See reconcile_audio_enable.
    rx1_enabled_pending: Option<(Instant, bool)>,
    rx2_enabled_pending: Option<(Instant, bool)>,
    /// RX2 spectrum subscription, SEPARATE from rx2_enabled (audio). Off = no
    /// RX2 spectrum stream; on = spectrum even without RX2 audio (phase 3b/4).
    rx2_spectrum_enabled: bool,
    /// Opt-in: Thetis RX1/RX2/BinR audio in wideband Opus (16 kHz)
    /// instead of narrowband (8 kHz). Default false; sent to the server
    /// via `ControlId::ThetisWidebandAudio`. See memory
    /// `reference_thetis_audio_paths` for when which path uses which
    /// sample rate.
    thetis_wideband_audio: bool,
    /// Whether the server should keep sending the full-DDC spectrum row.
    full_spectrum_enabled: bool,
    rx2_popout: bool,
    popout_joined: bool,
    /// S-meter type PER channel (analog=true / bar=false). Index via M_RX1..M_YAESU2.
    /// Clicking the meter toggles the type; persistent per channel.
    meter_analog: [bool; 6],
    ub_show_menu: bool,
    collapse_diversity: bool,
    collapse_yaesu_eq: bool,
    collapse_yaesu_memories: bool,
    collapse_yaesu_menu: bool,
    yaesu_memories_h: f32,
    yaesu2_memories_h: f32,
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
    /// Inline freq-edit state for RX2 - symmetric with `freq_editing` on RX1
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
    /// Per-target selected WebSDR URL (Thetis / Yaesu1 / Yaesu2), so each radio
    /// remembers its own WebSDR independently. The single embedded window uses
    /// whichever target's URL was last opened (`catsync.websdr_url`).
    websdr_urls: [String; 3],
}


#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatSyncTarget {
    Thetis,
    Yaesu1,
    Yaesu2,
}

impl CatSyncTarget {
    pub(crate) fn idx(self) -> usize {
        match self {
            CatSyncTarget::Thetis => 0,
            CatSyncTarget::Yaesu1 => 1,
            CatSyncTarget::Yaesu2 => 2,
        }
    }
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


// Indices into `meter_analog[..]` (s-meter type per channel).
const M_RX1: usize = 0;
const M_RX2: usize = 1;
const M_VRX1: usize = 2;
const M_VRX2: usize = 3;
const M_YAESU1: usize = 4;
const M_YAESU2: usize = 5;

impl SdrRemoteApp {
    /// Clicking an s-meter toggles the type (analog <-> bar) of that channel and
    /// saves it. `rect` is the area the meter helper returned.
    fn meter_click(&mut self, ui: &egui::Ui, rect: egui::Rect, ch: usize) {
        if rect.width() < 4.0 { return; }
        let resp = ui.interact(rect, egui::Id::new(("smeter_click", ch)), egui::Sense::click());
        let clicked = resp
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(rust_i18n::t!("main_meter_click_hover").to_string())
            .clicked();
        if clicked {
            self.meter_analog[ch] = !self.meter_analog[ch];
            self.save_full_config();
        }
    }

    /// Uniform subscription chips for one RX channel (parity by construction,
    /// model §1b/§6.4): `[audio-checkbox Name] [spectrum-toggle "spec"]`. All channels
    /// draw their audio checkbox + spectrum toggle via this helper + the shared
    /// `theme::tl_toggle_button`, so they can no longer diverge per channel.
    /// Returns `(audio_clicked, spectrum_clicked)`; the caller dispatches the
    /// channel-specific command (since RX1/RX2 each have their own ControlId).
    /// Shared block toggle for the main-screen channel chips: fixed width,
    /// blue fill only when toggled-ON (per UI convention), visible hover.
    /// Used by rx_sub_chips and yaesu_sub_chips (parity by construction).
    fn sized_toggle(ui: &mut egui::Ui, label: &str, on: bool, w: f32, hover: &str) -> bool {
        let text = if on { RichText::new(label).size(12.0).strong() } else { RichText::new(label).size(12.0) };
        let mut btn = egui::Button::new(text);
        if on { btn = btn.fill(theme::TL_SELECTED_FILL); }
        ui.add_sized([w, 20.0], btn).on_hover_text(hover.to_string()).clicked()
    }

    /// Width of one main-screen channel block. Sized on the longest label the
    /// buttons can carry across the four languages ("venster" / "Fenster" /
    /// "fenêtre"), so no translation gets clipped.
    pub(crate) const CHANNEL_CHIP_W: f32 = 58.0;

    /// The channel block on the main screen: the channel name as a heading with
    /// two buttons under it, [audio] and [venster].
    ///
    /// The heading says WHICH channel, the buttons say WHAT they do to it - the
    /// channel name used to be the audio button itself, which read as a label
    /// rather than as a switch. One helper for all six channels (RX1, RX2, VRX1,
    /// VRX2 and both Yaesu slots): parity by construction, per
    /// docs/internal/UI-STYLE-GUIDE.md. Only the hover texts differ, because what
    /// sits behind the window button differs (spectrum + waterfall for RX/VRX,
    /// the control panel for a Yaesu).
    ///
    /// Returns `(audio_clicked, window_clicked)`; the caller dispatches, since
    /// every channel has its own command.
    fn channel_sub_chips(
        ui: &mut egui::Ui,
        name: &str,
        audio_on: bool,
        window_on: bool,
        audio_hover: &str,
        window_hover: &str,
    ) -> (bool, bool) {
        let mut audio = false;
        let mut window = false;
        ui.group(|ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                let w = Self::CHANNEL_CHIP_W;
                ui.add_sized(
                    [w, 13.0],
                    egui::Label::new(RichText::new(name).size(11.0).strong()).selectable(false),
                );
                audio = Self::sized_toggle(
                    ui, &rust_i18n::t!("main_chip_audio").to_string(), audio_on, w, audio_hover);
                window = Self::sized_toggle(
                    ui, &rust_i18n::t!("main_chip_window").to_string(), window_on, w, window_hover);
            });
        });
        (audio, window)
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
        if ui.add(btn).on_hover_text(rust_i18n::t!("main_hover_popouts_split_joined").to_string()).clicked() && !active {
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
    /// capture gate. Call edge-triggered only.
    ///
    /// Playback-mute is now gated on spike-protection. ThetisLink is a
    /// multi-receiver client (RX1/RX2/VRX/Yaesu); muting ALL local RX audio during
    /// a TX on one of them is only wanted for a built-in speaker+mic (feedback +
    /// switch-on spike). With a headset / well-isolated audio (spike-protection
    /// off, the default) the operator wants to keep monitoring the other receivers
    /// while transmitting, so we do NOT mute. Keyup always unmutes (harmless).
    fn apply_ptt_spike_protection(&self, yaesu: bool, active: bool) {
        if active {
            let _ = self
                .cmd_tx
                .send(Command::SetMicGateDelayMs(self.ptt_gate_delay_ms(yaesu)));
            if self.spike_protection {
                let _ = self.cmd_tx.send(Command::SetPlaybackMute(true));
            }
        } else {
            let _ = self.cmd_tx.send(Command::SetPlaybackMute(false));
        }
    }

    // ---------------------------------------------------------------------
    // controls-scaffolding - sub-stap 4 writeback-extract
    // ---------------------------------------------------------------------

    /// Build an `RxChannelState` snapshot for RX1. Takes `freq_edit_text`
    /// via `std::mem::take` (no clone); the writeback restores it.
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

    /// Build an `RxChannelState` snapshot for RX2. Now also carries inline-edit
    /// state (PATCH-rx2-inline-edit - symmetric with RX1).
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

    /// Write possibly-mutated snap fields back to `self`.
    /// Idempotent: if the helper mutated nothing, the values are equal.
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

    /// Scaffold for a control-helper call: builds snap + ControlContext,
    /// calls `action`, writes snap back to `self`.
    ///
    /// Uses the app-level `ui_event_sink`. The sink is `Arc<dyn UiEventSink>`
    /// so test mode can swap in `RecordingSink` without touching call sites.
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

    /// Handle a band click from `controls::render_band_selector`.
    ///
    /// Band-switch is multi-command (SetMode, SetFrequency, filter-IDs, NR) via
    /// `restore_band`. That's why `ctx.dispatch()` (single-command) is not used,
    /// but bookended manually: `IntentEmitted` + conditional `CommandSent` /
    /// `CommandBlocked`. Frame-race safety: connected is read at emit time,
    /// the disconnected path skips `save_current_band`/`restore_band` to avoid
    /// UI drift.
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
}
