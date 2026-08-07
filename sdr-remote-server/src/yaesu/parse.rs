// SPDX-License-Identifier: GPL-2.0-or-later
//! Yaesu CAT response parser: drains complete (`;`-terminated) responses from
//! the read buffer and folds them into the shared YaesuState (freq/mode/meters/
//! status). Extracted verbatim from `yaesu/mod.rs` - pure relocation, no
//! behaviour/CAT/timing change. `use super::*;` reaches the shared imports
//! (Arc/Mutex, HashSet, RadioModel, YaesuState) and the mode-conversion + CAT
//! helpers in the parent; `pub(super)` keeps it callable from the poll loop.

use super::*;

/// Parse all complete responses (semicolon-terminated) from the buffer.
/// `prefix` = per-radio log tag (`[radio{N}/{MODEL}]`); `warned_modes` /
/// `warned_short_if` = warn-once guards against 500 ms-poll spam.
pub(super) fn parse_responses(
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
                    // Fail-safe: an unknown mode code would silently default to
                    // USB - warn once per unique char so an FTX-1-
                    // specific mode becomes visible during testing instead of hidden.
                    let known = matches!(mode_char,
                        '1'..='9'|'A'|'a'|'B'|'b'|'C'|'c'|'D'|'d'|'E'|'e'|'F'|'f'|'H'|'h'|'I'|'i');
                    if !known && warned_modes.insert(mode_char) {
                        warn!("{} unknown MD mode code '{}' - falling back to USB; possibly model-specific", prefix, mode_char);
                    }
                    let mode = yaesu_mode_to_internal(mode_char, model);
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
            "RM" => {
                // 991A RM6 = raw SWR meter (000-255). DIAGNOSTIC ONLY now — the alarm
                // comes from the official RI0 Hi-SWR flag (see the "RI" arm). We keep
                // the raw value so the RI0 log can show what RM6 read at the same moment
                // (e.g. to confirm what a dummy load reads on RM6 vs the RI0 flag).
                if !matches!(model, RadioModel::Ftx1) && payload.starts_with('6') {
                    if let Ok(raw) = payload[1..].trim().parse::<u16>() {
                        status.lock().unwrap().swr_meter_raw = raw;
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
                // nnn=watts -> payload 4 chars. 991A: "PC{nnn}" -> payload 3 chars.
                // Detect on length so both models are correct.
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
                // 991A: SC{P2} -> scan state at [0]. FTX-1: SC{P1}{P2} -> MAIN/SUB side
                // at [0], scan state at [1] (P2: 0=off, 1=up, 2=down). Without model
                // awareness the FTX-1 read the side instead of the scan state.
                let scan_char = match model {
                    RadioModel::Ftx1 => payload.chars().nth(1).unwrap_or('0'),
                    _ => payload.chars().nth(0).unwrap_or('0'),
                };
                status.lock().unwrap().scan_active = scan_char != '0';
            }
            "AC" => {
                // Internal ATU readback. payload = P1 P2 P3 (3 chars).
                //   991A: AC00P3; (P1P2 fixed "00"), P3: 0=off, 1=on, 2=tuning.
                //   FTX-1: AC P1 P2 P3; the radio reports its tuner with P1=1, P2=0 (empirical:
                //     'AC100' when off). P2=2 = ATAS (out of scope). P3: 0=off,1=on,3=tuning.
                // Read P3 as long as P2=0 (tuner mode), regardless of P1. Normalize P3->0/1/2,
                // never pass raw through.
                let pc: Vec<char> = payload.chars().collect();
                let p2 = pc.get(1).copied().unwrap_or('0');
                let p3 = pc.get(2).copied().unwrap_or('0');
                let state = if p2 != '0' {
                    0 // ATAS or non-tuner-mode -> no internal-ATU state
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
                // Hardware-verified (§13, build 32) on 991A and FTX-1: 1=FAST, 2=MID,
                // 3=SLOW, and AUTO is reported back as the *resolved* auto speed
                // 4=auto-fast/5=auto-mid/6=auto-slow. We always set AUTO with 4 and
                // normalize every readback 4/5/6 -> 4 (AUTO) so readback == set value.
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
                // NB0 P2 -> Noise Blanker on/off (991A), YaesuCtrl::NbOn bit 13. FTX-1 sends no NB.
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
                // 1=freq (P4=sign, P5-P8=0000-9999 Hz). 991A uses RT/XT + accumulation.
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
                if matches!(model, RadioModel::Ftx1) {
                    // FTX-1 Radio Information answer = P1..P8 (per FTX-1 CAT manual):
                    //   P2 = 0 Normal / 1 Hi-SWR
                    //   P4 = 0 RX / 1 TX / 2 TX-INHIBIT
                    //   P8 = 0 SQL closed / 1 SQL open (BUSY)  <- drives software-squelch
                    if let Some(p8) = payload.chars().last() {
                        let open = p8 == '1';
                        let mut s = status.lock().unwrap();
                        if open != s.squelch_open {
                            info!("{} squelch: {}", prefix, if open { "OPEN (BUSY)" } else { "CLOSED" });
                            s.squelch_open = open;
                        }
                    }
                    // High-SWR flag = P2 (0 Normal / 1 Hi-SWR). The radio only asserts it
                    // while transmitting into a high SWR; it clears otherwise.
                    let hi = payload.chars().nth(1) == Some('1');
                    {
                        let mut s = status.lock().unwrap();
                        if hi != s.hi_swr {
                            if hi { warn!("{} HIGH SWR (RI P2=1)", prefix); }
                            s.hi_swr = hi;
                        }
                    }
                } else {
                    // 991A: RI0; -> answer "RI0{P2}" with P2 = 0 Normal / 1 Hi-SWR
                    // (FT-991A CAT OM 1711-D). This is the radio's OWN calibrated Hi-SWR
                    // trip, asserted only while transmitting into a high SWR — no per-radio
                    // threshold guessing (unlike the old RM6 approach that could false-fire
                    // on a matched/dummy load). payload = "0" + P2, so P2 = char at index 1.
                    let hi = payload.chars().nth(1) == Some('1');
                    let mut s = status.lock().unwrap();
                    if hi != s.hi_swr {
                        if hi { warn!("{} HIGH SWR (991A RI0 P2=1; RM6 meter was {})", prefix, s.swr_meter_raw); }
                        s.hi_swr = hi;
                    }
                }
            }
            "IF" => {
                // IF field layout differs per model (see reference_ftx1_cat_protocol):
                //   991A : channel at [0..3], P7 (VFO/Mem) at [20], payload >=23.
                //   FTX-1: 5-digit channel [0..5], P7 at [22], payload 27 (same
                //          layout as MR/MW: P1(5) P2(9:freq) P3(5) P4 P5 P6 P7 ...).
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
                    if new_vfo == 0 {
                        // Radio confirms VFO -> escape landed, lift the guard.
                        if s.vfo_escape_pending != 0 {
                            s.vfo_escape_pending = 0;
                        }
                        if new_vfo != s.vfo_select {
                            info!("{} mode: VFO (IF P7='{}')", prefix, p7);
                            s.vfo_select = new_vfo;
                        }
                    } else if s.vfo_escape_pending != 0 {
                        // We just escaped memory->VFO; this "still Memory" response
                        // is usually an in-flight/stale IF-poll. Ignore and count down, so
                        // a truly failed escape still re-syncs after ~15 polls.
                        // Smoking-gun diagnostic: only WARN if all 15 polls still report Memory
                        // (guard exhausted) - then MA really did not switch the set to
                        // VFO operation (or the burst was rejected). A single
                        // stale poll at the start is normal and must not give a false error.
                        s.vfo_escape_pending -= 1;
                        if s.vfo_escape_pending == 0 {
                            warn!("{} memory-escape not confirmed after 15 IF-polls - radio stayed Memory (P7='{}'); MA did NOT switch to VFO (or MD/FA rejected)", prefix, p7);
                        }
                    } else if new_vfo != s.vfo_select {
                        info!("{} mode: {} (IF P7='{}')",
                            prefix, match new_vfo { 1 => "Memory", _ => "MemTune" }, p7);
                        s.vfo_select = new_vfo;
                    }
                    if let Ok(mc) = payload[0..ch_end].parse::<u16>() {
                        s.memory_channel = mc;
                    }
                    // 991A clarifier offset from IF P3 (chars [ch_end+9 .. +14], e.g.
                    // "+0050" = +50 Hz). The 991A does not read the offset back separately (RT/XT
                    // only give on/off), so we take it here from IF -> the client now also follows
                    // a turn of the CLAR knob on the set itself, even in Memory mode
                    // (tester report B3). FTX-1 keeps its own CF001 offset path.
                    if !matches!(model, RadioModel::Ftx1) {
                        let cs = ch_end + 9;
                        if let Some(field) = payload.get(cs..cs + 5) {
                            let sign = field.as_bytes()[0];
                            if let Ok(mag) = field[1..].trim().parse::<i16>() {
                                let off = if sign == b'-' { -mag } else { mag };
                                s.feature_freqs[3] = off as u16;
                            }
                        }
                    }
                } else {
                    // Fail-safe: deviating IF length -> do not index
                    // (no out-of-range/panic), skip parse + one warn. Makes a
                    // shifted FTX-1 field layout visible instead of failing silently.
                    if !*warned_short_if {
                        warn!("{} IF response {}B ('{}'), 991A expects >=22 - fields possibly shifted, parse skipped",
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
