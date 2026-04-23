// SPDX-License-Identifier: GPL-2.0-or-later

//! FT-991A EX menu definitions (153 items) for the menu editor.
//! Data from Yaesu CAT Operation Reference Manual + Operating Manual.

/// A single menu item definition.
pub struct MenuDef {
    pub number: u16,
    pub name: &'static str,
    pub p2_digits: u8,
    /// Encoding: either enum options "0:OFF 1:ON" or range "000-100"
    pub encoding: &'static str,
    pub default: &'static str,
}

/// Runtime state for a menu item (read from radio).
#[derive(Clone, Debug)]
pub struct MenuItem {
    pub number: u16,
    pub raw_value: String,  // raw P2 string from radio
}

/// All 153 EX menu definitions.
pub const MENU_DEFS: &[MenuDef] = &[
    MenuDef { number: 1, name: "AGC FAST DELAY", p2_digits: 4, encoding: "0020-4000 msec", default: "0300" },
    MenuDef { number: 2, name: "AGC MID DELAY", p2_digits: 4, encoding: "0020-4000 msec", default: "0700" },
    MenuDef { number: 3, name: "AGC SLOW DELAY", p2_digits: 4, encoding: "0020-4000 msec", default: "3000" },
    MenuDef { number: 4, name: "HOME FUNCTION", p2_digits: 1, encoding: "0:SCOPE 1:FUNCTION", default: "0" },
    MenuDef { number: 5, name: "MY CALL INDICATION", p2_digits: 1, encoding: "0:OFF 1:1s 2:2s 3:3s 4:4s 5:5s", default: "1" },
    MenuDef { number: 6, name: "DISPLAY COLOR", p2_digits: 1, encoding: "0:BLUE 1:GRAY 2:GREEN 3:ORANGE 4:PURPLE 5:RED 6:SKY BLUE", default: "0" },
    MenuDef { number: 7, name: "DIMMER LED", p2_digits: 1, encoding: "0:1 1:2", default: "1" },
    MenuDef { number: 8, name: "DIMMER TFT", p2_digits: 2, encoding: "00-15", default: "08" },
    MenuDef { number: 9, name: "BAR MTR PEAK HOLD", p2_digits: 1, encoding: "0:OFF 1:0.5s 2:1.0s 3:2.0s", default: "0" },
    MenuDef { number: 10, name: "DVS RX OUT LEVEL", p2_digits: 3, encoding: "000-100", default: "050" },
    MenuDef { number: 11, name: "DVS TX OUT LEVEL", p2_digits: 3, encoding: "000-100", default: "050" },
    MenuDef { number: 12, name: "KEYER TYPE", p2_digits: 1, encoding: "0:OFF 1:BUG 2:ELEKEY-A 3:ELEKEY-B 4:ELEKEY-Y 5:ACS", default: "3" },
    MenuDef { number: 13, name: "KEYER DOT/DASH", p2_digits: 1, encoding: "0:NORMAL 1:REVERSE", default: "0" },
    MenuDef { number: 14, name: "CW WEIGHT", p2_digits: 2, encoding: "25-45", default: "30" },
    MenuDef { number: 15, name: "BEACON INTERVAL", p2_digits: 3, encoding: "000-690", default: "000" },
    MenuDef { number: 16, name: "NUMBER STYLE", p2_digits: 1, encoding: "0:1290 1:AUNO 2:AUNT 3:A2NO 4:A2NT 5:12NO 6:12NT", default: "0" },
    MenuDef { number: 17, name: "CONTEST NUMBER", p2_digits: 4, encoding: "0000-9999", default: "0001" },
    MenuDef { number: 18, name: "CW MEMORY 1", p2_digits: 1, encoding: "0:TEXT 1:MESSAGE", default: "0" },
    MenuDef { number: 19, name: "CW MEMORY 2", p2_digits: 1, encoding: "0:TEXT 1:MESSAGE", default: "0" },
    MenuDef { number: 20, name: "CW MEMORY 3", p2_digits: 1, encoding: "0:TEXT 1:MESSAGE", default: "0" },
    MenuDef { number: 21, name: "CW MEMORY 4", p2_digits: 1, encoding: "0:TEXT 1:MESSAGE", default: "0" },
    MenuDef { number: 22, name: "CW MEMORY 5", p2_digits: 1, encoding: "0:TEXT 1:MESSAGE", default: "0" },
    MenuDef { number: 23, name: "NB WIDTH", p2_digits: 1, encoding: "0:1ms 1:3ms 2:10ms", default: "1" },
    MenuDef { number: 24, name: "NB REJECTION", p2_digits: 1, encoding: "0:10dB 1:30dB 2:50dB", default: "1" },
    MenuDef { number: 25, name: "NB LEVEL", p2_digits: 2, encoding: "00-10", default: "05" },
    MenuDef { number: 26, name: "BEEP LEVEL", p2_digits: 3, encoding: "000-100", default: "050" },
    MenuDef { number: 27, name: "TIME ZONE", p2_digits: 5, encoding: "UTC offset", default: "+0000" },
    MenuDef { number: 28, name: "GPS/232C SELECT", p2_digits: 1, encoding: "0:GPS1 1:GPS2 3:RS232C", default: "0" },
    MenuDef { number: 29, name: "232C RATE", p2_digits: 1, encoding: "0:4800 1:9600 2:19200 3:38400", default: "0" },
    MenuDef { number: 30, name: "232C TOT", p2_digits: 1, encoding: "0:10ms 1:100ms 2:1000ms 3:3000ms", default: "0" },
    MenuDef { number: 31, name: "CAT RATE", p2_digits: 1, encoding: "0:4800 1:9600 2:19200 3:38400", default: "0" },
    MenuDef { number: 32, name: "CAT TOT", p2_digits: 1, encoding: "0:10ms 1:100ms 2:1000ms 3:3000ms", default: "0" },
    MenuDef { number: 33, name: "CAT RTS", p2_digits: 1, encoding: "0:DISABLE 1:ENABLE", default: "1" },
    MenuDef { number: 34, name: "MEM GROUP", p2_digits: 1, encoding: "0:DISABLE 1:ENABLE", default: "0" },
    MenuDef { number: 35, name: "QUICK SPLIT FREQ", p2_digits: 3, encoding: "-20..+20 kHz", default: "005" },
    MenuDef { number: 36, name: "TX TOT", p2_digits: 2, encoding: "00:OFF 01-30 min", default: "00" },
    MenuDef { number: 37, name: "MIC SCAN", p2_digits: 1, encoding: "0:DISABLE 1:ENABLE", default: "1" },
    MenuDef { number: 38, name: "MIC SCAN RESUME", p2_digits: 1, encoding: "0:PAUSE 1:TIME", default: "1" },
    MenuDef { number: 39, name: "REF FREQ ADJ", p2_digits: 3, encoding: "-25..+25", default: "000" },
    MenuDef { number: 40, name: "CLAR MODE SELECT", p2_digits: 1, encoding: "0:RX 1:TX 2:TRX", default: "0" },
    MenuDef { number: 41, name: "AM LCUT FREQ", p2_digits: 2, encoding: "00:OFF 01-19:100-1000Hz", default: "00" },
    MenuDef { number: 42, name: "AM LCUT SLOPE", p2_digits: 1, encoding: "0:6dB/oct 1:18dB/oct", default: "0" },
    MenuDef { number: 43, name: "AM HCUT FREQ", p2_digits: 2, encoding: "00:OFF 01-67:700-4000Hz", default: "00" },
    MenuDef { number: 44, name: "AM HCUT SLOPE", p2_digits: 1, encoding: "0:6dB/oct 1:18dB/oct", default: "0" },
    MenuDef { number: 45, name: "AM MIC SELECT", p2_digits: 1, encoding: "0:MIC 1:REAR", default: "0" },
    MenuDef { number: 46, name: "AM OUT LEVEL", p2_digits: 3, encoding: "000-100", default: "050" },
    MenuDef { number: 47, name: "AM PTT SELECT", p2_digits: 1, encoding: "0:DAKY 1:RTS 2:DTR", default: "0" },
    MenuDef { number: 48, name: "AM PORT SELECT", p2_digits: 1, encoding: "0:DATA 1:USB", default: "0" },
    MenuDef { number: 49, name: "AM DATA GAIN", p2_digits: 3, encoding: "000-100", default: "050" },
    MenuDef { number: 50, name: "CW LCUT FREQ", p2_digits: 2, encoding: "00:OFF 01-19:100-1000Hz", default: "04" },
    MenuDef { number: 51, name: "CW LCUT SLOPE", p2_digits: 1, encoding: "0:6dB/oct 1:18dB/oct", default: "1" },
    MenuDef { number: 52, name: "CW HCUT FREQ", p2_digits: 2, encoding: "00:OFF 01-67:700-4000Hz", default: "11" },
    MenuDef { number: 53, name: "CW HCUT SLOPE", p2_digits: 1, encoding: "0:6dB/oct 1:18dB/oct", default: "1" },
    MenuDef { number: 54, name: "CW OUT LEVEL", p2_digits: 3, encoding: "000-100", default: "050" },
    MenuDef { number: 55, name: "CW AUTO MODE", p2_digits: 1, encoding: "0:OFF 1:50MHz 2:ON", default: "0" },
    MenuDef { number: 56, name: "CW BK-IN TYPE", p2_digits: 1, encoding: "0:SEMI 1:FULL", default: "0" },
    MenuDef { number: 57, name: "CW BK-IN DELAY", p2_digits: 4, encoding: "0030-3000 msec", default: "0200" },
    MenuDef { number: 58, name: "CW WAVE SHAPE", p2_digits: 1, encoding: "0:1ms 1:2ms 2:4ms 3:6ms", default: "2" },
    MenuDef { number: 59, name: "CW FREQ DISPLAY", p2_digits: 1, encoding: "0:DIRECT FREQ 1:PITCH OFFSET", default: "1" },
    MenuDef { number: 60, name: "PC KEYING", p2_digits: 1, encoding: "0:OFF 1:DAKY 2:RTS 3:DTR", default: "0" },
    MenuDef { number: 61, name: "QSK DELAY TIME", p2_digits: 1, encoding: "0:15ms 1:20ms 2:25ms 3:30ms", default: "0" },
    MenuDef { number: 62, name: "DATA MODE", p2_digits: 1, encoding: "0:PSK 1:OTHER", default: "0" },
    MenuDef { number: 63, name: "PSK TONE", p2_digits: 1, encoding: "0:1000Hz 1:1500Hz 2:2000Hz", default: "0" },
    MenuDef { number: 64, name: "OTHER DISP (SSB)", p2_digits: 5, encoding: "-3000..+3000 Hz", default: "+0000" },
    MenuDef { number: 65, name: "OTHER SHIFT (SSB)", p2_digits: 5, encoding: "-3000..+3000 Hz", default: "+0000" },
    MenuDef { number: 66, name: "DATA LCUT FREQ", p2_digits: 2, encoding: "00:OFF 01-19:100-1000Hz", default: "05" },
    MenuDef { number: 67, name: "DATA LCUT SLOPE", p2_digits: 1, encoding: "0:6dB/oct 1:18dB/oct", default: "1" },
    MenuDef { number: 68, name: "DATA HCUT FREQ", p2_digits: 2, encoding: "00:OFF 01-67:700-4000Hz", default: "47" },
    MenuDef { number: 69, name: "DATA HCUT SLOPE", p2_digits: 1, encoding: "0:6dB/oct 1:18dB/oct", default: "1" },
    MenuDef { number: 70, name: "DATA IN SELECT", p2_digits: 1, encoding: "0:MIC 1:REAR", default: "1" },
    MenuDef { number: 71, name: "DATA PTT SELECT", p2_digits: 1, encoding: "0:DAKY 1:RTS 2:DTR", default: "0" },
    MenuDef { number: 72, name: "DATA PORT SELECT", p2_digits: 1, encoding: "1:DATA 2:USB", default: "1" },
    MenuDef { number: 73, name: "DATA OUT LEVEL", p2_digits: 3, encoding: "000-100", default: "050" },
    MenuDef { number: 74, name: "FM MIC SELECT", p2_digits: 1, encoding: "0:MIC 1:REAR", default: "0" },
    MenuDef { number: 75, name: "FM OUT LEVEL", p2_digits: 3, encoding: "000-100", default: "050" },
    MenuDef { number: 76, name: "FM PKT PTT SELECT", p2_digits: 1, encoding: "0:DAKY 1:RTS 2:DTR", default: "0" },
    MenuDef { number: 77, name: "FM PKT PORT SELECT", p2_digits: 1, encoding: "1:DATA 2:USB", default: "1" },
    MenuDef { number: 78, name: "FM PKT TX GAIN", p2_digits: 3, encoding: "000-100", default: "050" },
    MenuDef { number: 79, name: "FM PKT MODE", p2_digits: 1, encoding: "0:1200 1:9600", default: "0" },
    MenuDef { number: 80, name: "RPT SHIFT 28MHz", p2_digits: 4, encoding: "0000-1000 kHz", default: "0100" },
    MenuDef { number: 81, name: "RPT SHIFT 50MHz", p2_digits: 4, encoding: "0000-4000 kHz", default: "1000" },
    MenuDef { number: 82, name: "RPT SHIFT 144MHz", p2_digits: 4, encoding: "0000-4000 kHz", default: "0600" },
    MenuDef { number: 83, name: "RPT SHIFT 430MHz", p2_digits: 5, encoding: "00000-10000 kHz", default: "05000" },
    MenuDef { number: 84, name: "ARS 144MHz", p2_digits: 1, encoding: "0:OFF 1:ON", default: "1" },
    MenuDef { number: 85, name: "ARS 430MHz", p2_digits: 1, encoding: "0:OFF 1:ON", default: "1" },
    MenuDef { number: 86, name: "DCS POLARITY", p2_digits: 1, encoding: "0:Tn-Rn 1:Tn-Riv 2:Tiv-Rn 3:Tiv-Riv", default: "0" },
    MenuDef { number: 87, name: "RADIO ID", p2_digits: 0, encoding: "read-only", default: "" },
    MenuDef { number: 88, name: "GM DISPLAY", p2_digits: 1, encoding: "0:DISTANCE 1:STRENGTH", default: "0" },
    MenuDef { number: 89, name: "DISTANCE", p2_digits: 1, encoding: "0:km 1:mile", default: "1" },
    MenuDef { number: 90, name: "AMS TX MODE", p2_digits: 1, encoding: "0:AUTO 1:MANUAL 2:DN 3:VW 4:ANALOG", default: "0" },
    MenuDef { number: 91, name: "STANDBY BEEP", p2_digits: 1, encoding: "0:OFF 1:ON", default: "1" },
    MenuDef { number: 92, name: "RTTY LCUT FREQ", p2_digits: 2, encoding: "00:OFF 01-19:100-1000Hz", default: "05" },
    MenuDef { number: 93, name: "RTTY LCUT SLOPE", p2_digits: 1, encoding: "0:6dB/oct 1:18dB/oct", default: "1" },
    MenuDef { number: 94, name: "RTTY HCUT FREQ", p2_digits: 2, encoding: "00:OFF 01-67:700-4000Hz", default: "47" },
    MenuDef { number: 95, name: "RTTY HCUT SLOPE", p2_digits: 1, encoding: "0:6dB/oct 1:18dB/oct", default: "1" },
    MenuDef { number: 96, name: "RTTY SHIFT PORT", p2_digits: 1, encoding: "0:SHIFT 1:DTR 2:RTS", default: "0" },
    MenuDef { number: 97, name: "RTTY POLARITY-RX", p2_digits: 1, encoding: "0:NORMAL 1:REVERSE", default: "0" },
    MenuDef { number: 98, name: "RTTY POLARITY-TX", p2_digits: 1, encoding: "0:NORMAL 1:REVERSE", default: "0" },
    MenuDef { number: 99, name: "RTTY OUT LEVEL", p2_digits: 3, encoding: "000-100", default: "050" },
    MenuDef { number: 100, name: "RTTY SHIFT FREQ", p2_digits: 1, encoding: "0:170Hz 1:200Hz 2:425Hz 3:850Hz", default: "0" },
    MenuDef { number: 101, name: "RTTY MARK FREQ", p2_digits: 1, encoding: "0:1275Hz 1:2125Hz", default: "1" },
    MenuDef { number: 102, name: "SSB LCUT FREQ", p2_digits: 2, encoding: "00:OFF 01-19:100-1000Hz", default: "01" },
    MenuDef { number: 103, name: "SSB LCUT SLOPE", p2_digits: 1, encoding: "0:6dB/oct 1:18dB/oct", default: "0" },
    MenuDef { number: 104, name: "SSB HCUT FREQ", p2_digits: 2, encoding: "00:OFF 01-67:700-4000Hz", default: "47" },
    MenuDef { number: 105, name: "SSB HCUT SLOPE", p2_digits: 1, encoding: "0:6dB/oct 1:18dB/oct", default: "0" },
    MenuDef { number: 106, name: "SSB MIC SELECT", p2_digits: 1, encoding: "0:MIC 1:REAR", default: "0" },
    MenuDef { number: 107, name: "SSB OUT LEVEL", p2_digits: 3, encoding: "000-100", default: "050" },
    MenuDef { number: 108, name: "SSB PTT SELECT", p2_digits: 1, encoding: "0:DAKY 1:RTS 2:DTR", default: "0" },
    MenuDef { number: 109, name: "SSB PORT SELECT", p2_digits: 1, encoding: "0:DATA 1:USB", default: "0" },
    MenuDef { number: 110, name: "SSB TX BPF", p2_digits: 1, encoding: "0:50-3000 1:100-2900 2:200-2800 3:300-2700 4:400-2600", default: "3" },
    MenuDef { number: 111, name: "APF WIDTH", p2_digits: 1, encoding: "0:NARROW 1:MEDIUM 2:WIDE", default: "1" },
    MenuDef { number: 112, name: "CONTOUR LEVEL", p2_digits: 3, encoding: "-40..+20", default: "-15" },
    MenuDef { number: 113, name: "CONTOUR WIDTH", p2_digits: 2, encoding: "01-11", default: "10" },
    MenuDef { number: 114, name: "IF NOTCH WIDTH", p2_digits: 1, encoding: "0:NARROW 1:WIDE", default: "1" },
    MenuDef { number: 115, name: "SCP DISPLAY MODE", p2_digits: 1, encoding: "0:SPECTRUM 1:WATERFALL", default: "0" },
    MenuDef { number: 116, name: "SCP SPAN FREQ", p2_digits: 2, encoding: "03:50kHz 04:100kHz 05:200kHz 06:500kHz 07:1000kHz", default: "04" },
    MenuDef { number: 117, name: "SPECTRUM COLOR", p2_digits: 1, encoding: "0:BLUE 1:GRAY 2:GREEN 3:ORANGE 4:PURPLE 5:RED 6:SKY BLUE", default: "0" },
    MenuDef { number: 118, name: "WATERFALL COLOR", p2_digits: 1, encoding: "0:BLUE 1:GRAY 2:GREEN 3:ORANGE 4:PURPLE 5:RED 6:SKY BLUE 7:MULTI", default: "7" },
    MenuDef { number: 119, name: "PRMTRC EQ1 FREQ", p2_digits: 2, encoding: "00:OFF 01-07:100-700Hz", default: "00" },
    MenuDef { number: 120, name: "PRMTRC EQ1 LEVEL", p2_digits: 3, encoding: "-20..+10", default: "+05" },
    MenuDef { number: 121, name: "PRMTRC EQ1 BWTH", p2_digits: 2, encoding: "01-10", default: "10" },
    MenuDef { number: 122, name: "PRMTRC EQ2 FREQ", p2_digits: 2, encoding: "00:OFF 01-09:700-1500Hz", default: "00" },
    MenuDef { number: 123, name: "PRMTRC EQ2 LEVEL", p2_digits: 3, encoding: "-20..+10", default: "+05" },
    MenuDef { number: 124, name: "PRMTRC EQ2 BWTH", p2_digits: 2, encoding: "01-10", default: "10" },
    MenuDef { number: 125, name: "PRMTRC EQ3 FREQ", p2_digits: 2, encoding: "00:OFF 01-18:1500-3200Hz", default: "00" },
    MenuDef { number: 126, name: "PRMTRC EQ3 LEVEL", p2_digits: 3, encoding: "-20..+10", default: "+05" },
    MenuDef { number: 127, name: "PRMTRC EQ3 BWTH", p2_digits: 2, encoding: "01-10", default: "10" },
    MenuDef { number: 128, name: "P-PRMTRC EQ1 FREQ", p2_digits: 2, encoding: "00:OFF 01-07:100-700Hz", default: "02" },
    MenuDef { number: 129, name: "P-PRMTRC EQ1 LEVEL", p2_digits: 3, encoding: "-20..+10", default: "+00" },
    MenuDef { number: 130, name: "P-PRMTRC EQ1 BWTH", p2_digits: 2, encoding: "01-10", default: "02" },
    MenuDef { number: 131, name: "P-PRMTRC EQ2 FREQ", p2_digits: 2, encoding: "00:OFF 01-09:700-1500Hz", default: "02" },
    MenuDef { number: 132, name: "P-PRMTRC EQ2 LEVEL", p2_digits: 3, encoding: "-20..+10", default: "+00" },
    MenuDef { number: 133, name: "P-PRMTRC EQ2 BWTH", p2_digits: 2, encoding: "01-10", default: "01" },
    MenuDef { number: 134, name: "P-PRMTRC EQ3 FREQ", p2_digits: 2, encoding: "00:OFF 01-18:1500-3200Hz", default: "07" },
    MenuDef { number: 135, name: "P-PRMTRC EQ3 LEVEL", p2_digits: 3, encoding: "-20..+10", default: "+00" },
    MenuDef { number: 136, name: "P-PRMTRC EQ3 BWTH", p2_digits: 2, encoding: "01-10", default: "01" },
    MenuDef { number: 137, name: "HF TX MAX POWER", p2_digits: 3, encoding: "005-100", default: "100" },
    MenuDef { number: 138, name: "50M TX MAX POWER", p2_digits: 3, encoding: "005-100", default: "100" },
    MenuDef { number: 139, name: "144M TX MAX POWER", p2_digits: 3, encoding: "005-050", default: "050" },
    MenuDef { number: 140, name: "430M TX MAX POWER", p2_digits: 3, encoding: "005-050", default: "050" },
    MenuDef { number: 141, name: "TUNER SELECT", p2_digits: 1, encoding: "0:OFF 1:INTERNAL 2:EXTERNAL 3:ATAS 4:LAMP", default: "1" },
    MenuDef { number: 142, name: "VOX SELECT", p2_digits: 1, encoding: "0:MIC 1:DATA", default: "0" },
    MenuDef { number: 143, name: "VOX GAIN", p2_digits: 3, encoding: "000-100", default: "050" },
    MenuDef { number: 144, name: "VOX DELAY", p2_digits: 4, encoding: "0030-3000 msec", default: "0500" },
    MenuDef { number: 145, name: "ANTI VOX GAIN", p2_digits: 3, encoding: "000-100", default: "050" },
    MenuDef { number: 146, name: "DATA VOX GAIN", p2_digits: 3, encoding: "000-100", default: "050" },
    MenuDef { number: 147, name: "DATA VOX DELAY", p2_digits: 4, encoding: "0030-3000 msec", default: "0100" },
    MenuDef { number: 148, name: "ANTI DVOX GAIN", p2_digits: 3, encoding: "000-100", default: "000" },
    MenuDef { number: 149, name: "EMERGENCY FREQ TX", p2_digits: 1, encoding: "0:DISABLE 1:ENABLE", default: "0" },
    MenuDef { number: 150, name: "PRT/WIRES FREQ", p2_digits: 1, encoding: "0:MANUAL 1:PRESET", default: "0" },
    MenuDef { number: 151, name: "PRESET FREQUENCY", p2_digits: 8, encoding: "00030000-47000000", default: "14537500" },
    MenuDef { number: 152, name: "SEARCH SETUP", p2_digits: 1, encoding: "0:HISTORY 1:ACTIVITY", default: "0" },
    MenuDef { number: 153, name: "WIRES DG-ID", p2_digits: 2, encoding: "00:AUTO 01-99", default: "00" },
];

/// Parse encoding string to check if it's an enumeration (contains ':').
pub fn is_enum(encoding: &str) -> bool {
    encoding.contains(':')
}

/// Parse enum encoding "0:OFF 1:ON 2:AUTO" into vec of (code, label).
pub fn parse_enum_options(encoding: &str) -> Vec<(String, String)> {
    encoding.split_whitespace()
        .filter_map(|item| {
            let mut parts = item.splitn(2, ':');
            let code = parts.next()?.to_string();
            let label = parts.next()?.to_string();
            Some((code, label))
        })
        .collect()
}

/// Format a display value from raw P2 and encoding.
pub fn format_value(raw: &str, encoding: &str) -> String {
    if is_enum(encoding) {
        let options = parse_enum_options(encoding);
        for (code, label) in &options {
            if code == raw {
                return label.clone();
            }
        }
        raw.to_string()
    } else {
        raw.to_string()
    }
}
