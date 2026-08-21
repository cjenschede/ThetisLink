// SPDX-License-Identifier: GPL-2.0-or-later

mod utils;
mod rotor;
mod amplitec;
mod tuner;
mod macros_ui;
mod spe;
mod rf2k;
mod ultrabeam;
mod chat_window;
mod status_panel;
mod window_placement;
mod update;
mod arranger;
mod startup;
mod app_state;

pub(crate) use utils::*;
use rotor::*;
use amplitec::*;
use tuner::*;
use macros_ui::*;
use spe::*;
use rf2k::*;
use ultrabeam::*;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use egui::{Color32, RichText, ViewportBuilder, ViewportId};
use log::Level;
use tokio::sync::watch;

use crate::amplitec::AmplitecSwitch;
use crate::config::ServerConfig;
use crate::macros::{self, MacroAction, MacroRunner, MacroSlots};
use crate::rf2k::Rf2k;
use crate::spe_expert::SpeExpert;
use crate::tuner::Jc4sTuner;
use crate::ultrabeam::UltraBeam;
use crate::LogBuffer;

enum Mode {
    Settings,
    Running,
}

/// A window that can be snapped onto the screen by the "Vensters schikken"
/// matrix. `Main` = the main window (root viewport); the rest are the
/// server's backend popouts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SnapWindow {
    Main,
    Tuner,
    Amplitec,
    Spe,
    Rf2k,
    Ultrabeam,
    Rotor,
}
impl sdr_remote_layout::SnapTarget for SnapWindow {
    fn all() -> &'static [Self] {
        &[
            SnapWindow::Main, SnapWindow::Tuner, SnapWindow::Amplitec, SnapWindow::Spe,
            SnapWindow::Rf2k, SnapWindow::Ultrabeam, SnapWindow::Rotor,
        ]
    }
    /// Stable ASCII key: goes into the config file and into egui Ids, so it must
    /// not follow the translated label.
    fn key(self) -> &'static str {
        match self {
            SnapWindow::Main => "main",
            SnapWindow::Tuner => "tuner",
            SnapWindow::Amplitec => "amplitec",
            SnapWindow::Spe => "spe",
            SnapWindow::Rf2k => "rf2k",
            SnapWindow::Ultrabeam => "ultrabeam",
            SnapWindow::Rotor => "rotor",
        }
    }
    fn label(self) -> String {
        match self {
            SnapWindow::Main => rust_i18n::t!("srv_win_main").to_string(),
            SnapWindow::Tuner => "Tuner".to_string(),
            SnapWindow::Amplitec => "Amplitec".to_string(),
            SnapWindow::Spe => "SPE".to_string(),
            SnapWindow::Rf2k => "RF2K".to_string(),
            SnapWindow::Ultrabeam => "UltraBeam".to_string(),
            SnapWindow::Rotor => "Rotor".to_string(),
        }
    }
    /// Fill color in the placement matrix (distinguishable per window).
    fn color(self) -> Color32 {
        match self {
            SnapWindow::Main => Color32::from_rgb(70, 110, 175),
            SnapWindow::Tuner => Color32::from_rgb(60, 140, 95),
            SnapWindow::Amplitec => Color32::from_rgb(95, 155, 70),
            SnapWindow::Spe => Color32::from_rgb(160, 115, 55),
            SnapWindow::Rf2k => Color32::from_rgb(175, 145, 60),
            SnapWindow::Ultrabeam => Color32::from_rgb(145, 80, 140),
            SnapWindow::Rotor => Color32::from_rgb(115, 90, 165),
        }
    }
}

/// Toggle-on blue (style guide): used for the active button/selection highlight.
const SNAP_ACCENT: Color32 = Color32::from_rgb(100, 160, 230);

/// The placement matrix, the stored arrangements and the grid limit all come from
/// the shared arranger (`sdr-remote-layout`), which the desktop client uses too.
/// They used to be a second copy here, and the two drifted: the client reached
/// 18x18 with arrangement memories and a UI scale while this one stayed at 12x12
/// with none of it.
pub(crate) type LayoutGrid = sdr_remote_layout::LayoutGrid<SnapWindow>;
pub(crate) type LayoutMemory = sdr_remote_layout::LayoutMemory<SnapWindow>;
pub(crate) use sdr_remote_layout::{SnapTarget, LAYOUT_MEM_SLOTS};

