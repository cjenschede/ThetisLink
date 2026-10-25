// SPDX-License-Identifier: GPL-2.0-or-later

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod audio;
mod ble;
mod catsync;
mod mdns;
mod midi;
mod paddle;
mod ui;
mod websdr;

// Desktop-UI translations (rust-i18n). Base = English (fallback); the user picks a
// language in Settings (persisted as `language=`), applied via rust_i18n::set_locale.
rust_i18n::i18n!("locales", fallback = "en");

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write as IoWrite;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use log::info;
use tokio::sync::watch;

use sdr_remote_logic::engine::ClientEngine;

/// Max log lines kept in memory
const MAX_LOG_LINES: usize = 500;

/// Shared log buffer for in-app display
pub type LogBuffer = Arc<Mutex<VecDeque<String>>>;

/// Custom logger that writes to a shared ring buffer and log file
struct GuiLogger {
    buffer: LogBuffer,
    file: Option<Mutex<std::fs::File>>,
}

impl log::Log for GuiLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        // The developer mode's one rule (build 295): a developer line only
        // while the mode is on, before the line reaches the screen or the file.
        metadata.level() <= log::Level::Info && sdr_remote_core::dev_mode::record_passes(metadata.target())
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // Wall-clock time first, matching the server log. Without it a client line
        // cannot be placed next to a server line, which is exactly what is needed
        // to tell which hop is holding a frame.
        //
        // Local time, the same as the server writes. It used to be seconds since
        // the epoch modulo a day - which is UTC, and therefore two hours out
        // from the server log all summer. The comment above was already right
        // about why this exists; the code underneath it simply did not do it,
        // and the two logs in a problem report could not be laid side by side.
        // It cost an hour of reading a session as two hours long when it had
        // lasted three minutes (2026-08-15).
        let line = format!(
            "[{} {}] {} - {}",
            chrono::Local::now().format("%H:%M:%S%.3f"),
            record.level(),
            record.target(),
            record.args()
        );
        if let Ok(mut buf) = self.buffer.lock() {
            if buf.len() >= MAX_LOG_LINES {
                buf.pop_front();
            }
            buf.push_back(line.clone());
        }

        if let Some(ref file_mutex) = self.file {
            if let Ok(mut f) = file_mutex.lock() {
                let _ = writeln!(f, "{}", line);
                let _ = f.flush();
            }
        }
    }

    fn flush(&self) {}
}

