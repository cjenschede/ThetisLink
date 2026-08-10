// SPDX-License-Identifier: GPL-2.0-or-later
//! Construction / initial state-wiring for `SdrRemoteApp`: the `new()`
//! constructor that loads config, spins up the relay monitor, seeds device
//! lists and MIDI, and builds the initial struct literal. Extracted verbatim
//! from `ui/mod.rs` - pure relocation, no behaviour change.
//!
//! The `SdrRemoteApp` struct definition itself stays in `ui/mod.rs`: its ~880
//! private fields are read across the whole `ui` module tree, so moving the
//! definition here would force `pub(super)` on every field. A child module
//! (this one) can still build the parent's private struct literal, which is
//! why `new()` relocates cleanly without touching field visibility.

use super::*;

impl SdrRemoteApp {
    pub fn new(
        state_rx: watch::Receiver<RadioState>,
        cmd_tx: mpsc::UnboundedSender<Command>,
        log_buffer: LogBuffer,
        // Phase C: if the relay is used as transport the monitor runs in
        // main.rs; then we only show its status (no second relay connection).
        external_relay_status: Option<sdr_remote_relay::RelayStatusHandle>,
    ) -> Self {
        let config = load_config();

        let input_devices = crate::audio::list_input_devices();
        let output_devices = crate::audio::list_output_devices();
        let relay_initial = sdr_remote_relay::RelayConfig {
            enabled: config.relay_enabled,
            url: config.relay_url.clone(),
            station: config.relay_station.clone(),
            token: config.relay_token.clone(),
            role: sdr_remote_relay::RelayRole::Client,
            instance: config.relay_instance_id.clone(),
            name: config.relay_device_name.clone(),
            udp_port: sdr_remote_relay::relay_udp_port_resolve(config.relay_udp_enabled),
        };
        let (relay_monitor, relay_status, relay_external) = if let Some(ext) = external_relay_status {
            // Relay transport: monitor runs in main.rs. Show only status; no
            // own monitor (there may be only one relay connection per client).
            (None, Some(ext), true)
        } else if relay_initial.enabled {
            let m = sdr_remote_relay::RelayMonitor::start_threaded(relay_initial);
            let s = m.status_handle();
            (Some(m), Some(s), false)
        } else {
            (None, None, false)
        };

        // Send initial device selections to engine
        if !config.input_device.is_empty() {
            let _ = cmd_tx.send(Command::SetInputDevice(config.input_device.clone()));
        }
        if !config.output_device.is_empty() {
            let _ = cmd_tx.send(Command::SetOutputDevice(config.output_device.clone()));
        }
        let _ = cmd_tx.send(Command::SetRxVolume(config.rx_volume));
        let _ = cmd_tx.send(Command::SetTxGain(config.tx_gain));
        let _ = cmd_tx.send(Command::SetVfoAVolume(config.vfo_a_volume));
        let _ = cmd_tx.send(Command::SetPlayVolume(config.play_volume));
        let _ = cmd_tx.send(Command::SetVfoBVolume(config.vfo_b_volume));
        let _ = cmd_tx.send(Command::SetLocalVolume(config.local_volume));
        let _ = cmd_tx.send(Command::SetRx2Volume(config.rx2_volume));
        let _ = cmd_tx.send(Command::SetThetisWidebandAudio(config.thetis_wideband_audio));
        let _ = cmd_tx.send(Command::SetFullSpectrumEnabled(config.full_spectrum_enabled));
        // Restore EQ + mic gain from active profile (radio 1)
        if let Some((_, en, gains, mic_gain)) = config.yaesu_eq_profiles.iter()
            .find(|(n, _, _, _)| n == &config.yaesu_eq_active) {
            let _ = cmd_tx.send(Command::SetYaesuEqEnabled(*en));
            for i in 0..5 { let _ = cmd_tx.send(Command::SetYaesuEqBand(i as u8, gains[i])); }
            let _ = cmd_tx.send(Command::SetYaesuTxGain(*mic_gain));
        }
        // Same for radio 2 (FTX-1): the engine gets the loaded EQ + mic-gain.
        if let Some((_, en, gains, mic_gain)) = config.yaesu2_eq_profiles.iter()
            .find(|(n, _, _, _)| n == &config.yaesu2_eq_active) {
            let _ = cmd_tx.send(Command::SetYaesu2EqEnabled(*en));
            for i in 0..5 { let _ = cmd_tx.send(Command::SetYaesu2EqBand(i as u8, gains[i])); }
            let _ = cmd_tx.send(Command::SetYaesu2TxGain(*mic_gain));
        }
        let _ = cmd_tx.send(Command::SetAgcEnabled(config.agc_enabled));
        // RX1 audio default ON on the server; only send if the client wants it OFF.
        if !config.rx1_enabled {
            let _ = cmd_tx.send(Command::SetRx1Enabled(false));
        }
        if config.rx2_enabled {
            let _ = cmd_tx.send(Command::SetRx2Enabled(true));
        }
        // RX2 spectrum subscription separate from the audio (phase 4).
        if config.rx2_spectrum_enabled {
            let _ = cmd_tx.send(Command::EnableRx2Spectrum(true));
        }
        if config.spectrum_enabled {
            let _ = cmd_tx.send(Command::EnableSpectrum(true));
        }
        if config.spectrum_max_bins != sdr_remote_core::DEFAULT_SPECTRUM_BINS as u16 {
            let _ = cmd_tx.send(Command::SetSpectrumMaxBins(config.spectrum_max_bins));
            let _ = cmd_tx.send(Command::SetControl(ControlId::Rx2SpectrumMaxBins, config.spectrum_max_bins));
        }
        if config.spectrum_fft_size_k != 0 {
            let _ = cmd_tx.send(Command::SetSpectrumFftSize(config.spectrum_fft_size_k));
        }
        if config.rx2_spectrum_fft_size_k != 0 {
            let _ = cmd_tx.send(Command::SetControl(ControlId::Rx2SpectrumFftSize, config.rx2_spectrum_fft_size_k));
        }

        let mut app = Self {
            state_rx,
            cmd_tx,
            ui_event_sink: std::sync::Arc::new(controls::TracingSink),
            server_input: config.server.clone(),
            password_input: config.password.clone(),
            totp_input: String::new(),
            mouse_ptt: false,
            ptt_toggle_mode: config.ptt_toggle,
            yaesu_ptt_toggle_mode: config.yaesu_ptt_toggle,
            yaesu_mouse_ptt: false,
            yaesu_ptt_last_sent: false,
            yaesu2_ptt_last_sent: false,
            spike_protection: config.spike_protection,
            mic_gate_delay_thetis_ms: config.mic_gate_delay_thetis_ms,
            mic_gate_delay_yaesu_ms: config.mic_gate_delay_yaesu_ms,
            recording: false,
            playing: false,
            rec_rx1: true,
            rec_rx2: false,
            rec_yaesu: false,
            rec_yaesu2: false,
            rec_vrx1: false,
            rec_vrx2: false,
            last_recorded_path: None,
            midi_ptt_toggle_mode: config.midi_ptt_toggle,
            smeter_source: config.smeter_source,
            thetis_autostart: config.thetis_autostart,
            thetis_autostart_fired: false,
            allow_zoom_below_2x: config.allow_zoom_below_2x,
            ui_language: config.language.clone(),
            reboot_confirm: false,
            diversity_enabled: false,
            diversity_state_read: false,
            diversity_ref: 1,       // RX1 as default reference
            diversity_source: 0,    // RX1+RX2
            audio_mode: 0,          // Mono
            diversity_gain_rx1: 1.5,
            diversity_gain_rx2: 1.5,
            diversity_gain_multi: 5.0,
            diversity_phase_lock: false,
            diversity_gain_lock: false,
            diversity_auto_active: false,
            diversity_auto_step: 0,
            diversity_auto_round: 0,
            diversity_auto_best_phase: 0.0,
            diversity_auto_best_gain: 1.0,
            diversity_auto_best_smeter: 999.0,
            diversity_auto_last_set: Instant::now(),
            diversity_auto_start_smeter: 0.0,
            diversity_auto_overall_best: 999.0,
            diversity_auto_result: 0,
            diversity_auto_improvement_db: 0.0,
            diversity_auto_slow: true,
            diversity_auto_smart: true,
            diversity_auto_ultra: false,
            diversity_auto_eq_gain_db: 0.0,
            diversity_sa_param: 0,
            diversity_sa_step: 90.0,
            diversity_sa_sub: 0,
            diversity_sa_center_smeter: 0.0,
            diversity_sa_plus_smeter: 0.0,
            diversity_sa_minus_smeter: 0.0,
            diversity_sa_iteration: 0,
            diversity_phase: 0.0,
            ddc_sample_rate_rx1: 0,
            ddc_sample_rate_rx2: 0,
            midi_ptt: false,
            freq_step_index: 3, // default 1 kHz
            memories: config.memories,
            save_mode: false,
            freq_editing: false,
            freq_edit_text: String::new(),
            tx_profiles: config.tx_profiles,
            input_devices,
            output_devices,
            device_refresh_at: Some(Instant::now()),
            selected_input: config.input_device,
            mic_profile_map: config.mic_profile_map.clone(),
            selected_output: config.output_device,
            rx_volume: config.rx_volume,
            play_volume: config.play_volume,
            vfo_a_volume: config.vfo_a_volume,
            vfo_b_volume: config.vfo_b_volume,
            local_volume: config.local_volume,
            ui_zoom: config.ui_zoom,
            ui_zoom_pending: true,
            master_volume_dirty: false,
            tx_gain: config.tx_gain,
            connected: false,
            ptt: false,
            ptt_denied: false,
            rtt_ms: 0,
            jitter_ms: 0.0,
            buffer_depth: 0,
            rx_packets: 0,
            yaesu_audio_packets: 0,
            yaesu_jitter_ms: 0.0,
            yaesu_buffer_depth: 0,
            yaesu2_audio_packets: 0,
            yaesu2_jitter_ms: 0.0,
            yaesu2_buffer_depth: 0,
            vrx1_audio_packets: 0,
            vrx1_jitter_ms: 0.0,
            vrx1_buffer_depth: 0,
            vrx2_audio_packets: 0,
            vrx2_jitter_ms: 0.0,
            vrx2_buffer_depth: 0,
            down_kbps: 0,
            up_kbps: 0,
            bw_breakdown: Vec::new(),
            bw_breakdown_expanded: config.bw_breakdown_expanded,
            loss_percent: 0,
            capture_level: 0.0,
            playback_level: 0.0,
            playback_level_bin_r: 0.0,
            playback_level_rx2: 0.0,
            playback_level_yaesu: 0.0,
            playback_level_yaesu2: 0.0,
            yaesu_mic_level: 0.0,
            frequency_hz: 0,
            mode: 0,
            smeter: sdr_remote_logic::state::SMETER_NO_DATA_DBM,
            smeter_peak: sdr_remote_logic::state::SMETER_NO_DATA_DBM,
            smeter_peak_time: Instant::now(),
            power_on: false,
            power_press_start: None,
            shutdown_sent: false,
            thetis_tuning: false,
            tune_pa_was_operate: false,
            tune_pending_on: None,
            tune_pending_restore: None,
            tx_profile: 0,
            nr_level: 0,
            anf_on: false,
            drive_level: 0,
            audio_error: false,
            agc_enabled: config.agc_enabled,
            other_tx: false,
            thetis_swr_x100: 100,
            filter_low_hz: 0,
            filter_high_hz: 0,
            filter_changed_at: None,
            tx_filter_follow_rx: true,
            tx_filter_low_hz: 0,
            tx_filter_high_hz: 0,
            tx_filter_supported: false,
            tx_filter_initialized: false,
            last_tx_follow_sent: None,
            tx_follow_last_send_at: None,
            thetis_configured: true,
            rx2_present: true,
            thetis_starting: false,
            spectrum_enabled: config.spectrum_enabled,
            spectrum_bins: Vec::new(),
            spectrum_center_hz: 0,
            spectrum_span_hz: 0,
            spectrum_ref_level: 0,
            spectrum_db_per_unit: 1,
            last_spectrum_seq: 0,
            full_spectrum_bins: Vec::new(),
            full_spectrum_center_hz: 0,
            full_spectrum_span_hz: 0,
            full_spectrum_sequence: 0,
            spectrum_ref_db: config.spectrum_ref_db,
            spectrum_range_db: config.spectrum_range_db,
            spectrum_zoom: 32.0,
            spectrum_pan: 0.0,
            last_sent_zoom: 32.0,
            last_sent_pan: 0.0,
            zoom_pan_changed_at: None,
            pending_freq: None,
            pending_freq_at: None,
            rx2_pending_freq: None,
            rx2_pending_freq_at: None,
            yaesu_pending_freq: None,
            yaesu_pending_freq_at: None,
            yaesu2_pending_freq: None,
            yaesu2_pending_freq_at: None,
            rx1_force_full_tuning: false,
            rx2_force_full_tuning: false,
            waterfall: WaterfallRingBuffer::new(200),
            waterfall_contrast: config.waterfall_contrast,
            auto_ref_enabled: config.auto_ref_enabled,
            tx_spectrum_saved_ref_db: None,
            tx_spectrum_saved_range: None,
            tx_spectrum_saved_auto_ref: None,
            tx_spectrum_restore_auto_at: None,
            wf_contrast_per_band: config.wf_contrast_per_band,
            band_mem: config.band_mem,
            current_band: None,
            spectrum_max_bins: config.spectrum_max_bins,
            spectrum_fft_size_k: config.spectrum_fft_size_k,
            rx2_spectrum_fft_size_k: config.rx2_spectrum_fft_size_k,
            spectrum_total_h: config.spectrum_total_h,
            spectrum_popout: config.spectrum_popout,
            window_w: config.window_w,
            window_h: config.window_h,
            main_geom_dirty: false,
            log_buffer,
            show_log: false,
            show_about: false,
            vrx1_popout: false,
            vrx2_popout: false,
            show_layout_arranger: false,
            layout_grid_per_monitor: arranger::layout_grids_from_config(&config.layout_grids),
            layout_pending: Vec::new(),
            layout_memories: {
                let mut v: Vec<LayoutMemory> = config.layout_memories.iter()
                    .map(|s| LayoutMemory::from_config_string(s)).collect();
                v.resize_with(LAYOUT_MEM_SLOTS, LayoutMemory::default);
                v
            },
            layout_active_item: None,
            layout_drag_anchor: None,
            layout_target_monitor: 0,
            vrx1_enabled: config.vrx1_enabled.unwrap_or(false),
            vrx1_freq_hz: config.vrx1_freq_hz.unwrap_or(0),
            vrx1_mode: config.vrx1_mode.unwrap_or(0),
            vrx1_volume: config.vrx1_volume.unwrap_or(1.0),
            vrx2_enabled: config.vrx2_enabled.unwrap_or(false),
            vrx2_freq_hz: config.vrx2_freq_hz.unwrap_or(0),
            vrx2_mode: config.vrx2_mode.unwrap_or(0),
            vrx2_volume: config.vrx2_volume.unwrap_or(1.0),
            vrx1_auto_tune: false,
            vrx2_auto_tune: false,
            vrx_rate_mode: 2, // default Auto
            vrx_rate_mode2: 2, // default Auto (VRX2, per-client per-VRX rate)
            last_vrx1_autotune_hz: 0,
            last_vrx2_autotune_hz: 0,
            vrx_popout_pos: config.vrx_popout_pos.map(|(x, y)| egui::pos2(x, y)),
            vrx_popout_size: config.vrx_popout_size.map(|(w, h)| egui::vec2(w, h)),
            vrx_popout_init_applied: false,
            vrx2_popout_pos: config.vrx2_popout_pos.map(|(x, y)| egui::pos2(x, y)),
            vrx2_popout_size: config.vrx2_popout_size.map(|(w, h)| egui::vec2(w, h)),
            vrx2_popout_init_applied: false,
            playback_level_vrx1: 0.0,
            playback_level_vrx2: 0.0,
            vrx1_freq_by_bucket: std::collections::HashMap::new(),
            vrx2_freq_by_bucket: std::collections::HashMap::new(),
            last_vrx1_ddc_center_hz: 0,
            last_vrx2_ddc_center_hz: 0,
            vrx1_spectrum_zoom: config.vrx1_spectrum_zoom.unwrap_or(32.0),
            vrx2_spectrum_zoom: config.vrx2_spectrum_zoom.unwrap_or(32.0),
            vrx1_ref_db: config.vrx1_ref_db.unwrap_or(-20.0),
            vrx1_range_db: config.vrx1_range_db.unwrap_or(100.0),
            vrx1_wf_contrast: config.vrx1_wf_contrast.unwrap_or(1.0),
            vrx1_pan: config.vrx1_pan.unwrap_or(0.0),
            vrx1_auto_ref: config.vrx1_auto_ref.unwrap_or(false),
            // If user has persisted a zoom we trust it; otherwise let
            // first DDC-span arrival pick default_zoom_for_span.
            vrx1_zoom_initialized: config.vrx1_spectrum_zoom.is_some(),
            vrx1_filter_low_hz: config.vrx1_filter_low_hz.unwrap_or_else(|| {
                if config.vrx1_mode.unwrap_or(0) == 1 { -3000 } else { 0 }
            }),
            vrx1_filter_high_hz: config.vrx1_filter_high_hz.unwrap_or_else(|| {
                if config.vrx1_mode.unwrap_or(0) == 1 { 0 } else { 3000 }
            }),
            vrx1_high_res_spectrum: config.vrx1_high_res_spectrum.unwrap_or(false),
            vrx1_high_res_last_span_khz: 0,
            rx1_spectrum: ChannelSpectrum::new(ChannelId::Rx1),
            rx2_spectrum: ChannelSpectrum::new(ChannelId::Rx2),
            vrx1_spectrum: ChannelSpectrum::new(ChannelId::Vrx1),
            vrx2_ref_db: config.vrx2_ref_db.unwrap_or(-20.0),
            vrx2_range_db: config.vrx2_range_db.unwrap_or(100.0),
            vrx2_wf_contrast: config.vrx2_wf_contrast.unwrap_or(1.0),
            vrx2_pan: config.vrx2_pan.unwrap_or(0.0),
            vrx2_auto_ref: config.vrx2_auto_ref.unwrap_or(false),
            vrx2_zoom_initialized: config.vrx2_spectrum_zoom.is_some(),
            vrx2_filter_low_hz: config.vrx2_filter_low_hz.unwrap_or_else(|| {
                if config.vrx2_mode.unwrap_or(0) == 1 { -3000 } else { 0 }
            }),
            vrx2_filter_high_hz: config.vrx2_filter_high_hz.unwrap_or_else(|| {
                if config.vrx2_mode.unwrap_or(0) == 1 { 0 } else { 3000 }
            }),
            vrx2_high_res_spectrum: config.vrx2_high_res_spectrum.unwrap_or(false),
            vrx2_high_res_last_span_khz: 0,
            vrx2_spectrum: ChannelSpectrum::new(ChannelId::Vrx2),
            vrx1_waterfall_texture: None,
            vrx2_waterfall_texture: None,
            vrx_state_sync_pending: true,
            active_tab: Tab::Radio,
            last_connect_status: sdr_remote_logic::state::ConnectStatus::Disconnected,
            // PATCH-3: kick off the mDNS browse on app start. Daemon-init failure
            // is caught inside `BrowseHandle::start` and surfaces as an empty
            // dropdown - manual IP entry stays available.
            mdns_browse: Some(crate::mdns::BrowseHandle::start()),
            relay_enabled: config.relay_enabled,
            relay_url: config.relay_url.clone(),
            relay_station: config.relay_station.clone(),
            relay_token: config.relay_token.clone(),
            relay_instance_id: config.relay_instance_id.clone(),
            relay_device_name: config.relay_device_name.clone(),
            relay_udp_enabled: config.relay_udp_enabled,
            relay_monitor,
            relay_status,
            relay_external,
            // PATCH-4: arm the first-run wizard if the user has never had a
            // successful connect (counter==0). Seeds with whatever
            // server/password the config already has - wizard still walks
            // through the steps but the fields are pre-populated.
            wizard_state: if crate::ui::config::is_first_run() {
                Some(wizard::WizardState::new(
                    config.server.clone(),
                    config.password.clone(),
                ))
            } else {
                None
            },
            device_tab: config.device_tab,
            amplitec_available: false,
            amplitec_connected: false,
            amplitec_switch_a: 0,
            amplitec_switch_b: 0,
            amplitec_labels: String::new(),
            amplitec_log: VecDeque::new(),
            amplitec_power_max_w: [0; 6],
            amplitec_power_tx_blocked: [false; 6],
            amplitec_power_loaded: false,
            amplitec_power_show: config.amplitec_power_show,
            websdr_favorite_editing: None,
            tuner_available: false,
            tuner_connected: false,
            tuner_state: 0,
            tuner_can_tune: false,
            tuner_tune_freq: 0,
            spe_connected: false,
            spe_state: 0,
            spe_band: 0,
            spe_ptt: false,
            spe_power_w: 0,
            spe_swr_x10: 10,
            spe_temp: 0,
            spe_warning: b'N',
            spe_alarm: b'N',
            spe_power_level: 0,
            spe_antenna: 0,
            spe_input: 0,
            spe_voltage_x10: 0,
            spe_current_x10: 0,
            spe_atu_bypassed: false,
            spe_available: false,
            spe_active: false,
            spe_peak_power: 0,
            spe_peak_time: Instant::now(),
            rf2k_connected: false,
            rf2k_operate: false,
            rf2k_band: 0,
            rf2k_frequency_khz: 0,
            rf2k_temperature_x10: 0,
            rf2k_voltage_x10: 0,
            rf2k_current_x10: 0,
            rf2k_forward_w: 0,
            rf2k_reflected_w: 0,
            rf2k_swr_x100: 100,
            rf2k_max_forward_w: 0,
            rf2k_max_reflected_w: 0,
            rf2k_max_swr_x100: 100,
            rf2k_error_state: 0,
            rf2k_error_text: String::new(),
            rf2k_antenna_type: 0,
            rf2k_antenna_number: 1,
            rf2k_tuner_mode: 0,
            rf2k_tuner_setup: String::new(),
            rf2k_tuner_l_nh: 0,
            rf2k_tuner_c_pf: 0,
            rf2k_drive_w: 0,
            rf2k_modulation: String::new(),
            rf2k_max_power_w: 0,
            rf2k_device_name: String::new(),
            rf2k_available: false,
            rf2k_active: false,
            rf2k_peak_power: 0,
            rf2k_peak_time: Instant::now(),
            rf2k_debug_available: false,
            rf2k_bias_pct_x10: 0,
            rf2k_psu_source: 0,
            rf2k_uptime_s: 0,
            rf2k_tx_time_s: 0,
            rf2k_error_count: 0,
            rf2k_error_history: Vec::new(),
            rf2k_storage_bank: 0,
            rf2k_hw_revision: String::new(),
            rf2k_frq_delay: 0,
            rf2k_autotune_threshold_x10: 0,
            rf2k_dac_alc: 0,
            rf2k_high_power: false,
            rf2k_tuner_6m: false,
            rf2k_band_gap_allowed: false,
            rf2k_controller_version: 0,
            rf2k_drive_config_ssb: [0; 11],
            rf2k_drive_config_am: [0; 11],
            rf2k_drive_config_cont: [0; 11],
            rf2k_show_debug: false,
            rf2k_show_drive_config: false,
            rf2k_confirm_high_power: false,
            rf2k_confirm_zero_fram: false,
            rf2k_drive_edit: [[0; 11]; 3],
            rf2k_drive_loaded: false,
            rf2k_confirm_fw_close: false,
            ub_connected: false,
            ub_frequency_khz: 0,
            ub_band: 0,
            ub_direction: 0,
            ub_off_state: true,
            ub_motors_moving: 0,
            ub_motor_completion: 0,
            ub_fw_major: 0,
            ub_fw_minor: 0,
            ub_available: false,
            ub_elements_mm: [0; 6],
            ub_operation: 0,
            ub_freq_min_mhz: 0,
            ub_freq_max_mhz: 0,
            ub_confirm_retract: false,
            ub_auto_track: false,
            ub_last_auto_khz: 0,
            rotor_connected: false,
            rotor_angle_x10: 0,
            rotor_rotating: false,
            rotor_target_x10: 0,
            rotor_available: false,
            yaesu_connected: false,
            yaesu_freq_a: 0,
            yaesu_freq_b: 0,
            yaesu_mode: 1,
            yaesu_smeter: 0,
            yaesu_smeter_peak: 0,
            yaesu_smeter_peak_time: Instant::now(),
            yaesu_tx_active: false,
            yaesu_power_on: false,
            yaesu_volume: config.yaesu_volume,
            yaesu_model: 0,
            yaesu2_model: 1,
            yaesu2_connected: false,
            // Optimistic presence: show a Yaesu that was present last session at once
            // (pre-connect); the server prunes it on connect if it is (no longer) there.
            yaesu_present_last: config.yaesu_present_last,
            yaesu2_present_last: config.yaesu2_present_last,
            yaesu2_freq_a: 0,
            yaesu2_freq_b: 0,
            yaesu2_mode: 1,
            yaesu2_smeter: 0,
            yaesu2_smeter_peak: 0,
            yaesu2_smeter_peak_time: Instant::now(),
            yaesu2_tx_active: false,
            yaesu2_power_on: false,
            yaesu2_split: false,
            yaesu2_scan: false,
            yaesu2_vfo_select: 0,
            yaesu2_memory_channel: 0,
            yaesu2_tuner_state: 0,
            yaesu2_hi_swr: false,
            yaesu2_feature_toggles: 0,
            yaesu2_feature_levels: [0u8; 16],
            yaesu2_squelch: 0,
            yaesu2_rf_gain: 0,
            yaesu2_rf_power: 0,
            // Standalone persistent mic-gain (last slider value), separate from
            // the EQ profile so the value is always remembered.
            yaesu2_mic_gain: config.yaesu2_mic_gain,
            yaesu2_eq_enabled: config.yaesu2_eq_profiles.iter()
                .find(|(n, _, _, _)| n == &config.yaesu2_eq_active)
                .map(|(_, e, _, _)| *e).unwrap_or(false),
            yaesu2_eq_gains: config.yaesu2_eq_profiles.iter()
                .find(|(n, _, _, _)| n == &config.yaesu2_eq_active)
                .map(|(_, _, g, _)| *g).unwrap_or([0.0; 5]),
            yaesu2_eq_profiles: config.yaesu2_eq_profiles.clone(),
            yaesu2_eq_active_profile: config.yaesu2_eq_active.clone(),
            yaesu2_eq_new_name: String::new(),
            collapse_yaesu2_eq: false,
            collapse_yaesu2_memories: config.collapse_yaesu2_memories,
            yaesu2_control_changed_at: None,
            yaesu2_volume: config.yaesu2_volume,
            yaesu2_enabled: config.yaesu2_enabled,
            yaesu2_ptt_toggle_mode: config.yaesu2_ptt_toggle,
            yaesu2_enable_sent: false,
            yaesu2_autoread_at: None,
            yaesu2_hf_swap_at: None,
            yaesu2_popout: config.yaesu2_popout,
            yaesu2_popout_pos: config.yaesu2_popout_pos.map(|(x, y)| egui::pos2(x, y)),
            yaesu2_popout_size: config.yaesu2_popout_size.map(|(w, h)| egui::vec2(w, h)),
            yaesu2_popout_init_applied: false,
            yaesu2_mouse_ptt: false,
            yaesu_popout: config.yaesu_popout,
            yaesu_popout_pos: config.yaesu_popout_pos.map(|(x, y)| egui::pos2(x, y)),
            yaesu_popout_size: config.yaesu_popout_size.map(|(w, h)| egui::vec2(w, h)),
            spectrum_popout_pos: config.spectrum_popout_pos.map(|(x, y)| egui::pos2(x, y)),
            spectrum_popout_size: config.spectrum_popout_size.map(|(w, h)| egui::vec2(w, h)),
            rx2_popout_pos: config.rx2_popout_pos.map(|(x, y)| egui::pos2(x, y)),
            rx2_popout_size: config.rx2_popout_size.map(|(w, h)| egui::vec2(w, h)),
            popout_joined_pos: config.popout_joined_pos.map(|(x, y)| egui::pos2(x, y)),
            popout_joined_size: config.popout_joined_size.map(|(w, h)| egui::vec2(w, h)),
            spectrum_popout_init_applied: false,
            rx2_popout_init_applied: false,
            popout_joined_init_applied: false,
            yaesu_popout_init_applied: false,
            yaesu_popout_first_frame: true,
            yaesu_enable_sent: false,
            yaesu_state_sent: None,
            yaesu2_state_sent: None,
            // Standalone persistent mic-gain (last slider value), separate from
            // the EQ profile so the value is always remembered.
            yaesu_mic_gain: config.yaesu_mic_gain,
            yaesu_compressor: config.yaesu_compressor,
            yaesu2_compressor: config.yaesu2_compressor,
            yaesu_tx_agc: config.yaesu_tx_agc,
            yaesu2_tx_agc: config.yaesu2_tx_agc,
            yaesu_eq_enabled: {
                // Load active EQ profile from config
                config.yaesu_eq_profiles.iter()
                    .find(|(n, _, _, _)| n == &config.yaesu_eq_active)
                    .map(|(_, e, _, _)| *e).unwrap_or(false)
            },
            yaesu_eq_gains: {
                config.yaesu_eq_profiles.iter()
                    .find(|(n, _, _, _)| n == &config.yaesu_eq_active)
                    .map(|(_, _, g, _)| *g).unwrap_or([0.0; 5])
            },
            yaesu_eq_profiles: config.yaesu_eq_profiles.clone(),
            yaesu_eq_active_profile: config.yaesu_eq_active.clone(),
            yaesu_eq_new_name: String::new(),
            yaesu_squelch: 0,
            yaesu_rf_gain: 255,
            yaesu_radio_mic_gain: 50,
            yaesu_rf_power: 50,
            yaesu_tx_power_max: 100,
            yaesu2_tx_power_max: 100,
            yaesu_power_pending: None,
            yaesu_power_pending_at: None,
            yaesu2_power_pending: None,
            yaesu2_power_pending_at: None,
            yaesu_scan_active: false,
            yaesu_split_active: false,
            yaesu_tuner_state: 0,
            yaesu_hi_swr: false,
            yaesu_feature_toggles: 0,
            yaesu_feature_levels: [0u8; 16],
            yaesu_level_sliders: [[0i32; 4]; 2],
            yaesu_freq_sliders: [[0i32; 3]; 2],
            yaesu_clar_offset: 0,
            yaesu2_clar_offset: 0,
            tune_step_hz: 1_000,
            yaesu_in_memory_mode: false,
            yaesu_current_mem_ch: None,
            yaesu_mem_active_ch: None,
            yaesu2_mem_active_ch: None,
            yaesu_mem_active_live: false,
            yaesu2_mem_active_live: false,
            yaesu_enabled: config.yaesu_enabled,
            // Starts EMPTY, like slot 1. It used to load the saved file here, which
            // meant slot 0 always showed a list and slot 1 never did - and a list from
            // disk is indistinguishable on screen from one read out of the radio. That
            // masked a fault where the pushed list never arrived (build 20), and it is
            // why the 991A appeared to have tones at startup while the FTX-1 did not:
            // those came from the file. The server pushes the real list within a second
            // of connecting; "Load file" still loads one on request.
            yaesu_mem_channels: Vec::new(),
            yaesu_mem_file: config.yaesu_mem_file.clone(),
            yaesu_mem_selected: None,
            yaesu_mem_filter: String::new(),
            yaesu_mem_dirty: false,
            yaesu_mem_push_deferred: false,
            yaesu_mem_expect_push: false,
            yaesu2_mem_expect_push: false,
            yaesu2_mem_push_deferred: false,
            yaesu_mem_radio_received: false,
            yaesu_mem_blob_hash: None,
            yaesu2_mem_channels: Vec::new(),
            yaesu2_mem_file: "ftx1_memories.tab".to_string(),
            yaesu2_mem_selected: None,
            yaesu2_mem_filter: String::new(),
            yaesu2_mem_dirty: false,
            yaesu2_mem_radio_received: false,
            yaesu2_mem_blob_hash: None,
            yaesu_mem_active_slot: 0,
            yaesu_menu_items: Vec::new(),
            yaesu_menu_received: false,
            yaesu_menu_blob_hash: None,
            yaesu2_menu_entries: Vec::new(),
            yaesu2_menu_received: false,
            yaesu2_menu_blob_hash: None,
            collapse_yaesu2_menu: config.collapse_yaesu2_menu,
            yaesu2_menu_edits: std::collections::HashMap::new(),
            yaesu2_menu_filter: String::new(),
            rotor_goto_input: String::new(),
            dx_spots: Vec::new(),
            smooth_display_center_hz: 0.0,
            rx2_smooth_display_center_hz: 0.0,
            smooth_alpha: 1.0,
            last_frame_time: Instant::now(),
            dx_spots_enabled: true,
            rx1_enabled: config.rx1_enabled,
            rx2_enabled: config.rx2_enabled,
            // Show the persisted enable state at once (optimistic); the first server
            // sync must confirm it within the grace window before the server takes
            // over. RX2 is server-default-off, so without this it flipped off then on.
            rx1_enabled_pending: Some((Instant::now(), config.rx1_enabled)),
            rx2_enabled_pending: Some((Instant::now(), config.rx2_enabled)),
            rx2_spectrum_enabled: config.rx2_spectrum_enabled,
            thetis_wideband_audio: config.thetis_wideband_audio,
            full_spectrum_enabled: config.full_spectrum_enabled,
            // RX2 spectrum only shows in the pop-out; so restore the window if
            // the RX2 spectrum subscription was on (otherwise subscribe-without-view).
            // Derived each frame from want (rx2_spectrum_enabled) && can_rx2 (model B);
            // this is only the frame-0 value before the first derivation runs.
            rx2_popout: false,
            popout_joined: config.popout_joined,
            meter_analog: config.meter_analog,
            ub_show_menu: config.ub_show_menu,
            collapse_diversity: config.collapse_diversity,
            collapse_yaesu_eq: config.collapse_yaesu_eq,
            collapse_yaesu_memories: config.collapse_yaesu_memories,
            collapse_yaesu_menu: config.collapse_yaesu_menu,
            yaesu_memories_h: config.yaesu_memories_h,
            yaesu2_memories_h: config.yaesu2_memories_h,
            main_window_pos: config.main_window_pos.map(|(x, y)| egui::pos2(x, y)),
            theme_variant: theme::ThemeVariant::from_str(&config.theme),
            theme_custom: theme::Palette::from_config_string(&config.theme_custom)
                .unwrap_or_else(theme::Palette::slate),
            theme_custom_dirty: false,
            popout_rx1_smeter_rect: egui::Rect::NOTHING,
            popout_rx2_smeter_rect: egui::Rect::NOTHING,
            rx2_volume: config.rx2_volume,
            rx2_af_gain_display: 0,
            rx2_frequency_hz: 0,
            rx2_mode: 0,
            rx2_smeter: sdr_remote_logic::state::SMETER_NO_DATA_DBM,
            rx2_smeter_peak: sdr_remote_logic::state::SMETER_NO_DATA_DBM,
            rx2_smeter_peak_time: Instant::now(),
            rx2_filter_low_hz: 0,
            rx2_filter_high_hz: 0,
            rx2_filter_changed_at: None,
            rx2_nr_level: 0,
            rx2_anf_on: false,
            rx2_freq_step_index: 3, // default 1 kHz
            rx2_freq_editing: false,
            rx2_freq_edit_text: String::new(),
            rx2_spectrum_bins: Vec::new(),
            rx2_spectrum_center_hz: 0,
            rx2_spectrum_span_hz: 0,
            rx2_last_spectrum_seq: 0,
            rx2_full_spectrum_bins: Vec::new(),
            rx2_full_spectrum_center_hz: 0,
            rx2_full_spectrum_span_hz: 0,
            rx2_full_spectrum_sequence: 0,
            rx2_spectrum_zoom: 32.0,
            rx2_spectrum_pan: 0.0,
            rx2_last_sent_zoom: 0.0,
            rx2_last_sent_pan: 0.0,
            rx2_zoom_pan_changed_at: None,
            rx2_waterfall: WaterfallRingBuffer::new(200),
            rx2_spectrum_ref_db: config.rx2_spectrum_ref_db,
            rx2_spectrum_range_db: config.rx2_spectrum_range_db,
            rx2_auto_ref_enabled: config.rx2_auto_ref_enabled,
            rx2_waterfall_contrast: config.rx2_waterfall_contrast,
            vfo_sync: false,
            mon_on: false,
            agc_mode: 3,
            agc_gain: 80,
            agc_auto_rx1: false,
            agc_auto_rx2: false,
            rit_enable: false,
            rit_offset: 0,
            xit_enable: false,
            xit_offset: 0,
            sql_enable: false,
            sql_level: 0,
            nb_enable: false,
            nb_level: 0,
            cw_keyer_speed: 20,
            vfo_lock: false,
            binaural: false,
            apf_enable: false,
            rx2_agc_mode: 3,
            rx2_agc_gain: 80,
            rx2_sql_enable: false,
            rx2_sql_level: 0,
            rx2_nb_enable: false,
            rx2_nb_level: 0,
            rx2_binaural: false,
            rx2_apf_enable: false,
            rx2_vfo_lock: false,
            mute: false,
            rx_mute: false,
            nf_enable: false,
            rx2_nf_enable: false,
            rx_balance: 0,
            tune_drive: 0,
            mon_volume: -40,
            tci_control_changed_at: None,
            yaesu_control_changed_at: None,
            midi: crate::midi::MidiManager::new(),
            midi_ports: Vec::new(),
            midi_selected_port: config.midi_device.clone(),
            midi_learn_for: None,
            midi_learn_action: crate::midi::MidiAction::Ptt,
            midi_last_event: String::new(),
            midi_encoder_hz: config.midi_encoder_hz,
            midi_last_dir_a: 0,
            midi_last_dir_b: 0,
            catsync: {
                let mut cs = crate::catsync::CatSync::new();
                cs.enabled = config.catsync_enabled;
                if !config.catsync_url.is_empty() {
                    cs.websdr_url = config.catsync_url.clone();
                }
                cs.favorites = config.catsync_favorites;
                cs
            },
            catsync_target: CatSyncTarget::Thetis,
            websdr_urls: {
                // Per-target URLs. Thetis uses the legacy `catsync_url` (or the
                // built-in default); each Yaesu falls back to that base when it
                // has no URL of its own yet (migration from the old shared URL).
                let base = if config.catsync_url.is_empty() {
                    crate::catsync::DEFAULT_WEBSDR_URL.to_string()
                } else {
                    config.catsync_url.clone()
                };
                let y1 = if config.catsync_url_y1.is_empty() { base.clone() } else { config.catsync_url_y1.clone() };
                let y2 = if config.catsync_url_y2.is_empty() { base.clone() } else { config.catsync_url_y2.clone() };
                [base, y1, y2]
            },
        };

        // Load MIDI mappings from config
        let midi_mappings: Vec<crate::midi::MidiMapping> = config.midi_mappings.iter()
            .filter_map(|s| crate::midi::MidiMapping::from_config(s))
            .collect();
        app.midi.set_mappings(midi_mappings);

        // Auto-connect MIDI if device was saved
        if !config.midi_device.is_empty() {
            app.midi_ports = crate::midi::MidiManager::list_ports();
            if app.midi_ports.contains(&config.midi_device) {
                app.midi.connect(&config.midi_device);
            }
        }

        app
    }
}
