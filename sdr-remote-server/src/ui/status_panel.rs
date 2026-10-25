// SPDX-License-Identifier: GPL-2.0-or-later

//! Compact server-state panel for the GUI (PATCH-2).
//!
//! Renders six elements in one screenful so the operator can answer
//! support questions ("is the server listening?", "what does it see?")
//! with a single screenshot:
//!
//! 1. Bind address + port
//! 2. TCI connection state + uptime
//! 3. Active clients (IP, RTT, since)
//! 4. Audio routing (per-channel activity + cumulative frame count)
//! 5. Recent connect attempts (ringbuffer N=10, success + failure)
//! 6. External devices (compact dot grid)
//!
//! All reads are non-blocking: SessionManager via `try_lock()` (returns
//! stale snapshot on contention - 1Hz refresh), audio/TCI via lock-free
//! atomics. UI never blocks the network or audio loops.

use std::sync::atomic::Ordering;
use egui::{Color32, RichText};

use crate::audio_stats::StatusPanelShared;
use crate::session::{ClientSnapshot, ConnectAttempt};

/// Snapshot cache: on contention for the SessionManager lock (try_lock
/// fails) we previously rendered a 1-line "(snapshot busy...)" placeholder.
/// That periodically shrank the panel height, causing the surrounding
/// ScrollArea to clamp the scroll position so the user visually saw
/// content jump upward while looking at the expanded MCP2221A section.
/// Instead the cache lets us keep showing the last-successful snapshot -
/// stale text is acceptable; layout jitter is not.
fn clients_cache() -> &'static std::sync::Mutex<Option<Vec<ClientSnapshot>>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Option<Vec<ClientSnapshot>>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(None))
}

fn attempts_cache() -> &'static std::sync::Mutex<Option<Vec<ConnectAttempt>>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Option<Vec<ConnectAttempt>>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(None))
}

/// Per-client bandwidth in kbps, broken out: (audio, spectrum, other, up).
/// Derived from the tracked-socket's cumulative byte counters: previous sample +
/// timestamp kept, rate over the interval (≥250 ms, against noise). Also prunes
/// stale address counters at the same time (each reconnect = new source port).
/// bits/ms == kbps.
type ClientRate = (u32, u32, u32, u32); // (audio, spectrum, other, up)
fn client_bandwidth_rates(
    active: &[std::net::SocketAddr],
) -> std::collections::HashMap<std::net::SocketAddr, ClientRate> {
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::time::Instant;
    struct BwState {
        prev: HashMap<SocketAddr, (u64, u64, u64, u64)>, // (aud, spec, oth, rx) totals
        prev_at: Option<Instant>,
        rates: HashMap<SocketAddr, ClientRate>,
    }
    static S: std::sync::OnceLock<std::sync::Mutex<BwState>> = std::sync::OnceLock::new();
    let m = S.get_or_init(|| {
        std::sync::Mutex::new(BwState { prev: HashMap::new(), prev_at: None, rates: HashMap::new() })
    });
    crate::tracked_socket::bw_retain(active);
    let snap = crate::tracked_socket::bw_snapshot();
    let now = Instant::now();
    let mut st = m.lock().unwrap();
    let due = st.prev_at.map(|t| now.duration_since(t).as_millis() >= 250).unwrap_or(true);
    if due {
        let dt_ms = st.prev_at.map(|t| now.duration_since(t).as_millis() as u64).unwrap_or(0);
        if dt_ms > 0 {
            let mut new_rates = HashMap::new();
            let kbps = |cur: u64, prev: u64| -> u32 {
                (cur.saturating_sub(prev).saturating_mul(8) / dt_ms) as u32
            };
            for s in &snap {
                let (pa, ps, po, pr) =
                    st.prev.get(&s.addr).copied().unwrap_or((s.tx_audio, s.tx_spectrum, s.tx_other, s.rx));
                new_rates.insert(
                    s.addr,
                    (kbps(s.tx_audio, pa), kbps(s.tx_spectrum, ps), kbps(s.tx_other, po), kbps(s.rx, pr)),
                );
            }
            st.rates = new_rates;
        }
        st.prev = snap.iter().map(|s| (s.addr, (s.tx_audio, s.tx_spectrum, s.tx_other, s.rx))).collect();
        st.prev_at = Some(now);
    }
    st.rates.clone()
}

