// SPDX-License-Identifier: GPL-2.0-or-later
//! Construction / initial state-wiring for `ServerApp`: the `new()` constructor
//! that loads config, seeds device/hardware lists and builds the initial struct
//! literal. Extracted verbatim from `ui/mod.rs` - pure relocation, no behaviour
//! change.
//!
//! The `ServerApp` struct definition itself stays in `ui/mod.rs`: its private
//! fields are read across the whole `ui` module tree, so moving the definition
//! here would force `pub(super)` on every field. A child module (this one) can
//! still build the parent's private struct literal, so `new()` relocates cleanly
//! without touching field visibility.

use super::*;

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
            tci_addr: config.tci_addr.unwrap_or_default(),
            rx2_present: config.rx2_present,
            thetis_path: config.thetis_path.unwrap_or_default(),
            yaesu_port: config.yaesu_port.unwrap_or_default(),
            yaesu_audio_device: config.yaesu_audio_device.unwrap_or_default(),
            yaesu_audio_output_device: config.yaesu_audio_output_device.unwrap_or_default(),
            yaesu_enabled: config.yaesu_enabled,
            yaesu_ssb_switch_on_ptt: config.yaesu_ssb_switch_on_ptt,
            ftx1_memory_write_ack: config.ftx1_memory_write_ack,
            yaesu2_port: config.yaesu2_port.clone().unwrap_or_default(),
            yaesu2_audio_device: config.yaesu2_audio_device.clone().unwrap_or_default(),
            yaesu2_audio_output_device: config.yaesu2_audio_output_device.clone().unwrap_or_default(),
            yaesu2_enabled: config.yaesu2_enabled,
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
            amplitec_max_w: config.amplitec_max_w,
            amplitec_tx_blocked: config.amplitec_tx_blocked,
            amplitec_log: VecDeque::new(),
            last_switch_a: 0,
            last_switch_b: 0,
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
            // ultrabeam_show_menu initialized below - load from config
            ultrabeam_window_pos: config.ultrabeam_window_pos,
            ultrabeam_show_menu: config.ultrabeam_show_menu,
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
            rotor_backend: config.rotor_backend,
            pstrotator_host: config.pstrotator_host,
            pstrotator_port: config.pstrotator_port,
            pstrotator_feedback_port: config.pstrotator_feedback_port,
            pstrotator_has_elevation: config.pstrotator_has_elevation,
            pstrotator_listen_enabled: config.pstrotator_listen_enabled,
            pstrotator_listen_port: config.pstrotator_listen_port,
            tuner_window_init_applied: false,
            amplitec_window_init_applied: false,
            spe_window_init_applied: false,
            rf2k_window_init_applied: false,
            ultrabeam_window_init_applied: false,
            rotor_window_init_applied: false,
            dxcluster_server: config.dxcluster_server.clone(),
            dxcluster_callsign: config.dxcluster_callsign.clone(),
            dxcluster_enabled: config.dxcluster_enabled,
            dxcluster_expiry_min: config.dxcluster_expiry_min,
            password: config.password.clone().unwrap_or_default(),
            totp_enabled: config.totp_enabled,
            friendly_name: config.friendly_name.clone().unwrap_or_default(),
            relay_enabled: config.relay_enabled,
            relay_url: config.relay_url.clone(),
            relay_station: config.relay_station.clone(),
            relay_token: config.relay_token.clone(),
            relay_udp_enabled: config.relay_udp_enabled,
            totp_secret: config.totp_secret.clone().unwrap_or_else(|| sdr_remote_core::auth::generate_totp_secret()),
            main_window_pos: config.main_window_pos,
            theme_variant: sdr_remote_theme::ThemeVariant::from_str(&config.theme),
            theme_custom: sdr_remote_theme::Palette::from_config_string(&config.theme_custom)
                .unwrap_or_else(sdr_remote_theme::Palette::slate),
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
            chat: sdr_remote_chat::ChatPanel::default(),
            show_chat_window: config.show_chat_window,
            chat_window_pos: config.chat_window_pos,
            chat_window_size: config.chat_window_size,
            chat_window_init_applied: false,
            status_panel_state: None,
            status_bind_addr: format!("0.0.0.0:{}", sdr_remote_core::DEFAULT_PORT),
            status_view: StatusView::Status,
            show_layout_arranger: false,
            layout_grid_per_monitor:
                sdr_remote_layout::layout_grids_from_config(&config.layout_grids),
            layout_pending: Vec::new(),
            layout_memories: {
                let mut v: Vec<LayoutMemory> = config.layout_memories.iter()
                    .map(|s| LayoutMemory::from_config_string(s)).collect();
                v.resize_with(LAYOUT_MEM_SLOTS, LayoutMemory::default);
                v
            },
            ui_zoom: config.ui_zoom,
            ui_zoom_pending: true,
            layout_active_item: None,
            layout_drag_anchor: None,
            layout_target_monitor: 0,
            ui_language: config.language.clone(),
        }
    }

}
