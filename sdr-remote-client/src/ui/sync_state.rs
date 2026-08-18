// SPDX-License-Identifier: GPL-2.0-or-later
//! `SdrRemoteApp::sync_state`: pulls the latest `RadioState` from the server watch
//! channel and fans it out into the UI-side app state each frame (frequencies, modes,
//! meters, Yaesu/VRX/relay/amplitec status, pending-write reconciliation). Extracted
//! verbatim from `ui/mod.rs` - pure relocation, no behaviour change. `pub(super)` keeps
//! it callable from the parent module tree; `use super::*;` brings in the types/imports.

use super::*;

/// Content fingerprint of a pushed blob. Cheaper to keep than the blob itself, and
/// all we need to know: "is this the same list I already parsed?"
fn blob_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

impl SdrRemoteApp {
    /// Optimistic audio-enable reconciliation (RX1/RX2 share this one path). The
    /// client shows the requested value immediately (set on the toggle / at startup,
    /// even before a connection); the server is authoritative and corrects it, but a
    /// short grace window stops the server's pre-request default from clobbering a
    /// just-made request - which made RX2 flip off-then-on for ~1 s on startup.
    /// `pending` carries (since, want): it clears once the server confirms `want`, or
    /// after the grace window (then the server value wins, so an enable the server
    /// can't or didn't honour turns back off).
    ///
    /// DESIGN NOTE - INVARIANT: the six audio enables (RX1, RX2, VRX1, VRX2, Yaesu1,
    /// Yaesu2) must all show IDENTICAL behaviour - the value the user set (or the
    /// persisted value at startup) appears at once, even before a connection, and the
    /// server may only veto it, never introduce a visible lag. VRX audio (`vrx*_enabled`)
    /// and the Yaesu audio chips (`yaesu*_enabled`) are already client-optimistic: the
    /// server keeps no enabled mirror for them, so the client owns the value outright.
    /// RX1/RX2 are the only two the server also tracks, so they route through this
    /// reconcile to LOOK the same (immediate) while the server can still turn them off.
    /// If VRX or Yaesu ever gain a server-side enabled state, send them through THIS
    /// helper too - never a bespoke server-clobber path (a direct `self.x = state.x`
    /// is exactly what gave RX2 the ~1 s off-then-on lag this fixes).
    fn reconcile_audio_enable(cur: &mut bool, pending: &mut Option<(Instant, bool)>, server: bool) {
        const GRACE: std::time::Duration = std::time::Duration::from_secs(3);
        match *pending {
            Some((since, want)) => {
                if server == want || since.elapsed() >= GRACE {
                    *cur = server;
                    *pending = None;
                } else {
                    *cur = want; // still optimistic within the grace window
                }
            }
            None => *cur = server,
        }
    }

    /// The radio stopped transmitting without us asking, so let go of the local PTT
    /// latch too.
    ///
    /// It stops on its own more often than you would think: a TX time-out timer runs
    /// out, a fault trips, someone keys the set's own PTT. The button already goes
    /// grey (it follows the reported state), but the latch behind it stayed held, so
    /// the next click only released a PTT that was no longer transmitting and it took
    /// a second click to key again - exactly the moment you did not want to press
    /// twice.
    ///
    /// Caller checks for a confirmed transmitting -> not transmitting edge. Acting on
    /// "not transmitting" alone would unlatch in the gap between keying and the server
    /// confirming it.
    fn release_ptt_latch(&mut self, slot: u8) {
        if slot == 0 {
            self.yaesu_mouse_ptt = false;
            self.yaesu_ptt_last_sent = false;
        } else {
            self.yaesu2_mouse_ptt = false;
            self.yaesu2_ptt_last_sent = false;
        }
        self.apply_ptt_spike_protection(true, false);
    }

