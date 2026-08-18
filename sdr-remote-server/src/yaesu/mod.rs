// SPDX-License-Identifier: GPL-2.0-or-later

use std::collections::HashSet;
use std::io::{Read, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{info, warn};

mod memory;
use memory::*;
mod audio;
use audio::{build_capture_stream, build_output_stream};
pub use audio::{available_audio_inputs, available_audio_outputs};
mod cat;
use cat::*;
mod ex_menu;
use ex_menu::*;
mod parse;
use parse::*;
mod poll;
mod tone_store;
use poll::*;

/// Radio model for the dual-radio abstraction (PATCH-dual-radio-991a-ftx1).
/// The Yaesu CAT dialect is shared across models; `RadioModel` only carries
/// the few per-model differences (autodetect `ID;` code, audio device name,
/// any mode-code extras). The FTX-1 is assumed CAT-compatible with the 991A.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RadioModel {
    Ft991a,
    Ftx1,
}

impl RadioModel {
    /// Short ASCII label for log prefixes (greppable, no Unicode/tofu).
    /// Delegates to the canonical table in core so server logs and client UI
    /// use the same name per model code (single source of truth).
    pub fn label(self) -> &'static str {
        sdr_remote_core::protocol::radio_model_name(self.as_code())
    }

    /// Per-radio log prefix `[radio{slot}/{MODEL}]`. Every log line
    /// in the slot chain starts with this so `grep radio1` shows all slot-1 events.
    pub fn tag(self, slot: u8) -> String {
        format!("[radio{}/{}]", slot, self.label())
    }

    /// Wire code for the `RadioInfo` packet (server -> client, panel naming):
    /// 0 = FT-991A, 1 = FTX-1. The mirror decode happens client-side on the u8.
    pub fn as_code(self) -> u8 {
        match self {
            RadioModel::Ft991a => 0,
            RadioModel::Ftx1 => 1,
        }
    }

    /// Map a Yaesu `ID;` response code to a known model. FT-991A = `0670`.
    /// The FTX-1 code is read live during bring-up; an unknown code returns
    /// `None` and the caller degrades to the shared 991A-compatible parser.
    pub fn from_id_code(code: &str) -> Option<RadioModel> {
        match code.trim() {
            "0670" => Some(RadioModel::Ft991a),
            // FTX-1 - captured live during the operator test 2026-06-14 (bring-up).
            "0840" => Some(RadioModel::Ftx1),
            _ => None,
        }
    }
}

/// Log all available audio input devices (once at startup). Helps the
/// operator pick the right device per radio (`yaesu_audio` / `yaesu2_audio`)
/// and makes edge-case 6 visible (two identically named "USB Audio CODEC").
pub fn log_input_devices() {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    match host.input_devices() {
        Ok(devs) => {
            let names: Vec<String> = devs.filter_map(|d| d.name().ok()).collect();
            info!("Available audio input devices ({}): {:?}", names.len(), names);
        }
        Err(e) => warn!("Could not enumerate audio input devices: {}", e),
    }
}

