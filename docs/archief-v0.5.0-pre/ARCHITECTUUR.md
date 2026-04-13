# ThetisLink Architectuur

## Overzicht

ThetisLink is een remote besturingssysteem voor de ANAN 7000DLE SDR met Thetis software. Het systeem bestaat uit een Windows-server (draait naast Thetis), en meerdere clients (Windows/macOS desktop, Android).

**Ontwerpprioriteit:** latency > bandbreedte > features

```mermaid
graph TB
    subgraph "Thetis PC (Windows)"
        Thetis[Thetis SDR Software]
        ANAN[ANAN 7000DLE]
        VACA[VB-Cable A<br/>RX Audio]
        VACB[VB-Cable B<br/>TX Audio]
        Server[ThetisLink Server]
        CAT[CAT TCP :13013]
        DDC[DDC I/Q UDP :1037]

        ANAN <-->|OpenHPSDR| Thetis
        Thetis -->|RX Audio| VACA
        VACB -->|TX Audio| Thetis
        VACA -->|Capture| Server
        Server -->|Playback| VACB
        Thetis <-->|CAT Commands| CAT
        Server <-->|CAT Polling| CAT
        ANAN -->|I/Q Data| DDC
        DDC -->|Spectrum| Server
    end

    subgraph "Netwerk (UDP)"
        UDP((UDP :4580))
    end

    subgraph "Clients"
        Desktop[Desktop Client<br/>Windows / macOS]
        Android[Android Client]
    end

    subgraph "Externe Apparaten"
        Amplitec[Amplitec 6/2<br/>Antenne Switch<br/>COM poort]
        Tuner[JC-4s Tuner<br/>COM poort]
        SPE[SPE Expert 1.3K-FA<br/>COM poort]
        RF2K[RF2K-S PA<br/>HTTP :8080]
        UB[UltraBeam RCU-06<br/>COM poort]
        Rotor[EA7HG Visual Rotor<br/>TCP :3010]
    end

    Server <--> UDP
    UDP <--> Desktop
    UDP <--> Android

    Server <-->|Serieel| Amplitec
    Server <-->|Serieel| Tuner
    Server <-->|Serieel| SPE
    Server <-->|HTTP| RF2K
    Server <-->|Serieel| UB
    Server <-->|TCP| Rotor
```

## Rust Workspace Structuur

```mermaid
graph LR
    subgraph "Workspace"
        Core[sdr-remote-core<br/>~1.943 LOC<br/>Protocol, Codec, Jitter]
        Logic[sdr-remote-logic<br/>~2.181 LOC<br/>Engine, State, Commands]
        Server[sdr-remote-server<br/>~12.856 LOC<br/>Netwerk, CAT, Apparaten]
        Client[sdr-remote-client<br/>~6.180 LOC<br/>Desktop UI]
        Android[sdr-remote-android<br/>~818 LOC<br/>FFI Bridge]
    end

    Logic --> Core
    Server --> Core
    Client --> Core
    Client --> Logic
    Android --> Core
    Android --> Logic
```

| Crate | Doel | Belangrijkste Dependencies |
|-------|------|---------------------------|
| `sdr-remote-core` | Gedeelde library: protocol, codec, jitter buffer | audiopus, anyhow, bytemuck |
| `sdr-remote-logic` | Client engine: audio pipeline, state, commands | tokio, core, rubato, ringbuf, cpal |
| `sdr-remote-server` | Windows server: netwerk, CAT, spectrum, apparaten | tokio, core, eframe/egui, serialport |
| `sdr-remote-client` | Desktop client: egui UI | tokio, core, logic, eframe/egui |
| `sdr-remote-android` | Android FFI bridge naar Kotlin/Compose UI | core, logic |

## Audio Routing

### RX Pad (Server → Client)

```mermaid
graph LR
    A[ANAN 7000DLE] -->|OpenHPSDR| B[Thetis]
    B -->|RX Audio 48kHz| C[VB-Cable A Out]
    C -->|Capture 48kHz| D[Server]
    D -->|Resample 48→8kHz| E[Opus Encode<br/>12.8 kbps]
    E -->|UDP Pakket| F[Client]
    F -->|Jitter Buffer| G[Opus Decode]
    G -->|Resample 8→48kHz| H[Volume<br/>rx × vfoA × master]
    H -->|Playback 48kHz| I[Luidsprekers]
```

