# ThetisLink — Technische Documentatie

## Overzicht

ThetisLink is een systeem voor het op afstand bedienen van een ANAN 7000DLE + Thetis SDR-ontvanger en een Yaesu FT-991A transceiver via een netwerkverbinding. Het biedt bidirectionele real-time audio streaming, PTT-bediening, DDC spectrum/waterfall display, volledige RX2/VFO-B ondersteuning, Yaesu memory channel management en radio settings editor over UDP met Opus codec.

**Versie:** v0.5.0 (gedeeld versienummer in `sdr-remote-core::VERSION`)
**Ontwikkeltaal:** Rust + Kotlin (Android UI)
**Doelplatform:** Windows 10/11, macOS (Intel/Apple Silicon), Android 8+ (arm64)
**Status:** Android Yaesu integratie, BT headset auto-detect, TCI v2.10.3.13 controls, diversity fix
**Ontwerpprioriteit:** latency > bandbreedte > features

---

## Architectuur

### TCI modus (v0.4.0+, aanbevolen en primaire modus)

```
┌──────────────────────────────┐           UDP           ┌───────────────────────────┐
│         SDR Remote           │    ◄──── 4580 ────►     │      SDR Remote           │
│          Client              │                         │       Server              │
│                              │                         │                           │
│  Microfoon ──► Opus enc ─────┼────────────────────────►│──► TCI TX_AUDIO_STREAM ──►│
│                              │    Audio + PTT packets  │                           │
│  Speaker ◄── Opus dec ◄──────┼◄───────────────────────│◄── TCI RX_AUDIO_STREAM   │
│                              │                         │                           │
│  UI: egui (desktop) of       │                         │  TCI WebSocket (:40001)   │
│  Jetpack Compose (Android)   │                         │  + CAT TCP (:13013)       │
└──────────────────────────────┘                         └───────────────────────────┘
       Laptop/telefoon                                      PC naast Thetis radio
```

TCI modus gebruikt één WebSocket verbinding naar Thetis voor audio, IQ en commando's. Een parallelle TCP CAT verbinding handelt commando's af die TCI (nog) niet ondersteunt (ZZLA, ZZLE, ZZBY, ZZCT, ZZPS, ZZTP).

**Voordelen t.o.v. legacy:**
- Geen VB-Audio Virtual Cable nodig
- Geen Administrator-rechten nodig (geen raw socket DDC capture)
- Eenvoudiger setup (alleen TCI URL + CAT adres)

### Legacy modus (VB-Cable) — verouderd

> **Let op:** De legacy VB-Cable modus is verouderd. TCI is de primaire en aanbevolen verbindingsmodus vanaf v0.4.0. De VB-Cable modus wordt nog ondersteund voor backward-compatibiliteit maar ontvangt geen nieuwe features meer.

```
┌──────────────────────────────┐           UDP           ┌───────────────────────────┐
│         SDR Remote           │    ◄──── 4580 ────►     │      SDR Remote           │
│          Client              │                         │       Server              │
│                              │                         │                           │
│  Microfoon ──► Opus enc ─────┼────────────────────────►│──► Opus dec ──► VB-Cable-B │
│                              │    Audio + PTT packets  │                    ↓       │
│  Speaker ◄── Opus dec ◄──────┼◄───────────────────────│◄── Opus enc ◄── VB-Cable-A │
│                              │                         │                           │
│  UI: egui (desktop) of       │                         │  Thetis CAT (TCP:13013)   │
│  Jetpack Compose (Android)   │                         │  PTT: ZZTX1;/ZZTX0;      │
└──────────────────────────────┘                         └───────────────────────────┘
       Laptop/telefoon                                      PC naast Thetis radio
```

### Audio Routing

**TCI modus:** Audio gaat direct via de TCI WebSocket (RX_AUDIO_STREAM / TX_AUDIO_STREAM). Geen VB-Cable nodig.

**Legacy modus (via VB-Audio Virtual Cable A+B):**
```
Thetis RX output ──► VB-Cable-A ──► Server capture ──► resample ──► Opus enc ──► UDP ──► Client
Client mic ──► Opus enc ──► UDP ──► Server ──► Opus dec ──► resample ──► VB-Cable-B ──► Thetis TX input
```

---

## Workspace Structuur

```
sdr-remote/
├── Cargo.toml                  # Workspace root
├── DOCUMENTATIE.md             # Dit document
├── sdr-remote-core/            # Gedeelde library (protocol, codec, jitter)
│   └── src/
│       ├── lib.rs              # Constanten (sample rates, frame sizes, poort)
│       ├── protocol.rs         # Packet format, serialisatie, deserialisatie
│       ├── codec.rs            # Opus encode/decode wrapper
│       └── jitter.rs           # Adaptieve jitter buffer
├── sdr-remote-logic/           # Platform-onafhankelijke client engine
│   └── src/
│       ├── lib.rs              # Crate root
│       ├── state.rs            # RadioState (read-only, broadcast via watch channel)
│       ├── commands.rs         # Command enum (UI → engine via mpsc channel)
│       ├── audio.rs            # AudioBackend trait (platform-abstractie)
│       └── engine.rs           # ClientEngine (netwerk, codec, resampling, jitter)
├── sdr-remote-server/          # Windows server (draait naast Thetis)
│   └── src/
│       ├── main.rs             # Opstart, argument parsing, shutdown
│       ├── audio.rs            # cpal WASAPI capture + playback via VB-Cable
│       ├── network.rs          # UDP send/receive, resampling, playout timer
│       ├── cat.rs              # CatConnection: Thetis CAT TCP + radio state
│       ├── tci.rs              # TciConnection: TCI WebSocket client + state + streams
│       ├── ptt.rs              # PttController: PTT safety (bevat Cat/TciConnection)
│       ├── session.rs          # Client sessie management (multi-client)
│       ├── spectrum.rs         # SpectrumProcessor: DDC FFT pipeline + test generator
│       ├── hpsdr_capture.rs    # HPSDR wideband + DDC I/Q capture (raw socket, Windows)
│       ├── config.rs           # Server configuratie (persistent)
│       └── ui.rs               # Server GUI (anan_interface instelling)
├── sdr-remote-client/          # Desktop client (egui)
│   └── src/
│       ├── main.rs             # Opstart, engine + UI threading
│       ├── audio.rs            # cpal AudioBackend impl + device listing
│       ├── ui.rs               # egui UI, config opslag
│       ├── websdr.rs           # Win32 venster + wry WebView (embedded WebSDR/KiwiSDR)
│       └── catsync.rs          # WebSDR kanaalcommunicatie, favorites, debounced freq sync
└── sdr-remote-android/         # Android client (Kotlin/Compose + Rust via UniFFI)
    ├── src/
    │   ├── lib.rs              # JNI entrypoint, Android logging
    │   ├── bridge.rs           # UniFFI bridge (Rust ↔ Kotlin)
    │   ├── audio_oboe.rs       # Oboe AudioBackend impl (AAudio)
    │   └── sdr_remote.udl      # UniFFI interface definitie
    └── android/                # Android Studio project
        └── app/src/main/java/com/sdrremote/
            ├── MainActivity.kt
            ├── viewmodel/SdrViewModel.kt
            └── ui/
                ├── screens/MainScreen.kt
                └── components/         # Compose UI componenten
```

### Client Architectuur (Fase 1)

```
┌──────────────────┐   watch::Receiver<RadioState>   ┌───────────────────┐
│  SdrRemoteApp    │ ◄────────────────────────────── │   ClientEngine    │
│  (egui UI)       │                                  │  (sdr-remote-     │
│  main thread     │ ────────────────────────────► │   logic)          │
└──────────────────┘   mpsc::Sender<Command>         │  tokio thread     │
                                                      │                   │
                                                      │  ┌─AudioBackend─┐ │
                                                      │  │ ClientAudio  │ │
                                                      │  │ (cpal)       │ │
                                                      │  └──────────────┘ │
                                                      └───────────────────┘
```

De UI leest state via een `watch` channel (non-blocking borrow) en stuurt commands via een `mpsc` channel. Geen `Arc<Mutex<>>` meer — de engine is eigenaar van alle netwerk- en audiostate. Dit maakt de business logic platform-onafhankelijk: de Android client implementeert alleen `AudioBackend` (Oboe) en de UI (Compose), de engine blijft identiek.

### Android Client Architectuur (Fase 2)

```
┌──────────────────┐   UniFFI (FFI bridge)   ┌───────────────────┐
│  Jetpack Compose  │ ◄────────────────────── │   bridge.rs       │
│  (Kotlin UI)      │   BridgeRadioState      │   (sdr-remote-    │
│  main thread      │ ────────────────────► │    android)       │
└──────────────────┘   connect/setPtt/etc     │                   │
        ↑                                      │  ┌─ClientEngine─┐ │
        │ collectAsStateWithLifecycle          │  │ (sdr-remote-  │ │
  ┌─────┴──────┐                               │  │  logic)       │ │
  │SdrViewModel│  polling (33ms)               │  ├─AudioBackend─┤ │
  │ (StateFlow)│ ◄──── getState() ────────── │  │ OboeAudio    │ │
  └────────────┘                               │  │ (AAudio)      │ │
                                               │  └──────────────┘ │
                                               └───────────────────┘
```

**UniFFI bridge:** `sdr_remote.udl` definieert de FFI interface. `uniffi-bindgen` genereert Kotlin bindings. De bridge runt de `ClientEngine` in een tokio runtime op een achtergrond thread. UI pollt state via `getState()` op 30fps.

**Audio:** Oboe (AAudio) met `PerformanceMode::LowLatency`, `SharingMode::Exclusive`. Capture: `InputPreset::VoiceRecognition` (geen AGC/noise suppression). Playback: `Usage::Media` (speaker, niet earpiece).

**Build:** `cargo ndk -t arm64-v8a build --release` → kopieert `libsdr_remote_android.so` naar `jniLibs/arm64-v8a/`.

### Server Architectuur

```
┌──────────────────┐
│  PttController   │
│  ├── PTT safety (timeouts, tail delay)
│  ├── RadioBackend::Cat(CatConnection)     ← legacy modus
│  │    ├── TCP naar Thetis CAT
│  │    ├── Radio state polling
│  │    └── S-meter polling
│  └── RadioBackend::Tci(TciConnection)     ← TCI modus (primair)
│       ├── WebSocket naar Thetis TCI
│       ├── Push-based state updates
│       ├── Audio streams (RX/TX)
│       ├── IQ streams → spectrum
│       └── aux_cat: CatConnection          ← parallel CAT
│            └── Commands TCI niet kent
└──────────────────┘
```

`PttController` ondersteunt twee backends: `CatConnection` (legacy, TCP CAT) en `TciConnection` (TCI WebSocket). In TCI modus draait een parallelle `CatConnection` ("aux_cat") voor commando's die TCI nog niet ondersteunt (ZZLA, ZZLE, ZZBY, ZZCT, ZZPS, ZZTP).

---

## Dependencies

| Crate | Versie | Doel |
|-------|--------|------|
| `tokio` | 1 (full) | Async runtime (UDP, TCP, timers) |
| `audiopus` | 0.3.0-rc.0 | Opus codec bindings (8kHz, mono, FEC) |
| `cpal` | 0.15 | Audio I/O (WASAPI op Windows, desktop client) |
| `rubato` | 0.14 | Resampling (sinc interpolatie) |
| `ringbuf` | 0.4 | Lock-free SPSC ring buffer (audio thread ↔ netwerk thread) |
| `eframe`/`egui` | 0.29 | Desktop client UI |
| `bytemuck` | 1 | Zero-copy byte casting |
| `log`/`env_logger` | 0.4/0.11 | Logging |
| `anyhow` | 1 | Foutafhandeling |
| `oboe` | 0.6 | Audio I/O (AAudio op Android) |
| `uniffi` | 0.28 | Rust ↔ Kotlin FFI bridge |
| `rustfft` | 6 | FFT voor spectrum verwerking (server) |
| `socket2` | 0.5 | Raw socket voor HPSDR capture (server, legacy) |
| `tokio-tungstenite` | 0.24 | WebSocket client voor TCI (server) |
| `futures-util` | 0.3 | StreamExt/SinkExt voor WebSocket (server) |
| `wry` | 0.x | WebView voor embedded WebSDR/KiwiSDR venster (client) |

**Build-optimalisatie:** Dependencies worden ook in dev mode geoptimaliseerd (`[profile.dev.package."*"] opt-level = 2`) omdat Opus en rubato te traag zijn zonder optimalisatie.

---

## UDP Protocol

Binair handgeschreven protocol op UDP poort **4580**. Alle multi-byte waarden zijn big-endian.

### Header (4 bytes)

Elk packet begint met dezelfde header:

| Offset | Grootte | Veld | Waarde |
|--------|---------|------|--------|
| 0 | 1 | Magic | `0xAA` |
| 1 | 1 | Version | `1` |
| 2 | 1 | PacketType | Zie onder |
| 3 | 1 | Flags | Bit 0 = PTT actief |

### Packet Types

#### Audio Packet (0x01) — variabele lengte

Draagt gecodeerde Opus audio + PTT status. Wordt 50x per seconde verzonden (elke 20ms).

