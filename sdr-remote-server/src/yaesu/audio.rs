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

/// An empty device list in the platform's own words, and its usual cause: one
/// source for the startup listing (`log_input_devices`) and every lookup that
/// finds nothing (`device_not_found`).
#[cfg(windows)]
pub(super) const NO_DEVICES: &str = "Windows hands out an empty list";
#[cfg(windows)]
pub(super) const NO_DEVICES_CAUSE: &str = "The usual cause is a Remote Desktop session: while one is connected it \
    takes the machine's sound cards out of the session. Disconnect it, or set its audio to play on the remote \
    computer, and then restart the server: the device list is only read at startup";
#[cfg(not(windows))]
pub(super) const NO_DEVICES: &str = "the sound system hands out an empty list";
#[cfg(not(windows))]
pub(super) const NO_DEVICES_CAUSE: &str = "Check that the sound cards are connected and that this user may use \
    them (`aplay -l` lists them), and then restart the server: the device list is only read at startup";

/// Why a device could not be found - said so that the reader can act on it.
///
/// An empty list and a name that does not match are two different faults with
/// the same old wording. On 2026-09-03 a station lost its transmit audio for an
/// evening: Windows handed out no sound cards at all, and the server said four
/// times per five seconds that it could not find one particular device. The
/// sentence that mattered - there are none - was logged once at startup and
/// scrolled away. So say it in the warning itself, and name the cause, because
/// there is one ordinary cause and the operator cannot guess it.
pub(super) fn device_not_found(kind: &str, wanted: &str, pos: usize, available: &[String]) -> String {
    if available.is_empty() {
        format!("no {kind} devices AT ALL: {NO_DEVICES}, so '{wanted}' was never looked for. {NO_DEVICES_CAUSE}")
    } else {
        format!(
            "no {kind} device matching '{wanted}' (#{pos}) among the {n} present: {available:?}",
            n = available.len()
        )
    }
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
    // Collect first, so a failure can say what WAS there. Picking straight out
    // of the iterator consumed the only list we had, and then the message could
    // only repeat the name we asked for.
    let devices: Vec<_> = host.input_devices()
        .map_err(|e| format!("enumerate input devices: {}", e))?
        .collect();
    let names: Vec<String> = devices.iter().map(|d| d.name().unwrap_or_default()).collect();
    let idx = names.iter()
        .enumerate()
        .filter(|(_, n)| n.to_lowercase().contains(&pat))
        .map(|(i, _)| i)
        .nth(pos - 1)
        .ok_or_else(|| device_not_found("input", &pat_name, pos, &names))?;
    let device = devices.into_iter().nth(idx).expect("index from this list");

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

    // In the device's own sample format (16-bit on a USB codec under ALSA); f32 here.
    let stream = sdr_remote_core::audio_stream::build_input_stream_f32(
        &device,
        &config,
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            // Latency trade-off (since v2.0.0):
            // Per-callback Vec allocation is deliberately accepted on this server-side
            // Yaesu RX path. Its latency cost is weighed against the cost of avoiding
            // it, and it stays because:
            //   (a) the server runs on the Thetis PC, no local real-time audio output;
            //       audio latency is overshadowed by encode + network path
            //   (b) alloc cost ~50µs is <0.5% of the ~10ms frame budget - not
            //       audible under normal load
            //   (c) Vec::with_capacity(frames) prevents grow-realloc with a stable
            //       input config
            //   (d) tokio::mpsc::Sender consumes the Vec (ownership move); zero
            //       alloc requires a Vec pool with a return channel - a non-trivial
            //       refactor.
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
        move |err| { log::warn!("{} audio capture error: {} (further errors of this stream are not logged)", prefix_err, err); },
    ).map_err(|e| format!("build input stream: {}", e))?;

    stream.play().map_err(|e| format!("start capture: {}", e))?;
    info!("{} audio capture started", prefix);

    Ok((stream, sample_rate))
}

