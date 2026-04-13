# ThetisLink Architecture

## System Overview

```mermaid
graph TB
    subgraph "Thetis PC (Windows)"
        THETIS[Thetis SDR v2.10.3]
        TCI_WS[TCI WebSocket :40001]
        CAT_TCP[TCP CAT :13013]
        THETIS --> TCI_WS
        THETIS --> CAT_TCP
    end

    subgraph "ThetisLink Server"
        TCI[TCI Connection]
        AUX_CAT[Aux CAT Connection]
        SPEC[Spectrum Processor<br/>FFT + smoothing]
        RX2_SPEC[RX2 Spectrum Processor]
        NET[Network Service<br/>UDP :4580]
        PTT[PTT Controller]
        YAESU[Yaesu FT-991A<br/>Serial + USB Audio]
        EQUIP[Equipment<br/>RF2K-S / SPE / JC-4s<br/>UltraBeam / Rotor]
        DXC[DX Cluster<br/>Telnet]

        TCI_WS -.-> TCI
        CAT_TCP -.-> AUX_CAT
        TCI --> PTT
        AUX_CAT --> PTT
        TCI -->|IQ data| SPEC
        TCI -->|IQ data| RX2_SPEC
        TCI -->|RX audio L| NET
        TCI -->|RX audio R binaural| NET
        PTT --> NET
        SPEC --> NET
        RX2_SPEC --> NET
        YAESU --> NET
        EQUIP --> NET
        DXC --> NET
    end

    subgraph "Desktop Client"
        ENGINE[Engine<br/>network + audio loop]
        AUDIO_OUT[Audio Output<br/>cpal stereo]
        AUDIO_IN[Audio Input<br/>cpal capture]
        UI[egui UI<br/>spectrum + waterfall<br/>controls + meters]
        MIDI_C[MIDI Controller]
        EQ[5-Band EQ]

        NET <-->|UDP packets| ENGINE
        ENGINE --> AUDIO_OUT
        AUDIO_IN --> ENGINE
        ENGINE <--> UI
        MIDI_C --> UI
        EQ --> ENGINE
    end

    subgraph "Android Client"
        ENGINE_A[Engine<br/>UniFFI bridge]
        AUDIO_A[Oboe Audio]
        UI_A[Compose UI]
        BT[BT Remote ZL-01]

        NET <-->|UDP packets| ENGINE_A
        ENGINE_A --> AUDIO_A
        ENGINE_A <--> UI_A
        BT --> UI_A
    end
```

## Audio Data Flow

### RX Audio (Thetis to Client)
```mermaid
graph LR
    T_WDSP[WDSP DSP] -->|48kHz float32| T_TCI[TCI Binary Frame]
    T_TCI -->|WebSocket| S_DECODE[Server Decode]
    S_DECODE -->|48k to 8k| S_OPUS[Opus Encode<br/>12.8 kbps]
    S_OPUS -->|UDP AudioPacket| C_JITTER[Jitter Buffer]
    C_JITTER -->|Opus Decode| C_RESAMPLE[8k to 48k]
    C_RESAMPLE -->|Volume + Mix| C_RING[Ring Buffer]
    C_RING -->|cpal callback| SPEAKER[Speaker]
```

### Binaural Stereo (BIN mode)
```mermaid
graph LR
    T_BIN[Thetis 2-ch] -->|L + R| S_SPLIT[Server Split]
    S_SPLIT -->|L| S_L[Opus L]
    S_SPLIT -->|R| S_R[Opus R]
    S_L -->|AudioPacket| C_L[Decode L]
    S_R -->|AudioBinR| C_R[Decode R lockstep]
    C_L --> STEREO[L to Left ear<br/>R to Right ear]
    C_R --> STEREO
```

### TX Audio (Client to Thetis)
```mermaid
graph RL
    MIC[Mic] -->|48kHz| AGC[TX AGC]
    AGC -->|48k to 8k| OPUS[Opus Encode]
    OPUS -->|UDP| SERVER[Server]
    SERVER -->|Decode + TCI| THETIS[Thetis TX]
```

