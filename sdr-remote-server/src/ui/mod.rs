// SPDX-License-Identifier: GPL-2.0-or-later

mod utils;
mod rotor;
mod amplitec;
mod tuner;
mod macros_ui;
mod spe;
mod rf2k;
mod ultrabeam;
mod status_panel;
mod window_placement;

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

/// Een venster dat door de "Vensters schikken"-matrix op het scherm gesnapt
/// kan worden. `Main` = het hoofdvenster (root-viewport); de rest zijn de
/// backend-popouts van de server.
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
impl SnapWindow {
    const ALL: [SnapWindow; 7] = [
        SnapWindow::Main, SnapWindow::Tuner, SnapWindow::Amplitec, SnapWindow::Spe,
        SnapWindow::Rf2k, SnapWindow::Ultrabeam, SnapWindow::Rotor,
    ];
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
    /// Vulkleur in de plaatsings-matrix (onderscheidbaar per venster).
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

/// Maximaal rasterformaat in de "Vensters schikken"-matrix (GRID_MAX x GRID_MAX).
const GRID_MAX: usize = 12;
/// Toggle-on-blauw (stijlgids): gebruikt voor de actieve knop/selectie-highlight.
const SNAP_ACCENT: Color32 = Color32::from_rgb(100, 160, 230);

/// Eén plaatsings-matrix (per monitor): raster van `rows` x `cols` cellen; elke
/// cel bevat maximaal één venster. Een venster dat meerdere aangrenzende cellen
/// beslaat wordt bij "Toepassen" over zijn omhullende rechthoek uitgerekt.
#[derive(Clone)]
pub(crate) struct LayoutGrid {
    rows: u8, // 1..=GRID_MAX
    cols: u8, // 1..=GRID_MAX
    cells: Vec<Option<SnapWindow>>, // rows*cols, rij-voor-rij
}
impl LayoutGrid {
    fn new() -> Self {
        Self { rows: 2, cols: 2, cells: vec![None; 4] }
    }
    fn set_size(&mut self, rows: u8, cols: u8) {
        let rows = rows.clamp(1, GRID_MAX as u8);
        let cols = cols.clamp(1, GRID_MAX as u8);
        if rows == self.rows && cols == self.cols { return; }
        let mut cells = vec![None; rows as usize * cols as usize];
        for r in 0..self.rows.min(rows) as usize {
            for c in 0..self.cols.min(cols) as usize {
                cells[r * cols as usize + c] = self.cells[r * self.cols as usize + c];
            }
        }
        self.rows = rows;
        self.cols = cols;
        self.cells = cells;
    }
    fn cell(&self, r: usize, c: usize) -> Option<SnapWindow> {
        self.cells.get(r * self.cols as usize + c).copied().flatten()
    }
    fn set(&mut self, r: usize, c: usize, w: Option<SnapWindow>) {
        if let Some(slot) = self.cells.get_mut(r * self.cols as usize + c) {
            *slot = w;
        }
    }
    fn remove(&mut self, w: SnapWindow) {
        for slot in self.cells.iter_mut() {
            if *slot == Some(w) { *slot = None; }
        }
    }
    fn bounds(&self, w: SnapWindow) -> Option<(usize, usize, usize, usize)> {
        let (mut minr, mut minc, mut maxr, mut maxc) = (usize::MAX, usize::MAX, 0, 0);
        let mut found = false;
        for r in 0..self.rows as usize {
            for c in 0..self.cols as usize {
                if self.cell(r, c) == Some(w) {
                    found = true;
                    minr = minr.min(r); minc = minc.min(c);
                    maxr = maxr.max(r); maxc = maxc.max(c);
                }
            }
        }
        if found { Some((minr, minc, maxr, maxc)) } else { None }
    }
    fn placed(&self) -> Vec<SnapWindow> {
        SnapWindow::ALL.iter().copied()
            .filter(|w| self.cells.contains(&Some(*w)))
            .collect()
    }
}

pub struct ServerApp {
    tci_addr: String,
    /// Radio heeft een tweede ontvanger (RX2). Default true; uitzetten voor
    /// single-receiver radio's -> clients tonen dan nergens RX2/VRX2.
    rx2_present: bool,
    thetis_path: String,
    yaesu_port: String,
    yaesu_audio_device: String,
    yaesu_audio_output_device: String,
    yaesu_enabled: bool,
    yaesu_ssb_switch_on_ptt: bool,
    // Dual-radio slot 1 (radio 2) - nu ook in de settings-GUI i.p.v. conf-only.
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
    // Rotor backend keuze + PstRotator-velden (alternatieve backend
    // naast EA7HG Visual Rotor).
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
    // UI-thema (gedeeld met de client via de sdr-remote-theme crate)
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
    /// PATCH-2: shared Status-panel probes - `Some` while a server is running,
    /// `None` before start_server / after Settings teardown.
    status_panel_state: Option<crate::audio_stats::StatusPanelShared>,
    /// PATCH-2: bind address shown in the Status panel (e.g. "0.0.0.0:4580").
    status_bind_addr: String,
    /// PATCH-2: which Mode::Running view is active.
    status_view: StatusView,
    // "Vensters schikken" matrix-plaatser (alleen sessie-state, niet persistent):
    // een raster PER monitor waarin je de server-vensters over cellen "schildert".
    show_layout_arranger: bool,
    layout_grid_per_monitor: Vec<LayoutGrid>,
    layout_active_item: Option<SnapWindow>,
    /// Ankercel van een lopende rechthoek-sleep in het plaatsingsraster.
    layout_drag_anchor: Option<(usize, usize)>,
    layout_target_monitor: usize,
    /// UI-taal ("en"/"nl"/"de"/"fr"); toegepast via rust_i18n::set_locale.
    ui_language: String,
}

/// PATCH-2: top-level view in Mode::Running - Logs (existing) or
/// Status (new compact server-state panel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusView {
    Status,
    Logs,
}

impl ServerApp {
    pub fn new(config: ServerConfig, log_buffer: LogBuffer) -> Self {
        let serial_ports = crate::amplitec::available_ports();

        let has_spe = config.spe_port.is_some();
        let has_rf2k = config.rf2k_addr.is_some();
        let active_pa_val = if config.active_pa != 0 {
            config.active_pa
        } else if has_spe && !has_rf2k {
            1
        } else if has_rf2k && !has_spe {
            2
        } else if has_spe && has_rf2k {
            1 // default SPE
        } else {
            0
        };

        Self {
            tci_addr: config.tci_addr.unwrap_or_default(),
            rx2_present: config.rx2_present,
            thetis_path: config.thetis_path.unwrap_or_default(),
            yaesu_port: config.yaesu_port.unwrap_or_default(),
            yaesu_audio_device: config.yaesu_audio_device.unwrap_or_default(),
            yaesu_audio_output_device: config.yaesu_audio_output_device.unwrap_or_default(),
            yaesu_enabled: config.yaesu_enabled,
            yaesu_ssb_switch_on_ptt: config.yaesu_ssb_switch_on_ptt,
            yaesu2_port: config.yaesu2_port.clone().unwrap_or_default(),
            yaesu2_audio_device: config.yaesu2_audio_device.clone().unwrap_or_default(),
            yaesu2_audio_output_device: config.yaesu2_audio_output_device.clone().unwrap_or_default(),
            yaesu2_enabled: config.yaesu2_enabled,
            amplitec_port: config.amplitec_port.unwrap_or_default(),
            amplitec_enabled: config.amplitec_enabled,
            serial_ports,
            mode: Mode::Settings,
            shutdown_tx: None,
            server_thread: None,
            log_buffer,
            yaesu: None,
            amplitec: None,
            show_amplitec_window: config.show_amplitec_window,
            amplitec_labels: config.amplitec_labels,
            amplitec_log: VecDeque::new(),
            last_switch_a: 0,
            last_switch_b: 0,
            tuner: None,
            show_tuner_window: config.show_tuner_window,
            tuner_log: VecDeque::new(),
            last_tuner_state: 0,
            spe_port: config.spe_port.unwrap_or_default(),
            spe_enabled: config.spe_enabled,
            spe: None,
            show_spe_window: config.show_spe_window,
            spe_log: VecDeque::new(),
            last_spe_state: 255,
            last_spe_warning: b'N',
            last_spe_alarm: b'N',
            spe_window_pos: config.spe_window_pos,
            rf2k_addr: config.rf2k_addr.unwrap_or_default(),
            rf2k_enabled: config.rf2k_enabled,
            rf2k: None,
            show_rf2k_window: config.show_rf2k_window,
            rf2k_window_pos: config.rf2k_window_pos,
            rf2k_peak_power: 0,
            rf2k_peak_time: std::time::Instant::now(),
            show_amplitec_log: false,
            show_tuner_log: false,
            show_spe_log: false,
            spe_peak_power: 0,
            spe_peak_time: std::time::Instant::now(),
            drive_level: Arc::new(AtomicU8::new(0)),
            macro_slots: macros::load(),
            macro_runner: MacroRunner::new(),
            macro_cat_tx: None,
            show_macro_editor: false,
            editor_slot: 0,
            editor_label: String::new(),
            editor_actions: Vec::new(),
            tuner_window_pos: config.tuner_window_pos,
            amplitec_window_pos: config.amplitec_window_pos,
            active_pa: Arc::new(AtomicU8::new(active_pa_val)),
            vfo_freq_shared: Arc::new(AtomicU64::new(0)),
            vfo_b_freq_shared: Arc::new(AtomicU64::new(0)),
            rf2k_show_debug: false,
            rf2k_show_drive_config: false,
            rf2k_confirm_high_power: false,
            rf2k_confirm_zero_fram: false,
            rf2k_confirm_fw_close: false,
            rf2k_drive_edit: [[0; 11]; 3],
            rf2k_drive_loaded: false,
            ultrabeam_port: config.ultrabeam_port.unwrap_or_default(),
            ultrabeam_enabled: config.ultrabeam_enabled,
            ultrabeam: None,
            show_ultrabeam_window: config.show_ultrabeam_window,
            // ultrabeam_show_menu initialized below - load from config
            ultrabeam_window_pos: config.ultrabeam_window_pos,
            ultrabeam_show_menu: config.ultrabeam_show_menu,
            ultrabeam_confirm_retract: false,
            ultrabeam_confirm_calibrate: false,
            ultrabeam_auto_track: false,
            ultrabeam_last_auto_khz: 0,
            rotor_addr: config.rotor_addr.unwrap_or_default(),
            rotor_enabled: config.rotor_enabled,
            rotor: None,
            show_rotor_window: config.show_rotor_window,
            rotor_window_pos: config.rotor_window_pos,
            rotor_goto_input: String::new(),
            rotor_backend: config.rotor_backend,
            pstrotator_host: config.pstrotator_host,
            pstrotator_port: config.pstrotator_port,
            pstrotator_feedback_port: config.pstrotator_feedback_port,
            pstrotator_has_elevation: config.pstrotator_has_elevation,
            pstrotator_listen_enabled: config.pstrotator_listen_enabled,
            pstrotator_listen_port: config.pstrotator_listen_port,
            tuner_window_init_applied: false,
            amplitec_window_init_applied: false,
            spe_window_init_applied: false,
            rf2k_window_init_applied: false,
            ultrabeam_window_init_applied: false,
            rotor_window_init_applied: false,
            dxcluster_server: config.dxcluster_server.clone(),
            dxcluster_callsign: config.dxcluster_callsign.clone(),
            dxcluster_enabled: config.dxcluster_enabled,
            dxcluster_expiry_min: config.dxcluster_expiry_min,
            password: config.password.clone().unwrap_or_default(),
            totp_enabled: config.totp_enabled,
            friendly_name: config.friendly_name.clone().unwrap_or_default(),
            relay_enabled: config.relay_enabled,
            relay_url: config.relay_url.clone(),
            relay_station: config.relay_station.clone(),
            relay_token: config.relay_token.clone(),
            relay_udp_enabled: config.relay_udp_enabled,
            totp_secret: config.totp_secret.clone().unwrap_or_else(|| sdr_remote_core::auth::generate_totp_secret()),
            main_window_pos: config.main_window_pos,
            theme_variant: sdr_remote_theme::ThemeVariant::from_str(&config.theme),
            theme_custom: sdr_remote_theme::Palette::from_config_string(&config.theme_custom)
                .unwrap_or_else(sdr_remote_theme::Palette::slate),
            autostart: config.autostart,
            pending_autostart: config.autostart,
            main_window_size: config.main_window_size,
            tuner_window_size: config.tuner_window_size,
            amplitec_window_size: config.amplitec_window_size,
            spe_window_size: config.spe_window_size,
            rf2k_window_size: config.rf2k_window_size,
            ultrabeam_window_size: config.ultrabeam_window_size,
            rotor_window_size: config.rotor_window_size,
            show_about: false,
            status_panel_state: None,
            status_bind_addr: format!("0.0.0.0:{}", sdr_remote_core::DEFAULT_PORT),
            status_view: StatusView::Status,
            show_layout_arranger: false,
            layout_grid_per_monitor: Vec::new(),
            layout_active_item: None,
            layout_drag_anchor: None,
            layout_target_monitor: 0,
            ui_language: config.language.clone(),
        }
    }

