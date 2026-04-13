#![allow(dead_code)]
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use log::{info, warn};
use midir::{MidiInput, MidiInputConnection, MidiInputPort, MidiOutput, MidiOutputConnection};

// ── MIDI actions ──────────────────────────────────────────────────────

/// All radio functions that can be mapped to a MIDI control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MidiAction {
    Ptt,
    VfoATune,
    VfoBTune,
    MasterVolume,
    VfoAVolume,
    VfoBVolume,
    TxGain,
    Drive,
    ModeCycle,
    BandUp,
    BandDown,
    NrToggle,
    AnfToggle,
    Rx2Toggle,
    VfoSwap,
    PowerToggle,
    MicAgcToggle,
    FreqStepUp,
    FreqStepDown,
    SpectrumZoom,
    SpectrumPan,
    RefLevel,
    WaterfallContrast,
    Rx2SpectrumZoom,
    Rx2SpectrumPan,
    Rx2RefLevel,
    Rx2WaterfallContrast,
    FilterWiden,
    FilterNarrow,
    NrLevel,
    // TCI controls (added v0.5.3+)
    AgcMode,
    AgcGain,
    NbToggle,
    ApfToggle,
    VfoLock,
    RitToggle,
    RitOffset,
    XitToggle,
    XitOffset,
    SqlToggle,
    SqlLevel,
    CwSpeed,
    TuneToggle,
    TuneDrive,
    MuteAll,
    Rx1Mute,
    MonVolume,
    RxBalance,
    // Yaesu controls
    YaesuVolume,
    YaesuRfGain,
    YaesuMicGain,
    YaesuPtt,
}