/// Render the Status panel into the provided `ui`. Designed to fit in
/// the same scrollable area as the existing log view; caller is
/// responsible for the surrounding ScrollArea.
pub fn render_status_panel(
    ui: &mut egui::Ui,
    shared: &StatusPanelShared,
    pending_bind_addr: &str,
    amplitec_configured: bool,
    tuner_configured: bool,
    spe_configured: bool,
    rf2k_configured: bool,
) {
    let server_start = shared.server_start;
    // What the server set up at its start; `None` until it has said (build 269).
    let configured = shared.configured.get();

    // ── 1. Bind address ──────────────────────────────────────────────────
    // Reflect the actual bind outcome - a hardcoded placeholder would lie
    // about the listen address when the bind itself failed.
    ui.horizontal(|ui| {
        ui.label(RichText::new("Server:").strong());
        match shared.bind_status.get() {
            Some(crate::audio_stats::BindStatus::Ok { addr }) => {
                ui.colored_label(
                    Color32::from_rgb(50, 200, 50),
                    format!("Listening (QUIC) on {}", addr),
                );
            }
            Some(crate::audio_stats::BindStatus::Failed { addr, error }) => {
                ui.colored_label(
                    Color32::from_rgb(220, 60, 60),
                    format!("Bind failed on {}: {}", addr, error),
                );
            }
            None => {
                ui.colored_label(
                    Color32::from_rgb(200, 160, 40),
                    format!("Binding {} ...", pending_bind_addr),
                );
            }
        }
    });

    ui.horizontal(|ui| {
        ui.label(RichText::new("Relay:").strong());
        if let Some(handle) = shared.relay_status.get() {
            let status = handle.snapshot();
            let color = match status.phase {
                sdr_remote_relay::RelayPhase::Authenticated => Color32::from_rgb(50, 200, 50),
                sdr_remote_relay::RelayPhase::Connecting
                | sdr_remote_relay::RelayPhase::WaitingForPeer
                | sdr_remote_relay::RelayPhase::WaitingForConfig => Color32::from_rgb(200, 160, 40),
                sdr_remote_relay::RelayPhase::Error => Color32::from_rgb(220, 60, 60),
                sdr_remote_relay::RelayPhase::Disabled
                | sdr_remote_relay::RelayPhase::Disconnected => Color32::from_rgb(130, 130, 130),
            };
            ui.colored_label(color, status.label()).on_hover_text(status.message);
        } else {
            ui.colored_label(Color32::from_rgb(130, 130, 130), "Disabled");
        }
    });
    // ── 2. TCI connection ────────────────────────────────────────────────
    let (tci_connected, tci_age) = shared.tci.snapshot(server_start);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Thetis:").strong());
        if configured.is_some_and(|c| !c.thetis) {
            // No TCI address: not a fault, so not red (build 269).
            ui.colored_label(Color32::from_rgb(130, 130, 130), "Not configured");
        } else if tci_connected {
            let dur = tci_age
                .map(format_duration_short)
                .unwrap_or_else(|| "-".to_string());
            ui.colored_label(
                Color32::from_rgb(50, 200, 50),
                format!("Connected  ({})", dur),
            );
        } else {
            let suffix = tci_age
                .map(|s| format!("for {}", format_duration_short(s)))
                .unwrap_or_else(|| "since startup".to_string());
            ui.colored_label(
                Color32::from_rgb(200, 60, 60),
                format!("Disconnected  ({})", suffix),
            );
        }
    });

    // ── 3. Active clients (snapshot via SessionManager.try_lock) ─────────
    // On lock contention we take the last cached snapshot so the
    // section height stays stable (see `clients_cache` doc for the reason).
    let clients_snapshot: Option<Vec<ClientSnapshot>> =
        if let Some(session_arc) = shared.session_slot.get() {
            match session_arc.try_lock() {
                Ok(guard) => {
                    let snap = guard.active_clients_snapshot();
                    *clients_cache().lock().unwrap() = Some(snap.clone());
                    Some(snap)
                }
                Err(_) => clients_cache().lock().unwrap().clone(),
            }
        } else {
            None // server not yet ready
        };

    match &clients_snapshot {
        Some(clients) if clients.is_empty() => {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Active clients:").strong());
                ui.label("0");
            });
        }
        Some(clients) => {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Active clients:").strong());
                ui.label(format!("{}", clients.len()));
            });
            let active_addrs: Vec<std::net::SocketAddr> = clients.iter().map(|c| c.addr).collect();
            let bw_rates = client_bandwidth_rates(&active_addrs);
            let normal_color = ui.visuals().text_color();
            let col_widths = [150.0, 56.0, 72.0, 56.0, 48.0, 62.0, 76.0, 76.0, 86.0, 76.0, 70.0, 48.0];
            egui::Grid::new("server_client_stats_grid")
                .num_columns(12)
                .spacing([4.0, 3.0])
                .striped(true)
                .show(ui, |ui| {
                    let headers = [
                        "Client", "Path", "Connected", "RTT", "Loss", "Jitter",
                        "Down", "Audio", "Spectrum", "Other", "Up", "Seen",
                    ];
                    for i in 0..headers.len() {
                        ui.add_sized(
                            [col_widths[i], 16.0],
                            egui::Label::new(RichText::new(headers[i]).strong().size(11.0)),
                        );
                    }
                    ui.end_row();

                    for c in clients {
                        let last_seen_age = c.last_seen.elapsed().as_secs();
                        let connected_for =
                            format_duration_short(c.connected_since.elapsed().as_secs_f32());
                        let (aud, spec, oth, up_kbps) =
                            bw_rates.get(&c.addr).copied().unwrap_or((0, 0, 0, 0));
                        let down_kbps = aud + spec + oth;
                        let color = if !c.authenticated {
                            Color32::from_rgb(200, 160, 40) // authenticating
                        } else if last_seen_age > 5 {
                            Color32::from_rgb(200, 160, 40) // stale
                        } else {
                            normal_color
                        };
                        let cell = |ui: &mut egui::Ui, text: String, width: f32| {
                            ui.add_sized(
                                [width, 16.0],
                                egui::Label::new(RichText::new(text).monospace().size(11.0).color(color)),
                            );
                        };

                        // The address in the row's own colour: the amber
                        // auth/stale cue when that applies. The way is a column
                        // of its own since build 143 - letters, no colour of
                        // its own (the connection names, owner 2026-10-08).
                        cell(ui, c.addr.to_string(), col_widths[0]);
                        cell(ui, c.path.map_or("-", |k| k.short()).to_string(), col_widths[1]);
                        cell(ui, connected_for, col_widths[2]);
                        cell(ui, format!("{} ms", c.rtt_ms), col_widths[3]);
                        cell(ui, format!("{}%", c.loss_percent), col_widths[4]);
                        cell(ui, format!("{} ms", c.jitter_ms), col_widths[5]);
                        cell(ui, format!("{} kbps", down_kbps), col_widths[6]);
                        cell(ui, format!("{} kbps", aud), col_widths[7]);
                        cell(ui, format!("{} kbps", spec), col_widths[8]);
                        cell(ui, format!("{} kbps", oth), col_widths[9]);
                        cell(ui, format!("{} kbps", up_kbps), col_widths[10]);
                        cell(ui, format!("{}s", last_seen_age), col_widths[11]);
                        ui.end_row();
                    }
                });
            // What the letters in the Path column mean.
            ui.label(
                RichText::new("Path: L local · D direct · R through the relay · +R found through the relay · R (TCP) through the relay over TCP")
                    .size(10.0)
                    .weak(),
            );

            // A client that is not this server's build, named. Written as a
            // line under the table rather than one more column because the
            // normal case is that they all agree and there is nothing to say.
            // Nothing is refused over this - it only tells.
            for c in clients.iter().filter(|c| c.client_build.is_trouble()) {
                let theirs = c
                    .client_build
                    .theirs()
                    .unwrap_or("a build too old to say which");
                ui.colored_label(
                    Color32::from_rgb(220, 150, 40),
                    RichText::new(format!(
                        "{} runs {} and this server is {} - they should be the same build.",
                        c.addr,
                        theirs,
                        sdr_remote_core::version_string()
                    ))
                    .size(10.0),
                );
            }
        }
        None => {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Active clients:").strong());
                ui.colored_label(
                    Color32::from_rgb(160, 160, 160),
                    "(snapshot busy - updating...)",
                );
            });
        }
    }

    // ── 4. Audio routing ─────────────────────────────────────────────────
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Audio:").strong());
        for chip in chips(configured) {
            let (rx, tx) = chip.stats(&shared.audio);
            let tx_age = tx.and_then(|t| t.snapshot(server_start).1);
            match tx_shown(chip.says_transmitting(shared), tx_age) {
                TxShown::Not => render_channel_chip(ui, chip.label(false), rx, server_start),
                shown => render_tx_chip(ui, chip.label(true), shown),
            }
        }
    });

    // ── 5. Recent connect attempts ───────────────────────────────────────
    // Same: on lock contention fall back to the last cached snapshot
    // so the section height stays stable.
    let attempts: Option<Vec<ConnectAttempt>> =
        if let Some(session_arc) = shared.session_slot.get() {
            match session_arc.try_lock() {
                Ok(guard) => {
                    let snap = guard.recent_connect_attempts();
                    *attempts_cache().lock().unwrap() = Some(snap.clone());
                    Some(snap)
                }
                Err(_) => attempts_cache().lock().unwrap().clone(),
            }
        } else {
            None
        };
    sdr_remote_theme::rule(ui);
    ui.label(RichText::new("Recent connect attempts:").strong());
    match &attempts {
        Some(list) if list.is_empty() => {
            ui.colored_label(
                Color32::from_rgb(160, 160, 160),
                "  (none yet)",
            );
        }
        Some(list) => {
            // Newest first for readability.
            for a in list.iter().rev() {
                let time_str = a.wall_clock.format("%H:%M:%S").to_string();
                let line = format!(
                    "  {}   {:<22}   {}",
                    time_str,
                    a.remote_addr.to_string(),
                    a.outcome.label()
                );
                let color = match a.outcome {
                    crate::session::ConnectOutcome::Accepted => {
                        Color32::from_rgb(50, 200, 50)
                    }
                    crate::session::ConnectOutcome::TotpRequired
                    | crate::session::ConnectOutcome::ChallengeSent => {
                        ui.visuals().text_color()
                    }
                    crate::session::ConnectOutcome::WrongPassword
                    | crate::session::ConnectOutcome::WrongTotp
                    | crate::session::ConnectOutcome::ProtocolVersionMismatch { .. } => {
                        Color32::from_rgb(220, 60, 60)
                    }
                };
                ui.colored_label(color, RichText::new(line).monospace());
            }
        }
        None => {
            ui.colored_label(
                Color32::from_rgb(160, 160, 160),
                "  (snapshot busy - updating...)",
            );
        }
    }

    // ── 6. Configured external devices (compact dot grid) ────────────────
    // Dots reflect Some/None at server-start, NOT live link health - true
    // online/offline detection per device is out of scope for PATCH-2.
    // Labelled "Configured devices" so the screenshot reader is not
    // misled into thinking a green dot means the radio replied.
    sdr_remote_theme::rule(ui);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Configured devices:").strong());
        let radios = configured.map_or([false, false], |c| c.radios);
        render_device_dot(ui, "Yaesu 1", radios[0]);
        render_device_dot(ui, "Yaesu 2", radios[1]);
        render_device_dot(ui, "Amplitec", amplitec_configured);
        render_device_dot(ui, "Tuner", tuner_configured);
        render_device_dot(ui, "SPE", spe_configured);
        render_device_dot(ui, "RF2K", rf2k_configured);
    });
    // Why a radio's port does not open, while the server runs (build 270):
    // above all another program holding it. The settings could not say it.
    // A radio that also keys on a line of its USB port (build 296). And the
    // lead in its sound card, in the developer mode only (builds 278, 296).
    for (i, radio) in shared.radios.get().into_iter().flatten().enumerate() {
        render_port_trouble(ui, i, radio.as_deref());
        render_line_keyed(ui, i, radio.as_deref());
        if sdr_remote_core::dev_mode::is_on() {
            render_lead_row(ui, i, radio.as_deref());
        }
    }

    // ── 7. MCP2221A tuner bridges - collapsible so the operator can hide the
    // per-tuner threshold sliders and board-scan tools during normal use.
    // Open/closed state is persisted in the config so it survives a restart.
    sdr_remote_theme::rule(ui);
    let current_expanded = crate::config::load().mcp2221_section_expanded;
    if super::chevron_label(
        ui,
        current_expanded,
        RichText::new("MCP2221A tuner bridges").strong(),
    )
    .clicked()
    {
        crate::config::save_mcp2221_section_expanded(!current_expanded);
    }
    if current_expanded {
        ui.indent("mcp2221_section", |ui| {
            // 7a - per-tuner rows. The number of slots equals the number of
            // entries in `config.tuners` (1..=`MAX_TUNERS`). Empty Vec =
            // no tuner rows; phase 3 adds the "Add tuner" wizard
            // that creates new entries based on a board scan.
            // For a slot that is in config but has no running
            // TunerInstance yet (e.g. disabled or startup failure)
            // we show the lighter "disabled config row" with the same
            // MCP serial / Amplitec-pos dropdowns.
            let active = shared.tuners_slot.get();
            let slot_count = crate::config::load().tuners.len();
            for slot in 0..slot_count {
                let inst = active.and_then(|t| {
                    t.instances().iter().find(|i| i.slot_index() == slot)
                });
                match inst {
                    Some(inst) => render_tuner_detector_row(ui, inst),
                    None => render_tuner_disabled_row(ui, slot),
                }
            }
            // 7b - Yaesu rotor (PATCH-yaesu-rotor-mcp2221 phase 3): live
            // ADC position + manual CW/CCW/Stop + DAC speed slider for
            // hardware verification. Only visible once a rot_*
            // instance has started.
            if let Some(rotor) = shared.rotor_slot.get() {
                sdr_remote_theme::rule(ui);
                render_rotor_row(ui, rotor);
            } else {
                // Rotor (MCP2221A) not bound. Show a link row so an
                // already-programmed rot_ board from a clean .conf can still be
                // linked to config.rotors - this link screen was missing
                // compared to the tuners (render_tuner_disabled_row).
                let backend_is_mcp =
                    crate::config::load().rotor_backend == "mcp2221_yaesu";
                let rot_board_detected = cached_scan_serials().iter().any(|s| {
                    s.starts_with(crate::mcp2221_scan::BoardKind::ROTOR_PREFIX)
                });
                if backend_is_mcp || rot_board_detected {
                    sdr_remote_theme::rule(ui);
                    render_rotor_disabled_row(ui);
                }
            }
            // 7c - board scan: list all MCP2221A devices on the USB bus so
            // the operator can figure out which USB serial belongs to which
            // physical tuner. Result cached statically until the next Scan
            // click so we don't hammer the HID enumerator on every repaint.
            sdr_remote_theme::rule(ui);
            render_board_scan_section(ui);
        });
    }
}

