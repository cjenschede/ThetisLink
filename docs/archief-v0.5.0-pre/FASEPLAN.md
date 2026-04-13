# ThetisLink — Faseplan

## Overzicht

Remote bediening ANAN 7000DLE + Thetis SDR over internet/netwerk.
Ontwikkeltaal: Rust + Kotlin (Android). Prioriteit: latency > bandbreedte > features.

| Fase | Thema | Status |
|------|-------|--------|
| **0** | Core: protocol, codec, jitter buffer, PTT safety | Compleet |
| **1** | Multiplatform architectuur: logic crate, AudioBackend trait | Compleet |
| **2** | Android client: UniFFI bridge, Oboe audio, Compose UI | Compleet |
| **3** | Spectrum/waterfall display via HPSDR wideband capture | Compleet |
| **3b** | DDC I/Q spectrum: detail rond VFO, zoom/pan, tuning | Compleet |
| **v0.1.0** | ThetisLink branding + netwerk-robuustheid (LAN/WiFi/4G) | Compleet |
| **v0.1.1** | RX filter bandwidth control (desktop + Android) | Compleet |
| **v0.3.0** | Volledige RX2/VFO-B: dual audio, DDC3 spectrum, joined windows | Compleet |
| **4** | Uitgebreide desktop GUI: resizable, spectrum integratie | Gepland |
| **5a** | Amplitec 6/2 antenna switch + JC-4s tuner | Compleet |
| **5b** | PA, rotor, overige switches | Gepland |

---

## Fase 0 — Core Hardening (Compleet)

Fundament: betrouwbare audio + PTT over UDP.

- Binair UDP protocol op poort 4580 (audio, heartbeat, control, freq, mode, smeter)
- Opus codec: 8 kHz mono, 12.8 kbps, FEC + DTX, 20ms frames
- Adaptieve jitter buffer (RFC 3550 EMA, overflow recovery, grace period)
- Resampling: rubato SincFixedIn (128-punt sinc, Blackman window)
- PTT safety: 5 lagen (flag per packet, burst bij state change, packet timeout, heartbeat timeout, tail delay)
- Lock-free SPSC ring buffers (audio ↔ netwerk thread)

## Fase 1 — Multiplatform Architectuur (Compleet)

Business logic gescheiden van UI en platform, klaar voor Android en toekomstige clients.

- `sdr-remote-logic` crate: ClientEngine met watch/mpsc channels
- `AudioBackend` trait: platform-abstractie voor audio I/O
- `CatConnection` extractie uit monolithische ptt.rs
- Desktop client: egui UI, cpal audio (WASAPI)
- Server: tokio async, multi-client sessie management, Thetis CAT TCP
- Bidirectionele controls: freq, mode, power, NR, ANF, drive, TX profile, RX volume (ZZLA)

## Fase 2 — Android Client (Compleet)

Volledige Android app met native Rust audio engine.

- `sdr-remote-android` crate met UniFFI 0.28 bridge (Rust ↔ Kotlin)
- Oboe audio (AAudio): LowLatency, Exclusive, VoiceRecognition input, Media output
- Jetpack Compose UI: PTT, freq display, S-meter, controls, memories
- SdrViewModel pollt bridge op 30fps via StateFlow
- TX AGC (schakelbaar, peak-based envelope follower)
- TX power indicator (forward power via ZZRM5)
- "TX in use" indicator voor multi-client (SmeterPacket flags)
- Persistente instellingen: volume, TX gain, AGC, memories (freq + mode)

---

## Fase 3 — Wideband Spectrum (Compleet, vervangen door 3b)

Oorspronkelijke wideband implementatie: passieve HPSDR capture van ADC data (poort 1027), 16k-punt FFT, 1024 bins 0–61.44 MHz. Werkte maar was onbruikbaar — te breed voor praktisch gebruik. Vervangen door DDC I/Q spectrum in Fase 3b.

Nog beschikbaar via `--wideband` flag.

---

## Fase 3b — DDC I/Q Spectrum (Compleet)

DDC I/Q spectrum gecentreerd op VFO frequentie met 262k-punt FFT en server-side zoom/pan. Vervangt wideband als standaard spectrum modus.

### Aanpak: Passieve DDC I/Q Capture + Server-Side Zoom

