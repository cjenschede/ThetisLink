// SPDX-License-Identifier: GPL-2.0-or-later
//! Yaesu memory-channel and EX-menu read/write over CAT: the FT-991A `MT`/menu
//! bulk read+write and the FTX-1 equivalents (memories, PMS, EX-menu chart), plus
//! their tag/mode-label helpers. Extracted verbatim from `yaesu/mod.rs` - pure
//! relocation, no behaviour/CAT change. `use super::*;` pulls in the shared types and
//! the CAT-query helpers; `pub(super)` keeps the bulk read/write callable from the
//! poll loop in the parent module.

use super::*;

/// CTCSS tone-frequency label for a tone number 0-49 (FT-991A CAT manual,
/// Table 1; the FTX-1 uses the same table). Out-of-range reads as unknown.
pub(super) fn ctcss_freq_label(n: u8) -> &'static str {
    const TONES: [&str; 50] = [
        "67.0 Hz", "69.3 Hz", "71.9 Hz", "74.4 Hz", "77.0 Hz", "79.7 Hz", "82.5 Hz", "85.4 Hz",
        "88.5 Hz", "91.5 Hz", "94.8 Hz", "97.4 Hz", "100.0 Hz", "103.5 Hz", "107.2 Hz", "110.9 Hz",
        "114.8 Hz", "118.8 Hz", "123.0 Hz", "127.3 Hz", "131.8 Hz", "136.5 Hz", "141.3 Hz",
        "146.2 Hz", "151.4 Hz", "156.7 Hz", "159.8 Hz", "162.2 Hz", "165.5 Hz", "167.9 Hz",
        "171.3 Hz", "173.8 Hz", "177.3 Hz", "179.9 Hz", "183.5 Hz", "186.2 Hz", "189.9 Hz",
        "192.8 Hz", "196.6 Hz", "199.5 Hz", "203.5 Hz", "206.5 Hz", "210.7 Hz", "218.1 Hz",
        "225.7 Hz", "229.1 Hz", "233.6 Hz", "241.8 Hz", "250.3 Hz", "254.1 Hz",
    ];
    TONES.get(n as usize).copied().unwrap_or("")
}

/// DCS code label for a code number 0-103 (FT-991A CAT manual, Table 2; the
/// FTX-1 uses the same chart). Out of range reads as unknown.
pub(super) fn dcs_code_label(n: u8) -> &'static str {
    const CODES: [&str; 104] = [
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
    CODES.get(n as usize).copied().unwrap_or("")
}

/// Inverse of [`dcs_code_label`]: DCS code label -> code number.
pub(super) fn dcs_num_from_label(label: &str) -> Option<u8> {
    (0u8..104).find(|&n| dcs_code_label(n) == label)
}

/// Inverse of [`ctcss_freq_label`]: tone-frequency label -> tone number.
pub(super) fn ctcss_num_from_label(label: &str) -> Option<u8> {
    (0u8..50).find(|&n| ctcss_freq_label(n) == label)
}

/// Narrow flag derived from the memory's mode code: the -N modes carry it
/// explicitly (`B` = FM-N, `D` = AM-N on both radios), and plain FM/AM
/// therefore mean narrow off. Every other mode stores its NAR flag outside
/// the MT/MR answer, so it stays unknown ("") rather than a guessed "Off".
fn narrow_from_mode(mode_char: char) -> &'static str {
    match mode_char {
        'B' | 'b' | 'D' | 'd' => "On",
        '4' | '5' => "Off",
        _ => "",
    }
}

