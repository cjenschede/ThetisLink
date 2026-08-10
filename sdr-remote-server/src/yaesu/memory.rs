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
    // Then start the scan from an EMPTY input buffer. Measured 2026-08-08: the CN
    // answer above can arrive after its own read has returned, and the first MT
    // query then consumes it ("channel 1 answered without an MT frame: [CN00012;]").
    // From there every channel receives the previous channel's answer and the last
    // one is never collected - a list one channel short, with the data silently
    // shifted. Intermittent, because it depends on whether the CN reply beats the
    // first MT query; a manual re-read always looked fine because the buffer was
    // clean by then. Same fault the tone walk had, same remedy.
    port.drain();

    // A live radio answers EVERY channel quickly (programmed or empty -
    // the full 117-channel read takes <1s). A radio in standby/off does not
    // answer MT: every query times out (~300ms). Without abort the read grinds
    // on ~35s AND blocks the other CAT commands on the
    // single-threaded poll loop that whole time (e.g. a just-pressed power-ON). So abort
    // after a few CONSECUTIVE timeouts (slow empty responses); a single
    // slow-but-valid response resets the counter, and empty leading channels (which
    // answer quickly) do not trigger this.
    let mut consec_timeouts = 0u16;
    // The echo check keeps reporting a channel number that is NOT the one asked
    // (constant 014 with the radio parked on memory 14), while the count and the
    // empty/occupied split are correct. Three explanations fitted the warnings and
    // two were already refuted by measurement, so dump the raw bytes at INFO - the
    // probe below sat at DEBUG and this server logs at INFO, which is why it has
    // never recorded a single frame.
    for ch in 1..=117u16 {
        let t0 = Instant::now();
        let response = cat_query(port, &format!("MT{:03};", ch));
        let timed_out = response.trim().is_empty() && t0.elapsed() >= Duration::from_millis(250);
        if timed_out {
            consec_timeouts += 1;
            if consec_timeouts >= 4 {
                return Err("radio not responding (powered off?)".to_string());
            }
            // Observation only, no retry (an attempt to add one in build 116 rejected
            // valid answers and cut the list to two channels - the shape of the reply
            // had been assumed rather than measured). This names the channel that
            // stayed silent, and how long it waited, so the next short read says WHICH
            // one went missing instead of only that the total was one too low.
            warn!("Yaesu: channel {} gave no answer after {} ms - skipped (list will be short)",
                  ch, t0.elapsed().as_millis());
            continue;
        }
        consec_timeouts = 0;

        if response.trim().is_empty() {
            warn!("Yaesu: channel {} answered nothing (in {} ms, no timeout) - skipped",
                  ch, t0.elapsed().as_millis());
            continue;
        }
        if response.contains("?;") {
            continue; // genuinely empty channel - definitive, not a miss
        }

        let Some(start) = response.find("MT") else {
            // Answered, but not with an MT frame. Measured, not repaired: this is
            // one of three ways a channel could vanish without a trace, which is
            // why a short list showed no warning at all.
            warn!("Yaesu: channel {} answered without an MT frame: [{}] - skipped",
                  ch, response.escape_debug());
            continue;
        };
        {
            let Some(end) = response[start..].find(';') else {
                warn!("Yaesu: channel {} answer has no terminator: [{}] - skipped",
                      ch, response.escape_debug());
                continue;
            };
            {
                let d = &response[start + 2..start + end]; // skip "MT"


                // MT response: P1(3)+P2(9)+P3(5)+P4(1)+P5(1)+P6(1)+P7(1)+P8(1)+P9(2)+P10(1)+P11(1)+P12(12) = 38
                if d.len() < 26 {
                    warn!("Yaesu: channel {} answer too short ({} chars, need 26): [{}] - skipped",
                          ch, d.len(), response.escape_debug());
                    continue;
                }

                // P1 (d[0..3]) is DELIBERATELY not read. Per the CAT reference it
                // holds the requested channel - "MT + P1(3) + P2(9) + ..." with the
                // example reply MT001145500000+00000040000REPEATER1   ; - but this
                // radio does not do that. Measured 2026-08-08 over three scans with
                // the radio parked on memory 14:
                //
                //   MT001 -> MT014144525000+0000004100000lokaal 1    ;
                //   MT002 -> MT014144700000+0000004100000lokaal 2    ;
                //   MT003 -> MT014145500000+0000004100000aanroep     ;
                //
                // P1 is the CURRENT memory channel, the same for every query, while
                // the payload belongs to the channel asked (different frequencies,
                // correct tags, the tag at the spec's positions 29-40). With memory
                // scan running on the radio, P1 followed the scan instead.
                //
                // So the answer cannot be matched to the question on this radio, and
                // an echo check here can only raise false alarms. This is why build
                // 116 - which copied the FTX-1's echo validation, where MR does echo
                // correctly - cut the list to one or two channels: valid answers were
                // rejected until consec_timeouts aborted the read. Do not add one back
                // without measuring P1 again first.
                let freq_hz: u64 = d[3..12].parse().unwrap_or(0); // P2: 9-digit freq
                if freq_hz == 0 {
                    warn!("Yaesu: channel {} has an unreadable frequency [{}] - skipped",
                          ch, &d[3..12]);
                    continue;
                }

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
/// Put the FTX-1's MAIN side on VFO for the duration of a memory write, and report
/// what it was so it can be put back.
///
/// The radio refuses `MW` for the channel it is CURRENTLY sitting on while it is in
/// memory mode - observed on air: writing 23 channels wrote 22, and the one refused
/// was the channel in use. Nothing says so; the radio answers "?;" and that single
/// channel silently keeps its old contents. The operator's manual route says the same
/// thing in front-panel terms: leave the channel first, then store.
///
/// FTX-1 CAT OM (2508-C), `VM VFO / MEMORY CHANNEL`:
///   Set  VM P1 P2 P2 ;   P1 0=MAIN 1=SUB, P2 00=VFO 10=MT 11=Memory 20=PMS
///   Read VM P1 ;         answer VM P1 P2 P2 ;
fn ftx1_leave_memory_mode<P: CatPort + ?Sized>(port: &mut P) -> Option<String> {
    let resp = port.query("VM0;");
    let i = resp.find("VM")?;
    let rest = &resp[i + 2..];
    // answer: VM + P1(1) + P2(2) + ';'
    if rest.len() < 4 { return None; }
    let was: String = rest[1..3].to_string();
    if was == "00" {
        return None; // already on VFO - nothing to restore
    }
    info!("Memory write: MAIN side is on {} (not VFO), switching to VFO for the write", was);
    port.send("VM000;");
    std::thread::sleep(Duration::from_millis(80));
    Some(was)
}

/// Counterpart of `ftx1_leave_memory_mode`.
fn ftx1_restore_memory_mode<P: CatPort + ?Sized>(port: &mut P, was: Option<String>) {
    if let Some(w) = was {
        port.send(&format!("VM0{};", w));
        info!("Memory write done: MAIN side put back on {}", w);
    }
}

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
/// Translate a memory record's tone mode (the `MW` P8 code) into the `CT` code.
///
/// They are NOT the same numbering, and the two that differ are the two that matter:
///
///   MW P8:  1 = CTCSS ENC/DEC      2 = CTCSS ENC
///   CT P2:  1 = ENC on / DEC off   2 = ENC on / DEC on
///
/// So 1 and 2 are swapped between them. Passing the value straight through would set
/// encode where encode+decode was asked for, and the other way round - a repeater that
/// opens but a squelch that never closes, or the reverse. Nothing would report it.
/// (FTX-1 CAT OM 2508-C: MW P8 on the MW page, CT P2 under "CT SQL TYPE".)
/// `ftx1_tone_mode_to_ct` for callers outside this module.
pub(super) fn ftx1_tone_mode_to_ct_pub(mw_p8: char) -> char { ftx1_tone_mode_to_ct(mw_p8) }

fn ftx1_tone_mode_to_ct(mw_p8: char) -> char {
    match mw_p8 {
        '1' => '2', // ENC/DEC
        '2' => '1', // ENC only
        '3' => '3', // DCS
        '4' => '4', // PR FREQ
        '5' => '5', // REV TONE
        _ => '0',   // off
    }
}

/// Read a channel and its two neighbours, for the log.
///
/// This is the measurement that decides between two defects that look identical in
/// the log we have: does `AM;` write to the WRONG channel, or does it not write at
/// all? The pass only ever checks the channel it meant to write, so both come out as
/// "the channel is unchanged". If a neighbour moved instead, the target pointer is
/// wrong; if nothing moved anywhere, the store is a no-op in this state. Those need
/// opposite fixes, and guessing between them has cost five attempts already.
fn ftx1_dump_neighbourhood<P: CatPort + ?Sized>(port: &mut P, ch: u16, when: &str) {
    for probe in [ch.saturating_sub(1), ch, ch + 1] {
        if probe == 0 { continue; }
        port.drain();
        let r = port.query(&format!("MR{:05};", probe));
        info!("PROBE {} ch {:>3}: [{}]", when, probe, r.trim().escape_debug());
    }
    port.drain();
    let mc = port.query("MC0;");
    let fa = port.query("FA;");
    let vm = port.query("VM0;");
    let cn = port.query("CN00;");
    info!("PROBE {} state: MC0=[{}] FA=[{}] VM0=[{}] CN00=[{}]",
          when, mc.trim().escape_debug(), fa.trim().escape_debug(),
          vm.trim().escape_debug(), cn.trim().escape_debug());
}

/// Store the CTCSS/DCS tone of an FTX-1 memory channel.
///
/// This mirrors, command for command, the front-panel sequence the operator worked out
/// on the radio itself (2026-08-09, reproducible):
///
///   in the channel -> M>V -> set the tone -> MW -> select the memory -> MW
///   -> the name is now empty -> type the name
///
/// In CAT:
///
///   MC0nnnnn;  be on the channel
///   MA;        MEMORY CHANNEL to MAIN-side   (M>V)
///   CN0 P2 nnn;  the tone (Table 1: 004 = 77.0 Hz)
///   MC0nnnnn;  select the target memory
///   AM;        MAIN-SIDE TO MEMORY CHANNEL   (MW)
///   MT0nnnnn<tag>;  the name, which the store clears
///
/// Two things were wrong in the attempt before this one, and both came from not
/// following that sequence. Rebuilding VFO-A with `FA` and `MD` only carries what is
/// sent - shift, direction and everything else silently reverted to whatever VFO-A
/// happened to hold. `MA` copies the WHOLE channel, so only the tone changes. And the
/// store empties the name, which nothing put back.
///
/// `AM` is also why an earlier attempt was abandoned: on 2026-08-05 it overwrote
/// memory 16 with the contents of VFO-A (430.125 MHz became 14.280 MHz). The command
/// was never the problem - VFO-A held something else. `MA` is exactly the preparation
/// that was missing.
///
/// Every channel is read back and the pass STOPS on the first mismatch rather than
/// working through the bank. That failure has cost a channel once; it must cost at
/// most one.
///
/// The caller has already stepped off memory mode and paused any scan.
/// Not called while the tone write is off; kept because the probe mode is the way
/// back in the moment a route is found.
#[allow(dead_code)]
fn ftx1_write_tones_via_vfo<P: CatPort + ?Sized>(
    port: &mut P,
    rows: &[(u16, u64, u8, bool, String, char, char)], // (channel, freq_hz, tone number, is_dcs, tag cmd, MW P8, mode)
    is_tx: &dyn Fn() -> bool,
) -> usize {
    let mut stored = 0usize;
    // Measuring mode. Set THETISLINK_FTX1_TONE_PROBE=<channel> before starting the
    // server: the pass then touches ONLY that channel, dumps it and its neighbours
    // before and after, and waits two seconds before reading back instead of 150 ms.
    // Nothing else in the bank is written, which is the point - the failure being
    // measured is "something else got written".
    let probe_only: Option<u16> = std::env::var("THETISLINK_FTX1_TONE_PROBE")
        .ok()
        .and_then(|v| v.trim().parse::<u16>().ok());
    if let Some(only) = probe_only {
        info!("FTX-1 tone PROBE mode: channel {} only, the rest of the bank is left alone", only);
    }
    for (ch, freq_hz, tone_num, is_dcs, mt_cmd, tone_mode, mode_char) in rows {
        if let Some(only) = probe_only {
            if *ch != only { continue; }
            ftx1_dump_neighbourhood(port, *ch, "before");
        }
        if is_tx() {
            warn!("FTX-1 tone write aborted at channel {}: radio went into TX", ch);
            break;
        }
        let select = format!("MC0{:05};", ch);
        // Be on the channel, and WAIT until the radio says it is there.
        //
        // Without this the pass wrote the previous channel's contents into the next
        // one: the recall had not landed yet when MA copied, so the whole walk shifted
        // by one. On air that turned into memories 5 and 6 holding each other's
        // frequency - a memory bank quietly rearranged, which is the worst kind of
        // failure here because nothing looks wrong until you use the channel.
        // The tone READ walk has always confirmed the recall this way; the write pass
        // did not.
        port.send(&select);
        std::thread::sleep(Duration::from_millis(80));
        let mut on_channel = false;
        for _ in 0..4 {
            if current_memory_channel(port, true) == Some(*ch) {
                on_channel = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        if !on_channel {
            warn!("FTX-1 channel {}: the radio did not report being on it - skipped, \
                   nothing written", ch);
            continue;
        }
        // Copy the WHOLE channel to MAIN - not a rebuilt version of it.
        port.send("MA;");
        std::thread::sleep(Duration::from_millis(80));
        // MA brings across what the RADIO holds, which is the right starting point for
        // everything this pass does not manage - shift, direction, filters. But where
        // the operator's list differs from the radio, the list is the intent: without
        // putting it back on top, MA followed by AM writes the radio's own old content
        // back and quietly undoes the MW pass that just ran. Observed: channel 6 came
        // back at 145.600 while the list said 145.625.
        port.send(&format!("FA{:09};", freq_hz));
        std::thread::sleep(Duration::from_millis(40));
        port.send(&format!("MD0{};", mode_char));
        std::thread::sleep(Duration::from_millis(40));
        // Then the tone: first WHICH kind of tone (CT), then WHICH tone (CN).
        // MA carried the channel's own tone mode across, but the operator may have
        // just changed it in the table, and CT is what makes that take.
        // Read the answers instead of firing and hoping. Both were sent with `send`,
        // so a "?;" refusal was invisible: we could see that the tone did not change
        // but not whether the radio had REFUSED the command or accepted and ignored
        // it. Those need opposite fixes - a wrong command form versus a radio that
        // will not take a tone in this state - and that is the last thing separating
        // them.
        let ct_cmd = format!("CT0{};", ftx1_tone_mode_to_ct(*tone_mode));
        port.drain();
        let ct_resp = port.query(&ct_cmd);
        info!("PROBE CT: sent [{}] -> [{}]", ct_cmd, ct_resp.trim().escape_debug());
        std::thread::sleep(Duration::from_millis(40));

        let p2 = if *is_dcs { 1 } else { 0 };
        let cn_cmd = format!("CN0{}{:03};", p2, tone_num);
        port.drain();
        let cn_resp = port.query(&cn_cmd);
        info!("PROBE CN: sent [{}] -> [{}]", cn_cmd, cn_resp.trim().escape_debug());
        std::thread::sleep(Duration::from_millis(60));
        // And straight back: did it take at all, before anything is stored?
        port.drain();
        let cn_now = port.query(if *is_dcs { "CN01;" } else { "CN00;" });
        info!("PROBE CN readback right after setting it: [{}]", cn_now.trim().escape_debug());
        // Last check before the irreversible step: MAIN must actually hold the
        // frequency we are about to store. Everything up to here is a set command
        // without a reply, so this is the first moment the radio can contradict us -
        // and storing the wrong contents is what rearranges a memory bank.
        port.drain();
        let main_now = port.query("FA;");
        let main_ok = main_now.find("FA")
            .and_then(|i| main_now.get(i + 2..i + 11))
            .and_then(|f| f.parse::<u64>().ok())
            == Some(*freq_hz);
        if !main_ok {
            warn!("FTX-1 channel {}: MAIN reads [{}] instead of {} just before storing - \
                   skipped, nothing written", ch, main_now.escape_debug(), freq_hz);
            continue;
        }

        // Store MAIN into the channel selected at the top of this loop. NO second MC
        // here: on the front panel the button after the first MW picks a target while
        // MAIN keeps the prepared contents, but over CAT `MC` is a plain RECALL - it
        // puts the channel back on MAIN and AM then stores the channel into itself.
        // Observed: channel 6 kept reading 145.600 where the list said 145.625, no
        // matter what was prepared. The selection made before MA is what AM writes to.
        // `VM;` rather than `AM;`. The manual lists both as "MAIN-SIDE TO MEMORY
        // CHANNEL", and the front-panel key the operator presses to store is the one
        // called MW - whose CAT name is the bare VM. AM has now been measured doing
        // nothing at all here: channel, neighbours and stored tone all unchanged, with
        // the radio left in memory mode. So try the other of the two.
        port.send("VM;");
        // A store that commits slowly would read back as "nothing happened", so in
        // measuring mode give it far more room than it should ever need.
        std::thread::sleep(Duration::from_millis(if probe_only.is_some() { 2000 } else { 150 }));
        if probe_only.is_some() {
            // The tone was measured present right after CN and absent at the end, so
            // one of our own two remaining steps undoes it. Read between them.
            port.drain();
            let after_store = port.query(if *is_dcs { "CN01;" } else { "CN00;" });
            info!("PROBE CN after the store command: [{}]", after_store.trim().escape_debug());
        }
        // The store clears the name; put it back.
        port.drain();
        let tag_resp = port.query(mt_cmd);
        if probe_only.is_some() {
            port.drain();
            let after_tag = port.query(if *is_dcs { "CN01;" } else { "CN00;" });
            info!("PROBE CN after the tag write [{}]: [{}]",
                  mt_cmd.escape_debug(), after_tag.trim().escape_debug());
            ftx1_dump_neighbourhood(port, *ch, "after AM");

            // The store wipes a tone set before it, so try the other way round: set it
            // AFTER, while sitting on the channel, and then leave and come back. That
            // last part is the whole question - an earlier note in this file says this
            // radio treats a CN change as memory-tune, which would mean it survives
            // until the next channel change and no further.
            let other = if *ch > 1 { *ch - 1 } else { *ch + 1 };
            port.send(&format!("MC0{:05};", ch));
            std::thread::sleep(Duration::from_millis(120));
            port.drain();
            let r1 = port.query(&format!("CT0{};", ftx1_tone_mode_to_ct(*tone_mode)));
            std::thread::sleep(Duration::from_millis(40));
            let r2 = port.query(&format!("CN0{}{:03};", p2, tone_num));
            std::thread::sleep(Duration::from_millis(120));
            port.drain();
            let on_channel = port.query(if *is_dcs { "CN01;" } else { "CN00;" });
            info!("PROBE tone set ON the channel: CT->[{}] CN->[{}] reads [{}]",
                  r1.trim().escape_debug(), r2.trim().escape_debug(),
                  on_channel.trim().escape_debug());

            port.send(&format!("MC0{:05};", other));
            std::thread::sleep(Duration::from_millis(200));
            port.send(&format!("MC0{:05};", ch));
            std::thread::sleep(Duration::from_millis(200));
            port.drain();
            let after_trip = port.query(if *is_dcs { "CN01;" } else { "CN00;" });
            info!("PROBE tone after leaving to ch {} and back: [{}]",
                  other, after_trip.trim().escape_debug());
        }
        if tag_resp.contains("?;") {
            warn!("FTX-1 channel {}: the name was refused after storing - sent [{}]",
                  ch, mt_cmd.escape_debug());
        }

        // Read the channel back. The frequency is the guard: if it moved, MAIN did not
        // hold what we thought and the channel has just been overwritten.
        port.drain();
        // MR takes a bare 5-digit channel - no MAIN/SUB digit, unlike MC. Sending
        // MR0nnnnn is one character too long and the radio answers "?;", which reads
        // as "cannot read the channel" and stopped the pass on its first channel.
        // The frequency sits at offset 7..16 of the answer, as the bulk reader has it.
        let want = format!("MR{:05}", ch);
        let back = port.query(&format!("MR{:05};", ch));
        let got = back.find(&want)
            .and_then(|i| back.get(i + 7..i + 16))
            .and_then(|f| f.parse::<u64>().ok());
        match got {
            Some(f) if f == *freq_hz => {
                let label = if *is_dcs { dcs_code_label(*tone_num) } else { ctcss_freq_label(*tone_num) };
                // The frequency proves the channel survived; the tone proves the write
                // did what it was for. Reading it back is the whole point - a tone that
                // did not take looks identical to one that did until you key up.
                port.drain();
                let key = if *is_dcs { "CN01" } else { "CN00" };
                let tone_back = port.query(if *is_dcs { "CN01;" } else { "CN00;" });
                let got_tone = tone_back.find(key)
                    .and_then(|i| tone_back[i + 4..].get(..3))
                    .and_then(|v| v.parse::<u8>().ok());
                match got_tone {
                    Some(v) if v == *tone_num => {
                        info!("FTX-1 channel {}: tone {} ({}) stored and read back", ch, tone_num, label);
                        stored += 1;
                    }
                    Some(v) => {
                        warn!("FTX-1 channel {}: asked for tone {} ({}) but the radio reads {} - \
                               stopping the tone pass", ch, tone_num, label, v);
                        break;
                    }
                    None => {
                        warn!("FTX-1 channel {}: could not read the tone back [{}] - stopping the tone pass",
                              ch, tone_back.escape_debug());
                        break;
                    }
                }
            }
            Some(f) => {
                warn!("FTX-1 channel {}: after storing, the frequency reads {} instead of {} - \
                       stopping the tone pass before it touches another channel", ch, f, freq_hz);
                break;
            }
            None => {
                warn!("FTX-1 channel {}: could not read the channel back [{}] - stopping the tone pass",
                      ch, back.escape_debug());
                break;
            }
        }
    }
    stored
}

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
            warn!(
                concat!(
                    "tone write skipped: radio is in VFO mode but its VFO-A frequency is ",
                    "unknown, so it could not be returned there",
                ),
            );
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
/// Merge tones read from the radio into the list.
///
/// `fill_only` is for the FTX-1, and it is not a preference - it follows from what
/// that radio can do. It cannot STORE a tone in a memory channel over CAT, and `MW`
/// resets the tone of every channel it writes to 100.0 Hz. So after any memory write
/// the radio reports 100.0 Hz for those channels, and merging that back over the list
/// replaced the operator's real tones with the damage - on every connect, because the
/// tone read runs then. The list is the truth for this radio; a read can fill a gap
/// (a tone set on the set's own front panel, which does work) but must never overwrite
/// a value that is already there.
///
/// The FT-991A stores tones properly, so for it a read IS the truth and overwrites.
pub(super) fn merge_tones_into_blob(
    blob: &str,
    tones: &[(u16, String, bool)],
    fill_only: bool,
    prefix: &str,
) -> String {
    let (out, skipped) = merge_tones_counted(blob, tones, fill_only);
    if skipped > 0 {
        // The sister path already reports what it KEPT; silence about what was
        // ignored is the half an operator would report as a fault, because the
        // radio disagrees with the list and nothing says which one won.
        info!(
            concat!(
                "{} {} tone(s) read from the radio were ignored: the list already had a ",
                "value there, and on this radio the list is the truth (it cannot store ",
                "a tone in a memory channel)",
            ),
            prefix, skipped,
        );
    }
    out
}

/// `merge_tones_into_blob` plus the number of read tones it declined to apply.
fn merge_tones_counted(
    blob: &str,
    tones: &[(u16, String, bool)],
    fill_only: bool,
) -> (String, usize) {
    let mut skipped = 0usize;
    let mut lines = blob.lines();
    let Some(header) = lines.next() else { return (blob.to_string(), 0) };
    let cols: Vec<&str> = header.split('\t').collect();
    let idx = |name: &str| cols.iter().position(|c| c.trim().eq_ignore_ascii_case(name));
    let (Some(col_ch), Some(col_ctcss), Some(col_dcs)) =
        (idx("Channel Number"), idx("CTCSS"), idx("DCS"))
    else {
        return (blob.to_string(), 0);
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
                    if !fill_only || slot.trim().is_empty() {
                        *slot = label.clone();
                    } else {
                        skipped += 1;
                    }
                }
            }
        }
        out.push_str(&f.join("\t"));
        out.push('\n');
    }
    (out, skipped)
}

/// The tone a memory channel should have, from the server's copy of the list:
/// `(tone number, is_dcs, CT code)`. `None` when that channel has no tone mode.
///
/// Used to re-apply the tone every time the FTX-1 lands on a channel - see
/// `ftx1_tone_keeper` in poll.rs for why that is necessary.
pub(super) fn tone_wanted_for_channel(blob: &str, ch: u16) -> Option<(u8, bool, char)> {
    let mut lines = blob.lines();
    let header = lines.next()?;
    let cols: Vec<&str> = header.split('\t').collect();
    let idx = |name: &str| cols.iter().position(|c| c.trim().eq_ignore_ascii_case(name));
    let (col_ch, col_tone, col_ctcss, col_dcs) =
        (idx("Channel Number")?, idx("Tone Mode")?, idx("CTCSS")?, idx("DCS")?);
    for line in lines {
        let f: Vec<&str> = line.split('\t').collect();
        if f.get(col_ch).and_then(|c| c.trim().parse::<u16>().ok()) != Some(ch) {
            continue;
        }
        let mode = match f.get(col_tone).map(|v| v.trim()) {
            Some("None") | None => return None,
            Some("Tone") => '1',
            Some("Tone ENC") => '2',
            Some("DCS") => '3',
            Some("PR FREQ") => '4',
            Some("REV") => '5',
            _ => return None,
        };
        let is_dcs = matches!(mode, '3' | '4');
        let label = if is_dcs { f.get(col_dcs) } else { f.get(col_ctcss) };
        let num = label
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .and_then(|v| if is_dcs { dcs_num_from_label(v) } else { ctcss_num_from_label(v) })?;
        return Some((num, is_dcs, mode));
    }
    None
}

/// The tones already known in a blob, as `(channel, label, is_dcs)` - the exact
/// shape `merge_tones_into_blob` takes, so what came out of one read can be carried
/// into the next.
///
/// The bulk memory read cannot fetch per-channel tones (P9 is fixed "00"); they are
/// gathered separately by stepping the radio through the channels that have a tone
/// mode. So a plain re-read produces a list with the tone columns empty, and without
/// this the tones silently disappeared every time anyone pressed "read radio" - or
/// every time an older client asked for a read on connect.
///
/// A tone is only carried over when the channel still has the SAME frequency. If the
/// channel was reprogrammed, the old tone does not belong to it any more, and a wrong
/// tone is worse than none.
pub(super) fn tones_from_blob(blob: &str) -> Vec<(u16, String, bool)> {
    let mut lines = blob.lines();
    let Some(header) = lines.next() else { return Vec::new() };
    let cols: Vec<&str> = header.split('\t').collect();
    let idx = |name: &str| cols.iter().position(|c| c.trim().eq_ignore_ascii_case(name));
    let (Some(col_ch), Some(col_ctcss), Some(col_dcs)) =
        (idx("Channel Number"), idx("CTCSS"), idx("DCS"))
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in lines {
        let f: Vec<&str> = line.split('\t').collect();
        let Some(ch) = f.get(col_ch).and_then(|c| c.trim().parse::<u16>().ok()) else { continue };
        for (col, is_dcs) in [(col_ctcss, false), (col_dcs, true)] {
            if let Some(v) = f.get(col) {
                let v = v.trim();
                if !v.is_empty() {
                    out.push((ch, v.to_string(), is_dcs));
                }
            }
        }
    }
    out
}

/// Frequency per channel, used to decide whether a remembered tone still applies.
pub(super) fn freqs_from_blob(blob: &str) -> Vec<(u16, String)> {
    let mut lines = blob.lines();
    let Some(header) = lines.next() else { return Vec::new() };
    let cols: Vec<&str> = header.split('\t').collect();
    let idx = |name: &str| cols.iter().position(|c| c.trim().eq_ignore_ascii_case(name));
    // Column name exactly as YAESU_MEMORY_TAB_HEADER spells it. A wrong name here
    // finds nothing and the carry-over silently does nothing at all.
    let (Some(col_ch), Some(col_rx)) = (idx("Channel Number"), idx("Receive Frequency")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in lines {
        let f: Vec<&str> = line.split('\t').collect();
        if let (Some(ch), Some(rx)) = (
            f.get(col_ch).and_then(|c| c.trim().parse::<u16>().ok()),
            f.get(col_rx),
        ) {
            out.push((ch, rx.trim().to_string()));
        }
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
    let mut tone_rows: Vec<(u16, u64, u8, bool, String, char, char)> = Vec::new();
    // A scanning radio ignores memory writes without saying so - the operator
    // just sees nothing happen. Pause the scan for the duration and put it back.
    let scan_was = pause_scan(port);
    // The channel the radio is sitting on cannot be written while it is in memory
    // mode, so step off it first. Without this exactly one channel out of the whole
    // bank silently kept its old contents - the one in use.
    let mode_was = ftx1_leave_memory_mode(port);

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
            // The command itself, so a refusal can be read instead of guessed at.
            // The radio says only "?;" - which field it objected to is in what we
            // sent, not in the answer.
            warn!("MW{:05} rejected - sent [{}]", ch, mw_cmd);
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
            warn!("MT{:05} (tag) rejected - sent [{}]", ch, mt_cmd.escape_debug());
        }
        count += 1;
        // Channels with an active tone mode get their tone via CN below; the
        // channel is re-stored with MW (the FTX-1's memory-write command).
        if tone != '0' {
            if let Some(n) = ctcss_num_from_label(get(col_ctcss)) {
                // FTX-1: collected for symmetry only - the tone pass is disabled
                // for this radio (see below), so nothing is written from it.
                tone_writes.push((ch, mw_cmd.clone(), n, matches!(tone, '3' | '4')));
                // The tone pass reprograms the channel from VFO-A, so it needs the
                // channel's own frequency and mode - MW cannot carry the tone.
                tone_rows.push((ch, freq_hz, n, matches!(tone, '3' | '4'), mt_cmd.clone(), tone, mode_char));
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
    // Tone writing is OFF for this radio. Measured 2026-08-09 with the probe mode
    // (THETISLINK_FTX1_TONE_PROBE), on channel 6, both store commands the manual
    // offers:
    //
    //   AM;  channel, both neighbours and the stored tone unchanged; radio left in
    //        memory mode (VM011)
    //   VM;  the same, radio left on VFO (VM000)
    //
    // Setting the tone DOES work - CN00048; is accepted and reads back. Both ways of
    // keeping it fail, each measured three times:
    //
    //   tone before the store -> the store wipes it (back to what the VFO held)
    //   tone after the store  -> accepted, but gone at the first channel change
    //
    // The second is what an older note in this file suspected: this radio treats a CN
    // change as memory-tune. The manual's own store commands carry no tone field
    // (MW has "P9 00: Fixed") and do not pick up the current one, so there is no route
    // left in the documentation. See docs/internal/OPEN-ftx1-memory-tone-write.md.
    //
    // Frequency, mode and name ARE written (the MW pass above). Only the tone is left
    // alone, and it is said out loud rather than silently skipped.
    if !tone_rows.is_empty() {
        // Off by default; the probe mode is the only way it runs, so a measurement
        // costs one env var and touches one channel.
        if std::env::var("THETISLINK_FTX1_TONE_PROBE").is_ok() {
            info!("FTX-1: PROBE run - the tone pass is off by default, measuring only");
            let _ = ftx1_write_tones_via_vfo(port, &tone_rows, is_tx);
        } else {
            warn!(
                concat!(
                    "FTX-1: {} channel(s) carry a tone, but this radio cannot STORE a tone in a ",
                    "memory channel over CAT - frequency, mode and name were written, and MW has ",
                    "reset the tone of those channels to 100.0 Hz in the radio itself. The list ",
                    "keeps the real tones and the server re-applies them on every channel change, ",
                    "so transmitting through ThetisLink is unaffected",
                ),
                  tone_rows.len());
        }
    }
    // Put the radio back where the operator had it: memory mode first, then the
    // scan - resuming a scan while still on VFO would start it in the wrong place.
    ftx1_restore_memory_mode(port, mode_was);
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

/// The radio's TX time-out timer, in minutes, out of the EX-menu blob this server
/// already reads once per connect. 0 = off (and 0 is also what an unreadable or
/// missing entry gives, so the watchdog stays quiet rather than guessing a limit).
///
/// The two models number their menus differently: FT-991A `036`, FTX-1 `030112`.
pub(super) fn tot_minutes_from_menu_blob(model: RadioModel, blob: &str) -> u8 {
    let key = if matches!(model, RadioModel::Ftx1) { "030112" } else { "036" };
    for line in blob.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        if k.trim() != key {
            continue;
        }
        // Both models answer 00-30 (minutes), 00 = OFF.
        return v.trim().parse::<u8>().ok().filter(|m| *m <= 30).unwrap_or(0);
    }
    0
}

#[cfg(test)]
mod tot_tests {
    use super::*;

    #[test]
    fn the_991a_reads_its_own_menu_number() {
        let blob = "035:0\n036:05\n037:1";
        assert_eq!(tot_minutes_from_menu_blob(RadioModel::Ft991a, blob), 5);
    }

    #[test]
    fn the_ftx1_reads_its_own_menu_number() {
        let blob = "030111:1\n030112:10\n030113:0";
        assert_eq!(tot_minutes_from_menu_blob(RadioModel::Ftx1, blob), 10);
    }

    /// The models must not read each other's menus - 036 means something else
    /// entirely on an FTX-1.
    #[test]
    fn a_model_ignores_the_other_ones_key() {
        let blob = "036:05";
        assert_eq!(tot_minutes_from_menu_blob(RadioModel::Ftx1, blob), 0);
    }

    #[test]
    fn off_and_missing_both_mean_no_limit() {
        assert_eq!(tot_minutes_from_menu_blob(RadioModel::Ft991a, "036:00"), 0);
        assert_eq!(tot_minutes_from_menu_blob(RadioModel::Ft991a, "037:5"), 0);
        assert_eq!(tot_minutes_from_menu_blob(RadioModel::Ft991a, ""), 0);
    }

    /// A value out of range is a misread, not a 99-minute limit.
    #[test]
    fn an_impossible_value_is_not_trusted() {
        assert_eq!(tot_minutes_from_menu_blob(RadioModel::Ft991a, "036:99"), 0);
    }
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

    /// The FTX-1 tone write is OFF, and this has now flipped twice - so the reasons
    /// are here rather than in a commit message.
    ///
    /// It was off for months on the conclusion that no CAT route could store a tone
    /// without damaging the channel: `AM` had overwritten memory 16 with the contents
    /// of VFO-A (hardware, 2026-08-05).
    ///
    /// On 2026-08-09 that conclusion was overturned by the operator, who found a
    /// reproducible front-panel sequence, and the write was enabled through it. Six
    /// sequences later it was measured with the probe mode
    /// (`THETISLINK_FTX1_TONE_PROBE`) rather than reasoned about:
    ///
    ///   `AM;`  channel, both neighbours and the stored tone unchanged; radio left in
    ///          memory mode (VM011)
    ///   `VM;`  the same, radio left on VFO (VM000)
    ///
    /// And the measurement that settles it: after `CN00048;` the radio still answered
    /// `CN00=004`. The tone had not changed in the CURRENT state, let alone in a
    /// channel. Setting the tone does not take on this radio, so no store route can
    /// help - which is why the search through store commands was the wrong search.
    ///
    /// Do not re-enable on reasoning. The probe mode is the way back in: it touches
    /// one channel, dumps both neighbours, and answers in one run what six sequences
    /// could not. See docs/internal/OPEN-ftx1-memory-tone-write.md.
    #[test]
    fn ftx1_tone_writing_is_off_and_says_so() {
        let src = include_str!("memory.rs");
        let ftx1_fn = src
            .split("pub(super) fn write_all_memories_ftx1")
            .nth(1)
            .expect("FTX-1 write path present");
        let body = &ftx1_fn[..ftx1_fn.find("
pub(super) fn").unwrap_or(ftx1_fn.len())];
        // The pass may only run behind the probe env var: a measurement costs one
        // variable and touches one channel, an ordinary write must never reach it.
        let call = body.find("ftx1_write_tones_via_vfo(port");
        if let Some(at) = call {
            let before = &body[..at];
            let guard = before.rfind("THETISLINK_FTX1_TONE_PROBE");
            assert!(
                guard.is_some() && guard.unwrap() > before.len().saturating_sub(400),
                "the tone pass may only run inside the probe guard"
            );
        }
        assert!(
            !body.contains("write_memory_tones("),
            "the 991A tone pass must not be used here either"
        );
        // Silently skipping is how this went unnoticed for months.
        assert!(
            body.contains("cannot STORE a tone"),
            "a skipped tone must be reported, not quietly dropped"
        );
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

#[cfg(test)]
mod tone_carry_tests {
    use super::*;

    /// The header these helpers read is the shared constant; a renamed column must
    /// fail the test rather than silently carry nothing over.
    fn blob(rows: &[&str]) -> String {
        let mut s = String::from(sdr_remote_core::YAESU_MEMORY_TAB_HEADER);
        s.push('\n');
        for r in rows { s.push_str(r); s.push('\n'); }
        s
    }

    /// 21 columns: Ch, RxFreq, TxFreq, Offset, Dir, Mode, TxMode, Name, ToneMode,
    /// CTCSS, DCS, then the rest empty.
    fn row(ch: u16, rx: &str, ctcss: &str) -> String {
        let mut f = vec![ch.to_string(), rx.to_string(), String::new(), String::new(),
                         String::new(), "FM".to_string(), String::new(), "REPEATER".to_string(),
                         "TONE".to_string(), ctcss.to_string(), String::new()];
        while f.len() < 21 { f.push(String::new()); }
        f.join("\t")
    }

    #[test]
    fn a_known_tone_is_read_back_out_of_a_blob() {
        let b = blob(&[&row(1, "145500000", "88.5 Hz"), &row(2, "145600000", "")]);
        let tones = tones_from_blob(&b);
        assert_eq!(tones.len(), 1);
        assert_eq!(tones[0].0, 1);
        assert_eq!(tones[0].1, "88.5 Hz");
        assert!(!tones[0].2); // CTCSS, not DCS
    }

    /// The whole point: a fresh read has empty tone columns, and the tones we already
    /// knew have to survive it.
    fn header_columns_exist() {
        assert!(!freqs_from_blob(&blob(&[&row(1, "145500000", "")])).is_empty(),
                "column names must match YAESU_MEMORY_TAB_HEADER");
    }

    #[test]
    fn the_column_names_match_the_shared_header() {
        header_columns_exist();
    }

    #[test]
    fn a_tone_survives_a_re_read_of_the_same_channel() {
        let old = blob(&[&row(1, "145500000", "88.5 Hz")]);
        let fresh = blob(&[&row(1, "145500000", "")]);
        let keep = tones_from_blob(&old);
        let merged = merge_tones_into_blob(&fresh, &keep, false, "[test]");
        assert!(merged.contains("88.5 Hz"), "tone should have been carried over");
    }

    /// A channel that was reprogrammed keeps no old tone - a wrong tone is worse
    /// than none.
    #[test]
    fn a_reprogrammed_channel_loses_its_old_tone() {
        let old = blob(&[&row(1, "145500000", "88.5 Hz")]);
        let fresh = blob(&[&row(1, "433000000", "")]);
        let of = freqs_from_blob(&old);
        let nf = freqs_from_blob(&fresh);
        let keep: Vec<_> = tones_from_blob(&old).into_iter()
            .filter(|(ch, _, _)| {
                let o = of.iter().find(|(c, _)| c == ch).map(|(_, f)| f);
                let n = nf.iter().find(|(c, _)| c == ch).map(|(_, f)| f);
                o.is_some() && o == n
            })
            .collect();
        assert!(keep.is_empty(), "a changed frequency must drop the remembered tone");
    }
}

#[cfg(test)]
mod ftx1_tone_mode_tests {
    use super::*;

    /// The two codings differ in exactly the two values that matter, and nothing
    /// reports it when they are confused: the repeater still opens, but the squelch
    /// behaves the other way round.
    ///
    ///   MW P8:  1 = CTCSS ENC/DEC      2 = CTCSS ENC
    ///   CT P2:  1 = ENC on / DEC off   2 = ENC on / DEC on
    #[test]
    fn the_two_tone_mode_codings_are_not_the_same() {
        assert_eq!(ftx1_tone_mode_to_ct('1'), '2', "MW 'ENC/DEC' is CT 2");
        assert_eq!(ftx1_tone_mode_to_ct('2'), '1', "MW 'ENC only' is CT 1");
    }

    /// The rest map straight through, including "no tone".
    #[test]
    fn the_other_tone_modes_map_straight_through() {
        assert_eq!(ftx1_tone_mode_to_ct('0'), '0');
        assert_eq!(ftx1_tone_mode_to_ct('3'), '3'); // DCS
        assert_eq!(ftx1_tone_mode_to_ct('4'), '4'); // PR FREQ
        assert_eq!(ftx1_tone_mode_to_ct('5'), '5'); // REV TONE
        assert_eq!(ftx1_tone_mode_to_ct('x'), '0', "anything unknown is safest as off");
    }
}

#[cfg(test)]
mod tone_keeper_tests {
    use super::*;

    fn blob(rows: &[&str]) -> String {
        let mut s = String::from(sdr_remote_core::YAESU_MEMORY_TAB_HEADER);
        s.push('\n');
        for r in rows { s.push_str(r); s.push('\n'); }
        s
    }

    /// 21 columns: Ch, RxFreq, TxFreq, Offset, Dir, Mode, TxMode, Name, ToneMode,
    /// CTCSS, DCS, rest empty.
    fn row(ch: u16, tone_mode: &str, ctcss: &str, dcs: &str) -> String {
        let mut f = vec![ch.to_string(), "145600000".into(), String::new(), String::new(),
                         String::new(), "FM".into(), String::new(), "REP".into(),
                         tone_mode.to_string(), ctcss.to_string(), dcs.to_string()];
        while f.len() < 21 { f.push(String::new()); }
        f.join("\t")
    }

    #[test]
    fn a_channel_with_a_ctcss_tone_is_found() {
        let b = blob(&[&row(6, "Tone", "77.0 Hz", "")]);
        assert_eq!(tone_wanted_for_channel(&b, 6), Some((4, false, '1')));
    }

    #[test]
    fn a_channel_without_a_tone_mode_asks_for_nothing() {
        let b = blob(&[&row(6, "None", "77.0 Hz", "")]);
        assert_eq!(tone_wanted_for_channel(&b, 6), None,
                   "a tone value with the mode off must not be applied");
    }

    /// A tone mode with no value is not something to send - CN would need a number
    /// this row does not have.
    #[test]
    fn a_tone_mode_without_a_value_asks_for_nothing() {
        let b = blob(&[&row(6, "Tone", "", "")]);
        assert_eq!(tone_wanted_for_channel(&b, 6), None);
    }

    #[test]
    fn dcs_uses_the_dcs_column_and_its_own_table() {
        let b = blob(&[&row(6, "DCS", "77.0 Hz", "023")]);
        let got = tone_wanted_for_channel(&b, 6).expect("a DCS row is found");
        assert!(got.1, "must be flagged as DCS so CN01 is used");
        assert_eq!(got.2, '3');
    }

    #[test]
    fn an_unknown_channel_asks_for_nothing() {
        let b = blob(&[&row(6, "Tone", "77.0 Hz", "")]);
        assert_eq!(tone_wanted_for_channel(&b, 7), None);
    }
}

#[cfg(test)]
mod tone_merge_tests {
    use super::*;

    fn blob(ctcss: &str) -> String {
        let mut cols = vec![""; 21];
        cols[0] = "6";
        cols[9] = ctcss;
        format!("{}\n{}\n", sdr_remote_core::YAESU_MEMORY_TAB_HEADER, cols.join("\t"))
    }

    fn ctcss_of(b: &str) -> String {
        b.lines().nth(1).unwrap().split('\t').nth(9).unwrap().to_string()
    }

    /// The FT-991A stores tones, so what it reports is the truth.
    #[test]
    fn a_991a_read_overwrites() {
        let m = merge_tones_into_blob(&blob("77.0 Hz"), &[(6, "100.0 Hz".into(), false)], false, "[test]");
        assert_eq!(ctcss_of(&m), "100.0 Hz");
    }

    /// The FTX-1 cannot store one, and reports 100.0 Hz for a channel that has none -
    /// which is exactly what MW leaves behind. Merging that back wiped the operator's
    /// real tones from the list on every connect.
    #[test]
    fn an_ftx1_read_never_overwrites_a_tone_the_list_already_has() {
        let m = merge_tones_into_blob(&blob("77.0 Hz"), &[(6, "100.0 Hz".into(), false)], true, "[test]");
        assert_eq!(ctcss_of(&m), "77.0 Hz", "the list is the truth for this radio");
    }

    /// A tone set on the set's own front panel does work, so a read still fills a gap.
    #[test]
    fn an_ftx1_read_still_fills_an_empty_one() {
        let m = merge_tones_into_blob(&blob(""), &[(6, "88.5 Hz".into(), false)], true, "[test]");
        assert_eq!(ctcss_of(&m), "88.5 Hz");
    }
}