fn main() -> Result<()> {
    // Instance profile (multi-instance): `--profile <name>` / `-p <name>` runs a
    // SECOND ThetisLink alongside the first on one PC, each with its OWN config
    // file, log files and single-instance identity (config seeded as a copy of the
    // default on first use). No arg = the default profile, byte-for-byte unchanged.
    // Parsed FIRST: the config name, log names and the guard mutex all key off it.
    {
        let args: Vec<String> = std::env::args().collect();
        let mut prof: Option<String> = None;
        let mut positional: Option<String> = None;
        let mut i = 1;
        while i < args.len() {
            let a = &args[i];
            if a == "--profile" || a == "-p" {
                prof = args.get(i + 1).cloned();
                i += 2;
            } else if let Some(v) = a.strip_prefix("--profile=") {
                prof = Some(v.to_string());
                i += 1;
            } else {
                // A bare (non-flag) token is also accepted as the profile name, so
                // `ThetisLink-Client.exe B` works, AND a mistyped flag like
                // `--provile B` still picks up the intended name from the bare `B`
                // instead of silently falling back to the default profile.
                if positional.is_none() && !a.starts_with('-') {
                    positional = Some(a.clone());
                }
                i += 1;
            }
        }
        ui::config::set_profile(prof.or(positional));
    }

    // Single-instance guard: a second ThetisLink client of the SAME profile fights
    // the first over the server connection / audio / spectrum subscription and looks
    // like a broad regression (spectrum/s-meter/Yaesu chaos). Refuse a second one of
    // the same profile. A named mutex: CreateMutexW sets ERROR_ALREADY_EXISTS when
    // one already runs. The mutex name is per profile, so DIFFERENT profiles run side
    // by side; the default profile keeps the original name (unchanged behaviour). The
    // handle is left open (not closed) so the mutex is held until this process exits;
    // no leak in the Rust sense (HANDLE is Copy, closed only via CloseHandle).
    #[cfg(windows)]
    unsafe {
        use windows::core::{w, HSTRING, PCWSTR};
        use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
        use windows::Win32::System::Threading::CreateMutexW;
        use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK};
        let mutex_name = match ui::config::profile() {
            Some(p) => format!("ThetisLink-Client-SingleInstance-{p}"),
            None => "ThetisLink-Client-SingleInstance".to_string(),
        };
        let mutex_name_w = HSTRING::from(mutex_name.as_str());
        if CreateMutexW(None, true, PCWSTR(mutex_name_w.as_ptr())).is_ok()
            && GetLastError() == ERROR_ALREADY_EXISTS
        {
            let msg = match ui::config::profile() {
                Some(p) => format!("ThetisLink (profiel {p}) draait al. Sluit eerst die client."),
                None => "ThetisLink draait al. Sluit eerst de bestaande client.".to_string(),
            };
            let msg_w = HSTRING::from(msg.as_str());
            MessageBoxW(
                None,
                PCWSTR(msg_w.as_ptr()),
                w!("ThetisLink"),
                MB_OK | MB_ICONINFORMATION,
            );
            std::process::exit(0);
        }
    }

    let log_buffer: LogBuffer = Arc::new(Mutex::new(VecDeque::with_capacity(MAX_LOG_LINES)));

    // Crash-safe coverage dump: on panic the UI-coverage matrix is especially valuable
    // (which controls had been rendered up to the crash?). Wrap without throwing away
    // the default hook - we still call it afterwards.
    {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            ui::controls::coverage::dump_if_enabled();
            default_hook(info);
        }));
    }

    // Open log file next to the executable (and in current working directory as fallback)
    let log_file = {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let log_name = ui::config::per_profile_file("thetislink-client", "log");
        let log_path = exe_dir.join(&log_name);
        let cwd_path = std::path::PathBuf::from(&log_name);
        // Try exe dir first, then current working directory
        match OpenOptions::new().create(true).write(true).truncate(true).open(&log_path) {
            Ok(f) => {
                eprintln!("Client log: {}", log_path.display());
                Some(Mutex::new(f))
            }
            Err(_) => match OpenOptions::new().create(true).write(true).truncate(true).open(&cwd_path) {
                Ok(f) => {
                    eprintln!("Client log: {}", cwd_path.display());
                    Some(Mutex::new(f))
                }
                Err(_) => None,
            }
        }
    };

    let logger = GuiLogger { buffer: log_buffer.clone(), file: log_file };
    log::set_boxed_logger(Box::new(logger)).unwrap();
    log::set_max_level(log::LevelFilter::Info);

    // Seed this profile's config from the default (copy) on first use, so
    // `--profile B` starts as a copy of the current settings. No-op for default.
    ui::config::seed_profile_config_if_absent();
    // The developer mode as saved (build 295), before anything is logged that
    // it holds back.
    sdr_remote_core::dev_mode::set(ui::config::load_config().developer_mode);
    if let Some(p) = ui::config::profile() {
        info!("Instance profile: {}", p);
    }

    // Tracing subscriber for UI-observability (controls/events.rs -> TracingSink).
    //
    // Writes to `ui-events.jsonl` next to the exe (NOT to stderr):
    // windows_subsystem = "windows" detaches stderr in GUI-builds; writes to
    // a dead fd hung the UI-thread under spectrum+click load.
    //
    // Non-blocking writer (tracing-appender) puts the I/O on a background
    // thread - UI-thread can NOT block on a write-syscall. Guard must
    // stay in scope until the end of main otherwise events are lost.
    //
    // Gated by `RUST_LOG` via EnvFilter. Without it there is nothing to write,
    // and since build 295 no file is made either (it was an empty file at every
    // start).
    let _tracing_guard = tracing_subscriber::EnvFilter::try_from_default_env().ok().and_then(|filter| {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let ui_log_path = exe_dir.join(ui::config::per_profile_file("ui-events", "jsonl"));
        match std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&ui_log_path)
        {
            Ok(f) => {
                let (non_blocking, guard) = tracing_appender::non_blocking(f);
                let _ = tracing_subscriber::fmt()
                    .with_env_filter(filter)
                    .with_writer(non_blocking)
                    .json()
                    .try_init();
                eprintln!("UI log: {}", ui_log_path.display());
                Some(guard)
            }
            Err(_) => None,
        }
    });

    info!("ThetisLink Client v{} starting", sdr_remote_core::version_string());
    // Named in advance (`logregels-build295.md`): in which mode this log is written.
    info!("{}", sdr_remote_core::dev_mode::state_line(sdr_remote_core::dev_mode::is_on()));

    let (engine, state_rx, cmd_tx) = ClientEngine::new();

    // Shutdown signal
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // The relay, when it is set up: a client relay monitor signs in and hands its
    // grants to the engine's QUIC client (phase 3b), and the UI gets its status.
    // Otherwise QUIC straight to the address typed. Chosen once at start.
    let relay_cfg_loaded = ui::config::load_config();
    // Apply the persisted UI language before the egui app starts.
    rust_i18n::set_locale(&relay_cfg_loaded.language);
    let mut _relay_monitor_keepalive: Option<sdr_remote_relay::RelayMonitor> = None;
    let mut relay_quic_grants = None;
    let mut relay_tcp_line = None;
    // The one rule the connect screen compares its "in use" line against.
    use sdr_remote_logic::measure::Route;
    let asked = Route::asked_for(sdr_remote_relay::is_configured(
        relay_cfg_loaded.relay_enabled,
        &relay_cfg_loaded.relay_url,
        &relay_cfg_loaded.relay_station,
        &relay_cfg_loaded.relay_token,
    ));
    let relay_status_handle: Option<sdr_remote_relay::RelayStatusHandle> = if asked == Route::QuicRelay {
        let relay_cfg = sdr_remote_relay::RelayConfig {
            quic: false,
            enabled: true,
            url: relay_cfg_loaded.relay_url.clone(),
            station: relay_cfg_loaded.relay_station.clone(),
            token: relay_cfg_loaded.relay_token.clone(),
            role: sdr_remote_relay::RelayRole::Client,
            instance: relay_cfg_loaded.relay_instance_id.clone(),
            name: relay_cfg_loaded.relay_device_name.clone(),
            // QUIC through the relay always needs the relay's UDP port.
            udp_port: sdr_remote_relay::relay_udp_port_resolve(true),
        };
        let (grant_tx, grant_rx) = tokio::sync::watch::channel(None);
        // The TCP line for a network that lets no UDP through (build 144).
        let (tcp, tcp_monitor) = sdr_remote_relay::tcp_line();
        let monitor = sdr_remote_relay::RelayMonitor::start_threaded_quic(relay_cfg, grant_tx, Some(tcp_monitor));
        let status = monitor.status_handle();
        _relay_monitor_keepalive = Some(monitor);
        relay_quic_grants = Some(grant_rx);
        relay_tcp_line = Some(tcp);
        Some(status)
    } else {
        None
    };
    // Which transport is logged by the engine, once, for both front ends.
    let engine = engine.with_quic_relay(relay_quic_grants, relay_tcp_line);

    // The CW sidetone is shared between the paddle reader (in the UI's
    // ownership) and the playback callback (in the engine's audio backend).
    // Made here, before either exists, and handed to both.
    let sidetone = std::sync::Arc::new(sdr_remote_logic::sidetone::Sidetone::new(
        sdr_remote_logic::sidetone::SidetoneSettings::default(),
    ));
    let sidetone_for_audio = sidetone.clone();
    // The engine's very first opening comes before the settings reach it: give
    // it the chosen devices, so the default microphone never opens first - it
    // did, for ~1.3 s, in the field log of 13-10 (build 189).
    let first_devices = std::sync::Mutex::new(Some((relay_cfg_loaded.input_device.clone(), relay_cfg_loaded.output_device.clone())));

    // Start engine in background thread (tokio runtime). Through the reporter, so
    // a panic leaves "no connection" on screen instead of the last state.
    let engine_stop = engine.stop_reporter();
    let network_thread = std::thread::spawn(move || {
        engine_stop.run(|| {
            let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
            // The result goes to the reporter: an error is a stopped engine,
            // like a panic, and it says so on screen.
            rt.block_on(async {
                engine.run(
                    |input, output| {
                        let open = |i: Option<&str>, o: Option<&str>| -> anyhow::Result<Box<dyn sdr_remote_logic::audio::AudioBackend>> {
                            let audio = audio::ClientAudio::new(i, o, sidetone_for_audio.clone())?;
                            audio.start()?;
                            Ok(Box::new(audio) as Box<dyn sdr_remote_logic::audio::AudioBackend>)
                        };
                        let first = first_devices.lock().unwrap().take();
                        audio::first_open(first, input, output, open)
                    },
                    shutdown_rx,
                ).await
            })
        });
    });

    // Run egui on the main thread
    let icon = egui::IconData {
        rgba: include_bytes!(concat!(env!("OUT_DIR"), "/icon_rgba.bin")).to_vec(),
        width: 32,
        height: 32,
    };
    let window_size = ui::load_window_size();
    let window_pos = ui::load_window_pos();
    // Base title, tagged with the profile ("  [B]") for named instances.
    let app_title = ui::config::window_title(&format!("ThetisLink v{}", sdr_remote_core::version_string()));
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size(window_size)
        .with_title(app_title.clone())
        .with_icon(std::sync::Arc::new(icon));
    if let Some(pos) = window_pos {
        // Only restore the saved position when a usable part of the window would land on a
        // currently-connected monitor. A position left on a since-disconnected/rearranged
        // second monitor is otherwise applied verbatim and the main window opens off-screen
        // (invisible, unrecoverable without editing the conf). If it is off all monitors we
        // drop with_position() and let it open on the primary. Pop-outs already do this.
        if ui::main_window_pos_visible(pos, window_size) {
            viewport = viewport.with_position(egui::pos2(pos[0], pos[1]));
        } else {
            log::warn!(
                "main window saved pos ({}, {}) is off all connected monitors - opening on primary",
                pos[0], pos[1]
            );
        }
    }
    let native_options = eframe::NativeOptions {
        viewport,
        // eframe's own window-state-restore would otherwise overwrite our
        // with_inner_size/with_position from the conf -> window geometry was not
        // remembered. We manage the geometry ourselves (load_window_size/pos +
        // save_full_config), so eframe's persist off.
        persist_window: false,
        ..Default::default()
    };

    let _ = eframe::run_native(
        &app_title,
        native_options,
        Box::new(move |_cc| {
            Ok(Box::new(ui::SdrRemoteApp::new(state_rx, cmd_tx, log_buffer, relay_status_handle, sidetone)))
        }),
    );

    // Signal shutdown
    let _ = shutdown_tx.send(true);
    let _ = network_thread.join();

    // Dump coverage-matrix for CI-gate (debug builds or feature ui-coverage).
    // No-op in release without feature. Writes to `target/ui-coverage.json`.
    ui::controls::coverage::dump_if_enabled();

    info!("Client stopped.");
    Ok(())
}
