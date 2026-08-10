// SPDX-License-Identifier: GPL-2.0-or-later
//! Config persistence for `SdrRemoteApp`: writing the PTT config, building the relay
//! config + (re)syncing the relay monitor, the full-config save, and per-band VFO
//! save/restore. Extracted verbatim from `ui/mod.rs` - pure relocation, no behaviour
//! change. `pub(super)` keeps them callable from the parent module tree.

use super::*;

impl SdrRemoteApp {
    pub(super) fn save_ptt_config(&self) {
        if let Ok(exe) = std::env::current_exe() {
            let path = exe.with_file_name(super::config::config_file_name());
            if let Ok(mut content) = std::fs::read_to_string(&path) {
                // Remove old ptt lines
                content = content.lines()
                    .filter(|l| !l.starts_with("ptt_toggle=") && !l.starts_with("midi_ptt_toggle=")
                        && !l.starts_with("yaesu_ptt_toggle=")
                        && !l.starts_with("yaesu2_enabled=") && !l.starts_with("yaesu2_ptt_toggle=")
                        && !l.starts_with("yaesu2_popout=")
                        && !l.starts_with("yaesu_mic_gain=") && !l.starts_with("yaesu2_mic_gain=")
                        && !l.starts_with("yaesu_compressor=") && !l.starts_with("yaesu2_compressor=")
                        && !l.starts_with("yaesu_tx_agc=") && !l.starts_with("yaesu2_tx_agc="))
                    .collect::<Vec<_>>().join("\n");
                content.push_str(&format!(
                    "\nptt_toggle={}\nyaesu_ptt_toggle={}\nmidi_ptt_toggle={}\nyaesu2_enabled={}\nyaesu2_ptt_toggle={}\nyaesu2_popout={}\nyaesu_mic_gain={:.3}\nyaesu2_mic_gain={:.3}\nyaesu_compressor={}\nyaesu2_compressor={}\nyaesu_tx_agc={}\nyaesu2_tx_agc={}\n",
                    self.ptt_toggle_mode, self.yaesu_ptt_toggle_mode, self.midi_ptt_toggle_mode,
                    self.yaesu2_enabled, self.yaesu2_ptt_toggle_mode,
                    self.yaesu2_popout, self.yaesu_mic_gain, self.yaesu2_mic_gain,
                    self.yaesu_compressor, self.yaesu2_compressor, self.yaesu_tx_agc, self.yaesu2_tx_agc));
                let _ = std::fs::write(path, content);
            }
        }
    }

    pub(super) fn relay_config(&self) -> sdr_remote_relay::RelayConfig {
        sdr_remote_relay::RelayConfig {
            enabled: self.relay_enabled,
            url: self.relay_url.trim().to_string(),
            station: self.relay_station.trim().to_string(),
            token: self.relay_token.clone(),
            role: sdr_remote_relay::RelayRole::Client,
            instance: self.relay_instance_id.clone(),
            name: self.relay_device_name.clone(),
            udp_port: sdr_remote_relay::relay_udp_port_resolve(self.relay_udp_enabled),
        }
    }

