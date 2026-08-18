// SPDX-License-Identifier: GPL-2.0-or-later

/// Sentinel for "no S-meter sample yet" — well below S0 (-127 dBm) so the
/// widget clamps the needle to the bottom of the arc without ever looking
/// like a real reading.
pub const SMETER_NO_DATA_DBM: f32 = -200.0;

/// A DX cluster spot for client-side display.
#[derive(Clone, Debug)]
pub struct DxSpotInfo {
    pub callsign: String,
    pub frequency_hz: u64,
    pub mode: String,
    pub spotter: String,
    pub comment: String,
    pub age_seconds: u16,
    /// Total spot lifetime in seconds (from server config)
    pub expiry_seconds: u16,
    /// When this spot was last received (for client-side expiry)
    pub received: std::time::Instant,
}

/// RGBA color for a DX spot mode string.
pub fn mode_color_rgba(mode: &str, alpha: u8) -> [u8; 4] {
    match mode {
        "CW" => [255, 255, 0, alpha],       // yellow
        "SSB" => [0, 255, 0, alpha],        // green
        "FT8" | "FT4" | "DIGI" => [0, 255, 255, alpha], // cyan
        _ => [255, 255, 255, alpha],        // white
    }
}

/// High-level connect-status that the engine reports to the UI.
///
/// Distinguishes intermediate states (no error) from terminal errors. The UI
/// renders different controls per variant: `AwaitingTotp` shows the 2FA input,
/// `Failed(err)` shows the error message via the i18n helper.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectStatus {
    /// Not currently attempting connect (initial / idle / after disconnect).
    Disconnected,
    /// Connection attempt in progress (sent first heartbeat, awaiting auth challenge or response).
    Connecting,
    /// Server has sent `AUTH_TOTP_REQUIRED`; client is awaiting user TOTP code.
    /// This is NOT an error — it is an expected step in the 2FA flow.
    AwaitingTotp,
    /// Fully connected and authenticated; receiving heartbeat acks and state pushes.
    Connected,
    /// Connect attempt failed. See `ConnectError` for the specific cause.
    Failed(ConnectError),
}

/// Specific reason a connect attempt failed.
///
/// All variants are `Clone` so the enum can live inside `RadioState`
/// (which is broadcast via `tokio::sync::watch` and must be clone-able).
/// `std::io::Error` is NOT stored directly because it is not `Clone`;
/// instead we keep the `io::ErrorKind` (which is `Copy`) plus a stringified
/// `Display` of the original error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectError {
    /// DNS resolution failed for the server hostname.
    DnsResolutionFailed {
        host: String,
        io_kind: std::io::ErrorKind,
        message: String,
    },
    /// No UDP response on the ThetisLink server port within the timeout window.
    /// Causes: server not running, firewall blocking UDP, wrong IP/port.
    NoUdpResponse {
        addr: String,
        timeout_secs: u32,
    },
    /// The remote endpoint replied but the bytes are not a valid ThetisLink
    /// protocol frame (something else listens on the port, or the handshake
    /// is mangled).
    MalformedResponse {
        addr: String,
        detail: String,
    },
    /// Server rejected the password before the TOTP step. Distinguished from
    /// `WrongTotp` by which phase the auth-state-machine was in when the
    /// reject arrived.
    WrongPassword,
    /// Server rejected the TOTP code after a successful password step.
    WrongTotp,
    /// Server protocol VERSION does not match client VERSION. The UI
    /// chooses the wording (client-too-old vs server-too-old) based on
    /// whether `server_version > client_version` or vice versa.
    ProtocolVersionMismatch {
        server_version: u8,
        client_version: u8,
    },
    /// Client is connected and authenticated, but the server itself cannot
    /// reach Thetis (the TCI WebSocket to Thetis is down). Primarily driven
    /// by an explicit server-side hint in the state-push; audio absence
    /// alone is NOT used because it would false-positive on a quiet band.
    TciUnreachable {
        server_addr: String,
        server_reported_detail: Option<String>,
        /// Server-side process-state hint: is Thetis.exe currently running on
        /// the server PC? `None` for old servers that don't advertise this
        /// (THETIS_RUNNING bit gated by REPORTS_STATE_FLAGS capability).
        /// `Some(true)` → "Thetis runs but TCI is down, check TCI settings".
        /// `Some(false)` → "Thetis is not running, press Start on Thetis tab".
        thetis_process_running: Option<bool>,
    },
    /// Any other failure (unhandled I/O error, etc.). Carries a Display
    /// message for the log; UI shows a generic "connection failed" plus
    /// the detail in small text.
    Other {
        message: String,
    },
}

