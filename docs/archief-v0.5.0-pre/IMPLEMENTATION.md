# ThetisLink Implementation

Detailed description of the implementation per module, including data flows and algorithms.

## sdr-remote-core

### protocol.rs — Packet Definitions (~900 LOC)

All packet types as Rust structs with `serialize()` / `deserialize()` methods.

**Wire format example — AudioPacket:**
```
Offset  Bytes  Field
0       1      magic (0xAA)
1       1      version (0x01)
2       1      packet_type (0x01)
3       1      flags (bit 0 = PTT)
4       4      sequence (u32 LE)
8       4      timestamp (u32 LE, ms)
12      2      opus_len (u16 LE)
14      N      opus_data
```

**ControlId enum:** 25+ control commands, each with u8 ID and u16 value. Used for bidirectional state sync between server and client.

**EquipmentStatus/Command:** Variable length, CSV-encoded telemetry in a `labels` string field. Each equipment type has its own CSV layout.

### codec.rs — Opus Audio Codec (~200 LOC)

| Parameter | Narrowband | Wideband |
|-----------|-----------|----------|
| Sample rate | 8 kHz | 16 kHz |
| Bitrate | 12.8 kbps | 24 kbps |
| Frame size | 160 samples (20ms) | 320 samples (20ms) |
| Bandwidth | Narrowband | Wideband |
| FEC | Inband, 10% loss | Inband, 10% loss |
| DTX | On | On |
| Signal type | Voice | Voice |

**Important:** Bitrate 12.8 kbps is just above the Opus FEC threshold (12.4 kbps). This guarantees that Forward Error Correction is always included.

### jitter.rs — Adaptive Jitter Buffer (~350 LOC)

```mermaid
graph TD
    A[UDP Packet Received] --> B{Sequence < next_seq?}
    B -->|Yes: too late| C[Drop packet]
    B -->|No| D[Calculate jitter<br/>RFC 3550 dual-alpha]
    D --> E[Add to BTreeMap]
    E --> F{Buffer > target+6?}
    F -->|Yes| G[Overflow recovery:<br/>drop 1-2 frames]
    F -->|No| H[Done]

    I[Pull 20ms tick] --> J{Frame available<br/>for next_seq?}
    J -->|Yes| K[Return Frame<br/>next_seq++]
    J -->|No| L{Next seq<br/>in buffer?}
    L -->|Yes| M[Return Missing<br/>use FEC from next]
    L -->|No| N{Buffer empty?}
    N -->|Yes| O[Return NotReady<br/>use PLC]
    N -->|No| P{Far behind?<br/>gap > max_depth}
    P -->|Yes| Q[Skip-ahead to<br/>oldest frame]
    P -->|No| O
```

**Jitter estimation (RFC 3550 variant):**
```
deviation = |expected_interval - actual_interval|

if deviation > current_estimate:
    jitter = jitter + 0.25 * (deviation - jitter)     # fast attack
else:
    jitter = jitter + 0.0625 * (deviation - jitter)   # slow decay
```

**Spike peak hold:** Peak value with exponential decay (~1 minute). Prevents the buffer from shrinking too quickly after a network spike.

**Target depth formula:**
```
target = max(jitter_estimate, spike_peak) / 15.0 + 2
clamped: 2..40 frames (40ms..800ms)
```

**Grace period:** First 25 pulls (500ms) after connection: no overflow recovery. Allows the buffer to stabilize.

## sdr-remote-logic

### commands.rs — Command Enum (~90 variants)

Commands are sent from UI to engine via `mpsc::UnboundedSender<Command>`. Groups:

