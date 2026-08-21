// SPDX-License-Identifier: GPL-2.0-or-later

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Cosmetic alias for a StockCorner tuner - JC-3s and JC-4s share the same
/// MCP2221A-driven control protocol; the model name is only used for display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TunerModel {
    /// StockCorner JC-4s (default).
    Jc4s,
    /// StockCorner JC-3s.
    Jc3s,
}

impl TunerModel {
    /// Human-readable label for UI and logs.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Jc4s => "JC-4s",
            Self::Jc3s => "JC-3s",
        }
    }
    /// Config-file token (lowercase, hyphenated).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Jc4s => "jc-4s",
            Self::Jc3s => "jc-3s",
        }
    }
    /// Parse a config-file token; unknown strings fall back to `Jc4s`.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "jc-3s" | "jc3s" => Self::Jc3s,
            _ => Self::Jc4s,
        }
    }
}

/// Upper bound on the number of tuner-slots that may be configured at
/// once. Matches the number of Amplitec-A positions (6) so that each
/// position can have at most its own tuner.
pub const MAX_TUNERS: usize = 6;

/// Maximum number of rotor-slots. For now single rotor (Yaesu G-1000DXC
/// direct via MCP2221A). Higher value would only matter if/when multiple
/// rotor-backends per server become a use-case (e.g. one az-only + one
/// az/el station); raise here if that materialises.
pub const MAX_ROTORS: usize = 1;

/// Per-slot tuner configuration. Up to `MAX_TUNERS` of these live in
/// [`ServerConfig::tuners`].
#[derive(Clone, Debug)]
pub struct TunerConfig {
    /// Enable this tuner slot at server start.
    pub enabled: bool,
    /// Cosmetic model alias (JC-4s/JC-3s) used in the UI and logs only.
    pub model: TunerModel,
    /// USB serial of the MCP2221A board that drives this tuner.
    /// Empty string = "first available board on the bus" (legacy fallback,
    /// only sensible with a single tuner).
    pub mcp_serial: String,
    /// Amplitec-A antenna position (1..6) this tuner sits behind on the
    /// coax switch. `None` means the tuner is not bound to any Amplitec
    /// position; pressing Tune does not auto-route to it.
    pub amplitec_pos: Option<u8>,
    /// Tune-detector switch threshold on the yellow tune-status wire, in
    /// volts. Default 2.25 V (midpoint between the typical ~4.5 V idle and
    /// ~0 V LED-on level).
    pub threshold_v: f32,
    /// Hysteresis around the threshold, in volts. yellow_v <
    /// (threshold - hyst/2) counts as tune-active; yellow_v >
    /// (threshold + hyst/2) counts as tune-idle; samples inside the window
    /// preserve the previous state. Default 0.50 V.
    pub hysteresis_v: f32,
}

impl Default for TunerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: TunerModel::Jc4s,
            mcp_serial: String::new(),
            amplitec_pos: None,
            threshold_v: 2.25,
            hysteresis_v: 0.50,
        }
    }
}

/// Per-slot rotor configuration (PATCH-yaesu-rotor-mcp2221 phase 1).
/// One entry per physical MCP2221A-board that drives a Yaesu G-1000DXC
/// directly. Phase 1 only fills `enabled` + `mcp_serial` + `name`;
/// calibration fields (v_at_0deg / v_at_max_deg / max_deg) and
/// default_speed are added in later phases.
#[derive(Clone, Debug)]
pub struct RotorConfig {
    /// Enable this rotor slot at server start.
    pub enabled: bool,
    /// Display alias (UI + logs). Often equal to the part after
    /// `rot_` in the USB serial, but may be free text.
    pub name: String,
    /// USB serial of the MCP2221A-board that drives this rotor
    /// (`rot_<naam>` prefix per ThetisLink convention). Empty = not
    /// bound.
    pub mcp_serial: String,
    /// Calibration (phase 4): Yaesu pin-4 voltage at 0 deg (CCW end-park).
    /// Default 0.0 = not calibrated. Operator sets this via "Park CCW".
    pub v_at_0deg: f32,
    /// Calibration (phase 4): Yaesu pin-4 voltage at `max_deg`
    /// (CW end-park). Default 4.5 V (typical G-1000DXC). Operator sets
    /// this via "Park CW".
    pub v_at_max_deg: f32,
    /// Maximum rotation in degrees (default 450 for G-1000DXC).
    pub max_deg: u16,
    /// Soft-start/stop speed ramp in percent per second
    /// (PATCH phase 6). Default 50%/sec = full ramp 0->max in 2 s.
    /// Lower value for heavy antennas (5-10%/sec); higher value
    /// (75-100%/sec) for light antennas. Used both for the
    /// soft-start (gate-on -> max DAC) and for the soft-stop at
    /// GoTo-landing.
    pub ramp_pct_per_sec: f32,
    /// For rotors with an overlap-range (max_deg > 360, e.g. G-1000DXC with
    /// 450 deg): at GoTo, automatically pick the shortest route via the
    /// overlap-zone. Example at max_deg=450: current 350 deg, target
    /// 30 deg -> CCW-route = 320 deg, CW via 390 deg = 40 deg -> CW wins. Default
    /// off so "go to 30 deg" literally ends physically at 30 deg.
    pub shortest_route_in_overlap: bool,
}

impl Default for RotorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            name: String::new(),
            mcp_serial: String::new(),
            v_at_0deg: 0.0,
            v_at_max_deg: 4.5,
            max_deg: 450,
            ramp_pct_per_sec: 50.0,
            shortest_route_in_overlap: false,
        }
    }
}

