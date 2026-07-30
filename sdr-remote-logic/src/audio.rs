// SPDX-License-Identifier: GPL-2.0-or-later

use std::sync::atomic::{AtomicU32, Ordering};

/// High-SWR alarm beep: pitch, level and duty cycle (150 ms tone per 500 ms).
const SWR_ALARM_HZ: f32 = 880.0;
const SWR_ALARM_LEVEL: f32 = 0.25;
const SWR_ALARM_PERIOD_MS: u32 = 500;
const SWR_ALARM_ON_MS: u32 = 150;
/// How long one alarm refresh keeps the beep alive. The engine re-arms it from
/// every radio state push (200 ms), so this only expires when those stop.
pub const SWR_ALARM_HOLD_MS: u32 = 1500;

/// Samples of beep one `set_swr_alarm(true)` is worth, at the given rate.
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
    let left = remaining.load(Ordering::Relaxed);
    if left == 0 {
        *pos = 0;
        *phase = 0.0;
        return;
    }
    let frames = (data.len() / channels.max(1)) as u32;
    remaining.store(left.saturating_sub(frames), Ordering::Relaxed);

    let rate = sample_rate.max(1);
    let period = (rate / 1000 * SWR_ALARM_PERIOD_MS).max(1);
    let on = rate / 1000 * SWR_ALARM_ON_MS;
    let step = std::f32::consts::TAU * SWR_ALARM_HZ / rate as f32;
    for frame in data.chunks_mut(channels.max(1)) {
        let sample = if *pos < on {
            let v = phase.sin() * SWR_ALARM_LEVEL;
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

/// Platform-agnostic audio backend trait.
/// Desktop implements this with cpal; Android could implement with Oboe/AAudio.
pub trait AudioBackend: Send + 'static {
    fn read_capture(&mut self, buf: &mut [f32]) -> usize;
    fn write_playback(&mut self, buf: &[f32]) -> usize;
    fn capture_level(&self) -> f32;
    fn playback_level(&self) -> f32;
    fn has_error(&self) -> bool;
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
}