| Group | Examples | Count |
|-------|----------|-------|
| Connection | Connect, Disconnect | 2 |
| Audio | SetRxVolume, SetLocalVolume, SetVfoAVolume, SetTxGain | 7 |
| Radio | SetPtt, SetFrequency, SetMode, SetControl | 6 |
| Spectrum | EnableSpectrum, SetSpectrumFps/Zoom/Pan | 8 |
| RX2 | SetRx2Enabled, SetFrequencyRx2, SetModeRx2 | 12 |
| Amplitec | SetAmplitecSwitchA/B | 2 |
| Tuner | TunerTune, TunerAbort | 2 |
| SPE Expert | SpeOperate, SpeTune, SpeAntenna, ... | 11 |
| RF2K-S | Rf2kOperate, Rf2kTune, Rf2kAnt1-4, ... | 23 |
| UltraBeam | UbRetract, UbSetFrequency, UbReadElements | 3 |
| Rotor | RotorGoTo, RotorStop, RotorCw, RotorCcw | 4 |

### state.rs — RadioState (~170+ fields)

Broadcast from engine to UI via `watch::Sender<RadioState>`. UI receives via `watch::Receiver` with change notification.

**Main groups:**
- Connection: connected, rtt_ms, jitter_ms, buffer_depth, loss_percent
- Audio: capture_level, playback_level, playback_level_rx2
- Radio: frequency_hz, mode, smeter, power_on, filter_low/high_hz
- RX2: rx2_enabled, frequency_rx2_hz, mode_rx2, smeter_rx2
- Spectrum: spectrum_bins[], center_hz, span_hz, ref_level (RX1 + RX2)
- Equipment: ~100 fields for 6 equipment types

### engine.rs — ClientEngine (~2,181 LOC)

The engine is the heart of every client. Runs as an async tokio task.

```mermaid
graph TB
    subgraph "Engine Main Loop (tokio::select!)"
        CMD[Command Channel<br/>mpsc::UnboundedReceiver]
        UDP[UDP Socket<br/>recv_from]
        TICK[Audio Tick<br/>20ms interval]
        HB[Heartbeat<br/>500ms interval]
    end

    subgraph "Command Processing"
        CMD --> CONN[Connect/Disconnect]
        CMD --> PTT[Set PTT]
        CMD --> VOL[Volume settings]
        CMD --> FREQ[Frequency/Mode]
        CMD --> SPEC[Spectrum settings]
        CMD --> EQUIP[Equipment commands]
    end

    subgraph "UDP Reception"
        UDP --> AUDIO_RX[Audio -> Jitter Buffer]
        UDP --> HBACK[HeartbeatAck -> RTT update]
        UDP --> STATE_UPD[Freq/Mode/Smeter -> State]
        UDP --> SPEC_RX[Spectrum -> State bins]
        UDP --> CTRL_RX[Control -> State sync]
        UDP --> EQUIP_RX[EquipmentStatus -> State]
    end

    subgraph "Audio Tick (20ms)"
        TICK --> PLAY[RX Playout]
        TICK --> CAP[TX Capture]
        PLAY --> JB1[RX1 Jitter Buffer Pull]
        PLAY --> JB2[RX2 Jitter Buffer Pull]
        JB1 --> DEC1[Opus Decode / FEC / PLC]
        JB2 --> DEC2[Opus Decode RX2]
        DEC1 --> RES1[Resample 8->48kHz]
        DEC2 --> RES2[Resample 8->48kHz]
        RES1 --> MIX[Mix RX1 + RX2]
        RES2 --> MIX
        MIX --> RING_OUT[Playback Ring Buffer]

        CAP --> RING_IN[Capture Ring Buffer]
        RING_IN --> RES_TX[Resample 48->8kHz]
        RES_TX --> AGC[TX AGC]
        AGC --> ENC[Opus Encode]
        ENC --> SEND[UDP Send]
    end

    subgraph "State Broadcast"
        STATE[RadioState via watch::send]
    end

    AUDIO_RX --> STATE
    STATE_UPD --> STATE
    PLAY --> STATE
    CAP --> STATE
```

#### Audio Playout (RX) — Detail