### TX Pad (Client → Server)

```mermaid
graph LR
    A[Microfoon] -->|Capture 48kHz| B[Client]
    B -->|Resample 48→8kHz| C[TX AGC]
    C -->|Opus Encode| D[UDP Pakket<br/>+ PTT Flag]
    D --> E[Server]
    E -->|Opus Decode| F[Resample 8→48kHz]
    F -->|Playback| G[VB-Cable B In]
    G --> H[Thetis]
    H -->|TX Audio| I[ANAN 7000DLE]
```

### RX2 / VFO-B (apart audio kanaal)

```mermaid
graph LR
    A[Thetis RX2] -->|Audio 48kHz| B[VB-Cable C Out<br/>of 2e VAC]
    B -->|Capture| C[Server]
    C -->|Opus Encode| D[UDP AudioRx2]
    D --> E[Client]
    E -->|Aparte Jitter Buffer| F[Opus Decode]
    F -->|Resample 8→48kHz| G[Volume<br/>rx2 × vfoB × master]
    G -->|Mix met RX1| H[Playback]
```

## Protocol

### UDP Pakketformaat

Alle pakketten beginnen met een 4-byte header:

```
[magic: 0xAA] [version: 0x01] [type: u8] [flags: u8]
```

### Pakkettypen

| Type | ID | Richting | Grootte | Beschrijving |
|------|----|----------|---------|--------------|
| Audio | 0x01 | S→C / C→S | 14+N | RX1 audio (Opus gecodeerd) |
| Heartbeat | 0x02 | C→S | 20 | Keep-alive + capabilities |
| HeartbeatAck | 0x03 | S→C | 16 | RTT meting + server capabilities |
| Control | 0x04 | Beide | 7 | Besturingsopdracht (id + waarde) |
| Disconnect | 0x05 | C→S | 4 | Verbreek verbinding |
| PttDenied | 0x06 | S→C | 4 | PTT geweigerd (andere zender actief) |
| Frequency | 0x07 | Beide | 12 | VFO-A frequentie (u64 Hz) |
| Mode | 0x08 | Beide | 5 | VFO-A modus (u8) |
| Smeter | 0x09 | S→C | 6 | S-meter niveau (u16, 0-260) |
| Spectrum | 0x0A | S→C | 18+N | Spectrum bins (per-client view) |
| FullSpectrum | 0x0B | S→C | 18+N | Waterfall data (volledige DDC) |
| EquipmentStatus | 0x0C | S→C | Variabel | Apparaatstatus (CSV gecodeerd) |
| EquipmentCommand | 0x0D | C→S | Variabel | Apparaatopdracht |
| AudioRx2 | 0x0E | S→C | 14+N | RX2 audio (apart kanaal) |
| FrequencyRx2 | 0x0F | Beide | 12 | VFO-B frequentie |
| ModeRx2 | 0x10 | Beide | 5 | VFO-B modus |
| SmeterRx2 | 0x11 | S→C | 6 | RX2 S-meter |
| SpectrumRx2 | 0x12 | S→C | 18+N | RX2 spectrum |
| FullSpectrumRx2 | 0x13 | S→C | 18+N | RX2 waterfall |

### Capabilities (u32 bitmask in Heartbeat)

| Bit | Naam | Beschrijving |
|-----|------|--------------|
| 0 | WIDEBAND_AUDIO | Client ondersteunt 16kHz Opus |
| 1 | SPECTRUM | Client wil spectrum/waterfall data |
| 2 | RX2 | Client ondersteunt dual receiver |

### Control IDs

