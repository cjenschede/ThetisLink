// SPDX-License-Identifier: GPL-2.0-or-later

use anyhow::{bail, Result};
use num_enum::TryFromPrimitive;

/// Magic byte identifying our protocol
pub const MAGIC: u8 = 0xAA;

/// Protocol version.
///
/// **Breaking change v0.3.x → v2.0.0 (TL2):** new packet types (AudioRx2,
/// AudioYaesu, AudioBinR, AudioMultiCh, Diversity*, Yaesu*, etc.), expanded
/// ControlId range, new auth path. v1-clients cannot interoperate with v2-
/// servers and vice versa; mismatch surfaces as `unsupported version` at
/// header-deserialize time (early, explicit failure).
///
/// **Breaking change v2 → v3 (build 58 bundle):** `SmeterPacket.level`
/// reinterpreted from unsigned 0-260 display-units to signed deci-units
/// (dBm × 10 in RX, watts × 10 in TX). Wire size unchanged (2 bytes BE),
/// but a v2-peer reading a v3-packet (or vice versa) would clamp/overflow
/// silently and corrupt the S-meter. Bumping VERSION forces explicit
/// "unsupported version" rejection at handshake / first packet, so mixed
/// deploys fail loudly instead of producing garbage readings. Server,
/// desktop client and Android must all upgrade in lockstep.
pub const VERSION: u8 = 3;

/// Packet type identifiers
#[derive(Debug, Clone, Copy, PartialEq, Eq, TryFromPrimitive)]
#[repr(u8)]
pub enum PacketType {
    Audio = 0x01,
    Heartbeat = 0x02,
    HeartbeatAck = 0x03,
    Control = 0x04,
    Disconnect = 0x05,
    PttDenied = 0x06,
    Frequency = 0x07,
    Mode = 0x08,
    Smeter = 0x09,
    Spectrum = 0x0A,
    /// Full DDC spectrum row for waterfall history (same format as Spectrum)
    FullSpectrum = 0x0B,
    /// Equipment status (server → client)
    EquipmentStatus = 0x0C,
    /// Equipment command (client → server)
    EquipmentCommand = 0x0D,
    /// RX2 audio (server → client, same format as Audio)
    AudioRx2 = 0x0E,
    /// RX2 frequency (bidirectional, same format as Frequency)
    FrequencyRx2 = 0x0F,
    /// RX2 mode (bidirectional, same format as Mode)
    ModeRx2 = 0x10,
    /// RX2 S-meter (server → client, same format as Smeter)
    SmeterRx2 = 0x11,
    /// RX2 spectrum (server → client, same format as Spectrum)
    SpectrumRx2 = 0x12,
    /// RX2 full spectrum row for waterfall (same format as FullSpectrum)
    FullSpectrumRx2 = 0x13,
    /// DX cluster spot (server → client)
    Spot = 0x14,
    /// TX profile list with names (server → client)
    TxProfiles = 0x15,
    /// Yaesu audio (server → client, same format as Audio)
    AudioYaesu = 0x16,
    /// Yaesu radio state (server → client)
    YaesuState = 0x17,
    /// Yaesu frequency set (client → server, same format as Frequency)
    FrequencyYaesu = 0x18,
    /// Binaural right channel audio (server → client, same format as Audio) [deprecated]
    AudioBinR = 0x1A,
    /// Multi-channel audio: 1-4 mono Opus frames bundled in one packet
    AudioMultiCh = 0x1B,
    /// RX1 S-meter Sig source (server → client, same SmeterPacket format).
    /// Emitted when client subscribes via SmeterSources bit 0. Carries WDSP
    /// RXA_S_PK (peak-hold with 100ms decay) — Multimeter "Sig" mode.
    SmeterSig = 0x1C,
    /// RX1 S-meter MaxBin source (server → client, same SmeterPacket format).
    /// Emitted when client subscribes via SmeterSources bit 2. Carries the
    /// highest single FFT bin in the passband — Multimeter "Max Bin" mode.
    SmeterMaxBin = 0x1D,
    /// RX2 S-meter Sig source (server → client). Bit 4 in SmeterSources.
    SmeterRx2Sig = 0x1E,
    /// RX2 S-meter MaxBin source (server → client). Bit 6 in SmeterSources.
    SmeterRx2MaxBin = 0x1F,
    /// Yaesu memory data (server → client, tab-separated text)
    YaesuMemoryData = 0x19,
    /// Amplitec power-cap table (server → client push, client → server update).
    /// 18-byte payload: 6 × { u16 max_w BE (0 = no cap), u8 tx_blocked (0/1) }.
    /// Drives the "Power-cap table" section in the client's Amplitec tab.
    AmplitecPowerTable = 0x20,
    /// Authentication challenge (server → client, 16-byte nonce)
    AuthChallenge = 0x30,
    /// Authentication response (client → server, 32-byte HMAC)
    AuthResponse = 0x31,
    /// Authentication result (server → client, 1 byte: 0=rejected, 1=accepted, 2=totp_required)
    AuthResult = 0x32,
    /// TOTP challenge (server → client, signals that TOTP code is needed)
    TotpChallenge = 0x33,
    /// TOTP response (client → server, 6-digit code as UTF-8 string)
    TotpResponse = 0x34,
    /// VRX audio (server → client) — Virtual RX audio stream, Opus-encoded
    /// narrowband (8 kHz mono). Carries a `vrx_id` byte so multiple VRX
    /// channels can be multiplexed on the same UDP socket. Currently
    /// experimental (M2b spike), single hardcoded VRX in v2.1.1+.
    AudioVrx = 0x21,
    /// VRX absolute frequency set (client → server). Uses VrxFrequencyPacket
    /// layout: header(4) + vrx_id(1) + frequency_hz(8) = 13 bytes. The server
    /// computes the bin-offset relative to the current DDC center.
    FrequencyVrx = 0x22,
    /// VRX1 high-res extracted spectrum view (server → client). Layout
    /// matches `SpectrumPacket`. Sent periodically by the server when a
    /// client has VRX1-high-res enabled (`ControlId::VrxSpectrumEnable`).
    /// Center is at VRX1 freq + caller-requested span.
    SpectrumVrx1 = 0x23,
    /// VRX2 high-res extracted spectrum view (server → client).
    SpectrumVrx2 = 0x24,

    // ── Dual-radio slot 1 (PATCH-dual-radio-991a-ftx1, Option B-prime) ──
    // Slot 0 = existing Yaesu packets (0x16-0x19), byte-identical/unchanged.
    // Slot 1 = second radio, own packet variants in the free gap 0x25-0x28.
    // Back-compat by construction: old clients don't know these types, parse
    // them as "unknown packet type" (no crash) and never subscribe to slot 1.
    /// Slot-1 radio audio (server → client, same layout as AudioYaesu).
    AudioYaesu2 = 0x25,
    /// Slot-1 radio state (server → client, same layout as YaesuState).
    YaesuState2 = 0x26,
    /// Slot-1 frequency set (client → server, same layout as FrequencyYaesu).
    FrequencyYaesu2 = 0x27,
    /// Slot-1 memory data (server → client, tab-separated text, same layout as YaesuMemoryData).
    YaesuMemoryData2 = 0x28,
    /// Per-radio model info (server → client): tells the client the detected
    /// model per slot for panel naming ("991A 1"/"FTX1" etc.).
    /// Separate packet so the byte-identical slot-0 YaesuState is left untouched
    /// (B-prime); old clients ignore this unknown type.
    RadioInfo = 0x29,
    /// VRX tracked carrier frequency (server → client) — SAM auto-tune.
    /// Layout like VrxFrequencyPacket: header(4) + vrx_id(1) + frequency_hz(8)
    /// = 13 bytes. Sent (throttled, ≥~10 Hz delta) when auto-tune is
    /// on and the PLL tracks the carrier, so the client VFO follows along.
    /// Per-client gated on VrxSamAutoTune(2); old clients never subscribe.
    FrequencyVrxActual = 0x2A,
    /// Current TX modulation filter band (server → client) — PATCH-tx-modulation-
    /// bandwidth. Layout: header(4) + low_hz(i32) + high_hz(i32) = 12 bytes.
    /// Sent as soon as Thetis reports a tx_filter_band_ex; the client shows the
    /// current value and thereby knows that setting it is supported. Old
    /// clients don't process this type — an unknown packet type is nonfatal
    /// (they log it at most as "unknown packet type" and carry on).
    TxFilterBand = 0x2B,
    /// Typed Yaesu DSP/function control (client→server): {slot, control, value}.
    /// Replaces YaesuButton magic-values (PATCH-yaesu-extra-controls). Additive.
    YaesuControl = 0x2C,
    /// Yaesu feature-state feedback (server→client): per-slot toggles bitfield +
    /// levels. Value-change-only, opt-in send. Additive.
    YaesuFeature = 0x2D,
    /// Yaesu radio presence (server→client, broadcast to all clients): which
    /// slots have a connected radio + their model. Decouples the selector/
    /// connected-status from the audio+state subscription — dynamic (dropping/
    /// appearing) instead of sticky. Push-on-change + once on (re)connect.
    /// Old clients ignore this unknown type (back-compat by construction).
    YaesuPresence = 0x2E,
    /// Ask the connected server for its own problem-report attachment
    /// (client -> server). Header only. Answered with `ServerReportPart`
    /// packets, or with nothing at all if the server cannot build one.
    ServerReportRequest = 0x35,
    /// One numbered part of that attachment (server -> client).
    ///
    /// `header(4) + part u16 + parts u16 + len u16 + bytes`. Numbered because
    /// this is UDP: a log is far too big for one datagram, and a client that
    /// cannot tell a complete transfer from a lossy one would attach a
    /// truncated log to a report and call it the server's log.
    ServerReportPart = 0x36,
}

impl PacketType {
    pub fn from_u8(v: u8) -> Option<Self> {
        Self::try_from(v).ok()
    }

    /// True for every audio-carrying packet type. Transport routing and the
    /// bandwidth classifier key on this: audio takes the low-latency path
    /// (relay UDP), everything else the reliable path (wss). The match is
    /// EXHAUSTIVE on purpose — adding a new `Audio*` variant forces a
    /// compile-time decision here instead of silently mis-routing over the
    /// slow path (the drift that previously lived as literal byte-sets in the
    /// relay and the server classifier).
    pub fn is_audio(self) -> bool {
        matches!(
            self,
            PacketType::Audio
                | PacketType::AudioRx2
                | PacketType::AudioYaesu
                | PacketType::AudioBinR
                | PacketType::AudioMultiCh
                | PacketType::AudioVrx
                | PacketType::AudioYaesu2
        )
    }

    /// True for every spectrum/waterfall packet type (used by the bandwidth
    /// classifier). Exhaustive like [`is_audio`](Self::is_audio).
    pub fn is_spectrum(self) -> bool {
        matches!(
            self,
            PacketType::Spectrum
                | PacketType::FullSpectrum
                | PacketType::SpectrumRx2
                | PacketType::FullSpectrumRx2
                | PacketType::SpectrumVrx1
                | PacketType::SpectrumVrx2
        )
    }
}

/// Audio-carrying packet type discriminants, as raw wire bytes. Single source
/// of truth for consumers that classify on the raw type byte without decoding
/// the whole packet (transport routing, bandwidth metering). Kept in lockstep
/// with [`PacketType::is_audio`] by a test (all 256 byte values must agree).
pub const AUDIO_PACKET_TYPES: &[u8] = &[
    PacketType::Audio as u8,
    PacketType::AudioRx2 as u8,
    PacketType::AudioYaesu as u8,
    PacketType::AudioBinR as u8,
    PacketType::AudioMultiCh as u8,
    PacketType::AudioVrx as u8,
    PacketType::AudioYaesu2 as u8,
];

/// Spectrum/waterfall packet type discriminants, as raw wire bytes. Kept in
/// lockstep with [`PacketType::is_spectrum`] by a test.
pub const SPECTRUM_PACKET_TYPES: &[u8] = &[
    PacketType::Spectrum as u8,
    PacketType::FullSpectrum as u8,
    PacketType::SpectrumRx2 as u8,
    PacketType::FullSpectrumRx2 as u8,
    PacketType::SpectrumVrx1 as u8,
    PacketType::SpectrumVrx2 as u8,
];

/// Control command identifiers
#[derive(Debug, Clone, Copy, PartialEq, Eq, TryFromPrimitive)]
#[repr(u8)]
pub enum ControlId {
    /// RX1 AF gain (value 0-100, CAT: ZZLA)
    Rx1AfGain = 0x01,
    /// Power on/off (value 0/1, CAT: ZZPS)
    PowerOnOff = 0x02,
    /// TX Profile index (value 0-99, CAT: ZZTP)
    TxProfile = 0x03,
    /// Noise Reduction level (value 0-4: 0=off, 1=NR1, 2=NR2, 3=NR3, 4=NR4, CAT: ZZNE)
    NoiseReduction = 0x04,
    /// Auto Notch Filter on/off (value 0/1, CAT: ZZNT)
    AutoNotchFilter = 0x05,
    /// Drive level (value 0-100, CAT: ZZPC)
    DriveLevel = 0x06,
    /// Spectrum enable (value 0=off, 1=on)
    SpectrumEnable = 0x07,
    /// Spectrum FPS (value 5-30)
    SpectrumFps = 0x08,
    /// Spectrum zoom level (value: zoom × 10 as u16, e.g. 10=1x, 320=32x, 10240=1024x)
    SpectrumZoom = 0x09,
    /// Spectrum pan position (value: (pan + 0.5) × 10000 as u16, 5000=center)
    SpectrumPan = 0x0A,
    /// Filter low cut (value: Hz offset from VFO as i16 cast to u16)
    FilterLow = 0x0B,
    /// Filter high cut (value: Hz offset from VFO as i16 cast to u16)
    FilterHigh = 0x0C,
    /// Thetis starting indicator (server→client, value 0/1)
    ThetisStarting = 0x0D,
    /// RX2 enable on/off (value 0/1)
    Rx2Enable = 0x0E,
    /// RX2 AF gain (value 0-100, CAT: ZZLB)
    Rx2AfGain = 0x0F,
    /// RX2 spectrum zoom (same encoding as SpectrumZoom)
    Rx2SpectrumZoom = 0x10,
    /// RX2 spectrum pan (same encoding as SpectrumPan)
    Rx2SpectrumPan = 0x11,
    /// RX2 filter low cut (same encoding as FilterLow)
    Rx2FilterLow = 0x12,
    /// RX2 filter high cut (same encoding as FilterHigh)
    Rx2FilterHigh = 0x13,
    /// VFO sync on/off (value 0/1: VFO-B follows VFO-A frequency)
    VfoSync = 0x14,
    /// RX2 spectrum enable (value 0=off, 1=on)
    Rx2SpectrumEnable = 0x15,
    /// RX2 spectrum FPS (value 5-30)
    Rx2SpectrumFps = 0x16,
    /// RX2 noise reduction (same encoding as NoiseReduction)
    Rx2NoiseReduction = 0x17,
    /// RX2 auto notch filter (value 0/1)
    Rx2AutoNotchFilter = 0x18,
    /// VFO A⇔B swap (write-only trigger, value ignored; maps to ZZVS2)
    VfoSwap = 0x19,
    /// Spectrum max bins per packet (value: max bins as u16, 0=server default)
    SpectrumMaxBins = 0x1A,
    /// RX2 spectrum max bins per packet (same encoding as SpectrumMaxBins)
    Rx2SpectrumMaxBins = 0x1B,
    /// FFT size for spectrum processing (value: size in K, e.g. 32=32768, 65=65536, 131=131072, 262=262144)
    SpectrumFftSize = 0x1C,
    /// RX2 FFT size for spectrum processing (same encoding as SpectrumFftSize)
    Rx2SpectrumFftSize = 0x3F,
    /// Spectrum bin depth: 8 = u8 bins (1 byte), 16 = u16 bins (2 bytes). Default 8.
    SpectrumBinDepth = 0x1D,
    /// TX Monitor on/off (value 0/1, CAT: ZZMO, TCI: MON_ENABLE)
    MonitorOn = 0x1E,
    /// Thetis TUNE on/off (value 0/1, CAT: ZZTU)
    ThetisTune = 0x1F,
    /// Yaesu AUDIO subscription (value 0/1). State (freq/s-meter/CAT) has run since the
    /// audio/state split via YaesuStateEnable; this control only sets the audio stream.
    YaesuEnable = 0x20,
    /// Yaesu PTT (value 0/1: TX0/TX1 via Yaesu CAT)
    YaesuPtt = 0x21,
    /// Yaesu frequency set (uses FrequencyPacket, not ControlPacket)
    YaesuFreq = 0x22,
    /// Yaesu mic gain (value: gain × 10, e.g. 200 = 20.0x)
    YaesuMicGain = 0x23,
    /// Yaesu operating mode (value: internal mode number)
    YaesuMode = 0x24,
    /// Yaesu read all memories (value: ignored)
    YaesuReadMemories = 0x25,
    /// Yaesu recall memory channel (value: channel number 1-99)
    YaesuRecallMemory = 0x26,
    /// Yaesu write all memories to radio (value: ignored)
    YaesuWriteMemories = 0x27,
    /// Yaesu select VFO (value: 0=VFO A, 1=VFO B, 2=swap)
    YaesuSelectVfo = 0x28,
    /// Yaesu squelch (value: 0-255)
    YaesuSquelch = 0x29,
    /// Yaesu RF gain (value: 0-255)
    YaesuRfGain = 0x2A,
    /// Yaesu mic gain (value: 0-100) — radio's own mic gain, not ThetisLink TX gain
    YaesuRadioMicGain = 0x2B,
    /// Yaesu RF power (value: 0-100)
    YaesuRfPower = 0x2C,
    /// Yaesu raw CAT button (value: button ID, see YAESU_BUTTONS)
    YaesuButton = 0x2D,
    /// Yaesu read all EX menu settings (value: ignored)
    YaesuReadMenus = 0x2E,
    /// Yaesu set EX menu item (value: menu number, P2 in separate data packet)
    YaesuSetMenu = 0x2F,
    /// AGC mode (value: 0=off, 1=long, 2=slow, 3=med, 4=fast, 5=custom; TCI: agc_mode)
    AgcMode = 0x30,
    /// AGC gain (value: 0-120; TCI: agc_gain)
    AgcGain = 0x31,
    /// RIT enable (value: 0/1; TCI: rit_enable)
    RitEnable = 0x32,
    /// RIT offset in Hz (value: i16 cast to u16; TCI: rit_offset)
    RitOffset = 0x33,
    /// XIT enable (value: 0/1; TCI: xit_enable)
    XitEnable = 0x34,
    /// XIT offset in Hz (value: i16 cast to u16; TCI: xit_offset)
    XitOffset = 0x35,
    /// Squelch enable (value: 0/1; TCI: sql_enable)
    SqlEnable = 0x36,
    /// Squelch level (value: 0-160; TCI: sql_level)
    SqlLevel = 0x37,
    /// Noise Blanker enable (value: 0/1; TCI: rx_nb_enable)
    NoiseBlanker = 0x38,
    /// CW keyer speed (value: 1-60 WPM; TCI: cw_keyer_speed)
    CwKeyerSpeed = 0x39,
    /// VFO lock (value: 0/1; TCI: vfo_lock)
    VfoLock = 0x3A,
    /// Binaural enable (value: 0/1; TCI: rx_bin_enable)
    Binaural = 0x3B,
    /// Audio Peak Filter enable (value: 0/1; TCI: rx_apf_enable)
    ApfEnable = 0x3C,