fn render_tuner_detector_row(
    ui: &mut egui::Ui,
    inst: &std::sync::Arc<crate::tuner::TunerInstance>,
) {
    let bridge = inst.bridge();
    let snap = bridge.snapshot();
    let slot_idx = inst.slot_index();
    sdr_remote_theme::group(ui).show(ui, |ui| {
        // Header: tuner label + connection status + delete button on the right
        ui.horizontal(|ui| {
            ui.label(RichText::new(inst.label()).strong());
            match &snap.status {
                crate::mcp2221_debug::Status::Connected => {
                    ui.colored_label(Color32::from_rgb(50, 200, 50), "Connected");
                }
                crate::mcp2221_debug::Status::NotInitialized => {
                    ui.colored_label(Color32::from_rgb(160, 160, 160), "Not connected");
                }
                crate::mcp2221_debug::Status::Error(e) => {
                    ui.colored_label(
                        Color32::from_rgb(220, 60, 60),
                        format!("Error: {}", e),
                    );
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if super::delete_button(ui)
                    .on_hover_text(rust_i18n::t!("srv_remove_tuner_slot").to_string())
                    .clicked()
                {
                    remove_tuner_slot(slot_idx);
                }
            });
        });
        // MCP serial selector - pick which physical Adafruit board (by USB
        // serial name, as the operator programmed it) is assigned to this tuner
        // slot. Options come from the latest scan cache; the current value
        // is always present even if the board is currently unplugged.
        // Changing the selection writes the new serial to config and
        // auto-restarts so the bridge re-opens on the chosen board.
        ui.horizontal(|ui| {
            ui.label("MCP serial:");
            let slot = inst.slot_index();
            let current_serial: String = crate::config::load()
                .tuners
                .get(slot)
                .map(|c| c.mcp_serial.clone())
                .unwrap_or_default();
            let mut chosen = current_serial.clone();
            let scan_results = cached_scan_serials();
            egui::ComboBox::from_id_salt(format!("tuner_mcp_serial_{}", slot))
                .selected_text(if chosen.is_empty() {
                    "(first available)".to_string()
                } else {
                    chosen.clone()
                })
                .show_ui(ui, |ui| {
                    // Build a deduplicated list: detected boards' serials
                    // PLUS the currently-configured one (so an unplugged
                    // assignment is still selectable / visible).
                    let mut options: Vec<String> = scan_results
                        .iter()
                        .filter(|s| !s.is_empty())
                        .cloned()
                        .collect();
                    if !current_serial.is_empty()
                        && !options.iter().any(|s| s == &current_serial)
                    {
                        options.push(current_serial.clone());
                    }
                    options.sort();
                    options.dedup();
                    for opt in &options {
                        ui.selectable_value(&mut chosen, opt.clone(), opt);
                    }
                });
            if chosen != current_serial {
                let chosen_for_log = chosen.clone();
                let mut applied = false;
                crate::config::modify_config(|config| {
                    if slot < config.tuners.len() {
                        config.tuners[slot].mcp_serial = chosen.clone();
                        config.tuners[slot].model = infer_model_from_serial(&chosen);
                        config.tuners[slot].enabled = true;
                        applied = true;
                    }
                });
                if applied {
                    log::info!(
                        "Tuner {} MCP serial changed to \"{}\" - auto-restart",
                        slot + 1,
                        chosen_for_log,
                    );
                    restart_server();
                }
            }
        });
        // Amplitec-A position picker - drives the Tune-button routing in
        // network.rs. Options come from the live config so the labels stay
        // in sync with whatever the operator has set per antenna position.
        ui.horizontal(|ui| {
            ui.label("Amplitec pos:");
            let slot = inst.slot_index();
            let live_config = crate::config::load();
            let current_pos: Option<u8> = live_config
                .tuners
                .get(slot)
                .and_then(|c| c.amplitec_pos);
            let labels = live_config.amplitec_labels.clone();
            let mut chosen: Option<u8> = current_pos;
            let selected_text = match current_pos {
                None => "(none)".to_string(),
                Some(p) if (1..=6).contains(&p) => {
                    let lbl = &labels[(p - 1) as usize];
                    if lbl.is_empty() {
                        format!("{}", p)
                    } else {
                        format!("{}: {}", p, lbl)
                    }
                }
                Some(p) => format!("{} (out of range)", p),
            };
            egui::ComboBox::from_id_salt(format!("tuner_amplitec_pos_{}", slot))
                .selected_text(selected_text)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut chosen, None, "(none)");
                    for p in 1u8..=6 {
                        let lbl = &labels[(p - 1) as usize];
                        let label_text = if lbl.is_empty() {
                            format!("{}", p)
                        } else {
                            format!("{}: {}", p, lbl)
                        };
                        ui.selectable_value(&mut chosen, Some(p), label_text);
                    }
                });
            if chosen != current_pos {
                let mut applied = false;
                crate::config::modify_config(|config| {
                    if slot < config.tuners.len() {
                        config.tuners[slot].amplitec_pos = chosen;
                        applied = true;
                    }
                });
                if applied {
                    log::info!(
                        "Tuner {} amplitec_pos changed to {:?} - auto-restart",
                        slot + 1,
                        chosen,
                    );
                    restart_server();
                }
            }
        });
        // Live yellow-wire voltage - single value, since raw and pre-divider
        // pin voltage add nothing for the operator who reads the schema in
        // post-divider volts.
        ui.horizontal(|ui| {
            ui.label("Live:");
            match snap.last_yellow_v {
                Some(yellow_v) => {
                    ui.colored_label(
                        ui.visuals().text_color(),
                        RichText::new(format!("yellow {:.2} V", yellow_v)).monospace(),
                    );
                }
                None => {
                    ui.colored_label(Color32::from_rgb(160, 160, 160), "(no sample)");
                }
            }
        });
        // Threshold slider - switch level on the yellow wire (V). Changes
        // apply to the live bridge AND persist to config so they survive a
        // restart.
        ui.horizontal(|ui| {
            let slot = inst.slot_index();
            ui.label("Threshold:");
            let mut v = snap.threshold_v;
            if ui
                .add(egui::Slider::new(&mut v, 0.5..=4.5).suffix(" V"))
                .changed()
            {
                bridge.set_threshold_v(v);
                persist_threshold_v(slot, v);
            }
        });
        // Hysteresis slider - width of the dead-band around the threshold (V).
        ui.horizontal(|ui| {
            let slot = inst.slot_index();
            ui.label("Hysteresis:");
            let mut v = snap.hysteresis_v;
            if ui
                .add(egui::Slider::new(&mut v, 0.1..=2.0).suffix(" V"))
                .changed()
            {
                bridge.set_hysteresis_v(v);
                persist_hysteresis_v(slot, v);
            }
        });
        // Computed edges, in V - what the bridge will compare each sample to.
        // When threshold ± hyst/2 falls outside the physically-reachable
        // yellow range (0..ADC_VREF*divider), the edges are clamped and an
        // amber warning is shown so the operator sees the slider combo is
        // degenerate (would never actually trigger a tune on hardware).
        ui.horizontal(|ui| {
            ui.label("Edges:");
            let edges_text = format!(
                "active < {:.2} V   idle > {:.2} V",
                snap.threshold_active_v, snap.threshold_idle_v
            );
            if snap.edges_clamped {
                ui.colored_label(
                    Color32::from_rgb(220, 160, 40),
                    RichText::new(format!("{}   ! clamped", edges_text)).monospace(),
                )
                .on_hover_text(
                    "Threshold + hysteresis falls outside the reachable yellow range. \
                     Lower the hysteresis or move the threshold further from the boundary.",
                );
            } else {
                ui.colored_label(
                    ui.visuals().text_color(),
                    RichText::new(edges_text).monospace(),
                );
            }
        });
    });
    // Keep the Live voltage ticking ~10×/s even when the user is not
    // interacting. snapshot() rate-limits the actual USB poll to 100 ms so
    // this is cheap.
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
}

/// Light config row for a tuner slot that is not yet enabled. Shows
/// only the MCP-serial and Amplitec-pos dropdowns; the voltage/threshold
/// sliders are omitted because there is no active bridge to read
/// from or configure. Once the operator selects an MCP serial
/// the tuner is set enabled and the server is restarted (same
/// auto-restart path as the full row).
fn render_tuner_disabled_row(ui: &mut egui::Ui, slot: usize) {
    sdr_remote_theme::group(ui).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Tuner {}", slot + 1)).strong());
            ui.colored_label(
                Color32::from_rgb(160, 160, 160),
                "Disabled \u{2014} select an MCP serial to enable",
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if super::delete_button(ui)
                    .on_hover_text(rust_i18n::t!("srv_remove_tuner_slot").to_string())
                    .clicked()
                {
                    remove_tuner_slot(slot);
                }
            });
        });
        // MCP serial dropdown - selecting enables the slot + auto-restart.
        ui.horizontal(|ui| {
            ui.label("MCP serial:");
            let current_serial: String = crate::config::load()
                .tuners
                .get(slot)
                .map(|c| c.mcp_serial.clone())
                .unwrap_or_default();
            let mut chosen = current_serial.clone();
            let scan_results = cached_scan_serials();
            egui::ComboBox::from_id_salt(format!("tuner_mcp_serial_disabled_{}", slot))
                .selected_text(if chosen.is_empty() {
                    "(select board)".to_string()
                } else {
                    chosen.clone()
                })
                .show_ui(ui, |ui| {
                    let mut options: Vec<String> = scan_results
                        .iter()
                        .filter(|s| !s.is_empty())
                        .cloned()
                        .collect();
                    if !current_serial.is_empty()
                        && !options.iter().any(|s| s == &current_serial)
                    {
                        options.push(current_serial.clone());
                    }
                    options.sort();
                    options.dedup();
                    if options.is_empty() {
                        ui.colored_label(
                            Color32::from_rgb(180, 180, 180),
                            "(no boards detected \u{2014} run Scan below)",
                        );
                    } else {
                        for opt in &options {
                            ui.selectable_value(&mut chosen, opt.clone(), opt);
                        }
                    }
                });
            if chosen != current_serial && !chosen.is_empty() {
                let chosen_for_log = chosen.clone();
                let mut applied = false;
                crate::config::modify_config(|config| {
                    if slot < config.tuners.len() {
                        config.tuners[slot].mcp_serial = chosen.clone();
                        config.tuners[slot].model = infer_model_from_serial(&chosen);
                        config.tuners[slot].enabled = true;
                        applied = true;
                    }
                });
                if applied {
                    log::info!(
                        "Tuner {} MCP serial set to \"{}\" \u{2014} auto-restart",
                        slot + 1,
                        chosen_for_log,
                    );
                    restart_server();
                }
            }
        });
        // Amplitec pos dropdown - can already be set before the slot is
        // enabled, but yields little without an active tuner; we show it
        // anyway so the operator can set everything in one field session.
        ui.horizontal(|ui| {
            ui.label("Amplitec pos:");
            let live_config = crate::config::load();
            let current_pos: Option<u8> = live_config
                .tuners
                .get(slot)
                .and_then(|c| c.amplitec_pos);
            let labels = live_config.amplitec_labels.clone();
            let mut chosen: Option<u8> = current_pos;
            let selected_text = match current_pos {
                None => "(none)".to_string(),
                Some(p) if (1..=6).contains(&p) => {
                    let lbl = &labels[(p - 1) as usize];
                    if lbl.is_empty() {
                        format!("{}", p)
                    } else {
                        format!("{}: {}", p, lbl)
                    }
                }
                Some(p) => format!("{} (out of range)", p),
            };
            egui::ComboBox::from_id_salt(format!("tuner_amplitec_pos_disabled_{}", slot))
                .selected_text(selected_text)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut chosen, None, "(none)");
                    for p in 1u8..=6 {
                        let lbl = &labels[(p - 1) as usize];
                        let label_text = if lbl.is_empty() {
                            format!("{}", p)
                        } else {
                            format!("{}: {}", p, lbl)
                        };
                        ui.selectable_value(&mut chosen, Some(p), label_text);
                    }
                });
            if chosen != current_pos {
                let mut applied = false;
                crate::config::modify_config(|config| {
                    if slot < config.tuners.len() {
                        config.tuners[slot].amplitec_pos = chosen;
                        applied = true;
                    }
                });
                if applied {
                    log::info!(
                        "Tuner {} amplitec_pos set to {:?} \u{2014} auto-restart",
                        slot + 1,
                        chosen,
                    );
                    restart_server();
                }
            }
        });
    });
}

