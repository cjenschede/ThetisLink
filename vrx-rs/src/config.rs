// SPDX-License-Identifier: GPL-2.0-or-later

//! Channelizer + runtime configuration types. Designed to be cheap
//! to clone and shareable across threads via `Arc<Mutex<...>>`.

/// Audio-bin width at output rate (8 kHz / 128-point iFFT = 62.5 Hz).
/// Filter edges (in Hz) are quantised to multiples of this so the
/// channelizer can map them straight to bin indices without aliasing.
pub const AUDIO_BIN_HZ: f32 = 62.5;

/// Demodulation mode for the channelizer.
///
/// SSB modes (Usb/Lsb) extract a single sideband and emit Re(z).
/// AM-family modes (Am/Sam) extract both sidebands and demodulate
/// either by envelope (Am) or by a carrier-tracking PLL (Sam,
/// synchronous AM, mirroring Thetis/WDSP amd.c: a 2nd-order loop
/// (ζ=1.0, ωN=250 rad/s, ±3 kHz capture) locks the carrier to the real
/// axis and the in-phase component is the recovered audio).
/// Fm extracts both sidebands and emits the phase derivative
/// between successive complex baseband samples.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum VrxMode {
    Usb,
    Lsb,
    Am,
    Sam,
    Fm,
}

impl VrxMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "usb" => Some(VrxMode::Usb),
            "lsb" => Some(VrxMode::Lsb),
            "am" => Some(VrxMode::Am),
            "sam" => Some(VrxMode::Sam),
            "fm" => Some(VrxMode::Fm),
            _ => None,
        }
    }
}

/// VRX audio-rate selection (one setting for all VRX; in `Auto` each VRX
/// resolves independently from its own filter width). Decoupled from the
/// global Thetis-wideband toggle.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum VrxRateMode {
    /// Force narrowband (8 kHz, audio ~4 kHz).
    Nb,
    /// Force wideband (16 kHz, audio ~8 kHz).
    Wb,
    /// Auto: wideband when the audio bandwidth needs it, else narrowband.
    Auto,
}

impl VrxRateMode {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => VrxRateMode::Wb,
            2 => VrxRateMode::Auto,
            _ => VrxRateMode::Nb,
        }
    }
}

/// Audio bandwidth (Hz) at/above which `Auto` selects wideband. NB output
/// (8 kHz) carries audio up to its 4 kHz Nyquist, so a filter that needs
/// ≥4 kHz audio bandwidth requires WB. Owner-chosen clean 4 kHz boundary.
pub const AUTO_WB_THRESHOLD_HZ: i32 = 4000;

/// Hysteresis band (Hz) below the threshold at which `Auto` drops back to
/// narrowband, avoids rebuild-thrash while dragging the filter near 4 kHz.
pub const AUTO_WB_HYSTERESIS_HZ: i32 = 250;

/// Single source of truth: does this rate-mode + filter want wideband, given
/// the current rate? For `Auto`, the audio bandwidth is the larger filter-edge
/// magnitude (= hi-edge for SSB, the half-bandwidth for AM/SAM/FM); it
/// switches up at `AUTO_WB_THRESHOLD_HZ` and back down `AUTO_WB_HYSTERESIS_HZ`
/// below it (pass the current wideband state for the hysteresis).
pub fn rate_mode_wants_wideband(
    mode: VrxRateMode,
    filter_low_hz: i32,
    filter_high_hz: i32,
    current_wb: bool,
) -> bool {
    match mode {
        VrxRateMode::Nb => false,
        VrxRateMode::Wb => true,
        VrxRateMode::Auto => {
            let bw = filter_low_hz.abs().max(filter_high_hz.abs());
            if current_wb {
                bw >= AUTO_WB_THRESHOLD_HZ - AUTO_WB_HYSTERESIS_HZ
            } else {
                bw >= AUTO_WB_THRESHOLD_HZ
            }
        }
    }
}

/// Channelizer fixed configuration (carrier offset + mode + bandwidth).
/// `carrier_offset_hz` is dynamic (updated each batch via
/// `VrxChannelizer::set_carrier_offset`); the other fields are
/// effectively const after construction.
#[derive(Clone, Debug)]
pub struct VrxConfig {
    pub carrier_offset_hz: f32,
    pub mode: VrxMode,
    /// Audio-bin offsets describing the SSB filter window after demod.
    /// Both are positive distances from carrier in 62.5 Hz steps.
    /// `lo` is the nearer edge, `hi` the farther edge. For symmetric
    /// 3 kHz SSB: lo=0, hi=48.
    pub filter_lo_bins: usize,
    pub filter_hi_bins: usize,
}

impl Default for VrxConfig {
    fn default() -> Self {
        Self {
            carrier_offset_hz: 5000.0,
            mode: VrxMode::Usb,
            filter_lo_bins: 0,
            filter_hi_bins: 48,
        }
    }
}

/// Shared VRX control state: what an external UI/network layer
/// mutates and the runtime reads each IQ batch. Held by the caller
/// in an `Arc<std::sync::Mutex<VrxControlState>>`; lock-windows are
/// short (a handful of field reads/writes) and never include an
/// `.await`, so a sync mutex is the right primitive.
#[derive(Debug, Clone)]
pub struct VrxControlState {
    /// User-toggleable stream enable (default off). When false the
    /// runtime skips all DSP work and the callback is not invoked.
    pub enabled: bool,
    /// Absolute listen frequency in Hz. The runtime translates this
    /// to a bin-offset relative to the current DDC center each batch.
    /// 0 = "not set yet"; in that case the runtime falls back to
    /// the VFO frequency supplied to `feed()`.
    pub target_freq_hz: u64,
    /// Demodulation mode (USB / LSB).
    pub mode: VrxMode,
    /// Mix gain (1.0 = unity). Provided here as a shared field so
    /// the sink can apply it externally; the runtime itself does
    /// NOT apply this gain (audio output is post-AGC, pre-volume).
    pub volume: f32,
    /// SSB filter edges as signed offsets from carrier in Hz. The
    /// runtime quantises these to audio bins per current mode. For
    /// USB both edges are typically positive (0..=3000); for LSB
    /// both negative (-3000..=0).
    pub filter_low_hz: i32,
    pub filter_high_hz: i32,
    /// SAM auto-tune-to-carrier. When true AND the
    /// mode is SAM AND the PLL is locked, the runtime nudges the listen
    /// frequency onto the detected carrier (continuous follow) and reports
    /// the new frequency so the client VFO tracks it. Default off; ignored
    /// outside SAM.
    pub sam_auto_tune: bool,
    /// The AGC mode and AGC-T of the receiver this VRX listens on; the
    /// channel's AGC follows them, at its own level.
    pub agc: crate::agc::VrxAgcSettings,
}

impl Default for VrxControlState {
    fn default() -> Self {
        Self {
            enabled: false,
            target_freq_hz: 0,
            mode: VrxMode::Usb,
            volume: 1.0,
            filter_low_hz: 0,
            filter_high_hz: 3000,
            sam_auto_tune: false,
            agc: crate::agc::VrxAgcSettings::default(),
        }
    }
}
