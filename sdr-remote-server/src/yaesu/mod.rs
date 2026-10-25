// SPDX-License-Identifier: GPL-2.0-or-later

use std::collections::HashSet;
use sdr_remote_core::dev_info;
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
pub mod tx_timing;
pub mod line_ptt;
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

/// What the operator asked of the internal ATU. No catch-all arm in
/// `atu_cat`, so a third action cannot be added for one model and forgotten
/// for the other - the compiler asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtuAction {
    /// Bypass: the tuner is out of the line.
    Off,
    /// In line, with whatever it last found.
    On,
    /// Start a tuning cycle. THIS TRANSMITS - a carrier, for as long as the
    /// radio needs. Everything that guards a transmission guards this too.
    TuneNow,
}

impl RadioModel {
    /// The model behind a model code, for the places that hold only the code
    /// (the shared atomic). Written out by hand twice before (review 318 r2).
    pub fn from_code(code: u8) -> RadioModel {
        if code == RadioModel::Ftx1.as_code() {
            RadioModel::Ftx1
        } else {
            RadioModel::Ft991a
        }
    }

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

    /// Scan on/off, per model. The FTX-1's `SC` carries a MAIN/SUB prefix
    /// (P1) and a direction (P2): `SC01` is MAIN scan-up on, `SC00` is MAIN
    /// scan off, and without that P1 the radio ignores the command outright.
    /// The 991A has no prefix: `SC1` / `SC0`.
    ///
    /// Here for the same reason as `atu_cat`, and found the same way: it sat
    /// one line above the ATU, in the same two mirrored blocks, still keyed on
    /// the SLOT (review 2120A3, both reviewers). `parse.rs` already read the
    /// scan state per model, so the two halves of one feature disagreed about
    /// what a radio is.
    pub fn scan_cat(self, on: bool) -> &'static str {
        match (self, on) {
            (RadioModel::Ft991a, true) => "SC1;",
            (RadioModel::Ft991a, false) => "SC0;",
            (RadioModel::Ftx1, true) => "SC01;",
            (RadioModel::Ftx1, false) => "SC00;",
        }
    }

    /// The highest memory channel this model has. The 991A counts 001-117 -
    /// 001-099 normal and 100-117 PMS - and the FTX-1 stops at 99, with its
    /// PMS under text labels instead (`memory.rs` says so at the read).
    ///
    /// The step-to-the-next-filled-channel button took this from the slot:
    /// 117 in the block for radio 1 and 99 in the block for radio 2. An FTX-1
    /// in slot 0 would have been stepped up to channels it does not have.
    pub fn memory_channels(self) -> u16 {
        match self {
            RadioModel::Ft991a => 117,
            RadioModel::Ftx1 => 99,
        }
    }

    /// The internal ATU, per model and never per slot. The FT-991A writes
    /// `AC00P3` and the FTX-1 `AC10P3`: the FTX-1 reports its tuner with P1=1
    /// (`AC;` answers `AC100` when off) and ignores P1=0, so the two dialects
    /// are not interchangeable - `AC002` is undefined on an FTX-1.
    ///
    /// It lives here because it was written out twice in `network.rs`, once per
    /// SLOT: the block for radio 1 spoke 991A and the block for radio 2 spoke
    /// FTX-1. Put an FTX-1 in slot 0 (which the crossed-slot field run of
    /// 2026-09-20 did) and the Tune button sent it a command it does not have,
    /// while the 991A in slot 1 got the FTX-1's. `PATCH-yaesu-internal-atu.md`
    /// named this as an existing bug at build 11 and it stayed; a doc is not a
    /// mechanism (review 2120A2).
    ///
    /// Reference: `PATCH-yaesu-internal-atu.md` sec. 2, both measured on the
    /// radios themselves.
    pub fn atu_cat(self, action: AtuAction) -> &'static str {
        match (self, action) {
            (RadioModel::Ft991a, AtuAction::Off) => "AC000;",
            (RadioModel::Ft991a, AtuAction::On) => "AC001;",
            (RadioModel::Ft991a, AtuAction::TuneNow) => "AC002;",
            (RadioModel::Ftx1, AtuAction::Off) => "AC100;",
            (RadioModel::Ftx1, AtuAction::On) => "AC101;",
            (RadioModel::Ftx1, AtuAction::TuneNow) => "AC103;",
        }
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
    let mut empty = 0;

    // Outputs as well as inputs. It only counted the inputs, and the output is
    // the way TO the radio - the direction that carries the transmission.
    for (kind, devs) in [
        ("input", host.input_devices().map(|d| d.collect::<Vec<_>>())),
        ("output", host.output_devices().map(|d| d.collect::<Vec<_>>())),
    ] {
        match devs {
            Ok(devs) => {
                let names: Vec<String> = devs.iter().filter_map(|d| d.name().ok()).collect();
                if names.is_empty() {
                    empty += 1;
                    warn!("NO audio {} devices at all - Windows hands out an empty list", kind);
                } else {
                    info!("Available audio {} devices ({}): {:?}", kind, names.len(), names);
                }
            }
            Err(e) => warn!("Could not enumerate audio {} devices: {}", kind, e),
        }
    }

    // Said once, loudly, where it is read: with no devices at all, every later
    // "cannot find device X" is a consequence and not the fault.
    if empty > 0 {
        warn!(
            "The machine has no sound cards to offer. The usual cause is a Remote Desktop              session: while one is connected it takes them out of the session. Disconnect it,              or set its audio to play on the remote computer, and restart the server - this              list is only read at startup."
        );
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
/// Does this slot's audio device still belong to the radio that is in it?
///
/// A slot has TWO settings that both say which radio is here: the COM port and
/// the name of the audio device. Nothing made them agree. Swap two radios in
/// Settings - which the crossed-slot CW run of 2026-09-20 did - and the ports
/// move while the audio devices stay: CAT then follows the right radio (S-meter,
/// every control) and the sound comes from the other one. That is a hard fault
/// to find, because everything you touch answers correctly and only what you
/// hear is wrong. It cost half a diagnosis.
///
/// **Deliberately not done by device name.** "USB Audio CODEC" is the FT-991A on
/// this station, but those names come from Windows and carry an enumeration
/// prefix that moves with the USB socket. A table of names would be a new
/// assumption to keep in step with reality - the very thing this warning is
/// against. What the server does know for certain is which model was in this
/// slot last time (`*_model_last` in the config), so the question it asks is the
/// narrow one it can answer.
///
/// `None` = nothing to say. `Some(text)` = the line to put in the log and on the
/// screen.
///
/// Not caught by this, and it is worth saying: an audio device chosen wrongly by
/// hand on a fresh install. There is no "last time" to compare with then.
pub fn audio_device_warning(
    slot: u8,
    model_now: RadioModel,
    model_last: Option<RadioModel>,
    audio_device: Option<&str>,
) -> Option<String> {
    let last = model_last?;
    let device = audio_device?;
    if last == model_now {
        return None;
    }
    Some(format!(
        "[radio{}] this slot now holds a {} where a {} was, but its audio device is unchanged ({}).          The COM port and the audio device are two separate settings: if you swapped two radios,          the sound is still coming from the other one. Check the audio device of both radios.",
        slot,
        model_now.label(),
        last.label(),
        device
    ))
}

pub fn detect_model(port_name: &str, preferred_baud: u32) -> Option<(RadioModel, u32)> {
    detect_model_or_trouble(port_name, preferred_baud).ok()
}

/// `detect_model`, and when nothing was found, why: the `PORT_TROUBLE_*` class
/// of a port that could not be opened, or `PORT_TROUBLE_NONE` for a port that
/// opened and stayed silent. The settings probe shows it (build 271): a port
/// another program holds is not a radio that is switched off.
pub fn detect_model_or_trouble(port_name: &str, preferred_baud: u32) -> Result<(RadioModel, u32), u8> {
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
            // Hardware flow control, exactly as the serial thread opens it.
            // Without it the FT-991A's CAT port never answers: its RTS stays
            // down and the radio stays silent. That was invisible for as long as
            // this existed, because a failed detection falls back to assuming a
            // 991A - and on a 991A that guess is right. It surfaced the moment
            // the settings screen started asking the port honestly and reported
            // "nothing answered" on a radio that was on and working (2026-08-20).
            .flow_control(serialport::FlowControl::Hardware)
            .timeout(Duration::from_millis(100))
            .open()
        {
            Ok(p) => p,
            // Failing to open the port (e.g. already in use / does not exist) is not
            // baud-dependent -> trying further makes no sense. But say WHICH of
            // the two it was: "in use by another program" asks for a different
            // next move than "check the cable".
            Err(e) => {
                let trouble = classify_open_error(&e);
                if let Some(text) = port_trouble_log_text(trouble) {
                    log::warn!("{}: {}", port_name, text);
                }
                return Err(trouble);
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
            if let Some(model) = model_from_id_response(&resp) {
                return Ok((model, baud));
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
    // The silent path used to leave no trace at all, so a log could only show
    // this as the ABSENCE of a line - and the caller's fallback then made the
    // result look like a detection.
    log::warn!(
        "{}: opened, but nothing answered ID; on any supported baud rate -          the model cannot be detected and the caller will have to assume one",
        port_name
    );
    Err(sdr_remote_core::protocol::PORT_TROUBLE_NONE)
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

#[derive(Clone, Copy, Debug, PartialEq)]
enum Ft991aUsbRoutingScope {
    Ssb,
    Am,
    All,
}

impl Ft991aUsbRoutingSnapshot {
    /// Every menu read. A missing one is not put back at restore.
    fn is_complete(&self) -> bool {
        self.ssb_mic_select.is_some() && self.ssb_port_select.is_some() && self.am_mic_select.is_some() && self.am_port_select.is_some()
    }

    /// The routing writes for `scope`, but only for the menus whose original
    /// value is known. The rule that makes the snapshot safe: a menu is
    /// changed only if it can be put back. It is also what makes reading a
    /// missing menu later safe (`fill_missing`) - a menu never written still
    /// holds the operator's own value. (Review 315 round 2 proposed reading
    /// again while incomplete; without this rule that would have stored our
    /// own USB setting as the original of a menu that failed to read once.)
    fn routing_writes(&self, scope: Ft991aUsbRoutingScope) -> Vec<&'static [u8]> {
        let mut w: Vec<&'static [u8]> = Vec::new();
        if matches!(scope, Ft991aUsbRoutingScope::Ssb | Ft991aUsbRoutingScope::All) {
            if self.ssb_mic_select.is_some() {
                w.push(b"EX1061;"); // SSB MIC SELECT = REAR
            }
            if self.ssb_port_select.is_some() {
                w.push(b"EX1091;"); // SSB PORT SELECT = USB
            }
        }
        if matches!(scope, Ft991aUsbRoutingScope::Am | Ft991aUsbRoutingScope::All) {
            if self.am_port_select.is_some() {
                w.push(b"EX0481;"); // AM PORT SELECT = USB
            }
            if self.am_mic_select.is_some() {
                w.push(b"EX0451;"); // AM MIC SELECT = REAR
            }
        }
        w
    }

    /// Read the menus still missing - only those. Safe because of the rule in
    /// `routing_writes`: a missing menu was never written, so what it reads
    /// now is still the operator's value.
    fn fill_missing(&mut self, port: &mut Box<dyn serialport::SerialPort>, prefix: &str) {
        if self.ssb_mic_select.is_none() {
            self.ssb_mic_select = read_ex_menu_value(port, prefix, 106, "SSB MIC SELECT");
        }
        if self.ssb_port_select.is_none() {
            self.ssb_port_select = read_ex_menu_value(port, prefix, 109, "SSB PORT SELECT");
        }
        if self.am_mic_select.is_none() {
            self.am_mic_select = read_ex_menu_value(port, prefix, 45, "AM MIC SELECT");
        }
        if self.am_port_select.is_none() {
            self.am_port_select = read_ex_menu_value(port, prefix, 48, "AM PORT SELECT");
        }
    }

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

/// When a port that came back has to be put in a known state first.
///
/// Only when THIS side had the radio keyed. A radio worked locally with its own
/// microphone is none of our business, and forcing RX there would cut the
/// operator off mid-sentence.
pub(crate) fn release_on_reopen(ptt_commanded: bool) -> bool {
    ptt_commanded
}

/// When a port that has stayed away means the radio has stopped by itself.
///
/// The radio's own TX time-out is the only brake left once the cable is gone,
/// and after it has run there is nothing to release - the transmission ended,
/// unheard, because the audio rides on that same USB.
///
/// **A time-out of zero is the operator switching that brake off**, and then
/// this cannot fire: the radio really can keep transmitting until somebody
/// walks over to it. Saying "it has stopped" there would be a guess dressed as
/// a fact. That condition was in the code and in no test, which is how a
/// safety net quietly stops being one (review finding).
pub(crate) fn release_on_timeout(
    ptt_commanded: bool,
    tot_minutes: u8,
    waited: std::time::Duration,
) -> bool {
    ptt_commanded
        && tot_minutes > 0
        && waited >= std::time::Duration::from_secs(u64::from(tot_minutes) * 60)
}

#[cfg(test)]
mod safety_release_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_port_that_returns_releases_only_what_we_were_keying() {
        assert!(release_on_reopen(true));
        assert!(!release_on_reopen(false));
    }

    #[test]
    fn the_time_out_has_to_have_actually_run() {
        let tot = 10;
        assert!(!release_on_timeout(true, tot, Duration::from_secs(9 * 60)));
        assert!(release_on_timeout(true, tot, Duration::from_secs(10 * 60)));
        assert!(release_on_timeout(true, tot, Duration::from_secs(30 * 60)));
    }

    /// One minute is the lowest setting the radio offers, and it is the one
    /// this whole subject was measured with. Every other test here uses ten or
    /// zero, so `tot_minutes > 0` could quietly become `> 1` and the net would
    /// disappear for exactly that setting - in the way this file exists to stop
    /// (review finding).
    #[test]
    fn one_minute_is_a_time_out_like_any_other() {
        assert!(!release_on_timeout(true, 1, Duration::from_secs(59)));
        assert!(release_on_timeout(true, 1, Duration::from_secs(60)));
        assert!(release_on_timeout(true, 5, Duration::from_secs(5 * 60)));
        // And the top of the menu, so the tested range covers what the radio
        // actually offers instead of stopping at the ten this was measured with.
        assert!(!release_on_timeout(true, 30, Duration::from_secs(29 * 60)));
        assert!(release_on_timeout(true, 30, Duration::from_secs(30 * 60)));
    }

    #[test]
    fn a_radio_we_were_not_keying_is_left_alone() {
        assert!(!release_on_timeout(false, 10, Duration::from_secs(60 * 60)));
    }

    /// The condition the whole safety net leans on, and it was written down
    /// nowhere. With the brake switched off on the radio there is no moment at
    /// which we may claim it has stopped - not after ten minutes, not after an
    /// hour.
    #[test]
    fn a_radio_with_its_time_out_switched_off_is_never_assumed_to_have_stopped() {
        // One case, not three: `tot_minutes > 0` short-circuits before the
        // clock is read, so a longer wait walks the same path. Written as a
        // range it only looked thorough (review finding).
        assert!(
            !release_on_timeout(true, 0, Duration::from_secs(100 * 3600)),
            "claimed the radio stopped after a hundred hours with no time-out set"
        );
    }
}

#[derive(Clone, Debug)]
pub struct YaesuState {
    pub connected: bool,
    /// The transmit chain's timing lines (build 163, `tx_timing`).
    pub tx_timing: tx_timing::TxTiming,
    /// Why the serial port cannot be opened, when it cannot - one of the
    /// `PORT_TROUBLE_*` codes from the protocol. Set by the reconnect loop on
    /// every failed open, cleared on a successful one. This is what turns the
    /// most common field problem - other CAT software still holding the port
    /// in the background - from a silent absence into a named one.
    pub port_trouble: u8,
    /// What the CAT link has been doing wrong since it last behaved.
    ///
    /// Lives here rather than in the poll loop because the loop returns on the
    /// first stalled write and is restarted; a counter inside it can never
    /// reach two, and two is what tells a busy port from a wrong baud rate.
    pub cat_trouble: sdr_remote_core::cat_health::CatTrouble,
    pub vfo_a_freq: u64,
    pub vfo_b_freq: u64,
    pub mode: u8,           // Internal mode (0=LSB, 1=USB, etc. - Thetis numbering)
    pub tx_active: bool,
    /// The last PTT state ThetisLink asked THIS radio for.
    ///
    /// Not `tx_active`, which is what the radio reports and which the 991A was
    /// measured to get wrong. This is our own side of the conversation, and it
    /// is the only thing that can answer "was this transmission ours" after the
    /// cable is gone.
    ///
    /// It lives here rather than in the poll loop for the same reason
    /// `cat_trouble` does: that loop returns on a stalled write and is
    /// restarted, so anything it holds is lost exactly when the port dies -
    /// which is the moment this has to survive.
    pub ptt_commanded: bool,
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
    /// Multi-state/level values indexed by YaesuCtrl (Phase B: Agc(6), PreAmp(7); Phase C: Nb(8),Dnr(9),Processor(10),Amc(11)).
    pub feature_levels: [u8; 16],
    /// Phase D frequency values (u16): [0]=Contour, [1]=APF, [2]=Notch.
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
    /// How often this server has let go of the PTT on its own initiative.
    ///
    /// Bumped in `poll.rs` when the radio stopped transmitting by itself or its
    /// TX time-out timer is about to fire. A counter and not a flag, so the
    /// network layer cannot miss one between two reads and cannot see the same
    /// one twice.
    ///
    /// It exists because unkeying the radio was only half the job: the session
    /// still had the client down as the holder, and the client still believed
    /// it was transmitting. Nothing told it. That is the whole of the complaint
    /// from 2026-08-07 and it survived five attempts at fixing it on the wrong
    /// side (2026-09-05).
    pub auto_release: u32,
    /// Whether an FTX-1 memory write is allowed at all - see `ftx1_memory_write_ack`
    /// in config.rs for what it costs. Never consulted for the FT-991A.
    pub ftx1_memory_write_ack: bool,
    /// The operator's choice of FTX-1 capture channel (0=L, 1=R, 2=mix). Held
    /// here rather than frozen into the audio thread, so that a radio which
    /// adopts its real model late still gets the right one - see
    /// `effective_capture_channel`.
    pub audio_channel: u8,
    /// The value menu 060 PC KEYING had when a CW run of this server first
    /// wrote RTS over it (CW design step 6); `None` while we have not written
    /// it. Kept here, not in the poll loop, so a reconnect does not read our
    /// own RTS back as the original. Put back at exit (RestoreBeforeExit).
    pub cw_pc_keying_original: Option<String>,
    /// The menus that key this radio on RTS or DTR of its USB port, from the
    /// menu blob (build 296; `line_ptt`). Set only by `line_ptt::refresh`.
    pub line_keyed: Vec<line_ptt::LineKeyed>,
    /// When a CW run of this server last keyed this radio (its key line), or
    /// `None`. Set when a CW packet is admitted, kept fresh by the safety tick
    /// while the run holds the radio, and cleared a second after it no longer
    /// does (`cw_tick`). The radio's own TX report does not see this keying.
    pub cw_keyed_at: Option<std::time::Instant>,
    /// The serial thread is stepping through memory channels (reading tones,
    /// writing memories). Set and cleared only by `ChannelWalk`.
    pub walking_channels: bool,
    /// `YAESU_LOADING_*` bits: what the serial thread is still reading from
    /// the radio (or is reading now). Written by the poll loop only.
    pub loading: u8,
}

impl Default for YaesuState {
    fn default() -> Self {
        Self {
            connected: false,
            tx_timing: tx_timing::TxTiming::default(),
            port_trouble: sdr_remote_core::protocol::PORT_TROUBLE_NONE,
            cat_trouble: Default::default(),
            vfo_a_freq: 0,
            vfo_b_freq: 0,
            mode: 1, // USB default
            tx_active: false,
            ptt_commanded: false,
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
            auto_release: 0,
            ftx1_memory_write_ack: false,
            audio_channel: 2,
            cw_pc_keying_original: None,
            line_keyed: Vec::new(),
            cw_keyed_at: None,
            walking_channels: false,
            loading: 0,
        }
    }
}

/// How long a CW mark outlives the run that set it: covers a packet admitted
/// but not yet played, which the playout does not hold yet.
pub const CW_KEYED_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

impl YaesuState {
    /// The thread reconnected and the radio on the other end turned out to be
    /// a different model (`bringup_probe`). Everything here was read from the
    /// PREVIOUS radio and is not refreshed until the new one has been read, so
    /// until then it is another radio's answer wearing this one's name.
    ///
    /// One list, next to the fields, because build 150 cleared exactly one of
    /// them - the PC KEYING original - and left the rest (review 2120A2,
    /// the review). The worst of the rest is `tot_minutes`: the PTT safety net uses
    /// it as a deadline, so until the new scan finishes the server would hold
    /// this radio to the transmit time-out of the one that was unplugged.
    ///
    /// NOT in here, on purpose: everything the poll loop refreshes within a
    /// second or two - frequency, mode, S-meter, the flags. Those are stale for
    /// one round, which no one can act on. What is in here is read once per
    /// connect and then kept.
    pub fn forget_previous_radio(&mut self) {
        self.cw_pc_keying_original = None;
        self.last_memory_blob = None;
        self.last_menu_blob = None;
        self.line_keyed.clear();
        self.filled_memory_channels.clear();
        self.tot_minutes = 0;
        self.max_pwr_hf = 0;
        self.power_head = 0;
    }

    /// Is this radio on the air by any account the server has: what the radio
    /// reports (`tx_active`, which the 991A gets wrong), what ThetisLink asked
    /// it for (`ptt_commanded`), or a CW run keying its key line.
    ///
    /// Everything that moves the radio - recalling memory channels, writing
    /// them - asks this. Until build 131 they asked `tx_active` only, and the
    /// connect-time tone read walked the 991A's channels while a CW run keyed
    /// it (field test 2026-09-19 14:53: the paddle held through a server
    /// restart; the radio ended on channel 3 in FM).
    pub fn on_the_air(&self) -> bool {
        self.tx_active || self.ptt_commanded || self.cw_keyed_at.is_some()
    }

    /// Is this radio transmitting, as the server reports it to clients and to
    /// the PA guard: what the radio says, or a CW run of this server keying it.
    /// The 991A reports RTS keying late or not at all (field test 2026-09-19:
    /// keyed at 15:33:13.4, `TX: ON` at 17.65), so the client's CW sign and
    /// the PA guard saw nothing for seconds. Not `ptt_commanded`: the PA guard
    /// has the PTT flags of its own, and a client wants the radio's word on a
    /// microphone transmission.
    pub fn reported_tx(&self) -> bool {
        self.connected && (self.tx_active || self.cw_keyed_at.is_some())
    }

    /// Why this radio may not START transmitting now, if so. While the serial
    /// thread reads it after connecting, everything for the radio waits in its
    /// queue - a PTT-off too, and for the FTX-1's 405 menu items that is some
    /// 40 s: a transmission started then could not be stopped (found preparing
    /// review 318). So no PTT and no CW begins while it loads; the other way
    /// round, no long read begins while it transmits (`on_the_air`, poll.rs).
    /// Letting go is never refused.
    pub fn tx_start_refusal(&self) -> Option<&'static str> {
        sdr_remote_core::protocol::yaesu_loading_blocks_tx(self.loading)
            .then_some("the radio is still loading its settings")
    }

    /// A CW packet asks to key this radio. Refused while the serial thread is
    /// stepping through channels; otherwise marked as keyed. Under the same
    /// lock as `ChannelWalk::begin`, so the two cannot both start.
    pub fn begin_cw_keying(&mut self, now: std::time::Instant) -> bool {
        if self.walking_channels {
            return false;
        }
        self.cw_keyed_at = Some(now);
        true
    }

    /// The safety tick: does a CW run hold this radio now? While it does the
    /// mark stays fresh; once it does not, the mark comes off after
    /// `CW_KEYED_GRACE`.
    pub fn cw_tick(&mut self, run_holds_this: bool, now: std::time::Instant) {
        if run_holds_this {
            self.cw_keyed_at = Some(now);
        } else if let Some(t) = self.cw_keyed_at {
            if now.saturating_duration_since(t) >= CW_KEYED_GRACE {
                self.cw_keyed_at = None;
            }
        }
    }

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
    SelectVfo(u8),      // VS command: 0=VFO A, 1=VFO B, 2=swap, 3=V/M toggle
    RawCat(String),     // Send any CAT command string directly
    /// Typed DSP/function control (PATCH-yaesu-extra-controls): (control=YaesuCtrl u8,
    /// value). Encoded per-model in the command dispatch. Phase A1: RfAtt, BreakIn.
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
    /// The server is stopping: put back what this session changed on the radio
    /// (today the 991A's USB transmit routing), then answer on the channel.
    /// See `YaesuRadio::restore_before_exit` for why this is a command with
    /// an answer and not something that happens when the radio is dropped.
    RestoreBeforeExit(std::sync::mpsc::Sender<()>),
    /// A CW run is about to key this radio through PC KEYING (CW design step
    /// 6): read menu 060, remember its original, set it to RTS. Answers Ok, or
    /// the reason it could not - then nothing was changed.
    PrepareCwKeying(std::sync::mpsc::Sender<Result<(), String>>),
}

/// What the end of the server needs from a radio. A trait so the order of
/// the steps can be tested without a serial port.
pub trait ExitRadio {
    fn exit_slot(&self) -> u8;
    /// Is this radio keyed by us right now?
    fn transmitting(&self) -> bool;
    /// Key it off, through the ordinary PTT-off.
    fn unkey(&self);
    /// Put back what the session changed; true when the radio confirmed.
    fn put_back(&self, wait: std::time::Duration) -> bool;
}

impl ExitRadio for YaesuRadio {
    fn exit_slot(&self) -> u8 {
        self.slot
    }
    fn transmitting(&self) -> bool {
        self.status().ptt_commanded
    }
    fn unkey(&self) {
        self.send_command(YaesuCmd::SetPtt(false));
    }
    fn put_back(&self, wait: std::time::Duration) -> bool {
        self.restore_before_exit(wait)
    }
}

/// The last thing the server does with its radios. Two passes: every radio
/// that is transmitting is unkeyed first, and only then is each one put back
/// and waited for - one radio that does not answer must not keep another on
/// the air for the length of its wait (review 315). A Yaesu is keyed by
/// a CAT command, so one left keyed at exit transmits until its own time-out,
/// which on a 991A is often off.
pub fn put_back_before_exit(radios: &[&dyn ExitRadio], wait: std::time::Duration) {
    for r in radios {
        if r.transmitting() {
            r.unkey();
            warn!("[radio{}] was transmitting at exit - PTT off first", r.exit_slot());
        }
    }
    for r in radios {
        if r.put_back(wait) {
            info!("[radio{}] put back before exit - confirmed", r.exit_slot());
        } else {
            warn!("[radio{}] put back before exit - not confirmed within {} ms (radio off, or its serial thread busy with a long read?)", r.exit_slot(), wait.as_millis());
        }
    }
}

#[cfg(test)]
mod routing_snapshot_tests {
    use super::{Ft991aUsbRoutingScope, Ft991aUsbRoutingSnapshot};

    /// A menu is written only if its original is known: that is what lets a
    /// missing one be read later, and what keeps every change restorable.
    #[test]
    fn only_a_menu_whose_original_is_known_is_changed() {
        let full = || Ft991aUsbRoutingSnapshot {
            ssb_mic_select: Some("0".into()),
            ssb_port_select: Some("0".into()),
            am_mic_select: Some("0".into()),
            am_port_select: Some("0".into()),
        };
        let all: Vec<&[u8]> = vec![b"EX1061;", b"EX1091;", b"EX0481;", b"EX0451;"];
        assert_eq!(full().routing_writes(Ft991aUsbRoutingScope::All), all, "the same menus, in the same order, as before");
        assert_eq!(full().routing_writes(Ft991aUsbRoutingScope::Ssb), vec![b"EX1061;" as &[u8], b"EX1091;"]);
        assert_eq!(full().routing_writes(Ft991aUsbRoutingScope::Am), vec![b"EX0481;" as &[u8], b"EX0451;"]);
        let asleep = Ft991aUsbRoutingSnapshot { ssb_mic_select: None, ssb_port_select: None, am_mic_select: None, am_port_select: None };
        assert!(asleep.routing_writes(Ft991aUsbRoutingScope::All).is_empty(), "a radio that did not answer is not changed at all");
        let one_missing = Ft991aUsbRoutingSnapshot { ssb_port_select: None, ..full() };
        assert_eq!(one_missing.routing_writes(Ft991aUsbRoutingScope::All), vec![b"EX1061;" as &[u8], b"EX0481;", b"EX0451;"]);
        let mic_missing = Ft991aUsbRoutingSnapshot { ssb_mic_select: None, ..full() };
        assert_eq!(mic_missing.routing_writes(Ft991aUsbRoutingScope::Ssb), vec![b"EX1091;" as &[u8]]);
        let am_port_missing = Ft991aUsbRoutingSnapshot { am_port_select: None, ..full() };
        assert_eq!(am_port_missing.routing_writes(Ft991aUsbRoutingScope::Am), vec![b"EX0451;" as &[u8]]);
        let am_mic_missing = Ft991aUsbRoutingSnapshot { am_mic_select: None, ..full() };
        assert_eq!(am_mic_missing.routing_writes(Ft991aUsbRoutingScope::Am), vec![b"EX0481;" as &[u8]]);
    }

    /// A snapshot missing any one menu is incomplete: that menu would not be
    /// put back, and a late snapshot is read again once before it is kept.
    #[test]
    fn a_snapshot_is_complete_only_with_all_four_menus() {
        let full = || Ft991aUsbRoutingSnapshot {
            ssb_mic_select: Some("0".into()),
            ssb_port_select: Some("0".into()),
            am_mic_select: Some("0".into()),
            am_port_select: Some("0".into()),
        };
        assert!(full().is_complete());
        let mut s = full();
        s.ssb_mic_select = None;
        assert!(!s.is_complete());
        let mut s = full();
        s.ssb_port_select = None;
        assert!(!s.is_complete());
        let mut s = full();
        s.am_mic_select = None;
        assert!(!s.is_complete());
        let mut s = full();
        s.am_port_select = None;
        assert!(!s.is_complete());
    }
}

#[cfg(test)]
mod exit_order_tests {
    use super::{put_back_before_exit, ExitRadio};
    use std::cell::RefCell;
    use std::time::Duration;

    struct Fake<'a> {
        slot: u8,
        keyed: bool,
        answers: bool,
        log: &'a RefCell<Vec<String>>,
    }
    impl ExitRadio for Fake<'_> {
        fn exit_slot(&self) -> u8 {
            self.slot
        }
        fn transmitting(&self) -> bool {
            self.keyed
        }
        fn unkey(&self) {
            self.log.borrow_mut().push(format!("unkey {}", self.slot));
        }
        fn put_back(&self, wait: Duration) -> bool {
            self.log.borrow_mut().push(format!("put back {} ({} ms)", self.slot, wait.as_millis()));
            self.answers
        }
    }

    #[test]
    fn every_transmitting_radio_is_unkeyed_before_any_is_put_back() {
        let log = RefCell::new(Vec::new());
        let r0 = Fake { slot: 0, keyed: false, answers: false, log: &log }; // off, will not answer
        let r1 = Fake { slot: 1, keyed: true, answers: true, log: &log };
        put_back_before_exit(&[&r0, &r1], Duration::from_millis(1));
        assert_eq!(*log.borrow(), vec!["unkey 1", "put back 0 (1 ms)", "put back 1 (1 ms)"]);
    }

    /// Every radio is put back, not only until one confirms, and each gets
    /// the wait it was given.
    #[test]
    fn every_radio_is_put_back_with_the_wait_given() {
        let log = RefCell::new(Vec::new());
        let r0 = Fake { slot: 0, keyed: true, answers: true, log: &log };
        let r1 = Fake { slot: 1, keyed: true, answers: true, log: &log };
        put_back_before_exit(&[&r0, &r1], Duration::from_millis(1000));
        assert_eq!(*log.borrow(), vec!["unkey 0", "unkey 1", "put back 0 (1000 ms)", "put back 1 (1000 ms)"]);
    }

    #[test]
    fn a_radio_that_is_not_keyed_is_not_sent_a_ptt_off() {
        let log = RefCell::new(Vec::new());
        let r0 = Fake { slot: 0, keyed: false, answers: true, log: &log };
        put_back_before_exit(&[&r0], Duration::from_millis(1));
        assert_eq!(*log.borrow(), vec!["put back 0 (1 ms)"]);
    }
}

/// Send a request that carries its own answer channel, and wait up to `wait`
/// for the answer. False when the request could not be sent, or no answer
/// came in time - a radio that is off, or a serial thread that is between
/// connections, never answers, and the caller says so in the log.
pub(crate) fn ask_and_wait(send: impl FnOnce(std::sync::mpsc::Sender<()>) -> bool, wait: std::time::Duration) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    if !send(tx) {
        return false;
    }
    rx.recv_timeout(wait).is_ok()
}