pub struct ServerApp {
    tci_addr: String,
    /// Radio has a second receiver (RX2). Default true; turn off for
    /// single-receiver radios -> clients then show RX2/VRX2 nowhere.
    rx2_present: bool,
    thetis_path: String,
    yaesu_port: String,
    yaesu_audio_device: String,
    yaesu_audio_output_device: String,
    yaesu_enabled: bool,
    // Per slot, indexed by it. Only meaningful on one model each, but the value
    // is a choice about one radio and two radios of the same type can be
    // attached - so it belongs to the slot, not to the type.
    ssb_switch_on_ptt: [bool; 2],
    memory_write_ack: [bool; 2],
    audio_channel: [u8; 2],
    /// What model is on each slot's port, as far as the settings screen knows:
    /// remembered from last time, refreshed by a probe when a port is picked.
    /// `None` means nobody has answered on that port yet, and then the screen
    /// offers no model-specific control at all - it cannot honestly say which
    /// radio the setting would be for.
    probe_model: [Option<u8>; 2],
    /// A probe is out on this slot. It opens the port and may walk seven baud
    /// rates, so it runs on its own thread and this is what the screen shows
    /// meanwhile.
    probe_busy: [bool; 2],
    /// This slot's port has been asked once this session. Without it the
    /// auto-probe below would fire again every frame on a radio that is off.
    probe_attempted: [bool; 2],
    /// The last probe on this port got no answer, while a model from an earlier
    /// session is still on screen. Switching a radio off must not take away the
    /// settings it had.
    probe_silent: [bool; 2],
    /// The port each `probe_model` belongs to, so a changed selection invalidates
    /// the answer instead of describing the previous radio.
    probe_port: [String; 2],
    probe_tx: std::sync::mpsc::Sender<(u8, String, Option<u8>)>,
    probe_rx: std::sync::mpsc::Receiver<(u8, String, Option<u8>)>,
    // Dual-radio slot 1 (radio 2) - now also in the settings GUI instead of conf-only.
    yaesu2_port: String,
    yaesu2_audio_device: String,
    yaesu2_audio_output_device: String,
    yaesu2_enabled: bool,
    amplitec_port: String,
    amplitec_enabled: bool,
    serial_ports: Vec<String>,
    mode: Mode,
    shutdown_tx: Option<watch::Sender<bool>>,
    server_thread: Option<std::thread::JoinHandle<()>>,
    log_buffer: LogBuffer,
    // Amplitec window
    yaesu: Option<Arc<crate::yaesu::YaesuRadio>>,
    amplitec: Option<Arc<AmplitecSwitch>>,
    show_amplitec_window: bool,
    amplitec_labels: [String; 6],
    // Power limit per Amplitec-A position (config; server-editable). Set in the
    // conf via modify_config; the network loop rereads it + pushes to
    // clients (which show it read-only). None = no cap.
    amplitec_max_w: [Option<u16>; 6],
    amplitec_tx_blocked: [bool; 6],
    amplitec_log: VecDeque<(String, String)>,
    last_switch_a: u8,
    last_switch_b: u8,
    // Tuner window
    tuner: Option<Arc<Jc4sTuner>>,
    show_tuner_window: bool,
    tuner_log: VecDeque<(String, String)>,
    last_tuner_state: u8,
    // Macro system
    macro_slots: MacroSlots,
    macro_runner: MacroRunner,
    macro_cat_tx: Option<tokio::sync::mpsc::Sender<String>>,
    show_macro_editor: bool,
    editor_slot: usize,
    editor_label: String,
    editor_actions: Vec<MacroAction>,
    // SPE Expert
    spe_port: String,
    spe_enabled: bool,
    spe: Option<Arc<SpeExpert>>,
    show_spe_window: bool,
    spe_log: VecDeque<(String, String)>,
    last_spe_state: u8,
    last_spe_warning: u8,
    last_spe_alarm: u8,
    spe_window_pos: Option<[f32; 2]>,
    // RF2K-S
    rf2k_addr: String,
    rf2k_enabled: bool,
    rf2k: Option<Arc<Rf2k>>,
    show_rf2k_window: bool,
    rf2k_window_pos: Option<[f32; 2]>,
    rf2k_peak_power: u16,
    rf2k_peak_time: std::time::Instant,
    // Log visibility per device window
    show_amplitec_log: bool,
    show_tuner_log: bool,
    show_spe_log: bool,
    // SPE peak hold
    spe_peak_power: u16,
    spe_peak_time: std::time::Instant,
    // Shared drive level from CAT (updated by network loop)
    drive_level: Arc<AtomicU8>,
    // Window positions
    tuner_window_pos: Option<[f32; 2]>,
    amplitec_window_pos: Option<[f32; 2]>,
    // Active PA: 0=none, 1=SPE, 2=RF2K (shared with network thread)
    active_pa: Arc<AtomicU8>,
    // VFO frequencies shared from network thread (for UltraBeam auto-track)
    vfo_freq_shared: Arc<AtomicU64>,
    vfo_b_freq_shared: Arc<AtomicU64>,
    // RF2K-S debug/drive UI state (Fase D)
    rf2k_show_debug: bool,
    rf2k_show_drive_config: bool,
    rf2k_confirm_high_power: bool,
    rf2k_confirm_zero_fram: bool,
    rf2k_confirm_fw_close: bool,
    rf2k_drive_edit: [[u8; 11]; 3], // local copy: [ssb, am, cont]
    rf2k_drive_loaded: bool,
    // UltraBeam RCU-06
    ultrabeam_port: String,
    ultrabeam_enabled: bool,
    ultrabeam: Option<Arc<UltraBeam>>,
    show_ultrabeam_window: bool,
    ultrabeam_window_pos: Option<[f32; 2]>,
    ultrabeam_show_menu: bool,
    ultrabeam_confirm_retract: bool,
    ultrabeam_confirm_calibrate: bool,
    ultrabeam_auto_track: bool,
    ultrabeam_last_auto_khz: u16,
    // EA7HG Visual Rotor
    rotor_addr: String,
    rotor_enabled: bool,
    rotor: Option<Arc<crate::rotor::Rotor>>,
    show_rotor_window: bool,
    rotor_window_pos: Option<[f32; 2]>,
    rotor_goto_input: String,
    // Rotor backend choice + PstRotator fields (alternative backend
    // alongside EA7HG Visual Rotor).
    rotor_backend: String,
    pstrotator_host: String,
    pstrotator_port: u16,
    pstrotator_feedback_port: u16,
    pstrotator_has_elevation: bool,
    pstrotator_listen_enabled: bool,
    pstrotator_listen_port: u16,
    // Per-popout "init applied" flags - see mirror impl in
    // sdr-remote-client mod.rs apply_popout_geometry for the rationale.
    // Repeated `with_position()` calls every frame caused the windows to
    // jitter when manually moved; we now only apply position on the first
    // frame after the window opens, then let the OS keep it where the user
    // left it.
    tuner_window_init_applied: bool,
    amplitec_window_init_applied: bool,
    spe_window_init_applied: bool,
    rf2k_window_init_applied: bool,
    ultrabeam_window_init_applied: bool,
    rotor_window_init_applied: bool,
    // DX Cluster
    dxcluster_server: String,
    dxcluster_callsign: String,
    dxcluster_enabled: bool,
    dxcluster_expiry_min: u16,
    // Authentication
    password: String,
    totp_enabled: bool,
    totp_secret: String,
    // PATCH-3 mDNS friendly name (optional human-readable label)
    friendly_name: String,
    // Phase A relay monitor settings (status only; no TL frames routed yet).
    relay_enabled: bool,
    relay_url: String,
    relay_station: String,
    relay_token: String,
    relay_udp_enabled: bool,
    // UI theme (shared with the client via the sdr-remote-theme crate)
    theme_variant: sdr_remote_theme::ThemeVariant,
    theme_custom: sdr_remote_theme::Palette,
    // Autostart
    autostart: bool,
    pending_autostart: bool,
    // Main window position (persisted)
    main_window_pos: Option<[f32; 2]>,
    // Window sizes (persisted)
    main_window_size: Option<[f32; 2]>,
    tuner_window_size: Option<[f32; 2]>,
    amplitec_window_size: Option<[f32; 2]>,
    spe_window_size: Option<[f32; 2]>,
    rf2k_window_size: Option<[f32; 2]>,
    ultrabeam_window_size: Option<[f32; 2]>,
    rotor_window_size: Option<[f32; 2]>,
    show_about: bool,
    /// The FTX-1 memory-write condition window is open. Set by the checkbox in
    /// the settings screen and by the "read the condition" button; the Accept
    /// button inside it is what actually grants `memory_write_ack` - for the
    /// slot it was opened from, which is why this carries one.
    show_memory_condition: Option<u8>,
    // Chat and problem reporting (docs/internal/DESIGN-relay-chat.md), the same
    // component the desktop client draws. The server is a station in its own
    // right, so it gets the same window rather than a smaller one.
    pub(crate) chat: sdr_remote_chat::ChatPanel,
    show_chat_window: bool,
    chat_window_pos: Option<[f32; 2]>,
    chat_window_size: Option<[f32; 2]>,
    chat_window_init_applied: bool,
    /// PATCH-2: shared Status-panel probes - `Some` while a server is running,
    /// `None` before start_server / after Settings teardown.
    status_panel_state: Option<crate::audio_stats::StatusPanelShared>,
    /// PATCH-2: bind address shown in the Status panel (e.g. "0.0.0.0:4580").
    status_bind_addr: String,
    /// PATCH-2: which Mode::Running view is active.
    status_view: StatusView,
    // "Vensters schikken" matrix placer (session state only, not persistent):
    // a grid PER monitor in which you "paint" the server windows over cells.
    show_layout_arranger: bool,
    layout_grid_per_monitor: Vec<LayoutGrid>,
    /// Stored arrangements (LAYOUT_MEM_SLOTS entries, empty = free slot).
    layout_memories: Vec<LayoutMemory>,
    /// Windows a recall wanted but could not place yet, because that backend was
    /// not running. Carried out as soon as they appear. Not persisted.
    layout_pending: Vec<(SnapWindow, [f32; 2], [f32; 2])>,
    /// UI scale. Kept next to egui's own zoom_factor and synced both ways in
    /// update(), so a stored choice applies at startup and egui's Ctrl+/Ctrl-
    /// is picked up instead of being reset on the next launch.
    ui_zoom: f32,
    ui_zoom_pending: bool,
    layout_active_item: Option<SnapWindow>,
    /// Anchor cell of an ongoing rectangle drag in the placement grid.
    layout_drag_anchor: Option<(usize, usize)>,
    layout_target_monitor: usize,
    /// UI language ("en"/"nl"/"de"/"fr"); applied via rust_i18n::set_locale.
    ui_language: String,
}