### Yaesu TX Audio
```mermaid
graph RL
    MIC[Mic] -->|48kHz| EQ[5-Band EQ]
    EQ -->|x mic gain| OPUS[Wideband Opus<br/>16kHz 24kbps]
    OPUS -->|UDP AudioYaesu| SERVER[Server]
    SERVER -->|Decode x 20| USB[Yaesu USB Audio]
```

## Spectrum Pipeline
```mermaid
graph TB
    IQ[TCI IQ 384kHz] --> FFT[FFT 65K-262K pt]
    FFT --> SMOOTH[EMA + Peak Hold]
    SMOOTH --> CAL[Calibration Offset]
    CAL --> VIEW[Extracted View<br/>zoom and pan]
    CAL --> FULL[Full DDC Row]
    VIEW -->|SpectrumPacket| CLIENT[Client]
    FULL -->|FullSpectrumPacket| CLIENT
    CLIENT --> PLOT[Spectrum Line]
    CLIENT --> WF[Waterfall<br/>hybrid render]
```

## State Flow
```mermaid
graph LR
    TCI[TCI Notification] --> TCIS[TciConnection<br/>80 fields]
    CAT[CAT Poll] --> CATS[CatConnection<br/>30 fields]
    TCIS --> PTT[PttController]
    CATS --> PTT
    PTT -->|UDP broadcast<br/>change-detect| ENGINE[RadioState<br/>260 fields]
    ENGINE -->|watch channel| UI[SdrRemoteApp<br/>200 fields]
```

## Control Command Sequence
```mermaid
sequenceDiagram
    User->>UI: Click USB
    UI->>Engine: SetMode(1)
    Engine->>Server: ModePacket
    Server->>Thetis: MODULATION:0,USB;
    Thetis-->>Server: modulation:0,USB;
    Server-->>Engine: ModePacket
    Engine-->>UI: state.mode = 1
```

## Crate Structure

```
sdr-remote/
  sdr-remote-core/        (2,634 lines)
    protocol.rs            Packets, ControlId, PacketType
    codec.rs               Opus encoder/decoder
    jitter.rs              Jitter buffer
    auth.rs                HMAC-SHA256 auth

  sdr-remote-logic/        (3,198 lines)
    engine.rs              Network loop + audio pipeline
    commands.rs            Command enum (UI to Engine)
    state.rs               RadioState (Engine to UI)
    eq.rs                  5-band parametric EQ
    audio.rs               AudioBackend trait

  sdr-remote-server/       (17,641 lines)
    network.rs             UDP service + broadcast + dispatch
    tci.rs                 TCI WebSocket + state + parser
    spectrum.rs            FFT + smoothing + calibration
    ptt.rs                 PTT controller + radio backend
    cat.rs                 TCP CAT polling
    yaesu.rs               FT-991A serial + audio
    rotor.rs               EA7HG Visual Rotor
    main.rs                Server startup + config

  sdr-remote-client/       (11,761 lines)
    ui/mod.rs              App state (200 fields) + rendering
    ui/screens.rs          TCI controls + MIDI
    ui/spectrum.rs         Spectrum + waterfall
    ui/devices.rs          Equipment panels + Yaesu popout
    ui/meters.rs           S-meter + TX power bars
    ui/helpers.rs          Level bars, formatting
    audio.rs               cpal audio + stereo ring buffer
    midi.rs                MIDI controller (48 actions)
    main.rs                App entry point

  sdr-remote-android/      (Kotlin + Rust bridge)
    src/bridge.rs          UniFFI bridge
    android/               Compose UI
```

## Refactoring Priorities

| # | Task | Effort | Impact |
|---|------|--------|--------|
| 1 | Extract audio loops to audio_loops.rs, unify 3 identical TCI loops | Low | High |
| 2 | Split broadcast task (700 lines) to broadcast.rs | Medium | High |
| 3 | Collapse RX1/RX2 into ReceiverState[2] | High | High |
| 4 | Replace manual from_u8 with num_enum derive | Low | Medium |
| 5 | Break up SdrRemoteApp (200 fields) into sub-structs | High | High |
| 6 | Extract control dispatch to control_dispatch.rs | Low | Medium |
| 7 | Shared utils (freq mapping, dB conversion, resampler params) | Low | Medium |
