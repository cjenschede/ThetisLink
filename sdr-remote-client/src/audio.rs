#![allow(dead_code)]
use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::Stream;
use log::info;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::HeapRb;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use sdr_remote_core::DEVICE_SAMPLE_RATE;

/// Ring buffer capacity in samples (2s at device rate)
const RING_CAPACITY: usize = DEVICE_SAMPLE_RATE as usize * 2;

/// List available input (capture) device names
pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    let mut names = Vec::new();
    if let Ok(devices) = host.input_devices() {
        for d in devices {
            if let Ok(name) = d.name() {
                names.push(name);
            }
        }
    }
    names
}

/// List available output (playback) device names
pub fn list_output_devices() -> Vec<String> {
    let host = cpal::default_host();
    let mut names = Vec::new();
    if let Ok(devices) = host.output_devices() {
        for d in devices {
            if let Ok(name) = d.name() {
                names.push(name);
            }
        }
    }
    names
}

/// Client audio pipeline.
/// Captures from microphone and plays received audio to speakers.
pub struct ClientAudio {
    capture_consumer: ringbuf::HeapCons<f32>,
    playback_producer: ringbuf::HeapProd<f32>,
    capture_stream: Option<Stream>,
    playback_stream: Option<Stream>,
    running: Arc<AtomicBool>,
    capture_level: Arc<AtomicU32>,
    playback_level: Arc<AtomicU32>,
    /// Set by error callbacks when a device error occurs
    audio_error: Arc<AtomicBool>,
    /// Gate: when false, capture callback discards audio (prevents speaker→mic bleed)
    capture_gate: Arc<AtomicBool>,
    /// Mute: when true, playback callback outputs zeros (instant speaker silence for TX)
    playback_mute: Arc<AtomicBool>,
    /// Actual sample rate of the capture device
    pub capture_sample_rate: u32,
    /// Actual sample rate of the playback device
    pub playback_sample_rate: u32,
}

// SAFETY: cpal::Stream on Windows (WASAPI) uses COM handles that are safe to
// move between threads. The audio callbacks run on their own dedicated thread.
unsafe impl Send for ClientAudio {}

impl ClientAudio {
    /// Create audio pipeline. Pass device names for specific devices, or None/empty for defaults.
    pub fn new(input_name: Option<&str>, output_name: Option<&str>) -> Result<Self> {
        let (capture_producer, capture_consumer) = HeapRb::<f32>::new(RING_CAPACITY).split();
        // Single interleaved stereo ring buffer (L,R,L,R,...) — 2x capacity for stereo
        let (playback_producer, playback_consumer) = HeapRb::<f32>::new(RING_CAPACITY * 2).split();

        let running = Arc::new(AtomicBool::new(false));
        let capture_level = Arc::new(AtomicU32::new(0));
        let playback_level = Arc::new(AtomicU32::new(0));
        let audio_error = Arc::new(AtomicBool::new(false));
        let capture_gate = Arc::new(AtomicBool::new(false));
        let playback_mute = Arc::new(AtomicBool::new(false));

        let mut audio = Self {
            capture_consumer,
            playback_producer,
            capture_stream: None,
            playback_stream: None,
            running,
            capture_level,
            playback_level,
            audio_error,
            capture_gate,
            playback_mute,
            capture_sample_rate: DEVICE_SAMPLE_RATE,
            playback_sample_rate: DEVICE_SAMPLE_RATE,
        };

        audio.setup_streams(capture_producer, playback_consumer, input_name, output_name)?;
        Ok(audio)
    }

