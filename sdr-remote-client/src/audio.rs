// SPDX-License-Identifier: GPL-2.0-or-later

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::Stream;
use log::{info, warn};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::HeapRb;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use sdr_remote_core::DEVICE_SAMPLE_RATE;
use sdr_remote_logic::audio::{
    mix_alarm, mix_swr_alarm, ptt_dropped_alarm_samples, swr_alarm_hold_samples, PA_DRIVE_ALARM, PTT_DROPPED_ALARM,
};
use sdr_remote_logic::sidetone::{mix_sidetone, Sidetone, SidetoneState};

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

/// A device name without the number Windows puts in front of a USB device
/// ("Microphone (2- USB Audio CODEC)" -> "Microphone (USB Audio CODEC)"). Windows
/// renumbers on replugging, and a stored name would stop matching the same
/// microphone (build 188). Only that number is ignored.
pub fn without_windows_number(name: &str) -> String {
    let Some(open) = name.find('(') else { return name.to_string() };
    let rest = &name[open + 1..];
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 && rest[digits..].starts_with("- ") {
        format!("{}({}", &name[..open], &rest[digits + 2..])
    } else {
        name.to_string()
    }
}

/// Which of these device names is the chosen one (builds 188, 189).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// The name itself.
    Exact(usize),
    /// The only one that differs just in the Windows number.
    Renumbered(usize),
    /// More than one differs just in the Windows number: two of the same
    /// codec, and the number was what told them apart. None is taken (review
    /// of 188: the first in the list could be the other codec).
    Ambiguous(usize),
    Missing,
}

/// The chosen name among the names there are: exactly, or else the one and
/// only name that differs just in the Windows number.
pub fn choose_input(names: &[String], wanted: &str) -> Choice {
    if let Some(i) = names.iter().position(|n| n == wanted) {
        return Choice::Exact(i);
    }
    let want = without_windows_number(wanted);
    let same: Vec<usize> = (0..names.len()).filter(|&i| without_windows_number(&names[i]) == want).collect();
    match same.len() {
        0 => Choice::Missing,
        1 => Choice::Renumbered(same[0]),
        n => Choice::Ambiguous(n),
    }
}

/// The input device chosen under this name, and how it was found.
fn find_input(host: &cpal::Host, name: &str) -> Result<(Option<cpal::Device>, Choice)> {
    let devices: Vec<cpal::Device> = host.input_devices()?.collect();
    let names: Vec<String> = devices.iter().map(|d| d.name().unwrap_or_default()).collect();
    let choice = choose_input(&names, name);
    let device = match choice {
        Choice::Exact(i) | Choice::Renumbered(i) => devices.into_iter().nth(i),
        Choice::Ambiguous(_) | Choice::Missing => None,
    };
    Ok((device, choice))
}

/// The engine's audio opening. The very first, without names, gets the chosen
/// devices (`first`, taken once) so the default microphone never opens first
/// (build 189); if those are there but do not open, the defaults - a chosen
/// device must not stop the engine, and the chosen microphone stays refused
/// because `SetInputDevice` follows and fails (builds 190, 191).
pub fn first_open<T>(
    first: Option<(String, String)>,
    input: Option<&str>,
    output: Option<&str>,
    open: impl Fn(Option<&str>, Option<&str>) -> Result<T>,
) -> Result<T> {
    match (&first, input, output) {
        (Some((i, o)), None, None) => {
            let (i, o) = (Some(i.as_str()).filter(|s| !s.is_empty()), Some(o.as_str()).filter(|s| !s.is_empty()));
            match open(i, o) {
                Ok(a) => Ok(a),
                Err(e) if i.is_some() || o.is_some() => {
                    // Named in advance (working rule 11, build 190).
                    warn!("Could not open the chosen devices at start ({}) - opening the defaults; the chosen microphone stays refused", e);
                    open(None, None)
                }
                Err(e) => Err(e),
            }
        }
        _ => open(input, output),
    }
}