#[cfg(test)]
mod ask_and_wait_tests {
    use super::ask_and_wait;
    use std::time::{Duration, Instant};

    #[test]
    fn an_answer_in_time_is_a_yes_silence_or_no_listener_is_a_no() {
        assert!(ask_and_wait(|ack| ack.send(()).is_ok(), Duration::from_millis(200)), "answered at once");
        let t = Instant::now();
        assert!(!ask_and_wait(|ack| { std::mem::forget(ack); true }, Duration::from_millis(50)), "nobody answers");
        let waited = t.elapsed();
        assert!(waited >= Duration::from_millis(50), "and it waited the time it was given");
        assert!(waited < Duration::from_secs(1), "and no longer: in headless mode this is the only brake on shutdown ({waited:?})");
        let t = Instant::now();
        // The request could not be sent, and the answer channel is still held
        // somewhere: no wait at all - not the five seconds (review 315: the
        // early return was only equivalent for today's caller).
        assert!(!ask_and_wait(|ack| { std::mem::forget(ack); false }, Duration::from_secs(5)));
        assert!(t.elapsed() < Duration::from_secs(1), "no wait for a request that never left ({:?})", t.elapsed());
        assert!(!ask_and_wait(|ack| { drop(ack); true }, Duration::from_millis(50)), "the thread went away without answering");
    }
}

