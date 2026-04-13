use std::fs;
use std::path::PathBuf;

#[derive(Clone)]
pub struct ServerConfig {
    pub cat_addr: String,
    /// TCI WebSocket address (e.g. "127.0.0.1:40001")
    pub tci_addr: Option<String>,
    pub spectrum_enabled: bool,
    /// Path to Thetis.exe for auto-launch (None = disabled)
    pub thetis_path: Option<String>,
    /// Yaesu FT-991A serial port (e.g. "COM8")
    pub yaesu_port: Option<String>,
    pub yaesu_enabled: bool,
    pub yaesu_baud: u32,
    /// Yaesu USB audio input device name pattern (e.g. "USB Audio")
    pub yaesu_audio_device: Option<String>,
    /// Amplitec 6/2 serial port (e.g. "COM3")
    pub amplitec_port: Option<String>,
    pub amplitec_enabled: bool,
    /// Labels for Amplitec antenna positions 1-6 (shared by A and B)
    pub amplitec_labels: [String; 6],
    /// Show Amplitec control window on start (default true)
    pub show_amplitec_window: bool,
    /// JC-4s tuner serial port (e.g. "COM5")
    pub tuner_port: Option<String>,
    pub tuner_enabled: bool,
    /// Show tuner control window on start (default true)
    pub show_tuner_window: bool,
    /// SPE Expert 1.3K-FA serial port (e.g. "COM6")
    pub spe_port: Option<String>,
    pub spe_enabled: bool,
    /// Show SPE Expert control window on start (default true)
    pub show_spe_window: bool,
    /// RF2K-S Raspberry Pi address (e.g. "192.168.1.50:8080")
    pub rf2k_addr: Option<String>,
    pub rf2k_enabled: bool,
    /// Show RF2K-S control window on start (default true)
    pub show_rf2k_window: bool,
    /// UltraBeam RCU-06 serial port (e.g. "COM7")
    pub ultrabeam_port: Option<String>,
    pub ultrabeam_enabled: bool,
    /// Show UltraBeam control window on start (default true)
    pub show_ultrabeam_window: bool,
    /// EA7HG Visual Rotor TCP address (e.g. "192.168.1.60:3010")
    pub rotor_addr: Option<String>,
    pub rotor_enabled: bool,
    /// Show Rotor control window on start (default true)
    pub show_rotor_window: bool,
    /// Saved window positions: [x, y]
    pub tuner_window_pos: Option<[f32; 2]>,
    pub amplitec_window_pos: Option<[f32; 2]>,
    pub spe_window_pos: Option<[f32; 2]>,
    pub rf2k_window_pos: Option<[f32; 2]>,
    pub ultrabeam_window_pos: Option<[f32; 2]>,
    pub rotor_window_pos: Option<[f32; 2]>,
    /// Saved main window position: [x, y]
    pub main_window_pos: Option<[f32; 2]>,
    /// Saved window sizes: [w, h]
    pub main_window_size: Option<[f32; 2]>,
    pub tuner_window_size: Option<[f32; 2]>,
    pub amplitec_window_size: Option<[f32; 2]>,
    pub spe_window_size: Option<[f32; 2]>,
    pub rf2k_window_size: Option<[f32; 2]>,
    pub ultrabeam_window_size: Option<[f32; 2]>,
    pub rotor_window_size: Option<[f32; 2]>,
    /// Auto-start server on launch (skip settings screen)
    pub autostart: bool,
    /// Active PA: 0=none, 1=SPE, 2=RF2K
    pub active_pa: u8,
    /// DX Cluster telnet server address (e.g. "dxc.pi4cc.nl:8000")
    pub dxcluster_server: String,
    /// DX Cluster callsign for login
    pub dxcluster_callsign: String,
    /// DX Cluster enabled
    pub dxcluster_enabled: bool,
    /// DX Cluster spot expiry time in minutes (default 10)
    pub dxcluster_expiry_min: u16,
    /// Network authentication password (None = no auth, any client can connect)
    pub password: Option<String>,
    /// TOTP 2FA secret (base32, None = 2FA disabled)
    pub totp_secret: Option<String>,
    /// TOTP 2FA enabled
    pub totp_enabled: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            cat_addr: "127.0.0.1:13013".to_string(),
            tci_addr: None,
            spectrum_enabled: true,
            thetis_path: detect_thetis_path(),
            yaesu_port: None,
            yaesu_enabled: false,
            yaesu_baud: 38400,
            yaesu_audio_device: None,
            amplitec_port: None,
            amplitec_enabled: true,
            amplitec_labels: default_labels("Ant"),
            show_amplitec_window: true,
            tuner_port: None,
            tuner_enabled: true,
            show_tuner_window: true,
            spe_port: None,
            spe_enabled: true,
            show_spe_window: true,
            rf2k_addr: None,
            rf2k_enabled: true,
            show_rf2k_window: true,
            ultrabeam_port: None,
            ultrabeam_enabled: true,
            show_ultrabeam_window: true,
            rotor_addr: None,
            rotor_enabled: true,
            show_rotor_window: true,
            tuner_window_pos: None,
            amplitec_window_pos: None,
            spe_window_pos: None,
            rf2k_window_pos: None,
            ultrabeam_window_pos: None,
            rotor_window_pos: None,
            main_window_pos: None,
            main_window_size: None,
            tuner_window_size: None,
            amplitec_window_size: None,
            spe_window_size: None,
            rf2k_window_size: None,
            ultrabeam_window_size: None,
            rotor_window_size: None,
            autostart: false,
            active_pa: 0,
            dxcluster_server: "dxc.pi4cc.nl:8000".to_string(),
            dxcluster_callsign: "PA3GHM".to_string(),
            dxcluster_enabled: true,
            dxcluster_expiry_min: 10,
            password: None,
            totp_secret: None,
            totp_enabled: false,
        }
    }
}

