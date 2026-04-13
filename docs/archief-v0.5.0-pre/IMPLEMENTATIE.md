# ThetisLink Implementatie

Gedetailleerde beschrijving van de implementatie per module, inclusief datastromen en algoritmen.

## sdr-remote-core

### protocol.rs — Pakketdefinities (~900 LOC)

Alle pakkettypen als Rust structs met `serialize()` / `deserialize()` methoden.

**Wire format voorbeeld — AudioPacket:**
```
Offset  Bytes  Veld
0       1      magic (0xAA)
1       1      version (0x01)
2       1      packet_type (0x01)
3       1      flags (bit 0 = PTT)
4       4      sequence (u32 LE)
8       4      timestamp (u32 LE, ms)
12      2      opus_len (u16 LE)
14      N      opus_data
```

**ControlId enum:** 25+ besturingscommando's, elk met u8 ID en u16 waarde. Gebruikt voor bidirectionele state sync tussen server en client.

**EquipmentStatus/Command:** Variabele lengte, CSV-gecodeerde telemetrie in een `labels` string veld. Elke apparaattype heeft een eigen CSV-layout.

### codec.rs — Opus Audio Codec (~200 LOC)

| Parameter | Narrowband | Wideband |
|-----------|-----------|----------|
| Sample rate | 8 kHz | 16 kHz |
| Bitrate | 12.8 kbps | 24 kbps |
| Frame size | 160 samples (20ms) | 320 samples (20ms) |
| Bandwidth | Narrowband | Wideband |
| FEC | Inband, 10% loss | Inband, 10% loss |
| DTX | Aan | Aan |
| Signaaltype | Voice | Voice |

**Belangrijk:** Bitrate 12.8 kbps ligt net boven de Opus FEC drempel (12.4 kbps). Dit garandeert dat Forward Error Correction altijd meegestuurd wordt.

### jitter.rs — Adaptieve Jitter Buffer (~350 LOC)

```mermaid
graph TD
    A[UDP Pakket Ontvangen] --> B{Sequence < next_seq?}
    B -->|Ja: te laat| C[Drop pakket]
    B -->|Nee| D[Bereken jitter<br/>RFC 3550 dual-alpha]
    D --> E[Voeg toe aan BTreeMap]
    E --> F{Buffer > target+6?}
    F -->|Ja| G[Overflow recovery:<br/>drop 1-2 frames]
    F -->|Nee| H[Klaar]

    I[Pull 20ms tick] --> J{Frame beschikbaar<br/>voor next_seq?}
    J -->|Ja| K[Return Frame<br/>next_seq++]
    J -->|Nee| L{Volgende seq<br/>in buffer?}
    L -->|Ja| M[Return Missing<br/>gebruik FEC van volgende]
    L -->|Nee| N{Buffer leeg?}
    N -->|Ja| O[Return NotReady<br/>gebruik PLC]
    N -->|Nee| P{Ver achter?<br/>gap > max_depth}
    P -->|Ja| Q[Skip-ahead naar<br/>oudste frame]
    P -->|Nee| O
```

**Jitter schatting (RFC 3550 variant):**
```
deviation = |verwachte_interval - werkelijke_interval|

als deviation > huidige_schatting:
    jitter = jitter + 0.25 × (deviation - jitter)     # snelle attack
anders:
    jitter = jitter + 0.0625 × (deviation - jitter)   # trage decay
```

**Spike peak hold:** Piekwaarde met exponentieel verval (~1 minuut). Voorkomt dat buffer te snel krimpt na een netwerkpiek.

**Target depth formule:**
```
target = max(jitter_estimate, spike_peak) / 15.0 + 2
clamped: 2..40 frames (40ms..800ms)
```

**Grace period:** Eerste 25 pulls (500ms) na verbinding: geen overflow recovery. Laat buffer stabiliseren.

## sdr-remote-logic

### commands.rs — Command Enum (~90 varianten)

Commands worden via `mpsc::UnboundedSender<Command>` van UI naar engine gestuurd. Groepen:

| Groep | Voorbeelden | Aantal |
|-------|------------|--------|
| Verbinding | Connect, Disconnect | 2 |
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

### state.rs — RadioState (~170+ velden)

