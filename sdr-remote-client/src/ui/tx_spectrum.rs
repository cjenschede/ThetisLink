// SPDX-License-Identifier: GPL-2.0-or-later

//! The transmit spectrum of a Yaesu: what the Opus encoder takes, after the
//! gain, EQ, compressor, AGC and limiter (`sdr_remote_logic::tx_tap`, tapped
//! where the level meter is).
//!
//! Three lines and an overlay, each switchable:
//! - **live**: one FFT of the newest 2048 samples (128 ms), about 20 times a
//!   second while the transmitter is keyed;
//! - **last over**: the average power spectrum of the whole last over (Welch,
//!   2048 points, half overlap), kept after the release; a roger beep is not in
//!   it;
//! - **peak hold**: the highest level per frequency during that over;
//! - **EQ**: the response of the EQ that sounds, on its own scale at the right
//!   (as the EQ curve above it); not drawn over a played WAV, which is sent
//!   without the EQ.
//!
//! The frequency axis is linear or logarithmic, the plot can be pulled taller
//! at its lower edge, and the pointer reads the frequency and the levels.
//!
//! Levels are dBFS: a full-scale sine reads 0 dB. The FFT runs here, on the
//! screen's thread; the transmit path only copies a frame while a spectrum is
//! on screen (`tap_wanted`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use rustfft::num_complex::Complex;
use sdr_remote_logic::eq;
use sdr_remote_logic::tx_tap::{TxTap, SAMPLE_RATE};

/// FFT size: 7.8 Hz per bin at 16 kHz.
pub(super) const FFT_SIZE: usize = 2048;
/// How often the live spectrum is recomputed.
const LIVE_EVERY: Duration = Duration::from_millis(50);
/// A spectrum not drawn for this long counts as closed: its window is shut or
/// its EQ section folded. The tap then stops copying.
pub(super) const SHOWN_WITHIN: Duration = Duration::from_secs(1);
/// The level axis, dBFS.
const DB_TOP: f32 = 0.0;
const DB_BOTTOM: f32 = -100.0;
/// The bottom of a logarithmic frequency axis.
const LOG_LOW_HZ: f32 = 50.0;
/// The plot's height: the default, and how far it can be pulled.
const HEIGHT_DEFAULT: f32 = 160.0;
const HEIGHT_MIN: f32 = 100.0;
const HEIGHT_MAX: f32 = 600.0;

const OVER_COLOR: egui::Color32 = egui::Color32::from_rgb(230, 150, 40);
const PEAK_COLOR: egui::Color32 = egui::Color32::from_gray(150);
const EQ_COLOR: egui::Color32 = egui::Color32::from_rgb(90, 190, 110);

/// The window, the plan and the scale for one FFT size.
pub(super) struct Analyzer {
    fft: Arc<dyn rustfft::Fft<f32>>,
    window: Vec<f32>,
    /// Sum of the window over two: a full-scale sine gives this magnitude.
    norm: f32,
}

impl Default for Analyzer {
    fn default() -> Self {
        let fft = rustfft::FftPlanner::new().plan_fft_forward(FFT_SIZE);
        let window: Vec<f32> = (0..FFT_SIZE)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / FFT_SIZE as f32).cos())
            .collect();
        let norm = window.iter().sum::<f32>() / 2.0;
        Self { fft, window, norm }
    }
}

impl Analyzer {
    /// The power per bin (0 Hz to half the rate) of the newest `FFT_SIZE`
    /// samples, zero-padded in front when there are fewer; 1.0 = full scale.
    fn power(&self, samples: &[f32]) -> Vec<f32> {
        let take = samples.len().min(FFT_SIZE);
        let pad = FFT_SIZE - take;
        let mut buf: Vec<Complex<f32>> = (0..FFT_SIZE)
            .map(|i| {
                let x = if i < pad { 0.0 } else { samples[samples.len() - take + (i - pad)] };
                Complex::new(x * self.window[i], 0.0)
            })
            .collect();
        self.fft.process(&mut buf);
        buf[..=FFT_SIZE / 2].iter().map(|c| c.norm_sqr() / (self.norm * self.norm)).collect()
    }