fn default_labels(prefix: &str) -> [String; 6] {
    [
        format!("{prefix}1"), format!("{prefix}2"), format!("{prefix}3"),
        format!("{prefix}4"), format!("{prefix}5"), format!("{prefix}6"),
    ]
}

fn detect_thetis_path() -> Option<String> {
    let default = r"C:\Program Files\OpenHPSDR\Thetis\Thetis.exe";
    if std::path::Path::new(default).exists() {
        Some(default.to_string())
    } else {
        None
    }
}

fn config_path() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.parent()
        .unwrap_or(std::path::Path::new("."))
        .join("thetislink-server.conf")
}

pub fn load() -> ServerConfig {
    let path = config_path();
    let mut config = ServerConfig::default();

    if let Ok(contents) = fs::read_to_string(&path) {
        for line in contents.lines() {
            let line = line.trim();
            if let Some((key, value)) = line.split_once('=') {
                match key.trim() {
                    "cat" => config.cat_addr = value.trim().to_string(),
                    "tci" => {
                        let v = value.trim().to_string();
                        config.tci_addr = if v.is_empty() { None } else { Some(v) };
                    }
                    // Legacy keys (ignored, kept for backward compat with old config files)
                    "input" | "input2" | "output" | "anan_interface" => {}
                    "thetis_path" => {
                        let v = value.trim().to_string();
                        if v.is_empty() {
                            config.thetis_path = None;
                        } else {
                            config.thetis_path = Some(v);
                        }
                    }
                    "yaesu_port" => {
                        let v = value.trim().to_string();
                        config.yaesu_port = if v.is_empty() { None } else { Some(v) };
                    }
                    "yaesu_enabled" => {
                        config.yaesu_enabled = value.trim() != "false";
                    }
                    "yaesu_baud" => {
                        if let Ok(v) = value.trim().parse::<u32>() {
                            config.yaesu_baud = v;
                        }
                    }
                    "yaesu_audio" => {
                        let v = value.trim().to_string();
                        config.yaesu_audio_device = if v.is_empty() { None } else { Some(v) };
                    }
                    "amplitec_port" => {
                        let v = value.trim().to_string();
                        config.amplitec_port = if v.is_empty() { None } else { Some(v) };
                    }
                    "amplitec_enabled" => {
                        config.amplitec_enabled = value.trim() != "false";
                    }
                    k if k.starts_with("amplitec_label") => {
                        if let Some(idx) = k.strip_prefix("amplitec_label").and_then(|s| s.parse::<usize>().ok()) {
                            if idx >= 1 && idx <= 6 {
                                config.amplitec_labels[idx - 1] = value.trim().to_string();
                            }
                        }
                    }
                    // Backward compat: read old amplitec_aN keys as shared labels
                    k if k.starts_with("amplitec_a") => {
                        if let Some(idx) = k.strip_prefix("amplitec_a").and_then(|s| s.parse::<usize>().ok()) {
                            if idx >= 1 && idx <= 6 {
                                config.amplitec_labels[idx - 1] = value.trim().to_string();
                            }
                        }
                    }
                    k if k.starts_with("amplitec_b") => {
                        // Old amplitec_bN keys: ignore (same antennas)
                    }
                    "amplitec_window" => {
                        config.show_amplitec_window = value.trim() == "true";
                    }
                    "tuner_port" => {
                        let v = value.trim().to_string();
                        config.tuner_port = if v.is_empty() { None } else { Some(v) };
                    }
                    "tuner_enabled" => {
                        config.tuner_enabled = value.trim() != "false";
                    }
                    "tuner_window" => {
                        config.show_tuner_window = value.trim() == "true";
                    }
                    "tuner_safe_drive" => {
                        // Legacy key, ignored
                    }
                    "tuner_pos_x" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.tuner_window_pos.get_or_insert([0.0, 0.0])[0] = v;
                        }
                    }
                    "tuner_pos_y" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.tuner_window_pos.get_or_insert([0.0, 0.0])[1] = v;
                        }
                    }
                    "amplitec_pos_x" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.amplitec_window_pos.get_or_insert([0.0, 0.0])[0] = v;
                        }
                    }
                    "amplitec_pos_y" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.amplitec_window_pos.get_or_insert([0.0, 0.0])[1] = v;
                        }
                    }
                    "spe_port" => {
                        let v = value.trim().to_string();
                        config.spe_port = if v.is_empty() { None } else { Some(v) };
                    }
                    "spe_enabled" => {
                        config.spe_enabled = value.trim() != "false";
                    }
                    "spe_window" => {
                        config.show_spe_window = value.trim() == "true";
                    }
                    "spe_pos_x" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.spe_window_pos.get_or_insert([0.0, 0.0])[0] = v;
                        }
                    }
                    "spe_pos_y" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.spe_window_pos.get_or_insert([0.0, 0.0])[1] = v;
                        }
                    }
                    "rf2k_addr" => {
                        let v = value.trim().to_string();
                        config.rf2k_addr = if v.is_empty() { None } else { Some(v) };
                    }
                    "rf2k_enabled" => {
                        config.rf2k_enabled = value.trim() != "false";
                    }
                    "rf2k_window" => {
                        config.show_rf2k_window = value.trim() == "true";
                    }
                    "rf2k_pos_x" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.rf2k_window_pos.get_or_insert([0.0, 0.0])[0] = v;
                        }
                    }
                    "rf2k_pos_y" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.rf2k_window_pos.get_or_insert([0.0, 0.0])[1] = v;
                        }
                    }
                    "ultrabeam_port" => {
                        let v = value.trim().to_string();
                        config.ultrabeam_port = if v.is_empty() { None } else { Some(v) };
                    }
                    "ultrabeam_enabled" => {
                        config.ultrabeam_enabled = value.trim() != "false";
                    }
                    "ultrabeam_window" => {
                        config.show_ultrabeam_window = value.trim() == "true";
                    }
                    "ultrabeam_pos_x" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.ultrabeam_window_pos.get_or_insert([0.0, 0.0])[0] = v;
                        }
                    }
                    "ultrabeam_pos_y" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.ultrabeam_window_pos.get_or_insert([0.0, 0.0])[1] = v;
                        }
                    }
                    "rotor_addr" => {
                        let v = value.trim().to_string();
                        config.rotor_addr = if v.is_empty() { None } else { Some(v) };
                    }
                    "rotor_enabled" => {
                        config.rotor_enabled = value.trim() != "false";
                    }
                    "rotor_window" => {
                        config.show_rotor_window = value.trim() == "true";
                    }
                    "rotor_pos_x" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.rotor_window_pos.get_or_insert([0.0, 0.0])[0] = v;
                        }
                    }
                    "rotor_pos_y" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.rotor_window_pos.get_or_insert([0.0, 0.0])[1] = v;
                        }
                    }
                    "main_pos_x" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.main_window_pos.get_or_insert([0.0, 0.0])[0] = v;
                        }
                    }
                    "main_pos_y" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.main_window_pos.get_or_insert([0.0, 0.0])[1] = v;
                        }
                    }
                    "main_size_w" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.main_window_size.get_or_insert([0.0, 0.0])[0] = v;
                        }
                    }
                    "main_size_h" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.main_window_size.get_or_insert([0.0, 0.0])[1] = v;
                        }
                    }
                    "tuner_size_w" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.tuner_window_size.get_or_insert([0.0, 0.0])[0] = v;
                        }
                    }
                    "tuner_size_h" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.tuner_window_size.get_or_insert([0.0, 0.0])[1] = v;
                        }
                    }
                    "amplitec_size_w" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.amplitec_window_size.get_or_insert([0.0, 0.0])[0] = v;
                        }
                    }
                    "amplitec_size_h" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.amplitec_window_size.get_or_insert([0.0, 0.0])[1] = v;
                        }
                    }
                    "spe_size_w" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.spe_window_size.get_or_insert([0.0, 0.0])[0] = v;
                        }
                    }
                    "spe_size_h" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.spe_window_size.get_or_insert([0.0, 0.0])[1] = v;
                        }
                    }
                    "rf2k_size_w" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.rf2k_window_size.get_or_insert([0.0, 0.0])[0] = v;
                        }
                    }
                    "rf2k_size_h" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.rf2k_window_size.get_or_insert([0.0, 0.0])[1] = v;
                        }
                    }
                    "ultrabeam_size_w" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.ultrabeam_window_size.get_or_insert([0.0, 0.0])[0] = v;
                        }
                    }
                    "ultrabeam_size_h" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.ultrabeam_window_size.get_or_insert([0.0, 0.0])[1] = v;
                        }
                    }
                    "rotor_size_w" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.rotor_window_size.get_or_insert([0.0, 0.0])[0] = v;
                        }
                    }
                    "rotor_size_h" => {
                        if let Ok(v) = value.trim().parse::<f32>() {
                            config.rotor_window_size.get_or_insert([0.0, 0.0])[1] = v;
                        }
                    }
                    "autostart" => {
                        config.autostart = value.trim() == "true";
                    }
                    "active_pa" => {
                        config.active_pa = value.trim().parse().unwrap_or(0);
                    }
                    "dxcluster_server" => {
                        let v = value.trim().to_string();
                        if !v.is_empty() { config.dxcluster_server = v; }
                    }
                    "dxcluster_callsign" => {
                        let v = value.trim().to_string();
                        if !v.is_empty() { config.dxcluster_callsign = v; }
                    }
                    "dxcluster_enabled" => {
                        config.dxcluster_enabled = value.trim() != "false";
                    }
                    "dxcluster_expiry_min" => {
                        if let Ok(v) = value.trim().parse::<u16>() {
                            config.dxcluster_expiry_min = v.max(1);
                        }
                    }
                    "password" => {
                        let v = value.trim();
                        if !v.is_empty() {
                            // Try deobfuscate first; if it fails, treat as plaintext (first time)
                            config.password = Some(
                                sdr_remote_core::auth::deobfuscate_password(v)
                                    .unwrap_or_else(|| v.to_string())
                            );
                        }
                    }
                    "totp_secret" => {
                        let v = value.trim();
                        if !v.is_empty() {
                            config.totp_secret = Some(
                                sdr_remote_core::auth::deobfuscate_password(v)
                                    .unwrap_or_else(|| v.to_string())
                            );
                        }
                    }
                    "totp_enabled" => {
                        config.totp_enabled = value.trim() == "true";
                    }
                    _ => {}
                }
            }
        }
    }

    config
}