/// Per-board scratch state for the "Program serial" text input.
/// Keyed by USB HID path so two anonymous boards do not collide.
fn board_serial_edit_state(path: &str) -> std::sync::Arc<std::sync::Mutex<String>> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    static MAP: OnceLock<Mutex<HashMap<String, Arc<Mutex<String>>>>> = OnceLock::new();
    let map = MAP.get_or_init(|| Mutex::new(HashMap::new()));
    let mut m = map.lock().unwrap();
    m.entry(path.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(String::new())))
        .clone()
}

/// Per-board scratch state for the "Add new board" wizard. Holds the
/// chosen function (Tuner/Rotor) for an unprogrammed board.
/// Default = Tuner so the operator can often click straight through.
fn board_function_state(
    path: &str,
) -> std::sync::Arc<std::sync::Mutex<crate::mcp2221_scan::BoardKind>> {
    use crate::mcp2221_scan::BoardKind;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    static MAP: OnceLock<Mutex<HashMap<String, Arc<Mutex<BoardKind>>>>> = OnceLock::new();
    let map = MAP.get_or_init(|| Mutex::new(HashMap::new()));
    let mut m = map.lock().unwrap();
    m.entry(path.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(BoardKind::Tuner)))
        .clone()
}

/// Per-board result of the most recent `program_serial_at_path` call.
fn board_program_result_state(
    path: &str,
) -> std::sync::Arc<std::sync::Mutex<Option<Result<(), String>>>> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    static MAP: OnceLock<Mutex<HashMap<String, Arc<Mutex<Option<Result<(), String>>>>>>> =
        OnceLock::new();
    let map = MAP.get_or_init(|| Mutex::new(HashMap::new()));
    let mut m = map.lock().unwrap();
    m.entry(path.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(None)))
        .clone()
}

/// Persist a tuner slot's switch threshold (V). Saves silently (no
/// auto-restart): `set_threshold_v` already updates the live bridge so the
/// new value is effective immediately. Uses `modify_config` so the load /
/// mutate / save sequence is atomic under `CONFIG_LOCK` - preventing the
/// build-58 RMW race from being reintroduced via the UI slider.
fn persist_threshold_v(slot: usize, v: f32) {
    crate::config::modify_config(|config| {
        if slot < config.tuners.len() {
            config.tuners[slot].threshold_v = v;
        }
    });
}

/// Persist a tuner slot's hysteresis (V). Same locking story as
/// `persist_threshold_v`.
fn persist_hysteresis_v(slot: usize, v: f32) {
    crate::config::modify_config(|config| {
        if slot < config.tuners.len() {
            config.tuners[slot].hysteresis_v = v;
        }
    });
}

/// Heuristic: pick the cosmetic [`TunerModel`] from a freshly-programmed USB
/// serial name. Anything containing `3s` (case-insensitive) maps to JC-3s,
/// everything else defaults to JC-4s. Owners who use entirely different
/// naming conventions just override via the bridge-row model dropdown.
fn infer_model_from_serial(serial: &str) -> crate::config::TunerModel {
    if serial.to_lowercase().contains("3s") {
        crate::config::TunerModel::Jc3s
    } else {
        crate::config::TunerModel::Jc4s
    }
}

/// Remove the TunerConfig at slot index `slot` from `config.tuners` and
/// trigger auto-restart so the runtime no longer tries to reach the
/// removed TunerInstance. The operator uses this to take a tuner slot
/// out of the config for good (e.g. physical board absent or
/// replaced via a rename).
fn remove_tuner_slot(slot: usize) {
    let mut removed_label: Option<String> = None;
    crate::config::modify_config(|c| {
        if slot < c.tuners.len() {
            let removed = c.tuners.remove(slot);
            removed_label = Some(format!(
                "Tuner {} (\"{}\")",
                slot + 1,
                removed.mcp_serial
            ));
        }
    });
    if let Some(label) = removed_label {
        log::info!("Remove tuner slot: {} - auto-restart", label);
        restart_server();
    } else {
        log::warn!("Remove tuner slot: index {} out of range (no-op)", slot);
    }
}

/// Self-restart the server. Requests an auto-restart via the global
/// `request_auto_restart()` flag in `ui/utils.rs`. The actual
/// cleanup + child-spawn + `process::exit(0)` runs in `ServerApp::
/// update()` as soon as it detects the flag; there the Drop handlers
/// run correctly (closing audio cpal-streams + TCI-connect) before
/// the new child tries to claim the devices.
fn restart_server() {
    super::request_auto_restart();
}

/// Shared scan cache used by both the "Detected boards" section and the
/// per-tuner MCP-serial dropdowns. Populated by the Scan button.
fn scan_cache(
) -> &'static std::sync::Mutex<Option<Result<Vec<crate::mcp2221_scan::BoardInfo>, String>>> {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<
        Mutex<Option<Result<Vec<crate::mcp2221_scan::BoardInfo>, String>>>,
    > = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Snapshot of every non-empty serial currently in the scan cache. Used by
/// the per-tuner serial dropdown so the operator can pick from physically
/// present boards. Returns an empty Vec when the operator hasn't clicked Scan
/// yet - the dropdown then only shows the currently-configured serial.
fn cached_scan_serials() -> Vec<String> {
    let cache = scan_cache().lock().unwrap();
    match cache.as_ref() {
        Some(Ok(list)) => list
            .iter()
            .map(|b| b.serial_number.clone())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// PATCH-yaesu-rotor-mcp2221 phase 3 - Yaesu G-1000DXC rotor live status.
/// Shows ADC position (raw counts + converted Yaesu-pin voltage),
/// CW/CCW/Stop buttons, DAC speed slider. Intended for hardware
/// verification and later calibration (phase 4). No local state - all
/// commands go straight to the driver, snapshot reads live state.
fn render_rotor_row(
    ui: &mut egui::Ui,
    rotor: &std::sync::Arc<crate::mcp2221_yaesu_rotor::RotorInstance>,
) {
    use crate::mcp2221_yaesu_rotor::{Status as RotorStatus, DAC_MAX};
    let snap = rotor.status();
    sdr_remote_theme::group(ui).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Yaesu rotor - {}", snap.label)).strong());
            match &snap.status {
                RotorStatus::Connected => {
                    ui.colored_label(Color32::from_rgb(50, 200, 50), "Connected");
                }
                RotorStatus::NotInitialized => {
                    ui.colored_label(Color32::from_rgb(160, 160, 160), "Not connected");
                }
                RotorStatus::Error(e) => {
                    ui.colored_label(Color32::from_rgb(220, 90, 90), "Error")
                        .on_hover_text(e);
                }
            }
        });

        // Primary position display: degrees (after calibration) as the main value,
        // pin-4 median voltage as secondary for diagnosis.
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("srv_position").to_string());
            match (snap.position_deg, snap.median_yaesu_volts) {
                (Some(deg), Some(v)) => {
                    ui.label(
                        RichText::new(format!("{:>3}°", deg.round() as i32))
                            .strong()
                            .size(16.0),
                    );
                    sdr_remote_theme::rule(ui);
                    ui.label(format!("≈ {:.3} V (median)", v));
                }
                (None, Some(v)) => {
                    ui.label(
                        RichText::new(rust_i18n::t!("srv_not_calibrated").to_string())
                            .weak(),
                    );
                    sdr_remote_theme::rule(ui);
                    ui.label(format!("≈ {:.3} V (median)", v));
                }
                _ => {
                    ui.label(RichText::new("- geen sample -").weak());
                }
            }
        });
        // Spread + last raw sample for noise diagnosis.
        if let Some(raw) = snap.last_adc_raw {
            ui.horizontal(|ui| {
                ui.label(RichText::new("  laatste raw:").weak());
                ui.label(RichText::new(format!("{}", raw)).weak());
                if let Some(p2p) = snap.adc_p2p_raw {
                    sdr_remote_theme::rule(ui);
                    ui.label(RichText::new(format!("spread d={} raw", p2p)).weak());
                    // Converted to Yaesu pin voltage for readable diagnosis.
                    // 1.8 k + 10 k voltage-divider correction matches the operator's hardware.
                    let p2p_v = (p2p as f32) * 4.096 / 1023.0 * (11_800.0 / 10_000.0);
                    ui.label(RichText::new(format!("≈ {:.3} V p2p", p2p_v)).weak());
                }
            });
        }

        // Direction buttons for manual test.
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("srv_test_direction").to_string());
            let cw_fill = if snap.gp0_cw_high {
                Some(Color32::from_rgb(100, 160, 230))
            } else {
                None
            };
            let cw_btn = match cw_fill {
                Some(c) => egui::Button::new("CW (R)").fill(c),
                None => egui::Button::new("CW (R)"),
            };
            if ui.add(cw_btn).clicked() {
                rotor.set_direction(!snap.gp0_cw_high, false);
            }

            let ccw_fill = if snap.gp1_ccw_high {
                Some(Color32::from_rgb(100, 160, 230))
            } else {
                None
            };
            let ccw_btn = match ccw_fill {
                Some(c) => egui::Button::new("CCW (L)").fill(c),
                None => egui::Button::new("CCW (L)"),
            };
            if ui.add(ccw_btn).clicked() {
                rotor.set_direction(false, !snap.gp1_ccw_high);
            }

            if ui.button(rust_i18n::t!("srv_stop").to_string()).clicked() {
                rotor.set_direction(false, false);
            }
        });

        // DAC speed-slider (5-bit, 0..31).
        ui.horizontal(|ui| {
            ui.label("Speed (DAC):");
            let mut dac = snap.dac_value as i32;
            let label = format!(
                "/ {} (≈{:.2} V)",
                DAC_MAX,
                snap.dac_value as f32 / DAC_MAX as f32 * 5.0
            );
            if ui
                .add(egui::Slider::new(&mut dac, 0..=(DAC_MAX as i32)).text(label))
                .changed()
            {
                rotor.set_dac(dac.clamp(0, DAC_MAX as i32) as u8);
            }
        });

        // Calibration (PATCH-yaesu-rotor-mcp2221 phase 4): the operator turns
        // the rotor to the CCW end stop, clicks "Park CCW (0°)" to
        // capture the current median voltage; then to the CW
        // end stop and "Park CW". The max_deg spinner sets the fullscale
        // (default 450° for G-1000DXC). All values persist to
        // `config.rotors[N]` so the mapping survives a restart.
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("srv_calibration").to_string());
            if ui
                .button("Park CCW (0°)")
                .on_hover_text(format!(
                    "Sla huidige mediaan vast als 0°. Nu: {:.3} V",
                    snap.calibration.v_at_0deg
                ))
                .clicked()
            {
                rotor.park_ccw();
            }
            ui.label(RichText::new(format!("{:.3} V", snap.calibration.v_at_0deg)).weak());
            if ui
                .button(format!("Park CW ({}°)", snap.calibration.max_deg))
                .on_hover_text(format!(
                    "Sla huidige mediaan vast als max°. Nu: {:.3} V",
                    snap.calibration.v_at_max_deg
                ))
                .clicked()
            {
                rotor.park_cw();
            }
            ui.label(RichText::new(format!("{:.3} V", snap.calibration.v_at_max_deg)).weak());
            sdr_remote_theme::rule(ui);
            ui.label("max:");
            let mut max_deg = snap.calibration.max_deg;
            if ui
                .add(egui::DragValue::new(&mut max_deg).range(90..=720).suffix("°"))
                .changed()
            {
                rotor.set_max_deg(max_deg);
            }
            sdr_remote_theme::rule(ui);
            // Ramp-rate slider: how fast the DAC ramps from 0 -> max (and
            // back) during a GoTo or start/stop. Low value =
            // slow, antenna-friendly acceleration (heavy
            // mast/large antenna); high value = quickly responsive.
            ui.label("ramp:")
                .on_hover_text(rust_i18n::t!("srv_ramp_hover").to_string());
            let mut ramp = snap.calibration.ramp_pct_per_sec;
            if ui
                .add(
                    egui::DragValue::new(&mut ramp)
                        .range(1.0..=200.0)
                        .speed(1.0)
                        .suffix(" %/s"),
                )
                .changed()
            {
                rotor.set_ramp_pct_per_sec(ramp);
            }
            // Shortest-route option only shown for rotors with an
            // overlap zone (max_deg > 360); for standard 360°
            // rotors the choice is meaningless.
            if snap.calibration.max_deg > 360 {
                sdr_remote_theme::rule(ui);
                let mut shortest = snap.calibration.shortest_route_in_overlap;
                if ui
                    .checkbox(&mut shortest, "shortest route")
                    .on_hover_text(
                        "Kies bij GoTo de kortste mechanische route via de overlap-zone.\n\
                         Bv. huidig 350°, target 30° -> CW via 390° i.p.v. CCW via 0°.",
                    )
                    .changed()
                {
                    rotor.set_shortest_route_in_overlap(shortest);
                }
            }
        });
    });
}