    /// The live spectrum in dBFS.
    pub(super) fn live_db(&self, samples: &[f32]) -> Vec<f32> {
        self.power(samples).into_iter().map(to_db).collect()
    }

    /// The average and the peak spectrum of a whole over, in dBFS (Welch:
    /// `FFT_SIZE` points, half overlap). `None` for an over shorter than one FFT.
    pub(super) fn over_db(&self, samples: &[f32]) -> Option<(Vec<f32>, Vec<f32>)> {
        if samples.len() < FFT_SIZE {
            return None;
        }
        let hop = FFT_SIZE / 2;
        let mut sum = vec![0.0f32; FFT_SIZE / 2 + 1];
        let mut peak = vec![0.0f32; FFT_SIZE / 2 + 1];
        let mut n = 0usize;
        let mut start = 0;
        while start + FFT_SIZE <= samples.len() {
            let p = self.power(&samples[start..start + FFT_SIZE]);
            for (i, v) in p.into_iter().enumerate() {
                sum[i] += v;
                peak[i] = peak[i].max(v);
            }
            n += 1;
            start += hop;
        }
        let avg = sum.into_iter().map(|v| to_db(v / n as f32)).collect();
        Some((avg, peak.into_iter().map(to_db).collect()))
    }
}

fn to_db(p: f32) -> f32 {
    10.0 * p.max(1e-12).log10()
}

/// The frequency of bin `i`.
fn bin_hz(i: usize) -> f32 {
    i as f32 * SAMPLE_RATE as f32 / FFT_SIZE as f32
}

/// The frequency axis: 0 Hz (or `LOG_LOW_HZ` when logarithmic) to `hi`.
#[derive(Clone, Copy)]
struct Axis {
    hi: f32,
    log: bool,
}

impl Axis {
    fn lo(self) -> f32 {
        if self.log { LOG_LOW_HZ } else { 0.0 }
    }

    /// Where `f` sits, 0 at the left edge and 1 at the right.
    fn frac(self, f: f32) -> f32 {
        let t = if self.log { (f.max(LOG_LOW_HZ) / LOG_LOW_HZ).ln() / (self.hi / LOG_LOW_HZ).ln() } else { f / self.hi };
        t.clamp(0.0, 1.0)
    }

    /// The frequency at `t` (0 to 1 across the plot).
    fn at(self, t: f32) -> f32 {
        if self.log { LOG_LOW_HZ * (self.hi / LOG_LOW_HZ).powf(t) } else { self.hi * t }
    }

    /// The grid lines, and whether each carries a label.
    fn grid(self) -> Vec<(f32, bool)> {
        if self.log {
            [100.0, 200.0, 300.0, 500.0, 1000.0, 2000.0, 3000.0, 5000.0]
                .into_iter()
                .filter(|&f| f > self.lo() && f < self.hi)
                .map(|f| (f, f != 300.0 && f != 3000.0))
                .collect()
        } else {
            let step = if self.hi <= 4000.0 { 500.0 } else { 1000.0 };
            (1..).map(|k| k as f32 * step).take_while(|&f| f < self.hi).map(|f| (f, (f % 1000.0).abs() < 1.0)).collect()
        }
    }
}

/// The level drawn in pixel column `c` of `cols`: the highest bin in the
/// column, so a narrow peak is not lost. The line and the pointer both read it,
/// so the number under the pointer is the line under it.
fn column_db(bins: &[f32], axis: Axis, c: usize, cols: usize) -> Option<f32> {
    let f0 = axis.at(c as f32 / cols as f32);
    let f1 = axis.at((c + 1) as f32 / cols as f32);
    let i0 = (f0 / bin_hz(1)).floor() as usize;
    let i1 = ((f1 / bin_hz(1)).ceil() as usize).max(i0 + 1).min(bins.len());
    bins.get(i0..i1)?.iter().copied().reduce(f32::max)
}

