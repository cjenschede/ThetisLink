// SPDX-License-Identifier: GPL-2.0-or-later

use std::collections::HashSet;
use std::io::{Read, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{info, warn};

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
    /// Short ASCII label for log prefixes (grep-baar, geen Unicode/tofu).
    pub fn label(self) -> &'static str {
        match self {
            RadioModel::Ft991a => "991A",
            RadioModel::Ftx1 => "FTX1",
        }
    }

    /// Per-radio log prefix `[radio{slot}/{MODEL}]`. Elke logregel
    /// in de slot-keten begint hiermee zodat `grep radio1` álle slot-1-events toont.
    pub fn tag(self, slot: u8) -> String {
        format!("[radio{}/{}]", slot, self.label())
    }

    /// Wire-code voor het `RadioInfo`-packet (server -> client, paneel-naamgeving):
    /// 0 = FT-991A, 1 = FTX-1. Spiegel-decode gebeurt client-side op de u8.
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
            // FTX-1 - live vastgelegd tijdens de operator-test 2026-06-14 (bring-up).
            "0840" => Some(RadioModel::Ftx1),
            _ => None,
        }
    }
}

/// Log alle beschikbare audio-input-devices (één keer bij startup). Helpt de
/// operator het juiste device per radio te kiezen (`yaesu_audio` / `yaesu2_audio`)
/// en maakt edge-case 6 zichtbaar (twee identiek genoemde "USB Audio CODEC").
pub fn log_input_devices() {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    match host.input_devices() {
        Ok(devs) => {
            let names: Vec<String> = devs.filter_map(|d| d.name().ok()).collect();
            info!("Beschikbare audio-input devices ({}): {:?}", names.len(), names);
        }
        Err(e) => warn!("Kon audio-input devices niet enumereren: {}", e),
    }
}

/// Probe a serial port to detect the radio model via the Yaesu `ID;` command,
/// met baud-fallback (PATCH-dual-radio-991a-ftx1 §2.3). Probeert `preferred_baud`
/// eerst, daarna de gangbare Yaesu-CAT-snelheden, tot een geldig `ID...;`-antwoord
/// komt. Geeft het gedetecteerde model + de baud die werkte terug.
///
/// **Model-toewijzing is per-poort, niet per-slot** - zo werkt élke combinatie:
/// 2× 991A, 2× FTX-1, of een mix. `ID=0670` -> FT-991A; elke andere geldige
/// Yaesu-`ID` -> FTX-1 via eliminatie (de exacte FTX-1-code is tot de eerste
/// bring-up onbekend en hoeft dat ook niet te zijn - de gedeelde parser werkt).
/// Geen antwoord op geen enkele baud -> `None`; de caller degradeert naar een
/// assumption label; the reconnect thread reads the real ID at startup.
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
            // Poort niet te openen (bv. al in gebruik / bestaat niet) is niet
            // baud-afhankelijk -> verder proberen heeft geen zin.
            Err(_) => return None,
        };
        let resp = cat_query(&mut port, "ID;");
        let code = resp_payload("ID", &resp);
        if resp.contains(';') && !code.is_empty() {
            let model = RadioModel::from_id_code(&code).unwrap_or(RadioModel::Ftx1);
            return Some((model, baud));
        }
        // Geen geldig antwoord op deze baud -> port wordt hier gedropt, volgende baud.
    }
    None
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
    /// Radio model + slot - drives the per-radio log prefix and per-model CAT quirks.
    pub model: RadioModel,
    pub slot: u8,
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
#[derive(Clone, Debug)]
pub struct YaesuState {
    pub connected: bool,
    pub vfo_a_freq: u64,
    pub vfo_b_freq: u64,
    pub mode: u8,           // Internal mode (0=LSB, 1=USB, etc. - Thetis numbering)
    pub tx_active: bool,
    pub smeter: u16,        // Raw S-meter value (0-255)
    pub af_gain: u8,        // 0-255
    pub tx_power: u8,       // 0-100
    /// FTX-1 power-head (uit `PC`-respons): 0 = geen/991A (`PC{nnn}`),
    /// 1 = field head (5-10W), 2 = SP A-1/Optima base (5-100W). Bepaalt het
    /// `PC`-zet-formaat zodat power op beide configs werkt.
    pub power_head: u8,
    pub squelch: u8,        // 0-255
    pub rf_gain: u8,        // 0-255
    pub mic_gain: u8,       // 0-100
    pub power_on: bool,
    pub mode_char: char,    // Raw Yaesu mode character ('1'-'E')
    pub vfo_select: u8,     // 0=VFO, 1=Memory, 2=MemTune (from IF P7)
    pub memory_channel: u16, // Current memory channel number (from IF)
    /// Sorted list of filled memory-channel numbers from the last full read.
    /// Persists independently of `memory_data` (which is consumed/taken when the
    /// dump is sent to the client), so Mem+/Mem- can skip empty channels.
    pub filled_memory_channels: Vec<u16>,
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
    /// Squelch open (BUSY) volgens de radio (FTX-1 `RI`-respons P8). True =
    /// signaal aanwezig / squelch open -> audio door. Default true (open) zodat
    /// radio's zonder RI (991A) of vóór de eerste poll nooit gegate worden.
    /// Drijft de server-side software-squelch op de FTX-1 USB-audio.
    pub squelch_open: bool,
    /// Auto-DATA PTT-toggle state: true wanneer de huidige TX-cyclus tijdelijk een
    /// DATA-mode gebruikt omdat USB-mic-TX in de gewone mode niet (goed) moduleert.
    /// Only FM uses this path: FM('4')->DATA-FM('A').
    /// PTT-off restores the original mode (see auto_dfm_saved_mode).
    pub auto_dfm_active: bool,
    /// Oorspronkelijke Yaesu-mode-char van vóór de auto-DATA-switch; hersteld op
    /// PTT-off (MD0{char};). '4'=FM.
    pub auto_dfm_saved_mode: char,
    /// Saved memory channel bij PTT-on als auto-DATA in Memory-mode actief is.
    /// 0 = niet-in-memory of ongeldig; restore via MC<nnn>; na mode-herstel op PTT-off.
    pub auto_dfm_saved_memory_channel: u16,
    /// 991A SSB/AM USB routing: true = per-PTT switching (radio normal outside TX),
    /// false = presence-based (routing active while a client is connected). Restore
    /// returns to the session snapshot read at CAT connect; no factory-default assumption.
    /// FTX-1 keeps its internal auto source selection; set in new_with_model.
    pub ssb_switch_on_ptt: bool,
}

impl Default for YaesuState {
    fn default() -> Self {
        Self {
            connected: false,
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
            vfo_select: 0,
            memory_channel: 0,
            filled_memory_channels: Vec::new(),
            split_active: false,
            scan_active: false,
            tuner_state: 0, // off/bypass until AC; readback says otherwise
            feature_toggles: 0,
            feature_levels: [0u8; 16],
            feature_freqs: [0u16; 4],
            squelch_open: true, // open by default (geen gating tot RI zegt anders)
            auto_dfm_active: false,
            auto_dfm_saved_mode: '4',
            auto_dfm_saved_memory_channel: 0,
            ssb_switch_on_ptt: true,
        }
    }
}

pub enum YaesuCmd {
    SetFreqA(u64),
    SetFreqB(u64),
    ReadAllMemories,
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

/// Map Yaesu MD0x mode digit to internal mode numbering (Thetis/TS-2000).
/// Yaesu: 1=LSB, 2=USB, 3=CW, 4=FM, 5=AM, 6=RTTY-LSB, 7=CW-R, 8=DATA-LSB, 9=RTTY-USB, A=DATA-FM, B=FM-N, C=DATA-USB
/// Internal: 0=LSB, 1=USB, 2=DSB, 3=CW-L, 4=CW-U, 5=FM, 6=AM, 7=DIGU, 8=SPEC, 9=DIGL, 10=SAM, 11=DRM
fn yaesu_mode_to_internal(yaesu: char) -> u8 {
    match yaesu {
        '1' => 0,  // LSB
        '2' => 1,  // USB
        '3' => 3,  // CW -> CW-L
        '4' => 5,  // FM
        '5' => 6,  // AM
        '6' => 9,  // RTTY-LSB -> DIGL
        '7' => 4,  // CW-R -> CW-U
        '8' => 9,  // DATA-LSB -> DIGL
        '9' => 7,  // RTTY-USB -> DIGU
        'A' | 'a' => 5,  // DATA-FM -> FM
        'B' | 'b' => 5,  // FM-N -> FM
        'C' | 'c' => 7,  // DATA-USB -> DIGU
        _ => 1,    // default USB
    }
}

/// Map internal mode to Yaesu MD0x mode character.
/// FM is sent as native FM ('4') for normal RX with built-in audio. USB-mic
/// TX-pad switcht runtime tijdelijk naar DATA-FM ('A') - zie SetPtt-handler in
/// yaesu_poll_loop. Eerdere implementatie forceerde DATA-FM altijd; operator-test
/// 2026-05-08 toonde dat USB-mic-audio in stand FM nu werkt na auto-toggle.
fn internal_mode_to_yaesu(internal: u8) -> char {
    match internal {
        0 => '1',  // LSB
        1 => '2',  // USB
        3 => '3',  // CW-L -> CW
        4 => '7',  // CW-U -> CW-R
        5 => '4',  // FM -> FM (RX); auto-switch naar 'A' (DATA-FM) bij PTT-on, terug bij PTT-off
        6 => '5',  // AM
        7 => 'C',  // DIGU -> DATA-USB
        9 => '8',  // DIGL -> DATA-LSB
        _ => '2',  // default USB
    }
}

impl YaesuRadio {
    /// Back-compat constructor: slot 0, FT-991A. Behoudt het bestaande
    /// single-radio call-pad (ui/mod.rs) zonder dat alle callers hoeven te
    /// wijzigen. Slot 1 (FTX-1) gebruikt `new_with_model`.
    pub fn new(port_name: &str, baud: u32, audio_device: Option<&str>) -> Result<Self, String> {
        Self::new_with_model(port_name, baud, audio_device, None, RadioModel::Ft991a, 0, 0, true)
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
    ) -> Result<Self, String> {
        let prefix = model.tag(slot);
        // Probe serial port (best-effort). If the Yaesu is off at server-start
        // the reconnect thread will retry silently in the background until the
        // radio appears - earlier behaviour was hard-fail here, which meant
        // powering up the Yaesu after the server was running required a full
        // server restart. Probe-open is just a courtesy log: drop immediately
        // and let the reconnect thread re-open in its own loop.
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
            Err(_) => false,
        };

        let status = Arc::new(Mutex::new(YaesuState::default()));
        status.lock().unwrap().ssb_switch_on_ptt = ssb_switch_on_ptt;
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

        // Initial audio setup.
        //
        // Default-rate is 48000 Hz omdat de Yaesu FT-991A USB Audio CODEC
        // dat altijd levert (input 48kHz/F32/1ch, output 48kHz/F32/2ch).
        // Bij cold-start (Yaesu uit) faalt de build_capture/output_stream
        // hieronder en blijven we op default 48000. Dat is nodig zodat
        // de eenmalig-gestarte `yaesu_audio_loop` (RX-richting) en de
        // TX-resampler in network.rs met een geldige sample-rate
        // initialiseren - niet met 0 wat tot `frame_samples = 0` en een
        // gedeeld-door-nul resampler-ratio leidt. Latere reconnect-
        // builds van de cpal streams gebruiken altijd 48000 zodat het
        // matched.
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
                        log::error!("{} TX audio bridge: tokio runtime init failed: {} - TX-audio disabled, RX/CAT blijven werken", prefix_tx, e);
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
            let port_name = port_name.to_string();
            let audio_device = audio_device.map(|s| s.to_string());
            let output_device = output_device.map(|s| s.to_string());
            let rx_audio_tx = rx_audio_tx.clone();
            let capture_stream = capture_stream.clone();
            let output_stream = output_stream.clone();
            let tx_producer = tx_producer.clone();
            let last_audio_time_clone = last_audio_time.clone();
            let prefix = prefix.clone();
            std::thread::spawn(move || {
                yaesu_reconnect_thread(
                    cmd_rx, status, memory_data,
                    port_name, baud, audio_device, output_device,
                    rx_audio_tx, capture_stream, output_stream, tx_producer,
                    last_audio_time_clone, model, prefix, capture_channel,
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
            memory_data: memory_data,
            model,
            slot,
        })
    }