/// Build a cpal output playback stream with a swappable ring buffer producer.
pub(super) fn build_output_stream(
    device_pattern: &str,
    producer_handle: Arc<Mutex<Option<ringbuf::HeapProd<f32>>>>,
    gaps: super::tx_timing::OutputGaps,
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
            return Err(device_not_found("output", &pat_name, pos, &names));
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

    // A zero the ring could not give, in the middle of an over, is counted
    // (build 263; until then it was silence nobody saw).
    let mut watch = super::tx_timing::GapWatch::default();
    // In the device's own sample format, as the capture above.
    let stream = sdr_remote_core::audio_stream::build_output_stream_f32(
        &device,
        &config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            use ringbuf::traits::{Consumer as _, Observer as _};
            let (keyed, over) = (gaps.keyed(), gaps.over());
            for sample in data.iter_mut() {
                let popped = consumer.try_pop();
                *sample = popped.unwrap_or(0.0);
                if let Some(n) = watch.sample(popped.is_some(), keyed, over) {
                    gaps.add(n / channels.max(1) as u64);
                }
            }
            // What is left, for the lead's rebuild in the radio's task (build 278).
            gaps.set_ring_ms(super::tx_timing::queued_ms(consumer.occupied_len(), consumer.capacity().get()));
        },
        move |err| { log::warn!("{} audio output error: {} (further errors of this stream are not logged)", prefix_err, err); },
    ).map_err(|e| format!("build output stream: {}", e))?;

    stream.play().map_err(|e| format!("start playback: {}", e))?;
    // Only now the bridge thread writes to the new ring: published before the
    // stream stood, a build or start that failed left the stream that is still
    // open reading a ring nobody wrote to any more - silent transmit audio.
    *producer_handle.lock().unwrap() = Some(producer);
    info!("{} audio output started", prefix);

    Ok((stream, sample_rate))
}

/// The rate a radio's sound card opened at, one per direction. The holder of
/// the open stream sets it (`StreamHolder::opened`); what converts to or from
/// the card reads it and rebuilds when it changes (`RateFollower`). A radio
/// switched on after the server started opens at the card's own rate (44100 Hz
/// on a USB codec under ALSA), and the loops converted on at the 48000 they
/// started with: one tick in twelve empty, a steady hiccup.
#[derive(Clone, Debug)]
pub struct OpenRate(Arc<std::sync::atomic::AtomicU32>);

impl OpenRate {
    /// Until a stream opens: the rate the loops start at.
    pub fn new(hz: u32) -> Self {
        Self(Arc::new(std::sync::atomic::AtomicU32::new(hz)))
    }

    pub fn get(&self) -> u32 {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn set(&self, hz: u32) {
        self.0.store(hz, std::sync::atomic::Ordering::Relaxed);
    }
}

/// What a converter remembers of the rate it was built for. A rate counts as
/// followed only once the converter for it stands (`built`): a rebuild that
/// failed is tried again on the next block, and logged once (`failed`).
#[derive(Debug)]
pub struct RateFollower {
    built_for: u32,
    failed_for: u32,
}

impl RateFollower {
    pub fn new(hz: u32) -> Self {
        Self { built_for: hz, failed_for: 0 }
    }

    /// The rate the card opened at, while the converter is built for another;
    /// the converter is then rebuilt for it.
    pub fn changed(&self, open: u32) -> Option<u32> {
        if open == self.built_for || open == 0 {
            return None;
        }
        Some(open)
    }

    /// The converter for `hz` stands.
    pub fn built(&mut self, hz: u32) {
        self.built_for = hz;
        self.failed_for = 0;
    }

    /// A rebuild for `hz` failed; true the first time for this rate, so the
    /// line is logged once while the rebuild is tried on every block.
    pub fn failed(&mut self, hz: u32) -> bool {
        let first = self.failed_for != hz;
        self.failed_for = hz;
        first
    }
}

/// Thread-safe holder for a cpal::Stream that can be swapped on reconnect,
/// with the rate it opened at: a stream goes in only with its rate, so no
/// opener can leave the rate behind (five of them threw it away,
/// `Ok((stream, _rate))`, and the two at the start kept it in a local that
/// nothing updated afterwards).
pub(super) struct StreamHolder {
    stream: Mutex<Option<cpal::Stream>>,
    rate: OpenRate,
}
// SAFETY: cpal::Stream on Windows (WASAPI) uses COM handles safe to move between threads.
unsafe impl Send for StreamHolder {}
unsafe impl Sync for StreamHolder {}

impl StreamHolder {
    pub(super) fn new(rate: OpenRate) -> Self {
        Self { stream: Mutex::new(None), rate }
    }
    /// A stream that plays, and the rate it plays at. A rebuild that failed
    /// never gets here, so the stream that stays keeps its rate.
    pub(super) fn opened(&self, stream: cpal::Stream, hz: u32) {
        *self.stream.lock().unwrap() = Some(stream);
        self.rate.set(hz);
    }
    pub(super) fn close(&self) {
        *self.stream.lock().unwrap() = None;
    }
    pub(super) fn is_set(&self) -> bool {
        self.stream.lock().unwrap().is_some()
    }
    pub(super) fn rate(&self) -> OpenRate {
        self.rate.clone()
    }
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
/// picker.
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


#[cfg(test)]
mod device_message_tests {
    use super::{device_not_found, OpenRate, RateFollower, StreamHolder, NO_DEVICES, NO_DEVICES_CAUSE};

