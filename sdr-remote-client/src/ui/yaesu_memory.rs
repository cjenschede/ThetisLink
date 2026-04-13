#![allow(dead_code)]
use std::path::Path;

/// A single Yaesu FT-991A memory channel.
#[derive(Clone, Debug)]
pub struct YaesuMemoryChannel {
    pub channel_number: u16,
    pub rx_freq_hz: u64,
    pub tx_freq_hz: u64,
    pub offset_freq: String,       // e.g. "600 kHz", "1,60 MHz"
    pub offset_direction: String,  // "Simplex", "Plus", "Minus"
    pub mode: String,              // "FM", "USB", "LSB", "CW", "AM", etc.
    pub tx_mode: String,
    pub name: String,
    pub tone_mode: String,         // "None", "Tone", "Tone ENC", "DCS", "DCS ENC"
    pub ctcss: String,             // "67.0 Hz", etc.
    pub dcs: String,               // "023", etc.
    pub narrow: bool,
    pub skip: bool,
    pub attenuator: bool,
    pub tuner: bool,
    pub agc: String,               // "Auto", "Fast", "Mid", "Slow", "Off"
    pub noise_blanker: bool,
    pub ipo: String,            // "IPO", "AMP1", "AMP2"
    pub dnr: String,            // "Off", "1"-"15"
    pub step: String,              // "6.25 kHz", etc.
    pub comment: String,
}

impl Default for YaesuMemoryChannel {
    fn default() -> Self {
        Self {
            channel_number: 0,
            rx_freq_hz: 145_500_000,
            tx_freq_hz: 145_500_000,
            offset_freq: String::new(),
            offset_direction: "Simplex".into(),
            mode: "FM".into(),
            tx_mode: "FM".into(),
            name: String::new(),
            tone_mode: "None".into(),
            ctcss: "67.0 Hz".into(),
            dcs: "023".into(),
            narrow: false,
            skip: false,
            attenuator: false,
            tuner: false,
            agc: "Auto".into(),
            noise_blanker: false,
            ipo: "IPO".into(),
            dnr: "Off".into(),
            step: "6.25 kHz".into(),
            comment: String::new(),
        }
    }
}

/// Parse a frequency string with European decimal separator to Hz.
/// "144,52500" -> 144_525_000
fn parse_freq_mhz(s: &str) -> Option<u64> {
    let normalized = s.trim().replace(',', ".");
    let mhz: f64 = normalized.parse().ok()?;
    Some((mhz * 1_000_000.0).round() as u64)
}

/// Format Hz to European MHz string: 144525000 -> "144,52500"
fn format_freq_mhz(hz: u64) -> String {
    let mhz = hz as f64 / 1_000_000.0;
    format!("{:.5}", mhz).replace('.', ",")
}

/// Parse a .tab file (FT-991A Programmer export).
pub fn parse_tab_file(path: &Path) -> Result<Vec<YaesuMemoryChannel>, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Read {}: {}", path.display(), e))?;
    parse_tab_string(&content)
}

