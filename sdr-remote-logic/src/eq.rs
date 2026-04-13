/// 5-band parametric EQ for Yaesu FM mic audio.
/// Uses biquad filters (Audio EQ Cookbook by Robert Bristow-Johnson).

const NUM_BANDS: usize = 5;

/// Center frequencies for each band (Hz)
pub const BAND_FREQS: [f32; NUM_BANDS] = [100.0, 300.0, 1000.0, 2500.0, 4000.0];

/// Band labels for UI
pub const BAND_LABELS: [&str; NUM_BANDS] = ["100", "300", "1k", "2.5k", "4k"];

/// Biquad filter coefficients
#[derive(Clone, Copy)]
struct BiquadCoeffs {
    b0: f32, b1: f32, b2: f32,
    a1: f32, a2: f32,
}

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

/// Compute peaking EQ biquad coefficients
fn peaking_eq(freq_hz: f32, gain_db: f32, q: f32, sample_rate: f32) -> BiquadCoeffs {
    if gain_db.abs() < 0.01 {
        // Unity: pass-through
        return BiquadCoeffs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };
    }
    let a = 10.0_f32.powf(gain_db / 40.0); // amplitude
    let w0 = 2.0 * std::f32::consts::PI * freq_hz / sample_rate;
    let alpha = w0.sin() / (2.0 * q);

    let b0 = 1.0 + alpha * a;
    let b1 = -2.0 * w0.cos();
    let b2 = 1.0 - alpha * a;
    let a0 = 1.0 + alpha / a;
    let a1 = -2.0 * w0.cos();
    let a2 = 1.0 - alpha / a;

    BiquadCoeffs {
        b0: b0 / a0, b1: b1 / a0, b2: b2 / a0,
        a1: a1 / a0, a2: a2 / a0,
    }
}

/// 5-band parametric equalizer
pub struct Equalizer {
    coeffs: [BiquadCoeffs; NUM_BANDS],
    states: [BiquadState; NUM_BANDS],
    gains_db: [f32; NUM_BANDS],
    sample_rate: f32,
    enabled: bool,
}

impl Equalizer {
    pub fn new(sample_rate: f32) -> Self {
        let unity = BiquadCoeffs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };
        Self {
            coeffs: [unity; NUM_BANDS],
            states: std::array::from_fn(|_| BiquadState::new()),
            gains_db: [0.0; NUM_BANDS],
            sample_rate,
            enabled: false,
        }
    }

    /// Set gain for a specific band (0-4), in dB (-12 to +12)
    pub fn set_band_gain(&mut self, band: usize, gain_db: f32) {
        if band >= NUM_BANDS { return; }
        let gain_db = gain_db.clamp(-12.0, 12.0);
        self.gains_db[band] = gain_db;
        // Q=1.4 gives moderate bandwidth (~1 octave), good for voice EQ
        self.coeffs[band] = peaking_eq(BAND_FREQS[band], gain_db, 1.4, self.sample_rate);
    }

    /// Set all 5 band gains at once
    pub fn set_gains(&mut self, gains: &[f32; NUM_BANDS]) {
        for i in 0..NUM_BANDS {
            self.set_band_gain(i, gains[i]);
        }
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

    pub fn gains(&self) -> &[f32; NUM_BANDS] {
        &self.gains_db
    }

    /// Process audio samples in-place
    pub fn process(&mut self, samples: &mut [f32]) {
        if !self.enabled { return; }
        for sample in samples.iter_mut() {
            let mut s = *sample;
            for i in 0..NUM_BANDS {
                s = self.states[i].process(&self.coeffs[i], s);
            }
            *sample = s;
        }
    }
}