/// Internal mode code for C4FM (Yaesu-specific; outside the Thetis/TS-2000 range
/// 0..11 so the client can show its own "C4FM" label instead of FM/USB).
pub const INTERNAL_C4FM: u8 = 12;

/// Map Yaesu MD0x mode digit to internal mode numbering (Thetis/TS-2000).
/// MODEL-DEPENDENT, but NOT for CW - that was the bug.
///
/// The code used to claim the two radios swap CW, and mapped the 991A's '3' to
/// CW-L. They do not swap: both use '3' for the normal CW and '7' for the reversed
/// one, and the FTX-1 manual simply spells the names out where the 991A uses the
/// older ones. Straight from the two CAT books in docs/internal/referentie:
///
///   FTX-1 (MD, p.20):  3: CW-U    7: CW-L
///   991A  (IF/MR P6):   3: CW      7: CW-R
///
/// On a Yaesu the plain "CW" is the upper-sideband one and "CW-R" the reverse, so
/// the two tables say the same thing. Confirmed on the radio: selecting CW-LSB on
/// a 991A - which is '7' - showed CW-U in the client, the wrong half of the pair.
/// - Extra modes: 991A E=C4FM; FTX-1 E=PSK, F=DATA-FM-N, H=C4FM-DN, I=C4FM-VW.
/// Internal: 0=LSB 1=USB 2=DSB 3=CW-L 4=CW-U 5=FM 6=AM 7=DIGU 8=SPEC 9=DIGL 10=SAM 11=DRM 12=C4FM
fn yaesu_mode_to_internal(yaesu: char, model: RadioModel) -> u8 {
    let ftx1 = matches!(model, RadioModel::Ftx1);
    match yaesu {
        '1' => 0,  // LSB
        '2' => 1,  // USB
        '3' => 4,  // CW-U (FTX-1 "CW-U", 991A "CW") - same on both
        '4' => 5,  // FM
        '5' => 6,  // AM
        '6' => 9,  // RTTY-LSB -> DIGL
        '7' => 3,  // CW-L (FTX-1 "CW-L", 991A "CW-R") - same on both
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
        3 => '7',  // CW-L -> the reversed one on both models
        4 => '3',  // CW-U -> the normal one on both models
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
        // This slot's capture-channel choice. NOT necessarily the channel this
        // radio will use: whether it means anything follows the model, and is
        // worked out per stream build.
        audio_channel: u8,
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
        status.lock().unwrap().audio_channel = audio_channel;
        status.lock().unwrap().tx_timing = tx_timing::TxTiming::new(slot);
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
            let capture_channel = crate::yaesu::poll::effective_capture_channel(model, &status);
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
            // The sound card counts its gaps for the radio's over line (build 263).
            let gaps = status.lock().unwrap().tx_timing.output_gaps();
            match build_output_stream(out_dev, tx_producer.clone(), gaps, &prefix) {
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
            // Transmit audio waits for TX1 (build 164, `tx_timing::Hold`).
            let timing = status.lock().unwrap().tx_timing.clone();
            let hold_max = (tx_rate as u64 * tx_timing::HOLD_MAX_MS / 1000) as usize;
            let mut hold = tx_timing::Hold::default();
            let mut unwritten = tx_timing::OverTally::default();
            let mut over_output = tx_timing::OverOutput::default();
            let mut lead_start = tx_timing::LeadStart::default();
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
                        let taken = hold.take(samples, timing.keyed(), timing.over(), hold_max);
                        if let Some(n) = hold.take_report() {
                            // Named in advance (working rule 11, build 184).
                            info!(
                                "{} TX audio: {} ms held while the radio was not keyed thrown away (previous over)",
                                timing.label(),
                                n as u64 * 1000 / tx_rate.max(1) as u64
                            );
                        }
                        let Some((out, held)) = taken else {
                            continue;
                        };
                        if held > 0 {
                            timing.held(held as u64 * 1000 / tx_rate.max(1) as u64);
                        }
                        let samples = out;
                        // The over's lead (build 278): zeros ahead of its first
                        // audio, so the target stands in the ring at once. 0 at
                        // target 0.
                        let pad = lead_start.pad(timing.over(), (timing.lead_ms() * tx_rate as u64 / 1000) as usize, samples.len());
                        // What reaches the sound card, in mono samples: a busy
                        // lock, no output, or a full ring loses the rest (build
                        // 185; review of 184).
                        let mut written = 0usize;
                        let mut queued_ms = 0u64;
                        if let Ok(ref mut guard) = producer.try_lock() {
                            if let Some(ref mut prod) = **guard {
                                use ringbuf::traits::{Observer, Producer};
                                for _ in 0..pad {
                                    let _ = prod.try_push(0.0);
                                    let _ = prod.try_push(0.0);
                                }
                                for &s in &samples {
                                    // Stereo: duplicate mono to both channels
                                    if prod.try_push(s).is_ok() {
                                        written += 1;
                                    }
                                    let _ = prod.try_push(s);
                                }
                                queued_ms = tx_timing::queued_ms(prod.occupied_len(), prod.capacity().get());
                            }
                        }
                        // The over line of the sound card, at the first block
                        // of the next over (build 263).
                        if written > 0 {
                            if let Some(summary) = over_output.written(timing.over(), crate::ptt::turnaround_clock(), queued_ms, timing.lead_ms()) {
                                let (frames, gaps) = timing.take_gaps();
                                // Named in advance (working rule 11, builds 263 and 278).
                                dev_info!(
                                    "{}",
                                    tx_timing::output_line(timing.label(), frames * 1000 / tx_rate.max(1) as u64, gaps, &summary)
                                );
                            }
                        }
                        if let Some(n) = unwritten.add(timing.over(), samples.len() - written) {
                            // Named in advance (working rule 11, build 185).
                            info!(
                                "{} TX audio: {} ms not written to the sound card (previous over)",
                                timing.label(),
                                n as u64 * 1000 / tx_rate.max(1) as u64
                            );
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
                    last_audio_time_clone, model, model_shared, alive, slot, prefix,
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

    /// Ask the serial thread to put back what this session changed on the
    /// radio, and wait up to `wait` for it to say it has. Returns whether it
    /// did.
    ///
    /// Why a command with an answer: the restore used to hang on the command
    /// channel closing when the radio was dropped. It never closes - the
    /// serial thread holds a sender of its own, for the PTT watchdog (commit
    /// cc67ff42) - and the thread's `alive` check returns without a restore.
    /// So the Exit button, written for exactly this (a 991A left in USB mode
    /// after shutdown), restored nothing: "command channel closed" appears in
    /// no server log (found 2026-09-18, building CW step 6, which needs the
    /// same thing for menu 060).
    /// Ask the serial thread to make the radio ready for CW keying through
    /// PC KEYING (menu 060 to RTS), and wait up to `wait` for the answer.
    pub fn prepare_cw_keying(&self, wait: std::time::Duration) -> Result<(), String> {
        let (tx, rx) = std::sync::mpsc::channel();
        if self.cmd_tx.send(YaesuCmd::PrepareCwKeying(tx)).is_err() {
            return Err("the radio's serial thread is gone".into());
        }
        rx.recv_timeout(wait).unwrap_or_else(|_| {
            // Not "radio off or port away": the commonest cause by far is a
            // serial thread in the middle of a long read - a menu batch of the
            // FTX-1 scan held the line for ~2.6 s (since build 284 one attempt,
            // ~0.7 s - but a long read it still is). Naming only the two causes
            // that need a cable sent people to the shack for a queue.
            Err(format!(
                "the radio did not answer within {} ms - it may be off, its port away, or its serial thread busy with a long read",
                wait.as_millis()
            ))
        })
    }

    pub fn restore_before_exit(&self, wait: std::time::Duration) -> bool {
        ask_and_wait(|ack| self.cmd_tx.send(YaesuCmd::RestoreBeforeExit(ack)).is_ok(), wait)
    }

    /// Write one EX/menu setting, in the CAT dialect THIS radio speaks.
    ///
    /// The 991A addresses its menu by number (`EX012...`), the FTX-1 by a
    /// six-digit address (`EX010316...`). Which of the two applies is a property
    /// of the radio, never of the slot it happens to sit in - and that is
    /// precisely what was wrong: both packet handlers decided by slot, so slot 0
    /// always spoke 991A and slot 1 always spoke FTX-1. Put an FTX-1 in slot 1
    /// and no EX setting could be written at all; a 991A in slot 2 likewise. The
    /// reading side had this right long ago (`poll.rs` dispatches on
    /// `RadioModel`); the writing side had not (2026-08-20).
    ///
    /// Returns the key as it will appear in a log line, or why it was refused.
    pub fn set_menu_entry(&self, key: &str, value: &str) -> Result<String, String> {
        let key = key.trim();
        if RadioModel::from_code(self.model_code()) == RadioModel::Ftx1 {
            if key.len() != 6 || !key.chars().all(|c| c.is_ascii_digit()) {
                return Err(format!(
                    "'{}' is not a six-digit FTX-1 EX address",
                    key
                ));
            }
            self.send_command(YaesuCmd::RawCat(format!("EX{}{};", key, value)));
            // Six-digit key here, three on the 991A - the scan writes them that
            // way, so the server's copy is patched with the same key.
            self.note_menu_value(key, value);
            Ok(format!("EX {}", key))
        } else {
            // A six-digit FTX-1 address would parse as a number here and then
            // format into a malformed three-digit EX command, so the range is
            // checked as well as the parse.
            match key.parse::<u16>() {
                Ok(num) if (1..=999).contains(&num) => {
                    self.send_command(YaesuCmd::SetMenu(num, value.to_string()));
                    self.note_menu_value(&format!("{:03}", num), value);
                    Ok(format!("menu {:03}", num))
                }
                _ => Err(format!("'{}' is not a menu number this radio has", key)),
            }
        }
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
        // The radio's TX time-out, and ThetisLink lets go of the PTT just
        // before it fires. Changed here, it has to follow (review 318;
        // owner's decision) - but only to a value the radio takes
        // (memory::tot_after_menu_write, review 318 round 2).
        let model = RadioModel::from_code(self.model_code());
        match crate::yaesu::memory::tot_after_menu_write(model, key, value) {
            crate::yaesu::memory::TotAfterWrite::Set(tot) if tot != st.tot_minutes => {
                info!("[{}] TX time-out timer is now {} min (menu changed)", model.label(), tot);
                st.tot_minutes = tot;
            }
            crate::yaesu::memory::TotAfterWrite::NotTaken => {
                warn!(
                    "[{}] TX time-out written as '{}' - not the two digits 00-30 the radio takes; still {} min",
                    model.label(),
                    value,
                    st.tot_minutes
                );
            }
            _ => {}
        }
        line_ptt::refresh(&mut st, model, &format!("[{}]", model.label()), line_ptt::MenuRead::OneValue);
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

/// A walk through memory channels, on the serial thread. Only one kind of
/// thing may happen to a radio at a time: stepping it through channels, or
/// keying it. `begin` refuses while the radio is on the air
/// (`YaesuState::on_the_air`), and while the walk lasts a CW packet is refused
/// (`begin_cw_keying`). Dropping the walk ends it - also when the serial
/// thread returns half-way, which it does on a stalled write.
pub struct ChannelWalk {
    status: Arc<Mutex<YaesuState>>,
}

impl ChannelWalk {
    pub fn begin(status: &Arc<Mutex<YaesuState>>) -> Option<ChannelWalk> {
        let mut st = status.lock().unwrap_or_else(|e| e.into_inner());
        if st.on_the_air() || st.walking_channels {
            return None;
        }
        st.walking_channels = true;
        Some(ChannelWalk { status: status.clone() })
    }
}

impl Drop for ChannelWalk {
    fn drop(&mut self) {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).walking_channels = false;
    }
}

#[cfg(test)]
mod channel_walk_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// The grace covers the gap between an admitted packet and the playout
    /// holding the radio (the playout delay plus one safety tick), and not
    /// much more: a minute would keep a radio "on the air" - no tone read, a
    /// red CW button, a PA guard seeing RF - long after the run (review 317).
    #[test]
    fn the_grace_covers_the_gap_and_not_much_more() {
        let gap = crate::cw_playout::PLAYOUT_DELAY + Duration::from_millis(100);
        assert!(CW_KEYED_GRACE > gap, "shorter than the gap it covers");
        assert!(CW_KEYED_GRACE <= Duration::from_secs(2), "outlives the run by more than it has to");
    }

    #[test]
    fn a_radio_that_loads_starts_no_transmission() {
        let mut s = YaesuState::default();
        assert_eq!(s.tx_start_refusal(), None);
        s.loading = sdr_remote_core::protocol::YAESU_LOADING_TONES;
        assert!(s.tx_start_refusal().is_some(), "walking its tones: a PTT-off would wait behind it");
        s.loading = sdr_remote_core::protocol::YAESU_LOADING_MENUS;
        assert_eq!(s.tx_start_refusal(), None, "the menus load in the gaps: no wait worth refusing for");
        s.loading = 0;
        assert_eq!(s.tx_start_refusal(), None, "done: free again");
    }

    #[test]
    fn the_reported_tx_includes_our_cw_keying() {
        let mut s = YaesuState::default();
        s.connected = true;
        assert!(!s.reported_tx());
        assert!(s.begin_cw_keying(Instant::now()));
        assert!(s.reported_tx(), "keyed on RTS: the radio has not said so yet");
        s.connected = false;
        assert!(!s.reported_tx(), "a radio that dropped out reports nothing");
        s.connected = true;
        s.cw_keyed_at = None;
        s.ptt_commanded = true;
        assert!(!s.reported_tx(), "a microphone transmission is the radio's word");
        s.tx_active = true;
        assert!(s.reported_tx());
    }

    #[test]
    fn a_radio_is_on_the_air_by_any_account() {
        let mut s = YaesuState::default();
        assert!(!s.on_the_air());
        s.tx_active = true;
        assert!(s.on_the_air(), "what the radio reports");
        s.tx_active = false;
        s.ptt_commanded = true;
        assert!(s.on_the_air(), "what we asked it for");
        s.ptt_commanded = false;
        assert!(s.begin_cw_keying(Instant::now()));
        assert!(s.on_the_air(), "a CW run on its key line - the 991A does not report that");
    }

    /// The field test of 2026-09-19 14:53, both ways round.
    #[test]
    fn keying_and_walking_the_channels_exclude_each_other() {
        let status = Arc::new(Mutex::new(YaesuState::default()));
        let now = Instant::now();
        // A CW run keys the radio: no walk starts.
        assert!(status.lock().unwrap().begin_cw_keying(now));
        assert!(ChannelWalk::begin(&status).is_none(), "the tone read must not walk a keyed radio");
        // The run ends; after the grace the walk may start.
        status.lock().unwrap().cw_tick(false, now + Duration::from_millis(500));
        assert!(ChannelWalk::begin(&status).is_none(), "within the grace");
        status.lock().unwrap().cw_tick(false, now + CW_KEYED_GRACE);
        let walk = ChannelWalk::begin(&status).expect("the run is over");
        // While it walks, CW is refused, and nothing is marked.
        assert!(!status.lock().unwrap().begin_cw_keying(now + Duration::from_secs(2)));
        assert!(status.lock().unwrap().cw_keyed_at.is_none());
        assert!(ChannelWalk::begin(&status).is_none(), "one walk at a time");
        // Dropping the walk ends it (also on an early return).
        drop(walk);
        assert!(!status.lock().unwrap().walking_channels);
        assert!(status.lock().unwrap().begin_cw_keying(now + Duration::from_secs(3)));
    }

    /// A model change leaves nothing of the radio that was there before. The
    /// one that matters most is the transmit time-out: the PTT safety net uses
    /// it as a deadline, so an FTX-1 replaced by a 991A would be held to the
    /// other radio's number until the new read finishes.
    #[test]
    fn a_new_model_keeps_nothing_of_the_old_radio() {
        let mut s = YaesuState::default();
        s.cw_pc_keying_original = Some("2".to_string());
        s.last_memory_blob = Some("MT001...".to_string());
        s.last_menu_blob = Some("EX...".to_string());
        s.filled_memory_channels = vec![1, 2, 3];
        s.tot_minutes = 10;
        s.max_pwr_hf = 100;
        s.power_head = 3;
        // And something the poll loop refreshes, which must NOT be cleared:
        // blanking it would make the screen flicker for no reason.
        s.vfo_a_freq = 14_070_000;
        s.mode = 3;

        s.forget_previous_radio();

        assert_eq!(s.cw_pc_keying_original, None);
        assert_eq!(s.last_memory_blob, None);
        assert_eq!(s.last_menu_blob, None);
        assert!(s.filled_memory_channels.is_empty());
        assert_eq!(s.tot_minutes, 0, "the other radio's transmit time-out is not this radio's deadline");
        assert_eq!((s.max_pwr_hf, s.power_head), (0, 0));
        assert_eq!((s.vfo_a_freq, s.mode), (14_070_000, 3), "what the poll refreshes is left alone");
    }

    /// The case that produced this warning: two radios swapped in Settings, so
    /// each slot holds the other model while its audio device stayed behind.
    #[test]
    fn a_slot_that_changed_model_and_kept_its_audio_says_so() {
        use super::{audio_device_warning, RadioModel};
        let w = audio_device_warning(
            0,
            RadioModel::Ftx1,
            Some(RadioModel::Ft991a),
            Some("Microfoon (USB Audio CODEC )"),
        );
        let w = w.expect("a slot that changed model has something to say about its audio");
        assert!(w.contains("[radio0]"), "it names the slot");
        assert!(w.contains("USB Audio CODEC"), "and the device that stayed behind");

        // The ordinary case: same radio as last time, nothing to say.
        assert_eq!(
            audio_device_warning(0, RadioModel::Ftx1, Some(RadioModel::Ftx1), Some("whatever")),
            None,
            "the same radio as last time is not a warning"
        );
        // A first start has nothing to compare with, and a slot without an
        // audio device has nothing to be wrong about. Neither may cry wolf.
        assert_eq!(audio_device_warning(0, RadioModel::Ftx1, None, Some("whatever")), None);
        assert_eq!(audio_device_warning(0, RadioModel::Ftx1, Some(RadioModel::Ft991a), None), None);
    }

    /// Scan and the memory range follow the model too. They sat one line above
    /// the ATU in the same two blocks and were still keyed on the slot (review
    /// 2120A3, both reviewers) - so the repair of build 156 had walked past
    /// them.
    #[test]
    fn scan_and_the_memory_range_follow_the_model_as_well() {
        use super::RadioModel;
        assert_eq!(RadioModel::Ft991a.scan_cat(true), "SC1;");
        assert_eq!(RadioModel::Ft991a.scan_cat(false), "SC0;");
        assert_eq!(RadioModel::Ftx1.scan_cat(true), "SC01;", "without P1 the FTX-1 ignores it outright");
        assert_eq!(RadioModel::Ftx1.scan_cat(false), "SC00;");
        assert_ne!(
            RadioModel::Ft991a.scan_cat(true),
            RadioModel::Ftx1.scan_cat(true),
            "one of them would be getting the other's"
        );
        assert_ne!(RadioModel::Ftx1.scan_cat(true), RadioModel::Ftx1.scan_cat(false), "on is not off");

        assert_eq!(RadioModel::Ft991a.memory_channels(), 117, "001-099 plus PMS 100-117");
        assert_eq!(RadioModel::Ftx1.memory_channels(), 99, "its PMS is under text labels");
    }

    /// Each radio gets its own ATU dialect, and the two do not overlap. They
    /// used to be chosen by SLOT, in two mirrored blocks in network.rs: put an
    /// FTX-1 in slot 0 and the Tune button sent it `AC002`, which that radio
    /// does not have (review 2120A2; named as a known bug in
    /// PATCH-yaesu-internal-atu since build 11 and left there).
    #[test]
    fn the_atu_speaks_the_models_dialect_and_not_the_slots() {
        use super::{AtuAction, RadioModel};
        let a991 = [
            RadioModel::Ft991a.atu_cat(AtuAction::Off),
            RadioModel::Ft991a.atu_cat(AtuAction::On),
            RadioModel::Ft991a.atu_cat(AtuAction::TuneNow),
        ];
        let ftx1 = [
            RadioModel::Ftx1.atu_cat(AtuAction::Off),
            RadioModel::Ftx1.atu_cat(AtuAction::On),
            RadioModel::Ftx1.atu_cat(AtuAction::TuneNow),
        ];
        assert_eq!(a991, ["AC000;", "AC001;", "AC002;"], "the 991A writes AC00P3");
        assert_eq!(ftx1, ["AC100;", "AC101;", "AC103;"], "the FTX-1 writes AC10P3 - P1=0 it ignores");
        for c in a991 {
            assert!(!ftx1.contains(&c), "{c} is sent to both models, so one of them is getting the other's");
        }
        // And the action a model is asked for is the action it gets: swapping
        // two rows inside one model would pass the two asserts above.
        assert_ne!(RadioModel::Ftx1.atu_cat(AtuAction::On), RadioModel::Ftx1.atu_cat(AtuAction::TuneNow));
        assert_ne!(RadioModel::Ft991a.atu_cat(AtuAction::On), RadioModel::Ft991a.atu_cat(AtuAction::TuneNow));
    }

    #[test]
    fn the_safety_tick_keeps_the_mark_while_the_run_holds_the_radio() {
        let mut s = YaesuState::default();
        let t0 = Instant::now();
        assert!(s.begin_cw_keying(t0));
        // A long run: the tick refreshes the mark, so it outlives the grace.
        s.cw_tick(true, t0 + Duration::from_secs(10));
        s.cw_tick(false, t0 + Duration::from_secs(10) + CW_KEYED_GRACE - Duration::from_millis(1));
        assert!(s.on_the_air());
        s.cw_tick(false, t0 + Duration::from_secs(10) + CW_KEYED_GRACE);
        assert!(!s.on_the_air());
        // A tick with no mark and no run leaves it off.
        s.cw_tick(false, t0 + Duration::from_secs(20));
        assert!(s.cw_keyed_at.is_none());
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

    /// A whole `ID;` answer, read frame by frame - what both readers use.
    #[test]
    fn an_id_answer_names_its_model_even_with_frames_run_together() {
        // The field log of 2026-09-29: two answers at once. Used to name nothing.
        assert_eq!(model_from_id_response("ID0840;ID0840;"), Some(RadioModel::Ftx1));
        // The ordinary answer, and one with its terminator lost.
        assert_eq!(model_from_id_response("ID0670;"), Some(RadioModel::Ft991a));
        assert_eq!(model_from_id_response("ID0840"), Some(RadioModel::Ftx1));
        // A station log: the ID answer, then the radio's "?" for the next query.
        assert_eq!(model_from_id_response("ID0670;?;"), Some(RadioModel::Ft991a));
        // A stray frame before the ID answer.
        assert_eq!(model_from_id_response("?;ID0670;"), Some(RadioModel::Ft991a));
        // The "ID" echo lost on a partial first read: the bare code still counts,
        // as it did when the answer was read as one payload.
        assert_eq!(model_from_id_response("0840;"), Some(RadioModel::Ftx1));
    }

    /// An answer without a known ID frame still names no model - the out-of-step
    /// read keeps its warning, and an unknown radio is not guessed.
    #[test]
    fn an_id_answer_without_a_known_id_frame_names_nothing() {
        // "MD04;0840;": a bare code after the first frame is some other command's
        // payload - the old read named nothing there either (review build 55).
        for resp in ["", ";", "?;", "ID0841;", "ID;", "007077100;MD04;TX0;AG0041;", "IF0840;", "08400;", "MD04;0840;"] {
            assert_eq!(model_from_id_response(resp), None, "{resp:?} must not name a model");
        }
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

#[cfg(test)]
mod cw_mode_tests {
    use super::*;

    /// The CW pair, straight out of the two CAT books, for both radios.
    ///
    /// It was swapped for the 991A: the code claimed the two models mirror each other
    /// and mapped '3' to CW-L. They do not mirror. Both use '3' for the normal CW and
    /// '7' for the reversed one; the FTX-1 manual writes the sideband into the name
    /// where the 991A uses the older "CW" and "CW-R".
    ///
    ///   FTX-1 (MD, p.20):  3: CW-U    7: CW-L
    ///   991A  (IF/MR P6):   3: CW      7: CW-R
    ///
    /// Internal numbering is Thetis's DSPMode: CWL = 3, CWU = 4.
    #[test]
    fn both_radios_agree_on_which_cw_is_which() {
        for model in [RadioModel::Ft991a, RadioModel::Ftx1] {
            assert_eq!(
                yaesu_mode_to_internal('3', model),
                4,
                "the normal CW is the upper one, on both radios"
            );
            assert_eq!(
                yaesu_mode_to_internal('7', model),
                3,
                "the reversed CW is the lower one, on both radios"
            );
        }
    }

    /// Sending a mode has to arrive at the same place reading one does.
    ///
    /// Two functions, opposite directions, and nothing tied them together - so one
    /// could be corrected without the other and the radio would end up in a mode the
    /// client did not ask for.
    #[test]
    fn reading_a_cw_mode_and_setting_it_agree() {
        for model in [RadioModel::Ft991a, RadioModel::Ftx1] {
            for internal in [3u8, 4u8] {
                let ch = internal_mode_to_yaesu(internal, model);
                assert_eq!(
                    yaesu_mode_to_internal(ch, model),
                    internal,
                    "round trip broke for internal mode {internal} on {model:?}"
                );
            }
        }
    }

    /// The FTX-1 was already right and must stay untouched.
    ///
    /// Verified on the radio by the owner on 2026-09-11, at the same time as the 991A
    /// fault. A fix to one model that quietly moves the other is the worst outcome
    /// here, and nothing else in the suite would have noticed.
    #[test]
    fn the_ftx1_mapping_is_unchanged() {
        let m = RadioModel::Ftx1;
        assert_eq!(yaesu_mode_to_internal('3', m), 4);
        assert_eq!(yaesu_mode_to_internal('7', m), 3);
        assert_eq!(internal_mode_to_yaesu(3, m), '7');
        assert_eq!(internal_mode_to_yaesu(4, m), '3');
    }

    /// And the modes that really do differ per model still do.
    ///
    /// Removing the CW conditional must not take the rest of the model handling with
    /// it: 'E' is C4FM on a 991A and PSK on an FTX-1, and C4FM goes out as a different
    /// character on each.
    #[test]
    fn the_modes_that_do_differ_still_differ() {
        assert_eq!(yaesu_mode_to_internal('E', RadioModel::Ft991a), INTERNAL_C4FM);
        assert_eq!(yaesu_mode_to_internal('E', RadioModel::Ftx1), 7);
        assert_eq!(internal_mode_to_yaesu(INTERNAL_C4FM, RadioModel::Ft991a), 'E');
        assert_eq!(internal_mode_to_yaesu(INTERNAL_C4FM, RadioModel::Ftx1), 'H');
    }
}