impl ConnectStatus {
    /// True only when the server explicitly reports that Thetis.exe is not
    /// running on the server PC. An old server that advertises no process
    /// state (`None`) reads as false - "we don't know" must never be treated
    /// as "not running", because clients act on this by launching Thetis.
    ///
    /// Shared by the desktop client and the Android bridge so both platforms
    /// gate their Thetis-autostart on exactly the same condition.
    pub fn thetis_reported_not_running(&self) -> bool {
        matches!(
            self,
            ConnectStatus::Failed(ConnectError::TciUnreachable {
                thetis_process_running: Some(false),
                ..
            })
        )
    }
}

/// Read-only radio state, broadcast from engine to UI via tokio::sync::watch.
#[derive(Clone, Debug)]
pub struct RadioState {
    // Connection
    pub connected: bool,
    pub ptt_denied: bool,
    pub audio_error: bool,
    /// High-level connect-status with optional error detail. Replaces the
    /// older `auth_rejected` / `totp_required` booleans (which are kept
    /// for backwards-compatibility during migration but should not be
    /// used in new UI code).
    pub connect_status: ConnectStatus,

    // Stats
    pub rtt_ms: u16,
    pub jitter_ms: f32,
    pub buffer_depth: u32,
    pub rx_packets: u64,
    pub yaesu_audio_packets: u64,
    pub yaesu_jitter_ms: f32,
    pub yaesu_buffer_depth: u32,
    pub yaesu2_audio_packets: u64,
    pub yaesu2_jitter_ms: f32,
    pub yaesu2_buffer_depth: u32,
    pub vrx1_audio_packets: u64,
    pub vrx1_jitter_ms: f32,
    pub vrx1_buffer_depth: u32,
    pub vrx2_audio_packets: u64,
    pub vrx2_jitter_ms: f32,
    pub vrx2_buffer_depth: u32,
    pub loss_percent: u8,
    /// Incoming UDP bandwidth over the last ~500 ms window (Kbit/s).
    /// Counts all bytes received on the socket; no breakdown per stream-type.
    pub down_kbps: u32,
    /// Outgoing UDP bandwidth over the last ~500 ms window (Kbit/s).
    /// Counts all bytes sent (audio TX + control + heartbeat).
    pub up_kbps: u32,
    /// Per-`PacketType` byte-counter — kbps over the last ~5 s window,
    /// sorted by decreasing value. Filled by the engine, read
    /// by the Server-tab UI as expand-detail under Down. Empty vec =
    /// no data available yet.
    pub bw_breakdown: Vec<(u8, u32)>,

    // Audio levels
    pub capture_level: f32,
    pub playback_level: f32,
    pub playback_level_bin_r: f32,
    pub playback_level_rx2: f32,
    pub playback_level_yaesu: f32,
    pub playback_level_yaesu2: f32,
    pub playback_level_vrx1: f32,
    pub playback_level_vrx2: f32,
    pub yaesu_mic_level: f32,

    // Radio
    pub frequency_hz: u64,
    pub mode: u8,
    /// RX1 S-meter — dBm during RX, watts during TX (disambiguated by
    /// `other_tx` plus client-local PTT). `SMETER_NO_DATA_DBM` (-200.0) when
    /// no sample has arrived yet, which the widget clamps to S0.
    pub smeter: f32,
    /// RX1 S-meter Sig source (peak-hold WDSP RXA_S_PK), dBm. Only populated
    /// when the client has subscribed via SmeterSources bit 0; otherwise
    /// stays at the no-data sentinel.
    pub smeter_sig: f32,
    /// RX1 S-meter MaxBin source (single highest FFT bin), dBm. SmeterSources bit 2.
    pub smeter_peakbin: f32,