/// How often a missing microphone is looked for (build 189).
const MIC_LOOK_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// Looks for a missing microphone on a thread of its own: counting the devices
/// takes 130-160 ms (measured, review of 188), and in the engine's playout tick
/// that was a stall every 2 s. Sets `back` once and stops; stops on `stop`.
fn watch_for_input(name: String, back: Arc<AtomicBool>, stop: Arc<AtomicBool>) {
    let spawned = std::thread::Builder::new().name("mic-watch".to_string()).spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(MIC_LOOK_EVERY);
            if stop.load(Ordering::Relaxed) {
                return;
            }
            if let Ok((Some(_), _)) = find_input(&cpal::default_host(), &name) {
                back.store(true, Ordering::Relaxed);
                return;
            }
        }
    });
    if let Err(e) = spawned {
        warn!("no thread to look for the microphone: {}", e);
    }
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
    /// Gate: when false, capture callback discards audio (prevents speaker->mic bleed)
    capture_gate: Arc<AtomicBool>,
    /// Mute: when true, playback callback outputs zeros (instant speaker silence for TX)
    playback_mute: Arc<AtomicBool>,
    /// High-SWR alarm, as a watchdog rather than a latch: samples of beep left
    /// to play. Refreshed by each radio state push; runs out by itself if those
    /// stop (server or radio gone), so the alarm can never get stuck on.
    /// The beep is mixed in even while muted for TX, which is when it matters.
    swr_alarm: Arc<AtomicU32>,
    /// Its own counter, so the two warnings cannot silence each other.
    pa_drive_alarm: Arc<AtomicU32>,
    /// The press let go with the session (build 253): armed once, plays out.
    ptt_dropped_alarm: Arc<AtomicU32>,
    /// The CW sidetone, keyed by the paddle reader and mixed in the playback
    /// callback - also while muted for transmit, which is when it is keyed.
    sidetone: Arc<Sidetone>,
    /// Samples of mic to discard right after the gate opens (keyup), so the
    /// speaker/chassis switch-on spike decays before mic audio is transmitted.
    /// 0 = disabled (no added latency - the default for isolated/headset audio).
    capture_gate_delay_samples: Arc<AtomicU32>,
    /// The chosen microphone that was not found, by the name it was chosen
    /// under. No other microphone is opened in its place (build 188): a wrong
    /// one sounds like it works, and the modulation goes bad unnoticed.
    missing_input: Option<String>,
    /// Set by the watch thread when the missing microphone is there again; and
    /// the flag that stops it when this pipeline goes (build 189).
    input_back: Arc<AtomicBool>,
    input_watch_stop: Arc<AtomicBool>,
    /// Actual sample rate of the capture device
    pub capture_sample_rate: u32,
    /// Actual sample rate of the playback device
    pub playback_sample_rate: u32,
}

impl Drop for ClientAudio {
    /// The watch thread goes with the pipeline (build 189).
    fn drop(&mut self) {
        self.input_watch_stop.store(true, Ordering::Relaxed);
    }
}

// SAFETY: cpal::Stream on Windows (WASAPI) uses COM handles that are safe to
// move between threads. The audio callbacks run on their own dedicated thread.
unsafe impl Send for ClientAudio {}