| Offset | Grootte | Veld | Beschrijving |
|--------|---------|------|--------------|
| 0-3 | 4 | Header | Magic, version, type=0x01, flags |
| 4-7 | 4 | Sequence | Oplopend volgnummer (u32, wrapping) |
| 8-11 | 4 | Timestamp | Milliseconden sinds start (u32) |
| 12-13 | 2 | OpusLen | Lengte opus data in bytes (u16) |
| 14+ | N | OpusData | Opus-gecodeerde audio |

**Totaal:** 14 + N bytes (typisch ~40-60 bytes bij 12.8 kbps)

PTT-only packets (zonder audio) hebben OpusLen=0 en worden gebruikt voor PTT burst bij state change.

#### Heartbeat (0x02) — 16-20 bytes

Periodiek (elke 500ms) door client verzonden voor verbindingsbewaking en RTT-meting.

| Offset | Grootte | Veld | Beschrijving |
|--------|---------|------|--------------|
| 0-3 | 4 | Header | Magic, version, type=0x02, flags |
| 4-7 | 4 | Sequence | Heartbeat volgnummer (u32) |
| 8-11 | 4 | LocalTime | Client timestamp in ms (u32) |
| 12-13 | 2 | RTT | Laatst gemeten RTT in ms (u16) |
| 14 | 1 | LossPercent | Geschat packet loss % (u8) |
| 15 | 1 | JitterMs | Geschatte jitter in ms (u8) |
| 16-19 | 4 | Capabilities | Client capability flags (u32, optioneel) |

Backward compatible: oude clients zonder capabilities (16 bytes) worden geaccepteerd.

#### HeartbeatAck (0x03) — 12-16 bytes

Server antwoord op Heartbeat. Echoot de client's timestamp terug voor RTT-berekening.

| Offset | Grootte | Veld | Beschrijving |
|--------|---------|------|--------------|
| 0-3 | 4 | Header | Magic, version, type=0x03, flags |
| 4-7 | 4 | EchoSequence | Geëchode heartbeat sequence |
| 8-11 | 4 | EchoTime | Geëchode client timestamp |
| 12-15 | 4 | Capabilities | Negotiated capabilities (u32, optioneel) |

**Capability flags:** Bit 0 = Wideband Audio (16kHz Opus). Negotiation via intersectie: server stuurt alleen flags die beide kanten ondersteunen.

#### Control Packet (0x04) — 7 bytes

Stuurt bedieningscommando's van client naar server.

| Offset | Grootte | Veld | Beschrijving |
|--------|---------|------|--------------|
| 0-3 | 4 | Header | Magic, version, type=0x04, flags |
| 4 | 1 | ControlId | Type besturing (zie onder) |
| 5-6 | 2 | Value | Waarde (u16) |

**ControlId waarden:**

| ID | Naam | Beschrijving | Waarden | CAT commando |
|----|------|--------------|---------|--------------|
| 0x01 | Rx1AfGain | RX1 ontvanger volume | 0-100 | `ZZLA{:03};` |
| 0x02 | PowerOnOff | Thetis power aan/uit | 0/1 | `ZZPS{};` |
| 0x03 | TxProfile | TX profiel index | 0-99 | `ZZTP{:02};` |
| 0x04 | NoiseReduction | Noise Reduction level | 0-4 (0=uit, 1=NR1..4=NR4) | `ZZNE{};` |
| 0x05 | AutoNotchFilter | Auto Notch Filter aan/uit | 0/1 | `ZZNT{};` |
| 0x06 | DriveLevel | Zendvermogen (drive) | 0-100 | `ZZPC{:03};` |
| 0x07 | SpectrumEnable | Spectrum aan/uit | 0/1 | — |
| 0x08 | SpectrumFps | Spectrum framerate | 5-30 | — |
| 0x09 | SpectrumZoom | Zoom niveau (×10) | 10-10240 | — |
| 0x0A | SpectrumPan | Pan positie ((pan+0.5)×10000) | 0-10000 | — |
| 0x0B | FilterLow | Filter low cut (signed Hz offset als i16→u16) | signed Hz | `ZZFL{};` |
| 0x0C | FilterHigh | Filter high cut (signed Hz offset als i16→u16) | signed Hz | `ZZFH{};` |

Control packets zijn **bidirectioneel**: client→server stuurt wijzigingen, server→client broadcast de huidige staat (elke 500ms). FilterLow/FilterHigh zijn bidirectioneel: server→client broadcast Thetis-waarden, client→server stuurt nieuwe filter-randen via `ZZFL`/`ZZFH` CAT commando's.

#### Disconnect Packet (0x05) — 4 bytes

Nette afmelding. Alleen de header, geen payload.

#### PttDenied Packet (0x06) — 4 bytes

Server → client. Verstuurd wanneer een client PTT aanvraagt terwijl een andere client de TX lock bezit. Alleen de header, geen payload.

#### Frequency Packet (0x07) — 12 bytes

Bidirectioneel: server→client (readback) en client→server (set).

| Offset | Grootte | Veld | Beschrijving |
|--------|---------|------|--------------|
| 0-3 | 4 | Header | Magic, version, type=0x07, flags |
| 4-11 | 8 | FrequencyHz | VFO-A frequentie in Hz (u64) |

#### Mode Packet (0x08) — 5 bytes

Bidirectioneel. Mode waarden: 00=LSB, 01=USB, 05=FM, 06=AM (Thetis ZZMD waarden).

| Offset | Grootte | Veld | Beschrijving |
|--------|---------|------|--------------|
| 0-3 | 4 | Header | Magic, version, type=0x08, flags |
| 4 | 1 | Mode | Operating mode (u8) |

#### S-meter Packet (0x09) — 6 bytes

Server→client. Raw waarde 0-260 (12 per S-unit, S9=108).

| Offset | Grootte | Veld | Beschrijving |
|--------|---------|------|--------------|
| 0-3 | 4 | Header | Magic, version, type=0x09, flags |
| 4-5 | 2 | Level | S-meter niveau (u16) |

#### Spectrum Packet (0x0A) — 18 + N bytes

Server→client. Spectrum data gecentreerd op VFO frequentie (DDC I/Q modus) of wideband (0–61.44 MHz). Wordt alleen gestuurd naar clients die spectrum hebben aangevraagd via ControlId::SpectrumEnable. Aantal bins is dynamisch per client (afhankelijk van schermresolutie × zoom).

| Offset | Grootte | Veld | Beschrijving |
|--------|---------|------|--------------|
| 0-3 | 4 | Header | Magic, version, type=0x0A, flags |
| 4-5 | 2 | Sequence | Frame volgnummer (u16, wrapping) |
| 6-7 | 2 | NumBins | Aantal spectrum bins (u16, dynamisch 512-32768) |
| 8-11 | 4 | CenterFreqHz | Centrum frequentie in Hz (u32, = DDC center freq) |
| 12-15 | 4 | SpanHz | Span in Hz (u32, = DDC sample rate, 384000 bij TCI) |
| 16 | 1 | RefLevel | Referentieniveau dBm (i8) |
| 17 | 1 | DbPerUnit | Bin breedte: 1=u8 (1 byte/bin), 2=u16 (2 bytes/bin) |
| 18+ | N×B | Bins | Log power waarden (u8: 0-255, u16: 0-65535, maps -150...-30 dB) |

**Dynamic bins:** Desktop client berekent automatisch `screen_width × zoom` (range 512-32768). Server extraheert per client het juiste aantal bins uit de FFT smoothed buffer.

**Instelbare FFT size:** Client kan de server FFT size instellen (32K/64K/128K/256K). Kleinere FFT = meer FFT frames/sec = snellere spectrum response, maar lagere frequentieresolutie.

| FFT size | Hz/bin @384kHz | FFT/sec | Data window |
|----------|---------------|---------|-------------|
| 32768 | 11.7 | ~47 | 85ms |
| 65536 | 5.9 | ~23 | 171ms |
| 131072 | 2.9 | ~12 | 341ms |
| 262144 (auto) | 1.5 | ~6 | 683ms |

**ControlId uitbreidingen voor spectrum:**

| ID | Naam | Beschrijving | Waarden |
|----|------|--------------|---------|
| 0x07 | SpectrumEnable | Spectrum aan/uit per client | 0/1 |
| 0x08 | SpectrumFps | Spectrum update rate | 5-30 |
| 0x1A | SpectrumMaxBins | Max bins per packet | 64-32768 (0=default 8192) |
| 0x1B | Rx2SpectrumMaxBins | RX2 max bins per packet | idem |
| 0x1C | SpectrumFftSize | FFT grootte in K | 32/64/128/256 (0=auto) |

**Bandbreedte:** Dynamisch per client. Typisch 2K bins × 15 fps = ~30 KB/s (desktop), ~8 KB/s @ 5 fps (Android).

---

## Opus Codec Configuratie

| Parameter | Waarde | Reden |
|-----------|--------|-------|
| Sample rate | 8 kHz | Narrowband, voldoende voor spraak |
| Kanalen | Mono | Eén audiokanaal |
| Applicatie | VOIP | Geoptimaliseerd voor spraak |
| Bitrate | 12.800 bps | Net boven 12.4k FEC-drempel |
| Bandbreedte | Narrowband | Past bij 8 kHz |
| Frame duur | 20 ms | 160 samples per frame |
| FEC | Aan | In-band Forward Error Correction |
| DTX | Aan | Discontinuous Transmission (stilte-onderdrukking) |
| Verwacht verlies | 10% | Optimaliseert FEC-overhead |

**Bandbreedte:** ~30 kbps per richting, ~60 kbps totaal (inclusief overhead).

---

## Resampling

Alle resampling gebruikt **rubato SincFixedIn** met sinc interpolatie voor hoge audiokwaliteit.

### Parameters

| Parameter | Waarde |
|-----------|--------|
| Sinc lengte | 128 taps |
| Cutoff frequentie | 0.95 (relatief aan Nyquist) |
| Oversampling factor | 128 |
| Interpolatie | Cubic |
| Window functie | Blackman |

### Waarom SincFixedIn (en niet FftFixedIn)

Oorspronkelijk gebruikten we `FftFixedIn`, maar dit gaf **robotachtig klinkende audio** bij 16kHz→8kHz downsampling (typisch voor USB headsets). Het probleem: FftFixedIn met slechts 320 input samples (16kHz × 20ms) heeft te weinig FFT-resolutie voor een goed anti-aliasing filter. Frequenties boven 4kHz vouwen terug als hoorbare artefacten.

`SincFixedIn` met 128-punt sinc filter en Blackman window geeft veel schonere audio, onafhankelijk van de input frame grootte.

### Resample paden

| Pad | Van | Naar | Waar |
|-----|-----|------|------|
| Server TX | Device rate (bv. 48kHz VB-Cable) | 8kHz | Server capture → Opus encoder |
| Server RX | 8kHz | Device rate (VB-Cable) | Opus decoder → Server playback |
| Client TX | Device rate (bv. 16kHz headset) | 8kHz | Mic capture → Opus encoder |
| Client RX | 8kHz | Device rate (bv. 44.1kHz headset) | Opus decoder → Speaker playback |

---

## PTT Veiligheid (4 lagen)

PTT-veiligheid is kritiek: een vastzittende zender kan schade veroorzaken aan de eindtrap en interfereert met andere gebruikers.

### Laag 1: PTT flag in elk audio packet

Elk audio packet (50x/sec) draagt de PTT-status in de flags byte. De server controleert deze bij elk ontvangen packet.

### Laag 2: Burst bij state change

Bij PTT aan/uit stuurt de client **5 kopieën** van het audio packet. Dit garandeert dat de server de state change ziet, zelfs bij packet loss.

### Laag 3: Packet timeout (500ms)

Als de server **500ms lang geen packets** ontvangt terwijl PTT actief is, wordt PTT automatisch uitgeschakeld. Log level: WARN.

### Laag 4: Heartbeat timeout (2s)

Als de server **2 seconden lang geen heartbeat** ontvangt, wordt de verbinding als verloren beschouwd en PTT wordt uitgeschakeld. Log level: ERROR (dit is een noodsituatie).

### Laag 5: PTT tail delay (150ms)

Bij PTT loslaten wacht de server **150ms** voordat `ZZTX0;` verstuurd wordt. Dit geeft de audio pipeline (jitter buffer + cpal/VB-Cable) tijd om te draineren, zodat de laatste audio niet afgeknipt wordt.

### Safety check loop

De server voert elke **100ms** een safety check uit (PTT timeouts + CAT polling) en elke **1 seconde** een sessie timeout check + CAT reconnect poging.

---

## Thetis CAT Interface

Communicatie met Thetis SDR via TCP (TS-2000 compatible protocol).

| Instelling | Waarde |
|------------|--------|
| Protocol | TCP/IP |
| Standaard adres | `127.0.0.1:13013` |
| Commando formaat | ZZ-prefix (Thetis extensie van TS-2000) |
| Verbinding timeout | 200ms bij connect poging |
| Meter poll | `ZZRM1;` (RX1) / `ZZRM2;` (RX2) / `ZZRM5;` (TX) elke 100ms |
| Full poll | `ZZFA;ZZMD;ZZTX;ZZPS;ZZTP;ZZNE;ZZNT;ZZPC;ZZLA;ZZFL;ZZFH;ZZCT;ZZFB;ZZME;ZZLB;ZZFS;ZZFR;ZZNV;ZZNU;` elke 500ms |

