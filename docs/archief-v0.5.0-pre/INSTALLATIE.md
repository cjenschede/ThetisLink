# ThetisLink v0.5.0 — Installatiehandleiding

Remote bediening voor Thetis SDR met HPSDR Protocol 2 apparaten.
Audio, PTT, frequentie, mode, controls, spectrum/waterfall en volledige RX2/VFO-B support over het netwerk.

**Compatibiliteit:** Getest met ANAN 7000DLE. Zou moeten werken met alle HPSDR Protocol 2 apparaten (ANAN-7000DLE, ANAN-8000DLE, ANAN-G2, Hermes-Lite 2, etc.) in combinatie met Thetis.

---

## Wat zit er in dit pakket?

| Bestand | Beschrijving |
|---------|-------------|
| ThetisLink-Server.exe | Server — draait op de PC naast Thetis |
| ThetisLink-Client.exe | Desktop client — Windows |
| ThetisLink-0.5.0.apk | Android client — telefoon/tablet |
| thetislink-server.conf | Voorbeeldconfiguratie server |
| thetislink-client.conf | Voorbeeldconfiguratie client |
| DOCUMENTATIE.md | Technische documentatie |
| SHA256SUMS.txt | Checksums ter verificatie |

---

## Stap 1: Server-PC instellen (naast Thetis)

### 1.1 Thetis TCI inschakelen

In Thetis → Setup → CAT Control:
1. Schakel **TCI Server** in
2. Standaard poort: **40001**

