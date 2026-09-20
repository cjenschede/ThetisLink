// SPDX-License-Identifier: GPL-2.0-or-later

//! The CW sidetone: the tone the operator hears the moment the key goes down,
//! made locally, so it does not wait for anything to come back over the link.
//!
//! Step 3 of `docs/internal/DESIGN-remote-cw.md`. The shape follows the
//! alarms in `audio.rs`: a small piece of shared state the key side writes
//! (`Sidetone`, atomics only), and a mixer the playback callback calls on
//! every buffer, in place and additive, so the receive path gains no buffer
//! and no delay. The tone must also sound while the speaker is muted for
//! transmit - that is exactly when it is being keyed.
//!
//! Click-free the way the roger beep is: a five-millisecond ramp at each end
//! (`FADE_MS`), which is inaudible as a fade and takes the splatter out of the
//! edges. The ramp runs per sample inside the buffer, so a key that changes
//! between two callbacks still starts and stops on a slope.
//!
//! Measurement 4 of the design lives here too: `set_key(true)` stamps the
//! moment of the edge, the mixer stamps the moment it first renders the tone,
//! and the difference - plus the buffer it was rendered into - is what the
//! reader's log reports as the onset latency. It is the software half of the
//! latency; the device adds its own after that.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

/// Ramp at each end of the tone.
pub const FADE_MS: u32 = 5;
pub const HZ_MIN: f32 = 300.0;
pub const HZ_MAX: f32 = 1200.0;
pub const HZ_DEFAULT: f32 = 600.0;
pub const VOLUME_DEFAULT: f32 = 0.3;

/// What the operator sets: on or off, pitch, level.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SidetoneSettings {
    pub on: bool,
    pub hz: f32,
    pub volume: f32,
}

impl Default for SidetoneSettings {
    fn default() -> Self {
        Self { on: true, hz: HZ_DEFAULT, volume: VOLUME_DEFAULT }
    }
}

impl SidetoneSettings {
    pub fn clamped(self) -> Self {
        Self {
            on: self.on,
            hz: if self.hz.is_finite() { self.hz.clamp(HZ_MIN, HZ_MAX) } else { HZ_DEFAULT },
            volume: if self.volume.is_finite() { self.volume.clamp(0.0, 1.0) } else { VOLUME_DEFAULT },
        }
    }
}