    /// The evening this was written for: nothing to choose from.
    #[test]
    fn an_empty_list_says_so_and_names_the_cause() {
        let msg = device_not_found("output", "Luidsprekers (USB Audio CODEC )", 1, &[]);
        assert!(msg.contains("AT ALL"), "{msg}");
        assert!(msg.contains(NO_DEVICES) && msg.contains(NO_DEVICES_CAUSE), "{msg}");
        #[cfg(windows)]
        assert!(msg.contains("Remote Desktop"), "{msg}");
        #[cfg(not(windows))]
        assert!(!msg.contains("Windows") && !msg.contains("Remote Desktop"), "{msg}");
        assert!(msg.contains("restart the server"), "{msg}");
    }

    /// The other fault, which the old wording could not be told apart from:
    /// devices exist, the name has shifted. Then the candidates are the answer.
    #[test]
    fn a_shifted_name_shows_what_was_there() {
        let have = vec![
            "Microfoon (2- USB Audio CODEC )".to_string(),
            "Microfoon (3- RODE NT-USB)".to_string(),
        ];
        let msg = device_not_found("input", "Microfoon (USB Audio CODEC )", 1, &have);
        assert!(!msg.contains("AT ALL"), "{msg}");
        assert!(msg.contains("2 present"), "{msg}");
        assert!(msg.contains("2- USB Audio CODEC"), "{msg}");
    }

    /// The transmit ring goes to the bridge only once the new stream plays: a
    /// reopen that fails keeps the stream that is open, writing into its ring.
    #[test]
    fn the_transmit_ring_is_published_after_the_stream_plays() {
        let src = sdr_remote_core::source_text::production_code(include_str!("audio.rs"));
        let body = &src[src.find("pub(super) fn build_output_stream(").unwrap()..];
        let played = body.find("stream.play().map_err(").unwrap();
        let published = body.find("*producer_handle.lock().unwrap() = Some(producer);").unwrap();
        assert!(played < published, "published after the stream plays");
        assert_eq!(src.matches("*producer_handle.lock().unwrap() = Some(producer);").count(), 1);
    }

    /// A converter follows the rate the card opened at: until the converter
    /// for it stands, the second change too, and nothing for the same rate or
    /// for no rate. A rebuild that failed is asked for again, logged once.
    #[test]
    fn a_converter_follows_the_rate_the_card_opened_at() {
        let rate = OpenRate::new(48_000);
        let mut follow = RateFollower::new(rate.get());
        assert_eq!(follow.changed(rate.get()), None, "built for it");
        for pass in 0..2 {
            rate.set(44_100);
            assert_eq!(follow.changed(rate.get()), Some(44_100), "pass {pass}: the radio switched on later");
            assert!(follow.failed(44_100), "pass {pass}: a failed rebuild is logged");
            assert_eq!(follow.changed(rate.get()), Some(44_100), "pass {pass}: and asked for again");
            assert!(!follow.failed(44_100), "pass {pass}: logged once");
            follow.built(44_100);
            assert_eq!(follow.changed(rate.get()), None, "pass {pass}: built, so once");
            rate.set(48_000);
            assert_eq!(follow.changed(rate.get()), Some(48_000), "pass {pass}: and back");
            follow.built(48_000);
            assert_eq!(follow.changed(rate.get()), None, "pass {pass}: built again");
        }
        assert_eq!(follow.changed(0), None, "no rate is no change");
        // The holder keeps the rate of the stream it holds: a rebuild that
        // failed leaves both as they were.
        let holder = StreamHolder::new(OpenRate::new(48_000));
        let shared = holder.rate();
        holder.rate.set(44_100);
        assert_eq!(shared.get(), 44_100, "every reader sees the holder's rate");
        holder.close();
        assert!(!holder.is_set());
        assert_eq!(shared.get(), 44_100, "closing does not invent a rate");
    }

