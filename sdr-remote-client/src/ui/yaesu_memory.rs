// SPDX-License-Identifier: GPL-2.0-or-later

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
    // The fields below are NOT in the radio's memory read (see
    // `sdr-remote-server/src/yaesu/memory.rs`): only Narrow is derivable, from
    // the mode code. `None` / an empty string therefore means "the radio did
    // not tell us", which the UI shows as "-". They are still parsed and
    // written so a .tab file from the Yaesu programmer keeps its values.
    pub narrow: Option<bool>,
    pub skip: Option<bool>,
    pub attenuator: Option<bool>,
    pub tuner: Option<bool>,
    pub agc: String,               // "Auto", "Fast", "Mid", "Slow", "Off"
    pub noise_blanker: Option<bool>,
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
            // Unknown by default: a new channel gets these from the radio or
            // from an imported .tab file, never invented here.
            ctcss: String::new(),
            dcs: String::new(),
            narrow: None,
            skip: None,
            attenuator: None,
            tuner: None,
            agc: String::new(),
            noise_blanker: None,
            ipo: String::new(),
            dnr: String::new(),
            step: String::new(),
            comment: String::new(),
        }
    }
}

/// Placeholder for a memory field the radio did not report, or that carries no
/// meaning in this channel's mode. One symbol for both cases, because for the
/// operator they amount to the same thing: nothing to read here.
pub const MEM_UNKNOWN: &str = "-";

/// Display text for a string memory field: empty reads as unknown.
pub fn mem_text(v: &str) -> String {
    if v.trim().is_empty() { MEM_UNKNOWN.to_string() } else { v.trim().to_string() }
}

/// Display text for an optional On/Off memory field.
pub fn mem_flag(v: Option<bool>) -> String {
    match v {
        Some(true) => "On".to_string(),
        Some(false) => "Off".to_string(),
        None => MEM_UNKNOWN.to_string(),
    }
}

/// True for the modes where repeater shift and CTCSS/DCS actually apply.
/// In SSB/CW/data the radio still stores a tone mode, but it does nothing -
/// showing it there suggests a setting that is not in play.
pub fn mem_mode_uses_tone(mode: &str) -> bool {
    matches!(mode.trim(), "FM" | "FM-N" | "DATA-FM" | "C4FM")
}

/// Tone value column: the CTCSS frequency or DCS code that belongs to the
/// channel's tone mode, or `-` when the mode has no tone (or none was read).
pub fn mem_tone_value(ch: &YaesuMemoryChannel) -> String {
    if !mem_mode_uses_tone(&ch.mode) {
        return MEM_UNKNOWN.to_string();
    }
    match ch.tone_mode.as_str() {
        "Tone" | "Tone ENC" | "T SQL" => mem_text(&ch.ctcss),
        "DCS" | "DCS ENC" | "D Code" => mem_text(&ch.dcs),
        _ => MEM_UNKNOWN.to_string(),
    }
}

/// Tone-mode column, blanked out in the modes where it does not apply.
pub fn mem_tone_mode(ch: &YaesuMemoryChannel) -> String {
    if !mem_mode_uses_tone(&ch.mode) || ch.tone_mode == "None" {
        return MEM_UNKNOWN.to_string();
    }
    ch.tone_mode.clone()
}

/// Parse an On/Off column. Empty (or anything unrecognized) is `None`:
/// the source did not report this field, which is not the same as "Off".
fn parse_on_off(s: &str) -> Option<bool> {
    match s.trim() {
        v if v.eq_ignore_ascii_case("on") => Some(true),
        v if v.eq_ignore_ascii_case("off") => Some(false),
        _ => None,
    }
}

/// Render an optional On/Off for the .tab file: unknown stays an empty column.
fn on_off_field(v: Option<bool>) -> &'static str {
    match v {
        Some(true) => "On",
        Some(false) => "Off",
        None => "",
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
            // An empty column means the source did not report it - keep that as
            // unknown instead of substituting a plausible-looking default.
            narrow: parse_on_off(get(col_narrow)),
            skip: parse_on_off(get(col_skip)),
            attenuator: parse_on_off(get(col_att)),
            tuner: parse_on_off(get(col_tuner)),
            agc: get(col_agc).to_string(),
            noise_blanker: parse_on_off(get(col_nb)),
            ipo: get(col_ipo).to_string(),
            dnr: get(col_dnr).to_string(),
            step: get(col_step).to_string(),
            comment: get(col_comment).to_string(),
        });
    }

    Ok(channels)
}

