// SPDX-License-Identifier: GPL-2.0-or-later

mod amplitec;
mod audio_loops;
mod cat;
mod config;
mod dxcluster;
mod macros;
mod network;
mod ptt;
mod rf2k;
mod session;
mod spe_expert;
mod spectrum;
mod tci;
mod tci_commands;
mod tci_parser;
mod tuner;
mod yaesu;
mod ultrabeam;
mod rotor;
mod ui;

use std::collections::VecDeque;

/// Load smart auto-null steps from diversity-smart.txt next to the server executable.
pub fn load_smart_steps_server() -> Vec<(Vec<f32>, bool)> {
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("diversity-smart.txt")));
    let path = match path {
        Some(p) => p,
        None => return Vec::new(),
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut steps = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        let is_phase = line.starts_with('P') || line.starts_with('p');
        let is_gain = line.starts_with('G') || line.starts_with('g');
        if !is_phase && !is_gain { continue; }
        let offsets: Vec<f32> = line[1..].split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        if !offsets.is_empty() {
            steps.push((offsets, is_phase));
        }
    }
    steps
}

/// Check for smart null mode (A line in diversity-smart.txt)
/// Format: A coarsestep coarsesettle finerange finestep finesettle gainrange gainstep gainsettle
pub fn load_smart_null_params() -> Option<Vec<f32>> {
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("diversity-smart.txt")));
    let content = std::fs::read_to_string(path?).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if (line.starts_with('A') || line.starts_with('a')) && !line.starts_with('#') {
            let vals: Vec<f32> = line[1..].split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
            if vals.len() >= 8 {
                return Some(vals);
            }
        }
    }
    None
}

/// Check for lag calibration mode (L line in diversity-smart.txt)
pub fn load_smart_lag_cal() -> bool {
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("diversity-smart.txt")));
    let path = match path { Some(p) => p, None => return false };
    let content = match std::fs::read_to_string(&path) { Ok(c) => c, Err(_) => return false };
    for line in content.lines() {
        let line = line.trim();
        if (line.starts_with('L') || line.starts_with('l')) && !line.starts_with('#') {
            return true;
        }
    }
    false
}

/// Check for fastsweep mode (F line in diversity-smart.txt)
/// Format: F start end step [settle_ms]
/// settle_ms=0 or absent: fast mode (instantaneous signal sample)
/// settle_ms>0: averaging mode (time-domain RMS averaged dBm)
pub fn load_smart_fastsweep() -> Option<(f32, f32, f32, u32, u32)> {
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("diversity-smart.txt")));
    let content = std::fs::read_to_string(path?).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if (line.starts_with('F') || line.starts_with('f')) && !line.starts_with('#') {
            let vals: Vec<f32> = line[1..].split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
            if vals.len() >= 3 {
                let settle = if vals.len() >= 4 { vals[3] as u32 } else { 0 };
                let meter = if vals.len() >= 5 { vals[4] as u32 } else { 0 };
                return Some((vals[0], vals[1], vals[2], settle, meter));
            }
        }
    }
    None
}

/// Read settle time from diversity-smart.txt (S line), default 20ms
pub fn load_smart_settle_ms() -> u32 {
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("diversity-smart.txt")));
    let path = match path { Some(p) => p, None => return 20 };
    let content = match std::fs::read_to_string(&path) { Ok(c) => c, Err(_) => return 20 };
    for line in content.lines() {
        let line = line.trim();
        if (line.starts_with('S') || line.starts_with('s')) && !line.starts_with('#') {
            if let Ok(ms) = line[1..].trim().parse::<u32>() {
                return ms.clamp(5, 500);
            }
        }
    }
    20
}
use std::env;
use std::fs::OpenOptions;
use std::io::Write as IoWrite;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU8, AtomicU64};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::Result;
use log::{info, warn, Level, Log, Metadata, Record};
use tokio::sync::{watch, Mutex};

use sdr_remote_core::DEFAULT_PORT;

use crate::config::ServerConfig;
use crate::network::NetworkService;
use crate::ptt::PttController;
use crate::session::SessionManager;
use crate::spectrum::{Rx2SpectrumProcessor, SpectrumProcessor};