    /// RX2 AGC mode (same encoding as AgcMode)
    Rx2AgcMode = 0x50,
    /// RX2 AGC gain (same encoding as AgcGain)
    Rx2AgcGain = 0x51,
    /// RX2 Squelch enable (same encoding as SqlEnable)
    Rx2SqlEnable = 0x52,
    /// RX2 Squelch level (same encoding as SqlLevel)
    Rx2SqlLevel = 0x53,
    /// RX2 Noise Blanker enable (same encoding as NoiseBlanker)
    Rx2NoiseBlanker = 0x54,
    /// RX2 Binaural enable (same encoding as Binaural)
    Rx2Binaural = 0x55,
    /// RX2 Audio Peak Filter enable (same encoding as ApfEnable)
    Rx2ApfEnable = 0x56,
    /// RX2 VFO lock (value: 0/1; TCI: vfo_lock:0,1)
    Rx2VfoLock = 0x57,
    /// Tune drive level (value: 0-100; TCI: tune_drive)
    TuneDrive = 0x58,
    /// Monitor volume in dB (value: i8 as u16, typically -40..0; TCI: mon_volume)
    MonitorVolume = 0x59,

    /// Trigger server-side diversity auto-null (value: 1=start)
    DiversityAutoNull = 0x4A,
    /// AGC Auto mode RX1 (value: 0=off, 1=on)
    AgcAutoRx1 = 0x48,
    /// AGC Auto mode RX2 (value: 0=off, 1=on)
    AgcAutoRx2 = 0x49,

    /// DDC sample rate RX1 (value: rate in kHz, e.g. 384 = 384000 Hz).
    /// Source: stock TCI `iq_samplerate` (primary) or `if_limits` (fallback) — see
    /// `PATCH-tl2-server-if-limits` (alpha-3). Both RX1 and RX2 get the same
    /// value in stock mode (TCI exposes one global rate). Per-RX divergence
    /// returns via TL2-x fork extensions in Phase 3.
    DdcSampleRateRx1 = 0x3D,
    /// DDC sample rate RX2 (same encoding as DdcSampleRateRx1).
    DdcSampleRateRx2 = 0x3E,

    /// Diversity enable (value: 0=off, 1=on)
    DiversityEnable = 0x40,
    /// Diversity reference source (value: 0=RX2, 1=RX1)
    DiversityRef = 0x41,
    /// Diversity RX source (value: 0=RX1+RX2, 1=RX1, 2=RX2)
    DiversitySource = 0x42,
    /// Diversity RX1 gain (value: gain × 1000, e.g. 2500 = 2.500)
    DiversityGainRx1 = 0x43,
    /// Diversity RX2 gain (value: gain × 1000, e.g. 2500 = 2.500)
    DiversityGainRx2 = 0x44,
    /// Diversity phase (value: phase × 100 + 18000, e.g. 18000=0°, 0=-180°, 36000=+180°)
    DiversityPhase = 0x45,
    /// Read diversity state from Thetis (value: ignored)
    DiversityRead = 0x46,
    /// Diversity GainMulti — gates per-RX gain max (value: multi × 100, range 100..1000 = 1.00..10.00).
    /// TL2-1 fork-only `diversity_gain_multi_ex` command.
    DiversityGainMulti = 0x47,

    /// Global mute (value: 0/1; TCI: mute)
    Mute = 0x5A,
    /// RX mute (value: 0/1; TCI: rx_mute:0)
    RxMute = 0x5B,
    /// Manual Notch Filter enable (value: 0/1; TCI: rx_nf_enable:0)
    ManualNotchFilter = 0x5C,
    /// RX Balance L/R pan (value: i8 -40..+40 as i16 cast to u16; TCI: rx_balance:0,0)
    RxBalance = 0x5D,

    /// CW keyer key down/up (value: bit 0 = pressed, bits 1-15 = duration_ms; 0 = no duration)
    CwKey = 0x5E,
    /// CW macro stop (value: ignored; TCI: cw_macros_stop)
    CwMacroStop = 0x5F,

    /// RX2 Manual Notch Filter enable (value: 0/1; TCI: rx_nf_enable:1)
    Rx2ManualNotchFilter = 0x60,
    /// Thetis SWR (value: SWR × 100, e.g. 150 = 1.50:1; server → client broadcast during TX)
    ThetisSwr = 0x61,
    /// Audio routing mode (0=Mono RX1→L+R, 1=Binaural RX1L+RX1R, 2=Split RX1→L RX2→R)
    AudioMode = 0x62,

    /// Per-client setup checkbox "Allow zoom below 2× (waterfall smear during tune)".
    /// Value: 0=unchecked (default, smear-free guaranteed), 1=checked (zoom 1× allowed, smear trade-off).
    /// TL2-1 server enforces strictest setting over all connected clients (zoom-min 2×
    /// as long as one client has it unchecked). Used by `auto_recenter_ex` feature.
    AllowZoomBelow2x = 0x63,

    /// Per-client S-meter source-subscription bitmap. Each bit toggles emission
    /// of one S-meter packet type by the server. Default (no control sent):
    /// `0x22` = RX1 Avg + RX2 Avg, matches pre-multi-source behaviour.
    ///
    /// Bitmap layout (u16):
    ///   bit 0  = RX1 Sig    → PacketType::SmeterSig         (WDSP RXA_S_PK)
    ///   bit 1  = RX1 Avg    → PacketType::Smeter            (WDSP RXA_S_AV)
    ///   bit 2  = RX1 MaxBin → PacketType::SmeterMaxBin      (single peak FFT bin)
    ///   bit 4  = RX2 Sig    → PacketType::SmeterRx2Sig
    ///   bit 5  = RX2 Avg    → PacketType::SmeterRx2         (existing)
    ///   bit 6  = RX2 MaxBin → PacketType::SmeterRx2MaxBin
    /// All other bits reserved.
    SmeterSources = 0x64,
    /// DX-spot stream opt-out (value 0=off, 1=on). Default ON. When OFF
    /// the server sends no more `PacketType::Spot` frames to this client
    /// — bandwidth saving on metered links.
    DxSpotsEnabled = 0x65,
    /// Thetis wideband-audio opt-in (value 0=narrowband 8 kHz default,
    /// 1=wideband 16 kHz). When 1 the server sends RX1/RX2/BinR via
    /// wideband Opus and accepts TX audio with `Flags::AUDIO_WIDEBAND`.
    /// Requires `Capabilities::WIDEBAND_AUDIO` on both server and client.
    ThetisWidebandAudio = 0x66,

    /// VRX enable (client → server, value 0/1, vrx_id implicit = 0 for single-VRX
    /// M2b.4 spike). When 1, the server runs the FFT-channelizer on RX1 IQ and
    /// streams Opus VRX audio. When 0, channelizer is paused.
    VrxEnable = 0x67,
    /// VRX mode (client → server, value 0=USB, 1=LSB). Matches `vrx::VrxMode`
    /// enum on the server side.
    VrxMode = 0x68,
    /// VRX volume (client → server, value = volume × 100, range 0..=200 =
    /// 0%..200%). Applied server-side before VRX is mixed into the RX1
    /// audio stream. Build 45+: VRX audio routes through the RX1 Opus
    /// encoder instead of its own stream.
    VrxVolume = 0x69,
    /// VRX2 enable (client → server, value 0/1). Mirrors `VrxEnable` for
    /// the second virtual receiver (running on the RX2 IQ stream + VFO-B).
    VrxEnable2 = 0x6A,
    /// VRX2 mode (client → server, value 0=USB, 1=LSB). Mirrors `VrxMode`.
    VrxMode2 = 0x6B,
    /// VRX2 volume placeholder — present for protocol symmetry with VRX1.
    /// Build 50+: local mix gain on the client; not currently applied
    /// server-side (channel is its own UDP stream).
    VrxVolume2 = 0x6C,
    /// VRX1 SSB filter low edge in Hz (signed i16 packed into u16).
    /// USB convention: positive; LSB convention: negative.
    VrxFilterLow = 0x6D,
    /// VRX1 SSB filter high edge in Hz (signed i16 packed into u16).
    VrxFilterHigh = 0x6E,
    /// VRX2 SSB filter low edge (mirror of VrxFilterLow).
    VrxFilterLow2 = 0x6F,
    /// VRX2 SSB filter high edge (mirror of VrxFilterHigh).
    VrxFilterHigh2 = 0x70,
    /// VRX1 high-res spectrum extraction enable (client → server, 0/1).
    /// When 1, server periodically emits SpectrumVrx1 packets matching
    /// the current VrxSpectrumSpan width centered on VrxFrequency.
    VrxSpectrumEnable = 0x71,
    /// VRX2 high-res spectrum extraction enable (client → server).
    VrxSpectrumEnable2 = 0x72,
    /// VRX1 high-res spectrum span in kHz (client → server, u16 kHz).
    /// Used as the visible-window width for the server-side extraction.
    VrxSpectrumSpanKhz = 0x73,
    /// VRX2 high-res spectrum span in kHz (client → server).
    VrxSpectrumSpanKhz2 = 0x74,

    // ── Wide / synchronous-AM VRX-UX (PATCH-vrx-wide-sam-ux) ──
    /// VRX audio-rate mode (client → server, 0=NB/8k, 1=WB/16k, 2=Auto).
    /// One control for VRX1+VRX2. In Auto the server picks the rate per VRX
    /// based on the filter width (≥4 kHz audio-BW → 16k). The client
    /// reads the actual rate from the AUDIO_WIDEBAND flag per packet.
    VrxAudioRate = 0x75,
    /// VRX1 SAM auto-tune-to-carrier enable (client → server, 0/1). Only
    /// active in SAM: on lock the listen frequency follows the carrier.
    VrxSamAutoTune = 0x76,
    /// VRX2 SAM auto-tune-to-carrier enable (client → server, 0/1).
    VrxSamAutoTune2 = 0x77,

    // ── TX modulation bandwidth (PATCH-tx-modulation-bandwidth) ──
    /// TX filter low edge (client → server, signed Hz as u16). Server sets it
    /// via tx_filter_band_ex (stock v2.10.3.14+ / fork). Main-radio TX, not VRX.
    TxFilterLow = 0x78,
    /// TX filter high edge (client → server, signed Hz as u16).
    TxFilterHigh = 0x79,

    /// VRX2 audio-rate NB/WB/Auto (client → server). Additive to
    /// `VrxAudioRate` (=VRX1) so each per-client independent VRX can set its own
    /// rate (PATCH-vrx-per-client, Option B). Back-compat: old clients
    /// don't send it (VRX1+2 then share `VrxAudioRate`); old servers drop this
    /// unknown control packet nonfatal. Client sends only on-change (no hot-loop spam).
    VrxAudioRate2 = 0x7A,

    // ── Dual-radio slot 1 (PATCH-dual-radio-991a-ftx1, Option B-prime) ──
    // 1:1 mirror of the slot-0 Yaesu controls (0x20-0x2F) on the free range
    // 0x80-0x8F. NOT 0x30-0x3F (occupied by AGC/RIT/DDC/Rx2 controls).
    // Yaesu2Enable is also the slot-1 subscription gate: a session only gets
    // slot-1 state/audio/memory after it sends this control with value=1.
    /// Slot-1 stream enable + subscription gate (value 0/1). Mirror of YaesuEnable.
    Yaesu2Enable = 0x80,
    /// Slot-1 PTT (value 0/1). Mirror of YaesuPtt.
    Yaesu2Ptt = 0x81,
    /// Slot-1 frequency set (uses FrequencyYaesu2 packet). Mirror of YaesuFreq.
    Yaesu2Freq = 0x82,
    /// Slot-1 ThetisLink mic gain (value × 10). Mirror of YaesuMicGain.
    Yaesu2MicGain = 0x83,
    /// Slot-1 operating mode (value: internal mode number). Mirror of YaesuMode.
    Yaesu2Mode = 0x84,
    /// Slot-1 read all memories. Mirror of YaesuReadMemories.
    Yaesu2ReadMemories = 0x85,
    /// Slot-1 recall memory channel (1-99). Mirror of YaesuRecallMemory.
    Yaesu2RecallMemory = 0x86,
    /// Slot-1 write all memories. Mirror of YaesuWriteMemories.
    Yaesu2WriteMemories = 0x87,
    /// Slot-1 select VFO (0=A, 1=B, 2=swap). Mirror of YaesuSelectVfo.
    Yaesu2SelectVfo = 0x88,
    /// Slot-1 squelch (0-255). Mirror of YaesuSquelch.
    Yaesu2Squelch = 0x89,
    /// Slot-1 RF gain (0-255). Mirror of YaesuRfGain.
    Yaesu2RfGain = 0x8A,
    /// Slot-1 radio mic gain (0-100). Mirror of YaesuRadioMicGain.
    Yaesu2RadioMicGain = 0x8B,
    /// Slot-1 RF power (0-100). Mirror of YaesuRfPower.
    Yaesu2RfPower = 0x8C,
    /// Slot-1 raw CAT button. Mirror of YaesuButton.
    Yaesu2Button = 0x8D,
    /// Slot-1 read all EX menus. Mirror of YaesuReadMenus.
    Yaesu2ReadMenus = 0x8E,
    /// Slot-1 set EX menu item. Mirror of YaesuSetMenu.
    Yaesu2SetMenu = 0x8F,
    /// Thetis TX-EQ on/off (client→server; CAT `ZZET` via TCI `run_cat_ex`).
    /// value 0 = TXEQ off, 1 = on. Used to bypass the mic-profile TX-EQ during
    /// WAV-playback to the main radio (off on Play-start, restore on stop),
    /// mirroring Thetis' own record/playback behaviour.
    ThetisTxeq = 0x90,
    /// RX1 audio subscription (value 0/1). Default ON at the server, so old
    /// clients that never send this keep getting RX1 audio. A client that
    /// uses only VRX sets this to 0 to stop the RX1 audio stream
    /// (save bandwidth). REFACTOR-audio-spectrum-per-channel phase 3.
    Rx1Enable = 0x91,
    /// Yaesu slot-0 STATE subscription (value 0/1), SEPARATE from the audio. Client sets this
    /// on when the control window is open: then freq/s-meter/CAT/feature keep running
    /// even with the audio (YaesuEnable) off = mute with a live window. Default OFF; audio
    /// subscribers get state anyway (state_addrs = state_enabled || yaesu_enabled).
    YaesuStateEnable = 0x92,
    /// Slot-1 mirror of YaesuStateEnable.
    Yaesu2StateEnable = 0x93,
    /// Yaesu radio power on/off (value 0/1 -> CAT `PS1`/`PS0`). Turns the radio
    /// fully on or off (no separate standby on Yaesu).
    YaesuPowerOnOff = 0x94,
    Yaesu2PowerOnOff = 0x95,
    /// Diagnostic: log every CAT frame from the Yaesu radios verbatim and ask
    /// them to report changes they make themselves (Auto Information). Value
    /// 1 = on, 0 = off; applies to both radio slots. For finding out what a
    /// radio actually sends when a control is used on its front panel.
    YaesuCatMonitor = 0x96,
    /// Read the CTCSS/DCS tone of every memory channel that has a tone mode.
    /// The tone is a current-channel setting, so the radio has to step through
    /// those channels - explicit action, never part of the ordinary read.
    /// Value = slot (0 = radio 1, 1 = radio 2).
    YaesuReadMemoryTones = 0x97,
    /// Send the full-DDC spectrum row alongside the extracted view (1 = on,
    /// 0 = off). One such row per receiver chain, shared by the RX window and
    /// the VRX riding that same DDC: it is what keeps a waterfall history filled
    /// after tuning or zooming, and what holds the RX plot steady during fast
    /// tuning. Off, every waterfall follows its own view at roughly half the
    /// spectrum bandwidth. Default on (server-side default too, so an old client
    /// keeps what it had).
    FullSpectrumEnabled = 0x98,
    /// VRX1 spectrum pan offset from the listening frequency, in units of
    /// 10 Hz, as an i16 carried in the u16 value (client -> server).
    ///
    /// The server cuts the VRX window around the listening frequency. Without
    /// this the client could only pan inside the window it had already been
    /// sent, which is one screen wide - so panning barely moved, while the
    /// main spectrum pans across the whole receiver. With it the server cuts
    /// the window where the operator is looking and the two behave alike.
    ///
    /// Ten hertz per step reaches +/-327 kHz, which covers half of any DDC
    /// this runs on, and is finer than a pixel at every zoom that exists here.
    /// A server that does not know this control ignores it and keeps
    /// centring on the listening frequency, which is what it did before.
    VrxSpectrumPan = 0x99,
    /// VRX2 spectrum pan offset. Same units and reasoning.
    VrxSpectrumPan2 = 0x9A,
}