Broadcast van engine naar UI via `watch::Sender<RadioState>`. UI ontvangt via `watch::Receiver` met change notification.

**Belangrijkste groepen:**
- Verbinding: connected, rtt_ms, jitter_ms, buffer_depth, loss_percent
- Audio: capture_level, playback_level, playback_level_rx2
- Radio: frequency_hz, mode, smeter, power_on, filter_low/high_hz
- RX2: rx2_enabled, frequency_rx2_hz, mode_rx2, smeter_rx2
- Spectrum: spectrum_bins[], center_hz, span_hz, ref_level (RX1 + RX2)
- Apparaten: ~100 velden voor 6 apparaattypen

### engine.rs — ClientEngine (~2.181 LOC)

De engine is het hart van elke client. Draait als async tokio task.

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
        CMD --> VOL[Volume instellingen]
        CMD --> FREQ[Frequentie/Mode]
        CMD --> SPEC[Spectrum settings]
        CMD --> EQUIP[Equipment commands]
    end

    subgraph "UDP Ontvangst"
        UDP --> AUDIO_RX[Audio → Jitter Buffer]
        UDP --> HBACK[HeartbeatAck → RTT update]
        UDP --> STATE_UPD[Freq/Mode/Smeter → State]
        UDP --> SPEC_RX[Spectrum → State bins]
        UDP --> CTRL_RX[Control → State sync]
        UDP --> EQUIP_RX[EquipmentStatus → State]
    end

    subgraph "Audio Tick (20ms)"
        TICK --> PLAY[RX Playout]
        TICK --> CAP[TX Capture]
        PLAY --> JB1[RX1 Jitter Buffer Pull]
        PLAY --> JB2[RX2 Jitter Buffer Pull]
        JB1 --> DEC1[Opus Decode / FEC / PLC]
        JB2 --> DEC2[Opus Decode RX2]
        DEC1 --> RES1[Resample 8→48kHz]
        DEC2 --> RES2[Resample 8→48kHz]
        RES1 --> MIX[Mix RX1 + RX2]
        RES2 --> MIX
        MIX --> RING_OUT[Playback Ring Buffer]

        CAP --> RING_IN[Capture Ring Buffer]
        RING_IN --> RES_TX[Resample 48→8kHz]
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
    A[20ms Tick] --> B[Check Ring Buffer niveau]
    B --> C{Buffer < 60ms?}
    C -->|Ja| D[Pull 2 frames<br/>uit jitter buffer]
    C -->|Nee| E{Buffer > 200ms?}
    E -->|Ja| F[Skip pull<br/>laat buffer leeglopen]
    E -->|Nee| G[Pull 1 frame]

    D --> H{Frame beschikbaar?}
    G --> H
    H -->|Frame| I[Opus Decode<br/>160 samples i16]
    H -->|Missing| J[FEC Decode<br/>of PLC]
    H -->|NotReady| K[PLC<br/>comfort noise]

    I --> L[Resample 8→48kHz<br/>rubato SincFixedIn]
    J --> L
    K --> L

    L --> M[Apply Volume<br/>rx × vfoA × master]
    M --> N{RX2 enabled?}
    N -->|Ja| O[Pull RX2 frames<br/>match RX1 count]
    O --> P[Decode + Resample RX2]
    P --> Q[Apply Volume<br/>rx2 × vfoB × master]
    Q --> R[Mix: RX1 + RX2<br/>additief]
    N -->|Nee| R
    R --> S[Write naar<br/>Playback Ring Buffer]
```

#### Audio Capture (TX) — Detail

```mermaid
graph TD
    A[20ms Tick] --> B[Pull samples uit<br/>Capture Ring Buffer]
    B --> C[Resample 48→8kHz<br/>rubato SincFixedIn]
    C --> D[Apply TX Gain<br/>standaard 0.5]
    D --> E{AGC enabled?}
    E -->|Ja| F[TX AGC:<br/>target -12dB<br/>range ±20dB<br/>attack 0.3, release 0.01<br/>noise gate -60dB]
    E -->|Nee| G[Accumuleer in buffer]
    F --> G
    G --> H{160 samples?}
    H -->|Nee| I[Wacht op meer]
    H -->|Ja| J[Opus Encode<br/>160 smp → ~60 bytes]
    J --> K[Bouw AudioPacket<br/>seq++, timestamp]
    K --> L{PTT transitie?}
    L -->|Ja| M[Burst: 5 pakketten<br/>snel achter elkaar]
    L -->|Nee| N[Normaal verzenden]
    M --> O[UDP Send]
    N --> O
