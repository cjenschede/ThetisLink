// SPDX-License-Identifier: GPL-2.0-or-later

mod utils;
mod rotor;
mod amplitec;
mod tuner;
mod macros_ui;
mod spe;
mod rf2k;
mod ultrabeam;

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

pub struct ServerApp {
    cat_addr: String,
    tci_addr: String,
    thetis_path: String,
    yaesu_port: String,
    yaesu_audio_device: String,
    yaesu_enabled: bool,
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
    tuner_port: String,
    tuner_enabled: bool,
    tuner_assume_tuned: bool,
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
    // DX Cluster
    dxcluster_server: String,
    dxcluster_callsign: String,
    dxcluster_enabled: bool,
    dxcluster_expiry_min: u16,
    // Authentication
    password: String,
    totp_enabled: bool,
    totp_secret: String,
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
            cat_addr: config.cat_addr,
            tci_addr: config.tci_addr.unwrap_or_default(),
            thetis_path: config.thetis_path.unwrap_or_default(),
            yaesu_port: config.yaesu_port.unwrap_or_default(),
            yaesu_audio_device: config.yaesu_audio_device.unwrap_or_default(),
            yaesu_enabled: config.yaesu_enabled,
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
            tuner_port: config.tuner_port.unwrap_or_default(),
            tuner_enabled: config.tuner_enabled,
            tuner_assume_tuned: config.tuner_assume_tuned,
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
            ultrabeam_window_pos: config.ultrabeam_window_pos,
            ultrabeam_show_menu: false,
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
            dxcluster_server: config.dxcluster_server.clone(),
            dxcluster_callsign: config.dxcluster_callsign.clone(),
            dxcluster_enabled: config.dxcluster_enabled,
            dxcluster_expiry_min: config.dxcluster_expiry_min,
            password: config.password.clone().unwrap_or_default(),
            totp_enabled: config.totp_enabled,
            totp_secret: config.totp_secret.clone().unwrap_or_else(|| sdr_remote_core::auth::generate_totp_secret()),
            main_window_pos: config.main_window_pos,
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
        }
    }

    fn start_server(&mut self) {
        // Clear log buffer for fresh start
        self.log_buffer.lock().unwrap().clear();

        let thetis = self.thetis_path.trim().to_string();
        let yaesu_port_str = self.yaesu_port.trim().to_string();
        let amp_port = self.amplitec_port.trim().to_string();
        let tun_port = self.tuner_port.trim().to_string();
        let spe_port_str = self.spe_port.trim().to_string();
        let rf2k_addr_str = self.rf2k_addr.trim().to_string();
        let ub_port = self.ultrabeam_port.trim().to_string();
        let rotor_addr_str = self.rotor_addr.trim().to_string();
        let config = ServerConfig {
            cat_addr: self.cat_addr.clone(),
            spectrum_enabled: true,
            thetis_path: if thetis.is_empty() { None } else { Some(thetis) },
            yaesu_port: if yaesu_port_str.is_empty() { None } else { Some(yaesu_port_str.clone()) },
            yaesu_enabled: self.yaesu_enabled,
            yaesu_baud: 38400,
            yaesu_audio_device: if self.yaesu_audio_device.is_empty() { None } else { Some(self.yaesu_audio_device.clone()) },
            amplitec_port: if amp_port.is_empty() { None } else { Some(amp_port.clone()) },
            amplitec_enabled: self.amplitec_enabled,
            amplitec_labels: self.amplitec_labels.clone(),
            show_amplitec_window: self.show_amplitec_window,
            tuner_port: if tun_port.is_empty() { None } else { Some(tun_port.clone()) },
            tuner_enabled: self.tuner_enabled,
            tuner_assume_tuned: self.tuner_assume_tuned,
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
            autostart: self.autostart,
            active_pa: self.active_pa.load(Ordering::Relaxed),
            tci_addr: if self.tci_addr.trim().is_empty() { None } else { Some(self.tci_addr.trim().to_string()) },
            dxcluster_server: self.dxcluster_server.clone(),
            dxcluster_callsign: self.dxcluster_callsign.clone(),
            dxcluster_enabled: self.dxcluster_enabled,
            dxcluster_expiry_min: self.dxcluster_expiry_min,
            password: if self.password.is_empty() { None } else { Some(self.password.clone()) },
            totp_secret: if self.totp_enabled { Some(self.totp_secret.clone()) } else { None },
            totp_enabled: self.totp_enabled,
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
            match with_timeout(com_timeout, move || crate::yaesu::YaesuRadio::new(&port, baud, audio_dev_opt.as_deref())) {
                Ok(radio) => {
                    log::info!("Yaesu FT-991A connected on {}", port_log);
                    self.yaesu = Some(Arc::new(radio));
                }
                Err(e) => {
                    log::warn!("Yaesu init failed: {}", e);
                }
            }
        }

        // Create AmplitecSwitch early so UI can access it too
        let amplitec = if !amp_port.is_empty() && self.amplitec_enabled {
            let port = amp_port.clone();
            match with_timeout(com_timeout, move || AmplitecSwitch::new(&port)) {
                Ok(sw) => {
                    log::info!("Amplitec 6/2 connected on {}", amp_port);
                    Some(Arc::new(sw))
                }
                Err(e) => {
                    log::warn!("Amplitec init failed: {}", e);
                    None
                }
            }
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

        // Create JC-4s tuner (after SPE + RF2K, so it gets PA references for safe tune)
        let tuner_arc = if !tun_port.is_empty() && self.tuner_enabled {
            let port = tun_port.clone();
            let spe_ref = spe_arc.clone();
            let rf2k_ref = rf2k_arc.clone();
            let assume_tuned = self.tuner_assume_tuned;
            match with_timeout(com_timeout, move || Jc4sTuner::new(&port, cat_tx, spe_ref, rf2k_ref, assume_tuned)) {
                Ok(t) => {
                    log::info!("JC-4s tuner connected on {}", tun_port);
                    let arc_t = Arc::new(t);
                    self.tuner = Some(arc_t.clone());
                    Some(arc_t)
                }
                Err(e) => {
                    log::warn!("JC-4s tuner init failed: {}", e);
                    None
                }
            }
        } else {
            None
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

        // Create Rotor if configured
        if !rotor_addr_str.is_empty() && self.rotor_enabled {
            log::info!("Rotor connecting to {}", rotor_addr_str);
            self.rotor = Some(Arc::new(crate::rotor::Rotor::new(&rotor_addr_str)));
        }

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
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
            rt.block_on(async {
                if let Err(e) = crate::run_server_async(config, shutdown_rx, amplitec, tuner_arc, spe_arc, rf2k_arc, ultrabeam_for_net, rotor_for_net, Some(cat_rx), Some(drive_level_shared), Some(active_pa_shared), Some(vfo_freq_shared), Some(vfo_b_freq_shared), yaesu_for_net).await {
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
        config.active_pa = self.active_pa.load(Ordering::Relaxed);
        crate::config::save(&config);
    }
}

impl eframe::App for ServerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Light grey background, lighter widget fills for contrast
        let mut visuals = ctx.style().visuals.clone();
        let light_grey = egui::Color32::from_rgb(230, 230, 230);
        visuals.panel_fill = light_grey;
        visuals.window_fill = light_grey;
        visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(210, 210, 215);
        visuals.widgets.inactive.weak_bg_fill = egui::Color32::from_rgb(210, 210, 215);
        visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(195, 195, 200);
        visuals.widgets.hovered.weak_bg_fill = egui::Color32::from_rgb(195, 195, 200);
        visuals.widgets.active.bg_fill = egui::Color32::from_rgb(180, 180, 190);
        visuals.widgets.active.weak_bg_fill = egui::Color32::from_rgb(180, 180, 190);
        ctx.set_visuals(visuals);

        // Auto-start on first frame if configured
        if self.pending_autostart {
            self.pending_autostart = false;
            self.start_server();
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
                    ui.heading(format!("ThetisLink Server v{}", sdr_remote_core::VERSION));
                    ui.add_space(10.0);

                    ui.label("Thetis CAT adres:");
                    ui.text_edit_singleline(&mut self.cat_addr);

                    ui.add_space(8.0);

                    ui.label("Thetis TCI adres (bijv. 127.0.0.1:40001):");
                    ui.text_edit_singleline(&mut self.tci_addr);

                    ui.add_space(8.0);

                    ui.label("Thetis.exe pad (optioneel, voor auto-start):");
                    ui.text_edit_singleline(&mut self.thetis_path);

                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.yaesu_enabled, "Yaesu FT-991A");
                        ui.label("CAT:");
                        egui::ComboBox::from_id_salt("yaesu_port")
                            .selected_text(if self.yaesu_port.is_empty() { "(Geen)" } else { &self.yaesu_port })
                            .width(120.0)
                            .show_ui(ui, |ui| {
                                if ui.selectable_label(self.yaesu_port.is_empty(), "(Geen)").clicked() {
                                    self.yaesu_port.clear();
                                }
                                for port in &self.serial_ports {
                                    if ui.selectable_label(*port == self.yaesu_port, port).clicked() {
                                        self.yaesu_port = port.clone();
                                    }
                                }
                            });
                        ui.label("Audio:");
                        egui::ComboBox::from_id_salt("yaesu_audio")
                            .selected_text(if self.yaesu_audio_device.is_empty() { "(Geen)" } else { &self.yaesu_audio_device })
                            .width(200.0)
                            .show_ui(ui, |ui| {
                                if ui.selectable_label(self.yaesu_audio_device.is_empty(), "(Geen)").clicked() {
                                    self.yaesu_audio_device.clear();
                                }
                                for name in crate::yaesu::available_audio_inputs() {
                                    if ui.selectable_label(name == self.yaesu_audio_device, &name).clicked() {
                                        self.yaesu_audio_device = name;
                                    }
                                }
                            });
                    });

                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.amplitec_enabled, "Amplitec 6/2");
                        egui::ComboBox::from_id_salt("amplitec_port")
                            .selected_text(if self.amplitec_port.is_empty() { "(Geen)" } else { &self.amplitec_port })
                            .width(200.0)
                            .show_ui(ui, |ui| {
                                if ui.selectable_label(self.amplitec_port.is_empty(), "(Geen)").clicked() {
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
                        ui.checkbox(&mut self.show_amplitec_window, "Amplitec venster openen bij starten");
                    }

                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.tuner_enabled, "JC-4s Tuner");
                        egui::ComboBox::from_id_salt("tuner_port")
                            .selected_text(if self.tuner_port.is_empty() { "(Geen)" } else { &self.tuner_port })
                            .width(200.0)
                            .show_ui(ui, |ui| {
                                if ui.selectable_label(self.tuner_port.is_empty(), "(Geen)").clicked() {
                                    self.tuner_port.clear();
                                }
                                for port in &self.serial_ports {
                                    if ui.selectable_label(*port == self.tuner_port, port).clicked() {
                                        self.tuner_port = port.clone();
                                    }
                                }
                            });
                    });

                    if !self.tuner_port.is_empty() {
                        ui.checkbox(&mut self.tuner_assume_tuned, "CTS-puls compensatie (herstart vereist)");
                        ui.checkbox(&mut self.show_tuner_window, "Tuner venster openen bij starten");
                    }

                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.spe_enabled, "SPE Expert");
                        egui::ComboBox::from_id_salt("spe_port")
                            .selected_text(if self.spe_port.is_empty() { "(Geen)" } else { &self.spe_port })
                            .width(200.0)
                            .show_ui(ui, |ui| {
                                if ui.selectable_label(self.spe_port.is_empty(), "(Geen)").clicked() {
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
                        ui.checkbox(&mut self.show_spe_window, "SPE Expert venster openen bij starten");
                    }

                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.rf2k_enabled, "RF2K-S");
                        ui.label("adres:");
                        ui.text_edit_singleline(&mut self.rf2k_addr);
                    });
                    if !self.rf2k_addr.is_empty() {
                        ui.checkbox(&mut self.show_rf2k_window, "RF2K-S venster openen bij starten");
                    }

                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.ultrabeam_enabled, "UltraBeam RCU-06");
                        egui::ComboBox::from_id_salt("ultrabeam_port")
                            .selected_text(if self.ultrabeam_port.is_empty() { "(Geen)" } else { &self.ultrabeam_port })
                            .width(200.0)
                            .show_ui(ui, |ui| {
                                if ui.selectable_label(self.ultrabeam_port.is_empty(), "(Geen)").clicked() {
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
                        ui.checkbox(&mut self.show_ultrabeam_window, "UltraBeam venster openen bij starten");
                    }

                    ui.add_space(8.0);

                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.rotor_enabled, "EA7HG Visual Rotor");
                        ui.label("adres:");
                        ui.text_edit_singleline(&mut self.rotor_addr);
                    });
                    if !self.rotor_addr.trim().is_empty() {
                        ui.checkbox(&mut self.show_rotor_window, "Rotor venster openen bij starten");
                    }

                    ui.add_space(16.0);

                    ui.add_space(8.0);
                    ui.heading("Security");
                    ui.horizontal(|ui| {
                        ui.label("Password:");
                        ui.add(egui::TextEdit::singleline(&mut self.password)
                            .desired_width(150.0).password(true)
                            .hint_text("(required)"));
                    });
                    if self.password.is_empty() {
                        ui.colored_label(egui::Color32::RED, "Password is required");
                    } else if let Err(msg) = sdr_remote_core::auth::validate_password_strength(&self.password) {
                        ui.colored_label(egui::Color32::from_rgb(255, 165, 0), msg);
                    }

                    ui.add_space(4.0);
                    ui.checkbox(&mut self.totp_enabled, "2FA (TOTP)");
                    if self.totp_enabled {
                        ui.horizontal(|ui| {
                            ui.label("Secret:");
                            ui.add(egui::TextEdit::singleline(&mut self.totp_secret)
                                .desired_width(220.0).font(egui::TextStyle::Monospace));
                        });
                        if ui.small_button("Generate new secret").clicked() {
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
                        ui.label(egui::RichText::new("Scan with Google Authenticator or similar app").small().weak());
                    }

                    ui.add_space(8.0);
                    ui.checkbox(&mut self.autostart, "Auto-start on launch");

                    ui.add_space(8.0);

                    let pw_valid = !self.password.is_empty()
                        && sdr_remote_core::auth::validate_password_strength(&self.password).is_ok();
                    if ui.add_enabled(pw_valid, egui::Button::new("Save & Start")).clicked() {
                        self.start_server();
                    }
                }
                Mode::Running => {
                    ui.horizontal(|ui| {
                        ui.heading(format!("ThetisLink Server v{}", sdr_remote_core::VERSION));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("About").clicked() {
                                self.show_about = !self.show_about;
                            }
                        });
                    });
                    ui.separator();

                    let logs = self.log_buffer.lock().unwrap();
                    let available = ui.available_height() - 30.0;
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

                    ui.separator();
                    if ui.button("Settings").clicked() {
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
                        crate::tuner::TUNER_DONE_ASSUMED => "Tune OK (assumed)".to_string(),
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
                let mut tuner_vb = ViewportBuilder::default()
                    .with_title("JC-4s Antenna Tuner")
                    .with_inner_size(tuner_sz);
                if let Some(pos) = self.tuner_window_pos {
                    tuner_vb = tuner_vb.with_position(egui::pos2(pos[0], pos[1]));
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
                            tuner_closed = true;
                            return;
                        }
                        egui::CentralPanel::default().show(ctx, |ui| {
                            render_tuner_panel(ui, &tuner_for_window, &status, &mut self.show_tuner_log);

                            ui.add_space(4.0);
                            ui.separator();

                            // Macro button grid
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("Macros").strong());
                                if macro_status.running {
                                    ui.colored_label(
                                        Color32::from_rgb(255, 170, 40),
                                        format!("\u{25B6} {} ({}/{})",
                                            macro_status.current_label,
                                            macro_status.step,
                                            macro_status.total_steps),
                                    );
                                    if ui.button("Abort macro").clicked() {
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
                            if ui.button("Bewerk macros...").clicked() {
                                self.show_macro_editor = true;
                                // Load current slot into editor
                                load_slot_into_editor(
                                    &self.macro_slots, self.editor_slot,
                                    &mut self.editor_label, &mut self.editor_actions,
                                );
                            }

                            ui.add_space(4.0);
                            render_tuner_log(ui, &log_entries, self.show_tuner_log);
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
                        render_macro_editor(
                            ui,
                            &mut self.macro_slots,
                            &mut self.editor_slot,
                            &mut self.editor_label,
                            &mut self.editor_actions,
                            &mut self.show_macro_editor,
                        );
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
                    self.amplitec_log.push_back((ts, format!("Poort A \u{2192} {} ({})", status.switch_a, label)));
                    if self.amplitec_log.len() > 100 { self.amplitec_log.pop_front(); }
                    self.last_switch_a = status.switch_a;
                }
                if status.switch_b != self.last_switch_b && status.switch_b > 0 {
                    let label = self.amplitec_labels[(status.switch_b - 1).min(5) as usize].clone();
                    let ts = chrono::Local::now().format("%H:%M:%S").to_string();
                    self.amplitec_log.push_back((ts, format!("Poort B \u{2192} {} ({})", status.switch_b, label)));
                    if self.amplitec_log.len() > 100 { self.amplitec_log.pop_front(); }
                    self.last_switch_b = status.switch_b;
                }

                let labels = self.amplitec_labels.clone();
                let log_entries: Vec<_> = self.amplitec_log.iter().cloned().collect();
                let amplitec_for_window = amplitec.clone();

                let amp_default_h = if self.show_amplitec_log { 330.0 } else { 175.0 };
                let amp_sz = self.amplitec_window_size.unwrap_or([420.0, amp_default_h]);
                let mut amp_vb = ViewportBuilder::default()
                    .with_title("Amplitec 6/2 Antenna Switch")
                    .with_inner_size(amp_sz);
                if let Some(pos) = self.amplitec_window_pos {
                    amp_vb = amp_vb.with_position(egui::pos2(pos[0], pos[1]));
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
                            amplitec_closed = true;
                            return;
                        }
                        egui::CentralPanel::default().show(ctx, |ui| {
                            render_amplitec_panel(
                                ui, &amplitec_for_window, &status,
                                &labels, &log_entries, &mut self.show_amplitec_log,
                            );
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
                        0 => "Status \u{2192} Off".to_string(),
                        1 => "Status \u{2192} Standby".to_string(),
                        2 => "Status \u{2192} Operate".to_string(),
                        _ => format!("Status \u{2192} Unknown ({})", status.state),
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
                    .with_inner_size(spe_sz)
                    .with_resizable(true);
                if let Some(pos) = self.spe_window_pos {
                    spe_vb = spe_vb.with_position(egui::pos2(pos[0], pos[1]));
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
                            spe_closed = true;
                            return;
                        }
                        let drive_pct = self.drive_level.load(Ordering::Relaxed);
                        egui::CentralPanel::default().show(ctx, |ui| {
                            render_spe_panel(ui, &spe_for_window, &status, &log_entries,
                                &mut self.show_spe_log, &mut self.spe_peak_power, &mut self.spe_peak_time, drive_pct,
                                &self.active_pa);
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
                    .with_inner_size(rf2k_sz)
                    .with_resizable(true);
                if let Some(pos) = self.rf2k_window_pos {
                    rf2k_vb = rf2k_vb.with_position(egui::pos2(pos[0], pos[1]));
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
                    .with_inner_size(ub_sz)
                    .with_resizable(true);
                if let Some(pos) = self.ultrabeam_window_pos {
                    ub_vb = ub_vb.with_position(egui::pos2(pos[0], pos[1]));
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
                            ub_closed = true;
                            return;
                        }
                        egui::CentralPanel::default().show(ctx, |ui| {
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                let amp_status = self.amplitec.as_ref().map(|a| a.status());
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

        // EA7HG Visual Rotor secondary window
        if matches!(self.mode, Mode::Running) && self.show_rotor_window {
            if let Some(ref rotor_ref) = self.rotor {
                let status = rotor_ref.status();
                let rotor_for_window = rotor_ref.clone();

                let rotor_sz = self.rotor_window_size.unwrap_or([340.0, 320.0]);
                let mut rotor_vb = ViewportBuilder::default()
                    .with_title("EA7HG Visual Rotor")
                    .with_inner_size(rotor_sz)
                    .with_resizable(true);
                if let Some(pos) = self.rotor_window_pos {
                    rotor_vb = rotor_vb.with_position(egui::pos2(pos[0], pos[1]));
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
                            rotor_closed = true;
                            return;
                        }
                        egui::CentralPanel::default().show(ctx, |ui| {
                            render_rotor_panel(ui, &rotor_for_window, &status, &mut self.rotor_goto_input);
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
                        egui::Grid::new("hw_grid_srv").num_columns(2).spacing([12.0, 2.0]).show(ui, |ui| {
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