    // Thetis controls
    pub power_on: bool,
    pub tx_profile: u8,
    pub nr_level: u8,
    pub anf_on: bool,
    pub drive_level: u8,
    pub rx_af_gain: u8,
    pub agc_enabled: bool,
    pub other_tx: bool,
    pub recording: bool,
    pub playing: bool,
    /// The recordings the last Start made, as (source name, file path).
    ///
    /// A list and not one path. Recording two sources at once wrote two files
    /// correctly and then offered a Play button bound to whichever was created
    /// last, so an operator recording both radios could only ever hear the
    /// second one and reasonably concluded the first had not been recorded
    /// (2026-08-13). The file was there the whole time; there was no way to
    /// ask for it.
    pub last_recorded: Vec<(String, String)>,
    pub thetis_swr_x100: u16,
    pub filter_low_hz: i32,
    pub filter_high_hz: i32,
    /// True unless a modern server explicitly reports that Thetis/TCI is disabled.
    pub thetis_configured: bool,
    /// True unless a modern server explicitly reports the radio has a single
    /// receiver (SINGLE_RECEIVER flag). Default true = normal 2-RX radio, so
    /// old servers and the default config keep RX2/VRX2 visible.
    pub rx2_present: bool,
    pub thetis_starting: bool,
    pub mon_on: bool,
    pub tx_profile_names: Vec<String>,

    // DX-cluster spot stream — toggle for metered-link data-saving
    pub dx_spots_enabled: bool,
    /// Goes up by one on every successful authentication.
    ///
    /// A COUNTER and not a flag, because the thing it replaces was a flank:
    /// "connected went false" only exists at an instant, it reaches the UI
    /// through a watch channel that keeps just the latest value, and the UI
    /// reads that once a frame. A short interruption therefore never happened
    /// as far as the UI was concerned - and the work that hangs off it, the
    /// VRX and Yaesu re-subscriptions, was simply skipped. A number that
    /// differs from the one you last acted on cannot be missed however slowly
    /// you look (2026-08-16).
    pub session_generation: u64,
    /// What the SERVER says this client is subscribed to, from the heartbeat.
    ///
    /// Not a wish and not a command: the other side's answer, put here so both
    /// the engine and the UI can hold it against their own. `None` against a
    /// server that predates the field - which is not the same as "nothing".
    pub server_subs: Option<sdr_remote_core::protocol::SubscriptionMask>,
    /// Whether the server still sends the full-DDC spectrum row next to the
    /// extracted view. Off = the RX waterfall is built from the view alone,
    /// like VRX, at roughly half the spectrum bandwidth per receiver.
    pub full_spectrum_enabled: bool,

    /// RX1 audio subscription (default ON). False = client wants no RX1 audio
    /// (e.g. VRX-only use, saving bandwidth).
    pub rx1_enabled: bool,

    // RX2 / VFO-B
    pub rx2_enabled: bool,
    /// RX2 spectrum subscription, SEPARATE from `rx2_enabled` (audio). Phase 3b/4.
    pub rx2_spectrum_enabled: bool,
    pub vfo_sync: bool,
    pub frequency_rx2_hz: u64,
    pub mode_rx2: u8,
    /// RX2 S-meter, dBm (RX-only — RX2 never transmits).
    pub smeter_rx2: f32,
    /// RX2 Sig source (SmeterSources bit 4), dBm.
    pub smeter_rx2_sig: f32,
    /// RX2 MaxBin source (SmeterSources bit 6), dBm.
    pub smeter_rx2_peakbin: f32,
    pub rx2_af_gain: u8,
    pub rx2_nr_level: u8,
    pub rx2_anf_on: bool,
    pub rx2_agc_mode: u8,
    pub rx2_agc_gain: u8,
    pub rx2_sql_enable: bool,
    pub rx2_sql_level: u8,
    pub rx2_nb_enable: bool,
    pub rx2_binaural: bool,
    pub rx2_apf_enable: bool,
    pub rx2_vfo_lock: bool,
    pub filter_rx2_low_hz: i32,
    pub filter_rx2_high_hz: i32,

    // RX2 Spectrum
    pub rx2_spectrum_bins: Vec<u16>,
    pub rx2_spectrum_center_hz: u32,
    pub rx2_spectrum_span_hz: u32,
    pub rx2_spectrum_ref_level: i8,
    pub rx2_spectrum_db_per_unit: u8,
    pub rx2_spectrum_sequence: u16,
    pub rx2_full_spectrum_bins: Vec<u16>,
    pub rx2_full_spectrum_center_hz: u32,
    pub rx2_full_spectrum_span_hz: u32,
    pub rx2_full_spectrum_sequence: u16,

    // Spectrum (extracted view for spectrum line)
    pub spectrum_bins: Vec<u16>,
    pub spectrum_center_hz: u32,
    pub spectrum_span_hz: u32,
    pub spectrum_ref_level: i8,
    pub spectrum_db_per_unit: u8,
    pub spectrum_sequence: u16,

