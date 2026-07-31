// SPDX-License-Identifier: GPL-2.0-or-later

mod amplitec;
mod audio_loops;
mod audio_stats;
mod config;
mod mcp2221_debug;
mod mcp2221_scan;
mod mcp2221_yaesu_rotor;
mod mdns;
mod ctun_recenter;
mod dxcluster;
mod macros;
mod network;
mod power_cap;
mod ptt;
mod rf2k;
mod session;
mod spe_expert;
mod spectrum;
mod tci;
mod tci_commands;
mod tracked_socket;
mod tci_parser;
mod tuner;
mod yaesu;
mod ultrabeam;
mod rotor;
mod pstrotator;
mod pstrotator_listen;
mod ui;
mod vrx_bridge;
mod vrx_manager;

// Server-GUI vertalingen (rust-i18n). Basis = Engels (fallback); de gebruiker
// kiest een taal in Settings (persisted als `language=`), toegepast via set_locale.
rust_i18n::i18n!("locales", fallback = "en");

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
    eprintln!("  --tci <ADDR>           Thetis TCI address (e.g. 127.0.0.1:40001)");
    eprintln!("  --thetis-path <PATH>   Path to Thetis.exe for auto-launch on POWER ON");
    eprintln!("  --amplitec-port <COM>  COM port for Amplitec 6/2 antenna switch");
    eprintln!("  --spe-port <COM>       COM port for SPE Expert 1.3K-FA amplifier");
    eprintln!("  --rf2k <ADDR>          RF2K-S Pi address (e.g. 192.168.1.50:8080)");
    eprintln!();
    eprintln!("Without arguments, a settings GUI is shown.");
}

/// Install a panic hook that writes panic info to thetislink-server.log next to
/// the executable, then delegates to the default handler so CLI-mode stderr is
/// unchanged.
///
/// In GUI-mode autostart, stderr goes nowhere - without this hook a panic in a
/// spawned tokio task (e.g. peripheral init racing an unstable USB device on
/// cold-boot) is invisible. Direct file-write avoids depending on `log::` macros,
/// which may be unsafe during panic-unwinding.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let log_path = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("thetislink-server.log");

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>");
        let location = info.location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let payload = info.payload();
        let message = payload.downcast_ref::<&str>().copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("<non-string panic payload>");
        let backtrace = std::backtrace::Backtrace::force_capture();

        let entry = format!(
            "\n[PANIC] ts={} thread={} location={} message={}\nbacktrace:\n{}\n",
            timestamp, thread_name, location, message, backtrace
        );

        // Best-effort append. If file-open fails the default handler still runs.
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true).append(true).open(&log_path)
        {
            let _ = f.write_all(entry.as_bytes());
        }

        default_hook(info);
    }));
}