/// Hertz per step of [`ControlId::VrxSpectrumPan`].
pub const VRX_PAN_STEP_HZ: i32 = 10;

/// Pack a pan offset in Hz into the control value, clamped to what fits.
pub fn pack_vrx_pan(offset_hz: i32) -> u16 {
    let steps = (offset_hz / VRX_PAN_STEP_HZ).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    steps as u16
}

/// Recover the pan offset in Hz from the control value.
pub fn unpack_vrx_pan(value: u16) -> i32 {
    (value as i16) as i32 * VRX_PAN_STEP_HZ
}

impl ControlId {
    pub fn from_u8(v: u8) -> Option<Self> {
        Self::try_from(v).ok()
    }
}

// ControlId::from_u8 and PacketType::from_u8 use num_enum TryFromPrimitive derive.
// Manual match blocks removed — the #[repr(u8)] values are the single source of truth.

/// Capability flags for protocol negotiation (bitfield)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities(pub u32);

impl Capabilities {
    pub const NONE: Self = Self(0);
    /// Support for 16kHz wideband Opus audio
    pub const WIDEBAND_AUDIO: u32 = 1 << 0;
    /// Support for spectrum/waterfall data
    pub const SPECTRUM: u32 = 1 << 1;
    /// Support for RX2/VFO-B dual receiver
    pub const RX2: u32 = 1 << 2;
    /// Server reports runtime state via the `state_flags` field in HeartbeatAck
    /// (PATCH-1). Client MUST check this bit before trusting `state_flags`;
    /// older servers (v2.0.0 release-tag and earlier) leave `state_flags` at
    /// NONE which would otherwise be misread as "TCI down". When this bit is
    /// clear in the negotiated capabilities, the client treats `state_flags`
    /// as unknown / not-authoritative.
    pub const REPORTS_STATE_FLAGS: u32 = 1 << 3;

    pub fn has(self, flag: u32) -> bool {
        self.0 & flag != 0
    }

    pub fn with(self, flag: u32) -> Self {
        Self(self.0 | flag)
    }

    /// Return the intersection of two capability sets (features both sides support)
    pub fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
}

/// Flags byte
///   bit 0 = PTT active
///   bit 1 = AUDIO_WIDEBAND — Audio/AudioTx/AudioRx2 payload is wideband
///           Opus (16 kHz), clear = narrowband (8 kHz). Applies only to
///           Thetis-audio packets; AudioYaesu is always wideband and ignores
///           the flag. Backwards-compat: old implementations (without
///           wideband-cap) see this bit as 0 and keep sending NB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flags(pub u8);

impl Flags {
    pub const NONE: Self = Self(0);
    pub const PTT: Self = Self(0x01);
    pub const AUDIO_WIDEBAND: Self = Self(0x02);

    pub fn ptt(self) -> bool {
        self.0 & 0x01 != 0
    }

    pub fn with_ptt(self, ptt: bool) -> Self {
        if ptt {
            Self(self.0 | 0x01)
        } else {
            Self(self.0 & !0x01)
        }
    }

    pub fn wideband(self) -> bool {
        self.0 & 0x02 != 0
    }

    pub fn with_wideband(self, wideband: bool) -> Self {
        if wideband {
            Self(self.0 | 0x02)
        } else {
            Self(self.0 & !0x02)
        }
    }
}

/// Common 4-byte header: magic, version, packet_type, flags
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub packet_type: PacketType,
    pub flags: Flags,
}

impl Header {
    pub const SIZE: usize = 4;

    pub fn new(packet_type: PacketType, flags: Flags) -> Self {
        Self { packet_type, flags }
    }

    pub fn serialize(&self, buf: &mut [u8]) {
        debug_assert!(buf.len() >= Self::SIZE);
        buf[0] = MAGIC;
        buf[1] = VERSION;
        buf[2] = self.packet_type as u8;
        buf[3] = self.flags.0;
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        if buf.len() < Self::SIZE {
            bail!("packet too short for header: {} bytes", buf.len());
        }
        if buf[0] != MAGIC {
            bail!("invalid magic byte: 0x{:02X}", buf[0]);
        }
        if buf[1] != VERSION {
            bail!("unsupported version: {}", buf[1]);
        }
        let packet_type = PacketType::from_u8(buf[2])
            .ok_or_else(|| anyhow::anyhow!("unknown packet type: 0x{:02X}", buf[2]))?;
        let flags = Flags(buf[3]);
        Ok(Self { packet_type, flags })
    }
}

/// Audio packet: header(4) + sequence(4) + timestamp(4) + opus_len(2) + opus_data(N)
/// Total: 14 + N bytes
#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub flags: Flags,
    pub sequence: u32,
    pub timestamp: u32,
    pub opus_data: Vec<u8>,
}

impl AudioPacket {
    pub const HEADER_SIZE: usize = Header::SIZE + 4 + 4 + 2; // 14 bytes

    pub fn serialize(&self, buf: &mut Vec<u8>) {
        self.serialize_as_type(buf, PacketType::Audio);
    }

    pub fn serialize_as_type(&self, buf: &mut Vec<u8>, ptype: PacketType) {
        let start = buf.len();
        buf.resize(start + Self::HEADER_SIZE + self.opus_data.len(), 0);
        let out = &mut buf[start..];

        let header = Header::new(ptype, self.flags);
        header.serialize(out);
        out[4..8].copy_from_slice(&self.sequence.to_be_bytes());
        out[8..12].copy_from_slice(&self.timestamp.to_be_bytes());
        out[12..14].copy_from_slice(&(self.opus_data.len() as u16).to_be_bytes());
        out[14..14 + self.opus_data.len()].copy_from_slice(&self.opus_data);
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        let header = Header::deserialize(buf)?;
        if header.packet_type != PacketType::Audio && header.packet_type != PacketType::AudioRx2
            && header.packet_type != PacketType::AudioYaesu && header.packet_type != PacketType::AudioBinR
            && header.packet_type != PacketType::AudioYaesu2 {
            bail!("expected Audio packet, got {:?}", header.packet_type);
        }
        if buf.len() < Self::HEADER_SIZE {
            bail!(
                "audio packet too short: {} < {}",
                buf.len(),
                Self::HEADER_SIZE
            );
        }

        let sequence = u32::from_be_bytes(buf[4..8].try_into().unwrap());
        let timestamp = u32::from_be_bytes(buf[8..12].try_into().unwrap());
        let opus_len = u16::from_be_bytes(buf[12..14].try_into().unwrap()) as usize;

        if buf.len() < Self::HEADER_SIZE + opus_len {
            bail!(
                "audio packet truncated: {} < {}",
                buf.len(),
                Self::HEADER_SIZE + opus_len
            );
        }

        let opus_data = buf[14..14 + opus_len].to_vec();

        Ok(Self {
            flags: header.flags,
            sequence,
            timestamp,
            opus_data,
        })
    }
}

/// VRX audio packet: server → client, narrowband Opus audio for one
/// Virtual RX channel. Layout:
///   header(4) + sequence(4) + timestamp(4) + vrx_id(1) + opus_len(2) + opus_data(N)
/// Total: 15 + N bytes.
#[derive(Debug, Clone)]
pub struct VrxAudioPacket {
    pub sequence: u32,
    pub timestamp: u32,
    pub vrx_id: u8,
    pub opus_data: Vec<u8>,
    /// True if Opus payload is 16 kHz wideband. False = 8 kHz narrowband.
    /// Carried in the header's `Flags::AUDIO_WIDEBAND` bit, not in the
    /// body — same convention as the main RX1 audio path.
    pub wideband: bool,
}

impl VrxAudioPacket {
    pub const HEADER_SIZE: usize = Header::SIZE + 4 + 4 + 1 + 2; // 15 bytes

    pub fn serialize(&self, buf: &mut Vec<u8>) {
        let start = buf.len();
        buf.resize(start + Self::HEADER_SIZE + self.opus_data.len(), 0);
        let out = &mut buf[start..];
        let flags = if self.wideband { Flags::AUDIO_WIDEBAND } else { Flags::NONE };
        let header = Header::new(PacketType::AudioVrx, flags);
        header.serialize(out);
        out[4..8].copy_from_slice(&self.sequence.to_be_bytes());
        out[8..12].copy_from_slice(&self.timestamp.to_be_bytes());
        out[12] = self.vrx_id;
        out[13..15].copy_from_slice(&(self.opus_data.len() as u16).to_be_bytes());
        out[15..15 + self.opus_data.len()].copy_from_slice(&self.opus_data);
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        let header = Header::deserialize(buf)?;
        if header.packet_type != PacketType::AudioVrx {
            bail!("expected AudioVrx packet, got {:?}", header.packet_type);
        }
        if buf.len() < Self::HEADER_SIZE {
            bail!("vrx audio packet too short: {} < {}", buf.len(), Self::HEADER_SIZE);
        }
        let sequence = u32::from_be_bytes(buf[4..8].try_into().unwrap());
        let timestamp = u32::from_be_bytes(buf[8..12].try_into().unwrap());
        let vrx_id = buf[12];
        let opus_len = u16::from_be_bytes(buf[13..15].try_into().unwrap()) as usize;
        if buf.len() < Self::HEADER_SIZE + opus_len {
            bail!(
                "vrx audio packet truncated: {} < {}",
                buf.len(),
                Self::HEADER_SIZE + opus_len
            );
        }
        let opus_data = buf[15..15 + opus_len].to_vec();
        let wideband = (header.flags.0 & Flags::AUDIO_WIDEBAND.0) != 0;
        Ok(Self { sequence, timestamp, vrx_id, opus_data, wideband })
    }
}

/// VRX frequency packet (client → server). Sets the absolute listen frequency
/// for one VRX channel. Layout:
///   header(4) + vrx_id(1) + frequency_hz(8) = 13 bytes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VrxFrequencyPacket {
    pub vrx_id: u8,
    pub frequency_hz: u64,
}

impl VrxFrequencyPacket {
    pub const SIZE: usize = Header::SIZE + 1 + 8; // 13 bytes

    pub fn serialize(&self, buf: &mut [u8; Self::SIZE]) {
        self.serialize_with_type(PacketType::FrequencyVrx, buf);
    }

    /// Serialize with an explicit packet type — `FrequencyVrx` (client →
    /// server tune) or `FrequencyVrxActual` (server → client SAM auto-tune
    /// follow). Identical 13-byte layout.
    pub fn serialize_with_type(&self, packet_type: PacketType, buf: &mut [u8; Self::SIZE]) {
        let header = Header::new(packet_type, Flags::NONE);
        header.serialize(buf);
        buf[4] = self.vrx_id;
        buf[5..13].copy_from_slice(&self.frequency_hz.to_be_bytes());
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        let header = Header::deserialize(buf)?;
        if header.packet_type != PacketType::FrequencyVrx
            && header.packet_type != PacketType::FrequencyVrxActual
        {
            bail!("expected FrequencyVrx(Actual) packet, got {:?}", header.packet_type);
        }
        if buf.len() < Self::SIZE {
            bail!("vrx frequency packet too short: {} < {}", buf.len(), Self::SIZE);
        }
        let vrx_id = buf[4];
        let frequency_hz = u64::from_be_bytes(buf[5..13].try_into().unwrap());
        Ok(Self { vrx_id, frequency_hz })
    }
}

/// TX modulation filter band (server → client). Layout:
///   header(4) + low_hz(i32) + high_hz(i32) = 12 bytes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxFilterBandPacket {
    pub low_hz: i32,
    pub high_hz: i32,
}

impl TxFilterBandPacket {
    pub const SIZE: usize = Header::SIZE + 4 + 4; // 12 bytes

    pub fn serialize(&self, buf: &mut [u8; Self::SIZE]) {
        let header = Header::new(PacketType::TxFilterBand, Flags::NONE);
        header.serialize(buf);
        buf[4..8].copy_from_slice(&self.low_hz.to_be_bytes());
        buf[8..12].copy_from_slice(&self.high_hz.to_be_bytes());
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        if buf.len() < Self::SIZE {
            bail!("tx filter band packet too short: {} < {}", buf.len(), Self::SIZE);
        }
        let low_hz = i32::from_be_bytes(buf[4..8].try_into().unwrap());
        let high_hz = i32::from_be_bytes(buf[8..12].try_into().unwrap());
        Ok(Self { low_hz, high_hz })
    }
}

/// Multi-channel audio packet: bundles 1-4 mono Opus frames in one UDP packet.
/// Perfect sync: all channels share one sequence number and timestamp.
///
/// Format:
///   header(4) + sequence(4) + timestamp(4) + channel_count(1) = 13 bytes
///   Per channel: channel_id(1) + opus_len(2) + opus_data(N)
///
/// Channel IDs:
///   0 = RX1 (or RX1-L when binaural)
///   1 = RX1-R (binaural right; absent when BIN off)
///   2 = RX2
///   3 = Yaesu (reserved for future bundling)
#[derive(Debug, Clone)]
pub struct MultiChannelAudioPacket {
    pub sequence: u32,
    pub timestamp: u32,
    pub channels: Vec<(u8, Vec<u8>)>, // (channel_id, opus_data)
    /// Flags byte of the packet header. The most important flag for this
    /// type is `Flags::AUDIO_WIDEBAND` (all channels in this packet
    /// are wideband Opus 16 kHz); absence = narrowband 8 kHz.
    pub flags: Flags,
}

impl MultiChannelAudioPacket {
    pub const HEADER_SIZE: usize = Header::SIZE + 4 + 4 + 1; // 13 bytes

    pub fn serialize(&self, buf: &mut Vec<u8>) {
        let payload_size: usize = self.channels.iter().map(|(_, d)| 1 + 2 + d.len()).sum();
        let start = buf.len();
        buf.resize(start + Self::HEADER_SIZE + payload_size, 0);
        let out = &mut buf[start..];

        let header = Header::new(PacketType::AudioMultiCh, self.flags);
        header.serialize(out);
        out[4..8].copy_from_slice(&self.sequence.to_be_bytes());
        out[8..12].copy_from_slice(&self.timestamp.to_be_bytes());
        out[12] = self.channels.len() as u8;

        let mut pos = Self::HEADER_SIZE;
        for (ch_id, opus_data) in &self.channels {
            out[pos] = *ch_id;
            out[pos + 1..pos + 3].copy_from_slice(&(opus_data.len() as u16).to_be_bytes());
            out[pos + 3..pos + 3 + opus_data.len()].copy_from_slice(opus_data);
            pos += 3 + opus_data.len();
        }
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        let header = Header::deserialize(buf)?;
        if header.packet_type != PacketType::AudioMultiCh {
            bail!("expected AudioMultiCh packet, got {:?}", header.packet_type);
        }
        if buf.len() < Self::HEADER_SIZE {
            bail!("multi-ch audio packet too short: {} < {}", buf.len(), Self::HEADER_SIZE);
        }

        let sequence = u32::from_be_bytes(buf[4..8].try_into().unwrap());
        let timestamp = u32::from_be_bytes(buf[8..12].try_into().unwrap());
        let channel_count = buf[12] as usize;

        let mut channels = Vec::with_capacity(channel_count);
        let mut pos = Self::HEADER_SIZE;
        for _ in 0..channel_count {
            if pos + 3 > buf.len() { break; }
            let ch_id = buf[pos];
            let opus_len = u16::from_be_bytes(buf[pos + 1..pos + 3].try_into().unwrap()) as usize;
            if pos + 3 + opus_len > buf.len() { break; }
            let opus_data = buf[pos + 3..pos + 3 + opus_len].to_vec();
            channels.push((ch_id, opus_data));
            pos += 3 + opus_len;
        }

        Ok(Self { sequence, timestamp, channels, flags: header.flags })
    }
}

/// Heartbeat packet: header(4) + sequence(4) + local_time(4) + rtt(u16) + loss(u8) + jitter(u8) + capabilities(4)
/// Total: 20 bytes (backward compatible: 16 bytes without capabilities)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Heartbeat {
    pub flags: Flags,
    pub sequence: u32,
    /// Local timestamp in milliseconds (wrapping)
    pub local_time: u32,
    /// Last measured RTT in milliseconds
    pub rtt_ms: u16,
    /// Packet loss percentage (0-100)
    pub loss_percent: u8,
    /// Jitter in milliseconds
    pub jitter_ms: u8,
    /// Client capabilities (0 if not present)
    pub capabilities: Capabilities,
}