    // Full DDC spectrum (for waterfall history — covers entire DDC bandwidth)
    pub full_spectrum_bins: Vec<u16>,
    pub full_spectrum_center_hz: u32,
    pub full_spectrum_span_hz: u32,
    pub full_spectrum_sequence: u16,

    // High-res VRX extracted spectrum (server-side per-VRX window).
    // Populated only when client enables it via SetVrxHighResSpectrum.
    // Empty bins → fall back to full_spectrum on the client side.
    pub vrx1_extracted_bins: Vec<u16>,
    pub vrx1_extracted_center_hz: u32,
    pub vrx1_extracted_span_hz: u32,
    pub vrx1_extracted_sequence: u16,
    pub vrx2_extracted_bins: Vec<u16>,
    pub vrx2_extracted_center_hz: u32,
    pub vrx2_extracted_span_hz: u32,
    pub vrx2_extracted_sequence: u16,

    // SAM auto-tune follow frequency (PATCH-vrx-wide-sam-ux). Latest carrier
    // frequency the server is following while auto-tune is on; 0 = no update.
    // The UI applies it to the VRX VFO display when it changes.
    pub vrx1_autotune_freq_hz: u64,
    pub vrx2_autotune_freq_hz: u64,

    // TX modulation filter band (PATCH-tx-modulation-bandwidth). Pushed by the
    // server when Thetis reports it; `tx_filter_supported` gates the client UI.
    pub tx_filter_low_hz: i32,
    pub tx_filter_high_hz: i32,
    pub tx_filter_supported: bool,

    // External equipment: Amplitec 6/2 antenna switch
    pub amplitec_available: bool,  // true once any Amplitec device-status packet is received
    pub amplitec_connected: bool,
    pub amplitec_switch_a: u8,  // 0=unknown, 1-6
    pub amplitec_switch_b: u8,
    pub amplitec_labels: String,  // comma-separated: "a1,a2,...,a6,b1,...,b6" (empty = not received)
    /// Amplitec power-cap table per Amplitec-A position (index 0 = A-1).
    /// 0 = no cap. Server pushes on connect and on change.
    pub amplitec_power_max_w: [u16; 6],
    /// TX-block per position. `true` = RX-only.
    pub amplitec_power_tx_blocked: [bool; 6],
    /// True once the server has pushed an AmplitecPowerTable packet.
    /// Used by the client UI to defer initializing
    /// the edit-state until we have the real values.
    pub amplitec_power_loaded: bool,

    // JC-4s antenna tuner
    pub tuner_available: bool,    // true once any tuner device-status packet is received
    pub tuner_connected: bool,  // Hardware connected
    pub tuner_state: u8,        // 0=Idle, 1=Tuning, 2=DoneOk, 3=Timeout, 4=Aborted, 5=DoneAssumed
    pub tuner_can_tune: bool,   // Amplitec switch A on JC-4s position

    // SPE Expert 1.3K-FA power amplifier
    pub spe_connected: bool,
    pub spe_state: u8,        // 0=Off, 1=Standby, 2=Operate
    pub spe_band: u8,         // band code
    pub spe_ptt: bool,
    pub spe_power_w: u16,
    pub spe_swr_x10: u16,
    pub spe_temp: u8,
    pub spe_warning: u8,
    pub spe_alarm: u8,
    pub spe_power_level: u8,  // 0=L, 1=M, 2=H
    pub spe_antenna: u8,
    pub spe_input: u8,
    pub spe_voltage_x10: u16,
    pub spe_current_x10: u16,
    pub spe_atu_bypassed: bool,
    pub spe_available: bool,    // true once any SPE status packet received
    pub spe_active: bool,       // true if SPE is the active PA for drive control