fn hz_label(f: f32) -> String {
    if f >= 1000.0 { format!("{}k", f / 1000.0) } else { format!("{f:.0}") }
}

/// The spectra of one over, as drawn.
struct OverSpectra {
    avg: Vec<f32>,
    peak: Vec<f32>,
    seconds: f32,
    /// A played WAV was in it: no EQ was on it.
    wav: bool,
}

/// What one Yaesu's spectrum shows, kept by the panel.
pub(super) struct TxSpectrumView {
    pub(super) open: bool,
    /// The top of the frequency axis: 4 or 8 kHz.
    pub(super) range_khz: u32,
    pub(super) log_freq: bool,
    pub(super) show_live: bool,
    pub(super) show_over: bool,
    pub(super) show_peak: bool,
    pub(super) show_eq: bool,
    height: f32,
    live_db: Vec<f32>,
    keyed: bool,
    live_wav: bool,
    over: Option<OverSpectra>,
    last_live: Option<Instant>,
    drawn_at: Option<Instant>,
}

impl Default for TxSpectrumView {
    fn default() -> Self {
        Self {
            open: false,
            range_khz: 4,
            log_freq: true,
            show_live: true,
            show_over: true,
            show_peak: false,
            show_eq: true,
            height: HEIGHT_DEFAULT,
            live_db: Vec::new(),
            keyed: false,
            live_wav: false,
            over: None,
            last_live: None,
            drawn_at: None,
        }
    }
}

impl TxSpectrumView {
    /// Take what the tap has for `slot`: a new live spectrum while keyed (at
    /// most every `LIVE_EVERY`), and the last over once it ended.
    pub(super) fn update(&mut self, analyzer: &Analyzer, tap: &TxTap, slot: usize) {
        let (live, keyed, wav) = tap.live(slot);
        self.keyed = keyed;
        if keyed && self.last_live.is_none_or(|t| t.elapsed() >= LIVE_EVERY) {
            self.live_db = analyzer.live_db(&live);
            self.live_wav = wav;
            self.last_live = Some(Instant::now());
        }
        if let Some(over) = tap.take_finished_over(slot) {
            if let Some((avg, peak)) = analyzer.over_db(&over.samples) {
                let seconds = over.samples.len() as f32 / SAMPLE_RATE as f32;
                self.over = Some(OverSpectra { avg, peak, seconds, wav: over.wav });
            }
        }
    }

    /// Forget what was shown (the spectrum was switched off); the switches stay.
    pub(super) fn clear(&mut self) {
        self.live_db.clear();
        self.keyed = false;
        self.live_wav = false;
        self.over = None;
        self.last_live = None;
        self.drawn_at = None;
    }

    /// The spectrum is drawn at `now`. Not drawn for `SHOWN_WITHIN` before (its
    /// window was shut or its section folded), it starts empty, as after
    /// unticking: the tap forgot that time too.
    fn mark_drawn(&mut self, now: Instant) {
        if self.drawn_at.is_some_and(|t| now.saturating_duration_since(t) >= SHOWN_WITHIN) {
            self.clear();
        }
        self.drawn_at = Some(now);
    }

    /// What is on screen was sent without the EQ (a played WAV): while keyed
    /// the newest frame, after the release the last over.
    fn without_eq(&self) -> bool {
        match (&self.over, self.keyed) {
            (Some(o), false) => o.wav,
            _ => self.live_wav,
        }
    }
}

/// Whether the tap should copy: a spectrum is switched on and was drawn within
/// `SHOWN_WITHIN`. A closed window or a folded EQ section stops drawing it, and
/// so stops the copy.
pub(super) fn tap_wanted(views: &[TxSpectrumView], now: Instant) -> bool {
    views.iter().any(|v| v.open && v.drawn_at.is_some_and(|t| now.saturating_duration_since(t) < SHOWN_WITHIN))
}

