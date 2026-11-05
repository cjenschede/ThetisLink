// SPDX-License-Identifier: GPL-2.0-or-later

use super::*;
use crate::ui::controls::frequency::step_on_grid;

/// Scroll-wheel tuning step of every spectrum and waterfall, in Hz.
///
/// Deliberately fixed: the step used to be derived from the zoom factor, so
/// zooming in silently changed how far one notch tuned. Fine tuning stays
/// available on the frequency digits, which step by the digit under the pointer.
pub(crate) const SPECTRUM_SCROLL_STEP_HZ: u64 = 1_000;

/// The VRX filter's raster on the server: its edges are whole audio bins of
/// 62.5 Hz (`vrx-rs`, `AUDIO_BIN_HZ`). A test holds the two together; the client
/// does not depend on `vrx-rs`.
pub(crate) const VRX_FILTER_STEP_HZ: f64 = 62.5;

/// A VRX filter edge as the server will hear it: on its raster.
pub(crate) fn snap_vrx_filter_hz(hz: f64) -> i32 {
    ((hz / VRX_FILTER_STEP_HZ).round() * VRX_FILTER_STEP_HZ) as i32
}

/// The one description of a spectrum panel, for RX1, RX2, VRX1 and VRX2, in
/// the main window and in a window of its own. What the panels differ in is a
/// field here, not a copy of the drawing code.
pub(crate) struct SpectrumPanel<'a> {
    /// Names the panel's interaction state in egui's memory and its texture.
    pub(crate) id: &'a str,
    /// The bins and the window they cover.
    pub(crate) bins: &'a [u16],
    pub(crate) bins_center_hz: f64,
    pub(crate) bins_span_hz: f64,
    /// The window on screen; its width is the bins' width.
    pub(crate) view_center_hz: f64,
    /// Where the red line and its label stand, and what the filter is relative to.
    pub(crate) marker_hz: u64,
    pub(crate) filter_low_hz: i32,
    pub(crate) filter_high_hz: i32,
    pub(crate) ref_db: f32,
    pub(crate) range_db: f32,
    pub(crate) fill_percent: u8,
    pub(crate) wf_contrast: f32,
    pub(crate) meter: SpectrumMeter,
    /// The large S-meter of a window of its own, or the small one of the main tab.
    pub(crate) big_meter: bool,
    /// RIT marker (RX1 only): the offset when RIT is on.
    pub(crate) rit_hz: Option<i32>,
    /// Band markers (RX1 only).
    pub(crate) band_markers: bool,
    pub(crate) dx_spots: &'a [sdr_remote_logic::state::DxSpotInfo],
    pub(crate) tuning: SpectrumTuning,
}

/// The reading top right.
#[derive(Clone, Copy)]
pub(crate) enum SpectrumMeter {
    /// Receiving: the S reading of this dBm.
    Signal(f32),
    /// This station transmits: watts.
    Transmitting(f32),
    /// Another client transmits: watts.
    OtherTransmits(f32),
}

/// What tuning in a panel may do.
#[derive(Clone, Copy)]
pub(crate) struct SpectrumTuning {
    /// The frequency a scroll notch steps from.
    pub(crate) tune_hz: u64,
    /// Where tuning stops (a VRX stays inside its DDC window).
    pub(crate) min_hz: u64,
    pub(crate) max_hz: u64,
    pub(crate) filter: FilterEdges,
}

/// How a dragged filter edge moves.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum FilterEdges {
    /// To the hertz; the receiver's handler applies the mode's rule
    /// (`filter_width::drag_edge`).
    Free,
    /// On the VRX raster, within ±4 kHz and at least two steps apart: what the
    /// server's VRX filter can be.
    VrxRaster,
}

/// What the operator did in a panel this frame.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum SpectrumAction {
    /// Tune to this frequency: a click, a drag or a scroll notch.
    Tune { hz: u64, how: TuneHow },
    /// A filter edge dragged to this offset from the marker.
    FilterEdge { low: bool, hz: i32 },
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum TuneHow {
    Click,
    Drag,
    Scroll,
}

impl SpectrumTuning {
    /// The upper limit as `step_on_grid` can take it (it works in i64).
    fn top(&self) -> u64 {
        self.max_hz.min(i64::MAX as u64)
    }

    /// A click lands on the nearest kHz, inside the limits.
    pub(crate) fn click(&self, hz: f64) -> u64 {
        step_on_grid(hz.max(0.0) as u64, 0, SPECTRUM_SCROLL_STEP_HZ, self.min_hz, self.top())
    }

    /// A drag follows the pointer in steps of 100 Hz, inside the limits.
    pub(crate) fn drag(&self, hz: f64) -> u64 {
        ((hz.max(0.0) as u64 / 100) * 100).clamp(self.min_hz, self.max_hz)
    }

    /// One notch: the next kHz up or down, inside the limits.
    pub(crate) fn scroll(&self, up: bool) -> u64 {
        let step = SPECTRUM_SCROLL_STEP_HZ as i64;
        step_on_grid(self.tune_hz, if up { step } else { -step }, SPECTRUM_SCROLL_STEP_HZ, self.min_hz, self.top())
    }

    /// A filter edge dragged to `offset_hz` from the marker.
    pub(crate) fn filter_edge(&self, low: bool, offset_hz: f64, low_hz: i32, high_hz: i32) -> i32 {
        match self.filter {
            FilterEdges::Free => offset_hz.round() as i32,
            FilterEdges::VrxRaster => {
                let gap = (2.0 * VRX_FILTER_STEP_HZ) as i32;
                let snapped = snap_vrx_filter_hz(offset_hz);
                if low { snapped.min(high_hz - gap).max(-4000) } else { snapped.max(low_hz + gap).min(4000) }
            }
        }
    }
}

/// The scroll notch over a panel this frame: up, down or none. Not when the
/// frequency digits took the wheel already.
fn scroll_notch(ui: &egui::Ui, response: &egui::Response) -> Option<bool> {
    let consumed: bool = ui.memory(|mem| mem.data.get_temp(egui::Id::new("freq_scroll_consumed")).unwrap_or(false));
    if !response.hovered() || consumed {
        return None;
    }
    let scroll = ui.input(|i| i.raw_scroll_delta.y);
    (scroll.abs() > 0.1).then_some(scroll > 0.0)
}

/// "Nice" grid steps (1, 2, 5 × 10^n) for about `lines` lines over `span`.
fn nice_step(span: f64, lines: f64) -> f64 {
    let raw = span / lines;
    let pow = 10.0f64.powf(raw.log10().floor());
    let n = raw / pow;
    let nice = if n < 1.5 { 1.0 } else if n < 3.5 { 2.0 } else if n < 7.5 { 5.0 } else { 10.0 };
    nice * pow
}

/// The span label: tenths of a kHz below 100, hundredths below 1.
fn span_label(span_khz: f64) -> String {
    if span_khz < 1.0 {
        format!("{:.2} kHz", span_khz)
    } else if span_khz < 100.0 {
        format!("{:.1} kHz", span_khz)
    } else {
        format!("{:.0} kHz", span_khz)
    }
}

/// The reference level a panel's rows allow, dB. MIDI keeps to the same.
pub(crate) const REF_DB_RANGE: std::ops::RangeInclusive<f32> = -90.0..=0.0;

/// The range (dB from the reference to the bottom) a panel's rows allow.
pub(crate) const RANGE_DB_RANGE: std::ops::RangeInclusive<f32> = 20.0..=130.0;

/// The waterfall contrast a panel's rows allow. MIDI keeps to the same.
pub(crate) const WF_CONTRAST_RANGE: std::ops::RangeInclusive<f32> = 0.3..=3.0;

/// The zoom a panel's rows allow: from 1x only with zoom below 2x allowed (one
/// flag for every panel, and the server keeps to the strictest client). MIDI
/// keeps to the same.
pub(crate) fn zoom_range(allow_zoom_below_2x: bool) -> std::ops::RangeInclusive<f32> {
    (if allow_zoom_below_2x { 1.0 } else { 2.0 })..=1024.0
}

/// The pan a zoom allows, either way from the centre. MIDI keeps to the same.
pub(crate) fn pan_limit(zoom: f32) -> f32 {
    crate::ui::tuning::max_pan_fraction(zoom)
}

/// The view settings of one panel, as the two rows above every spectrum show
/// them: Ref, Auto, Range, Fill; Zoom, Zoom <2x, Pan, Waterfall, FFT (and the
/// height of the main tab's panel).
pub(crate) struct ViewControls<'a> {
    pub(crate) ref_db: &'a mut f32,
    pub(crate) auto_ref: &'a mut bool,
    pub(crate) range_db: &'a mut f32,
    pub(crate) fill_percent: &'a mut u8,
    pub(crate) zoom: &'a mut f32,
    pub(crate) allow_zoom_below_2x: &'a mut bool,
    pub(crate) pan: &'a mut f32,
    pub(crate) wf_contrast: &'a mut f32,
    pub(crate) fft: FftControl<'a>,
    /// The main tab's panel height; a window of its own fills itself (None).
    pub(crate) height: Option<&'a mut f32>,
}

/// The sizes the FFT row offers at this sample rate, in K: from 32K up to the
/// server's limit, none above four times the rate.
fn fft_sizes_k(ddc_rate_hz: u32) -> Vec<u16> {
    std::iter::successors(Some(32u16), |k| k.checked_mul(2))
        .take_while(|&k| k <= sdr_remote_core::MAX_FFT_SIZE_K)
        .filter(|&k| k as u32 * 1024 <= ddc_rate_hz * 4)
        .collect()
}

/// The FFT size field.
pub(crate) enum FftControl<'a> {
    /// The panel's own receiver: the operator picks.
    Choose { size_k: &'a mut u16, ddc_rate_hz: u32, salt: &'a str },
    /// A VRX: the FFT of the receiver it rides on the server, shown.
    Shared { size_k: u16, ddc_rate_hz: u32, rx_label: &'a str },
}