impl ClientAudio {
    /// Create audio pipeline. Pass device names for specific devices, or None/empty for defaults.
    pub fn new(input_name: Option<&str>, output_name: Option<&str>, sidetone: Arc<Sidetone>) -> Result<Self> {
        let (capture_producer, capture_consumer) = HeapRb::<f32>::new(RING_CAPACITY).split();
        // Single interleaved stereo ring buffer (L,R,L,R,...) - 2x capacity for stereo
        let (playback_producer, playback_consumer) = HeapRb::<f32>::new(RING_CAPACITY * 2).split();

        let running = Arc::new(AtomicBool::new(false));
        let capture_level = Arc::new(AtomicU32::new(0));
        let playback_level = Arc::new(AtomicU32::new(0));
        let audio_error = Arc::new(AtomicBool::new(false));
        let capture_gate = Arc::new(AtomicBool::new(false));
        let playback_mute = Arc::new(AtomicBool::new(false));
        let swr_alarm = Arc::new(AtomicU32::new(0));
        let pa_drive_alarm = Arc::new(AtomicU32::new(0));
        let ptt_dropped_alarm = Arc::new(AtomicU32::new(0));
        let capture_gate_delay_samples = Arc::new(AtomicU32::new(0));

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
            swr_alarm,
            pa_drive_alarm,
            ptt_dropped_alarm,
            sidetone,
            capture_gate_delay_samples,
            missing_input: None,
            input_back: Arc::new(AtomicBool::new(false)),
            input_watch_stop: Arc::new(AtomicBool::new(false)),
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

        // Only the chosen microphone, or none (build 188): the speaker may fall
        // back, a microphone may not - see `missing_input`.
        let input_device = match input_name {
            Some(name) if !name.is_empty() => match find_input(&host, name)? {
                (Some(d), choice) => {
                    if let Choice::Renumbered(_) = choice {
                        let got = d.name().unwrap_or_default();
                        info!("Client input: {} (matched '{}' without the Windows number)", got, name);
                    }
                    Some(d)
                }
                (None, choice) => {
                    if let Choice::Ambiguous(n) = choice {
                        // Named in advance (working rule 11, build 189).
                        warn!(
                            "Input device '{}' matches {} devices without the Windows number - no microphone; pick it again in the settings",
                            name, n
                        );
                    } else {
                        // Named in advance (working rule 11, build 188).
                        warn!("Input device '{}' not found - no microphone; transmitting is refused until it is back", name);
                    }
                    self.missing_input = Some(name.to_string());
                    watch_for_input(name.to_string(), self.input_back.clone(), self.input_watch_stop.clone());
                    None
                }
            },
            _ => Some(host.default_input_device().context("no default input device")?),
        };
        let output_device = match output_name {
            Some(name) if !name.is_empty() => {
                let found = host.output_devices()?.find(|d| {
                    d.name().map(|n| n == name).unwrap_or(false)
                });
                match found {
                    Some(d) => d,
                    None => {
                        let d = host.default_output_device().context("no default output device")?;
                        let got = d.name().unwrap_or_else(|_| "(unknown)".to_string());
                        warn!(
                            "Output device '{}' not found - using '{}' instead. \
                             Windows renumbers USB devices, so a stored name can stop matching; \
                             pick the output again in the settings to make this stick.",
                            name, got
                        );
                        d
                    }
                }
            }
            _ => host.default_output_device().context("no default output device")?,
        };

        if let Some(d) = &input_device {
            info!("Client input: {}", d.name().unwrap_or_default());
        }
        info!(
            "Client output: {}",
            output_device.name().unwrap_or_default()
        );

        // Prefer 48kHz input (avoids 16kHz headset resampling issues).
        // Fall back to device default if 48kHz not supported.
        let input_config = match &input_device {
            None => None,
            Some(input_device) => Some({
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
            }),
        };
        let output_config = output_device
            .default_output_config()
            .context("no default output config")?;

        let in_channels = input_config.as_ref().map_or(1, |c| c.channels() as usize);
        let out_channels = output_config.channels() as usize;
        if let Some(c) = &input_config {
            self.capture_sample_rate = c.sample_rate().0;
        }
        self.playback_sample_rate = output_config.sample_rate().0;

        info!(
            "Input config: {}ch {}Hz",
            in_channels, self.capture_sample_rate
        );
        info!(
            "Output config: {}ch {}Hz",
            out_channels, self.playback_sample_rate
        );

        let in_stream_config = input_config.as_ref().map(|c| c.config());
        let out_stream_config = output_config.config();

        // Capture (microphone) - downmix to mono
        let level = self.capture_level.clone();
        let err_flag = self.audio_error.clone();
        let gate = self.capture_gate.clone();
        let gate_delay_samples = self.capture_gate_delay_samples.clone();
        // Per-callback state for the post-keyup gate-delay (mirrors the Android path):
        // when the gate opens we discard `gate_delay_remaining` mono samples so the
        // speaker/chassis switch-on spike decays before mic audio reaches TX.
        let mut was_open = false;
        let mut gate_delay_remaining: u32 = 0;
        // Pre-allocated scratch - retains capacity across callback invocations to
        // avoid real-time alloc on the audio thread (latency priority).
        let mut mono_scratch: Vec<f32> = Vec::with_capacity(8192);
        let capture_stream = match (input_device.as_ref(), in_stream_config.as_ref()) {
            (Some(input_device), Some(in_stream_config)) => Some(input_device
            .build_input_stream(
                in_stream_config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    // Only write to ring buffer when gate is open (PTT active).
                    // Prevents speaker->mic feedback from contaminating TX audio.
                    if !gate.load(Ordering::Relaxed) {
                        was_open = false;
                        gate_delay_remaining = 0;
                        level.store(0u32, Ordering::Relaxed);
                        return;
                    }
                    // On the open transition, latch the configured discard window.
                    if !was_open {
                        was_open = true;
                        gate_delay_remaining = gate_delay_samples.load(Ordering::Relaxed);
                    }
                    mono_scratch.clear();
                    for frame in data.chunks(in_channels) {
                        mono_scratch.push(frame[0]);
                    }

                    // Discard the first N mono samples after keyup (gate-delay).
                    let skip = (gate_delay_remaining as usize).min(mono_scratch.len());
                    gate_delay_remaining -= skip as u32;
                    let out = &mono_scratch[skip..];
                    if out.is_empty() {
                        level.store(0u32, Ordering::Relaxed);
                        return;
                    }

                    let rms = (out.iter().map(|&s| s * s).sum::<f32>()
                        / out.len().max(1) as f32).sqrt();
                    level.store(rms.to_bits(), Ordering::Relaxed);

                    capture_producer.push_slice(out);
                },
                move |err| {
                    log::error!("client capture error: {}", err);
                    err_flag.store(true, Ordering::Relaxed);
                },
                None,
            )
            .context("build client capture stream")?),
            _ => None,
        };