```mermaid
graph TD
    A[20ms Tick] --> B[Check Ring Buffer level]
    B --> C{Buffer < 60ms?}
    C -->|Yes| D[Pull 2 frames<br/>from jitter buffer]
    C -->|No| E{Buffer > 200ms?}
    E -->|Yes| F[Skip pull<br/>let buffer drain]
    E -->|No| G[Pull 1 frame]

    D --> H{Frame available?}
    G --> H
    H -->|Frame| I[Opus Decode<br/>160 samples i16]
    H -->|Missing| J[FEC Decode<br/>or PLC]
    H -->|NotReady| K[PLC<br/>comfort noise]

    I --> L[Resample 8->48kHz<br/>rubato SincFixedIn]
    J --> L
    K --> L

    L --> M[Apply Volume<br/>rx * vfoA * master]
    M --> N{RX2 enabled?}
    N -->|Yes| O[Pull RX2 frames<br/>match RX1 count]
    O --> P[Decode + Resample RX2]
    P --> Q[Apply Volume<br/>rx2 * vfoB * master]
    Q --> R[Mix: RX1 + RX2<br/>additive]
    N -->|No| R
    R --> S[Write to<br/>Playback Ring Buffer]
```

#### Audio Capture (TX) — Detail

```mermaid
graph TD
    A[20ms Tick] --> B[Pull samples from<br/>Capture Ring Buffer]
    B --> C[Resample 48->8kHz<br/>rubato SincFixedIn]
    C --> D[Apply TX Gain<br/>default 0.5]
    D --> E{AGC enabled?}
    E -->|Yes| F[TX AGC:<br/>target -12dB<br/>range +/-20dB<br/>attack 0.3, release 0.01<br/>noise gate -60dB]
    E -->|No| G[Accumulate in buffer]
    F --> G
    G --> H{160 samples?}
    H -->|No| I[Wait for more]
    H -->|Yes| J[Opus Encode<br/>160 smp -> ~60 bytes]
    J --> K[Build AudioPacket<br/>seq++, timestamp]
    K --> L{PTT transition?}
    L -->|Yes| M[Burst: 5 packets<br/>rapid succession]
    L -->|No| N[Normal send]
    M --> O[UDP Send]
    N --> O
```

#### Frequency Synchronization

```mermaid
sequenceDiagram
    participant UI as Client UI
    participant E as Engine
    participant S as Server
    participant T as Thetis

    UI->>E: SetFrequency(7.035 MHz)
    E->>E: pending_freq = 7.035 MHz
    E->>S: FrequencyPacket(7.035 MHz)
    S->>T: ZZFA00007035000;

    Note over S: CAT poll 200ms later
    S->>T: ZZFA;
    T->>S: ZZFA00007035000;
    S->>E: FrequencyPacket(7.035 MHz)

    E->>E: received == pending?
    Note over E: Yes -> pending_freq = None
    E->>E: state.frequency_hz = 7.035 MHz

    Note over T: User turns VFO knob in Thetis
    T->>S: (CAT poll) ZZFA00007036000;
    S->>E: FrequencyPacket(7.036 MHz)
    E->>E: pending == None -> accept
    E->>E: state.frequency_hz = 7.036 MHz
```

#### Volume Synchronization

```mermaid
graph TD
    A[Client start] --> B[rx_volume_synced = false]
    B --> C{Server sends<br/>Rx1AfGain control?}
    C -->|Yes| D[rx_volume_synced = true<br/>rx_volume = server value]
    C -->|No| E[Wait...]

    D --> F{User adjusts<br/>RX volume?}
    F -->|Yes| G[Send SetRxVolume<br/>to engine]
    G --> H[Engine sends<br/>Control to server]
    H --> I[Server sends<br/>ZZLA to Thetis]
```

## sdr-remote-server

### Main Structure