/// What changed in the rows this frame; the panel acts on it.
#[derive(Default, Clone, Copy, PartialEq, Debug)]
pub(crate) struct ViewChanges {
    pub(crate) ref_db: bool,
    /// Auto switched; `auto_now` says to what.
    pub(crate) auto_ref: bool,
    pub(crate) range_db: bool,
    pub(crate) fill: bool,
    pub(crate) zoom: bool,
    pub(crate) allow_zoom_below_2x: bool,
    pub(crate) pan: bool,
    pub(crate) wf_contrast: bool,
    pub(crate) fft: Option<u16>,
    pub(crate) height: bool,
}

impl ViewChanges {
    /// Whether the auto reference has to start again: switched on, or the
    /// range moved under it.
    pub(crate) fn restart_auto_ref(&self, auto_now: bool) -> bool {
        auto_now && (self.auto_ref || self.range_db)
    }

    /// Whether anything that is kept in the settings changed.
    pub(crate) fn settings(&self) -> bool {
        self.ref_db || self.auto_ref || self.range_db || self.fill || self.wf_contrast || self.fft.is_some() || self.height
    }
}

/// The two rows of view settings above every spectrum panel.
pub(crate) fn spectrum_view_rows(ui: &mut egui::Ui, c: ViewControls) -> ViewChanges {
    let mut ch = ViewChanges::default();
    let ViewControls { ref_db, auto_ref, range_db, fill_percent, zoom, allow_zoom_below_2x, pan, wf_contrast, fft, height } = c;
    ui.horizontal(|ui| {
        ui.spacing_mut().slider_width = super::theme::TL_SLIDER_WIDTH;
        ui.label(rust_i18n::t!("main_ref_label").to_string());
        if *auto_ref {
            let mut shown = *ref_db;
            ui.add_enabled(false, egui::Slider::new(&mut shown, REF_DB_RANGE).suffix(" dB").step_by(5.0))
                .on_disabled_hover_text(rust_i18n::t!("main_hover_ref").to_string());
        } else {
            let resp = ui.add(egui::Slider::new(ref_db, REF_DB_RANGE).suffix(" dB").step_by(5.0))
                .on_hover_text(rust_i18n::t!("main_hover_ref").to_string());
            let scrolled = helpers::slider_wheel(ui, &resp, ref_db, REF_DB_RANGE, 5.0);
            ch.ref_db = resp.changed() || scrolled;
        }
        ch.auto_ref = ui.checkbox(auto_ref, rust_i18n::t!("main_auto").to_string())
            .on_hover_text(rust_i18n::t!("main_hover_auto_ref").to_string())
            .changed();
        ui.label(rust_i18n::t!("main_range_label").to_string());
        let resp = ui.add(egui::Slider::new(range_db, RANGE_DB_RANGE).suffix(" dB").step_by(5.0))
            .on_hover_text(rust_i18n::t!("main_hover_range").to_string());
        let scrolled = helpers::slider_wheel(ui, &resp, range_db, RANGE_DB_RANGE, 5.0);
        ch.range_db = resp.changed() || scrolled;
        ch.fill = helpers::spectrum_fill_field(ui, fill_percent);
    });
    ui.horizontal(|ui| {
        ui.spacing_mut().slider_width = super::theme::TL_SLIDER_WIDTH;
        ui.label(rust_i18n::t!("main_zoom_label").to_string());
        // Zoom below 2x only when allowed (TL2-1 ctun-auto-recenter).
        let zooms = zoom_range(*allow_zoom_below_2x);
        // A zoom from outside the rows (the opening zoom of a band) pulled into
        // range is a change like any other, so the server hears it.
        let before = *zoom;
        *zoom = zoom.clamp(*zooms.start(), *zooms.end());
        let pulled_in = *zoom != before;
        let resp = ui.add(egui::Slider::new(zoom, zooms.clone()).logarithmic(true).custom_formatter(|v, _| format!("{:.0}x", v)))
            .on_hover_text(rust_i18n::t!("main_hover_zoom").to_string());
        let step = (*zoom as f64 * 0.1).max(1.0);
        let scrolled = helpers::slider_wheel(ui, &resp, zoom, zooms, step);
        ch.zoom = resp.changed() || scrolled || pulled_in;
        let max_pan = pan_limit(*zoom);
        if ch.zoom {
            *pan = pan.clamp(-max_pan, max_pan);
        }
        ch.allow_zoom_below_2x = ui.checkbox(allow_zoom_below_2x, rust_i18n::t!("main_allow_zoom_2x").to_string())
            .on_hover_text(rust_i18n::t!("hover_zoom_below_2x").to_string())
            .changed();
        ui.label(rust_i18n::t!("main_pan_label").to_string());
        let resp = ui.add(egui::Slider::new(pan, -max_pan..=max_pan).custom_formatter(|v, _| format!("{:+.2}", v)))
            .on_hover_text(rust_i18n::t!("main_hover_pan").to_string());
        let scrolled = helpers::slider_wheel(ui, &resp, pan, -max_pan..=max_pan, (max_pan as f64 * 0.1).max(0.0001));
        ch.pan = resp.changed() || scrolled;
        ui.label(rust_i18n::t!("main_wf_label").to_string());
        let resp = ui.add(egui::Slider::new(wf_contrast, WF_CONTRAST_RANGE).logarithmic(true).custom_formatter(|v, _| format!("{:.1}", v)))
            .on_hover_text(rust_i18n::t!("main_hover_wf").to_string());
        let scrolled = helpers::slider_wheel(ui, &resp, wf_contrast, WF_CONTRAST_RANGE, 0.1);
        ch.wf_contrast = resp.changed() || scrolled;
        match fft {
            FftControl::Choose { size_k, ddc_rate_hz, salt } => {
                let auto_k = sdr_remote_core::ddc_fft_size(ddc_rate_hz) / 1024;
                let label = if *size_k == 0 { format!("FFT: Auto ({}K)", auto_k) } else { format!("FFT: {}K", size_k) };
                // FFTs a second at each size, at this sample rate.
                let per_s = |k: u32| ddc_rate_hz / (k * 1024 / 8);
                let mut options = vec![(0u16, format!("Auto ({}K, ~{} FFT/s)", auto_k, per_s(auto_k as u32)))];
                for k in fft_sizes_k(ddc_rate_hz) {
                    options.push((k, format!("{}K (~{} FFT/s)", k, per_s(k as u32))));
                }
                egui::ComboBox::from_id_salt(salt).selected_text(&label).width(80.0).show_ui(ui, |ui| {
                    for (k, text) in &options {
                        if ui.selectable_label(*size_k == *k, text).clicked() {
                            *size_k = *k;
                            ch.fft = Some(*k);
                        }
                    }
                });
            }
            FftControl::Shared { size_k, ddc_rate_hz, rx_label } => {
                let auto_k = sdr_remote_core::ddc_fft_size(ddc_rate_hz) / 1024;
                let label = if size_k == 0 { format!("FFT: Auto ({}K)", auto_k) } else { format!("FFT: {}K", size_k) };
                ui.add_enabled(false, egui::Button::new(&label))
                    .on_disabled_hover_text(rust_i18n::t!("main_hover_server_wide_fft", rx = rx_label).to_string());
            }
        }
        if let Some(h) = height {
            ui.label("H:");
            let resp = ui.add(egui::Slider::new(h, 300.0..=1200.0).custom_formatter(|v, _| format!("{:.0}", v)))
                .on_hover_text(rust_i18n::t!("main_hover_spectrum_height").to_string());
            let scrolled = helpers::slider_wheel(ui, &resp, h, 300.0..=1200.0, 20.0);
            ch.height = resp.changed() || scrolled;
        }
    });
    super::helpers::rule(ui);
    ch
}

/// An RX panel's action, put where the RX handlers read it
/// (`handle_rx1_spectrum_keys`, `handle_rx2_spectrum_keys`): they run in the
/// viewport that drew the panel, which may be a window of its own. `prefix` is
/// "" for RX1 and "rx2_" for RX2.
pub(crate) fn store_rx_action(ui: &egui::Ui, prefix: &str, action: Option<SpectrumAction>) {
    match action {
        Some(SpectrumAction::Tune { hz, how }) => {
            let key = match how {
                TuneHow::Click => "click",
                TuneHow::Drag => "drag",
                TuneHow::Scroll => "scroll",
            };
            ui.memory_mut(|mem| mem.data.insert_temp(egui::Id::new(format!("{prefix}spectrum_{key}_freq")), hz));
        }
        Some(SpectrumAction::FilterEdge { low, hz }) => {
            let key = if low { "low" } else { "high" };
            ui.memory_mut(|mem| mem.data.insert_temp(egui::Id::new(format!("{prefix}spectrum_filter_{key}")), hz));
        }
        None => {}
    }
}

/// The fill under every spectrum curve, one value for all panels.
pub(crate) const SPECTRUM_FILL_DEFAULT: u8 = 15;

/// The filter passband behind the curve, its two edges and the VFO line: the
/// same colours as on the phone (`SpectrumView.kt`), and mixed the same way,
/// through `compose_colour`.
pub(crate) const FILTER_BAND_RGB: [u8; 3] = [25, 30, 55];
pub(crate) const FILTER_EDGE_RGBA: [u8; 4] = [200, 200, 0, 120];
pub(crate) const VFO_LINE_RGBA: [u8; 4] = [255, 50, 50, 180];

/// The decimals of a frequency in MHz that show a step of `step_hz`: none for
/// 1 MHz, one more per decade below it, six (1 Hz) at most.
pub(crate) fn mhz_decimals(step_hz: f64) -> usize {
    let mut d = 0;
    let mut unit = 1_000_000.0;
    while d < 6 && step_hz < unit * 0.999 {
        unit /= 10.0;
        d += 1;
    }
    d
}

/// The decimals of the VFO label: as the axis, but at least kHz.
pub(crate) fn vfo_mhz_decimals(tick_spacing_hz: f64) -> usize {
    mhz_decimals(tick_spacing_hz).max(3)
}