    // RF2K-S power amplifier
    pub rf2k_connected: bool,
    pub rf2k_operate: bool,
    pub rf2k_band: u8,
    pub rf2k_frequency_khz: u16,
    pub rf2k_temperature_x10: u16,
    pub rf2k_voltage_x10: u16,
    pub rf2k_current_x10: u16,
    pub rf2k_forward_w: u16,
    pub rf2k_reflected_w: u16,
    pub rf2k_swr_x100: u16,
    pub rf2k_max_forward_w: u16,
    pub rf2k_max_reflected_w: u16,
    pub rf2k_max_swr_x100: u16,
    pub rf2k_error_state: u8,
    pub rf2k_error_text: String,
    pub rf2k_antenna_type: u8,
    pub rf2k_antenna_number: u8,
    pub rf2k_tuner_mode: u8,
    pub rf2k_tuner_setup: String,
    pub rf2k_tuner_l_nh: u16,
    pub rf2k_tuner_c_pf: u16,
    pub rf2k_tuner_freq_khz: u16,
    pub rf2k_segment_size_khz: u16,
    pub rf2k_drive_w: u16,
    pub rf2k_modulation: String,
    pub rf2k_max_power_w: u16,
    pub rf2k_device_name: String,
    pub rf2k_available: bool,
    pub rf2k_active: bool,      // true if RF2K is the active PA for drive control
    // RF2K-S debug (Fase D)
    pub rf2k_debug_available: bool,
    pub rf2k_bias_pct_x10: u16,
    pub rf2k_psu_source: u8,
    pub rf2k_uptime_s: u32,
    pub rf2k_tx_time_s: u32,
    pub rf2k_error_count: u16,
    pub rf2k_error_history: Vec<(String, String)>,
    pub rf2k_storage_bank: u16,
    pub rf2k_hw_revision: String,
    pub rf2k_frq_delay: u16,
    pub rf2k_autotune_threshold_x10: u16,
    pub rf2k_dac_alc: u16,
    pub rf2k_high_power: bool,
    pub rf2k_tuner_6m: bool,
    pub rf2k_band_gap_allowed: bool,
    pub rf2k_controller_version: u16,
    pub rf2k_drive_config_ssb: [u8; 11],
    pub rf2k_drive_config_am: [u8; 11],
    pub rf2k_drive_config_cont: [u8; 11],

    // UltraBeam RCU-06
    pub ub_connected: bool,
    pub ub_frequency_khz: u16,
    pub ub_band: u8,
    pub ub_direction: u8,
    pub ub_off_state: bool,
    pub ub_motors_moving: u8,
    pub ub_motor_completion: u16,
    pub ub_fw_major: u8,
    pub ub_fw_minor: u8,
    pub ub_available: bool,
    pub ub_elements_mm: [u16; 6],
    pub ub_operation: u8,
    pub ub_freq_min_mhz: u16,
    pub ub_freq_max_mhz: u16,
    // DX Cluster spots
    pub dx_spots: Vec<DxSpotInfo>,

    // EA7HG Visual Rotor
    pub rotor_connected: bool,
    pub rotor_angle_x10: u16,
    pub rotor_rotating: bool,
    pub rotor_target_x10: u16,
    pub rotor_available: bool,

