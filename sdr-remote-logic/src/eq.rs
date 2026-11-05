// SPDX-License-Identifier: GPL-2.0-or-later

/// 20-channel parametric EQ for Yaesu mic audio. Every channel has its own
/// filter kind, frequency, Q and gain. The standard EQ (the phone's, and the
/// desktop's unless Advanced is ticked) is the first five channels on the five
/// fixed bands, only their gain set: `bands_from_legacy_gains`.
/// Uses biquad filters (Audio EQ Cookbook by Robert Bristow-Johnson).

pub const NUM_BANDS: usize = 20;

/// The rate the curve on the screen is drawn at. The engine runs the EQ at
/// the capture rate of the microphone (`Equalizer::set_sample_rate`). A
/// channel sits on the same frequency at both rates as long as it is below
/// 0.45 times the capture rate (`coeffs` moves it down there); its shape
/// differs close to half the capture rate. At 16 kHz the curve holds to within
/// about 1 dB below 3 kHz for a peak, a shelf or a gentle cut; there a steep
/// cut can be a few dB off and a channel in the top octave over ten. Above 3 kHz
/// it can be several dB off, and tens of dB near 8 kHz. At 44.1 and 48 kHz it holds everywhere.
pub const EQ_SAMPLE_RATE: f32 = 48000.0;

/// The five fixed bands of the earlier EQ. Channels 0-4 start on these, as
/// peaking filters with Q 1.4, so a profile saved with five gains sounds the
/// same as before.
pub const LEGACY_FREQS: [f32; 5] = [100.0, 300.0, 1000.0, 2500.0, 4000.0];
pub const LEGACY_LABELS: [&str; 5] = ["100", "300", "1k", "2.5k", "4k"];
const LEGACY_Q: f32 = 1.4;
/// The range of the standard EQ's five sliders, here and on the phone.
pub const STANDARD_GAIN_MAX_DB: f32 = 12.0;

/// The Q a low or high cut gets when it is chosen on a channel whose Q is
/// still the starting value: the flat Butterworth corner, no bump.
pub const CUT_Q: f32 = 0.707;

/// Where the channels after the first five start when switched on: spread
/// over the voice range, roughly a third of an octave apart.
const SPARE_FREQS: [f32; NUM_BANDS - 5] = [
    63.0, 150.0, 200.0, 400.0, 500.0, 630.0, 800.0, 1250.0,
    1600.0, 2000.0, 2800.0, 3200.0, 5000.0, 6300.0, 8000.0,
];

pub const FREQ_MIN: f32 = 20.0;
pub const FREQ_MAX: f32 = 8000.0;
pub const Q_MIN: f32 = 0.1;
pub const Q_MAX: f32 = 20.0;
pub const GAIN_MAX_DB: f32 = 15.0;

/// Filter kind of one channel. Shelves and peaks use the gain; the two
/// passes ignore it (their Q sets the corner: 0.707 is the flat Butterworth).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterKind {
    Off,
    Peak,
    LowShelf,
    HighShelf,
    /// Lets the highs through: cuts below the frequency (low cut).
    HighPass,
    /// Lets the lows through: cuts above the frequency (high cut).
    LowPass,
}

impl FilterKind {
    pub const ALL: [FilterKind; 6] = [
        FilterKind::Off, FilterKind::Peak, FilterKind::LowShelf,
        FilterKind::HighShelf, FilterKind::HighPass, FilterKind::LowPass,
    ];

    /// Short name, used in the settings file and on screen.
    pub fn code(self) -> &'static str {
        match self {
            FilterKind::Off => "off",
            FilterKind::Peak => "peak",
            FilterKind::LowShelf => "lshelf",
            FilterKind::HighShelf => "hshelf",
            FilterKind::HighPass => "lowcut",
            FilterKind::LowPass => "highcut",
        }
    }

    pub fn from_code(s: &str) -> Option<FilterKind> {
        FilterKind::ALL.iter().copied().find(|k| k.code() == s)
    }

    pub fn uses_gain(self) -> bool {
        matches!(self, FilterKind::Peak | FilterKind::LowShelf | FilterKind::HighShelf)
    }

    pub fn is_cut(self) -> bool {
        matches!(self, FilterKind::HighPass | FilterKind::LowPass)
    }
}

