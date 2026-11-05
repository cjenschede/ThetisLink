// SPDX-License-Identifier: GPL-2.0-or-later
//! VRX (secondary receiver) UI: the sub-receiver send helpers, the shared
//! per-channel controls renderer, the audio/spectrum toggles and the shared VRX
//! spectrum-panel renderer. VRX1 and VRX2 share exactly this code (parity by
//! construction). Extracted verbatim from
//! `ui/mod.rs` - pure relocation, no behaviour change. `pub(super)` keeps the
//! methods callable from the parent module tree (popouts/screens/update).

use super::*;
use crate::ui::controls::frequency::step_on_grid;

/// The width of one DDC bucket: a VRX keeps its last frequency per 100 kHz of
/// DDC centre, for when the operator comes back to that part of the band.
const BUCKET_HZ: u64 = 100_000;

/// A DDC centre as one VRX sees it on this frame (build 226).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DdcMove {
    pub center_hz: u64,
    pub last_center_hz: u64,
    /// Where the server lets this VRX listen around `center_hz`:
    /// `vrx_tune_limits`, the same guard as the server's (build 227, after
    /// 226 - not the half span, which reaches past it).
    pub reach_lo_hz: u64,
    pub reach_hi_hz: u64,
    /// The reach of the previous frame (build 228).
    pub last_reach_lo_hz: u64,
    pub last_reach_hi_hz: u64,
    /// Centre and span came from a spectrum packet.
    /// Without the full-band spectrum - off for a fresh install - the centre
    /// is the VFO, and with CTUN every tuning step looked like a move.
    pub measured: bool,
    /// Where the VRX listens now (0: never set).
    pub vrx_hz: u64,
    /// The VFO of the receiver the VRX sits on.
    pub vfo_hz: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VrxAfterMove {
    /// Nothing to do: the centre did not move, or moved within its bucket
    /// with the VRX still in reach.
    Unchanged,
    /// Another bucket, but the VRX is still in reach.
    Stays,
    /// Out of reach: to the frequency last used in the new bucket, or the VFO.
    To { hz: u64, saved: bool },
}

impl DdcMove {
    /// Every move of the centre is looked at (build 227): a re-centering
    /// within one bucket can take a VRX out of
    /// reach too. And every change of the reach with the centre standing still
    /// (build 228): another VRX filter, or the first spectrum packet. A VRX still in reach stays where it is (build 226); one out
    /// of reach goes to the frequency saved for the bucket, when that is in
    /// reach, or to the VFO. Into another bucket the old bucket keeps the
    /// frequency, for a later band change. A VRX never set (0) moves only at a
    /// bucket change, as before.
    pub(super) fn follow(self, saved: &mut std::collections::HashMap<u64, u64>) -> VrxAfterMove {
        let reach_same = (self.reach_lo_hz, self.reach_hi_hz) == (self.last_reach_lo_hz, self.last_reach_hi_hz);
        if self.last_center_hz == 0 || (self.center_hz == self.last_center_hz && reach_same) {
            return VrxAfterMove::Unchanged;
        }
        let cur = self.center_hz / BUCKET_HZ;
        let last = self.last_center_hz / BUCKET_HZ;
        let new_bucket = cur != last;
        // An estimated centre is looked at only at a bucket change, as up to
        // build 226 (build 229).
        if !self.measured && !new_bucket {
            return VrxAfterMove::Unchanged;
        }
        if new_bucket && self.vrx_hz > 0 {
            saved.insert(last, self.vrx_hz);
        }
        let in_reach = |f: u64| f >= self.reach_lo_hz && f <= self.reach_hi_hz;
        if self.vrx_hz > 0 && in_reach(self.vrx_hz) {
            return if new_bucket { VrxAfterMove::Stays } else { VrxAfterMove::Unchanged };
        }
        if self.vrx_hz == 0 && !new_bucket {
            return VrxAfterMove::Unchanged;
        }
        match saved.get(&cur).copied().filter(|&f| in_reach(f)) {
            Some(hz) => VrxAfterMove::To { hz, saved: true },
            None => VrxAfterMove::To { hz: self.vfo_hz, saved: false },
        }
    }
}

/// How wide the DDC of a VRX is: the rate the
/// server says first - it travels always, and the full-band row's span stays
/// behind when that row is switched off - then the row's span, then a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DdcWidth {
    /// `DdcSampleRateRx1/2`, kHz; 0 until the server said it.
    pub server_rate_khz: u16,
    /// The full-band row's span, Hz; 0 without that row.
    pub row_span_hz: u32,
    /// The full-band row is of now (`RowLive`, build 233/234): its span stays
    /// behind when it is switched off or stops coming, and then it is no source.
    pub row_on: bool,
}

impl DdcWidth {
    pub(super) fn hz(self) -> f64 {
        if self.server_rate_khz > 0 {
            self.server_rate_khz as f64 * 1000.0
        } else if self.row_on && self.row_span_hz > 0 {
            self.row_span_hz as f64
        } else {
            384_000.0
        }
    }
}

/// How long a connected server may leave a measured centre unsaid before the
/// estimate takes over: it says the centre on every change and every 10 s.
const FALLBACK_HOLD: std::time::Duration = std::time::Duration::from_secs(15);

/// A measured centre that is no longer there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FallBackInput {
    /// The previous frame's centre was measured.
    pub last_measured: bool,
    /// This frame's centre is measured.
    pub measured: bool,
    /// The client has a session.
    pub connected: bool,
    /// How long the connected server has left it unsaid (None: not counting).
    pub unsaid_for: Option<std::time::Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FallBack {
    /// Nothing to hold: follow as usual.
    Follow,
    /// A lost session, or a server that has not said it yet: no move, and the
    /// last measured centre stays.
    Hold,
    /// A connected server that stays silent (before 231, or no DDC mode): the
    /// estimate becomes the starting point, without moving the VRX.
    Rebase,
}

impl FallBackInput {
    pub(super) fn decide(self) -> FallBack {
        if !(self.last_measured && !self.measured) {
            return FallBack::Follow;
        }
        match (self.connected, self.unsaid_for) {
            (true, Some(d)) if d >= FALLBACK_HOLD => FallBack::Rebase,
            _ => FallBack::Hold,
        }
    }
}

/// Where a VRX's DDC stands, from what the client has (build 231). The DDC of
/// a VRX is its receiver's: a VRX never listens outside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DdcSources {
    /// What the server says (`DdcCentreRx1/2`); 0 from a server before 231.
    pub sent_hz: u64,
    /// The full-band spectrum's centre and span (0 without that row).
    pub full_hz: u64,
    pub full_span_hz: u64,
    /// The full-band row is of now (`RowLive`; build 233, and
    /// 234): switched off or no longer coming, its centre and
    /// span stay behind, and after a lost session that old centre passed for a
    /// measurement.
    pub row_on: bool,
    /// The receiver's VFO: the estimate when nothing else is known.
    pub vfo_hz: u64,
}

impl DdcSources {
    /// The centre, and whether it is measured. The server's word first; then
    /// the full-band row, as up to build 230; then the VFO as an estimate.
    pub(super) fn centre(self) -> (u64, bool) {
        if self.sent_hz > 0 {
            (self.sent_hz, true)
        } else if self.row_on && self.full_hz > 0 && self.full_span_hz > 0 {
            (self.full_hz, true)
        } else {
            (self.vfo_hz, false)
        }
    }
}

impl SdrRemoteApp {
    /// Where the DDC of this VRX's receiver stands, and whether that was
    /// measured (`DdcSources`). The one place the VRX follow, the VRX window and
    /// MIDI ask (build 231): with the full-band spectrum off the window took
    /// the VFO, and after a band change it held the tuning to the old band.
    pub(super) fn vrx_ddc_centre(&self, ch: VrxChannel) -> (u64, bool) {
        match ch {
            VrxChannel::Vrx1 => DdcSources {
                sent_hz: self.ddc_centre_rx1_hz,
                full_hz: self.full_spectrum_center_hz as u64,
                full_span_hz: self.full_spectrum_span_hz as u64,
                row_on: self.full_row_live(self.full_row_at),
                vfo_hz: self.frequency_hz,
            },
            VrxChannel::Vrx2 => DdcSources {
                sent_hz: self.ddc_centre_rx2_hz,
                full_hz: self.rx2_full_spectrum_center_hz as u64,
                full_span_hz: self.rx2_full_spectrum_span_hz as u64,
                row_on: self.full_row_live(self.rx2_full_row_at),
                vfo_hz: self.rx2_frequency_hz,
            },
        }
        .centre()
    }