### Gebruikte CAT commando's

| Commando | Beschrijving | Richting |
|----------|--------------|----------|
| `ZZTX1;`/`ZZTX0;` | PTT aan/uit | Server → Thetis |
| `ZZTX;` | Lees TX status | Server → Thetis (poll) |
| `ZZAG100;` | Master AF volume op 100% | Server → Thetis (bij connect) |
| `ZZLA;` / `ZZLA{:03};` | Lees/zet RX1 AF volume (000-100) | Bidirectioneel |
| `ZZFA;` / `ZZFA{:011};` | Lees/zet VFO-A frequentie | Bidirectioneel |
| `ZZMD;` / `ZZMD{:02};` | Lees/zet operating mode | Bidirectioneel |
| `ZZRM1;` | Lees S-meter (gemiddelde signaalsterkte) | Server → Thetis (poll, RX) |
| `ZZRM5;` | Lees forward power (watts) | Server → Thetis (poll, TX) |
| `ZZPS;` / `ZZPS0;`/`ZZPS1;` | Lees/zet power aan/uit | Bidirectioneel |
| `ZZTP;` / `ZZTP{:02};` | Lees/zet TX profiel index | Bidirectioneel |
| `ZZNE;` / `ZZNE{0-4};` | Lees/zet NR level (0=uit, 1-4=NR1-NR4) | Bidirectioneel |
| `ZZNT;` / `ZZNT0;`/`ZZNT1;` | Lees/zet Auto Notch Filter | Bidirectioneel |
| `ZZPC;` / `ZZPC{:03};` | Lees/zet drive level (000-100) | Bidirectioneel |
| `ZZFL;` / `ZZFL{hz};` | Lees/stel RX1 filter low cut in (signed Hz offset van VFO) | Bidirectioneel |
| `ZZFH;` / `ZZFH{hz};` | Lees/stel RX1 filter high cut in (signed Hz offset van VFO) | Bidirectioneel |
| `ZZCT;` | Lees CTUN status (0=uit, 1=aan) | Server → Thetis (poll) |
| `ZZFB;` / `ZZFB{:011};` | Lees/zet VFO-B frequentie (RX2) | Bidirectioneel |
| `ZZME;` / `ZZME{:02};` | Lees/zet RX2 operating mode | Bidirectioneel |
| `ZZLB;` / `ZZLB{:03};` | Lees/zet RX2 AF volume (000-100) | Bidirectioneel |
| `ZZFS;` / `ZZFS{hz};` | Lees/stel RX2 filter low cut in (signed Hz offset van VFO-B) | Bidirectioneel |
| `ZZFR;` / `ZZFR{hz};` | Lees/stel RX2 filter high cut in (signed Hz offset van VFO-B) | Bidirectioneel |
| `ZZNV;` / `ZZNV{0-4};` | Lees/zet RX2 NR level (0=uit, 1-4=NR1-NR4) | Bidirectioneel |
| `ZZNU;` / `ZZNU0;`/`ZZNU1;` | Lees/zet RX2 Auto Notch Filter | Bidirectioneel |
| `ZZRM2;` | Lees RX2 S-meter (gemiddelde signaalsterkte) | Server → Thetis (poll) |

**Let op ZZNV (v0.4.1 fix):** In v0.4.0 werd per abuis `ZZNF` gebruikt voor RX2 NR. `ZZNF` is echter een RX1 commando. Het correcte commando voor RX2 NR is `ZZNV`. Dit is gecorrigeerd in v0.4.1.

---

## TCI Protocol (v0.4.0+)

TCI (Transceiver Control Interface) is een WebSocket-gebaseerd protocol ingebouwd in Thetis. Standaard op `ws://127.0.0.1:40001`.

### Verbinding

1. WebSocket connect naar Thetis TCI server
2. Wacht op `READY;` melding
3. Subscribe op sensors: `RX_SENSORS_ENABLE:true,100;`, `TX_SENSORS_ENABLE:true,100;`
4. Start audio: `AUDIO_SAMPLERATE:48000;`, `AUDIO_START:0;`, `AUDIO_START:1;`
5. Start IQ: `IQ_SAMPLERATE:{rate};`, `IQ_START:0;`

### Push-based State Updates (text messages)

| TCI notification | Beschrijving | Mapped state |
|-----------------|-------------|-------------|
| `vfo:0,0,freq;` | VFO-A frequentie | vfo_a_freq |
| `vfo:0,1,freq;` / `vfo:1,0,freq;` | VFO-B frequentie | vfo_b_freq |
| `modulation:0,mode;` | Operating mode RX1 | vfo_a_mode |
| `modulation:1,mode;` | Operating mode RX2 | vfo_b_mode |
| `trx:0,bool;` | TX actief | tx_active |
| `drive:0,val;` | Drive level | drive_level |
| `rx_filter_band:0,lo,hi;` | RX1 filter grenzen | filter_low_hz, filter_high_hz |
| `rx_filter_band:1,lo,hi;` | RX2 filter grenzen | rx2_filter_low_hz, rx2_filter_high_hz |
| `dds:0,freq;` | DDC center frequentie RX1 | dds_freq[0] |
| `dds:1,freq;` | DDC center frequentie RX2 | dds_freq[1] |

### Binary Streams

| Stream type | Header | Data |
|------------|--------|------|
| `RX_AUDIO_STREAM` | 64-byte header (receiver, sample rate) | PCM int16/int32 |
| `TX_AUDIO_STREAM` | 64-byte header | PCM int16/int32 |
| `IQ_STREAM` | 64-byte header (receiver, sample rate) | Complex float32 I/Q pairs |

### Commando's via TCI

| Actie | TCI commando |
|-------|-------------|
| PTT aan/uit | `TRX:0,true;` / `TRX:0,false;` |
| VFO-A freq | `VFO:0,0,{freq};` |
| VFO-B freq | `VFO:0,1,{freq};` |
| Mode | `MODULATION:0,{mode_str};` |
| Drive | `DRIVE:0,{val};` |
| Filter | `RX_FILTER_BAND:0,{lo},{hi};` |
| NR | `RX_NB_ENABLE:0,{bool};` |
| ANF | `RX_ANF_ENABLE:0,{bool};` |
| Tune | `TUNE:0,{bool};` |

### Commando's via parallel CAT (niet in TCI)

De volgende commando's worden via de parallelle TCP CAT verbinding gestuurd omdat TCI ze (nog) niet ondersteunt:

| Commando | Beschrijving |
|----------|-------------|
| `ZZLA` | RX1 AF volume |
| `ZZLE` | RX2 AF volume |
| `ZZBY` | Shutdown |
| `ZZCT` | CTUN status |
| `ZZPS` | Power on/off |
| `ZZTP` | TX profiel |

---

## Jitter Buffer

Adaptieve jitter buffer gebaseerd op RFC 3550 jitter-schatting.

### Werking

1. **Push:** Ontvangen packets worden gesorteerd op volgnummer in een `BTreeMap`
2. **Pull:** Frames worden in volgorde afgeleverd, één per 20ms playout tick
3. **Initialisatie:** Wacht tot `target_depth` frames gebufferd zijn voordat playout begint
4. **Missende frames:** Triggert Opus PLC (Packet Loss Concealment) voor comfort noise
5. **Late packets:** Worden verworpen als het volgnummer al gepasseerd is

### Configuratie

| Parameter | Waarde | Beschrijving |
|-----------|--------|--------------|
| min_depth | 3 frames | Minimaal 60ms buffer |
| max_depth | 20 frames | Maximaal 400ms buffer (mobiele netwerken) |
| Adaptatie | Dual-alpha EMA | 1/4 omhoog (spike), 1/16 omlaag (herstel) |
| Overflow recovery | target + 4 | Geleidelijk: max 1 frame per pull() |

### Jitter schatting (dual-alpha EMA)

```
ts_diff_ms = (packet.timestamp - last_timestamp) / 8.0    // 8kHz sample rate
arrival_diff = packet.arrival_ms - last_arrival_ms
deviation = |arrival_diff - ts_diff_ms|
alpha = if deviation > jitter_estimate { 0.25 } else { 1/16 }  // Snel omhoog, langzaam omlaag
jitter_estimate += (deviation - jitter_estimate) * alpha
target_depth = (jitter_estimate / 20.0) + 1                    // +1 frame marge
```

De dual-alpha aanpak zorgt ervoor dat de buffer onmiddellijk groeit bij een latency-spike (4G in tunnel) maar langzaam krimpt wanneer het netwerk stabiliseert. Voorkomt hakkelen zonder onnodige latency op LAN.

### Overflow recovery

Bij variabele netwerken (WiFi, 4G/5G) kunnen packets in bursts aankomen waardoor de buffer volloopt. Omdat playout rate == arrival rate (beide 50 frames/sec) draineert de buffer niet vanzelf terug naar target depth.

**Probleem:** Zonder recovery bleef de buffer op max depth hangen. `push()` dropte de oudste frames, maar `pull()` zocht precies die gedropte frames → constant Missing → Opus PLC ruis.

**Oplossing (3 onderdelen):**
1. **`pull()` geleidelijke overflow recovery:** Als `depth > target_depth + 4`, drop 1 frame per pull(). Spreidt recovery over meerdere ticks — geen hoorbare klik/stutter meer.
2. **`push()` harde limiet:** `max_depth + 10` — voorkomt extreme opbouw.
3. **`max_depth` verhoogd naar 20** (400ms) — meer ruimte voor mobiele jitter.

### Sequence wrapping

32-bit volgnummers met correcte wraparound detectie:
```rust
fn is_seq_before(a: u32, b: u32) -> bool {
    a.wrapping_sub(b) > 0x8000_0000
}
```

---

## Client UI

De client gebruikt **egui** (via eframe) voor een desktop GUI.

### Scherm indeling

```
┌─────────────────────────────────┐
│  SDR Remote                     │
├─────────────────────────────────┤
│  Server: [192.168.1.79:4580]    │
│  Status: ● Connected            │
├─────────────────────────────────┤
│         ┌─────────┐             │
│         │   PTT   │             │
│         └─────────┘             │
│    (muis of spatiebalk)         │
├─────────────────────────────────┤
│  14.345.000 Hz  S9+10 / TX 50W  │
│  Stap: [100] [1k] [10k] [100k] │
│  Mode: [LSB] [USB] [AM] [FM]   │
│  M1  M2  M3  M4  M5  [Save]    │
├─────────────────────────────────┤
│  [POWER ON]  [NR2]  [ANF] [AGC]│
│  TX Profile: [Normaal]          │
│  Drive:  ════════●══  75%       │
├─────────────────────────────────┤
│  RX Volume: ────●────── 20%     │
│  TX Gain:   ──────●──── 50%     │
├─────────────────────────────────┤
│  MIC: ████████░░░░░░ -12 dB     │
│  RX:  ██████░░░░░░░░ -18 dB     │
├─────────────────────────────────┤
│  RTT:        12 ms              │
│  Jitter:     2.3 ms             │
│  Buffer:     3 frames           │
│  RX packets: 14523              │
└─────────────────────────────────┘
```

### Thetis Controls

Alle controls zijn bidirectioneel: server pollt de huidige staat uit Thetis (elke 500ms) en broadcast naar alle clients. Client stuurt wijzigingen direct naar de server.

| Control | UI element | Gedrag |
|---------|-----------|--------|
| **Power** | Toggle knop | Groen = aan, rood = uit |
| **NR** | Cycle knop | Klik: OFF → NR1 → NR2 → NR3 → NR4 → OFF |
| **ANF** | Toggle knop | Gehighlight wanneer actief |
| **AGC** | Toggle knop | TX Automatic Gain Control aan/uit |
| **TX Profile** | Toggle knop | Wisselt tussen geconfigureerde profielen, toont naam |
| **Drive** | Slider | 0-100%, zelfde stijl als RX Volume |

### PTT bediening

- **Muisklik:** Indrukken op PTT knop = TX, loslaten (overal) = RX
- **Spatiebalk:** Ingedrukt houden = TX, loslaten = RX
- PTT knop kleurt **rood** tijdens TX

### Configuratie bestand

`sdr-remote-client.conf` naast het executable, key=value formaat:

```
server=192.168.1.79:4580
volume=0.20
tx_gain=0.50
input_device=Microphone (RODE NT-USB)
output_device=Speakers (Realtek(R) Audio)
tx_profiles=21:Normaal,25:Remote
mem1=3630000,0
mem2=7073000,0
mem3=14345000,1
websdr_favorites=https://websdr.ewi.utwente.nl:8901/,https://kiwisdr.example.com:8073/
```

| Sleutel | Beschrijving | Default |
|---------|-------------|---------|
| `server` | Server adres:poort | `127.0.0.1:4580` |
| `volume` | RX volume 0.0-1.0 | `0.2` |
| `tx_gain` | TX gain 0.0-3.0 (0-300%) | `0.5` |
| `input_device` | Microfoon device naam (exact match) | (systeem default) |
| `output_device` | Speaker device naam (exact match) | (systeem default) |
| `tx_profiles` | TX profiel index:naam paren (komma-gescheiden) | `00:Default` |
| `memN` | Geheugen N: frequentie_hz,mode (N=1-5) | leeg |
| `spectrum_enabled` | Spectrum display aan/uit | `false` |
| `websdr_favorites` | Favoriete WebSDR/KiwiSDR URL's (komma-gescheiden) | leeg |

