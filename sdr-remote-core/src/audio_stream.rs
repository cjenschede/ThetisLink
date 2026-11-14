// SPDX-License-Identifier: GPL-2.0-or-later
//! A cpal stream in whatever sample format the device asks for, with the
//! caller always in f32. Windows hands out f32; ALSA on Linux often 16-bit
//! integers (a USB audio codec), and a stream opened as f32 on such a device
//! fails with "set_format ... Invalid argument". One place for the server's
//! radio audio and the desktop client: convert inside the callback, through
//! one scratch buffer per stream and no queue, so no extra latency. The F32
//! arm hands the callback to cpal as it is (no clipping there).
//!
//! Behind the `audio-stream` feature: `cargo test -p sdr-remote-core` alone
//! does not build these tests; the workspace run does (the server and the
//! client turn the feature on), or `--features audio-stream`.

use cpal::traits::DeviceTrait;
use cpal::{FromSample, Sample, SampleFormat, SizedSample};

/// Open an input stream in the device's own format; `on_data` gets f32.
pub fn build_input_stream_f32<D, E>(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    on_data: D,
    on_error: E,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    D: FnMut(&[f32], &cpal::InputCallbackInfo) + Send + 'static,
    E: FnMut(cpal::StreamError) + Send + 'static,
{
    let stream_config = config.config();
    let on_error = first_error_only(on_error);
    match config.sample_format() {
        SampleFormat::F32 => device.build_input_stream(&stream_config, on_data, on_error, None),
        SampleFormat::F64 => input::<f64, D, _>(device, &stream_config, on_data, on_error),
        SampleFormat::I8 => input::<i8, D, _>(device, &stream_config, on_data, on_error),
        SampleFormat::I16 => input::<i16, D, _>(device, &stream_config, on_data, on_error),
        SampleFormat::I32 => input::<i32, D, _>(device, &stream_config, on_data, on_error),
        SampleFormat::I64 => input::<i64, D, _>(device, &stream_config, on_data, on_error),
        SampleFormat::U8 => input::<u8, D, _>(device, &stream_config, on_data, on_error),
        SampleFormat::U16 => input::<u16, D, _>(device, &stream_config, on_data, on_error),
        SampleFormat::U32 => input::<u32, D, _>(device, &stream_config, on_data, on_error),
        SampleFormat::U64 => input::<u64, D, _>(device, &stream_config, on_data, on_error),
        _ => Err(cpal::BuildStreamError::StreamConfigNotSupported),
    }
}

/// Open an output stream in the device's own format; `fill` writes f32.
pub fn build_output_stream_f32<D, E>(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    fill: D,
    on_error: E,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    D: FnMut(&mut [f32], &cpal::OutputCallbackInfo) + Send + 'static,
    E: FnMut(cpal::StreamError) + Send + 'static,
{
    let stream_config = config.config();
    let on_error = first_error_only(on_error);
    match config.sample_format() {
        SampleFormat::F32 => device.build_output_stream(&stream_config, fill, on_error, None),
        SampleFormat::F64 => output::<f64, D, _>(device, &stream_config, fill, on_error),
        SampleFormat::I8 => output::<i8, D, _>(device, &stream_config, fill, on_error),
        SampleFormat::I16 => output::<i16, D, _>(device, &stream_config, fill, on_error),
        SampleFormat::I32 => output::<i32, D, _>(device, &stream_config, fill, on_error),
        SampleFormat::I64 => output::<i64, D, _>(device, &stream_config, fill, on_error),
        SampleFormat::U8 => output::<u8, D, _>(device, &stream_config, fill, on_error),
        SampleFormat::U16 => output::<u16, D, _>(device, &stream_config, fill, on_error),
        SampleFormat::U32 => output::<u32, D, _>(device, &stream_config, fill, on_error),
        SampleFormat::U64 => output::<u64, D, _>(device, &stream_config, fill, on_error),
        _ => Err(cpal::BuildStreamError::StreamConfigNotSupported),
    }
}

/// Only the first error of a stream reaches `on_error`. A device that goes
/// away (a radio switched off, a USB cable out) makes ALSA report the same
/// error from every poll, thousands within a millisecond, until the stream is
/// closed (2971 lines in one field log); the first says what happened, the
/// rest say nothing new. Every caller acts on the first (a log line, a flag
/// the recovery reads), and a new stream starts with a clean slate.
fn first_error_only<E>(mut on_error: E) -> impl FnMut(cpal::StreamError) + Send + 'static
where
    E: FnMut(cpal::StreamError) + Send + 'static,
{
    let mut reported = false;
    move |err| {
        if !reported {
            reported = true;
            on_error(err);
        }
    }
}

fn input<T, D, E>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut on_data: D,
    on_error: E,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample,
    f32: FromSample<T>,
    D: FnMut(&[f32], &cpal::InputCallbackInfo) + Send + 'static,
    E: FnMut(cpal::StreamError) + Send + 'static,
{
    // Grows to the callback size once, then keeps its capacity.
    let mut scratch: Vec<f32> = Vec::new();
    device.build_input_stream(
        config,
        move |data: &[T], info: &cpal::InputCallbackInfo| {
            to_f32(data, &mut scratch);
            on_data(&scratch, info);
        },
        on_error,
        None,
    )
}

fn output<T, D, E>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut fill: D,
    on_error: E,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + FromSample<f32>,
    D: FnMut(&mut [f32], &cpal::OutputCallbackInfo) + Send + 'static,
    E: FnMut(cpal::StreamError) + Send + 'static,
{
    let mut scratch: Vec<f32> = Vec::new();
    device.build_output_stream(
        config,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            fit(&mut scratch, data.len());
            fill(&mut scratch, info);
            from_f32(&scratch, data);
        },
        on_error,
        None,
    )
}

/// The scratch at exactly the callback's length and silent: a `fill` that
/// writes nothing gives the device zeros, not the previous callback's audio,
/// and `from_f32` (which zips) covers the whole device buffer.
fn fit(scratch: &mut Vec<f32>, n: usize) {
    scratch.clear();
    scratch.resize(n, 0.0);
}

/// Device samples to f32 in -1..1, into `dst` (cleared first).
pub fn to_f32<T: Sample>(src: &[T], dst: &mut Vec<f32>)
where
    f32: FromSample<T>,
{
    dst.clear();
    dst.extend(src.iter().map(|&s| s.to_sample::<f32>()));
}

/// f32 to device samples; out of -1..1 is clipped, not wrapped.
pub fn from_f32<T: Sample + FromSample<f32>>(src: &[f32], dst: &mut [T]) {
    for (out, &s) in dst.iter_mut().zip(src) {
        *out = T::from_sample(s.clamp(-1.0, 1.0));
    }
}

#[cfg(test)]
mod tests {
    use super::{from_f32, to_f32};

    /// 16-bit, the format of a USB audio codec on ALSA: full scale both ways,
    /// silence stays silence, and an overdriven sample clips instead of wrapping.
    #[test]
    fn sixteen_bit_converts_both_ways_and_clips() {
        let mut out = [0i16; 6];
        from_f32(&[0.0, 1.0, -1.0, 0.5, 2.0, -3.0], &mut out);
        assert_eq!(out[0], 0);
        assert_eq!(out[1], i16::MAX);
        assert_eq!(out[2], i16::MIN);
        assert!((out[3] as i32 - 16384).abs() <= 1, "{}", out[3]);
        assert_eq!((out[4], out[5]), (i16::MAX, i16::MIN), "clipped, not wrapped");
        let mut back = Vec::new();
        to_f32(&out[..4], &mut back);
        assert_eq!(back[0], 0.0);
        assert!((back[1] - 1.0).abs() < 1e-4 && (back[2] + 1.0).abs() < 1e-4 && (back[3] - 0.5).abs() < 1e-3, "{back:?}");
        // Twice into the same buffer: cleared, not appended.
        to_f32(&out[..2], &mut back);
        assert_eq!(back.len(), 2);
    }

    /// The other shapes a device can ask for: unsigned with its midpoint as
    /// silence, 32-bit, and f32 itself unchanged.
    #[test]
    fn other_formats_keep_silence_and_full_scale() {
        let mut u = [0u16; 3];
        from_f32(&[0.0, 1.0, -1.0], &mut u);
        assert_eq!(u[0], 32768, "unsigned silence is the midpoint");
        assert_eq!((u[1], u[2]), (u16::MAX, 0));
        let mut back = Vec::new();
        to_f32(&u, &mut back);
        assert!(back[0].abs() < 1e-4 && (back[1] - 1.0).abs() < 1e-4 && (back[2] + 1.0).abs() < 1e-4, "{back:?}");
        let mut w = [0i32; 2];
        from_f32(&[0.25, -0.25], &mut w);
        to_f32(&w, &mut back);
        assert!((back[0] - 0.25).abs() < 1e-6 && (back[1] + 0.25).abs() < 1e-6, "{back:?}");
        let mut f = [0f32; 2];
        from_f32(&[0.3, -0.7], &mut f);
        assert_eq!(f, [0.3, -0.7]);
        // Overdriven, in every integer shape: clipped to full scale, never wrapped.
        let mut u = [0u16; 2];
        from_f32(&[1.5, -1.5], &mut u);
        assert_eq!(u, [u16::MAX, 0]);
        let mut b = [0u8; 2];
        from_f32(&[1.5, -1.5], &mut b);
        assert_eq!(b, [u8::MAX, 0]);
        let mut c = [0i8; 2];
        from_f32(&[1.5, -1.5], &mut c);
        assert_eq!(c, [i8::MAX, i8::MIN]);
        let mut w = [0i32; 2];
        from_f32(&[1.5, -1.5], &mut w);
        assert_eq!(w, [i32::MAX, i32::MIN]);
        // f64 is the one float that goes through the conversion (the F32 arm
        // goes straight to cpal); the integer shapes saturate on their own.
        let mut f = [0f64; 2];
        from_f32(&[1.5, -1.5], &mut f);
        assert_eq!(f, [1.0, -1.0], "f64 too: the device gets full scale at most");
    }

    /// Every cpal stream of the server and the desktop client goes through this
    /// module: one opened as f32 directly fails on a 16-bit device, silently on
    /// Windows (which hands out f32) and loudly on Linux.
    #[test]
    fn every_stream_opens_in_the_device_format() {
        fn walk(dir: &std::path::Path, out: &mut String) {
            for e in std::fs::read_dir(dir).unwrap() {
                let path = e.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|x| x == "rs") {
                    out.push_str(&crate::source_text::production_code(&std::fs::read_to_string(&path).unwrap()));
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut code = String::new();
        for krate in ["sdr-remote-server", "sdr-remote-client"] {
            walk(&root.join(krate).join("src"), &mut code);
        }
        for form in [".build_input_stream(", "DeviceTrait::build_input_stream", "build_input_stream_raw"] {
            assert_eq!(code.matches(form).count(), 0, "a cpal input stream not through this module: {form}");
        }
        for form in [".build_output_stream(", "DeviceTrait::build_output_stream", "build_output_stream_raw"] {
            assert_eq!(code.matches(form).count(), 0, "a cpal output stream not through this module: {form}");
        }
        assert_eq!(code.matches("audio_stream::build_input_stream_f32(").count(), 2, "the Yaesu capture and the client microphone");
        assert_eq!(code.matches("audio_stream::build_output_stream_f32(").count(), 2, "the Yaesu output and the client playback");
    }

    /// The table itself: every format opens in its own type, f32 goes straight
    /// to cpal, and a format cpal adds later is refused instead of guessed.
    #[test]
    fn every_format_opens_in_its_own_type() {
        let src = crate::source_text::production_code(include_str!("audio_stream.rs"));
        for (variant, ty) in [
            ("F64", "f64"), ("I8", "i8"), ("I16", "i16"), ("I32", "i32"), ("I64", "i64"),
            ("U8", "u8"), ("U16", "u16"), ("U32", "u32"), ("U64", "u64"),
        ] {
            for fun in ["input", "output"] {
                let want = format!("SampleFormat::{variant} => {fun}::<{ty}, D, _>(");
                assert_eq!(src.matches(&want).count(), 1, "{want}");
            }
        }
        assert_eq!(src.matches("SampleFormat::F32 => device.build_input_stream(&stream_config, on_data, on_error, None)").count(), 1);
        assert_eq!(src.matches("SampleFormat::F32 => device.build_output_stream(&stream_config, fill, on_error, None)").count(), 1);
        assert_eq!(src.matches("_ => Err(cpal::BuildStreamError::StreamConfigNotSupported)").count(), 2, "refused, both ways");
        // The two callbacks, in this order: the scratch on the callback's
        // length and silent, filled, then written to the device; the input
        // converted, then handed on. Swapped or one line gone is silence on
        // every card that is not f32 (the Yaesu codecs under ALSA).
        assert_eq!(
            src.matches("fit(&mut scratch, data.len());
            fill(&mut scratch, info);
            from_f32(&scratch, data);").count(),
            1,
            "the output callback, in this order"
        );
        assert_eq!(
            src.matches("to_f32(data, &mut scratch);
            on_data(&scratch, info);").count(),
            1,
            "the input callback, in this order"
        );
    }

    /// The scratch at exactly the callback's length and silent, shorter and longer.
    #[test]
    fn the_scratch_is_the_callback_length_and_starts_silent() {
        let mut s = Vec::new();
        super::fit(&mut s, 4);
        s.copy_from_slice(&[0.5f32; 4]);
        super::fit(&mut s, 2);
        assert_eq!(s, [0.0, 0.0], "shorter: on length, and the old audio gone");
        s.copy_from_slice(&[-0.25f32; 2]);
        super::fit(&mut s, 5);
        assert_eq!(s, [0.0; 5], "longer: on length, and silent");
    }

    /// A stream passes on its first error only, a new stream its own first
    /// again; both openers wrap the caller's handler.
    #[test]
    fn a_stream_reports_its_first_error_only() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let seen = Arc::new(AtomicUsize::new(0));
        for pass in 1..=2 {
            let count = seen.clone();
            let mut on_error = super::first_error_only(move |_| {
                count.fetch_add(1, Ordering::Relaxed);
            });
            for _ in 0..3000 {
                on_error(cpal::StreamError::DeviceNotAvailable);
            }
            assert_eq!(seen.load(Ordering::Relaxed), pass, "pass {pass}: one per stream");
        }
        let code = sdr_remote_core_code();
        assert_eq!(code.matches("let on_error = first_error_only(on_error);").count(), 2, "the input and the output opener");
        assert_eq!(code.matches(", on_error, None)").count(), 2, "the F32 arms take the wrapped handler");
    }

    fn sdr_remote_core_code() -> String {
        crate::source_text::production_code(include_str!("audio_stream.rs")).to_string()
    }
}