    fn start_server(&mut self) {
        // Clear log buffer for fresh start
        self.log_buffer.lock().unwrap().clear();

        let thetis = self.thetis_path.trim().to_string();
        let yaesu_port_str = self.yaesu_port.trim().to_string();
        let amp_port = self.amplitec_port.trim().to_string();
        let spe_port_str = self.spe_port.trim().to_string();
        let rf2k_addr_str = self.rf2k_addr.trim().to_string();
        let ub_port = self.ultrabeam_port.trim().to_string();
        let rotor_addr_str = self.rotor_addr.trim().to_string();
        let config = ServerConfig {
            spectrum_enabled: true,
            thetis_path: if thetis.is_empty() { None } else { Some(thetis) },
            yaesu_port: if yaesu_port_str.is_empty() { None } else { Some(yaesu_port_str.clone()) },
            yaesu_enabled: self.yaesu_enabled,
            yaesu_ssb_switch_on_ptt: self.yaesu_ssb_switch_on_ptt,
            yaesu_baud: 38400,
            yaesu_audio_device: if self.yaesu_audio_device.is_empty() { None } else { Some(self.yaesu_audio_device.clone()) },
            yaesu_audio_output_device: if self.yaesu_audio_output_device.is_empty() { None } else { Some(self.yaesu_audio_output_device.clone()) },
            // Dual-radio slot 1 (radio 2) - nu uit de settings-UI. Baud + audio-channel
            // blijven van disk (baud autodetect via detect_model; channel = FTX-1 L/R).
            yaesu2_port: if self.yaesu2_port.is_empty() { None } else { Some(self.yaesu2_port.clone()) },
            yaesu2_enabled: self.yaesu2_enabled,
            yaesu2_baud: crate::config::load().yaesu2_baud,
            yaesu2_audio_device: if self.yaesu2_audio_device.is_empty() { None } else { Some(self.yaesu2_audio_device.clone()) },
            yaesu2_audio_output_device: if self.yaesu2_audio_output_device.is_empty() { None } else { Some(self.yaesu2_audio_output_device.clone()) },
            yaesu2_audio_channel: crate::config::load().yaesu2_audio_channel,
            amplitec_port: if amp_port.is_empty() { None } else { Some(amp_port.clone()) },
            amplitec_enabled: self.amplitec_enabled,
            amplitec_labels: self.amplitec_labels.clone(),
            amplitec_max_w: crate::config::load().amplitec_max_w,
            amplitec_tx_blocked: crate::config::load().amplitec_tx_blocked,
            show_amplitec_window: self.show_amplitec_window,
            show_tuner_window: self.show_tuner_window,
            spe_port: if spe_port_str.is_empty() { None } else { Some(spe_port_str.clone()) },
            spe_enabled: self.spe_enabled,
            show_spe_window: self.show_spe_window,
            rf2k_addr: if rf2k_addr_str.is_empty() { None } else { Some(rf2k_addr_str.clone()) },
            rf2k_enabled: self.rf2k_enabled,
            show_rf2k_window: self.show_rf2k_window,
            ultrabeam_port: if ub_port.is_empty() { None } else { Some(ub_port.clone()) },
            ultrabeam_enabled: self.ultrabeam_enabled,
            show_ultrabeam_window: self.show_ultrabeam_window,
            rotor_addr: if rotor_addr_str.is_empty() { None } else { Some(rotor_addr_str.clone()) },
            rotor_enabled: self.rotor_enabled,
            show_rotor_window: self.show_rotor_window,
            rotor_backend: self.rotor_backend.clone(),
            pstrotator_host: self.pstrotator_host.clone(),
            pstrotator_port: self.pstrotator_port,
            pstrotator_feedback_port: self.pstrotator_feedback_port,
            pstrotator_has_elevation: self.pstrotator_has_elevation,
            pstrotator_listen_enabled: self.pstrotator_listen_enabled,
            pstrotator_listen_port: self.pstrotator_listen_port,
            tuner_window_pos: self.tuner_window_pos,
            amplitec_window_pos: self.amplitec_window_pos,
            spe_window_pos: self.spe_window_pos,
            rf2k_window_pos: self.rf2k_window_pos,
            ultrabeam_window_pos: self.ultrabeam_window_pos,
            rotor_window_pos: self.rotor_window_pos,
            main_window_pos: self.main_window_pos,
            main_window_size: self.main_window_size,
            tuner_window_size: self.tuner_window_size,
            amplitec_window_size: self.amplitec_window_size,
            spe_window_size: self.spe_window_size,
            rf2k_window_size: self.rf2k_window_size,
            ultrabeam_window_size: self.ultrabeam_window_size,
            rotor_window_size: self.rotor_window_size,
            theme: self.theme_variant.as_str().to_string(),
            theme_custom: self.theme_custom.to_config_string(),
            language: self.ui_language.clone(),
            autostart: self.autostart,
            active_pa: self.active_pa.load(Ordering::Relaxed),
            // Preserve the persisted per-PA pre-Operate snapshot values; the
            // RF2K observer is the only writer (see rf2k.rs save_saved_drive
            // call). Reading them from `load()` here keeps `start_server()`
            // from clobbering the snapshot back to None on every restart.
            rf2k_saved_drive: crate::config::load().rf2k_saved_drive,
            spe_saved_drive: crate::config::load().spe_saved_drive,
            ultrabeam_show_menu: self.ultrabeam_show_menu,
            mcp2221_section_expanded: crate::config::load().mcp2221_section_expanded,
            // Preserve the multi-tuner schema across UI saves - until the
            // settings UI exposes tuner1/tuner2 the values just round-trip
            // through whatever was last loaded from disk.
            tuners: crate::config::load().tuners,
            rotors: crate::config::load().rotors,
            tci_addr: if self.tci_addr.trim().is_empty() { None } else { Some(self.tci_addr.trim().to_string()) },
            rx2_present: self.rx2_present,
            dxcluster_server: self.dxcluster_server.clone(),
            dxcluster_callsign: self.dxcluster_callsign.clone(),
            dxcluster_enabled: self.dxcluster_enabled,
            dxcluster_expiry_min: self.dxcluster_expiry_min,
            password: if self.password.is_empty() { None } else { Some(self.password.clone()) },
            totp_secret: if self.totp_enabled { Some(self.totp_secret.clone()) } else { None },
            totp_enabled: self.totp_enabled,
            friendly_name: if self.friendly_name.trim().is_empty() {
                None
            } else {
                Some(self.friendly_name.trim().to_string())
            },
            relay_enabled: self.relay_enabled,
            relay_url: self.relay_url.trim().to_string(),
            relay_station: self.relay_station.trim().to_string(),
            relay_token: self.relay_token.clone(),
            relay_udp_enabled: self.relay_udp_enabled,
        };
        crate::config::save(&config);

        let com_timeout = Duration::from_secs(5);

        // Create Yaesu FT-991A serial connection
        if !yaesu_port_str.is_empty() && self.yaesu_enabled {
            let port = yaesu_port_str;
            let baud = config.yaesu_baud;
            let port_log = port.clone();
            let audio_dev = self.yaesu_audio_device.clone();
            let audio_dev_opt = if audio_dev.is_empty() { None } else { Some(audio_dev) };
            let audio_out = self.yaesu_audio_output_device.clone();
            let audio_out_opt = if audio_out.is_empty() { None } else { Some(audio_out) };
            // Slot-0 model per-poort autodetect (dual-radio): zo werkt élke
            // combinatie incl. een FTX-1 als primaire radio. Detect + open
            // draaien in de timeout-thread (blokkeert de UI-thread niet).
            // Faalt detect (radio uit) -> 991A-aanname-label; bring-up logt echt ID.
            let ssb_on_ptt = config.yaesu_ssb_switch_on_ptt;
            match with_timeout(com_timeout, move || {
                let (model, det_baud) = crate::yaesu::detect_model(&port, baud)
                    .unwrap_or((crate::yaesu::RadioModel::Ft991a, baud));
                crate::yaesu::YaesuRadio::new_with_model(&port, det_baud, audio_dev_opt.as_deref(), audio_out_opt.as_deref(), model, 0, 0, ssb_on_ptt)
            }) {
                Ok(radio) => {
                    // YaesuRadio is fail-soft: the underlying serial open
                    // may have failed at probe-time. The actual connect/
                    // not-detected log line is emitted inside YaesuRadio::new()
                    // itself so we don't shadow it with a misleading
                    // "connected" message here.
                    log::debug!("Yaesu FT-991A instance created for {}", port_log);
                    self.yaesu = Some(Arc::new(radio));
                }
                Err(e) => {
                    log::warn!("Yaesu init failed: {}", e);
                }
            }
        }

        // Create AmplitecSwitch early so UI can access it too. De
        // worker-thread retry zelf bij offline device, dus we maken
        // de instance ook als het bord nu niet bereikbaar is - anders
        // verscheen het Amplitec-venster niet bij offline-start en
        // kwam het ook niet vanzelf terug na een power-cycle (de
        // oude thread brak op het eerste read-failure).
        let amplitec = if !amp_port.is_empty() && self.amplitec_enabled {
            log::info!("Amplitec 6/2 starting on {} (thread retries until reachable)", amp_port);
            Some(Arc::new(AmplitecSwitch::new(&amp_port)))
        } else {
            None
        };

        self.amplitec = amplitec.clone();
        self.amplitec_labels = config.amplitec_labels.clone();

        // Create shared CAT channel for tuner + macros
        let (cat_tx, cat_rx) = tokio::sync::mpsc::channel::<String>(16);
        self.macro_cat_tx = Some(cat_tx.clone());

        // Create SPE Expert early (before tuner, so tuner can reference it for safe tune)
        let spe_arc = if !spe_port_str.is_empty() && self.spe_enabled {
            let port = spe_port_str.clone();
            match with_timeout(com_timeout, move || SpeExpert::new(&port)) {
                Ok(dev) => {
                    log::info!("SPE Expert connected on {}", spe_port_str);
                    let arc_dev = Arc::new(dev);
                    self.spe = Some(arc_dev.clone());
                    Some(arc_dev)
                }
                Err(e) => {
                    log::warn!("SPE Expert init failed: {}", e);
                    None
                }
            }
        } else {
            None
        };

        // Create RF2K-S if configured (before tuner, so tuner can reference it for safe tune)
        let rf2k_arc: Option<Arc<Rf2k>> = if !rf2k_addr_str.is_empty() && self.rf2k_enabled {
            log::info!("RF2K-S connecting to {}", rf2k_addr_str);
            let rf = Arc::new(Rf2k::new(&rf2k_addr_str, Some(cat_tx.clone()), Some(self.drive_level.clone())));
            self.rf2k = Some(rf.clone());
            Some(rf)
        } else {
            None
        };

        // Build StockCorner tuner collection (post-MCP2221A refactor). Each
        // enabled `config.tuners` slot tries to open its MCP2221A board; we
        // don't fail server-start when a board is unplugged (the tuner thread
        // will retry on the next Tune press). The primary (first enabled)
        // tuner is kept in `self.tuner` for the legacy single-tuner UI / macro
        // paths; the full collection is passed downstream for per-position
        // routing in network.rs.
        let tuners_arc = {
            let tuner_configs = config.tuners.clone();
            let spe_ref = spe_arc.clone();
            let rf2k_ref = rf2k_arc.clone();
            let collection = crate::tuner::Tuners::new(&tuner_configs, cat_tx, spe_ref, rf2k_ref);
            let arc_collection = Arc::new(collection);
            self.tuner = arc_collection.primary();
            if !arc_collection.is_empty() {
                log::info!("Tuners online: {} instance(s)", arc_collection.instances().len());
            }
            Some(arc_collection)
        };
        // Create UltraBeam if configured
        if !ub_port.is_empty() && self.ultrabeam_enabled {
            let port = ub_port.clone();
            match with_timeout(com_timeout, move || UltraBeam::new(&port)) {
                Ok(dev) => {
                    log::info!("UltraBeam RCU-06 connected on {}", ub_port);
                    self.ultrabeam = Some(Arc::new(dev));
                }
                Err(e) => {
                    log::warn!("UltraBeam init failed: {}", e);
                }
            }
        }

        // Create Rotor if configured - backend keuze: EA7HG, PstRotator
        // of Adafruit MCP2221A (PATCH-yaesu-rotor-mcp2221).
        // RotorInstance voor mcp2221_yaesu wordt tijdelijk bewaard in
        // `pending_yaesu_rotor` zodat we 'm na het aanmaken van
        // status_panel_state (verderop in deze fn) kunnen publiceren
        // in rotor_slot.
        let mut pending_yaesu_rotor: Option<
            Arc<crate::mcp2221_yaesu_rotor::RotorInstance>,
        > = None;
        if self.rotor_enabled {
            match self.rotor_backend.as_str() {
                "pstrotator" => {
                    let host = self.pstrotator_host.trim().to_string();
                    if host.is_empty() {
                        log::warn!(
                            "PstRotator backend selected but host is empty; rotor disabled"
                        );
                    } else {
                        log::info!(
                            "Rotor (PstRotator) -> {}:{} (feedback :{}, ele={})",
                            host,
                            self.pstrotator_port,
                            self.pstrotator_feedback_port,
                            self.pstrotator_has_elevation,
                        );
                        let (tx, status) =
                            crate::pstrotator::spawn(crate::pstrotator::PstRotatorConfig {
                                host,
                                port: self.pstrotator_port,
                                feedback_port: self.pstrotator_feedback_port,
                                has_elevation: self.pstrotator_has_elevation,
                            });
                        self.rotor =
                            Some(Arc::new(crate::rotor::Rotor::from_handles(tx, status)));
                    }
                }
                "mcp2221_yaesu" => {
                    let rotors_cfg = crate::config::load().rotors;
                    if let Some(rot_cfg) = rotors_cfg.first() {
                        if rot_cfg.enabled && rot_cfg.mcp_serial.starts_with("rot_") {
                            let label = if rot_cfg.name.is_empty() {
                                rot_cfg.mcp_serial.clone()
                            } else {
                                rot_cfg.name.clone()
                            };
                            let calibration =
                                crate::mcp2221_yaesu_rotor::RotorCalibration {
                                    v_at_0deg: rot_cfg.v_at_0deg,
                                    v_at_max_deg: rot_cfg.v_at_max_deg,
                                    max_deg: rot_cfg.max_deg,
                                    ramp_pct_per_sec: rot_cfg.ramp_pct_per_sec,
                                    shortest_route_in_overlap: rot_cfg.shortest_route_in_overlap,
                                };
                            let inst = crate::mcp2221_yaesu_rotor::RotorInstance::new(
                                0,
                                &rot_cfg.mcp_serial,
                                &label,
                                calibration,
                            );
                            let facade = inst.make_rotor_facade();
                            self.rotor =
                                Some(Arc::new(facade));
                            pending_yaesu_rotor = Some(inst);
                            log::info!(
                                "Rotor (Adafruit MCP2221A) serial=\"{}\" label=\"{}\" cal {:.3}V->{:.3}V @ {}°",
                                rot_cfg.mcp_serial,
                                label,
                                rot_cfg.v_at_0deg,
                                rot_cfg.v_at_max_deg,
                                rot_cfg.max_deg,
                            );
                        } else {
                            log::warn!(
                                "mcp2221_yaesu backend geselecteerd maar config.rotors[0] is leeg of disabled - gebruik wizard om een rot_<naam> bord te claimen"
                            );
                        }
                    } else {
                        log::warn!(
                            "mcp2221_yaesu backend geselecteerd maar geen rotor in config.rotors"
                        );
                    }
                }
                _ => {
                    // EA7HG default (legacy "ea7hg" of leeg)
                    if !rotor_addr_str.is_empty() {
                        log::info!("Rotor (EA7HG) connecting to {}", rotor_addr_str);
                        self.rotor =
                            Some(Arc::new(crate::rotor::Rotor::new(&rotor_addr_str)));
                    }
                }
            }
        }

        // PstRotator UDP-listener (v2.1.1+) wordt server-side gespawnd in
        // main.rs::run_server_async met de pre-built rotor_inst - daar
        // hoort dit hoor; hier in ui/mod.rs zou een tweede spawn een
        // "address already in use" bind-conflict op de poort geven.

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        self.shutdown_tx = Some(shutdown_tx);

        // Let previous server thread finish in background
        self.server_thread.take();

        let drive_level_shared = self.drive_level.clone();
        let active_pa_shared = self.active_pa.clone();
        let vfo_freq_shared = self.vfo_freq_shared.clone();
        let vfo_b_freq_shared = self.vfo_b_freq_shared.clone();
        let ultrabeam_for_net = self.ultrabeam.clone();
        let rotor_for_net = self.rotor.clone();
        let yaesu_for_net = self.yaesu.clone();
        // PATCH-2: build the Status-panel state bundle and keep a clone for the UI.
        let status_panel_state = crate::audio_stats::StatusPanelShared::new();
        self.status_panel_state = Some(status_panel_state.clone());
        // Publiceer eventuele Adafruit-rotor instance in het status-panel
        // slot zodat het rotor-paneel (live ADC + Park-knoppen + DAC-slider)
        // verschijnt parallel met de standaard rotor-window.
        if let Some(inst) = pending_yaesu_rotor.take() {
            let _ = status_panel_state.rotor_slot.set(inst);
        }
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
            rt.block_on(async {
                if let Err(e) = crate::run_server_async(config, shutdown_rx, amplitec, tuners_arc, spe_arc, rf2k_arc, ultrabeam_for_net, rotor_for_net, Some(cat_rx), Some(drive_level_shared), Some(active_pa_shared), Some(vfo_freq_shared), Some(vfo_b_freq_shared), yaesu_for_net, Some(status_panel_state)).await {
                    log::error!("Server error: {}", e);
                }
            });
        });
        self.server_thread = Some(handle);

        self.mode = Mode::Running;
    }

    fn save_window_positions(&self) {
        let mut config = crate::config::load();
        config.tuner_window_pos = self.tuner_window_pos;
        config.amplitec_window_pos = self.amplitec_window_pos;
        config.spe_window_pos = self.spe_window_pos;
        config.rf2k_window_pos = self.rf2k_window_pos;
        config.ultrabeam_window_pos = self.ultrabeam_window_pos;
        config.rotor_window_pos = self.rotor_window_pos;
        config.main_window_pos = self.main_window_pos;
        config.main_window_size = self.main_window_size;
        config.tuner_window_size = self.tuner_window_size;
        config.amplitec_window_size = self.amplitec_window_size;
        config.spe_window_size = self.spe_window_size;
        config.rf2k_window_size = self.rf2k_window_size;
        config.ultrabeam_window_size = self.ultrabeam_window_size;
        config.rotor_window_size = self.rotor_window_size;
        config.theme = self.theme_variant.as_str().to_string();
        config.theme_custom = self.theme_custom.to_config_string();
        config.language = self.ui_language.clone();
        config.active_pa = self.active_pa.load(Ordering::Relaxed);
        crate::config::save(&config);
    }

    // ===== "Vensters schikken" matrix-plaatser =====

    fn viewport_native_ppp(ctx: &egui::Context) -> f32 {
        ctx.input(|i| i.viewport().native_pixels_per_point)
            .unwrap_or_else(|| ctx.pixels_per_point())
    }

    /// Is dit venster BESCHIKBAAR om te schikken? = die backend draait (Arc bestaat).
    /// Het hoofdvenster is er altijd.
    fn snap_is_available(&self, w: SnapWindow) -> bool {
        match w {
            SnapWindow::Main => true,
            SnapWindow::Tuner => self.tuner.is_some(),
            SnapWindow::Amplitec => self.amplitec.is_some(),
            SnapWindow::Spe => self.spe.is_some(),
            SnapWindow::Rf2k => self.rf2k.is_some(),
            SnapWindow::Ultrabeam => self.ultrabeam.is_some(),
            SnapWindow::Rotor => self.rotor.is_some(),
        }
    }

    /// Open het bijbehorende popout-venster (indien nog dicht) + reset init zodat
    /// de nieuwe geometrie wordt toegepast. Zo verschijnt een geplaatst-maar-dicht
    /// venster na "Toepassen".
    fn snap_open(&mut self, w: SnapWindow) {
        match w {
            SnapWindow::Main => {}
            SnapWindow::Tuner => { self.show_tuner_window = true; self.tuner_window_init_applied = false; }
            SnapWindow::Amplitec => { self.show_amplitec_window = true; self.amplitec_window_init_applied = false; }
            SnapWindow::Spe => { self.show_spe_window = true; self.spe_window_init_applied = false; }
            SnapWindow::Rf2k => { self.show_rf2k_window = true; self.rf2k_window_init_applied = false; }
            SnapWindow::Ultrabeam => { self.show_ultrabeam_window = true; self.ultrabeam_window_init_applied = false; }
            SnapWindow::Rotor => { self.show_rotor_window = true; self.rotor_window_init_applied = false; }
        }
    }

    /// Zet pos+size van een popout (root-viewport via ViewportCommand in apply_layout).
    fn snap_set_geometry(&mut self, w: SnapWindow, pos: [f32; 2], size: [f32; 2]) {
        match w {
            SnapWindow::Main => {}
            SnapWindow::Tuner => { self.tuner_window_pos = Some(pos); self.tuner_window_size = Some(size); self.tuner_window_init_applied = false; }
            SnapWindow::Amplitec => { self.amplitec_window_pos = Some(pos); self.amplitec_window_size = Some(size); self.amplitec_window_init_applied = false; }
            SnapWindow::Spe => { self.spe_window_pos = Some(pos); self.spe_window_size = Some(size); self.spe_window_init_applied = false; }
            SnapWindow::Rf2k => { self.rf2k_window_pos = Some(pos); self.rf2k_window_size = Some(size); self.rf2k_window_init_applied = false; }
            SnapWindow::Ultrabeam => { self.ultrabeam_window_pos = Some(pos); self.ultrabeam_window_size = Some(size); self.ultrabeam_window_init_applied = false; }
            SnapWindow::Rotor => { self.rotor_window_pos = Some(pos); self.rotor_window_size = Some(size); self.rotor_window_init_applied = false; }
        }
    }

    /// Index van de monitor waar het hoofdvenster staat (default-scherm bij openen).
    fn detect_monitor_index(&self, ctx: &egui::Context) -> usize {
        let ppp = Self::viewport_native_ppp(ctx).max(0.1);
        if let (Some(areas), Some((mx, my))) = (
            window_placement::monitor_work_areas_px(),
            ctx.input(|i| i.viewport().outer_rect)
                .map(|r| ((r.min.x * ppp) as i32, (r.min.y * ppp) as i32)),
        ) {
            if let Some(idx) = areas.iter().position(|a|
                mx >= a.left && mx < a.right && my >= a.top && my < a.bottom)
            {
                return idx;
            }
        }
        0
    }

    /// Snap de vensters uit de matrices op het scherm: elk venster over zijn
    /// omhullende cel-rechthoek. Hoofdvenster via ViewportCommand, popouts via
    /// snap_set_geometry (+ eerst openen).
    fn apply_layout(&mut self, ctx: &egui::Context) {
        let ppp = Self::viewport_native_ppp(ctx).max(0.1);
        let areas = window_placement::monitor_work_areas_px();
        let per_mon: Vec<LayoutGrid> = self.layout_grid_per_monitor.clone();
        const TITLE_H: f32 = 36.0;
        const GAP: f32 = 4.0;
        let mut placed_total = 0usize;
        for (m, grid) in per_mon.iter().enumerate() {
            let placed = grid.placed();
            if placed.is_empty() { continue; }
            let (ax, ay, aw, ah) = match areas.as_ref().and_then(|list| list.get(m).copied()) {
                Some(a) => (
                    a.left as f32 / ppp,
                    a.top as f32 / ppp,
                    (a.right - a.left) as f32 / ppp,
                    (a.bottom - a.top) as f32 / ppp,
                ),
                None if m == 0 => {
                    let sr = ctx.screen_rect();
                    (sr.min.x, sr.min.y, sr.width(), sr.height())
                }
                None => continue,
            };
            let col_w = aw / grid.cols as f32;
            let row_h = ah / grid.rows as f32;
            for w in placed {
                let Some((minr, minc, maxr, maxc)) = grid.bounds(w) else { continue };
                self.snap_open(w);
                let span_c = (maxc - minc + 1) as f32;
                let span_r = (maxr - minr + 1) as f32;
                let x = ax + col_w * minc as f32;
                let y = ay + row_h * minr as f32;
                let iw = (col_w * span_c - GAP).max(200.0);
                let ih = (row_h * span_r - TITLE_H).max(120.0);
                if w == SnapWindow::Main {
                    self.main_window_pos = Some([x, y]);
                    self.main_window_size = Some([iw, ih]);
                    ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(x, y)));
                    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(iw, ih)));
                } else {
                    self.snap_set_geometry(w, [x, y], [iw, ih]);
                }
                placed_total += 1;
            }
        }
        if placed_total > 0 {
            self.save_window_positions();
            log::info!("Layout toegepast: {} venster(s) gesnapt", placed_total);
        }
    }

    /// "Vensters schikken" - matrix-plaatser. Kies een rasterformaat (tot 8x8),
    /// selecteer een venster en schilder het over de cellen (klik/sleep). Spanning
    /// over meerdere cellen = venster uitgerekt. Per monitor een eigen raster.
    fn render_layout_arranger(&mut self, ctx: &egui::Context) {
        if !self.show_layout_arranger { return; }

        let n_monitors = window_placement::monitor_work_areas_px()
            .map(|a| a.len()).unwrap_or(1).max(1);
        if self.layout_grid_per_monitor.len() < n_monitors {
            self.layout_grid_per_monitor.resize_with(n_monitors, LayoutGrid::new);
        }
        if self.layout_target_monitor >= n_monitors { self.layout_target_monitor = 0; }

        // Niet-beschikbare vensters (backend niet actief) uit alle rasters + selectie.
        let avail: Vec<SnapWindow> =
            SnapWindow::ALL.iter().copied().filter(|w| self.snap_is_available(*w)).collect();
        for grid in self.layout_grid_per_monitor.iter_mut() {
            for slot in grid.cells.iter_mut() {
                if let Some(w) = *slot { if !avail.contains(&w) { *slot = None; } }
            }
        }
        if let Some(a) = self.layout_active_item {
            if !avail.contains(&a) { self.layout_active_item = None; }
        }

        let cur_mon = self.layout_target_monitor
            .min(self.layout_grid_per_monitor.len().saturating_sub(1));
        let gsnap = self.layout_grid_per_monitor[cur_mon].clone();
        let active = self.layout_active_item;
        let drag_anchor = self.layout_drag_anchor;

        let mut open_flag = self.show_layout_arranger;
        let mut sel_mon = cur_mon;
        let mut resize_to: Option<(u8, u8)> = None;
        let mut paint_cell: Option<(usize, usize)> = None;
        let mut clear_cell: Option<(usize, usize)> = None;
        let mut select: Option<Option<SnapWindow>> = None;
        let mut set_anchor: Option<Option<(usize, usize)>> = None;
        let mut fill_rect: Option<((usize, usize), (usize, usize))> = None;
        let mut clear_all = false;
        let mut do_apply = false;

        egui::Window::new(rust_i18n::t!("srv_arrange_title").to_string())
            .open(&mut open_flag)
            .resizable(true)
            .default_size([360.0, 560.0])
            .show(ctx, |ui| {
              egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
                ui.label(RichText::new(rust_i18n::t!("srv_arrange_help").to_string()).size(11.0).weak());
                ui.add_space(6.0);

                if let Some(areas) = window_placement::monitor_work_areas_px() {
                    if areas.len() > 1 {
                        ui.horizontal(|ui| {
                            ui.label(rust_i18n::t!("srv_screen").to_string());
                            egui::ComboBox::from_id_source("srv_layout_monitor")
                                .selected_text(rust_i18n::t!("srv_screen_n", n = sel_mon + 1).to_string())
                                .show_ui(ui, |ui| {
                                    for i in 0..areas.len() {
                                        ui.selectable_value(&mut sel_mon, i, rust_i18n::t!("srv_screen_n", n = i + 1).to_string());
                                    }
                                });
                        });
                        ui.add_space(6.0);
                    }
                }

                // --- Rasterformaat-kiezer (tot GRID_MAX x GRID_MAX; hover/klik) ---
                const MAXG: usize = GRID_MAX;
                const PCELL: f32 = 15.0;
                const PGAP: f32 = 2.0;
                let dim = PCELL * MAXG as f32 + PGAP * (MAXG as f32 - 1.0);
                ui.horizontal(|ui| {
                    ui.label(rust_i18n::t!("srv_grid").to_string());
                    let (prect, presp) =
                        ui.allocate_exact_size(egui::vec2(dim, dim), egui::Sense::click());
                    let hover = presp.hover_pos().map(|p| {
                        let c = (((p.x - prect.min.x) / (PCELL + PGAP)) as i32).clamp(0, MAXG as i32 - 1) as usize;
                        let r = (((p.y - prect.min.y) / (PCELL + PGAP)) as i32).clamp(0, MAXG as i32 - 1) as usize;
                        (r, c)
                    });
                    let pnt = ui.painter_at(prect);
                    for r in 0..MAXG {
                        for c in 0..MAXG {
                            let cr = egui::Rect::from_min_size(
                                prect.min + egui::vec2(c as f32 * (PCELL + PGAP), r as f32 * (PCELL + PGAP)),
                                egui::vec2(PCELL, PCELL),
                            );
                            let preview = hover.map_or(false, |(hr, hc)| r <= hr && c <= hc);
                            let cur = r < gsnap.rows as usize && c < gsnap.cols as usize;
                            let fill = if preview {
                                SNAP_ACCENT
                            } else if cur {
                                Color32::from_gray(80)
                            } else {
                                Color32::from_gray(45)
                            };
                            pnt.rect_filled(cr, egui::Rounding::same(2.0), fill);
                            pnt.rect_stroke(cr, egui::Rounding::same(2.0), egui::Stroke::new(1.0, Color32::from_gray(100)));
                        }
                    }
                    if let Some((hr, hc)) = hover {
                        if presp.clicked() { resize_to = Some((hr as u8 + 1, hc as u8 + 1)); }
                    }
                    let (lr, lc) = hover
                        .map(|(r, c)| (r + 1, c + 1))
                        .unwrap_or((gsnap.rows as usize, gsnap.cols as usize));
                    ui.label(RichText::new(format!("{} x {}", lr, lc)).strong());
                });

                ui.add_space(6.0);
                ui.separator();

                ui.label(rust_i18n::t!("srv_arrange_pick").to_string());
                ui.horizontal_wrapped(|ui| {
                    for w in &avail {
                        let is_active = active == Some(*w);
                        let txt = RichText::new(w.label()).size(12.0).color(Color32::WHITE);
                        let mut btn = egui::Button::new(txt)
                            .fill(w.color())
                            .rounding(egui::Rounding::same(4.0));
                        if is_active {
                            btn = btn.stroke(egui::Stroke::new(2.0, Color32::WHITE));
                        }
                        if ui.add(btn).clicked() {
                            select = Some(if is_active { None } else { Some(*w) });
                        }
                    }
                });

                ui.add_space(6.0);
                ui.separator();

                ui.label(rust_i18n::t!("srv_arrange_place").to_string());
                let cols = gsnap.cols as usize;
                let rows = gsnap.rows as usize;
                let avail_w = ui.available_width().clamp(120.0, 380.0);
                let cell = (avail_w / cols as f32).clamp(22.0, 88.0);
                let gw = cell * cols as f32;
                let gh = cell * rows as f32;
                let (grect, gresp) =
                    ui.allocate_exact_size(egui::vec2(gw, gh), egui::Sense::click_and_drag());
                let gp = ui.painter_at(grect);
                for r in 0..rows {
                    for c in 0..cols {
                        let cr = egui::Rect::from_min_size(
                            grect.min + egui::vec2(c as f32 * cell, r as f32 * cell),
                            egui::vec2(cell, cell),
                        ).shrink(1.5);
                        let w = gsnap.cell(r, c);
                        let fill = w.map(|w| w.color()).unwrap_or(Color32::from_gray(48));
                        gp.rect_filled(cr, egui::Rounding::same(3.0), fill);
                        gp.rect_stroke(cr, egui::Rounding::same(3.0), egui::Stroke::new(1.0, Color32::from_gray(100)));
                        if let Some(w) = w {
                            gp.text(
                                cr.center(),
                                egui::Align2::CENTER_CENTER,
                                w.label(),
                                egui::FontId::proportional((cell * 0.22).clamp(9.0, 13.0)),
                                Color32::WHITE,
                            );
                        }
                    }
                }
                // Cel onder de aanwijzer: `inside` = strikt binnen het raster
                // (klik/rechtsklik); `clamped` = op de rand geklemd (slepen).
                let ptr = gresp.interact_pointer_pos();
                let clamped = ptr.map(|p| {
                    let c = (((p.x - grect.min.x) / cell) as i32).clamp(0, cols as i32 - 1) as usize;
                    let r = (((p.y - grect.min.y) / cell) as i32).clamp(0, rows as i32 - 1) as usize;
                    (r, c)
                });
                let inside = ptr.and_then(|p| {
                    if !grect.contains(p) { return None; }
                    let c = ((p.x - grect.min.x) / cell) as usize;
                    let r = ((p.y - grect.min.y) / cell) as usize;
                    if r < rows && c < cols { Some((r, c)) } else { None }
                });
                // Sleep-preview: rechthoek anker..huidig oplichten.
                if gresp.dragged() {
                    if let (Some(a), Some(cur), Some(w)) = (drag_anchor, clamped, active) {
                        let (r0, r1) = (a.0.min(cur.0), a.0.max(cur.0));
                        let (c0, c1) = (a.1.min(cur.1), a.1.max(cur.1));
                        let pr = egui::Rect::from_min_max(
                            grect.min + egui::vec2(c0 as f32 * cell, r0 as f32 * cell),
                            grect.min + egui::vec2((c1 + 1) as f32 * cell, (r1 + 1) as f32 * cell),
                        ).shrink(1.5);
                        gp.rect_filled(pr, egui::Rounding::same(3.0), w.color().gamma_multiply(0.55));
                        gp.rect_stroke(pr, egui::Rounding::same(3.0), egui::Stroke::new(2.0, Color32::WHITE));
                    }
                }
                // Slepen = rechthoek vullen; losse klik = 1 cel; rechtsklik /
                // klik-op-actief = wissen.
                if gresp.secondary_clicked() {
                    if let Some((r, c)) = inside { clear_cell = Some((r, c)); }
                } else if gresp.drag_started() {
                    if active.is_some() {
                        if let Some(cell_rc) = inside.or(clamped) { set_anchor = Some(Some(cell_rc)); }
                    }
                } else if gresp.drag_stopped() {
                    if active.is_some() {
                        if let Some(cur) = clamped {
                            fill_rect = Some((drag_anchor.unwrap_or(cur), cur));
                        }
                    }
                    set_anchor = Some(None);
                } else if gresp.clicked() {
                    if let Some((r, c)) = inside {
                        match active {
                            Some(a) if gsnap.cell(r, c) == Some(a) => clear_cell = Some((r, c)),
                            Some(_) => paint_cell = Some((r, c)),
                            None => { if let Some(w) = gsnap.cell(r, c) { select = Some(Some(w)); } }
                        }
                    }
                }

                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(RichText::new(rust_i18n::t!("srv_apply").to_string()).strong())
                        .fill(SNAP_ACCENT)).clicked() { do_apply = true; }
                    if ui.button(rust_i18n::t!("srv_empty").to_string()).clicked() { clear_all = true; }
                });
              }); // ScrollArea
            });

        self.show_layout_arranger = open_flag;
        if sel_mon != cur_mon && sel_mon < self.layout_grid_per_monitor.len() {
            self.layout_target_monitor = sel_mon;
        }
        if let Some(sel) = select { self.layout_active_item = sel; }
        let mon = cur_mon;
        if let Some((rr, cc)) = resize_to { self.layout_grid_per_monitor[mon].set_size(rr, cc); }
        if clear_all {
            for slot in self.layout_grid_per_monitor[mon].cells.iter_mut() { *slot = None; }
        }
        if let Some((r, c)) = clear_cell { self.layout_grid_per_monitor[mon].set(r, c, None); }
        if let Some((r, c)) = paint_cell {
            if let Some(w) = active {
                // Verwijder dit venster eerst uit ALLE rasters (incl. de huidige
                // monitor) zodat oude cellen niet blijven staan en bounds() het
                // venster niet onbedoeld over een grotere rechthoek uitrekt.
                for g in self.layout_grid_per_monitor.iter_mut() { g.remove(w); }
                self.layout_grid_per_monitor[mon].set(r, c, Some(w));
            }
        }
        if let Some(sa) = set_anchor { self.layout_drag_anchor = sa; }
        if let Some((a, b)) = fill_rect {
            if let Some(w) = active {
                // Verwijder dit venster eerst uit ALLE rasters (incl. de huidige
                // monitor) zodat oude cellen niet blijven staan en bounds() het
                // venster niet onbedoeld over een grotere rechthoek uitrekt.
                for g in self.layout_grid_per_monitor.iter_mut() { g.remove(w); }
                let (r0, r1) = (a.0.min(b.0), a.0.max(b.0));
                let (c0, c1) = (a.1.min(b.1), a.1.max(b.1));
                let g = &mut self.layout_grid_per_monitor[mon];
                for r in r0..=r1 { for c in c0..=c1 { g.set(r, c, Some(w)); } }
            }
        }
        if do_apply { self.apply_layout(ctx); }
    }
}

