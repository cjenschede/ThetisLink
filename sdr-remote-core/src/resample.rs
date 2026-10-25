// SPDX-License-Identifier: GPL-2.0-or-later

//! Every audio resampler in the server and the apps is built here, and gets
//! its filter from the direction it goes (build 260).
//!
//! **Down** (the output slower than the input) always gets the anti-alias
//! filter: the server's receive audio from Thetis and the Yaesu radios, the
//! microphone in the apps, and - on a headset that plays at 8 kHz - the apps'
//! playback of a 16 kHz stream. **Up** (or the same rate) keeps the filter its
//! place chose: short in the apps' playback, for the delay; long towards the
//! radio. Until build 259 eighteen resamplers were built by hand in eight
//! places, and the playback one went down on such a headset with the short
//! filter, which lets a tone just past the edge come back.
//!
//! Down, measured with pure tones (2026-10-20, outside the repo). The old
//! filter, sinc 128 with the cutoff at 0.95 of the new Nyquist, let a tone
//! just above the edge come back below it at -14 dB on the way to 8 kHz and at
//! -23 dB on the way to 16 kHz: rubato's sinc length counts input samples, so
//! the same length is a shorter filter the larger the ratio. Now:
//!
//! | to | filter | tone just past the edge | further out | top of the band |
//! |---|---|---|---|---|
//! | 8 kHz | sinc 256, cutoff 0.90 | -69 dB | -84 dB | 3.4 kHz -1.2 dB |
//! | 16 kHz | sinc 128, cutoff 0.90 | -55 dB | -84 dB | 6.8 kHz -1.2 dB |
//!
//! Delay is half the filter, in input samples: rubato gives the first call
//! that many samples less and every later frame ends that much later (an
//! impulse at the end of one input frame came out 21 samples into the next
//! output frame at 8 kHz, against 10 before). So the longer filter for 8 kHz
//! costs about 1.3 ms, which is what `output_delay` says. Sample positions in
//! a concatenated output stay aligned, so a test that lines the stream up
//! afterwards sees no delay at all - that is how it was first measured wrong.
//! Playback going down 16 to 8 kHz pays 64 input samples instead of 16, ~3 ms,
//! and only on such a headset.
//!
//! Up, the images measured -66 dB and lower with either filter.
//!
//! **The same rate** gets no filter at all (build 261): the samples go through
//! untouched. Until then a microphone that records at 16 kHz went through sinc
//! 128 for nothing, 4 ms of delay, and narrowband playback at 8 kHz through
//! sinc 32, 1 ms.
//!
//! A `Resampler` knows its own filter, and the log lines read it from there:
//! a line that asked again with the same numbers could say one thing while the
//! resampler did another (round 260).

use std::fmt;

use anyhow::{Context, Result};
use rubato::{SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction};

/// The filter a resampler gets when it goes up or stays at the same rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UpFilter {
    /// sinc 32, cutoff 0.90: the apps' playback, where the delay counts most
    /// (2.0 ms 8 to 48 kHz).
    Short,
    /// sinc 128, cutoff 0.95: towards a radio, a microphone below 16 kHz, a
    /// recording played alongside.
    Long,
}

/// The filter a resampler gets.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Filter {
    pub down: bool,
    pub sinc_len: usize,
    pub cutoff: f32,
    pub oversampling: usize,
}

impl Filter {
    pub fn direction(&self) -> &'static str {
        if self.down { "down" } else { "up" }
    }
}

impl fmt::Display for Filter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sinc {}, cutoff {:.2}", self.sinc_len, self.cutoff)
    }
}