/// A colour with an opacity as the phone's Compose draws it: premultiplied in
/// gamma, so the alpha is the share of the colour on screen.
/// `Color32::from_rgba_unmultiplied` premultiplies in linear light, and the
/// screen then blends in gamma: an alpha of 2% added 15% of the colour, 15%
/// added 42%; `from_rgba_premultiplied` with the plain colour adds all of it.
pub(crate) fn compose_colour([r, g, b, a]: [u8; 4]) -> Color32 {
    let mul = |c: u8| ((c as u32 * a as u32 + 127) / 255) as u8;
    Color32::from_rgba_premultiplied(mul(r), mul(g), mul(b), a)
}

/// The fill of one column: the colour of its peak, from the top of the curve
/// to the bottom of the plot, at `percent` opacity, so 15 looks the same as on
/// the phone.
pub(crate) fn spectrum_fill_colour(r: u8, g: u8, b: u8, percent: u8) -> Color32 {
    compose_colour([r, g, b, (percent.min(100) as u32 * 255 / 100) as u8])
}

/// The strip with the frequency labels under every spectrum plot - RX and VRX
/// alike (build 241; it was 18 px on RX and 16 on VRX).
pub(crate) const PLOT_LABEL_H: f32 = 18.0;

/// Plot and waterfall heights that together fill `available`, for every
/// spectrum panel - RX1, RX2 and VRX (build 241). The label strip under the plot
/// and the gap between plot and waterfall (`gap`, the panel's item spacing) come
/// off first, and the rest is split evenly. Until 241 the RX panels split all of
/// it, and ran ~20 px past their window: the waterfall lost its bottom edge.
pub(crate) fn plot_and_waterfall_heights(available: f32, gap: f32) -> (f32, f32) {
    let each = ((available - PLOT_LABEL_H - gap) / 2.0).max(40.0);
    (each, each)
}

/// The waterfall's window and the width of a row that does not say its own.
#[derive(Clone, Copy)]
pub(crate) struct WaterfallWindow {
    pub(crate) center_hz: f64,
    pub(crate) span_hz: f64,
    pub(crate) row_span_hz: f64,
}

impl WaterfallRingBuffer {
    /// The width the waterfall shows: that of the newest view row, as the
    /// server cut it (its integer bin count can differ slightly from the DDC
    /// over the zoom); without one, the newest wide row; else `fallback`.
    pub(crate) fn display_span_hz(&self, fallback: f64) -> f64 {
        if self.height == 0 {
            return fallback;
        }
        let newest = (self.write_idx + self.height - 1) % self.height;
        let view = self.view_spans[newest] as f64;
        let full = self.full_spans[newest] as f64;
        if view > 0.0 {
            view
        } else if full > 0.0 {
            full
        } else {
            fallback
        }
    }
}

/// A spectrum panel's plot and waterfall, one under the other: what every RX
/// and VRX panel draws. Returns what the operator did in either.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spectrum_panel_view(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    panel: &SpectrumPanel,
    wf: &WaterfallRingBuffer,
    wf_window: WaterfallWindow,
    texture: &mut Option<egui::TextureHandle>,
    plot_height: f32,
    wf_height: f32,
) -> Option<SpectrumAction> {
    let from_plot = spectrum_plot(ui, panel, plot_height);
    let from_wf = spectrum_waterfall(ui, ctx, panel, wf, wf_window, texture, wf_height);
    from_plot.or(from_wf)
}