impl MidiAction {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Ptt => "PTT",
            Self::VfoATune => "VFO A Tune",
            Self::VfoBTune => "VFO B Tune",
            Self::MasterVolume => "Master Volume",
            Self::VfoAVolume => "VFO A Volume",
            Self::VfoBVolume => "VFO B Volume",
            Self::TxGain => "TX Gain",
            Self::Drive => "Drive",
            Self::ModeCycle => "Mode Cycle",
            Self::BandUp => "Band Up",
            Self::BandDown => "Band Down",
            Self::NrToggle => "NR Toggle",
            Self::AnfToggle => "ANF Toggle",
            Self::Rx2Toggle => "RX2 Toggle",
            Self::VfoSwap => "A ⇔ B Swap",
            Self::PowerToggle => "Power Toggle",
            Self::MicAgcToggle => "Mic AGC Toggle",
            Self::FreqStepUp => "Freq Step Up",
            Self::FreqStepDown => "Freq Step Down",
            Self::SpectrumZoom => "Spectrum Zoom",
            Self::SpectrumPan => "Spectrum Pan",
            Self::RefLevel => "Ref Level",
            Self::WaterfallContrast => "Waterfall Contrast",
            Self::Rx2SpectrumZoom => "RX2 Spectrum Zoom",
            Self::Rx2SpectrumPan => "RX2 Spectrum Pan",
            Self::Rx2RefLevel => "RX2 Ref Level",
            Self::Rx2WaterfallContrast => "RX2 Waterfall Contrast",
            Self::FilterWiden => "Filter Widen",
            Self::FilterNarrow => "Filter Narrow",
            Self::NrLevel => "NR Level",
            Self::AgcMode => "AGC Mode",
            Self::AgcGain => "AGC Gain",
            Self::NbToggle => "NB Toggle",
            Self::ApfToggle => "APF Toggle",
            Self::VfoLock => "VFO Lock",
            Self::RitToggle => "RIT Toggle",
            Self::RitOffset => "RIT Offset",
            Self::XitToggle => "XIT Toggle",
            Self::XitOffset => "XIT Offset",
            Self::SqlToggle => "SQL Toggle",
            Self::SqlLevel => "SQL Level",
            Self::CwSpeed => "CW Speed",
            Self::TuneToggle => "Tune Toggle",
            Self::TuneDrive => "Tune Drive",
            Self::MuteAll => "Mute All",
            Self::Rx1Mute => "RX1 Mute",
            Self::MonVolume => "Monitor Volume",
            Self::RxBalance => "RX Balance",
            Self::YaesuVolume => "Yaesu Volume",
            Self::YaesuRfGain => "Yaesu RF Gain",
            Self::YaesuMicGain => "Yaesu Mic Gain",
            Self::YaesuPtt => "Yaesu PTT",
        }
    }

    pub fn config_key(&self) -> &'static str {
        match self {
            Self::Ptt => "ptt",
            Self::VfoATune => "vfo_a_tune",
            Self::VfoBTune => "vfo_b_tune",
            Self::MasterVolume => "master_volume",
            Self::VfoAVolume => "vfo_a_volume",
            Self::VfoBVolume => "vfo_b_volume",
            Self::TxGain => "tx_gain",
            Self::Drive => "drive",
            Self::ModeCycle => "mode_cycle",
            Self::BandUp => "band_up",
            Self::BandDown => "band_down",
            Self::NrToggle => "nr_toggle",
            Self::AnfToggle => "anf_toggle",
            Self::Rx2Toggle => "rx2_toggle",
            Self::VfoSwap => "vfo_swap",
            Self::PowerToggle => "power_toggle",
            Self::MicAgcToggle => "mic_agc_toggle",
            Self::FreqStepUp => "freq_step_up",
            Self::FreqStepDown => "freq_step_down",
            Self::SpectrumZoom => "spectrum_zoom",
            Self::SpectrumPan => "spectrum_pan",
            Self::RefLevel => "ref_level",
            Self::WaterfallContrast => "waterfall_contrast",
            Self::Rx2SpectrumZoom => "rx2_spectrum_zoom",
            Self::Rx2SpectrumPan => "rx2_spectrum_pan",
            Self::Rx2RefLevel => "rx2_ref_level",
            Self::Rx2WaterfallContrast => "rx2_waterfall_contrast",
            Self::FilterWiden => "filter_widen",
            Self::FilterNarrow => "filter_narrow",
            Self::NrLevel => "nr_level",
            Self::AgcMode => "agc_mode",
            Self::AgcGain => "agc_gain",
            Self::NbToggle => "nb_toggle",
            Self::ApfToggle => "apf_toggle",
            Self::VfoLock => "vfo_lock",
            Self::RitToggle => "rit_toggle",
            Self::RitOffset => "rit_offset",
            Self::XitToggle => "xit_toggle",
            Self::XitOffset => "xit_offset",
            Self::SqlToggle => "sql_toggle",
            Self::SqlLevel => "sql_level",
            Self::CwSpeed => "cw_speed",
            Self::TuneToggle => "tune_toggle",
            Self::TuneDrive => "tune_drive",
            Self::MuteAll => "mute_all",
            Self::Rx1Mute => "rx1_mute",
            Self::MonVolume => "mon_volume",
            Self::RxBalance => "rx_balance",
            Self::YaesuVolume => "yaesu_volume",
            Self::YaesuRfGain => "yaesu_rf_gain",
            Self::YaesuMicGain => "yaesu_mic_gain",
            Self::YaesuPtt => "yaesu_ptt",
        }
    }

    pub fn from_config_key(key: &str) -> Option<Self> {
        match key {
            "ptt" => Some(Self::Ptt),
            "vfo_a_tune" => Some(Self::VfoATune),
            "vfo_b_tune" => Some(Self::VfoBTune),
            "master_volume" => Some(Self::MasterVolume),
            "vfo_a_volume" => Some(Self::VfoAVolume),
            "vfo_b_volume" => Some(Self::VfoBVolume),
            "tx_gain" => Some(Self::TxGain),
            "drive" => Some(Self::Drive),
            "mode_cycle" => Some(Self::ModeCycle),
            "band_up" => Some(Self::BandUp),
            "band_down" => Some(Self::BandDown),
            "nr_toggle" => Some(Self::NrToggle),
            "anf_toggle" => Some(Self::AnfToggle),
            "rx2_toggle" => Some(Self::Rx2Toggle),
            "vfo_swap" => Some(Self::VfoSwap),
            "power_toggle" => Some(Self::PowerToggle),
            "mic_agc_toggle" => Some(Self::MicAgcToggle),
            "freq_step_up" => Some(Self::FreqStepUp),
            "freq_step_down" => Some(Self::FreqStepDown),
            "spectrum_zoom" => Some(Self::SpectrumZoom),
            "spectrum_pan" => Some(Self::SpectrumPan),
            "ref_level" => Some(Self::RefLevel),
            "waterfall_contrast" => Some(Self::WaterfallContrast),
            "rx2_spectrum_zoom" => Some(Self::Rx2SpectrumZoom),
            "rx2_spectrum_pan" => Some(Self::Rx2SpectrumPan),
            "rx2_ref_level" => Some(Self::Rx2RefLevel),
            "rx2_waterfall_contrast" => Some(Self::Rx2WaterfallContrast),
            "filter_widen" => Some(Self::FilterWiden),
            "filter_narrow" => Some(Self::FilterNarrow),
            "nr_level" => Some(Self::NrLevel),
            "agc_mode" => Some(Self::AgcMode),
            "agc_gain" => Some(Self::AgcGain),
            "nb_toggle" => Some(Self::NbToggle),
            "apf_toggle" => Some(Self::ApfToggle),
            "vfo_lock" => Some(Self::VfoLock),
            "rit_toggle" => Some(Self::RitToggle),
            "rit_offset" => Some(Self::RitOffset),
            "xit_toggle" => Some(Self::XitToggle),
            "xit_offset" => Some(Self::XitOffset),
            "sql_toggle" => Some(Self::SqlToggle),
            "sql_level" => Some(Self::SqlLevel),
            "cw_speed" => Some(Self::CwSpeed),
            "tune_toggle" => Some(Self::TuneToggle),
            "tune_drive" => Some(Self::TuneDrive),
            "mute_all" => Some(Self::MuteAll),
            "rx1_mute" => Some(Self::Rx1Mute),
            "mon_volume" => Some(Self::MonVolume),
            "rx_balance" => Some(Self::RxBalance),
            "yaesu_volume" => Some(Self::YaesuVolume),
            "yaesu_rf_gain" => Some(Self::YaesuRfGain),
            "yaesu_mic_gain" => Some(Self::YaesuMicGain),
            "yaesu_ptt" => Some(Self::YaesuPtt),
            _ => None,
        }
    }

    /// All available actions for the UI mapping selector.
    pub const ALL: &'static [MidiAction] = &[
        Self::Ptt,
        Self::VfoATune,
        Self::VfoBTune,
        Self::MasterVolume,
        Self::VfoAVolume,
        Self::VfoBVolume,
        Self::TxGain,
        Self::Drive,
        Self::ModeCycle,
        Self::BandUp,
        Self::BandDown,
        Self::NrToggle,
        Self::AnfToggle,
        Self::Rx2Toggle,
        Self::VfoSwap,
        Self::PowerToggle,
        Self::MicAgcToggle,
        Self::FreqStepUp,
        Self::FreqStepDown,
        Self::SpectrumZoom,
        Self::SpectrumPan,
        Self::RefLevel,
        Self::WaterfallContrast,
        Self::Rx2SpectrumZoom,
        Self::Rx2SpectrumPan,
        Self::Rx2RefLevel,
        Self::Rx2WaterfallContrast,
        Self::FilterWiden,
        Self::FilterNarrow,
        Self::NrLevel,
        Self::AgcMode,
        Self::AgcGain,
        Self::NbToggle,
        Self::ApfToggle,
        Self::VfoLock,
        Self::RitToggle,
        Self::RitOffset,
        Self::XitToggle,
        Self::XitOffset,
        Self::SqlToggle,
        Self::SqlLevel,
        Self::CwSpeed,
        Self::TuneToggle,
        Self::TuneDrive,
        Self::MuteAll,
        Self::Rx1Mute,
        Self::MonVolume,
        Self::RxBalance,
        Self::YaesuVolume,
        Self::YaesuRfGain,
        Self::YaesuMicGain,
        Self::YaesuPtt,
    ];
}

