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
/// Make a memory list the server's own copy: the filled-channel numbers (so Mem+/Mem-
/// can skip the gaps) and the blob that both the push and the FTX-1 tone keeper read.
///
/// Called from three places that mean the same thing - a read finished, a write
/// finished, or a write was refused but the list itself is still good.
fn adopt_memory_list(
    tab_text: &str,
    status: &Arc<Mutex<YaesuState>>,
    memory_data: &Arc<Mutex<Option<String>>>,
) {
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
        st.last_memory_blob = Some(tab_text.to_string());
    }
    *memory_data.lock().unwrap() = Some(tab_text.to_string());
}

/// Which capture channel this radio's audio should be taken from, for the model
/// as it is CURRENTLY known.
///
/// A property of the radio type, not of the slot. Only the FTX-1 offers a stereo
/// capture endpoint - 2 channels against a 991A's 1, carrying its two receivers
/// separately - and it does so in whichever slot it sits. Before this, slot 1 was hardwired to L and only
/// slot 2 could choose - so the same radio had the setting in one slot and not
/// in the other.
///
/// Worked out at every stream (re)build and not frozen at construction: a radio
/// that is switched off when the server starts is assumed to be a 991A and
/// adopts its real model when it finally answers `ID;`. An FTX-1 that arrived
/// that way would otherwise keep the 991A's channel for the rest of the session.
pub(super) fn effective_capture_channel(
    model: RadioModel,
    status: &Arc<Mutex<YaesuState>>,
) -> u8 {
    if matches!(model, RadioModel::Ftx1) {
        status.lock().unwrap().audio_channel
    } else {
        // One channel on the endpoint; the capture code takes it as-is.
        0
    }
}