/// Save channels to a .tab file (same format as FT-991A Programmer export).
pub fn save_tab_file(path: &Path, channels: &[YaesuMemoryChannel]) -> Result<(), String> {
    std::fs::write(path, to_tab_text(channels))
        .map_err(|e| format!("Write {}: {}", path.display(), e))
}

/// Build the tab-separated memory text (same format as the saved `.tab` file).
/// Kept separate from `save_tab_file` so callers can build the text in memory
/// without writing to disk.
pub fn to_tab_text(channels: &[YaesuMemoryChannel]) -> String {
    let mut out = String::new();

    // Header
    out.push_str(sdr_remote_core::YAESU_MEMORY_TAB_HEADER);
    out.push('\n');

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
            on_off_field(ch.narrow),
            on_off_field(ch.skip),
            on_off_field(ch.attenuator),
            on_off_field(ch.tuner),
            ch.agc,
            on_off_field(ch.noise_blanker),
            ch.ipo,
            ch.dnr,
            ch.step,
            ch.comment,
        ));
    }

    out
}

/// Map mode string to internal mode number (Thetis numbering) for the server.
/// FM -> DATA-FM (internal 5, but Yaesu CAT char 'A') for USB mic compatibility.
pub fn mode_string_to_internal(mode: &str) -> u8 {
    match mode.trim() {
        "LSB" => 0,
        "USB" => 1,
        "CW" => 3,
        "CW-R" => 4,
        "FM" | "FM-N" | "DATA-FM" | "C4FM" => 5, // all FM variants -> internal FM
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

/// Parse offset frequency string to Hz. "600 kHz" -> 600000, "1,60 MHz" -> 1600000
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

/// CTCSS tones for the memory editor. Same table and order as the radio's tone
/// numbers (0-49), so the server can map a label back to the CN index.
pub const CTCSS_TONES: &[&str] = &[
    "67.0 Hz", "69.3 Hz", "71.9 Hz", "74.4 Hz", "77.0 Hz", "79.7 Hz", "82.5 Hz", "85.4 Hz",
    "88.5 Hz", "91.5 Hz", "94.8 Hz", "97.4 Hz", "100.0 Hz", "103.5 Hz", "107.2 Hz", "110.9 Hz",
    "114.8 Hz", "118.8 Hz", "123.0 Hz", "127.3 Hz", "131.8 Hz", "136.5 Hz", "141.3 Hz",
    "146.2 Hz", "151.4 Hz", "156.7 Hz", "159.8 Hz", "162.2 Hz", "165.5 Hz", "167.9 Hz",
    "171.3 Hz", "173.8 Hz", "177.3 Hz", "179.9 Hz", "183.5 Hz", "186.2 Hz", "189.9 Hz",
    "192.8 Hz", "196.6 Hz", "199.5 Hz", "203.5 Hz", "206.5 Hz", "210.7 Hz", "218.1 Hz",
    "225.7 Hz", "229.1 Hz", "233.6 Hz", "241.8 Hz", "250.3 Hz", "254.1 Hz",
];

/// DCS codes for the memory editor, in the radio's own code order (0-103) so
/// the server can map a label back to the CN code number. From the CAT manual's
/// DCS chart, not typed by hand.
pub const DCS_CODES: &[&str] = &[
    "023", "025", "026", "031", "032", "036", "043", "047", "051", "053",
    "054", "065", "071", "072", "073", "074", "114", "115", "116", "122",
    "125", "131", "132", "134", "143", "145", "152", "155", "156", "162",
    "165", "172", "174", "205", "212", "223", "225", "226", "243", "244",
    "245", "246", "251", "252", "255", "261", "263", "265", "266", "271",
    "274", "306", "311", "315", "325", "331", "332", "343", "346", "351",
    "356", "364", "365", "371", "411", "412", "413", "423", "431", "432",
    "445", "446", "452", "454", "455", "462", "464", "465", "466", "503",
    "506", "516", "523", "526", "532", "546", "565", "606", "612", "624",
    "627", "631", "632", "654", "662", "664", "703", "712", "723", "731",
    "732", "734", "743", "754",
];

/// Tone modes for combo box.
pub const TONE_MODES: &[&str] = &["None", "Tone", "T SQL", "DCS", "D Code"];
