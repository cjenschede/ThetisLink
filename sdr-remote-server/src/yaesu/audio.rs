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

/// The device name inside a Windows endpoint name, or the whole thing when
/// there is no such part.
///
/// Windows names the two ends of one USB sound device after their ROLE, in the
/// operator's own language, with the device itself in brackets:
///
/// ```text
///   Microfoon (2- USB Audio Device)     <- capture
///   Speakers (2- USB Audio Device)      <- playback
/// ```
///
/// Only the bracketed part is the device. That is what makes "use the same
/// device for output as for input" possible without knowing that the prefix is
/// "Microfoon" in Dutch, "Microphone" in English and "Mikrofon" in German.
///
/// A name with no brackets - an FT-991A presents both ends as plain
/// "USB Audio CODEC" - comes back unchanged, which is exactly the case that has
/// always worked.
fn device_core_name(full: &str) -> &str {
    let Some(open) = full.rfind('(') else {
        return full;
    };
    let Some(close) = full[open + 1..].find(')') else {
        return full;
    };
    let inner = full[open + 1..open + 1 + close].trim();
    if inner.is_empty() {
        full
    } else {
        inner
    }
}

/// Which of the three rules found the output device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputMatch {
    /// The configured name matched an endpoint outright.
    Exact,
    /// The bracketed device inside the configured name matched. This is the one
    /// that makes "same device as the input" work on Windows.
    SameDevice,
    /// Neither did; fell back to a plain "USB Audio CODEC" at the same position.
    Codec,
}

struct OutputChoice {
    index: usize,
    step: OutputMatch,
}

