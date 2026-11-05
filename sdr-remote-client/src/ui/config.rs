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
            "Profile {:?}: no default config to seed from; starting from defaults",
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
    pub(crate) spectrum_fill_percent: u8,
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
    // Every remembered chevron, groups and fold-outs alike (`Fold`).
    pub(crate) folds_open: FoldsOpen,
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
    /// What the server last said about Thetis and about a second receiver.
    ///
    /// Both seed the pre-connect display, the way `yaesu_present_last` seeds the
    /// radios. They used to be hardcoded optimistic: the client opened claiming
    /// Thetis and RX2 were there, so a bare installation showed the Radio and
    /// Thetis tabs plus RX1/RX2/VRX1/VRX2 rows until a connect took them away
    /// again. Every restart put them back (2026-08-20). Remembering what the
    /// server actually reported keeps a real station looking unchanged and a
    /// bare one bare.
    pub(crate) thetis_configured_last: bool,
    pub(crate) rx2_present_last: bool,
    /// Ids of administrator answers the reader folded away, comma-separated.
    pub(crate) chat_answers_seen: String,
    pub(crate) yaesu_eq_profiles: Vec<EqProfile>,
    pub(crate) yaesu_eq_active: String,
    pub(crate) yaesu2_eq_profiles: Vec<EqProfile>,
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
    pub(crate) multi_tx: bool,
    pub(crate) chat_open: bool,
    pub(crate) midi_ptt_toggle: bool,
    // The Bluetooth PTT button (ui/ble_screen.rs). Address as six hex bytes;
    // 0 = no button chosen. Target in PttTarget order: 0 Thetis, 1 Yaesu 1, 2 Yaesu 2.
    pub(crate) ble_ptt_enabled: bool,
    pub(crate) ble_ptt_address: u64,
    pub(crate) ble_ptt_name: String,
    pub(crate) ble_ptt_target: u8,
    pub(crate) ble_ptt_toggle: bool,
    // The Morse paddle on an MCP2221A (ui/paddle_screen.rs). The switch and
    // the sidetone; the keyer's own settings are in `cw_keyer` below.
    pub(crate) cw_paddle_enabled: bool,
    pub(crate) cw_sidetone: sdr_remote_logic::sidetone::SidetoneSettings,
    /// Speed, paddle mode, reversed paddles, straight key: the keyer's own
    /// settings. Speed is local for now; mirroring
    /// the radio's speed is a later step.
    pub(crate) cw_keyer: sdr_remote_logic::cw_keyer::KeyerSettings,
    /// Which transmitter the key goes to: 0 Thetis, 1 Yaesu slot 0, 2 slot 1.
    pub(crate) cw_target: u8,
    /// How long the server holds the transmitter after the last key-up, ms.
    pub(crate) cw_hang_ms: u16,
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
    pub(crate) yaesu_comp_attack_ms: f32,
    pub(crate) yaesu_comp_release_ms: f32,
    pub(crate) yaesu2_comp_attack_ms: f32,
    pub(crate) yaesu2_comp_release_ms: f32,
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
    /// The developer mode: opened with
    /// the code on the server screen, kept until it is closed there. What the
    /// mode is lives in `sdr_remote_core::dev_mode`.
    pub(crate) developer_mode: bool,

    /// PTT switch-on spike protection for a built-in speaker+mic in one chassis
    /// (tablets/laptops). When off, all gate-delays are 0 - no added latency. The
    /// per-path mic gate-delay (ms) discards the first mic audio after keyup so the
    /// speaker/chassis spike decays before it reaches TX. Thetis default 0 (the
    /// instant playback-mute is enough), Yaesu default 100 (later TX-path opening).
    pub(crate) spike_protection: bool,
    pub(crate) mic_gate_delay_thetis_ms: u32,
    pub(crate) mic_gate_delay_yaesu_ms: u32,

    /// Relay monitor settings (status only; direct transport remains default).
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

    /// UI language: "en" (base), "nl", "de" or "fr". Any other value falls back
    /// to "en". On a machine with no config file yet it is taken from the OS
    /// display language - see [`first_run`]; from then on it is the operator's
    /// own choice in Settings and is never overwritten.
    pub(crate) language: String,

    /// Count of successful connects to a real server. Used to
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
            spectrum_range_db: sdr_remote_logic::state::SPECTRUM_RANGE_DEFAULT_DB,
            auto_ref_enabled: sdr_remote_logic::state::SPECTRUM_AUTO_REF_DEFAULT,
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
            rx2_spectrum_range_db: sdr_remote_logic::state::SPECTRUM_RANGE_DEFAULT_DB,
            rx2_auto_ref_enabled: sdr_remote_logic::state::SPECTRUM_AUTO_REF_DEFAULT,
            rx2_waterfall_contrast: 1.2,
            spectrum_fill_percent: super::spectrum::SPECTRUM_FILL_DEFAULT,
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
            // True here, false in `first_run`: a config file that predates these
            // keys belongs to somebody whose client has always shown these, and
            // taking them away before the first connect of the new version would
            // be a change rather than a default. The next connect writes the
            // truth either way.
            thetis_configured_last: true,
            rx2_present_last: true,
            chat_answers_seen: String::new(),
            yaesu2_present_last: false,
            yaesu_mem_file: String::new(),
            popout_meter_analog: false,
            meter_analog: [false; 6],
            spectrum_popout: false,
            main_window_pos: None,
            folds_open: FoldsOpen::default(),
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
            multi_tx: false,
            chat_open: false,
            yaesu2_enabled: false,
            yaesu2_ptt_toggle: false,
            yaesu2_volume: 0.05,
            yaesu_mic_gain: 0.2,
            yaesu2_mic_gain: 0.2,
            yaesu_compressor: 0,
            yaesu2_compressor: 0,
            yaesu_comp_attack_ms: sdr_remote_logic::commands::COMP_ATTACK_DEFAULT_MS,
            yaesu_comp_release_ms: sdr_remote_logic::commands::COMP_RELEASE_DEFAULT_MS,
            yaesu2_comp_attack_ms: sdr_remote_logic::commands::COMP_ATTACK_DEFAULT_MS,
            yaesu2_comp_release_ms: sdr_remote_logic::commands::COMP_RELEASE_DEFAULT_MS,
            yaesu_tx_agc: false,
            yaesu2_tx_agc: false,
            yaesu2_popout: false,
            midi_ptt_toggle: true, // MIDI defaults to toggle (existing behavior)
            ble_ptt_enabled: false,
            ble_ptt_address: 0,
            ble_ptt_name: String::new(),
            ble_ptt_target: 0,
            // These buttons are made for short taps (measured: 110-400 ms), not for
            // holding, so a press switches like a MIDI button does.
            ble_ptt_toggle: true,
            cw_paddle_enabled: false,
            cw_sidetone: sdr_remote_logic::sidetone::SidetoneSettings::default(),
            cw_keyer: sdr_remote_logic::cw_keyer::KeyerSettings::default(),
            cw_target: 0,
            cw_hang_ms: 500,
            smeter_source: 1,      // Avg matches pre-multi-source server default
            thetis_autostart: false, // opt-in: powers up the radio without being asked
            roger: sdr_remote_logic::roger::RogerBeep::default(),
            catsync_enabled: false,
            catsync_url: String::new(),
            catsync_url_y1: String::new(),
            catsync_url_y2: String::new(),
            catsync_favorites: Vec::new(),
            allow_zoom_below_2x: false,
            developer_mode: false,
            spike_protection: false,
            mic_gate_delay_thetis_ms: 0,
            mic_gate_delay_yaesu_ms: 100,
            relay_enabled: false,
            relay_url: String::new(),
            relay_station: String::new(),
            relay_token: String::new(),
            relay_instance_id: String::new(),
            relay_device_name: String::new(),
            language: "en".to_string(),
            successful_connects: 0,
        }
    }
}