    pub(super) fn sync_state(&mut self) {
        let state = self.state_rx.borrow().clone();
        // Yaesu STATE subscription (freq/s-meter/CAT) follows the OPEN window, separate
        // from the audio checkbox -> a muted window stays live. Reset on disconnect so
        // it is re-sent after reconnect (server default is off).
        // Cleared by the session counter above rather than by `!state.connected`:
        // this was a fourth path with the same weakness. After a quick restart
        // the server has forgotten the subscription while this side still
        // believes it was sent, and then the comparison below finds nothing to
        // do and the window stays dead.
        if self.yaesu_state_sent != Some(self.yaesu_popout) {
            let _ = self.cmd_tx.send(Command::SetControl(
                ControlId::YaesuStateEnable, self.yaesu_popout as u16));
            self.yaesu_state_sent = Some(self.yaesu_popout);
        }
        if self.yaesu2_state_sent != Some(self.yaesu2_popout) {
            let _ = self.cmd_tx.send(Command::SetControl(
                ControlId::Yaesu2StateEnable, self.yaesu2_popout as u16));
            self.yaesu2_state_sent = Some(self.yaesu2_popout);
        }
        // Send Yaesu enable on first connect if persisted as enabled
        if state.connected && self.yaesu_enabled && !self.yaesu_enable_sent {
            let _ = self.cmd_tx.send(Command::SetControl(
                ControlId::YaesuEnable, 1));
            let _ = self.cmd_tx.send(Command::SetYaesuVolume(self.yaesu_volume));
            // Sync local mic gain to engine
            let _ = self.cmd_tx.send(Command::SetYaesuTxGain(self.yaesu_mic_gain));
            // Sync client-side TX chain (compressor + AGC) radio 1.
            let _ = self.cmd_tx.send(Command::SetYaesuCompressor(self.yaesu_compressor));
            let _ = self.cmd_tx.send(Command::SetYaesuTxAgc(self.yaesu_tx_agc));
            self.yaesu_enable_sent = true;
        }
        // 991A memory auto-read on radio DETECTION (yaesu_connected rising edge),
        // INDEPENDENT of audio-enable - just like the FTX-1. The enable block above
        // is gated on yaesu_enabled; with audio off the auto-read didn't fire and the
        // client showed the loaded file (e.g. 24) instead of the radio (23).
        // `self.yaesu_connected` = previous-frame value (updated further down).
        if state.yaesu_connected && !self.yaesu_connected {
            self.yaesu_mem_radio_received = false;
            // value 1 = "the server's copy is fine". The server reads the radio once
            // when the radio connects; asking it to walk 117 channels again for every
            // client that turns up repeats work whose answer has not changed, and
            // blocks the CAT link while it does. The Read radio button still sends 0
            // and forces a real read. An older server ignores the value and reads the
            // radio, which is simply today's behaviour.
            let _ = self.cmd_tx.send(Command::SetControl(ControlId::YaesuReadMemories, 1));
            log::info!("[radio0] 991A detected (yaesu_connected rising) - asked the server for its memory list");
        }
        // Dual-radio slot 1: the same Yaesu-enable also switches on the 2nd radio.
        // The client is dual-radio-aware -> sends Yaesu2Enable=1 (arrives on
        // yaesu2_addrs, gets RadioInfo + slot-1 data) and sets the muted-started
        // slot-1 volume to the UI value (unmute, build-88 lesson).
        if state.connected && self.yaesu2_enabled && !self.yaesu2_enable_sent {
            let _ = self.cmd_tx.send(Command::SetYaesu2Enable(true));
            let _ = self.cmd_tx.send(Command::SetYaesu2Volume(self.yaesu2_volume));
            // Sync local mic-gain to engine (like the 991A).
            let _ = self.cmd_tx.send(Command::SetYaesu2TxGain(self.yaesu2_mic_gain));
            // Sync client-side TX chain (compressor + AGC) radio 2 (FTX-1).
            let _ = self.cmd_tx.send(Command::SetYaesu2Compressor(self.yaesu2_compressor));
            let _ = self.cmd_tx.send(Command::SetYaesu2TxAgc(self.yaesu2_tx_agc));
            // FT0 = MAIN as active RX/TX -> audio follows A/MAIN (doesn't jump to SUB).
            let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2Button, 11));
            self.yaesu2_enable_sent = true;
        }
        // FTX-1 memory auto-read on CAT DETECTION (yaesu2_connected rising edge),
        // INDEPENDENT of audio-enable: the memory loads as soon as the radio is
        // detected. The enable block above is gated on yaesu2_enabled (audio)
        // and therefore didn't run when the FTX-1 audio is off - hence the
        // auto-read never fired. `self.yaesu2_connected` = previous-frame value
        // (updated further down), so this is a real rising edge. Deferred
        // ~1.5s so radio+server settle.
        if state.yaesu2_connected && !self.yaesu2_connected {
            // FTX-1 detected -> read the memory NOW. No client-side timer:
            // it doesn't fire while the client is idle (no repaint, audio off +
            // windows closed). The server does a whole-scan retry for the cold radio.
            self.yaesu2_mem_radio_received = false;
            let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2ReadMemories, 1));
            log::info!("[radio1] FTX-1 detected (yaesu2_connected rising) - asked the server for its memory list");
        }
        // A new session, counted rather than sensed. Everything that has to be
        // asked for again hangs off this one line now.
        //
        // It used to hang off `!state.connected`, and that is a flank: it
        // exists at an instant, it arrives through a watch channel that keeps
        // only the latest value, and this loop reads that once a frame. A
        // server that restarted and came back quickly therefore never happened
        // here - so VRX and both Yaesu slots were never asked for again, while
        // the engine's own restore (which reads its own variable and cannot
        // miss anything) brought RX1 and RX2 back. That is precisely the
        // "only RX comes back, VRX stays silent" the owner reported, and it
        // got worse rather than better when the engine side was fixed first
        // (2026-08-16).
        if state.session_generation != self.session_generation_seen {
            self.session_generation_seen = state.session_generation;
            self.vrx_state_sync_pending = true;
            self.yaesu_enable_sent = false;
            self.yaesu2_enable_sent = false;
            self.yaesu2_autoread_at = None;
            self.yaesu_state_sent = None;
            self.yaesu2_state_sent = None;
            log::info!(
                "new session {} - asking for VRX and Yaesu again",
                state.session_generation
            );
        }
        // Force HF to A (FTX-1): HF always wins the USB-audio. If HF is on B
        // while A is VHF/UHF, you'd transmit on A (FT0) but hear HF on B =
        // split. An A/B-swap (SV) brings HF to A/MAIN -> audio + TX + control
        // back together on A. (<100 MHz = HF/low incl. 6m/4m = the SDR receiver;
        // >100 MHz = VHF/UHF.) 2s cooldown prevents repeated swapping before the
        // state update is in.
        if state.yaesu2_connected {
            let a_hf = state.yaesu2_freq_a > 0 && state.yaesu2_freq_a < 100_000_000;
            let b_hf = state.yaesu2_freq_b > 0 && state.yaesu2_freq_b < 100_000_000;
            let cooldown_ok = self.yaesu2_hf_swap_at
                .map_or(true, |t| t.elapsed().as_millis() > 2000);
            if b_hf && !a_hf && cooldown_ok {
                let _ = self.cmd_tx.send(Command::SetControl(ControlId::Yaesu2SelectVfo, 2));
                self.yaesu2_hf_swap_at = Some(std::time::Instant::now());
            }
        }
        // VRX1+VRX2 state sync: when the server reconnects, push the
        // locally-persisted state for BOTH channels (enable + freq +
        // mode + volume) so the server matches what the client thinks
        // it has. Without this, a client-only restart would leave the
        // GUI showing defaults while the server still ran the previous
        // session's VRX state (or vice versa).
        if state.connected && self.vrx_state_sync_pending {
            // Re-send wideband audio toggle on (re)connect. App::new fires
            // it before server_addr is set, which drops the command -
            // server then defaults to NB until user toggles. Resending
            // here after handshake fixes the startup mismatch.
            let _ = self.cmd_tx.send(Command::SetThetisWidebandAudio(self.thetis_wideband_audio));
            // Same reason as the wideband toggle: the send at App::new is dropped
            // because server_addr is not set yet, so the server would keep its
            // default (on) while the GUI shows off.
            let _ = self.cmd_tx.send(Command::SetFullSpectrumEnabled(self.full_spectrum_enabled));
            // And the view itself. The client starts at 32x and assumed the
            // server did too; the server starts at 1x. Nothing sent it the
            // difference, because zoom and pan are only sent when they change
            // and neither had. So the server cut a window the client was not
            // drawing, and the band edges sat wrong until something forced a
            // send - which switching the full-band spectrum on happened to do,
            // permanently, which is why that looked like the cure (2026-08-14).
            //
            // Sent on every (re)connect, not once at startup: the server keeps
            // this per client and forgets it when the client goes away.
            let _ = self.cmd_tx.send(Command::SetSpectrumZoom(self.spectrum_zoom));
            let _ = self.cmd_tx.send(Command::SetSpectrumPan(self.spectrum_pan));
            self.last_sent_zoom = self.spectrum_zoom;
            self.last_sent_pan = self.spectrum_pan;
            let _ = self.cmd_tx.send(Command::SetRx2SpectrumZoom(self.rx2_spectrum_zoom));
            let _ = self.cmd_tx.send(Command::SetRx2SpectrumPan(self.rx2_spectrum_pan));
            self.rx2_last_sent_zoom = self.rx2_spectrum_zoom;
            self.rx2_last_sent_pan = self.rx2_spectrum_pan;
            let _ = self.cmd_tx.send(Command::SetVrxMode(self.vrx1_mode));
            // The server keeps the VRX window offset per client and forgets it
            // when the client goes; 0 here matches its own default.
            self.vrx1_last_sent_pan_hz = 0;
            self.vrx2_last_sent_pan_hz = 0;
            if self.vrx1_freq_hz > 0 {
                let _ = self.cmd_tx.send(Command::SetVrxFrequency(self.vrx1_freq_hz));
            }
            let _ = self.cmd_tx.send(Command::SetVrxVolume(self.vrx1_volume));
            let _ = self.cmd_tx.send(Command::SetVrxEnabled(self.vrx1_enabled));

            let _ = self.cmd_tx.send(Command::SetVrx2Mode(self.vrx2_mode));
            if self.vrx2_freq_hz > 0 {
                let _ = self.cmd_tx.send(Command::SetVrx2Frequency(self.vrx2_freq_hz));
            }
            let _ = self.cmd_tx.send(Command::SetVrx2Volume(self.vrx2_volume));
            let _ = self.cmd_tx.send(Command::SetVrx2Enabled(self.vrx2_enabled));
            let _ = self.cmd_tx.send(Command::SetVrxFilter(0, self.vrx1_filter_low_hz, self.vrx1_filter_high_hz));
            let _ = self.cmd_tx.send(Command::SetVrxFilter(1, self.vrx2_filter_low_hz, self.vrx2_filter_high_hz));
            // VRX rate-mode + SAM auto-tune (PATCH-vrx-wide-sam-ux) - resync.
            let _ = self.cmd_tx.send(Command::SetVrxRateMode(self.vrx_rate_mode));
            let _ = self.cmd_tx.send(Command::SetVrxRateMode2(self.vrx_rate_mode2));
            let _ = self.cmd_tx.send(Command::SetVrxAutoTune(0, self.vrx1_auto_tune));
            let _ = self.cmd_tx.send(Command::SetVrxAutoTune(1, self.vrx2_auto_tune));
            // Re-send high-res spectrum subscription on (re)connect — SEPARATE from the
            // VRX audio checkbox (checkbox model): gated on high_res, not on vrx_enabled.
            if self.vrx1_high_res_spectrum {
                let span_hz = (self.vrx_ddc_span_hz(VrxChannel::Vrx1) as f32 / self.vrx1_spectrum_zoom.max(1.0)) as u32;
                let span_khz = ((span_hz / 1000).max(1)) as u16;
                self.vrx1_high_res_last_span_khz = span_khz;
                let _ = self.cmd_tx.send(Command::SetVrxHighResSpectrum(0, true, span_khz));
            }
            if self.vrx2_high_res_spectrum {
                let span_hz = (self.vrx_ddc_span_hz(VrxChannel::Vrx2) as f32 / self.vrx2_spectrum_zoom.max(1.0)) as u32;
                let span_khz = ((span_hz / 1000).max(1)) as u16;
                self.vrx2_high_res_last_span_khz = span_khz;
                let _ = self.cmd_tx.send(Command::SetVrxHighResSpectrum(1, true, span_khz));
            }
            self.vrx_state_sync_pending = false;
        }
        // MEASUREMENT ONLY (2026-08-16, onderzoek reconnect). Held against what
        // the server says it has, for the two families this side owns and the
        // engine cannot see: VRX and the two Yaesu slots.
        //
        // The prediction being tested is that these go quiet together with the
        // VRX restore, because both hang on the same sampled edge - `connected`
        // arrives through a watch channel that only keeps the latest value, and
        // this loop reads it once a frame (twice a second while disconnected).
        // A `false` between two frames never existed here. Nothing is corrected;
        // that comes after this line has shown when it happens.
        if let Some(theirs) = state.server_subs {
            use sdr_remote_core::protocol::SubscriptionMask as M;
            let mut ours = M::default();
            ours.set(M::VRX1, self.vrx1_enabled);
            ours.set(M::VRX2, self.vrx2_enabled);
            ours.set(M::YAESU, self.yaesu_enabled);
            ours.set(M::YAESU2, self.yaesu2_enabled);
            const MINE: u16 = M::VRX1 | M::VRX2 | M::YAESU | M::YAESU2;
            let differ = (ours.0 ^ theirs.0) & MINE;
            if differ != self.subs_differ_seen {
                self.subs_differ_seen = differ;
                if differ == 0 {
                    log::info!("UI subscriptions agree again");
                } else {
                    log::warn!(
                        "UI subscriptions disagree on {:?}: we want {:#06x}, the server has {:#06x}",
                        M::names_of(differ), ours.0 & MINE, theirs.0 & MINE
                    );
                }
            }
        }

        // Note for whoever looks here for the zoom: the zoom flags are
        // deliberately NOT reset on a new session. Resetting them meant every
        // reconnect derived the opening zoom again and threw away whatever the
        // operator had set - and with a relay in the path, a link that drops
        // for a moment costs them their view. The width is matched once per
        // session; after that the zoom is the operator's, and a reconnect
        // re-sends it rather than replacing it (2026-08-15).
        // TL2-1 ctun-auto-recenter: snapshot previous connected-state before mutation,
        // otherwise we don't detect false->true reconnects (an older
        // `state.connected && !self.connected` check never worked because
        // self.connected would already be mutated here). Bug was latent - the
        // zoom-reset block on reconnect was probably skipped for a while.
        let was_connected = self.connected;
        self.connected = state.connected;
        self.ptt_denied = state.ptt_denied;
        self.rtt_ms = state.rtt_ms;
        self.jitter_ms = state.jitter_ms;
        self.buffer_depth = state.buffer_depth;
        self.rx_packets = state.rx_packets;
        self.yaesu_audio_packets = state.yaesu_audio_packets;
        self.yaesu_jitter_ms = state.yaesu_jitter_ms;
        self.yaesu_buffer_depth = state.yaesu_buffer_depth;
        self.yaesu2_audio_packets = state.yaesu2_audio_packets;
        self.yaesu2_jitter_ms = state.yaesu2_jitter_ms;
        self.yaesu2_buffer_depth = state.yaesu2_buffer_depth;
        self.vrx1_audio_packets = state.vrx1_audio_packets;
        self.vrx1_jitter_ms = state.vrx1_jitter_ms;
        self.vrx1_buffer_depth = state.vrx1_buffer_depth;
        self.vrx2_audio_packets = state.vrx2_audio_packets;
        self.vrx2_jitter_ms = state.vrx2_jitter_ms;
        self.vrx2_buffer_depth = state.vrx2_buffer_depth;
        self.down_kbps = state.down_kbps;
        self.up_kbps = state.up_kbps;
        self.bw_breakdown = state.bw_breakdown.clone();
        self.loss_percent = state.loss_percent;
        self.capture_level = state.capture_level;
        self.playback_level = state.playback_level;
        self.playback_level_bin_r = state.playback_level_bin_r;
        self.playback_level_rx2 = state.playback_level_rx2;
        self.playback_level_yaesu = state.playback_level_yaesu;
        self.playback_level_yaesu2 = state.playback_level_yaesu2;
        self.playback_level_vrx1 = state.playback_level_vrx1;
        self.playback_level_vrx2 = state.playback_level_vrx2;
        self.yaesu_mic_level = state.yaesu_mic_level;
        // Clear pending freq: must be at least 200ms old AND (exact match or >1s stale)
        if let Some(pf) = self.pending_freq {
            let age_ms = self.pending_freq_at.map_or(u128::MAX, |t| t.elapsed().as_millis());
            if age_ms >= 200 {
                let server_delta = (state.frequency_hz as i64 - pf as i64).unsigned_abs();
                if server_delta == 0 || age_ms > 1000 {
                    self.pending_freq = None;
                    self.pending_freq_at = None;
                    self.rx1_force_full_tuning = false;
                }
            }
        }
        // Accept server frequency only when no pending change
        if self.pending_freq.is_none() {
            self.frequency_hz = state.frequency_hz;
        }
        // UltraBeam auto-track: send SetFrequency when tracked VFO changes by >= 25 kHz
        if self.ub_auto_track && self.ub_connected {
            let (track_hz, _) = self.ub_track_vfo();
            let track_khz = (track_hz / 1000) as u16;
            let diff = (track_khz as i32 - self.ub_last_auto_khz as i32).unsigned_abs();
            if track_khz >= 1800 && track_khz <= 54000 && diff >= 25 {
                self.ub_last_auto_khz = track_khz;
                let _ = self.cmd_tx.send(Command::UbSetFrequency(track_khz, self.ub_direction));
            }
        }
        // Accept mode from server only if no recent local change
        let mode_accept = self.tci_control_changed_at
            .map_or(true, |t| t.elapsed().as_millis() > 500);
        if mode_accept && state.mode != self.mode {
            self.filter_changed_at = None; // accept new filter values on mode change
            self.mode = state.mode;
        }
        self.smeter = state.smeter;
        if state.smeter >= self.smeter_peak {
            self.smeter_peak = state.smeter;
            self.smeter_peak_time = Instant::now();
        } else if self.smeter_peak_time.elapsed().as_secs_f32() > 2.0 {
            self.smeter_peak = state.smeter;
            self.smeter_peak_time = Instant::now();
        }
        // Reset zoom on reconnect (connected false->true) or power ON
        // Span is reset to 0 so the first spectrum packet triggers zoom calculation.
        // Use `was_connected` snapshot - see comment above on connected-state mutation.
        let reconnected = state.connected && !was_connected;
        if reconnected || (state.power_on && !self.power_on) {
            // The bins are cleared with the span they belong to. Setting the
            // span to 0 alone left the previous session's full-band picture in
            // place, and two decisions further down read `is_empty()` on it to
            // choose what to draw (2026-08-15).
            self.reset_view_for_new_session();
            // Reset TCI control states to defaults - server will push current values
            self.vfo_sync = false;
            self.mon_on = false;
            self.nb_enable = false;
            self.nb_level = 0;
            self.anf_on = false;
            self.rx2_nb_enable = false;
            self.rx2_nb_level = 0;
            self.rx2_anf_on = false;
            self.ddc_sample_rate_rx1 = 0;
            self.ddc_sample_rate_rx2 = 0;
            // TL2-1 ctun-auto-recenter: push allow_zoom_below_2x on every
            // (re)connect so the server-strictest-checkbox policy can be
            // computed correctly WITHOUT waiting for a manual toggle.
            let _ = self.cmd_tx.send(Command::SetControl(
                sdr_remote_core::protocol::ControlId::AllowZoomBelow2x,
                if self.allow_zoom_below_2x { 1 } else { 0 },
            ));
            // Also push S-meter source choice on (re)connect - engine state
            // and UI state must agree so the right subscription mask is sent.
            let _ = self.cmd_tx.send(Command::SetSmeterSource(self.smeter_source));
        }
        self.power_on = state.power_on;
        self.tx_profile = state.tx_profile;
        // If server sends TX profile names (TCI mode), override local config
        if !state.tx_profile_names.is_empty() {
            let server_profiles: Vec<(u8, String)> = state.tx_profile_names.iter()
                .enumerate()
                .map(|(i, n)| (i as u8, n.clone()))
                .collect();
            if server_profiles != self.tx_profiles {
                self.tx_profiles = server_profiles;
            }
        }
        self.nr_level = state.nr_level;
        self.anf_on = state.anf_on;
        self.drive_level = state.drive_level;
        if state.rx_af_gain > 0 {
            self.rx_volume = state.rx_af_gain as f32 / 100.0;
        }
        self.audio_error = state.audio_error;
        self.agc_enabled = state.agc_enabled;
        self.other_tx = state.other_tx;
        if !state.playing { self.playing = false; }
        if !state.last_recorded.is_empty() && state.last_recorded != self.last_recorded {
            self.last_recorded = state.last_recorded.clone();
            // Everything just recorded starts ticked: it was asked for a
            // moment ago, so wanting to hear it is the safe assumption.
            self.play_ticked = vec![true; self.last_recorded.len()];
        }
        self.thetis_swr_x100 = state.thetis_swr_x100;
        self.thetis_configured = state.thetis_configured;
        // Single-receiver radio (server set to 1): show RX2 + VRX2 nowhere. On the
        // true->false transition disable the subscriptions once so the server
        // sends nothing more and the server-tab rows drain. VRX1 stays (on RX1).
        let was_rx2_present = self.rx2_present;
        self.rx2_present = state.rx2_present;
        if was_rx2_present && !self.rx2_present {
            if self.rx2_enabled {
                self.rx2_enabled = false;
                // Client-initiated off (RX2 receiver gone): mark pending so the reconcile
                // below confirms it and does not hold a stale optimistic "on".
                self.rx2_enabled_pending = Some((Instant::now(), false));
                let _ = self.cmd_tx.send(Command::SetRx2Enabled(false));
            }
            if self.rx2_spectrum_enabled {
                self.rx2_spectrum_enabled = false;
                let _ = self.cmd_tx.send(Command::EnableRx2Spectrum(false));
            }
            self.rx2_popout = false;
            if self.vrx2_enabled { self.toggle_vrx_audio(VrxChannel::Vrx2); }
            if self.vrx2_high_res_spectrum { self.toggle_vrx_spectrum(VrxChannel::Vrx2); }
            self.save_full_config();
        }
        // Once user changes filter locally, client is authoritative until mode changes.
        // filter_changed_at is cleared on mode change (above), so new mode values are accepted.
        if self.filter_changed_at.is_none() {
            self.filter_low_hz = state.filter_low_hz;
            self.filter_high_hz = state.filter_high_hz;
        }
        self.thetis_starting = state.thetis_starting;
        // TCI controls - suppress server sync for 500ms after local change
        let tci_accept = self.tci_control_changed_at
            .map_or(true, |t| t.elapsed().as_millis() > 1000);
        if tci_accept {
            self.tci_control_changed_at = None;
            self.agc_mode = state.agc_mode;
            self.agc_gain = state.agc_gain;
            self.agc_auto_rx1 = state.agc_auto_rx1;
            self.agc_auto_rx2 = state.agc_auto_rx2;
            self.rit_enable = state.rit_enable;
            self.rit_offset = state.rit_offset;
            self.xit_enable = state.xit_enable;
            self.xit_offset = state.xit_offset;
            self.sql_enable = state.sql_enable;
            self.sql_level = state.sql_level;
            self.nb_enable = state.nb_enable;
            self.nb_level = state.nb_level;
            self.cw_keyer_speed = state.cw_keyer_speed;
            self.vfo_lock = state.vfo_lock;
            self.binaural = state.binaural;
            self.apf_enable = state.apf_enable;
            self.rx2_agc_mode = state.rx2_agc_mode;
            self.rx2_agc_gain = state.rx2_agc_gain;
            self.rx2_sql_enable = state.rx2_sql_enable;
            self.rx2_sql_level = state.rx2_sql_level;
            self.rx2_nb_enable = state.rx2_nb_enable;
            self.rx2_nb_level = if state.rx2_nb_enable { self.rx2_nb_level.max(1) } else { 0 };
            self.rx2_binaural = state.rx2_binaural;
            self.rx2_apf_enable = state.rx2_apf_enable;
            self.rx2_vfo_lock = state.rx2_vfo_lock;
            self.mute = state.mute;
            self.rx_mute = state.rx_mute;
            self.nf_enable = state.nf_enable;
            self.rx2_nf_enable = state.rx2_nf_enable;
            self.rx_balance = -state.rx_balance; // Negate: TCI +40=left, slider -40=left
            self.tune_drive = state.tune_drive;
            self.mon_volume = state.mon_volume;
            self.ddc_sample_rate_rx1 = state.ddc_sample_rate_rx1;
            // Match the zoom to the receiver, the moment its width is known.
            //
            // This used to wait for the first full-band spectrum row, so with
            // the full-band option off it never happened at all: the client
            // opened at a fixed 32x whatever the receiver was, and the picture
            // was wrong until the option was switched on once - which set it,
            // and left it right afterwards. That is why turning full-band on
            // and off again "cured" it, and why fixing the zoom handshake in
            // build 68 did not: this is the value, that was the agreement.
            //
            // The width is in the state already; VRX has derived its own zoom
            // from it since it was written.
            // Once per session, and only against a live link. The flag used to
            // be cleared on disconnect, which made this run off the previous
            // session's rate the moment the link dropped: the slider went to
            // the opening zoom while the picture stayed frozen where the
            // operator had left it. The flag survives a disconnect now, so this
            // fires on the first connection and not again - the guard is what
            // states that deriving a width belongs to a live session, and it is
            // what keeps a stale rate from doing it (2026-08-15).
            if Self::should_derive_opening_zoom(
                state.connected,
                self.rx_zoom_initialized,
                self.ddc_sample_rate_rx1,
            ) {
                self.adopt_opening_zoom_rx1(self.ddc_sample_rate_rx1 as u32 * 1000);
            }
            self.ddc_sample_rate_rx2 = state.ddc_sample_rate_rx2;
            // RX2 gets its width the same way, and for the same reason: its
            // rate is in the state already, so waiting for a full-band row it
            // may never receive left it on the opening 32x whatever receiver
            // it was looking at. The one difference is that RX2 was never the
            // receiver anyone watched while the picture was wrong, so it kept
            // the fault RX1 was cured of in build 79.
            if Self::should_derive_opening_zoom(
                state.connected,
                self.rx2_zoom_initialized,
                self.ddc_sample_rate_rx2,
            ) {
                self.adopt_opening_zoom_rx2(self.ddc_sample_rate_rx2 as u32 * 1000);
            }
        }
        // Spectrum
        if state.spectrum_sequence != self.last_spectrum_seq && !state.spectrum_bins.is_empty() {
            self.spectrum_bins = state.spectrum_bins;
            self.spectrum_center_hz = state.spectrum_center_hz;
            self.spectrum_span_hz = state.spectrum_span_hz;
            self.spectrum_ref_level = state.spectrum_ref_level;
            self.spectrum_db_per_unit = state.spectrum_db_per_unit;
            self.last_spectrum_seq = state.spectrum_sequence;
            // Same bins into the channel type, so RX1 carries its identity the
            // way VRX already does. Rendering still reads the fields above;
            // what moves here is the derivation (auto-ref) below.
            // Does the view the server sends match the one this client asked
            // for? It should: the server cuts `DDC / zoom`. When it does not,
            // the two are working from different numbers and every frequency,
            // marker and band edge on screen is drawn against the wrong scale.
            //
            // This is checked rather than assumed because the setting is sent
            // once, on connect, and `set_spectrum_zoom` on the server drops it
            // without a word if the session is not in its map yet. Whether it
            // lands is a matter of timing - which is why the same build was
            // sometimes right and sometimes wrong, and why anything that forced
            // a resend appeared to cure it (2026-08-14).
            //
            // Detecting it needs nothing that is not already here, and unlike a
            // better-timed send it cannot lose a race.
            //
            // When to speak lives in `view_mismatch_worth_reporting`, where it
            // can be tested one condition at a time - the two mistakes this
            // check made in the field were both a missing condition, and both
            // would have been caught by a test that could not be written while
            // it sat here (2026-08-15).
            let ddc_span_hz = self.ddc_sample_rate_rx1 as f64 * 1000.0;
            if ddc_span_hz > 0.0 && self.spectrum_span_hz > 0 && self.spectrum_zoom >= 1.0 {
                let expected = ddc_span_hz / self.spectrum_zoom as f64;
                let got = self.spectrum_span_hz as f64;
                let since_report = self
                    .view_mismatch_at
                    .map_or(u128::MAX, |t| t.elapsed().as_millis());
                let since_send = self
                    .view_sent_at
                    .map_or(u128::MAX, |t| t.elapsed().as_millis());
                if Self::view_mismatch_worth_reporting(
                    expected,
                    got,
                    since_report,
                    since_send,
                    self.zoom_pan_changed_at.is_some(),
                ) {
                    self.view_mismatch_at = Some(Instant::now());
                    log::warn!(
                        "RX1 view disagrees: asked for {:.0} Hz at {:.1}x, receiving {} Hz - sending zoom and pan again",
                        expected, self.spectrum_zoom, self.spectrum_span_hz
                    );
                    let _ = self.cmd_tx.send(Command::SetSpectrumZoom(self.spectrum_zoom));
                    let _ = self.cmd_tx.send(Command::SetSpectrumPan(self.spectrum_pan));
                    self.last_sent_zoom = self.spectrum_zoom;
                    self.last_sent_pan = self.spectrum_pan;
                    self.view_sent_at = Some(Instant::now());
                }
            }
            self.rx1_spectrum.ingest(SpectrumSnapshot {
                channel: ChannelId::Rx1,
                bins: self.spectrum_bins.clone(),
                center_hz: self.spectrum_center_hz,
                span_hz: self.spectrum_span_hz,
                sequence: state.spectrum_sequence,
            });
        }
        if state.rx2_spectrum_sequence != self.rx2_spectrum.sequence()
            && !state.rx2_spectrum_bins.is_empty()
        {
            self.rx2_spectrum.ingest(SpectrumSnapshot {
                channel: ChannelId::Rx2,
                bins: state.rx2_spectrum_bins.clone(),
                center_hz: state.rx2_spectrum_center_hz,
                span_hz: state.rx2_spectrum_span_hz,
                sequence: state.rx2_spectrum_sequence,
            });
        }
        // Full-band backdrop for the VRX waterfalls. VRX1 lives in the RX1 DDC and
        // VRX2 in the RX2 DDC, and that is exactly the row the RX window already
        // receives - the same bytes, routed to a second window instead of asked
        // for twice. Set before ingest, so the row that arrives in this frame is
        // the one laid under this frame's view.
        if state.full_spectrum_enabled {
            if !state.full_spectrum_bins.is_empty() {
                self.vrx1_spectrum.set_backdrop(
                    &state.full_spectrum_bins,
                    state.full_spectrum_center_hz,
                    state.full_spectrum_span_hz,
                );
            }
            if !state.rx2_full_spectrum_bins.is_empty() {
                self.vrx2_spectrum.set_backdrop(
                    &state.rx2_full_spectrum_bins,
                    state.rx2_full_spectrum_center_hz,
                    state.rx2_full_spectrum_span_hz,
                );
            }
        } else {
            self.vrx1_spectrum.clear_backdrop();
            self.vrx2_spectrum.clear_backdrop();
        }
        // VRX's own extracted spectrum → typed input of the channel type.
        // The ChannelSpectrum pushes into its own waterfall itself.
        if state.vrx1_extracted_sequence != self.vrx1_spectrum.sequence()
            && !state.vrx1_extracted_bins.is_empty()
        {
            self.vrx1_spectrum.ingest(SpectrumSnapshot {
                channel: ChannelId::Vrx1,
                bins: state.vrx1_extracted_bins.clone(),
                center_hz: state.vrx1_extracted_center_hz,
                span_hz: state.vrx1_extracted_span_hz,
                sequence: state.vrx1_extracted_sequence,
            });
        }
        if state.vrx2_extracted_sequence != self.vrx2_spectrum.sequence()
            && !state.vrx2_extracted_bins.is_empty()
        {
            self.vrx2_spectrum.ingest(SpectrumSnapshot {
                channel: ChannelId::Vrx2,
                bins: state.vrx2_extracted_bins.clone(),
                center_hz: state.vrx2_extracted_center_hz,
                span_hz: state.vrx2_extracted_span_hz,
                sequence: state.vrx2_extracted_sequence,
            });
        }
        // SAM auto-tune: mirror the server-followed carrier freq onto the VRX
        // VFO display (display-only; we do NOT echo it back as a tune command,
        // so there's no feedback loop with the server's AFC).
        if state.vrx1_autotune_freq_hz != 0
            && state.vrx1_autotune_freq_hz != self.last_vrx1_autotune_hz
        {
            self.last_vrx1_autotune_hz = state.vrx1_autotune_freq_hz;
            self.vrx1_freq_hz = state.vrx1_autotune_freq_hz;
        }
        if state.vrx2_autotune_freq_hz != 0
            && state.vrx2_autotune_freq_hz != self.last_vrx2_autotune_hz
        {
            self.last_vrx2_autotune_hz = state.vrx2_autotune_freq_hz;
            self.vrx2_freq_hz = state.vrx2_autotune_freq_hz;
        }
        // TX modulation filter: mirror server-reported support + value. Seed the
        // editable fields once from the first push so they show the real band.
        self.tx_filter_supported = state.tx_filter_supported;
        if state.tx_filter_supported && !self.tx_filter_initialized {
            self.tx_filter_initialized = true;
            self.tx_filter_low_hz = state.tx_filter_low_hz;
            self.tx_filter_high_hz = state.tx_filter_high_hz;
        }
        // Follow-RX: while enabled, keep TX = RX filter. The TX filter is a
        // POSITIVE audio passband (Thetis applies the sideband per mode);
        // `rx_to_tx_band` converts the RX edges, incl. the straddle-zero case
        // for AM/SAM/FM so a symmetric band doesn't collapse to zero width.
        // Rate-limited to ~7/s so a spectrum drag doesn't spam Thetis with
        // tx_filter_band_ex; the dedupe still guarantees the final value lands.
        if self.tx_filter_follow_rx && self.tx_filter_supported {
            let cur = (self.filter_low_hz, self.filter_high_hz);
            if self.last_tx_follow_sent != Some(cur) {
                let ready = self.tx_follow_last_send_at
                    .map_or(true, |t| t.elapsed().as_millis() >= 150);
                if ready {
                    self.last_tx_follow_sent = Some(cur);
                    self.tx_follow_last_send_at = Some(Instant::now());
                    let (tlo, thi) = rx_to_tx_band(cur.0, cur.1);
                    let _ = self.cmd_tx.send(Command::SetTxFilter(tlo, thi));
                }
            }
        }
        if state.full_spectrum_sequence != self.full_spectrum_sequence && !state.full_spectrum_bins.is_empty() {
            // Adjust default zoom when span first becomes known (0 -> real value)
            let old_span = self.full_spectrum_span_hz;
            self.full_spectrum_bins = state.full_spectrum_bins;
            self.full_spectrum_center_hz = state.full_spectrum_center_hz;
            self.full_spectrum_span_hz = state.full_spectrum_span_hz;
            self.full_spectrum_sequence = state.full_spectrum_sequence;
            if old_span == 0 && self.full_spectrum_span_hz > 0 && !self.rx_zoom_initialized {
                // The fallback for a session that never learns the DDC rate.
                // Same derivation, same method - see `adopt_opening_zoom_rx1`.
                self.adopt_opening_zoom_rx1(self.full_spectrum_span_hz);
            }
        }

        // Per-VRX auto-ref from the channel's OWN bins (§6.1 #3), not from RX.
        if let Some(ref_db) =
            self.vrx1_spectrum
                .update_auto_ref(self.vrx1_auto_ref, self.vrx1_freq_hz, -3_000, 3_000, self.vrx1_range_db)
        {
            self.vrx1_ref_db = ref_db;
        }
        if let Some(ref_db) =
            self.vrx2_spectrum
                .update_auto_ref(self.vrx2_auto_ref, self.vrx2_freq_hz, -3_000, 3_000, self.vrx2_range_db)
        {
            self.vrx2_ref_db = ref_db;
        }

        // Per-VRX S-meter from the channel's OWN spectrum bins (§6.1 #2). The s-meter belongs to
        // the SPECTRUM, not to the audio checkbox: it is active as soon as the spectrum is on
        // (bins flowing), even with VRX audio off. Cal-offset (VRX1=10, VRX2=5)
        // empirically matches the main RX meter.
        self.vrx1_spectrum.update_smeter(
            self.vrx1_high_res_spectrum,
            self.vrx1_freq_hz,
            self.vrx1_filter_low_hz,
            self.vrx1_filter_high_hz,
            10.0,
        );
        self.vrx2_spectrum.update_smeter(
            self.vrx2_high_res_spectrum,
            self.vrx2_freq_hz,
            self.vrx2_filter_low_hz,
            self.vrx2_filter_high_hz,
            5.0,
        );
        // High-res VRX spectrum: when toggle on AND VRX enabled, re-send
        // the span to the server whenever the visible window changes
        // (driven by zoom). VRX Enable=OFF kills the high-res stream so
        // a disabled VRX channel costs zero bandwidth.
        // Reference width is the RX-independent DDC span (§6.2, #6): no
        // longer gated on `full_spectrum_span_hz > 0`, so zoom works even with the
        // RX spectrum off. Only send when connected.
        // Spectrum-zoom resend SEPARATE from the VRX audio checkbox (gated on high_res).
        if self.connected && self.vrx1_high_res_spectrum {
            let span_hz = (self.vrx_ddc_span_hz(VrxChannel::Vrx1) as f32 / self.vrx1_spectrum_zoom.max(1.0)) as u32;
            let span_khz = ((span_hz / 1000).max(1)) as u16;
            if span_khz != self.vrx1_high_res_last_span_khz {
                self.vrx1_high_res_last_span_khz = span_khz;
                let _ = self.cmd_tx.send(Command::SetVrxHighResSpectrum(0, true, span_khz));
            }
        }
        if self.connected && self.vrx2_high_res_spectrum {
            let span_hz = (self.vrx_ddc_span_hz(VrxChannel::Vrx2) as f32 / self.vrx2_spectrum_zoom.max(1.0)) as u32;
            let span_khz = ((span_hz / 1000).max(1)) as u16;
            if span_khz != self.vrx2_high_res_last_span_khz {
                self.vrx2_high_res_last_span_khz = span_khz;
                let _ = self.cmd_tx.send(Command::SetVrxHighResSpectrum(1, true, span_khz));
            }
        }
        // Where each VRX window should sit. Sent whenever it moves by more
        // than a step of the wire unit, so the server cuts its window where the
        // operator is looking - without this the client can only pan inside the
        // one screen it was already sent, and barely moves.
        if self.connected {
            let step = sdr_remote_core::protocol::VRX_PAN_STEP_HZ;
            let want1 =
                (self.vrx1_pan as f64 * self.vrx_ddc_span_hz(VrxChannel::Vrx1) as f64) as i32;
            let want2 =
                (self.vrx2_pan as f64 * self.vrx_ddc_span_hz(VrxChannel::Vrx2) as f64) as i32;
            let moved = (want1 - self.vrx1_last_sent_pan_hz).abs() >= step
                || (want2 - self.vrx2_last_sent_pan_hz).abs() >= step;
            if moved && self.vrx_pan_changed_at.is_none() {
                self.vrx_pan_changed_at = Some(Instant::now());
            }
            // Wait for the drag to settle, the way the main spectrum's zoom and
            // pan already do. Ten hertz is a fine step and a mouse crosses many
            // of them per second: sending on every one was a packet per frame
            // while dragging, per channel. The end of the gesture is what the
            // server needs, not every position along the way.
            if let Some(at) = self.vrx_pan_changed_at {
                if at.elapsed().as_millis() >= 100 {
                    self.vrx_pan_changed_at = None;
                    if (want1 - self.vrx1_last_sent_pan_hz).abs() >= step {
                        self.vrx1_last_sent_pan_hz = want1;
                        let _ = self.cmd_tx.send(Command::SetVrxSpectrumPan(0, want1));
                    }
                    if (want2 - self.vrx2_last_sent_pan_hz).abs() >= step {
                        self.vrx2_last_sent_pan_hz = want2;
                        let _ = self.cmd_tx.send(Command::SetVrxSpectrumPan(1, want2));
                    }
                }
            }
        }

        // (pending_freq already cleared above, before frequency acceptance)

        // Delayed auto_ref restore after TX->RX transition
        if let Some(at) = self.tx_spectrum_restore_auto_at {
            if Instant::now() >= at {
                if let Some(saved) = self.tx_spectrum_saved_auto_ref.take() {
                    self.auto_ref_enabled = saved;
                    if saved {
                        // Converge fast again after TX, as before - the state
                        // that decides that now lives in the channel.
                        self.rx1_spectrum.reset_auto_ref();
                    }
                }
                self.tx_spectrum_restore_auto_at = None;
            }
        }

        // Auto ref level from the noise floor, excluding the RX filter passband.
        // Shared with RX2 and both VRX channels via ChannelSpectrum - this used
        // to be four copies of the same loop, one per channel.
        if let Some(v) = self.rx1_spectrum.update_auto_ref(
            self.auto_ref_enabled,
            self.frequency_hz,
            self.filter_low_hz,
            self.filter_high_hz,
            self.spectrum_range_db,
        ) {
            self.spectrum_ref_db = v;
        }

        // Auto ref for RX2 - same derivation, own channel. RX2 falls back to
        // RX1's filter edges when it has none of its own; that choice stays
        // here, at the call site, instead of inside the shared code.
        {
            let (lo, hi) = if self.rx2_filter_low_hz != 0 || self.rx2_filter_high_hz != 0 {
                (self.rx2_filter_low_hz, self.rx2_filter_high_hz)
            } else {
                (self.filter_low_hz, self.filter_high_hz)
            };
            if let Some(v) = self.rx2_spectrum.update_auto_ref(
                self.rx2_auto_ref_enabled,
                self.rx2_frequency_hz,
                lo,
                hi,
                self.rx2_spectrum_range_db,
            ) {
                self.rx2_spectrum_ref_db = v;
            }
        }

        // Per-band WF contrast tracking
        let new_band = freq_to_band(self.frequency_hz);
        if new_band != self.current_band {
            // Save current contrast for old band
            if let Some(ref old) = self.current_band {
                self.wf_contrast_per_band.insert(old.clone(), self.waterfall_contrast);
            }
            // Load contrast for new band (or default 1.2)
            if let Some(ref nb) = new_band {
                self.waterfall_contrast = self.wf_contrast_per_band.get(nb).copied().unwrap_or(1.2);
            }
            // Reset auto-ref to fast convergence on band change
            if self.auto_ref_enabled {
                self.rx1_spectrum.reset_auto_ref();
            }
            self.current_band = new_band;
        }

        // Amplitec state
        let old_a = self.amplitec_switch_a;
        let old_b = self.amplitec_switch_b;
        let was_connected = self.amplitec_connected;
        self.amplitec_available = state.amplitec_available;
        self.amplitec_connected = state.amplitec_connected;
        self.amplitec_switch_a = state.amplitec_switch_a;
        self.amplitec_switch_b = state.amplitec_switch_b;
        if !state.amplitec_labels.is_empty() {
            self.amplitec_labels = state.amplitec_labels;
        }
        // Power-cap table: read-only mirror of the server config
        // (editing happens server-side in the Amplitec window).
        self.amplitec_power_max_w = state.amplitec_power_max_w;
        self.amplitec_power_tx_blocked = state.amplitec_power_tx_blocked;
        self.amplitec_power_loaded = state.amplitec_power_loaded;
        // Log changes
        let now = chrono_time();
        if state.amplitec_connected && !was_connected {
            self.amplitec_log_push(&now, "Connected");
        } else if !state.amplitec_connected && was_connected {
            self.amplitec_log_push(&now, "Disconnected");
        }
        if state.amplitec_switch_a != old_a && state.amplitec_switch_a > 0 {
            let label = self.amplitec_label_a(state.amplitec_switch_a);
            self.amplitec_log_push(&now, &format!("Switch A -> {} ({})", state.amplitec_switch_a, label));
        }
        if state.amplitec_switch_b != old_b && state.amplitec_switch_b > 0 {
            let label = self.amplitec_label_b(state.amplitec_switch_b);
            self.amplitec_log_push(&now, &format!("Switch B -> {} ({})", state.amplitec_switch_b, label));
        }

        // Tuner state
        let old_tuner_state = self.tuner_state;
        self.tuner_available = state.tuner_available;
        self.tuner_connected = state.tuner_connected;
        self.tuner_state = state.tuner_state;
        self.tuner_can_tune = state.tuner_can_tune;
        // Track tune frequency: on real tune (TUNING -> DONE_OK/DONE_ASSUMED) or first
        // done-state after connect (tune_freq still 0). Ignores the fake
        // IDLE -> done-state transitions from the server's stale override.
        let tuner_done = state.tuner_state == 2 || state.tuner_state == 5;
        if tuner_done && (old_tuner_state == 1 || self.tuner_tune_freq == 0) {
            self.tuner_tune_freq = self.frequency_hz;
        }

        // SPE Expert state
        self.spe_connected = state.spe_connected;
        self.spe_state = state.spe_state;
        self.spe_band = state.spe_band;
        self.spe_ptt = state.spe_ptt;
        self.spe_power_w = state.spe_power_w;
        self.spe_swr_x10 = state.spe_swr_x10;
        self.spe_temp = state.spe_temp;
        self.spe_warning = state.spe_warning;
        self.spe_alarm = state.spe_alarm;
        self.spe_power_level = state.spe_power_level;
        self.spe_antenna = state.spe_antenna;
        self.spe_input = state.spe_input;
        self.spe_voltage_x10 = state.spe_voltage_x10;
        self.spe_current_x10 = state.spe_current_x10;
        self.spe_atu_bypassed = state.spe_atu_bypassed;
        self.spe_available = state.spe_available;
        self.spe_active = state.spe_active;

        // RF2K-S Amplifier state
        self.rf2k_connected = state.rf2k_connected;
        self.rf2k_operate = state.rf2k_operate;
        self.rf2k_band = state.rf2k_band;
        self.rf2k_frequency_khz = state.rf2k_frequency_khz;
        self.rf2k_temperature_x10 = state.rf2k_temperature_x10;
        self.rf2k_voltage_x10 = state.rf2k_voltage_x10;
        self.rf2k_current_x10 = state.rf2k_current_x10;
        self.rf2k_forward_w = state.rf2k_forward_w;
        self.rf2k_reflected_w = state.rf2k_reflected_w;
        self.rf2k_swr_x100 = state.rf2k_swr_x100;
        self.rf2k_max_forward_w = state.rf2k_max_forward_w;
        self.rf2k_max_reflected_w = state.rf2k_max_reflected_w;
        self.rf2k_max_swr_x100 = state.rf2k_max_swr_x100;
        self.rf2k_error_state = state.rf2k_error_state;
        self.rf2k_error_text = state.rf2k_error_text.clone();
        self.rf2k_antenna_type = state.rf2k_antenna_type;
        self.rf2k_antenna_number = state.rf2k_antenna_number;
        self.rf2k_tuner_mode = state.rf2k_tuner_mode;
        self.rf2k_tuner_setup = state.rf2k_tuner_setup.clone();
        self.rf2k_tuner_l_nh = state.rf2k_tuner_l_nh;
        self.rf2k_tuner_c_pf = state.rf2k_tuner_c_pf;
        self.rf2k_drive_w = state.rf2k_drive_w;
        self.rf2k_modulation = state.rf2k_modulation.clone();
        self.rf2k_max_power_w = state.rf2k_max_power_w;
        self.rf2k_device_name = state.rf2k_device_name.clone();
        self.rf2k_available = state.rf2k_available;
        self.rf2k_active = state.rf2k_active;
        // Debug (Fase D)
        self.rf2k_debug_available = state.rf2k_debug_available;
        self.rf2k_bias_pct_x10 = state.rf2k_bias_pct_x10;
        self.rf2k_psu_source = state.rf2k_psu_source;
        self.rf2k_uptime_s = state.rf2k_uptime_s;
        self.rf2k_tx_time_s = state.rf2k_tx_time_s;
        self.rf2k_error_count = state.rf2k_error_count;
        self.rf2k_error_history = state.rf2k_error_history.clone();
        self.rf2k_storage_bank = state.rf2k_storage_bank;
        self.rf2k_hw_revision = state.rf2k_hw_revision.clone();
        self.rf2k_frq_delay = state.rf2k_frq_delay;
        self.rf2k_autotune_threshold_x10 = state.rf2k_autotune_threshold_x10;
        self.rf2k_dac_alc = state.rf2k_dac_alc;
        self.rf2k_high_power = state.rf2k_high_power;
        self.rf2k_tuner_6m = state.rf2k_tuner_6m;
        self.rf2k_band_gap_allowed = state.rf2k_band_gap_allowed;
        self.rf2k_controller_version = state.rf2k_controller_version;
        self.rf2k_drive_config_ssb = state.rf2k_drive_config_ssb;
        self.rf2k_drive_config_am = state.rf2k_drive_config_am;
        self.rf2k_drive_config_cont = state.rf2k_drive_config_cont;

        // UltraBeam
        self.ub_connected = state.ub_connected;
        self.ub_frequency_khz = state.ub_frequency_khz;
        self.ub_band = state.ub_band;
        self.ub_direction = state.ub_direction;
        self.ub_off_state = state.ub_off_state;
        self.ub_motors_moving = state.ub_motors_moving;
        self.ub_motor_completion = state.ub_motor_completion;
        self.ub_fw_major = state.ub_fw_major;
        self.ub_fw_minor = state.ub_fw_minor;
        self.ub_available = state.ub_available;
        self.ub_elements_mm = state.ub_elements_mm;
        self.ub_operation = state.ub_operation;
        self.ub_freq_min_mhz = state.ub_freq_min_mhz;
        self.ub_freq_max_mhz = state.ub_freq_max_mhz;

        // Rotor
        self.rotor_connected = state.rotor_connected;
        self.rotor_angle_x10 = state.rotor_angle_x10;
        self.rotor_rotating = state.rotor_rotating;
        self.rotor_target_x10 = state.rotor_target_x10;
        self.rotor_available = state.rotor_available;
        // Yaesu
        self.yaesu_connected = state.yaesu_connected;
        self.yaesu_port_trouble = state.yaesu_port_trouble;
        self.yaesu2_port_trouble = state.yaesu2_port_trouble;
        // Optimistic display presence: while actually connected the server is the
        // authority (prunes a radio that is gone); when NOT connected we hold the
        // last-known value so it seeds the pre-connect display next session (see the
        // yaesu_present_last field + config). Persisted via save_full_config.
        if state.connected {
            self.yaesu_present_last = state.yaesu_connected;
            self.yaesu2_present_last = state.yaesu2_connected;
        }
        Self::accept_yaesu_freq(
            &mut self.yaesu_freq_a,
            &mut self.yaesu_pending_freq,
            &mut self.yaesu_pending_freq_at,
            state.yaesu_freq_a,
        );
        self.yaesu_freq_b = state.yaesu_freq_b;
        self.yaesu_mode = state.yaesu_mode;
        self.yaesu_smeter = state.yaesu_smeter;
        if state.yaesu_smeter >= self.yaesu_smeter_peak {
            self.yaesu_smeter_peak = state.yaesu_smeter;
            self.yaesu_smeter_peak_time = Instant::now();
        } else if self.yaesu_smeter_peak_time.elapsed().as_secs_f32() > 2.0 {
            self.yaesu_smeter_peak = state.yaesu_smeter;
            self.yaesu_smeter_peak_time = Instant::now();
        }
        {
            let was_tx = self.yaesu_tx_active;
            self.yaesu_tx_active = state.yaesu_tx_active;
            if was_tx && !self.yaesu_tx_active && self.yaesu_mouse_ptt {
                self.release_ptt_latch(0);
            }
        }
        self.yaesu_power_on = state.yaesu_power_on;
        // Dual-radio slot 1
        self.yaesu_model = state.yaesu_model;
        self.yaesu2_model = state.yaesu2_model;
        self.yaesu2_connected = state.yaesu2_connected;
        Self::accept_yaesu_freq(
            &mut self.yaesu2_freq_a,
            &mut self.yaesu2_pending_freq,
            &mut self.yaesu2_pending_freq_at,
            state.yaesu2_freq_a,
        );
        self.yaesu2_freq_b = state.yaesu2_freq_b;
        self.yaesu2_mode = state.yaesu2_mode;
        self.yaesu2_smeter = state.yaesu2_smeter;
        if state.yaesu2_smeter >= self.yaesu2_smeter_peak {
            self.yaesu2_smeter_peak = state.yaesu2_smeter;
            self.yaesu2_smeter_peak_time = Instant::now();
        } else if self.yaesu2_smeter_peak_time.elapsed().as_secs_f32() > 2.0 {
            self.yaesu2_smeter_peak = state.yaesu2_smeter;
            self.yaesu2_smeter_peak_time = Instant::now();
        }
        {
            let was_tx = self.yaesu2_tx_active;
            self.yaesu2_tx_active = state.yaesu2_tx_active;
            if was_tx && !self.yaesu2_tx_active && self.yaesu2_mouse_ptt {
                self.release_ptt_latch(1);
            }
        }
        self.yaesu2_power_on = state.yaesu2_power_on;
        self.yaesu2_split = state.yaesu2_split;
        self.yaesu2_scan = state.yaesu2_scan;
        self.yaesu2_tuner_state = state.yaesu2_tuner_state;
        self.yaesu2_hi_swr = state.yaesu2_hi_swr;
        self.yaesu2_feature_levels = state.yaesu2_feature_levels;
        self.yaesu2_clar_offset = state.yaesu2_feature_freqs[3] as i16; // clarifier-offset (§15)
        // yaesu2_feature_toggles is synced behind the debounce (optimistic toggles).
        self.yaesu2_vfo_select = state.yaesu2_vfo_select;
        self.yaesu2_memory_channel = state.yaesu2_memory_channel;
        // Slider-sync with debounce (1s after local change), like slot 0.
        let yaesu2_accept = self.yaesu2_control_changed_at
            .map_or(true, |t| t.elapsed().as_millis() > 1000);
        if state.yaesu2_connected && yaesu2_accept {
            self.yaesu2_control_changed_at = None;
            self.yaesu2_squelch = state.yaesu2_squelch as u16;
            self.yaesu2_rf_gain = state.yaesu2_rf_gain as u16;
            match self.yaesu2_power_pending {
                Some(p) if state.yaesu2_tx_power as u16 == p => {
                    self.yaesu2_rf_power = p; self.yaesu2_power_pending = None;
                }
                Some(_) if self.yaesu2_power_pending_at.map_or(true, |t| t.elapsed().as_millis() >= 3000) => {
                    self.yaesu2_rf_power = state.yaesu2_tx_power as u16; self.yaesu2_power_pending = None;
                }
                Some(_) => {}
                None => { self.yaesu2_rf_power = state.yaesu2_tx_power as u16; }
            }
            for j in 0..4 { self.yaesu_level_sliders[1][j] = state.yaesu2_feature_levels[8 + j] as i32; }
            for j in 0..3 { self.yaesu_freq_sliders[1][j] = state.yaesu2_feature_freqs[j] as i32; }
            self.yaesu2_feature_toggles = state.yaesu2_feature_toggles;
        }
        // Sync slider values from radio - debounce 1s after local change
        let yaesu_accept = self.yaesu_control_changed_at
            .map_or(true, |t| t.elapsed().as_millis() > 1000);
        if state.yaesu_connected && yaesu_accept {
            self.yaesu_control_changed_at = None;
            self.yaesu_squelch = state.yaesu_squelch as u16;
            self.yaesu_rf_gain = state.yaesu_rf_gain as u16;
            // Power: only accept the readback once the radio confirms OUR last-sent
            // value (or after a 3s timeout) - otherwise the slow 991A-
            // PC readback bounces the slider back and forth.
            match self.yaesu_power_pending {
                Some(p) if state.yaesu_tx_power as u16 == p => {
                    self.yaesu_rf_power = p; self.yaesu_power_pending = None;
                }
                Some(_) if self.yaesu_power_pending_at.map_or(true, |t| t.elapsed().as_millis() >= 3000) => {
                    self.yaesu_rf_power = state.yaesu_tx_power as u16; self.yaesu_power_pending = None;
                }
                Some(_) => {} // wait for confirmation, keep the local slider value
                None => { self.yaesu_rf_power = state.yaesu_tx_power as u16; }
            }
            for j in 0..4 { self.yaesu_level_sliders[0][j] = state.yaesu_feature_levels[8 + j] as i32; }
            for j in 0..3 { self.yaesu_freq_sliders[0][j] = state.yaesu_feature_freqs[j] as i32; }
            self.yaesu_feature_toggles = state.yaesu_feature_toggles;
        }
        // Max-power follows the band directly (not debounced; is not a slider value).
        self.yaesu_tx_power_max = state.yaesu_tx_power_max as u16;
        self.yaesu2_tx_power_max = state.yaesu2_tx_power_max as u16;
        self.yaesu_split_active = state.yaesu_split;
        self.yaesu_scan_active = state.yaesu_scan;
        self.yaesu_tuner_state = state.yaesu_tuner_state;
        self.yaesu_hi_swr = state.yaesu_hi_swr;
        self.yaesu_feature_levels = state.yaesu_feature_levels;
        self.yaesu_clar_offset = state.yaesu_feature_freqs[3] as i16; // clarifier-offset (§15)
        // yaesu_feature_toggles is synced behind the debounce (optimistic toggles).
        self.yaesu_in_memory_mode = state.yaesu_vfo_select == 1 || state.yaesu_vfo_select == 2; // 1=Memory, 2=MemTune (not 3=VFO B)
        // Find the current memory channel in our loaded list
        if self.yaesu_in_memory_mode && state.yaesu_memory_channel > 0 {
            self.yaesu_current_mem_ch = self.yaesu_mem_channels.iter()
                .position(|ch| ch.channel_number == state.yaesu_memory_channel);
        }
        // The table's own marker, per radio. Kept as a channel number: the two lists
        // are different lengths, so one shared row index marked a different channel in
        // each table and moved one radio's marker when the other was tuned.
        self.yaesu_mem_active_live = self.yaesu_in_memory_mode;
        if self.yaesu_in_memory_mode && state.yaesu_memory_channel > 0 {
            self.yaesu_mem_active_ch = Some(state.yaesu_memory_channel);
        }
        let slot1_in_memory = state.yaesu2_vfo_select == 1 || state.yaesu2_vfo_select == 2;
        self.yaesu2_mem_active_live = slot1_in_memory;
        if slot1_in_memory && state.yaesu2_memory_channel > 0 {
            self.yaesu2_mem_active_ch = Some(state.yaesu2_memory_channel);
        }
        // Incoming Yaesu EX settings. Own field, own comparison - the memory list
        // below is a separate stream that arrives in the same instant on connect.
        if let Some(ref text) = state.yaesu_menu_data {
            // Content-compared, like the memory list: the EX values are pushed now
            // (once per subscriber, then on change), so a later push must be accepted
            // while an unchanged repeat must not re-parse every frame.
            if self.yaesu_menu_blob_hash != Some(blob_hash(text)) {
                self.yaesu_menu_blob_hash = Some(blob_hash(text));
                self.yaesu_menu_received = true;
                let menu_text = text.strip_prefix("MENU:").unwrap_or(text);
                let mut items = Vec::new();
                for line in menu_text.lines() {
                    if let Some((num_str, val)) = line.split_once(':') {
                        if let Ok(num) = num_str.trim().parse::<u16>() {
                            items.push(yaesu_menu::MenuItem { number: num, raw_value: val.to_string() });
                        }
                    }
                }
                log::info!("Received {} menu items from radio", items.len());
                self.yaesu_menu_items = items;
            }
        } else {
            self.yaesu_menu_received = false;
            // The hash deliberately stays. The engine holds a blob for half a
            // second and then drops it, so this branch runs between every push -
            // and clearing the hash here made the NEXT identical push look new.
            // The result was the same list parsed again every twenty seconds and
            // eight identical lines in the log each time, which is three quarters
            // of a quiet session and three quarters of what a problem report
            // carries. A changed blob still differs from this hash and is still
            // accepted; nothing is lost by remembering (2026-08-16).
        }
        // Incoming Yaesu memory list.
        //
        // An open edit wins. The list is pushed now - on connect, on change, and
        // through the slow safety net - so it arrives while the operator is typing a
        // tone into the table. Applying it then throws that away, and the value that
        // gets written to the radio a moment later is the old one. The hash is
        // deliberately NOT updated, so the next push after the edit is saved or
        // written does land. (Design note §2.4: a push must not overwrite unsaved
        // work in the client.)
        if let Some(ref text) = state.yaesu_memory_data {
            // Held back while there are UNSAVED EDITS, and only then.
            //
            // It used to be held back while the table was merely open as well,
            // on the reasoning that a list reordering itself under your hands is
            // unwelcome. The cost of that turned out to be worse: somebody
            // watching the table is usually watching it because they want to see
            // what the server now holds, and the update never arrived at all
            // while they looked. An FTX-1 tone restored on the server sat there
            // for minutes while the client showed the old list, with the log
            // saying "held back: the table is open" over and over (2026-08-12).
            // Unsaved work is worth protecting; a view of reality is not worth
            // withholding. Never held back when there is nothing to show yet -
            // the first list must always arrive - and never for an answer the
            // operator asked for, which IS the action they just took.
            let busy = !self.yaesu_mem_expect_push
                && !self.yaesu_mem_channels.is_empty()
                && self.yaesu_mem_dirty;
            if busy && self.yaesu_mem_blob_hash != Some(blob_hash(text)) {
                if !self.yaesu_mem_push_deferred {
                    self.yaesu_mem_push_deferred = true;
                    log::info!(
                        "Yaesu memory list from the server held back: the table has unsaved edits"
                    );
                }
            } else if self.yaesu_mem_blob_hash != Some(blob_hash(text)) {
                self.yaesu_mem_push_deferred = false;
                self.yaesu_mem_expect_push = false;
                // Memory channel data. Compared on CONTENT, not on a one-shot latch:
                // the server pushes this list (once per subscriber, then whenever it
                // changes), so a second push has to be accepted while an unchanged
                // repeat must not re-parse over local edits every frame.
                self.yaesu_mem_blob_hash = Some(blob_hash(text));
                self.yaesu_mem_radio_received = true;
                match crate::ui::yaesu_memory::parse_tab_string(text) {
                    Ok(mut radio_channels) => {
                        let existing = std::mem::take(&mut self.yaesu_mem_channels);
                        for rch in &mut radio_channels {
                            if rch.name.is_empty() || rch.name.starts_with("CH ") {
                                if let Some(match_ch) = existing.iter().find(|e| e.rx_freq_hz == rch.rx_freq_hz) {
                                    rch.name = match_ch.name.clone();
                                }
                            }
                        }
                        log::info!("Received {} memory channels from radio", radio_channels.len());
                        self.yaesu_mem_channels = radio_channels;
                        // NOT dirty: this list came from the radio, there is nothing
                        // unsaved about it. Marking it dirty lit the Save button
                        // unprompted and, worse, made "are there open edits?"
                        // permanently true - which is what this flag now guards.
                        self.yaesu_mem_dirty = false;
                    }
                    Err(e) => log::warn!("Parse memory data from radio: {}", e),
                }
            }
        } else {
            self.yaesu_mem_radio_received = false;
            // The hash deliberately stays. The engine holds a blob for half a
            // second and then drops it, so this branch runs between every push -
            // and clearing the hash here made the NEXT identical push look new.
            // The result was the same list parsed again every twenty seconds and
            // eight identical lines in the log each time, which is three quarters
            // of a quiet session and three quarters of what a problem report
            // carries. A changed blob still differs from this hash and is still
            // accepted; nothing is lost by remembering (2026-08-16).
        }
        // Slot-1 (FTX-1) EX values: own field, own comparison - same split as slot 0.
        if let Some(ref text) = state.yaesu2_menu_data {
            let menu_body = text.strip_prefix("MENU:").unwrap_or(text);
            if self.yaesu2_menu_blob_hash != Some(blob_hash(menu_body)) {
                self.yaesu2_menu_blob_hash = Some(blob_hash(menu_body));
                self.yaesu2_menu_received = true;
                self.yaesu2_menu_entries = menu_body.lines()
                    .filter_map(|l| l.split_once(':')
                        .map(|(a, v)| (a.trim().to_string(), v.trim().to_string())))
                    .filter(|(a, _)| a.len() == 6)
                    .collect();
                self.yaesu2_menu_edits.clear(); // fresh values -> reset edit buffers
                log::info!("[radio1] received {} EX menu values", self.yaesu2_menu_entries.len());
            }
        } else {
            self.yaesu2_menu_received = false;
            // The hash deliberately stays. The engine holds a blob for half a
            // second and then drops it, so this branch runs between every push -
            // and clearing the hash here made the NEXT identical push look new.
            // The result was the same list parsed again every twenty seconds and
            // eight identical lines in the log each time, which is three quarters
            // of a quiet session and three quarters of what a problem report
            // carries. A changed blob still differs from this hash and is still
            // accepted; nothing is lost by remembering (2026-08-16).
        }
        // Slot-1 (FTX-1) memory dump -> yaesu2_mem_channels (Phase B). Same rule as
        // slot 0: unsaved edits win over an incoming push, and nothing else does.
        // This is the radio the rule mattered most for: its tones live in the
        // server's list, so a client that refuses updates while the table is
        // open is a client that cannot show what it just asked to be stored.
        if let Some(ref text) = state.yaesu2_memory_data {
            let busy2 = !self.yaesu2_mem_expect_push
                && !self.yaesu2_mem_channels.is_empty()
                && self.yaesu2_mem_dirty;
            if busy2 && self.yaesu2_mem_blob_hash != Some(blob_hash(text)) {
                if !self.yaesu2_mem_push_deferred {
                    self.yaesu2_mem_push_deferred = true;
                    log::info!(
                        "[radio1] memory list from the server held back: the table has unsaved edits"
                    );
                }
            } else if self.yaesu2_mem_blob_hash != Some(blob_hash(text)) {
                self.yaesu2_mem_push_deferred = false;
                self.yaesu2_mem_expect_push = false;
                // Content-compared, exactly like slot 0.
                self.yaesu2_mem_blob_hash = Some(blob_hash(text));
                self.yaesu2_mem_radio_received = true;
                match crate::ui::yaesu_memory::parse_tab_string(text) {
                    Ok(mut radio_channels) => {
                        let existing = std::mem::take(&mut self.yaesu2_mem_channels);
                        if radio_channels.is_empty() {
                            // A failed / empty radio read must NEVER wipe a good list
                            // (the FTX-1 sometimes answers nothing on the first read,
                            // then the full list on a retry). Keep what we had.
                            log::info!("[radio1] memory read returned 0 channels - keeping existing {}", existing.len());
                            self.yaesu2_mem_channels = existing;
                        } else {
                            for rch in &mut radio_channels {
                                if rch.name.is_empty() || rch.name.starts_with("CH ") {
                                    if let Some(match_ch) = existing.iter().find(|e| e.rx_freq_hz == rch.rx_freq_hz) {
                                        rch.name = match_ch.name.clone();
                                    }
                                }
                            }
                            log::info!("[radio1] received {} memory channels", radio_channels.len());
                            self.yaesu2_mem_channels = radio_channels;
                            self.yaesu2_mem_dirty = false;
                        }
                    }
                    Err(e) => log::warn!("[radio1] parse memory data: {}", e),
                }
            }
        } else {
            self.yaesu2_mem_radio_received = false;
            // The hash deliberately stays. The engine holds a blob for half a
            // second and then drops it, so this branch runs between every push -
            // and clearing the hash here made the NEXT identical push look new.
            // The result was the same list parsed again every twenty seconds and
            // eight identical lines in the log each time, which is three quarters
            // of a quiet session and three quarters of what a problem report
            // carries. A changed blob still differs from this hash and is still
            // accepted; nothing is lost by remembering (2026-08-16).
        }
        self.dx_spots = state.dx_spots.clone();

        // RX2 / VFO-B
        self.dx_spots_enabled = state.dx_spots_enabled;
        // RX1/RX2 audio enable: optimistic client value, server-authoritative with a
        // grace window (see reconcile_audio_enable). Same path for both so they behave
        // identically - RX1 was client-only, RX2 was server-clobbered (the ~1 s lag).
        Self::reconcile_audio_enable(&mut self.rx1_enabled, &mut self.rx1_enabled_pending, state.rx1_enabled);
        Self::reconcile_audio_enable(&mut self.rx2_enabled, &mut self.rx2_enabled_pending, state.rx2_enabled);
        self.vfo_sync = state.vfo_sync;
        self.diversity_enabled = state.diversity_enabled;
        if state.diversity_phase != 0 {
            let decoded = (state.diversity_phase as i32 - 18000) as f32 / 100.0;
            self.diversity_phase = decoded;
        }
        if state.diversity_gain_rx1 != 0 {
            self.diversity_gain_rx1 = state.diversity_gain_rx1 as f32 / 1000.0;
        }
        if state.diversity_gain_rx2 != 0 {
            self.diversity_gain_rx2 = state.diversity_gain_rx2 as f32 / 1000.0;
        }
        if state.diversity_gain_multi != 0 {
            self.diversity_gain_multi = state.diversity_gain_multi as f32 / 100.0;
        }
        self.mon_on = state.mon_on;
        // New TCI controls: client-authoritative (no server broadcast).
        // State is only updated when Thetis pushes TCI notifications via
        // ControlPacket from server, which the engine writes into RadioState.
        // Until then, keep client-local values.
        // Clear RX2 pending freq: must be at least 200ms old AND (exact match or >1s stale)
        if let Some(pf) = self.rx2_pending_freq {
            let age_ms = self.rx2_pending_freq_at.map_or(u128::MAX, |t| t.elapsed().as_millis());
            if age_ms >= 200 {
                let server_delta = (state.frequency_rx2_hz as i64 - pf as i64).unsigned_abs();
                if server_delta == 0 || age_ms > 1000 {
                    self.rx2_pending_freq = None;
                    self.rx2_pending_freq_at = None;
                    self.rx2_force_full_tuning = false;
                }
            }
        }
        // Accept server frequency only when no pending change
        if self.rx2_pending_freq.is_none() {
            self.rx2_frequency_hz = state.frequency_rx2_hz;
        }
        if state.mode_rx2 != self.rx2_mode {
            self.rx2_filter_changed_at = None; // accept new filter values on mode change
        }
        self.rx2_mode = state.mode_rx2;
        self.rx2_smeter = state.smeter_rx2;
        if state.smeter_rx2 >= self.rx2_smeter_peak {
            self.rx2_smeter_peak = state.smeter_rx2;
            self.rx2_smeter_peak_time = Instant::now();
        } else if self.rx2_smeter_peak_time.elapsed().as_secs_f32() > 2.0 {
            self.rx2_smeter_peak = state.smeter_rx2;
            self.rx2_smeter_peak_time = Instant::now();
        }
        // Sync RX2 Vol from Thetis ZZLB (same as RX1 Vol syncs from ZZLA)
        if state.rx2_af_gain != self.rx2_af_gain_display {
            log::info!("UI: RX2 AF gain {} -> {}, slider {:.0}% -> {:.0}%",
                self.rx2_af_gain_display, state.rx2_af_gain,
                self.rx2_volume * 100.0, state.rx2_af_gain as f32);
            self.rx2_volume = state.rx2_af_gain as f32 / 100.0;
        }
        self.rx2_af_gain_display = state.rx2_af_gain;
        // Once user changes RX2 filter locally, client is authoritative until mode changes.
        if self.rx2_filter_changed_at.is_none() && (state.filter_rx2_low_hz != 0 || state.filter_rx2_high_hz != 0) {
            self.rx2_filter_low_hz = state.filter_rx2_low_hz;
            self.rx2_filter_high_hz = state.filter_rx2_high_hz;
        }
        self.rx2_nr_level = state.rx2_nr_level;
        self.rx2_anf_on = state.rx2_anf_on;
        // RX2 spectrum (view)
        if state.rx2_spectrum_sequence != self.rx2_last_spectrum_seq && !state.rx2_spectrum_bins.is_empty() {
            self.rx2_spectrum_bins = state.rx2_spectrum_bins;
            self.rx2_spectrum_center_hz = state.rx2_spectrum_center_hz;
            self.rx2_spectrum_span_hz = state.rx2_spectrum_span_hz;
            self.rx2_last_spectrum_seq = state.rx2_spectrum_sequence;
        }
        // RX2 full DDC spectrum (for waterfall)
        if state.rx2_full_spectrum_sequence != self.rx2_full_spectrum_sequence && !state.rx2_full_spectrum_bins.is_empty() {
            let old_span = self.rx2_full_spectrum_span_hz;
            self.rx2_full_spectrum_bins = state.rx2_full_spectrum_bins;
            self.rx2_full_spectrum_center_hz = state.rx2_full_spectrum_center_hz;
            self.rx2_full_spectrum_span_hz = state.rx2_full_spectrum_span_hz;
            self.rx2_full_spectrum_sequence = state.rx2_full_spectrum_sequence;
            if old_span == 0 && self.rx2_full_spectrum_span_hz > 0 && !self.rx2_zoom_initialized {
                self.adopt_opening_zoom_rx2(self.rx2_full_spectrum_span_hz);
            }
        }
        // (rx2_pending_freq already cleared above, before frequency acceptance)
    }
}
