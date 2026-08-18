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

/// Config file name stored next to the executable (default profile).
pub(crate) const CONFIG_FILE: &str = "thetislink-client.conf";

pub(crate) const NUM_MEMORIES: usize = 5;

// ---- Instance profile (multi-instance support) --------------------------------
// A named profile lets a SECOND ThetisLink run alongside the first on one PC,
// each with its OWN config file, log files and single-instance identity, so the
// two never clobber each other's persisted state. Selected once at startup with
// `--profile <name>`; no profile = the default, which keeps the original file
// names / mutex so existing installs are byte-for-byte unchanged. A named
// profile's config is seeded as a COPY of the default on first use (see
// seed_profile_config_if_absent) - one extra .conf you can just delete to remove
// the instance, leaving the original intact.
static PROFILE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// Record the active profile. Call ONCE, before any config/log path is resolved.
/// A second call is ignored (OnceLock). The name is sanitised to `[A-Za-z0-9_-]`
/// (safe as both a file-name fragment and a Win32 mutex name); an empty result
/// falls back to the default profile.
pub(crate) fn set_profile(name: Option<String>) {
    let cleaned = name.and_then(|n| {
        let s: String = n
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        if s.is_empty() { None } else { Some(s) }
    });
    let _ = PROFILE.set(cleaned);
}

/// The active profile name, or `None` for the default (unnamed) profile.
pub(crate) fn profile() -> Option<&'static str> {
    PROFILE.get().and_then(|o| o.as_deref())
}

/// Config file name for the active profile: `thetislink-client.conf` for the
/// default, `thetislink-client-<profile>.conf` for a named one.
pub(crate) fn config_file_name() -> String {
    match profile() {
        Some(p) => format!("thetislink-client-{p}.conf"),
        None => CONFIG_FILE.to_string(),
    }
}

/// Per-profile side-file name (log, ui-events): `<base>.<ext>` for the default
/// profile, `<base>-<profile>.<ext>` for a named one.
pub(crate) fn per_profile_file(base: &str, ext: &str) -> String {
    match profile() {
        Some(p) => format!("{base}-{p}.{ext}"),
        None => format!("{base}.{ext}"),
    }
}

/// Decorate a window title with the profile tag (`  [B]`) when a named profile
/// is active, so the windows of two instances are visually distinguishable. The
/// default profile returns the title unchanged.
pub(crate) fn window_title(base: &str) -> String {
    match profile() {
        Some(p) => format!("{base}  [{p}]"),
        None => base.to_string(),
    }
}

/// Seed a named profile's config file by copying the default config, ONCE, when
/// the profile file does not yet exist. This makes `--profile B` start as a copy
/// of the current settings (the user then tweaks only what differs). No-op for
/// the default profile, if the profile file already exists, or if the default
/// file is missing (the profile then starts from the built-in defaults).
pub(crate) fn seed_profile_config_if_absent() {
    if profile().is_none() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return; };
    let dst = exe.with_file_name(config_file_name());
    if dst.exists() {
        return;
    }
    // Source is always the DEFAULT config (copy FROM the original).
    let src = exe.with_file_name(CONFIG_FILE);
    if !src.exists() {
        log::info!(
            "Profile {:?}: no default config to seed from - starting from defaults",
            profile()
        );
        return;
    }
    match std::fs::copy(&src, &dst) {
        Ok(_) => log::info!("Seeded profile config {} from {}", dst.display(), src.display()),
        Err(e) => log::warn!("Could not seed profile config {}: {}", dst.display(), e),
    }
}