/// The spectrum plot of a panel: grid, passband, curve and fill, red line and
/// label, RIT, DX spots, labels, band markers, S-meter and span, and the
/// operator's click, drag and scroll on it.
pub(crate) fn spectrum_plot(ui: &mut egui::Ui, panel: &SpectrumPanel, plot_height: f32) -> Option<SpectrumAction> {
    let width = ui.available_width();
    let plot_height = plot_height.max(40.0);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, plot_height + PLOT_LABEL_H), egui::Sense::click_and_drag());
    let response = response.on_hover_text(rust_i18n::t!("spec_hover").to_string());
    let plot_rect = egui::Rect::from_min_max(rect.min, Pos2::new(rect.max.x, rect.max.y - PLOT_LABEL_H));
    let label_strip = egui::Rect::from_min_max(Pos2::new(rect.min.x, plot_rect.max.y), rect.max);
    let bg_color = Color32::from_rgba_premultiplied(10, 15, 30, 220);

    let visible_span = panel.bins_span_hz;
    let num_bins = panel.bins.len();
    if num_bins == 0 || visible_span <= 0.0 {
        if ui.is_rect_visible(rect) {
            let painter = ui.painter_at(rect);
            painter.rect_filled(plot_rect, 2.0, Color32::from_rgb(10, 15, 30));
            painter.rect_filled(label_strip, 0.0, Color32::from_rgb(18, 22, 40));
            painter.text(
                plot_rect.center(),
                egui::Align2::CENTER_CENTER,
                rust_i18n::t!("spectrum_not_received").to_string(),
                egui::FontId::proportional(11.0),
                Color32::GRAY,
            );
        }
        return None;
    }
    let start_hz = panel.view_center_hz - visible_span / 2.0;
    let end_hz = start_hz + visible_span;
    let bins_start_hz = panel.bins_center_hz - visible_span / 2.0;
    let ref_db = panel.ref_db;
    let range_db = panel.range_db;
    let floor_db = ref_db - range_db;
    let x_of = |hz: f64| rect.min.x + ((hz - start_hz) / visible_span) as f32 * rect.width();
    let hz_of = |x: f32| start_hz + ((x - rect.min.x) / rect.width()) as f64 * visible_span;
    let marker = panel.marker_hz as f64;

    let tick_spacing_hz = nice_step(visible_span, 7.0);
    let db_spacing = nice_step(range_db as f64, 6.0) as f32;
    let first_tick = (start_hz / tick_spacing_hz).ceil() as i64;
    let last_tick = (end_hz / tick_spacing_hz).floor() as i64;
    let first_db_tick = (floor_db / db_spacing).ceil() as i32;
    let last_db_tick = (ref_db / db_spacing).floor() as i32;

    if ui.is_rect_visible(rect) {
        let painter = ui.painter_at(rect);
        painter.rect_filled(plot_rect, 2.0, Color32::from_rgb(10, 15, 30));
        painter.rect_filled(label_strip, 0.0, Color32::from_rgb(18, 22, 40));

        // ── Grid lines (behind everything) ──
        let grid_stroke = Stroke::new(1.0, Color32::from_rgb(60, 60, 85));
        for tick_idx in first_tick..=last_tick {
            let freq = tick_idx as f64 * tick_spacing_hz;
            let frac = (freq - start_hz) / visible_span;
            if frac < 0.01 || frac > 0.99 { continue; }
            let x = x_of(freq);
            painter.line_segment([Pos2::new(x, plot_rect.min.y), Pos2::new(x, plot_rect.max.y)], grid_stroke);
            painter.line_segment(
                [Pos2::new(x, label_strip.min.y), Pos2::new(x, label_strip.min.y + 4.0)],
                Stroke::new(1.0, Color32::from_rgb(80, 80, 110)),
            );
        }
        for db_idx in first_db_tick..=last_db_tick {
            let db = db_idx as f32 * db_spacing;
            let frac = (ref_db - db) / range_db;
            if frac < 0.02 || frac > 0.98 { continue; }
            let y = plot_rect.min.y + frac * plot_rect.height();
            painter.line_segment([Pos2::new(plot_rect.min.x, y), Pos2::new(plot_rect.max.x, y)], grid_stroke);
        }

        // ── Filter passband ──
        if panel.marker_hz > 0 && (panel.filter_low_hz != 0 || panel.filter_high_hz != 0) {
            let lo_x = x_of(marker + panel.filter_low_hz as f64).max(rect.min.x);
            let hi_x = x_of(marker + panel.filter_high_hz as f64).min(rect.max.x);
            if hi_x > lo_x {
                painter.rect_filled(
                    egui::Rect::from_min_max(Pos2::new(lo_x, plot_rect.min.y), Pos2::new(hi_x, plot_rect.max.y)),
                    0.0,
                    Color32::from_rgb(FILTER_BAND_RGB[0], FILTER_BAND_RGB[1], FILTER_BAND_RGB[2]),
                );
                let edge_stroke = Stroke::new(0.5, compose_colour(FILTER_EDGE_RGBA));
                if lo_x > rect.min.x {
                    painter.line_segment([Pos2::new(lo_x, plot_rect.min.y), Pos2::new(lo_x, plot_rect.max.y)], edge_stroke);
                }
                if hi_x < rect.max.x {
                    painter.line_segment([Pos2::new(hi_x, plot_rect.min.y), Pos2::new(hi_x, plot_rect.max.y)], edge_stroke);
                }
            }
        }

        // ── The curve: the strongest bin under each pixel ──
        let pixel_count = plot_rect.width().max(1.0) as usize;
        let hz_per_bin = visible_span / num_bins as f64;
        let points: Vec<(Pos2, f32)> = (0..pixel_count).map(|px| {
            let x = plot_rect.min.x + px as f32;
            let freq0 = start_hz + (px as f64 / pixel_count as f64) * visible_span;
            let freq1 = start_hz + ((px + 1) as f64 / pixel_count as f64) * visible_span;
            let b0 = ((freq0 - bins_start_hz) / hz_per_bin).max(0.0);
            let b1 = ((freq1 - bins_start_hz) / hz_per_bin).max(0.0);
            let bs = b0.floor() as usize;
            let be = (b1.ceil() as usize).min(num_bins).max(bs + 1);
            let mut max_val = 0u16;
            if bs < num_bins {
                for i in bs..be.min(num_bins) {
                    max_val = max_val.max(panel.bins[i]);
                }
            }
            let db = sdr_remote_core::unpack_spectrum_db(max_val);
            let frac = (ref_db - db) / range_db;
            let y = plot_rect.min.y + frac * plot_rect.height();
            (Pos2::new(x, y.clamp(plot_rect.min.y, plot_rect.max.y)), frac.clamp(0.0, 1.0))
        }).collect();

        // ── The frequency label, behind the curve; the line on top, with a gap ──
        let mut marker_text_rect: Option<egui::Rect> = None;
        let mut marker_x: Option<f32> = None;
        if panel.marker_hz > 0 && (start_hz..=end_hz).contains(&marker) {
            let x = x_of(marker);
            marker_x = Some(x);
            let mhz = marker / 1_000_000.0;
            let text = format!("{:.*}", vfo_mhz_decimals(tick_spacing_hz), mhz);
            let color = Color32::from_rgb(255, 120, 120);
            let galley = painter.layout_no_wrap(text, egui::FontId::proportional(24.0), color);
            let text_pos = egui::Align2::CENTER_TOP.anchor_size(Pos2::new(x, plot_rect.min.y + 2.0), galley.size());
            let bg_rect = text_pos.expand(2.0);
            marker_text_rect = Some(bg_rect);
            painter.rect_filled(bg_rect, 2.0, bg_color);
            painter.galley(text_pos.min, galley, color);
        }

        if points.len() >= 2 {
            let bottom_y = plot_rect.max.y;
            let mut mesh = egui::Mesh::default();
            for (i, (pt, frac)) in points.iter().enumerate() {
                let uv = egui::epaint::WHITE_UV;
                let (r, g, b) = spectrum_level_color(1.0 - *frac);
                let fill_color = spectrum_fill_colour(r, g, b, panel.fill_percent);
                mesh.vertices.push(egui::epaint::Vertex { pos: *pt, uv, color: fill_color });
                mesh.vertices.push(egui::epaint::Vertex { pos: Pos2::new(pt.x, bottom_y), uv, color: fill_color });
                if i > 0 {
                    let base = (i as u32 - 1) * 2;
                    mesh.indices.extend_from_slice(&[base, base + 1, base + 2]);
                    mesh.indices.extend_from_slice(&[base + 1, base + 3, base + 2]);
                }
            }
            painter.add(egui::Shape::Mesh(mesh));
            for i in 1..points.len() {
                let (p0, f0) = &points[i - 1];
                let (p1, f1) = &points[i];
                let (r, g, b) = spectrum_level_color(1.0 - (f0 + f1) / 2.0);
                painter.line_segment([*p0, *p1], Stroke::new(1.5, Color32::from_rgb(r, g, b)));
            }
        }

        if let Some(x) = marker_x {
            let stroke = Stroke::new(2.0, compose_colour(VFO_LINE_RGBA));
            if let Some(tr) = marker_text_rect {
                if plot_rect.min.y < tr.min.y {
                    painter.line_segment([Pos2::new(x, plot_rect.min.y), Pos2::new(x, tr.min.y)], stroke);
                }
                painter.line_segment([Pos2::new(x, tr.max.y), Pos2::new(x, plot_rect.max.y)], stroke);
            } else {
                painter.line_segment([Pos2::new(x, plot_rect.min.y), Pos2::new(x, plot_rect.max.y)], stroke);
            }
        }

        // ── RIT marker ──
        if let Some(rit) = panel.rit_hz.filter(|r| *r != 0) {
            let rit_hz = marker + rit as f64;
            if panel.marker_hz > 0 && (start_hz..=end_hz).contains(&rit_hz) {
                let x = x_of(rit_hz);
                let rit_color = compose_colour([0, 200, 255, 160]);
                let mut y = plot_rect.min.y;
                while y < plot_rect.max.y {
                    let y_end = (y + 6.0).min(plot_rect.max.y);
                    painter.line_segment([Pos2::new(x, y), Pos2::new(x, y_end)], Stroke::new(1.5, rit_color));
                    y += 10.0;
                }
                let galley = painter.layout_no_wrap(format!("RIT {:+}", rit), egui::FontId::proportional(11.0), rit_color);
                painter.galley(Pos2::new(x + 3.0, plot_rect.min.y + 30.0), galley, rit_color);
            }
        }

        // ── DX spots: slide from top to 3/4 height over their lifetime, fade in the last 20% ──
        if !panel.dx_spots.is_empty() {
            let spot_font = egui::FontId::new(13.0, egui::FontFamily::Monospace);
            let now = std::time::Instant::now();
            let plot_h = plot_rect.height();
            for spot in panel.dx_spots {
                let f = spot.frequency_hz as f64;
                if f < start_hz || f > end_hz { continue; }
                let x = x_of(f);
                let expiry = spot.expiry_seconds.max(60) as f32;
                let total_age = spot.age_seconds as f32 + now.duration_since(spot.received).as_secs_f32();
                let age_frac = (total_age / expiry).clamp(0.0, 1.0);
                let alpha = if age_frac > 0.8 { ((1.0 - age_frac) / 0.2).clamp(0.0, 1.0) } else { 1.0 };
                let alpha_u8 = (alpha * 200.0) as u8;
                if alpha_u8 == 0 { continue; }
                let [r, g, b, _] = sdr_remote_logic::state::mode_color_rgba(&spot.mode, 255);
                let scaled = |a: u8| Color32::from_rgba_premultiplied(
                    (r as u16 * a as u16 / 255) as u8,
                    (g as u16 * a as u16 / 255) as u8,
                    (b as u16 * a as u16 / 255) as u8,
                    a,
                );
                let color = scaled(alpha_u8);
                let line_color = scaled(alpha_u8 / 2);
                let label_y = plot_rect.min.y + 2.0 + age_frac * plot_h * 0.75;
                let mut y = plot_rect.min.y;
                while y < plot_rect.max.y {
                    let end_y = (y + 4.0).min(plot_rect.max.y);
                    painter.line_segment([Pos2::new(x, y), Pos2::new(x, end_y)], Stroke::new(1.0, line_color));
                    y += 7.0;
                }
                let galley = painter.layout_no_wrap(spot.callsign.clone(), spot_font.clone(), color);
                let text_pos = Pos2::new(x + 3.0, label_y);
                let bg_rect = egui::Rect::from_min_size(text_pos, galley.size()).expand(2.0);
                painter.rect_filled(bg_rect, 2.0, Color32::from_rgba_premultiplied(10, 15, 30, (alpha * 220.0) as u8));
                painter.galley(text_pos, galley, color);
            }
        }

        // ── Labels: frequency under the plot, dB at the left edge ──
        let grid_font = egui::FontId::proportional(11.0);
        let grid_color = Color32::from_rgb(200, 200, 210);
        let freq_label_color = Color32::from_rgb(220, 220, 230);
        for tick_idx in first_tick..=last_tick {
            let freq = tick_idx as f64 * tick_spacing_hz;
            let frac = (freq - start_hz) / visible_span;
            if frac < 0.02 || frac > 0.98 { continue; }
            let label = format!("{:.*}", mhz_decimals(tick_spacing_hz), freq / 1_000_000.0);
            painter.text(
                Pos2::new(x_of(freq), label_strip.min.y + label_strip.height() / 2.0),
                egui::Align2::CENTER_CENTER,
                label,
                grid_font.clone(),
                freq_label_color,
            );
        }
        for db_idx in first_db_tick..=last_db_tick {
            let db = db_idx as f32 * db_spacing;
            let frac = (ref_db - db) / range_db;
            if frac < 0.02 || frac > 0.98 { continue; }
            let y = plot_rect.min.y + frac * plot_rect.height();
            let galley = painter.layout_no_wrap(format!("{:.0}", db), grid_font.clone(), grid_color);
            let text_pos = egui::Align2::LEFT_TOP.anchor_size(Pos2::new(plot_rect.min.x + 2.0, y + 1.0), galley.size());
            painter.rect_filled(text_pos.expand(1.0), 1.0, bg_color);
            painter.galley(text_pos.min, galley, grid_color);
        }

        // ── Band markers (RX1) ──
        if panel.band_markers {
            let bands: &[(f64, &str)] = &[
                (1.8e6, "160m"), (3.5e6, "80m"), (7.0e6, "40m"), (10.1e6, "30m"),
                (14.0e6, "20m"), (18.068e6, "17m"), (21.0e6, "15m"), (24.89e6, "12m"),
                (28.0e6, "10m"), (50.0e6, "6m"),
            ];
            let band_font = egui::FontId::proportional(10.0);
            for &(freq, label) in bands {
                if freq < start_hz || freq > end_hz { continue; }
                let x = x_of(freq);
                painter.line_segment(
                    [Pos2::new(x, plot_rect.max.y - 26.0), Pos2::new(x, plot_rect.max.y - 14.0)],
                    Stroke::new(1.0, Color32::from_rgb(100, 80, 40)),
                );
                let band_color = Color32::from_rgb(170, 140, 70);
                let galley = painter.layout_no_wrap(label.to_string(), band_font.clone(), band_color);
                let text_pos = egui::Align2::CENTER_BOTTOM.anchor_size(Pos2::new(x, plot_rect.max.y - 27.0), galley.size());
                painter.rect_filled(text_pos.expand(1.0), 1.0, bg_color);
                painter.galley(text_pos.min, galley, band_color);
            }
        }

        // ── S-meter or TX power, top right ──
        let (meter_text, s_reading) = match panel.meter {
            SpectrumMeter::Signal(dbm) => (sdr_remote_core::s_meter::text(dbm), true),
            SpectrumMeter::Transmitting(w) => (format!("TX: {:.0}W", w), false),
            SpectrumMeter::OtherTransmits(w) => (rust_i18n::t!("spec_tx_in_use", w = format!("{:.0}", w)).to_string(), false),
        };
        let meter_color = if s_reading { Color32::from_rgb(0, 220, 0) } else { Color32::from_rgb(255, 80, 80) };
        let meter_font = egui::FontId::proportional(if panel.big_meter { 42.0 } else { 21.0 });
        let galley = painter.layout_no_wrap(meter_text, meter_font.clone(), meter_color);
        let right = plot_rect.max.x - 4.0;
        let text_pos = if s_reading {
            let left = s_meter_left(&painter, &meter_font, right, galley.size().x);
            egui::Rect::from_min_size(Pos2::new(left, plot_rect.min.y + 4.0), galley.size())
        } else {
            egui::Align2::RIGHT_TOP.anchor_size(Pos2::new(right, plot_rect.min.y + 4.0), galley.size())
        };
        painter.rect_filled(text_pos.expand(2.0), 2.0, bg_color);
        painter.galley(text_pos.min, galley, meter_color);

        // ── Span, top left under the frequency (not when it is the whole band) ──
        let span_khz = visible_span / 1000.0;
        if span_khz < 1536.0 {
            let span_color = Color32::from_rgb(220, 220, 80);
            let galley = painter.layout_no_wrap(span_label(span_khz), egui::FontId::proportional(11.0), span_color);
            let text_pos = egui::Align2::LEFT_TOP.anchor_size(Pos2::new(plot_rect.min.x + 2.0, plot_rect.min.y + 14.0), galley.size());
            painter.rect_filled(text_pos.expand(2.0), 2.0, bg_color);
            painter.galley(text_pos.min, galley, span_color);
        }
    }

    // ── Interaction ──
    let edge_x = |offset: i32| {
        let hz = marker + offset as f64;
        (panel.marker_hz > 0 && (start_hz..=end_hz).contains(&hz)).then(|| x_of(hz))
    };
    let filter_lo_x = edge_x(panel.filter_low_hz);
    let filter_hi_x = edge_x(panel.filter_high_hz);
    let grab_dist = 8.0;
    let cursor_dist = 3.0;
    let hover_pos = response.hover_pos().filter(|p| plot_rect.contains(*p));
    let near = |edge: Option<f32>, dist: f32| hover_pos.and_then(|p| edge.map(|x| (p.x - x).abs() < dist)).unwrap_or(false);
    let near_lo = near(filter_lo_x, grab_dist);
    let near_hi = near(filter_hi_x, grab_dist);

    // Whether the drag that is going on grabbed a filter edge, decided where
    // it started (the pointer was near an edge before it moved), and which one.
    let drag_id = egui::Id::new(panel.id).with("filter_drag");
    let hover_ready_id = drag_id.with("hover_ready");
    let is_low_id = drag_id.with("is_low");
    let dragging_filter: Option<bool> = ui.memory(|mem| mem.data.get_temp(drag_id));

    let mut action = None;
    if !response.dragged() {
        ui.memory_mut(|mem| mem.data.insert_temp(hover_ready_id, near_lo || near_hi));
        if near(filter_lo_x, cursor_dist) || near(filter_hi_x, cursor_dist) {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
        if dragging_filter.is_some() {
            ui.memory_mut(|mem| {
                mem.data.remove::<bool>(drag_id);
                mem.data.remove::<bool>(is_low_id);
            });
        }
    }
    if response.dragged() {
        if let Some(pos) = response.interact_pointer_pos() {
            let filter_edge = dragging_filter.unwrap_or_else(|| {
                let was_near = ui.memory(|mem| mem.data.get_temp::<bool>(hover_ready_id)).unwrap_or(false);
                ui.memory_mut(|mem| mem.data.insert_temp(drag_id, was_near));
                if was_near {
                    let dl = filter_lo_x.map(|x| (pos.x - x).abs()).unwrap_or(f32::MAX);
                    let dh = filter_hi_x.map(|x| (pos.x - x).abs()).unwrap_or(f32::MAX);
                    ui.memory_mut(|mem| mem.data.insert_temp(is_low_id, dl < dh));
                }
                was_near
            });
            if filter_edge {
                let low = ui.memory(|mem| mem.data.get_temp::<bool>(is_low_id)).unwrap_or(near_lo);
                let hz = panel.tuning.filter_edge(low, hz_of(pos.x) - marker, panel.filter_low_hz, panel.filter_high_hz);
                action = Some(SpectrumAction::FilterEdge { low, hz });
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            } else {
                action = Some(SpectrumAction::Tune { hz: panel.tuning.drag(hz_of(pos.x)), how: TuneHow::Drag });
            }
        }
    } else if response.clicked() && !(near_lo || near_hi) {
        // Not on a filter edge: there a click is the start of a drag that did not move.
        if let Some(pos) = response.interact_pointer_pos() {
            // A click near a DX spot tunes to the spot.
            let spot = panel.dx_spots.iter().find(|s| {
                let f = s.frequency_hz as f64;
                f >= start_hz && f <= end_hz && (pos.x - x_of(f)).abs() < 15.0
            });
            let hz = match spot {
                Some(s) => (s.frequency_hz).clamp(panel.tuning.min_hz, panel.tuning.max_hz),
                None => panel.tuning.click(hz_of(pos.x)),
            };
            action = Some(SpectrumAction::Tune { hz, how: TuneHow::Click });
        }
    }
    if action.is_none() {
        if let Some(up) = scroll_notch(ui, &response) {
            action = Some(SpectrumAction::Tune { hz: panel.tuning.scroll(up), how: TuneHow::Scroll });
        }
    }
    action
}