/// Pick the output endpoint for a configured pattern, out of the names the host
/// offers. Pure, so the ORDER of the rules can be tested without a sound card -
/// and the order is where this went wrong.
///
/// Three rules, most specific first:
///
/// 1. the configured name, as given;
/// 2. the device inside its brackets - "Microfoon (2- USB Audio Device)" is the
///    capture end of the same box as "Speakers (2- USB Audio Device)";
/// 3. a plain "USB Audio CODEC" at the same position, the long-standing
///    fallback that keeps a single-radio FT-991A setup working.
///
/// Rule 2 is new. Without it "use the same device for output as for input"
/// could only ever work on a device that names both ends identically; on
/// everything Windows names by role it silently found nothing, and the operator
/// got no modulation while the right endpoint sat in the list (2026-08-20).
fn choose_output(names: &[String], pat_name: &str, pos: usize) -> Option<OutputChoice> {
    let nth = |needle: &str| -> Option<usize> {
        let needle = needle.to_lowercase();
        if needle.is_empty() {
            return None;
        }
        names
            .iter()
            .enumerate()
            .filter(|(_, n)| n.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .nth(pos.saturating_sub(1))
    };
    if let Some(index) = nth(pat_name) {
        return Some(OutputChoice { index, step: OutputMatch::Exact });
    }
    let core = device_core_name(pat_name);
    if !core.eq_ignore_ascii_case(pat_name) {
        if let Some(index) = nth(core) {
            return Some(OutputChoice { index, step: OutputMatch::SameDevice });
        }
    }
    nth("usb audio codec").map(|index| OutputChoice { index, step: OutputMatch::Codec })
}

/// Build a cpal input capture stream that feeds into an existing tokio sender.
pub(super) fn build_capture_stream(
    device_pattern: &str,
    tx: tokio::sync::mpsc::Sender<Vec<f32>>,
    last_audio_time: Arc<std::sync::atomic::AtomicU64>,
    prefix: &str,
    // Dual-RX channel choice (FTX-1): 0 = L, 1 = R, 2 = mix (average). L and R
    // carry the radio's two receivers separately - heard on the radio with both
    // switched on, 2026-08-20. Mono devices ignore this (the downmix branch
    // does not run).
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
    // Per-radio output device. When two radios both report as
    // "USB Audio CODEC" (edge-case 6) the TX path MUST match the output device
    // that belongs to THIS radio - otherwise radio-1's TX audio goes to
    // radio-0's codec; the device name must stay per slot. The #N position is
    // what tells two identically named devices apart.
    let devices: Vec<cpal::Device> = host
        .output_devices()
        .map_err(|e| format!("enumerate output devices: {}", e))?
        .collect();
    let names: Vec<String> = devices
        .iter()
        .map(|d| d.name().unwrap_or_default())
        .collect();

    let choice = choose_output(&names, &pat_name, pos);
    if let Some(step) = choice.as_ref().map(|c| c.step) {
        if step != OutputMatch::Exact {
            log::debug!(
                "{} output '{}' #{} matched by {:?}: {}",
                prefix, pat_name, pos, step,
                choice.as_ref().map(|c| names[c.index].as_str()).unwrap_or("")
            );
        }
    }
    let device = match choice {
        Some(c) => devices.into_iter().nth(c.index).expect("index from this list"),
        None => {
            return Err(format!("no output device matching '{}' (#{})", pat_name, pos));
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

#[cfg(test)]
mod device_name_tests {
    use super::device_core_name;

    /// The case the operator hit: Windows names the two ends of one USB device
    /// after their role, in the operator's own language, and only the bracketed
    /// part is the device. "Same device as the input" has to survive that.
    #[test]
    fn the_device_is_what_is_inside_the_brackets() {
        assert_eq!(device_core_name("Microfoon (2- USB Audio Device)"), "2- USB Audio Device");
        assert_eq!(device_core_name("Speakers (2- USB Audio Device)"), "2- USB Audio Device");
        assert_eq!(device_core_name("Microphone (3- BEHRINGER UMC202HD)"), "3- BEHRINGER UMC202HD");
        // Both ends reduce to the same device - that is the whole point.
        assert_eq!(
            device_core_name("Microfoon (2- USB Audio Device)"),
            device_core_name("Speakers (2- USB Audio Device)")
        );
    }

    /// A name without brackets is already the device. The FT-991A presents both
    /// ends as plain "USB Audio CODEC", which is the case that always worked and
    /// must keep working untouched.
    #[test]
    fn a_plain_name_is_left_alone() {
        assert_eq!(device_core_name("USB Audio CODEC"), "USB Audio CODEC");
        assert_eq!(device_core_name(""), "");
        assert_eq!(device_core_name("Weird ("), "Weird (");
        assert_eq!(device_core_name("Empty ()"), "Empty ()");
    }
}

#[cfg(test)]
mod output_choice_tests {
    use super::{choose_output, OutputMatch};

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// The operator's case, and the one that was broken: "same device for
    /// output as for input" hands this function the CAPTURE name. No playback
    /// endpoint carries it, and the device is only findable through the part in
    /// brackets.
    #[test]
    fn the_capture_name_finds_the_playback_end_of_the_same_box() {
        let out = names(&["Speakers (2- USB Audio Device)", "Speakers (Realtek)"]);
        let c = choose_output(&out, "Microfoon (2- USB Audio Device)", 1).expect("found");
        assert_eq!(c.index, 0);
        assert_eq!(c.step, OutputMatch::SameDevice);
    }

    /// An explicitly configured output still wins outright - the new rule may
    /// never step in front of what the operator picked from the list.
    #[test]
    fn an_explicit_choice_is_taken_as_given() {
        let out = names(&["Speakers (Realtek)", "Speakers (2- USB Audio Device)"]);
        let c = choose_output(&out, "Speakers (2- USB Audio Device)", 1).expect("found");
        assert_eq!(c.index, 1);
        assert_eq!(c.step, OutputMatch::Exact);
    }

    /// The FT-991A names both ends "USB Audio CODEC" and has always matched on
    /// the first rule. That must not change.
    #[test]
    fn the_codec_case_still_matches_first_time() {
        let out = names(&["USB Audio CODEC", "Speakers (Realtek)"]);
        let c = choose_output(&out, "USB Audio CODEC", 1).expect("found");
        assert_eq!(c.index, 0);
        assert_eq!(c.step, OutputMatch::Exact);
    }

    /// Two identical codecs, one per radio: the position is what keeps radio 2's
    /// transmit audio out of radio 1's codec.
    #[test]
    fn the_position_still_tells_two_identical_codecs_apart() {
        let out = names(&["USB Audio CODEC", "USB Audio CODEC"]);
        assert_eq!(choose_output(&out, "USB Audio CODEC", 1).unwrap().index, 0);
        assert_eq!(choose_output(&out, "USB Audio CODEC", 2).unwrap().index, 1);
        assert!(choose_output(&out, "USB Audio CODEC", 3).is_none());
    }

    /// Last resort, unchanged: an unknown name falls back to a plain codec.
    #[test]
    fn an_unknown_name_falls_back_to_the_codec() {
        let out = names(&["USB Audio CODEC"]);
        let c = choose_output(&out, "Something Else", 1).expect("found");
        assert_eq!(c.step, OutputMatch::Codec);
        assert!(choose_output(&names(&["Speakers (Realtek)"]), "Something Else", 1).is_none());
    }
}