/// CTCSS tone of the channel the radio is sitting on *right now*.
///
/// The per-memory tone is deliberately not derivable from the bulk read: P9 of
/// the MT/MR answer is documented as fixed "00" (FT-991A CAT manual, MR), so
/// the tone lives in the separate `CN` setting - which addresses the current
/// channel only. Reading it for all 117 channels would mean recalling every
/// channel on the radio, so we fill in the one channel we can read for free.
///
/// `current_mem_ch` is `Some(ch)` only when the radio is actually in memory
/// mode; in VFO mode `CN` describes the VFO and must not be attributed to a
/// memory channel. Returns `(channel, tone-label)`.
fn read_current_ctcss(
    port: &mut Box<dyn serialport::SerialPort>,
    current_mem_ch: Option<u16>,
) -> Option<(u16, &'static str)> {
    let ch = current_mem_ch?;
    // Read form is "CN P1 P2;" with P1=0 (fixed) and P2=0 (CTCSS, not DCS).
    let resp = cat_query(port, "CN00;");
    let start = resp.find("CN00")?;
    let digits: String = resp[start + 4..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let label = ctcss_freq_label(digits.parse::<u8>().ok()?);
    if label.is_empty() {
        return None;
    }
    log::debug!("CN: channel {} tone {}", ch, label);
    Some((ch, label))
}

/// Read all memory channels (001-117) from the FT-991A via MT commands.
/// Range per the FT-991A CAT spec: 001-099 = normal memories, 100-117 = PMS
/// (P-1L..P-9U). Previously the read stopped at 099 so channel 100+ was dropped;
/// the write already accepted 1-117. (The FTX-1 uses 00001-00099 + P-01L text
/// for PMS, so that read branch stays at 99 - see read_all_memories_ftx1.)
/// MT response format (41 chars):
///   MT P1(3:ch) P2(9:freq) P3(5:clar) P4(1:rxclar) P5(1:txclar)
///   P6(1:mode) P7(1:status) P8(1:tone) P9(2:00) P10(1:shift) P11(1:0) P12(12:TAG) ;
pub(super) fn read_all_memories(
    port: &mut Box<dyn serialport::SerialPort>,
    current_mem_ch: Option<u16>,
) -> Result<String, String> {
    let mut channels = Vec::new();

    // Before the long scan (one query, no channel switching) - see
    // `read_current_ctcss` for why this is the only tone we can honestly fill.
    let current_ctcss = read_current_ctcss(port, current_mem_ch);

    // A live radio answers EVERY channel quickly (programmed or empty -
    // the full 117-channel read takes <1s). A radio in standby/off does not
    // answer MT: every query times out (~300ms). Without abort the read grinds
    // on ~35s AND blocks the other CAT commands on the
    // single-threaded poll loop that whole time (e.g. a just-pressed power-ON). So abort
    // after a few CONSECUTIVE timeouts (slow empty responses); a single
    // slow-but-valid response resets the counter, and empty leading channels (which
    // answer quickly) do not trigger this.
    let mut consec_timeouts = 0u16;
    for ch in 1..=117u16 {
        let t0 = Instant::now();
        let response = cat_query(port, &format!("MT{:03};", ch));
        // Build 116 added an echo check here ("does the answer contain MT{ch:03}?")
        // and a per-channel retry, copied from the FTX-1 branch. It rejected valid
        // answers and cut the list to one or two channels: 117 channels went by in
        // 1.3 s, so the radio was answering - the assumption about what the answer
        // LOOKS like was wrong, and it was never measured. Reverted to the form
        // that reads this radio correctly. The raw probe below records the real
        // shape, so the next attempt can start from a measurement.
        if ch <= 3 {
            log::debug!("MT{:03} RAW probe: [{}] ({}B)", ch, response.escape_debug(), response.len());
        }
        let timed_out = response.trim().is_empty() && t0.elapsed() >= Duration::from_millis(250);
        if timed_out {
            consec_timeouts += 1;
            if consec_timeouts >= 4 {
                return Err("radio not responding (powered off?)".to_string());
            }
            continue;
        }
        consec_timeouts = 0;

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

                // P9 (`tone_num`) is documented as fixed "00", so it carries no
                // tone. Only the channel the radio currently sits on has a
                // readable tone (via CN); every other channel stays unknown.
                let _ = tone_num;
                let ctcss_freq = match current_ctcss {
                    Some((c, label)) if c == ch => label,
                    _ => "",
                };
                let narrow = narrow_from_mode(mode_char);

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

                // Columns after CTCSS are DCS, Narrow, Skip, Attenuator, Tuner,
                // AGC, Noise Blanker, IPO, DNR, Step, Comment. Only Narrow is
                // derivable (from the mode code); the rest is not in the MT
                // answer at all and stays EMPTY so the client can show it as
                // unknown. Filling them with "Off"/"Auto"/"6.25 kHz" presented
                // invented values as if they came from the radio.
                channels.push(format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t\t{}\t\t\t\t\t\t\t\t\t",
                    ch, freq_str, tx_freq_str, offset_freq_str, offset_dir, mode, mode,
                    display_name, tone_mode, ctcss_freq, narrow
                ));

                log::debug!("MT{:03}: {} {} {} {} {} ctcss={}", ch, display_name, freq_str, mode, tone_mode, offset_dir,
                    if ctcss_freq.is_empty() { "-" } else { ctcss_freq });
            }
        }
    }

    let mut out = String::new();
    out.push_str(sdr_remote_core::YAESU_MEMORY_TAB_HEADER);
    out.push('\n');
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
/// Ask the radio which memory channel it is on, rather than trusting the polled
/// status. The IF poll runs on its own cadence, so a channel the operator just
/// recalled may not be in `YaesuState` yet - and returning to a stale value
/// leaves the radio on the previous channel after a write.
/// The MC *read* form differs per model, and the difference is not cosmetic.
///
/// FT-991A: `MC;` -> `MC003;`.
/// FTX-1:   `MC P1 ;` where P1 is the side (0 = MAIN), so `MC0;` -> `MC000003;`
/// (FTX-1 CAT OM 2508-C p.19: Read = `M C P1 ;`, Answer = `M C P1 P2x5 ;`).
///
/// Sending the 991A form to an FTX-1 asks a malformed question and the radio
/// does not answer - which reads as "the radio did not confirm the channel" for
/// every channel in a walk, so "read tones" returns nothing while looking healthy.
fn mc_read(five_digit: bool) -> &'static str {
    if five_digit { "MC0;" } else { "MC;" }
}

fn current_memory_channel<P: CatPort + ?Sized>(port: &mut P, five_digit: bool) -> Option<u16> {
    let resp = port.query(mc_read(five_digit));
    let p = resp.find("MC")?;
    let digits: String = resp[p + 2..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    // FTX-1 answers "MC0" + 5 digits (side + channel); the 991A "MC" + 3.
    let digits = if five_digit && digits.len() > 5 { &digits[1..] } else { &digits[..] };
    digits.parse::<u16>().ok().filter(|&c| c >= 1)
}

/// Stop scanning if the radio is scanning, and report whether it was.
/// A scanning radio ignores a memory write, silently - the operator sees a
/// write that did nothing (`SC` per the CAT manual: P1 0 = off, 1 = up, 2 = down).
fn pause_scan<P: CatPort + ?Sized>(port: &mut P) -> Option<char> {
    let resp = port.query("SC;");
    let p = resp.find("SC")?;
    let mode = resp[p + 2..].chars().next()?;
    if mode == '0' {
        return None;
    }
    info!("Memory write: scan is running (SC{}), pausing it", mode);
    port.send("SC0;");
    std::thread::sleep(Duration::from_millis(80));
    Some(mode)
}

/// Radio state to return to after a memory write that had to recall channels
/// (see `write_memory_tones`). Captured by the poll loop before the write.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct MemoryWriteReturn {
    /// 0 = VFO, 1 = memory (from IF P7).
    pub vfo_select: u8,
    pub memory_channel: u16,
    pub vfo_a_freq: u64,
}