impl Heartbeat {
    /// Minimum size for backward compatibility (without capabilities)
    pub const MIN_SIZE: usize = 16;
    /// Full size including capabilities
    pub const SIZE: usize = 20;

    pub fn serialize(&self, buf: &mut [u8; Self::SIZE]) {
        let header = Header::new(PacketType::Heartbeat, self.flags);
        header.serialize(buf);
        buf[4..8].copy_from_slice(&self.sequence.to_be_bytes());
        buf[8..12].copy_from_slice(&self.local_time.to_be_bytes());
        buf[12..14].copy_from_slice(&self.rtt_ms.to_be_bytes());
        buf[14] = self.loss_percent;
        buf[15] = self.jitter_ms;
        buf[16..20].copy_from_slice(&self.capabilities.0.to_be_bytes());
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        let header = Header::deserialize(buf)?;
        if header.packet_type != PacketType::Heartbeat {
            bail!("expected Heartbeat packet, got {:?}", header.packet_type);
        }
        if buf.len() < Self::MIN_SIZE {
            bail!("heartbeat too short: {} < {}", buf.len(), Self::MIN_SIZE);
        }

        let capabilities = if buf.len() >= Self::SIZE {
            Capabilities(u32::from_be_bytes(buf[16..20].try_into().unwrap()))
        } else {
            Capabilities::NONE
        };

        Ok(Self {
            flags: header.flags,
            sequence: u32::from_be_bytes(buf[4..8].try_into().unwrap()),
            local_time: u32::from_be_bytes(buf[8..12].try_into().unwrap()),
            rtt_ms: u16::from_be_bytes(buf[12..14].try_into().unwrap()),
            loss_percent: buf[14],
            jitter_ms: buf[15],
            capabilities,
        })
    }
}

/// Server-state flags broadcast in HeartbeatAck. Independent from
/// `Capabilities` (which describes feature negotiation, static); this
/// describes runtime state (dynamic).
///
/// Added in v2.0.0-post (PATCH-1 client-connect-error-feedback) for
/// the client to detect "TCI not reachable on server PC" without
/// inferring from audio absence (false-positives on quiet bands).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerStateFlags(pub u32);

impl ServerStateFlags {
    pub const NONE: Self = Self(0);
    /// Server's TCI WebSocket to Thetis is currently up.
    /// When clear: TCI is down, retrying, or unreachable.
    pub const TCI_CONNECTED: u32 = 1 << 0;
    /// Thetis.exe process is currently running on the server PC.
    /// Used by the client to give a smarter TciUnreachable hint:
    /// - flag set + TCI_CONNECTED clear → "Thetis runs, check TCI server settings"
    /// - flag clear → "Thetis is not running, press Start on the Thetis tab"
    /// Only trustworthy when capabilities advertises REPORTS_STATE_FLAGS.
    pub const THETIS_RUNNING: u32 = 1 << 1;
    /// Server is in the middle of launching Thetis (orange "Start" button).
    /// Set between user pressing Start and TCI connecting (or the 60s
    /// launch timeout firing). Client should NOT show "TCI unreachable"
    /// while this is set — it's a normal transient launch phase.
    pub const THETIS_STARTING: u32 = 1 << 2;
    /// The server has a Thetis/TCI radio configured. When this is clear,
    /// clients must not turn a missing TCI connection into a user-facing
    /// "Radio not reachable" warning; Yaesu-only installs are valid.
    pub const THETIS_CONFIGURED: u32 = 1 << 3;
    /// The configured Thetis radio hardware has only ONE receiver (no RX2).
    /// Set by the server operator when the radio model is single-receiver.
    /// **Inverted on purpose**: absent = the normal 2-receiver default, so
    /// older servers (which never set this) and the default config both keep
    /// RX2/VRX2 visible. Only when POSITIVELY set do clients hide RX2 + VRX2
    /// everywhere. Only trustworthy when capabilities advertises REPORTS_STATE_FLAGS.
    pub const SINGLE_RECEIVER: u32 = 1 << 4;
    /// This server has NO DX cluster configured - no callsign, or the cluster
    /// switched off - so no spot will ever arrive from it.
    ///
    /// **Inverted, like `SINGLE_RECEIVER` and for the same reason**: absent
    /// means "a cluster, as far as you know", which is what every older server
    /// implies by never setting it. Only when POSITIVELY set do clients drop the
    /// DX-spot subscription and hide its toggle - a tick offering spots that
    /// cannot come is a promise the station cannot keep.
    pub const NO_DX_CLUSTER: u32 = 1 << 5;

    pub fn has(self, flag: u32) -> bool {
        self.0 & flag != 0
    }

    pub fn with(self, flag: u32) -> Self {
        Self(self.0 | flag)
    }
}

/// HeartbeatAck: header(4) + echo_seq(4) + echo_time(4) + capabilities(4) + state_flags(4) = 20 bytes
/// Backward compatible: 12 bytes (just header+echo) or 16 bytes (+capabilities) accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeartbeatAck {
    pub flags: Flags,
    pub echo_sequence: u32,
    pub echo_time: u32,
    /// Server capabilities (negotiated: intersection of client + server caps)
    pub capabilities: Capabilities,
    /// Server runtime-state flags (e.g. TCI_CONNECTED). Forward-compat:
    /// older servers don't send this; client treats absent as `NONE`.
    pub state_flags: ServerStateFlags,
    /// What the server believes this client is subscribed to, as a bitmask.
    ///
    /// Not a command and not an echo of one: it is the server's own answer to
    /// "what do I think you want", carried on a packet that already goes out
    /// every second. It exists because the client's wishes are restored on a
    /// transition - and a transition can be missed, over a transport with no
    /// acknowledgement, by a UI that samples it. What comes back after such a
    /// miss is exactly what the server switches on by itself, and what stays
    /// silent is exactly what the client should have asked for again.
    ///
    /// `None` where the server is older than this field. Absent is not the
    /// same as "nothing subscribed", and reading it as such would report a
    /// disagreement with every old server.
    pub subs: Option<SubscriptionMask>,
}

/// The subscriptions a client can hold, as the server sees them.
///
/// Kept as a mask rather than a struct on the wire because it rides along on an
/// existing packet: two bytes, no extra round trip, nothing on the hot path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SubscriptionMask(pub u16);

impl SubscriptionMask {
    pub const RX1_AUDIO: u16 = 1 << 0;
    pub const RX2_AUDIO: u16 = 1 << 1;
    pub const RX1_SPECTRUM: u16 = 1 << 2;
    pub const RX2_SPECTRUM: u16 = 1 << 3;
    pub const VRX1: u16 = 1 << 4;
    pub const VRX2: u16 = 1 << 5;
    pub const VRX1_SPECTRUM: u16 = 1 << 6;
    pub const VRX2_SPECTRUM: u16 = 1 << 7;
    pub const YAESU: u16 = 1 << 8;
    pub const YAESU2: u16 = 1 << 9;
    pub const FULL_SPECTRUM: u16 = 1 << 10;
    pub const DX_SPOTS: u16 = 1 << 11;

    pub fn has(&self, bit: u16) -> bool {
        self.0 & bit != 0
    }

    pub fn set(&mut self, bit: u16, on: bool) {
        if on {
            self.0 |= bit;
        } else {
            self.0 &= !bit;
        }
    }

    /// The bits that differ, for a line that says what disagrees rather than
    /// that something does.
    pub fn names_of(bits: u16) -> Vec<&'static str> {
        const NAMED: &[(u16, &str)] = &[
            (SubscriptionMask::RX1_AUDIO, "rx1-audio"),
            (SubscriptionMask::RX2_AUDIO, "rx2-audio"),
            (SubscriptionMask::RX1_SPECTRUM, "rx1-spectrum"),
            (SubscriptionMask::RX2_SPECTRUM, "rx2-spectrum"),
            (SubscriptionMask::VRX1, "vrx1"),
            (SubscriptionMask::VRX2, "vrx2"),
            (SubscriptionMask::VRX1_SPECTRUM, "vrx1-spectrum"),
            (SubscriptionMask::VRX2_SPECTRUM, "vrx2-spectrum"),
            (SubscriptionMask::YAESU, "yaesu"),
            (SubscriptionMask::YAESU2, "yaesu2"),
            (SubscriptionMask::FULL_SPECTRUM, "full-spectrum"),
            (SubscriptionMask::DX_SPOTS, "dx-spots"),
        ];
        NAMED.iter().filter(|(b, _)| bits & b != 0).map(|(_, n)| *n).collect()
    }
}

impl HeartbeatAck {
    /// Minimum size for backward compatibility (without capabilities or state_flags)
    pub const MIN_SIZE: usize = 12;
    /// Size including capabilities (v0.x .. v2.0.0 release)
    pub const SIZE_WITH_CAPS: usize = 16;
    /// Full size including state_flags (v2.0.0-post PATCH-1)
    pub const SIZE: usize = 20;
    /// With the subscription mask (build 99). Same forward-compatible shape as
    /// the two growths before it: an older server simply stops at 20 bytes.
    pub const SIZE_WITH_SUBS: usize = 22;

    pub fn serialize(&self, buf: &mut [u8; Self::SIZE]) {
        let header = Header::new(PacketType::HeartbeatAck, self.flags);
        header.serialize(buf);
        buf[4..8].copy_from_slice(&self.echo_sequence.to_be_bytes());
        buf[8..12].copy_from_slice(&self.echo_time.to_be_bytes());
        buf[12..16].copy_from_slice(&self.capabilities.0.to_be_bytes());
        buf[16..20].copy_from_slice(&self.state_flags.0.to_be_bytes());
    }

    /// Serialise including the subscription mask. Separate from `serialize`
    /// so the older, shorter form stays exactly what it was.
    pub fn serialize_with_subs(&self, buf: &mut [u8; Self::SIZE_WITH_SUBS]) {
        let mut head = [0u8; Self::SIZE];
        self.serialize(&mut head);
        buf[..Self::SIZE].copy_from_slice(&head);
        let mask = self.subs.unwrap_or_default().0;
        buf[20..22].copy_from_slice(&mask.to_be_bytes());
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        let header = Header::deserialize(buf)?;
        if header.packet_type != PacketType::HeartbeatAck {
            bail!("expected HeartbeatAck, got {:?}", header.packet_type);
        }
        if buf.len() < Self::MIN_SIZE {
            bail!("heartbeat ack too short: {} < {}", buf.len(), Self::MIN_SIZE);
        }

        let capabilities = if buf.len() >= Self::SIZE_WITH_CAPS {
            Capabilities(u32::from_be_bytes(buf[12..16].try_into().unwrap()))
        } else {
            Capabilities::NONE
        };

        let state_flags = if buf.len() >= Self::SIZE {
            ServerStateFlags(u32::from_be_bytes(buf[16..20].try_into().unwrap()))
        } else {
            ServerStateFlags::NONE
        };

        // Absent on a server older than this field, and absent is NOT "nothing
        // subscribed": reading it that way would report a disagreement against
        // every older server, every second.
        let subs = if buf.len() >= Self::SIZE_WITH_SUBS {
            Some(SubscriptionMask(u16::from_be_bytes(buf[20..22].try_into().unwrap())))
        } else {
            None
        };

        Ok(Self {
            flags: header.flags,
            echo_sequence: u32::from_be_bytes(buf[4..8].try_into().unwrap()),
            echo_time: u32::from_be_bytes(buf[8..12].try_into().unwrap()),
            capabilities,
            state_flags,
            subs,
        })
    }
}

/// Control packet: header(4) + control_id(1) + value(2) = 7 bytes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlPacket {
    pub control_id: ControlId,
    pub value: u16,
}

impl ControlPacket {
    pub const SIZE: usize = 7;

    pub fn serialize(&self, buf: &mut [u8; Self::SIZE]) {
        let header = Header::new(PacketType::Control, Flags::NONE);
        header.serialize(buf);
        buf[4] = self.control_id as u8;
        buf[5..7].copy_from_slice(&self.value.to_be_bytes());
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        let header = Header::deserialize(buf)?;
        if header.packet_type != PacketType::Control {
            bail!("expected Control packet, got {:?}", header.packet_type);
        }
        if buf.len() < Self::SIZE {
            bail!("control packet too short: {} < {}", buf.len(), Self::SIZE);
        }
        let control_id = ControlId::from_u8(buf[4])
            .ok_or_else(|| anyhow::anyhow!("unknown control id: 0x{:02X}", buf[4]))?;
        let value = u16::from_be_bytes(buf[5..7].try_into().unwrap());
        Ok(Self { control_id, value })
    }
}

/// Disconnect packet: just a header (4 bytes)
pub struct DisconnectPacket;

impl DisconnectPacket {
    pub const SIZE: usize = Header::SIZE;

    pub fn serialize(buf: &mut [u8; Self::SIZE]) {
        let header = Header::new(PacketType::Disconnect, Flags::NONE);
        header.serialize(buf);
    }
}

/// PTT denied packet: just a header (4 bytes)
/// Sent by server when a client's PTT request is rejected because another client holds TX.
pub struct PttDeniedPacket;

impl PttDeniedPacket {
    pub const SIZE: usize = Header::SIZE;

    pub fn serialize(buf: &mut [u8; Self::SIZE]) {
        let header = Header::new(PacketType::PttDenied, Flags::NONE);
        header.serialize(buf);
    }
}

/// Frequency packet: header(4) + frequency_hz(8) = 12 bytes
/// Bidirectional: server→client (readback) and client→server (set)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrequencyPacket {
    pub frequency_hz: u64,
}

impl FrequencyPacket {
    pub const SIZE: usize = 12;

    pub fn serialize(&self, buf: &mut [u8; Self::SIZE]) {
        self.serialize_as_type(buf, PacketType::Frequency);
    }

    pub fn serialize_as_type(&self, buf: &mut [u8; Self::SIZE], ptype: PacketType) {
        let header = Header::new(ptype, Flags::NONE);
        header.serialize(buf);
        buf[4..12].copy_from_slice(&self.frequency_hz.to_be_bytes());
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        let header = Header::deserialize(buf)?;
        if header.packet_type != PacketType::Frequency && header.packet_type != PacketType::FrequencyRx2 && header.packet_type != PacketType::FrequencyYaesu && header.packet_type != PacketType::FrequencyYaesu2 {
            bail!("expected Frequency/FrequencyRx2 packet, got {:?}", header.packet_type);
        }
        if buf.len() < Self::SIZE {
            bail!("frequency packet too short: {} < {}", buf.len(), Self::SIZE);
        }
        let frequency_hz = u64::from_be_bytes(buf[4..12].try_into().unwrap());
        Ok(Self { frequency_hz })
    }
}

/// Mode packet: header(4) + mode(1) = 5 bytes
/// Bidirectional: server→client (readback) and client→server (set)
/// Mode values: 00=LSB, 01=USB, 05=FM, 06=AM (Thetis ZZMD values)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModePacket {
    pub mode: u8,
}

impl ModePacket {
    pub const SIZE: usize = 5;

    pub fn serialize(&self, buf: &mut [u8; Self::SIZE]) {
        self.serialize_as_type(buf, PacketType::Mode);
    }

    pub fn serialize_as_type(&self, buf: &mut [u8; Self::SIZE], ptype: PacketType) {
        let header = Header::new(ptype, Flags::NONE);
        header.serialize(buf);
        buf[4] = self.mode;
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        let header = Header::deserialize(buf)?;
        if header.packet_type != PacketType::Mode && header.packet_type != PacketType::ModeRx2 {
            bail!("expected Mode/ModeRx2 packet, got {:?}", header.packet_type);
        }
        if buf.len() < Self::SIZE {
            bail!("mode packet too short: {} < {}", buf.len(), Self::SIZE);
        }
        Ok(Self { mode: buf[4] })
    }
}

/// S-meter packet: header(4) + level(2) = 6 bytes
/// Server→client only. Level is signed deci-units:
///  - `Flags::PTT` clear (RX): dBm × 10 (typically negative, e.g. -730 = -73 dBm = S9).
///  - `Flags::PTT` set (TX): watts × 10 (positive, e.g. 1000 = 100.0 W FWD).
/// The client owns all display-scale math; the wire format carries the raw
/// physical quantity so changes to the visual scale never break wire compat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmeterPacket {
    pub level: i16,
    pub flags: Flags,
}

impl SmeterPacket {
    pub const SIZE: usize = 6;

    pub fn serialize(&self, buf: &mut [u8; Self::SIZE]) {
        self.serialize_as_type(buf, PacketType::Smeter);
    }

    pub fn serialize_as_type(&self, buf: &mut [u8; Self::SIZE], ptype: PacketType) {
        let header = Header::new(ptype, self.flags);
        header.serialize(buf);
        buf[4..6].copy_from_slice(&self.level.to_be_bytes());
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        let header = Header::deserialize(buf)?;
        if !matches!(
            header.packet_type,
            PacketType::Smeter
                | PacketType::SmeterRx2
                | PacketType::SmeterSig
                | PacketType::SmeterMaxBin
                | PacketType::SmeterRx2Sig
                | PacketType::SmeterRx2MaxBin
        ) {
            bail!("expected Smeter packet, got {:?}", header.packet_type);
        }
        if buf.len() < Self::SIZE {
            bail!("smeter packet too short: {} < {}", buf.len(), Self::SIZE);
        }
        let level = i16::from_be_bytes(buf[4..6].try_into().unwrap());
        Ok(Self { level, flags: header.flags })
    }
}