/// One EQ channel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EqBand {
    pub kind: FilterKind,
    pub freq_hz: f32,
    pub q: f32,
    pub gain_db: f32,
}

impl EqBand {
    /// The same band with every value inside its range.
    pub fn clamped(self) -> EqBand {
        EqBand {
            kind: self.kind,
            freq_hz: self.freq_hz.clamp(FREQ_MIN, FREQ_MAX),
            q: self.q.clamp(Q_MIN, Q_MAX),
            gain_db: self.gain_db.clamp(-GAIN_MAX_DB, GAIN_MAX_DB),
        }
    }

    /// The same band with another kind. A cut chosen while the Q is still the
    /// starting value gets `CUT_Q`; a Q someone set stays.
    pub fn with_kind(self, kind: FilterKind) -> EqBand {
        let q = if kind.is_cut() && self.q == LEGACY_Q { CUT_Q } else { self.q };
        EqBand { kind, q, ..self }
    }

    /// Settings-file form: `kind:freq:q:gain`.
    pub fn to_field(&self) -> String {
        format!("{}:{:.0}:{:.2}:{:.1}", self.kind.code(), self.freq_hz, self.q, self.gain_db)
    }

    pub fn from_field(s: &str) -> Option<EqBand> {
        let mut it = s.trim().split(':');
        let kind = FilterKind::from_code(it.next()?)?;
        let freq_hz = finite(it.next()?)?;
        let q = finite(it.next()?)?;
        let gain_db = finite(it.next()?)?;
        if it.next().is_some() { return None; }
        Some(EqBand { kind, freq_hz, q, gain_db }.clamped())
    }
}

/// A number from the settings file. `nan` parses, and a NaN passes `clamp`
/// and silences every channel after it, so it is refused here.
fn finite(s: &str) -> Option<f32> {
    s.trim().parse::<f32>().ok().filter(|v| v.is_finite())
}

pub type EqBands = [EqBand; NUM_BANDS];

/// The starting point: channels 0-4 as the old five bands at 0 dB, the rest off.
pub fn default_bands() -> EqBands {
    std::array::from_fn(|i| {
        if i < 5 {
            EqBand { kind: FilterKind::Peak, freq_hz: LEGACY_FREQS[i], q: LEGACY_Q, gain_db: 0.0 }
        } else {
            EqBand { kind: FilterKind::Off, freq_hz: SPARE_FREQS[i - 5], q: LEGACY_Q, gain_db: 0.0 }
        }
    })
}

/// A profile saved with the old five gains, as twenty channels.
pub fn bands_from_legacy_gains(gains: &[f32; 5]) -> EqBands {
    let mut b = default_bands();
    for i in 0..5 {
        b[i].gain_db = gains[i].clamp(-GAIN_MAX_DB, GAIN_MAX_DB);
    }
    b
}

/// The five gains of a set of channels that is the standard EQ: the old five
/// bands as peaks within the standard range, every other channel off. `None`
/// when it is more than that.
pub fn legacy_gains(bands: &EqBands) -> Option<[f32; 5]> {
    let gains: [f32; 5] = std::array::from_fn(|i| bands[i].gain_db);
    let five = (0..5).all(|i| bands[i] == EqBand { gain_db: gains[i], ..default_bands()[i] }
        && gains[i].abs() <= STANDARD_GAIN_MAX_DB);
    let rest_off = bands[5..].iter().all(|b| b.kind == FilterKind::Off);
    (five && rest_off).then_some(gains)
}

