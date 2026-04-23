// SPDX-License-Identifier: GPL-2.0-or-later

use super::*;

use std::collections::HashMap;

use sdr_remote_core::DEFAULT_PORT;

/// Load smart auto-null steps from diversity-smart.txt next to the executable.
/// Format: P -180 -135 ... (phase offsets) or G -4 4 (gain offsets in dB)
/// Lines starting with # are comments.
pub(crate) fn load_smart_steps() -> Vec<(Vec<f32>, bool)> {
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("diversity-smart.txt")));
    let path = match path {
        Some(p) => p,
        None => return Vec::new(),
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut steps = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        let is_phase = line.starts_with('P') || line.starts_with('p');
        let is_gain = line.starts_with('G') || line.starts_with('g');
        if !is_phase && !is_gain { continue; }
        let offsets: Vec<f32> = line[1..].split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        if !offsets.is_empty() {
            steps.push((offsets, is_phase));
        }
    }
    if !steps.is_empty() {
        log::info!("Loaded {} smart auto-null steps from {}", steps.len(), path.display());
    }
    steps
}

/// Config file name stored next to the executable
pub(crate) const CONFIG_FILE: &str = "thetislink-client.conf";

pub(crate) const NUM_MEMORIES: usize = 5;

/// Client configuration
pub(crate) struct ClientConfig {
    pub(crate) server: String,
    pub(crate) password: String,
    pub(crate) rx_volume: f32,
    pub(crate) tx_gain: f32,
    pub(crate) vfo_a_volume: f32,
    pub(crate) vfo_b_volume: f32,
    pub(crate) local_volume: f32,
    pub(crate) rx2_volume: f32,
    pub(crate) memories: [Memory; NUM_MEMORIES],
    pub(crate) tx_profiles: Vec<(u8, String)>,
    pub(crate) input_device: String,
    pub(crate) output_device: String,
    pub(crate) mic_profile_map: std::collections::HashMap<String, String>,
    pub(crate) agc_enabled: bool,
    pub(crate) spectrum_enabled: bool,
    pub(crate) spectrum_ref_db: f32,
    pub(crate) spectrum_range_db: f32,
    pub(crate) auto_ref_enabled: bool,
    pub(crate) waterfall_contrast: f32,
    pub(crate) spectrum_max_bins: u16,
    pub(crate) spectrum_fft_size_k: u16,
    pub(crate) rx2_spectrum_fft_size_k: u16,
    pub(crate) wf_contrast_per_band: HashMap<String, f32>,
    pub(crate) rx2_spectrum_ref_db: f32,
    pub(crate) rx2_spectrum_range_db: f32,
    pub(crate) rx2_auto_ref_enabled: bool,
    pub(crate) rx2_waterfall_contrast: f32,
    pub(crate) rx2_enabled: bool,
    pub(crate) popout_joined: bool,
    pub(crate) popout_meter_analog: bool,
    pub(crate) device_tab: u8,
    pub(crate) yaesu_enabled: bool,
    pub(crate) yaesu_volume: f32,
    pub(crate) yaesu_popout: bool,
    pub(crate) yaesu_eq_profiles: Vec<(String, bool, [f32; 5])>,
    pub(crate) yaesu_eq_active: String,
    pub(crate) yaesu_mem_file: String,
    pub(crate) band_mem: HashMap<String, BandMemory>,
    pub(crate) window_w: f32,
    pub(crate) window_h: f32,
    pub(crate) midi_device: String,
    pub(crate) midi_mappings: Vec<String>,
    pub(crate) midi_encoder_hz: u64,
    pub(crate) ptt_toggle: bool,
    pub(crate) yaesu_ptt_toggle: bool,
    pub(crate) midi_ptt_toggle: bool,
    pub(crate) catsync_enabled: bool,
    pub(crate) catsync_url: String,
    pub(crate) catsync_favorites: Vec<(String, String)>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            server: format!("127.0.0.1:{}", DEFAULT_PORT),
            password: String::new(),
            rx_volume: 0.2,
            tx_gain: 0.5,
            vfo_a_volume: 1.0,
            vfo_b_volume: 1.0,
            local_volume: 1.0,
            rx2_volume: 0.2,
            memories: Default::default(),
            tx_profiles: vec![(0, "Default".to_string())],
            input_device: String::new(),
            output_device: String::new(),
            mic_profile_map: std::collections::HashMap::new(),
            agc_enabled: false,
            spectrum_enabled: false,
            spectrum_ref_db: -20.0,
            spectrum_range_db: 100.0,
            auto_ref_enabled: false,
            waterfall_contrast: 1.2,
            spectrum_max_bins: sdr_remote_core::DEFAULT_SPECTRUM_BINS as u16,
            spectrum_fft_size_k: 0,  // 0 = auto (server default)
            rx2_spectrum_fft_size_k: 0,
            wf_contrast_per_band: HashMap::new(),
            rx2_spectrum_ref_db: -20.0,
            rx2_spectrum_range_db: 100.0,
            rx2_auto_ref_enabled: false,
            rx2_waterfall_contrast: 1.2,
            rx2_enabled: false,
            popout_joined: false,
            device_tab: 0,
            yaesu_enabled: false,
            yaesu_volume: 0.05,
            yaesu_eq_profiles: Vec::new(),
            yaesu_eq_active: String::new(),
            yaesu_popout: false,
            yaesu_mem_file: String::new(),
            popout_meter_analog: false,
            band_mem: HashMap::new(),
            window_w: 400.0,
            window_h: 500.0,
            midi_device: String::new(),
            midi_mappings: Vec::new(),
            midi_encoder_hz: 100,
            ptt_toggle: false,
            yaesu_ptt_toggle: false,
            midi_ptt_toggle: true, // MIDI defaults to toggle (existing behavior)
            catsync_enabled: false,
            catsync_url: String::new(),
            catsync_favorites: Vec::new(),
        }
    }
}

