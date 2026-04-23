// SPDX-License-Identifier: GPL-2.0-or-later

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod audio;
mod catsync;
mod midi;
mod ui;
mod websdr;

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
        let line = format!(
            "[{}] {} — {}",
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
    let log_buffer: LogBuffer = Arc::new(Mutex::new(VecDeque::with_capacity(MAX_LOG_LINES)));

    // Crash-safe coverage dump: bij panic is de UI-coverage matrix juist waardevol
    // (welke controls waren gerendered tot aan crash?). Wrap zonder default-hook
    // weg te gooien — we bellen die daarna alsnog.
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
        let log_path = exe_dir.join("thetislink-client.log");
        let cwd_path = std::path::PathBuf::from("thetislink-client.log");
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

    // Tracing subscriber voor UI-observability (controls/events.rs → TracingSink).
    //
    // Schrijft naar `ui-events.jsonl` naast de exe (NIET naar stderr):
    // windows_subsystem = "windows" detacht stderr in GUI-builds; writes naar
    // een dead fd hingen de UI-thread op onder spectrum+click-belasting.
    //
    // Non-blocking writer (tracing-appender) zet de I/O op een background
    // thread — UI-thread kan NIET blokkeren op een write-syscall. Guard moet
    // in scope blijven tot einde main anders gaan events verloren.
    //
    // Gated door `RUST_LOG` via EnvFilter; default (leeg) filtert alles uit
    // → zero-cost in prod.
    let _tracing_guard = {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let ui_log_path = exe_dir.join("ui-events.jsonl");
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
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(window_size)
            .with_title(format!("ThetisLink v{}", sdr_remote_core::version_string()))
            .with_icon(std::sync::Arc::new(icon)),
        ..Default::default()
    };

    let _ = eframe::run_native(
        &format!("ThetisLink v{}", sdr_remote_core::version_string()),
        native_options,
        Box::new(move |_cc| {
            Ok(Box::new(ui::SdrRemoteApp::new(state_rx, cmd_tx, log_buffer)))
        }),
    );

    // Signal shutdown
    let _ = shutdown_tx.send(true);
    let _ = network_thread.join();

    // Dump coverage-matrix voor CI-gate (debug builds of feature ui-coverage).
    // No-op in release zonder feature. Schrijft naar `target/ui-coverage.json`.
    ui::controls::coverage::dump_if_enabled();

    info!("Client stopped.");
    Ok(())
}