/// Draw the spectrum of one Yaesu with its switches. `eq_bands` is the EQ that
/// sounds, `None` when the EQ is off (the overlay is then flat).
pub(super) fn show(ui: &mut egui::Ui, view: &mut TxSpectrumView, eq_bands: Option<&eq::EqBands>) {
    view.mark_drawn(Instant::now());
    ui.horizontal_wrapped(|ui| {
        ui.checkbox(&mut view.show_live, rust_i18n::t!("dev_txspec_live").to_string());
        ui.checkbox(&mut view.show_over, rust_i18n::t!("dev_txspec_over").to_string())
            .on_hover_text(rust_i18n::t!("dev_txspec_over_hover").to_string());
        ui.checkbox(&mut view.show_peak, rust_i18n::t!("dev_txspec_peak").to_string())
            .on_hover_text(rust_i18n::t!("dev_txspec_peak_hover").to_string());
        ui.checkbox(&mut view.show_eq, "EQ").on_hover_text(rust_i18n::t!("dev_txspec_eq_hover").to_string());
        ui.add_space(8.0);
        for khz in [4u32, 8] {
            if ui.selectable_label(view.range_khz == khz, format!("{khz} kHz")).clicked() {
                view.range_khz = khz;
            }
        }
        ui.checkbox(&mut view.log_freq, rust_i18n::t!("dev_txspec_log").to_string())
            .on_hover_text(rust_i18n::t!("dev_txspec_log_hover").to_string());
    });
    if let Some(o) = &view.over {
        ui.label(
            egui::RichText::new(format!("{} {:.1} s", rust_i18n::t!("dev_txspec_over_len"), o.seconds))
                .size(10.0)
                .weak(),
        );
    }
    let width = ui.available_width().clamp(240.0, 520.0);
    plot(ui, view, eq_bands, width);
    resize_handle(ui, view, width);
}