/// Client configuration
pub(crate) struct ClientConfig {
    pub(crate) server: String,
    pub(crate) password: String,
    pub(crate) rx_volume: f32,
    pub(crate) tx_gain: f32,
    /// WAV-playback ('Play') volume, client-only. 1.0 = recorded level.
    pub(crate) play_volume: f32,
    pub(crate) vfo_a_volume: f32,
    pub(crate) vfo_b_volume: f32,
    pub(crate) local_volume: f32,
    /// UI scale, independent of the Windows display scaling. On a high-DPI
    /// screen the OS scale decides how many POINTS the app has to lay out in,
    /// and a Surface at 200% ends up with fewer points than an ordinary 1080p
    /// monitor at 100% - so less fits, despite the higher resolution. This lets
    /// the app be scaled down without touching the system-wide setting.
    pub(crate) ui_zoom: f32,
    pub(crate) rx2_volume: f32,
    pub(crate) memories: [Memory; NUM_MEMORIES],
    pub(crate) tx_profiles: Vec<(u8, String)>,
    pub(crate) input_device: String,
    pub(crate) output_device: String,
    pub(crate) mic_profile_map: std::collections::HashMap<String, String>,
    pub(crate) agc_enabled: bool,
    /// UI theme variant ("classic" | "dark" | "slate" | "custom").
    pub(crate) theme: String,
    /// Custom palette as `rrggbb,rrggbb,rrggbb` (background,widget,text). Empty = unset.
    pub(crate) theme_custom: String,
    pub(crate) spectrum_enabled: bool,
    pub(crate) spectrum_ref_db: f32,
    pub(crate) spectrum_range_db: f32,
    pub(crate) auto_ref_enabled: bool,
    pub(crate) waterfall_contrast: f32,
    pub(crate) spectrum_max_bins: u16,
    pub(crate) spectrum_fft_size_k: u16,
    pub(crate) rx2_spectrum_fft_size_k: u16,
    /// Total height (in egui-points) reserved for the spectrum + waterfall
    /// area in the main Radio tab. Range 300-1200. When the value plus the
    /// rest of the Radio content exceeds the window the page becomes
    /// scrollable. Popouts keep using their full available height regardless
    /// of this setting.
    pub(crate) spectrum_total_h: f32,
    /// Persisted geometry per popout viewport - `Some((x, y))` / `Some((w, h))`
    /// is the last position / size the OS reported; `None` means use the
    /// hard-coded default and let the OS pick the position.
    pub(crate) spectrum_popout_pos: Option<(f32, f32)>,
    pub(crate) spectrum_popout_size: Option<(f32, f32)>,
    pub(crate) rx2_popout_pos: Option<(f32, f32)>,
    pub(crate) rx2_popout_size: Option<(f32, f32)>,
    pub(crate) popout_joined_pos: Option<(f32, f32)>,
    pub(crate) popout_joined_size: Option<(f32, f32)>,
    pub(crate) yaesu_popout_pos: Option<(f32, f32)>,
    pub(crate) yaesu_popout_size: Option<(f32, f32)>,
    pub(crate) yaesu2_popout_pos: Option<(f32, f32)>,
    pub(crate) yaesu2_popout_size: Option<(f32, f32)>,
    pub(crate) vrx_popout_pos: Option<(f32, f32)>,
    pub(crate) vrx_popout_size: Option<(f32, f32)>,
    pub(crate) vrx2_popout_pos: Option<(f32, f32)>,
    pub(crate) vrx2_popout_size: Option<(f32, f32)>,
    // VRX1 per-channel state (was `vrx_volume`/`vrx_enabled`/etc. in
    // build 48 - renamed in build 50 when VRX2 was added; the loader
    // still accepts the old keys for one-version backwards compat).
    pub(crate) vrx1_volume: Option<f32>,
    pub(crate) vrx1_enabled: Option<bool>,
    pub(crate) vrx1_freq_hz: Option<u64>,
    pub(crate) vrx1_mode: Option<u8>,
    pub(crate) vrx2_volume: Option<f32>,
    pub(crate) vrx2_enabled: Option<bool>,
    pub(crate) vrx2_freq_hz: Option<u64>,
    pub(crate) vrx2_mode: Option<u8>,
    pub(crate) vrx1_ref_db: Option<f32>,
    pub(crate) vrx1_range_db: Option<f32>,
    pub(crate) vrx1_wf_contrast: Option<f32>,
    pub(crate) vrx1_auto_ref: Option<bool>,
    pub(crate) vrx1_filter_low_hz: Option<i32>,
    pub(crate) vrx1_filter_high_hz: Option<i32>,
    pub(crate) vrx1_high_res_spectrum: Option<bool>,
    pub(crate) vrx2_ref_db: Option<f32>,
    pub(crate) vrx2_range_db: Option<f32>,
    pub(crate) vrx2_wf_contrast: Option<f32>,
    pub(crate) vrx2_auto_ref: Option<bool>,
    pub(crate) vrx2_filter_low_hz: Option<i32>,
    pub(crate) vrx2_filter_high_hz: Option<i32>,
    pub(crate) vrx2_high_res_spectrum: Option<bool>,
    /// The main receivers' zoom, kept the way VRX has always kept its own.
    /// `None` means it was never set, and only then is the opening zoom
    /// derived from the receiver's width on the first connection.
    ///
    /// The pan is deliberately not here. It survives a reconnect, because a
    /// link that drops for a moment should not move the view, but it starts a
    /// new session at zero: it is used far less often than the zoom, and an
    /// offset carried over from days ago reads as a fault rather than as a
    /// setting. The zoom is the one the operator still has in mind.
    pub(crate) wf_contrast_per_band: HashMap<String, f32>,
    pub(crate) rx2_spectrum_ref_db: f32,
    pub(crate) rx2_spectrum_range_db: f32,
    pub(crate) rx2_auto_ref_enabled: bool,
    pub(crate) rx2_waterfall_contrast: f32,
    pub(crate) rx1_enabled: bool,
    pub(crate) rx2_enabled: bool,
    pub(crate) rx2_spectrum_enabled: bool,
    /// Opt-in for wideband Thetis audio (RX1/RX2/BinR 16 kHz instead of
    /// 8 kHz default). Roughly doubles RX bandwidth per channel,
    /// so default false; enable via Settings -> Audio.
    pub(crate) thetis_wideband_audio: bool,
    /// Ask the server for the extra full-DDC spectrum row (default on).
    pub(crate) full_spectrum_enabled: bool,
    pub(crate) popout_joined: bool,
    /// Legacy global s-meter type (migrated to per-channel `meter_analog`).
    pub(crate) popout_meter_analog: bool,
    /// S-meter type per channel (M_RX1..M_YAESU2). Migrates from popout_meter_analog.
    pub(crate) meter_analog: [bool; 6],
    pub(crate) spectrum_popout: bool,
    pub(crate) main_window_pos: Option<(f32, f32)>,
    pub(crate) ub_show_menu: bool,
    pub(crate) collapse_diversity: bool,
    pub(crate) collapse_yaesu_eq: bool,
    pub(crate) collapse_yaesu_memories: bool,
    pub(crate) collapse_yaesu_menu: bool,
    pub(crate) collapse_yaesu2_memories: bool,
    pub(crate) collapse_yaesu2_menu: bool,
    pub(crate) amplitec_power_show: bool,
    pub(crate) bw_breakdown_expanded: bool,
    pub(crate) yaesu_memories_h: f32,
    /// Height of the slot-1 memory list. Separate from slot 0 on purpose: the
    /// two windows are laid out independently and a shared height moves one
    /// while you are adjusting the other.
    pub(crate) yaesu2_memories_h: f32,
    /// Per-monitor placement matrices, one config line (see arranger.rs).
    pub(crate) layout_grids: String,
    /// Stored arrangements, one config line each.
    pub(crate) layout_memories: Vec<String>,
    pub(crate) device_tab: u8,
    pub(crate) yaesu_enabled: bool,
    pub(crate) yaesu_volume: f32,
    pub(crate) yaesu_popout: bool,
    /// Last server-reported Yaesu presence (slot 0/1). Seeds the optimistic
    /// pre-connect display so a radio present last session shows again at once;
    /// the server prunes it on connect if it is (no longer) there.
    pub(crate) yaesu_present_last: bool,
    pub(crate) yaesu2_present_last: bool,
    // Tuple: (name, enabled, eq-gains, mic_gain)
    pub(crate) yaesu_eq_profiles: Vec<(String, bool, [f32; 5], f32)>,
    pub(crate) yaesu_eq_active: String,
    pub(crate) yaesu2_eq_profiles: Vec<(String, bool, [f32; 5], f32)>,
    pub(crate) yaesu2_eq_active: String,
    pub(crate) yaesu_mem_file: String,
    pub(crate) band_mem: HashMap<String, BandMemory>,
    pub(crate) window_w: f32,
    pub(crate) window_h: f32,
    pub(crate) midi_device: String,
    pub(crate) midi_mappings: Vec<String>,
    pub(crate) midi_encoder_hz: u64,
    pub(crate) ptt_toggle: bool,
    pub(crate) yaesu_ptt_toggle: bool,
    pub(crate) chat_open: bool,
    pub(crate) midi_ptt_toggle: bool,
    // Dual-radio slot 1 (FTX-1): own enable + PTT mode + volume, persistent.
    pub(crate) yaesu2_enabled: bool,
    pub(crate) yaesu2_ptt_toggle: bool,
    pub(crate) yaesu2_volume: f32,
    /// USB-TX mic-gain multiplier per radio, persistent (separate from the EQ
    /// profile so the slider value is always remembered). Range 0.05..=3.0.
    pub(crate) yaesu_mic_gain: f32,
    pub(crate) yaesu2_mic_gain: f32,
    /// Client-side TX compressor (0-100) and AGC toggle per radio, persistent.
    pub(crate) yaesu_compressor: u8,
    pub(crate) yaesu2_compressor: u8,
    pub(crate) yaesu_tx_agc: bool,
    pub(crate) yaesu2_tx_agc: bool,
    /// FTX-1 (radio 2) popout window open/closed state, persistent.
    pub(crate) yaesu2_popout: bool,
    /// S-meter source choice: 0=Sig, 1=Avg (default), 2=MaxBin.
    /// Shared by RX1 and RX2 - same presentation method for both receivers.
    pub(crate) smeter_source: u8,
    /// Launch Thetis on the server PC when this client starts and finds Thetis
    /// not running. Fires once per client run (see `thetis_autostart_fired`),
    /// so a deliberate shutdown from the Power button stays off.
    pub(crate) thetis_autostart: bool,
    /// The roger beep. One set of numbers, a tick per channel - see
    /// `sdr_remote_logic::roger`.
    pub(crate) roger: sdr_remote_logic::roger::RogerBeep,
    pub(crate) catsync_enabled: bool,
    pub(crate) catsync_url: String,
    /// Per-radio WebSDR URL for the two Yaesu radios (independent of Thetis and
    /// of each other). Empty = fall back to `catsync_url`.
    pub(crate) catsync_url_y1: String,
    pub(crate) catsync_url_y2: String,
    pub(crate) catsync_favorites: Vec<(String, String)>,
    /// TL2-1 ctun-auto-recenter: setup checkbox "Allow zoom below 2x (waterfall smear during tune)".
    /// Default false -> zoom-min 2x, anti-smear feature fully active.
    /// True -> zoom-min 1x allowed, smear when tuning <1.2× zoom.
    /// Server enforces strictest over all clients (as long as one client false -> server clamps to 2x).
    pub(crate) allow_zoom_below_2x: bool,

    /// PTT switch-on spike protection for a built-in speaker+mic in one chassis
    /// (tablets/laptops). When off, all gate-delays are 0 - no added latency. The
    /// per-path mic gate-delay (ms) discards the first mic audio after keyup so the
    /// speaker/chassis spike decays before it reaches TX. Thetis default 0 (the
    /// instant playback-mute is enough), Yaesu default 100 (later TX-path opening).
    pub(crate) spike_protection: bool,
    pub(crate) mic_gate_delay_thetis_ms: u32,
    pub(crate) mic_gate_delay_yaesu_ms: u32,

    /// Phase A relay monitor settings (status only; direct transport remains default).
    pub(crate) relay_enabled: bool,
    pub(crate) relay_url: String,
    pub(crate) relay_station: String,
    pub(crate) relay_token: String,
    /// Stable per-install id sent to the relay so a reconnecting client reclaims its
    /// own slot instead of piling up ghosts. Generated once on first run, persisted.
    pub(crate) relay_instance_id: String,
    /// Human device label sent to the relay (shown in relay logs/dashboard).
    /// Auto-defaulted to the hostname on first run; user-editable; may contain spaces.
    pub(crate) relay_device_name: String,
    /// PATCH-relay-audio-udp: route audio+PTT over raw UDP (port 443) instead of the
    /// wss tunnel - no retransmit, low latency. Default on. The env var
    /// THETISLINK_RELAY_UDP_PORT overrides this (testing / non-standard port).
    pub(crate) relay_udp_enabled: bool,

    /// PATCH-1: UI-language for connect-status / connect-error strings.
    /// Accepts "en" (default) or "nl". Any other value falls back to "en".
    /// TODO(future): auto-detect from OS locale.
    pub(crate) language: String,

