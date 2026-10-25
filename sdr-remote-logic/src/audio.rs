// SPDX-License-Identifier: GPL-2.0-or-later

use std::sync::atomic::{AtomicU32, Ordering};

/// A warning beep: pitch, level and duty cycle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AlarmTone {
    pub hz: f32,
    pub level: f32,
    pub period_ms: u32,
    pub on_ms: u32,
}

/// High SWR: the loud one, 150 ms per 500 ms.
pub const SWR_ALARM: AlarmTone = AlarmTone { hz: 880.0, level: 0.25, period_ms: 500, on_ms: 150 };

/// The drive was raised while a PA may be amplifying out of reach. Lower in pitch
/// and level than the SWR beep, and slower: it has to be audible over the radio
/// without drowning it, and it must not be mistaken for the SWR alarm - it says
/// something entirely different (owner, 2026-09-16: "niet heel erg luid, wel
/// hoorbaar").
pub const PA_DRIVE_ALARM: AlarmTone = AlarmTone { hz: 660.0, level: 0.12, period_ms: 1500, on_ms: 200 };
/// The press was let go because the session went (build 253): three short
/// beeps, once. A pitch of its own between the other two and well apart from
/// both, so it is not taken for the SWR or the drive warning (owner, 2026-10-18:
/// "door die toon weet de gebruiker dat er iets is en ga je op het scherm kijken").
pub const PTT_DROPPED_ALARM: AlarmTone = AlarmTone { hz: 520.0, level: 0.2, period_ms: 333, on_ms: 150 };
/// Three beeps and part of the silence after the third (it ends at 816 ms). The
/// mixer counts the hold down per callback, not per sample, so a hold of three
/// whole periods would let a fourth beep start in the callback that crosses 999 ms;
/// ending in the gap leaves room for callbacks of up to ~99 ms: one that starts
/// just before 900 ms with a frame still on the counter plays its whole length,
/// and has to be done before 999 ms (round 253).
pub const PTT_DROPPED_ALARM_MS: u32 = 900;

/// Samples of the dropped-press beeps, at the given rate.
pub fn ptt_dropped_alarm_samples(sample_rate: u32) -> u32 {
    sample_rate / 1000 * PTT_DROPPED_ALARM_MS
}

/// How long one alarm refresh keeps the beep alive. The engine re-arms it from
/// every radio state push (200 ms), so this only expires when those stop.
pub const SWR_ALARM_HOLD_MS: u32 = 1500;

/// Samples of beep one alarm refresh is worth, at the given rate.
pub fn swr_alarm_hold_samples(sample_rate: u32) -> u32 {
    sample_rate / 1000 * SWR_ALARM_HOLD_MS
}

/// Mix the high-SWR warning beep into a playback buffer, in place, and count the
/// played frames off the alarm watchdog. A no-op once the watchdog has run out.
///
/// Shared by the desktop (cpal) and Android (Oboe) backends. Called from inside
/// their existing playback callbacks and purely additive: no extra buffer, no
/// resampling and no dependency on the ring buffer, so the latency-critical RX
/// path is untouched. `pos` (sample counter within the beep cycle) and `phase`
/// (oscillator phase) are owned by the callback.
pub fn mix_swr_alarm(
    data: &mut [f32],
    channels: usize,
    sample_rate: u32,
    remaining: &AtomicU32,
    pos: &mut u32,
    phase: &mut f32,
) {
    mix_alarm(SWR_ALARM, data, channels, sample_rate, remaining, pos, phase)
}