/// The filter for going from `from_hz` to `to_hz`; none at the same rate.
/// Down: past a ratio of three (48 or 44.1 kHz to 8 kHz) twice as long, so
/// that it is as steep at the lower rate as the short one is at 16 kHz; `up`
/// is not looked at.
pub fn filter(from_hz: u32, to_hz: u32, up: UpFilter) -> Option<Filter> {
    if to_hz == from_hz {
        return None;
    }
    if to_hz < from_hz {
        let sinc_len = if from_hz > 3 * to_hz { 256 } else { 128 };
        return Some(Filter { down: true, sinc_len, cutoff: 0.90, oversampling: 128 });
    }
    Some(match up {
        UpFilter::Short => Filter { down: false, sinc_len: 32, cutoff: 0.90, oversampling: 32 },
        UpFilter::Long => Filter { down: false, sinc_len: 128, cutoff: 0.95, oversampling: 128 },
    })
}

/// One resampler: rubato's sinc with the filter `filter` chose, or nothing at
/// the same rate.
pub struct Resampler {
    sinc: Option<SincFixedIn<f32>>,
    filter: Option<Filter>,
}

impl Resampler {
    /// The filter it was built with; `None` when it passes the samples through.
    pub fn filter(&self) -> Option<Filter> {
        self.filter
    }

    /// One frame of mono samples. Rubato gives its first frame half a filter
    /// short; at the same rate everything comes out as it went in.
    pub fn process(&mut self, input: Vec<f32>) -> Result<Vec<f32>> {
        match &mut self.sinc {
            None => Ok(input),
            Some(sinc) => {
                use rubato::Resampler as _;
                Ok(sinc.process(&[input], None)?.into_iter().next().unwrap_or_default())
            }
        }
    }
}

/// For a log line: "down with sinc 128, cutoff 0.90", or "1:1, no resampling".
impl fmt::Display for Resampler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.filter {
            None => write!(f, "1:1, no resampling"),
            Some(filter) => write!(f, "{} with {}", filter.direction(), filter),
        }
    }
}