    /// PATCH-4: count of successful connects to a real server. Used to
    /// detect "first-run" without relying on file-absence (more robust
    /// for fresh installs that already touched the config). Wizard
    /// shows on `0`; bumped to `1` (and persisted) on the first
    /// `ConnectStatus::Connected` transition. Migration-safe: missing
    /// in legacy configs ⇒ default 0 ⇒ wizard runs once on upgrade.
    pub(crate) successful_connects: u32,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            server: format!("127.0.0.1:{}", DEFAULT_PORT),
            password: String::new(),
            rx_volume: 0.2,
            tx_gain: 0.5,
            play_volume: 1.0,
            vfo_a_volume: 1.0,
            vfo_b_volume: 1.0,
            local_volume: 1.0,
            ui_zoom: 1.0,
            rx2_volume: 0.2,
            memories: Default::default(),
            tx_profiles: vec![(0, "Default".to_string())],
            input_device: String::new(),
            output_device: String::new(),
            mic_profile_map: std::collections::HashMap::new(),
            agc_enabled: false,
            theme: "classic".to_string(),
            theme_custom: String::new(),
            spectrum_enabled: false,
            spectrum_ref_db: -20.0,
            spectrum_range_db: 100.0,
            auto_ref_enabled: true,
            waterfall_contrast: 1.2,
            spectrum_max_bins: sdr_remote_core::DEFAULT_SPECTRUM_BINS as u16,
            spectrum_fft_size_k: 0,  // 0 = auto (server default)
            rx2_spectrum_fft_size_k: 0,
            spectrum_total_h: 500.0,
            spectrum_popout_pos: None,
            spectrum_popout_size: None,
            rx2_popout_pos: None,
            rx2_popout_size: None,
            popout_joined_pos: None,
            popout_joined_size: None,
            yaesu_popout_pos: None,
            yaesu_popout_size: None,
            yaesu2_popout_pos: None,
            yaesu2_popout_size: None,
            vrx_popout_pos: None,
            vrx_popout_size: None,
            vrx2_popout_pos: None,
            vrx2_popout_size: None,
            vrx1_volume: None,
            vrx1_enabled: None,
            vrx1_freq_hz: None,
            vrx1_mode: None,
            vrx2_volume: None,
            vrx2_enabled: None,
            vrx2_freq_hz: None,
            vrx2_mode: None,
            vrx1_ref_db: None,
            vrx1_range_db: None,
            vrx1_wf_contrast: None,
            vrx1_auto_ref: None,
            vrx1_filter_low_hz: None,
            vrx1_filter_high_hz: None,
            vrx1_high_res_spectrum: None,
            vrx2_ref_db: None,
            vrx2_range_db: None,
            vrx2_wf_contrast: None,
            vrx2_auto_ref: None,
            vrx2_filter_low_hz: None,
            vrx2_filter_high_hz: None,
            vrx2_high_res_spectrum: None,
            wf_contrast_per_band: HashMap::new(),
            rx2_spectrum_ref_db: -20.0,
            rx2_spectrum_range_db: 100.0,
            rx2_auto_ref_enabled: true,
            rx2_waterfall_contrast: 1.2,
            rx1_enabled: true, // default ON
            rx2_enabled: false,
            rx2_spectrum_enabled: false,
            thetis_wideband_audio: false,
            full_spectrum_enabled: true,
            popout_joined: false,
            device_tab: 0,
            yaesu_enabled: false,
            yaesu_volume: 0.05,
            yaesu_eq_profiles: Vec::new(),
            yaesu_eq_active: String::new(),
            yaesu2_eq_profiles: Vec::new(),
            yaesu2_eq_active: String::new(),
            yaesu_popout: false,
            yaesu_present_last: false,
            yaesu2_present_last: false,
            yaesu_mem_file: String::new(),
            popout_meter_analog: false,
            meter_analog: [false; 6],
            spectrum_popout: false,
            main_window_pos: None,
            ub_show_menu: false,
            collapse_diversity: false,
            collapse_yaesu_eq: false,
            collapse_yaesu_memories: false,
            collapse_yaesu_menu: false,
            collapse_yaesu2_memories: false,
            collapse_yaesu2_menu: false,
            amplitec_power_show: false,
            bw_breakdown_expanded: false,
            yaesu_memories_h: 250.0,
            yaesu2_memories_h: 250.0,
            layout_grids: String::new(),
            layout_memories: Vec::new(),
            band_mem: HashMap::new(),
            window_w: 400.0,
            window_h: 500.0,
            midi_device: String::new(),
            midi_mappings: Vec::new(),
            midi_encoder_hz: 100,
            ptt_toggle: false,
            yaesu_ptt_toggle: false,
            chat_open: false,
            yaesu2_enabled: false,
            yaesu2_ptt_toggle: false,
            yaesu2_volume: 0.05,
            yaesu_mic_gain: 0.2,
            yaesu2_mic_gain: 0.2,
            yaesu_compressor: 0,
            yaesu2_compressor: 0,
            yaesu_tx_agc: false,
            yaesu2_tx_agc: false,
            yaesu2_popout: false,
            midi_ptt_toggle: true, // MIDI defaults to toggle (existing behavior)
            smeter_source: 1,      // Avg matches pre-multi-source server default
            thetis_autostart: false, // opt-in: powers up the radio without being asked
            roger: sdr_remote_logic::roger::RogerBeep::default(),
            catsync_enabled: false,
            catsync_url: String::new(),
            catsync_url_y1: String::new(),
            catsync_url_y2: String::new(),
            catsync_favorites: Vec::new(),
            allow_zoom_below_2x: false,
            spike_protection: false,
            mic_gate_delay_thetis_ms: 0,
            mic_gate_delay_yaesu_ms: 100,
            relay_enabled: false,
            relay_url: String::new(),
            relay_station: String::new(),
            relay_token: String::new(),
            relay_instance_id: String::new(),
            relay_device_name: String::new(),
            relay_udp_enabled: true,
            language: "en".to_string(),
            successful_connects: 0,
        }
    }
}

/// Parse a `f32,f32` pair (used for popout pos / size). Returns `None` on any
/// parse error or malformed input - callers fall back to OS default placement.
fn parse_f32_pair(val: &str) -> Option<(f32, f32)> {
    let mut it = val.trim().split(',');
    let a: f32 = it.next()?.trim().parse().ok()?;
    let b: f32 = it.next()?.trim().parse().ok()?;
    Some((a, b))
}

/// Load saved window size from config (for use before app creation).
pub(crate) fn load_window_size() -> [f32; 2] {
    let config = load_config();
    [config.window_w, config.window_h]
}

/// Load saved main window position from config (for use before app creation).
/// Returns None if no position has been saved yet.
pub(crate) fn load_window_pos() -> Option<[f32; 2]> {
    load_config().main_window_pos.map(|(x, y)| [x, y])
}

