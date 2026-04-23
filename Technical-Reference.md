# ThetisLink - Technical Reference

## 1. Overview

ThetisLink is a system for remote operation of an ANAN 7000DLE + Thetis SDR receiver and a Yaesu FT-991A transceiver over a network connection. It provides bidirectional real-time audio streaming, PTT control, DDC spectrum/waterfall display, full RX2/VFO-B support, diversity, Yaesu memory channel management and radio settings editor over UDP with Opus codec.

**Version:** v1.0.0 (shared version number in `sdr-remote-core::VERSION`)
**Development language:** Rust + Kotlin (Android UI)
**Target platform:** Windows 10/11, macOS (Intel/Apple Silicon), Android 8+ (arm64)
**Design priority:** latency > bandwidth > features

### Thetis compatibility

ThetisLink requires **Thetis v2.10.3.13** or newer (the latest official release by Richard Samphire, MW0LGE / [ramdor/Thetis](https://github.com/ramdor/Thetis)). Older Thetis versions do not fully support the TCI WebSocket protocol — in particular IQ streaming and audio routing do not work correctly.

There is also a **PA3GHM ThetisLink fork** ([cjenschede/Thetis](https://github.com/cjenschede/Thetis/tree/thetislink-tci-extended), branch `thetislink-tci-extended`) with extensions specifically for ThetisLink. This fork adds:

- **Extended IQ spectrum**: up to **1536 kHz** IQ bandwidth via TCI (standard Thetis is limited to 384 kHz)
- **TCI `_ex` commands**: CTUN, VFO sync, step attenuator, FM deviation, diversity, DDC sample rate, AGC auto, VFO swap — all via TCI instead of CAT
- **Push notifications**: real-time state updates to TCI clients when Thetis state changes
- **Diversity auto-null**: Smart and Ultra algorithms running server-side in Thetis (DSP speed)
- **BroadcastDiversityPhase/Gain**: real-time circle plot updates during auto-null sweep

All extensions are behind the **"ThetisLink extensions"** checkbox in Setup → Network → IQ Stream. With this option unchecked, Thetis behaves identically to the original v2.10.3.13 release. The current ThetisLink fork build tag is **TL-26**.

The default IQ sample rate is 384 kHz. With ThetisLink extensions the user can choose from: 48, 96, 192, 384, 768 or **1536 kHz** — selectable per receiver via the DDC sample rate dropdown in the client.

**Repos:**
- ThetisLink: [cjenschede/sdr-remote](https://github.com/cjenschede/sdr-remote) (branch `ThetisLink-TCI`)
- Thetis fork: [cjenschede/Thetis](https://github.com/cjenschede/Thetis) (branch `thetislink-tci-extended`)
- Original Thetis: [ramdor/Thetis](https://github.com/ramdor/Thetis)

---

## 2. Architecture

ThetisLink uses a TCI WebSocket connection to Thetis for audio, IQ and commands. A parallel TCP CAT connection handles commands that TCI does not (yet) support.

```mermaid
flowchart LR
    subgraph Client["Client (laptop/phone)"]
        Mic[Microphone] --> OpusEnc[Opus Encode]
        OpusDec[Opus Decode] --> Speaker[Speaker]
        UI[UI: egui / Compose]
    end
    subgraph Server["Server (PC next to Thetis)"]
        TCI["TCI WebSocket :40001"]
        CAT["CAT TCP :13013<br>(optional)"]
    end
    OpusEnc -->|"UDP :4580<br>Audio + PTT"| TCI
    TCI -->|"UDP :4580<br>Audio"| OpusDec
    TCI -->|TX_AUDIO_STREAM| Thetis[Thetis]
    Thetis -->|RX_AUDIO_STREAM| TCI
```

Audio goes directly via the TCI WebSocket (RX_AUDIO_STREAM / TX_AUDIO_STREAM). The CAT connection is optional (only without PA3GHM fork) for commands that TCI does not support.

### Client Architecture

```mermaid
flowchart LR
    UI["SdrRemoteApp<br>(egui UI)<br>main thread"] -->|"mpsc::Sender<Command>"| Engine["ClientEngine<br>(sdr-remote-logic)<br>tokio thread"]
    Engine -->|"watch::Receiver<RadioState>"| UI
    Engine --- Audio["AudioBackend<br>ClientAudio (cpal)"]
```

The UI reads state via a `watch` channel (non-blocking borrow) and sends commands via an `mpsc` channel. The engine owns all network and audio state. This makes the business logic platform-independent: the Android client only implements `AudioBackend` (Oboe) and the UI (Compose), the engine remains identical.

### Android Client Architecture

```mermaid
flowchart LR
    Compose["Jetpack Compose<br>(Kotlin UI)<br>main thread"] -->|"connect/setPtt/etc"| Bridge["bridge.rs<br>(UniFFI FFI bridge)"]
    Bridge -->|"BridgeRadioState"| VM["SdrViewModel<br>(StateFlow)"]
    VM -->|collectAsStateWithLifecycle| Compose
    Bridge --- Engine["ClientEngine<br>(sdr-remote-logic)"]
    Engine --- Audio["AudioBackend<br>OboeAudio (AAudio)"]
    VM -->|"getState()<br>polling 33ms"| Bridge
```

**UniFFI bridge:** `sdr_remote.udl` defines the FFI interface. `uniffi-bindgen` generates Kotlin bindings. The bridge runs the `ClientEngine` in a tokio runtime on a background thread. UI polls state via `getState()` at 30fps.

**Audio:** Oboe (AAudio) with `PerformanceMode::LowLatency`, `SharingMode::Exclusive`. Capture: `InputPreset::VoiceRecognition` (no AGC/noise suppression). Playback: `Usage::Media`.

### Server Architecture

```mermaid
flowchart TB
    PTT["PttController<br>PTT safety (timeouts, tail delay)"]
    PTT --> TCI["RadioBackend::Tci<br>TciConnection"]
    TCI --> WS["WebSocket to Thetis :40001<br>Push-based state updates<br>Audio streams RX/TX<br>IQ streams - spectrum"]
    TCI -.-> CAT["aux_cat: CatConnection<br>(optional)<br>Commands TCI does not know"]
```

---

## 3. Workspace Structure

```
sdr-remote/
├── Cargo.toml                  # Workspace root
├── Technische-Referentie.md    # Dutch technical reference (source)
├── sdr-remote-core/            # Shared library (protocol, codec, jitter, auth)
│   └── src/
│       ├── lib.rs              # Constants (sample rates, frame sizes, port)
│       ├── protocol.rs         # Packet format, serialization, deserialization
│       ├── codec.rs            # Opus encode/decode wrapper
│       ├── jitter.rs           # Adaptive jitter buffer
│       └── auth.rs             # HMAC-SHA256 authentication
├── sdr-remote-logic/           # Platform-independent client engine
│   └── src/
│       ├── lib.rs              # Crate root
│       ├── state.rs            # RadioState (read-only, broadcast via watch channel)
│       ├── commands.rs         # Command enum (UI -> engine via mpsc channel)
│       ├── audio.rs            # AudioBackend trait (platform abstraction)
│       └── engine.rs           # ClientEngine (network, codec, resampling, jitter)
├── sdr-remote-server/          # Windows server (runs next to Thetis)
│   └── src/
│       ├── main.rs             # Startup, argument parsing, shutdown
│       ├── network.rs          # UDP send/receive, resampling, playout timer
│       ├── cat.rs              # CatConnection: Thetis CAT TCP + radio state
│       ├── tci.rs              # TciConnection: TCI WebSocket client + state + streams
│       ├── ptt.rs              # PttController: PTT safety (contains TciConnection)
│       ├── session.rs          # Client session management (multi-client)
│       ├── spectrum.rs         # SpectrumProcessor: DDC FFT pipeline + test generator
│       ├── dxcluster.rs        # DX Cluster telnet client
│       ├── tuner.rs            # JC-4s tuner controller (serial RTS/CTS)
│       ├── config.rs           # Server configuration (persistent)
│       └── ui.rs               # Server GUI
├── sdr-remote-client/          # Desktop client (egui)
│   └── src/
│       ├── main.rs             # Startup, engine + UI threading
│       ├── audio.rs            # cpal AudioBackend impl + device listing
│       ├── ui.rs               # egui UI, config storage
│       ├── websdr.rs           # Win32 window + wry WebView (embedded WebSDR/KiwiSDR)
│       └── catsync.rs          # WebSDR channel communication, favorites, debounced freq sync
└── sdr-remote-android/         # Android client (Kotlin/Compose + Rust via UniFFI)
    ├── src/
    │   ├── lib.rs              # JNI entrypoint, Android logging
    │   ├── bridge.rs           # UniFFI bridge (Rust <-> Kotlin)
    │   ├── audio_oboe.rs       # Oboe AudioBackend impl (AAudio)
    │   └── sdr_remote.udl      # UniFFI interface definition
    └── android/                # Android Studio project
        └── app/src/main/java/com/sdrremote/
            ├── MainActivity.kt
            ├── viewmodel/SdrViewModel.kt
            └── ui/
                ├── screens/MainScreen.kt
                └── components/         # Compose UI components
```

---

## 4. Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `tokio` | 1 (full) | Async runtime (UDP, TCP, timers) |
| `audiopus` | 0.3.0-rc.0 | Opus codec bindings (8kHz narrowband, 16kHz wideband) |
| `cpal` | 0.15 | Audio I/O (WASAPI on Windows, desktop client) |
| `rubato` | 0.14 | Resampling (sinc interpolation) |
| `ringbuf` | 0.4 | Lock-free SPSC ring buffer (audio thread <-> network thread) |
| `eframe`/`egui` | 0.29 | Desktop client UI |
| `bytemuck` | 1 | Zero-copy byte casting |
| `log`/`env_logger` | 0.4/0.11 | Logging |
| `anyhow` | 1 | Error handling |
| `oboe` | 0.6 | Audio I/O (AAudio on Android) |
| `uniffi` | 0.28 | Rust <-> Kotlin FFI bridge |
| `rustfft` | 6 | FFT for spectrum processing (server) |
| `tokio-tungstenite` | 0.24 | WebSocket client for TCI (server) |
| `futures-util` | 0.3 | StreamExt/SinkExt for WebSocket (server) |
| `serialport` | 4.7 | Serial USB communication (devices) |
| `midir` | 0.10 | MIDI interface |
| `wry` | 0.x | WebView for embedded WebSDR/KiwiSDR window (client) |
| `hmac`/`sha2` | — | HMAC-SHA256 authentication |
| `rand` | — | Random nonce generation |

**Build optimization:** Dependencies are also optimized in dev mode (`[profile.dev.package."*"] opt-level = 2`) because Opus and rubato are too slow without optimization.

---

## 5. UDP Protocol

Binary hand-written protocol on UDP port **4580**. All multi-byte values are big-endian. Maximum packet size: 33,000 bytes (for spectrum data).

### Header (4 bytes)

Every packet starts with the same header:

| Offset | Size | Field | Value |
|--------|------|-------|-------|
| 0 | 1 | Magic | `0xAA` |
| 1 | 1 | Version | `1` |
| 2 | 1 | PacketType | See below |
| 3 | 1 | Flags | Bit 0 = PTT active |

### Packet Types

#### Audio Packet (0x01) — variable length

Carries encoded Opus audio + PTT status. Sent 50 times per second (every 20ms).

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0-3 | 4 | Header | Magic, version, type=0x01, flags |
| 4-7 | 4 | Sequence | Incrementing sequence number (u32, wrapping) |
| 8-11 | 4 | Timestamp | Milliseconds since start (u32) |
| 12-13 | 2 | OpusLen | Length of opus data in bytes (u16) |
| 14+ | N | OpusData | Opus-encoded audio |

**Total:** 14 + N bytes (typically ~40-60 bytes at 12.8 kbps narrowband, ~60-80 bytes at wideband)

PTT-only packets (without audio) have OpusLen=0 and are used for PTT burst on state change.

#### AudioRx2 (0x0E)

RX2 audio from the second receiver. Same binary format as Audio.

#### AudioYaesu (0x16)

Yaesu FT-991A audio stream. Same binary format as Audio.

#### Heartbeat (0x02) — 16-20 bytes

Periodically (every 500ms) sent by client for connection monitoring and RTT measurement.

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0-3 | 4 | Header | Magic, version, type=0x02, flags |
| 4-7 | 4 | Sequence | Heartbeat sequence number (u32) |
| 8-11 | 4 | LocalTime | Client timestamp in ms (u32) |
| 12-13 | 2 | RTT | Last measured RTT in ms (u16) |
| 14 | 1 | LossPercent | Estimated packet loss % (u8) |
| 15 | 1 | JitterMs | Estimated jitter in ms (u8) |
| 16-19 | 4 | Capabilities | Client capability flags (u32, optional) |

Backward compatible: old clients without capabilities (16 bytes) are accepted.

#### HeartbeatAck (0x03) — 12-16 bytes

Server response to Heartbeat. Echoes the client's timestamp back for RTT calculation.

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0-3 | 4 | Header | Magic, version, type=0x03, flags |
| 4-7 | 4 | EchoSequence | Echoed heartbeat sequence |
| 8-11 | 4 | EchoTime | Echoed client timestamp |
| 12-15 | 4 | Capabilities | Negotiated capabilities (u32, optional) |

**Capability flags:**

| Bit | Name | Description |
|-----|------|-------------|
| 0 | WIDEBAND_AUDIO | 16kHz wideband Opus |
| 1 | SPECTRUM | Spectrum/waterfall data |
| 2 | RX2 | RX2/VFO-B dual receiver |

Negotiation via intersection: server only sends flags both sides support.

#### Control Packet (0x04) — 7 bytes

Sends control commands (bidirectional).

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0-3 | 4 | Header | Magic, version, type=0x04, flags |
| 4 | 1 | ControlId | Control type (see below) |
| 5-6 | 2 | Value | Value (u16) |

**ControlId values — Thetis RX1:**

| ID | Name | Values | CAT/TCI |
|----|------|--------|---------|
| 0x01 | Rx1AfGain | 0-100 | `ZZLA` |
| 0x02 | PowerOnOff | 0/1 | `ZZPS` |
| 0x03 | TxProfile | 0-99 | `ZZTP` / TCI `tx_profile_ex` |
| 0x04 | NoiseReduction | 0-4 (0=off, 1=NR1..4=NR4) | TCI `rx_nb_enable` |
| 0x05 | AutoNotchFilter | 0/1 | TCI `rx_anf_enable` |
| 0x06 | DriveLevel | 0-100 | TCI `DRIVE` |
| 0x0D | ThetisStarting | 0/1 | TCI `start`/`stop` |
| 0x1E | MonitorOn | 0/1 | TCI `MON_ENABLE` |
| 0x1F | ThetisTune | 0/1 | TCI `TUNE` |

**ControlId values — Spectrum:**

| ID | Name | Values | Description |
|----|------|--------|-------------|
| 0x07 | SpectrumEnable | 0/1 | Spectrum on/off per client |
| 0x08 | SpectrumFps | 5-30 | Frame rate |
| 0x09 | SpectrumZoom | 10-10240 | Zoom level (value/10 = factor, e.g. 320=32x) |
| 0x0A | SpectrumPan | 0-10000 | Pan position ((pan+0.5)x10000, 5000=center) |
| 0x0B | FilterLow | signed Hz | Filter low cut (i16 as u16) |
| 0x0C | FilterHigh | signed Hz | Filter high cut (i16 as u16) |
| 0x1A | SpectrumMaxBins | 64-32768 | Max bins per packet (0=default 8192) |
| 0x1C | SpectrumFftSize | 32/64/128/256 | FFT size in K (0=auto) |
| 0x1D | SpectrumBinDepth | 8/16 | Bin depth: 8=u8 (1 byte/bin), 16=u16 (2 bytes/bin) |

**ControlId values — RX2/VFO-B:**

| ID | Name | Values | Description |
|----|------|--------|-------------|
| 0x0E | Rx2Enable | 0/1 | RX2 on/off |
| 0x0F | Rx2AfGain | 0-100 | RX2 AF volume (`ZZLB`) |
| 0x10 | Rx2SpectrumZoom | 10-10240 | Zoom |
| 0x11 | Rx2SpectrumPan | 0-10000 | Pan |
| 0x12 | Rx2FilterLow | signed Hz | RX2 filter low |
| 0x13 | Rx2FilterHigh | signed Hz | RX2 filter high |
| 0x14 | VfoSync | 0/1 | VFO-B follows VFO-A |
| 0x15 | Rx2SpectrumEnable | 0/1 | RX2 spectrum |
| 0x16 | Rx2SpectrumFps | 5-30 | RX2 spectrum FPS |
| 0x17 | Rx2NoiseReduction | 0-4 | RX2 NR level |
| 0x18 | Rx2AutoNotchFilter | 0/1 | RX2 ANF |
| 0x19 | VfoSwap | trigger | VFO A<->B swap (`ZZVS2`) |
| 0x1B | Rx2SpectrumMaxBins | 64-32768 | RX2 max bins |

**ControlId values — TCI Controls (v0.5.3):**

| ID | Name | Values | TCI command |
|----|------|--------|-------------|
| 0x30 | AgcMode | 0-5 (off/long/slow/med/fast/custom) | `agc_mode` |
| 0x31 | AgcGain | 0-120 | `agc_gain` |
| 0x32 | RitEnable | 0/1 | `rit_enable` |
| 0x33 | RitOffset | i16 as u16 (Hz) | `rit_offset` |
| 0x34 | XitEnable | 0/1 | `xit_enable` |
| 0x35 | XitOffset | i16 as u16 (Hz) | `xit_offset` |
| 0x36 | SqlEnable | 0/1 | `sql_enable` |
| 0x37 | SqlLevel | 0-160 | `sql_level` |
| 0x38 | NoiseBlanker | 0/1 | `rx_nb_enable` |
| 0x39 | CwKeyerSpeed | 1-60 WPM | `cw_keyer_speed` |
| 0x3A | VfoLock | 0/1 | `vfo_lock` |
| 0x3B | Binaural | 0/1 | `rx_bin_enable` |
| 0x3C | ApfEnable | 0/1 | `rx_apf_enable` |

**ControlId values — Diversity:**

| ID | Name | Values | Description |
|----|------|--------|-------------|
| 0x40 | DiversityEnable | 0/1 | Diversity on/off |
| 0x41 | DiversityRef | 0=RX2, 1=RX1 | Reference source |
| 0x42 | DiversitySource | 0=RX1+RX2, 1=RX1, 2=RX2 | RX source |
| 0x43 | DiversityGainRx1 | gain x1000 | E.g. 2500 = 2.500 |
| 0x44 | DiversityGainRx2 | gain x1000 | E.g. 2500 = 2.500 |
| 0x45 | DiversityPhase | phase x100 + 18000 | 18000=0deg, 0=-180deg, 36000=+180deg |
| 0x46 | DiversityRead | trigger | Read diversity state from Thetis |
| 0x48 | AgcAutoRx1 | 0/1 | AGC auto mode RX1 |
| 0x49 | AgcAutoRx2 | 0/1 | AGC auto mode RX2 |
| 0x4A | DiversityAutoNull | 1=start | Start Smart auto-null (Thetis-side) |

**ControlId values — RX2 TCI Controls:**

| ID | Name | Values | TCI command |
|----|------|--------|-------------|
| 0x50 | Rx2AgcMode | 0-5 | `agc_mode` rx=1 |
| 0x51 | Rx2AgcGain | 0-120 | `agc_gain` rx=1 |
| 0x52 | Rx2SqlEnable | 0/1 | `sql_enable` rx=1 |
| 0x53 | Rx2SqlLevel | 0-160 | `sql_level` rx=1 |
| 0x54 | Rx2NoiseBlanker | 0/1 | `rx_nb_enable` rx=1 |
| 0x55 | Rx2Binaural | 0/1 | `rx_bin_enable` rx=1 |
| 0x56 | Rx2ApfEnable | 0/1 | `rx_apf_enable` rx=1 |
| 0x57 | Rx2VfoLock | 0/1 | `vfo_lock` rx=1 |

**ControlId values — Extra Controls (v0.6.x):**

| ID | Name | Values | Description |
|----|------|--------|-------------|
| 0x58 | TuneDrive | 0-100 | Tune drive level |
| 0x59 | MonitorVolume | 0-100 | Monitor volume |
| 0x5A | Mute | 0/1 | Master mute |
| 0x5B | RxMute | 0/1 | RX mute |
| 0x5C | ManualNotchFilter | 0/1 | Manual notch filter RX1 |
| 0x5D | RxBalance | -100..+100 (as u16) | RX audio balance |
| 0x5E | CwKey | 0/1 | CW key down/up |
| 0x5F | CwMacroStop | trigger | Stop CW macro |
| 0x60 | Rx2ManualNotchFilter | 0/1 | Manual notch filter RX2 |
| 0x61 | ThetisSwr | SWR x100 | SWR broadcast during TX (e.g. 150 = 1.50:1) |
| 0x62 | AudioMode | 0-2 | Audio routing (0=Mono, 1=Binaural, 2=Split) |

**ControlId values — Yaesu:**

| ID | Name | Values | Description |
|----|------|--------|-------------|
| 0x20 | YaesuEnable | 0/1 | Yaesu audio+state stream |
| 0x21 | YaesuPtt | 0/1 | Yaesu TX |
| 0x22 | YaesuFreq | — | Uses FrequencyPacket format |
| 0x23 | YaesuMicGain | gain x10 | ThetisLink TX gain (200 = 20.0x) |
| 0x24 | YaesuMode | mode nr | Operating mode |
| 0x25 | YaesuReadMemories | trigger | Read all memories |
| 0x26 | YaesuRecallMemory | 1-99 | Memory channel recall |
| 0x27 | YaesuWriteMemories | trigger | Write all memories |
| 0x28 | YaesuSelectVfo | 0=A, 1=B, 2=swap | VFO selection |
| 0x29 | YaesuSquelch | 0-255 | Squelch level |
| 0x2A | YaesuRfGain | 0-255 | RF gain |
| 0x2B | YaesuRadioMicGain | 0-100 | Radio mic gain (not ThetisLink TX gain) |
| 0x2C | YaesuRfPower | 0-100 | RF power |
| 0x2D | YaesuButton | button ID | Raw CAT button |
| 0x2E | YaesuReadMenus | trigger | Read all 153 EX menu items |
| 0x2F | YaesuSetMenu | menu nr | Set EX menu item |

Control packets are **bidirectional**: client->server sends changes, server->client broadcasts current state.

#### Disconnect Packet (0x05) — 4 bytes

Clean disconnect. Header only, no payload.

#### PttDenied Packet (0x06) — 4 bytes

Server -> client. Sent when a client requests PTT while another client holds the TX lock.

#### Frequency Packet (0x07) — 12 bytes

Bidirectional: server->client (readback) and client->server (set).

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0-3 | 4 | Header | Magic, version, type=0x07, flags |
| 4-11 | 8 | FrequencyHz | VFO-A frequency in Hz (u64) |

#### FrequencyRx2 (0x0F) — 12 bytes

Same format as Frequency, for VFO-B.

#### FrequencyYaesu (0x18) — 12 bytes

Same format, for Yaesu frequency setting.

#### Mode Packet (0x08) — 5 bytes

Bidirectional. Mode values: 00=LSB, 01=USB, 05=FM, 06=AM (Thetis ZZMD values).

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0-3 | 4 | Header | Magic, version, type=0x08, flags |
| 4 | 1 | Mode | Operating mode (u8) |

#### ModeRx2 (0x10) — 5 bytes

Same format, for RX2 mode.

#### S-meter Packet (0x09) — 6 bytes

Server->client. Raw value 0-260 (12 per S-unit, S9=108). During TX: forward power in watts x10.

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0-3 | 4 | Header | Magic, version, type=0x09, flags |
| 4-5 | 2 | Level | S-meter level or TX power (u16) |

#### SmeterRx2 (0x11) — 6 bytes

Same format, RX2 S-meter.

#### Spectrum Packet (0x0A) — 18 + N bytes

Server->client. Spectrum data centered on VFO frequency (DDC I/Q mode). Number of bins is dynamic per client (depending on screen resolution x zoom).

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0-3 | 4 | Header | Magic, version, type=0x0A, flags |
| 4-5 | 2 | Sequence | Frame sequence number (u16, wrapping) |
| 6-7 | 2 | NumBins | Number of spectrum bins (u16, 512-32768) |
| 8-11 | 4 | CenterFreqHz | Center frequency in Hz (u32) |
| 12-15 | 4 | SpanHz | Span in Hz (u32, = DDC sample rate) |
| 16 | 1 | RefLevel | Reference level dBm (i8) |
| 17 | 1 | DbPerUnit | Bin depth: 1=u8 (1 byte/bin), 2=u16 (2 bytes/bin) |
| 18+ | NxB | Bins | Log power values (u8: 0-255 or u16: 0-65535, maps -150...-30 dB) |

**Dynamic bins:** Desktop client automatically calculates `screen_width x zoom` (range 512-32768). Server extracts the correct number of bins from the FFT smoothed buffer per client.

#### FullSpectrum (0x0B) / FullSpectrumRx2 (0x13)

Full DDC spectrum row for waterfall history. Same format as Spectrum.

#### SpectrumRx2 (0x12)

RX2 spectrum data. Same format as Spectrum.

#### EquipmentStatus (0x0C) — variable length

Server -> client, every 200ms. Status of external device.

| Field | Type | Description |
|-------|------|-------------|
| device_type | u8 | DeviceType enum |
| switch_a | u8 | Device-specific |
| switch_b | u8 | Device-specific |
| connected | bool | Hardware online |
| labels | Option\<String\> | Comma-separated extra data |

#### EquipmentCommand (0x0D) — variable length

Client -> server. Command to external device.

| Field | Type | Description |
|-------|------|-------------|
| device_type | u8 | Target device |
| command_id | u8 | Command (device-specific) |
| data | Vec\<u8\> | Parameters (variable length) |

#### Spot (0x14) — variable length

DX cluster spot (server -> client).

#### TxProfiles (0x15) — variable length

TX profile list with names (server -> client).

#### YaesuState (0x17) — variable length

Yaesu radio state (server -> client).

#### YaesuMemoryData (0x19) — variable length

Yaesu memory data (server -> client, tab-separated text).

#### AudioBinR (0x1A) — [deprecated]

Binaural right channel audio. Replaced by AudioMultiCh (0x1B).

#### AudioMultiCh (0x1B) — variable length

Multi-channel audio: bundles 1-4 mono Opus frames in a single UDP packet. Perfect sync: all channels share a sequence number and timestamp.

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0-3 | 4 | Header | Magic, version, type=0x1B, flags |
| 4-7 | 4 | Sequence | Incrementing sequence number (u32, wrapping) |
| 8-11 | 4 | Timestamp | Milliseconds since start (u32) |
| 12 | 1 | ChannelCount | Number of channels (1-4) |
| 13+ | variable | Channels | Per channel: channel_id(1) + opus_len(2) + opus_data(N) |

**Channel IDs:**

| ID | Channel | Description |
|----|---------|-------------|
| 0 | RX1 | RX1 mono (or RX1-L with binaural) |
| 1 | BinR | RX1-R binaural right (only present when BIN on) |
| 2 | RX2 | RX2 mono |
| 3 | Yaesu | Reserved for future bundling |

**Per-client filtering:** Server sends all channels, client filters based on platform and audio routing mode. Android receives only CH0 (RX1) by default.

#### Authentication Packets

| Type ID | Name | Direction | Payload |
|---------|------|-----------|---------|
| 0x30 | AuthChallenge | Server -> Client | 16-byte nonce |
| 0x31 | AuthResponse | Client -> Server | 32-byte HMAC |
| 0x32 | AuthResult | Server -> Client | 1 byte: 0=rejected, 1=accepted, 2=totp_required |
| 0x33 | TotpChallenge | Server -> Client | (no payload, signals TOTP input) |
| 0x34 | TotpResponse | Client -> Server | 2-byte length + UTF-8 TOTP code (6 digits) |

---

## 6. Opus Codec Configuration

### Narrowband (RX audio)

| Parameter | Value | Reason |
|-----------|-------|--------|
| Sample rate | 8 kHz | Narrowband, sufficient for SSB/CW reception |
| Channels | Mono | Single audio channel |
| Application | VOIP | Optimized for voice |
| Bitrate | 12,800 bps | Just above 12.4k FEC threshold |
| Bandwidth | Narrowband | Matches 8 kHz |
| Frame duration | 20 ms | 160 samples per frame |
| FEC | On | In-band Forward Error Correction |
| DTX | On | Discontinuous Transmission (silence suppression) |
| Expected loss | 10% | Optimizes FEC overhead |

### Wideband (TX audio)

| Parameter | Value | Reason |
|-----------|-------|--------|
| Sample rate | 16 kHz | Wideband, better voice quality for TX |
| Channels | Mono | Single audio channel |
| Application | VOIP | Optimized for voice |
| Bitrate | ~24 kbps | Wideband quality |
| Frame duration | 20 ms | 320 samples per frame |
| FEC | On | In-band Forward Error Correction |

### Constants (`sdr-remote-core/src/lib.rs`)

| Constant | Value | Description |
|----------|-------|-------------|
| `NETWORK_SAMPLE_RATE` | 8000 | Narrowband Opus rate |
| `NETWORK_SAMPLE_RATE_WIDEBAND` | 16000 | Wideband Opus rate |
| `DEVICE_SAMPLE_RATE` | 48000 | Audio device rate |
| `FRAME_DURATION_MS` | 20 | Opus frame duration |
| `FRAME_SAMPLES` | 160 | 8kHz x 20ms |
| `FRAME_SAMPLES_WIDEBAND` | 320 | 16kHz x 20ms |
| `DEVICE_FRAME_SAMPLES` | 960 | 48kHz x 20ms |
| `DEFAULT_PORT` | 4580 | Server UDP port |
| `MAX_PACKET_SIZE` | 33000 | Max UDP packet size |
| `DEFAULT_SPECTRUM_BINS` | 8192 | Default spectrum bins |
| `MAX_SPECTRUM_SEND_BINS` | 32768 | Max spectrum bins per packet |
| `DDC_FFT_SIZE` | 262144 | FFT size for DDC I/Q |
| `FULL_SPECTRUM_BINS` | 8192 | Full waterfall row bins |
| `DEFAULT_SPECTRUM_FPS` | 15 | Default spectrum FPS |

**Bandwidth:** ~30 kbps per direction (narrowband RX), ~60 kbps total including overhead.

---

## 7. Resampling

All resampling uses **rubato SincFixedIn** with sinc interpolation for high audio quality.

### Parameters

| Parameter | Value |
|-----------|-------|
| Sinc length | 128 taps |
| Cutoff frequency | 0.95 (relative to Nyquist) |
| Oversampling factor | 128 |
| Interpolation | Cubic |
| Window function | Blackman |

### Why SincFixedIn (and not FftFixedIn)

`FftFixedIn` with only 320 input samples (16kHz x 20ms) has too little FFT resolution for a good anti-aliasing filter. Frequencies above 4kHz fold back as audible artifacts. `SincFixedIn` with 128-point sinc filter and Blackman window gives cleaner audio, regardless of the input frame size.

### Resample paths

| Path | From | To | Where |
|------|------|----|-------|
| Server RX (TCI -> client) | 48kHz (TCI) | 8kHz | TCI RX_AUDIO_STREAM -> Opus narrowband encoder |
| Server TX (client -> TCI) | 16kHz (Opus wideband) | 48kHz | Opus wideband decode -> TCI TX_AUDIO_STREAM |
| Client TX (48kHz mic) | 48kHz | 16kHz | Resample ratio 3:1 |
| Client TX (16kHz headset) | 16kHz | 16kHz | **1:1 (no resample needed)** |
| Client RX | 8kHz | Device rate (e.g. 44.1kHz) | Opus decode -> Speaker playback |

---

## 8. PTT Safety (5 layers)

PTT safety is critical: a stuck transmitter can damage the final amplifier and interferes with other users.

### Layer 1: PTT flag in every audio packet

Every audio packet (50x/sec) carries the PTT status in the flags byte. The server checks this on every received packet.

### Layer 2: Burst on state change

On PTT on/off the client sends **5 copies** of the audio packet. This guarantees the server sees the state change, even with packet loss.

### Layer 3: Packet timeout (500ms)

If the server receives **no packets for 500ms** while PTT is active, PTT is automatically disabled. Log level: WARN.

### Layer 4: Heartbeat timeout (2s)

If the server receives **no heartbeat for 2 seconds**, the connection is considered lost and PTT is disabled. Log level: ERROR (emergency).

### Layer 5: PTT tail delay (150ms)

When PTT is released, the server waits **150ms** before disabling PTT via TCI (`TRX:0,false;`). This gives the audio pipeline (jitter buffer + resampling) time to drain, so the last audio is not cut off.

### Safety check loop

The server performs a safety check every **100ms** (PTT timeouts + state polling) and every **1 second** a session timeout check + reconnect attempt.

---

## 9. TCI Protocol

TCI (Transceiver Control Interface) is a WebSocket-based protocol built into Thetis. Default on `ws://127.0.0.1:40001`.

### Connection

1. WebSocket connect to Thetis TCI server
2. Wait for `READY;` notification
3. Subscribe to sensors: `RX_SENSORS_ENABLE:true,100;`, `TX_SENSORS_ENABLE:true,100;`
4. Start audio: `AUDIO_SAMPLERATE:48000;`, `AUDIO_START:0;`, `AUDIO_START:1;`
5. Start IQ: `IQ_SAMPLERATE:{rate};`, `IQ_START:0;`

### Push-based State Updates (text messages)

| TCI notification | Description | Mapped state |
|-----------------|-------------|-------------|
| `vfo:0,0,freq;` | VFO-A frequency | vfo_a_freq |
| `vfo:0,1,freq;` / `vfo:1,0,freq;` | VFO-B frequency | vfo_b_freq |
| `modulation:0,mode;` | Operating mode RX1 | vfo_a_mode |
| `modulation:1,mode;` | Operating mode RX2 | vfo_b_mode |
| `trx:0,bool;` | TX active | tx_active |
| `drive:0,val;` | Drive level | drive_level |
| `rx_filter_band:0,lo,hi;` | RX1 filter boundaries | filter_low_hz, filter_high_hz |
| `rx_filter_band:1,lo,hi;` | RX2 filter boundaries | rx2_filter_low/high |
| `dds:0,freq;` | DDC center frequency RX1 | dds_freq[0] |
| `dds:1,freq;` | DDC center frequency RX2 | dds_freq[1] |
| `rx_channel_sensors:R,C,dBm;` | S-meter (dBm) | smeter |
| `tx_sensors:R,mic,pwr,peak,swr;` | TX telemetry | fwd_power, swr |
| `start;` / `stop;` | Thetis start/stop | thetis_starting |
| `mon_enable:bool;` | TX Monitor | mon_on |
| `agc_mode:R,mode;` | AGC mode | agc_mode |
| `agc_gain:R,gain;` | AGC gain | agc_gain |
| `rit_enable:R,bool;` | RIT | rit_enable |
| `rit_offset:R,hz;` | RIT offset | rit_offset |
| `xit_enable:R,bool;` | XIT | xit_enable |
| `xit_offset:R,hz;` | XIT offset | xit_offset |
| `sql_enable:R,bool;` | Squelch enable | sql_enable |
| `sql_level:R,level;` | Squelch level | sql_level |
| `rx_nb_enable:R,bool;` | Noise Blanker | nb_enable |
| `cw_keyer_speed:speed;` | CW keyer speed | cw_keyer_speed |
| `vfo_lock:bool;` | VFO lock | vfo_lock |
| `rx_bin_enable:R,bool;` | Binaural | binaural |
| `rx_apf_enable:R,bool;` | Audio Peak Filter | apf_enable |
| `tx_profiles_ex:names;` | TX profile names (list) | tx_profile_names |
| `tx_profile_ex:name;` | Active TX profile name | tx_profile_name |
| `rx_channel_enable:R,C,bool;` | RX channel enable | — |
| `volume:db;` | Master volume | — |

### Binary Streams

| Stream type | Type code | Data |
|------------|-----------|------|
| `IQ_STREAM` | 0 | Complex float32 I/Q pairs |
| `RX_AUDIO_STREAM` | 1 | PCM int16/int32/float32 |
| `TX_AUDIO_STREAM` | 2 | PCM int16/int32/float32 |
| `TX_CHRONO` | 3 | Server sends TX audio as response |

TCI binary header: 16 x u32 = 64 bytes. Stream type at offset 24, sample format at offset 8 (0=int16, 2=int32, 3=float32).

### Commands via TCI

| Action | TCI command |
|--------|-------------|
| PTT on/off | `TRX:0,true;` / `TRX:0,false;` |
| VFO-A freq | `VFO:0,0,{freq};` |
| VFO-B freq | `VFO:0,1,{freq};` |
| Mode | `MODULATION:0,{mode_str};` |
| Drive | `DRIVE:0,{val};` |
| Filter | `RX_FILTER_BAND:0,{lo},{hi};` |
| NR | `RX_NB_ENABLE:0,{bool};` |
| ANF | `RX_ANF_ENABLE:0,{bool};` |
| Tune | `TUNE:0,{bool};` |
| Monitor | `MON_ENABLE:{bool};` |
| AGC mode | `AGC_MODE:{R},{mode};` |
| AGC gain | `AGC_GAIN:{R},{gain};` |
| RIT | `RIT_ENABLE:{R},{bool};` / `RIT_OFFSET:{R},{hz};` |
| XIT | `XIT_ENABLE:{R},{bool};` / `XIT_OFFSET:{R},{hz};` |
| Squelch | `SQL_ENABLE:{R},{bool};` / `SQL_LEVEL:{R},{level};` |
| NB | `RX_NB_ENABLE:{R},{bool};` |
| CW speed | `CW_KEYER_SPEED:{speed};` |
| VFO lock | `VFO_LOCK:{bool};` |
| Binaural | `RX_BIN_ENABLE:{R},{bool};` |
| APF | `RX_APF_ENABLE:{R},{bool};` |

---

## 10. CAT Interface (supplementary)

Communication with Thetis SDR via TCP (TS-2000 compatible protocol). Runs as a parallel connection alongside TCI for commands that TCI does not support.

| Setting | Value |
|---------|-------|
| Protocol | TCP/IP |
| Default address | `127.0.0.1:13013` |
| Command format | ZZ-prefix (Thetis extension of TS-2000) |
| Connection timeout | 200ms on connect attempt |

### Commands via parallel CAT (not in TCI)

| Command | Description | Direction |
|---------|-------------|-----------|
| `ZZLA` / `ZZLA{:03};` | RX1 AF volume (000-100) | Bidirectional |
| `ZZLB` / `ZZLB{:03};` | RX2 AF volume (000-100) | Bidirectional |
| `ZZBY` | Shutdown | Server -> Thetis |
| `ZZCT` | CTUN status (0=off, 1=on) | Server -> Thetis (poll) |
| `ZZPS` / `ZZPS0;`/`ZZPS1;` | Power on/off | Bidirectional |
| `ZZTP` / `ZZTP{:02};` | TX profile index | Bidirectional |

### Other CAT commands

| Command | Description | Direction |
|---------|-------------|-----------|
| `ZZTX1;`/`ZZTX0;` | PTT on/off (fallback) | Server -> Thetis |
| `ZZAG100;` | Master AF volume at 100% | Server -> Thetis (on connect) |
| `ZZFA;` / `ZZFA{:011};` | VFO-A frequency | Bidirectional |
| `ZZMD;` / `ZZMD{:02};` | Operating mode | Bidirectional |
| `ZZRM1;` | RX1 S-meter (average) | Server -> Thetis (poll, RX) |
| `ZZRM2;` | RX2 S-meter | Server -> Thetis (poll) |
| `ZZRM5;` | Forward power (watts) | Server -> Thetis (poll, TX) |
| `ZZPC;` / `ZZPC{:03};` | Drive level (000-100) | Bidirectional |
| `ZZNE;` / `ZZNE{0-4};` | NR level RX1 | Bidirectional |
| `ZZNT;` / `ZZNT0;`/`ZZNT1;` | ANF RX1 | Bidirectional |
| `ZZFL;` / `ZZFH;` | RX1 filter low/high | Bidirectional |
| `ZZFB;` / `ZZFB{:011};` | VFO-B frequency | Bidirectional |
| `ZZME;` / `ZZME{:02};` | RX2 mode | Bidirectional |
| `ZZFS;` / `ZZFR;` | RX2 filter low/high | Bidirectional |
| `ZZNV;` / `ZZNV{0-4};` | RX2 NR level | Bidirectional |
| `ZZNU;` / `ZZNU0;`/`ZZNU1;` | RX2 ANF | Bidirectional |
| `ZZVS2;` | VFO A<->B swap | Server -> Thetis |

### Polling intervals

| Poll | Interval | Commands |
|------|----------|----------|
| Meter | 100ms | `ZZRM1;` (RX1) / `ZZRM2;` (RX2) / `ZZRM5;` (TX) |
| Full poll | 500ms | `ZZFA;ZZMD;ZZTX;ZZPS;ZZTP;ZZNE;ZZNT;ZZPC;ZZLA;ZZFL;ZZFH;ZZCT;ZZFB;ZZME;ZZLB;ZZFS;ZZFR;ZZNV;ZZNU;` |

---

## 11. Jitter Buffer

Adaptive jitter buffer based on RFC 3550 jitter estimation with dual-alpha EMA.

### Operation

1. **Push:** Received packets are sorted by sequence number in a `BTreeMap`
2. **Pull:** Frames are delivered in order, one per 20ms playout tick
3. **Initialization:** Waits until `target_depth` frames are buffered before playout starts
4. **Missing frames:** Triggers Opus FEC (Forward Error Correction) or PLC (Packet Loss Concealment)
5. **Late packets:** Discarded if the sequence number has already passed

### Configuration

| Parameter | Value | Description |
|-----------|-------|-------------|
| min_depth | 3 frames | Minimum 60ms buffer |
| max_depth | 20 frames | Maximum 400ms buffer (mobile networks) |
| initial_fill | 25 frames | Grace period after init/reset |

### Jitter estimation (dual-alpha EMA)

```
ts_diff_ms = (packet.timestamp - last_timestamp) / 8.0    // 8kHz sample rate
arrival_diff = packet.arrival_ms - last_arrival_ms
deviation = |arrival_diff - ts_diff_ms|
alpha = if deviation > jitter_estimate { 0.25 } else { 1/16 }  // Fast up, slow down
jitter_estimate += (deviation - jitter_estimate) * alpha
```

The dual-alpha approach ensures the buffer grows immediately on a latency spike (4G in a tunnel) but shrinks slowly when the network stabilizes. Prevents stuttering without unnecessary latency on LAN.

### Spike peak hold

In addition to the EMA jitter estimate there is a spike peak hold with instant attack and ~1 minute exponential decay:

```
if deviation > spike_peak:
    spike_peak = deviation          // instant attack
else:
    spike_peak *= 1.0 - (1.0 / 3000.0)  // At 50 pkt/sec: 3000 = 1 minute decay
```

Target depth uses the higher of jitter_estimate and spike_peak:

```
desired = (max(jitter_estimate, spike_peak) / 15.0) + 2
target_depth = clamp(desired, min_depth, max_depth)
```

### Overflow recovery

On variable networks (WiFi, 4G/5G) packets can arrive in bursts causing the buffer to fill up. Because playout rate == arrival rate (both 50 frames/sec) the buffer does not drain back to target depth on its own.

**3-part solution:**
1. **`pull()` gradual recovery:** If `depth > target_depth + 4`, drop 1 frame per pull(). Spreads recovery over multiple ticks — no audible click/stutter.
2. **`push()` hard limit:** `max_depth + 10` — prevents extreme buildup.
3. **Underflow recovery:** When buffer is empty, playout pauses (`refilling = true`) until buffer is back at target depth.

### Sequence wrapping

32-bit sequence numbers with correct wraparound detection:
```rust
fn is_seq_before(a: u32, b: u32) -> bool {
    a.wrapping_sub(b) > 0x8000_0000
}
```

---

## 12. Client UI

The desktop client uses **egui** (via eframe) for a cross-platform GUI.

### Screen layout

```
+─────────────────────────────────+
│  ThetisLink                     │
+─────────────────────────────────+
│  Server: [192.168.1.79:4580]    │
│  Status: Connected              │
+─────────────────────────────────+
│         +─────────+             │
│         │   PTT   │             │
│         +─────────+             │
│    (mouse or spacebar)          │
+─────────────────────────────────+
│  14.345.000 Hz  S9+10 / TX 50W │
│  Step: [100] [1k] [10k] [100k] │
│  Mode: [LSB] [USB] [AM] [FM]   │
│  M1  M2  M3  M4  M5  [Save]    │
+─────────────────────────────────+
│  [POWER ON]  [NR2]  [ANF] [AGC]│
│  [MON] [TUNE] [NB] [APF] [BIN] │
│  TX Profile: [Normal]           │
│  Drive:  ═══════●══  75%       │
│  AGC: Fast  Gain: 80           │
│  RIT: +150 Hz  XIT: OFF        │
│  SQL: 50  CW: 25 WPM           │
+─────────────────────────────────+
│  RX Volume: ────●────── 20%    │
│  TX Gain:   ──────●──── 50%    │
+─────────────────────────────────+
│  MIC: ████████░░░░░░ -12 dB    │
│  RX:  ██████░░░░░░░░ -18 dB    │
+─────────────────────────────────+
│  RTT:        12 ms              │
│  Jitter:     2.3 ms             │
│  Buffer:     3 frames           │
│  RX packets: 14523              │
+─────────────────────────────────+
```

### Thetis Controls

All controls are bidirectional: server receives push updates via TCI and broadcasts to all clients.

| Control | UI element | Behavior |
|---------|-----------|----------|
| **Power** | Toggle button | Green = on, red = off |
| **NR** | Cycle button | Click: OFF -> NR1 -> NR2 -> NR3 -> NR4 -> OFF |
| **ANF** | Toggle button | Highlighted when active |
| **AGC (TX)** | Toggle button | TX Automatic Gain Control on/off |
| **TX Profile** | Toggle button | Cycles between profiles, shows name |
| **Drive** | Slider | 0-100% |
| **Monitor** | Toggle button | TX Monitor on/off |
| **Tune** | Toggle button | Thetis TUNE |
| **AGC Mode** | Cycle button | OFF/Long/Slow/Med/Fast/Custom |
| **AGC Gain** | Slider | 0-120 |
| **RIT** | Toggle + offset | Enable + Hz offset |
| **XIT** | Toggle + offset | Enable + Hz offset |
| **SQL** | Toggle + level | Squelch enable + level (0-160) |
| **NB** | Toggle button | Noise Blanker |
| **CW Speed** | Slider | 1-60 WPM |
| **VFO Lock** | Toggle button | VFO lock |
| **Binaural** | Toggle button | Binaural mode |
| **APF** | Toggle button | Audio Peak Filter |
| **Diversity** | Panel | Enable, ref, source, gain, phase |

### PTT operation

- **Mouse click:** Press PTT button = TX, release (anywhere) = RX
- **Spacebar:** Hold down = TX, release = RX
- PTT button turns **red** during TX

### S-meter / TX Power Indicator

The meter bar is context-dependent based on PTT status:
- **RX:** S-meter with 0-260 scale (12 per S-unit, S9=108). Green up to S9, red above S9.
- **TX:** Forward power bar with 0-100W scale. Fully red.

### Configuration file

`thetislink-client.conf` (or `sdr-remote-client.conf`) next to the executable, key=value format:

```
server=192.168.1.79:4580
password=MySecret
volume=0.20
tx_gain=0.50
input_device=Microphone (RODE NT-USB)
output_device=Speakers (Realtek(R) Audio)
tx_profiles=21:Normal,25:Remote
mem1=3630000,0
mem2=7073000,0
mem3=14345000,1
spectrum_enabled=false
websdr_favorites=https://websdr.ewi.utwente.nl:8901/,https://kiwisdr.example.com:8073/
```

| Key | Description | Default |
|-----|-------------|---------|
| `server` | Server address:port | `127.0.0.1:4580` |
| `password` | Server password (required, obfuscated after saving) | empty |
| `volume` | RX volume 0.0-1.0 | `0.2` |
| `tx_gain` | TX gain 0.0-3.0 (0-300%) | `0.5` |
| `input_device` | Microphone device name (exact match) | (system default) |
| `output_device` | Speaker device name (exact match) | (system default) |
| `tx_profiles` | TX profile index:name pairs (comma-separated) | `00:Default` |
| `memN` | Memory N: frequency_hz,mode (N=1-5) | empty |
| `spectrum_enabled` | Spectrum display on/off | `false` |
| `websdr_favorites` | Favorite WebSDR/KiwiSDR URLs (comma-separated) | empty |

**TX Profiles:** The index corresponds to the position in Thetis' TX profile dropdown (0-based). In TCI mode, profile names are automatically received via `tx_profiles_ex`.

### Embedded WebSDR/KiwiSDR WebView

The desktop client provides an integrated WebView window (via `wry`, Win32 + WebView2) that allows websdr.org and KiwiSDR receivers to be operated directly from ThetisLink.

- **Frequency sync:** Tuning in ThetisLink sends the frequency (debounced, 500ms) via JavaScript injection
- **Auto-detection:** SDR type (websdr.org or KiwiSDR) is automatically recognized
- **TX mute:** On PTT the WebSDR audio is immediately muted via JavaScript (no network delay)
- **Favorites:** Persistent list in config file
- **Spectrum zoom:** On opening, the WebSDR spectrum is automatically set to maximum zoom

---

## 13. Server Architecture

### Startup

```bash
ThetisLink-Server.exe --tci ws://127.0.0.1:40001 --cat "127.0.0.1:13013"
```

The server connects via TCI WebSocket to Thetis for audio, IQ and commands. A parallel CAT connection is automatically established for supplementary commands.

### Session Management

The server supports **multiple simultaneous clients** with single-TX arbitration.

- Multiple clients can connect simultaneously and receive RX audio
- TX (PTT) is first-come-first-served: first client to press PTT gets the TX lock
- Other clients receive a `PttDenied` packet
- On disconnect or timeout: TX lock is released
- Session timeout: **15 seconds** without activity (mobile resilience)
- On new client: jitter buffer and Opus decoder are reset

### Connect flow

1. Client sends first Heartbeat to server address
2. Server sends AuthChallenge -> client responds with AuthResponse (HMAC-SHA256)
3. If TOTP enabled: Server sends AuthResult(2) + TotpChallenge -> client shows input field -> client sends TotpResponse
4. Server registers client session (`TouchResult::NewClient`)
4. Server resets jitter buffer + decoder
5. Server responds with HeartbeatAck (+ negotiated capabilities)
6. Client marks connection as "Connected" on receiving HeartbeatAck

### Disconnect flow (timeout)

1. Client receives no HeartbeatAck AND no audio packets for longer than **max(6s, rtt x 8)**
2. Client marks as "Disconnected" but buffer drains via PLC so audio resumes smoothly if packets return
3. Server notices timeout after **15 seconds** and removes session

### Packet loss tracking

Client calculates loss per heartbeat window (500ms) with EMA smoothing (alpha=0.3):
```
raw_loss = (expected_packets - received_packets) / expected_packets x 100
smoothed_loss = smoothed_loss x 0.7 + raw_loss x 0.3
```

### Spectrum throttling on packet loss

Server adjusts spectrum FPS per client based on reported loss:
- **0-5% loss:** Normal FPS
- **5-15% loss:** Half FPS (skip every other frame)
- **>15% loss:** Spectrum paused — audio has priority

---

## 14. Audio Pipeline

### Ring buffers

| Buffer | Capacity | Purpose |
|--------|---------|---------|
| Capture | 2s @ device rate | Mic -> network thread |
| Playback | 2s @ device rate | Network thread -> speaker |

Lock-free SPSC (Single Producer Single Consumer) ring buffers separate the audio callback thread from the tokio network thread.

### Capture processing (client TX)

1. cpal/Oboe callback captures audio from microphone
2. Multi-channel -> mono (first channel)
3. Push to capture ring buffer
4. Network loop (20ms tick): pop to accumulation buffer
5. Process complete frames: resample -> AGC (optional) -> TX gain -> Opus wideband 16kHz encode -> UDP send
6. Incomplete frames remain in accumulation buffer for next tick

**Important:** The accumulation buffer prevents sample loss. Earlier implementation with `pop_slice` on a large buffer lost the remainder after each frame, causing choppy audio.

### Server RX (TCI -> client)

1. TCI RX_AUDIO_STREAM arrival (48kHz PCM float32)
2. Resample 48kHz -> 8kHz
3. Opus narrowband encode
4. UDP send as Audio packet (0x01) to all connected clients

### Server TX (client -> TCI)

1. UDP Audio packet received -> push to jitter buffer
2. Playout timer (every 20ms): pull 1 frame from jitter buffer
3. Opus wideband 16kHz decode
4. Resample 16kHz -> 48kHz
5. Send via TCI TX_AUDIO_STREAM to Thetis

### Client RX playout

1. UDP Audio packets received -> push to jitter buffer
2. Playout timer (every 20ms): pull 1 frame from jitter buffer
3. Opus narrowband 8kHz decode
4. Resample 8kHz -> device rate
5. Push to playback ring buffer
6. Audio callback reads from playback ring buffer

### RX1/RX2 Audio

The server sends RX1 and RX2 as separate packet types (`Audio` 0x01 and `AudioRx2` 0x0E). The client decodes and mixes them:
- RX1 audio -> left or mono
- RX2 audio -> right or mono

### Yaesu Audio

Yaesu RX audio is sent as a separate packet type (`AudioYaesu` 0x16). The server captures from the Yaesu USB Audio CODEC and encodes with Opus narrowband 8kHz. TX audio for the Yaesu uses Opus wideband 16kHz.

### TX Automatic Gain Control (AGC)

Switchable client-side AGC in the TX audio path. Signal path:

```
Mic capture -> resample 16kHz -> [AGC] -> TX gain -> Opus wideband encode -> UDP
```

AGC is placed before TX gain so the gain slider acts as additional boost/attenuation on top of the normalized signal.

Peak-based envelope follower with noise gate:

| Parameter | Value | Description |
|-----------|-------|-------------|
| Target | -12 dB (0.25) | Desired peak level after AGC |
| Max gain | +20 dB (10x) | Maximum amplification |
| Min gain | -20 dB (0.1x) | Minimum amplification |
| Attack | 0.3 | Fast response to loud peaks |
| Release | 0.01 | Slow return to higher gain |
| Noise gate | -60 dB (0.001) | No gain boost during silence |

### Jitter Buffer Reset on Device Switch

When switching audio device (microphone or speaker), all jitter buffers are reset to prevent frame buildup. Without reset the buffer can accumulate up to 38+ frames (~760ms delay).

### RX Volume (ZZLA) Synchronization

The RX Volume (RX1 AF Gain) is bidirectionally synchronized between Thetis and all clients:

1. **Server -> Client:** Server polls `ZZLA;` via CAT -> broadcast as ControlPacket -> client update
2. **Client -> Server:** Slider change -> ControlPacket(Rx1AfGain) -> server sends `ZZLA{:03};`

**Sync protocol:** Client only sends ZZLA to server after the first value from the server has been received (`rx_volume_synced` flag). This prevents the client from pushing its local value to Thetis on connect.

---

## 15. External Devices

ThetisLink supports 7 external devices via the server. Status is read and forwarded to all connected clients. Each device can be individually enabled/disabled in the server settings.

### DeviceType enum

| Value | Device | Connection |
|-------|--------|------------|
| 0x01 | Amplitec 6/2 Antenna Switch | Serial USB-TTL, 9600 baud |
| 0x02 | JC-4s Antenna Tuner | Serial USB, 9600 baud |
| 0x03 | SPE Expert 1.3K-FA Linear Amplifier | Serial USB, 115200 baud |
| 0x04 | RF2K-S Linear Amplifier | TCP/IP |
| 0x05 | UltraBeam RCU-06 Antenna Controller | Serial USB, 19200 baud |
| 0x06 | EA7HG Visual Rotor | UDP, port 2570 |
| 0x07 | RemoteServer | Internal |

### Amplitec 6/2 Antenna Switch

6-port coax switch with two independent switches (A and B).

**ThetisLink protocol:**
- **Status:** switch_a = position A (1-6), switch_b = position B (1-6), labels = CSV with position names
- **Commands:** SetSwitchA (0x01, value=position 1-6), SetSwitchB (0x02, value=position 1-6)

### JC-4s Antenna Tuner

Automatic antenna tuner. Uses serial USB RTS/CTS signal lines (no data). CAT commands (ZZTU1/ZZTU0) are forwarded via a tokio channel to Thetis.

**Tuner states:**

| State | Value | Description | Color in UI |
|-------|-------|-------------|------------|
| Idle | 0 | No tune active | Grey |
| Tuning | 1 | Tune in progress | Blue |
| DoneOk | 2 | Tune successful | Green |
| Timeout | 3 | No response within 30s | Orange (3s, then Idle) |
| Aborted | 4 | User aborted | Orange (3s, then Idle) |

**PA protection:** During tuning the SPE Expert or RF2K-S PA is automatically put in Standby and restored to Operate after tuning.

**Stale detection:** DoneOk turns grey if the VFO frequency has shifted more than 25 kHz from the last successful tune.

### SPE Expert 1.3K-FA Linear Amplifier

Linear amplifier, 1300W.

**Hardware protocol:** Proprietary binary protocol with `0xAA 0xAA` preamble + command byte + data + XOR checksum.

| Command | Byte | Description |
|---------|------|-------------|
| Status query | 0x00 | Full status |
| Operate | 0x01 | Operate mode |
| Standby | 0x02 | Standby |
| Tune | 0x03 | Start tune cycle |
| Antenna | 0x04 | Switch antenna |
| Input | 0x05 | Switch input |
| Power level | 0x06 | L/M/H |
| Band up/down | 0x07/0x08 | Band switching |
| Power on/off | 0x09/0x0A | On/off |

**Status:** band, antenna, input, power level, forward/reflected power, SWR, temperature, voltage, current, alarm/warning flags, ATU bypass.

**UI:** Power bar with peak-hold and automatic scale (L=500W, M=1000W, H=1500W), telemetry, optional protocol log.

### RF2K-S Linear Amplifier

RF2K-S (RFKIT) solid-state linear amplifier.

**Connection:** TCP/IP (e.g. `192.168.1.50:8080`).

**Status:** band, frequency, temperature, voltage, current, forward/reflected power, SWR, error state, antenna type/number, tuner mode/setup, max power, device name.

**Commands:** rf2k_operate(bool), rf2k_tune, rf2k_ant1..4/ext, rf2k_error_reset, rf2k_close, rf2k_drive_up/down, rf2k_tuner (mode/bypass/reset/store/L/C/K control).

**UI:** Band and frequency display, forward/reflected power with SWR, temperature, voltage, current, antenna selection (4 + ext), built-in tuner control, error status with reset button.

### UltraBeam RCU-06 Antenna Controller

Controller for UltraBeam steerable Yagi antenna. Controls element lengths via stepper motors.

**Hardware protocol:** Proprietary binary protocol via RS-232: `0x55` (sync) + command + data + checksum.

| Command | Description |
|---------|-------------|
| Status query | Band, direction, motors, elements |
| Set frequency | kHz + direction (forward/reverse) |
| Retract | Retract all elements (transport position) |
| Read elements | Element positions in mm |

Frequency steps: 25 kHz and 100 kHz.

**UI:** Frequency/band display, direction indicator (forward/reverse), motor status (moving/complete %), element lengths per element, retract button.

### EA7HG Visual Rotor

Rotor controller for rotatable antennas. Arduino Mega 2560 with W5100 LAN module.

**Connection:** UDP, port 2570. Based on Prosistel protocol.

**Commands:**

| Command | Description |
|---------|-------------|
| `AA?` + CR | Position query. Response: `STX A,?,<angle>,<status> CR` |
| `AAG<nnn>` + CR | GoTo angle (nnn = 000-360 degrees) |
| `AAG999` + CR | Emergency stop |

**Important:** No STX (0x02) prefix when sending. The response from the Arduino does include STX.

**Status:** `R` = Ready (stationary), `B` = Busy (rotating).

**Polling:** Every 200ms. Offline after 3 seconds without response.

**ThetisLink protocol:**
- **Status labels:** `angle_x10,rotating,target_x10` (angle in tenths of degrees, 0-3600)
- **Commands:** CMD_ROTOR_GOTO (0x01, data=angle_x10 LE u16), CMD_ROTOR_STOP (0x02), CMD_ROTOR_CW (0x03), CMD_ROTOR_CCW (0x04)

**UI:** Clickable compass circle with green needle (current position), yellow line (target position), N/E/S/W labels with 30-degree tick marks, STOP button, GoTo text field.

---

## 16. Spectrum/Waterfall

### TCI IQ Mode

In TCI mode the server receives IQ data via the TCI WebSocket (`IQ_STREAM`). The server processes complex float32 I/Q pairs via the FFT pipeline.

```
TCI IQ_STREAM (receiver 0 or 1) -> Complex float32 I/Q pairs -> Accumulation
-> Blackman-Harris window -> Complex FFT forward (rustfft)
-> FFT-shift (DC to center)
-> |c|^2 normalization (/ N^2)
-> dB scale -> 0-255 mapping (-150 dB -> 0, -30 dB -> 255)
-> EMA smoothing (alpha=0.4)
-> Per-client extract_view(zoom, pan) -> max 32768 bins
-> Rate limit (fps per client)
-> SpectrumPacket -> clients
```

### TCI IQ Stream

Spectrum data comes via the TCI WebSocket IQ stream (384 kHz sample rate).

- **Source:** TCI `IQ_STREAM` command, receiver 0 (RX1) and receiver 1 (RX2)
- **Sample format:** Float32 I/Q pairs
- **Sample rate:** 384 kHz (configurable via `IQ_SAMPLERATE` TCI command)
- **Consumer:** Dedicated tokio task drains IQ channels to the spectrum processor

### FFT Configuration

Dynamic FFT size based on sample rate (`ddc_fft_size()` in `lib.rs`):

```rust
pub fn ddc_fft_size(sample_rate_hz: u32) -> usize {
    let target = (sample_rate_hz as usize) * 2 / 3;
    target.next_power_of_two().max(4096)
}
```

87.5% overlap (hop = FFT size / 8) for Thetis-quality frequency resolution.

**Configurable FFT size (client -> server):**

| FFT size | Hz/bin @384kHz | FFT/sec | Data window |
|----------|---------------|---------|-------------|
| 32768 | 11.7 | ~47 | 85ms |
| 65536 | 5.9 | ~23 | 171ms |
| 131072 | 2.9 | ~12 | 341ms |
| 262144 (auto) | 1.5 | ~6 | 683ms |

### Server-Side Zoom/Pan

Each client has its own zoom/pan state on the server. The smoothed buffer is calculated once per FFT frame; a view is extracted per client:

- **extract_view(zoom, pan, max_bins)**: selects visible bins, decimates to max 32768
- **Float stride**: `stride = visible as f64 / max_bins as f64` — covers full visible range
- **Max-per-group**: preserves signal peaks during decimation
- **center_freq_hz + span_hz**: per-view frequency metadata in Hz precision

### Frequency Tracking

- Server receives VFO frequency via TCI push (`vfo:0,0,freq;`)
- `center_freq_hz` in SpectrumPacket = Hz precision
- DDS center freq from TCI `dds:0,freq;` or HPSDR HP packet NCO phaseword
- Band change detection: buffer reset on jump > sample_rate/4

### VFO Marker Stability

- Client `pending_freq` tracking prevents VFO marker bounce during tuning
- `pending_freq` is only cleared when spectrum center is within 500 Hz of pending value
- During pending: VFO marker pinned to display center
- `tune_base_hz`: scroll-tuning accumulates on actual VFO (not display-VFO)

### Desktop Client Rendering

- **Spectrum plot** (150px): cyan spectrum line, max-per-pixel aggregation (bins -> pixels)
- **Waterfall** (150px): scrolling texture with ring buffer, contrast setting
- **VFO marker**: red vertical line, stable during tuning (pending_freq pinning)
- **Filter passband**: grey background + yellow border lines, signed Hz offsets
- **Band markers**: only visible when band is within display range
- **Band highlight**: memory buttons turn blue at active band
- **Dynamic freq axis**: tick spacing adapts to zoom level
- **Scroll-to-tune**: scroll wheel in spectrum or waterfall = +/- 1 kHz
- **Drag-to-tune**: click and drag in spectrum = VFO follows mouse (100 Hz snap)
- **Click-to-tune**: single click on spectrum -> VFO moves (1 kHz snap)
- **Zoom/Pan sliders**: server-side zoom (1x-1024x), pan with 100ms debounce
- **Ref level (-80..0 dB) / range (20..130 dB) sliders**: configurable display range
- **Waterfall contrast slider**: power curve adjustment
- **Colormap**: black -> blue -> cyan -> yellow -> red -> white (5-point linear)

### TX Spectrum Auto-Override

On PTT the server automatically overrides the spectrum display:
- **TX:** Ref level -30 dBm, range 100 dB (120 dB with PA active)
- **RX:** User settings restored

### Android Client

- **SpectrumPlot**: Compose Canvas, 120dp high
- **WaterfallView**: Bitmap ring buffer, 100dp high
- **FPS**: 5 fps default (saves 4G bandwidth)
- **Toggle**: Button in MainScreen
- **Click-to-tune**: tap on spectrum -> VFO moves

### Test Spectrum Generator

Without IQ data the server generates simulated spectrum:
- DDC test: signals around VFO (+/- 10 kHz, +/- 30 kHz, +/- 50 kHz, +/- 65 kHz) + noise floor
- Available for UI development without hardware

---

## 17. RX2/VFO-B Support

### Overview

ThetisLink provides full support for the second receiver (RX2) of the ANAN 7000DLE. This includes independent audio, spectrum/waterfall, and all controls.

### Audio

In TCI mode the server receives RX2 audio via `RX_AUDIO_STREAM` with receiver=1. The server encodes this separately and sends as `AudioRx2` (0x0E) to clients that have RX2 enabled.

### Spectrum

RX2 spectrum is processed in an independent `Rx2SpectrumProcessor`, identical to the RX1 pipeline but with its own state:
- IQ data from TCI `IQ_STREAM` receiver=1
- Own zoom/pan/fps per client
- Own smoothed buffer and FFT pipeline

### Per-client RX2 spectrum state

| Setting | Range | Default |
|---------|-------|---------|
| rx2_spectrum_enabled | bool | false |
| rx2_spectrum_fps | 5-30 | 10 |
| rx2_spectrum_zoom | 1.0-1024.0 | 1.0 |
| rx2_spectrum_pan | -0.5 to 0.5 | 0.0 |

### RX2 CAT Commands

| Command | Function | Range |
|---------|----------|-------|
| `ZZFB` | VFO-B frequency | 11 digits (Hz) |
| `ZZME` | RX2 mode | 00-11 |
| `ZZLB` | RX2 AF volume | 000-100 |
| `ZZFS` | RX2 filter low cut | Signed Hz |
| `ZZFR` | RX2 filter high cut | Signed Hz |
| `ZZNV` | RX2 NR level | 0-4 (0=off, 1-4=NR1-NR4) |
| `ZZNU` | RX2 ANF | 0/1 |
| `ZZRM2` | RX2 S-meter | 0-260 |

### Desktop Client: Joined/Split Popout Windows

Two display modes:

**Split mode:** Clicking "Pop-out" opens two separate windows (RX1 + RX2).

**Joined mode:** After clicking "Join" both windows are merged:

```
+──────────────────────────────────────────────────────────+
│  VFO-A controls          │  [Join]  VFO-B controls       │
│  14.345.000 Hz  S9+10    │  7.073.000 Hz  S7             │
│  [LSB] [USB] [AM] [FM]   │  [LSB] [USB] [AM] [FM]       │
+──────────────────────────────────────────────────────────+
│  ═══════════════════ RX1 Spectrum ═══════════════════    │
│  ═══════════════════ RX2 Spectrum ═══════════════════    │
+──────────────────────────────────────────────────────────+
```

### VFO Sync / Swap

- **VfoSync** (ControlId 0x14): VFO-B automatically follows VFO-A frequency
- **VfoSwap** (ControlId 0x19): swaps VFO A and B frequencies (maps to `ZZVS2`)

### Diversity

Full diversity support with controls for enable, reference source (RX1/RX2), source (RX1+RX2/RX1/RX2), gain per receiver (3 decimals), and phase adjustment. Interactive circle plot (desktop + Android) shows real-time phase/gain vector.

**Smart Auto-Null** (Thetis-side, ~9 seconds):
1. Equalize — diversity off, RX1/RX2 meters separate, gain set automatically
2. Coarse sweep — 450 degrees (360+90 overlap) AVG sweep in configurable steps
3. Fine phase — +/-15 degrees around coarse null in 1 degree steps
4. Gain optimization — +/-6dB around equalized gain in 0.5dB steps
5. Comparison — diversity off/on with result in dB

Parameters configurable via `diversity-smart.txt` (A-line):
`A coarseStep coarseSettle fineRange fineStep fineSettle gainRange gainStep gainSettle`

During the sweep Thetis broadcasts real-time `diversity_phase_ex` and `diversity_gain_ex` to all TCI clients, causing the circle plot to animate live.

---

## 18. Multi-channel Audio (v0.6.5)

Replaces the separate Audio (0x01), AudioRx2 (0x0E) and AudioBinR (0x1A) packets with a single **AudioMultiCh (0x1B)** packet that bundles per-channel mono Opus frames. Each channel is independently encoded and included in the same UDP packet.

### Channel IDs

| ID | Channel | Present when |
|----|---------|--------------|
| 0 | RX1 | Always |
| 1 | BinR | Binaural on (RX1 right channel) |
| 2 | RX2 | RX2 enabled |

### Advantages

- **Zero L/R desync:** Binaural left and right are in the same packet with identical sequence/timestamp. No jitter difference between channels.
- **Atomic delivery:** All channels from the same timestamp arrive together or are lost together.
- **Per-client filtering:** Server sends all channels; client takes only what it needs. Android receives only CH0 (RX1) by default to save bandwidth.
- **Backward compatible:** Old Audio/AudioRx2 packets are still accepted as fallback.

### Packet format

See section 5 (UDP Protocol), AudioMultiCh (0x1B).

---

## 19. Audio Routing: Mono/BIN/Split (v0.6.5)

Client-side audio routing via the **Audio** dropdown (desktop) or settings (Android). Determines how received channels are mixed to the speaker.

### Modes

| Mode | Behavior | Required channels |
|------|----------|------------------|
| **Mono** | RX1 mono to both ears. RX2 mono to both ears (mix). | CH0 (+CH2 if RX2 on) |
| **BIN** | RX1-L left ear, BinR right ear (binaural). RX2 mono to both ears. | CH0, CH1, (+CH2) |
| **Split** | RX1 left ear, RX2 right ear. | CH0, CH2 |

### BIN mode and TX

Binaural is automatically disabled during TX. This is a workaround for a Thetis side-effect: BIN on during TX causes audio artifacts. The server disables `rx_bin_enable` on PTT-on and restores it on PTT-off. This is transparent to the user.

### ControlId

`AudioMode` (0x62): value 0=Mono, 1=BIN, 2=Split. Client-side only, not sent to server.

---

## 20. FEC Recovery (v0.6.5)

Improved lost-frame reconstruction via Opus Forward Error Correction in the jitter buffer.

### Mechanism

1. Jitter buffer detects a missing frame (sequence gap)
2. **Peek-ahead:** buffer checks if the next frame is already available
3. If the next frame is available: `decode_fec()` on that frame with `fec=true` reconstructs the missing frame from the embedded FEC data
4. If not available: standard PLC (Packet Loss Concealment) via `decode(None)`

### Properties

- Transparent improvement: no extra bandwidth, FEC data is already in the Opus frame (set via `set_inband_fec(true)` and `set_expected_packet_loss(10)`)
- Effective for spread loss (1-2 frames lost, next intact)
- Not effective for burst loss (multiple consecutive frames lost)

---

## 21. Audio Recording and Playback (v0.6.6)

WAV recorder for RX audio on the server, with playback to speaker or TX.

### Recording

- Per audio source: **RX1**, **RX2**, **Yaesu** — each with its own checkbox in the Server tab
- Format: WAV, 8 kHz, 16-bit mono (PCM)
- Filename: automatically with timestamp, e.g. `rx1_2026-04-05_14-30-00.wav`
- Recording starts/stops via checkboxes in the server GUI
- Raw PCM after Opus decode, before resampling to device rate

### Playback

Two modes:

| Mode | Description | PTT |
|------|-------------|-----|
| **Speaker** | Plays WAV via the local speaker. No network, no PTT. | Not needed |
| **TX inject** | Injects WAV audio into the TX pipeline (replaces microphone). Only active during PTT. | Required |

With TX inject the WAV data is mixed frame-by-frame (20ms) into the TX audio stream, as if it came from the microphone. Stops automatically at end of file or on PTT-off.

---

## 22. Ultra Diversity Auto-Null (v0.6.6)

Alternative to Smart Auto-Null: a continuous sweep algorithm that is more accurate by compensating for AVG meter lag.

### Algorithm

1. **Forward sweep** — 0 to 450 degrees in 1 degree steps, continuous (no per-step settle), reads AVG S-meter at each step
2. **Backward sweep** — 450 back to 0 degrees in 1 degree steps, same approach
3. **True null** — average of forward and backward minimum-phase. This compensates the AVG meter lag: forward measures "too late" (after the true minimum), backward measures "too early", the average is the actual minimum.
4. **Gain optimization** — sweep gain around the equalized value at the found null phase
5. **Comparison** — diversity off vs. on, result in dB difference

### Difference from Smart Auto-Null

| | Smart Auto-Null | Ultra Auto-Null |
|-|-----------------|-----------------|
| Steps | Coarse (configurable) + fine (1 degree) | 1 degree over full range |
| Settle | Per-step settle time | No settle, continuous |
| Direction | Forward only | Forward + backward |
| Accuracy | Depends on settle tuning | Compensates AVG lag automatically |
| Location | Thetis-side | Server-side |

---

## 23. Spectrum Colors (v0.6.7)

Level-dependent spectrum colors matching the waterfall colormap.

### Color gradient

Spectrum line and waterfall use the same normalization and the same color gradient:

| Level | Color |
|-------|-------|
| Low (noise floor) | Blue |
| Below average | Cyan |
| Average | Yellow |
| High (signal) | Red |
| Very high (strong signal) | White |

### Normalization

Each bin is normalized with the same `ref_db` and `range_db` parameters as the waterfall:

```
t = clamp((bin_db - ref_db + range_db) / range_db, 0.0, 1.0)
```

A `color_floor` of 0.25 ensures the noise floor is not black but dark blue, for better visibility.

### Platform-consistent

Desktop (egui) and Android (Compose Canvas) use identical color calculation. The colormap is hardcoded (not configurable).

---

## 24. CAT-to-TCI Translation (v0.6.7)

Automatic translation of ZZ CAT commands to TCI when the auxiliary CAT connection is not available (typically when TCI extensions are enabled).

### Translation table

| CAT command | TCI command | Description |
|-------------|-------------|-------------|
| `ZZFA{freq}` | `vfo:0,0,{freq}` | VFO-A frequency |
| `ZZFB{freq}` | `vfo:0,1,{freq}` | VFO-B frequency |
| `ZZMD{mode}` | `modulation:0,0,{mode}` | RX1 mode |
| `ZZME{mode}` | `modulation:0,1,{mode}` | RX2 mode |
| `ZZTU{0/1}` | `tune:0,{true/false}` | Tune on/off |
| `ZZTX{0/1}` | `trx:0,{true/false}` | TX on/off |

### When active

- Only when there is no separate CAT TCP connection (no `--cat` argument or connection failed)
- With TCI extensions: all controls go via TCI, CAT is not needed
- Fallback: if a command cannot be translated, it is ignored with a debug log

---

## 25. Remote Shutdown (v0.6.7)

In addition to remote reboot (existing), remote shutdown is now also available. Both commands go via EquipmentCommand packets.

### Commands

| Command | ID | Execution | Delay |
|---------|-----|-----------|-------|
| Reboot | `CMD_SERVER_REBOOT` (0x01) | `shutdown /r /t 5 /f` | 5 seconds |
| Shutdown | `CMD_SERVER_SHUTDOWN` (0x02) | `shutdown /s /t 5 /f` | 5 seconds |

### UI

- **Desktop client:** buttons in the Server tab (confirmation dialog)
- **Android:** buttons in the Settings menu (confirmation dialog)

The server executes the command on the Windows host where the server is running. The `/f` flag forces all applications to close.

---

## 26. Yaesu FT-991A Integration

### Overview

The FT-991A is controlled via USB serial (CP210x) and USB Audio CODEC. The server communicates via CAT ASCII commands (38400 baud, 8N1, hardware flow control RTS/CTS) on the Enhanced COM Port.

### Audio paths

| Path | Direction | Codec | Sample rate | Bandwidth |
|------|-----------|-------|-------------|-----------|
| Yaesu RX | 991A USB -> server -> client | Opus narrowband | 8 kHz | ~13 kbps |
| Yaesu TX | Client mic -> server -> 991A USB | Opus wideband | 16 kHz | ~24 kbps |

**FM -> DATA-FM transparency:** On Yaesu PTT the server automatically switches from FM to DATA-FM (needed for USB mic) and restores FM after TX. The user only sees FM in all lists.

### Memory Channel Editor

Reads/writes memory channels via the **MT command** (Memory Tag):
- **Read:** `MT{nnn};` -> response with freq, mode, tone, shift + 12-char name
- **Write:** `MT{nnn}{freq}{clar}{mode}0{tone}{tonenum}{shift}0{TAG12};`
- **Recall:** `MC{nnn};` switches to memory mode + selects channel
- On recall via ThetisLink the radio stays in memory mode (scanning works)

Fields per channel: name, RX freq, TX freq (calculated from offset), mode, offset direction, offset freq, tone mode, CTCSS tone, AGC, NB, DNR, IPO, ATT, tuner, skip, step.

### EX Menu Editor (153 items)

All 153 setup menu items are readable and settable via the **EX command**:
- **Read:** `EX{nnn};` -> response `EX{nnn}{P2};`
- **Write:** `EX{nnn}{P2};` with P2 in fixed field length (1-8 digits)
- Enumeration items show a dropdown, numeric items a text field
- Changed values marked with * (deviation from default)
- Read-only items (RADIO ID) are shown in grey
- **Warning:** menu 031-033 (CAT RATE/TOT/RTS) changes may break the connection

### Radio Controls (Yaesu Popout Window)

| Control | CAT command | Notes |
|---------|-------------|-------|
| A/B (swap) | `SV;` | Swaps VFO A <-> B frequencies |
| V/M | `VM;` | Toggle VFO <-> Memory mode |
| Mode (8 buttons) | `MD0{x};` | LSB/USB/CW/CW-R/FM/AM/DIG-U/DIG-L |
| Band +/- | `BU0;`/`BD0;` | Band up/down (VFO-A) |
| Mem +/- | `MC{n+/-1};` | Step through memory channels 1-99 |
| A=B | `AB;` | Copy VFO A -> B |
| Split | `ST1;`/`ST0;` | Split mode toggle |
| Scan | `SC1;`/`SC0;` | Memory/VFO scan toggle |
| Tune | `AC002;`/`AC000;` | Tuner on/off |
| SQL | `SQ0{nnn};` | Squelch slider (0-255) |
| PWR | `PC{nnn};` | RF power slider (0-100) |
| MIC | `MG{nnn};` | Mic gain slider (0-100) |
| RF Gain | `RG0{nnn};` | RF gain slider (0-255) |

### VFO/Memory Status

Server polls `IF;` every 500ms. P7 field gives the mode:
- **VFO** (green) — normal
- **VFO Split** (orange) — split mode active
- **MEM nn name freq** (blue) — memory mode with channel name

### Auto-reconnect

On power loss of the 991A:
1. Serial thread detects 5s no response -> status disconnected
2. Client shows "Power OFF"
3. Every 3s the server attempts to reconnect
4. On success: serial + audio capture + output are rebuilt
5. Persistent audio channels survive reconnects (network loops do not need to restart)

---

## 27. Network Authentication (HMAC-SHA256 + TOTP 2FA)

### Challenge-Response

ThetisLink uses mandatory password authentication via a pre-shared key (PSK) with optional TOTP two-factor authentication:

```mermaid
sequenceDiagram
    participant C as Client
    participant S as Server
    C->>S: Heartbeat (new address)
    S->>C: AuthChallenge(nonce) [16 byte random]
    C->>S: AuthResponse(HMAC) [HMAC-SHA256(password, nonce)]
    S->>C: AuthResult(1) [accepted]
    C->>S: Heartbeat (normal continue)
```

With TOTP 2FA enabled:

```mermaid
sequenceDiagram
    participant C as Client
    participant S as Server
    C->>S: Heartbeat (new address)
    S->>C: AuthChallenge(nonce) [16 byte random]
    C->>S: AuthResponse(HMAC) [HMAC-SHA256(password, nonce)]
    S->>C: AuthResult(2) [totp_required]
    S->>C: TotpChallenge
    Note over C: Client shows TOTP input field<br/>Heartbeats paused
    C->>S: TotpResponse(code) [6-digit code]
    S->>C: AuthResult(1) [accepted]
    C->>S: Heartbeat (normal continue)
```

The client stops heartbeats while waiting for TOTP input to prevent the server from resetting the auth session.

### Properties

- **Password required** - server does not start without a valid password (min. 8 characters, letters + digits)
- **TOTP 2FA optional** - TOTP-SHA1, 6 digits, 30 second period, +/-1 period clock skew tolerance
- **No overhead** on data packets (audio/spectrum) - only IP:port check after auth
- **Brute-force protection** - 5 attempts per client (IP:port), then 60s block
- **Password** never goes over the network (only HMAC of random nonce)
- **Obfuscated storage** in config files (XOR with fixed key + hex encoding)
- **Constant-time comparison** to prevent timing side-channel attacks

### Implementation Details

- Nonce: 16 bytes (`NONCE_SIZE`), generated with `rand::thread_rng()`
- HMAC output: 32 bytes (`HMAC_SIZE`), SHA-256
- TOTP: SHA-1, 6 digits, 30s period (RFC 6238 compatible)
- TOTP secret: 20 bytes random, stored as base32
- TOTP URI: `otpauth://totp/ThetisLink?secret=BASE32&issuer=ThetisLink`
- QR code in server GUI for easy scanning with authenticator app
- Obfuscation key: `ThetisLink-PSK-2026` (XOR per byte, not cryptographic)
- AuthResult codes: 0=rejected, 1=accepted, 2=totp_required

### Configuration

**Server** (`thetislink-server.conf`, automatically saved via GUI):
```
password=<obfuscated>
totp_enabled=true
totp_secret=<obfuscated>
```

**Client** (`thetislink-client.conf` or password field):
```
password=<obfuscated>
```

**Android:** Settings dialog -> Server password. TOTP code is requested on connection.

Passwords and TOTP secrets are automatically obfuscated on saving.

---

## 28. Known Limitations

1. **AM/FM audio:** Currently no audio in AM/FM mode on Thetis (SSB/CW works correctly).

2. **HPSDR Protocol 1:** Only Protocol 2 (ANAN 7000DLE, 8000DLE, etc.) is supported. Protocol 1 (Hermes, Angelia, Orion) is not implemented.

3. **Single TX:** Only one client can transmit at a time. Other clients receive PttDenied.

6. **Yaesu EX menu items 031-033:** Changing CAT RATE/TOT/RTS via ThetisLink may break the serial connection to the radio.

7. **macOS:** Experimental support. cpal works with CoreAudio but some USB audio devices are not correctly detected.

8. **Android spectrum:** No pinch-to-zoom or drag pan on the Android spectrum/waterfall display.