pub(super) fn yaesu_reconnect_thread(
    cmd_rx: mpsc::Receiver<YaesuCmd>,
    // The loop's own way back into the command queue. The PTT watchdog uses it to
    // release PTT through exactly the same handler an operator release goes through,
    // so the auto-DATA mode restore and the USB routing restore cannot drift apart
    // from it.
    self_tx: mpsc::Sender<YaesuCmd>,
    status: Arc<Mutex<YaesuState>>,
    memory_data: Arc<Mutex<Option<String>>>,
    menu_data: Arc<Mutex<Option<String>>>,
    port_name: String,
    baud: u32,
    audio_device: Option<String>,
    output_device: Option<String>,
    rx_audio_tx: tokio::sync::mpsc::Sender<Vec<f32>>,
    capture_stream: Arc<StreamHolder>,
    output_stream: Arc<StreamHolder>,
    tx_producer: Arc<Mutex<Option<ringbuf::HeapProd<f32>>>>,
    last_audio_time: Arc<std::sync::atomic::AtomicU64>,
    mut model: RadioModel,
    // The struct's copy of the model, read by the presence push. Written here
    // when the radio's own ID; names a different model than was assumed - an
    // assumption made for a silent port must not outlive the port speaking.
    model_shared: Arc<std::sync::atomic::AtomicU8>,
    // False once the owning `YaesuRadio` has been dropped. Checked wherever this
    // loop would otherwise carry on for the life of the process - it holds a
    // sender of its own command channel, so a closed channel cannot tell it that
    // nobody is listening any more.
    alive: Arc<std::sync::atomic::AtomicBool>,
    slot: u8,
    mut prefix: String,
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
        // Nobody owns this radio any more: stop, rather than keep a port open
        // and write log lines about a radio no client can see.
        if !alive.load(std::sync::atomic::Ordering::Relaxed) {
            info!("{} no longer in use, stopping", prefix);
            return;
        }
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
                // Name the failure while it lasts. A free, existing USB serial
                // port practically never fails to open, so the two failures
                // that do happen are worth their own words - above all
                // access-denied, which means other control software holds the
                // CAT port (the most common reason a radio "is not there").
                // Stored in the state so the presence push can carry it to the
                // client; logged only when the classification changes, because
                // this loop retries every 3 seconds for as long as it takes.
                let trouble = classify_open_error(&e);
                {
                    let mut s = status.lock().unwrap();
                    if s.port_trouble != trouble {
                        s.port_trouble = trouble;
                        if let Some(text) = port_trouble_log_text(trouble) {
                            warn!("{} {}: {}", prefix, port_name, text);
                        }
                    }
                }
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
        // The port opened: whatever named trouble there was is over.
        status.lock().unwrap().port_trouble = sdr_remote_core::protocol::PORT_TROUBLE_NONE;

        // Open succeeded - log the transition and reset the dedup flag for the
        // next possible outage cycle. The connect line contains COM+baud so
        // operator-checklist item (a) is directly greppable.
        if ever_connected {
            info!("{} serial reconnected on {} @ {} baud", prefix, port_name, baud);
        } else {
            info!("{} serial connected on {} @ {} baud", prefix, port_name, baud);
            // Counted so a later symptom can name its likely cause. See
            // `two_slot_hint`: two ports can be one radio, and then the two
            // pollers talk over each other.
            super::ACTIVE_CAT_SLOTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            struct SlotGuard;
            impl Drop for SlotGuard {
                fn drop(&mut self) {
                    super::ACTIVE_CAT_SLOTS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
            let _slot_guard = SlotGuard;
            ever_connected = true;
        }
        disconnect_logged = false;

        // Bring-up probe: once after each successful open, dump the
        // raw ID;/IF;/MD0;/FA; + one parse summary. Makes it live
        // visible whether the radio parses as 991A structure or where it deviates.
        //
        // And when the radio names a model this build knows, that names wins over
        // whatever was assumed. The 991A fallback exists for a radio that was OFF
        // during startup detection; an FTX-1 switched on later used to be driven
        // a whole session in the 991A dialect - dead Mem+/Mem-, a memory list of
        // zero channels - with nothing but a warning nobody sees. Everything
        // model-specific in this thread reads `model` per iteration, so adopting
        // here, before the per-connect reads below, corrects all of it.
        if let Some(detected) = bringup_probe(&mut port, &prefix, model) {
            if detected != model {
                model = detected;
                prefix = model.tag(slot);
                model_shared.store(model.as_code(), std::sync::atomic::Ordering::Relaxed);
                info!("{} dialect adopted from the radio's own ID", prefix);
            }
        }

        // Is the radio awake? A set on standby keeps its CAT port alive and
        // answers `PS;` - that is the whole basis of the power button - but
        // answers nothing else. Asking it anything now returns empty strings
        // that get logged as failures and stored as zeroes ("max-TX-power EX:
        // HF=0 50M=0 ...", "snapshot partial (0/4)"), which is worse than not
        // asking: the session then starts with values nobody can trust.
        let awake = cat_query(&mut port, "PS;").contains("PS1");
        if !awake {
            info!(
                "{} the radio is on standby - reading nothing from it until it is switched on",
                prefix
            );
            status.lock().unwrap().power_on = false;
        }

        // 991A SSB/AM USB TX routing is session-owned: snapshot only the menus
        // TL may temporarily change, then restore those exact values later. Do
        // not force a factory/default hand-mic state at connect; users may have
        // a custom normal setup (for example a USB microphone on the radio).
        let ft991a_usb_routing_snapshot = if !matches!(model, RadioModel::Ftx1) && awake {
            Some(Ft991aUsbRoutingSnapshot::read(&mut port, &prefix))
        } else {
            // No snapshot rather than a snapshot of nothing: the restore path
            // already refuses to put back values it never read, and that is the
            // right outcome for a radio that was asleep when we looked.
            None
        };

        // 991A per-band max TX power from the EX menu (PATCH-yaesu-power-scaling):
        // EX137 HF, EX138 50M, EX139 144M, EX140 430M (watt). Determines the client
        // slider range per band. FTX-1 = phase B (head-max via tx_power_max()).
        if !matches!(model, RadioModel::Ftx1) && awake {
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

        // FTX-1 firmware version, once per connect. RT Systems documents 1.08 as the
        // minimum for programming this radio, and the memory-tone write is unexplained
        // - so this belongs in the log rather than in an open question.
        // CAT OM p26: VE FIRMWARE VERSION, read `VE P1;` with P1 0 = MAIN CPU.
        if matches!(model, RadioModel::Ftx1) {
            let v = cat_query(&mut port, "VE0;");
            let v = v.trim();
            if v.is_empty() {
                warn!("{} firmware version: no answer to VE0;", prefix);
            } else {
                info!("{} firmware (MAIN CPU): [{}]", prefix, v.escape_debug());
            }
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

            let capture_channel = effective_capture_channel(model, &status);
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
            port, &cmd_rx, &self_tx, &status, &memory_data, &menu_data,
            &audio_device, &output_device, &rx_audio_tx, &capture_stream, &output_stream, &tx_producer, &last_audio_time,
            model, &alive, slot, &prefix, ft991a_usb_routing_snapshot,
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
    self_tx: &mpsc::Sender<YaesuCmd>,
    status: &Arc<Mutex<YaesuState>>,
    memory_data: &Arc<Mutex<Option<String>>>,
    menu_data: &Arc<Mutex<Option<String>>>,
    audio_device: &Option<String>,
    output_device: &Option<String>,
    rx_audio_tx: &tokio::sync::mpsc::Sender<Vec<f32>>,
    capture_stream: &Arc<StreamHolder>,
    output_stream: &Arc<StreamHolder>,
    tx_producer: &Arc<Mutex<Option<ringbuf::HeapProd<f32>>>>,
    last_audio_time: &Arc<std::sync::atomic::AtomicU64>,
    model: RadioModel,
    alive: &Arc<std::sync::atomic::AtomicBool>,
    // Which slot this radio sits in - the key its kept tones are filed under.
    slot: u8,
    prefix: &str,
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
    // Whether the operator has been told, once, that this radio cannot transmit.
    // The retry itself is silent on repeat - but silence about a dead TX path is
    // worse than noise, and the log used to manage both at once: 119 repeats of
    // an intermediate fallback notice, and not one line saying what it cost.
    let mut output_failure_reported = false;
    // Warn-once guards: prevent 500 ms-poll log spam while they
    // remove the current silent defaults. `warned_modes` = unknown MD codes
    // (one warn per unique char); `warned_short_if` = deviating IF length (one warn).
    let mut warned_modes: HashSet<char> = HashSet::new();
    let mut warned_short_if = false;
    // Read the memory channels ONCE per radio connect, rather than once per
    // CLIENT connect. Walking 117 channels takes ~0.6s (991A) / ~1.4s (FTX-1) and
    // blocks every other CAT command on this single-threaded loop for that whole
    // time; doing it again for each client repeats work whose answer has not
    // changed. Clients are served from the copy this fills (`last_memory_blob`).
    // What is traded away is freshness: a channel edited on the radio's own front
    // panel is not noticed until "Read radio" is pressed.
    // FTX-1 tone keeper. The radio cannot STORE a tone per memory channel over CAT -
    // measured from both sides, see docs/internal/OPEN-ftx1-memory-tone-write.md - but
    // it does accept one for the channel it is sitting on, and keeps it until the next
    // channel change. So re-apply it on every change: the operator's list then decides
    // what the radio actually transmits, which is what the tone is for.
    //
    // What this is not: it does not write to the memory bank, and it only holds while
    // ThetisLink is connected. Take TL away and the radio falls back to whatever is
    // stored in the channel.
    let mut tone_keeper_last: Option<u16> = None;
    // When we asked the radio to transmit. The PTT watchdog below needs it for both
    // of its checks; `None` = we are not asking for TX.
    let mut ptt_on_at: Option<Instant> = None;
    let mut auto_read_pending = true;
    // The per-channel tones are not in the bulk read (P9 is fixed "00"), so without
    // this a freshly connected client sees a list without tones. Read them once here
    // too, right after the memory list they depend on. Side effect worth knowing: the
    // radio briefly steps through the channels that have a tone mode and returns to
    // where it was - a handful of repeater channels, not the whole bank.
    let mut auto_tones_pending = true;
    // The EX/menu values follow the same rule, but LAST: an operator looks at the
    // memory list first, and the FTX-1 EX scan occupies the CAT link for seconds.
    let mut auto_menu_pending = true;
    // ...but not on the first iteration. The read needs to know which memory the
    // radio is on (the per-channel CTCSS tone is not in the bulk read; only the
    // current channel's tone can be fetched, via CN), and that comes from the IF
    // answer to the 500ms full poll, which has not been parsed yet at t=0. Let the
    // radio and the poll loop settle first - the same ~1.5s the FTX-1 auto-read
    // already waits for.
    let session_start = Instant::now();

    loop {
        // Same check as the reconnect loop above, for a session already running
        // when its radio is dropped.
        if !alive.load(std::sync::atomic::Ordering::Relaxed) {
            info!("{} no longer in use, closing the port", prefix);
            return;
        }
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

        // PTT watchdog: ThetisLink must not keep transmitting after the radio has
        // stopped. Nothing here polls anything extra - both signals are already in
        // hand - and nothing here runs on the PTT-on path, so keying stays as fast
        // as it was.
        //
        // Two independent reasons to let go, because they cover different failures:
        //
        //   the radio says so   the FTX-1 reports its real TX state in RI P4, which
        //                       the 200 ms poll already reads. This catches ANY cause
        //                       - the time-out timer, a fault, a hand on the set - and
        //                       is the truthful signal. The FT-991A has no reliable
        //                       equivalent (its `TX;` was measured unreliable), so it
        //                       gets nothing from this half.
        //
        //   the timer ran out   both models expose their TX time-out timer in the EX
        //                       menu, which is read once at connect. Knowing the limit,
        //                       we can stop just before the radio does instead of
        //                       finding out afterwards. This is the only half that
        //                       covers the FT-991A.
        //
        // Armed and disarmed ONLY by the SetPtt handler below (and by the release
        // here). Deliberately NOT gated on `tx_active`: that field is overwritten
        // by the radio's own `TX;` answer, and the FT-991A's `TX;` was measured
        // unreliable - one spurious "TX off" mid-transmission would silently
        // disarm the only net that radio has. Whether WE are asking for TX is
        // something we know without asking the radio.
        if let Some(since) = ptt_on_at {
            let (tot, streak) = {
                let s = status.lock().unwrap();
                (s.tot_minutes, s.radio_rx_streak)
            };
            // A run of four, not one: a single garbled or stale RI answer must
            // never cut a live transmission. At the 200 ms fast poll that is under
            // a second, and the radio has already stopped by then anyway.
            let radio_dropped = matches!(model, RadioModel::Ftx1)
                && since.elapsed() >= Duration::from_millis(1500)
                && streak >= 4;
            // Just under the limit rather than on it, so we are first and the
            // operator hears a clean end instead of a cut.
            let timer_due = tot > 0
                && since.elapsed() >= Duration::from_secs(tot as u64 * 60).saturating_sub(
                    Duration::from_millis(1500));
            if radio_dropped || timer_due {
                if radio_dropped {
                    warn!(
                        concat!(
                            "{} the radio stopped transmitting on its own (RI P4) - ",
                            "releasing PTT; a time-out timer, a fault or the set's own ",
                            "PTT will do this",
                        ),
                        prefix,
                    );
                } else {
                    warn!(
                        concat!(
                            "{} the radio's {}-minute TX time-out timer is about to ",
                            "expire - releasing PTT",
                        ),
                        prefix, tot,
                    );
                }
                ptt_on_at = None;
                // Through the queue, not inline: the release then does everything a
                // normal release does.
                let _ = self_tx.send(YaesuCmd::SetPtt(false));
            }
        }

        // Keep the FTX-1's tone in step with the list (see `tone_keeper_last`).
        //
        // Not while the connect-time reads are still coming in. The list at that
        // moment is whatever the radio just gave, and for this radio that is
        // exactly the value it cannot keep - so the keeper would write 100.0 Hz
        // onto a channel the operator had set to 77.0, moments before the kept
        // tones arrive and say so (seen at a station on 2026-08-12). Waiting
        // costs a second; writing the wrong tone costs a repeater contact.
        if matches!(model, RadioModel::Ftx1) && !auto_read_pending && !auto_tones_pending {
            let (in_memory, ch, tx) = {
                let st = status.lock().unwrap();
                (st.vfo_select == 1, st.memory_channel, st.tx_active)
            };
            if !in_memory || ch == 0 {
                tone_keeper_last = None; // out of memory mode: apply again on return
            } else if !tx && tone_keeper_last != Some(ch) {
                let blob = status.lock().unwrap().last_memory_blob.clone();
                if let Some(b) = blob {
                    if let Some((num, is_dcs, mode)) =
                        crate::yaesu::memory::tone_wanted_for_channel(&b, ch)
                    {
                        let ct = crate::yaesu::memory::ftx1_tone_mode_to_ct_pub(mode);
                        let _ = port.write_all(format!("CT0{};", ct).as_bytes());
                        std::thread::sleep(Duration::from_millis(30));
                        let p2 = if is_dcs { 1 } else { 0 };
                        let _ = port.write_all(format!("CN0{}{:03};", p2, num).as_bytes());
                        info!("{} channel {}: tone re-applied from the list (CT0{} CN0{}{:03})",
                              prefix, ch, ct, p2, num);
                    }
                    // Only mark it done when there WAS a list to consult; otherwise
                    // retry on the next pass, once the list has arrived.
                    tone_keeper_last = Some(ch);
                }
            }
        }

        // Nothing is read from a radio that is in standby, and this is not
        // politeness - it is what makes the power button work.
        //
        // A radio on standby keeps its CAT port alive (that is how `PS1;` can
        // wake it) but answers nothing else. The connect-time reads then walk
        // 117 memory channels, 20 tones and 153 menu items into timeouts, which
        // takes over a minute, and an operator pressing "power on" in that
        // minute has their command sit in the queue behind it. Measured twice
        // at a station: click at 22:57:40, radio actually woke at 22:59:01, and
        // the log in between is nothing but "did not confirm channel N"
        // (2026-08-12). The reads keep their turn - `power_on` going true is
        // what releases them.
        let radio_is_on = status.lock().unwrap().power_on;
        let incoming = if auto_read_pending
            && radio_is_on
            && session_start.elapsed() >= Duration::from_millis(1500)
        {
            auto_read_pending = false;
            info!("{} reading the memory channels once, now the radio is up", prefix);
            Ok(YaesuCmd::ReadAllMemories)
        } else if auto_tones_pending && !auto_read_pending && radio_is_on {
            auto_tones_pending = false;
            // Only meaningful if the memory read produced something; the tone walk
            // needs that list to know which channels have a tone mode at all.
            if status.lock().unwrap().last_memory_blob.is_some() {
                info!("{} reading the memory tones once, now the radio is up", prefix);
                Ok(YaesuCmd::ReadMemoryTones)
            } else {
                cmd_rx.try_recv()
            }
        } else if auto_menu_pending && !auto_read_pending && !auto_tones_pending && radio_is_on {
            auto_menu_pending = false;
            info!("{} reading the EX settings once, now the radio is up", prefix);
            Ok(YaesuCmd::ReadAllMenus)
        } else {
            cmd_rx.try_recv()
        };
        match incoming {
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
                        // Carry over the tones we already knew. The bulk read cannot
                        // fetch them (P9 is fixed "00"), so a fresh read has the tone
                        // columns empty - which silently threw away the tones every
                        // time anyone pressed "read radio", or an older client asked
                        // for a read on connect. Only for channels whose frequency is
                        // unchanged: a reprogrammed channel keeps no old tone.
                        let tab_text = {
                            let previous = status.lock().unwrap().last_memory_blob.clone();
                            match previous {
                                Some(prev) => {
                                    let old_freqs = crate::yaesu::memory::freqs_from_blob(&prev);
                                    let new_freqs = crate::yaesu::memory::freqs_from_blob(&tab_text);
                                    let keep: Vec<_> = crate::yaesu::memory::tones_from_blob(&prev)
                                        .into_iter()
                                        .filter(|(ch, _, _)| {
                                            let o = old_freqs.iter().find(|(c, _)| c == ch).map(|(_, f)| f);
                                            let n = new_freqs.iter().find(|(c, _)| c == ch).map(|(_, f)| f);
                                            o.is_some() && o == n
                                        })
                                        .collect();
                                    if keep.is_empty() {
                                        tab_text
                                    } else {
                                        info!("{} kept {} known tone(s) across the read", prefix, keep.len());
                                        // Carry-over: these ARE the list's tones going
                                        // back on, so they overwrite.
                                        crate::yaesu::memory::merge_tones_into_blob(&tab_text, &keep, false, prefix)
                                    }
                                }
                                None => tab_text,
                            }
                        };
                        let count = tab_text.lines().count() - 1;
                        info!("{} read {} memory channels", prefix, count);
                        // Persist the filled channel numbers (first column) so
                        // Mem+/Mem- can skip empties - memory_data itself is taken
                        // when sent to the client.
                        adopt_memory_list(&tab_text, &status, &memory_data);
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
                            // What the last session held comes first: for this
                            // radio the list is the truth, and a tone it cannot
                            // store is only in that list. The radio's own read
                            // then fills the gaps (a tone set on the front
                            // panel, which does work).
                            let kept = crate::yaesu::tone_store::load(slot, model, prefix);
                            let blob = if kept.is_empty() {
                                blob
                            } else {
                                crate::yaesu::memory::merge_tones_into_blob(
                                    &blob, &kept, false, prefix,
                                )
                            };
                            if !tones.is_empty() || !kept.is_empty() {
                                let merged = crate::yaesu::memory::merge_tones_into_blob(
                                    &blob, &tones, matches!(model, RadioModel::Ftx1), prefix,
                                );
                                status.lock().unwrap().last_memory_blob = Some(merged.clone());
                                // Hand the filled-in list to the client, same route
                                // as a normal memory read.
                                *memory_data.lock().unwrap() = Some(merged.clone());
                                info!("{} tones merged into the memory list", prefix);
                                // The set is holding whatever the tone keeper put
                                // there a moment ago, which was read off a list
                                // that did not have the kept tones in it yet - so
                                // the radio sat on 100.0 Hz while the list said
                                // 77.0 (2026-08-12). Forgetting which channel was
                                // last done makes the keeper apply this list to
                                // the channel the radio is on, now that it is the
                                // right list.
                                tone_keeper_last = None;
                                // And keep what the list now holds, so the next
                                // start begins where this one ended.
                                crate::yaesu::tone_store::save(
                                    slot,
                                    model,
                                    &crate::yaesu::memory::tones_from_blob(&merged),
                                    prefix,
                                );
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
                // The FTX-1 gate. Writing this radio's bank costs every CTCSS tone
                // stored in it: `MW` resets the channel's tone to 100.0 Hz and no CAT
                // command puts it back. That is not a cost to pay by accident, so the
                // write only happens once the operator has accepted it in the server
                // GUI. The FT-991A never reaches this check.
                if matches!(model, RadioModel::Ftx1)
                    && !status.lock().unwrap().ftx1_memory_write_ack
                {
                    warn!(
                        concat!(
                            "{} memory write refused: writing the FTX-1's memory resets the ",
                            "CTCSS tone of every channel it touches to 100.0 Hz, and the radio ",
                            "has no CAT command to put it back. Accept that in the server ",
                            "settings (Yaesu > 'Allow writing FTX-1 memory channels') if you ",
                            "want to write anyway.",
                        ),
                        prefix
                    );
                    // Refusing the RADIO write is not a reason to throw the list away.
                    // On this model the list is what decides the tone anyway: the keeper
                    // applies it on every channel landing, and setting a tone that way is
                    // free - it takes effect and is never stored. So adopt the list and
                    // skip only the part that does damage.
                    //
                    // What this costs is stated where the operator can see it: without
                    // the tick, a frequency or name edited here lives in the server's
                    // list and not in the radio.
                    adopt_memory_list(&tab_text, &status, &memory_data);
                    // This list is now the only place these tones exist, so it
                    // is written down. Without this the operator's work lived
                    // until the next server restart and no further (the fault
                    // two problem reports pinned down on 2026-08-12).
                    crate::yaesu::tone_store::save(
                        slot,
                        model,
                        &crate::yaesu::memory::tones_from_blob(&tab_text),
                        prefix,
                    );
                    info!(
                        concat!(
                            "{} the list is now the server's own (its tones are applied ",
                            "per channel); the radio itself was not written",
                        ),
                        prefix
                    );
                    continue;
                }
                let is_tx = || status.lock().map(|s| s.tx_active).unwrap_or(false);
                let write_result = match model {
                    // FTX-1 writes freq via MW + name via MT (both 5-digit).
                    RadioModel::Ftx1 => write_all_memories_ftx1(&mut port, &tab_text, ret, &is_tx),
                    _ => write_all_memories(&mut port, &tab_text, ret, &is_tx),
                };
                match write_result {
                    Ok(count) => {
                        info!("{} wrote {} memory channels", prefix, count);
                        // The list we just wrote IS the truth now, so the server's copy
                        // has to become it. Without this the copy stayed on the values
                        // from before the write, and two things went wrong with it: the
                        // safety net pushed that stale list back over the client within
                        // the minute (the freshly written tone vanished from the table
                        // again), and a later read carried the OLD tones over the new
                        // ones - same channel, same frequency, so the carry-over could
                        // not tell them apart. Written, then overwritten by its own
                        // predecessor.
                        adopt_memory_list(&tab_text, &status, &memory_data);
                    }
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
                        // Keep the server's own copy (so a client can be served without
                        // the radio being walked again) AND publish it for the push.
                        {
                            let tot = crate::yaesu::memory::tot_minutes_from_menu_blob(model, &data);
                            let mut s = status.lock().unwrap();
                            s.last_menu_blob = Some(data.clone());
                            s.tot_minutes = tot;
                            // Both branches, because the absence is the more useful
                            // half: on an FT-991A the time-out timer is the ONLY thing
                            // that releases PTT for us, so tot == 0 means no net at all.
                            // Logging only the presence put the asymmetry the wrong way
                            // round.
                            if tot > 0 {
                                info!(
                                    concat!(
                                        "{} TX time-out timer is set to {} min - ",
                                        "ThetisLink releases PTT just before it fires",
                                    ),
                                    prefix, tot,
                                );
                            } else if matches!(model, RadioModel::Ft991a) {
                                warn!(
                                    concat!(
                                        "{} no TX time-out timer is set - EX 036 did not read back a ",
                                        "usable value. This radio has no transmit ",
                                        "readback, so nothing will ",
                                        "release PTT if it stops on its own{}",
                                    ),
                                    prefix,
                                    super::two_slot_hint(),
                                );
                            } else {
                                info!(
                                    concat!(
                                        "{} no TX time-out timer is set; the radio's own ",
                                        "transmit state is followed instead",
                                    ),
                                    prefix,
                                );
                            }
                        }
                        *menu_data.lock().unwrap() = Some(format!("MENU:{}", data));
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
                            // `MA` only COPIES the channel into the VFO; on the FTX-1 it
                            // does not leave memory mode, so the radio kept operating from
                            // the channel and put its own frequency back a few seconds
                            // later. Measured, not guessed: the escape guard's own warning
                            // said "radio stayed Memory (P7='1')".
                            //
                            // Leaving is a separate command there - VM P1 P2P2 with P1=0
                            // MAIN and P2=00 VFO (FTX-1 CAT OM 2508-C p27), which is the
                            // parameterised VM, not the bare `VM;` that WRITES a memory.
                            // The FT-991A leaves on `MA` alone and is left as it was.
                            match model {
                                RadioModel::Ftx1 => format!("MA;VM000;MD0{};FA{:09};", mc, hz),
                                _ => format!("MA;MD0{};FA{:09};", mc, hz),
                            }
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
                        // Start/stop the watchdog's clock alongside the PTT itself.
                        ptt_on_at = if on { Some(Instant::now()) } else { None };
                        if on {
                            // Do not carry an old radio answer into a new transmission.
                            status.lock().unwrap().radio_rx_streak = 0;
                        }

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
                                // Same five-digit form the rest of the FTX-1 paths use.
                                // A three-digit MC is rejected there, so the operator was
                                // left in VFO on the wrong frequency after an FM
                                // transmission from a memory channel - silently, because
                                // the write itself succeeded.
                                let mc_cmd = match model {
                                    RadioModel::Ftx1 => format!("MC0{:05};", saved_mem),
                                    _ => format!("MC{:03};", saved_mem),
                                };
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
                    YaesuCmd::SetPower(on) => {
                        if on {
                            // Waking a set in standby has a ritual, and a bare
                            // PS1; is not it. FT-991A CAT OM (1711-D, PS): the
                            // power-ON "requires dummy data be initially sent.
                            // Then after one second and before two seconds the
                            // command is sent." Without the dummy and the pause
                            // the radio ignores the command - the operator's
                            // power button then only works after somebody
                            // switched the set on by hand once (field report,
                            // 2026-08-12). The pause blocks this serial thread
                            // alone, and the radio it talks to is off.
                            //
                            // The FTX-1 has no CAT power-ON at all: its PS Set
                            // documents only P1=0, OFF (CAT OM 2508-C). Say so
                            // instead of sending a command the set cannot obey.
                            if matches!(model, RadioModel::Ftx1) {
                                info!(
                                    "{} the FTX-1 cannot be switched ON via CAT (PS knows only OFF) - use the set's own switch",
                                    prefix
                                );
                                String::new()
                            } else {
                                if let Err(e) = port.write_all(b";") {
                                    warn!("{} power-on dummy write failed: {}", prefix, e);
                                }
                                std::thread::sleep(Duration::from_millis(1500));
                                "PS1;".to_string()
                            }
                        } else {
                            "PS0;".to_string()
                        }
                    }
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
                            // V/M. This used to send a bare "VM;" as a "toggle". It is
                            // not one: both CAT manuals give it as a WRITE.
                            //   FTX-1  p26: VM  MAIN-SIDE TO MEMORY CHANNEL
                            //   991A   p18: VM  VFO-A TO MEMORY CHANNEL
                            // One click on that button therefore overwrote a memory
                            // channel with whatever the VFO happened to hold. (The
                            // FTX-1's VM0nn; WITH parameters is a different command on
                            // the same page - that one switches VFO/memory mode and is
                            // what the memory write uses.)
                            //
                            // A toggle that writes nothing: entering memory is a plain
                            // recall of the current channel, and leaving it starts with
                            // `MA;` (MEMORY CHANNEL TO VFO-A / to MAIN-side) on both
                            // radios. But `MA` only COPIES: the 991A leaves memory
                            // operation on it, the FTX-1 does not and needs `VM000;`
                            // (P1=0 MAIN, P2=00 VFO - FTX-1 CAT OM 2508-C p27) as well.
                            // Without it the button appeared to work and the radio then
                            // put the channel's own frequency back, the same defect the
                            // frequency escape above had.
                            3 => {
                                let (in_memory, ch) = {
                                    let st = status.lock().unwrap();
                                    (st.vfo_select == 1 || st.vfo_select == 2, st.memory_channel)
                                };
                                if in_memory {
                                    match model {
                                        RadioModel::Ftx1 => "MA;VM000;".to_string(),
                                        _ => "MA;".to_string(),
                                    }
                                } else {
                                    let ch = if ch >= 1 { ch } else { 1 };
                                    match model {
                                        RadioModel::Ftx1 => format!("MC0{:05};", ch),
                                        _ => format!("MC{:03};", ch),
                                    }
                                }
                            }
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
                        let capture_channel = effective_capture_channel(model, &status);
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
                        output_failure_reported = false;
                    }
                    Err(e) => {
                        if !output_failure_reported {
                            output_failure_reported = true;
                            // Said once, and said in terms of what it costs: no
                            // transmit audio through this radio until the device
                            // turns up. A configured output that names a capture
                            // endpoint - a microphone - is the usual cause, and
                            // it can never match, so this retries forever.
                            warn!(
                                "{} NO TRANSMIT AUDIO: cannot open output device '{}' ({}). \
                                 Retrying every 5 s. Check the TX audio device in the server \
                                 settings - it must be a playback device, not a microphone.",
                                prefix, out_dev, e
                            );
                        } else {
                            log::debug!("{} audio output retry failed: {}", prefix, e);
                        }
                    }
                }
            }
        }

        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod capture_channel_tests {
    use super::{effective_capture_channel, RadioModel, YaesuState};
    use std::sync::{Arc, Mutex};

    fn state_with(channel: u8) -> Arc<Mutex<YaesuState>> {
        let st = YaesuState::default();
        let st = Arc::new(Mutex::new(st));
        st.lock().unwrap().audio_channel = channel;
        st
    }

    /// The choice belongs to the radio type. An FTX-1 gets it in EITHER slot -
    /// slot 1 used to be hardwired to L, so the same radio had the setting in
    /// one slot and not in the other (2026-08-20).
    #[test]
    fn an_ftx1_gets_the_operators_choice() {
        for channel in [0u8, 1, 2] {
            assert_eq!(
                effective_capture_channel(RadioModel::Ftx1, &state_with(channel)),
                channel
            );
        }
    }

    /// A 991A reports a single channel, so there is nothing to choose and the
    /// setting must not reach it - not even to ask for a "mix" of one channel.
    #[test]
    fn a_991a_takes_the_only_channel_it_has() {
        for channel in [0u8, 1, 2] {
            assert_eq!(
                effective_capture_channel(RadioModel::Ft991a, &state_with(channel)),
                0
            );
        }
    }
}
