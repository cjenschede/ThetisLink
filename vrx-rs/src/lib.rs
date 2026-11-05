// SPDX-License-Identifier: GPL-2.0-or-later

//! `vrx-rs`: standalone FFT-channelizer + Opus encode pipeline for
//! Virtual RX (VRX) channels in software-defined radio applications.
//!
//! ## What it does
//!
//! Given a wideband I/Q stream from any SDR (Thetis, KiwiSDR,
//! standalone HPSDR, etc.) the crate carves out a narrowband channel
//! at a tunable carrier offset, demodulates as SSB (USB / LSB), and
//! emits 20 ms Opus-encoded audio frames at 8 kHz mono. The DSP cost
//! is amortised across the input batch; adding more VRX channels
//! adds only per-channel iFFTs + Opus encodes, not whole-spectrum
//! FFTs. (See WebSDR / OpenWebRX for the same architectural pattern
//! at a larger scale.)
//!
//! ## Quick example
//!
//! ```no_run
//! use std::sync::{Arc, Mutex};
//! use vrx_rs::{VrxRuntime, VrxRuntimeOptions, VrxAudioCallback, VrxControlState};
//!
//! struct MySink;
//! impl VrxAudioCallback for MySink {
//!     fn on_frame(&mut self, vrx_id: u8, _audio: &[f32], opus: &[u8], seq: u32) {
//!         println!("VRX{} frame seq={} bytes={}", vrx_id, seq, opus.len());
//!     }
//! }
//!
//! let control = Arc::new(Mutex::new(VrxControlState {
//!     enabled: true,
//!     target_freq_hz: 14_255_000,
//!     mode: vrx_rs::VrxMode::Usb,
//!     volume: 1.0,
//!     filter_low_hz: 0,
//!     filter_high_hz: 3000,
//!     sam_auto_tune: false,
//!     agc: vrx_rs::VrxAgcSettings::default(),
//! }));
//! let mut runtime = VrxRuntime::new(VrxRuntimeOptions::default(), control);
//! let mut sink = MySink;
//!
//! // In your IQ-receive loop:
//! let iq_batch: Vec<(f32, f32)> = vec![/* ... */];
//! runtime.feed(384_000, &iq_batch, 14_255_000, 14_255_000, &mut sink);
//! ```
//!
//! ## Design
//!
//! - **Sync API.** `VrxRuntime::feed()` is sync. The host application
//!   chooses its own async runtime (tokio / async-std / smol /
//!   single-threaded). No tokio dependency in this crate.
//! - **Shared control via `Arc<Mutex<VrxControlState>>`.** Lock-windows
//!   are short (read a few fields, no `.await` held); `std::sync::Mutex`
//!   is the right primitive.
//! - **No network / no protocol.** Output is via the `VrxAudioCallback`
//!   trait, the host decides how to ship the Opus frames (UDP, local
//!   playback, file, etc.).
//! - **Rate-adaptive.** `feed()` accepts any input sample rate; the
//!   channelizer rebuilds with matching FFT size automatically.

pub mod agc;
pub mod channelizer;
pub mod config;
pub mod opus;
pub mod runtime;
pub mod wav;

pub use agc::{VrxAgcMode, VrxAgcSettings};
pub use channelizer::{
    fft_n_for_rate, VrxChannelizer, DEFAULT_FFT_N, DEFAULT_INPUT_HOP, IFFT_N,
    OUTPUT_HOP, OUTPUT_RATE_HZ,
};
pub use config::{
    rate_mode_wants_wideband, AUDIO_BIN_HZ, AUTO_WB_THRESHOLD_HZ, VrxConfig, VrxControlState,
    VrxMode, VrxRateMode,
};
pub use opus::{VrxOpusEncoder, FRAME_SAMPLES};
pub use runtime::{VrxAudioCallback, VrxRuntime, VrxRuntimeOptions};
pub use wav::RollingWavWriter;