/// The waterfall of a panel. Each row shows its own extracted view where it
/// reaches and the wide row underneath elsewhere, so the history fills in
/// after tuning; a pixel no row reaches is left clear. Click, drag and scroll
/// tune, as on the plot.
pub(crate) fn spectrum_waterfall(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    panel: &SpectrumPanel,
    wf: &WaterfallRingBuffer,
    window: WaterfallWindow,
    texture: &mut Option<egui::TextureHandle>,
    display_height: f32,
) -> Option<SpectrumAction> {
    let width = ui.available_width();
    let display_height = display_height.max(40.0);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, display_height), egui::Sense::click_and_drag());
    let response = response.on_hover_text(rust_i18n::t!("spec_wf_hover").to_string());
    if wf.count == 0 || window.span_hz <= 0.0 {
        return None;
    }

    let rows = wf.height.max(1);
    let out_width = (width.max(1.0) as usize).min(1024);
    let mut pixels = vec![0u8; out_width * rows * 4];
    let start_hz = window.center_hz - window.span_hz / 2.0;
    let px_hz_step = window.span_hz / out_width as f64;
    let contrast = panel.wf_contrast.max(0.1);
    for row in 0..rows.min(wf.count) {
        let src = (wf.write_idx + wf.height - 1 - row) % wf.height;
        let row_full = &wf.full_rows[src];
        let row_full_center = wf.full_centers[src] as f64;
        if row_full.is_empty() || row_full_center == 0.0 { continue; }
        // A row's own width when it was stored with one: with the full-band
        // row off the wide row IS the extracted view, and mapping it over the
        // whole DDC would stretch the waterfall away from the spectrum.
        let stored = wf.full_spans[src] as f64;
        let row_span = if stored > 0.0 { stored } else { window.row_span_hz };
        let full_hz_per_bin = row_span / row_full.len() as f64;
        let full_start_hz = row_full_center - row_span / 2.0;
        let view = &wf.view_rows[src];
        let view_span = wf.view_spans[src] as f64;
        let has_view = !view.is_empty() && view_span > 0.0;
        let view_hz_per_bin = if has_view { view_span / view.len() as f64 } else { 1.0 };
        let view_start_hz = wf.view_centers[src] as f64 - view_span / 2.0;
        let view_end_hz = view_start_hz + view_span;
        let dst = row * out_width * 4;
        for px in 0..out_width {
            let px_start_hz = start_hz + px as f64 * px_hz_step;
            let px_end_hz = px_start_hz + px_hz_step;
            let px_mid_hz = (px_start_hz + px_end_hz) / 2.0;
            let max_val = if has_view && px_mid_hz >= view_start_hz && px_mid_hz < view_end_hz {
                let b0 = (((px_start_hz - view_start_hz) / view_hz_per_bin) as isize).max(0) as usize;
                let b1 = ((((px_end_hz - view_start_hz) / view_hz_per_bin).ceil() as usize).max(b0 + 1)).min(view.len());
                (b0..b1).map(|j| view[j]).max().unwrap_or(0)
            } else {
                let b0 = ((px_start_hz - full_start_hz) / full_hz_per_bin) as isize;
                let b1 = (((px_end_hz - full_start_hz) / full_hz_per_bin).ceil() as isize).max(b0 + 1);
                if b1 <= 0 || b0 >= row_full.len() as isize { continue; }
                let b0c = b0.max(0) as usize;
                let b1c = (b1 as usize).min(row_full.len());
                (b0c..b1c).map(|j| row_full[j]).max().unwrap_or(0)
            };
            let db = sdr_remote_core::unpack_spectrum_db(max_val);
            let frac = ((panel.ref_db - db) / panel.range_db).clamp(0.0, 1.0);
            let level = (1.0 - frac).powf(1.0 / contrast).clamp(0.0, 1.0);
            let (r, g, b) = spectrum_level_color(level);
            let idx = dst + px * 4;
            pixels[idx..idx + 4].copy_from_slice(&[r, g, b, 255]);
        }
    }
    let image = egui::ColorImage::from_rgba_unmultiplied([out_width, rows], &pixels);
    match texture {
        Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
        None => *texture = Some(ctx.load_texture(format!("waterfall_{}", panel.id), image, egui::TextureOptions::LINEAR)),
    }
    if ui.is_rect_visible(rect) {
        if let Some(tex) = texture.as_ref() {
            let uv = egui::Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
            ui.painter_at(rect).image(tex.id(), rect, uv, Color32::WHITE);
        }
    }

    let hz_of = |x: f32| start_hz + ((x - rect.min.x) / rect.width()) as f64 * window.span_hz;
    if response.dragged() {
        if let Some(pos) = response.interact_pointer_pos() {
            return Some(SpectrumAction::Tune { hz: panel.tuning.drag(hz_of(pos.x)), how: TuneHow::Drag });
        }
    } else if response.clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            return Some(SpectrumAction::Tune { hz: panel.tuning.click(hz_of(pos.x)), how: TuneHow::Click });
        }
    }
    scroll_notch(ui, &response).map(|up| SpectrumAction::Tune { hz: panel.tuning.scroll(up), how: TuneHow::Scroll })
}


/// Every reading starts at the same x, so the S does not walk with the value:
/// the widest two-digit reading ends at `right`. A wider one (three digits)
/// still ends at `right`.
const S_METER_WIDEST: &str = "S9+00dB";

