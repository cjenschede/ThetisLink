// SPDX-License-Identifier: GPL-2.0-or-later
//! Server start-up: `ServerApp::start_server` - spins up the network/engine
//! thread, opens the configured hardware (radios, amplifiers, tuner, rotor) and
//! switches the GUI into Running mode. Extracted verbatim from `ui/mod.rs` - pure
//! relocation, no behaviour change. `use super::*;` reaches the shared ServerApp
//! fields, config, Mode and the hardware types; `pub(super)` keeps it callable
//! from the Start button in the update loop.

use super::*;

impl ServerApp {
    pub(super) fn start_server(&mut self) {
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
            ftx1_memory_write_ack: self.ftx1_memory_write_ack,
            yaesu_baud: 38400,
            yaesu_audio_device: if self.yaesu_audio_device.is_empty() { None } else { Some(self.yaesu_audio_device.clone()) },
            yaesu_audio_output_device: if self.yaesu_audio_output_device.is_empty() { None } else { Some(self.yaesu_audio_output_device.clone()) },
            // Dual-radio slot 1 (radio 2) - now from the settings UI. Baud + audio-channel
            // stay from disk (baud autodetect via detect_model; channel = FTX-1 L/R).
            yaesu2_port: if self.yaesu2_port.is_empty() { None } else { Some(self.yaesu2_port.clone()) },
            yaesu2_enabled: self.yaesu2_enabled,
            yaesu2_baud: crate::config::load().yaesu2_baud,
            yaesu2_audio_device: if self.yaesu2_audio_device.is_empty() { None } else { Some(self.yaesu2_audio_device.clone()) },
            yaesu2_audio_output_device: if self.yaesu2_audio_output_device.is_empty() { None } else { Some(self.yaesu2_audio_output_device.clone()) },
            yaesu2_audio_channel: crate::config::load().yaesu2_audio_channel,
            amplitec_port: if amp_port.is_empty() { None } else { Some(amp_port.clone()) },
            amplitec_enabled: self.amplitec_enabled,
            amplitec_labels: self.amplitec_labels.clone(),
            amplitec_max_w: self.amplitec_max_w,
            amplitec_tx_blocked: self.amplitec_tx_blocked,
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
            layout_grids: sdr_remote_layout::layout_grids_to_config(&self.layout_grid_per_monitor),
            layout_memories: self.layout_memories.iter().map(|m| m.to_config_string()).collect(),
            ui_zoom: self.ui_zoom,
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
            // Slot-0 model per-port autodetect (dual-radio): this way every
            // combination works incl. an FTX-1 as the primary radio. Detect + open
            // run in the timeout thread (does not block the UI thread).
            // If detect fails (radio off) -> 991A-assumed label; bring-up logs the real ID.
            let ssb_on_ptt = config.yaesu_ssb_switch_on_ptt;
            let mem_write_ack = config.ftx1_memory_write_ack;
            match with_timeout(com_timeout, move || {
                let (model, det_baud) = crate::yaesu::detect_model(&port, baud)
                    .unwrap_or((crate::yaesu::RadioModel::Ft991a, baud));
                crate::yaesu::YaesuRadio::new_with_model(&port, det_baud, audio_dev_opt.as_deref(), audio_out_opt.as_deref(), model, 0, 0, ssb_on_ptt, mem_write_ack)
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

        // Create AmplitecSwitch early so UI can access it too. The
        // worker thread retries by itself when the device is offline, so we
        // create the instance even if the board is unreachable now - otherwise
        // the Amplitec window did not appear on an offline start and
        // also did not come back by itself after a power-cycle (the
        // old thread broke on the first read failure).
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

        // Create Rotor if configured - backend choice: EA7HG, PstRotator
        // or Adafruit MCP2221A (PATCH-yaesu-rotor-mcp2221).
        // The RotorInstance for mcp2221_yaesu is temporarily held in
        // `pending_yaesu_rotor` so that we can publish it in rotor_slot
        // after creating status_panel_state (further down in this fn).
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
                                "mcp2221_yaesu backend selected but config.rotors[0] is empty or disabled - use the wizard to claim a rot_<name> board"
                            );
                        }
                    } else {
                        log::warn!(
                            "mcp2221_yaesu backend selected but no rotor in config.rotors"
                        );
                    }
                }
                _ => {
                    // EA7HG default (legacy "ea7hg" or empty)
                    if !rotor_addr_str.is_empty() {
                        log::info!("Rotor (EA7HG) connecting to {}", rotor_addr_str);
                        self.rotor =
                            Some(Arc::new(crate::rotor::Rotor::new(&rotor_addr_str)));
                    }
                }
            }
        }

        // The PstRotator UDP listener (v2.1.1+) is spawned server-side in
        // main.rs::run_server_async with the pre-built rotor_inst - that is
        // where it belongs; here in ui/mod.rs a second spawn would cause an
        // "address already in use" bind conflict on the port.

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
        // Publish any Adafruit-rotor instance in the status-panel
        // slot so the rotor panel (live ADC + Park buttons + DAC slider)
        // appears alongside the standard rotor window.
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

}