/// Serializes all read-modify-write sequences on the server config file.
/// Without it the UI thread (active_pa toggle, save_window_positions,
/// start_server) and the RF2K poll thread (save_saved_drive) can race:
/// each loads, modifies its field, writes - and the later writer silently
/// drops the other's update if their load-windows overlap.
static CONFIG_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone)]
pub struct ServerConfig {
    /// TCI WebSocket address (e.g. "127.0.0.1:40001")
    pub tci_addr: Option<String>,
    pub spectrum_enabled: bool,
    /// Whether the configured Thetis radio hardware has a second receiver (RX2).
    /// Set to `false` for single-receiver radios; the server then advertises
    /// `SINGLE_RECEIVER` so clients hide RX2 + VRX2 everywhere. Only affects
    /// display/subscription, not the radio.
    pub rx2_present: bool,
    /// Path to Thetis.exe for auto-launch (None = disabled)
    pub thetis_path: Option<String>,
    /// Yaesu FT-991A serial port (e.g. "COM8")
    pub yaesu_port: Option<String>,
    pub yaesu_enabled: bool,
    pub yaesu_baud: u32,
    /// Yaesu USB audio input device name pattern (e.g. "USB Audio")
    pub yaesu_audio_device: Option<String>,
    /// Yaesu USB audio OUTPUT (TX) device name pattern. `None`/empty -> use the input
    /// pattern for output too (legacy). Set it when capture/render endpoints have
    /// different names, so TX audio can't fall back onto the wrong codec
    /// (PATCH-yaesu-output-device).
    pub yaesu_audio_output_device: Option<String>,
    /// SSB/AM USB TX routing per PTT (default false): off = enable 991A routing
    /// while a client is present, then restore ~2 s after disconnect/shutdown.
    /// On = switch the 991A USB modulation source only during TX, then restore
    /// on PTT-off. FTX-1 keeps its internal auto source selection and is not driven here.
    pub yaesu_ssb_switch_on_ptt: bool,
    /// Same choice for slot 1. Per SLOT and not per model, because two radios of
    /// the same type can be attached (`detect_model` assigns per port: 2x 991A,
    /// 2x FTX-1 or a mix) and this is an operator's choice about ONE radio. The
    /// model decides only whether it means anything - a 991A thing.
    pub yaesu2_ssb_switch_on_ptt: bool,
    /// Permission to write the FTX-1's memory bank. Off by default, and deliberately
    /// so: on this radio `MW` resets the channel's CTCSS tone to 100.0 Hz, and there
    /// is no CAT route to put it back (measured from both sides). So a write costs
    /// the operator every tone stored in the radio itself. The server GUI explains
    /// that and asks for it to be accepted; without acceptance an FTX-1 memory write
    /// is refused. The FT-991A is unaffected - it stores tones fine.
    pub yaesu_memory_write_ack: bool,
    /// Same permission for slot 1, and separate on purpose: the cost lands on
    /// the tones stored in THAT radio, so granting it for one must not grant it
    /// for the other.
    pub yaesu2_memory_write_ack: bool,
    // -- Dual-radio slot 1 (PATCH-dual-radio-991a-ftx1) --
    // Second independent radio (e.g. FTX-1). The existing `yaesu_*`-keys
    // above = slot 0 (back-compat); these `yaesu2_*`-keys = slot 1. Both
    // share the common Yaesu-CAT dialect; `RadioModel` carries the differences.
    /// Slot-1 radio serial port (e.g. "COM25").
    pub yaesu2_port: Option<String>,
    pub yaesu2_enabled: bool,
    pub yaesu2_baud: u32,
    /// Slot-1 USB audio input device name pattern. Separate choice because two
    /// radios may identify identically as "USB Audio CODEC" (edge-case 6).
    pub yaesu2_audio_device: Option<String>,
    /// Slot-1 USB audio OUTPUT (TX) device pattern. `None`/empty -> input pattern
    /// (PATCH-yaesu-output-device).
    pub yaesu2_audio_output_device: Option<String>,
    /// Which channel of the FTX-1's stereo USB capture endpoint to take:
    /// 0 = L, 1 = R, 2 = mix (the AVERAGE of both, so a silent R costs 6 dB).
    /// Default 2.
    ///
    /// A property of the RADIO TYPE, not of a slot: this applies to an FTX-1 in
    /// whichever slot it sits, and to a 991A never - that one reports a single
    /// channel and the choice does not arise. It used to exist for slot 2 only,
    /// with slot 1 hardwired to L, so the same radio had a choice in one slot
    /// and not in the other (2026-08-20).
    ///
    /// Established by listening, on the radio, 2026-08-20: L and R really do
    /// carry the FTX-1's TWO RECEIVERS, separately. Switch on both receivers and
    /// `0` gives one, `1` gives the other, `2` gives both at once. The manual
    /// does not say so - it confirms a sub band exists and stops there - so this
    /// is an operator measurement, not a documented fact.
    ///
    /// `2` is the default because it cannot lose a receiver: whatever is on,
    /// you hear it. With one receiver active both channels carry the same thing
    /// and mix sounds like that receiver - measured that way on the radio.
    ///
    /// (An earlier note here claimed mix would then be 6 dB down, since mixing
    /// is an average. That only holds if the other channel is SILENT, and this
    /// radio appears to duplicate instead. Not measured either way - do not
    /// rely on it.)
    pub yaesu_audio_channel: u8,
    /// The radio model last detected on this slot's port, remembered so the
    /// settings screen knows which model-specific controls to offer without
    /// re-probing on every launch. `None` = never detected, and then the screen
    /// offers none of them: it cannot honestly say which radio they are for.
    /// Ids of administrator answers folded away in the server's chat window,
    /// comma-separated. Without this the server forgot them on every restart -
    /// the desktop client and the phone remember, and the server did not.
    pub chat_answers_seen: String,
    pub yaesu_model_last: Option<u8>,
    pub yaesu2_model_last: Option<u8>,
    /// Same choice for slot 1. Two FTX-1s can reasonably want different channels.
    pub yaesu2_audio_channel: u8,
    /// Amplitec 6/2 serial port (e.g. "COM3")
    pub amplitec_port: Option<String>,
    pub amplitec_enabled: bool,
    /// Labels for Amplitec antenna positions 1-6 (shared by A and B)
    pub amplitec_labels: [String; 6],
    /// Maximum forward power (W) per Amplitec-A position. `None` = no cap
    /// (PA drive runs free, identical to pre-v2.0.4 behaviour). When set,
    /// the power-cap controller sends `SpeCmd::DriveDown` /
    /// `Rf2kCmd::DriveDown` on the active PA whenever the PA-meter
    /// reports forward-watts > cap-for-current-mode. Mode multipliers
    /// (SSB/CW = 1.0, AM = 0.5, FM/DIG = 0.4) are applied uniformly.
    /// Snapshot is restored via the same number of `DriveUp` commands
    /// when the Amplitec-A switches to a different position.
    pub amplitec_max_w: [Option<u16>; 6],
    /// Per-position TX-blocked flag. When `true`, ANY TX detected while
    /// the Amplitec-A is on this position is force-cancelled by sending
    /// `TRX:0,false;` over TCI. Intended for RX-only antennas where any
    /// RF exposure would damage the front-end. The block is a reactive
    /// safety-net for Thetis-direct PTT (spacebar) and is independent
    /// of the power-cap drive-down mechanism.
    pub amplitec_tx_blocked: [bool; 6],
    /// Show Amplitec control window on start.
    pub show_amplitec_window: bool,
    /// Per-slot StockCorner tuner configuration. Index 0 = tuner 1, index 1 =
    /// tuner 2. Each slot is independently enabled, has its own model alias
    /// (cosmetic), targets a specific MCP2221A board by USB serial, and may be
    /// bound to a specific Amplitec-A antenna position so the server knows
    /// which tuner to drive when the user presses Tune. **Schema is wired into
    /// load/save now; the runtime + UI that consume it land in a follow-up
    /// patch, alongside the MCP2221A tuner-driver rewrite.**
    /// Dynamic list (1..=`MAX_TUNERS`) of tuner-slots, one per
    /// physical MCP2221A board. Previously a fixed `[TunerConfig; 2]`,
    /// now a `Vec` so the server can scale to the number of
    /// Amplitec-positions (max 6) without a code change per slot.
    /// Slot-index is 0-based internally; UI-labels and config-keys number
    /// from 1 (`tuner1_*`, `tuner2_*`, ...).
    pub tuners: Vec<TunerConfig>,
    /// Show tuner control window on start.
    pub show_tuner_window: bool,
    /// Per-slot Yaesu-rotor configuration (PATCH-yaesu-rotor-mcp2221).
    /// One entry per MCP2221A-board with `rot_` USB-serial prefix; up to
    /// `MAX_ROTORS`. Runtime-binding (rotor-backend `Mcp2221Yaesu`)
    /// lands in phase 3 of the brief; phase 1 fills the schema and the
    /// wizard-claim.
    pub rotors: Vec<RotorConfig>,
    /// SPE Expert 1.3K-FA serial port (e.g. "COM6")
    pub spe_port: Option<String>,
    pub spe_enabled: bool,
    /// Show SPE Expert control window on start.
    pub show_spe_window: bool,
    /// RF2K-S Raspberry Pi address (e.g. "192.168.1.50:8080")
    pub rf2k_addr: Option<String>,
    pub rf2k_enabled: bool,
    /// Show RF2K-S control window on start.
    pub show_rf2k_window: bool,
    /// Whether the chat window was open when the GUI last closed.
    pub show_chat_window: bool,
    /// UltraBeam RCU-06 serial port (e.g. "COM7")
    pub ultrabeam_port: Option<String>,
    pub ultrabeam_enabled: bool,
    /// Show UltraBeam control window on start.
    pub show_ultrabeam_window: bool,
    /// EA7HG Visual Rotor UDP address (e.g. "192.168.1.60:3010")
    pub rotor_addr: Option<String>,
    pub rotor_enabled: bool,
    /// Show Rotor control window on start.
    pub show_rotor_window: bool,
    /// Which rotor-backend is active: `"ea7hg"` (Visual Rotor, default) or
    /// `"pstrotator"` (XML over UDP to an external PstRotator). Empty or
    /// unknown is interpreted as `"ea7hg"` for backwards-compat.
    pub rotor_backend: String,
    /// PstRotator host - expects a numeric IP address (the worker
    /// parses `host:port` as `SocketAddr` and does no DNS resolution).
    /// PstRotator often runs on another PC in the same LAN -
    /// hence no hardcoded loopback default.
    pub pstrotator_host: String,
    /// PstRotator UDP listener port for XML commands. Default 12000.
    pub pstrotator_port: u16,
    /// Local UDP port on which our server receives PstRotator's replies
    /// (PstRotator sends replies to `port + 1` = default 12001).
    pub pstrotator_feedback_port: u16,
    /// Also poll `EL?` for elevation-rotors. Default `false` (AZ only).
    pub pstrotator_has_elevation: bool,
    /// PstRotator UDP-listener (v2.1.1+): listens in parallel to the active
    /// `rotor_backend` for incoming azimuth-broadcasts from e.g. Log4OM ->
    /// PstRotator and converts them into `RotorCmd::GoTo` on the Rotor-facade.
    /// Makes it possible to drive the Adafruit-rotor from a logging program
    /// without activating the outgoing PstRotator-backend.
    pub pstrotator_listen_enabled: bool,
    /// UDP-port for the listener. Default 12001 (=PstRotator's default
    /// feedback-port that broadcasts the outgoing side of its
    /// azimuth-updates).
    pub pstrotator_listen_port: u16,
    /// Saved window positions: [x, y]
    pub tuner_window_pos: Option<[f32; 2]>,
    pub amplitec_window_pos: Option<[f32; 2]>,
    pub spe_window_pos: Option<[f32; 2]>,
    pub rf2k_window_pos: Option<[f32; 2]>,
    pub chat_window_pos: Option<[f32; 2]>,
    pub ultrabeam_window_pos: Option<[f32; 2]>,
    pub rotor_window_pos: Option<[f32; 2]>,
    /// Saved main window position: [x, y]
    pub main_window_pos: Option<[f32; 2]>,
    /// Per-monitor placement matrices of the arranger, one line.
    pub layout_grids: String,
    /// Stored arrangements, one line each.
    pub layout_memories: Vec<String>,
    /// UI scale of the server GUI (1.0 = 100%).
    pub ui_zoom: f32,
    /// Saved window sizes: [w, h]
    pub main_window_size: Option<[f32; 2]>,
    pub tuner_window_size: Option<[f32; 2]>,
    pub amplitec_window_size: Option<[f32; 2]>,
    pub spe_window_size: Option<[f32; 2]>,
    pub rf2k_window_size: Option<[f32; 2]>,
    pub chat_window_size: Option<[f32; 2]>,
    pub ultrabeam_window_size: Option<[f32; 2]>,
    pub rotor_window_size: Option<[f32; 2]>,
    /// UI theme of the server-GUI: "classic" (default, the original
    /// light-grey), "dark", "slate" or "custom". Same variants as the client.
    pub theme: String,
    /// Custom-palette as `bg,widget,text,accent` (each rrggbb). Empty = not set.
    pub theme_custom: String,
    /// UI language of the server-GUI: "en"/"nl"/"de"/"fr". Default "nl" (server was
    /// always Dutch). Phased migration: not-yet-migrated texts stay
    /// Dutch regardless of this choice.
    pub language: String,
    /// Auto-start server on launch (skip settings screen)
    pub autostart: bool,
    /// Active PA: 0=none, 1=SPE, 2=RF2K
    pub active_pa: u8,
    /// Persisted pre-Operate Thetis ZZPC drive level per PA. `Some(n)` is the
    /// last value captured when the PA went Standby->Operate; `None` means
    /// no snapshot has been taken yet. Used by the TL2-server drive observer
    /// to restore the value when the PA goes Operate->Standby, even across a
    /// host restart that would otherwise lose the in-memory observer state.
    pub rf2k_saved_drive: Option<u8>,
    pub spe_saved_drive: Option<u8>,
    /// UltraBeam control window: Menu section expanded (collapsible state)
    pub ultrabeam_show_menu: bool,
    /// Status panel: MCP2221A tuner-bridges section expanded. Persisted so
    /// the operator's last open/closed choice survives a server restart.
    pub mcp2221_section_expanded: bool,
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
    /// PATCH-3: human-readable name advertised via mDNS so clients can
    /// distinguish multiple ThetisLink servers on the same LAN
    /// (e.g. "Shack PC"). `None` falls back to the OS hostname.
    pub friendly_name: Option<String>,
    /// Optional outbound relay monitor (Phase A only; does not route TL frames yet).
    pub relay_enabled: bool,
    pub relay_url: String,
    pub relay_station: String,
    pub relay_token: String,
    /// PATCH-relay-audio-udp: route audio+PTT over kale UDP (443) instead of the wss
    /// tunnel. Default on. Env THETISLINK_RELAY_UDP_PORT overrides.
    pub relay_udp_enabled: bool,
}

