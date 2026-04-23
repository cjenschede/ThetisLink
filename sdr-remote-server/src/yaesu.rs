// SPDX-License-Identifier: GPL-2.0-or-later

#![allow(dead_code)]
use std::io::{Read, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use log::{info, warn};

/// Yaesu FT-991A CAT serial controller with auto-reconnect.
/// Communicates via USB virtual COM port, ASCII commands terminated with ';'.
/// When the radio loses power or the serial connection drops, the controller
/// automatically retries every 3 seconds. Audio channels persist across
/// reconnects so the network audio loops don't need to restart.
pub struct YaesuRadio {
    cmd_tx: mpsc::Sender<YaesuCmd>,
    status: Arc<Mutex<YaesuState>>,
    /// Persistent audio RX channel — sender cloned into each new cpal capture stream.
    /// The receiver is taken once by the network audio loop and stays valid forever.
    _rx_audio_tx_keepalive: tokio::sync::mpsc::Sender<Vec<f32>>,
    pub audio_rx: Mutex<Option<tokio::sync::mpsc::Receiver<Vec<f32>>>>,
    pub audio_sample_rate: u32,
    /// Persistent TX audio sender — used by the network TX decode task.
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
}

// SAFETY: cpal::Stream on Windows (WASAPI) uses COM handles safe to move between threads.
unsafe impl Send for YaesuRadio {}
unsafe impl Sync for YaesuRadio {}

#[derive(Clone, Debug)]
pub struct YaesuState {
    pub connected: bool,
    pub vfo_a_freq: u64,
    pub vfo_b_freq: u64,
    pub mode: u8,           // Internal mode (0=LSB, 1=USB, etc. — Thetis numbering)
    pub tx_active: bool,
    pub smeter: u16,        // Raw S-meter value (0-255)
    pub af_gain: u8,        // 0-255
    pub tx_power: u8,       // 0-100
    pub squelch: u8,        // 0-255
    pub rf_gain: u8,        // 0-255
    pub mic_gain: u8,       // 0-100
    pub power_on: bool,
    pub mode_char: char,    // Raw Yaesu mode character ('1'-'E')
    pub vfo_select: u8,     // 0=VFO, 1=Memory, 2=MemTune (from IF P7)
    pub memory_channel: u16, // Current memory channel number (from IF)
    pub split_active: bool,  // true = split mode active
    pub scan_active: bool,   // true = scanning
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
            squelch: 0,
            rf_gain: 0,
            mic_gain: 0,
            power_on: false,
            mode_char: '2',
            vfo_select: 0,
            memory_channel: 0,
            split_active: false,
            scan_active: false,
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
        '3' => 3,  // CW → CW-L
        '4' => 5,  // FM
        '5' => 6,  // AM
        '6' => 9,  // RTTY-LSB → DIGL
        '7' => 4,  // CW-R → CW-U
        '8' => 9,  // DATA-LSB → DIGL
        '9' => 7,  // RTTY-USB → DIGU
        'A' | 'a' => 5,  // DATA-FM → FM
        'B' | 'b' => 5,  // FM-N → FM
        'C' | 'c' => 7,  // DATA-USB → DIGU
        _ => 1,    // default USB
    }
}

/// Map internal mode to Yaesu MD0x mode character.
/// FM is sent as DATA-FM ('A') because USB mic audio only works in DATA-FM
/// on the FT-991A (requires FM PKT PORT SELECT=USB in menu settings).
fn internal_mode_to_yaesu(internal: u8) -> char {
    match internal {
        0 => '1',  // LSB
        1 => '2',  // USB
        3 => '3',  // CW-L → CW
        4 => '7',  // CW-U → CW-R
        5 => 'A',  // FM → DATA-FM (for USB mic)
        6 => '5',  // AM
        7 => 'C',  // DIGU → DATA-USB
        9 => '8',  // DIGL → DATA-LSB
        _ => '2',  // default USB
    }
}