/// Write the CTCSS tone of each affected channel.
///
/// The tone is deliberately not part of the memory write: P9 of MT/MR is
/// documented as fixed "00" (FT-991A CAT manual). The tone lives in the
/// separate `CN` setting, which addresses whichever channel the radio is on -
/// so the only way to give a memory its tone is to recall that channel, set
/// `CN`, and store the channel again. That briefly moves the radio, so it runs
/// once at the end of a write and only for channels that actually carry a tone.
///
/// Returns the number of channels whose tone read back as requested.
/// `five_digit` selects the channel-number width: the 991A addresses memories
/// with 3 digits and stores via `MT`, the FTX-1 with 5 and stores via `MW`.
/// The `CN` command itself is identical on both (P1=0 is "fixed" on the 991A
/// and "MAIN-side" on the FTX-1, P2=0 = CTCSS), per both CAT manuals.
fn write_memory_tones<P: CatPort + ?Sized>(
    port: &mut P,
    entries: &[(u16, String, u8, bool)], // (channel, store cmd, value, is_dcs)
    ret: MemoryWriteReturn,
    five_digit: bool,
    is_tx: &dyn Fn() -> bool,
) -> usize {
    // Recall form per model, identical to `YaesuCmd::RecallMemory` in poll.rs:
    // the FTX-1 takes a MAIN/SUB side digit before the 5-digit channel, the
    // 991A a bare 3-digit channel. Without the side digit the FTX-1 rejects it.
    let recall = |ch: u16| if five_digit { format!("MC0{:05};", ch) } else { format!("MC{:03};", ch) };
    // Same reason as the read walk: a backlog from an earlier bulk read would
    // be mistaken for this walk's confirmations.
    port.drain();
    // Log the store command this radio actually uses (FTX-1 MW + 5 digits,
    // 991A MT + 3), so the log never names a command that was not sent.
    let tag = |ch: u16| if five_digit { format!("MW{:05}", ch) } else { format!("MT{:03}", ch) };
    // Returning to VFO needs a frequency to set (the escape is an FA write). If
    // the caller could not supply one, read it BEFORE the first recall - once we
    // start stepping channels the radio no longer reports the VFO we left.
    let mut ret = ret;
    // Prefer what the radio says over the polled snapshot.
    if ret.vfo_select == 1 {
        if let Some(ch) = current_memory_channel(port, five_digit) {
            ret.memory_channel = ch;
        }
    }
    if ret.vfo_select != 1 && ret.vfo_a_freq == 0 {
        let resp = port.query("FA;");
        if let Some(p) = resp.find("FA") {
            if let Ok(hz) = resp[p + 2..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse::<u64>()
            {
                ret.vfo_a_freq = hz;
            }
        }
        if ret.vfo_a_freq == 0 {
            warn!("tone write skipped: radio is in VFO mode but its VFO-A frequency is unknown,                    so it could not be returned there");
            return 0;
        }
    }

    let mut verified = 0;
    for (ch, mt_cmd, tone_num, is_dcs) in entries {
        // Recalling a channel changes the transmit frequency, so never do it
        // mid-transmission. Abort and restore rather than finish the round.
        if is_tx() {
            warn!("tone write aborted at channel {}: radio went into TX", ch);
            break;
        }
        // Set command: no reply to wait for (see CatPort::send).
        port.send(&recall(*ch));
        std::thread::sleep(Duration::from_millis(50));
        // CN: P1=0 (fixed/MAIN), P2 selects the table - 0 = CTCSS tone number
        // 000-049, 1 = DCS code number 000-103.
        let p2 = if *is_dcs { 1 } else { 0 };
        port.send(&format!("CN0{}{:03};", p2, tone_num));
        std::thread::sleep(Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(50));
        // Persist the tone into the channel. The two radios need different
        // steps, both established on hardware:
        //
        // 991A: re-storing with MT is what makes it stick - the repeater opens
        // and it survives a power cycle.
        // FTX-1: MW undoes it (no tone field there either, so it writes the
        // channel back without the tone), and doing nothing loses it at the
        // next channel change - that radio treats a CN change as memory-tune.
        // `AM;` is its store-to-memory command (CAT manual: "A M ;", no
        // parameters), the equivalent of pressing store on the front panel.
        if !five_digit {
            let _ = port.query(mt_cmd);
        } else {
            let _ = mt_cmd; // the main write pass already stored freq/mode/tag
            let _ = port.query("AM;");
        }
        std::thread::sleep(Duration::from_millis(50));
        // Read back rather than assume: whether the radio keeps a CN set this
        // way is not stated in the CAT manual, so report what it actually holds.
        let key = format!("CN0{}", p2);
        let resp = port.query(&format!("{};", key));
        let got = resp
            .find(&key)
            .map(|p| resp[p + 4..].chars().take_while(|c| c.is_ascii_digit()).collect::<String>())
            .and_then(|d| d.parse::<u8>().ok());
        // AM stores whatever the MAIN side currently holds. Had the radio not
        // actually been sitting on the recalled memory, that would write the VFO
        // into this channel and lose its frequency. Read the channel back and
        // say so, rather than quietly damaging a memory bank.
        if five_digit {
            let expect_freq = mt_cmd.get(7..16).unwrap_or("");
            let resp = port.query(&format!("MR{:05};", ch));
            let got_freq = resp
                .find(&format!("MR{:05}", ch))
                .and_then(|p| resp.get(p + 7..p + 16));
            match got_freq {
                Some(f) if f == expect_freq => {}
                Some(f) => warn!(
                    "MW{:05}: channel frequency changed by the tone store - expected {}, radio holds {}",
                    ch, expect_freq, f
                ),
                None => warn!("MW{:05}: no MR read-back after the tone store", ch),
            }
        }
        match got {
            Some(v) if v == *tone_num => {
                verified += 1;
                let label = if *is_dcs { dcs_code_label(*tone_num) } else { ctcss_freq_label(*tone_num) };
                info!("{} tone {} ({}) stored", tag(*ch), tone_num, label);
            }
            Some(v) => warn!(
                "{} tone write: asked {} ({}), radio holds {} ({})",
                tag(*ch), tone_num,
                if *is_dcs { dcs_code_label(*tone_num) } else { ctcss_freq_label(*tone_num) },
                v,
                if *is_dcs { dcs_code_label(v) } else { ctcss_freq_label(v) }
            ),
            None => warn!("{} tone write: no CN read-back", tag(*ch)),
        }
    }
    // Put the radio back where the operator left it. From VFO mode the escape is
    // a VFO-A frequency set - the same mechanism the poll loop uses elsewhere.
    if ret.vfo_select == 1 && ret.memory_channel >= 1 {
        let _ = port.query(&recall(ret.memory_channel));
    } else if ret.vfo_a_freq > 0 {
        let _ = port.query(&format!("FA{:09};", ret.vfo_a_freq));
    }
    verified
}

/// Read the CTCSS/DCS tone of every channel that has a tone mode.
///
/// The tone is a current-channel setting (`CN`), so the only way to learn it is
/// to recall each channel in turn - the same walk the write does, without
/// writing anything. Runs on request only: it moves the radio, so it is never
/// part of the ordinary memory read.
///
/// Returns `(channel, label, is_dcs)` per channel that answered.
pub(super) fn read_memory_tones<P: CatPort + ?Sized>(
    port: &mut P,
    entries: &[(u16, bool)], // (channel, wants DCS rather than CTCSS)
    ret: MemoryWriteReturn,
    five_digit: bool,
    is_tx: &dyn Fn() -> bool,
) -> Vec<(u16, String, bool)> {
    let recall = |ch: u16| if five_digit { format!("MC0{:05};", ch) } else { format!("MC{:03};", ch) };
    let mut out = Vec::new();
    // Start from an empty input buffer. This walk validates every answer against
    // the channel it asked for, so a backlog left by an earlier bulk read is not
    // merely noise - each stale frame consumes one of the four confirmation
    // attempts, and a full memory read leaves enough of them to make every
    // channel fail. Observed in the field: a tone read 8 s after a memory read
    // skipped all 19 channels, the same read 10 s later found 18.
    port.drain();
    // A scanning radio steps channels by itself, so a walk that recalls them
    // would read tones from wherever the scan happened to be. Same treatment as
    // the write: pause it, do the round, put it back as it was.
    let scan_was = pause_scan(port);
    let mut ret = ret;
    // Prefer what the radio says over the polled snapshot.
    if ret.vfo_select == 1 {
        if let Some(ch) = current_memory_channel(port, five_digit) {
            ret.memory_channel = ch;
        }
    }
    if ret.vfo_select != 1 && ret.vfo_a_freq == 0 {
        let resp = port.query("FA;");
        if let Some(p) = resp.find("FA") {
            if let Ok(hz) = resp[p + 2..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse::<u64>()
            {
                ret.vfo_a_freq = hz;
            }
        }
        if ret.vfo_a_freq == 0 {
            warn!("tone read skipped: in VFO mode with an unknown VFO-A frequency");
            if let Some(mode) = scan_was {
                port.send(&format!("SC{};", mode));
            }
            return out;
        }
    }
    for (ch, is_dcs) in entries {
        if is_tx() {
            warn!("tone read aborted at channel {}: radio went into TX", ch);
            break;
        }
        // Recall is a set: the radio sends nothing back, so waiting for a reply
        // only burns the read timeout. Instead confirm the switch by reading the
        // channel back - correct AND faster than a fixed settle delay, because
        // the tone must be read from the channel we asked for, not the previous one.
        port.send(&recall(*ch));
        let want = if five_digit { format!("MC0{:05}", ch) } else { format!("MC{:03}", ch) };
        let mut on_channel = false;
        for _ in 0..4 {
            if port.query(mc_read(five_digit)).contains(&want) {
                on_channel = true;
                break;
            }
        }
        if !on_channel {
            warn!("tone read: radio did not confirm channel {}, skipping it", ch);
            continue;
        }
        // CN P2: 0 = CTCSS tone number, 1 = DCS code number.
        let q = if *is_dcs { "CN01;" } else { "CN00;" };
        let resp = port.query(q);
        let key = if *is_dcs { "CN01" } else { "CN00" };
        let n = resp
            .find(key)
            .map(|p| resp[p + 4..].chars().take_while(|c| c.is_ascii_digit()).collect::<String>())
            .and_then(|d| d.parse::<u8>().ok());
        if let Some(n) = n {
            let label = if *is_dcs { dcs_code_label(n) } else { ctcss_freq_label(n) };
            if !label.is_empty() {
                out.push((*ch, label.to_string(), *is_dcs));
            }
        }
    }
    if ret.vfo_select == 1 && ret.memory_channel >= 1 {
        let _ = port.query(&recall(ret.memory_channel));
    } else if ret.vfo_a_freq > 0 {
        let _ = port.query(&format!("FA{:09};", ret.vfo_a_freq));
    }
    if let Some(mode) = scan_was {
        port.send(&format!("SC{};", mode));
        info!("Tone read done: scan resumed (SC{})", mode);
    }
    info!("Tone read: {} channel(s) answered", out.len());
    out
}

/// Fill the CTCSS/DCS columns of a memory blob with tones read from the radio.
/// Columns are looked up by name, like every other reader of this format.
pub(super) fn merge_tones_into_blob(blob: &str, tones: &[(u16, String, bool)]) -> String {
    let mut lines = blob.lines();
    let Some(header) = lines.next() else { return blob.to_string() };
    let cols: Vec<&str> = header.split('\t').collect();
    let idx = |name: &str| cols.iter().position(|c| c.trim().eq_ignore_ascii_case(name));
    let (Some(col_ch), Some(col_ctcss), Some(col_dcs)) =
        (idx("Channel Number"), idx("CTCSS"), idx("DCS"))
    else {
        return blob.to_string();
    };
    let mut out = String::with_capacity(blob.len() + 64);
    out.push_str(header);
    out.push('\n');
    for line in lines {
        let mut f: Vec<String> = line.split('\t').map(|s| s.to_string()).collect();
        if let Some(ch) = f.get(col_ch).and_then(|c| c.trim().parse::<u16>().ok()) {
            if let Some((_, label, is_dcs)) = tones.iter().find(|(c, _, _)| *c == ch) {
                let target = if *is_dcs { col_dcs } else { col_ctcss };
                if let Some(slot) = f.get_mut(target) {
                    *slot = label.clone();
                }
            }
        }
        out.push_str(&f.join("\t"));
        out.push('\n');
    }
    out
}

/// Channels in a memory blob that carry a tone mode, as `(channel, is_dcs)`.
pub(super) fn tone_channels(blob: &str) -> Vec<(u16, bool)> {
    let mut lines = blob.lines();
    let Some(header) = lines.next() else { return Vec::new() };
    let cols: Vec<&str> = header.split('\t').collect();
    let idx = |name: &str| cols.iter().position(|c| c.trim().eq_ignore_ascii_case(name));
    let (Some(col_ch), Some(col_tone)) = (idx("Channel Number"), idx("Tone Mode")) else {
        return Vec::new();
    };
    lines
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            let ch = f.get(col_ch)?.trim().parse::<u16>().ok()?;
            let mode = f.get(col_tone)?.trim();
            match mode {
                "" | "None" => None,
                m if m.starts_with("DCS") || m == "D Code" => Some((ch, true)),
                _ => Some((ch, false)),
            }
        })
        .collect()
}