```

#### Frequentie Synchronisatie

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

    E->>E: ontvangen == pending?
    Note over E: Ja → pending_freq = None
    E->>E: state.frequency_hz = 7.035 MHz

    Note over T: Gebruiker draait aan VFO in Thetis
    T->>S: (CAT poll) ZZFA00007036000;
    S->>E: FrequencyPacket(7.036 MHz)
    E->>E: pending == None → accepteer
    E->>E: state.frequency_hz = 7.036 MHz
```

#### Volume Synchronisatie

```mermaid
graph TD
    A[Client start] --> B[rx_volume_synced = false]
    B --> C{Server stuurt<br/>Rx1AfGain control?}
    C -->|Ja| D[rx_volume_synced = true<br/>rx_volume = server waarde]
    C -->|Nee| E[Wacht...]

    D --> F{Gebruiker past<br/>RX volume aan?}
    F -->|Ja| G[Stuur SetRxVolume<br/>naar engine]
    G --> H[Engine stuurt<br/>Control naar server]
    H --> I[Server stuurt<br/>ZZLA naar Thetis]
```

## sdr-remote-server

### Hoofdstructuur

```mermaid
graph TB
    subgraph "main.rs — Opstart"
        CONF[Config laden/GUI]
        AUDIO[AudioPipeline init<br/>cpal devices]
        CAT_INIT[CAT verbinding<br/>TCP naar Thetis]
        HPSDR[HPSDR Capture<br/>DDC I/Q listener]
        EQUIP[Equipment Controllers<br/>COM/TCP/HTTP]
        NET[NetworkService starten]
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

### network.rs — NetworkService (~1.363 LOC)

Beheert alle UDP communicatie met clients.

```mermaid
graph TB
    subgraph "Async Tasks"
        HB[Heartbeat Responder<br/>500ms interval]
        TX[Audio TX Task<br/>Capture → Encode → Send]
        RX[Audio RX Task<br/>Recv → Decode → Playback]
        SPEC[Spectrum Task<br/>FFT → View Extract → Send]
        SPEC2[RX2 Spectrum Task]
        POLL[CAT Poll Task<br/>200ms interval]
        CTRL[Control Broadcast<br/>State sync naar clients]
    end

    subgraph "Gedeelde State"
        SESSION[SessionManager<br/>HashMap addr → ClientSession]
        PTT_CTRL[PttController<br/>Single-TX arbitrage]
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

**Polling cyclus:**

```mermaid
graph LR
    subgraph "Elke 200ms"
        A[ZZFA → freq RX1]
        B[ZZFB → freq RX2]
        C[ZZMD → mode RX1]
        D[ZZME → mode RX2]
        E[ZZLA → RX1 AF gain]
        F[ZZLE → RX2 AF gain]
        G[ZZPC → TX drive]
    end

    subgraph "Elke 100ms"
        H{TX actief?}
        H -->|Ja| I[ZZRM5 → fwd power]
        H -->|Nee| J[ZZSM0 → RX1 S-meter<br/>ZZSM1 → RX2 S-meter]
    end
```

**S-meter verwerking:**
1. ZZSM geeft raw waarde (0-260)
2. Conversie: `dBm = raw / 2 - 140`
3. Opslag als lineaire milliwatt: `mW = 10^(dBm/10)`
4. RMS middeling over sliding window (4 samples, ~0.4 sec)
5. Terug naar display: `avg_mw → dBm → raw (0-260)`

### spectrum.rs — SpectrumProcessor (~994 LOC)

**DDC FFT Pipeline:**