De server vangt passief HPSDR Protocol 2 DDC I/Q pakketten af via Windows raw sockets (SIO_RCVALL). De ANAN stuurt DDC data naar Thetis op source ports 1035-1040 — de server luistert mee zonder het verkeer te onderbreken.

De 262k-punt FFT geeft 5.859 Hz/bin resolutie (Thetis-kwaliteit). Alle bins worden intern bewaard; per client wordt een zoom/pan-afhankelijke view van max 8192 bins geëxtraheerd en verstuurd.

```
ANAN 7000DLE ──UDP──► Thetis (ongewijzigd)
       │
       └── DDC I/Q data (src port 1035-1040)
               │
       SDR Remote Server (raw socket capture)
               │
               ├── Auto-detect + lock eerste DDC port (typisch 1037 = DDC2 = RX1)
               ├── 24-bit big-endian I/Q pairs, Q genegeerd
               ├── Accumulatie tot 262144 I/Q samples (50% overlap)
               ├── Blackman-Harris window (262144 punt)
               ├── Complex FFT forward (rustfft, 262144 punt)
               ├── FFT-shift (DC → centrum)
               ├── |c|² normalisatie (÷ N²)
               ├── dB → 0-255 mapping (-150 dB → 0, -30 dB → 255)
               ├── EMA smoothing (α=0.4, 262144 bins)
               ├── Per-client extract_view(zoom, pan) → max 8192 bins
               │     └── Float stride decimatie (volledige afdekking, geen offset)
               └── SpectrumPacket (≤8192 bins, center_hz, span_hz) → clients
```

| Zoom | Zichtbaar span | Bins | Hz/bin | Detail |
|------|---------------|------|--------|--------|
| 1x | 1.536 MHz | 8192 | 187.5 | Overzicht |
| 4x | 384 kHz | 8192 | 46.9 | Goed |
| 16x | 96 kHz | 8192 | 11.7 | Hoog |
| 32x | 48 kHz | 8192 | **5.859** | Thetis-kwaliteit |

### Implementatie

- **Capture**: `hpsdr_capture.rs` — DDC I/Q parsing op source ports 1035-1040, 24-bit signed I/Q
- **FFT**: `spectrum.rs` — `DdcFftPipeline` (262144-punt complex FFT + FFT-shift, 50% overlap, ~11.7 FPS)
- **Server-side zoom**: `extract_view(zoom, pan)` met float stride decimatie (geen integer afrondingsoffset)
- **Frequency tracking**: `set_vfo_freq()` — VFO-A als DDC center (non-CTUN modus), band change detectie
- **Protocol**: `SpectrumPacket` met `center_freq_hz` (Hz-precisie) en `span_hz`, max 8192 bins
- **Per-client zoom/pan**: `ControlId::SpectrumZoom` (0x09), `ControlId::SpectrumPan` (0x0A)
- **Client rendering**: Max-per-pixel aggregatie (8192 bins → scherm pixels)
- **Tuning**: Scroll-to-tune (±1 kHz), drag-to-tune (100 Hz snap), click-to-tune (1 kHz snap)
- **Zoom/pan**: Server-side sliders (1x–32x zoom, pan binnen bereik) met 100ms debounce
- **Bounce fix**: `pending_freq` gecleard op basis van spectrum center match (niet engine state)
- **Responsive scroll**: `tune_base_hz` parameter voor correcte accumulatie bij snel scrollen
- **Filter passband**: ZZFL/ZZFH CAT → signed Hz offsets → grijze achtergrond + gele randlijnen in spectrum
- **Band highlight**: Memory-knoppen kleuren blauw bij actieve band
- **CLI**: `--anan-interface <IP>`, `--ddc-rate <kHz>` (default 1536), `--wideband` (fallback)
- **Test mode**: DDC test generator met signalen rond VFO (±10–65 kHz)

### Features

- 262k-punt FFT: 5.859 Hz/bin native resolutie bij zoom 32x (Thetis-kwaliteit)
- Server-side zoom/pan: elke client eigen view, max 8192 bins per pakket
- VFO marker (rode lijn) met MHz label, stabiel bij tuning (geen bounce)
- RX filter passband weergave (grijze achtergrond + gele grenzen, ZZFL/ZZFH signed offsets)
- Band markers (alleen zichtbaar als binnen bereik)
- Dynamische frequentie-as labels (past aan zoom niveau aan)
- Waterfall met ring buffer, UV zoom/pan, contrast slider
- Ref level (-80..0 dB) / range (20..200 dB) / zoom / pan / contrast sliders
- Per-client spectrum enable/disable en fps instelling