/// Load saved window size from config (for use before app creation).
pub(crate) fn load_window_size() -> [f32; 2] {
    let config = load_config();
    [config.window_w, config.window_h]
}

/// Load config from file next to the executable.
pub(crate) fn load_config() -> ClientConfig {
    let mut config = ClientConfig::default();

    let path = match std::env::current_exe() {
        Ok(exe) => exe.with_file_name(CONFIG_FILE),
        Err(_) => return config,
    };
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return config,
    };

    let mut tx_profiles: Option<Vec<(u8, String)>> = None;
    let mut has_keys = false;
    for line in contents.lines() {
        if let Some(val) = line.strip_prefix("server=") {
            let v = val.trim();
            if !v.is_empty() {
                config.server = v.to_string();
            }
        } else if let Some(val) = line.strip_prefix("password=") {
            let v = val.trim();
            if !v.is_empty() {
                config.password = sdr_remote_core::auth::deobfuscate_password(v)
                    .unwrap_or_else(|| v.to_string());
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("volume=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.rx_volume = v.clamp(0.0, 1.0);
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("tx_gain=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.tx_gain = v.clamp(0.0, 3.0);
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("input_device=") {
            let v = val.trim();
            if !v.is_empty() {
                config.input_device = v.to_string();
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("output_device=") {
            let v = val.trim();
            if !v.is_empty() {
                config.output_device = v.to_string();
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("tx_profiles=") {
            let mut profiles = Vec::new();
            for entry in val.trim().split(',') {
                let entry = entry.trim();
                if let Some((idx_str, name)) = entry.split_once(':') {
                    if let Ok(idx) = idx_str.trim().parse::<u8>() {
                        let name = name.trim();
                        if !name.is_empty() {
                            profiles.push((idx, name.to_string()));
                        }
                    }
                }
            }
            if !profiles.is_empty() {
                tx_profiles = Some(profiles);
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("agc_enabled=") {
            config.agc_enabled = val.trim() == "true";
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("spectrum_enabled=") {
            config.spectrum_enabled = val.trim() == "true";
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("spectrum_ref_db=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.spectrum_ref_db = v.clamp(-80.0, 0.0);
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("spectrum_range_db=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.spectrum_range_db = v.clamp(20.0, 130.0);
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("spectrum_max_bins=") {
            if let Ok(v) = val.trim().parse::<u16>() {
                config.spectrum_max_bins = v.clamp(64, sdr_remote_core::MAX_SPECTRUM_SEND_BINS as u16);
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("rx2_spectrum_fft_size_k=") {
            if let Ok(v) = val.trim().parse::<u16>() {
                config.rx2_spectrum_fft_size_k = v;
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("spectrum_fft_size_k=") {
            if let Ok(v) = val.trim().parse::<u16>() {
                config.spectrum_fft_size_k = v;
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("auto_ref_enabled=") {
            config.auto_ref_enabled = val.trim() == "true";
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("waterfall_contrast=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.waterfall_contrast = v.clamp(0.3, 3.0);
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("wf_contrast_per_band=") {
            for entry in val.trim().split(',') {
                let entry = entry.trim();
                if let Some((band, contrast_str)) = entry.split_once(':') {
                    if let Ok(c) = contrast_str.trim().parse::<f32>() {
                        config.wf_contrast_per_band.insert(band.trim().to_string(), c.clamp(0.3, 3.0));
                    }
                }
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("rx2_spectrum_ref_db=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.rx2_spectrum_ref_db = v.clamp(-80.0, 0.0);
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("rx2_spectrum_range_db=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.rx2_spectrum_range_db = v.clamp(20.0, 130.0);
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("rx2_auto_ref_enabled=") {
            config.rx2_auto_ref_enabled = val.trim() == "true";
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("rx2_waterfall_contrast=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.rx2_waterfall_contrast = v.clamp(0.3, 3.0);
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("window_w=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.window_w = v.clamp(200.0, 4000.0);
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("window_h=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.window_h = v.clamp(200.0, 4000.0);
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("device_tab=") {
            if let Ok(v) = val.trim().parse::<u8>() { config.device_tab = v; }
        } else if let Some(val) = line.strip_prefix("yaesu_enabled=") {
            config.yaesu_enabled = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("yaesu_volume=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.yaesu_volume = v.clamp(0.001, 1.0);
            }
        } else if let Some(val) = line.strip_prefix("yaesu_popout=") {
            config.yaesu_popout = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("mic_profile=") {
            // Format: mic_device_name|tx_profile_name
            if let Some((mic, profile)) = val.trim().split_once('|') {
                config.mic_profile_map.insert(mic.to_string(), profile.to_string());
            }
        } else if let Some(val) = line.strip_prefix("yaesu_eq_active=") {
            config.yaesu_eq_active = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("yaesu_eq_profile=") {
            // Format: name|enabled|g0,g1,g2,g3,g4
            let parts: Vec<&str> = val.trim().splitn(3, '|').collect();
            if parts.len() == 3 {
                let name = parts[0].to_string();
                let enabled = parts[1] == "1";
                let gains: Vec<f32> = parts[2].split(',')
                    .filter_map(|s| s.trim().parse().ok()).collect();
                if gains.len() == 5 {
                    config.yaesu_eq_profiles.push((name, enabled, [gains[0], gains[1], gains[2], gains[3], gains[4]]));
                }
            }
        } else if let Some(val) = line.strip_prefix("yaesu_mem_file=") {
            config.yaesu_mem_file = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("rx2_enabled=") {
            config.rx2_enabled = val.trim() == "true";
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("popout_joined=") {
            config.popout_joined = val.trim() == "true";
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("popout_meter_analog=") {
            config.popout_meter_analog = val.trim() == "true";
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("vfo_a_volume=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.vfo_a_volume = v.clamp(0.0, 1.0);
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("vfo_b_volume=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.vfo_b_volume = v.clamp(0.0, 1.0);
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("local_volume=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.local_volume = v.clamp(0.0, 1.0);
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("rx2_volume=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.rx2_volume = v.clamp(0.0, 1.0);
            }
            has_keys = true;
        } else if let Some(rest) = line.strip_prefix("band_mem_") {
            // band_mem_40m=7073000:1:-100:2800:2
            if let Some((band, val)) = rest.split_once('=') {
                let parts: Vec<&str> = val.trim().split(':').collect();
                if parts.len() >= 5 {
                    if let (Ok(freq), Ok(mode), Ok(fl), Ok(fh), Ok(nr)) = (
                        parts[0].parse::<u64>(),
                        parts[1].parse::<u8>(),
                        parts[2].parse::<i32>(),
                        parts[3].parse::<i32>(),
                        parts[4].parse::<u8>(),
                    ) {
                        config.band_mem.insert(band.to_string(), BandMemory {
                            frequency_hz: freq, mode, filter_low_hz: fl, filter_high_hz: fh, nr_level: nr,
                        });
                    }
                }
            }
            has_keys = true;
        } else if let Some(rest) = line.strip_prefix("band_freqs=") {
            // Legacy: migrate old band_freqs to band_mem (freq only)
            for entry in rest.trim().split(',') {
                if let Some((band, freq_str)) = entry.trim().split_once(':') {
                    if let Ok(hz) = freq_str.trim().parse::<u64>() {
                        config.band_mem.entry(band.trim().to_string()).or_insert(BandMemory {
                            frequency_hz: hz, mode: 0, filter_low_hz: 0, filter_high_hz: 0, nr_level: 0,
                        });
                    }
                }
            }
            has_keys = true;
        } else if let Some(stripped) = line.strip_prefix("mem") {
            if let Some((idx_str, val)) = stripped.split_once('=') {
                if let Ok(idx) = idx_str.parse::<usize>() {
                    if idx >= 1 && idx <= NUM_MEMORIES {
                        let val = val.trim();
                        if !val.is_empty() {
                            let parts: Vec<&str> = val.split(',').collect();
                            if let Some(freq_str) = parts.first() {
                                if let Ok(hz) = freq_str.parse::<u64>() {
                                    config.memories[idx - 1].frequency_hz = Some(hz);
                                }
                            }
                            if let Some(mode_str) = parts.get(1) {
                                if let Ok(m) = mode_str.parse::<u8>() {
                                    config.memories[idx - 1].mode = Some(m);
                                }
                            }
                        }
                    }
                }
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("midi_device=") {
            let v = val.trim();
            if !v.is_empty() {
                config.midi_device = v.to_string();
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("midi_encoder_hz=") {
            if let Ok(v) = val.trim().parse::<u64>() {
                config.midi_encoder_hz = v.clamp(1, 10000);
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("ptt_toggle=") {
            config.ptt_toggle = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("yaesu_ptt_toggle=") {
            config.yaesu_ptt_toggle = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("midi_ptt_toggle=") {
            config.midi_ptt_toggle = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("catsync_enabled=") {
            config.catsync_enabled = val.trim() == "true";
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("catsync_url=") {
            let v = val.trim();
            if !v.is_empty() {
                config.catsync_url = v.to_string();
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("catsync_fav=") {
            // Format: label|url
            let v = val.trim();
            if let Some((label, url)) = v.split_once('|') {
                if !url.is_empty() {
                    config.catsync_favorites.push((label.to_string(), url.to_string()));
                }
            }
            has_keys = true;
        } else if let Some(rest) = line.strip_prefix("midi_map_") {
            // midi_map_0=cc:1:7:slider:master_volume
            if let Some((_idx, val)) = rest.split_once('=') {
                config.midi_mappings.push(val.trim().to_string());
            }
            has_keys = true;
        }
    }
    if !has_keys {
        let v = contents.trim();
        if !v.is_empty() {
            config.server = v.to_string();
        }
    }
    if let Some(profiles) = tx_profiles {
        config.tx_profiles = profiles;
    }
    config
}

/// Save config to file next to the executable.
pub(crate) fn save_config(
    server: &str,
    password: &str,
    volume: f32,
    tx_gain: f32,
    vfo_a_volume: f32,
    vfo_b_volume: f32,
    local_volume: f32,
    rx2_volume: f32,
    memories: &[Memory; NUM_MEMORIES],
    input_device: &str,
    output_device: &str,
    agc_enabled: bool,
    spectrum_enabled: bool,
    spectrum_ref_db: f32,
    spectrum_range_db: f32,
    auto_ref_enabled: bool,
    waterfall_contrast: f32,
    spectrum_max_bins: u16,
    spectrum_fft_size_k: u16,
    rx2_spectrum_fft_size_k: u16,
    wf_contrast_per_band: &HashMap<String, f32>,
    rx2_spectrum_ref_db: f32,
    rx2_spectrum_range_db: f32,
    rx2_auto_ref_enabled: bool,
    rx2_waterfall_contrast: f32,
    rx2_enabled: bool,
    popout_joined: bool,
    popout_meter_analog: bool,
    device_tab: u8,
    yaesu_enabled: bool,
    yaesu_volume: f32,
    yaesu_popout: bool,
    yaesu_eq_active: &str,
    yaesu_eq_profiles: &[(String, bool, [f32; 5])],
    yaesu_mem_file: &str,
    band_mem: &HashMap<String, BandMemory>,
    window_w: f32,
    window_h: f32,
    midi_device: &str,
    midi_mappings: &[crate::midi::MidiMapping],
    midi_encoder_hz: u64,
    catsync_enabled: bool,
    catsync_url: &str,
    catsync_favorites: &[(String, String)],
    mic_profile_map: &std::collections::HashMap<String, String>,
) {
    if let Ok(exe) = std::env::current_exe() {
        let path = exe.with_file_name(CONFIG_FILE);
        let pw_enc = if password.is_empty() { String::new() } else { sdr_remote_core::auth::obfuscate_password(password) };
        let mut content = format!("server={}\npassword={}\nvolume={:.2}\ntx_gain={:.2}\nvfo_a_volume={:.2}\nvfo_b_volume={:.2}\nlocal_volume={:.2}\nrx2_volume={:.2}\n",
            server, pw_enc, volume, tx_gain, vfo_a_volume, vfo_b_volume, local_volume, rx2_volume);
        if !input_device.is_empty() {
            content.push_str(&format!("input_device={}\n", input_device));
        }
        if !output_device.is_empty() {
            content.push_str(&format!("output_device={}\n", output_device));
        }
        content.push_str(&format!("agc_enabled={}\n", agc_enabled));
        content.push_str(&format!("spectrum_enabled={}\n", spectrum_enabled));
        content.push_str(&format!("spectrum_ref_db={:.0}\n", spectrum_ref_db));
        content.push_str(&format!("spectrum_range_db={:.0}\n", spectrum_range_db));
        content.push_str(&format!("auto_ref_enabled={}\n", auto_ref_enabled));
        content.push_str(&format!("spectrum_max_bins={}\n", spectrum_max_bins));
        content.push_str(&format!("spectrum_fft_size_k={}\n", spectrum_fft_size_k));
        content.push_str(&format!("rx2_spectrum_fft_size_k={}\n", rx2_spectrum_fft_size_k));
        content.push_str(&format!("waterfall_contrast={:.2}\n", waterfall_contrast));
        // Per-band WF contrast
        if !wf_contrast_per_band.is_empty() {
            let pairs: Vec<String> = wf_contrast_per_band.iter()
                .map(|(band, c)| format!("{}:{:.2}", band, c))
                .collect();
            content.push_str(&format!("wf_contrast_per_band={}\n", pairs.join(",")));
        }
        // RX2 spectrum settings
        content.push_str(&format!("rx2_spectrum_ref_db={:.0}\n", rx2_spectrum_ref_db));
        content.push_str(&format!("rx2_spectrum_range_db={:.0}\n", rx2_spectrum_range_db));
        content.push_str(&format!("rx2_auto_ref_enabled={}\n", rx2_auto_ref_enabled));
        content.push_str(&format!("rx2_waterfall_contrast={:.2}\n", rx2_waterfall_contrast));
        content.push_str(&format!("rx2_enabled={}\n", rx2_enabled));
        content.push_str(&format!("popout_joined={}\n", popout_joined));
        content.push_str(&format!("popout_meter_analog={}\n", popout_meter_analog));
        content.push_str(&format!("device_tab={}\n", device_tab));
        content.push_str(&format!("yaesu_enabled={}\n", yaesu_enabled));
        content.push_str(&format!("yaesu_volume={:.3}\n", yaesu_volume));
        content.push_str(&format!("yaesu_popout={}\n", yaesu_popout));
        content.push_str(&format!("yaesu_eq_active={}\n", yaesu_eq_active));
        for (name, enabled, gains) in yaesu_eq_profiles {
            content.push_str(&format!("yaesu_eq_profile={}|{}|{:.1},{:.1},{:.1},{:.1},{:.1}\n",
                name, if *enabled { "1" } else { "0" },
                gains[0], gains[1], gains[2], gains[3], gains[4]));
        }
        for (mic, profile) in mic_profile_map {
            content.push_str(&format!("mic_profile={}|{}\n", mic, profile));
        }
        content.push_str(&format!("yaesu_mem_file={}\n", yaesu_mem_file));
        content.push_str(&format!("window_w={:.0}\n", window_w));
        content.push_str(&format!("window_h={:.0}\n", window_h));
        // Per-band memory (freq:mode:filter_low:filter_high:nr)
        for (band, mem) in band_mem {
            content.push_str(&format!("band_mem_{}={}:{}:{}:{}:{}\n",
                band, mem.frequency_hz, mem.mode, mem.filter_low_hz, mem.filter_high_hz, mem.nr_level));
        }
        // Preserve tx_profiles from existing config
        if let Ok(existing) = std::fs::read_to_string(&path) {
            for line in existing.lines() {
                if line.starts_with("tx_profiles=") {
                    content.push_str(line);
                    content.push('\n');
                    break;
                }
            }
        }
        for (i, mem) in memories.iter().enumerate() {
            if let Some(hz) = mem.frequency_hz {
                let mode = mem.mode.unwrap_or(0);
                content.push_str(&format!("mem{}={},{}\n", i + 1, hz, mode));
            } else {
                content.push_str(&format!("mem{}=\n", i + 1));
            }
        }
        // MIDI
        if !midi_device.is_empty() {
            content.push_str(&format!("midi_device={}\n", midi_device));
        }
        content.push_str(&format!("midi_encoder_hz={}\n", midi_encoder_hz));
        content.push_str(&format!("catsync_enabled={}\n", catsync_enabled));
        if !catsync_url.is_empty() {
            content.push_str(&format!("catsync_url={}\n", catsync_url));
        }
        for (label, url) in catsync_favorites {
            content.push_str(&format!("catsync_fav={}|{}\n", label, url));
        }
        for (i, mapping) in midi_mappings.iter().enumerate() {
            content.push_str(&format!("midi_map_{}={}\n", i, mapping.to_config()));
        }
        // Preserve ptt_toggle + midi_ptt_toggle from existing config
        if let Ok(existing) = std::fs::read_to_string(&path) {
            for line in existing.lines() {
                if line.starts_with("ptt_toggle=") || line.starts_with("yaesu_ptt_toggle=") || line.starts_with("midi_ptt_toggle=") {
                    content.push_str(line);
                    content.push('\n');
                }
            }
        }
        let _ = std::fs::write(path, content);
    }
}