/// The value a key gets when an **existing** config file does not mention it.
///
/// That is the only thing this is for, and it is why several devices are on
/// here: a key missing from a real file means the file predates the key, and
/// switching a device off underneath an operator who has been using it is not
/// a default, it is a regression. A machine with no config file at all is a
/// different case entirely - see [`ServerConfig::first_run`].
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            tci_addr: None,
            spectrum_enabled: true,
            rx2_present: true,
            thetis_path: detect_thetis_path(),
            yaesu_port: None,
            yaesu_enabled: false,
            yaesu_baud: 38400,
            yaesu_ssb_switch_on_ptt: false,
            yaesu2_ssb_switch_on_ptt: false,
            yaesu_memory_write_ack: false,
            yaesu2_memory_write_ack: false,
            yaesu_audio_device: None,
            yaesu_audio_output_device: None,
            yaesu2_port: None,
            yaesu2_enabled: false,
            yaesu2_baud: 38400,
            yaesu2_audio_device: None,
            yaesu2_audio_output_device: None,
            // Mix: the choice that cannot lose a receiver (see the field docs).
            yaesu_audio_channel: 2,
            chat_answers_seen: String::new(),
            yaesu_model_last: None,
            yaesu2_model_last: None,
            yaesu2_audio_channel: 2,
            amplitec_port: None,
            amplitec_enabled: true,
            amplitec_labels: default_labels("Ant"),
            amplitec_max_w: [None; 6],
            amplitec_tx_blocked: [false; 6],
            show_amplitec_window: true,
            tuners: Vec::new(),
            show_tuner_window: true,
            rotors: Vec::new(),
            spe_port: None,
            spe_enabled: true,
            show_spe_window: true,
            rf2k_addr: None,
            rf2k_enabled: true,
            show_rf2k_window: true,
            // Closed until asked for: the chat is the least important thing here.
            show_chat_window: false,
            ultrabeam_port: None,
            ultrabeam_enabled: true,
            show_ultrabeam_window: true,
            rotor_addr: None,
            rotor_enabled: true,
            show_rotor_window: true,
            rotor_backend: "ea7hg".to_string(),
            pstrotator_host: String::new(),
            pstrotator_port: 12000,
            pstrotator_feedback_port: 12001,
            pstrotator_has_elevation: false,
            pstrotator_listen_enabled: false,
            pstrotator_listen_port: 12001,
            tuner_window_pos: None,
            amplitec_window_pos: None,
            spe_window_pos: None,
            rf2k_window_pos: None,
            chat_window_pos: None,
            ultrabeam_window_pos: None,
            rotor_window_pos: None,
            main_window_pos: None,
            layout_grids: String::new(),
            layout_memories: Vec::new(),
            ui_zoom: 1.0,
            main_window_size: None,
            tuner_window_size: None,
            amplitec_window_size: None,
            spe_window_size: None,
            rf2k_window_size: None,
            chat_window_size: None,
            ultrabeam_window_size: None,
            rotor_window_size: None,
            theme: "classic".to_string(),
            theme_custom: String::new(),
            language: "nl".to_string(),
            autostart: false,
            active_pa: 0,
            rf2k_saved_drive: None,
            spe_saved_drive: None,
            ultrabeam_show_menu: false,
            mcp2221_section_expanded: true,
            dxcluster_server: "dxc.pi4cc.nl:8000".to_string(),
            // No hardcoded callsign - the operator enters their own in server
            // settings on first run; DX cluster stays offline until it's set.
            dxcluster_callsign: String::new(),
            dxcluster_enabled: true,
            dxcluster_expiry_min: 10,
            password: None,
            totp_secret: None,
            totp_enabled: false,
            friendly_name: None,
            relay_enabled: false,
            relay_url: String::new(),
            relay_station: String::new(),
            relay_token: String::new(),
            relay_udp_enabled: true,
        }
    }
}

impl ServerConfig {
    /// What the server starts as on a machine that has never run it: no config
    /// file exists yet.
    ///
    /// Everything optional is off. The first start is then a bare server that
    /// a client can connect to and nothing else - no amplifier, no tuner, no
    /// rotor, no cluster, no second receiver, and no windows opening for
    /// hardware that is not there. The operator switches on what is actually
    /// attached, one thing at a time, and every one of those is a checkbox
    /// that was off a moment ago.
    ///
    /// Note what this is *not*: it is not [`Default`], which fills in keys an
    /// existing file happens to lack. Both are "the default" in English and
    /// they mean opposite things here, which is why they are two functions.
    pub fn first_run() -> Self {
        Self {
            // Read the OS display language once, here. From the next start on,
            // `language=` in the file is the operator's own choice.
            language: sdr_remote_core::oslang::detect_ui_language().to_string(),
            rx2_present: false,
            amplitec_enabled: false,
            show_amplitec_window: false,
            show_tuner_window: false,
            spe_enabled: false,
            show_spe_window: false,
            rf2k_enabled: false,
            show_rf2k_window: false,
            ultrabeam_enabled: false,
            show_ultrabeam_window: false,
            rotor_enabled: false,
            show_rotor_window: false,
            dxcluster_enabled: false,
            // Off with the rest, even though it is the lower-latency route:
            // the relay itself is off on a first run, so this decides nothing
            // yet, and a first screen where every box is clear is worth more
            // than a setting that only starts mattering two steps later.
            relay_udp_enabled: false,
            ..Self::default()
        }
    }
}

/// Tries to parse a `tuner<N>_FIELD` key. Returns `Some((slot0_idx,
/// field))` where `slot0_idx = N - 1`. Accepts N in 1..=`MAX_TUNERS`.
/// Returns `None` for non-tuner-keys or an unknown slot-index.
fn parse_tuner_slot_prefix(key: &str) -> Option<(usize, &str)> {
    let rest = key.strip_prefix("tuner")?;
    let underscore = rest.find('_')?;
    let n: usize = rest[..underscore].parse().ok()?;
    if (1..=MAX_TUNERS).contains(&n) {
        Some((n - 1, &rest[underscore + 1..]))
    } else {
        None
    }
}

/// Grows the `tuners`-Vec so that `slot0_idx` is a valid index, with
/// default `TunerConfig`-entries for gaps. Operator-config may have a Vec of
/// 0 (first run without tuners) and only `tuner2_*` keys; in that
/// case slot 0 (and any other gaps) get a default-entry.
fn ensure_tuner_slot(tuners: &mut Vec<TunerConfig>, slot0_idx: usize) {
    if slot0_idx >= MAX_TUNERS {
        return;
    }
    while tuners.len() <= slot0_idx {
        tuners.push(TunerConfig::default());
    }
}

/// Apply a `tuner<N>_FIELD=value` config entry to the matching
/// `TunerConfig` slot. Unknown sub-keys are silently ignored so a
/// future field-name doesn't have to be cross-version-compatible.
fn parse_tuner_key(t: &mut TunerConfig, sub: &str, value: &str) {
    match sub {
        "enabled" => t.enabled = value != "false",
        "model" => t.model = TunerModel::parse(value),
        "mcp_serial" => t.mcp_serial = value.to_string(),
        "amplitec_pos" => {
            t.amplitec_pos = match value.parse::<u8>().ok() {
                Some(n) if (1..=6).contains(&n) => Some(n),
                _ => None,
            };
        }
        "threshold_v" => {
            if let Ok(v) = value.parse::<f32>() {
                t.threshold_v = v.clamp(0.5, 4.5);
            }
        }
        "hysteresis_v" => {
            if let Ok(v) = value.parse::<f32>() {
                t.hysteresis_v = v.clamp(0.1, 2.0);
            }
        }
        _ => {}
    }
}

/// `rotor<N>_FIELD` parser parallel to `parse_tuner_slot_prefix`. Returns
/// `(slot0_idx, sub)` for N in 1..=`MAX_ROTORS`.
fn parse_rotor_slot_prefix(key: &str) -> Option<(usize, &str)> {
    let rest = key.strip_prefix("rotor")?;
    let underscore = rest.find('_')?;
    let n: usize = rest[..underscore].parse().ok()?;
    if (1..=MAX_ROTORS).contains(&n) {
        Some((n - 1, &rest[underscore + 1..]))
    } else {
        None
    }
}

fn ensure_rotor_slot(rotors: &mut Vec<RotorConfig>, slot0_idx: usize) {
    if slot0_idx >= MAX_ROTORS {
        return;
    }
    while rotors.len() <= slot0_idx {
        rotors.push(RotorConfig::default());
    }
}