impl YaesuRadio {
    pub fn new(port_name: &str, baud: u32, audio_device: Option<&str>) -> Result<Self, String> {
        // Open serial port (first time — must succeed)
        let port = serialport::new(port_name, baud)
            .data_bits(serialport::DataBits::Eight)
            .stop_bits(serialport::StopBits::One)
            .parity(serialport::Parity::None)
            .flow_control(serialport::FlowControl::Hardware)
            .timeout(Duration::from_millis(100))
            .open()
            .map_err(|e| format!("Failed to open {}: {}", port_name, e))?;

        let status = Arc::new(Mutex::new(YaesuState::default()));
        let (cmd_tx, cmd_rx) = mpsc::channel();

        // Create persistent audio RX channel (capture → network loop)
        let (rx_audio_tx, rx_audio_rx) = tokio::sync::mpsc::channel::<Vec<f32>>(64);

        // Create persistent TX audio channel (network → output)
        let (tx_audio_tx, tx_audio_rx) = tokio::sync::mpsc::channel::<Vec<f32>>(64);

        // Swappable cpal streams and ring buffer producer
        let capture_stream = Arc::new(StreamHolder::new(None));
        let output_stream = Arc::new(StreamHolder::new(None));
        let tx_producer: Arc<Mutex<Option<ringbuf::HeapProd<f32>>>> = Arc::new(Mutex::new(None));
        let last_audio_time = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let memory_data: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        // Initial audio setup
        let mut audio_rate = 0u32;
        let mut tx_rate = 0u32;
        if let Some(dev) = audio_device {
            // Capture (RX from Yaesu)
            // Seed audio timestamp so watchdog can detect if stream never starts
            let seed_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
            last_audio_time.store(seed_ms, std::sync::atomic::Ordering::Relaxed);
            match build_capture_stream(dev, rx_audio_tx.clone(), last_audio_time.clone()) {
                Ok((stream, rate)) => {
                    capture_stream.set(Some(stream));
                    audio_rate = rate;
                }
                Err(e) => warn!("Yaesu audio capture init failed: {}", e),
            }
            // Output (TX to Yaesu)
            match build_output_stream("USB Audio CODEC", tx_producer.clone()) {
                Ok((stream, rate)) => {
                    output_stream.set(Some(stream));
                    tx_rate = rate;
                }
                Err(e) => warn!("Yaesu audio output init failed: {}", e),
            }
        }

        // Start TX audio bridge thread: drains tx_audio_rx → ring buffer producer
        {
            let producer = tx_producer.clone();
            let mut rx = tx_audio_rx;
            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().unwrap();
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

        info!("Yaesu FT-991A connected on {} @ {} baud", port_name, baud);

        // Start self-reconnecting serial + audio thread
        // Note: Box<dyn SerialPort> is not Send, so we drop it and let the thread reopen.
        // The port was already validated above, so the first open in the thread will succeed.
        drop(port);
        {
            let status = status.clone();
            let memory_data = memory_data.clone();
            let port_name = port_name.to_string();
            let audio_device = audio_device.map(|s| s.to_string());
            let rx_audio_tx = rx_audio_tx.clone();
            let capture_stream = capture_stream.clone();
            let output_stream = output_stream.clone();
            let tx_producer = tx_producer.clone();
            let last_audio_time_clone = last_audio_time.clone();
            std::thread::spawn(move || {
                yaesu_reconnect_thread(
                    cmd_rx, status, memory_data,
                    port_name, baud, audio_device,
                    rx_audio_tx, capture_stream, output_stream, tx_producer,
                    last_audio_time_clone,
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
        })
    }

    pub fn send_command(&self, cmd: YaesuCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn status(&self) -> YaesuState {
        self.status.lock().unwrap().clone()
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
    rx_audio_tx: tokio::sync::mpsc::Sender<Vec<f32>>,
    capture_stream: Arc<StreamHolder>,
    output_stream: Arc<StreamHolder>,
    tx_producer: Arc<Mutex<Option<ringbuf::HeapProd<f32>>>>,
    last_audio_time: Arc<std::sync::atomic::AtomicU64>,
) {
    info!("Yaesu serial thread started on {}", port_name);

    // First connect (port was already validated in new(), should succeed immediately)
    let mut first = true;

    loop {
        if !first {
            warn!("Yaesu disconnected, retry in 3s...");

            // Drop old audio streams (device may have disappeared)
            capture_stream.set(None);
            output_stream.set(None);
            *tx_producer.lock().unwrap() = None;

            std::thread::sleep(Duration::from_secs(3));

            // Drain stale commands
            while cmd_rx.try_recv().is_ok() {}

            // Check if YaesuRadio was dropped (cmd channel disconnected)
            match cmd_rx.try_recv() {
                Err(mpsc::TryRecvError::Disconnected) => {
                    info!("Yaesu command channel closed, stopping reconnect");
                    return;
                }
                _ => {}
            }
        }
        first = false;

        // Try to open serial port
        let port = match serialport::new(&port_name, baud)
            .data_bits(serialport::DataBits::Eight)
            .stop_bits(serialport::StopBits::One)
            .parity(serialport::Parity::None)
            .flow_control(serialport::FlowControl::Hardware)
            .timeout(Duration::from_millis(100))
            .open()
        {
            Ok(p) => p,
            Err(e) => {
                log::debug!("Yaesu reconnect failed: {}", e);
                continue;
            }
        };

        if status.lock().unwrap().connected {
            info!("Yaesu serial connected on {}", port_name);
        } else {
            info!("Yaesu serial reconnected on {}", port_name);
            // Rebuild audio streams on reconnect (USB audio device should be back)
            if let Some(ref dev) = audio_device {
                // Small delay: USB audio device may appear after serial port
                std::thread::sleep(Duration::from_secs(1));

                match build_capture_stream(dev, rx_audio_tx.clone(), last_audio_time.clone()) {
                    Ok((stream, _rate)) => {
                        capture_stream.set(Some(stream));
                        info!("Yaesu audio capture reconnected");
                    }
                    Err(e) => warn!("Yaesu audio capture reconnect failed: {}", e),
                }
                match build_output_stream("USB Audio CODEC", tx_producer.clone()) {
                    Ok((stream, _rate)) => {
                        output_stream.set(Some(stream));
                        info!("Yaesu audio output reconnected");
                    }
                    Err(e) => warn!("Yaesu audio output reconnect failed: {}", e),
                }
            }
        }

        {
            let mut s = status.lock().unwrap();
            s.connected = true;
        }

        // Run poll loop until disconnect (with audio watchdog)
        yaesu_poll_loop(
            port, &cmd_rx, &status, &memory_data,
            &audio_device, &rx_audio_tx, &capture_stream, &output_stream, &tx_producer, &last_audio_time,
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
    rx_audio_tx: &tokio::sync::mpsc::Sender<Vec<f32>>,
    capture_stream: &Arc<StreamHolder>,
    output_stream: &Arc<StreamHolder>,
    tx_producer: &Arc<Mutex<Option<ringbuf::HeapProd<f32>>>>,
    last_audio_time: &Arc<std::sync::atomic::AtomicU64>,
) {
    let mut read_buf = String::new();
    let mut raw_buf = [0u8; 256];
    let mut last_full_poll = Instant::now() - Duration::from_secs(1);
    let mut last_smeter_poll = Instant::now();
    let mut last_response = Instant::now();

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
                warn!("Yaesu serial read error: {}", e);
                return;
            }
        }

        // Detect unresponsive radio (e.g. power supply removed while USB still connected)
        if last_response.elapsed().as_secs() >= 5 {
            warn!("Yaesu: no response for 5s, disconnecting");
            return;
        }

        // Parse complete responses (terminated by ';')
        parse_responses(&mut read_buf, status);

        // Handle commands from the application
        match cmd_rx.try_recv() {
            Ok(YaesuCmd::ReadAllMemories) => {
                info!("Yaesu: reading all memory channels...");
                match read_all_memories(&mut port) {
                    Ok(tab_text) => {
                        let count = tab_text.lines().count() - 1;
                        info!("Yaesu: read {} memory channels", count);
                        *memory_data.lock().unwrap() = Some(tab_text);
                    }
                    Err(e) => warn!("Yaesu memory read failed: {}", e),
                }
                last_response = Instant::now();
                last_full_poll = Instant::now();
                last_smeter_poll = Instant::now();
            }
            Ok(YaesuCmd::WriteAllMemories(tab_text)) => {
                info!("Yaesu: writing memory channels...");
                match write_all_memories(&mut port, &tab_text) {
                    Ok(count) => info!("Yaesu: wrote {} memory channels", count),
                    Err(e) => warn!("Yaesu memory write failed: {}", e),
                }
                last_response = Instant::now();
                last_full_poll = Instant::now();
                last_smeter_poll = Instant::now();
            }
            Ok(YaesuCmd::ReadAllMenus) => {
                info!("Yaesu: reading all menu settings...");
                match read_all_menus(&mut port) {
                    Ok(data) => {
                        info!("Yaesu: read {} menu values", data.lines().count());
                        *memory_data.lock().unwrap() = Some(format!("MENU:{}", data));
                    }
                    Err(e) => warn!("Yaesu menu read failed: {}", e),
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
                    YaesuCmd::SetPtt(on) => format!("TX{};", if on { 1 } else { 0 }),
                    YaesuCmd::SetAfGain(v) => format!("AG0{:03};", v.min(255)),
                    YaesuCmd::SetTxPower(v) => format!("PC{:03};", v.min(100)),
                    YaesuCmd::SetPower(on) => format!("PS{};", if on { 1 } else { 0 }),
                    YaesuCmd::RecallMemory(ch) => format!("MC{:03};", ch),
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
                    warn!("Yaesu send '{}' failed: {}", cmd_str, e);
                    return;
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                info!("Yaesu command channel closed, stopping");
                return;
            }
        }

        let now = Instant::now();

        // Fast poll: S-meter every 200ms
        if now.duration_since(last_smeter_poll).as_millis() >= 200 {
            last_smeter_poll = now;
            if let Err(e) = port.write_all(b"SM0;") {
                warn!("Yaesu S-meter poll failed: {}", e);
                return;
            }
        }

        // Full poll: freq, mode, TX state every 500ms
        if now.duration_since(last_full_poll).as_millis() >= 500 {
            last_full_poll = now;
            if let Err(e) = port.write_all(b"FA;FB;MD0;TX;AG0;PC;PS;IF;SQ0;RG0;MG;FT;SC;") {
                warn!("Yaesu full poll failed: {}", e);
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
                        warn!("Yaesu audio watchdog: no samples for {:.1}s, rebuilding streams", stale_ms as f64 / 1000.0);
                        // Reset timestamp to prevent repeated rebuilds — give new stream 10s to start
                        let future_ms = now_ms + 10_000;
                        last_audio_time.store(future_ms, std::sync::atomic::Ordering::Relaxed);
                        match build_capture_stream(dev, rx_audio_tx.clone(), last_audio_time.clone()) {
                            Ok((stream, _rate)) => {
                                capture_stream.set(Some(stream));
                                info!("Yaesu audio capture rebuilt by watchdog");
                            }
                            Err(e) => warn!("Yaesu audio watchdog capture failed: {}", e),
                        }
                        match build_output_stream("USB Audio CODEC", tx_producer.clone()) {
                            Ok((stream, _rate)) => {
                                output_stream.set(Some(stream));
                                info!("Yaesu audio output rebuilt by watchdog");
                            }
                            Err(e) => warn!("Yaesu audio watchdog output failed: {}", e),
                        }
                    }
                }
            }
        }

        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Parse all complete responses (semicolon-terminated) from the buffer.
fn parse_responses(buf: &mut String, status: &Arc<Mutex<YaesuState>>) {
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
                        log::debug!("Yaesu VFO A: {} Hz", hz);
                    }
                }
            }
            "FB" => {
                if let Ok(hz) = payload.parse::<u64>() {
                    let mut s = status.lock().unwrap();
                    if hz != s.vfo_b_freq {
                        s.vfo_b_freq = hz;
                        log::debug!("Yaesu VFO B: {} Hz", hz);
                    }
                }
            }
            "MD" => {
                if payload.len() >= 2 {
                    let mode_char = payload.chars().nth(1).unwrap_or('2');
                    let mode = yaesu_mode_to_internal(mode_char);
                    let mut s = status.lock().unwrap();
                    // Only log/update when internal mode changes (ignore FM↔DATA-FM flips)
                    if mode != s.mode {
                        info!("Yaesu mode: {} ({})", mode_char, mode);
                        s.mode = mode;
                    }
                    s.mode_char = mode_char; // always track raw char for PTT FM→DATA-FM
                }
            }
            "TX" => {
                let active = payload.starts_with('1') || payload.starts_with('2');
                let mut s = status.lock().unwrap();
                if active != s.tx_active {
                    info!("Yaesu TX: {}", if active { "ON" } else { "OFF" });
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
                if let Ok(val) = payload.parse::<u16>() {
                    status.lock().unwrap().tx_power = val.min(100) as u8;
                }
            }
            "PS" => {
                let on = payload.starts_with('1');
                let mut s = status.lock().unwrap();
                if on != s.power_on {
                    info!("Yaesu power: {}", if on { "ON" } else { "OFF" });
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
                // SC0=off, SC1/SC2/SC3=scanning
                let scanning = !payload.starts_with('0');
                status.lock().unwrap().scan_active = scanning;
            }
            "IF" => {
                // IF response: positions 0..2=mem_ch, 3..11=freq(9), ..., 20=P7(VFO/Mem)
                if payload.len() >= 22 {
                    let p7 = payload.chars().nth(20).unwrap_or('0');
                    let _p8 = payload.chars().nth(21).unwrap_or('0');
                    let mut s = status.lock().unwrap();

                    let new_vfo = match p7 {
                        '0' => 0, // VFO (always A, B is only for split TX)
                        '1' => 1, // Memory
                        '2' => 2, // Memory Tune
                        _ => 0,
                    };
                    if new_vfo != s.vfo_select {
                        info!("Yaesu mode: {} (IF P7='{}')",
                            match new_vfo { 0 => "VFO", 1 => "Memory", _ => "MemTune" }, p7);
                        s.vfo_select = new_vfo;
                    }
                    // Note: P8 (pos 21) in IF response is NOT scan status on FT-991A.
                    // Scan status is read via separate SC; command.
                    if let Ok(mc) = payload[0..3].parse::<u16>() {
                        s.memory_channel = mc;
                    }
                }
            }
            _ => {
                log::debug!("Yaesu unknown response: {}{}", cmd, payload);
            }
        }
    }

    // Prevent buffer from growing unbounded if no semicolons arrive
    if buf.len() > 1024 {
        buf.clear();
    }
}

/// List available serial ports (reuse for UI combo box).
/// Send a CAT command and read response until `;` or timeout.
fn cat_query(port: &mut Box<dyn serialport::SerialPort>, cmd: &str) -> String {
    let mut raw_buf = [0u8; 512];
    if port.write_all(cmd.as_bytes()).is_err() { return String::new(); }
    let mut response = String::new();
    let deadline = Instant::now() + Duration::from_millis(300);
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

/// Read all memory channels (001-099) from the FT-991A via MT commands.
/// MT response format (41 chars):
///   MT P1(3:ch) P2(9:freq) P3(5:clar) P4(1:rxclar) P5(1:txclar)
///   P6(1:mode) P7(1:status) P8(1:tone) P9(2:00) P10(1:shift) P11(1:0) P12(12:TAG) ;
fn read_all_memories(port: &mut Box<dyn serialport::SerialPort>) -> Result<String, String> {
    let mut channels = Vec::new();

    for ch in 1..=99u16 {
        let response = cat_query(port, &format!("MT{:03};", ch));

        if response.trim().is_empty() || response.contains("?;") {
            continue;
        }

        if let Some(start) = response.find("MT") {
            if let Some(end) = response[start..].find(';') {
                let d = &response[start + 2..start + end]; // skip "MT"

                // Log raw for first 3 channels
                if ch <= 3 {
                    info!("MT{:03} raw data: [{}] ({}B)", ch, d, d.len());
                }

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
    info!("Yaesu: read {} non-empty memory channels out of 99", channels.len());
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
        if freq_hz == 0 { continue; }

        // FM is always sent as DATA-FM ('A') for USB mic compatibility
        let mode_char = match get(col_mode) {
            "LSB" => '1', "USB" => '2', "CW" => '3',
            "FM" | "FM-N" | "DATA-FM" | "C4FM" => 'A', // ALL FM → DATA-FM
            "AM" | "AM-N" => '5', "RTTY-LSB" => '6', "CW-R" => '7',
            "DATA-LSB" => '8', "RTTY-USB" => '9',
            "DATA-USB" => 'C',
            _ => 'A', // default DATA-FM
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

        // MT set: P1(3) P2(9) P3(5) P4(1) P5(1) P6(1) P7(1:0) P8(1) P9(2) P10(1) P11(1) P12(12) ;
        let mt_cmd = format!("MT{:03}{:09}+000000{}0{}{:02}{}0{};",
            ch, freq_hz, mode_char, tone, tone_num, shift, tag);

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

pub fn available_ports() -> Vec<String> {
    serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.port_name)
        .collect()
}

// --- Audio stream builders (used for initial setup + reconnect) ---

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Build a cpal input capture stream that feeds into an existing tokio sender.
fn build_capture_stream(
    device_pattern: &str,
    tx: tokio::sync::mpsc::Sender<Vec<f32>>,
    last_audio_time: Arc<std::sync::atomic::AtomicU64>,
) -> Result<(cpal::Stream, u32), String> {
    let host = cpal::default_host();
    let pat = device_pattern.to_lowercase();
    let device = host.input_devices()
        .map_err(|e| format!("enumerate input devices: {}", e))?
        .find(|d| d.name().map(|n| n.to_lowercase().contains(&pat)).unwrap_or(false))
        .ok_or_else(|| format!("no input device matching '{}'", device_pattern))?;

    let device_name = device.name().unwrap_or_default();
    info!("Yaesu audio input: {}", device_name);

    let config = device.default_input_config()
        .map_err(|e| format!("input config: {}", e))?;
    let sample_rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    info!("Yaesu audio: {}Hz, {} channels, {:?}", sample_rate, channels, config.sample_format());

    let stream = device.build_input_stream(
        &config.into(),
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            let mono: Vec<f32> = if channels > 1 {
                data.chunks(channels).map(|ch| ch[0]).collect()
            } else {
                data.to_vec()
            };
            let _ = tx.try_send(mono);
            // Update watchdog timestamp
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            last_audio_time.store(now_ms, std::sync::atomic::Ordering::Relaxed);
        },
        |err| { log::warn!("Yaesu audio capture error: {}", err); },
        None,
    ).map_err(|e| format!("build input stream: {}", e))?;

    stream.play().map_err(|e| format!("start capture: {}", e))?;
    info!("Yaesu audio capture started");

    Ok((stream, sample_rate))
}

/// Build a cpal output playback stream with a swappable ring buffer producer.
fn build_output_stream(
    device_pattern: &str,
    producer_handle: Arc<Mutex<Option<ringbuf::HeapProd<f32>>>>,
) -> Result<(cpal::Stream, u32), String> {
    let host = cpal::default_host();
    let pat = device_pattern.to_lowercase();
    let device = host.output_devices()
        .map_err(|e| format!("enumerate output devices: {}", e))?
        .find(|d| d.name().map(|n| n.to_lowercase().contains(&pat)).unwrap_or(false))
        .ok_or_else(|| format!("no output device matching '{}'", device_pattern))?;

    let device_name = device.name().unwrap_or_default();
    info!("Yaesu audio output: {}", device_name);

    let config = device.default_output_config()
        .map_err(|e| format!("output config: {}", e))?;
    let sample_rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    info!("Yaesu audio output: {}Hz, {} channels, {:?}", sample_rate, channels, config.sample_format());

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
        |err| { log::warn!("Yaesu audio output error: {}", err); },
        None,
    ).map_err(|e| format!("build output stream: {}", e))?;

    stream.play().map_err(|e| format!("start playback: {}", e))?;
    info!("Yaesu audio output started");

    Ok((stream, sample_rate))
}

/// Legacy structs kept for API compatibility (unused internally now)
pub struct YaesuAudio {
    pub _capture_stream: cpal::Stream,
    pub rx_audio_rx: tokio::sync::mpsc::Receiver<Vec<f32>>,
    pub sample_rate: u32,
}
unsafe impl Send for YaesuAudio {}

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