| ID | Naam | Bereik | Beschrijving |
|----|------|--------|--------------|
| PowerOnOff | u16 | 0/1/2 | Aan/uit, 2=shutdown (ZZBY) |
| TxProfile | u16 | 0-99 | TX profiel nummer |
| NoiseReduction | u16 | 0-4 | 0=uit, 1-4=NR niveau |
| AutoNotchFilter | u16 | 0/1 | ANF aan/uit |
| DriveLevel | u16 | 0-100 | TX drive |
| Rx1AfGain | u16 | 0-100 | Thetis RX1 volume (ZZLA) |
| Rx2AfGain | u16 | 0-100 | Thetis RX2 volume (ZZLE) |
| FilterLow | i16 | Hz | Filter ondergrens |
| FilterHigh | i16 | Hz | Filter bovengrens |
| SpectrumEnable | u16 | 0/1 | Spectrum aan/uit |
| SpectrumFps | u16 | 5-30 | Spectrum framerate |
| SpectrumZoom | u16 | 1-1024 | Spectrum zoom factor |
| SpectrumPan | i16 | -500..500 | Spectrum pan (‰) |
| Rx2Enable | u16 | 0/1 | RX2 aan/uit |
| VfoSync | u16 | 0/1 | VFO-B volgt VFO-A |
| Rx2Spectrum* | | | Zelfde set voor RX2 |
| Rx2NoiseReduction | u16 | 0-4 | RX2 NR niveau |
| Rx2AutoNotchFilter | u16 | 0/1 | RX2 ANF |

## Thetis CAT Commando's

De server pollt Thetis via TCP CAT (poort 13013):

### Polling (elke 200ms tenzij anders)

| Commando | Interval | Beschrijving |
|----------|----------|--------------|
| ZZFA; | 200ms | RX1 frequentie uitlezen |
| ZZFB; | 200ms | RX2 frequentie uitlezen |
| ZZMD; | 200ms | RX1 modus |
| ZZME; | 200ms | RX2 modus |
| ZZLA; | 200ms | RX1 AF gain |
| ZZLE; | 200ms | RX2 AF gain |
| ZZPC; | 200ms | TX drive level |
| ZZSM0; | 100ms | RX1 S-meter (peak, 0-260) |
| ZZSM1; | 100ms | RX2 S-meter (peak, 0-260) |
| ZZRM5; | 100ms | Forward power (alleen tijdens TX) |
| ZZNE; | 200ms | Noise reduction niveau |
| ZZNT; | 200ms | Auto-notch filter |

### Aanstuurcommando's

| Commando | Beschrijving |
|----------|--------------|
| ZZFA{freq}; | Stel RX1 frequentie in |
| ZZFB{freq}; | Stel RX2 frequentie in |
| ZZMD{mode}; | Stel RX1 modus in |
| ZZME{mode}; | Stel RX2 modus in |
| ZZTX1; / ZZTX0; | PTT aan/uit |
| ZZTP{N}; | TX profiel selecteren |
| ZZNE{N}; | Noise reduction instellen |
| ZZNT{0/1}; | Auto-notch filter |
| ZZPC{N}; | Drive level instellen |
| ZZLA{N}; | RX1 AF gain instellen |
| ZZLE{N}; | RX2 AF gain instellen |
| ZZBY; | Thetis afsluiten (shutdown) |
| ZZFD{low},{high}; | RX1 filter instellen |
| ZZFS{low},{high}; | RX2 filter instellen |

## Externe Apparaten

```mermaid
graph TB
    Server[ThetisLink Server]

    subgraph "Serieel (COM poort)"
        Amp[Amplitec 6/2<br/>Antenne Switch<br/>220 LOC]
        Tuner[JC-4s Tuner<br/>503 LOC]
        SPE[SPE Expert 1.3K-FA<br/>568 LOC]
        UB[UltraBeam RCU-06<br/>461 LOC]
    end

    subgraph "Netwerk"
        RF2K[RF2K-S PA<br/>HTTP :8080<br/>1082 LOC]
        Rotor[EA7HG Visual Rotor<br/>TCP :3010<br/>245 LOC]
    end

    Server <-->|9600 baud| Amp
    Server <-->|9600 baud| Tuner
    Server <-->|9600 baud| SPE
    Server <-->|9600 baud| UB
    Server <-->|HTTP REST| RF2K
    Server <-->|TCP socket| Rotor
```

| Apparaat | Interface | Protocol | Functies |
|----------|-----------|----------|----------|
| Amplitec 6/2 | COM | Serieel | 2× 6-pos antenneschakelaar |
| JC-4s Tuner | COM | Serieel | Tune/abort, status polling |
| SPE Expert 1.3K-FA | COM | Serieel | Operate/tune, telemetrie (power, SWR, temp) |
| RF2K-S | HTTP :8080 | REST | Operate/tune, antenne, tuner, drive, debug |
| UltraBeam RCU-06 | COM | Serieel | Retract, frequentie, elementen uitlezen |
| EA7HG Visual Rotor | TCP :3010 | Socket | Goto/stop/CW/CCW, hoek uitlezen |

