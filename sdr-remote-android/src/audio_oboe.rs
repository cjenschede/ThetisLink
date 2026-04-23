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

use sdr_remote_logic::audio::AudioBackend;

/// Ring buffer capacity in samples (2 seconds at 48kHz)
const RING_CAPACITY: usize = 48_000 * 2;

// --- Capture callback ---

struct CaptureCallback {
    producer: ringbuf::HeapProd<f32>,
    level: Arc<AtomicU32>,
    error: Arc<AtomicBool>,
    gate: Arc<AtomicBool>,
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
}

impl AudioOutputCallback for PlaybackCallback {
    type FrameType = (f32, Mono);

    fn on_audio_ready(
        &mut self,
        _stream: &mut dyn oboe::AudioOutputStreamSafe,
        data: &mut [f32],
    ) -> DataCallbackResult {
        // Instant mute during TX: output zeros and drain ring buffer
        // so no stale audio remains when unmuted.
        if self.mute.load(Ordering::Relaxed) {
            self.consumer.pop_slice(data);
            data.fill(0.0);
            self.level.store(0u32, Ordering::Relaxed);
            return DataCallbackResult::Continue;
        }

        let read = self.consumer.pop_slice(data);

        // Zero-fill any remaining samples
        for sample in &mut data[read..] {
            *sample = 0.0;
        }

        // RMS level of played audio
        let rms = (data[..read]
            .iter()
            .map(|&s| s * s)
            .sum::<f32>()
            / read.max(1) as f32)
            .sqrt();
        self.level.store(rms.to_bits(), Ordering::Relaxed);

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
    playback_mute: Arc<AtomicBool>,
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
        let playback_mute = Arc::new(AtomicBool::new(false));

        // Capture stream (microphone)
        let capture_cb = CaptureCallback {
            producer: capture_producer,
            level: capture_level.clone(),
            error: audio_error.clone(),
            gate: capture_gate.clone(),
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
            playback_mute,
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

    fn set_playback_mute(&mut self, mute: bool) {
        self.playback_mute.store(mute, Ordering::Relaxed);
    }
}