/// Spectrum packet: header(4) + sequence(2) + num_bins(2) + center_freq_hz(4) + span_hz(4) + ref_level(1) + db_per_unit(1) + bins(N×B)
/// db_per_unit encodes bin byte width: 1 = u8 bins (0-255), 2 = u16 bins (0-65535).
/// Bins internally stored as Vec<u16>; u8 packets are upscaled ×257 on receive.
#[derive(Debug, Clone)]
pub struct SpectrumPacket {
    pub sequence: u16,
    pub num_bins: u16,
    pub center_freq_hz: u32,
    pub span_hz: u32,
    pub ref_level: i8,
    pub db_per_unit: u8,  // 1 = u8 bins on wire, 2 = u16 bins on wire
    pub bins: Vec<u16>,
}

impl SpectrumPacket {
    pub const HEADER_SIZE: usize = Header::SIZE + 2 + 2 + 4 + 4 + 1 + 1; // 18 bytes

    pub fn serialize(&self, buf: &mut Vec<u8>) {
        self.serialize_as_type(buf, PacketType::Spectrum);
    }

    pub fn serialize_as_type(&self, buf: &mut Vec<u8>, ptype: PacketType) {
        let start = buf.len();
        let bytes_per_bin = if self.db_per_unit == 2 { 2 } else { 1 };
        let bin_bytes = self.bins.len() * bytes_per_bin;
        buf.resize(start + Self::HEADER_SIZE + bin_bytes, 0);
        let out = &mut buf[start..];

        let header = Header::new(ptype, Flags::NONE);
        header.serialize(out);
        out[4..6].copy_from_slice(&self.sequence.to_be_bytes());
        out[6..8].copy_from_slice(&self.num_bins.to_be_bytes());
        out[8..12].copy_from_slice(&self.center_freq_hz.to_be_bytes());
        out[12..16].copy_from_slice(&self.span_hz.to_be_bytes());
        out[16] = self.ref_level as u8;
        out[17] = self.db_per_unit;
        if self.db_per_unit == 2 {
            // u16 bins: 2 bytes each, big-endian
            for (i, &val) in self.bins.iter().enumerate() {
                let offset = Self::HEADER_SIZE + i * 2;
                out[offset..offset + 2].copy_from_slice(&val.to_be_bytes());
            }
        } else {
            // u8 bins: 1 byte each (downscale from u16 → u8)
            for (i, &val) in self.bins.iter().enumerate() {
                out[Self::HEADER_SIZE + i] = (val >> 8) as u8;
            }
        }
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        let header = Header::deserialize(buf)?;
        if !matches!(header.packet_type, PacketType::Spectrum | PacketType::FullSpectrum | PacketType::SpectrumRx2 | PacketType::FullSpectrumRx2 | PacketType::SpectrumVrx1 | PacketType::SpectrumVrx2) {
            bail!("expected Spectrum packet variant, got {:?}", header.packet_type);
        }
        if buf.len() < Self::HEADER_SIZE {
            bail!(
                "spectrum packet too short: {} < {}",
                buf.len(),
                Self::HEADER_SIZE
            );
        }

        let sequence = u16::from_be_bytes(buf[4..6].try_into().unwrap());
        let num_bins = u16::from_be_bytes(buf[6..8].try_into().unwrap());
        let center_freq_hz = u32::from_be_bytes(buf[8..12].try_into().unwrap());
        let span_hz = u32::from_be_bytes(buf[12..16].try_into().unwrap());
        let ref_level = buf[16] as i8;
        let db_per_unit = buf[17];

        let bytes_per_bin = if db_per_unit == 2 { 2 } else { 1 };
        let expected_len = Self::HEADER_SIZE + num_bins as usize * bytes_per_bin;
        if buf.len() < expected_len {
            bail!(
                "spectrum packet truncated: {} < {}",
                buf.len(),
                expected_len
            );
        }

        let bins: Vec<u16> = if db_per_unit == 2 {
            // u16 bins: 2 bytes each
            (0..num_bins as usize)
                .map(|i| {
                    let offset = Self::HEADER_SIZE + i * 2;
                    u16::from_be_bytes(buf[offset..offset + 2].try_into().unwrap())
                })
                .collect()
        } else {
            // u8 bins: upscale to u16 (×257 maps 0-255 → 0-65535)
            (0..num_bins as usize)
                .map(|i| {
                    let v = buf[Self::HEADER_SIZE + i] as u16;
                    v | (v << 8)  // equivalent to v * 257
                })
                .collect()
        };

        Ok(Self {
            sequence,
            num_bins,
            center_freq_hz,
            span_hz,
            ref_level,
            db_per_unit,
            bins,
        })
    }
}

/// DX Cluster spot packet: header(4) + callsign_len(1) + callsign + freq_hz(8) + mode_len(1) + mode + spotter_len(1) + spotter + comment_len(1) + comment + age_seconds(2)
#[derive(Debug, Clone)]
pub struct SpotPacket {
    pub callsign: String,
    pub frequency_hz: u64,
    pub mode: String,
    pub spotter: String,
    pub comment: String,
    pub age_seconds: u16,
    /// Total spot lifetime in seconds (from config, e.g. 600 = 10 min)
    pub expiry_seconds: u16,
}

impl SpotPacket {
    pub fn serialize(&self, buf: &mut Vec<u8>) {
        let callsign_bytes = self.callsign.as_bytes();
        let mode_bytes = self.mode.as_bytes();
        let spotter_bytes = self.spotter.as_bytes();
        let comment_bytes = self.comment.as_bytes();
        let total = Header::SIZE + 1 + callsign_bytes.len() + 8 + 1 + mode_bytes.len()
            + 1 + spotter_bytes.len() + 1 + comment_bytes.len() + 2 + 2;

        let start = buf.len();
        buf.resize(start + total, 0);
        let out = &mut buf[start..];

        let header = Header::new(PacketType::Spot, Flags::NONE);
        header.serialize(out);
        let mut pos = Header::SIZE;

        out[pos] = callsign_bytes.len() as u8;
        pos += 1;
        out[pos..pos + callsign_bytes.len()].copy_from_slice(callsign_bytes);
        pos += callsign_bytes.len();

        out[pos..pos + 8].copy_from_slice(&self.frequency_hz.to_be_bytes());
        pos += 8;

        out[pos] = mode_bytes.len() as u8;
        pos += 1;
        out[pos..pos + mode_bytes.len()].copy_from_slice(mode_bytes);
        pos += mode_bytes.len();

        out[pos] = spotter_bytes.len() as u8;
        pos += 1;
        out[pos..pos + spotter_bytes.len()].copy_from_slice(spotter_bytes);
        pos += spotter_bytes.len();

        out[pos] = comment_bytes.len() as u8;
        pos += 1;
        out[pos..pos + comment_bytes.len()].copy_from_slice(comment_bytes);
        pos += comment_bytes.len();

        out[pos..pos + 2].copy_from_slice(&self.age_seconds.to_be_bytes());
        pos += 2;
        out[pos..pos + 2].copy_from_slice(&self.expiry_seconds.to_be_bytes());
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        let header = Header::deserialize(buf)?;
        if header.packet_type != PacketType::Spot {
            bail!("expected Spot packet, got {:?}", header.packet_type);
        }
        let mut pos = Header::SIZE;

        if pos >= buf.len() { bail!("spot packet truncated at callsign_len"); }
        let callsign_len = buf[pos] as usize;
        pos += 1;
        if pos + callsign_len > buf.len() { bail!("spot packet truncated at callsign"); }
        let callsign = String::from_utf8_lossy(&buf[pos..pos + callsign_len]).to_string();
        pos += callsign_len;

        if pos + 8 > buf.len() { bail!("spot packet truncated at freq"); }
        let frequency_hz = u64::from_be_bytes(buf[pos..pos + 8].try_into().unwrap());
        pos += 8;

        if pos >= buf.len() { bail!("spot packet truncated at mode_len"); }
        let mode_len = buf[pos] as usize;
        pos += 1;
        if pos + mode_len > buf.len() { bail!("spot packet truncated at mode"); }
        let mode = String::from_utf8_lossy(&buf[pos..pos + mode_len]).to_string();
        pos += mode_len;

        if pos >= buf.len() { bail!("spot packet truncated at spotter_len"); }
        let spotter_len = buf[pos] as usize;
        pos += 1;
        if pos + spotter_len > buf.len() { bail!("spot packet truncated at spotter"); }
        let spotter = String::from_utf8_lossy(&buf[pos..pos + spotter_len]).to_string();
        pos += spotter_len;

        if pos >= buf.len() { bail!("spot packet truncated at comment_len"); }
        let comment_len = buf[pos] as usize;
        pos += 1;
        if pos + comment_len > buf.len() { bail!("spot packet truncated at comment"); }
        let comment = String::from_utf8_lossy(&buf[pos..pos + comment_len]).to_string();
        pos += comment_len;

        if pos + 2 > buf.len() { bail!("spot packet truncated at age"); }
        let age_seconds = u16::from_be_bytes(buf[pos..pos + 2].try_into().unwrap());
        pos += 2;

        // expiry_seconds: optional for backward compat (default 600 = 10 min)
        let expiry_seconds = if pos + 2 <= buf.len() {
            u16::from_be_bytes(buf[pos..pos + 2].try_into().unwrap())
        } else {
            600
        };

        Ok(Self { callsign, frequency_hz, mode, spotter, comment, age_seconds, expiry_seconds })
    }
}

/// Device type identifiers for external equipment
#[derive(Debug, Clone, Copy, PartialEq, Eq, TryFromPrimitive)]
#[repr(u8)]
pub enum DeviceType {
    Amplitec6x2 = 0x01,
    Tuner = 0x02,
    SpeExpert = 0x03,
    Rf2k = 0x04,
    UltraBeam = 0x05,
    Rotor = 0x06,
    RemoteServer = 0x07,
}

impl DeviceType {
    pub fn from_u8(v: u8) -> Option<Self> {
        Self::try_from(v).ok()
    }
}

/// Remote server command IDs (client → server via EquipmentCommand)
pub const CMD_SERVER_REBOOT: u8 = 0x01;
pub const CMD_SERVER_SHUTDOWN: u8 = 0x02;

/// Amplitec 6/2 command IDs (client → server via EquipmentCommand).
/// Switching the switch position has no const — for that there are
/// `EquipmentCommandPacket::CMD_SET_SWITCH_A/_B` (pre-existing).
/// For the power-cap table: 18 bytes data
/// (6 × { u16 max_w BE, u8 tx_blocked }).
pub const CMD_AMPLITEC_SET_POWER_TABLE: u8 = 0x10;

/// Tuner command IDs (client → server via EquipmentCommand)
pub const CMD_TUNE_START: u8 = 0x01;
pub const CMD_TUNE_ABORT: u8 = 0x02;

/// SPE Expert command IDs (client → server via EquipmentCommand)
pub const CMD_SPE_OPERATE: u8 = 0x01;
pub const CMD_SPE_TUNE: u8 = 0x02;
pub const CMD_SPE_ANTENNA: u8 = 0x03;
pub const CMD_SPE_INPUT: u8 = 0x04;
pub const CMD_SPE_POWER: u8 = 0x05;
pub const CMD_SPE_BAND_UP: u8 = 0x06;
pub const CMD_SPE_BAND_DOWN: u8 = 0x07;
pub const CMD_SPE_OFF: u8 = 0x08;
pub const CMD_SPE_POWER_ON: u8 = 0x09;
pub const CMD_SPE_DRIVE_DOWN: u8 = 0x0A;
pub const CMD_SPE_DRIVE_UP: u8 = 0x0B;

/// RF2K-S command IDs (client → server via EquipmentCommand)
pub const CMD_RF2K_OPERATE: u8 = 0x01;
pub const CMD_RF2K_TUNE: u8 = 0x02;
pub const CMD_RF2K_ANT1: u8 = 0x03;
pub const CMD_RF2K_ANT2: u8 = 0x04;
pub const CMD_RF2K_ANT3: u8 = 0x05;
pub const CMD_RF2K_ANT4: u8 = 0x06;
pub const CMD_RF2K_ANT_EXT: u8 = 0x07;
pub const CMD_RF2K_ERROR_RESET: u8 = 0x08;
pub const CMD_RF2K_CLOSE: u8 = 0x09;
// Tuner controls (Phase B)
pub const CMD_RF2K_TUNER_MODE: u8 = 0x10;
pub const CMD_RF2K_TUNER_BYPASS: u8 = 0x11;
pub const CMD_RF2K_TUNER_RESET: u8 = 0x12;
pub const CMD_RF2K_TUNER_STORE: u8 = 0x13;
pub const CMD_RF2K_TUNER_L_UP: u8 = 0x14;
pub const CMD_RF2K_TUNER_L_DOWN: u8 = 0x15;
pub const CMD_RF2K_TUNER_C_UP: u8 = 0x16;
pub const CMD_RF2K_TUNER_C_DOWN: u8 = 0x17;
pub const CMD_RF2K_TUNER_K: u8 = 0x18;
// Drive controls (Phase C)
pub const CMD_RF2K_DRIVE_UP: u8 = 0x20;
pub const CMD_RF2K_DRIVE_DOWN: u8 = 0x21;
// Debug controls (Phase D)
pub const CMD_RF2K_SET_HIGH_POWER: u8 = 0x30;   // data[0]: 0/1
pub const CMD_RF2K_SET_TUNER_6M: u8 = 0x31;     // data[0]: 0/1
pub const CMD_RF2K_SET_BAND_GAP: u8 = 0x32;     // data[0]: 0/1
pub const CMD_RF2K_FRQ_DELAY_UP: u8 = 0x33;
pub const CMD_RF2K_FRQ_DELAY_DOWN: u8 = 0x34;
pub const CMD_RF2K_AUTOTUNE_THRESH_UP: u8 = 0x35;
pub const CMD_RF2K_AUTOTUNE_THRESH_DOWN: u8 = 0x36;
pub const CMD_RF2K_DAC_ALC_UP: u8 = 0x37;
pub const CMD_RF2K_DAC_ALC_DOWN: u8 = 0x38;
pub const CMD_RF2K_ZERO_FRAM: u8 = 0x39;
// Drive config (Phase D)
pub const CMD_RF2K_SET_DRIVE_SSB: u8 = 0x40;    // data[0]=band, data[1]=watts
pub const CMD_RF2K_SET_DRIVE_AM: u8 = 0x41;
pub const CMD_RF2K_SET_DRIVE_CONT: u8 = 0x42;

/// UltraBeam RCU-06 command IDs (client → server via EquipmentCommand)
pub const CMD_UB_RETRACT: u8 = 0x01;
pub const CMD_UB_SET_FREQ: u8 = 0x02;  // data[0..1]=khz_le, data[2]=direction
pub const CMD_UB_READ_ELEMENTS: u8 = 0x03;
pub const CMD_UB_MODIFY_ELEMENT: u8 = 0x04;  // data[0]=index, data[1..2]=length_mm_le

/// Rotor command IDs (client → server via EquipmentCommand)
pub const CMD_ROTOR_GOTO: u8 = 0x01;    // data[0..1] = angle_x10 LE (0-3600)
pub const CMD_ROTOR_STOP: u8 = 0x02;
pub const CMD_ROTOR_CW: u8 = 0x03;      // manual clockwise
pub const CMD_ROTOR_CCW: u8 = 0x04;     // manual counter-clockwise

/// Equipment status flags
const EQUIPMENT_FLAG_HAS_LABELS: u8 = 0x01;

/// Equipment status packet: header(4) + device_type(1) + flags(1) + data(N)
/// For Amplitec6x2: data = switch_a(1) + switch_b(1) + connected(1) [+ labels_len(2) + labels_utf8(N) if has_labels]
#[derive(Debug, Clone)]
pub struct EquipmentStatusPacket {
    pub device_type: DeviceType,
    pub switch_a: u8,
    pub switch_b: u8,
    pub connected: bool,
    pub labels: Option<String>,
}

impl EquipmentStatusPacket {
    pub const MIN_SIZE: usize = Header::SIZE + 1 + 1 + 1 + 1 + 1; // 9 bytes

    pub fn serialize(&self, buf: &mut Vec<u8>) {
        let has_labels = self.labels.is_some();
        let flags = if has_labels { EQUIPMENT_FLAG_HAS_LABELS } else { 0 };
        let labels_bytes = self.labels.as_deref().unwrap_or("").as_bytes();
        let total = Self::MIN_SIZE + if has_labels { 2 + labels_bytes.len() } else { 0 };

        let start = buf.len();
        buf.resize(start + total, 0);
        let out = &mut buf[start..];

        let header = Header::new(PacketType::EquipmentStatus, Flags::NONE);
        header.serialize(out);
        out[4] = self.device_type as u8;
        out[5] = flags;
        out[6] = self.switch_a;
        out[7] = self.switch_b;
        out[8] = self.connected as u8;

        if has_labels {
            let len = labels_bytes.len() as u16;
            out[9..11].copy_from_slice(&len.to_be_bytes());
            out[11..11 + labels_bytes.len()].copy_from_slice(labels_bytes);
        }
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        let header = Header::deserialize(buf)?;
        if header.packet_type != PacketType::EquipmentStatus {
            bail!("expected EquipmentStatus, got {:?}", header.packet_type);
        }
        if buf.len() < Self::MIN_SIZE {
            bail!("equipment status too short: {} < {}", buf.len(), Self::MIN_SIZE);
        }
        let device_type = DeviceType::from_u8(buf[4])
            .ok_or_else(|| anyhow::anyhow!("unknown device type: 0x{:02X}", buf[4]))?;
        let flags = buf[5];
        let switch_a = buf[6];
        let switch_b = buf[7];
        let connected = buf[8] != 0;

        let labels = if flags & EQUIPMENT_FLAG_HAS_LABELS != 0 && buf.len() >= Self::MIN_SIZE + 2 {
            let labels_len = u16::from_be_bytes(buf[9..11].try_into().unwrap()) as usize;
            if buf.len() >= 11 + labels_len {
                Some(String::from_utf8_lossy(&buf[11..11 + labels_len]).to_string())
            } else {
                None
            }
        } else {
            None
        };

        Ok(Self { device_type, switch_a, switch_b, connected, labels })
    }
}