    fn setup_streams(
        &mut self,
        mut capture_producer: ringbuf::HeapProd<f32>,
        mut playback_consumer: ringbuf::HeapCons<f32>,
        input_name: Option<&str>,
        output_name: Option<&str>,
    ) -> Result<()> {
        let host = cpal::default_host();

        let input_device = match input_name {
            Some(name) if !name.is_empty() => {
                let found = host.input_devices()?.find(|d| {
                    d.name().map(|n| n == name).unwrap_or(false)
                });
                match found {
                    Some(d) => d,
                    None => {
                        info!("Input device '{}' not found, using default", name);
                        host.default_input_device().context("no default input device")?
                    }
                }
            }
            _ => host.default_input_device().context("no default input device")?,
        };
        let output_device = match output_name {
            Some(name) if !name.is_empty() => {
                let found = host.output_devices()?.find(|d| {
                    d.name().map(|n| n == name).unwrap_or(false)
                });
                match found {
                    Some(d) => d,
                    None => {
                        info!("Output device '{}' not found, using default", name);
                        host.default_output_device().context("no default output device")?
                    }
                }
            }
            _ => host.default_output_device().context("no default output device")?,
        };

        info!("Client input: {}", input_device.name().unwrap_or_default());
        info!(
            "Client output: {}",
            output_device.name().unwrap_or_default()
        );

        // Prefer 48kHz input (avoids 16kHz headset resampling issues).
        // Fall back to device default if 48kHz not supported.
        let input_config = {
            use cpal::traits::DeviceTrait;
            let preferred_rate = cpal::SampleRate(48000);
            let mut best = input_device.default_input_config()
                .context("no default input config")?;
            if best.sample_rate() != preferred_rate {
                if let Ok(configs) = input_device.supported_input_configs() {
                    for cfg in configs {
                        if cfg.min_sample_rate() <= preferred_rate && cfg.max_sample_rate() >= preferred_rate {
                            best = cfg.with_sample_rate(preferred_rate);
                            info!("Input: using preferred 48kHz instead of device default");
                            break;
                        }
                    }
                }
            }
            best
        };
        let output_config = output_device
            .default_output_config()
            .context("no default output config")?;

        let in_channels = input_config.channels() as usize;
        let out_channels = output_config.channels() as usize;
        self.capture_sample_rate = input_config.sample_rate().0;
        self.playback_sample_rate = output_config.sample_rate().0;

        info!(
            "Input config: {}ch {}Hz",
            in_channels, self.capture_sample_rate
        );
        info!(
            "Output config: {}ch {}Hz",
            out_channels, self.playback_sample_rate
        );

        let in_stream_config = input_config.config();
        let out_stream_config = output_config.config();

        // Capture (microphone) — downmix to mono
        let level = self.capture_level.clone();
        let err_flag = self.audio_error.clone();
        let gate = self.capture_gate.clone();
        let capture_stream = input_device
            .build_input_stream(
                &in_stream_config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    // Only write to ring buffer when gate is open (PTT active).
                    // Prevents speaker→mic feedback from contaminating TX audio.
                    if !gate.load(Ordering::Relaxed) {
                        level.store(0u32, Ordering::Relaxed);
                        return;
                    }
                    let mono: Vec<f32> = data.chunks(in_channels).map(|frame| frame[0]).collect();

                    let rms =
                        (mono.iter().map(|&s| s * s).sum::<f32>() / mono.len() as f32).sqrt();
                    level.store(rms.to_bits(), Ordering::Relaxed);

                    capture_producer.push_slice(&mono);
                },
                move |err| {
                    log::error!("client capture error: {}", err);
                    err_flag.store(true, Ordering::Relaxed);
                },
                None,
            )
            .context("build client capture stream")?;