**TX Profiles:** De index correspondeert met de positie in Thetis' TX profiel dropdown (0-based). Profielnamen zijn niet via CAT uitleesbaar, daarom handmatig geconfigureerd.

**Audio devices:** Selecteerbaar in de UI wanneer niet verbonden. Opgeslagen in config en automatisch hergebruikt bij volgende start.

Automatisch opgeslagen bij wijzigingen (volume, tx_gain, devices, memories, websdr_favorites). Backwards-compatibel met oudere versie (plain address op eerste regel).

---

## Server Opstart

```bash
sdr-remote-server [OPTIONS]

Options:
  --input <NAME>         Input device (substring match, bv. "CABLE-A")
  --output <NAME>        Output device (substring match, bv. "CABLE-B Input")
  --cat <ADDR>           Thetis CAT adres (default: 127.0.0.1:13013)
  --anan-interface <IP>  Interface IP voor HPSDR DDC/wideband capture
  --ddc-rate <kHz>       DDC sample rate in kHz (default: 1536)
  --wideband             Gebruik wideband capture (poort 1027) i.p.v. DDC I/Q
  --no-spectrum          Schakel spectrum uit (zelfs met --anan-interface)
```

**ANAN interface IP** is ook instelbaar via de server GUI (wordt opgeslagen in config).

### Voorbeelden

```bash
# TCI modus (aanbevolen, geen VB-Cable nodig)
sdr-remote-server --tci ws://127.0.0.1:40001 --cat "127.0.0.1:13013"

# Legacy modus zonder spectrum (test generator beschikbaar voor clients)
sdr-remote-server --input "CABLE-A" --output "CABLE-B Input" --cat "127.0.0.1:13013"

# Legacy modus met DDC I/Q spectrum (vereist Administrator rechten)
sdr-remote-server --input "CABLE-A" --output "CABLE-B Input" --cat "127.0.0.1:13013" --anan-interface 192.168.1.100

# Legacy modus met oud wideband spectrum (0-61.44 MHz)
sdr-remote-server --input "CABLE-A" --output "CABLE-B Input" --anan-interface 192.168.1.100 --wideband
```

De server logt alle beschikbare audio devices bij opstart voor eenvoudige device selectie.

**Let op:** `"CABLE-B"` kan matchen met "CABLE-B In 16ch" (16-kanaals variant). Gebruik `"CABLE-B Input"` voor het 2-kanaals device.

---

## Sessie Management

De server ondersteunt **meerdere gelijktijdige clients** met single-TX arbitratie.

### Gedrag

- Meerdere clients kunnen tegelijk verbinden en RX audio ontvangen
- TX (PTT) is first-come-first-served: eerste client die PTT indrukt krijgt de TX lock
- Andere clients ontvangen een `PttDenied` packet als ze TX proberen terwijl de lock bezet is
- Bij disconnect of timeout: TX lock wordt vrijgegeven
- Bij nieuwe client: jitter buffer en Opus decoder worden gereset
- Session timeout: **15 seconden** zonder activiteit (mobiele resilience)
- Expliciete disconnect via Disconnect packet

### TouchResult

```rust
pub enum TouchResult {
    Existing,   // Bekende client, last_seen bijgewerkt
    NewClient,  // Nieuwe client → reset nodig
}
```

---

## Verbindingsbeheer

### Connect flow

1. Client stuurt eerste Heartbeat naar server adres
2. Server registreert client sessie (`TouchResult::NewClient`)
3. Server reset jitter buffer + decoder + stuurt `ZZAG100;` (Master AF)
4. Server antwoordt met HeartbeatAck
5. Client markeert verbinding als "Connected" bij ontvangst HeartbeatAck

### Disconnect flow (netjes)

1. Client stuurt Disconnect packet naar server
2. Server logt "Client X disconnected" en verwijdert sessie
3. Bij server shutdown: stuurt Disconnect naar actieve client

### Disconnect flow (timeout)

1. Client ontvangt geen HeartbeatAck EN geen audio pakketten langer dan **max(6s, rtt×8)**
2. Client markeert als "Disconnected" maar reset jitter buffer NIET — buffer draineert via PLC zodat audio vloeiend hervat als pakketten terugkomen
3. Server merkt timeout na **15 seconden** en verwijdert sessie

### Packet loss tracking

Client berekent loss per heartbeat-window (500ms) met EMA smoothing (α=0.3):
```
raw_loss = (expected_packets - received_packets) / expected_packets × 100
smoothed_loss = smoothed_loss × 0.7 + raw_loss × 0.3
```
Wordt meegestuurd in het Heartbeat packet. Server gebruikt dit voor spectrum throttling.

### Spectrum throttling bij packet loss

Server past spectrum FPS per client aan op basis van gerapporteerde loss:
- **0-5% loss:** Normale FPS
- **5-15% loss:** Halve FPS (skip every other frame)
- **>15% loss:** Spectrum gepauzeerd — audio heeft prioriteit

---

## Audio Pipeline Details

### Ring buffers

| Buffer | Capaciteit | Doel |
|--------|-----------|------|
| Capture | 2s @ device rate | Mic/VB-Cable → netwerk thread |
| Playback | 2s @ device rate | Netwerk thread → speaker/VB-Cable |

Lock-free SPSC (Single Producer Single Consumer) ring buffers scheiden de cpal audio callback thread van de tokio netwerk thread.

### Capture processing (client)

1. cpal callback vangt audio op van microfoon
2. Multi-kanaal → mono (eerste kanaal)
3. Push naar capture ring buffer
4. Netwerk loop (20ms tick): pop naar accumulatiebuffer
5. Verwerk complete frames: resample → AGC (optioneel) → TX gain → Opus encode → UDP send
6. Onvolledige frames blijven in accumulatiebuffer voor volgende tick

**Belangrijk:** De accumulatiebuffer voorkomt sample verlies. Eerdere implementatie met `pop_slice` op een grote buffer verloor het restant na elke frame, wat brokkelige audio veroorzaakte.

### Playout processing (server)

1. UDP packets ontvangen → push naar jitter buffer (alleen opslaan, niet afspelen)
2. Playout timer (elke 20ms): trek 1 frame uit jitter buffer
3. Opus decode → resample 8kHz → device rate → push naar playback ring buffer
4. cpal callback leest uit playback ring buffer

**Belangrijk:** De playout timer is gescheiden van packet ontvangst. Eerdere implementatie speelde frames direct af bij ontvangst, wat burst-achtige audio veroorzaakte.

### VB-Cable keepalive

De server's playback callback stuurt `NEAR_ZERO` (1e-7) in plaats van echte stilte (0.0) als de playback buffer leeg is. Dit voorkomt dat VB-Cable in slaapstand gaat.

---

## Bekende Problemen en Oplossingen

### 1. Geen modulatie na client herstart

**Probleem:** Na client herstart begint de client met sequence 0, maar de server's jitter buffer verwacht nog de oude sequence nummers (bv. 50000+). Alle nieuwe packets worden als "te laat" verworpen.

**Oplossing:** `SessionManager::touch()` retourneert `TouchResult::NewClient` bij nieuwe/vervangende client. De server reset dan de jitter buffer en maakt een nieuwe Opus decoder aan.

### 2. Robotachtige TX audio

**Probleem:** `FftFixedIn` resampler met 320 input samples (16kHz headset) heeft te weinig FFT-resolutie voor goed anti-aliasing filter. Frequenties boven 4kHz vouwen terug als hoorbare artefacten.

**Oplossing:** Overstap naar `SincFixedIn` met 128-punt sinc filter en Blackman window.

### 3. Brokkelige TX audio (sample verlies)

**Probleem:** Client's `pop_slice` op een grote drain buffer verliest het restant na elke frame verwerking. Bij een tick met 1500 samples en frames van 960: 540 samples weg.

**Oplossing:** Accumulatiebuffer die onverwerkte samples bewaart voor de volgende tick.

### 4. Server burst playout

**Probleem:** Server speelde alle beschikbare frames uit de jitter buffer af zodra een packet binnenkwam, in plaats van regelmatig elke 20ms. Dit veroorzaakte burst-achtig geluid.

**Oplossing:** Gescheiden 20ms playout timer die één frame per tick uit de jitter buffer trekt.

### 5. VB-Cable slaapstand

**Probleem:** Na client herstart stopt VB-Cable met doorsturen van audio (gaat in slaapstand bij stilte).

**Oplossing:** `NEAR_ZERO` (1e-7) in de cpal playback callback in plaats van 0.0.

### 6. Verkeerd VB-Cable device geselecteerd

**Probleem:** `--output "CABLE-B"` matchte met "CABLE-B In 16ch" (16-kanaals) in plaats van "CABLE-B Input" (2-kanaals).

**Oplossing:** Gebruik specifiekere substring: `--output "CABLE-B Input"`.

### 7. Client disconnect zonder UI-knop

**Probleem:** Als de client afgesloten wordt zonder op Disconnect te klikken, kreeg de server geen melding.

**Oplossing:** Client stuurt Disconnect packet in de shutdown handler (bij window close).

### 8. RX Volume push bij connect

**Probleem:** Bij connect stuurde de client direct zijn lokale RX volume naar Thetis, waardoor de instelling in Thetis overschreven werd.

**Oplossing:** `rx_volume_synced` flag — client stuurt pas ZZLA nadat de eerste waarde van de server is ontvangen. Bij disconnect wordt de flag gereset zodat reconnect opnieuw synchroniseert.

### 9. Slider sync na disconnect/reconnect

**Probleem:** Na disconnect en reconnect synchroniseerde de RX Volume slider niet meer met de server waarde, omdat `state.rx_af_gain` niet gereset werd en de `LaunchedEffect` key niet veranderde.

**Oplossing:** `state.rx_af_gain = 0` op alle 4 disconnect paden (Command::Disconnect, external disconnect, server Disconnect packet, connection timeout).

### 10. Android microfoon te zacht

**Probleem:** `InputPreset::VoiceCommunication` past AGC en noise suppression toe, waardoor het mic signaal sterk gedempt wordt.

**Oplossing:** `InputPreset::VoiceRecognition` voor raw mic input zonder processing. Aanvullend: TX gain range vergroot van 0-100% naar 0-300% voor extra boost indien nodig.

### 11. RX2 NR commando werkte niet (v0.4.1 fix)

**Probleem:** `ZZNF` werd gebruikt voor RX2 NR, maar dit is een RX1 commando. RX2 NR veranderde niet via de ThetisLink UI.

**Oplossing:** Gecorrigeerd naar `ZZNV` voor RX2 NR level.

---

## Ontwikkelhistorie

| Fase | Beschrijving |
|------|-------------|
| Fase 0 | Core hardening: jitter buffer, codec, resampling, PTT safety |
| Fase 1 | Architectuur refactor: business logic gescheiden van UI/platform. `sdr-remote-logic` crate, `CatConnection` extractie, watch/mpsc channels, `AudioBackend` trait |
| Fase 2 | Android client: `sdr-remote-android` crate, UniFFI bridge, Oboe audio, Jetpack Compose UI. ZZLA bidirectionele sync, mic sensitivity fix, TX gain boost, TX power indicator, jitter buffer overflow recovery, TX AGC |
| Fase 3 | Wideband spectrum: SpectrumPacket protocol, HPSDR wideband capture (raw socket SIO_RCVALL), 16k-punt FFT (rustfft), EMA smoothing, test generator. Desktop egui spectrum plot + waterfall + click-to-tune. Android SpectrumView + WaterfallView. Per-client spectrum enable/fps. |
| Fase 3b | DDC I/Q spectrum: Wideband vervangen door DDC capture (poort 1035+, 24-bit I/Q). 4096-punt complex FFT + FFT-shift. Spectrum gecentreerd op VFO (non-CTUN). Max-per-pixel client rendering. Scroll/drag/click-to-tune. Zoom/pan/contrast sliders. Frequency bounce fix (engine pending_freq). |
| v0.1.0 | ThetisLink branding + netwerk-robuustheid: dual-alpha jitter adaptatie, geleidelijke overflow recovery, dynamische timeout (HB+audio), spectrum throttling bij loss, session timeout 15s, loss EMA smoothing. Gedeeld versienummer in core. |
| v0.3.0 | Volledige RX2/VFO-B support: dual-input audio (aparte VAC), DDC3 spectrum+waterfall, joined/split popout windows, RX2 CAT (ZZFB/ZZME/ZZFS/ZZFR/ZZNF/ZZNU) |
| v0.4.0 | TCI WebSocket integratie: vervangt VB-Cable + raw socket als primaire verbindingsmodus. Waterfall click-to-tune Android. Geen Administrator-rechten of VB-Cable nodig in TCI modus. |
| v0.4.1 | Embedded WebSDR/KiwiSDR WebView (wry), TX spectrum auto-override bij zenden, range slider uitgebreid naar 130dB, RX2 NR CAT fix (ZZNF→ZZNV) |
| v0.4.2 | Power state fix: engine als single source of truth (Android+desktop identiek gedrag), power suppress timer voorkomt race condition bij Thetis shutdown, dead code cleanup server (cat/tci/ptt), TCI WS timeout hersteld naar 500ms |