/// Rotor not bound while the MCP2221A backend is chosen (or a
/// rot_ board is on the bus): let the operator link a detected rot_ board
/// to `config.rotors[0]`. Analogous to [`render_tuner_disabled_row`].
/// This fixes the gap that an already-programmed rot_ board from a clean
/// `.conf` could not be linked via the GUI (for tuners that is possible via the
/// MCP-serial dropdown; for the rotor that screen was missing). Linking sets
/// `enabled`, the serial and the rotor backend correctly and restarts the server.
fn render_rotor_disabled_row(ui: &mut egui::Ui) {
    use crate::mcp2221_scan::BoardKind;
    sdr_remote_theme::group(ui).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Yaesu rotor").strong());
            ui.colored_label(
                Color32::from_rgb(220, 160, 40),
                "Not linked \u{2014} select a rotor board to enable",
            );
        });
        ui.horizontal(|ui| {
            ui.label("MCP serial:");
            let current_serial: String = crate::config::load()
                .rotors
                .first()
                .map(|r| r.mcp_serial.clone())
                .unwrap_or_default();
            let mut chosen = current_serial.clone();
            let rot_boards: Vec<String> = cached_scan_serials()
                .into_iter()
                .filter(|s| s.starts_with(BoardKind::ROTOR_PREFIX))
                .collect();
            egui::ComboBox::from_id_salt("rotor_mcp_serial_disabled")
                .selected_text(if chosen.is_empty() {
                    "(select rot_ board)".to_string()
                } else {
                    chosen.clone()
                })
                .show_ui(ui, |ui| {
                    let mut options = rot_boards.clone();
                    if !current_serial.is_empty()
                        && !options.iter().any(|s| s == &current_serial)
                    {
                        options.push(current_serial.clone());
                    }
                    options.sort();
                    options.dedup();
                    if options.is_empty() {
                        ui.colored_label(
                            Color32::from_rgb(180, 180, 180),
                            "(no rot_ board detected \u{2014} run Scan, or program one via 'Add board' below)",
                        );
                    } else {
                        for opt in &options {
                            ui.selectable_value(&mut chosen, opt.clone(), opt);
                        }
                    }
                });
            if chosen != current_serial && !chosen.is_empty() {
                let chosen_for_log = chosen.clone();
                crate::config::modify_config(|c| {
                    if c.rotors.is_empty() {
                        c.rotors.push(crate::config::RotorConfig::default());
                    }
                    let name = chosen
                        .strip_prefix(BoardKind::ROTOR_PREFIX)
                        .unwrap_or(&chosen)
                        .to_string();
                    c.rotors[0].enabled = true;
                    c.rotors[0].mcp_serial = chosen.clone();
                    if c.rotors[0].name.is_empty() {
                        c.rotors[0].name = name;
                    }
                    // Linking implies the MCP-rotor backend; without it the
                    // server would create the EA7HG backend after restart and not bind.
                    c.rotor_backend = "mcp2221_yaesu".to_string();
                });
                log::info!(
                    "Rotor MCP serial linked to \"{}\" (backend -> mcp2221_yaesu) \u{2014} auto-restart",
                    chosen_for_log,
                );
                restart_server();
            }
        });
        ui.label(
            RichText::new(
                "  Tip: heeft het bord nog geen rot_-naam? Programmeer het eerst via 'Add board' hieronder (functie Rotor).",
            )
            .small()
            .color(Color32::from_rgb(160, 160, 160)),
        );
    });
}