        // Playback (speakers) — mono or stereo (binaural R channel)
        let level = self.playback_level.clone();
        let err_flag = self.audio_error.clone();
        let mute = self.playback_mute.clone();
        let playback_stream = output_device
            .build_output_stream(
                &out_stream_config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    // Instant mute during TX: output zeros and drain ring buffer
                    if mute.load(Ordering::Relaxed) {
                        let mut drain = vec![0.0f32; data.len()];
                        playback_consumer.pop_slice(&mut drain);
                        data.fill(0.0);
                        level.store(0u32, Ordering::Relaxed);
                        return;
                    }

                    // Read interleaved stereo (L,R,L,R,...) into temp buffer
                    let frames = data.len() / out_channels.max(1);
                    let stereo_samples = frames * 2;
                    let mut stereo_buf = vec![0.0f32; stereo_samples];
                    let read = playback_consumer.pop_slice(&mut stereo_buf);
                    let read_frames = read / 2;

                    // Scatter stereo into output channels (supports >2ch devices)
                    data.fill(0.0);
                    for i in 0..read_frames {
                        let l = stereo_buf[i * 2];
                        let r = stereo_buf[i * 2 + 1];
                        let base = i * out_channels;
                        if out_channels >= 2 {
                            data[base] = l;
                            data[base + 1] = r;
                        } else {
                            data[base] = (l + r) * 0.5;
                        }
                    }

                    // RMS level from L channel
                    let sum_sq: f32 = (0..read_frames).map(|i| { let s = stereo_buf[i * 2]; s * s }).sum();
                    let rms = (sum_sq / read_frames.max(1) as f32).sqrt();
                    level.store(rms.to_bits(), Ordering::Relaxed);
                },
                move |err| {
                    log::error!("client playback error: {}", err);
                    err_flag.store(true, Ordering::Relaxed);
                },
                None,
            )
            .context("build client playback stream")?;

        self.capture_stream = Some(capture_stream);
        self.playback_stream = Some(playback_stream);
        Ok(())
    }

    pub fn start(&self) -> Result<()> {
        if let Some(ref s) = self.capture_stream {
            s.play().context("start client capture")?;
        }
        if let Some(ref s) = self.playback_stream {
            s.play().context("start client playback")?;
        }
        self.running.store(true, Ordering::Relaxed);
        info!("Client audio started");
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        if let Some(ref s) = self.capture_stream {
            s.pause().context("pause client capture")?;
        }
        if let Some(ref s) = self.playback_stream {
            s.pause().context("pause client playback")?;
        }
        self.running.store(false, Ordering::Relaxed);
        info!("Client audio stopped");
        Ok(())
    }

    pub fn read_capture(&mut self, buf: &mut [f32]) -> usize {
        self.capture_consumer.pop_slice(buf)
    }

    pub fn write_playback(&mut self, buf: &[f32]) -> usize {
        self.playback_producer.push_slice(buf)
    }

    /// Write stereo: interleave L+R into single ring buffer.
    pub fn write_playback_stereo(&mut self, left: &[f32], right: &[f32]) -> usize {
        let n = left.len().min(right.len());
        let mut interleaved = Vec::with_capacity(n * 2);
        for i in 0..n {
            interleaved.push(left[i]);   // L sample
            interleaved.push(right[i]);  // R sample
        }
        self.playback_producer.push_slice(&interleaved) / 2
    }

    pub fn capture_level(&self) -> f32 {
        f32::from_bits(self.capture_level.load(Ordering::Relaxed))
    }

    pub fn playback_level(&self) -> f32 {
        f32::from_bits(self.playback_level.load(Ordering::Relaxed))
    }

    /// Check if an audio device error has occurred
    pub fn has_error(&self) -> bool {
        self.audio_error.load(Ordering::Relaxed)
    }
}

impl sdr_remote_logic::audio::AudioBackend for ClientAudio {
    fn read_capture(&mut self, buf: &mut [f32]) -> usize {
        ClientAudio::read_capture(self, buf)
    }

    fn write_playback(&mut self, buf: &[f32]) -> usize {
        ClientAudio::write_playback(self, buf)
    }

    fn supports_stereo(&self) -> bool { true }

    fn write_playback_stereo(&mut self, left: &[f32], right: &[f32]) -> usize {
        ClientAudio::write_playback_stereo(self, left, right)
    }

    fn capture_level(&self) -> f32 {
        ClientAudio::capture_level(self)
    }

    fn playback_level(&self) -> f32 {
        ClientAudio::playback_level(self)
    }

    fn has_error(&self) -> bool {
        ClientAudio::has_error(self)
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
