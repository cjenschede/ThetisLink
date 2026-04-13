use sdr_remote_core::protocol::ControlId;

/// Commands sent from UI to engine via mpsc channel.
/// Replaces SharedState write operations.
pub enum Command {
    Connect(String, Option<String>), // (addr, password)
    SendTotpCode(String),            // 6-digit TOTP code
    Disconnect,
    SetPtt(bool),
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
    WriteYaesuMemories(String), // tab-separated text to write to radio
    SetYaesuTxGain(f32),
    // Yaesu EQ: (band 0-4, gain_db -12..+12)
    SetYaesuEqBand(u8, f32),
    SetYaesuEqEnabled(bool),
    // Thetis TUNE (ZZTU) with PA bypass
    ThetisTune(bool),
    // TX Monitor
    SetMonitor(bool),
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
    // Rotor
    RotorGoTo(u16),    // angle_x10
    RotorStop,
    RotorCw,
    RotorCcw,
    // CW keying
    CwKey { pressed: bool, duration_ms: u16 },
    CwMacroStop,
    // Audio recording
    StartRecording { rx1: bool, rx2: bool, yaesu: bool, path: String },
    StopRecording,
    PlayRecording { path: String },  // play last recorded WAV
    StopPlayback,
    // Server management
    ServerReboot,
    ServerShutdown,
}
