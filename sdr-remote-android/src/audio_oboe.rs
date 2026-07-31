// SPDX-License-Identifier: GPL-2.0-or-later

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use log::info;
use oboe::{
    AudioInputCallback, AudioOutputCallback, AudioStream, AudioStreamAsync, AudioStreamBase,
    AudioStreamBuilder, DataCallbackResult, Input, InputPreset, Mono, Output, PerformanceMode,
    SharingMode, Usage,
};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::HeapRb;

use sdr_remote_logic::audio::{mix_swr_alarm, swr_alarm_hold_samples, AudioBackend};

/// Ring buffer capacity in samples (2 seconds at 48kHz)
const RING_CAPACITY: usize = 48_000 * 2;

// --- Capture callback ---

struct CaptureCallback {
    producer: ringbuf::HeapProd<f32>,
    level: Arc<AtomicU32>,
    error: Arc<AtomicBool>,
    gate: Arc<AtomicBool>,
    gate_delay_samples: Arc<AtomicU32>,
    /// Samples to skip after gate opens (anti-feedback: let speaker decay first)
    gate_delay_remaining: u32,
    was_open: bool,
}

impl AudioInputCallback for CaptureCallback {
    type FrameType = (f32, Mono);

    fn on_audio_ready(
        &mut self,
        _stream: &mut dyn oboe::AudioInputStreamSafe,
        data: &[f32],
    ) -> DataCallbackResult {
        // Only write to ring buffer when gate is open (PTT active).
        let gate_open = self.gate.load(Ordering::Relaxed);
        if !gate_open {
            self.was_open = false;
            self.gate_delay_remaining = 0;
            self.level.store(0u32, Ordering::Relaxed);
            return DataCallbackResult::Continue;
        }

        if !self.was_open {
            self.was_open = true;
            self.gate_delay_remaining = self.gate_delay_samples.load(Ordering::Relaxed);
        }

        let skip = self.gate_delay_remaining.min(data.len() as u32) as usize;
        self.gate_delay_remaining = self.gate_delay_remaining.saturating_sub(skip as u32);
        let data = &data[skip..];
        if data.is_empty() {
            self.level.store(0u32, Ordering::Relaxed);
            return DataCallbackResult::Continue;
        }

        let rms =
            (data.iter().map(|&s| s * s).sum::<f32>() / data.len().max(1) as f32).sqrt();
        self.level.store(rms.to_bits(), Ordering::Relaxed);

        self.producer.push_slice(data);

        DataCallbackResult::Continue
    }

    fn on_error_before_close(
        &mut self,
        _stream: &mut dyn oboe::AudioInputStreamSafe,
        _error: oboe::Error,
    ) {
        log::error!("Oboe capture error (before close)");
        self.error.store(true, Ordering::Relaxed);
    }

    fn on_error_after_close(
        &mut self,
        _stream: &mut dyn oboe::AudioInputStreamSafe,
        _error: oboe::Error,
    ) {
        log::error!("Oboe capture error (after close)");
        self.error.store(true, Ordering::Relaxed);
    }
}

// --- Playback callback ---

struct PlaybackCallback {
    consumer: ringbuf::HeapCons<f32>,
    level: Arc<AtomicU32>,
    error: Arc<AtomicBool>,
    mute: Arc<AtomicBool>,
    /// High-SWR alarm watchdog: samples of beep left to play (see
    /// `sdr_remote_logic::audio::mix_swr_alarm`).
    swr_alarm: Arc<AtomicU32>,
    /// Device rate, cached on the first callback: it is only known once the
    /// stream is open, which is after this callback is built.
    sample_rate: u32,
    alarm_pos: u32,
    alarm_phase: f32,
}

impl AudioOutputCallback for PlaybackCallback {
    type FrameType = (f32, Mono);

