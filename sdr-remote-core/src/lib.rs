// SPDX-License-Identifier: GPL-2.0-or-later

pub mod auth;
pub mod codec;
pub mod jitter;
pub mod protocol;

/// ThetisLink version - shared by server and client
pub const VERSION: &str = "2.7.0";

/// Build number for dev builds - displayed alongside version for testing.
/// Set to None for release builds (only show version).
pub const BUILD: Option<u32> = None;

/// Version string with optional build number: "0.1.5" or "0.1.5 (build 1)"
pub fn version_string() -> String {
    match BUILD {
        Some(b) => format!("{} (build {})", VERSION, b),
        None => VERSION.to_string(),
    }
}

/// Column header of the Yaesu memory blob (tab-separated, one line per channel).
///
/// Single source of truth for a format with three readers: the server writes it,
/// the desktop client parses it, and the Android client parses it. All three
/// look columns up BY NAME - never by position - so adding a column here cannot
/// silently shift what another reader shows. Matches the .tab export of the
/// Yaesu programmer, which is why the names are what they are.
pub const YAESU_MEMORY_TAB_HEADER: &str = "Channel Number	Receive Frequency	Transmit Frequency	Offset Frequency	Offset Direction	Operating Mode	Tx Operating Mode	Name	Tone Mode	CTCSS	DCS	Narrow	Skip	Attenuator	Tuner	AGC	Noise Blanker	IPO	DNR	Step	Comment	";

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

/// Compute FFT size for a given DDC sample rate, targeting ~25 FPS with 87.5% overlap.
/// Returns a power-of-two FFT size (minimum 4096).
/// At 384 kHz: 128K bins -> ~23 FPS, ~3 Hz/bin.
pub fn ddc_fft_size(sample_rate_hz: u32) -> usize {
    // Hop size = fft_size / 8. For ~25 FPS: fft_size / 8 = sample_rate / 25
    // -> fft_size = sample_rate * 8 / 25.
    let target = (sample_rate_hz as usize) * 8 / 25;
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

// ── DDC spectrum dB↔u16 packing (wire contract, server pack ↔ client unpack) ──
//
// A packed bin value `v` encodes `dB = FLOOR + (v / MAX) * RANGE`. The server
// packs bins with these constants and every client MUST unpack with the same
// ones, or every bin mis-scales. These are the single source of truth; both
// sides reference them instead of copying the literals -150 / 120 / 65535.
// NOTE: the server's *wideband* FFT path deliberately uses a different scale
// (see `sdr-remote-server/src/spectrum.rs`) and is intentionally not unified.

/// dB value that packs to bin 0 (noise floor of the DDC pack scale).
pub const SPECTRUM_PACK_FLOOR_DB: f32 = -150.0;
/// dB span covered by the full u16 range.
pub const SPECTRUM_PACK_RANGE_DB: f32 = 120.0;
/// Full-scale packed bin value (u16::MAX as f32).
pub const SPECTRUM_PACK_MAX: f32 = 65535.0;

/// Pack a dB value into the u16 wire bin (inverse of [`unpack_spectrum_db`]).
pub fn pack_spectrum_db(db: f32) -> u16 {
    (((db - SPECTRUM_PACK_FLOOR_DB) / SPECTRUM_PACK_RANGE_DB) * SPECTRUM_PACK_MAX)
        .clamp(0.0, SPECTRUM_PACK_MAX) as u16
}

/// Recover the dB value from a packed u16 bin (inverse of [`pack_spectrum_db`]).
pub fn unpack_spectrum_db(v: u16) -> f32 {
    SPECTRUM_PACK_FLOOR_DB + (v as f32 / SPECTRUM_PACK_MAX) * SPECTRUM_PACK_RANGE_DB
}

// Shared DSP utilities

/// Client-side helper: map dBm to a 0-228 display unit for arc-position math.
/// 0-108 = S0..S9 (12 raw units per S-unit = 6 dB/S, IARU Region 1).
/// S0 = -127 dBm, S9 = -73 dBm (HF reference, 50 uV across 50 ohm).
/// 108-228 = S9+dB zone, 2 raw units per dB - uniform with S1..S9.
/// Not used on the wire any more; the server sends raw dBm x 10 in `SmeterPacket.level`.
pub fn dbm_to_display(dbm: f32) -> u16 {
    if dbm <= -73.0 {
        ((dbm + 127.0) * 2.0).clamp(0.0, 108.0) as u16
    } else {
        (108.0 + (dbm + 73.0) * 2.0).clamp(108.0, 228.0) as u16
    }
}

#[cfg(test)]
mod spectrum_pack_tests {
    use super::*;

    #[test]
    fn spectrum_db_pack_roundtrip() {
        // The pack scale is the client/server wire contract. Assert the
        // documented anchors and that pack∘unpack is stable within one bin.
        assert_eq!(pack_spectrum_db(SPECTRUM_PACK_FLOOR_DB), 0);
        assert_eq!(pack_spectrum_db(SPECTRUM_PACK_FLOOR_DB + SPECTRUM_PACK_RANGE_DB), 65535);
        // -30 dB is the documented full-scale reference (-150 + 120).
        assert_eq!(pack_spectrum_db(-30.0), 65535);
        for &db in &[-150.0f32, -120.0, -93.0, -73.0, -50.0, -30.0] {
            let back = unpack_spectrum_db(pack_spectrum_db(db));
            assert!((back - db).abs() <= SPECTRUM_PACK_RANGE_DB / SPECTRUM_PACK_MAX + 0.01,
                "roundtrip drift at {db} dB: got {back}");
        }
    }
}