/// Load config from file next to the executable.
pub(crate) fn load_config() -> ClientConfig {
    let mut config = ClientConfig::default();

    let path = match std::env::current_exe() {
        Ok(exe) => exe.with_file_name(config_file_name()),
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
        } else if let Some(val) = line.strip_prefix("relay_enabled=") {
            config.relay_enabled = val.trim() == "true";
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("relay_udp_enabled=") {
            config.relay_udp_enabled = val.trim() == "true";
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("relay_url=") {
            config.relay_url = val.trim().to_string();
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("relay_station=") {
            config.relay_station = val.trim().to_string();
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("relay_token=") {
            let v = val.trim();
            if !v.is_empty() {
                config.relay_token = sdr_remote_core::auth::deobfuscate_password(v)
                    .unwrap_or_else(|| v.to_string());
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("relay_instance_id=") {
            config.relay_instance_id = val.trim().to_string();
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("relay_device_name=") {
            config.relay_device_name = val.trim().to_string();
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
        } else if let Some(val) = line.strip_prefix("theme=") {
            config.theme = val.trim().to_string();
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("theme_custom=") {
            config.theme_custom = val.trim().to_string();
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
        } else if let Some(val) = line.strip_prefix("spectrum_total_h=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.spectrum_total_h = v.clamp(300.0, 1200.0);
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("spectrum_popout_pos=") {
            config.spectrum_popout_pos = parse_f32_pair(val);
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("spectrum_popout_size=") {
            config.spectrum_popout_size = parse_f32_pair(val);
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("rx2_popout_pos=") {
            config.rx2_popout_pos = parse_f32_pair(val);
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("rx2_popout_size=") {
            config.rx2_popout_size = parse_f32_pair(val);
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("popout_joined_pos=") {
            config.popout_joined_pos = parse_f32_pair(val);
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("popout_joined_size=") {
            config.popout_joined_size = parse_f32_pair(val);
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("yaesu_popout_pos=") {
            config.yaesu_popout_pos = parse_f32_pair(val);
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("yaesu_popout_size=") {
            config.yaesu_popout_size = parse_f32_pair(val);
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("yaesu2_popout_pos=") {
            config.yaesu2_popout_pos = parse_f32_pair(val);
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("yaesu2_popout_size=") {
            config.yaesu2_popout_size = parse_f32_pair(val);
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("vrx_popout_pos=") {
            config.vrx_popout_pos = parse_f32_pair(val);
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("vrx_popout_size=") {
            config.vrx_popout_size = parse_f32_pair(val);
        } else if let Some(val) = line.strip_prefix("vrx2_popout_pos=") {
            config.vrx2_popout_pos = parse_f32_pair(val);
        } else if let Some(val) = line.strip_prefix("vrx2_popout_size=") {
            config.vrx2_popout_size = parse_f32_pair(val);
            has_keys = true;
        // VRX1 keys - accept both legacy `vrx_*` (build 48 and earlier)
        // and new `vrx1_*` (build 50+). New writes use only `vrx1_*`.
        } else if let Some(val) = line.strip_prefix("vrx1_volume=").or_else(|| line.strip_prefix("vrx_volume=")) {
            config.vrx1_volume = val.trim().parse().ok();
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("vrx1_enabled=").or_else(|| line.strip_prefix("vrx_enabled=")) {
            config.vrx1_enabled = Some(val.trim() == "true");
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("vrx1_freq_hz=").or_else(|| line.strip_prefix("vrx_freq_hz=")) {
            config.vrx1_freq_hz = val.trim().parse().ok();
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("vrx1_mode=").or_else(|| line.strip_prefix("vrx_mode=")) {
            config.vrx1_mode = val.trim().parse().ok();
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("vrx2_volume=") {
            config.vrx2_volume = val.trim().parse().ok();
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("vrx2_enabled=") {
            config.vrx2_enabled = Some(val.trim() == "true");
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("vrx2_freq_hz=") {
            config.vrx2_freq_hz = val.trim().parse().ok();
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("vrx2_mode=") {
            config.vrx2_mode = val.trim().parse().ok();
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("vrx1_ref_db=") {
            config.vrx1_ref_db = val.trim().parse().ok();
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("vrx1_range_db=") {
            config.vrx1_range_db = val.trim().parse().ok();
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("vrx1_wf_contrast=") {
            config.vrx1_wf_contrast = val.trim().parse().ok();
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("vrx2_ref_db=") {
            config.vrx2_ref_db = val.trim().parse().ok();
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("vrx2_range_db=") {
            config.vrx2_range_db = val.trim().parse().ok();
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("vrx2_wf_contrast=") {
            config.vrx2_wf_contrast = val.trim().parse().ok();
        } else if let Some(val) = line.strip_prefix("vrx1_auto_ref=") {
            config.vrx1_auto_ref = Some(val.trim() == "true");
        } else if let Some(val) = line.strip_prefix("vrx2_auto_ref=") {
            config.vrx2_auto_ref = Some(val.trim() == "true");
        } else if let Some(val) = line.strip_prefix("vrx1_filter_low_hz=") {
            config.vrx1_filter_low_hz = val.trim().parse().ok();
        } else if let Some(val) = line.strip_prefix("vrx1_filter_high_hz=") {
            config.vrx1_filter_high_hz = val.trim().parse().ok();
        } else if let Some(val) = line.strip_prefix("vrx2_filter_low_hz=") {
            config.vrx2_filter_low_hz = val.trim().parse().ok();
        } else if let Some(val) = line.strip_prefix("vrx2_filter_high_hz=") {
            config.vrx2_filter_high_hz = val.trim().parse().ok();
        } else if let Some(val) = line.strip_prefix("vrx1_high_res_spectrum=") {
            config.vrx1_high_res_spectrum = Some(val.trim() == "true");
        } else if let Some(val) = line.strip_prefix("vrx2_high_res_spectrum=") {
            config.vrx2_high_res_spectrum = Some(val.trim() == "true");
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
        } else if let Some(val) = line.strip_prefix("main_window_pos=") {
            config.main_window_pos = parse_f32_pair(val);
        } else if let Some(val) = line.strip_prefix("spectrum_popout=") {
            config.spectrum_popout = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("device_tab=") {
            if let Ok(v) = val.trim().parse::<u8>() { config.device_tab = v; }
        } else if let Some(val) = line.strip_prefix("yaesu_enabled=") {
            config.yaesu_enabled = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("yaesu_volume=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.yaesu_volume = v.clamp(0.001, 1.0);
            }
        } else if let Some(val) = line.strip_prefix("yaesu2_volume=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.yaesu2_volume = v.clamp(0.001, 1.0);
            }
        } else if let Some(val) = line.strip_prefix("yaesu_popout=") {
            config.yaesu_popout = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("yaesu_present_last=") {
            config.yaesu_present_last = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("yaesu2_present_last=") {
            config.yaesu2_present_last = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("mic_profile=") {
            // Format: mic_device_name|tx_profile_name
            if let Some((mic, profile)) = val.trim().split_once('|') {
                config.mic_profile_map.insert(mic.to_string(), profile.to_string());
            }
        } else if let Some(val) = line.strip_prefix("yaesu_eq_active=") {
            config.yaesu_eq_active = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("yaesu_eq_profile=") {
            // Format: name|enabled|g0,g1,g2,g3,g4[|mic_gain]
            // mic_gain (4th field) is optional - old profiles without
            // mic_gain fall back to internal default 0.2 so upgrades don't
            // break.
            let parts: Vec<&str> = val.trim().splitn(4, '|').collect();
            if parts.len() >= 3 {
                let name = parts[0].to_string();
                let enabled = parts[1] == "1";
                let gains: Vec<f32> = parts[2].split(',')
                    .filter_map(|s| s.trim().parse().ok()).collect();
                if gains.len() == 5 {
                    let mic_gain = parts.get(3)
                        .and_then(|s| s.trim().parse::<f32>().ok())
                        .unwrap_or(0.2)
                        .clamp(0.02, 0.4);
                    config.yaesu_eq_profiles.push((name, enabled,
                        [gains[0], gains[1], gains[2], gains[3], gains[4]],
                        mic_gain));
                }
            }
        } else if let Some(val) = line.strip_prefix("yaesu2_eq_active=") {
            config.yaesu2_eq_active = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("yaesu2_eq_profile=") {
            let parts: Vec<&str> = val.trim().splitn(4, '|').collect();
            if parts.len() >= 3 {
                let name = parts[0].to_string();
                let enabled = parts[1] == "1";
                let gains: Vec<f32> = parts[2].split(',')
                    .filter_map(|s| s.trim().parse().ok()).collect();
                if gains.len() == 5 {
                    let mic_gain = parts.get(3)
                        .and_then(|s| s.trim().parse::<f32>().ok())
                        .unwrap_or(0.2)
                        .clamp(0.02, 0.4);
                    config.yaesu2_eq_profiles.push((name, enabled,
                        [gains[0], gains[1], gains[2], gains[3], gains[4]],
                        mic_gain));
                }
            }
        } else if let Some(val) = line.strip_prefix("yaesu_mem_file=") {
            config.yaesu_mem_file = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("rx1_enabled=") {
            config.rx1_enabled = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("rx2_spectrum_enabled=") {
            config.rx2_spectrum_enabled = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("rx2_enabled=") {
            config.rx2_enabled = val.trim() == "true";
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("thetis_wideband_audio=") {
            config.thetis_wideband_audio = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("full_spectrum_enabled=") {
            config.full_spectrum_enabled = val.trim() == "true";
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("popout_joined=") {
            config.popout_joined = val.trim() == "true";
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("popout_meter_analog=") {
            // Legacy global type -> migrate to all channels (unless an explicit
            // meter_analog line overrides this later).
            let on = val.trim() == "true";
            config.popout_meter_analog = on;
            config.meter_analog = [on; 6];
        } else if let Some(val) = line.strip_prefix("meter_analog=") {
            for (i, part) in val.trim().split(',').enumerate() {
                if i < 6 { config.meter_analog[i] = part.trim() == "1" || part.trim() == "true"; }
            }
        } else if let Some(val) = line.strip_prefix("ub_show_menu=") {
            config.ub_show_menu = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("collapse_diversity=") {
            config.collapse_diversity = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("collapse_yaesu_eq=") {
            config.collapse_yaesu_eq = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("collapse_yaesu_memories=") {
            config.collapse_yaesu_memories = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("collapse_yaesu_menu=") {
            config.collapse_yaesu_menu = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("collapse_yaesu2_memories=") {
            config.collapse_yaesu2_memories = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("collapse_yaesu2_menu=") {
            config.collapse_yaesu2_menu = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("amplitec_power_show=") {
            config.amplitec_power_show = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("bw_breakdown_expanded=") {
            config.bw_breakdown_expanded = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("layout_grids=") {
            config.layout_grids = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("layout_mem") {
            // layout_mem<N>=<name>|<key:x,y,w,h>...  N is 1-based.
            if let Some((idx, body)) = val.split_once('=') {
                if let Ok(i) = idx.trim().parse::<usize>() {
                    if (1..=64).contains(&i) {
                        if config.layout_memories.len() < i {
                            config.layout_memories.resize(i, String::new());
                        }
                        config.layout_memories[i - 1] = body.trim().to_string();
                    }
                }
            }
        } else if let Some(val) = line.strip_prefix("yaesu2_memories_h=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.yaesu2_memories_h = v.clamp(100.0, 800.0);
            }
        } else if let Some(val) = line.strip_prefix("yaesu_memories_h=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.yaesu_memories_h = v.clamp(100.0, 800.0);
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("play_volume=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.play_volume = v.clamp(0.0, 4.0);
            }
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
        } else if let Some(val) = line.strip_prefix("ui_zoom=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.ui_zoom = v.clamp(0.5, 2.0);
            }
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
        } else if let Some(val) = line.strip_prefix("chat_open=") {
            config.chat_open = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("yaesu2_enabled=") {
            config.yaesu2_enabled = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("yaesu2_ptt_toggle=") {
            config.yaesu2_ptt_toggle = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("yaesu_mic_gain=") {
            if let Ok(v) = val.trim().parse::<f32>() { config.yaesu_mic_gain = v.clamp(0.02, 0.4); }
        } else if let Some(val) = line.strip_prefix("yaesu2_mic_gain=") {
            if let Ok(v) = val.trim().parse::<f32>() { config.yaesu2_mic_gain = v.clamp(0.02, 0.4); }
        } else if let Some(val) = line.strip_prefix("yaesu_compressor=") {
            if let Ok(v) = val.trim().parse::<u8>() { config.yaesu_compressor = v.min(100); }
        } else if let Some(val) = line.strip_prefix("yaesu2_compressor=") {
            if let Ok(v) = val.trim().parse::<u8>() { config.yaesu2_compressor = v.min(100); }
        } else if let Some(val) = line.strip_prefix("yaesu_tx_agc=") {
            config.yaesu_tx_agc = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("yaesu2_tx_agc=") {
            config.yaesu2_tx_agc = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("yaesu2_popout=") {
            config.yaesu2_popout = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("midi_ptt_toggle=") {
            config.midi_ptt_toggle = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("thetis_autostart=") {
            config.thetis_autostart = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("roger_freq_hz=") {
            if let Ok(v) = val.trim().parse() { config.roger.freq_hz = v; }
        } else if let Some(val) = line.strip_prefix("roger_volume=") {
            if let Ok(v) = val.trim().parse() { config.roger.volume = v; }
        } else if let Some(val) = line.strip_prefix("roger_duration_ms=") {
            if let Ok(v) = val.trim().parse() { config.roger.duration_ms = v; }
        } else if let Some(val) = line.strip_prefix("roger_include_fm=") {
            config.roger.include_fm = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("roger_on_thetis=") {
            config.roger.on_thetis = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("roger_on_radio1=") {
            config.roger.on_radio1 = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("roger_on_radio2=") {
            config.roger.on_radio2 = val.trim() == "true";
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("smeter_source=") {
            if let Ok(v) = val.trim().parse::<u8>() {
                if v <= 2 { config.smeter_source = v; }
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("allow_zoom_below_2x=") {
            config.allow_zoom_below_2x = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("spike_protection=") {
            config.spike_protection = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("mic_gate_delay_thetis_ms=") {
            if let Ok(v) = val.trim().parse::<u32>() { config.mic_gate_delay_thetis_ms = v.min(800); }
        } else if let Some(val) = line.strip_prefix("mic_gate_delay_yaesu_ms=") {
            if let Ok(v) = val.trim().parse::<u32>() { config.mic_gate_delay_yaesu_ms = v.min(800); }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("language=") {
            let v = val.trim().to_lowercase();
            // Accept the UI languages that have a locale (en base + nl/de/fr); unknown -> en.
            config.language = match v.as_str() {
                "nl" | "de" | "fr" => v,
                _ => "en".to_string(),
            };
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("successful_connects=") {
            // PATCH-4: parse as u32 with default 0 on malformed input.
            config.successful_connects = val.trim().parse::<u32>().unwrap_or(0);
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("catsync_enabled=") {
            config.catsync_enabled = val.trim() == "true";
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("catsync_url_y1=") {
            let v = val.trim();
            if !v.is_empty() {
                config.catsync_url_y1 = v.to_string();
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("catsync_url_y2=") {
            let v = val.trim();
            if !v.is_empty() {
                config.catsync_url_y2 = v.to_string();
            }
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
    // Ensure a stable per-install relay id exists (generate + persist on first run)
    // so a reconnecting client reclaims its own relay slot instead of piling up.
    if config.relay_instance_id.trim().is_empty() {
        config.relay_instance_id = generate_instance_id();
        save_relay_instance_id(&config.relay_instance_id);
    }
    // Auto-default the device name to the hostname on first run (user-editable later).
    if config.relay_device_name.trim().is_empty() {
        config.relay_device_name = generate_device_name();
        save_relay_device_name(&config.relay_device_name);
    }
    config
}

/// Auto-default device label = the computer's hostname (Windows COMPUTERNAME, else
/// HOSTNAME), or a generic fallback. A device label, not the login username.
fn generate_device_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Desktop".to_string())
}

/// Persist the device name (read-modify-write on its single key). May contain spaces.
pub(crate) fn save_relay_device_name(name: &str) {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    let path = exe.with_file_name(config_file_name());
    let new_line = format!("relay_device_name={}", name.trim());
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut found = false;
    let mut updated: Vec<String> = existing
        .lines()
        .map(|l| {
            if l.starts_with("relay_device_name=") {
                found = true;
                new_line.clone()
            } else {
                l.to_string()
            }
        })
        .collect();
    if !found {
        updated.push(new_line);
    }
    let _ = std::fs::write(path, updated.join("\n") + "\n");
}

/// Generate a stable-enough per-install id (first-run timestamp + pid, hex). Kept
/// once in the config file; only needs to be unique per install, not secret.
fn generate_instance_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("d{nanos:x}{:x}", std::process::id())
}

/// Persist the per-install relay id (read-modify-write on its single key).
fn save_relay_instance_id(id: &str) {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    let path = exe.with_file_name(config_file_name());
    let new_line = format!("relay_instance_id={id}");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut found = false;
    let mut updated: Vec<String> = existing
        .lines()
        .map(|l| {
            if l.starts_with("relay_instance_id=") {
                found = true;
                new_line.clone()
            } else {
                l.to_string()
            }
        })
        .collect();
    if !found {
        updated.push(new_line);
    }
    let _ = std::fs::write(path, updated.join("\n") + "\n");
}

/// Save config to file next to the executable.
pub(crate) fn save_config(
    server: &str,
    password: &str,
    volume: f32,
    tx_gain: f32,
    play_volume: f32,
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
    spectrum_total_h: f32,
    spectrum_popout_pos: Option<(f32, f32)>,
    spectrum_popout_size: Option<(f32, f32)>,
    rx2_popout_pos: Option<(f32, f32)>,
    rx2_popout_size: Option<(f32, f32)>,
    popout_joined_pos: Option<(f32, f32)>,
    popout_joined_size: Option<(f32, f32)>,
    yaesu_popout_pos_arg: Option<(f32, f32)>,
    yaesu_popout_size_arg: Option<(f32, f32)>,
    yaesu2_popout_pos_arg: Option<(f32, f32)>,
    yaesu2_popout_size_arg: Option<(f32, f32)>,
    vrx_popout_pos_arg: Option<(f32, f32)>,
    vrx_popout_size_arg: Option<(f32, f32)>,
    vrx2_popout_pos_arg: Option<(f32, f32)>,
    vrx2_popout_size_arg: Option<(f32, f32)>,
    vrx1_volume_arg: Option<f32>,
    vrx1_enabled_arg: Option<bool>,
    vrx1_freq_hz_arg: Option<u64>,
    vrx1_mode_arg: Option<u8>,
    vrx2_volume_arg: Option<f32>,
    vrx2_enabled_arg: Option<bool>,
    vrx2_freq_hz_arg: Option<u64>,
    vrx2_mode_arg: Option<u8>,
    vrx1_ref_db_arg: Option<f32>,
    vrx1_range_db_arg: Option<f32>,
    vrx1_wf_contrast_arg: Option<f32>,
    vrx1_auto_ref_arg: Option<bool>,
    vrx1_filter_low_hz_arg: Option<i32>,
    vrx1_filter_high_hz_arg: Option<i32>,
    vrx1_high_res_spectrum_arg: Option<bool>,
    vrx2_ref_db_arg: Option<f32>,
    vrx2_range_db_arg: Option<f32>,
    vrx2_wf_contrast_arg: Option<f32>,
    vrx2_auto_ref_arg: Option<bool>,
    vrx2_filter_low_hz_arg: Option<i32>,
    vrx2_filter_high_hz_arg: Option<i32>,
    vrx2_high_res_spectrum_arg: Option<bool>,
    wf_contrast_per_band: &HashMap<String, f32>,
    rx2_spectrum_ref_db: f32,
    rx2_spectrum_range_db: f32,
    rx2_auto_ref_enabled: bool,
    rx2_waterfall_contrast: f32,
    rx1_enabled: bool,
    rx2_enabled: bool,
    rx2_spectrum_enabled: bool,
    thetis_wideband_audio: bool,
    full_spectrum_enabled: bool,
    ui_zoom: f32,
    popout_joined: bool,
    meter_analog: [bool; 6],
    spectrum_popout: bool,
    main_window_pos: Option<(f32, f32)>,
    ub_show_menu: bool,
    collapse_diversity: bool,
    collapse_yaesu_eq: bool,
    collapse_yaesu_memories: bool,
    collapse_yaesu_menu: bool,
    collapse_yaesu2_memories: bool,
    collapse_yaesu2_menu: bool,
    amplitec_power_show: bool,
    bw_breakdown_expanded: bool,
    yaesu_memories_h: f32,
    yaesu2_memories_h: f32,
    layout_grids: &str,
    layout_memories: &[String],
    device_tab: u8,
    yaesu_enabled: bool,
    yaesu_volume: f32,
    yaesu2_volume: f32,
    yaesu_popout: bool,
    yaesu_eq_active: &str,
    yaesu_eq_profiles: &[(String, bool, [f32; 5], f32)],
    yaesu2_eq_active: &str,
    yaesu2_eq_profiles: &[(String, bool, [f32; 5], f32)],
    yaesu_mem_file: &str,
    band_mem: &HashMap<String, BandMemory>,
    window_w: f32,
    window_h: f32,
    midi_device: &str,
    midi_mappings: &[crate::midi::MidiMapping],
    midi_encoder_hz: u64,
    catsync_enabled: bool,
    catsync_url: &str,
    catsync_url_y1: &str,
    catsync_url_y2: &str,
    catsync_favorites: &[(String, String)],
    mic_profile_map: &std::collections::HashMap<String, String>,
    theme: &str,
    theme_custom: &str,
    language: &str,
    yaesu_present_last: bool,
    yaesu2_present_last: bool,
) {
    if let Ok(exe) = std::env::current_exe() {
        let path = exe.with_file_name(config_file_name());
        let pw_enc = if password.is_empty() { String::new() } else { sdr_remote_core::auth::obfuscate_password(password) };
        let mut content = format!("server={}\npassword={}\nvolume={:.2}\ntx_gain={:.2}\nplay_volume={:.2}\nvfo_a_volume={:.2}\nvfo_b_volume={:.2}\nlocal_volume={:.2}\nrx2_volume={:.2}\n",
            server, pw_enc, volume, tx_gain, play_volume, vfo_a_volume, vfo_b_volume, local_volume, rx2_volume);
        if !input_device.is_empty() {
            content.push_str(&format!("input_device={}\n", input_device));
        }
        if !output_device.is_empty() {
            content.push_str(&format!("output_device={}\n", output_device));
        }
        content.push_str(&format!("agc_enabled={}\n", agc_enabled));
        content.push_str(&format!("theme={}\n", theme));
        content.push_str(&format!("language={}\n", language));
        if !theme_custom.is_empty() {
            content.push_str(&format!("theme_custom={}\n", theme_custom));
        }
        content.push_str(&format!("spectrum_enabled={}\n", spectrum_enabled));
        content.push_str(&format!("spectrum_ref_db={:.0}\n", spectrum_ref_db));
        content.push_str(&format!("spectrum_range_db={:.0}\n", spectrum_range_db));
        content.push_str(&format!("auto_ref_enabled={}\n", auto_ref_enabled));
        content.push_str(&format!("spectrum_max_bins={}\n", spectrum_max_bins));
        content.push_str(&format!("spectrum_fft_size_k={}\n", spectrum_fft_size_k));
        content.push_str(&format!("rx2_spectrum_fft_size_k={}\n", rx2_spectrum_fft_size_k));
        content.push_str(&format!("spectrum_total_h={:.0}\n", spectrum_total_h));
        if let Some((x, y)) = spectrum_popout_pos {
            content.push_str(&format!("spectrum_popout_pos={:.0},{:.0}\n", x, y));
        }
        if let Some((w, h)) = spectrum_popout_size {
            content.push_str(&format!("spectrum_popout_size={:.0},{:.0}\n", w, h));
        }
        if let Some((x, y)) = rx2_popout_pos {
            content.push_str(&format!("rx2_popout_pos={:.0},{:.0}\n", x, y));
        }
        if let Some((w, h)) = rx2_popout_size {
            content.push_str(&format!("rx2_popout_size={:.0},{:.0}\n", w, h));
        }
        if let Some((x, y)) = popout_joined_pos {
            content.push_str(&format!("popout_joined_pos={:.0},{:.0}\n", x, y));
        }
        if let Some((w, h)) = popout_joined_size {
            content.push_str(&format!("popout_joined_size={:.0},{:.0}\n", w, h));
        }
        if let Some((x, y)) = yaesu_popout_pos_arg {
            content.push_str(&format!("yaesu_popout_pos={:.0},{:.0}\n", x, y));
        }
        if let Some((w, h)) = yaesu_popout_size_arg {
            content.push_str(&format!("yaesu_popout_size={:.0},{:.0}\n", w, h));
        }
        if let Some((x, y)) = yaesu2_popout_pos_arg {
            content.push_str(&format!("yaesu2_popout_pos={:.0},{:.0}\n", x, y));
        }
        if let Some((w, h)) = yaesu2_popout_size_arg {
            content.push_str(&format!("yaesu2_popout_size={:.0},{:.0}\n", w, h));
        }
        if let Some((x, y)) = vrx_popout_pos_arg {
            content.push_str(&format!("vrx_popout_pos={:.0},{:.0}\n", x, y));
        }
        if let Some((w, h)) = vrx_popout_size_arg {
            content.push_str(&format!("vrx_popout_size={:.0},{:.0}\n", w, h));
        }
        if let Some((x, y)) = vrx2_popout_pos_arg {
            content.push_str(&format!("vrx2_popout_pos={:.0},{:.0}\n", x, y));
        }
        if let Some((w, h)) = vrx2_popout_size_arg {
            content.push_str(&format!("vrx2_popout_size={:.0},{:.0}\n", w, h));
        }
        if let Some(v) = vrx1_volume_arg {
            content.push_str(&format!("vrx1_volume={:.3}\n", v));
        }
        if let Some(v) = vrx1_enabled_arg {
            content.push_str(&format!("vrx1_enabled={}\n", v));
        }
        if let Some(v) = vrx1_freq_hz_arg {
            content.push_str(&format!("vrx1_freq_hz={}\n", v));
        }
        if let Some(v) = vrx1_mode_arg {
            content.push_str(&format!("vrx1_mode={}\n", v));
        }
        if let Some(v) = vrx2_volume_arg {
            content.push_str(&format!("vrx2_volume={:.3}\n", v));
        }
        if let Some(v) = vrx2_enabled_arg {
            content.push_str(&format!("vrx2_enabled={}\n", v));
        }
        if let Some(v) = vrx2_freq_hz_arg {
            content.push_str(&format!("vrx2_freq_hz={}\n", v));
        }
        if let Some(v) = vrx2_mode_arg {
            content.push_str(&format!("vrx2_mode={}\n", v));
        }
        if let Some(v) = vrx1_ref_db_arg {
            content.push_str(&format!("vrx1_ref_db={:.1}\n", v));
        }
        if let Some(v) = vrx1_range_db_arg {
            content.push_str(&format!("vrx1_range_db={:.1}\n", v));
        }
        if let Some(v) = vrx1_wf_contrast_arg {
            content.push_str(&format!("vrx1_wf_contrast={:.2}\n", v));
        }
        if let Some(v) = vrx2_ref_db_arg {
            content.push_str(&format!("vrx2_ref_db={:.1}\n", v));
        }
        if let Some(v) = vrx2_range_db_arg {
            content.push_str(&format!("vrx2_range_db={:.1}\n", v));
        }
        if let Some(v) = vrx2_wf_contrast_arg {
            content.push_str(&format!("vrx2_wf_contrast={:.2}\n", v));
        }
        if let Some(v) = vrx1_auto_ref_arg {
            content.push_str(&format!("vrx1_auto_ref={}\n", v));
        }
        if let Some(v) = vrx2_auto_ref_arg {
            content.push_str(&format!("vrx2_auto_ref={}\n", v));
        }
        if let Some(v) = vrx1_filter_low_hz_arg {
            content.push_str(&format!("vrx1_filter_low_hz={}\n", v));
        }
        if let Some(v) = vrx1_filter_high_hz_arg {
            content.push_str(&format!("vrx1_filter_high_hz={}\n", v));
        }
        if let Some(v) = vrx2_filter_low_hz_arg {
            content.push_str(&format!("vrx2_filter_low_hz={}\n", v));
        }
        if let Some(v) = vrx2_filter_high_hz_arg {
            content.push_str(&format!("vrx2_filter_high_hz={}\n", v));
        }
        if let Some(v) = vrx1_high_res_spectrum_arg {
            content.push_str(&format!("vrx1_high_res_spectrum={}\n", v));
        }
        if let Some(v) = vrx2_high_res_spectrum_arg {
            content.push_str(&format!("vrx2_high_res_spectrum={}\n", v));
        }
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
        content.push_str(&format!("rx1_enabled={}\n", rx1_enabled));
        content.push_str(&format!("rx2_enabled={}\n", rx2_enabled));
        content.push_str(&format!("rx2_spectrum_enabled={}\n", rx2_spectrum_enabled));
        content.push_str(&format!("thetis_wideband_audio={}\n", thetis_wideband_audio));
        content.push_str(&format!("full_spectrum_enabled={}\n", full_spectrum_enabled));
        content.push_str(&format!("ui_zoom={:.2}\n", ui_zoom));
        content.push_str(&format!("popout_joined={}\n", popout_joined));
        content.push_str(&format!("meter_analog={}\n",
            meter_analog.iter().map(|b| if *b { "1" } else { "0" }).collect::<Vec<_>>().join(",")));
        content.push_str(&format!("spectrum_popout={}\n", spectrum_popout));
        if let Some((x, y)) = main_window_pos {
            content.push_str(&format!("main_window_pos={:.0},{:.0}\n", x, y));
        }
        content.push_str(&format!("ub_show_menu={}\n", ub_show_menu));
        content.push_str(&format!("collapse_diversity={}\n", collapse_diversity));
        content.push_str(&format!("collapse_yaesu_eq={}\n", collapse_yaesu_eq));
        content.push_str(&format!("collapse_yaesu_memories={}\n", collapse_yaesu_memories));
        content.push_str(&format!("collapse_yaesu_menu={}\n", collapse_yaesu_menu));
        content.push_str(&format!("collapse_yaesu2_memories={}\n", collapse_yaesu2_memories));
        content.push_str(&format!("collapse_yaesu2_menu={}\n", collapse_yaesu2_menu));
        content.push_str(&format!("amplitec_power_show={}\n", amplitec_power_show));
        content.push_str(&format!("bw_breakdown_expanded={}\n", bw_breakdown_expanded));
        content.push_str(&format!("yaesu_memories_h={:.0}\n", yaesu_memories_h));
        content.push_str(&format!("yaesu2_memories_h={:.0}\n", yaesu2_memories_h));
        if !layout_grids.is_empty() {
            content.push_str(&format!("layout_grids={}\n", layout_grids));
        }
        for (i, mem) in layout_memories.iter().enumerate() {
            if !mem.is_empty() {
                content.push_str(&format!("layout_mem{}={}\n", i + 1, mem));
            }
        }
        content.push_str(&format!("device_tab={}\n", device_tab));
        content.push_str(&format!("yaesu_enabled={}\n", yaesu_enabled));
        content.push_str(&format!("yaesu_volume={:.3}\n", yaesu_volume));
        content.push_str(&format!("yaesu2_volume={:.3}\n", yaesu2_volume));
        content.push_str(&format!("yaesu_popout={}\n", yaesu_popout));
        content.push_str(&format!("yaesu_present_last={}\n", yaesu_present_last));
        content.push_str(&format!("yaesu2_present_last={}\n", yaesu2_present_last));
        content.push_str(&format!("yaesu_eq_active={}\n", yaesu_eq_active));
        for (name, enabled, gains, mic_gain) in yaesu_eq_profiles {
            content.push_str(&format!("yaesu_eq_profile={}|{}|{:.1},{:.1},{:.1},{:.1},{:.1}|{:.3}\n",
                name, if *enabled { "1" } else { "0" },
                gains[0], gains[1], gains[2], gains[3], gains[4], mic_gain));
        }
        content.push_str(&format!("yaesu2_eq_active={}\n", yaesu2_eq_active));
        for (name, enabled, gains, mic_gain) in yaesu2_eq_profiles {
            content.push_str(&format!("yaesu2_eq_profile={}|{}|{:.1},{:.1},{:.1},{:.1},{:.1}|{:.3}\n",
                name, if *enabled { "1" } else { "0" },
                gains[0], gains[1], gains[2], gains[3], gains[4], mic_gain));
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
        if !catsync_url_y1.is_empty() {
            content.push_str(&format!("catsync_url_y1={}\n", catsync_url_y1));
        }
        if !catsync_url_y2.is_empty() {
            content.push_str(&format!("catsync_url_y2={}\n", catsync_url_y2));
        }
        for (label, url) in catsync_favorites {
            content.push_str(&format!("catsync_fav={}|{}\n", label, url));
        }
        for (i, mapping) in midi_mappings.iter().enumerate() {
            content.push_str(&format!("midi_map_{}={}\n", i, mapping.to_config()));
        }
        // Preserve toggles that are managed via save_ptt_config (read-modify-append),
        // so this full rewrite doesn't drop them. yaesu2_enabled/
        // _ptt_toggle/_popout + the mic-gains explicitly belong here - otherwise
        // the radio2 enable/window/mic state disappears on every other change.
        if let Ok(existing) = std::fs::read_to_string(&path) {
            for line in existing.lines() {
                if line.starts_with("ptt_toggle=") || line.starts_with("yaesu_ptt_toggle=") || line.starts_with("midi_ptt_toggle=")
                    || line.starts_with("allow_zoom_below_2x=")
                    || line.starts_with("spike_protection=")
                    || line.starts_with("mic_gate_delay_thetis_ms=")
                    || line.starts_with("mic_gate_delay_yaesu_ms=")
                    || line.starts_with("smeter_source=")
                    || line.starts_with("thetis_autostart=")
                    || line.starts_with("successful_connects=")
                    || line.starts_with("relay_enabled=")
                    || line.starts_with("relay_udp_enabled=")
                    || line.starts_with("relay_url=")
                    || line.starts_with("relay_station=")
                    || line.starts_with("relay_token=")
                    || line.starts_with("relay_instance_id=")
                    || line.starts_with("relay_device_name=")
                    || line.starts_with("yaesu2_enabled=")
                    || line.starts_with("yaesu2_ptt_toggle=")
                    || line.starts_with("yaesu2_popout=")
                    || line.starts_with("yaesu_mic_gain=")
                    || line.starts_with("yaesu2_mic_gain=")
                    || line.starts_with("yaesu_compressor=")
                    || line.starts_with("yaesu2_compressor=")
                    || line.starts_with("yaesu_tx_agc=")
                    || line.starts_with("yaesu2_tx_agc=")
                    // The roger beep. Written one key at a time like the rest
                    // of this list, and therefore invisible to the rewrite
                    // above unless it is named here - which is why the ticks
                    // were forgotten on restart (2026-08-14). One prefix rather
                    // than seven lines, so a settings added later cannot be
                    // half-remembered.
                    || line.starts_with("roger_") {
                    content.push_str(line);
                    content.push('\n');
                }
            }
        }
        let _ = std::fs::write(path, sdr_remote_core::conf_layout::group(&content, SECTIONS, UNSORTED));
    }
}

/// The trailing heading for keys this table has no home for. A key landing
/// here is not an error - it is the reminder that it needs one.
const UNSORTED: &str = "Not sorted yet";

/// How the client's settings file reads. See `conf_layout`: the longest
/// matching prefix wins, so a specific key beats the family it sits in.
///
/// Window geometry is deliberately NOT filed with its own device. Coordinates
/// are only worth reading against each other - a window that ended up on
/// another monitor stands out in a column of positions and disappears
/// entirely when it sits under its own heading.
const SECTIONS: &[sdr_remote_core::conf_layout::Section] = &[
    ("Connection", &["server", "password", "relay_", "successful_connects", "thetis_autostart"]),
    ("Audio", &[
        "volume", "play_volume", "tx_gain", "vfo_a_volume", "vfo_b_volume", "local_volume",
        "rx2_volume", "input_device", "output_device", "agc_enabled", "thetis_wideband_audio",
        "mic_profile", "mic_gate_delay_", "spike_protection", "roger_",
    ]),
    ("PTT", &["ptt_toggle", "midi_ptt_toggle", "yaesu_ptt_toggle", "yaesu2_ptt_toggle"]),
    ("RX1 spectrum and waterfall", &[
        "spectrum_", "rx1_", "waterfall_contrast", "wf_contrast_per_band", "auto_ref_enabled",
        "full_spectrum_enabled", "allow_zoom_below_2x",
    ]),
    ("RX2 spectrum and waterfall", &["rx2_"]),
    ("VRX", &["vrx_", "vrx1_", "vrx2_"]),
    ("Yaesu radio 1", &["yaesu_"]),
    ("Yaesu radio 2", &["yaesu2_"]),
    ("Memories", &["mem", "band_mem_", "layout_mem"]),
    ("CAT sync", &["catsync_"]),
    ("MIDI", &["midi_"]),
    ("Window positions and sizes", &[
        "window_w", "window_h", "main_window_pos", "spectrum_total_h",
        "spectrum_popout_pos", "spectrum_popout_size", "rx2_popout_pos", "rx2_popout_size",
        "vrx_popout_pos", "vrx_popout_size", "vrx2_popout_pos", "vrx2_popout_size",
        "yaesu_popout_pos", "yaesu_popout_size", "yaesu2_popout_pos", "yaesu2_popout_size",
        "popout_joined_pos", "popout_joined_size", "yaesu_memories_h", "yaesu2_memories_h",
    ]),
    ("Window visibility", &[
        "spectrum_popout", "yaesu_popout", "yaesu2_popout", "popout_joined", "collapse_",
        "device_tab", "meter_analog", "bw_breakdown_expanded", "amplitec_power_show",
        "ub_show_menu",
    ]),
    ("Appearance", &["theme", "language", "ui_zoom", "layout_grids"]),
];

/// Phase A relay monitor: persist relay settings without touching the large
/// direct-connection config rewrite path.
pub(crate) fn save_relay_config(
    enabled: bool,
    url: &str,
    station: &str,
    token: &str,
    udp_enabled: bool,
) {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    let path = exe.with_file_name(config_file_name());
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let token_enc = if token.is_empty() {
        String::new()
    } else {
        sdr_remote_core::auth::obfuscate_password(token)
    };
    let desired = [
        ("relay_enabled", enabled.to_string()),
        ("relay_url", url.trim().to_string()),
        ("relay_station", station.trim().to_string()),
        ("relay_token", token_enc),
        ("relay_udp_enabled", udp_enabled.to_string()),
    ];
    let mut lines: Vec<String> = existing.lines().map(|line| line.to_string()).collect();
    for (key, value) in desired {
        let prefix = format!("{}=", key);
        if let Some(line) = lines.iter_mut().find(|line| line.starts_with(&prefix)) {
            *line = format!("{}{}", prefix, value);
        } else {
            lines.push(format!("{}{}", prefix, value));
        }
    }
    let _ = std::fs::write(path, lines.join("\n") + "\n");
}
/// Persist PTT spike-protection settings (built-in speaker+mic in one chassis).
/// Read-modify-write on the three keys so other config keys are untouched.
pub(crate) fn save_spike_protection(enabled: bool, thetis_ms: u32, yaesu_ms: u32) {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    let path = exe.with_file_name(config_file_name());
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let desired = [
        ("spike_protection", enabled.to_string()),
        ("mic_gate_delay_thetis_ms", thetis_ms.to_string()),
        ("mic_gate_delay_yaesu_ms", yaesu_ms.to_string()),
    ];
    let mut lines: Vec<String> = existing.lines().map(|line| line.to_string()).collect();
    for (key, value) in desired {
        let prefix = format!("{}=", key);
        if let Some(line) = lines.iter_mut().find(|line| line.starts_with(&prefix)) {
            *line = format!("{}{}", prefix, value);
        } else {
            lines.push(format!("{}{}", prefix, value));
        }
    }
    let _ = std::fs::write(path, lines.join("\n") + "\n");
}

/// TL2-1 ctun-auto-recenter: persist setup checkbox "Allow zoom below 2x" to config file.
/// Read-modify-write on the `allow_zoom_below_2x=` line without touching other keys.
pub(crate) fn save_allow_zoom_below_2x(allow: bool) {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    let path = exe.with_file_name(config_file_name());
    let new_line = format!("allow_zoom_below_2x={}", allow);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut found = false;
    let mut updated_lines: Vec<String> = existing
        .lines()
        .map(|l| {
            if l.starts_with("allow_zoom_below_2x=") {
                found = true;
                new_line.clone()
            } else {
                l.to_string()
            }
        })
        .collect();
    if !found {
        updated_lines.push(new_line);
    }
    let _ = std::fs::write(path, updated_lines.join("\n") + "\n");
}

/// Persist one `key=value` line, leaving every other key untouched
/// (read-modify-write). For settings that live outside the full-config
/// rewrite; those keys must also be listed in the preserve-block of
/// `save_full_config` so the rewrite does not drop them.
fn save_single_key(key: &str, value: &str) {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    let path = exe.with_file_name(config_file_name());
    let prefix = format!("{}=", key);
    let new_line = format!("{}={}", key, value);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut found = false;
    let mut updated_lines: Vec<String> = existing
        .lines()
        .map(|l| {
            if l.starts_with(&prefix) {
                found = true;
                new_line.clone()
            } else {
                l.to_string()
            }
        })
        .collect();
    if !found {
        updated_lines.push(new_line);
    }
    let _ = std::fs::write(path, updated_lines.join("\n") + "\n");
}

/// Persist the S-meter source choice (0=Sig, 1=Avg, 2=MaxBin) as a single
/// Write the roger beep back, all seven keys at once.
///
/// Seven `save_single_key` calls rather than one combined line, because the
/// file is a flat list of `key=value` and a reader that only knows some of
/// these keys must still be able to read the rest.
pub(crate) fn save_roger(r: &sdr_remote_logic::roger::RogerBeep) {
    save_single_key("roger_freq_hz", &format!("{:.0}", r.freq_hz));
    save_single_key("roger_volume", &format!("{:.3}", r.volume));
    save_single_key("roger_duration_ms", &r.duration_ms.to_string());
    save_single_key("roger_include_fm", if r.include_fm { "true" } else { "false" });
    save_single_key("roger_on_thetis", if r.on_thetis { "true" } else { "false" });
    save_single_key("roger_on_radio1", if r.on_radio1 { "true" } else { "false" });
    save_single_key("roger_on_radio2", if r.on_radio2 { "true" } else { "false" });
}

/// `smeter_source=N` line. Called whenever the user changes the source in
/// the Thetis tab.
pub(crate) fn save_smeter_source(source: u8) {
    save_single_key("smeter_source", &source.to_string());
}

/// Persist the "start Thetis when this client starts" choice as a single
/// `thetis_autostart=` line. Called from the checkbox in the Thetis tab.
pub(crate) fn save_thetis_autostart(on: bool) {
    save_single_key("thetis_autostart", if on { "true" } else { "false" });
}

/// PATCH-4: detect first-run for the connection wizard. Returns true when:
///  - the config file is absent, OR
///  - it exists but `successful_connects == 0` (default for fresh installs
///    *and* for legacy configs upgrading to this version).
/// File-read errors are treated as first-run because we cannot prove the
/// user has connected before.
pub(crate) fn is_first_run() -> bool {
    let config = load_config();
    config.successful_connects == 0
}

/// PATCH-4: bump `successful_connects` to (at least) 1 on the first
/// `ConnectStatus::Connected` transition. Read-modify-write so other
/// fields are not touched. No-op if the value is already > 0 - keeps
/// the wizard from re-arming if the user clears their server field
/// mid-session.
pub(crate) fn mark_successful_connect() {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    let path = exe.with_file_name(config_file_name());
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    // Parse current value (if any). Default 0 covers both "missing key"
    // and "malformed value".
    let mut current: u32 = 0;
    for line in existing.lines() {
        if let Some(v) = line.strip_prefix("successful_connects=") {
            current = v.trim().parse::<u32>().unwrap_or(0);
            break;
        }
    }
    if current >= 1 {
        return;
    }
    let new_value = current.saturating_add(1).max(1);
    let new_line = format!("successful_connects={}", new_value);
    let mut found = false;
    let mut updated_lines: Vec<String> = existing
        .lines()
        .map(|l| {
            if l.starts_with("successful_connects=") {
                found = true;
                new_line.clone()
            } else {
                l.to_string()
            }
        })
        .collect();
    if !found {
        updated_lines.push(new_line);
    }
    let _ = std::fs::write(path, updated_lines.join("\n") + "\n");
}

#[cfg(test)]
mod conf_section_tests {
    use super::{SECTIONS, UNSORTED};

    /// Which heading a key ends up under.
    fn heading_of(key: &str) -> String {
        let grouped = sdr_remote_core::conf_layout::group(&format!("{key}=x\n"), SECTIONS, UNSORTED);
        grouped
            .lines()
            .next()
            .unwrap_or_default()
            .trim_matches(|c| c == '#' || c == ' ' || c == '-')
            .to_string()
    }

    /// Each of these matches two prefixes, and would land in the wrong place
    /// under a first-match rule.
    #[test]
    fn the_specific_key_beats_the_family_it_sits_in() {
        assert_eq!(heading_of("rx2_volume"), "Audio");
        assert_eq!(heading_of("rx2_spectrum_fft_size_k"), "RX2 spectrum and waterfall");
        assert_eq!(heading_of("yaesu_ptt_toggle"), "PTT");
        assert_eq!(heading_of("yaesu_mic_gain"), "Yaesu radio 1");
        assert_eq!(heading_of("yaesu_memories_h"), "Window positions and sizes");
    }

    /// The pair that reads almost the same and belongs apart: where a window
    /// is, versus whether it is open.
    #[test]
    fn a_position_is_not_a_visibility() {
        assert_eq!(heading_of("spectrum_popout_pos"), "Window positions and sizes");
        assert_eq!(heading_of("spectrum_popout"), "Window visibility");
        assert_eq!(heading_of("popout_joined_size"), "Window positions and sizes");
        assert_eq!(heading_of("popout_joined"), "Window visibility");
    }

    /// Radio 2's keys are not radio 1's keys with a digit in them.
    #[test]
    fn the_two_radios_do_not_share_a_heading() {
        assert_eq!(heading_of("yaesu_enabled"), "Yaesu radio 1");
        assert_eq!(heading_of("yaesu2_enabled"), "Yaesu radio 2");
    }

    /// Each receiver's spectrum settings sit with that receiver. The zoom and
    /// the pan are not among them: they are not written to this file at all,
    /// so that a restart always opens the same way.
    #[test]
    fn the_receivers_view_sits_with_its_receiver() {
        assert_eq!(heading_of("spectrum_ref_db"), "RX1 spectrum and waterfall");
        assert_eq!(heading_of("rx1_enabled"), "RX1 spectrum and waterfall");
        assert_eq!(heading_of("rx2_spectrum_ref_db"), "RX2 spectrum and waterfall");
    }
}