pub(super) fn write_all_memories<P: CatPort + ?Sized>(
    port: &mut P,
    tab_text: &str,
    ret: MemoryWriteReturn,
    is_tx: &dyn Fn() -> bool,
) -> Result<usize, String> {
    let mut count = 0;
    // (channel, its MT command, tone number) for every channel that carries a
    // tone - written after the main pass, see `write_memory_tones`.
    let mut tone_writes: Vec<(u16, String, u8, bool)> = Vec::new();
    // A scanning radio ignores memory writes without saying so - the operator
    // just sees nothing happen. Pause the scan for the duration and put it back.
    let scan_was = pause_scan(port);

    let mut lines = tab_text.lines();
    let header = lines.next().ok_or("Empty tab text")?;

    let cols: Vec<&str> = header.split('\t').collect();
    let find_col = |name: &str| cols.iter().position(|c| c.trim().eq_ignore_ascii_case(name));
    let col_ch = find_col("Channel Number");
    let col_rx = find_col("Receive Frequency");
    let col_mode = find_col("Operating Mode");
    let col_tone = find_col("Tone Mode");
    let col_ctcss = find_col("CTCSS");
    let col_dcs = find_col("DCS");
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
        // Mode codes must round-trip correctly with the read parser
        // above (line ~1003-1007): '4'->FM, 'B'->FM-N, '5'->AM,
        // 'D'->AM-N, 'A'->DATA-FM, 'E'->C4FM, etc. Earlier code mapped
        // AM-N->'5' (= AM) and C4FM->'A' (= DATA-FM), which broke the read-after-write
        // integrity.
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

        // The value that belongs to this channel's tone mode: a CTCSS tone number
        // or a DCS code number. Neither fits in MT (P9 is fixed), so it is
        // collected for the CN pass below - CN carries both, selected by P2.
        let is_dcs = matches!(tone, '3' | '4');
        let tone_num: Option<u8> = if is_dcs {
            dcs_num_from_label(get(col_dcs))
        } else {
            ctcss_num_from_label(get(col_ctcss))
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
        // P9 is fixed "00" per FT-991A CAT spec, so MT carries only the
        // tone-MODE flag (P8). The tone frequency goes out separately via CN.
        let mt_cmd = format!("MT{:03}{:09}+000000{}0{}00{}0{};",
            ch, freq_hz, mode_char, tone, shift, tag);

        log::debug!("MT write {:03}: [{}] ({}B)", ch, mt_cmd, mt_cmd.len());

        let response = port.query(&mt_cmd);
        if response.contains("?;") {
            warn!("MT{:03} rejected", ch);
        } else {
            count += 1;
            // Only channels with an active tone mode need one.
            if tone != '0' {
                if let Some(n) = tone_num {
                    tone_writes.push((ch, mt_cmd.clone(), n, is_dcs));
                }
            }
        }

        std::thread::sleep(Duration::from_millis(50));
    }

    if !tone_writes.is_empty() {
        info!("Writing CTCSS tone for {} channel(s) via CN", tone_writes.len());
        let verified = write_memory_tones(port, &tone_writes, ret, false, is_tx);
        info!("CTCSS tone stored on {}/{} channel(s)", verified, tone_writes.len());
    }
    if let Some(mode) = scan_was {
        port.send(&format!("SC{};", mode));
        info!("Memory write done: scan resumed (SC{})", mode);
    }

    Ok(count)
}