/// Settings-file form of all channels: `kind:freq:q:gain;...`.
pub fn bands_to_field(bands: &EqBands) -> String {
    bands.iter().map(EqBand::to_field).collect::<Vec<_>>().join(";")
}

/// Reads either form: twenty `kind:freq:q:gain` channels separated by `;`,
/// or the old five comma-separated gains.
pub fn bands_from_field(s: &str) -> Option<EqBands> {
    let s = s.trim();
    if s.contains(':') {
        let parsed: Vec<EqBand> = s.split(';').map(EqBand::from_field).collect::<Option<_>>()?;
        let mut b = default_bands();
        if parsed.len() > NUM_BANDS { return None; }
        b[..parsed.len()].copy_from_slice(&parsed);
        Some(b)
    } else {
        let gains: Vec<f32> = s.split(',').map(finite).collect::<Option<_>>()?;
        let gains: [f32; 5] = gains.try_into().ok()?;
        Some(bands_from_legacy_gains(&gains))
    }
}

/// Biquad filter coefficients
#[derive(Clone, Copy)]
struct BiquadCoeffs {
    b0: f32, b1: f32, b2: f32,
    a1: f32, a2: f32,
}

const UNITY: BiquadCoeffs = BiquadCoeffs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };

/// Biquad filter state (Direct Form II Transposed)
struct BiquadState {
    z1: f32,
    z2: f32,
}

impl BiquadState {
    fn new() -> Self {
        Self { z1: 0.0, z2: 0.0 }
    }

    fn process(&mut self, c: &BiquadCoeffs, x: f32) -> f32 {
        let y = c.b0 * x + self.z1;
        self.z1 = c.b1 * x - c.a1 * y + self.z2;
        self.z2 = c.b2 * x - c.a2 * y;
        y
    }

    fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }
}