fn main() -> Result<()> {
    install_panic_hook();

    // Parse command-line arguments
    let args: Vec<String> = env::args().collect();
    let mut tci_addr: Option<String> = None;
    let mut thetis_path: Option<String> = None;
    let mut amplitec_port: Option<String> = None;
    let mut spe_port: Option<String> = None;
    let mut rf2k_addr: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
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
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let has_cli_args = tci_addr.is_some()
        || thetis_path.is_some() || amplitec_port.is_some()
        || spe_port.is_some() || rf2k_addr.is_some();

    if has_cli_args {
        // CLI mode - normal env_logger, no GUI
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

        let defaults = ServerConfig::default();
        let amp_en = amplitec_port.is_some() || defaults.amplitec_port.is_some();
        let spe_en = spe_port.is_some() || defaults.spe_port.is_some();
        let rf2k_en = rf2k_addr.is_some() || defaults.rf2k_addr.is_some();
        let ub_en = defaults.ultrabeam_port.is_some();
        let rot_en = defaults.rotor_addr.is_some();
        let config = ServerConfig {
            tci_addr: tci_addr.or(defaults.tci_addr),
            spectrum_enabled: true,
            rx2_present: defaults.rx2_present,
            thetis_path: thetis_path.or(defaults.thetis_path),
            yaesu_port: defaults.yaesu_port,
            yaesu_enabled: defaults.yaesu_enabled,
            yaesu_baud: defaults.yaesu_baud,
            yaesu_ssb_switch_on_ptt: defaults.yaesu_ssb_switch_on_ptt,
            yaesu_audio_device: defaults.yaesu_audio_device,
            yaesu_audio_output_device: defaults.yaesu_audio_output_device,
            yaesu2_port: defaults.yaesu2_port,
            yaesu2_enabled: defaults.yaesu2_enabled,
            yaesu2_baud: defaults.yaesu2_baud,
            yaesu2_audio_device: defaults.yaesu2_audio_device,
            yaesu2_audio_output_device: defaults.yaesu2_audio_output_device,
            yaesu2_audio_channel: defaults.yaesu2_audio_channel,
            amplitec_port: amplitec_port.or(defaults.amplitec_port),
            amplitec_labels: defaults.amplitec_labels,
            amplitec_max_w: defaults.amplitec_max_w,
            amplitec_tx_blocked: defaults.amplitec_tx_blocked,
            show_amplitec_window: false, // no GUI in CLI mode
            show_tuner_window: false, // no GUI in CLI mode
            spe_port: spe_port.or(defaults.spe_port),
            show_spe_window: false, // no GUI in CLI mode
            rf2k_addr: rf2k_addr.or(defaults.rf2k_addr),
            show_rf2k_window: false, // no GUI in CLI mode
            ultrabeam_port: defaults.ultrabeam_port,
            show_ultrabeam_window: false, // no GUI in CLI mode
            rotor_addr: defaults.rotor_addr,
            show_rotor_window: false, // no GUI in CLI mode
            rotor_backend: defaults.rotor_backend,
            pstrotator_host: defaults.pstrotator_host,
            pstrotator_port: defaults.pstrotator_port,
            pstrotator_feedback_port: defaults.pstrotator_feedback_port,
            pstrotator_has_elevation: defaults.pstrotator_has_elevation,
            pstrotator_listen_enabled: defaults.pstrotator_listen_enabled,
            pstrotator_listen_port: defaults.pstrotator_listen_port,
            tuner_window_pos: None,
            amplitec_window_pos: None,
            spe_window_pos: None,
            rf2k_window_pos: None,
            ultrabeam_window_pos: None,
            rotor_window_pos: None,
            amplitec_enabled: amp_en,
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
            theme: defaults.theme,
            theme_custom: defaults.theme_custom,
            language: defaults.language,
            autostart: false,
            active_pa: 0,
            rf2k_saved_drive: None,
            spe_saved_drive: None,
            ultrabeam_show_menu: defaults.ultrabeam_show_menu,
            mcp2221_section_expanded: defaults.mcp2221_section_expanded,
            dxcluster_server: defaults.dxcluster_server,
            dxcluster_callsign: defaults.dxcluster_callsign,
            dxcluster_enabled: defaults.dxcluster_enabled,
            dxcluster_expiry_min: defaults.dxcluster_expiry_min,
            password: defaults.password,
            totp_secret: defaults.totp_secret,
            totp_enabled: defaults.totp_enabled,
            friendly_name: defaults.friendly_name,
            relay_enabled: defaults.relay_enabled,
            relay_url: defaults.relay_url,
            relay_station: defaults.relay_station,
            relay_token: defaults.relay_token,
            relay_udp_enabled: defaults.relay_udp_enabled,
            tuners: defaults.tuners,
            rotors: defaults.rotors,
        };
        info!("Starting with tci={:?}", config.tci_addr);
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

            // CLI mode: tuners are built inside `run_server_async` from
            // `config.tuners` (post-MCP2221A refactor); no prebuild here.
            let server = run_server_async(config, shutdown_rx, None, None, spe_arc, None, None, None, None, None, None, None, None, None, None);
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
        // GUI mode - dual logger (stderr + buffer) and hide console
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
        // Pas de opgeslagen UI-taal toe voordat de GUI opstart.
        rust_i18n::set_locale(&config.language);

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
    tuners_prebuilt: Option<Arc<tuner::Tuners>>,
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
    // PATCH-2: shared lock-free state for the Status panel. CLI mode
    // passes `None`; GUI mode passes pre-allocated probes.
    status_panel_state: Option<crate::audio_stats::StatusPanelShared>,
) -> Result<()> {
    let bind_addr: SocketAddr = format!("0.0.0.0:{}", DEFAULT_PORT).parse()?;
    info!("ThetisLink Server v{} starting up...", sdr_remote_core::version_string());
    info!("PA3GHM - Remote control for Thetis SDR + Yaesu FT-991A / FTX-1 (dual-radio)");
    info!("Licensed under GPL-2.0-or-later - source: https://github.com/cjenschede/ThetisLink");

    // PATCH-2: derive Status-panel state from the optional incoming bundle
    // (GUI mode) or construct a one-shot bundle for CLI mode so the rest of
    // run_server_async can always operate on real Arc<>s.
    let status_panel_state = status_panel_state
        .unwrap_or_else(audio_stats::StatusPanelShared::new);
    let audio_stats = status_panel_state.audio.clone();
    let tci_probe = status_panel_state.tci.clone();
    let server_start = status_panel_state.server_start;

    // Session manager
    let session = Arc::new(Mutex::new(SessionManager::new(config.password.clone(), config.totp_secret.clone())));
    // PATCH-2: publish to the Status-panel slot so the UI can read it.
    // CLI mode also publishes - harmless even though no UI reads from it.
    let _ = status_panel_state.session_slot.set(session.clone());

    // PTT controller. `tci_addr=None` is a valid Yaesu-only setup; do not
    // silently probe localhost because that makes Thetis look required.
    let tci_addr_configured = config.tci_addr.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false);
    if let Some(ref tci_addr_str) = config.tci_addr {
        if tci_addr_configured {
            info!("TCI mode: connecting to ws://{}", tci_addr_str);
        }
    }
    if !tci_addr_configured {
        info!("TCI mode: disabled (no Thetis TCI address configured)");
    }
    let ptt = Arc::new(Mutex::new(PttController::new_tci(
        config.tci_addr.as_deref().filter(|s| !s.trim().is_empty()),
        config.thetis_path.clone(),
    )));

    // Spectrum processor
    let spectrum = Arc::new(Mutex::new(SpectrumProcessor::new()));

    // RX2 spectrum processor
    let rx2_spectrum = Arc::new(Mutex::new(Rx2SpectrumProcessor::new()));

    // TCI IQ stream -> spectrum processor
    if config.spectrum_enabled && tci_addr_configured {
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

    // Amplitec antenna switch - use prebuilt (from GUI) or create here (CLI mode)
    // De serial-thread retry zelf naar binnen toe, dus we maken de
    // instance altijd zodra de poort geconfigureerd is - ook als het
    // apparaat nu offline staat. De UI ziet `connected=false` tot het
    // bord weer aangesloten is.
    let amplitec = if amplitec_prebuilt.is_some() {
        amplitec_prebuilt
    } else if config.amplitec_enabled && config.amplitec_port.is_some() {
        let port_name = config.amplitec_port.as_ref().unwrap();
        info!("Amplitec 6/2 starting on {} (thread retries until reachable)", port_name);
        Some(Arc::new(amplitec::AmplitecSwitch::new(port_name)))
    } else {
        None
    };

    // SPE Expert amplifier - use prebuilt (from GUI) or create here (CLI mode)
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
    let any_tuner_enabled = config.tuners.iter().any(|t| t.enabled);
    let needs_cat = tuners_prebuilt.is_none() && (any_tuner_enabled || (config.rf2k_enabled && config.rf2k_addr.is_some()));
    let (cli_cat_tx, cli_cat_rx) = if needs_cat && cat_cmd_rx.is_none() {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    // Merge: use existing cat_cmd_rx (from GUI) or new CLI channel
    let shared_cat_rx = cat_cmd_rx.or(cli_cat_rx);

    // RF2K-S power amplifier (HTTP) - use prebuilt (from GUI) or create here (CLI mode)
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

    // StockCorner tuners - use prebuilt (from GUI) or create here (CLI mode).
    // Multi-instance: 0, 1 or 2 tuners, each on its own MCP2221A board.
    let spe_arc = spe.as_ref().map(|s| s.clone());
    let rf2k_arc = rf2k.as_ref().map(|r| r.clone());
    let (tuners, tuner_cat_rx) = if let Some(t) = tuners_prebuilt {
        (t, shared_cat_rx)
    } else if any_tuner_enabled {
        // Need a tx half for the tuner thread(s) to push ZZTU1/ZZTU0. Prefer
        // the CLI-side tx (so the rx half is `shared_cat_rx`); otherwise
        // create a throw-away channel and let the GUI-supplied rx survive.
        let (cat_tx_for_tuner, cat_rx_for_tuner) = if let Some(tx) = cli_cat_tx {
            (tx, shared_cat_rx)
        } else if shared_cat_rx.is_some() {
            // GUI-supplied rx: we don't have a sibling tx, so spawn a fresh
            // (orphan) one. The tuner's writes go to /dev/null, which is OK
            // because the GUI path also routes its own ZZTU1 via the
            // pre-existing cat channel.
            let (tx, _) = tokio::sync::mpsc::channel::<String>(16);
            (tx, shared_cat_rx)
        } else {
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            (tx, Some(rx))
        };
        let collection = tuner::Tuners::new(&config.tuners, cat_tx_for_tuner, spe_arc.clone(), rf2k_arc.clone());
        (Arc::new(collection), cat_rx_for_tuner)
    } else {
        // No tuner enabled: build an empty collection so downstream code can
        // still call .primary() (returns None) without conditionals.
        let (throwaway_tx, _) = tokio::sync::mpsc::channel(16);
        let collection = tuner::Tuners::new(&config.tuners, throwaway_tx, spe_arc.clone(), rf2k_arc.clone());
        (Arc::new(collection), shared_cat_rx)
    };
    // Backwards-compat: legacy paths (macros, settings UI, single-tuner status
    // panel) consume `Option<Arc<TunerInstance>>` - give them the primary
    // (first enabled) tuner, matching the pre-multi-tuner behaviour.
    let tuner = tuners.primary();
    // Publish the collection to the Status-panel slot so the UI can render
    // per-tuner debug rows (auto-baseline, override, ratio). `.set` is
    // idempotent-safe via OnceLock.
    let _ = status_panel_state.tuners_slot.set(tuners.clone());

    // PATCH-yaesu-rotor-mcp2221 fase 3+4+5: RotorInstance wordt nu in
    // de rotor-backend match-arm hieronder aangemaakt (alleen voor
    // backend == "mcp2221_yaesu"), zodat de Rotor-facade en de
    // status_panel-RotorInstance één gedeelde MCP2221A driver delen.
    // De drie rotor-backends (EA7HG TCP / PstRotator UDP / Adafruit
    // MCP2221A) zijn mutex-exclusief - slechts één tegelijk actief om
    // hardware-conflicten te voorkomen.

    // UltraBeam RCU-06 - use prebuilt (from GUI) or create here (CLI mode)
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

    // Rotor - use prebuilt (from GUI) or create here (CLI mode).
    // Backend keuze: EA7HG Visual Rotor (default) of PstRotator XML/UDP.
    let rotor_inst = if rotor_prebuilt.is_some() {
        rotor_prebuilt
    } else if config.rotor_enabled {
        match config.rotor_backend.as_str() {
            "pstrotator" => {
                if config.pstrotator_host.is_empty() {
                    log::warn!(
                        "PstRotator backend selected but host is empty; rotor disabled"
                    );
                    None
                } else {
                    info!(
                        "Rotor (PstRotator) -> {}:{} (feedback :{}, ele={})",
                        config.pstrotator_host,
                        config.pstrotator_port,
                        config.pstrotator_feedback_port,
                        config.pstrotator_has_elevation,
                    );
                    let (tx, status) = pstrotator::spawn(pstrotator::PstRotatorConfig {
                        host: config.pstrotator_host.clone(),
                        port: config.pstrotator_port,
                        feedback_port: config.pstrotator_feedback_port,
                        has_elevation: config.pstrotator_has_elevation,
                    });
                    Some(Arc::new(rotor::Rotor::from_handles(tx, status)))
                }
            }
            "mcp2221_yaesu" => {
                // Aansturing direct via een Adafruit MCP2221A-printje
                // (PATCH-yaesu-rotor-mcp2221). Slot 0 in config.rotors;
                // de RotorInstance maakt zelf de poll-thread + Rotor
                // facade. We publiceren de RotorInstance óók in
                // `rotor_slot` zodat het status-panel zijn live ADC +
                // Park-knoppen + DAC-slider blijft tonen, en de Rotor
                // facade gaat de bestaande client-rotor-window kant op.
                if let Some(rot_cfg) = config.rotors.first() {
                    if rot_cfg.enabled && rot_cfg.mcp_serial.starts_with("rot_") {
                        let label = if rot_cfg.name.is_empty() {
                            rot_cfg.mcp_serial.clone()
                        } else {
                            rot_cfg.name.clone()
                        };
                        let calibration = mcp2221_yaesu_rotor::RotorCalibration {
                            v_at_0deg: rot_cfg.v_at_0deg,
                            v_at_max_deg: rot_cfg.v_at_max_deg,
                            max_deg: rot_cfg.max_deg,
                            ramp_pct_per_sec: rot_cfg.ramp_pct_per_sec,
                            shortest_route_in_overlap: rot_cfg.shortest_route_in_overlap,
                        };
                        let inst = mcp2221_yaesu_rotor::RotorInstance::new(
                            0,
                            &rot_cfg.mcp_serial,
                            &label,
                            calibration,
                        );
                        let facade = inst.make_rotor_facade();
                        let _ = status_panel_state.rotor_slot.set(inst);
                        info!(
                            "Rotor (Adafruit MCP2221A) instance gebonden - serial \"{}\" (label \"{}\", cal {:.3}V->{:.3}V @ {}°)",
                            rot_cfg.mcp_serial, label,
                            rot_cfg.v_at_0deg, rot_cfg.v_at_max_deg, rot_cfg.max_deg
                        );
                        Some(Arc::new(facade))
                    } else {
                        log::warn!(
                            "mcp2221_yaesu backend selected but config.rotors[0] is empty or disabled"
                        );
                        None
                    }
                } else {
                    log::warn!(
                        "mcp2221_yaesu backend selected but no rotor in config.rotors - gebruik wizard om een rot_<naam> bord te claimen"
                    );
                    None
                }
            }
            _ => {
                if let Some(addr) = config.rotor_addr.as_ref() {
                    info!("Rotor (EA7HG) connecting to {}", addr);
                    Some(Arc::new(rotor::Rotor::new(addr)))
                } else {
                    None
                }
            }
        }
    } else {
        None
    };

    // PstRotator UDP-listener (v2.1.1+): luistert parallel aan de
    // actieve `rotor_backend` voor inkomende azimuth-broadcasts. Spawn
    // alleen als er een actieve Rotor-facade is om het commando naartoe
    // te sturen - zonder rotor geen punt.
    let pstrotator_listen_shutdown = if config.pstrotator_listen_enabled {
        if let Some(rotor_arc) = rotor_inst.as_ref() {
            if config.rotor_backend == "pstrotator" {
                warn!(
                    "PstRotator listener: rotor_backend is ook 'pstrotator' - \
                     mogelijk feedback-loop met outgoing replies. Listener \
                     blijft actief maar wees alert op duplicaten."
                );
            }
            match pstrotator_listen::spawn(
                pstrotator_listen::ListenConfig { port: config.pstrotator_listen_port },
                rotor_arc.as_ref().clone(),
            ) {
                Ok(shutdown) => Some(shutdown),
                Err(e) => {
                    warn!(
                        "PstRotator listener bind on UDP {} faalde: {} (listener uitgeschakeld voor deze run)",
                        config.pstrotator_listen_port, e
                    );
                    None
                }
            }
        } else {
            warn!(
                "PstRotator listener ingeschakeld maar geen actieve rotor-backend - \
                 ingeschakelde listener zonder doel doet niets, listener overgeslagen"
            );
            None
        }
    } else {
        None
    };

    // DX Cluster - needs a login callsign (no hardcoded default; set it in
    // server settings). Skip the connection while it's blank so we never log in
    // with an empty/placeholder call.
    let dxcluster = if config.dxcluster_enabled && !config.dxcluster_callsign.trim().is_empty() {
        info!("DX Cluster: starting (server={}, callsign={}, expiry={}min)", config.dxcluster_server, config.dxcluster_callsign, config.dxcluster_expiry_min);
        Some(Arc::new(dxcluster::DxCluster::new(&config.dxcluster_server, &config.dxcluster_callsign, config.dxcluster_expiry_min)))
    } else {
        if config.dxcluster_enabled {
            info!("DX Cluster: enabled but no callsign configured - not connecting (set your callsign in server settings)");
        }
        None
    };

    // Relay Phase C: outbound relay path that also tunnels TL frames, so a client
    // that cannot port-forward can reach the server through the VPS relay instead
    // of direct UDP. The tunnel channels connect the relay monitor to the
    // TrackedSocket seam; direct UDP stays the default and is byte-identical.
    let (relay_monitor, relay_socket_ends) = if config.relay_enabled {
        let relay_cfg = sdr_remote_relay::RelayConfig {
            enabled: config.relay_enabled,
            url: config.relay_url.clone(),
            station: config.relay_station.clone(),
            token: config.relay_token.clone(),
            role: sdr_remote_relay::RelayRole::Station,
            // Station uses replace-on-reconnect already (single station slot); no
            // per-install id needed. Station identity is the station name; no device label.
            instance: String::new(),
            name: String::new(),
            udp_port: sdr_remote_relay::relay_udp_port_resolve(config.relay_udp_enabled),
        };
        // Tunnel channels between TrackedSocket and the relay monitor:
        //  - uplink:  TrackedSocket (send to sentinel) -> monitor (TLT1 over WS)
        //  - inbound: monitor (decoded TLT1)           -> TrackedSocket (recv_from)
        let (uplink_tx, uplink_rx) = tokio::sync::mpsc::unbounded_channel();
        let (inbound_tx, inbound_rx) = tokio::sync::mpsc::unbounded_channel();
        let sentinel = crate::tracked_socket::relay_sentinel_for(0);
        let tunnel = sdr_remote_relay::RelayTunnel { sentinel, inbound_tx, uplink_rx };
        // Own thread (see build 123): keeps the relay ping/echo off the busy DSP
        // runtime, so its scheduling doesn't inflate the measured RTT/jitter.
        let monitor = sdr_remote_relay::RelayMonitor::start_threaded_tunnel(relay_cfg, tunnel);
        let _ = status_panel_state.relay_status.set(monitor.status_handle());
        (Some(monitor), Some((uplink_tx, inbound_rx)))
    } else {
        (None, None)
    };
    // Network service
    // Discard the `tuner` binding (kept for compat with macros / settings) -
    // NetworkService now takes the full Tuners collection so it can route
    // Tune commands per Amplitec-A position.
    let _ = tuner;

    // Dual-radio slot 1 (PATCH-dual-radio-991a-ftx1, Optie B-prime). De 2e radio
    // wordt hier uit config aangemaakt (werkt in GUI- én headless-mode). Model is
    // per-poort autodetect via `ID;` (detect_model) -> élke combinatie 2×991A /
    // 2×FTX1 / mix werkt; faalt detect (radio uit) -> FTX1-aanname-label, bring-up
    // logt straks het echte ID. Slot 0 blijft volledig ongemoeid.
    yaesu::log_input_devices();
    let yaesu2_prebuilt: Option<Arc<yaesu::YaesuRadio>> = if config.yaesu2_enabled {
        if let Some(ref port) = config.yaesu2_port {
            let baud = config.yaesu2_baud;
            let (model, det_baud) = yaesu::detect_model(port, baud)
                .unwrap_or((yaesu::RadioModel::Ftx1, baud));
            info!("[radio1] slot 1 enabled: {} @ {} baud, model={:?}", port, det_baud, model);
            let audio = config.yaesu2_audio_device.clone();
            let audio_out = config.yaesu2_audio_output_device.clone();
            match yaesu::YaesuRadio::new_with_model(port, det_baud, audio.as_deref(), audio_out.as_deref(), model, 1, config.yaesu2_audio_channel, config.yaesu_ssb_switch_on_ptt) {
                Ok(r) => Some(Arc::new(r)),
                Err(e) => { warn!("[radio1] init failed: {} - slot 1 uit, server draait door", e); None }
            }
        } else {
            warn!("[radio1] enabled maar geen yaesu2_port geconfigureerd - slot 1 uit");
            None
        }
    } else {
        None
    };

    let network = NetworkService::new(
        bind_addr,
        session.clone(),
        ptt.clone(),
        spectrum.clone(),
        rx2_spectrum.clone(),
        shutdown_rx.clone(),
        amplitec,
        tuners,
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
        yaesu2_prebuilt,
        audio_stats.clone(),
        tci_probe.clone(),
        server_start,
        status_panel_state.bind_status.clone(),
        relay_socket_ends,
    )
    .await?;

    info!("Server ready, waiting for connections...");

    // PATCH-3: advertise on the local network so clients can find us
    // without typing an IP. mDNS failure is non-fatal - manual entry
    // remains the always-on fallback in the clients.
    let mdns_advertiser = match mdns::MdnsAdvertiser::start(
        sdr_remote_core::DEFAULT_PORT,
        config.friendly_name.as_deref(),
    ) {
        Ok(adv) => Some(adv),
        Err(e) => {
            log::warn!("mDNS advertise failed (non-fatal): {}", e);
            None
        }
    };

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

    // PstRotator-listener (v2.1.1+): signal de UDP/TCP listener-threads
    // dat de server stopt zodat ze poort 12001 vrijgeven binnen
    // READ_TIMEOUT (~500 ms). Zonder dit bleef de poort vasthouden tot
    // proces-exit en kon een directe server-herstart faken bind-failure.
    if let Some(s) = pstrotator_listen_shutdown.as_ref() {
        s.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    if let Some(monitor) = relay_monitor.as_ref() {
        monitor.stop();
    }
    // PATCH-3: deregister the mDNS service so clients see the server
    // disappear instead of relying on TTL expiry.
    if let Some(adv) = mdns_advertiser {
        adv.shutdown();
    }

    Ok(())
}