/// Amplitec power-cap table. Server sends the current table on
/// client-connect and on every change. Client sends the same
/// struct back when the operator presses "Save" in the Amplitec tab.
///
/// Wire: header(4) + 6 × { u16 max_w BE, u8 tx_blocked } = 22 bytes.
/// Index 0 = Amplitec-A position 1, index 5 = position 6.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmplitecPowerTablePacket {
    /// Max forward watts per position. `0` = no cap (none).
    pub max_w: [u16; 6],
    /// TX-block per position. `true` = RX-only, server allows no TX.
    pub tx_blocked: [bool; 6],
}

impl AmplitecPowerTablePacket {
    pub const SIZE: usize = Header::SIZE + 6 * 3;

    pub fn serialize(&self, buf: &mut [u8; Self::SIZE]) {
        let header = Header::new(PacketType::AmplitecPowerTable, Flags::NONE);
        header.serialize(&mut buf[..Header::SIZE]);
        for i in 0..6 {
            let off = Header::SIZE + i * 3;
            buf[off..off + 2].copy_from_slice(&self.max_w[i].to_be_bytes());
            buf[off + 2] = self.tx_blocked[i] as u8;
        }
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        if buf.len() < Self::SIZE {
            bail!("AmplitecPowerTable too short: {} < {}", buf.len(), Self::SIZE);
        }
        let header = Header::deserialize(buf)?;
        if header.packet_type != PacketType::AmplitecPowerTable {
            bail!("expected AmplitecPowerTable, got {:?}", header.packet_type);
        }
        let mut max_w = [0u16; 6];
        let mut tx_blocked = [false; 6];
        for i in 0..6 {
            let off = Header::SIZE + i * 3;
            max_w[i] = u16::from_be_bytes(buf[off..off + 2].try_into().unwrap());
            tx_blocked[i] = buf[off + 2] != 0;
        }
        Ok(Self { max_w, tx_blocked })
    }
}

/// Equipment command packet: header(4) + device_type(1) + command_id(1) + data(N)
/// For Amplitec6x2: SetSwitchA(0x01)=[pos], SetSwitchB(0x02)=[pos]
#[derive(Debug, Clone)]
pub struct EquipmentCommandPacket {
    pub device_type: DeviceType,
    pub command_id: u8,
    pub data: Vec<u8>,
}

impl EquipmentCommandPacket {
    pub const MIN_SIZE: usize = Header::SIZE + 1 + 1; // 6 bytes

    pub const CMD_SET_SWITCH_A: u8 = 0x01;
    pub const CMD_SET_SWITCH_B: u8 = 0x02;

    pub fn serialize(&self, buf: &mut Vec<u8>) {
        let start = buf.len();
        buf.resize(start + Self::MIN_SIZE + self.data.len(), 0);
        let out = &mut buf[start..];

        let header = Header::new(PacketType::EquipmentCommand, Flags::NONE);
        header.serialize(out);
        out[4] = self.device_type as u8;
        out[5] = self.command_id;
        out[Self::MIN_SIZE..Self::MIN_SIZE + self.data.len()].copy_from_slice(&self.data);
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        let header = Header::deserialize(buf)?;
        if header.packet_type != PacketType::EquipmentCommand {
            bail!("expected EquipmentCommand, got {:?}", header.packet_type);
        }
        if buf.len() < Self::MIN_SIZE {
            bail!("equipment command too short: {} < {}", buf.len(), Self::MIN_SIZE);
        }
        let device_type = DeviceType::from_u8(buf[4])
            .ok_or_else(|| anyhow::anyhow!("unknown device type: 0x{:02X}", buf[4]))?;
        let command_id = buf[5];
        let data = buf[Self::MIN_SIZE..].to_vec();
        Ok(Self { device_type, command_id, data })
    }
}

/// Yaesu radio state (server → client).
/// Format: [header(4)][freq_a(8)][freq_b(8)][mode(1)][smeter(2)][tx(1)][power_on(1)][af_gain(1)][tx_power(1)]
pub struct YaesuStatePacket {
    pub freq_a: u64,
    pub freq_b: u64,
    pub mode: u8,
    pub smeter: u16,
    pub tx_active: bool,
    pub power_on: bool,
    pub af_gain: u8,
    pub tx_power: u8,
    pub vfo_select: u8,    // 0=VFO, 1=Memory, 2=MemTune (from IF P7)
    pub memory_channel: u16, // Current memory channel (from IF)
    pub squelch: u8,       // 0-255
    pub rf_gain: u8,       // 0-255
    pub mic_gain: u8,      // 0-100
    pub split: bool,
    pub scan: bool,
    /// Internal ATU state (PATCH-yaesu-internal-atu): 0=off/bypass, 1=on, 2=tuning-in-progress.
    /// Additive trailing field — normalised server-side from the `AC;` readback (never raw P3).
    pub tuner_state: u8,
    /// High-SWR alarm (PATCH-swr-alarm): FTX-1 from RI P2 (0/1), 991A from RM6 >=
    /// threshold. Additive trailing field — false on old servers/packets.
    pub hi_swr: bool,
    /// Max TX power (watts) for the CURRENT transmit band (PATCH-yaesu-power-scaling).
    /// Additive trailing field; **0 = still unknown** (old server / before the first
    /// EX readout), NOT 0 watts. The client scales the slider to `5..=max`.
    pub tx_power_max: u8,
}

impl YaesuStatePacket {
    pub const SIZE: usize = Header::SIZE + 8 + 8 + 1 + 2 + 1 + 1 + 1 + 1 + 1 + 2 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1; // 38 bytes

    pub fn serialize(&self, buf: &mut [u8; Self::SIZE]) {
        self.serialize_as_type(buf, PacketType::YaesuState);
    }

    /// Serialize under a caller-chosen packet type so slot 1 can emit the same
    /// state layout under `PacketType::YaesuState2` (dual-radio Option B-prime).
    pub fn serialize_as_type(&self, buf: &mut [u8; Self::SIZE], ptype: PacketType) {
        let header = Header::new(ptype, Flags::NONE);
        header.serialize(buf);
        let mut pos = Header::SIZE;
        buf[pos..pos + 8].copy_from_slice(&self.freq_a.to_be_bytes()); pos += 8;
        buf[pos..pos + 8].copy_from_slice(&self.freq_b.to_be_bytes()); pos += 8;
        buf[pos] = self.mode; pos += 1;
        buf[pos..pos + 2].copy_from_slice(&self.smeter.to_be_bytes()); pos += 2;
        buf[pos] = self.tx_active as u8; pos += 1;
        buf[pos] = self.power_on as u8; pos += 1;
        buf[pos] = self.af_gain; pos += 1;
        buf[pos] = self.tx_power; pos += 1;
        buf[pos] = self.vfo_select; pos += 1;
        buf[pos..pos + 2].copy_from_slice(&self.memory_channel.to_be_bytes()); pos += 2;
        buf[pos] = self.squelch; pos += 1;
        buf[pos] = self.rf_gain; pos += 1;
        buf[pos] = self.mic_gain; pos += 1;
        buf[pos] = self.split as u8; pos += 1;
        buf[pos] = self.scan as u8; pos += 1;
        buf[pos] = self.tuner_state; pos += 1;
        buf[pos] = self.hi_swr as u8; pos += 1;
        buf[pos] = self.tx_power_max;
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        // Accept old shorter packets (missing trailing additive fields squelch..
        // tuner_state/hi_swr) for backward compat; the per-field guards below fill
        // absent trailing bytes with defaults.
        if buf.len() < Self::SIZE - 3 {
            bail!("YaesuState packet too short: {} < {}", buf.len(), Self::SIZE - 3);
        }
        let mut pos = Header::SIZE;
        let freq_a = u64::from_be_bytes(buf[pos..pos + 8].try_into().unwrap()); pos += 8;
        let freq_b = u64::from_be_bytes(buf[pos..pos + 8].try_into().unwrap()); pos += 8;
        let mode = buf[pos]; pos += 1;
        let smeter = u16::from_be_bytes(buf[pos..pos + 2].try_into().unwrap()); pos += 2;
        let tx_active = buf[pos] != 0; pos += 1;
        let power_on = buf[pos] != 0; pos += 1;
        let af_gain = buf[pos]; pos += 1;
        let tx_power = buf[pos]; pos += 1;
        let vfo_select = buf[pos]; pos += 1;
        let memory_channel = u16::from_be_bytes(buf[pos..pos + 2].try_into().unwrap()); pos += 2;
        let squelch = if buf.len() > pos { buf[pos] } else { 0 }; pos += 1;
        let rf_gain = if buf.len() > pos { buf[pos] } else { 0 }; pos += 1;
        let mic_gain = if buf.len() > pos { buf[pos] } else { 0 }; pos += 1;
        let split = if buf.len() > pos { buf[pos] != 0 } else { false }; pos += 1;
        let scan = if buf.len() > pos { buf[pos] != 0 } else { false }; pos += 1;
        let tuner_state = if buf.len() > pos { buf[pos] } else { 0 }; pos += 1;
        let hi_swr = if buf.len() > pos { buf[pos] != 0 } else { false }; pos += 1;
        let tx_power_max = if buf.len() > pos { buf[pos] } else { 0 };
        Ok(Self { freq_a, freq_b, mode, smeter, tx_active, power_on, af_gain, tx_power, vfo_select, memory_channel, squelch, rf_gain, mic_gain, split, scan, tuner_state, hi_swr, tx_power_max })
    }
}

/// TX profile list with names (server → client).
/// Format: [header][count: u8][active: u8][len1: u8][name1: bytes]...
pub struct TxProfilesPacket {
    pub names: Vec<String>,
    pub active: u8,
}

impl TxProfilesPacket {
    pub fn serialize(&self, buf: &mut Vec<u8>) {
        let count = self.names.len().min(255) as u8;
        let names_size: usize = self.names.iter()
            .take(count as usize)
            .map(|n| 1 + n.len().min(255))
            .sum();
        let total = Header::SIZE + 2 + names_size;

        let start = buf.len();
        buf.resize(start + total, 0);
        let out = &mut buf[start..];

        let header = Header::new(PacketType::TxProfiles, Flags::NONE);
        header.serialize(out);
        out[Header::SIZE] = count;
        out[Header::SIZE + 1] = self.active;

        let mut pos = Header::SIZE + 2;
        for name in self.names.iter().take(count as usize) {
            let bytes = name.as_bytes();
            let len = bytes.len().min(255);
            out[pos] = len as u8;
            pos += 1;
            out[pos..pos + len].copy_from_slice(&bytes[..len]);
            pos += len;
        }
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        let header = Header::deserialize(buf)?;
        if header.packet_type != PacketType::TxProfiles {
            bail!("expected TxProfiles packet, got {:?}", header.packet_type);
        }
        if buf.len() < Header::SIZE + 2 {
            bail!("TxProfiles packet too short");
        }
        let count = buf[Header::SIZE] as usize;
        let active = buf[Header::SIZE + 1];

        let mut pos = Header::SIZE + 2;
        let mut names = Vec::with_capacity(count);
        for _ in 0..count {
            if pos >= buf.len() { break; }
            let len = buf[pos] as usize;
            pos += 1;
            if pos + len > buf.len() { break; }
            names.push(String::from_utf8_lossy(&buf[pos..pos + len]).to_string());
            pos += len;
        }
        Ok(Self { names, active })
    }
}

/// Yaesu DSP/function control id (the `control` field of `YaesuControlPacket` and
/// the bit index in `YaesuFeaturePacket.toggles`). PATCH-yaesu-extra-controls.
/// Phase A1 wires RfAtt + BreakIn; the rest are reserved for later phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, TryFromPrimitive)]
#[repr(u8)]
pub enum YaesuCtrl {
    RfAtt = 0,
    BreakIn = 1,
    Narrow = 2,
    AutoNotch = 3,
    Vox = 4,
    Mox = 5,
    Agc = 6,
    PreAmp = 7,
    Nb = 8,
    Dnr = 9,
    Processor = 10,
    Amc = 11,
    Monitor = 12,
    /// 991A-only: Noise Blanker on/off (`NB`), separate from the NB level (`NL`=Nb).
    NbOn = 13,
    /// 991A-only: Noise Reduction on/off (`NR`), separate from the DNR level (`RL`=Dnr).
    NrOn = 14,
    // Phase D — composite controls: on/off (toggle-bit) + frequency (freqs-array).
    ContourOn = 15,
    ApfOn = 16,
    NotchOn = 17,
    ContourFreq = 18, // → freqs[0]
    ApfFreq = 19,     // → freqs[1]
    NotchFreq = 20,   // → freqs[2]
    // Clarifier (RIT/XIT). Toggles share one offset on Yaesu. Per-model: 991A `RT`/`XT`/
    // `RC`/`RU`/`RD` (relative), FTX-1 `CF` (P3=0 RX/TX on/off, P3=1 absolute freq).
    RitOn = 21,     // toggle-bit 21 (RX-clarifier)
    XitOn = 22,     // toggle-bit 22 (TX-clarifier)
    ClarClear = 23, // momentary: offset → 0
    ClarStep = 24,  // value = i16-as-u16 signed step in Hz; current offset → freqs[3]
}

/// Typed Yaesu control (client→server): set `control` (a `YaesuCtrl`) on `slot`
/// (0=radio1/991A, 1=radio2/FTX-1) to `value` (toggle 0/1, or level/multi-state).
pub struct YaesuControlPacket {
    pub slot: u8,
    pub control: u8,
    pub value: u16,
}

impl YaesuControlPacket {
    pub const SIZE: usize = Header::SIZE + 1 + 1 + 2; // 8

    pub fn serialize(&self, buf: &mut [u8; Self::SIZE]) {
        Header::new(PacketType::YaesuControl, Flags::NONE).serialize(buf);
        buf[4] = self.slot;
        buf[5] = self.control;
        buf[6..8].copy_from_slice(&self.value.to_be_bytes());
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        let header = Header::deserialize(buf)?;
        if header.packet_type != PacketType::YaesuControl {
            bail!("expected YaesuControl packet, got {:?}", header.packet_type);
        }
        if buf.len() < Self::SIZE {
            bail!("YaesuControl packet too short: {} < {}", buf.len(), Self::SIZE);
        }
        Ok(Self {
            slot: buf[4],
            control: buf[5],
            value: u16::from_be_bytes(buf[6..8].try_into().unwrap()),
        })
    }
}

/// Yaesu feature-state feedback (server→client): per-slot toggle bitfield (bit N =
/// `YaesuCtrl` N on/off) + level values (indexed by `YaesuCtrl`). Value-change-only.
pub struct YaesuFeaturePacket {
    pub slot: u8,
    pub toggles: u32,
    pub levels: [u8; Self::N_LEVELS],
    /// Frequency-type feature values (u16, up to 3200 Hz): [0]=Contour, [1]=APF, [2]=Notch.
    pub freqs: [u16; Self::N_FREQS],
}

impl YaesuFeaturePacket {
    pub const N_LEVELS: usize = 16;
    pub const N_FREQS: usize = 4;
    pub const SIZE: usize = Header::SIZE + 1 + 4 + Self::N_LEVELS + Self::N_FREQS * 2; // 33

    pub fn serialize(&self, buf: &mut [u8; Self::SIZE]) {
        Header::new(PacketType::YaesuFeature, Flags::NONE).serialize(buf);
        buf[4] = self.slot;
        buf[5..9].copy_from_slice(&self.toggles.to_be_bytes());
        buf[9..9 + Self::N_LEVELS].copy_from_slice(&self.levels);
        let mut pos = 9 + Self::N_LEVELS;
        for f in &self.freqs {
            buf[pos..pos + 2].copy_from_slice(&f.to_be_bytes());
            pos += 2;
        }
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        let header = Header::deserialize(buf)?;
        if header.packet_type != PacketType::YaesuFeature {
            bail!("expected YaesuFeature packet, got {:?}", header.packet_type);
        }
        if buf.len() < 9 {
            bail!("YaesuFeature packet too short: {}", buf.len());
        }
        let slot = buf[4];
        let toggles = u32::from_be_bytes(buf[5..9].try_into().unwrap());
        // Additive-tolerant: accept fewer trailing bytes (default 0).
        let mut levels = [0u8; Self::N_LEVELS];
        for (i, lvl) in levels.iter_mut().enumerate() {
            if buf.len() > 9 + i {
                *lvl = buf[9 + i];
            }
        }
        let mut freqs = [0u16; Self::N_FREQS];
        let fbase = 9 + Self::N_LEVELS;
        for (i, fr) in freqs.iter_mut().enumerate() {
            if buf.len() >= fbase + i * 2 + 2 {
                *fr = u16::from_be_bytes(buf[fbase + i * 2..fbase + i * 2 + 2].try_into().unwrap());
            }
        }
        Ok(Self { slot, toggles, levels, freqs })
    }
}

