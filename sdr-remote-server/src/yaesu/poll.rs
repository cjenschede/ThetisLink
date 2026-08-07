// SPDX-License-Identifier: GPL-2.0-or-later
//! Yaesu connection runtime: the self-reconnecting supervisor thread and the
//! inner serial poll loop (send periodic CAT queries, drain responses via
//! parse_responses, apply pending commands + audio streams). Extracted verbatim
//! from `yaesu/mod.rs` - pure relocation, no behaviour/CAT/timing change.
//! `use super::*;` reaches every shared item it drives (YaesuRadio/State/Cmd,
//! the audio stream builders, cat/parse/ex_menu helpers, mode conversions);
//! `pub(super)` keeps the reconnect thread spawnable from impl YaesuRadio. The
//! inner poll loop stays private (only the reconnect thread calls it).

use super::*;

/// Self-reconnecting thread: runs the serial poll loop, reconnects on failure.
pub(super) fn yaesu_reconnect_thread(
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

    // Connection-state tracking, local to this thread:
    //   `ever_connected` flips to true on the first successful open and
    //   determines whether an open failure is cold-start (silent) or mid-runtime
    //   disconnect (one warn).
    //   `disconnect_logged` deduplicates the disconnect warn so we don't
    //   generate log spam every 3 s during a long outage.
    //   `first` triggers the wait/drain block only after the first iteration
    //   (so the very first open attempt does not wait 3 s).
    let mut first = true;
    let mut ever_connected = false;
    let mut disconnect_logged = false;

    loop {
        if !first {
            // Drop old audio streams (only meaningful after a successful connect -
            // during cold-start retries there is nothing to drop).
            if ever_connected {
                capture_stream.set(None);
                output_stream.set(None);
                *tx_producer.lock().unwrap() = None;
            }

            std::thread::sleep(Duration::from_secs(3));

            // Drain stale commands
            // Count what is thrown away: a queue that had run up is exactly the
            // condition that made a radio keep stepping minutes after the knob
            // stopped, and it used to leave no trace at all.
            let mut stale = 0usize;
            while cmd_rx.try_recv().is_ok() { stale += 1; }
            if stale > 0 {
                warn!("{} dropped {} stale command(s) on reconnect", prefix, stale);
            }

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
                // Pre-connect (cold-start, Yaesu not yet on): silent retry,
                // no log spam per 3 s tick. One `debug!` for anyone debugging
                // with RUST_LOG=debug.
                // Post-connect (mid-runtime outage): one `warn!` on the
                // first failed-open after the disconnect, then silent until
                // reconnect or a new outage cycle.
                if ever_connected && !disconnect_logged {
                    warn!("{} disconnected, retrying in background", prefix);
                    disconnect_logged = true;
                }
                log::debug!("{} open attempt failed: {}", prefix, e);
                continue;
            }
        };

        // Open succeeded - log the transition and reset the dedup flag for the
        // next possible outage cycle. The connect line contains COM+baud so
        // operator-checklist item (a) is directly greppable.
        if ever_connected {
            info!("{} serial reconnected on {} @ {} baud", prefix, port_name, baud);
        } else {
            info!("{} serial connected on {} @ {} baud", prefix, port_name, baud);
            ever_connected = true;
        }
        disconnect_logged = false;

        // Bring-up probe: once after each successful open, dump the
        // raw ID;/IF;/MD0;/FA; + one parse summary. Makes it live
        // visible whether the radio parses as 991A structure or where it deviates.
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

        // 991A per-band max TX power from the EX menu (PATCH-yaesu-power-scaling):
        // EX137 HF, EX138 50M, EX139 144M, EX140 430M (watt). Determines the client
        // slider range per band. FTX-1 = phase B (head-max via tx_power_max()).
        if !matches!(model, RadioModel::Ftx1) {
            let rd = |port: &mut Box<dyn serialport::SerialPort>, ex: u16, name: &str| {
                read_ex_menu_value(port, &prefix, ex, name)
                    .and_then(|v| v.trim().parse::<u8>().ok())
                    .unwrap_or(0)
            };
            let hf = rd(&mut port, 137, "HF TX MAX POWER");
            let m50 = rd(&mut port, 138, "50M TX MAX POWER");
            let m144 = rd(&mut port, 139, "144M TX MAX POWER");
            let m430 = rd(&mut port, 140, "430M TX MAX POWER");
            if let Ok(mut s) = status.lock() {
                s.max_pwr_hf = hf; s.max_pwr_50 = m50; s.max_pwr_144 = m144; s.max_pwr_430 = m430;
            }
            info!("{} 991A max-TX-power EX: HF={} 50M={} 144M={} 430M={} (0=not read -> default)",
                prefix, hf, m50, m144, m430);
        }

        // Rebuild audio streams unconditionally after each successful open.
        // On cold-start (Yaesu was off when new() ran) the USB
        // audio device only becomes available here; on mid-runtime reconnect
        // the device may have disappeared briefly.
        //
        // The Yaesu FT-991A presents the capture and output side of its
        // USB Audio CODEC as two separate cpal devices that just barely
        // do not become enumerable simultaneously. In practice capture
        // becomes available ~100-300 ms earlier than output; a back-to-back
        // build of capture first and then output then fails with
        // "device is no longer available" on the output side. That is why
        // the output build below retries a few times with a short
        // delay between attempts.
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
            // Output-stream retry loop: up to 5 attempts, 500 ms between each.
            // Logs only the final status (ok or the last error) -
            // intermediate attempts stay on debug to keep the server log
            // quiet.
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
                    "{} audio output open failed after 5 attempts: {} - keeps retrying in the background every 5s until the device is free",
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
    let mut cat_monitor_on = false;
    // Diagnostic switch, deliberately not in the UI: it turns Auto Information
    // on at the radio and writes every CAT frame to the log, which is useful for
    // one evening of investigation and noise for everything else.
    //   set THETISLINK_CAT_MONITOR=1  before starting the server
    if std::env::var("THETISLINK_CAT_MONITOR").map(|v| v != "0").unwrap_or(false) {
        status.lock().unwrap().cat_monitor = true;
        info!("{} CAT monitor armed via THETISLINK_CAT_MONITOR", prefix);
    }
    let mut raw_buf = [0u8; 256];
    let mut last_full_poll = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    let mut last_smeter_poll = Instant::now();
    let mut last_response = Instant::now();
    let mut last_output_retry = Instant::now();
    // Warn-once guards: prevent 500 ms-poll log spam while they
    // remove the current silent defaults. `warned_modes` = unknown MD codes
    // (one warn per unique char); `warned_short_if` = deviating IF length (one warn).
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
        // Baud hint in the line: the operator sees immediately whether a silent radio
        // may be a baud mismatch between radio-menu and config.
        if last_response.elapsed().as_secs() >= 5 {
            warn!("{} no CAT response for 5s - disconnecting (check baud radio-menu vs config)", prefix);
            return;
        }

        // CAT monitor: mirror every complete frame to the log before parsing,
        // and keep Auto Information in step with the flag. With AI on the radio
        // reports what IT changes - front-panel included - which is how we can
        // see what a control on the set actually sends.
        {
            let want = status.lock().unwrap().cat_monitor;
            if want != cat_monitor_on {
                cat_monitor_on = want;
                let _ = cat_query(&mut port, if want { "AI1;" } else { "AI0;" });
                info!("{} CAT monitor {}", prefix, if want { "ON (AI1)" } else { "off (AI0)" });
            }
            if cat_monitor_on {
                let mut rest = read_buf.as_str();
                while let Some(i) = rest.find(';') {
                    info!("{} CAT-MON < {}", prefix, &rest[..=i]);
                    rest = &rest[i + 1..];
                }
            }
        }

        // Parse complete responses (terminated by ';')
        parse_responses(&mut read_buf, status, prefix, model, &mut warned_modes, &mut warned_short_if);

        // Handle commands from the application
        match cmd_rx.try_recv() {
            Ok(YaesuCmd::ReadAllMemories) => {
                info!("{} reading all memory channels...", prefix);
                // The per-memory CTCSS tone is not in the bulk read (P9 is fixed
                // "00"); only the channel the radio is actually sitting on can be
                // read, via CN. In VFO mode CN describes the VFO, so pass None.
                let current_mem_ch = {
                    let st = status.lock().unwrap();
                    if st.vfo_select == 1 { Some(st.memory_channel) } else { None }
                };
                let mem_result = match model {
                    // FTX-1 splits freq (MR) and name (MT), 5-digit channels.
                    RadioModel::Ftx1 => read_all_memories_ftx1(&mut port, current_mem_ch),
                    _ => read_all_memories(&mut port, current_mem_ch),
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
                        {
                            let mut st = status.lock().unwrap();
                            st.filled_memory_channels = filled;
                            st.last_memory_blob = Some(tab_text.clone());
                        }
                        *memory_data.lock().unwrap() = Some(tab_text);
                    }
                    // On "radio not responding" (standby/off) do NOT clobber: we
                    // return Err -> memory_data stays unchanged, so the client
                    // keeps its loaded list instead of getting an empty list.
                    Err(e) => warn!("{} memory read failed: {}", prefix, e),
                }
                last_response = Instant::now();
                last_full_poll = Instant::now();
                last_smeter_poll = Instant::now();
            }
            Ok(YaesuCmd::ReadMemoryTones) => {
                // Only worth doing for channels that actually have a tone mode,
                // so the radio steps through a handful of repeater channels
                // rather than the whole bank.
                let blob = status.lock().unwrap().last_memory_blob.clone();
                match blob {
                    Some(blob) => {
                        let entries = crate::yaesu::memory::tone_channels(&blob);
                        if entries.is_empty() {
                            info!("{} tone read: no channel has a tone mode", prefix);
                        } else if status.lock().unwrap().tx_active {
                            warn!("{} tone read refused: radio is transmitting", prefix);
                        } else {
                            info!("{} reading tones for {} channel(s)...", prefix, entries.len());
                            let ret = {
                                let st = status.lock().unwrap();
                                crate::yaesu::memory::MemoryWriteReturn {
                                    vfo_select: st.vfo_select,
                                    memory_channel: st.memory_channel,
                                    vfo_a_freq: st.vfo_a_freq,
                                }
                            };
                            let is_tx = || status.lock().map(|s| s.tx_active).unwrap_or(false);
                            let tones = crate::yaesu::memory::read_memory_tones(
                                &mut port, &entries, ret,
                                matches!(model, RadioModel::Ftx1), &is_tx,
                            );
                            if !tones.is_empty() {
                                let merged = crate::yaesu::memory::merge_tones_into_blob(&blob, &tones);
                                status.lock().unwrap().last_memory_blob = Some(merged.clone());
                                // Hand the filled-in list to the client, same route
                                // as a normal memory read.
                                *memory_data.lock().unwrap() = Some(merged);
                                info!("{} tones merged into the memory list", prefix);
                            }
                        }
                    }
                    None => warn!("{} tone read: read the memories first", prefix),
                }
                last_response = Instant::now();
                last_full_poll = Instant::now();
                last_smeter_poll = Instant::now();
            }
            Ok(YaesuCmd::WriteAllMemories(tab_text)) => {
                info!("{} writing memory channels...", prefix);
                // Writing a CTCSS tone means recalling that channel (the tone is
                // a current-channel setting), so capture where to put the radio
                // back before anything moves.
                let ret = {
                    let st = status.lock().unwrap();
                    crate::yaesu::memory::MemoryWriteReturn {
                        vfo_select: st.vfo_select,
                        memory_channel: st.memory_channel,
                        vfo_a_freq: st.vfo_a_freq,
                    }
                };
                // A write recalls channels (for the tone) and rewrites memories,
                // which moves the transmit frequency - refuse outright while the
                // radio is transmitting rather than write half a bank.
                if status.lock().unwrap().tx_active {
                    warn!("{} memory write refused: radio is transmitting", prefix);
                    continue;
                }
                let is_tx = || status.lock().map(|s| s.tx_active).unwrap_or(false);
                let write_result = match model {
                    // FTX-1 writes freq via MW + name via MT (both 5-digit).
                    RadioModel::Ftx1 => write_all_memories_ftx1(&mut port, &tab_text, ret, &is_tx),
                    _ => write_all_memories(&mut port, &tab_text, ret, &is_tx),
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
                    // FTX-1 EX is hierarchical (P1.P2.P3) -> scan-read instead of flat index.
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
                    YaesuCmd::SetFreqA(hz) => {
                        // Memory-mode escape: the 991A/FTX-1 do not accept a direct VFO freq set
                        // in memory mode (vfo_select 1=Memory / 2=MemTune). On a freq change
                        // we copy the channel to VFO-A (MA = MEMORY CHANNEL TO VFO-A; by default
                        // the set thereby leaves memory mode), restore the CURRENT mode (so an
                        // earlier MemTune mode change is not lost - MA copies the STORED
                        // mode), and then set the new freq. This way you slide seamlessly from memory to
                        // VFO. Optimistic vfo_select=0 so fast follow-up steps do not send MA
                        // again (the IF-poll confirms afterwards). MA/MD0 work on both models.
                        let (vfo_sel, cur_mode) = {
                            let s = status.lock().unwrap();
                            (s.vfo_select, s.mode)
                        };
                        if vfo_sel != 0 {
                            {
                                let mut s = status.lock().unwrap();
                                s.vfo_select = 0;
                                // Guard: ignore the next ~15 IF-polls that still
                                // report "Memory" (in-flight/stale) until the radio has
                                // actually performed the MA escape and reports VFO back.
                                s.vfo_escape_pending = 15;
                            }
                            let mc = internal_mode_to_yaesu(cur_mode, model);
                            format!("MA;MD0{};FA{:09};", mc, hz)
                        } else {
                            format!("FA{:09};", hz)
                        }
                    }
                    YaesuCmd::SetFreqB(hz) => format!("FB{:09};", hz),
                    YaesuCmd::SetMode(mode) => format!("MD0{};", internal_mode_to_yaesu(mode, model)),
                    YaesuCmd::SetPtt(on) => {
                        // Auto-DATA PTT-toggle: in the normal modes the Yaesu does not
                        // route USB-mic audio (well) as a TX modulation source - only in the
                        // DATA variants it does. That is why we switch temporarily to the DATA mode for the
                        // TX cycle, then back:
                        //   FM('4')->DATA-FM('A').
                        // PTT-off restores the original mode.
                        // SSB auto-DATA was removed on 2026-07-04 after operator testing showed
                        // that DATA-LSB/USB is unsuitable for voice TX.
                        // Split TX-toggle and mode-change with a short sleep so
                        // the Yaesu TX transition can complete before the mode change arrives.
                        //
                        // Current flow:
                        //   - Single source of truth: this is the ONLY auto-DFM
                        //     emission point (old network.rs wrapper removed ->
                        //     no more race).
                        //   - !in_memory guard - a mode change in Memory mode forces
                        //     the Yaesu to VFO; skip auto-DFM in Memory mode.
                        //
                        // Memory-restore extension:
                        //   - !in_memory guard removed; auto-DFM also works in Memory.
                        //   - At PTT-on: save memory_channel if in_memory.
                        //   - At PTT-off: after MD04, restore Memory mode via MC<nnn>;.
                        //   - Result: USB-mic TX works in Memory-FM and the operator stays
                        //     after PTT-off in Memory mode on the original channel.
                        let s_lock = status.lock().unwrap();
                        let mode_char = s_lock.mode_char;
                        let was_dfm = s_lock.auto_dfm_active;
                        let in_memory = s_lock.vfo_select == 1;
                        let mem_ch = s_lock.memory_channel;
                        drop(s_lock);

                        // Optimistically set TX state so the RX audio loop mutes immediately
                        // (the TX-poll confirms afterwards). Especially for the FTX-1, which does not
                        // mute its USB-RX in hardware during TX.
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
                                // Defensive diagnostic aid: Memory mode with memory_channel=0
                                // means silent memory-loss at PTT-off (no MC-restore).
                                if in_memory && mem_ch == 0 {
                                    warn!("{} auto-DATA: in Memory mode but memory_channel=0 - no MC-restore (state possibly not initialized)", prefix);
                                }
                                // To DATA mode first, settle 50ms, then PTT-on. In Memory mode
                                // the MD switch forces to VFO; channel saved + restored after PTT-off.
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
                                info!("{} auto-DATA: {} -> {} for PTT-on (memory={}, ch={})",
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
                            // PTT-off first (Yaesu switches TX off), settle 100ms for the
                            // TX transition, then the original mode back, possibly memory-restore.
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
                            info!("{} auto-DATA: DATA -> {} after PTT-off (mem-restore={})",
                                prefix, saved_mode, saved_mem);
                            String::new()  // all commands already sent
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
                        // FTX-1 requires the head prefix (PC{head}{nnn}); 991A does not (PC{nnn}).
                        // Server-side clamp on the band/head maximum (tx_power_max) so
                        // a client that ignores the maximum (e.g. older Android slider) can
                        // never drive the radio above its band limit. Floor = 5 W.
                        let (head, maxp) = {
                            let st = status.lock().unwrap();
                            (st.power_head, st.tx_power_max())
                        };
                        let maxp = if maxp >= 5 { maxp } else { 100 };
                        let v = v.clamp(5, maxp);
                        if head == 0 {
                            format!("PC{:03};", v)
                        } else {
                            format!("PC{}{:03};", head, v)
                        }
                    }
                    YaesuCmd::SetPower(on) => format!("PS{};", if on { 1 } else { 0 }),
                    // MC = memory recall. FTX-1: MAIN/SUB prefix + 5-digit channel
                    // (`MC0{ch:05};`, P1=0=MAIN); 991A: 3-digit (`MC{ch:03};`).
                    // Without the correct form, Mem-/Mem+ do nothing on the FTX-1.
                    YaesuCmd::RecallMemory(ch) => {
                        // Optimistic: a recall puts the set in memory mode, so vfo_select=1
                        // immediately (don't wait for the IF-poll) - otherwise a fast
                        // freq change afterwards does not escape to VFO (stale-state window). The IF-poll confirms.
                        {
                            let mut s = status.lock().unwrap();
                            s.vfo_select = 1;
                            s.vfo_escape_pending = 0; // recall = we WANT memory: no more escape guard
                        }
                        match model {
                            RadioModel::Ftx1 => format!("MC0{:05};", ch),
                            _ => format!("MC{:03};", ch),
                        }
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
                    // Typed DSP/function control (PATCH-yaesu-extra-controls, Phase A1).
                    // Generic P1=0/MAIN encode: works for the 991A (accepts the leading 0)
                    // and for the FTX-1 MAIN receiver (side=0). Model-specific controls are
                    // guarded (AMC=FTX-1 only, NB/NR on/off=991A only) so a stray
                    // packet to the wrong model is a no-op instead of surprising the radio; the
                    // client already gates this in the UI. Unknown/invalid-for-model -> empty (no-op).
                    // NB: hardware verification per command via the reply-parse below.
                    YaesuCmd::SetFeature(ctrl, val) => {
                        let is_ftx1 = matches!(model, RadioModel::Ftx1);
                        match ctrl {
                            0 => format!("RA0{};", val.min(1)), // RfAtt: RA P1=0 (fixed), P2 0/1
                            1 => format!("BI{};", val.min(1)),  // BreakIn: BI 0/1
                            2 => format!("NA0{};", val.min(1)), // Narrow: NA P1=0 (MAIN), P2 0/1
                            3 => format!("BC0{};", val.min(1)), // Auto-Notch (DNF): BC P1=0 (MAIN), P2 0/1
                            6 => format!("GT0{};", val.clamp(1, 4)), // AGC: GT P1=0, P2 1=fast/2=mid/3=slow/4=auto (hardware-verified §13; AUTO reads back as 4/5/6 -> server normalizes to 4)
                            7 => format!("PA0{};", val.min(2)), // Pre-amp/IPO (HF): PA P1=0, P2 0-2 (IPO/AMP1/AMP2)
                            8 => format!("NL0{:03};", val.min(10)),  // Noise Blanker level: NL P1=0, P2 000-010 (0=off)
                            9 => format!("RL0{:02};", val.min(10)),  // Noise Reduction (DNR) level: RL P1=0, P2 00-10 (0=off)
                            10 => format!("PL{:03};", val.min(100)), // Speech Processor level: PL 000-100 (0=off)
                            11 if is_ftx1 => format!("AO{:03};", val.clamp(1, 100)), // AMC output level: AO 001-100 (FTX-1-only; 991A has no AO)
                            13 if !is_ftx1 => format!("NB0{};", val.min(1)), // 991A: Noise Blanker on/off (NB), separate from NL level (FTX-1 encodes off-in-level)
                            14 if !is_ftx1 => format!("NR0{};", val.min(1)), // 991A: Noise Reduction on/off (NR), separate from RL level (FTX-1 encodes off-in-level)
                            // Fase D - CO P1=0, P2 (0=contour on/off,1=contour freq,2=APF on/off,3=APF freq), P3 4-digit.
                            15 => format!("CO00{:04};", val.min(1)),        // Contour on/off
                            16 => format!("CO02{:04};", val.min(1)),        // APF on/off
                            18 => format!("CO01{:04};", val.clamp(10, 3200)), // Contour freq (10-3200 Hz)
                            19 => format!("CO03{:04};", val.min(50)),       // APF freq (0000-0050)
                            // BP P1=0, P2 (0=notch on/off,1=notch freq), P3 3-digit.
                            17 => format!("BP00{:03};", val.min(1)),        // Manual notch on/off
                            20 => format!("BP01{:03};", val.clamp(1, 320)), // Manual notch freq (x10 Hz)
                            // Clarifier (§15, hardware-spec verified). 991A: RT/XT/RC/RU/RD
                            // (relative). FTX-1: CF - RX/TX-clar together in one P3=0 message (so
                            // read the other state from the tracked state) + absolute freq (P3=1).
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
                                        // Absolute: new offset from the tracked state, clamp ±9999.
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
                                        // 991A relative: accumulate state (no readback via a separate cmd).
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
                        let mode_char = internal_mode_to_yaesu(mode, model);
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
                    | YaesuCmd::ReadMemoryTones
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

        // Fast poll: S-meter every 200ms. FTX-1: also RI0; (P8 = squelch open/closed)
        // for the server-side software squelch. 991A: also RM6; (SWR meter, diagnostic)
        // + RI0; (official Hi-SWR flag P2 from the FT-991A CAT OM 1711-D — calibrated
        // trip point of the radio itself, instead of the uncalibrated RM6 threshold).
        if now.duration_since(last_smeter_poll).as_millis() >= 200 {
            last_smeter_poll = now;
            let fast: &[u8] = if matches!(model, RadioModel::Ftx1) { b"SM0;RI0;" } else { b"SM0;RM6;RI0;" };
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
            // Clarifier readback (§15): 991A reads RIT/XIT separately (RT/XT), offset we track
            // ourselves (no separate read-cmd). FTX-1: CF000=RX/TX-clar on/off, CF001=offset.
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

        // Output-stream recovery: if the TX output is not open (e.g. the CODEC was
        // busy / in exclusive mode / not yet free when opening), keep retrying
        // it periodically. This way TX comes back on its own once the device is
        // free -- without a manual server restart. The capture watchdog above
        // does not cover this (it only triggers on RX silence, and RX can run fine
        // while TX fails).
        if !output_stream.is_set() && last_output_retry.elapsed().as_secs() >= 5 {
            last_output_retry = Instant::now();
            if let Some(out_dev) = output_device.as_deref().or(audio_device.as_deref()) {
                match build_output_stream(out_dev, tx_producer.clone(), prefix) {
                    Ok((stream, _rate)) => {
                        output_stream.set(Some(stream));
                        info!("{} audio output recovered (device free)", prefix);
                    }
                    Err(e) => log::debug!("{} audio output retry failed: {}", prefix, e),
                }
            }
        }

        std::thread::sleep(Duration::from_millis(10));
    }
}