In Thetis → Setup → CAT Control:
3. Schakel ook **TCP/IP CAT Server** in op poort **13013** (nodig voor aanvullende commando's)

### 1.2 Server starten

Kopieer `ThetisLink-Server.exe` naar een map op de server-PC. Bij eerste start wordt automatisch een `thetislink-server.conf` aangemaakt.

Start vanuit een command prompt (geen admin nodig!):
```
ThetisLink-Server.exe
```

Vul in de server GUI in:
- **TCI**: `ws://127.0.0.1:40001` (of het IP/poort van Thetis)
- **CAT**: `127.0.0.1:13013` (voor aanvullende commando's)

> **Let op:** De server heeft **geen Administrator-rechten** nodig.

### 1.3 Externe apparaten

In de server GUI kun je optioneel externe apparaten configureren:

| Apparaat | Verbinding | Instelling |
|----------|-----------|-----------|
| Amplitec 6/2 | Serieel (USB) | COM poort selecteren |
| JC-4s Tuner | Serieel (USB) | COM poort selecteren |
| SPE Expert 1.3K-FA | Serieel (USB) | COM poort selecteren |
| RF2K-S | TCP/IP | IP:poort (bijv. 192.168.1.50:8080) |
| UltraBeam RCU-06 | Serieel (USB) | COM poort selecteren |
| EA7HG Visual Rotor | UDP | IP:poort (bijv. 192.168.1.66:2570) |

Elk apparaat heeft een **enable/disable vinkje**. Uitgeschakelde apparaten behouden hun configuratie (COM poort / IP adres) maar worden niet opgestart.

### 1.4 Firewall

Bij eerste start vraagt Windows Firewall om toestemming. Sta **privénetwerk** toe.

De server luistert op **UDP poort 4580**. Zorg dat deze poort open staat in de Windows Firewall.

Als dit niet automatisch gevraagd wordt:
1. Windows Defender Firewall → Geavanceerde instellingen
2. Binnenkomende regel → Nieuwe regel → Programma
3. Selecteer `ThetisLink-Server.exe`
4. Toestaan op privénetwerk

---

## Stap 2: Desktop client instellen

### 2.1 Installatie

Kopieer `ThetisLink-Client.exe` naar een map op de client-PC. Geen installatie nodig.

### 2.2 Eerste keer starten

1. Start `ThetisLink-Client.exe`
2. Selecteer je **microfoon** (Input) en **speaker/headset** (Output) bovenaan
3. Vul het **serveradres** in: `<server-IP>:4580` (bijv. `192.168.1.79:4580`)
4. Klik **Connect**

### 2.3 Configuratie

Instellingen worden automatisch opgeslagen in `thetislink-client.conf` naast de exe:
- Server adres, volume, TX gain, AGC, spectrum instellingen
- Frequentiemeldingen (M1-M5)
- TX profielen (in Settings)

---

## Stap 3: Android client installeren

### 3.1 APK installeren

1. Kopieer `ThetisLink-0.5.0.apk` naar je telefoon (USB, e-mail, of cloud)
2. Open het APK-bestand op de telefoon
3. Sta "Installeren van onbekende bronnen" toe als gevraagd
4. Installeer

Of via ADB (met telefoon aangesloten via USB, USB-debugging aan):
```
adb install ThetisLink-0.5.0.apk
```

### 3.2 Verbinden

1. Open ThetisLink
2. Vul het serveradres in: `<server-IP>:4580`
3. Tik **Connect**
4. Sta microfoontoegang toe als gevraagd

---

## Bediening

### Audio
- **RX Volume**: regelt het ontvangstvolume (stuurt ZZLA naar Thetis)
- **TX Gain**: regelt de microfoonversterking naar de server
- **AGC**: automatische TX gain controle (voorkomt oversturing)

### PTT
- Desktop: klik op de PTT-knop om te wisselen (klik = aan, klik = uit). Space bar = push-to-talk (vasthouden)
- Android: tik en vasthouden op de PTT-knop
- MIDI: PTT-knop op MIDI controller werkt als toggle met LED feedback

### MIDI Controller (Desktop + Android)
- Sluit een USB MIDI controller aan (desktop: direct USB, Android: via USB-OTG)
- Ga naar de **MIDI** tab (desktop) of klik op **MIDI** onderaan (Android)
- Klik **Scan/Refresh**, selecteer je device, klik **Connect**
- Gebruik **Learn** om knoppen/sliders toe te wijzen aan functies
- Beschikbare functies: PTT (met LED), VFO tune, volumes, drive, NR, ANF, mode, band, power
- Encoder stappen instelbaar: 1 Hz, 10 Hz, 100 Hz, 1 kHz per tick (desktop)

### Frequentie
- Klik op de frequentie om direct een waarde in te typen
- Gebruik de **-/+** knoppen met stappen (10 Hz, 100 Hz, 1 kHz, 10 kHz)
- In het spectrum: scroll = tune ±1 kHz, klik = spring naar frequentie
- In de waterfall: klik = spring naar frequentie (desktop + Android)

### Filter bandbreedte
- Gebruik de **[ - ] 2.7 kHz [ + ]** knoppen om de RX-filter aan te passen
- Presets wisselen automatisch per mode (SSB/CW/AM/FM)

### Mode
- Klik op **LSB**, **USB**, **AM** of **FM**

### Spectrum/Waterfall
- Klik op **Spectrum** om aan/uit te schakelen
- Regelaars: Ref (referentieniveau), Range (dB-bereik), Zoom, Pan, WF (waterfall contrast)

### WebSDR/KiwiSDR (Desktop)
- Klik op **WebSDR** om een embedded WebSDR/KiwiSDR venster te openen
- Ondersteunt websdr.org en KiwiSDR sites (auto-detectie)
- Frequentie synchronisatie: WebSDR volgt automatisch de VFO frequentie
- Mute bij TX: WebSDR audio stopt automatisch tijdens zenden
- Favorieten: sla veelgebruikte WebSDR URLs op met het ★ icoon

### Controls
- **Power**: Thetis aan/uit
- **NR**: Noise Reduction (cyclus: uit → NR1 → NR2 → NR3 → NR4 → uit)
- **ANF**: Auto Notch Filter aan/uit
- **Mic AGC**: Automatische microfoon gain controle aan/uit
- **Drive**: Zendvermogen 0-100%
- **TX Profile**: Wisselen tussen geconfigureerde TX-profielen

### TX Spectrum
- Bij zenden schakelt het spectrum automatisch naar TX-optimale instellingen:
  - Ref: -30 dB, Range: 100 dB (of 120 dB met PA actief)
- Na het loslaten van PTT worden de oorspronkelijke spectrum instellingen hersteld

### Externe apparaten
- Via de **Devices** tab (desktop/Android) of aparte vensters (server)
- Rotor: klik in de kompasscirkel om naar een hoek te draaien

---

## Netwerk vereisten

| Type | Bandbreedte | Latency |
|------|-------------|---------|
| Audio alleen | ~25 kbps | < 100 ms aanbevolen |
| Audio + spectrum | ~350-700 kbps | < 200 ms |

- **LAN/WiFi**: werkt direct, laagste latency
- **4G/5G mobiel**: werkt, adaptieve jitter buffer past zich aan
- **Poort**: UDP 4580 moet bereikbaar zijn (port forwarding bij internet-gebruik)

---

## Problemen oplossen

| Probleem | Oplossing |
|----------|-----------|
| Geen audio na connect | Controleer of TCI server actief is in Thetis |
| Frequentie verandert niet | Controleer Thetis CAT server (TCP poort 13013) |
| Disconnect na paar seconden | Firewall blokkeert UDP 4580, of instabiel WiFi |
| Audio hakkelt | Check loss% onderaan de client — hoog = netwerkprobleem |
| Spectrum toont niets | Controleer of TCI server actief is en IQ streams starten |
| APK installeert niet | Sta "onbekende bronnen" toe in Android-instellingen |
| Rotor offline | Check IP:poort, Visual Rotor software mag niet tegelijk draaien |

---

## Verificatie

Controleer de integriteit van de bestanden:
```
sha256sum -c SHA256SUMS.txt
```