        // Playback (speakers) - mono or stereo (binaural R channel)
        let level = self.playback_level.clone();
        let err_flag = self.audio_error.clone();
        let mute = self.playback_mute.clone();
        let alarm = self.swr_alarm.clone();
        let pa_alarm = self.pa_drive_alarm.clone();
        let alarm_rate = self.playback_sample_rate;
        let mut alarm_pos: u32 = 0;
        let mut alarm_phase: f32 = 0.0;
        let mut pa_alarm_pos: u32 = 0;
        let mut pa_alarm_phase: f32 = 0.0;
        let dropped_alarm = self.ptt_dropped_alarm.clone();
        let mut dropped_pos: u32 = 0;
        let mut dropped_phase: f32 = 0.0;
        let sidetone = self.sidetone.clone();
        let mut sidetone_state = SidetoneState::default();
        // Pre-allocated scratches for mute-drain and stereo-read. Capacity grows
        // automatically on the first call that needs more; no alloc after that.
        let mut drain_scratch: Vec<f32> = Vec::with_capacity(8192);
        let mut stereo_scratch: Vec<f32> = Vec::with_capacity(8192);
        let playback_stream = output_device
            .build_output_stream(
                &out_stream_config,
                move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                    // What the device itself predicts between this callback and
                    // the speaker: the half of the sidetone's latency that the
                    // callback cannot measure (CW design, measurement 4).
                    // Told on every callback, and told 0 when there is nothing to
                    // tell: the Sidetone outlives a device switch, and the figure
                    // of the previous device must not stand for this one.
                    let ts = info.timestamp();
                    let device_us = ts
                        .playback
                        .duration_since(&ts.callback)
                        .map_or(0, |d| d.as_micros().min(u32::MAX as u128) as u32);
                    sidetone.note_output_latency(device_us);
                    // Two ways to fill the buffer, and ONE place that mixes into it.
                    // The alarms and the sidetone used to be mixed at the end of
                    // each branch; a third branch with an early return would have
                    // dropped all three without a sound (rule 15). Nothing returns
                    // from inside a branch any more, and fitness.ps1 counts the
                    // mixing place: one.
                    if mute.load(Ordering::Relaxed) {
                        // Instant mute during TX: output zeros and drain ring buffer.
                        if drain_scratch.len() < data.len() {
                            drain_scratch.resize(data.len(), 0.0);
                        }
                        playback_consumer.pop_slice(&mut drain_scratch[..data.len()]);
                        data.fill(0.0);
                        level.store(0u32, Ordering::Relaxed);
                    } else {
                        // Read interleaved stereo (L,R,L,R,...) into temp buffer
                        let frames = data.len() / out_channels.max(1);
                        let stereo_samples = frames * 2;
                        if stereo_scratch.len() < stereo_samples {
                            stereo_scratch.resize(stereo_samples, 0.0);
                        }
                        let stereo_buf = &mut stereo_scratch[..stereo_samples];
                        let read = playback_consumer.pop_slice(stereo_buf);
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

                        // RMS level from L channel (before anything is mixed in, so
                        // the meter keeps showing received audio only)
                        let sum_sq: f32 = (0..read_frames).map(|i| { let s = stereo_buf[i * 2]; s * s }).sum();
                        let rms = (sum_sq / read_frames.max(1) as f32).sqrt();
                        level.store(rms.to_bits(), Ordering::Relaxed);
                    }

                    // High SWR is a TX condition and the sidetone is keyed during
                    // TX, so all three play into the muted buffer as well.
                    mix_swr_alarm(data, out_channels, alarm_rate, &alarm, &mut alarm_pos, &mut alarm_phase);
                    mix_alarm(PA_DRIVE_ALARM, data, out_channels, alarm_rate, &pa_alarm, &mut pa_alarm_pos, &mut pa_alarm_phase);
                    mix_alarm(PTT_DROPPED_ALARM, data, out_channels, alarm_rate, &dropped_alarm, &mut dropped_pos, &mut dropped_phase);
                    mix_sidetone(data, out_channels, alarm_rate, &sidetone, &mut sidetone_state);
                },
                move |err| {
                    log::error!("client playback error: {}", err);
                    err_flag.store(true, Ordering::Relaxed);
                },
                None,
            )
            .context("build client playback stream")?;

        self.capture_stream = capture_stream;
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

    #[allow(dead_code)] // public API kept symmetric to start()
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
    /// Direct per-sample try_push avoids allocating an intermediate Vec
    /// (latency priority). A `vacant_len >= 2` pre-check per frame prevents
    /// a half-frame edge case where L succeeds but R fails.
    pub fn write_playback_stereo(&mut self, left: &[f32], right: &[f32]) -> usize {
        let n = left.len().min(right.len());
        let mut written = 0;
        for i in 0..n {
            // Pre-check: ring must have room for both L and R together - otherwise
            // frame-skip to avoid leaving a half-frame in the ring.
            if self.playback_producer.vacant_len() < 2 {
                break;
            }
            let _ = self.playback_producer.try_push(left[i]);
            let _ = self.playback_producer.try_push(right[i]);
            written += 1;
        }
        written
    }

    pub fn capture_level(&self) -> f32 {
        f32::from_bits(self.capture_level.load(Ordering::Relaxed))
    }

    pub fn playback_level(&self) -> f32 {
        f32::from_bits(self.playback_level.load(Ordering::Relaxed))
    }

    /// The chosen microphone that was not found (build 188).
    pub fn missing_input(&self) -> Option<String> {
        self.missing_input.clone()
    }

    /// Whether the missing microphone is there again (build 188). One load: the
    /// looking happens on the watch thread (build 189).
    pub fn missing_input_is_back(&self) -> bool {
        self.input_back.load(Ordering::Relaxed)
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

    fn missing_input(&self) -> Option<String> {
        ClientAudio::missing_input(self)
    }
    fn missing_input_is_back(&self) -> bool {
        ClientAudio::missing_input_is_back(self)
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

    fn set_capture_gate_delay_ms(&mut self, delay_ms: u32) {
        // ms -> samples at the capture rate (round up), mirroring the Android path.
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

    fn set_pa_drive_alarm(&mut self, on: bool) {
        // Same re-arming as above, its own counter and its own tone.
        let samples = if on { swr_alarm_hold_samples(self.playback_sample_rate) } else { 0 };
        self.pa_drive_alarm.store(samples, Ordering::Relaxed);
    }

    fn sound_ptt_dropped(&mut self) {
        // Armed once; the callback counts it down to silence.
        self.ptt_dropped_alarm.store(ptt_dropped_alarm_samples(self.playback_sample_rate), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A missing microphone refuses every press of the PTT, for Thetis and both
    /// Yaesu slots, on the path a transmitter held by another station takes
    /// (build 188) - the field log is the proof that nothing is sent.
    #[test]
    fn a_missing_microphone_refuses_the_ptt() {
        let update = include_str!("ui/update.rs");
        assert_eq!(update.matches("held || self.mic_missing.is_some(),").count(), 1);
        assert_eq!(
            update.matches("let d = desktop_frame(self.thetis_latches, true, self.other_tx || self.mic_missing.is_some(), self.ptt);").count(),
            1
        );
        assert_eq!(update.matches("desktop_frame(").count(), 2, "no PTT path around the rule");
        // And every PTT button's face hears of it: Thetis here, both Yaesu
        // buttons through yaesu_ptt_face (review of 191: dropping either stayed
        // green, `..Default::default()` hides it).
        let popouts = include_str!("ui/popouts.rs");
        assert_eq!(
            update.matches("mic_missing: self.mic_missing.is_some(),").count()
                + popouts.matches("mic_missing: self.mic_missing.is_some(),").count(),
            2
        );
        // The engine's audio opens through first_open (review of 191, N2b).
        assert!(include_str!("main.rs").contains("audio::first_open(first, input, output, open)"));
    }

    /// The first opening: the chosen devices; if they will not open, the
    /// defaults; with no choice a failure stays a failure; and only the first
    /// call gets the choice (build 191; review of 190: taking the fallback out
    /// stayed green).
    #[test]
    fn the_first_opening_falls_back_to_the_defaults() {
        let opened = std::cell::RefCell::new(Vec::new());
        let open = |i: Option<&str>, o: Option<&str>| -> Result<()> {
            opened.borrow_mut().push((i.map(str::to_string), o.map(str::to_string)));
            if i == Some("busy mic") { anyhow::bail!("device in use") } else { Ok(()) }
        };
        let chosen = |i: &str, o: &str| Some((i.to_string(), o.to_string()));
        // The choice opens.
        assert!(first_open(chosen("RODE", "Speakers"), None, None, &open).is_ok());
        assert_eq!(opened.borrow_mut().drain(..).collect::<Vec<_>>(), vec![(Some("RODE".into()), Some("Speakers".into()))]);
        // The choice is there but busy: the defaults.
        assert!(first_open(chosen("busy mic", ""), None, None, &open).is_ok());
        assert_eq!(opened.borrow_mut().drain(..).collect::<Vec<_>>(), vec![(Some("busy mic".into()), None), (None, None)]);
        // No choice, and the defaults fail: the failure stands.
        let failing = |_: Option<&str>, _: Option<&str>| -> Result<()> { anyhow::bail!("no device") };
        assert!(first_open(chosen("", ""), None, None, failing).is_err());
        // Not the first call: what the engine asks, unchanged.
        assert!(first_open(None, Some("busy mic"), None, &open).is_err());
        assert!(first_open(None, None, None, &open).is_ok());
        assert_eq!(opened.borrow_mut().last().cloned(), Some((None, None)));
    }

    /// The choice: the name itself first; without the number only when exactly
    /// one fits - two of the same codec are told apart by that number, and the
    /// first in the list could be the other one (review of 188, M1 and B).
    #[test]
    fn the_chosen_microphone_is_found_or_none_is_taken() {
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let two = names(&["Microphone (Realtek(R) Audio)", "Microphone (2- USB Audio CODEC)", "Microphone (3- USB Audio CODEC)"]);
        assert_eq!(choose_input(&two, "Microphone (3- USB Audio CODEC)"), Choice::Exact(2));
        assert_eq!(choose_input(&two, "Microphone (4- USB Audio CODEC)"), Choice::Ambiguous(2), "none of two is taken");
        let one = names(&["Microphone (Realtek(R) Audio)", "Microphone (RODE NT-USB)"]);
        assert_eq!(choose_input(&one, "Microphone (2- RODE NT-USB)"), Choice::Renumbered(1), "the field case of 13-10");
        assert_eq!(choose_input(&one, "Microphone (2- Jabra Link 380)"), Choice::Missing);
        assert_eq!(choose_input(&[], "Microphone (2- RODE NT-USB)"), Choice::Missing);
    }

    /// No other microphone than the chosen one: the default device is opened
    /// only when nothing is chosen (review of 188, M5: putting the old
    /// fallback back stayed green).
    #[test]
    fn the_default_microphone_opens_only_without_a_choice() {
        let src = include_str!("audio.rs");
        let src = &src[..src.find("#[cfg(test)]").unwrap()];
        assert_eq!(src.matches("default_input_device()").count(), 1);
        assert!(src.contains("_ => Some(host.default_input_device().context(\"no default input device\")?),"));
    }

    /// The Windows number in front of a USB device is the one thing ignored
    /// when looking for the chosen microphone (build 188).
    #[test]
    fn only_the_windows_number_is_ignored() {
        assert_eq!(without_windows_number("Microphone (2- USB Audio CODEC)"), "Microphone (USB Audio CODEC)");
        assert_eq!(without_windows_number("Microphone (12- USB Audio CODEC)"), "Microphone (USB Audio CODEC)");
        assert_eq!(
            without_windows_number("Microphone (3- USB Audio CODEC)"),
            without_windows_number("Microphone (2- USB Audio CODEC)")
        );
        // Anything else stays as it is: another device is another device.
        assert_eq!(without_windows_number("Microphone (Realtek(R) Audio)"), "Microphone (Realtek(R) Audio)");
        assert_eq!(without_windows_number("Headset (2 Pro)"), "Headset (2 Pro)");
        assert_eq!(without_windows_number("No brackets"), "No brackets");
        assert_ne!(
            without_windows_number("Microphone (2- USB Audio CODEC)"),
            without_windows_number("Microphone (2- Jabra Link 380)")
        );
    }
}