    /// One VRX follows the DDC centre of its receiver, once per frame
    /// (`DdcMove::follow`). One rule for both VRXs: they were two copies in
    /// `update.rs` until build 226.
    pub(super) fn vrx_follow_ddc_centre(&mut self, ch: VrxChannel) {
        let (vfo, last, last_reach, vrx_hz, last_measured) = match ch {
            VrxChannel::Vrx1 => (
                self.frequency_hz, self.last_vrx1_ddc_center_hz, self.last_vrx1_reach_hz, self.vrx1_freq_hz,
                self.last_vrx1_measured,
            ),
            VrxChannel::Vrx2 => (
                self.rx2_frequency_hz, self.last_vrx2_ddc_center_hz, self.last_vrx2_reach_hz, self.vrx2_freq_hz,
                self.last_vrx2_measured,
            ),
        };
        let (center_hz, measured) = self.vrx_ddc_centre(ch);
        if center_hz == 0 {
            return;
        }
        // A measured centre falling back to the estimate is a lost session,
        // not a move: nothing happens, and the last
        // measured centre and reach stay for when the server speaks again. A
        // connected server that stays silent becomes the estimate's start.
        let connected = self.connected;
        let since = match ch {
            VrxChannel::Vrx1 => &mut self.vrx1_fallback_since,
            VrxChannel::Vrx2 => &mut self.vrx2_fallback_since,
        };
        if !(last_measured && !measured) || !connected {
            *since = None;
        } else if since.is_none() {
            *since = Some(std::time::Instant::now());
        }
        let unsaid_for = since.map(|t| t.elapsed());
        match (FallBackInput { last_measured, measured, connected, unsaid_for }).decide() {
            FallBack::Follow => {}
            FallBack::Hold => return,
            FallBack::Rebase => {
                *since = None;
                let reach = self.vrx_tune_limits(ch, center_hz);
                match ch {
                    VrxChannel::Vrx1 => {
                        self.last_vrx1_ddc_center_hz = center_hz;
                        self.last_vrx1_reach_hz = reach;
                        self.last_vrx1_measured = false;
                    }
                    VrxChannel::Vrx2 => {
                        self.last_vrx2_ddc_center_hz = center_hz;
                        self.last_vrx2_reach_hz = reach;
                        self.last_vrx2_measured = false;
                    }
                }
                return;
            }
        }
        // The server's own guard, one place.
        let (reach_lo_hz, reach_hi_hz) = self.vrx_tune_limits(ch, center_hz);
        let saved = match ch {
            VrxChannel::Vrx1 => &mut self.vrx1_freq_by_bucket,
            VrxChannel::Vrx2 => &mut self.vrx2_freq_by_bucket,
        };
        let after = DdcMove {
            center_hz,
            last_center_hz: last,
            reach_lo_hz,
            reach_hi_hz,
            last_reach_lo_hz: last_reach.0,
            last_reach_hi_hz: last_reach.1,
            measured,
            vrx_hz,
            vfo_hz: vfo,
        }
        .follow(saved);
        match after {
            VrxAfterMove::Unchanged => {}
            VrxAfterMove::Stays => log::info!(
                "{}: DDC centre to {:.3} kHz, still within reach; VRX stays on {:.3} kHz",
                ch.label(), center_hz as f64 / 1000.0, vrx_hz as f64 / 1000.0
            ),
            VrxAfterMove::To { hz, saved } => {
                // Commit only after a confirmed send (no drift).
                if self.vrx_send_freq(ch, hz) {
                    match ch {
                        VrxChannel::Vrx1 => self.vrx1_freq_hz = hz,
                        VrxChannel::Vrx2 => self.vrx2_freq_hz = hz,
                    }
                }
                let to = if saved { "saved" } else { "the VFO" };
                if center_hz == last {
                    log::info!(
                        "{}: reach now {:.3}-{:.3} kHz; VRX to {:.3} kHz ({})",
                        ch.label(), reach_lo_hz as f64 / 1000.0, reach_hi_hz as f64 / 1000.0,
                        hz as f64 / 1000.0, to
                    );
                } else {
                    log::info!(
                        "{}: DDC centre to {:.3} kHz; VRX to {:.3} kHz ({})",
                        ch.label(), center_hz as f64 / 1000.0, hz as f64 / 1000.0, to
                    );
                }
            }
        }
        match ch {
            VrxChannel::Vrx1 => {
                self.last_vrx1_ddc_center_hz = center_hz;
                self.last_vrx1_reach_hz = (reach_lo_hz, reach_hi_hz);
                self.last_vrx1_measured = measured;
            }
            VrxChannel::Vrx2 => {
                self.last_vrx2_ddc_center_hz = center_hz;
                self.last_vrx2_reach_hz = (reach_lo_hz, reach_hi_hz);
                self.last_vrx2_measured = measured;
            }
        }
    }

    /// Model B `can(VRX)`: VRX spectra are produced only from the Thetis DDC-IQ,
    /// so a VRX window can exist iff Thetis is configured. This is *availability*,
    /// not connection - `connected` gates the wire subscription + live-vs-placeholder
    /// inside an open window, never whether the window opens.
    pub(super) fn can_vrx(&self) -> bool {
        self.thetis_configured
    }

    /// Model B `can(RX2)`: the RX2 spectrum comes from the Thetis DDC and only
    /// exists when the server actually has a second receiver (`rx2_present`;
    /// single-receiver servers report it absent). Availability, not connection.
    pub(super) fn can_rx2(&self) -> bool {
        self.thetis_configured && self.rx2_present
    }

    /// Model B `can(RX1)`: the RX1 spectrum is the main Thetis DDC spectrum, so it
    /// can only be shown when Thetis is configured. (Availability, not connection -
    /// `connected` gates live-vs-placeholder inside an already-shown spectrum.)
    pub(super) fn can_rx1(&self) -> bool {
        self.thetis_configured
    }

    pub(super) fn vrx_send_enabled(&self, ch: VrxChannel, on: bool) -> bool {
        self.cmd_tx.send(match ch {
            VrxChannel::Vrx1 => Command::SetVrxEnabled(on),
            VrxChannel::Vrx2 => Command::SetVrx2Enabled(on),
        }).is_ok()
    }

    pub(super) fn vrx_send_freq(&self, ch: VrxChannel, hz: u64) -> bool {
        self.cmd_tx.send(match ch {
            VrxChannel::Vrx1 => Command::SetVrxFrequency(hz),
            VrxChannel::Vrx2 => Command::SetVrx2Frequency(hz),
        }).is_ok()
    }

    pub(super) fn vrx_send_mode(&self, ch: VrxChannel, mode: u8) -> bool {
        self.cmd_tx.send(match ch {
            VrxChannel::Vrx1 => Command::SetVrxMode(mode),
            VrxChannel::Vrx2 => Command::SetVrx2Mode(mode),
        }).is_ok()
    }

    pub(super) fn vrx_send_volume(&self, ch: VrxChannel, v: f32) -> bool {
        self.cmd_tx.send(match ch {
            VrxChannel::Vrx1 => Command::SetVrxVolume(v),
            VrxChannel::Vrx2 => Command::SetVrx2Volume(v),
        }).is_ok()
    }

    /// A shared renderer for VRX1 AND VRX2 controls (parity by construction).
    /// Top row = status (mode amber / BW weak),
    /// operation via shared segmented selectors and theme-toggle/action helpers with
    /// mandatory hover. Dispatch discipline same as RX: controls are
    /// only active when connected, and local state is only mutated after the
    /// command has actually been sent.
    /// Wrapper (like RX): in analog mode the s-meter sits at the top right with the
    /// controls in a left column; in bar mode inner renders everything in-line.
    pub(super) fn render_vrx_channel_controls(
        &mut self,
        ui: &mut egui::Ui,
        ch: VrxChannel,
        ddc_center: u64,
        ddc_min: u64,
        ddc_max: u64,
    ) {
        let meter_ch = match ch { VrxChannel::Vrx1 => M_VRX1, VrxChannel::Vrx2 => M_VRX2 };
        if self.meter_analog[meter_ch] {
            let smeter = match ch { VrxChannel::Vrx1 => self.vrx1_spectrum.smeter_dbm(), VrxChannel::Vrx2 => self.vrx2_spectrum.smeter_dbm() };
            let smeter_peak = match ch { VrxChannel::Vrx1 => self.vrx1_spectrum.smeter_peak(), VrxChannel::Vrx2 => self.vrx2_spectrum.smeter_peak() };
            let total_w = ui.available_width();
            let start = ui.cursor().min;
            // Measure the controls height at full width.
            let measure_rect = egui::Rect::from_min_size(start, egui::vec2(total_w, 500.0));
            let mut measure = ui.new_child(egui::UiBuilder::new().max_rect(measure_rect).layout(egui::Layout::top_down(egui::Align::LEFT)));
            self.render_vrx_channel_controls_inner(&mut measure, ch, ddc_center, ddc_min, ddc_max);
            let controls_h = measure.min_rect().height();
            // Meter right, controls left; leave at least ~260px for the controls.
            let meter_w = (total_w - 260.0).max(0.0).min(controls_h * SMETER_VIS_ASPECT);
            let controls_w = total_w - meter_w - if meter_w > 0.0 { 8.0 } else { 0.0 };
            let controls_rect = egui::Rect::from_min_size(start, egui::vec2(controls_w, 500.0));
            let mut left = ui.new_child(egui::UiBuilder::new().max_rect(controls_rect).layout(egui::Layout::top_down(egui::Align::LEFT)));
            self.render_vrx_channel_controls_inner(&mut left, ch, ddc_center, ddc_min, ddc_max);
            let mut mrect = egui::Rect::NOTHING;
            if meter_w > 80.0 {
                let meter_pos = egui::pos2(start.x + controls_w + 4.0, start.y);
                let meter_rect = egui::Rect::from_min_size(meter_pos, egui::vec2(meter_w, controls_h));
                let mut right = ui.new_child(egui::UiBuilder::new().max_rect(meter_rect).layout(egui::Layout::top_down(egui::Align::LEFT)));
                mrect = smeter_analog_sized(&mut right, smeter, smeter_peak, false, false, Some((meter_w, controls_h.min(180.0))));
            }
            ui.advance_cursor_after_rect(egui::Rect::from_min_size(start, egui::vec2(total_w, controls_h)));
            self.meter_click(ui, mrect, meter_ch);
        } else {
            self.render_vrx_channel_controls_inner(ui, ch, ddc_center, ddc_min, ddc_max);
        }
    }