fn parse_rotor_key(r: &mut RotorConfig, sub: &str, value: &str) {
    match sub {
        "enabled" => r.enabled = value != "false",
        "name" => r.name = value.to_string(),
        "mcp_serial" => r.mcp_serial = value.to_string(),
        "v_at_0deg" => {
            if let Ok(v) = value.parse::<f32>() {
                // Yaesu pin-4 voltage after undoing the
                // board voltage-divider. With the new 1.8k+2.2k divider
                // (ratio 1.818) the measurement range reaches ~7.45 V; an earlier
                // clamp of 5.0 V cut off the high-degrees calibration after
                // restart. 10 V gives ample margin for all realistic
                // rotors without accepting load-file corruption.
                r.v_at_0deg = v.clamp(0.0, 10.0);
            }
        }
        "v_at_max_deg" => {
            if let Ok(v) = value.parse::<f32>() {
                r.v_at_max_deg = v.clamp(0.0, 10.0);
            }
        }
        "max_deg" => {
            if let Ok(v) = value.parse::<u16>() {
                r.max_deg = v.clamp(90, 720);
            }
        }
        "ramp_pct_per_sec" => {
            if let Ok(v) = value.parse::<f32>() {
                r.ramp_pct_per_sec = v.clamp(1.0, 200.0);
            }
        }
        "shortest_route_in_overlap" => {
            r.shortest_route_in_overlap = matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            );
        }
        _ => {}
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

/// Public load: takes the global CONFIG_LOCK so external callers always see
/// a consistent snapshot - never the half-written file from a concurrent
/// save. Internal `load_unlocked` is for `modify_config` which already
/// holds the lock.
pub fn load() -> ServerConfig {
    let _guard = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    load_unlocked().0
}

/// Where a loaded config came from. The three are kept apart because two of
/// them look identical from the outside and are not the same thing at all.
#[derive(Debug, PartialEq, Eq)]
enum ConfigSource {
    /// Read and parsed.
    File,
    /// There is no file: nothing has ever been configured on this machine, and
    /// this is the only case that may hand out the bare-server defaults.
    FirstRun,
    /// There IS a file and it could not be read - locked by another process,
    /// permissions, a failing disk. The operator's settings are still in it, so
    /// nothing derived from this may be written back.
    Unreadable,
}

/// Classify the result of reading the config file.
///
/// Split off so the difference between "absent" and "unreadable" can be tested
/// without arranging a real unreadable file, which is awkward on Windows and
/// impossible in CI.
fn classify_read(read: &Result<String, io::Error>) -> ConfigSource {
    match read {
        Ok(_) => ConfigSource::File,
        Err(e) if e.kind() == io::ErrorKind::NotFound => ConfigSource::FirstRun,
        Err(_) => ConfigSource::Unreadable,
    }
}

/// The config, plus whether it is safe to write back.
///
/// The second half exists because `modify_config` is load-mutate-save over the
/// WHOLE file. Treating an unreadable file as a first run therefore does not
/// mean "the server starts bare" but "the operator's configuration is gone":
/// one failed read during normal use - the rotor storing its park position, an
/// Active tick - would replace ports, tuners, password and relay token with a
/// bare config. On Windows a sharing violation on a file next to the exe is the
/// realistic trigger. Found in review, 2026-08-20; the path predates this
/// release, but this release is the one that made the fallback emptier.
fn load_unlocked() -> (ServerConfig, bool) {
    let path = config_path();
    let read = fs::read_to_string(&path);
    let source = classify_read(&read);
    // The flag is process-wide and is cleared by any load that succeeds or that
    // confirms the file is absent. Nothing else clears it, and after startup only
    // `modify_config` still loads - so an unreadable file at start means this
    // process will not write until it is restarted. That is deliberate, and the
    // log line says so.
    LOAD_FAILED.store(source == ConfigSource::Unreadable, Ordering::Relaxed);
    match &source {
        ConfigSource::FirstRun => log::info!(
            "No {} yet - first run: UI language from the OS, all optional devices off",
            path.display()
        ),
        ConfigSource::Unreadable => log::error!(
            "Cannot read {} ({}). The file is still there, so its settings are NOT lost -              but this run will not write to it, to avoid replacing them with defaults.              Close whatever is holding the file and restart.",
            path.display(),
            read.as_ref()
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "unknown error".to_string())
        ),
        ConfigSource::File => {}
    }
    config_from_source(source, read.ok().as_deref())
}

/// Which config a classified read yields, and whether it may be written back.
///
/// Split from the reading and the logging because this mapping is what actually
/// broke: the defect was never in telling absent from unreadable, it was in
/// giving an unreadable file the bare first-run values AND permission to
/// overwrite. A test on the classifier alone stayed green through exactly that
/// (review finding, 2026-08-20).
fn config_from_source(source: ConfigSource, text: Option<&str>) -> (ServerConfig, bool) {
    match source {
        ConfigSource::File => (parse_conf(text.unwrap_or("")), true),
        // Nothing has ever been configured here, so the bare values are right
        // and writing them is right.
        ConfigSource::FirstRun => (ServerConfig::first_run(), true),
        // The settings are still in the file. Hand out something harmless and
        // forbid the write - NOT `first_run()`, which would put a bare server on
        // screen and, at the first save, in the file.
        ConfigSource::Unreadable => (ServerConfig::default(), false),
    }
}

/// Set when the config file exists but could not be read. Guards every write:
/// the settings screen keeps its own copy of the config and saves that, so
/// skipping the save in `modify_config` alone would still leave Save & Start
/// able to overwrite a file this process never managed to read.
static LOAD_FAILED: AtomicBool = AtomicBool::new(false);