    fn on_audio_ready(
        &mut self,
        stream: &mut dyn oboe::AudioOutputStreamSafe,
        data: &mut [f32],
    ) -> DataCallbackResult {
        if self.sample_rate == 0 {
            self.sample_rate = stream.get_sample_rate() as u32;
        }

        // Instant mute during TX: output zeros and drain ring buffer
        // so no stale audio remains when unmuted.
        if self.mute.load(Ordering::Relaxed) {
            self.consumer.pop_slice(data);
            data.fill(0.0);
            self.level.store(0u32, Ordering::Relaxed);
            // High SWR is a TX condition, so the alarm has to survive the TX
            // mute - it plays into the silenced buffer.
            mix_swr_alarm(data, 1, self.sample_rate, &self.swr_alarm,
                &mut self.alarm_pos, &mut self.alarm_phase);
            return DataCallbackResult::Continue;
        }

        let read = self.consumer.pop_slice(data);

        // Zero-fill any remaining samples
        for sample in &mut data[read..] {
            *sample = 0.0;
        }

        // RMS level of played audio (before the alarm is mixed in, so the meter
        // keeps showing received audio only)
        let rms = (data[..read]
            .iter()
            .map(|&s| s * s)
            .sum::<f32>()
            / read.max(1) as f32)
            .sqrt();
        self.level.store(rms.to_bits(), Ordering::Relaxed);

        mix_swr_alarm(data, 1, self.sample_rate, &self.swr_alarm,
            &mut self.alarm_pos, &mut self.alarm_phase);

        DataCallbackResult::Continue
    }

    fn on_error_before_close(
        &mut self,
        _stream: &mut dyn oboe::AudioOutputStreamSafe,
        _error: oboe::Error,
    ) {
        log::error!("Oboe playback error (before close)");
        self.error.store(true, Ordering::Relaxed);
    }

    fn on_error_after_close(
        &mut self,
        _stream: &mut dyn oboe::AudioOutputStreamSafe,
        _error: oboe::Error,
    ) {
        log::error!("Oboe playback error (after close)");
        self.error.store(true, Ordering::Relaxed);
    }
}

/// Oboe-based AudioBackend for Android.
/// Uses AAudio (API 26+) with low-latency exclusive mode.
/// Same ring buffer pattern as desktop cpal implementation.
pub struct OboeAudioBackend {
    capture_consumer: ringbuf::HeapCons<f32>,
    playback_producer: ringbuf::HeapProd<f32>,
    capture_level: Arc<AtomicU32>,
    playback_level: Arc<AtomicU32>,
    audio_error: Arc<AtomicBool>,
    capture_gate: Arc<AtomicBool>,
    capture_gate_delay_samples: Arc<AtomicU32>,
    playback_mute: Arc<AtomicBool>,
    swr_alarm: Arc<AtomicU32>,
    capture_sample_rate: u32,
    playback_sample_rate: u32,
    // Keep streams alive — dropped when OboeAudioBackend is dropped
    _capture_stream: AudioStreamAsync<Input, CaptureCallback>,
    _playback_stream: AudioStreamAsync<Output, PlaybackCallback>,
}