/// Parse tab-separated text (same format as .tab file).
pub fn parse_tab_string(content: &str) -> Result<Vec<YaesuMemoryChannel>, String> {

    let mut lines = content.lines();
    let header = lines.next().ok_or("Empty file")?;

    // Find column indices from header
    let cols: Vec<&str> = header.split('\t').collect();
    let find_col = |name: &str| -> Option<usize> {
        cols.iter().position(|c| c.trim().eq_ignore_ascii_case(name))
    };

    let col_ch = find_col("Channel Number");
    let col_rx = find_col("Receive Frequency");
    let col_tx = find_col("Transmit Frequency");
    let col_offset = find_col("Offset Frequency");
    let col_dir = find_col("Offset Direction");
    let col_mode = find_col("Operating Mode");
    let col_txmode = find_col("Tx Operating Mode");
    let col_name = find_col("Name");
    let col_tone = find_col("Tone Mode");
    let col_ctcss = find_col("CTCSS");
    let col_dcs = find_col("DCS");
    let col_narrow = find_col("Narrow");
    let col_skip = find_col("Skip");
    let col_att = find_col("Attenuator");
    let col_tuner = find_col("Tuner");
    let col_agc = find_col("AGC");
    let col_nb = find_col("Noise Blanker");
    let col_ipo = find_col("IPO");
    let col_dnr = find_col("DNR");
    let col_step = find_col("Step");
    let col_comment = find_col("Comment");

    let mut channels = Vec::new();

    for line in lines {
        let line = line.trim();
        if line.is_empty() { continue; }

        let fields: Vec<&str> = line.split('\t').collect();
        let get = |idx: Option<usize>| -> &str {
            idx.and_then(|i| fields.get(i).map(|s| s.trim())).unwrap_or("")
        };

        let ch_str = get(col_ch);
        let channel_number: u16 = match ch_str.parse() {
            Ok(n) => n,
            Err(_) => continue,
        };

        let rx_freq_hz = parse_freq_mhz(get(col_rx)).unwrap_or(0);
        let tx_freq_hz = parse_freq_mhz(get(col_tx)).unwrap_or(rx_freq_hz);

        if rx_freq_hz == 0 { continue; }

        channels.push(YaesuMemoryChannel {
            channel_number,
            rx_freq_hz,
            tx_freq_hz,
            offset_freq: get(col_offset).to_string(),
            offset_direction: get(col_dir).to_string(),
            mode: get(col_mode).to_string(),
            tx_mode: get(col_txmode).to_string(),
            name: get(col_name).to_string(),
            tone_mode: get(col_tone).to_string(),
            ctcss: get(col_ctcss).to_string(),
            dcs: get(col_dcs).to_string(),
            narrow: get(col_narrow).eq_ignore_ascii_case("on"),
            skip: get(col_skip).eq_ignore_ascii_case("on"),
            attenuator: get(col_att).eq_ignore_ascii_case("on"),
            tuner: get(col_tuner).eq_ignore_ascii_case("on"),
            agc: { let v = get(col_agc); if v.is_empty() { "Auto".into() } else { v.to_string() } },
            noise_blanker: get(col_nb).eq_ignore_ascii_case("on"),
            ipo: { let v = get(col_ipo); if v.is_empty() || v.eq_ignore_ascii_case("off") { "IPO".into() } else { v.to_string() } },
            dnr: { let v = get(col_dnr); if v.is_empty() || v.eq_ignore_ascii_case("off") { "Off".into() } else { v.to_string() } },
            step: { let v = get(col_step); if v.is_empty() { "6.25 kHz".into() } else { v.to_string() } },
            comment: get(col_comment).to_string(),
        });
    }

    Ok(channels)
}

/// Save channels to a .tab file (same format as FT-991A Programmer export).
pub fn save_tab_file(path: &Path, channels: &[YaesuMemoryChannel]) -> Result<(), String> {
    let mut out = String::new();

    // Header
    out.push_str("Channel Number\tReceive Frequency\tTransmit Frequency\tOffset Frequency\tOffset Direction\tOperating Mode\tTx Operating Mode\tName\tTone Mode\tCTCSS\tDCS\tNarrow\tSkip\tAttenuator\tTuner\tAGC\tNoise Blanker\tIPO\tDNR\tStep\tComment\t\n");

    for ch in channels {
        // Calculate TX freq from RX + offset direction + offset freq
        let tx_hz = calc_tx_freq(ch.rx_freq_hz, &ch.offset_direction, &ch.offset_freq);
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t\n",
            ch.channel_number,
            format_freq_mhz(ch.rx_freq_hz),
            format_freq_mhz(tx_hz),
            ch.offset_freq,
            ch.offset_direction,
            ch.mode,
            ch.tx_mode,
            ch.name,
            ch.tone_mode,
            ch.ctcss,
            ch.dcs,
            if ch.narrow { "On" } else { "Off" },
            if ch.skip { "On" } else { "Off" },
            if ch.attenuator { "On" } else { "Off" },
            if ch.tuner { "On" } else { "Off" },
            ch.agc,
            if ch.noise_blanker { "On" } else { "Off" },
            ch.ipo,
            ch.dnr,
            ch.step,
            ch.comment,
        ));
    }

    std::fs::write(path, &out)
        .map_err(|e| format!("Write {}: {}", path.display(), e))
}

/// Map mode string from .tab file to the Yaesu CAT mode character.
/// IMPORTANT: FM modes are sent as DATA-FM ('A') because USB mic audio
/// only works in DATA-FM mode on the FT-991A (FM PKT PORT SELECT=USB).
pub fn mode_string_to_yaesu_cat(mode: &str) -> char {
    match mode.trim() {
        "LSB" => '1',
        "USB" => '2',
        "CW" => '3',
        "FM" | "FM-N" | "DATA-FM" | "C4FM" => 'A', // DATA-FM for USB mic
        "AM" | "AM-N" => '5',
        "RTTY-LSB" => '6',
        "CW-R" => '7',
        "DATA-LSB" => '8',
        "RTTY-USB" => '9',
        "DATA-USB" => 'C',
        _ => '2', // default USB
    }
}

/// Map mode string to internal mode number (Thetis numbering) for the server.
/// FM → DATA-FM (internal 5, but Yaesu CAT char 'A') for USB mic compatibility.
pub fn mode_string_to_internal(mode: &str) -> u8 {
    match mode.trim() {
        "LSB" => 0,
        "USB" => 1,
        "CW" => 3,
        "CW-R" => 4,
        "FM" | "FM-N" | "DATA-FM" | "C4FM" => 5, // all FM variants → internal FM
        "AM" | "AM-N" => 6,
        "RTTY-USB" | "DATA-USB" => 7,
        "RTTY-LSB" | "DATA-LSB" => 9,
        _ => 1, // default USB
    }
}