## Multi-Client Architectuur

```mermaid
sequenceDiagram
    participant C1 as Client A (Desktop)
    participant C2 as Client B (Android)
    participant S as Server
    participant T as Thetis

    C1->>S: Heartbeat (caps: SPECTRUM|RX2)
    S->>C1: HeartbeatAck
    C2->>S: Heartbeat (caps: SPECTRUM)
    S->>C2: HeartbeatAck

    Note over S: Beide clients actief

    S->>C1: Audio + Spectrum + RX2 Audio
    S->>C2: Audio + Spectrum

    C1->>S: Audio + PTT=1
    S->>T: ZZTX1;
    Note over S: Client A heeft TX lock

    C2->>S: Audio + PTT=1
    S->>C2: PttDenied
    Note over C2: PTT geweigerd

    C1->>S: Audio + PTT=0
    S->>T: ZZTX0;
    Note over S: TX lock vrijgegeven
```

## Configuratie

### Server (thetislink-server.conf)

JSON-bestand naast de executable met:
- Audio devices (input, input2 voor RX2, output)
- CAT adres (standaard 127.0.0.1:13013)
- ANAN interface en DDC sample rate
- Spectrum instellingen
- Thetis.exe pad (autostart)
- COM poorten per apparaat
- RF2K netwerkadres
- Window posities/groottes
- Actieve PA selectie

### Client (thetislink-client.conf)

JSON-bestand naast de executable met:
- Server adres
- Audio devices (input/output)
- Volumes (rx, vfoA, vfoB, master, tx gain)
- Window posities/groottes
- Spectrum instellingen
- Band geheugens (freq/mode/filter/NR per band)

## Server: Two-Phase Connect Pattern

De server gebruikt een two-phase connect pattern voor TCI- en CAT-verbindingen. Verbindingsopbouw (TCP connect, WebSocket handshake) vindt plaats **buiten** de ptt-lock, zodat de hoofdpakketloop niet geblokkeerd wordt door trage of falende connects.

1. **Fase 1 — Connect (zonder lock):** De TCI/CAT verbinding wordt opgezet in een aparte scope, zonder de ptt mutex vast te houden. Dit voorkomt dat een trage DNS-lookup, TCP timeout of WebSocket handshake de hele pakketverwerking blokkeert.
2. **Fase 2 — Registreer (met lock):** Pas nadat de verbinding succesvol is, wordt de ptt mutex kort vergrendeld om de verbinding te registreren in de gedeelde state.

Dit patroon is essentieel omdat de pakketloop ook PTT-events verwerkt — een blokkerende connect zou de PTT-latency onacceptabel verhogen.

## Power State Flow

De `Engine` (in `sdr-remote-logic`) is de **single source of truth** voor de `power_on` state. Het verloop:

1. Client stuurt een PowerOnOff control pakket naar de server.
2. De server voert het CAT/TCI commando uit (bijv. `ZZPS1;` of TCI `start`).
3. De engine publiceert de nieuwe power state **onmiddellijk** naar de UI, zonder te wachten op bevestiging van de server.
4. Om te voorkomen dat een verouderde server-broadcast de lokale state overschrijft, onderdrukt de engine inkomende power-updates van de server gedurende **5 seconden** na het versturen van een power commando.
5. Na de suppressieperiode synchroniseert de engine weer normaal met de server-state.

Zowel de desktop client als de Android client gebruiken exact hetzelfde mechanisme via de gedeelde `Engine` in `sdr-remote-logic`.

## TCI Modus: Lock Contention Fix

In de TCI consumer tasks (die TCI WebSocket berichten verwerken) werd de mutex te lang vastgehouden: de lock bleef actief **over een sleep heen**. Dit veroorzaakte lock contention met de hoofdloop, waardoor commando-responstijden opliepen tot ~600ms.

**Fix:** De mutex wordt nu gedropt **voor** de sleep. Hierdoor daalde de commando-responstijd van ~600ms naar <1ms. Patroon:

```rust
// Fout: lock over sleep heen
let guard = mutex.lock().await;
// ... verwerk data ...
tokio::time::sleep(interval).await; // guard nog actief!

// Goed: drop voor sleep
{
    let guard = mutex.lock().await;
    // ... verwerk data ...
} // guard gedropt
tokio::time::sleep(interval).await;
```
