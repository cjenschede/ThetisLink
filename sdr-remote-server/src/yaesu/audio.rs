// SPDX-License-Identifier: GPL-2.0-or-later
//! Yaesu server-side audio: cpal capture/playback stream builders + device
//! enumeration. Extracted verbatim from `yaesu/mod.rs` - pure relocation, no
//! behaviour change. `use super::*;` pulls in the shared imports (Arc/Mutex,
//! info/warn); `pub(super)` keeps the stream builders callable from the poll
//! loop / reconnect in the parent module.

use super::*;

// --- Audio stream builders (used for initial setup + reconnect) ---

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Splits a configured device-pattern into (name-substring, position). An optional
/// suffix "#N" picks the **N-th** (1-based) device that matches the name -
/// needed when two radios have an identically named USB audio device
/// (e.g. 2× "USB Audio CODEC"). No suffix = #1 (first match) = unchanged behaviour.
/// Example config: `yaesu2_audio=USB Audio CODEC#2`.
fn parse_device_pattern(pattern: &str) -> (String, usize) {
    if let Some((name, idx)) = pattern.rsplit_once('#') {
        if let Ok(n) = idx.trim().parse::<usize>() {
            if n >= 1 {
                return (name.to_string(), n);
            }
        }
    }
    (pattern.to_string(), 1)
}

/// Build a cpal input capture stream that feeds into an existing tokio sender.
pub(super) fn build_capture_stream(
    device_pattern: &str,
    tx: tokio::sync::mpsc::Sender<Vec<f32>>,
    last_audio_time: Arc<std::sync::atomic::AtomicU64>,
    prefix: &str,
    // Dual-RX channel choice (FTX-1): 0 = L (hardware-RX 1), 1 = R (hardware-RX 2),
    // 2 = mix (average). Mono devices ignore this (downmix branch does not run).
    channel: u8,
) -> Result<(cpal::Stream, u32), String> {
    let host = cpal::default_host();
    let (pat_name, pos) = parse_device_pattern(device_pattern);
    let pat = pat_name.to_lowercase();
    let device = host.input_devices()
        .map_err(|e| format!("enumerate input devices: {}", e))?
        .filter(|d| d.name().map(|n| n.to_lowercase().contains(&pat)).unwrap_or(false))
        .nth(pos - 1)
        .ok_or_else(|| format!("no input device matching '{}' (#{})", pat_name, pos))?;

    let device_name = device.name().unwrap_or_default();
    // Device name with prefix: crucial for edge-case 6 (two identical
    // "USB Audio CODEC" devices -> this way you see which device belongs to which radio).
    info!("{} audio input: {}", prefix, device_name);

    let config = device.default_input_config()
        .map_err(|e| format!("input config: {}", e))?;
    let sample_rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    info!("{} audio: {}Hz, {} channels, {:?}", prefix, sample_rate, channels, config.sample_format());
    let prefix_err = prefix.to_string();

    let stream = device.build_input_stream(
        &config.into(),
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            // Operator LATENCY-WAIVER (release v2.0.0, operator: PA3GHM/cjenschede):
            // Per-callback Vec allocation is deliberately accepted on this server-side
            // Yaesu RX path. The operator weighed the latency priority against the
            // implementation cost and chose the current approach because:
            //   (a) the server runs on the Thetis PC, no local real-time audio output;
            //       audio latency is overshadowed by encode + network path
            //   (b) alloc cost ~50µs is <0.5% of the ~10ms frame budget - not
            //       audible under normal load
            //   (c) Vec::with_capacity(frames) prevents grow-realloc with a stable
            //       input config
            //   (d) tokio::mpsc::Sender consumes the Vec (ownership move); zero
            //       alloc requires a Vec pool with a return channel - a non-trivial
            //       refactor, planned for post-release optimization in v2.1+.
            let frames = data.len() / channels.max(1);
            let mut mono: Vec<f32> = Vec::with_capacity(frames);
            if channels > 1 {
                for ch in data.chunks(channels) {
                    let s = match channel {
                        0 => ch[0],
                        1 => *ch.get(1).unwrap_or(&ch[0]),
                        _ => ch.iter().sum::<f32>() / ch.len() as f32, // mix
                    };
                    mono.push(s);
                }
            } else {
                mono.extend_from_slice(data);
            }
            let _ = tx.try_send(mono);
            // Update watchdog timestamp
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            last_audio_time.store(now_ms, std::sync::atomic::Ordering::Relaxed);
        },
        move |err| { log::warn!("{} audio capture error: {}", prefix_err, err); },
        None,
    ).map_err(|e| format!("build input stream: {}", e))?;

    stream.play().map_err(|e| format!("start capture: {}", e))?;
    info!("{} audio capture started", prefix);

    Ok((stream, sample_rate))
}