const MAX_LOG_LINES: usize = 500;

pub type LogBuffer = Arc<StdMutex<VecDeque<(Level, String)>>>;

struct GuiLogger {
    inner: env_logger::Logger,
    buffer: LogBuffer,
    file: Option<StdMutex<std::fs::File>>,
}

impl Log for GuiLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        if self.inner.matches(record) {
            self.inner.log(record);
            let line = format!("{}", record.args());
            let mut buf = self.buffer.lock().unwrap();
            if buf.len() >= MAX_LOG_LINES {
                buf.pop_front();
            }
            buf.push_back((record.level(), line.clone()));

            if let Some(ref file_mutex) = self.file {
                if let Ok(mut f) = file_mutex.lock() {
                    let ts = chrono::Local::now().format("%H:%M:%S%.3f");
                    let _ = writeln!(f, "[{} {:5}] {}", ts, record.level(), line);
                    let _ = f.flush();
                }
            }
        }
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

/// Hide the console window in GUI mode (Windows only)
#[cfg(windows)]
fn hide_console() {
    unsafe {
        extern "system" {
            fn FreeConsole() -> i32;
        }
        FreeConsole();
    }
}

fn print_usage() {
    eprintln!("Usage: sdr-remote-server [OPTIONS]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --cat <ADDR>           Thetis CAT address (default: 127.0.0.1:13013)");
    eprintln!("  --tci <ADDR>           Thetis TCI address (e.g. 127.0.0.1:40001)");
    eprintln!("  --thetis-path <PATH>   Path to Thetis.exe for auto-launch on POWER ON");
    eprintln!("  --amplitec-port <COM>  COM port for Amplitec 6/2 antenna switch");
    eprintln!("  --tuner-port <COM>     COM port for JC-4s antenna tuner");
    eprintln!("  --spe-port <COM>       COM port for SPE Expert 1.3K-FA amplifier");
    eprintln!("  --rf2k <ADDR>          RF2K-S Pi address (e.g. 192.168.1.50:8080)");
    eprintln!();
    eprintln!("Without arguments, a settings GUI is shown.");
}