// ── Control type ──────────────────────────────────────────────────────

/// How the MIDI control behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlType {
    /// Momentary or toggle button (Note On/Off or CC as button)
    Button,
    /// Absolute value slider/knob (CC 0-127)
    Slider,
    /// Relative encoder (CC: 64=center, <64=left, >64=right)
    Encoder,
}

impl ControlType {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Button => "Button",
            Self::Slider => "Slider",
            Self::Encoder => "Encoder",
        }
    }

    pub fn config_key(&self) -> &'static str {
        match self {
            Self::Button => "button",
            Self::Slider => "slider",
            Self::Encoder => "encoder",
        }
    }

    pub fn from_config_key(key: &str) -> Option<Self> {
        match key {
            "button" => Some(Self::Button),
            "slider" => Some(Self::Slider),
            "encoder" => Some(Self::Encoder),
            _ => None,
        }
    }
}

// ── Mapping ───────────────────────────────────────────────────────────

/// One MIDI → action mapping.
#[derive(Debug, Clone)]
pub struct MidiMapping {
    /// true = Note On/Off message, false = CC message
    pub is_note: bool,
    /// MIDI channel (0-15), or 255 for "any channel"
    pub channel: u8,
    /// CC number or Note number (0-127)
    pub number: u8,
    /// How to interpret the control value
    pub control_type: ControlType,
    /// What action to trigger
    pub action: MidiAction,
}