    pub(super) fn sync_relay_monitor(&mut self) {
        if self.relay_external {
            // Relay runs as transport (monitor in main.rs); a live restart here
            // would create a second relay connection. Config change = app restart.
            return;
        }
        let cfg = self.relay_config();
        if !cfg.enabled {
            if let Some(monitor) = self.relay_monitor.take() {
                monitor.stop();
            }
            self.relay_status = None;
            return;
        }
        if let Some(monitor) = self.relay_monitor.as_ref() {
            monitor.update_config(cfg);
        } else {
            let monitor = sdr_remote_relay::RelayMonitor::start_threaded(cfg);
            self.relay_status = Some(monitor.status_handle());
            self.relay_monitor = Some(monitor);
        }
    }
    pub(super) fn save_full_config(&self) {
        save_config(
            &self.server_input,
            &self.password_input,
            self.rx_volume,
            self.tx_gain,
            self.play_volume,
            self.vfo_a_volume,
            self.vfo_b_volume,
            self.local_volume,
            self.rx2_volume,
            &self.memories,
            &self.selected_input,
            &self.selected_output,
            self.agc_enabled,
            self.spectrum_enabled,
            // During a TX spectrum override ref/range/auto are temporarily at
            // TX defaults (auto=false); then write out the SAVED pre-TX values,
            // so a save in that window (or closing during/right-after TX) does not
            // overwrite the user setting (auto-ref "didn't always remember").
            self.tx_spectrum_saved_ref_db.unwrap_or(self.spectrum_ref_db),
            self.tx_spectrum_saved_range.unwrap_or(self.spectrum_range_db),
            self.tx_spectrum_saved_auto_ref.unwrap_or(self.auto_ref_enabled),
            self.waterfall_contrast,
            self.spectrum_max_bins,
            self.spectrum_fft_size_k,
            self.rx2_spectrum_fft_size_k,
            self.spectrum_total_h,
            self.spectrum_popout_pos.map(|p| (p.x, p.y)),
            self.spectrum_popout_size.map(|v| (v.x, v.y)),
            self.rx2_popout_pos.map(|p| (p.x, p.y)),
            self.rx2_popout_size.map(|v| (v.x, v.y)),
            self.popout_joined_pos.map(|p| (p.x, p.y)),
            self.popout_joined_size.map(|v| (v.x, v.y)),
            self.yaesu_popout_pos.map(|p| (p.x, p.y)),
            self.yaesu_popout_size.map(|v| (v.x, v.y)),
            self.yaesu2_popout_pos.map(|p| (p.x, p.y)),
            self.yaesu2_popout_size.map(|v| (v.x, v.y)),
            self.vrx_popout_pos.map(|p| (p.x, p.y)),
            self.vrx_popout_size.map(|v| (v.x, v.y)),
            self.vrx2_popout_pos.map(|p| (p.x, p.y)),
            self.vrx2_popout_size.map(|v| (v.x, v.y)),
            Some(self.vrx1_volume),
            Some(self.vrx1_enabled),
            Some(self.vrx1_freq_hz),
            Some(self.vrx1_mode),
            Some(self.vrx2_volume),
            Some(self.vrx2_enabled),
            Some(self.vrx2_freq_hz),
            Some(self.vrx2_mode),
            Some(self.vrx1_spectrum_zoom),
            Some(self.vrx2_spectrum_zoom),
            Some(self.vrx1_ref_db),
            Some(self.vrx1_range_db),
            Some(self.vrx1_wf_contrast),
            Some(self.vrx1_pan),
            Some(self.vrx1_auto_ref),
            Some(self.vrx1_filter_low_hz),
            Some(self.vrx1_filter_high_hz),
            Some(self.vrx1_high_res_spectrum),
            Some(self.vrx2_ref_db),
            Some(self.vrx2_range_db),
            Some(self.vrx2_wf_contrast),
            Some(self.vrx2_pan),
            Some(self.vrx2_auto_ref),
            Some(self.vrx2_filter_low_hz),
            Some(self.vrx2_filter_high_hz),
            Some(self.vrx2_high_res_spectrum),
            &self.wf_contrast_per_band,
            self.rx2_spectrum_ref_db,
            self.rx2_spectrum_range_db,
            self.rx2_auto_ref_enabled,
            self.rx2_waterfall_contrast,
            self.rx1_enabled,
            self.rx2_enabled,
            self.rx2_spectrum_enabled,
            self.thetis_wideband_audio,
            self.full_spectrum_enabled,
            self.ui_zoom,
            self.popout_joined,
            self.meter_analog,
            self.spectrum_popout,
            self.main_window_pos.map(|p| (p.x, p.y)),
            self.ub_show_menu,
            self.collapse_diversity,
            self.collapse_yaesu_eq,
            self.collapse_yaesu_memories,
            self.collapse_yaesu_menu,
            self.collapse_yaesu2_memories,
            self.collapse_yaesu2_menu,
            self.amplitec_power_show,
            self.bw_breakdown_expanded,
            self.yaesu_memories_h,
            self.yaesu2_memories_h,
            &arranger::layout_grids_to_config(&self.layout_grid_per_monitor),
            &self.layout_memories.iter().map(|m| m.to_config_string()).collect::<Vec<_>>(),
            self.device_tab,
            self.yaesu_enabled,
            self.yaesu_volume,
            self.yaesu2_volume,
            self.yaesu_popout,
            &self.yaesu_eq_active_profile,
            &self.yaesu_eq_profiles,
            &self.yaesu2_eq_active_profile,
            &self.yaesu2_eq_profiles,
            &self.yaesu_mem_file,
            &self.band_mem,
            self.window_w,
            self.window_h,
            &self.midi_selected_port,
            &self.midi.get_mappings(),
            self.midi_encoder_hz,
            self.catsync.enabled,
            &self.websdr_urls[0],
            &self.websdr_urls[1],
            &self.websdr_urls[2],
            &self.catsync.favorites,
            &self.mic_profile_map,
            self.theme_variant.as_str(),
            &self.theme_custom.to_config_string(),
            &self.ui_language,
            self.yaesu_present_last,
            self.yaesu2_present_last,
        );
    }