/// The same mixer for any of the tones above.
pub fn mix_alarm(
    tone: AlarmTone,
    data: &mut [f32],
    channels: usize,
    sample_rate: u32,
    remaining: &AtomicU32,
    pos: &mut u32,
    phase: &mut f32,
) {
    let left = remaining.load(Ordering::Relaxed);
    if left == 0 {
        *pos = 0;
        *phase = 0.0;
        return;
    }
    let frames = (data.len() / channels.max(1)) as u32;
    remaining.store(left.saturating_sub(frames), Ordering::Relaxed);

    let rate = sample_rate.max(1);
    let period = (rate / 1000 * tone.period_ms).max(1);
    let on = rate / 1000 * tone.on_ms;
    let step = std::f32::consts::TAU * tone.hz / rate as f32;
    for frame in data.chunks_mut(channels.max(1)) {
        let sample = if *pos < on {
            let v = phase.sin() * tone.level;
            *phase += step;
            if *phase >= std::f32::consts::TAU {
                *phase -= std::f32::consts::TAU;
            }
            v
        } else {
            *phase = 0.0;
            0.0
        };
        *pos = (*pos + 1) % period;
        for out in frame.iter_mut() {
            *out = (*out + sample).clamp(-1.0, 1.0);
        }
    }
}

#[cfg(test)]
mod alarm_tone_tests {
    use super::{PA_DRIVE_ALARM, PTT_DROPPED_ALARM, PTT_DROPPED_ALARM_MS, SWR_ALARM, SWR_ALARM_HOLD_MS};

    /// Both tones written out by value. A tone expressed only through its own
    /// constant can drift to anything and no test notices - the lesson from the
    /// hold of build 47. And the two must stay apart: an operator has to hear
    /// which of the two warnings is speaking.
    #[test]
    fn the_two_warnings_do_not_sound_alike() {
        assert_eq!(SWR_ALARM.hz, 880.0);
        assert_eq!(SWR_ALARM.level, 0.25);
        assert_eq!(SWR_ALARM.period_ms, 500);
        assert_eq!(SWR_ALARM.on_ms, 150);

        assert_eq!(PA_DRIVE_ALARM.hz, 660.0);
        assert_eq!(PA_DRIVE_ALARM.level, 0.12);
        assert_eq!(PA_DRIVE_ALARM.period_ms, 1500);
        assert_eq!(PA_DRIVE_ALARM.on_ms, 200);

        assert!(PA_DRIVE_ALARM.hz < SWR_ALARM.hz, "lower in pitch");
        assert!(PA_DRIVE_ALARM.level < SWR_ALARM.level, "and quieter");
        assert!(PA_DRIVE_ALARM.period_ms > SWR_ALARM.period_ms, "and slower");

        // The hold both tones live on between two pushes of the state.
        assert_eq!(SWR_ALARM_HOLD_MS, 1500);
    }

    /// The dropped-press beeps (build 253), by value: a pitch of their own,
    /// at least 100 Hz from the other two, and played once - three whole beeps,
    /// none cut off by the end of the hold.
    #[test]
    fn the_dropped_press_sounds_three_beeps_of_its_own() {
        assert_eq!(PTT_DROPPED_ALARM.hz, 520.0);
        assert_eq!(PTT_DROPPED_ALARM.level, 0.2);
        assert_eq!(PTT_DROPPED_ALARM.period_ms, 333);
        assert_eq!(PTT_DROPPED_ALARM.on_ms, 150);
        assert_eq!(PTT_DROPPED_ALARM_MS, 900);
        for other in [SWR_ALARM.hz, PA_DRIVE_ALARM.hz] {
            assert!((PTT_DROPPED_ALARM.hz - other).abs() >= 100.0, "its own pitch, not {other} Hz");
        }
        let third_ends = 2 * PTT_DROPPED_ALARM.period_ms + PTT_DROPPED_ALARM.on_ms;
        assert!(PTT_DROPPED_ALARM_MS >= third_ends, "the third beep plays whole");
        assert!(PTT_DROPPED_ALARM_MS < 3 * PTT_DROPPED_ALARM.period_ms, "and ends before a fourth would start");
    }