fn render_board_scan_section(ui: &mut egui::Ui) {
    let cache = scan_cache();

    ui.horizontal(|ui| {
        ui.label(RichText::new("Detected MCP2221A boards:").strong());
        if ui.button(rust_i18n::t!("srv_scan").to_string()).clicked() {
            let result = crate::mcp2221_scan::list_boards().map_err(|e| format!("{:?}", e));
            *cache.lock().unwrap() = Some(result);
        }
    });
    let cached = cache.lock().unwrap();
    match cached.as_ref() {
        None => {
            ui.colored_label(
                Color32::from_rgb(160, 160, 160),
                "  (click Scan to enumerate)",
            );
        }
        Some(Ok(list)) if list.is_empty() => {
            ui.colored_label(
                Color32::from_rgb(220, 160, 40),
                "  (none detected - check USB cable)",
            );
        }
        Some(Ok(list)) => {
            // Count unprogrammed boards - at >1 disable the Add button
            // so the operator knows for sure which physical board they
            // are about to use (otherwise an added `tun_<name>`
            // could go to the wrong Adafruit). Operator convention:
            // always configure with at most 1 unnamed board connected.
            use crate::mcp2221_scan::BoardKind;
            let unprogrammed_count = list
                .iter()
                .filter(|b| b.kind() == BoardKind::Unprogrammed)
                .count();
            if unprogrammed_count > 1 {
                ui.colored_label(
                    Color32::from_rgb(220, 160, 40),
                    format!(
                        "  ! {} onbenoemde boards aangesloten - sluit max 1 ongeprogrammeerd board aan tijdens configuratie",
                        unprogrammed_count
                    ),
                );
            }
            for b in list {
                let kind = b.kind();
                sdr_remote_theme::group(ui).show(ui, |ui| {
                    // Header with function-classification tag + serial label.
                    ui.horizontal(|ui| {
                        let tag_color = match kind {
                            BoardKind::Tuner => Color32::from_rgb(80, 180, 220),
                            BoardKind::Rotor => Color32::from_rgb(220, 160, 60),
                            BoardKind::Unprogrammed => Color32::from_rgb(160, 160, 160),
                        };
                        ui.colored_label(
                            tag_color,
                            RichText::new(format!("[{}]", kind.label())).strong(),
                        );
                        ui.label(RichText::new(b.label()).monospace());
                    });
                    ui.label(
                        RichText::new(format!("path: {}", b.path))
                            .monospace()
                            .small()
                            .weak(),
                    );

                    if kind == BoardKind::Unprogrammed {
                        // Wizard for new boards: pick function + give name
                        // + Add. Server writes `tun_<name>` or `rot_<name>`
                        // to EEPROM and adds the new entry to
                        // `config.tuners` (only Tuner in v2.0.5; Rotor
                        // is planned for phase 4).
                        ui.horizontal(|ui| {
                            ui.label(rust_i18n::t!("srv_function").to_string());
                            let func = board_function_state(&b.path);
                            let mut sel = *func.lock().unwrap();
                            egui::ComboBox::from_id_salt(format!("kind_{}", b.path))
                                .selected_text(sel.label())
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut sel, BoardKind::Tuner, "Tuner");
                                    ui.selectable_value(&mut sel, BoardKind::Rotor, "Rotor");
                                });
                            *func.lock().unwrap() = sel;

                            ui.label(rust_i18n::t!("srv_name").to_string());
                            let text = board_serial_edit_state(&b.path);
                            let mut guard = text.lock().unwrap();
                            ui.add(
                                egui::TextEdit::singleline(&mut *guard)
                                    .desired_width(160.0)
                                    .hint_text("e.g. JC-4s loop / rotor1"),
                            );

                            let add_enabled = unprogrammed_count <= 1
                                && (sel == BoardKind::Tuner || sel == BoardKind::Rotor)
                                && !guard.trim().is_empty();
                            if ui.add_enabled(add_enabled, egui::Button::new("Add")).clicked() {
                                let name = guard.trim().to_string();
                                drop(guard);
                                let prefix = match sel {
                                    BoardKind::Tuner => BoardKind::TUNER_PREFIX,
                                    BoardKind::Rotor => BoardKind::ROTOR_PREFIX,
                                    BoardKind::Unprogrammed => "",
                                };
                                let new_serial = format!("{}{}", prefix, name);
                                let path = b.path.clone();
                                log::info!(
                                    "Add board: path=\"{}\" function={} new_serial=\"{}\"",
                                    path, sel.label(), new_serial
                                );
                                // Uniqueness check on config.tuners AND config.rotors
                                // (one serial must not already be in either).
                                let cfg_snap = crate::config::load();
                                let conflict = cfg_snap.tuners.iter().any(|t| t.mcp_serial == new_serial)
                                    || cfg_snap.rotors.iter().any(|r| r.mcp_serial == new_serial);
                                let res = if conflict {
                                    Err(format!("serial \"{}\" bestaat al", new_serial))
                                } else if sel == BoardKind::Tuner {
                                    crate::mcp2221_scan::program_serial_at_path(
                                        &path,
                                        &new_serial,
                                    )
                                    .map(|()| {
                                        // Add new TunerConfig entry
                                        // to config and save.
                                        crate::config::modify_config(|c| {
                                            if c.tuners.len() < crate::config::MAX_TUNERS {
                                                let mut t =
                                                    crate::config::TunerConfig::default();
                                                t.enabled = true;
                                                t.mcp_serial = new_serial.clone();
                                                c.tuners.push(t);
                                            }
                                        });
                                    })
                                    .map_err(|e| format!("{:?}", e))
                                } else if sel == BoardKind::Rotor {
                                    // PATCH-yaesu-rotor-mcp2221 phase 1: claim Adafruit
                                    // board as rotor slot. Runtime binding (driver
                                    // module + actual control of GP0/1 + DAC/
                                    // ADC) comes in phase 3.
                                    crate::mcp2221_scan::program_serial_at_path(
                                        &path,
                                        &new_serial,
                                    )
                                    .map(|()| {
                                        crate::config::modify_config(|c| {
                                            if c.rotors.len() < crate::config::MAX_ROTORS {
                                                let mut r =
                                                    crate::config::RotorConfig::default();
                                                r.enabled = true;
                                                r.name = name.clone();
                                                r.mcp_serial = new_serial.clone();
                                                c.rotors.push(r);
                                            }
                                            // Claiming a rotor board implies the
                                            // MCP-rotor backend; otherwise the
                                            // server won't bind it after restart (stays on
                                            // the default ea7hg).
                                            c.rotor_backend = "mcp2221_yaesu".to_string();
                                        });
                                    })
                                    .map_err(|e| format!("{:?}", e))
                                } else {
                                    Err("kies Tuner of Rotor als functie".to_string())
                                };
                                let ok = res.is_ok();
                                *board_program_result_state(&b.path).lock().unwrap() =
                                    Some(res);
                                if ok {
                                    // Restart so the TunerInstance binds the
                                    // new config entry immediately.
                                    restart_server();
                                }
                            }
                        });
                        // Hint about uniqueness + max-1-board rule
                        ui.label(
                            RichText::new(
                                "  Tip: configureer met max 1 onbenoemd board aangesloten.",
                            )
                            .small()
                            .weak(),
                        );
                    } else {
                        // Tuner or Rotor - rename flow stays available,
                        // prefix is enforced via a read-only label.
                        // For a Tuner board without a matching config
                        // entry we also show a "Koppel aan config" button;
                        // that covers the scenario "operator removed tuner1_*/2_*
                        // config lines but the board already has
                        // a programmed tun_ serial" - the new entry
                        // is added without reprogramming.
                        let prefix = match kind {
                            BoardKind::Tuner => BoardKind::TUNER_PREFIX,
                            BoardKind::Rotor => BoardKind::ROTOR_PREFIX,
                            BoardKind::Unprogrammed => "",
                        };
                        let in_config = crate::config::load()
                            .tuners
                            .iter()
                            .any(|t| t.mcp_serial == b.serial_number);
                        ui.horizontal(|ui| {
                            ui.label(rust_i18n::t!("srv_rename_label").to_string());
                            ui.label(RichText::new(prefix).monospace().weak());
                            let text = board_serial_edit_state(&b.path);
                            let mut guard = text.lock().unwrap();
                            if guard.is_empty() {
                                // Pre-fill with current name (without prefix)
                                *guard = b
                                    .serial_number
                                    .strip_prefix(prefix)
                                    .unwrap_or(&b.serial_number)
                                    .to_string();
                            }
                            ui.add(
                                egui::TextEdit::singleline(&mut *guard)
                                    .desired_width(160.0)
                                    .hint_text("naam"),
                            );
                            if ui.button(rust_i18n::t!("srv_rename").to_string()).clicked() {
                                let name = guard.trim().to_string();
                                drop(guard);
                                let new_serial = format!("{}{}", prefix, name);
                                let path = b.path.clone();
                                let old_serial = b.serial_number.clone();
                                log::info!(
                                    "Rename board: path=\"{}\" old=\"{}\" new=\"{}\"",
                                    path, old_serial, new_serial
                                );
                                let res = if name.is_empty() {
                                    Err(rust_i18n::t!("srv_name_empty").to_string().to_string())
                                } else {
                                    crate::mcp2221_scan::program_serial_at_path(
                                        &path,
                                        &new_serial,
                                    )
                                    .map(|()| {
                                        // Update references in config.tuners
                                        // (mcp_serial field matched on old name).
                                        crate::config::modify_config(|c| {
                                            for t in &mut c.tuners {
                                                if t.mcp_serial == old_serial {
                                                    t.mcp_serial = new_serial.clone();
                                                }
                                            }
                                        });
                                    })
                                    .map_err(|e| format!("{:?}", e))
                                };
                                let ok = res.is_ok();
                                *board_program_result_state(&b.path).lock().unwrap() =
                                    Some(res);
                                if ok {
                                    // Restart so the TunerInstance binds the
                                    // new serial; without this the old slot
                                    // keeps showing "not connected".
                                    restart_server();
                                }
                            }
                        });
                        if !in_config && kind == BoardKind::Tuner {
                            ui.horizontal(|ui| {
                                ui.colored_label(
                                    Color32::from_rgb(220, 160, 40),
                                    "  ! Geprogrammeerd tuner-board zonder config-entry.",
                                );
                                let can_add = crate::config::load().tuners.len()
                                    < crate::config::MAX_TUNERS;
                                if ui
                                    .add_enabled(can_add, egui::Button::new("Koppel aan config"))
                                    .clicked()
                                {
                                    let serial = b.serial_number.clone();
                                    log::info!(
                                        "Claim tuner board: serial=\"{}\" -> new config entry",
                                        serial
                                    );
                                    crate::config::modify_config(|c| {
                                        if c.tuners.len() < crate::config::MAX_TUNERS {
                                            let mut t =
                                                crate::config::TunerConfig::default();
                                            t.enabled = true;
                                            t.mcp_serial = serial.clone();
                                            t.model = infer_model_from_serial(&serial);
                                            c.tuners.push(t);
                                        }
                                    });
                                    *board_program_result_state(&b.path).lock().unwrap() =
                                        Some(Ok(()));
                                    restart_server();
                                }
                            });
                        }
                    }
                    // Result of the last Add/Rename attempt.
                    if let Some(res) = board_program_result_state(&b.path)
                        .lock()
                        .unwrap()
                        .as_ref()
                    {
                        match res {
                            Ok(()) => {
                                ui.colored_label(
                                    Color32::from_rgb(50, 200, 50),
                                    "OK - klik Scan om te verversen",
                                );
                            }
                            Err(msg) => {
                                ui.colored_label(
                                    Color32::from_rgb(220, 60, 60),
                                    format!("error: {}", msg),
                                );
                            }
                        }
                    }
                });
            }
        }
        Some(Err(e)) => {
            ui.colored_label(
                Color32::from_rgb(220, 60, 60),
                format!("  scan failed: {}", e),
            );
        }
    }
}

/// Why a radio's port does not open, as the key of its text: only while the
/// radio is not connected (build 270; the texts are the settings' old note).
pub(crate) fn port_trouble_key(connected: bool, trouble: u8) -> Option<&'static str> {
    use sdr_remote_core::protocol::{PORT_TROUBLE_BUSY, PORT_TROUBLE_MISSING};
    match (connected, trouble) {
        (false, PORT_TROUBLE_BUSY) => Some("srv_port_busy"),
        (false, PORT_TROUBLE_MISSING) => Some("srv_port_missing"),
        _ => None,
    }
}

/// "Yaesu N: why its port does not open", in orange, when the server knows.
fn render_port_trouble(ui: &mut egui::Ui, index: usize, radio: Option<&crate::yaesu::YaesuRadio>) {
    let Some(y) = radio else { return };
    let st = y.status();
    if let Some(key) = port_trouble_key(st.connected, st.port_trouble) {
        ui.colored_label(
            Color32::from_rgb(255, 170, 40),
            format!("Yaesu {}: {}", index + 1, rust_i18n::t!(key)),
        );
    }
}