### Bandbreedte

| Data | Grootte | Rate | Bandbreedte |
|------|---------|------|-------------|
| Audio (bestaand) | ~60 bytes | 50 fps | ~24 kbps |
| Spectrum DDC (desktop) | ~8210 bytes | 10 fps | ~657 kbps |
| Spectrum DDC (Android) | ~8210 bytes | 5 fps | ~329 kbps |

### Referenties

- [HPSDR Protocol 2 documentatie](https://github.com/TAPR/OpenHPSDR-Firmware/tree/master/Protocol%202/Documentation)
- [piHPSDR](https://github.com/g0orx/pihpsdr) — open-source HPSDR client (referentie)
- [Wireshark dissector voor Protocol 2](https://github.com/matthew-wolf-n4mtt/openhpsdr-e)

---

## v0.1.0 — ThetisLink Branding + Netwerk-robuustheid (Compleet)

Eerste versienummer. Adaptief voor LAN, WiFi en 4G/5G mobiel.

### Netwerk-robuustheid
- **Dual-alpha jitter adaptatie:** α=1/4 bij spike (snel buffer groeien), α=1/16 bij herstel (langzaam krimpen). Voorkomt hakkelen op 4G zonder onnodige latency op LAN.
- **Geleidelijke overflow recovery:** Max 1 frame per pull() skippen (was: alles tegelijk). Threshold target+4 (was: target+10). Geen hoorbare klik/stutter meer.
- **Dynamische connectie-timeout:** `max(6s, rtt×8)`. Vereist dat BEIDE heartbeat ACK en audio uitblijven. Op 4G in tunnel (3-5s uitval) geen onnodig disconnect.
- **Geen jitter reset bij timeout:** Buffer draineert via Opus PLC. Audio hervat vloeiend zodra pakketten terugkomen.
- **Spectrum throttling bij loss:** >15% → pauze, 5-15% → halve FPS. Audio heeft altijd prioriteit.
- **Server session timeout:** 5s → 15s. Mobiele netwerken krijgen meer tijd.
- **Loss EMA smoothing:** α=0.3. Loss display springt niet meer heen en weer.

### Branding
- Gedeeld versienummer `VERSION` in `sdr-remote-core` (server + client altijd synchroon)
- Versie zichtbaar in: window titles, UI headings, startup logs
- Exe namen: `ThetisLink-Server.exe`, `ThetisLink-Client.exe`
- Windows resource metadata (ProductName, FileDescription, icon)
- Client: in-app GUI logger (ring buffer, toggle in UI)

---

## v0.1.1 — RX Filter Bandwidth Control (Compleet)

RX ontvanger-bandbreedte instellen via desktop en Android client.

### Filter bandwidth control
- **UI:** `[ - ] 2.7 kHz [ + ]` — compact, naast frequentiedisplay
- **Mode-afhankelijke presets:**
  - SSB (LSB/USB/DIGU/DIGL): 1800, 2100, 2400, 2700, 3000, 3300, 3600, 4000 Hz
  - CW (CWL/CWU): 50, 100, 250, 500, 1000 Hz
  - AM/SAM/DSB/DRM: 4000, 6000, 8000, 10000, 12000 Hz
  - FM: 8000, 12000, 16000 Hz
- **Sideband-aware berekening:**
  - USB/DIGU: low edge verankerd (min 25 Hz), filter breidt naar boven uit
  - LSB/DIGL: high edge verankerd (max -25 Hz), filter breidt naar beneden uit
  - CW: gecentreerd rond CW pitch
  - AM/FM: symmetrisch rond 0
- **Server:** `ZZFL{hz};` / `ZZFH{hz};` CAT commando's (was read-only, nu bidirectioneel)
- **Protocol:** FilterLow (0x0B) en FilterHigh (0x0C) ControlPacket nu client→server

---

## Fase 5a — Amplitec 6/2 + JC-4s Antenna Tuner (Compleet)

Externe apparaten aansturing: Amplitec 6/2 antenna switch en JC-4s antenna tuner, volledig bediendbaar vanuit server UI, desktop client en Android app.

### Amplitec 6/2 Antenna Switch

6-poorts antenna switch met twee onafhankelijke schakelaars (A=TX+RX, B=RX-only). Communicatie via serieel (USB-TTL, 9600 baud). Server UI met apart venster voor directe bediening. Positie wordt elke 2 seconden naar alle clients gebroadcast via `EquipmentStatusPacket`.

### JC-4s Antenna Tuner — USB Tune Interface

De JC-4s is een automatische antenna tuner (CG-3000 kloon) die normaal wordt aangestuurd via de ACC-poort van een Yaesu transceiver. De JC-4s heeft **geen** ingebouwde USB- of computerinterface. De hieronder beschreven aansturing is volledig zelfgebouwd en maakt gebruik van een USB-naar-serieel adapter, waarbij alleen de RTS/CTS handshake-lijnen worden gebruikt — er worden geen data bytes verstuurd.

#### Achtergrond: hoe de JC-4s normaal werkt

De JC-4s verwacht een "KEY" signaal (actief laag) van de radio om een tune-cyclus te starten. Zodra KEY actief wordt, begint de tuner. Wanneer de tuner klaar is, geeft hij een "TUNING COMPLETE" signaal terug. Bij Yaesu radio's stuurt de transceiver automatisch een tune carrier en activeert KEY via de ACC-poort.

Met een ANAN/Thetis SDR is er geen ACC-poort. De tune carrier moet via CAT worden geactiveerd (`ZZTU1;`) en het KEY/COMPLETE signaal moet via een andere weg lopen.

#### Hardware: USB-serieel als GPIO

We gebruiken een standaard **USB-naar-serieel adapter** (FTDI, CH340, CP2102 — maakt niet uit) als twee-draads GPIO interface:

```
USB-Serial Adapter          JC-4s Tuner (ACC connector)
┌──────────────┐            ┌─────────────────┐
│              │            │                 │
│     RTS ─────┼────────────┼─► KEY (pin 5)   │  PC → Tuner: "start tune"
│              │            │                 │
│     CTS ◄────┼────────────┼── START (pin 3) │  Tuner → PC: "bezig / klaar"
│              │            │                 │
│     GND ─────┼────────────┼── GND (pin 2)   │  Gemeenschappelijke ground
│              │            │                 │
│     DTR ─────┼── (HIGH, niet gebruikt maar  │
│              │    nodig voor voeding)        │
└──────────────┘            └─────────────────┘
```

**Benodigde onderdelen:**
- 1x USB-serieel adapter (FTDI FT232R, CH340G, CP2102, etc.)
- 3 draadjes (RTS→KEY, CTS←START, GND)
- 1x DIN-8 connector passend op de JC-4s ACC-poort (of losse draden)

**Pin-mapping JC-4s ACC-poort (DIN-8):**
| Pin | Functie | Richting | Beschrijving |
|-----|---------|----------|--------------|
| 2 | GND | — | Ground referentie |
| 3 | START | Tuner → PC | Hoog tijdens tunen, laag als klaar |
| 5 | KEY | PC → Tuner | Laag = activeer tune |

> **Let op:** De exacte pin-nummering kan per JC-4s variant/revisie verschillen. Controleer met een multimeter welke pin het KEY-signaal verwacht en welke het START/COMPLETE signaal geeft. De logica is: KEY actief laag, START/COMPLETE gaat hoog bij tunen en laag bij gereed.

#### Software: tune sequentie

De complete tune-sequentie die ThetisLink uitvoert:

```
Tijd    Actie                        Signalen
─────   ──────────────────────────   ────────────────────
t=0     Drive verlagen naar 15%      ZZPC015; (via CAT)
t+200ms RTS HIGH                     RTS=1 → KEY actief
t+350ms Tune carrier aan             ZZTU1; (via CAT)
t+850ms RTS LOW                      RTS=0
        ← Wacht CTS HIGH            CTS wordt 1 (tuner bezig)
        ← Wacht CTS LOW             CTS wordt 0 (tune klaar!)
        Tune carrier uit             ZZTU0; (via CAT)
        Drive herstellen             ZZPC{orig}; (via CAT)
        Status: DONE_OK
```

**Timing details:**
1. **Drive bescherming (t=0):** Verlaag TX drive naar een veilig niveau (default 15%, configureerbaar via `tuner_safe_drive` in server config). Dit beschermt de eindtrap en antenne tijdens het tunen, wanneer de SWR hoog kan zijn.
2. **RTS HIGH (t+200ms):** Activeert de KEY-lijn. De JC-4s ziet dit als tune-verzoek en bereidt zich voor.
3. **ZZTU1 (t+350ms):** Schakelt de Thetis tune carrier in. De radio begint nu een ongemoduleerde draaggolf te zenden.
4. **RTS LOW (t+850ms):** Na 500ms carrier is de RF stabiel. RTS gaat laag. De JC-4s begint nu daadwerkelijk te tunen.
5. **CTS HIGH:** De JC-4s geeft via CTS (= START pin) aan dat hij bezig is met tunen.
6. **CTS LOW:** Tune compleet. De JC-4s heeft de juiste L/C combinatie gevonden.
7. **ZZTU0 + drive herstel:** Carrier uit, drive terug naar het originele niveau.

**Timeout en foutafhandeling:**
- CTS moet binnen 3 seconden TRUE worden na RTS LOW, anders: timeout
- Totale tune mag maximaal 30 seconden duren, anders: timeout
- Bij timeout of abort: carrier uit (`ZZTU0;`), drive hersteld, RTS laag
- Abort is altijd mogelijk: client stuurt AbortTune command

**Drive bescherming:**
- Bij tune via **client** (remote): server leest het huidige drive-niveau uit de PttController en herstelt exact dat niveau na de tune
- Bij tune via **server UI/macro**: gebruikt default restore van 100% (server UI kent het actuele drive-niveau niet direct)
- Het veilige tune-niveau is configureerbaar: `tuner_safe_drive=15` in de server config (0-100%)

#### Serieel port configuratie

```
Baud:      9600 (maakt niet uit — we sturen geen data)
Data bits: 8
Parity:    None
Stop bits: 1
Timeout:   100ms

Initialisatie:
  DTR = HIGH  (sommige adapters hebben dit nodig voor CTS readback)
  RTS = LOW   (standaard: niet tunen)
```

Er worden geen bytes gelezen of geschreven. De communicatie verloopt volledig via de modem control lines (RTS output, CTS input). De baud rate en data-instellingen zijn irrelevant maar moeten geldig zijn om de poort te openen.

#### Protocol: server → clients

De tuner status wordt via het bestaande Equipment protocol gebroadcast:

```
EquipmentStatusPacket {
    device_type: Tuner (0x02),
    switch_a:    tuner_state (0=Idle, 1=Tuning, 2=DoneOk, 3=Timeout, 4=Aborted),
    switch_b:    can_tune (1=ja, 0=nee),
    connected:   tuner hardware online,
    labels:      None,
}
```

De server bepaalt `can_tune` op basis van de Amplitec switch A positie: als het label van de huidige positie "jc-4s" bevat (case-insensitive), mag er getuned worden. Zonder Amplitec is tune altijd beschikbaar.

Client commands:
```
EquipmentCommandPacket {
    device_type: Tuner (0x02),
    command_id:  CMD_TUNE_START (0x01) of CMD_TUNE_ABORT (0x02),
}
```

#### UI: tune knop in alle clients

De tune knop verschijnt naast de PTT knop, alleen zichtbaar als `tuner_connected && tuner_can_tune`.

**Kleurcodering per state:**
| State | Kleur | Tekst | Actie bij klik |
|-------|-------|-------|----------------|
| Idle | Grijs | "Tune" | Start tune |
| Tuning | Blauw | "Tune..." | Abort tune |
| Done OK | Groen | "Tune ✓" | Start nieuwe tune |
| Timeout | Oranje | "Tune ✗" | Start nieuwe tune |
| Aborted | Oranje | "Tune ✗" | Start nieuwe tune |

**Frequency stale detectie:** Als de huidige VFO-frequentie meer dan 25 kHz afwijkt van de frequentie waarop de laatste succesvolle tune plaatsvond, gaat de knop terug naar grijs — ten teken dat er opnieuw getuned moet worden. De DONE_OK state op de server blijft wel staan; alleen de kleur in de client verandert.

#### Macro integratie

Tune is beschikbaar als macro-actie in het server macro systeem:
```
MacroAction::Tune
```
Een typische band-switch macro:
1. `CAT: ZZFA00014200000;` — Schakel naar 20m (14.200 MHz)
2. `Delay: 500ms` — Wacht op frequentiewissel
3. `Tune` — Start automatische tune

De macro runner wacht tot de tune compleet is (max 35s) of breekt af bij user abort.

#### Bestanden

| Bestand | Functie |
|---------|---------|
| `sdr-remote-server/src/tuner.rs` | JC-4s controller: serieel, tune sequentie, state machine |
| `sdr-remote-server/src/network.rs` | Tuner status broadcast, client command handling |
| `sdr-remote-server/src/config.rs` | `tuner_safe_drive`, window posities |
| `sdr-remote-server/src/macros.rs` | Tune als macro actie |
| `sdr-remote-server/src/ui.rs` | Server tune knop met kleurcodering |
| `sdr-remote-core/src/protocol.rs` | `DeviceType::Tuner`, CMD_TUNE_START/ABORT |
| `sdr-remote-logic/src/state.rs` | Client state: tuner_connected/state/can_tune |
| `sdr-remote-logic/src/engine.rs` | Tuner status parsing, tune/abort commands |
| `sdr-remote-client/src/ui.rs` | Desktop tune knop met kleuren + stale detectie |
| `sdr-remote-android/src/bridge.rs` | UniFFI bridge: tuner state + commands |
| `android/.../MainScreen.kt` | Android tune knop in bottom bar |

### Window posities onthouden

Amplitec en tuner vensters bewaren hun positie bij sluiten. Bij heropenen worden ze op dezelfde plek getoond. Opgeslagen in server config als `tuner_pos_x/y` en `amplitec_pos_x/y`.

---

## Fase 4 — Uitgebreide Desktop GUI (Gepland)

### Doel

Professionele desktop applicatie met groot spectrum/waterfall display, resizable vensters en uitgebreide instellingen. Vergelijkbaar met een volwaardige SDR applicatie.

### Belangrijkste features

1. **Spectrum/waterfall integratie**
   - Groot spectrum + waterfall display als hoofdelement van de GUI
   - Klikbaar: klik op frequentie om VFO te verplaatsen
   - Zoom en pan (scroll wheel, drag)
   - Instelbare referentieniveau, bereik, kleurenpalet
   - Band markers en segment grenzen

2. **Resizable layout**
   - Windows-native venster met resizable panels
   - Spectrum neemt beschikbare ruimte in
   - Controls in opvouwbare/dockable panels
   - Volledig scherm modus

3. **Uitgebreide instellingen**
   - Audio device selectie met preview/test
   - Netwerk configuratie (server, proxy, poorten)
   - Spectrum instellingen (FFT grootte, averaging, kleuren, watervalhoogte)
   - TX/RX audio processing parameters
   - Keyboard shortcuts configureerbaar
   - Meerdere profielen (verschillende stations/configuraties)

4. **Verbeterde controls**
   - Multi-VFO (VFO-A + VFO-B, split mode)
   - RIT/XIT offset
   - Bandscope met bandplan overlay
   - CW decoder display (optioneel)
   - Logging integratie (ADIF export)

5. **UI framework overweging**
   - Huidige egui is geschikt voor controls maar beperkt voor spectrum rendering
   - Opties: egui met custom OpenGL/wgpu painter, of migratie naar iced/slint
   - Beslissing nemen bij start fase 4

### Relatie met Fase 3/3b

Fase 3b levert de DDC spectrum pipeline (capture + FFT + protocol + renderer). Fase 4 integreert dit in een professionele GUI. De spectrum/waterfall renderer en tuning controls uit fase 3b worden hergebruikt en uitgebreid.

---

## Fase 5b — Overige Externe Apparaten (Gepland)

### Doel

Volledige remote station control: PA, rotor en overige apparaten.

### Apparaten en protocollen

#### Power Amplifier — RF2K-S

| Interface | Protocol | Details |
|-----------|----------|---------|
| USB (FTDI) | CAT serial | Frequentie, power, status readback |
| LAN/WiFi | UDP | Frequentie in tens of Hz, XML formaat |
| VNC | RFB | Volledige display remote (ingebouwde Raspberry Pi) |
| +12V RCA | Aan/uit | Remote power on/off |

De RF2K-S heeft een ingebouwde Raspberry Pi die display en interfaces beheert. Beschikbare info: forward/reflected power, SWR, temperatuur, band, ATU status.

Aanpak: Server communiceert via USB-serial of UDP met RF2K-S. Status en controls worden via het bestaande SDR Remote protocol naar clients gestuurd.

#### Andere PA's (optioneel, later)

| PA | Interface | Protocol |
|----|-----------|----------|
| ACOM 600S/1200S/2020S | RS-232 (DB-9) | Binair, 9600 baud 8N1, max 72-byte berichten |
| SPE Expert 1K-2K | RS-232 of USB | Serial, 9600 baud |

#### Antenna Rotor

De facto standaard: **Yaesu GS-232A/B protocol** (RS-232, 1200-9600 baud).

| Commando | Functie |
|----------|---------|
| `Mxxx` | Draai naar azimut xxx graden |
| `C` | Rapporteer huidige azimut |
| `Wxxx yyy` | Azimut + elevatie |
| `S` | Stop rotatie |
| `R` / `L` | Draai rechts/links |

Alternatief: **EasyComm II** protocol (populair voor satelliet tracking).

Aanpak: Server stuurt GS-232 commando's via seriële poort (of serial-over-IP). Client toont kompasroos met huidige heading en doelpositie.

#### Antenna Switches

- **BCD band decoder** aanpak: frequentie → BCD band data → relay switching
- **4O3A Antenna Genius:** LAN/Internet control, 8 antennes naar 2 radio's
- **RF2K-S MULTI INTERFACE:** DB-15 connector accepteert BCD band data

Aanpak: Server stuurt band data op basis van VFO frequentie. Switches volgen automatisch.

### Implementatie architectuur

```
┌─────────────────────────────────────────────────────┐
│                   SDR Remote Server                  │
│                                                      │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐          │
│  │ Thetis   │  │ RF2K-S   │  │ Rotor    │          │
│  │ CAT TCP  │  │ USB/UDP  │  │ Serial   │          │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘          │
│       │              │              │                │
│  ┌────┴──────────────┴──────────────┴─────┐         │
│  │         Equipment Manager              │         │
│  │   Aggregeert status, routeert cmds     │         │
│  └────────────────┬───────────────────────┘         │
│                   │                                  │
│            UDP protocol                              │
│         (bestaand + uitbreidingen)                   │
└───────────────────┼──────────────────────────────────┘
                    │
            ┌───────┴───────┐
            │  Remote Client │
            │  (Desktop/     │
            │   Android)     │
            └────────────────┘
```

### Protocol uitbreidingen

Nieuwe packet types voor externe apparaten:

| Type | Richting | Inhoud |
|------|----------|--------|
| EquipmentStatus | Server → Client | PA power/SWR/temp, rotor azimut, switch positie |
| EquipmentCommand | Client → Server | PA aan/uit, rotor target, switch selectie |

### Referenties

- [RF2K-S User Manual](https://rf-kit.de/files/RF2K-S_User_Manual_EN_V06.pdf)
- [ACOM serial protocol](https://static.dxengineering.com/global/images/technicalarticles/aom-600s_it.pdf)
- [K3NG Arduino Rotator Controller](https://github.com/k3ng/k3ng_rotator_controller)
- [4O3A Antenna Genius](https://4o3a.com/8x2-antenna-switch)
- [Yaesu GS-232A protocol](https://www.yaesu.com/Files/4CB6273C-1018-01AF-FA4D504B591F641A/GS232A.pdf)

---

## Tijdlijn

| Fase | Geschatte doorlooptijd | Afhankelijkheden |
|------|----------------------|-----------------|
| 0-2 | Compleet | — |
| 3 | Compleet | HPSDR protocol kennis, FFT implementatie |
| 3b | Compleet | DDC I/Q kennis, wideband als basis |
| v0.1.0 | Compleet | Branding + netwerk-robuustheid |
| v0.1.1 | Compleet | RX filter bandwidth control |
| 5a | Compleet | Amplitec 6/2 + JC-4s tuner |
| 4 | Groot | Fase 3b (DDC spectrum pipeline) |
| 5b | Middel per apparaat | Fysieke hardware voor testen |

Fase 3b vervangt wideband (fase 3) als standaard spectrum. Fase 4 bouwt voort op de DDC spectrum pipeline. Fase 5a (Amplitec + JC-4s) is compleet; fase 5b (PA, rotor) kan deels parallel aan fase 4 ontwikkeld worden.