```mermaid
graph TB
    subgraph "main.rs — Startup"
        CONF[Config load/GUI]
        AUDIO[AudioPipeline init<br/>cpal devices]
        CAT_INIT[CAT connection<br/>TCP to Thetis]
        HPSDR[HPSDR Capture<br/>DDC I/Q listener]
        EQUIP[Equipment Controllers<br/>COM/TCP/HTTP]
        NET[Start NetworkService]
    end

    CONF --> AUDIO
    CONF --> CAT_INIT
    CONF --> HPSDR
    CONF --> EQUIP
    AUDIO --> NET
    CAT_INIT --> NET
    HPSDR --> NET
    EQUIP --> NET
```

### network.rs — NetworkService (~1,363 LOC)

Manages all UDP communication with clients.

```mermaid
graph TB
    subgraph "Async Tasks"
        HB[Heartbeat Responder<br/>500ms interval]
        TX[Audio TX Task<br/>Capture -> Encode -> Send]
        RX[Audio RX Task<br/>Recv -> Decode -> Playback]
        SPEC[Spectrum Task<br/>FFT -> View Extract -> Send]
        SPEC2[RX2 Spectrum Task]
        POLL[CAT Poll Task<br/>200ms interval]
        CTRL[Control Broadcast<br/>State sync to clients]
    end

    subgraph "Shared State"
        SESSION[SessionManager<br/>HashMap addr -> ClientSession]
        PTT_CTRL[PttController<br/>Single-TX arbitration]
        CAT_STATE[CAT State<br/>Freq, Mode, Meters]
    end

    HB --> SESSION
    TX --> SESSION
    RX --> SESSION
    RX --> PTT_CTRL
    SPEC --> SESSION
    POLL --> CAT_STATE
    CTRL --> SESSION
```

### cat.rs — CAT Interface (~834 LOC)

**Polling cycle:**

```mermaid
graph LR
    subgraph "Every 200ms"
        A[ZZFA -> freq RX1]
        B[ZZFB -> freq RX2]
        C[ZZMD -> mode RX1]
        D[ZZME -> mode RX2]
        E[ZZLA -> RX1 AF gain]
        F[ZZLE -> RX2 AF gain]
        G[ZZPC -> TX drive]
    end

    subgraph "Every 100ms"
        H{TX active?}
        H -->|Yes| I[ZZRM5 -> fwd power]
        H -->|No| J[ZZSM0 -> RX1 S-meter<br/>ZZSM1 -> RX2 S-meter]
    end
```

**S-meter processing:**
1. ZZSM provides raw value (0-260)
2. Conversion: `dBm = raw / 2 - 140`
3. Storage as linear milliwatts: `mW = 10^(dBm/10)`
4. RMS averaging over sliding window (4 samples, ~0.4 sec)
5. Back to display: `avg_mw -> dBm -> raw (0-260)`

### spectrum.rs — SpectrumProcessor (~994 LOC)

**DDC FFT Pipeline:**

```mermaid
graph TD
    A[ANAN 7000DLE<br/>DDC I/Q samples] --> B[HPSDR Capture<br/>UDP :1037]
    B --> C[Accumulate complex samples]
    C --> D{fft_size/2<br/>new samples?}
    D -->|No| C
    D -->|Yes| E[50% overlap:<br/>combine with previous half]
    E --> F[Apply Hann Window]
    F --> G[Complex FFT<br/>e.g. 262144-point]
    G --> H[Calculate magnitude<br/>20 * log10]
    H --> I[EMA Smoothing<br/>alpha = 0.1]
    I --> J[8192 bins output]

    J --> K{Per client}
    K --> L[View extraction:<br/>apply zoom & pan]
    L --> M[Downsample to<br/>requested bin count]
    M --> N[Send SpectrumPacket]
```

**FFT size selection:**
```
target = sample_rate / 6
fft_size = next_power_of_two(target)
minimum = 4096

Examples:
  1536 kHz -> 262144 (~12 FPS)
   384 kHz ->  65536 (~12 FPS)
    96 kHz ->  16384 (~12 FPS)
    48 kHz ->   8192 (~12 FPS)
```

### ptt.rs — PTT Controller (~559 LOC)

**Single-TX Arbitration:**