/// Format Hz for display: 144525000 -> "144.525.00"
pub fn format_freq_display(hz: u64) -> String {
    let mhz = hz / 1_000_000;
    let khz = (hz % 1_000_000) / 1_000;
    let sub = (hz % 1_000) / 10;
    format!("{}.{:03}.{:02}", mhz, khz, sub)
}

/// Parse offset frequency string to Hz. "600 kHz" → 600000, "1,60 MHz" → 1600000
pub fn parse_offset_hz(s: &str) -> u64 {
    let s = s.trim();
    if s.is_empty() { return 0; }
    if let Some(khz) = s.strip_suffix("kHz").or_else(|| s.strip_suffix(" kHz")) {
        if let Ok(v) = khz.trim().replace(',', ".").parse::<f64>() {
            return (v * 1_000.0).round() as u64;
        }
    }
    if let Some(mhz) = s.strip_suffix("MHz").or_else(|| s.strip_suffix(" MHz")) {
        if let Ok(v) = mhz.trim().replace(',', ".").parse::<f64>() {
            return (v * 1_000_000.0).round() as u64;
        }
    }
    0
}

/// Calculate TX freq from RX freq, offset direction and offset frequency.
pub fn calc_tx_freq(rx_hz: u64, direction: &str, offset: &str) -> u64 {
    let off_hz = parse_offset_hz(offset);
    match direction {
        "Plus" => rx_hz + off_hz,
        "Minus" => rx_hz.saturating_sub(off_hz),
        _ => rx_hz, // Simplex
    }
}

/// All operating modes for combo box.
pub const MODES: &[&str] = &[
    "LSB", "USB", "CW", "CW-R", "FM", "FM-N", "AM", "AM-N",
    "RTTY-LSB", "RTTY-USB", "DATA-LSB", "DATA-USB", "DATA-FM", "C4FM",
];

/// Offset directions for combo box.
pub const OFFSET_DIRS: &[&str] = &["Simplex", "Minus", "Plus", "Split"];

/// Tone modes for combo box.
pub const TONE_MODES: &[&str] = &["None", "Tone", "T SQL", "DCS", "D Code"];

/// Offset frequencies for combo box.
pub const OFFSET_FREQS: &[&str] = &[
    "", "100 kHz", "500 kHz", "600 kHz", "1 MHz", "1,6 MHz",
    "3 MHz", "5 MHz", "7,6 MHz", "9,4 MHz",
];

/// AGC modes for combo box.
pub const AGC_MODES: &[&str] = &["Off", "Auto", "Fast", "Mid", "Slow"];

/// IPO modes for combo box.
pub const IPO_MODES: &[&str] = &["IPO", "AMP1", "AMP2"];

/// DNR levels for combo box.
pub const DNR_LEVELS: &[&str] = &[
    "Off", "1", "2", "3", "4", "5", "6", "7",
    "8", "9", "10", "11", "12", "13", "14", "15",
];

/// Step sizes for combo box.
pub const STEPS: &[&str] = &[
    "5 kHz", "6.25 kHz", "10 kHz", "12.5 kHz", "15 kHz", "20 kHz", "25 kHz",
];

/// Common CTCSS tones for combo box.
pub const CTCSS_TONES: &[&str] = &[
    "67.0 Hz", "69.3 Hz", "71.9 Hz", "74.4 Hz", "77.0 Hz", "79.7 Hz",
    "82.5 Hz", "85.4 Hz", "88.5 Hz", "91.5 Hz", "94.8 Hz", "97.4 Hz",
    "100.0 Hz", "103.5 Hz", "107.2 Hz", "110.9 Hz", "114.8 Hz", "118.8 Hz",
    "123.0 Hz", "127.3 Hz", "131.8 Hz", "136.5 Hz", "141.3 Hz", "146.2 Hz",
    "151.4 Hz", "156.7 Hz", "159.8 Hz", "162.2 Hz", "165.5 Hz", "167.9 Hz",
    "171.3 Hz", "173.8 Hz", "177.3 Hz", "179.9 Hz", "183.5 Hz", "186.2 Hz",
    "189.9 Hz", "192.8 Hz", "196.6 Hz", "199.5 Hz", "203.5 Hz", "206.5 Hz",
    "210.7 Hz", "218.1 Hz", "225.7 Hz", "229.1 Hz", "233.6 Hz", "241.8 Hz",
    "250.3 Hz", "254.1 Hz",
];
