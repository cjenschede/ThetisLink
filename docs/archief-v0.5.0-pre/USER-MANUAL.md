# ThetisLink v0.4.2 — Gebruikershandleiding

## Inhoudsopgave

1. [Overzicht](#overzicht)
2. [Installatie](#installatie)
3. [Server configuratie](#server-configuratie)
4. [Server starten](#server-starten)
5. [Client verbinden](#client-verbinden)
6. [Bediening](#bediening)
7. [Apparaten](#apparaten)
8. [DX Cluster](#dx-cluster)
9. [Macro's](#macros)
10. [Naamconventies](#naamconventies)

---

## Overzicht

ThetisLink is een remote bediening voor de ANAN 7000DLE SDR met Thetis. Het bestaat uit:

- **ThetisLink Server** — draait op de Thetis PC (Windows), bestuurt de radio via TCI/CAT
- **ThetisLink Client** — desktop client (Windows/macOS/Linux) met spectrum, waterval en volledige bediening
- **ThetisLink Android** — mobiele client app

De server communiceert met Thetis via TCI WebSocket (primair) en TCP CAT (voor aanvullende commando's). Audio wordt via Opus codec over UDP verzonden met minimale latency.

### Systeemvereisten

- **Server:** Windows 10/11, Thetis SDR software, ANAN 7000DLE (of compatibel)
- **Client:** Windows/macOS/Linux of Android 8+
- **Netwerk:** WiFi of LAN, UDP poort 4580

---

## Installatie

### Server (Windows)

1. Kopieer `ThetisLink-Server.exe` naar een map op de Thetis PC
2. Configureer `thetislink-server.conf` in dezelfde map (wordt automatisch aangemaakt bij eerste start)
3. Zorg dat Thetis draait met TCI server ingeschakeld (Setup > CAT > TCI)

### Client (Windows/macOS/Linux)

1. Kopieer `ThetisLink-Client.exe` naar een map
2. Start en voer het server IP-adres in

### Android

1. Installeer de APK via adb of sideload
2. Start de app en voer het server IP-adres in

---

## Server configuratie

Het configuratiebestand `thetislink-server.conf` staat in dezelfde map als de server executable. Hieronder de belangrijkste instellingen:

### Verbinding met Thetis

| Instelling | Voorbeeld | Beschrijving |
|---|---|---|
| `tci` | `192.168.1.79:40001` | TCI WebSocket adres (Thetis Setup > CAT > TCI) |
| `cat` | `192.168.1.79:13013` | CAT TCP adres (voor ZZLA, ZZLE, ZZBY commando's) |
| `thetis_path` | `C:\Program Files\Thetis\Thetis.exe` | Pad naar Thetis (voor remote start/stop) |
| `anan_interface` | `192.168.1.79` | ANAN radio interface IP |

### Audio

| Instelling | Voorbeeld | Beschrijving |
|---|---|---|
| `input` | `CABLE-A Output` | RX audio bron (van Thetis) |
| `output` | `CABLE-B Input` | TX audio bestemming (naar Thetis) |

> **Opmerking:** Vanaf v0.4.0 met TCI is VB-Cable niet meer nodig voor audio. TCI verzorgt de audio routing direct.

### Apparaten

| Instelling | Type | Beschrijving |
|---|---|---|
| `amplitec_port` | COM poort | Amplitec 6/2 antenne schakelaar (19200 baud) |
| `tuner_port` | COM poort | JC-4s automatische tuner |
| `spe_port` | COM poort | SPE Expert 1.3K-FA eindversterker |
| `rf2k_addr` | IP:poort | RF2K-S eindversterker (TCP, poort 8080) |
| `ultrabeam_port` | COM poort | UltraBeam RCU-06 antenne controller |
| `rotor_addr` | IP:poort | EA7HG Visual Rotor (UDP) |

Elk apparaat heeft een `_enabled` veld (true/false) en een `_window` veld om het venster bij starten te openen.

### DX Cluster

| Instelling | Voorbeeld | Beschrijving |
|---|---|---|
| `dxcluster_server` | `dxc.pi4cc.nl:8000` | DX cluster server adres |
| `dxcluster_callsign` | `PA3GHM` | Callsign voor cluster login |
| `dxcluster_enabled` | `true` | DX cluster aan/uit |
| `dxcluster_expiry_min` | `10` | Spot verlooptijd in minuten |

### Amplitec labels

```
amplitec_label1=JC-4s
amplitec_label2=A2
amplitec_label3=A3
amplitec_label4=A4
amplitec_label5=DummyL
amplitec_label6=UltraBeam
```

> **Belangrijk:** Zie [Naamconventies](#naamconventies) voor speciale integraties.

---

## Server starten

1. Start Thetis en schakel TCI in (Setup > CAT > TCI)
2. Start `ThetisLink-Server.exe`
3. Controleer de verbindingsinstellingen
4. Vink de gewenste apparaten aan
5. Klik **Start**
6. De server luistert op UDP poort 4580

### Server UI

De server toont:
- Verbindingsstatus (TCI/CAT)
- Actieve apparaat vensters (Tuner, Amplitec, SPE, RF2K, UltraBeam, Rotor)
- Macro knoppen (2 rijen van 12)
- Uptime en client info

---

## Client verbinden

1. Start de client
2. Voer het server IP-adres in (bijv. `192.168.1.79`)
3. Klik **Connect**

De client ontvangt automatisch:
- Real-time spectrum en waterval
- VFO frequentie, mode en filter
- S-meter waarden
- Apparaat status (Amplitec, UltraBeam, etc.)
- DX cluster spots

---

## Bediening

### VFO en frequentie

- **Frequentie display:** klik om direct een frequentie in te voeren
- **Stap knoppen:** +/- in stappen van 10 Hz, 100 Hz, 1 kHz, 10 kHz
- **Scroll wheel:** op het spectrum = 1 kHz stappen
- **Klik op spectrum:** tune naar die frequentie
- **Waterval klik (Android):** tune naar klik-positie

### Band geheugen

Per band wordt automatisch opgeslagen:
- Frequentie
- Mode (LSB/USB/CW/AM/FM/DIG)
- Filter breedte
- NR niveau

Bij bandwisseling worden deze automatisch hersteld. Daarnaast zijn er 5 vrije geheugenplaatsen (M1-M5).

### Mode

Selecteerbaar: LSB, USB, CW, AM, FM, DIG

### Filter

De filterbreedte is instelbaar met +/- knoppen. Presets zijn beschikbaar per mode:
- **CW:** 50, 100, 200, 500, 1000 Hz
- **SSB:** 1800, 2400, 2700, 3100, 3600 Hz
- **AM/FM:** 6000, 8000, 10000, 12000 Hz

### Volume

- **RX Volume:** ontvangstniveau (ZZLA commando)
- **TX Gain:** microfoon voorversterking
- **Drive:** zendvermogen 0-100%
- **Mic AGC:** automatische microfoon gain (aan/uit)

### Noise Reduction & Notch

- **NR:** cyclisch: UIT → NR1 → NR2 → NR3 → NR4
- **ANF:** Auto Notch Filter aan/uit

### PTT (Push-to-Talk)

- **Klik op PTT:** toggle zenden aan/uit
- **Spatiebalk ingedrukt houden:** push-to-talk (zenden zolang ingedrukt)

### Spectrum en waterval

- **Zoom:** verstelbaar, geeft nauwkeuriger frequentieweergave
- **Pan:** verschuif het zichtbare spectrum links/rechts (0 = gecentreerd op VFO)
- **Referentieniveau:** verschuif het dB bereik omhoog/omlaag
- **Auto Ref:** automatische referentieniveau-aanpassing op basis van ruisvloer
- **Contrast:** waterval helderheid per band (wordt onthouden)

### Popout vensters

De client ondersteunt losse vensters:
- **RX1 spectrum** — alleen RX1 spectrum + waterval met bediening
- **RX2 spectrum** — alleen RX2 spectrum + waterval met bediening
- **Joined** — RX1 en RX2 naast elkaar met gedeelde bediening

In popout vensters zijn beschikbaar:
- S-meter (bar of analoog naaldmeter, wisselbaar via toggle knop)
- Alle band/mode/filter/NR/ANF bediening
- VFO A<>B wisselknop (links-onder bij analoge naaldmeter)

### VFO B / RX2

Volledige tweede ontvanger ondersteuning:
- Onafhankelijke frequentie, mode, filter, S-meter
- Eigen spectrum en waterval
- VFO Sync: VFO B volgt automatisch VFO A
- A<>B: wissel VFO A en B

### WebSDR/KiwiSDR (Desktop)

Ingebouwde WebView voor WebSDR en KiwiSDR ontvangst:
- Frequentie synchronisatie: WebSDR volgt de VFO
- Automatisch muten tijdens zenden
- Favorietenlijst met ster-icoon

### MIDI Controller

Desktop en Android ondersteunen USB MIDI controllers:
- **Scan** knop zoekt beschikbare MIDI apparaten
- **Learn** modus: druk op een MIDI knop/slider, wijs een functie toe
- Beschikbare functies: PTT (met LED), VFO tune, volumes, drive, NR, ANF, mode, band, power
- Encoder stappen: 1 Hz, 10 Hz, 100 Hz, 1 kHz

---

## Apparaten

### Amplitec 6/2 Antenne Schakelaar

Serieel USB verbinding (19200 baud). Toont:
- Huidige schakelstand poort A en B
- 6 antenne posities met configureerbare labels
- Schakel knoppen per poort

### JC-4s Automatische Tuner

Serieel USB verbinding. Functies:
- **Tune** knop: start afstemming
- **Abort** knop: breek afstemming af
- Status weergave: Tuning, Done, Timeout, Aborted
- Log venster (optioneel)

De tuner werkt samen met eindversterkers (SPE/RF2K) voor veilig tunen: de PA gaat automatisch naar standby tijdens het tunen.

### SPE Expert 1.3K-FA

Serieel USB verbinding. Toont:
- Vermogen, SWR, temperatuur
- Antenne selectie
- Operate/Standby status

### RF2K-S

TCP/IP verbinding (poort 8080). Toont:
- Vermogen, SWR
- Bias spanning, PSU spanning
- Temperatuur
- Uptime
- Drive configuratie per band (SSB/AM/Cont)

### UltraBeam RCU-06

Serieel USB verbinding (19200 baud). Functies:
- **Frequentie display** met band indicatie
- **Direction knoppen:** Normal, 180°, Bi-Dir
- **Frequentie stap knoppen:** -100, -50, -25, +25, +50, +100 kHz
- **Sync VFO:** stel de UltraBeam in op de huidige VFO frequentie (A of B, afhankelijk van Amplitec schakelstand)
- **Auto:** automatische frequentie-tracking van de actieve VFO
  - Minimale stap: 25 kHz (voorkomt overbelasting van de motoren)
  - VFO selectie wordt automatisch bepaald via de Amplitec (zie [Naamconventies](#naamconventies))
- **Band presets:** snelkeuze knoppen per band
- **Motor voortgang:** progressiebalk tijdens element verplaatsing
- **Retract:** trek alle elementen in (met bevestiging)
- **Element weergave:** actuele element lengtes in mm

### EA7HG Visual Rotor

UDP verbinding. Toont:
- Kompas cirkel met huidige richting
- Azimuth en elevatie
- Klik op kompas om te draaien
- Handmatige invoer voor doelrichting

---

## DX Cluster

ThetisLink verbindt direct met een DX cluster server (telnet). Spots worden:
- Op het spectrum weergegeven als gekleurde stippellijnen met callsign labels
- Gefilterd op de band van VFO A en VFO B
- Automatisch verwijderd na de ingestelde verlooptijd

**Spot kleuren per mode:**
- CW: geel
- SSB/Phone: groen
- FT8/FT4/Digital: cyaan
- Overig: wit

Spots worden ook naar Thetis doorgestuurd via TCI `SPOT:` commando, zodat ze ook op het Thetis panorama verschijnen.

---

## Macro's

De server ondersteunt 24 programmeerbare macro knoppen in 2 rijen:
- **Rij 1:** F1 t/m F12 (typisch VFO A presets)
- **Rij 2:** ^F1 t/m ^F12 (typisch VFO B presets)

### Macro acties

Elke macro kan een reeks acties bevatten:
- **CAT commando:** bijv. `ZZFA00014292000;` (stel VFO A in op 14.292 MHz)
- **Delay:** bijv. `delay:200` (wacht 200ms)
- **Tune:** start de JC-4s tuner

### Macro configuratie

Macro's worden opgeslagen in `thetislink-macros.conf`:
```
macro_0_label=20m 14292
macro_0=ZZFA00014292000; ZZMD01;
```

### Veelgebruikte CAT commando's

| Commando | Beschrijving |
|---|---|
| `ZZFA00014292000;` | VFO A → 14.292 MHz |
| `ZZFB00007073000;` | VFO B → 7.073 MHz |
| `ZZMD00;` | VFO A mode → CW |
| `ZZMD01;` | VFO A mode → LSB |
| `ZZME00;` | VFO B mode → CW |
| `ZZME01;` | VFO B mode → LSB |

> **Let op:** Gebruik `ZZFA`/`ZZMD` voor VFO A en `ZZFB`/`ZZME` voor VFO B. Een veelgemaakte fout is ZZMD gebruiken in VFO B macro's — dit wijzigt dan de mode van VFO A!

---

## Naamconventies

ThetisLink gebruikt de Amplitec antenne label namen voor automatische integraties tussen apparaten. Als de labelnamen niet kloppen gaat er niets stuk, maar werken bepaalde automatische functies niet.

### UltraBeam integratie

De Amplitec label voor de UltraBeam antenne-uitgang moet een van deze woorden bevatten (niet hoofdlettergevoelig):
- `UltraBeam`
- `Ultra Beam`
- `UB`

**Wat dit oplevert:**
- De **Sync VFO** knop en **Auto** tracking in het UltraBeam panel kiezen automatisch de juiste VFO:
  - Als Amplitec poort **B** op de UltraBeam positie staat → volgt **VFO B**
  - Als Amplitec poort **A** op de UltraBeam positie staat → volgt **VFO A**
  - Geen match → default **VFO A**

**Voorbeeld configuratie:**
```
amplitec_label1=JC-4s
amplitec_label2=Dipole
amplitec_label3=Vertical
amplitec_label4=Beverage
amplitec_label5=DummyLoad
amplitec_label6=UltraBeam
```

In dit voorbeeld staat de UltraBeam op positie 6. Als je de Amplitec poort B schakelt naar positie 6, volgt de UltraBeam automatisch VFO B.

### JC-4s Tuner integratie

De Amplitec label voor de JC-4s tuner uitgang moet bevatten:
- `JC-4s`
- `JC4s`
- `Tuner`

*(Toekomstige integratie: automatische antenna selection voor safe tune)*

---

## Probleemoplossing

### Server start niet

- Controleer of Thetis draait en TCI is ingeschakeld
- Controleer het TCI adres in de configuratie (standaard poort 40001)
- Controleer of de CAT poort beschikbaar is

### Client verbindt niet

- Controleer het server IP-adres
- Controleer of UDP poort 4580 niet geblokkeerd wordt door een firewall
- Controleer of server en client op hetzelfde netwerk zitten

### UltraBeam timeout bij snel stappen

De UltraBeam RCU-06 heeft een beperkte serieel commando snelheid. Bij snel achter elkaar drukken op stap-knoppen worden tussenliggende commando's overgeslagen en alleen het laatste verzonden. Dit is normaal gedrag en voorkomt overbelasting.

### Spectrum en waterval lopen niet synchroon

Als het spectrum (lijn) en de waterval niet synchroon lopen bij het pannen, controleer de client versie. Dit is opgelost in build 17+.

---

## Versiegeschiedenis

| Versie | Hoogtepunten |
|---|---|
| 0.4.2 | Configureerbaar FFT formaat, dynamische spectrum bins, Android power knop fix |
| 0.4.1 | WebSDR/KiwiSDR integratie, frequentie sync, TX spectrum auto-override |
| 0.4.0 | TCI WebSocket (geen VB-Cable meer nodig), waterval click-to-tune Android |
| 0.3.2 | MIDI controller ondersteuning, PTT toggle met LED, Mic AGC |
| 0.3.1 | Band geheugen, FM filter fix, macOS client |
| 0.3.0 | Volledige RX2/VFO-B ondersteuning, DDC spectrum+waterval |