    /// Save current RX1 settings for the current band
    pub(super) fn save_current_band(&mut self, vfo: Vfo) {
        let (freq, mode, flo, fhi, nr) = match vfo {
            Vfo::A => (self.frequency_hz, self.mode, self.filter_low_hz, self.filter_high_hz, self.nr_level),
            Vfo::B => (self.rx2_frequency_hz, self.rx2_mode, self.rx2_filter_low_hz, self.rx2_filter_high_hz, self.rx2_nr_level),
        };
        if freq == 0 { return; }
        let bl = band_label(freq);
        if bl.is_empty() { return; }
        self.band_mem.insert(bl.to_string(), BandMemory {
            frequency_hz: freq, mode, filter_low_hz: flo, filter_high_hz: fhi, nr_level: nr,
        });
    }

    /// Restore band memory for target band, sending all CAT commands
    pub(super) fn restore_band(&mut self, vfo: Vfo, label: &str, default_freq: u64) {
        if let Some(mem) = self.band_mem.get(label).cloned() {
            let (cur_mode, cur_nr) = match vfo {
                Vfo::A => (self.mode, self.nr_level),
                Vfo::B => (self.rx2_mode, self.rx2_nr_level),
            };
            // Set mode first (filter range depends on mode). Skip a VFO-A (TX)
            // mode change during own TX: the server drops it (Thetis-bug
            // workaround) and Thetis won't echo, so an optimistic update would
            // leave the indication stuck out of sync. RX2 is RX-only.
            let block_mode = matches!(vfo, Vfo::A) && self.ptt;
            if mem.mode != cur_mode && !block_mode {
                let _ = self.cmd_tx.send(match vfo {
                    Vfo::A => Command::SetMode(mem.mode),
                    Vfo::B => Command::SetModeRx2(mem.mode),
                });
                match vfo { Vfo::A => self.mode = mem.mode, Vfo::B => self.rx2_mode = mem.mode }
            }
            let _ = self.cmd_tx.send(match vfo {
                Vfo::A => Command::SetFrequency(mem.frequency_hz),
                Vfo::B => Command::SetFrequencyRx2(mem.frequency_hz),
            });
            match vfo {
                Vfo::A => { self.set_pending_freq_a(mem.frequency_hz); }
                Vfo::B => { self.set_pending_freq_b(mem.frequency_hz); }
            }
            // Restore filter
            if mem.filter_low_hz != 0 || mem.filter_high_hz != 0 {
                let (flo_id, fhi_id) = match vfo {
                    Vfo::A => (ControlId::FilterLow, ControlId::FilterHigh),
                    Vfo::B => (ControlId::Rx2FilterLow, ControlId::Rx2FilterHigh),
                };
                let _ = self.cmd_tx.send(Command::SetControl(flo_id, mem.filter_low_hz as i16 as u16));
                let _ = self.cmd_tx.send(Command::SetControl(fhi_id, mem.filter_high_hz as i16 as u16));
                match vfo {
                    Vfo::A => { self.filter_low_hz = mem.filter_low_hz; self.filter_high_hz = mem.filter_high_hz; self.filter_changed_at = Some(Instant::now()); }
                    Vfo::B => { self.rx2_filter_low_hz = mem.filter_low_hz; self.rx2_filter_high_hz = mem.filter_high_hz; }
                }
            }
            // Restore NR
            if mem.nr_level != cur_nr {
                let nr_id = match vfo { Vfo::A => ControlId::NoiseReduction, Vfo::B => ControlId::Rx2NoiseReduction };
                let _ = self.cmd_tx.send(Command::SetControl(nr_id, mem.nr_level as u16));
                match vfo { Vfo::A => self.nr_level = mem.nr_level, Vfo::B => self.rx2_nr_level = mem.nr_level }
            }
        } else {
            let _ = self.cmd_tx.send(match vfo {
                Vfo::A => Command::SetFrequency(default_freq),
                Vfo::B => Command::SetFrequencyRx2(default_freq),
            });
            match vfo {
                Vfo::A => { self.set_pending_freq_a(default_freq); }
                Vfo::B => { self.set_pending_freq_b(default_freq); }
            }
        }
    }
}