pub fn save(config: &ServerConfig) {
    let path = config_path();
    let mut contents = format!(
        "cat={}\ntci={}\nthetis_path={}\nyaesu_port={}\nyaesu_enabled={}\nyaesu_baud={}\nyaesu_audio={}\namplitec_port={}\namplitec_enabled={}\namplitec_window={}\ntuner_port={}\ntuner_enabled={}\ntuner_window={}\nspe_port={}\nspe_enabled={}\nspe_window={}\nrf2k_addr={}\nrf2k_enabled={}\nrf2k_window={}\nultrabeam_port={}\nultrabeam_enabled={}\nultrabeam_window={}\nrotor_addr={}\nrotor_enabled={}\nrotor_window={}\n",
        config.cat_addr,
        config.tci_addr.as_deref().unwrap_or(""),
        config.thetis_path.as_deref().unwrap_or(""),
        config.yaesu_port.as_deref().unwrap_or(""),
        config.yaesu_enabled,
        config.yaesu_baud,
        config.yaesu_audio_device.as_deref().unwrap_or(""),
        config.amplitec_port.as_deref().unwrap_or(""),
        config.amplitec_enabled,
        config.show_amplitec_window,
        config.tuner_port.as_deref().unwrap_or(""),
        config.tuner_enabled,
        config.show_tuner_window,
        config.spe_port.as_deref().unwrap_or(""),
        config.spe_enabled,
        config.show_spe_window,
        config.rf2k_addr.as_deref().unwrap_or(""),
        config.rf2k_enabled,
        config.show_rf2k_window,
        config.ultrabeam_port.as_deref().unwrap_or(""),
        config.ultrabeam_enabled,
        config.show_ultrabeam_window,
        config.rotor_addr.as_deref().unwrap_or(""),
        config.rotor_enabled,
        config.show_rotor_window,
    );
    for i in 0..6 {
        contents.push_str(&format!("amplitec_label{}={}\n", i + 1, config.amplitec_labels[i]));
    }
    if let Some(pos) = config.tuner_window_pos {
        contents.push_str(&format!("tuner_pos_x={}\ntuner_pos_y={}\n", pos[0], pos[1]));
    }
    if let Some(pos) = config.amplitec_window_pos {
        contents.push_str(&format!("amplitec_pos_x={}\namplitec_pos_y={}\n", pos[0], pos[1]));
    }
    if let Some(pos) = config.spe_window_pos {
        contents.push_str(&format!("spe_pos_x={}\nspe_pos_y={}\n", pos[0], pos[1]));
    }
    if let Some(pos) = config.rf2k_window_pos {
        contents.push_str(&format!("rf2k_pos_x={}\nrf2k_pos_y={}\n", pos[0], pos[1]));
    }
    if let Some(pos) = config.ultrabeam_window_pos {
        contents.push_str(&format!("ultrabeam_pos_x={}\nultrabeam_pos_y={}\n", pos[0], pos[1]));
    }
    if let Some(pos) = config.rotor_window_pos {
        contents.push_str(&format!("rotor_pos_x={}\nrotor_pos_y={}\n", pos[0], pos[1]));
    }
    // Main window position
    if let Some(pos) = config.main_window_pos {
        contents.push_str(&format!("main_pos_x={}\nmain_pos_y={}\n", pos[0], pos[1]));
    }
    // Window sizes
    if let Some(sz) = config.main_window_size {
        contents.push_str(&format!("main_size_w={}\nmain_size_h={}\n", sz[0], sz[1]));
    }
    if let Some(sz) = config.tuner_window_size {
        contents.push_str(&format!("tuner_size_w={}\ntuner_size_h={}\n", sz[0], sz[1]));
    }
    if let Some(sz) = config.amplitec_window_size {
        contents.push_str(&format!("amplitec_size_w={}\namplitec_size_h={}\n", sz[0], sz[1]));
    }
    if let Some(sz) = config.spe_window_size {
        contents.push_str(&format!("spe_size_w={}\nspe_size_h={}\n", sz[0], sz[1]));
    }
    if let Some(sz) = config.rf2k_window_size {
        contents.push_str(&format!("rf2k_size_w={}\nrf2k_size_h={}\n", sz[0], sz[1]));
    }
    if let Some(sz) = config.ultrabeam_window_size {
        contents.push_str(&format!("ultrabeam_size_w={}\nultrabeam_size_h={}\n", sz[0], sz[1]));
    }
    if let Some(sz) = config.rotor_window_size {
        contents.push_str(&format!("rotor_size_w={}\nrotor_size_h={}\n", sz[0], sz[1]));
    }
    contents.push_str(&format!("autostart={}\n", config.autostart));
    contents.push_str(&format!("active_pa={}\n", config.active_pa));
    contents.push_str(&format!("dxcluster_server={}\n", config.dxcluster_server));
    contents.push_str(&format!("dxcluster_callsign={}\n", config.dxcluster_callsign));
    contents.push_str(&format!("dxcluster_enabled={}\n", config.dxcluster_enabled));
    contents.push_str(&format!("dxcluster_expiry_min={}\n", config.dxcluster_expiry_min));
    if let Some(ref pw) = config.password {
        contents.push_str(&format!("password={}\n", sdr_remote_core::auth::obfuscate_password(pw)));
    }
    contents.push_str(&format!("totp_enabled={}\n", config.totp_enabled));
    if let Some(ref secret) = config.totp_secret {
        contents.push_str(&format!("totp_secret={}\n", sdr_remote_core::auth::obfuscate_password(secret)));
    }
    let _ = fs::write(&path, contents);
}

/// Format labels as comma-separated string for protocol transmission.
/// Sends same labels twice (A and B share the same 6 antennas).
pub fn labels_string(config: &ServerConfig) -> String {
    let mut parts: Vec<&str> = Vec::with_capacity(12);
    for l in &config.amplitec_labels {
        parts.push(l);
    }
    // Duplicate for B (same antennas)
    for l in &config.amplitec_labels {
        parts.push(l);
    }
    parts.join(",")
}
