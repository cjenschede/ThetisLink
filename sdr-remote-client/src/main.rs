// SPDX-License-Identifier: GPL-2.0-or-later

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod audio;
mod catsync;
mod mdns;
mod midi;
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
        metadata.level() <= log::Level::Info
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
    // Gated by `RUST_LOG` via EnvFilter; default (empty) filters everything out
    // -> zero-cost in prod.
    let _tracing_guard = {
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
                    .with_env_filter(
                        tracing_subscriber::EnvFilter::try_from_default_env()
                            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("")),
                    )
                    .with_writer(non_blocking)
                    .json()
                    .try_init();
                eprintln!("UI log: {}", ui_log_path.display());
                Some(guard)
            }
            Err(_) => None,
        }
    };

    info!("ThetisLink Client v{} starting", sdr_remote_core::version_string());

    let (engine, state_rx, cmd_tx) = ClientEngine::new();

    // Shutdown signal
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Phase C: relay as transport. If the relay-config is complete, we set up a
    // client-relay-monitor with tunnel (role Client); the engine gets the tunnel as
    // ClientTransport::Relay, the UI gets the status-handle. Otherwise: direct-UDP (default).
    let relay_cfg_loaded = ui::config::load_config();
    // Apply the persisted UI language before the egui app starts.
    rust_i18n::set_locale(&relay_cfg_loaded.language);
    let mut _relay_monitor_keepalive: Option<sdr_remote_relay::RelayMonitor> = None;
    let (relay_tunnel, relay_status_handle): (
        Option<sdr_remote_logic::engine::ClientRelayTunnel>,
        Option<sdr_remote_relay::RelayStatusHandle>,
    ) = if sdr_remote_relay::is_configured(
        relay_cfg_loaded.relay_enabled,
        &relay_cfg_loaded.relay_url,
        &relay_cfg_loaded.relay_station,
        &relay_cfg_loaded.relay_token,
    ) {
        let (uplink_tx, uplink_rx) = tokio::sync::mpsc::unbounded_channel();
        let (inbound_tx, inbound_rx) = tokio::sync::mpsc::unbounded_channel();
        // Placeholder server address: display-label, ignored by the Relay-transport.
        let server_placeholder: std::net::SocketAddr = "203.0.113.1:4580".parse().unwrap();
        let relay_cfg = sdr_remote_relay::RelayConfig {
            enabled: true,
            url: relay_cfg_loaded.relay_url.clone(),
            station: relay_cfg_loaded.relay_station.clone(),
            token: relay_cfg_loaded.relay_token.clone(),
            role: sdr_remote_relay::RelayRole::Client,
            instance: relay_cfg_loaded.relay_instance_id.clone(),
            name: relay_cfg_loaded.relay_device_name.clone(),
            udp_port: sdr_remote_relay::relay_udp_port_resolve(relay_cfg_loaded.relay_udp_enabled),
        };
        let tunnel = sdr_remote_relay::RelayTunnel {
            sentinel: server_placeholder,
            inbound_tx,
            uplink_rx,
        };
        let monitor = sdr_remote_relay::RelayMonitor::start_threaded_tunnel(relay_cfg, tunnel);
        let status = monitor.status_handle();
        _relay_monitor_keepalive = Some(monitor);
        // The address is deliberately not written: see the same line on the
        // Android side. The log file it lands in is the one place it would
        // survive in plain text.
        info!("Client transport: relay tunnel (via <relay>)");
        (
            Some(sdr_remote_logic::engine::ClientRelayTunnel {
                uplink_tx,
                inbound_rx,
                server_addr: server_placeholder,
            }),
            Some(status),
        )
    } else {
        (None, None)
    };

    // Start engine in background thread (tokio runtime)
    let network_thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rt.block_on(async {
                if let Err(e) = engine.run(
                    |input, output| {
                        let audio = audio::ClientAudio::new(input, output)?;
                        audio.start()?;
                        Ok(Box::new(audio) as Box<dyn sdr_remote_logic::audio::AudioBackend>)
                    },
                    shutdown_rx,
                    relay_tunnel,
                ).await {
                    log::error!("Engine error: {}", e);
                }
            });
        }));
        if let Err(e) = result {
            log::error!("Network thread PANICKED: {:?}", e);
        }
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
            Ok(Box::new(ui::SdrRemoteApp::new(state_rx, cmd_tx, log_buffer, relay_status_handle)))
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