```mermaid
stateDiagram-v2
    [*] --> Idle: Start
    Idle --> TxActive: Client A PTT=1<br/>try_acquire_tx(A) -> true
    TxActive --> Idle: Client A PTT=0<br/>release_tx(A)
    TxActive --> TxActive: Client B PTT=1<br/>try_acquire_tx(B) -> false<br/>-> PttDenied

    state TxActive {
        [*] --> HoldingTx
        HoldingTx: tx_holder = Client A
        HoldingTx: CAT: ZZTX1;
    }

    state Idle {
        [*] --> NoTx
        NoTx: tx_holder = None
        NoTx: CAT: ZZTX0;
    }
```

### Equipment Handlers

All equipment handlers follow the same pattern:

```mermaid
graph TD
    A[Init: open COM/TCP/HTTP] --> B[Polling Loop]
    B --> C[Read status]
    C --> D[CSV encode in<br/>EquipmentStatus labels]
    D --> E[Broadcast to clients]
    E --> B

    F[EquipmentCommand<br/>from client] --> G[Parse command type]
    G --> H[Translate to<br/>device protocol]
    H --> I[Send to device]
```

| Handler | Interface | Poll Interval | Telemetry Fields |
|---------|-----------|---------------|------------------|
| amplitec.rs (220 LOC) | COM 9600 | 1s | switch_a, switch_b, labels |
| tuner.rs (503 LOC) | COM 9600 | 500ms | state, can_tune |
| spe_expert.rs (568 LOC) | COM 9600 | 500ms | 12 fields (power, SWR, temp, ...) |
| rf2k.rs (1082 LOC) | HTTP :8080 | 500ms | 28+ fields incl. debug |
| ultrabeam.rs (461 LOC) | COM 9600 | 1s | freq, band, direction, elements |
| rotor.rs (245 LOC) | TCP :3010 | 500ms | angle, rotating, target |

## sdr-remote-client

### main.rs — Startup

```mermaid
graph TD
    A[Start] --> B[Init tokio runtime]
    B --> C[Create ClientAudio<br/>cpal devices]
    C --> D[Create ClientEngine<br/>from sdr-remote-logic]
    D --> E[Spawn engine<br/>in background]
    E --> F[Start eframe/egui<br/>rendering loop]
    F --> G[UI update() per frame]
```

### audio.rs — ClientAudio

```mermaid
graph LR
    subgraph "Input (Capture)"
        MIC[Microphone] --> CPAL_IN[cpal Input Stream]
        CPAL_IN --> RING_IN[Ring Buffer<br/>lock-free SPSC]
    end

    subgraph "Output (Playback)"
        RING_OUT[Ring Buffer<br/>lock-free SPSC] --> CPAL_OUT[cpal Output Stream]
        CPAL_OUT --> SPK[Speakers]
    end

    RING_IN -.->|Engine reads| ENGINE[Engine]
    ENGINE -.->|Engine writes| RING_OUT
```

### ui.rs — Desktop UI (~5,668 LOC)

See separate document: [UI.md](UI.md)

## sdr-remote-android

### Architecture

```mermaid
graph TB
    subgraph "Kotlin / Jetpack Compose"
        Activity[MainActivity]
        VM[SdrViewModel]
        MS[MainScreen]
        RC[RadioControls]
        FD[FrequencyDisplay]
        SV[SpectrumView]
        SP[StatsPanel / VolumeControls]
    end

    subgraph "Rust via JNI"
        Bridge[bridge.rs<br/>FFI interface]
        Engine[ClientEngine<br/>sdr-remote-logic]
        Core[sdr-remote-core<br/>Protocol + Codec]
    end

    Activity --> VM
    VM --> Bridge
    MS --> RC
    MS --> FD
    MS --> SV
    MS --> SP

    Bridge --> Engine
    Engine --> Core

    VM -.->|state polling| Bridge
    VM -.->|commands| Bridge
```