fn s_meter_left(painter: &egui::Painter, font: &egui::FontId, right: f32, width: f32) -> f32 {
    let reserved = painter
        .layout_no_wrap(S_METER_WIDEST.to_string(), font.clone(), Color32::WHITE)
        .size()
        .x;
    right - reserved.max(width)
}

/// Map 8-bit power value to RGB color (waterfall colormap)
/// Black -> Blue -> Cyan -> Yellow -> Red -> White
pub(crate) fn waterfall_colormap(value: u8) -> (u8, u8, u8) {
    let v = value as f32 / 255.0;
    let (r, g, b) = if v < 0.2 {
        // Black -> Blue
        let t = v / 0.2;
        (0.0, 0.0, t)
    } else if v < 0.4 {
        // Blue -> Cyan
        let t = (v - 0.2) / 0.2;
        (0.0, t, 1.0)
    } else if v < 0.6 {
        // Cyan -> Yellow
        let t = (v - 0.4) / 0.2;
        (t, 1.0, 1.0 - t)
    } else if v < 0.8 {
        // Yellow -> Red
        let t = (v - 0.6) / 0.2;
        (1.0, 1.0 - t, 0.0)
    } else {
        // Red -> White
        let t = (v - 0.8) / 0.2;
        (1.0, t, t)
    };
    ((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}

/// Where the level colours of every spectrum panel start on the waterfall
/// colormap: the darkest quarter is skipped (0.0 = from black, 0.3 = from
/// cyan, 0.5 = from yellow). One value for RX1, RX2 and the VRX, line, fill and
/// waterfall alike; the phone has the same number in `SpectrumView.kt`.
pub(crate) const SPECTRUM_COLOR_FLOOR: f32 = 0.25;

/// Map normalized level (0.0=bottom/weak -> 1.0=top/strong) to RGB for the
/// spectrum line, fill and waterfall, from `SPECTRUM_COLOR_FLOOR` up.
fn spectrum_level_color(level: f32) -> (u8, u8, u8) {
    let mapped = SPECTRUM_COLOR_FLOOR + level.clamp(0.0, 1.0) * (1.0 - SPECTRUM_COLOR_FLOOR);
    waterfall_colormap((mapped * 255.0) as u8)
}

#[cfg(test)]
mod panel_height_tests {
    use super::{plot_and_waterfall_heights, PLOT_LABEL_H};

    /// Plot, label strip, gap and waterfall fill the panel exactly, plot and
    /// waterfall alike (build 241) - and RX1, RX2 and VRX all ask this one rule.
    #[test]
    fn every_spectrum_panel_fits_its_height() {
        for available in [200.0f32, 413.0, 600.0, 1200.0] {
            for gap in [0.0f32, 2.0, 3.0] {
                let (plot, wf) = plot_and_waterfall_heights(available, gap);
                assert_eq!(plot, wf, "{available}/{gap}: alike");
                assert!((plot + PLOT_LABEL_H + gap + wf - available).abs() < 0.01, "{available}/{gap}: fills it");
            }
        }
        let gap = "ui.spacing().item_spacing.y";
        for (name, src, from) in [
            ("RX1", include_str!("spectrum_content.rs"), "fn render_spectrum_content"),
            ("RX2", include_str!("rx_controls.rs"), "fn render_rx2_spectrum_only"),
            ("VRX", include_str!("vrx.rs"), "fn render_vrx_spectrum_panel"),
        ] {
            let body = &src[..src.find("#[cfg(test)]").unwrap_or(src.len())];
            let body = &body[body.find(from).unwrap()..];
            let body = &body[..body[3..].find("\n    pub(super) fn ").map_or(body.len(), |i| i + 3)];
            assert_eq!(body.matches("plot_and_waterfall_heights(").count(), 1, "{name}");
            assert!(body.contains(gap), "{name}: the real gap");
            assert!(!body.contains("* 0.50"), "{name}: no split of its own");
        }
        let draw = include_str!("spectrum.rs");
        let draw = &draw[..draw.find("#[cfg(test)]").unwrap()];
        assert_eq!(draw.matches("Vec2::new(width, plot_height + PLOT_LABEL_H)").count(), 1, "the one plot, for every panel");
    }
}

#[cfg(test)]
mod s_meter_tests {
    use super::S_METER_WIDEST;
    use sdr_remote_core::s_meter::text;

    /// The reserved width is a two-digit reading of the same form. The reading
    /// itself is `sdr_remote_core::s_meter` (build 218); no screen in the
    /// client formats one of its own.
    #[test]
    fn the_s_reading_and_its_reserved_width() {
        assert_eq!(S_METER_WIDEST.len(), text(-53.0).len());
        assert_eq!(S_METER_WIDEST.replace('0', "2"), text(-51.0), "the same form, digit for digit");
        for f in [
            include_str!("spectrum.rs"),
            include_str!("meters.rs"),
            include_str!("yaesu_panel.rs"),
        ] {
            let body = f.split("#[cfg(test)]").next().unwrap();
            assert_eq!(body.matches("S9+{").count(), 0, "an S reading formatted outside s_meter");
            assert_eq!(body.matches("format!(\"S{}\"").count(), 0, "an S step formatted outside s_meter");
        }
    }
}

#[cfg(test)]
mod fill_tests {
    use super::{
        compose_colour, mhz_decimals, spectrum_fill_colour, spectrum_level_color, vfo_mhz_decimals, waterfall_colormap,
        FILTER_BAND_RGB, FILTER_EDGE_RGBA, SPECTRUM_COLOR_FLOOR, SPECTRUM_FILL_DEFAULT, VFO_LINE_RGBA,
    };

    /// A frequency label has the decimals its tick step needs, one more per
    /// decade of zoom down to 1 Hz, and the VFO label at least kHz; RX, VRX and
    /// the phone by the one rule (2026-11-03: below 100 Hz per tick the labels
    /// came out twice the same).
    #[test]
    fn a_frequency_label_shows_its_tick_step() {
        for (step, want) in [
            (2_000_000.0, 0), (1_000_000.0, 0), (500_000.0, 1), (100_000.0, 1), (50_000.0, 2), (10_000.0, 2),
            (5_000.0, 3), (1_000.0, 3), (500.0, 4), (100.0, 4), (50.0, 5), (10.0, 5), (5.0, 6), (1.0, 6), (0.5, 6),
        ] {
            assert_eq!(mhz_decimals(step), want, "{step} Hz");
        }
        assert_eq!(vfo_mhz_decimals(100_000.0), 3);
        assert_eq!(vfo_mhz_decimals(1_000.0), 3);
        assert_eq!(vfo_mhz_decimals(50.0), 5);
        // 50 Hz ticks at 1024x: each label its own.
        assert_eq!(format!("{:.*}", mhz_decimals(50.0), 7_100_050.0 / 1e6), "7.10005");
        let draw = &SPECTRUM_RS[..SPECTRUM_RS.find("#[cfg(test)]").unwrap()];
        assert_eq!(draw.matches("format!(\"{:.*}\", mhz_decimals(tick_spacing_hz), freq / 1_000_000.0)").count(), 1, "the axis of every panel");
        assert_eq!(draw.matches("format!(\"{:.*}\", vfo_mhz_decimals(tick_spacing_hz), mhz)").count(), 1, "the frequency label of every panel");
        assert!(!draw.contains("{:.4}"), "no fixed depth left");
        // The phone: the same function, used at both labels.
        let kt = |s: &str| PHONE.matches(s).count();
        // The whole function on both sides, line for line the same loop.
        assert_eq!(kt("internal fun mhzDecimals(stepHz: Double): Int {\n    var d = 0\n    var unit = 1_000_000.0\n    while (d < 6 && stepHz < unit * 0.999) {\n        unit /= 10.0\n        d++\n    }\n    return d\n}\n"), 1);
        assert!(draw.contains("pub(crate) fn mhz_decimals(step_hz: f64) -> usize {\n    let mut d = 0;\n    let mut unit = 1_000_000.0;\n    while d < 6 && step_hz < unit * 0.999 {\n        unit /= 10.0;\n        d += 1;\n    }\n    d\n}\n"), "the same loop");
        // The VFO in f64: f32 has no six decimals at 30 MHz.
        assert!(draw.contains("let marker = panel.marker_hz as f64;"));
        assert!(draw.contains("let mhz = marker / 1_000_000.0;"));
        assert_eq!(kt("\"%.${mhzDecimals(tickSpacingHz)}f\".format(freqMhz)"), 1, "phone axis");
        assert_eq!(kt("\"%.${maxOf(3, mhzDecimals(tickSpacingHz))}f\".format(vfoMhz)"), 1, "phone VFO");
        assert_eq!(kt("\"%.4f\""), 0);
    }

    const SPECTRUM_RS: &str = include_str!("spectrum.rs");

    const PHONE: &str = include_str!("../../../sdr-remote-android/android/app/src/main/java/com/sdrremote/ui/components/SpectrumView.kt");

    /// The colour floor and the fill are one value each on the desktop, and
    /// the phone draws the same: its numbers live in Kotlin, so this test is
    /// what keeps the two from drifting apart (build 71).
    #[test]
    fn desktop_and_phone_share_the_colour_floor_and_the_fill() {
        let draw = include_str!("spectrum.rs");
        let draw = &draw[..draw.find("#[cfg(test)]").unwrap()];
        assert_eq!(draw.matches("spectrum_level_color(").count(), 4, "one function, three callers (fill, line, waterfall), no floor of their own");
        assert!(!draw.contains("color_floor"), "no panel carries a floor of its own");
        // The one function draws from the floor: its result, not its text.
        assert_eq!(spectrum_level_color(0.0), waterfall_colormap((SPECTRUM_COLOR_FLOOR * 255.0) as u8));
        assert_eq!(spectrum_level_color(1.0), waterfall_colormap(255));
        let phone = PHONE.replace("\r\n", "\n");
        // And on the phone the floor is the one in use, in both places.
        assert_eq!(phone.matches("val mapped = floor + level * (1f - floor)").count(), 1, "the phone's floor in its waterfall");
        assert_eq!(phone.matches("val mapped = floor + level.coerceIn(0f, 1f) * (1f - floor)").count(), 1, "and in its line colour");
        assert_eq!(phone.matches(&format!("val floor = {SPECTRUM_COLOR_FLOOR}f")).count(), 2, "the phone's colour floor, in its waterfall and in its line colour");
        let fill = format!("c.copy(alpha = {}f)", SPECTRUM_FILL_DEFAULT as f32 / 100.0);
        assert_eq!(phone.matches(&fill).count(), 1, "the phone's fill, {fill}");
    }

    /// The waterfall colormap is the same on desktop and phone (build 72).
    /// The desktop's is pinned by what it gives, the phone's by its text, in
    /// both functions that carry it (`waterfallColor`, `spectrumLevelColor`):
    /// a change on either side fails here, and then both are changed.
    #[test]
    fn desktop_and_phone_share_the_colormap() {
        for (value, rgb) in [
            (0, (0, 0, 0)), (25, (0, 0, 125)), (51, (0, 0, 255)), (76, (0, 125, 255)),
            (102, (0, 255, 255)), (127, (124, 255, 130)), (153, (255, 255, 0)),
            (178, (255, 130, 0)), (204, (255, 0, 0)), (229, (255, 124, 124)), (255, (255, 254, 254)),
        ] {
            assert_eq!(waterfall_colormap(value), rgb, "{value}");
        }
        let phone = PHONE.replace("\r\n", "\n");
        let map = "        v < 0.2f -> Triple(0f, 0f, v / 0.2f)
        v < 0.4f -> Triple(0f, (v - 0.2f) / 0.2f, 1f)
        v < 0.6f -> {
            val t = (v - 0.4f) / 0.2f
            Triple(t, 1f, 1f - t)
        }
        v < 0.8f -> Triple(1f, 1f - (v - 0.6f) / 0.2f, 0f)
        else -> {
            val t = (v - 0.8f) / 0.2f
            Triple(1f, t, t)
        }
";
        assert_eq!(phone.matches(map).count(), 2, "the phone's colormap, in its waterfall and in its line colour");
        assert_eq!(phone.matches("v < ").count(), 8, "no third colormap on the phone");
    }

    /// The bins on the wire are packed on one dB scale, and the phone reads
    /// them back on the same one (build 72): a different scale on either side
    /// puts every signal at the wrong height, without a word.
    #[test]
    fn desktop_and_phone_share_the_db_scale_of_the_bins() {
        use sdr_remote_core::{SPECTRUM_PACK_FLOOR_DB, SPECTRUM_PACK_MAX, SPECTRUM_PACK_RANGE_DB};
        assert_eq!(PHONE.matches(&format!("internal const val SERVER_FLOOR_DB = {SPECTRUM_PACK_FLOOR_DB}f")).count(), 1);
        assert_eq!(PHONE.matches(&format!("internal const val SERVER_RANGE_DB = {SPECTRUM_PACK_RANGE_DB}f")).count(), 1);
        // The phone's auto reference reads the bins on the same two constants.
        let main = include_str!("../../../sdr-remote-android/android/app/src/main/java/com/sdrremote/ui/screens/MainScreen.kt");
        assert_eq!(main.matches("val db = SERVER_FLOOR_DB.toDouble() + (bins[i].toInt().toDouble() / 255.0) * SERVER_RANGE_DB.toDouble()").count(), 1, "the phone's auto reference");
        assert!(!main.contains("-150.0"), "no scale of its own in MainScreen.kt");
        // The bridge hands the phone the top byte of the u16 bin, and the phone
        // divides by the top of a byte.
        assert_eq!(SPECTRUM_PACK_MAX, 65535.0);
        let bridge = include_str!("../../../sdr-remote-android/src/bridge.rs");
        // Both bins the phone draws: the spectrum, and the full row its waterfall takes.
        assert_eq!(bridge.matches("spectrum_bins: s.spectrum_bins.iter().map(|v| (v >> 8) as u8).collect(),").count(), 1);
        assert_eq!(bridge.matches("full_spectrum_bins: s.full_spectrum_bins.iter().map(|v| (v >> 8) as u8).collect(),").count(), 1);
        assert_eq!(bridge.matches("(v >> ").count(), 2, "no third bin path in the bridge");
        assert_eq!(PHONE.matches("val db = SERVER_FLOOR_DB + (maxVal.toFloat() / 255f) * SERVER_RANGE_DB").count(), 2, "spectrum and waterfall");
        assert_eq!(PHONE.matches("SERVER_FLOOR_DB").count(), 3, "declared, and used by both");
        // The desktop reads the bins back through core, as the phone does by the numbers.
        let draw = include_str!("spectrum.rs");
        let draw = &draw[..draw.find("#[cfg(test)]").unwrap()];
        assert_eq!(draw.matches("sdr_remote_core::unpack_spectrum_db(").count(), 2, "line and waterfall, of every panel");
        assert!(!draw.contains("65535"), "no packed scale of its own on the desktop");
        let server = include_str!("../../../sdr-remote-server/src/spectrum.rs");
        let server = &server[..server.find("#[cfg(test)]").unwrap()];
        assert!(!server.contains("65535"), "no packed scale of its own in the server");
        assert_eq!(server.matches("cal_shift(self.cal_offset_db)").count(), 6);
    }

    /// Every waterfall is the one picture with nothing drawn over it (build 74:
    /// the VRX had a thin line at its frequency, RX none; build 81: one
    /// waterfall for every panel, and a pixel no row reaches is left clear).
    #[test]
    fn every_waterfall_is_the_one_picture() {
        let draw = include_str!("spectrum.rs");
        let draw = &draw[..draw.find("#[cfg(test)]").unwrap()];
        let images: Vec<_> = draw.match_indices(".image(tex.id(), rect, uv, Color32::WHITE)").map(|(i, _)| i).collect();
        assert_eq!(images.len(), 1, "the one waterfall picture");
        // From the picture to the click handling: no painter call at all, so
        // no line in any spelling (`line_segment`, `vline`, a thin rect).
        let after = &draw[images[0] + ".image(".len()..];
        let block = &after[..after.find("let hz_of").unwrap()];
        assert!(!block.contains("painter"), "nothing over the waterfall: {block}");
        assert!(!block.contains("vline") && !block.contains("line_segment"));
        assert!(draw.contains("let mut pixels = vec![0u8; out_width * rows * 4];"), "clear where no row reaches");
        assert!(!draw.contains("from_rgb(8, 10, 20)"), "no background of its own");
    }

    /// The filter passband, its edges and the VFO line are the phone's
    /// colours, mixed as the phone mixes them (build 72).
    #[test]
    fn desktop_and_phone_share_the_passband_and_vfo_colours() {
        let [r, g, b] = FILTER_BAND_RGB;
        assert_eq!(PHONE.matches(&format!("Color({r}, {g}, {b})")).count(), 1, "the passband");
        let [r, g, b, a] = FILTER_EDGE_RGBA;
        assert_eq!(PHONE.matches(&format!("val edgeColor = Color({r}, {g}, {b}, {a})")).count(), 1, "the edges");
        let [r, g, b, a] = VFO_LINE_RGBA;
        assert_eq!(PHONE.matches(&format!("val vfoColor = Color({r}, {g}, {b}, {a})")).count(), 1, "the VFO line");
        let draw = include_str!("spectrum.rs");
        let draw = &draw[..draw.find("#[cfg(test)]").unwrap()];
        assert_eq!(draw.matches("Color32::from_rgb(FILTER_BAND_RGB[0], FILTER_BAND_RGB[1], FILTER_BAND_RGB[2])").count(), 1, "every panel");
        assert_eq!(draw.matches("compose_colour(FILTER_EDGE_RGBA)").count(), 1, "every panel");
        assert_eq!(draw.matches("compose_colour(VFO_LINE_RGBA)").count(), 1, "every panel");
        // Mixed as Compose: the alpha is the share of the colour (180/255 of 255 is 180).
        assert_eq!(compose_colour(VFO_LINE_RGBA).to_array(), [180, 35, 35, 180]);
        assert_eq!(compose_colour(FILTER_EDGE_RGBA).to_array(), [94, 94, 0, 120]);
        // Every colour with an opacity in the spectrum mixes this way, also
        // the RIT line, which the phone does not draw.
        assert!(!draw.contains("Color32::from_rgba_unmultiplied("), "no colour mixed in linear light");
        assert_eq!(draw.matches("compose_colour([0, 200, 255, 160])").count(), 1, "the RIT line");
    }

    /// The fill is the peak colour of its column at the chosen opacity, the
    /// same from the curve down to the bottom (build 70), in both meshes.
    #[test]
    fn the_fill_is_the_peak_colour_at_the_chosen_opacity() {
        assert_eq!(SPECTRUM_FILL_DEFAULT, 15);
        assert_eq!(spectrum_fill_colour(200, 100, 50, 0).a(), 0);
        assert_eq!(spectrum_fill_colour(200, 100, 50, 0).to_array(), [0, 0, 0, 0]);
        // The colour as it reaches the blend: the share of the peak colour the
        // field says, in the screen's own values (Android: `copy(alpha = 0.15f)`).
        assert_eq!(spectrum_fill_colour(200, 100, 50, 15).to_array(), [30, 15, 7, 38]);
        assert_eq!(spectrum_fill_colour(255, 255, 255, 2).to_array(), [5, 5, 5, 5]);
        assert_eq!(spectrum_fill_colour(200, 100, 50, 100).to_array(), [200, 100, 50, 255]);
        assert_eq!(spectrum_fill_colour(200, 100, 50, 250).to_array(), [200, 100, 50, 255], "clamped");
        let draw = include_str!("spectrum.rs");
        let draw = &draw[..draw.find("#[cfg(test)]").unwrap()];
        // The whole argument: the plot's own value, not the default above it.
        assert_eq!(draw.matches("let fill_color = spectrum_fill_colour(r, g, b, panel.fill_percent);").count(), 1, "every panel");
        assert_eq!(draw.matches("spectrum_fill_colour(").count(), 2, "the helper and its caller");
        assert_eq!(draw.matches("color: fill_color }").count(), 2, "top and bottom of the column alike");
        assert_eq!(draw.matches("bottom_color").count(), 0);
    }
}

#[cfg(test)]
mod view_limit_tests {
    use super::{fft_sizes_k, pan_limit, zoom_range, RANGE_DB_RANGE, REF_DB_RANGE, WF_CONTRAST_RANGE};

    fn code(src: &'static str) -> &'static str {
        &src[..src.find("#[cfg(test)]").unwrap_or(src.len())]
    }

    /// The FFT row offers nothing the server does not run: it clamps at the
    /// shared limit, so a 1024K here ran as 512K while the row said 1024K.
    #[test]
    fn the_fft_row_stops_at_the_servers_limit() {
        // At these rates the rate allows more, so the limit is what stops the row.
        assert_eq!(fft_sizes_k(1_536_000).last(), Some(&sdr_remote_core::MAX_FFT_SIZE_K));
        assert_eq!(fft_sizes_k(768_000).last(), Some(&sdr_remote_core::MAX_FFT_SIZE_K));
        assert_eq!(fft_sizes_k(48_000), vec![32, 64, 128]);
    }

    /// The rows and MIDI hold a panel to one set of limits: the rows read them
    /// from here, MIDI goes through the setters that do, and the setters hand
    /// the change to the one place the rows hand theirs to.
    #[test]
    fn the_rows_and_midi_keep_one_set_of_limits() {
        assert_eq!(zoom_range(false), 2.0..=1024.0);
        assert_eq!(zoom_range(true), 1.0..=1024.0);
        assert_eq!(REF_DB_RANGE, -90.0..=0.0);
        assert_eq!(RANGE_DB_RANGE, 20.0..=130.0);
        assert_eq!(WF_CONTRAST_RANGE, 0.3..=3.0);
        assert_eq!(pan_limit(1.0), 0.0);
        assert!((pan_limit(4.0) - 0.125).abs() < 1e-6);
        let rows = code(include_str!("spectrum.rs"));
        let rows = &rows[rows.find("pub(crate) fn spectrum_view_rows(").unwrap()..];
        let rows = &rows[..rows.find("\n}\n").unwrap()];
        for limit in ["-90.0", "0.3..", "1024.0", "130.0", "max_pan_fraction"] {
            assert!(!rows.contains(limit), "the rows read {limit} from the one place");
        }
        assert_eq!(rows.matches("REF_DB_RANGE").count(), 3);
        assert_eq!(rows.matches("WF_CONTRAST_RANGE").count(), 2);
        assert!(rows.contains("let zooms = zoom_range(*allow_zoom_below_2x);"));
        // A zoom pulls the pan into what it allows; the FFT choice is reported.
        assert!(rows.contains("        let max_pan = pan_limit(*zoom);\n        if ch.zoom {\n            *pan = pan.clamp(-max_pan, max_pan);\n        }\n"));
        assert!(rows.contains("ch.fft = Some(*k);"));
        let midi = code(include_str!("midi_screen.rs"));
        for limit in ["1024", "-140", "0.3 +", "clamp(-1.0, 1.0)", "clamp(0.3, 3.0)", "self.spectrum_zoom =", "self.rx2_spectrum_zoom =", "self.spectrum_pan =", "self.rx2_spectrum_pan =",
            "self.spectrum_ref_db =", "self.rx2_spectrum_ref_db =", "auto_ref_enabled = false", "self.waterfall_contrast =",
            "self.rx2_waterfall_contrast =", "zoom_pan_changed_at"] {
            assert!(!midi.contains(limit), "MIDI sets no view of its own: {limit}");
        }
        for setter in ["midi_set_zoom(", "midi_set_pan(", "midi_set_ref(", "midi_set_contrast("] {
            assert_eq!(midi.matches(setter).count(), 2, "slider and encoder: {setter}");
        }
        let after = code(include_str!("spectrum_content.rs"));
        for (setter, limits) in [
            ("fn midi_set_zoom(", &[
                "let zooms = super::spectrum::zoom_range(self.allow_zoom_below_2x);",
                "let zoom = zoom.clamp(*zooms.start(), *zooms.end());\n        let max_pan = super::spectrum::pan_limit(zoom);\n",
                "if rx2 { (&mut self.rx2_spectrum_zoom, &mut self.rx2_spectrum_pan) } else { (&mut self.spectrum_zoom, &mut self.spectrum_pan) };",
                "*z = zoom;\n        *p = p.clamp(-max_pan, max_pan);",
                "super::spectrum::ViewChanges { zoom: true, ..Default::default() }",
            ][..]),
            ("fn midi_set_pan(", &[
                "if rx2 { (self.rx2_spectrum_zoom, &mut self.rx2_spectrum_pan) } else { (self.spectrum_zoom, &mut self.spectrum_pan) };",
                "let max_pan = super::spectrum::pan_limit(z);",
                "*p = pan.clamp(-max_pan, max_pan);",
                "super::spectrum::ViewChanges { pan: true, ..Default::default() }",
            ][..]),
            ("fn midi_set_ref(", &[
                "if rx2 { (&mut self.rx2_spectrum_ref_db, &mut self.rx2_auto_ref_enabled) } else { (&mut self.spectrum_ref_db, &mut self.auto_ref_enabled) };",
                "*r = ref_db.clamp(*super::spectrum::REF_DB_RANGE.start(), *super::spectrum::REF_DB_RANGE.end());",
                "let was_auto = std::mem::replace(auto, false);",
                "super::spectrum::ViewChanges { ref_db: true, auto_ref: was_auto, ..Default::default() }",
            ][..]),
            ("fn midi_set_contrast(", &[
                "if rx2 { &mut self.rx2_waterfall_contrast } else { &mut self.waterfall_contrast };",
                "*c = contrast.clamp(*super::spectrum::WF_CONTRAST_RANGE.start(), *super::spectrum::WF_CONTRAST_RANGE.end());",
                "super::spectrum::ViewChanges { wf_contrast: true, ..Default::default() }",
            ][..]),
        ] {
            let body = &after[after.find(setter).unwrap()..];
            let body = &body[..body.find("\n    }\n").unwrap()];
            assert_eq!(body.matches("self.rx_view_changed(rx2, ").count(), 1, "{setter}");
            for limit in limits {
                assert_eq!(body.matches(limit).count(), 1, "{setter}: {limit}");
            }
        }
        assert_eq!(after.matches("self.rx_view_changed(false, changes);").count(), 1, "RX1's rows");
        assert_eq!(code(include_str!("rx_controls.rs")).matches("self.rx_view_changed(true, changes);").count(), 1, "RX2's rows");
        assert_eq!(after.matches("if let Some(k) = changes.fft {").count(), 2, "both send their FFT choice");
        let handler = &after[after.find("fn rx_view_changed(").unwrap()..];
        let handler = &handler[..handler.find("\n    }\n").unwrap()];
        for step in ["self.rx1_spectrum.reset_auto_ref();", "self.rx2_spectrum.reset_auto_ref();", "self.zoom_below_2x_changed();",
            "self.wf_contrast_per_band.insert(", "self.save_full_config();", "self.zoom_pan_changed_at = Some(", "self.rx2_zoom_pan_changed_at = Some("] {
            assert_eq!(handler.matches(step).count(), 1, "{step}");
        }
        // Each receiver's steps in its own branch, and what both do after them.
        let rx2 = &handler[handler.find("        if rx2 {\n").expect("the RX2 branch")..];
        let (rx2, rx1) = rx2.split_at(rx2.find("        } else {\n").expect("the RX1 branch"));
        let (rx1, both) = rx1.split_at(rx1.find("\n        }\n").unwrap() + 1);
        for step in ["changes.restart_auto_ref(self.rx2_auto_ref_enabled)", "self.rx2_spectrum.reset_auto_ref();",
            "Command::SetControl(ControlId::Rx2SpectrumFftSize, k)",
            "if changes.zoom || changes.pan {\n                self.rx2_zoom_pan_changed_at = Some("] {
            assert!(rx2.contains(step), "RX2: {step}");
        }
        for step in ["changes.restart_auto_ref(self.auto_ref_enabled)", "self.rx1_spectrum.reset_auto_ref();",
            "Command::SetSpectrumFftSize(k)",
            "if changes.wf_contrast {\n                if let Some(ref band) = self.current_band {\n                    self.wf_contrast_per_band.insert(",
            "if changes.zoom || changes.pan {\n                self.zoom_pan_changed_at = Some("] {
            assert!(rx1.contains(step) && !rx1.contains("rx2_"), "RX1: {step}");
        }
        assert!(both.contains("if changes.allow_zoom_below_2x {") && both.contains("if changes.settings() {\n            self.save_full_config();\n        }"));
        // A zoom pulled into range by the rows is reported, so it is sent.
        assert!(rows.contains("        let before = *zoom;\n        *zoom = zoom.clamp(*zooms.start(), *zooms.end());\n        let pulled_in = *zoom != before;\n"));
        assert!(rows.contains("ch.zoom = resp.changed() || scrolled || pulled_in;"));
        // The loader keeps to the same limits.
        let load = code(include_str!("config.rs"));
        for limit in ["-80.0", "clamp(0.3, 3.0)", "clamp(20.0, 130.0)"] {
            assert!(!load.contains(limit), "the loader: {limit}");
        }
        assert_eq!(load.matches("v.clamp(*crate::ui::spectrum::REF_DB_RANGE.start(), *crate::ui::spectrum::REF_DB_RANGE.end())").count(), 2);
        assert_eq!(load.matches("v.clamp(*crate::ui::spectrum::RANGE_DB_RANGE.start(), *crate::ui::spectrum::RANGE_DB_RANGE.end())").count(), 2);
        assert_eq!(load.matches("clamp(*crate::ui::spectrum::WF_CONTRAST_RANGE.start(), *crate::ui::spectrum::WF_CONTRAST_RANGE.end())").count(), 3);
    }
}