/// Probe a serial port to detect the radio model via the Yaesu `ID;` command,
/// with baud fallback (PATCH-dual-radio-991a-ftx1 §2.3). Tries `preferred_baud`
/// first, then the common Yaesu CAT speeds, until a valid `ID...;` response
/// arrives. Returns the detected model + the baud that worked.
///
/// **Model assignment is per-port, not per-slot** - so every combination works:
/// 2× 991A, 2× FTX-1, or a mix. Only a code this build recognises names a model:
/// `0670` -> FT-991A, `0840` -> FTX-1.
///
/// Anything else is NOT a model. It used to be "FTX-1 by elimination", which
/// reads as reasonable until the first read after opening a port comes back
/// garbled - as it routinely does - and a station with one FT-991A is driven all
/// session as an FTX-1: no memory channels, no menu values, an IF frame parsed
/// as gibberish, and the contradiction logged as a warning nobody sees.
///
/// No recognisable answer on any baud -> `None`, and the caller assumes the
/// 991A-compatible dialect. That assumption is temporary by construction: the
/// serial thread re-reads `ID;` on every successful open (bring-up probe) and
/// adopts the radio's own dialect when it names a model this build knows.
pub fn detect_model(port_name: &str, preferred_baud: u32) -> Option<(RadioModel, u32)> {
    let mut bauds = vec![preferred_baud];
    for b in [38400u32, 4800, 9600, 19200, 57600, 115200] {
        if !bauds.contains(&b) {
            bauds.push(b);
        }
    }
    for baud in bauds {
        let mut port = match serialport::new(port_name, baud)
            .data_bits(serialport::DataBits::Eight)
            .stop_bits(serialport::StopBits::One)
            .parity(serialport::Parity::None)
            .timeout(Duration::from_millis(100))
            .open()
        {
            Ok(p) => p,
            // Failing to open the port (e.g. already in use / does not exist) is not
            // baud-dependent -> trying further makes no sense. But say WHICH of
            // the two it was: "in use by another program" asks for a different
            // next move than "check the cable".
            Err(e) => {
                if let Some(text) = port_trouble_log_text(classify_open_error(&e)) {
                    log::warn!("{}: {}", port_name, text);
                }
                return None;
            }
        };
        // Anything left in the port from before this process is not an answer to
        // a question we asked. Draining is right HERE and wrong inside
        // `cat_query` (see the note there): a port just opened has no
        // conversation in flight to lose.
        let _ = port.clear(serialport::ClearBuffer::Input);

        // Three attempts, because the first read after opening a port routinely
        // comes back partial or empty and the real answer arrives a moment
        // later. That is not a theory: a station where this went wrong logged
        // `firmware (MAIN CPU): [ID0670;?;]` - the ID answer turning up as the
        // reply to the NEXT question, one query behind for the rest of the
        // session.
        for _ in 0..3 {
            let resp = cat_query(&mut port, "ID;");
            let code = resp_payload("ID", &resp);
            if let Some(model) = RadioModel::from_id_code(&code) {
                return Some((model, baud));
            }
            if resp.contains(';') && !code.is_empty() {
                // Something answered, but not with an ID we know. It used to be
                // called an FTX-1 at this point, which is how a garbled first
                // read turned an FT-991A into a radio the server then failed to
                // read a single memory channel or menu value from. A guess is
                // not a detection: keep asking.
                log::warn!(
                    "{}: ID; answered '{}', which is not a model this build knows - asking again",
                    port_name,
                    code
                );
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        // Nothing recognisable on this baud -> port is dropped here, next baud.
    }
    None
}

/// Sort a failed serial open into the `PORT_TROUBLE_*` wire codes.
///
/// A USB serial link that exists and is free practically never fails to open -
/// so the two failures that do happen deserve their own names. Access denied
/// means another program holds the port: Windows gives a COM port to one
/// process at a time, and 991A/FTX-1 owners routinely run other control
/// software that keeps the CAT port open in the background. Not-found means
/// the port itself is gone: cable out, or the COM number moved after a replug.
/// Anything else stays unnamed - a guess is not a diagnosis.
pub(crate) fn classify_open_error(e: &serialport::Error) -> u8 {
    use sdr_remote_core::protocol::{PORT_TROUBLE_BUSY, PORT_TROUBLE_MISSING, PORT_TROUBLE_NONE};
    match e.kind() {
        serialport::ErrorKind::NoDevice => PORT_TROUBLE_MISSING,
        serialport::ErrorKind::Io(std::io::ErrorKind::PermissionDenied) => PORT_TROUBLE_BUSY,
        serialport::ErrorKind::Io(std::io::ErrorKind::NotFound) => PORT_TROUBLE_MISSING,
        _ => PORT_TROUBLE_NONE,
    }
}

/// The operator-facing words for a trouble code, for the server log. The GUIs
/// translate the code themselves; the log speaks English like the rest of it.
pub(crate) fn port_trouble_log_text(code: u8) -> Option<&'static str> {
    use sdr_remote_core::protocol::{PORT_TROUBLE_BUSY, PORT_TROUBLE_MISSING};
    match code {
        PORT_TROUBLE_BUSY => Some(
            "the COM port is in use by another program - close other CAT/control software for this radio",
        ),
        PORT_TROUBLE_MISSING => Some(
            "the COM port does not exist - check the USB cable, and the COM number in Device Manager",
        ),
        _ => None,
    }
}

/// Yaesu FT-991A CAT serial controller with auto-reconnect.
/// Communicates via USB virtual COM port, ASCII commands terminated with ';'.
/// When the radio loses power or the serial connection drops, the controller
/// automatically retries every 3 seconds. Audio channels persist across
/// reconnects so the network audio loops don't need to restart.
pub struct YaesuRadio {
    cmd_tx: mpsc::Sender<YaesuCmd>,
    status: Arc<Mutex<YaesuState>>,
    /// Persistent audio RX channel - sender cloned into each new cpal capture stream.
    /// The receiver is taken once by the network audio loop and stays valid forever.
    _rx_audio_tx_keepalive: tokio::sync::mpsc::Sender<Vec<f32>>,
    pub audio_rx: Mutex<Option<tokio::sync::mpsc::Receiver<Vec<f32>>>>,
    pub audio_sample_rate: u32,
    /// Persistent TX audio sender - used by the network TX decode task.
    /// The receiver is consumed by the output bridge thread.
    pub tx_audio_tx: Option<tokio::sync::mpsc::Sender<Vec<f32>>>,
    pub tx_sample_rate: u32,
    /// Swappable cpal streams (replaced on reconnect)
    _capture_stream: Arc<StreamHolder>,
    _output_stream: Arc<StreamHolder>,
    /// Last time audio samples were received (epoch ms, for watchdog)
    _last_audio_time: Arc<std::sync::atomic::AtomicU64>,
    /// Swappable ring buffer producer for TX output (replaced on reconnect)
    _tx_producer: Arc<Mutex<Option<ringbuf::HeapProd<f32>>>>,
    /// Memory channel data read from radio (tab-separated text, ready to send to client)
    pub memory_data: Arc<Mutex<Option<String>>>,
    /// EX/menu values, in its OWN mailbox rather than sharing the memory one. Both
    /// are pushed independently now, and a shared mailbox means whichever is written
    /// second in a 200ms window silently replaces the first.
    pub menu_data: Arc<Mutex<Option<String>>>,
    /// Radio model as its wire code - drives the per-radio log prefix and
    /// per-model CAT quirks. Shared with the serial thread rather than fixed at
    /// construction: a silent port is assumed 991A, and the thread corrects
    /// this the moment the radio answers `ID;` with a model it knows. Read it
    /// through [`Self::model_code`].
    model_shared: Arc<std::sync::atomic::AtomicU8>,
    pub slot: u8,
    /// Cleared when this radio is dropped, so its threads stop.
    ///
    /// The serial thread holds a sender of the command channel itself (the PTT
    /// watchdog releases through it), which means the channel never disconnects
    /// and "the owner is gone" cannot be noticed that way. Without this flag a
    /// radio that was built but never adopted - the GUI gives up on a slow port
    /// after five seconds - kept a thread running for the life of the process:
    /// opening the port, reading memories, writing convincing `[radio0/991A]`
    /// lines into the log for a radio no client could see (2026-08-12).
    alive: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for YaesuRadio {
    fn drop(&mut self) {
        self.alive.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Thread-safe holder for a cpal::Stream that can be swapped on reconnect.
struct StreamHolder(Mutex<Option<cpal::Stream>>);
// SAFETY: cpal::Stream on Windows (WASAPI) uses COM handles safe to move between threads.
unsafe impl Send for StreamHolder {}
unsafe impl Sync for StreamHolder {}

impl StreamHolder {
    fn new(stream: Option<cpal::Stream>) -> Self {
        Self(Mutex::new(stream))
    }
    fn set(&self, stream: Option<cpal::Stream>) {
        *self.0.lock().unwrap() = stream;
    }
    fn is_set(&self) -> bool {
        self.0.lock().unwrap().is_some()
    }
}

// SAFETY: cpal::Stream on Windows (WASAPI) uses COM handles safe to move between threads.
unsafe impl Send for YaesuRadio {}
unsafe impl Sync for YaesuRadio {}

#[derive(Clone, Debug, Default)]
struct Ft991aUsbRoutingSnapshot {
    ssb_mic_select: Option<String>,
    ssb_port_select: Option<String>,
    am_mic_select: Option<String>,
    am_port_select: Option<String>,
}

#[derive(Clone, Copy, Debug)]
enum Ft991aUsbRoutingScope {
    Ssb,
    Am,
    All,
}

impl Ft991aUsbRoutingSnapshot {
    fn read(port: &mut Box<dyn serialport::SerialPort>, prefix: &str) -> Self {
        let snap = Self {
            ssb_mic_select: read_ex_menu_value(port, prefix, 106, "SSB MIC SELECT"),
            ssb_port_select: read_ex_menu_value(port, prefix, 109, "SSB PORT SELECT"),
            am_mic_select: read_ex_menu_value(port, prefix, 45, "AM MIC SELECT"),
            am_port_select: read_ex_menu_value(port, prefix, 48, "AM PORT SELECT"),
        };
        let known = [
            snap.ssb_mic_select.as_ref(),
            snap.ssb_port_select.as_ref(),
            snap.am_mic_select.as_ref(),
            snap.am_port_select.as_ref(),
        ]
        .iter()
        .filter(|v| v.is_some())
        .count();
        if known == 4 {
            info!(
                "{} 991A USB routing snapshot: SSB mic={} port={}, AM mic={} port={}",
                prefix,
                snap.ssb_mic_select.as_deref().unwrap_or("?"),
                snap.ssb_port_select.as_deref().unwrap_or("?"),
                snap.am_mic_select.as_deref().unwrap_or("?"),
                snap.am_port_select.as_deref().unwrap_or("?"),
            );
        } else {
            warn!(
                "{} 991A USB routing snapshot partial ({}/4); missing fields will not be restored blindly",
                prefix, known
            );
        }
        snap
    }
}
/// 991A high-SWR alarm threshold on the raw RM6 SWR-meter value (000-255).
/// Calibrated on hardware 2026-07-21: RM6 ~110 at SWR 2.8 (owner's chosen trip point),
/// ~0 at 1:1, 173-205 at SWR 4. Kept as a fallback in case the official RI0 Hi-SWR
/// flag turns out unreliable on some 991A firmware (then revert to this RM6 threshold).
#[allow(dead_code)]
const SWR_991A_RAW_THRESHOLD: u16 = 110;

#[derive(Clone, Debug)]
pub struct YaesuState {
    pub connected: bool,
    /// Why the serial port cannot be opened, when it cannot - one of the
    /// `PORT_TROUBLE_*` codes from the protocol. Set by the reconnect loop on
    /// every failed open, cleared on a successful one. This is what turns the
    /// most common field problem - other CAT software still holding the port
    /// in the background - from a silent absence into a named one.
    pub port_trouble: u8,
    pub vfo_a_freq: u64,
    pub vfo_b_freq: u64,
    pub mode: u8,           // Internal mode (0=LSB, 1=USB, etc. - Thetis numbering)
    pub tx_active: bool,
    pub smeter: u16,        // Raw S-meter value (0-255)
    pub af_gain: u8,        // 0-255
    pub tx_power: u8,       // 0-100
    /// FTX-1 power-head (from `PC` response): 0 = none/991A (`PC{nnn}`),
    /// 1 = field head (5-10W), 2 = SP A-1/Optima base (5-100W). Determines the
    /// `PC` set format so power works on both configs.
    pub power_head: u8,
    pub squelch: u8,        // 0-255
    pub rf_gain: u8,        // 0-255
    pub mic_gain: u8,       // 0-100
    pub power_on: bool,
    pub mode_char: char,    // Raw Yaesu mode character ('1'-'E')
    /// CAT monitor: log every frame the radio sends, verbatim, and ask the
    /// radio to report changes it makes on its own (Auto Information). For
    /// finding out what the radio actually does when a control is operated on
    /// the front panel - the only way left to learn how the FTX-1 stores a
    /// memory tone, after three documented CAT routes turned out wrong.
    pub cat_monitor: bool,
    pub vfo_select: u8,     // 0=VFO, 1=Memory, 2=MemTune (from IF P7)
    /// Number of IF-polls that a "still Memory" status is ignored after a
    /// memory->VFO escape (SetFreqA). Protects the optimistic vfo_select=0
    /// against an IF response that still reports the old Memory status and was
    /// already in flight when we sent the escape (race). 0 = not active.
    pub vfo_escape_pending: u8,
    pub memory_channel: u16, // Current memory channel number (from IF)
    /// Sorted list of filled memory-channel numbers from the last full read.
    /// Persists independently of `memory_data` (which is consumed/taken when the
    /// dump is sent to the client), so Mem+/Mem- can skip empty channels.
    pub filled_memory_channels: Vec<u16>,
    /// The last memory blob read from the radio, kept server-side. `memory_data`
    /// is TAKEN when it is handed to the client, so it cannot be the source for
    /// anything afterwards - the tone walk needs to know which channels carry a
    /// tone mode, long after the list was delivered.
    pub last_memory_blob: Option<String>,
    /// The server's copy of the EX/menu values, so a client can be served without
    /// the radio being walked again - the FTX-1 scan is 405 parameters and seconds
    /// of occupied CAT. See docs/internal/DESIGN-state-sync-push.md.
    pub last_menu_blob: Option<String>,
    pub split_active: bool,  // true = split mode active
    pub scan_active: bool,   // true = scanning
    /// Internal ATU state from the `AC;` readback (PATCH-yaesu-internal-atu), normalised:
    /// 0=off/bypass, 1=on, 2=tuning-in-progress. Never the raw CAT P3.
    pub tuner_state: u8,
    /// DSP/function toggle bitfield (PATCH-yaesu-extra-controls): bit N = YaesuCtrl N
    /// on/off, from the per-control read-parse. A1: RfAtt(0), BreakIn(1); A2: Narrow(2), AutoNotch(3).
    pub feature_toggles: u32,
    /// Multi-state/level values indexed by YaesuCtrl (Fase B: Agc(6), PreAmp(7); Fase C: Nb(8),Dnr(9),Processor(10),Amc(11)).
    pub feature_levels: [u8; 16],
    /// Fase D frequency values (u16): [0]=Contour, [1]=APF, [2]=Notch.
    pub feature_freqs: [u16; 4],
    /// Squelch open (BUSY) according to the radio (FTX-1 `RI` response P8). True =
    /// signal present / squelch open -> audio passes. Default true (open) so
    /// radios without RI (991A) or before the first poll are never gated.
    /// Drives the server-side software squelch on the FTX-1 USB audio.
    pub squelch_open: bool,
    /// High-SWR alarm (PATCH-swr-alarm): FTX-1 from RI P2; 991A now from RI0 P2
    /// (official Hi-SWR flag, FT-991A CAT OM 1711-D) instead of the ungated RM6
    /// threshold. Only meaningful during TX; clears when the flag drops (P2=0).
    pub hi_swr: bool,
    /// Diagnostic: last raw 991A RM6 SWR-meter value (000-255). Not an alarm source
    /// anymore — kept only so the RI0 Hi-SWR log can show the correlated RM6 reading
    /// (e.g. to confirm what a dummy load reads on RM6). Server-internal, not on the wire.
    pub swr_meter_raw: u16,
    /// Per-band max power from the 991A EX menus (EX137 HF, EX138 50M, EX139 144M,
    /// EX140 430M) in watts, read at connect. 0 = not read -> per-band default.
    /// The live max for the current band comes from the `tx_power_max()` method.
    pub max_pwr_hf: u8,
    pub max_pwr_50: u8,
    pub max_pwr_144: u8,
    pub max_pwr_430: u8,
    /// Auto-DATA PTT-toggle state: true when the current TX cycle temporarily uses a
    /// DATA mode because USB-mic TX does not modulate (well) in the normal mode.
    /// Only FM uses this path: FM('4')->DATA-FM('A').
    /// PTT-off restores the original mode (see auto_dfm_saved_mode).
    pub auto_dfm_active: bool,
    /// Original Yaesu mode char from before the auto-DATA switch; restored on
    /// PTT-off (MD0{char};). '4'=FM.
    pub auto_dfm_saved_mode: char,
    /// Saved memory channel at PTT-on when auto-DATA is active in Memory mode.
    /// 0 = not-in-memory or invalid; restore via MC<nnn>; after mode restore on PTT-off.
    pub auto_dfm_saved_memory_channel: u16,
    /// 991A SSB/AM USB routing: true = per-PTT switching (radio normal outside TX),
    /// false = presence-based (routing active while a client is connected). Restore
    /// returns to the session snapshot read at CAT connect; no factory-default assumption.
    /// FTX-1 keeps its internal auto source selection; set in new_with_model.
    pub ssb_switch_on_ptt: bool,
    /// What the radio itself says about transmitting, as opposed to what we asked
    /// for. `None` = the model gives no reliable answer (the FT-991A; its `TX;` was
    /// measured unreliable). The FTX-1 reports it in `RI` P4, which the fast poll
    /// already fetches, so this costs no extra CAT traffic.
    pub radio_tx: Option<bool>,
    /// Consecutive `RI` answers saying "not transmitting". A single garbled or stale
    /// frame must never drop a live transmission, so the watchdog waits for a run of
    /// them rather than acting on one.
    pub radio_rx_streak: u8,
    /// The radio's own TX time-out timer in minutes, from its EX menu (FT-991A 036,
    /// FTX-1 030112), read once when the radio connects. 0 = off.
    pub tot_minutes: u8,
    /// Whether an FTX-1 memory write is allowed at all - see `ftx1_memory_write_ack`
    /// in config.rs for what it costs. Never consulted for the FT-991A.
    pub ftx1_memory_write_ack: bool,
}

impl Default for YaesuState {
    fn default() -> Self {
        Self {
            connected: false,
            port_trouble: sdr_remote_core::protocol::PORT_TROUBLE_NONE,
            vfo_a_freq: 0,
            vfo_b_freq: 0,
            mode: 1, // USB default
            tx_active: false,
            smeter: 0,
            af_gain: 0,
            tx_power: 0,
            power_head: 0,
            squelch: 0,
            rf_gain: 0,
            mic_gain: 0,
            power_on: false,
            mode_char: '2',
            cat_monitor: false,
            vfo_select: 0,
            vfo_escape_pending: 0,
            memory_channel: 0,
            filled_memory_channels: Vec::new(),
            last_memory_blob: None,
            last_menu_blob: None,
            split_active: false,
            scan_active: false,
            tuner_state: 0, // off/bypass until AC; readback says otherwise
            feature_toggles: 0,
            feature_levels: [0u8; 16],
            feature_freqs: [0u16; 4],
            squelch_open: true, // open by default (no gating until RI says otherwise)
            hi_swr: false,
            swr_meter_raw: 0,
            max_pwr_hf: 0,
            max_pwr_50: 0,
            max_pwr_144: 0,
            max_pwr_430: 0,
            auto_dfm_active: false,
            auto_dfm_saved_mode: '4',
            auto_dfm_saved_memory_channel: 0,
            ssb_switch_on_ptt: true,
            radio_tx: None,
            radio_rx_streak: 0,
            tot_minutes: 0,
            ftx1_memory_write_ack: false,
        }
    }
}

impl YaesuState {
    /// Max TX power (watt) for the CURRENT band (PATCH-yaesu-power-scaling).
    /// 991A: from the read EX maxima (EX137-140) based on the VFO-A band; not-
    /// read or outside the amateur bands -> per-band default (the radio clamps its
    /// own maximum anyway, so an overly wide slider is never unsafe).
    /// FTX-1 (phase B): coarse head-max (field=10, Optima=100) until the EX mapping is tested.
    pub fn tx_power_max(&self) -> u8 {
        // FTX-1 field head: low power (5-10 W) on all bands.
        if self.power_head == 1 { return 10; }
        // 991A (head 0) AND FTX-1 Optima/base (head 2): per-band. 991A uses the
        // read EX values; the FTX-1 Optima falls back on the same per-band defaults
        // (HF/50M=100, 144/430=50) - the radio clamps its own max anyway.
        let mhz = self.vfo_a_freq as f64 / 1_000_000.0;
        if (1.8..=30.0).contains(&mhz) {
            if self.max_pwr_hf > 0 { self.max_pwr_hf } else { 100 }
        } else if (50.0..=54.0).contains(&mhz) {
            if self.max_pwr_50 > 0 { self.max_pwr_50 } else { 100 }
        } else if (144.0..=148.0).contains(&mhz) {
            if self.max_pwr_144 > 0 { self.max_pwr_144 } else { 50 }
        } else if (430.0..=450.0).contains(&mhz) {
            if self.max_pwr_430 > 0 { self.max_pwr_430 } else { 50 }
        } else {
            100 // outside ham band: full range, radio clamps itself
        }
    }
}

pub enum YaesuCmd {
    SetFreqA(u64),
    SetFreqB(u64),
    ReadAllMemories,
    /// Walk the channels that carry a tone and read each one's CTCSS/DCS value,
    /// then merge them into the cached memory blob. Explicit action: it steps
    /// the radio through those channels.
    ReadMemoryTones,
    RecallMemory(u16),  // MC command: select memory channel
    SelectVfo(u8),      // VS command: 0=VFO A, 1=VFO B, 2=swap
    RawCat(String),     // Send any CAT command string directly
    /// Typed DSP/function control (PATCH-yaesu-extra-controls): (control=YaesuCtrl u8,
    /// value). Encoded per-model in the command dispatch. Fase A1: RfAtt, BreakIn.
    SetFeature(u8, u16),
    WriteMemory {       // MW command: write a single memory channel
        channel: u16,
        freq_hz: u64,
        mode: u8,       // internal mode number
        ctcss: u8,      // 0=off, 1=enc/dec, 2=enc
        shift: u8,      // 0=simplex, 1=plus, 2=minus
    },
    WriteAllMemories(String), // tab-separated text with all channels
    ReadAllMenus,             // Read EX001-EX153 menu settings
    SetMenu(u16, String),     // Set EXnnn with P2 value
    SetMode(u8),       // Internal mode code
    SetPtt(bool),
    /// 991A SSB/AM USB TX routing on/off (opt-out/presence-based): true = switch to the
    /// 991A USB modulation source (SSB EX106/109, AM EX045/048), false = restore to
    /// the session snapshot read at CAT connect. FTX-1 is a no-op here.
    SetSsbRouting(bool),
    SetAfGain(u8),     // 0-255
    SetTxPower(u8),    // 0-100
    SetPower(bool),
}

/// Internal mode code for C4FM (Yaesu-specific; outside the Thetis/TS-2000 range
/// 0..11 so the client can show its own "C4FM" label instead of FM/USB).
pub const INTERNAL_C4FM: u8 = 12;

/// Map Yaesu MD0x mode digit to internal mode numbering (Thetis/TS-2000).
/// MODEL-DEPENDENT: the 991A and FTX-1 partly use different MD codes.
/// - CW is SWAPPED: 991A 3=CW-L/7=CW-R(CW-U); FTX-1 3=CW-U/7=CW-L.
/// - Extra modes: 991A E=C4FM; FTX-1 E=PSK, F=DATA-FM-N, H=C4FM-DN, I=C4FM-VW.
/// Internal: 0=LSB 1=USB 2=DSB 3=CW-L 4=CW-U 5=FM 6=AM 7=DIGU 8=SPEC 9=DIGL 10=SAM 11=DRM 12=C4FM
fn yaesu_mode_to_internal(yaesu: char, model: RadioModel) -> u8 {
    let ftx1 = matches!(model, RadioModel::Ftx1);
    match yaesu {
        '1' => 0,  // LSB
        '2' => 1,  // USB
        '3' => if ftx1 { 4 } else { 3 },  // FTX-1: CW-U ; 991A: CW -> CW-L
        '4' => 5,  // FM
        '5' => 6,  // AM
        '6' => 9,  // RTTY-LSB -> DIGL
        '7' => if ftx1 { 3 } else { 4 },  // FTX-1: CW-L ; 991A: CW-R -> CW-U
        '8' => 9,  // DATA-LSB -> DIGL
        '9' => 7,  // RTTY-USB -> DIGU
        'A' | 'a' => 5,  // DATA-FM -> FM
        'B' | 'b' => 5,  // FM-N -> FM
        'C' | 'c' => 7,  // DATA-USB -> DIGU
        'D' | 'd' => 6,  // AM-N -> AM (both)
        'E' | 'e' => if ftx1 { 7 } else { INTERNAL_C4FM },  // FTX-1: PSK -> DIGU ; 991A: C4FM
        'F' | 'f' => 5,  // FTX-1: DATA-FM-N -> FM
        'H' | 'h' | 'I' | 'i' => INTERNAL_C4FM,  // FTX-1: C4FM-DN / C4FM-VW
        _ => 1,    // default USB
    }
}

/// Map internal mode to Yaesu MD0x mode character (model-dependent, see above).
/// FM is sent as native FM ('4') for normal RX with built-in audio. The USB-mic
/// TX path switches at runtime temporarily to DATA-FM ('A') - see the SetPtt handler in
/// yaesu_poll_loop. An earlier implementation always forced DATA-FM; the operator test
/// 2026-05-08 showed that USB-mic audio in FM mode now works after the auto-toggle.
fn internal_mode_to_yaesu(internal: u8, model: RadioModel) -> char {
    let ftx1 = matches!(model, RadioModel::Ftx1);
    match internal {
        0 => '1',  // LSB
        1 => '2',  // USB
        3 => if ftx1 { '7' } else { '3' },  // CW-L : FTX-1 '7', 991A '3'
        4 => if ftx1 { '3' } else { '7' },  // CW-U : FTX-1 '3', 991A '7' (CW-R)
        5 => '4',  // FM -> FM (RX); auto-switch to 'A' (DATA-FM) at PTT-on, back at PTT-off
        6 => '5',  // AM
        7 => 'C',  // DIGU -> DATA-USB
        9 => '8',  // DIGL -> DATA-LSB
        INTERNAL_C4FM => if ftx1 { 'H' } else { 'E' },  // C4FM
        _ => '2',  // default USB
    }
}

impl YaesuRadio {
    /// Back-compat constructor: slot 0, FT-991A. Preserves the existing
    /// single-radio call path (ui/mod.rs) without requiring all callers to
    /// change. Slot 1 (FTX-1) uses `new_with_model`.
    pub fn new(port_name: &str, baud: u32, audio_device: Option<&str>) -> Result<Self, String> {
        Self::new_with_model(port_name, baud, audio_device, None, RadioModel::Ft991a, 0, 0, true, false)
    }

    pub fn new_with_model(
        port_name: &str,
        baud: u32,
        audio_device: Option<&str>,
        // Separate TX/output device (PATCH-yaesu-output-device). None -> use the input
        // `audio_device` pattern for output too (legacy behaviour). Set it when the
        // capture and render endpoints have different names, so TX audio can't fall
        // back onto the wrong USB codec.
        output_device: Option<&str>,
        model: RadioModel,
        slot: u8,
        capture_channel: u8,
        // 991A SSB/AM USB routing: true = per-PTT (radio normal outside TX), false =
        // presence-based routing. FTX-1 keeps its internal auto source selection.
        ssb_switch_on_ptt: bool,
        // FTX-1 only: permission to write its memory bank, which costs the tones
        // stored in the radio. Off unless the operator accepted that in the GUI.
        ftx1_memory_write_ack: bool,
    ) -> Result<Self, String> {
        let prefix = model.tag(slot);
        // Probe serial port (best-effort). If the Yaesu is off at server-start
        // the reconnect thread will retry silently in the background until the
        // radio appears - earlier behaviour was hard-fail here, which meant
        // powering up the Yaesu after the server was running required a full
        // server restart. Probe-open is just a courtesy log: drop immediately
        // and let the reconnect thread re-open in its own loop.
        let mut probe_trouble: Option<&'static str> = None;
        let initial_port_ok = match serialport::new(port_name, baud)
            .data_bits(serialport::DataBits::Eight)
            .stop_bits(serialport::StopBits::One)
            .parity(serialport::Parity::None)
            .flow_control(serialport::FlowControl::Hardware)
            .timeout(Duration::from_millis(100))
            .open()
        {
            Ok(port) => {
                drop(port);
                true
            }
            Err(e) => {
                probe_trouble = port_trouble_log_text(classify_open_error(&e));
                false
            }
        };

        let status = Arc::new(Mutex::new(YaesuState::default()));
        status.lock().unwrap().ssb_switch_on_ptt = ssb_switch_on_ptt;
        status.lock().unwrap().ftx1_memory_write_ack = ftx1_memory_write_ack;
        let model_shared = Arc::new(std::sync::atomic::AtomicU8::new(model.as_code()));
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (cmd_tx, cmd_rx) = mpsc::channel();

        // Create persistent audio RX channel (capture -> network loop)
        let (rx_audio_tx, rx_audio_rx) = tokio::sync::mpsc::channel::<Vec<f32>>(64);

        // Create persistent TX audio channel (network -> output)
        let (tx_audio_tx, tx_audio_rx) = tokio::sync::mpsc::channel::<Vec<f32>>(64);

        // Swappable cpal streams and ring buffer producer
        let capture_stream = Arc::new(StreamHolder::new(None));
        let output_stream = Arc::new(StreamHolder::new(None));
        let tx_producer: Arc<Mutex<Option<ringbuf::HeapProd<f32>>>> = Arc::new(Mutex::new(None));
        let last_audio_time = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let memory_data: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let menu_data: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        // Initial audio setup.
        //
        // Default rate is 48000 Hz because the Yaesu FT-991A USB Audio CODEC
        // always delivers that (input 48kHz/F32/1ch, output 48kHz/F32/2ch).
        // On cold-start (Yaesu off) the build_capture/output_stream below
        // fails and we stay on default 48000. That is needed so
        // the once-started `yaesu_audio_loop` (RX direction) and the
        // TX resampler in network.rs initialize with a valid sample rate
        // - not with 0, which leads to `frame_samples = 0` and a
        // divide-by-zero resampler ratio. Later reconnect
        // builds of the cpal streams always use 48000 so it
        // matches.
        let mut audio_rate = 48_000u32;
        let mut tx_rate = 48_000u32;
        if let Some(dev) = audio_device {
            // Capture (RX from Yaesu)
            // Seed audio timestamp so watchdog can detect if stream never starts
            let seed_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
            last_audio_time.store(seed_ms, std::sync::atomic::Ordering::Relaxed);
            match build_capture_stream(dev, rx_audio_tx.clone(), last_audio_time.clone(), &prefix, capture_channel) {
                Ok((stream, rate)) => {
                    capture_stream.set(Some(stream));
                    audio_rate = rate;
                }
                Err(e) => warn!("{} audio capture init failed: {}", prefix, e),
            }
            // Output (TX to Yaesu) - separate output device if configured, else the
            // input pattern (PATCH-yaesu-output-device).
            let out_dev = output_device.unwrap_or(dev);
            match build_output_stream(out_dev, tx_producer.clone(), &prefix) {
                Ok((stream, rate)) => {
                    output_stream.set(Some(stream));
                    tx_rate = rate;
                }
                Err(e) => warn!("{} audio output init failed: {}", prefix, e),
            }
        }

        // Start TX audio bridge thread: drains tx_audio_rx -> ring buffer producer
        {
            let producer = tx_producer.clone();
            let mut rx = tx_audio_rx;
            let prefix_tx = prefix.clone();
            std::thread::spawn(move || {
                let rt = match tokio::runtime::Runtime::new() {
                    Ok(rt) => rt,
                    Err(e) => {
                        log::error!("{} TX audio bridge: tokio runtime init failed: {} - TX-audio disabled, RX/CAT keep working", prefix_tx, e);
                        return;
                    }
                };
                rt.block_on(async {
                    while let Some(samples) = rx.recv().await {
                        if let Ok(ref mut guard) = producer.try_lock() {
                            if let Some(ref mut prod) = **guard {
                                use ringbuf::traits::Producer;
                                for &s in &samples {
                                    // Stereo: duplicate mono to both channels
                                    let _ = prod.try_push(s);
                                    let _ = prod.try_push(s);
                                }
                            }
                        }
                    }
                });
            });
        }

        if initial_port_ok {
            info!("{} serial probed OK on {} @ {} baud", prefix, port_name, baud);
        } else if let Some(text) = probe_trouble {
            // Not merely absent - the open failed for a nameable reason, and
            // that name is the operator's whole next move.
            warn!("{} {}: {}", prefix, port_name, text);
        } else {
            info!(
                "{} serial not detected on {} @ {} baud - background retry until radio comes online",
                prefix, port_name, baud
            );
        }

        // Start self-reconnecting serial + audio thread. The thread does the
        // real open (in a loop); the probe above was only a courtesy log so
        // operator sees immediately whether the radio is reachable. If the
        // probe failed the thread enters retry-mode silently.
        {
            let status = status.clone();
            let memory_data = memory_data.clone();
            let menu_data = menu_data.clone();
            let port_name = port_name.to_string();
            let audio_device = audio_device.map(|s| s.to_string());
            let output_device = output_device.map(|s| s.to_string());
            let rx_audio_tx = rx_audio_tx.clone();
            let capture_stream = capture_stream.clone();
            let output_stream = output_stream.clone();
            let tx_producer = tx_producer.clone();
            let last_audio_time_clone = last_audio_time.clone();
            let prefix = prefix.clone();
            let model_shared = model_shared.clone();
            let alive = alive.clone();
            // The loop's own sender, so the PTT watchdog can release through the
            // normal command path instead of a second copy of that logic.
            let self_tx = cmd_tx.clone();
            std::thread::spawn(move || {
                yaesu_reconnect_thread(
                    cmd_rx, self_tx, status, memory_data, menu_data,
                    port_name, baud, audio_device, output_device,
                    rx_audio_tx, capture_stream, output_stream, tx_producer,
                    last_audio_time_clone, model, model_shared, alive, slot, prefix, capture_channel,
                );
            });
        }

        Ok(Self {
            cmd_tx,
            status,
            _rx_audio_tx_keepalive: rx_audio_tx,
            audio_rx: Mutex::new(Some(rx_audio_rx)),
            audio_sample_rate: audio_rate,
            tx_audio_tx: Some(tx_audio_tx),
            tx_sample_rate: tx_rate,
            _capture_stream: capture_stream,
            _output_stream: output_stream,
            _last_audio_time: last_audio_time,
            _tx_producer: tx_producer,
            memory_data,
            menu_data,
            model_shared,
            slot,
            alive,
        })
    }

    /// The model as currently known: the configured/assumed one until the radio
    /// has answered `ID;`, the radio's own after that.
    pub fn model_code(&self) -> u8 {
        self.model_shared.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn send_command(&self, cmd: YaesuCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    /// Record a menu value WE just wrote, in the server's copy, and publish it.
    ///
    /// Without this the copy would be stale the moment ThetisLink changes an EX
    /// parameter itself, and only a full re-scan would repair it - seconds of
    /// occupied CAT for one changed value. The write knows what it changed, so the
    /// copy is patched directly. `key` is the line key as the scan writes it: three
    /// digits on the 991A ("017"), six on the FTX-1 ("010203").
    pub fn note_menu_value(&self, key: &str, value: &str) {
        let mut st = self.status.lock().unwrap();
        let Some(blob) = st.last_menu_blob.clone() else { return };
        let prefix = format!("{}:", key);
        let mut found = false;
        let mut out: Vec<String> = blob
            .lines()
            .map(|l| {
                if l.starts_with(&prefix) {
                    found = true;
                    format!("{}{}", prefix, value)
                } else {
                    l.to_string()
                }
            })
            .collect();
        if !found {
            out.push(format!("{}{}", prefix, value));
        }
        let merged = out.join("\n");
        st.last_menu_blob = Some(merged.clone());
        drop(st);
        *self.menu_data.lock().unwrap() = Some(format!("MENU:{}", merged));
    }

    pub fn status(&self) -> YaesuState {
        self.status.lock().unwrap().clone()
    }

    /// Shared live status (for the audio loop, which reads the squelch status
    /// for the software squelch). Clone of the Arc, not of the state.
    pub fn status_arc(&self) -> Arc<Mutex<YaesuState>> {
        self.status.clone()
    }
}

#[cfg(test)]
mod model_id_tests {
    use super::*;

    /// The two codes this build knows, and nothing else.
    #[test]
    fn only_a_known_id_names_a_model() {
        assert_eq!(RadioModel::from_id_code("0670"), Some(RadioModel::Ft991a));
        assert_eq!(RadioModel::from_id_code("0840"), Some(RadioModel::Ftx1));
    }

    /// An unknown or garbled code must stay unknown.
    ///
    /// It used to become an FTX-1 one call up, and that is how a station with a
    /// single FT-991A ended up being driven as an FTX-1: the first `ID;` after
    /// opening the port came back partial, the code was unrecognised, and the
    /// guess stuck for the whole session - no memory channels, no menu values,
    /// an IF frame parsed as gibberish, and a warning about it buried in the log.
    #[test]
    fn a_garbled_id_never_becomes_a_model() {
        for code in ["", "06", "0670;?", "ID0670", "xxxx", "0000", "?", "0841"] {
            assert_eq!(
                RadioModel::from_id_code(code),
                None,
                "{code:?} must not be read as a model"
            );
        }
    }

    /// Whitespace around a good code is still that code: a serial line adds
    /// stray characters more often than it drops them.
    #[test]
    fn a_known_code_survives_stray_whitespace() {
        assert_eq!(RadioModel::from_id_code(" 0670 "), Some(RadioModel::Ft991a));
    }
}

/// How many Yaesu slots currently hold an open CAT connection.
///
/// One number, process-wide, and it exists for one reason: a warning that says
/// what went wrong is worth little next to one that says WHY. Two slots on two
/// COM ports can still be one radio - a 991A's USB presents more than one port
/// and both accept CAT - and then the two pollers' answers interleave. Reads
/// come back holding somebody else's reply, memory channels are skipped, and
/// the time-out timer cannot be read. Every one of those looks like a
/// different fault in the log unless the common cause is named (2026-08-16,
/// from a report by a user running exactly this setup).
pub static ACTIVE_CAT_SLOTS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// The sentence appended to a symptom that two CAT talkers would explain.
/// Empty when only one slot is open, so a single-radio station never reads a
/// speculation about a second one.
pub fn two_slot_hint() -> &'static str {
    if ACTIVE_CAT_SLOTS.load(std::sync::atomic::Ordering::Relaxed) > 1 {
        " - NOTE: two radio slots hold a CAT connection. If both ports lead to the same radio (a 991A's USB offers more than one, and both accept CAT), their questions and answers interleave and this is the likely cause. Use one slot for CAT."
    } else {
        ""
    }
}