fn plot(ui: &mut egui::Ui, view: &TxSpectrumView, eq_bands: Option<&eq::EqBands>, width: f32) {
    let axis = Axis { hi: view.range_khz as f32 * 1000.0, log: view.log_freq };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, view.height), egui::Sense::hover());
    let p = ui.painter_at(rect);
    let v = ui.visuals();
    let live_color = v.selection.bg_fill;
    p.rect_filled(rect, 2.0, v.extreme_bg_color);
    let grid = egui::Stroke::new(1.0, v.widgets.noninteractive.bg_stroke.color);
    let text = v.weak_text_color();
    let font = egui::FontId::proportional(9.0);
    let x_of = |f: f32| rect.left() + rect.width() * axis.frac(f);
    let y_of = |db: f32| rect.top() + rect.height() * ((DB_TOP - db) / (DB_TOP - DB_BOTTOM)).clamp(0.0, 1.0);
    let eq_y = |db: f32| rect.center().y - rect.height() / 2.0 * (db / super::yaesu_panel::EQ_CURVE_DB).clamp(-1.0, 1.0);

    for (f, label) in axis.grid() {
        let x = x_of(f);
        p.line_segment([egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())], grid);
        if label {
            p.text(egui::pos2(x + 2.0, rect.bottom() - 2.0), egui::Align2::LEFT_BOTTOM, hz_label(f), font.clone(), text);
        }
    }
    let mut db = -20.0;
    while db > DB_BOTTOM {
        let y = y_of(db);
        p.line_segment([egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)], grid);
        p.text(egui::pos2(rect.left() + 2.0, y), egui::Align2::LEFT_CENTER, format!("{db:.0}"), font.clone(), text);
        db -= 20.0;
    }

    // One point per pixel column (`column_db`).
    let cols = rect.width().max(1.0) as usize;
    let curve = |bins: &[f32]| -> Vec<egui::Pos2> {
        (0..cols)
            .filter_map(|c| column_db(bins, axis, c, cols).map(|db| egui::pos2(rect.left() + c as f32, y_of(db))))
            .collect()
    };
    if view.show_peak {
        if let Some(o) = &view.over {
            p.add(egui::Shape::line(curve(&o.peak), egui::Stroke::new(1.0, PEAK_COLOR)));
        }
    }
    if view.show_over {
        if let Some(o) = &view.over {
            p.add(egui::Shape::line(curve(&o.avg), egui::Stroke::new(1.5, OVER_COLOR)));
        }
    }
    if view.show_live && !view.live_db.is_empty() {
        p.add(egui::Shape::line(curve(&view.live_db), egui::Stroke::new(1.5, live_color)));
    }
    // EQ off: flat. Over a played WAV: not drawn, it was sent without the EQ.
    let eq_db = |f: f32| eq_bands.map_or(0.0, |bands| eq::response_db(bands, f.max(1.0), eq::EQ_SAMPLE_RATE));
    let eq_drawn = view.show_eq && !view.without_eq();
    if view.show_eq {
        let label = if eq_drawn {
            let n = 160;
            let pts: Vec<egui::Pos2> = (0..=n)
                .map(|i| {
                    let f = axis.at(i as f32 / n as f32);
                    egui::pos2(x_of(f), eq_y(eq_db(f)))
                })
                .collect();
            p.add(egui::Shape::line(pts, egui::Stroke::new(1.0, EQ_COLOR)));
            format!("EQ ±{:.0} dB", super::yaesu_panel::EQ_CURVE_DB)
        } else {
            rust_i18n::t!("dev_txspec_eq_wav").to_string()
        };
        p.text(egui::pos2(rect.right() - 2.0, rect.top() + 2.0), egui::Align2::RIGHT_TOP, label, font, EQ_COLOR);
    }

    // The pointer: the frequency and level under it, and each line's level there.
    if let Some(pos) = response.hover_pos() {
        let f = axis.at((pos.x - rect.left()) / rect.width());
        let cursor_db = DB_TOP - (pos.y - rect.top()) / rect.height() * (DB_TOP - DB_BOTTOM);
        let hair = egui::Stroke::new(1.0, v.text_color().gamma_multiply(0.4));
        p.line_segment([egui::pos2(pos.x, rect.top()), egui::pos2(pos.x, rect.bottom())], hair);
        p.line_segment([egui::pos2(rect.left(), pos.y), egui::pos2(rect.right(), pos.y)], hair);
        // The column under the pointer, read as the line draws it.
        let c = ((pos.x - rect.left()).max(0.0) as usize).min(cols - 1);
        let dbfs = |bins: &[f32]| column_db(bins, axis, c, cols).map(|d| format!("{d:.1} dBFS"));
        let mut rows: Vec<(String, egui::Color32)> =
            vec![(format!("{f:.0} Hz"), v.strong_text_color()), (format!("{cursor_db:.0} dBFS"), v.text_color())];
        if view.show_live {
            if let Some(t) = dbfs(&view.live_db) {
                rows.push((format!("{} {t}", rust_i18n::t!("dev_txspec_live")), live_color));
            }
        }
        if let Some(o) = &view.over {
            if view.show_over {
                if let Some(t) = dbfs(&o.avg) {
                    rows.push((format!("{} {t}", rust_i18n::t!("dev_txspec_over")), OVER_COLOR));
                }
            }
            if view.show_peak {
                if let Some(t) = dbfs(&o.peak) {
                    rows.push((format!("{} {t}", rust_i18n::t!("dev_txspec_peak")), PEAK_COLOR));
                }
            }
        }
        if eq_drawn {
            rows.push((format!("EQ {:+.1} dB", eq_db(f)), EQ_COLOR));
        }
        let font = egui::FontId::proportional(11.0);
        let galleys: Vec<_> = rows.into_iter().map(|(t, c)| p.layout_no_wrap(t, font.clone(), c)).collect();
        let w = galleys.iter().map(|g| g.size().x).fold(0.0, f32::max);
        let h: f32 = galleys.iter().map(|g| g.size().y).sum();
        let x = if pos.x + 12.0 + w < rect.right() { pos.x + 12.0 } else { pos.x - 12.0 - w };
        let y = (pos.y - 8.0 - h).max(rect.top() + 2.0);
        p.rect_filled(egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h)).expand(3.0), 2.0, v.window_fill);
        let mut y = y;
        for g in galleys {
            let gh = g.size().y;
            p.galley(egui::pos2(x, y), g, v.text_color());
            y += gh;
        }
    }
}