/// Extract the tag name from an FTX-1 `MT` response.
/// Response: `MT` + P0(5:channel) + P1(up to 12 ASCII tag) + `;`.
pub(super) fn parse_ftx1_tag(mt: &str) -> String {
    if let (Some(s), Some(e)) = (mt.find("MT"), mt.find(';')) {
        if e > s + 7 {
            return mt[s + 7..e].trim().to_string();
        }
    }
    String::new()
}

/// FTX-1 mode code (P6 in MR/MW) -> label. Codes differ from the 991A
/// (3=CW-U, 7=CW-L, E=PSK, H/I=C4FM). Labels chosen so they round-trip
/// correctly with `ftx1_mode_to_code` and are recognizable in the client editor.
pub(super) fn ftx1_mode_label(c: char) -> &'static str {
    match c {
        '1' => "LSB", '2' => "USB", '3' => "CW", '4' => "FM", '5' => "AM",
        '6' => "RTTY-LSB", '7' => "CW-R", '8' => "DATA-LSB", '9' => "RTTY-USB",
        'A' | 'a' => "DATA-FM", 'B' | 'b' => "FM-N", 'C' | 'c' => "DATA-USB",
        'D' | 'd' => "AM-N", 'E' | 'e' => "PSK", 'F' | 'f' => "DATA-FM",
        'H' | 'h' | 'I' | 'i' => "C4FM",
        _ => "FM",
    }
}