**Bridge functions (Rust -> Kotlin):**
- `version()` -> String
- `state()` -> BridgeRadioState (130+ fields)
- `connect(addr)`, `disconnect()`
- `set_ptt(bool)`, `set_frequency(hz)`, `set_mode(u8)`
- `set_rx_volume(f32)`, `set_local_volume(f32)`, `set_tx_gain(f32)`
- `set_control(id, value)`
- `enable_spectrum(bool)`, `set_spectrum_fps/zoom/pan()`

**Audio:** Oboe (Android Native Audio), 48kHz mono f32

## Network Timing & Reliability

### Timeline of an audio frame

```
t=0ms    Client capture ring buffer -> samples available
t=1ms    Resample 48->8kHz, Opus encode
t=2ms    UDP send
t=Xms    Network transit (RTT/2)
t=X+1ms  Server receive
t=X+2ms  Opus decode, resample 8->48kHz
t=X+3ms  Playback ring buffer -> to Thetis
```

Total one-way latency: ~3ms processing + network transit + jitter buffer (40-800ms adaptive)

### Heartbeat & Connection Detection

```
Interval:     500ms
Timeout:      max(6000ms, RTT * 8)
RTT measurement: Echo timestamp in HeartbeatAck
Loss%:        Rolling window per heartbeat interval
Reconnect:    Reset codec + jitter buffer on first HeartbeatAck
```

### Packet Loss Recovery

| Scenario | Recovery Method |
|----------|----------------|
| 1 packet lost | FEC from next packet |
| 2+ packets lost | PLC (Packet Loss Concealment) |
| Burst loss | Jitter buffer absorbs up to target depth |
| Network spike | Spike peak hold prevents buffer from shrinking too quickly |
| Connection lost | Timeout after 6s, reconnect on new HeartbeatAck |

## v0.4.1 Changes

### ptt.rs — Two-Phase Connect

The Thetis CAT connection has been rewritten to a two-phase connect pattern:

- **New:** `needed_connections()` — returns which connections need to be established (CAT and/or TCI)
- **New:** `accept_connections()` — accepts already-connected TCP streams from the caller
- **Removed:** `try_connect_cat()`, `ptt_flag()` — no longer needed with the two-phase pattern
- `set_power()` unchanged, but now called correctly after the ZZBY command

### cat.rs — Two-Phase Connect

The CAT interface now uses the same two-phase connect pattern:

- **New:** `needs_connect()` — indicates whether a (re)connection is needed
- **New:** `accept_stream()` — accepts an already-connected TcpStream
- **Removed:** `try_connect()` (was dead code)
- `send()` no longer triggers connect attempts; silently returns if not connected
- **Rate limit:** 1s interval between reconnect attempts

### tci.rs — Two-Phase Connect

The TCI WebSocket interface follows the same pattern:

- **New:** `needs_connect_info()` — indicates whether a (re)connection is needed
- **New:** `accept_stream()` — accepts an already-connected WebSocket stream
- **Removed:** `try_connect()` (was dead code)
- `send()` no longer triggers connect; silently returns if not connected
- **Rate limit:** 1s reconnect interval (was 2s)

### network.rs — Background Connect Tasks

Connection logic has been moved to background tokio tasks:

- `cat_tick` spawns a background tokio task for two-phase connect
- Three TCI consumer tasks: `drop(ptt_guard)` before `sleep` to avoid lock contention
- `freq_tick`: 100ms interval (was 500ms) for faster frequency updates
- **Connect timeouts:** 100ms TCP, 500ms WebSocket — prevents blocking the main loop

### engine.rs — PowerOnOff & State Sync (sdr-remote-logic)

Power on/off logic improved:

- **PowerOnOff local state:** `value == 1` (was `value != 0`) for correct toggle behavior
- **state_tx.send()** immediately after PowerOnOff for instant UI update
- **power_suppress_until:** 5-second suppression of server power broadcasts after local toggle, prevents the server state from reverting the local change