    pub fn send_command(&self, cmd: YaesuCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn status(&self) -> YaesuState {
        self.status.lock().unwrap().clone()
    }

    /// Gedeelde live-status (voor de audio-loop, die de squelch-status leest
    /// voor de software-squelch). Clone van de Arc, niet van de state.
    pub fn status_arc(&self) -> Arc<Mutex<YaesuState>> {
        self.status.clone()
    }
}

/// Self-reconnecting thread: runs the serial poll loop, reconnects on failure.
fn yaesu_reconnect_thread(
    cmd_rx: mpsc::Receiver<YaesuCmd>,
    status: Arc<Mutex<YaesuState>>,
    memory_data: Arc<Mutex<Option<String>>>,
    port_name: String,
    baud: u32,
    audio_device: Option<String>,
    output_device: Option<String>,
    rx_audio_tx: tokio::sync::mpsc::Sender<Vec<f32>>,
    capture_stream: Arc<StreamHolder>,
    output_stream: Arc<StreamHolder>,
    tx_producer: Arc<Mutex<Option<ringbuf::HeapProd<f32>>>>,
    last_audio_time: Arc<std::sync::atomic::AtomicU64>,
    model: RadioModel,
    prefix: String,
    capture_channel: u8,
) {
    info!("{} serial thread started on {}", prefix, port_name);

    // Connection-state tracking, lokaal aan deze thread:
    //   `ever_connected` flipt naar true bij de eerste succesvolle open en
    //   bepaalt of een open-failure cold-start (silent) of mid-runtime
    //   disconnect (één warn) is.
    //   `disconnect_logged` dedupliceert de disconnect-warn zodat we niet
    //   elke 3 s log-spam genereren tijdens een langdurige outage.
    //   `first` triggert het wait/drain blok alleen ná de eerste iteratie
    //   (zodat de allereerste open-attempt geen 3 s wacht).
    let mut first = true;
    let mut ever_connected = false;
    let mut disconnect_logged = false;

    loop {
        if !first {
            // Drop oude audio streams (alleen na succesvolle connect zinvol -
            // tijdens cold-start retries is er niets om te droppen).
            if ever_connected {
                capture_stream.set(None);
                output_stream.set(None);
                *tx_producer.lock().unwrap() = None;
            }

            std::thread::sleep(Duration::from_secs(3));

            // Drain stale commands
            while cmd_rx.try_recv().is_ok() {}

            // Check if YaesuRadio was dropped (cmd channel disconnected)
            match cmd_rx.try_recv() {
                Err(mpsc::TryRecvError::Disconnected) => {
                    info!("{} command channel closed, stopping reconnect", prefix);
                    return;
                }
                _ => {}
            }
        }
        first = false;

        // Try to open serial port
        let mut port = match serialport::new(&port_name, baud)
            .data_bits(serialport::DataBits::Eight)
            .stop_bits(serialport::StopBits::One)
            .parity(serialport::Parity::None)
            .flow_control(serialport::FlowControl::Hardware)
            .timeout(Duration::from_millis(100))
            .open()
        {
            Ok(p) => p,
            Err(e) => {
                // Pre-connect (cold-start, Yaesu nog niet aan): stil retry,
                // geen log-spam per 3 s tick. Eén `debug!` voor wie met
                // RUST_LOG=debug debugt.
                // Post-connect (mid-runtime outage): één `warn!` bij het
                // eerste failed-open na de disconnect, daarna stil tot
                // re-connect of een nieuwe outage-cycle.
                if ever_connected && !disconnect_logged {
                    warn!("{} disconnected, retrying in background", prefix);
                    disconnect_logged = true;
                }
                log::debug!("{} open attempt failed: {}", prefix, e);
                continue;
            }
        };

        // Open succeeded - log de transitie en reset het dedup-flag voor de
        // volgende eventuele outage-cycle. Connect-regel bevat COM+baud zodat
        // operator-checklist item (a) direct grep-baar is.
        if ever_connected {
            info!("{} serial reconnected on {} @ {} baud", prefix, port_name, baud);
        } else {
            info!("{} serial connected on {} @ {} baud", prefix, port_name, baud);
            ever_connected = true;
        }
        disconnect_logged = false;

        // Bring-up probe: éénmalig ná elke succesvolle open de
        // ruwe ID;/IF;/MD0;/FA; dumpen + één parse-samenvatting. Maakt live
        // zichtbaar of de radio als 991A-structuur parseert of waar hij afwijkt.
        bringup_probe(&mut port, &prefix, model);

        // 991A SSB/AM USB TX routing is session-owned: snapshot only the menus
        // TL may temporarily change, then restore those exact values later. Do
        // not force a factory/default hand-mic state at connect; users may have
        // a custom normal setup (for example a USB microphone on the radio).
        let ft991a_usb_routing_snapshot = if !matches!(model, RadioModel::Ftx1) {
            Some(Ft991aUsbRoutingSnapshot::read(&mut port, &prefix))
        } else {
            None
        };

        // Rebuild audio streams onvoorwaardelijk na elke succesvolle open.
        // Bij cold-start (Yaesu was uit toen new() draaide) is het USB
        // audio device pas hier beschikbaar; bij mid-runtime reconnect kan
        // het device kort verdwenen zijn.
        //
        // De Yaesu FT-991A presenteert de capture- en output-kant van zijn
        // USB Audio CODEC als twee aparte cpal-devices die net niet
        // gelijktijdig enumerable worden. In de praktijk komt capture
        // ~100-300 ms eerder beschikbaar dan output; een back-to-back
        // build van eerst capture en dan output faalt dan met
        // "device is no longer available" op de output-kant. Daarom
        // retried de output-build hieronder een paar keer met korte
        // delay tussen pogingen.
        if let Some(ref dev) = audio_device {
            // Initial delay: USB audio device may appear after serial port
            std::thread::sleep(Duration::from_secs(1));

            match build_capture_stream(dev, rx_audio_tx.clone(), last_audio_time.clone(), &prefix, capture_channel) {
                Ok((stream, _rate)) => {
                    capture_stream.set(Some(stream));
                    info!("{} audio capture reconnected", prefix);
                }
                Err(e) => warn!("{} audio capture reconnect failed: {}", prefix, e),
            }
            // Output-stream retry-loop: tot 5 pogingen, 500 ms tussen elk.
            // Logt alleen de uiteindelijke status (ok of de laatste fout) -
            // tussenliggende attempts blijven op debug om server-log rust
            // te houden.
            let mut output_ok = false;
            let mut last_err: Option<String> = None;
            let out_dev = output_device.as_deref().unwrap_or(dev.as_str());
            for attempt in 1..=5 {
                match build_output_stream(out_dev, tx_producer.clone(), &prefix) {
                    Ok((stream, _rate)) => {
                        output_stream.set(Some(stream));
                        if attempt == 1 {
                            info!("{} audio output reconnected", prefix);
                        } else {
                            info!("{} audio output reconnected (attempt {})", prefix, attempt);
                        }
                        output_ok = true;
                        break;
                    }
                    Err(e) => {
                        log::debug!(
                            "{} audio output attempt {}/5 failed: {}",
                            prefix, attempt, e
                        );
                        last_err = Some(e.to_string());
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
            }
            if !output_ok {
                warn!(
                    "{} audio output open failed after 5 attempts: {} - blijft op de achtergrond elke 5s herproberen tot het apparaat vrij is",
                    prefix, last_err.unwrap_or_else(|| "unknown".to_string())
                );
            }
        }

        {
            let mut s = status.lock().unwrap();
            s.connected = true;
        }

        // Run poll loop until disconnect (with audio watchdog)
        yaesu_poll_loop(
            port, &cmd_rx, &status, &memory_data,
            &audio_device, &output_device, &rx_audio_tx, &capture_stream, &output_stream, &tx_producer, &last_audio_time,
            model, &prefix, capture_channel, ft991a_usb_routing_snapshot,
        );

        {
            let mut s = status.lock().unwrap();
            s.connected = false;
            s.power_on = false;
        }
    }
}

/// Inner serial polling loop. Returns when connection is lost or channel closes.
fn yaesu_poll_loop(
    mut port: Box<dyn serialport::SerialPort>,
    cmd_rx: &mpsc::Receiver<YaesuCmd>,
    status: &Arc<Mutex<YaesuState>>,
    memory_data: &Arc<Mutex<Option<String>>>,
    audio_device: &Option<String>,
    output_device: &Option<String>,
    rx_audio_tx: &tokio::sync::mpsc::Sender<Vec<f32>>,
    capture_stream: &Arc<StreamHolder>,
    output_stream: &Arc<StreamHolder>,
    tx_producer: &Arc<Mutex<Option<ringbuf::HeapProd<f32>>>>,
    last_audio_time: &Arc<std::sync::atomic::AtomicU64>,
    model: RadioModel,
    prefix: &str,
    capture_channel: u8,
    ft991a_usb_routing_snapshot: Option<Ft991aUsbRoutingSnapshot>,
) {
    let mut read_buf = String::new();
    let mut raw_buf = [0u8; 256];
    let mut last_full_poll = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    let mut last_smeter_poll = Instant::now();
    let mut last_response = Instant::now();
    let mut last_output_retry = Instant::now();
    // Warn-once guards: voorkomen 500 ms-poll log-spam terwijl ze
    // de huidige stille defaults wegnemen. `warned_modes` = onbekende MD-codes
    // (één warn per uniek teken); `warned_short_if` = afwijkende IF-lengte (één warn).
    let mut warned_modes: HashSet<char> = HashSet::new();
    let mut warned_short_if = false;

    loop {
        // Read available serial data
        match port.read(&mut raw_buf) {
            Ok(n) if n > 0 => {
                if let Ok(s) = std::str::from_utf8(&raw_buf[..n]) {
                    read_buf.push_str(s);
                    last_response = Instant::now();
                }
            }
            Ok(_) => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                warn!("{} serial read error: {}", prefix, e);
                return;
            }
        }

        // Detect unresponsive radio (e.g. power supply removed while USB still connected).
        // Baud-hint in de regel: operator ziet meteen of een stille radio
        // een baud-mismatch radio-menu vs config kan zijn.
        if last_response.elapsed().as_secs() >= 5 {
            warn!("{} no CAT response for 5s - disconnecting (controleer baud radio-menu vs config)", prefix);
            return;
        }

        // Parse complete responses (terminated by ';')
        parse_responses(&mut read_buf, status, prefix, model, &mut warned_modes, &mut warned_short_if);

        // Handle commands from the application
        match cmd_rx.try_recv() {
            Ok(YaesuCmd::ReadAllMemories) => {
                info!("{} reading all memory channels...", prefix);
                let mem_result = match model {
                    // FTX-1 splitst freq (MR) en naam (MT), 5-cijferige kanalen.
                    RadioModel::Ftx1 => read_all_memories_ftx1(&mut port),
                    _ => read_all_memories(&mut port),
                };
                match mem_result {
                    Ok(tab_text) => {
                        let count = tab_text.lines().count() - 1;
                        info!("{} read {} memory channels", prefix, count);
                        // Persist the filled channel numbers (first column) so
                        // Mem+/Mem- can skip empties - memory_data itself is taken
                        // when sent to the client.
                        let mut filled: Vec<u16> = tab_text
                            .lines()
                            .skip(1)
                            .filter_map(|l| l.split('\t').next())
                            .filter_map(|c| c.trim().parse::<u16>().ok())
                            .filter(|&c| c >= 1)
                            .collect();
                        filled.sort_unstable();
                        filled.dedup();
                        status.lock().unwrap().filled_memory_channels = filled;
                        *memory_data.lock().unwrap() = Some(tab_text);
                    }
                    Err(e) => warn!("{} memory read failed: {}", prefix, e),
                }
                last_response = Instant::now();
                last_full_poll = Instant::now();
                last_smeter_poll = Instant::now();
            }
            Ok(YaesuCmd::WriteAllMemories(tab_text)) => {
                info!("{} writing memory channels...", prefix);
                let write_result = match model {
                    // FTX-1 schrijft freq via MW + naam via MT (beide 5-cijferig).
                    RadioModel::Ftx1 => write_all_memories_ftx1(&mut port, &tab_text),
                    _ => write_all_memories(&mut port, &tab_text),
                };
                match write_result {
                    Ok(count) => info!("{} wrote {} memory channels", prefix, count),
                    Err(e) => warn!("{} memory write failed: {}", prefix, e),
                }
                last_response = Instant::now();
                last_full_poll = Instant::now();
                last_smeter_poll = Instant::now();
            }
            Ok(YaesuCmd::ReadAllMenus) => {
                info!("{} reading all menu settings...", prefix);
                let menu_result = match model {
                    // FTX-1 EX is hiërarchisch (P1.P2.P3) -> scan-read i.p.v. platte index.
                    RadioModel::Ftx1 => read_all_menus_ftx1(&mut port),
                    _ => read_all_menus(&mut port),
                };
                match menu_result {
                    Ok(data) => {
                        info!("{} read {} menu values", prefix, data.lines().count());
                        *memory_data.lock().unwrap() = Some(format!("MENU:{}", data));
                    }
                    Err(e) => warn!("{} menu read failed: {}", prefix, e),
                }
                last_response = Instant::now();
                last_full_poll = Instant::now();
                last_smeter_poll = Instant::now();
            }
            Ok(cmd) => {
                let cmd_str = match cmd {
                    YaesuCmd::SetFreqA(hz) => format!("FA{:09};", hz),
                    YaesuCmd::SetFreqB(hz) => format!("FB{:09};", hz),
                    YaesuCmd::SetMode(mode) => format!("MD0{};", internal_mode_to_yaesu(mode)),
                    YaesuCmd::SetPtt(on) => {
                        // Auto-DATA PTT-toggle: in de gewone modes routeert de Yaesu
                        // USB-mic-audio niet (goed) als TX-modulatiebron - alleen in de
                        // DATA-varianten wél. Daarom tijdelijk naar de DATA-mode voor de
                        // TX-cyclus, daarna terug:
                        //   FM('4')->DATA-FM('A').
                        // PTT-off restores the original mode.
                        // SSB auto-DATA was removed on 2026-07-04 after operator testing showed
                        // that DATA-LSB/USB is unsuitable for voice TX.
                        // Split TX-toggle and mode-change with a short sleep so
                        // Yaesu TX-transition kan voltooien voor mode-change komt.
                        //
                        // Current flow:
                        //   - Single source of truth: dit is het ENIGE auto-DFM
                        //     emission-punt (oude network.rs wrapper verwijderd ->
                        //     geen race meer).
                        //   - !in_memory guard - mode-change in Memory-mode forceert
                        //     Yaesu naar VFO; skip auto-DFM in Memory-mode.
                        //
                        // Memory-restore extension:
                        //   - !in_memory guard verwijderd; auto-DFM werkt ook in Memory.
                        //   - Bij PTT-on: bewaar memory_channel als in_memory.
                        //   - Bij PTT-off: na MD04, restore Memory-mode via MC<nnn>;.
                        //   - Resultaat: USB-mic-TX werkt in Memory-FM én operator blijft
                        //     na PTT-off in Memory-mode op origineel kanaal.
                        let s_lock = status.lock().unwrap();
                        let mode_char = s_lock.mode_char;
                        let was_dfm = s_lock.auto_dfm_active;
                        let in_memory = s_lock.vfo_select == 1;
                        let mem_ch = s_lock.memory_channel;
                        drop(s_lock);

                        // Optimistisch TX-state zetten zodat de RX-audio-loop meteen dempt
                        // (de TX-poll bevestigt daarna). Vooral voor de FTX-1, die zijn
                        // USB-RX niet in hardware dempt tijdens TX.
                        status.lock().unwrap().tx_active = on;

                        // 991A SSB USB TX routing per PTT (hybrid option): switch the 991A
                        // to USB source only during TX, then restore it on PTT-off. FTX-1
                        // keeps its internal auto source selection and is intentionally skipped.
                        let ssb_on_ptt = status.lock().unwrap().ssb_switch_on_ptt;
                        if ssb_on_ptt && !matches!(model, RadioModel::Ftx1) && matches!(mode_char, '1' | '2') {
                            if on {
                                let _ = port.write_all(b"EX1061;"); // SSB MIC SELECT = REAR
                                let _ = port.write_all(b"EX1091;"); // SSB PORT SELECT = USB
                                std::thread::sleep(Duration::from_millis(30)); // short settle before TX
                            } else {
                                restore_991a_usb_routing_snapshot(
                                    &mut port,
                                    prefix,
                                    ft991a_usb_routing_snapshot.as_ref(),
                                    Ft991aUsbRoutingScope::Ssb,
                                    "PTT-off SSB",
                                );
                            }
                        }

                        // FT-991A AM USB TX routing per PTT: AM has no DATA-AM mode.
                        // AM PORT SELECT stays USB; AM MIC SELECT switches hand mic (MIC)
                        // versus remote audio (REAR). In presence-based mode AM follows
                        // SetSsbRouting as well. FTX-1 does not use these 991A menus.
                        if ssb_on_ptt && !matches!(model, RadioModel::Ftx1) && matches!(mode_char, '5' | 'D' | 'd') {
                            if on {
                                let _ = port.write_all(b"EX0481;"); // AM PORT SELECT = USB
                                let _ = port.write_all(b"EX0451;"); // AM MIC SELECT = REAR
                                std::thread::sleep(Duration::from_millis(30));
                            } else {
                                restore_991a_usb_routing_snapshot(
                                    &mut port,
                                    prefix,
                                    ft991a_usb_routing_snapshot.as_ref(),
                                    Ft991aUsbRoutingScope::Am,
                                    "PTT-off AM",
                                );
                            }
                        }
                        // Map the current mode to the DATA variant used for USB mic TX.
                        // FM only: FM('4')->DATA-FM('A'). SSB->DATA-LSB/USB was reverted:
                        // those DATA modes introduce carrier offset and narrow data filters,
                        // which makes them unsuitable for speech.
                        let data_target = match mode_char {
                            '4' => Some(('A', "DATA-FM")),   // FM -> DATA-FM
                            _ => None,
                        };
                        if on {
                            if let (Some((dch, dname)), false) = (data_target, was_dfm) {
                                // Defensieve diagnose-aid: Memory-mode met memory_channel=0
                                // betekent stille memory-loss bij PTT-off (geen MC-restore).
                                if in_memory && mem_ch == 0 {
                                    warn!("{} auto-DATA: in Memory-mode maar memory_channel=0 - geen MC-restore (state mogelijk niet geïnitialiseerd)", prefix);
                                }
                                // Naar DATA-mode eerst, settle 50ms, dan PTT-on. Bij Memory-mode
                                // forceert de MD-switch naar VFO; kanaal bewaard + na PTT-off restoren.
                                let pre = format!("MD0{};", dch);
                                if let Err(e) = port.write_all(pre.as_bytes()) {
                                    warn!("{} auto-DATA pre-PTT {} failed: {}", prefix, pre, e);
                                    return;
                                }
                                std::thread::sleep(Duration::from_millis(50));
                                let mut s = status.lock().unwrap();
                                s.auto_dfm_active = true;
                                s.auto_dfm_saved_mode = mode_char;
                                s.auto_dfm_saved_memory_channel =
                                    if in_memory && mem_ch > 0 { mem_ch } else { 0 };
                                info!("{} auto-DATA: {} -> {} voor PTT-on (memory={}, ch={})",
                                    prefix, mode_char, dname, in_memory, s.auto_dfm_saved_memory_channel);
                                "TX1;".to_string()
                            } else {
                                "TX1;".to_string()
                            }
                        } else if was_dfm {
                            let (saved_mode, saved_mem) = {
                                let s = status.lock().unwrap();
                                (s.auto_dfm_saved_mode, s.auto_dfm_saved_memory_channel)
                            };
                            // PTT-off eerst (Yaesu schakelt TX uit), settle 100ms voor de
                            // TX-transition, dan de oorspronkelijke mode terug, evt. memory-restore.
                            if let Err(e) = port.write_all(b"TX0;") {
                                warn!("{} auto-DATA pre-MD TX0 failed: {}", prefix, e);
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(100));
                            let restore = format!("MD0{};", saved_mode);
                            if let Err(e) = port.write_all(restore.as_bytes()) {
                                warn!("{} auto-DATA restore {} failed: {}", prefix, restore, e);
                                return;
                            }
                            if saved_mem > 0 {
                                std::thread::sleep(Duration::from_millis(50));
                                let mc_cmd = format!("MC{:03};", saved_mem);
                                if let Err(e) = port.write_all(mc_cmd.as_bytes()) {
                                    warn!("{} auto-DATA memory-restore {} failed: {}",
                                        prefix, mc_cmd, e);
                                }
                            }
                            let mut s = status.lock().unwrap();
                            s.auto_dfm_active = false;
                            s.auto_dfm_saved_memory_channel = 0;
                            info!("{} auto-DATA: DATA -> {} na PTT-off (mem-restore={})",
                                prefix, saved_mode, saved_mem);
                            String::new()  // alle commando's al verstuurd
                        } else {
                            "TX0;".to_string()
                        }
                    }
                    YaesuCmd::SetSsbRouting(on) => {
                        // Presence-based (opt-out): enable 991A SSB/AM USB routing while a
                        // client is present, then restore the session snapshot after ~2 s without a client.
                        // FTX-1 keeps its internal auto source selection, so this command is a no-op for it.
                        if !matches!(model, RadioModel::Ftx1) {
                            if on {
                                let _ = port.write_all(b"EX1061;"); // SSB MIC SELECT = REAR
                                let _ = port.write_all(b"EX1091;"); // SSB PORT SELECT = USB
                                let _ = port.write_all(b"EX0481;"); // AM PORT SELECT = USB
                                let _ = port.write_all(b"EX0451;"); // AM MIC SELECT = REAR
                                info!("{} 991A SSB/AM USB routing ON (client present)", prefix);
                            } else {
                                restore_991a_usb_routing_snapshot(
                                    &mut port,
                                    prefix,
                                    ft991a_usb_routing_snapshot.as_ref(),
                                    Ft991aUsbRoutingScope::All,
                                    "no client",
                                );
                            }
                        }
                        String::new()
                    }
                    YaesuCmd::SetAfGain(v) => format!("AG0{:03};", v.min(255)),
                    YaesuCmd::SetTxPower(v) => {
                        // FTX-1 vereist de head-prefix (PC{head}{nnn}); 991A niet (PC{nnn}).
                        let head = status.lock().unwrap().power_head;
                        if head == 0 {
                            format!("PC{:03};", v.min(100))
                        } else {
                            format!("PC{}{:03};", head, v.min(100))
                        }
                    }
                    YaesuCmd::SetPower(on) => format!("PS{};", if on { 1 } else { 0 }),
                    // MC = memory recall. FTX-1: MAIN/SUB-prefix + 5-cijferig kanaal
                    // (`MC0{ch:05};`, P1=0=MAIN); 991A: 3-cijferig (`MC{ch:03};`).
                    // Zonder de juiste vorm doen Mem-/Mem+ niets op de FTX-1.
                    YaesuCmd::RecallMemory(ch) => match model {
                        RadioModel::Ftx1 => format!("MC0{:05};", ch),
                        _ => format!("MC{:03};", ch),
                    },
                    YaesuCmd::SelectVfo(vfo) => {
                        match vfo {
                            0 => "VS0;FT0;".to_string(),  // VFO A: select + TX on A
                            1 => "VS1;FT1;".to_string(),  // VFO B: select + TX on B
                            2 => "SV;".to_string(),        // A<>B swap
                            3 => "VM;".to_string(),        // V/M toggle
                            _ => String::new(),
                        }
                    }
                    YaesuCmd::RawCat(ref s) => s.clone(),
                    // Typed DSP/function control (PATCH-yaesu-extra-controls, Fase A1).
                    // Generieke P1=0/MAIN-encode: werkt voor de 991A (accepteert de leidende 0)
                    // én voor de FTX-1 MAIN-receiver (side=0). Model-specifieke controls zijn
                    // geguard (AMC=alleen FTX-1, NB/NR on/off=alleen 991A) zodat een verdwaald
                    // pakket naar het verkeerde model no-op is i.p.v. de radio te verrassen; de
                    // client gate't dit al in de UI. Onbekend/ongeldig-voor-model -> leeg (no-op).
                    // NB: hardware-verificatie per commando via de reply-parse hieronder.
                    YaesuCmd::SetFeature(ctrl, val) => {
                        let is_ftx1 = matches!(model, RadioModel::Ftx1);
                        match ctrl {
                            0 => format!("RA0{};", val.min(1)), // RfAtt: RA P1=0 (fixed), P2 0/1
                            1 => format!("BI{};", val.min(1)),  // BreakIn: BI 0/1
                            2 => format!("NA0{};", val.min(1)), // Narrow: NA P1=0 (MAIN), P2 0/1
                            3 => format!("BC0{};", val.min(1)), // Auto-Notch (DNF): BC P1=0 (MAIN), P2 0/1
                            6 => format!("GT0{};", val.clamp(1, 4)), // AGC: GT P1=0, P2 1=fast/2=mid/3=slow/4=auto (hardware-geverifieerd §13; AUTO leest terug als 4/5/6 -> server normaliseert naar 4)
                            7 => format!("PA0{};", val.min(2)), // Pre-amp/IPO (HF): PA P1=0, P2 0-2 (IPO/AMP1/AMP2)
                            8 => format!("NL0{:03};", val.min(10)),  // Noise Blanker level: NL P1=0, P2 000-010 (0=off)
                            9 => format!("RL0{:02};", val.min(10)),  // Noise Reduction (DNR) level: RL P1=0, P2 00-10 (0=off)
                            10 => format!("PL{:03};", val.min(100)), // Speech Processor level: PL 000-100 (0=off)
                            11 if is_ftx1 => format!("AO{:03};", val.clamp(1, 100)), // AMC output level: AO 001-100 (FTX-1-only; 991A kent geen AO)
                            13 if !is_ftx1 => format!("NB0{};", val.min(1)), // 991A: Noise Blanker on/off (NB), los van NL-niveau (FTX-1 codeert off-in-level)
                            14 if !is_ftx1 => format!("NR0{};", val.min(1)), // 991A: Noise Reduction on/off (NR), los van RL-niveau (FTX-1 codeert off-in-level)
                            // Fase D - CO P1=0, P2 (0=contour on/off,1=contour freq,2=APF on/off,3=APF freq), P3 4-digit.
                            15 => format!("CO00{:04};", val.min(1)),        // Contour on/off
                            16 => format!("CO02{:04};", val.min(1)),        // APF on/off
                            18 => format!("CO01{:04};", val.clamp(10, 3200)), // Contour freq (10-3200 Hz)
                            19 => format!("CO03{:04};", val.min(50)),       // APF freq (0000-0050)
                            // BP P1=0, P2 (0=notch on/off,1=notch freq), P3 3-digit.
                            17 => format!("BP00{:03};", val.min(1)),        // Manual notch on/off
                            20 => format!("BP01{:03};", val.clamp(1, 320)), // Manual notch freq (x10 Hz)
                            // Clarifier (§15, hardware-spec geverifieerd). 991A: RT/XT/RC/RU/RD
                            // (relatief). FTX-1: CF - RX/TX-clar samen in één P3=0-bericht (dus
                            // de andere stand uit de bijgehouden state lezen) + absolute freq (P3=1).
                            21 => match model { // RIT (RX-clarifier) on/off
                                RadioModel::Ftx1 => {
                                    let xit = (status.lock().unwrap().feature_toggles >> 22) & 1;
                                    format!("CF000{}{}000;", val.min(1), xit)
                                }
                                _ => format!("RT{};", val.min(1)),
                            },
                            22 => match model { // XIT (TX-clarifier) on/off
                                RadioModel::Ftx1 => {
                                    let rit = (status.lock().unwrap().feature_toggles >> 21) & 1;
                                    format!("CF000{}{}000;", rit, val.min(1))
                                }
                                _ => format!("XT{};", val.min(1)),
                            },
                            23 => { // Clarifier clear (offset -> 0)
                                status.lock().unwrap().feature_freqs[3] = 0;
                                match model {
                                    RadioModel::Ftx1 => "CF001+0000;".to_string(),
                                    _ => "RC;".to_string(),
                                }
                            }
                            24 => { // Clarifier step (value = i16-as-u16 signed Hz)
                                let step = val as i16;
                                match model {
                                    RadioModel::Ftx1 => {
                                        // Absoluut: nieuwe offset uit de bijgehouden state, clamp ±9999.
                                        let newv = {
                                            let mut s = status.lock().unwrap();
                                            let nv = (s.feature_freqs[3] as i16)
                                                .saturating_add(step).clamp(-9999, 9999);
                                            s.feature_freqs[3] = nv as u16;
                                            nv
                                        };
                                        let sign = if newv >= 0 { '+' } else { '-' };
                                        format!("CF001{}{:04};", sign, newv.unsigned_abs())
                                    }
                                    _ => {
                                        // 991A relatief: accumuleer state (geen readback via los cmd).
                                        {
                                            let mut s = status.lock().unwrap();
                                            let nv = (s.feature_freqs[3] as i16)
                                                .saturating_add(step).clamp(-9999, 9999);
                                            s.feature_freqs[3] = nv as u16;
                                        }
                                        if step >= 0 { format!("RU{:04};", step as u16) }
                                        else { format!("RD{:04};", step.unsigned_abs()) }
                                    }
                                }
                            }
                            _ => String::new(),
                        }
                    }
                    YaesuCmd::WriteMemory { channel, freq_hz, mode, ctcss, shift } => {
                        let mode_char = internal_mode_to_yaesu(mode);
                        // MW format mirrors MR response:
                        // MW + P1(1):bank=0 + ??(1):2 + freq(10) + clar(6):+00000
                        // + rxclar(1):0 + txclar(1):0 + mode(1) + vfo(1):2
                        // + ctcss(1) + tone#(2):00 + shift(1) + ;
                        // The channel number goes somewhere in the first bytes
                        // Try: MW + 0(bank) + channel(2) + freq(10) + rest
                        format!("MW0{:02}{:010}+00000{}0{}2{}00{};",
                            channel, freq_hz, 0, mode_char, ctcss, shift)
                    }
                    YaesuCmd::ReadAllMemories | YaesuCmd::WriteAllMemories(_)
                    | YaesuCmd::ReadAllMenus => unreachable!(),
                    YaesuCmd::SetMenu(num, ref val) => format!("EX{:03}{};", num, val),
                };
                if let Err(e) = port.write_all(cmd_str.as_bytes()) {
                    warn!("{} send '{}' failed: {}", prefix, cmd_str, e);
                    return;
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                // Graceful shutdown while the port is still alive -> restore the 991A
                // USB-routing session snapshot. FTX-1 auto source selection is left untouched.
                if !matches!(model, RadioModel::Ftx1) {
                    restore_991a_usb_routing_snapshot(
                        &mut port,
                        prefix,
                        ft991a_usb_routing_snapshot.as_ref(),
                        Ft991aUsbRoutingScope::All,
                        "command channel closed",
                    );
                    info!("{} command channel closed, stopping", prefix);
                } else {
                    info!("{} command channel closed, stopping", prefix);
                }
                return;
            }
        }

        let now = Instant::now();

        // Fast poll: S-meter every 200ms. FTX-1: óók RI0; (P8 = squelch open/dicht)
        // voor de server-side software-squelch - de FTX-1 gate't zijn USB-audio
        // niet zelf (anders dan de 991A). 991A krijgt geen RI (heeft het niet).
        if now.duration_since(last_smeter_poll).as_millis() >= 200 {
            last_smeter_poll = now;
            let fast: &[u8] = if matches!(model, RadioModel::Ftx1) { b"SM0;RI0;" } else { b"SM0;" };
            if let Err(e) = port.write_all(fast) {
                warn!("{} S-meter poll failed: {}", prefix, e);
                return;
            }
        }

        // Full poll: freq, mode, TX state every 500ms
        if now.duration_since(last_full_poll).as_millis() >= 500 {
            last_full_poll = now;
            if let Err(e) = port.write_all(b"FA;FB;MD0;TX;AG0;PC;PS;IF;SQ0;RG0;MG;FT;SC;AC;RA0;BI;NA0;BC0;GT0;PA0;NL0;RL0;PL;AO;NB0;NR0;CO00;CO01;CO02;CO03;BP00;BP01;") {
                warn!("{} full poll failed: {}", prefix, e);
                return;
            }
            // Clarifier readback (§15): 991A leest RIT/XIT los (RT/XT), offset houden we
            // zelf bij (geen los read-cmd). FTX-1: CF000=RX/TX-clar on/off, CF001=offset.
            let clar_poll: &[u8] = if matches!(model, RadioModel::Ftx1) { b"CF000;CF001;" } else { b"RT;XT;" };
            if let Err(e) = port.write_all(clar_poll) {
                warn!("{} clarifier poll failed: {}", prefix, e);
                return;
            }

            // Audio watchdog: rebuild streams if no samples for 5 seconds
            let last_ms = last_audio_time.load(std::sync::atomic::Ordering::Relaxed);
            if last_ms > 0 {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let stale_ms = now_ms.saturating_sub(last_ms);
                if stale_ms > 5000 {
                    if let Some(ref dev) = audio_device {
                        warn!("{} audio watchdog: no samples for {:.1}s, rebuilding streams", prefix, stale_ms as f64 / 1000.0);
                        // Reset timestamp to prevent repeated rebuilds - give new stream 10s to start
                        let future_ms = now_ms + 10_000;
                        last_audio_time.store(future_ms, std::sync::atomic::Ordering::Relaxed);
                        match build_capture_stream(dev, rx_audio_tx.clone(), last_audio_time.clone(), prefix, capture_channel) {
                            Ok((stream, _rate)) => {
                                capture_stream.set(Some(stream));
                                info!("{} audio capture rebuilt by watchdog", prefix);
                            }
                            Err(e) => warn!("{} audio watchdog capture failed: {}", prefix, e),
                        }
                        let out_dev = output_device.as_deref().unwrap_or(dev.as_str());
                        match build_output_stream(out_dev, tx_producer.clone(), prefix) {
                            Ok((stream, _rate)) => {
                                output_stream.set(Some(stream));
                                info!("{} audio output rebuilt by watchdog", prefix);
                            }
                            Err(e) => warn!("{} audio watchdog output failed: {}", prefix, e),
                        }
                    }
                }
            }
        }

        // Output-stream herstel: als de TX-uitgang niet open is (bv. de CODEC was
        // bij het openen bezet / in exclusieve modus / nog niet vrij), blijf 'm
        // periodiek herproberen. Zo komt TX vanzelf terug zodra het apparaat vrij
        // is -- zonder handmatige server-herstart. De capture-watchdog hierboven
        // dekt dit niet (die triggert alleen op RX-stilte, en RX kan prima lopen
        // terwijl TX faalt).
        if !output_stream.is_set() && last_output_retry.elapsed().as_secs() >= 5 {
            last_output_retry = Instant::now();
            if let Some(out_dev) = output_device.as_deref().or(audio_device.as_deref()) {
                match build_output_stream(out_dev, tx_producer.clone(), prefix) {
                    Ok((stream, _rate)) => {
                        output_stream.set(Some(stream));
                        info!("{} audio output hersteld (apparaat vrij)", prefix);
                    }
                    Err(e) => log::debug!("{} audio output retry mislukt: {}", prefix, e),
                }
            }
        }

        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Parse all complete responses (semicolon-terminated) from the buffer.
/// `prefix` = per-radio log-tag (`[radio{N}/{MODEL}]`); `warned_modes` /
/// `warned_short_if` = warn-once guards tegen 500 ms-poll-spam.
fn parse_responses(
    buf: &mut String,
    status: &Arc<Mutex<YaesuState>>,
    prefix: &str,
    model: RadioModel,
    warned_modes: &mut HashSet<char>,
    warned_short_if: &mut bool,
) {
    while let Some(semi_pos) = buf.find(';') {
        let response = buf[..semi_pos].to_string();
        buf.drain(..=semi_pos);

        if response.len() < 2 {
            continue;
        }

        let cmd = &response[..2];
        let payload = &response[2..];

        match cmd {
            "FA" => {
                if let Ok(hz) = payload.parse::<u64>() {
                    let mut s = status.lock().unwrap();
                    if hz != s.vfo_a_freq {
                        s.vfo_a_freq = hz;
                        log::debug!("{} VFO A: {} Hz", prefix, hz);
                    }
                }
            }
            "FB" => {
                if let Ok(hz) = payload.parse::<u64>() {
                    let mut s = status.lock().unwrap();
                    if hz != s.vfo_b_freq {
                        s.vfo_b_freq = hz;
                        log::debug!("{} VFO B: {} Hz", prefix, hz);
                    }
                }
            }
            "MD" => {
                if payload.len() >= 2 {
                    let mode_char = payload.chars().nth(1).unwrap_or('2');
                    // Faal-veilig: onbekende mode-code zou stil naar USB
                    // defaulten - warn één keer per uniek teken zodat een FTX-1-
                    // specifieke mode tijdens de test zichtbaar wordt i.p.v. verzwegen.
                    let known = matches!(mode_char,
                        '1'|'2'|'3'|'4'|'5'|'6'|'7'|'8'|'9'|'A'|'a'|'B'|'b'|'C'|'c');
                    if !known && warned_modes.insert(mode_char) {
                        warn!("{} onbekende MD mode-code '{}' - val terug op USB; mogelijk model-specifiek", prefix, mode_char);
                    }
                    let mode = yaesu_mode_to_internal(mode_char);
                    let mut s = status.lock().unwrap();
                    // Only log/update when internal mode changes (ignore FM<->DATA-FM flips)
                    if mode != s.mode {
                        info!("{} mode: {} ({})", prefix, mode_char, mode);
                        s.mode = mode;
                    }
                    s.mode_char = mode_char; // always track raw char for PTT FM->DATA-FM
                }
            }
            "TX" => {
                let active = payload.starts_with('1') || payload.starts_with('2');
                let mut s = status.lock().unwrap();
                if active != s.tx_active {
                    info!("{} TX: {}", prefix, if active { "ON" } else { "OFF" });
                    s.tx_active = active;
                }
            }
            "SM" => {
                if payload.len() >= 4 {
                    if let Ok(val) = payload[1..].parse::<u16>() {
                        status.lock().unwrap().smeter = val;
                    }
                }
            }
            "AG" => {
                if payload.len() >= 4 {
                    if let Ok(val) = payload[1..].parse::<u16>() {
                        status.lock().unwrap().af_gain = val.min(255) as u8;
                    }
                }
            }
            "PC" => {
                // FTX-1: "PC{P1}{nnn}" - P1=head (1=field 5-10W, 2=Optima 5-100W),
                // nnn=watts -> payload 4 tekens. 991A: "PC{nnn}" -> payload 3 tekens.
                // Detecteer op lengte zodat beide modellen kloppen.
                let p = payload.trim();
                if p.len() >= 4 {
                    let head = p.as_bytes()[0].wrapping_sub(b'0');
                    if let Ok(val) = p[1..].parse::<u16>() {
                        let mut s = status.lock().unwrap();
                        s.power_head = head;
                        s.tx_power = val.min(100) as u8;
                    }
                } else if let Ok(val) = p.parse::<u16>() {
                    let mut s = status.lock().unwrap();
                    s.power_head = 0;
                    s.tx_power = val.min(100) as u8;
                }
            }
            "PS" => {
                let on = payload.starts_with('1');
                let mut s = status.lock().unwrap();
                if on != s.power_on {
                    info!("{} power: {}", prefix, if on { "ON" } else { "OFF" });
                    s.power_on = on;
                }
            }
            "SQ" => {
                if payload.len() >= 4 {
                    if let Ok(val) = payload[1..].parse::<u16>() {
                        status.lock().unwrap().squelch = val.min(255) as u8;
                    }
                }
            }
            "RG" => {
                if payload.len() >= 4 {
                    if let Ok(val) = payload[1..].parse::<u16>() {
                        status.lock().unwrap().rf_gain = val.min(255) as u8;
                    }
                }
            }
            "MG" => {
                if let Ok(val) = payload.parse::<u16>() {
                    status.lock().unwrap().mic_gain = val.min(100) as u8;
                }
            }
            "FT" => {
                let split = payload.starts_with('1');
                status.lock().unwrap().split_active = split;
            }
            "SC" => {
                // 991A: SC{P2} -> scan-state op [0]. FTX-1: SC{P1}{P2} -> MAIN/SUB-side
                // op [0], scan-state op [1] (P2: 0=off, 1=up, 2=down). Zonder model-
                // awareness las de FTX-1 de side i.p.v. de scan-state.
                let scan_char = match model {
                    RadioModel::Ftx1 => payload.chars().nth(1).unwrap_or('0'),
                    _ => payload.chars().nth(0).unwrap_or('0'),
                };
                status.lock().unwrap().scan_active = scan_char != '0';
            }
            "AC" => {
                // Internal ATU readback. payload = P1 P2 P3 (3 chars).
                //   991A: AC00P3; (P1P2 fixed "00"), P3: 0=off, 1=on, 2=tuning.
                //   FTX-1: AC P1 P2 P3; radio meldt zijn tuner met P1=1, P2=0 (empirisch:
                //     'AC100' bij off). P2=2 = ATAS (buiten scope). P3: 0=off,1=on,3=tuning.
                // P3 lezen zolang P2=0 (tuner-mode), ongeacht P1. Normaliseer P3->0/1/2,
                // nooit raw doorgeven.
                let pc: Vec<char> = payload.chars().collect();
                let p2 = pc.get(1).copied().unwrap_or('0');
                let p3 = pc.get(2).copied().unwrap_or('0');
                let state = if p2 != '0' {
                    0 // ATAS of niet-tuner-mode -> geen interne-ATU-stand
                } else {
                    match p3 {
                        '0' => 0,
                        '1' => 1,
                        '2' | '3' => 2,
                        _ => 0,
                    }
                };
                let mut s = status.lock().unwrap();
                if state != s.tuner_state {
                    info!("{} tuner: {}", prefix, match state { 0 => "OFF", 1 => "ON", _ => "TUNING" });
                    s.tuner_state = state;
                }
            }
            "RA" => {
                // RA0 P2 -> RF-ATT on/off (PATCH-yaesu-extra-controls, YaesuCtrl::RfAtt bit 0).
                let on = payload.chars().nth(1) == Some('1');
                let mut s = status.lock().unwrap();
                let bit = 1u32 << 0;
                s.feature_toggles = if on { s.feature_toggles | bit } else { s.feature_toggles & !bit };
            }
            "BI" => {
                // BI P1 -> break-in on/off (YaesuCtrl::BreakIn bit 1).
                let on = payload.starts_with('1');
                let mut s = status.lock().unwrap();
                let bit = 1u32 << 1;
                s.feature_toggles = if on { s.feature_toggles | bit } else { s.feature_toggles & !bit };
            }
            "NA" => {
                // NA0 P2 -> narrow on/off (YaesuCtrl::Narrow bit 2). FTX-1: P1=side.
                let on = payload.chars().nth(1) == Some('1');
                let mut s = status.lock().unwrap();
                let bit = 1u32 << 2;
                s.feature_toggles = if on { s.feature_toggles | bit } else { s.feature_toggles & !bit };
            }
            "BC" => {
                // BC0 P2 -> auto-notch (DNF) on/off (YaesuCtrl::AutoNotch bit 3). FTX-1: P1=side.
                let on = payload.chars().nth(1) == Some('1');
                let mut s = status.lock().unwrap();
                let bit = 1u32 << 3;
                s.feature_toggles = if on { s.feature_toggles | bit } else { s.feature_toggles & !bit };
            }
            "GT" => {
                // GT0 P2 -> AGC mode (YaesuCtrl::Agc, level index 6).
                // Hardware-geverifieerd (§13, build 32) op 991A én FTX-1: 1=FAST, 2=MID,
                // 3=SLOW, en AUTO wordt teruggemeld als de *opgeloste* auto-snelheid
                // 4=auto-fast/5=auto-mid/6=auto-slow. We zetten AUTO altijd met 4 en
                // normaliseren élke readback 4/5/6 -> 4 (AUTO) zodat readback == set-waarde.
                let raw = payload.chars().nth(1).and_then(|c| c.to_digit(10)).unwrap_or(0) as u8;
                let v = if (4..=6).contains(&raw) { 4 } else { raw };
                let mut s = status.lock().unwrap();
                s.feature_levels[6] = v;
            }
            "PA" => {
                // PA0 P2 -> pre-amp/IPO on HF (YaesuCtrl::PreAmp, level index 7). FTX-1: P1=band.
                let v = payload.chars().nth(1).and_then(|c| c.to_digit(10)).unwrap_or(0) as u8;
                let mut s = status.lock().unwrap();
                s.feature_levels[7] = v;
            }
            "NL" => {
                // NL0 P2P2P2 -> Noise Blanker level (index 8). P1=side.
                let lvl: u16 = payload.get(1..).and_then(|x| x.trim().parse().ok()).unwrap_or(0);
                let mut s = status.lock().unwrap();
                s.feature_levels[8] = lvl.min(255) as u8;
            }
            "RL" => {
                // RL0 P2P2 -> Noise Reduction (DNR) level (index 9). P1=side.
                let lvl: u16 = payload.get(1..).and_then(|x| x.trim().parse().ok()).unwrap_or(0);
                let mut s = status.lock().unwrap();
                s.feature_levels[9] = lvl.min(255) as u8;
            }
            "PL" => {
                // PL P1P1P1 -> Speech Processor level (index 10). No side.
                let lvl: u16 = payload.trim().parse().unwrap_or(0);
                let mut s = status.lock().unwrap();
                s.feature_levels[10] = lvl.min(255) as u8;
            }
            "AO" => {
                // AO P1P1P1 -> AMC output level (index 11). No side.
                let lvl: u16 = payload.trim().parse().unwrap_or(0);
                let mut s = status.lock().unwrap();
                s.feature_levels[11] = lvl.min(255) as u8;
            }
            "NB" => {
                // NB0 P2 -> Noise Blanker on/off (991A), YaesuCtrl::NbOn bit 13. FTX-1 stuurt geen NB.
                let on = payload.chars().nth(1) == Some('1');
                let mut s = status.lock().unwrap();
                let bit = 1u32 << 13;
                s.feature_toggles = if on { s.feature_toggles | bit } else { s.feature_toggles & !bit };
            }
            "NR" => {
                // NR0 P2 -> Noise Reduction on/off (991A), YaesuCtrl::NrOn bit 14.
                let on = payload.chars().nth(1) == Some('1');
                let mut s = status.lock().unwrap();
                let bit = 1u32 << 14;
                s.feature_toggles = if on { s.feature_toggles | bit } else { s.feature_toggles & !bit };
            }
            "CO" => {
                // CO P1 P2 P3P3P3P3 - P2: 0=Contour on/off, 1=Contour freq, 2=APF on/off, 3=APF freq.
                let p2 = payload.chars().nth(1).unwrap_or('0');
                let p3: u16 = payload.get(2..).and_then(|x| x.trim().parse().ok()).unwrap_or(0);
                let mut s = status.lock().unwrap();
                match p2 {
                    '0' => { let b = 1u32 << 15; s.feature_toggles = if p3 != 0 { s.feature_toggles | b } else { s.feature_toggles & !b }; }
                    '1' => s.feature_freqs[0] = p3,
                    '2' => { let b = 1u32 << 16; s.feature_toggles = if p3 != 0 { s.feature_toggles | b } else { s.feature_toggles & !b }; }
                    '3' => s.feature_freqs[1] = p3,
                    _ => {}
                }
            }
            "BP" => {
                // BP P1 P2 P3P3P3 - P2: 0=Manual notch on/off, 1=notch freq (x10 Hz).
                let p2 = payload.chars().nth(1).unwrap_or('0');
                let p3: u16 = payload.get(2..).and_then(|x| x.trim().parse().ok()).unwrap_or(0);
                let mut s = status.lock().unwrap();
                match p2 {
                    '0' => { let b = 1u32 << 17; s.feature_toggles = if p3 != 0 { s.feature_toggles | b } else { s.feature_toggles & !b }; }
                    '1' => s.feature_freqs[2] = p3,
                    _ => {}
                }
            }
            "RT" => {
                // 991A RIT (RX-clarifier) on/off -> toggle-bit 21.
                let on = payload.starts_with('1');
                let mut s = status.lock().unwrap();
                let bit = 1u32 << 21;
                s.feature_toggles = if on { s.feature_toggles | bit } else { s.feature_toggles & !bit };
            }
            "XT" => {
                // 991A XIT (TX-clarifier) on/off -> toggle-bit 22.
                let on = payload.starts_with('1');
                let mut s = status.lock().unwrap();
                let bit = 1u32 << 22;
                s.feature_toggles = if on { s.feature_toggles | bit } else { s.feature_toggles & !bit };
            }
            "CF" => {
                // FTX-1 Clarifier. P3 (payload[2]): 0=setting (P4=RX/RIT, P5=TX/XIT),
                // 1=freq (P4=teken, P5-P8=0000-9999 Hz). 991A gebruikt RT/XT + accumulatie.
                let p3 = payload.chars().nth(2).unwrap_or('0');
                let mut s = status.lock().unwrap();
                match p3 {
                    '0' => {
                        let rit = payload.chars().nth(3) == Some('1');
                        let xit = payload.chars().nth(4) == Some('1');
                        let (rb, xb) = (1u32 << 21, 1u32 << 22);
                        s.feature_toggles = if rit { s.feature_toggles | rb } else { s.feature_toggles & !rb };
                        s.feature_toggles = if xit { s.feature_toggles | xb } else { s.feature_toggles & !xb };
                    }
                    '1' => {
                        let sign = payload.chars().nth(3).unwrap_or('+');
                        let mag: i16 = payload.get(4..8).and_then(|x| x.trim().parse().ok()).unwrap_or(0);
                        let off = if sign == '-' { -mag } else { mag };
                        s.feature_freqs[3] = off as u16;
                    }
                    _ => {}
                }
            }
            "RI" => {
                // FTX-1 Radio Information. P8 (laatste teken) = squelch/BUSY:
                // 0 = squelch dicht (geen signaal), 1 = open (BUSY). Drijft de
                // server-side software-squelch op de FTX-1 USB-audio (de FTX-1
                // gate't zijn USB-audio niet zelf). 991A stuurt geen RI.
                if let Some(p8) = payload.chars().last() {
                    let open = p8 == '1';
                    let mut s = status.lock().unwrap();
                    if open != s.squelch_open {
                        info!("{} squelch: {}", prefix, if open { "OPEN (BUSY)" } else { "DICHT" });
                        s.squelch_open = open;
                    }
                }
            }
            "IF" => {
                // IF-veldindeling verschilt per model (zie reference_ftx1_cat_protocol):
                //   991A : kanaal op [0..3], P7 (VFO/Mem) op [20], payload >=23.
                //   FTX-1: 5-cijferig kanaal [0..5], P7 op [22], payload 27 (zelfde
                //          layout als MR/MW: P1(5) P2(9:freq) P3(5) P4 P5 P6 P7 ...).
                let (ch_end, p7_idx, min_len) = match model {
                    RadioModel::Ftx1 => (5usize, 22usize, 27usize),
                    _ => (3usize, 20usize, 22usize),
                };
                if payload.len() >= min_len {
                    let p7 = payload.chars().nth(p7_idx).unwrap_or('0');
                    let mut s = status.lock().unwrap();

                    let new_vfo = match p7 {
                        '0' => 0, // VFO (always A, B is only for split TX)
                        '1' => 1, // Memory
                        '2' => 2, // Memory Tune
                        _ => 0,
                    };
                    if new_vfo != s.vfo_select {
                        info!("{} mode: {} (IF P7='{}')",
                            prefix, match new_vfo { 0 => "VFO", 1 => "Memory", _ => "MemTune" }, p7);
                        s.vfo_select = new_vfo;
                    }
                    if let Ok(mc) = payload[0..ch_end].parse::<u16>() {
                        s.memory_channel = mc;
                    }
                } else {
                    // Faal-veilig: afwijkende IF-lengte -> niet indexen
                    // (geen out-of-range/paniek), parse overslaan + één warn. Maakt een
                    // verschoven FTX-1-veldindeling zichtbaar i.p.v. stil te falen.
                    if !*warned_short_if {
                        warn!("{} IF-respons {}B ('{}'), 991A verwacht >=22 - velden mogelijk verschoven, parse overgeslagen",
                            prefix, payload.len(), payload);
                        *warned_short_if = true;
                    }
                }
            }
            _ => {
                log::debug!("{} unknown response: {}{}", prefix, cmd, payload);
            }
        }
    }

    // Prevent buffer from growing unbounded if no semicolons arrive
    if buf.len() > 1024 {
        buf.clear();
    }
}

fn parse_ex_menu_value(menu: u16, response: &str) -> Option<String> {
    let prefix = format!("EX{:03}", menu);
    let start = response.find(&prefix)?;
    let rest = &response[start + prefix.len()..];
    let end = rest.find(';')?;
    let value = rest[..end].trim();
    if value.is_empty() { None } else { Some(value.to_string()) }
}

fn read_ex_menu_value(
    port: &mut Box<dyn serialport::SerialPort>,
    prefix: &str,
    menu: u16,
    label: &str,
) -> Option<String> {
    let response = cat_query(port, &format!("EX{:03};", menu));
    if response.trim().is_empty() || response.contains("?;") {
        warn!(
            "{} 991A USB routing snapshot: EX{:03} {} read failed ({:?})",
            prefix, menu, label, response
        );
        return None;
    }
    let value = parse_ex_menu_value(menu, &response);
    if value.is_none() {
        warn!(
            "{} 991A USB routing snapshot: EX{:03} {} parse failed ({:?})",
            prefix, menu, label, response
        );
    }
    value
}

fn write_ex_menu_value(
    port: &mut Box<dyn serialport::SerialPort>,
    prefix: &str,
    menu: u16,
    value: &str,
    label: &str,
) -> bool {
    let cmd = format!("EX{:03}{};", menu, value);
    match port.write_all(cmd.as_bytes()) {
        Ok(()) => {
            log::debug!("{} restored {} via {}", prefix, label, cmd);
            std::thread::sleep(Duration::from_millis(30));
            true
        }
        Err(e) => {
            warn!("{} restore {} failed via {}: {}", prefix, label, cmd, e);
            false
        }
    }
}

fn restore_991a_usb_routing_snapshot(
    port: &mut Box<dyn serialport::SerialPort>,
    prefix: &str,
    snapshot: Option<&Ft991aUsbRoutingSnapshot>,
    scope: Ft991aUsbRoutingScope,
    reason: &str,
) {
    let Some(snapshot) = snapshot else {
        warn!("{} cannot restore 991A USB routing for {}: no session snapshot", prefix, reason);
        return;
    };

    let mut restored = 0usize;
    let mut skipped = 0usize;

    if matches!(scope, Ft991aUsbRoutingScope::Ssb | Ft991aUsbRoutingScope::All) {
        if let Some(value) = snapshot.ssb_mic_select.as_deref() {
            if write_ex_menu_value(port, prefix, 106, value, "SSB MIC SELECT") { restored += 1; }
        } else { skipped += 1; }
        if let Some(value) = snapshot.ssb_port_select.as_deref() {
            if write_ex_menu_value(port, prefix, 109, value, "SSB PORT SELECT") { restored += 1; }
        } else { skipped += 1; }
    }

    if matches!(scope, Ft991aUsbRoutingScope::Am | Ft991aUsbRoutingScope::All) {
        if let Some(value) = snapshot.am_mic_select.as_deref() {
            if write_ex_menu_value(port, prefix, 45, value, "AM MIC SELECT") { restored += 1; }
        } else { skipped += 1; }
        if let Some(value) = snapshot.am_port_select.as_deref() {
            if write_ex_menu_value(port, prefix, 48, value, "AM PORT SELECT") { restored += 1; }
        } else { skipped += 1; }
    }

    if skipped > 0 {
        warn!(
            "{} 991A USB routing restore for {} partial: restored {}, skipped {} missing snapshot values",
            prefix, reason, restored, skipped
        );
    } else {
        info!("{} 991A USB routing restored from session snapshot ({})", prefix, reason);
    }
}
/// List available serial ports (reuse for UI combo box).
/// Send a CAT command and read response until `;` or timeout.
fn cat_query(port: &mut Box<dyn serialport::SerialPort>, cmd: &str) -> String {
    cat_query_with_timeout(port, cmd, Duration::from_millis(300))
}

fn cat_query_with_timeout(
    port: &mut Box<dyn serialport::SerialPort>,
    cmd: &str,
    timeout: Duration,
) -> String {
    let mut raw_buf = [0u8; 512];
    if port.write_all(cmd.as_bytes()).is_err() { return String::new(); }
    let mut response = String::new();
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() > deadline { break; }
        match port.read(&mut raw_buf) {
            Ok(n) if n > 0 => {
                if let Ok(s) = std::str::from_utf8(&raw_buf[..n]) {
                    response.push_str(s);
                    if response.contains(';') { break; }
                }
            }
            Ok(_) => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
    response
}

/// Startup probe: read `ID;` once and warn if the connected model does not match
/// the configured slot model. Normal releases avoid raw CAT dumps; detailed CAT
/// traces should use debug logging around the specific command path being tested.
fn bringup_probe(port: &mut Box<dyn serialport::SerialPort>, prefix: &str, model: RadioModel) {
    let id_resp = cat_query(port, "ID;");
    let id_code = resp_payload("ID", &id_resp);
    let detected = RadioModel::from_id_code(&id_code);

    if id_code.is_empty() {
        warn!("{} autodetect: ID; returned no valid response ({:?}); check cable/baud radio-menu vs config; falling back to shared Yaesu parser", prefix, id_resp);
    } else if detected.is_none() {
        warn!("{} unknown ID code '{}'; assuming Yaesu-compatible CAT dialect", prefix, id_code);
    } else if detected != Some(model) {
        // Gedetecteerd model wijkt af van het geconfigureerde slot-model ->
        // mogelijke COM-/USB-enumeratie-swap. Niet-fataal, maar luidruchtig
        // zodat de operator de slot- en audio-device-toewijzing kan corrigeren.
        warn!(
            "{} model mismatch: configured={:?}, radio reports {:?} (ID {}); possible COM/USB enumeration swap",
            prefix, model, detected.unwrap(), id_code
        );
    }
}

/// Strip a 2-char CAT command echo + trailing `;` from a response, returning the
/// payload. `resp_payload("ID", "ID0670;")` -> `"0670"`. Tolerant of empty /
/// malformed input (returns what it can, never panics).
fn resp_payload(cmd: &str, resp: &str) -> String {
    let t = resp.trim().trim_end_matches(';');
    t.strip_prefix(cmd).unwrap_or(t).to_string()
}

/// Read all memory channels (001-117) from the FT-991A via MT commands.
/// Range per de FT-991A CAT-spec: 001-099 = gewone geheugens, 100-117 = PMS
/// (P-1L..P-9U). Voorheen stopte de read bij 099 waardoor kanaal 100+ wegviel;
/// de write accepteerde 1-117 al. (De FTX-1 gebruikt 00001-00099 + P-01L-tekst
/// voor PMS, dus die read-tak blijft op 99 - zie read_all_memories_ftx1.)
/// MT response format (41 chars):
///   MT P1(3:ch) P2(9:freq) P3(5:clar) P4(1:rxclar) P5(1:txclar)
///   P6(1:mode) P7(1:status) P8(1:tone) P9(2:00) P10(1:shift) P11(1:0) P12(12:TAG) ;
fn read_all_memories(port: &mut Box<dyn serialport::SerialPort>) -> Result<String, String> {
    let mut channels = Vec::new();

    for ch in 1..=117u16 {
        let response = cat_query(port, &format!("MT{:03};", ch));

        if response.trim().is_empty() || response.contains("?;") {
            continue;
        }

        if let Some(start) = response.find("MT") {
            if let Some(end) = response[start..].find(';') {
                let d = &response[start + 2..start + end]; // skip "MT"


                // MT response: P1(3)+P2(9)+P3(5)+P4(1)+P5(1)+P6(1)+P7(1)+P8(1)+P9(2)+P10(1)+P11(1)+P12(12) = 38
                if d.len() < 26 { continue; }

                let _ch_num = &d[0..3];   // P1: channel number
                let freq_hz: u64 = d[3..12].parse().unwrap_or(0); // P2: 9-digit freq
                if freq_hz == 0 { continue; }

                // P3: clar direction + offset (5 chars at 12..17), e.g. "+0000"
                // P4: rx_clar (17), P5: tx_clar (18)
                let mode_char = d.chars().nth(19).unwrap_or('2');  // P6
                // P7: status (20) - 0=VFO, 1=Memory
                let tone_char = d.chars().nth(21).unwrap_or('0');  // P8: CTCSS mode
                let tone_num = &d[22..24.min(d.len())];            // P9: tone number (00-49)
                let shift_char = d.chars().nth(24).unwrap_or('0'); // P10: shift
                // P11: 0 (25)

                // P12: TAG (12 chars, positions 26..38)
                let name = if d.len() >= 38 {
                    d[26..38].trim().to_string()
                } else if d.len() > 26 {
                    d[26..].trim().to_string()
                } else {
                    String::new()
                };

                let mode = match mode_char {
                    '1' => "LSB", '2' => "USB", '3' => "CW", '4' => "FM",
                    '5' => "AM", '6' => "RTTY-LSB", '7' => "CW-R",
                    '8' => "DATA-LSB", '9' => "RTTY-USB",
                    'A' | 'a' => "DATA-FM", 'B' | 'b' => "FM-N",
                    'C' | 'c' => "DATA-USB", 'D' | 'd' => "AM-N",
                    'E' | 'e' => "C4FM", _ => "USB",
                };
                let tone_mode = match tone_char {
                    '0' => "None", '1' => "Tone", '2' => "Tone ENC",
                    '3' => "DCS", '4' => "DCS ENC", _ => "None",
                };
                let offset_dir = match shift_char {
                    '0' => "Simplex", '1' => "Plus", '2' => "Minus", _ => "Simplex",
                };

                // CTCSS tone frequency from tone number (P9)
                let ctcss_freq = match tone_num.parse::<u8>().unwrap_or(0) {
                    0 => "67.0 Hz", 1 => "69.3 Hz", 2 => "71.9 Hz", 3 => "74.4 Hz",
                    4 => "77.0 Hz", 5 => "79.7 Hz", 6 => "82.5 Hz", 7 => "85.4 Hz",
                    8 => "88.5 Hz", 9 => "91.5 Hz", 10 => "94.8 Hz", 11 => "97.4 Hz",
                    12 => "100.0 Hz", 13 => "103.5 Hz", 14 => "107.2 Hz", 15 => "110.9 Hz",
                    16 => "114.8 Hz", 17 => "118.8 Hz", 18 => "123.0 Hz", 19 => "127.3 Hz",
                    20 => "131.8 Hz", 21 => "136.5 Hz", 22 => "141.3 Hz", 23 => "146.2 Hz",
                    24 => "151.4 Hz", 25 => "156.7 Hz", 26 => "159.8 Hz", 27 => "162.2 Hz",
                    28 => "165.5 Hz", 29 => "167.9 Hz", 30 => "171.3 Hz", 31 => "173.8 Hz",
                    32 => "177.3 Hz", 33 => "179.9 Hz", 34 => "183.5 Hz", 35 => "186.2 Hz",
                    36 => "189.9 Hz", 37 => "192.8 Hz", 38 => "196.6 Hz", 39 => "199.5 Hz",
                    40 => "203.5 Hz", 41 => "206.5 Hz", 42 => "210.7 Hz", 43 => "218.1 Hz",
                    44 => "225.7 Hz", 45 => "229.1 Hz", 46 => "233.6 Hz", 47 => "241.8 Hz",
                    48 => "250.3 Hz", 49 => "254.1 Hz",
                    _ => "67.0 Hz",
                };

                // Calculate TX freq and offset based on shift direction and band
                let (tx_freq_hz, offset_freq_str) = match shift_char {
                    '1' => { // Plus
                        let offset = if freq_hz >= 430_000_000 { 1_600_000u64 } else { 600_000 };
                        (freq_hz + offset, if offset == 1_600_000 { "1,60 MHz" } else { "600 kHz" })
                    }
                    '2' => { // Minus
                        let offset = if freq_hz >= 430_000_000 { 1_600_000u64 } else { 600_000 };
                        (freq_hz.saturating_sub(offset), if offset == 1_600_000 { "1,60 MHz" } else { "600 kHz" })
                    }
                    _ => (freq_hz, ""), // Simplex
                };

                let freq_mhz = freq_hz as f64 / 1_000_000.0;
                let freq_str = format!("{:.5}", freq_mhz).replace('.', ",");
                let tx_freq_mhz = tx_freq_hz as f64 / 1_000_000.0;
                let tx_freq_str = format!("{:.5}", tx_freq_mhz).replace('.', ",");
                let display_name = if name.is_empty() { format!("CH {:02}", ch) } else { name.clone() };

                channels.push(format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t\tOff\tOff\tOff\tOff\tAuto\tOff\tOff\tOff\t6.25 kHz\t",
                    ch, freq_str, tx_freq_str, offset_freq_str, offset_dir, mode, mode, display_name, tone_mode, ctcss_freq
                ));

                info!("MT{:03}: {} {} {} {} {} {} {}", ch, display_name, freq_str, mode, tone_mode, offset_dir, ctcss_freq, tone_num);
            }
        }
    }

    let mut out = String::new();
    out.push_str("Channel Number\tReceive Frequency\tTransmit Frequency\tOffset Frequency\tOffset Direction\tOperating Mode\tTx Operating Mode\tName\tTone Mode\tCTCSS\tDCS\tNarrow\tSkip\tAttenuator\tTuner\tAGC\tNoise Blanker\tIPO\tDNR\tStep\tComment\t\n");
    for line in &channels {
        out.push_str(line);
        out.push('\n');
    }
    info!("Yaesu: read {} non-empty memory channels out of 117", channels.len());
    Ok(out)
}

/// Write memory channels to the FT-991A via MT set commands.
/// MT set format (41 chars):
///   MT P1(3:ch) P2(9:freq) P3(5:clar) P4(1:rxclar) P5(1:txclar)
///   P6(1:mode) P7(1:0=fixed) P8(1:tone) P9(2:00) P10(1:shift) P11(1:0) P12(12:TAG) ;
fn write_all_memories(port: &mut Box<dyn serialport::SerialPort>, tab_text: &str) -> Result<usize, String> {
    let mut count = 0;

    let mut lines = tab_text.lines();
    let header = lines.next().ok_or("Empty tab text")?;

    let cols: Vec<&str> = header.split('\t').collect();
    let find_col = |name: &str| cols.iter().position(|c| c.trim().eq_ignore_ascii_case(name));
    let col_ch = find_col("Channel Number");
    let col_rx = find_col("Receive Frequency");
    let col_mode = find_col("Operating Mode");
    let col_tone = find_col("Tone Mode");
    let col_ctcss = find_col("CTCSS");
    let col_dir = find_col("Offset Direction");
    let col_name = find_col("Name");

    for line in lines {
        let line = line.trim();
        if line.is_empty() { continue; }

        let fields: Vec<&str> = line.split('\t').collect();
        let get = |idx: Option<usize>| idx.and_then(|i| fields.get(i).map(|s| s.trim())).unwrap_or("");

        let ch: u16 = match get(col_ch).parse() {
            Ok(n) if n >= 1 && n <= 117 => n,
            _ => continue,
        };

        let freq_str = get(col_rx).replace(',', ".");
        let freq_hz: u64 = match freq_str.parse::<f64>() {
            Ok(mhz) => (mhz * 1_000_000.0).round() as u64,
            Err(_) => continue,
        };
        // The FT-991A rejects a memory write with frequency 0 (out of the valid
        // 30 kHz-470 MHz range), so there is no CAT way to clear a channel - a
        // deleted row is simply not rewritten. Skip freq-0 rows. (Verified on
        // hardware 2026-07-18: MT with freq 0 -> "?;".)
        if freq_hz == 0 { continue; }

        // Memory-storage modes: respect what the client provided. The
        // FM -> DATA-FM auto-toggle is a RUNTIME PTT-mechanic in
        // `set_ptt()` (FM <-> DATA-FM around the TX window for USB-mic
        // compatibility), NOT a storage transform. Earlier code force-
        // mapped all FM variants to 'A' here, which left every memory
        // channel permanently in DATA-FM after a Write-radio cycle and
        // disabled local FM-mic on those channels. Operator-feedback
        // 2026-06-07.
        // Mode-codes moeten round-trip kloppen met de read-parser
        // hierboven (line ~1003-1007): '4'->FM, 'B'->FM-N, '5'->AM,
        // 'D'->AM-N, 'A'->DATA-FM, 'E'->C4FM, etc. Eerdere code mapte
        // AM-N->'5' (= AM) en C4FM->'A' (= DATA-FM), wat de read-na-write
        // integriteit brak.
        let mode_char = match get(col_mode) {
            "LSB" => '1', "USB" => '2', "CW" => '3',
            "FM" => '4',
            "FM-N" => 'B',
            "AM" => '5',
            "AM-N" => 'D',
            "RTTY-LSB" => '6', "CW-R" => '7',
            "DATA-LSB" => '8', "RTTY-USB" => '9',
            "DATA-FM" => 'A',
            "DATA-USB" => 'C',
            "C4FM" => 'E',
            _ => '4', // default plain FM (most common memory mode)
        };

        let tone = match get(col_tone) {
            "None" => '0', "Tone" => '1', "Tone ENC" => '2',
            "DCS" => '3', "DCS ENC" => '4', _ => '0',
        };

        // CTCSS tone number from frequency string
        let tone_num: u8 = match get(col_ctcss) {
            "67.0 Hz" => 0, "69.3 Hz" => 1, "71.9 Hz" => 2, "74.4 Hz" => 3,
            "77.0 Hz" => 4, "79.7 Hz" => 5, "82.5 Hz" => 6, "85.4 Hz" => 7,
            "88.5 Hz" => 8, "91.5 Hz" => 9, "94.8 Hz" => 10, "97.4 Hz" => 11,
            "100.0 Hz" => 12, "103.5 Hz" => 13, "107.2 Hz" => 14, "110.9 Hz" => 15,
            "114.8 Hz" => 16, "118.8 Hz" => 17, "123.0 Hz" => 18, "127.3 Hz" => 19,
            "131.8 Hz" => 20, "136.5 Hz" => 21, "141.3 Hz" => 22, "146.2 Hz" => 23,
            "151.4 Hz" => 24, "156.7 Hz" => 25, "159.8 Hz" => 26, "162.2 Hz" => 27,
            "165.5 Hz" => 28, "167.9 Hz" => 29, "171.3 Hz" => 30, "173.8 Hz" => 31,
            "177.3 Hz" => 32, "179.9 Hz" => 33, "183.5 Hz" => 34, "186.2 Hz" => 35,
            "189.9 Hz" => 36, "192.8 Hz" => 37, "196.6 Hz" => 38, "199.5 Hz" => 39,
            "203.5 Hz" => 40, "206.5 Hz" => 41, "210.7 Hz" => 42, "218.1 Hz" => 43,
            "225.7 Hz" => 44, "229.1 Hz" => 45, "233.6 Hz" => 46, "241.8 Hz" => 47,
            "250.3 Hz" => 48, "254.1 Hz" => 49,
            _ => 0,
        };

        let shift = match get(col_dir) {
            "Simplex" => '0', "Plus" => '1', "Minus" => '2', _ => '0',
        };

        // TAG: 12 chars, padded with spaces
        let name = get(col_name);
        let tag: String = if name.len() >= 12 {
            name[..12].to_string()
        } else {
            format!("{:<12}", name)
        };

        // MT set: P1(3) P2(9) P3(5) P4(1) P5(1) P6(1) P7(1:0) P8(1) P9(2:00) P10(1) P11(1) P12(12) ;
        //
        // P9 is fixed "00" per FT-991A CAT spec (not the CTCSS-tone
        // index). Earlier code formatted `tone_num` here, which produced
        // a non-spec MT for any channel with Tone Mode != "None" and
        // appears to have been silently rejected by the radio. The
        // CTCSS tone index is configured separately via CN; MT carries
        // only the tone-mode flag (P8). `tone_num` is kept for now -
        // if a future patch wires up CN-write it can move there.
        let _ = tone_num; // intentionally unused until CN-write lands
        let mt_cmd = format!("MT{:03}{:09}+000000{}0{}00{}0{};",
            ch, freq_hz, mode_char, tone, shift, tag);

        info!("MT write {:03}: [{}] ({}B)", ch, mt_cmd, mt_cmd.len());

        let response = cat_query(port, &mt_cmd);
        if response.contains("?;") {
            warn!("MT{:03} rejected", ch);
        } else {
            count += 1;
        }

        std::thread::sleep(Duration::from_millis(50));
    }

    Ok(count)
}

/// Tag-naam uit een FTX-1 `MT`-antwoord halen.
/// Antwoord: `MT` + P0(5:kanaal) + P1(tot 12 ASCII tag) + `;`.
fn parse_ftx1_tag(mt: &str) -> String {
    if let (Some(s), Some(e)) = (mt.find("MT"), mt.find(';')) {
        if e > s + 7 {
            return mt[s + 7..e].trim().to_string();
        }
    }
    String::new()
}

/// FTX-1 mode-code (P6 in MR/MW) -> label. Codes wijken af van de 991A
/// (3=CW-U, 7=CW-L, E=PSK, H/I=C4FM). Labels gekozen zodat ze round-trip
/// kloppen met `ftx1_mode_to_code` en herkenbaar zijn in de client-editor.
fn ftx1_mode_label(c: char) -> &'static str {
    match c {
        '1' => "LSB", '2' => "USB", '3' => "CW", '4' => "FM", '5' => "AM",
        '6' => "RTTY-LSB", '7' => "CW-R", '8' => "DATA-LSB", '9' => "RTTY-USB",
        'A' | 'a' => "DATA-FM", 'B' | 'b' => "FM-N", 'C' | 'c' => "DATA-USB",
        'D' | 'd' => "AM-N", 'E' | 'e' => "PSK", 'F' | 'f' => "DATA-FM",
        'H' | 'h' | 'I' | 'i' => "C4FM",
        _ => "FM",
    }
}

/// Inverse van [`ftx1_mode_label`]: label -> FTX-1 mode-code (P6).
fn ftx1_mode_to_code(label: &str) -> char {
    match label {
        "LSB" => '1', "USB" => '2', "CW" => '3', "FM" => '4', "AM" => '5',
        "RTTY-LSB" => '6', "CW-R" => '7', "DATA-LSB" => '8', "RTTY-USB" => '9',
        "DATA-FM" => 'A', "FM-N" => 'B', "DATA-USB" => 'C', "AM-N" => 'D',
        "PSK" => 'E', "C4FM" => 'H',
        _ => '4', // default plain FM
    }
}

/// Read all memory channels from the Yaesu FTX-1.
///
/// De FTX-1 splitst wat de FT-991A in één `MT`-query stopt over twee commando's
/// (FTX-1 CAT OM, MR + MT) en gebruikt **5-cijferige** kanaalnummers:
///   `MR{ch:05};` -> freq/mode/clarifier/shift/ctcss (GEEN naam), 27 data-chars:
///       P1(5:ch) P2(9:freq) P3(5:clar) P4(1:rxclar) P5(1:txclar)
///       P6(1:mode) P7(1:vfo/mem) P8(1:ctcss) P9(2:fixed00) P10(1:shift)
///   `MT{ch:05};` -> de 12-char tag (naam) van dat kanaal.
/// (De 991A gebruikt 3-cijferige kanalen + een gecombineerde MT-query, vandaar
/// dat `MT001;` op de FTX-1 `?;` teruggaf.)
fn read_all_memories_ftx1(port: &mut Box<dyn serialport::SerialPort>) -> Result<String, String> {
    let mut channels = Vec::new();

    for ch in 1..=99u16 {
        let mut mr = cat_query(port, &format!("MR{:05};", ch));
        // Transient timeout (lege respons) tijdens drukke client-connect -> tot 2
        // retries. `?;` = leeg kanaal (terecht overslaan, GEEN retry). Dit voorkomt
        // dat de auto-read bij opstarten kanalen mist (manueel idle lukt wel).
        let mut tries = 0;
        while mr.trim().is_empty() && tries < 2 {
            mr = cat_query(port, &format!("MR{:05};", ch));
            tries += 1;
        }

        // Ruwe-respons probe (eerste 3 kanalen) zodat de hardware het
        // manual-formaat bevestigt - net als bij PC/IF bring-up.
        if ch <= 3 {
            info!("MR{:05} RAW probe: [{}] ({}B)", ch, mr.escape_debug(), mr.len());
        }

        if mr.trim().is_empty() || mr.contains("?;") {
            continue;
        }
        let (start, end) = match (mr.find("MR"), mr.find(';')) {
            (Some(s), Some(e)) if e > s + 2 => (s, e),
            _ => continue,
        };
        let d = &mr[start + 2..end]; // skip "MR"
        if d.len() < 27 {
            continue;
        }
        let b = d.as_bytes();

        let freq_hz: u64 = d[5..14].parse().unwrap_or(0); // P2
        if freq_hz == 0 {
            continue;
        }
        let mode_char = b[21] as char; // P6
        let ctcss_char = b[23] as char; // P8
        let shift_char = b[26] as char; // P10

        // Naam via aparte MT-query.
        let mt = cat_query(port, &format!("MT{:05};", ch));
        if ch <= 3 {
            info!("MT{:05} RAW probe: [{}] ({}B)", ch, mt.escape_debug(), mt.len());
        }
        let name = parse_ftx1_tag(&mt);

        let mode = ftx1_mode_label(mode_char);
        let tone_mode = match ctcss_char {
            '0' => "None", '1' => "Tone", '2' => "Tone ENC",
            '3' => "DCS", '4' => "PR FREQ", '5' => "REV", _ => "None",
        };
        let offset_dir = match shift_char {
            '0' => "Simplex", '1' => "Plus", '2' => "Minus", _ => "Simplex",
        };
        let (tx_freq_hz, offset_freq_str) = match shift_char {
            '1' => {
                let o = if freq_hz >= 430_000_000 { 1_600_000u64 } else { 600_000 };
                (freq_hz + o, if o == 1_600_000 { "1,60 MHz" } else { "600 kHz" })
            }
            '2' => {
                let o = if freq_hz >= 430_000_000 { 1_600_000u64 } else { 600_000 };
                (freq_hz.saturating_sub(o), if o == 1_600_000 { "1,60 MHz" } else { "600 kHz" })
            }
            _ => (freq_hz, ""),
        };
        // P9 is fixed "00" in MR -> de CTCSS-toonfrequentie zit hier niet in;
        // default (zoals 991A vóór CN-write). Verfijning = aparte CT/CN-query.
        let ctcss_freq = "67.0 Hz";

        let freq_str = format!("{:.5}", freq_hz as f64 / 1_000_000.0).replace('.', ",");
        let tx_freq_str = format!("{:.5}", tx_freq_hz as f64 / 1_000_000.0).replace('.', ",");
        let display_name = if name.is_empty() { format!("CH {:02}", ch) } else { name.clone() };

        channels.push(format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t\tOff\tOff\tOff\tOff\tAuto\tOff\tOff\tOff\t6.25 kHz\t",
            ch, freq_str, tx_freq_str, offset_freq_str, offset_dir, mode, mode, display_name, tone_mode, ctcss_freq
        ));
        info!("MR{:05}: {} {} {} {} {}", ch, display_name, freq_str, mode, tone_mode, offset_dir);
    }

    let mut out = String::new();
    out.push_str("Channel Number\tReceive Frequency\tTransmit Frequency\tOffset Frequency\tOffset Direction\tOperating Mode\tTx Operating Mode\tName\tTone Mode\tCTCSS\tDCS\tNarrow\tSkip\tAttenuator\tTuner\tAGC\tNoise Blanker\tIPO\tDNR\tStep\tComment\t\n");
    for line in &channels {
        out.push_str(line);
        out.push('\n');
    }
    info!("FTX-1: read {} non-empty memory channels out of 99", channels.len());
    Ok(out)
}

/// Write memory channels to the Yaesu FTX-1.
///
/// Freq/mode/shift/ctcss via `MW` (zelfde 27-byte veldindeling als het
/// MR-antwoord), de naam via een aparte `MT`-tag-write. Beide 5-cijferig:
///   `MW{ch:05}{freq:09}{clar:5}{rxclar}{txclar}{mode}{p7=1}{ctcss}00{shift};`
///   `MT{ch:05}{tag:<12};`
fn write_all_memories_ftx1(port: &mut Box<dyn serialport::SerialPort>, tab_text: &str) -> Result<usize, String> {
    let mut count = 0;

    let mut lines = tab_text.lines();
    let header = lines.next().ok_or("Empty tab text")?;
    let cols: Vec<&str> = header.split('\t').collect();
    let find_col = |name: &str| cols.iter().position(|c| c.trim().eq_ignore_ascii_case(name));
    let col_ch = find_col("Channel Number");
    let col_rx = find_col("Receive Frequency");
    let col_mode = find_col("Operating Mode");
    let col_tone = find_col("Tone Mode");
    let col_dir = find_col("Offset Direction");
    let col_name = find_col("Name");

    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let get = |idx: Option<usize>| idx.and_then(|i| fields.get(i).map(|s| s.trim())).unwrap_or("");

        let ch: u16 = match get(col_ch).parse() {
            Ok(n) if (1..=99).contains(&n) => n,
            _ => continue,
        };
        let freq_str = get(col_rx).replace(',', ".");
        let freq_hz: u64 = match freq_str.parse::<f64>() {
            Ok(mhz) => (mhz * 1_000_000.0).round() as u64,
            Err(_) => continue,
        };
        if freq_hz == 0 {
            continue;
        }
        let mode_char = ftx1_mode_to_code(get(col_mode));
        let tone = match get(col_tone) {
            "None" => '0', "Tone" => '1', "Tone ENC" => '2',
            "DCS" => '3', "PR FREQ" => '4', "REV" => '5', _ => '0',
        };
        let shift = match get(col_dir) {
            "Simplex" => '0', "Plus" => '1', "Minus" => '2', _ => '0',
        };

        // MW: P1(5:ch) P2(9:freq) P3(5:clar="+0000") P4(1:rxclar=0)
        //     P5(1:txclar=0) P6(1:mode) P7(1:mem=1) P8(1:ctcss) P9(2:00) P10(1:shift)
        let mw_cmd = format!("MW{:05}{:09}+0000{}{}{}1{}00{};",
            ch, freq_hz, '0', '0', mode_char, tone, shift);
        info!("MW write {:05}: [{}] ({}B)", ch, mw_cmd, mw_cmd.len());
        let mw_resp = cat_query(port, &mw_cmd);
        if mw_resp.contains("?;") {
            warn!("MW{:05} rejected", ch);
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        std::thread::sleep(Duration::from_millis(50));

        // Naam (tag) los schrijven via MT (tot 12 chars, met spaties gevuld).
        let name = get(col_name);
        let tag: String = if name.len() >= 12 {
            name[..12].to_string()
        } else {
            format!("{:<12}", name)
        };
        let mt_cmd = format!("MT{:05}{};", ch, tag);
        info!("MT write {:05}: [{}] ({}B)", ch, mt_cmd, mt_cmd.len());
        let mt_resp = cat_query(port, &mt_cmd);
        if mt_resp.contains("?;") {
            warn!("MT{:05} (tag) rejected", ch);
        }
        count += 1;
        std::thread::sleep(Duration::from_millis(50));
    }

    Ok(count)
}

/// Read all 153 EX menu settings from the FT-991A.
/// Returns newline-separated "nnn:value" pairs.
fn read_all_menus(port: &mut Box<dyn serialport::SerialPort>) -> Result<String, String> {
    let mut lines = Vec::new();

    for menu in 1..=153u16 {
        let response = cat_query(port, &format!("EX{:03};", menu));

        if response.trim().is_empty() || response.contains("?;") {
            lines.push(format!("{:03}:", menu));
            continue;
        }

        // Parse: EXnnnVALUE;
        let prefix = format!("EX{:03}", menu);
        if let Some(start) = response.find(&prefix) {
            if let Some(end) = response[start..].find(';') {
                let value = &response[start + 5..start + end]; // skip "EXnnn"
                lines.push(format!("{:03}:{}", menu, value));
            } else {
                lines.push(format!("{:03}:", menu));
            }
        } else {
            lines.push(format!("{:03}:", menu));
        }
    }

    Ok(lines.join("\n"))
}

/// Read all EX menu settings from the Yaesu FTX-1.
///
/// De FTX-1 EX is hiërarchisch: `EX{P1:02}{P2:02}{P3:02};` -> antwoord
/// `EX{P1}{P2}{P3}{waarde};`. Er is geen platte index zoals de 991A. We
/// *scannen* de geldige adressen live op de radio (ground truth): een ongeldig
/// adres geeft `?;` en wordt overgeslagen. De client matcht de adressen tegen de
/// menu-chart (Table 3) voor labels - een fout label kan dus nooit een verkeerd
/// adres schrijven. Output: \"p1p2p3:waarde\"-regels (6-cijferig adres).
///
/// Begrenzing: P1 1..=11, P2 1..=9, P3 1..=40. Per (P1,P2) stoppen we vroeg als
/// P3=01 én 02 ontbreken (subgroep bestaat niet), en na 6 opeenvolgende missers
/// binnen een bestaande subgroep (einde items, tolereert gaten tot 5).
enum Ftx1ExRead {
    Value(String),
    Missing,
    NoResponse,
}

fn read_ftx1_ex_value(port: &mut Box<dyn serialport::SerialPort>, p1: u8, p2: u8, p3: u8) -> Ftx1ExRead {
    let cmd = format!("EX{:02}{:02}{:02};", p1, p2, p3);
    let prefix = format!("EX{:02}{:02}{:02}", p1, p2, p3);

    for attempt in 0..3 {
        let resp = cat_query_with_timeout(port, &cmd, Duration::from_millis(700));
        std::thread::sleep(Duration::from_millis(15));

        if resp.contains("?;") {
            return Ftx1ExRead::Missing;
        }
        if let Some(start) = resp.find(&prefix) {
            if let Some(end_rel) = resp[start..].find(';') {
                let value_start = start + prefix.len();
                let value_end = start + end_rel;
                if value_end >= value_start {
                    return Ftx1ExRead::Value(resp[value_start..value_end].to_string());
                }
            }
        }

        // Empty or stale/mismatched replies can happen during the long EX scan;
        // retry before treating the address as soft-missing.
        if attempt < 2 {
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    Ftx1ExRead::NoResponse
}

fn read_all_menus_ftx1(port: &mut Box<dyn serialport::SerialPort>) -> Result<String, String> {
    // The FTX-1 intermittently returns a transient "?;" or times out for an
    // address that actually has a value, so a single pass misses a variable
    // subset of the ~405 menus (this is why the old flow needed several manual
    // "Read" clicks, each recovering different stragglers). We run up to
    // MAX_PASSES full passes and merge results by address, re-querying only the
    // addresses not yet captured, and stop as soon as a pass adds nothing new.
    // One "Read" thus does internally what the operator used to do by clicking
    // repeatedly, converging on the complete set.
    //
    // Pathological guard: if the radio stops replying entirely mid-scan
    // (unplugged / hung), a non-existent subgroup would otherwise scan all 40
    // addresses at ~2.2 s each, blocking the single-threaded poll loop (PTT/CAT)
    // for minutes. Abort once we see this many *consecutive* no-response reads
    // with no valid value or "?;" in between (any real reply resets the counter),
    // spanning all passes.
    use std::collections::BTreeMap;
    const MAX_PASSES: u8 = 5;
    const MAX_CONSECUTIVE_NO_RESPONSE: u8 = 15; // ~33 s of uninterrupted silence
    let mut found: BTreeMap<String, String> = BTreeMap::new();
    let mut no_response_count = 0usize;
    let mut consecutive_no_response = 0u8;
    let mut aborted = false;

    'passes: for pass in 1..=MAX_PASSES {
        let before = found.len();
        'scan: for p1 in 1..=11u8 {
            for p2 in 1..=9u8 {
                let mut found_in_p2 = false;
                let mut consecutive_definitive_miss = 0u8;
                for p3 in 1..=40u8 {
                    let key = format!("{:02}{:02}{:02}", p1, p2, p3);
                    // Already captured in an earlier pass: treat as present, no I/O.
                    if found.contains_key(&key) {
                        found_in_p2 = true;
                        consecutive_definitive_miss = 0;
                        consecutive_no_response = 0;
                        continue;
                    }
                    match read_ftx1_ex_value(port, p1, p2, p3) {
                        Ftx1ExRead::Value(value) => {
                            found.insert(key, value);
                            found_in_p2 = true;
                            consecutive_definitive_miss = 0;
                            consecutive_no_response = 0;
                        }
                        Ftx1ExRead::Missing => {
                            consecutive_definitive_miss += 1;
                            consecutive_no_response = 0;
                        }
                        Ftx1ExRead::NoResponse => {
                            no_response_count += 1;
                            consecutive_no_response += 1;
                            if consecutive_no_response >= MAX_CONSECUTIVE_NO_RESPONSE {
                                aborted = true;
                                break 'scan;
                            }
                        }
                    }

                    if !found_in_p2 && p3 >= 2 && consecutive_definitive_miss >= 2 {
                        break; // subgroup does not exist: 01 and 02 were both rejected by the radio.
                    }
                    if found_in_p2 && consecutive_definitive_miss >= 6 {
                        break; // end of items in this subgroup; tolerate gaps up to 5 addresses.
                    }
                }
            }
        }
        let added = found.len() - before;
        info!(
            "FTX-1: EX menu scan pass {}/{} -> {} values total (+{} new)",
            pass,
            MAX_PASSES,
            found.len(),
            added
        );
        if aborted || added == 0 {
            break 'passes; // radio gone, or converged (a full pass found nothing new).
        }
    }

    if aborted {
        warn!(
            "FTX-1: EX menu scan aborted after {} consecutive no-response reads (radio not responding); returning {} partial values",
            MAX_CONSECUTIVE_NO_RESPONSE,
            found.len()
        );
    } else if no_response_count > 0 {
        warn!("FTX-1: EX menu scan had {} timeout/stale replies across passes (recovered via re-scan where possible)", no_response_count);
    }
    info!("FTX-1: read {} EX menu values", found.len());
    let lines: Vec<String> = found
        .into_iter()
        .map(|(k, v)| format!("{}:{}", k, v))
        .collect();
    Ok(lines.join("\n"))
}

#[allow(dead_code)] // helper for UI port-picker; not yet used by config flow
pub fn available_ports() -> Vec<String> {
    serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.port_name)
        .collect()
}

// --- Audio stream builders (used for initial setup + reconnect) ---

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Splits a configured device-pattern into (naam-substring, positie). Een optioneel
/// achtervoegsel "#N" kiest het **N-de** (1-based) apparaat dat op de naam matcht -
/// nodig wanneer twee radio's een identiek benoemd USB-audio-device hebben
/// (bv. 2× "USB Audio CODEC"). Geen suffix = #1 (eerste match) = ongewijzigd gedrag.
/// Voorbeeld config: `yaesu2_audio=USB Audio CODEC#2`.
fn parse_device_pattern(pattern: &str) -> (String, usize) {
    if let Some((name, idx)) = pattern.rsplit_once('#') {
        if let Ok(n) = idx.trim().parse::<usize>() {
            if n >= 1 {
                return (name.to_string(), n);
            }
        }
    }
    (pattern.to_string(), 1)
}

/// Build a cpal input capture stream that feeds into an existing tokio sender.
fn build_capture_stream(
    device_pattern: &str,
    tx: tokio::sync::mpsc::Sender<Vec<f32>>,
    last_audio_time: Arc<std::sync::atomic::AtomicU64>,
    prefix: &str,
    // Dual-RX kanaal-keuze (FTX-1): 0 = L (hardware-RX 1), 1 = R (hardware-RX 2),
    // 2 = mix (gemiddelde). Mono devices negeren dit (downmix-tak draait niet).
    channel: u8,
) -> Result<(cpal::Stream, u32), String> {
    let host = cpal::default_host();
    let (pat_name, pos) = parse_device_pattern(device_pattern);
    let pat = pat_name.to_lowercase();
    let device = host.input_devices()
        .map_err(|e| format!("enumerate input devices: {}", e))?
        .filter(|d| d.name().map(|n| n.to_lowercase().contains(&pat)).unwrap_or(false))
        .nth(pos - 1)
        .ok_or_else(|| format!("no input device matching '{}' (#{})", pat_name, pos))?;

    let device_name = device.name().unwrap_or_default();
    // Device-naam met prefix: cruciaal voor edge-case 6 (twee identieke
    // "USB Audio CODEC"-devices -> zo zie je welk device aan welke radio hangt).
    info!("{} audio input: {}", prefix, device_name);

    let config = device.default_input_config()
        .map_err(|e| format!("input config: {}", e))?;
    let sample_rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    info!("{} audio: {}Hz, {} channels, {:?}", prefix, sample_rate, channels, config.sample_format());
    let prefix_err = prefix.to_string();

    let stream = device.build_input_stream(
        &config.into(),
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            // Operator LATENCY-WAIVER (release v2.0.0, operator: PA3GHM/cjenschede):
            // Per-callback Vec-allocatie is bewust geaccepteerd op deze server-side
            // Yaesu RX-pad. Operator heeft de latency-prioriteit afgewogen tegen de
            // implementatie-kosten en gekozen voor de huidige aanpak omdat:
            //   (a) server runt op Thetis-PC, geen lokale real-time audio-output;
            //       audio-latency wordt overschaduwd door encode + netwerk-pad
            //   (b) alloc-cost ~50µs is <0.5% van ~10ms frame-budget - niet
            //       hoorbaar onder normale belasting
            //   (c) Vec::with_capacity(frames) voorkomt grow-realloc bij stabiele
            //       input-config
            //   (d) tokio::mpsc::Sender consumeert de Vec (ownership-move); zero
            //       alloc vereist Vec-pool met return-channel - niet-triviale
            //       refactor, gepland voor post-release optimization in v2.1+.
            let frames = data.len() / channels.max(1);
            let mut mono: Vec<f32> = Vec::with_capacity(frames);
            if channels > 1 {
                for ch in data.chunks(channels) {
                    let s = match channel {
                        0 => ch[0],
                        1 => *ch.get(1).unwrap_or(&ch[0]),
                        _ => ch.iter().sum::<f32>() / ch.len() as f32, // mix
                    };
                    mono.push(s);
                }
            } else {
                mono.extend_from_slice(data);
            }
            let _ = tx.try_send(mono);
            // Update watchdog timestamp
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            last_audio_time.store(now_ms, std::sync::atomic::Ordering::Relaxed);
        },
        move |err| { log::warn!("{} audio capture error: {}", prefix_err, err); },
        None,
    ).map_err(|e| format!("build input stream: {}", e))?;

    stream.play().map_err(|e| format!("start capture: {}", e))?;
    info!("{} audio capture started", prefix);

    Ok((stream, sample_rate))
}

/// Build a cpal output playback stream with a swappable ring buffer producer.
fn build_output_stream(
    device_pattern: &str,
    producer_handle: Arc<Mutex<Option<ringbuf::HeapProd<f32>>>>,
    prefix: &str,
) -> Result<(cpal::Stream, u32), String> {
    let host = cpal::default_host();
    let (pat_name, pos) = parse_device_pattern(device_pattern);
    let pat = pat_name.to_lowercase();
    // Per-radio output-device. Bij twee radio's die zich beide als
    // "USB Audio CODEC" melden (edge-case 6) MOET het TX-pad het output-device
    // matchen dat bij DEZE radio hoort - anders gaat radio-1's TX-audio naar
    // radio-0's codec; the device name must stay per slot.
    // We matchen op het per-radio device-patroon (zelfde USB-CODEC = zelfde
    // friendly-name voor capture én playback) + de #N-positie zodat twee
    // identiek benoemde devices uit elkaar te houden zijn. Fallback op
    // "USB Audio CODEC" (zelfde positie) als het specifieke patroon geen output
    // oplevert -> geen regressie t.o.v. het oude gedrag, single-radio blijft werken.
    let pick = |p: &str, n: usize| -> Option<cpal::Device> {
        host.output_devices()
            .ok()?
            .filter(|d| d.name().map(|nm| nm.to_lowercase().contains(p)).unwrap_or(false))
            .nth(n.saturating_sub(1))
    };
    let device = match pick(&pat, pos) {
        Some(d) => d,
        None => {
            if pat != "usb audio codec" {
                warn!(
                    "{} geen output-device #{} matcht '{}' - fallback naar 'USB Audio CODEC' #{}",
                    prefix, pos, pat_name, pos
                );
            }
            pick("usb audio codec", pos)
                .ok_or_else(|| format!("no output device matching '{}' (#{})", pat_name, pos))?
        }
    };

    let device_name = device.name().unwrap_or_default();
    info!("{} audio output: {}", prefix, device_name);

    let config = device.default_output_config()
        .map_err(|e| format!("output config: {}", e))?;
    let sample_rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    info!("{} audio output: {}Hz, {} channels, {:?}", prefix, sample_rate, channels, config.sample_format());
    let prefix_err = prefix.to_string();

    // Create new ring buffer
    use ringbuf::traits::Split;
    let (producer, mut consumer) = ringbuf::HeapRb::<f32>::new(sample_rate as usize * 2).split();

    // Install the new producer so the bridge thread can write to it
    *producer_handle.lock().unwrap() = Some(producer);

    let stream = device.build_output_stream(
        &config.into(),
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            use ringbuf::traits::Consumer as _;
            for sample in data.iter_mut() {
                *sample = consumer.try_pop().unwrap_or(0.0);
            }
        },
        move |err| { log::warn!("{} audio output error: {}", prefix_err, err); },
        None,
    ).map_err(|e| format!("build output stream: {}", e))?;

