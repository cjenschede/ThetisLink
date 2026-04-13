pub mod auth;
pub mod codec;
pub mod jitter;
pub mod protocol;

/// ThetisLink version — shared by server and client
pub const VERSION: &str = "0.7.0";

/// Build number for dev builds — displayed alongside version for testing.
/// Set to None for release builds (only show version).
pub const BUILD: Option<u32> = None;

/// Version string with optional build number: "0.1.5" or "0.1.5 (build 1)"
pub fn version_string() -> String {
    match BUILD {
        Some(b) => format!("{} (build {})", VERSION, b),
        None => VERSION.to_string(),
    }
}

/// Audio sample rate used over the network (Opus narrowband)
pub const NETWORK_SAMPLE_RATE: u32 = 8_000;

/// Audio sample rate for wideband Opus (16kHz)
pub const NETWORK_SAMPLE_RATE_WIDEBAND: u32 = 16_000;

/// Audio sample rate used by audio devices (cpal/WASAPI default)
pub const DEVICE_SAMPLE_RATE: u32 = 48_000;

/// Opus frame duration in milliseconds
pub const FRAME_DURATION_MS: u32 = 20;

/// Samples per Opus frame at network sample rate (8kHz * 20ms = 160)
pub const FRAME_SAMPLES: usize = (NETWORK_SAMPLE_RATE * FRAME_DURATION_MS / 1000) as usize;

/// Samples per Opus frame at wideband rate (16kHz * 20ms = 320)
pub const FRAME_SAMPLES_WIDEBAND: usize =
    (NETWORK_SAMPLE_RATE_WIDEBAND * FRAME_DURATION_MS / 1000) as usize;

/// Samples per frame at device sample rate (48kHz * 20ms = 960)
pub const DEVICE_FRAME_SAMPLES: usize = (DEVICE_SAMPLE_RATE * FRAME_DURATION_MS / 1000) as usize;

/// Default server port
pub const DEFAULT_PORT: u16 = 4580;

/// Maximum UDP packet size (32768 spectrum bins + header + margin)
pub const MAX_PACKET_SIZE: usize = 33_000;

/// Number of spectrum bins sent per frame (legacy, used for wideband)
pub const SPECTRUM_BINS: usize = 1024;

/// Maximum spectrum bins per packet (server-side view extraction)
pub const MAX_SPECTRUM_SEND_BINS: usize = 32_768;

/// Default spectrum bins for new clients (backward compatible)
pub const DEFAULT_SPECTRUM_BINS: usize = 8192;

/// FFT size for DDC I/Q spectrum processing (262144 = Thetis-quality resolution at 1536 kHz)
pub const DDC_FFT_SIZE: usize = 262_144;

/// Compute FFT size for a given DDC sample rate, targeting ~12 FPS with 87.5% overlap.
/// Returns a power-of-two FFT size (minimum 4096).
/// 87.5% overlap gives Thetis-quality frequency resolution (~1.5 Hz/bin at 384kHz).
pub fn ddc_fft_size(sample_rate_hz: u32) -> usize {
    // Hop size = fft_size / 8. For ~12 FPS: fft_size / 8 = sample_rate / 12
    // → fft_size = sample_rate * 8 / 12 = sample_rate * 2 / 3.
    let target = (sample_rate_hz as usize) * 2 / 3;
    target.next_power_of_two().max(4096)
}

/// Hop size for overlap: 1/8 of FFT size (87.5% overlap).
pub fn ddc_hop_size(fft_size: usize) -> usize {
    fft_size / 8
}

/// Number of bins for full DDC waterfall rows (sent alongside extracted view)
pub const FULL_SPECTRUM_BINS: usize = 8192;

/// Default spectrum frame rate
pub const DEFAULT_SPECTRUM_FPS: u8 = 15;

// ── Shared DSP utilities ────────────────────────────────────────────────

/// Convert dBm to S-meter display value (0-260 scale).
/// 0-108 = S0 to S9 (each S-unit = 12 raw units, -121 dBm = S0, -73 dBm = S9).
/// 108-260 = S9+dB (60 dB range above S9).
pub fn dbm_to_display(dbm: f32) -> u16 {
    if dbm <= -73.0 {
        ((dbm + 121.0) * (108.0 / 48.0)).clamp(0.0, 108.0) as u16
    } else {
        (108.0 + (dbm + 73.0) * (152.0 / 60.0)).clamp(108.0, 260.0) as u16
    }
}