```mermaid
graph TD
    A[ANAN 7000DLE<br/>DDC I/Q samples] --> B[HPSDR Capture<br/>UDP :1037]
    B --> C[Accumuleer complex samples]
    C --> D{fft_size/2<br/>nieuwe samples?}
    D -->|Nee| C
    D -->|Ja| E[50% overlap:<br/>combineer met vorige helft]
    E --> F[Hann Window toepassen]
    F --> G[Complex FFT<br/>bijv. 262144-punt]
    G --> H[Magnitude berekenen<br/>20 × log10]
    H --> I[EMA Smoothing<br/>alpha = 0.1]
    I --> J[8192 bins output]

    J --> K{Per client}
    K --> L[View extractie:<br/>zoom & pan toepassen]
    L --> M[Downsample naar<br/>gevraagd aantal bins]
    M --> N[SpectrumPacket verzenden]
```

**FFT grootte selectie:**
```
target = sample_rate / 6
fft_size = next_power_of_two(target)
minimum = 4096

Voorbeelden:
  1536 kHz → 262144 (~12 FPS)
   384 kHz →  65536 (~12 FPS)
    96 kHz →  16384 (~12 FPS)
    48 kHz →   8192 (~12 FPS)
```

### ptt.rs — PTT Controller (~559 LOC)

**Single-TX Arbitrage:**

```mermaid
stateDiagram-v2
    [*] --> Idle: Start
    Idle --> TxActive: Client A PTT=1<br/>try_acquire_tx(A) → true
    TxActive --> Idle: Client A PTT=0<br/>release_tx(A)
    TxActive --> TxActive: Client B PTT=1<br/>try_acquire_tx(B) → false<br/>→ PttDenied

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

Alle apparaathandlers volgen hetzelfde patroon:

```mermaid
graph TD
    A[Init: open COM/TCP/HTTP] --> B[Polling Loop]
    B --> C[Status uitlezen]
    C --> D[CSV encoderen in<br/>EquipmentStatus labels]
    D --> E[Broadcast naar clients]
    E --> B

    F[EquipmentCommand<br/>van client] --> G[Parse command type]
    G --> H[Vertaal naar<br/>apparaat protocol]
    H --> I[Verstuur naar apparaat]
```

| Handler | Interface | Poll Interval | Telemetrie Velden |
|---------|-----------|---------------|-------------------|
| amplitec.rs (220 LOC) | COM 9600 | 1s | switch_a, switch_b, labels |
| tuner.rs (503 LOC) | COM 9600 | 500ms | state, can_tune |
| spe_expert.rs (568 LOC) | COM 9600 | 500ms | 12 velden (power, SWR, temp, ...) |
| rf2k.rs (1082 LOC) | HTTP :8080 | 500ms | 28+ velden incl. debug |
| ultrabeam.rs (461 LOC) | COM 9600 | 1s | freq, band, direction, elements |
| rotor.rs (245 LOC) | TCP :3010 | 500ms | angle, rotating, target |

## sdr-remote-client

### main.rs — Opstart

```mermaid
graph TD
    A[Start] --> B[Init tokio runtime]
    B --> C[Maak ClientAudio<br/>cpal devices]
    C --> D[Maak ClientEngine<br/>uit sdr-remote-logic]
    D --> E[Spawn engine<br/>in achtergrond]
    E --> F[Start eframe/egui<br/>rendering loop]
    F --> G[UI update() per frame]
```

### audio.rs — ClientAudio

```mermaid
graph LR
    subgraph "Input (Capture)"
        MIC[Microfoon] --> CPAL_IN[cpal Input Stream]
        CPAL_IN --> RING_IN[Ring Buffer<br/>lock-free SPSC]
    end

    subgraph "Output (Playback)"
        RING_OUT[Ring Buffer<br/>lock-free SPSC] --> CPAL_OUT[cpal Output Stream]
        CPAL_OUT --> SPK[Luidsprekers]
    end

    RING_IN -.->|Engine leest| ENGINE[Engine]
    ENGINE -.->|Engine schrijft| RING_OUT