    // Yaesu FT-991A
    pub yaesu_connected: bool,
    pub yaesu_freq_a: u64,
    pub yaesu_freq_b: u64,
    pub yaesu_mode: u8,
    pub yaesu_smeter: u16,
    pub yaesu_tx_active: bool,
    pub yaesu_power_on: bool,
    pub yaesu_af_gain: u8,
    pub yaesu_tx_power: u8,
    /// Max TX power (watts) for the current band (PATCH-yaesu-power-scaling).
    /// 0 = old server/unknown -> client falls back to 100 for the slider range.
    pub yaesu_tx_power_max: u8,
    pub yaesu_squelch: u8,
    pub yaesu_rf_gain: u8,
    pub yaesu_mic_gain: u8,
    pub yaesu_split: bool,
    pub yaesu_scan: bool,
    /// Internal ATU state (PATCH-yaesu-internal-atu): 0=off, 1=on, 2=tuning-in-progress.
    pub yaesu_tuner_state: u8,
    /// Radio reports a high SWR while transmitting. Self-clearing: the radio
    /// drops the flag as soon as the match is good again.
    pub yaesu_hi_swr: bool,
    /// Yaesu DSP/function toggles bitfield (PATCH-yaesu-extra-controls): bit N =
    /// `YaesuCtrl` N on/off. Fed by YaesuFeaturePacket. Fase A1: RfAtt(0), BreakIn(1).
    pub yaesu_feature_toggles: u32,
    /// Yaesu multi-state/level values, indexed by `YaesuCtrl`. Fase B: Agc(6), PreAmp(7).
    pub yaesu_feature_levels: [u8; 16],
    /// Fase D frequency values: [0]=Contour, [1]=APF, [2]=Notch.
    pub yaesu_feature_freqs: [u16; 4],
    pub yaesu_vfo_select: u8,  // 0=VFO, 1=Memory, 2=MemTune
    pub yaesu_memory_channel: u16,
    pub yaesu_memory_data: Option<String>,
    /// EX/menu values, SEPARATE from the memory list above. They used to share one
    /// field, distinguished by a "MENU:" prefix. That worked only as long as the
    /// two never arrived close together - since both are pushed on connect they do,
    /// and the second silently replaced the first before the UI ever saw it.
    pub yaesu_menu_data: Option<String>,
    /// The server's own problem-report attachment, once every numbered part has
    /// arrived. `None` while nothing has been asked for, or while a transfer is
    /// still running.
    pub server_report: Option<String>,
    /// How a transfer ended when it did not end with a report: the parts that
    /// arrived and the parts expected. Shown to the operator rather than
    /// swallowed - a report missing its middle must not be attachable.
    pub server_report_failed: Option<(u16, u16)>,
    // Dual-radio slot 1 (PATCH-dual-radio-991a-ftx1) — mirror of the slot-0
    // Yaesu fields above. `*_model` = wire-code from RadioInfo (0=991A,
    // 1=FTX1) for panel naming. yaesu2_connected becomes true once a
    // YaesuState2 packet arrives → the UI then shows the 2nd panel.
    pub yaesu_model: u8,
    pub yaesu2_model: u8,
    /// Why an absent radio is absent, when the server can name it - the
    /// `PORT_TROUBLE_*` wire codes from the presence push. 0 when the radio is
    /// there or the server has nothing to say. The UI turns a non-zero code
    /// into words next to the radio selector; the most common one is "the COM
    /// port is held by another control program".
    pub yaesu_port_trouble: u8,
    pub yaesu2_port_trouble: u8,
    pub yaesu2_connected: bool,
    pub yaesu2_freq_a: u64,
    pub yaesu2_freq_b: u64,
    pub yaesu2_mode: u8,
    pub yaesu2_smeter: u16,
    pub yaesu2_tx_active: bool,
    pub yaesu2_power_on: bool,
    pub yaesu2_af_gain: u8,
    pub yaesu2_tx_power: u8,
    pub yaesu2_tx_power_max: u8,
    pub yaesu2_squelch: u8,
    pub yaesu2_rf_gain: u8,
    pub yaesu2_mic_gain: u8,
    pub yaesu2_split: bool,
    pub yaesu2_scan: bool,
    pub yaesu2_tuner_state: u8,
    pub yaesu2_hi_swr: bool,
    pub yaesu2_feature_toggles: u32,
    pub yaesu2_feature_levels: [u8; 16],
    pub yaesu2_feature_freqs: [u16; 4],
    pub yaesu2_vfo_select: u8,
    pub yaesu2_memory_channel: u16,
    pub yaesu2_memory_data: Option<String>, // tab-separated memory-dump radio 2 (Phase B)
    pub yaesu2_menu_data: Option<String>,   // EX/menu values radio 2, own field (see slot 0)
    // New TCI controls (v2.10.3.13)
    pub agc_mode: u8,       // 0=off, 1=long, 2=slow, 3=med, 4=fast, 5=custom
    pub agc_gain: u8,       // 0-120
    pub rit_enable: bool,
    pub rit_offset: i16,    // Hz
    pub xit_enable: bool,
    pub xit_offset: i16,    // Hz
    pub sql_enable: bool,
    pub sql_level: u8,      // 0-160
    pub nb_enable: bool,
    pub nb_level: u8,  // 0=off, 1=NB1, 2=NB2
    pub agc_auto_rx1: bool,
    pub agc_auto_rx2: bool,
    pub cw_keyer_speed: u8, // WPM
    pub vfo_lock: bool,
    pub binaural: bool,
    pub apf_enable: bool,
    pub mute: bool,
    pub rx_mute: bool,
    pub nf_enable: bool,
    pub rx2_nf_enable: bool,
    pub rx_balance: i8,
    pub tune_drive: u8,     // 0-100
    pub mon_volume: i8,     // dB, typically -40..0

    pub auth_rejected: bool,
    pub totp_required: bool,
    pub diversity_enabled: bool,
    pub diversity_ref: u8,
    pub diversity_source: u8,
    pub diversity_gain_rx1: u16,
    pub diversity_gain_rx2: u16,
    pub diversity_gain_multi: u16, // multi × 100 (range 100..1000 = 1.00..10.00)
    pub diversity_phase: u16, // encoded: phase*100 + 18000