/// The strip under the plot: drag it to make the plot taller or lower.
fn resize_handle(ui: &mut egui::Ui, view: &mut TxSpectrumView, width: f32) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 8.0), egui::Sense::drag());
    if response.dragged() {
        view.height = (view.height + response.drag_delta().y).clamp(HEIGHT_MIN, HEIGHT_MAX);
    }
    let active = response.hovered() || response.dragged();
    if active {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
    }
    let v = ui.visuals();
    let color = if active { v.strong_text_color() } else { v.weak_text_color() };
    for dy in [-1.5, 1.5] {
        let y = rect.center().y + dy;
        ui.painter().line_segment([egui::pos2(rect.center().x - 16.0, y), egui::pos2(rect.center().x + 16.0, y)], egui::Stroke::new(1.0, color));
    }
    response.on_hover_text(rust_i18n::t!("dev_txspec_resize").to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdr_remote_logic::tx_tap::Source;

    fn sine(hz: f32, amp: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| amp * (2.0 * std::f32::consts::PI * hz * i as f32 / SAMPLE_RATE as f32).sin()).collect()
    }

    fn peak_bin(db: &[f32]) -> usize {
        db.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0
    }

    fn tone_frame() -> Vec<i16> {
        sine(1000.0, 0.5, 320).iter().map(|x| (x * 32767.0) as i16).collect()
    }

    /// A full-scale sine reads about 0 dBFS at its own frequency.
    #[test]
    fn a_full_scale_sine_reads_zero_dbfs_at_its_frequency() {
        let a = Analyzer::default();
        let db = a.live_db(&sine(1000.0, 1.0, FFT_SIZE));
        let i = peak_bin(&db);
        assert!((bin_hz(i) - 1000.0).abs() <= bin_hz(1), "peak at {} Hz", bin_hz(i));
        assert!(db[i] > -1.5 && db[i] < 0.5, "{} dBFS", db[i]);
    }

    /// Half the amplitude is 6 dB lower; silence is far down.
    #[test]
    fn the_level_scales_in_db() {
        let a = Analyzer::default();
        let full = a.live_db(&sine(2000.0, 1.0, FFT_SIZE));
        let half = a.live_db(&sine(2000.0, 0.5, FFT_SIZE));
        let i = peak_bin(&full);
        assert!((full[i] - half[i] - 6.02).abs() < 0.3, "{} vs {}", full[i], half[i]);
        let quiet = a.live_db(&vec![0.0; FFT_SIZE]);
        assert!(quiet.iter().all(|&d| d < -100.0));
    }

    /// The over: the average of a steady sine equals one FFT of it, the peak is
    /// never below the average, and an over shorter than one FFT gives nothing.
    #[test]
    fn the_over_averages_and_holds_the_peak() {
        let a = Analyzer::default();
        let s = sine(700.0, 0.5, FFT_SIZE * 4);
        let (avg, peak) = a.over_db(&s).expect("long enough");
        let one = a.live_db(&s[..FFT_SIZE]);
        let i = peak_bin(&one);
        assert!((avg[i] - one[i]).abs() < 0.5, "{} vs {}", avg[i], one[i]);
        assert!(peak.iter().zip(&avg).all(|(p, a)| p + 1e-3 >= *a));
        assert!(a.over_db(&s[..FFT_SIZE - 1]).is_none());
    }

    /// The view takes the over from the tap once it ended, and the live
    /// spectrum only while keyed.
    #[test]
    fn the_view_takes_the_over_once_it_ended() {
        let tap = TxTap::new();
        tap.set_enabled(true);
        for _ in 0..10 {
            tap.push(0, &tone_frame(), Source::Mic);
        }
        let a = Analyzer::default();
        let mut view = TxSpectrumView::default();
        view.update(&a, &tap, 0);
        assert!(!view.live_db.is_empty(), "live while keyed");
        assert!(view.over.is_none(), "the over still runs");
        std::thread::sleep(sdr_remote_logic::tx_tap::OVER_GAP + Duration::from_millis(50));
        view.update(&a, &tap, 0);
        let over = view.over.as_ref().expect("the over ended");
        assert!((over.seconds - 0.2).abs() < 0.01);
    }

    /// The tap copies only for a spectrum that is switched on and was drawn
    /// lately: a shut window or a folded section stops drawing, and so the copy.
    #[test]
    fn the_tap_is_wanted_only_while_a_spectrum_is_drawn() {
        let now = Instant::now();
        let mut views: [TxSpectrumView; 2] = Default::default();
        assert!(!tap_wanted(&views, now), "nothing open");
        views[1].open = true;
        assert!(!tap_wanted(&views, now), "switched on but never drawn");
        views[1].drawn_at = Some(now);
        assert!(tap_wanted(&views, now));
        assert!(!tap_wanted(&views, now + SHOWN_WITHIN), "not drawn for a while: closed");
        views[1].open = false;
        assert!(!tap_wanted(&views, now), "switched off");
    }

    /// Switching off forgets what was shown, the WAV mark too, and keeps the
    /// switches (set away from their defaults, so a reset would show).
    #[test]
    fn clearing_forgets_the_lines_and_keeps_the_switches() {
        let tap = TxTap::new();
        tap.set_enabled(true);
        for _ in 0..10 {
            tap.push(0, &tone_frame(), Source::Wav);
        }
        let a = Analyzer::default();
        assert!(TxSpectrumView::default().log_freq, "logarithmic by default");
        let mut view = TxSpectrumView { range_khz: 8, log_freq: false, ..Default::default() };
        view.update(&a, &tap, 0);
        std::thread::sleep(sdr_remote_logic::tx_tap::OVER_GAP + Duration::from_millis(50));
        view.update(&a, &tap, 0);
        view.drawn_at = Some(Instant::now());
        assert!(!view.live_db.is_empty() && view.over.is_some() && view.without_eq());
        view.clear();
        assert!(view.live_db.is_empty() && view.over.is_none() && view.drawn_at.is_none());
        assert!(!view.without_eq(), "the WAV mark is forgotten too");
        assert_eq!((view.range_khz, view.log_freq), (8, false));
    }

    /// The EQ is not drawn over what a WAV sent: while keyed the newest frame
    /// decides, after the release the last over.
    #[test]
    fn the_eq_is_left_off_over_a_wav() {
        let mut view = TxSpectrumView::default();
        assert!(!view.without_eq());
        view.keyed = true;
        view.live_wav = true;
        assert!(view.without_eq(), "a WAV is being sent");
        view.keyed = false;
        view.over = Some(OverSpectra { avg: vec![], peak: vec![], seconds: 1.0, wav: false });
        assert!(!view.without_eq(), "the last over was the microphone");
        view.over.as_mut().unwrap().wav = true;
        view.live_wav = false;
        assert!(view.without_eq(), "the last over had a WAV in it");
    }

    /// From the tap to the view: frames of a WAV leave the EQ off while keyed
    /// and after the release; the microphone after it brings the EQ back.
    #[test]
    fn the_view_takes_the_wav_mark_from_the_tap() {
        let tap = TxTap::new();
        tap.set_enabled(true);
        let a = Analyzer::default();
        let mut view = TxSpectrumView::default();
        for _ in 0..10 {
            tap.push(0, &tone_frame(), Source::Wav);
        }
        view.update(&a, &tap, 0);
        assert!(view.without_eq(), "keyed on a WAV");
        std::thread::sleep(sdr_remote_logic::tx_tap::OVER_GAP + Duration::from_millis(50));
        view.update(&a, &tap, 0);
        assert!(view.over.as_ref().is_some_and(|o| o.wav) && view.without_eq(), "released after a WAV");
        for _ in 0..10 {
            tap.push(0, &tone_frame(), Source::Mic);
        }
        view.update(&a, &tap, 0);
        assert!(!view.without_eq(), "keyed on the microphone");
        std::thread::sleep(sdr_remote_logic::tx_tap::OVER_GAP + Duration::from_millis(50));
        view.update(&a, &tap, 0);
        assert!(!view.without_eq(), "released after the microphone");
    }

    /// A spectrum drawn again after its window was shut starts empty, as after
    /// unticking; drawn frame after frame it keeps what it has.
    #[test]
    fn drawn_again_after_a_while_it_starts_empty() {
        let now = Instant::now();
        let mut view = TxSpectrumView { range_khz: 8, ..Default::default() };
        view.live_db = vec![-20.0; 4];
        view.mark_drawn(now);
        view.mark_drawn(now + Duration::from_millis(500));
        assert!(!view.live_db.is_empty(), "drawn on: kept");
        view.mark_drawn(now + Duration::from_millis(500) + SHOWN_WITHIN);
        assert!(view.live_db.is_empty(), "not drawn for a while: cleared");
        assert_eq!(view.range_khz, 8, "the switches stay");
        assert!(view.drawn_at.is_some());
    }

    /// The level of a column is its highest bin: every column that holds the
    /// top of a tone reads the top, on both axes, so the pointer reads the
    /// line it is on.
    #[test]
    fn a_column_reads_the_highest_bin_in_it() {
        let a = Analyzer::default();
        let db = a.live_db(&sine(1000.0, 1.0, FFT_SIZE));
        let top = peak_bin(&db);
        for log in [false, true] {
            for cols in [240usize, 520] {
                let axis = Axis { hi: 4000.0, log };
                let mut holding = 0;
                for c in 0..cols {
                    let f0 = axis.at(c as f32 / cols as f32) / bin_hz(1);
                    let f1 = axis.at((c + 1) as f32 / cols as f32) / bin_hz(1);
                    if (f0.floor() as usize..(f1.ceil() as usize).max(f0.floor() as usize + 1)).contains(&top) {
                        holding += 1;
                        assert_eq!(column_db(&db, axis, c, cols), Some(db[top]), "{log} {cols} column {c}");
                    }
                }
                assert!(holding >= 1);
            }
        }
        assert_eq!(column_db(&db, Axis { hi: 8000.0, log: false }, 519, 520).map(|_| ()), Some(()), "the last column");
    }

    /// Both axes: the ends sit at the edges, `at` undoes `frac`, and on the
    /// logarithmic axis each decade takes the same width.
    #[test]
    fn the_frequency_axis_maps_both_ways() {
        for log in [false, true] {
            let ax = Axis { hi: 4000.0, log };
            assert_eq!(ax.frac(ax.lo()), 0.0);
            assert_eq!(ax.frac(4000.0), 1.0);
            for f in [100.0, 1000.0, 3000.0] {
                assert!((ax.at(ax.frac(f)) - f).abs() < 0.5, "{log} {f}");
            }
        }
        let ax = Axis { hi: 8000.0, log: true };
        let decade = ax.frac(1000.0) - ax.frac(100.0);
        assert!((ax.frac(500.0) - ax.frac(50.0) - decade).abs() < 1e-5);
        let lin = Axis { hi: 4000.0, log: false };
        assert!((lin.frac(1000.0) - 0.25).abs() < 1e-6);
    }
}