---

## RX Volume (ZZLA) Synchronisatie

De RX Volume (RX1 AF Gain) is bidirectioneel gesynchroniseerd tussen Thetis en alle clients:

1. **Server → Client:** Server pollt `ZZLA;` elke 500ms → broadcast als `ControlPacket(Rx1AfGain, waarde)` → client engine ontvangt → update `state.rx_af_gain` + interne `rx_volume` → UI slider synchroniseert
2. **Client → Server:** Slider wijziging → `Command::SetRxVolume` → engine stuurt `ControlPacket(Rx1AfGain, waarde)` → server stuurt `ZZLA{:03};` naar Thetis

**Sync protocol:** Client stuurt pas ZZLA naar server nadat de eerste waarde van de server is ontvangen (`rx_volume_synced` flag). Dit voorkomt dat de client zijn lokale waarde naar Thetis pusht bij connect. Bij disconnect wordt de flag gereset.

## S-meter / TX Power Indicator

De meter bar is context-afhankelijk op basis van PTT status:

**RX (ontvangst):** S-meter met 0-260 schaal (12 per S-unit, S9=108). Groen tot S9, rood boven S9. Server pollt `ZZRM1;` (gemiddelde signaalsterkte) en stuurt via SmeterPacket.

**TX (zenden):** Forward power bar met 0-100W schaal. Volledig rood. Server pollt `ZZRM5;` (forward power in watts) en stuurt via hetzelfde SmeterPacket (watts × 10 als u16). Client interpreteert op basis van lokale PTT status.

**Backwards compatibel:** Oude server stuurt S-meter waarden tijdens TX → client toont onzinnige watts (onschadelijk). Nieuwe server stuurt echte forward power.

## TX Automatic Gain Control (AGC)

Schakelbare client-side AGC in het TX audiopad. Normaliseert mic-niveau zodat zacht en luid spreken consistent TX-vermogen oplevert.

### Signaalpad

```
Mic capture → resample 8kHz → [AGC] → TX gain → Opus encode → UDP
```

AGC zit vóór TX gain zodat de gain slider als extra boost/demping werkt bovenop het genormaliseerde signaal.

### Algoritme

Peak-based envelope follower met noise gate:

| Parameter | Waarde | Beschrijving |
|-----------|--------|--------------|
| Target | -12 dB (0.25) | Gewenst piekniveau na AGC |
| Max gain | +20 dB (10×) | Maximale versterking |
| Min gain | -20 dB (0.1×) | Minimale versterking |
| Attack | 0.3 | Snel reageren op luide pieken |
| Release | 0.01 | Langzaam terugkeren naar hogere gain |
| Noise gate | -60 dB (0.001) | Geen gain boost bij stilte |

### Werking per frame (20ms)

1. Bepaal piekwaarde van het frame
2. Update peak envelope: `env += (peak - env) × coeff` (attack als peak > env, release anders)
3. Als envelope > noise gate: bereken gewenste gain = target / envelope, clamp naar min/max
4. Vermenigvuldig alle samples met gain

### UI

Toggle knop "AGC" naast ANF in beide clients. Blauw/gehighlight = aan, grijs = uit. State wordt via `RadioState.agc_enabled` teruggelezen zodat de UI altijd de actuele staat toont.

---

## Spectrum/Waterfall Display (Fase 3 + 3b)

### Architectuur

De server vangt passief HPSDR Protocol 2 DDC I/Q data af via Windows raw sockets (SIO_RCVALL). De ANAN stuurt DDC I/Q pakketten naar Thetis — de server luistert mee zonder het verkeer te onderbreken.

**DDC I/Q modus (standaard):** 262k-punt FFT gecentreerd op VFO frequentie (5.859 Hz/bin resolutie). Server-side zoom/pan extractie stuurt max 8192 bins per client. Bij zoom 32x: Thetis-kwaliteit detail.

**Wideband modus (legacy):** 0–61.44 MHz overzicht via ADC wideband data (poort 1027). Te breed voor praktisch gebruik; beschikbaar via `--wideband` flag.

```
DDC I/Q pipeline (standaard):
  Raw socket recv → IP/UDP parse → filter src port 1035-1040
  → Auto-detect en lock op eerste DDC port (typisch 1037 = DDC2 = RX1)
  → 24-bit big-endian I/Q pairs (3+3 bytes per paar, Q genegeerd voor correcte oriëntatie)
  → Accumulatie tot 262144 I/Q samples (50% overlap → ~11.7 FPS)
  → Blackman-Harris window (262144 punt)
  → Complex FFT forward (rustfft, 262144 punt, ~6-15ms)
  → FFT-shift (DC naar centrum: bin 0 = -fs/2, bin 131072 = DC/VFO, bin 262143 = +fs/2)
  → |c|² normalisatie (÷ N² voor rustfft unnormalized output)
  → dB schaal → 0-255 mapping (-150 dB → 0, -30 dB → 255)
  → EMA smoothing (α=0.4, 262144 bins)
  → Per-client extract_view(zoom, pan) → max 8192 bins
  │     └── Float stride decimatie: stride = visible/max_bins (f64)
  │         Voorkomt integer afrondingsoffset bij non-power-of-2 zoom
  → Rate limit (fps per client)
  → SpectrumPacket (≤8192 bins, center_freq_hz, span_hz) → clients
```

### Server-Side Zoom/Pan

Elke client heeft eigen zoom/pan state op de server. De 262k smoothed buffer wordt eenmalig per FFT frame berekend; per client wordt een view geëxtraheerd:

- **extract_view(zoom, pan, max_bins)**: selecteert visible bins, decimeert naar max 8192
- **Float stride**: `stride = visible as f64 / max_bins as f64` — dekt volledige zichtbare range af
- **Max-per-group**: behoud signaalpieken bij decimatie
- **center_freq_hz + span_hz**: per-view frequentie metadata in Hz-precisie

### Frequency Tracking

- Server pollt VFO frequentie via CAT (ZZFA) elke 500ms
- `center_freq_hz` in SpectrumPacket = Hz-precisie (was kHz, geüpgraded)
- VFO-A als DDC center (NCO sniffing verwijderd — onbetrouwbaar via RCVALL)
- DDC data van de ANAN volgt automatisch de VFO (non-CTUN modus)
- Band change detectie: buffer reset bij sprong > sample_rate/4

### VFO Marker Stabiliteit

- Client `pending_freq` tracking voorkomt VFO marker bounce bij tuning
- `pending_freq` wordt pas gecleard als spectrum center binnen 500 Hz van pending waarde
- Tijdens pending: VFO marker vastgepind op display center
- `tune_base_hz`: scroll-tuning accumuleert op werkelijke VFO (niet display-VFO)

### Test Spectrum Generator

Zonder `--anan-interface` genereert de server gesimuleerde data:
- **DDC test:** Signalen rond VFO (±10 kHz, ±30 kHz, ±50 kHz, ±65 kHz) + noise floor
- **Wideband test:** Signalen op ham banden (160m–6m) + broadcast interferentie

### Desktop Client

- **Spectrum plot** (150px): cyan spectrumlijn, max-per-pixel aggregatie (8192 bins → pixels)
- **Waterfall** (150px): scrollende textuur met ring buffer, contrast instelling
- **VFO marker**: rode verticale lijn, stabiel bij tuning (pending_freq pinning)
- **Filter passband**: grijze achtergrond + gele randlijnen, ZZFL/ZZFH signed Hz offsets (LSB negatief, USB positief)
- **Band markers**: alleen zichtbaar als band binnen display bereik valt
- **Band highlight**: memory-knoppen kleuren blauw bij actieve band
- **Dynamische freq-as**: tick spacing past zich aan aan zoom niveau
- **Scroll-to-tune**: scroll wheel in spectrum of waterfall = ±1 kHz, responsive accumulatie
- **Drag-to-tune**: klik en sleep in spectrum = VFO volgt muis (100 Hz snap)
- **Click-to-tune**: enkele klik op spectrum → VFO verplaatst (1 kHz snap)
- **Zoom/Pan sliders**: server-side zoom (1x–32x), pan met 100ms debounce
- **Ref level (-80..0 dB) / range (20..130 dB) sliders**: instelbaar display bereik (range uitgebreid van 90dB naar 130dB in v0.4.1; met PA beschikbaar tot 120dB)
- **Waterfall contrast slider**: power curve aanpassing
- **Colormap**: zwart → blauw → cyan → geel → rood → wit (5-punt lineair)
- **Toggle**: "Spectrum" knop schakelt display en server stream

### TX Spectrum Auto-Override (v0.4.1)

Bij het indrukken van PTT overschrijft de server automatisch de spectrum weergave-instellingen voor optimale TX-monitoring:

| Parameter | RX (normaal) | TX (override) |
|-----------|-------------|---------------|
| Ref level | Gebruikersinstelling | -30 dBm |
| Range | Gebruikersinstelling | 100 dB (120 dB met PA actief) |

Bij PTT loslaten worden de originele RX-instellingen automatisch hersteld. Dit geeft een optimaal beeld van het uitgezonden signaal zonder dat de gebruiker handmatig hoeft bij te stellen.

### Android Client

- **SpectrumPlot**: Compose Canvas, 120dp hoog
- **WaterfallView**: Bitmap ring buffer, 100dp hoog
- **FPS**: 5 fps standaard (bespaart 4G bandbreedte)
- **Toggle**: Button in MainScreen
- **Click-to-tune**: tik op spectrum → VFO verplaatst

### HPSDR DDC Capture Details

- **Socket**: `SOCK_RAW` + `IPPROTO_IP` + `SIO_RCVALL` (promiscuous mode)
- **Vereist**: Administrator rechten (Windows UAC)
- **Port filtering**: Source ports 1035-1040 (DDC I/Q data van ANAN)
- **Port lock**: Eerste DDC port gezien wordt gelocked (voorkomt menging van DDC receivers)
- **Auto-detect**: Eerste bron IP wordt als ANAN herkend
- **Packet formaat**: 16-byte header (seq + timestamp + bits_per_sample + samples_per_frame) + I/Q data
- **Sample formaat**: 24-bit big-endian signed, 238 samples per HPSDR packet
- **Q negatie**: `-q_val` corrigeert HPSDR I/Q conventie (voorkomt gespiegeld spectrum)
- **50% overlap**: Na frame verzenden, drain eerste helft (131072 samples), behoud tweede helft
- **Frame assembly**: Accumulatie tot DDC_FFT_SIZE (262144) I/Q samples
- **Thread**: Dedicated OS thread (blocking socket, niet tokio)
- **Communicatie**: `std::sync::mpsc` channel naar tokio task

---

## Embedded WebSDR/KiwiSDR WebView (v0.4.1)

### Overzicht

De desktop client biedt een geïntegreerd WebView-venster waarmee websdr.org- en KiwiSDR-ontvangers direct vanuit ThetisLink te bedienen zijn. Het venster draait via `wry` (Win32 + WebView2) in een apart OS-venster naast het hoofdvenster.

### Functionaliteit