/// Build a cpal output playback stream with a swappable ring buffer producer.
pub(super) fn build_output_stream(
    device_pattern: &str,
    producer_handle: Arc<Mutex<Option<ringbuf::HeapProd<f32>>>>,
    prefix: &str,
) -> Result<(cpal::Stream, u32), String> {
    let host = cpal::default_host();
    let (pat_name, pos) = parse_device_pattern(device_pattern);
    let pat = pat_name.to_lowercase();
    // Per-radio output device. When two radios both report as
    // "USB Audio CODEC" (edge-case 6) the TX path MUST match the output device
    // that belongs to THIS radio - otherwise radio-1's TX audio goes to
    // radio-0's codec; the device name must stay per slot.
    // We match on the per-radio device pattern (same USB-CODEC = same
    // friendly name for capture and playback) + the #N position so two
    // identically named devices can be told apart. Fallback to
    // "USB Audio CODEC" (same position) if the specific pattern yields no output
    // -> no regression versus the old behaviour, single-radio keeps working.
    let pick = |p: &str, n: usize| -> Option<cpal::Device> {
        host.output_devices()
            .ok()?
            .filter(|d| d.name().map(|nm| nm.to_lowercase().contains(p)).unwrap_or(false))
            .nth(n.saturating_sub(1))
    };
    let device = match pick(&pat, pos) {
        Some(d) => d,
        None => {
            if pat != "usb audio codec" {
                warn!(
                    "{} no output device #{} matches '{}' - fallback to 'USB Audio CODEC' #{}",
                    prefix, pos, pat_name, pos
                );
            }
            pick("usb audio codec", pos)
                .ok_or_else(|| format!("no output device matching '{}' (#{})", pat_name, pos))?
        }
    };

    let device_name = device.name().unwrap_or_default();
    info!("{} audio output: {}", prefix, device_name);

    let config = device.default_output_config()
        .map_err(|e| format!("output config: {}", e))?;
    let sample_rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    info!("{} audio output: {}Hz, {} channels, {:?}", prefix, sample_rate, channels, config.sample_format());
    let prefix_err = prefix.to_string();

    // Create new ring buffer
    use ringbuf::traits::Split;
    let (producer, mut consumer) = ringbuf::HeapRb::<f32>::new(sample_rate as usize * 2).split();

    // Install the new producer so the bridge thread can write to it
    *producer_handle.lock().unwrap() = Some(producer);

    let stream = device.build_output_stream(
        &config.into(),
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            use ringbuf::traits::Consumer as _;
            for sample in data.iter_mut() {
                *sample = consumer.try_pop().unwrap_or(0.0);
            }
        },
        move |err| { log::warn!("{} audio output error: {}", prefix_err, err); },
        None,
    ).map_err(|e| format!("build output stream: {}", e))?;

    stream.play().map_err(|e| format!("start playback: {}", e))?;
    info!("{} audio output started", prefix);

    Ok((stream, sample_rate))
}

/// Legacy structs kept for API compatibility (unused internally now)
#[allow(dead_code)]
pub struct YaesuAudio {
    pub _capture_stream: cpal::Stream,
    pub rx_audio_rx: tokio::sync::mpsc::Receiver<Vec<f32>>,
    pub sample_rate: u32,
}
unsafe impl Send for YaesuAudio {}

#[allow(dead_code)]
pub struct YaesuAudioOutput {
    _playback_stream: cpal::Stream,
    pub tx_audio_tx: tokio::sync::mpsc::Sender<Vec<f32>>,
    pub sample_rate: u32,
}
unsafe impl Send for YaesuAudioOutput {}

/// List available audio input devices (for UI combo box).
pub fn available_audio_inputs() -> Vec<String> {
    let host = cpal::default_host();
    host.input_devices()
        .map(|devices| {
            devices.filter_map(|d| d.name().ok()).collect()
        })
        .unwrap_or_default()
}

/// Enumerate output (render) devices - for the separate Yaesu TX/output device
/// picker (PATCH-yaesu-output-device).
pub fn available_audio_outputs() -> Vec<String> {
    let host = cpal::default_host();
    host.output_devices()
        .map(|devices| {
            devices.filter_map(|d| d.name().ok()).collect()
        })
        .unwrap_or_default()
}