/// "Yaesu N: transmits when Windows raises RTS or DTR ...": the menus that key
/// the radio on a line of its USB port (build 296; owner, 2026-10-24).
fn render_line_keyed(ui: &mut egui::Ui, index: usize, radio: Option<&crate::yaesu::YaesuRadio>) {
    let Some(y) = radio else { return };
    let st = y.status();
    use crate::yaesu::line_ptt;
    let shown = line_ptt::shown(&st);
    if shown.is_empty() {
        return;
    }
    ui.colored_label(
        Color32::from_rgb(255, 170, 40),
        format!("Yaesu {}: {}", index + 1, rust_i18n::t!("srv_line_ptt", menus = line_ptt::list(&shown))),
    );
}

/// The lead targets the developer mode offers (build 278; back in 296), in ms.
const LEAD_CHOICES: [u64; 8] = [0, 20, 40, 60, 80, 100, 120, crate::yaesu::tx_timing::LEAD_MAX_MS];

/// "Yaesu N TX lead": the target in the radio's sound card ring and whether
/// only every other over gets it (build 278). In the developer mode only, live
/// (the next over takes it), and not kept: outside the mode, and after a
/// restart, every over gets `LEAD_MS` (owner, 2026-10-24).
fn render_lead_row(ui: &mut egui::Ui, index: usize, radio: Option<&crate::yaesu::YaesuRadio>) {
    let Some(y) = radio else { return };
    let timing = y.status_arc().lock().unwrap().tx_timing.clone();
    let (mut ms, mut alternate) = timing.lead_setting();
    let before = (ms, alternate);
    ui.horizontal(|ui| {
        ui.label(format!("Yaesu {} TX lead (developer):", index + 1));
        egui::ComboBox::from_id_salt(format!("yaesu_tx_lead_{}", index))
            .selected_text(format!("{} ms", ms))
            .show_ui(ui, |ui| {
                for choice in LEAD_CHOICES {
                    ui.selectable_value(&mut ms, choice, format!("{} ms", choice));
                }
            });
        ui.checkbox(&mut alternate, "every other over")
            .on_hover_text("Odd overs get the lead, even overs none: both settings on the same road");
    });
    if (ms, alternate) != before {
        timing.set_lead(ms, alternate);
    }
}

/// The settings probe's note when it found no radio: why the port did not
/// open, when it did not (build 271; round 270), else `fallback` - "switch the
/// radio on" or "it did not answer". The same keys as the panel's line.
pub(crate) fn probe_note_key(trouble: u8, fallback: &'static str) -> &'static str {
    port_trouble_key(false, trouble).unwrap_or(fallback)
}

/// One audio chip on the panel, one per transmitter (build 271, owner's
/// proposal): its receive audio, and in its place its transmit audio while that
/// comes in. RX2 is a receiver only. Until 271 receive and transmit stood side
/// by side, both green.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chip {
    /// RX1, or TX while Thetis transmits.
    Thetis,
    Rx2,
    /// Radio 1 (`0`) or radio 2 (`1`).
    Radio(usize),
}

impl Chip {
    fn label(self, transmitting: bool) -> &'static str {
        match (self, transmitting) {
            (Chip::Thetis, false) => "RX1",
            (Chip::Thetis, true) => "TX",
            (Chip::Rx2, _) => "RX2",
            (Chip::Radio(0), false) => "Y1-RX",
            (Chip::Radio(0), true) => "Y1-TX",
            (Chip::Radio(_), false) => "Y2-RX",
            (Chip::Radio(_), true) => "Y2-TX",
        }
    }

    /// Does the transmitter itself say it transmits (build 272)? Thetis by its
    /// TRX notification or Tune; a radio by the TX state the server keeps of it
    /// (`reported_tx`, read under the lock, not copied). An FT-991A's answer
    /// was measured unreliable - here at most a short flicker.
    fn says_transmitting(self, shared: &StatusPanelShared) -> bool {
        match self {
            Chip::Thetis => shared.tci.transmitting.load(Ordering::Relaxed),
            Chip::Rx2 => false,
            Chip::Radio(r) => shared
                .radios
                .get()
                .and_then(|radios| radios[r.min(1)].as_ref())
                .is_some_and(|y| y.status_arc().lock().map_or(false, |s| s.reported_tx())),
        }
    }

    /// Its receive counter, and its transmit counter if it has a transmitter.
    fn stats(
        self,
        a: &crate::audio_stats::AudioActivityStats,
    ) -> (&crate::audio_stats::ChannelStats, Option<&crate::audio_stats::ChannelStats>) {
        match self {
            Chip::Thetis => (&a.rx1, Some(&a.tx)),
            Chip::Rx2 => (&a.rx2, None),
            Chip::Radio(r) => (&a.yaesu_rx[r.min(1)], Some(&a.yaesu_tx[r.min(1)])),
        }
    }
}

/// How fresh audio has to be to count as flowing: a chip is green, and transmit
/// audio means sending (one number since build 272; round 271).
const FRESH_SECS: f32 = 1.0;

/// Is transmit audio flowing: under `FRESH_SECS` old.
fn transmitting(tx_age: Option<f32>) -> bool {
    tx_age.is_some_and(|a| a < FRESH_SECS)
}

/// Why a chip is red (build 272): the transmitter says it transmits, transmit
/// audio flows through ThetisLink, or both. A microphone at the radio is the
/// first alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxShown {
    Not,
    Radio,
    Audio,
    Both,
}

fn tx_shown(radio_says: bool, tx_age: Option<f32>) -> TxShown {
    match (radio_says, transmitting(tx_age)) {
        (false, false) => TxShown::Not,
        (true, false) => TxShown::Radio,
        (false, true) => TxShown::Audio,
        (true, true) => TxShown::Both,
    }
}

/// The audio chips for what the server set up: Thetis's two only with a TCI
/// address, one for each radio that is there. Before the server has said,
/// Thetis's, as the panel always showed.
fn chips(configured: Option<&crate::audio_stats::Configured>) -> Vec<Chip> {
    let (thetis, radios) = configured.map_or((true, [false, false]), |c| (c.thetis, c.radios));
    let mut v = Vec::new();
    if thetis {
        v.extend([Chip::Thetis, Chip::Rx2]);
    }
    for (r, there) in radios.into_iter().enumerate() {
        if there {
            v.push(Chip::Radio(r));
        }
    }
    v
}

/// A transmitter sending: its transmit label in red (build 271), and the tip
/// says where the red comes from (build 272).
fn render_tx_chip(ui: &mut egui::Ui, label: &str, shown: TxShown) {
    let why = match shown {
        TxShown::Radio => "the radio reports transmitting",
        TxShown::Audio => "transmit audio is coming in through ThetisLink",
        TxShown::Both | TxShown::Not => "the radio reports transmitting, with transmit audio through ThetisLink",
    };
    ui.colored_label(Color32::from_rgb(220, 60, 60), RichText::new(format!("● {}", label)))
        .on_hover_text(format!("{}: {}", label, why));
}

fn render_channel_chip(
    ui: &mut egui::Ui,
    label: &str,
    stats: &crate::audio_stats::ChannelStats,
    server_start: std::time::Instant,
) {
    let (count, age) = stats.snapshot(server_start);
    let (sym, color) = match age {
        None => ("-", Color32::from_rgb(120, 120, 120)),
        Some(a) if a < FRESH_SECS => ("●", Color32::from_rgb(50, 200, 50)),
        Some(a) if a < 5.0 => ("●", Color32::from_rgb(200, 160, 40)),
        Some(_) => ("○", Color32::from_rgb(160, 160, 160)),
    };
    let tip = match age {
        None => format!("{}: never seen", label),
        // Audio, not signal: a quiet band is audio too (build 272).
        Some(a) => format!(
            "{}: audio {} ago (also when the band is quiet), {} frames since start",
            label,
            format_duration_short(a),
            count
        ),
    };
    ui.colored_label(color, RichText::new(format!("{} {}", sym, label)))
        .on_hover_text(tip);
}

/// Render a single device dot: filled = configured at server-start,
/// hollow = absent. Hover-tip clarifies that the dot is configuration-state,
/// not live link-health.
fn render_device_dot(ui: &mut egui::Ui, label: &str, configured: bool) {
    let (sym, color, tip) = if configured {
        (
            "●",
            Color32::from_rgb(80, 140, 200),
            format!("{}: configured at server-start", label),
        )
    } else {
        (
            "○",
            Color32::from_rgb(120, 120, 120),
            format!("{}: not configured", label),
        )
    };
    ui.colored_label(color, RichText::new(format!("{} {}", sym, label)))
        .on_hover_text(tip);
}

/// Format a seconds value as "Xm Ys" (or "Ys" when under a minute).
fn format_duration_short(secs: f32) -> String {
    let total = secs.max(0.0) as u64;
    let m = total / 60;
    let s = total % 60;
    if m == 0 {
        format!("{}s", s)
    } else if m < 60 {
        format!("{}m {}s", m, s)
    } else {
        let h = m / 60;
        let m = m % 60;
        format!("{}h {}m", h, m)
    }
}

#[cfg(test)]
mod setup_tests {
    use super::{chips, transmitting, tx_shown, Chip, TxShown, FRESH_SECS};
    use crate::audio_stats::Configured;

    /// What the panel shows follows what the server set up (build 269): no
    /// Thetis chips without a TCI address, and a radio's chips and dot for
    /// each radio that is there - radio 2 alone too (a new user with only an
    /// FTX-1 on slot 2, 2026-10-20).
    #[test]
    fn the_panel_shows_what_the_server_has() {
        let c = |thetis, r1, r2| Configured { thetis, radios: [r1, r2] };
        assert_eq!(chips(Some(&c(true, false, false))), vec![Chip::Thetis, Chip::Rx2]);
        assert_eq!(chips(Some(&c(false, false, true))), vec![Chip::Radio(1)]);
        assert_eq!(chips(Some(&c(true, true, true))), vec![Chip::Thetis, Chip::Rx2, Chip::Radio(0), Chip::Radio(1)]);
        assert_eq!(chips(Some(&c(false, false, false))), vec![], "a bare server: nothing to show");
        assert_eq!(chips(None), vec![Chip::Thetis, Chip::Rx2], "before the server said: as it was");
        // One chip per transmitter: its receive label, or its transmit label
        // while transmit audio comes in (build 271, owner's proposal).
        let labels: Vec<(&str, &str)> = [Chip::Thetis, Chip::Rx2, Chip::Radio(0), Chip::Radio(1)]
            .iter()
            .map(|c| (c.label(false), c.label(true)))
            .collect();
        assert_eq!(labels, [("RX1", "TX"), ("RX2", "RX2"), ("Y1-RX", "Y1-TX"), ("Y2-RX", "Y2-TX")]);
        // The start line, one per state.
        assert_eq!(c(true, true, false).line(), "Status panel: Thetis configured, radio 1 present, radio 2 absent");
        assert_eq!(c(false, false, true).line(), "Status panel: Thetis not configured, radio 1 absent, radio 2 present");
    }