fn main() -> Result<()> {
    // Parse command-line arguments
    let args: Vec<String> = env::args().collect();
    let mut cat_addr: Option<String> = None;
    let mut tci_addr: Option<String> = None;
    let mut thetis_path: Option<String> = None;
    let mut amplitec_port: Option<String> = None;
    let mut tuner_port: Option<String> = None;
    let mut spe_port: Option<String> = None;
    let mut rf2k_addr: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--cat" => {
                i += 1;
                cat_addr = Some(args.get(i).cloned().unwrap_or_default());
            }
            "--tci" => {
                i += 1;
                tci_addr = Some(args.get(i).cloned().unwrap_or_default());
            }
            "--thetis-path" => {
                i += 1;
                thetis_path = Some(args.get(i).cloned().unwrap_or_default());
            }
            "--amplitec-port" => {
                i += 1;
                amplitec_port = Some(args.get(i).cloned().unwrap_or_default());
            }
            "--tuner-port" => {
                i += 1;
                tuner_port = Some(args.get(i).cloned().unwrap_or_default());
            }
            "--spe-port" => {
                i += 1;
                spe_port = Some(args.get(i).cloned().unwrap_or_default());
            }
            "--rf2k" => {
                i += 1;
                rf2k_addr = Some(args.get(i).cloned().unwrap_or_default());
            }
            "--help" | "-h" => {
                print_usage();
                return Ok(());
            }
            other => {
                eprintln!("Unknown argument: {}", other);
                print_usage();
                return Ok(());
            }
        }
        i += 1;
    }

    let has_cli_args = cat_addr.is_some() || tci_addr.is_some()
        || thetis_path.is_some() || amplitec_port.is_some()
        || tuner_port.is_some() || spe_port.is_some() || rf2k_addr.is_some();

    if has_cli_args {
        // CLI mode — normal env_logger, no GUI
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

        let defaults = ServerConfig::default();
        let amp_en = amplitec_port.is_some() || defaults.amplitec_port.is_some();
        let tun_en = tuner_port.is_some() || defaults.tuner_port.is_some();
        let spe_en = spe_port.is_some() || defaults.spe_port.is_some();
        let rf2k_en = rf2k_addr.is_some() || defaults.rf2k_addr.is_some();
        let ub_en = defaults.ultrabeam_port.is_some();
        let rot_en = defaults.rotor_addr.is_some();
        let config = ServerConfig {
            cat_addr: cat_addr.unwrap_or(defaults.cat_addr),
            tci_addr: tci_addr.or(defaults.tci_addr),
            spectrum_enabled: true,
            thetis_path: thetis_path.or(defaults.thetis_path),
            yaesu_port: defaults.yaesu_port,
            yaesu_enabled: defaults.yaesu_enabled,
            yaesu_baud: defaults.yaesu_baud,
            yaesu_audio_device: defaults.yaesu_audio_device,
            amplitec_port: amplitec_port.or(defaults.amplitec_port),
            amplitec_labels: defaults.amplitec_labels,
            show_amplitec_window: false, // no GUI in CLI mode
            tuner_port: tuner_port.or(defaults.tuner_port),
            tuner_assume_tuned: defaults.tuner_assume_tuned,
            show_tuner_window: false, // no GUI in CLI mode
            spe_port: spe_port.or(defaults.spe_port),
            show_spe_window: false, // no GUI in CLI mode
            rf2k_addr: rf2k_addr.or(defaults.rf2k_addr),
            show_rf2k_window: false, // no GUI in CLI mode
            ultrabeam_port: defaults.ultrabeam_port,
            show_ultrabeam_window: false, // no GUI in CLI mode
            rotor_addr: defaults.rotor_addr,
            show_rotor_window: false, // no GUI in CLI mode
            tuner_window_pos: None,
            amplitec_window_pos: None,
            spe_window_pos: None,
            rf2k_window_pos: None,
            ultrabeam_window_pos: None,
            rotor_window_pos: None,
            amplitec_enabled: amp_en,
            tuner_enabled: tun_en,
            spe_enabled: spe_en,
            rf2k_enabled: rf2k_en,
            ultrabeam_enabled: ub_en,
            rotor_enabled: rot_en,
            main_window_pos: None,
            main_window_size: None,
            tuner_window_size: None,
            amplitec_window_size: None,
            spe_window_size: None,
            rf2k_window_size: None,
            ultrabeam_window_size: None,
            rotor_window_size: None,
            autostart: false,
            active_pa: 0,
            dxcluster_server: defaults.dxcluster_server,
            dxcluster_callsign: defaults.dxcluster_callsign,
            dxcluster_enabled: defaults.dxcluster_enabled,
            dxcluster_expiry_min: defaults.dxcluster_expiry_min,
            password: defaults.password,
            totp_secret: defaults.totp_secret,
            totp_enabled: defaults.totp_enabled,
        };
        info!("Starting with cat='{}', tci={:?}", config.cat_addr, config.tci_addr);
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            // Create SPE Expert if configured (CLI mode)
            let spe_arc = if let Some(ref port) = config.spe_port {
                match spe_expert::SpeExpert::new(port) {
                    Ok(dev) => {
                        info!("SPE Expert connected on {}", port);
                        Some(Arc::new(dev))
                    }
                    Err(e) => {
                        warn!("SPE Expert init failed: {}", e);
                        None
                    }
                }
            } else {
                None
            };

            // Create tuner if configured (CLI mode) — after SPE so it gets the reference
            let (tuner_arc, cat_cmd_rx) = if let Some(ref port) = config.tuner_port {
                let (cat_tx, cat_rx) = tokio::sync::mpsc::channel(16);
                match tuner::Jc4sTuner::new(port, cat_tx, spe_arc.clone(), None, config.tuner_assume_tuned) {
                    Ok(t) => {
                        info!("JC-4s tuner connected on {}", port);
                        (Some(Arc::new(t)), Some(cat_rx))
                    }
                    Err(e) => {
                        warn!("JC-4s tuner init failed: {}", e);
                        (None, Some(cat_rx))
                    }
                }
            } else {
                (None, None)
            };

            let server = run_server_async(config, shutdown_rx, None, tuner_arc, spe_arc, None, None, None, cat_cmd_rx, None, None, None, None, None);
            tokio::select! {
                result = server => {
                    if let Err(e) = result {
                        log::error!("Server error: {}", e);
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("Shutting down...");
                    let _ = shutdown_tx.send(true);
                }
            }
        });
    } else {
        // GUI mode — dual logger (stderr + buffer) and hide console
        let log_buffer: LogBuffer = Arc::new(StdMutex::new(VecDeque::new()));

        let env_log = env_logger::Builder::from_env(
            env_logger::Env::default().default_filter_or("info"),
        )
        .build();
        let max_level = env_log.filter();

        // Open log file next to the executable
        let log_file = {
            let exe_dir = env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                .unwrap_or_else(|| std::path::PathBuf::from("."));
            let log_path = exe_dir.join("thetislink-server.log");
            match OpenOptions::new().create(true).write(true).truncate(true).open(&log_path) {
                Ok(f) => {
                    eprintln!("Log file: {}", log_path.display());
                    Some(StdMutex::new(f))
                }
                Err(e) => {
                    eprintln!("Warning: could not open log file {}: {}", log_path.display(), e);
                    None
                }
            }
        };

        log::set_boxed_logger(Box::new(GuiLogger {
            inner: env_log,
            buffer: log_buffer.clone(),
            file: log_file,
        }))
        .unwrap();
        log::set_max_level(max_level);

        #[cfg(windows)]
        hide_console();

        let config = config::load();

        let icon = egui::IconData {
            rgba: include_bytes!(concat!(env!("OUT_DIR"), "/icon_rgba.bin")).to_vec(),
            width: 32,
            height: 32,
        };
        let main_sz = config.main_window_size.unwrap_or([500.0, 400.0]);
        let mut viewport = egui::ViewportBuilder::default()
            .with_inner_size(main_sz)
            .with_title(format!("ThetisLink Server v{}", sdr_remote_core::version_string()))
            .with_icon(std::sync::Arc::new(icon));
        if let Some(pos) = config.main_window_pos {
            viewport = viewport.with_position(egui::pos2(pos[0], pos[1]));
        }
        let native_options = eframe::NativeOptions {
            viewport,
            ..Default::default()
        };

        let _ = eframe::run_native(
            &format!("ThetisLink Server v{}", sdr_remote_core::version_string()),
            native_options,
            Box::new(move |_cc| Ok(Box::new(ui::ServerApp::new(config, log_buffer)))),
        );
    }

    Ok(())
}