    /// Every opener hands its stream and its rate to the holder in one call,
    /// and every converter of a radio's audio follows the rate.
    #[test]
    fn every_opener_and_every_converter_uses_the_open_rate() {
        let code = |s: &str| sdr_remote_core::source_text::production_code(s).to_string();
        let (m, poll) = (code(include_str!("mod.rs")), code(include_str!("poll.rs")));
        let audio = code(include_str!("audio.rs"));
        let opened = "Ok((stream, rate)) => capture_stream.opened(stream, rate)";
        assert_eq!(m.matches(opened).count(), 1);
        assert_eq!(m.matches("Ok((stream, rate)) => output_stream.opened(stream, rate)").count(), 1);
        assert_eq!(poll.matches("capture_stream.opened(stream, rate);").count(), 2, "reconnect and watchdog");
        assert_eq!(poll.matches("output_stream.opened(stream, rate);").count(), 3, "reconnect, watchdog, recovery");
        let builders = [&m, &poll].iter().map(|t| t.matches("build_capture_stream(").count() + t.matches("build_output_stream(").count()).sum::<usize>();
        assert_eq!(builders, 7, "each opener above, and no other");
        for t in [&m, &poll] {
            assert!(!t.contains("_rate)) =>"), "a rate thrown away");
        }
        assert_eq!(audio.matches("self.rate.set(hz);").count(), 1, "only an open stream sets it");
        // Both builders hand on the rate the card itself opened at.
        assert_eq!(audio.matches("let sample_rate = config.sample_rate().0;").count(), 2, "both builders take the card's rate");
        assert_eq!(audio.matches("Ok((stream, sample_rate))").count(), 2, "and hand that rate on");
        // Which rate goes where: the capture holder's to the receive loop, the
        // output holder's to the transmit task and the bridge.
        assert_eq!(m.matches("capture_rate: capture_stream.rate(),").count(), 1);
        assert_eq!(m.matches("output_rate: output_stream.rate(),").count(), 1);
        assert_eq!(m.matches("let output_rate = output_stream.rate();").count(), 1);
        // Code only, whitespace folded: a comment line in between changes nothing.
        let squash = |t: &str| t.lines().map(str::trim).filter(|l| !l.starts_with("//")).collect::<Vec<_>>().join(" ")
            .split_whitespace().collect::<Vec<_>>().join(" ");
        // The bridge reads it per block, and its hold limit from that read.
        let bridge = &m[m.find("while let Some(samples) = rx.recv().await {").unwrap()..];
        let bridge = squash(&bridge[..bridge.find("let taken = hold.take(").unwrap()]);
        assert_eq!(bridge, "while let Some(samples) = rx.recv().await { let tx_rate = output_rate.get(); \
            let hold_max = (tx_rate as u64 * tx_timing::HOLD_MAX_MS / 1000) as usize;", "per block, before it is used");
        // The receive loop: the start and a rebuild both build everything that
        // hangs on the rate in one value, and a rebuild comes before the block
        // goes in and counts only once it stands.
        let rx = code(include_str!("../audio_loops.rs"));
        let at_rate = &rx[rx.find("impl RxAtRate {").unwrap()..];
        let at_rate = squash(&at_rate[..at_rate.find("fn line(").unwrap()]);
        assert!(at_rate.contains("let frame = (hz * 20 / 1000) as usize; Ok(Self { hz, buf: RadioBuffer::new(frame), \
            nb: resampler(hz, NETWORK_SAMPLE_RATE, frame, UpFilter::Long)"), "{at_rate}");
        assert!(at_rate.contains("wb: resampler(hz, NETWORK_SAMPLE_RATE_WIDEBAND, frame, UpFilter::Long)"), "{at_rate}");
        assert_eq!(rx.matches("RxAtRate::new(").count(), 2, "the start and the rebuild");
        assert_eq!(rx.matches("let mut rx = RxAtRate::new(sample_rate)?;").count(), 1);
        // Both arms, to their end: the line from the builder, and a failed
        // rebuild that does not count as followed and is logged once.
        assert!(squash(&rx).contains("if let Some(hz) = follow.changed(capture_rate.get()) { match RxAtRate::new(hz) { \
            Ok(built) => { rx = built; follow.built(hz); \
            info!(\"Yaesu slot {} RX resamplers: {}; the sound card opened at {} Hz\", slot, rx.line(), hz); } \
            Err(e) => { if follow.failed(hz) { \
            log::error!(\"Yaesu slot {} RX resamplers for {} Hz: {:#}; the old ones stay, tried again on every block\", slot, hz, e); } } } }"),
            "the receive rebuild, whole");
        assert_eq!(rx.matches("info!(\"Yaesu slot {} RX resamplers: {}\", slot, rx.line());").count(), 1, "the start line, from the builder");
        assert!(rx.find("if let Some(hz) = follow.changed(capture_rate.get()) {").unwrap()
            < rx.find("rx.buf.push(&samples);").unwrap(), "rebuilt before the block goes in");
        assert_eq!(rx.matches("rx.buf.push(&samples);").count(), 1);
        // The transmit task: the same for its one resampler.
        let tx = code(include_str!("../tx_recover.rs"));
        assert!(squash(&tx).contains("fn to_card(hz: u32) -> anyhow::Result<Resampler> { \
            resampler(NETWORK_SAMPLE_RATE_WIDEBAND, hz, FRAME_SAMPLES_WIDEBAND, UpFilter::Long) }"));
        assert_eq!(tx.matches("to_card(").count(), 3, "defined, the start and the rebuild");
        assert_eq!(tx.matches("let mut to_radio = match to_card(tx_rate.get()) {").count(), 1);
        assert!(squash(&tx).contains("if let Some(hz) = follow.changed(tx_rate.get()) { match to_card(hz) { \
            Ok(r) => { to_radio = r; follow.built(hz); \
            info!(\"{} TX resampler: the sound card plays at {} Hz now; {}\", label, hz, to_radio.built()); } \
            Err(e) => { if follow.failed(hz) { \
            log::error!(\"{} TX resampler for {} Hz: {:#}; the old one stays, tried again on every tick\", label, hz, e); } } } }"),
            "the transmit rebuild, whole");
        assert!(squash(&tx).contains("\"{} TX task started (wideband Opus, jitter buffer as Thetis); {}\", label, to_radio.built() );"),
            "the start line, from the resampler");
        let net = code(include_str!("../network.rs"));
        assert_eq!(net.matches("Some((rx, y.capture_rate.clone(), y.status_arc()))").count(), 2, "both radios");
        assert_eq!(net.matches("map(|y| y.output_rate.clone());").count(), 2, "both radios");
        // Each radio's transmit task on its own card: the two blocks are a
        // mirror, and one letter decides which card a task follows.
        let net = squash(&net);
        for radio in ["yaesu", "yaesu2"] {
            let block = format!("let tx_audio_tx = {radio}.as_ref().and_then(|y| y.tx_audio_tx.clone()); \
                let tx_rate = {radio}.as_ref().map(|y| y.output_rate.clone()); \
                let timing = {radio}.as_ref().map(|y| y.status_arc().lock().unwrap().tx_timing.clone());");
            assert_eq!(net.matches(&block).count(), 1, "{radio}: its own card");
        }
        // And each radio's receive loop on its own capture and its own slot.
        for (radio, slot) in [("yaesu", 0), ("yaesu2", 1)] {
            let block = format!("let {radio}_audio = {radio}.as_ref().and_then(|y| {{ \
                let rx = y.audio_rx.lock().ok().and_then(|mut a| a.take())?; \
                Some((rx, y.capture_rate.clone(), y.status_arc())) }});");
            assert_eq!(net.matches(&block).count(), 1, "{radio}: its own capture");
            let call = format!("start, audio_stats, server_start, {slot}, {radio}_status)");
            assert_eq!(net.matches(&call).count(), 1, "{radio}: its own slot");
        }
    }
}