/// Canonical short display name for a Yaesu radio model code (the `model` byte
/// from `RadioInfo`/`YaesuPresence`). **Single source of truth**: this is the only
/// place where a model code is mapped to a name. Server (log prefixes) and
/// client (all "Yaesu N: <type>" labels) read here — a new radio type you add
/// here once and the name follows everywhere automatically. 0 = FT-991A, 1 = FTX-1.
pub fn radio_model_name(code: u8) -> &'static str {
    match code {
        0 => "991A",
        1 => "FTX1",
        _ => "Radio",
    }
}

/// The code a client holds for a slot the server has not named yet.
///
/// Never sent on the wire - it exists so a client can say "Yaesu 1" instead of
/// guessing a type. The desktop client used to open with 0 (991A) for slot 0
/// and 1 (FTX-1) for slot 1, which put two model names on screen on a station
/// that had no radio at all, and - worse - satisfied the two `== 0` / `== 1`
/// tests that gate model-specific controls. One of those is the CAT power
/// button, which is only safe on a confirmed 991A because the other types drop
/// their USB link at PS0 and cannot be switched back on remotely.
pub const RADIO_MODEL_UNKNOWN: u8 = 0xFF;

/// Has the server said which radio sits in this slot?
pub fn radio_model_known(code: u8) -> bool {
    code != RADIO_MODEL_UNKNOWN
}

/// How a slot is named on screen: "Yaesu 1: FTX1" once the server has said what
/// it is, and plain "Yaesu 1" until then.
///
/// Here rather than in a front end, because both of them draw this label and one
/// of them kept its own copy of the rule: the desktop learned to stop guessing a
/// model name, and the phone went on printing a fixed "Radio 1" / "Radio 2"
/// (owner, 2026-08-20). `slot` is 0-based; the label counts from one, the way the
/// settings do.
pub fn radio_slot_label(slot: u8, code: u8) -> String {
    if radio_model_known(code) {
        format!("Yaesu {}: {}", slot + 1, radio_model_name(code))
    } else {
        format!("Yaesu {}", slot + 1)
    }
}

/// Why a radio's serial port cannot be used right now, as a wire code on
/// `YaesuPresence`. **Single source of truth** for the codes, like
/// `radio_model_name` above: the server classifies, every UI only maps a code
/// to its own words. The distinction matters because the operator's next move
/// differs per case - and because "port in use by another program" is the
/// single most common reason a 991A/FTX-1 shows up dead in ThetisLink: other
/// control software still holding the CAT port in the background.
pub const PORT_TROUBLE_NONE: u8 = 0;
/// Windows refused the open with access-denied: another program has the port.
pub const PORT_TROUBLE_BUSY: u8 = 1;
/// The COM port does not exist: cable out, or the number changed after a
/// replug into a different USB socket.
pub const PORT_TROUBLE_MISSING: u8 = 2;

/// Yaesu radio presence (server→client, broadcast to all clients): per slot whether
/// there is a connected radio + the model + why its port cannot be opened when
/// it cannot. Decouples the connected-status/selector from the audio+state
/// subscription so dropping/appearing is dynamic (not sticky).
/// PATCH-android-yaesu-presence-datasaver.
///
/// The two trouble bytes are additive: an old client stops reading after the
/// model bytes, and an old server's shorter packet reads as trouble = none.
pub struct YaesuPresencePacket {
    pub slot0_present: bool,
    pub slot0_model: u8,
    pub slot1_present: bool,
    pub slot1_model: u8,
    pub slot0_trouble: u8,
    pub slot1_trouble: u8,
}

impl YaesuPresencePacket {
    pub const SIZE: usize = Header::SIZE + 6; // 10
    /// The pre-trouble size; a packet this short is an old server, not an error.
    const MIN_SIZE: usize = Header::SIZE + 4; // 8

    pub fn serialize(&self, buf: &mut [u8; Self::SIZE]) {
        Header::new(PacketType::YaesuPresence, Flags::NONE).serialize(buf);
        buf[4] = self.slot0_present as u8;
        buf[5] = self.slot0_model;
        buf[6] = self.slot1_present as u8;
        buf[7] = self.slot1_model;
        buf[8] = self.slot0_trouble;
        buf[9] = self.slot1_trouble;
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        let header = Header::deserialize(buf)?;
        if header.packet_type != PacketType::YaesuPresence {
            bail!("expected YaesuPresence packet, got {:?}", header.packet_type);
        }
        if buf.len() < Self::MIN_SIZE {
            bail!("YaesuPresence packet too short: {} < {}", buf.len(), Self::MIN_SIZE);
        }
        Ok(Self {
            slot0_present: buf[4] != 0,
            slot0_model: buf[5],
            slot1_present: buf[6] != 0,
            slot1_model: buf[7],
            slot0_trouble: buf.get(8).copied().unwrap_or(PORT_TROUBLE_NONE),
            slot1_trouble: buf.get(9).copied().unwrap_or(PORT_TROUBLE_NONE),
        })
    }
}

/// Parse any incoming packet by peeking at the header
pub enum Packet {
    Audio(AudioPacket),
    Heartbeat(Heartbeat),
    HeartbeatAck(HeartbeatAck),
    Control(ControlPacket),
    Disconnect,
    PttDenied,
    Frequency(FrequencyPacket),
    Mode(ModePacket),
    Smeter(SmeterPacket),
    Spectrum(SpectrumPacket),
    FullSpectrum(SpectrumPacket),
    EquipmentStatus(EquipmentStatusPacket),
    EquipmentCommand(EquipmentCommandPacket),
    AmplitecPowerTable(AmplitecPowerTablePacket),
    AudioRx2(AudioPacket),
    FrequencyRx2(FrequencyPacket),
    ModeRx2(ModePacket),
    SmeterRx2(SmeterPacket),
    SmeterSig(SmeterPacket),
    SmeterMaxBin(SmeterPacket),
    SmeterRx2Sig(SmeterPacket),
    SmeterRx2MaxBin(SmeterPacket),
    SpectrumRx2(SpectrumPacket),
    FullSpectrumRx2(SpectrumPacket),
    SpectrumVrx1(SpectrumPacket),
    SpectrumVrx2(SpectrumPacket),
    Spot(SpotPacket),
    TxProfiles(TxProfilesPacket),
    AudioYaesu(AudioPacket),
    AudioBinR(AudioPacket),
    AudioMultiCh(MultiChannelAudioPacket),
    YaesuState(YaesuStatePacket),
    FrequencyYaesu(FrequencyPacket),
    YaesuMemoryData(String),
    /// The client is asking for the server's own report attachment.
    ServerReportRequest,
    /// (part, parts, bytes) - see `PacketType::ServerReportPart`.
    ServerReportPart(u16, u16, Vec<u8>),
    // Dual-radio slot 1 (Option B-prime): same payload structs, own packet types.
    AudioYaesu2(AudioPacket),
    YaesuState2(YaesuStatePacket),
    FrequencyYaesu2(FrequencyPacket),
    YaesuMemoryData2(String),
    /// Per-radio model info (server → client): (slot, model_code). model_code:
    /// 0 = FT-991A, 1 = FTX-1. For panel naming in the client.
    RadioInfo { slot: u8, model: u8 },
    AuthChallenge([u8; 16]),    // nonce
    AuthResponse([u8; 32]),     // HMAC
    AuthResult(u8),             // 0=rejected, 1=accepted, 2=totp_required
    TotpChallenge,              // server requests TOTP code
    TotpResponse(String),       // 6-digit TOTP code
    AudioVrx(VrxAudioPacket),
    FrequencyVrx(VrxFrequencyPacket),
    FrequencyVrxActual(VrxFrequencyPacket),
    TxFilterBand(TxFilterBandPacket),
    YaesuControl(YaesuControlPacket),
    YaesuFeature(YaesuFeaturePacket),
    YaesuPresence(YaesuPresencePacket),
}

/// AuthResult codes
pub const AUTH_REJECTED: u8 = 0;
pub const AUTH_ACCEPTED: u8 = 1;
pub const AUTH_TOTP_REQUIRED: u8 = 2;

