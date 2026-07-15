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

use egui::{Color32, RichText};

use crate::audio_stats::StatusPanelShared;
use crate::session::{ClientSnapshot, ConnectAttempt};

/// Snapshot-cache: bij contentie op de SessionManager-lock (try_lock
/// faalt) renderden we eerder een 1-regel "(snapshot busy...)" placeholder.
/// Dat liet de paneel-hoogte periodiek krimpen waardoor de omringende
/// ScrollArea de scroll-positie clampte en de gebruiker visueel zag dat
/// content omhoog sprong terwijl hij naar de uitgeklapte MCP2221A-sectie
/// keek. Cache laat ons in plaats daarvan de laatst-succesvolle snapshot
/// blijven tonen - stale text is acceptabel; layout-jitter niet.
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

/// Per-client bandbreedte in kbps, uitgesplitst: (audio, spectrum, overig, up).
/// Afgeleid uit de cumulatieve byte-tellers van de tracked-socket: vorige sample +
/// tijdstip bewaard, snelheid over het interval (≥250 ms, tegen ruis). Prunet
/// tegelijk stale adres-tellers (elke herverbinding = nieuwe bron-poort).
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
    yaesu_configured: bool,
    amplitec_configured: bool,
    tuner_configured: bool,
    spe_configured: bool,
    rf2k_configured: bool,
) {
    let server_start = shared.server_start;

    // ── 1. Bind address ──────────────────────────────────────────────────
    // Reflect the actual bind outcome - a hardcoded placeholder would lie
    // about the listen address when the bind itself failed.
    ui.horizontal(|ui| {
        ui.label(RichText::new("Server:").strong());
        match shared.bind_status.get() {
            Some(crate::audio_stats::BindStatus::Ok { addr }) => {
                ui.colored_label(
                    Color32::from_rgb(50, 200, 50),
                    format!("Listening on UDP {}", addr),
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
        if tci_connected {
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
    // Bij lock-contentie pakken we de laatst gecachte snapshot zodat de
    // sectie-hoogte stabiel blijft (zie `clients_cache` doc voor reden).
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
            let col_widths = [170.0, 72.0, 56.0, 48.0, 62.0, 76.0, 76.0, 86.0, 76.0, 70.0, 48.0];
            egui::Grid::new("server_client_stats_grid")
                .num_columns(11)
                .spacing([4.0, 3.0])
                .striped(true)
                .show(ui, |ui| {
                    let headers = [
                        "Client", "Connected", "RTT", "Loss", "Jitter",
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

                        cell(ui, c.addr.to_string(), col_widths[0]);
                        cell(ui, connected_for, col_widths[1]);
                        cell(ui, format!("{} ms", c.rtt_ms), col_widths[2]);
                        cell(ui, format!("{}%", c.loss_percent), col_widths[3]);
                        cell(ui, format!("{} ms", c.jitter_ms), col_widths[4]);
                        cell(ui, format!("{} kbps", down_kbps), col_widths[5]);
                        cell(ui, format!("{} kbps", aud), col_widths[6]);
                        cell(ui, format!("{} kbps", spec), col_widths[7]);
                        cell(ui, format!("{} kbps", oth), col_widths[8]);
                        cell(ui, format!("{} kbps", up_kbps), col_widths[9]);
                        cell(ui, format!("{}s", last_seen_age), col_widths[10]);
                        ui.end_row();
                    }
                });
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
        render_channel_chip(ui, "RX1", &shared.audio.rx1, server_start);
        render_channel_chip(ui, "RX2", &shared.audio.rx2, server_start);
        render_channel_chip(ui, "TX", &shared.audio.tx, server_start);
        if yaesu_configured {
            render_channel_chip(ui, "Y-RX", &shared.audio.yaesu_rx, server_start);
            render_channel_chip(ui, "Y-TX", &shared.audio.yaesu_tx, server_start);
        }
    });

    // ── 5. Recent connect attempts ───────────────────────────────────────
    // Idem: bij lock-contentie val terug op de laatst gecachte snapshot
    // zodat de sectie-hoogte stabiel blijft.
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
    ui.separator();
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
    ui.separator();
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Configured devices:").strong());
        render_device_dot(ui, "Yaesu", yaesu_configured);
        render_device_dot(ui, "Amplitec", amplitec_configured);
        render_device_dot(ui, "Tuner", tuner_configured);
        render_device_dot(ui, "SPE", spe_configured);
        render_device_dot(ui, "RF2K", rf2k_configured);
    });

    // ── 7. MCP2221A tuner bridges - collapsible so the operator can hide the
    // per-tuner threshold sliders and board-scan tools during normal use.
    // Open/closed state is persisted in the config so it survives a restart.
    ui.separator();
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
            // 7a - per-tuner rows. Aantal slots is gelijk aan het aantal
            // entries in `config.tuners` (1..=`MAX_TUNERS`). Lege Vec =
            // geen tuner-rijen; fase 3 voegt de "Add tuner"-wizard toe
            // die nieuwe entries op basis van een board-scan aanmaakt.
            // Voor een slot dat in config staat maar nog geen running
            // TunerInstance heeft (bv. uitgeschakeld of opstart-failure)
            // tonen we de lichtere "disabled config row" met dezelfde
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
            // 7b - Yaesu rotor (PATCH-yaesu-rotor-mcp2221 fase 3): live
            // ADC-positie + handmatige CW/CCW/Stop + DAC speed-slider voor
            // hardware-verificatie. Alleen zichtbaar zodra een rot_*
            // instance is opgestart.
            if let Some(rotor) = shared.rotor_slot.get() {
                ui.separator();
                render_rotor_row(ui, rotor);
            } else {
                // Rotor (MCP2221A) niet gebonden. Toon een koppel-rij zodat een
                // al-geprogrammeerd rot_-bord vanuit een schone .conf alsnog aan
                // config.rotors gekoppeld kan worden - dit koppel-scherm ontbrak
                // t.o.v. de tuners (render_tuner_disabled_row).
                let backend_is_mcp =
                    crate::config::load().rotor_backend == "mcp2221_yaesu";
                let rot_board_detected = cached_scan_serials().iter().any(|s| {
                    s.starts_with(crate::mcp2221_scan::BoardKind::ROTOR_PREFIX)
                });
                if backend_is_mcp || rot_board_detected {
                    ui.separator();
                    render_rotor_disabled_row(ui);
                }
            }
            // 7c - board scan: list all MCP2221A devices on the USB bus so
            // the operator can figure out which USB serial belongs to which
            // physical tuner. Result cached statically until the next Scan
            // click so we don't hammer the HID enumerator on every repaint.
            ui.separator();
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
    egui::Frame::group(ui.style()).show(ui, |ui| {
        // Header: tuner label + connection status + delete-knop rechts
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
                    .on_hover_text("Verwijder dit tuner-slot uit config (auto-restart)")
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

/// Lichte config-row voor een tuner-slot dat nog niet enabled is. Toont
/// alleen de MCP-serial en Amplitec-pos dropdowns; de voltage/threshold
/// sliders zijn weggelaten omdat er geen actieve bridge is om uit te
/// lezen of te configureren. Zodra de operator een MCP serial selecteert
/// wordt de tuner enabled gezet en de server geherstart (zelfde
/// auto-restart pad als de full row).
fn render_tuner_disabled_row(ui: &mut egui::Ui, slot: usize) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Tuner {}", slot + 1)).strong());
            ui.colored_label(
                Color32::from_rgb(160, 160, 160),
                "Disabled \u{2014} select an MCP serial to enable",
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if super::delete_button(ui)
                    .on_hover_text("Verwijder dit tuner-slot uit config (auto-restart)")
                    .clicked()
                {
                    remove_tuner_slot(slot);
                }
            });
        });
        // MCP serial dropdown - selecteren enabled het slot + auto-restart.
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
        // Amplitec pos dropdown - kan al gezet worden vóór het slot enabled
        // is, maar levert weinig op zonder een actieve tuner; we tonen 'm
        // toch zodat de operator alles in één veld-sessie kan instellen.
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

/// Per-board scratch state voor de "Add new board"-wizard. Bevat de
/// gekozen functie (Tuner/Rotor) voor een ongeprogrammeerd board.
/// Default = Tuner zodat operator vaak direct kan klikken.
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

/// Verwijder de TunerConfig op slot-index `slot` uit `config.tuners` en
/// trigger auto-restart zodat de runtime de geneerde TunerInstance niet
/// meer probeert te benaderen. Operator gebruikt dit om een tuner-slot
/// definitief uit de config te halen (bv. fysiek board afwezig of
/// vervangen door een rename).
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

/// Self-restart the server. Vraagt een auto-restart aan via de globale
/// `request_auto_restart()` flag in `ui/utils.rs`. De daadwerkelijke
/// cleanup + child-spawn + `process::exit(0)` loopt in `ServerApp::
/// update()` zodra die de flag detecteert; daar worden Drop-handlers
/// correct gerund (audio cpal-streams + TCI-connect afsluiten) voor
/// de nieuwe child de devices probeert te claimen.
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

/// PATCH-yaesu-rotor-mcp2221 fase 3 - Yaesu G-1000DXC rotor live-status.
/// Toont ADC-positie (raw counts + omgerekende Yaesu-pin spanning),
/// CW/CCW/Stop knoppen, DAC speed-slider. Bedoeld voor hardware-
/// verificatie en latere kalibratie (fase 4). Geen lokale state - alle
/// commando's gaan direct naar de driver, snapshot leest live state.
fn render_rotor_row(
    ui: &mut egui::Ui,
    rotor: &std::sync::Arc<crate::mcp2221_yaesu_rotor::RotorInstance>,
) {
    use crate::mcp2221_yaesu_rotor::{Status as RotorStatus, DAC_MAX};
    let snap = rotor.status();
    egui::Frame::group(ui.style()).show(ui, |ui| {
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

        // Primaire positie-display: graden (na kalibratie) als hoofdwaarde,
        // pin-4 mediaan-spanning als secundair voor diagnose.
        ui.horizontal(|ui| {
            ui.label("Positie:");
            match (snap.position_deg, snap.median_yaesu_volts) {
                (Some(deg), Some(v)) => {
                    ui.label(
                        RichText::new(format!("{:>3}°", deg.round() as i32))
                            .strong()
                            .size(16.0),
                    );
                    ui.separator();
                    ui.label(format!("≈ {:.3} V (median)", v));
                }
                (None, Some(v)) => {
                    ui.label(
                        RichText::new("- niet gekalibreerd -")
                            .weak(),
                    );
                    ui.separator();
                    ui.label(format!("≈ {:.3} V (median)", v));
                }
                _ => {
                    ui.label(RichText::new("- geen sample -").weak());
                }
            }
        });
        // Spread + laatste raw sample voor ruis-diagnose.
        if let Some(raw) = snap.last_adc_raw {
            ui.horizontal(|ui| {
                ui.label(RichText::new("  laatste raw:").weak());
                ui.label(RichText::new(format!("{}", raw)).weak());
                if let Some(p2p) = snap.adc_p2p_raw {
                    ui.separator();
                    ui.label(RichText::new(format!("spread d={} raw", p2p)).weak());
                    // Omgerekend naar Yaesu pin-spanning voor leesbare diagnose.
                    // 1,8 k + 10 k spanningsdeler-correctie matcht operator's hardware.
                    let p2p_v = (p2p as f32) * 4.096 / 1023.0 * (11_800.0 / 10_000.0);
                    ui.label(RichText::new(format!("≈ {:.3} V p2p", p2p_v)).weak());
                }
            });
        }

        // Direction-knoppen voor handmatige test.
        ui.horizontal(|ui| {
            ui.label("Test richting:");
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

            if ui.button("Stop").clicked() {
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

        // Kalibratie (PATCH-yaesu-rotor-mcp2221 fase 4): operator draait
        // de rotor naar CCW-eindpark, klikt "Park CCW (0°)" om de
        // huidige mediaan-spanning vast te leggen; vervolgens naar CW-
        // eindpark en "Park CW". De max_deg-spinner zet de fullscale
        // (default 450° voor G-1000DXC). Alle waarden persisteren naar
        // `config.rotors[N]` zodat de mapping na restart blijft staan.
        ui.horizontal(|ui| {
            ui.label("Kalibratie:");
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
            ui.separator();
            ui.label("max:");
            let mut max_deg = snap.calibration.max_deg;
            if ui
                .add(egui::DragValue::new(&mut max_deg).range(90..=720).suffix("°"))
                .changed()
            {
                rotor.set_max_deg(max_deg);
            }
            ui.separator();
            // Ramp-rate slider: hoe snel de DAC van 0 -> max ramp-t (en
            // omgekeerd) tijdens een GoTo of start/stop. Lage waarde =
            // langzame, antenne-vriendelijke acceleratie (zware
            // mast/grote antenne); hoge waarde = snel reactief.
            ui.label("ramp:")
                .on_hover_text("Soft-start/stop snelheid (%/sec). Lager = traagheidsvriendelijker voor zware antennes.");
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
            // Shortest-route optie alleen tonen bij rotors met
            // overlap-zone (max_deg > 360); voor standaard 360°
            // rotors is de keuze betekenisloos.
            if snap.calibration.max_deg > 360 {
                ui.separator();
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

/// Rotor niet gebonden terwijl de MCP2221A-backend is gekozen (of er een
/// rot_-bord op de bus staat): laat de operator een gedetecteerd rot_-bord
/// koppelen aan `config.rotors[0]`. Analoog aan [`render_tuner_disabled_row`].
/// Dit fixt het gat dat een al-geprogrammeerd rot_-bord vanuit een schone
/// `.conf` niet via de GUI gekoppeld kon worden (bij tuners kan dat wel via de
/// MCP-serial dropdown; bij de rotor ontbrak dat scherm). Het koppelen zet
/// meteen `enabled`, de serial én de rotor-backend goed en herstart de server.
fn render_rotor_disabled_row(ui: &mut egui::Ui) {
    use crate::mcp2221_scan::BoardKind;
    egui::Frame::group(ui.style()).show(ui, |ui| {
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
                    // Koppelen impliceert de MCP-rotor-backend; zonder deze zou
                    // de server na restart de EA7HG-backend maken en niet binden.
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
        if ui.button("Scan").clicked() {
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
            // Tel ongeprogrammeerde boards - bij >1 disable de Add-knop
            // zodat de operator zeker weet welk fysiek board hij in
            // gebruik gaat nemen (anders kan een toegevoegd `tun_<naam>`
            // naar de verkeerde Adafruit gaan). Operator-conventie:
            // configureer altijd met max 1 onbenoemd board aangesloten.
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
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    // Header met functie-classificatie tag + serial-label.
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
                        // Wizard voor nieuwe boards: kies functie + geef naam
                        // + Add. Server schrijft `tun_<naam>` of `rot_<naam>`
                        // naar EEPROM en voegt de nieuwe entry toe aan
                        // `config.tuners` (alleen Tuner in v2.0.5; Rotor
                        // is voorzien voor fase 4).
                        ui.horizontal(|ui| {
                            ui.label("Functie:");
                            let func = board_function_state(&b.path);
                            let mut sel = *func.lock().unwrap();
                            egui::ComboBox::from_id_salt(format!("kind_{}", b.path))
                                .selected_text(sel.label())
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut sel, BoardKind::Tuner, "Tuner");
                                    ui.selectable_value(&mut sel, BoardKind::Rotor, "Rotor");
                                });
                            *func.lock().unwrap() = sel;

                            ui.label("Naam:");
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
                                // Uniciteit-check op config.tuners EN config.rotors
                                // (één serial mag in geen van beide al staan).
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
                                        // Voeg nieuwe TunerConfig entry toe
                                        // aan config en save.
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
                                    // PATCH-yaesu-rotor-mcp2221 fase 1: claim Adafruit
                                    // bord als rotor-slot. Runtime-binding (driver-
                                    // module + actuele aansturing van GP0/1 + DAC/
                                    // ADC) komt in fase 3.
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
                                            // Een rotor-bord claimen impliceert de
                                            // MCP-rotor-backend; anders bindt de
                                            // server 'm na restart niet (blijft op
                                            // de default ea7hg staan).
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
                                    // Restart zodat de TunerInstance de
                                    // nieuwe config-entry direct binds.
                                    restart_server();
                                }
                            }
                        });
                        // Hint over uniciteit + max-1-board-regel
                        ui.label(
                            RichText::new(
                                "  Tip: configureer met max 1 onbenoemd board aangesloten.",
                            )
                            .small()
                            .weak(),
                        );
                    } else {
                        // Tuner of Rotor - rename-flow blijft beschikbaar,
                        // prefix wordt afgedwongen via een read-only label.
                        // Voor een Tuner-board zonder bijbehorende config-
                        // entry tonen we óók een "Koppel aan config"-knop;
                        // dat dekt het scenario "operator heeft tuner1_*/2_*
                        // config-regels verwijderd maar het board heeft al
                        // een geprogrammeerd tun_-serial" - de nieuwe entry
                        // wordt zonder herprogrammering toegevoegd.
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
                            ui.label("Rename:");
                            ui.label(RichText::new(prefix).monospace().weak());
                            let text = board_serial_edit_state(&b.path);
                            let mut guard = text.lock().unwrap();
                            if guard.is_empty() {
                                // Pre-fill met huidige naam (zonder prefix)
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
                            if ui.button("Rename").clicked() {
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
                                    Err("naam mag niet leeg zijn".to_string())
                                } else {
                                    crate::mcp2221_scan::program_serial_at_path(
                                        &path,
                                        &new_serial,
                                    )
                                    .map(|()| {
                                        // Update referenties in config.tuners
                                        // (mcp_serial-veld matched op oude naam).
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
                                    // Restart zodat de TunerInstance de
                                    // nieuwe serial bindt; zonder dit blijft
                                    // de oude slot "niet verbonden" tonen.
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
                    // Resultaat van laatste Add/Rename-poging.
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

fn render_channel_chip(
    ui: &mut egui::Ui,
    label: &str,
    stats: &crate::audio_stats::ChannelStats,
    server_start: std::time::Instant,
) {
    let (count, age) = stats.snapshot(server_start);
    let (sym, color) = match age {
        None => ("-", Color32::from_rgb(120, 120, 120)),
        Some(a) if a < 1.0 => ("●", Color32::from_rgb(50, 200, 50)),
        Some(a) if a < 5.0 => ("●", Color32::from_rgb(200, 160, 40)),
        Some(_) => ("○", Color32::from_rgb(160, 160, 160)),
    };
    let tip = match age {
        None => format!("{}: never seen", label),
        Some(a) => format!(
            "{}: {} ago, {} frames since start",
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