    stream.play().map_err(|e| format!("start playback: {}", e))?;
    info!("{} audio output started", prefix);

    Ok((stream, sample_rate))
}

/// Legacy structs kept for API compatibility (unused internally now)
#[allow(dead_code)]
pub struct YaesuAudio {
    pub _capture_stream: cpal::Stream,
    pub rx_audio_rx: tokio::sync::mpsc::Receiver<Vec<f32>>,
    pub sample_rate: u32,
}
unsafe impl Send for YaesuAudio {}

#[allow(dead_code)]
pub struct YaesuAudioOutput {
    _playback_stream: cpal::Stream,
    pub tx_audio_tx: tokio::sync::mpsc::Sender<Vec<f32>>,
    pub sample_rate: u32,
}
unsafe impl Send for YaesuAudioOutput {}

/// List available audio input devices (for UI combo box).
pub fn available_audio_inputs() -> Vec<String> {
    let host = cpal::default_host();
    host.input_devices()
        .map(|devices| {
            devices.filter_map(|d| d.name().ok()).collect()
        })
        .unwrap_or_default()
}

/// Enumerate output (render) devices - for the separate Yaesu TX/output device
/// picker (PATCH-yaesu-output-device).
pub fn available_audio_outputs() -> Vec<String> {
    let host = cpal::default_host();
    host.output_devices()
        .map(|devices| {
            devices.filter_map(|d| d.name().ok()).collect()
        })
        .unwrap_or_default()
}