pub async fn run_server_async(
    config: ServerConfig,
    shutdown_rx: watch::Receiver<bool>,
    amplitec_prebuilt: Option<Arc<amplitec::AmplitecSwitch>>,
    tuner_prebuilt: Option<Arc<tuner::Jc4sTuner>>,
    spe_prebuilt: Option<Arc<spe_expert::SpeExpert>>,
    rf2k_prebuilt: Option<Arc<rf2k::Rf2k>>,
    ultrabeam_prebuilt: Option<Arc<ultrabeam::UltraBeam>>,
    rotor_prebuilt: Option<Arc<rotor::Rotor>>,
    cat_cmd_rx: Option<tokio::sync::mpsc::Receiver<String>>,
    drive_level_shared: Option<Arc<AtomicU8>>,
    active_pa_shared: Option<Arc<AtomicU8>>,
    vfo_freq_shared: Option<Arc<AtomicU64>>,
    vfo_b_freq_shared: Option<Arc<AtomicU64>>,
    yaesu_prebuilt: Option<Arc<yaesu::YaesuRadio>>,
) -> Result<()> {
    let bind_addr: SocketAddr = format!("0.0.0.0:{}", DEFAULT_PORT).parse()?;
    info!("ThetisLink Server v{} listening on {}", sdr_remote_core::version_string(), bind_addr);
    info!("PA3GHM — Remote control for Thetis SDR + Yaesu FT-991A");
    info!("Licensed under GPL-2.0-or-later — source: https://github.com/cjenschede/ThetisLink");

    // Session manager
    let session = Arc::new(Mutex::new(SessionManager::new(config.password.clone(), config.totp_secret.clone())));

    // PTT controller: TCI or CAT backend
    let ptt = if let Some(ref tci_addr) = config.tci_addr {
        info!("TCI mode: connecting to ws://{}", tci_addr);
        // Also create auxiliary TCP CAT connection for commands TCI doesn't support (ZZLA/ZZLE/ZZBY)
        Arc::new(Mutex::new(PttController::new_tci(Some(tci_addr), Some(&config.cat_addr), config.thetis_path.clone())))
    } else {
        Arc::new(Mutex::new(PttController::new(Some(&config.cat_addr), config.thetis_path.clone())))
    };

    // Spectrum processor
    let spectrum = Arc::new(Mutex::new(SpectrumProcessor::new()));

    // RX2 spectrum processor
    let rx2_spectrum = Arc::new(Mutex::new(Rx2SpectrumProcessor::new()));

    // TCI IQ stream → spectrum processor
    if config.spectrum_enabled {
        let tci_iq_rate = 384_000u32; // Initial default, updated dynamically from TCI IQ frame header
        {
            let mut s = spectrum.lock().await;
            s.init_ddc_fft(tci_iq_rate);
            s.set_tci_mode(true);
        }
        {
            let mut s = rx2_spectrum.lock().await;
            s.init_ddc_fft(tci_iq_rate);
            s.set_tci_mode(true);
        }
        info!("TCI mode: spectrum from TCI IQ stream ({}kHz)", tci_iq_rate / 1000);
    }

    // Amplitec antenna switch — use prebuilt (from GUI) or create here (CLI mode)
    let amplitec = if amplitec_prebuilt.is_some() {
        amplitec_prebuilt
    } else if config.amplitec_enabled && config.amplitec_port.is_some() {
        let port_name = config.amplitec_port.as_ref().unwrap();
        match amplitec::AmplitecSwitch::new(port_name) {
            Ok(sw) => {
                info!("Amplitec 6/2 connected on {}", port_name);
                Some(Arc::new(sw))
            }
            Err(e) => {
                warn!("Amplitec init failed: {}", e);
                None
            }
        }
    } else {
        None
    };

    // SPE Expert amplifier — use prebuilt (from GUI) or create here (CLI mode)
    // Must be created BEFORE tuner so tuner can reference it for safe tune
    let spe = if spe_prebuilt.is_some() {
        spe_prebuilt
    } else if config.spe_enabled && config.spe_port.is_some() {
        let port_name = config.spe_port.as_ref().unwrap();
        match spe_expert::SpeExpert::new(port_name) {
            Ok(dev) => {
                info!("SPE Expert connected on {}", port_name);
                Some(Arc::new(dev))
            }
            Err(e) => {
                warn!("SPE Expert init failed: {}", e);
                None
            }
        }
    } else {
        None
    };

    // Shared CAT channel for tuner + RF2K-S tune carrier (CLI mode only, GUI passes prebuilt)
    // In GUI mode, cat_cmd_rx already carries CAT commands from ui.rs's shared channel.
    // In CLI mode, we create one here if tuner or RF2K needs it.
    let needs_cat = !tuner_prebuilt.is_some() && ((config.tuner_enabled && config.tuner_port.is_some()) || (config.rf2k_enabled && config.rf2k_addr.is_some()));
    let (cli_cat_tx, cli_cat_rx) = if needs_cat && cat_cmd_rx.is_none() {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    // Merge: use existing cat_cmd_rx (from GUI) or new CLI channel
    let shared_cat_rx = cat_cmd_rx.or(cli_cat_rx);

    // RF2K-S power amplifier (HTTP) — use prebuilt (from GUI) or create here (CLI mode)
    // Must be created BEFORE tuner so tuner can reference it for safe tune
    let rf2k = if rf2k_prebuilt.is_some() {
        rf2k_prebuilt
    } else if config.rf2k_enabled && config.rf2k_addr.is_some() {
        let addr = config.rf2k_addr.as_ref().unwrap();
        info!("RF2K-S connecting to {}", addr);
        Some(Arc::new(rf2k::Rf2k::new(addr, cli_cat_tx.clone(), drive_level_shared.clone())))
    } else {
        None
    };

    // JC-4s tuner — use prebuilt (from GUI) or create here (CLI mode)
    let spe_arc = spe.as_ref().map(|s| s.clone());
    let rf2k_arc = rf2k.as_ref().map(|r| r.clone());
    let (tuner, tuner_cat_rx) = if tuner_prebuilt.is_some() {
        (tuner_prebuilt, shared_cat_rx)
    } else if config.tuner_enabled && config.tuner_port.is_some() {
        let port_name = config.tuner_port.as_ref().unwrap();
        let cat_tx = cli_cat_tx.unwrap_or_else(|| {
            let (tx, _) = tokio::sync::mpsc::channel(16);
            tx
        });
        let (cat_tx_for_tuner, cat_rx_for_tuner) = if shared_cat_rx.is_some() {
            (cat_tx, shared_cat_rx)
        } else {
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            (tx, Some(rx))
        };
        match tuner::Jc4sTuner::new(port_name, cat_tx_for_tuner, spe_arc, rf2k_arc, config.tuner_assume_tuned) {
            Ok(t) => {
                info!("JC-4s tuner connected on {}", port_name);
                (Some(Arc::new(t)), cat_rx_for_tuner)
            }
            Err(e) => {
                warn!("JC-4s tuner init failed: {}", e);
                (None, cat_rx_for_tuner)
            }
        }
    } else {
        (None, shared_cat_rx)
    };

    // UltraBeam RCU-06 — use prebuilt (from GUI) or create here (CLI mode)
    let ultrabeam = if ultrabeam_prebuilt.is_some() {
        ultrabeam_prebuilt
    } else if config.ultrabeam_enabled && config.ultrabeam_port.is_some() {
        let port_name = config.ultrabeam_port.as_ref().unwrap();
        match ultrabeam::UltraBeam::new(port_name) {
            Ok(dev) => {
                info!("UltraBeam RCU-06 connected on {}", port_name);
                Some(Arc::new(dev))
            }
            Err(e) => {
                warn!("UltraBeam init failed: {}", e);
                None
            }
        }
    } else {
        None
    };

    // EA7HG Visual Rotor — use prebuilt (from GUI) or create here (CLI mode)
    let rotor_inst = if rotor_prebuilt.is_some() {
        rotor_prebuilt
    } else if config.rotor_enabled && config.rotor_addr.is_some() {
        let addr = config.rotor_addr.as_ref().unwrap();
        info!("Rotor connecting to {}", addr);
        Some(Arc::new(rotor::Rotor::new(addr)))
    } else {
        None
    };

    // DX Cluster
    let dxcluster = if config.dxcluster_enabled {
        info!("DX Cluster: starting (server={}, callsign={}, expiry={}min)", config.dxcluster_server, config.dxcluster_callsign, config.dxcluster_expiry_min);
        Some(Arc::new(dxcluster::DxCluster::new(&config.dxcluster_server, &config.dxcluster_callsign, config.dxcluster_expiry_min)))
    } else {
        None
    };

    // Network service
    let network = NetworkService::new(
        bind_addr,
        session.clone(),
        ptt.clone(),
        spectrum.clone(),
        rx2_spectrum.clone(),
        shutdown_rx.clone(),
        amplitec,
        tuner,
        spe,
        rf2k,
        ultrabeam,
        rotor_inst,
        config.clone(),
        tuner_cat_rx,
        drive_level_shared,
        active_pa_shared,
        dxcluster,
        vfo_freq_shared,
        vfo_b_freq_shared,
        yaesu_prebuilt,
    )
    .await?;

    info!("Server ready, waiting for connections...");

    // Run until shutdown signal
    let mut shutdown = shutdown_rx;
    tokio::select! {
        result = network.run() => {
            if let Err(e) = result {
                log::error!("Network error: {}", e);
            }
        }
        _ = async { while !*shutdown.borrow_and_update() { shutdown.changed().await.ok(); } } => {
            info!("Shutdown signal received.");
        }
    }

    // Ensure PTT is released on shutdown
    ptt.lock().await.release().await;

    Ok(())
}