/// Cookbook coefficients for one channel.
fn coeffs(band: &EqBand, sample_rate: f32) -> BiquadCoeffs {
    let band = band.clamped();
    let nyquist_guard = sample_rate * 0.45;
    let f = band.freq_hz.min(nyquist_guard);
    let w0 = 2.0 * std::f32::consts::PI * f / sample_rate;
    let (sin, cos) = (w0.sin(), w0.cos());
    let alpha = sin / (2.0 * band.q);
    let a = 10.0_f32.powf(band.gain_db / 40.0);
    let (b0, b1, b2, a0, a1, a2) = match band.kind {
        FilterKind::Off => return UNITY,
        FilterKind::Peak => {
            if band.gain_db.abs() < 0.01 { return UNITY; }
            (1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a,
             1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a)
        }
        FilterKind::LowShelf => {
            if band.gain_db.abs() < 0.01 { return UNITY; }
            let k = 2.0 * a.sqrt() * alpha;
            (a * ((a + 1.0) - (a - 1.0) * cos + k),
             2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
             a * ((a + 1.0) - (a - 1.0) * cos - k),
             (a + 1.0) + (a - 1.0) * cos + k,
             -2.0 * ((a - 1.0) + (a + 1.0) * cos),
             (a + 1.0) + (a - 1.0) * cos - k)
        }
        FilterKind::HighShelf => {
            if band.gain_db.abs() < 0.01 { return UNITY; }
            let k = 2.0 * a.sqrt() * alpha;
            (a * ((a + 1.0) + (a - 1.0) * cos + k),
             -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
             a * ((a + 1.0) + (a - 1.0) * cos - k),
             (a + 1.0) - (a - 1.0) * cos + k,
             2.0 * ((a - 1.0) - (a + 1.0) * cos),
             (a + 1.0) - (a - 1.0) * cos - k)
        }
        FilterKind::HighPass => (
            (1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0,
            1.0 + alpha, -2.0 * cos, 1.0 - alpha),
        FilterKind::LowPass => (
            (1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0,
            1.0 + alpha, -2.0 * cos, 1.0 - alpha),
    };
    BiquadCoeffs { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
}

/// Magnitude of one biquad at `freq_hz`, in dB.
fn magnitude_db(c: &BiquadCoeffs, freq_hz: f32, sample_rate: f32) -> f32 {
    let w = 2.0 * std::f64::consts::PI * freq_hz as f64 / sample_rate as f64;
    let (c1, s1, c2, s2) = (w.cos(), -w.sin(), (2.0 * w).cos(), -(2.0 * w).sin());
    let (b0, b1, b2, a1, a2) = (c.b0 as f64, c.b1 as f64, c.b2 as f64, c.a1 as f64, c.a2 as f64);
    let num_re = b0 + b1 * c1 + b2 * c2;
    let num_im = b1 * s1 + b2 * s2;
    let den_re = 1.0 + a1 * c1 + a2 * c2;
    let den_im = a1 * s1 + a2 * s2;
    let num = num_re * num_re + num_im * num_im;
    let den = (den_re * den_re + den_im * den_im).max(1e-30);
    (10.0 * (num / den).max(1e-30).log10()) as f32
}

/// The total response of a set of channels at `freq_hz`, in dB: what the
/// curve on the screen draws.
pub fn response_db(bands: &EqBands, freq_hz: f32, sample_rate: f32) -> f32 {
    bands.iter().map(|b| magnitude_db(&coeffs(b, sample_rate), freq_hz, sample_rate)).sum()
}

/// 20-channel parametric equalizer
pub struct Equalizer {
    bands: EqBands,
    coeffs: [BiquadCoeffs; NUM_BANDS],
    states: [BiquadState; NUM_BANDS],
    sample_rate: f32,
    enabled: bool,
}

impl Equalizer {
    pub fn new(sample_rate: f32) -> Self {
        let sample_rate = if sample_rate > 0.0 { sample_rate } else { EQ_SAMPLE_RATE };
        let bands = default_bands();
        Self {
            coeffs: std::array::from_fn(|i| coeffs(&bands[i], sample_rate)),
            bands,
            states: std::array::from_fn(|_| BiquadState::new()),
            sample_rate,
            enabled: false,
        }
    }

    /// Set one channel completely.
    pub fn set_band(&mut self, band: usize, params: EqBand) {
        if band >= NUM_BANDS { return; }
        let params = params.clamped();
        let kind_changed = self.bands[band].kind != params.kind;
        self.bands[band] = params;
        self.coeffs[band] = coeffs(&params, self.sample_rate);
        if kind_changed {
            self.states[band].reset();
        }
    }

    /// Follow a new capture rate: the same channels, coefficients for the new
    /// rate, and the filter memory cleared. Returns whether the rate changed.
    pub fn set_sample_rate(&mut self, sample_rate: f32) -> bool {
        if !(sample_rate > 0.0) || sample_rate == self.sample_rate { return false; }
        self.sample_rate = sample_rate;
        for i in 0..NUM_BANDS {
            self.coeffs[i] = coeffs(&self.bands[i], sample_rate);
            self.states[i].reset();
        }
        true
    }

    /// Set only the gain of a channel, in dB (the phone's five sliders).
    pub fn set_band_gain(&mut self, band: usize, gain_db: f32) {
        if band >= NUM_BANDS { return; }
        let mut b = self.bands[band];
        b.gain_db = gain_db;
        self.set_band(band, b);
    }

    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        if !on {
            for s in &mut self.states {
                s.reset();
            }
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn bands(&self) -> &EqBands {
        &self.bands
    }

    /// Process audio samples in-place
    pub fn process(&mut self, samples: &mut [f32]) {
        if !self.enabled { return; }
        for sample in samples.iter_mut() {
            let mut s = *sample;
            for i in 0..NUM_BANDS {
                if self.bands[i].kind != FilterKind::Off {
                    s = self.states[i].process(&self.coeffs[i], s);
                }
            }
            *sample = s;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48000.0;

    #[test]
    fn the_default_is_flat() {
        let b = default_bands();
        for f in [50.0, 300.0, 1000.0, 3000.0, 7000.0] {
            assert!(response_db(&b, f, SR).abs() < 0.01, "{f} Hz");
        }
    }

    #[test]
    fn a_peak_reaches_its_gain_at_its_frequency() {
        let mut b = default_bands();
        b[7] = EqBand { kind: FilterKind::Peak, freq_hz: 1800.0, q: 2.0, gain_db: 6.0 };
        assert!((response_db(&b, 1800.0, SR) - 6.0).abs() < 0.05);
        assert!(response_db(&b, 200.0, SR).abs() < 0.3);
    }

    #[test]
    fn the_cuts_and_shelves_go_the_right_way() {
        let mut b = default_bands();
        b[5] = EqBand { kind: FilterKind::HighPass, freq_hz: 150.0, q: 0.707, gain_db: 0.0 };
        assert!(response_db(&b, 40.0, SR) < -18.0, "low cut lets the rumble through");
        assert!(response_db(&b, 1000.0, SR).abs() < 0.2);
        let mut b = default_bands();
        b[5] = EqBand { kind: FilterKind::LowPass, freq_hz: 3000.0, q: 0.707, gain_db: 0.0 };
        assert!(response_db(&b, 7000.0, SR) < -10.0, "high cut");
        assert!(response_db(&b, 500.0, SR).abs() < 0.2);
        let mut b = default_bands();
        b[5] = EqBand { kind: FilterKind::LowShelf, freq_hz: 300.0, q: 0.707, gain_db: -6.0 };
        b[6] = EqBand { kind: FilterKind::HighShelf, freq_hz: 2000.0, q: 0.707, gain_db: 4.0 };
        assert!((response_db(&b, 40.0, SR) + 6.0).abs() < 0.3);
        assert!((response_db(&b, 7000.0, SR) - 4.0).abs() < 0.5);
    }

    #[test]
    fn the_processed_signal_follows_the_response() {
        let mut eq = Equalizer::new(SR);
        eq.set_enabled(true);
        eq.set_band(3, EqBand { kind: FilterKind::Peak, freq_hz: 1000.0, q: 1.4, gain_db: 12.0 });
        let n = 48000;
        let mut x: Vec<f32> = (0..n).map(|i| 0.1 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / SR).sin()).collect();
        eq.process(&mut x);
        let peak = x[n / 2..].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        let want = 0.1 * 10f32.powf(response_db(eq.bands(), 1000.0, SR) / 20.0);
        assert!((peak - want).abs() / want < 0.02, "{peak} vs {want}");
    }

    #[test]
    fn a_saved_profile_reads_back_in_both_forms() {
        let mut b = default_bands();
        b[9] = EqBand { kind: FilterKind::HighShelf, freq_hz: 2500.0, q: 0.9, gain_db: -3.5 };
        assert_eq!(bands_from_field(&bands_to_field(&b)), Some(b));
        let old = bands_from_field("1.0,-2.0,0.0,3.0,4.0").unwrap();
        assert_eq!(old[1].gain_db, -2.0);
        assert_eq!(old[1].freq_hz, 300.0);
        assert_eq!(old[5].kind, FilterKind::Off);
        assert_eq!(bands_from_field("1,2,3"), None);
        assert_eq!(bands_from_field("peak:100:1:0;bogus"), None);
    }

    #[test]
    fn the_first_five_channels_are_the_old_five_bands() {
        let b = default_bands();
        for (i, f) in [100.0, 300.0, 1000.0, 2500.0, 4000.0].into_iter().enumerate() {
            assert_eq!(b[i], EqBand { kind: FilterKind::Peak, freq_hz: f, q: 1.4, gain_db: 0.0 });
        }
    }

    #[test]
    fn a_value_out_of_range_is_clamped_and_a_nan_is_refused() {
        let b = EqBand::from_field("peak:99999:50:40").unwrap();
        assert_eq!((b.freq_hz, b.q, b.gain_db), (FREQ_MAX, Q_MAX, GAIN_MAX_DB));
        let b = EqBand::from_field("peak:1:0.01:-40").unwrap();
        assert_eq!((b.freq_hz, b.q, b.gain_db), (FREQ_MIN, Q_MIN, -GAIN_MAX_DB));
        assert_eq!(EqBand::from_field("peak:1000:nan:3"), None);
        assert_eq!(EqBand::from_field("peak:1000:1:inf"), None);
        assert_eq!(bands_from_field("nan,0,0,0,0"), None);
    }

    #[test]
    fn a_shelf_is_halfway_at_its_frequency() {
        let mut b = default_bands();
        b[5] = EqBand { kind: FilterKind::LowShelf, freq_hz: 300.0, q: 0.707, gain_db: -6.0 };
        assert!((response_db(&b, 300.0, SR) + 3.0).abs() < 0.05);
        let mut b = default_bands();
        b[5] = EqBand { kind: FilterKind::HighShelf, freq_hz: 2000.0, q: 0.707, gain_db: 8.0 };
        assert!((response_db(&b, 2000.0, SR) - 4.0).abs() < 0.05);
    }

    /// Halfway at its frequency is true of any shelf; the slope beside it is
    /// what the code sets. A cut shelf never lifts anything, and has its full
    /// cut an octave and a half away.
    #[test]
    fn a_cut_shelf_never_lifts_and_reaches_its_cut() {
        for kind in [FilterKind::LowShelf, FilterKind::HighShelf] {
            let mut b = default_bands();
            b[5] = EqBand { kind, freq_hz: 300.0, q: 0.707, gain_db: -6.0 };
            let mut f = 20.0;
            while f < 8000.0 {
                assert!(response_db(&b, f, SR) < 0.01, "{kind:?} lifts {f} Hz");
                f *= 1.05;
            }
            let far = if kind == FilterKind::LowShelf { 300.0 / 3.0 } else { 300.0 * 3.0 };
            assert!((response_db(&b, far, SR) + 6.0).abs() < 1.0, "{kind:?} at {far} Hz");
        }
    }
    #[test]
    fn a_cut_chosen_on_the_starting_q_gets_the_flat_corner() {
        let start = default_bands()[7];
        assert_eq!(start.with_kind(FilterKind::HighPass).q, CUT_Q);
        assert_eq!(start.with_kind(FilterKind::LowPass).q, CUT_Q);
        assert_eq!(start.with_kind(FilterKind::Peak).q, 1.4);
        assert_eq!(start.with_kind(FilterKind::LowShelf).q, 1.4);
        assert_eq!(start.with_kind(FilterKind::HighShelf).q, 1.4);
        let set = EqBand { q: 2.0, ..start };
        assert_eq!(set.with_kind(FilterKind::HighPass).q, 2.0);
        let mut b = default_bands();
        b[5] = default_bands()[5].with_kind(FilterKind::HighPass);
        b[5].freq_hz = 1000.0;
        let corner = response_db(&b, 1000.0, SR);
        assert!((corner + 3.01).abs() < 0.1, "the flat corner, {corner} dB");
    }

    /// After a ringing peak, a channel switched to another kind starts from
    /// silence; the same kind keeps its memory, so dragging does not click.
    #[test]
    fn a_kind_change_clears_the_filter_memory_and_a_drag_does_not() {
        for (new_kind, quiet) in [(FilterKind::HighPass, true), (FilterKind::Peak, false)] {
            let mut eq = Equalizer::new(SR);
            eq.set_enabled(true);
            let peak = EqBand { kind: FilterKind::Peak, freq_hz: 1000.0, q: 8.0, gain_db: 12.0 };
            eq.set_band(7, peak);
            let mut x: Vec<f32> = (0..480).map(|i| (i as f32 * 0.13).sin()).collect();
            eq.process(&mut x);
            eq.set_band(7, EqBand { kind: new_kind, freq_hz: 1100.0, ..peak });
            let mut z = vec![0.0f32; 16];
            eq.process(&mut z);
            assert_eq!(z.iter().all(|v| *v == 0.0), quiet, "{new_kind:?}");
        }
    }

    #[test]
    fn the_equalizer_follows_the_capture_rate() {
        let sr = 16000.0;
        let mut eq = Equalizer::new(SR);
        eq.set_enabled(true);
        eq.set_band(3, EqBand { kind: FilterKind::Peak, freq_hz: 1000.0, q: 1.4, gain_db: 12.0 });
        assert!(eq.set_sample_rate(sr));
        assert!(!eq.set_sample_rate(sr));
        assert!(!eq.set_sample_rate(0.0));
        assert!(!eq.set_sample_rate(f32::NAN));
        let n = 16000;
        let mut x: Vec<f32> = (0..n).map(|i| 0.1 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr).sin()).collect();
        eq.process(&mut x);
        let peak = x[n / 2..].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        let want = 0.1 * 10f32.powf(12.0 / 20.0);
        assert!((peak - want).abs() / want < 0.02, "{peak} vs {want}");
    }

    /// Below 0.45 times the rate a channel keeps its frequency; above it, it
    /// is moved down to that point (the curve on the screen does not show this).
    #[test]
    fn a_channel_above_the_guard_moves_down_to_it() {
        let mut b = default_bands();
        b[19] = EqBand { kind: FilterKind::Peak, freq_hz: 8000.0, q: 1.4, gain_db: 12.0 };
        assert!((response_db(&b, 7200.0, 16000.0) - 12.0).abs() < 0.1);
        assert!((response_db(&b, 8000.0, SR) - 12.0).abs() < 0.1);
    }

    /// A saved profile holds these words; renaming one loses it at the next start.
    #[test]
    fn the_codes_in_the_settings_file_stay_as_written() {
        let codes: Vec<&str> = FilterKind::ALL.iter().map(|k| k.code()).collect();
        assert_eq!(codes, ["off", "peak", "lshelf", "hshelf", "lowcut", "highcut"]);
        let b = bands_from_field("lowcut:120:0.71:0;highcut:3000:0.71:0;off:1000:1.4:0").unwrap();
        assert_eq!((b[0].kind, b[1].kind, b[2].kind), (FilterKind::HighPass, FilterKind::LowPass, FilterKind::Off));
    }

    /// The labels over the five sliders name the frequencies they filter.
    #[test]
    fn each_standard_label_names_its_band() {
        for (label, freq) in LEGACY_LABELS.iter().zip(LEGACY_FREQS) {
            let hz = match label.strip_suffix('k') {
                Some(k) => k.parse::<f32>().unwrap() * 1000.0,
                None => label.parse::<f32>().unwrap(),
            };
            assert_eq!(hz, freq, "label {label}");
        }
    }

    /// The standard EQ is the old five bands and nothing else; a set of
    /// channels that is more stays advanced.
    #[test]
    fn the_standard_eq_is_the_five_bands_and_nothing_else() {
        let gains = [3.0, -2.0, 0.0, 6.5, -12.0];
        assert_eq!(legacy_gains(&bands_from_legacy_gains(&gains)), Some(gains));
        assert_eq!(legacy_gains(&default_bands()), Some([0.0; 5]));
        let mut b = bands_from_legacy_gains(&gains);
        b[2].freq_hz = 1100.0;
        assert_eq!(legacy_gains(&b), None, "a moved band");
        for spare in [5, 12, NUM_BANDS - 1] {
            let mut b = bands_from_legacy_gains(&gains);
            b[spare].kind = FilterKind::HighPass;
            assert_eq!(legacy_gains(&b), None, "spare channel {} on", spare + 1);
        }
        let mut b = bands_from_legacy_gains(&gains);
        b[0].gain_db = 14.0;
        assert_eq!(legacy_gains(&b), None, "beyond the standard sliders");
    }
}