impl OboeAudioBackend {
    pub fn new() -> Result<Self> {
        let (capture_producer, capture_consumer) = HeapRb::<f32>::new(RING_CAPACITY).split();
        let (playback_producer, playback_consumer) = HeapRb::<f32>::new(RING_CAPACITY).split();

        let capture_level = Arc::new(AtomicU32::new(0));
        let playback_level = Arc::new(AtomicU32::new(0));
        let audio_error = Arc::new(AtomicBool::new(false));
        let capture_gate = Arc::new(AtomicBool::new(false));
        let capture_gate_delay_samples = Arc::new(AtomicU32::new(0));
        let playback_mute = Arc::new(AtomicBool::new(false));
        let swr_alarm = Arc::new(AtomicU32::new(0));

        // Capture stream (microphone)
        let capture_cb = CaptureCallback {
            producer: capture_producer,
            level: capture_level.clone(),
            error: audio_error.clone(),
            gate: capture_gate.clone(),
            gate_delay_samples: capture_gate_delay_samples.clone(),
            gate_delay_remaining: 0,
            was_open: false,
        };

        let mut capture_stream = AudioStreamBuilder::default()
            .set_input()
            .set_performance_mode(PerformanceMode::LowLatency)
            .set_sharing_mode(SharingMode::Exclusive)
            .set_format::<f32>()
            .set_channel_count::<Mono>()
            .set_input_preset(InputPreset::VoiceRecognition)
            .set_callback(capture_cb)
            .open_stream()
            .context("open Oboe capture stream")?;

        let capture_sample_rate = capture_stream.get_sample_rate() as u32;

        // Playback stream (speaker/earpiece)
        let playback_cb = PlaybackCallback {
            consumer: playback_consumer,
            level: playback_level.clone(),
            error: audio_error.clone(),
            mute: playback_mute.clone(),
            swr_alarm: swr_alarm.clone(),
            sample_rate: 0,
            alarm_pos: 0,
            alarm_phase: 0.0,
        };

        let mut playback_stream = AudioStreamBuilder::default()
            .set_output()
            .set_performance_mode(PerformanceMode::LowLatency)
            .set_sharing_mode(SharingMode::Exclusive)
            .set_format::<f32>()
            .set_channel_count::<Mono>()
            .set_usage(Usage::Media)
            .set_callback(playback_cb)
            .open_stream()
            .context("open Oboe playback stream")?;

        let playback_sample_rate = playback_stream.get_sample_rate() as u32;

        // Start both streams
        capture_stream
            .start()
            .context("start Oboe capture stream")?;
        playback_stream
            .start()
            .context("start Oboe playback stream")?;

        info!(
            "Oboe audio started: capture {}Hz, playback {}Hz",
            capture_sample_rate, playback_sample_rate
        );

        Ok(Self {
            capture_consumer,
            playback_producer,
            capture_level,
            playback_level,
            audio_error,
            capture_gate,
            capture_gate_delay_samples,
            playback_mute,
            swr_alarm,
            capture_sample_rate,
            playback_sample_rate,
            _capture_stream: capture_stream,
            _playback_stream: playback_stream,
        })
    }
}

impl AudioBackend for OboeAudioBackend {
    fn read_capture(&mut self, buf: &mut [f32]) -> usize {
        self.capture_consumer.pop_slice(buf)
    }

    fn write_playback(&mut self, buf: &[f32]) -> usize {
        self.playback_producer.push_slice(buf)
    }

    fn capture_level(&self) -> f32 {
        f32::from_bits(self.capture_level.load(Ordering::Relaxed))
    }

    fn playback_level(&self) -> f32 {
        f32::from_bits(self.playback_level.load(Ordering::Relaxed))
    }

    fn has_error(&self) -> bool {
        self.audio_error.load(Ordering::Relaxed)
    }

    fn capture_sample_rate(&self) -> u32 {
        self.capture_sample_rate
    }

    fn playback_sample_rate(&self) -> u32 {
        self.playback_sample_rate
    }

    fn playback_buffer_level(&self) -> usize {
        self.playback_producer.occupied_len()
    }

    fn set_capture_gate(&mut self, open: bool) {
        self.capture_gate.store(open, Ordering::Relaxed);
    }

    fn set_capture_gate_delay_ms(&mut self, delay_ms: u32) {
        let samples = (self.capture_sample_rate as u64)
            .saturating_mul(delay_ms as u64)
            .saturating_add(999)
            / 1000;
        self.capture_gate_delay_samples
            .store(samples.min(u32::MAX as u64) as u32, Ordering::Relaxed);
    }

    fn set_playback_mute(&mut self, mute: bool) {
        self.playback_mute.store(mute, Ordering::Relaxed);
    }

    fn set_swr_alarm(&mut self, on: bool) {
        // Re-arm rather than latch: each call refreshes the hold window, so the
        // beep stops on its own if the state pushes that drive it dry up.
        let samples = if on { swr_alarm_hold_samples(self.playback_sample_rate) } else { 0 };
        self.swr_alarm.store(samples, Ordering::Relaxed);
    }
}