```

### ui.rs — Desktop UI (~5.668 LOC)

Zie apart document: [UI.md](UI.md)

## sdr-remote-android

### Architectuur

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

**Bridge functies (Rust → Kotlin):**
- `version()` → String
- `state()` → BridgeRadioState (130+ velden)
- `connect(addr)`, `disconnect()`
- `set_ptt(bool)`, `set_frequency(hz)`, `set_mode(u8)`
- `set_rx_volume(f32)`, `set_local_volume(f32)`, `set_tx_gain(f32)`
- `set_control(id, value)`
- `enable_spectrum(bool)`, `set_spectrum_fps/zoom/pan()`

**Audio:** Oboe (Android Native Audio), 48kHz mono f32

## Netwerk Timing & Betrouwbaarheid

### Tijdslijn van een audio frame

```
t=0ms    Client capture ring buffer → samples beschikbaar
t=1ms    Resample 48→8kHz, Opus encode
t=2ms    UDP verzenden
t=Xms    Netwerk transit (RTT/2)
t=X+1ms  Server ontvangst
t=X+2ms  Opus decode, resample 8→48kHz
t=X+3ms  Playback ring buffer → naar Thetis
```

Totale one-way latency: ~3ms processing + netwerk transit + jitter buffer (40-800ms adaptief)

### Heartbeat & Verbindingsdetectie

```
Interval:     500ms
Timeout:      max(6000ms, RTT × 8)
RTT meting:   Echo timestamp in HeartbeatAck
Loss%:        Rolling window per heartbeat interval
Reconnect:    Reset codec + jitter buffer bij eerste HeartbeatAck
```

### Pakketverlies Herstel

| Scenario | Herstel Methode |
|----------|----------------|
| 1 pakket verloren | FEC uit volgend pakket |
| 2+ pakketten verloren | PLC (Packet Loss Concealment) |
| Burst verlies | Jitter buffer absorbeert tot target depth |
| Netwerk piek | Spike peak hold voorkomt te snelle buffer krimp |
| Verbinding weg | Timeout na 6s, reconnect bij nieuwe HeartbeatAck |

## v0.4.1 Wijzigingen

### ptt.rs — Two-Phase Connect

Verbinding met Thetis CAT is herschreven naar een two-phase connect patroon:

- **Nieuw:** `needed_connections()` — retourneert welke verbindingen opgezet moeten worden (CAT en/of TCI)
- **Nieuw:** `accept_connections()` — accepteert reeds verbonden TCP streams van de caller
- **Verwijderd:** `try_connect_cat()`, `ptt_flag()` — niet meer nodig door two-phase patroon
- `set_power()` ongewijzigd, maar wordt nu correct aangeroepen na ZZBY commando

### cat.rs — Two-Phase Connect

CAT interface gebruikt nu hetzelfde two-phase connect patroon:

- **Nieuw:** `needs_connect()` — geeft aan of een (her)verbinding nodig is
- **Nieuw:** `accept_stream()` — accepteert een reeds verbonden TcpStream
- **Verwijderd:** `try_connect()` (was dead code)
- `send()` triggert geen verbindingspogingen meer; retourneert stil als niet verbonden
- **Rate limit:** 1s interval tussen reconnect pogingen

### tci.rs — Two-Phase Connect

TCI WebSocket interface volgt hetzelfde patroon:

- **Nieuw:** `needs_connect_info()` — geeft aan of een (her)verbinding nodig is
- **Nieuw:** `accept_stream()` — accepteert een reeds verbonden WebSocket stream
- **Verwijderd:** `try_connect()` (was dead code)
- `send()` triggert geen verbinding meer; retourneert stil als niet verbonden
- **Rate limit:** 1s reconnect interval (was 2s)

### network.rs — Achtergrond Connect Tasks

De verbindingslogica is verplaatst naar achtergrond tokio tasks:

- `cat_tick` spawnt een achtergrond tokio task voor two-phase connect
- Drie TCI consumer tasks: `drop(ptt_guard)` voor `sleep` om lock contention te voorkomen
- `freq_tick`: 100ms interval (was 500ms) voor snellere frequentie-updates
- **Connect timeouts:** 100ms TCP, 500ms WebSocket — voorkomt blokkering van de main loop

### engine.rs — PowerOnOff & State Sync (sdr-remote-logic)

Power on/off logica verbeterd:

- **PowerOnOff lokale state:** `value == 1` (was `value != 0`) voor correcte toggle
- **state_tx.send()** direct na PowerOnOff voor onmiddellijke UI update
- **power_suppress_until:** 5 seconden onderdrukking van server power broadcasts na lokale toggle, voorkomt dat server state de lokale wijziging terugdraait