impl Packet {
    pub fn deserialize(buf: &[u8]) -> Result<Self> {
        let header = Header::deserialize(buf)?;
        match header.packet_type {
            PacketType::Audio => Ok(Packet::Audio(AudioPacket::deserialize(buf)?)),
            PacketType::Heartbeat => Ok(Packet::Heartbeat(Heartbeat::deserialize(buf)?)),
            PacketType::HeartbeatAck => Ok(Packet::HeartbeatAck(HeartbeatAck::deserialize(buf)?)),
            PacketType::Control => Ok(Packet::Control(ControlPacket::deserialize(buf)?)),
            PacketType::Disconnect => Ok(Packet::Disconnect),
            PacketType::PttDenied => Ok(Packet::PttDenied),
            PacketType::Frequency => Ok(Packet::Frequency(FrequencyPacket::deserialize(buf)?)),
            PacketType::Mode => Ok(Packet::Mode(ModePacket::deserialize(buf)?)),
            PacketType::Smeter => Ok(Packet::Smeter(SmeterPacket::deserialize(buf)?)),
            PacketType::SmeterSig => Ok(Packet::SmeterSig(SmeterPacket::deserialize(buf)?)),
            PacketType::SmeterMaxBin => Ok(Packet::SmeterMaxBin(SmeterPacket::deserialize(buf)?)),
            PacketType::SmeterRx2Sig => Ok(Packet::SmeterRx2Sig(SmeterPacket::deserialize(buf)?)),
            PacketType::SmeterRx2MaxBin => Ok(Packet::SmeterRx2MaxBin(SmeterPacket::deserialize(buf)?)),
            PacketType::Spectrum => Ok(Packet::Spectrum(SpectrumPacket::deserialize(buf)?)),
            PacketType::FullSpectrum => Ok(Packet::FullSpectrum(SpectrumPacket::deserialize(buf)?)),
            PacketType::EquipmentStatus => Ok(Packet::EquipmentStatus(EquipmentStatusPacket::deserialize(buf)?)),
            PacketType::EquipmentCommand => Ok(Packet::EquipmentCommand(EquipmentCommandPacket::deserialize(buf)?)),
            PacketType::AmplitecPowerTable => Ok(Packet::AmplitecPowerTable(AmplitecPowerTablePacket::deserialize(buf)?)),
            PacketType::AudioRx2 => Ok(Packet::AudioRx2(AudioPacket::deserialize(buf)?)),
            PacketType::FrequencyRx2 => Ok(Packet::FrequencyRx2(FrequencyPacket::deserialize(buf)?)),
            PacketType::ModeRx2 => Ok(Packet::ModeRx2(ModePacket::deserialize(buf)?)),
            PacketType::SmeterRx2 => Ok(Packet::SmeterRx2(SmeterPacket::deserialize(buf)?)),
            PacketType::SpectrumRx2 => Ok(Packet::SpectrumRx2(SpectrumPacket::deserialize(buf)?)),
            PacketType::FullSpectrumRx2 => Ok(Packet::FullSpectrumRx2(SpectrumPacket::deserialize(buf)?)),
            PacketType::SpectrumVrx1 => Ok(Packet::SpectrumVrx1(SpectrumPacket::deserialize(buf)?)),
            PacketType::SpectrumVrx2 => Ok(Packet::SpectrumVrx2(SpectrumPacket::deserialize(buf)?)),
            PacketType::Spot => Ok(Packet::Spot(SpotPacket::deserialize(buf)?)),
            PacketType::TxProfiles => Ok(Packet::TxProfiles(TxProfilesPacket::deserialize(buf)?)),
            PacketType::AudioYaesu => Ok(Packet::AudioYaesu(AudioPacket::deserialize(buf)?)),
            PacketType::AudioBinR => Ok(Packet::AudioBinR(AudioPacket::deserialize(buf)?)),
            PacketType::AudioVrx => Ok(Packet::AudioVrx(VrxAudioPacket::deserialize(buf)?)),
            PacketType::FrequencyVrx => Ok(Packet::FrequencyVrx(VrxFrequencyPacket::deserialize(buf)?)),
            PacketType::FrequencyVrxActual => Ok(Packet::FrequencyVrxActual(VrxFrequencyPacket::deserialize(buf)?)),
            PacketType::TxFilterBand => Ok(Packet::TxFilterBand(TxFilterBandPacket::deserialize(buf)?)),
            PacketType::YaesuControl => Ok(Packet::YaesuControl(YaesuControlPacket::deserialize(buf)?)),
            PacketType::YaesuFeature => Ok(Packet::YaesuFeature(YaesuFeaturePacket::deserialize(buf)?)),
            PacketType::YaesuPresence => Ok(Packet::YaesuPresence(YaesuPresencePacket::deserialize(buf)?)),
            PacketType::AudioMultiCh => Ok(Packet::AudioMultiCh(MultiChannelAudioPacket::deserialize(buf)?)),
            PacketType::YaesuState => Ok(Packet::YaesuState(YaesuStatePacket::deserialize(buf)?)),
            PacketType::FrequencyYaesu => Ok(Packet::FrequencyYaesu(FrequencyPacket::deserialize(buf)?)),
            PacketType::AudioYaesu2 => Ok(Packet::AudioYaesu2(AudioPacket::deserialize(buf)?)),
            PacketType::YaesuState2 => Ok(Packet::YaesuState2(YaesuStatePacket::deserialize(buf)?)),
            PacketType::FrequencyYaesu2 => Ok(Packet::FrequencyYaesu2(FrequencyPacket::deserialize(buf)?)),
            PacketType::YaesuMemoryData2 => {
                if buf.len() < 6 { bail!("YaesuMemoryData2 too short"); }
                let len = u16::from_be_bytes(buf[4..6].try_into().unwrap()) as usize;
                if buf.len() < 6 + len { bail!("YaesuMemoryData2 truncated"); }
                let text = String::from_utf8_lossy(&buf[6..6+len]).to_string();
                Ok(Packet::YaesuMemoryData2(text))
            }
            PacketType::RadioInfo => {
                if buf.len() < Header::SIZE + 2 { bail!("RadioInfo too short"); }
                Ok(Packet::RadioInfo { slot: buf[Header::SIZE], model: buf[Header::SIZE + 1] })
            }
            PacketType::AuthChallenge => {
                if buf.len() < 20 { bail!("AuthChallenge too short"); }
                let mut nonce = [0u8; 16];
                nonce.copy_from_slice(&buf[4..20]);
                Ok(Packet::AuthChallenge(nonce))
            }
            PacketType::AuthResponse => {
                if buf.len() < 36 { bail!("AuthResponse too short"); }
                let mut hmac = [0u8; 32];
                hmac.copy_from_slice(&buf[4..36]);
                Ok(Packet::AuthResponse(hmac))
            }
            PacketType::AuthResult => {
                if buf.len() < 5 { bail!("AuthResult too short"); }
                Ok(Packet::AuthResult(buf[4]))
            }
            PacketType::TotpChallenge => {
                Ok(Packet::TotpChallenge)
            }
            PacketType::TotpResponse => {
                if buf.len() < 6 { bail!("TotpResponse too short"); }
                let len = u16::from_be_bytes(buf[4..6].try_into().unwrap()) as usize;
                if buf.len() < 6 + len { bail!("TotpResponse truncated"); }
                let code = String::from_utf8_lossy(&buf[6..6+len]).to_string();
                Ok(Packet::TotpResponse(code))
            }
            PacketType::ServerReportRequest => Ok(Packet::ServerReportRequest),
            PacketType::ServerReportPart => {
                if buf.len() < 10 { bail!("ServerReportPart too short"); }
                let part = u16::from_be_bytes(buf[4..6].try_into().unwrap());
                let parts = u16::from_be_bytes(buf[6..8].try_into().unwrap());
                let len = u16::from_be_bytes(buf[8..10].try_into().unwrap()) as usize;
                if buf.len() < 10 + len { bail!("ServerReportPart truncated"); }
                Ok(Packet::ServerReportPart(part, parts, buf[10..10 + len].to_vec()))
            }
            PacketType::YaesuMemoryData => {
                if buf.len() < 6 { bail!("YaesuMemoryData too short"); }
                let len = u16::from_be_bytes(buf[4..6].try_into().unwrap()) as usize;
                if buf.len() < 6 + len { bail!("YaesuMemoryData truncated"); }
                let text = String::from_utf8_lossy(&buf[6..6+len]).to_string();
                Ok(Packet::YaesuMemoryData(text))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// The "not named yet" code has to stay distinguishable from every real
    /// radio. If a future model ever took 0xFF, a client would silently go back
    /// to claiming a type it had not been told - and to letting the CAT power
    /// button through on a radio that cannot be switched back on remotely.
    #[test]
    fn the_unknown_model_code_is_not_a_real_radio() {
        use super::{radio_model_name, RADIO_MODEL_UNKNOWN};
        assert_eq!(radio_model_name(RADIO_MODEL_UNKNOWN), "Radio");
        for code in [0u8, 1] {
            assert_ne!(code, RADIO_MODEL_UNKNOWN, "a real model took the unknown code");
            assert_ne!(
                radio_model_name(code),
                radio_model_name(RADIO_MODEL_UNKNOWN),
                "model {code} is indistinguishable from an unnamed slot"
            );
        }
    }

    /// An unnamed slot must not read as a radio type anywhere a label is drawn.
    /// Both front ends draw this now, so getting it wrong here goes wrong twice.
    #[test]
    fn an_unnamed_slot_is_labelled_by_its_number_alone() {
        use super::{radio_model_known, radio_slot_label, RADIO_MODEL_UNKNOWN};
        assert!(!radio_model_known(RADIO_MODEL_UNKNOWN));
        assert_eq!(radio_slot_label(0, RADIO_MODEL_UNKNOWN), "Yaesu 1");
        assert_eq!(radio_slot_label(1, RADIO_MODEL_UNKNOWN), "Yaesu 2");
        // And a named one carries its type, counted from one.
        assert!(radio_model_known(0));
        assert_eq!(radio_slot_label(0, 0), "Yaesu 1: 991A");
        assert_eq!(radio_slot_label(1, 1), "Yaesu 2: FTX1");
        // Two 991As is a real station, so the slot number has to survive.
        assert_ne!(radio_slot_label(0, 0), radio_slot_label(1, 0));
    }

    /// The VRX pan offset is a signed value squeezed through an unsigned field,
    /// which is the kind of thing that works in one direction and silently
    /// wraps in the other. Both directions, and the ends.
    #[test]
    fn vrx_pan_survives_the_wire_in_both_directions() {
        for hz in [0i32, 10, -10, 1_000, -1_000, 123_450, -123_450] {
            let back = super::unpack_vrx_pan(super::pack_vrx_pan(hz));
            assert_eq!(back, hz, "{hz} Hz did not come back");
        }
    }

    /// Beyond what the field holds it must saturate, not wrap - a pan that
    /// jumped to the far side of the receiver would be worse than one that
    /// stopped.
    #[test]
    fn an_impossible_vrx_pan_stops_at_the_edge() {
        let far = super::unpack_vrx_pan(super::pack_vrx_pan(50_000_000));
        assert!(far > 300_000 && far < 400_000, "clamped to {far} Hz");
        let near = super::unpack_vrx_pan(super::pack_vrx_pan(-50_000_000));
        assert!(near < -300_000 && near > -400_000, "clamped to {near} Hz");
    }

    /// Sub-step offsets round towards zero rather than to something else.
    #[test]
    fn a_pan_finer_than_a_step_reads_as_no_pan() {
        assert_eq!(super::unpack_vrx_pan(super::pack_vrx_pan(9)), 0);
        assert_eq!(super::unpack_vrx_pan(super::pack_vrx_pan(-9)), 0);
    }

    use super::*;

    #[test]
    fn audio_spectrum_classification_is_consistent_for_all_bytes() {
        // The raw-byte consts and the typed predicates must agree for every
        // possible byte value, so a consumer that classifies on buf[2] (relay
        // routing, bw metering) can never disagree with the enum. Also asserts
        // audio and spectrum are disjoint.
        for b in 0u8..=255 {
            let is_audio_const = AUDIO_PACKET_TYPES.contains(&b);
            let is_spectrum_const = SPECTRUM_PACKET_TYPES.contains(&b);
            match PacketType::from_u8(b) {
                Some(pt) => {
                    assert_eq!(pt.is_audio(), is_audio_const, "audio mismatch for 0x{b:02X}");
                    assert_eq!(pt.is_spectrum(), is_spectrum_const, "spectrum mismatch for 0x{b:02X}");
                    assert!(!(pt.is_audio() && pt.is_spectrum()), "0x{b:02X} both audio+spectrum");
                }
                None => {
                    // Unknown byte: must not appear in either const set.
                    assert!(!is_audio_const && !is_spectrum_const, "unknown 0x{b:02X} in a type set");
                }
            }
        }
        assert_eq!(AUDIO_PACKET_TYPES.len(), 7);
        assert_eq!(SPECTRUM_PACKET_TYPES.len(), 6);
    }

    #[test]
    fn header_roundtrip() {
        let header = Header::new(PacketType::Audio, Flags::PTT);
        let mut buf = [0u8; 4];
        header.serialize(&mut buf);

        assert_eq!(buf[0], MAGIC);
        assert_eq!(buf[1], VERSION);
        assert_eq!(buf[2], 0x01);
        assert_eq!(buf[3], 0x01);

        let decoded = Header::deserialize(&buf).unwrap();
        assert_eq!(decoded.packet_type, PacketType::Audio);
        assert!(decoded.flags.ptt());
    }

    #[test]
    fn header_invalid_magic() {
        let buf = [0x00, VERSION, 0x01, 0x00];
        assert!(Header::deserialize(&buf).is_err());
    }

    #[test]
    fn yaesu_presence_roundtrip() {
        let pkt = YaesuPresencePacket {
            slot0_present: true,
            slot0_model: 0,   // FT-991A
            slot1_present: false,
            slot1_model: 1,   // FTX-1
            slot0_trouble: PORT_TROUBLE_BUSY,
            slot1_trouble: PORT_TROUBLE_NONE,
        };
        let mut buf = [0u8; YaesuPresencePacket::SIZE];
        pkt.serialize(&mut buf);
        // Parse via the generic path as the client receives it.
        match Packet::deserialize(&buf).unwrap() {
            Packet::YaesuPresence(p) => {
                assert!(p.slot0_present);
                assert_eq!(p.slot0_model, 0);
                assert!(!p.slot1_present);
                assert_eq!(p.slot1_model, 1);
                assert_eq!(p.slot0_trouble, PORT_TROUBLE_BUSY);
                assert_eq!(p.slot1_trouble, PORT_TROUBLE_NONE);
            }
            _ => panic!("expected YaesuPresence packet"),
        }
    }

    #[test]
    fn yaesu_presence_from_old_server_reads_as_no_trouble() {
        // An old server sends the 8-byte form; the two trouble bytes must
        // default to none rather than fail the parse.
        let pkt = YaesuPresencePacket {
            slot0_present: true,
            slot0_model: 1,
            slot1_present: false,
            slot1_model: 0,
            slot0_trouble: PORT_TROUBLE_MISSING, // must NOT survive the truncation
            slot1_trouble: PORT_TROUBLE_BUSY,
        };
        let mut buf = [0u8; YaesuPresencePacket::SIZE];
        pkt.serialize(&mut buf);
        let p = YaesuPresencePacket::deserialize(&buf[..8]).unwrap();
        assert!(p.slot0_present);
        assert_eq!(p.slot0_model, 1);
        assert_eq!(p.slot0_trouble, PORT_TROUBLE_NONE);
        assert_eq!(p.slot1_trouble, PORT_TROUBLE_NONE);
    }

    #[test]
    fn audio_packet_roundtrip() {
        let packet = AudioPacket {
            flags: Flags::NONE,
            sequence: 42,
            timestamp: 12345,
            opus_data: vec![0xDE, 0xAD, 0xBE, 0xEF],
        };
        let mut buf = Vec::new();
        packet.serialize(&mut buf);

        assert_eq!(buf.len(), AudioPacket::HEADER_SIZE + 4);

        let decoded = AudioPacket::deserialize(&buf).unwrap();
        assert_eq!(decoded.sequence, 42);
        assert_eq!(decoded.timestamp, 12345);
        assert_eq!(decoded.opus_data, vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert!(!decoded.flags.ptt());
    }

    #[test]
    fn audio_packet_with_ptt() {
        let packet = AudioPacket {
            flags: Flags::PTT,
            sequence: 1,
            timestamp: 0,
            opus_data: vec![0x01],
        };
        let mut buf = Vec::new();
        packet.serialize(&mut buf);

        let decoded = AudioPacket::deserialize(&buf).unwrap();
        assert!(decoded.flags.ptt());
    }

    #[test]
    fn heartbeat_roundtrip() {
        let hb = Heartbeat {
            flags: Flags::NONE,
            sequence: 100,
            local_time: 999_999,
            rtt_ms: 45,
            loss_percent: 2,
            jitter_ms: 10,
            capabilities: Capabilities::NONE.with(Capabilities::WIDEBAND_AUDIO),
        };
        let mut buf = [0u8; Heartbeat::SIZE];
        hb.serialize(&mut buf);

        assert_eq!(buf.len(), 20);

        let decoded = Heartbeat::deserialize(&buf).unwrap();
        assert_eq!(decoded.sequence, 100);
        assert_eq!(decoded.local_time, 999_999);
        assert_eq!(decoded.rtt_ms, 45);
        assert_eq!(decoded.loss_percent, 2);
        assert_eq!(decoded.jitter_ms, 10);
        assert!(decoded.capabilities.has(Capabilities::WIDEBAND_AUDIO));
    }

    #[test]
    fn heartbeat_backward_compat() {
        // Old 16-byte heartbeat without capabilities
        let mut buf = [0u8; 16];
        buf[0] = MAGIC;
        buf[1] = VERSION;
        buf[2] = PacketType::Heartbeat as u8;
        buf[3] = 0;
        buf[4..8].copy_from_slice(&42u32.to_be_bytes());
        let decoded = Heartbeat::deserialize(&buf).unwrap();
        assert_eq!(decoded.sequence, 42);
        assert_eq!(decoded.capabilities, Capabilities::NONE);
    }

    #[test]
    fn heartbeat_ack_roundtrip() {
        let ack = HeartbeatAck {
            flags: Flags::NONE,
            echo_sequence: 100,
            echo_time: 999_999,
            capabilities: Capabilities::NONE.with(Capabilities::WIDEBAND_AUDIO),
            state_flags: ServerStateFlags::NONE.with(ServerStateFlags::TCI_CONNECTED),
            subs: None,
        };
        let mut buf = [0u8; HeartbeatAck::SIZE];
        ack.serialize(&mut buf);

        // SIZE is now 20 bytes (was 16 before state_flags was added in PATCH-1)
        assert_eq!(buf.len(), 20);
        assert_eq!(HeartbeatAck::SIZE, 20);

        let decoded = HeartbeatAck::deserialize(&buf).unwrap();
        assert_eq!(decoded.echo_sequence, 100);
        assert_eq!(decoded.echo_time, 999_999);
        assert!(decoded.capabilities.has(Capabilities::WIDEBAND_AUDIO));
        assert!(decoded.state_flags.has(ServerStateFlags::TCI_CONNECTED));
    }

    #[test]
    fn heartbeat_ack_backward_compat_12_byte() {
        // Old 12-byte HeartbeatAck (pre-Capabilities, pre-state_flags)
        let mut buf = [0u8; 12];
        buf[0] = MAGIC;
        buf[1] = VERSION;
        buf[2] = PacketType::HeartbeatAck as u8;
        buf[3] = 0;
        buf[4..8].copy_from_slice(&55u32.to_be_bytes());
        buf[8..12].copy_from_slice(&12345u32.to_be_bytes());
        let decoded = HeartbeatAck::deserialize(&buf).unwrap();
        assert_eq!(decoded.echo_sequence, 55);
        assert_eq!(decoded.echo_time, 12345);
        assert_eq!(decoded.capabilities, Capabilities::NONE);
        assert_eq!(decoded.state_flags, ServerStateFlags::NONE);
    }

    #[test]
    fn heartbeat_ack_intermediate_compat_16_byte() {
        // Intermediate 16-byte HeartbeatAck (capabilities present, state_flags absent —
        // this is what a v2.0.0 release-tag server sends, before PATCH-1).
        // The decoder must accept it and fill state_flags with NONE.
        let mut buf = [0u8; 16];
        buf[0] = MAGIC;
        buf[1] = VERSION;
        buf[2] = PacketType::HeartbeatAck as u8;
        buf[3] = 0;
        buf[4..8].copy_from_slice(&77u32.to_be_bytes());
        buf[8..12].copy_from_slice(&54321u32.to_be_bytes());
        buf[12..16].copy_from_slice(&Capabilities::WIDEBAND_AUDIO.to_be_bytes());
        let decoded = HeartbeatAck::deserialize(&buf).unwrap();
        assert_eq!(decoded.echo_sequence, 77);
        assert_eq!(decoded.echo_time, 54321);
        assert!(decoded.capabilities.has(Capabilities::WIDEBAND_AUDIO));
        // Key invariant: when state_flags absent, default to NONE — caller must
        // NOT assume "no TCI_CONNECTED bit = TCI down" (compatibility-regression check).
        assert_eq!(decoded.state_flags, ServerStateFlags::NONE);
    }

    #[test]
    fn packet_dispatch() {
        // Audio
        let audio = AudioPacket {
            flags: Flags::NONE,
            sequence: 1,
            timestamp: 0,
            opus_data: vec![0x00],
        };
        let mut buf = Vec::new();
        audio.serialize(&mut buf);
        assert!(matches!(Packet::deserialize(&buf).unwrap(), Packet::Audio(_)));

        // Heartbeat
        let hb = Heartbeat {
            flags: Flags::NONE,
            sequence: 1,
            local_time: 0,
            rtt_ms: 0,
            loss_percent: 0,
            jitter_ms: 0,
            capabilities: Capabilities::NONE,
        };
        let mut buf = [0u8; Heartbeat::SIZE];
        hb.serialize(&mut buf);
        assert!(matches!(
            Packet::deserialize(&buf).unwrap(),
            Packet::Heartbeat(_)
        ));

        // HeartbeatAck
        let ack = HeartbeatAck {
            flags: Flags::NONE,
            echo_sequence: 1,
            echo_time: 0,
            capabilities: Capabilities::NONE,
            state_flags: ServerStateFlags::NONE,
            subs: None,
        };
        let mut buf = [0u8; HeartbeatAck::SIZE];
        ack.serialize(&mut buf);
        assert!(matches!(
            Packet::deserialize(&buf).unwrap(),
            Packet::HeartbeatAck(_)
        ));
    }

    #[test]
    fn flags_operations() {
        let f = Flags::NONE;
        assert!(!f.ptt());

        let f = f.with_ptt(true);
        assert!(f.ptt());
        assert_eq!(f, Flags::PTT);

        let f = f.with_ptt(false);
        assert!(!f.ptt());
        assert_eq!(f, Flags::NONE);
    }

    #[test]
    fn audio_packet_truncated() {
        let packet = AudioPacket {
            flags: Flags::NONE,
            sequence: 1,
            timestamp: 0,
            opus_data: vec![0x01, 0x02, 0x03],
        };
        let mut buf = Vec::new();
        packet.serialize(&mut buf);

        // Truncate: keep header but cut opus data
        buf.truncate(AudioPacket::HEADER_SIZE + 1);
        assert!(AudioPacket::deserialize(&buf).is_err());
    }
}

#[cfg(test)]
mod subscription_mask_tests {
    use super::*;

    /// The trap this field could fall into: an older server stops at 20 bytes,
    /// and reading that as "nothing subscribed" would report a disagreement
    /// against every one of them, once a second, for ever.
    #[test]
    fn an_older_server_is_unknown_and_not_empty() {
        let ack = HeartbeatAck {
            flags: Flags::NONE,
            echo_sequence: 1,
            echo_time: 2,
            capabilities: Capabilities::NONE,
            state_flags: ServerStateFlags::NONE,
            subs: Some(SubscriptionMask(SubscriptionMask::RX1_AUDIO)),
        };
        let mut old_buf = [0u8; HeartbeatAck::SIZE];
        ack.serialize(&mut old_buf);
        let read = HeartbeatAck::deserialize(&old_buf).unwrap();
        assert_eq!(read.subs, None, "absent must not read as an empty mask");
    }

    #[test]
    fn the_mask_survives_the_wire() {
        let mine = SubscriptionMask(
            SubscriptionMask::RX1_AUDIO | SubscriptionMask::VRX2 | SubscriptionMask::YAESU2,
        );
        let ack = HeartbeatAck {
            flags: Flags::NONE,
            echo_sequence: 7,
            echo_time: 8,
            capabilities: Capabilities::NONE,
            state_flags: ServerStateFlags::NONE,
            subs: Some(mine),
        };
        let mut buf = [0u8; HeartbeatAck::SIZE_WITH_SUBS];
        ack.serialize_with_subs(&mut buf);
        let read = HeartbeatAck::deserialize(&buf).unwrap();
        assert_eq!(read.subs, Some(mine));
        assert_eq!(read.echo_sequence, 7, "the older fields are untouched");
    }

    /// A newer client against an older server, and the reverse, must both keep
    /// reading everything that was there before.
    #[test]
    fn the_older_fields_still_read_from_the_longer_packet() {
        let ack = HeartbeatAck {
            flags: Flags::NONE,
            echo_sequence: 42,
            echo_time: 99,
            capabilities: Capabilities::NONE.with(Capabilities::REPORTS_STATE_FLAGS),
            state_flags: ServerStateFlags::NONE.with(ServerStateFlags::TCI_CONNECTED),
            subs: Some(SubscriptionMask(0)),
        };
        let mut buf = [0u8; HeartbeatAck::SIZE_WITH_SUBS];
        ack.serialize_with_subs(&mut buf);
        let read = HeartbeatAck::deserialize(&buf).unwrap();
        assert_eq!(read.echo_time, 99);
        assert!(read.capabilities.has(Capabilities::REPORTS_STATE_FLAGS));
        assert!(read.state_flags.has(ServerStateFlags::TCI_CONNECTED));
        assert_eq!(read.subs, Some(SubscriptionMask(0)), "an explicit empty mask IS a statement");
    }

    #[test]
    fn a_difference_is_reported_by_name() {
        let names = SubscriptionMask::names_of(
            SubscriptionMask::RX2_AUDIO | SubscriptionMask::VRX1,
        );
        assert_eq!(names, vec!["rx2-audio", "vrx1"]);
        assert!(SubscriptionMask::names_of(0).is_empty());
    }
}