    /// Mixed in callbacks of 10 ms, as a device asks for them, the beeps sound
    /// three times and then the counter is spent: once, not a latch.
    #[test]
    fn the_dropped_press_beeps_play_out_and_stop() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let rate = 48_000;
        let left = AtomicU32::new(super::ptt_dropped_alarm_samples(rate));
        let (mut pos, mut phase) = (0u32, 0.0f32);
        let mut buf = vec![0.0f32; (rate / 1000 * 1200) as usize];
        for chunk in buf.chunks_mut((rate / 100) as usize) {
            super::mix_alarm(PTT_DROPPED_ALARM, chunk, 1, rate, &left, &mut pos, &mut phase);
        }
        assert_eq!(left.load(Ordering::Relaxed), 0, "spent");
        // Count the starts of a beep: a sounding sample after a silent stretch.
        let ms = |i: usize| i as u32 * 1000 / rate;
        let mut starts = Vec::new();
        let mut silent_since: Option<usize> = Some(0);
        for (i, s) in buf.iter().enumerate() {
            if s.abs() > 1e-6 {
                if silent_since.is_some() {
                    starts.push(ms(i));
                }
                silent_since = None;
            } else if silent_since.is_none() {
                silent_since = Some(i);
            }
        }
        assert_eq!(starts, vec![0, 333, 666], "three beeps, a period apart");
        assert!(buf[(rate / 1000 * 820) as usize..].iter().all(|s| *s == 0.0), "and silence after them");
    }
}

/// Platform-agnostic audio backend trait.
/// Desktop implements this with cpal; Android could implement with Oboe/AAudio.
pub trait AudioBackend: Send + 'static {
    fn read_capture(&mut self, buf: &mut [f32]) -> usize;
    fn write_playback(&mut self, buf: &[f32]) -> usize;
    fn capture_level(&self) -> f32;
    fn playback_level(&self) -> f32;
    fn has_error(&self) -> bool;
    /// The chosen microphone that was not found, by the name it was chosen
    /// under; `None` when there is no choice or it was found. Nothing else is
    /// opened in its place (build 188). Android has no choice: always `None`.
    fn missing_input(&self) -> Option<String> { None }
    /// Whether that missing microphone is there again.
    fn missing_input_is_back(&self) -> bool { false }
    fn capture_sample_rate(&self) -> u32;
    fn playback_sample_rate(&self) -> u32;
    /// Number of samples currently in the playback ring buffer.
    /// Used by the engine to pull extra frames when the buffer is low.
    fn playback_buffer_level(&self) -> usize { 0 }
    /// True if backend supports stereo output (desktop). False = mono only (Android).
    fn supports_stereo(&self) -> bool { false }
    /// Write stereo playback (left + right interleaved into device channels).
    /// Default: writes left channel only (mono fallback).
    fn write_playback_stereo(&mut self, left: &[f32], _right: &[f32]) -> usize {
        // Default (Android): play only L channel (RX1). R channel (RX2) is ignored.
        self.write_playback(left)
    }
    /// Gate mic capture: when false, the capture callback discards audio
    /// instead of writing to the ring buffer.
    fn set_capture_gate(&mut self, _open: bool) {}
    /// Configure backend-local samples to discard each time mic capture opens.
    /// Android uses this for sample-precise PTT burst suppression; desktop ignores it.
    fn set_capture_gate_delay_ms(&mut self, _delay_ms: u32) {}
    /// Mute speaker output: when true, playback callback outputs zeros
    /// regardless of ring buffer contents (instant silence).
    fn set_playback_mute(&mut self, _mute: bool) {}
    /// High-SWR alarm: when true, the backend mixes a repeating warning beep
    /// over the speaker output until it is cleared. Mixing happens inside the
    /// existing playback callback, so the RX path gains no buffering or delay.
    fn set_swr_alarm(&mut self, _on: bool) {}
    /// The drive was raised while a PA may be amplifying out of reach: a softer,
    /// slower beep than the SWR one, on until the server clears it.
    fn set_pa_drive_alarm(&mut self, _on: bool) {}
    /// The press was let go because the session went (build 253): three short
    /// beeps, once, so an operator who is not looking at the screen knows the
    /// transmitter is off and can press again. Not a latch: it plays out and stops.
    fn sound_ptt_dropped(&mut self) {}
}