impl MidiMapping {
    /// Serialize to config string: "cc:1:7:slider:master_volume" or "note:1:60:button:ptt"
    pub fn to_config(&self) -> String {
        let msg_type = if self.is_note { "note" } else { "cc" };
        format!(
            "{}:{}:{}:{}:{}",
            msg_type,
            self.channel,
            self.number,
            self.control_type.config_key(),
            self.action.config_key(),
        )
    }

    /// Parse from config string.
    pub fn from_config(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 5 {
            return None;
        }
        let is_note = match parts[0] {
            "note" => true,
            "cc" => false,
            _ => return None,
        };
        let channel: u8 = parts[1].parse().ok()?;
        let number: u8 = parts[2].parse().ok()?;
        let control_type = ControlType::from_config_key(parts[3])?;
        let action = MidiAction::from_config_key(parts[4])?;
        Some(Self { is_note, channel, number, control_type, action })
    }

    /// Human-readable description for UI display.
    pub fn source_label(&self) -> String {
        let msg = if self.is_note { "Note" } else { "CC" };
        format!("{} ch{} #{}", msg, self.channel + 1, self.number)
    }
}

// ── MIDI event (sent from callback to UI) ─────────────────────────────

/// Parsed MIDI event delivered to the UI thread.
#[derive(Debug, Clone)]
pub enum MidiEvent {
    /// Button/note pressed: action + velocity (>0 = pressed, 0 = released)
    Button(MidiAction, u8),
    /// Slider/knob absolute: action + value (0-127)
    Slider(MidiAction, u8),
    /// Encoder relative: action + delta (negative = left, positive = right)
    Encoder(MidiAction, i8),
    /// Learn mode: raw event info for mapping (is_note, channel, number, value)
    Learn(bool, u8, u8, u8),
}

// ── MIDI manager ──────────────────────────────────────────────────────

/// Manages MIDI input/output: device enumeration, connection, event dispatch, LED feedback.
pub struct MidiManager {
    /// Active input connection (keeps callback alive; dropped on disconnect)
    connection: Option<MidiInputConnection<()>>,
    /// Active output connection for LED feedback
    out_connection: Option<MidiOutputConnection>,
    /// Name of the connected port
    connected_port_name: String,
    /// Receiver for events from the MIDI callback
    pub event_rx: mpsc::Receiver<MidiEvent>,
    /// Sender cloned into the callback
    event_tx: mpsc::Sender<MidiEvent>,
    /// Current mappings (shared with callback via Arc<Mutex>)
    mappings: Arc<Mutex<Vec<MidiMapping>>>,
    /// Learn mode: when true, next MIDI message sends Learn event instead of mapped action
    learn_mode: Arc<Mutex<bool>>,
}