    pub ddc_sample_rate_rx1: u16, // kHz (e.g. 192, 384, 1536)
    pub ddc_sample_rate_rx2: u16, // kHz
    pub diversity_autonull_result: u16, // 0=not done, >0 = improvement*10+32000
}

impl Default for RadioState {
    fn default() -> Self {
        Self {
            connected: false,
            ptt_denied: false,
            audio_error: false,
            connect_status: ConnectStatus::Disconnected,
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
            loss_percent: 0,
            down_kbps: 0,
            up_kbps: 0,
            bw_breakdown: Vec::new(),
            capture_level: 0.0,
            playback_level: 0.0,
            playback_level_bin_r: 0.0,
            playback_level_rx2: 0.0,
            playback_level_yaesu: 0.0,
            playback_level_yaesu2: 0.0,
            playback_level_vrx1: 0.0,
            playback_level_vrx2: 0.0,
            yaesu_mic_level: 0.0,
            frequency_hz: 0,
            mode: 0,
            smeter: SMETER_NO_DATA_DBM,
            smeter_sig: SMETER_NO_DATA_DBM,
            smeter_peakbin: SMETER_NO_DATA_DBM,
            power_on: false,
            tx_profile: 0,
            nr_level: 0,
            anf_on: false,
            drive_level: 0,
            rx_af_gain: 0,
            agc_enabled: false,
            other_tx: false,
            recording: false,
            playing: false,
            last_recorded: Vec::new(),
            thetis_swr_x100: 100,
            filter_low_hz: 0,
            filter_high_hz: 0,
            thetis_configured: true,
            rx2_present: true,
            thetis_starting: false,
            mon_on: false,
            tx_profile_names: Vec::new(),
            dx_spots_enabled: true,
            session_generation: 0,
            server_subs: None,
            full_spectrum_enabled: true,
            rx1_enabled: true,
            rx2_enabled: false,
            rx2_spectrum_enabled: false,
            vfo_sync: false,
            frequency_rx2_hz: 0,
            mode_rx2: 0,
            smeter_rx2: SMETER_NO_DATA_DBM,
            smeter_rx2_sig: SMETER_NO_DATA_DBM,
            smeter_rx2_peakbin: SMETER_NO_DATA_DBM,
            rx2_af_gain: 0,
            rx2_nr_level: 0,
            rx2_anf_on: false,
            rx2_agc_mode: 3,
            rx2_agc_gain: 80,
            rx2_sql_enable: false,
            rx2_sql_level: 0,
            rx2_nb_enable: false,
            rx2_binaural: false,
            rx2_apf_enable: false,
            rx2_vfo_lock: false,
            filter_rx2_low_hz: 0,
            filter_rx2_high_hz: 0,
            rx2_spectrum_bins: Vec::new(),
            rx2_spectrum_center_hz: 0,
            rx2_spectrum_span_hz: 0,
            rx2_spectrum_ref_level: 0,
            rx2_spectrum_db_per_unit: 1,
            rx2_spectrum_sequence: 0,
            rx2_full_spectrum_bins: Vec::new(),
            rx2_full_spectrum_center_hz: 0,
            rx2_full_spectrum_span_hz: 0,
            rx2_full_spectrum_sequence: 0,
            spectrum_bins: Vec::new(),
            spectrum_center_hz: 0,
            spectrum_span_hz: 0,
            spectrum_ref_level: 0,
            spectrum_db_per_unit: 1,
            spectrum_sequence: 0,
            full_spectrum_bins: Vec::new(),
            full_spectrum_center_hz: 0,
            full_spectrum_span_hz: 0,
            full_spectrum_sequence: 0,
            vrx1_extracted_bins: Vec::new(),
            vrx1_extracted_center_hz: 0,
            vrx1_extracted_span_hz: 0,
            vrx1_extracted_sequence: 0,
            vrx2_extracted_bins: Vec::new(),
            vrx2_extracted_center_hz: 0,
            vrx2_extracted_span_hz: 0,
            vrx2_extracted_sequence: 0,
            vrx1_autotune_freq_hz: 0,
            vrx2_autotune_freq_hz: 0,
            tx_filter_low_hz: 0,
            tx_filter_high_hz: 0,
            tx_filter_supported: false,
            amplitec_available: false,
            amplitec_connected: false,
            amplitec_switch_a: 0,
            amplitec_switch_b: 0,
            amplitec_labels: String::new(),
            amplitec_power_max_w: [0; 6],
            amplitec_power_tx_blocked: [false; 6],
            amplitec_power_loaded: false,
            tuner_available: false,
            tuner_connected: false,
            tuner_state: 0,
            tuner_can_tune: false,
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
            rf2k_tuner_freq_khz: 0,
            rf2k_segment_size_khz: 0,
            rf2k_drive_w: 0,
            rf2k_modulation: String::new(),
            rf2k_max_power_w: 0,
            rf2k_device_name: String::new(),
            rf2k_available: false,
            rf2k_active: false,
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
            dx_spots: Vec::new(),
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
            yaesu_tx_active: false,
            yaesu_power_on: false,
            yaesu_tx_power_max: 0,
            yaesu2_tx_power_max: 0,
            yaesu_af_gain: 0,
            yaesu_tx_power: 0,
            yaesu_squelch: 0,
            yaesu_rf_gain: 0,
            yaesu_mic_gain: 0,
            yaesu_split: false,
            yaesu_scan: false,
            yaesu_tuner_state: 0,
            yaesu_hi_swr: false,
            yaesu_feature_toggles: 0,
            yaesu_feature_levels: [0u8; 16],
            yaesu_feature_freqs: [0u16; 4],
            yaesu_vfo_select: 0,
            yaesu_memory_channel: 0,
            yaesu_memory_data: None,
            yaesu_menu_data: None,
            server_report: None,
            server_report_failed: None,
            yaesu_model: 0,
            yaesu2_model: 1,
            yaesu_port_trouble: 0,
            yaesu2_port_trouble: 0,
            yaesu2_connected: false,
            yaesu2_freq_a: 0,
            yaesu2_freq_b: 0,
            yaesu2_mode: 1,
            yaesu2_smeter: 0,
            yaesu2_tx_active: false,
            yaesu2_power_on: false,
            yaesu2_af_gain: 0,
            yaesu2_tx_power: 0,
            yaesu2_squelch: 0,
            yaesu2_rf_gain: 0,
            yaesu2_mic_gain: 0,
            yaesu2_split: false,
            yaesu2_scan: false,
            yaesu2_tuner_state: 0,
            yaesu2_hi_swr: false,
            yaesu2_feature_toggles: 0,
            yaesu2_feature_levels: [0u8; 16],
            yaesu2_feature_freqs: [0u16; 4],
            yaesu2_vfo_select: 0,
            yaesu2_memory_channel: 0,
            yaesu2_memory_data: None,
            yaesu2_menu_data: None,
            agc_mode: 3, // med
            agc_gain: 80,
            rit_enable: false,
            rit_offset: 0,
            xit_enable: false,
            xit_offset: 0,
            sql_enable: false,
            sql_level: 0,
            nb_enable: false,
            nb_level: 0,
            agc_auto_rx1: false,
            agc_auto_rx2: false,
            cw_keyer_speed: 20,
            vfo_lock: false,
            binaural: false,
            apf_enable: false,
            mute: false,
            rx_mute: false,
            nf_enable: false,
            rx2_nf_enable: false,
            rx_balance: 0,
            tune_drive: 0,
            mon_volume: -40,
            auth_rejected: false,
            totp_required: false,
            diversity_enabled: false,
            diversity_ref: 1,
            diversity_source: 0,
            diversity_gain_rx1: 1500,
            diversity_gain_rx2: 1500,
            diversity_gain_multi: 100, // 1.00x default; updated on connect from Thetis caps
            diversity_phase: 18000,
            ddc_sample_rate_rx1: 0,
            ddc_sample_rate_rx2: 0,
            diversity_autonull_result: 0,
        }
    }
}

#[cfg(test)]
mod recording_list_tests {
    use super::RadioState;

    /// Two sources recorded at once must both be reachable afterwards. When
    /// this was one path instead of a list, the second source overwrote the
    /// first and the Play button could only ever offer the last one - which an
    /// operator recording both radios read as "the first one did not record".
    #[test]
    fn every_started_recording_stays_reachable() {
        let mut s = RadioState::default();
        s.last_recorded.push(("Radio 1".to_string(), "/tmp/Yaesu1_x.wav".to_string()));
        s.last_recorded.push(("Radio 2".to_string(), "/tmp/Yaesu2_x.wav".to_string()));

        assert_eq!(s.last_recorded.len(), 2);
        let names: Vec<&str> = s.last_recorded.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["Radio 1", "Radio 2"]);
        assert!(s.last_recorded.iter().all(|(_, p)| !p.is_empty()));
    }
}