/// One clock for the key side and the audio side, so the onset can be measured.
fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Microseconds since the process first asked; never 0 once asked (0 means
/// "no edge pending" in the stamp below).
pub fn now_us() -> u64 {
    (epoch().elapsed().as_micros() as u64).max(1)
}

/// The shared state between whoever keys and the playback callback. Atomics
/// only: the callback never locks.
pub struct Sidetone {
    key_down: AtomicBool,
    on: AtomicBool,
    hz_bits: AtomicU32,
    volume_bits: AtomicU32,
    /// When the key went down and the tone has not started yet; 0 = none.
    edge_at_us: AtomicU64,
    onset_count: AtomicU32,
    last_onset_us: AtomicU32,
    last_buffer_frames: AtomicU32,
    /// The same buffer in microseconds, at the rate the mixer was really
    /// called with - a device that does not run at 48 kHz has buffers of
    /// another length than a frame count suggests (review finding).
    last_buffer_us: AtomicU32,
    /// What the audio device says lies between a callback and the speaker,
    /// in microseconds; 0 = never told. The part of the latency the callback
    /// cannot measure itself, reported by the backend that can.
    output_latency_us: AtomicU32,
}

impl Sidetone {
    pub fn new(settings: SidetoneSettings) -> Self {
        let s = settings.clamped();
        Self {
            key_down: AtomicBool::new(false),
            on: AtomicBool::new(s.on),
            hz_bits: AtomicU32::new(s.hz.to_bits()),
            volume_bits: AtomicU32::new(s.volume.to_bits()),
            edge_at_us: AtomicU64::new(0),
            onset_count: AtomicU32::new(0),
            last_onset_us: AtomicU32::new(0),
            last_buffer_frames: AtomicU32::new(0),
            last_buffer_us: AtomicU32::new(0),
            output_latency_us: AtomicU32::new(0),
        }
    }

    /// The key went down or up. A down that follows a down changes nothing
    /// and stamps nothing.
    pub fn set_key(&self, down: bool) {
        if down {
            if !self.key_down.swap(true, Ordering::SeqCst) {
                self.edge_at_us.store(now_us(), Ordering::SeqCst);
            }
        } else {
            self.key_down.store(false, Ordering::SeqCst);
            self.edge_at_us.store(0, Ordering::SeqCst);
        }
    }

    pub fn key_down(&self) -> bool {
        self.key_down.load(Ordering::SeqCst)
    }

    pub fn apply(&self, settings: SidetoneSettings) {
        let s = settings.clamped();
        self.on.store(s.on, Ordering::Relaxed);
        self.hz_bits.store(s.hz.to_bits(), Ordering::Relaxed);
        self.volume_bits.store(s.volume.to_bits(), Ordering::Relaxed);
    }

    pub fn settings(&self) -> SidetoneSettings {
        SidetoneSettings {
            on: self.on.load(Ordering::Relaxed),
            hz: f32::from_bits(self.hz_bits.load(Ordering::Relaxed)),
            volume: f32::from_bits(self.volume_bits.load(Ordering::Relaxed)),
        }
    }

    /// The backend's own figure for callback-to-speaker, from the playback
    /// callback (cpal: `timestamp().playback - timestamp().callback`). Called
    /// on every callback, with 0 when the backend has nothing to say: this
    /// state outlives a device switch, and a figure the previous device left
    /// behind must not speak for the next one (review finding).
    pub fn note_output_latency(&self, us: u32) {
        self.output_latency_us.store(us, Ordering::Relaxed);
    }

    /// How long the buffer of the last onset was, in microseconds, at the
    /// sample rate the mixer ran at.
    pub fn last_buffer_us(&self) -> u32 {
        self.last_buffer_us.load(Ordering::Relaxed)
    }

    /// Callback-to-speaker as the device reports it, in microseconds; 0 when
    /// the backend never said.
    pub fn output_latency_us(&self) -> u32 {
        self.output_latency_us.load(Ordering::Relaxed)
    }

    /// How many times the tone has started since the process began.
    pub fn onset_count(&self) -> u32 {
        self.onset_count.load(Ordering::Relaxed)
    }

    /// Edge-to-first-rendered-sample of the last onset, in microseconds, and
    /// the size of the buffer it was rendered into (frames). The buffer is the
    /// part of the latency the callback cannot see: the tone plays when that
    /// buffer does.
    pub fn last_onset(&self) -> (u32, u32) {
        (self.last_onset_us.load(Ordering::Relaxed), self.last_buffer_frames.load(Ordering::Relaxed))
    }
}

/// Oscillator phase and envelope, owned by the playback callback.
#[derive(Debug, Default)]
pub struct SidetoneState {
    phase: f32,
    env: f32,
}

/// Mix the sidetone into `data` in place. Additive, so whatever was in the
/// buffer (received audio, an alarm) stays.
pub fn mix_sidetone(data: &mut [f32], channels: usize, sample_rate: u32, st: &Sidetone, state: &mut SidetoneState) {
    let s = st.settings();
    let want = s.on && st.key_down();
    if !want && state.env <= 0.0 {
        state.phase = 0.0;
        return;
    }
    let channels = channels.max(1);
    let rate = sample_rate.max(1);
    let frames = data.len() / channels;
    if want && state.env <= 0.0 {
        // The tone starts in this buffer: that is the onset.
        let at = st.edge_at_us.swap(0, Ordering::SeqCst);
        if at != 0 {
            st.last_onset_us.store(now_us().saturating_sub(at) as u32, Ordering::Relaxed);
            st.last_buffer_frames.store(frames as u32, Ordering::Relaxed);
            st.last_buffer_us.store((frames as u64 * 1_000_000 / rate as u64) as u32, Ordering::Relaxed);
            st.onset_count.fetch_add(1, Ordering::Relaxed);
        }
    }
    let fade_samples = (rate * FADE_MS / 1000).max(1) as f32;
    let env_step = 1.0 / fade_samples;
    let phase_step = std::f32::consts::TAU * s.hz / rate as f32;
    for frame in data.chunks_mut(channels) {
        state.env = if want { (state.env + env_step).min(1.0) } else { (state.env - env_step).max(0.0) };
        let v = state.phase.sin() * s.volume * state.env;
        state.phase += phase_step;
        if state.phase >= std::f32::consts::TAU {
            state.phase -= std::f32::consts::TAU;
        }
        for out in frame.iter_mut() {
            *out = (*out + v).clamp(-1.0, 1.0);
        }
    }
    if state.env <= 0.0 {
        state.phase = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    const FADE: usize = 240; // 5 ms at 48 kHz

    fn tone() -> Sidetone {
        Sidetone::new(SidetoneSettings::default())
    }

    fn peak(buf: &[f32]) -> f32 {
        buf.iter().fold(0.0f32, |m, v| m.max(v.abs()))
    }

    /// The biggest jump between two neighbouring samples: what a click is.
    fn max_step(buf: &[f32]) -> f32 {
        buf.windows(2).fold(0.0f32, |m, w| m.max((w[1] - w[0]).abs()))
    }

    #[test]
    fn silence_while_the_key_is_up() {
        let st = tone();
        let mut state = SidetoneState::default();
        let mut buf = vec![0.0f32; 960];
        mix_sidetone(&mut buf, 2, RATE, &st, &mut state);
        assert_eq!(peak(&buf), 0.0);
        assert_eq!(st.onset_count(), 0);
    }

    #[test]
    fn the_tone_ramps_in_over_five_milliseconds_and_holds_the_volume() {
        let st = tone();
        let mut state = SidetoneState::default();
        st.set_key(true);
        let mut buf = vec![0.0f32; 960]; // 960 mono frames = 20 ms
        mix_sidetone(&mut buf, 1, RATE, &st, &mut state);
        assert!(peak(&buf[..FADE / 2]) < VOLUME_DEFAULT * 0.6, "still ramping in the first 2.5 ms");
        assert!((peak(&buf[FADE..]) - VOLUME_DEFAULT).abs() < 0.02, "full volume after the ramp: {}", peak(&buf[FADE..]));
        assert!(max_step(&buf) < 0.05, "no click on the way in: {}", max_step(&buf));
    }

    #[test]
    fn the_tone_fades_out_click_free_when_the_key_goes_up() {
        let st = tone();
        let mut state = SidetoneState::default();
        st.set_key(true);
        let mut buf = vec![0.0f32; 960];
        mix_sidetone(&mut buf, 1, RATE, &st, &mut state);
        st.set_key(false);
        let mut tail = vec![0.0f32; 960];
        mix_sidetone(&mut tail, 1, RATE, &st, &mut tail_state(&mut state));
        assert!(peak(&tail[..FADE]) > 0.0, "the fade is audible");
        assert_eq!(peak(&tail[FADE..]), 0.0, "and over after 5 ms");
        assert!(max_step(&tail) < 0.05, "no click on the way out: {}", max_step(&tail));
        // And once silent, nothing more is added.
        let mut after = vec![0.0f32; 960];
        mix_sidetone(&mut after, 1, RATE, &st, &mut state);
        assert_eq!(peak(&after), 0.0);
    }

    fn tail_state(s: &mut SidetoneState) -> &mut SidetoneState {
        s
    }

    #[test]
    fn switched_off_it_stays_silent_with_the_key_down() {
        let st = Sidetone::new(SidetoneSettings { on: false, ..SidetoneSettings::default() });
        let mut state = SidetoneState::default();
        st.set_key(true);
        let mut buf = vec![0.0f32; 480];
        mix_sidetone(&mut buf, 1, RATE, &st, &mut state);
        assert_eq!(peak(&buf), 0.0);
        assert_eq!(st.onset_count(), 0, "nothing started, so nothing measured");
    }

    #[test]
    fn it_adds_to_what_is_in_the_buffer() {
        let st = tone();
        let mut state = SidetoneState::default();
        let mut buf = vec![0.5f32; 480];
        mix_sidetone(&mut buf, 2, RATE, &st, &mut state);
        assert!(buf.iter().all(|v| *v == 0.5), "key up: received audio untouched");
        st.set_key(true);
        let mut buf = vec![0.5f32; 960];
        mix_sidetone(&mut buf, 2, RATE, &st, &mut state);
        assert!(buf[FADE * 2..].iter().any(|v| *v > 0.5) && buf[FADE * 2..].iter().any(|v| *v < 0.5), "the tone rides on top");
        assert!(buf.iter().all(|v| v.abs() <= 1.0), "clamped");
    }

    #[test]
    fn the_onset_is_measured_once_per_key_down() {
        let st = tone();
        let mut state = SidetoneState::default();
        let around = Instant::now();
        st.set_key(true);
        st.set_key(true); // a second down changes nothing
        // A millisecond, so the first onset is a number and not 0: a mutant
        // that keeps "the first onset ever" guards its store with a
        // compare-against-zero, and a first onset of 0 us let it through.
        std::thread::sleep(std::time::Duration::from_millis(1));
        let mut buf = vec![0.0f32; 480];
        mix_sidetone(&mut buf, 1, RATE, &st, &mut state);
        let first = st.last_onset().0;
        assert!(first >= 1_000, "the first onset was not measured: {first} us");
        // The figure is a difference, and no bigger than the time this took:
        // not the wall clock, not a constant, not ten times the truth.
        let took = around.elapsed().as_micros() as u32;
        assert!(st.last_onset().0 <= took, "onset {} us, but the whole thing took {} us", st.last_onset().0, took);
        assert_eq!(st.onset_count(), 1);
        assert_eq!(st.last_onset().1, 480, "the buffer it was rendered into");
        mix_sidetone(&mut buf, 1, RATE, &st, &mut state);
        assert_eq!(st.onset_count(), 1, "the same key-down is not an onset twice");
        st.set_key(false);
        let mut buf = vec![0.0f32; 480];
        mix_sidetone(&mut buf, 1, RATE, &st, &mut state);
        // The figure follows the edge it belongs to: the second onset, made
        // 3 ms long on purpose, is not the first one kept, and not rounded
        // away (a mutant that stored only the first onset ever stayed green
        // while both onset tests measured a fresh Sidetone).
        // Two clocks: `before` bounds the onset from above, `after` from below.
        // One `around` taken before set_key did both, and a thread preempted
        // between the two lines made the wait below end early by that much -
        // the onset came out a few ms short under a loaded full suite
        // (failed once on 2026-09-19, green on every rerun).
        let before = Instant::now();
        st.set_key(true);
        let after = Instant::now();
        // Longer than the first by construction, however coarse the timer:
        // wait until 3 ms more than the first onset has really passed.
        let longer = std::time::Duration::from_micros(u64::from(first) + 3_000);
        while after.elapsed() < longer {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        mix_sidetone(&mut buf, 1, RATE, &st, &mut state);
        let took = before.elapsed().as_micros() as u32;
        assert_eq!(st.onset_count(), 2);
        assert!(st.last_onset().0 >= first + 3_000, "the second onset ({} us) did not follow its own edge; the first was {first} us", st.last_onset().0);
        assert!(st.last_onset().0 <= took, "onset {} us, but the whole thing took {} us", st.last_onset().0, took);
    }

    /// The bound a click-free tone lives under at any setting: the biggest
    /// step the sine itself makes, plus one step of the ramp. The fixed 0.05
    /// of the tests above only means something at the default pitch and
    /// level - at 1200 Hz and full level the tone's own step is 0.157.
    fn click_bound(hz: f32, volume: f32) -> f32 {
        let tone = 2.0 * (std::f32::consts::PI * hz / RATE as f32).sin();
        volume * (tone + 1.0 / FADE as f32) * 1.02
    }

    #[test]
    fn no_click_at_the_extremes_of_pitch_and_level() {
        for (hz, volume) in [(HZ_MIN, 1.0), (HZ_MAX, 1.0), (HZ_MAX, 0.05), (HZ_DEFAULT, VOLUME_DEFAULT)] {
            let st = Sidetone::new(SidetoneSettings { on: true, hz, volume });
            let mut state = SidetoneState::default();
            st.set_key(true);
            // 1003 samples is a whole number of cycles at none of these pitches
            // (1000 was exactly 25 cycles at 1200 Hz), so the key-up lands
            // mid-swing. What this bound sees is "no ramp at all"; it does not
            // see the ramp's length - a sine steps most at its zero crossing
            // and a ramp at its peak, so the two never add up. The length is
            // pinned by the ramp test above.
            let mut all = vec![0.0f32; 1003];
            mix_sidetone(&mut all, 1, RATE, &st, &mut state);
            st.set_key(false);
            let mut tail = vec![0.0f32; 960];
            mix_sidetone(&mut tail, 1, RATE, &st, &mut state);
            all.extend_from_slice(&tail);
            assert!(peak(&all) > volume * 0.9, "{hz} Hz at {volume}: the tone reached its level");
            let worst = max_step(&all);
            assert!(worst <= click_bound(hz, volume), "{hz} Hz at {volume}: step {worst} over {}", click_bound(hz, volume));
        }
    }

    /// The envelope and the phase live per sample, so the callback's buffer
    /// size - which cpal does not promise to keep constant - changes nothing.
    #[test]
    fn the_tone_is_the_same_whatever_the_buffer_sizes() {
        let render = |sizes_down: &[usize], sizes_up: &[usize]| {
            let st = tone();
            let mut state = SidetoneState::default();
            let mut out = Vec::new();
            st.set_key(true);
            for &n in sizes_down {
                let mut b = vec![0.0f32; n];
                mix_sidetone(&mut b, 1, RATE, &st, &mut state);
                out.extend(b);
            }
            st.set_key(false);
            for &n in sizes_up {
                let mut b = vec![0.0f32; n];
                mix_sidetone(&mut b, 1, RATE, &st, &mut state);
                out.extend(b);
            }
            out
        };
        let whole = render(&[2400], &[960]);
        let pieces = render(&[7, 480, 1, 333, 960, 619], &[3, 441, 516]);
        assert_eq!(whole.len(), pieces.len());
        assert!(whole == pieces, "bit for bit the same");
    }

    /// Two channels and an odd number of samples: the last frame is short.
    /// No panic, and both channels of every whole frame carry the same tone.
    #[test]
    fn an_odd_sample_count_on_two_channels_is_harmless() {
        let st = tone();
        let mut state = SidetoneState::default();
        st.set_key(true);
        let mut buf = vec![0.0f32; 961];
        mix_sidetone(&mut buf, 2, RATE, &st, &mut state);
        assert!(buf.chunks_exact(2).all(|f| f[0] == f[1]));
        assert!(peak(&buf) > 0.0);
        assert_eq!(st.last_onset().1, 480, "whole frames");
    }

    /// Four tests that four surviving mutants asked for (the mixer without
    /// its clamp, without its phase wrap, with the onset stamp read instead
    /// of taken, and with a second key-down stamping again).
    #[test]
    fn a_full_level_tone_on_loud_audio_is_clamped() {
        let st = Sidetone::new(SidetoneSettings { on: true, hz: HZ_DEFAULT, volume: 1.0 });
        let mut state = SidetoneState::default();
        st.set_key(true);
        let mut buf = vec![0.9f32; 960];
        mix_sidetone(&mut buf, 1, RATE, &st, &mut state);
        assert!(buf.iter().any(|v| *v == 1.0), "it did reach the rail");
        assert!(buf.iter().all(|v| v.abs() <= 1.0), "and never went past it");
    }

    #[test]
    fn the_phase_stays_within_one_turn_however_long_the_key_is_down() {
        let st = tone();
        let mut state = SidetoneState::default();
        st.set_key(true);
        let mut buf = vec![0.0f32; 4800];
        for _ in 0..100 {
            mix_sidetone(&mut buf, 1, RATE, &st, &mut state); // ten seconds
        }
        assert!(state.phase >= 0.0 && state.phase < std::f32::consts::TAU + 0.2, "phase ran away: {}", state.phase);
    }

    /// Switching the sidetone off and on with the key held is a second start
    /// of the tone, not a second key-down: nothing to measure.
    #[test]
    fn the_tone_coming_back_on_a_held_key_is_not_an_onset() {
        let st = tone();
        let mut state = SidetoneState::default();
        st.set_key(true);
        let mut buf = vec![0.0f32; 960];
        mix_sidetone(&mut buf, 1, RATE, &st, &mut state);
        assert_eq!(st.onset_count(), 1);
        st.apply(SidetoneSettings { on: false, ..SidetoneSettings::default() });
        mix_sidetone(&mut buf, 1, RATE, &st, &mut state); // fades out
        st.apply(SidetoneSettings::default());
        mix_sidetone(&mut buf, 1, RATE, &st, &mut state);
        assert_eq!(st.onset_count(), 1, "the stamp was taken, not just read");
    }

    /// The onset is measured from the first key-down, not from a repeat of it.
    #[test]
    fn a_repeated_key_down_does_not_restart_the_clock() {
        let st = tone();
        let mut state = SidetoneState::default();
        let around = Instant::now();
        st.set_key(true);
        std::thread::sleep(std::time::Duration::from_millis(5));
        st.set_key(true);
        let mut buf = vec![0.0f32; 480];
        mix_sidetone(&mut buf, 1, RATE, &st, &mut state);
        let took = around.elapsed().as_micros() as u32;
        // A slow machine only makes both numbers bigger, never this false.
        assert!(st.last_onset().0 >= 5_000, "measured from the repeat: {} us", st.last_onset().0);
        assert!(st.last_onset().0 <= took, "onset {} us, but the whole thing took {} us", st.last_onset().0, took);
    }

    #[test]
    fn the_device_latency_is_kept_as_told_and_forgotten_when_told_nothing() {
        let st = tone();
        assert_eq!(st.output_latency_us(), 0, "never told");
        st.note_output_latency(21_300);
        assert_eq!(st.output_latency_us(), 21_300);
        st.note_output_latency(0); // the next device says nothing
        assert_eq!(st.output_latency_us(), 0, "the previous device does not speak for this one");
    }

    /// The buffer's length in time follows the rate the mixer ran at.
    #[test]
    fn the_buffer_duration_follows_the_real_sample_rate() {
        for (rate, frames, us) in [(48_000u32, 480usize, 10_000u32), (44_100, 441, 10_000), (96_000, 480, 5_000)] {
            let st = tone();
            let mut state = SidetoneState::default();
            st.set_key(true);
            let mut buf = vec![0.0f32; frames * 2];
            mix_sidetone(&mut buf, 2, rate, &st, &mut state);
            assert_eq!(st.last_buffer_us(), us, "{frames} frames at {rate} Hz");
        }
    }

    /// With the key up the mixer touches nothing - not even to clamp it. The
    /// receive path is not this module's to tidy (a mutant without the early
    /// return clamped every buffer, tone or no tone).
    #[test]
    fn with_the_key_up_the_buffer_is_left_exactly_as_it_was() {
        let st = tone();
        let mut state = SidetoneState::default();
        let mut buf = vec![1.5f32, -1.5, 0.5, 1.2];
        mix_sidetone(&mut buf, 1, RATE, &st, &mut state);
        assert_eq!(buf, vec![1.5, -1.5, 0.5, 1.2]);
    }

    /// The constructor is where the saved settings come in, so it clamps too.
    #[test]
    fn the_constructor_clamps_what_it_is_given() {
        let st = Sidetone::new(SidetoneSettings { on: true, hz: 9000.0, volume: 7.0 });
        assert_eq!(st.settings(), SidetoneSettings { on: true, hz: HZ_MAX, volume: 1.0 });
    }

    #[test]
    fn settings_are_clamped_to_what_a_sidetone_can_be() {
        let s = SidetoneSettings { on: true, hz: 5000.0, volume: 3.0 }.clamped();
        assert_eq!((s.hz, s.volume), (HZ_MAX, 1.0));
        let s = SidetoneSettings { on: true, hz: f32::NAN, volume: -1.0 }.clamped();
        assert_eq!((s.hz, s.volume), (HZ_DEFAULT, 0.0));
        let st = tone();
        st.apply(SidetoneSettings { on: false, hz: 10.0, volume: 0.5 });
        assert_eq!(st.settings(), SidetoneSettings { on: false, hz: HZ_MIN, volume: 0.5 });
    }
}