impl MidiManager {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            connection: None,
            out_connection: None,
            connected_port_name: String::new(),
            event_rx: rx,
            event_tx: tx,
            mappings: Arc::new(Mutex::new(Vec::new())),
            learn_mode: Arc::new(Mutex::new(false)),
        }
    }

    /// List available MIDI input port names.
    pub fn list_ports() -> Vec<String> {
        let midi_in = match MidiInput::new("ThetisLink-probe") {
            Ok(m) => m,
            Err(e) => {
                warn!("MIDI init failed: {}", e);
                return Vec::new();
            }
        };
        let ports = midi_in.ports();
        ports
            .iter()
            .filter_map(|p| midi_in.port_name(p).ok())
            .collect()
    }

    /// Connect to the MIDI port with the given name.
    pub fn connect(&mut self, port_name: &str) -> bool {
        // Drop previous connection
        self.disconnect();

        let midi_in = match MidiInput::new("ThetisLink") {
            Ok(m) => m,
            Err(e) => {
                warn!("MIDI init failed: {}", e);
                return false;
            }
        };

        let port = self.find_port(&midi_in, port_name);
        let port = match port {
            Some(p) => p,
            None => {
                warn!("MIDI port '{}' not found", port_name);
                return false;
            }
        };

        let tx = self.event_tx.clone();
        let mappings = self.mappings.clone();
        let learn = self.learn_mode.clone();

        match midi_in.connect(
            &port,
            "thetislink-in",
            move |_timestamp, message, _| {
                Self::handle_message(message, &tx, &mappings, &learn);
            },
            (),
        ) {
            Ok(conn) => {
                info!("MIDI connected: {}", port_name);
                self.connection = Some(conn);
                self.connected_port_name = port_name.to_string();

                // Also try to connect MIDI output for LED feedback
                self.connect_output(port_name);

                true
            }
            Err(e) => {
                warn!("MIDI connect failed: {}", e);
                false
            }
        }
    }

    /// Disconnect the current MIDI port.
    pub fn disconnect(&mut self) {
        if let Some(conn) = self.out_connection.take() {
            conn.close();
        }
        if let Some(conn) = self.connection.take() {
            conn.close();
            info!("MIDI disconnected");
        }
        self.connected_port_name.clear();
    }

    /// Whether a MIDI device is currently connected.
    pub fn is_connected(&self) -> bool {
        self.connection.is_some()
    }

    /// Name of the connected port.
    pub fn connected_port_name(&self) -> &str {
        &self.connected_port_name
    }

    /// Set learn mode on/off.
    pub fn set_learn_mode(&self, enabled: bool) {
        if let Ok(mut lm) = self.learn_mode.lock() {
            *lm = enabled;
        }
    }

    /// Check if learn mode is active.
    pub fn is_learn_mode(&self) -> bool {
        self.learn_mode.lock().map_or(false, |lm| *lm)
    }

    /// Replace all mappings.
    pub fn set_mappings(&self, new_mappings: Vec<MidiMapping>) {
        if let Ok(mut m) = self.mappings.lock() {
            *m = new_mappings;
        }
    }

    /// Get a clone of the current mappings.
    pub fn get_mappings(&self) -> Vec<MidiMapping> {
        self.mappings.lock().map_or_else(|_| Vec::new(), |m| m.clone())
    }

    /// Add a single mapping.
    pub fn add_mapping(&self, mapping: MidiMapping) {
        if let Ok(mut m) = self.mappings.lock() {
            // Remove existing mapping for same source (same is_note + channel + number)
            m.retain(|existing| {
                !(existing.is_note == mapping.is_note
                    && existing.channel == mapping.channel
                    && existing.number == mapping.number)
            });
            m.push(mapping);
        }
    }

    /// Remove mapping at index.
    pub fn remove_mapping(&self, index: usize) {
        if let Ok(mut m) = self.mappings.lock() {
            if index < m.len() {
                m.remove(index);
            }
        }
    }

    /// Send LED on/off for a specific action. Finds the mapping and sends
    /// Note On (velocity 127) or Note Off (velocity 0) to the output port.
    /// For CC-mapped buttons, sends CC value 127 or 0.
    pub fn send_led(&mut self, action: MidiAction, on: bool) {
        let conn = match self.out_connection.as_mut() {
            Some(c) => c,
            None => return,
        };
        let mappings = match self.mappings.lock() {
            Ok(m) => m.clone(),
            Err(_) => return,
        };
        for mapping in &mappings {
            if mapping.action != action || mapping.control_type != ControlType::Button {
                continue;
            }
            let velocity = if on { 127u8 } else { 0u8 };
            let msg = if mapping.is_note {
                // Note On with velocity 0 = LED off, velocity 127 = LED on
                [0x90 | (mapping.channel & 0x0F), mapping.number, velocity]
            } else {
                // CC value 127 = on, 0 = off
                [0xB0 | (mapping.channel & 0x0F), mapping.number, velocity]
            };
            let _ = conn.send(&msg);
            return;
        }
    }

    // ── Private ───────────────────────────────────────────────────────

    /// Try to connect MIDI output to a port matching the input name.
    fn connect_output(&mut self, input_name: &str) {
        let midi_out = match MidiOutput::new("ThetisLink-out") {
            Ok(m) => m,
            Err(e) => {
                warn!("MIDI output init failed: {}", e);
                return;
            }
        };
        let ports = midi_out.ports();
        // Try exact match first, then substring match
        let port = ports.iter().find(|p| {
            midi_out.port_name(p).map_or(false, |n| n == input_name)
        }).or_else(|| {
            // Many controllers have slightly different in/out names,
            // try matching by common prefix (first 10 chars or device name)
            let prefix = if input_name.len() > 10 { &input_name[..10] } else { input_name };
            ports.iter().find(|p| {
                midi_out.port_name(p).map_or(false, |n| n.starts_with(prefix))
            })
        });
        match port {
            Some(p) => {
                let port_name = midi_out.port_name(p).unwrap_or_default();
                match midi_out.connect(p, "thetislink-out") {
                    Ok(conn) => {
                        info!("MIDI output connected: {}", port_name);
                        self.out_connection = Some(conn);
                    }
                    Err(e) => {
                        warn!("MIDI output connect failed: {}", e);
                    }
                }
            }
            None => {
                info!("No matching MIDI output port found for LED feedback");
            }
        }
    }

    fn find_port(&self, midi_in: &MidiInput, name: &str) -> Option<MidiInputPort> {
        let ports = midi_in.ports();
        for port in &ports {
            if let Ok(pname) = midi_in.port_name(port) {
                if pname == name {
                    return Some(port.clone());
                }
            }
        }
        None
    }

    /// Parse raw MIDI bytes and dispatch events.
    fn handle_message(
        msg: &[u8],
        tx: &mpsc::Sender<MidiEvent>,
        mappings: &Arc<Mutex<Vec<MidiMapping>>>,
        learn: &Arc<Mutex<bool>>,
    ) {
        if msg.is_empty() {
            return;
        }

        let status = msg[0];
        let msg_type = status & 0xF0;
        let channel = status & 0x0F;

        let (is_note, number, value) = match msg_type {
            0x90 if msg.len() >= 3 => {
                // Note On (velocity 0 = Note Off)
                (true, msg[1], msg[2])
            }
            0x80 if msg.len() >= 3 => {
                // Note Off
                (true, msg[1], 0u8)
            }
            0xB0 if msg.len() >= 3 => {
                // Control Change
                (false, msg[1], msg[2])
            }
            _ => return, // Ignore other message types
        };

        // Learn mode: send raw event info
        if learn.lock().map_or(false, |lm| *lm) {
            let _ = tx.send(MidiEvent::Learn(is_note, channel, number, value));
            return;
        }

        // Find matching mapping
        let mappings = match mappings.lock() {
            Ok(m) => m,
            Err(_) => return,
        };

        for mapping in mappings.iter() {
            if mapping.is_note != is_note {
                continue;
            }
            if mapping.channel != 255 && mapping.channel != channel {
                continue;
            }
            if mapping.number != number {
                continue;
            }

            let event = match mapping.control_type {
                ControlType::Button => MidiEvent::Button(mapping.action, value),
                ControlType::Slider => MidiEvent::Slider(mapping.action, value),
                ControlType::Encoder => {
                    // Two's complement relative encoding:
                    // 0x01-0x3F = positive (clockwise), 0x41-0x7F = negative (counter-clockwise)
                    // 0x7F = -1, 0x7E = -2, etc.
                    let delta = if value <= 0x3F {
                        value as i8 // 1..63 = positive
                    } else {
                        (value as i16 - 128) as i8 // 65..127 → -63..-1
                    };
                    if delta != 0 {
                        MidiEvent::Encoder(mapping.action, delta)
                    } else {
                        return;
                    }
                }
            };
            let _ = tx.send(event);
            return; // First match wins
        }
    }
}