impl eframe::App for ServerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Gedeeld thema met de client. "Classic" is byte-voor-byte het oorspronkelijke
        // lichtgrijze schema van dit venster, dus de standaard verandert niets.
        sdr_remote_theme::apply_visuals(ctx, self.theme_variant, &self.theme_custom);

        // Auto-start on first frame if configured
        if self.pending_autostart {
            self.pending_autostart = false;
            self.start_server();
        }

        // Refresh in-memory mirror van label-config - wordt door het
        // Amplitec rename-dialog (context-menu) via `modify_config`
        // bijgewerkt, en dit pad zorgt dat de UI in dezelfde frame de
        // nieuwe naam toont zonder server-restart.
        {
            let live_labels = crate::config::load().amplitec_labels.clone();
            if live_labels != self.amplitec_labels {
                self.amplitec_labels = live_labels;
            }
        }

        // Auto-restart handling: een UI-knop (tuner config, slot delete,
        // serial rename) heeft via `request_auto_restart()` aangegeven dat
        // de server moet herstarten. Doe dat hier in de event-loop zodat:
        //   1. Drop-handlers correct runnen op de gestopte hardware-Arcs
        //      (audio cpal-streams + Thetis TCI-WebSocket vrijgeven).
        //   2. Een korte sleep het OS tijd geeft om die handles te
        //      releasen voor de nieuwe child probeert te enumeraten.
        // Voorheen riep restart_server direct process::exit(0) na spawn,
        // wat Drop oversloeg - audio op de nieuwe instance werkte dan
        // vaak niet tot operator handmatig de server afsloot en herstartte.
        if auto_restart_requested() {
            self.save_window_positions();
            if let Some(tx) = self.shutdown_tx.take() {
                let _ = tx.send(true);
            }
            // Drop alle hardware-Arcs -> cpal streams, serial ports en
            // TCI-connection worden via hun eigen Drop-impls afgesloten.
            self.yaesu = None;
            self.amplitec = None;
            self.tuner = None;
            self.spe = None;
            self.rf2k = None;
            self.ultrabeam = None;
            self.rotor = None;
            self.status_panel_state = None;
            // Geef OS tijd om USB-HID + audio-device handles los te laten
            // voor de nieuwe child enumeration start. Empirisch: 500-800
            // ms is voldoende op Windows; 600 ms is een veilige middenweg
            // tussen "audio claimt nog" en "operator merkt de pauze".
            std::thread::sleep(Duration::from_millis(600));
            spawn_replacement_and_exit();
        }
        // Track main window size and position
        if let Some(rect) = ctx.input(|i| i.viewport().inner_rect) {
            self.main_window_size = Some([rect.width(), rect.height()]);
        }
        if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
            self.main_window_pos = Some([rect.left(), rect.top()]);
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            match self.mode {
                Mode::Settings => {
                    // ScrollArea zodat het Settings-paneel ook bruikbaar
                    // blijft bij kleinere venstershoogtes - de Save & Start
                    // knop staat helemaal onderaan en moet altijd bereikbaar
                    // zijn zonder het hele venster groter te slepen.
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.heading(format!("ThetisLink Server v{}", sdr_remote_core::VERSION));
                    ui.add_space(10.0);

                    // Thema-keuze: zelfde varianten als de client. Direct zichtbaar,
                    // want apply_visuals draait elke frame vanuit self.theme_variant.
                    ui.horizontal(|ui| {
                        ui.label(rust_i18n::t!("srv_theme").to_string());
                        egui::ComboBox::from_id_salt("server_theme")
                            .selected_text(self.theme_variant.label())
                            .width(140.0)
                            .show_ui(ui, |ui| {
                                for v in sdr_remote_theme::ThemeVariant::ALL {
                                    if ui.selectable_label(self.theme_variant == v, v.label()).clicked() {
                                        self.theme_variant = v;
                                    }
                                }
                            });
                        if self.theme_variant == sdr_remote_theme::ThemeVariant::Custom {
                            let mut edit = |label: &str, c: &mut egui::Color32| {
                                let mut rgb = [c.r(), c.g(), c.b()];
                                if ui.color_edit_button_srgb(&mut rgb).on_hover_text(label).changed() {
                                    *c = egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
                                }
                            };
                            edit("Achtergrond", &mut self.theme_custom.background);
                            edit("Widgets", &mut self.theme_custom.widget);
                            edit("Tekst", &mut self.theme_custom.text);
                            edit("Accent (sliders)", &mut self.theme_custom.accent);
                        }
                    });

                    // UI-taal: basis EN + keuze NL/DE/FR. Direct toegepast
                    // (set_locale) en bewaard (language= in de server-conf).
                    // Gefaseerde migratie: nog-niet-gemigreerde teksten blijven NL.
                    ui.horizontal(|ui| {
                        ui.label(rust_i18n::t!("language").to_string());
                        let langs = [("en", "English"), ("nl", "Nederlands"), ("de", "Deutsch"), ("fr", "Francais")];
                        let cur_name = langs.iter().find(|(c, _)| *c == self.ui_language).map(|(_, n)| *n).unwrap_or("Nederlands");
                        let mut picked: Option<&str> = None;
                        egui::ComboBox::from_id_salt("server_language")
                            .selected_text(cur_name)
                            .width(140.0)
                            .show_ui(ui, |ui| {
                                for (code, name) in langs {
                                    if ui.selectable_label(self.ui_language == code, name).clicked() {
                                        picked = Some(code);
                                    }
                                }
                            });
                        if let Some(code) = picked {
                            self.ui_language = code.to_string();
                            rust_i18n::set_locale(code);
                            self.save_window_positions();
                        }
                    });

                    ui.add_space(8.0);

                    ui.label(rust_i18n::t!("srv_tci_addr").to_string());
                    ui.text_edit_singleline(&mut self.tci_addr);

                    ui.add_space(8.0);

                    ui.label(rust_i18n::t!("srv_thetis_path").to_string());
                    ui.text_edit_singleline(&mut self.thetis_path);

                    ui.add_space(8.0);

                    ui.checkbox(&mut self.rx2_present, rust_i18n::t!("srv_rx2_present").to_string())
                        .on_hover_text(rust_i18n::t!("srv_rx2_present_hover").to_string());

                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.yaesu_enabled, rust_i18n::t!("srv_yaesu_radio1").to_string());
                        ui.label("CAT:");
                        egui::ComboBox::from_id_salt("yaesu_port")
                            .selected_text(if self.yaesu_port.is_empty() { rust_i18n::t!("srv_none").to_string() } else { self.yaesu_port.clone() })
                            .width(120.0)
                            .show_ui(ui, |ui| {
                                if ui.selectable_label(self.yaesu_port.is_empty(), rust_i18n::t!("srv_none").to_string()).clicked() {
                                    self.yaesu_port.clear();
                                }
                                for port in &self.serial_ports {
                                    if ui.selectable_label(*port == self.yaesu_port, port).clicked() {
                                        self.yaesu_port = port.clone();
                                    }
                                }
                            });
                        ui.label(rust_i18n::t!("srv_audio_in").to_string());
                        egui::ComboBox::from_id_salt("yaesu_audio")
                            .selected_text(if self.yaesu_audio_device.is_empty() { rust_i18n::t!("srv_none").to_string() } else { self.yaesu_audio_device.clone() })
                            .width(200.0)
                            .show_ui(ui, |ui| {
                                if ui.selectable_label(self.yaesu_audio_device.is_empty(), rust_i18n::t!("srv_none").to_string()).clicked() {
                                    self.yaesu_audio_device.clear();
                                }
                                for name in crate::yaesu::available_audio_inputs() {
                                    if ui.selectable_label(name == self.yaesu_audio_device, &name).clicked() {
                                        self.yaesu_audio_device = name;
                                    }
                                }
                            });
                    });
                    // TX/output device: apart kiesbaar zodat de zend-audio altijd naar
                    // de juiste codec gaat (PATCH-yaesu-output-device). Leeg = zelfde als
                    // de input; kies dit als capture/render-endpoint anders heten.
                    ui.horizontal(|ui| {
                        ui.label(rust_i18n::t!("srv_audio_out_tx").to_string());
                        egui::ComboBox::from_id_salt("yaesu_audio_out")
                            .selected_text(if self.yaesu_audio_output_device.is_empty() { rust_i18n::t!("srv_same_as_input").to_string() } else { self.yaesu_audio_output_device.clone() })
                            .width(200.0)
                            .show_ui(ui, |ui| {
                                if ui.selectable_label(self.yaesu_audio_output_device.is_empty(), rust_i18n::t!("srv_same_as_input").to_string()).clicked() {
                                    self.yaesu_audio_output_device.clear();
                                }
                                for name in crate::yaesu::available_audio_outputs() {
                                    if ui.selectable_label(name == self.yaesu_audio_output_device, &name).clicked() {
                                        self.yaesu_audio_output_device = name;
                                    }
                                }
                            });
                    });

                    ui.add_space(4.0);

                    // 991A SSB/AM USB routing mode. Off (default): routing stays active while a client
                    // is connected, then restores ~2 s after disconnect. On: switch only during PTT.
                    // FTX-1 keeps its internal auto source selection either way.
                    ui.checkbox(&mut self.yaesu_ssb_switch_on_ptt, rust_i18n::t!("srv_991a_ptt_switch").to_string())
                        .on_hover_text(rust_i18n::t!("srv_991a_ptt_switch_hover").to_string());
                    ui.add_space(4.0);

                    // Dual-radio slot 1 (radio 2) - zelfde opzet als radio 1.
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.yaesu2_enabled, rust_i18n::t!("srv_yaesu_radio2").to_string());
                        ui.label("CAT:");
                        egui::ComboBox::from_id_salt("yaesu2_port")
                            .selected_text(if self.yaesu2_port.is_empty() { rust_i18n::t!("srv_none").to_string() } else { self.yaesu2_port.clone() })
                            .width(120.0)
                            .show_ui(ui, |ui| {
                                if ui.selectable_label(self.yaesu2_port.is_empty(), rust_i18n::t!("srv_none").to_string()).clicked() {
                                    self.yaesu2_port.clear();
                                }
                                for port in &self.serial_ports {
                                    if ui.selectable_label(*port == self.yaesu2_port, port).clicked() {
                                        self.yaesu2_port = port.clone();
                                    }
                                }
                            });
                        ui.label(rust_i18n::t!("srv_audio_in").to_string());
                        egui::ComboBox::from_id_salt("yaesu2_audio")
                            .selected_text(if self.yaesu2_audio_device.is_empty() { rust_i18n::t!("srv_none").to_string() } else { self.yaesu2_audio_device.clone() })
                            .width(200.0)
                            .show_ui(ui, |ui| {
                                if ui.selectable_label(self.yaesu2_audio_device.is_empty(), rust_i18n::t!("srv_none").to_string()).clicked() {
                                    self.yaesu2_audio_device.clear();
                                }
                                for name in crate::yaesu::available_audio_inputs() {
                                    if ui.selectable_label(name == self.yaesu2_audio_device, &name).clicked() {
                                        self.yaesu2_audio_device = name;
                                    }
                                }
                            });
                    });
                    ui.horizontal(|ui| {
                        ui.label(rust_i18n::t!("srv_audio_out_tx").to_string());
                        egui::ComboBox::from_id_salt("yaesu2_audio_out")
                            .selected_text(if self.yaesu2_audio_output_device.is_empty() { rust_i18n::t!("srv_same_as_input").to_string() } else { self.yaesu2_audio_output_device.clone() })
                            .width(200.0)
                            .show_ui(ui, |ui| {
                                if ui.selectable_label(self.yaesu2_audio_output_device.is_empty(), rust_i18n::t!("srv_same_as_input").to_string()).clicked() {
                                    self.yaesu2_audio_output_device.clear();
                                }
                                for name in crate::yaesu::available_audio_outputs() {
                                    if ui.selectable_label(name == self.yaesu2_audio_output_device, &name).clicked() {
                                        self.yaesu2_audio_output_device = name;
                                    }
                                }
                            });
                    });
                    ui.label(egui::RichText::new(rust_i18n::t!("srv_radio2_note").to_string()).size(10.0).color(Color32::GRAY));

                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.amplitec_enabled, "Amplitec 6/2");
                        egui::ComboBox::from_id_salt("amplitec_port")
                            .selected_text(if self.amplitec_port.is_empty() { rust_i18n::t!("srv_none").to_string() } else { self.amplitec_port.clone() })
                            .width(200.0)
                            .show_ui(ui, |ui| {
                                if ui.selectable_label(self.amplitec_port.is_empty(), rust_i18n::t!("srv_none").to_string()).clicked() {
                                    self.amplitec_port.clear();
                                }
                                for port in &self.serial_ports {
                                    if ui.selectable_label(*port == self.amplitec_port, port).clicked() {
                                        self.amplitec_port = port.clone();
                                    }
                                }
                            });
                    });

                    if !self.amplitec_port.is_empty() {
                        ui.checkbox(&mut self.show_amplitec_window, rust_i18n::t!("srv_open_at_start").to_string());
                    }

                    ui.add_space(8.0);

                    // JC-4s / JC-3s tuners - geen COM-poort meer. Elke tuner
                    // wordt aangestuurd via een Adafruit MCP2221A USB-HID
                    // breakout en per slot toegewezen in het server status-
                    // paneel onder "MCP2221A tuner bridges". Hier alleen nog
                    // het venster-openen-bij-start vinkje voor het Tuner-
                    // popout-paneel van de primaire tuner.
                    ui.label(
                        egui::RichText::new("JC-4s / JC-3s tuners")
                            .strong(),
                    );
                    ui.label(
                        egui::RichText::new(rust_i18n::t!("srv_tuner_mcp_note").to_string())
                        .small()
                        .weak(),
                    );
                    ui.checkbox(&mut self.show_tuner_window, rust_i18n::t!("srv_open_at_start").to_string());

                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.spe_enabled, "SPE Expert");
                        egui::ComboBox::from_id_salt("spe_port")
                            .selected_text(if self.spe_port.is_empty() { rust_i18n::t!("srv_none").to_string() } else { self.spe_port.clone() })
                            .width(200.0)
                            .show_ui(ui, |ui| {
                                if ui.selectable_label(self.spe_port.is_empty(), rust_i18n::t!("srv_none").to_string()).clicked() {
                                    self.spe_port.clear();
                                }
                                for port in &self.serial_ports {
                                    if ui.selectable_label(*port == self.spe_port, port).clicked() {
                                        self.spe_port = port.clone();
                                    }
                                }
                            });
                    });

                    if !self.spe_port.is_empty() {
                        ui.checkbox(&mut self.show_spe_window, rust_i18n::t!("srv_open_at_start").to_string());
                    }

                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.rf2k_enabled, "RF2K-S");
                        ui.label(rust_i18n::t!("srv_addr_label").to_string());
                        ui.text_edit_singleline(&mut self.rf2k_addr);
                    });
                    if !self.rf2k_addr.is_empty() {
                        ui.checkbox(&mut self.show_rf2k_window, rust_i18n::t!("srv_open_at_start").to_string());
                    }

                    ui.add_space(8.0);

                    // DX Cluster (spot stream). Login = the operator's own callsign
                    // (no password). No hardcoded default - enter your call here; the
                    // cluster stays offline until it is set.
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.dxcluster_enabled, "DX Cluster");
                        ui.label(rust_i18n::t!("srv_call").to_string());
                        ui.text_edit_singleline(&mut self.dxcluster_callsign);
                    });
                    if self.dxcluster_enabled && self.dxcluster_callsign.trim().is_empty() {
                        ui.label(RichText::new(rust_i18n::t!("srv_dxcluster_need_call").to_string())
                            .color(Color32::from_rgb(220, 160, 0)).size(11.0));
                    }

                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.ultrabeam_enabled, "UltraBeam RCU-06");
                        egui::ComboBox::from_id_salt("ultrabeam_port")
                            .selected_text(if self.ultrabeam_port.is_empty() { rust_i18n::t!("srv_none").to_string() } else { self.ultrabeam_port.clone() })
                            .width(200.0)
                            .show_ui(ui, |ui| {
                                if ui.selectable_label(self.ultrabeam_port.is_empty(), rust_i18n::t!("srv_none").to_string()).clicked() {
                                    self.ultrabeam_port.clear();
                                }
                                for port in &self.serial_ports {
                                    if ui.selectable_label(*port == self.ultrabeam_port, port).clicked() {
                                        self.ultrabeam_port = port.clone();
                                    }
                                }
                            });
                    });

                    if !self.ultrabeam_port.is_empty() {
                        ui.checkbox(&mut self.show_ultrabeam_window, rust_i18n::t!("srv_open_at_start").to_string());
                    }

                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.rotor_enabled, "Rotor");
                        ui.label(rust_i18n::t!("srv_backend").to_string());
                        // Snapshot voor change-detect; bij wijziging direct
                        // naar disk persisteren (anders raakt de keuze
                        // weg wanneer operator de server niet via Start
                        // herstart na de dropdown-wijziging).
                        let backend_before = self.rotor_backend.clone();
                        egui::ComboBox::from_id_salt("rotor_backend_combo")
                            .selected_text(match self.rotor_backend.as_str() {
                                "pstrotator" => "PstRotator (XML/UDP)",
                                "mcp2221_yaesu" => "Adafruit MCP2221A -> Yaesu G-1000DXC",
                                _ => "EA7HG Visual Rotor",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.rotor_backend,
                                    "ea7hg".to_string(),
                                    "EA7HG Visual Rotor",
                                );
                                ui.selectable_value(
                                    &mut self.rotor_backend,
                                    "pstrotator".to_string(),
                                    "PstRotator (XML/UDP)",
                                );
                                ui.selectable_value(
                                    &mut self.rotor_backend,
                                    "mcp2221_yaesu".to_string(),
                                    "Adafruit MCP2221A -> Yaesu G-1000DXC",
                                );
                            });
                        if backend_before != self.rotor_backend {
                            let new_backend = self.rotor_backend.clone();
                            log::info!("Rotor backend switched: {} -> {}", backend_before, new_backend);
                            crate::config::modify_config(|c| {
                                c.rotor_backend = new_backend.clone();
                            });
                        }
                    });
                    match self.rotor_backend.as_str() {
                        "pstrotator" => {
                            ui.horizontal(|ui| {
                                ui.label(rust_i18n::t!("srv_pstrotator_host").to_string());
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.pstrotator_host)
                                        .desired_width(180.0)
                                        .hint_text(rust_i18n::t!("srv_host_example").to_string()),
                                );
                                ui.label(rust_i18n::t!("srv_port").to_string());
                                ui.add(
                                    egui::DragValue::new(&mut self.pstrotator_port)
                                        .range(1u16..=65535)
                                        .speed(1.0),
                                );
                            });
                            ui.horizontal(|ui| {
                                ui.label(rust_i18n::t!("srv_feedback_port").to_string());
                                ui.add(
                                    egui::DragValue::new(&mut self.pstrotator_feedback_port)
                                        .range(1u16..=65535)
                                        .speed(1.0),
                                );
                                ui.checkbox(
                                    &mut self.pstrotator_has_elevation,
                                    rust_i18n::t!("srv_has_elevation").to_string(),
                                );
                            });
                            ui.label(
                                egui::RichText::new(rust_i18n::t!("srv_pstrotator_hint").to_string())
                                .size(10.0)
                                .color(egui::Color32::from_rgb(160, 160, 160)),
                            );
                        }
                        _ => {
                            ui.horizontal(|ui| {
                                ui.label(rust_i18n::t!("srv_ea7hg_addr").to_string());
                                ui.text_edit_singleline(&mut self.rotor_addr);
                            });
                        }
                    }
                    if self.rotor_enabled {
                        ui.checkbox(&mut self.show_rotor_window, rust_i18n::t!("srv_open_at_start").to_string());
                    }

                    // PstRotator listener - parallel input source bovenop
                    // de actieve rotor-backend. Onafhankelijk van de
                    // backend-keuze; werkt bv. om Log4OM -> PstRotator de
                    // Adafruit-rotor te laten besturen.
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.checkbox(
                            &mut self.pstrotator_listen_enabled,
                            rust_i18n::t!("srv_pstrotator_listener").to_string(),
                        )
                        .on_hover_text(rust_i18n::t!("srv_pstrotator_listener_hover").to_string());
                        ui.label(rust_i18n::t!("srv_port").to_string());
                        ui.add(
                            egui::DragValue::new(&mut self.pstrotator_listen_port)
                                .range(1u16..=65535)
                                .speed(1.0),
                        );
                    });
                    if self.pstrotator_listen_enabled && self.rotor_backend == "pstrotator" {
                        ui.label(
                            egui::RichText::new(rust_i18n::t!("srv_pstrotator_loop_warn").to_string())
                            .size(10.0)
                            .color(egui::Color32::from_rgb(220, 160, 40)),
                        );
                    }

                    ui.add_space(16.0);

                    ui.add_space(8.0);
                    ui.heading("Relay");
                    ui.checkbox(&mut self.relay_enabled, rust_i18n::t!("srv_relay_enable").to_string());
                    ui.checkbox(&mut self.relay_udp_enabled, rust_i18n::t!("srv_relay_udp").to_string())
                        .on_hover_text(rust_i18n::t!("srv_relay_udp_hover").to_string());
                    ui.horizontal(|ui| {
                        ui.label(rust_i18n::t!("srv_relay_url").to_string());
                        ui.add(
                            egui::TextEdit::singleline(&mut self.relay_url)
                                .desired_width(260.0)
                                .hint_text("ws://relay.example.com:18080"),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label(rust_i18n::t!("srv_station_name").to_string());
                        ui.add(
                            egui::TextEdit::singleline(&mut self.relay_station)
                                .desired_width(180.0)
                                .hint_text("my-station"),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label(rust_i18n::t!("srv_relay_token").to_string());
                        ui.add(
                            egui::TextEdit::singleline(&mut self.relay_token)
                                .desired_width(180.0)
                                .password(true),
                        );
                    });
                    ui.label(
                        egui::RichText::new(rust_i18n::t!("srv_relay_desc").to_string())
                        .size(10.0)
                        .color(egui::Color32::from_rgb(160, 160, 160)),
                    );

                    ui.add_space(8.0);
                    ui.heading(rust_i18n::t!("srv_security").to_string());
                    ui.horizontal(|ui| {
                        ui.label(rust_i18n::t!("srv_password").to_string());
                        ui.add(egui::TextEdit::singleline(&mut self.password)
                            .desired_width(150.0).password(true)
                            .hint_text(rust_i18n::t!("srv_required_hint").to_string()));
                    });
                    if self.password.is_empty() {
                        ui.colored_label(egui::Color32::RED, rust_i18n::t!("srv_password_required").to_string());
                    } else if let Err(msg) = sdr_remote_core::auth::validate_password_strength(&self.password) {
                        ui.colored_label(egui::Color32::from_rgb(255, 165, 0), msg);
                    }

                    ui.add_space(4.0);
                    ui.checkbox(&mut self.totp_enabled, "2FA (TOTP)");
                    if self.totp_enabled {
                        ui.horizontal(|ui| {
                            ui.label(rust_i18n::t!("srv_secret").to_string());
                            ui.add(egui::TextEdit::singleline(&mut self.totp_secret)
                                .desired_width(220.0).font(egui::TextStyle::Monospace));
                        });
                        if ui.small_button(rust_i18n::t!("srv_generate_secret").to_string()).clicked() {
                            self.totp_secret = sdr_remote_core::auth::generate_totp_secret();
                        }
                        // QR code for authenticator app
                        let uri = sdr_remote_core::auth::totp_uri(&self.totp_secret);
                        if let Ok(qr) = qrcode::QrCode::new(uri.as_bytes()) {
                            let modules: Vec<Vec<bool>> = qr.to_colors().chunks(qr.width()).map(|row| {
                                row.iter().map(|c| *c == qrcode::Color::Dark).collect()
                            }).collect();
                            let size = modules.len();
                            let scale = 3.0_f32;
                            let total = size as f32 * scale;
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(total, total),
                                egui::Sense::hover(),
                            );
                            let painter = ui.painter_at(rect);
                            painter.rect_filled(rect, 0.0, egui::Color32::WHITE);
                            for (y, row) in modules.iter().enumerate() {
                                for (x, &dark) in row.iter().enumerate() {
                                    if dark {
                                        let min = rect.min + egui::vec2(x as f32 * scale, y as f32 * scale);
                                        painter.rect_filled(
                                            egui::Rect::from_min_size(min, egui::vec2(scale, scale)),
                                            0.0,
                                            egui::Color32::BLACK,
                                        );
                                    }
                                }
                            }
                        }
                        ui.label(egui::RichText::new(rust_i18n::t!("srv_scan_qr").to_string()).small().weak());
                    }

                    ui.add_space(8.0);
                    ui.checkbox(&mut self.autostart, rust_i18n::t!("srv_autostart").to_string());

                    ui.add_space(8.0);

                    let pw_valid = !self.password.is_empty()
                        && sdr_remote_core::auth::validate_password_strength(&self.password).is_ok();
                    if ui.add_enabled(pw_valid, egui::Button::new(rust_i18n::t!("srv_save_start").to_string())).clicked() {
                        self.start_server();
                    }
                    }); // <- end ScrollArea wrap voor Mode::Settings
                }
                Mode::Running => {
                    ui.horizontal(|ui| {
                        ui.heading(format!("ThetisLink Server v{}", sdr_remote_core::VERSION));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button(rust_i18n::t!("srv_about").to_string()).clicked() {
                                self.show_about = !self.show_about;
                            }
                        });
                    });
                    // PATCH-2: Status / Logs tabs + "Schik"-knop (vensters ordenen).
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut self.status_view, StatusView::Status, rust_i18n::t!("srv_status").to_string());
                        ui.selectable_value(&mut self.status_view, StatusView::Logs, rust_i18n::t!("srv_logs").to_string());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let mut s = self.show_layout_arranger;
                            if ui.toggle_value(&mut s, rust_i18n::t!("srv_arrange").to_string())
                                .on_hover_text(rust_i18n::t!("srv_arrange_hover").to_string())
                                .changed()
                            {
                                self.show_layout_arranger = s;
                                if s { self.layout_target_monitor = self.detect_monitor_index(ctx); }
                            }
                        });
                    });
                    ui.separator();

                    // Reserveer onderaan ruimte voor de twee gestapelde knoppen
                    // (Exit + Settings) met hun separators, zodat een lange
                    // status/scan-lijst ze niet uit beeld duwt. Voorheen stond
                    // hier een vaste 30px - te weinig voor twee knoppen, waardoor
                    // na een Adafruit-Scan de Settings-knop onbereikbaar werd
                    // (het paneel zelf scrollt niet, alleen de ScrollArea).
                    let btn_h = ui.spacing().interact_size.y;
                    let gap = ui.spacing().item_spacing.y;
                    let bottom_reserve = 2.0 * (btn_h + gap) + 2.0 * (6.0 + gap) + 8.0;
                    let available = (ui.available_height() - bottom_reserve).max(60.0);
                    match self.status_view {
                        StatusView::Status => {
                            egui::ScrollArea::vertical()
                                .max_height(available)
                                .show(ui, |ui| {
                                    if let Some(ref shared) = self.status_panel_state {
                                        status_panel::render_status_panel(
                                            ui,
                                            shared,
                                            &self.status_bind_addr,
                                            self.yaesu.is_some(),
                                            self.amplitec.is_some(),
                                            self.tuner.is_some(),
                                            self.spe.is_some(),
                                            self.rf2k.is_some(),
                                        );
                                    } else {
                                        ui.colored_label(
                                            Color32::from_rgb(160, 160, 160),
                                            "Status panel not ready (server starting...)",
                                        );
                                    }
                                });
                        }
                        StatusView::Logs => {
                            let logs = self.log_buffer.lock().unwrap();
                            egui::ScrollArea::vertical()
                                .stick_to_bottom(true)
                                .max_height(available)
                                .show(ui, |ui| {
                                    for (level, msg) in logs.iter() {
                                        let color = match *level {
                                            Level::Error => egui::Color32::from_rgb(255, 80, 80),
                                            Level::Warn => egui::Color32::from_rgb(255, 170, 40),
                                            _ => ui.visuals().text_color(),
                                        };
                                        let prefix = match *level {
                                            Level::Error => "[ERROR]",
                                            Level::Warn => " [WARN]",
                                            Level::Info => " [INFO]",
                                            Level::Debug => "[DEBUG]",
                                            Level::Trace => "[TRACE]",
                                        };
                                        ui.colored_label(
                                            color,
                                            egui::RichText::new(format!("{} {}", prefix, msg))
                                                .monospace(),
                                        );
                                    }
                                });
                        }
                    }

                    ui.separator();
                    if ui.button(rust_i18n::t!("srv_exit").to_string()).clicked() {
                        // Nette afsluiting (PE1FMC-wens): sein de server-shutdown zodat de
                        // Yaesu SSB-routing wordt hersteld (connect-set-modus; in per-PTT-modus
                        // staat de radio al normaal), drop de radio-Arc zodat de cmd-channel
                        // dichtgaat -> restore in de serial-thread, geef die ~500 ms, sluit af.
                        // Zo blijft de 991 niet in USB-stand achter na afsluiten.
                        self.save_window_positions();
                        if let Some(tx) = self.shutdown_tx.take() {
                            let _ = tx.send(true);
                        }
                        self.yaesu = None;
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        std::process::exit(0);
                    }
                    ui.separator();
                    if ui.button(rust_i18n::t!("srv_settings").to_string()).clicked() {
                        // Stop server (thread finishes in background)
                        if let Some(tx) = self.shutdown_tx.take() {
                            let _ = tx.send(true);
                        }
                        self.yaesu = None;
                        self.amplitec = None;
                        self.tuner = None;
                        self.spe = None;
                        self.rf2k = None;
                        self.ultrabeam = None;
                        self.rotor = None;
                        self.status_panel_state = None;
                        // Reset per-popout init_applied flags so the saved
                        // position+size get re-applied on the next Save &
                        // Start. Without this the viewport closes silently
                        // (no close_requested event when we leave Running
                        // mode) and the next reopen sees init_applied=true,
                        // skipping with_position/with_inner_size, leaving
                        // every popout at the OS default clump.
                        self.tuner_window_init_applied = false;
                        self.amplitec_window_init_applied = false;
                        self.spe_window_init_applied = false;
                        self.rf2k_window_init_applied = false;
                        self.ultrabeam_window_init_applied = false;
                        self.rotor_window_init_applied = false;
                        self.mode = Mode::Settings;
                    }

                    ctx.request_repaint_after(Duration::from_millis(200));
                }
            }
        });

        // Tuner secondary window
        if matches!(self.mode, Mode::Running) && self.show_tuner_window {
            if let Some(ref tuner_ref) = self.tuner {
                let status = tuner_ref.status();

                // Change detection -> log
                if status.state != self.last_tuner_state {
                    let ts = chrono::Local::now().format("%H:%M:%S").to_string();
                    let msg = match status.state {
                        crate::tuner::TUNER_TUNING => "Tune gestart".to_string(),
                        crate::tuner::TUNER_DONE_OK => "Tune compleet".to_string(),
                        crate::tuner::TUNER_TIMEOUT => "Tune timeout (30s)".to_string(),
                        crate::tuner::TUNER_ABORTED => "Tune afgebroken".to_string(),
                        crate::tuner::TUNER_IDLE if self.last_tuner_state != 0 => "Status reset naar Idle".to_string(),
                        _ => String::new(),
                    };
                    if !msg.is_empty() {
                        self.tuner_log.push_back((ts, msg));
                        if self.tuner_log.len() > 50 { self.tuner_log.pop_front(); }
                    }
                    self.last_tuner_state = status.state;
                }

                let log_entries: Vec<_> = self.tuner_log.iter().cloned().collect();
                let tuner_for_window = tuner_ref.clone();
                let macro_status = self.macro_runner.status();

                let tuner_default_h = if self.show_tuner_log { 380.0 } else { 180.0 };
                let tuner_sz = self.tuner_window_size.unwrap_or([660.0, tuner_default_h]);
                // Popout title follows the primary tuner's label (e.g.
                // "Tuner1 (JC-4s loop)") so the window doesn't lie about the
                // model when slot 0 is a JC-3s, and matches what the status
                // panel shows. Falls back to a generic label when no live
                // tuner is bound.
                let tuner_title = tuner_for_window.label().to_string();
                let mut tuner_vb = ViewportBuilder::default()
                    .with_title(if tuner_title.is_empty() {
                        "StockCorner Tuner".to_string()
                    } else {
                        tuner_title
                    });
                if !self.tuner_window_init_applied {
                    tuner_vb = tuner_vb.with_inner_size(tuner_sz);
                    if let Some(pos) = self.tuner_window_pos {
                        tuner_vb = tuner_vb.with_position(egui::pos2(pos[0], pos[1]));
                    }
                    self.tuner_window_init_applied = true;
                }
                let mut tuner_closed = false;
                ctx.show_viewport_immediate(
                    ViewportId::from_hash_of("tuner_control"),
                    tuner_vb,
                    |ctx, _class| {
                        // Track window position and size
                        if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
                            self.tuner_window_pos = Some([rect.left(), rect.top()]);
                        }
                        if let Some(rect) = ctx.input(|i| i.viewport().inner_rect) {
                            self.tuner_window_size = Some([rect.width(), rect.height()]);
                        }
                        if ctx.input(|i| i.viewport().close_requested()) {
                            self.show_tuner_window = false;
                            self.tuner_window_init_applied = false;
                            tuner_closed = true;
                            return;
                        }
                        egui::CentralPanel::default().show(ctx, |ui| {
                            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                            render_tuner_panel(ui, &tuner_for_window, &status, &mut self.show_tuner_log);

                            ui.add_space(4.0);
                            ui.separator();

                            // Macro button grid
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(rust_i18n::t!("srv_macros_heading").to_string()).strong());
                                if macro_status.running {
                                    ui.colored_label(
                                        Color32::from_rgb(255, 170, 40),
                                        format!("> {} ({}/{})",
                                            macro_status.current_label,
                                            macro_status.step,
                                            macro_status.total_steps),
                                    );
                                    if ui.button(rust_i18n::t!("srv_abort_macro").to_string()).clicked() {
                                        self.macro_runner.abort();
                                    }
                                }
                            });
                            ui.add_space(2.0);

                            // Row 1: F1-F12
                            ui.horizontal(|ui| {
                                for i in 0..12 {
                                    render_macro_button(
                                        ui, i, &self.macro_slots, &macro_status,
                                        &self.macro_runner, &self.macro_cat_tx,
                                        &self.tuner,
                                    );
                                }
                            });
                            // Row 2: ^F1-^F12
                            ui.horizontal(|ui| {
                                for i in 12..24 {
                                    render_macro_button(
                                        ui, i, &self.macro_slots, &macro_status,
                                        &self.macro_runner, &self.macro_cat_tx,
                                        &self.tuner,
                                    );
                                }
                            });

                            ui.add_space(4.0);
                            if ui.button(rust_i18n::t!("srv_edit_macros").to_string()).clicked() {
                                self.show_macro_editor = true;
                                // Load current slot into editor
                                load_slot_into_editor(
                                    &self.macro_slots, self.editor_slot,
                                    &mut self.editor_label, &mut self.editor_actions,
                                );
                            }

                            ui.add_space(4.0);
                            render_tuner_log(ui, &log_entries, self.show_tuner_log);
                            }); // <- ScrollArea wrap voor Tuner content
                        });
                        ctx.request_repaint_after(Duration::from_millis(200));
                    },
                );
                if tuner_closed {
                    self.save_window_positions();
                }

            }
        }

        // Macro editor window
        if self.show_macro_editor {
            ctx.show_viewport_immediate(
                ViewportId::from_hash_of("macro_editor"),
                ViewportBuilder::default()
                    .with_title("Macro Editor")
                    .with_inner_size([550.0, 500.0]),
                |ctx, _class| {
                    if ctx.input(|i| i.viewport().close_requested()) {
                        self.show_macro_editor = false;
                        return;
                    }
                    egui::CentralPanel::default().show(ctx, |ui| {
                        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                            render_macro_editor(
                                ui,
                                &mut self.macro_slots,
                                &mut self.editor_slot,
                                &mut self.editor_label,
                                &mut self.editor_actions,
                                &mut self.show_macro_editor,
                            );
                        });
                    });
                },
            );
        }

        // Amplitec secondary window
        if matches!(self.mode, Mode::Running) && self.show_amplitec_window {
            if let Some(ref amplitec) = self.amplitec {
                let status = amplitec.status();

                // Change detection -> log
                if status.switch_a != self.last_switch_a && status.switch_a > 0 {
                    let label = self.amplitec_labels[(status.switch_a - 1).min(5) as usize].clone();
                    let ts = chrono::Local::now().format("%H:%M:%S").to_string();
                    self.amplitec_log.push_back((ts, format!("Poort A -> {} ({})", status.switch_a, label)));
                    if self.amplitec_log.len() > 100 { self.amplitec_log.pop_front(); }
                    self.last_switch_a = status.switch_a;
                }
                if status.switch_b != self.last_switch_b && status.switch_b > 0 {
                    let label = self.amplitec_labels[(status.switch_b - 1).min(5) as usize].clone();
                    let ts = chrono::Local::now().format("%H:%M:%S").to_string();
                    self.amplitec_log.push_back((ts, format!("Poort B -> {} ({})", status.switch_b, label)));
                    if self.amplitec_log.len() > 100 { self.amplitec_log.pop_front(); }
                    self.last_switch_b = status.switch_b;
                }

                let labels = self.amplitec_labels.clone();
                let log_entries: Vec<_> = self.amplitec_log.iter().cloned().collect();
                let amplitec_for_window = amplitec.clone();

                let amp_default_h = if self.show_amplitec_log { 330.0 } else { 175.0 };
                let amp_sz = self.amplitec_window_size.unwrap_or([420.0, amp_default_h]);
                let mut amp_vb = ViewportBuilder::default()
                    .with_title("Amplitec 6/2 Antenna Switch");
                if !self.amplitec_window_init_applied {
                    amp_vb = amp_vb.with_inner_size(amp_sz);
                    if let Some(pos) = self.amplitec_window_pos {
                        amp_vb = amp_vb.with_position(egui::pos2(pos[0], pos[1]));
                    }
                    self.amplitec_window_init_applied = true;
                }
                let mut amplitec_closed = false;
                ctx.show_viewport_immediate(
                    ViewportId::from_hash_of("amplitec_control"),
                    amp_vb,
                    |ctx, _class| {
                        if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
                            self.amplitec_window_pos = Some([rect.left(), rect.top()]);
                        }
                        if let Some(rect) = ctx.input(|i| i.viewport().inner_rect) {
                            self.amplitec_window_size = Some([rect.width(), rect.height()]);
                        }
                        if ctx.input(|i| i.viewport().close_requested()) {
                            self.show_amplitec_window = false;
                            self.amplitec_window_init_applied = false;
                            amplitec_closed = true;
                            return;
                        }
                        egui::CentralPanel::default().show(ctx, |ui| {
                            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                                render_amplitec_panel(
                                    ui, &amplitec_for_window, &status,
                                    &labels, &log_entries, &mut self.show_amplitec_log,
                                );
                            });
                        });
                        ctx.request_repaint_after(Duration::from_millis(500));
                    },
                );
                if amplitec_closed {
                    self.save_window_positions();
                }
            }
        }

        // SPE Expert secondary window
        if matches!(self.mode, Mode::Running) && self.show_spe_window {
            if let Some(ref spe_ref) = self.spe {
                let status = spe_ref.status();

                // Change detection -> log
                if status.state != self.last_spe_state {
                    let ts = chrono::Local::now().format("%H:%M:%S").to_string();
                    let msg = match status.state {
                        0 => "Status -> Off".to_string(),
                        1 => "Status -> Standby".to_string(),
                        2 => "Status -> Operate".to_string(),
                        _ => format!("Status -> Unknown ({})", status.state),
                    };
                    self.spe_log.push_back((ts, msg));
                    if self.spe_log.len() > 100 { self.spe_log.pop_front(); }
                    self.last_spe_state = status.state;
                }
                // Warning/alarm change detection
                if status.warning != self.last_spe_warning {
                    if status.warning != b'N' && status.warning != 0 {
                        let ts = chrono::Local::now().format("%H:%M:%S").to_string();
                        self.spe_log.push_back((ts, format!("Warning: {}", status.warning as char)));
                        if self.spe_log.len() > 100 { self.spe_log.pop_front(); }
                    }
                    self.last_spe_warning = status.warning;
                }
                if status.alarm != self.last_spe_alarm {
                    if status.alarm != b'N' && status.alarm != 0 {
                        let ts = chrono::Local::now().format("%H:%M:%S").to_string();
                        self.spe_log.push_back((ts, format!("ALARM: {}", status.alarm as char)));
                        if self.spe_log.len() > 100 { self.spe_log.pop_front(); }
                    }
                    self.last_spe_alarm = status.alarm;
                }

                let log_entries: Vec<_> = self.spe_log.iter().cloned().collect();
                let spe_for_window = spe_ref.clone();

                let spe_default_h = if self.show_spe_log { 320.0 } else { 200.0 };
                let spe_sz = self.spe_window_size.unwrap_or([460.0, spe_default_h]);
                let mut spe_vb = ViewportBuilder::default()
                    .with_title("SPE Expert 1.3K-FA")
                    .with_resizable(true);
                if !self.spe_window_init_applied {
                    spe_vb = spe_vb.with_inner_size(spe_sz);
                    if let Some(pos) = self.spe_window_pos {
                        spe_vb = spe_vb.with_position(egui::pos2(pos[0], pos[1]));
                    }
                    self.spe_window_init_applied = true;
                }
                let mut spe_closed = false;
                ctx.show_viewport_immediate(
                    ViewportId::from_hash_of("spe_expert_control"),
                    spe_vb,
                    |ctx, _class| {
                        if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
                            self.spe_window_pos = Some([rect.left(), rect.top()]);
                        }
                        if let Some(rect) = ctx.input(|i| i.viewport().inner_rect) {
                            self.spe_window_size = Some([rect.width(), rect.height()]);
                        }
                        if ctx.input(|i| i.viewport().close_requested()) {
                            self.show_spe_window = false;
                            self.spe_window_init_applied = false;
                            spe_closed = true;
                            return;
                        }
                        let drive_pct = self.drive_level.load(Ordering::Relaxed);
                        egui::CentralPanel::default().show(ctx, |ui| {
                            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                                render_spe_panel(ui, &spe_for_window, &status, &log_entries,
                                    &mut self.show_spe_log, &mut self.spe_peak_power, &mut self.spe_peak_time, drive_pct,
                                    &self.active_pa);
                            });
                        });
                        ctx.request_repaint_after(Duration::from_millis(100));
                    },
                );
                if spe_closed {
                    self.save_window_positions();
                }
            }
        }

        // RF2K-S secondary window
        if matches!(self.mode, Mode::Running) && self.show_rf2k_window {
            if let Some(ref rf2k_ref) = self.rf2k {
                let status = rf2k_ref.status();
                let rf2k_for_window = rf2k_ref.clone();

                let rf2k_sz = self.rf2k_window_size.unwrap_or([480.0, 520.0]);
                let mut rf2k_vb = ViewportBuilder::default()
                    .with_title("RF2K-S Power Amplifier")
                    .with_resizable(true);
                if !self.rf2k_window_init_applied {
                    rf2k_vb = rf2k_vb.with_inner_size(rf2k_sz);
                    if let Some(pos) = self.rf2k_window_pos {
                        rf2k_vb = rf2k_vb.with_position(egui::pos2(pos[0], pos[1]));
                    }
                    self.rf2k_window_init_applied = true;
                }
                let mut rf2k_closed = false;
                ctx.show_viewport_immediate(
                    ViewportId::from_hash_of("rf2k_control"),
                    rf2k_vb,
                    |ctx, _class| {
                        if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
                            self.rf2k_window_pos = Some([rect.left(), rect.top()]);
                        }
                        if let Some(rect) = ctx.input(|i| i.viewport().inner_rect) {
                            self.rf2k_window_size = Some([rect.width(), rect.height()]);
                        }
                        if ctx.input(|i| i.viewport().close_requested()) {
                            self.show_rf2k_window = false;
                            self.rf2k_window_init_applied = false;
                            rf2k_closed = true;
                            return;
                        }
                        egui::CentralPanel::default().show(ctx, |ui| {
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                render_rf2k_panel(ui, &rf2k_for_window, &status,
                                    &mut self.rf2k_peak_power, &mut self.rf2k_peak_time,
                                    &self.active_pa, &mut self.rf2k_confirm_fw_close);
                                render_rf2k_debug_section(ui, &rf2k_for_window, &status,
                                    &mut self.rf2k_show_debug,
                                    &mut self.rf2k_confirm_high_power,
                                    &mut self.rf2k_confirm_zero_fram);
                                render_rf2k_drive_config_section(ui, &rf2k_for_window, &status,
                                    &mut self.rf2k_show_drive_config,
                                    &mut self.rf2k_drive_edit,
                                    &mut self.rf2k_drive_loaded);
                                render_rf2k_footer(ui, &status);
                            });
                        });
                        ctx.request_repaint_after(Duration::from_millis(200));
                    },
                );
                if rf2k_closed {
                    self.save_window_positions();
                }
            }
        }

        // UltraBeam RCU-06 secondary window
        if matches!(self.mode, Mode::Running) && self.show_ultrabeam_window {
            if let Some(ref ub_ref) = self.ultrabeam {
                let status = ub_ref.status();
                let ub_for_window = ub_ref.clone();

                let ub_default_h = if self.ultrabeam_show_menu { 620.0 } else { 400.0 };
                let ub_sz = self.ultrabeam_window_size.unwrap_or([440.0, ub_default_h]);
                let mut ub_vb = ViewportBuilder::default()
                    .with_title("UltraBeam RCU-06")
                    .with_resizable(true);
                if !self.ultrabeam_window_init_applied {
                    ub_vb = ub_vb.with_inner_size(ub_sz);
                    if let Some(pos) = self.ultrabeam_window_pos {
                        ub_vb = ub_vb.with_position(egui::pos2(pos[0], pos[1]));
                    }
                    self.ultrabeam_window_init_applied = true;
                }
                let mut ub_closed = false;
                ctx.show_viewport_immediate(
                    ViewportId::from_hash_of("ultrabeam_control"),
                    ub_vb,
                    |ctx, _class| {
                        if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
                            self.ultrabeam_window_pos = Some([rect.left(), rect.top()]);
                        }
                        if let Some(rect) = ctx.input(|i| i.viewport().inner_rect) {
                            self.ultrabeam_window_size = Some([rect.width(), rect.height()]);
                        }
                        if ctx.input(|i| i.viewport().close_requested()) {
                            self.show_ultrabeam_window = false;
                            self.ultrabeam_window_init_applied = false;
                            ub_closed = true;
                            return;
                        }
                        egui::CentralPanel::default().show(ctx, |ui| {
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                let amp_status = self.amplitec.as_ref().map(|a| a.status());
                                let prev_show_menu = self.ultrabeam_show_menu;
                                render_ultrabeam_panel(ui, &ub_for_window, &status,
                                    &mut self.ultrabeam_show_menu,
                                    &mut self.ultrabeam_confirm_retract,
                                    &mut self.ultrabeam_confirm_calibrate,
                                    &mut self.ultrabeam_auto_track,
                                    &mut self.ultrabeam_last_auto_khz,
                                    &self.vfo_freq_shared,
                                    &self.vfo_b_freq_shared,
                                    &amp_status,
                                    &self.amplitec_labels);
                                if self.ultrabeam_show_menu != prev_show_menu {
                                    crate::config::save_ultrabeam_show_menu(self.ultrabeam_show_menu);
                                }
                            });
                        });
                        ctx.request_repaint_after(Duration::from_millis(200));
                    },
                );
                if ub_closed {
                    self.save_window_positions();
                }
            }
        }

        // Rotor secondary window. Titel volgt de actieve backend zodat
        // de operator direct ziet welke driver onder water werkt.
        if matches!(self.mode, Mode::Running) && self.show_rotor_window {
            if let Some(ref rotor_ref) = self.rotor {
                let status = rotor_ref.status();
                let rotor_for_window = rotor_ref.clone();

                let rotor_sz = self.rotor_window_size.unwrap_or([340.0, 320.0]);
                let backend_title = match self.rotor_backend.as_str() {
                    "pstrotator" => "Rotor - PstRotator",
                    "mcp2221_yaesu" => "Rotor - Adafruit MCP2221A -> Yaesu G-1000DXC",
                    _ => "Rotor - EA7HG Visual Rotor",
                };
                let mut rotor_vb = ViewportBuilder::default()
                    .with_title(backend_title)
                    .with_resizable(true);
                if !self.rotor_window_init_applied {
                    rotor_vb = rotor_vb.with_inner_size(rotor_sz);
                    if let Some(pos) = self.rotor_window_pos {
                        rotor_vb = rotor_vb.with_position(egui::pos2(pos[0], pos[1]));
                    }
                    self.rotor_window_init_applied = true;
                }
                let mut rotor_closed = false;
                ctx.show_viewport_immediate(
                    ViewportId::from_hash_of("rotor_control"),
                    rotor_vb,
                    |ctx, _class| {
                        if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
                            self.rotor_window_pos = Some([rect.left(), rect.top()]);
                        }
                        if let Some(rect) = ctx.input(|i| i.viewport().inner_rect) {
                            self.rotor_window_size = Some([rect.width(), rect.height()]);
                        }
                        if ctx.input(|i| i.viewport().close_requested()) {
                            self.show_rotor_window = false;
                            self.rotor_window_init_applied = false;
                            rotor_closed = true;
                            return;
                        }
                        egui::CentralPanel::default().show(ctx, |ui| {
                            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                                render_rotor_panel(ui, &rotor_for_window, &status, &mut self.rotor_goto_input);
                            });
                        });
                        ctx.request_repaint_after(Duration::from_millis(200));
                    },
                );
                if rotor_closed {
                    self.save_window_positions();
                }
            }
        }

        // About window
        if self.show_about {
            egui::Window::new("About ThetisLink")
                .collapsible(false)
                .resizable(true)
                .default_size([400.0, 480.0])
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new("ThetisLink Server").size(20.0).strong());
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
                        egui::Grid::new("hw_grid_srv").num_columns(2).spacing([12.0, 2.0]).show(ui, |ui| {
                            for (dev, iface) in [
                                ("ANAN 7000DLE", "TCI (via Thetis)"),
                                ("Yaesu FT-991A", "Serial CAT + USB Audio"),
                                ("Yaesu FTX-1", "Serial CAT + USB Audio"),
                                ("RF2K-S PA", "HTTP API"),
                                ("SPE Expert 1.3K-FA", "Serial"),
                                ("StockCorner JC-4s / JC-3s Tuner (×2)", "MCP2221A USB-HID"),
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

        // "Vensters schikken"-matrix. Alleen in Running mode zinvol (de popouts
        // bestaan alleen dan); bij terugkeer naar Settings sluit het venster.
        if matches!(self.mode, Mode::Running) {
            self.render_layout_arranger(ctx);
        } else {
            self.show_layout_arranger = false;
        }
    }
}

/// Spawn een verse copy van de huidige executable met dezelfde CLI-args
/// en `process::exit(0)` daarna, zodat de nieuwe process de UDP socket
/// en alle hardware-handles kan binden. Aangeroepen door de
/// auto-restart-flow in `update()` *nadat* alle Drop-handlers gelopen
/// zijn en de cpal/USB handles vrijgegeven zijn.
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
        // DETACHED_PROCESS (0x00000008) - de nieuwe process krijgt een
        // eigen console-handle-group, los van ons. CREATE_NEW_PROCESS_GROUP
        // (0x00000200) isoleert Ctrl-C-bezorging. Samen zorgen ze dat de
        // child volledig zelfstandig is zodat dit proces meteen kan exit.
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
