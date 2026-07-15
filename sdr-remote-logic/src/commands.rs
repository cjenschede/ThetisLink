// SPDX-License-Identifier: GPL-2.0-or-later

use sdr_remote_core::protocol::ControlId;

/// Commands sent from UI to engine via mpsc channel.
/// Replaces SharedState write operations.
pub enum Command {
    Connect(String, Option<String>), // (addr, password)
    SendTotpCode(String),            // 6-digit TOTP code
    Disconnect,
    SetPtt(bool),
    SetMicGateDelayMs(u32),
    SetPlaybackMute(bool),
    SetRxVolume(f32),
    SetLocalVolume(f32),
    SetVfoAVolume(f32),      // local RX1 playback volume (client-only, independent of Thetis ZZLA)
    SetVfoBVolume(f32),      // local RX2 playback volume (client-only, independent of Thetis ZZLB)
    SetTxGain(f32),
    SetFrequency(u64),
    SetMode(u8),
    SetControl(ControlId, u16),
    SetAgcEnabled(bool),
    SetInputDevice(String),
    SetOutputDevice(String),
    EnableSpectrum(bool),
    SetSpectrumFps(u8),
    SetSpectrumZoom(f32),
    SetSpectrumPan(f32),
    SetSpectrumMaxBins(u16),
    SetSpectrumFftSize(u16),  // FFT size in K (32, 65, 131, 262)
    SetAmplitecSwitchA(u8),  // 1-6
    SetAmplitecSwitchB(u8),  // 1-6
    /// Set the Amplitec power-cap table. `max_w[i]` = 0 means "no cap"
    /// for position i+1. `tx_blocked[i]` = true means RX-only (server
    /// refuses all server-initiated TX paths on that position).
    SetAmplitecPowerTable { max_w: [u16; 6], tx_blocked: [bool; 6] },
    TunerTune,
    TunerAbort,
    SpeOperate,
    SpeTune,
    SpeAntenna,
    SpeInput,
    SpePower,
    SpeBandUp,
    SpeBandDown,
    SpeOff,
    SpePowerOn,
    SpeDriveDown,
    SpeDriveUp,
    Rf2kOperate(bool),
    Rf2kTune,
    Rf2kAnt1,
    Rf2kAnt2,
    Rf2kAnt3,
    Rf2kAnt4,
    Rf2kAntExt,
    Rf2kErrorReset,
    Rf2kClose,
    Rf2kDriveUp,
    Rf2kDriveDown,
    Rf2kTunerMode(u8),     // 0=MANUAL, 1=AUTO
    Rf2kTunerBypass(bool),
    Rf2kTunerReset,
    Rf2kTunerStore,
    Rf2kTunerLUp,
    Rf2kTunerLDown,
    Rf2kTunerCUp,
    Rf2kTunerCDown,
    Rf2kTunerK,
    // RF2K-S debug (Fase D)
    Rf2kSetHighPower(bool),
    Rf2kSetTuner6m(bool),
    Rf2kSetBandGap(bool),
    Rf2kFrqDelayUp,
    Rf2kFrqDelayDown,
    Rf2kAutotuneThresholdUp,
    Rf2kAutotuneThresholdDown,
    Rf2kDacAlcUp,
    Rf2kDacAlcDown,
    Rf2kZeroFRAM,
    Rf2kSetDriveConfig { category: u8, band: u8, value: u8 },
    // Yaesu FT-991A
    SetYaesuVolume(f32),
    SetYaesuPtt(bool),
    SetYaesuFreq(u64),
    SetYaesuMode(u8),
    SetYaesuMenu(u16, String), // (menu number, P2 value)
    // Dual-radio slot 1 (PATCH-dual-radio-991a-ftx1) — spiegel van de slot-0
    // Yaesu-commando's, geroute naar radio 1 (Yaesu2*-controls / FrequencyYaesu2).
    SetYaesu2Enable(bool),
    SetYaesu2Volume(f32),
    SetYaesu2Ptt(bool),
    SetYaesu2Freq(u64),
    SetYaesu2Mode(u8),
    SetYaesu2TxGain(f32),
    SetYaesu2EqBand(u8, f32),
    SetYaesu2EqEnabled(bool),
    /// FTX-1 EX-menu set (Fase C): (adres "p1p2p3", waarde). Reist als
    /// YaesuMemoryData2 met "SETMENU:"-prefix naar de server.
    SetYaesu2Menu(String, String),
    // VRX1 (Virtual RX on RX1 IQ + VFO-A)
    SetVrxEnabled(bool),
    SetVrxMode(u8),    // 0=USB, 1=LSB
    SetVrxFrequency(u64),
    SetVrxVolume(f32), // local mix gain, 0.0..=2.0
    /// (vrx_id 0|1, filter_low_hz, filter_high_hz). Server quantises to
    /// 62.5 Hz audio bins; client should pre-snap so visual = audio.
    SetVrxFilter(u8, i32, i32),
    /// (vrx_id 0|1, enabled, span_khz). When enabled, server emits
    /// SpectrumVrx1/SpectrumVrx2 packets containing a high-resolution
    /// extracted view centered on the VRX freq with the given span.
    /// span_khz=0 with enabled=true defaults to 24 kHz server-side.
    SetVrxHighResSpectrum(u8, bool, u16),
    // VRX2 (Virtual RX on RX2 IQ + VFO-B)
    SetVrx2Enabled(bool),
    SetVrx2Mode(u8),
    SetVrx2Frequency(u64),
    SetVrx2Volume(f32),
    // VRX wide / synchronous-AM UX (PATCH-vrx-wide-sam-ux)
    /// VRX1 audio-rate mode: 0=NB, 1=WB, 2=Auto (PATCH-vrx-per-client).
    SetVrxRateMode(u8),
    /// VRX2 audio-rate mode: 0=NB, 1=WB, 2=Auto — per-client per-VRX (VrxAudioRate2).
    SetVrxRateMode2(u8),
    /// (vrx_id 0|1, on) — SAM auto-tune-to-carrier per VRX.
    SetVrxAutoTune(u8, bool),
    /// TX modulation filter band (low_hz, high_hz) — PATCH-tx-modulation-bandwidth.
    /// Main-radio TX, not VRX. Server applies via tx_filter_band_ex.
    SetTxFilter(i32, i32),
    /// Typed Yaesu DSP/function control (PATCH-yaesu-extra-controls):
    /// (slot 0=radio1/1=radio2, control=YaesuCtrl as u8, value=toggle 0/1 or level).
    SetYaesuControl(u8, u8, u16),
    WriteYaesuMemories(String), // tab-separated text to write to radio
    WriteYaesu2Memories(String), // idem radio 2 (Fase B)
    SetYaesuTxGain(f32),
    // Yaesu EQ: (band 0-4, gain_db -12..+12)
    SetYaesuEqBand(u8, f32),
    SetYaesuEqEnabled(bool),
    // Client-side Yaesu TX-keten per radio (net als de EQ): compressor-amount 0-100
    // en eigen AGC-toggle (los van de Thetis-AGC). Radio-processing werkt niet op USB.
    SetYaesuCompressor(u8),
    SetYaesuTxAgc(bool),
    SetYaesu2Compressor(u8),
    SetYaesu2TxAgc(bool),
    // Thetis TUNE (ZZTU) with PA bypass
    ThetisTune(bool),
    // TX Monitor
    SetMonitor(bool),
    // DX-cluster spot stream opt-out (data-saving voor metered links)
    SetDxSpotsEnabled(bool),
    /// Thetis-audio wideband opt-in (16 kHz Opus i.p.v. 8 kHz default).
    /// Stuurt `ControlId::ThetisWidebandAudio` naar server en switcht
    /// lokaal decoder/resampler pad bij ontvangst van WB-getagde audio
    /// packets.
    SetThetisWidebandAudio(bool),
    // RX2 / VFO-B
    SetRx2Enabled(bool),
    SetVfoSync(bool),
    SetFrequencyRx2(u64),
    SetModeRx2(u8),
    SetRx2Volume(f32),       // local RX2 playback volume
    EnableRx2Spectrum(bool),
    SetRx2SpectrumFps(u8),
    SetRx2SpectrumZoom(f32),
    SetRx2SpectrumPan(f32),
    // UltraBeam RCU-06
    UbRetract,
    UbSetFrequency(u16, u8),  // khz, direction
    UbReadElements,
    UbModifyElement(u8, u16),  // index (0-5), length_mm
    // Rotor
    RotorGoTo(u16),    // angle_x10
    RotorStop,
    RotorCw,
    RotorCcw,
    // CW keying
    CwKey { pressed: bool, duration_ms: u16 },
    CwMacroStop,
    // Audio recording
    StartRecording { rx1: bool, rx2: bool, yaesu: bool, yaesu2: bool, vrx1: bool, vrx2: bool, path: String },
    StopRecording,
    PlayRecording { path: String },  // play last recorded WAV
    StopPlayback,
    // Server management
    ServerReboot,
    ServerShutdown,
    // S-meter source selection (matches Thetis Multimeter Sig/Avg/MaxBin choice).
    // Value: 0=Sig, 1=Avg, 2=MaxBin. Applies to both RX1 and RX2.
    SetSmeterSource(u8),
}