/// Turn the text of a config file into a `ServerConfig`.
///
/// Split out from `load_unlocked` so the migration of older files can be
/// tested on a string instead of on whatever happens to be on this machine's
/// disk - the same reason `render_conf` was split off from saving.
fn parse_conf(contents: &str) -> ServerConfig {
    let mut config = ServerConfig::default();

    // Three settings that used to be one value for both radios. They are per
    // slot now - two radios of the same type can be attached, and each of these
    // is a choice about ONE radio. Collected as options so the old shared key
    // can seed a slot that has no key of its own, and never overrule one that
    // has (2026-08-20).
    let mut slot_ssb: [Option<bool>; 2] = [None; 2];
    let mut slot_mem_ack: [Option<bool>; 2] = [None; 2];
    let mut slot_channel: [Option<u8>; 2] = [None; 2];
    let mut shared_mem_ack: Option<bool> = None;
    let mut shared_channel: Option<u8> = None;

    for line in contents.lines() {
        let line = line.trim();
        if let Some((key, value)) = line.split_once('=') {
            match key.trim() {
                "tci" => {
                    let v = value.trim().to_string();
                    config.tci_addr = if v.is_empty() { None } else { Some(v) };
                }
                // Legacy keys (ignored, kept for backward compat with old config files)
                "input" | "input2" | "output" | "anan_interface" => {}
                "rx2_present" => {
                    // Default true; only an explicit "false" disables RX2.
                    config.rx2_present = value.trim() != "false";
                }
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
                // Absent key -> false: an older conf must not silently grant it.
                "yaesu_memory_write_ack" => {
                    slot_mem_ack[0] = Some(value.trim() == "true");
                }
                "yaesu2_memory_write_ack" => {
                    slot_mem_ack[1] = Some(value.trim() == "true");
                }
                // The old shared name. Applied to BOTH slots below, but only
                // where that slot has no key of its own, so an upgrade keeps
                // the permission the operator actually gave.
                "ftx1_memory_write_ack" => {
                    shared_mem_ack = Some(value.trim() == "true");
                }
                "yaesu_ssb_switch_on_ptt" => {
                    slot_ssb[0] = Some(value.trim() != "false");
                }
                "yaesu2_ssb_switch_on_ptt" => {
                    slot_ssb[1] = Some(value.trim() != "false");
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
                "yaesu_audio_out" => {
                    let v = value.trim().to_string();
                    config.yaesu_audio_output_device = if v.is_empty() { None } else { Some(v) };
                }
                "yaesu2_port" => {
                    let v = value.trim().to_string();
                    config.yaesu2_port = if v.is_empty() { None } else { Some(v) };
                }
                "yaesu2_enabled" => {
                    config.yaesu2_enabled = value.trim() != "false";
                }
                "yaesu2_baud" => {
                    if let Ok(v) = value.trim().parse::<u32>() {
                        config.yaesu2_baud = v;
                    }
                }
                "yaesu2_audio" => {
                    let v = value.trim().to_string();
                    config.yaesu2_audio_device = if v.is_empty() { None } else { Some(v) };
                }
                "yaesu2_audio_out" => {
                    let v = value.trim().to_string();
                    config.yaesu2_audio_output_device = if v.is_empty() { None } else { Some(v) };
                }
                "chat_answers_seen" => {
                    config.chat_answers_seen = value.trim().to_string();
                }
                "yaesu_model_last" => {
                    config.yaesu_model_last = value.trim().parse::<u8>().ok();
                }
                "yaesu2_model_last" => {
                    config.yaesu2_model_last = value.trim().parse::<u8>().ok();
                }
                // Three names for what is now two settings. `ftx1_audio_channel`
                // never shipped in a release - it was the shared name for a
                // handful of development builds - and `yaesu2_audio_channel` was
                // the SHARED setting up to and including 2.9.1, despite the "2"
                // in it. Both seed a slot that has no key of its own.
                "yaesu_audio_channel" | "yaesu2_audio_channel" | "ftx1_audio_channel" => {
                    // Tolerant: accept a digit (0/1/2) or L/R/MIX (case-insensitive).
                    let v = value.trim().to_lowercase();
                    let parsed = match v.as_str() {
                        "0" | "l" | "left" => 0,
                        "1" | "r" | "right" => 1,
                        _ => 2, // "2"/"mix"/unknown -> mix
                    };
                    match key.trim() {
                        "yaesu_audio_channel" => slot_channel[0] = Some(parsed),
                        "yaesu2_audio_channel" => slot_channel[1] = Some(parsed),
                        _ => shared_channel = Some(parsed),
                    }
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
                k if k.starts_with("amplitec_max_w_a") => {
                    if let Some(idx) = k
                        .strip_prefix("amplitec_max_w_a")
                        .and_then(|s| s.parse::<usize>().ok())
                    {
                        if (1..=6).contains(&idx) {
                            let v = value.trim();
                            // Empty string OR "0" = no cap (None). A
                            // configured 0 W cap is functionally not
                            // usable (the cap-loop would fire continuously on
                            // every fwd>0) and not what the operator intends.
                            config.amplitec_max_w[idx - 1] = match v.parse::<u16>() {
                                Ok(0) => None,
                                Ok(n) => Some(n),
                                Err(_) => None,
                            };
                        }
                    }
                }
                k if k.starts_with("amplitec_tx_blocked_a") => {
                    if let Some(idx) = k
                        .strip_prefix("amplitec_tx_blocked_a")
                        .and_then(|s| s.parse::<usize>().ok())
                    {
                        if (1..=6).contains(&idx) {
                            config.amplitec_tx_blocked[idx - 1] = value.trim() == "true";
                        }
                    }
                }
                // Legacy v2.0.2 keys silently ignored on load (multi-tuner
                // runtime supersedes the single serial-port flow):
                //   `tuner_port`           - COM-port no longer used
                //   `tuner_enabled`        - replaced by per-slot enable
                //   `tuner_assume_tuned`   - assume-tuned pad retired
                "tuner_port" => {}
                "tuner_enabled" => {
                    // Honour the legacy global toggle one last time so
                    // owners upgrading from v2.0.2 do not lose their
                    // slot-0 enable state: mirror into tuners[0].enabled.
                    let v = value.trim() != "false";
                    ensure_tuner_slot(&mut config.tuners, 0);
                    config.tuners[0].enabled = v;
                }
                "tuner_assume_tuned" => {}
                // Per-slot keys: `tuner<N>_FIELD=value` with N=1..MAX_TUNERS.
                // Auto-grow the Vec to index N-1 so old config-files
                // (two slots) and new ones (1..6 slots) both work.
                k if parse_tuner_slot_prefix(k).is_some() => {
                    if let Some((slot0_idx, sub)) = parse_tuner_slot_prefix(k) {
                        ensure_tuner_slot(&mut config.tuners, slot0_idx);
                        parse_tuner_key(&mut config.tuners[slot0_idx], sub, value.trim());
                    }
                }
                // Per-slot rotor-keys: `rotor<N>_FIELD=value` with N=1..MAX_ROTORS.
                // PATCH-yaesu-rotor-mcp2221 phase 1.
                k if parse_rotor_slot_prefix(k).is_some() => {
                    if let Some((slot0_idx, sub)) = parse_rotor_slot_prefix(k) {
                        ensure_rotor_slot(&mut config.rotors, slot0_idx);
                        parse_rotor_key(&mut config.rotors[slot0_idx], sub, value.trim());
                    }
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
                "chat_window" => {
                    config.show_chat_window = value.trim() == "true";
                }
                "chat_pos_x" => {
                    if let Ok(v) = value.trim().parse::<f32>() {
                        config.chat_window_pos.get_or_insert([0.0, 0.0])[0] = v;
                    }
                }
                "chat_pos_y" => {
                    if let Ok(v) = value.trim().parse::<f32>() {
                        config.chat_window_pos.get_or_insert([0.0, 0.0])[1] = v;
                    }
                }
                "chat_size_w" => {
                    if let Ok(v) = value.trim().parse::<f32>() {
                        config.chat_window_size.get_or_insert([0.0, 0.0])[0] = v;
                    }
                }
                "chat_size_h" => {
                    if let Ok(v) = value.trim().parse::<f32>() {
                        config.chat_window_size.get_or_insert([0.0, 0.0])[1] = v;
                    }
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
                "rotor_backend" => {
                    // Accept the three valid backends; anything else
                    // falls back to "ea7hg" for backwards-compat.
                    let v = value.trim().to_lowercase();
                    config.rotor_backend = match v.as_str() {
                        "pstrotator" => "pstrotator".to_string(),
                        "mcp2221_yaesu" => "mcp2221_yaesu".to_string(),
                        _ => "ea7hg".to_string(),
                    };
                }
                "pstrotator_host" => {
                    config.pstrotator_host = value.trim().to_string();
                }
                "pstrotator_port" => {
                    if let Ok(v) = value.trim().parse::<u16>() {
                        if v > 0 { config.pstrotator_port = v; }
                    }
                }
                "pstrotator_feedback_port" => {
                    if let Ok(v) = value.trim().parse::<u16>() {
                        if v > 0 { config.pstrotator_feedback_port = v; }
                    }
                }
                "pstrotator_has_elevation" => {
                    config.pstrotator_has_elevation = value.trim() == "true";
                }
                "pstrotator_listen_enabled" => {
                    config.pstrotator_listen_enabled = value.trim() == "true";
                }
                "pstrotator_listen_port" => {
                    if let Ok(v) = value.trim().parse::<u16>() {
                        if v > 0 { config.pstrotator_listen_port = v; }
                    }
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
                "layout_grids" => {
                    config.layout_grids = value.trim().to_string();
                }
                "ui_zoom" => {
                    if let Ok(v) = value.trim().parse::<f32>() {
                        config.ui_zoom = v.clamp(0.5, 2.0);
                    }
                }
                // layout_mem<N> = <name>|<key:x,y,w,h>...   N is 1-based.
                k if k.starts_with("layout_mem") => {
                    if let Ok(i) = k["layout_mem".len()..].trim().parse::<usize>() {
                        if (1..=64).contains(&i) {
                            if config.layout_memories.len() < i {
                                config.layout_memories.resize(i, String::new());
                            }
                            config.layout_memories[i - 1] = value.trim().to_string();
                        }
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
                "theme" => {
                    config.theme = value.trim().to_string();
                }
                "theme_custom" => {
                    config.theme_custom = value.trim().to_string();
                }
                "language" => {
                    // Accept only languages with a locale (en/nl/de/fr); otherwise nl.
                    config.language = match value.trim() {
                        "en" | "nl" | "de" | "fr" => value.trim().to_string(),
                        _ => "nl".to_string(),
                    };
                }
                "autostart" => {
                    config.autostart = value.trim() == "true";
                }
                "active_pa" => {
                    config.active_pa = value.trim().parse().unwrap_or(0);
                }
                "rf2k_saved_drive" => {
                    let v = value.trim();
                    config.rf2k_saved_drive = if v.is_empty() { None } else { v.parse().ok() };
                }
                "spe_saved_drive" => {
                    let v = value.trim();
                    config.spe_saved_drive = if v.is_empty() { None } else { v.parse().ok() };
                }
                "ultrabeam_show_menu" => {
                    config.ultrabeam_show_menu = value.trim() == "true";
                }
                "mcp2221_section_expanded" => {
                    config.mcp2221_section_expanded = value.trim() != "false";
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
                "friendly_name" => {
                    let v = value.trim();
                    if !v.is_empty() {
                        config.friendly_name = Some(v.to_string());
                    }
                }
                "relay_enabled" => {
                    config.relay_enabled = value.trim() == "true";
                }
                "relay_udp_enabled" => {
                    config.relay_udp_enabled = value.trim() == "true";
                }
                "relay_url" => {
                    config.relay_url = value.trim().to_string();
                }
                "relay_station" => {
                    config.relay_station = value.trim().to_string();
                }
                "relay_token" => {
                    let v = value.trim();
                    if !v.is_empty() {
                        config.relay_token = sdr_remote_core::auth::deobfuscate_password(v)
                            .unwrap_or_else(|| v.to_string());
                    }
                }
                _ => {}
            }
        }
    }

    // `yaesu2_audio_channel` is the one old key whose name lies: up to and
    // including 2.9.1 it was the SHARED channel setting, applied to whichever
    // slot held an FTX-1, and only the "2" in it suggests otherwise. A config
    // this build writes always carries both channel keys, so slot 2's key
    // standing alone can only mean an older file - and there it has to seed slot
    // 1 as well, or an operator with an FTX-1 in slot 1 silently loses the
    // channel they chose.

    if slot_channel[0].is_none() && shared_channel.is_none() {
        shared_channel = slot_channel[1];
    }

    // A slot's own key wins; otherwise the old shared key, which an upgrading
    // config still carries; otherwise the default already in place.
    let shared_ssb = if slot_ssb[0].is_some() { slot_ssb[0] } else { None };
    config.yaesu_ssb_switch_on_ptt = slot_ssb[0].unwrap_or(config.yaesu_ssb_switch_on_ptt);
    config.yaesu2_ssb_switch_on_ptt = slot_ssb[1]
        .or(shared_ssb)
        .unwrap_or(config.yaesu2_ssb_switch_on_ptt);
    config.yaesu_memory_write_ack = slot_mem_ack[0]
        .or(shared_mem_ack)
        .unwrap_or(config.yaesu_memory_write_ack);
    config.yaesu2_memory_write_ack = slot_mem_ack[1]
        .or(shared_mem_ack)
        .unwrap_or(config.yaesu2_memory_write_ack);
    config.yaesu_audio_channel = slot_channel[0]
        .or(shared_channel)
        .unwrap_or(config.yaesu_audio_channel);
    config.yaesu2_audio_channel = slot_channel[1]
        .or(shared_channel)
        .unwrap_or(config.yaesu2_audio_channel);

    config
}


/// Atomic load-modify-save helper. All read-modify-write helpers funnel
/// through here so the CONFIG_LOCK guarantees no other writer slips in
/// between the load and the save.
pub fn modify_config(f: impl FnOnce(&mut ServerConfig)) {
    let _guard = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut config, may_write) = load_unlocked();
    f(&mut config);
    if !may_write {
        // The load fell back because the file could not be READ. Writing now
        // would put that fallback where the operator's settings are.
        log::error!("Not saving: the config file could not be read, so this would overwrite it");
        return;
    }
    let _ = save_unlocked(&config);
}

/// Read-modify-write helper for the `active_pa` config key. Used when the
/// user clicks an Active checkbox on RF2K or SPE so the choice survives a
/// non-graceful shutdown (process kill / power loss) without waiting for
/// `start_server()` to persist the rest of the config.
pub fn save_active_pa(value: u8) {
    modify_config(|c| c.active_pa = value);
}

/// Read-modify-write helper for the per-PA pre-Operate drive snapshot.
/// `pa_id`: 1 = SPE, 2 = RF2K (matching the `active_pa` encoding).
/// `value`: `Some(zzpc)` to write, `None` to clear.
pub fn save_saved_drive(pa_id: u8, value: Option<u8>) {
    modify_config(|c| match pa_id {
        1 => c.spe_saved_drive = value,
        2 => c.rf2k_saved_drive = value,
        _ => {}
    });
}

/// Read-modify-write helper for the UltraBeam window Menu collapsible state.
pub fn save_ultrabeam_show_menu(value: bool) {
    modify_config(|c| c.ultrabeam_show_menu = value);
}

/// Read-modify-write helper for the MCP2221A tuner-bridges section
/// collapsible state in the Status panel.
pub fn save_mcp2221_section_expanded(value: bool) {
    modify_config(|c| c.mcp2221_section_expanded = value);
}

/// Public save: takes the global CONFIG_LOCK so writes are serialised with
/// reads and other writes. Internal `save_unlocked` is for `modify_config`
/// which already holds the lock.
/// Save, and say whether it actually happened.
///
/// The return value exists because the refusal below is otherwise invisible: an
/// operator whose config file was locked at startup would fill in a whole
/// settings screen, press Save & Start, watch the server run correctly for that
/// session, and find nothing saved - with not a word on screen (review finding,
/// 2026-08-20).
pub fn save(config: &ServerConfig) -> bool {
    let _guard = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    save_unlocked(config)
}

/// Could this process write the config file if it were asked to now?
///
/// False while the file exists but an earlier read of it failed. That lasts
/// until a load succeeds, and after startup only `modify_config` still loads -
/// so in practice it lasts until the server is restarted.
pub fn may_write() -> bool {
    !LOAD_FAILED.load(Ordering::Relaxed) || !config_path().exists()
}

fn save_unlocked(config: &ServerConfig) -> bool {
    let path = config_path();
    // The settings screen holds its own copy and saves that, so this guard has
    // to sit at the write itself rather than only in `modify_config`: Save &
    // Start would otherwise still replace a file we never managed to read.
    if LOAD_FAILED.load(Ordering::Relaxed) && path.exists() {
        log::error!(
            "Not writing {}: it exists but could not be read, and overwriting it              would replace the settings that are still in it",
            path.display()
        );
        return false;
    }
    fs::write(&path, render_conf(config)).is_ok()
}

/// The whole config file as text, without touching the disk.
///
/// Split off from the write so what lands in the file can be asserted on. It
/// needed to be: slot 0's memory permission was written under the OLD shared key
/// name while slot 1 used its own, and since the shared name seeds BOTH slots on
/// load, the two could never differ - grant it on one radio and the other had it
/// too, for ever. A test over this string is what catches that (2026-08-20).
fn render_conf(config: &ServerConfig) -> String {
    let mut contents = format!(
        "tci={}\nthetis_path={}\nyaesu_port={}\nyaesu_enabled={}\nyaesu_baud={}\nyaesu_ssb_switch_on_ptt={}\nyaesu_memory_write_ack={}\nyaesu_audio={}\nyaesu_audio_out={}\namplitec_port={}\namplitec_enabled={}\namplitec_window={}\ntuner_window={}\nspe_port={}\nspe_enabled={}\nspe_window={}\nrf2k_addr={}\nrf2k_enabled={}\nrf2k_window={}\nchat_window={}\nultrabeam_port={}\nultrabeam_enabled={}\nultrabeam_window={}\nrotor_addr={}\nrotor_enabled={}\nrotor_window={}\n",
        config.tci_addr.as_deref().unwrap_or(""),
        config.thetis_path.as_deref().unwrap_or(""),
        config.yaesu_port.as_deref().unwrap_or(""),
        config.yaesu_enabled,
        config.yaesu_baud,
        config.yaesu_ssb_switch_on_ptt,
        config.yaesu_memory_write_ack,
        config.yaesu_audio_device.as_deref().unwrap_or(""),
        config.yaesu_audio_output_device.as_deref().unwrap_or(""),
        config.amplitec_port.as_deref().unwrap_or(""),
        config.amplitec_enabled,
        config.show_amplitec_window,
        config.show_tuner_window,
        config.spe_port.as_deref().unwrap_or(""),
        config.spe_enabled,
        config.show_spe_window,
        config.rf2k_addr.as_deref().unwrap_or(""),
        config.rf2k_enabled,
        config.show_rf2k_window,
        config.show_chat_window,
        config.ultrabeam_port.as_deref().unwrap_or(""),
        config.ultrabeam_enabled,
        config.show_ultrabeam_window,
        config.rotor_addr.as_deref().unwrap_or(""),
        config.rotor_enabled,
        config.show_rotor_window,
    );
    // Dual-radio slot 1 (PATCH-dual-radio-991a-ftx1). Blocked separately so the
    // existing config-format stays unchanged on upgrade (old files
    // without these keys simply load the defaults: slot 1 disabled).
    contents.push_str(&format!("rx2_present={}\n", config.rx2_present));
    contents.push_str(&format!("yaesu2_port={}\n", config.yaesu2_port.as_deref().unwrap_or("")));
    contents.push_str(&format!("yaesu2_enabled={}\n", config.yaesu2_enabled));
    contents.push_str(&format!("yaesu2_baud={}\n", config.yaesu2_baud));
    contents.push_str(&format!("yaesu2_audio={}\n", config.yaesu2_audio_device.as_deref().unwrap_or("")));
    contents.push_str(&format!("yaesu2_audio_out={}\n", config.yaesu2_audio_output_device.as_deref().unwrap_or("")));
    if let Some(m) = config.yaesu_model_last {
        contents.push_str(&format!("yaesu_model_last={}\n", m));
    }
    if let Some(m) = config.yaesu2_model_last {
        contents.push_str(&format!("yaesu2_model_last={}\n", m));
    }
    contents.push_str(&format!("yaesu_audio_channel={}\n", config.yaesu_audio_channel));
    contents.push_str(&format!("chat_answers_seen={}
", config.chat_answers_seen));
    contents.push_str(&format!("yaesu2_audio_channel={}\n", config.yaesu2_audio_channel));
    contents.push_str(&format!("yaesu2_ssb_switch_on_ptt={}\n", config.yaesu2_ssb_switch_on_ptt));
    contents.push_str(&format!("yaesu2_memory_write_ack={}\n", config.yaesu2_memory_write_ack));
    // Rotor backend choice + PstRotator-fields. Blocked separately so the
    // existing EA7HG-config-format stays unchanged on upgrade.
    contents.push_str(&format!("rotor_backend={}\n", config.rotor_backend));
    contents.push_str(&format!("pstrotator_host={}\n", config.pstrotator_host));
    contents.push_str(&format!("pstrotator_port={}\n", config.pstrotator_port));
    contents.push_str(&format!(
        "pstrotator_feedback_port={}\n",
        config.pstrotator_feedback_port
    ));
    contents.push_str(&format!(
        "pstrotator_has_elevation={}\n",
        config.pstrotator_has_elevation
    ));
    contents.push_str(&format!(
        "pstrotator_listen_enabled={}\n",
        config.pstrotator_listen_enabled
    ));
    contents.push_str(&format!(
        "pstrotator_listen_port={}\n",
        config.pstrotator_listen_port
    ));
    // Per-tuner slots - tuner1_* / tuner2_* keys keep the file readable by hand.
    for (i, t) in config.tuners.iter().enumerate() {
        let prefix = format!("tuner{}", i + 1);
        contents.push_str(&format!("{}_enabled={}\n", prefix, t.enabled));
        contents.push_str(&format!("{}_model={}\n", prefix, t.model.as_str()));
        contents.push_str(&format!("{}_mcp_serial={}\n", prefix, t.mcp_serial));
        contents.push_str(&format!(
            "{}_amplitec_pos={}\n",
            prefix,
            t.amplitec_pos.map(|n| n.to_string()).unwrap_or_default()
        ));
        contents.push_str(&format!("{}_threshold_v={}\n", prefix, t.threshold_v));
        contents.push_str(&format!("{}_hysteresis_v={}\n", prefix, t.hysteresis_v));
    }
    // Per-rotor slots (PATCH-yaesu-rotor-mcp2221 phase 1) - rotor1_* / rotor2_*
    // keys. Phase 1 writes only enabled / name / mcp_serial; later phases
    // extend it with calibration + speed-fields.
    for (i, r) in config.rotors.iter().enumerate() {
        let prefix = format!("rotor{}", i + 1);
        contents.push_str(&format!("{}_enabled={}\n", prefix, r.enabled));
        contents.push_str(&format!("{}_name={}\n", prefix, r.name));
        contents.push_str(&format!("{}_mcp_serial={}\n", prefix, r.mcp_serial));
        contents.push_str(&format!("{}_v_at_0deg={:.3}\n", prefix, r.v_at_0deg));
        contents.push_str(&format!("{}_v_at_max_deg={:.3}\n", prefix, r.v_at_max_deg));
        contents.push_str(&format!("{}_max_deg={}\n", prefix, r.max_deg));
        contents.push_str(&format!("{}_ramp_pct_per_sec={:.1}\n", prefix, r.ramp_pct_per_sec));
        contents.push_str(&format!(
            "{}_shortest_route_in_overlap={}\n",
            prefix, r.shortest_route_in_overlap
        ));
    }
    for i in 0..6 {
        contents.push_str(&format!("amplitec_label{}={}\n", i + 1, config.amplitec_labels[i]));
    }
    for i in 0..6 {
        contents.push_str(&format!(
            "amplitec_max_w_a{}={}\n",
            i + 1,
            config.amplitec_max_w[i].map(|w| w.to_string()).unwrap_or_default()
        ));
    }
    for i in 0..6 {
        contents.push_str(&format!(
            "amplitec_tx_blocked_a{}={}\n",
            i + 1,
            config.amplitec_tx_blocked[i]
        ));
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
    if let Some(pos) = config.chat_window_pos {
        contents.push_str(&format!("chat_pos_x={}\nchat_pos_y={}\n", pos[0], pos[1]));
    }
    if let Some(pos) = config.ultrabeam_window_pos {
        contents.push_str(&format!("ultrabeam_pos_x={}\nultrabeam_pos_y={}\n", pos[0], pos[1]));
    }
    if let Some(pos) = config.rotor_window_pos {
        contents.push_str(&format!("rotor_pos_x={}\nrotor_pos_y={}\n", pos[0], pos[1]));
    }
    // Arranger: the painted matrices, the stored arrangements and the UI scale.
    if !config.layout_grids.is_empty() {
        contents.push_str(&format!("layout_grids={}\n", config.layout_grids));
    }
    for (i, mem) in config.layout_memories.iter().enumerate() {
        if !mem.is_empty() {
            contents.push_str(&format!("layout_mem{}={}\n", i + 1, mem));
        }
    }
    contents.push_str(&format!("ui_zoom={:.2}\n", config.ui_zoom));
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
    if let Some(sz) = config.chat_window_size {
        contents.push_str(&format!("chat_size_w={}\nchat_size_h={}\n", sz[0], sz[1]));
    }
    if let Some(sz) = config.ultrabeam_window_size {
        contents.push_str(&format!("ultrabeam_size_w={}\nultrabeam_size_h={}\n", sz[0], sz[1]));
    }
    if let Some(sz) = config.rotor_window_size {
        contents.push_str(&format!("rotor_size_w={}\nrotor_size_h={}\n", sz[0], sz[1]));
    }
    contents.push_str(&format!("language={}\n", config.language));
    contents.push_str(&format!("theme={}\n", config.theme));
    if !config.theme_custom.is_empty() {
        contents.push_str(&format!("theme_custom={}\n", config.theme_custom));
    }
    contents.push_str(&format!("autostart={}\n", config.autostart));
    contents.push_str(&format!("active_pa={}\n", config.active_pa));
    if let Some(v) = config.rf2k_saved_drive {
        contents.push_str(&format!("rf2k_saved_drive={}\n", v));
    }
    if let Some(v) = config.spe_saved_drive {
        contents.push_str(&format!("spe_saved_drive={}\n", v));
    }
    contents.push_str(&format!("ultrabeam_show_menu={}\n", config.ultrabeam_show_menu));
    contents.push_str(&format!("mcp2221_section_expanded={}\n", config.mcp2221_section_expanded));
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
    if let Some(ref name) = config.friendly_name {
        contents.push_str(&format!("friendly_name={}\n", name));
    }
    contents.push_str(&format!("relay_enabled={}\n", config.relay_enabled));
    contents.push_str(&format!("relay_udp_enabled={}\n", config.relay_udp_enabled));
    if !config.relay_url.is_empty() {
        contents.push_str(&format!("relay_url={}\n", config.relay_url));
    }
    if !config.relay_station.is_empty() {
        contents.push_str(&format!("relay_station={}\n", config.relay_station));
    }
    if !config.relay_token.is_empty() {
        contents.push_str(&format!("relay_token={}\n", sdr_remote_core::auth::obfuscate_password(&config.relay_token)));
    }
    sdr_remote_core::conf_layout::group(&contents, SECTIONS, UNSORTED)
}

/// The trailing heading for keys this table has no home for. A key landing
/// here is not an error - it is the reminder that it needs one.
const UNSORTED: &str = "Not sorted yet";

/// How the server's settings file reads. See `conf_layout`: the longest
/// matching prefix wins, so `amplitec_pos_x` files under the window headings
/// while `amplitec_port` stays with its amplifier.
///
/// Window geometry is deliberately NOT filed with its own device. Coordinates
/// are only worth reading against each other - a window that ended up on
/// another monitor stands out in a column of positions and disappears
/// entirely when it sits under its own heading.
const SECTIONS: &[sdr_remote_core::conf_layout::Section] = &[
    ("Connection and security", &[
        "tci", "password", "totp_", "friendly_name", "relay_", "thetis_path", "autostart",
    ]),
    ("Receivers", &["rx2_present"]),
    // Each radio's settings under its own slot, including the three that only
    // MEAN anything on one model: the value is a choice about that radio, and
    // two radios of the same type can be attached.
    ("Yaesu radio 1", &["yaesu_", "ftx1_"]),
    ("Yaesu radio 2", &["yaesu2_"]),
    ("Amplifiers", &["active_pa", "amplitec_", "spe_", "rf2k_", "ultrabeam_"]),
    ("Antenna tuners", &["tuner1_", "tuner2_"]),
    ("Rotor", &["rotor_", "rotor1_", "pstrotator_", "mcp2221_"]),
    ("DX cluster", &["dxcluster_"]),
    ("Window positions and sizes", &[
        "main_pos", "main_size", "tuner_pos", "tuner_size", "amplitec_pos", "amplitec_size",
        "spe_pos", "spe_size", "rf2k_pos", "rf2k_size", "chat_pos", "chat_size",
        "ultrabeam_pos", "ultrabeam_size", "rotor_pos", "rotor_size",
    ]),
    ("Window visibility", &[
        "tuner_window", "amplitec_window", "spe_window", "rf2k_window", "chat_window",
        "ultrabeam_window", "rotor_window", "ultrabeam_show_menu", "mcp2221_section_expanded",
    ]),
    ("Appearance", &["theme", "language", "ui_zoom", "layout_grids"]),
];

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

#[cfg(test)]
mod per_slot_conf_tests {
    use super::{classify_read, config_from_source, parse_conf, render_conf, ConfigSource, ServerConfig};

    /// Each slot writes its OWN key. Slot 0's memory permission went out under
    /// the old shared name `ftx1_memory_write_ack`, and that name seeds BOTH
    /// slots when the file is read back - so granting it for one radio granted
    /// it for the other, permanently, and the two settings could never differ.
    /// The operator spotted it as "both radios always show the same ticks".

    /// What a 2.9.1 config carried for these three settings. Deliberately the
    /// old spellings, including the one that lies: `yaesu2_audio_channel` was
    /// the SHARED channel there, not slot 2's.
    const OLD_CONF: &str = "yaesu_port=COM13
yaesu_enabled=true
yaesu_ssb_switch_on_ptt=false
ftx1_memory_write_ack=true
yaesu2_audio_channel=1
";

    /// An upgrading config must arrive with the operator's own answers on BOTH
    /// slots. The three keys were one setting each until 2.9.1, so the honest
    /// reading of an old file is "this applied to whatever radio was there".

    /// An absent file and an unreadable one look the same to a caller that only
    /// checks `is_err()`, and they are opposites: the first has nothing to
    /// preserve, the second still holds everything the operator set up. The
    /// load path used to treat both as a first run, and because `modify_config`
    /// is load-mutate-save over the whole file, one failed read during normal
    /// use would have written a bare config over a full one.

    /// The half that actually broke. Classifying the read correctly is not the
    /// point: the damage came from what the classification was mapped ONTO. Put
    /// the unreadable arm back on `(first_run(), true)` and this test fails,
    /// where a test on the classifier alone stays green.
    #[test]
    fn an_unreadable_file_yields_no_write_permission_and_no_bare_config() {
        let (config, may_write) = config_from_source(ConfigSource::Unreadable, None);
        assert!(!may_write, "an unreadable file must never be written back");
        // Not the first-run values: those are what would land in the file.
        let bare = ServerConfig::first_run();
        let plain = ServerConfig::default();
        assert_eq!(config.rx2_present, plain.rx2_present);
        assert_eq!(config.dxcluster_enabled, plain.dxcluster_enabled);
        assert!(
            config.rx2_present != bare.rx2_present
                || config.dxcluster_enabled != bare.dxcluster_enabled,
            "the unreadable case handed out the bare first-run config"
        );

        // And the two cases that MAY write really may.
        let (first, may_write) = config_from_source(ConfigSource::FirstRun, None);
        assert!(may_write);
        assert_eq!(first.rx2_present, bare.rx2_present);
        let (parsed, may_write) = config_from_source(ConfigSource::File, Some("yaesu_port=COM7
"));
        assert!(may_write);
        assert_eq!(parsed.yaesu_port.as_deref(), Some("COM7"));
    }

    #[test]
    fn an_unreadable_file_is_not_the_same_as_no_file() {
        use std::io::{Error, ErrorKind};
        assert_eq!(
            classify_read(&Ok("tci=127.0.0.1:40001
".to_string())),
            ConfigSource::File
        );
        assert_eq!(
            classify_read(&Err(Error::from(ErrorKind::NotFound))),
            ConfigSource::FirstRun
        );
        // Everything else is a file that IS there. Permission, a lock held by
        // another process, a failing disk - none of them mean "nothing has been
        // configured here".
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::Other,
            ErrorKind::InvalidData,
        ] {
            assert_eq!(
                classify_read(&Err(Error::from(kind))),
                ConfigSource::Unreadable,
                "{kind:?} was taken for a first run"
            );
        }
    }

    /// The old shared key really does seed BOTH slots. The two assertions that
    /// only check the old names are gone from the written file cannot fail by
    /// construction; this is the path that is the actual reason the migration
    /// branch exists.
    #[test]
    fn the_old_shared_channel_key_seeds_both_slots() {
        let c = parse_conf("ftx1_audio_channel=0
");
        assert_eq!(c.yaesu_audio_channel, 0);
        assert_eq!(c.yaesu2_audio_channel, 0);
        // And a slot with a key of its own is not overruled by it.
        let c = parse_conf("ftx1_audio_channel=0
yaesu2_audio_channel=1
");
        assert_eq!(c.yaesu_audio_channel, 0, "slot 1 should still take the shared key");
        assert_eq!(c.yaesu2_audio_channel, 1, "slot 2's own key must win");
    }

    #[test]
    fn an_old_config_seeds_both_slots() {
        let c = parse_conf(OLD_CONF);
        assert!(!c.yaesu_ssb_switch_on_ptt, "slot 1 lost the SSB choice");
        assert!(!c.yaesu2_ssb_switch_on_ptt, "slot 2 lost the SSB choice");
        assert!(c.yaesu_memory_write_ack, "slot 1 lost the memory permission");
        assert!(c.yaesu2_memory_write_ack, "slot 2 lost the memory permission");
        // The one that would have been silently dropped: an FTX-1 in slot 1
        // whose operator picked the left channel would have got mix back.
        assert_eq!(c.yaesu_audio_channel, 1, "slot 1 lost the audio channel");
        assert_eq!(c.yaesu2_audio_channel, 1, "slot 2 lost the audio channel");
        // And nothing else in the file was disturbed.
        assert_eq!(c.yaesu_port.as_deref(), Some("COM13"));
    }

    /// A file this build wrote carries both channel keys, and then slot 2's key
    /// is slot 2's own - it must NOT be taken as the old shared value and
    /// copied over slot 1.
    #[test]
    fn a_current_config_keeps_the_two_slots_apart() {
        let mut c = ServerConfig::default();
        c.yaesu_audio_channel = 0;
        c.yaesu2_audio_channel = 1;
        c.yaesu_memory_write_ack = true;
        c.yaesu2_memory_write_ack = false;
        c.yaesu_ssb_switch_on_ptt = true;
        c.yaesu2_ssb_switch_on_ptt = false;
        let back = parse_conf(&render_conf(&c));
        assert_eq!(back.yaesu_audio_channel, 0);
        assert_eq!(back.yaesu2_audio_channel, 1);
        assert!(back.yaesu_memory_write_ack);
        assert!(!back.yaesu2_memory_write_ack);
        assert!(back.yaesu_ssb_switch_on_ptt);
        assert!(!back.yaesu2_ssb_switch_on_ptt);
    }

    /// Reading an old file and writing it back must not leave the old names
    /// behind: that is what kept both slots tied together for a release.
    #[test]
    fn an_old_config_is_written_back_in_the_new_shape() {
        let text = render_conf(&parse_conf(OLD_CONF));
        for legacy in ["ftx1_memory_write_ack", "ftx1_audio_channel"] {
            assert!(!text.contains(legacy), "the old key {legacy} is still written");
        }
        assert!(text.contains("yaesu_audio_channel=1"));
        assert!(text.contains("yaesu2_audio_channel=1"));
    }

    #[test]
    fn the_two_slots_write_their_own_keys() {
        let mut c = ServerConfig::default();
        c.yaesu_memory_write_ack = true;
        c.yaesu2_memory_write_ack = false;
        c.yaesu_ssb_switch_on_ptt = true;
        c.yaesu2_ssb_switch_on_ptt = false;
        c.yaesu_audio_channel = 0;
        c.yaesu2_audio_channel = 2;
        let text = render_conf(&c);

        for line in [
            "yaesu_memory_write_ack=true",
            "yaesu2_memory_write_ack=false",
            "yaesu_ssb_switch_on_ptt=true",
            "yaesu2_ssb_switch_on_ptt=false",
            "yaesu_audio_channel=0",
            "yaesu2_audio_channel=2",
        ] {
            assert!(text.contains(line), "missing from the config file: {line}");
        }

        // The shared names are read for migration and must never be WRITTEN
        // again - writing one is what made the two slots inseparable.
        for legacy in ["ftx1_memory_write_ack", "ftx1_audio_channel"] {
            assert!(
                !text.contains(legacy),
                "the old shared key {legacy} is still being written"
            );
        }
    }

    /// And what is written comes back the same way round. A slot-0-only value
    /// must not arrive on slot 1.
    #[test]
    fn a_setting_on_one_slot_stays_on_that_slot() {
        let mut c = ServerConfig::default();
        c.yaesu_memory_write_ack = true;
        c.yaesu2_memory_write_ack = false;
        let text = render_conf(&c);
        let line_of = |key: &str| {
            text.lines()
                .find(|l| l.starts_with(&format!("{key}=")))
                .unwrap_or_else(|| panic!("{key} not in the file"))
                .to_string()
        };
        assert_eq!(line_of("yaesu_memory_write_ack"), "yaesu_memory_write_ack=true");
        assert_eq!(line_of("yaesu2_memory_write_ack"), "yaesu2_memory_write_ack=false");
    }
}

#[cfg(test)]
mod first_run_tests {
    use super::ServerConfig;

    /// A first start is a bare server: a client can connect, and nothing else
    /// is switched on. Every field here is a checkbox in the settings screen
    /// that has to be clear before the operator has touched anything.
    #[test]
    fn nothing_optional_is_on_at_a_first_start() {
        let c = ServerConfig::first_run();
        assert!(!c.rx2_present, "second receiver");
        assert!(!c.amplitec_enabled, "Amplitec");
        assert!(!c.spe_enabled, "SPE Expert");
        assert!(!c.rf2k_enabled, "RF2K-S");
        assert!(!c.ultrabeam_enabled, "UltraBeam");
        assert!(!c.rotor_enabled, "rotor");
        assert!(!c.dxcluster_enabled, "DX cluster");
        assert!(!c.relay_enabled, "relay");
        assert!(!c.relay_udp_enabled, "audio over UDP");
        assert!(!c.yaesu_enabled, "radio 1");
        assert!(!c.yaesu2_enabled, "radio 2");
        assert!(!c.autostart, "autostart");
        for (what, open) in [
            ("Amplitec", c.show_amplitec_window),
            ("tuner", c.show_tuner_window),
            ("SPE", c.show_spe_window),
            ("RF2K-S", c.show_rf2k_window),
            ("UltraBeam", c.show_ultrabeam_window),
            ("rotor", c.show_rotor_window),
            ("chat", c.show_chat_window),
        ] {
            assert!(!open, "{what} window opens at start");
        }
        assert!(c.tuners.is_empty());
        assert!(c.rotors.is_empty());
    }

    /// And the other default must NOT follow it there. It fills in keys an
    /// existing file happens to lack, so turning a device off in it would
    /// switch that device off underneath an operator already using it.
    #[test]
    fn an_existing_file_missing_a_key_keeps_its_device() {
        let d = ServerConfig::default();
        assert!(d.rx2_present);
        assert!(d.amplitec_enabled);
        assert!(d.spe_enabled);
        assert!(d.rf2k_enabled);
        assert!(d.ultrabeam_enabled);
        assert!(d.rotor_enabled);
        assert!(d.dxcluster_enabled);
    }

    /// Whatever the OS answered, the first-run language is one the GUI can set.
    #[test]
    fn the_first_run_language_is_one_we_ship() {
        let lang = ServerConfig::first_run().language;
        assert!(
            sdr_remote_core::oslang::UI_LANGUAGES.contains(&lang.as_str()),
            "first-run language {lang} is not translated"
        );
    }
}

#[cfg(test)]
mod conf_section_tests {
    use super::{SECTIONS, UNSORTED};

    fn heading_of(key: &str) -> String {
        let grouped = sdr_remote_core::conf_layout::group(&format!("{key}=x\n"), SECTIONS, UNSORTED);
        grouped
            .lines()
            .next()
            .unwrap_or_default()
            .trim_matches(|c| c == '#' || c == ' ' || c == '-')
            .to_string()
    }

    /// Where a window is, versus what the device is set to. Both start with
    /// the device's name, and only the longer prefix tells them apart.
    #[test]
    fn a_window_coordinate_is_not_a_device_setting() {
        assert_eq!(heading_of("amplitec_pos_x"), "Window positions and sizes");
        assert_eq!(heading_of("amplitec_size_w"), "Window positions and sizes");
        assert_eq!(heading_of("amplitec_port"), "Amplifiers");
        assert_eq!(heading_of("amplitec_window"), "Window visibility");
    }

    /// A tuner's position in the amplifier chain is not a window position,
    /// however much the key looks like one.
    #[test]
    fn a_tuners_place_in_the_chain_is_not_a_screen_position() {
        assert_eq!(heading_of("tuner1_amplitec_pos"), "Antenna tuners");
        assert_eq!(heading_of("tuner_pos_x"), "Window positions and sizes");
    }

    #[test]
    fn the_rotor_keeps_its_own_and_its_backend() {
        assert_eq!(heading_of("rotor1_max_deg"), "Rotor");
        assert_eq!(heading_of("pstrotator_host"), "Rotor");
        assert_eq!(heading_of("rotor_pos_y"), "Window positions and sizes");
    }

    #[test]
    fn the_two_radios_do_not_share_a_heading() {
        assert_eq!(heading_of("yaesu_port"), "Yaesu radio 1");
        assert_eq!(heading_of("yaesu2_port"), "Yaesu radio 2");
    }

    /// The three settings that only mean something on one model are still per
    /// SLOT, and file under their slot: an operator looking for the FTX-1's
    /// audio channel with that radio in slot 2 must find it under radio 2.
    #[test]
    fn a_model_specific_setting_still_files_under_its_own_slot() {
        for key in ["audio_channel", "memory_write_ack", "ssb_switch_on_ptt"] {
            assert_eq!(heading_of(&format!("yaesu_{key}")), "Yaesu radio 1");
            assert_eq!(heading_of(&format!("yaesu2_{key}")), "Yaesu radio 2");
        }
    }
}