/// Inverse of [`ftx1_mode_label`]: label -> FTX-1 mode code (P6).
pub(super) fn ftx1_mode_to_code(label: &str) -> char {
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
/// The FTX-1 splits what the FT-991A puts in one `MT` query across two commands
/// (FTX-1 CAT OM, MR + MT) and uses **5-digit** channel numbers:
///   `MR{ch:05};` -> freq/mode/clarifier/shift/ctcss (NO name), 27 data chars:
///       P1(5:ch) P2(9:freq) P3(5:clar) P4(1:rxclar) P5(1:txclar)
///       P6(1:mode) P7(1:vfo/mem) P8(1:ctcss) P9(2:fixed00) P10(1:shift)
///   `MT{ch:05};` -> the 12-char tag (name) of that channel.
/// (The 991A uses 3-digit channels + a combined MT query, which is why
/// `MT001;` returned `?;` on the FTX-1.)
pub(super) fn read_all_memories_ftx1(
    port: &mut Box<dyn serialport::SerialPort>,
    current_mem_ch: Option<u16>,
) -> Result<String, String> {
    let mut channels = Vec::new();

    // Same as the 991A: one CN query for the channel the radio is on, no
    // channel switching. The FTX-1 CAT OM lists CN (CTCSS NUMBER) as readable.
    let current_ctcss = read_current_ctcss(port, current_mem_ch);

    // The FTX-1 CAT sometimes ignores the ENTIRE first MR-scan after connect/idle
    // (every query times out / errors -> empty list); a repeat scan then succeeds.
    // Retry the whole scan up to 3x so the first read (and the auto-read on connect)
    // already returns the list instead of needing a manual 2nd/3rd click.
    for scan_attempt in 0..3u8 {
    channels.clear();
    // Channels that answered NOTHING - neither a record nor the "?;" that marks a
    // genuinely empty slot. Those two are indistinguishable in the final count, and
    // that is how a near-empty list passed for a real one: the retry below only
    // fired on ZERO channels, so "1 of 99" was accepted and pushed to the client.
    // A partial answer from a radio that is still waking up is as wrong as none.
    let mut unanswered = 0u16;
    // Warm-up: the FTX-1 swallows the FIRST MR query/queries after a pause (connect or
    // the inter-attempt sleep) -> channel 1 was missed because of that ("22 instead of 23"). Prime the
    // port with a few discarded queries so the real scan (below) does pick up channel
    // 1. On a truly cold radio they stay empty (attempt fails -> retry).
    let _ = cat_query(port, "MR00001;");
    let _ = cat_query(port, "MR00001;");
    for ch in 1..=99u16 {
        // Echo-validated read: accept a response ONLY if it echoes THIS channel
        // ("MR{ch:05}..."). A late / in-flight response for another channel is then
        // rejected and re-queried (cat_query flushes input first), so the list can
        // no longer shift or renumber - the exact symptom of "starts at 20/30,
        // correct order but wrong numbers". An empty channel answers "?;" and is
        // skipped without retrying.
        // 800ms timeout (instead of the 300ms default): the FTX-1 answers the first
        // time after connect/idle slowly (cold). With 300ms the query timed out, and the
        // buffer flush of the retry threw away the just-arriving slow response
        // -> the ENTIRE first read came back empty (only the 2nd click worked). 800ms catches
        // the cold response within one query.
        let mr_expect = format!("MR{:05}", ch);
        let mut mr = String::new();
        for _t in 0..4 {
            // ch<=3: diagnostic query so the log shows WHY an empty read happened
            // (write error / read error kind / timeout).
            let r = if ch <= 3 {
                cat_query_diag(port, &format!("MR{:05};", ch), Duration::from_millis(800))
            } else {
                cat_query_with_timeout(port, &format!("MR{:05};", ch), Duration::from_millis(800))
            };
            if let Some(p) = r.find(&mr_expect) { mr = r[p..].to_string(); break; } // aligned
            if r.contains("?;") { break; } // empty channel - definitive, no retry
            // else: timeout or a response echoing another channel -> retry
            if _t == 3 { unanswered += 1; }
        }

        // Raw-response probe (first 3 channels) so the hardware
        // confirms the manual format - just like the PC/IF bring-up.
        if ch <= 3 {
            // Raw CAT frames carry the operator's own memory names and
            // frequencies. Diagnostic detail, not ordinary logging.
            log::debug!("MR{:05} RAW probe: [{}] ({}B)", ch, mr.escape_debug(), mr.len());
        }

        if mr.is_empty() {
            continue;
        }
        // `mr` now starts with "MR{ch:05}"; take the payload up to the terminator.
        let end = match mr.find(';') { Some(e) => e, None => continue };
        let d = &mr[2..end]; // skip "MR"; d[0..5] = echoed channel (== ch)
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

        // Name via a separate MT query, also echo-validated on this channel.
        let mt_expect = format!("MT{:05}", ch);
        let mut mt = String::new();
        for _t in 0..3 {
            let r = if ch <= 3 {
                cat_query_diag(port, &format!("MT{:05};", ch), Duration::from_millis(800))
            } else {
                cat_query_with_timeout(port, &format!("MT{:05};", ch), Duration::from_millis(800))
            };
            if let Some(p) = r.find(&mt_expect) { mt = r[p..].to_string(); break; }
            if r.contains("?;") { break; }
        }
        if ch <= 3 {
            log::debug!("MT{:05} RAW probe: [{}] ({}B)", ch, mt.escape_debug(), mt.len());
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
        // P9 is fixed "00" in MR -> the CTCSS tone frequency is not in here.
        // Only the currently recalled channel has one, read via CN above.
        let ctcss_freq = match current_ctcss {
            Some((c, label)) if c == ch => label,
            _ => "",
        };
        let narrow = narrow_from_mode(mode_char);

        let freq_str = format!("{:.5}", freq_hz as f64 / 1_000_000.0).replace('.', ",");
        let tx_freq_str = format!("{:.5}", tx_freq_hz as f64 / 1_000_000.0).replace('.', ",");
        let display_name = if name.is_empty() { format!("CH {:02}", ch) } else { name.clone() };

        // Unknown columns stay empty - see the 991A branch for the reasoning.
        channels.push(format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t\t{}\t\t\t\t\t\t\t\t\t",
            ch, freq_str, tx_freq_str, offset_freq_str, offset_dir, mode, mode,
            display_name, tone_mode, ctcss_freq, narrow
        ));
        log::debug!("MR{:05}: {} {} {} {} {}", ch, display_name, freq_str, mode, tone_mode, offset_dir);
    }
    if !channels.is_empty() && unanswered == 0 { break; }
    if !channels.is_empty() && scan_attempt + 1 < 3 {
        log::info!(
            "FTX-1: memory scan attempt {}/3 got {} channel(s) but {} did not answer - retrying after settle",
            scan_attempt + 1, channels.len(), unanswered
        );
    }
    if scan_attempt + 1 < 3 {
        // The FTX-1 needs a moment after connect before it answers MR at all, so a
        // first empty pass is expected and the retry is the designed path. Only the
        // last attempt failing is worth a warning.
        if scan_attempt + 1 >= 3 {
            warn!("FTX-1: memory scan returned 0 channels after 3 attempts");
        } else {
            info!("FTX-1: memory scan attempt {}/3 returned 0 channels - retrying after settle", scan_attempt + 1);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    }

    let mut out = String::new();
    out.push_str(sdr_remote_core::YAESU_MEMORY_TAB_HEADER);
    out.push('\n');
    for line in &channels {
        out.push_str(line);
        out.push('\n');
    }
    info!("FTX-1: read {} non-empty memory channels out of 99", channels.len());
    Ok(out)
}

/// Write memory channels to the Yaesu FTX-1.
///
/// Freq/mode/shift/ctcss via `MW` (same 27-byte field layout as the
/// MR response), the name via a separate `MT` tag write. Both 5-digit:
///   `MW{ch:05}{freq:09}{clar:5}{rxclar}{txclar}{mode}{p7=1}{ctcss}00{shift};`
///   `MT{ch:05}{tag:<12};`
pub(super) fn write_all_memories_ftx1<P: CatPort + ?Sized>(
    port: &mut P,
    tab_text: &str,
    ret: MemoryWriteReturn,
    is_tx: &dyn Fn() -> bool,
) -> Result<usize, String> {
    let mut count = 0;
    // Same as the 991A: the tone is not in MW/MR (P9 fixed), so it goes out
    // per channel via CN afterwards - stored with MW here instead of MT.
    let mut tone_writes: Vec<(u16, String, u8, bool)> = Vec::new();
    // A scanning radio ignores memory writes without saying so - the operator
    // just sees nothing happen. Pause the scan for the duration and put it back.
    let scan_was = pause_scan(port);

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
        log::debug!("MW write {:05}: [{}] ({}B)", ch, mw_cmd, mw_cmd.len());
        let mw_resp = port.query(&mw_cmd);
        if mw_resp.contains("?;") {
            warn!("MW{:05} rejected", ch);
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        std::thread::sleep(Duration::from_millis(50));

        // Write the name (tag) separately via MT (up to 12 chars, padded with spaces).
        let name = get(col_name);
        let tag: String = if name.len() >= 12 {
            name[..12].to_string()
        } else {
            format!("{:<12}", name)
        };
        let mt_cmd = format!("MT{:05}{};", ch, tag);
        log::debug!("MT write {:05}: [{}] ({}B)", ch, mt_cmd, mt_cmd.len());
        let mt_resp = port.query(&mt_cmd);
        if mt_resp.contains("?;") {
            warn!("MT{:05} (tag) rejected", ch);
        }
        count += 1;
        // Channels with an active tone mode get their tone via CN below; the
        // channel is re-stored with MW (the FTX-1's memory-write command).
        if tone != '0' {
            if let Some(n) = ctcss_num_from_label(get(col_ctcss)) {
                // FTX-1: collected for symmetry only - the tone pass is disabled
                // for this radio (see below), so nothing is written from it.
                tone_writes.push((ch, mw_cmd.clone(), n, matches!(tone, '3' | '4')));
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // FTX-1 tone writing is DISABLED - no known CAT route stores a per-memory
    // tone on this radio, and every attempt so far damaged the channel:
    //
    //   MW after CN  -> writes the channel back without the tone (no tone field)
    //   nothing      -> the radio discards it as memory-tune at the next recall
    //   AM after CN  -> "MAIN-SIDE TO MEMORY CHANNEL" is VFO->memory, so it
    //                   overwrote the channel with VFO-A (hardware, 2026-08-05:
    //                   memory 16 went from 430.125 MHz to 14.280 MHz)
    //
    // Reading the tone of the current channel still works and stays on. Writing
    // stays off until a route is found that is verified not to touch the rest of
    // the channel - a memory bank is not the place to keep guessing.
    if !tone_writes.is_empty() {
        warn!(
            "FTX-1: {} channel(s) carry a tone, but writing tones is not supported on this radio              - frequency/mode/name were written, the tone was left untouched",
            tone_writes.len()
        );
    }
    if let Some(mode) = scan_was {
        port.send(&format!("SC{};", mode));
        info!("Memory write done: scan resumed (SC{})", mode);
    }
    let _ = (&ret, is_tx); // used by the 991A path only

    Ok(count)
}

/// Read all 153 EX menu settings from the FT-991A.
/// Returns newline-separated "nnn:value" pairs.
pub(super) fn read_all_menus(port: &mut Box<dyn serialport::SerialPort>) -> Result<String, String> {
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
/// The FTX-1 EX is hierarchical: `EX{P1:02}{P2:02}{P3:02};` -> response
/// `EX{P1}{P2}{P3}{value};`. There is no flat index like the 991A. We
/// *scan* the valid addresses live on the radio (ground truth): an invalid
/// address gives `?;` and is skipped. The client matches the addresses against the
/// menu chart (Table 3) for labels - so a wrong label can never write a wrong
/// address. Output: \"p1p2p3:value\" lines (6-digit address).
///
/// Bounds: P1 1..=11, P2 1..=9, P3 1..=40. Per (P1,P2) we stop early if
/// P3=01 and 02 are missing (subgroup does not exist), and after 6 consecutive misses
/// within an existing subgroup (end of items, tolerates gaps up to 5).
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

pub(super) fn read_all_menus_ftx1(port: &mut Box<dyn serialport::SerialPort>) -> Result<String, String> {
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

#[cfg(test)]
mod mc_read_form {
    use super::mc_read;

    /// The FTX-1 read form carries the side digit. Without it the radio does not
    /// answer at all, and a tone walk skips every channel while looking healthy:
    /// "read tones" simply returns 0 channels, which is what the field reported.
    /// Verified against FTX-1 CAT OM 2508-C p.19 (Read = `M C P1 ;`).
    #[test]
    fn the_ftx1_read_form_carries_the_side_digit() {
        assert_eq!(mc_read(true), "MC0;");
        assert_eq!(mc_read(false), "MC;");
    }
}


#[cfg(test)]
mod tone_write_tests {
    use super::*;

    /// Scripted stand-in for the radio: records every command and answers from a
    /// canned list, so a command sequence can be asserted without hardware.
    struct FakePort {
        sent: Vec<String>,
        answers: std::collections::HashMap<String, String>,
        /// The channel a recall put us on. A real radio answers `MC;` with where
        /// it actually is, and the walk relies on that to confirm the switch, so
        /// the double has to behave the same or it can never confirm anything.
        current_channel: Option<String>,
    }

    impl FakePort {
        fn new(answers: &[(&str, &str)]) -> Self {
            Self {
                sent: Vec::new(),
                answers: answers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
                current_channel: None,
            }
        }
        /// Commands actually put on the wire, in order.
        fn sent(&self) -> Vec<&str> {
            self.sent.iter().map(|s| s.as_str()).collect()
        }
    }

    impl FakePort {
        /// Mirror a recall the way a radio would: remember where we now are.
        fn note(&mut self, cmd: &str) {
            let body = cmd.trim_end_matches(';');
            if let Some(rest) = body.strip_prefix("MC") {
                if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                    self.current_channel = Some(format!("MC{};", rest));
                }
            }
        }
    }

    impl CatPort for FakePort {
        fn query(&mut self, cmd: &str) -> String {
            self.sent.push(cmd.to_string());
            self.note(cmd);
            if cmd == "MC;" {
                if let Some(cur) = &self.current_channel {
                    return cur.clone();
                }
            }
            self.answers.get(cmd).cloned().unwrap_or_default()
        }
        // The default would route through query(); recording it explicitly keeps
        // the pinned sequences readable whichever call the code uses.
        fn send(&mut self, cmd: &str) {
            self.sent.push(cmd.to_string());
            self.note(cmd);
        }
    }

    fn ret_memory(ch: u16) -> MemoryWriteReturn {
        MemoryWriteReturn { vfo_select: 1, memory_channel: ch, vfo_a_freq: 0 }
    }

    #[test]
    fn ftx1_tone_writing_is_not_attempted() {
        // No CAT route on this radio stores a per-memory tone without damaging
        // the channel - AM turned out to be VFO->memory and overwrote it. The
        // FTX-1 write path therefore collects no tone entries at all; this test
        // is the guard against quietly re-enabling it.
        let src = include_str!("memory.rs");
        let ftx1_fn = src
            .split("pub(super) fn write_all_memories_ftx1")
            .nth(1)
            .expect("FTX-1 write path present");
        let body = &ftx1_fn[..ftx1_fn.find("
pub(super) fn").unwrap_or(ftx1_fn.len())];
        assert!(
            !body.contains("write_memory_tones("),
            "FTX-1 must not call the tone pass - see the comment there for what each attempt did"
        );
    }

    #[test]
    fn ft991a_recalls_with_three_digits_and_stores_with_mt() {
        let mt = "MT007014550000+00000040100CH 7        ;".to_string();
        let mut port = FakePort::new(&[("CN00;", "CN00004;")]);
        let n = write_memory_tones(&mut port, &[(7, mt.clone(), 4, false)], ret_memory(3), false, &|| false);
        assert_eq!(n, 1);
        // "MC;" first: ask the radio where it is, so the return does not rely on
        // a snapshot that may lag behind what was just recalled.
        assert_eq!(port.sent(), vec!["MC;", "MC007;", "CN00004;", mt.as_str(), "CN00;", "MC003;"]);
    }

    #[test]
    fn a_running_scan_is_paused_and_resumed_around_a_tone_read() {
        // The walk recalls channels; a scanning radio steps them by itself, so
        // the tones would come from wherever the scan happened to be.
        let mut port = FakePort::new(&[
            ("SC;", "SC1;"),          // scanning upward
            ("MC;", "MC003;"),
            ("MC007;", ""),
            ("CN00;", "CN00004;"),
        ]);
        let ret = MemoryWriteReturn { vfo_select: 1, memory_channel: 3, vfo_a_freq: 0 };
        let out = read_memory_tones(&mut port, &[(7, false)], ret, false, &|| false);
        let sent = port.sent();
        assert_eq!(sent.first(), Some(&"SC;"), "ask whether it is scanning");
        assert!(sent.contains(&"SC0;"), "stop the scan before stepping channels");
        assert_eq!(sent.last(), Some(&"SC1;"), "and resume it in the mode it was in");
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn the_channel_to_return_to_comes_from_the_radio() {
        // The polled status can lag behind what the operator just recalled, and
        // returning to a stale channel leaves the radio somewhere else after a
        // write - observed on air: selected 11, ended on 9.
        let mut port = FakePort::new(&[("MC;", "MC011;"), ("CN00;", "CN00004;")]);
        let stale = MemoryWriteReturn { vfo_select: 1, memory_channel: 9, vfo_a_freq: 0 };
        write_memory_tones(&mut port, &[(7, "MT007;".into(), 4, false)], stale, false, &|| false);
        assert_eq!(
            port.sent().last(),
            Some(&"MC011;"),
            "return to the channel the radio reports, not the one in the snapshot"
        );
    }

    #[test]
    fn dcs_uses_the_other_cn_table() {
        // CN carries both tables, selected by P2: 0 = CTCSS tone number,
        // 1 = DCS code number (CAT manual, Table 1 and Table 2). A DCS channel
        // must therefore be written and read back with CN01, not CN00 - getting
        // that wrong would set a tone frequency where a code was meant.
        let mt = "MT007;".to_string();
        let mut port = FakePort::new(&[("CN01;", "CN01012;")]);
        let n = write_memory_tones(&mut port, &[(7, mt.clone(), 12, true)], ret_memory(3), false, &|| false);
        assert_eq!(n, 1, "DCS code should read back as stored");
        assert!(port.sent().contains(&"CN01012;"), "DCS is written with P2=1");
        assert!(port.sent().contains(&"CN01;"), "and read back from the same table");
        assert!(!port.sent().iter().any(|c| c.starts_with("CN00")), "never the CTCSS table");
    }

    #[test]
    fn mismatched_read_back_is_not_counted_as_stored() {
        // Radio answers a different tone than requested -> not verified.
        let mut port = FakePort::new(&[("CN00;", "CN00012;")]);
        let n = write_memory_tones(&mut port, &[(7, "MT007;".into(), 4, false)], ret_memory(3), false, &|| false);
        assert_eq!(n, 0, "a tone the radio did not take must not count as stored");
    }

    #[test]
    fn transmitting_aborts_before_any_channel_is_recalled() {
        // Recalling a channel moves the transmit frequency, so a round must not
        // start - let alone continue - while the radio is keyed.
        let mut port = FakePort::new(&[]);
        let n = write_memory_tones(&mut port, &[(7, "MT007;".into(), 4, false)], ret_memory(3), false, &|| true);
        assert_eq!(n, 0);
        assert_eq!(
            port.sent(),
            vec!["MC;", "MC003;"],
            "asks where it is, then only restores - no channel is recalled"
        );
    }

    #[test]
    fn vfo_return_without_a_known_frequency_skips_the_round() {
        // Returning to VFO is an FA write; without a frequency the radio would
        // be left parked on the last recalled memory channel.
        let ret = MemoryWriteReturn { vfo_select: 0, memory_channel: 0, vfo_a_freq: 0 };
        let mut port = FakePort::new(&[]); // FA; answers nothing
        let n = write_memory_tones(&mut port, &[(7, "MT007;".into(), 4, false)], ret, false, &|| false);
        assert_eq!(n, 0);
        assert_eq!(port.sent(), vec!["FA;"], "no channel may be recalled if we cannot come back");
    }

    #[test]
    fn vfo_frequency_is_read_before_the_first_recall() {
        let ret = MemoryWriteReturn { vfo_select: 0, memory_channel: 0, vfo_a_freq: 0 };
        let mut port = FakePort::new(&[("FA;", "FA014250000;"), ("CN00;", "CN00004;")]);
        let n = write_memory_tones(&mut port, &[(7, "MT007;".into(), 4, false)], ret, false, &|| false);
        assert_eq!(n, 1);
        assert_eq!(port.sent().first(), Some(&"FA;"), "read the VFO before stepping channels");
        assert_eq!(port.sent().last(), Some(&"FA014250000;"), "and return to it afterwards");
    }
}