    pub(super) fn render_vrx_channel_controls_inner(
        &mut self,
        ui: &mut egui::Ui,
        ch: VrxChannel,
        ddc_center: u64,
        ddc_min: u64,
        ddc_max: u64,
    ) {
        let id = ch.id();
        let connected = self.connected;
        let enabled = match ch { VrxChannel::Vrx1 => self.vrx1_enabled, VrxChannel::Vrx2 => self.vrx2_enabled };
        let freq_hz = match ch { VrxChannel::Vrx1 => self.vrx1_freq_hz, VrxChannel::Vrx2 => self.vrx2_freq_hz };
        let mode = match ch { VrxChannel::Vrx1 => self.vrx1_mode, VrxChannel::Vrx2 => self.vrx2_mode };
        let filter_low = match ch { VrxChannel::Vrx1 => self.vrx1_filter_low_hz, VrxChannel::Vrx2 => self.vrx2_filter_low_hz };
        let filter_high = match ch { VrxChannel::Vrx1 => self.vrx1_filter_high_hz, VrxChannel::Vrx2 => self.vrx2_filter_high_hz };
        let mut volume = match ch { VrxChannel::Vrx1 => self.vrx1_volume, VrxChannel::Vrx2 => self.vrx2_volume };
        let smeter = match ch { VrxChannel::Vrx1 => self.vrx1_spectrum.smeter_dbm(), VrxChannel::Vrx2 => self.vrx2_spectrum.smeter_dbm() };
        let smeter_peak = match ch { VrxChannel::Vrx1 => self.vrx1_spectrum.smeter_peak(), VrxChannel::Vrx2 => self.vrx2_spectrum.smeter_peak() };
        let source_freq = match ch { VrxChannel::Vrx1 => self.frequency_hz, VrxChannel::Vrx2 => self.rx2_frequency_hz };
        let cur_bw = (filter_high - filter_low).abs();
        let meter_ch = match ch { VrxChannel::Vrx1 => M_VRX1, VrxChannel::Vrx2 => M_VRX2 };
        let analog = self.meter_analog[meter_ch];
        let (freq_prefix, en_hover, copy_hover, mode_hover, bw_hover, vol_hover) = match ch {
            VrxChannel::Vrx1 => ("A:",
                rust_i18n::t!("main_vrx_enable_hover", n = 1).to_string(),
                rust_i18n::t!("main_vrx_copy_hover_a", n = 1).to_string(),
                rust_i18n::t!("main_vrx_mode_hover", n = 1).to_string(),
                rust_i18n::t!("main_vrx_bw_hover", n = 1).to_string(),
                rust_i18n::t!("main_vrx_vol_hover", n = 1).to_string()),
            VrxChannel::Vrx2 => ("B:",
                rust_i18n::t!("main_vrx_enable_hover", n = 2).to_string(),
                rust_i18n::t!("main_vrx_copy_hover_b", n = 2).to_string(),
                rust_i18n::t!("main_vrx_mode_hover", n = 2).to_string(),
                rust_i18n::t!("main_vrx_bw_hover", n = 2).to_string(),
                rust_i18n::t!("main_vrx_vol_hover", n = 2).to_string()),
        };

        // Header: VRXn + enable toggle + DDC range (status, not clickable)
        ui.horizontal(|ui| {
            ui.label(RichText::new(ch.label()).size(theme::TL_CHANNEL_HEADER_FONT).strong());
            if Self::render_window_audio_toggle(ui, enabled, connected, &en_hover) {
                self.toggle_vrx_audio(ch);
            }
            ui.label(RichText::new(format!(
                "{} | DDC {:.3} MHz | range {:.3}-{:.3} MHz",
                band_label(ddc_center),
                ddc_center as f64 / 1_000_000.0,
                ddc_min as f64 / 1_000_000.0,
                ddc_max as f64 / 1_000_000.0,
            )).size(theme::TL_BW_STATUS_FONT).color(Color32::GRAY));
            // Orange "out of band" warning: VRX frequency falls outside the
            // current DDC window (RX is on a different band). VRX then doesn't
            // work meaningfully / very low sensitivity. Amber = status (style guide).
            if freq_hz < ddc_min || freq_hz > ddc_max {
                ui.label(
                    RichText::new(rust_i18n::t!("main_out_of_band").to_string())
                        .size(theme::TL_BW_STATUS_FONT)
                        .strong()
                        .color(theme::TL_AMBER_TEXT),
                )
                .on_hover_text(
                    rust_i18n::t!("main_out_of_band_hover").to_string(),
                );
            }
        });

        // Frequency + mode/BW status + Copy VFO
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{}  ", freq_prefix)).size(theme::TL_FREQ_FONT).strong());
            if let Some(delta) = render_freq_scroll(ui, freq_hz) {
                // The digit under the pointer sets the step, so the grid is that
                // same digit - the band edge snaps inward to it instead of
                // leaving a stray remainder in the readout.
                let next = step_on_grid(freq_hz, delta, delta.unsigned_abs(), ddc_min, ddc_max);
                if connected && self.vrx_send_freq(ch, next) {
                    match ch { VrxChannel::Vrx1 => self.vrx1_freq_hz = next, VrxChannel::Vrx2 => self.vrx2_freq_hz = next };
                    self.save_full_config();
                }
            }
            ui.label(RichText::new(vrx_mode_label(mode)).size(theme::TL_MODE_STATUS_FONT).color(theme::TL_AMBER_TEXT));
            ui.label(RichText::new(format_bandwidth(cur_bw, false)).size(theme::TL_BW_STATUS_FONT).weak());
            if theme::tl_action_button(ui, &rust_i18n::t!("main_copy_vfo").to_string(), connected, theme::TL_SEGMENT_FONT, &copy_hover).clicked()
                && self.vrx_send_freq(ch, source_freq)
            {
                match ch { VrxChannel::Vrx1 => self.vrx1_freq_hz = source_freq, VrxChannel::Vrx2 => self.vrx2_freq_hz = source_freq };
                self.save_full_config();
            }
        });

        // S-meter: in-line in bar mode; in analog mode the wrapper draws it
        // at the top right (like RX). Clicking the meter toggles the type.
        if !analog {
            let mrect = smeter_bar_popout(ui, smeter, smeter_peak, false, false, 100);
            self.meter_click(ui, mrect, meter_ch);
        }

        // Volume
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("main_volume_label").to_string());
            let resp = ui.add_enabled(connected, egui::Slider::new(&mut volume, 0.001..=1.0)
                .logarithmic(true)
                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)))
                .on_hover_text(vol_hover);
            let scrolled = helpers::slider_wheel(ui, &resp, &mut volume, 0.001..=1.0, 0.02);
            if (resp.changed() || scrolled) && self.vrx_send_volume(ch, volume) {
                match ch { VrxChannel::Vrx1 => self.vrx1_volume = volume, VrxChannel::Vrx2 => self.vrx2_volume = volume };
                self.save_full_config();
            }
        });

        // Mode (shared segmented selector; sole control)
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("main_mode_label").to_string());
            if let Some(mode_val) = theme::tl_segmented_selector(
                ui,
                VRX_MODES.iter().map(|&(m, l)| (m, l.to_string())),
                mode, connected, theme::TL_SEGMENT_FONT, &mode_hover,
            ) {
                if self.vrx_send_mode(ch, mode_val) {
                    match ch { VrxChannel::Vrx1 => self.vrx1_mode = mode_val, VrxChannel::Vrx2 => self.vrx2_mode = mode_val };
                    let (lo, hi) = vrx_mode_default_filter(mode, mode_val, cur_bw);
                    // Default filter for this mode: only commit local filter state + persist
                    // after a confirmed send (dispatch-return discipline -
                    // no UI/server drift on a failed send).
                    if self.cmd_tx.send(Command::SetVrxFilter(id, lo, hi)).is_ok() {
                        match ch {
                            VrxChannel::Vrx1 => { self.vrx1_filter_low_hz = lo; self.vrx1_filter_high_hz = hi; }
                            VrxChannel::Vrx2 => { self.vrx2_filter_low_hz = lo; self.vrx2_filter_high_hz = hi; }
                        };
                        self.save_full_config();
                    }
                }
            }
        });

        // SAM auto-tune-to-carrier (only meaningful in SAM, mode 3)
        if mode == 3 {
            ui.horizontal(|ui| {
                let mut at = match ch { VrxChannel::Vrx1 => self.vrx1_auto_tune, VrxChannel::Vrx2 => self.vrx2_auto_tune };
                if ui.add_enabled(connected, egui::Checkbox::new(&mut at, rust_i18n::t!("main_auto_tune_carrier").to_string()))
                    .on_hover_text(rust_i18n::t!("main_auto_tune_carrier_hover").to_string())
                    .changed()
                    && self.cmd_tx.send(Command::SetVrxAutoTune(id, at)).is_ok()
                {
                    match ch { VrxChannel::Vrx1 => self.vrx1_auto_tune = at, VrxChannel::Vrx2 => self.vrx2_auto_tune = at };
                }
            });
        }

        // BW (shared segmented selector; mode-dependent presets)
        ui.horizontal(|ui| {
            ui.label(rust_i18n::t!("main_bw_label").to_string());
            if let Some(p) = theme::tl_segmented_selector(
                ui,
                vrx_filter_presets(mode).iter().map(|&p| (p, format_bandwidth(p, false))),
                vrx_preset_of_width(mode, cur_bw), connected, theme::TL_SEGMENT_FONT, &bw_hover,
            ) {
                let (lo, hi) = vrx_filter_from_preset(mode, p);
                if self.cmd_tx.send(Command::SetVrxFilter(id, lo, hi)).is_ok() {
                    match ch {
                        VrxChannel::Vrx1 => { self.vrx1_filter_low_hz = lo; self.vrx1_filter_high_hz = hi; }
                        VrxChannel::Vrx2 => { self.vrx2_filter_low_hz = lo; self.vrx2_filter_high_hz = hi; }
                    };
                    self.save_full_config();
                }
            }
        });
    }
    /// VRX audio checkbox (channel on/off). Sends ONLY the audio subscription; the
    /// VRX spectrum is separate (own toggle); audio off must not kill the spectrum
    /// and vice versa (checkbox model, same decouple as RX2).
    pub(super) fn toggle_vrx_audio(&mut self, ch: VrxChannel) {
        let enabled = match ch { VrxChannel::Vrx1 => self.vrx1_enabled, VrxChannel::Vrx2 => self.vrx2_enabled };
        let high_res = match ch { VrxChannel::Vrx1 => self.vrx1_high_res_spectrum, VrxChannel::Vrx2 => self.vrx2_high_res_spectrum };
        let now = !enabled;
        if self.vrx_send_enabled(ch, now) {
            match ch { VrxChannel::Vrx1 => self.vrx1_enabled = now, VrxChannel::Vrx2 => self.vrx2_enabled = now };
            match ch {
                VrxChannel::Vrx1 => log::info!("{}", self.vrx1_spectrum.debug_line(now, high_res)),
                VrxChannel::Vrx2 => log::info!("{}", self.vrx2_spectrum.debug_line(now, high_res)),
            }
            self.save_full_config();
        }
    }

    /// How many audio channels this setup can produce, counted on availability
    /// rather than on what is switched on: the label above the master slider
    /// must not flicker while channels are muted.
    pub(super) fn audio_channel_count(&self) -> usize {
        let mut n = 0;
        if self.can_rx1() { n += 1; }
        if self.can_rx2() { n += 1; }
        if self.can_vrx() { n += 1; }              // VRX1 rides RX1
        if self.can_vrx() && self.rx2_present { n += 1; } // VRX2 rides RX2
        if self.yaesu_present_last { n += 1; }
        if self.yaesu2_present_last { n += 1; }
        n
    }

    /// RX1 audio on/off. Shared by the main-screen channel block and the RX1
    /// window, so both routes run the same code instead of each repainting the
    /// same switch.
    ///
    /// Optimistic like all six audio toggles: the UI flips at once and the
    /// server may only veto within the grace window. That invariant is what
    /// keeps the six channels feeling identical - see `reconcile_audio_enable`.
    pub(super) fn toggle_rx1_audio(&mut self) {
        self.rx1_enabled = !self.rx1_enabled;
        self.rx1_enabled_pending = Some((Instant::now(), self.rx1_enabled));
        let _ = self.cmd_tx.send(Command::SetRx1Enabled(self.rx1_enabled));
        self.save_full_config();
    }

    /// RX2 audio on/off. Same contract as `toggle_rx1_audio`.
    pub(super) fn toggle_rx2_audio(&mut self) {
        self.rx2_enabled = !self.rx2_enabled;
        self.rx2_enabled_pending = Some((Instant::now(), self.rx2_enabled));
        let _ = self.cmd_tx.send(Command::SetRx2Enabled(self.rx2_enabled));
        self.save_full_config();
    }

    /// Yaesu audio on/off for one slot. Separate from the window: the control
    /// window can stay open with the audio muted.
    ///
    /// The two slots persist to different files - slot 0 lives in the full
    /// config, slot 1 in the PTT config - so the save differs while everything
    /// else is shared.
    pub(super) fn toggle_yaesu_audio(&mut self, slot: u8) {
        if slot == 0 {
            self.yaesu_enabled = !self.yaesu_enabled;
            let _ = self.cmd_tx.send(Command::SetControl(
                sdr_remote_core::protocol::ControlId::YaesuEnable, self.yaesu_enabled as u16));
            self.save_full_config();
        } else {
            self.yaesu2_enabled = !self.yaesu2_enabled;
            let _ = self.cmd_tx.send(Command::SetYaesu2Enable(self.yaesu2_enabled));
            self.save_ptt_config();
        }
    }

    /// The audio button as it appears INSIDE a channel window: same label,
    /// same fill convention and same hover as the block on the main screen, so
    /// a channel can be muted from either place without the two disagreeing.
    /// Returns true when clicked.
    pub(super) fn render_window_audio_toggle(
        ui: &mut egui::Ui,
        on: bool,
        connected: bool,
        hover: &str,
    ) -> bool {
        theme::tl_toggle_button(
            ui,
            &rust_i18n::t!("main_chip_audio").to_string(),
            on,
            connected,
            theme::TL_SEGMENT_FONT,
            hover,
        )
        .clicked()
    }

    /// The width of the VRX window to ask the server for: the DDC span over the
    /// zoom, in Hz (build 75; it went in whole kHz, at least 1, so the zoom
    /// stopped at the DDC rate / 1000).
    pub(super) fn vrx_requested_span_hz(&self, ch: VrxChannel) -> u32 {
        let zoom = match ch { VrxChannel::Vrx1 => self.vrx1_spectrum_zoom, VrxChannel::Vrx2 => self.vrx2_spectrum_zoom };
        (self.vrx_ddc_span_hz(ch) as f32 / zoom.max(1.0)) as u32
    }

    /// Where the VRX window sits relative to the listening frequency, in Hz, as
    /// the server receives it. The one value both the request and the drawing
    /// use: the drawing took the pan of the tuning range and the request that of
    /// the DDC, so at any pan but 0 the two windows lay apart.
    pub(super) fn vrx_pan_hz(&self, ch: VrxChannel) -> i32 {
        let pan = match ch { VrxChannel::Vrx1 => self.vrx1_pan, VrxChannel::Vrx2 => self.vrx2_pan };
        sdr_remote_core::protocol::vrx_pan_on_the_wire((pan as f64 * self.vrx_ddc_span_hz(ch) as f64) as i32)
    }

    /// VRX spectrum toggle (high-res). Shared logic (pop-out + row).
    pub(super) fn toggle_vrx_spectrum(&mut self, ch: VrxChannel) {
        let id: u8 = match ch { VrxChannel::Vrx1 => 0, VrxChannel::Vrx2 => 1 };
        let hr = !match ch { VrxChannel::Vrx1 => self.vrx1_high_res_spectrum, VrxChannel::Vrx2 => self.vrx2_high_res_spectrum };
        let span_hz = self.vrx_requested_span_hz(ch);
        // Spectrum subscription is SEPARATE from the VRX audio checkbox:
        // send `hr`, not `enabled && hr`. The server produces VRX spectrum without
        // the audio runtime (subscriber list + manager entry are independent).
        if self.cmd_tx.send(Command::SetVrxHighResSpectrum(id, hr, span_hz)).is_ok() {
            match ch { VrxChannel::Vrx1 => self.vrx1_high_res_spectrum = hr, VrxChannel::Vrx2 => self.vrx2_high_res_spectrum = hr };
            match ch { VrxChannel::Vrx1 => self.vrx1_high_res_last_span_hz = span_hz, VrxChannel::Vrx2 => self.vrx2_high_res_last_span_hz = span_hz };
            if hr {
                // Spectrum on → open the detached VRX window of THIS channel so the
                // spectrum actually becomes visible (VRX spectrum only shows there).
                // Window opens via the model-B derivation (want=high_res && can_vrx);
                // here we only reset init_applied so saved geometry re-applies on reopen.
                match ch {
                    VrxChannel::Vrx1 => { self.vrx_popout_init_applied = false; }
                    VrxChannel::Vrx2 => { self.vrx2_popout_init_applied = false; }
                }
            } else {
                // Spectrum off: clear the buffer. The window closes via derivation
                // (want=high_res drops); no imperative popout write needed.
                match ch {
                    VrxChannel::Vrx1 => { self.vrx1_spectrum.clear(); }
                    VrxChannel::Vrx2 => { self.vrx2_spectrum.clear(); }
                }
            }
            self.save_full_config();
        }
    }

    /// RX-independent DDC bandwidth (Hz) of the channel this VRX
    /// runs on, the reference width for the high-res span request. Comes from the
    /// DDC sample rate (always known once connected), NOT from the
    /// RX spectrum. This way the VRX zoom also works with the RX spectrum off.
    /// Fallback 384 kHz matches the existing DDC defaults.
    pub(super) fn vrx_ddc_span_hz(&self, ch: VrxChannel) -> u32 {
        let sr_khz = match ch {
            VrxChannel::Vrx1 => self.ddc_sample_rate_rx1,
            VrxChannel::Vrx2 => self.ddc_sample_rate_rx2,
        };
        if sr_khz > 0 { sr_khz as u32 * 1000 } else { 384_000 }
    }

    /// Lowest/highest frequency a VRX can actually be listened to, given the
    /// DDC window it lives in and its own filter width.
    ///
    /// This mirrors the server's guard exactly (`DDC_USABLE_FRACTION` in
    /// `vrx-rs/src/runtime.rs`): the outer tenth of the DDC band is the DDC's
    /// own roll-off, and the channel's far filter edge has to fit inside the
    /// remainder. The server holds the channel at that boundary, so the UI must
    /// stop there too - otherwise the readout and the spectrum keep travelling
    /// while the audio stands still, which is exactly what it sounds like.
    pub(super) fn vrx_tune_limits(&self, ch: VrxChannel, ddc_center: u64) -> (u64, u64) {
        // The same number as the server's; a test holds the two together
        // (build 228): the client does not depend on `vrx-rs`.
        const DDC_USABLE_FRACTION: f64 = 0.9;
        // The width: the server's DDC rate, then the full-band row, then a
        // guess (`DdcWidth`, build 232). A guess wider than the real band
        // re-opens the gap build 61 closed - tuning past where the server
        // listens - so it is the last resort.
        let span = match ch {
            VrxChannel::Vrx1 => DdcWidth {
                server_rate_khz: self.ddc_sample_rate_rx1,
                row_span_hz: self.full_spectrum_span_hz,
                row_on: self.full_row_live(self.full_row_at),
            },
            VrxChannel::Vrx2 => DdcWidth {
                server_rate_khz: self.ddc_sample_rate_rx2,
                row_span_hz: self.rx2_full_spectrum_span_hz,
                row_on: self.full_row_live(self.rx2_full_row_at),
            },
        }
        .hz();
        let (lo_hz, hi_hz) = match ch {
            VrxChannel::Vrx1 => (self.vrx1_filter_low_hz, self.vrx1_filter_high_hz),
            VrxChannel::Vrx2 => (self.vrx2_filter_low_hz, self.vrx2_filter_high_hz),
        };
        let filter_edge = lo_hz.abs().max(hi_hz.abs()) as f64;
        let reach = (span * 0.5 * DDC_USABLE_FRACTION - filter_edge).max(0.0) as u64;
        (ddc_center.saturating_sub(reach), ddc_center.saturating_add(reach))
    }


    /// Shared renderer for ONE VRX spectrum panel (VRX1 or VRX2): the
    /// Ref/Auto/Range and Zoom/Pan/WF button rows + the spectrum/waterfall strip.
    /// Parity by construction: VRX1 and VRX2 share exactly this code. Spectrum
    /// display settings are local
    /// (no server command); click-to-tune follows the dispatch discipline.
    pub(super) fn render_vrx_spectrum_panel(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        ch: VrxChannel,
        vrx_min: u64,
        vrx_max: u64,
    ) {
        let connected = self.connected;
        let rx_label = match ch { VrxChannel::Vrx1 => "RX1", VrxChannel::Vrx2 => "RX2" };
        // The FFT size of the receiver this channel rides (build 242: VRX2
        // showed RX1's).
        let fft_size_k = match ch { VrxChannel::Vrx1 => self.spectrum_fft_size_k, VrxChannel::Vrx2 => self.rx2_spectrum_fft_size_k };
        let ddc_sr = match ch { VrxChannel::Vrx1 => self.ddc_sample_rate_rx1, VrxChannel::Vrx2 => self.ddc_sample_rate_rx2 };
        let ddc_rate = if ddc_sr > 0 { ddc_sr as u32 * 1000 } else { 384_000 };
        // The view rows: the one set every spectrum panel has.
        let changes = {
            let (ref_db, auto_ref, range_db, zoom, pan, wf_contrast) = match ch {
                VrxChannel::Vrx1 => (&mut self.vrx1_ref_db, &mut self.vrx1_auto_ref, &mut self.vrx1_range_db,
                    &mut self.vrx1_spectrum_zoom, &mut self.vrx1_pan, &mut self.vrx1_wf_contrast),
                VrxChannel::Vrx2 => (&mut self.vrx2_ref_db, &mut self.vrx2_auto_ref, &mut self.vrx2_range_db,
                    &mut self.vrx2_spectrum_zoom, &mut self.vrx2_pan, &mut self.vrx2_wf_contrast),
            };
            super::spectrum::spectrum_view_rows(ui, super::spectrum::ViewControls {
                ref_db,
                auto_ref,
                range_db,
                fill_percent: &mut self.spectrum_fill_percent,
                zoom,
                allow_zoom_below_2x: &mut self.allow_zoom_below_2x,
                pan,
                wf_contrast,
                fft: super::spectrum::FftControl::Shared { size_k: fft_size_k, ddc_rate_hz: ddc_rate, rx_label },
                height: None,
            })
        };
        let auto_now = match ch { VrxChannel::Vrx1 => self.vrx1_auto_ref, VrxChannel::Vrx2 => self.vrx2_auto_ref };
        if changes.restart_auto_ref(auto_now) {
            match ch {
                VrxChannel::Vrx1 => self.vrx1_spectrum.reset_auto_ref(),
                VrxChannel::Vrx2 => self.vrx2_spectrum.reset_auto_ref(),
            }
        }
        if changes.allow_zoom_below_2x {
            self.zoom_below_2x_changed();
        }
        if changes.zoom {
            match ch { VrxChannel::Vrx1 => self.vrx1_zoom_initialized = true, VrxChannel::Vrx2 => self.vrx2_zoom_initialized = true }
        }
        if changes.settings() || changes.zoom || changes.pan {
            self.save_full_config();
        }
        let remaining = ui.available_height();
        // Plot and waterfall equal height (operator wish), the one rule for every
        // spectrum panel (build 241).
        let (spec_h, wf_h) = super::spectrum::plot_and_waterfall_heights(remaining, ui.spacing().item_spacing.y);
        let pan_hz = self.vrx_pan_hz(ch) as f64;
        let salt = match ch { VrxChannel::Vrx1 => "vrx1", VrxChannel::Vrx2 => "vrx2" };
        // VRX renders EXCLUSIVELY its own extracted spectrum: no RX fallback;
        // the bins are the window the server cut at the VRX's width and pan.
        let action = {
            let (spectrum, texture, freq, low, high, ref_db, range_db, wf_contrast) = match ch {
                VrxChannel::Vrx1 => (&self.vrx1_spectrum, &mut self.vrx1_waterfall_texture, self.vrx1_freq_hz,
                    self.vrx1_filter_low_hz, self.vrx1_filter_high_hz, self.vrx1_ref_db, self.vrx1_range_db, self.vrx1_wf_contrast),
                VrxChannel::Vrx2 => (&self.vrx2_spectrum, &mut self.vrx2_waterfall_texture, self.vrx2_freq_hz,
                    self.vrx2_filter_low_hz, self.vrx2_filter_high_hz, self.vrx2_ref_db, self.vrx2_range_db, self.vrx2_wf_contrast),
            };
            let span = spectrum.span_hz() as f64;
            let panel = super::spectrum::SpectrumPanel {
                id: salt,
                bins: spectrum.bins(),
                bins_center_hz: spectrum.center_hz() as f64,
                bins_span_hz: span,
                view_center_hz: freq as f64 + pan_hz,
                marker_hz: freq,
                filter_low_hz: low,
                filter_high_hz: high,
                ref_db,
                range_db,
                fill_percent: self.spectrum_fill_percent,
                wf_contrast,
                meter: super::spectrum::SpectrumMeter::Signal(spectrum.smeter_dbm()),
                big_meter: true,
                rit_hz: None,
                band_markers: false,
                dx_spots: &self.dx_spots,
                tuning: super::spectrum::SpectrumTuning {
                    tune_hz: freq,
                    min_hz: vrx_min,
                    max_hz: vrx_max,
                    filter: super::spectrum::FilterEdges::VrxRaster,
                },
            };
            let window = super::spectrum::WaterfallWindow { center_hz: panel.view_center_hz, span_hz: span, row_span_hz: span };
            super::spectrum::spectrum_panel_view(ui, ctx, &panel, spectrum.waterfall(), window, texture, spec_h, wf_h)
        };
        match action {
            Some(super::spectrum::SpectrumAction::Tune { hz, .. }) => {
                if connected && self.vrx_send_freq(ch, hz) {
                    match ch { VrxChannel::Vrx1 => self.vrx1_freq_hz = hz, VrxChannel::Vrx2 => self.vrx2_freq_hz = hz };
                    self.save_full_config();
                }
            }
            // The window's filter dispatch reads these (`popouts.rs`).
            Some(super::spectrum::SpectrumAction::FilterEdge { low, hz }) => {
                let key = format!("{}_filter_{}_hz", salt, if low { "low" } else { "high" });
                ui.memory_mut(|mem| mem.data.insert_temp(egui::Id::new(key), hz));
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    /// Every spectrum panel is the one description (build 81, the operator's
    /// wish: all spectrum and waterfall windows as equal as they can be): RX1,
    /// RX2 and the VRX each draw the shared view rows once and the shared plot
    /// and waterfall once, with nothing of their own around them; what differs
    /// is a field of the panel, held in this table.
    #[test]
    fn every_spectrum_panel_is_the_one_description() {
        let cut = |s: &'static str| &s[..s.find("#[cfg(test)]").unwrap_or(s.len())];
        let panel = |src: &'static str, from: &str| {
            let body = cut(src);
            let body = &body[body.find(from).unwrap()..];
            &body[..body[3..].find("\n    pub(super) fn ").map_or(body.len(), |i| i + 3)]
        };
        let rx1 = panel(include_str!("spectrum_content.rs"), "fn render_spectrum_content");
        let rx2 = panel(include_str!("rx_controls.rs"), "fn render_rx2_spectrum_only");
        let vrx = panel(include_str!("vrx.rs"), "fn render_vrx_spectrum_panel");
        for (name, body, band_markers, rit, filter) in [
            ("RX1", rx1, "band_markers: true,", "rit_hz: self.rit_enable.then_some(self.rit_offset as i32),", "FilterEdges::Free"),
            ("RX2", rx2, "band_markers: false,", "rit_hz: None,", "FilterEdges::Free"),
            ("VRX", vrx, "band_markers: false,", "rit_hz: None,", "FilterEdges::VrxRaster"),
        ] {
            assert_eq!(body.matches("super::spectrum::spectrum_view_rows(").count(), 1, "{name}: the view rows");
            assert_eq!(body.matches("super::spectrum::spectrum_panel_view(").count(), 1, "{name}: the plot and waterfall");
            let rows = body.find("spectrum_view_rows(").unwrap();
            assert!(rows < body.find("spectrum_panel_view(").unwrap(), "{name}: rows above the plot");
            for own in ["ui.horizontal(", "Slider::new(", "egui::ComboBox", "\"main_allow_zoom_2x\""] {
                assert!(!body.contains(own), "{name}: nothing drawn of its own ({own})");
            }
            // The shared values: one Fill and one Zoom <2x flag for every panel.
            assert!(body.contains("fill_percent: &mut self.spectrum_fill_percent,"), "{name}");
            assert!(body.contains("fill_percent: self.spectrum_fill_percent,"), "{name}: the plot reads it");
            assert!(body.contains("allow_zoom_below_2x: &mut self.allow_zoom_below_2x,"), "{name}");
            // What follows a change: RX hands it to the place MIDI hands its own to.
            let handled = match name {
                "RX1" => body.contains("self.rx_view_changed(false, changes);"),
                "RX2" => body.contains("self.rx_view_changed(true, changes);"),
                _ => body.contains("if changes.allow_zoom_below_2x {\n")
                    && body.contains("self.zoom_below_2x_changed();")
                    && body.contains("changes.settings()")
                    && body.contains("changes.restart_auto_ref("),
            };
            assert!(handled, "{name}: a change is handled and saved");
            assert!(body.contains(band_markers) && body.contains(rit) && body.contains(filter), "{name}: its row of the table");
            assert!(body.contains("dx_spots: &self.dx_spots,"), "{name}: DX spots in every panel");
        }
        // The VRX: the FFT of the receiver it rides (build 242: VRX2 showed
        // RX1's), shown and not chosen; it tunes inside its DDC window.
        assert!(vrx.contains("VrxChannel::Vrx2 => self.rx2_spectrum_fft_size_k"), "VRX2 shows RX2's FFT size");
        assert!(vrx.contains("super::spectrum::FftControl::Shared {"));
        assert!(vrx.contains("min_hz: vrx_min,") && vrx.contains("max_hz: vrx_max,"));
        assert!(rx1.contains("FftControl::Choose {") && rx2.contains("FftControl::Choose {"));
        // The rows: one checkbox, one Fill field, a rule under them.
        let draw = cut(include_str!("spectrum.rs"));
        let rows = &draw[draw.find("pub(crate) fn spectrum_view_rows(").unwrap()..];
        let rows = &rows[..rows.find("\n}\n").unwrap()];
        assert_eq!(rows.matches("rust_i18n::t!(\"main_allow_zoom_2x\")").count(), 1);
        assert_eq!(rows.matches("helpers::spectrum_fill_field(ui, fill_percent)").count(), 1);
        assert!(rows.ends_with("    super::helpers::rule(ui);\n    ch"), "a rule under the rows");
        let label = "rust_i18n::t!(\"main_allow_zoom_2x\")";
        for f in [include_str!("spectrum_content.rs"), include_str!("rx_controls.rs"), include_str!("vrx.rs")] {
            assert!(!cut(f).contains(label), "no checkbox of its own");
        }
        // One plot and one waterfall, no strip of the VRX's own.
        assert_eq!(draw.matches("ui.allocate_exact_size(").count(), 2, "the plot and the waterfall");
        assert!(!draw.contains("fn render_vrx_strip") && !draw.contains("fn render_waterfall"));
        let joined = cut(include_str!("popouts.rs"));
        assert!(joined.contains("super::helpers::rule(ui);\n            self.render_rx2_spectrum_only(ui, ctx);"), "a rule above RX2");
    }

    /// Clicking, dragging and scrolling do the same in every panel, plot and
    /// waterfall alike (build 81, the operator's choice: scrolling tunes, also
    /// over a filter edge; a filter edge is dragged). The VRX's filter edge goes
    /// on the server's raster, inside ±4 kHz.
    #[test]
    fn every_panel_tunes_the_same_way() {
        use super::super::spectrum::{FilterEdges, SpectrumTuning, VRX_FILTER_STEP_HZ, snap_vrx_filter_hz};
        let rx = SpectrumTuning { tune_hz: 7_100_400, min_hz: 1_000, max_hz: u64::MAX, filter: FilterEdges::Free };
        let vrx = SpectrumTuning { tune_hz: 7_100_400, min_hz: 7_000_000, max_hz: 7_200_000, filter: FilterEdges::VrxRaster };
        for t in [rx, vrx] {
            assert_eq!(t.click(7_123_499.0), 7_123_000, "a click: the nearest kHz");
            assert_eq!(t.click(7_123_500.0), 7_124_000);
            assert_eq!(t.drag(7_123_456.0), 7_123_400, "a drag: steps of 100 Hz");
            assert_eq!(t.scroll(true), 7_101_000, "a notch: the next kHz");
            assert_eq!(t.scroll(false), 7_099_000);
        }
        assert_eq!(vrx.click(6_000_000.0), 7_000_000, "a VRX stays in its window");
        assert_eq!(vrx.drag(8_000_000.0), 7_200_000);
        assert_eq!(rx.filter_edge(true, -2_412.4, -2_400, 0), -2_412, "RX: to the hertz");
        assert_eq!(vrx.filter_edge(false, 2_412.4, 0, 2_400), 2_437, "VRX: the raster (39 x 62.5)");
        assert_eq!(vrx.filter_edge(false, 9_000.0, 0, 2_400), 4_000, "inside 4 kHz");
        assert_eq!(vrx.filter_edge(true, -9_000.0, -2_400, 0), -4_000, "the low edge too");
        assert_eq!(vrx.filter_edge(false, 50.0, 0, 2_400), 125, "two steps from the other edge");
        assert_eq!(snap_vrx_filter_hz(2_100.0), 2_125);
        assert_eq!(VRX_FILTER_STEP_HZ, 62.5);
        let draw = include_str!("spectrum.rs");
        let draw = &draw[..draw.find("#[cfg(test)]").unwrap()];
        assert_eq!(draw.matches("scroll_notch(ui, &response)").count(), 2, "plot and waterfall");
        assert_eq!(draw.matches("panel.tuning.drag(hz_of(pos.x))").count(), 2, "plot and waterfall");
        assert_eq!(draw.matches("panel.tuning.click(hz_of(pos.x))").count(), 2, "plot and waterfall");
        assert!(!draw.contains("outward"), "no filter resize by scrolling");
        // A drag drags and a click clicks, in the waterfall as on the plot.
        assert_eq!(draw.matches("hz: panel.tuning.drag(hz_of(pos.x)), how: TuneHow::Drag").count(), 2);
        // The plot's click goes by way of the DX spots (below), the waterfall's straight.
        assert_eq!(draw.matches("hz: panel.tuning.click(hz_of(pos.x)), how: TuneHow::Click").count(), 1);
        assert!(draw.contains("                None => panel.tuning.click(hz_of(pos.x)),\n            };\n            action = Some(SpectrumAction::Tune { hz, how: TuneHow::Click });"));
        assert!(draw.contains("    if response.dragged() {
        if let Some(pos) = response.interact_pointer_pos() {
            return Some(SpectrumAction::Tune { hz: panel.tuning.drag("));
        // The edge being dragged is forgotten once the drag ends; else every
        // later drag would move it.
        assert!(draw.contains("    if !response.dragged() {
        ui.memory_mut(|mem| mem.data.insert_temp(hover_ready_id, near_lo || near_hi));
"));
        assert!(draw.contains("        if dragging_filter.is_some() {
            ui.memory_mut(|mem| {
                mem.data.remove::<bool>(drag_id);
                mem.data.remove::<bool>(is_low_id);
"));
        // The nearer edge is the one taken, and an RX edge lands on its own key.
        assert!(draw.contains("mem.data.insert_temp(is_low_id, dl < dh)"));
        assert!(draw.contains("let key = if low { \"low\" } else { \"high\" };"));
        // A spot clicked on a VRX stays in its window.
        assert!(draw.contains("Some(s) => (s.frequency_hz).clamp(panel.tuning.min_hz, panel.tuning.max_hz),"));
        // A click on a filter edge is a drag that did not move, not a tune.
        assert!(draw.contains("} else if response.clicked() && !(near_lo || near_hi) {"));
        // The RX actions land where the RX handlers read them.
        assert!(draw.contains("format!(\"{prefix}spectrum_{key}_freq\")") && draw.contains("format!(\"{prefix}spectrum_filter_{key}\")"));
        let keys = include_str!("rx_controls.rs");
        assert!(keys.contains("for key in [\"spectrum_scroll_freq\", \"spectrum_click_freq\", \"spectrum_drag_freq\"] {"));
        assert!(keys.contains("for key in [\"rx2_spectrum_scroll_freq\", \"rx2_spectrum_click_freq\", \"rx2_spectrum_drag_freq\"] {"));
        assert!(keys.contains("egui::Id::new(\"spectrum_filter_low\")") && keys.contains("egui::Id::new(\"rx2_spectrum_filter_high\")"));
        assert!(draw.contains("TuneHow::Click => \"click\",") && draw.contains("TuneHow::Drag => \"drag\",") && draw.contains("TuneHow::Scroll => \"scroll\","));
        let rx1 = include_str!("spectrum_content.rs");
        let rx2 = include_str!("rx_controls.rs");
        assert!(rx1.contains("super::spectrum::store_rx_action(ui, \"\", action);"));
        assert!(rx2.contains("super::spectrum::store_rx_action(ui, \"rx2_\", action);"));
        assert_eq!(draw.matches("rust_i18n::t!(\"spec_hover\")").count(), 1);
        assert_eq!(draw.matches("rust_i18n::t!(\"spec_wf_hover\")").count(), 1);
    }

    /// The pan is sent on every change the server can see, with no threshold,
    /// and drawn as the server receives it (build 75: a pan below 0.001 was
    /// drawn and never sent). The VRX takes its pan and width from one helper
    /// each, for the request and the drawing alike.
    #[test]
    fn the_spectrum_is_drawn_where_the_server_cuts_it() {
        let cut = |s: &'static str| &s[..s.find("#[cfg(test)]").unwrap_or(s.len())];
        for (name, src, pan, last) in [
            ("RX1", cut(include_str!("spectrum_content.rs")), "self.spectrum_pan", "self.last_sent_pan"),
            ("RX2", cut(include_str!("rx_controls.rs")), "self.rx2_spectrum_pan", "self.rx2_last_sent_pan"),
        ] {
            assert!(!src.contains("> 0.001"), "{name}: no threshold");
            // The pan against the one last sent, on the wire value.
            let moved = format!("let pan_moved = sdr_remote_core::protocol::pack_spectrum_pan_fine({pan})");
            let at = src.find(&moved).unwrap_or_else(|| panic!("{name}: {moved}"));
            let rest = src[at + moved.len()..].trim_start();
            assert!(rest.starts_with(&format!("!= sdr_remote_core::protocol::pack_spectrum_pan_fine({last});")), "{name}: {rest:.80}");
            assert!(src.contains(&format!("{pan},\n            self.server_fine_spectrum_view,\n")) || src.contains(&format!("{pan},\n                self.server_fine_spectrum_view,\n")), "{name}: drawn for this server");
        }
        let tuning = cut(include_str!("tuning.rs"));
        let centre = &tuning[tuning.find("fn spectrum_target_center_hz").unwrap()..];
        assert!(centre[..centre.find("\n    }").unwrap()].contains("spectrum_pan_on_the_wire(pan, fine)"), "drawn with the pan as received");
        let sync = cut(include_str!("sync_state.rs"));
        assert!(sync.contains("let want1 = self.vrx_pan_hz(VrxChannel::Vrx1);"));
        assert!(sync.contains("let want2 = self.vrx_pan_hz(VrxChannel::Vrx2);"));
        assert!(!sync.contains("span_khz"), "the VRX width goes in Hz");
        assert_eq!(sync.matches("self.vrx_requested_span_hz(").count(), 4, "reconnect and zoom, both VRX");
        // The width against the one last sent, as this server receives it.
        assert!(sync.contains("let on_the_wire = move |hz| sdr_remote_core::protocol::vrx_span_on_the_wire(hz, fine_view);"));
        assert!(sync.contains("if on_the_wire(span_hz) != on_the_wire(self.vrx1_high_res_last_span_hz) {"));
        assert!(sync.contains("if on_the_wire(span_hz) != on_the_wire(self.vrx2_high_res_last_span_hz) {"));
        assert!(sync.contains("let fine_view = self.server_fine_spectrum_view;"));
        // The field the drawing and the width read is the engine's, every frame.
        assert!(sync.contains("\n        self.server_fine_spectrum_view = state.server_fine_spectrum_view;\n"));
        assert_eq!(sync.matches("self.server_fine_spectrum_view =").count(), 1);
        assert_eq!(tuning.matches("self.spectrum_pan, self.server_fine_spectrum_view);").count(), 1, "RX1 pending frequency");
        assert_eq!(tuning.matches("self.rx2_spectrum_pan, self.server_fine_spectrum_view);").count(), 1, "RX2 pending frequency");
        let vrx = cut(include_str!("vrx.rs"));
        assert!(vrx.contains("let pan_hz = self.vrx_pan_hz(ch) as f64;"));
        let pan_hz = &vrx[vrx.find("fn vrx_pan_hz(").unwrap()..];
        assert!(pan_hz[..pan_hz.find("\n    }").unwrap()].contains("sdr_remote_core::protocol::vrx_pan_on_the_wire("), "the pan as sent");
        assert_eq!(vrx.matches("view_center_hz: freq as f64 + pan_hz,").count(), 1, "drawn with it");
        assert!(!cut(include_str!("spectrum.rs")).contains("pan_reference_hz"), "no pan of its own in the strip");
    }

    /// The VRX window has no checkbox of its own for its spectrum
    /// (2026-10-16, build 225): unticking it closed the window, which must not
    /// happen. The window opens with the chip on the main screen and closes
    /// with its X; both still go through `toggle_vrx_spectrum` and
    /// `close_vrx_popout`.
    #[test]
    fn the_vrx_window_closes_only_with_its_x() {
        let src = include_str!("vrx.rs");
        let body = &src[..src.find("#[cfg(test)]").unwrap()];
        assert!(!body.contains("main_high_res_spectrum"));
        assert!(!body.contains("Checkbox::new(&mut hr"));
        let yml = include_str!("../../locales/app.yml");
        assert!(!yml.contains("main_high_res_spectrum"), "the text goes with it");
        let update = include_str!("update.rs");
        assert_eq!(update.matches("if vrx1_spec_click { self.toggle_vrx_spectrum(VrxChannel::Vrx1); }").count(), 1);
        assert_eq!(update.matches("if vrx2_spec_click { self.toggle_vrx_spectrum(VrxChannel::Vrx2); }").count(), 1);
    }

    /// A re-centering across a 100 kHz line no longer moves a VRX that is
    /// still within reach; out of
    /// reach it goes to the frequency saved for the new bucket, or the VFO.
    /// Reach is the server's guard (build 227): 384 kHz x 0.45 less a 3 kHz
    /// filter edge here. The second pass: back across the line, and a band
    /// change and back.
    #[test]
    fn a_vrx_within_reach_stays_when_the_centre_crosses_a_bucket_line() {
        use super::{DdcMove, VrxAfterMove};
        let mut saved = std::collections::HashMap::new();
        const REACH: u64 = 169_800;
        let mv = |center_hz: u64, last_center_hz: u64, vrx_hz, vfo_hz| DdcMove {
            center_hz,
            last_center_hz,
            reach_lo_hz: center_hz - REACH,
            reach_hi_hz: center_hz + REACH,
            last_reach_lo_hz: last_center_hz.saturating_sub(REACH),
            last_reach_hi_hz: last_center_hz + REACH,
            measured: true,
            vrx_hz,
            vfo_hz,
        };
        // Nothing moved, and the first frame.
        assert_eq!(mv(7_099_900, 7_099_000, 7_050_000, 7_090_000).follow(&mut saved), VrxAfterMove::Unchanged);
        assert_eq!(mv(7_099_900, 0, 7_050_000, 7_090_000).follow(&mut saved), VrxAfterMove::Unchanged);
        assert!(saved.is_empty());
        // 7099.9 -> 7100.1: another bucket, the VRX at 7050 still in range.
        assert_eq!(mv(7_100_100, 7_099_900, 7_050_000, 7_100_000).follow(&mut saved), VrxAfterMove::Stays);
        assert_eq!(saved.get(&70), Some(&7_050_000), "the old bucket keeps it");
        // And back: stays again.
        assert_eq!(mv(7_099_900, 7_100_100, 7_050_000, 7_100_000).follow(&mut saved), VrxAfterMove::Stays);
        // A band change: out of range, nothing saved there yet -> the VFO.
        assert_eq!(
            mv(14_200_000, 7_099_900, 7_050_000, 14_200_000).follow(&mut saved),
            VrxAfterMove::To { hz: 14_200_000, saved: false }
        );
        // On 20 m the VRX went to 14 150; back to 40 m -> what 40 m had.
        assert_eq!(
            mv(7_099_900, 14_200_000, 14_150_000, 7_090_000).follow(&mut saved),
            VrxAfterMove::To { hz: 7_050_000, saved: true }
        );
        assert_eq!(saved.get(&142), Some(&14_150_000));
        // A saved frequency out of the new range is not used.
        saved.insert(71, 6_000_000);
        assert_eq!(
            mv(7_150_000, 14_200_000, 14_150_000, 7_140_000).follow(&mut saved),
            VrxAfterMove::To { hz: 7_140_000, saved: false }
        );
        // A VRX never set (0) goes where it went before 226.
        assert_eq!(
            mv(7_250_000, 7_150_000, 0, 7_240_000).follow(&mut saved),
            VrxAfterMove::To { hz: 7_240_000, saved: false }
        );
        // Between the server's guard and the half span (a probe on build
        // 226): 6920 kHz under a centre of 7100.1 is held at 6930.3 by the
        // server, so it does not stay.
        let mut fresh = std::collections::HashMap::new();
        assert_eq!(
            mv(7_100_100, 7_099_900, 6_920_000, 7_100_000).follow(&mut fresh),
            VrxAfterMove::To { hz: 7_100_000, saved: false }
        );
        // Within one bucket (build 227): in reach nothing happens, out of reach
        // it moves; a VRX never set does nothing there.
        let mut fresh = std::collections::HashMap::new();
        assert_eq!(mv(7_160_000, 7_150_000, 7_100_000, 7_160_000).follow(&mut fresh), VrxAfterMove::Unchanged);
        assert_eq!(
            mv(7_160_000, 7_150_000, 6_985_000, 7_160_000).follow(&mut fresh),
            VrxAfterMove::To { hz: 7_160_000, saved: false }
        );
        assert_eq!(mv(7_170_000, 7_160_000, 0, 7_170_000).follow(&mut fresh), VrxAfterMove::Unchanged);
        assert!(fresh.is_empty(), "within a bucket nothing is saved");
        // The reach changes with the centre standing still (build 228): a
        // wider filter takes 6990 kHz out of reach under 7160 -> the VFO; in
        // reach nothing happens, and a VRX never set does nothing either.
        let still = |vrx_hz, lo: u64, last_lo: u64| DdcMove {
            center_hz: 7_160_000,
            last_center_hz: 7_160_000,
            reach_lo_hz: lo,
            reach_hi_hz: 7_160_000 + (7_160_000 - lo),
            last_reach_lo_hz: last_lo,
            last_reach_hi_hz: 7_160_000 + (7_160_000 - last_lo),
            measured: true,
            vrx_hz,
            vfo_hz: 7_160_000,
        };
        let mut fresh = std::collections::HashMap::new();
        assert_eq!(still(6_990_000, 6_990_200, 6_990_200).follow(&mut fresh), VrxAfterMove::Unchanged, "nothing changed");
        assert_eq!(
            still(6_990_000, 6_995_000, 6_985_000).follow(&mut fresh),
            VrxAfterMove::To { hz: 7_160_000, saved: false },
            "a wider filter, out of reach"
        );
        assert_eq!(still(7_100_000, 6_995_000, 6_985_000).follow(&mut fresh), VrxAfterMove::Unchanged, "in reach");
        assert_eq!(still(0, 6_995_000, 6_985_000).follow(&mut fresh), VrxAfterMove::Unchanged, "never set");
        // An estimated centre (no full-band spectrum: the VFO) is looked at
        // only at a bucket change (build 229):
        // DDC really on 7100, VRX on 6935, the VFO from 7100 to 7150.
        let est = |center_hz: u64, last_center_hz: u64, lo_shift: u64, vrx_hz| DdcMove {
            center_hz,
            last_center_hz,
            reach_lo_hz: center_hz - REACH + lo_shift,
            reach_hi_hz: center_hz + REACH,
            last_reach_lo_hz: last_center_hz - REACH,
            last_reach_hi_hz: last_center_hz + REACH,
            measured: false,
            vrx_hz,
            vfo_hz: center_hz,
        };
        let mut fresh = std::collections::HashMap::new();
        assert_eq!(est(7_150_000, 7_100_000, 0, 6_935_000).follow(&mut fresh), VrxAfterMove::Unchanged, "a tuning step");
        assert_eq!(est(7_100_000, 7_100_000, 5_000, 6_935_000).follow(&mut fresh), VrxAfterMove::Unchanged, "a reach change");
        assert_eq!(
            est(7_250_000, 7_150_000, 0, 6_935_000).follow(&mut fresh),
            VrxAfterMove::To { hz: 7_250_000, saved: false },
            "a bucket change still counts"
        );
        // Measured, the same step does move it.
        let mut m = est(7_150_000, 7_100_000, 0, 6_935_000);
        m.measured = true;
        assert_eq!(m.follow(&mut fresh), VrxAfterMove::To { hz: 7_150_000, saved: false });
        // Reach comes from the one guard.
        let src = include_str!("vrx.rs");
        let body = &src[..src.find("#[cfg(test)]").unwrap()];
        assert_eq!(body.matches("let (reach_lo_hz, reach_hi_hz) = self.vrx_tune_limits(ch, center_hz);").count(), 1);
        // Measured: what `vrx_ddc_centre` says (build 231; a centre and a span
        // from a packet in build 229).
        assert_eq!(body.matches("let (center_hz, measured) = self.vrx_ddc_centre(ch);").count(), 1);
        // And remembered per VRX for the next frame (build 228).
        assert_eq!(body.matches("self.last_vrx1_reach_hz = (reach_lo_hz, reach_hi_hz);").count(), 1);
        assert_eq!(body.matches("self.last_vrx2_reach_hz = (reach_lo_hz, reach_hi_hz);").count(), 1);
        // Both VRXs through the one rule.
        let update = include_str!("update.rs");
        assert_eq!(update.matches("self.vrx_follow_ddc_centre(VrxChannel::Vrx1);").count(), 1);
        assert_eq!(update.matches("self.vrx_follow_ddc_centre(VrxChannel::Vrx2);").count(), 1);
        assert!(!update.contains("_freq_by_bucket"), "no second copy");
    }

    /// A lost session does not move a VRX: the
    /// centre falling back from measured to the estimate is no move, and the
    /// width of the reach comes from the server's DDC rate before the row.
    #[test]
    fn a_lost_session_does_not_move_a_vrx() {
        use super::DdcWidth;
        let w = |server_rate_khz, row_span_hz| DdcWidth { server_rate_khz, row_span_hz, row_on: true }.hz();
        assert_eq!(w(768, 384_000), 768_000.0, "the server's rate wins over a stale row");
        assert_eq!(w(0, 768_000), 768_000.0, "a server that has not said it yet: the row");
        assert_eq!(w(0, 0), 384_000.0, "nothing: the guess");
        // A row switched off is no source (build 233).
        assert_eq!(DdcWidth { server_rate_khz: 0, row_span_hz: 768_000, row_on: false }.hz(), 384_000.0);
        let src = include_str!("vrx.rs");
        let body = &src[..src.find("#[cfg(test)]").unwrap()];
        // Hold while away or not yet said; a silent connected server rebases
        // after FALLBACK_HOLD, without a move.
        use super::{FallBack, FallBackInput};
        let fb = |last_measured, measured, connected, secs: Option<u64>| {
            FallBackInput { last_measured, measured, connected, unsaid_for: secs.map(std::time::Duration::from_secs) }.decide()
        };
        assert_eq!(fb(false, false, true, None), FallBack::Follow, "never measured: the estimate's rules");
        assert_eq!(fb(true, true, true, None), FallBack::Follow, "still measured");
        assert_eq!(fb(false, true, true, None), FallBack::Follow, "the server speaks again");
        assert_eq!(fb(true, false, false, None), FallBack::Hold, "a lost session");
        assert_eq!(fb(true, false, false, Some(60)), FallBack::Hold, "away for long: still held");
        assert_eq!(fb(true, false, true, Some(3)), FallBack::Hold, "connected, not said yet");
        assert_eq!(fb(true, false, true, Some(15)), FallBack::Rebase, "a server that stays silent");
        for line in [
            "            FallBack::Hold => return,",
            "self.last_vrx1_measured = measured;",
            "self.last_vrx2_measured = measured;",
            "server_rate_khz: self.ddc_sample_rate_rx1,\n                row_span_hz: self.full_spectrum_span_hz,\n                row_on: self.full_row_live(self.full_row_at),",
            "server_rate_khz: self.ddc_sample_rate_rx2,\n                row_span_hz: self.rx2_full_spectrum_span_hz,\n                row_on: self.full_row_live(self.rx2_full_row_at),",
        ] {
            assert_eq!(body.matches(line).count(), 1, "{line}");
        }
        // The fall back comes before anything moves or is remembered.
        let fall = body.find("FallBack::Hold => return,").unwrap();
        assert!(fall < body.find("}\n        .follow(saved);").unwrap());
        assert!(fall < body.find("self.last_vrx1_ddc_center_hz = center_hz;").unwrap());
    }

    /// The DDC centre of a VRX comes from one place (build 231): the server's
    /// word, then the full-band row (a server before 231), then the VFO as an
    /// estimate. The VRX follow, the VRX window and MIDI all ask it.
    #[test]
    fn a_vrx_knows_its_ddc_centre_without_the_full_band_spectrum() {
        use super::DdcSources;
        let c = |sent_hz, full_hz, full_span_hz, vfo_hz| DdcSources { sent_hz, full_hz, full_span_hz, row_on: true, vfo_hz }.centre();
        // A row switched off is no source, even with a centre and a span left
        // behind (build 233, from a probe on build 232: the row off on 40 m,
        // the radio since on 20 m, the session lost).
        assert_eq!(
            DdcSources { sent_hz: 0, full_hz: 7_100_000, full_span_hz: 384_000, row_on: false, vfo_hz: 14_250_000 }.centre(),
            (14_250_000, false),
            "an old row is no measurement"
        );
        // Switched on, but the RX2 row stopped coming when its windows closed
        // (build 234): no measurement either.
        let stopped = super::super::tuning::RowLive {
            switched_on: true,
            age: Some(std::time::Duration::from_secs(30)),
        };
        assert_eq!(
            DdcSources { sent_hz: 0, full_hz: 7_073_000, full_span_hz: 384_000, row_on: stopped.live(), vfo_hz: 14_250_000 }.centre(),
            (14_250_000, false),
            "a row that no longer comes is no measurement"
        );
        assert_eq!(c(14_282_000, 7_138_000, 384_000, 14_250_000), (14_282_000, true), "the server's word wins over a stale row");
        assert_eq!(c(14_282_000, 0, 0, 14_250_000), (14_282_000, true), "the row off: still measured");
        assert_eq!(c(0, 7_138_000, 384_000, 7_100_000), (7_138_000, true), "a server before 231");
        assert_eq!(c(0, 7_138_000, 0, 7_100_000), (7_100_000, false), "a centre without a span is not a measurement");
        assert_eq!(c(0, 0, 0, 7_100_000), (7_100_000, false), "nothing known: the VFO");
        // The three readers.
        let vrx = include_str!("vrx.rs");
        let vrx = &vrx[..vrx.find("#[cfg(test)]").unwrap()];
        let popouts = include_str!("popouts.rs");
        let midi = include_str!("midi_screen.rs");
        assert_eq!(vrx.matches("self.vrx_ddc_centre(ch);").count(), 1, "the follow");
        // Each VRX asks its own receiver.
        for line in [
            "sent_hz: self.ddc_centre_rx1_hz,
                full_hz: self.full_spectrum_center_hz as u64,
                full_span_hz: self.full_spectrum_span_hz as u64,
                row_on: self.full_row_live(self.full_row_at),
                vfo_hz: self.frequency_hz,",
            "sent_hz: self.ddc_centre_rx2_hz,
                full_hz: self.rx2_full_spectrum_center_hz as u64,
                full_span_hz: self.rx2_full_spectrum_span_hz as u64,
                row_on: self.full_row_live(self.rx2_full_row_at),
                vfo_hz: self.rx2_frequency_hz,",
        ] {
            assert_eq!(vrx.matches(line).count(), 1, "{line}");
        }
        assert_eq!(popouts.matches("let (ddc_center, _) = self.vrx_ddc_centre(ch);").count(), 1, "the VRX window");
        assert_eq!(midi.matches("let (center, _) = self.vrx_ddc_centre(ch);").count(), 1, "MIDI");
        assert!(!midi.contains("full_spectrum_center_hz"), "MIDI has no second source");
    }

    /// The share of the DDC band a VRX may use is one number on both sides:
    /// the server clamps with it in `vrx-rs`, and
    /// the client's reach - which since 226 also decides whether a VRX stays -
    /// mirrors it. The client does not depend on `vrx-rs`, so this reads both.
    #[test]
    fn the_client_and_the_server_use_the_same_usable_share_of_the_ddc_band() {
        fn value(src: &str, decl: &str) -> f64 {
            let at = src.find(decl).unwrap_or_else(|| panic!("{decl}"));
            let rest = &src[at + decl.len()..];
            rest[..rest.find(';').unwrap()].trim().parse().unwrap()
        }
        let server = value(include_str!("../../../vrx-rs/src/runtime.rs"), "const DDC_USABLE_FRACTION: f32 =");
        let client = value(include_str!("vrx.rs"), "const DDC_USABLE_FRACTION: f64 =");
        assert_eq!(server, client);
        let src = include_str!("vrx.rs");
        let body = &src[..src.find("#[cfg(test)]").unwrap()];
        assert_eq!(body.matches("const DDC_USABLE_FRACTION").count(), 1, "one place in the client");
    }
}