- **URL-invoer**: Directe invoer van WebSDR/KiwiSDR URL in het venster
- **Auto-detectie**: Het SDR-type (websdr.org of KiwiSDR) wordt automatisch herkend aan de hand van de URL en paginainhoud
- **Favorieten**: Persistente favorietenlijst opgeslagen in `sdr-remote-client.conf` onder `websdr_favorites` (komma-gescheiden URL's)
- **Frequentiesync**: Tuning in ThetisLink stuurt (debounced, 500ms) de frequentie door naar het WebSDR/KiwiSDR venster via JavaScript-injectie
- **Spectrum zoom**: Bij openen wordt het WebSDR-spectrum automatisch naar maximale zoom ingesteld
- **TX mute (JS, instant)**: Bij PTT-indrukken wordt het WebSDR-geluid onmiddellijk gedempt via JavaScript (geen netwerkvertraging). Bij PTT loslaten wordt het geluid hersteld.

### Architectuur

```
sdr-remote-client/
├── websdr.rs     — Win32 venster aanmaken, wry WebView instantie, event loop
└── catsync.rs    — Communicatiekanaal tussen engine en WebView:
                     frequentie-updates (debounced 500ms),
                     PTT-status (voor JS mute),
                     favorites beheer (lezen/schrijven config)
```

**WebView ↔ UI communicatie:**
- `catsync` ontvangt frequentie- en PTT-events van de `ClientEngine` via een intern kanaal
- Frequentie-updates worden gedebouncet (500ms) zodat snelle VFO-draaiing niet elke stap naar het WebSDR stuurt
- JavaScript-injectie zorgt voor instant mute bij TX — geen vertraging via het netwerk

### JavaScript Injectie

**websdr.org tune:**
```javascript
setfreq({freq_khz}, 'usb');  // of 'lsb', 'am', 'fm' op basis van actieve mode
```

**KiwiSDR tune:**
```javascript
kiwisdr_setfreq({freq_khz});
```

**Mute bij TX:**
```javascript
// Mute
document.querySelectorAll('audio, video').forEach(el => { el.muted = true; });
// Unmute
document.querySelectorAll('audio, video').forEach(el => { el.muted = false; });
```

### UI

- Knop "WebSDR" in het hoofdvenster opent/sluit het WebView-venster
- URL-balk bovenaan het venster met dropdown voor favorieten
- "Toevoegen aan favorieten" knop slaat de huidige URL op en schrijft naar config

---

## Externe Apparaten

ThetisLink ondersteunt 6 externe apparaten via de server. Status wordt uitgelezen en doorgestuurd naar alle verbonden clients. Bediening is mogelijk vanuit zowel de server UI als de desktop/Android clients.

Elk apparaat kan in de server settings individueel worden in-/uitgeschakeld. Uitgeschakelde apparaten behouden hun configuratie (COM poort / IP adres).

### Equipment Protocol (ThetisLink intern)

Externe apparaten delen een generiek Equipment protocol over de ThetisLink UDP-verbinding:

| Packet | Type ID | Richting | Frequentie |
|--------|---------|----------|-----------|
| EquipmentStatus | 0x0E | Server → Client | Elke 200ms |
| EquipmentCommand | 0x0F | Client → Server | Bij gebruikersactie |

**EquipmentStatusPacket:**
| Veld | Type | Beschrijving |
|------|------|-------------|
| device_type | u8 | DeviceType enum (zie onder) |
| switch_a | u8 | Apparaat-specifiek |
| switch_b | u8 | Apparaat-specifiek |
| connected | bool | Hardware online |
| labels | Option\<String\> | Komma-gescheiden extra data |

**EquipmentCommandPacket:**
| Veld | Type | Beschrijving |
|------|------|-------------|
| device_type | u8 | Doelapparaat |
| command_id | u8 | Commando (apparaat-specifiek) |
| data | Vec\<u8\> | Parameters (variabele lengte) |

**DeviceType enum:**
| Waarde | Apparaat |
|--------|----------|
| 0x01 | Amplitec 6/2 |
| 0x02 | JC-4s Tuner |
| 0x03 | SPE Expert |
| 0x04 | RF2K-S |
| 0x05 | UltraBeam |
| 0x06 | Rotor |

---

### Amplitec 6/2 Antenneschakelaar

6-poorts coax switch met twee onafhankelijke schakelaars (A en B).

**Verbinding:** Serieel USB-TTL, 9600 baud, 8N1. Configuratie: COM poort selecteren in server GUI.

**Hardware protocol (server ↔ Amplitec):**
- Server stuurt positie-commando's als ASCII bytes via seriële poort
- Amplitec bevestigt met huidige positie
- Polling: server leest positie elke 200ms

**ThetisLink protocol:**
- **Status:** switch_a = positie A (1-6), switch_b = positie B (1-6), labels = CSV met positie-namen
- **Commands:** SetSwitchA (0x01, value=positie 1-6), SetSwitchB (0x02, value=positie 1-6)

**UI:** Server venster + tabblad in desktop/Android clients met knoppen per positie.

---

### JC-4s Antenna Tuner

Automatische antennetuner. Gebouwd met een custom USB-serieel interface (zie `FASEPLAN.md` sectie "Fase 5a" voor de bouwhandleiding).

**Verbinding:** Serieel USB, 9600 baud, 8N1. Configuratie: COM poort selecteren in server GUI.

**Hardware protocol (server ↔ JC-4s):**
- Tune start: server stuurt `T` byte
- Status polling: server leest status byte (0x00=idle, 0x01=tuning, 0x02=done)
- Tune abort: server stuurt `A` byte
- Polling interval: 200ms

**ThetisLink protocol:**
- **Status:** switch_a = tuner_state (0-4), switch_b = can_tune (0/1), connected = online
- **Commands:** CMD_TUNE_START (0x01), CMD_TUNE_ABORT (0x02)

**Tuner states:**
| State | Waarde | Beschrijving | Kleur in UI |
|-------|--------|-------------|------------|
| Idle | 0 | Geen tune actief | Grijs |
| Tuning | 1 | Tune bezig | Blauw |
| DoneOk | 2 | Tune succesvol | Groen |
| Timeout | 3 | Geen antwoord binnen 30s | Oranje (3s, dan Idle) |
| Aborted | 4 | Gebruiker afgebroken | Oranje (3s, dan Idle) |

**Drive bescherming:** Tijdens tuning wordt de drive automatisch verlaagd naar een veilig niveau.
**Stale detectie:** DoneOk wordt grijs als de VFO-frequentie meer dan 25 kHz is verschoven t.o.v. de laatst succesvolle tune.

---

### SPE Expert 1.3K-FA Eindversterker

Lineaire eindversterker, 1300W.

**Verbinding:** Serieel USB, 115200 baud, 8N1. Configuratie: COM poort selecteren in server GUI.

**Hardware protocol (server ↔ SPE Expert):**

De SPE Expert gebruikt een eigen binair serieel protocol:
- **Request frame:** `0xAA 0xAA` (preamble) + command byte + data bytes + checksum
- **Response frame:** `0xAA 0xAA` + response type + data bytes + checksum
- Checksum: XOR van alle bytes na preamble

| Command | Byte | Beschrijving |
|---------|------|-------------|
| Status query | 0x00 | Vraag volledige status op |
| Operate | 0x01 | Schakel naar Operate modus |
| Standby | 0x02 | Schakel naar Standby |
| Tune | 0x03 | Start tune cyclus |
| Antenna | 0x04 | Wissel antenne |
| Input | 0x05 | Wissel input |
| Power level | 0x06 | Wissel power level (L/M/H) |
| Band up/down | 0x07/0x08 | Band schakelen |
| Power on/off | 0x09/0x0A | Aan/uit |

Status response bevat: band, antenne, input, power level, forward power, reflected power, SWR, temperatuur, spanning, stroom, alarm/warning flags, ATU bypass status.

**ThetisLink protocol:**
- **Status labels CSV:** alle telemetriewaarden als komma-gescheiden string
- **Commands:** Operate, Tune, Antenna, Input, Power, BandUp, BandDown, Off, PowerOn, DriveDown, DriveUp

**UI features:**
- Power bar met peak-hold en automatische schaal (L=500W, M=1000W, H=1500W)
- Telemetrie: vermogen, SWR, temperatuur, spanning, stroom
- Server venster met optioneel protocol log

---

### RF2K-S Eindversterker

RF2K-S (RFKIT) solid-state eindversterker.

**Verbinding:** TCP/IP. Configuratie: IP:poort invoeren in server GUI (bijv. `192.168.1.50:8080`).

**Hardware protocol (server ↔ RF2K-S):**

De RF2K-S communiceert via UDP. De server verbindt met de RF2K-S controller via TCP socket:
- **Status polling:** Server stuurt status query, RF2K-S antwoordt met binair status frame
- **Commands:** Operate on/off, tune, antenna selectie (1-4 + ext), error reset, drive up/down
- Status frame bevat: band, frequentie, temperatuur, spanning, stroom, forward/reflected power, SWR, error state, antenne type/nummer, tuner mode/setup, max power, device name

**ThetisLink protocol:**
- **Status labels CSV:** alle telemetriewaarden gecodeerd in labels string
- **Commands:** rf2k_operate(bool), rf2k_tune, rf2k_ant1..4/ext, rf2k_error_reset, rf2k_close, rf2k_drive_up/down, rf2k_tuner_mode/bypass/reset/store/l_up/l_down/c_up/c_down/k

**UI features:**
- Band en frequentie display
- Forward/reflected power met SWR
- Temperatuur, spanning, stroom
- Antenna selectie (4 + ext)
- Ingebouwde tuner bediening (L/C/K, bypass, store, reset)
- Error status met reset knop

---

### UltraBeam RCU-06 Antennecontroller

Controller voor UltraBeam stuurbare Yagi-antenne. Bestuurt elementlengtes via stappenmotoren.

**Verbinding:** Serieel USB, 19200 baud, 8N1. Configuratie: COM poort selecteren in server GUI.

**Hardware protocol (server ↔ UltraBeam RCU-06):**

Eigen binair protocol via RS-232:
- **Request frame:** `0x55` (sync) + command byte + data + checksum
- **Response frame:** `0x55` + response + data + checksum

| Command | Beschrijving |
|---------|-------------|
| Status query | Vraag huidige status op (band, richting, motoren, elementen) |
| Set frequency | Stel frequentie in (kHz) + richting (forward/reverse) |
| Retract | Trek alle elementen in (transportstand) |
| Read elements | Lees elementposities uit (mm) |

Frequentiestappen: 25 kHz en 100 kHz (niet 10/100).

Status response bevat: frequentie (kHz), band, richting (forward/reverse/unknown), off-state, motoren actief, motor completion %, firmware versie, elementlengtes in mm.

**ThetisLink protocol:**
- **Status labels CSV:** alle waarden gecodeerd in labels string
- **Commands:** ub_set_frequency(khz, direction), ub_retract, ub_read_elements

**UI features:**
- Frequentie en band display
- Richting indicator (forward/reverse)
- Motor status (moving/complete %)
- Elementlengte display per element (mm)
- Retract knop

---

### EA7HG Visual Rotor

Rotor controller voor draaibare antennes. Gebaseerd op Arduino Mega 2560 met W5100 LAN module. Bestuurt de rotor via relays (CW/CCW) en leest de positie via een weerstandsspanning (potentiometer).

**Verbinding:** UDP, poort 2570. Configuratie: IP:poort invoeren in server GUI (bijv. `192.168.1.66:2570`).

**Hardware protocol (server ↔ Visual Rotor Arduino):**

Gebaseerd op het Prosistel protocol, maar via UDP in plaats van serieel. De Arduino firmware accepteert UDP-pakketten en antwoordt op hetzelfde adres/poort.

**Belangrijk:** Geen STX (0x02) prefix bij het versturen! De Arduino accepteert het commando direct gevolgd door CR (0x0D). De response van de Arduino bevat wél een STX prefix.

| Richting | Frame formaat |
|----------|--------------|
| Request (server → Arduino) | `command + CR (0x0D)` |
| Response (Arduino → server) | `STX (0x02) + data + CR (0x0D)` |

**Commando's:**

| Commando | Bytes | Beschrijving |
|----------|-------|-------------|
| `AA?` + CR | Positie query | Antwoord: `A,?,<hoek>,<status>` |
| `AAG<nnn>` + CR | GoTo hoek | nnn = 000-360 (graden, integer) |
| `AAG999` + CR | Stop | Noodstop, stopt rotatie onmiddellijk |

**Response formaat:** `STX A,?,<angle>,<status> CR`
- `angle`: huidige hoek als integer (0-360)
- `status`: `R` = Ready (stilstand), `B` = Busy (draait)

**Voorbeeld communicatie:**
```
Send: AA?\r        → Response: \x02A,?,172,R\r     (172°, stilstand)
Send: AAG090\r     → Response: \x02A,?,172,B\r     (draait naar 90°)
Send: AA?\r        → Response: \x02A,?,135,B\r     (onderweg, 135°)
Send: AA?\r        → Response: \x02A,?,090,R\r     (aangekomen, 90°)
Send: AAG999\r     → (stop, geen response verwacht)
```

**Let op:** CW/CCW commando's (`AAR`, `AAL`) worden NIET ondersteund door deze Arduino firmware. CW/CCW is geïmplementeerd als GoTo naar huidige positie ±5°.

**Polling:** Server stuurt elke 200ms een `AA?` query. Als er 3 seconden geen response komt, wordt de rotor als offline gemarkeerd.

**ThetisLink protocol:**
- **Status labels CSV:** `angle_x10,rotating,target_x10` (hoek in tienden van graden, 0-3600)
- **Commands:** CMD_ROTOR_GOTO (0x01, data=angle_x10 LE u16), CMD_ROTOR_STOP (0x02), CMD_ROTOR_CW (0x03), CMD_ROTOR_CCW (0x04)

**UI:** Klikbare kompas-cirkel met:
- Groene naald: huidige positie
- Gele lijn: doelpositie tijdens rotatie
- N/E/S/W labels met 30° tick marks
- Klik in de cirkel om naar die hoek te draaien
- STOP knop + GoTo tekstveld
- Server venster schaalt mee met venstergrootte

---

## Yaesu FT-991A Integratie (v0.4.5+)

### Overzicht

De FT-991A wordt via USB serieel (CP210x) en USB Audio CODEC aangestuurd. De server communiceert via CAT ASCII-commando's (38400 baud, 8N1, hardware flow control RTS/CTS) op de Enhanced COM Port.

### Audio paden

| Pad | Richting | Codec | Sample rate | Bandbreedte |
|-----|----------|-------|-------------|-------------|
| Yaesu RX | 991A USB → server → client | Opus narrowband | 8 kHz | ~13 kbps |
| Yaesu TX | Client mic → server → 991A USB | Opus wideband | 16 kHz | ~24 kbps |

**FM → DATA-FM transparantie:** Bij Yaesu PTT schakelt de server automatisch van FM naar DATA-FM (nodig voor USB mic) en herstelt FM na TX. De gebruiker ziet alleen FM in alle lijsten.

### Memory Channel Editor

De client leest/schrijft geheugenkanalen via het **MT commando** (Memory Tag):
- **Lezen:** `MT{nnn};` → response met freq, mode, tone, shift + 12-char naam
- **Schrijven:** `MT{nnn}{freq}{clar}{mode}0{tone}{tonenum}{shift}0{TAG12};`
- **Recall:** `MC{nnn};` schakelt naar memory mode + selecteert kanaal
- Bij recall via ThetisLink blijft de radio in memory mode (scannen werkt)

Velden per kanaal: naam, RX freq, TX freq (berekend uit offset), mode, offset richting, offset freq, tone mode, CTCSS toon, AGC, NB, DNR, IPO, ATT, tuner, skip, step.

### EX Menu Editor (153 items)

Alle 153 setup menu-items zijn uitleesbaar en instelbaar via het **EX commando**:
- **Lezen:** `EX{nnn};` → response `EX{nnn}{P2};`
- **Schrijven:** `EX{nnn}{P2};` met P2 in vaste veldlengte (1-8 digits)
- Enumeratie-items tonen een dropdown, numerieke items een tekstveld
- Gewijzigde waarden gemarkeerd met * (afwijking van default)
- Read-only items (RADIO ID) zijn grijs weergegeven
- **Waarschuwing:** menu 031-033 (CAT RATE/TOT/RTS) wijzigen kan de verbinding verbreken

### Radio Controls (Yaesu Popout Window)

| Control | CAT commando | Opmerkingen |
|---------|-------------|-------------|
| A/B (swap) | `SV;` | Wisselt VFO A ↔ B frequenties |
| V/M | `VM;` | Toggle VFO ↔ Memory mode |
| Mode (8 knoppen) | `MD0{x};` | LSB/USB/CW/CW-R/FM/AM/DIG-U/DIG-L |
| Band +/- | `BU0;`/`BD0;` | Band up/down (VFO-A) |
| Mem +/- | `MC{n±1};` | Stap door memory kanalen 1-99 |
| A=B | `AB;` | Kopieer VFO A → B |
| Split | `ST1;`/`ST0;` | Split mode toggle |
| Scan | `SC1;`/`SC0;` | Memory/VFO scan toggle |
| Tune | `AC002;`/`AC000;` | Tuner on/off |
| SQL/PWR/MIC/RF Gain | `SQ0{nnn};`/`PC{nnn};`/`MG{nnn};`/`RG0{nnn};` | Sliders met waarde weergave |

### VFO/Memory Status

De server pollt `IF;` elke 500ms. Het P7 veld geeft de modus: 0=VFO, 1=Memory, 2=Memory Tune. De client toont:
- **VFO** (groen) — normaal
- **VFO Split** (oranje) — split mode actief
- **MEM nn naam freq** (blauw) — memory mode met kanaalnaam

### Auto-reconnect

Bij stroomverlies van de 991A:
1. Serial thread detecteert 5s geen response → status disconnected
2. Client toont "Power OFF"
3. Elke 3s probeert de server opnieuw te verbinden
4. Bij succes: serial + audio capture + audio output worden herbouwd
5. Persistente audio channels overleven reconnects (network loops hoeven niet te herstarten)

---

## Network Authenticatie (v0.4.7+)

### Challenge-Response (HMAC-SHA256)

ThetisLink ondersteunt optionele wachtwoord-authenticatie via een pre-shared key (PSK):

```
Client                     Server
  |-- Heartbeat ---------->|  (nieuw adres)
  |<- AuthChallenge(nonce)-|  (16 byte random)
  |-- AuthResponse(HMAC) ->|  HMAC-SHA256(wachtwoord, nonce)
  |<- AuthResult(ok/nee) --|
  |-- Heartbeat ---------->|  (normaal verder)
```

- **Geen overhead** op data packets (audio/spectrum) — alleen IP:port check na auth
- **Backward compatible** — geen wachtwoord geconfigureerd = werkt als voorheen
- **Brute-force bescherming** — 5 pogingen per IP, daarna 60s blokkade
- **Wachtwoord** gaat nooit over het netwerk (alleen HMAC van random nonce)
- **Obfuscated opslag** in config bestanden (XOR + hex encoding)

### Configuratie

**Server** (`thetislink-server.conf` of GUI instellingen):
```
password=MijnGeheim
```

**Client** (password veld naast server adres, of `thetislink-client.conf`):
```
password=MijnGeheim
```

**Android** (Settings dialog → Server wachtwoord)

Bij eerste opslaan wordt het wachtwoord automatisch obfuscated.

---

## Wideband Opus TX (v0.4.9+)

### Motivatie

De originele narrowband Opus (8 kHz, 12.8 kbps) veroorzaakte stutter bij USB headsets met 16 kHz capture rate. De 16→8 kHz resampling (ratio 0.5) was instabiel met kleine frame sizes.

### Oplossing

Alle TX audio paden gebruiken nu **wideband Opus (16 kHz, ~24 kbps)**:

| Pad | Capture rate | Resample ratio | Opus | Bandbreedte |
|-----|-------------|----------------|------|-------------|
| Thetis TX (48kHz mic) | 48000 Hz | 3:1 (48→16) | Wideband 16kHz | ~24 kbps |
| Thetis TX (16kHz headset) | 16000 Hz | **1:1 (geen!)** | Wideband 16kHz | ~24 kbps |
| Yaesu TX | capture rate | → 16kHz | Wideband 16kHz | ~24 kbps |

De server heeft aparte resamplers: RX (8kHz→playback) en TX (16kHz→48kHz TCI).

### Jitter Buffer Reset bij Device Switch

Bij wisseling van audio device (microfoon of speaker) worden alle jitter buffers gereset om frame-ophoping te voorkomen. Zonder reset kan de buffer oplopen tot 38+ frames (~760ms vertraging).

---

## Spectrum — IP Fragment Filtering

De HPSDR Protocol 2 DDC capture maakt gebruik van raw sockets (SIO_RCVALL) om netwerkverkeer passief af te luisteren. Hierbij worden ook IP-fragmenten ontvangen. Vervolgfragmenten bevatten geen echte UDP-header — de databytes op de header-positie zijn willekeurig. Zonder filtering kunnen deze ten onrechte matchen op dst_port 1027 (HPC) en garbage-waarden als DDC NCO phaseword doorgeven.

**Fix:** Vervolgfragmenten (IP fragment offset > 0) worden gefilterd bij de HPC packet detectie. Alleen complete UDP packets (eerste fragment of niet-gefragmenteerd) worden als HPC verwerkt. DDC I/Q data packets zijn niet gefragmenteerd en worden ongewijzigd verwerkt.

---

## RX2/VFO-B Support (v0.3.0)

ThetisLink v0.3.0 biedt volledige ondersteuning voor de tweede ontvanger (RX2) van de ANAN 7000DLE. Dit omvat onafhankelijke audio, spectrum/waterfall, en alle bedieningselementen.

### Overzicht RX2 Systeem

```
┌────────────────────────────────────────────────────────────────────────┐
│                          ANAN 7000DLE                                  │
│                                                                        │
│  DDC2 (RX1) ──► Port 1037 ──► I/Q data ──┐                           │
│  DDC3 (RX2) ──► Port 1038 ──► I/Q data ──┤   HPSDR Protocol 2       │
│  HP packets ──► Port 1025 ──► NCO info ───┤   (Ethernet naar Thetis) │
│                                            │                           │
│  Thetis ◄──── CAT TCP:13013 ──────────────┤                           │
│  VAC1 ──► CABLE-A ──► RX1 audio           │                           │
│  VAC2 ──► CABLE   ──► RX2 audio           │                           │
└────────────────────────────────────────────────────────────────────────┘
         │ Ethernet (passief afgeluisterd)        │ VB-Cable audio
         ▼                                        ▼
┌────────────────────────────────────────────────────────────────────────┐
│                       ThetisLink Server                                │
│                                                                        │
│  Raw socket (SIO_RCVALL) ──► IP/UDP parse                             │
│    ├── Port 1037 ──► RX1 SpectrumProcessor (DDC2 I/Q → FFT)          │
│    ├── Port 1038 ──► RX2 Rx2SpectrumProcessor (DDC3 I/Q → FFT)       │
│    └── Port 1025 ──► HP packet parse                                  │
│         ├── Slot 0: RX1 NCO phaseword → RX1 DDC center freq          │
│         └── Slot 3: RX2 NCO phaseword → RX2 DDC center freq          │
│                                                                        │
│  CABLE-A capture ──► Opus enc ──► RX1 audio packets ──► Client        │
│  CABLE   capture ──► Opus enc ──► RX2 audio packets ──► Client        │
│                                                                        │
│  CAT poll ──► ZZFB/ZZME/ZZLB/ZZFS/ZZFR/ZZNV/ZZNU ──► RX2 state    │
└────────────────────────────────────────────────────────────────────────┘
         │ UDP 4580
         ▼
┌────────────────────────────────────────────────────────────────────────┐
│                       ThetisLink Client                                │
│                                                                        │
│  RX1 audio ──► Opus dec ──► Speaker (links of mono)                   │
│  RX2 audio ──► Opus dec ──► Speaker (rechts of mono)                  │
│  RX1 spectrum ──► Spectrum plot + waterfall (boven)                    │
│  RX2 spectrum ──► Spectrum plot + waterfall (onder)                    │
│  RX2 controls: freq, mode, volume, filter, NR, ANF                   │
└────────────────────────────────────────────────────────────────────────┘
```

### Interactie tussen Thetis SDR en ANAN 7000DLE via Protocol 2

De ANAN 7000DLE communiceert met Thetis via HPSDR Protocol 2 over Ethernet. Dit protocol gebruikt UDP op vaste poorten voor verschillende datastromen. ThetisLink luistert passief mee op hetzelfde netwerksegment via een raw socket (SIO_RCVALL).

**Relevante UDP poorten (bron = ANAN, doel = Thetis PC):**

| Bron poort | Functie | Data | Richting |
|------------|---------|------|----------|
| 1025 | High Priority (HP) | NCO phasewords, run/stop, CWX | ANAN → Thetis |
| 1027 | Wideband ADC | Ruwe ADC samples (0-61.44 MHz) | ANAN → Thetis |
| 1035 | DDC0 I/Q | Eerste DDC ontvanger | ANAN → Thetis |
| 1036 | DDC1 I/Q | Tweede DDC ontvanger | ANAN → Thetis |
| 1037 | DDC2 I/Q | **RX1** — Primaire ontvanger | ANAN → Thetis |
| 1038 | DDC3 I/Q | **RX2** — Tweede ontvanger | ANAN → Thetis |
| 1039-1040 | DDC4-DDC5 I/Q | Overige DDC ontvangers | ANAN → Thetis |

**Belangrijk:** Thetis gebruikt standaard DDC2 (poort 1037) voor RX1 en DDC3 (poort 1038) voor RX2. Dit is configureerbaar in Thetis maar de standaard toewijzing wordt door vrijwel iedereen gebruikt.

#### HP Packet Structuur (poort 1025)

Het HP (High Priority) packet bevat de NCO (Numerically Controlled Oscillator) phasewords voor alle DDC ontvangers. De NCO bepaalt de centerfrequentie van elke DDC.

```
Offset   Veld                Grootte   Beschrijving
──────   ────                ───────   ────────────
0-3      Sequence            4 bytes   Pakket volgnummer
4        Run/PTT/CWX        1 byte    Control bits
5-8      CWX0-CWX3          4 bytes   CW keying data
9-12     DDC0 NCO phaseword  4 bytes   NCO slot 0 (DDC0)
13-16    DDC1 NCO phaseword  4 bytes   NCO slot 1 (DDC1)
17-20    DDC2 NCO phaseword  4 bytes   NCO slot 2 (DDC2 = RX1)
21-24    DDC3 NCO phaseword  4 bytes   NCO slot 3 (DDC3 = RX2) ◄─
25-28    DDC4 NCO phaseword  4 bytes   NCO slot 4
29-32    DDC5 NCO phaseword  4 bytes   NCO slot 5
33-36    DDC6 NCO phaseword  4 bytes   NCO slot 6
37-40    DDC7 NCO phaseword  4 bytes   NCO slot 7
```

**NCO phaseword → frequentie conversie:**

De NCO phaseword is een 32-bit unsigned integer die de DDC centerfrequentie codeert als fractie van de klokfrequentie (122.88 MHz):

```
freq_hz = (phaseword as u64 × 122_880_000) >> 32
```

Voorbeeld: phaseword `0x1D4C0000` → `(0x1D4C0000 × 122_880_000) >> 32` = 14.345.000 Hz = 14.345 MHz (20m band)

ThetisLink extraheert slot 0 (RX1) en slot 3 (RX2) uit elk HP packet om de exacte DDC centerfrequentie te bepalen. Dit is nauwkeuriger dan de CAT-gepolde VFO-frequentie en essentieel voor correcte spectrum rendering bij CTUN.

#### DDC I/Q Data Packets (poort 1037/1038)

Elk DDC I/Q packet bevat 238 I/Q sample-paren in 24-bit big-endian signed formaat:

```
Offset   Veld                Grootte   Beschrijving
──────   ────                ───────   ────────────
0-3      Sequence            4 bytes   Pakket volgnummer
4-7      Timestamp           4 bytes   Sample timestamp
8        Bits per sample     1 byte    Altijd 24
9        Samples per frame   1 byte    Altijd 238
10-15    Header padding      6 bytes   (ongebruikt)
16+      I/Q data            238×6 B   24-bit I, 24-bit Q per sample
```

**Sample formaat:** Elke I/Q sample bestaat uit 6 bytes: 3 bytes I (signed 24-bit big-endian) + 3 bytes Q (signed 24-bit big-endian). De Q-waarde wordt genegeerd (`-q_val`) om de HPSDR I/Q conventie te corrigeren — zonder negatie zou het spectrum gespiegeld zijn.

**Sample rate:** Configureerbaar in Thetis (typisch 48, 96, 192, 384, 768 of 1536 kHz). ThetisLink detecteert de sample rate automatisch door de timing van inkomende packets te meten. Bij 1536 kHz komen er ~6470 packets per seconde (238 samples × 6470 ≈ 1.536.000 samples/sec).

### Effect van de CTUN-knop op DDC-netwerkstreams

De CTUN (Click-TUNe) functie in Thetis verandert fundamenteel hoe de VFO en de DDC samenwerken. Dit heeft directe gevolgen voor de spectrumdisplay in ThetisLink.

#### Normaal (CTUN uit)

```
Gebruiker draait VFO-A ──► Thetis stuurt nieuw NCO phaseword naar ANAN
                           ──► ANAN verplaatst DDC2 centerfrequentie
                           ──► I/Q data op poort 1037 verschuift mee
                           ──► ThetisLink spectrum centreert op nieuwe freq
```

Bij CTUN=uit beweegt de hele DDC mee met de VFO. De VFO-frequentie IS de DDC center­frequentie. Het spectrum in ThetisLink blijft altijd gecentreerd op de VFO.

**Gevolg:** Bij elke VFO-wijziging verschuift het hele spectrum. Dit is zichtbaar als een "sprong" in de waterfall.

#### CTUN aan

```
Gebruiker klikt in spectrum ──► Thetis verplaatst VFO-A marker
                                ──► DDC center blijft VAST (NCO ongewijzigd)
                                ──► I/Q data op poort 1037 onveranderd
                                ──► VFO offset = VFO_freq - DDC_center
```

Bij CTUN=aan "bevriest" Thetis de DDC positie. De NCO phaseword in het HP packet verandert niet meer wanneer de gebruiker tuned. In plaats daarvan beweegt alleen de VFO-marker binnen het bestaande DDC bereik. De DDC center wordt pas verplaatst als de VFO buiten het huidige DDC venster dreigt te vallen — dan doet Thetis een "re-center" en verschuift de DDC.

**Gevolg voor ThetisLink:**
- Het spectrum blijft stabiel (geen sprongen bij tuning)
- De VFO-marker (rode lijn) beweegt over het spectrum
- De HP packet NCO phaseword geeft de werkelijke DDC center (≠ VFO frequentie)
- ThetisLink berekent de offset: `vfo_offset = vfo_freq - ddc_center_freq`

#### Implementatie in ThetisLink

ThetisLink detecteert CTUN status via het CAT commando `ZZCT;` (gepolld elke 500ms).

**`Rx2SpectrumProcessor::set_vfo_freq(freq_hz, ctun)`:**
1. **CTUN uit + geen HP data:** DDC center volgt VFO (aanname: ze zijn gelijk)
2. **CTUN aan:** DDC center bevroren, VFO positie alleen voor display offset
3. **HP data beschikbaar:** DDC center komt altijd uit HP packet (meest nauwkeurig)

De HP packet data heeft altijd voorrang boven de CAT-gepolde frequentie, omdat:
- HP packets komen elke ~1.3ms (veel sneller dan 500ms CAT poll)
- HP packets bevatten de exacte NCO waarde (geen afrondingsfouten)
- Bij CTUN is het HP packet de enige bron van de werkelijke DDC center

#### Waarom CTUN de frequentie-offset lijkt te veranderen

Wanneer CTUN ingeschakeld wordt, verandert er ogenschijnlijk iets in de gecapturede DDC data. In werkelijkheid verandert er niets aan de data zelf — wat verandert is de relatie tussen VFO-frequentie en DDC center:

- **CTUN uit:** VFO = DDC center → offset = 0 → spectrum gecentreerd op VFO
- **CTUN aan:** VFO ≠ DDC center → offset ≠ 0 → VFO marker verschoven in spectrum

Als ThetisLink niet corrigeert voor deze offset, lijkt het spectrum "verkeerd gecentreerd" of lijkt de VFO-frequentie niet te kloppen met de spectrumlijn. De correctie zit in `extract_view()` waar het display bereik berekend wordt op basis van DDC center (niet VFO).

### RX2 DDC3 Spectrum Capture

De tweede ontvanger gebruikt DDC3 (poort 1038). ThetisLink start een aparte capture thread voor RX2, identiek aan de RX1 capture maar op een andere poort.

#### Opstarten

```rust
// main.rs: RX2 DDC port = RX1 DDC port + 1
let rx2_ddc_port = config.ddc_port + 1;  // 1037 + 1 = 1038

// Aparte capture thread voor DDC3
start_ddc_capture(
    anan_ip,
    rx2_ddc_port,      // 1038
    rx_id: 3,          // HP packet slot 3 voor NCO
    sample_rate,
    rx2_iq_sender,     // Channel naar Rx2SpectrumProcessor
    rx2_hp_sender,     // Channel voor HP NCO updates
);
```

#### Rx2SpectrumProcessor

Onafhankelijke FFT-pipeline, identiek aan de RX1 `SpectrumProcessor` maar met eigen state:

```rust
pub struct Rx2SpectrumProcessor {
    enabled: bool,                          // RX2 spectrum aan/uit per client
    fps: u8,                                // Frame rate (5-30)
    sequence: u16,                          // Frame teller
    smoothed: Vec<f32>,                     // Spectrum bins met peak-hold + decay
    ddc_pipeline: Option<DdcFftPipeline>,   // FFT processor (rustfft)
    vfo_freq_hz: u64,                       // VFO-B frequentie (uit CAT: ZZFB)
    ddc_center_hz: u64,                     // DDC3 center (uit HP slot 3)
    sample_rate_hz: u32,                    // Auto-gedetecteerd
    skip_fft_frames: u8,                    // Skip stale frames na freq change
    has_hp_center: bool,                    // HP packet ontvangen?
}
```

**FFT pipeline:**
- Dynamische FFT grootte op basis van sample rate (functie `ddc_fft_size()`)
- Bij 1536 kHz: FFT size = 262144 → 5.859 Hz/bin resolutie, ~12 FPS
- Bij lagere rates: kleinere FFT, zelfde ~12 FPS doel
- 50% overlap voor vloeiende updates
- Blackman-Harris window, complex FFT, FFT-shift, dB mapping, EMA smoothing

**Peak-hold decay:** `smoothed[i] = max(new_value, smoothed[i] × 0.6)` — pieken zakken langzaam weg, geeft goed leesbaar spectrum.

**Band change detectie:** Bij VFO sprong > sample_rate/4 wordt de smoothed buffer gereset en worden 2 FFT frames overgeslagen om stale data te vermijden.

#### Per-client RX2 spectrum state

Elke verbonden client heeft eigen RX2 spectrum instellingen:

| Setting | Bereik | Default | Beschrijving |
|---------|--------|---------|-------------|
| rx2_spectrum_enabled | bool | false | RX2 spectrum aan/uit |
| rx2_spectrum_fps | 5-30 | 10 | Frame rate |
| rx2_spectrum_zoom | 1.0-1024.0 | 1.0 | Zoom niveau |
| rx2_spectrum_pan | -0.5 tot 0.5 | 0.0 | Pan offset |

### RX2 Dual-Input Audio

De server ondersteunt twee onafhankelijke audio-invoerkanalen voor RX1 en RX2.

#### Configuratie

```bash
# Dual-input: aparte devices voor RX1 en RX2
ThetisLink-Server.exe --input "CABLE-A Output" --input2 "CABLE Output" --output "CABLE-B Input"
```

- `--input`: RX1 audio device (VAC1 output van Thetis)
- `--input2`: RX2 audio device (VAC2 output van Thetis)
- `--output`: TX audio device (VAC input naar Thetis)

#### Audio Pipeline

```
Thetis VAC1 ──► CABLE-A ──► Server capture1 ──► resample ──► Opus enc ──► RX1 audio packet ──► Client
Thetis VAC2 ──► CABLE   ──► Server capture2 ──► resample ──► Opus enc ──► RX2 audio packet ──► Client
```

Elke capture stream draait op een eigen cpal thread met eigen ring buffer. RX2 audio wordt als apart packet type (`AudioRx2`) verzonden naar clients die RX2 hebben ingeschakeld.

### RX2 CAT Commando's

De volgende Thetis CAT commando's worden gebruikt voor RX2 bediening:

| Commando | Functie | Bereik | Opmerkingen |
|----------|---------|--------|-------------|
| `ZZFB` | VFO-B frequentie | 11 cijfers (Hz) | Lezen + instellen |
| `ZZME` | RX2 mode | 00-11 | LSB=00, USB=01, DSB=02, CWL=03, CWU=04, FM=05, AM=06, ... |
| `ZZLB` | RX2 AF volume | 000-100 | Stuurt CABLE/VAC2 volume |
| `ZZFS` | RX2 filter low cut | Signed Hz | Negatief voor LSB |
| `ZZFR` | RX2 filter high cut | Signed Hz | Positief voor USB |
| `ZZNV` | RX2 NR level | 0-4 | 0=uit, 1-4=NR1-NR4 (gecorrigeerd in v0.4.1, was ZZNF) |
| `ZZNU` | RX2 ANF | 0/1 | Auto Notch Filter aan/uit |
| `ZZRM2` | RX2 S-meter | 0-260 | 12 per S-unit, S9=108 |
| `ZZCT` | CTUN status | 0/1 | Geldt voor hele radio (RX1+RX2) |

**Let op ZZNV (v0.4.1):** In v0.4.0 werd per abuis `ZZNF` gebruikt voor RX2 NR polling en instellen. `ZZNF` is een RX1-commando. Het correcte commando voor RX2 NR is `ZZNV`. Dit is gecorrigeerd in v0.4.1. De full-poll string is bijgewerkt van `...ZZNF;ZZNU;` naar `...ZZNV;ZZNU;`.

### Desktop Client: Joined/Split Popout Windows

De desktop client ondersteunt twee weergavemodi voor het popout window:

#### Split modus (standaard)

Bij klikken op "Pop-out" openen twee aparte vensters:
- **RX1 window:** VFO-A controls + RX1 spectrum/waterfall
- **RX2 window:** VFO-B controls + RX2 spectrum/waterfall (alleen als RX2 ingeschakeld)

Elk venster onthoudt zijn eigen positie en grootte.

#### Joined modus

Na klikken op "Join" in een popout window worden beide vensters samengevoegd tot één gecombineerd window:

```
┌──────────────────────────────────────────────────────────┐
│  VFO-A controls          │  [Join]  VFO-B controls       │
│  14.345.000 Hz  S9+10    │  7.073.000 Hz  S7             │
│  [LSB] [USB] [AM] [FM]   │  [LSB] [USB] [AM] [FM]       │
│  NR2  ANF  Filter 2.7k   │  NR1  ANF  Filter 2.7k       │
├──────────────────────────────────────────────────────────┤
│  ╔═══════════════════ RX1 Spectrum ══════════════════╗   │
│  ║  Spectrum plot + waterfall (bovenste helft)       ║   │
│  ╚═══════════════════════════════════════════════════╝   │
│  ╔═══════════════════ RX2 Spectrum ══════════════════╗   │
│  ║  Spectrum plot + waterfall (onderste helft)       ║   │
│  ╚═══════════════════════════════════════════════════╝   │
└──────────────────────────────────────────────────────────┘
```

De joined/split voorkeur wordt opgeslagen in de client configuratie en onthouden bij herstart.

---

## Toekomstige Verbeteringen

- [x] 2e VFO/RX2 ondersteuning (v0.3.0)
- [x] CTUN modus: spectrum vaste DDC center, VFO marker beweegt (v0.3.0)
- [x] TCI WebSocket integratie, geen VB-Cable nodig (v0.4.0)
- [x] Embedded WebSDR/KiwiSDR WebView met freq-sync en TX mute (v0.4.1)
- [x] TX spectrum auto-override bij zenden (v0.4.1)
- [x] Range slider uitgebreid naar 130dB (v0.4.1)
- [x] RX2 NR CAT commando gecorrigeerd naar ZZNV (v0.4.1)
- [ ] AM/FM audio werkend (nu geen geluid in AM/FM modus)
- [ ] Spectrum fijnslijping (betere rendering, kleurenpalet)
- [ ] HPSDR Protocol 1 ondersteuning (Hermes, Angelia, Orion)
- [ ] Andere grafische engine (Bevy of wgpu) voor betere spectrum/waterfall rendering
- [ ] Wideband Opus (16kHz) voor betere spraakkwaliteit
- [ ] Android spectrum zoom/pan (pinch-to-zoom + drag)
- [ ] Instelbaar waterfall kleurenpalet
- [x] Hogere FFT resolutie: 262144 punten (5.859 Hz/bin, Thetis-kwaliteit)
- [x] Ref level en range sliders in spectrum display (desktop)
- [x] Spectrum zoom/pan met sliders (desktop)
- [x] Scroll-to-tune en drag-to-tune in spectrum/waterfall
- [x] Alle externe apparaten (Amplitec, JC-4s, SPE, RF2K-S, UltraBeam, Rotor)