/// What the client starts as on a machine that has never run it: no config
/// file next to the executable.
///
/// The only thing that differs from [`ClientConfig::default`] is the language,
/// and the difference matters. `Default` also fills in keys an *existing* file
/// happens to lack, and a station that has been reading English for a year
/// must not be retranslated because Windows is Dutch - that would be a change,
/// not a default. A machine with no file has made no such choice yet, so
/// taking the language it reads the rest of its OS in is the best guess there
/// is, and the first screen it ever shows is the connect wizard.
fn first_run() -> ClientConfig {
    ClientConfig {
        language: sdr_remote_core::oslang::detect_ui_language().to_string(),
        // Nothing has ever answered this client, so it claims nothing: no
        // Thetis, no second receiver, and therefore no Radio tab, no Thetis tab
        // and no RX/VRX rows until a server says otherwise.
        thetis_configured_last: false,
        rx2_present_last: false,
        // The extra full-band spectrum row costs bandwidth per receiver chain,
        // and a waterfall works without it - it is then built from its own view.
        // On by default is a thing switched on for you; a fresh install starts
        // without it and the checkbox is right there.
        full_spectrum_enabled: false,
        ..ClientConfig::default()
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

/// Set when the config file exists but could not be read.
///
/// Guards the writes: a read that fails is not a client that has never been set
/// up, and treating it as one would put the bare first-run values on screen and
/// then in the file. The server has the same guard for the same reason - there
/// it is worse, because its saves are load-mutate-save over the whole file.
static LOAD_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// May this process write the config file?
pub(crate) fn may_write_config() -> bool {
    !LOAD_FAILED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Read the config file for a read-modify-write update.
///
/// `None` means: do not write. Every small per-key writer in this file patches
/// one line into the existing text and writes the result back, and they all used
/// `unwrap_or_default()` - which turns a read failure into an EMPTY string, so
/// what got written was a config with only that one line in it. Server address,
/// password, memories, TX profiles, theme: gone. And the writer that runs on
/// every successful connect (`mark_successful_connect`) is one of them, so the
/// common path was the destructive one while the guard sat on the rare one.
///
/// An absent file yields `Some("")`: there is nothing to preserve and the writer
/// should create it.
fn read_for_update(path: &std::path::Path) -> Option<String> {
    if !may_write_config() {
        log::error!("Not updating the config: an earlier read of it failed");
        return None;
    }
    match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(String::new()),
        Err(e) => {
            log::error!(
                "Not updating {} ({e}): it exists but could not be read, and writing now                  would leave only the line being changed",
                path.display()
            );
            None
        }
    }
}

/// Load config from file next to the executable.
pub(crate) fn load_config() -> ClientConfig {
    let mut config = ClientConfig::default();

    let path = match std::env::current_exe() {
        Ok(exe) => exe.with_file_name(config_file_name()),
        // Not knowing where we are is not the same as being new here, but there
        // is no path to look at either, so there is nothing better to do.
        Err(_) => return first_run(),
    };
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => {
            LOAD_FAILED.store(false, std::sync::atomic::Ordering::Relaxed);
            c
        }
        // No config file: nothing has ever been set up here. See `first_run`.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            LOAD_FAILED.store(false, std::sync::atomic::Ordering::Relaxed);
            return first_run();
        }
        // A file that IS there and cannot be read is the opposite case: the
        // settings are still in it. Handing out the bare first-run values here
        // would put them on screen and, at the next save, in the file.
        Err(e) => {
            LOAD_FAILED.store(true, std::sync::atomic::Ordering::Relaxed);
            log::error!(
                "Cannot read {} ({e}). Its settings are still in it, so this run will not \
                 write to it. Close whatever is holding the file and restart.",
                path.display()
            );
            return config;
        }
    };

    let mut tx_profiles: Option<Vec<(u8, String)>> = None;
    let mut has_keys = false;
    // A file from before build 171 has no `spectrum_defaults=`: version 0.
    let mut spectrum_defaults: u32 = 0;
    for line in contents.lines() {
        if let Some(val) = line.strip_prefix("spectrum_defaults=") {
            spectrum_defaults = val.trim().parse().unwrap_or(0);
        } else if let Some(val) = line.strip_prefix("server=") {
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
            if let Some(v) = parse_finite(val) {
                config.tx_gain = sdr_remote_logic::commands::clamp_tx_gain(v);
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
                config.spectrum_ref_db = v.clamp(*crate::ui::spectrum::REF_DB_RANGE.start(), *crate::ui::spectrum::REF_DB_RANGE.end());
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("spectrum_range_db=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.spectrum_range_db = v.clamp(*crate::ui::spectrum::RANGE_DB_RANGE.start(), *crate::ui::spectrum::RANGE_DB_RANGE.end());
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("spectrum_max_bins=") {
            if let Ok(v) = val.trim().parse::<u16>() {
                config.spectrum_max_bins = v.clamp(64, sdr_remote_core::MAX_SPECTRUM_SEND_BINS as u16);
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("rx2_spectrum_fft_size_k=") {
            if let Ok(v) = val.trim().parse::<u16>() {
                config.rx2_spectrum_fft_size_k = v.min(sdr_remote_core::MAX_FFT_SIZE_K);
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("spectrum_fft_size_k=") {
            if let Ok(v) = val.trim().parse::<u16>() {
                config.spectrum_fft_size_k = v.min(sdr_remote_core::MAX_FFT_SIZE_K);
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
                config.waterfall_contrast = v.clamp(*crate::ui::spectrum::WF_CONTRAST_RANGE.start(), *crate::ui::spectrum::WF_CONTRAST_RANGE.end());
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("wf_contrast_per_band=") {
            for entry in val.trim().split(',') {
                let entry = entry.trim();
                if let Some((band, contrast_str)) = entry.split_once(':') {
                    if let Ok(c) = contrast_str.trim().parse::<f32>() {
                        config.wf_contrast_per_band.insert(band.trim().to_string(), c.clamp(*crate::ui::spectrum::WF_CONTRAST_RANGE.start(), *crate::ui::spectrum::WF_CONTRAST_RANGE.end()));
                    }
                }
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("rx2_spectrum_ref_db=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.rx2_spectrum_ref_db = v.clamp(*crate::ui::spectrum::REF_DB_RANGE.start(), *crate::ui::spectrum::REF_DB_RANGE.end());
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("rx2_spectrum_range_db=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.rx2_spectrum_range_db = v.clamp(*crate::ui::spectrum::RANGE_DB_RANGE.start(), *crate::ui::spectrum::RANGE_DB_RANGE.end());
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("rx2_auto_ref_enabled=") {
            config.rx2_auto_ref_enabled = val.trim() == "true";
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("rx2_waterfall_contrast=") {
            if let Ok(v) = val.trim().parse::<f32>() {
                config.rx2_waterfall_contrast = v.clamp(*crate::ui::spectrum::WF_CONTRAST_RANGE.start(), *crate::ui::spectrum::WF_CONTRAST_RANGE.end());
            }
            has_keys = true;
        } else if let Some(val) = line.strip_prefix("spectrum_fill_percent=") {
            if let Ok(v) = val.trim().parse::<u8>() {
                config.spectrum_fill_percent = v.min(100);
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
        } else if let Some(val) = line.strip_prefix("thetis_configured_last=") {
            config.thetis_configured_last = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("rx2_present_last=") {
            config.rx2_present_last = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("chat_answers_seen=") {
            config.chat_answers_seen = val.trim().to_string();
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
            if let Some(p) = parse_eq_profile(val) { config.yaesu_eq_profiles.push(p); }
        } else if let Some(val) = line.strip_prefix("yaesu_eq_profile_advanced=") {
            parse_eq_profile_advanced(val, &mut config.yaesu_eq_profiles);
        } else if let Some(val) = line.strip_prefix("yaesu_eq_profile_chain=") {
            parse_eq_profile_chain(val, &mut config.yaesu_eq_profiles);
        } else if let Some(val) = line.strip_prefix("yaesu2_eq_active=") {
            config.yaesu2_eq_active = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("yaesu2_eq_profile=") {
            if let Some(p) = parse_eq_profile(val) { config.yaesu2_eq_profiles.push(p); }
        } else if let Some(val) = line.strip_prefix("yaesu2_eq_profile_advanced=") {
            parse_eq_profile_advanced(val, &mut config.yaesu2_eq_profiles);
        } else if let Some(val) = line.strip_prefix("yaesu2_eq_profile_chain=") {
            parse_eq_profile_chain(val, &mut config.yaesu2_eq_profiles);
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
        } else if config.folds_open.read_line(line) {
        } else if let Some(val) = line.strip_prefix("cw_paddle_enabled=") {
            config.cw_paddle_enabled = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("cw_sidetone_on=") {
            config.cw_sidetone.on = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("cw_sidetone_hz=") {
            if let Ok(v) = val.trim().parse() { config.cw_sidetone.hz = v; }
        } else if let Some(val) = line.strip_prefix("cw_sidetone_volume=") {
            if let Ok(v) = val.trim().parse() { config.cw_sidetone.volume = v; }
        } else if let Some(val) = line.strip_prefix("cw_wpm=") {
            if let Ok(v) = val.trim().parse::<u8>() { config.cw_keyer.wpm = v.clamp(4, 60); }
        } else if let Some(val) = line.strip_prefix("cw_mode=") {
            config.cw_keyer.mode = if val.trim() == "A" {
                sdr_remote_logic::cw_keyer::PaddleMode::IambicA
            } else {
                sdr_remote_logic::cw_keyer::PaddleMode::IambicB
            };
        } else if let Some(val) = line.strip_prefix("cw_reversed=") {
            config.cw_keyer.reversed = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("cw_straight_key=") {
            config.cw_keyer.straight_enabled = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("cw_target=") {
            config.cw_target = val.trim().parse().unwrap_or(0).min(2);
        } else if let Some(val) = line.strip_prefix("cw_hang_ms=") {
            config.cw_hang_ms = val.trim().parse::<u16>().unwrap_or(500).clamp(200, 3000);
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
        } else if let Some(val) = line.strip_prefix("multi_tx=") {
            config.multi_tx = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("yaesu_ptt_toggle=") {
            config.yaesu_ptt_toggle = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("chat_open=") {
            config.chat_open = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("yaesu2_enabled=") {
            config.yaesu2_enabled = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("yaesu2_ptt_toggle=") {
            config.yaesu2_ptt_toggle = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("yaesu_mic_gain=") {
            if let Some(v) = parse_finite(val) { config.yaesu_mic_gain = sdr_remote_logic::commands::clamp_yaesu_mic_gain(v); }
        } else if let Some(val) = line.strip_prefix("yaesu2_mic_gain=") {
            if let Some(v) = parse_finite(val) { config.yaesu2_mic_gain = sdr_remote_logic::commands::clamp_yaesu_mic_gain(v); }
        } else if let Some(val) = line.strip_prefix("yaesu_compressor=") {
            if let Ok(v) = val.trim().parse::<u8>() { config.yaesu_compressor = v.min(100); }
        } else if let Some(val) = line.strip_prefix("yaesu2_compressor=") {
            if let Ok(v) = val.trim().parse::<u8>() { config.yaesu2_compressor = v.min(100); }
        } else if let Some(val) = line.strip_prefix("yaesu_comp_attack=") {
            if let Some(v) = parse_finite(val) { config.yaesu_comp_attack_ms = v.clamp(*sdr_remote_logic::commands::COMP_ATTACK_RANGE_MS.start(), *sdr_remote_logic::commands::COMP_ATTACK_RANGE_MS.end()); }
        } else if let Some(val) = line.strip_prefix("yaesu_comp_release=") {
            if let Some(v) = parse_finite(val) { config.yaesu_comp_release_ms = v.clamp(*sdr_remote_logic::commands::COMP_RELEASE_RANGE_MS.start(), *sdr_remote_logic::commands::COMP_RELEASE_RANGE_MS.end()); }
        } else if let Some(val) = line.strip_prefix("yaesu2_comp_attack=") {
            if let Some(v) = parse_finite(val) { config.yaesu2_comp_attack_ms = v.clamp(*sdr_remote_logic::commands::COMP_ATTACK_RANGE_MS.start(), *sdr_remote_logic::commands::COMP_ATTACK_RANGE_MS.end()); }
        } else if let Some(val) = line.strip_prefix("yaesu2_comp_release=") {
            if let Some(v) = parse_finite(val) { config.yaesu2_comp_release_ms = v.clamp(*sdr_remote_logic::commands::COMP_RELEASE_RANGE_MS.start(), *sdr_remote_logic::commands::COMP_RELEASE_RANGE_MS.end()); }
        } else if let Some(val) = line.strip_prefix("yaesu_tx_agc=") {
            config.yaesu_tx_agc = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("yaesu2_tx_agc=") {
            config.yaesu2_tx_agc = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("yaesu2_popout=") {
            config.yaesu2_popout = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("midi_ptt_toggle=") {
            config.midi_ptt_toggle = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("ble_ptt_enabled=") {
            config.ble_ptt_enabled = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("ble_ptt_address=") {
            config.ble_ptt_address = crate::ble::parse_addr(val).unwrap_or(0);
        } else if let Some(val) = line.strip_prefix("ble_ptt_name=") {
            config.ble_ptt_name = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("ble_ptt_target=") {
            config.ble_ptt_target = val.trim().parse().unwrap_or(0).min(2);
        } else if let Some(val) = line.strip_prefix("ble_ptt_toggle=") {
            config.ble_ptt_toggle = val.trim() == "true";
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
        } else if let Some(val) = line.strip_prefix("developer_mode=") {
            config.developer_mode = val.trim() == "true";
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
            // Parse as u32 with default 0 on malformed input.
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
    // A named profile also gets its own when it still carries the default
    // profile's id from the copy it was seeded with (build 15).
    let default_conf = match (profile(), std::env::current_exe()) {
        (Some(_), Ok(exe)) => std::fs::read_to_string(exe.with_file_name(CONFIG_FILE)).ok(),
        _ => None,
    };
    if needs_own_relay_id(&config.relay_instance_id, default_conf.as_deref()) {
        if !config.relay_instance_id.trim().is_empty() {
            log::info!("Profile {:?}: relay id was the default profile's; giving it its own", profile());
        }
        config.relay_instance_id = generate_instance_id();
        save_relay_instance_id(&config.relay_instance_id);
    }
    // Auto-default the device name to the hostname on first run (user-editable later).
    if config.relay_device_name.trim().is_empty() {
        config.relay_device_name = generate_device_name();
        save_relay_device_name(&config.relay_device_name);
    }
    // A named profile that still shows the default profile's name gets its own
    // name behind it, so the relay's admin page tells the rows apart (build 16).
    if let Some(p) = profile() {
        if let Some(named) = profile_device_name(&config.relay_device_name, default_conf.as_deref(), p) {
            log::info!("Profile {:?}: device name was the default profile's; now {:?}", profile(), named);
            config.relay_device_name = named;
            save_relay_device_name(&config.relay_device_name);
        }
    }
    if spectrum_defaults < SPECTRUM_DEFAULTS_VERSION {
        let changed = adopt_new_spectrum_defaults(&mut config);
        if changed > 0 {
            log::info!(
                "Spectrum defaults: {} value(s) still at the old default set to the new one (auto ref on, range {} dB)",
                changed,
                sdr_remote_logic::state::SPECTRUM_RANGE_DEFAULT_DB
            );
        }
    }
    config
}

/// The version of the spectrum defaults a saved file was written with. Every
/// save writes it, so a value the operator sets later is never taken for an
/// untouched old default.
const SPECTRUM_DEFAULTS_VERSION: u32 = 2;

/// Once, for a file from before build 171: a spectrum value still exactly at
/// the old default was never changed - every save writes every value, so
/// that is the only way to tell - and gets the new one (2026-10-11:
/// only a range value nobody changed). Returns how many
/// changed.
fn adopt_new_spectrum_defaults(config: &mut ClientConfig) -> usize {
    use sdr_remote_logic::state::{SPECTRUM_AUTO_REF_DEFAULT, SPECTRUM_RANGE_DEFAULT_DB};
    const OLD_RANGE_DB: f32 = 100.0;
    let mut changed = 0;
    for range in [&mut config.spectrum_range_db, &mut config.rx2_spectrum_range_db] {
        if *range == OLD_RANGE_DB {
            *range = SPECTRUM_RANGE_DEFAULT_DB;
            changed += 1;
        }
    }
    for range in [&mut config.vrx1_range_db, &mut config.vrx2_range_db] {
        if *range == Some(OLD_RANGE_DB) {
            *range = Some(SPECTRUM_RANGE_DEFAULT_DB);
            changed += 1;
        }
    }
    // The VRX screens had it off by default; RX1 and RX2 had it on already.
    for auto in [&mut config.vrx1_auto_ref, &mut config.vrx2_auto_ref] {
        if *auto == Some(false) {
            *auto = Some(SPECTRUM_AUTO_REF_DEFAULT);
            changed += 1;
        }
    }
    changed
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
    let existing = match read_for_update(&path) {
        Some(text) => text,
        None => return,
    };
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

/// Whether a config needs a relay id of its own. Empty: first run. The same id
/// as `default_conf`, the default profile's config: a named profile is seeded
/// as a byte copy of that file and so started out with its id, and two clients
/// with one id evict each other at the relay every five seconds - the ghost of
/// the FT-991A on the client side. `default_conf` is
/// `None` for the default profile itself.
fn needs_own_relay_id(id: &str, default_conf: Option<&str>) -> bool {
    let id = id.trim();
    if id.is_empty() {
        return true;
    }
    default_conf
        .and_then(|text| text.lines().find_map(|l| l.strip_prefix("relay_instance_id=")))
        .is_some_and(|default_id| default_id.trim() == id)
}

/// The device name a named profile shows at the relay when it still carries the
/// default profile's: the same name with the profile behind it, so the admin
/// page tells the rows apart. `None` when the name is the
/// profile's own - set by hand, or already given. Only the label: the relay
/// keys the slot on the install id.
fn profile_device_name(name: &str, default_conf: Option<&str>, profile: &str) -> Option<String> {
    let name = name.trim();
    let default_name = default_conf?
        .lines()
        .find_map(|l| l.strip_prefix("relay_device_name="))?
        .trim();
    (!name.is_empty() && name == default_name).then(|| format!("{name} ({profile})"))
}

/// Persist the per-install relay id (read-modify-write on its single key).
fn save_relay_instance_id(id: &str) {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    let path = exe.with_file_name(config_file_name());
    let new_line = format!("relay_instance_id={id}");
    let existing = match read_for_update(&path) {
        Some(text) => text,
        None => return,
    };
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
/// Does a line of the existing config survive the full rewrite?
///
/// `save_config` rebuilds the whole file from what the app holds, and any
/// key it does not hold is only kept because it is named here. Three times
/// now a setting written one key at a time has been forgotten on restart
/// for exactly that reason - the roger beep in August, and the folded-away
/// chat answers this week. Split out of the loop so the list can be tested
/// against the writers instead of read against them.
pub(crate) fn survives_full_save(line: &str) -> bool {
    line.starts_with("ptt_toggle=") || line.starts_with("yaesu_ptt_toggle=") || line.starts_with("midi_ptt_toggle=")
        || line.starts_with("allow_zoom_below_2x=")
        || line.starts_with("spike_protection=")
        || line.starts_with("mic_gate_delay_thetis_ms=")
        || line.starts_with("mic_gate_delay_yaesu_ms=")
        || line.starts_with("smeter_source=")
        || line.starts_with("thetis_autostart=")
        || line.starts_with("developer_mode=")
        || line.starts_with("successful_connects=")
        || line.starts_with("relay_enabled=")
        || line.starts_with("relay_url=")
        || line.starts_with("relay_station=")
        || line.starts_with("relay_token=")
        || line.starts_with("relay_instance_id=")
        || line.starts_with("relay_device_name=")
        || line.starts_with("yaesu2_enabled=")
        || line.starts_with("yaesu2_ptt_toggle=")
        || line.starts_with("yaesu2_popout=")
        || line.starts_with("chat_open=")
        || line.starts_with("multi_tx=")
        || line.starts_with("yaesu_mic_gain=")
        || line.starts_with("yaesu2_mic_gain=")
        || line.starts_with("yaesu_compressor=")
        || line.starts_with("yaesu2_compressor=")
        || line.starts_with("yaesu_comp_")
        || line.starts_with("yaesu2_comp_")
        || line.starts_with("yaesu_tx_agc=")
        || line.starts_with("yaesu2_tx_agc=")
        // The roger beep. Written one key at a time like the rest
        // of this list, and therefore invisible to the rewrite
        // above unless it is named here - which is why the ticks
        // were forgotten on restart (2026-08-14). One prefix rather
        // than seven lines, so a settings added later cannot be
        // half-remembered.
        || line.starts_with("roger_")
        // The Bluetooth PTT button: five keys written by save_ptt_config, one
        // prefix here for the same reason as `roger_`.
        || line.starts_with("ble_ptt_")
        // The Morse paddle, written by save_cw_paddle_enabled; one prefix for
        // the keyer's keys next to it as well.
        || line.starts_with("cw_")
        // Every remembered chevron, written one key at a time by
        // `FoldsOpen::toggle`. Its own list, so a new fold is kept without
        // being named here.
        || Fold::ALL.iter().any(|fold| line.strip_prefix(fold.key()).is_some_and(|rest| rest.starts_with('=')))
        // Which administrator answers were folded away. Same trap
        || line.starts_with("chat_answers_seen=")
        // One prefix for the relay keys, for the same reason as `roger_`
        // above: seven are named individually and an eighth would land
        // outside both this list and the tests. `save_config` writes no
        // `relay_` key itself, so a blanket prefix cannot duplicate a line.
        || line.starts_with("relay_")
        // as the roger keys above: written one key at a time, so
        // invisible to this rewrite unless it is named, and the
        // answers came back on every restart.
}

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
    spectrum_fill_percent: u8,
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
    yaesu_eq_profiles: &[EqProfile],
    yaesu2_eq_active: &str,
    yaesu2_eq_profiles: &[EqProfile],
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
    thetis_configured_last: bool,
    rx2_present_last: bool,
) {
    // A config file that exists but could not be read still holds the
    // operator's settings; writing over it with what this process managed to
    // assemble would throw them away. See `may_write_config`.
    if !may_write_config() {
        log::error!("Not saving the config: it could not be read, so this would overwrite it");
        return;
    }
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
        content.push_str(&format!("spectrum_defaults={}\n", SPECTRUM_DEFAULTS_VERSION));
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
        content.push_str(&format!("spectrum_fill_percent={}\n", spectrum_fill_percent));
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
        content.push_str(&format!("thetis_configured_last={}\n", thetis_configured_last));
        content.push_str(&format!("rx2_present_last={}\n", rx2_present_last));
        content.push_str(&format!("yaesu_eq_active={}\n", yaesu_eq_active));
        for p in yaesu_eq_profiles {
            content.push_str(&eq_profile_lines("yaesu", p));
        }
        content.push_str(&format!("yaesu2_eq_active={}\n", yaesu2_eq_active));
        for p in yaesu2_eq_profiles {
            content.push_str(&eq_profile_lines("yaesu2", p));
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
                if survives_full_save(line) {
                    content.push_str(line);
                    content.push('\n');
                }
            }
        }
        let _ = std::fs::write(path, grouped(&content));
    }
}

/// The settings file as it is written: grouped under its section headings.
/// Both writers use it (`save_ptt_config` too), so neither undoes the other.
pub(crate) fn grouped(content: &str) -> String {
    sdr_remote_core::conf_layout::group(content, SECTIONS, UNSORTED)
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
    ("Connection", &["server", "password", "relay_", "quic_", "successful_connects", "thetis_autostart"]),
    ("Audio", &[
        "volume", "play_volume", "tx_gain", "vfo_a_volume", "vfo_b_volume", "local_volume",
        "rx2_volume", "input_device", "output_device", "agc_enabled", "thetis_wideband_audio",
        "mic_profile", "mic_gate_delay_", "spike_protection", "roger_",
    ]),
    ("PTT", &["ptt_toggle", "midi_ptt_toggle", "yaesu_ptt_toggle", "yaesu2_ptt_toggle", "ble_ptt_", "multi_tx"]),
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
        "ub_show_menu", "group_", "chat_open",
    ]),
    ("Appearance", &["theme", "language", "ui_zoom", "layout_grids"]),
    ("Development", &["developer_mode"]),
];

/// Relay monitor: persist relay settings without touching the large
/// direct-connection config rewrite path.
pub(crate) fn save_relay_config(
    enabled: bool,
    url: &str,
    station: &str,
    token: &str,
) {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    let path = exe.with_file_name(config_file_name());
    let existing = match read_for_update(&path) {
        Some(text) => text,
        None => return,
    };
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
    let existing = match read_for_update(&path) {
        Some(text) => text,
        None => return,
    };
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
    let existing = match read_for_update(&path) {
        Some(text) => text,
        None => return,
    };
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
    let existing = match read_for_update(&path) {
        Some(text) => text,
        None => return,
    };
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
/// Remember which administrator answers have been folded away.
///
/// One key, comma-separated. Without it a restart brought every answer back and
/// the panel that shows them covered the conversation, so a reader with a few
/// answers could no longer read the chat (two users, 2026-08-20).
pub(crate) fn save_chat_answers_seen(ids: &[i64]) {
    let list: Vec<String> = ids.iter().map(|i| i.to_string()).collect();
    save_single_key("chat_answers_seen", &list.join(","));
}

/// What was written down, back at startup. Unparsable entries are skipped
/// rather than fatal: the worst a bad id can do is show an answer again.
pub(crate) fn load_chat_answers_seen() -> Vec<i64> {
    load_config()
        .chat_answers_seen
        .split(',')
        .filter_map(|p| p.trim().parse::<i64>().ok())
        .collect()
}

/// Every chevron whose open/closed state is remembered: the groups on the
/// Server tab (`helpers::settings_group`) and the fold-outs elsewhere
/// (`helpers::fold`). One list for all of them: the key in the conf, reading
/// it back, writing it and keeping it through a full save all come from here,
/// so a fold cannot be half-remembered (builds 198-199; owner: generic where it
/// can be). Until 199 the fold-outs were a field, a loader line, a parameter
/// of `save_config`, a line it wrote and an argument in `persistence.rs` each,
/// and radio 2's equalizer was never written at all. A new fold is a new
/// variant: `key` and `slot` have no catch-all, so the compiler asks for both,
/// and `ALL` is checked against `slot` by `every_fold_has_one_key_and_one_slot`.
///
/// The keys are the ones the conf already had, `collapse_` meaning open, so
/// what was open before 199 is open after it. The menu groups of a Yaesu are
/// not here: their names come from the radio, so they stay in egui's memory
/// for the run (`helpers::fold_by_id`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Fold {
    Relay,
    Display,
    Audio,
    Roger,
    Ble,
    Cw,
    UltrabeamMenu,
    Diversity,
    YaesuEq,
    Yaesu2Eq,
    YaesuMemories,
    Yaesu2Memories,
    YaesuMenu,
    Yaesu2Menu,
    YaesuLevels,
    Yaesu2Levels,
    AmplitecPower,
    BandwidthBreakdown,
    Rf2kDebug,
    Rf2kDriveConfig,
    Developer,
}

impl Fold {
    pub(crate) const ALL: [Fold; 21] = [
        Self::Relay,
        Self::Display,
        Self::Audio,
        Self::Roger,
        Self::Ble,
        Self::Cw,
        Self::UltrabeamMenu,
        Self::Diversity,
        Self::YaesuEq,
        Self::Yaesu2Eq,
        Self::YaesuMemories,
        Self::Yaesu2Memories,
        Self::YaesuMenu,
        Self::Yaesu2Menu,
        Self::YaesuLevels,
        Self::Yaesu2Levels,
        Self::AmplitecPower,
        Self::BandwidthBreakdown,
        Self::Rf2kDebug,
        Self::Rf2kDriveConfig,
        Self::Developer,
    ];

    /// The key in the conf.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Relay => "group_relay_open",
            Self::Display => "group_display_open",
            Self::Audio => "group_audio_open",
            Self::Roger => "group_roger_open",
            Self::Ble => "group_ble_open",
            Self::Cw => "group_cw_open",
            Self::UltrabeamMenu => "ub_show_menu",
            Self::Diversity => "collapse_diversity",
            Self::YaesuEq => "collapse_yaesu_eq",
            Self::Yaesu2Eq => "collapse_yaesu2_eq",
            Self::YaesuMemories => "collapse_yaesu_memories",
            Self::Yaesu2Memories => "collapse_yaesu2_memories",
            Self::YaesuMenu => "collapse_yaesu_menu",
            Self::Yaesu2Menu => "collapse_yaesu2_menu",
            Self::YaesuLevels => "collapse_yaesu_levels",
            Self::Yaesu2Levels => "collapse_yaesu2_levels",
            Self::AmplitecPower => "amplitec_power_show",
            Self::BandwidthBreakdown => "bw_breakdown_expanded",
            Self::Rf2kDebug => "rf2k_show_debug",
            Self::Rf2kDriveConfig => "rf2k_show_drive_config",
            Self::Developer => "group_developer_open",
        }
    }

    fn slot(self) -> usize {
        match self {
            Self::Relay => 0,
            Self::Display => 1,
            Self::Audio => 2,
            Self::Roger => 3,
            Self::Ble => 4,
            Self::Cw => 5,
            Self::UltrabeamMenu => 6,
            Self::Diversity => 7,
            Self::YaesuEq => 8,
            Self::Yaesu2Eq => 9,
            Self::YaesuMemories => 10,
            Self::Yaesu2Memories => 11,
            Self::YaesuMenu => 12,
            Self::Yaesu2Menu => 13,
            Self::YaesuLevels => 14,
            Self::Yaesu2Levels => 15,
            Self::AmplitecPower => 16,
            Self::BandwidthBreakdown => 17,
            Self::Rf2kDebug => 18,
            Self::Rf2kDriveConfig => 19,
            Self::Developer => 20,
        }
    }
}

/// Which folds are open. All closed by default: the tab is long, and the
/// point of a group is that it can be shut.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct FoldsOpen([bool; Fold::ALL.len()]);

impl FoldsOpen {
    pub(crate) fn is_open(self, fold: Fold) -> bool {
        self.0[fold.slot()]
    }

    fn set(&mut self, fold: Fold, open: bool) {
        self.0[fold.slot()] = open;
    }

    /// Open or close a fold, and write it to the conf at once: closing the
    /// window with a fold open keeps it open.
    pub(crate) fn toggle(&mut self, fold: Fold) {
        let open = !self.is_open(fold);
        self.set(fold, open);
        save_single_key(fold.key(), if open { "true" } else { "false" });
    }

    /// One line of the conf; true when it was a fold's.
    fn read_line(&mut self, line: &str) -> bool {
        for fold in Fold::ALL {
            if let Some(val) = line.strip_prefix(fold.key()).and_then(|rest| rest.strip_prefix('=')) {
                self.set(fold, val.trim() == "true");
                return true;
            }
        }
        false
    }
}

/// The Morse paddle's switch (ui/paddle_screen.rs).
pub(crate) fn save_cw_paddle_enabled(on: bool) {
    save_single_key("cw_paddle_enabled", if on { "true" } else { "false" });
}

/// The keyer's settings (ui/paddle_screen.rs). `paddle_enabled` is not one
/// of them: the paddle is switched with `save_cw_paddle_enabled`.
pub(crate) fn save_cw_keyer(s: &sdr_remote_logic::cw_keyer::KeyerSettings) {
    save_single_key("cw_wpm", &s.wpm.to_string());
    save_single_key("cw_mode", match s.mode {
        sdr_remote_logic::cw_keyer::PaddleMode::IambicA => "A",
        sdr_remote_logic::cw_keyer::PaddleMode::IambicB => "B",
    });
    save_single_key("cw_reversed", if s.reversed { "true" } else { "false" });
    save_single_key("cw_straight_key", if s.straight_enabled { "true" } else { "false" });
}

/// Which transmitter the CW key goes to (ui/paddle_screen.rs).
pub(crate) fn save_cw_target(t: u8) {
    save_single_key("cw_target", &t.to_string());
}

/// The hang time after the last key-up (ui/paddle_screen.rs).
pub(crate) fn save_cw_hang_ms(ms: u16) {
    save_single_key("cw_hang_ms", &ms.to_string());
}

/// The CW sidetone (ui/paddle_screen.rs).
pub(crate) fn save_cw_sidetone(s: &sdr_remote_logic::sidetone::SidetoneSettings) {
    save_single_key("cw_sidetone_on", if s.on { "true" } else { "false" });
    save_single_key("cw_sidetone_hz", &format!("{:.0}", s.hz));
    save_single_key("cw_sidetone_volume", &format!("{:.3}", s.volume));
}

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

/// Persist the developer mode as a single `developer_mode=` line (build 295).
pub(crate) fn save_developer_mode(on: bool) {
    save_single_key("developer_mode", if on { "true" } else { "false" });
}

/// Detect first-run for the connection wizard. Returns true when:
///  - the config file is absent, OR
///  - it exists but `successful_connects == 0` (default for fresh installs
///    *and* for legacy configs upgrading to this version).
/// File-read errors are treated as first-run because we cannot prove the
/// user has connected before.
pub(crate) fn is_first_run() -> bool {
    let config = load_config();
    config.successful_connects == 0
}

/// Bump `successful_connects` to (at least) 1 on the first
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
    let existing = match read_for_update(&path) {
        Some(text) => text,
        None => return,
    };
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
mod spectrum_defaults_tests {
    use super::{adopt_new_spectrum_defaults, ClientConfig};

    /// A file from before build 171: what is still exactly at the old default
    /// takes the new one, what the operator set stays (build 171).
    #[test]
    fn only_untouched_spectrum_values_take_the_new_defaults() {
        let mut c = ClientConfig::default();
        c.spectrum_range_db = 100.0; // never touched
        c.rx2_spectrum_range_db = 85.0; // set by the operator
        c.vrx1_range_db = Some(100.0);
        c.vrx2_range_db = None; // never written
        c.vrx1_auto_ref = Some(false);
        c.vrx2_auto_ref = Some(false);
        assert_eq!(adopt_new_spectrum_defaults(&mut c), 4);
        assert_eq!(c.spectrum_range_db, 70.0);
        assert_eq!(c.rx2_spectrum_range_db, 85.0, "the operator's value stays");
        assert_eq!(c.vrx1_range_db, Some(70.0));
        assert_eq!(c.vrx2_range_db, None);
        assert_eq!((c.vrx1_auto_ref, c.vrx2_auto_ref), (Some(true), Some(true)));
        // A range chosen above the old default stays.
        let mut chosen = ClientConfig::default();
        chosen.spectrum_range_db = 120.0;
        assert_eq!(adopt_new_spectrum_defaults(&mut chosen), 0);
        assert_eq!(chosen.spectrum_range_db, 120.0);
        // A fresh install starts on the new ones, and nothing is left to adopt.
        let mut fresh = ClientConfig::default();
        assert_eq!((fresh.auto_ref_enabled, fresh.rx2_auto_ref_enabled), (true, true));
        assert_eq!((fresh.spectrum_range_db, fresh.rx2_spectrum_range_db), (70.0, 70.0));
        assert_eq!(adopt_new_spectrum_defaults(&mut fresh), 0);
        // Every save marks the file, so a later 100 by hand is never taken for
        // an old default (a text guard).
        let code = include_str!("config.rs");
        let code = &code[..code.find("mod spectrum_defaults_tests").unwrap()];
        assert_eq!(code.matches("content.push_str(&format!(\"spectrum_defaults={}\\n\", SPECTRUM_DEFAULTS_VERSION));").count(), 1);
        assert_eq!(code.matches("if spectrum_defaults < SPECTRUM_DEFAULTS_VERSION {").count(), 1);
        // And every load reads the mark - without it every start would take
        // a 100 chosen later for an old default.
        assert_eq!(code.matches("if let Some(val) = line.strip_prefix(\"spectrum_defaults=\") {\n            spectrum_defaults = val.trim().parse().unwrap_or(0);").count(), 1);
    }
}

#[cfg(test)]
mod spectrum_fill_conf_tests {
    /// The Fill value is written by the full save, read back by the load, put
    /// on the screen at start and handed to the save (build 71). There is no
    /// save at exit, so each of these missing sets the value back to 15 after
    /// a restart without a word.
    #[test]
    fn the_fill_value_is_written_read_and_kept() {
        let cut = |s: &'static str| &s[..s.find("#[cfg(test)]").unwrap_or(s.len())];
        let conf = cut(include_str!("config.rs"));
        assert_eq!(conf.matches("content.push_str(&format!(\"spectrum_fill_percent={}\\n\", spectrum_fill_percent));").count(), 1);
        assert_eq!(conf.matches("strip_prefix(\"spectrum_fill_percent=\")").count(), 1);
        assert_eq!(conf.matches("config.spectrum_fill_percent = v.min(100);").count(), 1);
        assert_eq!(cut(include_str!("app_state.rs")).matches("spectrum_fill_percent: config.spectrum_fill_percent,").count(), 1);
        // At its place in `save_config`, between the two keys either side of it:
        // swapped with another u8 it would still compile.
        assert_eq!(cut(include_str!("persistence.rs")).replace("\r\n", "\n").matches(
            "self.rx2_waterfall_contrast,\n            self.spectrum_fill_percent,\n            self.rx1_enabled,").count(), 1);
        assert_eq!(conf.replace("\r\n", "\n").matches(
            "rx2_waterfall_contrast: f32,\n    spectrum_fill_percent: u8,\n    rx1_enabled: bool,").count(), 1, "the parameter of save_config, matching the argument");
    }
}

#[cfg(test)]
mod first_run_tests {
    use super::{first_run, ClientConfig};

    /// A machine that has never run this client claims nothing about the
    /// station it has not spoken to yet: no Thetis, no second receiver. That is
    /// what keeps a bare installation showing a bare screen instead of a Radio
    /// tab, a Thetis tab and four receiver rows that vanish on connect.
    #[test]
    fn a_first_run_claims_no_hardware() {
        let c = first_run();
        assert!(!c.thetis_configured_last);
        assert!(!c.rx2_present_last);
        assert!(!c.full_spectrum_enabled, "full-band row is bandwidth nobody asked for");
        assert!(sdr_remote_core::oslang::UI_LANGUAGES.contains(&c.language.as_str()));
    }

    /// And the other default must not follow it there: it fills in keys an
    /// existing config file lacks, and that file belongs to somebody whose
    /// client has always shown these. Taking them away before their first
    /// connect on the new version would be a change, not a default.
    #[test]
    fn an_existing_file_missing_the_keys_keeps_showing_what_it_showed() {
        let d = ClientConfig::default();
        assert!(d.thetis_configured_last);
        assert!(d.rx2_present_last);
    }
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

#[cfg(test)]
mod fold_tests {
    use super::{survives_full_save, Fold, FoldsOpen};

    /// Every fold has its own key and its own slot, keeps its key through a
    /// full save, and comes back from the conf - itself and no other (builds
    /// 198-199). The loader, the writer and the window state take the whole
    /// set, pinned as text: those are the lines where a fold was lost before.
    #[test]
    fn every_fold_has_one_key_and_one_slot() {
        let mut keys: Vec<&str> = Fold::ALL.iter().map(|f| f.key()).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), Fold::ALL.len(), "two folds share a key");
        for (i, fold) in Fold::ALL.into_iter().enumerate() {
            assert_eq!(fold.slot(), i, "{fold:?} is not in ALL at its own slot");
            assert!(survives_full_save(&format!("{}=true", fold.key())), "{fold:?} is thrown away by a full save");
            let mut open = FoldsOpen::default();
            assert!(open.read_line(&format!("{}=true", fold.key())));
            for other in Fold::ALL {
                assert_eq!(open.is_open(other), other == fold, "{fold:?} read back as {other:?}");
            }
            assert!(open.read_line(&format!("{}=false", fold.key())));
            assert_eq!(open, FoldsOpen::default());
        }
        assert!(!FoldsOpen::default().read_line("group_audio_opened=true"));
        assert!(!survives_full_save("collapse_yaesu_eqs=true"));
        let config = include_str!("config.rs");
        let config = &config[..config.find("#[cfg(test)]").unwrap()];
        assert_eq!(config.matches("} else if config.folds_open.read_line(line) {").count(), 1);
        assert_eq!(config.matches("save_single_key(fold.key(), if open {").count(), 1);
        assert_eq!(include_str!("app_state.rs").matches("folds_open: config.folds_open,").count(), 1);
        // The Ultrabeam asks for its elements on the frame its menu opens, not
        // on every frame it is open: an `if` without
        // `!was_open` floods the server and looks normal on the screen.
        assert_eq!(
            include_str!("devices.rs").matches("if !was_open && self.folds_open.is_open(menu) {").count(),
            1
        );
    }

    /// A conf written before 199 - the fold-outs as the full save wrote them -
    /// opens the same folds after the update (build 199). The other mode is a
    /// fresh conf, which is `FoldsOpen::default()`: everything closed.
    #[test]
    fn a_conf_from_before_199_opens_the_same_folds() {
        let old = "ub_show_menu=false\ncollapse_diversity=true\ncollapse_yaesu_eq=false\n\
                   collapse_yaesu_memories=true\ncollapse_yaesu_menu=false\ncollapse_yaesu2_memories=false\n\
                   collapse_yaesu2_menu=true\namplitec_power_show=true\nbw_breakdown_expanded=false\n";
        let mut open = FoldsOpen::default();
        for line in old.lines() {
            assert!(open.read_line(line.trim()), "{line} is no longer read");
        }
        let opened: Vec<Fold> = Fold::ALL.into_iter().filter(|f| open.is_open(*f)).collect();
        assert_eq!(
            opened,
            [Fold::Diversity, Fold::YaesuMemories, Fold::Yaesu2Menu, Fold::AmplitecPower]
        );
        // The old writers are gone, so nothing writes these keys a second way.
        let persistence = include_str!("persistence.rs");
        assert!(!persistence.contains("self.collapse_"), "a fold is passed to the full save again");
        let config = &include_str!("config.rs")[..include_str!("config.rs").find("#[cfg(test)]").unwrap()];
        assert!(!config.contains("strip_prefix(\"collapse_"), "a fold has its own loader line again");
        assert!(!config.contains("format!(\"collapse_"), "a fold is written by the full save again");
    }
}

#[cfg(test)]
mod full_save_tests {
    use super::survives_full_save;

    /// Every setting written ONE KEY AT A TIME has to survive the full save,
    /// and the list of them is read out of this file rather than typed here.
    ///
    /// A hand-written list falls short: it covers the keys that were forgotten,
    /// not the one that will be. So the test scans its own source for every
    /// `save_single_key("...")` literal and demands each survives. Add a writer
    /// tomorrow and this test covers it without anyone remembering to.
    ///
    /// Three settings have already been lost this way: the roger beep in August
    /// 2026, and the folded-away chat answers this week - the desktop client
    /// being the only one of three front ends that rewrites its whole config.
    #[test]
    fn every_single_key_writer_survives_a_full_save() {
        let source = include_str!("config.rs");
        let mut keys: Vec<&str> = Vec::new();
        for (i, _) in source.match_indices("save_single_key(\"") {
            let rest = &source[i + "save_single_key(\"".len()..];
            if let Some(end) = rest.find('"') {
                let key = &rest[..end];
                // A scan over source text also finds the mentions in comments
                // and in this test's own strings, so only things shaped like a
                // config key count. The first run of this test tripped over the
                // "save_single_key(\"...\")" in the doc comment above.
                let looks_like_a_key = !key.is_empty()
                    && key
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
                if looks_like_a_key {
                    keys.push(key);
                }
            }
        }
        keys.sort_unstable();
        keys.dedup();
        // If this ever finds nothing the test would pass vacuously, which is the
        // one way a self-scanning test can rot.
        // Twenty today (the five group keys left the scan in build 198: they are
        // written as `save_single_key(group.key(), ...)` and pinned by
        // `every_fold_has_one_key_and_one_slot`). Not lower: a floor below the
        // real count lets writers disappear without a word, so it would say "I
        // am still reading" where it should say "I am reading all of them".
        assert!(
            keys.len() >= 20,
            "the scan found only {} keys where there were 20, so either a writer              was removed or the scan stopped reading its own source",
            keys.len()
        );
        for key in keys {
            assert!(
                survives_full_save(&format!("{key}=whatever")),
                "{key} is written one key at a time but is not in the keep-list,                  so the next full save throws it away"
            );
            // Surviving the save is half of it. A key that is written and never
            // read back is just as gone, only later - and the two spellings sit
            // hundreds of lines apart.
            assert!(
                source.contains(&format!("strip_prefix(\"{key}=\"")),
                "{key} is written but this file never reads it back with                  strip_prefix(\"{key}=\")"
            );
        }
    }

    /// The writers that patch their own line instead of going through
    /// `save_single_key`, and that the scan above therefore cannot see.
    ///
    /// Named by hand, and that is the weakness: twelve of them, and a thirteenth
    /// would not be here. The way out is to move them onto `save_single_key` -
    /// they all sit behind the same guarded read since 2026-08-20, so they are
    /// nearly identical already - and then the scan covers them too.
    /// How many functions still patch their own line instead of going through
    /// `save_single_key`. Six today.
    ///
    /// A hand-written list of their keys cannot see a thirteenth, which is the
    /// silent case. This cannot see one either - but it CAN see a seventh
    /// function, and that is where a thirteenth key would arrive. It turns
    /// silence into a prompt without touching a single write path.
    #[test]
    fn no_new_hand_patched_writer_slips_in_unnamed() {
        let source = include_str!("config.rs");
        let calls = source.matches("read_for_update(").count();
        // The helper's own definition and its doc mentions are in that count, so
        // this is a tripwire on the number, not a census.
        assert_eq!(
            calls, 9,
            "the number of read_for_update sites changed. If a writer was added:              move it onto save_single_key so the scanning test covers it, or add              its keys to the list below; and update this count."
        );
    }

    #[test]
    fn the_hand_patched_writers_survive_a_full_save() {
        for key in [
            "relay_enabled",
            "relay_url",
            "relay_station",
            "relay_token",
            "relay_instance_id",
            "relay_device_name",
            "spike_protection",
            "mic_gate_delay_thetis_ms",
            "mic_gate_delay_yaesu_ms",
            "allow_zoom_below_2x",
            "successful_connects",
        ] {
            assert!(
                survives_full_save(&format!("{key}=whatever")),
                "{key} patches its own line but is not in the keep-list"
            );
        }
    }

    /// The keys `save_ptt_config` writes (persistence.rs, outside this file, so
    /// the scan above cannot see them), read out of its format string rather
    /// than typed here. Each has to survive the full save, be read back, and be
    /// filtered out before it is written again (or every save adds a line).
    ///
    /// This was a typed list until build 31, and it missed two of its
    /// twenty-three keys: `chat_open` and `multi_tx` (the "transmit on both"
    /// button) were dropped by every full save, and moving the window is one.
    #[test]
    fn every_key_save_ptt_config_writes_survives_a_full_save() {
        let source = include_str!("config.rs");
        let writer = include_str!("persistence.rs");
        let body = &writer[writer.find("fn save_ptt_config").expect("save_ptt_config")..];
        let body = &body[..body.find("\n    pub(super) fn ").expect("the end of save_ptt_config")];
        // One format string: a second one would hold keys this scan never sees.
        assert_eq!(body.matches("push_str(&format!(").count(), 1, "save_ptt_config writes in more than one place");
        let format = &body[body.find("push_str(&format!(").expect("the written lines")..];
        let format = &format[format.find('"').expect("the format string") + 1..];
        let format = &format[..format.find('"').expect("the end of the format string")];
        let filter = &body[..body.find("push_str(&format!(").unwrap()];
        let keys: Vec<&str> = format.split("\\n").filter_map(|l| l.split_once('=').map(|(k, _)| k)).collect();
        // Twenty-three today. A scan that finds fewer has stopped reading.
        assert!(keys.len() >= 23, "the scan found only {} keys in save_ptt_config: {keys:?}", keys.len());
        for key in keys {
            assert!(survives_full_save(&format!("{key}=whatever")), "{key} is thrown away by a full save");
            assert!(!super::grouped(&format!("{key}=x\n")).contains(super::UNSORTED), "{key} has no section");
            assert!(source.contains(&format!("strip_prefix(\"{key}=\"")), "{key} is never read back");
            let filtered = filter.match_indices("starts_with(\"").any(|(i, _)| {
                let rest = &filter[i + "starts_with(\"".len()..];
                rest.find('"').is_some_and(|end| key.starts_with(&rest[..end]) || format!("{key}=") == rest[..end])
            });
            assert!(filtered, "{key} is not filtered out before save_ptt_config writes it again");
        }
    }

    /// The furthest a save sits from its place today is 14 lines
    /// (`set_ble_enabled`); the median is 2.
    const SAVE_WITHIN_LINES: usize = 20;

    /// A full save keeps these keys as they stand in the file, so wherever the
    /// screen changes one, `save_ptt_config` has to follow. Build 31 missed it
    /// for the chat window's cross, build 32 for a chosen profile's mic gain and
    /// for a layout that opens the FTX-1 window.
    ///
    /// The fields are read out of `save_ptt_config`'s own arguments and every
    /// file in `ui/` is read, so a new key or a new screen is in at once. Each
    /// place is followed by `save_ptt_config()` within `SAVE_WITHIN_LINES` lines
    /// and in the same function: a call earlier on, or one for another key
    /// further down a long screen, does not count (build 33 let the chat button
    /// go back to `save_full_config` because a box below it saved). That does
    /// not prove the call runs on that path (it may sit behind a flag), so the
    /// panel's profile choice has its own pin.
    #[test]
    fn a_ptt_config_key_changed_on_screen_is_written_by_save_ptt_config() {
        let writer = include_str!("persistence.rs");
        let body = &writer[writer.find("fn save_ptt_config").expect("save_ptt_config")..];
        let body = &body[..body.find("\n    pub(super) fn ").expect("the end of save_ptt_config")];
        let args = &body[body.find("push_str(&format!(").expect("the format")..];
        let args = &args[args.find("\",").expect("the end of the format string")..];
        let fields: Vec<&str> = args.split("self.").skip(1)
            .map(|rest| rest.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).next().unwrap())
            .collect();
        assert!(fields.len() >= 23, "{fields:?}");

        // Set from the settings file at start, or the writer itself: not a screen.
        let not_screens = ["config.rs", "persistence.rs", "app_state.rs"];
        // Set every frame, so saving there would write the file every frame:
        // the slot-2 window is closed while radio 2 is not shown, and what the
        // file says comes back only when it is.
        let every_frame = [("yaesu_panel.rs", "render_device_yaesu", "yaesu2_popout")];
        let mut every_frame_seen = 0;
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join("ui");
        let mut files = Vec::new();
        let mut todo = vec![dir];
        while let Some(d) = todo.pop() {
            for e in std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().path()) {
                if e.is_dir() { todo.push(e); } else if e.extension().is_some_and(|x| x == "rs") { files.push(e); }
            }
        }
        let mut found = 0;
        let mut missing = Vec::new();
        for path in files {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if not_screens.contains(&name.as_str()) { continue; }
            let src = std::fs::read_to_string(&path).unwrap();
            let src = &src[..src.find("#[cfg(test)]").unwrap_or(src.len())];
            for field in &fields {
                // `.field = `, `.field=`, `.field ^= ` and the like; not `==`.
                let dotted = format!(".{field}");
                let mut places: Vec<usize> = src.match_indices(&dotted).map(|(at, _)| at).filter(|at| {
                    let after = &src[at + dotted.len()..];
                    if after.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_') { return false; }
                    let after = after.trim_start_matches([' ', '\t']);
                    let after = after.strip_prefix(['^', '|', '&', '+', '-']).unwrap_or(after);
                    after.starts_with('=') && !after.starts_with("==")
                }).collect();
                let borrow = format!("&mut self.{field}");
                places.extend(src.match_indices(&borrow).map(|(at, _)| at)
                    .filter(|at| !src[at + borrow.len()..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')));
                for at in places {
                    found += 1;
                    let (fn_name, rest) = enclosing_fn(src, at);
                    if every_frame.contains(&(name.as_str(), fn_name.as_str(), *field))
                        && src[..src[..at].rfind('\n').unwrap_or(0)].trim_end().ends_with("} else {")
                    {
                        every_frame_seen += 1;
                    } else if !rest.lines().take(SAVE_WITHIN_LINES).any(|l| l.contains("save_ptt_config()")) {
                        missing.push(format!("{name}: {fn_name} sets {field} and does not save it after"));
                    }
                }
            }
        }
        assert!(missing.is_empty(), "set without save_ptt_config after it:\n{}", missing.join("\n"));
        assert_eq!(every_frame_seen, every_frame.len(), "an exception that is no longer there");
        // Forty places on 2026-10-31.
        assert!(found >= 40, "the scan found {found} places");
    }

    /// The function around byte `at`: its name, and its body from `at` to its
    /// closing brace.
    fn enclosing_fn(src: &str, at: usize) -> (String, &str) {
        let head = src[..at].rmatch_indices("fn ").find(|(i, _)| {
            let line_start = src[..*i].rfind('\n').map_or(0, |n| n + 1);
            let lead = src[line_start..*i].trim_start();
            ["", "pub ", "pub(super) ", "pub(crate) "].contains(&lead)
        }).map(|(i, _)| i).expect("a function");
        let name: String = src[head + 3..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        let open = head + src[head..].find('{').unwrap();
        let mut depth = 0;
        for (i, c) in src[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => { depth -= 1; if depth == 0 { return (name, &src[at..open + i]); } }
                _ => {}
            }
        }
        (name, &src[at..])
    }

    /// And the list is a prefix match, so it must not keep something merely
    /// because it starts the same way - that would resurrect a renamed key.
    #[test]
    fn an_unrelated_line_is_not_kept() {
        assert!(!survives_full_save("something_else=1"));
        assert!(!survives_full_save(""));
    }
}

#[cfg(test)]
mod relay_id_tests {
    use super::{needs_own_relay_id, profile_device_name};

    #[test]
    fn a_profile_with_the_default_device_name_gets_its_profile_behind_it() {
        let default_conf = "relay_device_name=SHACK-PC
relay_instance_id=d18c5ee1
";
        assert_eq!(profile_device_name("SHACK-PC", Some(default_conf), "B").as_deref(), Some("SHACK-PC (B)"));
        // Twice round: the second start finds its own name and leaves it.
        assert_eq!(profile_device_name("SHACK-PC (B)", Some(default_conf), "B"), None);
    }

    #[test]
    fn a_device_name_set_by_hand_or_without_a_default_is_kept() {
        let default_conf = "relay_device_name=SHACK-PC
";
        assert_eq!(profile_device_name("Laptop", Some(default_conf), "B"), None, "set by hand");
        assert_eq!(profile_device_name("SHACK-PC", None, "B"), None, "no default config beside it");
        assert_eq!(profile_device_name("SHACK-PC", Some("server=1.2.3.4
"), "B"), None, "a default without a name");
    }

    #[test]
    fn a_profile_with_the_default_profiles_relay_id_gets_its_own() {
        let default_conf = "server=1.2.3.4
relay_instance_id=d18c5ee1
relay_enabled=true
";
        assert!(needs_own_relay_id("d18c5ee1", Some(default_conf)), "a copied id must be replaced");
        assert!(needs_own_relay_id(" d18c5ee1 ", Some(default_conf)), "whitespace does not make it another id");
    }

    #[test]
    fn an_own_relay_id_is_kept() {
        let default_conf = "relay_instance_id=d18c5ee1
";
        assert!(!needs_own_relay_id("d19aa000", Some(default_conf)), "a profile's own id stays");
        assert!(!needs_own_relay_id("d18c5ee1", None), "the default profile keeps its id");
        assert!(!needs_own_relay_id("d19aa000", Some("server=1.2.3.4
")), "a default without an id changes nothing");
    }

    #[test]
    fn an_empty_relay_id_is_always_made() {
        assert!(needs_own_relay_id("", None));
        assert!(needs_own_relay_id("  ", Some("relay_instance_id=
")), "two empty ids are not a shared id to keep");
    }
}

/// A number from the settings file, or nothing for `nan` and `inf`: `nan`
/// parses and passes `clamp`, and a NaN in the TX chain silences it.
fn parse_finite(s: &str) -> Option<f32> {
    s.trim().parse::<f32>().ok().filter(|v| v.is_finite())
}

/// One Yaesu TX EQ profile: the standard EQ (five gains on the fixed bands,
/// as on the phone), the advanced one (twenty channels, desktop only), which
/// of the two sounds, the mic gain, and the TX chain after the EQ.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EqProfile {
    pub(crate) name: String,
    pub(crate) enabled: bool,
    pub(crate) advanced: bool,
    pub(crate) gains: [f32; 5],
    pub(crate) bands: sdr_remote_logic::eq::EqBands,
    /// `None` for a profile saved before the mic gain was part of it: choosing
    /// it leaves the mic gain as it is, as the chain below.
    pub(crate) mic_gain: Option<f32>,
    /// `None` for a profile saved before build 33: choosing it leaves the
    /// compressor and the AGC as they are.
    pub(crate) chain: Option<TxChain>,
}

/// The client-side TX chain of a profile: compressor, its timing, and the AGC.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TxChain {
    pub(crate) comp: u8,
    pub(crate) attack_ms: f32,
    pub(crate) release_ms: f32,
    pub(crate) agc: bool,
}

impl EqProfile {
    /// The channels the engine runs for this profile.
    pub(crate) fn active_bands(&self) -> sdr_remote_logic::eq::EqBands {
        active_eq_bands(self.advanced, &self.gains, &self.bands)
    }
}

/// The channels the engine runs: the advanced twenty, or the standard five.
pub(crate) fn active_eq_bands(advanced: bool, gains: &[f32; 5], bands: &sdr_remote_logic::eq::EqBands)
    -> sdr_remote_logic::eq::EqBands
{
    if advanced { *bands } else { sdr_remote_logic::eq::bands_from_legacy_gains(gains) }
}

/// A name for a new profile, as it will read back from the file: trimmed, not
/// empty, without the `|` that separates the fields of a profile line, and
/// without a line break (pasting puts one in a single-line field).
pub(crate) fn profile_name(input: &str) -> Option<String> {
    let name = input.trim();
    (!name.is_empty() && !name.contains('|') && !name.chars().any(char::is_control)).then(|| name.to_string())
}

/// A profile in the settings file, two or three lines:
///
/// `<radio>_eq_profile=name|enabled|g0,g1,g2,g3,g4|mic_gain` - the standard EQ,
/// in the form every earlier version reads;
/// `<radio>_eq_profile_advanced=name|advanced|kind:freq:q:gain;...` - the
/// twenty channels and whether they sound;
/// `<radio>_eq_profile_chain=name|comp|attack_ms|release_ms|agc` - the TX
/// chain, when the profile has one. An earlier version skips the last two.
fn eq_profile_lines(radio: &str, p: &EqProfile) -> String {
    let g = &p.gains;
    let mic_gain = p.mic_gain.map(|m| format!("|{m:.3}")).unwrap_or_default();
    let mut lines = format!("{radio}_eq_profile={}|{}|{:.1},{:.1},{:.1},{:.1},{:.1}{mic_gain}\n{radio}_eq_profile_advanced={}|{}|{}\n",
        p.name, if p.enabled { "1" } else { "0" }, g[0], g[1], g[2], g[3], g[4],
        p.name, if p.advanced { "1" } else { "0" }, sdr_remote_logic::eq::bands_to_field(&p.bands));
    if let Some(c) = p.chain {
        lines.push_str(&format!("{radio}_eq_profile_chain={}|{}|{:.3}|{:.1}|{}\n",
            p.name, c.comp, c.attack_ms, c.release_ms, if c.agc { "1" } else { "0" }));
    }
    lines
}

/// The first line of a profile. Its third field is the five gains, or (builds
/// 29 and 30) the twenty channels: those that are only the five bands become
/// the standard EQ, the rest the advanced one. The mic gain is optional: a
/// profile without one (or with one that does not read) has none, and choosing
/// it leaves the mic gain as it is.
fn parse_eq_profile(val: &str) -> Option<EqProfile> {
    use sdr_remote_logic::eq;
    let parts: Vec<&str> = val.trim().splitn(4, '|').collect();
    if parts.len() < 3 { return None; }
    let read = eq::bands_from_field(parts[2])?;
    let (advanced, gains, bands) = match eq::legacy_gains(&read) {
        Some(gains) => (false, gains, eq::default_bands()),
        None => {
            let max = eq::STANDARD_GAIN_MAX_DB;
            (true, std::array::from_fn(|i| read[i].gain_db.clamp(-max, max)), read)
        }
    };
    let mic_gain = parts.get(3)
        .and_then(|s| parse_finite(s))
        .map(sdr_remote_logic::commands::clamp_yaesu_mic_gain);
    // Trimmed as `yaesu_eq_active` is, or a name with a space at its end
    // (possible up to build 31) is never found at start.
    Some(EqProfile { name: parts[0].trim().to_string(), enabled: parts[1] == "1", advanced, gains, bands, mic_gain, chain: None })
}

/// The second line of a profile, onto the profile of that name read before it.
fn parse_eq_profile_advanced(val: &str, profiles: &mut [EqProfile]) {
    let parts: Vec<&str> = val.trim().splitn(3, '|').collect();
    let [name, advanced, bands] = parts[..] else { return };
    let (Some(p), Some(bands)) = (profiles.iter_mut().rev().find(|p| p.name == name.trim()),
                                   sdr_remote_logic::eq::bands_from_field(bands)) else { return };
    p.advanced = advanced == "1";
    p.bands = bands;
}

/// The third line of a profile, its TX chain, onto the profile of that name
/// read before it. A line that does not read leaves the profile without one.
fn parse_eq_profile_chain(val: &str, profiles: &mut [EqProfile]) {
    use sdr_remote_logic::commands::{COMP_ATTACK_RANGE_MS, COMP_RELEASE_RANGE_MS};
    let parts: Vec<&str> = val.trim().split('|').collect();
    let [name, comp, attack, release, agc] = parts[..] else { return };
    let (Ok(comp), Some(attack), Some(release)) = (comp.trim().parse::<u32>(), parse_finite(attack), parse_finite(release))
        else { return };
    let Some(p) = profiles.iter_mut().rev().find(|p| p.name == name.trim()) else { return };
    p.chain = Some(TxChain {
        comp: comp.min(100) as u8,
        attack_ms: attack.clamp(*COMP_ATTACK_RANGE_MS.start(), *COMP_ATTACK_RANGE_MS.end()),
        release_ms: release.clamp(*COMP_RELEASE_RANGE_MS.start(), *COMP_RELEASE_RANGE_MS.end()),
        agc: agc.trim() == "1",
    });
}

#[cfg(test)]
mod eq_profile_tests {
    use super::{active_eq_bands, eq_profile_lines, parse_eq_profile, parse_eq_profile_advanced, parse_eq_profile_chain,
                parse_finite, profile_name, EqProfile, TxChain};
    use sdr_remote_logic::eq;

    fn read_back(text: &str) -> Vec<EqProfile> {
        let mut profiles = Vec::new();
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("yaesu_eq_profile=") {
                profiles.extend(parse_eq_profile(v));
            } else if let Some(v) = line.strip_prefix("yaesu_eq_profile_advanced=") {
                parse_eq_profile_advanced(v, &mut profiles);
            } else if let Some(v) = line.strip_prefix("yaesu_eq_profile_chain=") {
                parse_eq_profile_chain(v, &mut profiles);
            }
        }
        profiles
    }

    /// Both EQs of a profile and which one sounds survive the file, in both
    /// positions of the checkbox.
    #[test]
    fn a_profile_keeps_both_eqs_and_its_checkbox() {
        let mut bands = eq::default_bands();
        bands[9] = eq::EqBand { kind: eq::FilterKind::HighShelf, freq_hz: 2500.0, q: 0.9, gain_db: -3.5 };
        for advanced in [false, true] {
            let p = EqProfile { name: "rag chew".into(), enabled: true, advanced,
                gains: [3.0, -2.0, 0.0, 6.5, -12.0], bands, mic_gain: Some(0.25), chain: None };
            assert_eq!(read_back(&eq_profile_lines("yaesu", &p)), vec![p]);
        }
    }

    /// A name with a `|` shifted the fields of its line and the whole profile
    /// was gone at the next start; a name of spaces came back empty.
    #[test]
    fn a_profile_name_is_one_that_comes_back() {
        assert_eq!(profile_name("  rag chew "), Some("rag chew".to_string()));
        assert_eq!(profile_name("low|high"), None);
        assert_eq!(profile_name("   "), None);
        assert_eq!(profile_name("rag\nchew"), None, "a pasted line break");
        let p = EqProfile { name: profile_name(" dx ").unwrap(), enabled: true, advanced: false,
            gains: [0.0; 5], bands: eq::default_bands(), mic_gain: Some(0.2), chain: None };
        assert_eq!(read_back(&eq_profile_lines("yaesu", &p)), vec![p]);
    }

    /// A profile from an earlier version has no mic gain (v2.13.0 wrote none), and
    /// it reads back as none and is written back without one, so choosing it
    /// later still leaves the mic gain alone.
    #[test]
    fn a_profile_without_a_mic_gain_keeps_none() {
        let p = parse_eq_profile("dx|1|1.0,2.0,0.0,-1.0,0.5").unwrap();
        assert_eq!(p.mic_gain, None);
        assert_eq!(read_back(&eq_profile_lines("yaesu", &p)), vec![p.clone()]);
        assert!(eq_profile_lines("yaesu", &p).starts_with("yaesu_eq_profile=dx|1|1.0,2.0,0.0,-1.0,0.5\n"));
    }

    /// The TX chain travels with the profile; a profile saved before it
    /// existed has none, and a third line that does not read gives none.
    #[test]
    fn a_profile_keeps_its_tx_chain() {
        let chain = TxChain { comp: 65, attack_ms: 0.175, release_ms: 3.1, agc: true };
        for chain in [Some(chain), None] {
            let p = EqProfile { name: "dx".into(), enabled: true, advanced: false,
                gains: [0.0; 5], bands: eq::default_bands(), mic_gain: Some(0.2), chain };
            assert_eq!(read_back(&eq_profile_lines("yaesu", &p)), vec![p]);
        }
        let first = "yaesu_eq_profile=dx|1|0.0,0.0,0.0,0.0,0.0|0.200\n";
        let read = read_back(&format!("{first}yaesu_eq_profile_chain=dx|250|nan|3.1|1\n"));
        assert_eq!(read[0].chain, None, "a chain that does not read");
        let read = read_back(&format!("{first}yaesu_eq_profile_chain=dx|300|0.01|5000|0\n"));
        assert_eq!(read[0].chain, Some(TxChain { comp: 100, attack_ms: 0.1, release_ms: 1000.0, agc: false }));
    }

    /// A name with a space at its edge (possible up to build 31) reads back
    /// trimmed, as `yaesu_eq_active` does, with its other lines attached.
    #[test]
    fn an_old_name_with_a_space_at_its_edge_is_found() {
        let text = "yaesu_eq_profile=dx |1|1.0,0.0,0.0,0.0,0.0|0.200\n\
                    yaesu_eq_profile_advanced=dx |1|peak:100:1.4:1\n\
                    yaesu_eq_profile_chain=dx |10|0.5|20.0|0\n";
        let read = read_back(text);
        assert_eq!((read[0].name.as_str(), read[0].advanced, read[0].chain.map(|c| c.comp)), ("dx", true, Some(10)));
    }

    /// The engine runs the advanced twenty with the box on, and the five
    /// standard gains with it off - never the other set.
    #[test]
    fn the_checkbox_picks_the_eq_that_sounds() {
        let gains = [4.0, -3.0, 0.0, 2.0, -6.0];
        let mut bands = eq::default_bands();
        bands[7] = eq::EqBand { kind: eq::FilterKind::HighPass, freq_hz: 150.0, q: 0.71, gain_db: 0.0 };
        assert_eq!(active_eq_bands(true, &gains, &bands), bands);
        assert_eq!(active_eq_bands(false, &gains, &bands), eq::bands_from_legacy_gains(&gains));
    }

    /// The second line finds its profile by name and by order. Through the
    /// real grouping of a saved file, every profile still gets its own.
    #[test]
    fn the_saved_file_keeps_each_second_line_with_its_profile() {
        let mut low = eq::default_bands();
        low[5] = eq::EqBand { kind: eq::FilterKind::HighPass, freq_hz: 120.0, q: 0.71, gain_db: 0.0 };
        let mut high = eq::default_bands();
        high[19] = eq::EqBand { kind: eq::FilterKind::LowPass, freq_hz: 3000.0, q: 0.71, gain_db: 0.0 };
        let a = EqProfile { name: "a".into(), enabled: true, advanced: true,
            gains: [1.0, 0.0, 0.0, 0.0, 0.0], bands: low, mic_gain: Some(0.2),
            chain: Some(TxChain { comp: 40, attack_ms: 0.5, release_ms: 120.0, agc: true }) };
        let b = EqProfile { name: "b".into(), enabled: true, advanced: false,
            gains: [0.0, 0.0, 2.0, 0.0, 0.0], bands: high, mic_gain: Some(0.3), chain: None };
        let text = format!("yaesu_eq_active=a\n{}{}rx_volume=0.5\n", eq_profile_lines("yaesu", &a), eq_profile_lines("yaesu", &b));
        let grouped = sdr_remote_core::conf_layout::group(&text, super::SECTIONS, super::UNSORTED);
        assert_eq!(read_back(&grouped), vec![a, b]);
    }

    /// The edges of the first line: a build-30 gain beyond the standard
    /// sliders is clamped onto them, and a line with too few fields is no profile.
    #[test]
    fn a_first_line_out_of_range_or_short_is_handled() {
        let mut more = eq::bands_from_legacy_gains(&[14.0, 0.0, 0.0, 0.0, 0.0]);
        more[5].kind = eq::FilterKind::HighPass;
        let p = parse_eq_profile(&format!("p|1|{}|0.200", eq::bands_to_field(&more))).unwrap();
        assert_eq!((p.advanced, p.gains[0]), (true, 12.0));
        assert!(parse_eq_profile("p|1").is_none());
    }

    /// The first line is the form every earlier version reads: the five gains.
    #[test]
    fn the_first_line_is_the_old_form() {
        let p = EqProfile { name: "p".into(), enabled: false, advanced: true,
            gains: [1.0, 2.0, 3.0, 4.0, 5.0], bands: eq::default_bands(), mic_gain: Some(0.2), chain: None };
        let text = eq_profile_lines("yaesu2", &p);
        assert_eq!(text.lines().next(), Some("yaesu2_eq_profile=p|0|1.0,2.0,3.0,4.0,5.0|0.200"));
    }

    /// A profile from build 29 or 30 holds twenty channels on its first line.
    /// The five bands alone become the standard EQ; anything more the advanced
    /// one, with the checkbox on.
    #[test]
    fn a_profile_from_build_30_lands_on_the_right_side() {
        let five = eq::bands_from_legacy_gains(&[2.0, 0.0, -1.0, 0.0, 3.0]);
        let p = parse_eq_profile(&format!("p|1|{}|0.300", eq::bands_to_field(&five))).unwrap();
        assert_eq!((p.advanced, p.gains), (false, [2.0, 0.0, -1.0, 0.0, 3.0]));
        let mut more = five;
        more[6] = eq::EqBand { kind: eq::FilterKind::HighPass, freq_hz: 120.0, q: 0.71, gain_db: 0.0 };
        let p = parse_eq_profile(&format!("p|1|{}|0.300", eq::bands_to_field(&more))).unwrap();
        assert_eq!((p.advanced, p.bands, p.gains), (true, more, [2.0, 0.0, -1.0, 0.0, 3.0]));
    }

    #[test]
    fn a_nan_in_a_profile_is_not_read() {
        assert_eq!(parse_finite(" 5.5 "), Some(5.5));
        assert_eq!(parse_finite("nan"), None);
        assert_eq!(parse_finite("inf"), None);
        let p = parse_eq_profile("p|1|0,0,0,0,0|nan").unwrap();
        assert_eq!(p.mic_gain, None, "no mic gain, not NaN: choosing it keeps the current one");
        assert!(parse_eq_profile("p|1|peak:1000:nan:3").is_none());
        // The mic gain keeps the new range (the old one stopped at 0.4), and off stays off.
        assert_eq!(parse_eq_profile("p|1|0,0,0,0,0|5.0").unwrap().mic_gain, Some(5.0));
        assert_eq!(parse_eq_profile("p|1|0,0,0,0,0|0.000").unwrap().mic_gain, Some(0.0));
        // Above the range it is clamped to the top (+30 dB), not taken as it is.
        let top = sdr_remote_logic::commands::clamp_yaesu_mic_gain(f32::MAX);
        assert_eq!(parse_eq_profile("p|1|0,0,0,0,0|40.0").unwrap().mic_gain, Some(top));
        assert!(top < 40.0);
    }

    /// The transmit gains and the compressor times are read through
    /// `parse_finite`: a `nan` in a gain silences the transmitter (`clamp`
    /// lets it through), and in a time it rides along into the next save.
    #[test]
    fn the_gains_and_the_times_are_read_through_parse_finite() {
        let source = include_str!("config.rs");
        for key in ["tx_gain", "yaesu_mic_gain", "yaesu2_mic_gain",
                    "yaesu_comp_attack", "yaesu_comp_release", "yaesu2_comp_attack", "yaesu2_comp_release"] {
            let at = source.find(&format!("strip_prefix(\"{key}=\")")).unwrap_or_else(|| panic!("{key} is not read"));
            let branch = &source[at..at + source[at..].find("} else if").expect("the next branch")];
            assert!(branch.contains("parse_finite(") && !branch.contains("parse::<f32>"), "{key}: {branch}");
        }
    }
}
