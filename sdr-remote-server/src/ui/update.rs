// SPDX-License-Identifier: GPL-2.0-or-later
//! The server-GUI eframe main loop: `eframe::App::update` for `ServerApp` - the
//! per-frame driver (settings vs running mode, panels, device/hardware controls,
//! meters, the arranger). Extracted verbatim from `ui/mod.rs` - pure relocation,
//! no behaviour change. `use super::*;` pulls in the parent module's types,
//! imports and the `ServerApp` inherent methods this loop calls.

use super::*;

impl eframe::App for ServerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Shared theme with the client. "Classic" is byte-for-byte the original
        // light-gray scheme of this window, so the default changes nothing.
        sdr_remote_theme::apply_visuals(ctx, self.theme_variant, &self.theme_custom);

        // Finish a recall that had to wait for a device to appear.
        self.apply_pending_layout(ctx);

        // UI scale. Two directions, on purpose:
        //  - our setting -> egui, so a stored choice applies on startup and on change;
        //  - egui -> our setting, so egui's own Ctrl+/Ctrl-/Ctrl+0 is picked up and
        //    persisted instead of being silently reset on the next launch.
        // Applies to the device windows too: they share this context.
        let egui_zoom = ctx.zoom_factor();
        if (egui_zoom - self.ui_zoom).abs() > 0.001 {
            if self.ui_zoom_pending {
                ctx.set_zoom_factor(self.ui_zoom);
                self.ui_zoom_pending = false;
            } else {
                self.ui_zoom = egui_zoom.clamp(0.5, 2.0);
                self.save_window_positions();
            }
        }

        // Auto-start on first frame if configured
        if self.pending_autostart {
            self.pending_autostart = false;
            self.start_server();
        }

        // Refresh in-memory mirror of the label config - it is updated by the
        // Amplitec rename dialog (context menu) via `modify_config`,
        // and this path ensures the UI shows the new name in the same frame
        // without a server restart.
        {
            let live_labels = crate::config::load().amplitec_labels.clone();
            if live_labels != self.amplitec_labels {
                self.amplitec_labels = live_labels;
            }
        }

        // Auto-restart handling: a UI button (tuner config, slot delete,
        // serial rename) has signaled via `request_auto_restart()` that
        // the server must restart. Do that here in the event loop so that:
        //   1. Drop handlers run correctly on the stopped hardware Arcs
        //      (release audio cpal streams + Thetis TCI WebSocket).
        //   2. A short sleep gives the OS time to release those handles
        //      before the new child tries to enumerate.
        // Previously restart_server called process::exit(0) directly after spawn,
        // which skipped Drop - audio on the new instance then often
        // did not work until the operator manually shut down and restarted the server.
        if auto_restart_requested() {
            self.save_window_positions();
            if let Some(tx) = self.shutdown_tx.take() {
                let _ = tx.send(true);
            }
            // Drop all hardware Arcs -> cpal streams, serial ports and
            // TCI connection are closed via their own Drop impls.
            self.yaesu = None;
            self.amplitec = None;
            self.tuner = None;
            self.spe = None;
            self.rf2k = None;
            self.ultrabeam = None;
            self.rotor = None;
            self.status_panel_state = None;
            // Give the OS time to release USB-HID + audio-device handles
            // before the new child enumeration starts. Empirically: 500-800
            // ms is enough on Windows; 600 ms is a safe middle ground
            // between "audio still claimed" and "operator notices the pause".
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
                    // ScrollArea so the Settings panel stays usable
                    // at smaller window heights too - the Save & Start
                    // button sits all the way at the bottom and must always be
                    // reachable without dragging the whole window larger.
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.heading(format!("ThetisLink Server v{}", sdr_remote_core::VERSION));
                    ui.add_space(10.0);

                    // Theme choice: same variants as the client. Immediately visible,
                    // because apply_visuals runs every frame from self.theme_variant.
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

                    // UI language: base EN + choice NL/DE/FR. Applied immediately
                    // (set_locale) and saved (language= in the server conf).
                    // Phased migration: not-yet-migrated texts stay NL.
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
                    // TX/output device: separately selectable so the transmit audio always
                    // goes to the right codec (PATCH-yaesu-output-device). Empty = same as
                    // the input; choose this when the capture/render endpoints have different names.
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

                    // FTX-1 memory write permission. Costs the tones stored in the radio,
                    // so the condition is spelled out next to the box rather than hidden in
                    // a tooltip - a hover text is not where you put something irreversible.
                    ui.checkbox(&mut self.ftx1_memory_write_ack, rust_i18n::t!("srv_ftx1_mem_write_ack").to_string())
                        .on_hover_text(rust_i18n::t!("srv_ftx1_mem_write_ack_hover").to_string());
                    ui.indent("ftx1_mem_write_note", |ui| {
                        ui.label(
                            egui::RichText::new(rust_i18n::t!("srv_ftx1_mem_write_note").to_string())
                                .small()
                                .color(ui.visuals().weak_text_color()),
                        );
                    });
                    ui.add_space(4.0);

                    // Dual-radio slot 1 (radio 2) - same setup as radio 1.
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

                    // JC-4s / JC-3s tuners - no more COM port. Each tuner
                    // is driven via an Adafruit MCP2221A USB-HID
                    // breakout and assigned per slot in the server status
                    // panel under "MCP2221A tuner bridges". Here only
                    // the open-window-at-start checkbox for the primary tuner's
                    // Tuner popout panel remains.
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
                        // Snapshot for change detection; on change persist
                        // to disk immediately (otherwise the choice is
                        // lost when the operator doesn't restart the server via Start
                        // after the dropdown change).
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

                    // PstRotator listener - parallel input source on top of
                    // the active rotor backend. Independent of the
                    // backend choice; works e.g. to let Log4OM -> PstRotator
                    // control the Adafruit rotor.
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
                    }); // <- end ScrollArea wrap for Mode::Settings
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
                    // UI scale, next to the arrange button: the two belong together -
                    // a smaller scale is what makes a finer grid worth having.
                    ui.horizontal(|ui| {
                        ui.label(rust_i18n::t!("srv_ui_scale").to_string());
                        if let Some(v) = sdr_remote_layout::ui_scale_picker(ui, "srv_ui_zoom", self.ui_zoom) {
                            self.ui_zoom = v;
                            self.ui_zoom_pending = true;
                            self.save_window_positions();
                        }
                        ui.label(egui::RichText::new(rust_i18n::t!("srv_ui_scale_hint").to_string())
                            .size(11.0).color(egui::Color32::GRAY));
                    });
                    // PATCH-2: Status / Logs tabs + "Schik" button (arrange windows).
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

                    // Reserve space at the bottom for the two stacked buttons
                    // (Exit + Settings) with their separators, so a long
                    // status/scan list doesn't push them out of view. Previously
                    // there was a fixed 30px here - too little for two buttons, so
                    // after an Adafruit scan the Settings button became unreachable
                    // (the panel itself doesn't scroll, only the ScrollArea).
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
                        // Clean shutdown (PE1FMC request): signal the server shutdown so the
                        // Yaesu SSB routing is restored (connect-set mode; in per-PTT mode
                        // the radio is already normal), drop the radio Arc so the cmd channel
                        // closes -> restore in the serial thread, give it ~500 ms, shut down.
                        // This keeps the 991 from being left in USB mode after shutdown.
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
                    // Geometry is stored in SYSTEM points so a saved layout survives
                    // a change of UI scale; a ViewportBuilder wants EGUI points, which
                    // egui-winit scales by zoom_factor. Identical at 100%, a factor
                    // `zoom` out at anything else.
                    let z = ctx.zoom_factor().max(0.01);
                    tuner_vb = tuner_vb.with_inner_size([tuner_sz[0] / z, tuner_sz[1] / z]);
                    if let Some(pos) = self.tuner_window_pos {
                        tuner_vb = tuner_vb.with_position(egui::pos2(pos[0] / z, pos[1] / z));
                    }
                    self.tuner_window_init_applied = true;
                }
                let mut tuner_closed = false;
                ctx.show_viewport_immediate(
                    ViewportId::from_hash_of("tuner_control"),
                    tuner_vb,
                    |ctx, _class| {
                        // Track window position and size
                        // Back to SYSTEM points on the way in (see the builder above).
                        let z = ctx.zoom_factor().max(0.01);
                        if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
                            self.tuner_window_pos = Some([rect.left() * z, rect.top() * z]);
                        }
                        if let Some(rect) = ctx.input(|i| i.viewport().inner_rect) {
                            self.tuner_window_size = Some([rect.width() * z, rect.height() * z]);
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
                            }); // <- ScrollArea wrap for Tuner content
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
                    // Geometry is stored in SYSTEM points; a ViewportBuilder wants
                    // EGUI points, which egui-winit scales by zoom_factor.
                    let z = ctx.zoom_factor().max(0.01);
                    amp_vb = amp_vb.with_inner_size([amp_sz[0] / z, amp_sz[1] / z]);
                    if let Some(pos) = self.amplitec_window_pos {
                        amp_vb = amp_vb.with_position(egui::pos2(pos[0] / z, pos[1] / z));
                    }
                    self.amplitec_window_init_applied = true;
                }
                let mut amplitec_closed = false;
                ctx.show_viewport_immediate(
                    ViewportId::from_hash_of("amplitec_control"),
                    amp_vb,
                    |ctx, _class| {
                        // Back to SYSTEM points on the way in (see the builder above).
                        let z = ctx.zoom_factor().max(0.01);
                        if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
                            self.amplitec_window_pos = Some([rect.left() * z, rect.top() * z]);
                        }
                        if let Some(rect) = ctx.input(|i| i.viewport().inner_rect) {
                            self.amplitec_window_size = Some([rect.width() * z, rect.height() * z]);
                        }
                        if ctx.input(|i| i.viewport().close_requested()) {
                            self.show_amplitec_window = false;
                            self.amplitec_window_init_applied = false;
                            amplitec_closed = true;
                            return;
                        }
                        egui::CentralPanel::default().show(ctx, |ui| {
                            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                                let power_changed = render_amplitec_panel(
                                    ui, &amplitec_for_window, &status,
                                    &labels,
                                    &mut self.amplitec_max_w, &mut self.amplitec_tx_blocked,
                                    &log_entries, &mut self.show_amplitec_log,
                                );
                                if power_changed {
                                    // Persist to the server conf; the network loop
                                    // rereads it (config::load) and pushes the
                                    // table to the clients (read-only display).
                                    let mw = self.amplitec_max_w;
                                    let tb = self.amplitec_tx_blocked;
                                    crate::config::modify_config(|cfg| {
                                        cfg.amplitec_max_w = mw;
                                        cfg.amplitec_tx_blocked = tb;
                                    });
                                }
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
                    // Geometry is stored in SYSTEM points so a saved layout survives
                    // a change of UI scale; a ViewportBuilder wants EGUI points, which
                    // egui-winit scales by zoom_factor. Identical at 100%, a factor
                    // `zoom` out at anything else.
                    let z = ctx.zoom_factor().max(0.01);
                    spe_vb = spe_vb.with_inner_size([spe_sz[0] / z, spe_sz[1] / z]);
                    if let Some(pos) = self.spe_window_pos {
                        spe_vb = spe_vb.with_position(egui::pos2(pos[0] / z, pos[1] / z));
                    }
                    self.spe_window_init_applied = true;
                }
                let mut spe_closed = false;
                ctx.show_viewport_immediate(
                    ViewportId::from_hash_of("spe_expert_control"),
                    spe_vb,
                    |ctx, _class| {
                        // Back to SYSTEM points on the way in (see the builder above).
                        let z = ctx.zoom_factor().max(0.01);
                        if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
                            self.spe_window_pos = Some([rect.left() * z, rect.top() * z]);
                        }
                        if let Some(rect) = ctx.input(|i| i.viewport().inner_rect) {
                            self.spe_window_size = Some([rect.width() * z, rect.height() * z]);
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
                    // Geometry is stored in SYSTEM points so a saved layout survives
                    // a change of UI scale; a ViewportBuilder wants EGUI points, which
                    // egui-winit scales by zoom_factor. Identical at 100%, a factor
                    // `zoom` out at anything else.
                    let z = ctx.zoom_factor().max(0.01);
                    rf2k_vb = rf2k_vb.with_inner_size([rf2k_sz[0] / z, rf2k_sz[1] / z]);
                    if let Some(pos) = self.rf2k_window_pos {
                        rf2k_vb = rf2k_vb.with_position(egui::pos2(pos[0] / z, pos[1] / z));
                    }
                    self.rf2k_window_init_applied = true;
                }
                let mut rf2k_closed = false;
                ctx.show_viewport_immediate(
                    ViewportId::from_hash_of("rf2k_control"),
                    rf2k_vb,
                    |ctx, _class| {
                        // Back to SYSTEM points on the way in (see the builder above).
                        let z = ctx.zoom_factor().max(0.01);
                        if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
                            self.rf2k_window_pos = Some([rect.left() * z, rect.top() * z]);
                        }
                        if let Some(rect) = ctx.input(|i| i.viewport().inner_rect) {
                            self.rf2k_window_size = Some([rect.width() * z, rect.height() * z]);
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
                    // Geometry is stored in SYSTEM points; a ViewportBuilder wants
                    // EGUI points, which egui-winit scales by zoom_factor.
                    let z = ctx.zoom_factor().max(0.01);
                    ub_vb = ub_vb.with_inner_size([ub_sz[0] / z, ub_sz[1] / z]);
                    if let Some(pos) = self.ultrabeam_window_pos {
                        ub_vb = ub_vb.with_position(egui::pos2(pos[0] / z, pos[1] / z));
                    }
                    self.ultrabeam_window_init_applied = true;
                }
                let mut ub_closed = false;
                ctx.show_viewport_immediate(
                    ViewportId::from_hash_of("ultrabeam_control"),
                    ub_vb,
                    |ctx, _class| {
                        // Back to SYSTEM points on the way in (see the builder above).
                        let z = ctx.zoom_factor().max(0.01);
                        if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
                            self.ultrabeam_window_pos = Some([rect.left() * z, rect.top() * z]);
                        }
                        if let Some(rect) = ctx.input(|i| i.viewport().inner_rect) {
                            self.ultrabeam_window_size = Some([rect.width() * z, rect.height() * z]);
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

        // Rotor secondary window. Title follows the active backend so
        // the operator immediately sees which driver is working under the hood.
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
                    // Geometry is stored in SYSTEM points so a saved layout survives
                    // a change of UI scale; a ViewportBuilder wants EGUI points, which
                    // egui-winit scales by zoom_factor. Identical at 100%, a factor
                    // `zoom` out at anything else.
                    let z = ctx.zoom_factor().max(0.01);
                    rotor_vb = rotor_vb.with_inner_size([rotor_sz[0] / z, rotor_sz[1] / z]);
                    if let Some(pos) = self.rotor_window_pos {
                        rotor_vb = rotor_vb.with_position(egui::pos2(pos[0] / z, pos[1] / z));
                    }
                    self.rotor_window_init_applied = true;
                }
                let mut rotor_closed = false;
                ctx.show_viewport_immediate(
                    ViewportId::from_hash_of("rotor_control"),
                    rotor_vb,
                    |ctx, _class| {
                        // Back to SYSTEM points on the way in (see the builder above).
                        let z = ctx.zoom_factor().max(0.01);
                        if let Some(rect) = ctx.input(|i| i.viewport().outer_rect) {
                            self.rotor_window_pos = Some([rect.left() * z, rect.top() * z]);
                        }
                        if let Some(rect) = ctx.input(|i| i.viewport().inner_rect) {
                            self.rotor_window_size = Some([rect.width() * z, rect.height() * z]);
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
                            ui.label(rust_i18n::t!("srv_about_tagline").to_string());
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
                            if ui.button(rust_i18n::t!("srv_close").to_string()).clicked() {
                                self.show_about = false;
                            }
                        });
                    });
                });
        }

        // "Vensters schikken" matrix. Only meaningful in Running mode (the popouts
        // exist only then); on returning to Settings the window closes.
        if matches!(self.mode, Mode::Running) {
            self.render_layout_arranger(ctx);
        } else {
            self.show_layout_arranger = false;
        }
    }
}