/// A resampler from `from_hz` to `to_hz`, fed `chunk` samples at a time.
pub fn resampler(from_hz: u32, to_hz: u32, chunk: usize, up: UpFilter) -> Result<Resampler> {
    let Some(f) = filter(from_hz, to_hz, up) else {
        return Ok(Resampler { sinc: None, filter: None });
    };
    let sinc = SincFixedIn::<f32>::new(
        to_hz as f64 / from_hz as f64,
        1.0,
        SincInterpolationParameters {
            sinc_len: f.sinc_len,
            f_cutoff: f.cutoff,
            oversampling_factor: f.oversampling,
            interpolation: SincInterpolationType::Cubic,
            window: WindowFunction::Blackman,
        },
        chunk,
        1,
    )
    .with_context(|| format!("resampler {} Hz to {} Hz", from_hz, to_hz))?;
    Ok(Resampler { sinc: Some(sinc), filter: Some(f) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(f: f64, hz: u32) -> Vec<f32> {
        (0..hz as usize).map(|i| (0.5 * (2.0 * std::f64::consts::PI * f * i as f64 / hz as f64).sin()) as f32).collect()
    }

    /// Frame by frame, 20 ms at a time, as every caller feeds it.
    fn through(x: &[f32], from: u32, to: u32, up: UpFilter) -> Vec<f32> {
        let chunk = (from / 50) as usize;
        let mut r = resampler(from, to, chunk, up).unwrap();
        let mut out = Vec::new();
        for c in x.chunks_exact(chunk) {
            out.extend_from_slice(&r.process(c.to_vec()).unwrap());
        }
        out
    }

    /// Level in dB, against the 0.5 of the tone, of `f` in the second half of `y`.
    fn level(y: &[f32], f: f64, hz: u32) -> f64 {
        let s = &y[y.len() / 2..];
        let n = s.len() as f64;
        let (mut re, mut im, mut w) = (0.0, 0.0, 0.0);
        for (i, &v) in s.iter().enumerate() {
            let h = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n).cos();
            let ph = 2.0 * std::f64::consts::PI * f * i as f64 / hz as f64;
            re += v as f64 * h * ph.cos();
            im += v as f64 * h * ph.sin();
            w += h;
        }
        20.0 * ((2.0 * (re * re + im * im).sqrt() / w) / 0.5).max(1e-9).log10()
    }

    /// Where a tone at `f` lands at the rate `hz`.
    fn fold(f: f64, hz: u32) -> f64 {
        let m = f % hz as f64;
        if m > hz as f64 / 2.0 { hz as f64 - m } else { m }
    }

    /// The loudest alias of a tone just past the new Nyquist (up to 15 % past
    /// it), and of one further out, up to the old Nyquist.
    fn worst_aliases(from: u32, to: u32, up: UpFilter) -> (f64, f64) {
        let nyq = to as f64 / 2.0;
        let (mut edge, mut past) = (-200.0f64, -200.0f64);
        let mut f = nyq + 100.0;
        while f < from as f64 / 2.0 {
            let l = level(&through(&tone(f, from), from, to, up), fold(f, to), to);
            if f < nyq * 1.15 { edge = edge.max(l) } else { past = past.max(l) }
            f += if f < nyq * 1.15 { 100.0 } else { 500.0 };
        }
        (edge, past)
    }

    #[test]
    fn going_down_to_8k_keeps_the_aliases_out() {
        for from in [48_000, 44_100] {
            let (edge, past) = worst_aliases(from, 8_000, UpFilter::Long);
            assert!(edge < -60.0, "{from}: a tone just past 4 kHz comes back at {edge:.1} dB");
            assert!(past < -75.0, "{from}: a tone further out comes back at {past:.1} dB");
            let pass = level(&through(&tone(3_000.0, from), from, 8_000, UpFilter::Long), 3_000.0, 8_000);
            assert!(pass.abs() < 0.1, "{from}: 3 kHz is at {pass:.2} dB");
        }
    }

    #[test]
    fn going_down_to_16k_keeps_the_aliases_out() {
        for from in [48_000, 44_100] {
            let (edge, past) = worst_aliases(from, 16_000, UpFilter::Long);
            assert!(edge < -50.0, "{from}: a tone just past 8 kHz comes back at {edge:.1} dB");
            assert!(past < -80.0, "{from}: a tone further out comes back at {past:.1} dB");
            let pass = level(&through(&tone(6_000.0, from), from, 16_000, UpFilter::Long), 6_000.0, 16_000);
            assert!(pass.abs() < 0.1, "{from}: 6 kHz is at {pass:.2} dB");
        }
    }

    /// The apps' playback asks for the short filter. On a headset at 8 kHz a
    /// 16 kHz stream goes down, and gets the anti-alias filter all the same.
    #[test]
    fn playback_going_down_gets_the_anti_alias_filter() {
        let (edge, past) = worst_aliases(16_000, 8_000, UpFilter::Short);
        assert!(edge < -60.0, "a tone just past 4 kHz comes back at {edge:.1} dB");
        assert!(past < -80.0, "a tone further out comes back at {past:.1} dB");
        let pass = level(&through(&tone(3_000.0, 16_000), 16_000, 8_000, UpFilter::Short), 3_000.0, 8_000);
        assert!(pass.abs() < 0.1, "3 kHz is at {pass:.2} dB");
    }

    /// What a resampler built for these rates says about itself.
    fn describe(from: u32, to: u32, up: UpFilter) -> String {
        resampler(from, to, (from / 50) as usize, up).unwrap().to_string()
    }

    #[test]
    fn going_up_keeps_the_filter_of_its_place() {
        assert_eq!(describe(8_000, 48_000, UpFilter::Short), "up with sinc 32, cutoff 0.90");
        assert_eq!(describe(16_000, 44_100, UpFilter::Short), "up with sinc 32, cutoff 0.90");
        assert_eq!(describe(16_000, 48_000, UpFilter::Long), "up with sinc 128, cutoff 0.95");
        // a microphone that records at 8 kHz goes up to 16
        assert_eq!(describe(8_000, 16_000, UpFilter::Long), "up with sinc 128, cutoff 0.95");
    }

    /// Every `.rs` file under the crates that resample audio.
    fn sources() -> Vec<(std::path::PathBuf, String)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<(std::path::PathBuf, String)>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push((p.clone(), std::fs::read_to_string(&p).unwrap()));
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut out = Vec::new();
        for c in ["sdr-remote-core", "sdr-remote-server", "sdr-remote-logic", "sdr-remote-client", "sdr-remote-android", "vrx-rs"] {
            walk(&root.join(c).join("src"), &mut out);
        }
        // the one place that builds them - by its whole path: a second file
        // named resample.rs elsewhere is read like any other (round 261)
        out.retain(|(p, _)| !p.ends_with("sdr-remote-core/src/resample.rs"));
        out
    }

    /// The same rate: nothing filtered, nothing held back, the first frame too
    /// (build 261). A microphone that records at 16 kHz, narrowband played at 8.
    #[test]
    fn the_same_rate_goes_through_untouched() {
        for (hz, up) in [(16_000, UpFilter::Long), (8_000, UpFilter::Short)] {
            let mut r = resampler(hz, hz, (hz / 50) as usize, up).unwrap();
            assert_eq!(r.filter(), None);
            assert_eq!(r.to_string(), "1:1, no resampling");
            let x = tone(1_000.0, hz);
            for frame in x.chunks_exact((hz / 50) as usize) {
                assert_eq!(r.process(frame.to_vec()).unwrap(), frame, "{hz}: a frame came out changed");
            }
        }
    }

    /// Every number of every filter, the oversampling too (round 260: it was
    /// in no log line and no test).
    #[test]
    fn every_filter_is_pinned_by_value() {
        let f = |down, sinc_len, cutoff, oversampling| Some(Filter { down, sinc_len, cutoff, oversampling });
        assert_eq!(filter(48_000, 8_000, UpFilter::Short), f(true, 256, 0.90, 128));
        assert_eq!(filter(48_000, 16_000, UpFilter::Short), f(true, 128, 0.90, 128));
        assert_eq!(filter(16_000, 48_000, UpFilter::Short), f(false, 32, 0.90, 32));
        assert_eq!(filter(16_000, 48_000, UpFilter::Long), f(false, 128, 0.95, 128));
        assert_eq!(filter(16_000, 16_000, UpFilter::Long), None);
        // what a resampler says is what it was built with
        let r = resampler(16_000, 8_000, 320, UpFilter::Short).unwrap();
        assert_eq!(r.filter(), filter(16_000, 8_000, UpFilter::Short));
    }

    /// One source: no resampler is built outside this file, in any form - a
    /// ratio written another way, another rubato type. One built by hand gets
    /// its own filter numbers again, which is how eight copies came about, and
    /// a reviewer's probe showed that a guard on the spelling of the ratio
    /// missed one (round 259).
    #[test]
    fn no_resampler_is_built_outside_this_file() {
        let kinds = [
            "SincFixedIn::", "SincFixedOut::", "FftFixedIn::", "FftFixedOut::", "FftFixedInOut::", "FastFixedIn::",
            "FastFixedOut::",
        ];
        let mut by_hand = Vec::new();
        for (path, src) in sources() {
            for k in kinds {
                for _ in src.matches(k) {
                    by_hand.push(format!("{}: {}", path.display(), k));
                }
            }
        }
        assert!(by_hand.is_empty(), "resamplers built outside resample.rs: {by_hand:#?}");
    }

    #[test]
    fn the_filter_says_what_it_is() {
        assert_eq!(describe(48_000, 8_000, UpFilter::Long), "down with sinc 256, cutoff 0.90");
        assert_eq!(describe(44_100, 16_000, UpFilter::Short), "down with sinc 128, cutoff 0.90");
        // exactly three: the short filter, so the microphone gets no extra delay
        assert_eq!(describe(48_000, 16_000, UpFilter::Long), "down with sinc 128, cutoff 0.90");
        assert_eq!(describe(44_100, 8_000, UpFilter::Long), "down with sinc 256, cutoff 0.90");
    }
}