    /// Each radio counts its own audio, where it is ticked; the two used one
    /// counter between them.
    #[test]
    fn each_radio_counts_its_own_audio() {
        let loops = include_str!("../audio_loops.rs");
        assert_eq!(loops.matches("audio_stats.yaesu_rx[(slot as usize).min(1)].tick(server_start);").count(), 1);
        let net = include_str!("../network.rs");
        // One block for both radios since build 282, by its slot.
        assert_eq!(net.matches("self.audio_stats.yaesu_tx[slot].tick(self.server_start);").count(), 1);
        assert_eq!(net.matches("radio_audio!(pkt, addr, 0, crate::session::TxTarget::Yaesu1, yaesu_tx_packet_tx, crate::tx_arrivals::TxStream::Radio1);").count(), 1, "radio 1's arm counts radio 1");
        assert_eq!(net.matches("radio_audio!(pkt, addr, 1, crate::session::TxTarget::Yaesu2, yaesu2_tx_packet_tx, crate::tx_arrivals::TxStream::Radio2);").count(), 1, "radio 2's arm counts radio 2");
        // And Thetis has one source for "configured": the heartbeat and the start.
        assert_eq!(net.matches("self.config.thetis_configured()").count(), 1);
        let main = include_str!("../main.rs");
        assert_eq!(main.matches("thetis: config.thetis_configured(),").count(), 1);
    }

    /// The chip turns to transmit while transmit audio is under a second old -
    /// the same second that makes a chip green - and never for RX2, which has
    /// no transmitter.
    #[test]
    fn a_chip_shows_transmit_while_its_audio_comes_in() {
        assert!(!transmitting(None), "never any");
        assert!(transmitting(Some(0.0)));
        assert!(transmitting(Some(0.99)));
        assert!(!transmitting(Some(1.0)), "a second old: back to receive");
        assert!(!transmitting(Some(30.0)));
        let a = crate::audio_stats::AudioActivityStats::default();
        assert!(Chip::Rx2.stats(&a).1.is_none(), "RX2 has no transmitter");
        assert!(std::ptr::eq(Chip::Thetis.stats(&a).1.unwrap(), &a.tx));
        assert!(std::ptr::eq(Chip::Radio(1).stats(&a).0, &a.yaesu_rx[1]));
        assert!(std::ptr::eq(Chip::Radio(1).stats(&a).1.unwrap(), &a.yaesu_tx[1]));
        assert!(std::ptr::eq(Chip::Radio(0).stats(&a).1.unwrap(), &a.yaesu_tx[0]));
    }

    /// Red also when the transmitter itself says it transmits - a microphone at
    /// the radio, Thetis's own MOX or a footswitch, Tune (build 272, owner's
    /// question) - and the tip says which.
    #[test]
    fn a_chip_is_red_when_the_transmitter_says_so_too() {
        assert_eq!(tx_shown(false, None), TxShown::Not);
        assert_eq!(tx_shown(false, Some(5.0)), TxShown::Not);
        assert_eq!(tx_shown(true, None), TxShown::Radio, "the microphone at the radio");
        assert_eq!(tx_shown(false, Some(0.2)), TxShown::Audio);
        assert_eq!(tx_shown(true, Some(0.2)), TxShown::Both);
        assert_eq!(FRESH_SECS, 1.0);
        assert!(transmitting(Some(FRESH_SECS - 0.01)) && !transmitting(Some(FRESH_SECS)));
        // One second for green and for "transmit audio flows" (round 271).
        let src = include_str!("status_panel.rs");
        let src = &src[..src.find("#[cfg(test)]\nmod setup_tests").unwrap()];
        assert_eq!(src.matches("Some(a) if a < FRESH_SECS =>").count(), 1);
        assert_eq!(src.matches("a < 1.0").count(), 0);
        // Where the transmitter's own word comes from.
        assert_eq!(src.matches("Chip::Thetis => shared.tci.transmitting.load(Ordering::Relaxed),").count(), 1);
        assert_eq!(src.matches(".lock().map_or(false, |s| s.reported_tx())").count(), 1);
        assert_eq!(src.matches("Chip::Rx2 => false,").count(), 1);
        let net = include_str!("../network.rs");
        assert_eq!(net.matches("probe.set_transmitting(p.tx_now().thetis);").count(), 1);
        // "Thetis transmits" has one definition, `thetis_on_the_air` (builds 273
        // and 275; rounds 272 and 274), in either spelling.
        // Every source file of the server (build 274; round 273: a copy in a
        // third file stayed green). Whole files: none has it in a test.
        let mut written_out = 0;
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut dirs = vec![src];
        while let Some(d) = dirs.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    dirs.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let text = std::fs::read_to_string(&p).unwrap();
                    // Pieced together, so this test does not count itself.
                    let n = text.matches(concat!("thetis_tx_active", "() || ")).count()
                        + text.matches(concat!("thetis_tx_active", " || self.tune_active")).count();
                    written_out += n;
                }
            }
        }
        assert_eq!(written_out, 1, "TRX or Tune, spelled out once: in thetis_on_the_air");
        let ptt = include_str!("../ptt.rs");
        assert_eq!(ptt.matches("thetis: self.tci.thetis_on_the_air(),").count(), 1, "tx_now asks it");
    }

    /// Why a probe found no radio, in the settings (build 271; round 270): a
    /// port another program holds says so, not "switch the radio on".
    #[test]
    fn the_settings_probe_names_a_port_that_does_not_open() {
        use sdr_remote_core::protocol::{PORT_TROUBLE_BUSY, PORT_TROUBLE_MISSING, PORT_TROUBLE_NONE};
        assert_eq!(super::probe_note_key(PORT_TROUBLE_BUSY, "srv_probe_none"), "srv_port_busy");
        assert_eq!(super::probe_note_key(PORT_TROUBLE_MISSING, "srv_probe_silent"), "srv_port_missing");
        assert_eq!(super::probe_note_key(PORT_TROUBLE_NONE, "srv_probe_none"), "srv_probe_none", "a silent radio");
        let up = include_str!("update.rs");
        assert_eq!(up.matches("super::status_panel::probe_note_key(self.probe_trouble[s], \"srv_probe_none\")").count(), 1);
        assert_eq!(up.matches("super::status_panel::probe_note_key(self.probe_trouble[s], \"srv_probe_silent\")").count(), 1);
        assert_eq!(up.matches("crate::yaesu::detect_model_or_trouble(&port, baud)").count(), 1);
        // Every answer for the current port sets it, before the early exit for a
        // silent radio that keeps its model.
        let at = up.find("fn drain_model_probes(").unwrap();
        let body = &up[at..];
        let set = body.find("self.probe_trouble[s] = trouble;").expect("set from the answer");
        assert!(set < body.find("self.probe_silent[s] = true;").unwrap());
    }

    /// The lines no test drives, pinned (round 269: two probes stayed green): which
    /// dot reads which radio, and the grey Thetis line.
    #[test]
    fn the_dots_and_the_grey_line_read_the_right_thing() {
        let src = include_str!("status_panel.rs");
        let src = &src[..src.find("#[cfg(test)]\nmod setup_tests").unwrap()];
        assert_eq!(src.matches("render_device_dot(ui, \"Yaesu 1\", radios[0]);").count(), 1);
        assert_eq!(src.matches("render_device_dot(ui, \"Yaesu 2\", radios[1]);").count(), 1);
        let grey = src.find("if configured.is_some_and(|c| !c.thetis) {").expect("the branch without Thetis");
        assert!(src[grey..].find("\"Not configured\"").unwrap() < 200, "and what it says");
    }

    /// Why a radio's port cannot be opened, on the status panel while the
    /// server runs, for each radio (build 270). The note in the settings could
    /// never show - they exist only while the server is stopped - and is gone.
    #[test]
    fn each_radio_says_on_the_panel_why_its_port_does_not_open() {
        use sdr_remote_core::protocol::{PORT_TROUBLE_BUSY, PORT_TROUBLE_MISSING};
        assert_eq!(super::port_trouble_key(false, PORT_TROUBLE_BUSY), Some("srv_port_busy"));
        assert_eq!(super::port_trouble_key(false, PORT_TROUBLE_MISSING), Some("srv_port_missing"));
        assert_eq!(super::port_trouble_key(true, PORT_TROUBLE_BUSY), None, "an open port says nothing");
        assert_eq!(super::port_trouble_key(false, 0), None);
        let src = include_str!("status_panel.rs");
        let src = &src[..src.find("#[cfg(test)]\nmod setup_tests").unwrap()];
        assert_eq!(src.matches("for (i, radio) in shared.radios.get().into_iter().flatten().enumerate() {").count(), 1);
        assert_eq!(src.matches("render_port_trouble(ui, i, radio.as_deref());").count(), 1);
        let up = include_str!("update.rs");
        assert_eq!(up.matches("srv_port_busy").count(), 0, "not in the settings, where it could not show");
        let main = include_str!("../main.rs");
        assert_eq!(main.matches("status_panel_state.radios.set([yaesu_prebuilt.clone(), yaesu2_prebuilt.clone()]);").count(), 1);
        // The lead's row, in the same loop, in the developer mode only, and
        // not kept (build 296; owner: "wel in de ontwikkelaars mode").
        assert!(src.contains("        render_port_trouble(ui, i, radio.as_deref());\n        render_line_keyed(ui, i, radio.as_deref());\n        if sdr_remote_core::dev_mode::is_on() {\n            render_lead_row(ui, i, radio.as_deref());"));
        assert_eq!(src.matches("set_lead(").count(), 1);
        assert_eq!(src.matches("    if (ms, alternate) != before {\n        timing.set_lead(ms, alternate);\n    }").count(), 1);
        assert_eq!(src.matches("save_tx_lead").count(), 0, "not kept");
    }
}