/// PATCH-2: top-level view in Mode::Running - Logs (existing) or
/// Status (new compact server-state panel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusView {
    Status,
    Logs,
}



/// Spawn a fresh copy of the current executable with the same CLI args
/// and `process::exit(0)` afterwards, so the new process can bind the UDP socket
/// and all hardware handles. Called by the
/// auto-restart flow in `update()` *after* all Drop handlers have run
/// and the cpal/USB handles have been released.
fn spawn_replacement_and_exit() -> ! {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            log::error!("Auto-restart: cannot read current_exe(): {}", e);
            std::process::exit(1);
        }
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    log::info!("Auto-restart: relaunching {:?} (args: {:?})", exe, args);

    // Build the command with explicit null stdio + (on Windows) detached
    // process flags. Without this the spawn fails with ERROR_NOT_SUPPORTED
    // (os error 50) when the parent is a GUI-subsystem binary whose stdio
    // handles are NULL: CreateProcess refuses to clone them into the child.
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS (0x00000008) - the new process gets its
        // own console-handle group, separate from ours. CREATE_NEW_PROCESS_GROUP
        // (0x00000200) isolates Ctrl-C delivery. Together they make the
        // child fully self-contained so this process can exit immediately.
        const DETACHED_PROCESS: u32 = 0x00000008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    match cmd.spawn() {
        Ok(child) => {
            log::info!("Auto-restart: spawned PID {}, exiting", child.id());
            std::process::exit(0);
        }
        Err(e) => {
            log::error!("Auto-restart: spawn failed: {}", e);
            std::process::exit(1);
        }
    }
}

impl Drop for ServerApp {
    fn drop(&mut self) {
        self.save_window_positions();
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(true);
        }
    }
}
